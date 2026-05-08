//! Help overlay. Toggled by `?`. Shows the keymap in a centered popup.

#![allow(dead_code, unused_variables)]

use ratatui::layout::Rect;
use ratatui::Frame;

use crate::ui::events::Action;
use crate::ui::state::AppState;
use crate::ui::theme::Theme;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    todo!(
        "Lane 4: centered popup, 60-70% width, listing the keymap. Group headings: \
         'Navigation', 'Resources', 'Detail / Logs', 'Global'. Press ? or Esc to dismiss."
    )
}

pub fn handle(action: Action, state: &mut AppState) -> bool {
    todo!("Lane 4: any key dismisses (returns to previous_view)")
}
