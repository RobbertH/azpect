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
    DisableMouseCapture, EnableMouseCapture, Event as CtEvent, KeyCode, KeyEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::Terminal;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tui_input::backend::crossterm::EventHandler as _;

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
                // Command palette has top priority. While active, all keys
                // except Esc (cancel) and Enter (execute) flow into the
                // command input buffer.
                if state.command_active {
                    if should_forward_to_command(state, key) {
                        state.command_input.handle_event(&CtEvent::Key(key));
                        continue;
                    }
                    match key.code {
                        KeyCode::Esc => {
                            state.command_active = false;
                            state.command_input.reset();
                            continue;
                        }
                        KeyCode::Enter => {
                            let cmd = state.command_input.value().to_string();
                            run_command(state, &cmd);
                            state.command_active = false;
                            state.command_input.reset();
                            if state.should_quit {
                                break;
                            }
                            continue;
                        }
                        _ => {}
                    }
                }
                // When the list filter input has focus, forward raw keystrokes
                // into the `tui_input::Input` widget. `Esc` and `Enter` still
                // flow through the action dispatcher so they can cancel/apply.
                if should_forward_to_filter(state, key) {
                    state.list_filter.handle_event(&CtEvent::Key(key));
                    // Filter contents may shrink; keep the cursor in range.
                    state.list_cursor = 0;
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
                        spawn_missing_list_metrics(state, auth, tx);
                    }
                    Err(e) => state.status_message = Some(format!("resources: {e}")),
                }
            }
            AppEvent::MetricsLoaded { resource_id, result } => {
                state.metrics.pending.remove(&resource_id);
                state.metrics.loading = false;
                match result {
                    Ok(series) => {
                        state.metrics.by_resource.insert(resource_id, series);
                        state.metrics.last_error = None;
                    }
                    Err(e) => {
                        // Insert empty series so the row renders UNKNOWN rather
                        // than spinning LOADING forever; keep the diagnostic
                        // around in `last_error` for the status bar.
                        state.metrics.by_resource.insert(resource_id, Vec::new());
                        state.metrics.last_error = Some(e);
                    }
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

/// True when the list filter is active *and* the key is something the input
/// widget should consume (i.e. anything except `Esc` / `Enter`, which remain
/// reserved for cancel / apply via [`decide_action`]).
fn should_forward_to_filter(state: &AppState, key: crossterm::event::KeyEvent) -> bool {
    state.list_filter_active
        && !matches!(key.code, KeyCode::Esc | KeyCode::Enter)
}

/// True when the command palette is active *and* the key is something the
/// input widget should consume. `Esc` (cancel) and `Enter` (execute) are
/// reserved and handled directly by the event loop.
fn should_forward_to_command(state: &AppState, key: crossterm::event::KeyEvent) -> bool {
    state.command_active && !matches!(key.code, KeyCode::Esc | KeyCode::Enter)
}

/// Parse and execute a command palette buffer. Unknown commands surface a
/// status message instead of crashing; empty input is silently ignored.
fn run_command(state: &mut AppState, cmd: &str) {
    let trimmed = cmd.trim();
    match trimmed {
        "q" | "quit" | "qa" | "qa!" | "quitall" => state.should_quit = true,
        "" => {} // empty: silently ignore
        other => {
            state.status_message = Some(format!("unknown command: :{other}"));
        }
    }
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
    if apply_navigation_action(action, state) {
        return;
    }
    if let Action::Refresh = action {
        // Refresh the visible view's primary data.
        kick_off_loads_for_view(state, auth, tx, /* force */ true);
        return;
    }
    if let Action::StartCommand = action {
        state.command_active = true;
        state.command_input.reset();
        return;
    }
    // Otherwise: unhandled — view ignored it, nothing to do.
}

/// Pure navigation/quit subset of `global_handle`. Touches only `state`, so it
/// can be unit-tested without constructing `AzureAuth` or an event channel.
/// Returns `true` if the action was handled here.
fn apply_navigation_action(action: Action, state: &mut AppState) -> bool {
    match action {
        Action::Quit => {
            state.should_quit = true;
            true
        }
        Action::Back => {
            match state.view_stack.pop() {
                Some(prev) => state.view = prev,
                None => state.should_quit = true,
            }
            true
        }
        Action::Help => {
            if state.view != View::Help {
                state.view_stack.push(state.view);
                state.view = View::Help;
            }
            true
        }
        Action::SwitchSubscription => {
            if state.view != View::Subscriptions {
                state.view_stack.push(state.view);
                state.view = View::Subscriptions;
            }
            true
        }
        _ => false,
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
                if force {
                    // Drop cached badges so they refresh once the new resource
                    // list arrives; in-flight fetches keep their pending entries.
                    state.metrics.by_resource.clear();
                }
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
    // When the command palette is active, reserve the bottom row for the bar
    // and render the underlying view into the area above it.
    let (view_area, command_area) = if state.command_active && area.height > 0 {
        let view_area = Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: area.height - 1,
        };
        let command_area = Rect {
            x: area.x,
            y: area.y + area.height - 1,
            width: area.width,
            height: 1,
        };
        (view_area, Some(command_area))
    } else {
        (area, None)
    };

    match state.view {
        View::Subscriptions => crate::ui::views::subscriptions::render(f, view_area, state, theme),
        View::List => crate::ui::views::list::render(f, view_area, state, theme),
        View::Detail => crate::ui::views::detail::render(f, view_area, state, theme),
        View::Logs => crate::ui::views::logs::render(f, view_area, state, theme),
        View::Help => crate::ui::views::help::render(f, view_area, state, theme),
    }

    if let Some(ca) = command_area {
        render_command_bar(f, ca, state, theme);
    }
}

/// Render the one-line command palette at `area`, styled like the search
/// input but with a `:` prompt instead of `>`.
fn render_command_bar(
    f: &mut ratatui::Frame,
    area: Rect,
    state: &AppState,
    theme: &Theme,
) {
    use ratatui::style::Style;
    use ratatui::text::{Line, Span};
    use ratatui::widgets::Paragraph;

    let p = Paragraph::new(Line::from(vec![
        Span::styled(":", Style::default().fg(theme.accent)),
        Span::styled(state.command_input.value(), Style::default().fg(theme.fg)),
        Span::styled("█", Style::default().fg(theme.accent)),
    ]));
    f.render_widget(p, area);
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

/// Kick off a metrics fetch for every resource whose health badge is unknown
/// and not already in flight. Used after `ResourcesLoaded` to populate the
/// list view's per-row badges without waiting for the user to enter Detail.
fn spawn_missing_list_metrics(
    state: &mut AppState,
    auth: &AzureAuth,
    tx: &UnboundedSender<AppEvent>,
) {
    let range = state.metrics.range;
    let to_fetch: Vec<Resource> = state
        .resources
        .iter()
        .filter(|r| {
            !state.metrics.by_resource.contains_key(&r.id)
                && !state.metrics.pending.contains(&r.id)
        })
        .cloned()
        .collect();
    for resource in to_fetch {
        state.metrics.pending.insert(resource.id.clone());
        spawn_load_metrics(auth.clone(), resource, range, tx.clone());
    }
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
    fn filter_forwarding_gating() {
        let mut state = fresh_state();

        // Filter inactive: nothing should be forwarded, even printable keys.
        assert!(!should_forward_to_filter(&state, k('a')));
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert!(!should_forward_to_filter(&state, esc));
        assert!(!should_forward_to_filter(&state, enter));

        // Filter active: printable keys forward; Esc/Enter still reach the dispatcher.
        state.list_filter_active = true;
        assert!(should_forward_to_filter(&state, k('a')));
        assert!(should_forward_to_filter(&state, k('/')));
        let backspace = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        assert!(should_forward_to_filter(&state, backspace));
        assert!(!should_forward_to_filter(&state, esc));
        assert!(!should_forward_to_filter(&state, enter));
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

    #[test]
    fn command_mode_q_quits() {
        let mut state = fresh_state();
        state.command_active = true;
        // Simulate the user having typed "q" into the buffer.
        state.command_input = state.command_input.clone().with_value("q".to_string());
        let buf = state.command_input.value().to_string();
        run_command(&mut state, &buf);
        assert!(state.should_quit);
        assert!(state.status_message.is_none());
    }

    #[test]
    fn command_mode_unknown_sets_status() {
        let mut state = fresh_state();
        state.command_active = true;
        run_command(&mut state, "foo");
        assert!(!state.should_quit);
        assert_eq!(
            state.status_message.as_deref(),
            Some("unknown command: :foo")
        );
    }

    #[test]
    fn command_mode_quit_aliases_all_quit() {
        for cmd in ["q", "quit", "qa", "qa!", "quitall"] {
            let mut state = fresh_state();
            run_command(&mut state, cmd);
            assert!(state.should_quit, "{cmd} should set should_quit");
            assert!(state.status_message.is_none(), "{cmd} should not set status");
        }
    }

    #[test]
    fn command_mode_empty_is_silent() {
        let mut state = fresh_state();
        run_command(&mut state, "");
        run_command(&mut state, "   ");
        assert!(!state.should_quit);
        assert!(state.status_message.is_none());
    }

    #[test]
    fn command_mode_forwarding_gating() {
        let mut state = fresh_state();

        // Command palette inactive: nothing forwards, even printable keys.
        assert!(!should_forward_to_command(&state, k('a')));
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert!(!should_forward_to_command(&state, esc));
        assert!(!should_forward_to_command(&state, enter));

        // Command palette active: printable keys forward; Esc/Enter still
        // reach the event loop directly so they can cancel/execute.
        state.command_active = true;
        assert!(should_forward_to_command(&state, k('a')));
        assert!(should_forward_to_command(&state, k('q')));
        assert!(should_forward_to_command(&state, k(':')));
        let backspace = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        assert!(should_forward_to_command(&state, backspace));
        assert!(!should_forward_to_command(&state, esc));
        assert!(!should_forward_to_command(&state, enter));
    }

    #[test]
    fn back_navigation_unwinds_full_stack_without_bouncing() {
        // Regression: previous_view was a single-slot stack of depth 1, so
        // Subs -> List -> Detail -> Esc landed in List with previous_view set
        // to Detail; the next Esc warped back into Detail. With view_stack
        // (Vec<View>) the breadcrumb chain is preserved and Back unwinds it.
        use crate::azure::resources::{Resource, ResourceKind};
        use crate::azure::subscriptions::Subscription;

        let mut state = fresh_state();
        // Seed minimal data so view-local handlers actually transition.
        state.subscriptions = vec![Subscription {
            id: "sub-1".into(),
            display_name: "alpha".into(),
            state: "Enabled".into(),
            tenant_id: "t".into(),
        }];
        let resource = Resource {
            id: "/r/one".into(),
            name: "alpha-func".into(),
            kind: ResourceKind::FunctionApp,
            location: "westeurope".into(),
            resource_group: "rg".into(),
            subscription_id: "sub-1".into(),
            state: Some("Running".into()),
        };
        state.view = View::Subscriptions;

        // Forward: Subs -> List
        assert!(crate::ui::views::subscriptions::handle(
            Action::OpenSelected,
            &mut state,
        ));
        assert_eq!(state.view, View::List);
        assert_eq!(state.view_stack, vec![View::Subscriptions]);

        // The subs handler clears resources to force a fresh load — re-seed for
        // the next forward step.
        state.resources = vec![resource];

        // Forward: List -> Detail
        assert!(crate::ui::views::list::handle(Action::OpenSelected, &mut state));
        assert_eq!(state.view, View::Detail);
        assert_eq!(state.view_stack, vec![View::Subscriptions, View::List]);

        // Back from Detail: view-local handler must NOT consume it; global pops List.
        assert!(!crate::ui::views::detail::handle(Action::Back, &mut state));
        assert!(apply_navigation_action(Action::Back, &mut state));
        assert_eq!(state.view, View::List, "first Esc lands in List");
        assert_eq!(state.view_stack, vec![View::Subscriptions]);
        assert!(!state.should_quit);

        // Back from List: pops Subscriptions — must NOT bounce back into Detail.
        assert!(!crate::ui::views::list::handle(Action::Back, &mut state));
        assert!(apply_navigation_action(Action::Back, &mut state));
        assert_eq!(
            state.view,
            View::Subscriptions,
            "second Esc lands in Subscriptions, never Detail"
        );
        assert!(state.view_stack.is_empty());
        assert!(!state.should_quit);

        // Back from Subscriptions with empty stack: quits cleanly.
        assert!(apply_navigation_action(Action::Back, &mut state));
        assert!(state.should_quit);
    }
}
