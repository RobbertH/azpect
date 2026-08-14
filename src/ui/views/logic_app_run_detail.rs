//! Action breakdown of the pinned Logic App run, in execution order, with a
//! synthetic first row for the trigger itself (its inputs/outputs are the
//! message that started the run). Enter pins the row's content links and
//! opens [`View::LogicAppContent`].

#![allow(dead_code, unused_variables)]

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;

use super::logic_app_runs::{duration_label, format_time, status_style};
use crate::azure::logic_apps::RunAction;
use crate::ui::events::Action;
use crate::ui::state::{AppState, LogicAppsCache, LogicContentSource, View};
use crate::ui::theme::Theme;

const FOOTER_HINT: &str =
    "j/k move  Enter content  Esc back  r refresh  y yank action  o portal  ? help  q quit";
const HALF_PAGE: usize = 10;

/// Rows shown for the pinned run: the synthetic trigger row (when the run
/// carries a trigger) followed by the fetched actions. Rebuilt per frame /
/// keypress from the cache — cheap, the action count is small.
fn visible_rows(state: &AppState) -> Vec<DetailRow> {
    let Some(workflow) = state.logic_apps.selected_workflow.as_ref() else {
        return Vec::new();
    };
    let Some(run) = state.logic_apps.selected_run.as_ref() else {
        return Vec::new();
    };
    let key = LogicAppsCache::actions_key(&workflow.id, &run.name);
    let Some(actions) = state.logic_apps.actions.get(&key) else {
        return Vec::new();
    };
    let mut rows = Vec::with_capacity(actions.len() + 1);
    if let Some(trigger) = run.trigger_name.as_deref() {
        rows.push(DetailRow::Trigger {
            name: trigger.to_string(),
        });
    }
    rows.extend(actions.iter().cloned().map(DetailRow::Action));
    rows
}

/// One row of the actions table: the run's trigger or a real action.
#[derive(Clone, Debug)]
enum DetailRow {
    Trigger { name: String },
    Action(RunAction),
}

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);

    let (Some(workflow), Some(run)) = (
        state.logic_apps.selected_workflow.as_ref(),
        state.logic_apps.selected_run.as_ref(),
    ) else {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.border))
            .title(" run actions ");
        let inner = block.inner(chunks[0]);
        frame.render_widget(block, chunks[0]);
        let p = Paragraph::new(Line::from(Span::styled(
            "no run selected.",
            Style::default().fg(theme.muted),
        )));
        frame.render_widget(p, inner);
        render_footer(frame, chunks[1], theme);
        return;
    };

    let key = LogicAppsCache::actions_key(&workflow.id, &run.name);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Line::from(vec![
            Span::styled(
                format!(
                    " run · {} · {} ",
                    workflow.name,
                    format_time(run.start_time)
                ),
                Style::default().fg(theme.fg),
            ),
            Span::styled(
                format!("· {} ", run.status),
                status_style(&run.status, theme),
            ),
        ]));
    let inner = block.inner(chunks[0]);
    frame.render_widget(block, chunks[0]);

    if let Some(err) = state.logic_apps.actions_error.get(&key) {
        let p = Paragraph::new(Text::styled(
            format!("error: {err}"),
            Style::default().fg(theme.critical),
        ))
        .wrap(Wrap { trim: false });
        frame.render_widget(p, inner);
        render_footer(frame, chunks[1], theme);
        return;
    }

    let loading = state.logic_apps.actions_pending.contains(&key);
    let loaded = state.logic_apps.actions.contains_key(&key);
    let rows = visible_rows(state);
    if !loaded {
        let msg = if loading {
            "loading run actions …"
        } else {
            "press r to load run actions."
        };
        let p = Paragraph::new(Line::from(Span::styled(
            msg,
            Style::default().fg(theme.muted),
        )));
        frame.render_widget(p, inner);
    } else if rows.is_empty() {
        let p = Paragraph::new(Line::from(Span::styled(
            "run has no recorded actions.",
            Style::default().fg(theme.muted),
        )));
        frame.render_widget(p, inner);
    } else {
        let widths = [
            Constraint::Min(24),    // ACTION
            Constraint::Length(10), // STATUS
            Constraint::Length(12), // CODE
            Constraint::Length(19), // STARTED
            Constraint::Length(9),  // DURATION
            Constraint::Min(18),    // ERROR
        ];
        let header_row = Row::new(["ACTION", "STATUS", "CODE", "STARTED", "DURATION", "ERROR"])
            .style(
                Style::default()
                    .fg(theme.muted)
                    .add_modifier(Modifier::BOLD),
            );
        let cursor = state.logic_apps.actions_cursor.min(rows.len() - 1);
        let body_rows: Vec<Row> = rows.iter().map(|r| build_row(r, run, theme)).collect();

        let table = Table::new(body_rows, widths)
            .header(header_row)
            .row_highlight_style(theme.selection())
            .highlight_symbol("▍ ")
            .column_spacing(2);
        let mut ts = TableState::default().with_offset(state.logic_apps.actions_view_top.get());
        ts.select(Some(cursor));
        frame.render_stateful_widget(table, inner, &mut ts);
        state.logic_apps.actions_view_top.set(ts.offset());
    }

    render_footer(frame, chunks[1], theme);
}

fn build_row<'a>(
    row: &'a DetailRow,
    run: &'a crate::azure::logic_apps::WorkflowRun,
    theme: &Theme,
) -> Row<'a> {
    match row {
        DetailRow::Trigger { name } => Row::new(vec![
            Cell::from(format!("⚡ trigger · {name}")).style(Style::default().fg(theme.accent)),
            Cell::from("").style(Style::default().fg(theme.muted)),
            Cell::from(""),
            Cell::from(format_time(run.start_time)).style(Style::default().fg(theme.muted)),
            Cell::from(""),
            Cell::from(""),
        ]),
        DetailRow::Action(a) => Row::new(vec![
            Cell::from(a.name.as_str()).style(Style::default().fg(theme.fg)),
            Cell::from(a.status.as_str()).style(status_style(&a.status, theme)),
            Cell::from(a.code.as_deref().unwrap_or("").to_string())
                .style(Style::default().fg(theme.muted)),
            Cell::from(format_time(a.start_time)).style(Style::default().fg(theme.muted)),
            Cell::from(duration_label(a.start_time, a.end_time))
                .style(Style::default().fg(theme.muted)),
            Cell::from(a.error.as_deref().unwrap_or("").to_string())
                .style(Style::default().fg(theme.degraded)),
        ]),
    }
}

fn render_footer(frame: &mut Frame, area: Rect, theme: &Theme) {
    let p = Paragraph::new(Line::from(Span::styled(
        FOOTER_HINT,
        Style::default().fg(theme.muted),
    )));
    frame.render_widget(p, area);
}

/// Name of the action (or trigger) under the cursor, for yank.
pub fn selected_row_name(state: &AppState) -> Option<String> {
    let rows = visible_rows(state);
    match rows.get(state.logic_apps.actions_cursor)? {
        DetailRow::Trigger { name } => Some(name.clone()),
        DetailRow::Action(a) => Some(a.name.clone()),
    }
}

pub fn handle(action: Action, state: &mut AppState) -> bool {
    let len = visible_rows(state).len();

    match action {
        Action::MoveDown => {
            if len > 0 {
                state.logic_apps.actions_cursor =
                    (state.logic_apps.actions_cursor + 1).min(len - 1);
            }
            true
        }
        Action::MoveUp => {
            state.logic_apps.actions_cursor = state.logic_apps.actions_cursor.saturating_sub(1);
            true
        }
        Action::HalfPageDown => {
            if len > 0 {
                state.logic_apps.actions_cursor =
                    (state.logic_apps.actions_cursor + HALF_PAGE).min(len - 1);
            }
            true
        }
        Action::HalfPageUp => {
            state.logic_apps.actions_cursor =
                state.logic_apps.actions_cursor.saturating_sub(HALF_PAGE);
            true
        }
        Action::GotoTop => {
            state.logic_apps.actions_cursor = 0;
            true
        }
        Action::GotoBottom => {
            if len > 0 {
                state.logic_apps.actions_cursor = len - 1;
            }
            true
        }
        Action::OpenSelected => {
            let workflow_id = state
                .logic_apps
                .selected_workflow
                .as_ref()
                .map(|w| w.id.clone());
            let run = state.logic_apps.selected_run.clone();
            let row = visible_rows(state)
                .get(state.logic_apps.actions_cursor)
                .cloned();
            if let (Some(workflow_id), Some(run), Some(row)) = (workflow_id, run, row) {
                let source = match row {
                    DetailRow::Trigger { name } => LogicContentSource {
                        key: format!("{workflow_id}/runs/{}/trigger", run.name),
                        title: format!("trigger · {name}"),
                        inputs: run.trigger_inputs.clone(),
                        outputs: run.trigger_outputs.clone(),
                        origin: View::LogicAppRunDetail,
                    },
                    DetailRow::Action(a) => LogicContentSource {
                        key: format!("{workflow_id}/runs/{}/actions/{}", run.name, a.name),
                        title: a.name.clone(),
                        inputs: a.inputs,
                        outputs: a.outputs,
                        origin: View::LogicAppRunDetail,
                    },
                };
                state.logic_apps.selected_content = Some(source);
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
    use crate::azure::logic_apps::{ContentLink, LogicApp, WorkflowRun};
    use crate::config::Config;
    use chrono::{TimeZone, Utc};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn workflow() -> LogicApp {
        LogicApp {
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

    fn run() -> WorkflowRun {
        WorkflowRun {
            name: "r1".into(),
            status: "Succeeded".into(),
            start_time: Some(Utc.with_ymd_and_hms(2026, 8, 13, 11, 0, 0).unwrap()),
            end_time: Some(Utc.with_ymd_and_hms(2026, 8, 13, 11, 0, 4).unwrap()),
            trigger_name: Some("When_a_message_arrives".into()),
            trigger_inputs: Some(ContentLink {
                uri: "https://x/in?sig=S".into(),
                size: Some(64),
            }),
            trigger_outputs: None,
            error: None,
            correlation_id: None,
        }
    }

    fn action(name: &str, status: &str) -> RunAction {
        RunAction {
            name: name.into(),
            status: status.into(),
            code: Some("OK".into()),
            start_time: Some(Utc.with_ymd_and_hms(2026, 8, 13, 11, 0, 1).unwrap()),
            end_time: Some(Utc.with_ymd_and_hms(2026, 8, 13, 11, 0, 2).unwrap()),
            error: None,
            inputs: Some(ContentLink {
                uri: "https://x/a-in?sig=S".into(),
                size: Some(64),
            }),
            outputs: None,
        }
    }

    fn fixture() -> AppState {
        let mut state = AppState::new(Config::default());
        state.view = View::LogicAppRunDetail;
        state.logic_apps.selected_workflow = Some(workflow());
        state.logic_apps.selected_run = Some(run());
        state.logic_apps.actions.insert(
            LogicAppsCache::actions_key(&workflow().id, "r1"),
            vec![
                action("Parse_JSON", "Succeeded"),
                action("Post_Message", "Failed"),
            ],
        );
        state
    }

    #[test]
    fn renders_trigger_row_then_actions() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(160, 12);
        let mut term = Terminal::new(backend).unwrap();
        let state = fixture();
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("trigger · When_a_message_arrives"));
        assert!(buf.contains("Parse_JSON"));
        assert!(buf.contains("Post_Message"));
    }

    #[test]
    fn enter_on_trigger_row_pins_trigger_links() {
        let mut state = fixture();
        state.logic_apps.actions_cursor = 0;
        assert!(handle(Action::OpenSelected, &mut state));
        assert_eq!(state.view, View::LogicAppContent);
        let src = state.logic_apps.selected_content.as_ref().expect("pinned");
        assert!(src.key.ends_with("/runs/r1/trigger"));
        assert!(src.inputs.is_some());
        assert_eq!(src.origin, View::LogicAppRunDetail);
    }

    #[test]
    fn enter_on_action_row_pins_action_links() {
        let mut state = fixture();
        state.logic_apps.actions_cursor = 1; // first real action after trigger row
        assert!(handle(Action::OpenSelected, &mut state));
        let src = state.logic_apps.selected_content.as_ref().expect("pinned");
        assert!(src.key.ends_with("/runs/r1/actions/Parse_JSON"));
        assert_eq!(src.title, "Parse_JSON");
    }

    #[test]
    fn cursor_clamps_to_trigger_plus_actions() {
        let mut state = fixture();
        assert!(handle(Action::GotoBottom, &mut state));
        // 1 trigger row + 2 actions.
        assert_eq!(state.logic_apps.actions_cursor, 2);
    }
}
