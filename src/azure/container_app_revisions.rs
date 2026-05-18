//! Synthesize a `ResourceAvailability` for a Container App by inspecting its
//! revisions. `Microsoft.ResourceHealth/availabilityStatuses/current` doesn't
//! reflect revision-level failure modes (ActivationFailed, Unhealthy, …) for
//! Container Apps, so the platform's signal is `Unknown` even when active
//! revisions are clearly broken. Revision data is the authoritative source.
//!
//! The same revisions response also carries useful display metadata (active
//! revision name, image tag, replica count, scale floor/ceiling), so we parse
//! both in one pass and return them together — saves a second round trip.

#![allow(dead_code, unused_variables)]

use anyhow::Context;
use serde::Deserialize;

use crate::azure::auth::AzureAuth;
use crate::azure::client::ArmClient;
use crate::azure::resource_health::{AvailabilityState, ResourceAvailability};

/// Display metadata for the most-recently-created active revision. Empty when
/// no revision is active.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ActiveRevisionMeta {
    pub name: String,
    /// Image string of the first container in the revision's template (e.g.
    /// `myacr.azurecr.io/files-api:abc123`). Multi-container apps still show
    /// the primary image — sidecars are noise in a one-line header.
    pub image: Option<String>,
    /// Provisioned replicas for this revision (`properties.replicas`).
    pub replicas: u32,
    /// Autoscale floor / ceiling from the revision's template scale config.
    /// Zero means "not set" (KEDA default applies on the server side).
    pub min_replicas: u32,
    pub max_replicas: u32,
}

/// Composite result of `fetch` — the availability signal AND the display meta.
/// Both come from the same revisions response.
#[derive(Clone, Debug)]
pub struct RevisionInfo {
    pub availability: ResourceAvailability,
    pub active_revision: Option<ActiveRevisionMeta>,
}

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

pub async fn fetch(auth: &AzureAuth, resource_id: &str) -> anyhow::Result<RevisionInfo> {
    let client = ArmClient::new(auth.clone())?;
    let path = format!("{resource_id}/revisions");
    let resp = client
        .get(&path, &[("api-version", "2024-03-01")])
        .await
        .with_context(|| format!("revisions for {resource_id}"))?;
    Ok(RevisionInfo {
        availability: derive(&resp),
        active_revision: derive_active_revision(&resp),
    })
}

/// Pick display metadata from the most-recently-created active revision.
/// Falls back to `None` when no active revisions exist or the response has
/// an unexpected shape.
pub fn derive_active_revision(value: &serde_json::Value) -> Option<ActiveRevisionMeta> {
    let revisions = value.get("value").and_then(|v| v.as_array())?;

    let mut best: Option<(&serde_json::Value, &str)> = None;
    for r in revisions {
        let props = r.get("properties")?;
        if !props
            .get("active")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            continue;
        }
        let created = props
            .get("createdTime")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        match &best {
            // Lexicographic compare on RFC3339 timestamps is order-preserving,
            // so we can avoid pulling chrono into this hot path.
            Some((_, prev_created)) if created <= *prev_created => {}
            _ => best = Some((r, created)),
        }
    }

    let (r, _) = best?;
    let props = r.get("properties")?;
    let name = r
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let image = props
        .pointer("/template/containers/0/image")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let replicas = props.get("replicas").and_then(|v| v.as_u64()).unwrap_or(0) as u32;

    let min_replicas = props
        .pointer("/template/scale/minReplicas")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    let max_replicas = props
        .pointer("/template/scale/maxReplicas")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;

    Some(ActiveRevisionMeta {
        name,
        image,
        replicas,
        min_replicas,
        max_replicas,
    })
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
                active: props
                    .get("active")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
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
        assert!(r
            .reason
            .as_deref()
            .unwrap_or("")
            .contains("ActivationFailed"));
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

    #[test]
    fn active_revision_meta_picks_most_recent_active_revision() {
        let payload = json!({ "value": [
            {
                "name": "files-api--old",
                "properties": {
                    "active": true,
                    "createdTime": "2026-05-01T00:00:00Z",
                    "replicas": 1,
                    "template": {
                        "containers": [{ "image": "old:1.0" }],
                        "scale": { "minReplicas": 1, "maxReplicas": 5 }
                    }
                }
            },
            {
                "name": "files-api--new",
                "properties": {
                    "active": true,
                    "createdTime": "2026-05-18T16:00:00Z",
                    "replicas": 2,
                    "template": {
                        "containers": [{ "image": "new:abc123" }],
                        "scale": { "minReplicas": 1, "maxReplicas": 10 }
                    }
                }
            }
        ]});
        let meta = derive_active_revision(&payload).expect("expected active meta");
        assert_eq!(meta.name, "files-api--new");
        assert_eq!(meta.image.as_deref(), Some("new:abc123"));
        assert_eq!(meta.replicas, 2);
        assert_eq!(meta.min_replicas, 1);
        assert_eq!(meta.max_replicas, 10);
    }

    #[test]
    fn active_revision_meta_ignores_inactive_revisions() {
        let payload = json!({ "value": [
            {
                "name": "files-api--failed",
                "properties": {
                    "active": false,
                    "createdTime": "2026-05-18T16:00:00Z",
                    "replicas": 0,
                    "template": { "containers": [{ "image": "failed:1.0" }] }
                }
            },
            {
                "name": "files-api--good",
                "properties": {
                    "active": true,
                    "createdTime": "2026-05-01T00:00:00Z",
                    "replicas": 1,
                    "template": { "containers": [{ "image": "good:1.0" }] }
                }
            }
        ]});
        let meta = derive_active_revision(&payload).expect("expected active meta");
        assert_eq!(meta.name, "files-api--good");
        assert_eq!(meta.image.as_deref(), Some("good:1.0"));
    }

    #[test]
    fn active_revision_meta_returns_none_when_no_active() {
        let payload = json!({ "value": [
            { "name": "x", "properties": { "active": false, "template": {} } }
        ]});
        assert!(derive_active_revision(&payload).is_none());
    }

    #[test]
    fn active_revision_meta_tolerates_missing_scale_block() {
        // Some apps don't configure scale explicitly — min/max default to 0
        // (which the renderer translates as "scale: default").
        let payload = json!({ "value": [{
            "name": "files-api--rev",
            "properties": {
                "active": true,
                "createdTime": "2026-05-18T16:00:00Z",
                "replicas": 1,
                "template": { "containers": [{ "image": "img:tag" }] }
            }
        }]});
        let meta = derive_active_revision(&payload).expect("expected active meta");
        assert_eq!(meta.min_replicas, 0);
        assert_eq!(meta.max_replicas, 0);
    }
}
