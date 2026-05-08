//! Logs view: scrollable table of recent log lines for the selected resource,
//! with an errors-only toggle and the same `d/w` window control as the detail view.

#![allow(dead_code, unused_variables)]

use ratatui::layout::Rect;
use ratatui::Frame;

use crate::ui::events::Action;
use crate::ui::state::AppState;
use crate::ui::theme::Theme;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    todo!(
        "Lane 4: render Table with columns: ts (HH:MM:SS), level/code, source, message. \
         Use theme.critical for errors. Header shows 'logs: <name> (<kind>)  <window>' and \
         'filter: [errors only ✓]' when state.logs.errors_only is true. Footer: \
         'j/k scroll  e errors-only  d/w window  / search  Esc back'. \
         If the resource is APIM, render a single-line message: 'logs not supported for APIM in v1'."
    )
}

pub fn handle(action: Action, state: &mut AppState) -> bool {
    todo!(
        "Lane 4: ToggleErrorsOnly (also re-fires a logs load via the AppEvent channel), \
         SetWindowDay/Week, MoveDown/Up, Back. Search starts a substring filter on message text."
    )
}
