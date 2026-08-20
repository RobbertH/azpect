//! Container Registry pull/push activity from Azure Monitor platform metrics.
//!
//! `TotalPullCount` / `TotalPushCount` are *platform* metrics: every registry
//! emits them with no diagnostic setting, no workspace, no opt-in. That makes
//! them the perfect counterpart to the `ContainerRegistryRepositoryEvents`
//! access log (see [`crate::azure::registry_logs`]), which IS opt-in and is
//! silently empty on a registry nobody configured: metric bars next to an
//! empty event table prove that pulls are happening but not being recorded.
//! The trade-off is granularity — Monitor counts carry no identity or
//! repository dimension, so they can never say *who* or *which image*, only
//! *when* and *how many* (registry-wide, even when the access log is scoped
//! to one repository).
//!
//! Metric retention is ~93 days; windows reaching further back simply show
//! zeros for the older bins.

#![allow(dead_code, unused_variables)]

use anyhow::anyhow;
use chrono::{DateTime, Utc};

use crate::azure::auth::AzureAuth;
use crate::azure::client::ArmClient;
use crate::azure::key_vault_logs::AccessWindow;
use crate::azure::metrics::{MetricKind, MetricPoint, MetricSeries};
use crate::azure::registries::Registry;

/// Pull and push counts per bin over one [`AccessWindow`]. Either series can
/// be all-zero (a quiet registry) but both are always present — Monitor
/// returns every bin in the timespan, including empty ones.
#[derive(Clone, Debug)]
pub struct RegistryActivity {
    pub pulls: MetricSeries,
    pub pushes: MetricSeries,
}

impl RegistryActivity {
    /// Total pulls over the window.
    pub fn pull_total(&self) -> f64 {
        self.pulls.sum()
    }

    /// Total pushes over the window.
    pub fn push_total(&self) -> f64 {
        self.pushes.sum()
    }

    /// Whether the window saw any activity at all — the signal the access-log
    /// empty state uses to call out "pulls happen but aren't being logged".
    pub fn any_activity(&self) -> bool {
        self.pull_total() > 0.0 || self.push_total() > 0.0
    }
}

/// Monitor `interval` for an arbitrary window length: the finest grain that
/// keeps the bin count in sparkline territory (~60–370 bins) and inside
/// Monitor's per-request point limits. The access window is user-typed and
/// unbounded ("6m", "1y"), unlike the fixed `TimeRange` grains elsewhere.
pub fn interval_for_hours(hours: i64) -> &'static str {
    match hours {
        i64::MIN..=2 => "PT1M",
        3..=12 => "PT5M",
        13..=48 => "PT15M",
        49..=168 => "PT1H",     // ≤7d  → ≤168 bins
        169..=1440 => "PT6H",   // ≤60d → ≤240 bins
        1441..=2880 => "PT12H", // ≤120d → ≤240 bins
        _ => "P1D",
    }
}

/// The chosen interval's bin length in minutes — used by the demo generator
/// so synthetic series land on the same grid as real ones.
pub fn bin_minutes_for_hours(hours: i64) -> i64 {
    match interval_for_hours(hours) {
        "PT1M" => 1,
        "PT5M" => 5,
        "PT15M" => 15,
        "PT1H" => 60,
        "PT6H" => 360,
        "PT12H" => 720,
        _ => 1440,
    }
}

/// Fetch the registry's pull/push counts over `window`. One Monitor call for
/// both metric names — unlike the per-plan Web-site metrics, these two exist
/// on every ACR SKU, so a batched call can't lose one to a per-name 400.
pub async fn fetch(
    auth: &AzureAuth,
    registry: &Registry,
    window: &AccessWindow,
) -> anyhow::Result<RegistryActivity> {
    let client = ArmClient::new(auth.clone())?;
    let path = format!(
        "{}/providers/Microsoft.Insights/metrics",
        registry.id.trim_end_matches('/')
    );
    let timespan = window.timespan();
    let interval = interval_for_hours(window.duration().num_hours().max(1));
    let params: Vec<(&str, &str)> = vec![
        ("api-version", "2023-10-01"),
        ("timespan", &timespan),
        ("interval", interval),
        ("metricnames", "TotalPullCount,TotalPushCount"),
        ("aggregation", "Total"),
    ];
    let value = client.get(&path, &params).await?;
    parse_response(&value)
}

/// Parse the two-metric Monitor response. A metric entry that's missing or
/// has no timeseries degrades to an empty series rather than an error — the
/// chart renders a flat row and the totals read 0.
fn parse_response(value: &serde_json::Value) -> anyhow::Result<RegistryActivity> {
    let metrics = value
        .get("value")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("metrics response missing 'value'"))?;

    let points_for = |physical: &str| -> Vec<MetricPoint> {
        metrics
            .iter()
            .find(|m| {
                m.get("name")
                    .and_then(|n| n.get("value"))
                    .and_then(|n| n.as_str())
                    == Some(physical)
            })
            .and_then(|m| m.get("timeseries"))
            .and_then(|t| t.as_array())
            .and_then(|a| a.first())
            .and_then(|ts| ts.get("data"))
            .and_then(|d| d.as_array())
            .map(|data| {
                data.iter()
                    .filter_map(|d| {
                        let ts = d
                            .get("timeStamp")
                            .and_then(|t| t.as_str())
                            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())?
                            .with_timezone(&Utc);
                        // Quiet bins come back without a `total` field.
                        let v = d.get("total").and_then(|x| x.as_f64()).unwrap_or(0.0);
                        Some(MetricPoint { ts, value: v })
                    })
                    .collect()
            })
            .unwrap_or_default()
    };

    // `MetricKind` reuse, not new variants: the kind's only job downstream is
    // the sparkline colour (`color_for_metric`), and Traffic (accent) /
    // Executions (green) are exactly the two hues the access-log chart wants.
    Ok(RegistryActivity {
        pulls: MetricSeries {
            kind: MetricKind::Traffic,
            label: "Pulls".to_string(),
            unit: "count".to_string(),
            points: points_for("TotalPullCount"),
            peak_replica: None,
        },
        pushes: MetricSeries {
            kind: MetricKind::Executions,
            label: "Pushes".to_string(),
            unit: "count".to_string(),
            points: points_for("TotalPushCount"),
            peak_replica: None,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn interval_scales_with_window_length() {
        assert_eq!(interval_for_hours(1), "PT1M");
        assert_eq!(interval_for_hours(24), "PT15M");
        assert_eq!(interval_for_hours(24 * 7), "PT1H");
        assert_eq!(interval_for_hours(24 * 30), "PT6H");
        assert_eq!(interval_for_hours(24 * 120), "PT12H");
        // 1y at P1D = 365 bins, safely under Monitor's point limits.
        assert_eq!(interval_for_hours(24 * 365), "P1D");
        assert_eq!(bin_minutes_for_hours(24), 15);
        assert_eq!(bin_minutes_for_hours(24 * 365), 1440);
    }

    #[test]
    fn parse_maps_both_metrics_regardless_of_order() {
        let payload = json!({
            "value": [
                {
                    "name": { "value": "TotalPushCount", "localizedValue": "Total Push Count" },
                    "unit": "Count",
                    "timeseries": [{ "data": [
                        { "timeStamp": "2026-08-20T10:00:00Z", "total": 2.0 },
                    ]}]
                },
                {
                    "name": { "value": "TotalPullCount", "localizedValue": "Total Pull Count" },
                    "unit": "Count",
                    "timeseries": [{ "data": [
                        { "timeStamp": "2026-08-20T10:00:00Z", "total": 41.0 },
                        { "timeStamp": "2026-08-20T10:15:00Z" },
                    ]}]
                }
            ]
        });
        let activity = parse_response(&payload).unwrap();
        assert_eq!(activity.pulls.label, "Pulls");
        assert_eq!(activity.pull_total(), 41.0);
        // The `total`-less quiet bin parses as an explicit zero point.
        assert_eq!(activity.pulls.points.len(), 2);
        assert_eq!(activity.pulls.points[1].value, 0.0);
        assert_eq!(activity.push_total(), 2.0);
        assert!(activity.any_activity());
    }

    #[test]
    fn parse_missing_metric_degrades_to_empty_series() {
        let payload = json!({ "value": [] });
        let activity = parse_response(&payload).unwrap();
        assert!(activity.pulls.points.is_empty());
        assert!(activity.pushes.points.is_empty());
        assert!(!activity.any_activity());
    }

    #[test]
    fn parse_missing_value_errors() {
        assert!(parse_response(&json!({})).is_err());
    }
}
