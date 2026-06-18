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

// Thresholds for the error-to-traffic ratio. These are deliberately
// pessimistic: any single signal crossing a line drags the whole verdict down.

/// Sustained ratio over the *whole* 24h window: an app that's quietly erroring
/// 1–5% of the time is degraded; >5% is critical.
const SUSTAINED_DEGRADED: f64 = 0.01;
const SUSTAINED_CRITICAL: f64 = 0.05;

/// Per-bin spike ratio. A single 15-minute bin that's >10% errors is a spike
/// worth surfacing even if the daily average looks fine; >30% is critical. The
/// daily average can hide a sharp outage — the spike check catches it.
const SPIKE_DEGRADED: f64 = 0.10;
const SPIKE_CRITICAL: f64 = 0.30;

/// Ignore bins with fewer requests than this when looking for spikes, so a
/// "2 errors out of 3 requests" blip in a near-idle bin doesn't read as a 66%
/// spike. Tuned for the 15-minute bins of the fixed 24h health window.
const SPIKE_MIN_REQUESTS: f64 = 20.0;

/// Outcome of the errors-vs-traffic heuristic over the whole window. Lets
/// `derive` combine the platform-reported availability with traffic-based
/// signals (worst-of).
enum MetricVerdict {
    /// Traffic present; holds the worst badge across the sustained ratio and the
    /// per-bin spike check (Healthy/Degraded/Critical).
    Traffic(HealthStatus),
    /// Errors+Traffic both present but no traffic in the 24h window.
    NoTraffic,
    /// Series missing or both empty — we genuinely don't know.
    NoData,
}

/// Compute a health badge from the fixed-24h metrics window, optional resource
/// state, and the optional Azure Resource Health signal.
///
/// `metrics` are the Errors + Traffic series over a fixed 24h window (see
/// `metrics::fetch_health`) — *not* the chart's selected range, so the verdict
/// is stable regardless of what the user is looking at.
///
/// The combine rule is **worst-of**: state, platform availability, the
/// sustained error ratio, and per-bin spikes each contribute a severity, and
/// the most severe one wins. A clean platform signal never upgrades a metric
/// problem away.
///
/// | state == Stopped            | → CRITICAL                                |
/// | availability == Unavailable | → CRITICAL                                |
/// | any signal CRITICAL         | → CRITICAL                                |
/// | availability == Degraded    | → DEGRADED (unless a signal is CRITICAL)  |
/// | any signal DEGRADED         | → DEGRADED                                |
/// | otherwise, traffic present  | → HEALTHY                                 |
/// | otherwise, no traffic       | running/available → IDLE, else UNKNOWN    |
/// | otherwise, no data          | available → HEALTHY, else UNKNOWN         |
pub fn derive(
    metrics: &[MetricSeries],
    state: Option<&str>,
    availability: Option<AvailabilityState>,
) -> HealthStatus {
    if matches!(state, Some(s) if s.eq_ignore_ascii_case("Stopped")) {
        return HealthStatus::Critical;
    }
    if availability == Some(AvailabilityState::Unavailable) {
        return HealthStatus::Critical;
    }

    let verdict = metric_verdict(metrics);

    // Worst-of: a critical metric signal beats a merely-degraded platform, and
    // a degraded platform beats clean metrics.
    if matches!(verdict, MetricVerdict::Traffic(HealthStatus::Critical)) {
        return HealthStatus::Critical;
    }
    if availability == Some(AvailabilityState::Degraded)
        || matches!(verdict, MetricVerdict::Traffic(HealthStatus::Degraded))
    {
        return HealthStatus::Degraded;
    }

    let running = matches!(state, Some(s) if s.eq_ignore_ascii_case("Running"));
    let available = availability == Some(AvailabilityState::Available);

    match verdict {
        // Already handled the Degraded/Critical cases above.
        MetricVerdict::Traffic(_) => HealthStatus::Healthy,
        // Up but quiet: distinguish a fine-but-idle app from one we can't read.
        MetricVerdict::NoTraffic if running || available => HealthStatus::Idle,
        MetricVerdict::NoTraffic => HealthStatus::Unknown,
        // No metric series at all: trust the platform if it says we're up,
        // otherwise we genuinely don't know (don't claim IDLE without data).
        MetricVerdict::NoData if available => HealthStatus::Healthy,
        MetricVerdict::NoData => HealthStatus::Unknown,
    }
}

/// Classify the Errors-vs-Traffic series over the whole window: the worst of the
/// sustained (windowed) ratio and any single-bin spike.
fn metric_verdict(metrics: &[MetricSeries]) -> MetricVerdict {
    let (errors, traffic) = match (
        find(metrics, MetricKind::Errors),
        find(metrics, MetricKind::Traffic),
    ) {
        (Some(e), Some(t)) => (e, t),
        _ => return MetricVerdict::NoData,
    };

    if errors.points.is_empty() && traffic.points.is_empty() {
        return MetricVerdict::NoData;
    }

    let traffic_sum: f64 = traffic.points.iter().map(|p| p.value).sum();
    if traffic_sum <= 0.0 {
        return MetricVerdict::NoTraffic;
    }
    let errors_sum: f64 = errors.points.iter().map(|p| p.value).sum();

    let sustained = ratio_status(
        errors_sum / traffic_sum,
        SUSTAINED_DEGRADED,
        SUSTAINED_CRITICAL,
    );
    let spike = worst_bin_status(errors, traffic);
    MetricVerdict::Traffic(worse(sustained, spike))
}

/// Worst per-bin error ratio across the window, ignoring near-idle bins so a
/// tiny-denominator blip doesn't masquerade as a spike. Bins are paired by
/// timestamp (the two series share the same query window/grain, but pairing by
/// `ts` is robust to any misalignment).
fn worst_bin_status(errors: &MetricSeries, traffic: &MetricSeries) -> HealthStatus {
    let errors_at: std::collections::HashMap<_, f64> =
        errors.points.iter().map(|p| (p.ts, p.value)).collect();
    let mut worst = HealthStatus::Healthy;
    for t in &traffic.points {
        if t.value < SPIKE_MIN_REQUESTS {
            continue;
        }
        let e = errors_at.get(&t.ts).copied().unwrap_or(0.0);
        worst = worse(
            worst,
            ratio_status(e / t.value, SPIKE_DEGRADED, SPIKE_CRITICAL),
        );
    }
    worst
}

/// Map an error ratio to a badge given the degraded/critical thresholds.
fn ratio_status(ratio: f64, degraded: f64, critical: f64) -> HealthStatus {
    if ratio > critical {
        HealthStatus::Critical
    } else if ratio > degraded {
        HealthStatus::Degraded
    } else {
        HealthStatus::Healthy
    }
}

/// Return the more severe of two badges. Only meaningful for the metric-derived
/// trio (Healthy < Degraded < Critical); other variants rank as 0.
fn worse(a: HealthStatus, b: HealthStatus) -> HealthStatus {
    fn rank(s: HealthStatus) -> u8 {
        match s {
            HealthStatus::Critical => 3,
            HealthStatus::Degraded => 2,
            HealthStatus::Healthy => 1,
            _ => 0,
        }
    }
    if rank(a) >= rank(b) {
        a
    } else {
        b
    }
}

/// Find a series by kind. Convenience for callers.
pub fn find(metrics: &[MetricSeries], kind: MetricKind) -> Option<&MetricSeries> {
    metrics.iter().find(|m| m.kind == kind)
}

/// Total 5xx errors across the health window (0.0 if there's no Errors series).
///
/// This is a *presence* signal, deliberately separate from the [`derive`]
/// verdict: an app can sit comfortably under the error-ratio thresholds (so the
/// badge is HEALTHY) while still throwing 500s worth eyeballing. Callers surface
/// it as a flag next to the badge rather than folding it into the severity.
pub fn errors_total(metrics: &[MetricSeries]) -> f64 {
    find(metrics, MetricKind::Errors)
        .map(|s| s.points.iter().map(|p| p.value).sum())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::metrics::{MetricPoint, MetricSeries};
    use chrono::{Duration, Utc};

    fn series(kind: MetricKind, values: &[f64]) -> MetricSeries {
        series_at(kind, values, Utc::now())
    }

    fn series_at(kind: MetricKind, values: &[f64], now: chrono::DateTime<Utc>) -> MetricSeries {
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
            peak_replica: None,
        }
    }

    /// Errors + Traffic series stamped with the *same* bin timestamps, so the
    /// per-bin spike check (which pairs by `ts`) lines them up — mirrors how
    /// Azure returns aligned bins for the two metrics of one query.
    fn aligned(errors: &[f64], traffic: &[f64]) -> Vec<MetricSeries> {
        let now = Utc::now();
        vec![
            series_at(MetricKind::Errors, errors, now),
            series_at(MetricKind::Traffic, traffic, now),
        ]
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
        assert_eq!(derive(&metrics, Some("Running"), None), HealthStatus::Idle);
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
            derive(
                &metrics,
                Some("Running"),
                Some(AvailabilityState::Unavailable)
            ),
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
            derive(&metrics, Some("Running"), Some(AvailabilityState::Unknown)),
            HealthStatus::Idle
        );
    }

    #[test]
    fn single_bin_spike_degrades_despite_healthy_average() {
        // 15 errors in the last bin against 100 requests = 15% for that bin,
        // but the daily average is 15 / 15_100 ≈ 0.1% — well under the sustained
        // threshold. The spike check must still surface it. This is the
        // rnd3-context-api-tst case: spiky 5xx the windowed ratio would hide.
        let metrics = aligned(&[0.0, 0.0, 0.0, 15.0], &[5000.0, 5000.0, 5000.0, 100.0]);
        assert_eq!(
            derive(
                &metrics,
                Some("Running"),
                Some(AvailabilityState::Available)
            ),
            HealthStatus::Degraded
        );
    }

    #[test]
    fn severe_single_bin_spike_is_critical() {
        // 50 / 100 = 50% in one bin → critical, even though Resource Health is
        // happy and the daily average is negligible.
        let metrics = aligned(&[0.0, 0.0, 0.0, 50.0], &[5000.0, 5000.0, 5000.0, 100.0]);
        assert_eq!(
            derive(
                &metrics,
                Some("Running"),
                Some(AvailabilityState::Available)
            ),
            HealthStatus::Critical
        );
    }

    #[test]
    fn tiny_bin_blip_is_not_a_spike() {
        // 2 errors out of 3 requests is 66%, but 3 requests is below the
        // min-requests guard, so it must not register as a spike.
        let metrics = aligned(&[0.0, 0.0, 0.0, 2.0], &[5000.0, 5000.0, 5000.0, 3.0]);
        assert_eq!(
            derive(&metrics, Some("Running"), None),
            HealthStatus::Healthy
        );
    }

    #[test]
    fn errors_total_sums_the_5xx_series() {
        let metrics = aligned(&[1.0, 0.0, 4.0, 8.0], &[100.0, 100.0, 100.0, 100.0]);
        assert_eq!(errors_total(&metrics), 13.0);
        // No Errors series → 0, not a panic.
        assert_eq!(errors_total(&[series(MetricKind::Cpu, &[5.0])]), 0.0);
    }

    #[test]
    fn platform_degraded_loses_to_critical_metrics() {
        // Worst-of: a critical error ratio outranks a merely-degraded platform.
        let metrics = vec![
            series(MetricKind::Errors, &[25.0, 25.0, 25.0, 25.0]),
            series(MetricKind::Traffic, &[250.0, 250.0, 250.0, 250.0]),
        ];
        assert_eq!(
            derive(&metrics, Some("Running"), Some(AvailabilityState::Degraded)),
            HealthStatus::Critical
        );
    }
}
