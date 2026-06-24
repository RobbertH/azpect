//! The built-in mock tenant behind `azpect --demo`.
//!
//! Everything here is fabricated: the fictional **Contoso** company, fake
//! subscription GUIDs, invented resource names, and synthesized metrics/logs.
//! Demo mode exists so screenshots and demos never expose a real tenant
//! (subscription ids, resource-group names, hostnames, traffic patterns).
//!
//! ## Contract
//!
//! - Pure data: no I/O, no awaits, no randomness (deterministic pseudo-noise
//!   keyed on bucket index), so the same build renders the same screenshots.
//! - Return types are exactly what the corresponding `crate::azure::*` fetch
//!   functions produce — the `spawn_load_*` functions in `ui::app` feed them
//!   through the same `AppEvent`s, so every view renders unchanged.
//! - Referential integrity is tested below: every resource belongs to a demo
//!   subscription, APIM children chain off the demo service id, and so on.

#![allow(dead_code)]

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};

use crate::azure::apim::{Api, Operation};
use crate::azure::appgw_backends::{BackendAddress, BackendPool, NicIpConfigRef};
use crate::azure::container_app_overview::{ContainerAppOverview, ContainerSpec};
use crate::azure::container_app_replicas::{ReplicaContainer, ReplicaInstance};
use crate::azure::container_app_revisions::{ActiveRevisionMeta, RevisionInfo};
use crate::azure::cosmos::{CosmosAccount, CosmosContainer, CosmosDatabase, CosmosItemPreview};
use crate::azure::env_vars::EnvVar;
use crate::azure::function_app_config::WebConfig;
use crate::azure::function_app_triggers::FunctionTrigger;
use crate::azure::key_vault::{ItemKind, KeyVault, KeyVaultItem};
use crate::azure::logs::{LogLevel, LogLine, LogsPage};
use crate::azure::metrics::{MetricKind, MetricPoint, MetricSeries, MetricsResult, TimeRange};
use crate::azure::registries::{Registry, Repository, Tag};
use crate::azure::resource_health::{AvailabilityState, ResourceAvailability};
use crate::azure::resources::{Resource, ResourceKind, ResourceMeta};
use crate::azure::service_bus::{
    CountDetails, ServiceBusNamespace, ServiceBusQueue, ServiceBusSubscription, ServiceBusTopic,
};
use crate::azure::sql::{SqlKind, SqlResource};
use crate::azure::storage::{
    Blob, BlobContainer, BlobMetadata, BlobPreview, BlobPreviewBody, StorageAccount,
    StorageAccountStats,
};
use crate::azure::subscriptions::Subscription;

/// Fabricated tenant + subscription GUIDs. Versioned-looking but invented.
pub const TENANT_ID: &str = "c7e9a2d4-1f3b-4e8c-9a5d-6b2f8c4e1a73";
pub const SUB_PROD: &str = "7f3e9c1d-2b4a-4d58-9a6e-5c1f8b2d7e90";
pub const SUB_STAGING: &str = "a1b8d3f2-6c5e-4b7a-8d29-3e7f1a9c4b56";

const LOCATION: &str = "westeurope";
const CREATED_BY: &str = "toon.miet@contoso.com";

/// True when `filter` is empty (= "all subscriptions") or contains `sub_id`.
fn in_subs(sub_id: &str, filter: &[String]) -> bool {
    filter.is_empty() || filter.iter().any(|s| s == sub_id)
}

/// Deterministic pseudo-noise in `[0, 1)` from a bucket index — a Weyl-style
/// integer hash so charts look organic without pulling in a PRNG dependency
/// (and without `Date::now`-style nondeterminism between frames).
fn noise(seed: u64, i: usize) -> f64 {
    let x = (i as u64)
        .wrapping_add(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15))
        .wrapping_mul(0xBF58_476D_1CE4_E5B9);
    let x = (x ^ (x >> 31)).wrapping_mul(0x94D0_49BB_1331_11EB);
    ((x ^ (x >> 33)) % 10_000) as f64 / 10_000.0
}

// ---------------------------------------------------------------------------
// Subscriptions + resources
// ---------------------------------------------------------------------------

pub fn subscriptions() -> Vec<Subscription> {
    vec![
        Subscription {
            id: SUB_PROD.to_string(),
            display_name: "Contoso Production".to_string(),
            state: "Enabled".to_string(),
            tenant_id: TENANT_ID.to_string(),
        },
        Subscription {
            id: SUB_STAGING.to_string(),
            display_name: "Contoso Staging".to_string(),
            state: "Enabled".to_string(),
            tenant_id: TENANT_ID.to_string(),
        },
    ]
}

fn resource_id(sub: &str, rg: &str, provider: &str, name: &str) -> String {
    format!("/subscriptions/{sub}/resourceGroups/{rg}/providers/{provider}/{name}")
}

fn meta(tags: &[(&str, &str)]) -> ResourceMeta {
    ResourceMeta {
        created_by: Some(CREATED_BY.to_string()),
        created_by_type: Some("User".to_string()),
        modified_by: Some(CREATED_BY.to_string()),
        modified_by_type: Some("User".to_string()),
        tags: tags
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        ..Default::default()
    }
}

#[allow(clippy::too_many_arguments)]
fn resource(
    sub: &str,
    rg: &str,
    provider: &str,
    name: &str,
    kind: ResourceKind,
    state: &str,
    age_days: i64,
    meta: ResourceMeta,
) -> Resource {
    let now = Utc::now();
    Resource {
        id: resource_id(sub, rg, provider, name),
        name: name.to_string(),
        kind,
        location: LOCATION.to_string(),
        resource_group: rg.to_string(),
        subscription_id: sub.to_string(),
        state: Some(state.to_string()),
        created_at: Some(now - Duration::days(age_days)),
        modified_at: Some(now - Duration::days(age_days.min(11))),
        meta,
    }
}

/// The full mock resource inventory, optionally filtered to `sub_ids`
/// (empty = all, mirroring Resource Graph's behavior). Sorted by name like
/// the real KQL's `order by name asc`.
pub fn resources(sub_ids: &[String]) -> Vec<Resource> {
    let mut apim_meta = meta(&[("env", "prod"), ("team", "platform")]);
    apim_meta.gateway_url = Some("https://api.contoso.com".to_string());
    apim_meta.public_ips = vec!["20.93.184.27".to_string()];
    apim_meta.public_network_access = Some("Enabled".to_string());

    let mut all = vec![
        resource(
            SUB_PROD,
            "rg-commerce-prod",
            "Microsoft.Web/sites",
            "func-orders-prod",
            ResourceKind::FunctionApp,
            "Running",
            412,
            meta(&[("env", "prod"), ("team", "commerce")]),
        ),
        resource(
            SUB_PROD,
            "rg-commerce-prod",
            "Microsoft.Web/sites",
            "func-invoice-gen",
            ResourceKind::FunctionApp,
            "Running",
            389,
            meta(&[("env", "prod"), ("team", "commerce")]),
        ),
        resource(
            SUB_PROD,
            "rg-integrations-prod",
            "Microsoft.Web/sites",
            "func-webhook-relay",
            ResourceKind::FunctionApp,
            "Running",
            201,
            meta(&[("env", "prod"), ("team", "integrations")]),
        ),
        resource(
            SUB_PROD,
            "rg-platform-prod",
            "Microsoft.ApiManagement/service",
            "apim-contoso-prod",
            ResourceKind::Apim,
            "Running",
            540,
            apim_meta,
        ),
        resource(
            SUB_PROD,
            "rg-commerce-prod",
            "Microsoft.App/containerApps",
            "ca-checkout-api",
            ResourceKind::ContainerApp,
            "Running",
            156,
            meta(&[("env", "prod"), ("team", "commerce")]),
        ),
        resource(
            SUB_PROD,
            "rg-platform-prod",
            "Microsoft.App/containerApps",
            "ca-search-api",
            ResourceKind::ContainerApp,
            "Running",
            148,
            meta(&[("env", "prod"), ("team", "platform")]),
        ),
        resource(
            SUB_PROD,
            "rg-network-prod",
            "Microsoft.Network/applicationGateways",
            "agw-edge-prod",
            ResourceKind::AppGateway,
            "Running",
            530,
            meta(&[("env", "prod"), ("team", "platform")]),
        ),
        resource(
            SUB_STAGING,
            "rg-commerce-staging",
            "Microsoft.Web/sites",
            "func-orders-staging",
            ResourceKind::FunctionApp,
            "Running",
            97,
            meta(&[("env", "staging"), ("team", "commerce")]),
        ),
        resource(
            SUB_STAGING,
            "rg-commerce-staging",
            "Microsoft.App/containerApps",
            "ca-checkout-api-stg",
            ResourceKind::ContainerApp,
            "Running",
            97,
            meta(&[("env", "staging"), ("team", "commerce")]),
        ),
    ];
    all.retain(|r| in_subs(&r.subscription_id, sub_ids));
    all.sort_by(|a, b| a.name.cmp(&b.name));
    all
}

/// The demo APIM service id (there is exactly one APIM in the mock tenant).
pub fn apim_service_id() -> String {
    resource_id(
        SUB_PROD,
        "rg-platform-prod",
        "Microsoft.ApiManagement/service",
        "apim-contoso-prod",
    )
}

// ---------------------------------------------------------------------------
// Metrics + health
// ---------------------------------------------------------------------------

fn buckets(range: TimeRange) -> usize {
    match range {
        TimeRange::Hour => 60,
        TimeRange::Day => 96,
        TimeRange::Week => 168,
    }
}

fn bucket_step(range: TimeRange) -> Duration {
    match range {
        TimeRange::Hour => Duration::minutes(1),
        TimeRange::Day => Duration::minutes(15),
        TimeRange::Week => Duration::hours(1),
    }
}

/// Stable per-resource seed so each resource's charts differ but stay
/// consistent between fetches.
fn seed_for(resource_id: &str) -> u64 {
    resource_id.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ b as u64).wrapping_mul(0x1000_0000_01b3)
    })
}

fn series(
    kind: MetricKind,
    label: &str,
    unit: &str,
    range: TimeRange,
    seed: u64,
    f: impl Fn(usize, f64, f64) -> f64,
) -> MetricSeries {
    let n = buckets(range);
    let step = bucket_step(range);
    let now = Utc::now();
    let points = (0..n)
        .map(|i| {
            // Diurnal-looking wave: one slow cycle across the window.
            let phase = (i as f64) / (n as f64) * std::f64::consts::TAU;
            let wave = 0.5 + 0.5 * phase.sin();
            MetricPoint {
                ts: now - step * ((n - i) as i32),
                value: f(i, wave, noise(seed, i)),
            }
        })
        .collect();
    MetricSeries {
        kind,
        label: label.to_string(),
        unit: unit.to_string(),
        points,
        peak_replica: None,
    }
}

/// Chart metrics for the Detail view, shaped per resource kind exactly like
/// `metrics::fetch` would return them (including the `missing` map for kinds
/// that don't expose a given metric).
pub fn metrics(resource: &Resource, range: TimeRange) -> MetricsResult {
    let seed = seed_for(&resource.id);
    // `func-webhook-relay` carries a visible 5xx burst so screenshots show a
    // degraded badge / error sparkline next to otherwise-healthy rows.
    let bursty = resource.name == "func-webhook-relay";
    let n = buckets(range);

    let traffic = series(
        MetricKind::Traffic,
        "Requests",
        "count",
        range,
        seed,
        |_i, wave, nz| (40.0 + 220.0 * wave + 30.0 * nz).round(),
    );
    let errors = series(
        MetricKind::Errors,
        "Http 5xx",
        "count",
        range,
        seed.wrapping_add(1),
        move |i, _wave, nz| {
            if bursty && i > n * 2 / 3 && i < n * 2 / 3 + n / 12 {
                (18.0 + 14.0 * nz).round()
            } else if nz > 0.93 {
                (3.0 * nz).round()
            } else {
                0.0
            }
        },
    );

    let mut out = MetricsResult {
        series: vec![errors, traffic],
        missing: HashMap::new(),
    };

    match resource.kind {
        ResourceKind::FunctionApp => {
            out.series.push(series(
                MetricKind::Cpu,
                "CPU",
                "s",
                range,
                seed.wrapping_add(2),
                |_i, wave, nz| 4.0 + 22.0 * wave + 5.0 * nz,
            ));
            out.series.push(series(
                MetricKind::Memory,
                "Memory",
                "bytes",
                range,
                seed.wrapping_add(3),
                |_i, wave, nz| 1.45e8 + 4.0e7 * wave + 1.0e7 * nz,
            ));
        }
        ResourceKind::ContainerApp => {
            // The plotted series is the across-replica average; stamp a busier
            // single-replica peak so the demo exercises the `peak-replica` line.
            let mut cpu = series(
                MetricKind::Cpu,
                "CPU",
                "mCores",
                range,
                seed.wrapping_add(2),
                |_i, wave, nz| 60.0 + 180.0 * wave + 40.0 * nz,
            );
            cpu.peak_replica = Some(cpu.max().max(0.0) * 1.35);
            out.series.push(cpu);
            let mut mem = series(
                MetricKind::Memory,
                "Memory",
                "bytes",
                range,
                seed.wrapping_add(3),
                |_i, wave, nz| 3.1e8 + 1.2e8 * wave + 2.0e7 * nz,
            );
            mem.peak_replica = Some(mem.max().max(0.0) * 1.25);
            out.series.push(mem);
        }
        ResourceKind::Apim => {
            out.series.push(series(
                MetricKind::Cpu,
                "Capacity",
                "%",
                range,
                seed.wrapping_add(2),
                |_i, wave, nz| 8.0 + 19.0 * wave + 4.0 * nz,
            ));
            out.missing.insert(
                MetricKind::Memory,
                "not exposed by Microsoft.ApiManagement".to_string(),
            );
        }
        ResourceKind::AppGateway => {
            out.series.push(series(
                MetricKind::Cpu,
                "Capacity Units",
                "count",
                range,
                seed.wrapping_add(2),
                |_i, wave, nz| (2.0 + 3.0 * wave + nz).round(),
            ));
            out.missing.insert(
                MetricKind::Memory,
                "not exposed by Microsoft.Network/applicationGateways".to_string(),
            );
        }
    }
    out
}

/// Fixed-24h Errors + Traffic window for the health badge, mirroring
/// `metrics::fetch_health`.
pub fn health_metrics(resource_id: &str, kind: ResourceKind) -> Vec<MetricSeries> {
    let probe = Resource {
        id: resource_id.to_string(),
        name: resource_id.rsplit('/').next().unwrap_or("").to_string(),
        kind,
        location: LOCATION.to_string(),
        resource_group: String::new(),
        subscription_id: String::new(),
        state: None,
        created_at: None,
        modified_at: None,
        meta: ResourceMeta::default(),
    };
    metrics(&probe, crate::azure::metrics::HEALTH_RANGE)
        .series
        .into_iter()
        .filter(|s| matches!(s.kind, MetricKind::Errors | MetricKind::Traffic))
        .collect()
}

/// Platform availability. One resource reports Degraded so the badge palette
/// shows more than green in screenshots.
pub fn availability(resource_id: &str) -> ResourceAvailability {
    if resource_id.ends_with("/func-webhook-relay") {
        ResourceAvailability {
            state: AvailabilityState::Degraded,
            reason: Some("Elevated 5xx rate detected on instance wk-7".to_string()),
        }
    } else {
        ResourceAvailability {
            state: AvailabilityState::Available,
            reason: None,
        }
    }
}

/// Container App availability + active revision metadata (one fetch feeds two
/// events in the real path; demo mirrors that shape).
pub fn revision_info(resource_id: &str) -> RevisionInfo {
    let app = resource_id.rsplit('/').next().unwrap_or("app");
    RevisionInfo {
        availability: ResourceAvailability {
            state: AvailabilityState::Available,
            reason: None,
        },
        active_revision: Some(ActiveRevisionMeta {
            name: format!("{app}--v42"),
            image: Some(format!("crcontosoprod.azurecr.io/{app}:1.7.3")),
            replicas: 3,
            min_replicas: 1,
            max_replicas: 10,
            running_state: "Running".to_string(),
            provisioning_error: None,
        }),
    }
}

// ---------------------------------------------------------------------------
// Container Apps
// ---------------------------------------------------------------------------

pub fn container_app_overview(resource_id: &str) -> ContainerAppOverview {
    let app = resource_id.rsplit('/').next().unwrap_or("app");
    let env_vars = vec![
        EnvVar {
            name: "ASPNETCORE_ENVIRONMENT".to_string(),
            value: "Production".to_string(),
            is_secret: false,
            ..Default::default()
        },
        EnvVar {
            name: "ORDERS_DB_CONNECTION".to_string(),
            // Same display shape `from_container_env` produces, so Enter on this
            // row resolves through `secrets` below to the Key Vault and decodes.
            value: "(secret: orders-db-connection)".to_string(),
            is_secret: true,
            ..Default::default()
        },
        EnvVar {
            name: "SERVICEBUS_NAMESPACE".to_string(),
            value: "sb-contoso-prod.servicebus.windows.net".to_string(),
            is_secret: false,
            ..Default::default()
        },
    ];
    let containers = vec![ContainerSpec {
        name: app.to_string(),
        image: Some(format!("crcontosoprod.azurecr.io/{app}:1.7.3")),
        cpu_millicores: 500,
        memory_bytes: 1024 * 1024 * 1024,
        ephemeral_storage: Some("2Gi".to_string()),
        env_vars,
        is_init: false,
    }];
    ContainerAppOverview {
        cpu_millicores: 500,
        memory_bytes: 1024 * 1024 * 1024,
        fqdn: Some(format!(
            "{app}.kindplant-8e3f12a9.westeurope.azurecontainerapps.io"
        )),
        ingress_external: Some(true),
        access_restricted: false,
        managed_environment: Some("cae-contoso-prod".to_string()),
        managed_identity: Some("SystemAssigned".to_string()),
        env_vars: crate::azure::container_app_overview::explode_container_env(&containers),
        containers,
        // `ORDERS_DB_CONNECTION`'s `secretRef` resolves here: a Key Vault-backed
        // app secret pointing at `kv-contoso-prod` (see `key_vaults`).
        secrets: vec![crate::azure::container_app_overview::ContainerAppSecret {
            name: "orders-db-connection".to_string(),
            key_vault_url: Some(
                "https://kv-contoso-prod.vault.azure.net/secrets/orders-db-connection".to_string(),
            ),
        }],
    }
}

pub fn replicas(resource_id: &str, revision_name: &str) -> Vec<ReplicaInstance> {
    let app = resource_id.rsplit('/').next().unwrap_or("app");
    let now = Utc::now();

    // `ca-search-api` demos a stuck rollout: a fresh replica whose container
    // can't pull its image (ImagePullBackOff), so the instances block shows the
    // reason inline instead of a bare `✗ restarts 0`.
    if app == "ca-search-api" {
        return vec![ReplicaInstance {
            name: format!("{revision_name}-d4f9k"),
            created_at: Some(now - Duration::minutes(6)),
            running_state: Some("NotRunning".to_string()),
            containers: vec![ReplicaContainer {
                name: app.to_string(),
                ready: Some(false),
                started: Some(false),
                restart_count: 0,
                running_state: Some("Waiting".to_string()),
                running_state_details: Some(format!(
                    "Back-off pulling image \"crcontosoprod.azurecr.io/{app}:2.4.0-rc1\" \
                     — manifest tagged \"2.4.0-rc1\" not found in registry"
                )),
            }],
        }];
    }

    (0..3)
        .map(|i| ReplicaInstance {
            name: format!("{revision_name}-{}", ["fl9k2", "x7m4p", "q2v8c"][i]),
            created_at: Some(now - Duration::hours(14 + 3 * i as i64)),
            running_state: Some("Running".to_string()),
            containers: vec![ReplicaContainer {
                name: app.to_string(),
                ready: Some(true),
                started: Some(true),
                restart_count: if i == 2 { 1 } else { 0 },
                running_state: Some("Running".to_string()),
                running_state_details: None,
            }],
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Function Apps
// ---------------------------------------------------------------------------

pub fn web_config(resource_id: &str) -> WebConfig {
    let image = if resource_id.ends_with("/func-orders-prod") {
        Some("DOCKER|crcontosoprod.azurecr.io/func-orders:2.3.1".to_string())
    } else {
        None
    };
    WebConfig {
        image,
        access_restricted: false,
    }
}

pub fn function_app_settings(_resource_id: &str) -> Vec<EnvVar> {
    let var = |name: &str, value: &str, secret: bool| EnvVar {
        name: name.to_string(),
        value: value.to_string(),
        is_secret: secret,
        ..Default::default()
    };
    vec![
        var("FUNCTIONS_EXTENSION_VERSION", "~4", false),
        var("FUNCTIONS_WORKER_RUNTIME", "dotnet-isolated", false),
        var(
            "APPLICATIONINSIGHTS_CONNECTION_STRING",
            "InstrumentationKey=00000000-0000-0000-0000-000000000000",
            true,
        ),
        var(
            "AzureWebJobsStorage",
            "DefaultEndpointsProtocol=https;AccountName=stcontosofnprod;AccountKey=…",
            true,
        ),
        var(
            "SERVICEBUS_CONNECTION__fullyQualifiedNamespace",
            "sb-contoso-prod.servicebus.windows.net",
            false,
        ),
        var("ORDERS_TOPIC_NAME", "order-events", false),
    ]
}

pub fn function_app_triggers(_resource_id: &str) -> Vec<FunctionTrigger> {
    let trigger = |function: &str, kind: &str, detail: &str| FunctionTrigger {
        function: function.to_string(),
        kind: kind.to_string(),
        detail: Some(detail.to_string()),
    };
    vec![
        trigger(
            "ProcessOrder",
            "serviceBusTrigger",
            "queue: orders-incoming",
        ),
        trigger("GetOrderStatus", "httpTrigger", "GET /api/orders/{orderId}"),
        trigger("NightlyReconciliation", "timerTrigger", "0 0 2 * * *"),
    ]
}

pub fn principal_display_name(_object_id: &str) -> Option<String> {
    Some("Contoso CI Pipeline".to_string())
}

// ---------------------------------------------------------------------------
// APIM
// ---------------------------------------------------------------------------

pub fn apim_apis(service_id: &str) -> Vec<Api> {
    [
        (
            "orders-api",
            "Orders API",
            "orders",
            Some("https://orders.internal.demo.local"),
        ),
        (
            "payments-api",
            "Payments API",
            "payments",
            Some("https://payments.internal.demo.local"),
        ),
        (
            "catalog-api",
            "Catalog API",
            "catalog",
            Some("https://catalog.internal.demo.local"),
        ),
        // No static backend — routed in policy via set-backend-service.
        ("echo-api", "Echo API", "echo", None),
    ]
    .iter()
    .map(|(name, display, path, service_url)| Api {
        id: format!("{service_id}/apis/{name}"),
        name: name.to_string(),
        display_name: display.to_string(),
        path: path.to_string(),
        service_url: service_url.map(|s| s.to_string()),
    })
    .collect()
}

pub fn apim_operations(api_id: &str) -> Vec<Operation> {
    let ops: &[(&str, &str, &str, &str)] = &[
        ("list", "List", "GET", "/"),
        ("get-by-id", "Get by id", "GET", "/{id}"),
        ("create", "Create", "POST", "/"),
        ("update", "Update", "PUT", "/{id}"),
        ("delete", "Delete", "DELETE", "/{id}"),
    ];
    ops.iter()
        .map(|(name, display, method, template)| Operation {
            id: format!("{api_id}/operations/{name}"),
            name: name.to_string(),
            display_name: display.to_string(),
            method: method.to_string(),
            url_template: template.to_string(),
        })
        .collect()
}

/// Policy XML for any demo operation. The `delete` operations report no policy
/// (`None`) so the "no policy configured" placeholder is photographable too.
pub fn apim_operation_policy(operation_id: &str) -> Option<String> {
    if operation_id.ends_with("/operations/delete") {
        return None;
    }
    Some(
        r#"<policies>
    <inbound>
        <base />
        <rate-limit-by-key calls="100" renewal-period="60" counter-key="@(context.Subscription.Id)" />
        <validate-jwt header-name="Authorization" failed-validation-httpcode="401">
            <openid-config url="https://login.contoso.com/.well-known/openid-configuration" />
            <required-claims>
                <claim name="aud" match="any">
                    <value>api://contoso-orders</value>
                </claim>
            </required-claims>
        </validate-jwt>
        <set-backend-service base-url="https://ca-checkout-api.internal.contoso.com" />
    </inbound>
    <backend>
        <forward-request timeout="20" />
    </backend>
    <outbound>
        <base />
        <set-header name="X-Powered-By" exists-action="delete" />
    </outbound>
    <on-error>
        <base />
    </on-error>
</policies>"#
            .to_string(),
    )
}

// ---------------------------------------------------------------------------
// Application Gateway
// ---------------------------------------------------------------------------

pub fn appgw_backends(_resource_id: &str) -> Vec<BackendPool> {
    vec![
        BackendPool {
            name: "apim-backend".to_string(),
            addresses: vec![BackendAddress {
                fqdn: Some("apim-contoso-prod.azure-api.net".to_string()),
                ip_address: None,
            }],
            nic_ip_config_refs: vec![],
        },
        BackendPool {
            name: "web-frontend".to_string(),
            addresses: vec![
                BackendAddress {
                    fqdn: None,
                    ip_address: Some("10.0.2.4".to_string()),
                },
                BackendAddress {
                    fqdn: None,
                    ip_address: Some("10.0.2.5".to_string()),
                },
            ],
            nic_ip_config_refs: vec![NicIpConfigRef {
                nic_name: "nic-web-01".to_string(),
                config_name: "ipconfig1".to_string(),
                full_id: format!(
                    "/subscriptions/{SUB_PROD}/resourceGroups/rg-network-prod/providers/Microsoft.Network/networkInterfaces/nic-web-01/ipConfigurations/ipconfig1"
                ),
            }],
        },
    ]
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

pub fn storage_accounts(sub_ids: &[String]) -> Vec<StorageAccount> {
    let now = Utc::now();
    let account = |sub: &str, rg: &str, name: &str, days: i64| StorageAccount {
        id: resource_id(sub, rg, "Microsoft.Storage/storageAccounts", name),
        name: name.to_string(),
        resource_group: rg.to_string(),
        subscription_id: sub.to_string(),
        location: LOCATION.to_string(),
        kind: Some("StorageV2".to_string()),
        sku: Some("Standard_LRS".to_string()),
        access_tier: Some("Hot".to_string()),
        is_hns_enabled: Some(false),
        https_only: Some(true),
        allow_blob_public_access: Some(false),
        created_at: Some(now - Duration::days(days)),
    };
    let mut all = vec![
        account(SUB_PROD, "rg-commerce-prod", "stcontosodataprod", 410),
        account(SUB_PROD, "rg-platform-prod", "stcontosologs", 530),
        account(SUB_STAGING, "rg-commerce-staging", "stcontosodatastg", 97),
    ];
    all.retain(|a| in_subs(&a.subscription_id, sub_ids));
    all
}

pub fn storage_containers(_account: &StorageAccount) -> Vec<BlobContainer> {
    let now = Utc::now();
    ["invoices", "exports", "raw-events"]
        .iter()
        .enumerate()
        .map(|(i, name)| BlobContainer {
            name: name.to_string(),
            public_access: None,
            last_modified: Some(now - Duration::hours(3 + 7 * i as i64)),
            has_immutability_policy: Some(*name == "invoices"),
        })
        .collect()
}

pub fn storage_overview(_account: &StorageAccount) -> StorageAccountStats {
    StorageAccountStats {
        used_capacity_bytes: Some(48_318_382_080),
        container_count: Some(3),
        blob_count: Some(18_204),
        blob_capacity_bytes: Some(46_170_898_432),
        file_share_count: Some(1),
        file_count: Some(112),
        file_capacity_bytes: Some(1_073_741_824),
        queue_count: Some(2),
        queue_message_count: Some(14),
        queue_capacity_bytes: Some(65_536),
        table_count: Some(4),
        table_entity_count: Some(92_330),
        table_capacity_bytes: Some(1_073_741_824),
        as_of: Some(Utc::now() - Duration::hours(1)),
    }
}

pub fn storage_blobs(_account_name: &str, container: &str) -> Vec<Blob> {
    let now = Utc::now();
    let blob = |name: &str, size: u64, ct: &str, hours: i64| Blob {
        name: name.to_string(),
        size,
        content_type: Some(ct.to_string()),
        last_modified: Some(now - Duration::hours(hours)),
        blob_type: "BlockBlob".to_string(),
    };
    match container {
        "invoices" => vec![
            blob("2026/06/INV-100482.pdf", 184_320, "application/pdf", 2),
            blob("2026/06/INV-100481.pdf", 162_004, "application/pdf", 5),
            blob("2026/06/INV-100480.pdf", 177_551, "application/pdf", 9),
            blob("2026/05/INV-100479.pdf", 158_220, "application/pdf", 240),
        ],
        "exports" => vec![
            blob("orders-2026-06-09.csv", 2_493_001, "text/csv", 14),
            blob("orders-2026-06-08.csv", 2_311_876, "text/csv", 38),
        ],
        _ => vec![
            blob(
                "events/2026/06/09/batch-0042.json",
                912_330,
                "application/json",
                1,
            ),
            blob(
                "events/2026/06/09/batch-0041.json",
                877_104,
                "application/json",
                2,
            ),
            blob(
                "events/2026/06/08/batch-0040.json",
                901_274,
                "application/json",
                26,
            ),
        ],
    }
}

pub fn blob_preview(_account_name: &str, _container: &str, blob: &str) -> BlobPreview {
    let now = Utc::now();
    if blob.ends_with(".pdf") {
        return BlobPreview {
            metadata: BlobMetadata {
                content_type: Some("application/pdf".to_string()),
                content_length: 184_320,
                etag: Some("\"0x8DDA1B2C3D4E5F6\"".to_string()),
                last_modified: Some(now - Duration::hours(2)),
                content_md5: Some("1B2M2Y8AsgTpgAmY7PhCfg==".to_string()),
            },
            body: BlobPreviewBody::Binary {
                reason: "binary content (application/pdf, 180.0 KB)".to_string(),
            },
        };
    }
    let body = r#"{
  "batchId": "batch-0042",
  "generatedAt": "2026-06-09T14:02:11Z",
  "events": [
    { "type": "order.created",  "orderId": "ord_18452", "total": 129.90, "currency": "EUR" },
    { "type": "order.paid",     "orderId": "ord_18452", "provider": "adyen" },
    { "type": "order.shipped",  "orderId": "ord_18421", "carrier": "dhl", "tracking": "JD014600003573" }
  ]
}"#;
    BlobPreview {
        metadata: BlobMetadata {
            content_type: Some("application/json".to_string()),
            content_length: body.len() as u64,
            etag: Some("\"0x8DDA9F8E7D6C5B4\"".to_string()),
            last_modified: Some(now - Duration::hours(1)),
            content_md5: None,
        },
        body: BlobPreviewBody::Text(body.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Container registries
// ---------------------------------------------------------------------------

pub fn registries(sub_ids: &[String]) -> Vec<Registry> {
    let mut all = vec![Registry {
        id: resource_id(
            SUB_PROD,
            "rg-platform-prod",
            "Microsoft.ContainerRegistry/registries",
            "crcontosoprod",
        ),
        name: "crcontosoprod".to_string(),
        resource_group: "rg-platform-prod".to_string(),
        subscription_id: SUB_PROD.to_string(),
        location: LOCATION.to_string(),
        sku: Some("Standard".to_string()),
        login_server: Some("crcontosoprod.azurecr.io".to_string()),
        admin_user_enabled: Some(false),
        public_network_access: Some("Enabled".to_string()),
        anonymous_pull_enabled: Some(false),
        created_at: Some(Utc::now() - Duration::days(520)),
    }];
    all.retain(|r| in_subs(&r.subscription_id, sub_ids));
    all
}

pub fn repositories(_registry: &Registry) -> Vec<Repository> {
    [
        "ca-checkout-api",
        "ca-search-api",
        "func-orders",
        "base/dotnet-runtime",
    ]
    .iter()
    .map(|n| Repository {
        name: n.to_string(),
    })
    .collect()
}

pub fn tags(_repository: &str) -> Vec<Tag> {
    ["1.7.3", "1.7.2", "1.7.1", "1.6.0", "latest"]
        .iter()
        .map(|n| Tag {
            name: n.to_string(),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Cosmos DB
// ---------------------------------------------------------------------------

/// Flat list of demo Azure SQL resources: a couple of elastic pools and a few
/// single databases (one of them pooled), spread across the demo subscriptions.
pub fn sql_resources(sub_ids: &[String]) -> Vec<SqlResource> {
    let sql_id = |sub: &str, rg: &str, server: &str, leaf: &str| {
        resource_id(sub, rg, &format!("Microsoft.Sql/servers/{server}"), leaf)
    };
    let pool_a_id = sql_id(
        SUB_PROD,
        "rg-commerce-prod",
        "sql-contoso-prod",
        "elasticPools/pool-prod",
    );
    let mut all = vec![
        SqlResource {
            id: pool_a_id.clone(),
            name: "pool-prod".to_string(),
            server: "sql-contoso-prod".to_string(),
            resource_group: "rg-commerce-prod".to_string(),
            subscription_id: SUB_PROD.to_string(),
            location: LOCATION.to_string(),
            kind: SqlKind::ElasticPool,
            sku_name: Some("GP_Gen5".to_string()),
            sku_tier: Some("GeneralPurpose".to_string()),
            capacity: Some(8),
            status: Some("Ready".to_string()),
            elastic_pool_id: None,
            max_size_bytes: Some(268_435_456_000),
        },
        SqlResource {
            id: sql_id(
                SUB_PROD,
                "rg-commerce-prod",
                "sql-contoso-prod",
                "databases/orders",
            ),
            name: "orders".to_string(),
            server: "sql-contoso-prod".to_string(),
            resource_group: "rg-commerce-prod".to_string(),
            subscription_id: SUB_PROD.to_string(),
            location: LOCATION.to_string(),
            kind: SqlKind::Database,
            sku_name: Some("GP_Gen5_2".to_string()),
            sku_tier: Some("GeneralPurpose".to_string()),
            capacity: Some(2),
            status: Some("Online".to_string()),
            // Member of the prod pool above.
            elastic_pool_id: Some(pool_a_id),
            max_size_bytes: Some(34_359_738_368),
        },
        SqlResource {
            id: sql_id(
                SUB_PROD,
                "rg-commerce-prod",
                "sql-contoso-prod",
                "databases/inventory",
            ),
            name: "inventory".to_string(),
            server: "sql-contoso-prod".to_string(),
            resource_group: "rg-commerce-prod".to_string(),
            subscription_id: SUB_PROD.to_string(),
            location: LOCATION.to_string(),
            kind: SqlKind::Database,
            sku_name: Some("S3".to_string()),
            sku_tier: Some("Standard".to_string()),
            capacity: Some(100),
            status: Some("Online".to_string()),
            elastic_pool_id: None,
            max_size_bytes: Some(268_435_456_000),
        },
        SqlResource {
            id: sql_id(
                SUB_STAGING,
                "rg-commerce-staging",
                "sql-contoso-staging",
                "databases/orders",
            ),
            name: "orders".to_string(),
            server: "sql-contoso-staging".to_string(),
            resource_group: "rg-commerce-staging".to_string(),
            subscription_id: SUB_STAGING.to_string(),
            location: LOCATION.to_string(),
            kind: SqlKind::Database,
            sku_name: Some("Basic".to_string()),
            sku_tier: Some("Basic".to_string()),
            capacity: Some(5),
            status: Some("Paused".to_string()),
            elastic_pool_id: None,
            max_size_bytes: Some(2_147_483_648),
        },
    ];
    all.retain(|r| in_subs(&r.subscription_id, sub_ids));
    all
}

/// Synthesized utilization metrics for a demo SQL pool / database, shaped
/// exactly like [`crate::azure::sql::fetch_metrics`] would return them. Basic
/// (DTU) tier resources omit the workers metric to exercise the `missing` path.
pub fn sql_metrics(resource_id: &str, range: TimeRange) -> MetricsResult {
    let seed = seed_for(resource_id);
    let pct = |base: f64, amp: f64| {
        move |_i: usize, wave: f64, nz: f64| (base + amp * wave + 6.0 * nz).clamp(0.0, 100.0)
    };
    let series = vec![
        series(MetricKind::Cpu, "CPU", "%", range, seed, pct(22.0, 50.0)),
        series(
            MetricKind::Dtu,
            "eDTU",
            "%",
            range,
            seed.wrapping_add(1),
            pct(30.0, 45.0),
        ),
        series(
            MetricKind::Storage,
            "Storage",
            "%",
            range,
            seed.wrapping_add(2),
            pct(58.0, 4.0),
        ),
        series(
            MetricKind::Workers,
            "Workers",
            "%",
            range,
            seed.wrapping_add(3),
            pct(12.0, 30.0),
        ),
    ];
    MetricsResult {
        series,
        missing: HashMap::new(),
    }
}

pub fn cosmos_accounts(sub_ids: &[String]) -> Vec<CosmosAccount> {
    let mut all = vec![CosmosAccount {
        id: resource_id(
            SUB_PROD,
            "rg-commerce-prod",
            "Microsoft.DocumentDB/databaseAccounts",
            "cosmos-contoso-prod",
        ),
        name: "cosmos-contoso-prod".to_string(),
        resource_group: "rg-commerce-prod".to_string(),
        subscription_id: SUB_PROD.to_string(),
        location: LOCATION.to_string(),
        kind: Some("GlobalDocumentDB".to_string()),
        document_endpoint: Some("https://cosmos-contoso-prod.documents.azure.com:443/".to_string()),
        capabilities: vec![],
        is_serverless: false,
        public_network_access: Some("Enabled".to_string()),
        created_at: Some(Utc::now() - Duration::days(388)),
    }];
    all.retain(|a| in_subs(&a.subscription_id, sub_ids));
    all
}

pub fn cosmos_databases(account: &CosmosAccount) -> Vec<CosmosDatabase> {
    ["commerce", "telemetry"]
        .iter()
        .map(|n| CosmosDatabase {
            id: format!("{}/sqlDatabases/{n}", account.id),
            name: n.to_string(),
        })
        .collect()
}

pub fn cosmos_containers(account: &CosmosAccount, db: &str) -> Vec<CosmosContainer> {
    let container = |name: &str, pk: &str, ttl: Option<i64>| CosmosContainer {
        id: format!("{}/sqlDatabases/{db}/containers/{name}", account.id),
        name: name.to_string(),
        partition_key_paths: vec![pk.to_string()],
        partition_key_kind: Some("Hash".to_string()),
        default_ttl: ttl,
        indexing_mode: Some("consistent".to_string()),
    };
    match db {
        "commerce" => vec![
            container("orders", "/customerId", None),
            container("customers", "/id", None),
            container("carts", "/customerId", Some(86_400)),
        ],
        _ => vec![container("request-traces", "/day", Some(604_800))],
    }
}

pub fn cosmos_items(coll: &str) -> CosmosItemPreview {
    let items = match coll {
        "orders" => vec![
            serde_json::json!({
                "id": "ord_18452",
                "customerId": "cus_99031",
                "status": "shipped",
                "total": 129.90,
                "currency": "EUR",
                "lines": [
                    { "sku": "SKU-4471", "qty": 1, "price": 89.90 },
                    { "sku": "SKU-1208", "qty": 2, "price": 20.00 }
                ],
                "createdAt": "2026-06-08T09:14:02Z"
            }),
            serde_json::json!({
                "id": "ord_18453",
                "customerId": "cus_50217",
                "status": "paid",
                "total": 54.50,
                "currency": "EUR",
                "createdAt": "2026-06-09T11:40:51Z"
            }),
        ],
        _ => vec![serde_json::json!({
            "id": "cus_99031",
            "name": "Avery Quinn",
            "email": "avery.quinn@example.com",
            "tier": "gold",
            "since": "2024-03-17"
        })],
    };
    CosmosItemPreview {
        items,
        request_charge: Some(2.83),
        partial: false,
    }
}

// ---------------------------------------------------------------------------
// Key Vault
// ---------------------------------------------------------------------------

pub fn key_vaults(sub_ids: &[String]) -> Vec<KeyVault> {
    let mut all = vec![KeyVault {
        id: resource_id(
            SUB_PROD,
            "rg-platform-prod",
            "Microsoft.KeyVault/vaults",
            "kv-contoso-prod",
        ),
        name: "kv-contoso-prod".to_string(),
        resource_group: "rg-platform-prod".to_string(),
        subscription_id: SUB_PROD.to_string(),
        location: LOCATION.to_string(),
        sku: Some("standard".to_string()),
        vault_uri: Some("https://kv-contoso-prod.vault.azure.net/".to_string()),
        rbac_authorization_enabled: Some(true),
        soft_delete_enabled: Some(true),
        purge_protection_enabled: Some(true),
        public_network_access: Some("Enabled".to_string()),
    }];
    all.retain(|v| in_subs(&v.subscription_id, sub_ids));
    all
}

pub fn key_vault_items(kind: ItemKind) -> Vec<KeyVaultItem> {
    let now = Utc::now();
    let item = |name: &str, ct: Option<&str>, expires_days: Option<i64>| KeyVaultItem {
        kind,
        name: name.to_string(),
        enabled: Some(true),
        expires: expires_days.map(|d| now + Duration::days(d)),
        not_before: None,
        created: Some(now - Duration::days(180)),
        updated: Some(now - Duration::days(30)),
        content_type: ct.map(str::to_string),
    };
    match kind {
        ItemKind::Secret => vec![
            item("orders-db-connection", None, None),
            item("payment-api-signing-key", None, Some(140)),
            item("smtp-relay-password", None, Some(15)),
        ],
        ItemKind::Certificate => vec![
            item("api-contoso-com", Some("application/x-pkcs12"), Some(204)),
            item(
                "internal-mtls-client",
                Some("application/x-pkcs12"),
                Some(63),
            ),
        ],
    }
}

pub fn key_vault_secret_value(name: &str) -> String {
    match name {
        "orders-db-connection" => {
            "Server=tcp:sql-contoso-prod.database.windows.net,1433;Database=orders;Authentication=Active Directory Default;".to_string()
        }
        _ => "s3cr3t-demo-value-not-real".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Service Bus
// ---------------------------------------------------------------------------

pub fn sb_namespaces(sub_ids: &[String]) -> Vec<ServiceBusNamespace> {
    let mut all = vec![ServiceBusNamespace {
        id: resource_id(
            SUB_PROD,
            "rg-commerce-prod",
            "Microsoft.ServiceBus/namespaces",
            "sb-contoso-prod",
        ),
        name: "sb-contoso-prod".to_string(),
        resource_group: "rg-commerce-prod".to_string(),
        subscription_id: SUB_PROD.to_string(),
        location: LOCATION.to_string(),
        sku: Some("Standard".to_string()),
        status: Some("Active".to_string()),
        endpoint: Some("https://sb-contoso-prod.servicebus.windows.net:443/".to_string()),
        created_at: Some(Utc::now() - Duration::days(401)),
    }];
    all.retain(|n| in_subs(&n.subscription_id, sub_ids));
    all
}

fn counts(active: i64, dead_letter: i64, scheduled: i64) -> CountDetails {
    CountDetails {
        active,
        dead_letter,
        scheduled,
        transfer: 0,
        transfer_dead_letter: 0,
    }
}

pub fn sb_queues(namespace: &ServiceBusNamespace) -> Vec<ServiceBusQueue> {
    let now = Utc::now();
    let queue = |name: &str, c: CountDetails, size: i64| ServiceBusQueue {
        id: format!("{}/queues/{name}", namespace.id),
        name: name.to_string(),
        status: Some("Active".to_string()),
        total_message_count: Some(c.active + c.dead_letter + c.scheduled),
        counts: c,
        max_delivery_count: Some(10),
        size_bytes: Some(size),
        requires_session: Some(false),
        updated_at: Some(now - Duration::minutes(7)),
    };
    vec![
        queue("orders-incoming", counts(12, 2, 0), 48_120),
        queue("invoice-requests", counts(0, 0, 3), 9_410),
        queue("webhook-retries", counts(4, 17, 0), 122_004),
    ]
}

pub fn sb_topics(namespace: &ServiceBusNamespace) -> Vec<ServiceBusTopic> {
    let now = Utc::now();
    vec![
        ServiceBusTopic {
            id: format!("{}/topics/order-events", namespace.id),
            name: "order-events".to_string(),
            status: Some("Active".to_string()),
            subscription_count: Some(3),
            size_bytes: Some(204_800),
            updated_at: Some(now - Duration::minutes(2)),
        },
        ServiceBusTopic {
            id: format!("{}/topics/audit-events", namespace.id),
            name: "audit-events".to_string(),
            status: Some("Active".to_string()),
            subscription_count: Some(1),
            size_bytes: Some(58_220),
            updated_at: Some(now - Duration::minutes(31)),
        },
    ]
}

pub fn sb_subscriptions(
    namespace: &ServiceBusNamespace,
    topic: &str,
) -> Vec<ServiceBusSubscription> {
    let now = Utc::now();
    let sub = |name: &str, c: CountDetails, forward: Option<&str>| ServiceBusSubscription {
        id: format!("{}/topics/{topic}/subscriptions/{name}", namespace.id),
        name: name.to_string(),
        status: Some("Active".to_string()),
        total_message_count: Some(c.active + c.dead_letter),
        counts: c,
        max_delivery_count: Some(10),
        requires_session: Some(false),
        forward_to: forward.map(str::to_string),
        updated_at: Some(now - Duration::minutes(4)),
    };
    match topic {
        "order-events" => vec![
            sub("billing", counts(3, 0, 0), None),
            sub("analytics", counts(0, 0, 0), None),
            sub("shipping", counts(1, 5, 0), Some("webhook-retries")),
        ],
        _ => vec![sub("compliance-archive", counts(0, 0, 0), None)],
    }
}

// ---------------------------------------------------------------------------
// Logs
// ---------------------------------------------------------------------------

/// One page of synthesized logs for a resource. Mirrors `logs::fetch`:
/// `errors_only` filters to Warn/Error, an `older_than` cursor returns an
/// empty terminal page (`has_more: false`), and Function App / Container App /
/// APIM each get their native log shape (the APIM lines look exactly like
/// parsed `ApiManagementGatewayLogs` request rows).
pub fn logs(
    resource: &Resource,
    range: TimeRange,
    errors_only: bool,
    older_than: Option<DateTime<Utc>>,
    around: Option<DateTime<Utc>>,
) -> LogsPage {
    if older_than.is_some() {
        return LogsPage {
            lines: Vec::new(),
            has_more: false,
            workspace_arm_id: None,
        };
    }
    let mut lines = match resource.kind {
        ResourceKind::FunctionApp => function_app_log_lines(range),
        ResourceKind::ContainerApp => container_app_log_lines(&resource.name, range),
        ResourceKind::Apim => apim_log_lines(range),
        ResourceKind::AppGateway => Vec::new(),
    };
    if let Some(ts) = around {
        // Context jump: an unfiltered window around the error's timestamp, so the
        // INFO lines bracketing it survive (mirrors the real windowed fetch).
        let half = Duration::minutes(3);
        lines.retain(|l| (l.ts - ts).num_seconds().abs() <= half.num_seconds());
    } else if errors_only {
        lines.retain(|l| matches!(l.level, LogLevel::Warn | LogLevel::Error));
    }
    LogsPage {
        lines,
        has_more: false,
        workspace_arm_id: None,
    }
}

/// Number of synthesized log lines per window — enough to scroll through but
/// comfortably under one page (`logs::PAGE_SIZE`), so the header reads
/// "window complete" instead of teasing a fetch-more that returns nothing.
fn log_count(range: TimeRange) -> usize {
    match range {
        TimeRange::Hour => 90,
        TimeRange::Day => 180,
        TimeRange::Week => 260,
    }
}

fn log_spacing(range: TimeRange) -> Duration {
    match range {
        TimeRange::Hour => Duration::seconds(38),
        TimeRange::Day => Duration::seconds(8 * 60),
        TimeRange::Week => Duration::seconds(37 * 60),
    }
}

fn line(
    ts: DateTime<Utc>,
    level: LogLevel,
    source: &str,
    message: String,
    fields: Vec<(&str, String)>,
) -> LogLine {
    LogLine {
        ts,
        level,
        source: source.to_string(),
        message,
        fields: fields
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
    }
}

fn function_app_log_lines(range: TimeRange) -> Vec<LogLine> {
    let now = Utc::now();
    let spacing = log_spacing(range);
    let n = log_count(range);
    (0..n)
        .map(|i| {
            let ts = now - spacing * (i as i32) - Duration::seconds((noise(11, i) * 20.0) as i64);
            let order = 18_460 - (i as i64 / 3);
            match i % 9 {
                0 => line(
                    ts,
                    LogLevel::Info,
                    "AppRequests",
                    format!("200 GET /api/orders/ord_{order}"),
                    vec![
                        ("OperationName", "GetOrderStatus".to_string()),
                        ("DurationMs", format!("{}", 18 + (noise(3, i) * 40.0) as i64)),
                        ("ClientIP", "203.0.113.42".to_string()),
                    ],
                ),
                3 => line(
                    ts,
                    LogLevel::Info,
                    "FunctionAppLogs/ProcessOrder",
                    format!("Executed 'Functions.ProcessOrder' (Succeeded, Id=c4{order:x}-eb)"),
                    vec![
                        ("Category", "Function.ProcessOrder".to_string()),
                        ("HostInstanceId", "9c1d2e3f-demo".to_string()),
                    ],
                ),
                5 if noise(7, i) > 0.55 => line(
                    ts,
                    LogLevel::Error,
                    "AppExceptions",
                    format!(
                        "System.TimeoutException: Payment provider did not respond within 10s (order ord_{order})"
                    ),
                    vec![
                        ("ProblemId", "System.TimeoutException at PaymentClient.Capture".to_string()),
                        ("OperationName", "ProcessOrder".to_string()),
                    ],
                ),
                5 => line(
                    ts,
                    LogLevel::Warn,
                    "AppTraces",
                    format!("Retrying payment capture for order ord_{order} (attempt 2/3)"),
                    vec![("Category", "PaymentClient".to_string())],
                ),
                7 => line(
                    ts,
                    LogLevel::Info,
                    "AppTraces",
                    format!("Queue trigger fired: orders-incoming message {}", 9_900 + i),
                    vec![("Category", "Host.Triggers.ServiceBus".to_string())],
                ),
                _ => line(
                    ts,
                    LogLevel::Info,
                    "FunctionAppLogs/ProcessOrder",
                    format!("Order ord_{order} validated and persisted in {}ms", 12 + (noise(5, i) * 60.0) as i64),
                    vec![("Category", "Function.ProcessOrder".to_string())],
                ),
            }
        })
        .collect()
}

fn container_app_log_lines(app: &str, range: TimeRange) -> Vec<LogLine> {
    let now = Utc::now();
    let spacing = log_spacing(range);
    let n = log_count(range);
    (0..n)
        .map(|i| {
            let ts = now - spacing * (i as i32) - Duration::seconds((noise(13, i) * 15.0) as i64);
            let latency = 40 + (noise(17, i) * 160.0) as i64;
            let order = 18_460 - (i as i64 / 2);
            let (level, msg) = match i % 11 {
                4 if noise(19, i) > 0.6 => (
                    LogLevel::Error,
                    "ERROR redis: connection reset by peer, reconnecting (attempt 1)".to_string(),
                ),
                4 => (
                    LogLevel::Warn,
                    format!("WARN slow query: basket lookup took {}ms", 350 + latency),
                ),
                8 => (
                    LogLevel::Info,
                    format!("INFO health: readiness probe ok (uptime {}h)", 14 + i % 70),
                ),
                _ => (
                    LogLevel::Info,
                    format!("INFO {app}: order ord_{order} priced and reserved in {latency}ms"),
                ),
            };
            line(
                ts,
                level,
                // Real rows surface the emitting container as the source (see
                // `logs::parse_container_app_row`); the demo app runs a single
                // container named after the app (matching `demo::replicas`).
                app,
                msg,
                vec![
                    ("ContainerAppName", app.to_string()),
                    ("ContainerName", app.to_string()),
                    ("RevisionName", format!("{app}--v42")),
                ],
            )
        })
        .collect()
}

/// APIM gateway request rows, shaped exactly like `logs::parse_apim_row`
/// renders real `ApiManagementGatewayLogs` rows — status-led message, timing
/// suffix, full column set in `fields` for the detail view.
fn apim_log_lines(range: TimeRange) -> Vec<LogLine> {
    let now = Utc::now();
    let spacing = log_spacing(range);
    let n = log_count(range);
    let routes: &[(&str, &str, &str, &str)] = &[
        ("GET", "/orders/ord_18452", "orders-api", "get-by-id"),
        ("POST", "/orders", "orders-api", "create"),
        ("GET", "/catalog/items?page=2", "catalog-api", "list"),
        ("POST", "/payments/captures", "payments-api", "create"),
        ("GET", "/orders?status=open", "orders-api", "list"),
    ];
    (0..n)
        .map(|i| {
            let ts = now - spacing * (i as i32) - Duration::seconds((noise(23, i) * 12.0) as i64);
            let (method, path, api, op) = routes[i % routes.len()];
            let backend = 8 + (noise(29, i) * 60.0) as i64;
            let total = backend + 4 + (noise(31, i) * 14.0) as i64;
            let nz = noise(37, i);
            let (code, level, last_error): (i64, LogLevel, Option<&str>) = if nz > 0.96 {
                (502, LogLevel::Error, Some("BackendConnectionFailure"))
            } else if nz > 0.90 {
                (429, LogLevel::Warn, None)
            } else if nz > 0.84 {
                (401, LogLevel::Warn, None)
            } else if method == "POST" && nz > 0.78 {
                (201, LogLevel::Info, None)
            } else {
                (200, LogLevel::Info, None)
            };
            let mut message = format!("{code} {method} {path}  ·  {total}ms");
            if code != 502 {
                message.push_str(&format!(" (backend {backend}ms)"));
            }
            if let Some(reason) = last_error {
                message.push_str(&format!("  ·  {reason}"));
            }
            let mut fields = vec![
                ("Method", method.to_string()),
                ("Url", format!("https://api.contoso.com{path}")),
                (
                    "BackendUrl",
                    format!("https://ca-checkout-api.internal.contoso.com{path}"),
                ),
                ("ResponseCode", code.to_string()),
                (
                    "BackendResponseCode",
                    if code == 502 {
                        "0".to_string()
                    } else {
                        code.to_string()
                    },
                ),
                ("TotalTime", total.to_string()),
                ("BackendTime", backend.to_string()),
                ("ApiId", api.to_string()),
                ("OperationId", op.to_string()),
                ("CallerIpAddress", "203.0.113.42".to_string()),
                ("Region", "West Europe".to_string()),
                ("IsRequestSuccess", (code < 400).to_string()),
            ];
            if let Some(reason) = last_error {
                fields.push(("LastErrorReason", reason.to_string()));
            }
            line(ts, level, "ApiManagementGatewayLogs", message, fields)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_resource_belongs_to_a_demo_subscription() {
        let subs: Vec<String> = subscriptions().into_iter().map(|s| s.id).collect();
        for r in resources(&[]) {
            assert!(
                subs.contains(&r.subscription_id),
                "{} has unknown subscription {}",
                r.name,
                r.subscription_id
            );
            assert!(r
                .id
                .starts_with(&format!("/subscriptions/{}", r.subscription_id)));
        }
    }

    #[test]
    fn subscription_filter_scopes_resources() {
        let staging = vec![SUB_STAGING.to_string()];
        let rs = resources(&staging);
        assert!(!rs.is_empty());
        assert!(rs.iter().all(|r| r.subscription_id == SUB_STAGING));
        // Empty filter = everything.
        assert!(resources(&[]).len() > rs.len());
    }

    #[test]
    fn sql_resources_cover_both_kinds_belong_to_subs_and_have_metrics() {
        let subs: Vec<String> = subscriptions().into_iter().map(|s| s.id).collect();
        let all = sql_resources(&[]);
        assert!(all.iter().any(|r| r.kind == SqlKind::ElasticPool), "a pool");
        assert!(
            all.iter().any(|r| r.kind == SqlKind::Database),
            "a database"
        );
        // A pooled database points at a pool that exists in the list.
        let pool_ids: Vec<&str> = all
            .iter()
            .filter(|r| r.kind == SqlKind::ElasticPool)
            .map(|r| r.id.as_str())
            .collect();
        for r in &all {
            assert!(subs.contains(&r.subscription_id), "{} bad sub", r.name);
            assert!(r.id.contains("/servers/"), "{} id has no server", r.name);
            if let Some(pool) = r.elastic_pool_id.as_deref() {
                assert!(
                    pool_ids.contains(&pool),
                    "{} points at unknown pool",
                    r.name
                );
            }
            // Every resource yields the four utilization series in demo mode.
            let m = sql_metrics(&r.id, TimeRange::Day);
            assert_eq!(m.series.len(), 4, "{} metric count", r.name);
            assert!(m.series.iter().all(|s| s.unit == "%"));
        }
        // Subscription filter scopes the list.
        let staging = sql_resources(&[SUB_STAGING.to_string()]);
        assert!(!staging.is_empty());
        assert!(staging.iter().all(|r| r.subscription_id == SUB_STAGING));
    }

    #[test]
    fn resources_cover_every_kind_and_are_sorted() {
        let rs = resources(&[]);
        for kind in [
            ResourceKind::FunctionApp,
            ResourceKind::Apim,
            ResourceKind::ContainerApp,
            ResourceKind::AppGateway,
        ] {
            assert!(rs.iter().any(|r| r.kind == kind), "missing {kind:?}");
        }
        let names: Vec<&str> = rs.iter().map(|r| r.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
    }

    #[test]
    fn apim_children_chain_off_the_service_id() {
        let svc = apim_service_id();
        assert!(resources(&[]).iter().any(|r| r.id == svc));
        let apis = apim_apis(&svc);
        assert!(!apis.is_empty());
        for api in &apis {
            assert!(api.id.starts_with(&format!("{svc}/apis/")));
            let ops = apim_operations(&api.id);
            assert!(!ops.is_empty());
            for op in &ops {
                assert!(op.id.starts_with(&format!("{}/operations/", api.id)));
            }
        }
        // Delete has no policy; others do.
        assert!(
            apim_operation_policy(&format!("{svc}/apis/orders-api/operations/delete")).is_none()
        );
        let policy =
            apim_operation_policy(&format!("{svc}/apis/orders-api/operations/list")).unwrap();
        assert!(policy.contains("<policies>"));
    }

    #[test]
    fn metrics_bucket_count_tracks_range_and_missing_map_is_kind_aware() {
        let rs = resources(&[]);
        let apim = rs.iter().find(|r| r.kind == ResourceKind::Apim).unwrap();
        let func = rs
            .iter()
            .find(|r| r.kind == ResourceKind::FunctionApp)
            .unwrap();

        let hour = metrics(func, TimeRange::Hour);
        assert_eq!(hour.series.len(), 4);
        assert!(hour.series.iter().all(|s| s.points.len() == 60));
        assert!(hour.missing.is_empty());

        let day = metrics(apim, TimeRange::Day);
        assert!(day.series.iter().all(|s| s.points.len() == 96));
        assert!(day.missing.contains_key(&MetricKind::Memory));
    }

    #[test]
    fn metrics_are_deterministic_per_resource() {
        let rs = resources(&[]);
        let r = &rs[0];
        let a = metrics(r, TimeRange::Hour);
        let b = metrics(r, TimeRange::Hour);
        let va: Vec<f64> = a.series[0].points.iter().map(|p| p.value).collect();
        let vb: Vec<f64> = b.series[0].points.iter().map(|p| p.value).collect();
        assert_eq!(va, vb);
    }

    #[test]
    fn health_metrics_are_errors_and_traffic_only() {
        let kinds: Vec<MetricKind> = health_metrics("/r/x", ResourceKind::FunctionApp)
            .iter()
            .map(|s| s.kind)
            .collect();
        assert!(kinds.contains(&MetricKind::Errors));
        assert!(kinds.contains(&MetricKind::Traffic));
        assert_eq!(kinds.len(), 2);
    }

    #[test]
    fn logs_match_resource_shape_and_filters() {
        let rs = resources(&[]);
        let apim = rs.iter().find(|r| r.kind == ResourceKind::Apim).unwrap();

        let page = logs(apim, TimeRange::Hour, false, None, None);
        assert!(!page.lines.is_empty());
        assert!(
            !page.has_more,
            "single demo page must read 'window complete'"
        );
        assert!(page
            .lines
            .iter()
            .all(|l| l.source == "ApiManagementGatewayLogs"));
        // Request rows lead with a status code and keep raw columns for detail.
        let first = &page.lines[0];
        assert!(first.message.chars().take(3).all(|c| c.is_ascii_digit()));
        assert!(first.fields.iter().any(|(k, _)| k == "ApiId"));

        // Newest first, like the real `order by TimeGenerated desc`.
        for w in page.lines.windows(2) {
            assert!(w[0].ts >= w[1].ts);
        }

        let errors = logs(apim, TimeRange::Hour, true, None, None);
        assert!(
            !errors.lines.is_empty(),
            "demo data must include some 4xx/5xx"
        );
        assert!(errors
            .lines
            .iter()
            .all(|l| matches!(l.level, LogLevel::Warn | LogLevel::Error)));

        // Pagination cursor terminates immediately.
        let older = logs(apim, TimeRange::Hour, false, Some(Utc::now()), None);
        assert!(older.lines.is_empty());
        assert!(!older.has_more);
    }

    #[test]
    fn demo_collections_are_nonempty_and_sub_scoped() {
        assert_eq!(subscriptions().len(), 2);
        assert!(!storage_accounts(&[]).is_empty());
        assert!(storage_accounts(&[SUB_STAGING.to_string()])
            .iter()
            .all(|a| a.subscription_id == SUB_STAGING));
        assert!(!registries(&[]).is_empty());
        assert!(!cosmos_accounts(&[]).is_empty());
        assert!(!key_vaults(&[]).is_empty());
        assert!(!sb_namespaces(&[]).is_empty());
        assert!(!key_vault_items(ItemKind::Secret).is_empty());
        assert!(!key_vault_items(ItemKind::Certificate).is_empty());
    }

    #[test]
    fn cosmos_and_service_bus_children_reference_parents() {
        let account = &cosmos_accounts(&[])[0];
        for db in cosmos_databases(account) {
            assert!(db.id.starts_with(&account.id));
            for c in cosmos_containers(account, &db.name) {
                assert!(c.id.starts_with(&account.id));
            }
        }
        let ns = &sb_namespaces(&[])[0];
        assert!(sb_queues(ns).iter().all(|q| q.id.starts_with(&ns.id)));
        for t in sb_topics(ns) {
            assert!(t.id.starts_with(&ns.id));
            assert!(sb_subscriptions(ns, &t.name)
                .iter()
                .all(|s| s.id.starts_with(&t.id)));
        }
    }

    #[test]
    fn nothing_in_demo_data_resembles_a_real_tenant_marker() {
        // Belt-and-braces: the demo dataset must never grow strings that look
        // like they came from a real environment. Spot-check the renderable
        // surfaces for the fictional company name instead.
        let everything = format!(
            "{:?}{:?}{:?}{:?}{:?}",
            subscriptions(),
            resources(&[]),
            storage_accounts(&[]),
            key_vaults(&[]),
            sb_namespaces(&[]),
        );
        assert!(everything.to_lowercase().contains("contoso"));
    }
}
