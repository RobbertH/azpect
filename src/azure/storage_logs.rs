//! Storage account access (audit) logs — who read / wrote which blob, when,
//! from where — read from the `StorageBlobLogs` table via the resource-centric
//! Log Analytics query endpoint. Requires a diagnostic setting on the
//! account's *blob service* forwarding the `StorageRead` / `StorageWrite` /
//! `StorageDelete` categories to a workspace — without one the query
//! legitimately returns zero rows. (Querying at the account scope still finds
//! the rows: resource-centric queries include child-resource logs, and the
//! blob service is a child of the account.)
//!
//! Blob only, deliberately: the drill-in chain this view hangs off is the
//! blob one (accounts → containers → blobs), and the file/queue/table log
//! tables have their own schemas.
//!
//! Caller identity: unlike Key Vault's claim columns, storage rows describe
//! *how* the request authenticated (`AuthenticationType`) and only carry a
//! requester identity for OAuth traffic:
//! - `RequesterUpn` — a human via Entra (portal, `az storage` with AD auth),
//! - `RequesterObjectId` — a service principal / managed identity; the UI
//!   resolves it to a display name via Microsoft Graph, best-effort,
//! - `RequesterAppId` — an app registration with no oid logged,
//! - none of the above — the row is SAS, account-key, or anonymous traffic,
//!   identified only by its `AuthenticationType` (and IP). Account-key and
//!   anonymous rows are the audit outliers worth flagging.

#![allow(dead_code, unused_variables)]

use anyhow::anyhow;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::azure::auth::{AzureAuth, SCOPE_LOGS};
use crate::azure::client::LogsClient;
use crate::azure::key_vault_logs::{AccessWindow, SelfIdentity};
use crate::azure::storage::StorageAccount;

/// Rows per query — same single-page discipline as
/// [`crate::azure::key_vault_logs::ACCESS_PAGE_SIZE`].
pub const ACCESS_PAGE_SIZE: u32 = 500;

/// How a row authenticated / who it resolved to — drives the column styling
/// and whether the UI attempts a Graph display-name lookup.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallerKind {
    /// A human — `RequesterUpn` present (OAuth).
    User,
    /// A directory object id (service principal / managed identity, OAuth).
    /// Graph-resolvable.
    Principal,
    /// An app registration — only `RequesterAppId` was logged (OAuth).
    App,
    /// A shared-access-signature request (any SAS flavor) — the token names
    /// nobody; the signing key does.
    Sas,
    /// The account's shared key — a static credential every holder shares,
    /// worth flagging.
    AccountKey,
    /// Unauthenticated read on a public container.
    Anonymous,
    Unknown,
}

/// One `StorageBlobLogs` row, reduced to what the access view shows.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AccessEvent {
    pub ts: DateTime<Utc>,
    /// `OperationName`: `GetBlob`, `PutBlob`, `ListBlobs`, …
    pub operation: String,
    /// The OAuth requester identity — a UPN, an object id, or an app id per
    /// `caller_kind`; empty for SAS / key / anonymous rows.
    pub identity: String,
    pub caller_kind: CallerKind,
    /// Raw `AuthenticationType` (`OAuth`, `SAS`, `AccountKey`, `Anonymous`,
    /// …) for the AUTH column.
    pub auth_type: String,
    /// `CallerIpAddress` with the `:port` suffix stripped.
    pub ip: String,
    /// `{container}/{blob}` from `ObjectKey` (account prefix stripped).
    /// `None` for account/service-level operations.
    pub object: Option<String>,
    /// `StatusText` when present (`Success`, `AuthorizationPermissionMismatch`,
    /// …), else the HTTP status code.
    pub result: String,
    /// Whether the request succeeded (2xx status).
    pub ok: bool,
}

/// One fetched page of access events plus the query metadata the header shows.
#[derive(Debug, Default)]
pub struct AccessPage {
    pub events: Vec<AccessEvent>,
    /// The page came back full — older rows in the window were cut off.
    pub truncated: bool,
    /// The identity the query excluded (when "exclude me" was on).
    pub hidden: Option<SelfIdentity>,
}

pub async fn fetch(
    auth: &AzureAuth,
    account: &StorageAccount,
    window: &AccessWindow,
    container: Option<&str>,
    exclude_self: bool,
) -> anyhow::Result<AccessPage> {
    // Resolve "me" from the same token the query will use — no extra call.
    let hidden = if exclude_self {
        let identity = SelfIdentity::from_token(&auth.token(SCOPE_LOGS).await?);
        if identity.is_empty() {
            return Err(anyhow!(
                "can't resolve your identity from the token (no upn/oid/ipaddr claim) — exclude-me is unavailable for this login"
            ));
        }
        Some(identity)
    } else {
        None
    };

    let kql = build_access_kql(&account.name, container, hidden.as_ref());
    let client = LogsClient::new(auth.clone())?;
    let resp = client.query(&account.id, &kql, &window.timespan()).await?;
    let events = parse_access_response(&resp, &account.name)?;
    let truncated = events.len() as u32 >= ACCESS_PAGE_SIZE;
    Ok(AccessPage {
        events,
        truncated,
        hidden,
    })
}

/// Escape a value for interpolation inside a double-quoted KQL string literal.
fn escape_kql(value: &str) -> String {
    value.replace('\\', r"\\").replace('"', r#"\""#)
}

/// Build the blob-logs query. `StorageBlobLogs` is a resource-specific table
/// with a fixed schema, but every optional column still goes through
/// `column_ifexists` — matching the Key Vault module's defensive posture.
fn build_access_kql(
    account_name: &str,
    container: Option<&str>,
    exclude: Option<&SelfIdentity>,
) -> String {
    let mut kql = String::from(
        r#"StorageBlobLogs
| extend upn_ = tolower(tostring(column_ifexists("RequesterUpn", "")))
| extend oid_ = tolower(tostring(column_ifexists("RequesterObjectId", "")))
| extend appid_ = tostring(column_ifexists("RequesterAppId", ""))
| extend auth_ = tostring(column_ifexists("AuthenticationType", ""))
| extend ip_ = tostring(column_ifexists("CallerIpAddress", ""))
| extend obj_ = tostring(column_ifexists("ObjectKey", ""))
| extend status_ = tostring(column_ifexists("StatusCode", ""))
| extend result_ = tostring(column_ifexists("StatusText", ""))
"#,
    );
    if let Some(container) = container {
        // `ObjectKey` is `/{account}/{container}/{blob}`; also match the bare
        // container key so container-level operations stay in scope.
        let prefix = format!("/{account_name}/{container}");
        kql.push_str(&format!(
            "| where obj_ startswith \"{}/\" or obj_ =~ \"{}\"\n",
            escape_kql(&prefix),
            escape_kql(&prefix),
        ));
    }
    if let Some(me) = exclude {
        let mut clauses = Vec::new();
        if let Some(upn) = &me.upn {
            clauses.push(format!("upn_ == \"{}\"", escape_kql(upn)));
        }
        if let Some(oid) = &me.oid {
            clauses.push(format!("oid_ == \"{}\"", escape_kql(oid)));
        }
        if let Some(ip) = &me.ip {
            // `CallerIpAddress` carries a `:port` suffix.
            let ip = escape_kql(ip);
            clauses.push(format!("ip_ == \"{ip}\" or ip_ startswith \"{ip}:\""));
        }
        if !clauses.is_empty() {
            kql.push_str(&format!("| where not({})\n", clauses.join(" or ")));
        }
    }
    kql.push_str(&format!(
        "| order by TimeGenerated desc\n| take {ACCESS_PAGE_SIZE}\n| project TimeGenerated, OperationName, upn_, oid_, appid_, auth_, ip_, obj_, status_, result_\n"
    ));
    kql
}

/// Classify a row — OAuth identity first (most specific), then the
/// authentication type for the identity-less shapes.
pub(crate) fn classify_row(upn: &str, oid: &str, appid: &str, auth_type: &str) -> CallerKind {
    if !upn.is_empty() {
        return CallerKind::User;
    }
    if !oid.is_empty() {
        return CallerKind::Principal;
    }
    if !appid.is_empty() {
        return CallerKind::App;
    }
    let auth = auth_type.to_ascii_lowercase();
    if auth.contains("sas") {
        return CallerKind::Sas;
    }
    match auth.as_str() {
        "accountkey" => CallerKind::AccountKey,
        "anonymous" => CallerKind::Anonymous,
        _ => CallerKind::Unknown,
    }
}

/// Strip the `:port` suffix Azure appends to `CallerIpAddress`. Conservative:
/// only when what follows the last `:` is all digits and what precedes it
/// looks like an IPv4 or bracketed IPv6 host — a bare IPv6 address keeps all
/// its colons.
pub(crate) fn strip_port(ip: &str) -> String {
    if let Some((host, port)) = ip.rsplit_once(':') {
        if !port.is_empty()
            && port.bytes().all(|b| b.is_ascii_digit())
            && (host.contains('.') || (host.starts_with('[') && host.ends_with(']')))
        {
            return host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_string();
        }
    }
    ip.to_string()
}

/// `/{account}/{container}/{blob}` → `container/blob`. `None` when the key is
/// empty or names only the account (service-level ops).
pub(crate) fn object_from_key(key: &str, account_name: &str) -> Option<String> {
    let path = key.strip_prefix('/').unwrap_or(key);
    let rest = path
        .strip_prefix(account_name)?
        .strip_prefix('/')?
        .trim_end_matches('/');
    (!rest.is_empty()).then(|| rest.to_string())
}

fn parse_access_response(
    value: &serde_json::Value,
    account_name: &str,
) -> anyhow::Result<Vec<AccessEvent>> {
    let table = value
        .get("tables")
        .and_then(|t| t.as_array())
        .and_then(|a| a.first())
        .ok_or_else(|| anyhow!("no tables in access-log response"))?;
    let columns: Vec<String> = table
        .get("columns")
        .and_then(|c| c.as_array())
        .map(|cols| {
            cols.iter()
                .filter_map(|c| c.get("name").and_then(|n| n.as_str()))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let idx = |name: &str| columns.iter().position(|c| c == name);
    let (Some(i_ts), Some(i_op)) = (idx("TimeGenerated"), idx("OperationName")) else {
        return Err(anyhow!("access-log response missing expected columns"));
    };

    let cell = |row: &[serde_json::Value], i: Option<usize>| -> String {
        i.and_then(|i| row.get(i))
            .map(|v| match v {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Null => String::new(),
                other => other.to_string(),
            })
            .unwrap_or_default()
    };

    let (i_upn, i_oid, i_appid, i_auth, i_ip, i_obj, i_status, i_result) = (
        idx("upn_"),
        idx("oid_"),
        idx("appid_"),
        idx("auth_"),
        idx("ip_"),
        idx("obj_"),
        idx("status_"),
        idx("result_"),
    );

    let mut events = Vec::new();
    for row in table
        .get("rows")
        .and_then(|r| r.as_array())
        .into_iter()
        .flatten()
    {
        let Some(row) = row.as_array() else { continue };
        let Some(ts) = row
            .get(i_ts)
            .and_then(|v| v.as_str())
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc))
        else {
            continue;
        };
        let upn = cell(row, i_upn);
        let oid = cell(row, i_oid);
        let appid = cell(row, i_appid);
        let auth_type = cell(row, i_auth);
        let caller_kind = classify_row(&upn, &oid, &appid, &auth_type);
        let identity = match caller_kind {
            CallerKind::User => upn,
            CallerKind::Principal => oid,
            CallerKind::App => appid,
            _ => String::new(),
        };
        let status = cell(row, i_status);
        let ok = status.starts_with('2');
        let result = match (cell(row, i_result), status) {
            (text, _) if !text.is_empty() => text,
            (_, code) if !code.is_empty() => code,
            _ => "?".to_string(),
        };
        events.push(AccessEvent {
            ts,
            operation: cell(row, Some(i_op)),
            identity,
            caller_kind,
            auth_type,
            ip: strip_port(&cell(row, i_ip)),
            object: object_from_key(&cell(row, i_obj), account_name),
            result,
            ok,
        });
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kql_scopes_to_container_and_excludes_self() {
        let me = SelfIdentity {
            upn: Some("robbert@contoso.com".into()),
            ip: Some("203.0.113.7".into()),
            oid: Some("9e8d7c6b-5a49-4038-b2c1-d0e9f8a7b6c5".into()),
        };
        let kql = build_access_kql("stcontoso", Some("invoices"), Some(&me));
        assert!(kql.contains(r#"obj_ startswith "/stcontoso/invoices/""#));
        assert!(kql.contains(r#"obj_ =~ "/stcontoso/invoices""#));
        assert!(kql.contains(r#"upn_ == "robbert@contoso.com""#));
        assert!(kql.contains(r#"oid_ == "9e8d7c6b-5a49-4038-b2c1-d0e9f8a7b6c5""#));
        assert!(kql.contains(r#"ip_ == "203.0.113.7" or ip_ startswith "203.0.113.7:""#));
        assert!(kql.contains("take 500"));
        assert!(kql.contains(r#"column_ifexists("AuthenticationType""#));
    }

    #[test]
    fn kql_without_filters_has_no_where() {
        let kql = build_access_kql("st", None, None);
        assert_eq!(kql.matches("| where").count(), 0, "{kql}");
    }

    #[test]
    fn classify_row_prefers_identity_then_auth_type() {
        assert_eq!(
            classify_row("dana@contoso.com", "guid", "guid", "OAuth"),
            CallerKind::User
        );
        assert_eq!(classify_row("", "guid", "", "OAuth"), CallerKind::Principal);
        assert_eq!(classify_row("", "", "guid", "OAuth"), CallerKind::App);
        assert_eq!(classify_row("", "", "", "SAS"), CallerKind::Sas);
        assert_eq!(
            classify_row("", "", "", "DelegationSas"),
            CallerKind::Sas,
            "every SAS flavor folds into Sas"
        );
        assert_eq!(
            classify_row("", "", "", "AccountKey"),
            CallerKind::AccountKey
        );
        assert_eq!(classify_row("", "", "", "Anonymous"), CallerKind::Anonymous);
        assert_eq!(classify_row("", "", "", ""), CallerKind::Unknown);
    }

    #[test]
    fn strip_port_handles_v4_v6_and_bare_hosts() {
        assert_eq!(strip_port("203.0.113.7:52413"), "203.0.113.7");
        assert_eq!(strip_port("203.0.113.7"), "203.0.113.7");
        assert_eq!(strip_port("[2001:db8::1]:443"), "2001:db8::1");
        // A bare IPv6 address keeps its colons — the tail isn't a port.
        assert_eq!(strip_port("2001:db8::1"), "2001:db8::1");
        assert_eq!(strip_port(""), "");
    }

    #[test]
    fn object_from_key_strips_account_prefix() {
        assert_eq!(
            object_from_key("/stcontoso/invoices/2026/08.pdf", "stcontoso"),
            Some("invoices/2026/08.pdf".to_string())
        );
        assert_eq!(
            object_from_key("/stcontoso/invoices", "stcontoso"),
            Some("invoices".to_string())
        );
        assert_eq!(object_from_key("/stcontoso", "stcontoso"), None);
        assert_eq!(object_from_key("", "stcontoso"), None);
    }

    #[test]
    fn parse_access_response_resolves_rows() {
        let resp = serde_json::json!({
            "tables": [{
                "columns": [
                    {"name": "TimeGenerated"}, {"name": "OperationName"},
                    {"name": "upn_"}, {"name": "oid_"}, {"name": "appid_"},
                    {"name": "auth_"}, {"name": "ip_"}, {"name": "obj_"},
                    {"name": "status_"}, {"name": "result_"}
                ],
                "rows": [
                    ["2026-08-20T10:00:00Z", "GetBlob", "", "f3c9a2e1-0d4b-4f7e-9a1c-2b5d8e7f6a3c", "", "OAuth", "10.0.1.12:44412", "/stcontoso/invoices/a.pdf", "200", "Success"],
                    ["2026-08-20T09:00:00Z", "PutBlob", "", "", "", "AccountKey", "198.51.100.3:9911", "/stcontoso/backups/dump.bak", "403", "AuthorizationFailure"]
                ]
            }]
        });
        let events = parse_access_response(&resp, "stcontoso").unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].caller_kind, CallerKind::Principal);
        assert_eq!(events[0].identity, "f3c9a2e1-0d4b-4f7e-9a1c-2b5d8e7f6a3c");
        assert_eq!(events[0].ip, "10.0.1.12");
        assert_eq!(events[0].object.as_deref(), Some("invoices/a.pdf"));
        assert!(events[0].ok);
        assert_eq!(events[1].caller_kind, CallerKind::AccountKey);
        assert_eq!(events[1].result, "AuthorizationFailure");
        assert!(!events[1].ok);
    }
}
