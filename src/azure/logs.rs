//! Resource-centric Log Analytics queries:
//! `POST https://api.loganalytics.io/v1{resourceId}/query`
//!
//! Works as long as the resource has diagnostic settings forwarding to a
//! workspace; we do not need to discover the workspace ID separately.

#![allow(dead_code, unused_variables)]

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::azure::auth::AzureAuth;
use crate::azure::metrics::TimeRange;
use crate::azure::resources::{Resource, ResourceKind};

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
pub const KQL_FUNCTION_APP: &str = r#"
union AppTraces, AppExceptions, AppRequests
| order by TimeGenerated desc
| take 200
"#;

pub const KQL_FUNCTION_APP_ERRORS_FILTER: &str = r#"
| where SeverityLevel >= 3 or (Success == false and itemType == "request") or itemType == "exception"
"#;

pub const KQL_CONTAINER_APP: &str = r#"
ContainerAppConsoleLogs_CL
| order by TimeGenerated desc
| take 200
"#;

pub const KQL_CONTAINER_APP_ERRORS_FILTER: &str =
    r#"| where Log_s matches regex @"(?i)\b(error|exception|fatal|panic|stack)\b""#;

pub async fn fetch(
    auth: &AzureAuth,
    resource: &Resource,
    range: TimeRange,
    errors_only: bool,
) -> anyhow::Result<Vec<LogLine>> {
    todo!(
        "Lane 2: pick template by resource.kind; append errors filter before order/take if requested; \
         POST to LogsClient::query with timespan derived from range; parse columns→rows into LogLine; \
         on 'no diagnostic settings' return AzpectError::NoLogDestination wrapped via anyhow"
    )
}
