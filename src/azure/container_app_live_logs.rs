//! Real-time console log stream for a Container App — the portal's "Real-time"
//! log display, and what `az containerapp logs show --follow` does.
//!
//! Unlike [`crate::azure::logs`], this never touches the managed environment or
//! its Log Analytics workspace, so it works for callers who can read the app
//! but not the environment (where historical queries fail with a 403):
//!
//! 1. `GET {app}/revisions` → the active revisions.
//! 2. `GET {app}/revisions/{rev}/replicas` → per container, a
//!    `logStreamEndpoint` on the regional data plane.
//! 3. `POST {app}/getAuthToken` → a short-lived bearer for that data plane.
//! 4. `GET {logStreamEndpoint}?follow=true&tailLines=N&output=text` → a chunked
//!    response that stays open, one log line per text line.
//!
//! [`run`] supervises one stream per replica × container, re-listing replicas
//! periodically so scale-out and new revisions are picked up, and reconnecting
//! streams that drop (idle timeout, token expiry, replica restart).

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context};
use chrono::{DateTime, NaiveDateTime, Utc};

use crate::azure::auth::AzureAuth;
use crate::azure::client::{ArmClient, USER_AGENT};
use crate::azure::logs::{LogLevel, LogLine};

const API_VERSION: &str = "2024-03-01";

/// Backlog requested when a stream first connects. The service caps this at
/// 300, so the live tail can never replace the historical view.
const INITIAL_TAIL_LINES: u32 = 300;

/// Backlog requested on a reconnect; lines at or before the last one already
/// delivered are dropped, so this only has to cover the gap.
const RECONNECT_TAIL_LINES: u32 = 50;

/// How often the replica set is re-listed to pick up scale-out / new revisions
/// and retire streams whose replica is gone.
const REPLICA_POLL: Duration = Duration::from_secs(30);

/// A stream that has been silent this long is assumed half-dead and is
/// reconnected. Quiet apps reconnect invisibly (the backlog is deduplicated).
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Upper bound on concurrent streams, so a heavily scaled-out app doesn't open
/// dozens of long-lived connections.
const MAX_STREAMS: usize = 16;

/// Delays between reconnect attempts after consecutive failures.
const BACKOFF_SECS: &[u64] = &[2, 5, 10, 30];

/// Connection state of the live tail, for the logs header.
#[derive(Clone, Debug, PartialEq)]
pub enum LiveStatus {
    Connecting,
    /// At least one stream is open; `streams` is how many.
    Streaming {
        streams: usize,
    },
    /// Nothing to stream from right now (e.g. scaled to zero); still polling.
    Waiting(String),
    /// The last attempt failed; retrying in the background.
    Failed(String),
}

#[derive(Debug)]
pub enum LiveEvent {
    Lines(Vec<LogLine>),
    Status(LiveStatus),
}

pub type Emit = Arc<dyn Fn(LiveEvent) + Send + Sync>;

/// One container of one replica: the unit a log stream attaches to.
#[derive(Clone, Debug, PartialEq)]
pub struct StreamTarget {
    pub revision: String,
    pub replica: String,
    pub container: String,
    pub endpoint: String,
}

impl StreamTarget {
    fn key(&self) -> String {
        format!("{}/{}", self.replica, self.container)
    }
}

/// Names of the active revisions in a `{app}/revisions` response.
pub fn active_revisions(value: &serde_json::Value) -> Vec<String> {
    value
        .get("value")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter(|r| {
                    r.pointer("/properties/active")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                })
                .filter_map(|r| r.get("name").and_then(|v| v.as_str()))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Stream targets in a `{app}/revisions/{rev}/replicas` response. Containers
/// without an https `logStreamEndpoint` are skipped — the app's bearer token
/// is only ever sent to an endpoint ARM handed us, over TLS.
pub fn stream_targets(revision: &str, value: &serde_json::Value) -> Vec<StreamTarget> {
    let mut out = Vec::new();
    let Some(replicas) = value.get("value").and_then(|v| v.as_array()) else {
        return out;
    };
    for replica in replicas {
        let Some(replica_name) = replica.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        let containers = replica
            .pointer("/properties/containers")
            .and_then(|v| v.as_array());
        for c in containers.into_iter().flatten() {
            let name = c.get("name").and_then(|v| v.as_str());
            let endpoint = c.get("logStreamEndpoint").and_then(|v| v.as_str());
            if let (Some(name), Some(endpoint)) = (name, endpoint) {
                if endpoint.starts_with("https://") {
                    out.push(StreamTarget {
                        revision: revision.to_string(),
                        replica: replica_name.to_string(),
                        container: name.to_string(),
                        endpoint: endpoint.to_string(),
                    });
                }
            }
        }
    }
    out
}

async fn list_targets(auth: &AzureAuth, app_id: &str) -> anyhow::Result<Vec<StreamTarget>> {
    let client = ArmClient::new(auth.clone())?;
    let revisions = client
        .get(
            &format!("{app_id}/revisions"),
            &[("api-version", API_VERSION)],
        )
        .await
        .context("listing revisions")?;
    let mut targets = Vec::new();
    for revision in active_revisions(&revisions) {
        let replicas = client
            .get(
                &format!("{app_id}/revisions/{revision}/replicas"),
                &[("api-version", API_VERSION)],
            )
            .await
            .with_context(|| format!("listing replicas of {revision}"))?;
        targets.extend(stream_targets(&revision, &replicas));
    }
    Ok(targets)
}

async fn fetch_stream_token(auth: &AzureAuth, app_id: &str) -> anyhow::Result<String> {
    let client = ArmClient::new(auth.clone())?;
    let resp = client
        .post(
            &format!("{app_id}/getAuthToken?api-version={API_VERSION}"),
            &serde_json::json!({}),
        )
        .await
        .context("requesting log stream token")?;
    resp.pointer("/properties/token")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("log stream token missing from getAuthToken response"))
}

fn parse_timestamp(s: &str) -> Option<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    // The platform's own banner lines carry a zone-less timestamp.
    NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f")
        .ok()
        .map(|n| n.and_utc())
}

fn infer_level(message: &str) -> LogLevel {
    let lower = message.to_lowercase();
    if ["error", "exception", "fatal", "panic", "critical"]
        .iter()
        .any(|kw| lower.contains(kw))
    {
        LogLevel::Error
    } else if lower.contains("warn") {
        LogLevel::Warn
    } else {
        LogLevel::Info
    }
}

/// Split a container-runtime prefix (`stdout F ` / `stderr P `) off `rest`,
/// returning the stream name and the message. `None` when there is no prefix.
fn split_runtime_prefix(rest: &str) -> Option<(&'static str, &str)> {
    let (stream, after) = ["stdout", "stderr"].into_iter().find_map(|name| {
        let after = rest.strip_prefix(name)?.strip_prefix(' ')?;
        Some((name, after))
    })?;
    let message = after
        .strip_prefix("F ")
        .or_else(|| after.strip_prefix("P "))
        .unwrap_or(after);
    Some((stream, message))
}

/// Parse one line of the stream. The text format is
/// `<rfc3339> <stdout|stderr> <F|P> <message>`; a JSON line
/// (`{"TimeStamp": …, "Log": …}`) is accepted too. The platform's own
/// connection banners are dropped (`None`). Lines in neither shape are kept
/// verbatim, stamped with `now`, rather than silently lost. The `bool` is
/// whether the timestamp came from the line itself.
pub fn parse_line(raw: &str, target: &StreamTarget, now: DateTime<Utc>) -> Option<(LogLine, bool)> {
    let raw = raw.trim_end_matches(['\r', '\n']);
    if raw.trim().is_empty() {
        return None;
    }

    let mut ts = None;
    let mut body = raw;
    let mut json_log = None;
    if raw.starts_with('{') {
        if let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(raw) {
            let get = |key: &str| {
                map.iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(key))
                    .and_then(|(_, v)| v.as_str())
                    .map(str::to_string)
            };
            if let Some(log) = get("Log") {
                ts = get("TimeStamp").and_then(|t| parse_timestamp(&t));
                json_log = Some(log);
            }
        }
    }
    if let Some(log) = json_log.as_deref() {
        body = log;
    } else if let Some((first, rest)) = raw.split_once(' ') {
        if let Some(parsed) = parse_timestamp(first) {
            ts = Some(parsed);
            body = rest.trim_start();
        }
    }

    let (stream, message) = match split_runtime_prefix(body) {
        Some((stream, message)) => (Some(stream), message),
        None => (None, body),
    };
    // Timestamped lines without a stdout/stderr marker are the service
    // announcing the connection, not application output.
    if stream.is_none()
        && json_log.is_none()
        && ts.is_some()
        && (message.starts_with("Connecting to") || message.starts_with("Successfully Connected"))
    {
        return None;
    }

    let mut fields = vec![
        ("container".to_string(), target.container.clone()),
        ("replica".to_string(), target.replica.clone()),
        ("revision".to_string(), target.revision.clone()),
    ];
    if let Some(stream) = stream {
        fields.push(("stream".to_string(), stream.to_string()));
    }
    Some((
        LogLine {
            ts: ts.unwrap_or(now),
            level: infer_level(message),
            source: target.container.clone(),
            message: message.to_string(),
            fields,
        },
        ts.is_some(),
    ))
}

/// Parse a batch of raw lines, dropping any at or before `last_ts` (the
/// backlog replayed on a reconnect) and advancing `last_ts`.
fn parse_batch(
    raw_lines: &[String],
    target: &StreamTarget,
    last_ts: &mut Option<DateTime<Utc>>,
    floor: Option<DateTime<Utc>>,
) -> Vec<LogLine> {
    let now = Utc::now();
    let mut out = Vec::new();
    for raw in raw_lines {
        let Some((line, stamped)) = parse_line(raw, target, now) else {
            continue;
        };
        if stamped {
            if floor.is_some_and(|f| line.ts <= f) {
                continue;
            }
            if last_ts.is_none_or(|l| line.ts > l) {
                *last_ts = Some(line.ts);
            }
        }
        out.push(line);
    }
    out
}

/// Why one connection attempt ended.
enum StreamEnd {
    /// The server closed the stream, or it went idle; reconnect promptly.
    Closed,
    /// The replica or container no longer exists; stop for good.
    Gone,
    Failed(String),
}

/// Decrements the open-stream count when a connection ends, including when
/// the task is aborted mid-read.
struct ConnectedGuard {
    connected: Arc<AtomicUsize>,
}

impl Drop for ConnectedGuard {
    fn drop(&mut self) {
        self.connected.fetch_sub(1, Ordering::SeqCst);
    }
}

#[allow(clippy::too_many_arguments)]
async fn stream_once(
    http: &reqwest::Client,
    auth: &AzureAuth,
    app_id: &str,
    target: &StreamTarget,
    tail_lines: u32,
    last_ts: &mut Option<DateTime<Utc>>,
    connected: &Arc<AtomicUsize>,
    emit: &Emit,
) -> StreamEnd {
    let token = match fetch_stream_token(auth, app_id).await {
        Ok(t) => t,
        Err(e) => return StreamEnd::Failed(format!("{e:#}")),
    };
    let resp = http
        .get(&target.endpoint)
        .query(&[
            ("follow", "true".to_string()),
            ("tailLines", tail_lines.to_string()),
            ("output", "text".to_string()),
        ])
        .bearer_auth(token)
        .send()
        .await;
    let mut resp = match resp {
        Ok(r) => r,
        Err(e) => return StreamEnd::Failed(format!("log stream connect failed: {e}")),
    };
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return StreamEnd::Gone;
    }
    if !status.is_success() {
        let body: String = resp
            .text()
            .await
            .unwrap_or_default()
            .chars()
            .take(500)
            .collect();
        return StreamEnd::Failed(format!("log stream error {}: {body}", status.as_u16()));
    }

    let streams = connected.fetch_add(1, Ordering::SeqCst) + 1;
    let _guard = ConnectedGuard {
        connected: connected.clone(),
    };
    emit(LiveEvent::Status(LiveStatus::Streaming { streams }));

    // Lines at or before this were already delivered by the previous
    // connection; the reconnect backlog replays them.
    let floor = *last_ts;
    let mut pending: Vec<u8> = Vec::new();
    loop {
        let chunk = match tokio::time::timeout(IDLE_TIMEOUT, resp.chunk()).await {
            Err(_) => return StreamEnd::Closed,
            Ok(Err(e)) => return StreamEnd::Failed(format!("log stream interrupted: {e}")),
            Ok(Ok(None)) => return StreamEnd::Closed,
            Ok(Ok(Some(chunk))) => chunk,
        };
        pending.extend_from_slice(&chunk);
        let mut raw_lines = Vec::new();
        while let Some(pos) = pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = pending.drain(..=pos).collect();
            raw_lines.push(String::from_utf8_lossy(&line).into_owned());
        }
        let lines = parse_batch(&raw_lines, target, last_ts, floor);
        if !lines.is_empty() {
            emit(LiveEvent::Lines(lines));
        }
    }
}

async fn run_stream(
    http: reqwest::Client,
    auth: AzureAuth,
    app_id: String,
    target: StreamTarget,
    connected: Arc<AtomicUsize>,
    emit: Emit,
) {
    let mut last_ts = None;
    let mut first = true;
    let mut failures = 0usize;
    loop {
        let tail = if first {
            INITIAL_TAIL_LINES
        } else {
            RECONNECT_TAIL_LINES
        };
        first = false;
        let end = stream_once(
            &http,
            &auth,
            &app_id,
            &target,
            tail,
            &mut last_ts,
            &connected,
            &emit,
        )
        .await;
        let delay = match end {
            StreamEnd::Gone => return,
            StreamEnd::Closed => {
                failures = 0;
                1
            }
            StreamEnd::Failed(e) => {
                tracing::warn!(target = %target.key(), "live log stream failed: {e}");
                // Only surface the failure when no stream is delivering;
                // otherwise one flapping replica would mask a healthy tail.
                if connected.load(Ordering::SeqCst) == 0 {
                    emit(LiveEvent::Status(LiveStatus::Failed(e)));
                }
                let d = BACKOFF_SECS[failures.min(BACKOFF_SECS.len() - 1)];
                failures += 1;
                d
            }
        };
        tokio::time::sleep(Duration::from_secs(delay)).await;
    }
}

/// Tail every container of every replica of `app_id`'s active revisions,
/// reporting lines and connection state through `emit`. Runs until aborted;
/// aborting the task running this future tears down every stream.
pub async fn run(auth: AzureAuth, app_id: String, emit: Emit) {
    emit(LiveEvent::Status(LiveStatus::Connecting));
    let http = match reqwest::Client::builder()
        .use_rustls_tls()
        .user_agent(USER_AGENT)
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
    {
        Ok(h) => h,
        Err(e) => {
            emit(LiveEvent::Status(LiveStatus::Failed(format!(
                "failed to build http client: {e}"
            ))));
            return;
        }
    };
    let connected = Arc::new(AtomicUsize::new(0));
    // Dropping the set (when this future is aborted) aborts every stream.
    let mut streams = tokio::task::JoinSet::new();
    let mut running: HashMap<String, tokio::task::AbortHandle> = HashMap::new();
    loop {
        match list_targets(&auth, &app_id).await {
            Ok(mut targets) => {
                targets.truncate(MAX_STREAMS);
                running.retain(|key, handle| {
                    let keep = !handle.is_finished() && targets.iter().any(|t| t.key() == *key);
                    if !keep {
                        handle.abort();
                    }
                    keep
                });
                if targets.is_empty() {
                    emit(LiveEvent::Status(LiveStatus::Waiting(
                        "no running replicas".to_string(),
                    )));
                }
                for target in targets {
                    let key = target.key();
                    if running.contains_key(&key) {
                        continue;
                    }
                    let handle = streams.spawn(run_stream(
                        http.clone(),
                        auth.clone(),
                        app_id.clone(),
                        target,
                        connected.clone(),
                        emit.clone(),
                    ));
                    running.insert(key, handle);
                }
            }
            Err(e) => {
                if connected.load(Ordering::SeqCst) == 0 {
                    emit(LiveEvent::Status(LiveStatus::Failed(format!("{e:#}"))));
                }
            }
        }
        while streams.try_join_next().is_some() {}
        tokio::time::sleep(REPLICA_POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn target() -> StreamTarget {
        StreamTarget {
            revision: "app--0000001".into(),
            replica: "app--0000001-5c4ccc869d-wsckt".into(),
            container: "listener".into(),
            endpoint: "https://region.example/logstream".into(),
        }
    }

    #[test]
    fn active_revisions_skips_inactive() {
        let v = json!({ "value": [
            { "name": "app--1", "properties": { "active": false } },
            { "name": "app--2", "properties": { "active": true } },
            { "name": "app--3", "properties": {} },
        ]});
        assert_eq!(active_revisions(&v), vec!["app--2".to_string()]);
        assert!(active_revisions(&json!({})).is_empty());
    }

    #[test]
    fn stream_targets_takes_https_endpoints_per_container() {
        let v = json!({ "value": [{
            "name": "app--2-abc",
            "properties": { "containers": [
                { "name": "main", "logStreamEndpoint": "https://r.example/main/logstream" },
                { "name": "sidecar", "logStreamEndpoint": "https://r.example/sidecar/logstream" },
                { "name": "plain", "logStreamEndpoint": "http://r.example/plain/logstream" },
                { "name": "none" },
            ]}
        }]});
        let targets = stream_targets("app--2", &v);
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].container, "main");
        assert_eq!(targets[0].replica, "app--2-abc");
        assert_eq!(targets[0].revision, "app--2");
        assert_eq!(targets[1].endpoint, "https://r.example/sidecar/logstream");
    }

    #[test]
    fn parses_text_line_with_runtime_prefix() {
        let raw = "2026-11-25T16:43:31.1739224Z stderr F 2026-11-25 16:43:31,173 INFO [poll] src.poll: Polling queue 'q'...";
        let (line, stamped) = parse_line(raw, &target(), Utc::now()).unwrap();
        assert!(stamped);
        assert_eq!(
            line.ts,
            DateTime::parse_from_rfc3339("2026-11-25T16:43:31.1739224Z").unwrap()
        );
        assert_eq!(
            line.message,
            "2026-11-25 16:43:31,173 INFO [poll] src.poll: Polling queue 'q'..."
        );
        assert_eq!(line.source, "listener");
        assert_eq!(line.level, LogLevel::Info);
        assert!(line
            .fields
            .contains(&("stream".to_string(), "stderr".to_string())));
        assert!(line.fields.contains(&(
            "replica".to_string(),
            "app--0000001-5c4ccc869d-wsckt".to_string()
        )));
    }

    #[test]
    fn infers_level_from_message() {
        let t = target();
        let now = Utc::now();
        let err = parse_line(
            "2026-11-25T16:43:31Z stdout F Traceback: ValueError exception",
            &t,
            now,
        )
        .unwrap()
        .0;
        assert_eq!(err.level, LogLevel::Error);
        let warn = parse_line("2026-11-25T16:43:31Z stdout F WARNING slow poll", &t, now)
            .unwrap()
            .0;
        assert_eq!(warn.level, LogLevel::Warn);
    }

    #[test]
    fn drops_connection_banners_and_blank_lines() {
        let t = target();
        let now = Utc::now();
        assert!(parse_line(
            "2026-11-25T16:46:47.17289  Connecting to the container 'listener'...",
            &t,
            now
        )
        .is_none());
        assert!(parse_line(
            "2026-11-25T16:46:47.21050  Successfully Connected to container: 'listener' [Revision: 'r']",
            &t,
            now
        )
        .is_none());
        assert!(parse_line("  \r\n", &t, now).is_none());
    }

    #[test]
    fn parses_json_line() {
        let raw = r#"{"TimeStamp":"2026-11-25T16:43:31.17392Z","Log":"stdout F hello"}"#;
        let (line, stamped) = parse_line(raw, &target(), Utc::now()).unwrap();
        assert!(stamped);
        assert_eq!(line.message, "hello");
    }

    #[test]
    fn keeps_unrecognised_lines_verbatim() {
        let now = Utc::now();
        let (line, stamped) = parse_line("plain output, no timestamp", &target(), now).unwrap();
        assert!(!stamped);
        assert_eq!(line.ts, now);
        assert_eq!(line.message, "plain output, no timestamp");
    }

    #[test]
    fn reconnect_backlog_is_deduplicated() {
        let t = target();
        let raw: Vec<String> = [
            "2026-11-25T16:43:31Z stdout F one",
            "2026-11-25T16:43:41Z stdout F two",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let mut last_ts = None;
        assert_eq!(parse_batch(&raw, &t, &mut last_ts, None).len(), 2);

        // The reconnect replays both lines plus one new one.
        let floor = last_ts;
        let mut replay = raw.clone();
        replay.push("2026-11-25T16:43:51Z stdout F three".to_string());
        let fresh = parse_batch(&replay, &t, &mut last_ts, floor);
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].message, "three");
    }
}
