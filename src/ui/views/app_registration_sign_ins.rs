//! Sign-in log drill-in for the pinned app registration: every sign-in where
//! the app was the client — interactive users, silent refreshes, and the
//! client-credential / managed-identity flows that make up daemon usage.
//! Opened with Enter or `l` on an app row.
//!
//! Filters:
//! - `0` / `1` / `7` / `3` pick the 1h / 1d / 7d / 30d window; `t` takes a
//!   free-form window. Entra retains sign-ins 7d (Free) / 30d (P1+), so
//!   anything longer legitimately adds nothing.
//! - `m` excludes *you* (client-side — Graph's filter has no `ne`).
//! - Tab / Shift-Tab cycle a client-side sign-in *kind* filter through the
//!   kinds present in the fetched page.
//!
//! Window and exclude-me are *query* parameters: changing them drops the
//! buffer and refetches (guarded by `sign_ins_generation`). The kind filter
//! narrows the cached page.

#![allow(dead_code, unused_variables)]

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;

use crate::azure::app_registration_logs::{SignInEvent, SignInKind};
use crate::azure::key_vault_logs::AccessWindow;
use crate::ui::events::Action;
use crate::ui::state::{AppState, View};
use crate::ui::theme::Theme;

const FOOTER_HINT: &str = "j/k move  0 1h  1 1d  7 7d  3 30d  t custom window  m hide me  Tab kind  y yank  r refresh  Esc back  ? help";
const HALF_PAGE: usize = 10;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);

    let visible = state.app_reg.visible_sign_ins();

    let app_label = state
        .app_reg
        .selected_app
        .as_ref()
        .map(|a| a.display_name.clone())
        .unwrap_or_default();
    let mut title_spans: Vec<Span> = vec![
        Span::styled(" sign-ins ", Style::default().fg(theme.fg)),
        Span::styled(format!("· {app_label} "), Style::default().fg(theme.accent)),
    ];
    if let Some(total) = state.app_reg.sign_ins.as_ref().map(|e| e.len()) {
        let shown = visible.len();
        let count = if shown != total {
            format!("· {shown} of {total} rows ")
        } else {
            format!("· {total} rows ")
        };
        title_spans.push(Span::styled(count, Style::default().fg(theme.muted)));
    }
    title_spans.push(Span::styled(
        format!("· {} ", state.app_reg.sign_ins_window.label()),
        Style::default().fg(theme.fg),
    ));
    if state.app_reg.sign_ins_truncated {
        title_spans.push(Span::styled(
            "· newest rows only (window has more) ",
            Style::default().fg(theme.degraded),
        ));
    }
    if let Some(kind) = state.app_reg.sign_ins_kind {
        title_spans.push(Span::styled(
            format!("· kind: {} ", kind.label()),
            Style::default().fg(theme.accent),
        ));
    }
    if state.app_reg.sign_ins_exclude_self {
        let who = state
            .app_reg
            .sign_ins_hidden
            .as_ref()
            .map(|h| h.label())
            .unwrap_or_else(|| "you".to_string());
        title_spans.push(Span::styled(
            format!("· hiding {who} ✓ "),
            Style::default().fg(theme.accent),
        ));
    }
    // The full fallback reason lives in `sign_ins_note`; the chip carries the
    // operative fact (daemon usage is invisible until the beta call works).
    if state.app_reg.sign_ins_note.is_some() {
        title_spans.push(Span::styled(
            "· interactive only ",
            Style::default().fg(theme.degraded),
        ));
    }
    if state.app_reg.sign_ins_pending && state.app_reg.sign_ins.is_some() {
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
    let (input_area, body_area) = if state.app_reg.sign_ins_window_input_active {
        let parts = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(inner);
        (Some(parts[0]), parts[1])
    } else {
        (None, inner)
    };
    if let Some(ia) = input_area {
        let p = Paragraph::new(Line::from(vec![
            Span::styled("window> ", Style::default().fg(theme.accent)),
            Span::styled(
                state.app_reg.sign_ins_window_input.value(),
                Style::default().fg(theme.fg),
            ),
            Span::styled("█", Style::default().fg(theme.accent)),
            Span::styled(
                "  (e.g. 12h, 7d, 30d — Enter applies, Esc cancels; Entra retains ≤30d)",
                Style::default().fg(theme.muted),
            ),
        ]));
        frame.render_widget(p, ia);
    }

    if let Some(err) = state.app_reg.sign_ins_error.as_ref() {
        let p = Paragraph::new(Text::styled(
            format!("error: {err}"),
            Style::default().fg(theme.critical),
        ))
        .wrap(Wrap { trim: false });
        frame.render_widget(p, body_area);
        render_footer(frame, chunks[1], theme);
        return;
    }

    match state.app_reg.sign_ins.as_ref() {
        None if state.app_reg.sign_ins_pending => {
            let p = Paragraph::new(Line::from(Span::styled(
                "loading sign-in log…",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        None => {
            let p = Paragraph::new(Line::from(Span::styled(
                "press r to load the sign-in log.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(rows) if rows.is_empty() => {
            // Zero rows is a real answer here — nothing signed in with this
            // app in the window — but the retention caveat matters.
            let p = Paragraph::new(Text::styled(
                "no sign-ins in this window — nothing used this app registration.\n\nnote: Entra keeps sign-in logs 7 days (Free) / 30 days (P1/P2); windows\nbeyond that can't see further back.",
                Style::default().fg(theme.muted),
            ));
            frame.render_widget(p, body_area);
        }
        Some(_) if visible.is_empty() => {
            let p = Paragraph::new(Line::from(Span::styled(
                "no rows match the current kind filter.",
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
            let widths = [
                Constraint::Length(19),       // WHEN
                Constraint::Length(17),       // KIND
                Constraint::Length(caller_w), // CALLER
                Constraint::Length(15),       // IP
                Constraint::Min(12),          // RESOURCE
                Constraint::Length(16),       // LOCATION
                Constraint::Length(8),        // RESULT
            ];
            let header_row = Row::new(vec![
                "WHEN", "KIND", "CALLER", "IP", "RESOURCE", "LOCATION", "RESULT",
            ])
            .style(
                Style::default()
                    .fg(theme.muted)
                    .add_modifier(Modifier::BOLD),
            );
            let cursor = state.app_reg.sign_ins_cursor.min(visible.len() - 1);
            let body_rows: Vec<Row> = visible.iter().map(|e| build_row(e, theme)).collect();
            let table = Table::new(body_rows, widths)
                .header(header_row)
                .row_highlight_style(theme.selection())
                .highlight_symbol("▍ ")
                .column_spacing(2);
            let mut ts = TableState::default().with_offset(state.app_reg.sign_ins_view_top.get());
            ts.select(Some(cursor));
            frame.render_stateful_widget(table, body_area, &mut ts);
            state.app_reg.sign_ins_view_top.set(ts.offset());
        }
    }

    render_footer(frame, chunks[1], theme);
}

fn build_row<'a>(event: &'a SignInEvent, theme: &Theme) -> Row<'a> {
    let when = event
        .ts
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    let kind_style = match event.kind {
        SignInKind::Interactive => Style::default().fg(theme.accent),
        SignInKind::NonInteractive => Style::default().fg(theme.muted),
        SignInKind::ServicePrincipal => Style::default().fg(theme.healthy),
        SignInKind::ManagedIdentity => Style::default().fg(theme.healthy),
        SignInKind::Unknown => Style::default().fg(theme.muted),
    };
    // Failed sign-ins are what an auditor hunts for — flag them red.
    let ok = event.result == "OK";
    let result_style = if ok {
        Style::default().fg(theme.muted)
    } else {
        Style::default().fg(theme.critical)
    };
    Row::new(vec![
        Cell::from(when).style(Style::default().fg(theme.muted)),
        Cell::from(event.kind.label()).style(kind_style),
        Cell::from(event.caller.as_str()).style(Style::default().fg(theme.fg)),
        Cell::from(event.ip.as_str()).style(Style::default().fg(theme.muted)),
        Cell::from(event.resource.as_str()).style(Style::default().fg(theme.fg)),
        Cell::from(event.location.as_str()).style(Style::default().fg(theme.muted)),
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
/// (window / exclude-me). The caller's `after_action` hook spawns the refetch.
fn invalidate_fetch(state: &mut AppState) {
    state.app_reg.sign_ins = None;
    state.app_reg.sign_ins_error = None;
    state.app_reg.sign_ins_hidden = None;
    state.app_reg.sign_ins_truncated = false;
    state.app_reg.sign_ins_note = None;
    state.app_reg.sign_ins_cursor = 0;
    state.app_reg.sign_ins_view_top.set(0);
    state.app_reg.sign_ins_kind = None;
    state.app_reg.sign_ins_generation = state.app_reg.sign_ins_generation.wrapping_add(1);
}

fn set_window(state: &mut AppState, window: AccessWindow) -> bool {
    if state.app_reg.sign_ins_window == window {
        return true;
    }
    state.app_reg.sign_ins_window = window;
    invalidate_fetch(state);
    true
}

/// Cycle the client-side kind filter: all → kind₁ → kind₂ → … → all, in the
/// label order of kinds present in the fetched page.
fn cycle_kind(state: &mut AppState, direction: i32) {
    let kinds = state.app_reg.sign_in_kinds();
    if kinds.is_empty() {
        return;
    }
    let all = kinds.len() as i32;
    let next = match state.app_reg.sign_ins_kind {
        None => Some((all + direction).rem_euclid(all + 1)),
        Some(current) => kinds
            .iter()
            .position(|k| *k == current)
            .map(|i| (i as i32 + direction).rem_euclid(all + 1)),
    };
    state.app_reg.sign_ins_kind = match next {
        Some(n) if n != all => Some(kinds[n as usize]),
        _ => None,
    };
    state.app_reg.sign_ins_cursor = 0;
    state.app_reg.sign_ins_view_top.set(0);
}

/// Yank text for the selected row: everything the table shows plus the
/// client app and the failure reason (the table has no room for either).
pub fn yank_text(state: &AppState) -> Option<String> {
    let visible = state.app_reg.visible_sign_ins();
    let event = visible.get(
        state
            .app_reg
            .sign_ins_cursor
            .min(visible.len().checked_sub(1)?),
    )?;
    let mut parts = vec![
        event.ts.to_rfc3339(),
        event.kind.label().to_string(),
        event.caller.clone(),
        event.ip.clone(),
        event.resource.clone(),
    ];
    if !event.client_app.is_empty() {
        parts.push(event.client_app.clone());
    }
    if !event.location.is_empty() {
        parts.push(event.location.clone());
    }
    parts.push(event.result.clone());
    if let Some(reason) = &event.failure_reason {
        parts.push(reason.clone());
    }
    Some(parts.join("  "))
}

pub fn handle(action: Action, state: &mut AppState) -> bool {
    // Custom-window input focus: raw keys flow into the input via app.rs;
    // only Enter (apply) and Esc (cancel) land here as actions.
    if state.app_reg.sign_ins_window_input_active {
        match action {
            Action::Back => {
                state.app_reg.sign_ins_window_input_active = false;
                state.app_reg.sign_ins_window_input.reset();
                return true;
            }
            Action::OpenSelected => {
                let raw = state.app_reg.sign_ins_window_input.value().to_string();
                match AccessWindow::parse(&raw) {
                    Some(window) => {
                        state.app_reg.sign_ins_window_input_active = false;
                        state.app_reg.sign_ins_window_input.reset();
                        set_window(state, window);
                    }
                    None => {
                        state.set_status(format!("can't parse \"{raw}\" — try 12h, 7d, or 30d"));
                    }
                }
                return true;
            }
            _ => return false,
        }
    }

    let len = state.app_reg.visible_sign_ins().len();
    match action {
        Action::MoveDown => {
            if len > 0 {
                state.app_reg.sign_ins_cursor = (state.app_reg.sign_ins_cursor + 1).min(len - 1);
            }
            true
        }
        Action::MoveUp => {
            state.app_reg.sign_ins_cursor = state.app_reg.sign_ins_cursor.saturating_sub(1);
            true
        }
        Action::HalfPageDown => {
            if len > 0 {
                state.app_reg.sign_ins_cursor =
                    (state.app_reg.sign_ins_cursor + HALF_PAGE).min(len - 1);
            }
            true
        }
        Action::HalfPageUp => {
            state.app_reg.sign_ins_cursor = state.app_reg.sign_ins_cursor.saturating_sub(HALF_PAGE);
            true
        }
        Action::GotoTop => {
            state.app_reg.sign_ins_cursor = 0;
            true
        }
        Action::GotoBottom => {
            state.app_reg.sign_ins_cursor = len.saturating_sub(1);
            true
        }
        Action::SetWindowHour => set_window(state, AccessWindow::Hour),
        Action::SetWindowDay => set_window(state, AccessWindow::Day),
        Action::SetWindowWeek => set_window(state, AccessWindow::Week),
        Action::SetWindowMonth => set_window(
            state,
            AccessWindow::Custom {
                hours: 30 * 24,
                label: "30d".to_string(),
            },
        ),
        Action::SetCustomWindow => {
            state.app_reg.sign_ins_window_input.reset();
            state.app_reg.sign_ins_window_input_active = true;
            true
        }
        Action::ToggleExcludeSelf => {
            state.app_reg.sign_ins_exclude_self = !state.app_reg.sign_ins_exclude_self;
            invalidate_fetch(state);
            true
        }
        Action::CycleSourceFilter => {
            cycle_kind(state, 1);
            true
        }
        Action::CycleSourceFilterBack => {
            cycle_kind(state, -1);
            true
        }
        Action::Back => {
            state.view = state
                .app_reg
                .sign_ins_return_view
                .take()
                .unwrap_or(View::AppRegistrations);
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

    fn event(min_ago: i64, kind: SignInKind, caller: &str, ip: &str, result: &str) -> SignInEvent {
        SignInEvent {
            ts: chrono::Utc::now() - chrono::Duration::minutes(min_ago),
            kind,
            caller: caller.to_string(),
            ip: ip.to_string(),
            resource: "Microsoft Graph".to_string(),
            client_app: "Browser".to_string(),
            result: result.to_string(),
            failure_reason: (result != "OK").then(|| "Invalid client secret.".to_string()),
            location: "Amsterdam, NL".to_string(),
        }
    }

    fn fixture() -> AppState {
        let mut state = AppState::new(Config::default());
        state.view = View::AppRegistrationSignIns;
        state.app_reg.selected_app = Some(crate::azure::demo::app_registrations().apps[0].clone());
        state.app_reg.sign_ins = Some(vec![
            event(
                1,
                SignInKind::Interactive,
                "dana@contoso.com",
                "198.51.100.23",
                "OK",
            ),
            event(
                2,
                SignInKind::ServicePrincipal,
                "Contoso Orders API",
                "20.93.10.4",
                "OK",
            ),
            event(
                3,
                SignInKind::ServicePrincipal,
                "Contoso Orders API",
                "20.93.10.4",
                "7000215",
            ),
        ]);
        state
    }

    #[test]
    fn window_keys_change_window_and_drop_buffer() {
        let mut state = fixture();
        let gen0 = state.app_reg.sign_ins_generation;
        assert!(handle(Action::SetWindowDay, &mut state));
        assert_eq!(state.app_reg.sign_ins_window, AccessWindow::Day);
        assert!(state.app_reg.sign_ins.is_none());
        assert_eq!(state.app_reg.sign_ins_generation, gen0 + 1);
        // 30d rung.
        assert!(handle(Action::SetWindowMonth, &mut state));
        assert_eq!(state.app_reg.sign_ins_window.label(), "30d");
        // Same window again: no invalidation.
        let gen1 = state.app_reg.sign_ins_generation;
        state.app_reg.sign_ins = Some(Vec::new());
        assert!(handle(Action::SetWindowMonth, &mut state));
        assert!(state.app_reg.sign_ins.is_some());
        assert_eq!(state.app_reg.sign_ins_generation, gen1);
    }

    #[test]
    fn custom_window_input_parses_and_applies() {
        let mut state = fixture();
        assert!(handle(Action::SetCustomWindow, &mut state));
        assert!(state.app_reg.sign_ins_window_input_active);
        state.app_reg.sign_ins_window_input = "12h".into();
        assert!(handle(Action::OpenSelected, &mut state));
        assert!(!state.app_reg.sign_ins_window_input_active);
        assert_eq!(state.app_reg.sign_ins_window.label(), "12h");
        assert!(state.app_reg.sign_ins.is_none(), "scope changed → refetch");
        // Junk stays focused with a status message.
        handle(Action::SetCustomWindow, &mut state);
        state.app_reg.sign_ins_window_input = "junk".into();
        assert!(handle(Action::OpenSelected, &mut state));
        assert!(state.app_reg.sign_ins_window_input_active);
        assert!(state.status_message.is_some());
    }

    #[test]
    fn exclude_self_toggle_invalidates_fetch() {
        let mut state = fixture();
        let gen0 = state.app_reg.sign_ins_generation;
        assert!(handle(Action::ToggleExcludeSelf, &mut state));
        assert!(state.app_reg.sign_ins_exclude_self);
        assert!(state.app_reg.sign_ins.is_none());
        assert_eq!(state.app_reg.sign_ins_generation, gen0 + 1);
    }

    #[test]
    fn tab_cycles_kind_filter_through_present_kinds() {
        let mut state = fixture();
        // Kinds present: interactive, service principal (label-sorted).
        assert!(handle(Action::CycleSourceFilter, &mut state));
        assert_eq!(state.app_reg.sign_ins_kind, Some(SignInKind::Interactive));
        assert_eq!(state.app_reg.visible_sign_ins().len(), 1);
        assert!(handle(Action::CycleSourceFilter, &mut state));
        assert_eq!(
            state.app_reg.sign_ins_kind,
            Some(SignInKind::ServicePrincipal)
        );
        assert_eq!(state.app_reg.visible_sign_ins().len(), 2);
        assert!(handle(Action::CycleSourceFilter, &mut state));
        assert_eq!(state.app_reg.sign_ins_kind, None);
        assert_eq!(state.app_reg.visible_sign_ins().len(), 3);
    }

    #[test]
    fn back_returns_to_app_list() {
        let mut state = fixture();
        state.app_reg.sign_ins_return_view = Some(View::AppRegistrations);
        assert!(handle(Action::Back, &mut state));
        assert_eq!(state.view, View::AppRegistrations);
        // Without a recorded origin, still fall back to the list.
        state.view = View::AppRegistrationSignIns;
        assert!(handle(Action::Back, &mut state));
        assert_eq!(state.view, View::AppRegistrations);
    }

    #[test]
    fn yank_includes_failure_reason() {
        let mut state = fixture();
        state.app_reg.sign_ins_cursor = 2;
        let y = yank_text(&state).unwrap();
        assert!(y.contains("7000215"));
        assert!(y.contains("Invalid client secret."));
        assert!(y.contains("service principal"));
    }

    #[test]
    fn renders_table_with_kind_and_hidden_chips() {
        let theme = crate::ui::theme::Theme::catppuccin_mocha();
        let backend = TestBackend::new(180, 20);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.app_reg.sign_ins_exclude_self = true;
        state.app_reg.sign_ins_hidden = Some(crate::azure::demo::self_identity());
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("dana@contoso.com"), "caller column missing");
        assert!(s.contains("service principal"), "kind column missing");
        assert!(
            s.contains("hiding robbert@contoso.com"),
            "hidden chip missing"
        );
        assert!(s.contains("WHEN"), "header missing");
    }

    #[test]
    fn renders_empty_state_with_retention_hint() {
        let theme = crate::ui::theme::Theme::catppuccin_mocha();
        let backend = TestBackend::new(120, 14);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.app_reg.sign_ins = Some(Vec::new());
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("nothing used this app registration"));
        assert!(s.contains("30 days"));
    }
}
