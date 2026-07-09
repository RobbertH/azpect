//! Key Vault access-log drill-in: who touched the pinned vault, when, from
//! where — the `AuditEvent` rows the vault's diagnostic setting forwards to
//! Log Analytics. Opened with `l` on a vault (vault-wide) or on a specific
//! secret / certificate row (pre-scoped to that item).
//!
//! Filters:
//! - `0` / `1` / `7` pick the 1h / 1d / 7d window; `t` takes a free-form
//!   window like `6m` or `1y` (audit questions routinely reach months back).
//! - `m` excludes *you* server-side — your UPN and sign-in IP, decoded from
//!   the bearer token's claims — so your own browsing doesn't drown the trail.
//! - Tab / Shift-Tab cycle a client-side `OperationName` filter (`SecretGet`,
//!   `SecretList`, …) through the operations present in the fetched page.
//!
//! Window, exclude-me, and item scope are *query* parameters: changing them
//! drops the buffer and refetches (guarded by `access_generation` so a stale
//! in-flight page can't land). The operation filter narrows the cached page.

#![allow(dead_code, unused_variables)]

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;

use crate::azure::key_vault_logs::{AccessEvent, AccessWindow, CallerKind};
use crate::ui::events::Action;
use crate::ui::state::{AppState, View};
use crate::ui::theme::Theme;

const FOOTER_HINT: &str = "j/k move  0 1h  1 1d  7 7d  t custom window  m hide me  Tab operation  y yank  r refresh  Esc back  ? help";
const HALF_PAGE: usize = 10;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);

    let visible = state.key_vault.visible_access_events();

    let mut title_spans: Vec<Span> =
        vec![Span::styled(" access log ", Style::default().fg(theme.fg))];
    if let Some(total) = state.key_vault.access_events.as_ref().map(|e| e.len()) {
        let shown = visible.len();
        let count = if shown != total {
            format!("· {shown} of {total} rows ")
        } else {
            format!("· {total} rows ")
        };
        title_spans.push(Span::styled(count, Style::default().fg(theme.muted)));
    }
    title_spans.push(Span::styled(
        format!("· {} ", state.key_vault.access_window.label()),
        Style::default().fg(theme.fg),
    ));
    if state.key_vault.access_truncated {
        title_spans.push(Span::styled(
            "· newest rows only (window has more) ",
            Style::default().fg(theme.degraded),
        ));
    }
    if let Some(scope) = state.key_vault.access_scope.as_ref() {
        title_spans.push(Span::styled(
            format!("· {} ✓ ", scope.path()),
            Style::default().fg(theme.accent),
        ));
    }
    if let Some(op) = state.key_vault.access_operation.as_deref() {
        title_spans.push(Span::styled(
            format!("· op: {op} "),
            Style::default().fg(theme.accent),
        ));
    }
    if state.key_vault.access_exclude_self {
        let who = state
            .key_vault
            .access_hidden
            .as_ref()
            .map(|h| h.label())
            .unwrap_or_else(|| "you".to_string());
        title_spans.push(Span::styled(
            format!("· hiding {who} ✓ "),
            Style::default().fg(theme.accent),
        ));
    }
    if state.key_vault.access_pending && state.key_vault.access_events.is_some() {
        title_spans.push(Span::styled(
            "· refreshing… ",
            Style::default().fg(theme.muted),
        ));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Line::from(title_spans));
    let inner = block.inner(chunks[0]);
    frame.render_widget(block, chunks[0]);

    // Custom-window entry row, shown only while `t` has focus.
    let (input_area, body_area) = if state.key_vault.access_window_input_active {
        let parts = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(inner);
        (Some(parts[0]), parts[1])
    } else {
        (None, inner)
    };
    if let Some(ia) = input_area {
        let p = Paragraph::new(Line::from(vec![
            Span::styled("window> ", Style::default().fg(theme.accent)),
            Span::styled(
                state.key_vault.access_window_input.value(),
                Style::default().fg(theme.fg),
            ),
            Span::styled("█", Style::default().fg(theme.accent)),
            Span::styled(
                "  (e.g. 12h, 30d, 6m, 1y — Enter applies, Esc cancels)",
                Style::default().fg(theme.muted),
            ),
        ]));
        frame.render_widget(p, ia);
    }

    if let Some(err) = state.key_vault.access_error.as_ref() {
        let p = Paragraph::new(Text::styled(
            format!("error: {err}"),
            Style::default().fg(theme.critical),
        ))
        .wrap(Wrap { trim: false });
        frame.render_widget(p, body_area);
        render_footer(frame, chunks[1], theme);
        return;
    }

    match state.key_vault.access_events.as_ref() {
        None if state.key_vault.access_pending => {
            let p = Paragraph::new(Line::from(Span::styled(
                "loading access log…",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        None => {
            let p = Paragraph::new(Line::from(Span::styled(
                "press r to load the access log.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(rows) if rows.is_empty() => {
            // Zero rows is ambiguous: nothing happened, or nothing is being
            // recorded. Say so — a vault without a diagnostic setting will
            // sit empty forever and that's worth knowing.
            let p = Paragraph::new(Text::styled(
                "no audit events in this window.\n\nif this is unexpected, check the vault's diagnostic settings — AuditEvent \nmust be forwarded to a Log Analytics workspace for rows to appear here.",
                Style::default().fg(theme.muted),
            ));
            frame.render_widget(p, body_area);
        }
        Some(_) if visible.is_empty() => {
            let p = Paragraph::new(Line::from(Span::styled(
                "no rows match the current operation filter.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(_) => {
            let caller_w = visible
                .iter()
                .map(|e| e.caller.chars().count() as u16)
                .max()
                .unwrap_or(0)
                .clamp(6, 40);
            let op_w = visible
                .iter()
                .map(|e| e.operation.chars().count() as u16)
                .max()
                .unwrap_or(0)
                .max(9);
            let widths = [
                Constraint::Length(19),       // WHEN (YYYY-MM-DD HH:MM:SS)
                Constraint::Length(op_w),     // OPERATION
                Constraint::Length(caller_w), // CALLER
                Constraint::Length(15),       // IP
                Constraint::Min(12),          // OBJECT
                Constraint::Length(9),        // RESULT
            ];
            let header_row = Row::new(vec![
                "WHEN",
                "OPERATION",
                "CALLER",
                "IP",
                "OBJECT",
                "RESULT",
            ])
            .style(
                Style::default()
                    .fg(theme.muted)
                    .add_modifier(Modifier::BOLD),
            );
            let cursor = state.key_vault.access_cursor.min(visible.len() - 1);
            let body_rows: Vec<Row> = visible.iter().map(|e| build_row(e, theme)).collect();
            let table = Table::new(body_rows, widths)
                .header(header_row)
                .row_highlight_style(theme.selection())
                .highlight_symbol("▍ ")
                .column_spacing(2);
            let mut ts = TableState::default().with_offset(state.key_vault.access_view_top.get());
            ts.select(Some(cursor));
            frame.render_stateful_widget(table, body_area, &mut ts);
            state.key_vault.access_view_top.set(ts.offset());
        }
    }

    render_footer(frame, chunks[1], theme);
}

fn build_row<'a>(event: &'a AccessEvent, theme: &Theme) -> Row<'a> {
    let when = event
        .ts
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    let caller_style = match event.caller_kind {
        CallerKind::User => Style::default().fg(theme.accent),
        CallerKind::ManagedIdentity => Style::default().fg(theme.healthy),
        CallerKind::App => Style::default().fg(theme.degraded),
        CallerKind::Unknown => Style::default().fg(theme.muted),
    };
    // Non-OK results are the rows an auditor is hunting for — flag them red.
    let ok = event.result == "OK" || event.result.starts_with('2');
    let result_style = if ok {
        Style::default().fg(theme.muted)
    } else {
        Style::default().fg(theme.critical)
    };
    Row::new(vec![
        Cell::from(when).style(Style::default().fg(theme.muted)),
        Cell::from(event.operation.as_str()).style(Style::default().fg(theme.fg)),
        Cell::from(event.caller.as_str()).style(caller_style),
        Cell::from(event.ip.as_str()).style(Style::default().fg(theme.muted)),
        Cell::from(event.object.as_deref().unwrap_or("—")).style(Style::default().fg(theme.fg)),
        Cell::from(event.result.as_str()).style(result_style),
    ])
}

fn render_footer(frame: &mut Frame, area: Rect, theme: &Theme) {
    let p = Paragraph::new(Line::from(Span::styled(
        FOOTER_HINT,
        Style::default().fg(theme.muted),
    )));
    frame.render_widget(p, area);
}

/// Drop the fetched page and bump the generation — the query scope changed
/// (window / exclude-me), so the buffer no longer describes what the header
/// claims and any in-flight fetch is stale. The caller's `after_action` hook
/// spawns the refetch.
fn invalidate_fetch(state: &mut AppState) {
    state.key_vault.access_events = None;
    state.key_vault.access_error = None;
    state.key_vault.access_hidden = None;
    state.key_vault.access_truncated = false;
    state.key_vault.access_cursor = 0;
    state.key_vault.access_view_top.set(0);
    state.key_vault.access_operation = None;
    state.key_vault.access_generation = state.key_vault.access_generation.wrapping_add(1);
}

fn set_window(state: &mut AppState, window: AccessWindow) -> bool {
    if state.key_vault.access_window == window {
        return true;
    }
    state.key_vault.access_window = window;
    invalidate_fetch(state);
    true
}

/// Cycle the client-side operation filter: all → op₁ → op₂ → … → all, in the
/// sorted order of operations present in the fetched page.
fn cycle_operation(state: &mut AppState, direction: i32) {
    let ops = state.key_vault.access_operations();
    if ops.is_empty() {
        return;
    }
    let all = ops.len() as i32;
    let next = match state.key_vault.access_operation.as_deref() {
        None => Some((all + direction).rem_euclid(all + 1)),
        Some(current) => ops
            .iter()
            .position(|o| o == current)
            .map(|i| (i as i32 + direction).rem_euclid(all + 1)),
    };
    state.key_vault.access_operation = match next {
        Some(n) if n != all => Some(ops[n as usize].clone()),
        _ => None,
    };
    state.key_vault.access_cursor = 0;
    state.key_vault.access_view_top.set(0);
}

/// Yank text for the selected row: everything the table shows plus the full
/// managed-identity ARM id (the table only shows its trailing name).
pub fn yank_text(state: &AppState) -> Option<String> {
    let visible = state.key_vault.visible_access_events();
    let event = visible.get(
        state
            .key_vault
            .access_cursor
            .min(visible.len().checked_sub(1)?),
    )?;
    let mut parts = vec![
        event.ts.to_rfc3339(),
        event.operation.clone(),
        event.caller.clone(),
        event.ip.clone(),
    ];
    if let Some(obj) = &event.object {
        parts.push(obj.clone());
    }
    parts.push(event.result.clone());
    if let Some(mirid) = &event.mirid {
        parts.push(mirid.clone());
    }
    Some(parts.join("  "))
}

pub fn handle(action: Action, state: &mut AppState) -> bool {
    // Custom-window input focus: raw keys flow into the input via app.rs;
    // only Enter (apply) and Esc (cancel) land here as actions.
    if state.key_vault.access_window_input_active {
        match action {
            Action::Back => {
                state.key_vault.access_window_input_active = false;
                state.key_vault.access_window_input.reset();
                return true;
            }
            Action::OpenSelected => {
                let raw = state.key_vault.access_window_input.value().to_string();
                match AccessWindow::parse(&raw) {
                    Some(window) => {
                        state.key_vault.access_window_input_active = false;
                        state.key_vault.access_window_input.reset();
                        set_window(state, window);
                    }
                    None => {
                        state
                            .set_status(format!("can't parse \"{raw}\" — try 12h, 30d, 6m, or 1y"));
                    }
                }
                return true;
            }
            _ => return false,
        }
    }

    let len = state.key_vault.visible_access_events().len();
    match action {
        Action::MoveDown => {
            if len > 0 {
                state.key_vault.access_cursor = (state.key_vault.access_cursor + 1).min(len - 1);
            }
            true
        }
        Action::MoveUp => {
            state.key_vault.access_cursor = state.key_vault.access_cursor.saturating_sub(1);
            true
        }
        Action::HalfPageDown => {
            if len > 0 {
                state.key_vault.access_cursor =
                    (state.key_vault.access_cursor + HALF_PAGE).min(len - 1);
            }
            true
        }
        Action::HalfPageUp => {
            state.key_vault.access_cursor = state.key_vault.access_cursor.saturating_sub(HALF_PAGE);
            true
        }
        Action::GotoTop => {
            state.key_vault.access_cursor = 0;
            true
        }
        Action::GotoBottom => {
            state.key_vault.access_cursor = len.saturating_sub(1);
            true
        }
        Action::SetWindowHour => set_window(state, AccessWindow::Hour),
        Action::SetWindowDay => set_window(state, AccessWindow::Day),
        Action::SetWindowWeek => set_window(state, AccessWindow::Week),
        Action::SetCustomWindow => {
            state.key_vault.access_window_input.reset();
            state.key_vault.access_window_input_active = true;
            true
        }
        Action::ToggleExcludeSelf => {
            state.key_vault.access_exclude_self = !state.key_vault.access_exclude_self;
            invalidate_fetch(state);
            true
        }
        Action::CycleSourceFilter => {
            cycle_operation(state, 1);
            true
        }
        Action::CycleSourceFilterBack => {
            cycle_operation(state, -1);
            true
        }
        Action::Back => {
            // Return to wherever `l` was pressed: the items list when scoped
            // to one secret, the vaults list otherwise.
            state.view = state
                .key_vault
                .access_return_view
                .take()
                .unwrap_or(View::KeyVaults);
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::key_vault_logs::ItemScope;
    use crate::config::Config;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn event(min_ago: i64, op: &str, caller: &str, ip: &str, object: Option<&str>) -> AccessEvent {
        AccessEvent {
            ts: chrono::Utc::now() - chrono::Duration::minutes(min_ago),
            operation: op.to_string(),
            caller: caller.to_string(),
            caller_kind: CallerKind::User,
            ip: ip.to_string(),
            object: object.map(str::to_owned),
            result: "OK".to_string(),
            mirid: None,
        }
    }

    fn fixture() -> AppState {
        let mut state = AppState::new(Config::default());
        state.view = View::KeyVaultAccessLogs;
        state.key_vault.access_events = Some(vec![
            event(
                1,
                "SecretGet",
                "dana@contoso.com",
                "198.51.100.23",
                Some("secrets/a"),
            ),
            event(2, "SecretList", "robbert@contoso.com", "203.0.113.7", None),
            event(
                3,
                "SecretGet",
                "ca-checkout-api",
                "10.0.1.12",
                Some("secrets/b"),
            ),
        ]);
        state
    }

    #[test]
    fn window_keys_change_window_and_drop_buffer() {
        let mut state = fixture();
        let gen0 = state.key_vault.access_generation;
        assert!(handle(Action::SetWindowWeek, &mut state));
        assert_eq!(state.key_vault.access_window, AccessWindow::Week);
        assert!(state.key_vault.access_events.is_none());
        assert_eq!(state.key_vault.access_generation, gen0 + 1);
        // Same window again: no invalidation.
        let events = vec![event(1, "SecretGet", "x", "1.2.3.4", None)];
        state.key_vault.access_events = Some(events);
        assert!(handle(Action::SetWindowWeek, &mut state));
        assert!(state.key_vault.access_events.is_some());
    }

    #[test]
    fn custom_window_input_parses_and_applies() {
        let mut state = fixture();
        assert!(handle(Action::SetCustomWindow, &mut state));
        assert!(state.key_vault.access_window_input_active);
        state.key_vault.access_window_input = "6m".into();
        assert!(handle(Action::OpenSelected, &mut state));
        assert!(!state.key_vault.access_window_input_active);
        assert_eq!(state.key_vault.access_window.label(), "6m");
        assert_eq!(state.key_vault.access_window.duration().num_days(), 180);
        assert!(
            state.key_vault.access_events.is_none(),
            "scope changed → refetch"
        );
    }

    #[test]
    fn custom_window_input_rejects_junk_and_stays_focused() {
        let mut state = fixture();
        handle(Action::SetCustomWindow, &mut state);
        state.key_vault.access_window_input = "sixmonths".into();
        assert!(handle(Action::OpenSelected, &mut state));
        assert!(
            state.key_vault.access_window_input_active,
            "stays open to retry"
        );
        assert!(state.status_message.is_some());
        // Esc cancels without touching the window.
        assert!(handle(Action::Back, &mut state));
        assert!(!state.key_vault.access_window_input_active);
        assert_eq!(state.key_vault.access_window, AccessWindow::default());
    }

    #[test]
    fn exclude_self_toggle_invalidates_fetch() {
        let mut state = fixture();
        let gen0 = state.key_vault.access_generation;
        assert!(handle(Action::ToggleExcludeSelf, &mut state));
        assert!(state.key_vault.access_exclude_self);
        assert!(state.key_vault.access_events.is_none());
        assert_eq!(state.key_vault.access_generation, gen0 + 1);
    }

    #[test]
    fn tab_cycles_operation_filter_through_sorted_ops() {
        let mut state = fixture();
        // all → SecretGet → SecretList → all
        assert!(handle(Action::CycleSourceFilter, &mut state));
        assert_eq!(
            state.key_vault.access_operation.as_deref(),
            Some("SecretGet")
        );
        assert_eq!(state.key_vault.visible_access_events().len(), 2);
        assert!(handle(Action::CycleSourceFilter, &mut state));
        assert_eq!(
            state.key_vault.access_operation.as_deref(),
            Some("SecretList")
        );
        assert_eq!(state.key_vault.visible_access_events().len(), 1);
        assert!(handle(Action::CycleSourceFilter, &mut state));
        assert_eq!(state.key_vault.access_operation, None);
        assert_eq!(state.key_vault.visible_access_events().len(), 3);
    }

    #[test]
    fn back_returns_to_recorded_origin() {
        let mut state = fixture();
        state.key_vault.access_return_view = Some(View::KeyVaultItems);
        assert!(handle(Action::Back, &mut state));
        assert_eq!(state.view, View::KeyVaultItems);
        // Without a recorded origin, fall back to the vaults list.
        state.view = View::KeyVaultAccessLogs;
        assert!(handle(Action::Back, &mut state));
        assert_eq!(state.view, View::KeyVaults);
    }

    #[test]
    fn yank_includes_full_mirid() {
        let mut state = fixture();
        state.key_vault.access_events.as_mut().unwrap()[0].mirid =
            Some("/subscriptions/s/resourceGroups/rg/providers/x/y/ca-app".to_string());
        state.key_vault.access_cursor = 0;
        let y = yank_text(&state).unwrap();
        assert!(y.contains("SecretGet"));
        assert!(y.contains("/subscriptions/s/resourceGroups/rg/providers/x/y/ca-app"));
    }

    #[test]
    fn renders_table_scope_and_hidden_chips() {
        let theme = crate::ui::theme::Theme::catppuccin_mocha();
        let backend = TestBackend::new(170, 20);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.key_vault.access_scope = Some(ItemScope {
            kind_segment: "secrets".into(),
            name: "orders-db-connection".into(),
        });
        state.key_vault.access_exclude_self = true;
        state.key_vault.access_hidden = Some(crate::azure::demo::self_identity());
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("dana@contoso.com"), "caller column missing");
        assert!(
            s.contains("secrets/orders-db-connection"),
            "scope chip missing"
        );
        assert!(
            s.contains("hiding robbert@contoso.com"),
            "hidden chip missing"
        );
        assert!(s.contains("WHEN"), "header missing");
    }

    #[test]
    fn renders_empty_state_with_diagnostics_hint() {
        let theme = crate::ui::theme::Theme::catppuccin_mocha();
        let backend = TestBackend::new(120, 14);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.key_vault.access_events = Some(Vec::new());
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("diagnostic settings"));
    }
}
