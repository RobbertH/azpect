//! `DefaultAzureCredential` wrapper with per-scope token caching.
//!
//! ## Contract (do not change without coordinating with all lanes)
//!
//! - `AzureAuth::new()` resolves a credential chain (env → workload identity →
//!   managed identity → Azure CLI → Azure PowerShell → azd). It must succeed if
//!   *any* link in the chain produces a usable credential; the failure error
//!   should explain which links were tried.
//! - `AzureAuth::token(scope)` returns a fresh bearer (no `Bearer ` prefix) for
//!   the requested OAuth scope. The result is cached per scope and refreshed
//!   when within `REFRESH_BEFORE_EXPIRY` of expiry.
//! - Tokens MUST NOT appear in tracing output at any level. Use `Debug` impls
//!   carefully.

#![allow(dead_code, unused_variables)]

/// OAuth scope for ARM, Resource Graph, and Monitor metrics.
pub const SCOPE_ARM: &str = "https://management.azure.com/.default";

/// OAuth scope for Log Analytics queries (`api.loganalytics.io`).
pub const SCOPE_LOGS: &str = "https://api.loganalytics.io/.default";

/// Refresh tokens this far before their stated expiry.
pub const REFRESH_BEFORE_EXPIRY: std::time::Duration = std::time::Duration::from_secs(60);

/// A token with its absolute expiry. Cached per-scope inside [`AzureAuth`].
#[derive(Clone)]
pub struct CachedToken {
    pub token: String,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

/// The credential wrapper. Cheap to clone (interior `Arc`).
#[derive(Clone)]
pub struct AzureAuth {
    // Lane 1 fills this in. Suggested: Arc<azure_identity::DefaultAzureCredential>
    // and Arc<RwLock<HashMap<String, CachedToken>>>.
    _private: (),
}

impl AzureAuth {
    /// Construct the credential chain. Surfaces a single error that lists which
    /// chain links were attempted, so the user can diagnose `az login`-vs-env
    /// confusion quickly.
    pub async fn new() -> anyhow::Result<Self> {
        todo!("Lane 1: build DefaultAzureCredential and seed empty cache")
    }

    /// Acquire (and cache) a bearer token for `scope`.
    pub async fn token(&self, scope: &str) -> anyhow::Result<String> {
        todo!("Lane 1: lookup cache, request via credential if missing/expiring, return token string")
    }
}
