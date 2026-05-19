//! APIM APIs panel. Drill-in from the Detail view when the selected resource
//! is an APIM service. Pressing Enter on a row opens [`View::ApimOperations`]
//! for that API.

#![allow(dead_code, unused_variables)]

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::azure::resources::ResourceKind;
use crate::ui::events::Action;
use crate::ui::state::{AppState, View};
use crate::ui::theme::Theme;

const FOOTER_HINT: &str = "j/k move  Enter operations  Esc back  r refresh  ? help  q quit";
const HALF_PAGE: usize = 10;

const NAME_COL_WIDTH: usize = 36;
const PATH_COL_WIDTH: usize = 24;

/// Resource id of the APIM service we're drilling into. Resolved off the
/// currently selected resource — kept here (not in state) because the cursor
/// in the list view always points at the parent APIM row while these views are
/// on the stack.
pub fn service_id(state: &AppState) -> Option<String> {
    state
        .selected_resource()
        .filter(|r| r.kind == ResourceKind::Apim)
        .map(|r| r.id.clone())
}

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(area);

    let resource = state.selected_resource();
    let header_name = resource.map(|r| r.name.as_str()).unwrap_or("(no APIM)");
    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            " APIs ",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            header_name,
            Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
        ),
    ]));
    frame.render_widget(header, chunks[0]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(" apis ", Style::default().fg(theme.fg)));
    let inner = block.inner(chunks[1]);
    frame.render_widget(block, chunks[1]);

    let Some(svc_id) = service_id(state) else {
        let p = Paragraph::new(Line::from(Span::styled(
            "no APIM service selected.",
            Style::default().fg(theme.muted),
        )));
        frame.render_widget(p, inner);
        render_footer(frame, chunks[2], theme);
        return;
    };

    if let Some(err) = state.apim.apis_error.get(&svc_id) {
        let p = Paragraph::new(Line::from(Span::styled(
            format!("error: {err}"),
            Style::default().fg(theme.critical),
        )));
        frame.render_widget(p, inner);
        render_footer(frame, chunks[2], theme);
        return;
    }

    let apis = state.apim.apis.get(&svc_id);
    let loading = state.apim.apis_pending.contains(&svc_id);
    match apis {
        None if loading => {
            let p = Paragraph::new(Line::from(Span::styled(
                "loading APIs …",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, inner);
        }
        None => {
            let p = Paragraph::new(Line::from(Span::styled(
                "press r to load APIs.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, inner);
        }
        Some(rows) if rows.is_empty() => {
            let p = Paragraph::new(Line::from(Span::styled(
                "no APIs defined on this APIM service.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, inner);
        }
        Some(rows) => {
            let cursor = state.apim.apis_cursor.min(rows.len() - 1);
            let visible = inner.height as usize;
            let scroll = scroll_for(cursor, rows.len(), visible);

            let lines: Vec<Line> = rows
                .iter()
                .enumerate()
                .skip(scroll)
                .take(visible)
                .map(|(i, api)| {
                    let selected = i == cursor;
                    let name = format!(
                        "{:<w$}",
                        truncate_right(&api.display_name, NAME_COL_WIDTH),
                        w = NAME_COL_WIDTH
                    );
                    let path = format!(
                        "{:<w$}",
                        truncate_right(&api.path, PATH_COL_WIDTH),
                        w = PATH_COL_WIDTH
                    );
                    let spans = vec![
                        Span::raw(if selected { "▍ " } else { "  " }),
                        Span::styled(name, Style::default().fg(theme.fg)),
                        Span::raw("  "),
                        Span::styled("/".to_string(), Style::default().fg(theme.muted)),
                        Span::styled(path, Style::default().fg(theme.accent)),
                    ];
                    if selected {
                        Line::from(spans).style(theme.selection())
                    } else {
                        Line::from(spans)
                    }
                })
                .collect();
            frame.render_widget(Paragraph::new(lines), inner);
        }
    }

    render_footer(frame, chunks[2], theme);
}

fn render_footer(frame: &mut Frame, area: Rect, theme: &Theme) {
    let p = Paragraph::new(Line::from(Span::styled(
        FOOTER_HINT,
        Style::default().fg(theme.muted),
    )));
    frame.render_widget(p, area);
}

pub fn handle(action: Action, state: &mut AppState) -> bool {
    let Some(svc_id) = service_id(state) else {
        return false;
    };
    let len = state.apim.apis.get(&svc_id).map(|v| v.len()).unwrap_or(0);

    match action {
        Action::MoveDown => {
            if len > 0 {
                state.apim.apis_cursor = (state.apim.apis_cursor + 1).min(len - 1);
            }
            true
        }
        Action::MoveUp => {
            state.apim.apis_cursor = state.apim.apis_cursor.saturating_sub(1);
            true
        }
        Action::HalfPageDown => {
            if len > 0 {
                state.apim.apis_cursor = (state.apim.apis_cursor + HALF_PAGE).min(len - 1);
            }
            true
        }
        Action::HalfPageUp => {
            state.apim.apis_cursor = state.apim.apis_cursor.saturating_sub(HALF_PAGE);
            true
        }
        Action::GotoTop => {
            state.apim.apis_cursor = 0;
            true
        }
        Action::GotoBottom => {
            if len > 0 {
                state.apim.apis_cursor = len - 1;
            }
            true
        }
        Action::OpenSelected => {
            let api_id = state
                .apim
                .apis
                .get(&svc_id)
                .and_then(|rows| rows.get(state.apim.apis_cursor))
                .map(|api| api.id.clone());
            if let Some(api_id) = api_id {
                state.apim.selected_api_id = Some(api_id);
                state.apim.operations_cursor = 0;
                state.view_stack.push(state.view);
                state.view = View::ApimOperations;
            }
            true
        }
        _ => false,
    }
}

fn truncate_right(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    if max <= 1 {
        return "…".to_string();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

fn scroll_for(cursor: usize, len: usize, visible: usize) -> usize {
    if visible == 0 || len <= visible {
        return 0;
    }
    if cursor < visible {
        return 0;
    }
    (cursor + 1).saturating_sub(visible).min(len - visible)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::apim::Api;
    use crate::azure::resources::{Resource, ResourceKind};
    use crate::config::Config;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn fixture() -> AppState {
        let mut state = AppState::new(Config::default());
        state.resources = vec![Resource {
            id: "/svc/myapim".into(),
            name: "myapim".into(),
            kind: ResourceKind::Apim,
            location: "westeurope".into(),
            resource_group: "rg".into(),
            subscription_id: "sub".into(),
            state: None,
        }];
        state.list_cursor = 0;
        state.view = View::ApimApis;
        state
    }

    #[test]
    fn renders_loading() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(80, 12);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.apim.apis_pending.insert("/svc/myapim".into());
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("loading APIs"));
    }

    #[test]
    fn renders_rows() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(100, 12);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.apim.apis.insert(
            "/svc/myapim".into(),
            vec![Api {
                id: "/svc/myapim/apis/echo".into(),
                name: "echo".into(),
                display_name: "Echo API".into(),
                path: "echo".into(),
            }],
        );
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("Echo API"));
        assert!(buf.contains("echo"));
    }

    #[test]
    fn enter_transitions_to_operations_and_pins_api() {
        let mut state = fixture();
        state.apim.apis.insert(
            "/svc/myapim".into(),
            vec![Api {
                id: "/svc/myapim/apis/echo".into(),
                name: "echo".into(),
                display_name: "Echo API".into(),
                path: "echo".into(),
            }],
        );
        assert!(handle(Action::OpenSelected, &mut state));
        assert_eq!(state.view, View::ApimOperations);
        assert_eq!(
            state.apim.selected_api_id.as_deref(),
            Some("/svc/myapim/apis/echo")
        );
    }

    #[test]
    fn navigation_clamps() {
        let mut state = fixture();
        state.apim.apis.insert(
            "/svc/myapim".into(),
            vec![
                Api {
                    id: "a1".into(),
                    name: "a".into(),
                    display_name: "A".into(),
                    path: "a".into(),
                },
                Api {
                    id: "a2".into(),
                    name: "b".into(),
                    display_name: "B".into(),
                    path: "b".into(),
                },
            ],
        );
        assert!(handle(Action::MoveDown, &mut state));
        assert_eq!(state.apim.apis_cursor, 1);
        assert!(handle(Action::MoveDown, &mut state));
        assert_eq!(state.apim.apis_cursor, 1, "clamped to last");
        assert!(handle(Action::GotoTop, &mut state));
        assert_eq!(state.apim.apis_cursor, 0);
    }
}
