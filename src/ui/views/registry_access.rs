//! Container Registry access-log drill-in: who pulled / pushed which image,
//! when, from where — the `ContainerRegistryRepositoryEvents` rows the
//! registry's diagnostic setting forwards to Log Analytics. Opened with `l`
//! on a registry row (registry-wide) or on a repository row (pre-scoped to
//! that repository). Mirrors the Key Vault access view's controls:
//!
//! - `0` / `1` / `7` pick the 1h / 1d / 7d window; `t` takes a free-form
//!   window like `6m` or `1y`.
//! - `m` excludes *you* server-side — your UPN, object id, and sign-in IP,
//!   decoded from the bearer token's claims.
//! - Tab / Shift-Tab cycle a client-side `OperationName` filter (`Pull`,
//!   `Push`, …) through the operations present in the fetched page.
//!
//! Window, exclude-me, and repository scope are *query* parameters: changing
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
use crate::azure::registry_logs::{AccessEvent, CallerKind};
use crate::ui::events::Action;
use crate::ui::state::{AppState, View};
use crate::ui::theme::Theme;

use super::detail::format_count;
use super::metric_chart::{render_chart_row, render_time_axis_minutes};
use super::sql_audit::display_principal;

const FOOTER_HINT: &str = "j/k move  0 1h  1 1d  7 7d  t custom window  m hide me  Tab operation  y yank  r refresh  Esc back  ? help";
const HALF_PAGE: usize = 10;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);

    let visible = state.registry.visible_access_events();

    let mut title_spans: Vec<Span> =
        vec![Span::styled(" access log ", Style::default().fg(theme.fg))];
    if let Some(total) = state.registry.access_events.as_ref().map(|e| e.len()) {
        let shown = visible.len();
        let count = if shown != total {
            format!("· {shown} of {total} rows ")
        } else {
            format!("· {total} rows ")
        };
        title_spans.push(Span::styled(count, Style::default().fg(theme.muted)));
    }
    title_spans.push(Span::styled(
        format!("· {} ", state.registry.access_window.label()),
        Style::default().fg(theme.fg),
    ));
    if state.registry.access_truncated {
        title_spans.push(Span::styled(
            "· newest rows only (window has more) ",
            Style::default().fg(theme.degraded),
        ));
    }
    if let Some(repo) = state.registry.access_scope.as_deref() {
        title_spans.push(Span::styled(
            format!("· {repo} ✓ "),
            Style::default().fg(theme.accent),
        ));
    }
    if let Some(op) = state.registry.access_operation.as_deref() {
        title_spans.push(Span::styled(
            format!("· op: {op} "),
            Style::default().fg(theme.accent),
        ));
    }
    if state.registry.access_exclude_self {
        let who = state
            .registry
            .access_hidden
            .as_ref()
            .map(|h| h.label())
            .unwrap_or_else(|| "you".to_string());
        title_spans.push(Span::styled(
            format!("· hiding {who} ✓ "),
            Style::default().fg(theme.accent),
        ));
    }
    if state.registry.access_pending && state.registry.access_events.is_some() {
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
    let (input_area, body_area) = if state.registry.access_window_input_active {
        let parts = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(inner);
        (Some(parts[0]), parts[1])
    } else {
        (None, inner)
    };
    if let Some(ia) = input_area {
        let p = Paragraph::new(Line::from(vec![
            Span::styled("window> ", Style::default().fg(theme.accent)),
            Span::styled(
                state.registry.access_window_input.value(),
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

    // Pull/push activity chart from Monitor platform metrics — carved off
    // above whatever the rest of the body shows (table, empty state, or
    // error), because it works precisely when the event query doesn't:
    // platform metrics need no diagnostic setting.
    let body_area = if state.registry.access_metrics.is_some() && body_area.height >= 14 {
        let parts = Layout::vertical([Constraint::Length(CHART_HEIGHT), Constraint::Min(0)])
            .split(body_area);
        render_activity_charts(frame, parts[0], state, theme);
        parts[1]
    } else {
        body_area
    };

    if let Some(err) = state.registry.access_error.as_ref() {
        let mut lines = vec![Line::from(Span::styled(
            format!("error: {err}"),
            Style::default().fg(theme.critical),
        ))];
        // A workspace that never received registry logs fails KQL table
        // resolution — same root cause as the empty page, so same hint.
        if err.contains("Failed to resolve table") || err.contains("SEM0100") {
            lines.push(Line::default());
            lines.extend(diagnostics_warning_lines(state, theme));
        }
        let p = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
        frame.render_widget(p, body_area);
        render_footer(frame, chunks[1], theme);
        return;
    }

    match state.registry.access_events.as_ref() {
        None if state.registry.access_pending => {
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
            // recorded. Almost always it's the latter — forwarding repository
            // events is opt-in and most registries never got the diagnostic
            // setting — so warn loudly instead of shrugging, and let the
            // metrics chart prove pulls are happening but going unlogged.
            let mut lines = vec![
                Line::from(Span::styled(
                    "no repository events in this window.",
                    Style::default().fg(theme.muted),
                )),
                Line::default(),
            ];
            lines.extend(diagnostics_warning_lines(state, theme));
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
                Constraint::Length(15),       // IP
                Constraint::Min(12),          // IMAGE
                Constraint::Length(9),        // RESULT
            ];
            let header_row = Row::new(vec!["WHEN", "OPERATION", "CALLER", "IP", "IMAGE", "RESULT"])
                .style(
                    Style::default()
                        .fg(theme.muted)
                        .add_modifier(Modifier::BOLD),
                );
            let cursor = state.registry.access_cursor.min(visible.len() - 1);
            let body_rows: Vec<Row> = visible.iter().map(|e| build_row(state, e, theme)).collect();
            let table = Table::new(body_rows, widths)
                .header(header_row)
                .row_highlight_style(theme.selection())
                .highlight_symbol("▍ ")
                .column_spacing(2);
            let mut ts = TableState::default().with_offset(state.registry.access_view_top.get());
            ts.select(Some(cursor));
            frame.render_stateful_widget(table, body_area, &mut ts);
            state.registry.access_view_top.set(ts.offset());
        }
    }

    render_footer(frame, chunks[1], theme);
}

/// Two 3-line sparkline rows (Pulls, Pushes) plus a 1-line time axis.
const CHART_HEIGHT: u16 = 7;

/// The pulls/pushes sparklines over the current window, from Monitor platform
/// metrics. Registry-wide even when the event query below is scoped to one
/// repository — the metrics carry no repository dimension, which the summary
/// chip calls out to avoid reading the bars as that repo's traffic.
fn render_activity_charts(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let Some(activity) = state.registry.access_metrics.as_ref() else {
        return;
    };
    let rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .split(area);
    let scope_note = if state.registry.access_scope.is_some() {
        " · whole registry"
    } else {
        ""
    };
    let pulls_summary = format!(
        "total: {}{}",
        format_count(activity.pull_total()),
        scope_note
    );
    let pushes_summary = format!(
        "total: {}{}",
        format_count(activity.push_total()),
        scope_note
    );
    render_chart_row(
        frame,
        rows[0],
        activity.pulls.kind,
        "Pulls",
        Some(&activity.pulls),
        &pulls_summary,
        None,
        theme,
    );
    render_chart_row(
        frame,
        rows[1],
        activity.pushes.kind,
        "Pushes",
        Some(&activity.pushes),
        &pushes_summary,
        None,
        theme,
    );
    render_time_axis_minutes(
        frame,
        rows[2],
        state.registry.access_window.duration().num_minutes(),
        theme,
    );
}

/// The "your registry probably isn't logging anything" warning, shared by the
/// empty page and the no-such-table error. Orange (`client_error`) rather than
/// muted: an auditor who trusts an empty page here walks away thinking nobody
/// pulls their images, and forwarding repository events is OFF by default on
/// ACR, so the empty page is almost always a configuration gap — not quiet.
/// When Monitor metrics counted activity in the same window, say so: that's
/// proof pulls are happening but going unrecorded.
fn diagnostics_warning_lines(state: &AppState, theme: &Theme) -> Vec<Line<'static>> {
    let warn = Style::default().fg(theme.client_error);
    let mut lines = vec![
        Line::from(Span::styled(
            "⚠ most likely cause: this registry is not forwarding its repository events anywhere.",
            warn.add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "ACR does NOT record pull/push logs by default. A diagnostic setting on the registry must",
            warn,
        )),
        Line::from(Span::styled(
            "explicitly send the ContainerRegistryRepositoryEvents category to a Log Analytics",
            warn,
        )),
        Line::from(Span::styled(
            "workspace, and only events from after that point are captured",
            warn,
        )),
        Line::from(Span::styled(
            "(Portal: registry → Monitoring → Diagnostic settings).",
            warn,
        )),
    ];
    if let Some(activity) = state.registry.access_metrics.as_ref() {
        if activity.any_activity() {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                format!(
                    "monitor metrics count ~{} pulls / ~{} pushes in this window — images ARE being pulled, but nothing records by whom.",
                    format_count(activity.pull_total()),
                    format_count(activity.push_total()),
                ),
                warn.add_modifier(Modifier::BOLD),
            )));
        }
    }
    lines
}

/// Caller column text: the Graph-resolved display name when the identity is a
/// GUID we've resolved, the raw value otherwise, with fixed labels for the
/// identity-less shapes.
fn caller_display(state: &AppState, event: &AccessEvent) -> String {
    match event.caller_kind {
        CallerKind::Anonymous => "anonymous".to_string(),
        CallerKind::Admin => format!("{} (admin)", event.identity),
        CallerKind::Principal => display_principal(state, &event.identity),
        _ => event.identity.clone(),
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
        // Admin user and anonymous pulls are the audit outliers — flag them.
        CallerKind::Admin | CallerKind::Anonymous => Style::default().fg(theme.critical),
        CallerKind::Unknown => Style::default().fg(theme.muted),
    };
    // `ResultDescription` is normally empty on success; anything present is
    // the failure text an auditor is hunting for.
    let (result, result_style) = if event.result.is_empty() {
        ("—".to_string(), Style::default().fg(theme.muted))
    } else {
        (event.result.clone(), Style::default().fg(theme.critical))
    };
    Row::new(vec![
        Cell::from(when).style(Style::default().fg(theme.muted)),
        Cell::from(event.operation.as_str()).style(Style::default().fg(theme.fg)),
        Cell::from(caller_display(state, event)).style(caller_style),
        Cell::from(event.ip.as_str()).style(Style::default().fg(theme.muted)),
        Cell::from(event.image()).style(Style::default().fg(theme.fg)),
        Cell::from(result).style(result_style),
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
    state.registry.access_events = None;
    state.registry.access_error = None;
    state.registry.access_hidden = None;
    state.registry.access_truncated = false;
    state.registry.access_cursor = 0;
    state.registry.access_view_top.set(0);
    state.registry.access_operation = None;
    state.registry.access_generation = state.registry.access_generation.wrapping_add(1);
}

fn set_window(state: &mut AppState, window: AccessWindow) -> bool {
    if state.registry.access_window == window {
        return true;
    }
    state.registry.access_window = window;
    invalidate_fetch(state);
    true
}

/// Cycle the client-side operation filter: all → op₁ → op₂ → … → all, in the
/// sorted order of operations present in the fetched page.
fn cycle_operation(state: &mut AppState, direction: i32) {
    let ops = state.registry.access_operations();
    if ops.is_empty() {
        return;
    }
    let all = ops.len() as i32;
    let next = match state.registry.access_operation.as_deref() {
        None => Some((all + direction).rem_euclid(all + 1)),
        Some(current) => ops
            .iter()
            .position(|o| o == current)
            .map(|i| (i as i32 + direction).rem_euclid(all + 1)),
    };
    state.registry.access_operation = match next {
        Some(n) if n != all => Some(ops[n as usize].clone()),
        _ => None,
    };
    state.registry.access_cursor = 0;
    state.registry.access_view_top.set(0);
}

/// Yank text for the selected row: everything the table shows plus the raw
/// identity and the full digest (the table shortens both).
pub fn yank_text(state: &AppState) -> Option<String> {
    let visible = state.registry.visible_access_events();
    let event = visible.get(
        state
            .registry
            .access_cursor
            .min(visible.len().checked_sub(1)?),
    )?;
    let mut parts = vec![
        event.ts.to_rfc3339(),
        event.operation.clone(),
        caller_display(state, event),
        event.ip.clone(),
        event.image(),
    ];
    if event.caller_kind == CallerKind::Principal {
        parts.push(event.identity.clone());
    }
    if let Some(digest) = &event.digest {
        parts.push(digest.clone());
    }
    if !event.result.is_empty() {
        parts.push(event.result.clone());
    }
    Some(parts.join("  "))
}

pub fn handle(action: Action, state: &mut AppState) -> bool {
    // Custom-window input focus: raw keys flow into the input via app.rs;
    // only Enter (apply) and Esc (cancel) land here as actions.
    if state.registry.access_window_input_active {
        match action {
            Action::Back => {
                state.registry.access_window_input_active = false;
                state.registry.access_window_input.reset();
                return true;
            }
            Action::OpenSelected => {
                let raw = state.registry.access_window_input.value().to_string();
                match AccessWindow::parse(&raw) {
                    Some(window) => {
                        state.registry.access_window_input_active = false;
                        state.registry.access_window_input.reset();
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

    let len = state.registry.visible_access_events().len();
    match action {
        Action::MoveDown => {
            if len > 0 {
                state.registry.access_cursor = (state.registry.access_cursor + 1).min(len - 1);
            }
            true
        }
        Action::MoveUp => {
            state.registry.access_cursor = state.registry.access_cursor.saturating_sub(1);
            true
        }
        Action::HalfPageDown => {
            if len > 0 {
                state.registry.access_cursor =
                    (state.registry.access_cursor + HALF_PAGE).min(len - 1);
            }
            true
        }
        Action::HalfPageUp => {
            state.registry.access_cursor = state.registry.access_cursor.saturating_sub(HALF_PAGE);
            true
        }
        Action::GotoTop => {
            state.registry.access_cursor = 0;
            true
        }
        Action::GotoBottom => {
            state.registry.access_cursor = len.saturating_sub(1);
            true
        }
        Action::SetWindowHour => set_window(state, AccessWindow::Hour),
        Action::SetWindowDay => set_window(state, AccessWindow::Day),
        Action::SetWindowWeek => set_window(state, AccessWindow::Week),
        Action::SetCustomWindow => {
            state.registry.access_window_input.reset();
            state.registry.access_window_input_active = true;
            true
        }
        Action::ToggleExcludeSelf => {
            state.registry.access_exclude_self = !state.registry.access_exclude_self;
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
            // Return to wherever `l` was pressed: the repositories list when
            // scoped to one repo, the registries list otherwise.
            state.view = state
                .registry
                .access_return_view
                .take()
                .unwrap_or(View::Registries);
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
        ip: &str,
        repo: &str,
        tag: Option<&str>,
    ) -> AccessEvent {
        AccessEvent {
            ts: chrono::Utc::now() - chrono::Duration::minutes(min_ago),
            operation: op.to_string(),
            identity: identity.to_string(),
            caller_kind: kind,
            ip: ip.to_string(),
            repository: repo.to_string(),
            tag: tag.map(str::to_owned),
            digest: None,
            result: String::new(),
        }
    }

    fn fixture() -> AppState {
        let mut state = AppState::new(Config::default());
        state.view = View::RegistryAccessLogs;
        state.registry.access_events = Some(vec![
            event(
                1,
                "Pull",
                "f3c9a2e1-0d4b-4f7e-9a1c-2b5d8e7f6a3c",
                CallerKind::Principal,
                "10.240.0.4",
                "ca-checkout-api",
                Some("1.7.3"),
            ),
            event(
                2,
                "Push",
                "robbert@contoso.com",
                CallerKind::User,
                "203.0.113.7",
                "ca-checkout-api",
                Some("1.7.3"),
            ),
            event(
                3,
                "Pull",
                "dana@contoso.com",
                CallerKind::User,
                "198.51.100.23",
                "base/dotnet-runtime",
                None,
            ),
        ]);
        state
    }

    #[test]
    fn window_keys_change_window_and_drop_buffer() {
        let mut state = fixture();
        let gen0 = state.registry.access_generation;
        assert!(handle(Action::SetWindowWeek, &mut state));
        assert_eq!(state.registry.access_window, AccessWindow::Week);
        assert!(state.registry.access_events.is_none());
        assert_eq!(state.registry.access_generation, gen0 + 1);
        // Same window again: no invalidation.
        let events = vec![event(
            1,
            "Pull",
            "x",
            CallerKind::Unknown,
            "1.2.3.4",
            "repo",
            None,
        )];
        state.registry.access_events = Some(events);
        assert!(handle(Action::SetWindowWeek, &mut state));
        assert!(state.registry.access_events.is_some());
    }

    #[test]
    fn custom_window_input_parses_and_applies() {
        let mut state = fixture();
        assert!(handle(Action::SetCustomWindow, &mut state));
        assert!(state.registry.access_window_input_active);
        state.registry.access_window_input = "6m".into();
        assert!(handle(Action::OpenSelected, &mut state));
        assert!(!state.registry.access_window_input_active);
        assert_eq!(state.registry.access_window.label(), "6m");
        assert!(
            state.registry.access_events.is_none(),
            "scope changed → refetch"
        );
    }

    #[test]
    fn exclude_self_toggle_invalidates_fetch() {
        let mut state = fixture();
        let gen0 = state.registry.access_generation;
        assert!(handle(Action::ToggleExcludeSelf, &mut state));
        assert!(state.registry.access_exclude_self);
        assert!(state.registry.access_events.is_none());
        assert_eq!(state.registry.access_generation, gen0 + 1);
    }

    #[test]
    fn tab_cycles_operation_filter_through_sorted_ops() {
        let mut state = fixture();
        // all → Pull → Push → all
        assert!(handle(Action::CycleSourceFilter, &mut state));
        assert_eq!(state.registry.access_operation.as_deref(), Some("Pull"));
        assert_eq!(state.registry.visible_access_events().len(), 2);
        assert!(handle(Action::CycleSourceFilter, &mut state));
        assert_eq!(state.registry.access_operation.as_deref(), Some("Push"));
        assert_eq!(state.registry.visible_access_events().len(), 1);
        assert!(handle(Action::CycleSourceFilter, &mut state));
        assert_eq!(state.registry.access_operation, None);
        assert_eq!(state.registry.visible_access_events().len(), 3);
    }

    #[test]
    fn back_returns_to_recorded_origin() {
        let mut state = fixture();
        state.registry.access_return_view = Some(View::RegistryRepositories);
        assert!(handle(Action::Back, &mut state));
        assert_eq!(state.view, View::RegistryRepositories);
        // Without a recorded origin, fall back to the registries list.
        state.view = View::RegistryAccessLogs;
        assert!(handle(Action::Back, &mut state));
        assert_eq!(state.view, View::Registries);
    }

    #[test]
    fn caller_renders_resolved_principal_name() {
        let state = {
            let mut s = fixture();
            s.principals.by_id.insert(
                "f3c9a2e1-0d4b-4f7e-9a1c-2b5d8e7f6a3c".to_string(),
                "sp-orders-deploy".to_string(),
            );
            s
        };
        let e = &state.registry.visible_access_events()[0].clone();
        assert_eq!(caller_display(&state, e), "sp-orders-deploy");
    }

    #[test]
    fn yank_includes_image_and_raw_identity() {
        let mut state = fixture();
        state.registry.access_events.as_mut().unwrap()[0].digest =
            Some("sha256:9f86d081884c7d659a2feaa0c55ad015".to_string());
        state.registry.access_cursor = 0;
        let y = yank_text(&state).unwrap();
        assert!(y.contains("Pull"));
        assert!(y.contains("ca-checkout-api:1.7.3"));
        assert!(y.contains("f3c9a2e1-0d4b-4f7e-9a1c-2b5d8e7f6a3c"));
        assert!(y.contains("sha256:9f86d081884c7d659a2feaa0c55ad015"));
    }

    #[test]
    fn renders_table_scope_and_hidden_chips() {
        let theme = crate::ui::theme::Theme::catppuccin_mocha();
        let backend = TestBackend::new(170, 20);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.registry.access_scope = Some("ca-checkout-api".to_string());
        state.registry.access_exclude_self = true;
        state.registry.access_hidden = Some(crate::azure::demo::self_identity());
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("dana@contoso.com"), "caller column missing");
        assert!(s.contains("ca-checkout-api ✓"), "scope chip missing");
        assert!(
            s.contains("hiding robbert@contoso.com"),
            "hidden chip missing"
        );
        assert!(s.contains("IMAGE"), "header missing");
    }

    #[test]
    fn renders_empty_state_with_explicit_forwarding_warning() {
        let theme = crate::ui::theme::Theme::catppuccin_mocha();
        let backend = TestBackend::new(120, 16);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.registry.access_events = Some(Vec::new());
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("not forwarding its repository events"));
        assert!(s.contains("NOT record pull/push logs by default"));
        assert!(s.contains("ContainerRegistryRepositoryEvents"));
    }

    #[test]
    fn empty_state_cites_metric_counts_when_activity_exists() {
        let theme = crate::ui::theme::Theme::catppuccin_mocha();
        let backend = TestBackend::new(140, 26);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.registry.access_events = Some(Vec::new());
        state.registry.access_metrics =
            Some(crate::azure::demo::registry_activity(&AccessWindow::Day));
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(
            s.contains("images ARE being pulled"),
            "metrics corroboration line missing"
        );
    }

    #[test]
    fn renders_activity_chart_rows_above_table() {
        let theme = crate::ui::theme::Theme::catppuccin_mocha();
        let backend = TestBackend::new(140, 26);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.registry.access_metrics =
            Some(crate::azure::demo::registry_activity(&AccessWindow::Day));
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("Pulls"), "pulls chart row missing");
        assert!(s.contains("Pushes"), "pushes chart row missing");
        assert!(s.contains("total:"), "chart summary missing");
        assert!(s.contains("IMAGE"), "event table must still render");
    }

    #[test]
    fn chart_summary_flags_registry_scope_when_table_is_repo_scoped() {
        let theme = crate::ui::theme::Theme::catppuccin_mocha();
        let backend = TestBackend::new(140, 26);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.registry.access_scope = Some("ca-checkout-api".to_string());
        state.registry.access_metrics =
            Some(crate::azure::demo::registry_activity(&AccessWindow::Day));
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(
            s.contains("whole registry"),
            "repo-scoped view must flag that the chart is registry-wide"
        );
    }

    #[test]
    fn chart_is_skipped_on_short_terminals() {
        let theme = crate::ui::theme::Theme::catppuccin_mocha();
        let backend = TestBackend::new(140, 12);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.registry.access_metrics =
            Some(crate::azure::demo::registry_activity(&AccessWindow::Day));
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(!s.contains("Pushes"), "chart must yield to the table");
        assert!(s.contains("IMAGE"), "event table must render");
    }

    #[test]
    fn table_missing_error_gets_forwarding_warning() {
        let theme = crate::ui::theme::Theme::catppuccin_mocha();
        let backend = TestBackend::new(120, 16);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.registry.access_events = None;
        state.registry.access_error = Some(
            "azure api error 400: Failed to resolve table or column expression named 'ContainerRegistryRepositoryEvents'".to_string(),
        );
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("error:"));
        assert!(s.contains("not forwarding its repository events"));
    }
}
