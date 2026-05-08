//! Azure Monitor metrics fetch + per-resource-type metric mapping.
//!
//! All resource types expose four logical metrics through this module — Errors,
//! Traffic, Cpu, Memory — backed by different physical metric names depending
//! on the resource. Callers see a uniform `Vec<MetricSeries>`.

#![allow(dead_code, unused_variables)]

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::azure::auth::AzureAuth;
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
    pub fn timespan(&self) -> String {
        let end = Utc::now();
        let start = end - self.duration();
        format!("{}/{}", start.to_rfc3339(), end.to_rfc3339())
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

/// Fetch all relevant metrics for a resource over the given range, in parallel
/// where possible. Missing metrics (e.g. APIM has no memory) are simply absent
/// from the returned vec — never an error.
pub async fn fetch(auth: &AzureAuth, resource: &Resource, range: TimeRange) -> anyhow::Result<Vec<MetricSeries>> {
    todo!(
        "Lane 2: GET {{resourceId}}/providers/Microsoft.Insights/metrics?\
         api-version=2023-10-01&timespan=...&interval=...&metricnames=...&aggregation=...&$filter=... \
         then parse value[].timeseries[].data[] into MetricPoint"
    )
}
