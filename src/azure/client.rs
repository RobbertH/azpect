//! Shared HTTP plumbing for ARM, Resource Graph, Monitor, and Log Analytics
//! calls. Owns the `reqwest::Client`, retry policy, and bearer-token attachment.
//!
//! ## Contract
//!
//! The other modules in `azure::` should go through [`ArmClient`] / [`LogsClient`]
//! rather than holding their own `reqwest::Client`. This keeps connection pools
//! shared, retry policy uniform, and tracing redaction in one place.

#![allow(dead_code, unused_variables)]

use crate::azure::auth::AzureAuth;

/// Default user agent on every outbound request.
pub const USER_AGENT: &str = concat!("azpect/", env!("CARGO_PKG_VERSION"));

/// Base URL for ARM, Resource Graph, and Monitor metrics.
pub const ARM_BASE: &str = "https://management.azure.com";

/// Base URL for Log Analytics resource-centric queries.
pub const LOGS_BASE: &str = "https://api.loganalytics.io";

#[derive(Clone)]
pub struct ArmClient {
    pub(crate) auth: AzureAuth,
    pub(crate) http: reqwest::Client,
}

#[derive(Clone)]
pub struct LogsClient {
    pub(crate) auth: AzureAuth,
    pub(crate) http: reqwest::Client,
}

impl ArmClient {
    pub fn new(auth: AzureAuth) -> anyhow::Result<Self> {
        todo!("Lane 2: reqwest::Client::builder().user_agent(USER_AGENT).build()")
    }

    /// `GET https://management.azure.com{path}` with bearer for ARM scope.
    /// Caller passes `path` starting with `/`. Handles 429 + 5xx with bounded backoff.
    pub async fn get(&self, path: &str, query: &[(&str, &str)]) -> anyhow::Result<serde_json::Value> {
        todo!("Lane 2")
    }

    pub async fn post(&self, path: &str, body: &serde_json::Value) -> anyhow::Result<serde_json::Value> {
        todo!("Lane 2")
    }
}

impl LogsClient {
    pub fn new(auth: AzureAuth) -> anyhow::Result<Self> {
        todo!("Lane 2")
    }

    pub async fn query(&self, resource_id: &str, kql: &str, timespan: &str) -> anyhow::Result<serde_json::Value> {
        todo!("Lane 2: POST {LOGS_BASE}/v1{resource_id}/query with body {{query, timespan}}")
    }
}
