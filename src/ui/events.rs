//! Event types flowing through the single `mpsc::UnboundedReceiver<AppEvent>`
//! that drives the UI loop.
//!
//! ## Vim-flavored input model
//!
//! Cursor movement uses `h j k l` and chords like `g g`. Single-letter actions
//! (`L` for logs, `f` favorite, `s` subscription, `r` refresh, `d/w` window,
//! `e` errors-only, `q` quit) are **uppercase or distinct from hjkl** so they
//! never clobber navigation. Lane 3 is responsible for the chord state machine
//! (e.g. tracking the first `g` of `g g`).

#![allow(dead_code, unused_variables)]

use crossterm::event::KeyEvent;

use crate::azure::logs::LogLine;
use crate::azure::metrics::MetricSeries;
use crate::azure::resources::Resource;
use crate::azure::subscriptions::Subscription;

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
    /// Background load completion: metrics for a specific resource id.
    MetricsLoaded {
        resource_id: String,
        result: Result<Vec<MetricSeries>, String>,
    },
    /// Background load completion: logs for a specific resource id.
    LogsLoaded {
        resource_id: String,
        result: Result<Vec<LogLine>, String>,
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
    SwitchSubscription,
    Refresh,
    SetWindowDay,
    SetWindowWeek,
    Help,
    Noop,
}
