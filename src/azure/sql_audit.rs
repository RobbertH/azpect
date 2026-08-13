//! Azure SQL audit logs, read via the resource-centric Log Analytics query
//! endpoint. Two query shapes serve the audit view:
//!
//! - a **principal roll-up** (`fetch_principals`): one row per
//!   `server_principal_name` with last-seen / event counts — the "is this user
//!   still alive, can I delete it?" screen;
//! - a **per-principal event page** (`fetch_events`): the newest audit rows
//!   for one principal, statement text included.
//!
//! ## Where the rows live
//!
//! Auditing must be enabled with a Log Analytics destination or both queries
//! legitimately return zero rows. Two placement quirks the fetches absorb:
//!
//! - **Server-level vs database-level auditing.** The common server-level
//!   setup emits its diagnostic rows under the *server's `master` database*
//!   resource (covering every database, with a `database_name` column), so
//!   the primary query targets `.../servers/{srv}/databases/master`. The
//!   rarer per-database setup stamps rows with the user database's own ARM
//!   id — when the master query comes back empty and the audit target is a
//!   database, the fetch retries against that id.
//! - **Two table modes.** Legacy diagnostic settings write to
//!   `AzureDiagnostics` (`Category == "SQLSecurityAuditEvents"`, suffixed
//!   columns like `server_principal_name_s`); resource-specific mode writes a
//!   dedicated `SQLSecurityAuditEvents` table with clean names. The KQL
//!   `union isfuzzy=true`s both and normalizes the columns, so a missing
//!   table never fails the query.
//!
//! Statements longer than 4000 chars are split across audit rows sharing a
//! `sequence_group_id`; [`fetch_events`] stitches them back together so long
//! queries don't render as garbled fragments.

use anyhow::anyhow;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::azure::auth::AzureAuth;
use crate::azure::client::LogsClient;
use crate::azure::key_vault_logs::AccessWindow;
use crate::azure::sql::{SqlKind, SqlResource};

/// Row cap for the per-principal event page (same "newest N" semantics as the
/// Key Vault access page). `BATCH_COMPLETED` volume on a busy database is
/// orders of magnitude past Key Vault audit volume, hence the per-principal
/// scoping before raw rows are ever fetched.
pub const EVENTS_PAGE_SIZE: u32 = 500;

/// Cap on the aggregated principal roll-up. Aggregation happens server-side,
/// so this only guards against pathological principal counts.
pub const PRINCIPALS_PAGE_SIZE: u32 = 200;

/// What the audit queries run against, derived from the pinned SQL resource.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditTarget {
    /// ARM id of the server's `master` database — where server-level auditing
    /// (the common setup) lands its rows. Primary query scope.
    pub master_id: String,
    /// ARM id of the specific database, for the database-level-auditing
    /// fallback. `None` when the audit view was opened on an elastic pool.
    pub database_id: Option<String>,
    /// Logical server name (header display).
    pub server: String,
    /// Database name to filter `database_name` on; `None` ⇒ server-wide.
    pub database: Option<String>,
}

impl AuditTarget {
    /// Derive the query target from a pinned pool / database. A database scopes
    /// the queries to itself (and enables the database-level fallback); a pool
    /// audits server-wide — pools have no audit stream of their own.
    pub fn from_resource(r: &SqlResource) -> Option<AuditTarget> {
        let lower = r.id.to_lowercase();
        let idx = lower.find("/servers/")?;
        let start = idx + "/servers/".len();
        let rest = &r.id[start..];
        let server = rest.split('/').next().unwrap_or("");
        if server.is_empty() {
            return None;
        }
        let server_id = &r.id[..start + server.len()];
        let (database, database_id) = match r.kind {
            SqlKind::Database => (Some(r.name.clone()), Some(r.id.clone())),
            SqlKind::ElasticPool => (None, None),
        };
        Some(AuditTarget {
            master_id: format!("{server_id}/databases/master"),
            database_id,
            server: server.to_string(),
            database,
        })
    }

    /// Header chip: `server` or `server/database`.
    pub fn label(&self) -> String {
        match &self.database {
            Some(db) => format!("{}/{}", self.server, db),
            None => self.server.clone(),
        }
    }
}

/// One row of the principal roll-up: everything needed to answer "does this
/// principal still do anything, and what would break if I deleted it".
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PrincipalSummary {
    pub principal: String,
    pub last_seen: DateTime<Utc>,
    /// Total audit events in the window (queries + logins + transaction /
    /// audit-session noise).
    pub events: u64,
    /// Actual work: `BATCH COMPLETED` + `RPC COMPLETED` events.
    pub queries: u64,
    /// Authentication events, succeeded or failed (`DBAS` / `DBAF`).
    pub logins: u64,
    /// Events with `succeeded == false` — mostly failed logins; a principal
    /// with *only* failures is already broken, which is its own answer.
    pub failed: u64,
    /// Distinct databases touched (server-side `make_set`, capped at 8).
    pub databases: Vec<String>,
    /// Distinct client IPs seen.
    pub distinct_ips: u64,
    /// Distinct application names seen (capped at 8) — cheaply separates "the
    /// app doing its job" from "someone in SSMS".
    pub apps: Vec<String>,
}

#[derive(Debug, Default)]
pub struct PrincipalsPage {
    pub principals: Vec<PrincipalSummary>,
    /// The roll-up hit its row cap — extremely unusual, but say so.
    pub truncated: bool,
}

/// One audit event for the per-principal drill-in, statement chunks already
/// stitched.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuditEvent {
    pub ts: DateTime<Utc>,
    /// Raw `action_id` (`BCM`, `RCM`, `DBAS`, `DBAF`, …), trimmed.
    pub action: String,
    pub succeeded: bool,
    pub database: String,
    pub ip: String,
    pub app: String,
    pub host: String,
    /// Full statement text (empty for login events).
    pub statement: String,
    /// `additional_information` — XML that, for failed events, usually names
    /// the actual error (invalid object, permission denied, …). Raw; shown in
    /// the event detail view.
    pub info: String,
    pub affected_rows: Option<i64>,
    pub response_rows: Option<i64>,
}

/// Short human label for an audit `action_id` (codes per
/// `sys.dm_audit_actions`). Unknown codes pass through raw — better an opaque
/// code than a wrong guess.
pub fn action_label(code: &str) -> &str {
    match code {
        "BCM" => "batch",
        "RCM" => "rpc",
        "DBAS" => "login",
        "DBAF" => "login-failed",
        "TRBC" => "tx-begin",
        "TRCC" => "tx-commit",
        "TRRC" => "tx-rollback",
        "AUSC" => "audit-session",
        other => other,
    }
}

impl AuditEvent {
    /// [`action_label`] for this event's action code.
    pub fn action_label(&self) -> &str {
        action_label(&self.action)
    }
}

#[derive(Debug, Default)]
pub struct EventsPage {
    pub events: Vec<AuditEvent>,
    /// The page came back at the row cap — older rows in the window exist.
    pub truncated: bool,
}

pub async fn fetch_principals(
    auth: &AzureAuth,
    target: &AuditTarget,
    window: &AccessWindow,
) -> anyhow::Result<PrincipalsPage> {
    let kql = build_principals_kql(target.database.as_deref());
    let client = LogsClient::new(auth.clone())?;
    let resp = query_with_fallback(&client, target, &kql, &window.timespan()).await?;
    let principals = parse_principals_response(&resp)?;
    let truncated = principals.len() as u32 >= PRINCIPALS_PAGE_SIZE;
    Ok(PrincipalsPage {
        principals,
        truncated,
    })
}

/// Fetch the newest events page for `principal`. `errors_only` filters to
/// `succeeded == false` server-side (all failures in the window, not just
/// failures among the newest rows); `before` fetches the page *older than*
/// that timestamp — the scroll-past-bottom pagination, mirroring the logs
/// view.
pub async fn fetch_events(
    auth: &AzureAuth,
    target: &AuditTarget,
    window: &AccessWindow,
    principal: &str,
    errors_only: bool,
    before: Option<DateTime<Utc>>,
) -> anyhow::Result<EventsPage> {
    let kql = build_events_kql(target.database.as_deref(), principal, errors_only, before);
    let client = LogsClient::new(auth.clone())?;
    let resp = query_with_fallback(&client, target, &kql, &window.timespan()).await?;
    let events = parse_events_response(&resp)?;
    // Truncation is judged on raw rows fetched, before stitching merges chunks.
    let truncated = raw_row_count(&resp) as u32 >= EVENTS_PAGE_SIZE;
    Ok(EventsPage { events, truncated })
}

/// Query the server's `master` database (server-level auditing), falling back
/// to the specific database's own resource id (database-level auditing) when
/// master yields nothing. An empty master result is kept when the fallback
/// also comes up empty or errors — "no rows" from the primary scope must not
/// be masked by a fallback failure.
async fn query_with_fallback(
    client: &LogsClient,
    target: &AuditTarget,
    kql: &str,
    timespan: &str,
) -> anyhow::Result<serde_json::Value> {
    match client.query(&target.master_id, kql, timespan).await {
        Ok(v) if raw_row_count(&v) > 0 => Ok(v),
        Ok(v) => match &target.database_id {
            Some(id) => match client.query(id, kql, timespan).await {
                Ok(v2) if raw_row_count(&v2) > 0 => Ok(v2),
                _ => Ok(v),
            },
            None => Ok(v),
        },
        Err(e) => match &target.database_id {
            Some(id) => client.query(id, kql, timespan).await.map_err(|_| e),
            None => Err(e),
        },
    }
}

/// Escape a value for interpolation inside a double-quoted KQL string literal.
fn escape_kql(value: &str) -> String {
    value.replace('\\', r"\\").replace('"', r#"\""#)
}

/// The normalizing base: both table modes unioned into one column set
/// (`ts`, `principal_`, `action_`, `succeeded_`, `db_`, `ip_`, `app_`,
/// `host_`, `stmt_`, `affected_`, `response_`, `seqgrp_`, `seq_`). Every
/// source column goes through `column_ifexists` — schemas drift and a column
/// that never appeared for this server must not fail the whole query.
fn base_kql(database: Option<&str>) -> String {
    let mut kql = String::from(
        r#"union isfuzzy=true
(AzureDiagnostics
| where Category == "SQLSecurityAuditEvents"
| project ts=TimeGenerated,
    principal_=tostring(column_ifexists("server_principal_name_s","")),
    action_=tostring(column_ifexists("action_id_s","")),
    succeeded_=tolower(tostring(column_ifexists("succeeded_s",""))),
    db_=tostring(column_ifexists("database_name_s","")),
    ip_=tostring(column_ifexists("client_ip_s","")),
    app_=tostring(column_ifexists("application_name_s","")),
    host_=tostring(column_ifexists("host_name_s","")),
    stmt_=tostring(column_ifexists("statement_s","")),
    info_=tostring(column_ifexists("additional_information_s","")),
    affected_=tolong(column_ifexists("affected_rows_d",real(null))),
    response_=tolong(column_ifexists("response_rows_d",real(null))),
    seqgrp_=tostring(column_ifexists("sequence_group_id_g","")),
    seq_=tolong(column_ifexists("sequence_number_d",real(null)))),
(SQLSecurityAuditEvents
| project ts=TimeGenerated,
    principal_=tostring(column_ifexists("ServerPrincipalName","")),
    action_=tostring(column_ifexists("ActionId","")),
    succeeded_=tolower(tostring(column_ifexists("Succeeded",""))),
    db_=tostring(column_ifexists("DatabaseName","")),
    ip_=tostring(column_ifexists("ClientIp","")),
    app_=tostring(column_ifexists("ApplicationName","")),
    host_=tostring(column_ifexists("HostName","")),
    stmt_=tostring(column_ifexists("Statement","")),
    info_=tostring(column_ifexists("AdditionalInformation","")),
    affected_=tolong(column_ifexists("AffectedRows",long(null))),
    response_=tolong(column_ifexists("ResponseRows",long(null))),
    seqgrp_=tostring(column_ifexists("SequenceGroupId","")),
    seq_=tolong(column_ifexists("SequenceNumber",long(null))))
| where isnotempty(principal_)
"#,
    );
    if let Some(db) = database {
        // Server-level auditing streams every database through master's
        // resource — the database_name column is what scopes a single one.
        kql.push_str(&format!("| where db_ =~ \"{}\"\n", escape_kql(db)));
    }
    kql
}

/// Public so the view can show the exact query while it loads (or fails).
pub fn build_principals_kql(database: Option<&str>) -> String {
    let mut kql = base_kql(database);
    kql.push_str(&format!(
        r#"| summarize last_seen=max(ts), events_=count(), queries_=countif(action_ startswith "BCM" or action_ startswith "RCM"), logins_=countif(action_ startswith "DBA"), failed_=countif(succeeded_ == "false"), ips_=dcount(ip_), dbs_=make_set(db_, 8), apps_=make_set(app_, 8) by principal_
| order by last_seen desc
| take {PRINCIPALS_PAGE_SIZE}
"#,
    ));
    kql
}

/// Public so the view can show the exact query while it loads (or fails).
pub fn build_events_kql(
    database: Option<&str>,
    principal: &str,
    errors_only: bool,
    before: Option<DateTime<Utc>>,
) -> String {
    let mut kql = base_kql(database);
    kql.push_str(&format!(
        "| where principal_ =~ \"{}\"\n",
        escape_kql(principal)
    ));
    if errors_only {
        kql.push_str("| where succeeded_ == \"false\"\n");
    }
    if let Some(before) = before {
        // Older-than page for the scroll-past-bottom fetch.
        kql.push_str(&format!(
            "| where ts < datetime({})\n",
            before.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        ));
    }
    kql.push_str(&format!(
        "| order by ts desc\n| take {EVENTS_PAGE_SIZE}\n| project ts, action_, succeeded_, db_, ip_, app_, host_, stmt_, info_, affected_, response_, seqgrp_, seq_\n",
    ));
    kql
}

// ---------------------------------------------------------------------------
// Response parsing
// ---------------------------------------------------------------------------

/// The first table of a Log Analytics response, decomposed into named-column
/// row access. Owns nothing — borrows the response value.
struct ResponseTable<'a> {
    columns: Vec<&'a str>,
    rows: &'a [serde_json::Value],
}

impl<'a> ResponseTable<'a> {
    fn from_response(value: &'a serde_json::Value) -> anyhow::Result<ResponseTable<'a>> {
        let table = value
            .get("tables")
            .and_then(|t| t.as_array())
            .and_then(|a| a.first())
            .ok_or_else(|| anyhow!("no tables in audit-log response"))?;
        let columns = table
            .get("columns")
            .and_then(|c| c.as_array())
            .map(|cols| {
                cols.iter()
                    .filter_map(|c| c.get("name").and_then(|n| n.as_str()))
                    .collect()
            })
            .unwrap_or_default();
        let rows = table
            .get("rows")
            .and_then(|r| r.as_array())
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        Ok(ResponseTable { columns, rows })
    }

    fn idx(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| *c == name)
    }
}

/// Row count of the first table; 0 for anything malformed. Drives both the
/// master→database fallback and the truncation flag.
fn raw_row_count(value: &serde_json::Value) -> usize {
    ResponseTable::from_response(value)
        .map(|t| t.rows.len())
        .unwrap_or(0)
}

fn cell_str(row: &[serde_json::Value], i: Option<usize>) -> String {
    i.and_then(|i| row.get(i))
        .map(|v| match v {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Null => String::new(),
            other => other.to_string(),
        })
        .unwrap_or_default()
}

fn cell_i64(row: &[serde_json::Value], i: Option<usize>) -> Option<i64> {
    match i.and_then(|i| row.get(i))? {
        serde_json::Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        serde_json::Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn cell_ts(row: &[serde_json::Value], i: usize) -> Option<DateTime<Utc>> {
    row.get(i)
        .and_then(|v| v.as_str())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
}

/// A `make_set` cell: the v1 API returns dynamic values either as a JSON
/// array or as a string containing one — accept both.
fn cell_string_set(row: &[serde_json::Value], i: Option<usize>) -> Vec<String> {
    let value = match i.and_then(|i| row.get(i)) {
        Some(v) => v,
        None => return Vec::new(),
    };
    let items: Vec<serde_json::Value> = match value {
        serde_json::Value::Array(a) => a.clone(),
        serde_json::Value::String(s) => serde_json::from_str(s).unwrap_or_default(),
        _ => Vec::new(),
    };
    let mut out: Vec<String> = items
        .iter()
        .filter_map(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    out.sort();
    out
}

fn parse_principals_response(value: &serde_json::Value) -> anyhow::Result<Vec<PrincipalSummary>> {
    let table = ResponseTable::from_response(value)?;
    let (Some(i_principal), Some(i_last_seen)) = (table.idx("principal_"), table.idx("last_seen"))
    else {
        // A zero-row response can legitimately omit columns; anything else is
        // a shape we don't understand.
        if table.rows.is_empty() {
            return Ok(Vec::new());
        }
        return Err(anyhow!("audit roll-up response missing expected columns"));
    };
    let (i_events, i_queries, i_logins, i_failed, i_ips, i_dbs, i_apps) = (
        table.idx("events_"),
        table.idx("queries_"),
        table.idx("logins_"),
        table.idx("failed_"),
        table.idx("ips_"),
        table.idx("dbs_"),
        table.idx("apps_"),
    );

    let mut out = Vec::new();
    for row in table.rows {
        let Some(row) = row.as_array() else { continue };
        let Some(last_seen) = cell_ts(row, i_last_seen) else {
            continue;
        };
        let principal = cell_str(row, Some(i_principal));
        if principal.is_empty() {
            continue;
        }
        let count = |i: Option<usize>| cell_i64(row, i).unwrap_or(0).max(0) as u64;
        out.push(PrincipalSummary {
            principal,
            last_seen,
            events: count(i_events),
            queries: count(i_queries),
            logins: count(i_logins),
            failed: count(i_failed),
            databases: cell_string_set(row, i_dbs),
            distinct_ips: count(i_ips),
            apps: cell_string_set(row, i_apps),
        });
    }
    Ok(out)
}

fn parse_events_response(value: &serde_json::Value) -> anyhow::Result<Vec<AuditEvent>> {
    let table = ResponseTable::from_response(value)?;
    let (Some(i_ts), Some(i_action)) = (table.idx("ts"), table.idx("action_")) else {
        if table.rows.is_empty() {
            return Ok(Vec::new());
        }
        return Err(anyhow!("audit events response missing expected columns"));
    };
    let (
        i_succeeded,
        i_db,
        i_ip,
        i_app,
        i_host,
        i_stmt,
        i_info,
        i_affected,
        i_response,
        i_grp,
        i_seq,
    ) = (
        table.idx("succeeded_"),
        table.idx("db_"),
        table.idx("ip_"),
        table.idx("app_"),
        table.idx("host_"),
        table.idx("stmt_"),
        table.idx("info_"),
        table.idx("affected_"),
        table.idx("response_"),
        table.idx("seqgrp_"),
        table.idx("seq_"),
    );

    // Statement chunks: rows sharing a non-empty `sequence_group_id` are one
    // logical statement split at 4000 chars. Keep the first row's slot (rows
    // arrive newest-first), collect `(sequence_number, chunk)` per group, and
    // assemble in sequence order at the end.
    let mut out: Vec<(AuditEvent, Vec<(i64, String)>)> = Vec::new();
    let mut groups: HashMap<String, usize> = HashMap::new();
    for row in table.rows {
        let Some(row) = row.as_array() else { continue };
        let Some(ts) = cell_ts(row, i_ts) else {
            continue;
        };
        let statement = cell_str(row, i_stmt);
        let seq = cell_i64(row, i_seq).unwrap_or(1);
        let group = cell_str(row, i_grp);
        let event = AuditEvent {
            ts,
            action: cell_str(row, Some(i_action)).trim().to_string(),
            succeeded: cell_str(row, i_succeeded) == "true",
            database: cell_str(row, i_db),
            ip: cell_str(row, i_ip),
            app: cell_str(row, i_app),
            host: cell_str(row, i_host),
            statement: String::new(),
            info: cell_str(row, i_info),
            affected_rows: cell_i64(row, i_affected),
            response_rows: cell_i64(row, i_response),
        };
        if group.is_empty() {
            let mut event = event;
            event.statement = statement;
            out.push((event, Vec::new()));
            continue;
        }
        match groups.get(&group) {
            Some(&slot) => out[slot].1.push((seq, statement)),
            None => {
                groups.insert(group, out.len());
                out.push((event, vec![(seq, statement)]));
            }
        }
    }

    Ok(out
        .into_iter()
        .map(|(mut event, mut chunks)| {
            if !chunks.is_empty() {
                chunks.sort_by_key(|(seq, _)| *seq);
                event.statement = chunks.into_iter().map(|(_, s)| s).collect();
            }
            event
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Principal identity resolution
// ---------------------------------------------------------------------------

/// Extract the GUID a principal name can be resolved through via Microsoft
/// Graph, if any. Azure SQL renders Entra principals in several raw forms:
///
/// - `{clientId}@{tenantId}` — an app registration / service principal login;
/// - a bare GUID — a client id or directory object id;
/// - `S-1-9-3-a-b-c-d` — the SID string form SQL falls back to when it has no
///   name for the principal: the four trailing numbers are the 16-byte Entra
///   id (object id for users/groups, client id for apps) as little-endian
///   u32s of its `uniqueidentifier` binary layout.
///
/// `None` for anything that already reads as a name (SQL logins, UPNs).
pub fn graph_candidate(principal: &str) -> Option<String> {
    let p = principal.trim();
    if let Some(guid) = decode_sid_guid(p) {
        return Some(guid);
    }
    let mut parts = p.split('@');
    let first = parts.next().unwrap_or(p);
    let domain_ok = match parts.next() {
        // `x@y@z` is nothing we recognize.
        Some(second) => is_guid(second) && parts.next().is_none(),
        None => true,
    };
    (is_guid(first) && domain_ok).then(|| first.to_lowercase())
}

fn is_guid(s: &str) -> bool {
    let bytes = s.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => *b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

/// Decode a `S-1-9-3-a-b-c-d` SID string back to the GUID it encodes. The
/// four subauthorities are consecutive little-endian u32s of the 16-byte
/// `uniqueidentifier` binary form (data1/2/3 little-endian, final 8 bytes
/// as-is). Best-effort: if the encoding assumption is off for some principal,
/// Graph resolution simply misses and the raw SID stays on screen.
fn decode_sid_guid(sid: &str) -> Option<String> {
    let rest = sid.strip_prefix("S-1-9-3-")?;
    let parts: Vec<u32> = rest
        .split('-')
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    if parts.len() != 4 {
        return None;
    }
    let mut b = [0u8; 16];
    for (i, part) in parts.iter().enumerate() {
        b[i * 4..i * 4 + 4].copy_from_slice(&part.to_le_bytes());
    }
    let d1 = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    let d2 = u16::from_le_bytes([b[4], b[5]]);
    let d3 = u16::from_le_bytes([b[6], b[7]]);
    Some(format!(
        "{d1:08x}-{d2:04x}-{d3:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    ))
}

/// Compact "how long ago" for the LAST SEEN column: `3m`, `2h`, `5d`, `3mo`,
/// `1y`. Sub-minute is `now`.
pub fn humanize_ago(ts: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let mins = (now - ts).num_minutes();
    match mins {
        i64::MIN..=0 => "now".to_string(),
        1..=59 => format!("{mins}m"),
        60..=1439 => format!("{}h", mins / 60),
        1440..=86_399 => format!("{}d", mins / 1440), // < 60 days
        86_400..=525_599 => format!("{}mo", mins / 43_200),
        _ => format!("{}y", mins / 525_600),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db_resource() -> SqlResource {
        SqlResource {
            id: "/subscriptions/s/resourceGroups/rg/providers/Microsoft.Sql/servers/srv-1/databases/orders".to_string(),
            name: "orders".to_string(),
            server: "srv-1".to_string(),
            resource_group: "rg".to_string(),
            subscription_id: "s".to_string(),
            location: "westeurope".to_string(),
            kind: SqlKind::Database,
            sku_name: None,
            sku_tier: None,
            capacity: None,
            status: None,
            elastic_pool_id: None,
            max_size_bytes: None,
        }
    }

    #[test]
    fn target_from_database_scopes_and_falls_back() {
        let t = AuditTarget::from_resource(&db_resource()).unwrap();
        assert_eq!(
            t.master_id,
            "/subscriptions/s/resourceGroups/rg/providers/Microsoft.Sql/servers/srv-1/databases/master"
        );
        assert_eq!(t.database.as_deref(), Some("orders"));
        assert_eq!(t.database_id.as_deref(), Some(db_resource().id.as_str()));
        assert_eq!(t.label(), "srv-1/orders");
    }

    #[test]
    fn target_from_pool_is_server_wide() {
        let mut r = db_resource();
        r.id = "/subscriptions/s/resourceGroups/rg/providers/Microsoft.Sql/servers/srv-1/elasticPools/pool-a".to_string();
        r.name = "pool-a".to_string();
        r.kind = SqlKind::ElasticPool;
        let t = AuditTarget::from_resource(&r).unwrap();
        assert_eq!(t.database, None);
        assert_eq!(t.database_id, None);
        assert!(t.master_id.ends_with("/servers/srv-1/databases/master"));
        assert_eq!(t.label(), "srv-1");

        r.id = "garbage".to_string();
        assert!(AuditTarget::from_resource(&r).is_none());
    }

    #[test]
    fn principals_kql_unions_both_tables_and_filters_db() {
        let kql = build_principals_kql(Some("orders"));
        assert!(kql.contains("union isfuzzy=true"));
        assert!(kql.contains(r#"Category == "SQLSecurityAuditEvents""#));
        assert!(kql.contains("SQLSecurityAuditEvents\n"));
        assert!(kql.contains(r#"column_ifexists("server_principal_name_s""#));
        assert!(kql.contains(r#"column_ifexists("ServerPrincipalName""#));
        assert!(kql.contains(r#"db_ =~ "orders""#));
        assert!(kql.contains("summarize last_seen=max(ts)"));
        assert!(kql.contains("take 200"));
        // Server-wide (pool) roll-up has no db filter.
        assert!(!build_principals_kql(None).contains("db_ =~"));
    }

    #[test]
    fn kql_parens_are_balanced() {
        // Regression: the resource-specific union branch once closed one paren
        // too many, which Log Analytics rejects with SYN0002 at the stray ')'.
        // A naive count is sound here because no string literal in the
        // generated KQL contains a paren.
        let balance = |kql: &str| {
            let mut depth: i64 = 0;
            for c in kql.chars() {
                match c {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        assert!(depth >= 0, "unmatched ')' in KQL:\n{kql}");
                    }
                    _ => {}
                }
            }
            assert_eq!(depth, 0, "unclosed '(' in KQL:\n{kql}");
        };
        balance(&build_principals_kql(None));
        balance(&build_principals_kql(Some("orders")));
        balance(&build_events_kql(None, "app-orders", false, None));
        balance(&build_events_kql(
            Some("orders"),
            "app-orders",
            true,
            Some(Utc::now()),
        ));
    }

    #[test]
    fn events_kql_scopes_to_principal_and_escapes() {
        let kql = build_events_kql(None, r#"app"user\x"#, false, None);
        assert!(kql.contains(r#"principal_ =~ "app\"user\\x""#));
        assert!(kql.contains("take 500"));
        assert!(kql.contains("order by ts desc"));
        assert!(!kql.contains("succeeded_ == \"false\"\n"));
        assert!(!kql.contains("ts < datetime"));
    }

    #[test]
    fn events_kql_applies_errors_only_and_older_than() {
        let before = DateTime::parse_from_rfc3339("2026-08-01T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let kql = build_events_kql(None, "app-orders", true, Some(before));
        assert!(kql.contains("| where succeeded_ == \"false\""));
        assert!(kql.contains("| where ts < datetime(2026-08-01T10:00:00.000Z)"));
    }

    #[test]
    fn principals_kql_counts_queries_and_logins() {
        let kql = build_principals_kql(None);
        assert!(kql
            .contains(r#"queries_=countif(action_ startswith "BCM" or action_ startswith "RCM")"#));
        assert!(kql.contains(r#"logins_=countif(action_ startswith "DBA")"#));
    }

    #[test]
    fn graph_candidate_recognizes_entra_forms() {
        // clientId@tenantId — resolve through the client id.
        assert_eq!(
            graph_candidate(
                "F3C9A2E1-0D4B-4F7E-9A1C-2B5D8E7F6A3C@9188040d-6c67-4c5b-b112-36a304b66dad"
            )
            .as_deref(),
            Some("f3c9a2e1-0d4b-4f7e-9a1c-2b5d8e7f6a3c")
        );
        // Bare GUID.
        assert_eq!(
            graph_candidate("f3c9a2e1-0d4b-4f7e-9a1c-2b5d8e7f6a3c").as_deref(),
            Some("f3c9a2e1-0d4b-4f7e-9a1c-2b5d8e7f6a3c")
        );
        // Names / UPNs / SQL logins pass through unresolved.
        assert_eq!(graph_candidate("dana@contoso.com"), None);
        assert_eq!(graph_candidate("app-orders"), None);
        assert_eq!(graph_candidate("legacy_readonly"), None);
    }

    #[test]
    fn sid_string_decodes_to_guid() {
        // GUID aabbccdd-eeff-1122-3344-556677889900 in uniqueidentifier binary
        // form is dd cc bb aa | ff ee | 22 11 | 33 44 55 66 77 88 99 00, whose
        // four little-endian u32s are the subauthorities below.
        assert_eq!(
            graph_candidate("S-1-9-3-2864434397-287502079-1716864051-10061943").as_deref(),
            Some("aabbccdd-eeff-1122-3344-556677889900")
        );
        // Wrong shape → not decoded.
        assert_eq!(graph_candidate("S-1-9-3-1-2-3"), None);
        assert_eq!(graph_candidate("S-1-5-21-1-2-3-4"), None);
    }

    #[test]
    fn transaction_actions_get_labels() {
        let mut e = AuditEvent {
            ts: Utc::now(),
            action: "TRBC".to_string(),
            succeeded: true,
            database: String::new(),
            ip: String::new(),
            app: String::new(),
            host: String::new(),
            statement: String::new(),
            info: String::new(),
            affected_rows: None,
            response_rows: None,
        };
        assert_eq!(e.action_label(), "tx-begin");
        e.action = "TRCC".to_string();
        assert_eq!(e.action_label(), "tx-commit");
        e.action = "TRRC".to_string();
        assert_eq!(e.action_label(), "tx-rollback");
    }

    #[test]
    fn parse_principals_reads_rollup_rows() {
        let resp = serde_json::json!({
            "tables": [{
                "columns": [
                    {"name": "principal_"}, {"name": "last_seen"}, {"name": "events_"},
                    {"name": "queries_"}, {"name": "logins_"},
                    {"name": "failed_"}, {"name": "ips_"}, {"name": "dbs_"}, {"name": "apps_"}
                ],
                "rows": [
                    ["app-orders", "2026-08-09T10:00:00Z", 4211, 3900, 280, 0, 2, "[\"orders\"]", "[\"orders-api\"]"],
                    // Resource-specific mode returns dynamics as real arrays.
                    ["legacy_readonly", "2026-02-01T08:00:00Z", 12, 0, 12, 12, 1, ["orders", "billing"], []]
                ]
            }]
        });
        let rows = parse_principals_response(&resp).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].principal, "app-orders");
        assert_eq!(rows[0].events, 4211);
        assert_eq!(rows[0].queries, 3900);
        assert_eq!(rows[0].logins, 280);
        assert_eq!(rows[0].apps, vec!["orders-api"]);
        assert_eq!(rows[1].failed, 12);
        assert_eq!(rows[1].queries, 0);
        assert_eq!(rows[1].databases, vec!["billing", "orders"]);
        assert!(rows[1].apps.is_empty());
    }

    #[test]
    fn parse_principals_tolerates_empty_response() {
        let resp = serde_json::json!({ "tables": [{ "columns": [], "rows": [] }] });
        assert!(parse_principals_response(&resp).unwrap().is_empty());
        assert!(parse_principals_response(&serde_json::json!({})).is_err());
    }

    #[test]
    fn parse_events_stitches_statement_chunks() {
        let cols: Vec<serde_json::Value> = [
            "ts",
            "action_",
            "succeeded_",
            "db_",
            "ip_",
            "app_",
            "host_",
            "stmt_",
            "info_",
            "affected_",
            "response_",
            "seqgrp_",
            "seq_",
        ]
        .iter()
        .map(|n| serde_json::json!({"name": n}))
        .collect();
        let resp = serde_json::json!({
            "tables": [{
                "columns": cols,
                "rows": [
                    ["2026-08-09T10:00:00Z", "BCM ", "true", "orders", "10.0.0.4", "orders-api", "host-1", "SELECT * FROM ", "", null, 12, "g-1", 1],
                    ["2026-08-09T10:00:00Z", "BCM ", "true", "orders", "10.0.0.4", "orders-api", "host-1", "big_table", "", null, 12, "g-1", 2],
                    ["2026-08-09T09:00:00Z", "DBAF", "false", "orders", "198.51.100.9", "SSMS", "laptop", "", "<action_info>Login failed for user</action_info>", null, null, "", null]
                ]
            }]
        });
        let events = parse_events_response(&resp).unwrap();
        assert_eq!(events.len(), 2, "chunks merged into one event");
        assert_eq!(events[0].statement, "SELECT * FROM big_table");
        assert!(events[1].info.contains("Login failed"), "info column read");
        assert_eq!(events[0].action, "BCM", "action id trimmed");
        assert_eq!(events[0].action_label(), "batch");
        assert!(events[0].succeeded);
        assert_eq!(events[0].response_rows, Some(12));
        assert_eq!(events[1].action_label(), "login-failed");
        assert!(!events[1].succeeded);
    }

    #[test]
    fn humanize_ago_scales() {
        let now = DateTime::parse_from_rfc3339("2026-08-10T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let at = |s: &str| DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc);
        assert_eq!(humanize_ago(at("2026-08-10T11:59:40Z"), now), "now");
        assert_eq!(humanize_ago(at("2026-08-10T11:15:00Z"), now), "45m");
        assert_eq!(humanize_ago(at("2026-08-10T07:00:00Z"), now), "5h");
        assert_eq!(humanize_ago(at("2026-08-05T12:00:00Z"), now), "5d");
        assert_eq!(humanize_ago(at("2026-05-10T12:00:00Z"), now), "3mo");
        assert_eq!(humanize_ago(at("2024-01-10T12:00:00Z"), now), "2y");
    }
}
