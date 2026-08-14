//! Top-level Logic Apps mode entry view: lists consumption Logic Apps
//! (`Microsoft.Logic/workflows`) visible to the current subscription scope.
//! Pressing Enter on a row pins the workflow into
//! `state.logic_apps.selected_workflow` and opens [`View::LogicAppRuns`].

#![allow(dead_code, unused_variables)]

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;

use super::{name_col_width, truncate_ellipsis};
use crate::azure::logic_apps::LogicApp;
use crate::ui::events::Action;
use crate::ui::state::{subscription_display_name, AppState, View};
use crate::ui::theme::Theme;

const FOOTER_HINT: &str =
    "j/k move  Enter runs  / filter  Esc back  r refresh  y yank id  o portal  ? help  q quit";
const HALF_PAGE: usize = 10;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);

    let filter_value = state.logic_apps.workflows_filter.value();
    let filter_active = state.logic_apps.workflows_filter_active;
    let total = state.logic_apps.workflows.as_ref().map(|v| v.len());
    let filtered = state.logic_apps.filtered_workflows();
    let count_label = match total {
        Some(t) if !filter_value.is_empty() => format!("· {} of {} ", filtered.len(), t),
        Some(t) => format!("· {t} "),
        None => String::new(),
    };
    let mut title_spans: Vec<Span> = vec![
        Span::styled(" logic apps ", Style::default().fg(theme.fg)),
        Span::styled(count_label, Style::default().fg(theme.muted)),
    ];
    if filter_active || !filter_value.is_empty() {
        title_spans.push(Span::styled(
            format!("/{filter_value} "),
            Style::default().fg(theme.accent),
        ));
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Line::from(title_spans));
    let inner = block.inner(chunks[0]);
    frame.render_widget(block, chunks[0]);

    let (search_area, body_area) = if filter_active {
        let parts = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(inner);
        (Some(parts[0]), parts[1])
    } else {
        (None, inner)
    };
    if let Some(sa) = search_area {
        let p = Paragraph::new(Line::from(vec![
            Span::styled("> ", Style::default().fg(theme.accent)),
            Span::styled(filter_value, Style::default().fg(theme.fg)),
            Span::styled("█", Style::default().fg(theme.accent)),
        ]));
        frame.render_widget(p, sa);
    }

    if let Some(err) = state.logic_apps.workflows_error.as_deref() {
        let p = Paragraph::new(Text::styled(
            format!("error: {err}"),
            Style::default().fg(theme.critical),
        ))
        .wrap(Wrap { trim: false });
        frame.render_widget(p, body_area);
        render_footer(frame, chunks[1], theme);
        return;
    }

    match state.logic_apps.workflows.as_deref() {
        None if state.logic_apps.workflows_pending => {
            let p = Paragraph::new(Line::from(Span::styled(
                "loading logic apps …",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        None => {
            let p = Paragraph::new(Line::from(Span::styled(
                "press r to load logic apps.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some([]) => {
            let p = Paragraph::new(Line::from(Span::styled(
                "No consumption logic apps found in selected subscriptions.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(_) if filtered.is_empty() => {
            let p = Paragraph::new(Line::from(Span::styled(
                "no logic apps match the current filter.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(_) => {
            let show_sub_cols = state.selected_subscription.is_none();

            // NAME absorbs the leftover width (see `registries.rs` for the
            // pattern). `fixed_w` sums the non-NAME widths below; keep in sync.
            let fixed_w: u16 = 9 + 22 + 10 + if show_sub_cols { 22 } else { 0 } + 14;
            let n_cols: u16 = 5 + if show_sub_cols { 1 } else { 0 };
            let longest = filtered
                .iter()
                .map(|w| w.name.chars().count() as u16)
                .max()
                .unwrap_or(0);
            let name_w = name_col_width(body_area.width, fixed_w, n_cols, longest);

            let mut widths: Vec<Constraint> = vec![
                Constraint::Length(name_w), // NAME
                Constraint::Length(9),      // STATE
                Constraint::Length(22),     // RG
                Constraint::Length(10),     // CHANGED
            ];
            let mut headers: Vec<&'static str> = vec!["NAME", "STATE", "RESOURCE GROUP", "CHANGED"];
            if show_sub_cols {
                widths.push(Constraint::Length(22));
                headers.push("SUB NAME");
            }
            widths.push(Constraint::Length(14)); // LOCATION
            headers.push("LOCATION");

            let header_row = Row::new(headers).style(
                Style::default()
                    .fg(theme.muted)
                    .add_modifier(Modifier::BOLD),
            );

            let cursor = state.logic_apps.workflows_cursor.min(filtered.len() - 1);
            let body_rows: Vec<Row> = filtered
                .iter()
                .map(|wf| build_row(wf, state, show_sub_cols, name_w, theme))
                .collect();

            let table = Table::new(body_rows, widths)
                .header(header_row)
                .row_highlight_style(theme.selection())
                .highlight_symbol("▍ ")
                .column_spacing(2);

            let mut ts =
                TableState::default().with_offset(state.logic_apps.workflows_view_top.get());
            ts.select(Some(cursor));
            frame.render_stateful_widget(table, body_area, &mut ts);
            state.logic_apps.workflows_view_top.set(ts.offset());
        }
    }

    render_footer(frame, chunks[1], theme);
}

fn build_row<'a>(
    wf: &'a LogicApp,
    state: &'a AppState,
    show_sub_cols: bool,
    name_w: u16,
    theme: &Theme,
) -> Row<'a> {
    let state_cell = match wf.state.as_deref() {
        Some("Enabled") => Cell::from("Enabled").style(Style::default().fg(theme.healthy)),
        // A disabled workflow silently drops every trigger — loud signal.
        Some("Disabled") => Cell::from("Disabled").style(Style::default().fg(theme.critical)),
        Some(s) => Cell::from(s.to_string()).style(Style::default().fg(theme.degraded)),
        None => Cell::from("?").style(Style::default().fg(theme.muted)),
    };
    let mut cells: Vec<Cell> = vec![
        Cell::from(truncate_ellipsis(&wf.name, name_w as usize))
            .style(Style::default().fg(theme.fg)),
        state_cell,
        Cell::from(wf.resource_group.as_str()).style(Style::default().fg(theme.muted)),
        Cell::from(format_date(wf.changed_at.as_ref())).style(Style::default().fg(theme.muted)),
    ];
    if show_sub_cols {
        cells.push(
            Cell::from(
                subscription_display_name(state, &wf.subscription_id)
                    .unwrap_or("")
                    .to_string(),
            )
            .style(Style::default().fg(theme.muted)),
        );
    }
    cells.push(Cell::from(wf.location.as_str()).style(Style::default().fg(theme.muted)));
    Row::new(cells)
}

fn render_footer(frame: &mut Frame, area: Rect, theme: &Theme) {
    let p = Paragraph::new(Line::from(Span::styled(
        FOOTER_HINT,
        Style::default().fg(theme.muted),
    )));
    frame.render_widget(p, area);
}

fn format_date(dt: Option<&chrono::DateTime<chrono::Utc>>) -> String {
    match dt {
        Some(d) => d.format("%Y-%m-%d").to_string(),
        None => String::new(),
    }
}

pub fn handle(action: Action, state: &mut AppState) -> bool {
    let len = state.logic_apps.filtered_workflows().len();

    if state.logic_apps.workflows_filter_active {
        match action {
            Action::Back => {
                state.logic_apps.workflows_filter_active = false;
                state.logic_apps.workflows_filter.reset();
                state.logic_apps.workflows_cursor = 0;
                return true;
            }
            Action::OpenSelected => {
                state.logic_apps.workflows_filter_active = false;
                return true;
            }
            Action::MoveDown => {
                state.logic_apps.workflows_filter_active = false;
            }
            Action::MoveUp
            | Action::HalfPageDown
            | Action::HalfPageUp
            | Action::GotoTop
            | Action::GotoBottom => {}
            _ => return false,
        }
    }

    match action {
        Action::MoveDown => {
            if len > 0 {
                state.logic_apps.workflows_cursor =
                    (state.logic_apps.workflows_cursor + 1).min(len - 1);
            }
            true
        }
        Action::MoveUp => {
            state.logic_apps.workflows_cursor = state.logic_apps.workflows_cursor.saturating_sub(1);
            true
        }
        Action::HalfPageDown => {
            if len > 0 {
                state.logic_apps.workflows_cursor =
                    (state.logic_apps.workflows_cursor + HALF_PAGE).min(len - 1);
            }
            true
        }
        Action::HalfPageUp => {
            state.logic_apps.workflows_cursor =
                state.logic_apps.workflows_cursor.saturating_sub(HALF_PAGE);
            true
        }
        Action::GotoTop => {
            state.logic_apps.workflows_cursor = 0;
            true
        }
        Action::GotoBottom => {
            if len > 0 {
                state.logic_apps.workflows_cursor = len - 1;
            }
            true
        }
        Action::StartSearch => {
            state.logic_apps.workflows_filter.reset();
            state.logic_apps.workflows_cursor = 0;
            state.logic_apps.workflows_filter_active = true;
            true
        }
        Action::OpenSelected => {
            let wf = state
                .logic_apps
                .filtered_workflows()
                .get(state.logic_apps.workflows_cursor)
                .copied()
                .cloned();
            if let Some(wf) = wf {
                state.logic_apps.selected_workflow = Some(wf);
                state.logic_apps.runs_cursor = 0;
                state.view = View::LogicAppRuns;
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
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn fixture() -> AppState {
        let mut state = AppState::new(Config::default());
        state.view = View::LogicApps;
        state
    }

    pub(crate) fn workflow(name: &str) -> LogicApp {
        LogicApp {
            id: format!(
                "/subscriptions/sub/resourceGroups/rg/providers/Microsoft.Logic/workflows/{name}",
            ),
            name: name.into(),
            resource_group: "rg".into(),
            subscription_id: "sub".into(),
            location: "westeurope".into(),
            state: Some("Enabled".into()),
            changed_at: None,
            created_at: None,
        }
    }

    #[test]
    fn renders_loading_state() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(120, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.logic_apps.workflows_pending = true;
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("loading logic apps"));
    }

    #[test]
    fn renders_workflow_row_with_state() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(180, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        let mut disabled = workflow("logic-legacy");
        disabled.state = Some("Disabled".into());
        state.logic_apps.workflows = Some(vec![workflow("logic-orders"), disabled]);
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("logic-orders"));
        assert!(buf.contains("Enabled"));
        assert!(buf.contains("Disabled"));
    }

    #[test]
    fn enter_pins_workflow_and_drills_in() {
        let mut state = fixture();
        state.logic_apps.workflows = Some(vec![workflow("logic-orders")]);
        assert!(handle(Action::OpenSelected, &mut state));
        assert_eq!(state.view, View::LogicAppRuns);
        assert_eq!(
            state
                .logic_apps
                .selected_workflow
                .as_ref()
                .map(|w| w.name.as_str()),
            Some("logic-orders")
        );
    }

    #[test]
    fn filter_substring_match_is_case_insensitive() {
        let mut state = fixture();
        state.logic_apps.workflows = Some(vec![
            workflow("logic-orders"),
            workflow("Logic-Invoices"),
            workflow("other"),
        ]);
        state.logic_apps.workflows_filter = tui_input::Input::default().with_value("LOGIC".into());
        let names: Vec<&str> = state
            .logic_apps
            .filtered_workflows()
            .iter()
            .map(|w| w.name.as_str())
            .collect();
        assert_eq!(names, vec!["logic-orders", "Logic-Invoices"]);
    }
}
