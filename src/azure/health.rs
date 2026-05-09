//! Derive a health badge from already-loaded metrics + Resource Health hint.
//!
//! When available, the Azure Resource Health signal (an authoritative
//! `Available`/`Degraded`/`Unavailable` from the platform) takes precedence
//! over the metric-derived heuristic. The heuristic stays as a fallback for
//! resources Resource Health flags as `Unknown`, and to refine `Available`
//! cases (a 4xx storm should still surface as `Degraded` even if the platform
//! is happy with us).

#![allow(dead_code, unused_variables)]

use serde::{Deserialize, Serialize};

use crate::azure::metrics::{MetricKind, MetricSeries};
use crate::azure::resource_health::AvailabilityState;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, Default)]
pub enum HealthStatus {
    Healthy,
    /// Resource is up and running, but there's been no traffic in the trailing
    /// window. Distinguishes a quiet-but-fine app from one we have no data on.
    Idle,
    Degraded,
    Critical,
    #[default]
    Unknown,
}

impl HealthStatus {
    pub fn label(&self) -> &'static str {
        match self {
            HealthStatus::Healthy => "HEALTHY",
            HealthStatus::Idle => "IDLE",
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

/// Outcome of running just the metric-ratio heuristic. Lets `derive` combine
/// the platform-reported availability with traffic-based fine tuning.
enum MetricVerdict {
    /// Errors+Traffic both present and traffic_sum > 0. Holds the resolved badge
    /// from the ratio (Healthy/Degraded/Critical).
    Traffic(HealthStatus),
    /// Errors+Traffic both present but traffic_sum == 0 in the trailing window.
    NoTraffic,
    /// Series missing or both empty — we genuinely don't know.
    NoData,
}

/// Compute health from loaded metrics, optional resource state, and the optional
/// Azure Resource Health signal.
///
/// Decision table (first match wins):
///
/// | state == Stopped             | → CRITICAL                            |
/// | availability == Unavailable  | → CRITICAL                            |
/// | availability == Degraded     | → DEGRADED                            |
/// | availability == Available    | metric ratio > 0% → ratio verdict     |
/// |                              | else state == Running → IDLE          |
/// |                              | else → HEALTHY                        |
/// | availability == Unknown/None | metric ratio > 0% → ratio verdict     |
/// |                              | else state == Running → IDLE          |
/// |                              | else → UNKNOWN                        |
pub fn derive(
    metrics: &[MetricSeries],
    state: Option<&str>,
    availability: Option<AvailabilityState>,
) -> HealthStatus {
    if matches!(state, Some(s) if s.eq_ignore_ascii_case("Stopped")) {
        return HealthStatus::Critical;
    }

    match availability {
        Some(AvailabilityState::Unavailable) => return HealthStatus::Critical,
        Some(AvailabilityState::Degraded) => return HealthStatus::Degraded,
        _ => {}
    }

    let verdict = metric_verdict(metrics);
    let running = matches!(state, Some(s) if s.eq_ignore_ascii_case("Running"));

    match availability {
        Some(AvailabilityState::Available) => match verdict {
            MetricVerdict::Traffic(status) => status,
            MetricVerdict::NoTraffic if running => HealthStatus::Idle,
            // Resource Health says we're up — trust it even with no metric data.
            _ => HealthStatus::Healthy,
        },
        // Unknown / None: fall back to the heuristic.
        _ => match verdict {
            MetricVerdict::Traffic(status) => status,
            MetricVerdict::NoTraffic if running => HealthStatus::Idle,
            _ => HealthStatus::Unknown,
        },
    }
}

/// Run the existing errors-vs-traffic ratio against the trailing window and
/// classify the result without consulting any other signals.
fn metric_verdict(metrics: &[MetricSeries]) -> MetricVerdict {
    let errors = find(metrics, MetricKind::Errors);
    let traffic = find(metrics, MetricKind::Traffic);

    let (errors, traffic) = match (errors, traffic) {
        (Some(e), Some(t)) => (e, t),
        _ => return MetricVerdict::NoData,
    };

    if errors.points.is_empty() && traffic.points.is_empty() {
        return MetricVerdict::NoData;
    }

    let traffic_sum = trailing_sum(traffic, LAST_HOUR_POINTS);
    if traffic_sum <= 0.0 {
        return MetricVerdict::NoTraffic;
    }

    let errors_sum = trailing_sum(errors, LAST_HOUR_POINTS);
    let ratio = errors_sum / traffic_sum;

    let status = if ratio > 0.05 {
        HealthStatus::Critical
    } else if ratio > 0.01 {
        HealthStatus::Degraded
    } else {
        HealthStatus::Healthy
    };
    MetricVerdict::Traffic(status)
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
        assert_eq!(
            derive(&metrics, Some("Stopped"), None),
            HealthStatus::Critical
        );
    }

    #[test]
    fn no_traffic_running_is_idle() {
        let metrics = vec![
            series(MetricKind::Errors, &[0.0, 0.0, 0.0, 0.0]),
            series(MetricKind::Traffic, &[0.0, 0.0, 0.0, 0.0]),
        ];
        assert_eq!(
            derive(&metrics, Some("Running"), None),
            HealthStatus::Idle
        );
    }

    #[test]
    fn missing_series_is_unknown() {
        let metrics = vec![series(MetricKind::Cpu, &[1.0, 2.0])];
        assert_eq!(derive(&metrics, None, None), HealthStatus::Unknown);
    }

    #[test]
    fn low_error_ratio_is_healthy() {
        // 1 error / 1000 requests = 0.1% → healthy.
        let metrics = vec![
            series(MetricKind::Errors, &[0.0, 0.0, 0.0, 1.0]),
            series(MetricKind::Traffic, &[250.0, 250.0, 250.0, 250.0]),
        ];
        assert_eq!(
            derive(&metrics, Some("Running"), None),
            HealthStatus::Healthy
        );
    }

    #[test]
    fn medium_error_ratio_is_degraded() {
        // 30 errors / 1000 requests = 3% → degraded.
        let metrics = vec![
            series(MetricKind::Errors, &[5.0, 10.0, 5.0, 10.0]),
            series(MetricKind::Traffic, &[250.0, 250.0, 250.0, 250.0]),
        ];
        assert_eq!(
            derive(&metrics, Some("Running"), None),
            HealthStatus::Degraded
        );
    }

    #[test]
    fn high_error_ratio_is_critical() {
        // 100 errors / 1000 requests = 10% → critical.
        let metrics = vec![
            series(MetricKind::Errors, &[25.0, 25.0, 25.0, 25.0]),
            series(MetricKind::Traffic, &[250.0, 250.0, 250.0, 250.0]),
        ];
        assert_eq!(
            derive(&metrics, Some("Running"), None),
            HealthStatus::Critical
        );
    }

    #[test]
    fn availability_unavailable_is_critical() {
        // No metrics, but platform says we're down.
        assert_eq!(
            derive(&[], Some("Running"), Some(AvailabilityState::Unavailable)),
            HealthStatus::Critical
        );
        // Even with happy metrics, Unavailable wins.
        let metrics = vec![
            series(MetricKind::Errors, &[0.0, 0.0, 0.0, 0.0]),
            series(MetricKind::Traffic, &[100.0, 100.0, 100.0, 100.0]),
        ];
        assert_eq!(
            derive(&metrics, Some("Running"), Some(AvailabilityState::Unavailable)),
            HealthStatus::Critical
        );
    }

    #[test]
    fn availability_degraded_is_degraded() {
        assert_eq!(
            derive(&[], Some("Running"), Some(AvailabilityState::Degraded)),
            HealthStatus::Degraded
        );
    }

    #[test]
    fn availability_available_with_no_traffic_running_is_idle() {
        let metrics = vec![
            series(MetricKind::Errors, &[0.0, 0.0, 0.0, 0.0]),
            series(MetricKind::Traffic, &[0.0, 0.0, 0.0, 0.0]),
        ];
        assert_eq!(
            derive(
                &metrics,
                Some("Running"),
                Some(AvailabilityState::Available)
            ),
            HealthStatus::Idle
        );
    }

    #[test]
    fn availability_available_with_traffic_uses_metric_ratio() {
        // Healthy ratio.
        let healthy = vec![
            series(MetricKind::Errors, &[0.0, 0.0, 0.0, 1.0]),
            series(MetricKind::Traffic, &[250.0, 250.0, 250.0, 250.0]),
        ];
        assert_eq!(
            derive(
                &healthy,
                Some("Running"),
                Some(AvailabilityState::Available)
            ),
            HealthStatus::Healthy
        );

        // Degraded ratio still surfaces even though Resource Health is happy.
        let degraded = vec![
            series(MetricKind::Errors, &[5.0, 10.0, 5.0, 10.0]),
            series(MetricKind::Traffic, &[250.0, 250.0, 250.0, 250.0]),
        ];
        assert_eq!(
            derive(
                &degraded,
                Some("Running"),
                Some(AvailabilityState::Available)
            ),
            HealthStatus::Degraded
        );

        // Critical ratio also wins over Available.
        let critical = vec![
            series(MetricKind::Errors, &[25.0, 25.0, 25.0, 25.0]),
            series(MetricKind::Traffic, &[250.0, 250.0, 250.0, 250.0]),
        ];
        assert_eq!(
            derive(
                &critical,
                Some("Running"),
                Some(AvailabilityState::Available)
            ),
            HealthStatus::Critical
        );
    }

    #[test]
    fn availability_available_with_no_metric_data_is_healthy() {
        // Resource Health says we're up; we trust it even without any metrics.
        assert_eq!(
            derive(&[], Some("Running"), Some(AvailabilityState::Available)),
            HealthStatus::Healthy
        );
    }

    #[test]
    fn unknown_availability_running_no_traffic_is_idle() {
        let metrics = vec![
            series(MetricKind::Errors, &[0.0, 0.0, 0.0, 0.0]),
            series(MetricKind::Traffic, &[0.0, 0.0, 0.0, 0.0]),
        ];
        assert_eq!(
            derive(
                &metrics,
                Some("Running"),
                Some(AvailabilityState::Unknown)
            ),
            HealthStatus::Idle
        );
    }
}
