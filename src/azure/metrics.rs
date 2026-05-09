//! Azure Monitor metrics fetch + per-resource-type metric mapping.
//!
//! All resource types expose four logical metrics through this module — Errors,
//! Traffic, Cpu, Memory — backed by different physical metric names depending
//! on the resource. Callers see a uniform `Vec<MetricSeries>`.

#![allow(dead_code, unused_variables)]

use anyhow::anyhow;
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use crate::azure::auth::AzureAuth;
use crate::azure::client::ArmClient;
use crate::azure::resources::{Resource, ResourceKind};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum MetricKind {
    Errors,
    Traffic,
    Cpu,
    Memory,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, Default)]
pub enum TimeRange {
    #[default]
    Day,
    Week,
}

impl TimeRange {
    /// ISO-8601 timespan for the Monitor `timespan` query parameter.
    ///
    /// Uses `Z` as the UTC marker (not `+00:00`): Azure decodes query strings
    /// as `application/x-www-form-urlencoded`, which turns an unencoded `+`
    /// into a space and breaks timestamp parsing on the server.
    pub fn timespan(&self) -> String {
        let end = Utc::now();
        let start = end - self.duration();
        format!(
            "{}/{}",
            start.to_rfc3339_opts(SecondsFormat::Secs, true),
            end.to_rfc3339_opts(SecondsFormat::Secs, true),
        )
    }

    pub fn duration(&self) -> chrono::Duration {
        match self {
            TimeRange::Day => chrono::Duration::hours(24),
            TimeRange::Week => chrono::Duration::days(7),
        }
    }

    /// ISO-8601 grain for `interval`. Day → `PT15M`, Week → `PT1H`.
    pub fn interval(&self) -> &'static str {
        match self {
            TimeRange::Day => "PT15M",
            TimeRange::Week => "PT1H",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            TimeRange::Day => "1d",
            TimeRange::Week => "7d",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MetricPoint {
    pub ts: DateTime<Utc>,
    pub value: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MetricSeries {
    pub kind: MetricKind,
    /// Display label, e.g. `"Http 5xx"`, `"Requests"`, `"CPU time"`, `"Memory"`.
    pub label: String,
    /// Display unit, e.g. `"count"`, `"%"`, `"bytes"`, `"ms"`.
    pub unit: String,
    pub points: Vec<MetricPoint>,
}

impl MetricSeries {
    pub fn latest(&self) -> Option<f64> {
        self.points.last().map(|p| p.value)
    }

    pub fn sum(&self) -> f64 {
        self.points.iter().map(|p| p.value).sum()
    }

    pub fn max(&self) -> f64 {
        self.points.iter().map(|p| p.value).fold(f64::NEG_INFINITY, f64::max)
    }
}

/// Per-resource-type physical-metric mapping. Lane 2 turns this into actual
/// Monitor REST queries (some resource types need `$filter` for dimensions).
pub fn metric_names(kind: ResourceKind) -> &'static [(MetricKind, &'static str, &'static str)] {
    match kind {
        ResourceKind::FunctionApp => &[
            (MetricKind::Errors, "Http5xx", "Total"),
            (MetricKind::Traffic, "Requests", "Total"),
            (MetricKind::Cpu, "CpuTime", "Total"),
            (MetricKind::Memory, "MemoryWorkingSet", "Average"),
        ],
        ResourceKind::Apim => &[
            // Errors via $filter on GatewayResponseCodeCategory eq '5xx'
            (MetricKind::Errors, "Requests", "Total"),
            (MetricKind::Traffic, "Requests", "Total"),
            (MetricKind::Cpu, "Capacity", "Average"),
            // No memory metric on APIM
        ],
        ResourceKind::ContainerApp => &[
            // Errors via $filter on statusCode startswith '5'
            (MetricKind::Errors, "Requests", "Total"),
            (MetricKind::Traffic, "Requests", "Total"),
            (MetricKind::Cpu, "UsageNanoCores", "Average"),
            (MetricKind::Memory, "WorkingSetBytes", "Average"),
        ],
    }
}

/// Friendly display label for a metric.
fn label_for(kind: MetricKind, resource_kind: ResourceKind) -> &'static str {
    match (kind, resource_kind) {
        (MetricKind::Errors, _) => "Http 5xx",
        (MetricKind::Traffic, _) => "Requests",
        (MetricKind::Cpu, ResourceKind::Apim) => "Capacity",
        (MetricKind::Cpu, _) => "CPU",
        (MetricKind::Memory, _) => "Memory",
    }
}

/// Resolve a Monitor `unit` string into a short display tag.
fn short_unit(monitor_unit: &str) -> String {
    match monitor_unit.to_lowercase().as_str() {
        "count" => "count".to_string(),
        "percent" => "%".to_string(),
        "bytes" => "bytes".to_string(),
        "bytespersecond" => "bytes/s".to_string(),
        "milliseconds" => "ms".to_string(),
        "seconds" => "s".to_string(),
        "" | "unspecified" => "".to_string(),
        other => other.to_string(),
    }
}

/// Pick the data-point field name for the requested aggregation.
fn aggregation_field(aggregation: &str) -> &'static str {
    match aggregation.to_lowercase().as_str() {
        "total" => "total",
        "average" => "average",
        "maximum" => "maximum",
        "minimum" => "minimum",
        "count" => "count",
        _ => "total",
    }
}

/// Fetch all relevant metrics for a resource over the given range, in parallel
/// where possible. Missing metrics (e.g. APIM has no memory) are simply absent
/// from the returned vec — never an error.
pub async fn fetch(
    auth: &AzureAuth,
    resource: &Resource,
    range: TimeRange,
) -> anyhow::Result<Vec<MetricSeries>> {
    let client = ArmClient::new(auth.clone())?;
    let mappings = metric_names(resource.kind);
    let timespan = range.timespan();
    let interval = range.interval().to_string();

    // Group 1: regular (non-error-filtered) metrics.
    // Group 2: error metric for APIM/ContainerApp where a $filter is required.
    let needs_error_filter = matches!(resource.kind, ResourceKind::Apim | ResourceKind::ContainerApp);

    let regular: Vec<&(MetricKind, &str, &str)> = mappings
        .iter()
        .filter(|(k, _, _)| !(needs_error_filter && *k == MetricKind::Errors))
        .collect();
    let error_only: Option<&(MetricKind, &str, &str)> = if needs_error_filter {
        mappings.iter().find(|(k, _, _)| *k == MetricKind::Errors)
    } else {
        None
    };

    let path = format!(
        "{}/providers/Microsoft.Insights/metrics",
        resource.id.trim_end_matches('/')
    );

    // Build the parallel futures. We use tokio::spawn so each request is a real task.
    let mut handles: Vec<tokio::task::JoinHandle<anyhow::Result<Vec<MetricSeries>>>> = Vec::new();

    if !regular.is_empty() {
        let metricnames = regular
            .iter()
            .map(|(_, n, _)| *n)
            .collect::<Vec<_>>()
            .join(",");
        let aggregation = regular
            .iter()
            .map(|(_, _, a)| *a)
            .collect::<Vec<_>>()
            .join(",");
        let regular_owned: Vec<(MetricKind, String, String)> = regular
            .iter()
            .map(|(k, n, a)| (*k, (*n).to_string(), (*a).to_string()))
            .collect();
        let path = path.clone();
        let timespan = timespan.clone();
        let interval = interval.clone();
        let resource_kind = resource.kind;
        let client = client.clone();
        handles.push(tokio::spawn(async move {
            let value = client
                .get(
                    &path,
                    &[
                        ("api-version", "2023-10-01"),
                        ("timespan", &timespan),
                        ("interval", &interval),
                        ("metricnames", &metricnames),
                        ("aggregation", &aggregation),
                    ],
                )
                .await?;
            Ok(parse_metrics_response(&value, &regular_owned, resource_kind))
        }));
    }

    if let Some((kind, name, agg)) = error_only {
        let metricnames = (*name).to_string();
        let aggregation = (*agg).to_string();
        let filter = match resource.kind {
            ResourceKind::Apim => "GatewayResponseCodeCategory eq '5xx'".to_string(),
            ResourceKind::ContainerApp => "statusCode startswith '5'".to_string(),
            // Function App handled in `regular` group above.
            ResourceKind::FunctionApp => unreachable!(),
        };
        let owned = vec![(*kind, metricnames.clone(), aggregation.clone())];
        let path = path.clone();
        let timespan = timespan.clone();
        let interval = interval.clone();
        let resource_kind = resource.kind;
        let client = client.clone();
        handles.push(tokio::spawn(async move {
            let value = client
                .get(
                    &path,
                    &[
                        ("api-version", "2023-10-01"),
                        ("timespan", &timespan),
                        ("interval", &interval),
                        ("metricnames", &metricnames),
                        ("aggregation", &aggregation),
                        ("$filter", &filter),
                    ],
                )
                .await?;
            Ok(parse_metrics_response(&value, &owned, resource_kind))
        }));
    }

    let mut out: Vec<MetricSeries> = Vec::new();
    for h in handles {
        match h.await {
            Ok(Ok(mut v)) => out.append(&mut v),
            Ok(Err(e)) => return Err(e),
            Err(join_err) => return Err(anyhow!("metrics task join error: {join_err}")),
        }
    }
    Ok(out)
}

/// Parse a single Monitor metrics response. `requested` carries the
/// `(MetricKind, metric-name, aggregation)` triples that this call asked for —
/// we use it both to map response entries back to our `MetricKind` and to know
/// which response field to pluck (`total` vs `average` vs …).
fn parse_metrics_response(
    value: &serde_json::Value,
    requested: &[(MetricKind, String, String)],
    resource_kind: ResourceKind,
) -> Vec<MetricSeries> {
    let metrics = match value.get("value").and_then(|v| v.as_array()) {
        Some(a) => a,
        None => return Vec::new(),
    };

    let mut out = Vec::new();

    for m in metrics {
        // The metric "name" comes through as { "value": "Http5xx", "localizedValue": "..." }
        let name = m
            .get("name")
            .and_then(|n| n.get("value"))
            .and_then(|n| n.as_str())
            .unwrap_or("");

        // Find which requested metric this corresponds to. Match on name; if
        // multiple requested entries share a physical name (rare), pick the first.
        let (kind, _physical, aggregation) = match requested.iter().find(|(_, n, _)| n == name) {
            Some(triple) => triple,
            None => continue,
        };

        let unit = m.get("unit").and_then(|u| u.as_str()).unwrap_or("");
        let agg_field = aggregation_field(aggregation);

        let timeseries = m
            .get("timeseries")
            .and_then(|t| t.as_array())
            .and_then(|a| a.first());
        let points: Vec<MetricPoint> = match timeseries {
            Some(ts) => ts
                .get("data")
                .and_then(|d| d.as_array())
                .map(|data| {
                    data.iter()
                        .filter_map(|d| {
                            let ts_str = d.get("timeStamp").and_then(|t| t.as_str())?;
                            let ts = DateTime::parse_from_rfc3339(ts_str).ok()?.with_timezone(&Utc);
                            let v = d.get(agg_field).and_then(|x| x.as_f64()).unwrap_or(0.0);
                            Some(MetricPoint { ts, value: v })
                        })
                        .collect()
                })
                .unwrap_or_default(),
            None => Vec::new(),
        };

        out.push(MetricSeries {
            kind: *kind,
            label: label_for(*kind, resource_kind).to_string(),
            unit: short_unit(unit),
            points,
        });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn timespan_uses_z_suffix_not_plus_offset() {
        // `+` in a query string decodes to space under
        // application/x-www-form-urlencoded, which is how Azure parses these.
        // We must serialize UTC as `Z` to round-trip correctly.
        let span = TimeRange::Day.timespan();
        assert!(!span.contains('+'), "timespan should not contain '+': {span}");
        assert_eq!(span.matches('Z').count(), 2, "expected two Z markers: {span}");
        assert!(span.contains('/'), "missing start/end separator: {span}");
    }

    #[test]
    fn parses_function_app_metrics_response() {
        let payload = json!({
            "value": [
                {
                    "id": "/subscriptions/x/resourceGroups/y/providers/Microsoft.Web/sites/z/providers/Microsoft.Insights/metrics/Http5xx",
                    "type": "Microsoft.Insights/metrics",
                    "name": { "value": "Http5xx", "localizedValue": "Http Server Errors" },
                    "unit": "Count",
                    "timeseries": [
                        {
                            "metadatavalues": [],
                            "data": [
                                { "timeStamp": "2026-01-01T00:00:00Z", "total": 0.0 },
                                { "timeStamp": "2026-01-01T00:15:00Z", "total": 3.0 }
                            ]
                        }
                    ]
                },
                {
                    "id": "/.../Requests",
                    "type": "Microsoft.Insights/metrics",
                    "name": { "value": "Requests", "localizedValue": "Requests" },
                    "unit": "Count",
                    "timeseries": [
                        {
                            "data": [
                                { "timeStamp": "2026-01-01T00:00:00Z", "total": 100.0 },
                                { "timeStamp": "2026-01-01T00:15:00Z", "total": 200.0 }
                            ]
                        }
                    ]
                }
            ]
        });

        let requested = vec![
            (MetricKind::Errors, "Http5xx".to_string(), "Total".to_string()),
            (MetricKind::Traffic, "Requests".to_string(), "Total".to_string()),
        ];

        let series = parse_metrics_response(&payload, &requested, ResourceKind::FunctionApp);
        assert_eq!(series.len(), 2);

        let errors = series.iter().find(|s| s.kind == MetricKind::Errors).unwrap();
        assert_eq!(errors.label, "Http 5xx");
        assert_eq!(errors.unit, "count");
        assert_eq!(errors.points.len(), 2);
        assert_eq!(errors.points[1].value, 3.0);

        let traffic = series.iter().find(|s| s.kind == MetricKind::Traffic).unwrap();
        assert_eq!(traffic.points.len(), 2);
        assert_eq!(traffic.sum(), 300.0);
    }

    #[test]
    fn empty_timeseries_yields_empty_points() {
        let payload = json!({
            "value": [
                {
                    "name": { "value": "Requests", "localizedValue": "Requests" },
                    "unit": "Count",
                    "timeseries": []
                }
            ]
        });

        let requested = vec![
            (MetricKind::Traffic, "Requests".to_string(), "Total".to_string()),
        ];
        let series = parse_metrics_response(&payload, &requested, ResourceKind::Apim);
        assert_eq!(series.len(), 1);
        assert!(series[0].points.is_empty());
    }

    #[test]
    fn picks_aggregation_field_average() {
        let payload = json!({
            "value": [
                {
                    "name": { "value": "MemoryWorkingSet", "localizedValue": "Memory" },
                    "unit": "Bytes",
                    "timeseries": [
                        {
                            "data": [
                                { "timeStamp": "2026-01-01T00:00:00Z", "average": 12345.0, "total": 0.0 }
                            ]
                        }
                    ]
                }
            ]
        });

        let requested = vec![(
            MetricKind::Memory,
            "MemoryWorkingSet".to_string(),
            "Average".to_string(),
        )];
        let series = parse_metrics_response(&payload, &requested, ResourceKind::FunctionApp);
        assert_eq!(series.len(), 1);
        assert_eq!(series[0].unit, "bytes");
        assert_eq!(series[0].points[0].value, 12345.0);
    }
}
