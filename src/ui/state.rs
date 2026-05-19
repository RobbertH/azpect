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

#[derive(Clone, Default)]
pub struct LogsCache {
    /// keyed by resource id
    pub by_resource: HashMap<String, Vec<LogLine>>,
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
    /// actions; Esc cancels (deactivates input, keeps query), Enter commits.
    pub search_active: bool,
    /// Case-insensitive substring filter applied to the source and message
    /// columns. Persists across `search_active` toggles so the user can edit,
    /// confirm with Enter, navigate, and re-open with `/`.
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
            status_message: None,
            status_message_until: None,
            should_quit: false,
            quit_confirm: false,
            quit_confirm_yes: false,
            command_active: false,
            command_input: Input::default(),
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
