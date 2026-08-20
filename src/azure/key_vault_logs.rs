//! Key Vault access (audit) logs, read from the `AzureDiagnostics` table via
//! the resource-centric Log Analytics query endpoint. Requires the vault to
//! have a diagnostic setting forwarding `AuditEvent` to a workspace — without
//! one the query legitimately returns zero rows.
//!
//! Caller identity resolution (who accessed the vault), in order:
//! 1. `identity_claim_upn_s` (or its legacy xmlsoap-schema variant) — a human.
//! 2. `identity_claim_xms_mirid_s` — the ARM id of the managed identity /
//!    resource that used the secret (e.g. a Container App).
//! 3. `identity_claim_appid_g` — a bare service principal / app registration.

use anyhow::anyhow;
use base64::Engine;
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use crate::azure::auth::{AzureAuth, SCOPE_LOGS};
use crate::azure::client::LogsClient;
use crate::azure::key_vault::KeyVault;

/// Rows per query. Key Vault audit volumes are usually modest; one page keeps
/// the fetch simple. A full page sets [`AccessPage::truncated`] so the UI can
/// say "newest 500" instead of implying completeness.
pub const ACCESS_PAGE_SIZE: u32 = 500;

/// Time window for the access-logs query. Unlike
/// [`crate::azure::metrics::TimeRange`] (which also picks metric bin sizes and
/// is capped at a week), audit trails are routinely inspected months back —
/// so this carries an arbitrary user-typed duration too ("6m", "1y").
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum AccessWindow {
    Hour,
    /// The default: audit questions usually start with "who touched it today".
    #[default]
    Day,
    Week,
    /// User-typed window, e.g. "6m" or "1y". `hours` is the parsed length;
    /// `label` is the input verbatim for the header chip.
    Custom {
        hours: i64,
        label: String,
    },
}

impl AccessWindow {
    pub fn duration(&self) -> chrono::Duration {
        match self {
            AccessWindow::Hour => chrono::Duration::hours(1),
            AccessWindow::Day => chrono::Duration::hours(24),
            AccessWindow::Week => chrono::Duration::days(7),
            AccessWindow::Custom { hours, .. } => chrono::Duration::hours(*hours),
        }
    }

    /// ISO-8601 timespan (see `TimeRange::timespan` for the `Z`-suffix rationale).
    pub fn timespan(&self) -> String {
        let end = Utc::now();
        let start = end - self.duration();
        format!(
            "{}/{}",
            start.to_rfc3339_opts(SecondsFormat::Secs, true),
            end.to_rfc3339_opts(SecondsFormat::Secs, true),
        )
    }

    /// Header-chip label: `1h` / `1d` / `7d` / the custom input verbatim.
    pub fn label(&self) -> String {
        match self {
            AccessWindow::Hour => "1h".to_string(),
            AccessWindow::Day => "1d".to_string(),
            AccessWindow::Week => "7d".to_string(),
            AccessWindow::Custom { label, .. } => label.clone(),
        }
    }

    /// Parse a user-typed window: `<n><unit>` where unit is `h`our, `d`ay,
    /// `w`eek, `m`onth (30 days), or `y`ear (365 days). Whitespace and case
    /// are forgiven ("6M", " 1 y"). `None` for anything else or a zero length.
    pub fn parse(input: &str) -> Option<AccessWindow> {
        let compact: String = input
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            .to_lowercase();
        let unit = compact.chars().last()?;
        let n: i64 = compact[..compact.len() - 1].parse().ok()?;
        if n <= 0 {
            return None;
        }
        let hours = match unit {
            'h' => n,
            'd' => n.checked_mul(24)?,
            'w' => n.checked_mul(24 * 7)?,
            'm' => n.checked_mul(24 * 30)?,
            'y' => n.checked_mul(24 * 365)?,
            _ => return None,
        };
        Some(AccessWindow::Custom {
            hours,
            label: compact,
        })
    }
}

/// What kind of principal a row's caller resolved to — drives the column
/// styling and the short display form.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallerKind {
    /// A human (UPN claim present).
    User,
    /// A managed identity — `identity_claim_xms_mirid_s` names the ARM
    /// resource that used the secret.
    ManagedIdentity,
    /// A bare service principal (only an app id claim).
    App,
    Unknown,
}

/// One `AuditEvent` row from `AzureDiagnostics`, already reduced to what the
/// access view shows.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AccessEvent {
    pub ts: DateTime<Utc>,
    /// `OperationName`: `SecretGet`, `SecretList`, `VaultGet`, …
    pub operation: String,
    /// Short display identity: the UPN, the managed identity's resource name,
    /// or the app id — per the resolution order in the module docs.
    pub caller: String,
    pub caller_kind: CallerKind,
    /// `CallerIPAddress`. Empty when the row carried none.
    pub ip: String,
    /// `{kind}/{name}` parsed from the data-plane URL in `id_s`
    /// (e.g. `secrets/orders-db-connection`). `None` for vault-level ops.
    pub object: Option<String>,
    /// `ResultSignature` (falls back to the HTTP status code).
    pub result: String,
    /// Full `identity_claim_xms_mirid_s` for the detail-minded (yank).
    pub mirid: Option<String>,
}

/// The signed-in principal, decoded from the bearer token's claims. Used by
/// the "exclude me" filter: a row is *you* when its UPN or caller IP matches.
/// The ACR access log matches on the object id instead — its `Identity`
/// column carries oids, not UPNs (see `crate::azure::registry_logs`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SelfIdentity {
    /// `upn` / `unique_name` claim (humans). Lowercased.
    pub upn: Option<String>,
    /// `ipaddr` claim — the client IP Entra saw at sign-in.
    pub ip: Option<String>,
    /// `oid` claim — the directory object id (present for every principal
    /// kind, unlike `upn`).
    pub oid: Option<String>,
}

impl SelfIdentity {
    pub fn is_empty(&self) -> bool {
        self.upn.is_none() && self.ip.is_none() && self.oid.is_none()
    }

    /// Short human description for the header chip. The oid is only shown
    /// when there's nothing friendlier — it's a GUID.
    pub fn label(&self) -> String {
        match (&self.upn, &self.ip) {
            (Some(u), Some(ip)) => format!("{u} / {ip}"),
            (Some(u), None) => u.clone(),
            (None, Some(ip)) => ip.clone(),
            (None, None) => self.oid.clone().unwrap_or_else(|| "unknown".to_string()),
        }
    }

    /// Decode the (unverified) JWT payload and pull the identity claims. We
    /// only introspect a token we ourselves just received from Entra over
    /// TLS, so skipping signature verification is fine — this is display /
    /// filter input, not an authorization decision.
    pub fn from_token(token: &str) -> SelfIdentity {
        let Some(payload) = token.split('.').nth(1) else {
            return SelfIdentity::default();
        };
        let Ok(bytes) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload) else {
            return SelfIdentity::default();
        };
        let Ok(claims) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            return SelfIdentity::default();
        };
        let s = |key: &str| {
            claims
                .get(key)
                .and_then(|v| v.as_str())
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
        };
        SelfIdentity {
            upn: s("upn")
                .or_else(|| s("unique_name"))
                .map(|u| u.to_lowercase()),
            ip: s("ipaddr"),
            oid: s("oid").map(|o| o.to_lowercase()),
        }
    }
}

/// One fetched page of access events plus the query metadata the header shows.
#[derive(Debug, Default)]
pub struct AccessPage {
    pub events: Vec<AccessEvent>,
    /// The page came back full — older rows in the window were cut off.
    pub truncated: bool,
    /// The identity the query excluded (when "exclude me" was on), so the
    /// header can say *who* is being hidden.
    pub hidden: Option<SelfIdentity>,
}

/// Scope the query to one item, set when the view is opened with `l` on a
/// specific secret / certificate row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemScope {
    /// Data-plane path segment: `secrets` or `certificates`.
    pub kind_segment: String,
    pub name: String,
}

impl ItemScope {
    pub fn path(&self) -> String {
        format!("{}/{}", self.kind_segment, self.name)
    }
}

pub async fn fetch(
    auth: &AzureAuth,
    vault: &KeyVault,
    window: &AccessWindow,
    scope: Option<&ItemScope>,
    exclude_self: bool,
) -> anyhow::Result<AccessPage> {
    // Resolve "me" from the same token the query will use — no extra call.
    let hidden = if exclude_self {
        let identity = SelfIdentity::from_token(&auth.token(SCOPE_LOGS).await?);
        if identity.is_empty() {
            // A service-principal login has neither a UPN nor an ipaddr claim;
            // silently excluding nothing would misleadingly relabel the data.
            return Err(anyhow!(
                "can't resolve your identity from the token (no upn/ipaddr claim) — exclude-me is unavailable for this login"
            ));
        }
        Some(identity)
    } else {
        None
    };

    let kql = build_access_kql(scope, hidden.as_ref());
    let client = LogsClient::new(auth.clone())?;
    let resp = client.query(&vault.id, &kql, &window.timespan()).await?;
    let events = parse_access_response(&resp)?;
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

/// Build the `AzureDiagnostics` audit query. Identity columns are dynamic
/// per-row in that table, so every reference goes through `column_ifexists` —
/// a vault whose rows never carried a claim must not fail the whole query.
fn build_access_kql(scope: Option<&ItemScope>, exclude: Option<&SelfIdentity>) -> String {
    let mut kql = String::from(
        r#"AzureDiagnostics
| where Category == "AuditEvent"
| extend upn_ = tolower(tostring(column_ifexists("identity_claim_upn_s", "")))
| extend upn_ = iif(isempty(upn_), tolower(tostring(column_ifexists("identity_claim_http_schemas_xmlsoap_org_ws_2005_05_identity_claims_upn_s", ""))), upn_)
| extend mirid_ = tostring(column_ifexists("identity_claim_xms_mirid_s", ""))
| extend appid_ = tostring(column_ifexists("identity_claim_appid_g", ""))
| extend oid_ = tolower(tostring(column_ifexists("identity_claim_oid_g", "")))
| extend ip_ = tostring(column_ifexists("CallerIPAddress", ""))
| extend obj_ = tostring(column_ifexists("id_s", ""))
| extend result_ = tostring(column_ifexists("ResultSignature", ""))
| extend status_ = tostring(column_ifexists("httpStatusCode_d", ""))
"#,
    );
    if let Some(scope) = scope {
        // `id_s` is the full data-plane URL (`https://{vault}/secrets/{name}/{ver}`);
        // `contains` is case-insensitive, matching Key Vault's own name rules.
        kql.push_str(&format!(
            "| where obj_ contains \"/{}\"\n",
            escape_kql(&scope.path())
        ));
    }
    if let Some(me) = exclude {
        let mut clauses = Vec::new();
        if let Some(upn) = &me.upn {
            clauses.push(format!("upn_ == \"{}\"", escape_kql(upn)));
        }
        if let Some(ip) = &me.ip {
            clauses.push(format!("ip_ == \"{}\"", escape_kql(ip)));
        }
        if let Some(oid) = &me.oid {
            clauses.push(format!("oid_ == \"{}\"", escape_kql(oid)));
        }
        if !clauses.is_empty() {
            kql.push_str(&format!("| where not({})\n", clauses.join(" or ")));
        }
    }
    kql.push_str(&format!(
        "| order by TimeGenerated desc\n| take {ACCESS_PAGE_SIZE}\n| project TimeGenerated, OperationName, upn_, mirid_, appid_, ip_, obj_, result_, status_\n"
    ));
    kql
}

fn parse_access_response(value: &serde_json::Value) -> anyhow::Result<Vec<AccessEvent>> {
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

    let (i_upn, i_mirid, i_appid, i_ip, i_obj, i_result, i_status) = (
        idx("upn_"),
        idx("mirid_"),
        idx("appid_"),
        idx("ip_"),
        idx("obj_"),
        idx("result_"),
        idx("status_"),
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
        let mirid = cell(row, i_mirid);
        let appid = cell(row, i_appid);
        let (caller, caller_kind) = resolve_caller(&upn, &mirid, &appid);
        let result = match (cell(row, i_result), cell(row, i_status)) {
            (sig, _) if !sig.is_empty() => sig,
            (_, status) if !status.is_empty() => status,
            _ => "?".to_string(),
        };
        events.push(AccessEvent {
            ts,
            operation: cell(row, Some(i_op)),
            caller,
            caller_kind,
            ip: cell(row, i_ip),
            object: object_from_url(&cell(row, i_obj)),
            result,
            mirid: (!mirid.is_empty()).then_some(mirid),
        });
    }
    Ok(events)
}

/// Resolve the display identity: UPN if it's a human, else the managed
/// identity's resource name (from `identity_claim_xms_mirid_s`), else the
/// bare app id.
fn resolve_caller(upn: &str, mirid: &str, appid: &str) -> (String, CallerKind) {
    if !upn.is_empty() {
        return (upn.to_string(), CallerKind::User);
    }
    if !mirid.is_empty() {
        // ".../providers/microsoft.app/containerapps/ca-checkout-api" — the
        // trailing segment is the resource name people recognize.
        let name = mirid.rsplit('/').next().unwrap_or(mirid);
        return (name.to_string(), CallerKind::ManagedIdentity);
    }
    if !appid.is_empty() {
        return (appid.to_string(), CallerKind::App);
    }
    ("unknown".to_string(), CallerKind::Unknown)
}

/// `https://{vault}.vault.azure.net/secrets/{name}/{version}?...` →
/// `secrets/{name}`. `None` when the URL has no item path (vault-level ops).
fn object_from_url(url: &str) -> Option<String> {
    let path = url.split("://").nth(1).unwrap_or(url);
    let mut segments = path
        .split('?')
        .next()
        .unwrap_or(path)
        .split('/')
        .skip(1)
        .filter(|s| !s.is_empty());
    let kind = segments.next()?;
    let name = segments.next()?;
    Some(format!("{kind}/{name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_window_accepts_hours_days_weeks_months_years() {
        assert_eq!(
            AccessWindow::parse("12h").unwrap().duration().num_hours(),
            12
        );
        assert_eq!(
            AccessWindow::parse("30d").unwrap().duration().num_days(),
            30
        );
        assert_eq!(AccessWindow::parse("2w").unwrap().duration().num_days(), 14);
        assert_eq!(
            AccessWindow::parse("6m").unwrap().duration().num_days(),
            180
        );
        assert_eq!(
            AccessWindow::parse("1y").unwrap().duration().num_days(),
            365
        );
        // Whitespace / case forgiven; label keeps the compact form.
        let w = AccessWindow::parse(" 6 M ").unwrap();
        assert_eq!(w.label(), "6m");
    }

    #[test]
    fn parse_window_rejects_junk() {
        for junk in ["", "m", "6", "0d", "-3d", "6x", "sixm", "6mm"] {
            assert!(AccessWindow::parse(junk).is_none(), "{junk:?} should fail");
        }
    }

    #[test]
    fn kql_scopes_to_item_and_excludes_self() {
        let scope = ItemScope {
            kind_segment: "secrets".into(),
            name: "orders-db-connection".into(),
        };
        let me = SelfIdentity {
            upn: Some("robbert@contoso.com".into()),
            ip: Some("203.0.113.7".into()),
            oid: None,
        };
        let kql = build_access_kql(Some(&scope), Some(&me));
        assert!(kql.contains(r#"obj_ contains "/secrets/orders-db-connection""#));
        assert!(kql.contains(r#"not(upn_ == "robbert@contoso.com" or ip_ == "203.0.113.7")"#));
        assert!(kql.contains("take 500"));
        // Identity columns are dynamic — every access must be guarded.
        assert!(kql.contains(r#"column_ifexists("identity_claim_xms_mirid_s""#));
        assert!(kql.contains(r#"column_ifexists("identity_claim_upn_s""#));
    }

    #[test]
    fn kql_without_filters_has_no_where_beyond_category() {
        let kql = build_access_kql(None, None);
        assert_eq!(kql.matches("| where").count(), 1, "{kql}");
    }

    #[test]
    fn caller_resolution_prefers_upn_then_mirid_then_appid() {
        let (c, k) = resolve_caller("dana@contoso.com", "/x/y/ca-app", "guid");
        assert_eq!((c.as_str(), k), ("dana@contoso.com", CallerKind::User));
        let (c, k) = resolve_caller(
            "",
            "/subs/s/rg/r/providers/microsoft.app/containerapps/ca-checkout-api",
            "guid",
        );
        assert_eq!(
            (c.as_str(), k),
            ("ca-checkout-api", CallerKind::ManagedIdentity)
        );
        let (c, k) = resolve_caller("", "", "1e2f…");
        assert_eq!((c.as_str(), k), ("1e2f…", CallerKind::App));
        assert_eq!(resolve_caller("", "", "").1, CallerKind::Unknown);
    }

    #[test]
    fn object_from_url_extracts_kind_and_name() {
        assert_eq!(
            object_from_url("https://kv.vault.azure.net/secrets/db-pass/4a5b?api-version=7.4"),
            Some("secrets/db-pass".to_string())
        );
        assert_eq!(
            object_from_url("https://kv.vault.azure.net/certificates/tls-cert"),
            Some("certificates/tls-cert".to_string())
        );
        assert_eq!(object_from_url("https://kv.vault.azure.net/"), None);
        assert_eq!(object_from_url(""), None);
    }

    #[test]
    fn self_identity_from_token_reads_upn_and_ipaddr() {
        // Fabricate an unsigned JWT: header.payload.signature.
        let payload = serde_json::json!({
            "upn": "Robbert@Contoso.com",
            "ipaddr": "203.0.113.7",
            "oid": "9E8D7C6B-5A49-4038-B2C1-D0E9F8A7B6C5",
        });
        let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
        let token = format!("{}.{}.sig", b64(b"{}"), b64(payload.to_string().as_bytes()));
        let me = SelfIdentity::from_token(&token);
        assert_eq!(me.upn.as_deref(), Some("robbert@contoso.com"));
        assert_eq!(me.ip.as_deref(), Some("203.0.113.7"));
        assert_eq!(
            me.oid.as_deref(),
            Some("9e8d7c6b-5a49-4038-b2c1-d0e9f8a7b6c5")
        );
        assert!(SelfIdentity::from_token("garbage").is_empty());
    }

    #[test]
    fn parse_access_response_resolves_rows() {
        let resp = serde_json::json!({
            "tables": [{
                "columns": [
                    {"name": "TimeGenerated"}, {"name": "OperationName"},
                    {"name": "upn_"}, {"name": "mirid_"}, {"name": "appid_"},
                    {"name": "ip_"}, {"name": "obj_"}, {"name": "result_"}, {"name": "status_"}
                ],
                "rows": [
                    ["2026-07-08T10:00:00Z", "SecretGet", "", "/s/x/providers/microsoft.app/containerapps/ca-checkout-api", "", "10.0.0.4", "https://kv.vault.azure.net/secrets/orders-db-connection/1", "OK", "200"],
                    ["2026-07-08T09:00:00Z", "VaultGet", "dana@contoso.com", "", "", "198.51.100.3", "", "", "403"]
                ]
            }]
        });
        let events = parse_access_response(&resp).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].caller, "ca-checkout-api");
        assert_eq!(events[0].caller_kind, CallerKind::ManagedIdentity);
        assert_eq!(
            events[0].object.as_deref(),
            Some("secrets/orders-db-connection")
        );
        assert_eq!(events[0].result, "OK");
        assert_eq!(events[1].caller, "dana@contoso.com");
        assert_eq!(events[1].caller_kind, CallerKind::User);
        assert_eq!(events[1].object, None);
        assert_eq!(events[1].result, "403");
    }
}
