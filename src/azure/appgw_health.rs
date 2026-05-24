//! Application Gateway backend *health*: the live probe verdict for every
//! server behind a gateway, grouped by backend pool. This is the companion to
//! `appgw_backends` (which shows what a gateway is *wired to*); here we answer
//! "are those targets actually up?".
//!
//! ## Why this is more involved than the pool listing
//!
//! `GET {gateway}?…` is a plain synchronous read. `/backendhealth` is an
//! asynchronous (long-running) ARM operation: the initial `POST` usually
//! returns `202 Accepted` with a `Location` header pointing at an operation
//! URL that has to be polled until it flips to `200 OK` carrying the health
//! document. Small gateways occasionally answer `200` inline on the first POST,
//! so we handle both. The polling cadence honours `Retry-After` and is bounded
//! ([`MAX_POLLS`]) so a stuck operation can't hang the UI task forever.
//!
//! The HTTP orchestration lives in [`fetch_backend_health`]; the decision logic
//! ([`classify_poll`]) and the response parser ([`parse_health`]) are pure so
//! they can be unit-tested without a live tenant.

#![allow(dead_code)]

use std::time::Duration;

use anyhow::{anyhow, Context};
use reqwest::StatusCode;
use serde_json::json;

use crate::azure::auth::AzureAuth;
use crate::azure::client::ArmClient;

const API_VERSION: &str = "2023-09-01";

/// Upper bound on poll attempts before we give up. With the clamped interval
/// below this is on the order of a minute of wall-clock — backend health
/// probes settle well inside that for any realistic gateway.
const MAX_POLLS: usize = 30;

/// Wait between polls when the server doesn't tell us otherwise.
const DEFAULT_POLL_INTERVAL_SECS: u64 = 2;

/// Cap on a server-supplied `Retry-After` so a pathological value can't wedge
/// the operation for minutes per poll.
const MAX_POLL_INTERVAL_SECS: u64 = 15;

/// Per-server probe verdict. ARM's enum spells the up/down states `Up`/`Down`;
/// we normalise to friendlier names and fold anything unrecognised into
/// `Unknown` rather than dropping the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HealthStatus {
    Healthy,
    Unhealthy,
    Partial,
    Draining,
    Unknown,
}

impl HealthStatus {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "up" | "healthy" => HealthStatus::Healthy,
            "down" | "unhealthy" => HealthStatus::Unhealthy,
            "partial" => HealthStatus::Partial,
            "draining" => HealthStatus::Draining,
            _ => HealthStatus::Unknown,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            HealthStatus::Healthy => "Healthy",
            HealthStatus::Unhealthy => "Unhealthy",
            HealthStatus::Partial => "Partial",
            HealthStatus::Draining => "Draining",
            HealthStatus::Unknown => "Unknown",
        }
    }
}

/// One server's health within a pool. `address` is the concrete target ARM
/// probed (resolved IP even for NIC-based pools — which is information the
/// static pool listing never had). `http_setting` records which backend HTTP
/// settings the verdict was measured under, since a server can be probed
/// differently per settings group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerHealth {
    pub address: String,
    pub health: HealthStatus,
    pub http_setting: Option<String>,
    /// Free-form reason from the probe (e.g. `"Success. Received 200 status
    /// code"` or a timeout/cert error). Often empty for healthy servers.
    pub probe_log: Option<String>,
}

/// Health for one backend pool: the pool name plus every probed server. A pool
/// with no servers reported still gets an entry (so the view can show it exists
/// but currently has nothing to probe).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PoolHealth {
    pub name: String,
    pub servers: Vec<ServerHealth>,
}

/// Aggregate server counts across all pools — drives the at-a-glance summary in
/// the view's title bar.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HealthCounts {
    pub healthy: usize,
    pub unhealthy: usize,
    pub other: usize,
}

impl HealthCounts {
    pub fn total(&self) -> usize {
        self.healthy + self.unhealthy + self.other
    }
}

/// Tally server health across pools. `other` folds Partial / Draining /
/// Unknown together — the title bar only needs to draw the eye to red.
pub fn summarize(pools: &[PoolHealth]) -> HealthCounts {
    let mut c = HealthCounts::default();
    for pool in pools {
        for srv in &pool.servers {
            match srv.health {
                HealthStatus::Healthy => c.healthy += 1,
                HealthStatus::Unhealthy => c.unhealthy += 1,
                _ => c.other += 1,
            }
        }
    }
    c
}

/// POST `/backendhealth` and drive the long-running operation to completion,
/// returning the per-pool server health.
pub async fn fetch_backend_health(
    auth: &AzureAuth,
    gateway_id: &str,
) -> anyhow::Result<Vec<PoolHealth>> {
    let client = ArmClient::new(auth.clone())?;
    let path = format!("{gateway_id}/backendhealth");

    let resp = client
        .post_raw(&path, &[("api-version", API_VERSION)], &json!({}))
        .await
        .with_context(|| format!("POST backendhealth for {gateway_id}"))?;

    // Fast path: some gateways answer the result inline on the first POST.
    if resp.status == StatusCode::OK && resp.body.get("backendAddressPools").is_some() {
        return parse_health(&resp.body);
    }

    // Async path. The `Location` header carries the *result* URL (returns the
    // health document on completion); `Azure-AsyncOperation` carries a *status*
    // URL. Prefer Location for polling so a single GET both reports progress
    // and hands back the body; fall back to the status URL if that's all we got.
    let result_url = resp.header("location");
    let status_url = resp.header("azure-asyncoperation");
    let poll_url = result_url
        .clone()
        .or_else(|| status_url.clone())
        .ok_or_else(|| {
            anyhow!(
                "backendhealth returned {} with no Location/Azure-AsyncOperation header to poll",
                resp.status.as_u16()
            )
        })?;

    let mut interval = DEFAULT_POLL_INTERVAL_SECS;
    for _ in 0..MAX_POLLS {
        tokio::time::sleep(Duration::from_secs(interval)).await;

        let p = client
            .get_url_raw(&poll_url)
            .await
            .context("poll backendhealth operation")?;

        if let Some(secs) = p
            .header("retry-after")
            .and_then(|s| s.trim().parse::<u64>().ok())
        {
            interval = secs.clamp(1, MAX_POLL_INTERVAL_SECS);
        }

        match classify_poll(p.status.as_u16(), &p.body) {
            PollOutcome::Done => {
                if p.body.get("backendAddressPools").is_some() {
                    return parse_health(&p.body);
                }
                // Operation reported success via a status doc but didn't inline
                // the result; the body is at the result URL. Fetch it once.
                if let Some(ru) = result_url.as_deref().filter(|ru| *ru != poll_url) {
                    let r = client
                        .get_url_raw(ru)
                        .await
                        .context("fetch completed backendhealth result")?;
                    return parse_health(&r.body);
                }
                // Nothing more to follow — treat as an empty (no pools) result
                // rather than erroring.
                return Ok(Vec::new());
            }
            PollOutcome::Pending => continue,
            PollOutcome::Failed(msg) => {
                return Err(anyhow!("backendhealth operation failed: {msg}"))
            }
        }
    }

    Err(anyhow!(
        "backendhealth did not complete after {MAX_POLLS} polls"
    ))
}

/// What the poll loop should do next, derived purely from a response's status
/// code and body. Kept separate from [`fetch_backend_health`] so the branching
/// can be exercised without HTTP.
#[derive(Debug, PartialEq, Eq)]
enum PollOutcome {
    /// The operation is finished successfully; the result is in this body (if
    /// it carries `backendAddressPools`) or at the result URL otherwise.
    Done,
    /// Still running — poll again.
    Pending,
    /// Terminal failure, with a best-effort reason.
    Failed(String),
}

fn classify_poll(status_code: u16, body: &serde_json::Value) -> PollOutcome {
    // 202 Accepted means the operation is still in flight.
    if status_code == 202 {
        return PollOutcome::Pending;
    }
    if status_code == 200 {
        // The health document itself.
        if body.get("backendAddressPools").is_some() {
            return PollOutcome::Done;
        }
        // Otherwise it's an async-operation status doc: `{ "status": "..." }`.
        if let Some(s) = body.get("status").and_then(|s| s.as_str()) {
            return match s.trim().to_ascii_lowercase().as_str() {
                "succeeded" => PollOutcome::Done,
                "failed" | "canceled" | "cancelled" => PollOutcome::Failed(extract_error(body)),
                // InProgress / Running / Accepted / anything else → keep polling.
                _ => PollOutcome::Pending,
            };
        }
        // 200 with neither marker: nothing left to wait for.
        return PollOutcome::Done;
    }
    // Any other status here is unexpected (errors were already turned into Err
    // by the client); poll again rather than declaring success.
    PollOutcome::Pending
}

/// Best-effort error message out of an async-operation status doc. ARM nests
/// it at `error.message`; fall back to the whole `error` object or a generic
/// string so we never surface an empty reason.
fn extract_error(body: &serde_json::Value) -> String {
    if let Some(msg) = body
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
    {
        return msg.to_string();
    }
    if let Some(err) = body.get("error") {
        return err.to_string();
    }
    "operation reported failure with no detail".to_string()
}

/// Parse the `ApplicationGatewayBackendHealth` document. A missing or non-array
/// `backendAddressPools` yields an empty list (a valid "nothing to probe"
/// state) rather than an error.
pub fn parse_health(value: &serde_json::Value) -> anyhow::Result<Vec<PoolHealth>> {
    let arr = match value.get("backendAddressPools").and_then(|v| v.as_array()) {
        Some(a) => a,
        None => return Ok(Vec::new()),
    };
    Ok(arr.iter().map(parse_one_pool).collect())
}

fn parse_one_pool(v: &serde_json::Value) -> PoolHealth {
    let name = v
        .get("backendAddressPool")
        .and_then(|p| p.get("id"))
        .and_then(|i| i.as_str())
        .map(last_segment)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(unnamed)".to_string());

    let mut servers = Vec::new();
    if let Some(settings) = v
        .get("backendHttpSettingsCollection")
        .and_then(|c| c.as_array())
    {
        for s in settings {
            let setting_name = s
                .get("backendHttpSettings")
                .and_then(|h| h.get("id"))
                .and_then(|i| i.as_str())
                .map(last_segment)
                .filter(|s| !s.is_empty());
            if let Some(srv_arr) = s.get("servers").and_then(|x| x.as_array()) {
                for srv in srv_arr {
                    servers.push(parse_one_server(srv, setting_name.clone()));
                }
            }
        }
    }

    PoolHealth { name, servers }
}

fn parse_one_server(v: &serde_json::Value, http_setting: Option<String>) -> ServerHealth {
    let address = v
        .get("address")
        .and_then(|a| a.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("(unknown)")
        .to_string();
    let health = v
        .get("health")
        .and_then(|h| h.as_str())
        .map(HealthStatus::parse)
        .unwrap_or(HealthStatus::Unknown);
    let probe_log = v
        .get("healthProbeLog")
        .and_then(|p| p.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    ServerHealth {
        address,
        health,
        http_setting,
        probe_log,
    }
}

/// Last non-empty path segment of an ARM id (e.g. `…/backendAddressPools/web`
/// → `web`). Returns the whole input if there are no slashes.
fn last_segment(id: &str) -> String {
    id.rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or(id)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn health_doc() -> serde_json::Value {
        json!({
            "backendAddressPools": [
                {
                    "backendAddressPool": {
                        "id": "/subscriptions/s/resourceGroups/rg/providers/Microsoft.Network/applicationGateways/gw/backendAddressPools/web-pool"
                    },
                    "backendHttpSettingsCollection": [
                        {
                            "backendHttpSettings": {
                                "id": "/subscriptions/s/.../backendHttpSettingsCollection/https-setting"
                            },
                            "servers": [
                                {
                                    "address": "10.0.1.4",
                                    "health": "Up",
                                    "healthProbeLog": "Success. Received 200 status code"
                                },
                                {
                                    "address": "10.0.1.5",
                                    "health": "Down",
                                    "healthProbeLog": "Backend server timed out"
                                }
                            ]
                        }
                    ]
                },
                {
                    "backendAddressPool": {
                        "id": "/x/backendAddressPools/idle-pool"
                    },
                    "backendHttpSettingsCollection": []
                }
            ]
        })
    }

    #[test]
    fn parses_pools_settings_and_servers() {
        let pools = parse_health(&health_doc()).unwrap();
        assert_eq!(pools.len(), 2);

        let web = &pools[0];
        assert_eq!(web.name, "web-pool");
        assert_eq!(web.servers.len(), 2);
        assert_eq!(web.servers[0].address, "10.0.1.4");
        assert_eq!(web.servers[0].health, HealthStatus::Healthy);
        assert_eq!(
            web.servers[0].http_setting.as_deref(),
            Some("https-setting")
        );
        assert_eq!(web.servers[1].health, HealthStatus::Unhealthy);
        assert_eq!(
            web.servers[1].probe_log.as_deref(),
            Some("Backend server timed out")
        );

        // A pool with no http settings reports no servers but still exists.
        assert_eq!(pools[1].name, "idle-pool");
        assert!(pools[1].servers.is_empty());
    }

    #[test]
    fn missing_pools_array_is_empty_not_error() {
        assert!(parse_health(&json!({})).unwrap().is_empty());
        assert!(parse_health(&json!({ "backendAddressPools": null }))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn unknown_health_string_folds_to_unknown() {
        assert_eq!(HealthStatus::parse("Up"), HealthStatus::Healthy);
        assert_eq!(HealthStatus::parse("healthy"), HealthStatus::Healthy);
        assert_eq!(HealthStatus::parse("Down"), HealthStatus::Unhealthy);
        assert_eq!(HealthStatus::parse("Draining"), HealthStatus::Draining);
        assert_eq!(HealthStatus::parse("Partial"), HealthStatus::Partial);
        assert_eq!(HealthStatus::parse("whatever"), HealthStatus::Unknown);
    }

    #[test]
    fn server_without_address_or_health_degrades_gracefully() {
        let srv = parse_one_server(&json!({}), None);
        assert_eq!(srv.address, "(unknown)");
        assert_eq!(srv.health, HealthStatus::Unknown);
        assert!(srv.probe_log.is_none());
        assert!(srv.http_setting.is_none());
    }

    #[test]
    fn summary_counts_split_healthy_unhealthy_other() {
        let pools = parse_health(&health_doc()).unwrap();
        let c = summarize(&pools);
        assert_eq!(c.healthy, 1);
        assert_eq!(c.unhealthy, 1);
        assert_eq!(c.other, 0);
        assert_eq!(c.total(), 2);
    }

    #[test]
    fn last_segment_handles_trailing_slash_and_no_slash() {
        assert_eq!(last_segment("/a/b/c"), "c");
        assert_eq!(last_segment("/a/b/c/"), "c");
        assert_eq!(last_segment("bare"), "bare");
    }

    #[test]
    fn classify_202_is_pending() {
        assert_eq!(classify_poll(202, &json!(null)), PollOutcome::Pending);
    }

    #[test]
    fn classify_200_with_result_is_done() {
        let body = json!({ "backendAddressPools": [] });
        assert_eq!(classify_poll(200, &body), PollOutcome::Done);
    }

    #[test]
    fn classify_status_doc_succeeded_is_done() {
        assert_eq!(
            classify_poll(200, &json!({ "status": "Succeeded" })),
            PollOutcome::Done
        );
    }

    #[test]
    fn classify_status_doc_in_progress_is_pending() {
        assert_eq!(
            classify_poll(200, &json!({ "status": "InProgress" })),
            PollOutcome::Pending
        );
    }

    #[test]
    fn classify_status_doc_failed_extracts_message() {
        let body = json!({
            "status": "Failed",
            "error": { "code": "X", "message": "probe configuration invalid" }
        });
        assert_eq!(
            classify_poll(200, &body),
            PollOutcome::Failed("probe configuration invalid".to_string())
        );
    }
}
