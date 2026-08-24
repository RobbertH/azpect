//! Sign-in (audit) logs for one app registration, read from Microsoft Graph
//! `auditLogs/signIns` — the concrete "how much is this app used" trail:
//! every interactive user sign-in, non-interactive token refresh, service
//! principal (client-credential) grant, and managed-identity sign-in where
//! this registration was the **client** (`appId`).
//!
//! Unlike the other access-log views this does not go through Log Analytics —
//! Graph needs no diagnostic-setting prerequisite. The trade-off is retention:
//! Entra keeps sign-in logs 7 days (Free) or 30 days (P1/P2), so windows past
//! `30d` legitimately return nothing more.
//!
//! Query strategy: the **beta** endpoint first, with a `signInEventTypes`
//! filter that pulls all four event classes (most registrations are daemons —
//! their usage is invisible in the interactive-only default). If beta refuses,
//! fall back to v1.0 without the event-type filter (interactive sign-ins
//! only) and say so via [`SignInPage::note`].

#![allow(dead_code, unused_variables)]

use anyhow::anyhow;
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use crate::azure::app_registrations::{classify_activity_error, parse_ts};
use crate::azure::auth::{AzureAuth, SCOPE_GRAPH};
use crate::azure::client::{graph_page, GraphClient};
use crate::azure::key_vault_logs::{AccessWindow, SelfIdentity};

/// Rows per fetch. A busy daemon can produce thousands of client-credential
/// sign-ins a day; one bounded page keeps the fetch snappy and the UI says
/// "newest 500" via [`SignInPage::truncated`].
pub const SIGN_IN_PAGE_SIZE: usize = 500;

/// What class of sign-in a row is — drives column styling and the Tab filter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SignInKind {
    /// A human at a browser / CLI prompt.
    Interactive,
    /// Silent token acquisition on a user's behalf (refresh tokens, SSO).
    NonInteractive,
    /// Client-credential flow — the app itself, no user. This is what daemon
    /// usage looks like.
    ServicePrincipal,
    ManagedIdentity,
    Unknown,
}

impl SignInKind {
    /// Short label for the KIND column and the Tab filter chip.
    pub fn label(self) -> &'static str {
        match self {
            SignInKind::Interactive => "interactive",
            SignInKind::NonInteractive => "non-interactive",
            SignInKind::ServicePrincipal => "service principal",
            SignInKind::ManagedIdentity => "managed identity",
            SignInKind::Unknown => "unknown",
        }
    }
}

/// One sign-in event, reduced to what the view shows.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SignInEvent {
    pub ts: DateTime<Utc>,
    pub kind: SignInKind,
    /// Who signed in: the UPN for user flows, the service principal's display
    /// name for app flows, or `"unknown"`.
    pub caller: String,
    /// `ipAddress`. Empty when the row carried none (common for MSI).
    pub ip: String,
    /// `resourceDisplayName` — the API the token was issued **for**
    /// (e.g. `Microsoft Graph`, `Azure Key Vault`).
    pub resource: String,
    /// `clientAppUsed` (`Browser`, `Mobile Apps and Desktop clients`, …).
    /// Often empty for non-user flows.
    pub client_app: String,
    /// `status.errorCode`: `OK` for 0, the AADSTS error number otherwise.
    pub result: String,
    /// `status.failureReason` for failed rows — yank-only detail.
    pub failure_reason: Option<String>,
    /// `location.city, location.countryOrRegion` when present.
    pub location: String,
}

/// One fetched page of sign-ins plus the query metadata the header shows.
#[derive(Debug, Default)]
pub struct SignInPage {
    pub events: Vec<SignInEvent>,
    /// The page hit [`SIGN_IN_PAGE_SIZE`] — older rows in the window exist.
    pub truncated: bool,
    /// The identity the client-side "exclude me" filter hid, for the header.
    pub hidden: Option<SelfIdentity>,
    /// Degradation note (e.g. fell back to interactive-only v1.0 rows).
    pub note: Option<String>,
}

pub async fn fetch(
    auth: &AzureAuth,
    app_id: &str,
    window: &AccessWindow,
    exclude_self: bool,
) -> anyhow::Result<SignInPage> {
    // Resolve "me" from the same token the query will use — no extra call.
    let hidden = if exclude_self {
        let identity = SelfIdentity::from_token(&auth.token(SCOPE_GRAPH).await?);
        if identity.is_empty() {
            return Err(anyhow!(
                "can't resolve your identity from the token (no upn/oid claim) — exclude-me is unavailable for this login"
            ));
        }
        Some(identity)
    } else {
        None
    };

    let client = GraphClient::new(auth.clone())?;
    let start = Utc::now() - window.duration();

    // Beta first: `signInEventTypes` pulls non-interactive / service-principal
    // / managed-identity rows, which is where daemon usage lives.
    let (mut resp, note, _beta) = match client.get_beta(&sign_ins_path(app_id, &start, true)).await
    {
        Ok(resp) => (resp, None, true),
        Err(beta_err) => {
            let resp = client
                .get(&sign_ins_path(app_id, &start, false))
                .await
                .map_err(classify_sign_ins_error)?;
            let why = classify_activity_error(beta_err);
            (
                resp,
                Some(format!(
                    "interactive sign-ins only — full event types unavailable: {why:#}"
                )),
                false,
            )
        }
    };

    let mut events: Vec<SignInEvent> = Vec::new();
    let mut truncated = false;
    loop {
        let (rows, next) = graph_page(&resp);
        events.extend(rows.iter().filter_map(parse_sign_in));
        if events.len() >= SIGN_IN_PAGE_SIZE {
            truncated = true;
            events.truncate(SIGN_IN_PAGE_SIZE);
            break;
        }
        match next {
            Some(link) => resp = client.get_url(&link).await?,
            None => break,
        }
    }

    // Graph returns newest-first, but don't rely on it — the view's WHEN
    // column promises descending order.
    events.sort_by_key(|e| std::cmp::Reverse(e.ts));

    // Client-side "exclude me": Graph's sign-in filter has no `ne`, so the
    // exclusion happens after the fetch (unlike the KQL-backed views).
    if let Some(me) = &hidden {
        events.retain(|e| !is_self(e, me));
    }

    Ok(SignInPage {
        events,
        truncated,
        hidden,
        note,
    })
}

/// Does this row describe the signed-in azpect user? Matched on lowercased
/// UPN or sign-in IP — the same claims the KQL views exclude server-side.
fn is_self(event: &SignInEvent, me: &SelfIdentity) -> bool {
    if let Some(upn) = &me.upn {
        if event.caller.to_lowercase() == *upn {
            return true;
        }
    }
    if let Some(ip) = &me.ip {
        if !event.ip.is_empty() && event.ip == *ip {
            return true;
        }
    }
    false
}

/// Build the `auditLogs/signIns` path. `event_types` adds the beta-only
/// `signInEventTypes` clause covering all four event classes. Spaces are
/// percent-encoded by hand — the rest of the OData filter is URL-safe.
fn sign_ins_path(app_id: &str, start: &DateTime<Utc>, event_types: bool) -> String {
    // `'` is the OData string delimiter; app ids are GUIDs from Graph, but
    // escape defensively anyway.
    let app_id = app_id.replace('\'', "''");
    let start = start.to_rfc3339_opts(SecondsFormat::Secs, true);
    let mut filter = format!("createdDateTime ge {start} and appId eq '{app_id}'");
    if event_types {
        filter.push_str(
            " and (signInEventTypes/any(t: t eq 'interactiveUser' \
             or t eq 'nonInteractiveUser' or t eq 'servicePrincipal' \
             or t eq 'managedIdentity'))",
        );
    }
    format!(
        "/auditLogs/signIns?$top=500&$filter={}",
        filter.replace(' ', "%20")
    )
}

fn classify_sign_ins_error(e: anyhow::Error) -> anyhow::Error {
    // Same permission/license gates as the activity report.
    classify_activity_error(e)
}

pub(crate) fn parse_sign_in(v: &serde_json::Value) -> Option<SignInEvent> {
    let ts = v
        .get("createdDateTime")
        .and_then(|t| t.as_str())
        .and_then(parse_ts)?;
    let s = |key: &str| -> String {
        v.get(key)
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string()
    };

    let upn = s("userPrincipalName");
    let sp_name = s("servicePrincipalName");
    let sp_id = s("servicePrincipalId");
    let kind = resolve_kind(v, &upn, &sp_id);
    let caller = if !upn.is_empty() {
        upn
    } else if !sp_name.is_empty() {
        sp_name
    } else if !sp_id.is_empty() {
        sp_id
    } else {
        "unknown".to_string()
    };

    let status = v.get("status");
    let error_code = status
        .and_then(|st| st.get("errorCode"))
        .and_then(|c| c.as_i64())
        .unwrap_or(0);
    let result = if error_code == 0 {
        "OK".to_string()
    } else {
        error_code.to_string()
    };
    let failure_reason = status
        .and_then(|st| st.get("failureReason"))
        .and_then(|r| r.as_str())
        .filter(|r| !r.is_empty() && *r != "Other.")
        .map(str::to_owned);

    let location = match (
        v.get("location")
            .and_then(|l| l.get("city"))
            .and_then(|c| c.as_str())
            .filter(|c| !c.is_empty()),
        v.get("location")
            .and_then(|l| l.get("countryOrRegion"))
            .and_then(|c| c.as_str())
            .filter(|c| !c.is_empty()),
    ) {
        (Some(city), Some(country)) => format!("{city}, {country}"),
        (Some(one), None) | (None, Some(one)) => one.to_string(),
        (None, None) => String::new(),
    };

    Some(SignInEvent {
        ts,
        kind,
        caller,
        ip: s("ipAddress"),
        resource: s("resourceDisplayName"),
        client_app: s("clientAppUsed"),
        result,
        failure_reason,
        location,
    })
}

/// Event kind: the beta `signInEventTypes` array when present; otherwise
/// inferred — a row with a service principal and no user is an app flow, and
/// a v1.0 row with a user is interactive (v1.0 returns nothing else without
/// the event-type filter).
fn resolve_kind(v: &serde_json::Value, upn: &str, sp_id: &str) -> SignInKind {
    let types: Vec<&str> = v
        .get("signInEventTypes")
        .and_then(|t| t.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
        .unwrap_or_default();
    for t in &types {
        match *t {
            "interactiveUser" => return SignInKind::Interactive,
            "nonInteractiveUser" => return SignInKind::NonInteractive,
            "servicePrincipal" => return SignInKind::ServicePrincipal,
            "managedIdentity" => return SignInKind::ManagedIdentity,
            _ => {}
        }
    }
    if !sp_id.is_empty() && upn.is_empty() {
        SignInKind::ServicePrincipal
    } else if !upn.is_empty() {
        SignInKind::Interactive
    } else {
        SignInKind::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn path_encodes_filter_and_scopes_event_types() {
        let start = parse_ts("2026-08-01T00:00:00Z").unwrap();
        let path = sign_ins_path("aaaa-bbbb", &start, true);
        assert!(path.starts_with("/auditLogs/signIns?$top=500&$filter="));
        assert!(
            !path.contains(' '),
            "spaces must be percent-encoded: {path}"
        );
        assert!(path.contains("appId%20eq%20'aaaa-bbbb'"));
        assert!(path.contains("signInEventTypes"));
        // v1.0 fallback drops the event-type clause.
        let path = sign_ins_path("aaaa-bbbb", &start, false);
        assert!(!path.contains("signInEventTypes"));
        // OData quote escape.
        let path = sign_ins_path("a'b", &start, false);
        assert!(path.contains("'a''b'"));
    }

    #[test]
    fn parses_interactive_user_row() {
        let row = json!({
            "createdDateTime": "2026-08-20T10:15:00Z",
            "userPrincipalName": "dana@contoso.com",
            "appId": "app",
            "ipAddress": "198.51.100.23",
            "clientAppUsed": "Browser",
            "resourceDisplayName": "Microsoft Graph",
            "signInEventTypes": ["interactiveUser"],
            "status": { "errorCode": 0 },
            "location": { "city": "Amsterdam", "countryOrRegion": "NL" }
        });
        let e = parse_sign_in(&row).expect("expected event");
        assert_eq!(e.kind, SignInKind::Interactive);
        assert_eq!(e.caller, "dana@contoso.com");
        assert_eq!(e.result, "OK");
        assert_eq!(e.location, "Amsterdam, NL");
        assert_eq!(e.client_app, "Browser");
    }

    #[test]
    fn parses_service_principal_row_and_failure() {
        let row = json!({
            "createdDateTime": "2026-08-20T04:00:00Z",
            "servicePrincipalId": "sp-guid",
            "servicePrincipalName": "Contoso Orders API",
            "ipAddress": "10.0.1.12",
            "resourceDisplayName": "Azure Key Vault",
            "signInEventTypes": ["servicePrincipal"],
            "status": { "errorCode": 7000215, "failureReason": "Invalid client secret provided." }
        });
        let e = parse_sign_in(&row).expect("expected event");
        assert_eq!(e.kind, SignInKind::ServicePrincipal);
        assert_eq!(e.caller, "Contoso Orders API");
        assert_eq!(e.result, "7000215");
        assert_eq!(
            e.failure_reason.as_deref(),
            Some("Invalid client secret provided.")
        );
    }

    #[test]
    fn infers_kind_without_event_types() {
        // v1.0 rows carry no `signInEventTypes`.
        let user = json!({
            "createdDateTime": "2026-08-20T10:00:00Z",
            "userPrincipalName": "dana@contoso.com",
            "status": { "errorCode": 0 }
        });
        assert_eq!(parse_sign_in(&user).unwrap().kind, SignInKind::Interactive);
        let sp = json!({
            "createdDateTime": "2026-08-20T10:00:00Z",
            "servicePrincipalId": "sp-guid",
            "status": { "errorCode": 0 }
        });
        assert_eq!(
            parse_sign_in(&sp).unwrap().kind,
            SignInKind::ServicePrincipal
        );
    }

    #[test]
    fn is_self_matches_upn_case_insensitively_and_ip() {
        let me = SelfIdentity {
            upn: Some("robbert@contoso.com".into()),
            ip: Some("203.0.113.7".into()),
            oid: None,
        };
        let mut e = parse_sign_in(&json!({
            "createdDateTime": "2026-08-20T10:00:00Z",
            "userPrincipalName": "Robbert@Contoso.com",
            "ipAddress": "1.2.3.4",
            "status": { "errorCode": 0 }
        }))
        .unwrap();
        assert!(is_self(&e, &me), "upn match, case-insensitive");
        e.caller = "dana@contoso.com".into();
        assert!(!is_self(&e, &me));
        e.ip = "203.0.113.7".into();
        assert!(is_self(&e, &me), "ip match");
        // An empty row IP must never match.
        e.ip = String::new();
        assert!(!is_self(&e, &me));
    }

    #[test]
    fn rows_without_timestamp_are_skipped() {
        assert!(parse_sign_in(&json!({ "userPrincipalName": "x" })).is_none());
    }
}
