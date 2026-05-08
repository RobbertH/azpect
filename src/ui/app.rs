//! App entry point: owns the terminal, runs the event loop, dispatches to views.
//!
//! Lane 3 lives here. Responsibilities:
//!
//! 1. Set up crossterm raw mode + alternate screen, panic-safely (via a
//!    [`TerminalGuard`] whose `Drop` impl always restores the terminal).
//! 2. Build a `tokio::sync::mpsc::UnboundedChannel<AppEvent>` and spawn:
//!    - a blocking key/event reader thread,
//!    - a 250ms tick task,
//!    - the initial subscriptions loader,
//!    - and on demand: resources / metrics / logs loaders.
//! 3. Run the main loop until `state.should_quit`, drawing the active view
//!    and applying input through the chord-aware key handler.

#![allow(dead_code, unused_variables)]

use std::io::{stdout, Stdout};
use std::time::{Duration, Instant};

use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event as CtEvent, KeyEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::Terminal;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::azure::auth::AzureAuth;
use crate::azure::metrics::TimeRange;
use crate::azure::resources::Resource;
use crate::config::Config;
use crate::ui::events::{
    is_chord_starter, key_to_action, resolve_chord, Action, AppEvent,
};
use crate::ui::state::{AppState, View};
use crate::ui::theme::Theme;

/// How often the tick task fires. Drives spinner refresh, chord timeout, etc.
const TICK_INTERVAL: Duration = Duration::from_millis(250);

/// `g` chord must complete within this window or it's discarded.
const CHORD_TIMEOUT: Duration = Duration::from_millis(1000);

/// Restores the terminal on drop. Owned by `run`; if a panic blows past it
/// (or the program exits early via `?`), the user gets their terminal back.
struct TerminalGuard {
    /// Set to `false` after a clean shutdown so we don't double-restore.
    active: bool,
}

impl TerminalGuard {
    fn enter() -> std::io::Result<Self> {
        enable_raw_mode()?;
        execute!(stdout(), EnterAlternateScreen, EnableMouseCapture)?;
        Ok(Self { active: true })
    }

    fn leave(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        // Best-effort restore. Log but don't propagate — we're probably already
        // unwinding and the terminal will be in a bad state regardless.
        let _ = execute!(stdout(), DisableMouseCapture, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.leave();
    }
}

/// Per-loop input bookkeeping (chord state machine).
#[derive(Default)]
struct InputState {
    /// First key of a pending chord (currently only `'g'`) and the time it
    /// arrived. Cleared on resolve, on timeout, or on any non-completing key.
    pending_chord: Option<(char, Instant)>,
}

impl InputState {
    fn maybe_expire(&mut self, now: Instant) {
        if let Some((_, t)) = self.pending_chord {
            if now.duration_since(t) > CHORD_TIMEOUT {
                self.pending_chord = None;
            }
        }
    }
}

/// Run the TUI to completion. Returns when the user quits.
pub async fn run(auth: AzureAuth, cfg: Config) -> anyhow::Result<()> {
    let theme = Theme::by_name(&cfg.theme);
    let mut state = AppState::new(cfg);

    // Set up terminal *before* we spawn anything that might print to stderr —
    // tracing is configured to write to stderr in main.rs, which is fine in alt
    // screen but the user only sees the TUI surface anyway.
    let mut guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;

    let (tx, mut rx) = mpsc::unbounded_channel::<AppEvent>();

    // Spawn the blocking key reader on its own thread (crossterm::event::read
    // is sync). Lives until the channel is dropped.
    spawn_input_reader(tx.clone());

    // Spawn periodic tick.
    spawn_ticker(tx.clone());

    // Kick off subscriptions load.
    spawn_load_subscriptions(auth.clone(), tx.clone());

    let result = event_loop(&mut terminal, &mut state, &theme, &auth, &tx, &mut rx).await;

    // Restore terminal *before* trying to write the config or print errors.
    guard.leave();
    drop(terminal);

    // Persist config; non-fatal.
    if let Err(e) = crate::config::save(&state.config) {
        tracing::warn!("failed to save config: {e:#}");
    }

    result
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state: &mut AppState,
    theme: &Theme,
    auth: &AzureAuth,
    tx: &UnboundedSender<AppEvent>,
    rx: &mut UnboundedReceiver<AppEvent>,
) -> anyhow::Result<()> {
    let mut input = InputState::default();

    loop {
        terminal.draw(|f| dispatch_view(f, f.area(), state, theme))?;

        // `recv()` yields `None` only when all senders have dropped, which
        // shouldn't happen while the input thread is alive. Treat it as an
        // orderly shutdown anyway.
        let Some(event) = rx.recv().await else { break };

        match event {
            AppEvent::Tick => {
                input.maybe_expire(Instant::now());
            }
            AppEvent::Resize { .. } => {
                // ratatui re-measures on next draw. Nothing to do.
            }
            AppEvent::Key(key) => {
                // Only react to *press* events. Some terminals (kitty, win-pty)
                // emit Release/Repeat which would otherwise double-fire.
                if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
                    continue;
                }
                let action = decide_action(&mut input, key, state);
                if action != Action::Noop {
                    apply_action(action, state, auth, tx);
                }
            }
            AppEvent::SubscriptionsLoaded(res) => {
                state.loading_subscriptions = false;
                match res {
                    Ok(subs) => {
                        state.subscriptions = subs;
                        // Restore last-used subscription cursor if possible.
                        if let Some(last) = state.selected_subscription.clone() {
                            if let Some(idx) =
                                state.subscriptions.iter().position(|s| s.id == last)
                            {
                                state.subscription_cursor = idx;
                            }
                        }
                    }
                    Err(e) => {
                        state.status_message = Some(format!("subscriptions: {e}"));
                    }
                }
            }
            AppEvent::ResourcesLoaded(res) => {
                state.loading_resources = false;
                match res {
                    Ok(rs) => {
                        state.resources = rs;
                        if state.list_cursor >= state.resources.len() {
                            state.list_cursor = state.resources.len().saturating_sub(1);
                        }
                    }
                    Err(e) => state.status_message = Some(format!("resources: {e}")),
                }
            }
            AppEvent::MetricsLoaded { resource_id, result } => {
                state.metrics.loading = false;
                match result {
                    Ok(series) => {
                        state.metrics.by_resource.insert(resource_id, series);
                        state.metrics.last_error = None;
                    }
                    Err(e) => state.metrics.last_error = Some(e),
                }
            }
            AppEvent::LogsLoaded { resource_id, result } => {
                state.logs.loading = false;
                match result {
                    Ok(lines) => {
                        state.logs.by_resource.insert(resource_id, lines);
                        state.logs.last_error = None;
                    }
                    Err(e) => state.logs.last_error = Some(e),
                }
            }
        }

        if state.should_quit {
            break;
        }
    }

    Ok(())
}

/// Run the key event through the chord state machine and the per-view +
/// global keymap, returning the resolved action (or [`Action::Noop`] if no
/// action should fire this frame).
fn decide_action(
    input: &mut InputState,
    key: crossterm::event::KeyEvent,
    state: &AppState,
) -> Action {
    let now = Instant::now();
    input.maybe_expire(now);

    // Did we already see the first half of a chord?
    if let Some((starter, _)) = input.pending_chord {
        input.pending_chord = None;
        if let Some(action) = resolve_chord(starter, key) {
            return action;
        }
        // Fall through: process this key as a fresh input.
    }

    // First-key-of-chord? Stash and wait.
    if is_chord_starter(key, state.list_filter_active) {
        input.pending_chord = Some(('g', now));
        return Action::Noop;
    }

    key_to_action(key, state.view, state.list_filter_active)
}

/// Apply an action: first to the active view's local handler, then — if the
/// view didn't consume it — to the global handler.
fn apply_action(
    action: Action,
    state: &mut AppState,
    auth: &AzureAuth,
    tx: &UnboundedSender<AppEvent>,
) {
    let consumed = view_handle(action, state);
    if !consumed {
        global_handle(action, state, auth, tx);
    }

    // Side-effects: certain transitions require kicking off background loads.
    after_action(action, state, auth, tx);
}

/// Dispatch the view-local handler. Wraps each view in `catch_unwind` is *not*
/// done here — view modules are `todo!()` until Lane 4 fills them in, so this
/// will panic at runtime, but compile-time is fine and that's all Lane 3 owns.
fn view_handle(action: Action, state: &mut AppState) -> bool {
    match state.view {
        View::Subscriptions => crate::ui::views::subscriptions::handle(action, state),
        View::List => crate::ui::views::list::handle(action, state),
        View::Detail => crate::ui::views::detail::handle(action, state),
        View::Logs => crate::ui::views::logs::handle(action, state),
        View::Help => crate::ui::views::help::handle(action, state),
    }
}

/// Globals applied when the view ignores the action.
fn global_handle(
    action: Action,
    state: &mut AppState,
    auth: &AzureAuth,
    tx: &UnboundedSender<AppEvent>,
) {
    match action {
        Action::Quit => state.should_quit = true,
        Action::Back => match state.previous_view {
            Some(prev) => {
                state.view = prev;
                state.previous_view = None;
            }
            None => state.should_quit = true,
        },
        Action::Help => {
            if state.view != View::Help {
                state.previous_view = Some(state.view);
                state.view = View::Help;
            }
        }
        Action::SwitchSubscription => {
            if state.view != View::Subscriptions {
                state.previous_view = Some(state.view);
                state.view = View::Subscriptions;
            }
        }
        Action::Refresh => {
            // Refresh the visible view's primary data.
            kick_off_loads_for_view(state, auth, tx, /* force */ true);
        }
        _ => { /* unhandled — view ignored it, nothing to do */ }
    }
}

/// Side-effects triggered by an action *after* the view+global handlers ran.
/// E.g., entering `Detail` should fetch metrics.
fn after_action(
    action: Action,
    state: &mut AppState,
    auth: &AzureAuth,
    tx: &UnboundedSender<AppEvent>,
) {
    match action {
        // The view handler likely transitioned `state.view`. Kick off loads
        // appropriate to whatever the new view is.
        Action::OpenSelected
        | Action::OpenLogs
        | Action::SetWindowDay
        | Action::SetWindowWeek
        | Action::ToggleErrorsOnly => {
            kick_off_loads_for_view(state, auth, tx, /* force */ false);
        }
        _ => {}
    }
}

/// Look at `state.view` and the loading flags, and spawn whichever loaders are
/// missing. `force` overrides the loading-flag debounce (used for the explicit
/// Refresh action).
fn kick_off_loads_for_view(
    state: &mut AppState,
    auth: &AzureAuth,
    tx: &UnboundedSender<AppEvent>,
    force: bool,
) {
    match state.view {
        View::Subscriptions => {
            if force || (!state.loading_subscriptions && state.subscriptions.is_empty()) {
                state.loading_subscriptions = true;
                spawn_load_subscriptions(auth.clone(), tx.clone());
            }
        }
        View::List => {
            if force || (!state.loading_resources && state.resources.is_empty()) {
                let sub_ids = match &state.selected_subscription {
                    Some(id) => vec![id.clone()],
                    None => state.subscriptions.iter().map(|s| s.id.clone()).collect(),
                };
                state.loading_resources = true;
                spawn_load_resources(auth.clone(), sub_ids, tx.clone());
            }
        }
        View::Detail => {
            if let Some(resource) = state.selected_resource().cloned() {
                if force || !state.metrics.loading {
                    state.metrics.loading = true;
                    spawn_load_metrics(
                        auth.clone(),
                        resource,
                        state.metrics.range,
                        tx.clone(),
                    );
                }
            }
        }
        View::Logs => {
            if let Some(resource) = state.selected_resource().cloned() {
                if force || !state.logs.loading {
                    state.logs.loading = true;
                    spawn_load_logs(
                        auth.clone(),
                        resource,
                        state.logs.range,
                        state.logs.errors_only,
                        tx.clone(),
                    );
                }
            }
        }
        View::Help => {}
    }
}

fn dispatch_view(
    f: &mut ratatui::Frame,
    area: Rect,
    state: &AppState,
    theme: &Theme,
) {
    match state.view {
        View::Subscriptions => crate::ui::views::subscriptions::render(f, area, state, theme),
        View::List => crate::ui::views::list::render(f, area, state, theme),
        View::Detail => crate::ui::views::detail::render(f, area, state, theme),
        View::Logs => crate::ui::views::logs::render(f, area, state, theme),
        View::Help => crate::ui::views::help::render(f, area, state, theme),
    }
}

// ---------------------------------------------------------------------------
// Background task spawn helpers
// ---------------------------------------------------------------------------

fn spawn_input_reader(tx: UnboundedSender<AppEvent>) {
    std::thread::spawn(move || {
        loop {
            // `read()` blocks until an event is available.
            match crossterm::event::read() {
                Ok(CtEvent::Key(k)) => {
                    if tx.send(AppEvent::Key(k)).is_err() {
                        break;
                    }
                }
                Ok(CtEvent::Resize(w, h)) => {
                    if tx.send(AppEvent::Resize { width: w, height: h }).is_err() {
                        break;
                    }
                }
                Ok(_) => { /* ignore mouse/focus/paste/etc. for now */ }
                Err(e) => {
                    tracing::warn!("crossterm::event::read failed: {e}");
                    break;
                }
            }
        }
    });
}

fn spawn_ticker(tx: UnboundedSender<AppEvent>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(TICK_INTERVAL);
        // First tick fires immediately by default; skip it so we don't spam.
        interval.tick().await;
        loop {
            interval.tick().await;
            if tx.send(AppEvent::Tick).is_err() {
                break;
            }
        }
    });
}

fn spawn_load_subscriptions(auth: AzureAuth, tx: UnboundedSender<AppEvent>) {
    tokio::spawn(async move {
        let result = crate::azure::subscriptions::list(&auth)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::SubscriptionsLoaded(result));
    });
}

fn spawn_load_resources(
    auth: AzureAuth,
    sub_ids: Vec<String>,
    tx: UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        let result = crate::azure::resources::list(&auth, &sub_ids)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::ResourcesLoaded(result));
    });
}

fn spawn_load_metrics(
    auth: AzureAuth,
    resource: Resource,
    range: TimeRange,
    tx: UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        let resource_id = resource.id.clone();
        let result = crate::azure::metrics::fetch(&auth, &resource, range)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::MetricsLoaded { resource_id, result });
    });
}

fn spawn_load_logs(
    auth: AzureAuth,
    resource: Resource,
    range: TimeRange,
    errors_only: bool,
    tx: UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        let resource_id = resource.id.clone();
        let result = crate::azure::logs::fetch(&auth, &resource, range, errors_only)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::LogsLoaded { resource_id, result });
    });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn fresh_state() -> AppState {
        AppState::new(Config::default())
    }

    fn k(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn first_g_is_pending_second_g_resolves_to_top() {
        let mut input = InputState::default();
        let state = fresh_state();
        let a1 = decide_action(&mut input, k('g'), &state);
        assert_eq!(a1, Action::Noop, "first g should be a no-op");
        assert!(input.pending_chord.is_some());

        let a2 = decide_action(&mut input, k('g'), &state);
        assert_eq!(a2, Action::GotoTop);
        assert!(input.pending_chord.is_none());
    }

    #[test]
    fn g_then_other_key_clears_chord_and_processes_normally() {
        let mut input = InputState::default();
        let state = fresh_state();
        let _ = decide_action(&mut input, k('g'), &state);
        assert!(input.pending_chord.is_some());

        let a = decide_action(&mut input, k('j'), &state);
        // `j` after a stale `g` should be MoveDown, not GotoTop.
        assert_eq!(a, Action::MoveDown);
        assert!(input.pending_chord.is_none());
    }

    #[test]
    fn chord_expires_after_timeout() {
        // Manually inject a stale pending chord.
        let mut input = InputState {
            pending_chord: Some((
                'g',
                Instant::now() - CHORD_TIMEOUT - Duration::from_millis(50),
            )),
        };
        input.maybe_expire(Instant::now());
        assert!(input.pending_chord.is_none());
    }
}
