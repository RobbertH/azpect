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
                let now = Instant::now();
                input.maybe_expire(now);
                // Auto-clear the bottom status hint once its deadline passes
                // so transient messages ("copied 245 bytes…") don't stick.
                if let Some(deadline) = state.status_message_until {
                    if now >= deadline {
                        state.status_message = None;
                        state.status_message_until = None;
                    }
                }
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
                // except Esc (cancel), Enter (execute), and Tab / Shift+Tab
                // (completion cycle) flow into the command input buffer.
                if state.command_active {
                    match key.code {
                        KeyCode::Esc => {
                            state.command_active = false;
                            state.command_input.reset();
                            state.command_tab_cycle = None;
                            continue;
                        }
                        KeyCode::Enter => {
                            let cmd = state.command_input.value().to_string();
                            // Closing the palette before dispatching the
                            // action so a status message from `run_command`
                            // shows in the cleared bottom-of-screen layout.
                            state.command_active = false;
                            state.command_input.reset();
                            state.command_tab_cycle = None;
                            let view_before = state.view;
                            run_command(state, &cmd);
                            // `:refresh` is the one command we can't fully
                            // resolve inside `run_command` because the load
                            // helpers need `auth` and `tx`. Route it through
                            // the normal force-refresh path the `r` key uses.
                            if cmd.trim() == "refresh" {
                                kick_off_loads_for_view(state, auth, tx, true);
                            } else if state.view != view_before {
                                // Navigation command (`:storage`, `:apis`,
                                // …) just transitioned views. Kick off
                                // whatever loads the new view needs, the same
                                // way the after-action chain does for keys.
                                kick_off_loads_for_view(state, auth, tx, false);
                            }
                            if state.should_quit {
                                break;
                            }
                            continue;
                        }
                        KeyCode::Tab => {
                            step_palette_tab_cycle(state, true);
                            continue;
                        }
                        KeyCode::BackTab => {
                            step_palette_tab_cycle(state, false);
                            continue;
                        }
                        _ => {
                            if should_forward_to_command(state, key) {
                                // Any new keystroke breaks the cycle so the
                                // next Tab rebuilds candidates against the
                                // freshly-edited buffer.
                                state.command_tab_cycle = None;
                                state.command_input.handle_event(&CtEvent::Key(key));
                                continue;
                            }
                        }
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
                // Same shape for the logs less-style search input.
                if should_forward_to_logs_search(state, key) {
                    state.logs.search_input.handle_event(&CtEvent::Key(key));
                    continue;
                }
                // Storage blobs name filter shares the same carve-out shape.
                // Reset the cursor so a shrinking match list never points past
                // the end of `filtered_blobs()`.
                if should_forward_to_blobs_filter(state, key) {
                    state.storage.blobs_filter.handle_event(&CtEvent::Key(key));
                    state.storage.blobs_cursor = 0;
                    continue;
                }
                // Storage containers name filter shares the same carve-out shape.
                // Reset the cursor so a shrinking match list never points past
                // the end of `filtered_containers()`.
                if should_forward_to_containers_filter(state, key) {
                    state
                        .storage
                        .containers_filter
                        .handle_event(&CtEvent::Key(key));
                    state.storage.containers_cursor = 0;
                    continue;
                }
                // Storage accounts name filter shares the same carve-out shape.
                // Reset the cursor so a shrinking match list never points past
                // the end of `filtered_accounts()`.
                if should_forward_to_accounts_filter(state, key) {
                    state
                        .storage
                        .accounts_filter
                        .handle_event(&CtEvent::Key(key));
                    state.storage.accounts_cursor = 0;
                    continue;
                }
                let action = decide_action(&mut input, key, state);
                if action != Action::Noop {
                    apply_action(action, state, auth, tx);
                }
                drain_fetch_more_requested(state, auth, tx);
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
                        state.set_status(format!("subscriptions: {msg}"));
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
                        spawn_missing_container_app_limits(state, auth, tx);
                    }
                    Err(e) => state.set_status(format!("resources: {e}")),
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
                append,
                result,
            } => {
                if append {
                    state.logs.loading_more = false;
                } else {
                    state.logs.loading = false;
                }
                match result {
                    Ok(page) => {
                        state.logs.last_error = None;
                        state
                            .logs
                            .more_available
                            .insert(resource_id.clone(), page.has_more);
                        if append {
                            let entry = state.logs.by_resource.entry(resource_id).or_default();
                            entry.extend(page.lines);
                        } else {
                            state.logs.by_resource.insert(resource_id, page.lines);
                        }
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
            AppEvent::ContainerAppLimitsLoaded {
                resource_id,
                result,
            } => {
                state.limits.pending.remove(&resource_id);
                if let Ok(limits) = result {
                    state.limits.by_resource.insert(resource_id, limits);
                }
                // On error we silently leave the cache empty; the detail view
                // just omits the "/ max" suffix rather than surfacing noise
                // for a non-critical decoration.
            }
            AppEvent::ContainerAppRevisionMetaLoaded {
                resource_id,
                result,
            } => {
                if let Ok(Some(meta)) = result {
                    state.revision_meta.by_resource.insert(resource_id, meta);
                }
                // Same silent-on-error policy as limits: decorative.
            }
            AppEvent::ApimApisLoaded { service_id, result } => {
                state.apim.apis_pending.remove(&service_id);
                match result {
                    Ok(apis) => {
                        state.apim.apis_error.remove(&service_id);
                        state.apim.apis.insert(service_id, apis);
                    }
                    Err(e) => {
                        state.apim.apis.remove(&service_id);
                        state.apim.apis_error.insert(service_id, e);
                    }
                }
            }
            AppEvent::ApimOperationsLoaded { api_id, result } => {
                state.apim.operations_pending.remove(&api_id);
                match result {
                    Ok(ops) => {
                        state.apim.operations_error.remove(&api_id);
                        state.apim.operations.insert(api_id, ops);
                    }
                    Err(e) => {
                        state.apim.operations.remove(&api_id);
                        state.apim.operations_error.insert(api_id, e);
                    }
                }
            }
            AppEvent::ApimOperationPolicyLoaded {
                operation_id,
                result,
            } => {
                state.apim.policy_pending.remove(&operation_id);
                match result {
                    Ok(content) => {
                        state.apim.policy_error.remove(&operation_id);
                        state.apim.policy.insert(operation_id, content);
                    }
                    Err(e) => {
                        state.apim.policy.remove(&operation_id);
                        state.apim.policy_error.insert(operation_id, e);
                    }
                }
            }
            AppEvent::AppGatewayBackendsLoaded {
                resource_id,
                result,
            } => {
                state.appgw.pools_pending.remove(&resource_id);
                match result {
                    Ok(pools) => {
                        state.appgw.pools_error.remove(&resource_id);
                        state.appgw.pools.insert(resource_id, pools);
                    }
                    Err(e) => {
                        state.appgw.pools.remove(&resource_id);
                        state.appgw.pools_error.insert(resource_id, e);
                    }
                }
            }
            AppEvent::StorageAccountsLoaded(res) => {
                state.storage.accounts_pending = false;
                match res {
                    Ok(accounts) => {
                        state.storage.accounts_error = None;
                        // Clamp cursor if the previous list was longer than the
                        // freshly-fetched one (e.g. user switched subscription).
                        if !accounts.is_empty() && state.storage.accounts_cursor >= accounts.len() {
                            state.storage.accounts_cursor = accounts.len() - 1;
                        }
                        state.storage.accounts = Some(accounts);
                    }
                    Err(e) => {
                        state.storage.accounts = None;
                        state.storage.accounts_error = Some(e);
                    }
                }
            }
            AppEvent::StorageContainersLoaded { account_id, result } => {
                state.storage.containers_pending.remove(&account_id);
                match result {
                    Ok(containers) => {
                        state.storage.containers_error.remove(&account_id);
                        state.storage.containers.insert(account_id, containers);
                    }
                    Err(e) => {
                        state.storage.containers.remove(&account_id);
                        state.storage.containers_error.insert(account_id, e);
                    }
                }
            }
            AppEvent::StorageOverviewLoaded { account_id, result } => {
                state.storage.overview_pending.remove(&account_id);
                match result {
                    Ok(stats) => {
                        state.storage.overview_error.remove(&account_id);
                        state.storage.overview_stats.insert(account_id, stats);
                    }
                    Err(e) => {
                        state.storage.overview_stats.remove(&account_id);
                        state.storage.overview_error.insert(account_id, e);
                    }
                }
            }
            AppEvent::StorageBlobsLoaded { key, result } => {
                state.storage.blobs_pending.remove(&key);
                match result {
                    Ok(blobs) => {
                        state.storage.blobs_error.remove(&key);
                        state.storage.blobs.insert(key, blobs);
                    }
                    Err(e) => {
                        state.storage.blobs.remove(&key);
                        state.storage.blobs_error.insert(key, e);
                    }
                }
            }
            AppEvent::StorageBlobPreviewLoaded { key, result } => {
                state.storage.blob_preview_pending.remove(&key);
                match result {
                    Ok(preview) => {
                        state.storage.blob_preview_error.remove(&key);
                        state.storage.blob_preview.insert(key, preview);
                    }
                    Err(e) => {
                        state.storage.blob_preview.remove(&key);
                        state.storage.blob_preview_error.insert(key, e);
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
            state.set_status("logged in via az");
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

/// Mirror of `should_forward_to_filter` for the logs view's less-style search
/// box. Same carve-outs: arrows / page nav drive the underlying table so the
/// user can scroll context around the live highlights while still typing.
fn should_forward_to_logs_search(state: &AppState, key: crossterm::event::KeyEvent) -> bool {
    state.logs.search_active
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
/// input widget should consume. `Esc` (cancel), `Enter` (execute), and
/// `Tab` / `BackTab` (completion cycle) are reserved and handled directly by
/// the event loop.
fn should_forward_to_command(state: &AppState, key: crossterm::event::KeyEvent) -> bool {
    state.command_active
        && !matches!(
            key.code,
            KeyCode::Esc | KeyCode::Enter | KeyCode::Tab | KeyCode::BackTab
        )
}

/// Mirror of `should_forward_to_filter` for the storage-blobs name filter.
/// Only forwards while the blobs view has its filter box focused; same Esc /
/// Enter carve-out so cancel / commit still reach the dispatcher.
fn should_forward_to_blobs_filter(state: &AppState, key: crossterm::event::KeyEvent) -> bool {
    state.view == View::StorageBlobs
        && state.storage.blobs_filter_active
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

/// Mirror of `should_forward_to_filter` for the storage-containers name
/// filter. Only forwards while the containers view has its filter box
/// focused; same Esc / Enter / arrow carve-outs so cancel / commit / nav
/// still reach the dispatcher.
fn should_forward_to_containers_filter(state: &AppState, key: crossterm::event::KeyEvent) -> bool {
    state.view == View::StorageContainers
        && state.storage.containers_filter_active
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

/// Mirror of `should_forward_to_filter` for the storage-accounts name filter.
/// Only forwards while the accounts view has its filter box focused; same
/// Esc / Enter / arrow carve-outs so cancel / commit / nav still reach the
/// dispatcher.
fn should_forward_to_accounts_filter(state: &AppState, key: crossterm::event::KeyEvent) -> bool {
    state.view == View::StorageAccounts
        && state.storage.accounts_filter_active
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

/// Registered palette commands: `(canonical, aliases, description)`. Each
/// entry produces one canonical name plus zero or more aliases that resolve
/// to the same action in [`run_command`]. The list also feeds Tab-completion
/// — every name (canonical + aliases) is a candidate for prefix-matching.
const PALETTE_COMMANDS: &[(&str, &[&str])] = &[
    ("storage", &["s"]),
    ("apis", &["a", "resources", "r"]),
    ("subscriptions", &["subs"]),
    ("help", &["h", "?"]),
    ("quit", &["q"]),
    ("refresh", &[]),
];

/// Flattened list of every palette name (canonical + aliases) plus the legacy
/// vim-style quit aliases. Returned in deterministic order so Tab-completion
/// cycles predictably.
fn palette_completion_candidates() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (canonical, aliases) in PALETTE_COMMANDS {
        out.push((*canonical).to_string());
        for a in *aliases {
            out.push((*a).to_string());
        }
    }
    // Legacy vim-style aliases for `quit` — still recognised by `run_command`
    // and worth completing so muscle memory works.
    for extra in ["qa", "qa!", "quitall"] {
        out.push(extra.to_string());
    }
    out
}

/// Parse and execute a command palette buffer. Unknown commands surface a
/// status message instead of crashing; empty input is silently ignored.
fn run_command(state: &mut AppState, cmd: &str) {
    let trimmed = cmd.trim();
    match trimmed {
        "" => {} // empty: silently ignore
        // `:q` and friends quit immediately and intentionally bypass the
        // quit-confirmation modal: typing `:q` is explicit user intent, not
        // an accidental Esc press.
        "q" | "quit" | "qa" | "qa!" | "quitall" => state.should_quit = true,
        // Top-level Storage mode. Mirrors the `S` key path: push the current
        // view onto the stack so Esc walks back; reset the cursor; load on
        // arrival is handled by the after-action side-effect chain — for
        // palette commands we trigger that via `apply_palette_command`.
        "storage" | "s" => {
            if !matches!(
                state.view,
                View::StorageAccounts
                    | View::StorageAccountOverview
                    | View::StorageContainers
                    | View::StorageBlobs
                    | View::StorageBlobDetail
            ) {
                state.view_stack.push(state.view);
                state.view = View::StorageAccounts;
                state.storage.accounts_cursor = 0;
            }
        }
        "apis" | "a" | "resources" | "r" => {
            if state.view != View::List {
                state.view_stack.push(state.view);
                state.view = View::List;
            }
        }
        "subscriptions" | "subs" => {
            if state.view != View::Subscriptions {
                state.view_stack.push(state.view);
                state.view = View::Subscriptions;
            }
        }
        "help" | "h" | "?" => {
            if state.view != View::Help {
                state.view_stack.push(state.view);
                state.view = View::Help;
            }
        }
        // Refresh is handled by the caller after we return — it needs `auth`
        // and `tx`, which `run_command` doesn't have. Signal via status hint
        // and let the dispatcher kick off the load.
        "refresh" => {
            // Nothing to do here: `dispatch_command_palette` peeks the buffer
            // before calling `run_command` and routes refresh through the
            // normal `kick_off_loads_for_view` path.
        }
        other => {
            state.set_status(format!("unknown command: :{other}"));
        }
    }
}

/// Compute the next Tab-completion candidate set for the current buffer. The
/// candidates are every palette name (canonical + aliases) whose name starts
/// with the trimmed buffer, in registration order.
fn palette_tab_candidates(buffer: &str) -> Vec<String> {
    let needle = buffer.trim();
    palette_completion_candidates()
        .into_iter()
        .filter(|name| name.starts_with(needle))
        .collect()
}

/// Inline ghost-text suffix shown after the typed input — the part Tab would
/// fill in. Returns the empty string when the buffer is empty, when no
/// candidate prefix-matches, or when the buffer already equals the first
/// candidate (nothing left to suggest).
fn palette_ghost_hint(buffer: &str) -> String {
    let needle = buffer.trim();
    if needle.is_empty() {
        return String::new();
    }
    palette_tab_candidates(buffer)
        .into_iter()
        .next()
        .and_then(|cand| cand.strip_prefix(needle).map(|s| s.to_string()))
        .unwrap_or_default()
}

/// Advance (or rewind) the palette Tab-completion cycle by one step. Builds
/// the candidate list lazily on the first Tab; subsequent Tabs cycle through
/// it. `forward = false` walks backward (Shift+Tab).
fn step_palette_tab_cycle(state: &mut AppState, forward: bool) {
    let buffer = state.command_input.value().to_string();
    // Lazy init: build candidates from whatever the user has typed so far.
    if state.command_tab_cycle.is_none() {
        let candidates = palette_tab_candidates(&buffer);
        if candidates.is_empty() {
            return;
        }
        state.command_tab_cycle = Some((buffer, candidates, 0));
    } else if let Some((_, cands, idx)) = state.command_tab_cycle.as_mut() {
        if cands.is_empty() {
            return;
        }
        let len = cands.len();
        *idx = if forward {
            (*idx + 1) % len
        } else {
            // Wrap-around without going negative.
            (*idx + len - 1) % len
        };
    }
    // Apply the current candidate to the input.
    if let Some((_, cands, idx)) = state.command_tab_cycle.as_ref() {
        if let Some(cand) = cands.get(*idx).cloned() {
            state.command_input = Input::default().with_value(cand);
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

    // Any text-input widget currently focused? While the user is typing into
    // one, printable keys must not fire global actions like `n` (next match)
    // or open a chord — they belong to the input field.
    let input_focused = state.list_filter_active
        || state.logs.search_active
        || (state.view == View::StorageBlobs && state.storage.blobs_filter_active)
        || (state.view == View::StorageContainers && state.storage.containers_filter_active)
        || (state.view == View::StorageAccounts && state.storage.accounts_filter_active);

    // First-key-of-chord? Stash and wait.
    if is_chord_starter(key, input_focused) {
        input.pending_chord = Some(('g', now));
        return Action::Noop;
    }

    key_to_action(key, state.view, input_focused)
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
        View::ApimApis => crate::ui::views::apim_apis::handle(action, state),
        View::ApimOperations => crate::ui::views::apim_operations::handle(action, state),
        View::ApimPolicy => crate::ui::views::apim_policy::handle(action, state),
        View::AppGatewayBackends => crate::ui::views::appgw_backends::handle(action, state),
        View::StorageAccounts => crate::ui::views::storage_accounts::handle(action, state),
        View::StorageAccountOverview => {
            crate::ui::views::storage_account_overview::handle(action, state)
        }
        View::StorageContainers => crate::ui::views::storage_containers::handle(action, state),
        View::StorageBlobs => crate::ui::views::storage_blobs::handle(action, state),
        View::StorageBlobDetail => crate::ui::views::storage_blob_detail::handle(action, state),
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
        state.set_status("nothing to copy");
        return;
    };
    match crate::ui::clipboard::copy(&text) {
        Ok(n) => {
            state.set_status(format!("copied {n} bytes to clipboard"));
        }
        Err(e) => {
            state.set_status(format!("clipboard write failed: {e}"));
        }
    }
}

/// Compute the contextual portal URL for the current view and hand it to the
/// system default browser. Posts a status hint on success or failure.
fn do_open_in_browser(state: &mut AppState) {
    let Some(url) = portal_url_for(state) else {
        state.set_status("nothing to open");
        return;
    };
    match open::that_detached(&url) {
        Ok(()) => state.set_status(format!("opened {url}")),
        Err(e) => state.set_status(format!("failed to open browser: {e}")),
    }
}

/// Resolve what `o` should open. List/Detail open the selected resource's
/// overview blade; Logs/LogDetail jump straight to the resource's Logs (KQL)
/// blade so the user lands on the same signal they were just reading.
/// Subscriptions opens the highlighted subscription's overview.
/// Trim the `/apis/{apiName}` suffix off an API resource id to recover the
/// owning APIM service id. Returns `None` if the shape doesn't match.
fn service_id_from_api_id(api_id: &str) -> Option<&str> {
    api_id.rsplit_once("/apis/").map(|(svc, _)| svc)
}

fn portal_url_for(state: &AppState) -> Option<String> {
    const PORTAL_BASE: &str = "https://portal.azure.com/#@/resource";
    match state.view {
        View::List | View::Detail => state
            .selected_resource()
            .map(|r| format!("{PORTAL_BASE}{}", r.id)),
        View::Logs | View::LogDetail => state
            .selected_resource()
            .map(|r| format!("{PORTAL_BASE}{}/logs", r.id)),
        // Per-API/per-operation resource URLs land on Azure's generic resource
        // view, not the APIM editor — so from any APIM view we open the
        // service's APIs blade where the real management UX lives.
        View::ApimApis => state
            .selected_resource()
            .map(|r| format!("{PORTAL_BASE}{}/apim-apis", r.id)),
        View::ApimOperations | View::ApimPolicy => state
            .apim
            .selected_api_id
            .as_deref()
            .and_then(service_id_from_api_id)
            .map(|svc_id| format!("{PORTAL_BASE}{svc_id}/apim-apis")),
        View::AppGatewayBackends => state
            .selected_resource()
            .map(|r| format!("{PORTAL_BASE}{}", r.id)),
        View::Subscriptions => state
            .subscriptions
            .get(state.subscription_cursor)
            .map(|s| format!("{PORTAL_BASE}/subscriptions/{}/overview", s.id)),
        // Storage views land on the account's overview blade; the portal
        // exposes the container/blob drill-down off of there. The cursor
        // indexes into the *filtered* view so `o` follows what's on screen.
        View::StorageAccounts => state
            .storage
            .filtered_accounts()
            .get(state.storage.accounts_cursor)
            .map(|a| format!("{PORTAL_BASE}{}", a.id)),
        View::StorageAccountOverview
        | View::StorageContainers
        | View::StorageBlobs
        | View::StorageBlobDetail => state
            .storage
            .selected_account
            .as_ref()
            .map(|a| format!("{PORTAL_BASE}{}", a.id)),
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
        View::ApimApis => state.selected_resource().and_then(|r| {
            state
                .apim
                .apis
                .get(&r.id)
                .and_then(|rows| rows.get(state.apim.apis_cursor))
                .map(|api| api.id.clone())
        }),
        View::ApimOperations => state.apim.selected_api_id.as_deref().and_then(|api_id| {
            state
                .apim
                .operations
                .get(api_id)
                .and_then(|rows| rows.get(state.apim.operations_cursor))
                .map(|op| op.id.clone())
        }),
        View::ApimPolicy => crate::ui::views::apim_policy::yank_text(state)
            .or_else(|| state.apim.selected_operation_id.clone()),
        View::AppGatewayBackends => crate::ui::views::appgw_backends::yank_text(state)
            .or_else(|| state.selected_resource().map(|r| r.id.clone())),
        View::Subscriptions => state
            .subscriptions
            .get(state.subscription_cursor)
            .map(|s| s.id.clone()),
        View::StorageAccounts => state
            .storage
            .filtered_accounts()
            .get(state.storage.accounts_cursor)
            .map(|a| a.id.clone()),
        View::StorageAccountOverview => state
            .storage
            .selected_account
            .as_ref()
            .map(|a| a.id.clone()),
        View::StorageContainers => {
            let acc = state.storage.selected_account.as_ref()?;
            state
                .storage
                .filtered_containers(&acc.id)
                .get(state.storage.containers_cursor)
                .map(|c| format!("{}/{}", acc.name, c.name))
        }
        View::StorageBlobs => crate::ui::views::storage_blobs::yank_text(state),
        View::StorageBlobDetail => {
            crate::ui::views::storage_blob_detail::yank_text(state).or_else(|| {
                let acc = state.storage.selected_account.as_ref()?;
                let cont = state.storage.selected_container.as_deref()?;
                let blob = state.storage.selected_blob.as_deref()?;
                Some(format!("{}/{}/{}", acc.name, cont, blob))
            })
        }
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
            // Esc / q goes one abstraction level *up* in the breadcrumb tree —
            // not back through navigation history. So opening Storage with `S`
            // from a Function-App Logs view and then pressing Esc takes you to
            // `Subscriptions` (the parent of `StorageAccounts`), not back to
            // the Logs view you came from. Help stays modal: it pops the stack
            // so `?` from anywhere round-trips correctly.
            if state.view == View::Help {
                if let Some(prev) = state.view_stack.pop() {
                    state.view = prev;
                    return true;
                }
            }
            match semantic_parent(state.view) {
                Some(parent) => state.view = parent,
                None => {
                    // Root view (Subscriptions): open quit-confirm modal.
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
        Action::OpenStorage => {
            // Capital `S` from any non-storage view enters top-level Storage
            // mode. Pushing the current view onto the stack lets Esc walk
            // back the way the user came. Idempotent within the storage
            // chain so repeated presses don't grow the stack.
            if !matches!(
                state.view,
                View::StorageAccounts
                    | View::StorageAccountOverview
                    | View::StorageContainers
                    | View::StorageBlobs
                    | View::StorageBlobDetail
            ) {
                state.view_stack.push(state.view);
                state.view = View::StorageAccounts;
                state.storage.accounts_cursor = 0;
            }
            true
        }
        _ => false,
    }
}

/// Returns the view one abstraction level up from `view` — the parent in the
/// breadcrumb tree. `None` means `view` is the root (Subscriptions); Esc on
/// the root opens the quit-confirm modal. `Help` is treated as modal and
/// handled separately via `view_stack` so `?` can be used from anywhere
/// without losing the underlying location.
fn semantic_parent(view: View) -> Option<View> {
    match view {
        View::Subscriptions => None,
        View::Help => None,
        View::List => Some(View::Subscriptions),
        View::Detail => Some(View::List),
        View::Logs => Some(View::Detail),
        View::LogDetail => Some(View::Logs),
        View::AppGatewayBackends => Some(View::List),
        View::ApimApis => Some(View::Detail),
        View::ApimOperations => Some(View::ApimApis),
        View::ApimPolicy => Some(View::ApimOperations),
        View::StorageAccounts => Some(View::Subscriptions),
        // Overview sits between accounts and containers. Esc from overview
        // returns to the accounts list — the parent in the breadcrumb tree.
        View::StorageAccountOverview => Some(View::StorageAccounts),
        View::StorageContainers => Some(View::StorageAccountOverview),
        View::StorageBlobs => Some(View::StorageContainers),
        View::StorageBlobDetail => Some(View::StorageBlobs),
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
        | Action::OpenStorage
        | Action::SetWindowHour
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
                    // Fresh fetch always starts from the newest end — drop the
                    // pagination metadata so the next page detection starts
                    // clean (otherwise a stale `more_available = false` would
                    // suppress fetch-more on the new dataset).
                    state.logs.more_available.remove(&resource.id);
                    state.logs.loading = true;
                    spawn_load_logs(
                        auth.clone(),
                        resource,
                        state.logs.range,
                        state.logs.errors_only,
                        None,
                        tx.clone(),
                    );
                }
            }
        }
        View::ApimApis => {
            if let Some(svc_id) = state
                .selected_resource()
                .map(|r| r.id.clone())
                .and_then(|id| {
                    if state
                        .selected_resource()
                        .map(|r| r.kind == crate::azure::resources::ResourceKind::Apim)
                        .unwrap_or(false)
                    {
                        Some(id)
                    } else {
                        None
                    }
                })
            {
                let cached = state.apim.apis.contains_key(&svc_id);
                let in_flight = state.apim.apis_pending.contains(&svc_id);
                if force || (!cached && !in_flight) {
                    if force {
                        state.apim.apis.remove(&svc_id);
                        state.apim.apis_error.remove(&svc_id);
                    }
                    state.apim.apis_pending.insert(svc_id.clone());
                    spawn_load_apim_apis(auth.clone(), svc_id, tx.clone());
                }
            }
        }
        View::ApimOperations => {
            if let Some(api_id) = state.apim.selected_api_id.clone() {
                let cached = state.apim.operations.contains_key(&api_id);
                let in_flight = state.apim.operations_pending.contains(&api_id);
                if force || (!cached && !in_flight) {
                    if force {
                        state.apim.operations.remove(&api_id);
                        state.apim.operations_error.remove(&api_id);
                    }
                    state.apim.operations_pending.insert(api_id.clone());
                    spawn_load_apim_operations(auth.clone(), api_id, tx.clone());
                }
            }
        }
        View::ApimPolicy => {
            if let Some(op_id) = state.apim.selected_operation_id.clone() {
                let cached = state.apim.policy.contains_key(&op_id);
                let in_flight = state.apim.policy_pending.contains(&op_id);
                if force || (!cached && !in_flight) {
                    if force {
                        state.apim.policy.remove(&op_id);
                        state.apim.policy_error.remove(&op_id);
                    }
                    state.apim.policy_pending.insert(op_id.clone());
                    spawn_load_apim_operation_policy(auth.clone(), op_id, tx.clone());
                }
            }
        }
        View::AppGatewayBackends => {
            // The drilled-into gateway is whatever is currently under the list
            // cursor; the view module's `gateway_id` helper enforces the kind
            // filter for us.
            if let Some(gw_id) = crate::ui::views::appgw_backends::gateway_id(state) {
                let cached = state.appgw.pools.contains_key(&gw_id);
                let in_flight = state.appgw.pools_pending.contains(&gw_id);
                if force || (!cached && !in_flight) {
                    if force {
                        state.appgw.pools.remove(&gw_id);
                        state.appgw.pools_error.remove(&gw_id);
                    }
                    state.appgw.pools_pending.insert(gw_id.clone());
                    spawn_load_appgw_backends(auth.clone(), gw_id, tx.clone());
                }
            }
        }
        View::StorageAccounts => {
            // Use the same subscription-scope rule as the resource list: a
            // single pinned subscription if one is selected, else everything
            // the credential can see.
            let cached = state.storage.accounts.is_some();
            let in_flight = state.storage.accounts_pending;
            if force || (!cached && !in_flight) {
                let sub_ids = match &state.selected_subscription {
                    Some(id) => vec![id.clone()],
                    None => state.subscriptions.iter().map(|s| s.id.clone()).collect(),
                };
                if force {
                    state.storage.accounts = None;
                    state.storage.accounts_error = None;
                }
                state.storage.accounts_pending = true;
                spawn_load_storage_accounts(auth.clone(), sub_ids, tx.clone());
            }
        }
        View::StorageAccountOverview => {
            // Overview is the new landing pad for an account: spawn the five
            // Azure Monitor calls only on first entry (or explicit Refresh).
            // The metrics are daily-resolution server-side, so re-fetching on
            // every revisit would just burn ARM quota for stale data.
            if let Some(account) = state.storage.selected_account.clone() {
                let cached = state.storage.overview_stats.contains_key(&account.id);
                let in_flight = state.storage.overview_pending.contains(&account.id);
                if force || (!cached && !in_flight) {
                    if force {
                        state.storage.overview_stats.remove(&account.id);
                        state.storage.overview_error.remove(&account.id);
                    }
                    state.storage.overview_pending.insert(account.id.clone());
                    spawn_load_storage_overview(auth.clone(), account, tx.clone());
                }
            }
        }
        View::StorageContainers => {
            if let Some(account) = state.storage.selected_account.clone() {
                let cached = state.storage.containers.contains_key(&account.id);
                let in_flight = state.storage.containers_pending.contains(&account.id);
                if force || (!cached && !in_flight) {
                    if force {
                        state.storage.containers.remove(&account.id);
                        state.storage.containers_error.remove(&account.id);
                    }
                    state.storage.containers_pending.insert(account.id.clone());
                    spawn_load_storage_containers(auth.clone(), account, tx.clone());
                }
            }
        }
        View::StorageBlobs => {
            if let (Some(acc), Some(container)) = (
                state.storage.selected_account.clone(),
                state.storage.selected_container.clone(),
            ) {
                let key = crate::ui::state::StorageCache::blobs_key(&acc.name, &container);
                let cached = state.storage.blobs.contains_key(&key);
                let in_flight = state.storage.blobs_pending.contains(&key);
                if force || (!cached && !in_flight) {
                    if force {
                        state.storage.blobs.remove(&key);
                        state.storage.blobs_error.remove(&key);
                    }
                    state.storage.blobs_pending.insert(key);
                    spawn_load_storage_blobs(auth.clone(), acc.name, container, tx.clone());
                }
            }
        }
        View::StorageBlobDetail => {
            if let (Some(acc), Some(container), Some(blob)) = (
                state.storage.selected_account.clone(),
                state.storage.selected_container.clone(),
                state.storage.selected_blob.clone(),
            ) {
                let key =
                    crate::ui::state::StorageCache::blob_preview_key(&acc.name, &container, &blob);
                let cached = state.storage.blob_preview.contains_key(&key);
                let in_flight = state.storage.blob_preview_pending.contains(&key);
                if force || (!cached && !in_flight) {
                    if force {
                        state.storage.blob_preview.remove(&key);
                        state.storage.blob_preview_error.remove(&key);
                    }
                    state.storage.blob_preview_pending.insert(key);
                    spawn_load_storage_blob_preview(
                        auth.clone(),
                        acc.name,
                        container,
                        blob,
                        tx.clone(),
                    );
                }
            }
        }
        // LogDetail is a pure-view-over-state screen; nothing to load.
        View::LogDetail | View::Help => {}
    }
}

/// Spawn an older-than fetch when the logs view raised `fetch_more_requested`.
/// View handlers can't spawn tasks themselves (no access to `auth` / `tx`), so
/// they set the flag and we drain it from the event loop. Drains exactly once
/// per action; double-presses while a fetch is already in flight are coalesced
/// by the `loading_more` guard.
fn drain_fetch_more_requested(
    state: &mut AppState,
    auth: &AzureAuth,
    tx: &UnboundedSender<AppEvent>,
) {
    if !std::mem::take(&mut state.logs.fetch_more_requested) {
        return;
    }
    if state.logs.loading || state.logs.loading_more {
        return;
    }
    let Some(resource) = state.selected_resource().cloned() else {
        return;
    };
    let oldest = state
        .logs
        .by_resource
        .get(&resource.id)
        .and_then(|lines| lines.last())
        .map(|l| l.ts);
    let Some(older_than) = oldest else {
        return;
    };
    state.logs.loading_more = true;
    spawn_load_logs(
        auth.clone(),
        resource,
        state.logs.range,
        state.logs.errors_only,
        Some(older_than),
        tx.clone(),
    );
}

fn dispatch_view(f: &mut ratatui::Frame, area: Rect, state: &AppState, theme: &Theme) {
    // Reserve a single-row strip at the very top for the global breadcrumb
    // bar, so every view sees the same `path > to > here` line. Carve before
    // the command/status rows so the breadcrumb is always at the top edge.
    let mut remaining = area;
    let breadcrumb_area = if remaining.height > 0 {
        let (row, below) = split_off_top_row(remaining);
        remaining = below;
        Some(row)
    } else {
        None
    };
    // Reserve bottom rows for, in order from the bottom up: the command bar
    // (when active) and the status hint (whenever `status_message` is set).
    // Both shrink the view area so the underlying view never overlaps them.
    let command_area = if state.command_active && remaining.height > 0 {
        let (above, row) = split_off_bottom_row(remaining);
        remaining = above;
        Some(row)
    } else {
        None
    };
    let status_area = if state.status_message.is_some() && remaining.height > 0 {
        let (above, row) = split_off_bottom_row(remaining);
        remaining = above;
        Some(row)
    } else {
        None
    };
    let view_area = remaining;

    if let Some(ba) = breadcrumb_area {
        crate::ui::breadcrumb::render(f, ba, state, theme);
    }

    match state.view {
        View::Subscriptions => crate::ui::views::subscriptions::render(f, view_area, state, theme),
        View::List => crate::ui::views::list::render(f, view_area, state, theme),
        View::Detail => crate::ui::views::detail::render(f, view_area, state, theme),
        View::Logs => crate::ui::views::logs::render(f, view_area, state, theme),
        View::LogDetail => crate::ui::views::logs_detail::render(f, view_area, state, theme),
        View::ApimApis => crate::ui::views::apim_apis::render(f, view_area, state, theme),
        View::ApimOperations => {
            crate::ui::views::apim_operations::render(f, view_area, state, theme)
        }
        View::ApimPolicy => crate::ui::views::apim_policy::render(f, view_area, state, theme),
        View::AppGatewayBackends => {
            crate::ui::views::appgw_backends::render(f, view_area, state, theme)
        }
        View::StorageAccounts => {
            crate::ui::views::storage_accounts::render(f, view_area, state, theme)
        }
        View::StorageAccountOverview => {
            crate::ui::views::storage_account_overview::render(f, view_area, state, theme)
        }
        View::StorageContainers => {
            crate::ui::views::storage_containers::render(f, view_area, state, theme)
        }
        View::StorageBlobs => crate::ui::views::storage_blobs::render(f, view_area, state, theme),
        View::StorageBlobDetail => {
            crate::ui::views::storage_blob_detail::render(f, view_area, state, theme)
        }
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

    if let Some(sa) = status_area {
        render_status_row(f, sa, state, theme);
    }
    if let Some(ca) = command_area {
        render_command_bar(f, ca, state, theme);
    }
}

/// Split a single-row strip off the top of `area`, returning the row itself
/// and the area below. Assumes `area.height >= 1`.
fn split_off_top_row(area: Rect) -> (Rect, Rect) {
    let row = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: 1,
    };
    let below = Rect {
        x: area.x,
        y: area.y + 1,
        width: area.width,
        height: area.height - 1,
    };
    (row, below)
}

/// Split a single-row strip off the bottom of `area`, returning the area
/// above and the row itself. Assumes `area.height >= 1`.
fn split_off_bottom_row(area: Rect) -> (Rect, Rect) {
    let row = Rect {
        x: area.x,
        y: area.y + area.height - 1,
        width: area.width,
        height: 1,
    };
    let above = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: area.height - 1,
    };
    (above, row)
}

/// Render the auto-expiring bottom-row status hint (set via
/// [`AppState::set_status`]). Caller has already confirmed
/// `state.status_message.is_some()`.
fn render_status_row(f: &mut ratatui::Frame, area: Rect, state: &AppState, theme: &Theme) {
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::Paragraph;
    let Some(msg) = state.status_message.as_deref() else {
        return;
    };
    let p = Paragraph::new(Line::from(Span::styled(
        format!(" {msg} "),
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
    )));
    f.render_widget(p, area);
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

    let buffer = state.command_input.value();
    let ghost = palette_ghost_hint(buffer);
    let mut spans = vec![
        Span::styled(":", Style::default().fg(theme.accent)),
        Span::styled(buffer, Style::default().fg(theme.fg)),
    ];
    if !ghost.is_empty() {
        spans.push(Span::styled(ghost, Style::default().fg(theme.muted)));
    }
    let p = Paragraph::new(Line::from(spans));
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

fn spawn_load_health(
    auth: AzureAuth,
    resource_id: String,
    kind: crate::azure::resources::ResourceKind,
    tx: UnboundedSender<AppEvent>,
) {
    use crate::azure::resources::ResourceKind;
    tokio::spawn(async move {
        // Container Apps don't expose meaningful state via the generic
        // Microsoft.ResourceHealth endpoint — it returns `Unknown` even when
        // active revisions are ActivationFailed/Unhealthy. The revisions
        // endpoint gives us both the authoritative availability signal and
        // the display metadata (active revision name, image, replicas), so
        // one fetch feeds two events.
        match kind {
            ResourceKind::ContainerApp => {
                match crate::azure::container_app_revisions::fetch(&auth, &resource_id).await {
                    Ok(info) => {
                        let _ = tx.send(AppEvent::HealthLoaded {
                            resource_id: resource_id.clone(),
                            result: Ok(info.availability),
                        });
                        let _ = tx.send(AppEvent::ContainerAppRevisionMetaLoaded {
                            resource_id,
                            result: Ok(info.active_revision),
                        });
                    }
                    Err(e) => {
                        let msg = format!("{e:#}");
                        let _ = tx.send(AppEvent::HealthLoaded {
                            resource_id,
                            result: Err(msg),
                        });
                    }
                }
            }
            _ => {
                let result = crate::azure::resource_health::fetch(&auth, &resource_id)
                    .await
                    .map_err(|e| format!("{e:#}"));
                let _ = tx.send(AppEvent::HealthLoaded {
                    resource_id,
                    result,
                });
            }
        }
    });
}

fn spawn_load_container_app_limits(
    auth: AzureAuth,
    resource_id: String,
    tx: UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        let result = crate::azure::container_app_limits::fetch(&auth, &resource_id)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::ContainerAppLimitsLoaded {
            resource_id,
            result,
        });
    });
}

/// Kick off a Container App template fetch for every Container App that
/// doesn't already have cached limits. Same eager-on-load pattern as
/// `spawn_missing_list_health`.
fn spawn_missing_container_app_limits(
    state: &mut AppState,
    auth: &AzureAuth,
    tx: &UnboundedSender<AppEvent>,
) {
    use crate::azure::resources::ResourceKind;
    let to_fetch: Vec<String> = state
        .resources
        .iter()
        .filter(|r| r.kind == ResourceKind::ContainerApp)
        .filter(|r| {
            !state.limits.by_resource.contains_key(&r.id) && !state.limits.pending.contains(&r.id)
        })
        .map(|r| r.id.clone())
        .collect();
    for resource_id in to_fetch {
        state.limits.pending.insert(resource_id.clone());
        spawn_load_container_app_limits(auth.clone(), resource_id, tx.clone());
    }
}

/// Kick off a Resource Health fetch for every loaded resource that doesn't
/// already have one cached or in flight. Mirrors `spawn_missing_list_metrics`.
fn spawn_missing_list_health(
    state: &mut AppState,
    auth: &AzureAuth,
    tx: &UnboundedSender<AppEvent>,
) {
    let to_fetch: Vec<(String, crate::azure::resources::ResourceKind)> = state
        .resources
        .iter()
        .filter(|r| {
            !state.health.by_resource.contains_key(&r.id) && !state.health.pending.contains(&r.id)
        })
        .map(|r| (r.id.clone(), r.kind))
        .collect();
    for (resource_id, kind) in to_fetch {
        state.health.pending.insert(resource_id.clone());
        spawn_load_health(auth.clone(), resource_id, kind, tx.clone());
    }
}

fn spawn_load_apim_apis(auth: AzureAuth, service_id: String, tx: UnboundedSender<AppEvent>) {
    tokio::spawn(async move {
        let result = crate::azure::apim::list_apis(&auth, &service_id)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::ApimApisLoaded { service_id, result });
    });
}

fn spawn_load_apim_operations(auth: AzureAuth, api_id: String, tx: UnboundedSender<AppEvent>) {
    tokio::spawn(async move {
        let result = crate::azure::apim::list_operations(&auth, &api_id)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::ApimOperationsLoaded { api_id, result });
    });
}

fn spawn_load_apim_operation_policy(
    auth: AzureAuth,
    operation_id: String,
    tx: UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        let result = crate::azure::apim::fetch_operation_policy(&auth, &operation_id)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::ApimOperationPolicyLoaded {
            operation_id,
            result,
        });
    });
}

fn spawn_load_appgw_backends(auth: AzureAuth, resource_id: String, tx: UnboundedSender<AppEvent>) {
    tokio::spawn(async move {
        let result = crate::azure::appgw_backends::list_backend_pools(&auth, &resource_id)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::AppGatewayBackendsLoaded {
            resource_id,
            result,
        });
    });
}

fn spawn_load_storage_accounts(
    auth: AzureAuth,
    sub_ids: Vec<String>,
    tx: UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        let result = crate::azure::storage::list_accounts(&auth, &sub_ids)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::StorageAccountsLoaded(result));
    });
}

fn spawn_load_storage_containers(
    auth: AzureAuth,
    account: crate::azure::storage::StorageAccount,
    tx: UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        let account_id = account.id.clone();
        let result = crate::azure::storage::list_containers(&auth, &account)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::StorageContainersLoaded { account_id, result });
    });
}

/// Five concurrent Azure Monitor calls (account scope + blob / file / queue /
/// table services) feeding the storage account overview panel. Cached behind
/// `overview_pending` / `overview_stats` to avoid re-fetching on re-entry —
/// these metrics update at most a few times per day server-side.
fn spawn_load_storage_overview(
    auth: AzureAuth,
    account: crate::azure::storage::StorageAccount,
    tx: UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        let account_id = account.id.clone();
        let result = crate::azure::storage::fetch_account_overview_stats(&auth, &account)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::StorageOverviewLoaded { account_id, result });
    });
}

fn spawn_load_storage_blobs(
    auth: AzureAuth,
    account_name: String,
    container: String,
    tx: UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        let key = crate::ui::state::StorageCache::blobs_key(&account_name, &container);
        // Filtering is now client-side, so we always fetch the full container
        // and pass `None` as the server-side prefix to `list_blobs`.
        let result = crate::azure::storage::list_blobs(&auth, &account_name, &container, None)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::StorageBlobsLoaded { key, result });
    });
}

/// Hard cap on how many bytes [`spawn_load_storage_blob_preview`] asks the
/// backend to fetch. Matches the documented "64 KB preview" contract.
const STORAGE_PREVIEW_MAX_BYTES: u64 = 64 * 1024;

fn spawn_load_storage_blob_preview(
    auth: AzureAuth,
    account_name: String,
    container: String,
    blob: String,
    tx: UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        let key =
            crate::ui::state::StorageCache::blob_preview_key(&account_name, &container, &blob);
        let result = crate::azure::storage::preview_blob(
            &auth,
            &account_name,
            &container,
            &blob,
            STORAGE_PREVIEW_MAX_BYTES,
        )
        .await
        .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::StorageBlobPreviewLoaded { key, result });
    });
}

fn spawn_load_logs(
    auth: AzureAuth,
    resource: Resource,
    range: TimeRange,
    errors_only: bool,
    older_than: Option<chrono::DateTime<chrono::Utc>>,
    tx: UnboundedSender<AppEvent>,
) {
    let append = older_than.is_some();
    tokio::spawn(async move {
        let resource_id = resource.id.clone();
        let result = crate::azure::logs::fetch(&auth, &resource, range, errors_only, older_than)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::LogsLoaded {
            resource_id,
            append,
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
    fn command_mode_storage_aliases_switch_view() {
        for cmd in ["storage", "s"] {
            let mut state = fresh_state();
            state.view = View::List;
            run_command(&mut state, cmd);
            assert_eq!(
                state.view,
                View::StorageAccounts,
                "{cmd} should open storage"
            );
            assert_eq!(state.view_stack, vec![View::List]);
            assert!(state.status_message.is_none());
        }
    }

    #[test]
    fn command_mode_resources_aliases_switch_view() {
        // `apis` is the canonical name; `a`, `resources`, and `r` are legacy
        // aliases that still route to the same view.
        for cmd in ["apis", "a", "resources", "r"] {
            let mut state = fresh_state();
            state.view = View::StorageAccounts;
            run_command(&mut state, cmd);
            assert_eq!(state.view, View::List, "{cmd} should land in apis list");
            assert!(state.status_message.is_none());
        }
    }

    #[test]
    fn command_mode_subscriptions_aliases_switch_view() {
        for cmd in ["subscriptions", "subs"] {
            let mut state = fresh_state();
            state.view = View::List;
            run_command(&mut state, cmd);
            assert_eq!(
                state.view,
                View::Subscriptions,
                "{cmd} should open subscription picker"
            );
            assert!(state.status_message.is_none());
        }
    }

    #[test]
    fn command_mode_help_aliases_open_help() {
        for cmd in ["help", "h", "?"] {
            let mut state = fresh_state();
            state.view = View::List;
            run_command(&mut state, cmd);
            assert_eq!(state.view, View::Help, "{cmd} should open help");
            assert!(state.status_message.is_none());
        }
    }

    #[test]
    fn command_mode_help_does_not_stack_help_on_itself() {
        // Regression guard: if the user is already on the Help view, `:h`
        // must not push Help onto its own stack (which would break Esc).
        let mut state = fresh_state();
        state.view = View::Help;
        run_command(&mut state, "help");
        assert_eq!(state.view, View::Help);
        assert!(state.view_stack.is_empty(), "no self-push allowed");
    }

    #[test]
    fn palette_tab_completion_matches_prefix() {
        // Empty prefix lists every registered name.
        let all = palette_tab_candidates("");
        assert!(all.contains(&"storage".to_string()));
        assert!(all.contains(&"apis".to_string()));
        // `resources` lingers as a legacy alias so muscle memory still works.
        assert!(all.contains(&"resources".to_string()));
        assert!(all.contains(&"subscriptions".to_string()));
        assert!(all.contains(&"refresh".to_string()));
        assert!(all.contains(&"quit".to_string()));

        // `s` matches both `storage` and `subscriptions` (and `subs`).
        let with_s = palette_tab_candidates("s");
        assert!(with_s.iter().any(|c| c == "storage"));
        assert!(with_s.iter().any(|c| c == "subscriptions"));
        assert!(with_s.iter().any(|c| c == "subs"));

        // `ap` narrows to `apis`.
        let with_ap = palette_tab_candidates("ap");
        assert!(with_ap.iter().any(|c| c == "apis"));
        assert!(!with_ap.iter().any(|c| c == "storage"));

        // `re` narrows to `resources` (legacy alias) / `refresh`.
        let with_re = palette_tab_candidates("re");
        assert!(with_re.iter().any(|c| c == "resources"));
        assert!(with_re.iter().any(|c| c == "refresh"));
        assert!(!with_re.iter().any(|c| c == "storage"));

        // Nonsense prefix returns nothing.
        assert!(palette_tab_candidates("zzz").is_empty());
    }

    #[test]
    fn palette_ghost_hint_shows_remainder_of_first_candidate() {
        // `st` → only `storage` matches, hint is the rest of the word.
        assert_eq!(palette_ghost_hint("st"), "orage");
        // `s` → first candidate (in registration order) is `storage`.
        assert_eq!(palette_ghost_hint("s"), "torage");
        // `re` → first candidate is `resources`.
        assert_eq!(palette_ghost_hint("re"), "sources");
        // Exact match: nothing left to suggest.
        assert_eq!(palette_ghost_hint("storage"), "");
        // No prefix match: no hint.
        assert_eq!(palette_ghost_hint("zzz"), "");
        // Empty buffer: no hint (don't show the first command as a phantom).
        assert_eq!(palette_ghost_hint(""), "");
        // Whitespace-only buffer is treated as empty.
        assert_eq!(palette_ghost_hint("  "), "");
    }

    #[test]
    fn palette_tab_cycle_forward_then_backward() {
        let mut state = fresh_state();
        state.command_active = true;
        state.command_input = Input::default().with_value("s".to_string());

        // First forward Tab: fills in the first match.
        step_palette_tab_cycle(&mut state, true);
        let first = state.command_input.value().to_string();
        assert!(
            first.starts_with('s'),
            "first cycle pick should start with s, got {first}"
        );

        // Second forward Tab: rotates to the next candidate.
        step_palette_tab_cycle(&mut state, true);
        let second = state.command_input.value().to_string();
        // Must still match the original prefix and (in this fixture) differ
        // from the first pick because there are multiple `s` candidates.
        assert!(second.starts_with('s'));
        assert_ne!(first, second);

        // Shift+Tab walks back to the first pick.
        step_palette_tab_cycle(&mut state, false);
        let back = state.command_input.value().to_string();
        assert_eq!(back, first, "backward step should land on prior candidate");
    }

    #[test]
    fn palette_tab_cycle_empty_buffer_lists_everything_starting_first() {
        let mut state = fresh_state();
        state.command_active = true;
        // Empty buffer: every command name is a candidate.
        step_palette_tab_cycle(&mut state, true);
        let pick = state.command_input.value().to_string();
        assert!(!pick.is_empty(), "should have inserted a candidate");
        // First candidate per registration order is `storage`.
        assert_eq!(pick, "storage");
    }

    #[test]
    fn palette_tab_cycle_no_match_is_noop() {
        let mut state = fresh_state();
        state.command_active = true;
        state.command_input = Input::default().with_value("zzz".to_string());
        step_palette_tab_cycle(&mut state, true);
        // Buffer is unchanged because there are no candidates.
        assert_eq!(state.command_input.value(), "zzz");
        assert!(state.command_tab_cycle.is_none());
    }

    #[test]
    fn palette_refresh_command_is_recognized() {
        // `:refresh` doesn't change the view by itself — the dispatcher
        // routes it through `kick_off_loads_for_view`. We just verify here
        // that `run_command` doesn't surface an "unknown command" status.
        let mut state = fresh_state();
        state.view = View::List;
        run_command(&mut state, "refresh");
        assert!(
            state.status_message.is_none(),
            "refresh should not be unknown"
        );
    }

    #[test]
    fn command_forwarding_excludes_tab_and_backtab() {
        // Tab / Shift+Tab are reserved for completion and must not flow into
        // the input widget. Esc and Enter were already covered upstream.
        let mut state = fresh_state();
        state.command_active = true;
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        let backtab = KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT);
        assert!(!should_forward_to_command(&state, tab));
        assert!(!should_forward_to_command(&state, backtab));
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
        // Esc walks the semantic-parent tree (Detail → List → Subscriptions →
        // quit modal) regardless of the history that led there. view_stack is
        // no longer manipulated by Back — it's reserved for Help's modal pop.
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
            created_at: None,
            modified_at: None,
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

        // Back from Detail: view-local handler must NOT consume it; semantic
        // parent of Detail is List.
        assert!(!crate::ui::views::detail::handle(Action::Back, &mut state));
        assert!(apply_navigation_action(Action::Back, &mut state));
        assert_eq!(state.view, View::List, "first Esc lands in List");
        assert!(!state.should_quit);

        // Back from List: semantic parent is Subscriptions — must NOT bounce back into Detail.
        assert!(!crate::ui::views::list::handle(Action::Back, &mut state));
        assert!(apply_navigation_action(Action::Back, &mut state));
        assert_eq!(
            state.view,
            View::Subscriptions,
            "second Esc lands in Subscriptions, never Detail"
        );
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
    fn back_from_non_root_view_does_not_open_modal() {
        // Back from any view with a semantic parent navigates there; the modal
        // only opens at the root (Subscriptions). view_stack content is
        // irrelevant to navigation now.
        let mut state = fresh_state();
        state.view = View::List;

        assert!(apply_navigation_action(Action::Back, &mut state));
        assert_eq!(
            state.view,
            View::Subscriptions,
            "Back from List walks to its semantic parent Subscriptions"
        );
        assert!(
            !state.quit_confirm,
            "navigating to a parent must not open the quit modal"
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
            created_at: None,
            modified_at: None,
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
            created_at: None,
            modified_at: None,
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
            created_at: None,
            modified_at: None,
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

    #[test]
    fn yank_in_log_detail_returns_full_record_with_fields() {
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
            created_at: None,
            modified_at: None,
        }];
        state.list_cursor = 0;
        state.view = View::LogDetail;
        let ts = Utc
            .with_ymd_and_hms(2026, 5, 10, 12, 0, 0)
            .single()
            .unwrap();
        state.logs.by_resource.insert(
            "/r/one".into(),
            vec![LogLine {
                ts,
                level: LogLevel::Error,
                source: "FunctionAppLogs/http_app_func".into(),
                message: "Executed (Failed, Id=abc)".into(),
                fields: vec![
                    ("FunctionInvocationId".into(), "abc-123".into()),
                    ("OperationId".into(), "op-456".into()),
                ],
            }],
        );

        let yanked = yank_target(&state).expect("log detail should yield text");
        assert!(yanked.contains("Executed (Failed, Id=abc)"));
        assert!(yanked.contains("FunctionInvocationId: abc-123"));
        assert!(yanked.contains("OperationId: op-456"));
    }

    #[test]
    fn portal_url_in_log_detail_points_to_resource_blade() {
        use crate::azure::resources::{Resource, ResourceKind};
        let mut state = fresh_state();
        state.resources = vec![Resource {
            id: "/subscriptions/X/resourceGroups/rg/providers/Microsoft.Web/sites/alpha".into(),
            name: "alpha".into(),
            kind: ResourceKind::FunctionApp,
            location: "westeurope".into(),
            resource_group: "rg".into(),
            subscription_id: "X".into(),
            state: Some("Running".into()),
            created_at: None,
            modified_at: None,
        }];
        state.list_cursor = 0;
        state.view = View::LogDetail;
        let url = portal_url_for(&state).expect("log detail should yield a portal URL");
        assert!(url.contains("portal.azure.com"));
        assert!(url.contains("/sites/alpha"));
        assert!(
            url.ends_with("/logs"),
            "log views should open the Logs blade, got {url}"
        );
    }
}
