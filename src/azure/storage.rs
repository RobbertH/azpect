//! Read-only Azure Storage **blob** inspection (no queues/tables/files).
//!
//! ## Contract (do not change without coordinating with the UI lane)
//!
//! Four public functions form the surface the UI consumes:
//!
//! - [`list_accounts`] — Resource Graph KQL discovery of storage accounts
//!   across the supplied subscriptions.
//! - [`list_containers`] — ARM control-plane GET that lists blob containers
//!   for one account (no data-plane permissions required).
//! - [`list_blobs`] — Data-plane XML enumeration of blobs in a container
//!   (requires `Storage Blob Data Reader` on the account).
//! - [`preview_blob`] — Data-plane HEAD + optional ranged GET that yields
//!   metadata plus a UTF-8 text preview (or a textual marker for binary
//!   content) for one blob.
//!
//! ## Scope decisions worth flagging
//!
//! - **AAD bearer only**: no SAS-token codepath. The bearer audience for the
//!   blob endpoints is `https://storage.azure.com/.default` (see
//!   [`crate::azure::auth::SCOPE_STORAGE`]) — this is distinct from ARM, and a
//!   common 403 failure mode is the identity having only `Reader` on the
//!   account (sufficient for control-plane container listing) but missing
//!   `Storage Blob Data Reader` (required for blob enumeration). The error
//!   message bubbled up from [`list_blobs`] / [`preview_blob`] calls this out
//!   explicitly so the UI can surface it.
//! - **Pagination**: matches `resources.rs` precedent — we cap at 1000 results
//!   per call and `tracing::warn!` if the server signals more.
//! - **Read-only**: no DELETE/PUT codepaths, even stubs.

#![allow(dead_code, unused_variables)]

use anyhow::{anyhow, Context};
use chrono::{DateTime, Utc};
use reqwest::header::HeaderMap;
use serde::Deserialize;

use crate::azure::auth::AzureAuth;
use crate::azure::client::{ArmClient, StorageClient};

/// Storage account discovered via Resource Graph.
#[derive(Clone, Debug)]
pub struct StorageAccount {
    /// Full ARM resource id.
    pub id: String,
    pub name: String,
    pub resource_group: String,
    pub subscription_id: String,
    pub location: String,
    /// e.g. `StorageV2`, `BlockBlobStorage`.
    pub kind: Option<String>,
    /// SKU name, e.g. `Standard_LRS`, `Standard_GRS`, `Premium_LRS`. Optional
    /// because Resource Graph may omit it on legacy / classic accounts.
    pub sku: Option<String>,
    /// `Hot` / `Cool` (and rarer tiers). Only meaningful on v2 / blob-only
    /// accounts; `None` on premium / classic.
    pub access_tier: Option<String>,
    /// ADLS Gen2 marker (`isHnsEnabled`). Hierarchical namespace.
    pub is_hns_enabled: Option<bool>,
    /// `supportsHttpsTrafficOnly`. Almost always true for modern accounts.
    pub https_only: Option<bool>,
    /// `allowBlobPublicAccess`. `Some(true)` → anonymous public containers
    /// are permitted (account-wide gate); `Some(false)` → blocked; `None` →
    /// the field was absent from the Resource Graph row.
    pub allow_blob_public_access: Option<bool>,
    /// `properties.creationTime` parsed to UTC. `None` if Resource Graph
    /// omitted the field (rare — present on every account I've seen).
    pub created_at: Option<DateTime<Utc>>,
}

/// One container under a storage account.
#[derive(Clone, Debug)]
pub struct BlobContainer {
    pub name: String,
    /// `"None"`, `"Blob"`, `"Container"`, or `None` if the field was absent.
    pub public_access: Option<String>,
    pub last_modified: Option<DateTime<Utc>>,
    pub has_immutability_policy: Option<bool>,
}

/// One blob within a container.
#[derive(Clone, Debug)]
pub struct Blob {
    pub name: String,
    pub size: u64,
    pub content_type: Option<String>,
    pub last_modified: Option<DateTime<Utc>>,
    /// `"BlockBlob"`, `"PageBlob"`, or `"AppendBlob"`.
    pub blob_type: String,
}

/// Metadata returned by HEAD on a single blob.
#[derive(Clone, Debug)]
pub struct BlobMetadata {
    pub content_type: Option<String>,
    pub content_length: u64,
    pub etag: Option<String>,
    pub last_modified: Option<DateTime<Utc>>,
    pub content_md5: Option<String>,
}

/// Per-account aggregated stats sourced from Azure Monitor metrics. Mirrors
/// the "Storage browser → account overview" panel in the Azure portal: a
/// snapshot of container / blob / file / queue / table counts and totals.
///
/// Each field is `Option` because Azure Monitor returns metrics per *service*
/// (blob, file, queue, table). An account that disables one service (or whose
/// signed-in identity lacks the role on that scope) will surface a 4xx for
/// that sub-fetch; we leave the corresponding fields as `None` rather than
/// failing the whole call. The UI renders `—` for missing values.
///
/// All counts/byte fields land via the `Average` aggregation of daily-grain
/// metrics, taking the latest non-stale datapoint. The portal does the same
/// computation, which is why the numbers track 1:1 with the "overview" tile.
#[derive(Clone, Debug, Default)]
pub struct StorageAccountStats {
    pub used_capacity_bytes: Option<u64>,
    pub container_count: Option<u64>,
    pub blob_count: Option<u64>,
    pub blob_capacity_bytes: Option<u64>,
    pub file_share_count: Option<u64>,
    pub file_count: Option<u64>,
    pub file_capacity_bytes: Option<u64>,
    pub queue_count: Option<u64>,
    pub queue_message_count: Option<u64>,
    pub queue_capacity_bytes: Option<u64>,
    pub table_count: Option<u64>,
    pub table_entity_count: Option<u64>,
    pub table_capacity_bytes: Option<u64>,
    /// Latest data-point timestamp across every populated metric — communicates
    /// the freshness lag explicitly so the UI can surface "as of N hours ago".
    /// `None` if no metric returned a point.
    pub as_of: Option<DateTime<Utc>>,
}

/// What the preview path actually decided to return for the body.
#[derive(Clone, Debug)]
pub enum BlobPreviewBody {
    /// Decoded text, already truncated to the caller's `max_bytes`.
    Text(String),
    /// We chose not to fetch the body; `reason` is a short user-facing string
    /// like `"binary content (application/octet-stream, 1.2 MB)"`.
    Binary { reason: String },
}

/// Metadata + body produced by [`preview_blob`].
#[derive(Clone, Debug)]
pub struct BlobPreview {
    pub metadata: BlobMetadata,
    pub body: BlobPreviewBody,
}

/// Resource Graph KQL for storage accounts. Same body/`subscriptions` envelope
/// as [`super::resources::KQL`]. The extra `sku` / `properties.*` projections
/// feed the metadata columns rendered by `views::storage_accounts` — they
/// piggyback on the existing call so there's no second ARM round-trip.
// Projects the raw `sku` and `properties` dynamic blobs and lets the Rust
// parser dig into them. Earlier versions used KQL `tobool(properties.foo)` to
// produce flat columns, but Resource Graph silently returned null for some
// tenants/account types, leaving every metadata column as `?` in the UI.
// Pulling the whole blob is more defensive and adds negligible bytes per row.
const ACCOUNTS_KQL: &str = r#"
Resources
| where type == 'microsoft.storage/storageaccounts'
| project id, name, type, kind, location, resourceGroup, subscriptionId, sku, properties
| order by name asc
"#;

/// API version used for the ARM control-plane container listing call.
const STORAGE_CONTROL_API_VERSION: &str = "2023-05-01";

/// Soft cap for `maxresults` on the list-blobs XML call — matches the warn
/// threshold in `resources.rs`.
const LIST_BLOBS_MAX_RESULTS: u32 = 1000;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Enumerate storage accounts across `subscription_ids`. Empty slice → all
/// subscriptions visible to the credential (Resource Graph default scope).
pub async fn list_accounts(
    auth: &AzureAuth,
    subscription_ids: &[String],
) -> anyhow::Result<Vec<StorageAccount>> {
    let client = ArmClient::new(auth.clone())?;

    let body = if subscription_ids.is_empty() {
        serde_json::json!({ "query": ACCOUNTS_KQL })
    } else {
        serde_json::json!({
            "subscriptions": subscription_ids,
            "query": ACCOUNTS_KQL,
        })
    };

    let resp = client
        .post(
            "/providers/Microsoft.ResourceGraph/resources?api-version=2022-10-01",
            &body,
        )
        .await
        .context("resource graph: list storage accounts")?;

    let rows = resp
        .get("data")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("resource graph response missing 'data' array"))?;

    if rows.len() >= 1000 {
        tracing::warn!(
            "resource graph returned {} storage accounts; pagination not implemented in v1",
            rows.len()
        );
    }

    Ok(rows.iter().filter_map(parse_account).collect())
}

/// List blob containers under a storage account via the ARM control plane.
/// Does NOT require any data-plane (`Storage Blob Data Reader`) permissions —
/// `Reader` on the account is sufficient. Container *contents* require
/// data-plane access; see [`list_blobs`].
pub async fn list_containers(
    auth: &AzureAuth,
    account: &StorageAccount,
) -> anyhow::Result<Vec<BlobContainer>> {
    let client = ArmClient::new(auth.clone())?;
    let path = format!(
        "/subscriptions/{}/resourceGroups/{}/providers/Microsoft.Storage/storageAccounts/{}/blobServices/default/containers",
        account.subscription_id, account.resource_group, account.name
    );
    let resp = client
        .get(&path, &[("api-version", STORAGE_CONTROL_API_VERSION)])
        .await
        .with_context(|| format!("list containers for storage account '{}'", account.name))?;

    Ok(parse_containers_json(&resp))
}

/// Fetch the per-account stats panel ("Storage browser → account overview" in
/// the portal) for `account`. Issues **five** Azure Monitor metrics calls in
/// parallel — one each at account scope and at the four service scopes (blob,
/// file, queue, table) — and stitches the results into [`StorageAccountStats`].
///
/// Resilience: a sub-service whose call 4xxs (404 = service disabled, 403 =
/// missing data-plane role on that scope, etc.) leaves its fields as `None`
/// instead of failing the whole call. The portal's overview tile renders the
/// same partial state — there's no value in being stricter than the official
/// UI.
///
/// Cache aggressively at the caller: these metrics update at most a few times
/// per day server-side, so re-fetching on every view re-entry is pure waste.
pub async fn fetch_account_overview_stats(
    auth: &AzureAuth,
    account: &StorageAccount,
) -> anyhow::Result<StorageAccountStats> {
    let account_scope = OverviewScope {
        path: account.id.clone(),
        metrics: ACCOUNT_METRICS,
    };
    let blob_scope = OverviewScope {
        path: format!("{}/blobServices/default", account.id),
        metrics: BLOB_METRICS,
    };
    let file_scope = OverviewScope {
        path: format!("{}/fileServices/default", account.id),
        metrics: FILE_METRICS,
    };
    let queue_scope = OverviewScope {
        path: format!("{}/queueServices/default", account.id),
        metrics: QUEUE_METRICS,
    };
    let table_scope = OverviewScope {
        path: format!("{}/tableServices/default", account.id),
        metrics: TABLE_METRICS,
    };

    let client = ArmClient::new(auth.clone())?;
    // Parallel by-scope: five concurrent ARM calls, one per service. We use
    // `tokio::join!` (not `try_join!`) so a 4xx on one scope doesn't poison
    // the rest — the per-scope helper folds errors into `None` values.
    let (acct, blob, file, queue, table) = tokio::join!(
        fetch_overview_metrics(&client, &account_scope),
        fetch_overview_metrics(&client, &blob_scope),
        fetch_overview_metrics(&client, &file_scope),
        fetch_overview_metrics(&client, &queue_scope),
        fetch_overview_metrics(&client, &table_scope),
    );

    let mut stats = StorageAccountStats::default();
    let mut latest: Option<DateTime<Utc>> = None;

    let mut apply = |name: &str, point: (Option<u64>, DateTime<Utc>)| {
        let (value, ts) = point;
        if value.is_none() {
            return;
        }
        match name {
            "UsedCapacity" => stats.used_capacity_bytes = value,
            "ContainerCount" => stats.container_count = value,
            "BlobCount" => stats.blob_count = value,
            "BlobCapacity" => stats.blob_capacity_bytes = value,
            "FileShareCount" => stats.file_share_count = value,
            "FileCount" => stats.file_count = value,
            "FileCapacity" => stats.file_capacity_bytes = value,
            "QueueCount" => stats.queue_count = value,
            "QueueMessageCount" => stats.queue_message_count = value,
            "QueueCapacity" => stats.queue_capacity_bytes = value,
            "TableCount" => stats.table_count = value,
            "TableEntityCount" => stats.table_entity_count = value,
            "TableCapacity" => stats.table_capacity_bytes = value,
            _ => {}
        }
        if let Some(prev) = latest {
            if ts > prev {
                latest = Some(ts);
            }
        } else {
            latest = Some(ts);
        }
    };

    for (scope, result) in [
        (&account_scope, acct),
        (&blob_scope, blob),
        (&file_scope, file),
        (&queue_scope, queue),
        (&table_scope, table),
    ] {
        match result {
            Ok(points) => {
                for (name, point) in points {
                    apply(name, point);
                }
            }
            Err(e) => {
                // A 4xx on any one service tile is expected (e.g. classic
                // accounts without file shares 404 on /fileServices/default).
                // Log and move on — the missing fields render as `—`.
                tracing::debug!(
                    "storage overview: metrics fetch for {} failed: {e:#}",
                    scope.path
                );
            }
        }
    }

    stats.as_of = latest;
    Ok(stats)
}

/// One Azure Monitor scope to query in [`fetch_account_overview_stats`]:
/// either the account itself or one of its sub-service scopes. `metrics`
/// holds the `(physical_name, aggregation)` pairs to request.
struct OverviewScope {
    path: String,
    metrics: &'static [(&'static str, &'static str)],
}

const ACCOUNT_METRICS: &[(&str, &str)] = &[("UsedCapacity", "Average")];
const BLOB_METRICS: &[(&str, &str)] = &[
    ("BlobCount", "Average"),
    ("BlobCapacity", "Average"),
    ("ContainerCount", "Average"),
];
const FILE_METRICS: &[(&str, &str)] = &[
    ("FileCount", "Average"),
    ("FileCapacity", "Average"),
    ("FileShareCount", "Average"),
];
const QUEUE_METRICS: &[(&str, &str)] = &[
    ("QueueCount", "Average"),
    ("QueueMessageCount", "Average"),
    ("QueueCapacity", "Average"),
];
const TABLE_METRICS: &[(&str, &str)] = &[
    ("TableCount", "Average"),
    ("TableEntityCount", "Average"),
    ("TableCapacity", "Average"),
];

/// One parsed Monitor datapoint: the requested metric's canonical name plus
/// `(value, timestamp)`. `value` is `None` only when the response was empty
/// for that series — negative / non-finite numbers are filtered upstream.
type OverviewDatapoint = (&'static str, (Option<u64>, DateTime<Utc>));

/// Single Azure Monitor metrics call for one scope. Returns the latest
/// `(value, timestamp)` per requested metric, with `value = None` for series
/// that came back empty. Whole-call errors propagate so the caller can decide
/// whether to fold them into `None` (partial-success) or surface them.
async fn fetch_overview_metrics(
    client: &ArmClient,
    scope: &OverviewScope,
) -> anyhow::Result<Vec<OverviewDatapoint>> {
    let names = scope
        .metrics
        .iter()
        .map(|(n, _)| *n)
        .collect::<Vec<_>>()
        .join(",");
    let path = format!(
        "{}/providers/Microsoft.Insights/metrics",
        scope.path.trim_end_matches('/')
    );
    let value = client
        .get(
            &path,
            &[
                ("api-version", "2018-01-01"),
                ("metricnames", &names),
                ("aggregation", "Average"),
                // P1D timespan + daily grain matches what the portal shows for
                // capacity / object-count metrics, which only update a few
                // times per day server-side.
                ("timespan", "P1D"),
                ("interval", "PT1H"),
            ],
        )
        .await?;
    Ok(parse_overview_metrics_response(&value, scope.metrics))
}

/// Pluck the latest datapoint per requested metric out of an Azure Monitor
/// response. Returns one entry per *known* requested metric (skips ones that
/// the response omitted entirely or where every datapoint was null). Public
/// for tests.
pub(crate) fn parse_overview_metrics_response(
    value: &serde_json::Value,
    requested: &'static [(&'static str, &'static str)],
) -> Vec<OverviewDatapoint> {
    let mut out = Vec::new();
    let metrics = match value.get("value").and_then(|v| v.as_array()) {
        Some(a) => a,
        None => return out,
    };

    for m in metrics {
        let name = m
            .get("name")
            .and_then(|n| n.get("value"))
            .and_then(|n| n.as_str())
            .unwrap_or("");
        // Resolve the canonical &'static str so the caller can match on it.
        let Some((known, _agg)) = requested.iter().find(|(n, _)| *n == name) else {
            continue;
        };
        let timeseries = m
            .get("timeseries")
            .and_then(|t| t.as_array())
            .and_then(|a| a.first());
        let Some(ts) = timeseries else { continue };
        let data = match ts.get("data").and_then(|d| d.as_array()) {
            Some(d) => d,
            None => continue,
        };
        // Walk newest → oldest, take the first datapoint with a real value.
        // Some metrics return a window with a stale tail of nulls (capacity
        // updates lag the bucket boundary); we don't want to display 0 in
        // those cases.
        // Walk newest → oldest, take the first datapoint with a real *and*
        // physically meaningful value. Capacity / count metrics can never be
        // negative — if Monitor returns one (or NaN / infinity), treat the
        // point as missing so the UI shows `—` rather than 0 or a wrapped
        // u64 cast to "16 EB".
        let latest = data.iter().rev().find_map(|pt| {
            let ts_str = pt.get("timeStamp").and_then(|t| t.as_str())?;
            let ts = DateTime::parse_from_rfc3339(ts_str)
                .ok()?
                .with_timezone(&Utc);
            let v = pt.get("average").and_then(|x| x.as_f64())?;
            if !v.is_finite() || v < 0.0 {
                return None;
            }
            Some((v, ts))
        });
        if let Some((v, ts)) = latest {
            out.push((*known, (Some(v.round() as u64), ts)));
        }
    }

    out
}

/// Enumerate blobs in `container` for `account_name` via the data plane.
///
/// Pagination: we cap `maxresults` at [`LIST_BLOBS_MAX_RESULTS`] and warn (but
/// do **not** error) if the server signals a `NextMarker` — matching the
/// `resources.rs` truncation behaviour.
pub async fn list_blobs(
    auth: &AzureAuth,
    account_name: &str,
    container: &str,
    prefix: Option<&str>,
) -> anyhow::Result<Vec<Blob>> {
    let client = StorageClient::new(auth.clone())?;
    let mut url = format!(
        "https://{account_name}.blob.core.windows.net/{container}?restype=container&comp=list&maxresults={}",
        LIST_BLOBS_MAX_RESULTS
    );
    if let Some(p) = prefix.filter(|s| !s.is_empty()) {
        url.push_str("&prefix=");
        url.push_str(&urlencode(p));
    }

    let body = client
        .get_xml(&url)
        .await
        .map_err(|e| classify_data_plane_error(account_name, e))?;

    if body.trim().is_empty() {
        return Ok(Vec::new());
    }

    parse_list_blobs_xml(&body)
}

/// Fetch metadata + a bounded preview body for one blob.
///
/// Decision rule for the body:
///   - Textual MIME (`text/*`, `application/json`, `application/xml`,
///     `application/javascript`) → fetch up to `max_bytes`, decode UTF-8 lossy.
///   - Unknown MIME (`None`) but `content_length ≤ 64 KB` → same.
///   - Otherwise → return a textual `Binary { reason }` marker without
///     touching the body.
pub async fn preview_blob(
    auth: &AzureAuth,
    account_name: &str,
    container: &str,
    blob: &str,
    max_bytes: u64,
) -> anyhow::Result<BlobPreview> {
    let client = StorageClient::new(auth.clone())?;
    let url = format!(
        "https://{account_name}.blob.core.windows.net/{container}/{}",
        encode_blob_path(blob)
    );

    let headers = client
        .head(&url)
        .await
        .map_err(|e| classify_data_plane_error(account_name, e))?;

    let metadata = parse_blob_metadata(&headers);

    if should_preview_as_text(metadata.content_type.as_deref(), metadata.content_length) {
        let end = max_bytes.saturating_sub(1);
        let range = format!("bytes=0-{end}");
        let bytes = client
            .get_bytes_range(&url, &range)
            .await
            .map_err(|e| classify_data_plane_error(account_name, e))?;
        let mut text = String::from_utf8_lossy(&bytes).into_owned();
        if text.len() as u64 > max_bytes {
            // String::from_utf8_lossy can occasionally produce a longer string
            // (replacement chars are 3 bytes each); honour the caller's cap.
            let mut end = max_bytes as usize;
            while !text.is_char_boundary(end) && end > 0 {
                end -= 1;
            }
            text.truncate(end);
        }
        Ok(BlobPreview {
            metadata,
            body: BlobPreviewBody::Text(text),
        })
    } else {
        let reason = format!(
            "binary content ({}, {})",
            metadata.content_type.as_deref().unwrap_or("unknown type"),
            human_bytes(metadata.content_length),
        );
        Ok(BlobPreview {
            metadata,
            body: BlobPreviewBody::Binary { reason },
        })
    }
}

// ---------------------------------------------------------------------------
// Parsers — JSON
// ---------------------------------------------------------------------------

/// Pull a boolean out of a JSON value that might be `true` / `false`, the
/// strings `"true"` / `"false"` (case-insensitive), or `null`. Resource Graph
/// has been observed to surface storage-account boolean properties in either
/// of the first two shapes depending on the resource type / tenant.
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

fn parse_account(v: &serde_json::Value) -> Option<StorageAccount> {
    let ty = v.get("type")?.as_str()?.to_lowercase();
    if ty != "microsoft.storage/storageaccounts" {
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
    let kind = v
        .get("kind")
        .and_then(|n| n.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let props = v.get("properties");
    let sku = v
        .get("sku")
        .and_then(|s| s.get("name"))
        .and_then(|n| n.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let access_tier = props
        .and_then(|p| p.get("accessTier"))
        .and_then(|n| n.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let is_hns_enabled = props
        .and_then(|p| p.get("isHnsEnabled"))
        .and_then(extract_bool);
    let https_only = props
        .and_then(|p| p.get("supportsHttpsTrafficOnly"))
        .and_then(extract_bool);
    let allow_blob_public_access = props
        .and_then(|p| p.get("allowBlobPublicAccess"))
        .and_then(extract_bool);
    let created_at = props
        .and_then(|p| p.get("creationTime"))
        .and_then(|n| n.as_str())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc));
    Some(StorageAccount {
        id,
        name,
        resource_group,
        subscription_id,
        location,
        kind,
        sku,
        access_tier,
        is_hns_enabled,
        https_only,
        allow_blob_public_access,
        created_at,
    })
}

/// Public for tests — parses the ARM `value: []` response from the
/// list-containers endpoint.
pub(crate) fn parse_containers_json(v: &serde_json::Value) -> Vec<BlobContainer> {
    let Some(items) = v.get("value").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let name = item
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            if name.is_empty() {
                return None;
            }
            let props = item.get("properties");
            let public_access = props
                .and_then(|p| p.get("publicAccess"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());
            let last_modified = props
                .and_then(|p| p.get("lastModifiedTime"))
                .and_then(|v| v.as_str())
                .and_then(parse_rfc3339);
            let has_immutability_policy = props
                .and_then(|p| p.get("hasImmutabilityPolicy"))
                .and_then(|v| v.as_bool());
            Some(BlobContainer {
                name,
                public_access,
                last_modified,
                has_immutability_policy,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Parsers — XML (list_blobs)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct XmlEnumerationResults {
    #[serde(rename = "Blobs", default)]
    blobs: XmlBlobs,
    #[serde(rename = "NextMarker", default)]
    next_marker: String,
}

#[derive(Debug, Default, Deserialize)]
struct XmlBlobs {
    #[serde(rename = "Blob", default)]
    blob: Vec<XmlBlob>,
}

#[derive(Debug, Deserialize)]
struct XmlBlob {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Properties", default)]
    properties: XmlBlobProperties,
}

#[derive(Debug, Default, Deserialize)]
struct XmlBlobProperties {
    #[serde(rename = "Content-Length", default)]
    content_length: Option<u64>,
    #[serde(rename = "Content-Type", default)]
    content_type: Option<String>,
    #[serde(rename = "Last-Modified", default)]
    last_modified: Option<String>,
    #[serde(rename = "BlobType", default)]
    blob_type: Option<String>,
}

/// Public for tests.
pub(crate) fn parse_list_blobs_xml(body: &str) -> anyhow::Result<Vec<Blob>> {
    let parsed: XmlEnumerationResults =
        quick_xml::de::from_str(body).map_err(|e| anyhow!("parse list-blobs xml: {e}"))?;

    if !parsed.next_marker.is_empty() {
        tracing::warn!(
            "list-blobs returned NextMarker (more than {} blobs); pagination not implemented in v1",
            LIST_BLOBS_MAX_RESULTS
        );
    }

    Ok(parsed
        .blobs
        .blob
        .into_iter()
        .map(|b| {
            let last_modified = b
                .properties
                .last_modified
                .as_deref()
                .and_then(parse_http_date);
            Blob {
                name: b.name,
                size: b.properties.content_length.unwrap_or(0),
                content_type: b.properties.content_type.filter(|s| !s.is_empty()),
                last_modified,
                blob_type: b.properties.blob_type.unwrap_or_default(),
            }
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Parsers — HEAD headers
// ---------------------------------------------------------------------------

fn parse_blob_metadata(headers: &HeaderMap) -> BlobMetadata {
    let content_length = headers
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    let content_type = headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let etag = headers
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let last_modified = headers
        .get(reqwest::header::LAST_MODIFIED)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_http_date);
    let content_md5 = headers
        .get("content-md5")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    BlobMetadata {
        content_type,
        content_length,
        etag,
        last_modified,
        content_md5,
    }
}

// ---------------------------------------------------------------------------
// Error classification
// ---------------------------------------------------------------------------

/// Convert raw transport errors into a clearer, role-aware message for the UI.
fn classify_data_plane_error(account_name: &str, e: anyhow::Error) -> anyhow::Error {
    let msg = format!("{e}");
    let lower = msg.to_lowercase();
    if msg.contains("azure api error 403") || lower.contains(" 403 ") || lower.contains("forbidden")
    {
        return anyhow!(
            "403 from storage data plane: identity likely lacks 'Storage Blob Data Reader' role \
             on account '{account_name}'. Control-plane access (Reader) is not sufficient for blob \
             enumeration."
        );
    }
    if lower.contains("dns")
        || lower.contains("failed to lookup")
        || lower.contains("no such host")
        || lower.contains("name or service not known")
    {
        return anyhow!(
            "DNS lookup failed for '{account_name}.blob.core.windows.net' — does the storage \
             account '{account_name}' exist (or is it firewalled to a private endpoint)? \
             underlying error: {msg}"
        );
    }
    e
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Minimal percent-encoder for query-string values. Avoids pulling in
/// `percent-encoding` for the one place we need to escape a user prefix.
fn urlencode(s: &str) -> String {
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

/// Encode a blob path for inclusion in a URL while preserving `/` separators
/// (Azure treats `/` in blob names as a visual hierarchy, not a path delimiter,
/// but the bytes are passed through unencoded).
fn encode_blob_path(s: &str) -> String {
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

fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Azure storage XML returns RFC1123 timestamps (e.g.
/// `Wed, 18 May 2026 12:00:00 GMT`); HEAD's `Last-Modified` is the same.
fn parse_http_date(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc2822(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
        .or_else(|| parse_rfc3339(s))
}

fn should_preview_as_text(content_type: Option<&str>, content_length: u64) -> bool {
    const TEXT_PREFIXES: &[&str] = &[
        "text/",
        "application/json",
        "application/xml",
        "application/javascript",
    ];
    match content_type {
        Some(ct) => {
            let lower = ct.to_ascii_lowercase();
            TEXT_PREFIXES.iter().any(|p| lower.starts_with(p))
        }
        // Unknown MIME — only attempt a preview for "small" blobs to avoid
        // pulling megabytes of opaque bytes off the wire.
        None => content_length <= 64 * 1024,
    }
}

fn human_bytes(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if n >= GB {
        format!("{:.1} GB", n as f64 / GB as f64)
    } else if n >= MB {
        format!("{:.1} MB", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{:.1} KB", n as f64 / KB as f64)
    } else {
        format!("{n} B")
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_storage_account_row() {
        let row = json!({
            "id": "/subscriptions/sub1/resourceGroups/rg1/providers/Microsoft.Storage/storageAccounts/mystg",
            "name": "mystg",
            "type": "microsoft.storage/storageaccounts",
            "kind": "StorageV2",
            "location": "westeurope",
            "resourceGroup": "rg1",
            "subscriptionId": "sub1"
        });
        let acct = parse_account(&row).expect("expected an account");
        assert_eq!(acct.name, "mystg");
        assert_eq!(acct.resource_group, "rg1");
        assert_eq!(acct.subscription_id, "sub1");
        assert_eq!(acct.location, "westeurope");
        assert_eq!(acct.kind.as_deref(), Some("StorageV2"));
        // Metadata fields are absent in this fixture → all `None`.
        assert_eq!(acct.sku, None);
        assert_eq!(acct.access_tier, None);
        assert_eq!(acct.is_hns_enabled, None);
        assert_eq!(acct.https_only, None);
        assert_eq!(acct.allow_blob_public_access, None);
    }

    #[test]
    fn parses_storage_account_metadata_columns() {
        // Shape Resource Graph actually returns: `sku` is `{name, tier}` and
        // metadata booleans live under `properties`.
        let full = json!({
            "id": "/subscriptions/s/resourceGroups/rg/providers/Microsoft.Storage/storageAccounts/full",
            "name": "full",
            "type": "microsoft.storage/storageaccounts",
            "kind": "StorageV2",
            "location": "westeurope",
            "resourceGroup": "rg",
            "subscriptionId": "s",
            "sku": { "name": "Standard_GRS", "tier": "Standard" },
            "properties": {
                "accessTier": "Hot",
                "isHnsEnabled": true,
                "supportsHttpsTrafficOnly": true,
                "allowBlobPublicAccess": false,
            }
        });
        let acct = parse_account(&full).expect("expected an account");
        assert_eq!(acct.sku.as_deref(), Some("Standard_GRS"));
        assert_eq!(acct.access_tier.as_deref(), Some("Hot"));
        assert_eq!(acct.is_hns_enabled, Some(true));
        assert_eq!(acct.https_only, Some(true));
        assert_eq!(acct.allow_blob_public_access, Some(false));

        // Same shape but with string booleans — Resource Graph has been
        // observed surfacing these as strings on some tenants.
        let stringy = json!({
            "id": "/subscriptions/s/resourceGroups/rg/providers/Microsoft.Storage/storageAccounts/stringy",
            "name": "stringy",
            "type": "microsoft.storage/storageaccounts",
            "kind": "StorageV2",
            "location": "westeurope",
            "resourceGroup": "rg",
            "subscriptionId": "s",
            "sku": { "name": "Standard_LRS" },
            "properties": {
                "accessTier": "Cool",
                "isHnsEnabled": "false",
                "supportsHttpsTrafficOnly": "True",
                "allowBlobPublicAccess": "FALSE",
            }
        });
        let acct = parse_account(&stringy).expect("expected an account");
        assert_eq!(acct.is_hns_enabled, Some(false));
        assert_eq!(acct.https_only, Some(true));
        assert_eq!(acct.allow_blob_public_access, Some(false));

        // Properties bag entirely missing → all metadata None, account still parses.
        let bare = json!({
            "id": "/subscriptions/s/resourceGroups/rg/providers/Microsoft.Storage/storageAccounts/bare",
            "name": "bare",
            "type": "microsoft.storage/storageaccounts",
            "kind": "Storage",
            "location": "westeurope",
            "resourceGroup": "rg",
            "subscriptionId": "s",
        });
        let acct = parse_account(&bare).expect("expected an account");
        assert_eq!(acct.sku, None);
        assert_eq!(acct.access_tier, None);
        assert_eq!(acct.is_hns_enabled, None);
        assert_eq!(acct.https_only, None);
        assert_eq!(acct.allow_blob_public_access, None);
    }

    #[test]
    fn skips_non_storage_rows_in_account_parser() {
        let row = json!({
            "id": "/subscriptions/x/resourceGroups/y/providers/Microsoft.Web/sites/z",
            "name": "z",
            "type": "microsoft.web/sites",
            "kind": "functionapp",
            "location": "westeurope",
            "resourceGroup": "y",
            "subscriptionId": "x"
        });
        assert!(parse_account(&row).is_none());
    }

    #[test]
    fn parses_list_containers_json() {
        // Trimmed-down ARM response shape from
        // `/blobServices/default/containers?api-version=2023-05-01`.
        let payload = json!({
            "value": [
                {
                    "id": "/subscriptions/s/resourceGroups/r/providers/Microsoft.Storage/storageAccounts/a/blobServices/default/containers/logs",
                    "name": "logs",
                    "type": "Microsoft.Storage/storageAccounts/blobServices/containers",
                    "properties": {
                        "publicAccess": "None",
                        "lastModifiedTime": "2026-05-18T12:34:56.0000000Z",
                        "hasImmutabilityPolicy": false
                    }
                },
                {
                    "name": "public-assets",
                    "properties": {
                        "publicAccess": "Blob",
                        "lastModifiedTime": "2026-04-01T09:00:00.0000000Z",
                        "hasImmutabilityPolicy": true
                    }
                },
                {
                    // No properties block at all — should still parse the name.
                    "name": "bare"
                },
                {
                    // Missing name — must be skipped.
                    "properties": { "publicAccess": "None" }
                }
            ]
        });

        let parsed = parse_containers_json(&payload);
        assert_eq!(parsed.len(), 3);

        assert_eq!(parsed[0].name, "logs");
        assert_eq!(parsed[0].public_access.as_deref(), Some("None"));
        assert_eq!(parsed[0].has_immutability_policy, Some(false));
        assert!(parsed[0].last_modified.is_some());

        assert_eq!(parsed[1].name, "public-assets");
        assert_eq!(parsed[1].public_access.as_deref(), Some("Blob"));
        assert_eq!(parsed[1].has_immutability_policy, Some(true));

        assert_eq!(parsed[2].name, "bare");
        assert_eq!(parsed[2].public_access, None);
        assert_eq!(parsed[2].has_immutability_policy, None);
        assert_eq!(parsed[2].last_modified, None);
    }

    #[test]
    fn list_containers_json_handles_missing_value_array() {
        // 200 with `{}` body shouldn't blow up — return an empty list.
        let parsed = parse_containers_json(&json!({}));
        assert!(parsed.is_empty());
    }

    #[test]
    fn parses_list_blobs_xml_happy_path() {
        // Real-world XML shape from the ListBlobs REST API (trimmed).
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<EnumerationResults ServiceEndpoint="https://acct.blob.core.windows.net/" ContainerName="logs">
  <Blobs>
    <Blob>
      <Name>app/2026/05/18.log</Name>
      <Properties>
        <Last-Modified>Mon, 18 May 2026 12:34:56 GMT</Last-Modified>
        <Etag>0x8DC123</Etag>
        <Content-Length>4096</Content-Length>
        <Content-Type>text/plain</Content-Type>
        <BlobType>BlockBlob</BlobType>
      </Properties>
    </Blob>
    <Blob>
      <Name>img/cat.png</Name>
      <Properties>
        <Last-Modified>Tue, 01 Apr 2026 09:00:00 GMT</Last-Modified>
        <Content-Length>10485760</Content-Length>
        <Content-Type>image/png</Content-Type>
        <BlobType>BlockBlob</BlobType>
      </Properties>
    </Blob>
  </Blobs>
  <NextMarker />
</EnumerationResults>"#;
        let blobs = parse_list_blobs_xml(xml).expect("parse should succeed");
        assert_eq!(blobs.len(), 2);

        assert_eq!(blobs[0].name, "app/2026/05/18.log");
        assert_eq!(blobs[0].size, 4096);
        assert_eq!(blobs[0].content_type.as_deref(), Some("text/plain"));
        assert_eq!(blobs[0].blob_type, "BlockBlob");
        assert!(blobs[0].last_modified.is_some());

        assert_eq!(blobs[1].name, "img/cat.png");
        assert_eq!(blobs[1].size, 10_485_760);
        assert_eq!(blobs[1].content_type.as_deref(), Some("image/png"));
    }

    #[test]
    fn parses_list_blobs_xml_empty_container() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<EnumerationResults ServiceEndpoint="https://acct.blob.core.windows.net/" ContainerName="empty">
  <Blobs />
  <NextMarker />
</EnumerationResults>"#;
        let blobs = parse_list_blobs_xml(xml).expect("parse should succeed");
        assert!(blobs.is_empty());
    }

    #[test]
    fn text_preview_decision_matrix() {
        // Explicit textual MIME types.
        assert!(should_preview_as_text(Some("text/plain"), 5_000_000));
        assert!(should_preview_as_text(
            Some("application/json; charset=utf-8"),
            5_000_000
        ));
        assert!(should_preview_as_text(Some("application/xml"), 1));
        assert!(should_preview_as_text(Some("application/javascript"), 1));

        // Binary MIMEs never previewed as text.
        assert!(!should_preview_as_text(Some("image/png"), 1));
        assert!(!should_preview_as_text(Some("application/octet-stream"), 1));

        // Unknown MIME — small files yes, large files no.
        assert!(should_preview_as_text(None, 1024));
        assert!(should_preview_as_text(None, 64 * 1024));
        assert!(!should_preview_as_text(None, 64 * 1024 + 1));
    }

    #[test]
    fn human_bytes_formats_each_magnitude() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(2_048), "2.0 KB");
        assert_eq!(human_bytes(1_572_864), "1.5 MB");
        assert_eq!(human_bytes(2 * 1024 * 1024 * 1024), "2.0 GB");
    }

    #[test]
    fn url_encoders_preserve_slashes_and_escape_spaces() {
        assert_eq!(urlencode("foo bar/baz"), "foo%20bar/baz");
        assert_eq!(
            encode_blob_path("dir/with space/file.txt"),
            "dir/with%20space/file.txt"
        );
    }

    #[test]
    fn classifies_403_with_role_hint() {
        let err = classify_data_plane_error("acct", anyhow!("azure api error 403: forbidden"));
        let msg = format!("{err}");
        assert!(msg.contains("Storage Blob Data Reader"), "got: {msg}");
        assert!(msg.contains("acct"), "got: {msg}");
    }

    #[test]
    fn classifies_dns_failure_with_account_name() {
        let err = classify_data_plane_error(
            "ghost",
            anyhow!("network error: dns error: failed to lookup address"),
        );
        let msg = format!("{err}");
        assert!(msg.contains("ghost.blob.core.windows.net"), "got: {msg}");
    }

    #[test]
    fn parse_http_date_accepts_rfc1123() {
        let dt = parse_http_date("Mon, 18 May 2026 12:34:56 GMT").expect("should parse");
        assert_eq!(dt.to_rfc3339(), "2026-05-18T12:34:56+00:00");
    }

    #[test]
    fn parse_overview_metrics_picks_latest_nonnull_datapoint() {
        // Real-world shape from /providers/Microsoft.Insights/metrics with
        // `metricnames=BlobCount,BlobCapacity,ContainerCount&aggregation=Average`.
        // The trailing datapoint in `BlobCount` is null (capacity-style metrics
        // often have a stale-tail bucket); the parser must skip to the previous
        // entry rather than returning 0.
        let payload = json!({
            "value": [
                {
                    "name": { "value": "BlobCount", "localizedValue": "Blob Count" },
                    "unit": "Count",
                    "timeseries": [{
                        "data": [
                            { "timeStamp": "2026-05-19T00:00:00Z", "average": 100.0 },
                            { "timeStamp": "2026-05-20T00:00:00Z", "average": 4_360_000.0 },
                            { "timeStamp": "2026-05-21T00:00:00Z" }
                        ]
                    }]
                },
                {
                    "name": { "value": "BlobCapacity", "localizedValue": "Blob Capacity" },
                    "unit": "Bytes",
                    "timeseries": [{
                        "data": [
                            { "timeStamp": "2026-05-20T00:00:00Z", "average": 5_012_316_192_768.0 }
                        ]
                    }]
                },
                {
                    "name": { "value": "ContainerCount", "localizedValue": "Container Count" },
                    "unit": "Count",
                    "timeseries": [{
                        "data": [
                            { "timeStamp": "2026-05-20T00:00:00Z", "average": 49.0 }
                        ]
                    }]
                }
            ]
        });

        let out = parse_overview_metrics_response(&payload, BLOB_METRICS);
        // 3 metrics requested, 3 returned.
        assert_eq!(out.len(), 3);
        let map: std::collections::HashMap<_, _> = out.into_iter().collect();
        assert_eq!(map.get("BlobCount").unwrap().0, Some(4_360_000));
        assert_eq!(map.get("BlobCapacity").unwrap().0, Some(5_012_316_192_768));
        assert_eq!(map.get("ContainerCount").unwrap().0, Some(49));
        // Latest stamp must be 2026-05-20 (the null tail is ignored).
        assert_eq!(
            map.get("BlobCount").unwrap().1.to_rfc3339(),
            "2026-05-20T00:00:00+00:00",
        );
    }

    #[test]
    fn parse_overview_metrics_handles_empty_or_missing_series() {
        // `FileShareCount` returns an entry but with empty data — must NOT show
        // up in the output (UI surfaces `—` for missing fields).
        let payload = json!({
            "value": [
                {
                    "name": { "value": "FileCount", "localizedValue": "File Count" },
                    "unit": "Count",
                    "timeseries": [{ "data": [] }]
                },
                {
                    "name": { "value": "FileShareCount", "localizedValue": "File Share Count" },
                    "unit": "Count",
                    "timeseries": []
                }
                // FileCapacity entirely omitted.
            ]
        });
        let out = parse_overview_metrics_response(&payload, FILE_METRICS);
        // None of the three requested metrics produced a usable point.
        assert!(
            out.is_empty(),
            "empty / missing series should yield no entries, got {out:?}",
        );
    }

    #[test]
    fn parse_overview_metrics_skips_unknown_metric_names() {
        // A response with an unrelated metric name must not appear in the
        // output, even if the data shape is otherwise valid.
        let payload = json!({
            "value": [
                {
                    "name": { "value": "RandomMetric", "localizedValue": "x" },
                    "unit": "Count",
                    "timeseries": [{
                        "data": [{ "timeStamp": "2026-05-20T00:00:00Z", "average": 5.0 }]
                    }]
                },
                {
                    "name": { "value": "QueueCount", "localizedValue": "Queue Count" },
                    "unit": "Count",
                    "timeseries": [{
                        "data": [{ "timeStamp": "2026-05-20T00:00:00Z", "average": 3.0 }]
                    }]
                }
            ]
        });
        let out = parse_overview_metrics_response(&payload, QUEUE_METRICS);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "QueueCount");
        assert_eq!(out[0].1 .0, Some(3));
    }

    #[test]
    fn parse_overview_metrics_clamps_negative_or_nonfinite_to_none() {
        // Capacity metrics shouldn't return negatives, but defend against it
        // (and against NaN / infinity) — saturating-as-u64 would wrap to a
        // huge number that the UI would then format as petabytes.
        let payload = json!({
            "value": [
                {
                    "name": { "value": "UsedCapacity", "localizedValue": "Used Capacity" },
                    "unit": "Bytes",
                    "timeseries": [{
                        "data": [{ "timeStamp": "2026-05-20T00:00:00Z", "average": -42.0 }]
                    }]
                }
            ]
        });
        let out = parse_overview_metrics_response(&payload, ACCOUNT_METRICS);
        assert!(
            out.is_empty(),
            "negative average must be filtered, got {out:?}"
        );
    }
}
