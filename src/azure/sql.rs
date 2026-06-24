//! Read-only Azure SQL inspection: elastic pools and single (standalone)
//! databases, with utilization metrics.
//!
//! ## Contract (do not change without coordinating with the UI lane)
//!
//! Two public functions form the surface the UI consumes:
//!
//! - [`list_sql_resources`] — Resource Graph KQL discovery of
//!   `Microsoft.Sql/servers/elasticPools` **and**
//!   `Microsoft.Sql/servers/databases` across the supplied subscriptions. The
//!   per-server system `master` database is filtered out (it carries no useful
//!   utilization and isn't a user database). Control plane only (`Reader`
//!   suffices).
//! - [`fetch_metrics`] — Azure Monitor (`Microsoft.Insights/metrics`) fetch of
//!   the four utilization series (CPU %, eDTU/DTU %, storage %, workers %) for a
//!   single pool or database. One Monitor call per metric, in parallel, so a
//!   metric that doesn't exist for the resource's purchasing model (e.g.
//!   `dtu_consumption_percent` on a vCore database) only loses its own
//!   sparkline rather than failing the whole fetch — mirroring
//!   [`crate::azure::metrics::fetch`].
//!
//! ## Scope decisions worth flagging
//!
//! - **Pools + single databases, flat**: both resource types are surfaced in a
//!   single flat list keyed off the logical server, rather than a
//!   servers→pools drill chain. Databases that live inside an elastic pool are
//!   still listed (they have their own per-database utilization).
//! - **DTU vs vCore**: `dtu_consumption_percent` only exists on DTU-model
//!   resources; vCore pools/databases simply don't report it and the UI shows
//!   `n/a` for that row. `storage_percent` likewise is DTU/most-vCore but not
//!   universal. Both degrade gracefully via the per-metric `missing` map.
//! - **Read-only**: discovery + metrics only. No scale / pause / DDL codepaths.

#![allow(dead_code, unused_variables)]

use std::collections::HashMap;

use anyhow::{anyhow, Context};
use chrono::{DateTime, Utc};

use crate::azure::auth::AzureAuth;
use crate::azure::client::ArmClient;
use crate::azure::metrics::{MetricKind, MetricPoint, MetricSeries, MetricsResult, TimeRange};

/// Whether a listed SQL resource is an elastic pool or a standalone database.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SqlKind {
    ElasticPool,
    Database,
}

impl SqlKind {
    /// Short tag for the list's KIND column.
    pub fn short_tag(self) -> &'static str {
        match self {
            SqlKind::ElasticPool => "Pool",
            SqlKind::Database => "Database",
        }
    }
}

/// One Azure SQL resource (elastic pool or single database) discovered via
/// Resource Graph.
#[derive(Clone, Debug)]
pub struct SqlResource {
    /// Full ARM resource id.
    pub id: String,
    /// Pool or database name (the leaf resource name).
    pub name: String,
    /// Logical SQL server the resource lives under, parsed from the id.
    pub server: String,
    pub resource_group: String,
    pub subscription_id: String,
    pub location: String,
    pub kind: SqlKind,
    /// SKU name, e.g. `GP_Gen5`, `StandardPool`, `BC_Gen5_2`.
    pub sku_name: Option<String>,
    /// SKU tier, e.g. `GeneralPurpose`, `Standard`, `BusinessCritical`.
    pub sku_tier: Option<String>,
    /// Provisioned capacity: eDTU count (DTU model) or vCore count (vCore model).
    pub capacity: Option<i64>,
    /// Database state (`Online`, `Paused`, …) or elastic-pool state (`Ready`,
    /// `Disabled`). `None` when the field is absent.
    pub status: Option<String>,
    /// For databases: the elastic pool ARM id this database belongs to, if any.
    /// `None` for standalone databases and for pools themselves.
    pub elastic_pool_id: Option<String>,
    /// Max storage in bytes, when reported.
    pub max_size_bytes: Option<i64>,
}

impl SqlResource {
    /// Whether this database is a member of an elastic pool. Always `false` for
    /// pools.
    pub fn is_pooled(&self) -> bool {
        self.elastic_pool_id.is_some()
    }
}

/// Resource Graph KQL for Azure SQL elastic pools + single databases. The
/// `master` system database (one per server) is excluded — it's not a user
/// database and reports no useful utilization. `sku` and `properties` ride
/// along for the SKU/tier/capacity/status columns; the Rust parser
/// ([`parse_resource`]) reads them defensively because the shape differs
/// between pools and databases.
const SQL_KQL: &str = r#"
Resources
| where type in~ ('microsoft.sql/servers/elasticpools', 'microsoft.sql/servers/databases')
| where name != 'master'
| project id, name, type, location, resourceGroup, subscriptionId, sku, properties
| order by name asc
"#;

/// ARM Monitor metrics API version (same as `crate::azure::metrics`).
const METRICS_API_VERSION: &str = "2023-10-01";

/// The four utilization metrics charted for SQL pools / databases, as
/// `(logical kind, physical metric name, display label)`. All are read with
/// the `Average` aggregation and reported as a percentage by Azure Monitor.
const SQL_METRICS: &[(MetricKind, &str, &str)] = &[
    (MetricKind::Cpu, "cpu_percent", "CPU"),
    (MetricKind::Dtu, "dtu_consumption_percent", "eDTU"),
    (MetricKind::Storage, "storage_percent", "Storage"),
    (MetricKind::Workers, "workers_percent", "Workers"),
];

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Enumerate Azure SQL elastic pools + single databases across
/// `subscription_ids`. Empty slice → all subscriptions visible to the
/// credential.
pub async fn list_sql_resources(
    auth: &AzureAuth,
    subscription_ids: &[String],
) -> anyhow::Result<Vec<SqlResource>> {
    let client = ArmClient::new(auth.clone())?;

    let body = if subscription_ids.is_empty() {
        serde_json::json!({ "query": SQL_KQL })
    } else {
        serde_json::json!({
            "subscriptions": subscription_ids,
            "query": SQL_KQL,
        })
    };

    let resp = client
        .post(
            "/providers/Microsoft.ResourceGraph/resources?api-version=2022-10-01",
            &body,
        )
        .await
        .context("resource graph: list azure sql pools and databases")?;

    let rows = resp
        .get("data")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("resource graph response missing 'data' array"))?;

    if rows.len() >= 1000 {
        tracing::warn!(
            "resource graph returned {} sql resources; pagination not implemented in v1",
            rows.len()
        );
    }

    Ok(rows.iter().filter_map(parse_resource).collect())
}

/// Fetch the four utilization series for one pool / database over `range`. One
/// Monitor call per metric, in parallel; a metric absent for the resource's
/// purchasing model lands in `missing` rather than failing the whole fetch.
/// Returns `Err` only when *every* metric call failed.
pub async fn fetch_metrics(
    auth: &AzureAuth,
    resource_id: &str,
    range: TimeRange,
) -> anyhow::Result<MetricsResult> {
    let client = ArmClient::new(auth.clone())?;
    let timespan = range.timespan();
    let interval = range.interval().to_string();
    let path = format!(
        "{}/providers/Microsoft.Insights/metrics",
        resource_id.trim_end_matches('/')
    );

    type Handle = tokio::task::JoinHandle<(MetricKind, Result<Option<MetricSeries>, String>)>;
    let mut handles: Vec<Handle> = Vec::new();

    for (kind, name, label) in SQL_METRICS {
        let kind = *kind;
        let name = (*name).to_string();
        let label = (*label).to_string();
        let path = path.clone();
        let timespan = timespan.clone();
        let interval = interval.clone();
        let client = client.clone();

        handles.push(tokio::spawn(async move {
            let params: Vec<(&str, &str)> = vec![
                ("api-version", METRICS_API_VERSION),
                ("timespan", &timespan),
                ("interval", &interval),
                ("metricnames", &name),
                ("aggregation", "Average"),
            ];
            let res = match client.get(&path, &params).await {
                Ok(value) => Ok(parse_metric_series(&value, kind, &name, &label)),
                Err(e) => Err(format!("{e:#}")),
            };
            (kind, res)
        }));
    }

    let mut series: Vec<MetricSeries> = Vec::new();
    let mut missing: HashMap<MetricKind, String> = HashMap::new();
    let mut any_ok = false;
    let mut errors: Vec<String> = Vec::new();
    for h in handles {
        match h.await {
            Ok((_, Ok(Some(s)))) => {
                any_ok = true;
                series.push(s);
            }
            Ok((_, Ok(None))) => {
                any_ok = true;
            }
            Ok((kind, Err(e))) => {
                tracing::debug!("sql metric {kind:?} fetch failed for {resource_id}: {e}");
                missing.insert(kind, e.clone());
                errors.push(e);
            }
            Err(join_err) => errors.push(format!("task join: {join_err}")),
        }
    }

    if !any_ok && !errors.is_empty() {
        return Err(anyhow!("{}", errors.join("; ")));
    }
    Ok(MetricsResult { series, missing })
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parse one Resource Graph row into a [`SqlResource`]. Returns `None` for rows
/// missing the essentials (id / name / type) or with an unrecognized type.
fn parse_resource(v: &serde_json::Value) -> Option<SqlResource> {
    let id = v.get("id").and_then(|x| x.as_str())?.to_string();
    let name = v.get("name").and_then(|x| x.as_str())?.to_string();
    let type_str = v.get("type").and_then(|x| x.as_str())?.to_lowercase();
    let kind = match type_str.as_str() {
        "microsoft.sql/servers/elasticpools" => SqlKind::ElasticPool,
        "microsoft.sql/servers/databases" => SqlKind::Database,
        _ => return None,
    };

    let server = server_from_id(&id).unwrap_or_default();
    let resource_group = v
        .get("resourceGroup")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let subscription_id = v
        .get("subscriptionId")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let location = v
        .get("location")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();

    let sku = v.get("sku");
    let sku_name = sku
        .and_then(|s| s.get("name"))
        .and_then(|x| x.as_str())
        .map(str::to_string);
    let sku_tier = sku
        .and_then(|s| s.get("tier"))
        .and_then(|x| x.as_str())
        .map(str::to_string);
    let capacity = sku.and_then(|s| s.get("capacity")).and_then(|x| x.as_i64());

    let props = v.get("properties");
    // Databases expose `status`; elastic pools expose `state`.
    let status = props
        .and_then(|p| p.get("status").or_else(|| p.get("state")))
        .and_then(|x| x.as_str())
        .map(str::to_string);
    let elastic_pool_id = props
        .and_then(|p| p.get("elasticPoolId"))
        .and_then(|x| x.as_str())
        .map(str::to_string);
    let max_size_bytes = props
        .and_then(|p| p.get("maxSizeBytes"))
        .and_then(|x| x.as_i64());

    Some(SqlResource {
        id,
        name,
        server,
        resource_group,
        subscription_id,
        location,
        kind,
        sku_name,
        sku_tier,
        capacity,
        status,
        elastic_pool_id,
        max_size_bytes,
    })
}

/// Pull the logical server name out of a SQL resource id. Both pools and
/// databases live under `.../Microsoft.Sql/servers/{server}/...`, so we split
/// on the (case-insensitive) `/servers/` segment and take the next path
/// component.
fn server_from_id(id: &str) -> Option<String> {
    let lower = id.to_lowercase();
    let idx = lower.find("/servers/")?;
    let after = &id[idx + "/servers/".len()..];
    let server = after.split('/').next()?;
    if server.is_empty() {
        None
    } else {
        Some(server.to_string())
    }
}

/// Parse a single Monitor metrics response into a [`MetricSeries`] for `kind`.
/// Reads the `Average` data field and the `%` unit. `None` when the response
/// carries no matching metric / timeseries.
fn parse_metric_series(
    value: &serde_json::Value,
    kind: MetricKind,
    physical_name: &str,
    label: &str,
) -> Option<MetricSeries> {
    let metrics = value.get("value").and_then(|v| v.as_array())?;
    let m = metrics.iter().find(|m| {
        m.get("name")
            .and_then(|n| n.get("value"))
            .and_then(|n| n.as_str())
            .map(|n| n.eq_ignore_ascii_case(physical_name))
            .unwrap_or(false)
    })?;

    let unit = m.get("unit").and_then(|u| u.as_str()).unwrap_or("");
    let unit_label = if unit.eq_ignore_ascii_case("percent") {
        "%".to_string()
    } else {
        unit.to_lowercase()
    };

    let data = m
        .get("timeseries")
        .and_then(|t| t.as_array())
        .and_then(|a| a.first())
        .and_then(|ts| ts.get("data"))
        .and_then(|d| d.as_array());

    let points: Vec<MetricPoint> = match data {
        Some(rows) => rows
            .iter()
            .filter_map(|d| {
                let ts_str = d.get("timeStamp").and_then(|t| t.as_str())?;
                let ts = DateTime::parse_from_rfc3339(ts_str)
                    .ok()?
                    .with_timezone(&Utc);
                let v = d.get("average").and_then(|x| x.as_f64()).unwrap_or(0.0);
                Some(MetricPoint { ts, value: v })
            })
            .collect(),
        None => Vec::new(),
    };

    Some(MetricSeries {
        kind,
        label: label.to_string(),
        unit: unit_label,
        points,
        peak_replica: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_from_id_parses_pool_and_database() {
        let pool = "/subscriptions/s/resourceGroups/rg/providers/Microsoft.Sql/servers/srv-1/elasticPools/pool-a";
        let db = "/subscriptions/s/resourceGroups/rg/providers/Microsoft.Sql/servers/srv-2/databases/db-x";
        assert_eq!(server_from_id(pool).as_deref(), Some("srv-1"));
        assert_eq!(server_from_id(db).as_deref(), Some("srv-2"));
        assert_eq!(server_from_id("/no/servers/here/").as_deref(), Some("here"));
        assert_eq!(server_from_id("garbage"), None);
    }

    #[test]
    fn parse_resource_reads_pool_and_database_shapes() {
        let pool = serde_json::json!({
            "id": "/subscriptions/s/resourceGroups/rg/providers/Microsoft.Sql/servers/srv/elasticPools/pool-a",
            "name": "pool-a",
            "type": "microsoft.sql/servers/elasticPools",
            "location": "westeurope",
            "resourceGroup": "rg",
            "subscriptionId": "s",
            "sku": { "name": "StandardPool", "tier": "Standard", "capacity": 100 },
            "properties": { "state": "Ready", "maxSizeBytes": 268435456000_i64 }
        });
        let r = parse_resource(&pool).expect("pool parses");
        assert_eq!(r.kind, SqlKind::ElasticPool);
        assert_eq!(r.server, "srv");
        assert_eq!(r.sku_tier.as_deref(), Some("Standard"));
        assert_eq!(r.capacity, Some(100));
        assert_eq!(r.status.as_deref(), Some("Ready"));
        assert!(!r.is_pooled());

        let db = serde_json::json!({
            "id": "/subscriptions/s/resourceGroups/rg/providers/Microsoft.Sql/servers/srv/databases/db-x",
            "name": "db-x",
            "type": "microsoft.sql/servers/databases",
            "location": "westeurope",
            "resourceGroup": "rg",
            "subscriptionId": "s",
            "sku": { "name": "GP_Gen5_2", "tier": "GeneralPurpose", "capacity": 2 },
            "properties": {
                "status": "Online",
                "elasticPoolId": "/subscriptions/s/resourceGroups/rg/providers/Microsoft.Sql/servers/srv/elasticPools/pool-a"
            }
        });
        let r = parse_resource(&db).expect("db parses");
        assert_eq!(r.kind, SqlKind::Database);
        assert_eq!(r.status.as_deref(), Some("Online"));
        assert!(r.is_pooled());
    }

    #[test]
    fn parse_resource_rejects_unknown_type() {
        let other = serde_json::json!({
            "id": "/x", "name": "y", "type": "microsoft.sql/servers"
        });
        assert!(parse_resource(&other).is_none());
    }

    #[test]
    fn parse_metric_series_reads_average_and_percent() {
        let resp = serde_json::json!({
            "value": [{
                "name": { "value": "cpu_percent" },
                "unit": "Percent",
                "timeseries": [{
                    "data": [
                        { "timeStamp": "2026-06-23T10:00:00Z", "average": 12.5 },
                        { "timeStamp": "2026-06-23T10:15:00Z", "average": 40.0 }
                    ]
                }]
            }]
        });
        let s = parse_metric_series(&resp, MetricKind::Cpu, "cpu_percent", "CPU").unwrap();
        assert_eq!(s.unit, "%");
        assert_eq!(s.points.len(), 2);
        assert_eq!(s.points[1].value, 40.0);
        // A request for a metric not present in the response yields None.
        assert!(
            parse_metric_series(&resp, MetricKind::Dtu, "dtu_consumption_percent", "eDTU")
                .is_none()
        );
    }
}
