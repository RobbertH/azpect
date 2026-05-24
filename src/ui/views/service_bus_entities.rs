//! Service Bus entities drill-in: queues *or* topics inside the pinned
//! namespace in [`crate::ui::state::ServiceBusCache::selected_namespace`].
//! Tab / Shift-Tab toggles between the two kinds (mirroring the Key Vault
//! secrets/certs toggle). The active / dead-letter message counts are the
//! point of this view — a non-zero dead-letter depth is flagged in colour.
//!
//! Enter on a topic pins it and opens [`View::ServiceBusSubscriptions`]; queues
//! are terminal (Enter is a no-op there).

#![allow(dead_code, unused_variables)]

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};
use ratatui::Frame;

use crate::azure::service_bus::{EntityKind, ServiceBusQueue, ServiceBusTopic};
use crate::ui::events::Action;
use crate::ui::state::{AppState, View};
use crate::ui::theme::Theme;

const FOOTER_HINT: &str =
    "j/k move  Tab queues/topics  Enter subs (topics)  / filter  Esc back  r refresh  y yank  o portal  ? help  q quit";
const HALF_PAGE: usize = 10;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);

    let kind = state.service_bus.entity_kind;
    let kind_label = kind.label();
    let filter_value = state.service_bus.entities_filter.value();
    let filter_active = state.service_bus.entities_filter_active;

    let (total, filtered_len) = match (state.service_bus.selected_namespace.as_ref(), kind) {
        (Some(ns), EntityKind::Queue) => (
            state.service_bus.queues.get(&ns.id).map(|v| v.len()),
            state.service_bus.filtered_queues(&ns.id).len(),
        ),
        (Some(ns), EntityKind::Topic) => (
            state.service_bus.topics.get(&ns.id).map(|v| v.len()),
            state.service_bus.filtered_topics(&ns.id).len(),
        ),
        (None, _) => (None, 0),
    };
    let count_label = match total {
        Some(t) if !filter_value.is_empty() => format!("· {filtered_len} of {t} "),
        Some(t) => format!("· {t} "),
        None => String::new(),
    };
    let mut title_spans: Vec<Span> = vec![
        Span::styled(format!(" {kind_label} "), Style::default().fg(theme.fg)),
        Span::styled(count_label, Style::default().fg(theme.muted)),
        Span::styled("[Tab: switch]", Style::default().fg(theme.muted)),
        Span::raw(" "),
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

    let Some(ns) = state.service_bus.selected_namespace.as_ref() else {
        let p = Paragraph::new(Line::from(Span::styled(
            "no namespace selected.",
            Style::default().fg(theme.muted),
        )));
        frame.render_widget(p, body_area);
        render_footer(frame, chunks[1], theme);
        return;
    };

    let (error, pending, has_rows) = match kind {
        EntityKind::Queue => (
            state.service_bus.queues_error.get(&ns.id),
            state.service_bus.queues_pending.contains(&ns.id),
            state.service_bus.queues.contains_key(&ns.id),
        ),
        EntityKind::Topic => (
            state.service_bus.topics_error.get(&ns.id),
            state.service_bus.topics_pending.contains(&ns.id),
            state.service_bus.topics.contains_key(&ns.id),
        ),
    };

    if let Some(err) = error {
        let p = Paragraph::new(Line::from(Span::styled(
            format!("error: {err}"),
            Style::default().fg(theme.critical),
        )));
        frame.render_widget(p, body_area);
        render_footer(frame, chunks[1], theme);
        return;
    }

    if !has_rows {
        let msg = if pending {
            format!("loading {kind_label} …")
        } else {
            format!("press r to load {kind_label}.")
        };
        let p = Paragraph::new(Line::from(Span::styled(
            msg,
            Style::default().fg(theme.muted),
        )));
        frame.render_widget(p, body_area);
        render_footer(frame, chunks[1], theme);
        return;
    }

    match kind {
        EntityKind::Queue => render_queues(frame, body_area, state, &ns.id, theme),
        EntityKind::Topic => render_topics(frame, body_area, state, &ns.id, theme),
    }

    render_footer(frame, chunks[1], theme);
}

fn render_queues(frame: &mut Frame, area: Rect, state: &AppState, ns_id: &str, theme: &Theme) {
    let filtered = state.service_bus.filtered_queues(ns_id);
    if filtered.is_empty() {
        let msg = if state.service_bus.entities_filter.value().is_empty() {
            "no queues in this namespace."
        } else {
            "no queues match the current filter."
        };
        let p = Paragraph::new(Line::from(Span::styled(
            msg,
            Style::default().fg(theme.muted),
        )));
        frame.render_widget(p, area);
        return;
    }

    let name_w = filtered
        .iter()
        .map(|q| q.name.chars().count() as u16)
        .max()
        .unwrap_or(0)
        .max(4);

    let widths = [
        Constraint::Length(name_w), // NAME
        Constraint::Length(10),     // STATUS
        Constraint::Length(8),      // ACTIVE
        Constraint::Length(8),      // DLQ
        Constraint::Length(8),      // SCHED
        Constraint::Length(8),      // TOTAL
        Constraint::Length(7),      // MAXDEL
    ];
    let header_row = Row::new(vec![
        "NAME", "STATUS", "ACTIVE", "DLQ", "SCHED", "TOTAL", "MAXDEL",
    ])
    .style(
        Style::default()
            .fg(theme.muted)
            .add_modifier(Modifier::BOLD),
    );

    let cursor = state.service_bus.entities_cursor.min(filtered.len() - 1);
    let body_rows: Vec<Row> = filtered.iter().map(|q| queue_row(q, theme)).collect();

    let table = Table::new(body_rows, widths)
        .header(header_row)
        .row_highlight_style(theme.selection())
        .highlight_symbol("▍ ")
        .column_spacing(2);

    let mut ts = TableState::default();
    ts.select(Some(cursor));
    frame.render_stateful_widget(table, area, &mut ts);
}

fn queue_row<'a>(q: &'a ServiceBusQueue, theme: &Theme) -> Row<'a> {
    Row::new(vec![
        Cell::from(q.name.as_str()).style(Style::default().fg(theme.fg)),
        status_cell(q.status.as_deref(), theme),
        Cell::from(q.counts.active.to_string()).style(Style::default().fg(theme.fg)),
        dlq_cell(q.counts.dead_letter, theme),
        Cell::from(q.counts.scheduled.to_string()).style(Style::default().fg(theme.muted)),
        Cell::from(opt_num(q.total_message_count)).style(Style::default().fg(theme.muted)),
        Cell::from(opt_num(q.max_delivery_count)).style(Style::default().fg(theme.muted)),
    ])
}

fn render_topics(frame: &mut Frame, area: Rect, state: &AppState, ns_id: &str, theme: &Theme) {
    let filtered = state.service_bus.filtered_topics(ns_id);
    if filtered.is_empty() {
        let msg = if state.service_bus.entities_filter.value().is_empty() {
            "no topics in this namespace."
        } else {
            "no topics match the current filter."
        };
        let p = Paragraph::new(Line::from(Span::styled(
            msg,
            Style::default().fg(theme.muted),
        )));
        frame.render_widget(p, area);
        return;
    }

    let name_w = filtered
        .iter()
        .map(|t| t.name.chars().count() as u16)
        .max()
        .unwrap_or(0)
        .max(4);

    let widths = [
        Constraint::Length(name_w), // NAME
        Constraint::Length(10),     // STATUS
        Constraint::Length(6),      // SUBS
        Constraint::Length(12),     // SIZE
    ];
    let header_row = Row::new(vec!["NAME", "STATUS", "SUBS", "SIZE"]).style(
        Style::default()
            .fg(theme.muted)
            .add_modifier(Modifier::BOLD),
    );

    let cursor = state.service_bus.entities_cursor.min(filtered.len() - 1);
    let body_rows: Vec<Row> = filtered.iter().map(|t| topic_row(t, theme)).collect();

    let table = Table::new(body_rows, widths)
        .header(header_row)
        .row_highlight_style(theme.selection())
        .highlight_symbol("▍ ")
        .column_spacing(2);

    let mut ts = TableState::default();
    ts.select(Some(cursor));
    frame.render_stateful_widget(table, area, &mut ts);
}

fn topic_row<'a>(t: &'a ServiceBusTopic, theme: &Theme) -> Row<'a> {
    Row::new(vec![
        Cell::from(t.name.as_str()).style(Style::default().fg(theme.fg)),
        status_cell(t.status.as_deref(), theme),
        Cell::from(opt_num(t.subscription_count)).style(Style::default().fg(theme.fg)),
        Cell::from(format_bytes(t.size_bytes)).style(Style::default().fg(theme.muted)),
    ])
}

/// Dead-letter cell: zero is muted, any positive depth is flagged so a backed-up
/// dead-letter queue jumps out while scanning the list.
fn dlq_cell<'a>(count: i64, theme: &Theme) -> Cell<'a> {
    if count > 0 {
        Cell::from(count.to_string()).style(
            Style::default()
                .fg(theme.critical)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Cell::from("0").style(Style::default().fg(theme.muted))
    }
}

fn status_cell<'a>(status: Option<&str>, theme: &Theme) -> Cell<'a> {
    match status {
        Some("Active") => Cell::from("Active").style(Style::default().fg(theme.healthy)),
        Some(s) => Cell::from(s.to_string()).style(Style::default().fg(theme.degraded)),
        None => Cell::from("?").style(Style::default().fg(theme.muted)),
    }
}

fn opt_num(v: Option<i64>) -> String {
    match v {
        Some(n) => n.to_string(),
        None => "—".to_string(),
    }
}

/// Human-readable byte size (B / KiB / MiB / GiB). `None` → "—".
fn format_bytes(bytes: Option<i64>) -> String {
    let Some(b) = bytes else {
        return "—".to_string();
    };
    if b < 1024 {
        return format!("{b} B");
    }
    let units = ["KiB", "MiB", "GiB", "TiB"];
    let mut value = b as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < units.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", units[unit])
}

fn render_footer(frame: &mut Frame, area: Rect, theme: &Theme) {
    let p = Paragraph::new(Line::from(Span::styled(
        FOOTER_HINT,
        Style::default().fg(theme.muted),
    )));
    frame.render_widget(p, area);
}

/// Bare name of the entity (queue or topic) currently under the cursor. The
/// global yank handler prepends the namespace name.
pub fn yank_text(state: &AppState) -> Option<String> {
    let ns = state.service_bus.selected_namespace.as_ref()?;
    match state.service_bus.entity_kind {
        EntityKind::Queue => state
            .service_bus
            .filtered_queues(&ns.id)
            .get(state.service_bus.entities_cursor)
            .map(|q| q.name.clone()),
        EntityKind::Topic => state
            .service_bus
            .filtered_topics(&ns.id)
            .get(state.service_bus.entities_cursor)
            .map(|t| t.name.clone()),
    }
}

/// Toggle between queues and topics, resetting the shared cursor + filter so
/// the user lands at the top of the other list cleanly.
fn toggle_kind(state: &mut AppState) {
    state.service_bus.entity_kind = match state.service_bus.entity_kind {
        EntityKind::Queue => EntityKind::Topic,
        EntityKind::Topic => EntityKind::Queue,
    };
    state.service_bus.entities_cursor = 0;
    state.service_bus.entities_filter = tui_input::Input::default();
}

/// Length of the list currently on screen, after the active filter.
fn current_len(state: &AppState) -> usize {
    let Some(ns) = state.service_bus.selected_namespace.as_ref() else {
        return 0;
    };
    match state.service_bus.entity_kind {
        EntityKind::Queue => state.service_bus.filtered_queues(&ns.id).len(),
        EntityKind::Topic => state.service_bus.filtered_topics(&ns.id).len(),
    }
}

pub fn handle(action: Action, state: &mut AppState) -> bool {
    if state.service_bus.selected_namespace.is_none() {
        return false;
    }
    let len = current_len(state);

    if state.service_bus.entities_filter_active {
        match action {
            Action::Back => {
                state.service_bus.entities_filter_active = false;
                state.service_bus.entities_filter.reset();
                state.service_bus.entities_cursor = 0;
                return true;
            }
            Action::OpenSelected => {
                state.service_bus.entities_filter_active = false;
                return true;
            }
            Action::MoveDown => {
                state.service_bus.entities_filter_active = false;
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
                state.service_bus.entities_cursor =
                    (state.service_bus.entities_cursor + 1).min(len - 1);
            }
            true
        }
        Action::MoveUp => {
            state.service_bus.entities_cursor = state.service_bus.entities_cursor.saturating_sub(1);
            true
        }
        Action::HalfPageDown => {
            if len > 0 {
                state.service_bus.entities_cursor =
                    (state.service_bus.entities_cursor + HALF_PAGE).min(len - 1);
            }
            true
        }
        Action::HalfPageUp => {
            state.service_bus.entities_cursor =
                state.service_bus.entities_cursor.saturating_sub(HALF_PAGE);
            true
        }
        Action::GotoTop => {
            state.service_bus.entities_cursor = 0;
            true
        }
        Action::GotoBottom => {
            if len > 0 {
                state.service_bus.entities_cursor = len - 1;
            }
            true
        }
        Action::StartSearch => {
            state.service_bus.entities_filter_active = true;
            true
        }
        Action::NextPanel | Action::PrevPanel => {
            toggle_kind(state);
            true
        }
        Action::OpenSelected => {
            // Topics drill into their subscriptions; queues are terminal.
            if state.service_bus.entity_kind == EntityKind::Topic {
                let ns_id = state
                    .service_bus
                    .selected_namespace
                    .as_ref()
                    .map(|n| n.id.clone());
                let topic = ns_id.and_then(|id| {
                    state
                        .service_bus
                        .filtered_topics(&id)
                        .get(state.service_bus.entities_cursor)
                        .map(|t| t.name.clone())
                });
                if let Some(topic) = topic {
                    state.service_bus.selected_topic = Some(topic);
                    state.service_bus.subscriptions_cursor = 0;
                    state.service_bus.subscriptions_filter = tui_input::Input::default();
                    state.view_stack.push(state.view);
                    state.view = View::ServiceBusSubscriptions;
                }
            }
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::service_bus::{CountDetails, ServiceBusNamespace, ServiceBusTopic};
    use crate::config::Config;
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
        state.view = View::ServiceBusEntities;
        state.service_bus.selected_namespace = Some(namespace());
        state
    }

    fn queue(name: &str, active: i64, dead_letter: i64) -> ServiceBusQueue {
        ServiceBusQueue {
            id: format!("/subs/s/rg/r/providers/Microsoft.ServiceBus/namespaces/ns/queues/{name}"),
            name: name.into(),
            status: Some("Active".into()),
            total_message_count: Some(active + dead_letter),
            counts: CountDetails {
                active,
                dead_letter,
                ..Default::default()
            },
            max_delivery_count: Some(10),
            size_bytes: Some(2048),
            requires_session: Some(false),
            updated_at: None,
        }
    }

    fn topic(name: &str, subs: i64) -> ServiceBusTopic {
        ServiceBusTopic {
            id: format!("/subs/s/rg/r/providers/Microsoft.ServiceBus/namespaces/ns/topics/{name}"),
            name: name.into(),
            status: Some("Active".into()),
            subscription_count: Some(subs),
            size_bytes: Some(1024),
            updated_at: None,
        }
    }

    #[test]
    fn renders_queue_counts() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(120, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state
            .service_bus
            .queues
            .insert(namespace().id, vec![queue("orders", 40, 3)]);
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("orders"), "queue name should render");
        assert!(buf.contains("ACTIVE"), "active header should render");
        assert!(buf.contains("DLQ"), "dlq header should render");
    }

    #[test]
    fn tab_toggles_kind_and_resets_cursor() {
        let mut state = fixture();
        state.service_bus.entities_cursor = 4;
        assert_eq!(state.service_bus.entity_kind, EntityKind::Queue);
        assert!(handle(Action::NextPanel, &mut state));
        assert_eq!(state.service_bus.entity_kind, EntityKind::Topic);
        assert_eq!(state.service_bus.entities_cursor, 0);
        assert!(handle(Action::PrevPanel, &mut state));
        assert_eq!(state.service_bus.entity_kind, EntityKind::Queue);
    }

    #[test]
    fn enter_on_topic_drills_into_subscriptions() {
        let mut state = fixture();
        state.service_bus.entity_kind = EntityKind::Topic;
        state
            .service_bus
            .topics
            .insert(namespace().id, vec![topic("events", 2)]);
        assert!(handle(Action::OpenSelected, &mut state));
        assert_eq!(state.view, View::ServiceBusSubscriptions);
        assert_eq!(state.service_bus.selected_topic.as_deref(), Some("events"));
    }

    #[test]
    fn enter_on_queue_is_terminal() {
        let mut state = fixture();
        state.service_bus.entity_kind = EntityKind::Queue;
        state
            .service_bus
            .queues
            .insert(namespace().id, vec![queue("orders", 1, 0)]);
        assert!(handle(Action::OpenSelected, &mut state));
        // Stays put — queues have no deeper level.
        assert_eq!(state.view, View::ServiceBusEntities);
        assert!(state.service_bus.selected_topic.is_none());
    }

    #[test]
    fn yank_returns_selected_entity_name() {
        let mut state = fixture();
        state.service_bus.queues.insert(
            namespace().id,
            vec![queue("first", 0, 0), queue("second", 0, 0)],
        );
        state.service_bus.entities_cursor = 1;
        assert_eq!(yank_text(&state).as_deref(), Some("second"));
    }

    #[test]
    fn format_bytes_scales_units() {
        assert_eq!(format_bytes(None), "—");
        assert_eq!(format_bytes(Some(512)), "512 B");
        assert_eq!(format_bytes(Some(2048)), "2.0 KiB");
        assert_eq!(format_bytes(Some(5 * 1024 * 1024)), "5.0 MiB");
    }
}
