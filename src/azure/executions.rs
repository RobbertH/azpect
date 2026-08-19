//! Function-App execution counts from App Insights `AppRequests`.
//!
//! The `Requests` platform metric on `Microsoft.Web/sites` counts HTTP hits on
//! the site's front end — for an event-triggered Function App that is almost
//! entirely Always On keep-alive pings and platform probes, while the actual
//! blob/queue/timer/orchestration invocations never appear (the host polls
//! storage internally, no HTTP involved). App Insights, by contrast, logs one
//! `AppRequests` row per function *invocation* regardless of trigger type, so
//! a per-bin count over that table is the real "how busy is this app" series.
//!
//! The `FunctionExecutionCount` Monitor metric is a weaker alternative: it is
//! documented for Consumption (billing) and emitted inconsistently on other
//! plans, and it has no per-invocation detail behind it. `AppRequests` aligns
//! with the logs view and lets a user drill from a spike to the actual rows.
//!
//! Uses the same resource-centric Log Analytics endpoint as the logs view
//! (`azure::logs`), but scoped to the app's **App Insights component** (from
//! the `hidden-link: /app-insights-resource-id` tag) rather than the site:
//! workspace-based AI stamps `_ResourceId` on its rows with the component, so
//! a site-scoped query can resolve zero `AppRequests` rows even when telemetry
//! exists. Falls back to the site scope when the tag is absent; an app with no
//! (workspace-based) App Insights at all fails table resolution server-side
//! and the caller degrades the row to `not available`.

#![allow(dead_code, unused_variables)]

use anyhow::anyhow;
use chrono::{DateTime, Duration, DurationRound, Utc};
use std::collections::HashMap;

use crate::azure::auth::AzureAuth;
use crate::azure::client::LogsClient;
use crate::azure::metrics::{MetricKind, MetricPoint, MetricSeries, TimeRange};
use crate::azure::resources::Resource;

/// KQL bin size matching the Monitor `interval` for each range, so the
/// Executions sparkline lines up bar-for-bar with the Requests row above it.
fn bin(range: TimeRange) -> &'static str {
    match range {
        TimeRange::Hour => "1m",
        TimeRange::Day => "15m",
        TimeRange::Week => "1h",
    }
}

fn bin_duration(range: TimeRange) -> Duration {
    match range {
        TimeRange::Hour => Duration::minutes(1),
        TimeRange::Day => Duration::minutes(15),
        TimeRange::Week => Duration::hours(1),
    }
}

/// `sum(ItemCount)`, not `count()`: App Insights adaptive sampling stores one
/// row per N sampled invocations with `ItemCount` carrying the true count, so
/// a plain row count under-reports on busy apps.
fn kql(range: TimeRange) -> String {
    format!(
        "AppRequests\n\
         | summarize value = sum(ItemCount) by ts = bin(TimeGenerated, {bin})\n\
         | order by ts asc",
        bin = bin(range),
    )
}

/// Fetch the executions-per-bin series for a Function App over `range`,
/// scoped to its linked App Insights component when the `hidden-link` tag
/// names one (see the module docs for why the site scope is not enough).
///
/// Errors bubble up verbatim (auth, network, or KQL table resolution — the
/// latter is what a no-App-Insights app produces) so the caller can file them
/// in the per-metric `missing` map.
pub async fn fetch(
    auth: &AzureAuth,
    resource: &Resource,
    range: TimeRange,
) -> anyhow::Result<MetricSeries> {
    let scope = resource
        .meta
        .app_insights_resource_id()
        .unwrap_or(&resource.id);
    let client = LogsClient::new(auth.clone())?;
    let resp = client.query(scope, &kql(range), &range.timespan()).await?;
    parse_response(&resp, range, Utc::now())
}

/// Parse the Log Analytics response into a dense series: `summarize … by bin()`
/// only returns bins that had rows, but the sparkline resampler assumes evenly
/// spaced points, so quiet bins are filled with explicit zeros over the whole
/// window. Bin boundaries are epoch-aligned exactly like KQL's `bin()`, so
/// returned timestamps land on our grid.
fn parse_response(
    value: &serde_json::Value,
    range: TimeRange,
    end: DateTime<Utc>,
) -> anyhow::Result<MetricSeries> {
    let table = value
        .get("tables")
        .and_then(|t| t.as_array())
        .and_then(|a| a.first())
        .ok_or_else(|| anyhow!("executions response missing 'tables'"))?;

    let columns = table
        .get("columns")
        .and_then(|c| c.as_array())
        .ok_or_else(|| anyhow!("executions response table missing 'columns'"))?;
    let col_idx = |name: &str| {
        columns
            .iter()
            .position(|c| c.get("name").and_then(|n| n.as_str()) == Some(name))
    };
    let ts_idx = col_idx("ts").ok_or_else(|| anyhow!("executions response missing 'ts' column"))?;
    let value_idx =
        col_idx("value").ok_or_else(|| anyhow!("executions response missing 'value' column"))?;

    let rows = table
        .get("rows")
        .and_then(|r| r.as_array())
        .ok_or_else(|| anyhow!("executions response table missing 'rows'"))?;

    let mut by_bin: HashMap<DateTime<Utc>, f64> = HashMap::with_capacity(rows.len());
    for row in rows {
        let Some(cells) = row.as_array() else {
            continue;
        };
        let ts = cells
            .get(ts_idx)
            .and_then(|v| v.as_str())
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));
        // `sum()` over a long column comes back as a JSON number; a bin whose
        // every row was sampled away can surface a null — treat it as 0.
        let v = cells.get(value_idx).and_then(|v| v.as_f64()).unwrap_or(0.0);
        if let Some(ts) = ts {
            by_bin.insert(ts, v);
        }
    }

    let step = bin_duration(range);
    let start = (end - range.duration())
        .duration_trunc(step)
        .unwrap_or(end - range.duration());
    let mut points = Vec::new();
    let mut t = start;
    while t <= end {
        points.push(MetricPoint {
            ts: t,
            value: by_bin.get(&t).copied().unwrap_or(0.0),
        });
        t += step;
    }

    Ok(MetricSeries {
        kind: MetricKind::Executions,
        label: "Executions".to_string(),
        unit: "count".to_string(),
        points,
        peak_replica: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn response(rows: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "tables": [{
                "name": "PrimaryResult",
                "columns": [
                    { "name": "ts", "type": "datetime" },
                    { "name": "value", "type": "long" },
                ],
                "rows": rows,
            }]
        })
    }

    #[test]
    fn parse_fills_quiet_bins_with_zeros() {
        // 7d / 1h bins: two busy hours in a week, everything else zero.
        let end = Utc.with_ymd_and_hms(2026, 8, 18, 12, 30, 0).unwrap();
        let resp = response(serde_json::json!([
            ["2026-08-15T09:00:00Z", 340],
            ["2026-08-17T14:00:00Z", 12],
        ]));
        let s = parse_response(&resp, TimeRange::Week, end).unwrap();
        assert_eq!(s.kind, MetricKind::Executions);
        // Window start 2026-08-11T12:30 truncated to the hour, hourly steps up
        // to and including 12:00 on the 18th → 169 bins.
        assert_eq!(s.points.len(), 169);
        assert_eq!(s.sum(), 352.0);
        assert!(s
            .points
            .first()
            .unwrap()
            .ts
            .to_rfc3339()
            .starts_with("2026-08-11T12:00"));
        let busy: Vec<_> = s.points.iter().filter(|p| p.value > 0.0).collect();
        assert_eq!(busy.len(), 2);
        assert_eq!(busy[0].value, 340.0);
        assert!(busy[0].ts.to_rfc3339().starts_with("2026-08-15T09:00"));
    }

    #[test]
    fn parse_empty_rows_is_an_all_zero_series_not_an_error() {
        // The table resolved (App Insights is wired up) but nothing ran — a
        // legitimate all-zero week, distinct from the no-table error case.
        let end = Utc.with_ymd_and_hms(2026, 8, 18, 12, 0, 0).unwrap();
        let resp = response(serde_json::json!([]));
        let s = parse_response(&resp, TimeRange::Day, end).unwrap();
        assert!(!s.points.is_empty());
        assert_eq!(s.sum(), 0.0);
    }

    #[test]
    fn parse_missing_tables_errors() {
        let resp = serde_json::json!({ "tables": [] });
        let end = Utc::now();
        assert!(parse_response(&resp, TimeRange::Hour, end).is_err());
    }

    #[test]
    fn parse_null_bin_value_collapses_to_zero() {
        let end = Utc.with_ymd_and_hms(2026, 8, 18, 12, 0, 0).unwrap();
        let resp = response(serde_json::json!([["2026-08-18T11:00:00Z", null]]));
        let s = parse_response(&resp, TimeRange::Day, end).unwrap();
        assert_eq!(s.sum(), 0.0);
    }

    #[test]
    fn kql_bins_match_monitor_intervals() {
        assert!(kql(TimeRange::Hour).contains("bin(TimeGenerated, 1m)"));
        assert!(kql(TimeRange::Day).contains("bin(TimeGenerated, 15m)"));
        assert!(kql(TimeRange::Week).contains("bin(TimeGenerated, 1h)"));
        assert!(kql(TimeRange::Week).contains("sum(ItemCount)"));
    }
}
