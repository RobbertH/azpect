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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event as CtEvent, KeyCode, KeyEventKind,
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
use tui_input::Input;

use crate::azure::auth::AzureAuth;
use crate::azure::az_exec::{self, AzExecOptions};
use crate::azure::az_login::{self, AzLoginOptions};
use crate::azure::metrics::TimeRange;
use crate::azure::resources::{Resource, ResourceKind};
use crate::config::Config;
use crate::ui::events::{is_chord_starter, key_to_action, resolve_chord, Action, AppEvent};
use crate::ui::state::{
    AppState, AppliedEnvEdit, AuthMenuFocus, AuthPrompt, EnvVarEditMode, EnvVarEditPhase,
    EnvVarField, PendingExec, PendingLogin, View,
};
use crate::ui::theme::Theme;

/// How often the tick task fires. Drives spinner refresh, chord timeout, etc.
const TICK_INTERVAL: Duration = Duration::from_millis(250);

/// `g` chord must complete within this window or it's discarded.
const CHORD_TIMEOUT: Duration = Duration::from_millis(1000);

/// Max concurrent per-row health fetches. Each one fans out to an availability
/// call plus two Monitor metric calls, so the unbounded "one task per resource"
/// fan-out hit ARM/Monitor with a burst that throttled (429s) on large
/// subscriptions and made the badges *slower* to settle. A modest cap smooths the
/// burst — and matters more now that auto-refresh re-fires it on a timer.
/// The same gate covers the other per-resource list decorations (Container App
/// overview + replicas, Function App `config/web`) — they ride the same list
/// load / auto-refresh fan-out and were the remaining source of 429 bursts.
const HEALTH_FETCH_CONCURRENCY: usize = 8;

/// Process-wide gate limiting concurrent health fetches to [`HEALTH_FETCH_CONCURRENCY`].
/// Lazily created so it shares one permit pool across every `spawn_load_health`.
fn health_fetch_gate() -> std::sync::Arc<tokio::sync::Semaphore> {
    static GATE: std::sync::OnceLock<std::sync::Arc<tokio::sync::Semaphore>> =
        std::sync::OnceLock::new();
    GATE.get_or_init(|| std::sync::Arc::new(tokio::sync::Semaphore::new(HEALTH_FETCH_CONCURRENCY)))
        .clone()
}

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
        // Bracketed paste IS captured: it turns a paste into one `Event::Paste`
        // instead of a burst of key events, so stray pastes can't fire
        // keybindings (see `handle_paste`).
        execute!(stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
        Ok(Self { active: true })
    }

    fn leave(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        // Best-effort restore. Log but don't propagate — we're probably already
        // unwinding and the terminal will be in a bad state regardless.
        let _ = execute!(stdout(), DisableBracketedPaste, LeaveAlternateScreen);
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
        execute!(stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
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

    // Restore the terminal *before* the default panic hook prints. The hook
    // fires while the alternate screen is still active, so without this its
    // message (and any RUST_BACKTRACE) lands on the alternate buffer and is
    // wiped the moment `TerminalGuard::drop` switches back — a panic then
    // looks like a silent exit into a clean shell. The guard's own restore
    // stays in place for non-panic exits; re-running these escape codes on
    // unwind is harmless (they're idempotent).
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(stdout(), DisableBracketedPaste, LeaveAlternateScreen);
        let _ = disable_raw_mode();
        default_hook(info);
    }));

    // Set up terminal *before* we spawn anything that might print to stderr —
    // tracing is configured to write to stderr in main.rs, which is fine in alt
    // screen but the user only sees the TUI surface anyway.
    let mut guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;

    let (tx, mut rx) = mpsc::unbounded_channel::<AppEvent>();

    // Spawn the blocking key reader on its own thread (crossterm::event::read
    // is sync). Lives until the channel is dropped.
    spawn_input_reader(tx.clone(), state.input_suspended.clone());

    // Spawn periodic tick.
    spawn_ticker(tx.clone());

    // Kick off subscriptions load.
    spawn_load_subscriptions(auth.clone(), state.scope_generation, tx.clone());

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

    // Persist config; non-fatal. Demo sessions never write — favorites or a
    // last-resource id from the mock tenant must not leak into the real config.
    if !auth.is_demo() {
        if let Err(e) = crate::config::save(&state.config) {
            tracing::warn!("failed to save config: {e:#}");
        }
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

    // The app now lands directly on a resource list (no subscription gate), so
    // kick off that view's load once up front — loads are otherwise only driven
    // by user actions, which would leave the initial screen stuck on "loading".
    kick_off_loads_for_view(state, auth, tx, /* force */ false);

    loop {
        // Drain every already-queued event before redrawing, and only redraw
        // once the queue is momentarily empty. Terminals translate touchpad
        // scroll into a *burst* of arrow-key presses (mouse capture is off), and
        // momentum keeps emitting them after the finger lifts; redrawing once per
        // event made the backlog drain one slow frame at a time, so scrolling
        // lingered. Collapsing a burst into a single redraw makes it track — and
        // stop with — the finger. `recv()` yields `None` only when all senders
        // have dropped (input thread gone); treat it as an orderly shutdown.
        let event = match rx.try_recv() {
            Ok(ev) => ev,
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                terminal.draw(|f| dispatch_view(f, f.area(), state, theme))?;
                match rx.recv().await {
                    Some(ev) => ev,
                    None => break,
                }
            }
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
        };

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
                maybe_auto_refresh(state, auth, tx, now);
            }
            AppEvent::Resize { .. } => {
                // ratatui re-measures on next draw. Nothing to do.
            }
            AppEvent::Paste(text) => {
                handle_paste(state, &text, auth, tx);
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
                // Guarded env-var editor. While open it owns every key (typing,
                // field switching, and the final confirm) and routes the write
                // spawn, which needs auth/tx — so it sits in the event loop, not
                // a view handler. Sits below quit/auth so those overlays win.
                if state.env_var_edit.is_some() {
                    handle_env_var_edit_key(state, key, auth, tx);
                    continue;
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
                // When one of the text inputs (list filters, search boxes, the
                // subscription picker) has focus, forward the raw keystroke
                // into its `tui_input::Input` widget instead of dispatching an
                // action. Shared with `handle_paste`, which feeds pasted text
                // through the same focus rules one character at a time.
                if forward_to_focused_text_input(state, key) {
                    continue;
                }
                let action = decide_action(&mut input, key, state);
                if action != Action::Noop {
                    apply_action(action, state, auth, tx);
                }
                drain_fetch_more_requested(state, auth, tx);
            }
            AppEvent::SubscriptionsLoaded { scope, result } => {
                // A fetch issued under a previous scope (the user re-logged-in
                // while it was in flight) describes the old identity; drop it
                // wholesale. The login path spawned a fresh fetch that owns the
                // loading flag.
                if scope != state.scope_generation {
                    continue;
                }
                state.loading_subscriptions = false;
                match result {
                    Ok(subs) => {
                        let was_empty = subs.is_empty();
                        state.subscriptions = subs;
                        // Restore last-used subscription cursor if possible.
                        if let Some(last) = state.selected_subscription.clone() {
                            if let Some(idx) = state.subscriptions.iter().position(|s| s.id == last)
                            {
                                // Row 0 is the synthetic "All subscriptions" scope,
                                // so the matching subscription renders at idx + 1.
                                state.subscription_cursor = idx + 1;
                            }
                        }
                        // No subs visible to this credential is almost always
                        // an auth/tenant problem — surface the login modal.
                        // (Checked regardless of view now that the app lands on
                        // the resource list rather than the subscription gate.)
                        if was_empty && state.auth_prompt == AuthPrompt::Hidden {
                            open_auth_prompt(state, None);
                        } else {
                            // Subscriptions just (re)loaded — most notably after
                            // an in-app `az login`, which only re-spawns *this*
                            // load. Drive the current view's data now that the
                            // sub list is known: the up-front kick at startup
                            // ran before this list existed (a no-op for the
                            // "all subscriptions" case), and the post-login path
                            // never re-kicks resources on its own, leaving the
                            // list empty until a manual `r`. The debounce in
                            // `kick_off_loads_for_view` keeps this a no-op when
                            // resources are already present or still loading.
                            kick_off_loads_for_view(state, auth, tx, /* force */ false);
                        }
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        state.set_status(format!("subscriptions: {msg}"));
                        // Same treatment for outright failures: the chain may
                        // simply have no usable credential.
                        if state.auth_prompt == AuthPrompt::Hidden {
                            open_auth_prompt(state, Some(msg));
                        }
                    }
                }
            }
            AppEvent::ResourcesLoaded { scope, result } => {
                // Stale scope: the user switched subscription (or re-logged-in)
                // after this fetch was issued. Its rows belong to the previous
                // scope — applying them would display the wrong subscription's
                // resources under the new title. Drop wholesale; the loading
                // flag was already reset when the scope changed.
                if scope != state.scope_generation {
                    continue;
                }
                state.loading_resources = false;
                match result {
                    Ok(rs) => {
                        // `list_cursor` indexes into `filtered_resources()`, not
                        // the full `resources` vec, so the cursor restore/clamp
                        // below must work in filtered-index space — otherwise an
                        // active filter (where the two spaces diverge) leaves the
                        // cursor pointing past the visible rows. Capture the row
                        // the user was actually on before swapping in the reload.
                        let anchor_id = state
                            .selected_resource()
                            .map(|r| r.id.clone())
                            .or_else(|| state.config.last_resource_id.clone());
                        state.resources = rs;
                        // Stamp the load time for the "updated Xs ago" indicator
                        // and to re-arm the auto-refresh interval (see
                        // `maybe_auto_refresh`).
                        state.resources_loaded_at = Some(Instant::now());
                        // Restore the cursor to the previously-selected resource
                        // if it survived the reload (and the active filter);
                        // otherwise clamp to the filtered list so a 60s
                        // autorefresh under a filter can't strand the cursor
                        // below the last visible row.
                        state.restore_list_cursor(anchor_id.as_deref());
                        // Per-row badges derive from the health fetch (availability
                        // + a fixed-24h Errors+Traffic window). Chart metrics are
                        // fetched lazily on detail entry, not eagerly per row.
                        spawn_missing_list_health(state, auth, tx, /* force */ false);
                        spawn_missing_container_app_overview(
                            state, auth, tx, /* force */ false,
                        );
                        spawn_missing_function_app_image(state, auth, tx, /* force */ false);
                    }
                    Err(e) => state.set_status(format!("resources: {e}")),
                }
            }
            AppEvent::MetricsLoaded {
                resource_id,
                range,
                result,
            } => {
                state.metrics.pending.remove(&resource_id);
                state.metrics.loading = !state.metrics.pending.is_empty();
                // Stale window: the user switched the chart range while this
                // fetch was in flight. The pending bookkeeping above still
                // applies (the fetch IS finished), but its series must not be
                // rendered under the new range's axis — the force-respawned
                // fetch for the current range owns the cache entry.
                if range != state.metrics.range {
                    continue;
                }
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
                generation,
                result,
            } => {
                // Stale-fetch guard: the user changed the filter / window / context
                // after this fetch was issued, so its result no longer describes
                // what's on screen. Drop it without touching the buffer or the
                // loading flags — the fetch for the *current* generation owns those.
                if generation != state.logs.generation {
                    // Intentionally nothing: the in-flight current-generation fetch
                    // still owns the loading flags and will populate the buffer.
                } else {
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
                            // Workspace ARM id (Container Apps, first page only) —
                            // the scope `o` uses for the portal Logs deep-link.
                            // Cache it; its absence on later pages must not evict a
                            // known value.
                            if let Some(ws) = page.workspace_arm_id {
                                state.logs.workspace_ids.insert(resource_id.clone(), ws);
                            }
                            if append {
                                let entry = state
                                    .logs
                                    .by_resource
                                    .entry(resource_id.clone())
                                    .or_default();
                                entry.extend(page.lines);
                            } else {
                                state
                                    .logs
                                    .by_resource
                                    .insert(resource_id.clone(), page.lines);
                            }
                            // Every landed page is a moment a pending anchor or an
                            // armed error hunt can make progress — including appends,
                            // which is how both chase rows the first page stopped
                            // short of. Ordinary user-scrolled appends carry neither,
                            // so their cursor never jumps.
                            resolve_pending_anchor(state, &resource_id);
                            crate::ui::views::logs::advance_error_hunt(state, &resource_id);
                        }
                        Err(e) => {
                            state.logs.last_error = Some(e);
                            // The chain is broken — don't leave the "searching…"
                            // chip up with nothing in flight. The pending anchor is
                            // kept: the next fresh page resolves it as usual.
                            state.logs.error_hunt = false;
                        }
                    }
                    // Anchor/hunt resolution above may have raised
                    // `fetch_more_requested`; the usual drain point runs only on
                    // key events, so chain the next page here.
                    drain_fetch_more_requested(state, auth, tx);
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
            AppEvent::HealthMetricsLoaded {
                resource_id,
                result,
            } => match result {
                Ok(series) => {
                    state.health.metrics_failures.remove(&resource_id);
                    state.health.metrics.insert(resource_id, series);
                }
                Err(e) => {
                    state.health.metrics_failures.insert(resource_id, e);
                }
            },
            AppEvent::ContainerAppOverviewLoaded {
                resource_id,
                result,
            } => {
                state.container_app_overview.pending.remove(&resource_id);
                if let Ok(limits) = result {
                    state
                        .container_app_overview
                        .by_resource
                        .insert(resource_id, limits);
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
                    // Kick off the per-replica live-status fetch as soon as we
                    // know the active revision name — that's the only place
                    // it's available, and the replicas endpoint needs it in
                    // the path. Skip if already in-flight so refresh paths
                    // (which re-fire this event) don't stack duplicates.
                    let revision_name = meta.name.clone();
                    let needs_spawn = !revision_name.is_empty()
                        && !state.replica_instances.pending.contains(&resource_id);
                    state
                        .revision_meta
                        .by_resource
                        .insert(resource_id.clone(), meta);
                    if needs_spawn {
                        state.replica_instances.pending.insert(resource_id.clone());
                        spawn_load_container_app_replicas(
                            auth.clone(),
                            resource_id,
                            revision_name,
                            tx.clone(),
                        );
                    }
                }
                // Same silent-on-error policy as limits: decorative.
            }
            AppEvent::ContainerAppReplicasLoaded {
                resource_id,
                result,
            } => {
                state.replica_instances.pending.remove(&resource_id);
                match result {
                    Ok(replicas) => {
                        state.replica_instances.failures.remove(&resource_id);
                        state
                            .replica_instances
                            .by_resource
                            .insert(resource_id, replicas);
                    }
                    Err(msg) => {
                        // Leave any previously-cached replicas in place so a
                        // transient error doesn't blank the section; the
                        // failure entry lets the renderer add a hint line.
                        state.replica_instances.failures.insert(resource_id, msg);
                    }
                }
            }
            AppEvent::FunctionAppImageLoaded {
                resource_id,
                result,
            } => {
                state.func_image.pending.remove(&resource_id);
                if let Ok(web) = result {
                    state
                        .func_image
                        .access_restricted
                        .insert(resource_id.clone(), web.access_restricted);
                    state.func_image.by_resource.insert(resource_id, web.image);
                }
                // Silent on error (typically a 403 / config read denial): the
                // VERSION column / network detail just stay blank, same policy as
                // the Container App overview decoration.
            }
            AppEvent::FunctionAppSettingsLoaded {
                resource_id,
                result,
            } => {
                state.func_settings.pending.remove(&resource_id);
                match result {
                    Ok(vars) => {
                        state.func_settings.failures.remove(&resource_id);
                        state.func_settings.by_resource.insert(resource_id, vars);
                    }
                    // Keep the error (typically 403): the detail view turns it
                    // into a "needs config/list permission" hint.
                    Err(e) => {
                        state.func_settings.by_resource.remove(&resource_id);
                        state.func_settings.failures.insert(resource_id, e);
                    }
                }
            }
            AppEvent::FunctionAppTriggersLoaded {
                resource_id,
                result,
            } => {
                state.func_triggers.pending.remove(&resource_id);
                match result {
                    // Empty is a valid result (no functions synced); cache it so
                    // the block collapses cleanly rather than spinning forever.
                    Ok(triggers) => {
                        state.func_triggers.failures.remove(&resource_id);
                        state
                            .func_triggers
                            .by_resource
                            .insert(resource_id, triggers);
                    }
                    // Keep the error so the detail view can show a hint; drop any
                    // stale cache so a failed refresh doesn't read as success.
                    Err(e) => {
                        state.func_triggers.by_resource.remove(&resource_id);
                        state.func_triggers.failures.insert(resource_id, e);
                    }
                }
            }
            AppEvent::PrincipalResolved { object_id, result } => {
                state.principals.pending.remove(&object_id);
                match result {
                    Ok(Some(name)) => {
                        state.principals.failed.remove(&object_id);
                        state.principals.by_id.insert(object_id, name);
                    }
                    // Couldn't resolve (no name, or 403/404): remember the miss
                    // so we don't keep hammering Graph; the UI shows the GUID.
                    Ok(None) | Err(_) => {
                        state.principals.failed.insert(object_id);
                    }
                }
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
            AppEvent::StorageAccountsLoaded { scope, result } => {
                // Stale scope — see the `ResourcesLoaded` arm.
                if scope != state.scope_generation {
                    continue;
                }
                state.storage.accounts_pending = false;
                match result {
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
            AppEvent::RegistriesLoaded { scope, result } => {
                // Stale scope — see the `ResourcesLoaded` arm.
                if scope != state.scope_generation {
                    continue;
                }
                state.registry.registries_pending = false;
                match result {
                    Ok(rows) => {
                        state.registry.registries_error = None;
                        if !rows.is_empty() && state.registry.registries_cursor >= rows.len() {
                            state.registry.registries_cursor = rows.len() - 1;
                        }
                        state.registry.registries = Some(rows);
                    }
                    Err(e) => {
                        state.registry.registries = None;
                        state.registry.registries_error = Some(e);
                    }
                }
            }
            AppEvent::RegistryRepositoriesLoaded {
                registry_id,
                result,
            } => {
                state.registry.repositories_pending.remove(&registry_id);
                match result {
                    Ok(rows) => {
                        state.registry.repositories_error.remove(&registry_id);
                        state.registry.repositories.insert(registry_id, rows);
                    }
                    Err(e) => {
                        state.registry.repositories.remove(&registry_id);
                        state.registry.repositories_error.insert(registry_id, e);
                    }
                }
            }
            AppEvent::RegistryTagsLoaded { key, result } => {
                state.registry.tags_pending.remove(&key);
                match result {
                    Ok(rows) => {
                        state.registry.tags_error.remove(&key);
                        state.registry.tags.insert(key, rows);
                    }
                    Err(e) => {
                        state.registry.tags.remove(&key);
                        state.registry.tags_error.insert(key, e);
                    }
                }
            }
            AppEvent::CosmosAccountsLoaded { scope, result } => {
                // Stale scope — see the `ResourcesLoaded` arm.
                if scope != state.scope_generation {
                    continue;
                }
                state.cosmos.accounts_pending = false;
                match result {
                    Ok(rows) => {
                        state.cosmos.accounts_error = None;
                        if !rows.is_empty() && state.cosmos.accounts_cursor >= rows.len() {
                            state.cosmos.accounts_cursor = rows.len() - 1;
                        }
                        state.cosmos.accounts = Some(rows);
                    }
                    Err(e) => {
                        state.cosmos.accounts = None;
                        state.cosmos.accounts_error = Some(e);
                    }
                }
            }
            AppEvent::CosmosDatabasesLoaded { account_id, result } => {
                state.cosmos.databases_pending.remove(&account_id);
                match result {
                    Ok(rows) => {
                        state.cosmos.databases_error.remove(&account_id);
                        state.cosmos.databases.insert(account_id, rows);
                    }
                    Err(e) => {
                        state.cosmos.databases.remove(&account_id);
                        state.cosmos.databases_error.insert(account_id, e);
                    }
                }
            }
            AppEvent::CosmosContainersLoaded { key, result } => {
                state.cosmos.containers_pending.remove(&key);
                match result {
                    Ok(rows) => {
                        state.cosmos.containers_error.remove(&key);
                        state.cosmos.containers.insert(key, rows);
                    }
                    Err(e) => {
                        state.cosmos.containers.remove(&key);
                        state.cosmos.containers_error.insert(key, e);
                    }
                }
            }
            AppEvent::CosmosItemsLoaded { key, result } => {
                state.cosmos.items_pending.remove(&key);
                match result {
                    Ok(preview) => {
                        state.cosmos.items_error.remove(&key);
                        state.cosmos.items.insert(key, preview);
                    }
                    Err(e) => {
                        state.cosmos.items.remove(&key);
                        state.cosmos.items_error.insert(key, e);
                    }
                }
            }
            AppEvent::SqlResourcesLoaded { scope, result } => {
                // Stale scope — see the `ResourcesLoaded` arm.
                if scope != state.scope_generation {
                    continue;
                }
                state.sql.pending = false;
                match result {
                    Ok(rows) => {
                        state.sql.error = None;
                        if !rows.is_empty() && state.sql.cursor >= rows.len() {
                            state.sql.cursor = rows.len() - 1;
                        }
                        state.sql.resources = Some(rows);
                    }
                    Err(e) => {
                        state.sql.resources = None;
                        state.sql.error = Some(e);
                    }
                }
            }
            AppEvent::SqlMetricsLoaded {
                resource_id,
                range,
                result,
            } => {
                state.sql.metrics_pending.remove(&resource_id);
                // Same stale-window rule as `MetricsLoaded` above.
                if range != state.sql.metrics_range {
                    continue;
                }
                match result {
                    Ok(r) => {
                        state.sql.metrics_failures.remove(&resource_id);
                        if r.missing.is_empty() {
                            state.sql.metrics_missing.remove(&resource_id);
                        } else {
                            state
                                .sql
                                .metrics_missing
                                .insert(resource_id.clone(), r.missing);
                        }
                        state.sql.metrics.insert(resource_id, r.series);
                    }
                    Err(e) => {
                        state.sql.metrics.remove(&resource_id);
                        state.sql.metrics_missing.remove(&resource_id);
                        state.sql.metrics_failures.insert(resource_id, e);
                    }
                }
            }
            AppEvent::KeyVaultsLoaded { scope, result } => {
                // Stale scope — see the `ResourcesLoaded` arm.
                if scope != state.scope_generation {
                    continue;
                }
                state.key_vault.vaults_pending = false;
                match result {
                    Ok(rows) => {
                        state.key_vault.vaults_error = None;
                        if !rows.is_empty() && state.key_vault.vaults_cursor >= rows.len() {
                            state.key_vault.vaults_cursor = rows.len() - 1;
                        }
                        state.key_vault.vaults = Some(rows);
                    }
                    Err(e) => {
                        state.key_vault.vaults = None;
                        state.key_vault.vaults_error = Some(e);
                    }
                }
            }
            AppEvent::KeyVaultItemsLoaded { key, result } => {
                state.key_vault.items_pending.remove(&key);
                match result {
                    Ok(rows) => {
                        state.key_vault.items_error.remove(&key);
                        state.key_vault.items.insert(key, rows);
                    }
                    Err(e) => {
                        state.key_vault.items.remove(&key);
                        state.key_vault.items_error.insert(key, e);
                    }
                }
            }
            AppEvent::KeyVaultAccessLoaded { generation, result } => {
                // A page fetched under an older query scope (window / item /
                // exclude-me changed while it was in flight) no longer
                // describes what the header claims — drop it; the fetch for
                // the current generation owns the pending flag.
                if generation == state.key_vault.access_generation {
                    state.key_vault.access_pending = false;
                    match result {
                        Ok(page) => {
                            state.key_vault.access_error = None;
                            state.key_vault.access_truncated = page.truncated;
                            state.key_vault.access_hidden = page.hidden;
                            state.key_vault.access_events = Some(page.events);
                            state.key_vault.access_cursor = 0;
                            state.key_vault.access_view_top.set(0);
                        }
                        Err(e) => state.key_vault.access_error = Some(e),
                    }
                }
            }
            AppEvent::SqlAuditPrincipalsLoaded { generation, result } => {
                // Same stale-scope rule as `KeyVaultAccessLoaded`.
                if generation == state.sql.audit.generation {
                    state.sql.audit.pending = false;
                    match result {
                        Ok(page) => {
                            // GUID-shaped principals (client ids, S-1-9-3
                            // SIDs) resolve to directory names via Graph —
                            // best-effort, cached, deduped like the Detail
                            // view's author resolution.
                            for p in &page.principals {
                                let Some(id) =
                                    crate::azure::sql_audit::graph_candidate(&p.principal)
                                else {
                                    continue;
                                };
                                if state.principals.by_id.contains_key(&id)
                                    || state.principals.failed.contains(&id)
                                    || state.principals.pending.contains(&id)
                                {
                                    continue;
                                }
                                state.principals.pending.insert(id.clone());
                                spawn_resolve_principal(auth.clone(), id, tx.clone());
                            }
                            state.sql.audit.error = None;
                            state.sql.audit.principals_truncated = page.truncated;
                            state.sql.audit.principals = Some(page.principals);
                            state.sql.audit.cursor = 0;
                            state.sql.audit.view_top.set(0);
                        }
                        Err(e) => state.sql.audit.error = Some(e),
                    }
                }
            }
            AppEvent::SqlAuditEventsLoaded {
                generation,
                append,
                result,
            } => {
                if generation == state.sql.audit.events_generation {
                    state.sql.audit.events_pending = false;
                    state.sql.audit.events_loading_more = false;
                    match result {
                        Ok(page) => {
                            state.sql.audit.events_error = None;
                            state.sql.audit.events_truncated = page.truncated;
                            if append {
                                // Older-than page: extend, keep the cursor
                                // where the user was scrolling.
                                state
                                    .sql
                                    .audit
                                    .events
                                    .get_or_insert_with(Vec::new)
                                    .extend(page.events);
                            } else {
                                state.sql.audit.events = Some(page.events);
                                state.sql.audit.events_cursor = 0;
                                state.sql.audit.events_view_top.set(0);
                            }
                        }
                        Err(e) => state.sql.audit.events_error = Some(e),
                    }
                }
            }
            AppEvent::SqlAuditDbUsersLoaded { generation, result } => {
                if generation == state.sql.audit.db_users_generation {
                    state.sql.audit.db_users_pending = false;
                    match result {
                        Ok(users) => {
                            state.sql.audit.db_users_note = None;
                            state.sql.audit.db_users = Some(users);
                        }
                        // Best-effort: a T-SQL failure demotes to a note; the
                        // audit roll-up itself is unaffected.
                        Err(e) => {
                            state.sql.audit.db_users = None;
                            // First line only — the friendly errors are
                            // multi-line and the note is a single row.
                            let short = e.lines().next().unwrap_or(&e).to_string();
                            state.sql.audit.db_users_note = Some(short);
                        }
                    }
                }
            }
            AppEvent::SqlSessionsLoaded { generation, result } => {
                if generation == state.sql.sessions.generation {
                    state.sql.sessions.pending = false;
                    match result {
                        Ok(rows) => {
                            state.sql.sessions.error = None;
                            state.sql.sessions.rows = Some(rows);
                            state.sql.sessions.cursor = 0;
                            state.sql.sessions.view_top.set(0);
                        }
                        Err(e) => state.sql.sessions.error = Some(e),
                    }
                }
            }
            AppEvent::KeyVaultSecretValueLoaded {
                vault_id,
                name,
                result,
            } => {
                // Only apply if the modal is still open for this exact secret;
                // otherwise the user closed it or reopened on another row and a
                // stale value must not leak into the wrong modal.
                if let Some(modal) = state.key_vault.secret_modal.as_mut() {
                    if modal.vault_id == vault_id && modal.name == name {
                        modal.status = match result {
                            Ok(value) => crate::ui::state::SecretRevealStatus::Loaded(value),
                            Err(e) => crate::ui::state::SecretRevealStatus::Error(e),
                        };
                    }
                }
            }
            AppEvent::ServiceBusNamespacesLoaded { scope, result } => {
                // Stale scope — see the `ResourcesLoaded` arm.
                if scope != state.scope_generation {
                    continue;
                }
                state.service_bus.namespaces_pending = false;
                match result {
                    Ok(rows) => {
                        state.service_bus.namespaces_error = None;
                        if !rows.is_empty() && state.service_bus.namespaces_cursor >= rows.len() {
                            state.service_bus.namespaces_cursor = rows.len() - 1;
                        }
                        state.service_bus.namespaces = Some(rows);
                    }
                    Err(e) => {
                        state.service_bus.namespaces = None;
                        state.service_bus.namespaces_error = Some(e);
                    }
                }
            }
            AppEvent::ServiceBusQueuesLoaded {
                namespace_id,
                result,
            } => {
                state.service_bus.queues_pending.remove(&namespace_id);
                match result {
                    Ok(rows) => {
                        state.service_bus.queues_error.remove(&namespace_id);
                        state.service_bus.queues.insert(namespace_id, rows);
                    }
                    Err(e) => {
                        state.service_bus.queues.remove(&namespace_id);
                        state.service_bus.queues_error.insert(namespace_id, e);
                    }
                }
            }
            AppEvent::ServiceBusTopicsLoaded {
                namespace_id,
                result,
            } => {
                state.service_bus.topics_pending.remove(&namespace_id);
                match result {
                    Ok(rows) => {
                        state.service_bus.topics_error.remove(&namespace_id);
                        state.service_bus.topics.insert(namespace_id, rows);
                    }
                    Err(e) => {
                        state.service_bus.topics.remove(&namespace_id);
                        state.service_bus.topics_error.insert(namespace_id, e);
                    }
                }
            }
            AppEvent::ServiceBusSubscriptionsLoaded { key, result } => {
                state.service_bus.subscriptions_pending.remove(&key);
                match result {
                    Ok(rows) => {
                        state.service_bus.subscriptions_error.remove(&key);
                        state.service_bus.subscriptions.insert(key, rows);
                    }
                    Err(e) => {
                        state.service_bus.subscriptions.remove(&key);
                        state.service_bus.subscriptions_error.insert(key, e);
                    }
                }
            }
            AppEvent::EnvVarWriteCompleted {
                applied,
                is_demo,
                result,
            } => match result {
                Ok(()) => {
                    // Optimistic: show the new value at once, close the modal,
                    // then confirm against the server (skipped in the demo, where
                    // a refetch would just wipe the simulated edit).
                    apply_env_edit_to_cache(state, &applied);
                    state.env_var_edit = None;
                    let short = applied.resource_id.rsplit('/').next().unwrap_or("resource");
                    state.set_status(format!("wrote {} on {short}", applied.name));
                    if !is_demo {
                        refetch_env_after_write(state, &applied, auth, tx);
                    }
                }
                Err(e) => {
                    // Keep the modal up on the confirm step so the user can read
                    // the error and retry or cancel; nothing was committed.
                    if let Some(edit) = state.env_var_edit.as_mut() {
                        edit.in_flight = false;
                        edit.phase = EnvVarEditPhase::Confirming;
                        edit.error = Some(e);
                    } else {
                        state.set_status(format!("env-var write failed: {e}"));
                    }
                }
            },
        }

        // Drain a pending login request: the modal handler set it on Enter,
        // and now we own the terminal so we can suspend safely.
        if let Some(req) = state.pending_login.take() {
            run_pending_login(terminal, guard, state, auth, tx, req).await;
        }

        // Drain a pending container shell (`s`): same terminal-ownership reason
        // as login — we suspend the TUI, hand the terminal to `az`, then resume.
        if let Some(req) = state.pending_exec.take() {
            run_pending_exec(terminal, guard, state, req).await;
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
    // Park the input-reader thread before handing the terminal over — same
    // reasoning as `run_pending_exec`: recent `az login` versions prompt
    // interactively (subscription/tenant selection) on stdin, and a reader
    // still polling the terminal would steal those keystrokes (replaying them
    // as TUI actions on resume) or get the process SIGTTIN-stopped once az
    // becomes the foreground process group. The brief wait lets the reader
    // finish any in-flight `poll` and park.
    state.input_suspended.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(120)).await;

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
    // Resume input only after the terminal is ours again.
    state.input_suspended.store(false, Ordering::SeqCst);

    match outcome {
        Ok(()) => {
            state.auth_prompt = AuthPrompt::Hidden;
            state.auth_last_error = None;
            // The previous user's bearer is now stale; drop it before we
            // refetch subscriptions or any other ARM call would still go out
            // under the old identity.
            auth.clear_cache().await;
            // New identity ⇒ new scope: orphan every in-flight fetch (their
            // results describe the old identity) and drop every cache — the
            // old identity's resources, metrics, and secret-bearing settings
            // must not stay visible or yankable under the new login.
            state.scope_generation = state.scope_generation.wrapping_add(1);
            state.flush_identity_caches();
            state.loading_subscriptions = true;
            state.subscriptions.clear();
            spawn_load_subscriptions(auth.clone(), state.scope_generation, tx.clone());
            state.set_status("logged in via az");
        }
        Err(e) => {
            // Stay on the menu so the user can retry / pick a different mode.
            state.auth_prompt = AuthPrompt::Menu;
            state.auth_last_error = Some(format!("{e}"));
        }
    }
}

/// Suspend the TUI, run `az containerapp exec` for an interactive shell, then
/// restore the TUI. Mirrors [`run_pending_login`]. Errors are surfaced as a
/// status hint; az's own stderr is already visible inline since stdio is
/// inherited during the shell.
async fn run_pending_exec(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    guard: &mut TerminalGuard,
    state: &mut AppState,
    req: PendingExec,
) {
    // Park the input-reader thread first so it stops reading the terminal before
    // we hand it to the child — otherwise it races `az` for keystrokes and, once
    // the child becomes the foreground process group, would SIGTTIN-stop azpect.
    // The brief wait lets the reader finish any in-flight `poll` and park.
    state.input_suspended.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(120)).await;

    guard.suspend();

    // Banner on the parent shell so the user knows what's launching and how to
    // get back (the shell's own `exit` / Ctrl-D returns to the TUI).
    {
        use std::io::Write as _;
        let mut out = stdout();
        let mut hint = format!("\nazpect: launching shell in {}", req.name);
        if let Some(c) = req.container.as_deref() {
            hint.push_str(&format!(" · container {c}"));
        }
        if let Some(r) = req.replica.as_deref() {
            hint.push_str(&format!(" · replica {r}"));
        }
        hint.push_str("\n(exit / Ctrl-D to return to azpect)\n\n");
        let _ = out.write_all(hint.as_bytes());
        let _ = out.flush();
    }

    let opts = AzExecOptions {
        name: req.name,
        resource_group: req.resource_group,
        subscription: req.subscription,
        revision: req.revision,
        replica: req.replica,
        container: req.container,
        command: "/bin/sh".to_string(),
    };
    let outcome = az_exec::run(opts).await;

    // Always try to restore the TUI — the user is sitting in a bare shell and
    // expects the app back regardless of how the session ended.
    if let Err(e) = guard.resume() {
        tracing::error!("failed to resume terminal after container shell: {e}");
        state.should_quit = true;
        return;
    }
    let _ = terminal.clear();
    // Resume input only after the terminal is ours again.
    state.input_suspended.store(false, Ordering::SeqCst);

    match outcome {
        Ok(()) => state.set_status("shell session ended"),
        Err(e) => state.set_status(format!("shell: {e}")),
    }
}

/// Route `key` into whichever text input currently has focus, if any. One
/// branch per input, each mirroring the old inline carve-outs in the event
/// loop: forward the keystroke into the `tui_input::Input` widget and, for
/// filters, reset the matching cursor so a shrinking match list never points
/// past its end. Returns `true` when the key was consumed by an input.
/// Reused by `handle_paste` so pasted text follows the exact same focus rules.
fn forward_to_focused_text_input(state: &mut AppState, key: crossterm::event::KeyEvent) -> bool {
    if should_forward_to_filter(state, key) {
        state.list_filter.handle_event(&CtEvent::Key(key));
        state.list_cursor = 0;
        return true;
    }
    // The logs less-style search input is the one carve-out with no cursor to
    // reset — search steers the scroll position on Enter, not a filtered list.
    if should_forward_to_logs_search(state, key) {
        state.logs.search_input.handle_event(&CtEvent::Key(key));
        return true;
    }
    // Same shape for the KV access-logs custom window ("6m", "1y"): a plain
    // value input, no list cursor attached.
    if should_forward_to_access_window_input(state, key) {
        state
            .key_vault
            .access_window_input
            .handle_event(&CtEvent::Key(key));
        return true;
    }
    // The SQL audit views' custom window input (`t`), same shape.
    if should_forward_to_sql_audit_window_input(state, key) {
        state
            .sql
            .audit
            .window_input
            .handle_event(&CtEvent::Key(key));
        return true;
    }
    if should_forward_to_blobs_filter(state, key) {
        state.storage.blobs_filter.handle_event(&CtEvent::Key(key));
        state.storage.blobs_cursor = 0;
        return true;
    }
    if should_forward_to_containers_filter(state, key) {
        state
            .storage
            .containers_filter
            .handle_event(&CtEvent::Key(key));
        state.storage.containers_cursor = 0;
        return true;
    }
    if should_forward_to_accounts_filter(state, key) {
        state
            .storage
            .accounts_filter
            .handle_event(&CtEvent::Key(key));
        state.storage.accounts_cursor = 0;
        return true;
    }
    if should_forward_to_registries_filter(state, key) {
        state
            .registry
            .registries_filter
            .handle_event(&CtEvent::Key(key));
        state.registry.registries_cursor = 0;
        return true;
    }
    if should_forward_to_repositories_filter(state, key) {
        state
            .registry
            .repositories_filter
            .handle_event(&CtEvent::Key(key));
        state.registry.repositories_cursor = 0;
        return true;
    }
    if should_forward_to_tags_filter(state, key) {
        state.registry.tags_filter.handle_event(&CtEvent::Key(key));
        state.registry.tags_cursor = 0;
        return true;
    }
    if should_forward_to_cosmos_accounts_filter(state, key) {
        state
            .cosmos
            .accounts_filter
            .handle_event(&CtEvent::Key(key));
        state.cosmos.accounts_cursor = 0;
        return true;
    }
    if should_forward_to_cosmos_databases_filter(state, key) {
        state
            .cosmos
            .databases_filter
            .handle_event(&CtEvent::Key(key));
        state.cosmos.databases_cursor = 0;
        return true;
    }
    if should_forward_to_cosmos_containers_filter(state, key) {
        state
            .cosmos
            .containers_filter
            .handle_event(&CtEvent::Key(key));
        state.cosmos.containers_cursor = 0;
        return true;
    }
    if should_forward_to_sql_filter(state, key) {
        state.sql.filter.handle_event(&CtEvent::Key(key));
        state.sql.cursor = 0;
        return true;
    }
    if should_forward_to_sql_audit_filter(state, key) {
        state
            .sql
            .audit
            .principals_filter
            .handle_event(&CtEvent::Key(key));
        state.sql.audit.cursor = 0;
        return true;
    }
    if should_forward_to_key_vaults_filter(state, key) {
        state
            .key_vault
            .vaults_filter
            .handle_event(&CtEvent::Key(key));
        state.key_vault.vaults_cursor = 0;
        return true;
    }
    if should_forward_to_key_vault_items_filter(state, key) {
        state
            .key_vault
            .items_filter
            .handle_event(&CtEvent::Key(key));
        state.key_vault.items_cursor = 0;
        return true;
    }
    if should_forward_to_sb_namespaces_filter(state, key) {
        state
            .service_bus
            .namespaces_filter
            .handle_event(&CtEvent::Key(key));
        state.service_bus.namespaces_cursor = 0;
        return true;
    }
    if should_forward_to_sb_entities_filter(state, key) {
        state
            .service_bus
            .entities_filter
            .handle_event(&CtEvent::Key(key));
        state.service_bus.entities_cursor = 0;
        return true;
    }
    if should_forward_to_sb_subscriptions_filter(state, key) {
        state
            .service_bus
            .subscriptions_filter
            .handle_event(&CtEvent::Key(key));
        state.service_bus.subscriptions_cursor = 0;
        return true;
    }
    // Subscription picker `/`-search box.
    if should_forward_to_subscriptions_filter(state, key) {
        state.subscription_filter.handle_event(&CtEvent::Key(key));
        state.subscription_cursor = 0;
        return true;
    }
    if should_forward_to_apim_apis_filter(state, key) {
        state.apim.apis_filter.handle_event(&CtEvent::Key(key));
        state.apim.apis_cursor = 0;
        return true;
    }
    if should_forward_to_apim_operations_filter(state, key) {
        state
            .apim
            .operations_filter
            .handle_event(&CtEvent::Key(key));
        state.apim.operations_cursor = 0;
        return true;
    }
    false
}

/// Feed a bracketed paste into the focused text input, one synthesized
/// keystroke per character. Anywhere else the paste is swallowed whole —
/// dispatching pasted characters as keybindings is exactly the failure
/// bracketed paste exists to prevent. Modals are stricter still: their
/// non-typing phases interpret plain characters as commit/cancel shortcuts
/// (`y` confirms an env-var write, menu keys start a login), so pasting is
/// only allowed where the modal is actually reading text.
fn handle_paste(
    state: &mut AppState,
    text: &str,
    auth: &AzureAuth,
    tx: &UnboundedSender<AppEvent>,
) {
    // None of the inputs are multi-line and control characters would confuse
    // `tui_input`; strip rather than reject so pasting a value with a trailing
    // newline (the common copy-from-terminal shape) still works.
    let clean: String = text.chars().filter(|c| !c.is_control()).collect();
    if clean.is_empty() {
        return;
    }
    // Auth modal: only the tenant-id capture step reads text; the menu treats
    // characters as shortcuts.
    if state.auth_prompt != AuthPrompt::Hidden {
        if state.auth_prompt == AuthPrompt::TenantInput {
            for key in synthesized_keys(&clean) {
                handle_auth_prompt_key(state, key);
            }
        }
        return;
    }
    if state.quit_confirm {
        return;
    }
    // Env-var editor: typing only lands in the fields during `Editing` (and
    // never while a write is in flight); `Confirming` reads `y`/`n`.
    if let Some(edit) = state.env_var_edit.as_ref() {
        if edit.phase == EnvVarEditPhase::Editing && !edit.in_flight {
            for key in synthesized_keys(&clean) {
                handle_env_var_edit_key(state, key, auth, tx);
            }
        }
        return;
    }
    if state.command_active {
        state.command_tab_cycle = None;
        for key in synthesized_keys(&clean) {
            if should_forward_to_command(state, key) {
                state.command_input.handle_event(&CtEvent::Key(key));
            }
        }
        return;
    }
    for key in synthesized_keys(&clean) {
        if !forward_to_focused_text_input(state, key) {
            // No input focused: drop the paste. (Checked per character only
            // because focus never changes while typing plain chars; bailing on
            // the first miss keeps a stray paste from doing anything at all.)
            return;
        }
    }
}

/// Plain no-modifier key presses for each character of `s` — the shape every
/// input-forwarding helper already accepts, so pasted text can reuse the
/// keystroke path instead of each input growing a paste API.
fn synthesized_keys(s: &str) -> impl Iterator<Item = crossterm::event::KeyEvent> + '_ {
    s.chars().map(|ch| {
        crossterm::event::KeyEvent::new(KeyCode::Char(ch), crossterm::event::KeyModifiers::NONE)
    })
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

/// Mirror of `should_forward_to_filter` for the subscription picker's `/`-search
/// box. Only forwards while the picker has its filter focused; same Esc / Enter /
/// arrow carve-outs so cancel / commit / nav still reach the dispatcher.
fn should_forward_to_subscriptions_filter(
    state: &AppState,
    key: crossterm::event::KeyEvent,
) -> bool {
    state.view == View::Subscriptions
        && state.subscription_filter_active
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

/// Mirror of `should_forward_to_filter` for the APIM APIs name filter. Only
/// forwards while the apim-apis view has its filter box focused; same Esc /
/// Enter / arrow carve-outs so cancel / commit / nav still reach the dispatcher.
fn should_forward_to_apim_apis_filter(state: &AppState, key: crossterm::event::KeyEvent) -> bool {
    state.view == View::ApimApis
        && state.apim.apis_filter_active
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

/// Mirror of `should_forward_to_apim_apis_filter` for the APIM operations
/// (routes) name filter.
fn should_forward_to_apim_operations_filter(
    state: &AppState,
    key: crossterm::event::KeyEvent,
) -> bool {
    state.view == View::ApimOperations
        && state.apim.operations_filter_active
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

/// Mirror of `should_forward_to_filter` for the registries-list filter.
fn should_forward_to_registries_filter(state: &AppState, key: crossterm::event::KeyEvent) -> bool {
    state.view == View::Registries
        && state.registry.registries_filter_active
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

/// Mirror of `should_forward_to_filter` for the registry-repositories filter.
fn should_forward_to_repositories_filter(
    state: &AppState,
    key: crossterm::event::KeyEvent,
) -> bool {
    state.view == View::RegistryRepositories
        && state.registry.repositories_filter_active
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

/// Mirror of `should_forward_to_filter` for the registry-tags filter.
fn should_forward_to_tags_filter(state: &AppState, key: crossterm::event::KeyEvent) -> bool {
    state.view == View::RegistryTags
        && state.registry.tags_filter_active
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

/// Mirror of `should_forward_to_filter` for the cosmos-accounts name filter.
/// Mirror of `should_forward_to_filter` for the flat Azure SQL list filter.
fn should_forward_to_sql_filter(state: &AppState, key: crossterm::event::KeyEvent) -> bool {
    state.view == View::SqlResources
        && state.sql.filter_active
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

fn should_forward_to_sql_audit_filter(state: &AppState, key: crossterm::event::KeyEvent) -> bool {
    state.view == View::SqlAuditPrincipals
        && state.sql.audit.principals_filter_active
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

fn should_forward_to_cosmos_accounts_filter(
    state: &AppState,
    key: crossterm::event::KeyEvent,
) -> bool {
    state.view == View::CosmosAccounts
        && state.cosmos.accounts_filter_active
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

/// Mirror of `should_forward_to_filter` for the cosmos-databases name filter.
fn should_forward_to_cosmos_databases_filter(
    state: &AppState,
    key: crossterm::event::KeyEvent,
) -> bool {
    state.view == View::CosmosDatabases
        && state.cosmos.databases_filter_active
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

/// Mirror of `should_forward_to_filter` for the cosmos-containers name filter.
fn should_forward_to_cosmos_containers_filter(
    state: &AppState,
    key: crossterm::event::KeyEvent,
) -> bool {
    state.view == View::CosmosContainers
        && state.cosmos.containers_filter_active
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

/// Mirror of `should_forward_to_filter` for the key-vault list name filter.
fn should_forward_to_key_vaults_filter(state: &AppState, key: crossterm::event::KeyEvent) -> bool {
    state.view == View::KeyVaults
        && state.key_vault.vaults_filter_active
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

/// Mirror of `should_forward_to_filter` for the key-vault items name filter.
fn should_forward_to_key_vault_items_filter(
    state: &AppState,
    key: crossterm::event::KeyEvent,
) -> bool {
    state.view == View::KeyVaultItems
        && state.key_vault.items_filter_active
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

/// Mirror of `should_forward_to_filter` for the key-vault access-logs custom
/// window input (`t`, e.g. "6m"). Up/Down aren't carved out — the input is a
/// one-line value, not a filter steering a list underneath.
fn should_forward_to_access_window_input(
    state: &AppState,
    key: crossterm::event::KeyEvent,
) -> bool {
    state.view == View::KeyVaultAccessLogs
        && state.key_vault.access_window_input_active
        && !matches!(key.code, KeyCode::Esc | KeyCode::Enter)
}

/// Mirror of `should_forward_to_access_window_input` for the SQL audit views'
/// custom window input.
fn should_forward_to_sql_audit_window_input(
    state: &AppState,
    key: crossterm::event::KeyEvent,
) -> bool {
    matches!(state.view, View::SqlAuditPrincipals | View::SqlAuditEvents)
        && state.sql.audit.window_input_active
        && !matches!(key.code, KeyCode::Esc | KeyCode::Enter)
}

/// Mirror of `should_forward_to_filter` for the service-bus namespaces filter.
fn should_forward_to_sb_namespaces_filter(
    state: &AppState,
    key: crossterm::event::KeyEvent,
) -> bool {
    state.view == View::ServiceBusNamespaces
        && state.service_bus.namespaces_filter_active
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

/// Mirror of `should_forward_to_filter` for the service-bus entities filter
/// (shared across the queues and topics lists).
fn should_forward_to_sb_entities_filter(state: &AppState, key: crossterm::event::KeyEvent) -> bool {
    state.view == View::ServiceBusEntities
        && state.service_bus.entities_filter_active
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

/// Mirror of `should_forward_to_filter` for the service-bus subscriptions filter.
fn should_forward_to_sb_subscriptions_filter(
    state: &AppState,
    key: crossterm::event::KeyEvent,
) -> bool {
    state.view == View::ServiceBusSubscriptions
        && state.service_bus.subscriptions_filter_active
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

/// Palette commands that aren't tied to a [`Category`]. Subscriptions / help
/// / quit / refresh are global so they sit here as a small static table;
/// every other command flows through [`Category::palette_aliases`].
const PALETTE_FIXED_COMMANDS: &[(&str, &[&str])] = &[
    ("subscriptions", &["subs"]),
    ("help", &["h", "?"]),
    ("quit", &["q"]),
    ("refresh", &[]),
];

/// Flattened list of every palette name (category aliases + fixed commands +
/// the legacy vim-style quit aliases). Returned in deterministic order so
/// Tab-completion cycles predictably: categories first (in
/// [`crate::ui::state::Category::ALL`] order), then fixed commands, then the
/// legacy quit aliases.
fn palette_completion_candidates() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for category in crate::ui::state::Category::ALL {
        for alias in category.palette_aliases() {
            out.push((*alias).to_string());
        }
    }
    for (canonical, aliases) in PALETTE_FIXED_COMMANDS {
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
    if trimmed.is_empty() {
        return;
    }

    // `:q` and friends quit immediately and intentionally bypass the
    // quit-confirmation modal: typing `:q` is explicit user intent, not an
    // accidental Esc press.
    if matches!(trimmed, "q" | "quit" | "qa" | "qa!" | "quitall") {
        state.should_quit = true;
        return;
    }

    // Category routing: every `:storage` / `:registries` / `:reg` / `:acr` /
    // `:apis` / `:cosmos` flows through the same `enter_category` helper as
    // the keybinds. Adding a new resource type means adding a `Category`
    // variant — this loop picks it up for free.
    if let Some(category) = crate::ui::state::Category::ALL
        .iter()
        .copied()
        .find(|c| c.palette_aliases().contains(&trimmed))
    {
        crate::ui::state::enter_category(state, category);
        return;
    }

    match trimmed {
        "subscriptions" | "subs" => {
            if state.view != View::Subscriptions {
                state.view = View::Subscriptions;
                state.subscription_filter.reset();
                state.subscription_filter_active = false;
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
        || (state.view == View::Subscriptions && state.subscription_filter_active)
        || (state.view == View::StorageBlobs && state.storage.blobs_filter_active)
        || (state.view == View::StorageContainers && state.storage.containers_filter_active)
        || (state.view == View::StorageAccounts && state.storage.accounts_filter_active)
        || (state.view == View::Registries && state.registry.registries_filter_active)
        || (state.view == View::RegistryRepositories && state.registry.repositories_filter_active)
        || (state.view == View::RegistryTags && state.registry.tags_filter_active)
        || (state.view == View::CosmosAccounts && state.cosmos.accounts_filter_active)
        || (state.view == View::CosmosDatabases && state.cosmos.databases_filter_active)
        || (state.view == View::CosmosContainers && state.cosmos.containers_filter_active)
        || (state.view == View::KeyVaults && state.key_vault.vaults_filter_active)
        || (state.view == View::KeyVaultItems && state.key_vault.items_filter_active)
        || (state.view == View::KeyVaultAccessLogs && state.key_vault.access_window_input_active)
        || (state.view == View::ServiceBusNamespaces && state.service_bus.namespaces_filter_active)
        || (state.view == View::ServiceBusEntities && state.service_bus.entities_filter_active)
        || (state.view == View::ServiceBusSubscriptions
            && state.service_bus.subscriptions_filter_active)
        || (state.view == View::ApimApis && state.apim.apis_filter_active)
        || (state.view == View::ApimOperations && state.apim.operations_filter_active)
        || (state.view == View::SqlResources && state.sql.filter_active)
        || (matches!(state.view, View::SqlAuditPrincipals | View::SqlAuditEvents)
            && state.sql.audit.window_input_active)
        || (state.view == View::SqlAuditPrincipals && state.sql.audit.principals_filter_active);

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
        View::EnvVars => crate::ui::views::env_vars::handle(action, state),
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
        View::Registries => crate::ui::views::registries::handle(action, state),
        View::RegistryRepositories => {
            crate::ui::views::registry_repositories::handle(action, state)
        }
        View::RegistryTags => crate::ui::views::registry_tags::handle(action, state),
        View::CosmosAccounts => crate::ui::views::cosmos_accounts::handle(action, state),
        View::CosmosDatabases => crate::ui::views::cosmos_databases::handle(action, state),
        View::CosmosContainers => crate::ui::views::cosmos_containers::handle(action, state),
        View::CosmosItem => crate::ui::views::cosmos_item::handle(action, state),
        View::KeyVaults => crate::ui::views::key_vaults::handle(action, state),
        View::KeyVaultItems => crate::ui::views::key_vault_items::handle(action, state),
        View::KeyVaultAccessLogs => crate::ui::views::key_vault_access::handle(action, state),
        View::ServiceBusNamespaces => {
            crate::ui::views::service_bus_namespaces::handle(action, state)
        }
        View::ServiceBusEntities => crate::ui::views::service_bus_entities::handle(action, state),
        View::ServiceBusSubscriptions => {
            crate::ui::views::service_bus_subscriptions::handle(action, state)
        }
        View::SqlResources => crate::ui::views::sql_resources::handle(action, state),
        View::SqlDetail => crate::ui::views::sql_detail::handle(action, state),
        View::SqlAuditPrincipals | View::SqlAuditEvents | View::SqlAuditEventDetail => {
            crate::ui::views::sql_audit::handle(action, state)
        }
        View::SqlSessions => crate::ui::views::sql_sessions::handle(action, state),
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
    // Enter / `x` on a Key Vault secret row opens the reveal modal and fetches
    // the value on demand. Routed here (not the view handler) because spawning
    // the fetch needs `auth`/`tx`. Only reached when no modal is already open —
    // the view handler owns the open modal's scroll / yank / close.
    if matches!(action, Action::OpenSelected | Action::DecodeSecret)
        && state.view == View::KeyVaultItems
        && state.key_vault.secret_modal.is_none()
    {
        open_key_vault_secret_modal(state, auth, tx);
        return;
    }
    // Enter on a Key Vault-backed env var jumps to the referenced vault and
    // opens the reveal modal on the secret — so a `@Microsoft.KeyVault(...)`
    // reference can be followed and decoded in place. Routed here for auth/tx.
    if matches!(action, Action::OpenSelected) && state.view == View::EnvVars {
        open_key_vault_ref_from_env_var(state, auth, tx);
        return;
    }
    // `s` on a Container App: queue an `az containerapp exec` shell (drained by
    // the event loop, which owns the terminal). For any other resource it keeps
    // `s`'s global switch-subscription meaning.
    if let Action::ShellIntoContainer = action {
        request_container_shell(state);
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

/// Open the secret-value reveal modal for the selected secret row and kick off
/// the on-demand value fetch. No-op (with a status hint) for certificates —
/// they have no plaintext value — and when nothing is selected.
fn open_key_vault_secret_modal(
    state: &mut AppState,
    auth: &AzureAuth,
    tx: &UnboundedSender<AppEvent>,
) {
    use crate::azure::key_vault::ItemKind;
    use crate::ui::state::{SecretModal, SecretRevealStatus};

    if state.key_vault.items_kind != ItemKind::Secret {
        state.set_status("only secrets have a value to reveal (certificates don't)");
        return;
    }
    let Some(vault) = state.key_vault.selected_vault.clone() else {
        return;
    };
    let Some(name) = state
        .key_vault
        .filtered_items(&vault.id)
        .get(state.key_vault.items_cursor)
        .map(|i| i.name.clone())
    else {
        return;
    };
    state.key_vault.secret_modal = Some(SecretModal {
        vault_id: vault.id.clone(),
        name: name.clone(),
        status: SecretRevealStatus::Loading,
        scroll: 0,
    });
    spawn_load_key_vault_secret_value(auth.clone(), vault, name, tx.clone());
}

/// Enter on a Key Vault-backed env var: pin the referenced vault, switch to its
/// secrets list, and open the reveal modal on the referenced secret — fetching +
/// decoding the value on demand. Two sources resolve to a vault: a Function App
/// `@Microsoft.KeyVault(...)` reference (the value itself), and a Container App
/// `secretRef` whose app-level secret carries a `keyVaultUrl`. Plain env vars
/// are a silent no-op; a `secretRef` to a plain in-app secret just gets a hint
/// (ARM doesn't expose that value).
fn open_key_vault_ref_from_env_var(
    state: &mut AppState,
    auth: &AzureAuth,
    tx: &UnboundedSender<AppEvent>,
) {
    use crate::azure::key_vault::{ItemKind, KeyVault};
    use crate::ui::state::{SecretModal, SecretRevealStatus};

    let Some(resource) = state.selected_resource() else {
        return;
    };
    let (id, kind) = (resource.id.clone(), resource.kind);
    let cursor = state.env_vars_view.cursor;
    let selected = crate::ui::views::detail::env_vars_for(state, &id, kind)
        .and_then(|vars| vars.get(cursor.min(vars.len().saturating_sub(1))).cloned());
    let Some(v) = selected else {
        return;
    };
    if !v.is_secret {
        return;
    }

    // A Function App reference is self-describing (`@Microsoft.KeyVault(...)`).
    // A Container App `secretRef` is an indirection: the env var names an
    // app-level secret, whose `keyVaultUrl` (when present) is the actual vault
    // pointer — so resolve through `configuration.secrets`.
    let parsed = crate::azure::key_vault::parse_key_vault_ref(&v.value).or_else(|| {
        if kind != ResourceKind::ContainerApp {
            return None;
        }
        let secret_name = crate::azure::env_vars::secret_ref_name(&v.value)?;
        let url = state
            .container_app_overview
            .by_resource
            .get(&id)?
            .secret_key_vault_url(secret_name)?;
        crate::azure::key_vault::key_vault_ref_from_secret_uri(url)
    });

    let Some(parsed) = parsed else {
        // Secret-backed, but nothing to follow: a Container App secret holding a
        // plain value (ARM redacts it), or any other non-vault reference.
        state.set_status(if kind == ResourceKind::ContainerApp {
            "this app secret is a plain value, not a Key Vault reference — \
             its plaintext isn't returned by ARM"
        } else {
            "not a Key Vault reference"
        });
        return;
    };

    // Prefer an already-discovered vault (real ARM id + metadata) so the items
    // cache and any later KeyVaults drill-in share one entry; otherwise build a
    // minimal vault from the parsed reference — enough for the data-plane fetch.
    let vault = state
        .key_vault
        .vaults
        .as_ref()
        .and_then(|vaults| {
            vaults.iter().find(|kv| {
                kv.name.eq_ignore_ascii_case(&parsed.vault_name)
                    || parsed.vault_uri.as_deref().is_some_and(|u| {
                        kv.vault_uri_or_default().trim_end_matches('/') == u.trim_end_matches('/')
                    })
            })
        })
        .cloned()
        .unwrap_or_else(|| {
            let vault_uri = parsed
                .vault_uri
                .clone()
                .unwrap_or_else(|| format!("https://{}.vault.azure.net/", parsed.vault_name));
            KeyVault {
                id: vault_uri.trim_end_matches('/').to_string(),
                name: parsed.vault_name.clone(),
                resource_group: String::new(),
                subscription_id: String::new(),
                location: String::new(),
                sku: None,
                vault_uri: Some(vault_uri),
                rbac_authorization_enabled: None,
                soft_delete_enabled: None,
                purge_protection_enabled: None,
                public_network_access: None,
            }
        });

    state.key_vault.selected_vault = Some(vault.clone());
    state.key_vault.items_kind = ItemKind::Secret;
    state.key_vault.items_cursor = 0;
    state.key_vault.items_filter = tui_input::Input::default();
    state.key_vault.secret_modal = Some(SecretModal {
        vault_id: vault.id.clone(),
        name: parsed.secret_name.clone(),
        status: SecretRevealStatus::Loading,
        scroll: 0,
    });
    // Record where the jump came from so Esc returns here instead of walking
    // to KeyVaultItems' semantic parent (a KeyVaults list the user never
    // visited). See `KeyVaultCache::items_return_view`.
    state.key_vault.items_return_view = Some(state.view);
    state.view = View::KeyVaultItems;
    spawn_load_key_vault_secret_value(auth.clone(), vault, parsed.secret_name, tx.clone());
}

/// `s` handler: queue a container shell for the selected Container App, or fall
/// back to `s`'s global switch-subscription meaning for anything else. The
/// actual `az containerapp exec` runs from the event loop (it must own the
/// terminal to suspend the TUI), so this only records the target.
fn request_container_shell(state: &mut AppState) {
    let resolved = match state.selected_resource() {
        Some(r) if r.kind == ResourceKind::ContainerApp => {
            let subscription = (!r.subscription_id.is_empty()).then(|| r.subscription_id.clone());
            Some((
                r.id.clone(),
                r.name.clone(),
                r.resource_group.clone(),
                subscription,
            ))
        }
        _ => None,
    };
    let Some((id, name, resource_group, subscription)) = resolved else {
        // Not a Container App — `s` keeps switching subscription.
        apply_navigation_action(Action::SwitchSubscription, state);
        return;
    };

    // Target the active revision and the busiest-relevant replica/container so
    // the shell lands where the instances block points. Any unresolved field is
    // left `None` and `az` fills in its own default (latest revision, a replica,
    // first container).
    let revision = state
        .revision_meta
        .by_resource
        .get(&id)
        .map(|m| m.name.clone());
    let (replica, container) = pick_exec_target(state, &id);
    state.pending_exec = Some(PendingExec {
        name,
        resource_group,
        subscription,
        revision,
        replica,
        container,
    });
}

/// Choose which replica + container `az containerapp exec` should target: the
/// newest *running* replica (falling back to the newest overall), and its first
/// container — the app container in template order, matching the "first/primary
/// container" choice. Returns `(None, None)` when no replica is cached, letting
/// `az` pick its own defaults.
fn pick_exec_target(state: &AppState, id: &str) -> (Option<String>, Option<String>) {
    let Some(replicas) = state.replica_instances.by_resource.get(id) else {
        return (None, None);
    };
    let mut sorted: Vec<&crate::azure::container_app_replicas::ReplicaInstance> =
        replicas.iter().collect();
    sorted.sort_by_key(|r| std::cmp::Reverse(r.created_at));
    let chosen = sorted
        .iter()
        .find(|r| {
            r.running_state
                .as_deref()
                .is_some_and(|s| s.eq_ignore_ascii_case("Running"))
        })
        .or_else(|| sorted.first())
        .copied();
    let Some(rep) = chosen else {
        return (None, None);
    };
    let replica = Some(rep.name.clone()).filter(|n| !n.is_empty());
    let container = rep
        .containers
        .first()
        .map(|c| c.name.clone())
        .filter(|n| !n.is_empty());
    (replica, container)
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
    // A visual-line yank ends the selection, vim-style — the copy is done.
    if state.view == View::Logs {
        state.logs.visual_anchor = None;
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
        View::List => state
            .selected_resource()
            .map(|r| format!("{PORTAL_BASE}{}", r.id)),
        // Most Detail meta rows open the resource overview, but some deep-link to
        // a specific blade based on which row the cursor is on — e.g. the
        // Function App `network:` row → the resource's Networking blade.
        View::Detail => state.selected_resource().map(|r| {
            let suffix =
                crate::ui::views::detail::selected_meta_portal_suffix(state, r).unwrap_or("");
            format!("{PORTAL_BASE}{}{suffix}", r.id)
        }),
        // From the env-vars page, `o` jumps straight to the resource's
        // environment-variables blade rather than its overview. Function Apps
        // have a dedicated "Environment variables" blade; Container Apps surface
        // env vars inside the "Containers" blade (there's no standalone one).
        // The empty-tenant `#@/resource` base lets the portal resolve to the
        // signed-in user's tenant, so no tenant id is baked in here.
        View::EnvVars => state.selected_resource().map(|r| {
            use crate::azure::resources::ResourceKind;
            let blade = match r.kind {
                ResourceKind::FunctionApp => "/environmentVariablesAppSettings",
                ResourceKind::ContainerApp => "/containers",
                _ => "",
            };
            format!("{PORTAL_BASE}{}{blade}", r.id)
        }),
        // Logs views open the Azure Monitor **Logs blade** with the same KQL
        // azpect ran pre-filled, scoped to the resource. When a specific line is
        // highlighted, the time window is narrowed to bracket it so the user
        // lands on (or right next to) that row. (The old `{id}/logs` anchor
        // isn't a real blade ref, so the portal silently fell back to the
        // resource overview — this replaces it.)
        View::Logs | View::LogDetail => state.selected_resource().and_then(|r| {
            use crate::azure::resources::ResourceKind;
            let lines = state
                .logs
                .by_resource
                .get(&r.id)
                .map(|v| v.as_slice())
                .unwrap_or(&[]);
            let query = crate::azure::logs::portal_query(r, lines)?;
            let selected_ts = crate::ui::views::logs_detail::selected_line(state).map(|l| l.ts);
            let timespan = match selected_ts {
                Some(ts) => logs_timespan_around(ts),
                None => logs_timespan_for_range(state.logs.range),
            };
            // Container App logs live in the env's Log Analytics workspace, so
            // the blade must be scoped there (the app resource scope has none of
            // the console-log tables). Fall back to the resource id if the
            // workspace hasn't been resolved yet — Function Apps are always
            // resource-scoped.
            let scope_id = match r.kind {
                ResourceKind::ContainerApp => state
                    .logs
                    .workspace_ids
                    .get(&r.id)
                    .map(String::as_str)
                    .unwrap_or(r.id.as_str()),
                _ => r.id.as_str(),
            };
            Some(logs_blade_url(scope_id, &query, &timespan))
        }),
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
        // Cursor 0 is the synthetic "All subscriptions" row (no single sub to
        // open); rows below index the *filtered* list at `cursor - 1`.
        View::Subscriptions => state
            .subscription_cursor
            .checked_sub(1)
            .and_then(|i| state.filtered_subscription_list().into_iter().nth(i))
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
        // Registry views land on the registry's **repository** blade — the
        // overview page is just a metadata panel, whereas the repository
        // blade is where users actually browse images and is what the
        // in-app drill-in shows. Matches the pattern we use for APIM (open
        // on the APIs blade, not the service overview).
        View::Registries => state
            .registry
            .filtered_registries()
            .get(state.registry.registries_cursor)
            .map(|r| format!("{PORTAL_BASE}{}/repository", r.id)),
        View::RegistryRepositories | View::RegistryTags => state
            .registry
            .selected_registry
            .as_ref()
            .map(|r| format!("{PORTAL_BASE}{}/repository", r.id)),
        // Cosmos views land on the account's Data Explorer blade — that's
        // where the user can act on what they were browsing in the TUI.
        View::CosmosAccounts => state
            .cosmos
            .filtered_accounts()
            .get(state.cosmos.accounts_cursor)
            .map(|a| format!("{PORTAL_BASE}{}", a.id)),
        View::CosmosDatabases | View::CosmosContainers | View::CosmosItem => state
            .cosmos
            .selected_account
            .as_ref()
            .map(|a| format!("{PORTAL_BASE}{}", a.id)),
        // Key Vault views land on the vault's overview blade; the Azure
        // portal's secrets/certs panes are reachable via the side nav from
        // there. The cursor indexes into the *filtered* view so `o` follows
        // what's on screen.
        View::KeyVaults => state
            .key_vault
            .filtered_vaults()
            .get(state.key_vault.vaults_cursor)
            .map(|v| format!("{PORTAL_BASE}{}", v.id)),
        View::KeyVaultItems => state
            .key_vault
            .selected_vault
            .as_ref()
            .map(|v| format!("{PORTAL_BASE}{}", v.id)),
        // The access log's portal counterpart is the vault's diagnostics /
        // logs side nav; the overview blade is the stable entry point.
        View::KeyVaultAccessLogs => state
            .key_vault
            .selected_vault
            .as_ref()
            .map(|v| format!("{PORTAL_BASE}{}", v.id)),
        // Service Bus views land on the namespace overview blade; the portal
        // exposes queues / topics / subscriptions from the side nav there. The
        // cursor indexes into the *filtered* view so `o` follows the screen.
        View::ServiceBusNamespaces => state
            .service_bus
            .filtered_namespaces()
            .get(state.service_bus.namespaces_cursor)
            .map(|n| format!("{PORTAL_BASE}{}", n.id)),
        View::ServiceBusEntities | View::ServiceBusSubscriptions => state
            .service_bus
            .selected_namespace
            .as_ref()
            .map(|n| format!("{PORTAL_BASE}{}", n.id)),
        // Azure SQL: the flat list opens the highlighted pool/database blade;
        // the detail view opens the pinned resource. Both land on the resource
        // overview, where the portal's Metrics blade is one click away.
        View::SqlResources => state
            .sql
            .filtered_resources()
            .get(state.sql.cursor)
            .map(|r| format!("{PORTAL_BASE}{}", r.id)),
        View::SqlDetail => state
            .sql
            .selected
            .as_ref()
            .map(|r| format!("{PORTAL_BASE}{}", r.id)),
        // The audit views' portal counterpart is the server's Auditing blade;
        // the pinned resource's overview is the stable entry point.
        View::SqlAuditPrincipals
        | View::SqlAuditEvents
        | View::SqlAuditEventDetail
        | View::SqlSessions => state
            .sql
            .selected
            .as_ref()
            .map(|r| format!("{PORTAL_BASE}{}", r.id)),
        View::Help => None,
    }
}

/// Assemble the Azure Monitor Logs blade deep link scoped to `scope_id` (an ARM
/// resource id — the resource itself for Function Apps, the Log Analytics
/// workspace for Container Apps), pre-filled with `query` over `timespan`.
///
/// Uses the uncompressed `query` path segment (plain percent-encoded KQL), which
/// the portal accepts for queries under ~1.6k chars — ours are far shorter, so
/// we avoid pulling in gzip+base64 just for the compressed `q` form. The
/// empty-tenant `#@/` base (matching [`portal_url_for`]'s `PORTAL_BASE`) lets the
/// portal resolve the signed-in user's tenant.
fn logs_blade_url(scope_id: &str, query: &str, timespan: &str) -> String {
    const BLADE_BASE: &str =
        "https://portal.azure.com/#@/blade/Microsoft_Azure_Monitoring_Logs/LogsBlade";
    format!(
        "{BLADE_BASE}/resourceId/{rid}/source/LogsBlade.AnalyticsShareLinkToQuery/query/{q}/timespan/{ts}",
        rid = percent_encode(scope_id),
        q = percent_encode(query),
        ts = percent_encode(timespan),
    )
}

/// ISO-8601 timespan for the Logs blade when no specific line is selected: the
/// same relative window the logs view is showing (`PT1H` / `P1D` / `P7D`).
fn logs_timespan_for_range(range: crate::azure::metrics::TimeRange) -> String {
    use crate::azure::metrics::TimeRange;
    // Uniform `PT<hours>H` durations — the portal time picker parses these more
    // reliably than the day/week `P1D`/`P7D` forms.
    match range {
        TimeRange::Hour => "PT1H",
        TimeRange::Day => "PT24H",
        TimeRange::Week => "PT168H",
    }
    .to_string()
}

/// Absolute Logs-blade timespan bracketing a highlighted line: a one-minute
/// window centred on its timestamp. Narrow enough that the row is right there,
/// wide enough to absorb sub-second precision / clock-skew so the line is always
/// inside the window. Format is `start/end` in the ISO-8601 form the portal's
/// time picker accepts.
fn logs_timespan_around(ts: chrono::DateTime<chrono::Utc>) -> String {
    let half = chrono::Duration::seconds(30);
    let fmt = |t: chrono::DateTime<chrono::Utc>| t.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
    format!("{}/{}", fmt(ts - half), fmt(ts + half))
}

/// Percent-encode a string for use as a single Azure-portal deep-link path
/// segment: everything outside the RFC 3986 *unreserved* set (`A-Za-z0-9-_.~`)
/// is escaped, including `/` → `%2F`, so a multi-segment resource id or a KQL
/// body with slashes/pipes can't break the blade's `/`-delimited path parsing.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Resolve what `y` should copy from the currently-visible view. The logs
/// view prefers the displayed error (when the table is empty / the error
/// banner is showing) and otherwise the highlighted log line.
fn yank_target(state: &AppState) -> Option<String> {
    match state.view {
        View::Logs => yank_from_logs(state),
        View::LogDetail => crate::ui::views::logs_detail::selected_line(state)
            .map(crate::ui::views::logs_detail::yank_text),
        View::EnvVars => crate::ui::views::env_vars::yank_text(state),
        View::List | View::Detail => state.selected_resource().map(|r| r.id.clone()),
        View::ApimApis => state.selected_resource().and_then(|r| {
            // Resolve via the filtered slice so `y` matches the row on screen.
            state
                .apim
                .filtered_apis(&r.id)
                .get(state.apim.apis_cursor)
                .map(|api| api.id.clone())
        }),
        View::ApimOperations => state.apim.selected_api_id.as_deref().and_then(|api_id| {
            // Index the *filtered* slice so yank follows what's on screen.
            state
                .apim
                .filtered_operations(api_id)
                .get(state.apim.operations_cursor)
                .map(|op| op.id.clone())
        }),
        View::ApimPolicy => crate::ui::views::apim_policy::yank_text(state)
            .or_else(|| state.apim.selected_operation_id.clone()),
        View::AppGatewayBackends => crate::ui::views::appgw_backends::yank_text(state)
            .or_else(|| state.selected_resource().map(|r| r.id.clone())),
        // Cursor 0 = synthetic "All" row (nothing to yank); below indexes the
        // *filtered* list at `cursor - 1`.
        View::Subscriptions => state
            .subscription_cursor
            .checked_sub(1)
            .and_then(|i| state.filtered_subscription_list().into_iter().nth(i))
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
        View::Registries => state
            .registry
            .filtered_registries()
            .get(state.registry.registries_cursor)
            .map(|r| r.id.clone()),
        View::RegistryRepositories => {
            let registry = state.registry.selected_registry.as_ref()?;
            state
                .registry
                .filtered_repositories(&registry.id)
                .get(state.registry.repositories_cursor)
                .map(|r| format!("{}/{}", registry.login_server_or_default(), r.name))
        }
        View::RegistryTags => crate::ui::views::registry_tags::yank_text(state).or_else(|| {
            let registry = state.registry.selected_registry.as_ref()?;
            let repository = state.registry.selected_repository.as_deref()?;
            Some(format!(
                "{}/{}",
                registry.login_server_or_default(),
                repository
            ))
        }),
        View::CosmosAccounts => state
            .cosmos
            .filtered_accounts()
            .get(state.cosmos.accounts_cursor)
            .map(|a| a.id.clone()),
        View::CosmosDatabases => {
            let account = state.cosmos.selected_account.as_ref()?;
            state
                .cosmos
                .filtered_databases(&account.id)
                .get(state.cosmos.databases_cursor)
                .map(|d| format!("{}/{}", account.name, d.name))
        }
        View::CosmosContainers => {
            let account = state.cosmos.selected_account.as_ref()?;
            let db = state.cosmos.selected_database.as_deref()?;
            state
                .cosmos
                .filtered_containers(&account.id, db)
                .get(state.cosmos.containers_cursor)
                .map(|c| format!("{}/{}/{}", account.name, db, c.name))
        }
        View::CosmosItem => crate::ui::views::cosmos_item::yank_text(state).or_else(|| {
            let account = state.cosmos.selected_account.as_ref()?;
            let db = state.cosmos.selected_database.as_deref()?;
            let coll = state.cosmos.selected_container.as_deref()?;
            Some(format!("{}/{}/{}", account.name, db, coll))
        }),
        View::KeyVaults => state
            .key_vault
            .filtered_vaults()
            .get(state.key_vault.vaults_cursor)
            .map(|v| v.id.clone()),
        View::KeyVaultItems => crate::ui::views::key_vault_items::yank_text(state).or_else(|| {
            let vault = state.key_vault.selected_vault.as_ref()?;
            Some(vault.id.clone())
        }),
        View::KeyVaultAccessLogs => crate::ui::views::key_vault_access::yank_text(state),
        View::ServiceBusNamespaces => state
            .service_bus
            .filtered_namespaces()
            .get(state.service_bus.namespaces_cursor)
            .map(|n| n.id.clone()),
        View::ServiceBusEntities => {
            let ns = state.service_bus.selected_namespace.as_ref()?;
            crate::ui::views::service_bus_entities::yank_text(state)
                .map(|name| format!("{}/{}", ns.name, name))
        }
        View::ServiceBusSubscriptions => {
            let ns = state.service_bus.selected_namespace.as_ref()?;
            let topic = state.service_bus.selected_topic.as_deref()?;
            state
                .service_bus
                .filtered_subscriptions(&ns.id, topic)
                .get(state.service_bus.subscriptions_cursor)
                .map(|s| format!("{}/{}/{}", ns.name, topic, s.name))
        }
        // Azure SQL: yank the highlighted/pinned resource's ARM id.
        View::SqlResources => state
            .sql
            .filtered_resources()
            .get(state.sql.cursor)
            .map(|r| r.id.clone()),
        View::SqlDetail => state.sql.selected.as_ref().map(|r| r.id.clone()),
        View::SqlAuditPrincipals | View::SqlAuditEvents | View::SqlAuditEventDetail => {
            crate::ui::views::sql_audit::yank_text(state)
        }
        View::SqlSessions => crate::ui::views::sql_sessions::yank_text(state),
        View::Help => None,
    }
}

fn yank_from_logs(state: &AppState) -> Option<String> {
    let resource = state.selected_resource()?;
    let empty = state
        .logs
        .by_resource
        .get(&resource.id)
        .map(|l| l.is_empty())
        .unwrap_or(true);
    // Error banner is showing iff there's an error AND no rows to display.
    if empty {
        if let Some(err) = state.logs.last_error.as_deref() {
            return Some(crate::ui::views::logs::friendly_log_error(err));
        }
    }
    // The cursor indexes the source-filtered view, same as the table render.
    let lines = state.visible_log_lines(&resource.id);
    if lines.is_empty() {
        return None;
    }
    let cursor = state.logs.scroll.min(lines.len() - 1);

    // Visual-line mode: yank every row in the anchored span, one per line.
    // Otherwise just the line under the cursor.
    let (lo, hi) = match state.logs.visual_anchor {
        Some(anchor) => {
            let anchor = anchor.min(lines.len() - 1);
            (anchor.min(cursor), anchor.max(cursor))
        }
        None => (cursor, cursor),
    };
    let text = lines[lo..=hi]
        .iter()
        .map(|line| format_log_line_for_yank(line))
        .collect::<Vec<_>>()
        .join("\n");
    Some(text)
}

/// One log line as the single-line yank payload: timestamp, level, source,
/// and message on one row so multi-line selections paste cleanly.
fn format_log_line_for_yank(line: &crate::azure::logs::LogLine) -> String {
    format!(
        "{}  {:?}  {}  {}",
        line.ts.format("%Y-%m-%dT%H:%M:%SZ"),
        line.level,
        line.source,
        line.message
    )
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
            // Followed a Key Vault reference (env vars → items view): return
            // to the origin instead of the items view's semantic parent.
            if state.view == View::KeyVaultItems {
                if let Some(origin) = state.key_vault.items_return_view.take() {
                    state.view = origin;
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
                state.view = View::Subscriptions;
                // Open the picker on the full list (drop any stale `/`-search).
                state.subscription_filter.reset();
                state.subscription_filter_active = false;
            }
            true
        }
        Action::OpenStorage => {
            crate::ui::state::enter_category(state, crate::ui::state::Category::Storage);
            true
        }
        Action::OpenRegistries => {
            crate::ui::state::enter_category(state, crate::ui::state::Category::Registries);
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
        View::EnvVars => Some(View::Detail),
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
        View::Registries => Some(View::Subscriptions),
        View::RegistryRepositories => Some(View::Registries),
        View::RegistryTags => Some(View::RegistryRepositories),
        View::CosmosAccounts => Some(View::Subscriptions),
        View::CosmosDatabases => Some(View::CosmosAccounts),
        View::CosmosContainers => Some(View::CosmosDatabases),
        View::CosmosItem => Some(View::CosmosContainers),
        View::KeyVaults => Some(View::Subscriptions),
        View::KeyVaultItems => Some(View::KeyVaults),
        // The view's own Back handler consumes Esc first (it returns to the
        // recorded origin — vaults list or items list); this is the static
        // fallback for the breadcrumb tree.
        View::KeyVaultAccessLogs => Some(View::KeyVaults),
        View::ServiceBusNamespaces => Some(View::Subscriptions),
        View::ServiceBusEntities => Some(View::ServiceBusNamespaces),
        View::ServiceBusSubscriptions => Some(View::ServiceBusEntities),
        View::SqlResources => Some(View::Subscriptions),
        View::SqlDetail => Some(View::SqlResources),
        // The principals view's own Back handler consumes Esc first (it
        // returns to the recorded origin — list or detail); this is the
        // static fallback for the breadcrumb tree.
        View::SqlAuditPrincipals => Some(View::SqlResources),
        View::SqlAuditEvents => Some(View::SqlAuditPrincipals),
        View::SqlAuditEventDetail => Some(View::SqlAuditEvents),
        // The view's own Back handler returns to the recorded origin; this is
        // the static breadcrumb fallback.
        View::SqlSessions => Some(View::SqlResources),
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
        // Changing the logs fetch scope (errors-only toggle, time window) must
        // ALWAYS spawn a fresh fetch, even while a prior one is still in flight —
        // otherwise a rapid second press is swallowed by the loading debounce and
        // the buffer ends up reflecting the wrong filter. The generation guard in
        // `LogsLoaded` discards whichever in-flight fetch is now stale, so forcing
        // here is safe. Forcing is scoped to the logs view; the Detail metrics
        // window-change clears its own cache and needs no force.
        Action::ToggleErrorsOnly
        | Action::SetWindowHour
        | Action::SetWindowDay
        | Action::SetWindowWeek
        | Action::SetWindowMonth
        | Action::SetWindowYear
        | Action::ToggleExcludeSelf => {
            // Chart windows force too: the view handler dropped the cached
            // series for the new range, and a fetch for the *old* range still
            // in flight would otherwise debounce the respawn away — the range
            // tag on `MetricsLoaded` / `SqlMetricsLoaded` discards that stale
            // result, so forcing is safe (mirrors the logs generation guard).
            // The KV access view is in the force set for the same reason: its
            // window keys and the exclude-me toggle change the query scope and
            // its generation guard discards the stale in-flight page.
            let force = matches!(
                state.view,
                View::Logs
                    | View::LogDetail
                    | View::Detail
                    | View::SqlDetail
                    | View::KeyVaultAccessLogs
                    | View::SqlAuditPrincipals
                    | View::SqlAuditEvents
            );
            kick_off_loads_for_view(state, auth, tx, force);
        }
        // Enter in the KV access view while its buffer is invalidated is the
        // custom-window input committing (the view handler just parsed and
        // applied it) — spawn the refetch past the pending-flag debounce.
        Action::OpenSelected
            if state.view == View::KeyVaultAccessLogs
                && !state.key_vault.access_window_input_active
                && state.key_vault.access_events.is_none() =>
        {
            kick_off_loads_for_view(state, auth, tx, /* force */ true);
        }
        // Same custom-window-commit spawn for the SQL audit views (Enter on
        // the principals view is otherwise the drill-in, handled by the
        // generic OpenSelected arm below).
        Action::OpenSelected
            if state.view == View::SqlAuditPrincipals
                && !state.sql.audit.window_input_active
                && state.sql.audit.principals.is_none() =>
        {
            kick_off_loads_for_view(state, auth, tx, /* force */ true);
        }
        Action::OpenSelected
            if state.view == View::SqlAuditEvents
                && !state.sql.audit.window_input_active
                && state.sql.audit.events.is_none() =>
        {
            kick_off_loads_for_view(state, auth, tx, /* force */ true);
        }
        // Esc from the events view lands on the principals view, whose buffer
        // may have been invalidated by a window change made *inside* events —
        // kick a (non-force, so a no-op when cached) reload for it.
        Action::Back if state.view == View::SqlAuditPrincipals => {
            kick_off_loads_for_view(state, auth, tx, /* force */ false);
        }
        // Opening logs always starts a fresh newest-end fetch; force past the
        // global `logs.loading` debounce so switching to resource B while
        // resource A's fetch is still in flight doesn't leave B's view empty
        // (the generation bump in the Logs kick orphans A's result).
        Action::OpenLogs if state.view == View::Logs => {
            kick_off_loads_for_view(state, auth, tx, /* force */ true);
        }
        // The view handler likely transitioned `state.view`. Kick off loads
        // appropriate to whatever the new view is.
        Action::OpenSelected
        | Action::OpenLogs
        | Action::OpenSessions
        | Action::OpenStorage
        | Action::OpenRegistries
        // NextPanel / PrevPanel toggle a sub-kind in place (Key Vault
        // secrets↔certs, Service Bus queues↔topics). Kick off a load so the
        // newly-selected kind fetches without an extra `r`; the per-key
        // debounce in `kick_off_loads_for_view` makes it a no-op when cached.
        | Action::NextPanel
        | Action::PrevPanel => {
            kick_off_loads_for_view(state, auth, tx, /* force */ false);
        }
        _ => {}
    }

    // SQL audit events: the view handler requests an older-than page by
    // setting this flag (view handlers can't spawn tasks) — drain it here,
    // whatever action raised it.
    if state.sql.audit.events_fetch_older {
        state.sql.audit.events_fetch_older = false;
        fetch_older_sql_audit_events(state, auth, tx);
    }
}

/// Spawn the *older-than* audit-events page for the scroll-past-bottom fetch:
/// same scope as the loaded buffer, `before` pinned to its oldest row. Runs
/// under the current events generation (no bump — it extends, not replaces,
/// the buffer; any scope change bumps and orphans it).
fn fetch_older_sql_audit_events(
    state: &mut AppState,
    auth: &AzureAuth,
    tx: &UnboundedSender<AppEvent>,
) {
    let audit = &state.sql.audit;
    if audit.events_pending || audit.events_loading_more || !audit.events_truncated {
        return;
    }
    let (Some(target), Some(principal)) = (audit.target.clone(), audit.selected_principal.clone())
    else {
        return;
    };
    let Some(oldest) = audit.events.as_ref().and_then(|e| e.last()).map(|e| e.ts) else {
        return;
    };
    state.sql.audit.events_loading_more = true;
    spawn_load_sql_audit_events(
        auth.clone(),
        target,
        state.sql.audit.window.clone(),
        principal,
        state.sql.audit.events_errors_only,
        Some(oldest),
        /* append */ true,
        state.sql.audit.events_generation,
        tx.clone(),
    );
}

/// Look at `state.view` and the loading flags, and spawn whichever loaders are
/// missing. `force` overrides the loading-flag debounce (used for the explicit
/// Refresh action).
/// Silently re-fetch the resource list's health + version on a timer so the list
/// self-updates without the user pressing `r`. Driven from the `Tick` arm.
///
/// Only fires on the List view, and only once `config.refresh_secs` has elapsed
/// since the last load landed (`resources_loaded_at`). It's a no-op while a
/// resource load is already in flight (`loading_resources` re-arms once it
/// completes) and while any input-capturing overlay is open — a background reload
/// that reordered rows or moved the cursor mid-interaction would be hostile.
///
/// The refresh is a `force` `kick_off_loads_for_view`, identical to `r`: it
/// re-fetches in place without dropping cached values, so settled rows update
/// silently instead of flashing back to LOADING.
fn maybe_auto_refresh(
    state: &mut AppState,
    auth: &AzureAuth,
    tx: &UnboundedSender<AppEvent>,
    now: Instant,
) {
    let interval = state.config.refresh_secs;
    if interval == 0 || state.view != View::List || state.loading_resources {
        return;
    }
    // Don't yank data out from under an open modal / active typing.
    if state.command_active
        || state.env_var_edit.is_some()
        || state.quit_confirm
        || state.auth_prompt != AuthPrompt::Hidden
        || state.list_filter_active
    {
        return;
    }
    // Re-arm off whichever is most recent: the last successful load *or* the last
    // auto-refresh attempt. Using the max means a failed/throttled reload (which
    // doesn't advance `resources_loaded_at`) still waits a full interval before
    // retrying instead of firing every tick, and a manual `r` (which advances
    // `resources_loaded_at`) pushes the next auto-refresh out by an interval too.
    let reference = match (state.resources_loaded_at, state.last_auto_refresh) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    };
    // No successful load yet — the initial kick-off (or a manual `r`) owns that;
    // auto-refresh only keeps an already-loaded list fresh.
    let due = reference.is_some_and(|t| now.duration_since(t) >= Duration::from_secs(interval));
    if due {
        state.last_auto_refresh = Some(now);
        kick_off_loads_for_view(state, auth, tx, /* force */ true);
    }
}

/// Subscription ids defining the current fetch scope: the pinned subscription
/// if one is selected, else every subscription the credential can see. `None`
/// while the tenant's subscription list is still unknown (startup, or right
/// after a re-login): an "all subscriptions" fetch issued with an empty list
/// would come back empty, and that empty result would stick until the next
/// manual refresh because the non-force kicks debounce on "already loaded".
/// The `SubscriptionsLoaded` handler re-kicks the active view once the list
/// lands, so deferring here loses nothing.
fn scope_sub_ids(state: &AppState) -> Option<Vec<String>> {
    match &state.selected_subscription {
        Some(id) => Some(vec![id.clone()]),
        None if state.subscriptions.is_empty() => None,
        None => Some(state.subscriptions.iter().map(|s| s.id.clone()).collect()),
    }
}

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
                spawn_load_subscriptions(auth.clone(), state.scope_generation, tx.clone());
            }
        }
        View::List => {
            if force || (!state.loading_resources && state.resources.is_empty()) {
                let Some(sub_ids) = scope_sub_ids(state) else {
                    return;
                };
                state.loading_resources = true;
                spawn_load_resources(auth.clone(), state.scope_generation, sub_ids, tx.clone());
                if force {
                    // Refresh per-row badges + Container App overview *in place*:
                    // re-fetch without dropping the current values, so rows
                    // update silently instead of flickering to LOADING. (The
                    // resource list itself reloads above; new rows get filled by
                    // the non-force `spawn_missing_*` in the ResourcesLoaded
                    // handler.)
                    spawn_missing_list_health(state, auth, tx, /* force */ true);
                    spawn_missing_container_app_overview(state, auth, tx, /* force */ true);
                    spawn_missing_function_app_image(state, auth, tx, /* force */ true);
                }
            }
        }
        View::Detail => {
            if let Some(resource) = state.selected_resource().cloned() {
                let id = resource.id.clone();
                let kind = resource.kind;
                // Capture the directory principals to resolve before `resource`
                // is moved into the metrics fetch below.
                let principal_candidates: Vec<String> = [
                    (
                        resource.meta.created_by.clone(),
                        resource.meta.created_by_type.clone(),
                    ),
                    (
                        resource.meta.modified_by.clone(),
                        resource.meta.modified_by_type.clone(),
                    ),
                ]
                .into_iter()
                .filter_map(|(by, ty)| {
                    principal_to_resolve(by.as_deref(), ty.as_deref()).map(|s| s.to_string())
                })
                .collect();
                // Debounce per resource, not on the global loading flag: a
                // fetch still in flight for resource A must not block the
                // first fetch for resource B when the user navigates Detail
                // A → back → Detail B (B would render blank until a manual
                // refresh otherwise).
                if force || !state.metrics.pending.contains(&resource.id) {
                    if force {
                        state.metrics.failures.remove(&resource.id);
                    }
                    state.metrics.loading = true;
                    state.metrics.pending.insert(resource.id.clone());
                    spawn_load_metrics(auth.clone(), resource, state.metrics.range, tx.clone());
                }
                // Health — and, for Container Apps, the active-revision metadata
                // + overview that ride alongside it — is otherwise fetched once
                // eagerly and never refreshed, so `r` after a deploy would keep
                // showing the old active revision. Re-fetch on explicit refresh,
                // but WITHOUT dropping the cached values: the old rows stay shown
                // (and the header's "· refreshing…" signals the reload) until
                // the fresh data arrives and overwrites them — no flicker.
                if force {
                    if !state.health.pending.contains(&id) {
                        state.health.pending.insert(id.clone());
                        spawn_load_health(auth.clone(), id.clone(), kind, tx.clone());
                    }
                    if kind == crate::azure::resources::ResourceKind::ContainerApp
                        && !state.container_app_overview.pending.contains(&id)
                    {
                        state.container_app_overview.pending.insert(id.clone());
                        spawn_load_container_app_overview(auth.clone(), id.clone(), tx.clone());
                    }
                }
                // Function App OS env vars + per-function triggers are lazy-loaded
                // on Detail entry — neither is shown in the list, so fetching them
                // eagerly for every app would be wasteful. (Container App env vars
                // ride on the eagerly-fetched limits.) Guard against re-spawning
                // while one is cached / in flight.
                if kind == crate::azure::resources::ResourceKind::FunctionApp {
                    let cached = state.func_settings.by_resource.contains_key(&id)
                        || state.func_settings.failures.contains_key(&id);
                    let in_flight = state.func_settings.pending.contains(&id);
                    if force || (!cached && !in_flight) {
                        if force {
                            state.func_settings.by_resource.remove(&id);
                            state.func_settings.failures.remove(&id);
                        }
                        state.func_settings.pending.insert(id.clone());
                        spawn_load_function_app_settings(auth.clone(), id.clone(), tx.clone());
                    }
                    let t_cached = state.func_triggers.by_resource.contains_key(&id)
                        || state.func_triggers.failures.contains_key(&id);
                    let t_in_flight = state.func_triggers.pending.contains(&id);
                    if force || (!t_cached && !t_in_flight) {
                        if force {
                            state.func_triggers.by_resource.remove(&id);
                            state.func_triggers.failures.remove(&id);
                        }
                        state.func_triggers.pending.insert(id.clone());
                        spawn_load_function_app_triggers(auth.clone(), id, tx.clone());
                    }
                }
                // Resolve Application / ManagedIdentity authors to display names
                // via Graph (best-effort, cached, deduped).
                for oid in principal_candidates {
                    if state.principals.by_id.contains_key(&oid)
                        || state.principals.failed.contains(&oid)
                        || state.principals.pending.contains(&oid)
                    {
                        continue;
                    }
                    state.principals.pending.insert(oid.clone());
                    spawn_resolve_principal(auth.clone(), oid, tx.clone());
                }
            }
        }
        View::EnvVars => {
            // The page is reached from Detail (which already fetched), but a
            // refresh here should re-pull the selected resource's env vars.
            if let Some(resource) = state.selected_resource().cloned() {
                let id = resource.id;
                match resource.kind {
                    crate::azure::resources::ResourceKind::FunctionApp => {
                        let cached = state.func_settings.by_resource.contains_key(&id)
                            || state.func_settings.failures.contains_key(&id);
                        let in_flight = state.func_settings.pending.contains(&id);
                        if force || (!cached && !in_flight) {
                            if force {
                                state.func_settings.by_resource.remove(&id);
                                state.func_settings.failures.remove(&id);
                            }
                            state.func_settings.pending.insert(id.clone());
                            spawn_load_function_app_settings(auth.clone(), id, tx.clone());
                        }
                    }
                    // Container App env vars ride on the overview fetch, so a
                    // refresh re-pulls that (otherwise `r` keeps stale values).
                    crate::azure::resources::ResourceKind::ContainerApp if force => {
                        state.container_app_overview.by_resource.remove(&id);
                        state.container_app_overview.pending.insert(id.clone());
                        spawn_load_container_app_overview(auth.clone(), id, tx.clone());
                    }
                    _ => {}
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
                    // Any in-flight fetch (initial or fetch-more, possibly for
                    // a *different* resource) is stale by definition: this
                    // fresh fetch replaces the buffer and owns the loading
                    // flags. Bump the generation so the old result is dropped
                    // instead of clearing flags out from under us.
                    state.logs.generation = state.logs.generation.wrapping_add(1);
                    state.logs.loading = true;
                    // A pending context jump fetches an unfiltered window
                    // centered on the error; otherwise the newest-rows window for
                    // the current errors-only / range scope.
                    let around = state.logs.context_around;
                    spawn_load_logs(
                        auth.clone(),
                        resource,
                        state.logs.range,
                        state.logs.errors_only,
                        None,
                        around,
                        state.logs.generation,
                        tx.clone(),
                    );
                }
            }
        }
        View::ApimApis => {
            if let Some(svc_id) = state
                .selected_resource()
                .filter(|r| r.kind == crate::azure::resources::ResourceKind::Apim)
                .map(|r| r.id.clone())
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
                let Some(sub_ids) = scope_sub_ids(state) else {
                    return;
                };
                if force {
                    state.storage.accounts = None;
                    state.storage.accounts_error = None;
                }
                state.storage.accounts_pending = true;
                spawn_load_storage_accounts(
                    auth.clone(),
                    state.scope_generation,
                    sub_ids,
                    tx.clone(),
                );
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
        View::Registries => {
            let cached = state.registry.registries.is_some();
            let in_flight = state.registry.registries_pending;
            if force || (!cached && !in_flight) {
                let Some(sub_ids) = scope_sub_ids(state) else {
                    return;
                };
                if force {
                    state.registry.registries = None;
                    state.registry.registries_error = None;
                }
                state.registry.registries_pending = true;
                spawn_load_registries(auth.clone(), state.scope_generation, sub_ids, tx.clone());
            }
        }
        View::RegistryRepositories => {
            if let Some(registry) = state.registry.selected_registry.clone() {
                let cached = state.registry.repositories.contains_key(&registry.id);
                let in_flight = state.registry.repositories_pending.contains(&registry.id);
                if force || (!cached && !in_flight) {
                    if force {
                        state.registry.repositories.remove(&registry.id);
                        state.registry.repositories_error.remove(&registry.id);
                    }
                    state
                        .registry
                        .repositories_pending
                        .insert(registry.id.clone());
                    spawn_load_repositories(auth.clone(), registry, tx.clone());
                }
            }
        }
        View::RegistryTags => {
            if let (Some(registry), Some(repository)) = (
                state.registry.selected_registry.clone(),
                state.registry.selected_repository.clone(),
            ) {
                let key = crate::ui::state::RegistryCache::tags_key(&registry.id, &repository);
                let cached = state.registry.tags.contains_key(&key);
                let in_flight = state.registry.tags_pending.contains(&key);
                if force || (!cached && !in_flight) {
                    if force {
                        state.registry.tags.remove(&key);
                        state.registry.tags_error.remove(&key);
                    }
                    state.registry.tags_pending.insert(key);
                    spawn_load_tags(auth.clone(), registry, repository, tx.clone());
                }
            }
        }
        View::CosmosAccounts => {
            let cached = state.cosmos.accounts.is_some();
            let in_flight = state.cosmos.accounts_pending;
            if force || (!cached && !in_flight) {
                let Some(sub_ids) = scope_sub_ids(state) else {
                    return;
                };
                if force {
                    state.cosmos.accounts = None;
                    state.cosmos.accounts_error = None;
                }
                state.cosmos.accounts_pending = true;
                spawn_load_cosmos_accounts(
                    auth.clone(),
                    state.scope_generation,
                    sub_ids,
                    tx.clone(),
                );
            }
        }
        View::SqlResources => {
            let cached = state.sql.resources.is_some();
            let in_flight = state.sql.pending;
            if force || (!cached && !in_flight) {
                let Some(sub_ids) = scope_sub_ids(state) else {
                    return;
                };
                if force {
                    state.sql.resources = None;
                    state.sql.error = None;
                }
                state.sql.pending = true;
                spawn_load_sql_resources(auth.clone(), state.scope_generation, sub_ids, tx.clone());
            }
        }
        View::SqlDetail => {
            if let Some(resource) = state.sql.selected.clone() {
                let id = resource.id.clone();
                let cached = state.sql.metrics.contains_key(&id);
                let in_flight = state.sql.metrics_pending.contains(&id);
                if force || (!cached && !in_flight) {
                    if force {
                        state.sql.metrics.remove(&id);
                        state.sql.metrics_failures.remove(&id);
                    }
                    state.sql.metrics_pending.insert(id.clone());
                    spawn_load_sql_metrics(auth.clone(), id, state.sql.metrics_range, tx.clone());
                }
            }
        }
        View::SqlAuditPrincipals => {
            if let Some(target) = state.sql.audit.target.clone() {
                let cached = state.sql.audit.principals.is_some();
                let in_flight = state.sql.audit.pending;
                if force || (!cached && !in_flight) {
                    if force {
                        state.sql.audit.principals = None;
                        state.sql.audit.error = None;
                    }
                    // Every spawn owns the buffer — see the KV access arm.
                    state.sql.audit.generation = state.sql.audit.generation.wrapping_add(1);
                    state.sql.audit.pending = true;
                    spawn_load_sql_audit_principals(
                        auth.clone(),
                        target.clone(),
                        state.sql.audit.window.clone(),
                        state.sql.audit.generation,
                        tx.clone(),
                    );
                }
                // The (⚠ live T-SQL) database-users merge rides along when the
                // target is a single database. Not window-scoped, so it only
                // fetches once per entry (or on force). Config-off leaves a
                // note instead of a socket.
                if let Some(database) = target.database.clone() {
                    if !state.config.sql_live_queries {
                        if state.sql.audit.db_users_note.is_none() {
                            state.sql.audit.db_users_note =
                                Some("live T-SQL disabled (sql_live_queries = false)".to_string());
                        }
                    } else {
                        let u_cached = state.sql.audit.db_users.is_some()
                            || state.sql.audit.db_users_note.is_some();
                        let u_in_flight = state.sql.audit.db_users_pending;
                        if force || (!u_cached && !u_in_flight) {
                            if force {
                                state.sql.audit.db_users = None;
                                state.sql.audit.db_users_note = None;
                            }
                            state.sql.audit.db_users_generation =
                                state.sql.audit.db_users_generation.wrapping_add(1);
                            state.sql.audit.db_users_pending = true;
                            spawn_load_sql_audit_db_users(
                                auth.clone(),
                                target.server.clone(),
                                database,
                                state.config.sql_live_queries,
                                state.sql.audit.db_users_generation,
                                tx.clone(),
                            );
                        }
                    }
                }
            }
        }
        View::SqlSessions => {
            if let Some(target) = state.sql.sessions.target.clone() {
                let cached = state.sql.sessions.rows.is_some();
                let in_flight = state.sql.sessions.pending;
                if force || (!cached && !in_flight) {
                    if force {
                        state.sql.sessions.rows = None;
                        state.sql.sessions.error = None;
                    }
                    state.sql.sessions.generation = state.sql.sessions.generation.wrapping_add(1);
                    state.sql.sessions.pending = true;
                    spawn_load_sql_sessions(
                        auth.clone(),
                        target,
                        state.config.sql_live_queries,
                        state.sql.sessions.generation,
                        tx.clone(),
                    );
                }
            }
        }
        View::SqlAuditEvents => {
            if let (Some(target), Some(principal)) = (
                state.sql.audit.target.clone(),
                state.sql.audit.selected_principal.clone(),
            ) {
                let cached = state.sql.audit.events.is_some();
                let in_flight = state.sql.audit.events_pending;
                if force || (!cached && !in_flight) {
                    if force {
                        state.sql.audit.events = None;
                        state.sql.audit.events_error = None;
                    }
                    state.sql.audit.events_generation =
                        state.sql.audit.events_generation.wrapping_add(1);
                    state.sql.audit.events_pending = true;
                    spawn_load_sql_audit_events(
                        auth.clone(),
                        target,
                        state.sql.audit.window.clone(),
                        principal,
                        state.sql.audit.events_errors_only,
                        /* before */ None,
                        /* append */ false,
                        state.sql.audit.events_generation,
                        tx.clone(),
                    );
                }
            }
        }
        View::CosmosDatabases => {
            if let Some(account) = state.cosmos.selected_account.clone() {
                let cached = state.cosmos.databases.contains_key(&account.id);
                let in_flight = state.cosmos.databases_pending.contains(&account.id);
                if force || (!cached && !in_flight) {
                    if force {
                        state.cosmos.databases.remove(&account.id);
                        state.cosmos.databases_error.remove(&account.id);
                    }
                    state.cosmos.databases_pending.insert(account.id.clone());
                    spawn_load_cosmos_databases(auth.clone(), account, tx.clone());
                }
            }
        }
        View::CosmosContainers => {
            if let (Some(account), Some(db)) = (
                state.cosmos.selected_account.clone(),
                state.cosmos.selected_database.clone(),
            ) {
                let key = crate::ui::state::CosmosCache::containers_key(&account.id, &db);
                let cached = state.cosmos.containers.contains_key(&key);
                let in_flight = state.cosmos.containers_pending.contains(&key);
                if force || (!cached && !in_flight) {
                    if force {
                        state.cosmos.containers.remove(&key);
                        state.cosmos.containers_error.remove(&key);
                    }
                    state.cosmos.containers_pending.insert(key);
                    spawn_load_cosmos_containers(auth.clone(), account, db, tx.clone());
                }
            }
        }
        View::CosmosItem => {
            if let (Some(account), Some(db), Some(coll)) = (
                state.cosmos.selected_account.clone(),
                state.cosmos.selected_database.clone(),
                state.cosmos.selected_container.clone(),
            ) {
                let key = crate::ui::state::CosmosCache::items_key(&account.id, &db, &coll);
                let cached = state.cosmos.items.contains_key(&key);
                let in_flight = state.cosmos.items_pending.contains(&key);
                if force || (!cached && !in_flight) {
                    if force {
                        state.cosmos.items.remove(&key);
                        state.cosmos.items_error.remove(&key);
                    }
                    state.cosmos.items_pending.insert(key);
                    spawn_load_cosmos_items(auth.clone(), account, db, coll, tx.clone());
                }
            }
        }
        View::KeyVaults => {
            let cached = state.key_vault.vaults.is_some();
            let in_flight = state.key_vault.vaults_pending;
            if force || (!cached && !in_flight) {
                let Some(sub_ids) = scope_sub_ids(state) else {
                    return;
                };
                if force {
                    state.key_vault.vaults = None;
                    state.key_vault.vaults_error = None;
                }
                state.key_vault.vaults_pending = true;
                spawn_load_key_vaults(auth.clone(), state.scope_generation, sub_ids, tx.clone());
            }
        }
        View::KeyVaultItems => {
            if let Some(vault) = state.key_vault.selected_vault.clone() {
                let kind = state.key_vault.items_kind;
                let key = crate::ui::state::KeyVaultCache::items_key(&vault.id, kind);
                let cached = state.key_vault.items.contains_key(&key);
                let in_flight = state.key_vault.items_pending.contains(&key);
                if force || (!cached && !in_flight) {
                    if force {
                        state.key_vault.items.remove(&key);
                        state.key_vault.items_error.remove(&key);
                    }
                    state.key_vault.items_pending.insert(key);
                    spawn_load_key_vault_items(auth.clone(), vault, kind, tx.clone());
                }
            }
        }
        View::KeyVaultAccessLogs => {
            if let Some(vault) = state.key_vault.selected_vault.clone() {
                let cached = state.key_vault.access_events.is_some();
                let in_flight = state.key_vault.access_pending;
                if force || (!cached && !in_flight) {
                    if force {
                        state.key_vault.access_events = None;
                        state.key_vault.access_error = None;
                    }
                    // Every spawn owns the buffer: bump the generation so an
                    // older in-flight page is discarded when it lands instead
                    // of clobbering this fetch's flags (mirrors the logs view).
                    state.key_vault.access_generation =
                        state.key_vault.access_generation.wrapping_add(1);
                    state.key_vault.access_pending = true;
                    spawn_load_key_vault_access(
                        auth.clone(),
                        vault,
                        state.key_vault.access_window.clone(),
                        state.key_vault.access_scope.clone(),
                        state.key_vault.access_exclude_self,
                        state.key_vault.access_generation,
                        tx.clone(),
                    );
                }
            }
        }
        View::ServiceBusNamespaces => {
            let cached = state.service_bus.namespaces.is_some();
            let in_flight = state.service_bus.namespaces_pending;
            if force || (!cached && !in_flight) {
                let Some(sub_ids) = scope_sub_ids(state) else {
                    return;
                };
                if force {
                    state.service_bus.namespaces = None;
                    state.service_bus.namespaces_error = None;
                }
                state.service_bus.namespaces_pending = true;
                spawn_load_sb_namespaces(auth.clone(), state.scope_generation, sub_ids, tx.clone());
            }
        }
        View::ServiceBusEntities => {
            if let Some(ns) = state.service_bus.selected_namespace.clone() {
                // The active toggle decides whether we fetch queues or topics.
                match state.service_bus.entity_kind {
                    crate::azure::service_bus::EntityKind::Queue => {
                        let cached = state.service_bus.queues.contains_key(&ns.id);
                        let in_flight = state.service_bus.queues_pending.contains(&ns.id);
                        if force || (!cached && !in_flight) {
                            if force {
                                state.service_bus.queues.remove(&ns.id);
                                state.service_bus.queues_error.remove(&ns.id);
                            }
                            state.service_bus.queues_pending.insert(ns.id.clone());
                            spawn_load_sb_queues(auth.clone(), ns, tx.clone());
                        }
                    }
                    crate::azure::service_bus::EntityKind::Topic => {
                        let cached = state.service_bus.topics.contains_key(&ns.id);
                        let in_flight = state.service_bus.topics_pending.contains(&ns.id);
                        if force || (!cached && !in_flight) {
                            if force {
                                state.service_bus.topics.remove(&ns.id);
                                state.service_bus.topics_error.remove(&ns.id);
                            }
                            state.service_bus.topics_pending.insert(ns.id.clone());
                            spawn_load_sb_topics(auth.clone(), ns, tx.clone());
                        }
                    }
                }
            }
        }
        View::ServiceBusSubscriptions => {
            if let (Some(ns), Some(topic)) = (
                state.service_bus.selected_namespace.clone(),
                state.service_bus.selected_topic.clone(),
            ) {
                let key = crate::ui::state::ServiceBusCache::subscriptions_key(&ns.id, &topic);
                let cached = state.service_bus.subscriptions.contains_key(&key);
                let in_flight = state.service_bus.subscriptions_pending.contains(&key);
                if force || (!cached && !in_flight) {
                    if force {
                        state.service_bus.subscriptions.remove(&key);
                        state.service_bus.subscriptions_error.remove(&key);
                    }
                    state.service_bus.subscriptions_pending.insert(key);
                    spawn_load_sb_subscriptions(auth.clone(), ns, topic, tx.clone());
                }
            }
        }
        // LogDetail is a pure-view-over-state screen; nothing to load.
        // Leaves rendered entirely from already-loaded state — nothing to kick.
        View::LogDetail | View::SqlAuditEventDetail | View::Help => {}
    }
}

/// Resolve `logs.pending_anchor` against the buffer that just landed: re-select
/// the anchored line (or the nearest by time) and center it. But only once the
/// buffer actually reaches the anchor's timestamp or the window has no older
/// rows left — a context window centered on an error holds at most one page of
/// its *newest* rows, so on a busy stream the anchored row may be several pages
/// down. Landing "nearest by time" then would silently put the cursor on a
/// later row; instead keep the anchor pending and chain another older-than
/// fetch. Esc in the logs view cancels the chase.
fn resolve_pending_anchor(state: &mut AppState, resource_id: &str) {
    let Some(anchor) = state.logs.pending_anchor.take() else {
        return;
    };
    // Lines are sorted newest → oldest, so the buffer covers the anchor once
    // its last (oldest) row is at or before the anchor's timestamp.
    let covered = state
        .logs
        .by_resource
        .get(resource_id)
        .and_then(|lines| lines.last())
        .is_some_and(|oldest| oldest.ts <= anchor.ts);
    let more = state
        .logs
        .more_available
        .get(resource_id)
        .copied()
        .unwrap_or(false);
    if !covered && more {
        state.logs.pending_anchor = Some(anchor);
        state.logs.fetch_more_requested = true;
        return;
    }
    if let Some(idx) = state.anchor_index(resource_id, &anchor) {
        state.logs.scroll = idx;
        state.logs.center_pending.set(true);
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
        // Pagination never applies to a context window — it's a bounded slice.
        None,
        state.logs.generation,
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
        View::EnvVars => crate::ui::views::env_vars::render(f, view_area, state, theme),
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
        View::Registries => crate::ui::views::registries::render(f, view_area, state, theme),
        View::RegistryRepositories => {
            crate::ui::views::registry_repositories::render(f, view_area, state, theme)
        }
        View::RegistryTags => crate::ui::views::registry_tags::render(f, view_area, state, theme),
        View::CosmosAccounts => {
            crate::ui::views::cosmos_accounts::render(f, view_area, state, theme)
        }
        View::CosmosDatabases => {
            crate::ui::views::cosmos_databases::render(f, view_area, state, theme)
        }
        View::CosmosContainers => {
            crate::ui::views::cosmos_containers::render(f, view_area, state, theme)
        }
        View::CosmosItem => crate::ui::views::cosmos_item::render(f, view_area, state, theme),
        View::KeyVaults => crate::ui::views::key_vaults::render(f, view_area, state, theme),
        View::KeyVaultItems => {
            crate::ui::views::key_vault_items::render(f, view_area, state, theme)
        }
        View::KeyVaultAccessLogs => {
            crate::ui::views::key_vault_access::render(f, view_area, state, theme)
        }
        View::ServiceBusNamespaces => {
            crate::ui::views::service_bus_namespaces::render(f, view_area, state, theme)
        }
        View::ServiceBusEntities => {
            crate::ui::views::service_bus_entities::render(f, view_area, state, theme)
        }
        View::ServiceBusSubscriptions => {
            crate::ui::views::service_bus_subscriptions::render(f, view_area, state, theme)
        }
        View::SqlResources => crate::ui::views::sql_resources::render(f, view_area, state, theme),
        View::SqlDetail => crate::ui::views::sql_detail::render(f, view_area, state, theme),
        View::SqlAuditPrincipals | View::SqlAuditEvents | View::SqlAuditEventDetail => {
            crate::ui::views::sql_audit::render(f, view_area, state, theme)
        }
        View::SqlSessions => crate::ui::views::sql_sessions::render(f, view_area, state, theme),
        View::Help => crate::ui::views::help::render(f, view_area, state, theme),
    }

    // Detail's row-Enter modal stacks above the page itself but below the
    // global overlays (quit / auth) so the latter always win foreground.
    if state.view == View::Detail && state.detail_view.modal.is_some() {
        crate::ui::views::detail::render_modal(f, view_area, state, theme);
    }
    // Key Vault secret-reveal modal — same stacking rationale as Detail's.
    if state.view == View::KeyVaultItems && state.key_vault.secret_modal.is_some() {
        crate::ui::views::key_vault_items::render_modal(f, view_area, state, theme);
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
    // Guarded env-var editor (Ctrl+E / Ctrl+N from the env-vars page). Drawn on
    // top of its page; mutually exclusive with the quit/auth overlays by gating.
    if state.env_var_edit.is_some() {
        crate::ui::views::env_var_edit::render(f, area, state, theme);
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

/// The concrete payload handed to [`spawn_env_var_write`].
enum EnvWrite {
    /// Function App: just the edited entry. The PUT replaces the whole
    /// collection, but `function_app_settings::update` re-reads the live map
    /// and applies this single upsert on top — sending a cached snapshot from
    /// here would silently revert keys changed server-side since the view
    /// last fetched.
    FunctionApp { name: String, value: String },
    /// Container App: the targeted template entry plus its new literal value;
    /// the spawn does the GET-modify-PATCH against the raw template.
    ContainerApp {
        target: crate::azure::container_app_env_update::EnvTarget,
        value: String,
    },
}

/// Key handler for the guarded add/edit-env-var modal. Owns every key while the
/// modal is open. Two phases: `Editing` (type into the fields) and `Confirming`
/// (final yes/no with the diff shown). The modal is taken out of `state` for the
/// duration so the field widgets and the rest of `state` (cache, spawn) can be
/// mutated without borrow conflicts; it's put back unless the flow closed it.
fn handle_env_var_edit_key(
    state: &mut AppState,
    key: crossterm::event::KeyEvent,
    auth: &AzureAuth,
    tx: &UnboundedSender<AppEvent>,
) {
    let mut edit = match state.env_var_edit.take() {
        Some(e) => e,
        None => return,
    };

    // While a write is in flight, freeze the fields. Esc abandons the modal —
    // the background write still completes and updates the cache via its event.
    if edit.in_flight {
        if key.code != KeyCode::Esc {
            state.env_var_edit = Some(edit);
        }
        return;
    }

    match edit.phase {
        EnvVarEditPhase::Editing => match key.code {
            // Esc abandons the whole editor (modal dropped, not put back).
            KeyCode::Esc => {}
            KeyCode::Tab | KeyCode::BackTab => {
                // Only Add has two editable fields; Edit locks the name.
                if matches!(edit.mode, EnvVarEditMode::Add) {
                    edit.focus = match edit.focus {
                        EnvVarField::Name => EnvVarField::Value,
                        EnvVarField::Value => EnvVarField::Name,
                    };
                }
                state.env_var_edit = Some(edit);
            }
            KeyCode::Enter => {
                let name = edit.name.value().trim().to_string();
                if name.is_empty() {
                    edit.error = Some("name can't be empty".into());
                } else if let EnvVarEditMode::Edit { original_value } = &edit.mode {
                    if original_value == edit.value.value() {
                        edit.error = Some("value unchanged — nothing to write".into());
                    } else {
                        edit.error = None;
                        edit.confirm_yes = false; // opt-in: default to Cancel
                        edit.phase = EnvVarEditPhase::Confirming;
                    }
                } else {
                    edit.error = None;
                    edit.confirm_yes = false;
                    edit.phase = EnvVarEditPhase::Confirming;
                }
                state.env_var_edit = Some(edit);
            }
            _ => {
                // Forward typing to the focused field. In Edit mode the name is
                // display-only, so only the value field accepts input.
                match edit.focus {
                    EnvVarField::Name if matches!(edit.mode, EnvVarEditMode::Add) => {
                        edit.name.handle_event(&CtEvent::Key(key));
                    }
                    EnvVarField::Value => {
                        edit.value.handle_event(&CtEvent::Key(key));
                    }
                    _ => {}
                }
                state.env_var_edit = Some(edit);
            }
        },
        EnvVarEditPhase::Confirming => match key.code {
            // Esc steps back to editing so the user can tweak, rather than
            // throwing away what they typed.
            KeyCode::Esc => {
                edit.phase = EnvVarEditPhase::Editing;
                edit.error = None;
                state.env_var_edit = Some(edit);
            }
            KeyCode::Left
            | KeyCode::Right
            | KeyCode::Char('h')
            | KeyCode::Char('l')
            | KeyCode::Tab
            | KeyCode::BackTab => {
                edit.confirm_yes = !edit.confirm_yes;
                state.env_var_edit = Some(edit);
            }
            // Direct yes/no shortcuts, regardless of focus.
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                commit_env_var_edit(state, edit, auth, tx);
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {}
            KeyCode::Enter => {
                if edit.confirm_yes {
                    commit_env_var_edit(state, edit, auth, tx);
                }
                // else: focus was on Cancel — drop the modal.
            }
            _ => {
                state.env_var_edit = Some(edit);
            }
        },
    }
}

/// Build the write payload from the confirmed edit, apply the in-flight flag,
/// and spawn the Azure write. Leaves the modal up (with `in_flight = true`) so
/// the completion event can close it on success or surface an error on failure.
fn commit_env_var_edit(
    state: &mut AppState,
    mut edit: crate::ui::state::EnvVarEdit,
    auth: &AzureAuth,
    tx: &UnboundedSender<AppEvent>,
) {
    use crate::azure::resources::ResourceKind;

    let name = edit.name.value().trim().to_string();
    let value = edit.value.value().to_string();
    let applied = AppliedEnvEdit {
        resource_id: edit.resource_id.clone(),
        kind: edit.resource_kind,
        name: name.clone(),
        value: value.clone(),
        container: edit.container.clone(),
        is_init: edit.is_init,
        attribution: edit.attribution.clone(),
    };

    let write = match edit.resource_kind {
        ResourceKind::FunctionApp => EnvWrite::FunctionApp {
            name: name.clone(),
            value: value.clone(),
        },
        ResourceKind::ContainerApp => {
            let Some(container) = edit.container.clone() else {
                edit.error = Some("no target container resolved".into());
                state.env_var_edit = Some(edit);
                return;
            };
            EnvWrite::ContainerApp {
                target: crate::azure::container_app_env_update::EnvTarget {
                    container,
                    is_init: edit.is_init,
                    name: name.clone(),
                },
                value: value.clone(),
            }
        }
        _ => {
            edit.error = Some("this resource kind has no editable env vars".into());
            state.env_var_edit = Some(edit);
            return;
        }
    };

    edit.in_flight = true;
    edit.error = None;
    state.env_var_edit = Some(edit);
    spawn_env_var_write(auth.clone(), tx.clone(), applied, write);
}

/// Run the env-var write off the UI thread. In the demo tenant there's nothing
/// to call, so we report success immediately and let the optimistic cache
/// update stand (no refetch, which would wipe the simulated edit).
fn spawn_env_var_write(
    auth: AzureAuth,
    tx: UnboundedSender<AppEvent>,
    applied: AppliedEnvEdit,
    write: EnvWrite,
) {
    if auth.is_demo() {
        let _ = tx.send(AppEvent::EnvVarWriteCompleted {
            applied,
            is_demo: true,
            result: Ok(()),
        });
        return;
    }
    let resource_id = applied.resource_id.clone();
    tokio::spawn(async move {
        let result = match write {
            EnvWrite::FunctionApp { name, value } => {
                crate::azure::function_app_settings::update(&auth, &resource_id, &name, &value)
                    .await
            }
            EnvWrite::ContainerApp { target, value } => {
                crate::azure::container_app_env_update::update(&auth, &resource_id, &target, &value)
                    .await
            }
        }
        .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::EnvVarWriteCompleted {
            applied,
            is_demo: false,
            result,
        });
    });
}

/// Upsert `name=value` into an env-var list (used for both the Function App
/// write payload and the optimistic cache update). New entries keep the list
/// name-sorted to match the fetch path's ordering.
fn upsert_env(vars: &mut Vec<crate::azure::env_vars::EnvVar>, name: &str, value: &str) {
    if let Some(v) = vars.iter_mut().find(|v| v.name == name) {
        v.value = value.to_string();
    } else {
        vars.push(crate::azure::env_vars::EnvVar {
            name: name.to_string(),
            value: value.to_string(),
            ..Default::default()
        });
        vars.sort_by(|a, b| a.name.cmp(&b.name));
    }
}

/// Apply a just-confirmed edit to the in-memory cache so the env-vars page shows
/// the new value immediately, before the confirming refetch lands.
fn apply_env_edit_to_cache(state: &mut AppState, applied: &AppliedEnvEdit) {
    use crate::azure::resources::ResourceKind;
    match applied.kind {
        ResourceKind::FunctionApp => {
            let vars = state
                .func_settings
                .by_resource
                .entry(applied.resource_id.clone())
                .or_default();
            upsert_env(vars, &applied.name, &applied.value);
        }
        ResourceKind::ContainerApp => {
            if let Some(ov) = state
                .container_app_overview
                .by_resource
                .get_mut(&applied.resource_id)
            {
                let existing = ov.env_vars.iter_mut().find(|v| {
                    v.name == applied.name
                        && v.container == applied.container
                        && v.is_init == applied.is_init
                });
                if let Some(v) = existing {
                    v.value = applied.value.clone();
                } else {
                    ov.env_vars.push(crate::azure::env_vars::EnvVar {
                        name: applied.name.clone(),
                        value: applied.value.clone(),
                        is_secret: false,
                        attribution: applied.attribution.clone(),
                        container: applied.container.clone(),
                        is_init: applied.is_init,
                    });
                    // Match explode_container_env's name-then-container ordering.
                    ov.env_vars.sort_by(|a, b| {
                        a.name
                            .cmp(&b.name)
                            .then_with(|| a.attribution.cmp(&b.attribution))
                    });
                }
            }
        }
        _ => {}
    }
}

/// Re-pull the selected resource's env vars after a successful write to confirm
/// the server-side state (and pick up any normalization). Non-destructive: the
/// optimistic value stays visible until the fresh data overwrites it.
fn refetch_env_after_write(
    state: &mut AppState,
    applied: &AppliedEnvEdit,
    auth: &AzureAuth,
    tx: &UnboundedSender<AppEvent>,
) {
    use crate::azure::resources::ResourceKind;
    match applied.kind {
        ResourceKind::FunctionApp
            if state
                .func_settings
                .pending
                .insert(applied.resource_id.clone()) =>
        {
            spawn_load_function_app_settings(auth.clone(), applied.resource_id.clone(), tx.clone());
        }
        ResourceKind::ContainerApp
            if state
                .container_app_overview
                .pending
                .insert(applied.resource_id.clone()) =>
        {
            spawn_load_container_app_overview(
                auth.clone(),
                applied.resource_id.clone(),
                tx.clone(),
            );
        }
        _ => {}
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

fn spawn_input_reader(tx: UnboundedSender<AppEvent>, suspended: Arc<AtomicBool>) {
    use std::time::Duration;
    // Poll-with-timeout (rather than a bare blocking `read()`) so the thread can
    // observe `suspended` and stop touching the terminal while a shell-out child
    // owns it — otherwise it would steal the user's keystrokes and, once the
    // child is the foreground process group, get SIGTTIN-stopped on read.
    const POLL: Duration = Duration::from_millis(100);
    std::thread::spawn(move || {
        loop {
            if suspended.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(40));
                continue;
            }
            match crossterm::event::poll(POLL) {
                Ok(false) => continue, // timed out; re-check `suspended`
                Ok(true) => match crossterm::event::read() {
                    Ok(CtEvent::Key(k)) => {
                        if tx.send(AppEvent::Key(k)).is_err() {
                            break;
                        }
                    }
                    Ok(CtEvent::Paste(s)) => {
                        if tx.send(AppEvent::Paste(s)).is_err() {
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
                },
                Err(e) => {
                    tracing::warn!("crossterm::event::poll failed: {e}");
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

// Every spawn_load_* below starts with the same demo-mode short-circuit: in
// `azpect --demo` the canned dataset from `crate::azure::demo` is sent through
// the normal AppEvent so the rest of the app (caches, loading flags, views)
// behaves identically — just without any network. `AzureAuth::token` also
// refuses every scope in demo mode, so a path missed here fails closed
// instead of reaching a live tenant.

fn spawn_load_subscriptions(auth: AzureAuth, scope: u64, tx: UnboundedSender<AppEvent>) {
    if auth.is_demo() {
        let _ = tx.send(AppEvent::SubscriptionsLoaded {
            scope,
            result: Ok(crate::azure::demo::subscriptions()),
        });
        return;
    }
    tokio::spawn(async move {
        let result = crate::azure::subscriptions::list(&auth)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::SubscriptionsLoaded { scope, result });
    });
}

fn spawn_load_resources(
    auth: AzureAuth,
    scope: u64,
    sub_ids: Vec<String>,
    tx: UnboundedSender<AppEvent>,
) {
    if auth.is_demo() {
        let _ = tx.send(AppEvent::ResourcesLoaded {
            scope,
            result: Ok(crate::azure::demo::resources(&sub_ids)),
        });
        return;
    }
    tokio::spawn(async move {
        let result = crate::azure::resources::list(&auth, &sub_ids)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::ResourcesLoaded { scope, result });
    });
}

fn spawn_load_metrics(
    auth: AzureAuth,
    resource: Resource,
    range: TimeRange,
    tx: UnboundedSender<AppEvent>,
) {
    if auth.is_demo() {
        let _ = tx.send(AppEvent::MetricsLoaded {
            resource_id: resource.id.clone(),
            range,
            result: Ok(crate::azure::demo::metrics(&resource, range)),
        });
        return;
    }
    tokio::spawn(async move {
        let resource_id = resource.id.clone();
        let result = crate::azure::metrics::fetch(&auth, &resource, range)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::MetricsLoaded {
            resource_id,
            range,
            result,
        });
    });
}

fn spawn_load_health(
    auth: AzureAuth,
    resource_id: String,
    kind: crate::azure::resources::ResourceKind,
    tx: UnboundedSender<AppEvent>,
) {
    use crate::azure::resources::ResourceKind;
    if auth.is_demo() {
        // Mirror the real fan-out: availability (+ revision meta for Container
        // Apps) and the fixed-24h health metrics, all from the mock tenant.
        if kind == ResourceKind::ContainerApp {
            let info = crate::azure::demo::revision_info(&resource_id);
            let _ = tx.send(AppEvent::HealthLoaded {
                resource_id: resource_id.clone(),
                result: Ok(info.availability),
            });
            let _ = tx.send(AppEvent::ContainerAppRevisionMetaLoaded {
                resource_id: resource_id.clone(),
                result: Ok(info.active_revision),
            });
        } else {
            let _ = tx.send(AppEvent::HealthLoaded {
                resource_id: resource_id.clone(),
                result: Ok(crate::azure::demo::availability(&resource_id)),
            });
        }
        let _ = tx.send(AppEvent::HealthMetricsLoaded {
            resource_id: resource_id.clone(),
            result: Ok(crate::azure::demo::health_metrics(&resource_id, kind)),
        });
        return;
    }
    tokio::spawn(async move {
        // Hold a permit for the duration of the fan-out so no more than
        // HEALTH_FETCH_CONCURRENCY resources hit ARM/Monitor at once. The task is
        // already spawned; it just parks here until a slot frees. A closed gate
        // (never happens — we never close it) would drop the permit and proceed.
        let _permit = health_fetch_gate().acquire_owned().await;
        // Two independent signals feed the health badge, fetched concurrently:
        //   1. availability — the platform's up/degraded/down state
        //   2. health metrics — a fixed-24h Errors+Traffic window (range-agnostic)
        // `derive` combines them pessimistically (worst-of).
        let availability = {
            let auth = auth.clone();
            let resource_id = resource_id.clone();
            let tx = tx.clone();
            async move {
                // Container Apps don't expose meaningful state via the generic
                // Microsoft.ResourceHealth endpoint — it returns `Unknown` even
                // when active revisions are ActivationFailed/Unhealthy. The
                // revisions endpoint gives us both the authoritative availability
                // signal and the display metadata (active revision name, image,
                // replicas), so one fetch feeds two events.
                match kind {
                    ResourceKind::ContainerApp => {
                        match crate::azure::container_app_revisions::fetch(&auth, &resource_id)
                            .await
                        {
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
            }
        };
        let metrics = async move {
            let result = crate::azure::metrics::fetch_health(&auth, &resource_id, kind)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(AppEvent::HealthMetricsLoaded {
                resource_id,
                result,
            });
        };
        tokio::join!(availability, metrics);
    });
}

fn spawn_load_container_app_overview(
    auth: AzureAuth,
    resource_id: String,
    tx: UnboundedSender<AppEvent>,
) {
    if auth.is_demo() {
        let result = Ok(crate::azure::demo::container_app_overview(&resource_id));
        let _ = tx.send(AppEvent::ContainerAppOverviewLoaded {
            resource_id,
            result,
        });
        return;
    }
    tokio::spawn(async move {
        // Same gate as `spawn_load_health`: this fetch fans out per matching
        // resource (and auto-refresh re-fires it every interval), so ungated
        // it recreates exactly the ARM 429 burst the semaphore exists to
        // smooth.
        let _permit = health_fetch_gate().acquire_owned().await;
        let result = crate::azure::container_app_overview::fetch(&auth, &resource_id)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::ContainerAppOverviewLoaded {
            resource_id,
            result,
        });
    });
}

fn spawn_load_container_app_replicas(
    auth: AzureAuth,
    resource_id: String,
    revision_name: String,
    tx: UnboundedSender<AppEvent>,
) {
    if auth.is_demo() {
        let result = Ok(crate::azure::demo::replicas(&resource_id, &revision_name));
        let _ = tx.send(AppEvent::ContainerAppReplicasLoaded {
            resource_id,
            result,
        });
        return;
    }
    tokio::spawn(async move {
        // Gated like the overview fetch: every health fan-out re-emits
        // `ContainerAppRevisionMetaLoaded`, whose handler re-spawns this per
        // app — on the auto-refresh timer that's another per-resource burst.
        let _permit = health_fetch_gate().acquire_owned().await;
        let result =
            crate::azure::container_app_replicas::fetch(&auth, &resource_id, &revision_name)
                .await
                .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::ContainerAppReplicasLoaded {
            resource_id,
            result,
        });
    });
}

fn spawn_load_function_app_image(
    auth: AzureAuth,
    resource_id: String,
    tx: UnboundedSender<AppEvent>,
) {
    if auth.is_demo() {
        let result = Ok(crate::azure::demo::web_config(&resource_id));
        let _ = tx.send(AppEvent::FunctionAppImageLoaded {
            resource_id,
            result,
        });
        return;
    }
    tokio::spawn(async move {
        // Gated like the overview fetch: one `config/web` GET per Function App
        // on every list load *and* every auto-refresh tick.
        let _permit = health_fetch_gate().acquire_owned().await;
        let result = crate::azure::function_app_config::fetch(&auth, &resource_id)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::FunctionAppImageLoaded {
            resource_id,
            result,
        });
    });
}

fn spawn_load_function_app_settings(
    auth: AzureAuth,
    resource_id: String,
    tx: UnboundedSender<AppEvent>,
) {
    if auth.is_demo() {
        let result = Ok(crate::azure::demo::function_app_settings(&resource_id));
        let _ = tx.send(AppEvent::FunctionAppSettingsLoaded {
            resource_id,
            result,
        });
        return;
    }
    tokio::spawn(async move {
        let result = crate::azure::function_app_settings::fetch(&auth, &resource_id)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::FunctionAppSettingsLoaded {
            resource_id,
            result,
        });
    });
}

fn spawn_load_function_app_triggers(
    auth: AzureAuth,
    resource_id: String,
    tx: UnboundedSender<AppEvent>,
) {
    if auth.is_demo() {
        let result = Ok(crate::azure::demo::function_app_triggers(&resource_id));
        let _ = tx.send(AppEvent::FunctionAppTriggersLoaded {
            resource_id,
            result,
        });
        return;
    }
    tokio::spawn(async move {
        let result = crate::azure::function_app_triggers::fetch(&auth, &resource_id)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::FunctionAppTriggersLoaded {
            resource_id,
            result,
        });
    });
}

fn spawn_resolve_principal(auth: AzureAuth, object_id: String, tx: UnboundedSender<AppEvent>) {
    if auth.is_demo() {
        let result = Ok(crate::azure::demo::principal_display_name(&object_id));
        let _ = tx.send(AppEvent::PrincipalResolved { object_id, result });
        return;
    }
    tokio::spawn(async move {
        let result = crate::azure::principals::resolve_display_name(&auth, &object_id)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::PrincipalResolved { object_id, result });
    });
}

/// If `(by, by_type)` names a directory principal worth resolving via Graph —
/// an `Application` / `ManagedIdentity` author, whose `by` is a GUID object-id —
/// return that object-id. `User` authors are already UPNs and need no lookup.
fn principal_to_resolve<'a>(by: Option<&'a str>, by_type: Option<&str>) -> Option<&'a str> {
    match (by, by_type) {
        (Some(id), Some("Application" | "ManagedIdentity")) if looks_like_guid(id) => Some(id),
        _ => None,
    }
}

/// Cheap shape check for a canonical GUID (`8-4-4-4-12` hex) so we never fire a
/// Graph request at a malformed author value.
fn looks_like_guid(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

/// Kick off a Container App template fetch for every Container App that
/// doesn't already have cached limits. Same eager-on-load pattern as
/// `spawn_missing_list_health`.
fn spawn_missing_container_app_overview(
    state: &mut AppState,
    auth: &AzureAuth,
    tx: &UnboundedSender<AppEvent>,
    force: bool,
) {
    use crate::azure::resources::ResourceKind;
    let to_fetch: Vec<String> = state
        .resources
        .iter()
        .filter(|r| r.kind == ResourceKind::ContainerApp)
        .filter(|r| {
            (force || !state.container_app_overview.by_resource.contains_key(&r.id))
                && !state.container_app_overview.pending.contains(&r.id)
        })
        .map(|r| r.id.clone())
        .collect();
    for resource_id in to_fetch {
        state
            .container_app_overview
            .pending
            .insert(resource_id.clone());
        spawn_load_container_app_overview(auth.clone(), resource_id, tx.clone());
    }
}

/// Kick off a `config/web` fetch for every Function App that doesn't already
/// have a cached deployed image. Feeds the list's VERSION column; same
/// eager-on-load pattern as `spawn_missing_container_app_overview` (Container
/// Apps get their image for free off the health fetch, so they're skipped here).
fn spawn_missing_function_app_image(
    state: &mut AppState,
    auth: &AzureAuth,
    tx: &UnboundedSender<AppEvent>,
    force: bool,
) {
    use crate::azure::resources::ResourceKind;
    let to_fetch: Vec<String> = state
        .resources
        .iter()
        .filter(|r| r.kind == ResourceKind::FunctionApp)
        .filter(|r| {
            (force || !state.func_image.by_resource.contains_key(&r.id))
                && !state.func_image.pending.contains(&r.id)
        })
        .map(|r| r.id.clone())
        .collect();
    for resource_id in to_fetch {
        state.func_image.pending.insert(resource_id.clone());
        spawn_load_function_app_image(auth.clone(), resource_id, tx.clone());
    }
}

/// Kick off a Resource Health fetch for every loaded resource that doesn't
/// already have one cached or in flight. Mirrors `spawn_missing_list_metrics`.
fn spawn_missing_list_health(
    state: &mut AppState,
    auth: &AzureAuth,
    tx: &UnboundedSender<AppEvent>,
    force: bool,
) {
    let to_fetch: Vec<(String, crate::azure::resources::ResourceKind)> = state
        .resources
        .iter()
        .filter(|r| {
            (force || !state.health.by_resource.contains_key(&r.id))
                && !state.health.pending.contains(&r.id)
        })
        .map(|r| (r.id.clone(), r.kind))
        .collect();
    for (resource_id, kind) in to_fetch {
        state.health.pending.insert(resource_id.clone());
        spawn_load_health(auth.clone(), resource_id, kind, tx.clone());
    }
}

fn spawn_load_apim_apis(auth: AzureAuth, service_id: String, tx: UnboundedSender<AppEvent>) {
    if auth.is_demo() {
        let result = Ok(crate::azure::demo::apim_apis(&service_id));
        let _ = tx.send(AppEvent::ApimApisLoaded { service_id, result });
        return;
    }
    tokio::spawn(async move {
        let result = crate::azure::apim::list_apis(&auth, &service_id)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::ApimApisLoaded { service_id, result });
    });
}

fn spawn_load_apim_operations(auth: AzureAuth, api_id: String, tx: UnboundedSender<AppEvent>) {
    if auth.is_demo() {
        let result = Ok(crate::azure::demo::apim_operations(&api_id));
        let _ = tx.send(AppEvent::ApimOperationsLoaded { api_id, result });
        return;
    }
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
    if auth.is_demo() {
        let result = Ok(crate::azure::demo::apim_operation_policy(&operation_id));
        let _ = tx.send(AppEvent::ApimOperationPolicyLoaded {
            operation_id,
            result,
        });
        return;
    }
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
    if auth.is_demo() {
        let result = Ok(crate::azure::demo::appgw_backends(&resource_id));
        let _ = tx.send(AppEvent::AppGatewayBackendsLoaded {
            resource_id,
            result,
        });
        return;
    }
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
    scope: u64,
    sub_ids: Vec<String>,
    tx: UnboundedSender<AppEvent>,
) {
    if auth.is_demo() {
        let _ = tx.send(AppEvent::StorageAccountsLoaded {
            scope,
            result: Ok(crate::azure::demo::storage_accounts(&sub_ids)),
        });
        return;
    }
    tokio::spawn(async move {
        let result = crate::azure::storage::list_accounts(&auth, &sub_ids)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::StorageAccountsLoaded { scope, result });
    });
}

fn spawn_load_storage_containers(
    auth: AzureAuth,
    account: crate::azure::storage::StorageAccount,
    tx: UnboundedSender<AppEvent>,
) {
    if auth.is_demo() {
        let _ = tx.send(AppEvent::StorageContainersLoaded {
            account_id: account.id.clone(),
            result: Ok(crate::azure::demo::storage_containers(&account)),
        });
        return;
    }
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
    if auth.is_demo() {
        let _ = tx.send(AppEvent::StorageOverviewLoaded {
            account_id: account.id.clone(),
            result: Ok(crate::azure::demo::storage_overview(&account)),
        });
        return;
    }
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
    if auth.is_demo() {
        let key = crate::ui::state::StorageCache::blobs_key(&account_name, &container);
        let result = Ok(crate::azure::demo::storage_blobs(&account_name, &container));
        let _ = tx.send(AppEvent::StorageBlobsLoaded { key, result });
        return;
    }
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
    if auth.is_demo() {
        let key =
            crate::ui::state::StorageCache::blob_preview_key(&account_name, &container, &blob);
        let result = Ok(crate::azure::demo::blob_preview(
            &account_name,
            &container,
            &blob,
        ));
        let _ = tx.send(AppEvent::StorageBlobPreviewLoaded { key, result });
        return;
    }
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

fn spawn_load_registries(
    auth: AzureAuth,
    scope: u64,
    sub_ids: Vec<String>,
    tx: UnboundedSender<AppEvent>,
) {
    if auth.is_demo() {
        let _ = tx.send(AppEvent::RegistriesLoaded {
            scope,
            result: Ok(crate::azure::demo::registries(&sub_ids)),
        });
        return;
    }
    tokio::spawn(async move {
        let result = crate::azure::registries::list_registries(&auth, &sub_ids)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::RegistriesLoaded { scope, result });
    });
}

fn spawn_load_repositories(
    auth: AzureAuth,
    registry: crate::azure::registries::Registry,
    tx: UnboundedSender<AppEvent>,
) {
    if auth.is_demo() {
        let _ = tx.send(AppEvent::RegistryRepositoriesLoaded {
            registry_id: registry.id.clone(),
            result: Ok(crate::azure::demo::repositories(&registry)),
        });
        return;
    }
    tokio::spawn(async move {
        let registry_id = registry.id.clone();
        let result = crate::azure::registries::list_repositories(&auth, &registry)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::RegistryRepositoriesLoaded {
            registry_id,
            result,
        });
    });
}

fn spawn_load_tags(
    auth: AzureAuth,
    registry: crate::azure::registries::Registry,
    repository: String,
    tx: UnboundedSender<AppEvent>,
) {
    if auth.is_demo() {
        let key = crate::ui::state::RegistryCache::tags_key(&registry.id, &repository);
        let result = Ok(crate::azure::demo::tags(&repository));
        let _ = tx.send(AppEvent::RegistryTagsLoaded { key, result });
        return;
    }
    tokio::spawn(async move {
        let key = crate::ui::state::RegistryCache::tags_key(&registry.id, &repository);
        let result = crate::azure::registries::list_tags(&auth, &registry, &repository)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::RegistryTagsLoaded { key, result });
    });
}

fn spawn_load_sql_resources(
    auth: AzureAuth,
    scope: u64,
    sub_ids: Vec<String>,
    tx: UnboundedSender<AppEvent>,
) {
    if auth.is_demo() {
        let _ = tx.send(AppEvent::SqlResourcesLoaded {
            scope,
            result: Ok(crate::azure::demo::sql_resources(&sub_ids)),
        });
        return;
    }
    tokio::spawn(async move {
        let result = crate::azure::sql::list_sql_resources(&auth, &sub_ids)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::SqlResourcesLoaded { scope, result });
    });
}

fn spawn_load_sql_metrics(
    auth: AzureAuth,
    resource_id: String,
    range: TimeRange,
    tx: UnboundedSender<AppEvent>,
) {
    if auth.is_demo() {
        let _ = tx.send(AppEvent::SqlMetricsLoaded {
            resource_id: resource_id.clone(),
            range,
            result: Ok(crate::azure::demo::sql_metrics(&resource_id, range)),
        });
        return;
    }
    tokio::spawn(async move {
        let result = crate::azure::sql::fetch_metrics(&auth, &resource_id, range)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::SqlMetricsLoaded {
            resource_id,
            range,
            result,
        });
    });
}

fn spawn_load_cosmos_accounts(
    auth: AzureAuth,
    scope: u64,
    sub_ids: Vec<String>,
    tx: UnboundedSender<AppEvent>,
) {
    if auth.is_demo() {
        let _ = tx.send(AppEvent::CosmosAccountsLoaded {
            scope,
            result: Ok(crate::azure::demo::cosmos_accounts(&sub_ids)),
        });
        return;
    }
    tokio::spawn(async move {
        let result = crate::azure::cosmos::list_accounts(&auth, &sub_ids)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::CosmosAccountsLoaded { scope, result });
    });
}

fn spawn_load_cosmos_databases(
    auth: AzureAuth,
    account: crate::azure::cosmos::CosmosAccount,
    tx: UnboundedSender<AppEvent>,
) {
    if auth.is_demo() {
        let _ = tx.send(AppEvent::CosmosDatabasesLoaded {
            account_id: account.id.clone(),
            result: Ok(crate::azure::demo::cosmos_databases(&account)),
        });
        return;
    }
    tokio::spawn(async move {
        let account_id = account.id.clone();
        let result = crate::azure::cosmos::list_databases(&auth, &account)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::CosmosDatabasesLoaded { account_id, result });
    });
}

fn spawn_load_cosmos_containers(
    auth: AzureAuth,
    account: crate::azure::cosmos::CosmosAccount,
    db: String,
    tx: UnboundedSender<AppEvent>,
) {
    if auth.is_demo() {
        let key = crate::ui::state::CosmosCache::containers_key(&account.id, &db);
        let result = Ok(crate::azure::demo::cosmos_containers(&account, &db));
        let _ = tx.send(AppEvent::CosmosContainersLoaded { key, result });
        return;
    }
    tokio::spawn(async move {
        let key = crate::ui::state::CosmosCache::containers_key(&account.id, &db);
        let result = crate::azure::cosmos::list_containers(&auth, &account, &db)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::CosmosContainersLoaded { key, result });
    });
}

fn spawn_load_cosmos_items(
    auth: AzureAuth,
    account: crate::azure::cosmos::CosmosAccount,
    db: String,
    coll: String,
    tx: UnboundedSender<AppEvent>,
) {
    if auth.is_demo() {
        let key = crate::ui::state::CosmosCache::items_key(&account.id, &db, &coll);
        let result = Ok(crate::azure::demo::cosmos_items(&coll));
        let _ = tx.send(AppEvent::CosmosItemsLoaded { key, result });
        return;
    }
    tokio::spawn(async move {
        let key = crate::ui::state::CosmosCache::items_key(&account.id, &db, &coll);
        let result = crate::azure::cosmos::query_top_items(&auth, &account, &db, &coll)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::CosmosItemsLoaded { key, result });
    });
}

fn spawn_load_key_vaults(
    auth: AzureAuth,
    scope: u64,
    sub_ids: Vec<String>,
    tx: UnboundedSender<AppEvent>,
) {
    if auth.is_demo() {
        let _ = tx.send(AppEvent::KeyVaultsLoaded {
            scope,
            result: Ok(crate::azure::demo::key_vaults(&sub_ids)),
        });
        return;
    }
    tokio::spawn(async move {
        let result = crate::azure::key_vault::list_vaults(&auth, &sub_ids)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::KeyVaultsLoaded { scope, result });
    });
}

fn spawn_load_key_vault_items(
    auth: AzureAuth,
    vault: crate::azure::key_vault::KeyVault,
    kind: crate::azure::key_vault::ItemKind,
    tx: UnboundedSender<AppEvent>,
) {
    if auth.is_demo() {
        let key = crate::ui::state::KeyVaultCache::items_key(&vault.id, kind);
        let result = Ok(crate::azure::demo::key_vault_items(kind));
        let _ = tx.send(AppEvent::KeyVaultItemsLoaded { key, result });
        return;
    }
    tokio::spawn(async move {
        let key = crate::ui::state::KeyVaultCache::items_key(&vault.id, kind);
        let result = crate::azure::key_vault::list_items(&auth, &vault, kind)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::KeyVaultItemsLoaded { key, result });
    });
}

/// Fetch one page of Key Vault `AuditEvent` rows for the access-logs view.
/// `generation` is the query-scope token the result is validated against on
/// landing (see `AppEvent::KeyVaultAccessLoaded`).
fn spawn_load_key_vault_access(
    auth: AzureAuth,
    vault: crate::azure::key_vault::KeyVault,
    window: crate::azure::key_vault_logs::AccessWindow,
    scope: Option<crate::azure::key_vault_logs::ItemScope>,
    exclude_self: bool,
    generation: u64,
    tx: UnboundedSender<AppEvent>,
) {
    if auth.is_demo() {
        let _ = tx.send(AppEvent::KeyVaultAccessLoaded {
            generation,
            result: Ok(crate::azure::demo::key_vault_access(
                &window,
                scope.as_ref(),
                exclude_self,
            )),
        });
        return;
    }
    tokio::spawn(async move {
        let result = crate::azure::key_vault_logs::fetch(
            &auth,
            &vault,
            &window,
            scope.as_ref(),
            exclude_self,
        )
        .await
        .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::KeyVaultAccessLoaded { generation, result });
    });
}

/// Fetch the SQL audit principal roll-up. `generation` is the query-scope
/// token the result is validated against on landing (see
/// `AppEvent::SqlAuditPrincipalsLoaded`).
fn spawn_load_sql_audit_principals(
    auth: AzureAuth,
    target: crate::azure::sql_audit::AuditTarget,
    window: crate::azure::key_vault_logs::AccessWindow,
    generation: u64,
    tx: UnboundedSender<AppEvent>,
) {
    if auth.is_demo() {
        let _ = tx.send(AppEvent::SqlAuditPrincipalsLoaded {
            generation,
            result: Ok(crate::azure::demo::sql_audit_principals(&window, &target)),
        });
        return;
    }
    tokio::spawn(async move {
        let result = crate::azure::sql_audit::fetch_principals(&auth, &target, &window)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::SqlAuditPrincipalsLoaded { generation, result });
    });
}

/// Fetch one page of audit events for the pinned principal. `before` +
/// `append` carry the scroll-past-bottom pagination (see
/// `fetch_older_sql_audit_events`).
#[allow(clippy::too_many_arguments)]
fn spawn_load_sql_audit_events(
    auth: AzureAuth,
    target: crate::azure::sql_audit::AuditTarget,
    window: crate::azure::key_vault_logs::AccessWindow,
    principal: String,
    errors_only: bool,
    before: Option<chrono::DateTime<chrono::Utc>>,
    append: bool,
    generation: u64,
    tx: UnboundedSender<AppEvent>,
) {
    if auth.is_demo() {
        let _ = tx.send(AppEvent::SqlAuditEventsLoaded {
            generation,
            append,
            result: Ok(crate::azure::demo::sql_audit_events(
                &window,
                &principal,
                errors_only,
            )),
        });
        return;
    }
    tokio::spawn(async move {
        let result = crate::azure::sql_audit::fetch_events(
            &auth,
            &target,
            &window,
            &principal,
            errors_only,
            before,
        )
        .await
        .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::SqlAuditEventsLoaded {
            generation,
            append,
            result,
        });
    });
}

/// Fetch the audited database's user list (⚠ live T-SQL) for the roll-up's
/// silent-users merge. `enabled` is the `sql_live_queries` config flag —
/// `sql_tds` fails closed on it before opening any socket.
fn spawn_load_sql_audit_db_users(
    auth: AzureAuth,
    server: String,
    database: String,
    enabled: bool,
    generation: u64,
    tx: UnboundedSender<AppEvent>,
) {
    if auth.is_demo() {
        let _ = tx.send(AppEvent::SqlAuditDbUsersLoaded {
            generation,
            result: Ok(crate::azure::demo::sql_db_users()),
        });
        return;
    }
    tokio::spawn(async move {
        let result = crate::azure::sql_tds::fetch_db_users(&auth, &server, &database, enabled)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::SqlAuditDbUsersLoaded { generation, result });
    });
}

/// Query open sessions on the pinned SQL target (⚠ live T-SQL).
fn spawn_load_sql_sessions(
    auth: AzureAuth,
    target: crate::azure::sql_audit::AuditTarget,
    enabled: bool,
    generation: u64,
    tx: UnboundedSender<AppEvent>,
) {
    if auth.is_demo() {
        let _ = tx.send(AppEvent::SqlSessionsLoaded {
            generation,
            result: Ok(crate::azure::demo::sql_sessions()),
        });
        return;
    }
    tokio::spawn(async move {
        let result = crate::azure::sql_tds::fetch_sessions(
            &auth,
            &target.server,
            target.database.as_deref(),
            enabled,
        )
        .await
        .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::SqlSessionsLoaded { generation, result });
    });
}

/// Fetch one secret's plaintext value for the reveal modal. Carries the
/// `(vault_id, name)` back so the handler can ignore the result if the modal
/// has since closed or moved to another secret.
fn spawn_load_key_vault_secret_value(
    auth: AzureAuth,
    vault: crate::azure::key_vault::KeyVault,
    name: String,
    tx: UnboundedSender<AppEvent>,
) {
    if auth.is_demo() {
        let result = Ok(crate::azure::demo::key_vault_secret_value(&name));
        let _ = tx.send(AppEvent::KeyVaultSecretValueLoaded {
            vault_id: vault.id,
            name,
            result,
        });
        return;
    }
    tokio::spawn(async move {
        let result = crate::azure::key_vault::get_secret_value(&auth, &vault, &name)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::KeyVaultSecretValueLoaded {
            vault_id: vault.id,
            name,
            result,
        });
    });
}

fn spawn_load_sb_namespaces(
    auth: AzureAuth,
    scope: u64,
    sub_ids: Vec<String>,
    tx: UnboundedSender<AppEvent>,
) {
    if auth.is_demo() {
        let _ = tx.send(AppEvent::ServiceBusNamespacesLoaded {
            scope,
            result: Ok(crate::azure::demo::sb_namespaces(&sub_ids)),
        });
        return;
    }
    tokio::spawn(async move {
        let result = crate::azure::service_bus::list_namespaces(&auth, &sub_ids)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::ServiceBusNamespacesLoaded { scope, result });
    });
}

fn spawn_load_sb_queues(
    auth: AzureAuth,
    namespace: crate::azure::service_bus::ServiceBusNamespace,
    tx: UnboundedSender<AppEvent>,
) {
    if auth.is_demo() {
        let _ = tx.send(AppEvent::ServiceBusQueuesLoaded {
            namespace_id: namespace.id.clone(),
            result: Ok(crate::azure::demo::sb_queues(&namespace)),
        });
        return;
    }
    tokio::spawn(async move {
        let namespace_id = namespace.id.clone();
        let result = crate::azure::service_bus::list_queues(&auth, &namespace)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::ServiceBusQueuesLoaded {
            namespace_id,
            result,
        });
    });
}

fn spawn_load_sb_topics(
    auth: AzureAuth,
    namespace: crate::azure::service_bus::ServiceBusNamespace,
    tx: UnboundedSender<AppEvent>,
) {
    if auth.is_demo() {
        let _ = tx.send(AppEvent::ServiceBusTopicsLoaded {
            namespace_id: namespace.id.clone(),
            result: Ok(crate::azure::demo::sb_topics(&namespace)),
        });
        return;
    }
    tokio::spawn(async move {
        let namespace_id = namespace.id.clone();
        let result = crate::azure::service_bus::list_topics(&auth, &namespace)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::ServiceBusTopicsLoaded {
            namespace_id,
            result,
        });
    });
}

fn spawn_load_sb_subscriptions(
    auth: AzureAuth,
    namespace: crate::azure::service_bus::ServiceBusNamespace,
    topic: String,
    tx: UnboundedSender<AppEvent>,
) {
    if auth.is_demo() {
        let key = crate::ui::state::ServiceBusCache::subscriptions_key(&namespace.id, &topic);
        let result = Ok(crate::azure::demo::sb_subscriptions(&namespace, &topic));
        let _ = tx.send(AppEvent::ServiceBusSubscriptionsLoaded { key, result });
        return;
    }
    tokio::spawn(async move {
        let key = crate::ui::state::ServiceBusCache::subscriptions_key(&namespace.id, &topic);
        let result = crate::azure::service_bus::list_subscriptions(&auth, &namespace, &topic)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::ServiceBusSubscriptionsLoaded { key, result });
    });
}

#[allow(clippy::too_many_arguments)]
fn spawn_load_logs(
    auth: AzureAuth,
    resource: Resource,
    range: TimeRange,
    errors_only: bool,
    older_than: Option<chrono::DateTime<chrono::Utc>>,
    around: Option<chrono::DateTime<chrono::Utc>>,
    generation: u64,
    tx: UnboundedSender<AppEvent>,
) {
    let append = older_than.is_some();
    if auth.is_demo() {
        let _ = tx.send(AppEvent::LogsLoaded {
            resource_id: resource.id.clone(),
            append,
            generation,
            result: Ok(crate::azure::demo::logs(
                &resource,
                range,
                errors_only,
                older_than,
                around,
            )),
        });
        return;
    }
    tokio::spawn(async move {
        let resource_id = resource.id.clone();
        let result =
            crate::azure::logs::fetch(&auth, &resource, range, errors_only, older_than, around)
                .await
                .map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::LogsLoaded {
            resource_id,
            generation,
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

    fn log_line(off: i64, msg: &str) -> crate::azure::logs::LogLine {
        crate::azure::logs::LogLine {
            ts: chrono::Utc::now() - chrono::Duration::minutes(off),
            level: crate::azure::logs::LogLevel::Info,
            source: "app".into(),
            message: msg.into(),
            fields: Vec::new(),
        }
    }

    fn state_with_log_resource() -> (AppState, String) {
        let mut state = fresh_state();
        state.resources = vec![Resource {
            id: "/r/one".into(),
            name: "alpha".into(),
            kind: crate::azure::resources::ResourceKind::ContainerApp,
            location: "westeurope".into(),
            resource_group: "rg-demo".into(),
            subscription_id: "sub".into(),
            state: Some("Running".into()),
            created_at: None,
            modified_at: None,
            meta: Default::default(),
        }];
        state.list_cursor = 0;
        (state, "/r/one".into())
    }

    #[test]
    fn pending_anchor_chains_fetch_until_its_timestamp_is_covered() {
        let (mut state, id) = state_with_log_resource();
        // Anchor at t-10m, but the landed page only reaches back to t-3m — the
        // busy window held more rows than one page. Resolving now would snap
        // the cursor to a later row.
        let anchored = log_line(10, "the error");
        state.logs.pending_anchor = Some(crate::ui::state::LineAnchor::of(&anchored));
        state.logs.by_resource.insert(
            id.clone(),
            vec![log_line(1, "newest"), log_line(3, "later")],
        );
        state.logs.more_available.insert(id.clone(), true);

        resolve_pending_anchor(&mut state, &id);
        assert!(state.logs.pending_anchor.is_some(), "anchor stays pending");
        assert!(state.logs.fetch_more_requested, "chains an older page");
        assert_eq!(state.logs.scroll, 0, "cursor untouched until covered");

        // The chained page reaches past the anchor: now it resolves and centers.
        state
            .logs
            .by_resource
            .get_mut(&id)
            .unwrap()
            .extend([anchored, log_line(12, "older")]);
        state.logs.fetch_more_requested = false;
        resolve_pending_anchor(&mut state, &id);
        assert!(state.logs.pending_anchor.is_none());
        assert_eq!(state.logs.scroll, 2, "cursor on the anchored line");
        assert!(state.logs.center_pending.get());
        assert!(!state.logs.fetch_more_requested);
    }

    #[test]
    fn pending_anchor_falls_back_to_nearest_when_window_is_exhausted() {
        let (mut state, id) = state_with_log_resource();
        let anchored = log_line(10, "the error");
        state.logs.pending_anchor = Some(crate::ui::state::LineAnchor::of(&anchored));
        state.logs.by_resource.insert(
            id.clone(),
            vec![log_line(1, "newest"), log_line(3, "later")],
        );
        state.logs.more_available.insert(id.clone(), false);

        resolve_pending_anchor(&mut state, &id);
        // No older rows exist, so nearest-by-time is the best we can do.
        assert!(state.logs.pending_anchor.is_none());
        assert_eq!(state.logs.scroll, 1);
        assert!(!state.logs.fetch_more_requested);
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
    fn demo_edit_flow_commits_and_updates_cache() {
        use crate::azure::env_vars::EnvVar;
        use crate::azure::resources::{Resource, ResourceKind};

        let auth = AzureAuth::demo();
        let (tx, mut rx) = mpsc::unbounded_channel::<AppEvent>();

        let mut state = fresh_state();
        state.resources = vec![Resource {
            id: "/r/fn".into(),
            name: "func".into(),
            kind: ResourceKind::FunctionApp,
            location: "we".into(),
            resource_group: "rg".into(),
            subscription_id: "s".into(),
            state: Some("Running".into()),
            created_at: None,
            modified_at: None,
            meta: Default::default(),
        }];
        state.list_cursor = 0;
        state.view = View::EnvVars;
        state.func_settings.by_resource.insert(
            "/r/fn".into(),
            vec![EnvVar {
                name: "API_KEY".into(),
                value: "old".into(),
                ..Default::default()
            }],
        );

        // Ctrl+E opens the editor seeded with the selected var.
        assert!(crate::ui::views::env_vars::handle(
            Action::EditEnvVar,
            &mut state
        ));
        // Type a new value into the (focused) value field.
        state.env_var_edit.as_mut().unwrap().value =
            tui_input::Input::default().with_value("new".into());

        // Enter advances Editing -> Confirming (guarded, no write yet).
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        handle_env_var_edit_key(&mut state, enter, &auth, &tx);
        assert_eq!(
            state.env_var_edit.as_ref().unwrap().phase,
            EnvVarEditPhase::Confirming
        );
        assert!(
            rx.try_recv().is_err(),
            "no write should fire before confirm"
        );

        // `y` confirms; the demo spawn reports success synchronously.
        handle_env_var_edit_key(&mut state, k('y'), &auth, &tx);
        match rx.try_recv().expect("completion event") {
            AppEvent::EnvVarWriteCompleted {
                applied,
                is_demo,
                result,
            } => {
                assert!(is_demo);
                assert!(result.is_ok());
                assert_eq!(applied.name, "API_KEY");
                assert_eq!(applied.value, "new");
                apply_env_edit_to_cache(&mut state, &applied);
            }
            _ => panic!("expected EnvVarWriteCompleted"),
        }
        let vars = &state.func_settings.by_resource["/r/fn"];
        assert_eq!(
            vars.iter().find(|v| v.name == "API_KEY").unwrap().value,
            "new"
        );
    }

    #[test]
    fn enter_on_kv_ref_env_var_jumps_to_vault_and_reveals_secret() {
        use crate::azure::env_vars::EnvVar;
        use crate::azure::resources::{Resource, ResourceKind};

        let auth = AzureAuth::demo();
        let (tx, mut rx) = mpsc::unbounded_channel::<AppEvent>();

        let mut state = fresh_state();
        state.resources = vec![Resource {
            id: "/r/fn".into(),
            name: "func".into(),
            kind: ResourceKind::FunctionApp,
            location: "we".into(),
            resource_group: "rg".into(),
            subscription_id: "s".into(),
            state: Some("Running".into()),
            created_at: None,
            modified_at: None,
            meta: Default::default(),
        }];
        state.list_cursor = 0;
        state.view = View::EnvVars;
        state.func_settings.by_resource.insert(
            "/r/fn".into(),
            vec![EnvVar {
                name: "ApiKey".into(),
                value: "@Microsoft.KeyVault(SecretUri=https://myvault.vault.azure.net/secrets/api-key/)"
                    .into(),
                is_secret: true,
                ..Default::default()
            }],
        );
        state.env_vars_view.cursor = 0;

        open_key_vault_ref_from_env_var(&mut state, &auth, &tx);

        // Navigated to the vault's secrets list with the modal open on the
        // referenced secret.
        assert_eq!(state.view, View::KeyVaultItems);
        let vault = state
            .key_vault
            .selected_vault
            .as_ref()
            .expect("vault pinned");
        assert_eq!(vault.name, "myvault");
        let modal = state.key_vault.secret_modal.as_ref().expect("modal open");
        assert_eq!(modal.name, "api-key");

        // The demo fetch reports a value synchronously; applying it loads the
        // modal rather than leaking the secret into the items cache.
        match rx.try_recv().expect("secret value event") {
            AppEvent::KeyVaultSecretValueLoaded {
                vault_id,
                name,
                result,
            } => {
                assert_eq!(name, "api-key");
                assert_eq!(vault_id, vault.id);
                assert!(result.is_ok());
            }
            _ => panic!("expected KeyVaultSecretValueLoaded"),
        }
    }

    #[test]
    fn enter_on_container_secret_ref_follows_key_vault_url() {
        use crate::azure::container_app_overview::{ContainerAppOverview, ContainerAppSecret};
        use crate::azure::env_vars::EnvVar;
        use crate::azure::resources::{Resource, ResourceKind};

        let auth = AzureAuth::demo();
        let (tx, mut rx) = mpsc::unbounded_channel::<AppEvent>();

        let mut state = fresh_state();
        state.resources = vec![Resource {
            id: "/r/ca".into(),
            name: "app".into(),
            kind: ResourceKind::ContainerApp,
            location: "we".into(),
            resource_group: "rg".into(),
            subscription_id: "s".into(),
            state: Some("Running".into()),
            created_at: None,
            modified_at: None,
            meta: Default::default(),
        }];
        state.list_cursor = 0;
        state.view = View::EnvVars;
        // A `secretRef` env var (display shape from `from_container_env`) whose
        // app secret is Key Vault-backed.
        state.container_app_overview.by_resource.insert(
            "/r/ca".into(),
            ContainerAppOverview {
                env_vars: vec![EnvVar {
                    name: "DB".into(),
                    value: "(secret: db-conn)".into(),
                    is_secret: true,
                    attribution: Some("app".into()),
                    container: Some("app".into()),
                    ..Default::default()
                }],
                secrets: vec![ContainerAppSecret {
                    name: "db-conn".into(),
                    key_vault_url: Some("https://myvault.vault.azure.net/secrets/db-conn".into()),
                }],
                ..Default::default()
            },
        );
        state.env_vars_view.cursor = 0;

        open_key_vault_ref_from_env_var(&mut state, &auth, &tx);

        assert_eq!(state.view, View::KeyVaultItems);
        assert_eq!(
            state
                .key_vault
                .selected_vault
                .as_ref()
                .map(|v| v.name.as_str()),
            Some("myvault")
        );
        let modal = state.key_vault.secret_modal.as_ref().expect("modal open");
        assert_eq!(modal.name, "db-conn");
        match rx.try_recv().expect("secret value event") {
            AppEvent::KeyVaultSecretValueLoaded { name, result, .. } => {
                assert_eq!(name, "db-conn");
                assert!(result.is_ok());
            }
            _ => panic!("expected KeyVaultSecretValueLoaded"),
        }
    }

    #[test]
    fn enter_on_plain_in_app_container_secret_hints_instead_of_navigating() {
        use crate::azure::container_app_overview::{ContainerAppOverview, ContainerAppSecret};
        use crate::azure::env_vars::EnvVar;
        use crate::azure::resources::{Resource, ResourceKind};

        let auth = AzureAuth::demo();
        let (tx, _rx) = mpsc::unbounded_channel::<AppEvent>();

        let mut state = fresh_state();
        state.resources = vec![Resource {
            id: "/r/ca".into(),
            name: "app".into(),
            kind: ResourceKind::ContainerApp,
            location: "we".into(),
            resource_group: "rg".into(),
            subscription_id: "s".into(),
            state: Some("Running".into()),
            created_at: None,
            modified_at: None,
            meta: Default::default(),
        }];
        state.list_cursor = 0;
        state.view = View::EnvVars;
        state.container_app_overview.by_resource.insert(
            "/r/ca".into(),
            ContainerAppOverview {
                env_vars: vec![EnvVar {
                    name: "DB".into(),
                    value: "(secret: db-conn)".into(),
                    is_secret: true,
                    ..Default::default()
                }],
                // Plain in-app secret: no keyVaultUrl to follow.
                secrets: vec![ContainerAppSecret {
                    name: "db-conn".into(),
                    key_vault_url: None,
                }],
                ..Default::default()
            },
        );
        state.env_vars_view.cursor = 0;

        open_key_vault_ref_from_env_var(&mut state, &auth, &tx);
        // No navigation; a hint is posted instead.
        assert_eq!(state.view, View::EnvVars);
        assert!(state.key_vault.secret_modal.is_none());
        assert!(state.status_message.is_some());
    }

    #[test]
    fn s_on_container_app_queues_shell_targeting_running_replica() {
        use crate::azure::container_app_replicas::{ReplicaContainer, ReplicaInstance};
        use crate::azure::container_app_revisions::ActiveRevisionMeta;
        use crate::azure::resources::{Resource, ResourceKind};
        use chrono::{Duration, Utc};

        let mut state = fresh_state();
        state.resources = vec![Resource {
            id: "/r/ca".into(),
            name: "ca-app".into(),
            kind: ResourceKind::ContainerApp,
            location: "we".into(),
            resource_group: "rg-x".into(),
            subscription_id: "sub-1".into(),
            state: Some("Running".into()),
            created_at: None,
            modified_at: None,
            meta: Default::default(),
        }];
        state.list_cursor = 0;
        state.view = View::Detail;
        state.revision_meta.by_resource.insert(
            "/r/ca".into(),
            ActiveRevisionMeta {
                name: "ca-app--0000004".into(),
                ..Default::default()
            },
        );
        let now = Utc::now();
        let container = |name: &str| ReplicaContainer {
            name: name.into(),
            ready: Some(true),
            started: Some(true),
            restart_count: 0,
            running_state: Some("Running".into()),
            running_state_details: None,
        };
        state.replica_instances.by_resource.insert(
            "/r/ca".into(),
            vec![
                ReplicaInstance {
                    name: "ca-app--0000004-old".into(),
                    created_at: Some(now - Duration::hours(2)),
                    running_state: Some("Running".into()),
                    containers: vec![container("maintenance")],
                },
                ReplicaInstance {
                    name: "ca-app--0000004-new".into(),
                    created_at: Some(now),
                    running_state: Some("Running".into()),
                    containers: vec![container("maintenance"), container("http-auth")],
                },
            ],
        );

        request_container_shell(&mut state);

        let exec = state.pending_exec.expect("shell queued");
        assert_eq!(exec.name, "ca-app");
        assert_eq!(exec.resource_group, "rg-x");
        assert_eq!(exec.subscription.as_deref(), Some("sub-1"));
        assert_eq!(exec.revision.as_deref(), Some("ca-app--0000004"));
        // Newest replica, first (primary) container.
        assert_eq!(exec.replica.as_deref(), Some("ca-app--0000004-new"));
        assert_eq!(exec.container.as_deref(), Some("maintenance"));
        // View is unchanged — the shell runs from the event loop.
        assert_eq!(state.view, View::Detail);
    }

    #[test]
    fn s_on_non_container_falls_back_to_switch_subscription() {
        use crate::azure::resources::{Resource, ResourceKind};
        let mut state = fresh_state();
        state.resources = vec![Resource {
            id: "/r/fn".into(),
            name: "func".into(),
            kind: ResourceKind::FunctionApp,
            location: "we".into(),
            resource_group: "rg".into(),
            subscription_id: "sub".into(),
            state: Some("Running".into()),
            created_at: None,
            modified_at: None,
            meta: Default::default(),
        }];
        state.list_cursor = 0;
        state.view = View::Detail;

        request_container_shell(&mut state);
        // No shell queued; `s` kept its global switch-subscription meaning.
        assert!(state.pending_exec.is_none());
        assert_eq!(state.view, View::Subscriptions);
    }

    #[test]
    fn enter_on_plain_env_var_is_a_noop() {
        use crate::azure::env_vars::EnvVar;
        use crate::azure::resources::{Resource, ResourceKind};

        let auth = AzureAuth::demo();
        let (tx, _rx) = mpsc::unbounded_channel::<AppEvent>();

        let mut state = fresh_state();
        state.resources = vec![Resource {
            id: "/r/fn".into(),
            name: "func".into(),
            kind: ResourceKind::FunctionApp,
            location: "we".into(),
            resource_group: "rg".into(),
            subscription_id: "s".into(),
            state: Some("Running".into()),
            created_at: None,
            modified_at: None,
            meta: Default::default(),
        }];
        state.list_cursor = 0;
        state.view = View::EnvVars;
        state.func_settings.by_resource.insert(
            "/r/fn".into(),
            vec![EnvVar {
                name: "PORT".into(),
                value: "8080".into(),
                ..Default::default()
            }],
        );

        open_key_vault_ref_from_env_var(&mut state, &auth, &tx);
        // Stays put — no vault pinned, no modal, no view change.
        assert_eq!(state.view, View::EnvVars);
        assert!(state.key_vault.selected_vault.is_none());
        assert!(state.key_vault.secret_modal.is_none());
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
    fn command_mode_storage_alias_switches_view() {
        // Single-letter aliases (`:s`) were dropped — only the canonical name
        // routes. `:s` now hits the `unknown command` fallback (covered by a
        // separate test below).
        let mut state = fresh_state();
        state.view = View::List;
        run_command(&mut state, "storage");
        assert_eq!(state.view, View::StorageAccounts);
        // Category navigation is semantic-parent-based; the Help-only
        // view_stack must not grow.
        assert!(state.view_stack.is_empty());
        assert!(state.status_message.is_none());
    }

    #[test]
    fn command_mode_apis_alias_switches_view() {
        // `:apis` is the only alias for the Apis category after the cleanup
        // (legacy `a`, `resources`, `r` were dropped because they made
        // Tab-completion noisy).
        let mut state = fresh_state();
        state.view = View::StorageAccounts;
        run_command(&mut state, "apis");
        assert_eq!(state.view, View::List);
        assert!(state.status_message.is_none());
    }

    #[test]
    fn command_mode_cosmos_alias_switches_view() {
        let mut state = fresh_state();
        state.view = View::List;
        run_command(&mut state, "cosmos");
        assert_eq!(state.view, View::CosmosAccounts);
        assert!(state.view_stack.is_empty());
        assert!(state.status_message.is_none());
    }

    #[test]
    fn command_mode_dropped_single_letter_aliases_are_unknown() {
        // Regression guard: dropping `s` / `a` / `r` / `resources` from the
        // palette aliases means those buffers must surface "unknown command"
        // rather than silently routing.
        for cmd in ["s", "a", "r", "resources"] {
            let mut state = fresh_state();
            state.view = View::List;
            run_command(&mut state, cmd);
            assert_eq!(state.view, View::List, "{cmd} must not move the view");
            assert!(
                state
                    .status_message
                    .as_deref()
                    .map(|m| m.contains("unknown command"))
                    .unwrap_or(false),
                "{cmd} should surface unknown-command status"
            );
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
        assert!(all.contains(&"cosmos".to_string()));
        assert!(all.contains(&"subscriptions".to_string()));
        assert!(all.contains(&"refresh".to_string()));
        assert!(all.contains(&"quit".to_string()));
        // Legacy `resources` alias was dropped from the palette to keep
        // Tab-completion focused on canonical names.
        assert!(!all.contains(&"resources".to_string()));

        // `s` matches `storage` and `subscriptions` (and `subs`) — no
        // single-letter category aliases remain.
        let with_s = palette_tab_candidates("s");
        assert!(with_s.iter().any(|c| c == "storage"));
        assert!(with_s.iter().any(|c| c == "subscriptions"));
        assert!(with_s.iter().any(|c| c == "subs"));

        // `ap` narrows to `apis`.
        let with_ap = palette_tab_candidates("ap");
        assert!(with_ap.iter().any(|c| c == "apis"));
        assert!(!with_ap.iter().any(|c| c == "storage"));

        // `re` narrows to `registries` / `reg` / `refresh` — `resources` is
        // gone from the alias list.
        let with_re = palette_tab_candidates("re");
        assert!(with_re.iter().any(|c| c == "registries"));
        assert!(with_re.iter().any(|c| c == "refresh"));
        assert!(!with_re.iter().any(|c| c == "resources"));

        // `co` narrows to `cosmos`.
        let with_co = palette_tab_candidates("co");
        assert!(with_co.iter().any(|c| c == "cosmos"));

        // Nonsense prefix returns nothing.
        assert!(palette_tab_candidates("zzz").is_empty());
    }

    #[test]
    fn palette_ghost_hint_shows_remainder_of_first_candidate() {
        // `st` → only `storage` matches, hint is the rest of the word.
        assert_eq!(palette_ghost_hint("st"), "orage");
        // `s` → first `s*` candidate is `storage` (Apis no longer has an `s`
        // alias).
        assert_eq!(palette_ghost_hint("s"), "torage");
        // `re` → first match is `registries` (Apis dropped its `resources`
        // alias). `refresh` is reachable via Tab.
        assert_eq!(palette_ghost_hint("re"), "gistries");
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
        // First candidate is `apis` — the canonical alias of the first
        // category in `Category::ALL`.
        assert_eq!(pick, "apis");
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
            meta: Default::default(),
        };
        state.view = View::Subscriptions;

        // Forward: Subs -> List
        assert!(crate::ui::views::subscriptions::handle(
            Action::OpenSelected,
            &mut state,
        ));
        assert_eq!(state.view, View::List);
        // Forward navigation no longer records history — Back is
        // semantic-parent-based and the view_stack is Help-only.
        assert!(state.view_stack.is_empty());

        // The subs handler clears resources to force a fresh load — re-seed for
        // the next forward step.
        state.resources = vec![resource];

        // Forward: List -> Detail
        assert!(crate::ui::views::list::handle(
            Action::OpenSelected,
            &mut state
        ));
        assert_eq!(state.view, View::Detail);
        assert!(state.view_stack.is_empty());

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
        // Subscriptions is the breadcrumb root that opens the quit modal on Esc.
        state.view = View::Subscriptions;
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
            meta: Default::default(),
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
            peak_replica: None,
        };
        state.health.metrics.insert(
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
        let (_, label, _) = crate::ui::views::list::badge_for_row(&resource, &state, &theme);
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
            peak_replica: None,
        };
        state.health.metrics.insert(
            resource.id.clone(),
            vec![zero_series(MetricKind::Errors), traffic_series],
        );
        let (_, label, _) = crate::ui::views::list::badge_for_row(&resource, &state, &theme);
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
            meta: Default::default(),
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
            meta: Default::default(),
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
    fn yank_in_logs_visual_mode_returns_every_selected_line() {
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
            meta: Default::default(),
        }];
        state.list_cursor = 0;
        state.view = View::Logs;
        let ts = Utc
            .with_ymd_and_hms(2026, 5, 10, 12, 0, 0)
            .single()
            .unwrap();
        let mk = |msg: &str| LogLine {
            ts,
            level: LogLevel::Info,
            source: "AppTraces".into(),
            message: msg.into(),
            fields: Vec::new(),
        };
        state.logs.by_resource.insert(
            "/r/one".into(),
            vec![mk("first"), mk("second"), mk("third")],
        );

        // Anchor at row 0, cursor at row 2 → all three lines, one per row.
        state.logs.visual_anchor = Some(0);
        state.logs.scroll = 2;
        let yanked = yank_target(&state).expect("visual span should yield text");
        assert_eq!(yanked.lines().count(), 3);
        assert!(yanked.contains("first"));
        assert!(yanked.contains("second"));
        assert!(yanked.contains("third"));

        // A live yank clears the selection (vim-style) — exercised via do_yank.
        do_yank(&mut state);
        assert_eq!(state.logs.visual_anchor, None);
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
            meta: Default::default(),
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
            meta: Default::default(),
        }];
        state.list_cursor = 0;
        state.view = View::LogDetail;
        let url = portal_url_for(&state).expect("log detail should yield a portal URL");
        // Opens the Azure Monitor Logs blade (not the resource overview) with the
        // resource as the query scope and a pre-filled KQL query.
        assert!(url.contains("portal.azure.com"));
        assert!(url.contains("Microsoft_Azure_Monitoring_Logs/LogsBlade"));
        assert!(url.contains("LogsBlade.AnalyticsShareLinkToQuery"));
        // Resource id is percent-encoded into the resourceId path segment.
        assert!(url.contains(
            "resourceId/%2Fsubscriptions%2FX%2FresourceGroups%2Frg%2Fproviders%2FMicrosoft.Web%2Fsites%2Falpha"
        ));
        // The Function App table union is carried in the query segment.
        assert!(url.contains("FunctionAppLogs"));
        // No selected line here (logs cache empty) → relative range timespan.
        assert!(url.contains("/timespan/PT1H"));
    }

    #[test]
    fn portal_url_in_logs_brackets_the_selected_line_timespan() {
        use crate::azure::logs::{LogLevel, LogLine};
        use crate::azure::resources::{Resource, ResourceKind};
        use chrono::{TimeZone, Utc};
        let mut state = fresh_state();
        state.resources = vec![Resource {
            id: "/subscriptions/X/resourceGroups/rg/providers/Microsoft.App/containerApps/beta"
                .into(),
            name: "beta".into(),
            kind: ResourceKind::ContainerApp,
            location: "westeurope".into(),
            resource_group: "rg".into(),
            subscription_id: "X".into(),
            state: Some("Running".into()),
            created_at: None,
            modified_at: None,
            meta: Default::default(),
        }];
        state.list_cursor = 0;
        state.view = View::Logs;
        let ts = Utc
            .with_ymd_and_hms(2026, 5, 10, 12, 0, 0)
            .single()
            .unwrap();
        state.logs.by_resource.insert(
            state.resources[0].id.clone(),
            vec![LogLine {
                ts,
                level: LogLevel::Info,
                source: "ContainerAppConsoleLogs".into(),
                message: "hello".into(),
                fields: vec![],
            }],
        );
        state.logs.scroll = 0;
        let url = portal_url_for(&state).expect("logs should yield a portal URL");
        // Container App query scopes by name inside the blade query.
        assert!(url.contains("ContainerAppConsoleLogs"));
        assert!(url.contains("beta"));
        // Timespan is a one-minute absolute window centred on the line
        // (11:59:30 → 12:00:30), encoded as start%2Fend.
        assert!(
            url.contains("/timespan/2026-05-10T11%3A59%3A30.000Z%2F2026-05-10T12%3A00%3A30.000Z"),
            "expected a bracketed absolute timespan, got {url}"
        );
    }

    #[test]
    fn portal_url_container_app_logs_scope_to_workspace_when_resolved() {
        use crate::azure::logs::{LogLevel, LogLine};
        use crate::azure::resources::{Resource, ResourceKind};
        use chrono::{TimeZone, Utc};
        let mut state = fresh_state();
        let app_id =
            "/subscriptions/X/resourceGroups/rg/providers/Microsoft.App/containerApps/beta";
        let workspace_id =
            "/subscriptions/X/resourceGroups/rg/providers/Microsoft.OperationalInsights/workspaces/law";
        state.resources = vec![Resource {
            id: app_id.into(),
            name: "beta".into(),
            kind: ResourceKind::ContainerApp,
            location: "westeurope".into(),
            resource_group: "rg".into(),
            subscription_id: "X".into(),
            state: Some("Running".into()),
            created_at: None,
            modified_at: None,
            meta: Default::default(),
        }];
        state.list_cursor = 0;
        state.view = View::Logs;
        let ts = Utc
            .with_ymd_and_hms(2026, 5, 10, 12, 0, 0)
            .single()
            .unwrap();
        state.logs.by_resource.insert(
            app_id.into(),
            vec![LogLine {
                ts,
                level: LogLevel::Info,
                source: "ContainerAppConsoleLogs".into(),
                message: "hi".into(),
                fields: vec![],
            }],
        );
        // Workspace resolved (as it would be after the first log page).
        state
            .logs
            .workspace_ids
            .insert(app_id.into(), workspace_id.into());
        let url = portal_url_for(&state).expect("logs should yield a portal URL");
        // The blade scope (resourceId path segment) is the WORKSPACE, not the
        // container app — that's what makes the console-log tables resolvable.
        assert!(
            url.contains(
                "resourceId/%2Fsubscriptions%2FX%2FresourceGroups%2Frg%2Fproviders%2FMicrosoft.OperationalInsights%2Fworkspaces%2Flaw"
            ),
            "expected workspace-scoped blade, got {url}"
        );
        assert!(!url.contains("containerApps%2Fbeta"));
    }

    #[test]
    fn subscriptions_yank_and_portal_index_the_filtered_list() {
        use crate::azure::subscriptions::Subscription;
        let sub = |id: &str, name: &str| Subscription {
            id: id.into(),
            display_name: name.into(),
            state: "Enabled".into(),
            tenant_id: "t".into(),
        };
        let mut state = fresh_state();
        state.view = View::Subscriptions;
        state.subscriptions = vec![sub("sub-alpha", "alpha"), sub("sub-beta", "beta")];

        // Cursor 0 = synthetic "All" row → nothing to yank / open.
        state.subscription_cursor = 0;
        assert!(yank_target(&state).is_none());
        assert!(portal_url_for(&state).is_none());

        // Cursor 1 = first row → alpha (guards the old off-by-one that returned
        // the *next* subscription).
        state.subscription_cursor = 1;
        assert_eq!(yank_target(&state).as_deref(), Some("sub-alpha"));
        assert!(portal_url_for(&state)
            .unwrap()
            .contains("/subscriptions/sub-alpha/overview"));

        // With a `/`-filter matching only beta, row 1 now maps to beta.
        state.subscription_filter = tui_input::Input::default().with_value("beta".into());
        state.subscription_cursor = 1;
        assert_eq!(yank_target(&state).as_deref(), Some("sub-beta"));
        assert!(portal_url_for(&state)
            .unwrap()
            .contains("/subscriptions/sub-beta/overview"));
    }

    #[test]
    fn portal_url_in_detail_network_row_targets_networking_blade() {
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
            meta: Default::default(),
        }];
        state.list_cursor = 0;
        state.view = View::Detail;
        // Minimal Function App → meta rows are [state, network]; cursor 1 is the
        // network row. (Guarded by the detail.rs unit test that finds it by name.)
        state.detail_view.cursor = 1;
        let url = portal_url_for(&state).expect("detail should yield a portal URL");
        assert!(
            url.ends_with("/sites/alpha/networkingHub"),
            "network row should open the Networking blade, got {url}"
        );

        // A different row (state, cursor 0) opens the plain resource overview.
        state.detail_view.cursor = 0;
        let url = portal_url_for(&state).unwrap();
        assert!(url.ends_with("/sites/alpha"), "got {url}");
    }

    #[test]
    fn portal_url_in_env_vars_targets_the_environment_variables_blade() {
        use crate::azure::resources::{Resource, ResourceKind};
        let mk = |id: &str, kind: ResourceKind| Resource {
            id: id.into(),
            name: "x".into(),
            kind,
            location: "westeurope".into(),
            resource_group: "rg".into(),
            subscription_id: "X".into(),
            state: Some("Running".into()),
            created_at: None,
            modified_at: None,
            meta: Default::default(),
        };

        // Function App: dedicated Environment variables blade.
        let mut state = fresh_state();
        state.resources = vec![mk(
            "/subscriptions/X/resourceGroups/rg/providers/Microsoft.Web/sites/alpha",
            ResourceKind::FunctionApp,
        )];
        state.list_cursor = 0;
        state.view = View::EnvVars;
        let url = portal_url_for(&state).expect("env vars should yield a portal URL");
        assert!(
            url.ends_with("/sites/alpha/environmentVariablesAppSettings"),
            "function app env vars should open the env-vars blade, got {url}"
        );

        // Container App: env vars live in the Containers blade.
        let mut state = fresh_state();
        state.resources = vec![mk(
            "/subscriptions/X/resourceGroups/rg/providers/Microsoft.App/containerApps/beta",
            ResourceKind::ContainerApp,
        )];
        state.list_cursor = 0;
        state.view = View::EnvVars;
        let url = portal_url_for(&state).expect("env vars should yield a portal URL");
        assert!(
            url.ends_with("/containerApps/beta/containers"),
            "container app env vars should open the Containers blade, got {url}"
        );
    }
}
