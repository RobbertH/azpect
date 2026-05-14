//! Azure Resource Health: per-resource availability state from
//! `Microsoft.ResourceHealth/availabilityStatuses/current`. Independent of
//! metric traffic, so it correctly classifies idle but healthy resources.

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::azure::auth::AzureAuth;
use crate::azure::client::ArmClient;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum AvailabilityState {
    Available,
    Unavailable,
    Degraded,
    Unknown,
}

impl AvailabilityState {
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "available" => Self::Available,
            "unavailable" => Self::Unavailable,
            "degraded" => Self::Degraded,
            _ => Self::Unknown,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ResourceAvailability {
    pub state: AvailabilityState,
    /// Free-form human reason (e.g. `"PlatformInitiated"`). Sometimes empty.
    pub reason: Option<String>,
}

pub async fn fetch(auth: &AzureAuth, resource_id: &str) -> anyhow::Result<ResourceAvailability> {
    let client = ArmClient::new(auth.clone())?;
    let path =
        format!("{resource_id}/providers/Microsoft.ResourceHealth/availabilityStatuses/current");
    let resp = client
        .get(&path, &[("api-version", "2022-10-01")])
        .await
        .with_context(|| format!("availabilityStatuses for {resource_id}"))?;
    parse(&resp)
}

fn parse(value: &serde_json::Value) -> anyhow::Result<ResourceAvailability> {
    let props = value
        .get("properties")
        .ok_or_else(|| anyhow::anyhow!("availabilityStatuses response missing 'properties'"))?;
    let state = props
        .get("availabilityState")
        .and_then(|v| v.as_str())
        .map(AvailabilityState::parse)
        .unwrap_or(AvailabilityState::Unknown);
    let reason = props
        .get("reasonType")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    Ok(ResourceAvailability { state, reason })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_available_state() {
        let payload = json!({
            "properties": { "availabilityState": "Available", "reasonType": "" }
        });
        let r = parse(&payload).unwrap();
        assert_eq!(r.state, AvailabilityState::Available);
        assert!(r.reason.is_none());
    }

    #[test]
    fn parses_degraded_with_reason() {
        let payload = json!({
            "properties": {
                "availabilityState": "Degraded",
                "reasonType": "PlatformInitiated"
            }
        });
        let r = parse(&payload).unwrap();
        assert_eq!(r.state, AvailabilityState::Degraded);
        assert_eq!(r.reason.as_deref(), Some("PlatformInitiated"));
    }

    #[test]
    fn unknown_state_for_missing_field() {
        let payload = json!({ "properties": {} });
        let r = parse(&payload).unwrap();
        assert_eq!(r.state, AvailabilityState::Unknown);
    }
}
