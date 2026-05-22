//! Read-only Azure Cosmos DB inspection (SQL/Core API only).
//!
//! ## Contract (do not change without coordinating with the UI lane)
//!
//! Four public functions form the surface the UI consumes:
//!
//! - [`list_accounts`] — Resource Graph KQL discovery of Cosmos DB accounts
//!   across the supplied subscriptions, filtered to SQL/Core API accounts
//!   (`kind == GlobalDocumentDB` AND no Cassandra/Gremlin/Table capability).
//!   Control plane only (`Reader` is sufficient).
//! - [`list_databases`] — ARM control-plane enumeration of SQL databases under
//!   one account. Same auth as the account discovery.
//! - [`list_containers`] — ARM control-plane enumeration of containers
//!   (collections) under one database, including partition key + indexing mode
//!   + default TTL from `properties.resource`.
//! - [`query_top_items`] — Cosmos **data plane** `POST /dbs/{db}/colls/{coll}/docs`
//!   running `SELECT TOP 20 * FROM c`. Requires the signed-in identity to have
//!   the `Cosmos DB Built-in Data Reader` role assigned at the account scope
//!   via `dataPlaneRoleDefinitions` — control-plane `Reader` is NOT enough.
//!
//! ## Scope decisions worth flagging
//!
//! - **SQL/Core only**: Cassandra, Gremlin, Table accounts share `kind ==
//!   GlobalDocumentDB` but expose those APIs via `properties.capabilities[]`
//!   flags (`EnableCassandra`, etc.). The Rust parser filters them out *after*
//!   KQL — `properties.capabilities` is a dynamic array and `mv-apply`/
//!   `set_has_any` in KQL has been brittle when the field is absent. MongoDB
//!   has its own `kind == MongoDB` so the KQL already excludes it.
//! - **AAD bearer (no HMAC, no exchange)**: Cosmos SQL API accepts AAD tokens
//!   for `https://cosmos.azure.com/.default` directly — there's no two-step
//!   exchange like ACR. The required header shape is
//!   `Authorization: type%3Daad%26ver%3D1.0%26sig%3D{jwt}` (URL-encoded
//!   per the Cosmos REST docs).
//! - **Read-only**: account list + database list + container list + item query
//!   only. No item write / DDL / throughput-change codepaths, even stubs.
//! - **Item preview cap**: `SELECT TOP 20`; we don't follow `x-ms-continuation`
//!   even if it appears — the UI sets `partial = true` and shows a warning.

#![allow(dead_code, unused_variables)]

use anyhow::{anyhow, Context};
use chrono::{DateTime, Utc};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};

use crate::azure::auth::{AzureAuth, SCOPE_COSMOS};
use crate::azure::client::ArmClient;

/// One Cosmos DB account discovered via Resource Graph. Always SQL/Core API
/// after the post-KQL capability filter.
#[derive(Clone, Debug)]
pub struct CosmosAccount {
    /// Full ARM resource id.
    pub id: String,
    pub name: String,
    pub resource_group: String,
    pub subscription_id: String,
    pub location: String,
    /// `kind` from Resource Graph — always `GlobalDocumentDB` post-filter.
    pub kind: Option<String>,
    /// `properties.documentEndpoint`, e.g. `https://acc.documents.azure.com:443/`.
    /// Source of truth for the data-plane host. Falls back to
    /// `https://{name}.documents.azure.com:443/` when missing — see
    /// [`Self::document_endpoint_or_default`].
    pub document_endpoint: Option<String>,
    /// Lowercased capability names from `properties.capabilities[].name`.
    /// SQL accounts have either an empty array or `enableserverless`.
    pub capabilities: Vec<String>,
    /// Derived: `capabilities` contains `enableserverless`. Serverless accounts
    /// have no provisioned throughput and bill per-request.
    pub is_serverless: bool,
    /// `properties.publicNetworkAccess`. `Enabled` / `Disabled`.
    pub public_network_access: Option<String>,
    /// `properties.systemData.createdAt` or `properties.createTime` parsed to
    /// UTC. `None` when both are missing.
    pub created_at: Option<DateTime<Utc>>,
}

impl CosmosAccount {
    /// `documentEndpoint` from Resource Graph when present; otherwise the
    /// canonical `https://{name}.documents.azure.com:443/`. The portal always
    /// renders the field, but Resource Graph has been observed to omit it on
    /// some legacy accounts.
    pub fn document_endpoint_or_default(&self) -> String {
        self.document_endpoint
            .clone()
            .unwrap_or_else(|| format!("https://{}.documents.azure.com:443/", self.name))
    }
}

/// One SQL database inside an account.
#[derive(Clone, Debug)]
pub struct CosmosDatabase {
    /// Full ARM resource id (`{account.id}/sqlDatabases/{name}`).
    pub id: String,
    pub name: String,
}

/// One SQL container (collection) inside a database.
#[derive(Clone, Debug)]
pub struct CosmosContainer {
    /// Full ARM resource id (`{account.id}/sqlDatabases/{db}/containers/{name}`).
    pub id: String,
    pub name: String,
    /// `properties.resource.partitionKey.paths`. Typically a single path like
    /// `/userId`; multi-path is rare but supported by the schema.
    pub partition_key_paths: Vec<String>,
    /// `properties.resource.partitionKey.kind`. Usually `Hash`. `None` when
    /// the container predates the field (very old accounts).
    pub partition_key_kind: Option<String>,
    /// `properties.resource.defaultTtl`. `-1` = TTL enabled but no default,
    /// `0` / missing = disabled, positive = seconds-until-expiry default.
    pub default_ttl: Option<i64>,
    /// `properties.resource.indexingPolicy.indexingMode`. Usually `consistent`.
    pub indexing_mode: Option<String>,
}

/// Output of [`query_top_items`]: the first N documents from the container
/// plus the Cosmos-reported request charge.
#[derive(Clone, Debug)]
pub struct CosmosItemPreview {
    /// The raw `Documents[]` rows from the response. Capped at
    /// [`MAX_ITEMS_PREVIEW`] by the query itself (`SELECT TOP N`); we don't
    /// follow continuation tokens.
    pub items: Vec<serde_json::Value>,
    /// `x-ms-request-charge` from the response header — useful diagnostic for
    /// "how expensive was this exploratory read".
    pub request_charge: Option<f64>,
    /// `true` if the response carried an `x-ms-continuation` token we ignored.
    /// The UI surfaces this as "showing first N (more available)".
    pub partial: bool,
}

/// Resource Graph KQL for Cosmos DB accounts. Same envelope as
/// [`super::registries::REGISTRIES_KQL`]. The capability filter is applied
/// in Rust — see [`parse_account`] — because dynamic-array filtering in KQL
/// has been observed to behave inconsistently across tenants when the field
/// is absent.
const COSMOS_KQL: &str = r#"
Resources
| where type == 'microsoft.documentdb/databaseaccounts'
| where kind =~ 'GlobalDocumentDB'
| project id, name, type, kind, location, resourceGroup, subscriptionId, properties
| order by name asc
"#;

/// API version used for both `sqlDatabases` and `containers` endpoints.
const COSMOS_API_VERSION: &str = "2024-05-15";

/// Max items returned by `query_top_items`. We embed it in the SQL (`SELECT
/// TOP N`) AND pass it as `x-ms-max-item-count` so the server caps the page
/// regardless of statement.
const MAX_ITEMS_PREVIEW: usize = 20;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Enumerate SQL/Core Cosmos DB accounts across `subscription_ids`. Empty
/// slice → all subscriptions visible to the credential.
pub async fn list_accounts(
    auth: &AzureAuth,
    subscription_ids: &[String],
) -> anyhow::Result<Vec<CosmosAccount>> {
    let client = ArmClient::new(auth.clone())?;

    let body = if subscription_ids.is_empty() {
        serde_json::json!({ "query": COSMOS_KQL })
    } else {
        serde_json::json!({
            "subscriptions": subscription_ids,
            "query": COSMOS_KQL,
        })
    };

    let resp = client
        .post(
            "/providers/Microsoft.ResourceGraph/resources?api-version=2022-10-01",
            &body,
        )
        .await
        .context("resource graph: list cosmos db accounts")?;

    let rows = resp
        .get("data")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("resource graph response missing 'data' array"))?;

    if rows.len() >= 1000 {
        tracing::warn!(
            "resource graph returned {} cosmos accounts; pagination not implemented in v1",
            rows.len()
        );
    }

    Ok(rows.iter().filter_map(parse_account).collect())
}

/// List SQL databases inside `account` via the ARM control plane.
pub async fn list_databases(
    auth: &AzureAuth,
    account: &CosmosAccount,
) -> anyhow::Result<Vec<CosmosDatabase>> {
    let client = ArmClient::new(auth.clone())?;
    let path = format!("{}/sqlDatabases", account.id);
    let resp = client
        .get(&path, &[("api-version", COSMOS_API_VERSION)])
        .await
        .with_context(|| format!("list sql databases for {}", account.name))?;
    Ok(parse_databases_json(&resp))
}

/// List SQL containers (collections) inside `db_name` of `account`.
pub async fn list_containers(
    auth: &AzureAuth,
    account: &CosmosAccount,
    db_name: &str,
) -> anyhow::Result<Vec<CosmosContainer>> {
    let client = ArmClient::new(auth.clone())?;
    let path = format!("{}/sqlDatabases/{}/containers", account.id, db_name);
    let resp = client
        .get(&path, &[("api-version", COSMOS_API_VERSION)])
        .await
        .with_context(|| format!("list sql containers for {}/{}", account.name, db_name))?;
    Ok(parse_containers_json(&resp))
}

/// Run `SELECT TOP {MAX_ITEMS_PREVIEW} * FROM c` against `coll_name` in
/// `db_name`. Data-plane call — requires the identity to have
/// `Cosmos DB Built-in Data Reader` (or stronger) at the account scope.
pub async fn query_top_items(
    auth: &AzureAuth,
    account: &CosmosAccount,
    db_name: &str,
    coll_name: &str,
) -> anyhow::Result<CosmosItemPreview> {
    let endpoint = account.document_endpoint_or_default();
    let host = endpoint_host(&endpoint).unwrap_or_else(|| account.name.clone());
    let url = items_url(&endpoint, db_name, coll_name);
    let bearer = auth
        .token(SCOPE_COSMOS)
        .await
        .context("acquire Cosmos data-plane token")?;
    let http = build_http()?;

    let auth_header = format!(
        "type%3Daad%26ver%3D1.0%26sig%3D{}",
        urlencode_token(&bearer)
    );
    tracing::debug!(
        "cosmos query: POST {url}, auth-format type=aad&ver=1.0&sig=<redacted, len={}>",
        bearer.len()
    );

    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&auth_header)
            .map_err(|_| anyhow!("cosmos AAD authorization header contained invalid chars"))?,
    );
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/query+json"),
    );
    headers.insert("x-ms-version", HeaderValue::from_static("2018-12-31"));
    headers.insert(
        "x-ms-date",
        HeaderValue::from_str(&rfc1123_now())
            .map_err(|_| anyhow!("rfc1123 date contained invalid chars"))?,
    );
    headers.insert("x-ms-documentdb-isquery", HeaderValue::from_static("true"));
    headers.insert(
        "x-ms-documentdb-query-enablecrosspartition",
        HeaderValue::from_static("true"),
    );
    headers.insert(
        "x-ms-max-item-count",
        HeaderValue::from_str(&MAX_ITEMS_PREVIEW.to_string()).unwrap(),
    );

    let body = serde_json::json!({
        "query": format!("SELECT TOP {MAX_ITEMS_PREVIEW} * FROM c"),
        "parameters": [],
    });

    let resp = http
        .post(&url)
        .headers(headers)
        .body(serde_json::to_vec(&body).expect("cosmos query body is always valid json"))
        .send()
        .await
        .map_err(|e| anyhow!("cosmos data-plane network error: {e}"))
        .map_err(|e| classify_data_plane_error(&account.name, &host, e))?;

    let status = resp.status();
    let response_headers = resp.headers().clone();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(classify_data_plane_error(
            &account.name,
            &host,
            anyhow!(
                "cosmos data-plane returned {}: {}",
                status.as_u16(),
                truncate_error_body(&body)
            ),
        ));
    }

    let request_charge = response_headers
        .get("x-ms-request-charge")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<f64>().ok());
    let partial = response_headers
        .get("x-ms-continuation")
        .map(|v| !v.is_empty())
        .unwrap_or(false);

    let raw = resp
        .text()
        .await
        .map_err(|e| anyhow!("read cosmos response body: {e}"))?;
    let parsed: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| anyhow!("parse cosmos query response: {e}"))?;
    let items = parse_items_response(&parsed);

    Ok(CosmosItemPreview {
        items,
        request_charge,
        partial,
    })
}

// ---------------------------------------------------------------------------
// HTTP plumbing
// ---------------------------------------------------------------------------

fn build_http() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("azpect/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| anyhow!("failed to build reqwest client: {e}"))
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

/// Format `Utc::now()` as an RFC 1123 datestamp (`Sun, 06 Sep 2009 17:39:00
/// GMT`). Cosmos rejects non-GMT dates and dates outside ±15 minutes of server
/// time.
fn rfc1123_now() -> String {
    Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

/// Build the items-query URL by stripping trailing `/` from the endpoint
/// (Resource Graph reports `documentEndpoint` with a trailing slash) and
/// joining with `/dbs/{db}/colls/{coll}/docs`. Factored out for unit testing.
pub(crate) fn items_url(endpoint: &str, db: &str, coll: &str) -> String {
    let trimmed = endpoint.trim_end_matches('/');
    format!("{trimmed}/dbs/{db}/colls/{coll}/docs")
}

/// Pull the host out of an endpoint URL for error classification (so the
/// "DNS lookup failed for X" message names the host the user can ping).
/// Returns `None` if the endpoint isn't a parseable URL.
fn endpoint_host(endpoint: &str) -> Option<String> {
    let after_scheme = endpoint
        .strip_prefix("https://")
        .or_else(|| endpoint.strip_prefix("http://"))?;
    let host_with_port = after_scheme.split('/').next()?;
    let host = host_with_port.split(':').next()?;
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

/// URL-encode a JWT for embedding in `Authorization: type%3Daad%26...&sig%3D`.
/// JWTs contain only `[A-Za-z0-9._-]` which are all safe in URLs, so we encode
/// only the conservative set (`=`, `&`, `+`, `/`, space) just in case. This is
/// intentionally not a full percent-encoder — keeping it tight makes the
/// header value stable across reqwest versions.
fn urlencode_token(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '=' => out.push_str("%3D"),
            '&' => out.push_str("%26"),
            '+' => out.push_str("%2B"),
            '/' => out.push_str("%2F"),
            ' ' => out.push_str("%20"),
            _ => out.push(c),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Parsers
// ---------------------------------------------------------------------------

/// Capability flags that mean "this account is NOT SQL/Core" even though
/// `kind == GlobalDocumentDB`. Matched case-insensitively.
const NON_SQL_CAPABILITIES: &[&str] = &[
    "enablecassandra",
    "enablegremlin",
    "enabletable",
    "enablemongo",
];

pub(crate) fn parse_account(v: &serde_json::Value) -> Option<CosmosAccount> {
    let ty = v.get("type")?.as_str()?.to_lowercase();
    if ty != "microsoft.documentdb/databaseaccounts" {
        return None;
    }
    let kind = v
        .get("kind")
        .and_then(|n| n.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    // Reject anything that isn't GlobalDocumentDB. Mongo is `kind == "MongoDB"`
    // and won't survive this check; Cassandra/Gremlin/Table report
    // GlobalDocumentDB but are eliminated by the capability filter below.
    let kind_str = kind.as_deref().unwrap_or("");
    if !kind_str.eq_ignore_ascii_case("GlobalDocumentDB") {
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
    let document_endpoint = props
        .and_then(|p| p.get("documentEndpoint"))
        .and_then(|n| n.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let public_network_access = props
        .and_then(|p| p.get("publicNetworkAccess"))
        .and_then(|n| n.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let capabilities: Vec<String> = props
        .and_then(|p| p.get("capabilities"))
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|cap| cap.get("name").and_then(|n| n.as_str()))
                .map(|s| s.to_ascii_lowercase())
                .collect()
        })
        .unwrap_or_default();

    // Drop Cassandra/Gremlin/Table/Mongo-API accounts.
    for cap in &capabilities {
        if NON_SQL_CAPABILITIES.contains(&cap.as_str()) {
            return None;
        }
    }
    let is_serverless = capabilities.iter().any(|c| c == "enableserverless");

    // `properties.systemData.createdAt` (newer accounts) or
    // `properties.createTime` (older). Try both.
    let created_at = props
        .and_then(|p| {
            p.get("systemData")
                .and_then(|s| s.get("createdAt"))
                .or_else(|| p.get("createTime"))
        })
        .and_then(|n| n.as_str())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc));

    Some(CosmosAccount {
        id,
        name,
        resource_group,
        subscription_id,
        location,
        kind,
        document_endpoint,
        capabilities,
        is_serverless,
        public_network_access,
        created_at,
    })
}

pub(crate) fn parse_databases_json(v: &serde_json::Value) -> Vec<CosmosDatabase> {
    v.get("value")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|row| {
                    let id = row.get("id")?.as_str()?.to_string();
                    let name = row
                        .get("name")
                        .and_then(|n| n.as_str())
                        .or_else(|| {
                            row.get("properties")
                                .and_then(|p| p.get("resource"))
                                .and_then(|r| r.get("id"))
                                .and_then(|n| n.as_str())
                        })
                        .unwrap_or("")
                        .to_string();
                    if name.is_empty() {
                        None
                    } else {
                        Some(CosmosDatabase { id, name })
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn parse_containers_json(v: &serde_json::Value) -> Vec<CosmosContainer> {
    v.get("value")
        .and_then(|a| a.as_array())
        .map(|arr| arr.iter().filter_map(parse_container_row).collect())
        .unwrap_or_default()
}

fn parse_container_row(row: &serde_json::Value) -> Option<CosmosContainer> {
    let id = row.get("id")?.as_str()?.to_string();
    let name = row
        .get("name")
        .and_then(|n| n.as_str())
        .or_else(|| {
            row.get("properties")
                .and_then(|p| p.get("resource"))
                .and_then(|r| r.get("id"))
                .and_then(|n| n.as_str())
        })
        .unwrap_or("")
        .to_string();
    if name.is_empty() {
        return None;
    }
    let resource = row.get("properties").and_then(|p| p.get("resource"));
    let pk = resource.and_then(|r| r.get("partitionKey"));
    let partition_key_paths: Vec<String> = pk
        .and_then(|p| p.get("paths"))
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|p| p.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let partition_key_kind = pk
        .and_then(|p| p.get("kind"))
        .and_then(|n| n.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let default_ttl = resource
        .and_then(|r| r.get("defaultTtl"))
        .and_then(|n| n.as_i64());
    let indexing_mode = resource
        .and_then(|r| r.get("indexingPolicy"))
        .and_then(|p| p.get("indexingMode"))
        .and_then(|n| n.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    Some(CosmosContainer {
        id,
        name,
        partition_key_paths,
        partition_key_kind,
        default_ttl,
        indexing_mode,
    })
}

pub(crate) fn parse_items_response(v: &serde_json::Value) -> Vec<serde_json::Value> {
    v.get("Documents")
        .and_then(|a| a.as_array())
        .cloned()
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Error classification
// ---------------------------------------------------------------------------

fn classify_data_plane_error(account_name: &str, host: &str, e: anyhow::Error) -> anyhow::Error {
    let msg = format!("{e}");
    let lower = msg.to_lowercase();
    if lower.contains(" 401 ") || lower.contains("returned 401") || lower.contains("unauthorized") {
        return anyhow!(
            "401 from Cosmos data plane: identity rejected by '{account_name}'. \
             The signed-in account needs the `Cosmos DB Built-in Data Reader` role \
             assigned at the account scope (via `az cosmosdb sql role assignment create`) \
             — control-plane `Reader` is not enough. underlying: {msg}"
        );
    }
    if lower.contains(" 403 ") || lower.contains("returned 403") || lower.contains("forbidden") {
        return anyhow!(
            "403 from Cosmos data plane: identity lacks `Cosmos DB Built-in Data Reader` \
             (or stronger) on account '{account_name}'. underlying: {msg}"
        );
    }
    if lower.contains(" 404 ") || lower.contains("returned 404") {
        return anyhow!(
            "404 from Cosmos data plane: database or container not found on \
             account '{account_name}' (or account has a firewall blocking your IP). \
             underlying: {msg}"
        );
    }
    if lower.contains("dns")
        || lower.contains("failed to lookup")
        || lower.contains("no such host")
        || lower.contains("name or service not known")
    {
        return anyhow!(
            "DNS lookup failed for '{host}' — does account '{account_name}' exist \
             (or is it firewalled to a private endpoint)? underlying: {msg}"
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
    fn parses_sql_account_row() {
        let row = json!({
            "id": "/subscriptions/s/resourceGroups/rg/providers/Microsoft.DocumentDB/databaseAccounts/acc",
            "name": "acc",
            "type": "microsoft.documentdb/databaseaccounts",
            "kind": "GlobalDocumentDB",
            "location": "westeurope",
            "resourceGroup": "rg",
            "subscriptionId": "s",
            "properties": {
                "documentEndpoint": "https://acc.documents.azure.com:443/",
                "publicNetworkAccess": "Enabled",
                "capabilities": [{ "name": "EnableServerless" }],
                "systemData": { "createdAt": "2026-01-02T03:04:05.000Z" }
            }
        });
        let acc = parse_account(&row).expect("expected account");
        assert_eq!(acc.name, "acc");
        assert_eq!(acc.kind.as_deref(), Some("GlobalDocumentDB"));
        assert_eq!(
            acc.document_endpoint.as_deref(),
            Some("https://acc.documents.azure.com:443/")
        );
        assert!(acc.is_serverless);
        assert!(acc.created_at.is_some());
    }

    #[test]
    fn rejects_mongo_kind_accounts() {
        let row = json!({
            "id": "/subs/s/rg/x/providers/Microsoft.DocumentDB/databaseAccounts/m",
            "name": "m",
            "type": "microsoft.documentdb/databaseaccounts",
            "kind": "MongoDB",
            "location": "westeurope",
            "resourceGroup": "x",
            "subscriptionId": "s",
            "properties": {}
        });
        assert!(parse_account(&row).is_none());
    }

    #[test]
    fn rejects_cassandra_gremlin_table_capabilities() {
        for cap in ["EnableCassandra", "EnableGremlin", "EnableTable"] {
            let row = json!({
                "id": "/subs/s/rg/x/providers/Microsoft.DocumentDB/databaseAccounts/x",
                "name": "x",
                "type": "microsoft.documentdb/databaseaccounts",
                "kind": "GlobalDocumentDB",
                "location": "westeurope",
                "resourceGroup": "x",
                "subscriptionId": "s",
                "properties": { "capabilities": [{ "name": cap }] }
            });
            assert!(
                parse_account(&row).is_none(),
                "capability {cap} should disqualify the account"
            );
        }
    }

    #[test]
    fn accepts_sql_account_with_no_capabilities() {
        let row = json!({
            "id": "/subs/s/rg/x/providers/Microsoft.DocumentDB/databaseAccounts/x",
            "name": "x",
            "type": "microsoft.documentdb/databaseaccounts",
            "kind": "GlobalDocumentDB",
            "location": "westeurope",
            "resourceGroup": "x",
            "subscriptionId": "s",
            "properties": { "capabilities": [] }
        });
        let acc = parse_account(&row).expect("plain SQL account should pass");
        assert!(!acc.is_serverless);
        assert!(acc.capabilities.is_empty());
    }

    #[test]
    fn skips_non_cosmos_rows() {
        let row = json!({
            "id": "/subs/x/rg/y/providers/Microsoft.Web/sites/z",
            "name": "z",
            "type": "microsoft.web/sites",
            "location": "westeurope",
            "resourceGroup": "y",
            "subscriptionId": "x"
        });
        assert!(parse_account(&row).is_none());
    }

    #[test]
    fn account_document_endpoint_falls_back_to_name() {
        let mut acc = CosmosAccount {
            id: "id".into(),
            name: "legacy".into(),
            resource_group: "rg".into(),
            subscription_id: "s".into(),
            location: "westeurope".into(),
            kind: Some("GlobalDocumentDB".into()),
            document_endpoint: None,
            capabilities: Vec::new(),
            is_serverless: false,
            public_network_access: None,
            created_at: None,
        };
        assert_eq!(
            acc.document_endpoint_or_default(),
            "https://legacy.documents.azure.com:443/"
        );
        acc.document_endpoint = Some("https://legacy.documents.azure.com:443/".into());
        assert_eq!(
            acc.document_endpoint_or_default(),
            "https://legacy.documents.azure.com:443/"
        );
    }

    #[test]
    fn parses_databases_value_envelope() {
        let body = json!({
            "value": [
                { "id": "/subs/x/sa/acc/sqlDatabases/db1", "name": "db1" },
                { "id": "/subs/x/sa/acc/sqlDatabases/db2", "name": "db2" }
            ]
        });
        let dbs = parse_databases_json(&body);
        assert_eq!(dbs.len(), 2);
        assert_eq!(dbs[0].name, "db1");
        assert_eq!(dbs[1].name, "db2");
    }

    #[test]
    fn parses_containers_extracts_partition_key_and_ttl() {
        let body = json!({
            "value": [{
                "id": "/x/sqlDatabases/db/containers/c",
                "name": "c",
                "properties": {
                    "resource": {
                        "id": "c",
                        "partitionKey": { "paths": ["/userId"], "kind": "Hash" },
                        "defaultTtl": 3600,
                        "indexingPolicy": { "indexingMode": "consistent" }
                    }
                }
            }]
        });
        let conts = parse_containers_json(&body);
        assert_eq!(conts.len(), 1);
        let c = &conts[0];
        assert_eq!(c.name, "c");
        assert_eq!(c.partition_key_paths, vec!["/userId".to_string()]);
        assert_eq!(c.partition_key_kind.as_deref(), Some("Hash"));
        assert_eq!(c.default_ttl, Some(3600));
        assert_eq!(c.indexing_mode.as_deref(), Some("consistent"));
    }

    #[test]
    fn parses_items_response_extracts_documents() {
        let body = json!({
            "_rid": "abc",
            "Documents": [
                { "id": "1", "name": "alpha" },
                { "id": "2", "name": "beta" }
            ],
            "_count": 2
        });
        let items = parse_items_response(&body);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].get("name").and_then(|v| v.as_str()), Some("alpha"));
    }

    #[test]
    fn items_url_strips_endpoint_trailing_slash() {
        assert_eq!(
            items_url("https://acc.documents.azure.com:443/", "db", "c"),
            "https://acc.documents.azure.com:443/dbs/db/colls/c/docs"
        );
        assert_eq!(
            items_url("https://acc.documents.azure.com:443", "db", "c"),
            "https://acc.documents.azure.com:443/dbs/db/colls/c/docs"
        );
    }

    #[test]
    fn endpoint_host_extracts_host_without_port() {
        assert_eq!(
            endpoint_host("https://acc.documents.azure.com:443/"),
            Some("acc.documents.azure.com".to_string())
        );
        assert_eq!(
            endpoint_host("https://acc.documents.azure.com/"),
            Some("acc.documents.azure.com".to_string())
        );
        assert_eq!(endpoint_host("not-a-url"), None);
    }

    #[test]
    fn classifies_401_with_role_hint() {
        let err = classify_data_plane_error(
            "myacc",
            "myacc.documents.azure.com",
            anyhow!("cosmos data-plane returned 401: unauthorized"),
        );
        let msg = format!("{err}");
        assert!(
            msg.contains("Cosmos DB Built-in Data Reader"),
            "expected role hint in: {msg}"
        );
        assert!(msg.contains("myacc"), "expected account name in: {msg}");
    }

    #[test]
    fn classifies_403_with_role_hint() {
        let err = classify_data_plane_error(
            "myacc",
            "myacc.documents.azure.com",
            anyhow!("cosmos data-plane returned 403: forbidden"),
        );
        let msg = format!("{err}");
        assert!(msg.contains("Cosmos DB Built-in Data Reader"), "got: {msg}");
    }

    #[test]
    fn classifies_dns_failure_with_host() {
        let err = classify_data_plane_error(
            "ghost",
            "ghost.documents.azure.com",
            anyhow!("cosmos data-plane network error: dns error: failed to lookup address"),
        );
        let msg = format!("{err}");
        assert!(msg.contains("ghost.documents.azure.com"), "got: {msg}");
    }

    #[test]
    fn urlencode_token_replaces_url_unsafe_chars() {
        assert_eq!(urlencode_token("abc.def-ghi_jkl"), "abc.def-ghi_jkl");
        assert_eq!(urlencode_token("a=b&c+d/e"), "a%3Db%26c%2Bd%2Fe");
    }

    #[test]
    fn rfc1123_now_has_gmt_suffix_and_correct_shape() {
        let s = rfc1123_now();
        assert!(s.ends_with(" GMT"), "expected RFC1123 GMT suffix: {s}");
        // Shape: "Sun, 06 Sep 2009 17:39:00 GMT" → 29 chars
        assert_eq!(s.len(), 29, "expected 29-char RFC1123 string, got: {s}");
    }

    #[test]
    fn truncate_error_body_bounds_oversized_strings() {
        let big = "x".repeat(5_000);
        let out = truncate_error_body(&big);
        assert!(out.ends_with('…'));
        assert!(out.chars().count() <= 1_025);
    }
}
