//! `GET https://management.azure.com/subscriptions?api-version=2022-12-01`.

#![allow(dead_code, unused_variables)]

use serde::{Deserialize, Serialize};

use crate::azure::auth::AzureAuth;

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
    todo!("Lane 2: ArmClient::get(\"/subscriptions\", &[(\"api-version\", \"2022-12-01\")])")
}
