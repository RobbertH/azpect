//! Azure Monitor metrics fetch + per-resource-type metric mapping.
//!
//! All resource types expose four logical metrics through this module — Errors,
//! Traffic, Cpu, Memory — backed by different physical metric names depending
//! on the resource. Callers see a uniform `Vec<MetricSeries>`.

#![allow(dead_code, unused_variables)]

use std::collections::HashMap;

use anyhow::anyhow;
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use crate::azure::auth::AzureAuth;
use crate::azure::client::ArmClient;
use crate::azure::resources::{Resource, ResourceKind};

/// Outcome of a per-resource metrics fetch. Carries both the successfully
/// loaded series and per-metric error messages for ones that 4xx'd or weren't
/// exposed for the resource's plan (e.g. `CpuTime` doesn't exist on Premium /
/// App Service-plan Function Apps).
#[derive(Clone, Debug, Default)]
pub struct MetricsResult {
    pub series: Vec<MetricSeries>,
    pub missing: HashMap<MetricKind, String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum MetricKind {
    Errors,
    Traffic,
    Cpu,
    Memory,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, Default)]
pub enum TimeRange {
    /// Last hour, aggregated per PT1M (60 bins). The default: fine-grained
    /// enough to catch a recent spike that 15-minute bars would hide.
    #[default]
    Hour,
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
            TimeRange::Hour => chrono::Duration::hours(1),
            TimeRange::Day => chrono::Duration::hours(24),
            TimeRange::Week => chrono::Duration::days(7),
        }
    }

    /// ISO-8601 grain for `interval`. Hour → `PT1M`, Day → `PT15M`, Week → `PT1H`.
    pub fn interval(&self) -> &'static str {
        match self {
            TimeRange::Hour => "PT1M",
            TimeRange::Day => "PT15M",
            TimeRange::Week => "PT1H",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            TimeRange::Hour => "1h",
            TimeRange::Day => "1d",
            TimeRange::Week => "7d",
        }
    }

    /// Human-readable form of the per-bin aggregation interval. Surfaced in
    /// the detail header so the user understands why a single bar represents
    /// 1 / 15 minutes / 1 hour of data rather than near-realtime.
    pub fn pretty_interval(&self) -> &'static str {
        match self {
            TimeRange::Hour => "1m",
            TimeRange::Day => "15m",
            TimeRange::Week => "1h",
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
    /// Highest single value any one dimension (e.g. a Container App *replica*)
    /// reached over the window — the window peak of a parallel `Maximum`
    /// aggregation, in the same scaled unit as [`Self::points`]. `None` unless
    /// the fetch requested `Maximum` alongside the plotted aggregation (only
    /// Container App CPU/Memory do, where the plotted series is the average
    /// across replicas and this surfaces the busiest replica). See
    /// [`parse_metrics_response`].
    #[serde(default)]
    pub peak_replica: Option<f64>,
}

impl MetricSeries {
    pub fn latest(&self) -> Option<f64> {
        self.points.last().map(|p| p.value)
    }

    pub fn sum(&self) -> f64 {
        self.points.iter().map(|p| p.value).sum()
    }

    pub fn max(&self) -> f64 {
        self.points
            .iter()
            .map(|p| p.value)
            .fold(f64::NEG_INFINITY, f64::max)
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
            // Errors via $filter on statusCodeCategory eq '5xx'
            (MetricKind::Errors, "Requests", "Total"),
            (MetricKind::Traffic, "Requests", "Total"),
            (MetricKind::Cpu, "UsageNanoCores", "Average"),
            (MetricKind::Memory, "WorkingSetBytes", "Average"),
        ],
        ResourceKind::AppGateway => &[
            // Errors via $filter on HttpStatusGroup eq '5xx'
            (MetricKind::Errors, "ResponseStatus", "Total"),
            (MetricKind::Traffic, "TotalRequests", "Total"),
            // CapacityUnits is v2-only; on v1 SKUs the per-metric call 400s
            // and the fetch layer logs it as "missing" without failing the rest.
            (MetricKind::Cpu, "CapacityUnits", "Average"),
            // No memory-equivalent metric on App Gateway.
        ],
    }
}

/// Friendly display label for a metric.
fn label_for(kind: MetricKind, resource_kind: ResourceKind) -> &'static str {
    match (kind, resource_kind) {
        (MetricKind::Errors, _) => "Http 5xx",
        (MetricKind::Traffic, _) => "Requests",
        (MetricKind::Cpu, ResourceKind::Apim) => "Capacity",
        (MetricKind::Cpu, ResourceKind::AppGateway) => "Capacity Units",
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

/// Apply per-metric unit scaling. Container App `UsageNanoCores` lands as raw
/// nanocores (e.g. 12_500_000), which is unreadable in the sparkline header.
/// Scale to millicores (12.5 mCores) and override the unit label. Everything
/// else falls through to the generic `short_unit` mapping.
fn normalize_unit(
    physical_name: &str,
    raw_unit: &str,
    points: Vec<MetricPoint>,
) -> (String, Vec<MetricPoint>) {
    if physical_name == "UsageNanoCores" {
        let scaled = points
            .into_iter()
            .map(|p| MetricPoint {
                ts: p.ts,
                value: scale_metric_value(physical_name, p.value),
            })
            .collect();
        return ("mCores".to_string(), scaled);
    }
    (short_unit(raw_unit), points)
}

/// Apply the same per-metric value scaling [`normalize_unit`] does, for a single
/// scalar (used to scale the `Maximum`-aggregation peak into the display unit).
/// Container App `UsageNanoCores` → millicores; everything else is passed
/// through.
fn scale_metric_value(physical_name: &str, v: f64) -> f64 {
    const NANO_PER_MILLI: f64 = 1_000_000.0;
    if physical_name == "UsageNanoCores" {
        v / NANO_PER_MILLI
    } else {
        v
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

/// Fixed window the health verdict is computed over, independent of whatever
/// chart range the user has selected. `TimeRange::Day` is exactly 24h at PT15M
/// (96 bins) — fine-grained enough for per-bin spike detection. See
/// `azure::health::derive`.
pub const HEALTH_RANGE: TimeRange = TimeRange::Day;

/// Fetch all relevant metrics for a resource over the given range, one Monitor
/// call per metric in parallel. See `fetch_core` for the per-metric rationale.
pub async fn fetch(
    auth: &AzureAuth,
    resource: &Resource,
    range: TimeRange,
) -> anyhow::Result<MetricsResult> {
    fetch_core(auth, &resource.id, resource.kind, range, None).await
}

/// Fetch just the Errors + Traffic series over the fixed [`HEALTH_RANGE`], for
/// the health badge. Kept separate from the chart fetch so the verdict always
/// covers a stable 24h window regardless of which chart range is selected.
/// Returns only the series (the per-metric error map isn't needed for the
/// badge — a missing series degrades to `Unknown` via `derive`).
pub async fn fetch_health(
    auth: &AzureAuth,
    resource_id: &str,
    kind: ResourceKind,
) -> anyhow::Result<Vec<MetricSeries>> {
    let result = fetch_core(
        auth,
        resource_id,
        kind,
        HEALTH_RANGE,
        Some(&[MetricKind::Errors, MetricKind::Traffic]),
    )
    .await?;
    Ok(result.series)
}

/// One Monitor call per (selected) metric, in parallel.
///
/// Per-metric calls are deliberate: Monitor's batch endpoint 400s the *whole*
/// batch when a single name is invalid for the resource's plan (e.g. `CpuTime`
/// only exists on Consumption Function Apps, not Premium/Dedicated). With
/// per-metric calls a bad name only loses that one sparkline, the badge still
/// works from Errors+Traffic, and the user sees partial data rather than an
/// all-or-nothing failure.
///
/// `only` restricts which logical metrics are fetched (e.g. just Errors+Traffic
/// for the health badge); `None` fetches everything the resource type exposes.
///
/// Returns `Err` only if *every* metric call failed; otherwise returns whatever
/// subset succeeded plus a per-metric error map describing which ones didn't.
async fn fetch_core(
    auth: &AzureAuth,
    resource_id: &str,
    resource_kind: ResourceKind,
    range: TimeRange,
    only: Option<&[MetricKind]>,
) -> anyhow::Result<MetricsResult> {
    let client = ArmClient::new(auth.clone())?;
    let mappings: Vec<(MetricKind, &str, &str)> = metric_names(resource_kind)
        .iter()
        .filter(|(k, _, _)| only.is_none_or(|allow| allow.contains(k)))
        .map(|(k, n, a)| (*k, *n, *a))
        .collect();
    let timespan = range.timespan();
    let interval = range.interval().to_string();
    let path = format!(
        "{}/providers/Microsoft.Insights/metrics",
        resource_id.trim_end_matches('/')
    );

    let needs_error_filter = matches!(
        resource_kind,
        ResourceKind::Apim | ResourceKind::ContainerApp | ResourceKind::AppGateway
    );

    type Handle = tokio::task::JoinHandle<(MetricKind, Result<Option<MetricSeries>, String>)>;
    let mut handles: Vec<Handle> = Vec::new();

    for (kind, name, agg) in &mappings {
        let kind = *kind;
        let name = (*name).to_string();
        let agg = (*agg).to_string();
        let filter = if needs_error_filter && kind == MetricKind::Errors {
            match resource_kind {
                ResourceKind::Apim => Some("GatewayResponseCodeCategory eq '5xx'".to_string()),
                // Monitor's $filter only supports `eq`, `ne`, and `sw`
                // (NOT `startswith`), so an earlier `statusCode startswith '5'`
                // got rejected with BadRequest. `statusCodeCategory` is a
                // first-class dimension on Container App `Requests` whose
                // values are `2xx` / `4xx` / `5xx`, so an `eq` on it is both
                // valid syntax and cleaner than `statusCode sw '5'`.
                ResourceKind::ContainerApp => Some("statusCodeCategory eq '5xx'".to_string()),
                // App Gateway exposes `HttpStatusGroup` as `1xx`/`2xx`/.../`5xx`
                // on the `ResponseStatus` metric.
                ResourceKind::AppGateway => Some("HttpStatusGroup eq '5xx'".to_string()),
                ResourceKind::FunctionApp => None,
            }
        } else {
            None
        };
        let path = path.clone();
        let timespan = timespan.clone();
        let interval = interval.clone();
        // `resource_kind` is `Copy`, so each `async move` block captures its own
        // copy and the outer binding stays usable across loop iterations.
        let client = client.clone();

        handles.push(tokio::spawn(async move {
            // Container App CPU/Memory are reported per replica; we plot the
            // average across replicas, but also ask for `Maximum` in the same
            // call so the summary can surface the busiest single replica (which
            // the average hides). The plotted aggregation stays `agg`.
            let agg_param = if resource_kind == ResourceKind::ContainerApp
                && matches!(kind, MetricKind::Cpu | MetricKind::Memory)
            {
                format!("{agg},Maximum")
            } else {
                agg.clone()
            };
            let mut params: Vec<(&str, &str)> = vec![
                ("api-version", "2023-10-01"),
                ("timespan", &timespan),
                ("interval", &interval),
                ("metricnames", &name),
                ("aggregation", &agg_param),
            ];
            if let Some(f) = filter.as_deref() {
                params.push(("$filter", f));
            }
            let res = match client.get(&path, &params).await {
                Ok(value) => {
                    let triple = vec![(kind, name, agg)];
                    let mut series = parse_metrics_response(&value, &triple, resource_kind);
                    Ok(series.pop())
                }
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
                // Metric call returned but had no parseable series. Treat as a
                // soft success so we don't trigger the all-failed Err path.
                any_ok = true;
            }
            Ok((kind, Err(e))) => {
                tracing::debug!("metric {kind:?} fetch failed for {resource_id}: {e}");
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
        let (kind, physical, aggregation) = match requested.iter().find(|(_, n, _)| n == name) {
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
                            let ts = DateTime::parse_from_rfc3339(ts_str)
                                .ok()?
                                .with_timezone(&Utc);
                            let v = d.get(agg_field).and_then(|x| x.as_f64()).unwrap_or(0.0);
                            Some(MetricPoint { ts, value: v })
                        })
                        .collect()
                })
                .unwrap_or_default(),
            None => Vec::new(),
        };

        // Window peak of a parallel `Maximum` aggregation, when the call asked
        // for one (Container App CPU/Memory) — the busiest single replica over
        // the window. Absent in the response otherwise, so this stays `None`.
        let peak_raw = timeseries
            .and_then(|ts| ts.get("data"))
            .and_then(|d| d.as_array())
            .and_then(|data| {
                data.iter()
                    .filter_map(|d| d.get("maximum").and_then(|x| x.as_f64()))
                    .fold(None, |acc: Option<f64>, v| {
                        Some(acc.map_or(v, |a| a.max(v)))
                    })
            });

        let (unit_label, points) = normalize_unit(physical, unit, points);
        let peak_replica = peak_raw.map(|p| scale_metric_value(physical, p));

        out.push(MetricSeries {
            kind: *kind,
            label: label_for(*kind, resource_kind).to_string(),
            unit: unit_label,
            points,
            peak_replica,
        });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hour_range_uses_minute_intervals() {
        // 1h window with PT1M bins gives 60 data points — fine-grained enough
        // for near-realtime investigation without exceeding Monitor limits.
        assert_eq!(TimeRange::Hour.interval(), "PT1M");
        assert_eq!(TimeRange::Hour.label(), "1h");
        assert_eq!(TimeRange::Hour.pretty_interval(), "1m");
        assert_eq!(TimeRange::Hour.duration(), chrono::Duration::hours(1));
    }

    #[test]
    fn timespan_uses_z_suffix_not_plus_offset() {
        // `+` in a query string decodes to space under
        // application/x-www-form-urlencoded, which is how Azure parses these.
        // We must serialize UTC as `Z` to round-trip correctly.
        let span = TimeRange::Day.timespan();
        assert!(
            !span.contains('+'),
            "timespan should not contain '+': {span}"
        );
        assert_eq!(
            span.matches('Z').count(),
            2,
            "expected two Z markers: {span}"
        );
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
            (
                MetricKind::Errors,
                "Http5xx".to_string(),
                "Total".to_string(),
            ),
            (
                MetricKind::Traffic,
                "Requests".to_string(),
                "Total".to_string(),
            ),
        ];

        let series = parse_metrics_response(&payload, &requested, ResourceKind::FunctionApp);
        assert_eq!(series.len(), 2);

        let errors = series
            .iter()
            .find(|s| s.kind == MetricKind::Errors)
            .unwrap();
        assert_eq!(errors.label, "Http 5xx");
        assert_eq!(errors.unit, "count");
        assert_eq!(errors.points.len(), 2);
        assert_eq!(errors.points[1].value, 3.0);

        let traffic = series
            .iter()
            .find(|s| s.kind == MetricKind::Traffic)
            .unwrap();
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

        let requested = vec![(
            MetricKind::Traffic,
            "Requests".to_string(),
            "Total".to_string(),
        )];
        let series = parse_metrics_response(&payload, &requested, ResourceKind::Apim);
        assert_eq!(series.len(), 1);
        assert!(series[0].points.is_empty());
    }

    #[test]
    fn container_app_usage_nanocores_is_scaled_to_millicores() {
        // 12_500_000 nanocores = 12.5 mCores. Unit must override the raw
        // "NanoCores" string Azure returns.
        let payload = json!({
            "value": [
                {
                    "name": { "value": "UsageNanoCores", "localizedValue": "CPU" },
                    "unit": "NanoCores",
                    "timeseries": [
                        {
                            "data": [
                                { "timeStamp": "2026-01-01T00:00:00Z", "average": 12_500_000.0 },
                                { "timeStamp": "2026-01-01T00:15:00Z", "average": 0.0 }
                            ]
                        }
                    ]
                }
            ]
        });
        let requested = vec![(
            MetricKind::Cpu,
            "UsageNanoCores".to_string(),
            "Average".to_string(),
        )];
        let series = parse_metrics_response(&payload, &requested, ResourceKind::ContainerApp);
        assert_eq!(series.len(), 1);
        assert_eq!(series[0].unit, "mCores");
        assert!(
            (series[0].points[0].value - 12.5).abs() < 1e-9,
            "expected 12.5, got {}",
            series[0].points[0].value,
        );
        assert_eq!(series[0].points[1].value, 0.0);
    }

    #[test]
    fn peak_replica_is_window_max_of_maximum_field_scaled() {
        // When the call requests Average,Maximum, the plotted points come from
        // `average` while `peak_replica` is the window peak of `maximum` — the
        // busiest single replica — scaled to the same unit (mCores).
        let payload = json!({
            "value": [
                {
                    "name": { "value": "UsageNanoCores", "localizedValue": "CPU" },
                    "unit": "NanoCores",
                    "timeseries": [
                        {
                            "data": [
                                { "timeStamp": "2026-01-01T00:00:00Z", "average": 40_000_000.0, "maximum": 60_000_000.0 },
                                { "timeStamp": "2026-01-01T00:15:00Z", "average": 50_000_000.0, "maximum": 240_000_000.0 }
                            ]
                        }
                    ]
                }
            ]
        });
        // The triple still carries the *primary* aggregation (Average) for
        // point selection; the Maximum is read opportunistically.
        let requested = vec![(
            MetricKind::Cpu,
            "UsageNanoCores".to_string(),
            "Average".to_string(),
        )];
        let series = parse_metrics_response(&payload, &requested, ResourceKind::ContainerApp);
        assert_eq!(series.len(), 1);
        // Plotted = average, scaled.
        assert!((series[0].points[1].value - 50.0).abs() < 1e-9);
        // Peak-replica = max(maximum) = 240_000_000 ns → 240 mCores.
        assert_eq!(series[0].peak_replica.map(|p| p.round()), Some(240.0));
    }

    #[test]
    fn peak_replica_is_none_without_maximum_field() {
        // A response with only `average` (no Maximum requested) leaves
        // peak_replica unset.
        let payload = json!({
            "value": [
                {
                    "name": { "value": "UsageNanoCores", "localizedValue": "CPU" },
                    "unit": "NanoCores",
                    "timeseries": [
                        { "data": [ { "timeStamp": "2026-01-01T00:00:00Z", "average": 10_000_000.0 } ] }
                    ]
                }
            ]
        });
        let requested = vec![(
            MetricKind::Cpu,
            "UsageNanoCores".to_string(),
            "Average".to_string(),
        )];
        let series = parse_metrics_response(&payload, &requested, ResourceKind::ContainerApp);
        assert_eq!(series[0].peak_replica, None);
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
