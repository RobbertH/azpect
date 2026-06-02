//! Resolve the Log Analytics workspace customer ID for a Container App by
//! walking up to its parent Container Apps Environment, which is where the
//! `appLogsConfiguration` actually lives.
//!
//! Two ARM round-trips: first fetch the Container App to read
//! `properties.managedEnvironmentId`, then fetch the env to read
//! `properties.appLogsConfiguration.logAnalyticsConfiguration.customerId`.
//! Container Apps don't expose this via per-resource diagnostic settings, so
//! this is the only reliable path.

#![allow(dead_code, unused_variables)]

use anyhow::{anyhow, Context};

use crate::azure::auth::AzureAuth;
use crate::azure::client::ArmClient;
use crate::error::AzpectError;

/// API version that exposes `appLogsConfiguration` on managed environments.
const API_VERSION: &str = "2024-03-01";

pub async fn resolve(auth: &AzureAuth, container_app_id: &str) -> anyhow::Result<String> {
    let client = ArmClient::new(auth.clone())?;

    let app = client
        .get(container_app_id, &[("api-version", API_VERSION)])
        .await
        .with_context(|| format!("fetching container app {container_app_id}"))?;
    let env_id = app
        .pointer("/properties/managedEnvironmentId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("container app response missing managedEnvironmentId"))?;

    let env = client
        .get(env_id, &[("api-version", API_VERSION)])
        .await
        .with_context(|| format!("fetching managed environment {env_id}"))?;

    extract_customer_id(&env)
}

/// Map a Log Analytics workspace `customerId` (the GUID exposed on the env's
/// `logAnalyticsConfiguration`) to its **ARM resource id** via Resource Graph.
///
/// Needed because portal log deep-links scope by ARM id, but Container Apps only
/// surface the workspace's `customerId`. The GUID is unique across the tenant,
/// so the query isn't subscription-scoped. Best-effort: `Ok(None)` when no
/// workspace with that customerId is visible to the caller (a failed lookup
/// shouldn't break log viewing, which uses the customerId directly).
pub async fn arm_id_from_customer_id(
    auth: &AzureAuth,
    customer_id: &str,
) -> anyhow::Result<Option<String>> {
    // customerId is an Azure-issued GUID; strip quotes defensively so it can't
    // break out of the KQL string literal.
    let cid = customer_id.to_lowercase().replace('\'', "");
    let kql = format!(
        "resources \
         | where type =~ 'microsoft.operationalinsights/workspaces' \
         | where tolower(tostring(properties.customerId)) == '{cid}' \
         | project id | limit 1"
    );
    let client = ArmClient::new(auth.clone())?;
    let resp = client
        .post(
            "/providers/Microsoft.ResourceGraph/resources?api-version=2022-10-01",
            &serde_json::json!({ "query": kql }),
        )
        .await
        .with_context(|| "resolving workspace ARM id from customerId")?;
    Ok(workspace_id_from_graph(&resp))
}

/// First `data[].id` from a Resource Graph response, if any.
fn workspace_id_from_graph(resp: &serde_json::Value) -> Option<String> {
    resp.get("data")?
        .as_array()?
        .first()?
        .get("id")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// Pull `customerId` out of an env response, surfacing the empty-destination
/// case as `NoLogDestination` so the UI shows the friendly hint instead of a
/// generic ARM error.
fn extract_customer_id(env: &serde_json::Value) -> anyhow::Result<String> {
    let destination = env
        .pointer("/properties/appLogsConfiguration/destination")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if !destination.eq_ignore_ascii_case("log-analytics") {
        return Err(anyhow!(AzpectError::NoLogDestination));
    }

    env.pointer("/properties/appLogsConfiguration/logAnalyticsConfiguration/customerId")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!(AzpectError::NoLogDestination))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn returns_customer_id_when_destination_is_log_analytics() {
        let env = json!({
            "properties": {
                "appLogsConfiguration": {
                    "destination": "log-analytics",
                    "logAnalyticsConfiguration": { "customerId": "abc-123" }
                }
            }
        });
        assert_eq!(extract_customer_id(&env).unwrap(), "abc-123");
    }

    #[test]
    fn destination_azure_monitor_is_no_destination() {
        // Env is forwarding, but not to a workspace we can KQL against.
        let env = json!({
            "properties": {
                "appLogsConfiguration": { "destination": "azure-monitor" }
            }
        });
        let err = extract_customer_id(&env).unwrap_err();
        assert!(err
            .downcast_ref::<AzpectError>()
            .map(|e| matches!(e, AzpectError::NoLogDestination))
            .unwrap_or(false));
    }

    #[test]
    fn missing_destination_is_no_destination() {
        let env = json!({ "properties": { "appLogsConfiguration": {} } });
        let err = extract_customer_id(&env).unwrap_err();
        assert!(err
            .downcast_ref::<AzpectError>()
            .map(|e| matches!(e, AzpectError::NoLogDestination))
            .unwrap_or(false));
    }

    #[test]
    fn workspace_id_from_graph_takes_first_row_id() {
        let resp = json!({
            "data": [
                { "id": "/subscriptions/s/resourceGroups/rg/providers/Microsoft.OperationalInsights/workspaces/law-1" }
            ]
        });
        assert_eq!(
            workspace_id_from_graph(&resp).as_deref(),
            Some("/subscriptions/s/resourceGroups/rg/providers/Microsoft.OperationalInsights/workspaces/law-1")
        );
    }

    #[test]
    fn workspace_id_from_graph_none_when_empty() {
        assert!(workspace_id_from_graph(&json!({ "data": [] })).is_none());
        assert!(workspace_id_from_graph(&json!({})).is_none());
    }

    #[test]
    fn empty_customer_id_is_no_destination() {
        let env = json!({
            "properties": {
                "appLogsConfiguration": {
                    "destination": "log-analytics",
                    "logAnalyticsConfiguration": { "customerId": "" }
                }
            }
        });
        let err = extract_customer_id(&env).unwrap_err();
        assert!(err
            .downcast_ref::<AzpectError>()
            .map(|e| matches!(e, AzpectError::NoLogDestination))
            .unwrap_or(false));
    }
}
