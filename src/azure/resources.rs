//! Resource Graph KQL query that enumerates Function Apps, APIM instances, and
//! Container Apps across the supplied subscriptions in one call.

#![allow(dead_code, unused_variables)]

use serde::{Deserialize, Serialize};

use crate::azure::auth::AzureAuth;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum ResourceKind {
    FunctionApp,
    Apim,
    ContainerApp,
}

impl ResourceKind {
    /// Three-letter tag for the list view: `Func`, `APIM`, `CtrA`.
    pub fn short_tag(&self) -> &'static str {
        match self {
            ResourceKind::FunctionApp => "Func",
            ResourceKind::Apim => "APIM",
            ResourceKind::ContainerApp => "CtrA",
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
}

/// KQL query template. Lane 2 substitutes nothing — Resource Graph honors the
/// `subscriptions` field in the request body to scope, so this query is fixed.
pub const KQL: &str = r#"
Resources
| where (type == 'microsoft.web/sites' and kind contains 'functionapp')
    or type == 'microsoft.apimanagement/service'
    or type == 'microsoft.app/containerapps'
| project id, name, type, kind, location, resourceGroup, subscriptionId,
          state = tostring(properties.state)
| order by name asc
"#;

/// Query Resource Graph. Empty `subscription_ids` means "all subscriptions the
/// credential can see" — the caller should resolve that via [`super::subscriptions::list`] first.
pub async fn list(auth: &AzureAuth, subscription_ids: &[String]) -> anyhow::Result<Vec<Resource>> {
    todo!("Lane 2: POST /providers/Microsoft.ResourceGraph/resources?api-version=2022-10-01 with body {{subscriptions, query: KQL}}; map type+kind into ResourceKind")
}
