//! Event types flowing through the single `mpsc::UnboundedReceiver<AppEvent>`
//! that drives the UI loop.
//!
//! ## Vim-flavored input model
//!
//! Cursor movement uses `h j k` and chords like `g g`. Vertical movement is
//! the only direction the views use, so `l` is repurposed (k9s-style) to open
//! the logs view. Single-letter actions (`l` logs, `f` favorite, `s`
//! subscription, `r` refresh, `1/7` window, `w` wrap, `e` errors-only, `q`
//! quit) avoid clobbering `j`/`k`. Lane 3 is responsible for the chord state
//! machine (e.g. tracking the first `g` of `g g`).

#![allow(dead_code, unused_variables)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::azure::metrics::MetricsResult;
use crate::azure::resources::Resource;
use crate::azure::subscriptions::Subscription;
use crate::ui::state::View;

/// Everything that can happen to the app.
#[derive(Debug)]
pub enum AppEvent {
    /// Periodic tick (e.g. for clock in title bar, in-flight spinner).
    Tick,
    /// Raw keyboard input from crossterm.
    Key(KeyEvent),
    /// Terminal resize.
    Resize { width: u16, height: u16 },

    /// Background load completion: subscription list.
    SubscriptionsLoaded(Result<Vec<Subscription>, String>),
    /// Background load completion: resource list for the active subscription set.
    ResourcesLoaded(Result<Vec<Resource>, String>),
    /// Background load completion: metrics for a specific resource id. The
    /// success payload carries both the loaded series and a per-metric error
    /// map for ones that the resource's plan doesn't expose.
    MetricsLoaded {
        resource_id: String,
        result: Result<MetricsResult, String>,
    },
    /// Background load completion: logs for a specific resource id.
    ///
    /// `append` distinguishes the initial / refresh fetch (`false` — replace
    /// the cached vec) from a paginated older-than fetch (`true` — push the
    /// page onto the end of the existing vec).
    LogsLoaded {
        resource_id: String,
        append: bool,
        /// The `logs.generation` this fetch was issued under. The handler drops
        /// the page if it no longer matches the current generation — a stale
        /// in-flight fetch must not clobber a buffer the user has since refiltered.
        generation: u64,
        result: Result<crate::azure::logs::LogsPage, String>,
    },
    /// Background load completion: Azure Resource Health availability for a
    /// specific resource id.
    HealthLoaded {
        resource_id: String,
        result: Result<crate::azure::resource_health::ResourceAvailability, String>,
    },
    /// Background load completion: the fixed-24h Errors+Traffic series used to
    /// compute the health badge, independent of the chart's selected range.
    /// Fired alongside `HealthLoaded` from `spawn_load_health`.
    HealthMetricsLoaded {
        resource_id: String,
        result: Result<Vec<crate::azure::metrics::MetricSeries>, String>,
    },
    /// Background load completion: configured CPU/memory caps from a
    /// Container App's template. Only fired for `ResourceKind::ContainerApp`.
    ContainerAppOverviewLoaded {
        resource_id: String,
        result: Result<crate::azure::container_app_overview::ContainerAppOverview, String>,
    },
    /// Background load completion: active-revision metadata (name, image,
    /// replicas, scale) from a Container App. Fired alongside `HealthLoaded`
    /// for Container Apps since both come from the same revisions response.
    ContainerAppRevisionMetaLoaded {
        resource_id: String,
        result: Result<Option<crate::azure::container_app_revisions::ActiveRevisionMeta>, String>,
    },
    /// Background load completion: per-replica live status (containers, ready,
    /// restart count) for a Container App's active revision. Fired downstream
    /// of `ContainerAppRevisionMetaLoaded` since the replicas endpoint needs
    /// the revision name in its path.
    ContainerAppReplicasLoaded {
        resource_id: String,
        result: Result<Vec<crate::azure::container_app_replicas::ReplicaInstance>, String>,
    },
    /// Background load completion: a Function App's `config/web` — its deployed
    /// container image (for the VERSION column) plus its public-access posture
    /// (for the Detail `network:` row). Only fired for `ResourceKind::FunctionApp`.
    /// `Err` leaves both blank silently.
    FunctionAppImageLoaded {
        resource_id: String,
        result: Result<crate::azure::function_app_config::WebConfig, String>,
    },
    /// Background load completion: a Function App's application settings (OS env
    /// vars) from the `config/appsettings/list` action. Only fired for
    /// `ResourceKind::FunctionApp`. `Err` is commonly a 403 for read-only
    /// principals (the action returns secrets).
    FunctionAppSettingsLoaded {
        resource_id: String,
        result: Result<Vec<crate::azure::env_vars::EnvVar>, String>,
    },
    /// Background load completion: a Function App's per-function trigger summary
    /// from the `functions` list. Only fired for `ResourceKind::FunctionApp`.
    /// `Ok(vec![])` means no functions are synced to ARM (e.g. run-from-package
    /// apps); `Err` is surfaced as a hint in the Detail overview.
    FunctionAppTriggersLoaded {
        resource_id: String,
        result: Result<Vec<crate::azure::function_app_triggers::FunctionTrigger>, String>,
    },
    /// Background load completion: a directory principal's display name resolved
    /// via Microsoft Graph (best-effort; `Err` / `Ok(None)` ⇒ fall back to the
    /// object-id). Keyed by the object-id.
    PrincipalResolved {
        object_id: String,
        result: Result<Option<String>, String>,
    },
    /// Background load completion: list of APIs for an APIM service.
    ApimApisLoaded {
        service_id: String,
        result: Result<Vec<crate::azure::apim::Api>, String>,
    },
    /// Background load completion: list of operations for an APIM API.
    ApimOperationsLoaded {
        api_id: String,
        result: Result<Vec<crate::azure::apim::Operation>, String>,
    },
    /// Background load completion: policy XML for an APIM operation. `Ok(None)`
    /// means APIM reported no policy is configured (404 swallowed in the
    /// fetcher); `Ok(Some(xml))` is the raw document.
    ApimOperationPolicyLoaded {
        operation_id: String,
        result: Result<Option<String>, String>,
    },
    /// Background load completion: backend pools (and their members) for one
    /// Application Gateway, keyed by gateway resource id.
    AppGatewayBackendsLoaded {
        resource_id: String,
        result: Result<Vec<crate::azure::appgw_backends::BackendPool>, String>,
    },
    /// Background load completion: list of storage accounts for the current
    /// subscription scope.
    StorageAccountsLoaded(Result<Vec<crate::azure::storage::StorageAccount>, String>),
    /// Background load completion: list of blob containers for one storage
    /// account, keyed by ARM id.
    StorageContainersLoaded {
        account_id: String,
        result: Result<Vec<crate::azure::storage::BlobContainer>, String>,
    },
    /// Background load completion: per-account aggregate stats (container /
    /// blob / file / queue / table counts and totals) for the storage account
    /// overview view, keyed by ARM id.
    StorageOverviewLoaded {
        account_id: String,
        result: Result<crate::azure::storage::StorageAccountStats, String>,
    },
    /// Background load completion: list of blobs in one container. `key` is the
    /// `(account, container)` pair flattened by
    /// [`crate::ui::state::StorageCache::blobs_key`].
    StorageBlobsLoaded {
        key: String,
        result: Result<Vec<crate::azure::storage::Blob>, String>,
    },
    /// Background load completion: metadata + body preview for one blob. `key`
    /// is the `(account, container, blob)` triple flattened by
    /// [`crate::ui::state::StorageCache::blob_preview_key`].
    StorageBlobPreviewLoaded {
        key: String,
        result: Result<crate::azure::storage::BlobPreview, String>,
    },
    /// Background load completion: list of container registries for the
    /// current subscription scope.
    RegistriesLoaded(Result<Vec<crate::azure::registries::Registry>, String>),
    /// Background load completion: list of repositories inside one registry,
    /// keyed by registry ARM id.
    RegistryRepositoriesLoaded {
        registry_id: String,
        result: Result<Vec<crate::azure::registries::Repository>, String>,
    },
    /// Background load completion: list of tags for one repository, keyed by
    /// the `(registry_id, repository)` pair flattened by
    /// [`crate::ui::state::RegistryCache::tags_key`].
    RegistryTagsLoaded {
        key: String,
        result: Result<Vec<crate::azure::registries::Tag>, String>,
    },
    /// Background load completion: list of Cosmos DB (SQL/Core) accounts for
    /// the current subscription scope.
    CosmosAccountsLoaded(Result<Vec<crate::azure::cosmos::CosmosAccount>, String>),
    /// Background load completion: list of SQL databases inside one Cosmos
    /// account, keyed by account ARM id.
    CosmosDatabasesLoaded {
        account_id: String,
        result: Result<Vec<crate::azure::cosmos::CosmosDatabase>, String>,
    },
    /// Background load completion: list of SQL containers inside one database.
    /// `key` is the `(account_id, db_name)` pair flattened by
    /// [`crate::ui::state::CosmosCache::containers_key`].
    CosmosContainersLoaded {
        key: String,
        result: Result<Vec<crate::azure::cosmos::CosmosContainer>, String>,
    },
    /// Background load completion: first-20 item preview for one container.
    /// `key` is the `(account_id, db, coll)` triple flattened by
    /// [`crate::ui::state::CosmosCache::items_key`].
    CosmosItemsLoaded {
        key: String,
        result: Result<crate::azure::cosmos::CosmosItemPreview, String>,
    },
    /// Background load completion: flat list of Azure SQL elastic pools +
    /// single databases for the current subscription scope. Resource Graph
    /// control-plane call.
    SqlResourcesLoaded(Result<Vec<crate::azure::sql::SqlResource>, String>),
    /// Background load completion: utilization metrics for one pool / database,
    /// keyed by ARM id. Mirrors [`AppEvent::MetricsLoaded`] but scoped to the
    /// SQL category's cache.
    SqlMetricsLoaded {
        resource_id: String,
        result: Result<crate::azure::metrics::MetricsResult, String>,
    },
    /// Background load completion: list of Key Vaults for the current
    /// subscription scope. Resource Graph control-plane call.
    KeyVaultsLoaded(Result<Vec<crate::azure::key_vault::KeyVault>, String>),
    /// Background load completion: list of secrets *or* certificates inside
    /// one vault. `key` is the `(vault_id, kind)` pair flattened by
    /// [`crate::ui::state::KeyVaultCache::items_key`]. Data-plane call.
    KeyVaultItemsLoaded {
        key: String,
        result: Result<Vec<crate::azure::key_vault::KeyVaultItem>, String>,
    },
    /// Background load completion: the decoded plaintext value of a single
    /// secret, fetched on an explicit user reveal (`x` / Enter). Carries
    /// `(vault_id, name)` so a stale result can't populate a modal the user
    /// has since closed or reopened on a different secret. Data-plane call.
    KeyVaultSecretValueLoaded {
        vault_id: String,
        name: String,
        result: Result<String, String>,
    },
    /// Background load completion: list of Service Bus namespaces for the
    /// current subscription scope. Resource Graph control-plane call.
    ServiceBusNamespacesLoaded(Result<Vec<crate::azure::service_bus::ServiceBusNamespace>, String>),
    /// Background load completion: list of queues inside one namespace, keyed
    /// by namespace ARM id.
    ServiceBusQueuesLoaded {
        namespace_id: String,
        result: Result<Vec<crate::azure::service_bus::ServiceBusQueue>, String>,
    },
    /// Background load completion: list of topics inside one namespace, keyed
    /// by namespace ARM id.
    ServiceBusTopicsLoaded {
        namespace_id: String,
        result: Result<Vec<crate::azure::service_bus::ServiceBusTopic>, String>,
    },
    /// Background load completion: list of subscriptions on one topic. `key` is
    /// the `(namespace_id, topic)` pair flattened by
    /// [`crate::ui::state::ServiceBusCache::subscriptions_key`].
    ServiceBusSubscriptionsLoaded {
        key: String,
        result: Result<Vec<crate::azure::service_bus::ServiceBusSubscription>, String>,
    },
    /// Completion of a guarded env-var write (add or edit). On `Ok` the cache is
    /// updated optimistically from `applied` and a confirming refetch is kicked
    /// off; on `Err` the message is shown in the still-open editor so the user
    /// can retry or cancel. `is_demo` suppresses the (pointless) refetch in the
    /// mock tenant so the simulated edit sticks.
    EnvVarWriteCompleted {
        applied: crate::ui::state::AppliedEnvEdit,
        is_demo: bool,
        result: Result<(), String>,
    },
}

/// Logical actions produced by the input handler. Lane 3 maps `KeyEvent` →
/// `Action` then applies the action to `AppState`. Centralising this keeps the
/// keymap declarative and makes tests easy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    Quit,
    Back,
    MoveLeft,
    MoveDown,
    MoveUp,
    MoveRight,
    HalfPageDown,
    HalfPageUp,
    GotoTop,
    GotoBottom,
    NextPanel,
    PrevPanel,
    OpenSelected,
    OpenLogs,
    ToggleErrorsOnly,
    ToggleFavorite,
    ToggleFavoritesOnly,
    StartSearch,
    /// Jump cursor to the next/previous log line whose source or message
    /// contains `state.logs.search_input` (case-insensitive). Only meaningful
    /// in the logs view; no-op elsewhere.
    NextMatch,
    PrevMatch,
    SwitchSubscription,
    /// Cycle the logs view's client-side source filter through the distinct
    /// `LogLine::source` values in the cached buffer (all → A → B → … → all).
    /// Bound to `Tab` / `Shift+Tab` (forward / back) inside the logs view, shown
    /// as a source tab-bar atop the page. (Previously `s`; freed so `s` can mean
    /// shell, k9s-style.)
    CycleSourceFilter,
    /// Reverse of [`Self::CycleSourceFilter`] — bound to `Shift+Tab` in logs.
    CycleSourceFilterBack,
    /// Shell into the selected Container App's running container via
    /// `az containerapp exec` (k9s-style). Bound to `s` in the Detail and Logs
    /// views; for non-Container-App resources the handler falls back to `s`'s
    /// global switch-subscription meaning.
    ShellIntoContainer,
    /// Open the top-level Storage mode (blob accounts list). Bound to `S`
    /// (capital so it doesn't collide with `s` = switch subscription).
    OpenStorage,
    /// Open the top-level Container Registries mode. Bound to `R` (capital so
    /// it doesn't collide with `r` = refresh).
    OpenRegistries,
    Refresh,
    SetWindowHour,
    SetWindowDay,
    SetWindowWeek,
    /// Toggle word-wrap in the logs view so long source/message cells render
    /// as multi-line rows instead of being truncated. Bound to `w`.
    ToggleWrap,
    /// Open the dedicated env-vars page for the selected API asset. Bound to
    /// `e` while in Detail (in the logs views `e` stays errors-only). No-op for
    /// resource kinds without env vars.
    OpenEnvVars,
    /// Reveal / re-mask (k9s-style "decode") env-var values in the env-vars
    /// page. Bound to `x`.
    DecodeSecret,
    /// Open the guarded editor on the selected env var. Bound to `Ctrl+E` in the
    /// env-vars page only — the Ctrl modifier keeps write-mode deliberately hard
    /// to enter by accident.
    EditEnvVar,
    /// Open the guarded editor to add a new env var. Bound to `Ctrl+N` in the
    /// env-vars page only.
    AddEnvVar,
    Help,
    /// Open the vim/k9s-style command palette (`:`).
    StartCommand,
    /// Vim-style yank: copy something contextual (selected log line, displayed
    /// error, selected resource id, …) to the system clipboard via OSC52.
    Yank,
    /// Vim-style visual-line mode toggle (`V`). In the logs view this anchors a
    /// multi-line selection at the current row; `j`/`k` extend it and `y` yanks
    /// the whole span. Pressing `V` again (or `Esc`) cancels. No-op elsewhere.
    ToggleVisualLine,
    /// Open the contextual target (selected resource / subscription) in the
    /// Azure Portal in the system default browser.
    OpenInBrowser,
    /// Sentinel emitted on the *first* `g` of a `g g` chord. The event loop
    /// stashes pending state and waits for the second key. Also returned for
    /// any key the input handler doesn't recognise.
    Noop,
}

/// Translate a single key event into a logical [`Action`].
///
/// `view` is provided so view-specific keymaps can diverge later (currently
/// only used to decide that the help overlay treats every key as Back).
///
/// `search_active` reflects whether `state.list_filter_active` is set. When
/// the search input has focus, all *printable* keys belong to the input field
/// and we surface them as [`Action::Noop`]; only `Esc` (close) and `Enter`
/// (apply) reach the global handler.
///
/// The `g g` chord is **not** resolved here — it requires history. The caller
/// (event loop) holds the chord state and consults [`is_chord_starter`] /
/// [`resolve_chord`].
pub fn key_to_action(key: KeyEvent, view: View, search_active: bool) -> Action {
    // Search-mode capture: the input field eats most keys, but vertical
    // navigation must continue to drive the underlying list so the user can
    // pick a row without leaving the search box.
    if search_active {
        return match key.code {
            KeyCode::Esc => Action::Back,
            KeyCode::Enter => Action::OpenSelected,
            KeyCode::Up => Action::MoveUp,
            KeyCode::Down => Action::MoveDown,
            KeyCode::PageDown => Action::HalfPageDown,
            KeyCode::PageUp => Action::HalfPageUp,
            _ => Action::Noop,
        };
    }

    // Help view: any key dismisses.
    if view == View::Help {
        return match key.code {
            KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q') => Action::Back,
            _ => Action::Back,
        };
    }

    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    match key.code {
        KeyCode::Esc => Action::Back,

        // Guarded env-var write entry. Ctrl-gated and scoped to the env-vars
        // page so it can never collide with `e` (errors-only) / `n` (next match)
        // elsewhere, and so write mode takes a deliberate chord to enter.
        KeyCode::Char('e') if ctrl && view == View::EnvVars => Action::EditEnvVar,
        KeyCode::Char('n') if ctrl && view == View::EnvVars => Action::AddEnvVar,

        // Navigation
        KeyCode::Char('h') if !ctrl => Action::MoveLeft,
        KeyCode::Char('j') if !ctrl => Action::MoveDown,
        KeyCode::Char('k') if !ctrl => Action::MoveUp,
        // `l` is intentionally NOT bound to MoveRight: k9s-style, lowercase `l`
        // opens the logs view (see the `Char('l')` arm further down).
        KeyCode::Down => Action::MoveDown,
        KeyCode::Up => Action::MoveUp,
        KeyCode::Left => Action::MoveLeft,
        KeyCode::Right => Action::MoveRight,

        // Half-page jumps (Ctrl-d / Ctrl-u). NB ratatui PageUp/PageDown also map.
        KeyCode::Char('d') if ctrl => Action::HalfPageDown,
        KeyCode::Char('u') if ctrl => Action::HalfPageUp,
        KeyCode::PageDown => Action::HalfPageDown,
        KeyCode::PageUp => Action::HalfPageUp,

        // Top / bottom. `g` alone is a chord starter; the caller resolves `g g`.
        KeyCode::Char('G') => Action::GotoBottom,
        KeyCode::Char('g') => Action::Noop, // chord starter — handled by caller

        // Search match navigation (logs view consumes; other views ignore).
        KeyCode::Char('n') => Action::NextMatch,
        KeyCode::Char('N') => Action::PrevMatch,

        // Panel cycling — except in the logs view, where Tab / Shift+Tab cycle
        // the source filter (shown as a tab-bar). Routing it to a dedicated
        // action keeps `after_action` from treating it like a panel switch and
        // refetching the log buffer on every keystroke.
        KeyCode::Tab => match view {
            View::Logs => Action::CycleSourceFilter,
            _ => Action::NextPanel,
        },
        KeyCode::BackTab => match view {
            View::Logs => Action::CycleSourceFilterBack,
            _ => Action::PrevPanel,
        },

        KeyCode::Enter => Action::OpenSelected,

        // Action keys (uppercase or distinct from hjkl).
        // Lowercase `l` opens the logs view (k9s-style), but when the user is
        // *already* inside the logs/log-detail view we fall back to vim-style
        // MoveRight so the key isn't a no-op there. Uppercase `L` is kept as
        // a universal alias for muscle memory.
        KeyCode::Char('l') if !ctrl => match view {
            // Horizontal nav in views that use it; elsewhere `l` opens logs.
            View::Logs | View::LogDetail | View::EnvVars => Action::MoveRight,
            _ => Action::OpenLogs,
        },
        KeyCode::Char('L') => Action::OpenLogs,
        // `e` opens the env-vars page from Detail, but stays errors-only inside
        // the logs views (mirrors how `l` is context-aware).
        KeyCode::Char('e') => match view {
            View::Logs | View::LogDetail => Action::ToggleErrorsOnly,
            View::Detail => Action::OpenEnvVars,
            _ => Action::ToggleErrorsOnly,
        },
        KeyCode::Char('x') => Action::DecodeSecret,
        KeyCode::Char('f') => Action::ToggleFavorite,
        KeyCode::Char('F') => Action::ToggleFavoritesOnly,
        KeyCode::Char('/') => Action::StartSearch,
        // `s` shells into a container (k9s-style) on the Container App Detail /
        // Logs views; the handler falls back to switch-subscription when the
        // resource isn't a Container App. Everywhere else it switches
        // subscription directly. (Log source cycling moved to `Tab`.)
        KeyCode::Char('s') => match view {
            View::Detail | View::Logs => Action::ShellIntoContainer,
            _ => Action::SwitchSubscription,
        },
        KeyCode::Char('S') => Action::OpenStorage,
        KeyCode::Char('R') => Action::OpenRegistries,
        KeyCode::Char('r') => Action::Refresh,
        // Digit-keyed time-range shortcuts, grouped numerically:
        // `0` → 1h (less than a day), `1` → 1d, `7` → 7d (a week).
        KeyCode::Char('0') => Action::SetWindowHour,
        KeyCode::Char('1') => Action::SetWindowDay,
        KeyCode::Char('7') => Action::SetWindowWeek,
        KeyCode::Char('w') => Action::ToggleWrap,
        KeyCode::Char('?') => Action::Help,
        KeyCode::Char(':') => Action::StartCommand,
        KeyCode::Char('y') => Action::Yank,
        // Visual-line mode (k9s/vim `V`). Only the logs view acts on it; other
        // views let it fall through to the global handler as a no-op.
        KeyCode::Char('V') => Action::ToggleVisualLine,
        KeyCode::Char('o') => Action::OpenInBrowser,
        KeyCode::Char('q') => Action::Back,

        _ => Action::Noop,
    }
}

/// Returns `true` when this key event is the *first* `g` of a potential `g g`
/// chord. The event loop should stash the time and wait for the next key.
pub fn is_chord_starter(key: KeyEvent, search_active: bool) -> bool {
    !search_active
        && matches!(key.code, KeyCode::Char('g'))
        && !key.modifiers.contains(KeyModifiers::CONTROL)
}

/// Given a pending chord starter and the next key, return the resolved action
/// (and `true` if the chord was consumed). If the second key doesn't complete
/// any known chord, the caller should clear the pending state and process the
/// new key normally.
pub fn resolve_chord(starter: char, next: KeyEvent) -> Option<Action> {
    match (starter, next.code) {
        ('g', KeyCode::Char('g')) => Some(Action::GotoTop),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }
    fn key_shift(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::SHIFT)
    }
    fn key_ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn hjk_maps_to_directions_and_l_opens_logs() {
        // Lowercase `l` opens logs from non-logs views; inside the logs view
        // itself it falls back to MoveRight so it isn't a no-op there.
        let v = View::List;
        assert_eq!(key_to_action(key('h'), v, false), Action::MoveLeft);
        assert_eq!(key_to_action(key('j'), v, false), Action::MoveDown);
        assert_eq!(key_to_action(key('k'), v, false), Action::MoveUp);
        assert_eq!(key_to_action(key('l'), v, false), Action::OpenLogs);
        assert_eq!(
            key_to_action(key('l'), View::Logs, false),
            Action::MoveRight
        );
        assert_eq!(
            key_to_action(key('l'), View::LogDetail, false),
            Action::MoveRight
        );
    }

    #[test]
    fn arrow_keys_also_navigate() {
        let v = View::List;
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(key_to_action(down, v, false), Action::MoveDown);
        assert_eq!(key_to_action(up, v, false), Action::MoveUp);
    }

    #[test]
    fn both_cases_of_l_open_logs() {
        // Lowercase mimics k9s; uppercase is kept as an alias for muscle memory.
        let v = View::Detail;
        assert_eq!(key_to_action(key_shift('L'), v, false), Action::OpenLogs);
        assert_eq!(key_to_action(key('l'), v, false), Action::OpenLogs);
    }

    #[test]
    fn gg_chord_via_resolve_chord() {
        // First `g` is reported as a chord starter, returning Noop.
        assert_eq!(key_to_action(key('g'), View::List, false), Action::Noop);
        assert!(is_chord_starter(key('g'), false));
        // Second `g` resolves to GotoTop.
        assert_eq!(resolve_chord('g', key('g')), Some(Action::GotoTop));
        // `g` followed by something else does not resolve.
        assert!(resolve_chord('g', key('j')).is_none());
    }

    #[test]
    fn capital_g_jumps_to_bottom_directly() {
        assert_eq!(
            key_to_action(key_shift('G'), View::List, false),
            Action::GotoBottom
        );
    }

    #[test]
    fn search_active_captures_letters_but_lets_esc_and_enter_through() {
        let v = View::List;
        // Letters become Noop because the input field consumes them.
        assert_eq!(key_to_action(key('j'), v, true), Action::Noop);
        assert_eq!(key_to_action(key('q'), v, true), Action::Noop);
        assert_eq!(key_to_action(key('L'), v, true), Action::Noop);
        // Esc cancels search → Back.
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(key_to_action(esc, v, true), Action::Back);
        // Enter applies search → OpenSelected sentinel.
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(key_to_action(enter, v, true), Action::OpenSelected);
    }

    #[test]
    fn search_active_lets_arrow_keys_through_for_list_navigation() {
        let v = View::List;
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        let pgdn = KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE);
        let pgup = KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE);
        assert_eq!(key_to_action(down, v, true), Action::MoveDown);
        assert_eq!(key_to_action(up, v, true), Action::MoveUp);
        assert_eq!(key_to_action(pgdn, v, true), Action::HalfPageDown);
        assert_eq!(key_to_action(pgup, v, true), Action::HalfPageUp);
    }

    #[test]
    fn ctrl_d_and_ctrl_u_are_half_page() {
        let v = View::List;
        assert_eq!(key_to_action(key_ctrl('d'), v, false), Action::HalfPageDown);
        assert_eq!(key_to_action(key_ctrl('u'), v, false), Action::HalfPageUp);
        // Lowercase `d` without ctrl is no longer bound — `1` is the day-window
        // action now that `w` is repurposed for wrap.
        assert_eq!(key_to_action(key('d'), v, false), Action::Noop);
        assert_eq!(key_to_action(key('1'), v, false), Action::SetWindowDay);
    }

    #[test]
    fn capital_v_toggles_visual_line() {
        // `V` maps to the visual-line toggle; only the logs view acts on it,
        // but the keymap itself is view-independent.
        assert_eq!(
            key_to_action(key_shift('V'), View::Logs, false),
            Action::ToggleVisualLine
        );
        assert_eq!(
            key_to_action(key_shift('V'), View::List, false),
            Action::ToggleVisualLine
        );
    }

    #[test]
    fn capital_s_opens_storage_mode() {
        // Lowercase `s` switches subscription; capital `S` enters the
        // top-level Storage mode. Keep them distinct so muscle memory for
        // the existing subscription picker doesn't accidentally yank the
        // user out of their current view.
        let v = View::List;
        assert_eq!(
            key_to_action(key('s'), v, false),
            Action::SwitchSubscription
        );
        assert_eq!(key_to_action(key_shift('S'), v, false), Action::OpenStorage);
    }

    #[test]
    fn action_keys_match_table() {
        let v = View::List;
        assert_eq!(key_to_action(key('e'), v, false), Action::ToggleErrorsOnly);
        assert_eq!(key_to_action(key('f'), v, false), Action::ToggleFavorite);
        assert_eq!(
            key_to_action(key_shift('F'), v, false),
            Action::ToggleFavoritesOnly
        );
        assert_eq!(key_to_action(key('/'), v, false), Action::StartSearch);
        assert_eq!(
            key_to_action(key('s'), v, false),
            Action::SwitchSubscription
        );
        assert_eq!(key_to_action(key('r'), v, false), Action::Refresh);
        assert_eq!(key_to_action(key('0'), v, false), Action::SetWindowHour);
        assert_eq!(key_to_action(key('7'), v, false), Action::SetWindowWeek);
        assert_eq!(key_to_action(key('w'), v, false), Action::ToggleWrap);
        assert_eq!(key_to_action(key('?'), v, false), Action::Help);
        assert_eq!(key_to_action(key(':'), v, false), Action::StartCommand);
        assert_eq!(key_to_action(key('q'), v, false), Action::Back);
        assert_eq!(key_to_action(key('o'), v, false), Action::OpenInBrowser);
    }

    #[test]
    fn s_shells_into_container_on_detail_and_logs() {
        // `s` is the k9s shell key on the Detail and Logs views (the handler
        // decides whether the resource is actually a Container App). Source
        // cycling moved to Tab; the per-line detail keeps switch-subscription.
        assert_eq!(
            key_to_action(key('s'), View::Detail, false),
            Action::ShellIntoContainer
        );
        assert_eq!(
            key_to_action(key('s'), View::Logs, false),
            Action::ShellIntoContainer
        );
        assert_eq!(
            key_to_action(key('s'), View::LogDetail, false),
            Action::SwitchSubscription
        );
    }

    #[test]
    fn tab_cycles_panels() {
        let v = View::List;
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        let backtab = KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT);
        assert_eq!(key_to_action(tab, v, false), Action::NextPanel);
        assert_eq!(key_to_action(backtab, v, false), Action::PrevPanel);
    }

    #[test]
    fn help_view_dismisses_on_any_key() {
        assert_eq!(key_to_action(key('x'), View::Help, false), Action::Back);
        assert_eq!(key_to_action(key('?'), View::Help, false), Action::Back);
    }

    #[test]
    fn ctrl_e_n_enter_write_mode_only_inside_env_vars() {
        // Ctrl-gated + view-scoped so write mode is deliberate and never steals
        // `e` (errors-only) / `n` (next match) elsewhere.
        assert_eq!(
            key_to_action(key_ctrl('e'), View::EnvVars, false),
            Action::EditEnvVar
        );
        assert_eq!(
            key_to_action(key_ctrl('n'), View::EnvVars, false),
            Action::AddEnvVar
        );
        // Outside the env-vars page the chords fall through to their normal
        // (non-write) meanings.
        assert_eq!(
            key_to_action(key_ctrl('e'), View::Detail, false),
            Action::OpenEnvVars
        );
        assert_ne!(
            key_to_action(key_ctrl('n'), View::List, false),
            Action::AddEnvVar
        );
        // Plain (un-modified) e/n in the env-vars page must NOT enter write mode.
        assert_ne!(
            key_to_action(key('e'), View::EnvVars, false),
            Action::EditEnvVar
        );
        assert_ne!(
            key_to_action(key('n'), View::EnvVars, false),
            Action::AddEnvVar
        );
    }
}
