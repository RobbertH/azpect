//! `GET https://management.azure.com/subscriptions?api-version=2022-12-01`.

#![allow(dead_code, unused_variables)]

use anyhow::anyhow;
use serde::{Deserialize, Serialize};

use crate::azure::auth::AzureAuth;
use crate::azure::client::ArmClient;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Subscription {
    /// Subscription GUID. The Azure REST `id` field is `/subscriptions/<guid>`;
    /// here we store just the guid for convenience.
    pub id: String,
    pub display_name: String,
    pub state: String,
    pub tenant_id: String,
}

/// List every subscription the credential can see. Sorted by display name.
pub async fn list(auth: &AzureAuth) -> anyhow::Result<Vec<Subscription>> {
    let client = ArmClient::new(auth.clone())?;
    let resp = client
        .get("/subscriptions", &[("api-version", "2022-12-01")])
        .await?;

    let value = resp
        .get("value")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("subscriptions response missing 'value' array"))?;

    let mut subs: Vec<Subscription> = value.iter().filter_map(parse_subscription).collect();

    subs.sort_by(|a, b| {
        a.display_name
            .to_lowercase()
            .cmp(&b.display_name.to_lowercase())
    });
    Ok(subs)
}

fn parse_subscription(v: &serde_json::Value) -> Option<Subscription> {
    let id = v.get("subscriptionId")?.as_str()?.to_string();
    let display_name = v
        .get("displayName")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let state = v
        .get("state")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let tenant_id = v
        .get("tenantId")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    Some(Subscription {
        id,
        display_name,
        state,
        tenant_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_and_sorts_subscriptions() {
        let payload = json!({
            "value": [
                {
                    "subscriptionId": "11111111-1111-1111-1111-111111111111",
                    "displayName": "Zebra",
                    "state": "Enabled",
                    "tenantId": "tttttttt-tttt-tttt-tttt-tttttttttttt",
                },
                {
                    "subscriptionId": "22222222-2222-2222-2222-222222222222",
                    "displayName": "alpha",
                    "state": "Enabled",
                    "tenantId": "tttttttt-tttt-tttt-tttt-tttttttttttt",
                },
            ]
        });

        let mut subs: Vec<Subscription> = payload["value"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(parse_subscription)
            .collect();
        subs.sort_by(|a, b| {
            a.display_name
                .to_lowercase()
                .cmp(&b.display_name.to_lowercase())
        });

        assert_eq!(subs.len(), 2);
        assert_eq!(subs[0].display_name, "alpha");
        assert_eq!(subs[1].display_name, "Zebra");
        assert_eq!(subs[0].id, "22222222-2222-2222-2222-222222222222");
    }
}
