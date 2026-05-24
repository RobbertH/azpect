//! Azure-side modules: auth, REST client, resource discovery, metrics, logs, derived health.

pub mod apim;
pub mod appgw_backends;
pub mod auth;
pub mod az_login;
pub mod client;
pub mod container_app_limits;
pub mod container_app_revisions;
pub mod container_app_workspace;
pub mod cosmos;
pub mod health;
pub mod key_vault;
pub mod logs;
pub mod metrics;
pub mod registries;
pub mod resource_health;
pub mod resources;
pub mod service_bus;
pub mod storage;
pub mod subscriptions;
