//! Resolve a directory principal object-id — the GUID `systemData.createdBy` /
//! `lastModifiedBy` carries for `Application` and `ManagedIdentity` authors — to
//! a human display name via Microsoft Graph.
//!
//! Best-effort and non-critical: the detail view falls back to the raw GUID, so
//! a 403 (no directory-read permission), 404, or offline error is fine — the
//! caller caches the failure and stops retrying.

#![allow(dead_code, unused_variables)]

use anyhow::Context;

use crate::azure::auth::AzureAuth;
use crate::azure::client::GraphClient;

/// Resolve `object_id` to its `displayName`. `Ok(None)` means Graph answered but
/// the object has no display name; `Err` means the lookup itself failed (which
/// the caller treats the same as "couldn't resolve").
pub async fn resolve_display_name(
    auth: &AzureAuth,
    object_id: &str,
) -> anyhow::Result<Option<String>> {
    let client = GraphClient::new(auth.clone())?;
    // `/directoryObjects/{id}` resolves users, service principals, and managed
    // identities alike; `$select` keeps the payload to just the name.
    let path = format!("/directoryObjects/{object_id}?$select=displayName");
    let resp = client
        .get(&path)
        .await
        .with_context(|| format!("resolving principal {object_id}"))?;
    Ok(display_name(&resp))
}

fn display_name(resp: &serde_json::Value) -> Option<String> {
    resp.get("displayName")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_display_name() {
        let resp = json!({ "id": "x", "displayName": "di-sp-adp-devops-agent" });
        assert_eq!(
            display_name(&resp).as_deref(),
            Some("di-sp-adp-devops-agent")
        );
    }

    #[test]
    fn missing_or_empty_display_name_is_none() {
        assert!(display_name(&json!({ "id": "x" })).is_none());
        assert!(display_name(&json!({ "displayName": "" })).is_none());
    }
}
