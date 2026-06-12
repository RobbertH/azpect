//! APIM operations (routes) panel. Reached from [`super::apim_apis`] via Enter.
//! Pressing Enter on a row opens the operation's policy XML in
//! [`super::apim_policy`].

#![allow(dead_code, unused_variables)]

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::ui::events::Action;
use crate::ui::state::{AppState, View};
use crate::ui::theme::Theme;

const FOOTER_HINT: &str =
    "j/k move  Enter policy  / filter  y yank  o portal  Esc back  r refresh  ? help  q quit";
const HALF_PAGE: usize = 10;

const METHOD_COL_WIDTH: usize = 7;
const URL_COL_WIDTH: usize = 40;
/// Two-cell selection-marker gutter on the left, matched by the header row.
const MARKER_PAD: &str = "  ";

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let chunks = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(area);

    let api_id = state.apim.selected_api_id.as_deref();
    let api_display = api_id
        .and_then(|id| display_name_for(state, id))
        .unwrap_or_else(|| "(no API)".to_string());

    // Second header line: the API's static backend (`properties.serviceUrl`).
    // `None` means APIM has no static backend set — typically routing is done
    // in policy via `set-backend-service`, so say so rather than show nothing.
    let backend = api_id.and_then(|id| service_url_for(state, id));
    let backend_line = match backend {
        Some(url) => Line::from(vec![
            Span::styled(" backend  ", Style::default().fg(theme.muted)),
            Span::styled(url, Style::default().fg(theme.accent)),
        ]),
        None => Line::from(vec![
            Span::styled(" backend  ", Style::default().fg(theme.muted)),
            Span::styled("— (set in policy)", Style::default().fg(theme.muted)),
        ]),
    };

    let header = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(
                " operations ",
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                api_display,
                Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
            ),
        ]),
        backend_line,
    ]);
    frame.render_widget(header, chunks[0]);

    let filter_value = state.apim.operations_filter.value();
    let filter_active = state.apim.operations_filter_active;

    // Title: total count, switching to `N of M` while a filter narrows the
    // list, plus a `/{filter}` chip. Mirrors the APIs view.
    let (total, filtered_len) = match api_id {
        Some(id) => (
            state.apim.operations.get(id).map(|v| v.len()),
            state.apim.filtered_operations(id).len(),
        ),
        None => (None, 0),
    };
    let count_label = match total {
        Some(t) if !filter_value.is_empty() => format!("· {filtered_len} of {t} "),
        Some(t) => format!("· {t} "),
        None => String::new(),
    };
    let mut title_spans: Vec<Span> = vec![
        Span::styled(" routes ", Style::default().fg(theme.fg)),
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
    let inner = block.inner(chunks[1]);
    frame.render_widget(block, chunks[1]);

    // Optional filter input row at the top of the inner area.
    let (search_area, inner) = if filter_active {
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

    let Some(api_id) = api_id else {
        let p = Paragraph::new(Line::from(Span::styled(
            "no API selected.",
            Style::default().fg(theme.muted),
        )));
        frame.render_widget(p, inner);
        render_footer(frame, chunks[2], theme);
        return;
    };

    if let Some(err) = state.apim.operations_error.get(api_id) {
        let p = Paragraph::new(Line::from(Span::styled(
            format!("error: {err}"),
            Style::default().fg(theme.critical),
        )));
        frame.render_widget(p, inner);
        render_footer(frame, chunks[2], theme);
        return;
    }

    let ops = state.apim.operations.get(api_id);
    let loading = state.apim.operations_pending.contains(api_id);
    let filtered = state.apim.filtered_operations(api_id);
    match ops {
        None if loading => {
            let p = Paragraph::new(Line::from(Span::styled(
                "loading operations …",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, inner);
        }
        None => {
            let p = Paragraph::new(Line::from(Span::styled(
                "press r to load operations.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, inner);
        }
        Some(rows) if rows.is_empty() => {
            let p = Paragraph::new(Line::from(Span::styled(
                "no operations defined on this API.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, inner);
        }
        Some(_) if filtered.is_empty() => {
            let p = Paragraph::new(Line::from(Span::styled(
                "no operations match the current filter.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, inner);
        }
        Some(_) => {
            // Reserve the top row of the body for the column-header line.
            let parts = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(inner);
            frame.render_widget(Paragraph::new(column_header(theme)), parts[0]);
            let rows_area = parts[1];

            let cursor = state.apim.operations_cursor.min(filtered.len() - 1);
            let visible = rows_area.height as usize;
            let scroll = scroll_for(cursor, filtered.len(), visible);

            // `name` is the trailing column, so it gets exactly the width left
            // after the marker gutter + method + url columns and their gaps —
            // never more (so the `…` always lands inside the pane rather than
            // being clipped at the edge) and never a fixed cap that wastes the
            // rest of the row. The `1` and `2` are the literal gaps in the row
            // format below.
            let name_width = (rows_area.width as usize)
                .saturating_sub(MARKER_PAD.len() + METHOD_COL_WIDTH + 1 + URL_COL_WIDTH + 2);

            let lines: Vec<Line> = filtered
                .iter()
                .enumerate()
                .skip(scroll)
                .take(visible)
                .map(|(i, op)| {
                    let selected = i == cursor;
                    let method = format!("{:<w$}", op.method, w = METHOD_COL_WIDTH);
                    let url = format!(
                        "{:<w$}",
                        truncate_right(&op.url_template, URL_COL_WIDTH),
                        w = URL_COL_WIDTH
                    );
                    let name = format!(
                        "{:<w$}",
                        truncate_right(&op.display_name, name_width),
                        w = name_width
                    );
                    let method_color = color_for_method(&op.method, theme);
                    let spans = vec![
                        Span::raw(if selected { "▍ " } else { "  " }),
                        Span::styled(
                            method,
                            Style::default()
                                .fg(method_color)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(" "),
                        Span::styled(url, Style::default().fg(theme.fg)),
                        Span::raw("  "),
                        Span::styled(name, Style::default().fg(theme.muted)),
                    ];
                    if selected {
                        Line::from(spans).style(theme.selection())
                    } else {
                        Line::from(spans)
                    }
                })
                .collect();
            frame.render_widget(Paragraph::new(lines), rows_area);
        }
    }

    render_footer(frame, chunks[2], theme);
}

/// Column-header row, aligned to the same widths the data rows use (including
/// the two-cell selection-marker gutter on the left).
fn column_header(theme: &Theme) -> Line<'static> {
    let style = Style::default()
        .fg(theme.muted)
        .add_modifier(Modifier::BOLD);
    let head = format!(
        "{MARKER_PAD}{:<mw$} {:<uw$}  {}",
        "method",
        "url",
        "name",
        mw = METHOD_COL_WIDTH,
        uw = URL_COL_WIDTH,
    );
    Line::from(Span::styled(head, style))
}

/// Method-colored chips so the verb stands out at a glance. Falls back to
/// the foreground colour for anything we don't recognise (custom verbs are
/// rare in APIM but allowed).
fn color_for_method(method: &str, theme: &Theme) -> Color {
    match method.to_ascii_uppercase().as_str() {
        "GET" => theme.healthy,
        "POST" => theme.accent,
        "PUT" | "PATCH" => theme.degraded,
        "DELETE" => theme.critical,
        _ => theme.fg,
    }
}

fn display_name_for(state: &AppState, api_id: &str) -> Option<String> {
    state
        .apim
        .apis
        .values()
        .flat_map(|v| v.iter())
        .find(|a| a.id == api_id)
        .map(|a| {
            if a.path.is_empty() {
                a.display_name.clone()
            } else {
                format!("{} · /{}", a.display_name, a.path)
            }
        })
}

/// The static backend (`serviceUrl`) of the API we're drilling into, looked up
/// from the cached APIs list. `None` when the API isn't cached or has no static
/// backend configured.
fn service_url_for(state: &AppState, api_id: &str) -> Option<String> {
    state
        .apim
        .apis
        .values()
        .flat_map(|v| v.iter())
        .find(|a| a.id == api_id)
        .and_then(|a| a.service_url.clone())
}

fn render_footer(frame: &mut Frame, area: Rect, theme: &Theme) {
    let p = Paragraph::new(Line::from(Span::styled(
        FOOTER_HINT,
        Style::default().fg(theme.muted),
    )));
    frame.render_widget(p, area);
}

pub fn handle(action: Action, state: &mut AppState) -> bool {
    let Some(api_id) = state.apim.selected_api_id.clone() else {
        return false;
    };
    // Navigation operates on the filtered slice so the cursor never points past
    // the end of what's rendered. Mirrors `apim_apis::handle`.
    let len = state.apim.filtered_operations(&api_id).len();

    // While the filter input has focus, swallow most actions but let the
    // dispatcher's filter-forwarding gate push raw chars into the buffer.
    // Esc cancels (deactivates AND clears); Enter commits (deactivates, keeps
    // the value). Down hands focus back to the filtered list.
    if state.apim.operations_filter_active {
        match action {
            Action::Back => {
                state.apim.operations_filter_active = false;
                state.apim.operations_filter.reset();
                state.apim.operations_cursor = 0;
                return true;
            }
            Action::OpenSelected => {
                state.apim.operations_filter_active = false;
                return true;
            }
            Action::MoveDown => {
                state.apim.operations_filter_active = false;
                // fall through to navigation handling below
            }
            Action::MoveUp
            | Action::HalfPageDown
            | Action::HalfPageUp
            | Action::GotoTop
            | Action::GotoBottom => {
                // fall through to navigation handling below
            }
            _ => return false,
        }
    }

    match action {
        Action::StartSearch => {
            state.apim.operations_filter_active = true;
            true
        }
        Action::MoveDown => {
            if len > 0 {
                state.apim.operations_cursor = (state.apim.operations_cursor + 1).min(len - 1);
            }
            true
        }
        Action::MoveUp => {
            state.apim.operations_cursor = state.apim.operations_cursor.saturating_sub(1);
            true
        }
        Action::HalfPageDown => {
            if len > 0 {
                state.apim.operations_cursor =
                    (state.apim.operations_cursor + HALF_PAGE).min(len - 1);
            }
            true
        }
        Action::HalfPageUp => {
            state.apim.operations_cursor = state.apim.operations_cursor.saturating_sub(HALF_PAGE);
            true
        }
        Action::GotoTop => {
            state.apim.operations_cursor = 0;
            true
        }
        Action::GotoBottom => {
            if len > 0 {
                state.apim.operations_cursor = len - 1;
            }
            true
        }
        Action::OpenSelected => {
            // Resolve via the filtered slice so the cursor's row matches what
            // the user actually sees on screen.
            let op_id = state
                .apim
                .filtered_operations(&api_id)
                .get(state.apim.operations_cursor)
                .map(|op| op.id.clone());
            if let Some(op_id) = op_id {
                state.apim.selected_operation_id = Some(op_id);
                state.apim.policy_scroll = 0;
                state.view_stack.push(state.view);
                state.view = View::ApimPolicy;
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
    use crate::azure::apim::Operation;
    use crate::config::Config;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn fixture() -> AppState {
        let mut state = AppState::new(Config::default());
        state.view = View::ApimOperations;
        state.apim.selected_api_id = Some("/svc/myapim/apis/echo".into());
        state
    }

    #[test]
    fn renders_method_and_url() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(100, 12);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.apim.operations.insert(
            "/svc/myapim/apis/echo".into(),
            vec![Operation {
                id: "/svc/myapim/apis/echo/operations/get-resource".into(),
                name: "get-resource".into(),
                display_name: "Retrieve resource".into(),
                method: "GET".into(),
                url_template: "/resource".into(),
            }],
        );
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("GET"));
        assert!(buf.contains("/resource"));
        assert!(buf.contains("Retrieve resource"));
    }

    #[test]
    fn renders_backend_service_url() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(100, 12);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.apim.apis.insert(
            "/svc/myapim".into(),
            vec![crate::azure::apim::Api {
                id: "/svc/myapim/apis/echo".into(),
                name: "echo".into(),
                display_name: "Echo API".into(),
                path: "echo".into(),
                service_url: Some("https://echo.internal.example.com".into()),
            }],
        );
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("backend"));
        assert!(buf.contains("https://echo.internal.example.com"));
    }

    #[test]
    fn renders_policy_backend_placeholder_when_no_service_url() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(100, 12);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.apim.apis.insert(
            "/svc/myapim".into(),
            vec![crate::azure::apim::Api {
                id: "/svc/myapim/apis/echo".into(),
                name: "echo".into(),
                display_name: "Echo API".into(),
                path: "echo".into(),
                service_url: None,
            }],
        );
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("set in policy"));
    }

    fn three_ops() -> AppState {
        let mut state = fixture();
        state.apim.operations.insert(
            "/svc/myapim/apis/echo".into(),
            vec![
                Operation {
                    id: "/svc/myapim/apis/echo/operations/get-orders".into(),
                    name: "get-orders".into(),
                    display_name: "List orders".into(),
                    method: "GET".into(),
                    url_template: "/orders".into(),
                },
                Operation {
                    id: "/svc/myapim/apis/echo/operations/post-payments".into(),
                    name: "post-payments".into(),
                    display_name: "Create payment".into(),
                    method: "POST".into(),
                    url_template: "/payments".into(),
                },
                Operation {
                    id: "/svc/myapim/apis/echo/operations/del-catalog".into(),
                    name: "del-catalog".into(),
                    display_name: "Remove catalog item".into(),
                    method: "DELETE".into(),
                    url_template: "/catalog".into(),
                },
            ],
        );
        state
    }

    #[test]
    fn renders_column_headers() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(120, 12);
        let mut term = Terminal::new(backend).unwrap();
        let state = three_ops();
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("method"), "method header missing: {buf}");
        assert!(buf.contains("url"), "url header missing: {buf}");
        assert!(buf.contains("name"), "name header missing: {buf}");
    }

    #[test]
    fn slash_opens_filter_input() {
        let mut state = three_ops();
        assert!(handle(Action::StartSearch, &mut state));
        assert!(state.apim.operations_filter_active);
    }

    #[test]
    fn esc_in_filter_clears_buffer() {
        let mut state = three_ops();
        state.apim.operations_filter_active = true;
        state.apim.operations_filter = tui_input::Input::default().with_value("pay".to_string());
        assert!(handle(Action::Back, &mut state));
        assert!(!state.apim.operations_filter_active);
        assert_eq!(state.apim.operations_filter.value(), "");
    }

    #[test]
    fn filter_matches_name_url_and_method() {
        let mut state = three_ops();
        // url-template match
        state.apim.operations_filter =
            tui_input::Input::default().with_value("catalog".to_string());
        let names: Vec<&str> = state
            .apim
            .filtered_operations("/svc/myapim/apis/echo")
            .iter()
            .map(|o| o.display_name.as_str())
            .collect();
        assert_eq!(names, vec!["Remove catalog item"]);
        // method match (case-insensitive)
        state.apim.operations_filter = tui_input::Input::default().with_value("post".to_string());
        let names: Vec<&str> = state
            .apim
            .filtered_operations("/svc/myapim/apis/echo")
            .iter()
            .map(|o| o.display_name.as_str())
            .collect();
        assert_eq!(names, vec!["Create payment"]);
    }

    #[test]
    fn navigation_and_open_use_filtered_slice() {
        let mut state = three_ops();
        state.apim.operations_filter = tui_input::Input::default().with_value("pay".to_string());
        state.apim.operations_cursor = 0;
        // Only one match → GotoBottom clamps to filtered len-1 == 0.
        assert!(handle(Action::GotoBottom, &mut state));
        assert_eq!(state.apim.operations_cursor, 0);
        assert!(handle(Action::OpenSelected, &mut state));
        assert_eq!(state.view, View::ApimPolicy);
        assert_eq!(
            state.apim.selected_operation_id.as_deref(),
            Some("/svc/myapim/apis/echo/operations/post-payments"),
            "drills into the filtered row, not the same index in the raw list",
        );
    }

    #[test]
    fn renders_filter_chip_in_title() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(120, 12);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = three_ops();
        state.apim.operations_filter = tui_input::Input::default().with_value("pay".to_string());
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("/pay"), "title chip should show /pay: {buf}");
        assert!(
            buf.contains("1 of 3"),
            "count should switch to `N of M` when filtering: {buf}",
        );
    }

    #[test]
    fn enter_opens_policy_view() {
        let mut state = fixture();
        state.apim.operations.insert(
            "/svc/myapim/apis/echo".into(),
            vec![Operation {
                id: "/svc/myapim/apis/echo/operations/get-resource".into(),
                name: "get-resource".into(),
                display_name: "Retrieve".into(),
                method: "GET".into(),
                url_template: "/resource".into(),
            }],
        );
        assert!(handle(Action::OpenSelected, &mut state));
        assert_eq!(state.view, View::ApimPolicy);
        assert_eq!(
            state.apim.selected_operation_id.as_deref(),
            Some("/svc/myapim/apis/echo/operations/get-resource")
        );
    }
}
