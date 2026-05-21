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

use crate::azure::auth::{AzureAuth, SCOPE_ARM, SCOPE_LOGS, SCOPE_STORAGE};

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

/// Backoff schedule for retries on 429/5xx (in addition to any `Retry-After`).
const BACKOFF_MS: &[u64] = &[250, 500, 1_000, 2_000];

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
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| anyhow!("failed to build reqwest client: {e}"))
}

/// Decide whether a status code is worth retrying.
fn should_retry(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// Pull a `Retry-After: <seconds>` header (we only honour the integer-seconds
/// form; the HTTP-date form is rare in Azure responses and not worth the deps).
fn retry_after_secs(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
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
            http: build_http()?,
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
    /// `"bytes=0-1023"`).
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
