//! Read-only Azure Key Vault inspection (secrets + certificates).
//!
//! ## Contract (do not change without coordinating with the UI lane)
//!
//! Three public functions form the surface the UI consumes:
//!
//! - [`list_vaults`] — Resource Graph KQL discovery of Key Vaults across the
//!   supplied subscriptions. Control plane only (`Reader` is sufficient).
//! - [`list_items`] — Data-plane enumeration of secrets *or* certificates
//!   inside one vault, following `nextLink` until exhausted. Returns
//!   **metadata only** (`enabled`, expiry, created/updated timestamps,
//!   content type, tags) — never the secret value, never the cert body.
//! - [`get_secret_value`] — Data-plane fetch of a *single* secret's plaintext,
//!   called only on an explicit user reveal (`Enter` / `x`). Needs `get` on
//!   the secret (RBAC `Key Vault Secrets User`, or a `get` access policy).
//!
//! ## Scope decisions worth flagging
//!
//! - **Listing is metadata only.** `list_items` returns `attributes` but never
//!   the secret bytes, so browsing a vault fetches no secret material. A value
//!   is pulled exactly once, on demand, when the user explicitly reveals a
//!   single secret via [`get_secret_value`]; the plaintext lives only in the
//!   reveal modal's payload and never enters the list cache. Certificates have
//!   no plaintext value, so reveal is secrets-only. The portal-open keybind
//!   (`o`) remains the escape hatch for anything the in-app reveal doesn't
//!   cover.
//! - **Secrets + certs only in v1**; keys are deferred. The data-plane URL
//!   surface is identical (`/keys` would slot in trivially) but keys add
//!   HSM-vs-software questions the UI doesn't need yet.
//! - **Both auth models supported.** A vault either uses RBAC (the modern
//!   default — `enableRbacAuthorization=true`) or legacy access policies.
//!   Either grants a `list` permission via `Key Vault Reader` / `Key Vault
//!   Secrets User` (RBAC) or `get`/`list` access-policy entries. The 403
//!   classifier mentions both because the data-plane error doesn't tell us
//!   which model is in effect.
//! - **AAD bearer with `vault.azure.net` audience.** No exchange step (unlike
//!   ACR) — the data plane accepts AAD tokens directly with `Authorization:
//!   Bearer <jwt>`. Note the audience has no `.default` path prefix beyond
//!   the host; the token's `aud` claim is literally `https://vault.azure.net`.
//! - **Pagination required.** Vaults routinely hold hundreds of secrets, so
//!   unlike `cosmos` (where we cap at 20 and ignore continuation) we follow
//!   `nextLink` until exhausted, capped at [`MAX_ITEMS`] with a warn-and-stop
//!   like `storage` / `registries`.

#![allow(dead_code, unused_variables)]

use anyhow::{anyhow, Context};
use chrono::{DateTime, TimeZone, Utc};
use reqwest::header::{HeaderValue, AUTHORIZATION};
use serde::Deserialize;

use crate::azure::auth::{AzureAuth, SCOPE_KEY_VAULT};
use crate::azure::client::ArmClient;
use crate::azure::cosmos::{build_http, send_with_retry};

/// One Key Vault discovered via Resource Graph.
#[derive(Clone, Debug)]
pub struct KeyVault {
    /// Full ARM resource id.
    pub id: String,
    pub name: String,
    pub resource_group: String,
    pub subscription_id: String,
    pub location: String,
    /// `properties.sku.name`: `standard` or `premium`. `None` when missing.
    pub sku: Option<String>,
    /// `properties.vaultUri`, e.g. `https://myvault.vault.azure.net/`.
    /// Source of truth for the data-plane host. Falls back to
    /// `https://{name}.vault.azure.net/` when missing — see
    /// [`Self::vault_uri_or_default`].
    pub vault_uri: Option<String>,
    /// `properties.enableRbacAuthorization`. `Some(true)` → RBAC roles gate
    /// data-plane access; `Some(false)` → legacy access-policy model.
    pub rbac_authorization_enabled: Option<bool>,
    /// `properties.enableSoftDelete`. Soft delete is a tenant default since
    /// 2020 and effectively always true for new vaults.
    pub soft_delete_enabled: Option<bool>,
    /// `properties.enablePurgeProtection`. When true, deleted vaults / secrets
    /// cannot be hard-deleted before their retention period — a compliance
    /// signal worth surfacing in the list.
    pub purge_protection_enabled: Option<bool>,
    /// `properties.publicNetworkAccess`. `Enabled` / `Disabled`.
    pub public_network_access: Option<String>,
}

impl KeyVault {
    /// `vaultUri` from Resource Graph when present; otherwise the canonical
    /// `https://{name}.vault.azure.net/`. Resource Graph reliably populates
    /// the field on standard public-cloud vaults; the fallback covers sovereign
    /// clouds where the suffix differs only if the user reaches them anyway.
    pub fn vault_uri_or_default(&self) -> String {
        self.vault_uri
            .clone()
            .unwrap_or_else(|| format!("https://{}.vault.azure.net/", self.name))
    }
}

/// Which kind of item this row represents inside a vault. Drives the
/// data-plane URL segment (`/secrets` vs `/certificates`) and the UI filter.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub enum ItemKind {
    /// Default kind the items view lands on. Most teams reach for secrets first.
    #[default]
    Secret,
    Certificate,
}

impl ItemKind {
    /// Data-plane URL segment for this kind — slotted between the vault URI
    /// and the api-version query.
    pub fn path_segment(self) -> &'static str {
        match self {
            ItemKind::Secret => "secrets",
            ItemKind::Certificate => "certificates",
        }
    }

    /// Short human label used in UI columns and error messages.
    pub fn label(self) -> &'static str {
        match self {
            ItemKind::Secret => "secret",
            ItemKind::Certificate => "certificate",
        }
    }
}

/// One secret or certificate inside a vault. Metadata only — the secret value
/// and the certificate bytes are never fetched.
#[derive(Clone, Debug)]
pub struct KeyVaultItem {
    pub kind: ItemKind,
    /// Short name (last segment of the data-plane id URL).
    pub name: String,
    /// `attributes.enabled`. Disabled items still appear in `list` responses.
    pub enabled: Option<bool>,
    /// `attributes.exp` parsed from Unix epoch seconds. `None` = no expiry set.
    pub expires: Option<DateTime<Utc>>,
    /// `attributes.nbf` (not-before) parsed from Unix epoch seconds.
    pub not_before: Option<DateTime<Utc>>,
    /// `attributes.created` parsed from Unix epoch seconds.
    pub created: Option<DateTime<Utc>>,
    /// `attributes.updated` parsed from Unix epoch seconds.
    pub updated: Option<DateTime<Utc>>,
    /// `contentType` — typically `application/x-pkcs12` for certs, free-form
    /// for secrets (often empty). Stored verbatim.
    pub content_type: Option<String>,
}

/// Resource Graph KQL for Key Vaults. Same envelope as
/// [`super::cosmos::COSMOS_KQL`] / [`super::registries::REGISTRIES_KQL`].
const VAULTS_KQL: &str = r#"
Resources
| where type == 'microsoft.keyvault/vaults'
| project id, name, type, location, resourceGroup, subscriptionId, properties
| order by name asc
"#;

/// Key Vault data-plane REST API version. `7.4` is the current GA — `7.5`
/// adds preview surface we don't use.
const KV_API_VERSION: &str = "7.4";

/// Max items returned per data-plane page. Server cap is 25; we ask for the
/// max so a typical vault comes back in 1–2 round-trips.
const PAGE_SIZE: u32 = 25;

/// Soft cap on total items returned per list call. Beyond this we stop
/// following `nextLink` and warn — matches the precedent in `storage.rs` and
/// `registries.rs`.
const MAX_ITEMS: usize = 5_000;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Enumerate Key Vaults across `subscription_ids`. Empty slice → all
/// subscriptions visible to the credential.
pub async fn list_vaults(
    auth: &AzureAuth,
    subscription_ids: &[String],
) -> anyhow::Result<Vec<KeyVault>> {
    let client = ArmClient::new(auth.clone())?;

    let body = if subscription_ids.is_empty() {
        serde_json::json!({ "query": VAULTS_KQL })
    } else {
        serde_json::json!({
            "subscriptions": subscription_ids,
            "query": VAULTS_KQL,
        })
    };

    let resp = client
        .post(
            "/providers/Microsoft.ResourceGraph/resources?api-version=2022-10-01",
            &body,
        )
        .await
        .context("resource graph: list key vaults")?;

    let rows = resp
        .get("data")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("resource graph response missing 'data' array"))?;

    if rows.len() >= 1000 {
        tracing::warn!(
            "resource graph returned {} key vaults; pagination not implemented in v1",
            rows.len()
        );
    }

    Ok(rows.iter().filter_map(parse_vault).collect())
}

/// List secrets or certificates inside `vault` via the data plane. Follows
/// `nextLink` pages until exhausted or [`MAX_ITEMS`] is reached. Requires the
/// signed-in identity to have a `list`-permitting role (RBAC) or access policy
/// on the vault.
pub async fn list_items(
    auth: &AzureAuth,
    vault: &KeyVault,
    kind: ItemKind,
) -> anyhow::Result<Vec<KeyVaultItem>> {
    let host_uri = vault.vault_uri_or_default();
    let host = vault_host(&host_uri).unwrap_or_else(|| vault.name.clone());
    let bearer = auth
        .token(SCOPE_KEY_VAULT)
        .await
        .context("acquire Key Vault data-plane token")?;
    let http = build_http()?;

    let trimmed = host_uri.trim_end_matches('/');
    let mut url = format!(
        "{trimmed}/{}?api-version={KV_API_VERSION}&maxresults={PAGE_SIZE}",
        kind.path_segment()
    );
    let mut out: Vec<KeyVaultItem> = Vec::new();

    loop {
        let auth_value = HeaderValue::from_str(&format!("Bearer {bearer}"))
            .map_err(|_| anyhow!("Key Vault bearer contained invalid header characters"))?;
        // Retried: Key Vault throttles list calls (429) and a metadata GET is
        // safe to replay.
        let resp = send_with_retry(|| http.get(&url).header(AUTHORIZATION, auth_value.clone()))
            .await
            .map_err(|e| anyhow!("key vault data-plane network error: {e}"))
            .map_err(|e| classify_data_plane_error(&vault.name, &host, e))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(classify_data_plane_error(
                &vault.name,
                &host,
                anyhow!(
                    "key vault data-plane returned {}:\n{}",
                    status.as_u16(),
                    format_error_body(&body)
                ),
            ));
        }
        let text = resp
            .text()
            .await
            .map_err(|e| anyhow!("read key vault data-plane body: {e}"))?;
        let page: ListResponse = serde_json::from_str(&text)
            .map_err(|e| anyhow!("parse key vault /{} response: {e}", kind.path_segment()))?;
        for row in page.value {
            if let Some(item) = parse_item(kind, &row) {
                out.push(item);
            }
        }
        if out.len() >= MAX_ITEMS {
            tracing::warn!(
                "key vault /{}: stopping at {} items for {host}; pagination cap reached",
                kind.path_segment(),
                out.len()
            );
            break;
        }
        match page.next_link {
            Some(next) if !next.is_empty() => url = next,
            _ => break,
        }
    }

    Ok(out)
}

/// Fetch the plaintext **value** of a single secret via the data plane.
///
/// Unlike [`list_items`] — which is metadata-only by design — this returns the
/// secret material, so it is only ever called on an explicit user reveal
/// (`x` / Enter in the items view), never during listing. Requires the
/// signed-in identity to have `get` on the secret (RBAC `Key Vault Secrets
/// User`, or a `get` access policy). Secrets-only: certificates have no
/// plaintext value to decode.
pub async fn get_secret_value(
    auth: &AzureAuth,
    vault: &KeyVault,
    name: &str,
) -> anyhow::Result<String> {
    let host_uri = vault.vault_uri_or_default();
    let host = vault_host(&host_uri).unwrap_or_else(|| vault.name.clone());
    let bearer = auth
        .token(SCOPE_KEY_VAULT)
        .await
        .context("acquire Key Vault data-plane token")?;
    let http = build_http()?;

    // Version-less GET resolves to the secret's current version. Secret names
    // are restricted by Azure to `[0-9a-zA-Z-]`, so no percent-encoding needed.
    let trimmed = host_uri.trim_end_matches('/');
    let url = format!("{trimmed}/secrets/{name}?api-version={KV_API_VERSION}");

    let auth_value = HeaderValue::from_str(&format!("Bearer {bearer}"))
        .map_err(|_| anyhow!("Key Vault bearer contained invalid header characters"))?;
    // Retried: revealing a secret is a plain GET, safe to replay on 429/5xx.
    let resp = send_with_retry(|| http.get(&url).header(AUTHORIZATION, auth_value.clone()))
        .await
        .map_err(|e| anyhow!("key vault data-plane network error: {e}"))
        .map_err(|e| classify_data_plane_error(&vault.name, &host, e))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(classify_data_plane_error(
            &vault.name,
            &host,
            anyhow!(
                "key vault data-plane returned {}:\n{}",
                status.as_u16(),
                format_error_body(&body)
            ),
        ));
    }
    let text = resp
        .text()
        .await
        .map_err(|e| anyhow!("read key vault secret body: {e}"))?;
    let parsed: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| anyhow!("parse key vault secret response: {e}"))?;
    parsed
        .get("value")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("key vault secret response had no `value` field"))
}

// ---------------------------------------------------------------------------
// HTTP plumbing
// ---------------------------------------------------------------------------

/// Render an error response body for display. Key Vault returns its standard
/// `{"error":{"code":…,"message":…,"innererror":…}}` envelope as JSON; pretty-
/// print it so the (wrapped) error reads as indented structure rather than one
/// dense blob. Bodies that aren't JSON — some proxy/401 cases — pass through
/// unchanged. Either way the result is length-capped by [`truncate_error_body`].
fn format_error_body(s: &str) -> String {
    let pretty = serde_json::from_str::<serde_json::Value>(s.trim())
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| s.to_string());
    truncate_error_body(&pretty)
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

/// Pull the host out of `https://vault.vault.azure.net/` for error messages
/// — so DNS-fail diagnostics name the host the user can ping.
fn vault_host(uri: &str) -> Option<String> {
    let after_scheme = uri
        .strip_prefix("https://")
        .or_else(|| uri.strip_prefix("http://"))?;
    let host_with_port = after_scheme.split('/').next()?;
    let host = host_with_port.split(':').next()?;
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

// ---------------------------------------------------------------------------
// Parsers
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ListResponse {
    #[serde(default)]
    value: Vec<serde_json::Value>,
    #[serde(rename = "nextLink", default)]
    next_link: Option<String>,
}

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

pub(crate) fn parse_vault(v: &serde_json::Value) -> Option<KeyVault> {
    let ty = v.get("type")?.as_str()?.to_lowercase();
    if ty != "microsoft.keyvault/vaults" {
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
    let props = v.get("properties");
    let sku = props
        .and_then(|p| p.get("sku"))
        .and_then(|s| s.get("name"))
        .and_then(|n| n.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let vault_uri = props
        .and_then(|p| p.get("vaultUri"))
        .and_then(|n| n.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let rbac_authorization_enabled = props
        .and_then(|p| p.get("enableRbacAuthorization"))
        .and_then(extract_bool);
    let soft_delete_enabled = props
        .and_then(|p| p.get("enableSoftDelete"))
        .and_then(extract_bool);
    let purge_protection_enabled = props
        .and_then(|p| p.get("enablePurgeProtection"))
        .and_then(extract_bool);
    let public_network_access = props
        .and_then(|p| p.get("publicNetworkAccess"))
        .and_then(|n| n.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    Some(KeyVault {
        id,
        name,
        resource_group,
        subscription_id,
        location,
        sku,
        vault_uri,
        rbac_authorization_enabled,
        soft_delete_enabled,
        purge_protection_enabled,
        public_network_access,
    })
}

/// A parsed Function App `@Microsoft.KeyVault(...)` app-setting reference.
/// Both documented shapes are accepted:
///   `@Microsoft.KeyVault(SecretUri=https://v.vault.azure.net/secrets/name/ver)`
///   `@Microsoft.KeyVault(VaultName=v;SecretName=name;SecretVersion=ver)`
/// Only the vault + secret name are kept; any version is dropped (a reveal
/// always resolves the secret's current version).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyVaultRef {
    /// Short vault name (e.g. `myvault`) — the first DNS label of the host in
    /// the `SecretUri` form, or the literal `VaultName` value.
    pub vault_name: String,
    /// Full data-plane vault URI when derivable (the `SecretUri` form carries
    /// it). `None` for the `VaultName=` form, where callers fall back to the
    /// canonical `https://{vault_name}.vault.azure.net/`.
    pub vault_uri: Option<String>,
    /// Secret name to reveal.
    pub secret_name: String,
}

/// Parse a Function App Key Vault reference (an app-setting value shaped like
/// `@Microsoft.KeyVault(...)`). Returns `None` for anything else — including
/// Container App `secretRef` markers, which point at container-app secrets
/// rather than a vault. Both the `SecretUri=` and `VaultName=;SecretName=`
/// shapes are recognized.
pub fn parse_key_vault_ref(value: &str) -> Option<KeyVaultRef> {
    let inner = value
        .trim()
        .strip_prefix("@Microsoft.KeyVault(")?
        .strip_suffix(')')?;

    let mut secret_uri = None;
    let mut vault_name = None;
    let mut secret_name = None;
    for part in inner.split(';') {
        // Skip empty or `=`-less parts instead of failing the whole parse —
        // App Service tolerates a trailing `;`, so a followable reference must
        // not turn into `None` over one.
        let Some((k, v)) = part.split_once('=') else {
            continue;
        };
        match k.trim() {
            "SecretUri" => secret_uri = Some(v.trim().to_string()),
            "VaultName" => vault_name = Some(v.trim().to_string()),
            "SecretName" => secret_name = Some(v.trim().to_string()),
            _ => {}
        }
    }

    if let Some(uri) = secret_uri {
        key_vault_ref_from_secret_uri(&uri)
    } else {
        Some(KeyVaultRef {
            vault_name: vault_name?,
            vault_uri: None,
            secret_name: secret_name?,
        })
    }
}

/// Parse a bare data-plane secret URL — e.g.
/// `https://myvault.vault.azure.net/secrets/api-key/abc123` — into a
/// [`KeyVaultRef`]. This is the `SecretUri=` payload of a Function App
/// reference, and also the `keyVaultUrl` carried by a Key Vault-backed
/// Container App secret. Returns `None` if the host or secret name can't be
/// extracted.
pub fn key_vault_ref_from_secret_uri(uri: &str) -> Option<KeyVaultRef> {
    let host = vault_host(uri)?;
    let name = item_name_from_id(uri)?;
    let label = host.split('.').next().unwrap_or(&host).to_string();
    Some(KeyVaultRef {
        vault_name: label,
        vault_uri: Some(format!("https://{host}/")),
        secret_name: name,
    })
}

/// Pull the trailing path segment out of a Key Vault data-plane id URL like
/// `https://myvault.vault.azure.net/secrets/my-secret/abc123`. We want the
/// secret/cert *name*, which is the second-to-last segment when a version
/// suffix is present and the last when it isn't. The list API generally
/// returns version-less ids, but the data plane sometimes echoes a versioned
/// id — handle both shapes defensively.
pub(crate) fn item_name_from_id(id: &str) -> Option<String> {
    let after_host = id.strip_prefix("https://")?.split_once('/')?.1;
    let mut segments: Vec<&str> = after_host.split('/').filter(|s| !s.is_empty()).collect();
    // Drop the kind segment (`secrets` / `certificates` / `keys`) if present
    // at the front so we don't accidentally pick it up as the name.
    if matches!(
        segments.first().copied(),
        Some("secrets" | "certificates" | "keys")
    ) {
        segments.remove(0);
    }
    segments.first().map(|s| s.to_string())
}

pub(crate) fn parse_item(kind: ItemKind, v: &serde_json::Value) -> Option<KeyVaultItem> {
    let id = v.get("id").and_then(|n| n.as_str())?;
    let name = item_name_from_id(id)?;
    let attrs = v.get("attributes");
    let enabled = attrs.and_then(|a| a.get("enabled")).and_then(extract_bool);
    let expires = attrs.and_then(|a| a.get("exp")).and_then(parse_epoch);
    let not_before = attrs.and_then(|a| a.get("nbf")).and_then(parse_epoch);
    let created = attrs.and_then(|a| a.get("created")).and_then(parse_epoch);
    let updated = attrs.and_then(|a| a.get("updated")).and_then(parse_epoch);
    let content_type = v
        .get("contentType")
        .and_then(|n| n.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    Some(KeyVaultItem {
        kind,
        name,
        enabled,
        expires,
        not_before,
        created,
        updated,
        content_type,
    })
}

/// Parse Key Vault's epoch-seconds timestamp shape. The data plane returns
/// these as JSON numbers (e.g. `"exp": 1791292800`); both i64 and u64 paths
/// are handled because serde-json widens depending on magnitude.
fn parse_epoch(v: &serde_json::Value) -> Option<DateTime<Utc>> {
    let secs = if let Some(n) = v.as_i64() {
        n
    } else if let Some(n) = v.as_u64() {
        i64::try_from(n).ok()?
    } else if let Some(f) = v.as_f64() {
        // Some legacy responses returned floats; truncate to whole seconds.
        f as i64
    } else {
        return None;
    };
    Utc.timestamp_opt(secs, 0).single()
}

// ---------------------------------------------------------------------------
// Error classification
// ---------------------------------------------------------------------------

fn classify_data_plane_error(vault_name: &str, host: &str, e: anyhow::Error) -> anyhow::Error {
    let msg = format!("{e}");
    let lower = msg.to_lowercase();
    if lower.contains(" 401 ") || lower.contains("returned 401") || lower.contains("unauthorized") {
        return anyhow!(
            "401 from Key Vault data plane: identity rejected by '{vault_name}'. \
             Sign in with `az login` to refresh the AAD token.\nunderlying: {msg}"
        );
    }
    if lower.contains(" 403 ") || lower.contains("returned 403") || lower.contains("forbidden") {
        return anyhow!(
            "403 from Key Vault data plane on '{vault_name}': identity lacks \
             `list` permission. If the vault uses RBAC (the modern default), \
             assign `Key Vault Reader` (or `Key Vault Secrets User` for \
             secrets / `Key Vault Certificate User` for certs). If the vault \
             still uses access policies, grant `list` on the relevant object \
             types. Control-plane `Reader` is not enough.\nunderlying: {msg}"
        );
    }
    if lower.contains("dns")
        || lower.contains("failed to lookup")
        || lower.contains("no such host")
        || lower.contains("name or service not known")
    {
        return anyhow!(
            "DNS lookup failed for '{host}' — does vault '{vault_name}' exist \
             (or is it firewalled to a private endpoint)?\nunderlying: {msg}"
        );
    }
    e
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn format_error_body_pretty_prints_json_envelope() {
        let raw = r#"{"error":{"code":"Forbidden","message":"no list permission","innererror":{"code":"AccessDenied"}}}"#;
        let out = format_error_body(raw);
        // Pretty-printing introduces newlines and indentation; the original is
        // a single line.
        assert!(out.contains('\n'), "expected multi-line output, got: {out}");
        assert!(out.contains("\"code\": \"Forbidden\""));
        assert!(out.contains("\"code\": \"AccessDenied\""));
    }

    #[test]
    fn classify_breaks_underlying_and_json_onto_their_own_lines() {
        // Mirror the real call path: the body fetch builds
        // "…returned 403:\n{pretty json}" and classify prepends guidance plus
        // "\nunderlying: ".
        let body = r#"{"error":{"code":"Forbidden","message":"no list permission"}}"#;
        let inner = anyhow!(
            "key vault data-plane returned 403:\n{}",
            format_error_body(body)
        );
        let msg = format!(
            "{}",
            classify_data_plane_error("v", "v.vault.azure.net", inner)
        );
        assert!(
            msg.contains("not enough.\nunderlying:"),
            "guidance and `underlying:` must be on separate lines:\n{msg}"
        );
        assert!(
            msg.contains("returned 403:\n{"),
            "the JSON body must start on its own line:\n{msg}"
        );
    }

    #[test]
    fn format_error_body_passes_through_non_json() {
        let raw = "502 Bad Gateway (html proxy page)";
        assert_eq!(format_error_body(raw), raw);
    }

    #[test]
    fn parses_vault_row() {
        let row = json!({
            "id": "/subscriptions/s/resourceGroups/rg/providers/Microsoft.KeyVault/vaults/myvault",
            "name": "myvault",
            "type": "microsoft.keyvault/vaults",
            "location": "westeurope",
            "resourceGroup": "rg",
            "subscriptionId": "s",
            "properties": {
                "sku": { "name": "standard", "family": "A" },
                "vaultUri": "https://myvault.vault.azure.net/",
                "enableRbacAuthorization": true,
                "enableSoftDelete": true,
                "enablePurgeProtection": false,
                "publicNetworkAccess": "Enabled"
            }
        });
        let v = parse_vault(&row).expect("expected vault");
        assert_eq!(v.name, "myvault");
        assert_eq!(v.sku.as_deref(), Some("standard"));
        assert_eq!(
            v.vault_uri.as_deref(),
            Some("https://myvault.vault.azure.net/")
        );
        assert_eq!(v.rbac_authorization_enabled, Some(true));
        assert_eq!(v.purge_protection_enabled, Some(false));
        assert_eq!(v.public_network_access.as_deref(), Some("Enabled"));
    }

    #[test]
    fn vault_uri_falls_back_to_canonical_host() {
        let mut v = KeyVault {
            id: "id".into(),
            name: "legacy".into(),
            resource_group: "rg".into(),
            subscription_id: "s".into(),
            location: "westeurope".into(),
            sku: None,
            vault_uri: None,
            rbac_authorization_enabled: None,
            soft_delete_enabled: None,
            purge_protection_enabled: None,
            public_network_access: None,
        };
        assert_eq!(v.vault_uri_or_default(), "https://legacy.vault.azure.net/");
        v.vault_uri = Some("https://legacy.vault.azure.net/".into());
        assert_eq!(v.vault_uri_or_default(), "https://legacy.vault.azure.net/");
    }

    #[test]
    fn skips_non_vault_rows() {
        let row = json!({
            "id": "/subs/x/rg/y/providers/Microsoft.Web/sites/z",
            "name": "z",
            "type": "microsoft.web/sites",
            "location": "westeurope",
            "resourceGroup": "y",
            "subscriptionId": "x"
        });
        assert!(parse_vault(&row).is_none());
    }

    #[test]
    fn parse_key_vault_ref_secret_uri_form() {
        let r = parse_key_vault_ref(
            "@Microsoft.KeyVault(SecretUri=https://myvault.vault.azure.net/secrets/api-key/abc123)",
        )
        .expect("should parse SecretUri form");
        assert_eq!(r.vault_name, "myvault");
        assert_eq!(
            r.vault_uri.as_deref(),
            Some("https://myvault.vault.azure.net/")
        );
        assert_eq!(r.secret_name, "api-key");
    }

    #[test]
    fn parse_key_vault_ref_secret_uri_without_version() {
        let r = parse_key_vault_ref(
            "@Microsoft.KeyVault(SecretUri=https://v.vault.azure.net/secrets/db-pass/)",
        )
        .expect("should parse versionless SecretUri form");
        assert_eq!(r.vault_name, "v");
        assert_eq!(r.secret_name, "db-pass");
    }

    #[test]
    fn parse_key_vault_ref_vault_name_form() {
        let r = parse_key_vault_ref(
            "@Microsoft.KeyVault(VaultName=myvault;SecretName=api-key;SecretVersion=abc)",
        )
        .expect("should parse VaultName form");
        assert_eq!(r.vault_name, "myvault");
        assert_eq!(r.vault_uri, None);
        assert_eq!(r.secret_name, "api-key");
    }

    #[test]
    fn key_vault_ref_from_secret_uri_parses_bare_url() {
        // The `keyVaultUrl` carried by a Container App secret is a bare
        // data-plane URL (no `@Microsoft.KeyVault(...)` wrapper).
        let r = key_vault_ref_from_secret_uri(
            "https://kv-contoso-prod.vault.azure.net/secrets/orders-db-connection",
        )
        .expect("should parse bare secret URL");
        assert_eq!(r.vault_name, "kv-contoso-prod");
        assert_eq!(
            r.vault_uri.as_deref(),
            Some("https://kv-contoso-prod.vault.azure.net/")
        );
        assert_eq!(r.secret_name, "orders-db-connection");
        assert!(key_vault_ref_from_secret_uri("not-a-url").is_none());
    }

    #[test]
    fn parse_key_vault_ref_tolerates_trailing_semicolon() {
        // App Service accepts and resolves a reference with a trailing `;`,
        // so the parser must too (a `=`-less part is skipped, not fatal).
        let r = parse_key_vault_ref("@Microsoft.KeyVault(VaultName=myvault;SecretName=api-key;)")
            .expect("trailing semicolon should not break the parse");
        assert_eq!(r.vault_name, "myvault");
        assert_eq!(r.secret_name, "api-key");

        let r = parse_key_vault_ref(
            "@Microsoft.KeyVault(SecretUri=https://v.vault.azure.net/secrets/db-pass;)",
        )
        .expect("trailing semicolon after SecretUri should not break the parse");
        assert_eq!(r.secret_name, "db-pass");
    }

    #[test]
    fn parse_key_vault_ref_rejects_non_references() {
        // Plain literal, Container App secretRef marker, and an empty/garbage
        // value all fail to parse.
        assert!(parse_key_vault_ref("plain-value").is_none());
        assert!(parse_key_vault_ref("(secret: db-password)").is_none());
        assert!(parse_key_vault_ref("@Microsoft.KeyVault(VaultName=v)").is_none());
    }

    #[test]
    fn item_name_from_id_handles_version_suffix_and_kind_prefix() {
        assert_eq!(
            item_name_from_id("https://v.vault.azure.net/secrets/my-secret").as_deref(),
            Some("my-secret"),
        );
        assert_eq!(
            item_name_from_id("https://v.vault.azure.net/secrets/my-secret/abc123").as_deref(),
            Some("my-secret"),
        );
        assert_eq!(
            item_name_from_id("https://v.vault.azure.net/certificates/wildcard-cert").as_deref(),
            Some("wildcard-cert"),
        );
        assert_eq!(item_name_from_id("not-a-url"), None);
    }

    #[test]
    fn parses_secret_item_with_attributes() {
        let row = json!({
            "id": "https://v.vault.azure.net/secrets/my-secret",
            "contentType": "application/json",
            "attributes": {
                "enabled": true,
                "created": 1_700_000_000,
                "updated": 1_710_000_000,
                "exp": 1_791_292_800,
                "nbf": 1_700_000_000
            },
            "tags": { "env": "prod" }
        });
        let item = parse_item(ItemKind::Secret, &row).expect("expected item");
        assert_eq!(item.name, "my-secret");
        assert_eq!(item.enabled, Some(true));
        assert!(item.expires.is_some());
        assert_eq!(item.expires.map(|d| d.timestamp()), Some(1_791_292_800));
        assert_eq!(item.content_type.as_deref(), Some("application/json"));
    }

    #[test]
    fn parses_item_without_attributes() {
        // Disabled / freshly-created items occasionally come back without an
        // attributes block. Parser must accept them and return None timestamps
        // rather than panic.
        let row = json!({
            "id": "https://v.vault.azure.net/secrets/bare"
        });
        let item = parse_item(ItemKind::Secret, &row).expect("expected item");
        assert_eq!(item.name, "bare");
        assert_eq!(item.enabled, None);
        assert!(item.expires.is_none());
    }

    #[test]
    fn parse_epoch_accepts_i64_u64_and_f64() {
        assert_eq!(
            parse_epoch(&json!(1_791_292_800_i64)).map(|d| d.timestamp()),
            Some(1_791_292_800),
        );
        assert_eq!(
            parse_epoch(&json!(1_791_292_800_u64)).map(|d| d.timestamp()),
            Some(1_791_292_800),
        );
        assert_eq!(
            parse_epoch(&json!(1_791_292_800.5_f64)).map(|d| d.timestamp()),
            Some(1_791_292_800),
        );
        assert!(parse_epoch(&json!("not-a-number")).is_none());
        assert!(parse_epoch(&json!(null)).is_none());
    }

    #[test]
    fn vault_host_extracts_host_without_port_or_trailing_slash() {
        assert_eq!(
            vault_host("https://myvault.vault.azure.net/"),
            Some("myvault.vault.azure.net".to_string()),
        );
        assert_eq!(
            vault_host("https://myvault.vault.azure.net:443/"),
            Some("myvault.vault.azure.net".to_string()),
        );
        assert_eq!(vault_host("not-a-url"), None);
    }

    #[test]
    fn item_kind_path_segments_are_stable() {
        assert_eq!(ItemKind::Secret.path_segment(), "secrets");
        assert_eq!(ItemKind::Certificate.path_segment(), "certificates");
    }

    #[test]
    fn classifies_401_with_login_hint() {
        let err = classify_data_plane_error(
            "v",
            "v.vault.azure.net",
            anyhow!("key vault data-plane returned 401: unauthorized"),
        );
        let msg = format!("{err}");
        assert!(msg.contains("az login"), "got: {msg}");
        assert!(msg.contains("'v'"), "got: {msg}");
    }

    #[test]
    fn classifies_403_with_both_auth_model_hints() {
        let err = classify_data_plane_error(
            "v",
            "v.vault.azure.net",
            anyhow!("key vault data-plane returned 403: forbidden"),
        );
        let msg = format!("{err}");
        assert!(
            msg.contains("RBAC") && msg.contains("access polic"),
            "expected both RBAC and access policy hints, got: {msg}"
        );
    }

    #[test]
    fn classifies_dns_failure_with_host() {
        let err = classify_data_plane_error(
            "ghost",
            "ghost.vault.azure.net",
            anyhow!("key vault data-plane network error: dns error: failed to lookup address"),
        );
        let msg = format!("{err}");
        assert!(msg.contains("ghost.vault.azure.net"), "got: {msg}");
    }

    #[test]
    fn truncate_error_body_bounds_oversized_strings() {
        let big = "x".repeat(5_000);
        let out = truncate_error_body(&big);
        assert!(out.ends_with('…'));
        assert!(out.chars().count() <= 1_025);
    }

    #[test]
    fn list_response_parses_with_and_without_next_link() {
        let body = r#"{"value":[{"id":"https://v.vault.azure.net/secrets/a"}],"nextLink":"https://v.vault.azure.net/secrets?api-version=7.4&$skiptoken=xxx"}"#;
        let page: ListResponse = serde_json::from_str(body).unwrap();
        assert_eq!(page.value.len(), 1);
        assert!(page.next_link.is_some());

        let body = r#"{"value":[]}"#;
        let page: ListResponse = serde_json::from_str(body).unwrap();
        assert!(page.value.is_empty());
        assert!(page.next_link.is_none());
    }
}
