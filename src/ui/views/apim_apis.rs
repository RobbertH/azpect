//! APIM APIs panel. Drill-in from the Detail view when the selected resource
//! is an APIM service. Pressing Enter on a row opens [`View::ApimOperations`]
//! for that API.

#![allow(dead_code, unused_variables)]

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use super::edge_scroll;
use crate::azure::resources::ResourceKind;
use crate::ui::events::Action;
use crate::ui::state::{AppState, View};
use crate::ui::theme::Theme;

const FOOTER_HINT: &str =
    "j/k move  Enter operations  / filter  y yank  o portal  Esc back  r refresh  ? help  q quit";
const HALF_PAGE: usize = 10;

const NAME_COL_WIDTH: usize = 32;
const PATH_COL_WIDTH: usize = 20;
/// Gap between columns, and the two-cell selection-marker gutter on the left.
const COL_GAP: &str = "  ";
const MARKER_PAD: &str = "  ";

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

    let svc_id_opt = service_id(state);
    let filter_value = state.apim.apis_filter.value();
    let filter_active = state.apim.apis_filter_active;

    // Title: total count, switching to `N of M` while a filter narrows the
    // list, plus a `/{filter}` chip. Mirrors the storage / registry views.
    let (total, filtered_len) = match svc_id_opt.as_deref() {
        Some(id) => (
            state.apim.apis.get(id).map(|v| v.len()),
            state.apim.filtered_apis(id).len(),
        ),
        None => (None, 0),
    };
    let count_label = match total {
        Some(t) if !filter_value.is_empty() => format!("· {filtered_len} of {t} "),
        Some(t) => format!("· {t} "),
        None => String::new(),
    };
    let mut title_spans: Vec<Span> = vec![
        Span::styled(" apis ", Style::default().fg(theme.fg)),
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

    let Some(svc_id) = svc_id_opt else {
        let p = Paragraph::new(Line::from(Span::styled(
            "no APIM service selected.",
            Style::default().fg(theme.muted),
        )));
        frame.render_widget(p, body_area);
        render_footer(frame, chunks[2], theme);
        return;
    };

    if let Some(err) = state.apim.apis_error.get(&svc_id) {
        // `Text` keeps any line breaks from a pretty-printed JSON error body;
        // `wrap` folds long lines so nothing runs off the right edge.
        let p = Paragraph::new(Text::styled(
            format!("error: {err}"),
            Style::default().fg(theme.critical),
        ))
        .wrap(Wrap { trim: false });
        frame.render_widget(p, body_area);
        render_footer(frame, chunks[2], theme);
        return;
    }

    let apis = state.apim.apis.get(&svc_id);
    let loading = state.apim.apis_pending.contains(&svc_id);
    let filtered = state.apim.filtered_apis(&svc_id);
    match apis {
        None if loading => {
            let p = Paragraph::new(Line::from(Span::styled(
                "loading APIs …",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        None => {
            let p = Paragraph::new(Line::from(Span::styled(
                "press r to load APIs.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(rows) if rows.is_empty() => {
            let p = Paragraph::new(Line::from(Span::styled(
                "no APIs defined on this APIM service.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(_) if filtered.is_empty() => {
            let p = Paragraph::new(Line::from(Span::styled(
                "no APIs match the current filter.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(_) => {
            // Reserve the top row of the body for the column-header line; the
            // data rows scroll beneath it.
            let parts =
                Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(body_area);
            frame.render_widget(Paragraph::new(column_header(theme)), parts[0]);
            let rows_area = parts[1];

            let cursor = state.apim.apis_cursor.min(filtered.len() - 1);
            let visible = rows_area.height as usize;
            let scroll = edge_scroll(&state.apim.apis_view_top, cursor, filtered.len(), visible);

            // `service url` is the trailing column, so it gets exactly the width
            // left after the marker gutter + the two fixed columns and their
            // gaps — never more (so the `…` always lands inside the pane rather
            // than being clipped by ratatui at the edge) and never a fixed cap
            // that wastes the rest of the row.
            let url_width = (rows_area.width as usize).saturating_sub(
                MARKER_PAD.len() + NAME_COL_WIDTH + COL_GAP.len() + PATH_COL_WIDTH + COL_GAP.len(),
            );

            let lines: Vec<Line> = filtered
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
                    // Fold the leading `/` into the path cell so it aligns under
                    // the "path" header.
                    let path = format!(
                        "{:<w$}",
                        truncate_right(&format!("/{}", api.path), PATH_COL_WIDTH),
                        w = PATH_COL_WIDTH
                    );
                    // Static backend (`properties.serviceUrl`). `None` means the
                    // backend is chosen in policy, mirroring the operations view.
                    let (service_url, url_style) = match api.service_url.as_deref() {
                        Some(u) => (truncate_right(u, url_width), Style::default().fg(theme.fg)),
                        None => (
                            "— (set in policy)".to_string(),
                            Style::default().fg(theme.muted),
                        ),
                    };
                    let spans = vec![
                        Span::raw(if selected { "▍ " } else { MARKER_PAD }),
                        Span::styled(name, Style::default().fg(theme.fg)),
                        Span::raw(COL_GAP),
                        Span::styled(path, Style::default().fg(theme.accent)),
                        Span::raw(COL_GAP),
                        Span::styled(service_url, url_style),
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
        "{MARKER_PAD}{:<nw$}{COL_GAP}{:<pw$}{COL_GAP}{}",
        "name",
        "path",
        "service url",
        nw = NAME_COL_WIDTH,
        pw = PATH_COL_WIDTH,
    );
    Line::from(Span::styled(head, style))
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
    // Navigation operates on the filtered slice so the cursor never points past
    // the end of what's rendered. Mirrors `storage_blobs::handle`.
    let len = state.apim.filtered_apis(&svc_id).len();

    // While the filter input has focus, swallow most actions but let the
    // dispatcher's filter-forwarding gate push raw chars into the buffer.
    // Esc cancels (deactivates AND clears); Enter commits (deactivates, keeps
    // the value). Down hands focus back to the filtered list.
    if state.apim.apis_filter_active {
        match action {
            Action::Back => {
                state.apim.apis_filter_active = false;
                state.apim.apis_filter.reset();
                state.apim.apis_cursor = 0;
                return true;
            }
            Action::OpenSelected => {
                state.apim.apis_filter_active = false;
                return true;
            }
            Action::MoveDown => {
                state.apim.apis_filter_active = false;
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
        Action::StartSearch => {
            state.apim.apis_filter.reset();
            state.apim.apis_cursor = 0;
            state.apim.apis_filter_active = true;
            true
        }
        Action::OpenSelected => {
            // Resolve via the filtered slice so the cursor's row matches what
            // the user actually sees on screen.
            let api_id = state
                .apim
                .filtered_apis(&svc_id)
                .get(state.apim.apis_cursor)
                .map(|api| api.id.clone());
            if let Some(api_id) = api_id {
                state.apim.selected_api_id = Some(api_id);
                state.apim.operations_cursor = 0;
                state.apim.operations_filter = tui_input::Input::default();
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
            created_at: None,
            modified_at: None,
            meta: Default::default(),
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
                service_url: Some("https://echo.example.com".into()),
            }],
        );
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("Echo API"));
        assert!(buf.contains("echo"));
    }

    #[test]
    fn renders_column_headers() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(120, 12);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.apim.apis.insert(
            "/svc/myapim".into(),
            vec![Api {
                id: "/svc/myapim/apis/echo".into(),
                name: "echo".into(),
                display_name: "Echo API".into(),
                path: "echo".into(),
                service_url: Some("https://echo.example.com".into()),
            }],
        );
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("name"), "name header missing: {buf}");
        assert!(buf.contains("path"), "path header missing: {buf}");
        assert!(
            buf.contains("service url"),
            "service url header missing: {buf}"
        );
    }

    #[test]
    fn renders_service_url_column() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(120, 12);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.apim.apis.insert(
            "/svc/myapim".into(),
            vec![
                Api {
                    id: "/svc/myapim/apis/echo".into(),
                    name: "echo".into(),
                    display_name: "Echo API".into(),
                    path: "echo".into(),
                    service_url: Some("https://echo.example.com".into()),
                },
                Api {
                    id: "/svc/myapim/apis/policy".into(),
                    name: "policy".into(),
                    display_name: "Policy API".into(),
                    path: "policy".into(),
                    service_url: None,
                },
            ],
        );
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        // Static backend is shown verbatim; the policy-routed API gets the
        // placeholder instead of a blank cell.
        assert!(buf.contains("https://echo.example.com"), "url cell: {buf}");
        assert!(buf.contains("set in policy"), "placeholder cell: {buf}");
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
                service_url: Some("https://echo.example.com".into()),
            }],
        );
        state.apim.operations_filter = tui_input::Input::default().with_value("stale".to_string());
        assert!(handle(Action::OpenSelected, &mut state));
        assert_eq!(state.view, View::ApimOperations);
        assert_eq!(
            state.apim.selected_api_id.as_deref(),
            Some("/svc/myapim/apis/echo")
        );
        // Drilling in must not carry a stale operations filter along.
        assert_eq!(state.apim.operations_filter.value(), "");
    }

    fn three_apis() -> AppState {
        let mut state = fixture();
        state.apim.apis.insert(
            "/svc/myapim".into(),
            vec![
                Api {
                    id: "/svc/myapim/apis/orders".into(),
                    name: "orders-api".into(),
                    display_name: "Orders API".into(),
                    path: "orders".into(),
                    service_url: None,
                },
                Api {
                    id: "/svc/myapim/apis/payments".into(),
                    name: "payments-api".into(),
                    display_name: "Payments API".into(),
                    path: "payments".into(),
                    service_url: None,
                },
                Api {
                    id: "/svc/myapim/apis/catalog".into(),
                    name: "catalog-api".into(),
                    display_name: "Catalog API".into(),
                    path: "catalog".into(),
                    service_url: None,
                },
            ],
        );
        state
    }

    #[test]
    fn slash_opens_filter_input() {
        let mut state = three_apis();
        assert!(handle(Action::StartSearch, &mut state));
        assert!(state.apim.apis_filter_active);
    }

    #[test]
    fn esc_in_filter_clears_buffer() {
        let mut state = three_apis();
        state.apim.apis_filter_active = true;
        state.apim.apis_filter = tui_input::Input::default().with_value("pay".to_string());
        assert!(handle(Action::Back, &mut state));
        assert!(!state.apim.apis_filter_active);
        assert_eq!(state.apim.apis_filter.value(), "");
    }

    #[test]
    fn enter_in_filter_keeps_value_and_deactivates() {
        let mut state = three_apis();
        state.apim.apis_filter_active = true;
        state.apim.apis_filter = tui_input::Input::default().with_value("pay".to_string());
        assert!(handle(Action::OpenSelected, &mut state));
        assert!(!state.apim.apis_filter_active);
        assert_eq!(state.apim.apis_filter.value(), "pay");
        assert_eq!(state.view, View::ApimApis);
    }

    #[test]
    fn filter_matches_display_name_path_and_slug() {
        let mut state = three_apis();
        // path match
        state.apim.apis_filter = tui_input::Input::default().with_value("catalog".to_string());
        let names: Vec<&str> = state
            .apim
            .filtered_apis("/svc/myapim")
            .iter()
            .map(|a| a.display_name.as_str())
            .collect();
        assert_eq!(names, vec!["Catalog API"]);
        // case-insensitive display-name match
        state.apim.apis_filter = tui_input::Input::default().with_value("ORDER".to_string());
        let names: Vec<&str> = state
            .apim
            .filtered_apis("/svc/myapim")
            .iter()
            .map(|a| a.display_name.as_str())
            .collect();
        assert_eq!(names, vec!["Orders API"]);
    }

    #[test]
    fn navigation_and_open_use_filtered_slice() {
        let mut state = three_apis();
        state.apim.apis_filter = tui_input::Input::default().with_value("pay".to_string());
        state.apim.apis_cursor = 0;
        // Only one match -> GotoBottom clamps to filtered len-1 == 0.
        assert!(handle(Action::GotoBottom, &mut state));
        assert_eq!(state.apim.apis_cursor, 0);
        assert!(handle(Action::OpenSelected, &mut state));
        assert_eq!(state.view, View::ApimOperations);
        assert_eq!(
            state.apim.selected_api_id.as_deref(),
            Some("/svc/myapim/apis/payments"),
            "drills into the filtered row, not the same index in the raw list",
        );
    }

    #[test]
    fn renders_filter_chip_in_title() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(100, 12);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = three_apis();
        state.apim.apis_filter = tui_input::Input::default().with_value("pay".to_string());
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(
            buf.contains("/pay"),
            "title chip should show /pay, got: {buf}"
        );
        assert!(
            buf.contains("1 of 3"),
            "count should switch to `N of M` when filtering, got: {buf}",
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
                    service_url: None,
                },
                Api {
                    id: "a2".into(),
                    name: "b".into(),
                    display_name: "B".into(),
                    path: "b".into(),
                    service_url: None,
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
