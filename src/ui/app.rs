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

use crossterm::event::{Event as CtEvent, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::Terminal;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tui_input::backend::crossterm::EventHandler as _;
use tui_input::Input;

use crate::azure::auth::AzureAuth;
use crate::azure::az_login::{self, AzLoginOptions};
use crate::azure::metrics::TimeRange;
use crate::azure::resources::Resource;
use crate::config::Config;
use crate::ui::events::{is_chord_starter, key_to_action, resolve_chord, Action, AppEvent};
use crate::ui::state::{AppState, AuthMenuFocus, AuthPrompt, PendingLogin, View};
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
        // Don't capture mouse events: we don't react to them anywhere, and
        // capturing them breaks native click-and-drag text selection in the
        // terminal emulator (so users can't copy error messages).
        execute!(stdout(), EnterAlternateScreen)?;
        Ok(Self { active: true })
    }

    fn leave(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        // Best-effort restore. Log but don't propagate — we're probably already
        // unwinding and the terminal will be in a bad state regardless.
        let _ = execute!(stdout(), LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }

    /// Temporarily hand the terminal back to the parent shell so a child
    /// process (e.g. `az login`) can interact with the user. Pair with
    /// [`Self::resume`]. Idempotent if already suspended.
    fn suspend(&mut self) {
        self.leave();
    }

    /// Re-enter the alternate screen + raw mode after a [`Self::suspend`].
    /// Errors here propagate so the caller can decide whether to abort —
    /// continuing without raw mode would leave the TUI in a broken state.
    fn resume(&mut self) -> std::io::Result<()> {
        if self.active {
            return Ok(());
        }
        enable_raw_mode()?;
        execute!(stdout(), EnterAlternateScreen)?;
        self.active = true;
        Ok(())
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

    let result = event_loop(
        &mut terminal,
        &mut guard,
        &mut state,
        &theme,
        &auth,
        &tx,
        &mut rx,
    )
    .await;

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
    guard: &mut TerminalGuard,
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
                // Auth-prompt modal takes priority over quit-confirm so its
                // Esc dismisses the prompt instead of opening the quit dialog.
                if state.auth_prompt != AuthPrompt::Hidden {
                    handle_auth_prompt_key(state, key);
                    continue;
                }
                // Quit-confirmation modal takes the highest priority. While
                // it's up, every key is either yes/no for the dialog or
                // swallowed — nothing else (command palette, filter,
                // dispatcher) runs.
                if state.quit_confirm {
                    match key.code {
                        // Direct yes/no shortcuts always commit, regardless of
                        // which button is focused.
                        KeyCode::Char('y') | KeyCode::Char('Y') => {
                            state.should_quit = true;
                            break;
                        }
                        KeyCode::Char('n')
                        | KeyCode::Char('N')
                        | KeyCode::Esc
                        | KeyCode::Char('q') => {
                            state.quit_confirm = false;
                            continue;
                        }
                        // Enter commits whichever button is currently focused.
                        KeyCode::Enter => {
                            if state.quit_confirm_yes {
                                state.should_quit = true;
                                break;
                            } else {
                                state.quit_confirm = false;
                                continue;
                            }
                        }
                        // Toggle focus between Yes and No.
                        KeyCode::Left
                        | KeyCode::Char('h')
                        | KeyCode::Right
                        | KeyCode::Char('l')
                        | KeyCode::Tab
                        | KeyCode::BackTab => {
                            state.quit_confirm_yes = !state.quit_confirm_yes;
                            continue;
                        }
                        _ => continue,
                    }
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
                        let was_empty = subs.is_empty();
                        state.subscriptions = subs;
                        // Restore last-used subscription cursor if possible.
                        if let Some(last) = state.selected_subscription.clone() {
                            if let Some(idx) = state.subscriptions.iter().position(|s| s.id == last)
                            {
                                state.subscription_cursor = idx;
                            }
                        }
                        // No subs visible to this credential is almost always
                        // an auth/tenant problem — surface the login modal.
                        if was_empty
                            && state.view == View::Subscriptions
                            && state.auth_prompt == AuthPrompt::Hidden
                        {
                            open_auth_prompt(state, None);
                        }
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        state.status_message = Some(format!("subscriptions: {msg}"));
                        // Same treatment for outright failures: the chain may
                        // simply have no usable credential.
                        if state.view == View::Subscriptions
                            && state.auth_prompt == AuthPrompt::Hidden
                        {
                            open_auth_prompt(state, Some(msg));
                        }
                    }
                }
            }
            AppEvent::ResourcesLoaded(res) => {
                state.loading_resources = false;
                match res {
                    Ok(rs) => {
                        state.resources = rs;
                        // Restore the cursor to the last-selected resource if
                        // it's still in the loaded set; otherwise clamp.
                        if let Some(last) = state.config.last_resource_id.as_deref() {
                            if let Some(idx) = state.resources.iter().position(|r| r.id == last) {
                                state.list_cursor = idx;
                            }
                        }
                        if state.list_cursor >= state.resources.len() {
                            state.list_cursor = state.resources.len().saturating_sub(1);
                        }
                        spawn_missing_list_metrics(state, auth, tx);
                        spawn_missing_list_health(state, auth, tx);
                    }
                    Err(e) => state.status_message = Some(format!("resources: {e}")),
                }
            }
            AppEvent::MetricsLoaded {
                resource_id,
                result,
            } => {
                state.metrics.pending.remove(&resource_id);
                state.metrics.loading = false;
                match result {
                    Ok(r) => {
                        state.metrics.failures.remove(&resource_id);
                        if r.missing.is_empty() {
                            state.metrics.missing.remove(&resource_id);
                        } else {
                            state.metrics.missing.insert(resource_id.clone(), r.missing);
                        }
                        state.metrics.by_resource.insert(resource_id, r.series);
                        state.metrics.last_error = None;
                    }
                    Err(e) => {
                        state.metrics.by_resource.remove(&resource_id);
                        state.metrics.missing.remove(&resource_id);
                        state.metrics.failures.insert(resource_id, e.clone());
                        state.metrics.last_error = Some(e);
                    }
                }
            }
            AppEvent::LogsLoaded {
                resource_id,
                result,
            } => {
                state.logs.loading = false;
                match result {
                    Ok(lines) => {
                        state.logs.by_resource.insert(resource_id, lines);
                        state.logs.last_error = None;
                    }
                    Err(e) => state.logs.last_error = Some(e),
                }
            }
            AppEvent::HealthLoaded {
                resource_id,
                result,
            } => {
                state.health.pending.remove(&resource_id);
                match result {
                    Ok(avail) => {
                        state.health.failures.remove(&resource_id);
                        state.health.by_resource.insert(resource_id, avail);
                    }
                    Err(e) => {
                        state.health.failures.insert(resource_id, e);
                    }
                }
            }
        }

        // Drain a pending login request: the modal handler set it on Enter,
        // and now we own the terminal so we can suspend safely.
        if let Some(req) = state.pending_login.take() {
            run_pending_login(terminal, guard, state, auth, tx, req).await;
        }

        if state.should_quit {
            break;
        }
    }

    Ok(())
}

/// Suspend the TUI, run `az login`, restore the TUI, clear the auth cache,
/// and trigger a fresh subscriptions load. All errors are captured into
/// `state.auth_last_error` and surfaced via the modal on the next frame.
async fn run_pending_login(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    guard: &mut TerminalGuard,
    state: &mut AppState,
    auth: &AzureAuth,
    tx: &UnboundedSender<AppEvent>,
    req: PendingLogin,
) {
    guard.suspend();

    // Surface what we're about to do on the parent shell so the user sees
    // context before az takes over the terminal.
    {
        use std::io::Write as _;
        let mut out = stdout();
        let mut hint = String::from("\nazpect: launching `az login`");
        if let Some(t) = req.tenant.as_deref() {
            hint.push_str(&format!(" --tenant {t}"));
        }
        if req.use_device_code {
            hint.push_str(" --use-device-code");
        }
        hint.push_str("\n\n");
        let _ = out.write_all(hint.as_bytes());
        let _ = out.flush();
    }

    let opts = AzLoginOptions {
        tenant: req.tenant,
        use_device_code: req.use_device_code,
    };
    let outcome = az_login::run(opts).await;

    // Always try to restore the TUI — even on login failure the user is
    // sitting in a bare shell and expects the app back.
    if let Err(e) = guard.resume() {
        // If we can't re-enter the alt screen, bail. Bubbling this up via a
        // status message would never be visible.
        tracing::error!("failed to resume terminal after az login: {e}");
        state.should_quit = true;
        return;
    }
    let _ = terminal.clear();

    match outcome {
        Ok(()) => {
            state.auth_prompt = AuthPrompt::Hidden;
            state.auth_last_error = None;
            // The previous user's bearer is now stale; drop it before we
            // refetch subscriptions or any other ARM call would still go out
            // under the old identity.
            auth.clear_cache().await;
            state.loading_subscriptions = true;
            state.subscriptions.clear();
            spawn_load_subscriptions(auth.clone(), tx.clone());
            state.status_message = Some("logged in via az".to_string());
        }
        Err(e) => {
            // Stay on the menu so the user can retry / pick a different mode.
            state.auth_prompt = AuthPrompt::Menu;
            state.auth_last_error = Some(format!("{e}"));
        }
    }
}

/// True when the list filter is active *and* the key is something the input
/// widget should consume. `Esc` / `Enter` are reserved for cancel / apply, and
/// arrow / page-navigation keys must continue to steer the underlying list
/// rather than moving the text cursor inside the search box.
fn should_forward_to_filter(state: &AppState, key: crossterm::event::KeyEvent) -> bool {
    state.list_filter_active
        && !matches!(
            key.code,
            KeyCode::Esc
                | KeyCode::Enter
                | KeyCode::Up
                | KeyCode::Down
                | KeyCode::PageUp
                | KeyCode::PageDown
        )
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
        // `:q` and friends quit immediately and intentionally bypass the
        // quit-confirmation modal: typing `:q` is explicit user intent, not
        // an accidental Esc press.
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
        View::LogDetail => crate::ui::views::logs_detail::handle(action, state),
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
    if let Action::Yank = action {
        do_yank(state);
    }
    if let Action::OpenInBrowser = action {
        do_open_in_browser(state);
    }
    // Otherwise: unhandled — view ignored it, nothing to do.
}

/// Compute the contextual yank target for the current view, copy it to the
/// system clipboard via OSC52, and post a status hint. No-op (with a status
/// hint) if there's nothing meaningful to copy.
fn do_yank(state: &mut AppState) {
    let target = yank_target(state);
    let Some(text) = target else {
        state.status_message = Some("nothing to copy".to_string());
        return;
    };
    match crate::ui::clipboard::copy(&text) {
        Ok(n) => {
            state.status_message = Some(format!("copied {n} bytes to clipboard"));
        }
        Err(e) => {
            state.status_message = Some(format!("clipboard write failed: {e}"));
        }
    }
}

/// Compute the contextual portal URL for the current view and hand it to the
/// system default browser. Posts a status hint on success or failure.
fn do_open_in_browser(state: &mut AppState) {
    let Some(url) = portal_url_for(state) else {
        state.status_message = Some("nothing to open".to_string());
        return;
    };
    match open::that_detached(&url) {
        Ok(()) => state.status_message = Some(format!("opened {url}")),
        Err(e) => state.status_message = Some(format!("failed to open browser: {e}")),
    }
}

/// Resolve what `o` should open. List/Detail/Logs open the selected resource's
/// portal blade; Subscriptions opens the highlighted subscription's overview.
fn portal_url_for(state: &AppState) -> Option<String> {
    const PORTAL_BASE: &str = "https://portal.azure.com/#@/resource";
    match state.view {
        View::List | View::Detail | View::Logs | View::LogDetail => state
            .selected_resource()
            .map(|r| format!("{PORTAL_BASE}{}", r.id)),
        View::Subscriptions => state
            .subscriptions
            .get(state.subscription_cursor)
            .map(|s| format!("{PORTAL_BASE}/subscriptions/{}/overview", s.id)),
        View::Help => None,
    }
}

/// Resolve what `y` should copy from the currently-visible view. The logs
/// view prefers the displayed error (when the table is empty / the error
/// banner is showing) and otherwise the highlighted log line.
fn yank_target(state: &AppState) -> Option<String> {
    match state.view {
        View::Logs => yank_from_logs(state),
        View::LogDetail => crate::ui::views::logs_detail::selected_line(state)
            .map(crate::ui::views::logs_detail::yank_text),
        View::List | View::Detail => state.selected_resource().map(|r| r.id.clone()),
        View::Subscriptions => state
            .subscriptions
            .get(state.subscription_cursor)
            .map(|s| s.id.clone()),
        View::Help => None,
    }
}

fn yank_from_logs(state: &AppState) -> Option<String> {
    let resource = state.selected_resource()?;
    let lines = state.logs.by_resource.get(&resource.id);
    let empty = lines.map(|l| l.is_empty()).unwrap_or(true);
    // Error banner is showing iff there's an error AND no rows to display.
    if empty {
        if let Some(err) = state.logs.last_error.as_deref() {
            return Some(crate::ui::views::logs::friendly_log_error(err));
        }
    }
    let lines = lines?;
    let cursor = state.logs.scroll.min(lines.len().saturating_sub(1));
    let line = lines.get(cursor)?;
    Some(format!(
        "{}  {:?}  {}  {}",
        line.ts.format("%Y-%m-%dT%H:%M:%SZ"),
        line.level,
        line.source,
        line.message
    ))
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
                // Empty stack: open the quit-confirmation modal instead of
                // hard-quitting. The event loop's top-priority handler
                // routes y/n until the user resolves it. `:q` and friends in
                // `run_command` bypass this and quit immediately — that's
                // explicit intent.
                None => {
                    state.quit_confirm = true;
                    state.quit_confirm_yes = false;
                }
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
                    state.metrics.failures.clear();
                    state.metrics.missing.clear();
                }
                state.loading_resources = true;
                spawn_load_resources(auth.clone(), sub_ids, tx.clone());
            }
        }
        View::Detail => {
            if let Some(resource) = state.selected_resource().cloned() {
                if force || !state.metrics.loading {
                    if force {
                        state.metrics.failures.remove(&resource.id);
                    }
                    state.metrics.loading = true;
                    state.metrics.pending.insert(resource.id.clone());
                    spawn_load_metrics(auth.clone(), resource, state.metrics.range, tx.clone());
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
        // LogDetail is a pure-view-over-state screen; nothing to load.
        View::LogDetail | View::Help => {}
    }
}

fn dispatch_view(f: &mut ratatui::Frame, area: Rect, state: &AppState, theme: &Theme) {
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
        View::LogDetail => crate::ui::views::logs_detail::render(f, view_area, state, theme),
        View::Help => crate::ui::views::help::render(f, view_area, state, theme),
    }

    // Quit-confirmation modal overlays the underlying view AND must beat the
    // command bar to the screen — render it before the command bar. (In
    // practice both flags can't be true at once given input gating.)
    if state.quit_confirm {
        render_quit_modal(f, area, state, theme);
    }
    if state.auth_prompt != AuthPrompt::Hidden {
        render_auth_modal(f, area, state, theme);
    }

    if let Some(ca) = command_area {
        render_command_bar(f, ca, state, theme);
    }
}

/// Render a centered "Are you sure you want to quit?" modal with focusable
/// Yes / No buttons. Caller invokes only when `state.quit_confirm`.
fn render_quit_modal(f: &mut ratatui::Frame, area: Rect, state: &AppState, theme: &Theme) {
    use ratatui::layout::Alignment;
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, Borders, Clear, Paragraph};

    let popup = centered_fixed_rect(50, 7, area);
    if popup.width == 0 || popup.height == 0 {
        return;
    }
    f.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(
            " quit? ",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(theme.bg).fg(theme.fg));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let focused_style = Style::default()
        .bg(theme.accent)
        .fg(theme.bg)
        .add_modifier(Modifier::BOLD);
    let unfocused_style = Style::default().fg(theme.muted);

    let yes_label = "  Yes  ";
    let no_label = "  No  ";
    let (yes_style, no_style) = if state.quit_confirm_yes {
        (focused_style, unfocused_style)
    } else {
        (unfocused_style, focused_style)
    };

    // Inner is 5 rows; lay out as: message, blank, buttons, blank, hint.
    let lines = vec![
        Line::from(Span::styled(
            "Are you sure you want to quit?",
            Style::default().fg(theme.fg),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(yes_label, yes_style),
            Span::raw("    "),
            Span::styled(no_label, no_style),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "←/→ or Tab \u{2014} choose · Enter \u{2014} confirm · Esc \u{2014} cancel",
            Style::default().fg(theme.muted),
        )),
    ];
    let paragraph = Paragraph::new(lines).alignment(Alignment::Center);
    f.render_widget(paragraph, inner);
}

/// Open the auth prompt, optionally with a pre-populated error message that
/// the menu will surface so the user understands why it appeared.
fn open_auth_prompt(state: &mut AppState, error: Option<String>) {
    state.auth_prompt = AuthPrompt::Menu;
    state.auth_menu_focus = AuthMenuFocus::Browser;
    state.auth_last_error = error;
}

/// Key handler invoked while `auth_prompt != Hidden`. Mirrors the quit-modal
/// short-circuit: every key is either consumed by the modal or swallowed.
fn handle_auth_prompt_key(state: &mut AppState, key: crossterm::event::KeyEvent) {
    match state.auth_prompt {
        AuthPrompt::Hidden => {}
        AuthPrompt::Menu => match key.code {
            KeyCode::Esc => {
                state.auth_prompt = AuthPrompt::Hidden;
            }
            KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => {
                state.auth_menu_focus = match state.auth_menu_focus {
                    AuthMenuFocus::Browser => AuthMenuFocus::Tenant,
                    AuthMenuFocus::DeviceCode => AuthMenuFocus::Browser,
                    AuthMenuFocus::Tenant => AuthMenuFocus::DeviceCode,
                };
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                state.auth_menu_focus = match state.auth_menu_focus {
                    AuthMenuFocus::Browser => AuthMenuFocus::DeviceCode,
                    AuthMenuFocus::DeviceCode => AuthMenuFocus::Tenant,
                    AuthMenuFocus::Tenant => AuthMenuFocus::Browser,
                };
            }
            // Direct shortcuts.
            KeyCode::Char('l') | KeyCode::Char('L') => {
                state.pending_login = Some(PendingLogin {
                    tenant: state.auth_tenant.clone(),
                    use_device_code: false,
                });
            }
            KeyCode::Char('d') | KeyCode::Char('D') => {
                state.pending_login = Some(PendingLogin {
                    tenant: state.auth_tenant.clone(),
                    use_device_code: true,
                });
            }
            KeyCode::Char('t') | KeyCode::Char('T') => {
                state.auth_prompt = AuthPrompt::TenantInput;
                // Pre-fill with the existing tenant so editing is easy.
                let initial = state.auth_tenant.clone().unwrap_or_default();
                state.auth_tenant_input = Input::default().with_value(initial);
            }
            KeyCode::Enter => match state.auth_menu_focus {
                AuthMenuFocus::Browser => {
                    state.pending_login = Some(PendingLogin {
                        tenant: state.auth_tenant.clone(),
                        use_device_code: false,
                    });
                }
                AuthMenuFocus::DeviceCode => {
                    state.pending_login = Some(PendingLogin {
                        tenant: state.auth_tenant.clone(),
                        use_device_code: true,
                    });
                }
                AuthMenuFocus::Tenant => {
                    state.auth_prompt = AuthPrompt::TenantInput;
                    let initial = state.auth_tenant.clone().unwrap_or_default();
                    state.auth_tenant_input = Input::default().with_value(initial);
                }
            },
            _ => {}
        },
        AuthPrompt::TenantInput => match key.code {
            KeyCode::Esc => {
                state.auth_prompt = AuthPrompt::Menu;
            }
            KeyCode::Enter => {
                let v = state.auth_tenant_input.value().trim().to_string();
                state.auth_tenant = if v.is_empty() { None } else { Some(v) };
                state.auth_prompt = AuthPrompt::Menu;
            }
            _ => {
                state.auth_tenant_input.handle_event(&CtEvent::Key(key));
            }
        },
    }
}

/// Render the in-app `az login` modal. Two states: the menu (with three
/// focusable rows + last error if any) and the tenant-input field.
fn render_auth_modal(f: &mut ratatui::Frame, area: Rect, state: &AppState, theme: &Theme) {
    use ratatui::layout::Alignment;
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, Borders, Clear, Paragraph};

    let height: u16 = match state.auth_prompt {
        AuthPrompt::Menu => {
            // Title + 3 option rows + tenant summary + hint + optional 2-line error.
            let base = 11u16;
            if state.auth_last_error.is_some() {
                base + 2
            } else {
                base
            }
        }
        AuthPrompt::TenantInput => 9,
        AuthPrompt::Hidden => return,
    };
    let popup = centered_fixed_rect(64, height, area);
    if popup.width == 0 || popup.height == 0 {
        return;
    }
    f.render_widget(Clear, popup);

    let title = match state.auth_prompt {
        AuthPrompt::TenantInput => " az login · tenant ",
        _ => " az login ",
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(
            title,
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(theme.bg).fg(theme.fg));
    let inner = block.inner(popup);
    f.render_widget(block, popup);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let focused = Style::default()
        .bg(theme.accent)
        .fg(theme.bg)
        .add_modifier(Modifier::BOLD);
    let unfocused = Style::default().fg(theme.fg);
    let muted = Style::default().fg(theme.muted);

    match state.auth_prompt {
        AuthPrompt::Menu => {
            let mark = |f: AuthMenuFocus, want: AuthMenuFocus| {
                if f == want {
                    " ▍ "
                } else {
                    "   "
                }
            };
            let style_for = |want: AuthMenuFocus| {
                if state.auth_menu_focus == want {
                    focused
                } else {
                    unfocused
                }
            };
            let tenant_label = state
                .auth_tenant
                .as_deref()
                .map(|t| format!("[T] tenant…              {t}"))
                .unwrap_or_else(|| "[T] tenant…              (default)".to_string());

            let mut lines: Vec<Line> = vec![
                Line::from(Span::styled(
                    "no subscriptions visible · choose a login flow",
                    muted,
                )),
                Line::from(""),
                Line::from(vec![
                    Span::raw(mark(state.auth_menu_focus, AuthMenuFocus::Browser)),
                    Span::styled(
                        "[L] browser login        az login",
                        style_for(AuthMenuFocus::Browser),
                    ),
                ]),
                Line::from(vec![
                    Span::raw(mark(state.auth_menu_focus, AuthMenuFocus::DeviceCode)),
                    Span::styled(
                        "[D] device code          az login --use-device-code",
                        style_for(AuthMenuFocus::DeviceCode),
                    ),
                ]),
                Line::from(vec![
                    Span::raw(mark(state.auth_menu_focus, AuthMenuFocus::Tenant)),
                    Span::styled(tenant_label, style_for(AuthMenuFocus::Tenant)),
                ]),
                Line::from(""),
                Line::from(Span::styled(
                    "↑/↓ choose · Enter run · L/D shortcuts · T set tenant · Esc cancel",
                    muted,
                )),
            ];

            if let Some(err) = state.auth_last_error.as_deref() {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    truncate_error(err, inner.width as usize),
                    Style::default().fg(theme.degraded),
                )));
            }

            let p = Paragraph::new(lines).alignment(Alignment::Left);
            f.render_widget(p, inner);
        }
        AuthPrompt::TenantInput => {
            let value = state.auth_tenant_input.value();
            let lines = vec![
                Line::from(Span::styled(
                    "tenant id or domain (blank = default tenant)",
                    muted,
                )),
                Line::from(""),
                Line::from(vec![
                    Span::styled("> ", Style::default().fg(theme.accent)),
                    Span::styled(value.to_string(), Style::default().fg(theme.fg)),
                    Span::styled("█", Style::default().fg(theme.accent)),
                ]),
                Line::from(""),
                Line::from(Span::styled("Enter accept · Esc cancel", muted)),
            ];
            let p = Paragraph::new(lines).alignment(Alignment::Left);
            f.render_widget(p, inner);
        }
        AuthPrompt::Hidden => {}
    }
}

fn truncate_error(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if s.chars().count() <= width {
        return s.to_string();
    }
    let take = width.saturating_sub(1);
    let mut out: String = s.chars().take(take).collect();
    out.push('…');
    out
}

/// Centered rect of a fixed cell size (vs. percentage-based). Clamps to
/// `area` if the requested size doesn't fit.
fn centered_fixed_rect(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

/// Render the one-line command palette at `area`, styled like the search
/// input but with a `:` prompt instead of `>`.
fn render_command_bar(f: &mut ratatui::Frame, area: Rect, state: &AppState, theme: &Theme) {
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
                    if tx
                        .send(AppEvent::Resize {
                            width: w,
                            height: h,
                        })
                        .is_err()
                    {
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

fn spawn_load_resources(auth: AzureAuth, sub_ids: Vec<String>, tx: UnboundedSender<AppEvent>) {
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
        let _ = tx.send(AppEvent::MetricsLoaded {
            resource_id,
            result,
        });
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
            !state.metrics.by_resource.contains_key(&r.id) && !state.metrics.pending.contains(&r.id)
        })
        .cloned()
        .collect();
    for resource in to_fetch {
        state.metrics.pending.insert(resource.id.clone());
        spawn_load_metrics(auth.clone(), resource, range, tx.clone());
    }
}

fn spawn_load_health(auth: AzureAuth, resource_id: String, tx: UnboundedSender<AppEvent>) {
    tokio::spawn(async move {
        let result = crate::azure::resource_health::fetch(&auth, &resource_id)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::HealthLoaded {
            resource_id,
            result,
        });
    });
}

/// Kick off a Resource Health fetch for every loaded resource that doesn't
/// already have one cached or in flight. Mirrors `spawn_missing_list_metrics`.
fn spawn_missing_list_health(
    state: &mut AppState,
    auth: &AzureAuth,
    tx: &UnboundedSender<AppEvent>,
) {
    let to_fetch: Vec<String> = state
        .resources
        .iter()
        .filter(|r| {
            !state.health.by_resource.contains_key(&r.id) && !state.health.pending.contains(&r.id)
        })
        .map(|r| r.id.clone())
        .collect();
    for resource_id in to_fetch {
        state.health.pending.insert(resource_id.clone());
        spawn_load_health(auth.clone(), resource_id, tx.clone());
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
        let _ = tx.send(AppEvent::LogsLoaded {
            resource_id,
            result,
        });
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

        // Filter active: printable keys forward; Esc/Enter still reach the
        // dispatcher; arrows / page nav stay with the dispatcher so they can
        // drive the underlying list.
        state.list_filter_active = true;
        assert!(should_forward_to_filter(&state, k('a')));
        assert!(should_forward_to_filter(&state, k('/')));
        let backspace = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        assert!(should_forward_to_filter(&state, backspace));
        assert!(!should_forward_to_filter(&state, esc));
        assert!(!should_forward_to_filter(&state, enter));
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        let pgdn = KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE);
        let pgup = KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE);
        assert!(!should_forward_to_filter(&state, down));
        assert!(!should_forward_to_filter(&state, up));
        assert!(!should_forward_to_filter(&state, pgdn));
        assert!(!should_forward_to_filter(&state, pgup));
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
            assert!(
                state.status_message.is_none(),
                "{cmd} should not set status"
            );
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
        assert!(crate::ui::views::list::handle(
            Action::OpenSelected,
            &mut state
        ));
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

        // Back from Subscriptions with empty stack: opens the quit-confirm
        // modal instead of hard-quitting. The user must answer y/Enter (or
        // type `:q`) for `should_quit` to flip.
        assert!(apply_navigation_action(Action::Back, &mut state));
        assert!(state.quit_confirm);
        assert!(!state.should_quit);
    }

    #[test]
    fn back_with_empty_stack_opens_quit_modal() {
        let mut state = fresh_state();
        assert!(state.view_stack.is_empty());
        assert!(!state.quit_confirm);
        assert!(!state.should_quit);

        assert!(apply_navigation_action(Action::Back, &mut state));
        assert!(state.quit_confirm, "Back on empty stack opens the modal");
        assert!(
            !state.should_quit,
            "Back on empty stack does NOT quit directly — modal first"
        );
    }

    #[test]
    fn back_with_nonempty_stack_does_not_open_modal() {
        let mut state = fresh_state();
        // Start in List with Subscriptions on the stack (typical after Open).
        state.view = View::List;
        state.view_stack.push(View::Subscriptions);

        assert!(apply_navigation_action(Action::Back, &mut state));
        assert_eq!(state.view, View::Subscriptions, "Back popped the stack");
        assert!(state.view_stack.is_empty());
        assert!(
            !state.quit_confirm,
            "popping a non-empty stack must not open the quit modal"
        );
        assert!(!state.should_quit);
    }

    #[test]
    fn command_q_bypasses_modal() {
        let mut state = fresh_state();
        run_command(&mut state, "q");
        assert!(state.should_quit, ":q quits immediately");
        assert!(
            !state.quit_confirm,
            ":q is explicit intent and must not open the modal"
        );
    }

    #[test]
    fn list_badge_uses_resource_health_signal() {
        // When Resource Health says Available and there's no traffic but the
        // resource is Running, the list badge should render IDLE (not UNKNOWN
        // and not LOADING…). This pins the wiring between the cache, the
        // public `badge_for_row`, and `derive`'s decision table.
        use crate::azure::metrics::{MetricKind, MetricPoint, MetricSeries};
        use crate::azure::resource_health::{AvailabilityState, ResourceAvailability};
        use crate::azure::resources::{Resource, ResourceKind};
        use crate::ui::theme::Theme;
        use chrono::{Duration, Utc};

        let mut state = fresh_state();
        let resource = Resource {
            id: "/r/idle".into(),
            name: "quiet-func".into(),
            kind: ResourceKind::FunctionApp,
            location: "westeurope".into(),
            resource_group: "rg".into(),
            subscription_id: "sub".into(),
            state: Some("Running".into()),
        };
        state.resources = vec![resource.clone()];

        // Synthesize zero-traffic metrics so `derive` finds the series but
        // the trailing sum is 0 — the IDLE branch.
        let now = Utc::now();
        let zero_series = |kind| MetricSeries {
            kind,
            label: String::new(),
            unit: String::new(),
            points: (0..4)
                .map(|i| MetricPoint {
                    ts: now - Duration::minutes(15 * (4 - i)),
                    value: 0.0,
                })
                .collect(),
        };
        state.metrics.by_resource.insert(
            resource.id.clone(),
            vec![
                zero_series(MetricKind::Errors),
                zero_series(MetricKind::Traffic),
            ],
        );
        state.health.by_resource.insert(
            resource.id.clone(),
            ResourceAvailability {
                state: AvailabilityState::Available,
                reason: None,
            },
        );

        let theme = Theme::catppuccin_mocha();
        let (_, label) = crate::ui::views::list::badge_for_row(&resource, &state, &theme);
        assert_eq!(label.trim(), "IDLE");

        // And with traffic + healthy ratio under Available, we should get HEALTHY.
        let traffic_series = MetricSeries {
            kind: MetricKind::Traffic,
            label: String::new(),
            unit: String::new(),
            points: (0..4)
                .map(|i| MetricPoint {
                    ts: now - Duration::minutes(15 * (4 - i)),
                    value: 250.0,
                })
                .collect(),
        };
        state.metrics.by_resource.insert(
            resource.id.clone(),
            vec![zero_series(MetricKind::Errors), traffic_series],
        );
        let (_, label) = crate::ui::views::list::badge_for_row(&resource, &state, &theme);
        assert_eq!(label.trim(), "HEALTHY");
    }

    #[test]
    fn yank_in_logs_prefers_error_when_table_empty() {
        use crate::azure::resources::{Resource, ResourceKind};

        let mut state = fresh_state();
        state.resources = vec![Resource {
            id: "/r/one".into(),
            name: "alpha-func".into(),
            kind: ResourceKind::FunctionApp,
            location: "westeurope".into(),
            resource_group: "rg".into(),
            subscription_id: "sub".into(),
            state: Some("Running".into()),
        }];
        state.list_cursor = 0;
        state.view = View::Logs;
        state.logs.last_error = Some(
            r#"azure api error 400: {"error":{"message":"outer","code":"BadArgumentError","innererror":{"code":"SEM0100","message":"column X not found"}}}"#
                .into(),
        );

        let yanked = yank_target(&state).expect("error banner should yield text");
        assert!(yanked.contains("column X not found"));
    }

    #[test]
    fn yank_in_logs_returns_selected_line_when_rows_present() {
        use crate::azure::logs::{LogLevel, LogLine};
        use crate::azure::resources::{Resource, ResourceKind};
        use chrono::{TimeZone, Utc};

        let mut state = fresh_state();
        state.resources = vec![Resource {
            id: "/r/one".into(),
            name: "alpha-func".into(),
            kind: ResourceKind::FunctionApp,
            location: "westeurope".into(),
            resource_group: "rg".into(),
            subscription_id: "sub".into(),
            state: Some("Running".into()),
        }];
        state.list_cursor = 0;
        state.view = View::Logs;
        let ts = Utc
            .with_ymd_and_hms(2026, 5, 10, 12, 0, 0)
            .single()
            .unwrap();
        state.logs.by_resource.insert(
            "/r/one".into(),
            vec![LogLine {
                ts,
                level: LogLevel::Error,
                source: "AppExceptions".into(),
                message: "kaboom".into(),
                fields: Vec::new(),
            }],
        );

        let yanked = yank_target(&state).expect("selected line should yield text");
        assert!(yanked.contains("AppExceptions"));
        assert!(yanked.contains("kaboom"));
        assert!(yanked.contains("2026-05-10T12:00:00Z"));
    }
}
