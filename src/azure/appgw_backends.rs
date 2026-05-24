//! Application Gateway backend pools: enumerate the pools attached to one
//! Application Gateway and, for each pool, the static backend addresses
//! (FQDN / IP) plus references to NIC IP configurations. Used by the
//! `appgw_backends` UI view (Enter on an Application Gateway in the resource
//! list).
//!
//! Single ARM call: `GET {appgw_resource_id}?api-version=…` returns the full
//! gateway document; the pool list lives at `properties.backendAddressPools`.
//! NIC references are *not* resolved to concrete IPs — that would require N
//! extra ARM lookups per pool — only their `nic / ipconfig` names are surfaced
//! so the user can see what's wired up.
//!
//! Backend *health* (live per-server probe verdicts) lives in the sibling
//! [`crate::azure::appgw_health`] module: it's an async ARM operation (POST
//! `/backendhealth` → 202 + a polled long-running operation), so it's kept
//! separate and fetched lazily when the user toggles the view into health mode.

#![allow(dead_code)]

use anyhow::{anyhow, Context};

use crate::azure::auth::AzureAuth;
use crate::azure::client::ArmClient;

const API_VERSION: &str = "2023-09-01";

/// One backend pool on an Application Gateway. A pool may contain any mix of
/// static addresses and NIC IP-config references (or be empty — that's a valid
/// gateway-configured-but-not-targeted state).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BackendPool {
    /// Pool name from `properties.backendAddressPools[].name`. Falls back to
    /// `(unnamed)` if the upstream document somehow omits it.
    pub name: String,
    pub addresses: Vec<BackendAddress>,
    pub nic_ip_config_refs: Vec<NicIpConfigRef>,
}

/// A static backend address. ARM populates exactly one of `fqdn`/`ip_address`
/// for most pools, but both fields may be set (rare) or both unset (also rare:
/// a placeholder entry); we preserve whatever ARM hands us.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BackendAddress {
    pub fqdn: Option<String>,
    pub ip_address: Option<String>,
}

/// A reference to a NIC IP configuration that's been wired into a pool. We do
/// not resolve these to concrete IPs (would be one extra ARM call per ref);
/// the view just renders `nic_name / config_name` so the operator can pivot
/// in the portal if they need the actual address.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NicIpConfigRef {
    pub nic_name: String,
    pub config_name: String,
    pub full_id: String,
}

/// `GET {appgw_resource_id}?api-version=…` and parse the pool list.
///
/// Order matches what ARM returns (which is the gateway's authoring order);
/// we deliberately do not sort so a pool's position in the view matches the
/// portal.
pub async fn list_backend_pools(
    auth: &AzureAuth,
    appgw_resource_id: &str,
) -> anyhow::Result<Vec<BackendPool>> {
    let client = ArmClient::new(auth.clone())?;
    let resp = client
        .get(appgw_resource_id, &[("api-version", API_VERSION)])
        .await
        .with_context(|| format!("get application gateway {appgw_resource_id}"))?;
    parse_pools(&resp)
}

fn parse_pools(value: &serde_json::Value) -> anyhow::Result<Vec<BackendPool>> {
    // The pools array lives at properties.backendAddressPools. A gateway with
    // zero pools is valid (freshly created, no targets yet); only a totally
    // malformed response — no `properties` at all — is treated as an error.
    let props = value
        .get("properties")
        .ok_or_else(|| anyhow!("application gateway response missing 'properties'"))?;
    let arr = match props.get("backendAddressPools").and_then(|v| v.as_array()) {
        Some(a) => a,
        None => return Ok(Vec::new()),
    };
    Ok(arr.iter().map(parse_one_pool).collect())
}

fn parse_one_pool(v: &serde_json::Value) -> BackendPool {
    let name = v
        .get("name")
        .and_then(|n| n.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("(unnamed)")
        .to_string();
    let props = v.get("properties");

    let addresses = props
        .and_then(|p| p.get("backendAddresses"))
        .and_then(|a| a.as_array())
        .map(|arr| arr.iter().map(parse_one_address).collect())
        .unwrap_or_default();

    let nic_ip_config_refs = props
        .and_then(|p| p.get("backendIPConfigurations"))
        .and_then(|a| a.as_array())
        .map(|arr| arr.iter().filter_map(parse_one_nic_ref).collect())
        .unwrap_or_default();

    BackendPool {
        name,
        addresses,
        nic_ip_config_refs,
    }
}

fn parse_one_address(v: &serde_json::Value) -> BackendAddress {
    let fqdn = v
        .get("fqdn")
        .and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let ip_address = v
        .get("ipAddress")
        .and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    BackendAddress { fqdn, ip_address }
}

fn parse_one_nic_ref(v: &serde_json::Value) -> Option<NicIpConfigRef> {
    let id = v.get("id")?.as_str()?.to_string();
    let (nic_name, config_name) = parse_nic_id(&id);
    Some(NicIpConfigRef {
        nic_name,
        config_name,
        full_id: id,
    })
}

/// Pull the NIC name and IP-config name out of an ARM id like
/// `…/networkInterfaces/{nic}/ipConfigurations/{cfg}`. Falls back to
/// `(unknown)` for either segment if the id doesn't match the expected shape
/// — we'd rather render a row than drop a backend reference on the floor.
fn parse_nic_id(id: &str) -> (String, String) {
    let mut nic = String::from("(unknown)");
    let mut cfg = String::from("(unknown)");
    let parts: Vec<&str> = id.split('/').collect();
    for i in 0..parts.len() {
        if parts[i].eq_ignore_ascii_case("networkInterfaces") && i + 1 < parts.len() {
            nic = parts[i + 1].to_string();
        }
        if parts[i].eq_ignore_ascii_case("ipConfigurations") && i + 1 < parts.len() {
            cfg = parts[i + 1].to_string();
        }
    }
    (nic, cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn appgw(pools: serde_json::Value) -> serde_json::Value {
        json!({
            "id": "/subscriptions/s/resourceGroups/rg/providers/Microsoft.Network/applicationGateways/gw",
            "name": "gw",
            "properties": { "backendAddressPools": pools }
        })
    }

    #[test]
    fn pool_with_fqdn_address() {
        let payload = appgw(json!([
            {
                "name": "pool-fqdn",
                "properties": {
                    "backendAddresses": [{ "fqdn": "api.example.com" }]
                }
            }
        ]));
        let pools = parse_pools(&payload).unwrap();
        assert_eq!(pools.len(), 1);
        assert_eq!(pools[0].name, "pool-fqdn");
        assert_eq!(pools[0].addresses.len(), 1);
        assert_eq!(
            pools[0].addresses[0].fqdn.as_deref(),
            Some("api.example.com")
        );
        assert!(pools[0].addresses[0].ip_address.is_none());
        assert!(pools[0].nic_ip_config_refs.is_empty());
    }

    #[test]
    fn pool_with_ip_address() {
        let payload = appgw(json!([
            {
                "name": "pool-ip",
                "properties": {
                    "backendAddresses": [{ "ipAddress": "10.0.1.4" }]
                }
            }
        ]));
        let pools = parse_pools(&payload).unwrap();
        assert_eq!(
            pools[0].addresses[0].ip_address.as_deref(),
            Some("10.0.1.4")
        );
        assert!(pools[0].addresses[0].fqdn.is_none());
    }

    #[test]
    fn pool_with_nic_refs() {
        let payload = appgw(json!([
            {
                "name": "pool-nic",
                "properties": {
                    "backendIPConfigurations": [
                        {
                            "id": "/subscriptions/s/resourceGroups/rg/providers/Microsoft.Network/networkInterfaces/nic-web-01/ipConfigurations/ipconfig1"
                        }
                    ]
                }
            }
        ]));
        let pools = parse_pools(&payload).unwrap();
        let nic_ref = &pools[0].nic_ip_config_refs[0];
        assert_eq!(nic_ref.nic_name, "nic-web-01");
        assert_eq!(nic_ref.config_name, "ipconfig1");
        assert!(nic_ref.full_id.ends_with("/ipconfig1"));
    }

    #[test]
    fn pool_with_all_three() {
        let payload = appgw(json!([
            {
                "name": "everything",
                "properties": {
                    "backendAddresses": [
                        { "fqdn": "api.example.com", "ipAddress": "10.0.1.4" }
                    ],
                    "backendIPConfigurations": [
                        { "id": "/x/networkInterfaces/nic-a/ipConfigurations/cfg-a" }
                    ]
                }
            }
        ]));
        let pools = parse_pools(&payload).unwrap();
        assert_eq!(pools[0].name, "everything");
        assert_eq!(
            pools[0].addresses[0].fqdn.as_deref(),
            Some("api.example.com")
        );
        assert_eq!(
            pools[0].addresses[0].ip_address.as_deref(),
            Some("10.0.1.4")
        );
        assert_eq!(pools[0].nic_ip_config_refs[0].nic_name, "nic-a");
        assert_eq!(pools[0].nic_ip_config_refs[0].config_name, "cfg-a");
    }

    #[test]
    fn pool_with_none() {
        let payload = appgw(json!([{ "name": "empty", "properties": {} }]));
        let pools = parse_pools(&payload).unwrap();
        assert_eq!(pools[0].name, "empty");
        assert!(pools[0].addresses.is_empty());
        assert!(pools[0].nic_ip_config_refs.is_empty());
    }

    #[test]
    fn unnamed_pool_gets_placeholder() {
        let payload = appgw(json!([{ "properties": {} }]));
        let pools = parse_pools(&payload).unwrap();
        assert_eq!(pools[0].name, "(unnamed)");
    }

    #[test]
    fn missing_backend_pools_array_yields_empty() {
        let payload = json!({ "properties": {} });
        let pools = parse_pools(&payload).unwrap();
        assert!(pools.is_empty());
    }

    #[test]
    fn missing_properties_is_error() {
        let payload = json!({ "name": "gw" });
        assert!(parse_pools(&payload).is_err());
    }

    #[test]
    fn malformed_nic_id_falls_back_to_unknown() {
        // No /networkInterfaces/ segment at all.
        let (nic, cfg) = parse_nic_id("/some/other/path");
        assert_eq!(nic, "(unknown)");
        assert_eq!(cfg, "(unknown)");
    }
}
