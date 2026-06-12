//! APIM ARM endpoints: list APIs on a service, list operations on an API,
//! fetch the policy XML for a single operation. Used by the APIM drill-in
//! views (apim_apis -> apim_operations -> apim_policy).
//!
//! All three calls share an `api-version` (`2024-05-01`); pinned here so the
//! UI never has to think about it.

#![allow(dead_code)]

use anyhow::{anyhow, Context};
use serde::{Deserialize, Serialize};

use crate::azure::auth::AzureAuth;
use crate::azure::client::ArmClient;

const API_VERSION: &str = "2024-05-01";

/// A single API hosted by an APIM service. `path` is the gateway prefix
/// (e.g. `echo`) that all of this API's operations sit under.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Api {
    /// Full ARM resource id (`{serviceId}/apis/{apiName}`).
    pub id: String,
    /// Stable APIM name (slug). Used for lookups; `display_name` is for UI.
    pub name: String,
    pub display_name: String,
    pub path: String,
    /// Backend the API forwards to (`properties.serviceUrl`). `None` when the
    /// API has no static backend set — common when routing is done in policy
    /// via `set-backend-service`.
    pub service_url: Option<String>,
}

/// A single operation (route) on an API.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Operation {
    /// Full ARM resource id (`{apiId}/operations/{operationName}`).
    pub id: String,
    pub name: String,
    pub display_name: String,
    /// HTTP method (`GET`, `POST`, …). Empty when the upstream doesn't set one.
    pub method: String,
    /// URL template relative to the API path (`/users/{userId}`).
    pub url_template: String,
}

/// `GET {service_id}/apis?api-version=…`. Returns APIs in the order APIM hands
/// them back, then sorts by display name so the list view is deterministic.
pub async fn list_apis(auth: &AzureAuth, service_id: &str) -> anyhow::Result<Vec<Api>> {
    let client = ArmClient::new(auth.clone())?;
    let path = format!("{service_id}/apis");
    let resp = client
        .get(&path, &[("api-version", API_VERSION)])
        .await
        .with_context(|| format!("list APIs on {service_id}"))?;
    let mut apis = parse_apis(&resp)?;
    apis.sort_by(|a, b| a.display_name.cmp(&b.display_name));
    Ok(apis)
}

/// `GET {api_id}/operations?api-version=…`. Sorted by (path, method) so the
/// route list reads in a stable order regardless of upstream pagination.
pub async fn list_operations(auth: &AzureAuth, api_id: &str) -> anyhow::Result<Vec<Operation>> {
    let client = ArmClient::new(auth.clone())?;
    let path = format!("{api_id}/operations");
    let resp = client
        .get(&path, &[("api-version", API_VERSION)])
        .await
        .with_context(|| format!("list operations on {api_id}"))?;
    let mut ops = parse_operations(&resp)?;
    ops.sort_by(|a, b| {
        a.url_template
            .cmp(&b.url_template)
            .then_with(|| a.method.cmp(&b.method))
    });
    Ok(ops)
}

/// Fetched policy text for one operation, or `None` if APIM reports no policy
/// is set (404 on the `policies/policy` child). Anything else is a hard error.
pub async fn fetch_operation_policy(
    auth: &AzureAuth,
    operation_id: &str,
) -> anyhow::Result<Option<String>> {
    let client = ArmClient::new(auth.clone())?;
    let path = format!("{operation_id}/policies/policy");
    // `rawxml` is the human-readable form; the default (`xml`) escapes
    // entities and is unpleasant to read in a terminal.
    let result = client
        .get(&path, &[("api-version", API_VERSION), ("format", "rawxml")])
        .await;
    match result {
        Ok(value) => {
            let content = value
                .pointer("/properties/value")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            Ok(content)
        }
        Err(e) => {
            // The send_with_retry error includes the status code in its string
            // form ("azure api error 404: …"). 404 here means "no policy
            // configured" — surface as `Ok(None)` so the UI can show a friendly
            // placeholder instead of a red banner.
            let msg = format!("{e:#}");
            if msg.contains("azure api error 404") {
                Ok(None)
            } else {
                Err(e).with_context(|| format!("fetch operation policy {operation_id}"))
            }
        }
    }
}

fn parse_apis(value: &serde_json::Value) -> anyhow::Result<Vec<Api>> {
    let arr = value
        .get("value")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("APIM apis response missing 'value' array"))?;
    Ok(arr.iter().filter_map(parse_one_api).collect())
}

fn parse_one_api(v: &serde_json::Value) -> Option<Api> {
    let id = v.get("id")?.as_str()?.to_string();
    let name = v
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .to_string();
    let props = v.get("properties");
    let display_name = props
        .and_then(|p| p.get("displayName"))
        .and_then(|d| d.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(&name)
        .to_string();
    let path = props
        .and_then(|p| p.get("path"))
        .and_then(|d| d.as_str())
        .unwrap_or("")
        .to_string();
    let service_url = props
        .and_then(|p| p.get("serviceUrl"))
        .and_then(|d| d.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    Some(Api {
        id,
        name,
        display_name,
        path,
        service_url,
    })
}

fn parse_operations(value: &serde_json::Value) -> anyhow::Result<Vec<Operation>> {
    let arr = value
        .get("value")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("APIM operations response missing 'value' array"))?;
    Ok(arr.iter().filter_map(parse_one_operation).collect())
}

fn parse_one_operation(v: &serde_json::Value) -> Option<Operation> {
    let id = v.get("id")?.as_str()?.to_string();
    let name = v
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .to_string();
    let props = v.get("properties");
    let display_name = props
        .and_then(|p| p.get("displayName"))
        .and_then(|d| d.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(&name)
        .to_string();
    let method = props
        .and_then(|p| p.get("method"))
        .and_then(|d| d.as_str())
        .unwrap_or("")
        .to_string();
    let url_template = props
        .and_then(|p| p.get("urlTemplate"))
        .and_then(|d| d.as_str())
        .unwrap_or("")
        .to_string();
    Some(Operation {
        id,
        name,
        display_name,
        method,
        url_template,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_api_list() {
        let payload = json!({
            "value": [
                {
                    "id": "/subscriptions/s/resourceGroups/rg/providers/Microsoft.ApiManagement/service/svc/apis/echo-api",
                    "name": "echo-api",
                    "properties": {
                        "displayName": "Echo API",
                        "path": "echo",
                        "serviceUrl": "https://echo.internal.example.com",
                        "protocols": ["https"]
                    }
                }
            ]
        });
        let apis = parse_apis(&payload).unwrap();
        assert_eq!(apis.len(), 1);
        assert_eq!(apis[0].name, "echo-api");
        assert_eq!(apis[0].display_name, "Echo API");
        assert_eq!(apis[0].path, "echo");
        assert_eq!(
            apis[0].service_url.as_deref(),
            Some("https://echo.internal.example.com")
        );
    }

    #[test]
    fn parse_api_falls_back_to_name_when_display_missing() {
        let payload = json!({
            "value": [
                {
                    "id": "/svc/apis/raw",
                    "name": "raw",
                    "properties": { "path": "p" }
                }
            ]
        });
        let apis = parse_apis(&payload).unwrap();
        assert_eq!(apis[0].display_name, "raw");
    }

    #[test]
    fn parses_operation_list() {
        let payload = json!({
            "value": [
                {
                    "id": "/svc/apis/echo-api/operations/get",
                    "name": "get",
                    "properties": {
                        "displayName": "Retrieve",
                        "method": "GET",
                        "urlTemplate": "/resource"
                    }
                },
                {
                    "id": "/svc/apis/echo-api/operations/post",
                    "name": "post",
                    "properties": {
                        "displayName": "Create",
                        "method": "POST",
                        "urlTemplate": "/resource"
                    }
                }
            ]
        });
        let ops = parse_operations(&payload).unwrap();
        assert_eq!(ops.len(), 2);
        assert_eq!(ops[0].display_name, "Retrieve");
        assert_eq!(ops[0].method, "GET");
        assert_eq!(ops[0].url_template, "/resource");
    }

    #[test]
    fn missing_value_array_is_an_error() {
        let payload = json!({ "items": [] });
        assert!(parse_apis(&payload).is_err());
        assert!(parse_operations(&payload).is_err());
    }
}
