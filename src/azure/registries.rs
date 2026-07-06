//! Read-only Azure Container Registry inspection.
//!
//! ## Contract (do not change without coordinating with the UI lane)
//!
//! Three public functions form the surface the UI consumes:
//!
//! - [`list_registries`] — Resource Graph KQL discovery of ACR registries
//!   across the supplied subscriptions (control plane, only `Reader` needed).
//! - [`list_repositories`] — Docker Registry v2 catalog enumeration for one
//!   registry (data plane, requires `AcrPull` or stronger on the registry).
//! - [`list_tags`] — Docker Registry v2 tags listing for one repository
//!   (same data-plane permission requirement).
//!
//! ## Scope decisions worth flagging
//!
//! - **AAD bearer with OAuth2 exchange**: the ACR data plane does NOT accept
//!   AAD bearer tokens directly. Callers must POST the AAD token to
//!   `https://{registry}/oauth2/exchange` to obtain an ACR refresh token, then
//!   POST that refresh token to `https://{registry}/oauth2/token` with a
//!   `scope=...` clause to get the actual bearer used by `/v2/*`. See
//!   [`acquire_acr_access_token`].
//! - **Read-only**: catalog + tags only. No manifest delete / repo delete /
//!   image pull codepaths, even stubs.
//! - **Pagination**: bounded `n=200` per page; we follow `Link: rel="next"`
//!   until exhausted or we hit [`MAX_REPOSITORIES`] / [`MAX_TAGS`] and warn.
//!   ACR's `/v2/_catalog` and `/v2/{repo}/tags/list` are alphabetical, so the
//!   truncation behaviour is deterministic — matches the precedent in
//!   `storage.rs` / `resources.rs`.

#![allow(dead_code, unused_variables)]

use anyhow::{anyhow, Context};
use chrono::{DateTime, Utc};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde::Deserialize;

use crate::azure::auth::{AzureAuth, SCOPE_ARM};
use crate::azure::client::ArmClient;
use crate::azure::cosmos::{build_http, send_with_retry};

/// One container registry discovered via Resource Graph.
#[derive(Clone, Debug)]
pub struct Registry {
    /// Full ARM resource id.
    pub id: String,
    pub name: String,
    pub resource_group: String,
    pub subscription_id: String,
    pub location: String,
    /// SKU name: `Basic`, `Standard`, `Premium`, or legacy `Classic`. `None`
    /// when Resource Graph omitted the field.
    pub sku: Option<String>,
    /// `properties.loginServer`, e.g. `myregistry.azurecr.io`. Source of truth
    /// for data-plane host. Falls back to `{name}.azurecr.io` when missing —
    /// see [`Self::login_server_or_default`].
    pub login_server: Option<String>,
    /// `properties.adminUserEnabled`. `Some(true)` → the static admin
    /// username/password is enabled (a security flag worth surfacing).
    pub admin_user_enabled: Option<bool>,
    /// `properties.publicNetworkAccess`. `Enabled` / `Disabled`.
    pub public_network_access: Option<String>,
    /// `properties.anonymousPullEnabled`. Only available on `Standard`+;
    /// `Some(true)` → the registry serves manifests without auth.
    pub anonymous_pull_enabled: Option<bool>,
    /// `properties.creationDate` parsed to UTC.
    pub created_at: Option<DateTime<Utc>>,
}

impl Registry {
    /// `loginServer` from Resource Graph when present; otherwise the canonical
    /// `{name}.azurecr.io`. The portal always shows the field, but Resource
    /// Graph has been observed to omit it on `Classic` SKUs.
    pub fn login_server_or_default(&self) -> String {
        self.login_server
            .clone()
            .unwrap_or_else(|| format!("{}.azurecr.io", self.name))
    }
}

/// One repository inside a registry. The Docker Registry v2 `_catalog` API
/// returns just the name — image counts / sizes live behind manifest fetches
/// we don't make.
#[derive(Clone, Debug)]
pub struct Repository {
    pub name: String,
}

/// One tag inside a repository.
#[derive(Clone, Debug)]
pub struct Tag {
    pub name: String,
}

/// Resource Graph KQL for ACR registries. Same body/`subscriptions` envelope
/// as [`super::resources::KQL`] / [`super::storage::ACCOUNTS_KQL`].
const REGISTRIES_KQL: &str = r#"
Resources
| where type == 'microsoft.containerregistry/registries'
| project id, name, type, location, resourceGroup, subscriptionId, sku, properties
| order by name asc
"#;

/// Soft cap on rows accepted from `/v2/_catalog` per registry. Beyond this we
/// stop paginating and warn — matches the precedent in `storage.rs`.
const MAX_REPOSITORIES: usize = 1000;

/// Soft cap on tags accepted from `/v2/{repo}/tags/list` per repository.
const MAX_TAGS: usize = 1000;

/// Per-page request size for catalog / tags. Higher than the OCI default
/// (which is server-defined and often very small) so a typical registry comes
/// back in a single round-trip. `n=500` works on every ACR tier.
const PAGE_SIZE: u32 = 500;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Enumerate ACR registries across `subscription_ids`. Empty slice → all
/// subscriptions visible to the credential (Resource Graph default scope).
pub async fn list_registries(
    auth: &AzureAuth,
    subscription_ids: &[String],
) -> anyhow::Result<Vec<Registry>> {
    let client = ArmClient::new(auth.clone())?;

    let body = if subscription_ids.is_empty() {
        serde_json::json!({ "query": REGISTRIES_KQL })
    } else {
        serde_json::json!({
            "subscriptions": subscription_ids,
            "query": REGISTRIES_KQL,
        })
    };

    let resp = client
        .post(
            "/providers/Microsoft.ResourceGraph/resources?api-version=2022-10-01",
            &body,
        )
        .await
        .context("resource graph: list container registries")?;

    let rows = resp
        .get("data")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("resource graph response missing 'data' array"))?;

    if rows.len() >= 1000 {
        tracing::warn!(
            "resource graph returned {} container registries; pagination not implemented in v1",
            rows.len()
        );
    }

    Ok(rows.iter().filter_map(parse_registry).collect())
}

/// List repositories inside `registry` via the Docker Registry v2 `_catalog`
/// endpoint. Requires the identity to have `AcrPull` (or stronger) on the
/// registry — otherwise the OAuth2 exchange step returns 401 / 403.
pub async fn list_repositories(
    auth: &AzureAuth,
    registry: &Registry,
) -> anyhow::Result<Vec<Repository>> {
    let host = registry.login_server_or_default();
    let token = acquire_acr_access_token(auth, &host, "registry:catalog:*")
        .await
        .map_err(|e| classify_data_plane_error(&registry.name, &host, e))?;
    let http = build_http()?;

    let mut url = format!("https://{host}/v2/_catalog?n={PAGE_SIZE}");
    let mut out: Vec<Repository> = Vec::new();

    loop {
        let body = fetch_with_bearer(&http, &url, &token)
            .await
            .map_err(|e| classify_data_plane_error(&registry.name, &host, e))?;
        let page: V2CatalogResponse = serde_json::from_str(&body.json)
            .map_err(|e| anyhow!("parse /v2/_catalog response: {e}"))?;
        for name in page.repositories {
            if !name.is_empty() {
                out.push(Repository { name });
            }
        }
        if out.len() >= MAX_REPOSITORIES {
            tracing::warn!(
                "/v2/_catalog: stopping at {} repositories for {host}; pagination cap reached",
                out.len()
            );
            break;
        }
        match next_link(&body.headers, &host) {
            Some(next) => url = next,
            None => break,
        }
    }

    Ok(out)
}

/// List tags inside `repository` of `registry`. Same data-plane permission
/// requirement as [`list_repositories`].
pub async fn list_tags(
    auth: &AzureAuth,
    registry: &Registry,
    repository: &str,
) -> anyhow::Result<Vec<Tag>> {
    let host = registry.login_server_or_default();
    let scope = format!("repository:{repository}:metadata_read");
    let token = acquire_acr_access_token(auth, &host, &scope)
        .await
        .map_err(|e| classify_data_plane_error(&registry.name, &host, e))?;
    let http = build_http()?;

    let repo_path = encode_repository_path(repository);
    let mut url = format!("https://{host}/v2/{repo_path}/tags/list?n={PAGE_SIZE}");
    let mut out: Vec<Tag> = Vec::new();

    loop {
        let body = fetch_with_bearer(&http, &url, &token)
            .await
            .map_err(|e| classify_data_plane_error(&registry.name, &host, e))?;
        let page: V2TagsResponse = serde_json::from_str(&body.json)
            .map_err(|e| anyhow!("parse /v2/{repository}/tags/list response: {e}"))?;
        for name in page.tags.unwrap_or_default() {
            if !name.is_empty() {
                out.push(Tag { name });
            }
        }
        if out.len() >= MAX_TAGS {
            tracing::warn!(
                "/v2/{repository}/tags/list: stopping at {} tags for {host}; pagination cap reached",
                out.len()
            );
            break;
        }
        match next_link(&body.headers, &host) {
            Some(next) => url = next,
            None => break,
        }
    }

    Ok(out)
}

// ---------------------------------------------------------------------------
// OAuth2 token exchange (AAD → ACR refresh token → ACR access token)
// ---------------------------------------------------------------------------

/// Two-step OAuth2 exchange that turns an AAD bearer into a scoped ACR data
/// plane access token. We don't cache the result: callers fetch one token per
/// data-plane operation and the per-view cache prevents repeated requests.
async fn acquire_acr_access_token(
    auth: &AzureAuth,
    host: &str,
    scope: &str,
) -> anyhow::Result<String> {
    let aad = auth
        .token(SCOPE_ARM)
        .await
        .context("acquire ARM token for ACR exchange")?;
    let http = build_http()?;

    // Step 1: exchange AAD access token for an ACR refresh token.
    let exchange_url = format!("https://{host}/oauth2/exchange");
    let exchange_body = [
        ("grant_type", "access_token"),
        ("service", host),
        ("access_token", aad.as_str()),
    ];
    // Retried: token exchange is side-effect free and ACR fronts it with the
    // same throttling as the rest of the data plane.
    let exchange_resp = send_with_retry(|| {
        http.post(&exchange_url)
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .form(&exchange_body)
    })
    .await
    .map_err(|e| anyhow!("acr oauth2/exchange network error: {e}"))?;
    let status = exchange_resp.status();
    if !status.is_success() {
        let body = exchange_resp.text().await.unwrap_or_default();
        return Err(anyhow!(
            "acr oauth2/exchange returned {}: {}",
            status.as_u16(),
            truncate_error_body(&body)
        ));
    }
    let exchange: AcrRefreshTokenResponse = exchange_resp
        .json()
        .await
        .map_err(|e| anyhow!("acr oauth2/exchange: parse json: {e}"))?;

    // Step 2: trade the refresh token for a scoped access token.
    let token_url = format!("https://{host}/oauth2/token");
    let token_body = [
        ("grant_type", "refresh_token"),
        ("service", host),
        ("scope", scope),
        ("refresh_token", exchange.refresh_token.as_str()),
    ];
    let token_resp = send_with_retry(|| {
        http.post(&token_url)
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .form(&token_body)
    })
    .await
    .map_err(|e| anyhow!("acr oauth2/token network error: {e}"))?;
    let status = token_resp.status();
    if !status.is_success() {
        let body = token_resp.text().await.unwrap_or_default();
        return Err(anyhow!(
            "acr oauth2/token returned {}: {}",
            status.as_u16(),
            truncate_error_body(&body)
        ));
    }
    let access: AcrAccessTokenResponse = token_resp
        .json()
        .await
        .map_err(|e| anyhow!("acr oauth2/token: parse json: {e}"))?;
    Ok(access.access_token)
}

#[derive(Debug, Deserialize)]
struct AcrRefreshTokenResponse {
    refresh_token: String,
}

#[derive(Debug, Deserialize)]
struct AcrAccessTokenResponse {
    access_token: String,
}

#[derive(Debug, Deserialize)]
struct V2CatalogResponse {
    #[serde(default)]
    repositories: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct V2TagsResponse {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// HTTP plumbing
// ---------------------------------------------------------------------------

struct RawResponse {
    json: String,
    headers: HeaderMap,
}

async fn fetch_with_bearer(
    http: &reqwest::Client,
    url: &str,
    bearer: &str,
) -> anyhow::Result<RawResponse> {
    let value = HeaderValue::from_str(&format!("Bearer {bearer}"))
        .map_err(|_| anyhow!("ACR bearer contained invalid header characters"))?;
    // Retried: catalog/tags listing is a plain GET, safe to replay on 429/5xx.
    let resp = send_with_retry(|| http.get(url).header(AUTHORIZATION, value.clone()))
        .await
        .map_err(|e| anyhow!("acr data-plane network error: {e}"))?;
    let status = resp.status();
    let headers = resp.headers().clone();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!(
            "acr data-plane returned {}: {}",
            status.as_u16(),
            truncate_error_body(&body)
        ));
    }
    let text = resp
        .text()
        .await
        .map_err(|e| anyhow!("read acr data-plane body: {e}"))?;
    Ok(RawResponse {
        json: text,
        headers,
    })
}

/// Pull a `Link: <next>; rel="next"` next-page URL out of the response headers,
/// resolving relative `Link` paths against `https://{host}`. Returns `None`
/// when there's no next-page link.
fn next_link(headers: &HeaderMap, host: &str) -> Option<String> {
    let raw = headers.get(reqwest::header::LINK)?.to_str().ok()?;
    parse_next_link(raw, host)
}

/// Public for tests — parses an OCI Distribution `Link` header and returns
/// the next-page URL, resolving relative paths against `https://{host}`.
pub(crate) fn parse_next_link(link_header: &str, host: &str) -> Option<String> {
    for part in link_header.split(',') {
        let part = part.trim();
        // Shape: `</v2/_catalog?n=500&last=foo>; rel="next"`
        if !part.contains("rel=\"next\"") && !part.contains("rel=next") {
            continue;
        }
        let start = part.find('<')?;
        let end = part.find('>')?;
        if end <= start + 1 {
            continue;
        }
        let path = &part[start + 1..end];
        let url = if path.starts_with("http://") || path.starts_with("https://") {
            path.to_string()
        } else if let Some(stripped) = path.strip_prefix('/') {
            format!("https://{host}/{stripped}")
        } else {
            format!("https://{host}/{path}")
        };
        return Some(url);
    }
    None
}

fn truncate_error_body(s: &str) -> String {
    const LIMIT: usize = 1024;
    if s.len() <= LIMIT {
        s.to_string()
    } else {
        let mut end = LIMIT;
        while !s.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

// ---------------------------------------------------------------------------
// Parsers
// ---------------------------------------------------------------------------

fn extract_bool(v: &serde_json::Value) -> Option<bool> {
    if let Some(b) = v.as_bool() {
        return Some(b);
    }
    if let Some(s) = v.as_str() {
        match s.to_ascii_lowercase().as_str() {
            "true" => return Some(true),
            "false" => return Some(false),
            _ => return None,
        }
    }
    None
}

pub(crate) fn parse_registry(v: &serde_json::Value) -> Option<Registry> {
    let ty = v.get("type")?.as_str()?.to_lowercase();
    if ty != "microsoft.containerregistry/registries" {
        return None;
    }
    let id = v.get("id")?.as_str()?.to_string();
    let name = v
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .to_string();
    let resource_group = v
        .get("resourceGroup")
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .to_string();
    let subscription_id = v
        .get("subscriptionId")
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .to_string();
    let location = v
        .get("location")
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .to_string();
    let sku = v
        .get("sku")
        .and_then(|s| s.get("name"))
        .and_then(|n| n.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let props = v.get("properties");
    let login_server = props
        .and_then(|p| p.get("loginServer"))
        .and_then(|n| n.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let admin_user_enabled = props
        .and_then(|p| p.get("adminUserEnabled"))
        .and_then(extract_bool);
    let public_network_access = props
        .and_then(|p| p.get("publicNetworkAccess"))
        .and_then(|n| n.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let anonymous_pull_enabled = props
        .and_then(|p| p.get("anonymousPullEnabled"))
        .and_then(extract_bool);
    let created_at = props
        .and_then(|p| p.get("creationDate"))
        .and_then(|n| n.as_str())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc));

    Some(Registry {
        id,
        name,
        resource_group,
        subscription_id,
        location,
        sku,
        login_server,
        admin_user_enabled,
        public_network_access,
        anonymous_pull_enabled,
        created_at,
    })
}

// ---------------------------------------------------------------------------
// Error classification
// ---------------------------------------------------------------------------

fn classify_data_plane_error(registry_name: &str, host: &str, e: anyhow::Error) -> anyhow::Error {
    let msg = format!("{e}");
    let lower = msg.to_lowercase();
    if lower.contains(" 401 ") || lower.contains("returned 401") || lower.contains("unauthorized") {
        return anyhow!(
            "401 from ACR data plane: identity rejected by '{registry_name}'. \
             The signed-in account needs `AcrPull` (or stronger) on the registry — \
             control-plane `Reader` is not enough. underlying: {msg}"
        );
    }
    if lower.contains(" 403 ") || lower.contains("returned 403") || lower.contains("forbidden") {
        return anyhow!(
            "403 from ACR data plane: identity lacks `AcrPull` (or stronger) on \
             registry '{registry_name}'. underlying: {msg}"
        );
    }
    if lower.contains("dns")
        || lower.contains("failed to lookup")
        || lower.contains("no such host")
        || lower.contains("name or service not known")
    {
        return anyhow!(
            "DNS lookup failed for '{host}' — does registry '{registry_name}' exist \
             (or is it firewalled to a private endpoint)? underlying: {msg}"
        );
    }
    e
}

/// Percent-encode a Docker repository path while preserving `/` separators
/// (multi-segment names like `team/svc` are valid in OCI / ACR).
fn encode_repository_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let ok = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/');
        if ok {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_registry_row() {
        let row = json!({
            "id": "/subscriptions/s/resourceGroups/rg/providers/Microsoft.ContainerRegistry/registries/myreg",
            "name": "myreg",
            "type": "microsoft.containerregistry/registries",
            "location": "westeurope",
            "resourceGroup": "rg",
            "subscriptionId": "s",
            "sku": { "name": "Premium", "tier": "Premium" },
            "properties": {
                "loginServer": "myreg.azurecr.io",
                "adminUserEnabled": false,
                "publicNetworkAccess": "Enabled",
                "anonymousPullEnabled": false,
                "creationDate": "2026-01-02T03:04:05.000Z"
            }
        });
        let reg = parse_registry(&row).expect("expected registry");
        assert_eq!(reg.name, "myreg");
        assert_eq!(reg.sku.as_deref(), Some("Premium"));
        assert_eq!(reg.login_server.as_deref(), Some("myreg.azurecr.io"));
        assert_eq!(reg.admin_user_enabled, Some(false));
        assert_eq!(reg.public_network_access.as_deref(), Some("Enabled"));
        assert_eq!(reg.anonymous_pull_enabled, Some(false));
        assert!(reg.created_at.is_some());
    }

    #[test]
    fn registry_login_server_falls_back_to_name() {
        let mut reg = Registry {
            id: "id".into(),
            name: "legacy".into(),
            resource_group: "rg".into(),
            subscription_id: "s".into(),
            location: "westeurope".into(),
            sku: None,
            login_server: None,
            admin_user_enabled: None,
            public_network_access: None,
            anonymous_pull_enabled: None,
            created_at: None,
        };
        assert_eq!(reg.login_server_or_default(), "legacy.azurecr.io");
        reg.login_server = Some("legacy.westeurope.cr.azure.io".to_string());
        assert_eq!(
            reg.login_server_or_default(),
            "legacy.westeurope.cr.azure.io"
        );
    }

    #[test]
    fn skips_non_registry_rows() {
        let row = json!({
            "id": "/subscriptions/x/resourceGroups/y/providers/Microsoft.Web/sites/z",
            "name": "z",
            "type": "microsoft.web/sites",
            "location": "westeurope",
            "resourceGroup": "y",
            "subscriptionId": "x"
        });
        assert!(parse_registry(&row).is_none());
    }

    #[test]
    fn parses_v2_catalog_response() {
        let body = r#"{"repositories":["alpine","library/nginx","team/svc/api"]}"#;
        let parsed: V2CatalogResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.repositories.len(), 3);
        assert_eq!(parsed.repositories[2], "team/svc/api");
    }

    #[test]
    fn parses_v2_tags_response_with_nulls() {
        // ACR omits the `tags` field on a freshly-created repo with no images
        // pushed yet; `serde(default)` must coerce that to an empty Vec.
        let body = r#"{"name":"empty"}"#;
        let parsed: V2TagsResponse = serde_json::from_str(body).unwrap();
        assert!(parsed.tags.is_none());

        let body = r#"{"name":"img","tags":["latest","v1.0","v1.1"]}"#;
        let parsed: V2TagsResponse = serde_json::from_str(body).unwrap();
        let tags = parsed.tags.expect("tags should parse");
        assert_eq!(
            tags.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            vec!["latest", "v1.0", "v1.1"],
        );
    }

    #[test]
    fn parse_next_link_resolves_relative_path() {
        let header = "</v2/_catalog?n=500&last=zeta>; rel=\"next\"";
        let url = parse_next_link(header, "myreg.azurecr.io").unwrap();
        assert_eq!(url, "https://myreg.azurecr.io/v2/_catalog?n=500&last=zeta");
    }

    #[test]
    fn parse_next_link_absolute_url_passes_through() {
        let header = "<https://other.azurecr.io/v2/_catalog?n=500&last=z>; rel=\"next\"";
        let url = parse_next_link(header, "myreg.azurecr.io").unwrap();
        assert_eq!(url, "https://other.azurecr.io/v2/_catalog?n=500&last=z");
    }

    #[test]
    fn parse_next_link_returns_none_when_absent() {
        assert_eq!(parse_next_link("", "host"), None);
        assert_eq!(
            parse_next_link("</v2/foo>; rel=\"prev\"", "host"),
            None,
            "only rel=next entries should be followed",
        );
    }

    #[test]
    fn encode_repository_preserves_slashes() {
        assert_eq!(encode_repository_path("alpine"), "alpine");
        assert_eq!(encode_repository_path("team/svc"), "team/svc");
        assert_eq!(encode_repository_path("team/svc api"), "team/svc%20api");
    }

    #[test]
    fn classifies_401_with_role_hint() {
        let err = classify_data_plane_error(
            "myreg",
            "myreg.azurecr.io",
            anyhow!("acr data-plane returned 401: unauthorized"),
        );
        let msg = format!("{err}");
        assert!(msg.contains("AcrPull"), "got: {msg}");
        assert!(msg.contains("myreg"), "got: {msg}");
    }

    #[test]
    fn classifies_dns_failure_with_host() {
        let err = classify_data_plane_error(
            "ghost",
            "ghost.azurecr.io",
            anyhow!("acr data-plane network error: dns error: failed to lookup address"),
        );
        let msg = format!("{err}");
        assert!(msg.contains("ghost.azurecr.io"), "got: {msg}");
    }

    #[test]
    fn truncate_error_body_bounds_oversized_strings() {
        let big = "x".repeat(5_000);
        let out = truncate_error_body(&big);
        assert!(out.ends_with('…'));
        assert!(out.chars().count() <= 1_025);
    }
}
