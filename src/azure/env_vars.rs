//! A normalized environment-variable model shared by the two API-asset kinds
//! that expose them: Container Apps (revision template `env` array) and
//! Function Apps (`config/appsettings`). Both flatten to a `name` + a display
//! `value` + an `is_secret` flag.
//!
//! `is_secret` marks entries whose value is *not* a literal the user typed:
//! Container App `secretRef` entries (the literal value is never returned by
//! ARM) and Function App Key Vault references (`@Microsoft.KeyVault(...)`).
//! The detail view masks *all* values by default regardless; `is_secret` only
//! influences how a revealed entry is annotated.

#![allow(dead_code)]

/// One environment variable / app setting, normalized for display.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EnvVar {
    pub name: String,
    /// The value to show when revealed. For secret-backed entries this is a
    /// human marker (e.g. `(secret: my-secret)`) rather than an actual secret —
    /// ARM never returns the resolved value for those.
    pub value: String,
    pub is_secret: bool,
    /// Display label for the owning container on the env-vars page (Container
    /// Apps): the container name, with an `(init)` suffix for init containers
    /// (e.g. `"files"`, `"migrate (init)"`). `None` for flat sources without a
    /// container dimension (Function Apps), where the column is hidden. Set by
    /// [`crate::azure::container_app_overview::explode_container_env`]; the raw
    /// per-container parser leaves it `None`.
    pub attribution: Option<String>,
    /// Raw owning container name (no `(init)` suffix) used to target the exact
    /// template entry when *writing* an edit back. `None` for flat sources.
    /// Paired with [`EnvVar::is_init`] to disambiguate a name that exists in
    /// both `containers` and `initContainers`.
    pub container: Option<String>,
    /// `true` when the owning container is an init container — needed alongside
    /// [`EnvVar::container`] to locate the right entry for write-back.
    pub is_init: bool,
}

/// Parse a Container App revision/template `env` array
/// (`properties.template.containers[N].env`). Each entry is either
/// `{ "name", "value" }` (literal) or `{ "name", "secretRef" }` (points at a
/// container-app secret, whose value ARM does not expose). Entries missing a
/// name are skipped. Result is sorted by name for stable rendering.
pub fn from_container_env(env: &serde_json::Value) -> Vec<EnvVar> {
    let Some(arr) = env.as_array() else {
        return Vec::new();
    };
    let mut out: Vec<EnvVar> = arr
        .iter()
        .filter_map(|e| {
            let name = e.get("name").and_then(|v| v.as_str())?.to_string();
            if name.is_empty() {
                return None;
            }
            if let Some(secret) = e.get("secretRef").and_then(|v| v.as_str()) {
                Some(EnvVar {
                    name,
                    value: format!("(secret: {secret})"),
                    is_secret: true,
                    ..Default::default()
                })
            } else {
                let value = e
                    .get("value")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                Some(EnvVar {
                    name,
                    value,
                    is_secret: false,
                    ..Default::default()
                })
            }
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Parse a Function App app-settings `properties` object — a flat
/// `{ "KEY": "value", ... }` map. Values shaped like
/// `@Microsoft.KeyVault(SecretUri=...)` are flagged `is_secret` so a revealed
/// entry reads as a reference rather than a typed-in literal. Sorted by name.
pub fn from_app_settings(properties: &serde_json::Value) -> Vec<EnvVar> {
    let Some(map) = properties.as_object() else {
        return Vec::new();
    };
    let mut out: Vec<EnvVar> = map
        .iter()
        .map(|(k, v)| {
            let value = v.as_str().unwrap_or("").to_string();
            let is_secret = value.starts_with("@Microsoft.KeyVault(");
            EnvVar {
                name: k.clone(),
                value,
                is_secret,
                ..Default::default()
            }
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn container_env_splits_literal_and_secret_ref() {
        let env = json!([
            { "name": "LOG_LEVEL", "value": "info" },
            { "name": "DB_PASSWORD", "secretRef": "db-password" },
            { "name": "EMPTY" }
        ]);
        let vars = from_container_env(&env);
        // Sorted by name: DB_PASSWORD, EMPTY, LOG_LEVEL.
        assert_eq!(vars.len(), 3);
        assert_eq!(vars[0].name, "DB_PASSWORD");
        assert!(vars[0].is_secret);
        assert_eq!(vars[0].value, "(secret: db-password)");
        assert_eq!(vars[1].name, "EMPTY");
        assert_eq!(vars[1].value, "");
        assert!(!vars[1].is_secret);
        assert_eq!(vars[2].name, "LOG_LEVEL");
        assert_eq!(vars[2].value, "info");
    }

    #[test]
    fn container_env_skips_nameless_entries() {
        let env = json!([{ "value": "orphan" }, { "name": "", "value": "x" }]);
        assert!(from_container_env(&env).is_empty());
    }

    #[test]
    fn container_env_non_array_is_empty() {
        assert!(from_container_env(&json!({})).is_empty());
        assert!(from_container_env(&serde_json::Value::Null).is_empty());
    }

    #[test]
    fn app_settings_flags_key_vault_refs() {
        let props = json!({
            "WEBSITE_RUN_FROM_PACKAGE": "1",
            "ApiKey": "@Microsoft.KeyVault(SecretUri=https://v.vault.azure.net/secrets/api-key/)"
        });
        let vars = from_app_settings(&props);
        assert_eq!(vars.len(), 2);
        // Sorted: ApiKey, WEBSITE_RUN_FROM_PACKAGE.
        assert_eq!(vars[0].name, "ApiKey");
        assert!(vars[0].is_secret);
        assert_eq!(vars[1].name, "WEBSITE_RUN_FROM_PACKAGE");
        assert!(!vars[1].is_secret);
        assert_eq!(vars[1].value, "1");
    }

    #[test]
    fn app_settings_non_object_is_empty() {
        assert!(from_app_settings(&json!([])).is_empty());
    }
}
