//! Fetch a Function App's application settings — its OS environment variables —
//! via the ARM `config/appsettings/list` POST action.
//!
//! ## Permissions caveat
//! This is a `.../list` *action* that returns secret values, so it needs more
//! than `Microsoft.Web/sites/read`: a principal with only `Reader` gets 403.
//! The caller surfaces that as a friendly "needs config/list permission" hint
//! rather than an error toast (see the detail view), because azpect is happy to
//! run read-only and app settings are a non-critical decoration.

#![allow(dead_code, unused_variables)]

use anyhow::Context;

use crate::azure::auth::AzureAuth;
use crate::azure::client::ArmClient;
use crate::azure::env_vars::{from_app_settings, EnvVar};

/// Microsoft.Web API version exposing the appsettings list action.
const API_VERSION: &str = "2023-12-01";

pub async fn fetch(auth: &AzureAuth, function_app_id: &str) -> anyhow::Result<Vec<EnvVar>> {
    let client = ArmClient::new(auth.clone())?;
    fetch_via(&client, function_app_id).await
}

/// The list-action POST behind [`fetch`], reusable with an existing client —
/// [`update`] calls it to re-read the live settings just before writing.
async fn fetch_via(client: &ArmClient, function_app_id: &str) -> anyhow::Result<Vec<EnvVar>> {
    // ARM `post()` builds `{ARM_BASE}{path}`; resource ids start with `/`.
    let path = format!("{function_app_id}/config/appsettings/list?api-version={API_VERSION}");
    let resp = client
        .post(&path, &serde_json::json!({}))
        .await
        .with_context(|| format!("listing app settings for {function_app_id}"))?;
    Ok(extract(&resp))
}

/// Pull the settings out of the list-action response (`{ "properties": {...} }`).
pub fn extract(resp: &serde_json::Value) -> Vec<EnvVar> {
    match resp.get("properties") {
        Some(props) => from_app_settings(props),
        None => Vec::new(),
    }
}

/// Write one application setting: upsert `name = value` on top of the app's
/// **live** map.
///
/// `config/appsettings` is a *full-replace* collection — a `PUT` sets the
/// entire map — so the write is a read-modify-write: re-fetch the live
/// settings here (shrinking the stale window from "however long the detail
/// view has been open" to milliseconds), apply the single edit, and PUT the
/// result. Taking just the edited entry (rather than the caller's cached
/// list) means keys added *or changed* server-side since the UI last fetched
/// are preserved verbatim; this path can never delete or revert one.
///
/// Needs write permission (`Microsoft.Web/sites/config/write`); a Reader gets
/// 403, surfaced verbatim to the confirmation modal.
pub async fn update(
    auth: &AzureAuth,
    function_app_id: &str,
    name: &str,
    value: &str,
) -> anyhow::Result<()> {
    let client = ArmClient::new(auth.clone())?;
    let current = fetch_via(&client, function_app_id).await?;
    let props = properties_with_edit(&current, name, value);
    let body = serde_json::json!({ "properties": props });
    let path = format!("{function_app_id}/config/appsettings?api-version={API_VERSION}");
    client
        .put(&path, &body)
        .await
        .with_context(|| format!("updating app settings for {function_app_id}"))?;
    Ok(())
}

/// Live settings as the base, the single edit upserted on top. Pure + testable
/// half of [`update`].
fn properties_with_edit(
    current: &[EnvVar],
    name: &str,
    value: &str,
) -> serde_json::Map<String, serde_json::Value> {
    let mut props = build_properties(current);
    props.insert(name.to_string(), serde_json::Value::String(value.into()));
    props
}

/// Flatten the env-var list back into the `{ "KEY": "value", ... }` object the
/// `config/appsettings` endpoint expects. The display value IS the real value
/// for app settings (including `@Microsoft.KeyVault(...)` references), so this
/// round-trips losslessly.
fn build_properties(settings: &[EnvVar]) -> serde_json::Map<String, serde_json::Value> {
    settings
        .iter()
        .map(|ev| (ev.name.clone(), serde_json::Value::String(ev.value.clone())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_settings_sorted_with_keyvault_flag() {
        let resp = json!({
            "id": "/subscriptions/s/.../config/appsettings",
            "properties": {
                "FUNCTIONS_WORKER_RUNTIME": "dotnet",
                "MyConn": "@Microsoft.KeyVault(SecretUri=https://v.vault.azure.net/secrets/conn/)"
            }
        });
        let vars = extract(&resp);
        assert_eq!(vars.len(), 2);
        // Sorted by name: FUNCTIONS_WORKER_RUNTIME, MyConn.
        assert_eq!(vars[0].name, "FUNCTIONS_WORKER_RUNTIME");
        assert!(!vars[0].is_secret);
        assert_eq!(vars[1].name, "MyConn");
        assert!(vars[1].is_secret);
    }

    #[test]
    fn missing_properties_is_empty() {
        assert!(extract(&json!({})).is_empty());
    }

    #[test]
    fn build_properties_round_trips_every_key() {
        let settings = vec![
            EnvVar {
                name: "A".into(),
                value: "1".into(),
                ..Default::default()
            },
            EnvVar {
                name: "Conn".into(),
                value: "@Microsoft.KeyVault(SecretUri=https://v/secrets/c/)".into(),
                is_secret: true,
                ..Default::default()
            },
        ];
        let props = build_properties(&settings);
        // Full map is reproduced verbatim — the PUT replaces the whole collection,
        // so dropping any key here would delete it server-side.
        assert_eq!(props.len(), 2);
        assert_eq!(props["A"], json!("1"));
        assert_eq!(
            props["Conn"],
            json!("@Microsoft.KeyVault(SecretUri=https://v/secrets/c/)")
        );
    }

    fn ev(name: &str, value: &str) -> EnvVar {
        EnvVar {
            name: name.into(),
            value: value.into(),
            ..Default::default()
        }
    }

    #[test]
    fn properties_with_edit_preserves_server_side_state() {
        // `NEW_KEY` appeared (and `LOG_LEVEL` changed) server-side after the
        // UI's cache was taken; the single-edit upsert must preserve both —
        // only the edited key may differ from the live map.
        let current = vec![ev("LOG_LEVEL", "warn"), ev("NEW_KEY", "added-elsewhere")];
        let props = properties_with_edit(&current, "B", "2");
        assert_eq!(props.len(), 3);
        assert_eq!(props["LOG_LEVEL"], json!("warn"));
        assert_eq!(props["NEW_KEY"], json!("added-elsewhere"));
        assert_eq!(props["B"], json!("2"));
    }

    #[test]
    fn properties_with_edit_overwrites_the_edited_key() {
        let current = vec![ev("LOG_LEVEL", "info")];
        let props = properties_with_edit(&current, "LOG_LEVEL", "debug");
        assert_eq!(props.len(), 1);
        assert_eq!(props["LOG_LEVEL"], json!("debug"));
    }
}
