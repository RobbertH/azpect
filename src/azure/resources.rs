//! Resource Graph KQL query that enumerates Function Apps, APIM instances, and
//! Container Apps across the supplied subscriptions in one call.

#![allow(dead_code, unused_variables)]

use anyhow::anyhow;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::azure::auth::AzureAuth;
use crate::azure::client::ArmClient;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum ResourceKind {
    FunctionApp,
    Apim,
    ContainerApp,
    AppGateway,
}

impl ResourceKind {
    /// Four-letter tag for the list view: `Func`, `APIM`, `Cont`, `AppG`.
    pub fn short_tag(&self) -> &'static str {
        match self {
            ResourceKind::Apim => "APIM",
            ResourceKind::FunctionApp => "Func",
            ResourceKind::ContainerApp => "Cont",
            ResourceKind::AppGateway => "AppG",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Resource {
    /// Full Azure resource ID (`/subscriptions/.../resourceGroups/.../providers/.../<name>`).
    pub id: String,
    pub name: String,
    pub kind: ResourceKind,
    pub location: String,
    pub resource_group: String,
    pub subscription_id: String,
    /// Azure resource state (`Running`, `Stopped`, etc.) when the provider exposes it.
    pub state: Option<String>,
    /// `systemData.createdAt` from the ARM envelope. `None` for older resource
    /// types that pre-date systemData (created before ~2020) or where Resource
    /// Graph hasn't surfaced it.
    pub created_at: Option<DateTime<Utc>>,
    /// `systemData.lastModifiedAt` from the ARM envelope. Same caveats as
    /// `created_at` — missing for legacy resources.
    pub modified_at: Option<DateTime<Utc>>,
}

/// KQL query template. Lane 2 substitutes nothing — Resource Graph honors the
/// `subscriptions` field in the request body to scope, so this query is fixed.
///
/// `state` is coalesced per resource family: Function Apps and APIM expose
/// `properties.state` (`Running`/`Stopped`), Container Apps expose
/// `properties.runningStatus` (`Running`/`Progressing`/`Stopped`/`Suspended`),
/// and Application Gateways expose `properties.operationalState`
/// (`Running`/`Stopped`/`Starting`/`Stopping`). Without the case() the Detail
/// view shows "state: unknown" for those families.
pub const KQL: &str = r#"
Resources
| where (type == 'microsoft.web/sites' and kind contains 'functionapp')
    or type == 'microsoft.apimanagement/service'
    or type == 'microsoft.app/containerapps'
    or type == 'microsoft.network/applicationgateways'
| project id, name, type, kind, location, resourceGroup, subscriptionId,
          state = case(
              type == 'microsoft.app/containerapps', tostring(properties.runningStatus),
              type == 'microsoft.network/applicationgateways', tostring(properties.operationalState),
              tostring(properties.state)
          ),
          // systemData is the standard ARM envelope but older Resource Providers
          // (notably microsoft.web/sites and microsoft.apimanagement/service)
          // don't populate it. Coalesce per type to their RP-specific timestamp
          // fields so most rows actually have a date. modifiedAt has the same
          // story but only Function Apps expose `properties.lastModifiedTimeUtc`;
          // for APIM there's no equivalent so it falls back to systemData / null.
          createdAt = case(
              type == 'microsoft.web/sites', tostring(properties.createdTime),
              type == 'microsoft.apimanagement/service', tostring(properties.createdAtUtc),
              tostring(systemData.createdAt)
          ),
          modifiedAt = case(
              type == 'microsoft.web/sites', tostring(properties.lastModifiedTimeUtc),
              tostring(systemData.lastModifiedAt)
          )
| order by name asc
"#;

/// Query Resource Graph. Empty `subscription_ids` means "all subscriptions the
/// credential can see" — the caller should resolve that via [`super::subscriptions::list`] first.
pub async fn list(auth: &AzureAuth, subscription_ids: &[String]) -> anyhow::Result<Vec<Resource>> {
    let client = ArmClient::new(auth.clone())?;

    let body = if subscription_ids.is_empty() {
        serde_json::json!({ "query": KQL })
    } else {
        serde_json::json!({
            "subscriptions": subscription_ids,
            "query": KQL,
        })
    };

    let resp = client
        .post(
            "/providers/Microsoft.ResourceGraph/resources?api-version=2022-10-01",
            &body,
        )
        .await?;

    let rows = resp
        .get("data")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("resource graph response missing 'data' array"))?;

    if rows.len() >= 1000 {
        tracing::warn!(
            "resource graph returned {} rows; pagination not implemented in v1",
            rows.len()
        );
    }

    let resources: Vec<Resource> = rows.iter().filter_map(parse_resource).collect();
    Ok(resources)
}

fn parse_resource(v: &serde_json::Value) -> Option<Resource> {
    let ty = v.get("type")?.as_str()?.to_lowercase();
    let kind_str = v.get("kind").and_then(|k| k.as_str()).unwrap_or("");

    let kind = if ty == "microsoft.web/sites" && kind_str.to_lowercase().contains("functionapp") {
        ResourceKind::FunctionApp
    } else if ty == "microsoft.apimanagement/service" {
        ResourceKind::Apim
    } else if ty == "microsoft.app/containerapps" {
        ResourceKind::ContainerApp
    } else if ty == "microsoft.network/applicationgateways" {
        ResourceKind::AppGateway
    } else {
        return None;
    };

    let id = v.get("id")?.as_str()?.to_string();
    let name = v
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .to_string();
    let location = v
        .get("location")
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .to_string();
    let resource_group = v
        .get("resourceGroup")
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .to_string();
    let subscription_id = v
        .get("subscriptionId")
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .to_string();
    let state = v
        .get("state")
        .and_then(|n| n.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let created_at = parse_optional_rfc3339(v.get("createdAt"));
    let modified_at = parse_optional_rfc3339(v.get("modifiedAt"));

    Some(Resource {
        id,
        name,
        kind,
        location,
        resource_group,
        subscription_id,
        state,
        created_at,
        modified_at,
    })
}

/// Pull an RFC3339 timestamp out of a JSON field, tolerant of `null`, missing
/// and empty-string. Returns `None` if absent or unparseable — older resources
/// pre-date `systemData`, and Resource Graph surfaces those as empty strings.
fn parse_optional_rfc3339(v: Option<&serde_json::Value>) -> Option<DateTime<Utc>> {
    let s = v?.as_str()?;
    if s.is_empty() {
        return None;
    }
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_each_resource_kind() {
        let payload = json!({
            "data": [
                {
                    "id": "/subscriptions/sub1/resourceGroups/rg1/providers/Microsoft.Web/sites/myfunc",
                    "name": "myfunc",
                    "type": "microsoft.web/sites",
                    "kind": "functionapp,linux",
                    "location": "westeurope",
                    "resourceGroup": "rg1",
                    "subscriptionId": "sub1",
                    "state": "Running",
                    "createdAt": "2024-03-15T08:30:00Z",
                    "modifiedAt": "2024-05-01T12:00:00Z"
                },
                {
                    "id": "/subscriptions/sub1/resourceGroups/rg1/providers/Microsoft.ApiManagement/service/myapim",
                    "name": "myapim",
                    "type": "microsoft.apimanagement/service",
                    "kind": "",
                    "location": "westeurope",
                    "resourceGroup": "rg1",
                    "subscriptionId": "sub1",
                    "state": "",
                    "createdAt": "",
                    "modifiedAt": ""
                },
                {
                    "id": "/subscriptions/sub1/resourceGroups/rg1/providers/Microsoft.App/containerApps/myca",
                    "name": "myca",
                    "type": "microsoft.app/containerapps",
                    "kind": "",
                    "location": "westeurope",
                    "resourceGroup": "rg1",
                    "subscriptionId": "sub1",
                    "state": "Running"
                },
                {
                    "id": "/subscriptions/sub1/resourceGroups/rg1/providers/Microsoft.Web/sites/justawebapp",
                    "name": "justawebapp",
                    "type": "microsoft.web/sites",
                    "kind": "app,linux",
                    "location": "westeurope",
                    "resourceGroup": "rg1",
                    "subscriptionId": "sub1",
                    "state": "Running"
                },
                {
                    "id": "/subscriptions/sub1/resourceGroups/rg1/providers/Microsoft.Network/applicationGateways/myagw",
                    "name": "myagw",
                    "type": "microsoft.network/applicationgateways",
                    "kind": "",
                    "location": "westeurope",
                    "resourceGroup": "rg1",
                    "subscriptionId": "sub1",
                    "state": "Running"
                }
            ]
        });

        let resources: Vec<Resource> = payload["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(parse_resource)
            .collect();

        // Plain web app (kind=app,linux) must be skipped.
        assert_eq!(resources.len(), 4);
        assert_eq!(resources[0].kind, ResourceKind::FunctionApp);
        assert_eq!(resources[0].state.as_deref(), Some("Running"));
        // Function App row carried both systemData timestamps.
        assert!(resources[0].created_at.is_some());
        assert_eq!(
            resources[0].created_at.unwrap().to_rfc3339(),
            "2024-03-15T08:30:00+00:00"
        );
        assert!(resources[0].modified_at.is_some());
        assert_eq!(resources[1].kind, ResourceKind::Apim);
        assert_eq!(resources[1].state, None); // empty string filtered out
                                              // APIM row sent empty strings for the timestamps — both must collapse to None.
        assert!(resources[1].created_at.is_none());
        assert!(resources[1].modified_at.is_none());
        assert_eq!(resources[2].kind, ResourceKind::ContainerApp);
        // Container app row omitted createdAt entirely — should also be None.
        assert!(resources[2].created_at.is_none());
        assert_eq!(resources[3].kind, ResourceKind::AppGateway);
        assert_eq!(resources[3].state.as_deref(), Some("Running"));
    }

    #[test]
    fn skips_unknown_resource_types() {
        let payload = json!({
            "data": [
                {
                    "id": "/subscriptions/x/resourceGroups/y/providers/Microsoft.Storage/storageAccounts/z",
                    "name": "z",
                    "type": "microsoft.storage/storageaccounts",
                    "kind": "",
                    "location": "westeurope",
                    "resourceGroup": "y",
                    "subscriptionId": "x",
                    "state": ""
                }
            ]
        });

        let resources: Vec<Resource> = payload["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(parse_resource)
            .collect();
        assert!(resources.is_empty());
    }
}
