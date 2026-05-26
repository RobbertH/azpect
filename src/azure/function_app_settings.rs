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
}
