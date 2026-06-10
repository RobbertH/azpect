//! Top-level Service Bus mode entry view: lists namespaces visible to the
//! current subscription scope. Pressing Enter on a row pins the namespace into
//! `state.service_bus.selected_namespace` and opens [`View::ServiceBusEntities`].

#![allow(dead_code, unused_variables)]

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};
use ratatui::Frame;

use super::{name_col_width, truncate_ellipsis};
use crate::azure::service_bus::ServiceBusNamespace;
use crate::ui::events::Action;
use crate::ui::state::{subscription_display_name, AppState, View};
use crate::ui::theme::Theme;

const FOOTER_HINT: &str =
    "j/k move  Enter entities  / filter  Esc back  r refresh  y yank id  o portal  ? help  q quit";
const HALF_PAGE: usize = 10;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);

    let filter_value = state.service_bus.namespaces_filter.value();
    let filter_active = state.service_bus.namespaces_filter_active;
    let total = state.service_bus.namespaces.as_ref().map(|v| v.len());
    let filtered = state.service_bus.filtered_namespaces();
    let count_label = match total {
        Some(t) if !filter_value.is_empty() => format!("· {} of {} ", filtered.len(), t),
        Some(t) => format!("· {t} "),
        None => String::new(),
    };
    let mut title_spans: Vec<Span> = vec![
        Span::styled(" service bus ", Style::default().fg(theme.fg)),
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

    if let Some(err) = state.service_bus.namespaces_error.as_deref() {
        let p = Paragraph::new(Line::from(Span::styled(
            format!("error: {err}"),
            Style::default().fg(theme.critical),
        )));
        frame.render_widget(p, body_area);
        render_footer(frame, chunks[1], theme);
        return;
    }

    match state.service_bus.namespaces.as_deref() {
        None if state.service_bus.namespaces_pending => {
            let p = Paragraph::new(Line::from(Span::styled(
                "loading service bus namespaces …",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        None => {
            let p = Paragraph::new(Line::from(Span::styled(
                "press r to load service bus namespaces.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some([]) => {
            let p = Paragraph::new(Line::from(Span::styled(
                "No Service Bus namespaces found in selected subscriptions.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(_) if filtered.is_empty() => {
            let p = Paragraph::new(Line::from(Span::styled(
                "no namespaces match the current filter.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(_) => {
            let show_sub_cols = state.selected_subscription.is_none();

            // NAME absorbs the leftover width; on a narrow terminal it caps to
            // the budget and truncates with an ellipsis (see `build_row`) rather
            // than the table clipping it silently. `fixed_w` sums the Length()s
            // below — keep them in sync.
            let fixed_w: u16 = 10 + 10 + 22 + 10 + if show_sub_cols { 22 } else { 0 } + 14;
            let n_cols: u16 = if show_sub_cols { 7 } else { 6 };
            let longest = filtered
                .iter()
                .map(|n| n.name.chars().count() as u16)
                .max()
                .unwrap_or(0);
            let name_w = name_col_width(body_area.width, fixed_w, n_cols, longest);

            let mut widths: Vec<Constraint> = vec![
                Constraint::Length(name_w), // NAME
                Constraint::Length(10),     // SKU
                Constraint::Length(10),     // STATUS
                Constraint::Length(22),     // RG
                Constraint::Length(10),     // CREATED
            ];
            let mut headers: Vec<&'static str> =
                vec!["NAME", "SKU", "STATUS", "RESOURCE GROUP", "CREATED"];
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

            let cursor = state.service_bus.namespaces_cursor.min(filtered.len() - 1);
            let body_rows: Vec<Row> = filtered
                .iter()
                .map(|ns| build_row(ns, state, show_sub_cols, name_w, theme))
                .collect();

            let table = Table::new(body_rows, widths)
                .header(header_row)
                .row_highlight_style(theme.selection())
                .highlight_symbol("▍ ")
                .column_spacing(2);

            let mut ts = TableState::default();
            ts.select(Some(cursor));
            frame.render_stateful_widget(table, body_area, &mut ts);
        }
    }

    render_footer(frame, chunks[1], theme);
}

fn build_row<'a>(
    ns: &'a ServiceBusNamespace,
    state: &'a AppState,
    show_sub_cols: bool,
    name_w: u16,
    theme: &Theme,
) -> Row<'a> {
    let status = match ns.status.as_deref() {
        Some("Active") => Cell::from("Active").style(Style::default().fg(theme.healthy)),
        Some(s) => Cell::from(s.to_string()).style(Style::default().fg(theme.degraded)),
        None => Cell::from("?").style(Style::default().fg(theme.muted)),
    };
    let mut cells: Vec<Cell> = vec![
        Cell::from(truncate_ellipsis(&ns.name, name_w as usize))
            .style(Style::default().fg(theme.fg)),
        Cell::from(ns.sku.as_deref().unwrap_or("—").to_string())
            .style(Style::default().fg(theme.fg)),
        status,
        Cell::from(ns.resource_group.as_str()).style(Style::default().fg(theme.muted)),
        Cell::from(format_date(ns.created_at.as_ref())).style(Style::default().fg(theme.muted)),
    ];
    if show_sub_cols {
        cells.push(
            Cell::from(
                subscription_display_name(state, &ns.subscription_id)
                    .unwrap_or("")
                    .to_string(),
            )
            .style(Style::default().fg(theme.muted)),
        );
    }
    cells.push(Cell::from(ns.location.as_str()).style(Style::default().fg(theme.muted)));
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
    let len = state.service_bus.filtered_namespaces().len();

    if state.service_bus.namespaces_filter_active {
        match action {
            Action::Back => {
                state.service_bus.namespaces_filter_active = false;
                state.service_bus.namespaces_filter.reset();
                state.service_bus.namespaces_cursor = 0;
                return true;
            }
            Action::OpenSelected => {
                state.service_bus.namespaces_filter_active = false;
                return true;
            }
            Action::MoveDown => {
                state.service_bus.namespaces_filter_active = false;
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
                state.service_bus.namespaces_cursor =
                    (state.service_bus.namespaces_cursor + 1).min(len - 1);
            }
            true
        }
        Action::MoveUp => {
            state.service_bus.namespaces_cursor =
                state.service_bus.namespaces_cursor.saturating_sub(1);
            true
        }
        Action::HalfPageDown => {
            if len > 0 {
                state.service_bus.namespaces_cursor =
                    (state.service_bus.namespaces_cursor + HALF_PAGE).min(len - 1);
            }
            true
        }
        Action::HalfPageUp => {
            state.service_bus.namespaces_cursor = state
                .service_bus
                .namespaces_cursor
                .saturating_sub(HALF_PAGE);
            true
        }
        Action::GotoTop => {
            state.service_bus.namespaces_cursor = 0;
            true
        }
        Action::GotoBottom => {
            if len > 0 {
                state.service_bus.namespaces_cursor = len - 1;
            }
            true
        }
        Action::StartSearch => {
            state.service_bus.namespaces_filter_active = true;
            true
        }
        Action::OpenSelected => {
            let ns = state
                .service_bus
                .filtered_namespaces()
                .get(state.service_bus.namespaces_cursor)
                .copied()
                .cloned();
            if let Some(ns) = ns {
                state.service_bus.selected_namespace = Some(ns);
                // Fresh entry into the entities view: default to queues, reset
                // the shared cursor / filter.
                state.service_bus.entity_kind = crate::azure::service_bus::EntityKind::Queue;
                state.service_bus.entities_cursor = 0;
                state.service_bus.entities_filter = tui_input::Input::default();
                state.service_bus.selected_topic = None;
                state.view_stack.push(state.view);
                state.view = View::ServiceBusEntities;
            }
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::service_bus::ServiceBusNamespace;
    use crate::config::Config;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn fixture() -> AppState {
        let mut state = AppState::new(Config::default());
        state.view = View::ServiceBusNamespaces;
        state
    }

    fn namespace(name: &str) -> ServiceBusNamespace {
        ServiceBusNamespace {
            id: format!(
                "/subscriptions/sub/resourceGroups/rg/providers/Microsoft.ServiceBus/namespaces/{name}",
            ),
            name: name.into(),
            resource_group: "rg".into(),
            subscription_id: "sub".into(),
            location: "westeurope".into(),
            sku: Some("Standard".into()),
            status: Some("Active".into()),
            endpoint: Some(format!("https://{name}.servicebus.windows.net:443/")),
            created_at: None,
        }
    }

    #[test]
    fn renders_loading_state() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(160, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.service_bus.namespaces_pending = true;
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("loading service bus namespaces"));
    }

    #[test]
    fn renders_namespace_row() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(180, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.service_bus.namespaces = Some(vec![namespace("orders-ns")]);
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("orders-ns"), "name should render");
        assert!(buf.contains("Standard"), "sku should render");
    }

    #[test]
    fn enter_pins_namespace_and_drills_in() {
        let mut state = fixture();
        state.service_bus.namespaces = Some(vec![namespace("ns")]);
        state.service_bus.entity_kind = crate::azure::service_bus::EntityKind::Topic;
        assert!(handle(Action::OpenSelected, &mut state));
        assert_eq!(state.view, View::ServiceBusEntities);
        assert_eq!(
            state
                .service_bus
                .selected_namespace
                .as_ref()
                .map(|n| n.name.as_str()),
            Some("ns")
        );
        // Drilling in resets the toggle back to queues.
        assert_eq!(
            state.service_bus.entity_kind,
            crate::azure::service_bus::EntityKind::Queue
        );
    }
}
