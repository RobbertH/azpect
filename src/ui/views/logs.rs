//! Logs view: scrollable table of recent log lines for the selected resource,
//! with an errors-only toggle and the same `d/w` window control as the detail view.

#![allow(dead_code, unused_variables)]

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, Wrap};
use ratatui::Frame;

use crate::azure::logs::{LogLevel, LogLine};
use crate::azure::metrics::TimeRange;
use crate::azure::resources::ResourceKind;
use crate::ui::events::Action;
#[cfg(test)]
use crate::ui::state::View;
use crate::ui::state::AppState;
use crate::ui::theme::Theme;

const FOOTER_HINT: &str = "j/k scroll  y yank  e errors-only  d 1d  w 7d  Esc back  q quit";
const HALF_PAGE: usize = 10;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(area);

    let selected = state.selected_resource();

    // Header
    let mut header_spans = vec![Span::styled(
        " logs ",
        Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
    )];
    if let Some(r) = selected {
        header_spans.push(Span::styled(&r.name, Style::default().fg(theme.fg)));
        header_spans.push(Span::styled(
            format!(" ({}) ", r.kind.short_tag()),
            Style::default().fg(theme.muted),
        ));
        header_spans.push(Span::styled(
            format!("· {} ", state.logs.range.label()),
            Style::default().fg(theme.muted),
        ));
        if state.logs.errors_only {
            header_spans.push(Span::styled(
                "· filter: errors only ✓ ",
                Style::default().fg(theme.degraded),
            ));
        }
    } else {
        header_spans.push(Span::styled(
            "(no selection)",
            Style::default().fg(theme.muted),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(header_spans)), chunks[0]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(" recent ", Style::default().fg(theme.fg)));
    let inner = block.inner(chunks[1]);
    frame.render_widget(block, chunks[1]);

    let Some(resource) = selected else {
        center_message(frame, inner, "no resource selected.", theme.muted);
        render_footer(frame, chunks[2], theme);
        return;
    };

    if matches!(resource.kind, ResourceKind::Apim) {
        center_message(
            frame,
            inner,
            "Logs are not supported for APIM in v1.",
            theme.muted,
        );
        render_footer(frame, chunks[2], theme);
        return;
    }

    let lines = state.logs.by_resource.get(&resource.id);

    if state.logs.loading && lines.map(|l| l.is_empty()).unwrap_or(true) {
        center_message(frame, inner, "Loading logs…", theme.muted);
        render_footer(frame, chunks[2], theme);
        return;
    }

    if let Some(err) = state.logs.last_error.as_deref() {
        if lines.map(|l| l.is_empty()).unwrap_or(true) {
            let msg = friendly_log_error(err);
            render_error_message(frame, inner, &msg, theme.degraded);
            render_footer(frame, chunks[2], theme);
            return;
        }
    }

    let lines = lines.map(|v| v.as_slice()).unwrap_or(&[]);
    if lines.is_empty() {
        center_message(frame, inner, "no log lines in window.", theme.muted);
        render_footer(frame, chunks[2], theme);
        return;
    }

    render_table(frame, inner, lines, state, theme);
    render_footer(frame, chunks[2], theme);
}

fn render_footer(frame: &mut Frame, area: Rect, theme: &Theme) {
    let p = Paragraph::new(Line::from(Span::styled(
        FOOTER_HINT,
        Style::default().fg(theme.muted),
    )));
    frame.render_widget(p, area);
}

fn render_table(
    frame: &mut Frame,
    area: Rect,
    lines: &[LogLine],
    state: &AppState,
    theme: &Theme,
) {
    let visible = area.height as usize;
    let scroll = scroll_for(state.logs.scroll, lines.len(), visible);
    let cursor = state.logs.scroll.min(lines.len().saturating_sub(1));

    let rows: Vec<Row> = lines
        .iter()
        .enumerate()
        .skip(scroll)
        .take(visible.max(1))
        .map(|(i, l)| {
            let selected = i == cursor;
            let ts = l.ts.format("%H:%M:%S").to_string();
            let (lvl_text, lvl_color) = level_cell(l, theme);
            let row = Row::new(vec![
                Cell::from(Span::styled(ts, Style::default().fg(theme.muted))),
                Cell::from(Span::styled(lvl_text, Style::default().fg(lvl_color))),
                Cell::from(Span::styled(
                    l.source.clone(),
                    Style::default().fg(theme.accent),
                )),
                Cell::from(Span::styled(l.message.clone(), Style::default().fg(theme.fg))),
            ]);
            if selected {
                row.style(theme.selection())
            } else {
                row
            }
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(8),
            Constraint::Length(5),
            Constraint::Length(22),
            Constraint::Min(20),
        ],
    )
    .header(
        Row::new(vec!["time", "lvl", "source", "message"])
            .style(Style::default().fg(theme.muted).add_modifier(Modifier::BOLD)),
    )
    .column_spacing(2);
    frame.render_widget(table, area);
}

fn level_cell(line: &LogLine, theme: &Theme) -> (String, Color) {
    // For Function App requests, the source typically encodes a status; if the
    // message starts with a 3-digit status code we surface that instead of the
    // generic level.
    if line.source.eq_ignore_ascii_case("AppRequests") {
        if let Some(code) = extract_status(&line.message) {
            let color = match code {
                100..=299 => theme.healthy,
                300..=399 => theme.muted,
                400..=499 => theme.degraded,
                500..=599 => theme.critical,
                _ => theme.fg,
            };
            return (format!("{code}"), color);
        }
    }
    match line.level {
        LogLevel::Error => ("ERR".into(), theme.critical),
        LogLevel::Warn => ("WRN".into(), theme.degraded),
        LogLevel::Info => ("INF".into(), theme.fg),
        LogLevel::Trace => ("TRC".into(), theme.muted),
    }
}

fn extract_status(msg: &str) -> Option<u16> {
    let trimmed = msg.trim_start();
    let head: String = trimmed.chars().take(3).collect();
    if head.len() == 3 && head.chars().all(|c| c.is_ascii_digit()) {
        head.parse().ok()
    } else {
        None
    }
}

fn center_message(frame: &mut Frame, area: Rect, msg: &str, color: Color) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let mid_y = area.y + area.height / 2;
    let target = Rect {
        x: area.x,
        y: mid_y,
        width: area.width,
        height: 1,
    };
    let p = Paragraph::new(Line::from(Span::styled(msg, Style::default().fg(color))))
        .alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(p, target);
}

/// Render a (possibly long) error string vertically centered in `area`, with
/// word wrapping so the user can read past the right margin.
fn render_error_message(frame: &mut Frame, area: Rect, msg: &str, color: Color) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    // Estimate wrapped line count from char width. Over-counts on ASCII-heavy
    // text (which is fine — we just take a slightly taller block). Leaves a
    // ~25% horizontal margin so the text doesn't run flush to the borders.
    let usable_width = area.width.saturating_sub(area.width / 8).max(20) as usize;
    let est_lines = msg.chars().count().div_ceil(usable_width).max(1);
    let block_h = (est_lines as u16).min(area.height);
    let top_pad = (area.height.saturating_sub(block_h)) / 2;
    let target = Rect {
        x: area.x,
        y: area.y + top_pad,
        width: area.width,
        height: block_h,
    };
    let p = Paragraph::new(msg.to_string())
        .style(Style::default().fg(color))
        .alignment(ratatui::layout::Alignment::Center)
        .wrap(Wrap { trim: false });
    frame.render_widget(p, target);
}

/// Turn a raw Azure / Log Analytics error string into something readable.
///
/// The transport prefixes errors like `azure api error 400: { ...JSON... }`.
/// Log Analytics nests its real diagnostic inside an `innererror` chain whose
/// deepest entry names the actual problem (missing column, malformed timespan,
/// etc.) — so we walk the chain and surface the deepest message rather than
/// the generic outer one. Also folds the well-known "no diagnostic settings"
/// variants into a single actionable hint.
pub fn friendly_log_error(raw: &str) -> String {
    let body = raw.trim();
    let lowered = body.to_lowercase();

    let no_destination = lowered.contains("nologdestination")
        || lowered.contains("no log destination")
        || lowered.contains("pathnotfounderror")
        || lowered.contains("workspace not found")
        || lowered.contains("diagnostic settings");

    if no_destination {
        return "No diagnostic settings configured for this resource. \
                Forward logs to a Log Analytics workspace to see them here."
            .to_string();
    }

    if let Some(start) = body.find('{') {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body[start..]) {
            if let Some(err) = v.get("error") {
                let (message, code) = deepest_error(err);
                if let Some(message) = message {
                    return match code {
                        Some(c) => format!("{message} ({c})"),
                        None => message,
                    };
                }
            }
        }
    }

    body.to_string()
}

/// Walk the `innererror` chain and return the deepest available
/// `(message, code)`. Falls back to the outer level if no inner entry has
/// a message.
fn deepest_error(err: &serde_json::Value) -> (Option<String>, Option<String>) {
    let mut current = err;
    let mut best_message = current.get("message").and_then(|m| m.as_str()).map(str::to_owned);
    let mut best_code = current.get("code").and_then(|c| c.as_str()).map(str::to_owned);
    while let Some(inner) = current.get("innererror").or_else(|| current.get("innerError")) {
        if let Some(msg) = inner.get("message").and_then(|m| m.as_str()) {
            best_message = Some(msg.to_string());
        }
        if let Some(code) = inner.get("code").and_then(|c| c.as_str()) {
            best_code = Some(code.to_string());
        }
        current = inner;
    }
    (best_message, best_code)
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

pub fn handle(action: Action, state: &mut AppState) -> bool {
    let lines_len = state
        .selected_resource()
        .and_then(|r| state.logs.by_resource.get(&r.id))
        .map(|v| v.len())
        .unwrap_or(0);

    match action {
        Action::ToggleErrorsOnly => {
            state.logs.errors_only = !state.logs.errors_only;
            if let Some(id) = state.selected_resource().map(|r| r.id.clone()) {
                state.logs.by_resource.remove(&id);
            }
            state.logs.scroll = 0;
            true
        }
        Action::SetWindowDay => set_window(state, TimeRange::Day),
        Action::SetWindowWeek => set_window(state, TimeRange::Week),
        Action::MoveDown => {
            if lines_len > 0 {
                state.logs.scroll = (state.logs.scroll + 1).min(lines_len - 1);
            }
            true
        }
        Action::MoveUp => {
            state.logs.scroll = state.logs.scroll.saturating_sub(1);
            true
        }
        Action::HalfPageDown => {
            if lines_len > 0 {
                state.logs.scroll = (state.logs.scroll + HALF_PAGE).min(lines_len - 1);
            }
            true
        }
        Action::HalfPageUp => {
            state.logs.scroll = state.logs.scroll.saturating_sub(HALF_PAGE);
            true
        }
        Action::GotoTop => {
            state.logs.scroll = 0;
            true
        }
        Action::GotoBottom => {
            if lines_len > 0 {
                state.logs.scroll = lines_len - 1;
            }
            true
        }
        _ => false,
    }
}

fn set_window(state: &mut AppState, range: TimeRange) -> bool {
    if state.logs.range == range {
        return true;
    }
    state.logs.range = range;
    if let Some(id) = state.selected_resource().map(|r| r.id.clone()) {
        state.logs.by_resource.remove(&id);
    }
    state.logs.scroll = 0;
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::logs::{LogLevel, LogLine};
    use crate::azure::resources::{Resource, ResourceKind};
    use crate::config::Config;
    use chrono::{Duration, Utc};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn r(kind: ResourceKind) -> Resource {
        Resource {
            id: "/r/one".into(),
            name: "alpha-func".into(),
            kind,
            location: "westeurope".into(),
            resource_group: "rg-demo".into(),
            subscription_id: "sub".into(),
            state: Some("Running".into()),
        }
    }

    fn line(off: i64, level: LogLevel, source: &str, msg: &str) -> LogLine {
        LogLine {
            ts: Utc::now() - Duration::minutes(off),
            level,
            source: source.into(),
            message: msg.into(),
        }
    }

    fn fixture(kind: ResourceKind) -> AppState {
        let mut s = AppState::new(Config::default());
        s.resources = vec![r(kind)];
        s.list_cursor = 0;
        s.view = View::Logs;
        s
    }

    #[test]
    fn renders_apim_unsupported() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(80, 12);
        let mut term = Terminal::new(backend).unwrap();
        let state = fixture(ResourceKind::Apim);
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("APIM"));
    }

    #[test]
    fn renders_loading() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(80, 12);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture(ResourceKind::FunctionApp);
        state.logs.loading = true;
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.to_lowercase().contains("loading"));
    }

    #[test]
    fn renders_no_destination_error() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(120, 12);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture(ResourceKind::FunctionApp);
        state.logs.last_error = Some("NoLogDestination".into());
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.to_lowercase().contains("diagnostic"));
    }

    #[test]
    fn friendly_log_error_extracts_message_and_code() {
        let raw = r#"azure api error 400: {"error":{"message":"The request had some invalid properties","code":"BadArgumentError","correlationId":"abc"}}"#;
        let out = friendly_log_error(raw);
        assert!(out.contains("The request had some invalid properties"));
        assert!(out.contains("BadArgumentError"));
        assert!(!out.contains("correlationId"));
    }

    #[test]
    fn friendly_log_error_walks_innererror_chain_for_real_diagnostic() {
        // Real Log Analytics shape: outer "BadArgumentError" is generic; the
        // useful detail lives in the deepest innererror.message.
        let raw = r#"azure api error 400: {"error":{"message":"The request had some invalid properties","code":"BadArgumentError","innererror":{"code":"SyntaxError","message":"A recognition error occurred","innererror":{"code":"SEM0100","message":"'where' operator: Failed to resolve column or table 'Success'"}}}}"#;
        let out = friendly_log_error(raw);
        assert!(
            out.contains("Failed to resolve column or table 'Success'"),
            "expected deepest message, got {out:?}",
        );
        assert!(out.contains("SEM0100"), "expected deepest code, got {out:?}");
    }

    #[test]
    fn friendly_log_error_collapses_no_destination_variants() {
        for variant in [
            "NoLogDestination",
            "azure api error 404: {\"error\":{\"code\":\"PathNotFoundError\"}}",
            "diagnostic settings missing",
        ] {
            let out = friendly_log_error(variant);
            assert!(
                out.to_lowercase().contains("diagnostic"),
                "expected destination hint for {variant:?}, got {out:?}"
            );
        }
    }

    #[test]
    fn friendly_log_error_passes_through_plain_text() {
        let out = friendly_log_error("connection reset by peer");
        assert_eq!(out, "connection reset by peer");
    }

    #[test]
    fn renders_lines() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(120, 16);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture(ResourceKind::FunctionApp);
        state.logs.by_resource.insert(
            "/r/one".into(),
            vec![
                line(1, LogLevel::Info, "AppTraces", "started"),
                line(2, LogLevel::Error, "AppExceptions", "boom"),
                line(3, LogLevel::Info, "AppRequests", "200 GET /healthz"),
            ],
        );
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("AppExceptions"));
        assert!(s.contains("started"));
        assert!(s.contains("200"));
    }

    #[test]
    fn toggle_errors_only_clears_cache() {
        let mut state = fixture(ResourceKind::FunctionApp);
        state
            .logs
            .by_resource
            .insert("/r/one".into(), vec![line(1, LogLevel::Info, "x", "y")]);
        assert!(handle(Action::ToggleErrorsOnly, &mut state));
        assert!(state.logs.errors_only);
        assert!(!state.logs.by_resource.contains_key("/r/one"));
    }

    #[test]
    fn back_is_not_consumed_by_view() {
        // Logs view must NOT consume Action::Back — it falls through to the
        // global handler which pops the view_stack. Consuming it here would
        // re-introduce bug_009: an explicit "back to Detail" arm overwrites
        // any older breadcrumb on the stack.
        let mut state = fixture(ResourceKind::FunctionApp);
        assert!(!handle(Action::Back, &mut state));
        assert_eq!(state.view, View::Logs, "view-local handler must not transition on Back");
    }

    #[test]
    fn move_down_clamps() {
        let mut state = fixture(ResourceKind::FunctionApp);
        state.logs.by_resource.insert(
            "/r/one".into(),
            vec![
                line(1, LogLevel::Info, "x", "a"),
                line(2, LogLevel::Info, "x", "b"),
            ],
        );
        assert!(handle(Action::MoveDown, &mut state));
        assert_eq!(state.logs.scroll, 1);
        assert!(handle(Action::MoveDown, &mut state));
        assert_eq!(state.logs.scroll, 1);
    }

    #[test]
    fn extract_status_works() {
        assert_eq!(extract_status("200 OK"), Some(200));
        assert_eq!(extract_status(" 404 Not Found"), Some(404));
        assert_eq!(extract_status("hello"), None);
    }
}
