//! Read the configured CPU/memory caps from a Container App's template so the
//! detail view can show "latest: X / max Y mCores" instead of "latest: X" with
//! no context. Sum across containers in the template — that's the spec a user
//! pays for, even on apps with a sidecar.

#![allow(dead_code, unused_variables)]

use anyhow::Context;

use crate::azure::auth::AzureAuth;
use crate::azure::client::ArmClient;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ContainerAppLimits {
    /// Total reserved CPU across all containers in the template, expressed in
    /// millicores. `0.5` CPU → `500` mCores.
    pub cpu_millicores: u32,
    /// Total reserved memory across all containers in the template, in bytes.
    pub memory_bytes: u64,
    /// Public ingress FQDN if ingress is enabled (e.g.
    /// `my-app.westeurope.azurecontainerapps.io`). `None` when the app has
    /// no ingress configured or ingress is internal-only.
    pub fqdn: Option<String>,
}

const API_VERSION: &str = "2024-03-01";

pub async fn fetch(auth: &AzureAuth, container_app_id: &str) -> anyhow::Result<ContainerAppLimits> {
    let client = ArmClient::new(auth.clone())?;
    let app = client
        .get(container_app_id, &[("api-version", API_VERSION)])
        .await
        .with_context(|| format!("fetching container app {container_app_id}"))?;
    Ok(extract(&app))
}

pub fn extract(value: &serde_json::Value) -> ContainerAppLimits {
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

    let mut total = ContainerAppLimits {
        fqdn,
        ..ContainerAppLimits::default()
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
        assert_eq!(l, ContainerAppLimits::default());
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
}
