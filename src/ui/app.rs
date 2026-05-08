//! App entry point: owns the terminal, runs the event loop, dispatches to views.

#![allow(dead_code, unused_variables)]

use crate::azure::auth::AzureAuth;
use crate::config::Config;

/// Run the TUI to completion. Returns when the user quits.
///
/// Lane 3 is the only owner of:
///   - the crossterm raw-mode setup/teardown
///   - the `mpsc::UnboundedSender<AppEvent>` end (cloned to background tasks)
///   - the chord state machine for vim-style multi-key shortcuts (`gg`, `dG`, ...)
///
/// Lane 3 calls into `crate::ui::views::*::render(frame, area, &state, &theme)`
/// once per draw, and `crate::ui::views::*::handle(action, &mut state)` to apply
/// view-specific input. Background loaders (subscriptions/resources/metrics/logs)
/// are spawned with `tokio::spawn` and report results back via `AppEvent`.
pub async fn run(auth: AzureAuth, cfg: Config) -> anyhow::Result<()> {
    todo!(
        "Lane 3: set up crossterm + ratatui terminal, init AppState::new(cfg), \
         spawn initial subscriptions load, run event loop until should_quit, \
         restore terminal on exit (panic-safe via a guard)"
    )
}
