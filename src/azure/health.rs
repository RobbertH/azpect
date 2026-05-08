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

/// Compute health from the loaded metrics + optional resource state.
///
/// Rules (Lane 2 implements the actual ratio math):
///
/// - `state == "Stopped"` → CRITICAL
/// - 5xx / requests > 5% (over the last hour of the window) → CRITICAL
/// - 5xx / requests in 1–5% → DEGRADED
/// - 5xx / requests < 1% and resource running → HEALTHY
/// - no traffic in window or metric fetch failed → UNKNOWN
pub fn derive(metrics: &[MetricSeries], state: Option<&str>) -> HealthStatus {
    todo!("Lane 2: pick MetricKind::Errors and MetricKind::Traffic, compare ratios per the rules above")
}

/// Find a series by kind. Convenience for callers.
pub fn find(metrics: &[MetricSeries], kind: MetricKind) -> Option<&MetricSeries> {
    metrics.iter().find(|m| m.kind == kind)
}
