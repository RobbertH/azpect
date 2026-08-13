//! Shared application state. Both the event loop (Lane 3) and the view
//! renderers (Lane 4) read this struct, so it's part of the contract.

#![allow(dead_code, unused_variables)]

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};

use tui_input::Input;

use crate::azure::logs::LogLine;
use crate::azure::metrics::{MetricKind, MetricSeries, TimeRange};
use crate::azure::resources::Resource;
use crate::azure::subscriptions::Subscription;
use crate::config::Config;

/// One of the top-level resource categories the app surfaces. Adding a new
/// resource type means: define a new `Category` variant, list its views via
/// [`Category::contains`], slot it into [`Category::ALL`], and implement
/// [`Category::clear_cache`] / [`Category::reset_root_cursor`] /
/// [`Category::palette_aliases`]. Every other piece of cross-cutting wiring
/// (subscription-switch cache flush, palette routing, `OpenX` action handlers,
/// `last_category` tracking) flows through this enum rather than living as
/// scattered `match state.view` arms.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Category {
    /// Function Apps / APIM / Container Apps / Application Gateways list.
    Apis,
    /// Blob storage accounts and the container / blob / blob-preview chain.
    Storage,
    /// Azure Container Registries and the repository / tag chain.
    Registries,
    /// Cosmos DB (SQL/Core API) accounts and the database / container / items
    /// chain. Reaches the item preview via Cosmos data-plane auth — see
    /// [`crate::azure::cosmos`].
    Cosmos,
    /// Key Vaults and the secrets / certificates drill-in. Data-plane auth via
    /// `https://vault.azure.net/.default`; metadata only — see
    /// [`crate::azure::key_vault`].
    KeyVaults,
    /// Service Bus namespaces and the entities (queues / topics) → subscriptions
    /// chain. Control-plane only — message and dead-letter counts come from the
    /// ARM `countDetails` block, so `Reader` suffices. See
    /// [`crate::azure::service_bus`].
    ServiceBus,
    /// Azure SQL elastic pools + single databases, in one flat list, each with
    /// a utilization-sparkline detail view. Control-plane discovery via
    /// Resource Graph + Azure Monitor metrics. See [`crate::azure::sql`].
    Sql,
}

impl Category {
    /// Every category, in stable order. Iterate this to register palette
    /// commands, flush every category's cache on subscription switch, or list
    /// the categories in help output.
    pub const ALL: &'static [Category] = &[
        Category::Apis,
        Category::Storage,
        Category::Registries,
        Category::Cosmos,
        Category::KeyVaults,
        Category::ServiceBus,
        Category::Sql,
    ];

    /// The top-level list view for this category — the screen the user lands
    /// on after picking a subscription, after pressing the category keybind
    /// (`S` / `R`), or after running the palette command.
    pub fn root_view(self) -> View {
        match self {
            Category::Apis => View::List,
            Category::Storage => View::StorageAccounts,
            Category::Registries => View::Registries,
            Category::Cosmos => View::CosmosAccounts,
            Category::KeyVaults => View::KeyVaults,
            Category::ServiceBus => View::ServiceBusNamespaces,
            Category::Sql => View::SqlResources,
        }
    }

    /// Does `view` live inside this category's drill chain? Used by
    /// idempotency checks ("don't push the root onto the stack if I'm already
    /// inside the chain") and by [`Category::of`].
    pub fn contains(self, view: View) -> bool {
        matches!(
            (self, view),
            (
                Category::Apis,
                View::List
                    | View::Detail
                    | View::Logs
                    | View::LogDetail
                    | View::EnvVars
                    | View::ApimApis
                    | View::ApimOperations
                    | View::ApimPolicy
                    | View::AppGatewayBackends,
            ) | (
                Category::Storage,
                View::StorageAccounts
                    | View::StorageAccountOverview
                    | View::StorageContainers
                    | View::StorageBlobs
                    | View::StorageBlobDetail,
            ) | (
                Category::Registries,
                View::Registries | View::RegistryRepositories | View::RegistryTags,
            ) | (
                Category::Cosmos,
                View::CosmosAccounts
                    | View::CosmosDatabases
                    | View::CosmosContainers
                    | View::CosmosItem,
            ) | (
                Category::KeyVaults,
                View::KeyVaults | View::KeyVaultItems | View::KeyVaultAccessLogs,
            ) | (
                Category::ServiceBus,
                View::ServiceBusNamespaces
                    | View::ServiceBusEntities
                    | View::ServiceBusSubscriptions,
            ) | (
                Category::Sql,
                View::SqlResources
                    | View::SqlDetail
                    | View::SqlAuditPrincipals
                    | View::SqlAuditEvents
                    | View::SqlAuditEventDetail
                    | View::SqlSessions,
            )
        )
    }

    /// Which category does `view` belong to, if any? Returns `None` for
    /// non-category views (`Subscriptions`, `Help`) since they're modal-ish
    /// entry points outside any drill chain.
    pub fn of(view: View) -> Option<Category> {
        Self::ALL.iter().copied().find(|c| c.contains(view))
    }

    /// Reset this category's subscription-scoped cache so the next
    /// `kick_off_loads_for_view` re-fetches against the freshly-pinned
    /// subscription. Called once per category on subscription switch.
    ///
    /// Note: per-resource-id caches that hang off the Apis category (metrics,
    /// logs, health, container_app_overview, apim) are intentionally *not*
    /// cleared here. Their keys are resource ids (subscription-scoped on the
    /// Azure side); they go dormant once `resources` is cleared and are
    /// effectively dead memory until the user re-pins a sub that re-exposes
    /// the same id. Leaving them alone matches the pre-refactor behavior.
    pub fn clear_cache(self, state: &mut AppState) {
        match self {
            Category::Apis => {
                state.resources.clear();
                state.appgw = AppGatewayBackendsCache::default();
                // Any in-flight resource fetch is for the old scope; its result
                // will be dropped by the `ResourcesLoaded` scope guard, so it no
                // longer owns this flag. Left `true`, the flag would debounce
                // away the re-fetch for the *new* scope.
                state.loading_resources = false;
            }
            Category::Storage => {
                state.storage = StorageCache::default();
            }
            Category::Registries => {
                state.registry = RegistryCache::default();
            }
            Category::Cosmos => {
                state.cosmos = CosmosCache::default();
            }
            Category::KeyVaults => {
                state.key_vault = KeyVaultCache::default();
            }
            Category::ServiceBus => {
                state.service_bus = ServiceBusCache::default();
            }
            Category::Sql => {
                state.sql = SqlCache::default();
            }
        }
    }

    /// Reset the cursor on this category's root view. Called by
    /// [`enter_category`] so re-entering a category from elsewhere lands the
    /// cursor at the top instead of stale positions from a prior session.
    pub fn reset_root_cursor(self, state: &mut AppState) {
        match self {
            Category::Apis => state.list_cursor = 0,
            Category::Storage => state.storage.accounts_cursor = 0,
            Category::Registries => state.registry.registries_cursor = 0,
            Category::Cosmos => state.cosmos.accounts_cursor = 0,
            Category::KeyVaults => state.key_vault.vaults_cursor = 0,
            Category::ServiceBus => state.service_bus.namespaces_cursor = 0,
            Category::Sql => state.sql.cursor = 0,
        }
    }

    /// Palette command names (canonical + aliases) that route into this
    /// category. The list is flat — `palette_completion_candidates` uses it
    /// directly, and `run_command` linear-scans it. Keep the canonical (full)
    /// name first so it sorts to the top of Tab completion.
    pub fn palette_aliases(self) -> &'static [&'static str] {
        match self {
            // Single-letter aliases were dropped — they cluttered Tab
            // completion and the keybinds (`S`, `R`) already cover muscle
            // memory for the most common entries.
            Category::Apis => &["apis"],
            Category::Storage => &["storage"],
            Category::Registries => &["registries", "reg", "acr"],
            Category::Cosmos => &["cosmos"],
            Category::KeyVaults => &["keyvaults", "kv", "vaults"],
            Category::ServiceBus => &["servicebus", "sb", "bus"],
            Category::Sql => &["sql", "sqldb", "sqlpools"],
        }
    }
}

/// Mark `category` as the user's active top-level section, route them to its
/// root view (if not already inside the chain), and reset the root cursor.
/// Idempotent inside the chain so repeated presses don't disrupt drill-in
/// state.
///
/// Single source of truth for "enter a resource category": called from the
/// `OpenStorage` / `OpenRegistries` actions and from every category palette
/// command. New resource types added via [`Category`] flow through here
/// automatically.
pub fn enter_category(state: &mut AppState, category: Category) {
    if !category.contains(state.view) {
        state.view = category.root_view();
        category.reset_root_cursor(state);
    }
    state.last_category = category;
}

/// Which screen we are currently rendering.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum View {
    Subscriptions,
    List,
    Detail,
    Logs,
    /// Full-screen detail panel for a single log line. Opened with Enter from
    /// the logs table; reads `LogsCache::scroll` to pick the line.
    LogDetail,
    /// Full-screen, scrollable list of the selected API asset's OS environment
    /// variables (Container App template env / Function App app-settings).
    /// Opened with `e` from Detail. Values are masked by default; `x` reveals
    /// them (k9s-style decode).
    EnvVars,
    /// APIM-only: list of APIs hosted by the selected APIM service. Opened
    /// with Enter from Detail when the resource is `ResourceKind::Apim`.
    ApimApis,
    /// APIM-only: list of operations (routes) on the selected API. Opened
    /// with Enter from `ApimApis`.
    ApimOperations,
    /// APIM-only: policy XML for the selected operation. Opened with Enter
    /// from `ApimOperations`.
    ApimPolicy,
    /// Top-level "Storage" mode entry point — lists storage accounts across the
    /// selected subscriptions. Opened with `S` from any non-modal view.
    StorageAccounts,
    /// Azure-portal-style "account overview" panel for the pinned storage
    /// account: aggregate container / blob / file-share / queue / table counts
    /// and capacity sourced from Azure Monitor metrics. Sits between
    /// `StorageAccounts` and `StorageContainers` in the drill chain — Enter
    /// here opens the containers list. Stats are daily-resolution with a
    /// reporting lag of up to ~24h (same data the portal shows).
    StorageAccountOverview,
    /// List of blob containers under the selected storage account. Opened with
    /// Enter from `StorageAccountOverview`.
    StorageContainers,
    /// List of blobs inside the selected container. Opened with Enter from
    /// `StorageContainers`. Supports `/` to filter by case-insensitive
    /// substring match on the blob name (client-side over the full list).
    StorageBlobs,
    /// Metadata + bounded body preview for the selected blob. Opened with Enter
    /// from `StorageBlobs`.
    StorageBlobDetail,
    /// Top-level "Container registries" mode entry point — lists ACR registries
    /// across the selected subscriptions. Opened with `R` from any non-modal
    /// view.
    Registries,
    /// List of repositories (Docker image names) inside the pinned registry.
    /// Opened with Enter from `Registries`. Data-plane call — requires the
    /// signed-in identity to have `AcrPull` on the registry.
    RegistryRepositories,
    /// List of tags for the pinned repository inside the pinned registry.
    /// Opened with Enter from `RegistryRepositories`. Same data-plane
    /// permission requirement as the repositories list.
    RegistryTags,
    /// Top-level "Cosmos DB" mode entry point — lists SQL/Core API Cosmos
    /// accounts across the selected subscriptions. Opened via the `:cosmos`
    /// palette command (no keybind).
    CosmosAccounts,
    /// List of SQL databases inside the pinned Cosmos account. Opened with
    /// Enter from `CosmosAccounts`. Control-plane call — `Reader` suffices.
    CosmosDatabases,
    /// List of SQL containers (collections) inside the pinned database. Opened
    /// with Enter from `CosmosDatabases`. Control-plane call.
    CosmosContainers,
    /// First-20 item preview for the pinned container. Opened with Enter from
    /// `CosmosContainers`. Data-plane call — requires the signed-in identity
    /// to have `Cosmos DB Built-in Data Reader` at the account scope.
    CosmosItem,
    /// Application-Gateway-only: backend pools (and their members) for the
    /// selected gateway. Opened with Enter from `List` when the resource is
    /// `ResourceKind::AppGateway`.
    AppGatewayBackends,
    /// Top-level "Key Vaults" mode entry point — lists vaults across the
    /// selected subscriptions. Opened via the `:keyvaults` palette command
    /// (no keybind — `V` is unused but reserved in case we want it later).
    KeyVaults,
    /// Metadata-only list of secrets *or* certificates inside the pinned
    /// vault. Opened with Enter from `KeyVaults`. Data-plane call — requires
    /// the signed-in identity to have a `list`-permitting role (RBAC) or
    /// access policy on the vault. Press `s`/`c` to toggle between kinds.
    KeyVaultItems,
    /// Access (audit) log for the pinned vault — who touched what, when, from
    /// where — read from `AzureDiagnostics` `AuditEvent` rows. Opened with `l`
    /// from `KeyVaults` (vault-wide) or from `KeyVaultItems` (pre-scoped to
    /// the selected secret / certificate).
    KeyVaultAccessLogs,
    /// Top-level "Service Bus" mode entry point — lists namespaces across the
    /// selected subscriptions. Opened via the `:servicebus` / `:sb` palette
    /// command (no keybind).
    ServiceBusNamespaces,
    /// Queues *or* topics inside the pinned namespace. Opened with Enter from
    /// `ServiceBusNamespaces`. Control-plane call. Tab / Shift-Tab toggles
    /// between queues and topics. Enter on a topic drills into
    /// `ServiceBusSubscriptions`; queues are terminal.
    ServiceBusEntities,
    /// Subscriptions on the pinned topic, with their active / dead-letter
    /// counts. Opened with Enter from `ServiceBusEntities` while viewing topics.
    ServiceBusSubscriptions,
    /// Top-level "Azure SQL" mode entry point — one flat list of elastic pools
    /// and single databases across the selected subscriptions. Opened via the
    /// `:sql` palette command (no keybind). Enter pins the row and opens
    /// `SqlDetail`.
    SqlResources,
    /// Utilization-sparkline detail for the pinned pool / database: CPU %,
    /// eDTU/DTU %, storage %, workers %. Opened with Enter from `SqlResources`.
    SqlDetail,
    /// SQL audit-log principal roll-up for the pinned pool / database's
    /// server: one row per `server_principal_name` with last-seen / event
    /// counts — answers "does anything still use this login?". Opened with
    /// `l` from `SqlResources` or `SqlDetail`. Requires auditing to forward
    /// to a Log Analytics workspace.
    SqlAuditPrincipals,
    /// Newest audit rows (statements included) for one principal. Opened with
    /// Enter from `SqlAuditPrincipals`.
    SqlAuditEvents,
    /// Full-screen detail for a single audit event: the complete (wrapped)
    /// statement plus `additional_information` — which carries the actual
    /// error for failed events. Opened with Enter from `SqlAuditEvents`.
    SqlAuditEventDetail,
    /// Open sessions on the SQL server / database — who is connected *right
    /// now*, since when, idle how long. Opened with `u` from the SQL views.
    /// ⚠ Backed by **live T-SQL** (`sys.dm_exec_sessions` over TDS), not
    /// REST; gated by the `sql_live_queries` config flag.
    SqlSessions,
    Help,
}

/// Modal overlay for in-app `az login`. Hidden by default; surfaced when the
/// subscription list comes back empty or with an auth-shaped error so the user
/// can re-authenticate without leaving the TUI.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
pub enum AuthPrompt {
    #[default]
    Hidden,
    Menu,
    /// Tenant-id capture step — invoked from `Menu` via `T`. On Enter we go
    /// back to `Menu` with the tenant pre-filled in `auth_tenant`.
    TenantInput,
}

/// Which option is focused in the auth menu. Drives both highlight and Enter.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
pub enum AuthMenuFocus {
    #[default]
    Browser,
    DeviceCode,
    Tenant,
}

/// Captured intent to run `az login`. The event loop drains this between
/// frames, suspends the TUI, runs `az login`, then clears the slot.
#[derive(Clone, Debug)]
pub struct PendingLogin {
    pub tenant: Option<String>,
    pub use_device_code: bool,
}

/// A queued request to shell into a Container App's running container via
/// `az containerapp exec`. Set by the `s` handler and drained by the event loop
/// (which owns the terminal and can safely suspend the TUI), mirroring
/// [`PendingLogin`]. See [`crate::azure::az_exec`].
#[derive(Clone, Debug, Default)]
pub struct PendingExec {
    pub name: String,
    pub resource_group: String,
    pub subscription: Option<String>,
    pub revision: Option<String>,
    pub replica: Option<String>,
    pub container: Option<String>,
}

/// Per-resource cached metrics. The detail view reads these; the loader writes
/// them when a `MetricsReady` event arrives.
#[derive(Clone, Default)]
pub struct MetricsCache {
    pub by_resource: HashMap<String, Vec<MetricSeries>>,
    /// Per-resource, per-metric error messages: which individual metrics didn't
    /// load even when the overall fetch succeeded (e.g. `CpuTime` doesn't exist
    /// on Premium / App Service-plan Function Apps). The detail view uses these
    /// to explain blank sparklines instead of just rendering `—`.
    pub missing: HashMap<String, HashMap<MetricKind, String>>,
    /// Per-resource failure messages. Mutually exclusive with `by_resource`:
    /// a successful fetch removes the resource from `failures`, and vice versa.
    pub failures: HashMap<String, String>,
    /// Resource IDs whose metrics fetch is currently in flight.
    pub pending: HashSet<String>,
    pub range: TimeRange,
    pub loading: bool,
    pub last_error: Option<String>,
}

/// Per-Container-App overview metadata from a single Container App GET: the
/// CPU/memory/ephemeral caps, ingress FQDN, managed environment, managed
/// identity, and primary-container env vars. CPU/memory are rendered alongside
/// the metrics as "latest: X / max Y"; the rest populate the Detail header.
/// Cached per resource id; only populated for `ResourceKind::ContainerApp`.
#[derive(Clone, Default)]
pub struct ContainerAppOverviewCache {
    pub by_resource: HashMap<String, crate::azure::container_app_overview::ContainerAppOverview>,
    pub pending: HashSet<String>,
}

/// Per-Container-App active revision metadata (name, image, replicas, scale)
/// from the revisions endpoint. Populated by the same fetch that drives the
/// health badge for Container Apps.
#[derive(Clone, Default)]
pub struct RevisionMetaCache {
    pub by_resource: HashMap<String, crate::azure::container_app_revisions::ActiveRevisionMeta>,
}

/// Per-Container-App live replica instances (one per running replica) with
/// per-container readiness, restart counts, and start status. Populated by a
/// background fetch fired once the active revision name is known, since the
/// `…/revisions/{rev}/replicas` endpoint needs the revision name in its path.
///
/// `failures` carries the error message for failed fetches (commonly 403 for
/// principals without access to the replicas sub-resource) so the Detail view
/// can show a hint instead of silently dropping the live status section.
#[derive(Clone, Default)]
pub struct ReplicaInstancesCache {
    pub by_resource: HashMap<String, Vec<crate::azure::container_app_replicas::ReplicaInstance>>,
    pub pending: HashSet<String>,
    pub failures: HashMap<String, String>,
}

/// Per-Function-App deployed container image, parsed from `config/web`'s
/// `linuxFxVersion`. Populated by a background fetch kicked off after
/// `ResourcesLoaded` (same eager pattern as [`ContainerAppOverviewCache`]) and
/// consumed by the list's VERSION column. The `Option` value distinguishes
/// "fetched, but the app is code-deployed (no image)" from "not fetched yet".
///
/// (Container Apps don't need a cache here — their deployed image rides on
/// [`RevisionMetaCache`], populated by the same fetch that drives their health badge.)
#[derive(Clone, Default)]
pub struct FuncImageCache {
    pub by_resource: HashMap<String, Option<String>>,
    /// Whether public access is IP/VNet-restricted, per resource — the other
    /// fact read off the same `config/web` fetch. Presence means "known"; an
    /// absent entry means the fetch hasn't landed (or failed), so the Detail
    /// `network:` row shows posture without the restriction detail.
    pub access_restricted: HashMap<String, bool>,
    pub pending: HashSet<String>,
}

/// Per-Function-App application settings (OS env vars) from the
/// `config/appsettings/list` action. Lazily fetched on entering the Detail
/// view. The list action returns secret values, so it can 403 for read-only
/// principals; the failure message is cached so the detail view can show a
/// permission hint instead of silently empty env vars.
///
/// (Container App env vars don't need a cache here — they ride on
/// [`ContainerAppOverviewCache`] since both come from the same Container App GET.)
#[derive(Clone, Default)]
pub struct FuncSettingsCache {
    pub by_resource: HashMap<String, Vec<crate::azure::env_vars::EnvVar>>,
    pub failures: HashMap<String, String>,
    pub pending: HashSet<String>,
}

/// Per-Function-App trigger summary (one [`crate::azure::function_app_triggers::FunctionTrigger`]
/// per function) from the `functions` list. Lazily fetched on entering the
/// Detail view, like [`FuncSettingsCache`]. An empty vec is a legitimate result
/// (no functions synced to ARM); `failures` carries the error so the detail
/// view can show a hint instead of silently dropping the triggers block.
#[derive(Clone, Default)]
pub struct FuncTriggersCache {
    pub by_resource: HashMap<String, Vec<crate::azure::function_app_triggers::FunctionTrigger>>,
    pub failures: HashMap<String, String>,
    pub pending: HashSet<String>,
}

/// Directory principal object-id → display name, resolved best-effort via
/// Microsoft Graph for the `created`/`modified` authorship lines in the Detail
/// view. Only `Application` / `ManagedIdentity` authors (which are GUIDs) get
/// resolved; `User` authors are already UPNs.
#[derive(Clone, Default)]
pub struct PrincipalCache {
    /// object-id → resolved display name.
    pub by_id: HashMap<String, String>,
    /// object-ids we tried and couldn't resolve (no permission / not found);
    /// kept so we don't retry them every render.
    pub failed: HashSet<String>,
    pub pending: HashSet<String>,
}

/// UI state for the dedicated env-vars page. The list itself is read from the
/// per-resource caches ([`ContainerAppOverviewCache`] / [`FuncSettingsCache`])
/// keyed by the selected resource, so this only tracks scroll + reveal.
#[derive(Clone, Default)]
pub struct EnvVarsView {
    pub cursor: usize,
    /// Persisted viewport top for the env-var list, reconciled each render by
    /// [`crate::ui::views::edge_scroll`] (`Cell`: render takes `&AppState`).
    pub view_top: Cell<usize>,
    /// `x` toggles this — when `true`, values are shown instead of masked.
    pub revealed: bool,
    /// Horizontal scroll offset (in characters) for long revealed values.
    /// Advanced with `l`, retreated with `h`; reset when reveal is toggled.
    pub h_offset: usize,
}

/// Which field the env-var editor's cursor is in. Add mode uses both; Edit mode
/// locks the name (you can't rename a setting — that's a delete + add) and keeps
/// focus on the value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvVarField {
    Name,
    Value,
}

/// Whether the guarded editor is changing an existing var or creating one.
#[derive(Clone, PartialEq, Eq)]
pub enum EnvVarEditMode {
    /// Editing an existing var; carries the original value so the confirm step
    /// can show an `old → new` diff and suppress a no-op write.
    Edit { original_value: String },
    /// Adding a brand-new var.
    Add,
}

/// Two-step gate for the env-var editor. `Editing` = typing into the fields;
/// `Confirming` = the final yes/no, with the diff shown and focus defaulting to
/// Cancel so an accidental Enter never commits a write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvVarEditPhase {
    Editing,
    Confirming,
}

/// Live state of the guarded add/edit-env-var flow. Present (`Some`) only while
/// the modal is open; entering it is deliberately gated behind `Ctrl+E` /
/// `Ctrl+N` so a stray keypress can't drop the user into write mode. The actual
/// Azure write fires only on explicit confirm in the `Confirming` phase.
#[derive(Clone)]
pub struct EnvVarEdit {
    pub phase: EnvVarEditPhase,
    pub mode: EnvVarEditMode,
    /// Resource the edit targets — snapshotted so a background refresh that
    /// shuffles the selection can't retarget an in-flight edit.
    pub resource_id: String,
    pub resource_kind: crate::azure::resources::ResourceKind,
    /// Owning container (Container Apps only); `None` for flat Function Apps.
    pub container: Option<String>,
    pub is_init: bool,
    /// Display label for the owning container, echoed in the confirm modal and
    /// reused if the row needs to be inserted optimistically.
    pub attribution: Option<String>,
    /// Editable name field. Read-only (display-only) in `Edit` mode.
    pub name: Input,
    /// Editable value field.
    pub value: Input,
    pub focus: EnvVarField,
    /// Confirm-button focus. Starts `false` (Cancel) every time the confirm step
    /// is entered — the write is opt-in, never the default.
    pub confirm_yes: bool,
    /// `true` once the write has been spawned; freezes input until the
    /// completion event lands so a double-Enter can't fire two writes.
    pub in_flight: bool,
    /// Last write error, shown in the modal so the user can retry or cancel.
    pub error: Option<String>,
}

/// The concrete edit handed to the write spawn and echoed back on completion so
/// the cache can be updated optimistically before the confirming refetch lands.
#[derive(Clone, Debug)]
pub struct AppliedEnvEdit {
    pub resource_id: String,
    pub kind: crate::azure::resources::ResourceKind,
    pub name: String,
    pub value: String,
    pub container: Option<String>,
    pub is_init: bool,
    pub attribution: Option<String>,
}

/// UI state for the Detail view's meta-row navigation and Enter modal. The
/// rows themselves are computed from the various per-resource caches on every
/// render, so this struct only tracks the cursor position into the selectable
/// row list and the modal payload when one is open.
#[derive(Clone, Default)]
pub struct DetailView {
    /// Index into the *selectable* row list (skeleton placeholders skip past).
    /// Clamped at render time when the row count shrinks. Resets to 0 each time
    /// the user enters Detail from a different view so a fresh resource starts
    /// at the top.
    pub cursor: usize,
    /// `Some(_)` when a row's Enter modal is open. Cleared on Esc / Back.
    pub modal: Option<DetailModal>,
}

/// One Enter-opened detail modal payload. Snapshot of the row's full content
/// at the time the modal was opened — not a live view on the cache — so
/// background refreshes during the modal's lifetime won't shuffle the text
/// out from under the reader.
#[derive(Clone, Default)]
pub struct DetailModal {
    /// Window title (e.g. `replica · …r58pz` or `image`).
    pub title: String,
    /// Body rendered one line per element. Each line is wrapped on the
    /// modal's width, so callers don't need to pre-wrap.
    pub lines: Vec<String>,
    /// Vertical scroll offset (in rendered rows) inside the modal body.
    pub scroll: u16,
}

/// Per-resource cached Azure Resource Health availability. Populated by a
/// background fetch kicked off after `ResourcesLoaded`; consumed by
/// `azure::health::derive` to outrank the metric-derived heuristic.
#[derive(Clone, Default)]
pub struct HealthCache {
    /// keyed by resource id
    pub by_resource: HashMap<String, crate::azure::resource_health::ResourceAvailability>,
    /// IDs whose fetch is currently in-flight, so we don't double-spawn.
    pub pending: HashSet<String>,
    /// Per-resource failure messages.
    pub failures: HashMap<String, String>,
    /// Fixed-24h Errors+Traffic series feeding the badge verdict, independent of
    /// the chart's selected range. Fetched alongside `by_resource` (availability)
    /// in `spawn_load_health`; consumed by `azure::health::derive`.
    pub metrics: HashMap<String, Vec<MetricSeries>>,
    /// Per-resource health-metrics failure messages (all metric calls failed).
    pub metrics_failures: HashMap<String, String>,
}

/// State for the APIM drill-in views: APIs list (per APIM service), operations
/// list (per API), and policy XML (per operation). Three caches are bundled
/// together because they share a navigation chain and the same fetch/error
/// shape, and isolating each into its own AppState field would just spam the
/// struct.
#[derive(Clone, Default)]
pub struct ApimCache {
    /// Keyed by APIM service resource id.
    pub apis: HashMap<String, Vec<crate::azure::apim::Api>>,
    pub apis_pending: HashSet<String>,
    pub apis_error: HashMap<String, String>,
    pub apis_cursor: usize,
    /// Persisted viewport top for the APIs list — see
    /// [`crate::ui::views::edge_scroll`].
    pub apis_view_top: Cell<usize>,
    /// `/`-search over the APIs list. `/` focuses on an empty input (a
    /// previously committed value is discarded, vim-style), Enter commits
    /// (value persists until the next `/`), Esc cancels and clears. Mirrors
    /// the storage/registry filters.
    pub apis_filter: Input,
    pub apis_filter_active: bool,

    /// Keyed by API resource id (`{service}/apis/{apiName}`).
    pub operations: HashMap<String, Vec<crate::azure::apim::Operation>>,
    pub operations_pending: HashSet<String>,
    pub operations_error: HashMap<String, String>,
    pub operations_cursor: usize,
    /// Persisted viewport top for the operations list — see
    /// [`crate::ui::views::edge_scroll`].
    pub operations_view_top: Cell<usize>,
    /// `/`-search over the operations list. Same shape as `apis_filter`.
    pub operations_filter: Input,
    pub operations_filter_active: bool,
    /// Which API the operations view is currently drilling into. Pinned when
    /// the user opens an API from `ApimApis` so navigating back to that view
    /// doesn't lose track. `None` outside the operations/policy views.
    pub selected_api_id: Option<String>,

    /// Keyed by operation resource id. `None` value = APIM returned 404 (no
    /// policy configured); `Some(xml)` = the raw policy document.
    pub policy: HashMap<String, Option<String>>,
    pub policy_pending: HashSet<String>,
    pub policy_error: HashMap<String, String>,
    pub policy_scroll: u16,
    pub selected_operation_id: Option<String>,
}

impl ApimCache {
    /// Apply `apis_filter` to the APIs under `service_id`. Case-insensitive
    /// substring match over display name, gateway path, and slug; empty filter
    /// returns everything. Mirrors `StorageCache::filtered_blobs`.
    pub fn filtered_apis(&self, service_id: &str) -> Vec<&crate::azure::apim::Api> {
        let needle = self.apis_filter.value().to_lowercase();
        match self.apis.get(service_id) {
            Some(rows) => rows
                .iter()
                .filter(|a| {
                    needle.is_empty()
                        || a.display_name.to_lowercase().contains(&needle)
                        || a.path.to_lowercase().contains(&needle)
                        || a.name.to_lowercase().contains(&needle)
                })
                .collect(),
            None => Vec::new(),
        }
    }

    /// Apply `operations_filter` to the operations under `api_id`.
    /// Case-insensitive substring match over display name, URL template, HTTP
    /// method, and slug; empty filter returns everything.
    pub fn filtered_operations(&self, api_id: &str) -> Vec<&crate::azure::apim::Operation> {
        let needle = self.operations_filter.value().to_lowercase();
        match self.operations.get(api_id) {
            Some(rows) => rows
                .iter()
                .filter(|o| {
                    needle.is_empty()
                        || o.display_name.to_lowercase().contains(&needle)
                        || o.url_template.to_lowercase().contains(&needle)
                        || o.method.to_lowercase().contains(&needle)
                        || o.name.to_lowercase().contains(&needle)
                })
                .collect(),
            None => Vec::new(),
        }
    }
}

/// State for the Application Gateway backends drill-in view: a single map of
/// pools keyed by gateway resource id, with the usual pending / error / cursor
/// sidecars. The view is one level deep (gateway → pools+members) so there's
/// no parent selection to pin beyond the cursor in the resource list itself.
#[derive(Clone, Default)]
pub struct AppGatewayBackendsCache {
    /// Keyed by Application Gateway resource id.
    pub pools: HashMap<String, Vec<crate::azure::appgw_backends::BackendPool>>,
    pub pools_pending: HashSet<String>,
    pub pools_error: HashMap<String, String>,
    /// Cursor into the *flattened* row list rendered by the view (pool header
    /// rows + member rows interleaved). Lives here so it survives refresh /
    /// re-entry.
    pub cursor: usize,
    /// Persisted viewport top for the flattened row list — see
    /// [`crate::ui::views::edge_scroll`].
    pub view_top: Cell<usize>,
}

/// State for the Storage drill-in views. Mirrors `ApimCache`: each level of
/// the chain (accounts → containers → blobs → blob preview) has its own
/// in-memory cache, pending set, and per-key error map so the views can render
/// loading / error / data states without re-fetching on every frame.
#[derive(Clone, Default)]
pub struct StorageCache {
    /// All storage accounts discovered for the current subscription scope.
    /// Wrapped in `Option` so the view can distinguish "never fetched" from
    /// "fetched and empty".
    pub accounts: Option<Vec<crate::azure::storage::StorageAccount>>,
    pub accounts_pending: bool,
    pub accounts_error: Option<String>,
    pub accounts_cursor: usize,
    /// Persisted table scroll offset, written back after render so the window
    /// only moves when the cursor pushes against an edge (see the table
    /// views' `TableState` wiring).
    pub accounts_view_top: Cell<usize>,
    /// Client-side case-insensitive substring filter applied to the accounts
    /// list by name. Mirrors `list_filter` + `list_filter_active` so the input
    /// forwarding code in `app.rs` can route keystrokes to the right buffer.
    /// Empty value → all accounts pass.
    pub accounts_filter: Input,
    pub accounts_filter_active: bool,
    /// Pinned account the user drilled into. Carried across the rest of the
    /// chain so per-resource fetches keep using the same account name even if
    /// the cursor in `StorageAccounts` moves.
    pub selected_account: Option<crate::azure::storage::StorageAccount>,

    /// Per-account aggregated stats shown by `StorageAccountOverview` (the
    /// Azure-portal "account overview" panel). Keyed by ARM id. Populated
    /// lazily on first entry and *only* refetched on explicit `r` because the
    /// underlying Azure Monitor metrics update at most a few times per day.
    pub overview_stats: HashMap<String, crate::azure::storage::StorageAccountStats>,
    pub overview_pending: HashSet<String>,
    pub overview_error: HashMap<String, String>,

    /// Keyed by storage account resource id.
    pub containers: HashMap<String, Vec<crate::azure::storage::BlobContainer>>,
    pub containers_pending: HashSet<String>,
    pub containers_error: HashMap<String, String>,
    pub containers_cursor: usize,
    /// Persisted table scroll offset — see `accounts_view_top`.
    pub containers_view_top: Cell<usize>,
    /// Client-side case-insensitive substring filter applied to the containers
    /// list by name. Mirrors `accounts_filter` + `accounts_filter_active` so
    /// the input forwarding code in `app.rs` can route keystrokes to the right
    /// buffer. Empty value → all containers pass.
    pub containers_filter: Input,
    pub containers_filter_active: bool,
    /// Pinned container name the user drilled into.
    pub selected_container: Option<String>,

    /// Keyed by `"{account_name}/{container_name}"`. The full set of blobs in
    /// the container is fetched once and filtered client-side, so the prefix
    /// is no longer a cache-key dimension.
    pub blobs: HashMap<String, Vec<crate::azure::storage::Blob>>,
    pub blobs_pending: HashSet<String>,
    pub blobs_error: HashMap<String, String>,
    pub blobs_cursor: usize,
    /// Persisted table scroll offset — see `accounts_view_top`.
    pub blobs_view_top: Cell<usize>,
    /// Client-side case-insensitive substring filter applied to the blobs list
    /// by name. Mirrors `accounts_filter` + `accounts_filter_active` so the
    /// input forwarding code in `app.rs` can route keystrokes to the right
    /// buffer. Empty value → all blobs pass.
    pub blobs_filter: Input,
    pub blobs_filter_active: bool,
    /// Pinned blob name the user drilled into.
    pub selected_blob: Option<String>,

    /// Keyed by `"{account_name}/{container_name}/{blob_name}"`.
    pub blob_preview: HashMap<String, crate::azure::storage::BlobPreview>,
    pub blob_preview_pending: HashSet<String>,
    pub blob_preview_error: HashMap<String, String>,
    pub blob_preview_scroll: u16,
}

impl StorageCache {
    /// Cache key for the blobs map. The full container is fetched once and
    /// filtered client-side, so the key is just `{account}/{container}` — no
    /// prefix dimension.
    pub fn blobs_key(account: &str, container: &str) -> String {
        format!("{account}/{container}")
    }

    /// Cache key for the blob preview map.
    pub fn blob_preview_key(account: &str, container: &str, blob: &str) -> String {
        format!("{account}/{container}/{blob}")
    }

    /// Apply `accounts_filter` to `accounts` as a case-insensitive substring
    /// match on the account name. Returns an empty vector when accounts
    /// haven't been fetched yet. An empty filter passes everything through.
    pub fn filtered_accounts(&self) -> Vec<&crate::azure::storage::StorageAccount> {
        let needle = self.accounts_filter.value().to_lowercase();
        match self.accounts.as_ref() {
            Some(rows) => rows
                .iter()
                .filter(|a| needle.is_empty() || a.name.to_lowercase().contains(&needle))
                .collect(),
            None => Vec::new(),
        }
    }

    /// Apply `containers_filter` to the containers under `account_id` as a
    /// case-insensitive substring match on the container name. Returns an
    /// empty vector when containers haven't been fetched yet for this
    /// account. An empty filter passes everything through.
    pub fn filtered_containers(
        &self,
        account_id: &str,
    ) -> Vec<&crate::azure::storage::BlobContainer> {
        let needle = self.containers_filter.value().to_lowercase();
        match self.containers.get(account_id) {
            Some(rows) => rows
                .iter()
                .filter(|c| needle.is_empty() || c.name.to_lowercase().contains(&needle))
                .collect(),
            None => Vec::new(),
        }
    }

    /// Apply `blobs_filter` to the blobs under `(account, container)` as a
    /// case-insensitive substring match on the blob name. Returns an empty
    /// vector when blobs haven't been fetched yet. An empty filter passes
    /// everything through.
    pub fn filtered_blobs(
        &self,
        account: &str,
        container: &str,
    ) -> Vec<&crate::azure::storage::Blob> {
        let needle = self.blobs_filter.value().to_lowercase();
        let key = Self::blobs_key(account, container);
        match self.blobs.get(&key) {
            Some(rows) => rows
                .iter()
                .filter(|b| needle.is_empty() || b.name.to_lowercase().contains(&needle))
                .collect(),
            None => Vec::new(),
        }
    }
}

/// State for the Container Registries drill-in views. Mirrors `StorageCache`
/// (minus the per-account-overview metrics layer, which ACR doesn't have):
/// each level of the chain (registries → repositories → tags) has its own
/// in-memory cache, pending set, error map, cursor, and a client-side filter.
#[derive(Clone, Default)]
pub struct RegistryCache {
    /// All registries discovered for the current subscription scope. Wrapped
    /// in `Option` so the view can distinguish "never fetched" from
    /// "fetched and empty".
    pub registries: Option<Vec<crate::azure::registries::Registry>>,
    pub registries_pending: bool,
    pub registries_error: Option<String>,
    pub registries_cursor: usize,
    /// Persisted table scroll offset — see `StorageCache::accounts_view_top`.
    pub registries_view_top: Cell<usize>,
    /// Client-side case-insensitive substring filter applied to the registries
    /// list by name. Mirrors the storage filter inputs.
    pub registries_filter: Input,
    pub registries_filter_active: bool,
    /// Pinned registry the user drilled into.
    pub selected_registry: Option<crate::azure::registries::Registry>,

    /// Keyed by registry resource id.
    pub repositories: HashMap<String, Vec<crate::azure::registries::Repository>>,
    pub repositories_pending: HashSet<String>,
    pub repositories_error: HashMap<String, String>,
    pub repositories_cursor: usize,
    /// Persisted table scroll offset — see `StorageCache::accounts_view_top`.
    pub repositories_view_top: Cell<usize>,
    pub repositories_filter: Input,
    pub repositories_filter_active: bool,
    /// Pinned repository name the user drilled into.
    pub selected_repository: Option<String>,

    /// Keyed by `"{registry_id}/{repository_name}"`.
    pub tags: HashMap<String, Vec<crate::azure::registries::Tag>>,
    pub tags_pending: HashSet<String>,
    pub tags_error: HashMap<String, String>,
    pub tags_cursor: usize,
    /// Persisted table scroll offset — see `StorageCache::accounts_view_top`.
    pub tags_view_top: Cell<usize>,
    pub tags_filter: Input,
    pub tags_filter_active: bool,
}

impl RegistryCache {
    /// Cache key for the tags map.
    pub fn tags_key(registry_id: &str, repository: &str) -> String {
        format!("{registry_id}/{repository}")
    }

    /// Apply `registries_filter` to `registries` as a case-insensitive
    /// substring match on the registry name. Returns an empty vector when
    /// registries haven't been fetched yet. An empty filter passes everything
    /// through.
    pub fn filtered_registries(&self) -> Vec<&crate::azure::registries::Registry> {
        let needle = self.registries_filter.value().to_lowercase();
        match self.registries.as_ref() {
            Some(rows) => rows
                .iter()
                .filter(|r| needle.is_empty() || r.name.to_lowercase().contains(&needle))
                .collect(),
            None => Vec::new(),
        }
    }

    /// Apply `repositories_filter` to the repositories under `registry_id` as
    /// a case-insensitive substring match on the repository name.
    pub fn filtered_repositories(
        &self,
        registry_id: &str,
    ) -> Vec<&crate::azure::registries::Repository> {
        let needle = self.repositories_filter.value().to_lowercase();
        match self.repositories.get(registry_id) {
            Some(rows) => rows
                .iter()
                .filter(|r| needle.is_empty() || r.name.to_lowercase().contains(&needle))
                .collect(),
            None => Vec::new(),
        }
    }

    /// Apply `tags_filter` to the tags under `(registry_id, repository)` as a
    /// case-insensitive substring match on the tag name.
    pub fn filtered_tags(
        &self,
        registry_id: &str,
        repository: &str,
    ) -> Vec<&crate::azure::registries::Tag> {
        let needle = self.tags_filter.value().to_lowercase();
        let key = Self::tags_key(registry_id, repository);
        match self.tags.get(&key) {
            Some(rows) => rows
                .iter()
                .filter(|t| needle.is_empty() || t.name.to_lowercase().contains(&needle))
                .collect(),
            None => Vec::new(),
        }
    }
}

/// State for the Cosmos DB drill-in views (SQL/Core API only). Same layered
/// shape as [`RegistryCache`] / [`StorageCache`], extended one level deep so
/// the item preview can live in the same per-key cache discipline as the
/// blob preview. The data plane (only the item-preview tier) carries its own
/// auth via `SCOPE_COSMOS` — the first three tiers all flow through ARM.
#[derive(Clone, Default)]
pub struct CosmosCache {
    /// All Cosmos accounts discovered for the current subscription scope.
    /// Wrapped in `Option` so the view can distinguish "never fetched" from
    /// "fetched and empty".
    pub accounts: Option<Vec<crate::azure::cosmos::CosmosAccount>>,
    pub accounts_pending: bool,
    pub accounts_error: Option<String>,
    pub accounts_cursor: usize,
    /// Persisted table scroll offset — see `StorageCache::accounts_view_top`.
    pub accounts_view_top: Cell<usize>,
    pub accounts_filter: Input,
    pub accounts_filter_active: bool,
    /// Pinned account the user drilled into.
    pub selected_account: Option<crate::azure::cosmos::CosmosAccount>,

    /// Keyed by account resource id.
    pub databases: HashMap<String, Vec<crate::azure::cosmos::CosmosDatabase>>,
    pub databases_pending: HashSet<String>,
    pub databases_error: HashMap<String, String>,
    pub databases_cursor: usize,
    /// Persisted table scroll offset — see `StorageCache::accounts_view_top`.
    pub databases_view_top: Cell<usize>,
    pub databases_filter: Input,
    pub databases_filter_active: bool,
    /// Pinned database name the user drilled into.
    pub selected_database: Option<String>,

    /// Keyed by `"{account_id}/{db_name}"`.
    pub containers: HashMap<String, Vec<crate::azure::cosmos::CosmosContainer>>,
    pub containers_pending: HashSet<String>,
    pub containers_error: HashMap<String, String>,
    pub containers_cursor: usize,
    /// Persisted table scroll offset — see `StorageCache::accounts_view_top`.
    pub containers_view_top: Cell<usize>,
    pub containers_filter: Input,
    pub containers_filter_active: bool,
    /// Pinned container name the user drilled into.
    pub selected_container: Option<String>,

    /// Keyed by `"{account_id}/{db_name}/{coll_name}"`.
    pub items: HashMap<String, crate::azure::cosmos::CosmosItemPreview>,
    pub items_pending: HashSet<String>,
    pub items_error: HashMap<String, String>,
    /// Vertical scroll offset inside the item preview pane.
    pub items_scroll: u16,
}

impl CosmosCache {
    /// Cache key for the containers map: `{account_id}/{db}`.
    pub fn containers_key(account_id: &str, db: &str) -> String {
        format!("{account_id}/{db}")
    }

    /// Cache key for the items map: `{account_id}/{db}/{coll}`.
    pub fn items_key(account_id: &str, db: &str, coll: &str) -> String {
        format!("{account_id}/{db}/{coll}")
    }

    /// Apply `accounts_filter` to `accounts` as a case-insensitive substring
    /// match on the account name. Empty filter passes everything through.
    pub fn filtered_accounts(&self) -> Vec<&crate::azure::cosmos::CosmosAccount> {
        let needle = self.accounts_filter.value().to_lowercase();
        match self.accounts.as_ref() {
            Some(rows) => rows
                .iter()
                .filter(|a| needle.is_empty() || a.name.to_lowercase().contains(&needle))
                .collect(),
            None => Vec::new(),
        }
    }

    /// Apply `databases_filter` to the databases under `account_id`.
    pub fn filtered_databases(
        &self,
        account_id: &str,
    ) -> Vec<&crate::azure::cosmos::CosmosDatabase> {
        let needle = self.databases_filter.value().to_lowercase();
        match self.databases.get(account_id) {
            Some(rows) => rows
                .iter()
                .filter(|d| needle.is_empty() || d.name.to_lowercase().contains(&needle))
                .collect(),
            None => Vec::new(),
        }
    }

    /// Apply `containers_filter` to the containers under `(account_id, db)`.
    pub fn filtered_containers(
        &self,
        account_id: &str,
        db: &str,
    ) -> Vec<&crate::azure::cosmos::CosmosContainer> {
        let needle = self.containers_filter.value().to_lowercase();
        let key = Self::containers_key(account_id, db);
        match self.containers.get(&key) {
            Some(rows) => rows
                .iter()
                .filter(|c| needle.is_empty() || c.name.to_lowercase().contains(&needle))
                .collect(),
            None => Vec::new(),
        }
    }
}

/// State for the Key Vault drill-in views. Two levels deep: vaults → items
/// (secrets / certs / — keys deferred). Same per-key cache discipline as
/// [`RegistryCache`]; the items map is keyed by `(vault_id, kind)` so the two
/// kinds coexist in the cache and toggling `s`/`c` doesn't blow away the
/// other side.
#[derive(Clone, Default)]
pub struct KeyVaultCache {
    /// All Key Vaults discovered for the current subscription scope. Wrapped
    /// in `Option` so the view can distinguish "never fetched" from "fetched
    /// and empty".
    pub vaults: Option<Vec<crate::azure::key_vault::KeyVault>>,
    pub vaults_pending: bool,
    pub vaults_error: Option<String>,
    pub vaults_cursor: usize,
    /// Persisted table scroll offset — see `StorageCache::accounts_view_top`.
    pub vaults_view_top: Cell<usize>,
    pub vaults_filter: Input,
    pub vaults_filter_active: bool,
    /// Pinned vault the user drilled into.
    pub selected_vault: Option<crate::azure::key_vault::KeyVault>,

    /// Which item kind the items view is currently showing for the pinned
    /// vault. Toggled with `s` (secrets) / `c` (certificates).
    pub items_kind: crate::azure::key_vault::ItemKind,

    /// Where Esc from the items view should land when the user got there by
    /// following a Key Vault reference (`x` on a secret-backed env var) rather
    /// than by drilling in from the vaults list. The semantic parent of
    /// `KeyVaultItems` is `KeyVaults` — a view a ref-follower never visited
    /// (and which sits unloaded, showing "press r") — so the ref-jump records
    /// its origin here and `Action::Back` consumes it. Cleared on a normal
    /// vaults-list drill-in.
    pub items_return_view: Option<View>,

    /// Keyed by `(vault_id, kind)` via [`Self::items_key`]. Holds both
    /// secrets and certs simultaneously so the kind toggle is instant if the
    /// other side is already cached.
    pub items: HashMap<String, Vec<crate::azure::key_vault::KeyVaultItem>>,
    pub items_pending: HashSet<String>,
    pub items_error: HashMap<String, String>,
    pub items_cursor: usize,
    /// Persisted table scroll offset — see `StorageCache::accounts_view_top`.
    pub items_view_top: Cell<usize>,
    pub items_filter: Input,
    pub items_filter_active: bool,

    /// `Some(_)` while a secret-value reveal modal is open (Enter / `x` on a
    /// secret row). Holds the async fetch lifecycle so the modal can show a
    /// spinner, the decoded value, or an error — and so the plaintext never
    /// touches the list cache. Cleared on Esc / close.
    pub secret_modal: Option<SecretModal>,

    // -- access-logs view (`l` on a vault or on a specific item) -----------
    /// Fetched `AuditEvent` rows for the pinned vault under the *current*
    /// query scope (window / item / exclude-me). Dropped whenever that scope
    /// changes — the scope is part of the query, not a client-side filter.
    pub access_events: Option<Vec<crate::azure::key_vault_logs::AccessEvent>>,
    pub access_pending: bool,
    pub access_error: Option<String>,
    pub access_cursor: usize,
    /// Persisted table scroll offset — see `StorageCache::accounts_view_top`.
    pub access_view_top: Cell<usize>,
    /// Monotonic fetch-scope token, same idea as `LogsCache::generation`: a
    /// landing page whose generation is stale is discarded, so rapid filter
    /// toggles stay deterministic.
    pub access_generation: u64,
    /// Query time window; `0`/`1`/`7` keys plus a free-form custom entry
    /// (`t`, e.g. "6m" / "1y").
    pub access_window: crate::azure::key_vault_logs::AccessWindow,
    /// Server-side "exclude me" (your UPN / sign-in IP from the token
    /// claims). Toggled with `m`.
    pub access_exclude_self: bool,
    /// Who the current page actually hid (returned by the fetch), for the
    /// header chip.
    pub access_hidden: Option<crate::azure::key_vault_logs::SelfIdentity>,
    /// Scope the query to one secret / certificate — set when the view was
    /// opened with `l` on an item row rather than on the vault.
    pub access_scope: Option<crate::azure::key_vault_logs::ItemScope>,
    /// Client-side `OperationName` filter (`SecretGet`, …), cycled with
    /// Tab / Shift+Tab through the distinct operations in the fetched page.
    pub access_operation: Option<String>,
    /// The page came back at the row cap — older rows in the window exist.
    pub access_truncated: bool,
    /// Where Esc should land: `KeyVaultItems` when opened from an item row,
    /// `KeyVaults` when opened from the vaults list.
    pub access_return_view: Option<View>,
    /// Free-form custom-window input (`t`), e.g. "6m". While active, raw
    /// keystrokes flow into it; Enter parses + applies, Esc cancels.
    pub access_window_input: Input,
    pub access_window_input_active: bool,
}

/// Payload for the secret-value reveal modal. The value is fetched on demand
/// (see [`crate::azure::key_vault::get_secret_value`]) and lives only here, for
/// the modal's lifetime — closing the modal drops it.
#[derive(Clone)]
pub struct SecretModal {
    /// Vault the secret belongs to — matched against the async result so a
    /// stale fetch can't populate a modal the user reopened on another secret.
    pub vault_id: String,
    /// Secret name (also the modal title).
    pub name: String,
    pub status: SecretRevealStatus,
    /// Vertical scroll offset inside the modal body (long values wrap).
    pub scroll: u16,
}

/// Lifecycle of a secret-value reveal.
#[derive(Clone)]
pub enum SecretRevealStatus {
    /// Fetch in flight.
    Loading,
    /// Decoded plaintext value.
    Loaded(String),
    /// Fetch failed (e.g. the identity has `list` but not `get`).
    Error(String),
}

impl KeyVaultCache {
    /// Cache key for the items map: `{vault_id}/{kind.path_segment()}`. The
    /// kind segment is fixed-string (`secrets`/`certificates`) so the key is
    /// stable across the toggle.
    pub fn items_key(vault_id: &str, kind: crate::azure::key_vault::ItemKind) -> String {
        format!("{vault_id}/{}", kind.path_segment())
    }

    /// Apply `vaults_filter` to `vaults` as a case-insensitive substring match
    /// on the vault name. Empty filter passes everything through.
    pub fn filtered_vaults(&self) -> Vec<&crate::azure::key_vault::KeyVault> {
        let needle = self.vaults_filter.value().to_lowercase();
        match self.vaults.as_ref() {
            Some(rows) => rows
                .iter()
                .filter(|v| needle.is_empty() || v.name.to_lowercase().contains(&needle))
                .collect(),
            None => Vec::new(),
        }
    }

    /// Apply `items_filter` to the items under `(vault_id, items_kind)` as a
    /// case-insensitive substring match on the item name.
    pub fn filtered_items(&self, vault_id: &str) -> Vec<&crate::azure::key_vault::KeyVaultItem> {
        let needle = self.items_filter.value().to_lowercase();
        let key = Self::items_key(vault_id, self.items_kind);
        match self.items.get(&key) {
            Some(rows) => rows
                .iter()
                .filter(|i| needle.is_empty() || i.name.to_lowercase().contains(&needle))
                .collect(),
            None => Vec::new(),
        }
    }

    /// Access events with the client-side `access_operation` filter applied —
    /// the view the access-logs table (and its cursor) indexes into.
    pub fn visible_access_events(&self) -> Vec<&crate::azure::key_vault_logs::AccessEvent> {
        match self.access_events.as_ref() {
            Some(rows) => rows
                .iter()
                .filter(|e| {
                    self.access_operation
                        .as_deref()
                        .is_none_or(|op| e.operation == op)
                })
                .collect(),
            None => Vec::new(),
        }
    }

    /// Distinct `OperationName` values in the fetched page, sorted — the Tab
    /// cycle order for the operation filter.
    pub fn access_operations(&self) -> Vec<String> {
        let mut ops: Vec<String> = self
            .access_events
            .iter()
            .flatten()
            .map(|e| e.operation.clone())
            .collect();
        ops.sort();
        ops.dedup();
        ops
    }

    /// Reset the access-logs view for entering it fresh on `vault` (scoped to
    /// `scope` when opened from an item row). Filters and window survive
    /// re-entry on the *same* vault+scope only when the buffer does; a scope
    /// change always starts clean so stale rows can't masquerade as current.
    pub fn enter_access_view(
        &mut self,
        scope: Option<crate::azure::key_vault_logs::ItemScope>,
        return_view: View,
    ) {
        self.access_events = None;
        self.access_error = None;
        self.access_pending = false;
        self.access_cursor = 0;
        self.access_view_top.set(0);
        self.access_scope = scope;
        self.access_operation = None;
        self.access_truncated = false;
        self.access_return_view = Some(return_view);
        self.access_window_input.reset();
        self.access_window_input_active = false;
        self.access_generation = self.access_generation.wrapping_add(1);
    }
}

/// State for the Service Bus drill-in views. Three levels: namespaces →
/// entities (queues / topics, toggled with Tab) → subscriptions (topics only).
/// Same per-key cache discipline as [`CosmosCache`]. Queues and topics share
/// the `entities_cursor` / `entities_filter` (mirroring the Key Vault
/// secrets/certs toggle) but live in separate maps because their row types
/// differ. Everything is control-plane — no second auth tier.
#[derive(Clone, Default)]
pub struct ServiceBusCache {
    /// All namespaces discovered for the current subscription scope. Wrapped in
    /// `Option` so the view can distinguish "never fetched" from "fetched and
    /// empty".
    pub namespaces: Option<Vec<crate::azure::service_bus::ServiceBusNamespace>>,
    pub namespaces_pending: bool,
    pub namespaces_error: Option<String>,
    pub namespaces_cursor: usize,
    /// Persisted table scroll offset — see `StorageCache::accounts_view_top`.
    pub namespaces_view_top: Cell<usize>,
    pub namespaces_filter: Input,
    pub namespaces_filter_active: bool,
    /// Pinned namespace the user drilled into.
    pub selected_namespace: Option<crate::azure::service_bus::ServiceBusNamespace>,

    /// Which entity kind the entities view is currently showing. Toggled with
    /// Tab / Shift-Tab.
    pub entity_kind: crate::azure::service_bus::EntityKind,
    /// Shared cursor + filter across the queue and topic lists; reset on toggle.
    pub entities_cursor: usize,
    /// Persisted table scroll offset — see `StorageCache::accounts_view_top`.
    pub entities_view_top: Cell<usize>,
    pub entities_filter: Input,
    pub entities_filter_active: bool,

    /// Queues keyed by namespace resource id.
    pub queues: HashMap<String, Vec<crate::azure::service_bus::ServiceBusQueue>>,
    pub queues_pending: HashSet<String>,
    pub queues_error: HashMap<String, String>,

    /// Topics keyed by namespace resource id.
    pub topics: HashMap<String, Vec<crate::azure::service_bus::ServiceBusTopic>>,
    pub topics_pending: HashSet<String>,
    pub topics_error: HashMap<String, String>,

    /// Pinned topic name the user drilled into (for the subscriptions view).
    pub selected_topic: Option<String>,

    /// Subscriptions keyed by `"{namespace_id}/{topic_name}"`.
    pub subscriptions: HashMap<String, Vec<crate::azure::service_bus::ServiceBusSubscription>>,
    pub subscriptions_pending: HashSet<String>,
    pub subscriptions_error: HashMap<String, String>,
    pub subscriptions_cursor: usize,
    /// Persisted table scroll offset — see `StorageCache::accounts_view_top`.
    pub subscriptions_view_top: Cell<usize>,
    pub subscriptions_filter: Input,
    pub subscriptions_filter_active: bool,
}

impl ServiceBusCache {
    /// Cache key for the subscriptions map: `{namespace_id}/{topic}`.
    pub fn subscriptions_key(namespace_id: &str, topic: &str) -> String {
        format!("{namespace_id}/{topic}")
    }

    /// Apply `namespaces_filter` to `namespaces` as a case-insensitive
    /// substring match on the namespace name. Empty filter passes everything.
    pub fn filtered_namespaces(&self) -> Vec<&crate::azure::service_bus::ServiceBusNamespace> {
        let needle = self.namespaces_filter.value().to_lowercase();
        match self.namespaces.as_ref() {
            Some(rows) => rows
                .iter()
                .filter(|n| needle.is_empty() || n.name.to_lowercase().contains(&needle))
                .collect(),
            None => Vec::new(),
        }
    }

    /// Apply `entities_filter` to the queues under `namespace_id`.
    pub fn filtered_queues(
        &self,
        namespace_id: &str,
    ) -> Vec<&crate::azure::service_bus::ServiceBusQueue> {
        let needle = self.entities_filter.value().to_lowercase();
        match self.queues.get(namespace_id) {
            Some(rows) => rows
                .iter()
                .filter(|q| needle.is_empty() || q.name.to_lowercase().contains(&needle))
                .collect(),
            None => Vec::new(),
        }
    }

    /// Apply `entities_filter` to the topics under `namespace_id`.
    pub fn filtered_topics(
        &self,
        namespace_id: &str,
    ) -> Vec<&crate::azure::service_bus::ServiceBusTopic> {
        let needle = self.entities_filter.value().to_lowercase();
        match self.topics.get(namespace_id) {
            Some(rows) => rows
                .iter()
                .filter(|t| needle.is_empty() || t.name.to_lowercase().contains(&needle))
                .collect(),
            None => Vec::new(),
        }
    }

    /// Apply `subscriptions_filter` to the subscriptions under
    /// `(namespace_id, topic)`.
    pub fn filtered_subscriptions(
        &self,
        namespace_id: &str,
        topic: &str,
    ) -> Vec<&crate::azure::service_bus::ServiceBusSubscription> {
        let needle = self.subscriptions_filter.value().to_lowercase();
        let key = Self::subscriptions_key(namespace_id, topic);
        match self.subscriptions.get(&key) {
            Some(rows) => rows
                .iter()
                .filter(|s| needle.is_empty() || s.name.to_lowercase().contains(&needle))
                .collect(),
            None => Vec::new(),
        }
    }
}

/// State for the Azure SQL category: one flat list of elastic pools + single
/// databases (no drill chain), plus per-resource utilization metrics keyed by
/// ARM id. The metrics fields mirror [`MetricsCache`] but are scoped to this
/// category so a SQL detail refresh never collides with the Apis chart cache.
#[derive(Clone, Default)]
pub struct SqlCache {
    /// All pools + databases discovered for the current subscription scope.
    /// `Option` distinguishes "never fetched" from "fetched and empty".
    pub resources: Option<Vec<crate::azure::sql::SqlResource>>,
    pub pending: bool,
    pub error: Option<String>,
    pub cursor: usize,
    /// Persisted table scroll offset — see `StorageCache::accounts_view_top`.
    pub view_top: Cell<usize>,
    pub filter: Input,
    pub filter_active: bool,
    /// Row the user drilled into — pinned so a background list refresh that
    /// reorders the list can't retarget the open detail view.
    pub selected: Option<crate::azure::sql::SqlResource>,

    /// Utilization series keyed by SQL resource id.
    pub metrics: HashMap<String, Vec<MetricSeries>>,
    pub metrics_pending: HashSet<String>,
    /// Per-resource, per-metric "missing" reasons (e.g. `dtu_consumption_percent`
    /// absent on a vCore resource), so the detail view can explain a blank row.
    pub metrics_missing: HashMap<String, HashMap<MetricKind, String>>,
    /// Per-resource whole-fetch failures (mutually exclusive with `metrics`).
    pub metrics_failures: HashMap<String, String>,
    /// Currently-selected chart window; changed with `0`/`1`/`7` in the detail.
    pub metrics_range: TimeRange,

    /// Audit-log drill-in (`l` on a pool / database): the principal roll-up
    /// and per-principal event views.
    pub audit: SqlAuditState,

    /// Open-sessions view (`u` on a pool / database). ⚠ live T-SQL.
    pub sessions: SqlSessionsState,
}

/// State for the open-sessions view — the only view backed by live T-SQL
/// (with the audit roll-up's database-user merge). Same fetch discipline as
/// the audit views: `generation` orphans stale in-flight results.
#[derive(Clone, Default)]
pub struct SqlSessionsState {
    /// Server (+ optional database) the query targets, derived from the
    /// pinned SQL resource on entry.
    pub target: Option<crate::azure::sql_audit::AuditTarget>,
    /// Where Esc should land (wherever `u` was pressed).
    pub return_view: Option<View>,
    pub rows: Option<Vec<crate::azure::sql_tds::DbSession>>,
    pub pending: bool,
    pub error: Option<String>,
    pub cursor: usize,
    /// Persisted table scroll offset — see `StorageCache::accounts_view_top`.
    pub view_top: Cell<usize>,
    pub generation: u64,
}

impl SqlSessionsState {
    /// Reset for a fresh entry on `target`.
    pub fn enter(&mut self, target: crate::azure::sql_audit::AuditTarget, return_view: View) {
        self.target = Some(target);
        self.return_view = Some(return_view);
        self.rows = None;
        self.pending = false;
        self.error = None;
        self.cursor = 0;
        self.view_top.set(0);
        self.generation = self.generation.wrapping_add(1);
    }
}

/// State for the SQL audit views (principal roll-up + per-principal events).
/// Same query-scope discipline as the Key Vault access view: the window and
/// target are *query* parameters — changing them drops the buffers and bumps
/// `generation` so a stale in-flight page can't land.
#[derive(Clone)]
pub struct SqlAuditState {
    /// What the queries run against, derived from the pinned SQL resource on
    /// entry. `None` until the view is first opened.
    pub target: Option<crate::azure::sql_audit::AuditTarget>,
    /// Where Esc from the principals view should land (`SqlResources` or
    /// `SqlDetail`, wherever `l` was pressed).
    pub return_view: Option<View>,
    /// Query time window, shared by both audit views. Defaults to 30 days —
    /// "can I delete this user" needs lookback, not today.
    pub window: crate::azure::key_vault_logs::AccessWindow,
    /// Free-form custom-window input (`t`), e.g. "6m" — see the KV access view.
    pub window_input: Input,
    pub window_input_active: bool,
    /// Monotonic fetch-scope token for the principal roll-up; a landing page
    /// with a stale generation is discarded. The events fetch has its own
    /// token (`events_generation`) so spawning one can never orphan an
    /// in-flight fetch of the other.
    pub generation: u64,
    /// Fetch-scope token for the per-principal events page.
    pub events_generation: u64,

    /// The database's actual user list (⚠ via live T-SQL,
    /// `sys.database_principals`) — merged into the roll-up so users with
    /// *zero* audit rows in the window become visible instead of invisible.
    /// Only fetched when the target is a single database and
    /// `sql_live_queries` is on. Not window-scoped: survives window changes.
    pub db_users: Option<Vec<crate::azure::sql_tds::DbUser>>,
    pub db_users_pending: bool,
    /// Fetch-scope token for the user list — separate from `generation` so a
    /// window-change refetch of the roll-up can never orphan an in-flight
    /// user fetch (the list isn't window-scoped).
    pub db_users_generation: u64,
    /// Why the user list is absent (T-SQL failed / disabled by config) — shown
    /// as a one-line note, never as a fatal error: the audit roll-up itself
    /// is unaffected.
    pub db_users_note: Option<String>,

    /// The principal roll-up. `Option` distinguishes "never fetched" from
    /// "fetched and empty".
    pub principals: Option<Vec<crate::azure::sql_audit::PrincipalSummary>>,
    pub principals_truncated: bool,
    pub pending: bool,
    pub error: Option<String>,
    pub cursor: usize,
    /// Persisted table scroll offset — see `StorageCache::accounts_view_top`.
    pub view_top: Cell<usize>,
    /// `/`-filter over the roll-up (matches the raw principal *and* its
    /// Graph-resolved display name — the user filters what they see). Standard
    /// substring-filter pattern shared with every list view.
    pub principals_filter: Input,
    pub principals_filter_active: bool,

    /// Principal pinned by Enter on a roll-up row (the events view's scope).
    pub selected_principal: Option<String>,
    pub events: Option<Vec<crate::azure::sql_audit::AuditEvent>>,
    pub events_truncated: bool,
    pub events_pending: bool,
    pub events_error: Option<String>,
    pub events_cursor: usize,
    /// Persisted table scroll offset — see `StorageCache::accounts_view_top`.
    pub events_view_top: Cell<usize>,
    /// Server-side `succeeded == false` filter, toggled with `e` in the
    /// events view. A query parameter (all failures in the window), not a
    /// client-side narrowing of the fetched page.
    pub events_errors_only: bool,
    /// Client-side action-code filter (`BCM`, `TRCC`, …), cycled with Tab /
    /// Shift-Tab through the distinct actions in the fetched page — hides tx /
    /// session noise to leave just the queries (or just the logins). Unlike
    /// `events_errors_only` this narrows the cached page, not the query.
    pub events_action_filter: Option<String>,
    /// Vertical scroll inside the event detail view (long statements wrap).
    pub detail_scroll: u16,
    /// An *older-than* page is in flight (scroll-past-bottom fetch). Distinct
    /// from `events_pending` so the body doesn't wipe back to "loading" while
    /// appending — mirrors `LogsCache::loading_more`.
    pub events_loading_more: bool,
    /// Set by the events view when the cursor pushes past the last row and
    /// the window has more; drained by `after_action`, which spawns the
    /// older-than fetch (view handlers can't spawn tasks).
    pub events_fetch_older: bool,
}

impl Default for SqlAuditState {
    fn default() -> Self {
        SqlAuditState {
            target: None,
            return_view: None,
            // 30 days, not the KV default of 1 day: the audit questions here
            // ("is this login dead?") need lookback.
            window: crate::azure::key_vault_logs::AccessWindow::Custom {
                hours: 30 * 24,
                label: "30d".to_string(),
            },
            window_input: Input::default(),
            window_input_active: false,
            generation: 0,
            events_generation: 0,
            db_users: None,
            db_users_pending: false,
            db_users_generation: 0,
            db_users_note: None,
            principals: None,
            principals_truncated: false,
            pending: false,
            error: None,
            cursor: 0,
            view_top: Cell::new(0),
            principals_filter: Input::default(),
            principals_filter_active: false,
            selected_principal: None,
            events: None,
            events_truncated: false,
            events_pending: false,
            events_error: None,
            events_cursor: 0,
            events_view_top: Cell::new(0),
            events_errors_only: false,
            events_action_filter: None,
            detail_scroll: 0,
            events_loading_more: false,
            events_fetch_older: false,
        }
    }
}

impl SqlAuditState {
    /// Reset for a fresh entry on `target` (from wherever `l` was pressed).
    /// The window survives re-entry — lookback preference is sticky — but the
    /// buffers never do: a different resource (or a refreshed one) must not
    /// show stale rows under a new header.
    pub fn enter(&mut self, target: crate::azure::sql_audit::AuditTarget, return_view: View) {
        self.target = Some(target);
        self.return_view = Some(return_view);
        self.db_users = None;
        self.db_users_pending = false;
        self.db_users_note = None;
        self.db_users_generation = self.db_users_generation.wrapping_add(1);
        self.window_input.reset();
        self.window_input_active = false;
        self.principals = None;
        self.principals_truncated = false;
        self.pending = false;
        self.error = None;
        self.cursor = 0;
        self.view_top.set(0);
        self.principals_filter.reset();
        self.principals_filter_active = false;
        self.selected_principal = None;
        self.events_errors_only = false;
        self.drop_events();
        self.generation = self.generation.wrapping_add(1);
        self.events_generation = self.events_generation.wrapping_add(1);
    }

    /// Drop the per-principal event buffer (entering the events view on a new
    /// principal, or invalidating on a window change). The errors-only filter
    /// survives — it's a preference, not part of the buffer.
    pub fn drop_events(&mut self) {
        self.events = None;
        self.events_truncated = false;
        self.events_pending = false;
        self.events_error = None;
        self.events_cursor = 0;
        self.events_view_top.set(0);
        self.events_loading_more = false;
        self.events_fetch_older = false;
        // The action filter narrows the buffer being dropped; a fresh page
        // starts unfiltered (the errors-only flag is a query param and sticks).
        self.events_action_filter = None;
    }

    /// Events with the client-side action filter applied — the view the
    /// events table (and its cursor) indexes into.
    pub fn visible_events(&self) -> Vec<&crate::azure::sql_audit::AuditEvent> {
        match self.events.as_ref() {
            Some(rows) => rows
                .iter()
                .filter(|e| {
                    self.events_action_filter
                        .as_deref()
                        .is_none_or(|a| e.action == a)
                })
                .collect(),
            None => Vec::new(),
        }
    }

    /// Distinct action codes in the fetched page, sorted — the Tab cycle
    /// order for the action filter.
    pub fn event_actions(&self) -> Vec<String> {
        let mut actions: Vec<String> = self
            .events
            .iter()
            .flatten()
            .map(|e| e.action.clone())
            .collect();
        actions.sort();
        actions.dedup();
        actions
    }

    /// Drop both buffers and bump the generation — the query scope (window)
    /// changed, so neither buffer describes what the header claims and any
    /// in-flight fetch is stale.
    pub fn invalidate_fetch(&mut self) {
        self.principals = None;
        self.principals_truncated = false;
        self.pending = false;
        self.error = None;
        self.cursor = 0;
        self.view_top.set(0);
        self.drop_events();
        self.generation = self.generation.wrapping_add(1);
        self.events_generation = self.events_generation.wrapping_add(1);
    }
}

impl SqlCache {
    /// Apply `filter` to `resources` as a case-insensitive substring match on
    /// the resource name or its logical server, then order the survivors so
    /// each elastic pool is immediately followed by its member databases (the
    /// view indents those). A member whose pool didn't survive the filter
    /// sorts as a standalone row. Empty filter passes everything; returns
    /// empty when nothing's been fetched yet.
    pub fn filtered_resources(&self) -> Vec<&crate::azure::sql::SqlResource> {
        let needle = self.filter.value().to_lowercase();
        let Some(rows) = self.resources.as_ref() else {
            return Vec::new();
        };
        let mut out: Vec<&crate::azure::sql::SqlResource> = rows
            .iter()
            .filter(|r| {
                needle.is_empty()
                    || r.name.to_lowercase().contains(&needle)
                    || r.server.to_lowercase().contains(&needle)
            })
            .collect();
        let pool_ids = Self::pool_ids(&out);
        // Group key: pooled databases adopt their pool's leaf name and sort
        // *after* it (rank 1); pools and standalone databases interleave by
        // their own name (rank 0).
        let key = |r: &crate::azure::sql::SqlResource| -> (String, String, u8, String) {
            let server = r.server.to_lowercase();
            match r
                .elastic_pool_id
                .as_deref()
                .map(str::to_lowercase)
                .filter(|id| pool_ids.contains(id))
            {
                Some(pool_id) => {
                    let pool_name = pool_id.rsplit('/').next().unwrap_or(&pool_id).to_string();
                    (server, pool_name, 1, r.name.to_lowercase())
                }
                None => (server, r.name.to_lowercase(), 0, String::new()),
            }
        };
        out.sort_by_key(|r| key(r));
        out
    }

    /// Lowercased ARM ids of the elastic pools in `rows` — the "is this
    /// database's pool on screen" test shared by the ordering above and the
    /// view's indentation.
    pub fn pool_ids(rows: &[&crate::azure::sql::SqlResource]) -> HashSet<String> {
        rows.iter()
            .filter(|r| matches!(r.kind, crate::azure::sql::SqlKind::ElasticPool))
            .map(|r| r.id.to_lowercase())
            .collect()
    }

    /// The row currently under the cursor in the filtered list.
    pub fn selected_in_list(&self) -> Option<crate::azure::sql::SqlResource> {
        self.filtered_resources().get(self.cursor).copied().cloned()
    }
}

#[derive(Clone, Default)]
pub struct LogsCache {
    /// keyed by resource id
    pub by_resource: HashMap<String, Vec<LogLine>>,
    /// Log Analytics workspace ARM id per resource, resolved as a side effect of
    /// the first log page (Container Apps only — their logs live in the env's
    /// workspace, not the app resource). Used to scope `o`'s portal Logs blade
    /// deep-link to the workspace. Absent for resources whose logs are
    /// resource-scoped (Function Apps) or when resolution failed.
    pub workspace_ids: HashMap<String, String>,
    /// Per-resource flag for "the last fetch came back full, so older rows
    /// may still exist in the window." Drives both the header indicator and
    /// the auto-fetch trigger on G / scroll-past-bottom.
    pub more_available: HashMap<String, bool>,
    /// Set true while an *older-than* page is in flight. Distinct from
    /// `loading`, which is the initial / refresh fetch — keeping them apart
    /// avoids the body wiping back to "Loading logs…" during a fetch-more.
    pub loading_more: bool,
    /// Set by the logs view when the user crosses the bottom of the buffer
    /// (G or MoveDown at last row). Drained by the event loop, which spawns
    /// the older-than fetch and clears it. View handlers can't spawn tasks
    /// directly, so this flag is the bridge.
    pub fetch_more_requested: bool,
    pub range: TimeRange,
    pub errors_only: bool,
    /// Client-side filter on `LogLine::source` (for Container Apps the
    /// emitting container, for Function Apps the table/function). `None`
    /// shows every cached line. Cycled with `s` through the distinct sources
    /// present in the buffer; unlike `errors_only` it never refetches —
    /// it narrows what's already cached.
    pub source_filter: Option<String>,
    pub loading: bool,
    pub last_error: Option<String>,
    /// Cursor row inside the logs table (index into the cached lines).
    /// Kept separate from `AppState::list_cursor` so navigating logs does not corrupt the resource selection in the List view.
    pub scroll: usize,
    /// Index of the first visible row — the viewport's top. Distinct from
    /// `scroll` (the cursor) so the cursor can move freely within the viewport
    /// and the window only scrolls once the cursor reaches an edge. Persisted
    /// across frames; reconciled in [`crate::ui::views::logs`]'s render, the
    /// only place that knows the viewport height (it depends on the dynamic
    /// breadcrumb / command / status rows). `Cell` because render takes
    /// `&AppState` yet must write back the offset it just computed.
    pub view_top: Cell<usize>,
    /// When true, render the source and message columns as multi-line wrapped
    /// text so long values are fully visible (row heights expand). Toggled with `w`.
    pub wrap: bool,
    /// Vertical scroll offset (in lines) for the log-detail view.
    pub detail_scroll: u16,
    /// Horizontal column offset (in characters) applied to the source and
    /// message columns when `wrap` is OFF. Lets the user scroll long lines
    /// past the column boundary with `h`/`l`. No-op when `wrap` is ON because
    /// the text already lays out across multiple rows.
    pub h_offset: usize,
    /// Whether the logs search box currently has focus. While true, raw
    /// keystrokes are forwarded into `search_input` rather than dispatched as
    /// actions; Esc cancels AND clears the query (consistent with the storage
    /// views — "Esc removes the filter, period"); Enter commits and jumps to
    /// the next match.
    pub search_active: bool,
    /// Case-insensitive substring filter applied to the source and message
    /// columns. Reset on Esc; persists across Enter-commits so the user can
    /// keep `n`/`N`-navigating without re-typing.
    pub search_input: Input,
    /// Vim visual-line mode anchor. `Some(row)` means a multi-line selection is
    /// active, anchored at `row` (the cursor at the time `V` was pressed); the
    /// live selection spans `min(anchor, scroll)..=max(anchor, scroll)`. `j`/`k`
    /// extend it, `y` yanks the whole span, and `V`/`Esc` cancel. `None` is the
    /// normal single-line mode. Reset whenever the buffer is refetched or
    /// refiltered, since the anchored index would otherwise point at a stale row.
    pub visual_anchor: Option<usize>,
    /// Monotonic fetch-scope token. Bumped on every change that invalidates the
    /// current buffer (errors-only toggle, window change, context jump). Each
    /// fetch carries the generation it was issued under; `LogsLoaded` discards a
    /// page whose generation is stale, so a slow in-flight fetch can't clobber
    /// the buffer after the user has changed the filter again. This is what makes
    /// the `e` toggle deterministic no matter how fast it's pressed.
    pub generation: u64,
    /// A specific log line the next view change should try to keep selected. Set
    /// just before an errors-only toggle / context jump (whose refetch is async);
    /// resolved in `LogsLoaded` once the new page lands — the cursor moves to the
    /// same line if it still exists in the new buffer. Source cycling resolves it
    /// synchronously instead. `None` outside a view transition.
    pub pending_anchor: Option<LineAnchor>,
    /// When set, the requested window for the *next* initial fetch is centered on
    /// this timestamp (unfiltered) rather than the newest rows — this is the
    /// "context around an error" jump triggered by toggling errors-only OFF while
    /// a line is selected. Cleared once errors-only is turned back on, the window
    /// changes, or the view is re-entered.
    pub context_around: Option<chrono::DateTime<chrono::Utc>>,
    /// One-shot flag: when true, the next render centers the cursor row in the
    /// viewport (instead of the usual edge-scroll), then clears it. Set after an
    /// anchor is resolved so the kept line lands mid-screen with context above and
    /// below. `Cell` because render holds `&AppState`.
    pub center_pending: Cell<bool>,
    /// True while an `E` (jump-to-next-error) hunt is in flight: no error-level
    /// line exists below the cursor in the buffer yet, so the event loop keeps
    /// chaining older-than fetches, re-checking after each page lands. Cleared
    /// when an error is found, the window is exhausted, the fetch scope changes,
    /// or the user cancels with Esc. The logs header renders a "searching…"
    /// chip while set.
    pub error_hunt: bool,
    /// Memoized distinct-source list for the tab-bar / Tab-cycling, paired with
    /// the buffer fingerprint it was computed from. Recomputing it means cloning
    /// and sorting every cached line's source on each 250ms redraw — wasted work
    /// on buffers that scroll-to-load grows to thousands of rows. The buffer
    /// only changes in `LogsLoaded` (replace or append), so the fingerprint
    /// (resource id, generation, row count, first/last timestamps) is enough to
    /// detect staleness. `RefCell` because render holds `&AppState`.
    pub sources_memo: RefCell<Option<(LogsBufferFingerprint, Vec<String>)>>,
}

/// Identity of one exact logs-buffer state, used to invalidate
/// [`LogsCache::sources_memo`]: resource id, fetch generation, row count, and
/// the first/last rows' timestamps. Appends grow the count and move the last
/// timestamp; a same-length refresh moves the first (newest) timestamp; scope
/// changes bump the generation — so any buffer mutation shifts the fingerprint.
pub type LogsBufferFingerprint = (
    String,
    u64,
    usize,
    Option<chrono::DateTime<chrono::Utc>>,
    Option<chrono::DateTime<chrono::Utc>>,
);

/// Identity of a single log line, used to re-select "the same line" across a
/// filter/source change. There's no server-side unique id, so we key on the
/// triple that is effectively unique in practice: emission time, emitting
/// source, and the message text.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LineAnchor {
    pub ts: chrono::DateTime<chrono::Utc>,
    pub source: String,
    pub message: String,
}

impl LineAnchor {
    pub fn of(line: &crate::azure::logs::LogLine) -> Self {
        Self {
            ts: line.ts,
            source: line.source.clone(),
            message: line.message.clone(),
        }
    }

    pub fn matches(&self, line: &crate::azure::logs::LogLine) -> bool {
        self.ts == line.ts && self.source == line.source && self.message == line.message
    }
}

impl LogsCache {
    /// Clear the per-view, resource-specific filters when (re)opening the Logs
    /// view. The `source_filter` in particular holds a value from whatever
    /// resource was viewed last (for Container Apps, a container name) — carried
    /// into a different app it matches nothing, leaving an empty list and a
    /// "no cached lines from source 'x'" message. The free-text search is
    /// likewise stale, and a visual-selection anchor would point at a row that
    /// no longer exists. Fetch-scoping prefs (`errors_only`, `range`, `wrap`)
    /// are intentionally left alone — they're resource-agnostic.
    pub fn reset_view_filters(&mut self) {
        self.source_filter = None;
        self.search_active = false;
        self.search_input.reset();
        self.visual_anchor = None;
        // A context jump is resource-specific (it centers on one error's
        // timestamp), so dropping into a different resource's logs must clear it
        // along with any anchor still waiting to resolve.
        self.context_around = None;
        self.pending_anchor = None;
        self.center_pending.set(false);
        // An error hunt chains fetches for one specific resource's buffer;
        // carried into another resource it would chase the wrong stream.
        self.error_hunt = false;
        // The cursor / viewport / horizontal offset are equally resource-bound:
        // left at the previous resource's deep-scroll position, a fresh buffer
        // renders pinned to its clamped bottom while `k` decrements an index
        // hundreds of rows past the end — the view looks frozen until the
        // counter walks back into range. Same idiom as the errors-only toggle
        // and window-change paths, which zero these whenever the buffer scope
        // changes.
        self.scroll = 0;
        self.view_top.set(0);
        self.h_offset = 0;
    }
}

/// Top-level UI state. Lane 3 mutates this in response to events; Lane 4 reads it for rendering.
pub struct AppState {
    pub config: Config,
    pub view: View,
    /// Return path for the Help overlay — the one view that behaves as a
    /// modal (`?` from anywhere, Esc goes back to wherever that was). Pushed
    /// only when Help opens; popped by its Back. All other navigation walks
    /// the `semantic_parent` breadcrumb tree instead, so nothing else may
    /// push here: stale entries would leak into Help's return path.
    pub view_stack: Vec<View>,
    /// Which top-level resource category the user was most recently inside.
    /// Sticky across the subscription picker, so switching subscriptions
    /// returns the user to the same category list (under the new scope)
    /// instead of always landing them on apis. Updated by [`enter_category`].
    pub last_category: Category,

    /// Monotonic counter identifying the current subscription scope + signed-in
    /// identity. Bumped when the user pins a different subscription (or "All")
    /// and after an in-app `az login`. Every scope-level background fetch
    /// (resource/account/namespace lists) captures it at spawn time and carries
    /// it back in its `*Loaded` event; the handlers drop results whose scope no
    /// longer matches, so a slow in-flight fetch for the previous scope can
    /// never be displayed as if it were the current one.
    pub scope_generation: u64,

    pub subscriptions: Vec<Subscription>,
    pub selected_subscription: Option<String>,
    pub subscription_cursor: usize,
    /// `/`-search box for the subscription picker. Mirrors `list_filter` +
    /// `list_filter_active`: a subsequence match over each subscription's
    /// display name and id (see [`AppState::filtered_subscription_list`]).
    pub subscription_filter: Input,
    pub subscription_filter_active: bool,
    pub loading_subscriptions: bool,

    pub resources: Vec<Resource>,
    pub list_cursor: usize,
    /// Persisted viewport top for the resource list, reconciled each render by
    /// [`crate::ui::views::edge_scroll`] so the window only moves when the
    /// cursor pushes against an edge. `Cell` because render takes `&AppState`.
    pub list_view_top: Cell<usize>,
    pub list_filter: Input,
    pub list_filter_active: bool,
    pub favorites_only: bool,
    pub loading_resources: bool,
    /// When the resource list last finished loading. Drives the "updated Xs ago"
    /// indicator in the list title and the auto-refresh timer (see
    /// `app::maybe_auto_refresh`). `None` until the first successful load. Stored
    /// as `std::time::Instant` (monotonic) like `status_message_until`, so the
    /// field doesn't pull a Serialize bound onto the rest of the struct.
    pub resources_loaded_at: Option<std::time::Instant>,
    /// When auto-refresh last *fired* a reload (success or not). Kept distinct
    /// from `resources_loaded_at` (which only advances on a successful load) so a
    /// failing/throttled reload re-arms the interval instead of retrying every
    /// tick. See `app::maybe_auto_refresh`.
    pub last_auto_refresh: Option<std::time::Instant>,

    /// State of the dedicated env-vars page (cursor + whether values are
    /// revealed). Reset on entering the page, so values always start masked.
    pub env_vars_view: EnvVarsView,

    /// Active add/edit-env-var flow, or `None` when no editor is open. Gated
    /// behind `Ctrl+E` / `Ctrl+N` and a confirm step before any write.
    pub env_var_edit: Option<EnvVarEdit>,

    /// Detail-view cursor (over the meta rows) plus the optional Enter modal
    /// payload. Reset on entering Detail from a different view.
    pub detail_view: DetailView,

    /// Max valid vertical scroll offset for the scrollable body currently in
    /// the foreground (the full-screen detail panes and Enter/reveal modals —
    /// only one such body is ever on top at a time, so a single slot serves
    /// them all). Written back by each render pass, the only place that knows
    /// the wrapped row count and viewport height; read by the key handlers so
    /// `G`/`j` clamp the *stored* offset instead of parking it at a huge
    /// sentinel where `k` decrements invisibly for thousands of presses.
    /// `Cell` because render holds `&AppState`.
    pub scroll_max: Cell<u16>,

    pub metrics: MetricsCache,
    pub health: HealthCache,
    pub logs: LogsCache,
    pub container_app_overview: ContainerAppOverviewCache,
    pub revision_meta: RevisionMetaCache,
    pub replica_instances: ReplicaInstancesCache,
    pub func_image: FuncImageCache,
    pub func_settings: FuncSettingsCache,
    pub func_triggers: FuncTriggersCache,
    pub principals: PrincipalCache,
    pub apim: ApimCache,
    pub appgw: AppGatewayBackendsCache,
    pub storage: StorageCache,
    pub registry: RegistryCache,
    pub cosmos: CosmosCache,
    pub key_vault: KeyVaultCache,
    pub service_bus: ServiceBusCache,
    pub sql: SqlCache,

    pub status_message: Option<String>,
    /// When set, the status bar auto-clears at this point in time. The event
    /// loop's tick handler is responsible for the clearing — see the `Tick`
    /// arm in `app.rs`. Stored as `std::time::Instant` so the field doesn't
    /// take a Serialize bound for the rest of the struct.
    pub status_message_until: Option<std::time::Instant>,
    pub should_quit: bool,

    /// Modal flag: when true, a "Are you sure you want to quit?" overlay is
    /// rendered on top of the current view and the event loop short-circuits
    /// keyboard input to the modal handler. Set via `Action::Back` on an empty
    /// view stack; cleared by answering No (or any cancel key). Note this is
    /// *not* a `View` variant — the underlying view keeps rendering behind it.
    pub quit_confirm: bool,
    /// Which button is focused inside the quit modal. `true` = Yes, `false` =
    /// No. Always reset to `false` (the safer default) when the modal opens.
    pub quit_confirm_yes: bool,

    /// Whether the vim/k9s-style command palette (`:`) is currently capturing
    /// input. While true, raw keystrokes are forwarded into `command_input`
    /// rather than dispatched as actions; Esc cancels, Enter executes.
    pub command_active: bool,
    pub command_input: Input,
    /// Active Tab-completion cycle. `(original_prefix, candidates, index)`:
    /// `original_prefix` is what the user had typed before the *first* Tab in
    /// the cycle (so Shift+Tab back past zero restores it); `candidates` is
    /// the prefix-match list captured at that moment; `index` is the
    /// currently-shown candidate. Cleared as soon as the user types a key
    /// other than Tab / Shift+Tab.
    pub command_tab_cycle: Option<(String, Vec<String>, usize)>,

    /// In-app `az login` modal state. See [`AuthPrompt`].
    pub auth_prompt: AuthPrompt,
    /// Currently-focused option inside the auth menu.
    pub auth_menu_focus: AuthMenuFocus,
    /// Tenant-id buffer captured via the `T` step. `None` ⇒ use the
    /// signed-in account's default tenant.
    pub auth_tenant: Option<String>,
    /// Working buffer while the user is typing in the tenant input.
    pub auth_tenant_input: Input,
    /// Last error from a finished `az login` attempt, if any. Rendered inside
    /// the menu so the user knows why the previous try failed.
    pub auth_last_error: Option<String>,
    /// Set to `Some` by the modal handler when the user confirms a login.
    /// The event loop takes ownership, suspends the TUI, runs `az login`,
    /// clears the auth cache, and triggers a subscriptions reload.
    pub pending_login: Option<PendingLogin>,
    /// Set by the `s` handler on a Container App; the event loop takes ownership,
    /// suspends the TUI, and runs `az containerapp exec` for an interactive
    /// shell. Mirrors [`Self::pending_login`].
    pub pending_exec: Option<PendingExec>,
    /// When `true`, the input-reader thread parks instead of reading the
    /// terminal. Set while a shell-out child (`az containerapp exec`) owns the
    /// terminal so azpect neither competes for the user's keystrokes nor gets
    /// `SIGTTIN`-stopped once the child becomes the terminal's foreground
    /// process group. Shared with the reader thread.
    pub input_suspended: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        let range = config.default_window;
        Self {
            // Land straight on the API resources list (all subscriptions, or a
            // previously-pinned one restored below). The Subscriptions view is
            // now an optional filter reached with `s`, not a startup gate.
            view: View::List,
            view_stack: Vec::new(),
            last_category: Category::Apis,
            scope_generation: 0,
            subscriptions: Vec::new(),
            selected_subscription: config.last_subscription_id.clone(),
            subscription_cursor: 0,
            subscription_filter: Input::default(),
            subscription_filter_active: false,
            loading_subscriptions: true,
            resources: Vec::new(),
            list_cursor: 0,
            list_view_top: Cell::new(0),
            list_filter: Input::default(),
            list_filter_active: false,
            favorites_only: false,
            loading_resources: false,
            resources_loaded_at: None,
            last_auto_refresh: None,
            env_vars_view: EnvVarsView::default(),
            env_var_edit: None,
            detail_view: DetailView::default(),
            scroll_max: Cell::new(0),
            metrics: MetricsCache {
                range,
                ..Default::default()
            },
            health: HealthCache::default(),
            logs: LogsCache {
                range,
                ..Default::default()
            },
            container_app_overview: ContainerAppOverviewCache::default(),
            revision_meta: RevisionMetaCache::default(),
            replica_instances: ReplicaInstancesCache::default(),
            func_image: FuncImageCache::default(),
            func_settings: FuncSettingsCache::default(),
            func_triggers: FuncTriggersCache::default(),
            principals: PrincipalCache::default(),
            apim: ApimCache::default(),
            appgw: AppGatewayBackendsCache::default(),
            storage: StorageCache::default(),
            registry: RegistryCache::default(),
            cosmos: CosmosCache::default(),
            key_vault: KeyVaultCache::default(),
            service_bus: ServiceBusCache::default(),
            sql: SqlCache::default(),
            status_message: None,
            status_message_until: None,
            should_quit: false,
            quit_confirm: false,
            quit_confirm_yes: false,
            command_active: false,
            command_input: Input::default(),
            command_tab_cycle: None,
            auth_prompt: AuthPrompt::Hidden,
            auth_menu_focus: AuthMenuFocus::Browser,
            auth_tenant: None,
            auth_tenant_input: Input::default(),
            auth_last_error: None,
            pending_login: None,
            pending_exec: None,
            input_suspended: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            config,
        }
    }

    /// Drop every cache that belongs to the previously signed-in identity:
    /// the subscription-scoped lists (via [`Category::clear_cache`]) plus all
    /// per-resource-id caches — metrics, health, logs, app settings and env
    /// vars, some of which are secret-bearing. Called after a successful
    /// in-app `az login`: the old identity's data must not stay visible (or
    /// yankable) under the new login. User preferences that happen to live
    /// inside the caches (chart window, errors-only toggle) survive.
    pub fn flush_identity_caches(&mut self) {
        for category in Category::ALL {
            category.clear_cache(self);
        }
        self.metrics = MetricsCache {
            range: self.metrics.range,
            ..Default::default()
        };
        self.logs = LogsCache {
            range: self.logs.range,
            errors_only: self.logs.errors_only,
            // Orphan any in-flight fetch: a late page from the old identity
            // must not repopulate the buffer we just flushed.
            generation: self.logs.generation.wrapping_add(1),
            ..Default::default()
        };
        self.health = HealthCache::default();
        self.container_app_overview = ContainerAppOverviewCache::default();
        self.revision_meta = RevisionMetaCache::default();
        self.replica_instances = ReplicaInstancesCache::default();
        self.func_image = FuncImageCache::default();
        self.func_settings = FuncSettingsCache::default();
        self.func_triggers = FuncTriggersCache::default();
        self.principals = PrincipalCache::default();
        self.apim = ApimCache::default();
        self.resources_loaded_at = None;
    }

    /// Progress of the per-row decoration fetches for the *current* resource
    /// list — health badge for every row, overview for Container Apps, image
    /// for Function Apps — as `(back, launched)`. These fetches are throttled
    /// through a shared semaphore to avoid ARM 429s, so on a large
    /// subscription the list takes a while to settle; the header uses this to
    /// show that progress instead of leaving the user staring at LOADING
    /// badges with no sense of how far along the sweep is. `None` once
    /// nothing is in flight (the indicator should disappear, not stick at
    /// n/n). Errored fetches that cache no failure marker drop out of
    /// `launched` when they finish — counts may shrink slightly while
    /// settling, but they converge and the condition for hiding is exact.
    pub fn list_fetch_progress(&self) -> Option<(usize, usize)> {
        use crate::azure::resources::ResourceKind;
        let mut in_flight = 0usize;
        let mut back = 0usize;
        let mut tally = |pending: bool, done: bool| {
            if pending {
                in_flight += 1;
            } else if done {
                back += 1;
            }
        };
        for r in &self.resources {
            tally(
                self.health.pending.contains(&r.id),
                self.health.by_resource.contains_key(&r.id)
                    || self.health.failures.contains_key(&r.id),
            );
            match r.kind {
                ResourceKind::ContainerApp => tally(
                    self.container_app_overview.pending.contains(&r.id),
                    self.container_app_overview.by_resource.contains_key(&r.id),
                ),
                ResourceKind::FunctionApp => tally(
                    self.func_image.pending.contains(&r.id),
                    self.func_image.by_resource.contains_key(&r.id),
                ),
                _ => {}
            }
        }
        if in_flight == 0 {
            None
        } else {
            Some((back, back + in_flight))
        }
    }

    /// Resource currently under the cursor in the list view, after applying filter.
    pub fn selected_resource(&self) -> Option<&Resource> {
        // Lane 3/4 will likely want a filtered iterator helper; this naive impl is a placeholder.
        self.filtered_resources().get(self.list_cursor).copied()
    }

    /// Cached log lines for `resource_id` after applying `logs.source_filter`.
    /// This is what the logs table renders and what `logs.scroll` indexes into,
    /// so every consumer of "the selected log line" (log detail, yank, search
    /// jumps) must resolve through this same view of the cache.
    pub fn visible_log_lines(&self, resource_id: &str) -> Vec<&crate::azure::logs::LogLine> {
        let Some(lines) = self.logs.by_resource.get(resource_id) else {
            return Vec::new();
        };
        match self.logs.source_filter.as_deref() {
            Some(src) => lines.iter().filter(|l| l.source == src).collect(),
            None => lines.iter().collect(),
        }
    }

    /// The log line currently under the cursor in the logs table, resolved
    /// through the same `visible_log_lines` view the table renders. `None` when
    /// no resource is selected or the visible buffer is empty.
    pub fn selected_log_line(&self) -> Option<crate::azure::logs::LogLine> {
        let id = self.selected_resource()?.id.clone();
        let lines = self.visible_log_lines(&id);
        lines
            .get(self.logs.scroll.min(lines.len().saturating_sub(1)))
            .map(|l| (*l).clone())
    }

    /// Locate `anchor`'s line in the current visible buffer for `resource_id`,
    /// returning its row index. Falls back to the row with the nearest timestamp
    /// when the exact line is gone (e.g. it was an INFO line and errors-only just
    /// turned on, or the message differs), so the cursor lands as close as
    /// possible to where the user was. `None` only when the buffer is empty.
    pub fn anchor_index(&self, resource_id: &str, anchor: &LineAnchor) -> Option<usize> {
        let lines = self.visible_log_lines(resource_id);
        if lines.is_empty() {
            return None;
        }
        if let Some(i) = lines.iter().position(|l| anchor.matches(l)) {
            return Some(i);
        }
        // Nearest-by-time fallback.
        lines
            .iter()
            .enumerate()
            .min_by_key(|(_, l)| (l.ts - anchor.ts).num_milliseconds().abs())
            .map(|(i, _)| i)
    }

    /// Set the bottom-row status hint with the standard auto-clear window.
    /// Use this instead of writing to `status_message` directly so the message
    /// gets a deadline (currently 4 seconds) the tick handler can act on.
    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status_message = Some(msg.into());
        self.status_message_until =
            Some(std::time::Instant::now() + std::time::Duration::from_secs(4));
    }

    /// Apply `list_filter` + `favorites_only` to `resources`.
    ///
    /// The filter is a case-insensitive subsequence match: typing `filedev`
    /// matches `rnd3-filemonitor-dev` because the characters appear in that
    /// order. This is more forgiving than substring matching and lets users
    /// pick out resources without remembering exact prefixes.
    pub fn filtered_resources(&self) -> Vec<&Resource> {
        let needle = self.list_filter.value().to_lowercase();
        self.resources
            .iter()
            .filter(|r| !self.favorites_only || self.config.is_favorite(&r.id))
            .filter(|r| needle.is_empty() || is_subsequence(&needle, &r.name.to_lowercase()))
            .collect()
    }

    /// Re-anchor `list_cursor` after `resources` is replaced (e.g. by the 60s
    /// autorefresh or a manual `r`). Because `list_cursor` indexes into
    /// [`Self::filtered_resources`] — not the full `resources` vec — both the
    /// restore and the clamp must happen in *filtered* index space. Clamping
    /// against the full list instead lets an active filter strand the cursor
    /// past the last visible row, which renders as a jump to the bottom that
    /// then needs many `k` presses to climb out of (the highlight stays pinned
    /// while the out-of-range index counts down). Restores to `anchor_id` if
    /// that resource is still in the filtered set; otherwise just clamps.
    pub fn restore_list_cursor(&mut self, anchor_id: Option<&str>) {
        let filtered = self.filtered_resources();
        let filtered_len = filtered.len();
        let restored = anchor_id.and_then(|anchor| filtered.iter().position(|r| r.id == anchor));
        drop(filtered);
        if let Some(idx) = restored {
            self.list_cursor = idx;
        }
        if self.list_cursor >= filtered_len {
            self.list_cursor = filtered_len.saturating_sub(1);
        }
    }

    /// Subscriptions matching the picker's `/`-search box, in original order.
    /// Subsequence match (same as [`Self::filtered_resources`]) over the display
    /// name and the id, so a partial name or a chunk of the GUID both narrow the
    /// list. Empty filter passes everything through. The synthetic "All
    /// subscriptions" row is not part of this list — the view prepends it.
    pub fn filtered_subscription_list(&self) -> Vec<&Subscription> {
        let needle = self.subscription_filter.value().to_lowercase();
        self.subscriptions
            .iter()
            .filter(|s| {
                needle.is_empty()
                    || is_subsequence(&needle, &s.display_name.to_lowercase())
                    || is_subsequence(&needle, &s.id.to_lowercase())
            })
            .collect()
    }
}

/// Look up the human-readable display name for a subscription GUID. Returns
/// `None` when the id isn't in `state.subscriptions`, so callers can render a
/// blank cell rather than echoing the id back at the user.
///
/// Free function rather than an `AppState` method so views that already iterate
/// over `state.subscriptions` (or hold a borrow into it) can call this without
/// re-borrowing through `&self`.
pub fn subscription_display_name<'a>(state: &'a AppState, sub_id: &str) -> Option<&'a str> {
    state
        .subscriptions
        .iter()
        .find(|s| s.id == sub_id)
        .map(|s| s.display_name.as_str())
}

/// Returns true if every character of `needle` appears in `haystack` in order
/// (not necessarily contiguous). Both inputs are expected to already be in the
/// same case. Empty needle matches anything.
fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut needle_chars = needle.chars().peekable();
    for h in haystack.chars() {
        match needle_chars.peek() {
            Some(&n) if n == h => {
                needle_chars.next();
            }
            Some(_) => {}
            None => return true,
        }
    }
    needle_chars.peek().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn subsequence_basic_matches() {
        assert!(is_subsequence("filedev", "rnd3-filemonitor-dev"));
        assert!(is_subsequence("abc", "aXbYcZ"));
        assert!(is_subsequence("", "anything"));
    }

    #[test]
    fn subsequence_rejects_wrong_order() {
        assert!(!is_subsequence("dev-file", "rnd3-filemonitor-dev"));
        assert!(!is_subsequence("xyz", "abcde"));
    }

    #[test]
    fn subsequence_exact_substring_still_matches() {
        assert!(is_subsequence("file", "filemonitor"));
        assert!(is_subsequence("monitor", "filemonitor"));
    }

    #[test]
    fn category_of_classifies_every_resource_view() {
        // Every view that lives inside a category's drill chain must resolve
        // to that category. Acts as a structural lock: adding a new view
        // variant without claiming it via a Category::contains arm will trip
        // this test (the new view will resolve to `None`).
        assert_eq!(Category::of(View::List), Some(Category::Apis));
        assert_eq!(Category::of(View::Detail), Some(Category::Apis));
        assert_eq!(Category::of(View::Logs), Some(Category::Apis));
        assert_eq!(Category::of(View::LogDetail), Some(Category::Apis));
        assert_eq!(Category::of(View::ApimApis), Some(Category::Apis));
        assert_eq!(Category::of(View::ApimOperations), Some(Category::Apis));
        assert_eq!(Category::of(View::ApimPolicy), Some(Category::Apis));
        assert_eq!(Category::of(View::AppGatewayBackends), Some(Category::Apis));
        assert_eq!(Category::of(View::StorageAccounts), Some(Category::Storage));
        assert_eq!(
            Category::of(View::StorageAccountOverview),
            Some(Category::Storage)
        );
        assert_eq!(
            Category::of(View::StorageContainers),
            Some(Category::Storage)
        );
        assert_eq!(Category::of(View::StorageBlobs), Some(Category::Storage));
        assert_eq!(
            Category::of(View::StorageBlobDetail),
            Some(Category::Storage)
        );
        assert_eq!(Category::of(View::Registries), Some(Category::Registries));
        assert_eq!(
            Category::of(View::RegistryRepositories),
            Some(Category::Registries)
        );
        assert_eq!(Category::of(View::RegistryTags), Some(Category::Registries));
        assert_eq!(Category::of(View::CosmosAccounts), Some(Category::Cosmos));
        assert_eq!(Category::of(View::CosmosDatabases), Some(Category::Cosmos));
        assert_eq!(Category::of(View::CosmosContainers), Some(Category::Cosmos));
        assert_eq!(Category::of(View::CosmosItem), Some(Category::Cosmos));
        assert_eq!(Category::of(View::KeyVaults), Some(Category::KeyVaults));
        assert_eq!(Category::of(View::KeyVaultItems), Some(Category::KeyVaults));
        assert_eq!(
            Category::of(View::ServiceBusNamespaces),
            Some(Category::ServiceBus)
        );
        assert_eq!(
            Category::of(View::ServiceBusEntities),
            Some(Category::ServiceBus)
        );
        assert_eq!(
            Category::of(View::ServiceBusSubscriptions),
            Some(Category::ServiceBus)
        );
        // Subscriptions and Help are modal entry points — outside any chain.
        assert_eq!(Category::of(View::Subscriptions), None);
        assert_eq!(Category::of(View::Help), None);
    }

    #[test]
    fn category_root_view_round_trips() {
        // Every category's `root_view` must itself belong to that category.
        // A typo (e.g. `Category::Storage::root_view() = View::Registries`)
        // would be a navigation bomb — this guarantees the invariant.
        for category in Category::ALL {
            assert_eq!(
                Category::of(category.root_view()),
                Some(*category),
                "{category:?}.root_view() must belong to {category:?}",
            );
        }
    }

    #[test]
    fn enter_category_from_outside_chain_resets_cursor() {
        let mut state = AppState::new(Config::default());
        state.view = View::Subscriptions;
        // Pretend an old cursor was lingering from a previous session.
        state.storage.accounts_cursor = 7;
        enter_category(&mut state, Category::Storage);
        assert_eq!(state.view, View::StorageAccounts);
        assert_eq!(state.last_category, Category::Storage);
        // Back navigation is semantic-parent-based; the Help-only view_stack
        // must stay untouched by category switches.
        assert!(state.view_stack.is_empty());
        assert_eq!(
            state.storage.accounts_cursor, 0,
            "root cursor should reset on enter",
        );
    }

    #[test]
    fn enter_category_from_within_chain_is_idempotent() {
        // User is already deep in the storage chain. Pressing `S` (or
        // running `:storage`) again must NOT push the stack or move the view
        // — they're already where they wanted to be. Only `last_category`
        // gets re-asserted.
        let mut state = AppState::new(Config::default());
        state.view = View::StorageBlobs;
        state.storage.accounts_cursor = 4; // user's place — leave alone
        let stack_before = state.view_stack.clone();
        enter_category(&mut state, Category::Storage);
        assert_eq!(state.view, View::StorageBlobs, "view must not move");
        assert_eq!(state.view_stack, stack_before, "stack must not grow");
        assert_eq!(state.storage.accounts_cursor, 4, "cursor must not reset");
        assert_eq!(state.last_category, Category::Storage);
    }

    #[test]
    fn category_clear_cache_wipes_only_its_own_caches() {
        // Each category's clear_cache should reset *its* state without
        // touching the others. Catches the kind of cross-contamination bug
        // where Storage::clear accidentally wiped registries.
        let mut state = AppState::new(Config::default());
        state.resources.push(crate::azure::resources::Resource {
            id: "r1".into(),
            name: "n".into(),
            kind: crate::azure::resources::ResourceKind::FunctionApp,
            location: "westeurope".into(),
            resource_group: "rg".into(),
            subscription_id: "s".into(),
            state: None,
            created_at: None,
            modified_at: None,
            meta: Default::default(),
        });
        state.storage.accounts = Some(Vec::new());
        state.registry.registries = Some(Vec::new());
        state.cosmos.accounts = Some(Vec::new());
        state.key_vault.vaults = Some(Vec::new());
        state.service_bus.namespaces = Some(Vec::new());

        Category::Storage.clear_cache(&mut state);
        assert!(state.storage.accounts.is_none(), "storage cleared");
        assert!(
            !state.resources.is_empty(),
            "apis untouched by storage clear"
        );
        assert!(
            state.registry.registries.is_some(),
            "registries untouched by storage clear"
        );
        assert!(
            state.cosmos.accounts.is_some(),
            "cosmos untouched by storage clear"
        );
        assert!(
            state.key_vault.vaults.is_some(),
            "key vaults untouched by storage clear"
        );

        Category::Registries.clear_cache(&mut state);
        assert!(state.registry.registries.is_none(), "registries cleared");
        assert!(
            !state.resources.is_empty(),
            "apis untouched by registries clear"
        );
        assert!(
            state.cosmos.accounts.is_some(),
            "cosmos untouched by registries clear"
        );

        Category::Cosmos.clear_cache(&mut state);
        assert!(state.cosmos.accounts.is_none(), "cosmos cleared");
        assert!(
            !state.resources.is_empty(),
            "apis untouched by cosmos clear"
        );
        assert!(
            state.key_vault.vaults.is_some(),
            "key vaults untouched by cosmos clear"
        );

        Category::KeyVaults.clear_cache(&mut state);
        assert!(state.key_vault.vaults.is_none(), "key vaults cleared");
        assert!(
            state.service_bus.namespaces.is_some(),
            "service bus untouched by key vaults clear"
        );

        Category::ServiceBus.clear_cache(&mut state);
        assert!(
            state.service_bus.namespaces.is_none(),
            "service bus cleared"
        );

        Category::Apis.clear_cache(&mut state);
        assert!(state.resources.is_empty(), "apis cleared");
    }

    #[test]
    fn palette_aliases_drop_single_letters() {
        // Single-letter palette aliases were dropped to keep Tab completion
        // clean. The keybind shortcuts (`S` / `R`) are unaffected.
        let apis = Category::Apis.palette_aliases();
        assert!(
            !apis.contains(&"a") && !apis.contains(&"r") && !apis.contains(&"resources"),
            "Apis should expose only `apis`, got {apis:?}"
        );
        let storage = Category::Storage.palette_aliases();
        assert!(
            !storage.contains(&"s"),
            "Storage should not expose `s`, got {storage:?}"
        );
        assert!(
            Category::Cosmos.palette_aliases().contains(&"cosmos"),
            "Cosmos palette alias missing"
        );
        let kv = Category::KeyVaults.palette_aliases();
        assert!(
            kv.contains(&"keyvaults") && kv.contains(&"kv"),
            "Key Vault palette aliases missing, got {kv:?}"
        );
    }

    #[test]
    fn category_palette_aliases_are_disjoint() {
        // Aliases must not collide across categories — otherwise `:s` could
        // route to two different categories depending on whichever
        // `Category::ALL` lookup arrived first. This locks the invariant.
        use std::collections::HashSet;
        let mut seen: HashSet<&str> = HashSet::new();
        for category in Category::ALL {
            for alias in category.palette_aliases() {
                assert!(
                    seen.insert(*alias),
                    "alias `{alias}` is claimed by more than one category",
                );
            }
        }
    }
}
