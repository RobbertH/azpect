//! Write a single environment-variable edit back to a Container App.
//!
//! Container App revisions are immutable: you never edit a running container in
//! place. Changing an env var means replacing the revision template, which spins
//! up a NEW revision (in the default single-revision mode it immediately takes
//! 100% of traffic — the "click click done" the portal shows). So the write is a
//! read-modify-write of the whole template:
//!
//! 1. GET the app to get the current `properties.template` (this is the *raw*
//!    template, NOT the exploded display model — each container's `env` array
//!    maps 1:1 to what we edit).
//! 2. Locate the target container (`containers` or `initContainers`, by name +
//!    init flag) and set/insert the `{name,value}` entry, preserving every other
//!    entry — including untouched `secretRef` ones.
//! 3. PATCH the resource with just `{properties:{template}}` so we don't replay
//!    read-only fields.
//!
//! Only plain literal entries are editable. A `secretRef` entry's literal value
//! lives in `properties.configuration.secrets`, which a template GET never
//! returns, so the UI blocks editing those and this module refuses to convert a
//! `secretRef` entry into a literal.

#![allow(dead_code, unused_variables)]

use anyhow::{anyhow, Context};

use crate::azure::auth::AzureAuth;
use crate::azure::client::ArmClient;

/// Microsoft.App API version used for the read + write. Matches the overview
/// fetch so we never mix template shapes across versions.
const API_VERSION: &str = "2024-03-01";

/// Identifies the exact template entry an edit targets, carried over from the
/// exploded display row ([`crate::azure::env_vars::EnvVar`]).
#[derive(Clone, Debug)]
pub struct EnvTarget {
    /// Raw owning-container name (no `(init)` suffix).
    pub container: String,
    /// `true` ⇒ the container lives in `initContainers`, not `containers`.
    pub is_init: bool,
    /// Env-var name to set or insert.
    pub name: String,
}

/// Read the app, apply the edit to its template, and PATCH it back. On success
/// the new revision is being provisioned (the call returns once ARM accepts the
/// PATCH, not once the revision is healthy).
pub async fn update(
    auth: &AzureAuth,
    container_app_id: &str,
    target: &EnvTarget,
    new_value: &str,
) -> anyhow::Result<()> {
    let client = ArmClient::new(auth.clone())?;
    let mut app = client
        .get(container_app_id, &[("api-version", API_VERSION)])
        .await
        .with_context(|| format!("fetching container app {container_app_id} for edit"))?;

    let template = app
        .pointer_mut("/properties/template")
        .ok_or_else(|| anyhow!("container app {container_app_id} has no properties.template"))?;
    apply_env_edit(template, target, new_value)?;

    let template = template.clone();
    let body = serde_json::json!({ "properties": { "template": template } });
    client
        .patch(
            &format!("{container_app_id}?api-version={API_VERSION}"),
            &body,
        )
        .await
        .with_context(|| format!("patching container app {container_app_id}"))?;
    Ok(())
}

/// Set (or insert) the `{name,value}` entry in the targeted container's `env`
/// array, in place, on a raw `properties.template` value. Pure + testable.
///
/// Errors if the target container can't be found, or if the named entry already
/// exists as a `secretRef` (converting it to a literal would silently change its
/// meaning — the UI prevents reaching here, this is the backstop).
pub fn apply_env_edit(
    template: &mut serde_json::Value,
    target: &EnvTarget,
    new_value: &str,
) -> anyhow::Result<()> {
    let array_key = if target.is_init {
        "initContainers"
    } else {
        "containers"
    };
    let containers = template
        .get_mut(array_key)
        .and_then(|v| v.as_array_mut())
        .ok_or_else(|| anyhow!("template has no {array_key} array"))?;

    let container = containers
        .iter_mut()
        .find(|c| c.get("name").and_then(|n| n.as_str()) == Some(target.container.as_str()))
        .ok_or_else(|| anyhow!("container '{}' not found in {array_key}", target.container))?;

    // Ensure an `env` array exists, then upsert.
    let env = container
        .as_object_mut()
        .ok_or_else(|| anyhow!("container '{}' is not an object", target.container))?
        .entry("env")
        .or_insert_with(|| serde_json::Value::Array(Vec::new()));
    let env = env
        .as_array_mut()
        .ok_or_else(|| anyhow!("container '{}' env is not an array", target.container))?;

    if let Some(existing) = env
        .iter_mut()
        .find(|e| e.get("name").and_then(|n| n.as_str()) == Some(target.name.as_str()))
    {
        if existing.get("secretRef").is_some() {
            return Err(anyhow!(
                "'{}' is a secret reference; edit it in the Container App's secrets, not here",
                target.name
            ));
        }
        let obj = existing
            .as_object_mut()
            .ok_or_else(|| anyhow!("env entry '{}' is not an object", target.name))?;
        obj.insert(
            "value".to_string(),
            serde_json::Value::String(new_value.to_string()),
        );
    } else {
        env.push(serde_json::json!({ "name": target.name, "value": new_value }));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn target(container: &str, name: &str) -> EnvTarget {
        EnvTarget {
            container: container.into(),
            is_init: false,
            name: name.into(),
        }
    }

    #[test]
    fn edits_existing_literal_in_named_container() {
        let mut tpl = json!({
            "containers": [
                { "name": "files", "env": [ { "name": "LOG_LEVEL", "value": "info" } ] },
                { "name": "api", "env": [ { "name": "LOG_LEVEL", "value": "info" } ] }
            ]
        });
        apply_env_edit(&mut tpl, &target("api", "LOG_LEVEL"), "debug").unwrap();
        // Only the targeted container changes; the other keeps its value.
        assert_eq!(tpl["containers"][0]["env"][0]["value"], json!("info"));
        assert_eq!(tpl["containers"][1]["env"][0]["value"], json!("debug"));
    }

    #[test]
    fn inserts_new_var_when_absent() {
        let mut tpl = json!({
            "containers": [ { "name": "app", "env": [ { "name": "A", "value": "1" } ] } ]
        });
        apply_env_edit(&mut tpl, &target("app", "B"), "2").unwrap();
        let env = tpl["containers"][0]["env"].as_array().unwrap();
        assert_eq!(env.len(), 2);
        assert!(env
            .iter()
            .any(|e| e["name"] == json!("B") && e["value"] == json!("2")));
    }

    #[test]
    fn creates_env_array_when_missing() {
        let mut tpl = json!({ "containers": [ { "name": "app" } ] });
        apply_env_edit(&mut tpl, &target("app", "A"), "1").unwrap();
        assert_eq!(tpl["containers"][0]["env"][0]["value"], json!("1"));
    }

    #[test]
    fn refuses_to_overwrite_a_secret_ref() {
        let mut tpl = json!({
            "containers": [ { "name": "app", "env": [ { "name": "DB", "secretRef": "db-conn" } ] } ]
        });
        let err = apply_env_edit(&mut tpl, &target("app", "DB"), "plaintext").unwrap_err();
        assert!(err.to_string().contains("secret reference"));
        // Untouched — still a secretRef.
        assert_eq!(
            tpl["containers"][0]["env"][0]["secretRef"],
            json!("db-conn")
        );
    }

    #[test]
    fn targets_init_containers_separately() {
        let mut tpl = json!({
            "containers": [ { "name": "app", "env": [ { "name": "X", "value": "main" } ] } ],
            "initContainers": [ { "name": "app", "env": [ { "name": "X", "value": "init" } ] } ]
        });
        let t = EnvTarget {
            container: "app".into(),
            is_init: true,
            name: "X".into(),
        };
        apply_env_edit(&mut tpl, &t, "edited").unwrap();
        // The init container changes; the main one is left alone despite the
        // shared name.
        assert_eq!(tpl["containers"][0]["env"][0]["value"], json!("main"));
        assert_eq!(tpl["initContainers"][0]["env"][0]["value"], json!("edited"));
    }

    #[test]
    fn errors_when_container_missing() {
        let mut tpl = json!({ "containers": [ { "name": "app", "env": [] } ] });
        let err = apply_env_edit(&mut tpl, &target("nope", "A"), "1").unwrap_err();
        assert!(err.to_string().contains("not found"));
    }
}
