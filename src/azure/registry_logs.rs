//! Container Registry access (audit) logs — who pulled / pushed which image,
//! when, from where — read from the `ContainerRegistryRepositoryEvents` table
//! via the resource-centric Log Analytics query endpoint. Requires the
//! registry to have a diagnostic setting forwarding `RepositoryEvents` to a
//! workspace — without one the query legitimately returns zero rows.
//!
//! Caller identity: unlike Key Vault's `AzureDiagnostics` rows (which carry a
//! spread of `identity_claim_*` columns), ACR emits a single `Identity`
//! column. What's in it depends on how the caller authenticated:
//! - a UPN for humans (`az acr login` on a user account),
//! - a directory object id (GUID) for service principals / managed
//!   identities / AKS kubelets — the UI resolves those to display names via
//!   Microsoft Graph, best-effort,
//! - the registry's own name for the built-in admin user,
//! - empty for anonymous pulls.

#![allow(dead_code, unused_variables)]

use anyhow::anyhow;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::azure::auth::{AzureAuth, SCOPE_LOGS};
use crate::azure::client::LogsClient;
use crate::azure::key_vault_logs::{AccessWindow, SelfIdentity};
use crate::azure::registries::Registry;

/// Rows per query — same single-page discipline as
/// [`crate::azure::key_vault_logs::ACCESS_PAGE_SIZE`]: a full page sets
/// [`AccessPage::truncated`] so the UI can say "newest 500" instead of
/// implying completeness.
pub const ACCESS_PAGE_SIZE: u32 = 500;

/// What the `Identity` column resolved to — drives the column styling and
/// whether the UI attempts a Graph display-name lookup.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallerKind {
    /// A human — the identity is a UPN.
    User,
    /// A directory object id (service principal, managed identity, or a user
    /// authenticated in a way that logged the oid). Graph-resolvable.
    Principal,
    /// The registry's built-in admin user (identity == registry name) — a
    /// static shared credential, worth flagging.
    Admin,
    /// Empty identity — an anonymous pull.
    Anonymous,
    /// Anything else (e.g. an ACR repository-scoped token name).
    Unknown,
}

/// One `ContainerRegistryRepositoryEvents` row, reduced to what the access
/// view shows.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AccessEvent {
    pub ts: DateTime<Utc>,
    /// `OperationName`: `Pull`, `Push`, `Untag`, `Delete`.
    pub operation: String,
    /// Raw `Identity` column — a UPN, an object id, the admin user name, or
    /// empty for anonymous. The view maps object ids to Graph display names.
    pub identity: String,
    pub caller_kind: CallerKind,
    /// `CallerIpAddress`. Empty when the row carried none.
    pub ip: String,
    /// `Repository` (e.g. `team/svc`). Empty on malformed rows.
    pub repository: String,
    /// `Tag` — absent for digest-addressed pulls and for `Delete`.
    pub tag: Option<String>,
    /// `Digest` (`sha256:…`), when the row carried one.
    pub digest: Option<String>,
    /// `ResultDescription`. Usually empty on success.
    pub result: String,
}

impl AccessEvent {
    /// Image reference for display: `repo:tag`, else `repo@sha256:abcd1234…`
    /// (digest shortened), else the bare repository.
    pub fn image(&self) -> String {
        if let Some(tag) = self.tag.as_deref().filter(|t| !t.is_empty()) {
            return format!("{}:{}", self.repository, tag);
        }
        if let Some(digest) = self.digest.as_deref().filter(|d| !d.is_empty()) {
            let short: String = digest.chars().take(19).collect(); // sha256: + 12 hex
            return format!("{}@{}…", self.repository, short);
        }
        self.repository.clone()
    }
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
    registry: &Registry,
    window: &AccessWindow,
    repository: Option<&str>,
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

    let kql = build_access_kql(repository, hidden.as_ref());
    let client = LogsClient::new(auth.clone())?;
    let resp = client.query(&registry.id, &kql, &window.timespan()).await?;
    let events = parse_access_response(&resp, &registry.name)?;
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

/// Build the repository-events query. `ContainerRegistryRepositoryEvents` is
/// a resource-specific table with a fixed schema, but every optional column
/// still goes through `column_ifexists` — matching the Key Vault module's
/// defensive posture against schema drift across clouds / API versions.
fn build_access_kql(repository: Option<&str>, exclude: Option<&SelfIdentity>) -> String {
    let mut kql = String::from(
        r#"ContainerRegistryRepositoryEvents
| extend identity_ = tostring(column_ifexists("Identity", ""))
| extend ip_ = tostring(column_ifexists("CallerIpAddress", ""))
| extend repo_ = tostring(column_ifexists("Repository", ""))
| extend tag_ = tostring(column_ifexists("Tag", ""))
| extend digest_ = tostring(column_ifexists("Digest", ""))
| extend result_ = tostring(column_ifexists("ResultDescription", ""))
"#,
    );
    if let Some(repo) = repository {
        // `=~` is case-insensitive equals; ACR repository names are
        // lowercase-enforced but pinned values may come from user input.
        kql.push_str(&format!("| where repo_ =~ \"{}\"\n", escape_kql(repo)));
    }
    if let Some(me) = exclude {
        let mut clauses = Vec::new();
        if let Some(upn) = &me.upn {
            clauses.push(format!("tolower(identity_) == \"{}\"", escape_kql(upn)));
        }
        if let Some(oid) = &me.oid {
            clauses.push(format!("tolower(identity_) == \"{}\"", escape_kql(oid)));
        }
        if let Some(ip) = &me.ip {
            clauses.push(format!("ip_ == \"{}\"", escape_kql(ip)));
        }
        if !clauses.is_empty() {
            kql.push_str(&format!("| where not({})\n", clauses.join(" or ")));
        }
    }
    kql.push_str(&format!(
        "| order by TimeGenerated desc\n| take {ACCESS_PAGE_SIZE}\n| project TimeGenerated, OperationName, identity_, ip_, repo_, tag_, digest_, result_\n"
    ));
    kql
}

/// Classify the raw `Identity` column — see the module docs for the shapes.
pub(crate) fn classify_identity(identity: &str, registry_name: &str) -> CallerKind {
    if identity.is_empty() {
        return CallerKind::Anonymous;
    }
    if identity.eq_ignore_ascii_case(registry_name) {
        return CallerKind::Admin;
    }
    // `graph_candidate` recognizes GUIDs / `clientId@tenantId` / SIDs — the
    // Graph-resolvable shapes.
    if crate::azure::sql_audit::graph_candidate(identity).is_some() {
        return CallerKind::Principal;
    }
    if identity.contains('@') {
        return CallerKind::User;
    }
    CallerKind::Unknown
}

fn parse_access_response(
    value: &serde_json::Value,
    registry_name: &str,
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

    let (i_identity, i_ip, i_repo, i_tag, i_digest, i_result) = (
        idx("identity_"),
        idx("ip_"),
        idx("repo_"),
        idx("tag_"),
        idx("digest_"),
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
        let identity = cell(row, i_identity);
        let caller_kind = classify_identity(&identity, registry_name);
        let tag = cell(row, i_tag);
        let digest = cell(row, i_digest);
        events.push(AccessEvent {
            ts,
            operation: cell(row, Some(i_op)),
            identity,
            caller_kind,
            ip: cell(row, i_ip),
            repository: cell(row, i_repo),
            tag: (!tag.is_empty()).then_some(tag),
            digest: (!digest.is_empty()).then_some(digest),
            result: cell(row, i_result),
        });
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kql_scopes_to_repository_and_excludes_self() {
        let me = SelfIdentity {
            upn: Some("robbert@contoso.com".into()),
            ip: Some("203.0.113.7".into()),
            oid: Some("9e8d7c6b-5a49-4038-b2c1-d0e9f8a7b6c5".into()),
        };
        let kql = build_access_kql(Some("team/svc"), Some(&me));
        assert!(kql.contains(r#"repo_ =~ "team/svc""#));
        assert!(kql.contains(r#"tolower(identity_) == "robbert@contoso.com""#));
        assert!(kql.contains(r#"tolower(identity_) == "9e8d7c6b-5a49-4038-b2c1-d0e9f8a7b6c5""#));
        assert!(kql.contains(r#"ip_ == "203.0.113.7""#));
        assert!(kql.contains("take 500"));
        // The table's optional columns must all be schema-drift-guarded.
        assert!(kql.contains(r#"column_ifexists("Identity""#));
        assert!(kql.contains(r#"column_ifexists("Digest""#));
    }

    #[test]
    fn kql_without_filters_has_no_where() {
        let kql = build_access_kql(None, None);
        assert_eq!(kql.matches("| where").count(), 0, "{kql}");
    }

    #[test]
    fn classify_identity_covers_all_shapes() {
        assert_eq!(
            classify_identity("dana@contoso.com", "myreg"),
            CallerKind::User
        );
        assert_eq!(
            classify_identity("f3c9a2e1-0d4b-4f7e-9a1c-2b5d8e7f6a3c", "myreg"),
            CallerKind::Principal
        );
        assert_eq!(classify_identity("myreg", "myreg"), CallerKind::Admin);
        assert_eq!(classify_identity("MyReg", "myreg"), CallerKind::Admin);
        assert_eq!(classify_identity("", "myreg"), CallerKind::Anonymous);
        assert_eq!(
            classify_identity("ci-pull-token", "myreg"),
            CallerKind::Unknown
        );
    }

    #[test]
    fn image_prefers_tag_then_digest_then_repo() {
        let mut e = AccessEvent {
            ts: Utc::now(),
            operation: "Pull".into(),
            identity: String::new(),
            caller_kind: CallerKind::Anonymous,
            ip: String::new(),
            repository: "team/svc".into(),
            tag: Some("1.7.3".into()),
            digest: Some(
                "sha256:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08".into(),
            ),
            result: String::new(),
        };
        assert_eq!(e.image(), "team/svc:1.7.3");
        e.tag = None;
        assert_eq!(e.image(), "team/svc@sha256:9f86d081884c…");
        e.digest = None;
        assert_eq!(e.image(), "team/svc");
    }

    #[test]
    fn parse_access_response_resolves_rows() {
        let resp = serde_json::json!({
            "tables": [{
                "columns": [
                    {"name": "TimeGenerated"}, {"name": "OperationName"},
                    {"name": "identity_"}, {"name": "ip_"}, {"name": "repo_"},
                    {"name": "tag_"}, {"name": "digest_"}, {"name": "result_"}
                ],
                "rows": [
                    ["2026-07-08T10:00:00Z", "Pull", "f3c9a2e1-0d4b-4f7e-9a1c-2b5d8e7f6a3c", "10.240.0.4", "ca-checkout-api", "1.7.3", "sha256:9f86d081884c7d65", ""],
                    ["2026-07-08T09:00:00Z", "Push", "dana@contoso.com", "198.51.100.3", "base/dotnet-runtime", "", "", "denied: pushes disabled"]
                ]
            }]
        });
        let events = parse_access_response(&resp, "crcontosoprod").unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].caller_kind, CallerKind::Principal);
        assert_eq!(events[0].image(), "ca-checkout-api:1.7.3");
        assert_eq!(events[0].ip, "10.240.0.4");
        assert_eq!(events[1].caller_kind, CallerKind::User);
        assert_eq!(events[1].tag, None);
        assert_eq!(events[1].image(), "base/dotnet-runtime");
        assert_eq!(events[1].result, "denied: pushes disabled");
    }
}
