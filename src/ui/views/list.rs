//! Resource list with fuzzy filter, favorites toggle, and a per-row health badge.

#![allow(dead_code, unused_variables)]

use ratatui::layout::Rect;
use ratatui::Frame;

use crate::ui::events::Action;
use crate::ui::state::AppState;
use crate::ui::theme::Theme;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    todo!(
        "Lane 4: split area horizontally if there's room (list | detail-preview); render \
         filtered_resources() with columns: ★?  name  short_tag  ●  HEALTH  rg-name; show search input \
         when state.list_filter_active; footer hint shows vim bindings + L=logs"
    )
}

pub fn handle(action: Action, state: &mut AppState) -> bool {
    todo!("Lane 4: handle MoveDown/Up/HalfPage/Top/Bottom on list_cursor; ToggleFavorite, ToggleFavoritesOnly, StartSearch, OpenSelected, OpenLogs")
}
