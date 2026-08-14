//! Logic Apps (consumption `Microsoft.Logic/workflows`) — read-only run
//! diagnostics: the workflow list, per-workflow run history, per-trigger
//! firing history, per-run action breakdown, and the message content
//! (inputs/outputs) behind a run action or trigger firing.
//!
//! Contract with the UI: five fetchers, all read-only.
//!
//! * [`list_workflows`] — Resource Graph, one query across the subscription
//!   scope (mirrors `registries.rs` / `cosmos.rs`).
//! * [`list_runs`] / [`list_trigger_history`] / [`list_run_actions`] — the ARM
//!   control plane under the workflow's resource id (`Reader` on the workflow
//!   is enough; no data-plane role exists for Logic Apps history).
//! * [`fetch_content`] — downloads `inputsLink` / `outputsLink` payloads. The
//!   links are pre-signed SAS URIs minted by ARM in the listing responses:
//!   they carry their own credential in the query string and must be fetched
//!   **without** an `Authorization` header (Azure rejects a request carrying
//!   both a SAS and a bearer), so this goes through `cosmos.rs`'s raw
//!   `build_http`/`send_with_retry` plumbing rather than `client.rs`. The
//!   query string is a credential — it is never logged and always redacted
//!   from error messages.
//!
//! Standard (single-tenant) Logic Apps are `Microsoft.Web/sites` with a
//! `workflowapp` kind and a completely different history API — out of scope
//! for v1; this module only surfaces consumption workflows.

#![allow(dead_code, unused_variables)]

use anyhow::{anyhow, Context};
use chrono::{DateTime, Utc};

use crate::azure::auth::AzureAuth;
use crate::azure::client::{ArmClient, ARM_BASE};
use crate::azure::cosmos::{build_http, send_with_retry};

/// A consumption Logic App workflow, from Resource Graph.
#[derive(Clone, Debug)]
pub struct LogicApp {
    /// Full ARM id (`/subscriptions/.../providers/Microsoft.Logic/workflows/<name>`).
    pub id: String,
    pub name: String,
    pub resource_group: String,
    pub subscription_id: String,
    pub location: String,
    /// `properties.state` — `Enabled` / `Disabled` / `Suspended`.
    pub state: Option<String>,
    /// `properties.changedTime` — last definition change.
    pub changed_at: Option<DateTime<Utc>>,
    /// `properties.createdTime`.
    pub created_at: Option<DateTime<Utc>>,
}

/// A pre-signed content link (`inputsLink` / `outputsLink`) from a run,
/// action, or trigger-history row. `uri`'s query string is a short-lived SAS
/// credential: never log it, never show it, redact it from errors.
#[derive(Clone, Debug)]
pub struct ContentLink {
    pub uri: String,
    /// `contentSize` in bytes, when the service reports it. Used to skip the
    /// download entirely for payloads over the preview cap.
    pub size: Option<i64>,
}

/// One row of a workflow's run history (`GET {workflow}/runs`).
#[derive(Clone, Debug)]
pub struct WorkflowRun {
    /// The run id (`name`), an opaque sortable stamp — also the path segment
    /// for the run's actions listing.
    pub name: String,
    /// `properties.status` — `Succeeded` / `Failed` / `Running` / `Cancelled`
    /// / `Waiting` / `Aborted`.
    pub status: String,
    pub start_time: Option<DateTime<Utc>>,
    pub end_time: Option<DateTime<Utc>>,
    /// `properties.trigger.name`.
    pub trigger_name: Option<String>,
    /// `properties.trigger.inputsLink` / `outputsLink` — the triggering
    /// message's content for this run; surfaced as a synthetic first row in
    /// the actions view so the payload that started the run is one Enter away.
    pub trigger_inputs: Option<ContentLink>,
    pub trigger_outputs: Option<ContentLink>,
    /// `properties.error.code`: `message`, flattened for the table.
    pub error: Option<String>,
    /// `properties.correlation.clientTrackingId`.
    pub correlation_id: Option<String>,
}

/// One action of a run (`GET {workflow}/runs/{run}/actions`).
#[derive(Clone, Debug)]
pub struct RunAction {
    pub name: String,
    /// `properties.status` — same vocabulary as runs, plus `Skipped`.
    pub status: String,
    /// `properties.code`, e.g. `OK` / `BadRequest` / `ActionFailed`.
    pub code: Option<String>,
    pub start_time: Option<DateTime<Utc>>,
    pub end_time: Option<DateTime<Utc>>,
    /// `properties.error.code`: `message`, flattened.
    pub error: Option<String>,
    pub inputs: Option<ContentLink>,
    pub outputs: Option<ContentLink>,
}

/// One trigger firing (`GET {workflow}/triggers/{trigger}/histories`),
/// flattened across the workflow's triggers (there is usually exactly one).
#[derive(Clone, Debug)]
pub struct TriggerHistory {
    /// History id (`name`) — unique per trigger.
    pub name: String,
    pub trigger_name: String,
    /// `properties.status` — `Succeeded` / `Failed` / `Skipped`.
    pub status: String,
    /// `properties.fired` — whether this check actually started a run. A
    /// polling trigger logs a `Succeeded, fired: false` row for every empty
    /// poll, so this column is the signal/noise separator.
    pub fired: bool,
    pub start_time: Option<DateTime<Utc>>,
    pub end_time: Option<DateTime<Utc>>,
    /// `properties.run.name` — the run this firing started, when it fired.
    pub run_name: Option<String>,
    pub inputs: Option<ContentLink>,
    pub outputs: Option<ContentLink>,
}

/// Downloaded message content for one action / trigger firing. A side is
/// `None` when the source row carried no link (e.g. an action with no inputs).
#[derive(Clone, Debug, Default)]
pub struct ActionContent {
    pub inputs: Option<String>,
    pub outputs: Option<String>,
}

/// Everything in this module speaks the classic Logic Apps api-version; it is
/// the GA version for `Microsoft.Logic/workflows` runtime reads.
const LOGIC_API_VERSION: &str = "2016-06-01";

const LOGIC_APPS_KQL: &str = r#"
Resources
| where type == 'microsoft.logic/workflows'
| project id, name, type, location, resourceGroup, subscriptionId, properties
| order by name asc
"#;

/// Rows requested per history/actions page (`$top`).
const PAGE_TOP: u32 = 100;

/// `nextLink` pages followed per listing before warn-and-stop. Run history on
/// a busy workflow is effectively unbounded; 3 × [`PAGE_TOP`] recent rows is
/// plenty for a diagnostic TUI.
const MAX_PAGES: usize = 3;

/// Byte cap on one downloaded content side (inputs or outputs). Payloads over
/// the cap are skipped up front when the listing reported `contentSize`, and
/// truncated after download otherwise.
pub const CONTENT_MAX_BYTES: usize = 256 * 1024;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// All consumption Logic Apps across the given subscriptions (all reachable
/// subscriptions when the slice is empty), via one Resource Graph query.
pub async fn list_workflows(
    auth: &AzureAuth,
    subscription_ids: &[String],
) -> anyhow::Result<Vec<LogicApp>> {
    let client = ArmClient::new(auth.clone())?;
    let body = if subscription_ids.is_empty() {
        serde_json::json!({ "query": LOGIC_APPS_KQL, "options": { "$top": 1000 } })
    } else {
        serde_json::json!({
            "query": LOGIC_APPS_KQL,
            "subscriptions": subscription_ids,
            "options": { "$top": 1000 },
        })
    };
    let resp = client
        .post(
            "/providers/Microsoft.ResourceGraph/resources?api-version=2022-10-01",
            &body,
        )
        .await
        .context("resource graph: list logic apps")?;
    let rows = resp
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| anyhow!("resource graph response missing 'data' array"))?;
    if rows.len() >= 1000 {
        tracing::warn!(
            "resource graph returned 1000 logic apps; more may exist beyond the page cap"
        );
    }
    Ok(rows.iter().filter_map(parse_workflow).collect())
}

/// Run history for one workflow, newest first (the service's native order),
/// capped at [`MAX_PAGES`] × [`PAGE_TOP`] rows.
pub async fn list_runs(auth: &AzureAuth, workflow: &LogicApp) -> anyhow::Result<Vec<WorkflowRun>> {
    let client = ArmClient::new(auth.clone())?;
    let path = format!("{}/runs", workflow.id);
    let pages = get_all_pages(&client, &path, "run history", &workflow.name)
        .await
        .map_err(|e| classify_history_error(&workflow.name, e))?;
    Ok(pages
        .iter()
        .flat_map(page_values)
        .filter_map(parse_run)
        .collect())
}

/// Trigger firing history for one workflow: the trigger list first, then each
/// trigger's histories (one page per trigger — a workflow rarely has more than
/// one), merged and sorted newest first.
pub async fn list_trigger_history(
    auth: &AzureAuth,
    workflow: &LogicApp,
) -> anyhow::Result<Vec<TriggerHistory>> {
    let client = ArmClient::new(auth.clone())?;
    let triggers_path = format!("{}/triggers", workflow.id);
    let resp = client
        .get(&triggers_path, &[("api-version", LOGIC_API_VERSION)])
        .await
        .map_err(|e| classify_history_error(&workflow.name, e))?;
    let trigger_names: Vec<String> = resp
        .get("value")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();

    let mut rows = Vec::new();
    for trigger in &trigger_names {
        let path = format!(
            "{}/triggers/{}/histories",
            workflow.id,
            encode_path_segment(trigger)
        );
        let resp = client
            .get(
                &path,
                &[
                    ("api-version", LOGIC_API_VERSION),
                    ("$top", &PAGE_TOP.to_string()),
                ],
            )
            .await
            .with_context(|| format!("trigger history for {}/{trigger}", workflow.name))?;
        if let Some(arr) = resp.get("value").and_then(|v| v.as_array()) {
            rows.extend(arr.iter().filter_map(|v| parse_trigger_history(v, trigger)));
        }
    }
    // Per-trigger pages are each newest-first; merging loses that, so re-sort.
    rows.sort_by_key(|h| std::cmp::Reverse(h.start_time));
    Ok(rows)
}

/// The action breakdown of one run, in execution order (the service returns
/// reverse order; we sort by start time ascending so the table reads like the
/// workflow definition ran).
pub async fn list_run_actions(
    auth: &AzureAuth,
    workflow: &LogicApp,
    run_name: &str,
) -> anyhow::Result<Vec<RunAction>> {
    let client = ArmClient::new(auth.clone())?;
    let path = format!(
        "{}/runs/{}/actions",
        workflow.id,
        encode_path_segment(run_name)
    );
    let pages = get_all_pages(&client, &path, "run actions", &workflow.name)
        .await
        .map_err(|e| classify_history_error(&workflow.name, e))?;
    let mut actions: Vec<RunAction> = pages
        .iter()
        .flat_map(page_values)
        .filter_map(parse_action)
        .collect();
    actions.sort_by_key(|a| a.start_time);
    Ok(actions)
}

/// Download the message content behind one action / trigger firing. Each side
/// is fetched independently; a side without a link stays `None`. JSON bodies
/// are pretty-printed; anything else is passed through as (lossy) text.
pub async fn fetch_content(
    inputs: Option<&ContentLink>,
    outputs: Option<&ContentLink>,
) -> anyhow::Result<ActionContent> {
    let http = build_http()?;
    let inputs = match inputs {
        Some(link) => Some(fetch_link(&http, link).await.context("fetching inputs")?),
        None => None,
    };
    let outputs = match outputs {
        Some(link) => Some(fetch_link(&http, link).await.context("fetching outputs")?),
        None => None,
    };
    Ok(ActionContent { inputs, outputs })
}

/// Download one content link, bounded by [`CONTENT_MAX_BYTES`].
async fn fetch_link(http: &reqwest::Client, link: &ContentLink) -> anyhow::Result<String> {
    if let Some(size) = link.size {
        if size > CONTENT_MAX_BYTES as i64 {
            return Ok(format!(
                "[content {} — over the {} preview cap; open the run in the portal (o) to download it]",
                human_bytes(size as u64),
                human_bytes(CONTENT_MAX_BYTES as u64),
            ));
        }
    }
    // SAS-authenticated GET: the credential is the query string, so NO
    // Authorization header (the service rejects a request carrying both).
    let url = link.uri.clone();
    let resp = send_with_retry(|| http.get(&url)).await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(classify_content_error(status.as_u16(), &body, &link.uri));
    }
    let bytes = resp.bytes().await.context("reading content body")?;
    let truncated = bytes.len() > CONTENT_MAX_BYTES;
    let slice = &bytes[..bytes.len().min(CONTENT_MAX_BYTES)];
    // Pretty-print when the payload parses as JSON (the overwhelmingly common
    // case); otherwise show the raw (lossy-decoded) text.
    let mut text = match serde_json::from_slice::<serde_json::Value>(slice) {
        Ok(v) => serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string()),
        Err(_) => {
            let mut t = String::from_utf8_lossy(slice).into_owned();
            // Lossy decode can grow past the cap (replacement chars are 3
            // bytes); honour it like `storage::preview_blob` does.
            if t.len() > CONTENT_MAX_BYTES {
                let mut end = CONTENT_MAX_BYTES;
                while !t.is_char_boundary(end) && end > 0 {
                    end -= 1;
                }
                t.truncate(end);
            }
            t
        }
    };
    if truncated {
        text.push_str(&format!(
            "\n… [truncated at {}]",
            human_bytes(CONTENT_MAX_BYTES as u64)
        ));
    }
    Ok(text)
}

// ---------------------------------------------------------------------------
// HTTP plumbing
// ---------------------------------------------------------------------------

/// GET `first_path` and every `nextLink` page after it (same shape as
/// `service_bus.rs`), warn-and-stop at [`MAX_PAGES`].
async fn get_all_pages(
    client: &ArmClient,
    first_path: &str,
    what: &str,
    workflow_name: &str,
) -> anyhow::Result<Vec<serde_json::Value>> {
    let mut pages = Vec::new();
    let mut resp = client
        .get(
            first_path,
            &[
                ("api-version", LOGIC_API_VERSION),
                ("$top", &PAGE_TOP.to_string()),
            ],
        )
        .await?;
    loop {
        let next = next_link_path(&resp);
        pages.push(resp);
        if pages.len() >= MAX_PAGES && next.is_some() {
            tracing::warn!(
                "logic app {what} for {workflow_name}: stopping after {MAX_PAGES} pages; \
                 older rows exist beyond the cap"
            );
            break;
        }
        match next {
            // nextLink embeds api-version and the skip token in its query
            // string, so no extra query params on follow-up requests.
            Some(path) => resp = client.get(&path, &[]).await?,
            None => break,
        }
    }
    Ok(pages)
}

/// Extract a page's `nextLink` as an [`ArmClient`]-relative path. A link on a
/// foreign host is not followed (ARM never does this in practice).
fn next_link_path(resp: &serde_json::Value) -> Option<String> {
    let link = resp
        .get("nextLink")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?;
    match link.strip_prefix(ARM_BASE) {
        Some(path) => Some(path.to_string()),
        None => {
            tracing::warn!("ignoring nextLink not rooted at {ARM_BASE}");
            None
        }
    }
}

fn page_values(page: &serde_json::Value) -> impl Iterator<Item = &serde_json::Value> {
    page.get("value")
        .and_then(|v| v.as_array())
        .map(|a| a.iter())
        .into_iter()
        .flatten()
}

/// Rewrite the opaque ARM errors of the history endpoints into actionable
/// guidance. 403 on a workflow the user can *list* means the role covers
/// Resource Graph but not `Microsoft.Logic/workflows/runs/read`.
fn classify_history_error(workflow_name: &str, e: anyhow::Error) -> anyhow::Error {
    let msg = format!("{e:#}");
    if msg.contains("azure api error 403") || msg.contains("AuthorizationFailed") {
        anyhow!(
            "access denied reading {workflow_name}'s history: your role must include \
             'Microsoft.Logic/workflows/runs/read' ('Reader' or 'Logic App Operator' \
             on the workflow is enough). Underlying error: {msg}"
        )
    } else {
        e
    }
}

/// Content-link failures, with the SAS query string redacted. Expired links
/// are the common case — they are minted per listing response and only live
/// for a few minutes.
fn classify_content_error(status: u16, body: &str, uri: &str) -> anyhow::Error {
    let host = redact_sas(uri);
    if status == 403 || status == 401 {
        anyhow!(
            "content link for {host} rejected ({status}): the pre-signed link has likely \
             expired — refresh the list (r) and open the row again"
        )
    } else {
        anyhow!(
            "content link for {host} returned {status}: {}",
            truncate_error_body(body)
        )
    }
}

/// Strip the query string — it is the SAS credential — leaving only the
/// host/path for error messages.
fn redact_sas(url: &str) -> &str {
    url.split('?').next().unwrap_or(url)
}

fn truncate_error_body(s: &str) -> String {
    const LIMIT: usize = 1024;
    if s.len() <= LIMIT {
        s.to_string()
    } else {
        let mut end = LIMIT;
        while !s.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

/// Percent-encode one URL path segment (`/` NOT preserved — these are single
/// segments). Trigger and action names come from user-authored workflow
/// definitions, so odd characters are possible.
fn encode_path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn human_bytes(n: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB"];
    let mut val = n as f64;
    let mut unit = 0;
    while val >= 1024.0 && unit < UNITS.len() - 1 {
        val /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} {}", UNITS[0])
    } else {
        format!("{val:.1} {}", UNITS[unit])
    }
}

// ---------------------------------------------------------------------------
// Parsers
// ---------------------------------------------------------------------------

pub(crate) fn parse_workflow(v: &serde_json::Value) -> Option<LogicApp> {
    let ty = v.get("type")?.as_str()?.to_lowercase();
    if ty != "microsoft.logic/workflows" {
        return None;
    }
    let props = v.get("properties");
    Some(LogicApp {
        id: v.get("id")?.as_str()?.to_string(),
        name: v.get("name")?.as_str()?.to_string(),
        resource_group: v
            .get("resourceGroup")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string(),
        subscription_id: v
            .get("subscriptionId")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string(),
        location: v
            .get("location")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string(),
        state: props
            .and_then(|p| p.get("state"))
            .and_then(|s| s.as_str())
            .map(str::to_owned),
        changed_at: props.and_then(|p| parse_time(p, "changedTime")),
        created_at: props.and_then(|p| parse_time(p, "createdTime")),
    })
}

fn parse_run(v: &serde_json::Value) -> Option<WorkflowRun> {
    let props = v.get("properties")?;
    let trigger = props.get("trigger");
    Some(WorkflowRun {
        name: v.get("name")?.as_str()?.to_string(),
        status: props
            .get("status")
            .and_then(|s| s.as_str())
            .unwrap_or("Unknown")
            .to_string(),
        start_time: parse_time(props, "startTime"),
        end_time: parse_time(props, "endTime"),
        trigger_name: trigger
            .and_then(|t| t.get("name"))
            .and_then(|s| s.as_str())
            .map(str::to_owned),
        trigger_inputs: trigger.and_then(|t| parse_content_link(t, "inputsLink")),
        trigger_outputs: trigger.and_then(|t| parse_content_link(t, "outputsLink")),
        error: parse_error(props),
        correlation_id: props
            .get("correlation")
            .and_then(|c| c.get("clientTrackingId"))
            .and_then(|s| s.as_str())
            .map(str::to_owned),
    })
}

fn parse_action(v: &serde_json::Value) -> Option<RunAction> {
    let props = v.get("properties")?;
    Some(RunAction {
        name: v.get("name")?.as_str()?.to_string(),
        status: props
            .get("status")
            .and_then(|s| s.as_str())
            .unwrap_or("Unknown")
            .to_string(),
        code: props
            .get("code")
            .and_then(|s| s.as_str())
            .map(str::to_owned),
        start_time: parse_time(props, "startTime"),
        end_time: parse_time(props, "endTime"),
        error: parse_error(props),
        inputs: parse_content_link(props, "inputsLink"),
        outputs: parse_content_link(props, "outputsLink"),
    })
}

fn parse_trigger_history(v: &serde_json::Value, trigger_name: &str) -> Option<TriggerHistory> {
    let props = v.get("properties")?;
    Some(TriggerHistory {
        name: v.get("name")?.as_str()?.to_string(),
        trigger_name: trigger_name.to_string(),
        status: props
            .get("status")
            .and_then(|s| s.as_str())
            .unwrap_or("Unknown")
            .to_string(),
        fired: props
            .get("fired")
            .and_then(|b| b.as_bool())
            .unwrap_or(false),
        start_time: parse_time(props, "startTime"),
        end_time: parse_time(props, "endTime"),
        run_name: props
            .get("run")
            .and_then(|r| r.get("name"))
            .and_then(|s| s.as_str())
            .map(str::to_owned),
        inputs: parse_content_link(props, "inputsLink"),
        outputs: parse_content_link(props, "outputsLink"),
    })
}

fn parse_content_link(parent: &serde_json::Value, key: &str) -> Option<ContentLink> {
    let link = parent.get(key)?;
    Some(ContentLink {
        uri: link.get("uri")?.as_str()?.to_string(),
        size: link.get("contentSize").and_then(|s| s.as_i64()),
    })
}

/// `properties.error` → `"CODE: message"` (either half may be absent).
fn parse_error(props: &serde_json::Value) -> Option<String> {
    let err = props.get("error")?;
    let code = err.get("code").and_then(|s| s.as_str()).unwrap_or_default();
    let message = err
        .get("message")
        .and_then(|s| s.as_str())
        .unwrap_or_default();
    match (code.is_empty(), message.is_empty()) {
        (true, true) => None,
        (false, true) => Some(code.to_string()),
        (true, false) => Some(message.to_string()),
        (false, false) => Some(format!("{code}: {message}")),
    }
}

fn parse_time(props: &serde_json::Value, key: &str) -> Option<DateTime<Utc>> {
    props
        .get(key)
        .and_then(|s| s.as_str())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_workflow_row() {
        let row = json!({
            "id": "/subscriptions/sub1/resourceGroups/rg1/providers/Microsoft.Logic/workflows/wf-orders",
            "name": "wf-orders",
            "type": "microsoft.logic/workflows",
            "location": "westeurope",
            "resourceGroup": "rg1",
            "subscriptionId": "sub1",
            "properties": {
                "state": "Enabled",
                "createdTime": "2024-01-05T09:00:00Z",
                "changedTime": "2026-02-11T10:30:00Z"
            }
        });
        let wf = parse_workflow(&row).expect("parses");
        assert_eq!(wf.name, "wf-orders");
        assert_eq!(wf.state.as_deref(), Some("Enabled"));
        assert!(wf.changed_at.is_some());
        // Non-workflow rows are skipped.
        let other = json!({"id": "/x", "name": "n", "type": "microsoft.web/sites"});
        assert!(parse_workflow(&other).is_none());
    }

    #[test]
    fn parses_run_with_trigger_links_and_error() {
        let row = json!({
            "name": "08585287554104334735",
            "properties": {
                "status": "Failed",
                "startTime": "2026-08-13T11:00:00Z",
                "endTime": "2026-08-13T11:00:04Z",
                "correlation": { "clientTrackingId": "corr-1" },
                "trigger": {
                    "name": "When_a_message_arrives",
                    "inputsLink": { "uri": "https://prod-01.westeurope.logic.azure.com/in?sig=SECRET", "contentSize": 512 },
                    "outputsLink": { "uri": "https://prod-01.westeurope.logic.azure.com/out?sig=SECRET", "contentSize": 1024 }
                },
                "error": { "code": "ActionFailed", "message": "An action failed." }
            }
        });
        let run = parse_run(&row).expect("parses");
        assert_eq!(run.status, "Failed");
        assert_eq!(run.trigger_name.as_deref(), Some("When_a_message_arrives"));
        assert_eq!(run.trigger_inputs.as_ref().unwrap().size, Some(512));
        assert_eq!(
            run.error.as_deref(),
            Some("ActionFailed: An action failed.")
        );
        assert_eq!(run.correlation_id.as_deref(), Some("corr-1"));
    }

    #[test]
    fn parses_action_and_trigger_history() {
        let action = json!({
            "name": "Parse_JSON",
            "properties": {
                "status": "Succeeded",
                "code": "OK",
                "startTime": "2026-08-13T11:00:01Z",
                "endTime": "2026-08-13T11:00:02Z",
                "inputsLink": { "uri": "https://x/in?sig=S", "contentSize": 64 }
            }
        });
        let a = parse_action(&action).expect("parses");
        assert_eq!(a.code.as_deref(), Some("OK"));
        assert!(a.inputs.is_some() && a.outputs.is_none());

        let hist = json!({
            "name": "08585287554104334735936",
            "properties": {
                "status": "Succeeded",
                "fired": true,
                "startTime": "2026-08-13T11:00:00Z",
                "run": { "name": "08585287554104334735" }
            }
        });
        let h = parse_trigger_history(&hist, "Recurrence").expect("parses");
        assert!(h.fired);
        assert_eq!(h.trigger_name, "Recurrence");
        assert_eq!(h.run_name.as_deref(), Some("08585287554104334735"));
    }

    #[test]
    fn redact_sas_strips_query_string() {
        assert_eq!(
            redact_sas("https://host/path?sv=1&sig=SECRET"),
            "https://host/path"
        );
        assert_eq!(redact_sas("https://host/path"), "https://host/path");
    }

    #[test]
    fn error_flattening_handles_partial_shapes() {
        let both = json!({"error": {"code": "C", "message": "m"}});
        assert_eq!(parse_error(&both).as_deref(), Some("C: m"));
        let code_only = json!({"error": {"code": "C"}});
        assert_eq!(parse_error(&code_only).as_deref(), Some("C"));
        let none = json!({});
        assert!(parse_error(&none).is_none());
    }

    #[test]
    fn encode_path_segment_escapes_reserved_chars() {
        assert_eq!(encode_path_segment("Parse_JSON"), "Parse_JSON");
        assert_eq!(encode_path_segment("a b/c"), "a%20b%2Fc");
    }
}
