//! Resource Graph KQL query that enumerates Function Apps, Web Apps, APIM
//! instances, and Container Apps across the supplied subscriptions in one call.

#![allow(dead_code, unused_variables)]

use anyhow::anyhow;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::azure::auth::AzureAuth;
use crate::azure::client::ArmClient;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum ResourceKind {
    FunctionApp,
    /// A plain App Service web app — `microsoft.web/sites` whose `kind` does
    /// NOT contain `functionapp`. Shares the Function App ARM surface
    /// (app settings, `config/web`, site metrics) but has no functions list.
    WebApp,
    Apim,
    ContainerApp,
    AppGateway,
}

impl ResourceKind {
    /// Short tag for the list view: `FuncApp`, `WebApp`, `APIM`, `ContApp`, `AppGW`.
    pub fn short_tag(&self) -> &'static str {
        match self {
            ResourceKind::Apim => "APIM",
            ResourceKind::FunctionApp => "FuncApp",
            ResourceKind::WebApp => "WebApp",
            ResourceKind::ContainerApp => "ContApp",
            ResourceKind::AppGateway => "AppGW",
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
    /// Extended ARM-envelope bits (authorship, tags) plus kind-specific
    /// networking (APIM gateway/VIPs, Function App public-access posture)
    /// surfaced in the list + Detail overview. Bundled so the (many) test
    /// fixtures can opt out with `meta: Default::default()`. `#[serde(default)]`
    /// keeps older cached payloads deserializable.
    #[serde(default)]
    pub meta: ResourceMeta,
}

/// `systemData` authorship + resource tags + kind-specific networking. All
/// optional / empty for legacy resource providers (notably `microsoft.web/sites`
/// and APIM) that don't populate `systemData`; the renderer collapses absent
/// lines.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ResourceMeta {
    /// `systemData.createdBy` — a UPN/email for `User`, an object id for
    /// `Application`/`ManagedIdentity`.
    pub created_by: Option<String>,
    /// `systemData.createdByType` — `User` / `Application` / `ManagedIdentity` / `Key`.
    pub created_by_type: Option<String>,
    pub modified_by: Option<String>,
    pub modified_by_type: Option<String>,
    /// Resource tags as `(key, value)` pairs, sorted by key.
    pub tags: Vec<(String, String)>,
    /// APIM only: `properties.gatewayUrl` — the service's gateway endpoint
    /// (e.g. `https://myapim.azure-api.net`). `None` for other kinds.
    #[serde(default)]
    pub gateway_url: Option<String>,
    /// APIM only: `properties.publicIPAddresses` — the public virtual IP(s) the
    /// gateway answers on. Empty for non-APIM resources.
    #[serde(default)]
    pub public_ips: Vec<String>,
    /// APIM only: `properties.privateIPAddresses` — the private VIP(s), present
    /// only for internal-VNet-mode services. Empty otherwise.
    #[serde(default)]
    pub private_ips: Vec<String>,
    /// `properties.publicNetworkAccess` (`Enabled` / `Disabled`). Populated for
    /// Function Apps (and APIM); `None` when the provider doesn't expose it or
    /// the value is unset. See [`ResourceMeta::public_network_enabled`].
    #[serde(default)]
    pub public_network_access: Option<String>,
}

impl ResourceMeta {
    /// Whether the resource is reachable over the public network. The ARM field
    /// `properties.publicNetworkAccess` is `Disabled` to lock a resource down;
    /// any other value — including unset and `Enabled` — leaves it publicly
    /// reachable, which is Azure's default for `microsoft.web/sites`.
    pub fn public_network_enabled(&self) -> bool {
        !matches!(
            self.public_network_access.as_deref(),
            Some(a) if a.eq_ignore_ascii_case("Disabled")
        )
    }

    /// ARM id of the App Insights component linked to this app, from the
    /// `hidden-link: /app-insights-resource-id` tag the portal / Functions
    /// tooling stamps on the site. This is the right *scope* for querying the
    /// app's AI telemetry: workspace-based App Insights stamps `_ResourceId`
    /// on its rows with the component, not the site, so a resource-centric
    /// query on the site resolves none of the `App*` tables even when
    /// telemetry exists. See [`crate::azure::executions`].
    pub fn app_insights_resource_id(&self) -> Option<&str> {
        self.tags
            .iter()
            .find(|(k, _)| k.starts_with("hidden-link") && k.contains("app-insights-resource-id"))
            .map(|(_, v)| v.trim())
            .filter(|v| !v.is_empty())
    }
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
| where type == 'microsoft.web/sites'
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
          ),
          // systemData authorship + tags for the Detail overview. Empty for RPs
          // that don't populate systemData (web/sites, apimanagement); the
          // renderer just omits those lines.
          createdBy = tostring(systemData.createdBy),
          createdByType = tostring(systemData.createdByType),
          modifiedBy = tostring(systemData.lastModifiedBy),
          modifiedByType = tostring(systemData.lastModifiedByType),
          tags = tags,
          // Kind-specific networking surfaced in the list's NETWORK column and
          // the Detail overview. gatewayUrl / hostnameConfigurations / public+
          // private IPs only exist on APIM (`microsoft.apimanagement/service`);
          // publicNetworkAccess is the Function App (and APIM) public-access
          // toggle. Absent fields project as null/empty and the renderer just
          // omits them. The effective gateway URL prefers the custom `Proxy`
          // hostname (what the portal Overview shows, e.g. `https://api.acme.io`)
          // over the default `gatewayUrl` (`https://<name>.azure-api.net`); both
          // are carried so the parser can pick — see `effective_gateway_url`.
          gatewayUrl = tostring(properties.gatewayUrl),
          hostnameConfigurations = properties.hostnameConfigurations,
          publicIPs = properties.publicIPAddresses,
          privateIPs = properties.privateIPAddresses,
          publicNetworkAccess = tostring(properties.publicNetworkAccess)
| order by name asc
"#;

/// Upper bound on `$skipToken` continuation pages. Resource Graph returns at
/// most 1000 rows per page, so this caps the list at ~5000 resources — plenty
/// for a TUI list while keeping a runaway tenant from stalling startup.
const MAX_PAGES: usize = 5;

/// Query Resource Graph. Empty `subscription_ids` means "all subscriptions the
/// credential can see" — the caller should resolve that via [`super::subscriptions::list`] first.
///
/// Follows `$skipToken` continuations (Resource Graph pages at 1000 rows) up
/// to [`MAX_PAGES`] pages, so tenants past the single-page limit aren't
/// silently truncated.
pub async fn list(auth: &AzureAuth, subscription_ids: &[String]) -> anyhow::Result<Vec<Resource>> {
    let client = ArmClient::new(auth.clone())?;

    let mut resources: Vec<Resource> = Vec::new();
    let mut skip_token: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let mut body = if subscription_ids.is_empty() {
            serde_json::json!({ "query": KQL })
        } else {
            serde_json::json!({
                "subscriptions": subscription_ids,
                "query": KQL,
            })
        };
        if let Some(token) = &skip_token {
            body["options"] = serde_json::json!({ "$skipToken": token });
        }

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
        resources.extend(rows.iter().filter_map(parse_resource));

        skip_token = resp
            .get("$skipToken")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        if skip_token.is_none() {
            break;
        }
    }
    if skip_token.is_some() {
        tracing::warn!(
            "resource graph listing hit the {MAX_PAGES}-page cap ({} rows); list is truncated",
            resources.len()
        );
    }

    Ok(resources)
}

fn parse_resource(v: &serde_json::Value) -> Option<Resource> {
    let ty = v.get("type")?.as_str()?.to_lowercase();
    let kind_str = v.get("kind").and_then(|k| k.as_str()).unwrap_or("");

    let kind = if ty == "microsoft.web/sites" {
        // `kind` distinguishes the site flavors sharing this ARM type:
        // `functionapp[,linux]` (and Logic Apps Standard's `functionapp,workflowapp`)
        // vs plain web apps (`app`, `app,linux`, `api`, …).
        if kind_str.to_lowercase().contains("functionapp") {
            ResourceKind::FunctionApp
        } else {
            ResourceKind::WebApp
        }
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

    let meta = ResourceMeta {
        created_by: non_empty_string(v.get("createdBy")),
        created_by_type: non_empty_string(v.get("createdByType")),
        modified_by: non_empty_string(v.get("modifiedBy")),
        modified_by_type: non_empty_string(v.get("modifiedByType")),
        tags: parse_tags(v.get("tags")),
        gateway_url: effective_gateway_url(v),
        public_ips: parse_string_array(v.get("publicIPs")),
        private_ips: parse_string_array(v.get("privateIPs")),
        public_network_access: non_empty_string(v.get("publicNetworkAccess")),
    };

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
        meta,
    })
}

/// Read a JSON field as a non-empty string. Resource Graph surfaces absent
/// `systemData` fields as empty strings (via `tostring(...)`), so collapse those
/// to `None`.
fn non_empty_string(v: Option<&serde_json::Value>) -> Option<String> {
    v.and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// The APIM gateway URL as the portal Overview shows it: the custom gateway
/// (`Proxy`) hostname when one is configured (e.g. `https://api.acme.io`),
/// otherwise the default `properties.gatewayUrl` (`https://<name>.azure-api.net`).
/// `None` for non-APIM resources (both projected fields are null there).
fn effective_gateway_url(v: &serde_json::Value) -> Option<String> {
    if let Some(host) = proxy_hostname(v.get("hostnameConfigurations")) {
        return Some(format!("https://{host}"));
    }
    non_empty_string(v.get("gatewayUrl"))
}

/// Pick the gateway (`Proxy`) custom hostname out of APIM's
/// `hostnameConfigurations` array, preferring the entry marked as the default
/// SSL binding when several `Proxy` hostnames exist. Returns `None` when there's
/// no custom gateway domain (only the default `*.azure-api.net` applies) — note
/// `DeveloperPortal` / `Management` / `Scm` / `Portal` entries are deliberately
/// ignored so a custom developer-portal domain never masquerades as the gateway.
fn proxy_hostname(v: Option<&serde_json::Value>) -> Option<String> {
    let arr = v?.as_array()?;
    let is_proxy = |h: &&serde_json::Value| {
        h.get("type")
            .and_then(|t| t.as_str())
            .is_some_and(|t| t.eq_ignore_ascii_case("Proxy"))
    };
    let host_of = |h: &serde_json::Value| {
        h.get("hostName")
            .and_then(|n| n.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    // Prefer the default-SSL-binding Proxy host; else the first Proxy host.
    arr.iter()
        .filter(is_proxy)
        .find(|h| {
            h.get("defaultSslBinding")
                .and_then(|b| b.as_bool())
                .unwrap_or(false)
        })
        .and_then(host_of)
        .or_else(|| arr.iter().filter(is_proxy).find_map(host_of))
}

/// Read a JSON field as a vec of strings, tolerant of `null` / missing / a
/// non-array value. Resource Graph projects an absent dynamic column (e.g.
/// `properties.publicIPAddresses` on a non-APIM resource) as `null`; non-string
/// array entries are skipped. Yields an empty vec in all the absent cases.
fn parse_string_array(v: Option<&serde_json::Value>) -> Vec<String> {
    v.and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Flatten a `tags` object into `(key, value)` pairs sorted by key. Non-string
/// tag values are coerced via their JSON display; a missing/non-object `tags`
/// field yields an empty vec.
fn parse_tags(v: Option<&serde_json::Value>) -> Vec<(String, String)> {
    let Some(map) = v.and_then(|v| v.as_object()) else {
        return Vec::new();
    };
    let mut out: Vec<(String, String)> = map
        .iter()
        .map(|(k, val)| {
            let s = match val {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            (k.clone(), s)
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
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

        // Plain web app (kind=app,linux) parses as WebApp.
        assert_eq!(resources.len(), 5);
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
        assert_eq!(resources[3].kind, ResourceKind::WebApp);
        assert_eq!(resources[3].name, "justawebapp");
        assert_eq!(resources[3].state.as_deref(), Some("Running"));
        assert_eq!(resources[4].kind, ResourceKind::AppGateway);
        assert_eq!(resources[4].state.as_deref(), Some("Running"));
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

    #[test]
    fn parses_systemdata_authorship_and_tags() {
        let payload = json!({
            "data": [{
                "id": "/subscriptions/sub1/resourceGroups/rg1/providers/Microsoft.App/containerApps/myca",
                "name": "myca",
                "type": "microsoft.app/containerapps",
                "kind": "",
                "location": "westeurope",
                "resourceGroup": "rg1",
                "subscriptionId": "sub1",
                "state": "Running",
                "createdBy": "00000000-0000-0000-0000-000000000001",
                "createdByType": "Application",
                "modifiedBy": "someone@example.com",
                "modifiedByType": "User",
                "tags": { "Terraform": "true", "Domain": "tool" }
            }]
        });
        let r = payload["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(parse_resource)
            .next()
            .unwrap();
        assert_eq!(
            r.meta.created_by.as_deref(),
            Some("00000000-0000-0000-0000-000000000001")
        );
        assert_eq!(r.meta.created_by_type.as_deref(), Some("Application"));
        assert_eq!(r.meta.modified_by.as_deref(), Some("someone@example.com"));
        assert_eq!(r.meta.modified_by_type.as_deref(), Some("User"));
        // Tags sorted by key.
        assert_eq!(
            r.meta.tags,
            vec![
                ("Domain".to_string(), "tool".to_string()),
                ("Terraform".to_string(), "true".to_string()),
            ]
        );
    }

    #[test]
    fn app_insights_resource_id_reads_the_hidden_link_tag() {
        let mut meta = ResourceMeta::default();
        assert_eq!(meta.app_insights_resource_id(), None);

        meta.tags = vec![
            ("Domain".to_string(), "tool".to_string()),
            (
                "hidden-link: /app-insights-resource-id".to_string(),
                "/subscriptions/s/resourceGroups/rg/providers/microsoft.insights/components/my-ai"
                    .to_string(),
            ),
        ];
        assert_eq!(
            meta.app_insights_resource_id(),
            Some(
                "/subscriptions/s/resourceGroups/rg/providers/microsoft.insights/components/my-ai"
            )
        );

        // An empty value (tag present but blank) must not become the scope.
        meta.tags = vec![(
            "hidden-link: /app-insights-resource-id".to_string(),
            " ".to_string(),
        )];
        assert_eq!(meta.app_insights_resource_id(), None);
    }

    #[test]
    fn parses_apim_gateway_and_virtual_ips() {
        let payload = json!({
            "data": [{
                "id": "/subscriptions/sub1/resourceGroups/rg1/providers/Microsoft.ApiManagement/service/myapim",
                "name": "myapim",
                "type": "microsoft.apimanagement/service",
                "kind": "",
                "location": "westeurope",
                "resourceGroup": "rg1",
                "subscriptionId": "sub1",
                "state": "",
                "gatewayUrl": "https://myapim.azure-api.net",
                "publicIPs": ["20.1.2.3"],
                "privateIPs": ["10.0.0.4", "10.0.0.5"],
                "publicNetworkAccess": "Enabled"
            }]
        });
        let r = payload["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(parse_resource)
            .next()
            .unwrap();
        assert_eq!(r.kind, ResourceKind::Apim);
        assert_eq!(
            r.meta.gateway_url.as_deref(),
            Some("https://myapim.azure-api.net")
        );
        assert_eq!(r.meta.public_ips, vec!["20.1.2.3".to_string()]);
        assert_eq!(
            r.meta.private_ips,
            vec!["10.0.0.4".to_string(), "10.0.0.5".to_string()]
        );
        assert!(r.meta.public_network_enabled());
    }

    #[test]
    fn apim_gateway_prefers_custom_proxy_hostname() {
        // Portal "Gateway URL" reflects the custom Proxy domain when set, not the
        // default *.azure-api.net. A DeveloperPortal custom domain must NOT be
        // mistaken for the gateway.
        let payload = json!({
            "data": [{
                "id": "/subscriptions/sub1/resourceGroups/rg1/providers/Microsoft.ApiManagement/service/apim",
                "name": "apim",
                "type": "microsoft.apimanagement/service",
                "kind": "",
                "location": "westeurope",
                "resourceGroup": "rg1",
                "subscriptionId": "sub1",
                "state": "",
                "gatewayUrl": "https://apim.azure-api.net",
                "hostnameConfigurations": [
                    { "type": "DeveloperPortal", "hostName": "portal.acme.io" },
                    { "type": "Proxy", "hostName": "api.acme.io", "defaultSslBinding": true }
                ]
            }]
        });
        let r = payload["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(parse_resource)
            .next()
            .unwrap();
        assert_eq!(r.meta.gateway_url.as_deref(), Some("https://api.acme.io"));
    }

    #[test]
    fn apim_gateway_picks_default_ssl_binding_among_multiple_proxies() {
        let v = json!({
            "hostnameConfigurations": [
                { "type": "Proxy", "hostName": "legacy.acme.io", "defaultSslBinding": false },
                { "type": "Proxy", "hostName": "api.acme.io", "defaultSslBinding": true }
            ],
            "gatewayUrl": "https://apim.azure-api.net"
        });
        assert_eq!(
            effective_gateway_url(&v).as_deref(),
            Some("https://api.acme.io")
        );
    }

    #[test]
    fn apim_gateway_falls_back_to_default_when_no_custom_proxy() {
        // Only a custom developer-portal domain → the gateway stays the default.
        let v = json!({
            "hostnameConfigurations": [
                { "type": "DeveloperPortal", "hostName": "portal.acme.io" }
            ],
            "gatewayUrl": "https://apim.azure-api.net"
        });
        assert_eq!(
            effective_gateway_url(&v).as_deref(),
            Some("https://apim.azure-api.net")
        );
        // No hostname configs at all → default gateway.
        let v = json!({ "gatewayUrl": "https://apim.azure-api.net" });
        assert_eq!(
            effective_gateway_url(&v).as_deref(),
            Some("https://apim.azure-api.net")
        );
    }

    #[test]
    fn function_app_public_network_access_disabled_reads_as_private() {
        let payload = json!({
            "data": [{
                "id": "/subscriptions/sub1/resourceGroups/rg1/providers/Microsoft.Web/sites/locked",
                "name": "locked",
                "type": "microsoft.web/sites",
                "kind": "functionapp,linux",
                "location": "westeurope",
                "resourceGroup": "rg1",
                "subscriptionId": "sub1",
                "state": "Running",
                "publicNetworkAccess": "Disabled"
            }]
        });
        let r = payload["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(parse_resource)
            .next()
            .unwrap();
        assert_eq!(r.kind, ResourceKind::FunctionApp);
        assert_eq!(r.meta.public_network_access.as_deref(), Some("Disabled"));
        assert!(!r.meta.public_network_enabled());
        // Non-APIM rows carry no gateway / VIPs.
        assert!(r.meta.gateway_url.is_none());
        assert!(r.meta.public_ips.is_empty());
        assert!(r.meta.private_ips.is_empty());
    }

    #[test]
    fn public_network_unset_defaults_to_enabled() {
        // Azure leaves publicNetworkAccess unset on apps that have never toggled
        // it; the default posture is "Enabled" (publicly reachable).
        let meta = ResourceMeta::default();
        assert!(meta.public_network_enabled());
        let meta = ResourceMeta {
            public_network_access: Some("Enabled".into()),
            ..Default::default()
        };
        assert!(meta.public_network_enabled());
    }

    #[test]
    fn empty_systemdata_strings_collapse_to_none() {
        let payload = json!({
            "data": [{
                "id": "/subscriptions/sub1/resourceGroups/rg1/providers/Microsoft.Web/sites/f",
                "name": "f",
                "type": "microsoft.web/sites",
                "kind": "functionapp",
                "location": "westeurope",
                "resourceGroup": "rg1",
                "subscriptionId": "sub1",
                "state": "Running",
                "createdBy": "",
                "modifiedBy": ""
            }]
        });
        let r = payload["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(parse_resource)
            .next()
            .unwrap();
        assert!(r.meta.created_by.is_none());
        assert!(r.meta.modified_by.is_none());
        assert!(r.meta.tags.is_empty());
    }
}
