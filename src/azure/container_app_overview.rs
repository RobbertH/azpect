//! Parse the per-app overview metadata azpect surfaces in the Detail view from
//! a single Container App GET: the configured CPU/memory/ephemeral caps, public
//! ingress FQDN, the managed environment it runs in, its managed-identity
//! summary, and the primary container's environment variables. CPU/memory/
//! ephemeral are summed across containers in the template — that's the spec a
//! user pays for, even on apps with a sidecar.
//!
//! (The struct keeps the historical `ContainerAppOverview` name; it now carries
//! more than limits, all read from the one GET so there's no extra round trip.)

#![allow(dead_code, unused_variables)]

use anyhow::Context;

use crate::azure::auth::AzureAuth;
use crate::azure::client::ArmClient;
use crate::azure::env_vars::EnvVar;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ContainerAppOverview {
    /// Total reserved CPU across all containers in the template, expressed in
    /// millicores. `0.5` CPU → `500` mCores.
    pub cpu_millicores: u32,
    /// Total reserved memory across all containers in the template, in bytes.
    pub memory_bytes: u64,
    /// Ephemeral (scratch) storage of the primary container, kept verbatim in
    /// Azure's own notation (e.g. `"4Gi"`). Read-only, derived from the SKU.
    /// `None` when not reported.
    pub ephemeral_storage: Option<String>,
    /// Public ingress FQDN if ingress is enabled (e.g.
    /// `my-app.westeurope.azurecontainerapps.io`). `None` when the app has
    /// no ingress configured or ingress is internal-only.
    pub fqdn: Option<String>,
    /// Name of the Container Apps managed environment this app runs in — the
    /// last path segment of `properties.managedEnvironmentId`. `None` if absent.
    pub managed_environment: Option<String>,
    /// One-line managed-identity summary (see [`summarize_identity`]). `None`
    /// when the app has no managed identity configured.
    pub managed_identity: Option<String>,
    /// Environment variables from the primary container's template. Masked in
    /// the UI by default; see [`crate::azure::env_vars`].
    pub env_vars: Vec<EnvVar>,
}

const API_VERSION: &str = "2024-03-01";

pub async fn fetch(
    auth: &AzureAuth,
    container_app_id: &str,
) -> anyhow::Result<ContainerAppOverview> {
    let client = ArmClient::new(auth.clone())?;
    let app = client
        .get(container_app_id, &[("api-version", API_VERSION)])
        .await
        .with_context(|| format!("fetching container app {container_app_id}"))?;
    Ok(extract(&app))
}

pub fn extract(value: &serde_json::Value) -> ContainerAppOverview {
    let containers = value
        .pointer("/properties/template/containers")
        .and_then(|v| v.as_array())
        .map(|a| a.as_slice())
        .unwrap_or(&[]);

    let fqdn = value
        .pointer("/properties/configuration/ingress/fqdn")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let managed_environment = value
        .pointer("/properties/managedEnvironmentId")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .and_then(|s| s.rsplit('/').next())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let managed_identity = summarize_identity(value.get("identity"));

    let env_vars = value
        .pointer("/properties/template/containers/0/env")
        .map(crate::azure::env_vars::from_container_env)
        .unwrap_or_default();

    // Ephemeral storage is reported per container; show the primary container's
    // value verbatim (Azure's own `4Gi`-style notation), matching how `image`
    // tracks the primary container.
    let ephemeral_storage = value
        .pointer("/properties/template/containers/0/resources/ephemeralStorage")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let mut total = ContainerAppOverview {
        fqdn,
        managed_environment,
        managed_identity,
        ephemeral_storage,
        env_vars,
        ..ContainerAppOverview::default()
    };
    for c in containers {
        let cpu_cores = c
            .pointer("/resources/cpu")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        // Cores → millicores. Round to nearest int; ARM stores values like
        // 0.25 / 0.5 / 0.75 / 1.0 so this is always exact in practice.
        let cpu_mc = (cpu_cores * 1000.0).round();
        if cpu_mc > 0.0 && cpu_mc < u32::MAX as f64 {
            total.cpu_millicores = total.cpu_millicores.saturating_add(cpu_mc as u32);
        }

        let mem_str = c
            .pointer("/resources/memory")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        total.memory_bytes = total
            .memory_bytes
            .saturating_add(parse_memory_bytes(mem_str));
    }
    total
}

/// Summarize an ARM `identity` object into one display line. Returns `None`
/// when no managed identity is configured (type `None`/absent) so the renderer
/// can omit the line entirely. Otherwise one of:
/// - `SystemAssigned`
/// - `UserAssigned: name-a, name-b`
/// - `SystemAssigned + UserAssigned: name-a, name-b`
///
/// User-assigned names are the last path segment of each
/// `userAssignedIdentities` key, sorted for stable output and capped at 3
/// (`…, +N more`). No principal/tenant GUIDs are surfaced.
fn summarize_identity(identity: Option<&serde_json::Value>) -> Option<String> {
    let identity = identity?;
    let ty = identity
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("None");
    let has_system = ty.contains("SystemAssigned");
    let has_user = ty.contains("UserAssigned");
    if !has_system && !has_user {
        return None;
    }

    let mut parts: Vec<String> = Vec::new();
    if has_system {
        parts.push("SystemAssigned".to_string());
    }
    if has_user {
        let mut names: Vec<String> = identity
            .get("userAssignedIdentities")
            .and_then(|v| v.as_object())
            .map(|m| {
                m.keys()
                    .filter_map(|k| k.rsplit('/').next())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        parts.push(format_user_assigned(&names));
    }
    Some(parts.join(" + "))
}

fn format_user_assigned(names: &[String]) -> String {
    const CAP: usize = 3;
    if names.is_empty() {
        "UserAssigned".to_string()
    } else if names.len() <= CAP {
        format!("UserAssigned: {}", names.join(", "))
    } else {
        format!(
            "UserAssigned: {}, +{} more",
            names[..CAP].join(", "),
            names.len() - CAP
        )
    }
}

/// Parse Container Apps memory strings like `"0.5Gi"`, `"512Mi"`, `"1Gi"`.
/// Container Apps doesn't accept arbitrary units, but be tolerant of both
/// IEC (`Gi`/`Mi`) and SI (`G`/`M`) forms. Returns 0 on parse failure so a
/// bad row doesn't poison the whole sum.
fn parse_memory_bytes(s: &str) -> u64 {
    let s = s.trim();
    if s.is_empty() {
        return 0;
    }

    let (number, unit) = split_number_and_unit(s);
    let n: f64 = match number.parse() {
        Ok(v) => v,
        Err(_) => return 0,
    };

    let multiplier: f64 = match unit {
        "Gi" => 1024.0 * 1024.0 * 1024.0,
        "Mi" => 1024.0 * 1024.0,
        "Ki" => 1024.0,
        "G" => 1_000_000_000.0,
        "M" => 1_000_000.0,
        "K" | "k" => 1_000.0,
        "" => 1.0,
        _ => return 0,
    };

    let bytes = n * multiplier;
    if bytes < 0.0 || bytes >= u64::MAX as f64 {
        0
    } else {
        bytes as u64
    }
}

fn split_number_and_unit(s: &str) -> (&str, &str) {
    let split_at = s
        .char_indices()
        .find(|(_, c)| c.is_alphabetic())
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    (s[..split_at].trim(), s[split_at..].trim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn single_container_with_half_cpu_and_one_gib() {
        let v = json!({
            "properties": {
                "template": {
                    "containers": [
                        { "resources": { "cpu": 0.5, "memory": "1Gi" } }
                    ]
                }
            }
        });
        let l = extract(&v);
        assert_eq!(l.cpu_millicores, 500);
        assert_eq!(l.memory_bytes, 1024 * 1024 * 1024);
    }

    #[test]
    fn sums_across_multiple_containers() {
        // App + sidecar split: ARM stores per-container reservations.
        let v = json!({
            "properties": {
                "template": {
                    "containers": [
                        { "resources": { "cpu": 0.25, "memory": "512Mi" } },
                        { "resources": { "cpu": 0.5,  "memory": "1Gi" } }
                    ]
                }
            }
        });
        let l = extract(&v);
        assert_eq!(l.cpu_millicores, 750);
        assert_eq!(l.memory_bytes, 512 * 1024 * 1024 + 1024 * 1024 * 1024);
    }

    #[test]
    fn missing_template_returns_zero_limits() {
        let v = json!({});
        let l = extract(&v);
        assert_eq!(l, ContainerAppOverview::default());
    }

    #[test]
    fn unknown_memory_unit_falls_back_to_zero_for_that_row() {
        // 0.25 CPU survives even if memory string is malformed.
        let v = json!({
            "properties": {
                "template": {
                    "containers": [
                        { "resources": { "cpu": 0.25, "memory": "9Z9Z" } }
                    ]
                }
            }
        });
        let l = extract(&v);
        assert_eq!(l.cpu_millicores, 250);
        assert_eq!(l.memory_bytes, 0);
    }

    #[test]
    fn parses_mi_and_gi_correctly() {
        assert_eq!(parse_memory_bytes("512Mi"), 512 * 1024 * 1024);
        assert_eq!(
            parse_memory_bytes("0.5Gi"),
            (0.5 * 1024.0 * 1024.0 * 1024.0) as u64
        );
        assert_eq!(parse_memory_bytes("2Gi"), 2 * 1024 * 1024 * 1024);
    }

    #[test]
    fn extracts_fqdn_when_ingress_is_configured() {
        let v = json!({
            "properties": {
                "configuration": {
                    "ingress": { "fqdn": "files-api.example.azurecontainerapps.io" }
                },
                "template": {
                    "containers": [{ "resources": { "cpu": 0.5, "memory": "1Gi" } }]
                }
            }
        });
        let l = extract(&v);
        assert_eq!(
            l.fqdn.as_deref(),
            Some("files-api.example.azurecontainerapps.io")
        );
    }

    #[test]
    fn no_fqdn_when_ingress_block_absent() {
        let v = json!({
            "properties": {
                "template": {
                    "containers": [{ "resources": { "cpu": 0.5, "memory": "1Gi" } }]
                }
            }
        });
        assert!(extract(&v).fqdn.is_none());
    }

    #[test]
    fn parses_si_units_too() {
        assert_eq!(parse_memory_bytes("500M"), 500_000_000);
        assert_eq!(parse_memory_bytes("1G"), 1_000_000_000);
    }

    #[test]
    fn extracts_environment_name_from_managed_environment_id() {
        let v = json!({
            "properties": {
                "managedEnvironmentId": "/subscriptions/s/resourceGroups/rg/providers/Microsoft.App/managedEnvironments/my-managed-env",
                "template": { "containers": [{ "resources": { "cpu": 0.5, "memory": "1Gi" } }] }
            }
        });
        assert_eq!(
            extract(&v).managed_environment.as_deref(),
            Some("my-managed-env")
        );
    }

    #[test]
    fn ephemeral_storage_kept_verbatim_from_primary_container() {
        let v = json!({
            "properties": {
                "template": {
                    "containers": [
                        { "resources": { "cpu": 0.25, "memory": "512Mi", "ephemeralStorage": "4Gi" } }
                    ]
                }
            }
        });
        // Azure's notation is preserved exactly, not reformatted to bytes/GB.
        assert_eq!(extract(&v).ephemeral_storage.as_deref(), Some("4Gi"));
    }

    #[test]
    fn extracts_env_vars_from_primary_container() {
        let v = json!({
            "properties": {
                "template": {
                    "containers": [{
                        "resources": { "cpu": 0.5, "memory": "1Gi" },
                        "env": [
                            { "name": "LOG_LEVEL", "value": "debug" },
                            { "name": "TOKEN", "secretRef": "api-token" }
                        ]
                    }]
                }
            }
        });
        let l = extract(&v);
        assert_eq!(l.env_vars.len(), 2);
        // Sorted by name: LOG_LEVEL, TOKEN.
        assert_eq!(l.env_vars[0].name, "LOG_LEVEL");
        assert!(l.env_vars[1].is_secret);
    }

    #[test]
    fn summarize_identity_none_when_unset() {
        assert!(summarize_identity(None).is_none());
        assert!(summarize_identity(Some(&json!({ "type": "None" }))).is_none());
    }

    #[test]
    fn summarize_identity_system_only() {
        let id = json!({ "type": "SystemAssigned", "principalId": "ignored-guid" });
        assert_eq!(
            summarize_identity(Some(&id)).as_deref(),
            Some("SystemAssigned")
        );
    }

    #[test]
    fn summarize_identity_user_assigned_lists_sorted_names() {
        let id = json!({
            "type": "UserAssigned",
            "userAssignedIdentities": {
                "/subscriptions/s/resourceGroups/rg/providers/Microsoft.ManagedIdentity/userAssignedIdentities/uami-b": {},
                "/subscriptions/s/resourceGroups/rg/providers/Microsoft.ManagedIdentity/userAssignedIdentities/uami-a": {}
            }
        });
        assert_eq!(
            summarize_identity(Some(&id)).as_deref(),
            Some("UserAssigned: uami-a, uami-b")
        );
    }

    #[test]
    fn summarize_identity_system_and_user_combined() {
        let id = json!({
            "type": "SystemAssigned, UserAssigned",
            "userAssignedIdentities": {
                "/subscriptions/s/resourceGroups/rg/providers/Microsoft.ManagedIdentity/userAssignedIdentities/uami": {}
            }
        });
        assert_eq!(
            summarize_identity(Some(&id)).as_deref(),
            Some("SystemAssigned + UserAssigned: uami")
        );
    }

    #[test]
    fn format_user_assigned_caps_at_three() {
        let names: Vec<String> = ["a", "b", "c", "d", "e"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            format_user_assigned(&names),
            "UserAssigned: a, b, c, +2 more"
        );
    }
}
