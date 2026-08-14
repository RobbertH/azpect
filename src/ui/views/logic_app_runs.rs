//! Run history for the pinned Logic App workflow, newest first. Enter pins
//! the run and opens its action breakdown ([`View::LogicAppRunDetail`]); `t`
//! opens the trigger firing history ([`View::LogicAppTriggerHistory`]).

#![allow(dead_code, unused_variables)]

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;

use crate::azure::logic_apps::WorkflowRun;
use crate::ui::events::Action;
use crate::ui::state::{AppState, View};
use crate::ui::theme::Theme;

const FOOTER_HINT: &str =
    "j/k move  Enter actions  t trigger history  Esc back  r refresh  y yank run id  o portal  ? help  q quit";
const HALF_PAGE: usize = 10;

/// Style a Logic Apps status word (shared by the runs / trigger-history /
/// actions tables so the color vocabulary stays identical).
pub(crate) fn status_style(status: &str, theme: &Theme) -> Style {
    match status {
        "Succeeded" => Style::default().fg(theme.healthy),
        "Failed" | "Aborted" | "TimedOut" => Style::default().fg(theme.critical),
        "Running" | "Waiting" => Style::default().fg(theme.accent),
        "Cancelled" | "Skipped" => Style::default().fg(theme.muted),
        _ => Style::default().fg(theme.degraded),
    }
}

/// `start → end` as a compact duration (`4.2s`, `1m 03s`, `2h 05m`); empty
/// while either end is missing (still running / never started).
pub(crate) fn duration_label(
    start: Option<chrono::DateTime<chrono::Utc>>,
    end: Option<chrono::DateTime<chrono::Utc>>,
) -> String {
    let (Some(start), Some(end)) = (start, end) else {
        return String::new();
    };
    let ms = (end - start).num_milliseconds().max(0);
    let secs = ms as f64 / 1000.0;
    if secs < 60.0 {
        format!("{secs:.1}s")
    } else if secs < 3600.0 {
        format!("{}m {:02}s", (secs / 60.0) as u64, (secs % 60.0) as u64)
    } else {
        format!(
            "{}h {:02}m",
            (secs / 3600.0) as u64,
            ((secs % 3600.0) / 60.0) as u64
        )
    }
}

/// Run/firing timestamp in local time, date included — history windows span
/// days, so a bare clock time would be ambiguous.
pub(crate) fn format_time(dt: Option<chrono::DateTime<chrono::Utc>>) -> String {
    match dt {
        Some(d) => d
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string(),
        None => String::new(),
    }
}

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);

    let Some(workflow) = state.logic_apps.selected_workflow.as_ref() else {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.border))
            .title(" run history ");
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

    let runs = state.logic_apps.runs.get(&workflow.id);
    let count_label = runs.map(|r| format!("· {} ", r.len())).unwrap_or_default();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Line::from(vec![
            Span::styled(
                format!(" runs · {} ", workflow.name),
                Style::default().fg(theme.fg),
            ),
            Span::styled(count_label, Style::default().fg(theme.muted)),
        ]));
    let inner = block.inner(chunks[0]);
    frame.render_widget(block, chunks[0]);

    if let Some(err) = state.logic_apps.runs_error.get(&workflow.id) {
        let p = Paragraph::new(Text::styled(
            format!("error: {err}"),
            Style::default().fg(theme.critical),
        ))
        .wrap(Wrap { trim: false });
        frame.render_widget(p, inner);
        render_footer(frame, chunks[1], theme);
        return;
    }

    let loading = state.logic_apps.runs_pending.contains(&workflow.id);
    match runs {
        None if loading => {
            let p = Paragraph::new(Line::from(Span::styled(
                "loading run history …",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, inner);
        }
        None => {
            let p = Paragraph::new(Line::from(Span::styled(
                "press r to load run history.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, inner);
        }
        Some(rows) if rows.is_empty() => {
            let p = Paragraph::new(Line::from(Span::styled(
                "no runs in the retained history.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, inner);
        }
        Some(rows) => {
            let widths = [
                Constraint::Length(19), // STARTED
                Constraint::Length(10), // STATUS
                Constraint::Length(9),  // DURATION
                Constraint::Length(24), // TRIGGER
                Constraint::Min(20),    // ERROR
            ];
            let header_row = Row::new(["STARTED", "STATUS", "DURATION", "TRIGGER", "ERROR"]).style(
                Style::default()
                    .fg(theme.muted)
                    .add_modifier(Modifier::BOLD),
            );
            let cursor = state.logic_apps.runs_cursor.min(rows.len() - 1);
            let body_rows: Vec<Row> = rows.iter().map(|run| build_row(run, theme)).collect();

            let table = Table::new(body_rows, widths)
                .header(header_row)
                .row_highlight_style(theme.selection())
                .highlight_symbol("▍ ")
                .column_spacing(2);
            let mut ts = TableState::default().with_offset(state.logic_apps.runs_view_top.get());
            ts.select(Some(cursor));
            frame.render_stateful_widget(table, inner, &mut ts);
            state.logic_apps.runs_view_top.set(ts.offset());
        }
    }

    render_footer(frame, chunks[1], theme);
}

fn build_row<'a>(run: &'a WorkflowRun, theme: &Theme) -> Row<'a> {
    Row::new(vec![
        Cell::from(format_time(run.start_time)).style(Style::default().fg(theme.fg)),
        Cell::from(run.status.as_str()).style(status_style(&run.status, theme)),
        Cell::from(duration_label(run.start_time, run.end_time))
            .style(Style::default().fg(theme.muted)),
        Cell::from(run.trigger_name.as_deref().unwrap_or("—").to_string())
            .style(Style::default().fg(theme.muted)),
        Cell::from(run.error.as_deref().unwrap_or("").to_string())
            .style(Style::default().fg(theme.degraded)),
    ])
}

fn render_footer(frame: &mut Frame, area: Rect, theme: &Theme) {
    let p = Paragraph::new(Line::from(Span::styled(
        FOOTER_HINT,
        Style::default().fg(theme.muted),
    )));
    frame.render_widget(p, area);
}

/// The run currently under the cursor, if any.
pub fn selected_run(state: &AppState) -> Option<WorkflowRun> {
    let workflow = state.logic_apps.selected_workflow.as_ref()?;
    state
        .logic_apps
        .runs
        .get(&workflow.id)?
        .get(state.logic_apps.runs_cursor)
        .cloned()
}

pub fn handle(action: Action, state: &mut AppState) -> bool {
    let len = state
        .logic_apps
        .selected_workflow
        .as_ref()
        .and_then(|w| state.logic_apps.runs.get(&w.id))
        .map(|r| r.len())
        .unwrap_or(0);

    match action {
        Action::MoveDown => {
            if len > 0 {
                state.logic_apps.runs_cursor = (state.logic_apps.runs_cursor + 1).min(len - 1);
            }
            true
        }
        Action::MoveUp => {
            state.logic_apps.runs_cursor = state.logic_apps.runs_cursor.saturating_sub(1);
            true
        }
        Action::HalfPageDown => {
            if len > 0 {
                state.logic_apps.runs_cursor =
                    (state.logic_apps.runs_cursor + HALF_PAGE).min(len - 1);
            }
            true
        }
        Action::HalfPageUp => {
            state.logic_apps.runs_cursor = state.logic_apps.runs_cursor.saturating_sub(HALF_PAGE);
            true
        }
        Action::GotoTop => {
            state.logic_apps.runs_cursor = 0;
            true
        }
        Action::GotoBottom => {
            if len > 0 {
                state.logic_apps.runs_cursor = len - 1;
            }
            true
        }
        Action::OpenSelected => {
            if let Some(run) = selected_run(state) {
                state.logic_apps.selected_run = Some(run);
                state.logic_apps.actions_cursor = 0;
                state.view = View::LogicAppRunDetail;
            }
            true
        }
        Action::OpenTriggerHistory => {
            if state.logic_apps.selected_workflow.is_some() {
                state.logic_apps.trigger_history_cursor = 0;
                state.view = View::LogicAppTriggerHistory;
            }
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::logic_apps::LogicApp;
    use crate::config::Config;
    use chrono::{TimeZone, Utc};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    pub(crate) fn workflow() -> LogicApp {
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

    pub(crate) fn run(name: &str, status: &str) -> WorkflowRun {
        WorkflowRun {
            name: name.into(),
            status: status.into(),
            start_time: Some(Utc.with_ymd_and_hms(2026, 8, 13, 11, 0, 0).unwrap()),
            end_time: Some(Utc.with_ymd_and_hms(2026, 8, 13, 11, 0, 4).unwrap()),
            trigger_name: Some("When_a_message_arrives".into()),
            trigger_inputs: None,
            trigger_outputs: None,
            error: (status == "Failed").then(|| "ActionFailed: boom".to_string()),
            correlation_id: None,
        }
    }

    fn fixture() -> AppState {
        let mut state = AppState::new(Config::default());
        state.view = View::LogicAppRuns;
        state.logic_apps.selected_workflow = Some(workflow());
        state
    }

    #[test]
    fn renders_loading_when_pending() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(120, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.logic_apps.runs_pending.insert(workflow().id.clone());
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("loading run history"));
    }

    #[test]
    fn renders_runs_with_status_and_error() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(160, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.logic_apps.runs.insert(
            workflow().id,
            vec![run("r1", "Succeeded"), run("r2", "Failed")],
        );
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("Succeeded"));
        assert!(buf.contains("Failed"));
        assert!(buf.contains("ActionFailed"));
        assert!(buf.contains("When_a_message_arrives"));
    }

    #[test]
    fn enter_pins_run_and_opens_actions() {
        let mut state = fixture();
        state
            .logic_apps
            .runs
            .insert(workflow().id, vec![run("r1", "Succeeded")]);
        assert!(handle(Action::OpenSelected, &mut state));
        assert_eq!(state.view, View::LogicAppRunDetail);
        assert_eq!(
            state
                .logic_apps
                .selected_run
                .as_ref()
                .map(|r| r.name.as_str()),
            Some("r1")
        );
    }

    #[test]
    fn t_opens_trigger_history() {
        let mut state = fixture();
        assert!(handle(Action::OpenTriggerHistory, &mut state));
        assert_eq!(state.view, View::LogicAppTriggerHistory);
        assert_eq!(state.logic_apps.trigger_history_cursor, 0);
    }

    #[test]
    fn duration_label_scales_units() {
        let s = Utc.with_ymd_and_hms(2026, 8, 13, 11, 0, 0).unwrap();
        assert_eq!(
            duration_label(Some(s), Some(s + chrono::Duration::milliseconds(4200))),
            "4.2s"
        );
        assert_eq!(
            duration_label(Some(s), Some(s + chrono::Duration::seconds(63))),
            "1m 03s"
        );
        assert_eq!(
            duration_label(Some(s), Some(s + chrono::Duration::seconds(7500))),
            "2h 05m"
        );
        assert_eq!(duration_label(Some(s), None), "");
    }
}
