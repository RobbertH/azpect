//! Read-only Azure Service Bus inspection.
//!
//! ## Contract (do not change without coordinating with the UI lane)
//!
//! Four public functions form the surface the UI consumes:
//!
//! - [`list_namespaces`] — Resource Graph KQL discovery of Service Bus
//!   namespaces across the supplied subscriptions.
//! - [`list_queues`] — ARM control-plane enumeration of queues under one
//!   namespace, including `countDetails` (active / dead-letter / scheduled /
//!   transfer message counts).
//! - [`list_topics`] — ARM control-plane enumeration of topics under one
//!   namespace, including the subscription count.
//! - [`list_subscriptions`] — ARM control-plane enumeration of the
//!   subscriptions on one topic, again with `countDetails`.
//!
//! ## Scope decisions worth flagging
//!
//! - **Control plane only**: everything here goes through [`ArmClient`] with the
//!   ARM scope, so plain `Reader` on the namespace is sufficient — including the
//!   dead-letter depths, which the ARM `countDetails` block surfaces without any
//!   data-plane (SAS / AAD-to-Service-Bus) auth. This makes Service Bus the
//!   simplest of the drill-in resource families auth-wise; contrast Cosmos /
//!   ACR / Key Vault, all of which need a second token.
//! - **Read-only**: namespace / queue / topic / subscription enumeration only.
//!   No send / receive / peek / purge codepaths, even stubs.
//! - **Paginated**: the ARM list endpoints page at 100 entities, so the queue /
//!   topic / subscription lists follow `nextLink` until exhausted, capped at
//!   [`MAX_PAGES`] pages with a `tracing::warn` when the cap is hit (mirrors
//!   the warn-and-stop precedent in `storage.rs` / `key_vault.rs`).

#![allow(dead_code, unused_variables)]

use anyhow::{anyhow, Context};
use chrono::{DateTime, Utc};

use crate::azure::auth::AzureAuth;
use crate::azure::client::{ArmClient, ARM_BASE};

/// API version for the queues / topics / subscriptions control-plane endpoints.
const SERVICE_BUS_API_VERSION: &str = "2021-11-01";

/// Cap on `nextLink` pages followed per list call. ARM pages at 100 entities,
/// so this bounds a single list at ~5000 rows — far beyond what the TUI can
/// usefully show, and it keeps a pathological namespace from stalling a view.
const MAX_PAGES: usize = 50;

/// One Service Bus namespace discovered via Resource Graph.
#[derive(Clone, Debug)]
pub struct ServiceBusNamespace {
    /// Full ARM resource id.
    pub id: String,
    pub name: String,
    pub resource_group: String,
    pub subscription_id: String,
    pub location: String,
    /// `sku.name` — `Basic` / `Standard` / `Premium`. Basic namespaces have no
    /// topics (the topics list comes back empty), which the entities view
    /// surfaces rather than erroring.
    pub sku: Option<String>,
    /// `properties.status` — usually `Active`.
    pub status: Option<String>,
    /// `properties.serviceBusEndpoint`, e.g. `https://ns.servicebus.windows.net:443/`.
    pub endpoint: Option<String>,
    /// `properties.createdAt` parsed to UTC. `None` when missing.
    pub created_at: Option<DateTime<Utc>>,
}

/// The two entity kinds a namespace holds. The entities view toggles between
/// them with Tab, mirroring the Key Vault secrets/certificates toggle.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
pub enum EntityKind {
    #[default]
    Queue,
    Topic,
}

impl EntityKind {
    pub fn label(self) -> &'static str {
        match self {
            EntityKind::Queue => "queues",
            EntityKind::Topic => "topics",
        }
    }
}

/// The `countDetails` block ARM returns for queues and subscriptions. Topics
/// expose it too but only the transfer fields are ever non-zero (a topic holds
/// no messages itself), so the entities view ignores it for topics.
#[derive(Clone, Copy, Debug, Default)]
pub struct CountDetails {
    pub active: i64,
    pub dead_letter: i64,
    pub scheduled: i64,
    pub transfer: i64,
    pub transfer_dead_letter: i64,
}

/// One queue inside a namespace.
#[derive(Clone, Debug)]
pub struct ServiceBusQueue {
    /// Full ARM resource id (`{namespace.id}/queues/{name}`).
    pub id: String,
    pub name: String,
    /// `properties.status` — `Active` / `Disabled` / `ReceiveDisabled` / …
    pub status: Option<String>,
    /// `properties.messageCount` — total messages (active + dead-letter +
    /// scheduled). `None` when ARM omits it.
    pub total_message_count: Option<i64>,
    /// `properties.countDetails`.
    pub counts: CountDetails,
    /// `properties.maxDeliveryCount` — deliveries before a message is
    /// dead-lettered.
    pub max_delivery_count: Option<i64>,
    /// `properties.sizeInBytes`.
    pub size_bytes: Option<i64>,
    /// `properties.requiresSession`.
    pub requires_session: Option<bool>,
    /// `properties.updatedAt` parsed to UTC.
    pub updated_at: Option<DateTime<Utc>>,
}

/// One topic inside a namespace.
#[derive(Clone, Debug)]
pub struct ServiceBusTopic {
    /// Full ARM resource id (`{namespace.id}/topics/{name}`).
    pub id: String,
    pub name: String,
    pub status: Option<String>,
    /// `properties.subscriptionCount`.
    pub subscription_count: Option<i64>,
    /// `properties.sizeInBytes`.
    pub size_bytes: Option<i64>,
    pub updated_at: Option<DateTime<Utc>>,
}

/// One subscription on a topic.
#[derive(Clone, Debug)]
pub struct ServiceBusSubscription {
    /// Full ARM resource id (`{topic.id}/subscriptions/{name}`).
    pub id: String,
    pub name: String,
    pub status: Option<String>,
    /// `properties.messageCount`.
    pub total_message_count: Option<i64>,
    /// `properties.countDetails`.
    pub counts: CountDetails,
    pub max_delivery_count: Option<i64>,
    pub requires_session: Option<bool>,
    /// `properties.forwardTo` — auto-forward target, when configured.
    pub forward_to: Option<String>,
    pub updated_at: Option<DateTime<Utc>>,
}

/// Resource Graph KQL for Service Bus namespaces. `sku` is a top-level field on
/// the Resource Graph row (sibling of `properties`), so it's projected
/// alongside.
const NAMESPACES_KQL: &str = r#"
Resources
| where type == 'microsoft.servicebus/namespaces'
| project id, name, type, location, resourceGroup, subscriptionId, sku, properties
| order by name asc
"#;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Enumerate Service Bus namespaces across `subscription_ids`. Empty slice →
/// all subscriptions visible to the credential.
pub async fn list_namespaces(
    auth: &AzureAuth,
    subscription_ids: &[String],
) -> anyhow::Result<Vec<ServiceBusNamespace>> {
    let client = ArmClient::new(auth.clone())?;

    let body = if subscription_ids.is_empty() {
        serde_json::json!({ "query": NAMESPACES_KQL })
    } else {
        serde_json::json!({
            "subscriptions": subscription_ids,
            "query": NAMESPACES_KQL,
        })
    };

    let resp = client
        .post(
            "/providers/Microsoft.ResourceGraph/resources?api-version=2022-10-01",
            &body,
        )
        .await
        .context("resource graph: list service bus namespaces")?;

    let rows = resp
        .get("data")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("resource graph response missing 'data' array"))?;

    if rows.len() >= 1000 {
        tracing::warn!(
            "resource graph returned {} service bus namespaces; pagination not implemented",
            rows.len()
        );
    }

    Ok(rows.iter().filter_map(parse_namespace).collect())
}

/// List queues inside `namespace` via the ARM control plane, following
/// `nextLink` until exhausted (or [`MAX_PAGES`]).
pub async fn list_queues(
    auth: &AzureAuth,
    namespace: &ServiceBusNamespace,
) -> anyhow::Result<Vec<ServiceBusQueue>> {
    let client = ArmClient::new(auth.clone())?;
    let path = format!("{}/queues", namespace.id);
    let pages = get_all_pages(&client, &path, "queues", &namespace.name)
        .await
        .with_context(|| format!("list service bus queues for {}", namespace.name))?;
    Ok(pages.iter().flat_map(parse_queues_json).collect())
}

/// List topics inside `namespace` via the ARM control plane, following
/// `nextLink` until exhausted (or [`MAX_PAGES`]).
pub async fn list_topics(
    auth: &AzureAuth,
    namespace: &ServiceBusNamespace,
) -> anyhow::Result<Vec<ServiceBusTopic>> {
    let client = ArmClient::new(auth.clone())?;
    let path = format!("{}/topics", namespace.id);
    let pages = get_all_pages(&client, &path, "topics", &namespace.name)
        .await
        .with_context(|| format!("list service bus topics for {}", namespace.name))?;
    Ok(pages.iter().flat_map(parse_topics_json).collect())
}

/// List subscriptions on `topic_name` inside `namespace`, following `nextLink`
/// until exhausted (or [`MAX_PAGES`]). `topic_name` is used verbatim — topic
/// names may legitimately contain `/`, which ARM accepts as literal path
/// characters here, exactly as they appear in the topic's resource id.
pub async fn list_subscriptions(
    auth: &AzureAuth,
    namespace: &ServiceBusNamespace,
    topic_name: &str,
) -> anyhow::Result<Vec<ServiceBusSubscription>> {
    let client = ArmClient::new(auth.clone())?;
    let path = format!("{}/topics/{}/subscriptions", namespace.id, topic_name);
    let pages = get_all_pages(&client, &path, "subscriptions", &namespace.name)
        .await
        .with_context(|| {
            format!(
                "list service bus subscriptions for {}/{}",
                namespace.name, topic_name
            )
        })?;
    Ok(pages.iter().flat_map(parse_subscriptions_json).collect())
}

/// GET `first_path` and every `nextLink` page after it, returning the raw page
/// envelopes (each with its own `value` array). Stops with a `tracing::warn`
/// at [`MAX_PAGES`] so one huge namespace can't stall the view forever.
async fn get_all_pages(
    client: &ArmClient,
    first_path: &str,
    what: &str,
    namespace_name: &str,
) -> anyhow::Result<Vec<serde_json::Value>> {
    let mut pages = Vec::new();
    let mut resp = client
        .get(first_path, &[("api-version", SERVICE_BUS_API_VERSION)])
        .await?;
    loop {
        let next = next_link_path(&resp);
        pages.push(resp);
        if pages.len() >= MAX_PAGES && next.is_some() {
            tracing::warn!(
                "service bus {what} for {namespace_name}: stopping after {MAX_PAGES} pages; \
                 more entities exist beyond the cap"
            );
            break;
        }
        match next {
            // nextLink embeds the api-version (and skip token) in its query
            // string, so no extra query params on follow-up requests.
            Some(path) => resp = client.get(&path, &[]).await?,
            None => break,
        }
    }
    Ok(pages)
}

/// Extract a page's `nextLink` as an [`ArmClient`]-relative path (the client
/// prepends `ARM_BASE`). A `nextLink` pointing at a foreign host would be
/// re-rooted onto ARM_BASE at best and is not followed; ARM never does this
/// in practice, so warn-and-stop is the safe reading.
fn next_link_path(resp: &serde_json::Value) -> Option<String> {
    let link = resp
        .get("nextLink")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?;
    match link.strip_prefix(ARM_BASE) {
        Some(path) => Some(path.to_string()),
        None => {
            tracing::warn!("ignoring nextLink not rooted at {ARM_BASE}: {link}");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Parsers
// ---------------------------------------------------------------------------

pub(crate) fn parse_namespace(v: &serde_json::Value) -> Option<ServiceBusNamespace> {
    let ty = v.get("type")?.as_str()?.to_lowercase();
    if ty != "microsoft.servicebus/namespaces" {
        return None;
    }
    let id = v.get("id")?.as_str()?.to_string();
    let name = string_field(v, "name");
    let resource_group = string_field(v, "resourceGroup");
    let subscription_id = string_field(v, "subscriptionId");
    let location = string_field(v, "location");

    let sku = v
        .get("sku")
        .and_then(|s| s.get("name"))
        .and_then(|n| n.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let props = v.get("properties");
    let status = opt_string(props.and_then(|p| p.get("status")));
    let endpoint = opt_string(props.and_then(|p| p.get("serviceBusEndpoint")));
    let created_at = parse_optional_rfc3339(props.and_then(|p| p.get("createdAt")));

    Some(ServiceBusNamespace {
        id,
        name,
        resource_group,
        subscription_id,
        location,
        sku,
        status,
        endpoint,
        created_at,
    })
}

pub(crate) fn parse_queues_json(v: &serde_json::Value) -> Vec<ServiceBusQueue> {
    array_value(v).iter().filter_map(parse_queue_row).collect()
}

fn parse_queue_row(row: &serde_json::Value) -> Option<ServiceBusQueue> {
    let id = row.get("id")?.as_str()?.to_string();
    let name = entity_name(row, "/queues/")?;
    let props = row.get("properties");
    Some(ServiceBusQueue {
        id,
        name,
        status: opt_string(props.and_then(|p| p.get("status"))),
        total_message_count: opt_i64(props.and_then(|p| p.get("messageCount"))),
        counts: parse_count_details(props.and_then(|p| p.get("countDetails"))),
        max_delivery_count: opt_i64(props.and_then(|p| p.get("maxDeliveryCount"))),
        size_bytes: opt_i64(props.and_then(|p| p.get("sizeInBytes"))),
        requires_session: props
            .and_then(|p| p.get("requiresSession"))
            .and_then(|b| b.as_bool()),
        updated_at: parse_optional_rfc3339(props.and_then(|p| p.get("updatedAt"))),
    })
}

pub(crate) fn parse_topics_json(v: &serde_json::Value) -> Vec<ServiceBusTopic> {
    array_value(v).iter().filter_map(parse_topic_row).collect()
}

fn parse_topic_row(row: &serde_json::Value) -> Option<ServiceBusTopic> {
    let id = row.get("id")?.as_str()?.to_string();
    let name = entity_name(row, "/topics/")?;
    let props = row.get("properties");
    Some(ServiceBusTopic {
        id,
        name,
        status: opt_string(props.and_then(|p| p.get("status"))),
        subscription_count: opt_i64(props.and_then(|p| p.get("subscriptionCount"))),
        size_bytes: opt_i64(props.and_then(|p| p.get("sizeInBytes"))),
        updated_at: parse_optional_rfc3339(props.and_then(|p| p.get("updatedAt"))),
    })
}

pub(crate) fn parse_subscriptions_json(v: &serde_json::Value) -> Vec<ServiceBusSubscription> {
    array_value(v)
        .iter()
        .filter_map(parse_subscription_row)
        .collect()
}

fn parse_subscription_row(row: &serde_json::Value) -> Option<ServiceBusSubscription> {
    let id = row.get("id")?.as_str()?.to_string();
    let name = subscription_name(row)?;
    let props = row.get("properties");
    Some(ServiceBusSubscription {
        id,
        name,
        status: opt_string(props.and_then(|p| p.get("status"))),
        total_message_count: opt_i64(props.and_then(|p| p.get("messageCount"))),
        counts: parse_count_details(props.and_then(|p| p.get("countDetails"))),
        max_delivery_count: opt_i64(props.and_then(|p| p.get("maxDeliveryCount"))),
        requires_session: props
            .and_then(|p| p.get("requiresSession"))
            .and_then(|b| b.as_bool()),
        forward_to: opt_string(props.and_then(|p| p.get("forwardTo"))),
        updated_at: parse_optional_rfc3339(props.and_then(|p| p.get("updatedAt"))),
    })
}

fn parse_count_details(v: Option<&serde_json::Value>) -> CountDetails {
    let Some(c) = v else {
        return CountDetails::default();
    };
    CountDetails {
        active: opt_i64(c.get("activeMessageCount")).unwrap_or(0),
        dead_letter: opt_i64(c.get("deadLetterMessageCount")).unwrap_or(0),
        scheduled: opt_i64(c.get("scheduledMessageCount")).unwrap_or(0),
        transfer: opt_i64(c.get("transferMessageCount")).unwrap_or(0),
        transfer_dead_letter: opt_i64(c.get("transferDeadLetterMessageCount")).unwrap_or(0),
    }
}

// ---------------------------------------------------------------------------
// Small JSON helpers
// ---------------------------------------------------------------------------

fn array_value(v: &serde_json::Value) -> &[serde_json::Value] {
    v.get("value")
        .and_then(|a| a.as_array())
        .map(|a| a.as_slice())
        .unwrap_or(&[])
}

/// Queue/topic name from the row's `name` field, **verbatim** — queue and
/// topic names legitimately contain `/` (e.g. `orders/v2`), so stripping to
/// the last path segment would mangle them (and break the subscriptions URL
/// built from a topic's name). Falls back to the id: everything after the
/// entity-kind marker (`/queues/` or `/topics/`), again keeping any embedded
/// slashes intact.
fn entity_name(row: &serde_json::Value, id_marker: &str) -> Option<String> {
    if let Some(raw) = row
        .get("name")
        .and_then(|n| n.as_str())
        .filter(|s| !s.is_empty())
    {
        return Some(raw.to_string());
    }
    let id = row.get("id")?.as_str()?;
    // rfind: resource ids start with `/subscriptions/{subId}/…`, so the
    // *last* occurrence of the marker is the one that precedes the name.
    let start = id.rfind(id_marker)? + id_marker.len();
    let rest = &id[start..];
    if rest.is_empty() {
        None
    } else {
        Some(rest.to_string())
    }
}

/// Subscription name from the row. ARM returns subscription names as
/// `topic/sub` in the `name` field; subscription names themselves cannot
/// contain `/` (only their parent topic's name can), so the trailing segment
/// is always the bare subscription name.
fn subscription_name(row: &serde_json::Value) -> Option<String> {
    if let Some(raw) = row
        .get("name")
        .and_then(|n| n.as_str())
        .filter(|s| !s.is_empty())
    {
        return Some(raw.rsplit('/').next().unwrap_or(raw).to_string());
    }
    entity_name(row, "/subscriptions/")
}

fn string_field(v: &serde_json::Value, key: &str) -> String {
    v.get(key)
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .to_string()
}

fn opt_string(v: Option<&serde_json::Value>) -> Option<String> {
    v.and_then(|n| n.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// Pull an integer out of a JSON field that may be a number or a string. ARM
/// is mostly consistent about returning numbers for the count fields, but
/// tolerating the string form costs nothing and guards against surprises.
fn opt_i64(v: Option<&serde_json::Value>) -> Option<i64> {
    let v = v?;
    if let Some(n) = v.as_i64() {
        return Some(n);
    }
    if let Some(f) = v.as_f64() {
        return Some(f as i64);
    }
    v.as_str().and_then(|s| s.trim().parse::<i64>().ok())
}

fn parse_optional_rfc3339(v: Option<&serde_json::Value>) -> Option<DateTime<Utc>> {
    let s = v?.as_str()?;
    if s.is_empty() {
        return None;
    }
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_namespace_row() {
        let row = json!({
            "id": "/subscriptions/s/resourceGroups/rg/providers/Microsoft.ServiceBus/namespaces/ns",
            "name": "ns",
            "type": "microsoft.servicebus/namespaces",
            "location": "westeurope",
            "resourceGroup": "rg",
            "subscriptionId": "s",
            "sku": { "name": "Standard", "tier": "Standard" },
            "properties": {
                "status": "Active",
                "serviceBusEndpoint": "https://ns.servicebus.windows.net:443/",
                "createdAt": "2026-01-02T03:04:05.000Z"
            }
        });
        let ns = parse_namespace(&row).expect("expected namespace");
        assert_eq!(ns.name, "ns");
        assert_eq!(ns.sku.as_deref(), Some("Standard"));
        assert_eq!(ns.status.as_deref(), Some("Active"));
        assert!(ns.endpoint.as_deref().unwrap().contains("servicebus"));
        assert!(ns.created_at.is_some());
    }

    #[test]
    fn skips_non_namespace_rows() {
        let row = json!({
            "id": "/subs/x/rg/y/providers/Microsoft.Web/sites/z",
            "name": "z",
            "type": "microsoft.web/sites",
            "location": "westeurope",
            "resourceGroup": "y",
            "subscriptionId": "x"
        });
        assert!(parse_namespace(&row).is_none());
    }

    #[test]
    fn parses_queue_with_count_details() {
        let body = json!({
            "value": [{
                "id": "/subs/s/rg/r/providers/Microsoft.ServiceBus/namespaces/ns/queues/orders",
                "name": "orders",
                "properties": {
                    "status": "Active",
                    "messageCount": 42,
                    "maxDeliveryCount": 10,
                    "sizeInBytes": 2048,
                    "requiresSession": false,
                    "updatedAt": "2026-02-03T00:00:00Z",
                    "countDetails": {
                        "activeMessageCount": 40,
                        "deadLetterMessageCount": 2,
                        "scheduledMessageCount": 0,
                        "transferMessageCount": 0,
                        "transferDeadLetterMessageCount": 0
                    }
                }
            }]
        });
        let queues = parse_queues_json(&body);
        assert_eq!(queues.len(), 1);
        let q = &queues[0];
        assert_eq!(q.name, "orders");
        assert_eq!(q.status.as_deref(), Some("Active"));
        assert_eq!(q.total_message_count, Some(42));
        assert_eq!(q.counts.active, 40);
        assert_eq!(q.counts.dead_letter, 2);
        assert_eq!(q.max_delivery_count, Some(10));
        assert_eq!(q.requires_session, Some(false));
    }

    #[test]
    fn queue_missing_count_details_defaults_to_zero() {
        let body = json!({
            "value": [{
                "id": "/x/queues/q",
                "name": "q",
                "properties": { "status": "Active" }
            }]
        });
        let queues = parse_queues_json(&body);
        assert_eq!(queues.len(), 1);
        assert_eq!(queues[0].counts.dead_letter, 0);
        assert_eq!(queues[0].counts.active, 0);
        assert_eq!(queues[0].total_message_count, None);
    }

    #[test]
    fn parses_topic_with_subscription_count() {
        let body = json!({
            "value": [{
                "id": "/x/topics/events",
                "name": "events",
                "properties": {
                    "status": "Active",
                    "subscriptionCount": 3,
                    "sizeInBytes": 1024,
                    "updatedAt": "2026-02-03T00:00:00Z"
                }
            }]
        });
        let topics = parse_topics_json(&body);
        assert_eq!(topics.len(), 1);
        assert_eq!(topics[0].name, "events");
        assert_eq!(topics[0].subscription_count, Some(3));
    }

    #[test]
    fn parses_subscription_with_forward_and_counts() {
        let body = json!({
            "value": [{
                "id": "/x/topics/events/subscriptions/audit",
                "name": "events/audit",
                "properties": {
                    "status": "Active",
                    "messageCount": 7,
                    "maxDeliveryCount": 5,
                    "requiresSession": true,
                    "forwardTo": "sink-queue",
                    "countDetails": {
                        "activeMessageCount": 5,
                        "deadLetterMessageCount": 2
                    }
                }
            }]
        });
        let subs = parse_subscriptions_json(&body);
        assert_eq!(subs.len(), 1);
        let s = &subs[0];
        // name comes back as `topic/subscription`; we keep only the bare name.
        assert_eq!(s.name, "audit");
        assert_eq!(s.total_message_count, Some(7));
        assert_eq!(s.counts.dead_letter, 2);
        assert_eq!(s.max_delivery_count, Some(5));
        assert_eq!(s.requires_session, Some(true));
        assert_eq!(s.forward_to.as_deref(), Some("sink-queue"));
    }

    #[test]
    fn queue_and_topic_names_with_slashes_stay_verbatim() {
        // Queue/topic names legitimately contain `/`; truncating to the last
        // segment displayed the wrong name and broke drill-in URLs.
        let body = json!({
            "value": [{
                "id": "/subs/s/rg/r/providers/Microsoft.ServiceBus/namespaces/ns/queues/orders/v2",
                "name": "orders/v2",
                "properties": { "status": "Active" }
            }]
        });
        let queues = parse_queues_json(&body);
        assert_eq!(queues[0].name, "orders/v2");

        let body = json!({
            "value": [{
                "id": "/subs/s/rg/r/providers/Microsoft.ServiceBus/namespaces/ns/topics/orders/v2",
                "name": "orders/v2",
                "properties": { "status": "Active" }
            }]
        });
        let topics = parse_topics_json(&body);
        assert_eq!(topics[0].name, "orders/v2");
    }

    #[test]
    fn entity_name_falls_back_to_id_after_kind_marker() {
        // Missing `name` field: derive from the id, keeping embedded slashes.
        let row = json!({
            "id": "/subscriptions/s/resourceGroups/r/providers/Microsoft.ServiceBus/namespaces/ns/queues/orders/v2"
        });
        assert_eq!(entity_name(&row, "/queues/").as_deref(), Some("orders/v2"));
        assert_eq!(entity_name(&row, "/topics/"), None);
    }

    #[test]
    fn subscription_under_slash_topic_keeps_bare_sub_name() {
        // Topic `orders/v2`, subscription `audit`: ARM's name field is the
        // whole `topic/sub` path; only the trailing segment is the sub name.
        let body = json!({
            "value": [{
                "id": "/subs/s/rg/r/providers/Microsoft.ServiceBus/namespaces/ns/topics/orders/v2/subscriptions/audit",
                "name": "orders/v2/audit",
                "properties": { "status": "Active" }
            }]
        });
        let subs = parse_subscriptions_json(&body);
        assert_eq!(subs[0].name, "audit");
    }

    #[test]
    fn next_link_path_strips_arm_base_and_rejects_foreign_hosts() {
        let resp = json!({
            "value": [],
            "nextLink": "https://management.azure.com/x/queues?api-version=2021-11-01&$skipToken=y"
        });
        assert_eq!(
            next_link_path(&resp).as_deref(),
            Some("/x/queues?api-version=2021-11-01&$skipToken=y")
        );
        assert_eq!(next_link_path(&json!({ "value": [] })), None);
        assert_eq!(next_link_path(&json!({ "nextLink": "" })), None);
        let foreign = json!({ "nextLink": "https://evil.example.com/x" });
        assert_eq!(next_link_path(&foreign), None);
    }

    #[test]
    fn opt_i64_handles_number_and_string() {
        assert_eq!(opt_i64(Some(&json!(5))), Some(5));
        assert_eq!(opt_i64(Some(&json!("12"))), Some(12));
        assert_eq!(opt_i64(Some(&json!(3.0))), Some(3));
        assert_eq!(opt_i64(Some(&json!("nan"))), None);
        assert_eq!(opt_i64(None), None);
    }

    #[test]
    fn empty_value_array_yields_no_rows() {
        let body = json!({ "value": [] });
        assert!(parse_queues_json(&body).is_empty());
        assert!(parse_topics_json(&body).is_empty());
        assert!(parse_subscriptions_json(&body).is_empty());
        // Missing `value` entirely is also tolerated.
        let empty = json!({});
        assert!(parse_queues_json(&empty).is_empty());
    }
}
