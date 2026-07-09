//! Service Bus subscriptions drill-in: lists the subscriptions on the pinned
//! topic ([`crate::ui::state::ServiceBusCache::selected_topic`]) inside the
//! pinned namespace, with their active / dead-letter message counts. Terminal
//! view — subscriptions have no deeper level.

#![allow(dead_code, unused_variables)]

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;

use crate::azure::service_bus::ServiceBusSubscription;
use crate::ui::events::Action;
use crate::ui::state::{AppState, ServiceBusCache};
use crate::ui::theme::Theme;

const FOOTER_HINT: &str =
    "j/k move  / filter  Esc back  r refresh  y yank name  o portal  ? help  q quit";
const HALF_PAGE: usize = 10;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);

    let filter_value = state.service_bus.subscriptions_filter.value();
    let filter_active = state.service_bus.subscriptions_filter_active;

    let cache_key = match (
        state.service_bus.selected_namespace.as_ref(),
        state.service_bus.selected_topic.as_deref(),
    ) {
        (Some(ns), Some(topic)) => Some(ServiceBusCache::subscriptions_key(&ns.id, topic)),
        _ => None,
    };
    let total = cache_key
        .as_ref()
        .and_then(|k| state.service_bus.subscriptions.get(k))
        .map(|v| v.len());
    let filtered: Vec<&ServiceBusSubscription> = match (
        state.service_bus.selected_namespace.as_ref(),
        state.service_bus.selected_topic.as_deref(),
    ) {
        (Some(ns), Some(topic)) => state.service_bus.filtered_subscriptions(&ns.id, topic),
        _ => Vec::new(),
    };
    let count_label = match total {
        Some(t) if !filter_value.is_empty() => format!("· {} of {} ", filtered.len(), t),
        Some(t) => format!("· {t} "),
        None => String::new(),
    };
    let topic_label = state
        .service_bus
        .selected_topic
        .as_deref()
        .unwrap_or("subscriptions");
    let mut title_spans: Vec<Span> = vec![
        Span::styled(format!(" {topic_label} "), Style::default().fg(theme.fg)),
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

    let Some(key) = cache_key else {
        let p = Paragraph::new(Line::from(Span::styled(
            "no topic selected.",
            Style::default().fg(theme.muted),
        )));
        frame.render_widget(p, body_area);
        render_footer(frame, chunks[1], theme);
        return;
    };

    if let Some(err) = state.service_bus.subscriptions_error.get(&key) {
        // `Text` keeps any line breaks from a pretty-printed JSON error body;
        // `wrap` folds long lines so nothing runs off the right edge.
        let p = Paragraph::new(Text::styled(
            format!("error: {err}"),
            Style::default().fg(theme.critical),
        ))
        .wrap(Wrap { trim: false });
        frame.render_widget(p, body_area);
        render_footer(frame, chunks[1], theme);
        return;
    }

    let subs = state.service_bus.subscriptions.get(&key);
    let loading = state.service_bus.subscriptions_pending.contains(&key);
    match subs {
        None if loading => {
            render_msg(frame, body_area, theme, "loading subscriptions …");
        }
        None => {
            render_msg(frame, body_area, theme, "press r to load subscriptions.");
        }
        Some(rows) if rows.is_empty() => {
            render_msg(frame, body_area, theme, "no subscriptions on this topic.");
        }
        Some(_) if filtered.is_empty() => {
            render_msg(
                frame,
                body_area,
                theme,
                "no subscriptions match the current filter.",
            );
        }
        Some(_) => {
            let name_w = filtered
                .iter()
                .map(|s| s.name.chars().count() as u16)
                .max()
                .unwrap_or(0)
                .max(4);

            let widths = [
                Constraint::Length(name_w), // NAME
                Constraint::Length(10),     // STATUS
                Constraint::Length(8),      // ACTIVE
                Constraint::Length(8),      // DLQ
                Constraint::Length(8),      // TOTAL
                Constraint::Length(7),      // MAXDEL
                Constraint::Min(10),        // FORWARD TO
            ];
            let header_row = Row::new(vec![
                "NAME",
                "STATUS",
                "ACTIVE",
                "DLQ",
                "TOTAL",
                "MAXDEL",
                "FORWARD TO",
            ])
            .style(
                Style::default()
                    .fg(theme.muted)
                    .add_modifier(Modifier::BOLD),
            );

            let cursor = state
                .service_bus
                .subscriptions_cursor
                .min(filtered.len() - 1);
            let body_rows: Vec<Row> = filtered.iter().map(|s| build_row(s, theme)).collect();

            let table = Table::new(body_rows, widths)
                .header(header_row)
                .row_highlight_style(theme.selection())
                .highlight_symbol("▍ ")
                .column_spacing(2);

            // Offset persisted across frames so the window only scrolls when the
            // cursor pushes against an edge (ratatui reconciles it during render).
            let mut ts =
                TableState::default().with_offset(state.service_bus.subscriptions_view_top.get());
            ts.select(Some(cursor));
            frame.render_stateful_widget(table, body_area, &mut ts);
            state.service_bus.subscriptions_view_top.set(ts.offset());
        }
    }

    render_footer(frame, chunks[1], theme);
}

fn build_row<'a>(s: &'a ServiceBusSubscription, theme: &Theme) -> Row<'a> {
    let status = match s.status.as_deref() {
        Some("Active") => Cell::from("Active").style(Style::default().fg(theme.healthy)),
        Some(st) => Cell::from(st.to_string()).style(Style::default().fg(theme.degraded)),
        None => Cell::from("?").style(Style::default().fg(theme.muted)),
    };
    let dlq = if s.counts.dead_letter > 0 {
        Cell::from(s.counts.dead_letter.to_string()).style(
            Style::default()
                .fg(theme.critical)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Cell::from("0").style(Style::default().fg(theme.muted))
    };
    Row::new(vec![
        Cell::from(s.name.as_str()).style(Style::default().fg(theme.fg)),
        status,
        Cell::from(s.counts.active.to_string()).style(Style::default().fg(theme.fg)),
        dlq,
        Cell::from(opt_num(s.total_message_count)).style(Style::default().fg(theme.muted)),
        Cell::from(opt_num(s.max_delivery_count)).style(Style::default().fg(theme.muted)),
        Cell::from(s.forward_to.as_deref().unwrap_or("—").to_string())
            .style(Style::default().fg(theme.muted)),
    ])
}

fn opt_num(v: Option<i64>) -> String {
    match v {
        Some(n) => n.to_string(),
        None => "—".to_string(),
    }
}

fn render_msg(frame: &mut Frame, area: Rect, theme: &Theme, msg: &str) {
    let p = Paragraph::new(Line::from(Span::styled(
        msg.to_string(),
        Style::default().fg(theme.muted),
    )));
    frame.render_widget(p, area);
}

fn render_footer(frame: &mut Frame, area: Rect, theme: &Theme) {
    let p = Paragraph::new(Line::from(Span::styled(
        FOOTER_HINT,
        Style::default().fg(theme.muted),
    )));
    frame.render_widget(p, area);
}

pub fn handle(action: Action, state: &mut AppState) -> bool {
    let (Some(ns_id), Some(topic)) = (
        state
            .service_bus
            .selected_namespace
            .as_ref()
            .map(|n| n.id.clone()),
        state.service_bus.selected_topic.clone(),
    ) else {
        return false;
    };
    let len = state
        .service_bus
        .filtered_subscriptions(&ns_id, &topic)
        .len();

    if state.service_bus.subscriptions_filter_active {
        match action {
            Action::Back => {
                state.service_bus.subscriptions_filter_active = false;
                state.service_bus.subscriptions_filter.reset();
                state.service_bus.subscriptions_cursor = 0;
                return true;
            }
            Action::OpenSelected => {
                state.service_bus.subscriptions_filter_active = false;
                return true;
            }
            Action::MoveDown => {
                state.service_bus.subscriptions_filter_active = false;
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
                state.service_bus.subscriptions_cursor =
                    (state.service_bus.subscriptions_cursor + 1).min(len - 1);
            }
            true
        }
        Action::MoveUp => {
            state.service_bus.subscriptions_cursor =
                state.service_bus.subscriptions_cursor.saturating_sub(1);
            true
        }
        Action::HalfPageDown => {
            if len > 0 {
                state.service_bus.subscriptions_cursor =
                    (state.service_bus.subscriptions_cursor + HALF_PAGE).min(len - 1);
            }
            true
        }
        Action::HalfPageUp => {
            state.service_bus.subscriptions_cursor = state
                .service_bus
                .subscriptions_cursor
                .saturating_sub(HALF_PAGE);
            true
        }
        Action::GotoTop => {
            state.service_bus.subscriptions_cursor = 0;
            true
        }
        Action::GotoBottom => {
            if len > 0 {
                state.service_bus.subscriptions_cursor = len - 1;
            }
            true
        }
        Action::StartSearch => {
            state.service_bus.subscriptions_filter.reset();
            state.service_bus.subscriptions_cursor = 0;
            state.service_bus.subscriptions_filter_active = true;
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::service_bus::{CountDetails, ServiceBusNamespace, ServiceBusSubscription};
    use crate::config::Config;
    use crate::ui::state::View;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn namespace() -> ServiceBusNamespace {
        ServiceBusNamespace {
            id: "/subs/s/rg/r/providers/Microsoft.ServiceBus/namespaces/ns".into(),
            name: "ns".into(),
            resource_group: "rg".into(),
            subscription_id: "s".into(),
            location: "westeurope".into(),
            sku: Some("Standard".into()),
            status: Some("Active".into()),
            endpoint: None,
            created_at: None,
        }
    }

    fn fixture() -> AppState {
        let mut state = AppState::new(Config::default());
        state.view = View::ServiceBusSubscriptions;
        state.service_bus.selected_namespace = Some(namespace());
        state.service_bus.selected_topic = Some("events".into());
        state
    }

    fn sub(name: &str, dead_letter: i64) -> ServiceBusSubscription {
        ServiceBusSubscription {
            id: format!("/x/topics/events/subscriptions/{name}"),
            name: name.into(),
            status: Some("Active".into()),
            total_message_count: Some(dead_letter + 1),
            counts: CountDetails {
                active: 1,
                dead_letter,
                ..Default::default()
            },
            max_delivery_count: Some(10),
            requires_session: Some(false),
            forward_to: None,
            updated_at: None,
        }
    }

    #[test]
    fn renders_subscription_with_dlq() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(140, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        let key = ServiceBusCache::subscriptions_key(&namespace().id, "events");
        state
            .service_bus
            .subscriptions
            .insert(key, vec![sub("audit", 5)]);
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("audit"), "subscription name should render");
        assert!(buf.contains("DLQ"), "dlq header should render");
    }

    #[test]
    fn cursor_moves_within_bounds() {
        let mut state = fixture();
        let key = ServiceBusCache::subscriptions_key(&namespace().id, "events");
        state
            .service_bus
            .subscriptions
            .insert(key, vec![sub("a", 0), sub("b", 0)]);
        assert!(handle(Action::MoveDown, &mut state));
        assert_eq!(state.service_bus.subscriptions_cursor, 1);
        // Can't move past the last row.
        assert!(handle(Action::MoveDown, &mut state));
        assert_eq!(state.service_bus.subscriptions_cursor, 1);
        assert!(handle(Action::MoveUp, &mut state));
        assert_eq!(state.service_bus.subscriptions_cursor, 0);
    }
}
