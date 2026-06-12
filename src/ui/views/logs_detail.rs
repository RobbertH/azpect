//! Full-screen detail panel for a single log line. Opened with Enter from the
//! logs table; reads `LogsCache::scroll` to pick the line and `detail_scroll`
//! for its own j/k navigation.

#![allow(dead_code, unused_variables)]

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::azure::logs::LogLine;
use crate::ui::events::Action;
use crate::ui::state::AppState;
use crate::ui::theme::Theme;

const FOOTER_HINT: &str = "j/k scroll  y yank  o open in portal  Esc back  q quit";
const HALF_PAGE: u16 = 10;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(area);

    let title_spans = vec![
        Span::styled(
            " log line ",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            state
                .selected_resource()
                .map(|r| r.name.as_str())
                .unwrap_or_default(),
            Style::default().fg(theme.fg),
        ),
    ];
    frame.render_widget(Paragraph::new(Line::from(title_spans)), chunks[0]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(" detail ", Style::default().fg(theme.fg)));
    let inner = block.inner(chunks[1]);
    frame.render_widget(block, chunks[1]);

    if let Some(line) = selected_line(state) {
        let body = build_body(line, theme);
        // Clamp scroll to keep the last logical row aligned with the bottom of
        // the panel. ratatui 0.29's Paragraph overflows when `scroll.0 +
        // area.height` exceeds u16::MAX (paragraph.rs:483), so we must cap
        // detail_scroll at a sane value before passing it in. `body.len()` is
        // an unwrapped lower bound on wrapped line count; using it as the
        // ceiling lets the user scroll a little past the content but never
        // anywhere near u16::MAX.
        let total = body.len() as u16;
        let max_scroll = total.saturating_sub(inner.height.max(1).saturating_sub(1));
        let scroll = state.logs.detail_scroll.min(max_scroll);
        let p = Paragraph::new(body)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0));
        frame.render_widget(p, inner);
    } else if inner.height > 0 {
        let p = Paragraph::new(Line::from(Span::styled(
            "no log line selected.",
            Style::default().fg(theme.muted),
        )))
        .alignment(ratatui::layout::Alignment::Center);
        frame.render_widget(p, inner);
    }

    let footer = Paragraph::new(Line::from(Span::styled(
        FOOTER_HINT,
        Style::default().fg(theme.muted),
    )));
    frame.render_widget(footer, chunks[2]);
}

/// Resolve the currently-selected log line from `state.logs.scroll`. Goes
/// through `visible_log_lines` because the scroll index is relative to the
/// source-filtered view of the cache, not the raw buffer.
pub fn selected_line(state: &AppState) -> Option<&LogLine> {
    let resource = state.selected_resource()?;
    let lines = state.visible_log_lines(&resource.id);
    if lines.is_empty() {
        return None;
    }
    let idx = state.logs.scroll.min(lines.len() - 1);
    lines.get(idx).copied()
}

fn build_body(line: &LogLine, theme: &Theme) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();

    let ts_local = line
        .ts
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();

    out.push(kv("time", &ts_local, theme));
    out.push(kv("level", &format!("{:?}", line.level), theme));
    out.push(kv("source", &line.source, theme));
    out.push(Line::from(""));

    out.push(section_header("message", theme));
    for l in line.message.split('\n') {
        out.push(Line::from(Span::styled(
            l.to_string(),
            Style::default().fg(theme.fg),
        )));
    }
    out.push(Line::from(""));

    if !line.fields.is_empty() {
        out.push(section_header("fields", theme));
        // Compute column width for label alignment, capped so very long names
        // don't push the values off-screen.
        let label_w = line
            .fields
            .iter()
            .map(|(k, _)| k.chars().count())
            .max()
            .unwrap_or(0)
            .min(28);
        for (k, v) in &line.fields {
            push_field(&mut out, k, v, label_w, theme);
        }
    }

    out
}

fn section_header(text: &str, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
    ))
}

fn kv(key: &str, value: &str, theme: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{:<8} ", format!("{key}:")),
            Style::default().fg(theme.muted),
        ),
        Span::styled(value.to_string(), Style::default().fg(theme.fg)),
    ])
}

/// Push one field. If the value contains newlines, render the key on its own
/// line and indent each value line so the structure stays readable in the
/// wrapped paragraph.
fn push_field(out: &mut Vec<Line<'static>>, key: &str, value: &str, label_w: usize, theme: &Theme) {
    let is_multiline = value.contains('\n');
    if is_multiline {
        out.push(Line::from(Span::styled(
            format!("{key}:"),
            Style::default()
                .fg(theme.muted)
                .add_modifier(Modifier::BOLD),
        )));
        for l in value.split('\n') {
            out.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(l.to_string(), Style::default().fg(theme.fg)),
            ]));
        }
    } else {
        out.push(Line::from(vec![
            Span::styled(
                format!("{:<w$} ", format!("{key}:"), w = label_w + 1),
                Style::default().fg(theme.muted),
            ),
            Span::styled(value.to_string(), Style::default().fg(theme.fg)),
        ]));
    }
}

/// Build the plain-text yank payload for the displayed log line: a
/// reproducible `key: value` dump that includes the canonical fields and
/// every column we kept.
pub fn yank_text(line: &LogLine) -> String {
    let mut s = String::new();
    s.push_str(&format!("time: {}\n", line.ts.format("%Y-%m-%dT%H:%M:%SZ")));
    s.push_str(&format!("level: {:?}\n", line.level));
    s.push_str(&format!("source: {}\n", line.source));
    s.push_str(&format!("message: {}\n", line.message));
    for (k, v) in &line.fields {
        s.push_str(&format!("{k}: {v}\n"));
    }
    s
}

/// Sentinel "go to bottom" value. Must be small enough that
/// `scroll + terminal_height` does not overflow u16 (ratatui 0.29's Paragraph
/// adds them without saturation), and large enough to exceed any realistic
/// log-detail body. A log row's body is at most a few hundred lines even with
/// long stack traces, so 10_000 leaves comfortable headroom on both sides.
const GOTO_BOTTOM_SENTINEL: u16 = 10_000;

pub fn handle(action: Action, state: &mut AppState) -> bool {
    match action {
        Action::MoveDown => {
            state.logs.detail_scroll = state.logs.detail_scroll.saturating_add(1);
            true
        }
        Action::MoveUp => {
            state.logs.detail_scroll = state.logs.detail_scroll.saturating_sub(1);
            true
        }
        Action::HalfPageDown => {
            state.logs.detail_scroll = state.logs.detail_scroll.saturating_add(HALF_PAGE);
            true
        }
        Action::HalfPageUp => {
            state.logs.detail_scroll = state.logs.detail_scroll.saturating_sub(HALF_PAGE);
            true
        }
        Action::GotoTop => {
            state.logs.detail_scroll = 0;
            true
        }
        Action::GotoBottom => {
            // Render clamps this to the actual content height so the bottom
            // row aligns with the bottom of the panel.
            state.logs.detail_scroll = GOTO_BOTTOM_SENTINEL;
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::logs::LogLevel;
    use crate::azure::resources::{Resource, ResourceKind};
    use crate::config::Config;
    use crate::ui::state::View;
    use chrono::{TimeZone, Utc};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn fixture() -> AppState {
        let mut s = AppState::new(Config::default());
        s.resources = vec![Resource {
            id: "/r/one".into(),
            name: "alpha-func".into(),
            kind: ResourceKind::FunctionApp,
            location: "westeurope".into(),
            resource_group: "rg-demo".into(),
            subscription_id: "sub".into(),
            state: Some("Running".into()),
            created_at: None,
            modified_at: None,
            meta: Default::default(),
        }];
        s.list_cursor = 0;
        s.view = View::LogDetail;
        s
    }

    fn line() -> LogLine {
        LogLine {
            ts: Utc.with_ymd_and_hms(2026, 5, 10, 22, 32, 51).unwrap(),
            level: LogLevel::Error,
            source: "FunctionAppLogs/http_app_func".into(),
            message: "Executed Functions.http_app_func (Failed, Id=abc, Duration=77261ms)".into(),
            fields: vec![
                ("FunctionInvocationId".into(), "abc-123".into()),
                ("OperationId".into(), "op-456".into()),
                (
                    "ExceptionDetails".into(),
                    "stack line 1\nstack line 2".into(),
                ),
            ],
        }
    }

    #[test]
    fn renders_message_and_fields() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(100, 30);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.logs.by_resource.insert("/r/one".into(), vec![line()]);

        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("Executed Functions.http_app_func"));
        assert!(s.contains("FunctionInvocationId"));
        assert!(s.contains("op-456"));
        assert!(s.contains("stack line 2"));
    }

    #[test]
    fn renders_empty_state_when_no_lines() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(100, 20);
        let mut term = Terminal::new(backend).unwrap();
        let state = fixture();
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.to_lowercase().contains("no log line"));
    }

    #[test]
    fn yank_text_includes_canonical_and_extra_fields() {
        let t = yank_text(&line());
        assert!(t.contains("level: Error"));
        assert!(t.contains("source: FunctionAppLogs/http_app_func"));
        assert!(t.contains("FunctionInvocationId: abc-123"));
        assert!(t.contains("OperationId: op-456"));
    }

    #[test]
    fn render_survives_goto_bottom_sentinel() {
        // Regression: capital G used to set detail_scroll to u16::MAX, which
        // overflowed ratatui 0.29's Paragraph (paragraph.rs:483 does scroll +
        // area.height without saturation) and crashed the binary. The render
        // path must clamp scroll to the content length.
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(100, 20);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.logs.by_resource.insert("/r/one".into(), vec![line()]);
        // Both the sentinel value and the historical overflow value must be
        // safe to render.
        for s in [GOTO_BOTTOM_SENTINEL, u16::MAX] {
            state.logs.detail_scroll = s;
            term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        }
    }

    #[test]
    fn handle_scrolls() {
        let mut s = fixture();
        assert!(handle(Action::MoveDown, &mut s));
        assert_eq!(s.logs.detail_scroll, 1);
        assert!(handle(Action::HalfPageDown, &mut s));
        assert_eq!(s.logs.detail_scroll, 1 + HALF_PAGE);
        assert!(handle(Action::GotoTop, &mut s));
        assert_eq!(s.logs.detail_scroll, 0);
        assert!(handle(Action::MoveUp, &mut s));
        assert_eq!(s.logs.detail_scroll, 0); // saturates at 0
    }
}
