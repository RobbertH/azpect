//! Trigger firing history for the pinned Logic App workflow: every poll /
//! firing the service retained, including `fired: false` checks that started
//! no run. Opened with `t` from the runs view. Enter opens the firing's
//! message content ([`View::LogicAppContent`]).

#![allow(dead_code, unused_variables)]

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;

use super::logic_app_runs::{duration_label, format_time, status_style};
use crate::azure::logic_apps::TriggerHistory;
use crate::ui::events::Action;
use crate::ui::state::{AppState, LogicContentSource, View};
use crate::ui::theme::Theme;

const FOOTER_HINT: &str =
    "j/k move  Enter content  Esc back  r refresh  y yank run id  o portal  ? help  q quit";
const HALF_PAGE: usize = 10;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);

    let Some(workflow) = state.logic_apps.selected_workflow.as_ref() else {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.border))
            .title(" trigger history ");
        let inner = block.inner(chunks[0]);
        frame.render_widget(block, chunks[0]);
        let p = Paragraph::new(Line::from(Span::styled(
            "no logic app selected.",
            Style::default().fg(theme.muted),
        )));
        frame.render_widget(p, inner);
        render_footer(frame, chunks[1], theme);
        return;
    };

    let rows = state.logic_apps.trigger_history.get(&workflow.id);
    let count_label = rows.map(|r| format!("· {} ", r.len())).unwrap_or_default();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Line::from(vec![
            Span::styled(
                format!(" trigger history · {} ", workflow.name),
                Style::default().fg(theme.fg),
            ),
            Span::styled(count_label, Style::default().fg(theme.muted)),
        ]));
    let inner = block.inner(chunks[0]);
    frame.render_widget(block, chunks[0]);

    if let Some(err) = state.logic_apps.trigger_history_error.get(&workflow.id) {
        let p = Paragraph::new(Text::styled(
            format!("error: {err}"),
            Style::default().fg(theme.critical),
        ))
        .wrap(Wrap { trim: false });
        frame.render_widget(p, inner);
        render_footer(frame, chunks[1], theme);
        return;
    }

    let loading = state
        .logic_apps
        .trigger_history_pending
        .contains(&workflow.id);
    match rows {
        None if loading => {
            let p = Paragraph::new(Line::from(Span::styled(
                "loading trigger history …",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, inner);
        }
        None => {
            let p = Paragraph::new(Line::from(Span::styled(
                "press r to load trigger history.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, inner);
        }
        Some(rows) if rows.is_empty() => {
            let p = Paragraph::new(Line::from(Span::styled(
                "no trigger firings in the retained history.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, inner);
        }
        Some(rows) => {
            let widths = [
                Constraint::Length(19), // TIME
                Constraint::Length(24), // TRIGGER
                Constraint::Length(10), // STATUS
                Constraint::Length(5),  // FIRED
                Constraint::Length(9),  // DURATION
                Constraint::Min(18),    // RUN
            ];
            let header_row = Row::new(["TIME", "TRIGGER", "STATUS", "FIRED", "DURATION", "RUN"])
                .style(
                    Style::default()
                        .fg(theme.muted)
                        .add_modifier(Modifier::BOLD),
                );
            let cursor = state.logic_apps.trigger_history_cursor.min(rows.len() - 1);
            let body_rows: Vec<Row> = rows.iter().map(|h| build_row(h, theme)).collect();

            let table = Table::new(body_rows, widths)
                .header(header_row)
                .row_highlight_style(theme.selection())
                .highlight_symbol("▍ ")
                .column_spacing(2);
            let mut ts =
                TableState::default().with_offset(state.logic_apps.trigger_history_view_top.get());
            ts.select(Some(cursor));
            frame.render_stateful_widget(table, inner, &mut ts);
            state.logic_apps.trigger_history_view_top.set(ts.offset());
        }
    }

    render_footer(frame, chunks[1], theme);
}

fn build_row<'a>(h: &'a TriggerHistory, theme: &Theme) -> Row<'a> {
    let fired = if h.fired {
        Cell::from("yes").style(Style::default().fg(theme.healthy))
    } else {
        Cell::from("no").style(Style::default().fg(theme.muted))
    };
    Row::new(vec![
        Cell::from(format_time(h.start_time)).style(Style::default().fg(theme.fg)),
        Cell::from(h.trigger_name.as_str()).style(Style::default().fg(theme.muted)),
        Cell::from(h.status.as_str()).style(status_style(&h.status, theme)),
        fired,
        Cell::from(duration_label(h.start_time, h.end_time))
            .style(Style::default().fg(theme.muted)),
        Cell::from(h.run_name.as_deref().unwrap_or("").to_string())
            .style(Style::default().fg(theme.muted)),
    ])
}

fn render_footer(frame: &mut Frame, area: Rect, theme: &Theme) {
    let p = Paragraph::new(Line::from(Span::styled(
        FOOTER_HINT,
        Style::default().fg(theme.muted),
    )));
    frame.render_widget(p, area);
}

/// The firing currently under the cursor, if any.
pub fn selected_history(state: &AppState) -> Option<TriggerHistory> {
    let workflow = state.logic_apps.selected_workflow.as_ref()?;
    state
        .logic_apps
        .trigger_history
        .get(&workflow.id)?
        .get(state.logic_apps.trigger_history_cursor)
        .cloned()
}

pub fn handle(action: Action, state: &mut AppState) -> bool {
    let len = state
        .logic_apps
        .selected_workflow
        .as_ref()
        .and_then(|w| state.logic_apps.trigger_history.get(&w.id))
        .map(|r| r.len())
        .unwrap_or(0);

    match action {
        Action::MoveDown => {
            if len > 0 {
                state.logic_apps.trigger_history_cursor =
                    (state.logic_apps.trigger_history_cursor + 1).min(len - 1);
            }
            true
        }
        Action::MoveUp => {
            state.logic_apps.trigger_history_cursor =
                state.logic_apps.trigger_history_cursor.saturating_sub(1);
            true
        }
        Action::HalfPageDown => {
            if len > 0 {
                state.logic_apps.trigger_history_cursor =
                    (state.logic_apps.trigger_history_cursor + HALF_PAGE).min(len - 1);
            }
            true
        }
        Action::HalfPageUp => {
            state.logic_apps.trigger_history_cursor = state
                .logic_apps
                .trigger_history_cursor
                .saturating_sub(HALF_PAGE);
            true
        }
        Action::GotoTop => {
            state.logic_apps.trigger_history_cursor = 0;
            true
        }
        Action::GotoBottom => {
            if len > 0 {
                state.logic_apps.trigger_history_cursor = len - 1;
            }
            true
        }
        Action::OpenSelected => {
            let workflow_id = state
                .logic_apps
                .selected_workflow
                .as_ref()
                .map(|w| w.id.clone());
            if let (Some(workflow_id), Some(h)) = (workflow_id, selected_history(state)) {
                state.logic_apps.selected_content = Some(LogicContentSource {
                    key: format!("{workflow_id}/triggers/{}/{}", h.trigger_name, h.name),
                    title: format!(
                        "trigger · {} · {}",
                        h.trigger_name,
                        format_time(h.start_time)
                    ),
                    inputs: h.inputs,
                    outputs: h.outputs,
                    origin: View::LogicAppTriggerHistory,
                });
                state.logic_apps.content_scroll = 0;
                state.view = View::LogicAppContent;
            }
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use chrono::{TimeZone, Utc};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn history(name: &str, fired: bool) -> TriggerHistory {
        TriggerHistory {
            name: name.into(),
            trigger_name: "Recurrence".into(),
            status: "Succeeded".into(),
            fired,
            start_time: Some(Utc.with_ymd_and_hms(2026, 8, 13, 11, 0, 0).unwrap()),
            end_time: Some(Utc.with_ymd_and_hms(2026, 8, 13, 11, 0, 1).unwrap()),
            run_name: fired.then(|| "08585287554104334735".to_string()),
            inputs: None,
            outputs: None,
        }
    }

    fn workflow() -> crate::azure::logic_apps::LogicApp {
        crate::azure::logic_apps::LogicApp {
            id: "/subscriptions/sub/resourceGroups/rg/providers/Microsoft.Logic/workflows/wf"
                .into(),
            name: "wf".into(),
            resource_group: "rg".into(),
            subscription_id: "sub".into(),
            location: "westeurope".into(),
            state: Some("Enabled".into()),
            changed_at: None,
            created_at: None,
        }
    }

    fn fixture() -> AppState {
        let mut state = AppState::new(Config::default());
        state.view = View::LogicAppTriggerHistory;
        state.logic_apps.selected_workflow = Some(workflow());
        state
    }

    #[test]
    fn renders_firings_with_fired_flag() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(160, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        let wf_id = state
            .logic_apps
            .selected_workflow
            .as_ref()
            .unwrap()
            .id
            .clone();
        state
            .logic_apps
            .trigger_history
            .insert(wf_id, vec![history("h1", true), history("h2", false)]);
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("Recurrence"));
        assert!(buf.contains("yes"));
        assert!(buf.contains("08585287554104334735"));
    }

    #[test]
    fn enter_pins_content_source_with_trigger_origin() {
        let mut state = fixture();
        let wf_id = state
            .logic_apps
            .selected_workflow
            .as_ref()
            .unwrap()
            .id
            .clone();
        state
            .logic_apps
            .trigger_history
            .insert(wf_id.clone(), vec![history("h1", true)]);
        assert!(handle(Action::OpenSelected, &mut state));
        assert_eq!(state.view, View::LogicAppContent);
        let src = state.logic_apps.selected_content.as_ref().expect("pinned");
        assert_eq!(src.origin, View::LogicAppTriggerHistory);
        assert!(src.key.starts_with(&wf_id));
        assert_eq!(state.logic_apps.content_scroll, 0);
    }
}
