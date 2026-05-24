//! `DefaultAzureCredential` wrapper with per-scope token caching.
//!
//! ## Contract (do not change without coordinating with all lanes)
//!
//! - `AzureAuth::new()` resolves a credential chain. In `azure_identity` 0.27
//!   that chain is the Azure CLI (`az`) followed by the Azure Developer CLI
//!   (`azd`) — there is no managed-identity / IMDS probe link. It must succeed
//!   if *any* link produces a usable credential.
//! - `AzureAuth::token(scope)` returns a fresh bearer (no `Bearer ` prefix) for
//!   the requested OAuth scope. The result is cached per scope and refreshed
//!   when within `REFRESH_BEFORE_EXPIRY` of expiry. The underlying credential
//!   shells out to `az account get-access-token`, which has no timeout of its
//!   own — so this method bounds it with [`TOKEN_ACQUISITION_TIMEOUT`]. A hung
//!   `az` then surfaces as an error (and the in-app login modal) instead of an
//!   indefinite hang on the subscriptions screen.
//! - Tokens MUST NOT appear in tracing output at any level. Use `Debug` impls
//!   carefully.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use anyhow::{anyhow, Context};
use azure_core::credentials::TokenCredential;
use azure_identity::DefaultAzureCredential;
use chrono::{DateTime, TimeZone, Utc};
use tokio::sync::RwLock;

/// OAuth scope for ARM, Resource Graph, and Monitor metrics.
pub const SCOPE_ARM: &str = "https://management.azure.com/.default";

/// OAuth scope for Log Analytics queries (`api.loganalytics.io`).
pub const SCOPE_LOGS: &str = "https://api.loganalytics.io/.default";

/// OAuth scope for the Azure Storage **data plane** (`*.blob.core.windows.net`,
/// etc.). The control-plane operations on storage accounts go through ARM
/// (`SCOPE_ARM`); enumerating containers' contents or fetching blob bytes
/// requires this dedicated audience and the caller needs the
/// `Storage Blob Data Reader` role (or stronger) on the account.
pub const SCOPE_STORAGE: &str = "https://storage.azure.com/.default";

/// OAuth scope for the Azure Cosmos DB **data plane** (`*.documents.azure.com`).
/// Account control-plane operations (list databases, list containers, fetch
/// container properties) go through ARM (`SCOPE_ARM`); reading items via
/// `POST /dbs/{db}/colls/{coll}/docs` requires this dedicated audience plus the
/// `Cosmos DB Built-in Data Reader` role assigned at the account scope via
/// `dataPlaneRoleDefinitions` — control-plane `Reader` is not enough.
pub const SCOPE_COSMOS: &str = "https://cosmos.azure.com/.default";

/// OAuth scope for the Azure Key Vault **data plane** (`*.vault.azure.net`).
/// The vault list comes from Resource Graph / ARM (`SCOPE_ARM`); enumerating
/// secrets / keys / certificates inside a vault requires this dedicated
/// audience. The signed-in identity needs either a `list`-permitting access
/// policy or an RBAC role like `Key Vault Reader` (control-plane `Reader` is
/// not enough). Note the audience host has no trailing path — Key Vault is
/// one of the older services that uses a host-form audience.
pub const SCOPE_KEY_VAULT: &str = "https://vault.azure.net/.default";

/// Refresh tokens this far before their stated expiry.
pub const REFRESH_BEFORE_EXPIRY: std::time::Duration = std::time::Duration::from_secs(60);

/// Upper bound on a single token acquisition. The Azure CLI credential shells
/// out to `az account get-access-token`, which can hang indefinitely (e.g. a
/// stale session that needs `az login`, or a blocked AAD round-trip — a common
/// case is broken IPv6, where the AAD endpoint resolves to an unroutable v6
/// address and the connect never completes) rather than erroring promptly.
/// Without this bound the app sits forever on "loading subscriptions…"; with
/// it, a hung `az` becomes a clean error that drives the in-app login modal. A
/// healthy `az` returns in well under a second, so 10s is ample headroom for a
/// slow-but-working CLI cold start while still failing fast.
pub const TOKEN_ACQUISITION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// A token with its absolute expiry. Cached per-scope inside [`AzureAuth`].
#[derive(Clone)]
pub struct CachedToken {
    pub token: String,
    pub expires_at: DateTime<Utc>,
}

impl fmt::Debug for CachedToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CachedToken")
            .field("token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// The credential wrapper. Cheap to clone (interior `Arc`).
#[derive(Clone)]
pub struct AzureAuth {
    credential: Arc<DefaultAzureCredential>,
    cache: Arc<RwLock<HashMap<String, CachedToken>>>,
}

impl fmt::Debug for AzureAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AzureAuth")
            .field("credential", &"<DefaultAzureCredential>")
            .field("cache", &"<redacted>")
            .finish()
    }
}

impl AzureAuth {
    /// Construct the credential chain. Surfaces a single error that lists which
    /// chain links were attempted, so the user can diagnose `az login`-vs-env
    /// confusion quickly.
    pub async fn new() -> anyhow::Result<Self> {
        let credential = DefaultAzureCredential::new().map_err(|e| {
            anyhow!(
                "failed to initialize Azure credential chain (env, workload identity, \
                 managed identity, Azure CLI, Azure Developer CLI): {e}. \
                 Try running `az login` or setting AZURE_* environment variables."
            )
        })?;
        Ok(Self {
            credential,
            cache: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// Drop every cached token. Call after a re-auth (e.g. `az login`) so the
    /// next request acquires a fresh token reflecting the new identity/tenant
    /// instead of returning the previous user's still-valid bearer.
    pub async fn clear_cache(&self) {
        self.cache.write().await.clear();
    }

    /// Acquire (and cache) a bearer token for `scope`.
    pub async fn token(&self, scope: &str) -> anyhow::Result<String> {
        let now = Utc::now();
        let refresh_before = chrono::Duration::from_std(REFRESH_BEFORE_EXPIRY)
            .unwrap_or_else(|_| chrono::Duration::seconds(60));

        // Fast path: read lock + cache hit + not within the refresh window.
        {
            let cache = self.cache.read().await;
            if let Some(entry) = cache.get(scope) {
                if entry.expires_at - refresh_before > now {
                    return Ok(entry.token.clone());
                }
            }
        }

        // Slow path: request a new token. Do this without holding any lock so
        // concurrent requests for *different* scopes aren't serialized. Bound
        // the call so a hung `az account get-access-token` can't wedge the UI
        // indefinitely — see [`TOKEN_ACQUISITION_TIMEOUT`].
        let access_token = match tokio::time::timeout(
            TOKEN_ACQUISITION_TIMEOUT,
            self.credential.get_token(&[scope], None),
        )
        .await
        {
            Ok(result) => result.context(
                "failed to acquire Azure access token; try `az login` or check your environment",
            )?,
            Err(_elapsed) => {
                return Err(anyhow!(
                    "timed out after {}s acquiring an Azure access token. The Azure CLI \
                         (`az account get-access-token`) did not respond — it may be hanging or \
                         need re-authentication. Try `az login` (or `az account clear` then \
                         `az login`).",
                    TOKEN_ACQUISITION_TIMEOUT.as_secs()
                ));
            }
        };

        let token_string = access_token.token.secret().to_string();
        let expires_at = offset_datetime_to_chrono(access_token.expires_on);

        let mut cache = self.cache.write().await;
        // Re-check: another task may have populated the cache while we awaited
        // the network round-trip. Prefer the freshest entry.
        if let Some(existing) = cache.get(scope) {
            if existing.expires_at >= expires_at
                && existing.expires_at - refresh_before > Utc::now()
            {
                return Ok(existing.token.clone());
            }
        }
        cache.insert(
            scope.to_string(),
            CachedToken {
                token: token_string.clone(),
                expires_at,
            },
        );
        Ok(token_string)
    }
}

/// Convert a `time::OffsetDateTime` (re-exported by `azure_core`) into a chrono
/// UTC datetime without pulling the `time` crate into our public API surface.
fn offset_datetime_to_chrono(odt: azure_core::time::OffsetDateTime) -> DateTime<Utc> {
    let unix = odt.unix_timestamp();
    let nanos = odt.nanosecond();
    Utc.timestamp_opt(unix, nanos)
        .single()
        .unwrap_or_else(Utc::now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_token_debug_redacts_secret() {
        let t = CachedToken {
            token: "supersecret-bearer-value".to_string(),
            expires_at: Utc::now(),
        };
        let rendered = format!("{t:?}");
        assert!(rendered.contains("<redacted>"));
        assert!(!rendered.contains("supersecret"));
    }

    #[test]
    fn azure_auth_debug_redacts_cache() {
        // Construction of `DefaultAzureCredential` does not probe credentials,
        // so this should succeed in any environment. If it ever doesn't we just
        // skip — token redaction in `Debug` is the property under test, and
        // `cached_token_debug_redacts_secret` covers the per-entry Debug.
        let Ok(credential) = DefaultAzureCredential::new() else {
            return;
        };
        let auth = AzureAuth {
            credential,
            cache: Arc::new(RwLock::new(HashMap::from([(
                SCOPE_ARM.to_string(),
                CachedToken {
                    token: "leak-me-if-you-can".to_string(),
                    expires_at: Utc::now(),
                },
            )]))),
        };
        let rendered = format!("{auth:?}");
        assert!(!rendered.contains("leak-me-if-you-can"));
    }

    #[test]
    fn scope_constants_are_stable() {
        assert_eq!(SCOPE_ARM, "https://management.azure.com/.default");
        assert_eq!(SCOPE_LOGS, "https://api.loganalytics.io/.default");
        assert_eq!(SCOPE_STORAGE, "https://storage.azure.com/.default");
        assert_eq!(SCOPE_COSMOS, "https://cosmos.azure.com/.default");
        assert_eq!(SCOPE_KEY_VAULT, "https://vault.azure.net/.default");
    }
}
