//! Resource-centric Log Analytics queries:
//! `POST https://api.loganalytics.io/v1{resourceId}/query`
//!
//! Works as long as the resource has diagnostic settings forwarding to a
//! workspace; we do not need to discover the workspace ID separately.

#![allow(dead_code, unused_variables)]

use anyhow::anyhow;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::azure::auth::AzureAuth;
use crate::azure::client::LogsClient;
use crate::azure::metrics::TimeRange;
use crate::azure::resources::{Resource, ResourceKind};
use crate::error::AzpectError;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum LogLevel {
    Trace,
    Info,
    Warn,
    Error,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogLine {
    pub ts: DateTime<Utc>,
    pub level: LogLevel,
    /// Origin table or signal name, e.g. `AppExceptions`, `AppRequests`,
    /// `ContainerAppConsoleLogs` (or its legacy `_CL` variant).
    pub source: String,
    pub message: String,
    /// Every (column, value) pair from the originating Log Analytics row.
    /// Preserves response order so the log-detail view can render columns
    /// in a stable, predictable layout. Empty values are dropped at parse
    /// time to avoid cluttering the detail view with blank entries.
    #[serde(default)]
    pub fields: Vec<(String, String)>,
}

/// Whether we know how to query logs for this resource type. APIM and
/// Application Gateway are `false` in v1.
pub fn supports_logs(kind: ResourceKind) -> bool {
    matches!(kind, ResourceKind::FunctionApp | ResourceKind::ContainerApp)
}

/// KQL templates per resource type. Lane 2 appends the errors-only filter when requested.
///
/// `isfuzzy=true` makes Log Analytics skip tables that don't exist in the
/// workspace instead of hard-failing with SEM0100. The union mixes the
/// workspace-based Application Insights tables (`AppTraces`, `AppExceptions`,
/// `AppRequests`) with `FunctionAppLogs`, which is what the Function App
/// resource ships when only diagnostic settings are configured (no AI, or
/// classic AI). At least one of the four must resolve or KQL returns SEM0529.
///
/// `{limit}` is substituted at build time. Sizing the cap to the time range
/// (see [`row_limit`]) keeps a low-volume 1h window cheap while letting a
/// 1d / 7d window actually span the requested duration — otherwise `G` lands
/// at the bottom of the buffer, not the bottom of the window.
pub const KQL_FUNCTION_APP: &str = r#"
union isfuzzy=true AppTraces, AppExceptions, AppRequests, FunctionAppLogs
| order by TimeGenerated desc
| take {limit}
"#;

/// `Level` covers `FunctionAppLogs`; `SeverityLevel`/`itemType`/`Success` cover
/// the workspace-based AI tables. `column_ifexists` is required because when
/// none of the AI tables resolve (e.g. a Function App that only ships
/// `FunctionAppLogs`), referencing `SeverityLevel`/`Success`/`itemType`
/// directly fails the whole query with SEM0100 — the columns exist on no
/// resolved source. `column_ifexists` substitutes a default in that case.
pub const KQL_FUNCTION_APP_ERRORS_FILTER: &str = r#"
| where column_ifexists("SeverityLevel", int(0)) >= 3
     or (column_ifexists("Success", true) == false and column_ifexists("itemType", "") == "request")
     or column_ifexists("itemType", "") == "exception"
     or column_ifexists("Level", "") in ("Error", "Critical")
"#;

/// Container Apps land in one of two console-log tables depending on the
/// environment's `appLogsConfiguration` destination: `ContainerAppConsoleLogs_CL`
/// (legacy "Log Analytics" destination, column `Log_s` + `ContainerAppName_s`)
/// or `ContainerAppConsoleLogs` (modern Azure Monitor "Resource specific"
/// destination, columns `Log` + `ContainerAppName`). `isfuzzy=true` lets the
/// query succeed when only one of them is populated.
///
/// Unlike Function Apps / APIM, Container Apps don't carry per-resource
/// diagnostic settings — logs are forwarded by the parent Container Apps
/// Environment. The resource-centric Log Analytics endpoint therefore resolves
/// to an empty scope, so we query the workspace directly and filter by
/// `ContainerAppName_s` / `ContainerAppName` to scope to one app. The
/// `{name}` placeholder is substituted at build time.
pub const KQL_CONTAINER_APP_TEMPLATE: &str = r#"
union isfuzzy=true ContainerAppConsoleLogs_CL, ContainerAppConsoleLogs
| where column_ifexists("ContainerAppName_s", "") == "{name}"
     or column_ifexists("ContainerAppName", "") == "{name}"
| order by TimeGenerated desc
| take {limit}
"#;

/// `column_ifexists` is required because the two unioned tables don't share
/// a column name for the log body (`Log_s` vs `Log`), and referencing the
/// missing one would fail the whole query with SEM0100.
pub const KQL_CONTAINER_APP_ERRORS_FILTER: &str = r#"
| where column_ifexists("Log_s", "") matches regex @"(?i)\b(error|exception|fatal|panic|stack)\b"
     or column_ifexists("Log", "") matches regex @"(?i)\b(error|exception|fatal|panic|stack)\b"
"#;

/// Maximum length of `LogLine.message` before truncation.
const MESSAGE_TRUNCATE: usize = 500;

/// Rows per fetch. Each page round-trips through the workspace, so smaller
/// pages keep the first paint snappy; the UI calls [`fetch`] with an
/// `older_than` cursor whenever the user scrolls past the bottom, so the
/// effective coverage of a 1d / 7d window is unbounded by this constant.
///
/// A returned page with exactly `PAGE_SIZE` rows is taken as the signal that
/// older data may still exist (`has_more = true`). Fewer rows → we've hit
/// the start of the window.
pub const PAGE_SIZE: u32 = 500;

/// One page of log rows plus whether the workspace likely has more older rows
/// in the same window. `has_more` is the page-saturation heuristic: a fetch
/// that came back full is taken as "older rows still exist," partial → done.
#[derive(Debug, Default)]
pub struct LogsPage {
    pub lines: Vec<LogLine>,
    pub has_more: bool,
}

pub async fn fetch(
    auth: &AzureAuth,
    resource: &Resource,
    range: TimeRange,
    errors_only: bool,
    older_than: Option<DateTime<Utc>>,
) -> anyhow::Result<LogsPage> {
    if !supports_logs(resource.kind) {
        return Err(anyhow!(AzpectError::UnsupportedMetric(format!(
            "logs not supported for {:?}",
            resource.kind
        ))));
    }

    let kql = build_kql(resource, errors_only, older_than)?;
    let timespan = range.timespan();

    let client = LogsClient::new(auth.clone())?;
    let resp = match resource.kind {
        ResourceKind::ContainerApp => {
            let customer_id =
                crate::azure::container_app_workspace::resolve(auth, &resource.id).await?;
            client
                .query_workspace(&customer_id, &kql, &timespan)
                .await?
        }
        _ => client.query(&resource.id, &kql, &timespan).await?,
    };

    let lines = parse_logs_response(&resp, resource.kind)?;
    let has_more = lines.len() as u32 >= PAGE_SIZE;
    Ok(LogsPage { lines, has_more })
}

/// Splice the errors-only filter (and an `older_than` cursor for pagination)
/// in BEFORE the `| order by` clause. Both filters land in the same spot so
/// the order by / take still see the narrowest possible row set.
fn build_kql(
    resource: &Resource,
    errors_only: bool,
    older_than: Option<DateTime<Utc>>,
) -> anyhow::Result<String> {
    let (template, errors_filter) = match resource.kind {
        ResourceKind::FunctionApp => (KQL_FUNCTION_APP.to_string(), KQL_FUNCTION_APP_ERRORS_FILTER),
        ResourceKind::ContainerApp => (
            container_app_kql(&resource.name),
            KQL_CONTAINER_APP_ERRORS_FILTER,
        ),
        ResourceKind::Apim | ResourceKind::AppGateway => {
            return Err(anyhow!(
                "logs not supported for {:?} in v1 (no resource-centric Log Analytics template)",
                resource.kind
            ));
        }
    };

    let template = template.replace("{limit}", &PAGE_SIZE.to_string());

    // Build the filter block (zero, one, or two clauses) and splice it before
    // `| order by`. Each clause is a full `| where …` line; concatenated they
    // form a contiguous filter section.
    let mut filter_block = String::new();
    if errors_only {
        filter_block.push_str(errors_filter.trim());
        filter_block.push('\n');
    }
    if let Some(ts) = older_than {
        // RFC3339 with microsecond precision keeps cursor strictly older than
        // the last received row even if two rows landed in the same second.
        let stamp = ts.to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
        filter_block.push_str(&format!(r#"| where TimeGenerated < datetime("{stamp}")"#));
        filter_block.push('\n');
    }

    if filter_block.is_empty() {
        return Ok(template);
    }

    if let Some(idx) = template.find("| order by") {
        let mut spliced = String::with_capacity(template.len() + filter_block.len());
        spliced.push_str(&template[..idx]);
        spliced.push_str(&filter_block);
        spliced.push_str(&template[idx..]);
        Ok(spliced)
    } else {
        Ok(format!("{template}\n{filter_block}"))
    }
}

/// Substitute the container app name into the template. Container App names
/// per Azure are `[a-z][a-z0-9-]{1,31}`, so they can't contain quotes, but
/// we backslash-escape defensively in case an aliased / renamed resource
/// somehow violates that.
fn container_app_kql(name: &str) -> String {
    let escaped = name.replace('\\', r"\\").replace('"', r#"\""#);
    KQL_CONTAINER_APP_TEMPLATE.replace("{name}", &escaped)
}

fn parse_logs_response(
    value: &serde_json::Value,
    kind: ResourceKind,
) -> anyhow::Result<Vec<LogLine>> {
    let table = value
        .get("tables")
        .and_then(|t| t.as_array())
        .and_then(|a| a.first());

    let table = match table {
        Some(t) => t,
        // Some "no data" responses come back without a tables array at all.
        None => return Err(anyhow!(AzpectError::NoLogDestination)),
    };

    let columns = table
        .get("columns")
        .and_then(|c| c.as_array())
        .ok_or_else(|| anyhow!("logs response table missing 'columns'"))?;

    let col_names: Vec<&str> = columns
        .iter()
        .map(|c| c.get("name").and_then(|n| n.as_str()).unwrap_or(""))
        .collect();

    let rows = table
        .get("rows")
        .and_then(|r| r.as_array())
        .ok_or_else(|| anyhow!("logs response table missing 'rows'"))?;

    // Empty rows just means no log lines in the requested window
    // (quiet period, errors-only filter excluded everything, etc.).
    // `NoLogDestination` is reserved for the missing-tables case above.
    if rows.is_empty() {
        return Ok(Vec::new());
    }

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let cells = match row.as_array() {
            Some(c) => c,
            None => continue,
        };
        if let Some(line) = parse_row(&col_names, cells, kind) {
            out.push(line);
        }
    }
    Ok(out)
}

fn cell<'a>(
    columns: &[&str],
    cells: &'a [serde_json::Value],
    name: &str,
) -> Option<&'a serde_json::Value> {
    columns
        .iter()
        .position(|c| *c == name)
        .and_then(|i| cells.get(i))
}

fn parse_row(columns: &[&str], cells: &[serde_json::Value], kind: ResourceKind) -> Option<LogLine> {
    let ts_str = cell(columns, cells, "TimeGenerated").and_then(|v| v.as_str())?;
    let ts = DateTime::parse_from_rfc3339(ts_str)
        .ok()?
        .with_timezone(&Utc);

    let (level, source, message) = match kind {
        ResourceKind::FunctionApp => parse_function_app_row(columns, cells),
        ResourceKind::ContainerApp => parse_container_app_row(columns, cells),
        // Unreachable in practice — supports_logs() filters these out
        // before fetch() is ever called.
        ResourceKind::Apim | ResourceKind::AppGateway => return None,
    };

    let message = truncate(message, MESSAGE_TRUNCATE);
    let fields = collect_fields(columns, cells);

    Some(LogLine {
        ts,
        level,
        source,
        message,
        fields,
    })
}

/// Capture every non-empty (column, value) pair from the row, skipping the
/// timestamp column (already exposed as `LogLine::ts`). JSON booleans and
/// numbers are stringified so the detail view can render them uniformly.
fn collect_fields(columns: &[&str], cells: &[serde_json::Value]) -> Vec<(String, String)> {
    let mut out = Vec::with_capacity(columns.len());
    for (name, value) in columns.iter().zip(cells.iter()) {
        if name.is_empty() || *name == "TimeGenerated" {
            continue;
        }
        let s = match value {
            serde_json::Value::Null => continue,
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Bool(b) => b.to_string(),
            serde_json::Value::Number(n) => n.to_string(),
            other => other.to_string(),
        };
        if s.trim().is_empty() {
            continue;
        }
        out.push(((*name).to_string(), s));
    }
    out
}

fn parse_function_app_row(
    columns: &[&str],
    cells: &[serde_json::Value],
) -> (LogLevel, String, String) {
    let item_type = cell(columns, cells, "itemType")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // FunctionAppLogs rows have `Level` (string) and no `itemType`. Detect
    // them and route to a dedicated extractor so we don't lose the level
    // signal or display them as generic "AppLogs".
    let level_str = cell(columns, cells, "Level")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if item_type.is_empty() && !level_str.is_empty() {
        return parse_function_app_logs_row(columns, cells, level_str);
    }

    let severity = cell(columns, cells, "SeverityLevel")
        .and_then(|v| v.as_i64())
        .unwrap_or(1);

    let level = if item_type.eq_ignore_ascii_case("exception") {
        LogLevel::Error
    } else {
        match severity {
            i if i >= 3 => LogLevel::Error,
            2 => LogLevel::Warn,
            _ => LogLevel::Info,
        }
    };

    let source = if item_type.is_empty() {
        "AppLogs".to_string()
    } else {
        // E.g. itemType "trace" → "AppTraces" for display friendliness.
        match item_type.to_lowercase().as_str() {
            "trace" => "AppTraces".to_string(),
            "exception" => "AppExceptions".to_string(),
            "request" => "AppRequests".to_string(),
            other => other.to_string(),
        }
    };

    let message = cell(columns, cells, "Message")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            cell(columns, cells, "OuterMessage")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .or_else(|| {
            cell(columns, cells, "InnermostMessage")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_default();

    (level, source, message)
}

/// Extract a row that came from the `FunctionAppLogs` diagnostic-settings
/// table. Schema: `Level` (string), `Message`, `FunctionName`, `Category`.
fn parse_function_app_logs_row(
    columns: &[&str],
    cells: &[serde_json::Value],
    level_str: &str,
) -> (LogLevel, String, String) {
    let level = match level_str {
        "Critical" | "Error" => LogLevel::Error,
        "Warning" => LogLevel::Warn,
        "Trace" | "Debug" => LogLevel::Trace,
        _ => LogLevel::Info,
    };

    let function_name = cell(columns, cells, "FunctionName")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let source = if function_name.is_empty() {
        "FunctionAppLogs".to_string()
    } else {
        format!("FunctionAppLogs/{function_name}")
    };

    // For exception rows the runtime leaves `Message` empty and writes the
    // real detail to `ExceptionMessage` (one-liner) or `ExceptionDetails`
    // (full stack trace). Fall back through them in order of usefulness so an
    // empty Message doesn't hide the actual error.
    let message = first_non_empty(
        columns,
        cells,
        &["Message", "ExceptionMessage", "ExceptionDetails"],
    );

    (level, source, message)
}

/// Return the first non-empty string value across the given column names.
fn first_non_empty(columns: &[&str], cells: &[serde_json::Value], names: &[&str]) -> String {
    for name in names {
        if let Some(s) = cell(columns, cells, name).and_then(|v| v.as_str()) {
            let t = s.trim();
            if !t.is_empty() {
                return s.to_string();
            }
        }
    }
    String::new()
}

fn parse_container_app_row(
    columns: &[&str],
    cells: &[serde_json::Value],
) -> (LogLevel, String, String) {
    // `Log_s` is the legacy `_CL` schema; `Log` is the modern Azure Monitor
    // resource-specific schema. Whichever table the row came from, only one
    // of these is populated.
    let log = first_non_empty(columns, cells, &["Log_s", "Log"]);

    let lower = log.to_lowercase();
    let is_error = ["error", "exception", "fatal", "panic"]
        .iter()
        .any(|kw| lower.contains(kw));
    let level = if is_error {
        LogLevel::Error
    } else {
        LogLevel::Info
    };

    // Log Analytics injects the originating table name into the `Type` column
    // for every row, so the source label tracks which schema we're reading.
    let source = cell(columns, cells, "Type")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("ContainerAppConsoleLogs")
        .to_string();
    (level, source, log)
}

fn truncate(s: String, max: usize) -> String {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    let mut t = String::with_capacity(end + 1);
    t.push_str(&s[..end]);
    t.push('…');
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_resource(kind: ResourceKind, name: &str) -> Resource {
        Resource {
            id: "/r/x".into(),
            name: name.into(),
            kind,
            location: "westeurope".into(),
            resource_group: "rg".into(),
            subscription_id: "sub".into(),
            state: None,
            created_at: None,
            modified_at: None,
            meta: Default::default(),
        }
    }

    #[test]
    fn errors_filter_inserted_before_order_by() {
        let r = test_resource(ResourceKind::FunctionApp, "func");
        let kql = build_kql(&r, true, None).unwrap();
        let order_idx = kql.find("| order by").expect("order by present");
        let filter_idx = kql
            .find(r#"column_ifexists("SeverityLevel", int(0)) >= 3"#)
            .expect("filter present");
        assert!(filter_idx < order_idx, "filter must come before order by");
    }

    #[test]
    fn no_filter_when_errors_only_false() {
        let r = test_resource(ResourceKind::ContainerApp, "my-app");
        let kql = build_kql(&r, false, None).unwrap();
        assert!(!kql.contains("matches regex"));
    }

    #[test]
    fn apim_kql_returns_err() {
        let r = test_resource(ResourceKind::Apim, "apim");
        assert!(build_kql(&r, false, None).is_err());
    }

    #[test]
    fn empty_rows_yields_empty_vec() {
        let payload = json!({
            "tables": [
                { "name": "PrimaryResult", "columns": [{"name": "TimeGenerated", "type": "datetime"}], "rows": [] }
            ]
        });
        let lines = parse_logs_response(&payload, ResourceKind::FunctionApp).unwrap();
        assert!(lines.is_empty());
    }

    #[test]
    fn missing_tables_yields_no_log_destination() {
        let payload = json!({});
        let err = parse_logs_response(&payload, ResourceKind::FunctionApp).unwrap_err();
        assert!(err
            .downcast_ref::<AzpectError>()
            .map(|e| matches!(e, AzpectError::NoLogDestination))
            .unwrap_or(false));
    }

    #[test]
    fn parses_function_app_logs_row_when_only_level_present() {
        // FunctionAppLogs schema: Level + Message + FunctionName, no itemType.
        let payload = json!({
            "tables": [{
                "name": "PrimaryResult",
                "columns": [
                    { "name": "TimeGenerated", "type": "datetime" },
                    { "name": "Level", "type": "string" },
                    { "name": "Message", "type": "string" },
                    { "name": "FunctionName", "type": "string" }
                ],
                "rows": [
                    [ "2026-01-01T00:00:00Z", "Error", "kaboom in handler", "ProcessOrder" ],
                    [ "2026-01-01T00:01:00Z", "Information", "started", "" ]
                ]
            }]
        });
        let lines = parse_logs_response(&payload, ResourceKind::FunctionApp).unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].level, LogLevel::Error);
        assert_eq!(lines[0].source, "FunctionAppLogs/ProcessOrder");
        assert_eq!(lines[0].message, "kaboom in handler");
        assert_eq!(lines[1].level, LogLevel::Info);
        assert_eq!(lines[1].source, "FunctionAppLogs");
    }

    #[test]
    fn function_app_logs_falls_back_to_exception_fields_when_message_is_empty() {
        // Real Azure shape for a Function App invocation failure: the runtime
        // writes the summary into one row (Message populated) and the actual
        // exception into a sibling row where Message is empty and the body
        // lives in ExceptionMessage / ExceptionDetails.
        let payload = json!({
            "tables": [{
                "name": "PrimaryResult",
                "columns": [
                    { "name": "TimeGenerated", "type": "datetime" },
                    { "name": "Level", "type": "string" },
                    { "name": "Message", "type": "string" },
                    { "name": "ExceptionMessage", "type": "string" },
                    { "name": "ExceptionDetails", "type": "string" },
                    { "name": "FunctionName", "type": "string" }
                ],
                "rows": [
                    // Has ExceptionMessage but Message is empty
                    [ "2026-01-01T00:00:00Z", "Error", "", "Null reference at line 42", "stack…", "http_app_func" ],
                    // Both empty except ExceptionDetails
                    [ "2026-01-01T00:00:01Z", "Error", "   ", "", "System.IO.IOException: file locked\n  at …", "http_app_func" ]
                ]
            }]
        });
        let lines = parse_logs_response(&payload, ResourceKind::FunctionApp).unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].message, "Null reference at line 42");
        assert!(
            lines[1].message.starts_with("System.IO.IOException"),
            "ExceptionDetails should be used when Message and ExceptionMessage are blank, got {:?}",
            lines[1].message,
        );
    }

    #[test]
    fn errors_filter_includes_function_app_logs_level() {
        let r = test_resource(ResourceKind::FunctionApp, "func");
        let kql = build_kql(&r, true, None).unwrap();
        assert!(
            kql.contains(r#"column_ifexists("Level", "") in ("Error", "Critical")"#),
            "errors-only filter must catch FunctionAppLogs Error/Critical rows"
        );
    }

    #[test]
    fn function_app_kql_uses_fuzzy_union_with_function_app_logs() {
        let r = test_resource(ResourceKind::FunctionApp, "func");
        let kql = build_kql(&r, false, None).unwrap();
        assert!(kql.contains("isfuzzy=true"));
        assert!(kql.contains("FunctionAppLogs"));
    }

    #[test]
    fn parses_function_app_row_with_severity() {
        let payload = json!({
            "tables": [{
                "name": "PrimaryResult",
                "columns": [
                    { "name": "TimeGenerated", "type": "datetime" },
                    { "name": "SeverityLevel", "type": "int" },
                    { "name": "itemType", "type": "string" },
                    { "name": "Message", "type": "string" }
                ],
                "rows": [
                    [ "2026-01-01T00:00:00Z", 3, "trace", "boom" ],
                    [ "2026-01-01T00:01:00Z", 1, "request", "ok" ]
                ]
            }]
        });
        let lines = parse_logs_response(&payload, ResourceKind::FunctionApp).unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].level, LogLevel::Error);
        assert_eq!(lines[0].source, "AppTraces");
        assert_eq!(lines[0].message, "boom");
        assert_eq!(lines[1].level, LogLevel::Info);
        assert_eq!(lines[1].source, "AppRequests");
    }

    #[test]
    fn parses_container_app_row_marks_errors() {
        let payload = json!({
            "tables": [{
                "name": "PrimaryResult",
                "columns": [
                    { "name": "TimeGenerated", "type": "datetime" },
                    { "name": "Log_s", "type": "string" }
                ],
                "rows": [
                    [ "2026-01-01T00:00:00Z", "Some random INFO line" ],
                    [ "2026-01-01T00:00:01Z", "FATAL: process exiting" ]
                ]
            }]
        });
        let lines = parse_logs_response(&payload, ResourceKind::ContainerApp).unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].level, LogLevel::Info);
        assert_eq!(lines[1].level, LogLevel::Error);
        assert!(lines[1].message.contains("FATAL"));
    }

    #[test]
    fn parses_container_app_row_modern_schema() {
        // Resource-specific destination: column is `Log`, not `Log_s`, and
        // `Type` carries the originating table name.
        let payload = json!({
            "tables": [{
                "name": "PrimaryResult",
                "columns": [
                    { "name": "TimeGenerated", "type": "datetime" },
                    { "name": "Log", "type": "string" },
                    { "name": "Type", "type": "string" }
                ],
                "rows": [
                    [ "2026-01-01T00:00:00Z", "startup ok", "ContainerAppConsoleLogs" ],
                    [ "2026-01-01T00:00:01Z", "panic: nil pointer", "ContainerAppConsoleLogs" ]
                ]
            }]
        });
        let lines = parse_logs_response(&payload, ResourceKind::ContainerApp).unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].source, "ContainerAppConsoleLogs");
        assert_eq!(lines[0].message, "startup ok");
        assert_eq!(lines[1].level, LogLevel::Error);
        assert!(lines[1].message.contains("panic"));
    }

    #[test]
    fn container_app_kql_uses_fuzzy_union_over_both_tables() {
        let r = test_resource(ResourceKind::ContainerApp, "my-app");
        let kql = build_kql(&r, false, None).unwrap();
        assert!(kql.contains("isfuzzy=true"));
        assert!(kql.contains("ContainerAppConsoleLogs_CL"));
        // Modern table appears followed by newline+whitespace, not `_CL`.
        assert!(kql.contains("ContainerAppConsoleLogs\n"));
    }

    #[test]
    fn container_app_kql_filters_by_resource_name() {
        // Workspace-centric path requires a name filter; otherwise the whole
        // workspace's container apps come back interleaved.
        let r = test_resource(ResourceKind::ContainerApp, "files-api");
        let kql = build_kql(&r, false, None).unwrap();
        assert!(kql.contains(r#"column_ifexists("ContainerAppName_s", "") == "files-api""#));
        assert!(kql.contains(r#"column_ifexists("ContainerAppName", "") == "files-api""#));
    }

    #[test]
    fn container_app_kql_escapes_double_quote_in_name() {
        // Defensive: a name containing `"` would otherwise close the KQL
        // string literal and let the rest be parsed as KQL.
        let r = test_resource(ResourceKind::ContainerApp, r#"bad"name"#);
        let kql = build_kql(&r, false, None).unwrap();
        assert!(kql.contains(r#"bad\"name"#));
    }

    #[test]
    fn container_app_errors_filter_uses_column_ifexists_for_both_log_columns() {
        let r = test_resource(ResourceKind::ContainerApp, "my-app");
        let kql = build_kql(&r, true, None).unwrap();
        assert!(kql.contains(r#"column_ifexists("Log_s", "")"#));
        assert!(kql.contains(r#"column_ifexists("Log", "")"#));
    }

    #[test]
    fn truncates_long_message() {
        let big = "x".repeat(1000);
        let t = truncate(big, MESSAGE_TRUNCATE);
        // "…" is 3 bytes, so length = 500 + 3.
        assert!(t.len() <= MESSAGE_TRUNCATE + 3);
        assert!(t.ends_with('…'));
    }

    #[test]
    fn build_kql_splices_older_than_cursor_before_order_by() {
        let r = test_resource(ResourceKind::ContainerApp, "files-api");
        let cursor = DateTime::parse_from_rfc3339("2026-05-19T03:17:43.123456Z")
            .unwrap()
            .with_timezone(&Utc);
        let kql = build_kql(&r, false, Some(cursor)).unwrap();
        let where_idx = kql
            .find("| where TimeGenerated < datetime(")
            .expect("cursor clause present");
        let order_idx = kql.find("| order by").expect("order by present");
        assert!(
            where_idx < order_idx,
            "cursor must filter before order by/take, got:\n{kql}"
        );
        assert!(
            kql.contains(r#"datetime("2026-05-19T03:17:43.123456Z")"#),
            "expected microsecond-precision RFC3339 cursor, got:\n{kql}"
        );
    }

    #[test]
    fn build_kql_combines_errors_filter_and_older_than() {
        // Both clauses must appear, both before order by, both as their own
        // `| where …` lines so KQL parses them as filters in sequence.
        let r = test_resource(ResourceKind::FunctionApp, "func");
        let cursor = DateTime::parse_from_rfc3339("2026-05-19T01:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let kql = build_kql(&r, true, Some(cursor)).unwrap();
        let order_idx = kql.find("| order by").expect("order by present");
        let errors_idx = kql
            .find(r#"column_ifexists("SeverityLevel", int(0)) >= 3"#)
            .expect("errors filter present");
        let cursor_idx = kql.find("| where TimeGenerated <").expect("cursor present");
        assert!(errors_idx < order_idx);
        assert!(cursor_idx < order_idx);
    }

    #[test]
    fn build_kql_substitutes_page_size_into_take() {
        let r = test_resource(ResourceKind::FunctionApp, "func");
        let kql = build_kql(&r, false, None).unwrap();
        assert!(
            kql.contains(&format!("take {PAGE_SIZE}")),
            "expected `take {PAGE_SIZE}` in:\n{kql}"
        );
        // The `{limit}` placeholder must not survive to the server.
        assert!(!kql.contains("{limit}"));
    }
}
