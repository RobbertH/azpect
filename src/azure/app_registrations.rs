//! Entra ID app registrations (`/applications` via Microsoft Graph), enriched
//! best-effort with per-app **last sign-in activity** so the list answers
//! "is this registration still used?" at a glance.
//!
//! Unlike every other category, app registrations are **tenant-scoped**: they
//! don't live in a subscription and Resource Graph doesn't index them, so the
//! list fetch takes no subscription filter and the rows carry no ARM id.
//!
//! Data sources:
//! - `GET /v1.0/applications` (paged) — the registrations themselves plus
//!   their password/key credentials (metadata only: counts and expiry, never
//!   secret material). Needs `Application.Read.All` / `Directory.Read.All`.
//! - `GET /beta/reports/servicePrincipalSignInActivities` (paged) — one row
//!   per service principal with the last delegated / app-only sign-in
//!   timestamps. Beta + needs `AuditLog.Read.All` and an Entra ID P1+ tenant,
//!   so a failure degrades to "activity unavailable" instead of failing the
//!   whole list.

#![allow(dead_code, unused_variables)]

use anyhow::{anyhow, Context};
use chrono::{DateTime, Utc};

use crate::azure::auth::AzureAuth;
use crate::azure::client::{graph_page, GraphClient};

/// Soft cap on registrations returned. Beyond this we stop following
/// `@odata.nextLink` and warn — matches the precedent in `key_vault.rs`.
const MAX_APPS: usize = 5_000;

/// Graph page size for `/applications`. 999 is the documented maximum.
const PAGE_SIZE: u32 = 999;

/// One app registration, reduced to what the list view shows.
#[derive(Clone, Debug)]
pub struct AppRegistration {
    /// Directory **object id** of the application object. Distinct from
    /// `app_id` — audit filters use this, sign-in filters use `app_id`.
    pub object_id: String,
    /// The **client id** (`appId`) — what sign-in logs and connection strings
    /// carry, and what the portal blade is keyed on.
    pub app_id: String,
    pub display_name: String,
    pub created: Option<DateTime<Utc>>,
    /// `signInAudience`: `AzureADMyOrg`, `AzureADMultipleOrgs`, …
    pub sign_in_audience: Option<String>,
    /// Number of client secrets (`passwordCredentials`).
    pub secret_count: usize,
    /// Number of certificates (`keyCredentials`).
    pub cert_count: usize,
    /// Soonest end date across all credentials, expired ones included —
    /// the list flags upcoming/lapsed expiry from this.
    pub next_cred_expiry: Option<DateTime<Utc>>,
    /// How many credentials are already past their end date.
    pub expired_creds: usize,
    /// Best-effort last sign-in (max across delegated and app-only activity,
    /// from the beta report). `None` = never seen *or* activity unavailable —
    /// the page-level `activity_note` disambiguates.
    pub last_sign_in: Option<DateTime<Utc>>,
}

/// The list fetch result: the registrations plus a note when the sign-in
/// activity enrichment was unavailable (no `AuditLog.Read.All`, no P1
/// license, beta endpoint moved) — the view shows the note instead of
/// letting an empty LAST SIGN-IN column read as "nothing signs in here".
#[derive(Clone, Debug, Default)]
pub struct AppRegistrationList {
    pub apps: Vec<AppRegistration>,
    pub activity_note: Option<String>,
}

/// Enumerate the tenant's app registrations, newest activity first is NOT
/// applied here — rows come back sorted by display name; the view owns
/// ordering. Follows `@odata.nextLink` until exhausted (or [`MAX_APPS`]).
pub async fn list_app_registrations(auth: &AzureAuth) -> anyhow::Result<AppRegistrationList> {
    let client = GraphClient::new(auth.clone())?;

    let select =
        "id,appId,displayName,createdDateTime,signInAudience,passwordCredentials,keyCredentials";
    let mut resp = client
        .get(&format!("/applications?$select={select}&$top={PAGE_SIZE}"))
        .await
        .map_err(classify_applications_error)
        .context("graph: list app registrations")?;

    let mut apps: Vec<AppRegistration> = Vec::new();
    loop {
        let (rows, next) = graph_page(&resp);
        apps.extend(rows.iter().filter_map(parse_app));
        if apps.len() >= MAX_APPS {
            tracing::warn!(
                "graph /applications: stopping at {} rows; pagination cap reached",
                apps.len()
            );
            break;
        }
        match next {
            Some(link) => resp = client.get_url(&link).await?,
            None => break,
        }
    }
    apps.sort_by_key(|a| a.display_name.to_lowercase());

    // Best-effort enrichment: one report call covers every app, so the list
    // shows "last used" without a per-row fan-out. Failure is a note, never
    // an error — the registrations themselves loaded fine.
    let activity_note = match fetch_sign_in_activity(&client).await {
        Ok(by_app_id) => {
            for app in &mut apps {
                app.last_sign_in = by_app_id.get(&app.app_id).copied();
            }
            None
        }
        Err(e) => Some(format!("last sign-in unavailable: {e:#}")),
    };

    Ok(AppRegistrationList {
        apps,
        activity_note,
    })
}

/// `appId` → most recent sign-in timestamp, from the beta
/// `servicePrincipalSignInActivities` report (covers delegated *and*
/// app-only/client-credential flows — the latter is what most registrations
/// used by daemons show up as).
async fn fetch_sign_in_activity(
    client: &GraphClient,
) -> anyhow::Result<std::collections::HashMap<String, DateTime<Utc>>> {
    // No `$top`: the beta report pages on its own via `@odata.nextLink`, and
    // an unsupported query param here would needlessly kill the enrichment.
    let mut resp = client
        .get_beta("/reports/servicePrincipalSignInActivities")
        .await
        .map_err(classify_activity_error)?;
    let mut out = std::collections::HashMap::new();
    let mut total = 0usize;
    loop {
        let (rows, next) = graph_page(&resp);
        total += rows.len();
        for row in &rows {
            let Some(app_id) = row.get("appId").and_then(|v| v.as_str()) else {
                continue;
            };
            if let Some(ts) = last_activity(row) {
                out.insert(app_id.to_string(), ts);
            }
        }
        if total >= MAX_APPS {
            tracing::warn!("graph sign-in activities: stopping at {total} rows");
            break;
        }
        match next {
            Some(link) => resp = client.get_url(&link).await?,
            None => break,
        }
    }
    Ok(out)
}

/// Max timestamp across the activity blocks of one
/// `servicePrincipalSignInActivity` row. The top-level `lastSignInActivity`
/// is usually the max already, but compute it defensively — the beta shape
/// has grown blocks over time.
pub(crate) fn last_activity(row: &serde_json::Value) -> Option<DateTime<Utc>> {
    const BLOCKS: &[&str] = &[
        "lastSignInActivity",
        "delegatedClientSignInActivity",
        "delegatedResourceSignInActivity",
        "applicationAuthenticationClientSignInActivity",
        "applicationAuthenticationResourceSignInActivity",
    ];
    BLOCKS
        .iter()
        .filter_map(|b| {
            row.get(*b)
                .and_then(|v| v.get("lastSignInDateTime"))
                .and_then(|v| v.as_str())
                .and_then(parse_ts)
        })
        .max()
}

pub(crate) fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

pub(crate) fn parse_app(v: &serde_json::Value) -> Option<AppRegistration> {
    let object_id = v.get("id")?.as_str()?.to_string();
    let app_id = v
        .get("appId")
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .to_string();
    let display_name = v
        .get("displayName")
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .to_string();
    let created = v
        .get("createdDateTime")
        .and_then(|n| n.as_str())
        .and_then(parse_ts);
    let sign_in_audience = v
        .get("signInAudience")
        .and_then(|n| n.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    let cred_ends = |key: &str| -> Vec<Option<DateTime<Utc>>> {
        v.get(key)
            .and_then(|c| c.as_array())
            .map(|creds| {
                creds
                    .iter()
                    .map(|c| {
                        c.get("endDateTime")
                            .and_then(|e| e.as_str())
                            .and_then(parse_ts)
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let secrets = cred_ends("passwordCredentials");
    let certs = cred_ends("keyCredentials");
    let now = Utc::now();
    let all_ends = || secrets.iter().chain(certs.iter()).filter_map(|e| *e);
    let next_cred_expiry = all_ends().min();
    let expired_creds = all_ends().filter(|e| *e < now).count();

    Some(AppRegistration {
        object_id,
        app_id,
        display_name,
        created,
        sign_in_audience,
        secret_count: secrets.len(),
        cert_count: certs.len(),
        next_cred_expiry,
        expired_creds,
        last_sign_in: None,
    })
}

// ---------------------------------------------------------------------------
// Error classification
// ---------------------------------------------------------------------------

/// The `/applications` list needs directory read; a plain ARM `Reader` gets a
/// Graph 403 with an unhelpful body — rewrite it with the actual fix.
fn classify_applications_error(e: anyhow::Error) -> anyhow::Error {
    let msg = format!("{e:#}");
    let lower = msg.to_lowercase();
    if lower.contains("error 403") || lower.contains("authorization_requestdenied") {
        return anyhow!(
            "403 from Microsoft Graph listing app registrations: the signed-in \
             user lacks directory read. Any member user can usually read \
             applications, but a tenant that restricts this needs a directory \
             role (e.g. `Directory Readers`) or `Application.Read.All`. Note \
             azure `Reader` on subscriptions buys nothing in Entra ID.\nunderlying: {msg}"
        );
    }
    if lower.contains("error 401") {
        return anyhow!(
            "401 from Microsoft Graph: token rejected. Re-run `az login` to \
             refresh the session.\nunderlying: {msg}"
        );
    }
    e
}

/// Sign-in reports are the most permission- and license-gated Graph surface
/// azpect touches; classify the two common refusals into actionable text.
pub(crate) fn classify_activity_error(e: anyhow::Error) -> anyhow::Error {
    let msg = format!("{e:#}");
    let lower = msg.to_lowercase();
    if lower.contains("nonpremiumtenant") || lower.contains("premium license") {
        return anyhow!(
            "tenant has no Entra ID P1/P2 license — Azure AD sign-in reports are a premium feature"
        );
    }
    if lower.contains("error 403") || lower.contains("authorization_requestdenied") {
        return anyhow!(
            "needs the `Reports Reader`, `Security Reader`, or `Global Reader` \
             directory role (AuditLog.Read.All)"
        );
    }
    e
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_app_row_with_credentials() {
        let now = Utc::now();
        let past = (now - chrono::Duration::days(10)).to_rfc3339();
        let soon = (now + chrono::Duration::days(20)).to_rfc3339();
        let later = (now + chrono::Duration::days(300)).to_rfc3339();
        let row = json!({
            "id": "11111111-2222-3333-4444-555555555555",
            "appId": "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
            "displayName": "Contoso Orders API",
            "createdDateTime": "2023-04-01T09:00:00Z",
            "signInAudience": "AzureADMyOrg",
            "passwordCredentials": [
                { "endDateTime": past },
                { "endDateTime": later }
            ],
            "keyCredentials": [
                { "endDateTime": soon }
            ]
        });
        let app = parse_app(&row).expect("expected app");
        assert_eq!(app.display_name, "Contoso Orders API");
        assert_eq!(app.app_id, "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
        assert_eq!(app.secret_count, 2);
        assert_eq!(app.cert_count, 1);
        assert_eq!(app.expired_creds, 1);
        // The soonest end date is the already-expired one.
        assert!(app.next_cred_expiry.unwrap() < now);
        assert_eq!(app.sign_in_audience.as_deref(), Some("AzureADMyOrg"));
        assert!(app.created.is_some());
        assert!(app.last_sign_in.is_none());
    }

    #[test]
    fn parses_app_row_without_credentials() {
        let row = json!({
            "id": "x",
            "appId": "y",
            "displayName": "bare"
        });
        let app = parse_app(&row).expect("expected app");
        assert_eq!(app.secret_count, 0);
        assert_eq!(app.cert_count, 0);
        assert_eq!(app.expired_creds, 0);
        assert!(app.next_cred_expiry.is_none());
    }

    #[test]
    fn last_activity_takes_max_across_blocks() {
        let row = json!({
            "appId": "a",
            "lastSignInActivity": { "lastSignInDateTime": "2026-08-01T10:00:00Z" },
            "applicationAuthenticationClientSignInActivity": {
                "lastSignInDateTime": "2026-08-20T04:30:00Z"
            },
            "delegatedClientSignInActivity": {
                "lastSignInDateTime": "2026-05-02T08:00:00Z"
            }
        });
        let ts = last_activity(&row).expect("expected timestamp");
        assert_eq!(ts, parse_ts("2026-08-20T04:30:00Z").unwrap());
        assert!(last_activity(&json!({ "appId": "b" })).is_none());
    }

    #[test]
    fn classifies_403_and_license_errors() {
        let msg = format!(
            "{}",
            classify_applications_error(anyhow!(
                "azure api error 403: Authorization_RequestDenied"
            ))
        );
        assert!(msg.contains("Application.Read.All"), "got: {msg}");

        let msg = format!(
            "{}",
            classify_activity_error(anyhow!(
                "azure api error 403: Authentication_RequestFromNonPremiumTenantOrB2CTenant"
            ))
        );
        assert!(msg.contains("P1/P2"), "got: {msg}");

        let msg = format!(
            "{}",
            classify_activity_error(anyhow!("azure api error 403: Authorization_RequestDenied"))
        );
        assert!(msg.contains("Reports Reader"), "got: {msg}");
    }
}
