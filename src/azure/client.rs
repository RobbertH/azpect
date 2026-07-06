//! Shared HTTP plumbing for ARM, Resource Graph, Monitor, and Log Analytics
//! calls. Owns the `reqwest::Client`, retry policy, and bearer-token attachment.
//!
//! ## Contract
//!
//! The other modules in `azure::` should go through [`ArmClient`] / [`LogsClient`]
//! rather than holding their own `reqwest::Client`. This keeps connection pools
//! shared, retry policy uniform, and tracing redaction in one place.

#![allow(dead_code, unused_variables)]

use std::time::Duration;

use anyhow::anyhow;
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE, RETRY_AFTER};
use reqwest::{Method, Response, StatusCode};

use crate::azure::auth::{AzureAuth, SCOPE_ARM, SCOPE_GRAPH, SCOPE_LOGS, SCOPE_STORAGE};

/// `x-ms-version` header value for blob data-plane requests. Pinned to a stable
/// release of the REST API; bump in lockstep across all storage calls so server
/// behavior stays consistent.
pub const STORAGE_API_VERSION: &str = "2021-12-02";

/// Default user agent on every outbound request.
pub const USER_AGENT: &str = concat!("azpect/", env!("CARGO_PKG_VERSION"));

/// Base URL for ARM, Resource Graph, and Monitor metrics.
pub const ARM_BASE: &str = "https://management.azure.com";

/// Base URL for Log Analytics resource-centric queries.
pub const LOGS_BASE: &str = "https://api.loganalytics.io";

/// Base URL for Microsoft Graph v1.0.
pub const GRAPH_BASE: &str = "https://graph.microsoft.com/v1.0";

/// Backoff schedule for retries on 429/5xx (in addition to any `Retry-After`).
const BACKOFF_MS: &[u64] = &[250, 500, 1_000, 2_000];

/// Upper bound honoured for a server-supplied `Retry-After`. Azure throttling
/// windows can be long (86400s has been seen in the wild) and the header is
/// attacker/typo-shaped input; sleeping that long parks the fetch task for the
/// rest of the session, and an unbounded value overflows the millisecond
/// conversion. Better to retry sooner and eat another 429.
const MAX_RETRY_AFTER_SECS: u64 = 60;

/// Connect timeout for every outbound request. reqwest sets no default, so a
/// black-holed endpoint (e.g. an unroutable IPv6 address — the same failure
/// mode `auth.rs` bounds for token acquisition) would otherwise hang a fetch
/// forever.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Overall per-request deadline for the JSON control-plane clients (ARM,
/// Resource Graph, Log Analytics, Microsoft Graph). Generous enough for a slow
/// KQL query; small enough that a stalled response surfaces as a retryable
/// error instead of wedging the UI.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Overall per-request deadline for blob data-plane transfers. Larger than
/// [`REQUEST_TIMEOUT`]: enumeration pages and ranged blob previews are bounded
/// in size but can be slow on a thin link.
const STORAGE_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Maximum number of bytes from an error response body included in the error message.
const ERROR_BODY_EXCERPT: usize = 4096;

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

fn build_http() -> anyhow::Result<reqwest::Client> {
    build_http_with_timeout(REQUEST_TIMEOUT)
}

fn build_http_with_timeout(timeout: Duration) -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(timeout)
        .build()
        .map_err(|e| anyhow!("failed to build reqwest client: {e}"))
}

/// Decide whether a status code is worth retrying.
fn should_retry(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// Pull a `Retry-After: <seconds>` header (we only honour the integer-seconds
/// form; the HTTP-date form is rare in Azure responses and not worth the deps).
/// Clamped to [`MAX_RETRY_AFTER_SECS`] so a huge server value can't park the
/// task for hours or overflow the millisecond math at the call sites.
fn retry_after_secs(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|secs| secs.min(MAX_RETRY_AFTER_SECS))
}

/// Read up to `ERROR_BODY_EXCERPT` chars of the body to attach to error messages.
async fn body_excerpt(resp: Response) -> String {
    match resp.text().await {
        Ok(text) => {
            if text.len() > ERROR_BODY_EXCERPT {
                let mut end = ERROR_BODY_EXCERPT;
                while !text.is_char_boundary(end) && end > 0 {
                    end -= 1;
                }
                format!("{}…", &text[..end])
            } else {
                text
            }
        }
        Err(_) => String::new(),
    }
}

/// Execute a request with bearer attach + retry logic. Returns parsed JSON on 2xx.
///
/// `build` is invoked on every attempt so that we can attach a freshly-resolved
/// bearer token (and avoid replaying a stale request body builder).
async fn send_with_retry<F>(
    http: &reqwest::Client,
    auth: &AzureAuth,
    scope: &str,
    mut build: F,
) -> anyhow::Result<serde_json::Value>
where
    F: FnMut(&reqwest::Client) -> reqwest::RequestBuilder,
{
    let mut last_err: Option<anyhow::Error> = None;
    let mut iter = BACKOFF_MS.iter().copied();
    let mut next_backoff: Option<u64> = Some(0); // first attempt has no preceding wait

    while let Some(prelude_ms) = next_backoff {
        if prelude_ms > 0 {
            tokio::time::sleep(Duration::from_millis(prelude_ms)).await;
        }
        // Pre-compute whether we'll have another retry available after this attempt.
        let upcoming = iter.next();
        next_backoff = upcoming;

        let token = auth
            .token(scope)
            .await
            .map_err(|e| anyhow!("token acquisition for {scope} failed: {e}"))?;

        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        let bearer = format!("Bearer {token}");
        let auth_value = HeaderValue::from_str(&bearer)
            .map_err(|_| anyhow!("bearer token contained invalid header characters"))?;
        headers.insert(AUTHORIZATION, auth_value);

        let req = build(http).headers(headers);
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                // Network error — retry while we still have budget.
                last_err = Some(anyhow!("network error: {e}"));
                if next_backoff.is_some() {
                    continue;
                } else {
                    break;
                }
            }
        };

        let status = resp.status();
        if status.is_success() {
            // 204 No Content has empty body; return Null in that case.
            let bytes = resp.bytes().await.map_err(|e| anyhow!("read body: {e}"))?;
            if bytes.is_empty() {
                return Ok(serde_json::Value::Null);
            }
            return serde_json::from_slice(&bytes).map_err(|e| anyhow!("parse json: {e}"));
        }

        if should_retry(status) {
            if let Some(base) = next_backoff {
                let extra = retry_after_secs(resp.headers()).unwrap_or(0);
                // Drain the body so we don't keep the connection in an odd state.
                let _ = resp.bytes().await;
                next_backoff = Some(base + extra * 1_000);
                last_err = Some(anyhow!("retryable status {status}"));
                continue;
            }
        }

        // Terminal error (4xx other than 429, or out of retry budget).
        let body = body_excerpt(resp).await;
        return Err(anyhow!("azure api error {}: {}", status.as_u16(), body));
    }

    Err(last_err.unwrap_or_else(|| anyhow!("request failed after retries")))
}

impl ArmClient {
    pub fn new(auth: AzureAuth) -> anyhow::Result<Self> {
        Ok(Self {
            auth,
            http: build_http()?,
        })
    }

    /// `GET https://management.azure.com{path}` with bearer for ARM scope.
    /// Caller passes `path` starting with `/`. Handles 429 + 5xx with bounded backoff.
    pub async fn get(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> anyhow::Result<serde_json::Value> {
        let url = format!("{ARM_BASE}{path}");
        let query_owned: Vec<(String, String)> = query
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        send_with_retry(&self.http, &self.auth, SCOPE_ARM, |http| {
            http.request(Method::GET, &url).query(&query_owned)
        })
        .await
    }

    /// `GET {url}` where `url` is a full ARM URL. Used to follow `nextLink`
    /// pagination continuations, which arrive as absolute URLs with the
    /// continuation token already encoded in the query string.
    pub async fn get_url(&self, url: &str) -> anyhow::Result<serde_json::Value> {
        let url_owned = url.to_string();
        send_with_retry(&self.http, &self.auth, SCOPE_ARM, |http| {
            http.request(Method::GET, &url_owned)
        })
        .await
    }

    pub async fn post(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let url = format!("{ARM_BASE}{path}");
        send_with_retry(&self.http, &self.auth, SCOPE_ARM, |http| {
            http.request(Method::POST, &url)
                .header(CONTENT_TYPE, "application/json")
                .json(body)
        })
        .await
    }

    /// `PUT https://management.azure.com{path}` with `body` as the JSON payload.
    /// Used for full-replace writes (e.g. a Function App's `config/appsettings`).
    /// Same retry/redaction discipline as the read path.
    pub async fn put(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let url = format!("{ARM_BASE}{path}");
        send_with_retry(&self.http, &self.auth, SCOPE_ARM, |http| {
            http.request(Method::PUT, &url)
                .header(CONTENT_TYPE, "application/json")
                .json(body)
        })
        .await
    }

    /// `PATCH https://management.azure.com{path}` with `body` as the JSON
    /// payload. Used for partial-update writes (e.g. a Container App resource,
    /// where we send only `{properties:{template}}` to avoid replaying read-only
    /// fields; ARM applies it as a merge and spins a new revision).
    pub async fn patch(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let url = format!("{ARM_BASE}{path}");
        send_with_retry(&self.http, &self.auth, SCOPE_ARM, |http| {
            http.request(Method::PATCH, &url)
                .header(CONTENT_TYPE, "application/json")
                .json(body)
        })
        .await
    }
}

/// Client for Microsoft Graph (`graph.microsoft.com`). Same retry/redaction
/// discipline as [`ArmClient`], but uses `SCOPE_GRAPH`. Used best-effort to
/// resolve directory object-ids to display names; callers tolerate 4xx.
#[derive(Clone)]
pub struct GraphClient {
    pub(crate) auth: AzureAuth,
    pub(crate) http: reqwest::Client,
}

impl GraphClient {
    pub fn new(auth: AzureAuth) -> anyhow::Result<Self> {
        Ok(Self {
            auth,
            http: build_http()?,
        })
    }

    /// `GET https://graph.microsoft.com/v1.0{path}` with a Graph bearer. Caller
    /// passes `path` starting with `/` (it may include a `?$select=` query).
    pub async fn get(&self, path: &str) -> anyhow::Result<serde_json::Value> {
        let url = format!("{GRAPH_BASE}{path}");
        send_with_retry(&self.http, &self.auth, SCOPE_GRAPH, |http| {
            http.request(Method::GET, &url)
        })
        .await
    }
}

/// Outcome of a successful blob data-plane request.
///
/// Returned by [`send_bytes_with_retry`]. The `headers` map is always present
/// (HEAD responses use it exclusively); `body` is empty for HEAD.
pub struct BytesResponse {
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

/// Variant of [`send_with_retry`] for non-JSON responses (XML enumeration,
/// raw blob bytes, HEAD metadata). Same retry/backoff/redaction discipline,
/// but returns headers + bytes verbatim instead of parsing JSON.
///
/// `extra_headers` is invoked on every attempt so callers can attach
/// per-request headers (e.g. `x-ms-version`, `Range`, `Accept`) without
/// re-allocating in the success path.
async fn send_bytes_with_retry<F, H>(
    http: &reqwest::Client,
    auth: &AzureAuth,
    scope: &str,
    mut build: F,
    mut extra_headers: H,
) -> anyhow::Result<BytesResponse>
where
    F: FnMut(&reqwest::Client) -> reqwest::RequestBuilder,
    H: FnMut(&mut HeaderMap),
{
    let mut last_err: Option<anyhow::Error> = None;
    let mut iter = BACKOFF_MS.iter().copied();
    let mut next_backoff: Option<u64> = Some(0);

    while let Some(prelude_ms) = next_backoff {
        if prelude_ms > 0 {
            tokio::time::sleep(Duration::from_millis(prelude_ms)).await;
        }
        let upcoming = iter.next();
        next_backoff = upcoming;

        let token = auth
            .token(scope)
            .await
            .map_err(|e| anyhow!("token acquisition for {scope} failed: {e}"))?;

        let mut headers = HeaderMap::new();
        let bearer = format!("Bearer {token}");
        let auth_value = HeaderValue::from_str(&bearer)
            .map_err(|_| anyhow!("bearer token contained invalid header characters"))?;
        headers.insert(AUTHORIZATION, auth_value);
        extra_headers(&mut headers);

        let req = build(http).headers(headers);
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                last_err = Some(anyhow!("network error: {e}"));
                if next_backoff.is_some() {
                    continue;
                } else {
                    break;
                }
            }
        };

        let status = resp.status();
        if status.is_success() {
            let headers = resp.headers().clone();
            let bytes = resp.bytes().await.map_err(|e| anyhow!("read body: {e}"))?;
            return Ok(BytesResponse {
                headers,
                body: bytes.to_vec(),
            });
        }

        if should_retry(status) {
            if let Some(base) = next_backoff {
                let extra = retry_after_secs(resp.headers()).unwrap_or(0);
                let _ = resp.bytes().await;
                next_backoff = Some(base + extra * 1_000);
                last_err = Some(anyhow!("retryable status {status}"));
                continue;
            }
        }

        // Terminal error.
        let status_code = status.as_u16();
        let body = body_excerpt(resp).await;
        return Err(anyhow!("azure api error {}: {}", status_code, body));
    }

    Err(last_err.unwrap_or_else(|| anyhow!("request failed after retries")))
}

impl LogsClient {
    pub fn new(auth: AzureAuth) -> anyhow::Result<Self> {
        Ok(Self {
            auth,
            http: build_http()?,
        })
    }

    pub async fn query(
        &self,
        resource_id: &str,
        kql: &str,
        timespan: &str,
    ) -> anyhow::Result<serde_json::Value> {
        // Resource IDs always start with `/`.
        let url = format!("{LOGS_BASE}/v1{resource_id}/query");
        let body = serde_json::json!({
            "query": kql,
            "timespan": timespan,
        });
        send_with_retry(&self.http, &self.auth, SCOPE_LOGS, |http| {
            http.request(Method::POST, &url)
                .header(CONTENT_TYPE, "application/json")
                .json(&body)
        })
        .await
    }

    /// Workspace-centric query. Required for Container Apps, where logs are
    /// forwarded by the parent Container Apps Environment (not by per-resource
    /// diagnostic settings), so the resource-centric path resolves to an empty
    /// scope and every union returns SEM0529.
    pub async fn query_workspace(
        &self,
        customer_id: &str,
        kql: &str,
        timespan: &str,
    ) -> anyhow::Result<serde_json::Value> {
        let url = format!("{LOGS_BASE}/v1/workspaces/{customer_id}/query");
        let body = serde_json::json!({
            "query": kql,
            "timespan": timespan,
        });
        send_with_retry(&self.http, &self.auth, SCOPE_LOGS, |http| {
            http.request(Method::POST, &url)
                .header(CONTENT_TYPE, "application/json")
                .json(&body)
        })
        .await
    }
}

/// Client for blob data-plane calls (`*.blob.core.windows.net`). Unlike
/// [`ArmClient`], the host changes per account so callers pass full URLs.
/// Uses `SCOPE_STORAGE` and attaches the `x-ms-version` header automatically.
#[derive(Clone)]
pub struct StorageClient {
    pub(crate) auth: AzureAuth,
    pub(crate) http: reqwest::Client,
}

impl StorageClient {
    pub fn new(auth: AzureAuth) -> anyhow::Result<Self> {
        Ok(Self {
            auth,
            // Data-plane transfers get the longer deadline — see
            // [`STORAGE_REQUEST_TIMEOUT`].
            http: build_http_with_timeout(STORAGE_REQUEST_TIMEOUT)?,
        })
    }

    /// `GET {url}` returning the raw response body as a UTF-8 string. Intended
    /// for the XML container/blob enumeration endpoints. Errors if the body
    /// is not valid UTF-8 (Azure always sends ASCII XML for these endpoints).
    pub async fn get_xml(&self, url: &str) -> anyhow::Result<String> {
        let url_owned = url.to_string();
        let resp = send_bytes_with_retry(
            &self.http,
            &self.auth,
            SCOPE_STORAGE,
            |http| http.request(Method::GET, &url_owned),
            |headers| {
                headers.insert(ACCEPT, HeaderValue::from_static("application/xml"));
                headers.insert(
                    "x-ms-version",
                    HeaderValue::from_static(STORAGE_API_VERSION),
                );
            },
        )
        .await?;
        String::from_utf8(resp.body)
            .map_err(|e| anyhow!("storage response was not valid utf-8: {e}"))
    }

    /// `HEAD {url}` returning only response headers (blob metadata lookup).
    pub async fn head(&self, url: &str) -> anyhow::Result<HeaderMap> {
        let url_owned = url.to_string();
        let resp = send_bytes_with_retry(
            &self.http,
            &self.auth,
            SCOPE_STORAGE,
            |http| http.request(Method::HEAD, &url_owned),
            |headers| {
                headers.insert(
                    "x-ms-version",
                    HeaderValue::from_static(STORAGE_API_VERSION),
                );
            },
        )
        .await?;
        Ok(resp.headers)
    }

    /// `GET {url}` with a `Range:` header set, returning the raw bytes. Caller
    /// is responsible for forming a valid HTTP range expression (e.g.
    /// `"bytes=0-1023"`). Note the whole request shares
    /// [`STORAGE_REQUEST_TIMEOUT`]; callers requesting very large ranges
    /// should size them accordingly.
    pub async fn get_bytes_range(&self, url: &str, range: &str) -> anyhow::Result<Vec<u8>> {
        let url_owned = url.to_string();
        let range_owned = range.to_string();
        let resp = send_bytes_with_retry(
            &self.http,
            &self.auth,
            SCOPE_STORAGE,
            |http| http.request(Method::GET, &url_owned),
            |headers| {
                headers.insert(
                    "x-ms-version",
                    HeaderValue::from_static(STORAGE_API_VERSION),
                );
                if let Ok(v) = HeaderValue::from_str(&range_owned) {
                    headers.insert(reqwest::header::RANGE, v);
                }
            },
        )
        .await?;
        Ok(resp.body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers_with_retry_after(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_str(value).unwrap());
        headers
    }

    #[test]
    fn retry_after_parses_integer_seconds() {
        assert_eq!(retry_after_secs(&headers_with_retry_after("5")), Some(5));
        assert_eq!(
            retry_after_secs(&headers_with_retry_after(" 12 ")),
            Some(12)
        );
    }

    #[test]
    fn retry_after_clamps_huge_values() {
        // A day-long throttle window must not park the fetch task for a day,
        // and u64::MAX must not overflow the *1000 conversion at the call site.
        assert_eq!(
            retry_after_secs(&headers_with_retry_after("86400")),
            Some(MAX_RETRY_AFTER_SECS)
        );
        let max = u64::MAX.to_string();
        assert_eq!(
            retry_after_secs(&headers_with_retry_after(&max)),
            Some(MAX_RETRY_AFTER_SECS)
        );
    }

    #[test]
    fn retry_after_ignores_http_date_form() {
        // We only honour integer seconds; the HTTP-date form parses as None
        // and the caller falls back to the plain backoff schedule.
        let h = headers_with_retry_after("Wed, 21 Oct 2026 07:28:00 GMT");
        assert_eq!(retry_after_secs(&h), None);
        assert_eq!(retry_after_secs(&HeaderMap::new()), None);
    }
}
