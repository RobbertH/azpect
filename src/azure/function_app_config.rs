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

/// GET the Function App's web config and pull out its container image, if any.
pub async fn fetch_image(
    auth: &AzureAuth,
    function_app_id: &str,
) -> anyhow::Result<Option<String>> {
    let client = ArmClient::new(auth.clone())?;
    let path = format!("{function_app_id}/config/web");
    let resp = client
        .get(&path, &[("api-version", API_VERSION)])
        .await
        .with_context(|| format!("fetching web config for {function_app_id}"))?;
    Ok(extract(&resp))
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
}
