//! Fetch a Function App's deployed container image from its site config.
//!
//! Container-deployed Function Apps store the image in
//! `properties.linuxFxVersion` as `DOCKER|<registry>/<image>:<tag>`. Unlike the
//! `config/appsettings/list` *action* (which returns secrets and 403s for
//! read-only principals — see [`super::function_app_settings`]), a plain GET of
//! `config/web` carries no secrets and is readable with `Reader`. So this is the
//! cheap, low-privilege source for the deployed image shown in the list's
//! VERSION column.
//!
//! Code-deployed Function Apps (e.g. `linuxFxVersion = DOTNET|8.0`, or empty on
//! Windows) have no container image; those yield `None` and the column stays
//! blank.

#![allow(dead_code, unused_variables)]

use anyhow::Context;

use crate::azure::auth::AzureAuth;
use crate::azure::client::ArmClient;

/// Microsoft.Web API version exposing the `config/web` resource.
const API_VERSION: &str = "2023-12-01";

/// The two facts azpect reads off a Function App's `config/web`: its deployed
/// container image (if any) and whether public access is gated by IP / VNet
/// access restrictions. Both come from the one GET, so the list's VERSION column
/// and the Detail view's `network:` row share a single fetch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WebConfig {
    /// Container image (`registry/image:tag`) for container-deployed apps;
    /// `None` for code-deployed apps. See [`extract`].
    pub image: Option<String>,
    /// `true` when the app, *while publicly reachable*, restricts which
    /// IPs / VNets may reach it (the portal's "Enabled from select virtual
    /// networks and IP addresses"); `false` when wide open ("Enabled with no
    /// access restrictions"). Meaningless when `publicNetworkAccess` is
    /// Disabled — the caller checks that first. See [`extract_access_restricted`].
    pub access_restricted: bool,
}

/// GET the Function App's web config and pull out its image + access posture.
pub async fn fetch(auth: &AzureAuth, function_app_id: &str) -> anyhow::Result<WebConfig> {
    let client = ArmClient::new(auth.clone())?;
    let path = format!("{function_app_id}/config/web");
    let resp = client
        .get(&path, &[("api-version", API_VERSION)])
        .await
        .with_context(|| format!("fetching web config for {function_app_id}"))?;
    Ok(WebConfig {
        image: extract(&resp),
        access_restricted: extract_access_restricted(&resp),
    })
}

/// Extract the container image reference from a `config/web` response.
///
/// Returns the image (everything after the `DOCKER|` prefix, e.g.
/// `myacr.azurecr.io/files-api:abc123`) for container-deployed apps, or `None`
/// when `linuxFxVersion` is absent, empty, or names a runtime stack rather than
/// a Docker image.
pub fn extract(value: &serde_json::Value) -> Option<String> {
    let fx = value
        .pointer("/properties/linuxFxVersion")
        .and_then(|v| v.as_str())?
        .trim();
    // `DOCKER|<image>` — the prefix is case-insensitive in practice.
    let (prefix, image) = fx.split_once('|')?;
    if !prefix.eq_ignore_ascii_case("docker") {
        return None;
    }
    let image = image.trim();
    if image.is_empty() {
        None
    } else {
        Some(image.to_string())
    }
}

/// Decide whether a `config/web` response describes IP / VNet access
/// restrictions on the main site. `true` ⇒ the portal would show "Enabled from
/// select virtual networks and IP addresses"; `false` ⇒ "Enabled with no access
/// restrictions".
///
/// An app with no restrictions either has an empty `ipSecurityRestrictions`
/// array or just the implicit catch-all (`Allow` from `Any`). It's restricted
/// when the default action is `Deny`, or any rule denies, scopes to a VNet
/// subnet, or allows a *specific* address rather than `Any`.
pub fn extract_access_restricted(value: &serde_json::Value) -> bool {
    // A Deny default action locks the app down even with no explicit rules.
    let default_deny = value
        .pointer("/properties/ipSecurityRestrictionsDefaultAction")
        .and_then(|v| v.as_str())
        .map(|a| a.eq_ignore_ascii_case("Deny"))
        .unwrap_or(false);
    if default_deny {
        return true;
    }
    value
        .pointer("/properties/ipSecurityRestrictions")
        .and_then(|v| v.as_array())
        .map(|rules| rules.iter().any(is_real_restriction))
        .unwrap_or(false)
}

/// A single `ipSecurityRestrictions` entry that actually narrows access (as
/// opposed to the implicit "Allow all from Any" default).
fn is_real_restriction(rule: &serde_json::Value) -> bool {
    let action = rule
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("Allow");
    if action.eq_ignore_ascii_case("Deny") {
        return true;
    }
    if rule
        .get("vnetSubnetResourceId")
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.trim().is_empty())
    {
        return true;
    }
    // An allow rule scoped to a specific address (anything but the `Any`
    // catch-all) means others are implicitly denied.
    let ip = rule
        .get("ipAddress")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    !ip.is_empty() && !ip.eq_ignore_ascii_case("Any")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_docker_image() {
        let v = json!({
            "properties": { "linuxFxVersion": "DOCKER|myacr.azurecr.io/files-api:abc123" }
        });
        assert_eq!(
            extract(&v).as_deref(),
            Some("myacr.azurecr.io/files-api:abc123")
        );
    }

    #[test]
    fn docker_prefix_is_case_insensitive() {
        let v = json!({ "properties": { "linuxFxVersion": "Docker|acr/f:v1" } });
        assert_eq!(extract(&v).as_deref(), Some("acr/f:v1"));
    }

    #[test]
    fn runtime_stack_is_not_an_image() {
        let v = json!({ "properties": { "linuxFxVersion": "DOTNET|8.0" } });
        assert!(extract(&v).is_none());
    }

    #[test]
    fn empty_or_absent_yields_none() {
        assert!(extract(&json!({ "properties": { "linuxFxVersion": "" } })).is_none());
        assert!(extract(&json!({ "properties": { "linuxFxVersion": "DOCKER|" } })).is_none());
        assert!(extract(&json!({})).is_none());
    }

    #[test]
    fn no_restrictions_when_only_the_allow_all_default() {
        // The implicit catch-all the portal shows as "no access restrictions".
        let v = json!({
            "properties": {
                "ipSecurityRestrictions": [
                    { "ipAddress": "Any", "action": "Allow", "priority": 2147483647, "name": "Allow all" }
                ]
            }
        });
        assert!(!extract_access_restricted(&v));
        // Empty / absent also count as unrestricted.
        assert!(!extract_access_restricted(
            &json!({ "properties": { "ipSecurityRestrictions": [] } })
        ));
        assert!(!extract_access_restricted(&json!({})));
    }

    #[test]
    fn restricted_by_specific_ip_allow_rule() {
        let v = json!({
            "properties": {
                "ipSecurityRestrictions": [
                    { "ipAddress": "203.0.113.0/24", "action": "Allow", "priority": 100, "name": "office" },
                    { "ipAddress": "Any", "action": "Deny", "priority": 2147483647, "name": "Deny all" }
                ]
            }
        });
        assert!(extract_access_restricted(&v));
    }

    #[test]
    fn restricted_by_vnet_rule_or_default_deny() {
        let vnet = json!({
            "properties": {
                "ipSecurityRestrictions": [
                    { "vnetSubnetResourceId": "/subscriptions/s/.../subnets/app", "action": "Allow" }
                ]
            }
        });
        assert!(extract_access_restricted(&vnet));

        let default_deny = json!({
            "properties": { "ipSecurityRestrictionsDefaultAction": "Deny" }
        });
        assert!(extract_access_restricted(&default_deny));
    }

    #[test]
    fn fetch_combines_image_and_access_posture() {
        let v = json!({
            "properties": {
                "linuxFxVersion": "DOCKER|acr/api:v1",
                "ipSecurityRestrictions": [
                    { "ipAddress": "10.0.0.0/8", "action": "Allow" }
                ]
            }
        });
        // Exercise the two extractors the way `fetch` composes them.
        assert_eq!(extract(&v).as_deref(), Some("acr/api:v1"));
        assert!(extract_access_restricted(&v));
    }
}
