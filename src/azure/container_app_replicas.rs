//! Fetch per-replica live status for a Container App revision. The portal's
//! "Replica details" pane lists each running container in a replica with its
//! Ready / Restart / Started status; that data comes from
//! `…/revisions/{rev}/replicas`, a different endpoint from the one that drives
//! the health badge (`/revisions`). Both run on the same `2024-03-01` api
//! version.
//!
//! A revision can have N active replicas (autoscale floor → ceiling), and
//! every replica runs *all* the containers defined in the revision template,
//! so the response shape is replicas × containers.

#![allow(dead_code, unused_variables)]

use anyhow::Context;
use chrono::{DateTime, Utc};

use crate::azure::auth::AzureAuth;
use crate::azure::client::ArmClient;

const API_VERSION: &str = "2024-03-01";

/// One running replica of a revision. Mirrors what the portal calls a "replica"
/// — the unit autoscale spins up. Each carries N container statuses.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReplicaInstance {
    /// Full replica name (e.g. `ca-pp-rnd3-files-dev--0000002-b77496699-r58pz`).
    /// The renderer trims this for display since the prefix repeats across
    /// every replica of the same revision.
    pub name: String,
    /// Replica creation time. Used to sort newest-first in the UI. `None` when
    /// the field is missing or unparseable (treated as oldest).
    pub created_at: Option<DateTime<Utc>>,
    /// Top-level `runningState` for the replica (e.g. `Running`, `Failed`,
    /// `Stopped`). `None` when absent.
    pub running_state: Option<String>,
    /// Per-container status, in the order Azure returns them (which matches
    /// the template's container order in practice).
    pub containers: Vec<ReplicaContainer>,
}

/// One container inside a replica. `ready` / `started` map to Kubernetes-style
/// probe results; `restart_count` is the count since the replica was created.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReplicaContainer {
    /// Container name (`files`, `files-api`, `http-auth`, …). Matches the
    /// `name` in the revision template's `containers` array.
    pub name: String,
    /// `properties.containers[].ready`. `None` when the field is absent.
    pub ready: Option<bool>,
    /// `properties.containers[].started`. `None` when the field is absent.
    pub started: Option<bool>,
    /// `properties.containers[].restartCount`. Defaults to 0 when missing.
    pub restart_count: u32,
    /// Per-container `runningState` (e.g. `Running`, `Waiting`, `Terminated`).
    /// `None` when not reported.
    pub running_state: Option<String>,
}

pub async fn fetch(
    auth: &AzureAuth,
    container_app_id: &str,
    revision_name: &str,
) -> anyhow::Result<Vec<ReplicaInstance>> {
    let client = ArmClient::new(auth.clone())?;
    let path = format!("{container_app_id}/revisions/{revision_name}/replicas");
    let resp = client
        .get(&path, &[("api-version", API_VERSION)])
        .await
        .with_context(|| format!("replicas for {container_app_id}@{revision_name}"))?;
    Ok(extract(&resp))
}

/// Pulled out so tests can exercise the parser without going through ARM.
pub fn extract(value: &serde_json::Value) -> Vec<ReplicaInstance> {
    let arr = match value.get("value").and_then(|v| v.as_array()) {
        Some(a) => a,
        None => return Vec::new(),
    };

    arr.iter().map(extract_replica).collect()
}

fn extract_replica(r: &serde_json::Value) -> ReplicaInstance {
    let name = r
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let props = r.get("properties");

    let created_at = props
        .and_then(|p| p.get("createdTime"))
        .and_then(|v| v.as_str())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc));

    let running_state = props
        .and_then(|p| p.get("runningState"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let containers = props
        .and_then(|p| p.get("containers"))
        .and_then(|v| v.as_array())
        .map(|a| a.iter().map(extract_container).collect())
        .unwrap_or_default();

    ReplicaInstance {
        name,
        created_at,
        running_state,
        containers,
    }
}

fn extract_container(c: &serde_json::Value) -> ReplicaContainer {
    let name = c
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let ready = c.get("ready").and_then(|v| v.as_bool());
    let started = c.get("started").and_then(|v| v.as_bool());
    let restart_count = c
        .get("restartCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
        .min(u32::MAX as u64) as u32;
    let running_state = c
        .get("runningState")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    ReplicaContainer {
        name,
        ready,
        started,
        restart_count,
        running_state,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_replica_with_three_containers_all_ready() {
        // Shape matches the portal sample: one replica, three containers
        // (files / files-api / http-auth), all Ready=true with zero restarts.
        let payload = json!({ "value": [
            {
                "name": "ca-pp-rnd3-files-dev--0000002-b77496699-r58pz",
                "properties": {
                    "createdTime": "2026-05-26T16:37:49+00:00",
                    "runningState": "Running",
                    "containers": [
                        { "name": "files",     "ready": true, "started": true, "restartCount": 0, "runningState": "Running" },
                        { "name": "files-api", "ready": true, "started": true, "restartCount": 0, "runningState": "Running" },
                        { "name": "http-auth", "ready": true, "started": true, "restartCount": 0, "runningState": "Running" }
                    ]
                }
            }
        ]});
        let replicas = extract(&payload);
        assert_eq!(replicas.len(), 1);
        let r = &replicas[0];
        assert_eq!(r.name, "ca-pp-rnd3-files-dev--0000002-b77496699-r58pz");
        assert_eq!(r.running_state.as_deref(), Some("Running"));
        assert!(r.created_at.is_some());
        assert_eq!(r.containers.len(), 3);
        assert_eq!(r.containers[0].name, "files");
        assert_eq!(r.containers[0].ready, Some(true));
        assert_eq!(r.containers[0].restart_count, 0);
        assert_eq!(r.containers[2].name, "http-auth");
    }

    #[test]
    fn missing_value_array_returns_empty() {
        assert!(extract(&json!({})).is_empty());
        assert!(extract(&json!({ "value": null })).is_empty());
    }

    #[test]
    fn empty_value_array_returns_empty() {
        assert!(extract(&json!({ "value": [] })).is_empty());
    }

    #[test]
    fn tolerates_missing_per_container_probe_fields() {
        // A waiting/initializing container can omit `ready` and `started` —
        // we surface them as `None` rather than defaulting to false (which
        // would lie about probe status).
        let payload = json!({ "value": [
            {
                "name": "rev--xyz",
                "properties": {
                    "containers": [
                        { "name": "sidecar", "restartCount": 2 }
                    ]
                }
            }
        ]});
        let replicas = extract(&payload);
        let c = &replicas[0].containers[0];
        assert_eq!(c.name, "sidecar");
        assert!(c.ready.is_none());
        assert!(c.started.is_none());
        assert_eq!(c.restart_count, 2);
    }

    #[test]
    fn tolerates_missing_properties_block() {
        // Pathological row with no properties at all — we still record the
        // name so the caller can show *something*, but containers is empty.
        let payload = json!({ "value": [{ "name": "stub" }] });
        let replicas = extract(&payload);
        assert_eq!(replicas.len(), 1);
        assert_eq!(replicas[0].name, "stub");
        assert!(replicas[0].containers.is_empty());
    }

    #[test]
    fn parses_iso_created_time_into_utc() {
        use chrono::{Datelike, Timelike};
        let payload = json!({ "value": [
            { "name": "r", "properties": { "createdTime": "2026-05-26T16:37:49Z" } }
        ]});
        let dt = extract(&payload)[0].created_at.expect("created_at parsed");
        assert_eq!(dt.year(), 2026);
        assert_eq!(dt.month(), 5);
        assert_eq!(dt.day(), 26);
        assert_eq!(dt.hour(), 16);
        assert_eq!(dt.minute(), 37);
        assert_eq!(dt.second(), 49);
    }

    #[test]
    fn skips_unparseable_created_time_without_panicking() {
        let payload = json!({ "value": [
            { "name": "r", "properties": { "createdTime": "not-a-date" } }
        ]});
        assert!(extract(&payload)[0].created_at.is_none());
    }
}
