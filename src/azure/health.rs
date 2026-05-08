//! Derive a health badge from already-loaded metrics. No additional API calls.

#![allow(dead_code, unused_variables)]

use serde::{Deserialize, Serialize};

use crate::azure::metrics::{MetricKind, MetricSeries};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, Default)]
pub enum HealthStatus {
    Healthy,
    Degraded,
    Critical,
    #[default]
    Unknown,
}

impl HealthStatus {
    pub fn label(&self) -> &'static str {
        match self {
            HealthStatus::Healthy => "HEALTHY",
            HealthStatus::Degraded => "DEGRADED",
            HealthStatus::Critical => "CRITICAL",
            HealthStatus::Unknown => "UNKNOWN",
        }
    }
}

/// How many trailing points constitute "the last hour" for v1.
/// PT15M -> 4 points = 1h. PT1H -> 1 point = 1h, but we still take up to 4 to
/// stay in line with the spec.
const LAST_HOUR_POINTS: usize = 4;

/// Compute health from the loaded metrics + optional resource state.
///
/// Rules:
///
/// - `state == "Stopped"` → CRITICAL
/// - 5xx / requests > 5% (over the last hour of the window) → CRITICAL
/// - 5xx / requests in 1–5% → DEGRADED
/// - 5xx / requests < 1% and resource running → HEALTHY
/// - no traffic in window or metric fetch failed → UNKNOWN
pub fn derive(metrics: &[MetricSeries], state: Option<&str>) -> HealthStatus {
    if matches!(state, Some(s) if s.eq_ignore_ascii_case("Stopped")) {
        return HealthStatus::Critical;
    }

    let errors = find(metrics, MetricKind::Errors);
    let traffic = find(metrics, MetricKind::Traffic);

    let (errors, traffic) = match (errors, traffic) {
        (Some(e), Some(t)) => (e, t),
        _ => return HealthStatus::Unknown,
    };

    if errors.points.is_empty() && traffic.points.is_empty() {
        return HealthStatus::Unknown;
    }

    let traffic_sum = trailing_sum(traffic, LAST_HOUR_POINTS);
    if traffic_sum <= 0.0 {
        return HealthStatus::Unknown;
    }

    let errors_sum = trailing_sum(errors, LAST_HOUR_POINTS);
    let ratio = errors_sum / traffic_sum;

    if ratio > 0.05 {
        HealthStatus::Critical
    } else if ratio > 0.01 {
        HealthStatus::Degraded
    } else {
        HealthStatus::Healthy
    }
}

fn trailing_sum(series: &MetricSeries, n: usize) -> f64 {
    let len = series.points.len();
    let start = len.saturating_sub(n);
    series.points[start..].iter().map(|p| p.value).sum()
}

/// Find a series by kind. Convenience for callers.
pub fn find(metrics: &[MetricSeries], kind: MetricKind) -> Option<&MetricSeries> {
    metrics.iter().find(|m| m.kind == kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::metrics::{MetricPoint, MetricSeries};
    use chrono::{Duration, Utc};

    fn series(kind: MetricKind, values: &[f64]) -> MetricSeries {
        let now = Utc::now();
        let points = values
            .iter()
            .enumerate()
            .map(|(i, v)| MetricPoint {
                ts: now - Duration::minutes(15 * (values.len() - i) as i64),
                value: *v,
            })
            .collect();
        MetricSeries {
            kind,
            label: String::new(),
            unit: String::new(),
            points,
        }
    }

    #[test]
    fn stopped_state_is_critical() {
        let metrics = vec![
            series(MetricKind::Errors, &[0.0]),
            series(MetricKind::Traffic, &[100.0]),
        ];
        assert_eq!(derive(&metrics, Some("Stopped")), HealthStatus::Critical);
    }

    #[test]
    fn no_traffic_is_unknown() {
        let metrics = vec![
            series(MetricKind::Errors, &[0.0, 0.0, 0.0, 0.0]),
            series(MetricKind::Traffic, &[0.0, 0.0, 0.0, 0.0]),
        ];
        assert_eq!(derive(&metrics, Some("Running")), HealthStatus::Unknown);
    }

    #[test]
    fn missing_series_is_unknown() {
        let metrics = vec![series(MetricKind::Cpu, &[1.0, 2.0])];
        assert_eq!(derive(&metrics, None), HealthStatus::Unknown);
    }

    #[test]
    fn low_error_ratio_is_healthy() {
        // 1 error / 1000 requests = 0.1% → healthy.
        let metrics = vec![
            series(MetricKind::Errors, &[0.0, 0.0, 0.0, 1.0]),
            series(MetricKind::Traffic, &[250.0, 250.0, 250.0, 250.0]),
        ];
        assert_eq!(derive(&metrics, Some("Running")), HealthStatus::Healthy);
    }

    #[test]
    fn medium_error_ratio_is_degraded() {
        // 30 errors / 1000 requests = 3% → degraded.
        let metrics = vec![
            series(MetricKind::Errors, &[5.0, 10.0, 5.0, 10.0]),
            series(MetricKind::Traffic, &[250.0, 250.0, 250.0, 250.0]),
        ];
        assert_eq!(derive(&metrics, Some("Running")), HealthStatus::Degraded);
    }

    #[test]
    fn high_error_ratio_is_critical() {
        // 100 errors / 1000 requests = 10% → critical.
        let metrics = vec![
            series(MetricKind::Errors, &[25.0, 25.0, 25.0, 25.0]),
            series(MetricKind::Traffic, &[250.0, 250.0, 250.0, 250.0]),
        ];
        assert_eq!(derive(&metrics, Some("Running")), HealthStatus::Critical);
    }
}
