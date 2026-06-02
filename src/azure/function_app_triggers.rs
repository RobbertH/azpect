//! Fetch a Function App's functions and summarize each one's trigger from its
//! binding metadata, via the ARM functions list (`Microsoft.Web/sites/functions`).
//!
//! `GET {id}/functions?api-version=2023-12-01` returns every function with its
//! `properties.config.bindings[]`. A function's *trigger* is the binding whose
//! `type` ends in `Trigger` (e.g. `httpTrigger`, `queueTrigger`, `kafkaTrigger`,
//! `serviceBusTrigger`, `eventHubTrigger`, `timerTrigger`, `blobTrigger`,
//! `cosmosDBTrigger`). We surface a short kind plus the one or two parameters
//! that identify *what* it listens to (queue/topic name, CRON schedule, …).
//!
//! ## Permissions
//! Listing functions and reading their `config.bindings` needs only
//! `Microsoft.Web/sites/read`-level access — no `.../listkeys` action. The
//! bindings carry app-setting *names* (e.g. `%BrokerList%`, `AzureWebJobsStorage`)
//! rather than secret values, so this is safe to run with `Reader`, matching
//! the rest of azpect's read-only posture.
//!
//! ## Caveat: not-yet-synced apps
//! Apps deployed with `WEBSITE_RUN_FROM_PACKAGE` or cold Consumption-plan apps
//! may not have their function metadata synced to ARM yet, in which case `value`
//! comes back empty. The caller renders that as "no triggers" rather than an
//! error — same decorative, best-effort policy as the other Function App fetches.

#![allow(dead_code, unused_variables)]

use anyhow::Context;

use crate::azure::auth::AzureAuth;
use crate::azure::client::ArmClient;

/// Microsoft.Web API version exposing the `functions` collection.
const API_VERSION: &str = "2023-12-01";

/// One function's trigger, distilled to what the Detail overview shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FunctionTrigger {
    /// The function's name (e.g. `ProcessOrders`, `TimerCleanup`).
    pub function: String,
    /// Short, lower-case trigger kind: `http`, `queue`, `servicebus`,
    /// `eventhub`, `kafka`, `timer`, `blob`, `cosmosdb`, `eventgrid`, … For an
    /// unrecognized `fooTrigger` the `Trigger` suffix is stripped (→ `foo`).
    /// Empty only for the pathological case of a function with no trigger
    /// binding at all.
    pub kind: String,
    /// One or two identifying parameters for the trigger, pre-formatted for
    /// display: the queue/topic name, CRON schedule, blob path, HTTP methods,
    /// etc. `None` when the binding carries nothing useful to show. Values may
    /// be app-setting references like `%TopicName%` — shown verbatim (they're
    /// names, not secrets).
    pub detail: Option<String>,
}

/// GET the Function App's functions and summarize each one's trigger.
pub async fn fetch(
    auth: &AzureAuth,
    function_app_id: &str,
) -> anyhow::Result<Vec<FunctionTrigger>> {
    let client = ArmClient::new(auth.clone())?;
    let path = format!("{function_app_id}/functions");
    let resp = client
        .get(&path, &[("api-version", API_VERSION)])
        .await
        .with_context(|| format!("listing functions for {function_app_id}"))?;
    Ok(extract(&resp))
}

/// Pull a [`FunctionTrigger`] out of each entry in the `functions` list
/// response (`{ "value": [ { "properties": { "config": { "bindings": [...] }}}]}`).
/// Functions without a recognizable trigger binding are still listed (with an
/// empty `kind`) so the count reflects reality. Sorted by function name for
/// stable rendering.
pub fn extract(resp: &serde_json::Value) -> Vec<FunctionTrigger> {
    let Some(arr) = resp.get("value").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut out: Vec<FunctionTrigger> = arr.iter().map(extract_one).collect();
    out.sort_by(|a, b| a.function.cmp(&b.function));
    out
}

/// Summarize a single function object into a [`FunctionTrigger`].
fn extract_one(func: &serde_json::Value) -> FunctionTrigger {
    let function = function_name(func);
    let bindings = func
        .pointer("/properties/config/bindings")
        .and_then(|v| v.as_array())
        .map(|a| a.as_slice())
        .unwrap_or(&[]);
    // The trigger is the binding whose type ends in `Trigger`. There is exactly
    // one per function; take the first match if Azure ever emits more.
    let trigger = bindings.iter().find(|b| is_trigger_binding(b));
    match trigger {
        Some(b) => {
            let (kind, detail) = summarize_binding(b);
            FunctionTrigger {
                function,
                kind,
                detail,
            }
        }
        None => FunctionTrigger {
            function,
            kind: String::new(),
            detail: None,
        },
    }
}

/// A function's display name: prefer `properties.name`, fall back to the last
/// path segment of the ARM `name` (`<site>/<func>`) or `id`.
fn function_name(func: &serde_json::Value) -> String {
    if let Some(n) = func
        .pointer("/properties/name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        return n.to_string();
    }
    for ptr in ["/name", "/id"] {
        if let Some(s) = func.pointer(ptr).and_then(|v| v.as_str()) {
            if let Some(tail) = s.rsplit('/').next().filter(|t| !t.is_empty()) {
                return tail.to_string();
            }
        }
    }
    String::new()
}

/// True when a binding describes a trigger (its `type` ends in `trigger`,
/// case-insensitively, e.g. `queueTrigger`).
fn is_trigger_binding(b: &serde_json::Value) -> bool {
    b.get("type")
        .and_then(|v| v.as_str())
        .map(|t| t.to_ascii_lowercase().ends_with("trigger"))
        .unwrap_or(false)
}

/// Map a trigger binding to a `(kind, detail)` pair. `kind` is a short
/// lower-case label; `detail` is the one/two identifying parameters worth
/// showing. Unknown trigger types degrade to their type with the `Trigger`
/// suffix stripped and no detail, so new Azure extensions still render sanely.
fn summarize_binding(b: &serde_json::Value) -> (String, Option<String>) {
    let raw = b.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let ty = raw.to_ascii_lowercase();
    let s = |key: &str| str_field(b, key);
    match ty.as_str() {
        "httptrigger" => {
            // Identify by the allowed methods, else the auth level.
            let detail = methods_field(b).or_else(|| s("authLevel").map(|a| format!("auth {a}")));
            ("http".to_string(), detail)
        }
        "timertrigger" => ("timer".to_string(), s("schedule")),
        "queuetrigger" => ("queue".to_string(), s("queueName")),
        "servicebustrigger" => {
            // Topic+subscription if present, otherwise a queue.
            let detail = match (s("topicName"), s("subscriptionName")) {
                (Some(t), Some(sub)) => Some(format!("{t}/{sub}")),
                (Some(t), None) => Some(t),
                _ => s("queueName"),
            };
            ("servicebus".to_string(), detail)
        }
        "eventhubtrigger" => (
            "eventhub".to_string(),
            with_consumer_group(b, s("eventHubName")),
        ),
        "kafkatrigger" => ("kafka".to_string(), with_consumer_group(b, s("topic"))),
        "blobtrigger" => ("blob".to_string(), s("path")),
        "cosmosdbtrigger" => {
            // Newer bindings use containerName/databaseName; older ones
            // collectionName/databaseName. Show `db/container` when both known.
            let container = s("containerName").or_else(|| s("collectionName"));
            let detail = match (s("databaseName"), container) {
                (Some(db), Some(c)) => Some(format!("{db}/{c}")),
                (None, Some(c)) => Some(c),
                (Some(db), None) => Some(db),
                _ => None,
            };
            ("cosmosdb".to_string(), detail)
        }
        "eventgridtrigger" => ("eventgrid".to_string(), None),
        "rabbitmqtrigger" => ("rabbitmq".to_string(), s("queueName")),
        "redisstreamtrigger"
        | "redispubsubtrigger"
        | "rediskeyspacetrigger"
        | "redislisttrigger" => ("redis".to_string(), s("key").or_else(|| s("channel"))),
        "servicebustopictrigger" => ("servicebus".to_string(), s("topicName")),
        // Unknown: strip the trailing `trigger` and show no detail.
        _ => {
            let kind = ty.strip_suffix("trigger").unwrap_or(&ty);
            let kind = if kind.is_empty() {
                ty.clone()
            } else {
                kind.to_string()
            };
            (kind, None)
        }
    }
}

/// A non-empty string binding field, trimmed; `None` when absent/blank.
fn str_field(b: &serde_json::Value, key: &str) -> Option<String> {
    b.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// Join an HTTP binding's `methods` array into `GET, POST` (upper-cased).
fn methods_field(b: &serde_json::Value) -> Option<String> {
    let arr = b.get("methods").and_then(|v| v.as_array())?;
    let joined = arr
        .iter()
        .filter_map(|m| m.as_str())
        .map(|m| m.to_ascii_uppercase())
        .collect::<Vec<_>>()
        .join(", ");
    (!joined.is_empty()).then_some(joined)
}

/// Append a non-default consumer group to an Event Hub / Kafka detail string,
/// e.g. `my-hub (group: workers)`. `$Default` is implied and omitted.
fn with_consumer_group(b: &serde_json::Value, base: Option<String>) -> Option<String> {
    let group = str_field(b, "consumerGroup").filter(|g| !g.eq_ignore_ascii_case("$Default"));
    match (base, group) {
        (Some(base), Some(g)) => Some(format!("{base} (group: {g})")),
        (base, _) => base,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn summarizes_a_queue_trigger() {
        let resp = json!({
            "value": [{
                "name": "app/ProcessOrders",
                "properties": {
                    "name": "ProcessOrders",
                    "config": { "bindings": [
                        { "name": "msg", "type": "queueTrigger", "direction": "in",
                          "queueName": "orders", "connection": "AzureWebJobsStorage" }
                    ]}
                }
            }]
        });
        let t = extract(&resp);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].function, "ProcessOrders");
        assert_eq!(t[0].kind, "queue");
        assert_eq!(t[0].detail.as_deref(), Some("orders"));
    }

    #[test]
    fn summarizes_kafka_trigger_with_consumer_group() {
        let resp = json!({
            "value": [{
                "properties": {
                    "name": "Ingest",
                    "config": { "bindings": [
                        { "type": "kafkaTrigger", "direction": "in",
                          "topic": "events", "consumerGroup": "workers",
                          "brokerList": "%BrokerList%" }
                    ]}
                }
            }]
        });
        let t = extract(&resp);
        assert_eq!(t[0].kind, "kafka");
        assert_eq!(t[0].detail.as_deref(), Some("events (group: workers)"));
    }

    #[test]
    fn kafka_default_consumer_group_is_omitted() {
        let resp = json!({
            "value": [{
                "properties": { "name": "Ingest", "config": { "bindings": [
                    { "type": "kafkaTrigger", "topic": "events", "consumerGroup": "$Default" }
                ]}}
            }]
        });
        let t = extract(&resp);
        assert_eq!(t[0].detail.as_deref(), Some("events"));
    }

    #[test]
    fn summarizes_timer_and_http() {
        let resp = json!({
            "value": [
                { "properties": { "name": "Cleanup", "config": { "bindings": [
                    { "type": "timerTrigger", "schedule": "0 0 * * * *" }
                ]}}},
                { "properties": { "name": "Api", "config": { "bindings": [
                    { "type": "httpTrigger", "authLevel": "function",
                      "methods": ["get", "post"], "direction": "in" }
                ]}}}
            ]
        });
        let t = extract(&resp);
        // Sorted by name: Api, Cleanup.
        assert_eq!(t[0].function, "Api");
        assert_eq!(t[0].kind, "http");
        assert_eq!(t[0].detail.as_deref(), Some("GET, POST"));
        assert_eq!(t[1].function, "Cleanup");
        assert_eq!(t[1].kind, "timer");
        assert_eq!(t[1].detail.as_deref(), Some("0 0 * * * *"));
    }

    #[test]
    fn service_bus_topic_and_subscription() {
        let resp = json!({
            "value": [{ "properties": { "name": "Sb", "config": { "bindings": [
                { "type": "serviceBusTrigger", "topicName": "orders", "subscriptionName": "billing" }
            ]}}}]
        });
        let t = extract(&resp);
        assert_eq!(t[0].kind, "servicebus");
        assert_eq!(t[0].detail.as_deref(), Some("orders/billing"));
    }

    #[test]
    fn cosmosdb_database_and_container() {
        let resp = json!({
            "value": [{ "properties": { "name": "Feed", "config": { "bindings": [
                { "type": "cosmosDBTrigger", "databaseName": "shop", "containerName": "orders" }
            ]}}}]
        });
        let t = extract(&resp);
        assert_eq!(t[0].kind, "cosmosdb");
        assert_eq!(t[0].detail.as_deref(), Some("shop/orders"));
    }

    #[test]
    fn unknown_trigger_strips_suffix_with_no_detail() {
        let resp = json!({
            "value": [{ "properties": { "name": "X", "config": { "bindings": [
                { "type": "someFutureTrigger", "wat": "nope" }
            ]}}}]
        });
        let t = extract(&resp);
        assert_eq!(t[0].kind, "somefuture");
        assert!(t[0].detail.is_none());
    }

    #[test]
    fn name_falls_back_to_arm_name_tail() {
        let resp = json!({
            "value": [{ "name": "mysite/Worker", "config": { "bindings": [] },
                       "properties": { "config": { "bindings": [
                           { "type": "queueTrigger", "queueName": "q" }
                       ]}}}]
        });
        let t = extract(&resp);
        assert_eq!(t[0].function, "Worker");
    }

    #[test]
    fn function_without_trigger_binding_is_listed_with_empty_kind() {
        let resp = json!({
            "value": [{ "properties": { "name": "OnlyOutputs", "config": { "bindings": [
                { "type": "queue", "direction": "out", "queueName": "out" }
            ]}}}]
        });
        let t = extract(&resp);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].function, "OnlyOutputs");
        assert!(t[0].kind.is_empty());
    }

    #[test]
    fn empty_or_missing_value_is_empty() {
        assert!(extract(&json!({ "value": [] })).is_empty());
        assert!(extract(&json!({})).is_empty());
    }
}
