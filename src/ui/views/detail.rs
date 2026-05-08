//! Detail view: four sparklines (Requests, Http 5xx, CPU, Memory) plus a header
//! with the resource name + RG + health badge + window label.

#![allow(dead_code, unused_variables)]

use ratatui::layout::Rect;
use ratatui::Frame;

use crate::ui::events::Action;
use crate::ui::state::AppState;
use crate::ui::theme::Theme;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    todo!(
        "Lane 4: header line with name + rg + ● HEALTH + window 1d/7d. Then 4 stacked sparkline rows. \
         Use ratatui Sparkline. If a metric is missing (APIM has no Memory), render '—'. \
         Footer: 'd/w window  L logs  Enter expand  Esc back  q quit'."
    )
}

pub fn handle(action: Action, state: &mut AppState) -> bool {
    todo!("Lane 4: SetWindowDay, SetWindowWeek, OpenLogs (transition state.view = Logs), Back, Refresh")
}
