//! Subscription picker. Shown on first launch, and again when the user presses `s`.

#![allow(dead_code, unused_variables)]

use ratatui::layout::Rect;
use ratatui::Frame;

use crate::ui::events::Action;
use crate::ui::state::AppState;
use crate::ui::theme::Theme;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    todo!("Lane 4: vertical list of state.subscriptions, highlight at state.subscription_cursor, footer hint 'j/k select  Enter open  q quit'")
}

/// View-local input handler. Returns `true` if the action was consumed.
pub fn handle(action: Action, state: &mut AppState) -> bool {
    todo!("Lane 4")
}
