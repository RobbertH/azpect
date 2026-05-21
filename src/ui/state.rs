//! Shared application state. Both the event loop (Lane 3) and the view
//! renderers (Lane 4) read this struct, so it's part of the contract.

#![allow(dead_code, unused_variables)]

use std::collections::{HashMap, HashSet};

use tui_input::Input;

use crate::azure::logs::LogLine;
use crate::azure::metrics::{MetricKind, MetricSeries, TimeRange};
use crate::azure::resources::Resource;
use crate::azure::subscriptions::Subscription;
use crate::config::Config;

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
    /// Application-Gateway-only: backend pools (and their members) for the
    /// selected gateway. Opened with Enter from `List` when the resource is
    /// `ResourceKind::AppGateway`.
    AppGatewayBackends,
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

/// Per-Container-App configured CPU/memory caps from the resource template,
/// rendered alongside the CPU/Memory metrics as "latest: X / max Y". Cached
/// per resource id; only populated for `ResourceKind::ContainerApp`.
#[derive(Clone, Default)]
pub struct LimitsCache {
    pub by_resource: HashMap<String, crate::azure::container_app_limits::ContainerAppLimits>,
    pub pending: HashSet<String>,
}

/// Per-Container-App active revision metadata (name, image, replicas, scale)
/// from the revisions endpoint. Populated by the same fetch that drives the
/// health badge for Container Apps.
#[derive(Clone, Default)]
pub struct RevisionMetaCache {
    pub by_resource: HashMap<String, crate::azure::container_app_revisions::ActiveRevisionMeta>,
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

    /// Keyed by API resource id (`{service}/apis/{apiName}`).
    pub operations: HashMap<String, Vec<crate::azure::apim::Operation>>,
    pub operations_pending: HashSet<String>,
    pub operations_error: HashMap<String, String>,
    pub operations_cursor: usize,
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

#[derive(Clone, Default)]
pub struct LogsCache {
    /// keyed by resource id
    pub by_resource: HashMap<String, Vec<LogLine>>,
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
    pub loading: bool,
    pub last_error: Option<String>,
    /// Scroll offset/cursor inside the logs table.
    /// Kept separate from `AppState::list_cursor` so navigating logs does not corrupt the resource selection in the List view.
    pub scroll: usize,
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
}

/// Top-level UI state. Lane 3 mutates this in response to events; Lane 4 reads it for rendering.
pub struct AppState {
    pub config: Config,
    pub view: View,
    /// Stack of views the user has navigated through. Pushed on every forward
    /// transition (e.g. Subs -> List -> Detail -> Logs); `Action::Back` pops.
    /// Empty stack + Back triggers a quit.
    pub view_stack: Vec<View>,

    pub subscriptions: Vec<Subscription>,
    pub selected_subscription: Option<String>,
    pub subscription_cursor: usize,
    pub loading_subscriptions: bool,

    pub resources: Vec<Resource>,
    pub list_cursor: usize,
    pub list_filter: Input,
    pub list_filter_active: bool,
    pub favorites_only: bool,
    pub loading_resources: bool,

    pub metrics: MetricsCache,
    pub health: HealthCache,
    pub logs: LogsCache,
    pub limits: LimitsCache,
    pub revision_meta: RevisionMetaCache,
    pub apim: ApimCache,
    pub appgw: AppGatewayBackendsCache,
    pub storage: StorageCache,

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
}

impl AppState {
    pub fn new(config: Config) -> Self {
        let range = config.default_window;
        Self {
            view: View::Subscriptions,
            view_stack: Vec::new(),
            subscriptions: Vec::new(),
            selected_subscription: config.last_subscription_id.clone(),
            subscription_cursor: 0,
            loading_subscriptions: true,
            resources: Vec::new(),
            list_cursor: 0,
            list_filter: Input::default(),
            list_filter_active: false,
            favorites_only: false,
            loading_resources: false,
            metrics: MetricsCache {
                range,
                ..Default::default()
            },
            health: HealthCache::default(),
            logs: LogsCache {
                range,
                ..Default::default()
            },
            limits: LimitsCache::default(),
            revision_meta: RevisionMetaCache::default(),
            apim: ApimCache::default(),
            appgw: AppGatewayBackendsCache::default(),
            storage: StorageCache::default(),
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
            config,
        }
    }

    /// Resource currently under the cursor in the list view, after applying filter.
    pub fn selected_resource(&self) -> Option<&Resource> {
        // Lane 3/4 will likely want a filtered iterator helper; this naive impl is a placeholder.
        self.filtered_resources().get(self.list_cursor).copied()
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
    use super::is_subsequence;

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
}
