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
    /// `ContainerAppConsoleLogs_CL`.
    pub source: String,
    pub message: String,
}

/// Whether we know how to query logs for this resource type. APIM is `false` in v1.
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
pub const KQL_FUNCTION_APP: &str = r#"
union isfuzzy=true AppTraces, AppExceptions, AppRequests, FunctionAppLogs
| order by TimeGenerated desc
| take 200
"#;

/// `Level` covers `FunctionAppLogs`; `SeverityLevel`/`itemType`/`Success` cover
/// the workspace-based AI tables. Missing columns evaluate to null in a fuzzy
/// union, so each clause only matches rows from the table that actually has it.
pub const KQL_FUNCTION_APP_ERRORS_FILTER: &str = r#"
| where SeverityLevel >= 3 or (Success == false and itemType == "request") or itemType == "exception" or Level in ("Error", "Critical")
"#;

pub const KQL_CONTAINER_APP: &str = r#"
ContainerAppConsoleLogs_CL
| order by TimeGenerated desc
| take 200
"#;

pub const KQL_CONTAINER_APP_ERRORS_FILTER: &str =
    r#"| where Log_s matches regex @"(?i)\b(error|exception|fatal|panic|stack)\b""#;

/// Maximum length of `LogLine.message` before truncation.
const MESSAGE_TRUNCATE: usize = 500;

pub async fn fetch(
    auth: &AzureAuth,
    resource: &Resource,
    range: TimeRange,
    errors_only: bool,
) -> anyhow::Result<Vec<LogLine>> {
    if !supports_logs(resource.kind) {
        return Err(anyhow!(AzpectError::UnsupportedMetric(format!(
            "logs not supported for {:?}",
            resource.kind
        ))));
    }

    let kql = build_kql(resource.kind, errors_only)?;
    let timespan = range.timespan();

    let client = LogsClient::new(auth.clone())?;
    let resp = client.query(&resource.id, &kql, &timespan).await?;

    parse_logs_response(&resp, resource.kind)
}

/// Splice the errors-only filter in BEFORE the `| order by` clause if requested.
fn build_kql(kind: ResourceKind, errors_only: bool) -> anyhow::Result<String> {
    let (template, filter) = match kind {
        ResourceKind::FunctionApp => (KQL_FUNCTION_APP, KQL_FUNCTION_APP_ERRORS_FILTER),
        ResourceKind::ContainerApp => (KQL_CONTAINER_APP, KQL_CONTAINER_APP_ERRORS_FILTER),
        ResourceKind::Apim => {
            return Err(anyhow!(
                "APIM logs not supported in v1 (no resource-centric Log Analytics template)"
            ));
        }
    };

    if !errors_only {
        return Ok(template.to_string());
    }

    // Find `| order by` and splice the filter just above it. Falls back to appending if absent.
    if let Some(idx) = template.find("| order by") {
        let mut spliced = String::with_capacity(template.len() + filter.len() + 1);
        spliced.push_str(&template[..idx]);
        spliced.push_str(filter.trim());
        spliced.push('\n');
        spliced.push_str(&template[idx..]);
        Ok(spliced)
    } else {
        Ok(format!("{template}\n{filter}"))
    }
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
        ResourceKind::Apim => return None, // unreachable in practice
    };

    let message = truncate(message, MESSAGE_TRUNCATE);

    Some(LogLine {
        ts,
        level,
        source,
        message,
    })
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

    let message = cell(columns, cells, "Message")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    (level, source, message)
}

fn parse_container_app_row(
    columns: &[&str],
    cells: &[serde_json::Value],
) -> (LogLevel, String, String) {
    let log = cell(columns, cells, "Log_s")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let lower = log.to_lowercase();
    let is_error = ["error", "exception", "fatal", "panic"]
        .iter()
        .any(|kw| lower.contains(kw));
    let level = if is_error {
        LogLevel::Error
    } else {
        LogLevel::Info
    };

    let source = "ContainerAppConsoleLogs_CL".to_string();
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

    #[test]
    fn errors_filter_inserted_before_order_by() {
        let kql = build_kql(ResourceKind::FunctionApp, true).unwrap();
        let order_idx = kql.find("| order by").expect("order by present");
        let filter_idx = kql.find("SeverityLevel >= 3").expect("filter present");
        assert!(filter_idx < order_idx, "filter must come before order by");
    }

    #[test]
    fn no_filter_when_errors_only_false() {
        let kql = build_kql(ResourceKind::ContainerApp, false).unwrap();
        assert!(!kql.contains("matches regex"));
    }

    #[test]
    fn apim_kql_returns_err() {
        assert!(build_kql(ResourceKind::Apim, false).is_err());
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
    fn errors_filter_includes_function_app_logs_level() {
        let kql = build_kql(ResourceKind::FunctionApp, true).unwrap();
        assert!(
            kql.contains(r#"Level in ("Error", "Critical")"#),
            "errors-only filter must catch FunctionAppLogs Error/Critical rows"
        );
    }

    #[test]
    fn function_app_kql_uses_fuzzy_union_with_function_app_logs() {
        let kql = build_kql(ResourceKind::FunctionApp, false).unwrap();
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
    fn truncates_long_message() {
        let big = "x".repeat(1000);
        let t = truncate(big, MESSAGE_TRUNCATE);
        // "…" is 3 bytes, so length = 500 + 3.
        assert!(t.len() <= MESSAGE_TRUNCATE + 3);
        assert!(t.ends_with('…'));
    }
}
