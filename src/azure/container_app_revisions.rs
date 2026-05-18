//! Synthesize a `ResourceAvailability` for a Container App by inspecting its
//! revisions. `Microsoft.ResourceHealth/availabilityStatuses/current` doesn't
//! reflect revision-level failure modes (ActivationFailed, Unhealthy, …) for
//! Container Apps, so the platform's signal is `Unknown` even when active
//! revisions are clearly broken. Revision data is the authoritative source.

#![allow(dead_code, unused_variables)]

use anyhow::Context;
use serde::Deserialize;

use crate::azure::auth::AzureAuth;
use crate::azure::client::ArmClient;
use crate::azure::resource_health::{AvailabilityState, ResourceAvailability};

/// Snapshot of one revision row that we care about for the health verdict.
#[derive(Debug, Deserialize, Default)]
struct RevisionProperties {
    #[serde(default)]
    active: bool,
    /// `Healthy` / `Unhealthy` / `None` (no probes configured).
    #[serde(default)]
    health_state: String,
    /// `Running` / `Processing` / `Activating` / `ActivationFailed` /
    /// `Stopped` / `Failed` / `Degraded` / `Unknown`.
    #[serde(default)]
    running_state: String,
}

pub async fn fetch(auth: &AzureAuth, resource_id: &str) -> anyhow::Result<ResourceAvailability> {
    let client = ArmClient::new(auth.clone())?;
    let path = format!("{resource_id}/revisions");
    let resp = client
        .get(&path, &[("api-version", "2024-03-01")])
        .await
        .with_context(|| format!("revisions for {resource_id}"))?;
    Ok(derive(&resp))
}

/// Public for callers that already have the raw response (e.g. tests).
pub fn derive(value: &serde_json::Value) -> ResourceAvailability {
    let revisions = value
        .get("value")
        .and_then(|v| v.as_array())
        .map(|a| a.as_slice())
        .unwrap_or(&[]);

    let active: Vec<RevisionProperties> = revisions
        .iter()
        .filter_map(|r| {
            let props = r.get("properties")?;
            // serde_json's camelCase-tolerant path: rename via manual mapping
            // because the API uses camelCase and we want snake_case fields.
            Some(RevisionProperties {
                active: props.get("active").and_then(|v| v.as_bool()).unwrap_or(false),
                health_state: props
                    .get("healthState")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                running_state: props
                    .get("runningState")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            })
        })
        .filter(|r| r.active)
        .collect();

    if active.is_empty() {
        return ResourceAvailability {
            state: AvailabilityState::Unknown,
            reason: Some("no active revisions".into()),
        };
    }

    // Worst signal across all active revisions wins. A Container App with one
    // healthy and one ActivationFailed revision is still split-brain bad.
    let mut worst = Verdict::Healthy;
    let mut reason: Option<String> = None;
    for r in &active {
        let v = verdict_for(&r.running_state, &r.health_state);
        if v.severity() > worst.severity() {
            worst = v;
            reason = Some(format!("{}/{}", r.running_state, r.health_state));
        }
    }

    let state = match worst {
        Verdict::Healthy => AvailabilityState::Available,
        Verdict::Progressing | Verdict::Unhealthy => AvailabilityState::Degraded,
        Verdict::Failed => AvailabilityState::Unavailable,
    };
    ResourceAvailability { state, reason }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Verdict {
    Healthy,
    /// Still spinning up — not failed yet but not steady either.
    Progressing,
    /// Steady-state but probes are failing.
    Unhealthy,
    /// Terminal failure: ActivationFailed, Failed, Stopped, Degraded.
    Failed,
}

impl Verdict {
    fn severity(self) -> u8 {
        match self {
            Verdict::Healthy => 0,
            Verdict::Progressing => 1,
            Verdict::Unhealthy => 2,
            Verdict::Failed => 3,
        }
    }
}

fn verdict_for(running: &str, health: &str) -> Verdict {
    // Failure modes first — they override anything healthState says.
    if matches!(
        running,
        "ActivationFailed" | "Failed" | "Stopped" | "Degraded"
    ) {
        return Verdict::Failed;
    }
    if matches!(running, "Processing" | "Activating") {
        return Verdict::Progressing;
    }
    if health.eq_ignore_ascii_case("Unhealthy") {
        return Verdict::Unhealthy;
    }
    // `running == "Running"` (or empty/Unknown) with no Unhealthy signal → Healthy.
    // `healthState == "None"` simply means no readiness probe configured; not
    // a negative signal on its own.
    Verdict::Healthy
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn no_active_revisions_is_unknown() {
        let payload = json!({ "value": [
            { "properties": { "active": false, "healthState": "Healthy", "runningState": "Running" } }
        ]});
        let r = derive(&payload);
        assert_eq!(r.state, AvailabilityState::Unknown);
    }

    #[test]
    fn single_healthy_running_is_available() {
        let payload = json!({ "value": [
            { "properties": { "active": true, "healthState": "Healthy", "runningState": "Running" } }
        ]});
        let r = derive(&payload);
        assert_eq!(r.state, AvailabilityState::Available);
    }

    #[test]
    fn running_with_health_none_is_available() {
        // Probes not configured — `None` is not a failure signal.
        let payload = json!({ "value": [
            { "properties": { "active": true, "healthState": "None", "runningState": "Running" } }
        ]});
        let r = derive(&payload);
        assert_eq!(r.state, AvailabilityState::Available);
    }

    #[test]
    fn activation_failed_active_revision_is_unavailable() {
        let payload = json!({ "value": [
            { "properties": { "active": true, "healthState": "Unhealthy", "runningState": "ActivationFailed" } }
        ]});
        let r = derive(&payload);
        assert_eq!(r.state, AvailabilityState::Unavailable);
        assert!(r.reason.as_deref().unwrap_or("").contains("ActivationFailed"));
    }

    #[test]
    fn activating_active_revision_is_degraded() {
        let payload = json!({ "value": [
            { "properties": { "active": true, "healthState": "None", "runningState": "Activating" } }
        ]});
        let r = derive(&payload);
        assert_eq!(r.state, AvailabilityState::Degraded);
    }

    #[test]
    fn worst_active_revision_dominates() {
        // One healthy revision + one ActivationFailed → Unavailable wins
        // (split-brain rollout, half the traffic is dropping).
        let payload = json!({ "value": [
            { "properties": { "active": true, "healthState": "Healthy", "runningState": "Running" } },
            { "properties": { "active": true, "healthState": "Unhealthy", "runningState": "ActivationFailed" } },
            { "properties": { "active": false, "healthState": "Healthy", "runningState": "Running" } }
        ]});
        let r = derive(&payload);
        assert_eq!(r.state, AvailabilityState::Unavailable);
    }

    #[test]
    fn unhealthy_running_is_degraded() {
        // Running but probes failing — degraded, not failed.
        let payload = json!({ "value": [
            { "properties": { "active": true, "healthState": "Unhealthy", "runningState": "Running" } }
        ]});
        let r = derive(&payload);
        assert_eq!(r.state, AvailabilityState::Degraded);
    }

    #[test]
    fn ignores_inactive_revisions() {
        // Old failed revision is inactive — shouldn't affect the verdict.
        let payload = json!({ "value": [
            { "properties": { "active": false, "healthState": "Unhealthy", "runningState": "ActivationFailed" } },
            { "properties": { "active": true,  "healthState": "Healthy",   "runningState": "Running" } }
        ]});
        let r = derive(&payload);
        assert_eq!(r.state, AvailabilityState::Available);
    }

    #[test]
    fn empty_value_array_is_unknown() {
        let r = derive(&json!({ "value": [] }));
        assert_eq!(r.state, AvailabilityState::Unknown);
    }
}
