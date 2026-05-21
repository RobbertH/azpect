//! Per-screen rendering. Each view module exposes a `render(frame, area, state, theme)`
//! function and may expose a `handle(action, state)` helper for view-local input.

pub mod apim_apis;
pub mod apim_operations;
pub mod apim_policy;
pub mod appgw_backends;
pub mod detail;
pub mod help;
pub mod list;
pub mod logs;
pub mod logs_detail;
pub mod registries;
pub mod registry_repositories;
pub mod registry_tags;
pub mod storage_account_overview;
pub mod storage_accounts;
pub mod storage_blob_detail;
pub mod storage_blobs;
pub mod storage_containers;
pub mod subscriptions;
