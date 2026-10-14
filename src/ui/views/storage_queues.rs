//! Storage queues drill-in: lists the queues under the pinned account in
//! [`crate::ui::state::StorageCache::selected_account`] with their approximate
//! message counts. Opened with `q` from the account overview; Enter on a row
//! pins the queue and opens [`View::StorageQueueMessages`] (a read-only
//! peek). `/` filters by a case-insensitive substring of the queue name.
//!
//! The names come from the control plane, the counts from the data plane, so
//! an identity with `Reader` but no `Storage Queue Data Reader` still gets the
//! list — the MESSAGES column shows `—` and a one-line strip says why.
//! Non-empty `-poison` queues (where Azure Functions parks messages that keep
//! failing) are flagged in the critical colour.

#![allow(dead_code, unused_variables)]

use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;

use crate::azure::storage::StorageQueue;
use crate::ui::events::Action;
use crate::ui::state::{AppState, View};
use crate::ui::theme::Theme;
use crate::ui::views::detail::format_count;

const FOOTER_HINT: &str =
    "j/k move  Enter peek messages  / filter  Esc back  r refresh  y yank name  ? help";
const HALF_PAGE: usize = 10;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);

    let filter_value = state.storage.queues_filter.value();
    let filter_active = state.storage.queues_filter_active;
    let listing = state
        .storage
        .selected_account
        .as_ref()
        .and_then(|a| state.storage.queues.get(&a.id));
    let filtered = state
        .storage
        .selected_account
        .as_ref()
        .map(|a| state.storage.filtered_queues(&a.id))
        .unwrap_or_default();
    let count_label = match listing.map(|l| l.queues.len()) {
        Some(t) if !filter_value.is_empty() => format!("· {} of {} ", filtered.len(), t),
        Some(t) => format!("· {t} "),
        None => String::new(),
    };
    let mut title_spans: Vec<Span> = vec![
        Span::styled(" queues ", Style::default().fg(theme.fg)),
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

    let Some(account) = state.storage.selected_account.as_ref() else {
        let p = Paragraph::new(Line::from(Span::styled(
            "no storage account selected.",
            Style::default().fg(theme.muted),
        )));
        frame.render_widget(p, body_area);
        render_footer(frame, chunks[1], theme);
        return;
    };

    if let Some(err) = state.storage.queues_error.get(&account.id) {
        let p = Paragraph::new(Text::styled(
            format!("error: {err}"),
            Style::default().fg(theme.critical),
        ))
        .wrap(Wrap { trim: false });
        frame.render_widget(p, body_area);
        render_footer(frame, chunks[1], theme);
        return;
    }

    let loading = state.storage.queues_pending.contains(&account.id);
    match listing {
        None if loading => {
            let p = Paragraph::new(Line::from(Span::styled(
                "loading queues …",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        None => {
            let p = Paragraph::new(Line::from(Span::styled(
                "press r to load queues.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(l) if l.queues.is_empty() => {
            let p = Paragraph::new(Line::from(Span::styled(
                "no queues on this account.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(_) if filtered.is_empty() => {
            let p = Paragraph::new(Line::from(Span::styled(
                "no queues match the current filter.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(l) => {
            // Counts failed: say why above the table (2 rows so the role hint
            // isn't cut off), keep the names usable below it.
            let table_area = match l.count_error.as_deref() {
                Some(err) if body_area.height > 3 => {
                    let parts = Layout::vertical([Constraint::Length(2), Constraint::Min(0)])
                        .split(body_area);
                    let p = Paragraph::new(Text::styled(
                        format!("message counts unavailable: {err}"),
                        Style::default().fg(theme.degraded),
                    ))
                    .wrap(Wrap { trim: false });
                    frame.render_widget(p, parts[0]);
                    parts[1]
                }
                _ => body_area,
            };
            render_table(frame, table_area, &filtered, state, theme);
        }
    }

    render_footer(frame, chunks[1], theme);
}

fn render_table(
    frame: &mut Frame,
    area: Rect,
    queues: &[&StorageQueue],
    state: &AppState,
    theme: &Theme,
) {
    // Two columns only: size NAME to the longest name (Azure caps queue names
    // at 63 chars) and let a trailing spacer take the slack, so the counts sit
    // next to the names instead of at the far edge of a wide terminal.
    let name_w = queues
        .iter()
        .map(|q| q.name.chars().count())
        .max()
        .unwrap_or(0)
        .max(20) as u16;
    let widths = [
        Constraint::Length(name_w), // NAME
        Constraint::Length(10),     // MESSAGES ("12.3k", "—")
        Constraint::Fill(1),        // spacer
    ];
    let header_row = Row::new(vec![
        Cell::from("NAME"),
        Cell::from(Line::from("MESSAGES").alignment(Alignment::Right)),
    ])
    .style(
        Style::default()
            .fg(theme.muted)
            .add_modifier(Modifier::BOLD),
    );

    let body_rows: Vec<Row> = queues
        .iter()
        .map(|q| {
            let (name_color, count_color) = row_colors(q, theme);
            let count = q
                .approximate_message_count
                .map(|n| format_count(n as f64))
                .unwrap_or_else(|| "—".to_string());
            Row::new(vec![
                Cell::from(q.name.as_str()).style(Style::default().fg(name_color)),
                Cell::from(Line::from(count).alignment(Alignment::Right))
                    .style(Style::default().fg(count_color)),
            ])
        })
        .collect();

    let cursor = state.storage.queues_cursor.min(queues.len() - 1);
    let table = Table::new(body_rows, widths)
        .header(header_row)
        .row_highlight_style(theme.selection())
        .highlight_symbol("▍ ")
        .column_spacing(2);
    let mut ts = TableState::default().with_offset(state.storage.queues_view_top.get());
    ts.select(Some(cursor));
    frame.render_stateful_widget(table, area, &mut ts);
    state.storage.queues_view_top.set(ts.offset());
}

/// `(name, count)` colours for one row: a poison queue holding messages is
/// the thing to notice (critical); empty queues fade to muted; an unknown
/// count stays muted too.
fn row_colors(q: &StorageQueue, theme: &Theme) -> (ratatui::style::Color, ratatui::style::Color) {
    match q.approximate_message_count {
        Some(n) if n > 0 && q.is_poison() => (theme.critical, theme.critical),
        Some(n) if n > 0 => (theme.fg, theme.fg),
        _ => (theme.fg, theme.muted),
    }
}

fn render_footer(frame: &mut Frame, area: Rect, theme: &Theme) {
    let p = Paragraph::new(Line::from(Span::styled(
        FOOTER_HINT,
        Style::default().fg(theme.muted),
    )));
    frame.render_widget(p, area);
}

pub fn handle(action: Action, state: &mut AppState) -> bool {
    let Some(account_id) = state
        .storage
        .selected_account
        .as_ref()
        .map(|a| a.id.clone())
    else {
        return false;
    };

    // Navigation operates on the filtered slice so the cursor never points
    // past the end of what's rendered.
    let len = state.storage.filtered_queues(&account_id).len();

    // Filter input focused: same contract as the containers view — Esc
    // cancels and clears, Enter commits, Down hands focus back to the list.
    // Esc clears the filter whether the box is still focused or it was
    // applied then defocused (Enter / Down); only a clear list lets Esc
    // fall through to navigation. Mirrors the resource list.
    if matches!(action, Action::Back)
        && (state.storage.queues_filter_active || !state.storage.queues_filter.value().is_empty())
    {
        state.storage.queues_filter_active = false;
        state.storage.queues_filter.reset();
        state.storage.queues_cursor = 0;
        return true;
    }

    if state.storage.queues_filter_active {
        match action {
            Action::OpenSelected => {
                state.storage.queues_filter_active = false;
                return true;
            }
            Action::MoveDown => {
                state.storage.queues_filter_active = false;
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
                state.storage.queues_cursor = (state.storage.queues_cursor + 1).min(len - 1);
            }
            true
        }
        Action::MoveUp => {
            state.storage.queues_cursor = state.storage.queues_cursor.saturating_sub(1);
            true
        }
        Action::HalfPageDown => {
            if len > 0 {
                state.storage.queues_cursor =
                    (state.storage.queues_cursor + HALF_PAGE).min(len - 1);
            }
            true
        }
        Action::HalfPageUp => {
            state.storage.queues_cursor = state.storage.queues_cursor.saturating_sub(HALF_PAGE);
            true
        }
        Action::GotoTop => {
            state.storage.queues_cursor = 0;
            true
        }
        Action::GotoBottom => {
            if len > 0 {
                state.storage.queues_cursor = len - 1;
            }
            true
        }
        Action::StartSearch => {
            state.storage.queues_filter.reset();
            state.storage.queues_cursor = 0;
            state.storage.queues_filter_active = true;
            true
        }
        Action::OpenSelected => {
            if let Some(name) = selected_queue_name(state, &account_id) {
                state.storage.selected_queue = Some(name);
                state.storage.messages_scroll = 0;
                state.view = View::StorageQueueMessages;
            }
            true
        }
        _ => false,
    }
}

/// Name of the queue under the cursor, resolved through the filtered slice
/// so it matches what's on screen.
fn selected_queue_name(state: &AppState, account_id: &str) -> Option<String> {
    state
        .storage
        .filtered_queues(account_id)
        .get(state.storage.queues_cursor)
        .map(|q| q.name.clone())
}

/// `y` payload: `{account}/{queue}` for the row under the cursor.
pub fn yank_text(state: &AppState) -> Option<String> {
    let acc = state.storage.selected_account.as_ref()?;
    let name = selected_queue_name(state, &acc.id)?;
    Some(format!("{}/{}", acc.name, name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::storage::{QueueListing, StorageAccount};
    use crate::config::Config;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    const ACCOUNT_ID: &str = "/subs/x/rg/y/sa/acct1";

    fn account_fixture() -> StorageAccount {
        StorageAccount {
            id: ACCOUNT_ID.into(),
            name: "acct1".into(),
            resource_group: "rg".into(),
            subscription_id: "sub".into(),
            location: "westeurope".into(),
            kind: Some("StorageV2".into()),
            sku: None,
            access_tier: None,
            is_hns_enabled: None,
            https_only: None,
            allow_blob_public_access: None,
            created_at: None,
        }
    }

    fn queue(name: &str, n: Option<u64>) -> StorageQueue {
        StorageQueue {
            name: name.into(),
            approximate_message_count: n,
        }
    }

    fn fixture(queues: Vec<StorageQueue>, count_error: Option<&str>) -> AppState {
        let mut state = AppState::new(Config::default());
        state.view = View::StorageQueues;
        state.storage.selected_account = Some(account_fixture());
        state.storage.queues.insert(
            ACCOUNT_ID.into(),
            QueueListing {
                queues,
                count_error: count_error.map(str::to_string),
            },
        );
        state
    }

    fn draw(state: &AppState) -> String {
        let theme = Theme::catppuccin_mocha();
        let mut term = Terminal::new(TestBackend::new(100, 12)).unwrap();
        term.draw(|f| render(f, f.area(), state, &theme)).unwrap();
        format!("{:?}", term.backend().buffer())
    }

    #[test]
    fn renders_queues_with_counts() {
        let state = fixture(
            vec![
                queue("orders", Some(12_300)),
                queue("orders-poison", Some(2)),
            ],
            None,
        );
        let buf = draw(&state);
        assert!(buf.contains("MESSAGES"), "got: {buf}");
        assert!(buf.contains("orders-poison"), "got: {buf}");
        assert!(buf.contains("12.3k"), "got: {buf}");
        assert!(buf.contains("queues · 2"), "got: {buf}");
    }

    #[test]
    fn missing_counts_render_dash_and_reason() {
        let state = fixture(
            vec![queue("orders", None)],
            Some("403 … lacks 'Storage Queue Data Reader'"),
        );
        let buf = draw(&state);
        assert!(buf.contains("message counts unavailable"), "got: {buf}");
        assert!(buf.contains("Storage Queue Data Reader"), "got: {buf}");
        assert!(buf.contains("—"), "got: {buf}");
        assert!(buf.contains("orders"), "names still list, got: {buf}");
    }

    #[test]
    fn renders_empty_and_loading_states() {
        let state = fixture(Vec::new(), None);
        assert!(draw(&state).contains("no queues on this account"));

        let mut state = fixture(Vec::new(), None);
        state.storage.queues.clear();
        state.storage.queues_pending.insert(ACCOUNT_ID.into());
        assert!(draw(&state).contains("loading queues"));
    }

    #[test]
    fn poison_queue_with_messages_is_critical() {
        let theme = Theme::catppuccin_mocha();
        assert_eq!(
            row_colors(&queue("orders-poison", Some(3)), &theme).1,
            theme.critical
        );
        assert_eq!(
            row_colors(&queue("orders-poison", Some(0)), &theme).1,
            theme.muted
        );
        assert_eq!(row_colors(&queue("orders", Some(3)), &theme).1, theme.fg);
    }

    #[test]
    fn enter_pins_filtered_queue_and_opens_messages() {
        let mut state = fixture(vec![queue("alpha", Some(1)), queue("beta", Some(0))], None);
        state.storage.queues_filter = tui_input::Input::default().with_value("BET".to_string());
        state.storage.messages_scroll = 7;
        assert!(handle(Action::OpenSelected, &mut state));
        assert_eq!(state.view, View::StorageQueueMessages);
        assert_eq!(state.storage.selected_queue.as_deref(), Some("beta"));
        assert_eq!(state.storage.messages_scroll, 0);
    }

    #[test]
    fn navigation_clamps_to_filtered_length() {
        let mut state = fixture(
            vec![queue("a-1", None), queue("b", None), queue("a-2", None)],
            None,
        );
        state.storage.queues_filter = tui_input::Input::default().with_value("a-".to_string());
        assert!(handle(Action::GotoBottom, &mut state));
        assert_eq!(state.storage.queues_cursor, 1);
        assert!(handle(Action::MoveDown, &mut state));
        assert_eq!(state.storage.queues_cursor, 1);
        assert_eq!(yank_text(&state).as_deref(), Some("acct1/a-2"));
    }

    #[test]
    fn esc_while_filtering_clears_filter() {
        let mut state = fixture(vec![queue("orders", None)], None);
        assert!(handle(Action::StartSearch, &mut state));
        assert!(state.storage.queues_filter_active);
        state.storage.queues_filter = tui_input::Input::default().with_value("or".to_string());
        assert!(handle(Action::Back, &mut state));
        assert!(!state.storage.queues_filter_active);
        assert_eq!(state.storage.queues_filter.value(), "");
        assert_eq!(state.view, View::StorageQueues);
    }

    #[test]
    fn esc_clears_applied_filter_after_defocus() {
        // `/or`, Enter to browse the narrowed list, then Esc clears the filter
        // instead of leaving the view.
        let mut state = fixture(vec![queue("orders", None)], None);
        state.storage.queues_filter = tui_input::Input::default().with_value("or".to_string());
        assert!(handle(Action::Back, &mut state));
        assert_eq!(state.storage.queues_filter.value(), "");
        assert_eq!(state.view, View::StorageQueues);
        // With the filter gone, Esc falls through to breadcrumb navigation.
        assert!(!handle(Action::Back, &mut state));
    }
}
