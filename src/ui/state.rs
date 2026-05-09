//! Shared application state. Both the event loop (Lane 3) and the view
//! renderers (Lane 4) read this struct, so it's part of the contract.

#![allow(dead_code, unused_variables)]

use std::collections::{HashMap, HashSet};

use tui_input::Input;

use crate::azure::logs::LogLine;
use crate::azure::metrics::{MetricSeries, TimeRange};
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
    Help,
}

/// Per-resource cached metrics. The detail view reads these; the loader writes
/// them when a `MetricsReady` event arrives.
#[derive(Clone, Default)]
pub struct MetricsCache {
    pub by_resource: HashMap<String, Vec<MetricSeries>>,
    /// Per-resource failure messages. Mutually exclusive with `by_resource`:
    /// a successful fetch removes the resource from `failures`, and vice versa.
    pub failures: HashMap<String, String>,
    /// Resource IDs whose metrics fetch is currently in flight.
    pub pending: HashSet<String>,
    pub range: TimeRange,
    pub loading: bool,
    pub last_error: Option<String>,
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
    pub logs: LogsCache,

    pub status_message: Option<String>,
    pub should_quit: bool,

    /// Modal flag: when true, a "Are you sure you want to quit?" overlay is
    /// rendered on top of the current view and the event loop short-circuits
    /// keyboard input to y/n handling. Set via `Action::Back` on an empty view
    /// stack; cleared by answering 'n' (or any cancel key). Note this is *not*
    /// a `View` variant — the underlying view keeps rendering behind it.
    pub quit_confirm: bool,

    /// Whether the vim/k9s-style command palette (`:`) is currently capturing
    /// input. While true, raw keystrokes are forwarded into `command_input`
    /// rather than dispatched as actions; Esc cancels, Enter executes.
    pub command_active: bool,
    pub command_input: Input,
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
            metrics: MetricsCache { range, ..Default::default() },
            logs: LogsCache { range, ..Default::default() },
            status_message: None,
            should_quit: false,
            quit_confirm: false,
            command_active: false,
            command_input: Input::default(),
            config,
        }
    }

    /// Resource currently under the cursor in the list view, after applying filter.
    pub fn selected_resource(&self) -> Option<&Resource> {
        // Lane 3/4 will likely want a filtered iterator helper; this naive impl is a placeholder.
        self.filtered_resources().get(self.list_cursor).copied()
    }

    /// Apply `list_filter` + `favorites_only` to `resources`.
    pub fn filtered_resources(&self) -> Vec<&Resource> {
        let needle = self.list_filter.value().to_lowercase();
        self.resources
            .iter()
            .filter(|r| !self.favorites_only || self.config.is_favorite(&r.id))
            .filter(|r| needle.is_empty() || r.name.to_lowercase().contains(&needle))
            .collect()
    }
}
