//! Storage access-log drill-in: who read / wrote which blob, when, from
//! where — the `StorageBlobLogs` rows the account's blob-service diagnostic
//! setting forwards to Log Analytics. Opened with `l` on a storage account
//! row (account-wide) or on a container row (pre-scoped to that container).
//! Mirrors the Key Vault / ACR access views' controls:
//!
//! - `0` / `1` / `7` pick the 1h / 1d / 7d window; `t` takes a free-form
//!   window like `6m` or `1y`.
//! - `m` excludes *you* server-side — your UPN, object id, and sign-in IP,
//!   decoded from the bearer token's claims.
//! - Tab / Shift-Tab cycle a client-side `OperationName` filter (`GetBlob`,
//!   `PutBlob`, …) through the operations present in the fetched page.
//!
//! Window, exclude-me, and container scope are *query* parameters: changing
//! them drops the buffer and refetches (guarded by `access_generation`).
//! GUID-shaped identities render under their Graph-resolved display name
//! once `state.principals` has it (same best-effort flow as SQL audit).

#![allow(dead_code, unused_variables)]

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;

use crate::azure::key_vault_logs::AccessWindow;
use crate::azure::storage_logs::{AccessEvent, CallerKind};
use crate::ui::events::Action;
use crate::ui::state::{AppState, View};
use crate::ui::theme::Theme;

use super::sql_audit::display_principal;

const HALF_PAGE: usize = 10;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);

    let visible = state.storage.visible_access_events();

    let mut title_spans: Vec<Span> =
        vec![Span::styled(" access log ", Style::default().fg(theme.fg))];
    if let Some(total) = state.storage.access_events.as_ref().map(|e| e.len()) {
        let shown = visible.len();
        let count = if shown != total {
            format!("· {shown} of {total} rows ")
        } else {
            format!("· {total} rows ")
        };
        title_spans.push(Span::styled(count, Style::default().fg(theme.muted)));
    }
    title_spans.push(Span::styled(
        format!("· {} ", state.storage.access_window.label()),
        Style::default().fg(theme.fg),
    ));
    if state.storage.access_truncated {
        title_spans.push(Span::styled(
            "· newest rows only (window has more) ",
            Style::default().fg(theme.degraded),
        ));
    }
    if let Some(container) = state.storage.access_scope.as_deref() {
        title_spans.push(Span::styled(
            format!("· {container} ✓ "),
            Style::default().fg(theme.accent),
        ));
    }
    if let Some(op) = state.storage.access_operation.as_deref() {
        title_spans.push(Span::styled(
            format!("· op: {op} "),
            Style::default().fg(theme.accent),
        ));
    }
    if state.storage.access_exclude_self {
        let who = state
            .storage
            .access_hidden
            .as_ref()
            .map(|h| h.label())
            .unwrap_or_else(|| "you".to_string());
        title_spans.push(Span::styled(
            format!("· hiding {who} ✓ "),
            Style::default().fg(theme.accent),
        ));
    }
    if state.storage.access_pending && state.storage.access_events.is_some() {
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
    let (input_area, body_area) = if state.storage.access_window_input_active {
        let parts = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(inner);
        (Some(parts[0]), parts[1])
    } else {
        (None, inner)
    };
    if let Some(ia) = input_area {
        let p = Paragraph::new(Line::from(vec![
            Span::styled("window> ", Style::default().fg(theme.accent)),
            Span::styled(
                state.storage.access_window_input.value(),
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

    if let Some(err) = state.storage.access_error.as_ref() {
        let mut lines = vec![Line::from(Span::styled(
            format!("error: {err}"),
            Style::default().fg(theme.critical),
        ))];
        // A workspace that never received blob logs fails KQL table
        // resolution — same root cause as the empty page, so same hint.
        if err.contains("Failed to resolve table") || err.contains("SEM0100") {
            lines.push(Line::default());
            lines.extend(diagnostics_warning_lines(theme));
        }
        let p = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
        frame.render_widget(p, body_area);
        render_footer(frame, chunks[1], state, theme);
        return;
    }

    match state.storage.access_events.as_ref() {
        None if state.storage.access_pending => {
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
            // recorded. Forwarding blob logs is opt-in and most accounts
            // never got the diagnostic setting — warn loudly.
            let mut lines = vec![
                Line::from(Span::styled(
                    "no blob-access events in this window.",
                    Style::default().fg(theme.muted),
                )),
                Line::default(),
            ];
            lines.extend(diagnostics_warning_lines(theme));
            let p = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
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
                .map(|e| caller_display(state, e).chars().count() as u16)
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
                Constraint::Length(8),        // AUTH
                Constraint::Length(15),       // IP
                Constraint::Min(12),          // OBJECT
                Constraint::Length(12),       // RESULT
            ];
            let header_row = Row::new(vec![
                "WHEN",
                "OPERATION",
                "CALLER",
                "AUTH",
                "IP",
                "OBJECT",
                "RESULT",
            ])
            .style(
                Style::default()
                    .fg(theme.muted)
                    .add_modifier(Modifier::BOLD),
            );
            let cursor = state.storage.access_cursor.min(visible.len() - 1);
            let body_rows: Vec<Row> = visible.iter().map(|e| build_row(state, e, theme)).collect();
            let table = Table::new(body_rows, widths)
                .header(header_row)
                .row_highlight_style(theme.selection())
                .highlight_symbol("▍ ")
                .column_spacing(2);
            let mut ts = TableState::default().with_offset(state.storage.access_view_top.get());
            ts.select(Some(cursor));
            frame.render_stateful_widget(table, body_area, &mut ts);
            state.storage.access_view_top.set(ts.offset());
        }
    }

    render_footer(frame, chunks[1], state, theme);
}

/// The "your account probably isn't logging anything" warning, shared by the
/// empty page and the no-such-table error. Same rationale as the ACR view's:
/// blob-access logging is OFF by default, so an empty page is almost always a
/// configuration gap — not quiet.
fn diagnostics_warning_lines(theme: &Theme) -> Vec<Line<'static>> {
    let warn = Style::default().fg(theme.client_error);
    vec![
        Line::from(Span::styled(
            "⚠ most likely cause: this account is not forwarding its blob access logs anywhere.",
            warn.add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "Storage does NOT record access logs by default. A diagnostic setting on the account's",
            warn,
        )),
        Line::from(Span::styled(
            "blob service must explicitly send StorageRead / StorageWrite / StorageDelete to a",
            warn,
        )),
        Line::from(Span::styled(
            "Log Analytics workspace, and only events from after that point are captured",
            warn,
        )),
        Line::from(Span::styled(
            "(Portal: storage account → Monitoring → Diagnostic settings → blob).",
            warn,
        )),
    ]
}

/// Caller column text: the identity for OAuth rows (Graph-resolved when it's
/// a GUID we've looked up), a fixed label for the identity-less shapes.
fn caller_display(state: &AppState, event: &AccessEvent) -> String {
    match event.caller_kind {
        CallerKind::User | CallerKind::App => event.identity.clone(),
        CallerKind::Principal => display_principal(state, &event.identity),
        CallerKind::Sas => "SAS".to_string(),
        CallerKind::AccountKey => "account key".to_string(),
        CallerKind::Anonymous => "anonymous".to_string(),
        CallerKind::Unknown => {
            if event.auth_type.is_empty() {
                "unknown".to_string()
            } else {
                event.auth_type.clone()
            }
        }
    }
}

fn build_row<'a>(state: &AppState, event: &'a AccessEvent, theme: &Theme) -> Row<'a> {
    let when = event
        .ts
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    let caller_style = match event.caller_kind {
        CallerKind::User => Style::default().fg(theme.accent),
        CallerKind::Principal => Style::default().fg(theme.healthy),
        CallerKind::App | CallerKind::Sas => Style::default().fg(theme.degraded),
        // Shared static key and anonymous reads are the audit outliers.
        CallerKind::AccountKey | CallerKind::Anonymous => Style::default().fg(theme.critical),
        CallerKind::Unknown => Style::default().fg(theme.muted),
    };
    let result_style = if event.ok {
        Style::default().fg(theme.muted)
    } else {
        Style::default().fg(theme.critical)
    };
    Row::new(vec![
        Cell::from(when).style(Style::default().fg(theme.muted)),
        Cell::from(event.operation.as_str()).style(Style::default().fg(theme.fg)),
        Cell::from(caller_display(state, event)).style(caller_style),
        Cell::from(event.auth_type.as_str()).style(Style::default().fg(theme.muted)),
        Cell::from(event.ip.as_str()).style(Style::default().fg(theme.muted)),
        Cell::from(event.object.as_deref().unwrap_or("—")).style(Style::default().fg(theme.fg)),
        Cell::from(event.result.as_str()).style(result_style),
    ])
}

fn render_footer(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let current = state.storage.access_window.label();
    let mut segments = vec![("j/k move".to_string(), false)];
    segments.extend(super::window_rung_segments(
        &current,
        super::WINDOW_RUNGS,
        Some("t custom window"),
    ));
    for hint in [
        "m hide me",
        "Tab operation",
        "y yank",
        "r refresh",
        "Esc back",
        "? help",
    ] {
        segments.push((hint.to_string(), false));
    }
    frame.render_widget(Paragraph::new(super::footer_line(theme, &segments)), area);
}

/// Drop the fetched page and bump the generation — the query scope changed
/// (window / exclude-me). The caller's `after_action` hook spawns the refetch.
fn invalidate_fetch(state: &mut AppState) {
    state.storage.access_events = None;
    state.storage.access_error = None;
    state.storage.access_hidden = None;
    state.storage.access_truncated = false;
    state.storage.access_cursor = 0;
    state.storage.access_view_top.set(0);
    state.storage.access_operation = None;
    state.storage.access_generation = state.storage.access_generation.wrapping_add(1);
}

fn set_window(state: &mut AppState, window: AccessWindow) -> bool {
    if state.storage.access_window == window {
        return true;
    }
    state.storage.access_window = window;
    invalidate_fetch(state);
    true
}

/// Cycle the client-side operation filter: all → op₁ → op₂ → … → all, in the
/// sorted order of operations present in the fetched page.
fn cycle_operation(state: &mut AppState, direction: i32) {
    let ops = state.storage.access_operations();
    if ops.is_empty() {
        return;
    }
    let all = ops.len() as i32;
    let next = match state.storage.access_operation.as_deref() {
        None => Some((all + direction).rem_euclid(all + 1)),
        Some(current) => ops
            .iter()
            .position(|o| o == current)
            .map(|i| (i as i32 + direction).rem_euclid(all + 1)),
    };
    state.storage.access_operation = match next {
        Some(n) if n != all => Some(ops[n as usize].clone()),
        _ => None,
    };
    state.storage.access_cursor = 0;
    state.storage.access_view_top.set(0);
}

/// Yank text for the selected row: everything the table shows plus the raw
/// identity when the table shows a resolved name instead.
pub fn yank_text(state: &AppState) -> Option<String> {
    let visible = state.storage.visible_access_events();
    let event = visible.get(
        state
            .storage
            .access_cursor
            .min(visible.len().checked_sub(1)?),
    )?;
    let mut parts = vec![
        event.ts.to_rfc3339(),
        event.operation.clone(),
        caller_display(state, event),
        event.auth_type.clone(),
        event.ip.clone(),
    ];
    if event.caller_kind == CallerKind::Principal {
        parts.push(event.identity.clone());
    }
    if let Some(obj) = &event.object {
        parts.push(obj.clone());
    }
    parts.push(event.result.clone());
    Some(parts.join("  "))
}

pub fn handle(action: Action, state: &mut AppState) -> bool {
    // Custom-window input focus: raw keys flow into the input via app.rs;
    // only Enter (apply) and Esc (cancel) land here as actions.
    if state.storage.access_window_input_active {
        match action {
            Action::Back => {
                state.storage.access_window_input_active = false;
                state.storage.access_window_input.reset();
                return true;
            }
            Action::OpenSelected => {
                let raw = state.storage.access_window_input.value().to_string();
                match AccessWindow::parse(&raw) {
                    Some(window) => {
                        state.storage.access_window_input_active = false;
                        state.storage.access_window_input.reset();
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

    let len = state.storage.visible_access_events().len();
    match action {
        Action::MoveDown => {
            if len > 0 {
                state.storage.access_cursor = (state.storage.access_cursor + 1).min(len - 1);
            }
            true
        }
        Action::MoveUp => {
            state.storage.access_cursor = state.storage.access_cursor.saturating_sub(1);
            true
        }
        Action::HalfPageDown => {
            if len > 0 {
                state.storage.access_cursor =
                    (state.storage.access_cursor + HALF_PAGE).min(len - 1);
            }
            true
        }
        Action::HalfPageUp => {
            state.storage.access_cursor = state.storage.access_cursor.saturating_sub(HALF_PAGE);
            true
        }
        Action::GotoTop => {
            state.storage.access_cursor = 0;
            true
        }
        Action::GotoBottom => {
            state.storage.access_cursor = len.saturating_sub(1);
            true
        }
        Action::SetWindowHour => set_window(state, AccessWindow::Hour),
        Action::SetWindowDay => set_window(state, AccessWindow::Day),
        Action::SetWindowWeek => set_window(state, AccessWindow::Week),
        Action::SetCustomWindow => {
            state.storage.access_window_input.reset();
            state.storage.access_window_input_active = true;
            true
        }
        Action::ToggleExcludeSelf => {
            state.storage.access_exclude_self = !state.storage.access_exclude_self;
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
            // Return to wherever `l` was pressed: the containers list when
            // scoped to one container, the accounts list otherwise.
            state.view = state
                .storage
                .access_return_view
                .take()
                .unwrap_or(View::StorageAccounts);
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

    fn event(
        min_ago: i64,
        op: &str,
        identity: &str,
        kind: CallerKind,
        auth: &str,
        ip: &str,
        object: Option<&str>,
    ) -> AccessEvent {
        AccessEvent {
            ts: chrono::Utc::now() - chrono::Duration::minutes(min_ago),
            operation: op.to_string(),
            identity: identity.to_string(),
            caller_kind: kind,
            auth_type: auth.to_string(),
            ip: ip.to_string(),
            object: object.map(str::to_owned),
            result: "Success".to_string(),
            ok: true,
        }
    }

    fn fixture() -> AppState {
        let mut state = AppState::new(Config::default());
        state.view = View::StorageAccessLogs;
        state.storage.access_events = Some(vec![
            event(
                1,
                "GetBlob",
                "f3c9a2e1-0d4b-4f7e-9a1c-2b5d8e7f6a3c",
                CallerKind::Principal,
                "OAuth",
                "10.0.1.12",
                Some("invoices/a.pdf"),
            ),
            event(
                2,
                "PutBlob",
                "robbert@contoso.com",
                CallerKind::User,
                "OAuth",
                "203.0.113.7",
                Some("invoices/b.pdf"),
            ),
            event(
                3,
                "GetBlob",
                "",
                CallerKind::AccountKey,
                "AccountKey",
                "198.51.100.23",
                Some("backups/dump.bak"),
            ),
        ]);
        state
    }

    #[test]
    fn window_keys_change_window_and_drop_buffer() {
        let mut state = fixture();
        let gen0 = state.storage.access_generation;
        assert!(handle(Action::SetWindowWeek, &mut state));
        assert_eq!(state.storage.access_window, AccessWindow::Week);
        assert!(state.storage.access_events.is_none());
        assert_eq!(state.storage.access_generation, gen0 + 1);
    }

    #[test]
    fn custom_window_input_parses_and_applies() {
        let mut state = fixture();
        assert!(handle(Action::SetCustomWindow, &mut state));
        assert!(state.storage.access_window_input_active);
        state.storage.access_window_input = "6m".into();
        assert!(handle(Action::OpenSelected, &mut state));
        assert!(!state.storage.access_window_input_active);
        assert_eq!(state.storage.access_window.label(), "6m");
        assert!(
            state.storage.access_events.is_none(),
            "scope changed → refetch"
        );
    }

    #[test]
    fn exclude_self_toggle_invalidates_fetch() {
        let mut state = fixture();
        let gen0 = state.storage.access_generation;
        assert!(handle(Action::ToggleExcludeSelf, &mut state));
        assert!(state.storage.access_exclude_self);
        assert!(state.storage.access_events.is_none());
        assert_eq!(state.storage.access_generation, gen0 + 1);
    }

    #[test]
    fn tab_cycles_operation_filter_through_sorted_ops() {
        let mut state = fixture();
        // all → GetBlob → PutBlob → all
        assert!(handle(Action::CycleSourceFilter, &mut state));
        assert_eq!(state.storage.access_operation.as_deref(), Some("GetBlob"));
        assert_eq!(state.storage.visible_access_events().len(), 2);
        assert!(handle(Action::CycleSourceFilter, &mut state));
        assert_eq!(state.storage.access_operation.as_deref(), Some("PutBlob"));
        assert!(handle(Action::CycleSourceFilter, &mut state));
        assert_eq!(state.storage.access_operation, None);
    }

    #[test]
    fn back_returns_to_recorded_origin() {
        let mut state = fixture();
        state.storage.access_return_view = Some(View::StorageContainers);
        assert!(handle(Action::Back, &mut state));
        assert_eq!(state.view, View::StorageContainers);
        // Without a recorded origin, fall back to the accounts list.
        state.view = View::StorageAccessLogs;
        assert!(handle(Action::Back, &mut state));
        assert_eq!(state.view, View::StorageAccounts);
    }

    #[test]
    fn caller_renders_resolved_principal_and_auth_labels() {
        let mut state = fixture();
        state.principals.by_id.insert(
            "f3c9a2e1-0d4b-4f7e-9a1c-2b5d8e7f6a3c".to_string(),
            "sp-orders-deploy".to_string(),
        );
        let events = state.storage.visible_access_events();
        let resolved = caller_display(&state, events[0]);
        let key = caller_display(&state, events[2]);
        assert_eq!(resolved, "sp-orders-deploy");
        assert_eq!(key, "account key");
    }

    #[test]
    fn yank_includes_object_and_raw_identity() {
        let mut state = fixture();
        state.storage.access_cursor = 0;
        let y = yank_text(&state).unwrap();
        assert!(y.contains("GetBlob"));
        assert!(y.contains("invoices/a.pdf"));
        assert!(y.contains("f3c9a2e1-0d4b-4f7e-9a1c-2b5d8e7f6a3c"));
    }

    #[test]
    fn renders_table_scope_and_hidden_chips() {
        let theme = crate::ui::theme::Theme::catppuccin_mocha();
        let backend = TestBackend::new(190, 20);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.storage.access_scope = Some("invoices".to_string());
        state.storage.access_exclude_self = true;
        state.storage.access_hidden = Some(crate::azure::demo::self_identity());
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("robbert@contoso.com"), "caller column missing");
        assert!(s.contains("invoices ✓"), "scope chip missing");
        assert!(
            s.contains("hiding robbert@contoso.com"),
            "hidden chip missing"
        );
        assert!(s.contains("AUTH"), "header missing");
        assert!(s.contains("account key"), "auth label missing");
    }

    #[test]
    fn renders_empty_state_with_diagnostics_warning() {
        let theme = crate::ui::theme::Theme::catppuccin_mocha();
        let backend = TestBackend::new(120, 16);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.storage.access_events = Some(Vec::new());
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("Diagnostic settings"));
        assert!(s.contains("StorageRead"));
    }
}
