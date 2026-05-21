//! Logs view: scrollable table of recent log lines for the selected resource,
//! with an errors-only toggle, a wrap toggle, and the same `1/7` window
//! control as the detail view.

#![allow(dead_code, unused_variables)]

use chrono::Offset;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, Wrap};
use ratatui::Frame;

use crate::azure::logs::{LogLevel, LogLine};
use crate::azure::metrics::TimeRange;
use crate::ui::events::Action;
use crate::ui::state::AppState;
#[cfg(test)]
use crate::ui::state::View;
use crate::ui::theme::Theme;

const FOOTER_HINT: &str =
    "j/k scroll  h/l ← →  Enter detail  / search  n/N next/prev match  y yank  e errors-only  w wrap  r refresh  0 1h  1 1d  7 7d  Esc back  q quit";
const FOOTER_HINT_SEARCH: &str = "type to search  Enter jump  Esc cancel";
const HALF_PAGE: usize = 10;
const H_SCROLL_STEP: usize = 8;
/// Minimum visible characters that should remain on the longest line at the
/// rightmost scroll position. Conservative lower bound on the message column's
/// actual width — the layout uses `Constraint::Min(20)` for that column.
const H_SCROLL_MIN_VISIBLE: usize = 20;
const TIME_COL: u16 = 19;
const LVL_COL: u16 = 5;
const SOURCE_COL: u16 = 32;
const COLUMN_SPACING: u16 = 2;

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
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
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
        // Buffer status: row count, plus a hint about whether more older rows
        // are reachable. Lets the user see why G stops where it does, and
        // confirms when scroll-to-load has actually pulled fresh pages in.
        let cached = state
            .logs
            .by_resource
            .get(&r.id)
            .map(|v| v.len())
            .unwrap_or(0);
        if cached > 0 {
            let more = state
                .logs
                .more_available
                .get(&r.id)
                .copied()
                .unwrap_or(false);
            header_spans.push(Span::styled(
                format!("· {cached} rows "),
                Style::default().fg(theme.muted),
            ));
            if state.logs.loading_more {
                header_spans.push(Span::styled(
                    "· loading more… ",
                    Style::default().fg(theme.muted),
                ));
            } else if more {
                header_spans.push(Span::styled(
                    "· more on ↓ ",
                    Style::default().fg(theme.muted),
                ));
            } else {
                header_spans.push(Span::styled(
                    "· window complete ",
                    Style::default().fg(theme.muted),
                ));
            }
        }
        if state.logs.errors_only {
            header_spans.push(Span::styled(
                "· filter: errors only ✓ ",
                Style::default().fg(theme.degraded),
            ));
        }
        if state.logs.search_active || !state.logs.search_input.value().is_empty() {
            header_spans.push(Span::styled(
                format!("· /{} ", state.logs.search_input.value()),
                Style::default().fg(theme.fg),
            ));
        }
        // Reload-in-flight indicator: only useful when we already have lines
        // showing (otherwise the body's "Loading logs…" already conveys it).
        if state.logs.loading
            && state
                .logs
                .by_resource
                .get(&r.id)
                .is_some_and(|lines| !lines.is_empty())
        {
            header_spans.push(Span::styled(
                "· refreshing… ",
                Style::default().fg(theme.muted),
            ));
        }
    } else {
        header_spans.push(Span::styled(
            "(no selection)",
            Style::default().fg(theme.muted),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(header_spans)), chunks[0]);

    let title = if state.logs.search_active {
        " recent (search) "
    } else {
        " recent "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(title, Style::default().fg(theme.fg)));
    let inner = block.inner(chunks[1]);
    frame.render_widget(block, chunks[1]);

    // When the search input has focus, peel a single row off the top of the
    // bordered area for the live query — the table renders below it. The box
    // disappears on Esc/Enter; the committed query stays in the header chip.
    let (search_area, body) = if state.logs.search_active {
        let parts = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(inner);
        (Some(parts[0]), parts[1])
    } else {
        (None, inner)
    };
    if let Some(sa) = search_area {
        let p = Paragraph::new(Line::from(vec![
            Span::styled("/", Style::default().fg(theme.accent)),
            Span::styled(
                state.logs.search_input.value().to_string(),
                Style::default().fg(theme.fg),
            ),
            Span::styled("█", Style::default().fg(theme.accent)),
        ]));
        frame.render_widget(p, sa);
    }

    let footer_text = if state.logs.search_active {
        FOOTER_HINT_SEARCH
    } else {
        FOOTER_HINT
    };

    let Some(resource) = selected else {
        center_message(frame, body, "no resource selected.", theme.muted);
        render_footer(frame, chunks[2], theme, footer_text);
        return;
    };

    if !crate::azure::logs::supports_logs(resource.kind) {
        let msg = format!(
            "Logs are not supported for {} in v1.",
            resource.kind.short_tag()
        );
        center_message(frame, body, &msg, theme.muted);
        render_footer(frame, chunks[2], theme, footer_text);
        return;
    }

    let lines = state.logs.by_resource.get(&resource.id);

    if state.logs.loading && lines.map(|l| l.is_empty()).unwrap_or(true) {
        center_message(frame, body, "Loading logs…", theme.muted);
        render_footer(frame, chunks[2], theme, footer_text);
        return;
    }

    if let Some(err) = state.logs.last_error.as_deref() {
        if lines.map(|l| l.is_empty()).unwrap_or(true) {
            let msg = friendly_log_error(err);
            render_error_message(frame, body, &msg, theme.degraded);
            render_footer(frame, chunks[2], theme, footer_text);
            return;
        }
    }

    let lines = lines.map(|v| v.as_slice()).unwrap_or(&[]);
    if lines.is_empty() {
        center_message(frame, body, "no log lines in window.", theme.muted);
        render_footer(frame, chunks[2], theme, footer_text);
        return;
    }

    render_table(frame, body, lines, state, theme);
    render_footer(frame, chunks[2], theme, footer_text);
}

fn render_footer(frame: &mut Frame, area: Rect, theme: &Theme, text: &str) {
    let p = Paragraph::new(Line::from(Span::styled(
        text.to_string(),
        Style::default().fg(theme.muted),
    )));
    frame.render_widget(p, area);
}

fn render_table(frame: &mut Frame, area: Rect, lines: &[LogLine], state: &AppState, theme: &Theme) {
    let wrap = state.logs.wrap;
    let cursor = state.logs.scroll.min(lines.len().saturating_sub(1));
    let query = state.logs.search_input.value();
    let hi_style = Style::default()
        .bg(theme.favorite)
        .fg(Color::Black)
        .add_modifier(Modifier::BOLD);

    // Header is rendered above the data rows by the Table widget, so the
    // available data rows occupy `area.height - 1` cells of vertical space.
    let data_height = (area.height as usize).saturating_sub(1).max(1);

    // Width available for the message column after fixed columns and spacing.
    let used = TIME_COL + LVL_COL + SOURCE_COL + 3 * COLUMN_SPACING;
    let msg_w = (area.width.saturating_sub(used)).max(20) as usize;
    let source_w = SOURCE_COL as usize;

    // Pick the slice of rows to render. In non-wrap mode every row is height 1
    // so this collapses to "as many rows as fit". In wrap mode each row may
    // span multiple cells; visible_range walks both directions from the cursor
    // until the cumulative height fills the area.
    let (start, end) = visible_range(lines, cursor, data_height, wrap, source_w, msg_w);

    let rows: Vec<Row> = lines[start..=end]
        .iter()
        .enumerate()
        .map(|(off, l)| {
            let i = start + off;
            let selected = i == cursor;
            let ts =
                l.ts.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string();
            let (lvl_text, lvl_color) = level_cell(l, theme);
            // Horizontal scroll is non-wrap-only and applies *only* to the
            // message column. Source values are short (usually a table /
            // signal name) and the column is fixed-width 32, so scrolling
            // it would just hide useful information behind a `…`.
            let h = if wrap { 0 } else { state.logs.h_offset };
            let message_view = apply_h_offset(&l.message, h);
            let (source_text, source_lines) =
                cell_text(&l.source, query, source_w, wrap, theme.accent, hi_style);
            let (message_text, message_lines) =
                cell_text(&message_view, query, msg_w, wrap, theme.fg, hi_style);
            let row_h = source_lines.max(message_lines).max(1) as u16;

            let row = Row::new(vec![
                Cell::from(Span::styled(ts, Style::default().fg(theme.muted))),
                Cell::from(Span::styled(lvl_text, Style::default().fg(lvl_color))),
                Cell::from(source_text),
                Cell::from(message_text),
            ])
            .height(row_h);

            if selected {
                row.style(theme.selection())
            } else {
                row
            }
        })
        .collect();

    let time_header = format!("time ({})", local_tz_label());
    let table = Table::new(
        rows,
        [
            Constraint::Length(TIME_COL),
            Constraint::Length(LVL_COL),
            Constraint::Length(SOURCE_COL),
            Constraint::Min(20),
        ],
    )
    .header(
        Row::new(vec![
            time_header,
            "lvl".to_string(),
            "source".to_string(),
            "message".to_string(),
        ])
        .style(
            Style::default()
                .fg(theme.muted)
                .add_modifier(Modifier::BOLD),
        ),
    )
    .column_spacing(COLUMN_SPACING);
    frame.render_widget(table, area);
}

/// Character count of the longest cached message for the currently-selected
/// resource, used to cap horizontal scrolling. Returns 0 when nothing is
/// loaded, which collapses the right-scroll cap so `l` becomes a no-op.
fn longest_message_chars(state: &AppState) -> usize {
    state
        .selected_resource()
        .and_then(|r| state.logs.by_resource.get(&r.id))
        .map(|lines| {
            lines
                .iter()
                .map(|l| l.message.chars().count())
                .max()
                .unwrap_or(0)
        })
        .unwrap_or(0)
}

/// Char-aware horizontal scroll: drop the first `offset` characters and
/// prepend a `…` marker so the user can tell content has been scrolled past.
/// When `offset == 0` returns the original string verbatim. Handles UTF-8
/// safely by stepping in `char_indices` — slicing by byte index would panic
/// mid-codepoint.
fn apply_h_offset(value: &str, offset: usize) -> String {
    if offset == 0 {
        return value.to_string();
    }
    let mut iter = value.char_indices();
    if let Some((byte_idx, _)) = iter.nth(offset) {
        let mut out = String::with_capacity(value.len() - byte_idx + 3);
        out.push('\u{2026}');
        out.push_str(&value[byte_idx..]);
        out
    } else {
        // Offset past the end of the string — show the marker alone so the
        // user knows the row exists but is fully scrolled off.
        "\u{2026}".to_string()
    }
}

/// Build a cell's text content. In wrap mode the value is split into multiple
/// lines at the given character width; otherwise it is returned as a single
/// line and ratatui will truncate to fit the column.
///
/// When `query` is non-empty, every case-insensitive occurrence is rendered
/// with `hi_style` so live search reads instantly across the visible window.
fn cell_text(
    value: &str,
    query: &str,
    width: usize,
    wrap: bool,
    color: Color,
    hi_style: Style,
) -> (Text<'static>, usize) {
    let base_style = Style::default().fg(color);
    let matches = find_matches(value, query);

    if !wrap || width == 0 {
        let spans = build_spans(value, &matches, base_style, hi_style);
        return (Text::from(Line::from(spans)), 1);
    }
    let lines = wrap_highlighted(value, &matches, width, base_style, hi_style);
    let count = lines.len().max(1);
    (Text::from(lines), count)
}

/// Locate every case-insensitive ASCII occurrence of `query` in `value`.
/// Returns byte ranges suitable for slicing `value` directly (the lowercase
/// transform preserves byte indices because non-ASCII bytes are untouched).
fn find_matches(value: &str, query: &str) -> Vec<(usize, usize)> {
    if query.is_empty() || value.is_empty() {
        return Vec::new();
    }
    let lower = value.to_ascii_lowercase();
    let q = query.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut from = 0;
    while from <= lower.len() {
        match lower[from..].find(&q) {
            Some(rel) => {
                let start = from + rel;
                let end = start + q.len();
                out.push((start, end));
                // Step at least one byte forward to avoid an infinite loop on
                // empty queries (already short-circuited above) and to handle
                // back-to-back matches like "aaa" with query "aa".
                from = end.max(start + 1);
            }
            None => break,
        }
    }
    out
}

/// Non-wrap path: turn `value` into a flat row of styled spans where each
/// match range carries `hi_style`.
fn build_spans(
    value: &str,
    matches: &[(usize, usize)],
    base_style: Style,
    hi_style: Style,
) -> Vec<Span<'static>> {
    if matches.is_empty() {
        return vec![Span::styled(value.to_string(), base_style)];
    }
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut pos = 0;
    for &(start, end) in matches {
        if start > pos {
            out.push(Span::styled(value[pos..start].to_string(), base_style));
        }
        out.push(Span::styled(value[start..end].to_string(), hi_style));
        pos = end;
    }
    if pos < value.len() {
        out.push(Span::styled(value[pos..].to_string(), base_style));
    }
    out
}

/// Wrap-mode path. Walks the string char by char, tagging each char as match
/// or non-match, then chunks into lines of `width` characters and collapses
/// adjacent same-style chars into a single span per line.
fn wrap_highlighted(
    value: &str,
    matches: &[(usize, usize)],
    width: usize,
    base_style: Style,
    hi_style: Style,
) -> Vec<Line<'static>> {
    if width == 0 {
        return vec![Line::from(build_spans(
            value, matches, base_style, hi_style,
        ))];
    }
    let tokens: Vec<(char, bool)> = char_tokens(value, matches);
    if tokens.is_empty() {
        return vec![Line::from(String::new())];
    }
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(tokens.len().div_ceil(width));
    let mut i = 0;
    while i < tokens.len() {
        let end = (i + width).min(tokens.len());
        let chunk = &tokens[i..end];
        lines.push(Line::from(spans_from_tokens(chunk, base_style, hi_style)));
        i = end;
    }
    lines
}

/// Pair each char of `value` with a boolean indicating whether that char
/// position falls inside any of the (byte-indexed) `matches` ranges.
fn char_tokens(value: &str, matches: &[(usize, usize)]) -> Vec<(char, bool)> {
    let mut out: Vec<(char, bool)> = Vec::with_capacity(value.len());
    let mut iter = matches.iter().copied().peekable();
    let mut cur = iter.next();
    for (byte_pos, ch) in value.char_indices() {
        // Advance past any ranges we've already exited.
        while let Some((_s, e)) = cur {
            if byte_pos >= e {
                cur = iter.next();
            } else {
                break;
            }
        }
        let in_match = matches!(cur, Some((s, e)) if byte_pos >= s && byte_pos < e);
        out.push((ch, in_match));
    }
    out
}

fn spans_from_tokens(
    chunk: &[(char, bool)],
    base_style: Style,
    hi_style: Style,
) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut buf_match = chunk[0].1;
    for &(ch, m) in chunk {
        if m != buf_match {
            spans.push(Span::styled(
                std::mem::take(&mut buf),
                if buf_match { hi_style } else { base_style },
            ));
            buf_match = m;
        }
        buf.push(ch);
    }
    if !buf.is_empty() {
        spans.push(Span::styled(
            buf,
            if buf_match { hi_style } else { base_style },
        ));
    }
    spans
}

/// Hard-wrap on character boundaries. Logs commonly contain long unbroken
/// identifiers (request ids, URLs), so word-wrapping would leave huge ragged
/// edges; chunked char-wrap keeps the column dense.
fn wrap_chars(s: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![s.to_string()];
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.is_empty() {
        return vec![String::new()];
    }
    let mut out = Vec::with_capacity(chars.len().div_ceil(width));
    let mut i = 0;
    while i < chars.len() {
        let end = (i + width).min(chars.len());
        out.push(chars[i..end].iter().collect());
        i = end;
    }
    out
}

/// Height (in terminal rows) a single log line will occupy.
fn line_height(l: &LogLine, wrap: bool, source_w: usize, msg_w: usize) -> usize {
    if !wrap {
        return 1;
    }
    let s = wrap_chars(&l.source, source_w).len().max(1);
    let m = wrap_chars(&l.message, msg_w).len().max(1);
    s.max(m)
}

/// Choose the [start, end] index range of log lines to render so that the
/// `cursor` row is visible and the rendered rows together fit within
/// `data_height` cells of vertical space.
fn visible_range(
    lines: &[LogLine],
    cursor: usize,
    data_height: usize,
    wrap: bool,
    source_w: usize,
    msg_w: usize,
) -> (usize, usize) {
    if lines.is_empty() {
        return (0, 0);
    }
    let cursor = cursor.min(lines.len() - 1);
    // Walk backwards from the cursor accumulating heights until we've filled
    // the visible area, then walk forward to extend if there's slack left.
    let mut used = line_height(&lines[cursor], wrap, source_w, msg_w);
    let mut start = cursor;
    while start > 0 {
        let h = line_height(&lines[start - 1], wrap, source_w, msg_w);
        if used + h > data_height {
            break;
        }
        used += h;
        start -= 1;
    }
    let mut end = cursor;
    while end + 1 < lines.len() {
        let h = line_height(&lines[end + 1], wrap, source_w, msg_w);
        if used + h > data_height {
            break;
        }
        used += h;
        end += 1;
    }
    (start, end)
}

/// Best-effort IANA timezone name (e.g. `Europe/Brussels`) for the header.
///
/// Falls back through `$TZ`, `/etc/timezone`, then a `/etc/localtime` symlink
/// resolve, and finally a `UTC±HH:MM` offset string when nothing else works.
fn local_tz_label() -> String {
    if let Ok(tz) = std::env::var("TZ") {
        let t = tz.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    if let Ok(s) = std::fs::read_to_string("/etc/timezone") {
        let s = s.trim();
        if !s.is_empty() {
            return s.to_string();
        }
    }
    if let Ok(link) = std::fs::read_link("/etc/localtime") {
        let s = link.to_string_lossy().to_string();
        if let Some(idx) = s.find("zoneinfo/") {
            return s[idx + "zoneinfo/".len()..].to_string();
        }
    }
    let secs = chrono::Local::now().offset().fix().local_minus_utc();
    let sign = if secs >= 0 { '+' } else { '-' };
    let abs = secs.unsigned_abs();
    format!("UTC{}{:02}:{:02}", sign, abs / 3600, (abs % 3600) / 60)
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
        LogLevel::Warn => ("WARN".into(), theme.degraded),
        LogLevel::Info => ("INFO".into(), theme.fg),
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

    // SEM0529 is "union: must have at least one operand that can be evaluated
    // successfully when running with 'Fuzzy' mode" — i.e. every table in the
    // fuzzy union failed to resolve. For our resource-centric queries that
    // means the workspace has no rows from any of the expected tables, which
    // is functionally identical to "diagnostic settings aren't forwarding
    // anything here yet."
    let no_destination = lowered.contains("nologdestination")
        || lowered.contains("no log destination")
        || lowered.contains("pathnotfounderror")
        || lowered.contains("workspace not found")
        || lowered.contains("diagnostic settings")
        || lowered.contains("sem0529");

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
    let mut best_message = current
        .get("message")
        .and_then(|m| m.as_str())
        .map(str::to_owned);
    let mut best_code = current
        .get("code")
        .and_then(|c| c.as_str())
        .map(str::to_owned);
    while let Some(inner) = current
        .get("innererror")
        .or_else(|| current.get("innerError"))
    {
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

pub fn handle(action: Action, state: &mut AppState) -> bool {
    let lines_len = state
        .selected_resource()
        .and_then(|r| state.logs.by_resource.get(&r.id))
        .map(|v| v.len())
        .unwrap_or(0);

    // Search-input focus: only a small set of actions reach this handler — the
    // raw keystrokes flow into `logs.search_input` via app.rs. Esc cancels
    // AND clears the query (matching the storage views — "Esc removes the
    // filter, period"). Enter commits and jumps to the next match.
    if state.logs.search_active {
        match action {
            Action::Back => {
                state.logs.search_active = false;
                state.logs.search_input.reset();
                return true;
            }
            Action::OpenSelected => {
                state.logs.search_active = false;
                jump_to_match(state, 1);
                return true;
            }
            // Vertical nav while typing: drive the underlying table so the user
            // can preview matches against the surrounding context.
            Action::MoveDown
            | Action::MoveUp
            | Action::HalfPageDown
            | Action::HalfPageUp
            | Action::GotoTop
            | Action::GotoBottom => {
                // fall through to the navigation arms below
            }
            _ => return false,
        }
    }

    match action {
        Action::ToggleErrorsOnly => {
            state.logs.errors_only = !state.logs.errors_only;
            if let Some(id) = state.selected_resource().map(|r| r.id.clone()) {
                state.logs.by_resource.remove(&id);
            }
            state.logs.scroll = 0;
            true
        }
        Action::SetWindowHour => set_window(state, TimeRange::Hour),
        Action::SetWindowDay => set_window(state, TimeRange::Day),
        Action::SetWindowWeek => set_window(state, TimeRange::Week),
        Action::ToggleWrap => {
            state.logs.wrap = !state.logs.wrap;
            // Reset horizontal offset when turning wrap ON — it would be a
            // no-op anyway and we don't want the offset to "stick" invisibly
            // and surprise the user the next time wrap goes off.
            if state.logs.wrap {
                state.logs.h_offset = 0;
            }
            true
        }
        // Horizontal scroll. Only meaningful when wrap is OFF — otherwise the
        // cell already spans rows and there's nothing to reveal. Eight chars
        // per keystroke is a reasonable middle ground: fine-grained enough to
        // line up an interesting span, coarse enough that you're not mashing
        // `l` to cross a long stack trace.
        Action::MoveLeft => {
            if !state.logs.wrap {
                state.logs.h_offset = state.logs.h_offset.saturating_sub(H_SCROLL_STEP);
            }
            true
        }
        Action::MoveRight => {
            if !state.logs.wrap {
                // Cap at `longest_msg - MIN_VISIBLE` so the last MIN_VISIBLE
                // characters of the longest message stay on screen. When
                // every message is short enough to fit comfortably, the cap
                // saturates to 0 and `l` becomes a no-op — no point letting
                // the user scroll content that's already fully visible.
                let max_msg = longest_message_chars(state);
                let cap = max_msg.saturating_sub(H_SCROLL_MIN_VISIBLE);
                let new_offset = state.logs.h_offset.saturating_add(H_SCROLL_STEP);
                state.logs.h_offset = new_offset.min(cap);
            }
            true
        }
        Action::StartSearch => {
            state.logs.search_active = true;
            true
        }
        Action::NextMatch => {
            jump_to_match(state, 1);
            true
        }
        Action::PrevMatch => {
            jump_to_match(state, -1);
            true
        }
        Action::OpenSelected => {
            // Enter opens the per-line detail view. Only meaningful when at
            // least one log line is rendered.
            if lines_len > 0 {
                state.view_stack.push(crate::ui::state::View::Logs);
                state.view = crate::ui::state::View::LogDetail;
                state.logs.detail_scroll = 0;
            }
            true
        }
        Action::MoveDown => {
            if lines_len > 0 {
                let at_bottom = state.logs.scroll + 1 >= lines_len;
                state.logs.scroll = (state.logs.scroll + 1).min(lines_len - 1);
                if at_bottom {
                    request_fetch_more(state);
                }
            }
            true
        }
        Action::MoveUp => {
            state.logs.scroll = state.logs.scroll.saturating_sub(1);
            true
        }
        Action::HalfPageDown => {
            if lines_len > 0 {
                let at_bottom = state.logs.scroll + HALF_PAGE >= lines_len;
                state.logs.scroll = (state.logs.scroll + HALF_PAGE).min(lines_len - 1);
                if at_bottom {
                    request_fetch_more(state);
                }
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
            // G is also a "give me everything" hint — always ask for more,
            // even when the cursor was already at the bottom. The drain in
            // app.rs coalesces a second press while a fetch is in flight.
            request_fetch_more(state);
            true
        }
        _ => false,
    }
}

/// True iff the log line's source or message contains `query` (case-insensitive
/// ASCII). Empty query never matches — callers should short-circuit so `n`
/// isn't a hidden GotoBottom alias.
pub(crate) fn line_matches(line: &LogLine, query: &str) -> bool {
    if query.is_empty() {
        return false;
    }
    let q = query.to_ascii_lowercase();
    line.source.to_ascii_lowercase().contains(&q) || line.message.to_ascii_lowercase().contains(&q)
}

/// Move the logs cursor to the next (`direction == 1`) or previous (`-1`)
/// matching line. No-op when the query is empty, no resource is selected, or
/// no line matches. The cursor stays put if there's exactly one match and it
/// is already selected — pressing `n` shouldn't reset position.
fn jump_to_match(state: &mut AppState, direction: i32) {
    let query = state.logs.search_input.value().to_string();
    if query.is_empty() {
        return;
    }
    let Some(resource_id) = state.selected_resource().map(|r| r.id.clone()) else {
        return;
    };
    let Some(lines) = state.logs.by_resource.get(&resource_id) else {
        return;
    };
    if lines.is_empty() {
        return;
    }
    let cursor = state.logs.scroll.min(lines.len() - 1);
    let next = find_next_match(lines, cursor, &query, direction);
    if let Some(idx) = next {
        state.logs.scroll = idx;
    }
}

/// Find the next/previous matching line index, starting *after* (or *before*)
/// `cursor` and wrapping around once.
fn find_next_match(lines: &[LogLine], cursor: usize, query: &str, direction: i32) -> Option<usize> {
    if lines.is_empty() {
        return None;
    }
    let n = lines.len();
    let step: isize = if direction >= 0 { 1 } else { -1 };
    let mut idx = cursor as isize;
    for _ in 0..n {
        idx += step;
        if idx < 0 {
            idx = (n as isize) - 1;
        } else if idx >= n as isize {
            idx = 0;
        }
        if line_matches(&lines[idx as usize], query) {
            return Some(idx as usize);
        }
    }
    // No other match — if the current line itself matches, stay put.
    if line_matches(&lines[cursor], query) {
        Some(cursor)
    } else {
        None
    }
}

/// Signal the event loop to fetch an older page for the currently-selected
/// resource. Guarded by `more_available` so we don't spam the workspace once
/// the user has reached the bottom of the window, and by `loading_more` so a
/// rapid `G`-press doesn't queue duplicate fetches.
fn request_fetch_more(state: &mut AppState) {
    if state.logs.loading || state.logs.loading_more {
        return;
    }
    let Some(id) = state.selected_resource().map(|r| r.id.clone()) else {
        return;
    };
    if !state.logs.more_available.get(&id).copied().unwrap_or(false) {
        return;
    }
    state.logs.fetch_more_requested = true;
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
            created_at: None,
            modified_at: None,
        }
    }

    fn line(off: i64, level: LogLevel, source: &str, msg: &str) -> LogLine {
        LogLine {
            ts: Utc::now() - Duration::minutes(off),
            level,
            source: source.into(),
            message: msg.into(),
            fields: Vec::new(),
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
        assert!(
            out.contains("SEM0100"),
            "expected deepest code, got {out:?}"
        );
    }

    #[test]
    fn friendly_log_error_collapses_no_destination_variants() {
        for variant in [
            "NoLogDestination",
            "azure api error 404: {\"error\":{\"code\":\"PathNotFoundError\"}}",
            "diagnostic settings missing",
            // SEM0529: fuzzy union resolved zero tables — same root cause as
            // "no destination configured" from the user's perspective.
            r#"azure api error 400: {"error":{"message":"x","code":"BadArgumentError","innererror":{"code":"SemanticError","message":"union: must have at least one operand that can be evaluated successfully when running with 'Fuzzy' mode. (SEM0529)"}}}"#,
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
        assert_eq!(
            state.view,
            View::Logs,
            "view-local handler must not transition on Back"
        );
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

    #[test]
    fn toggle_wrap_flips_flag() {
        let mut state = fixture(ResourceKind::FunctionApp);
        assert!(!state.logs.wrap);
        assert!(handle(Action::ToggleWrap, &mut state));
        assert!(state.logs.wrap);
        assert!(handle(Action::ToggleWrap, &mut state));
        assert!(!state.logs.wrap);
    }

    #[test]
    fn toggle_wrap_on_resets_h_offset() {
        // Scrolling right then turning wrap on should reset the offset so
        // the row layout is sensible and stays sensible when wrap is later
        // toggled back off.
        let mut state = fixture(ResourceKind::FunctionApp);
        state.logs.h_offset = 24;
        assert!(handle(Action::ToggleWrap, &mut state));
        assert!(state.logs.wrap);
        assert_eq!(state.logs.h_offset, 0);
    }

    /// Seed the logs cache with a single line whose message has the given
    /// character length, so horizontal-scroll tests have something to clamp
    /// against.
    fn seed_with_message(state: &mut AppState, message_chars: usize) {
        let id = state.selected_resource().unwrap().id.clone();
        let message = "x".repeat(message_chars);
        state.logs.by_resource.insert(
            id,
            vec![crate::azure::logs::LogLine {
                ts: chrono::Utc::now(),
                level: crate::azure::logs::LogLevel::Info,
                source: "AppLogs".into(),
                message,
                fields: vec![],
            }],
        );
    }

    #[test]
    fn move_left_right_scroll_horizontally_when_unwrapped() {
        let mut state = fixture(ResourceKind::FunctionApp);
        // Plenty of message room: 1000 chars - 20 min-visible = cap of 980.
        seed_with_message(&mut state, 1000);
        assert!(!state.logs.wrap);
        assert!(handle(Action::MoveRight, &mut state));
        assert_eq!(state.logs.h_offset, H_SCROLL_STEP);
        assert!(handle(Action::MoveRight, &mut state));
        assert_eq!(state.logs.h_offset, 2 * H_SCROLL_STEP);
        assert!(handle(Action::MoveLeft, &mut state));
        assert_eq!(state.logs.h_offset, H_SCROLL_STEP);
        // Saturating: extra MoveLeft beyond zero stays at zero.
        for _ in 0..5 {
            assert!(handle(Action::MoveLeft, &mut state));
        }
        assert_eq!(state.logs.h_offset, 0);
    }

    #[test]
    fn move_right_saturates_at_longest_message_minus_min_visible() {
        let mut state = fixture(ResourceKind::FunctionApp);
        // Message is 100 chars long — cap should be 100 - 20 = 80.
        seed_with_message(&mut state, 100);
        // Hammer MoveRight until it stops advancing.
        let mut last = state.logs.h_offset;
        for _ in 0..200 {
            assert!(handle(Action::MoveRight, &mut state));
            if state.logs.h_offset == last {
                break;
            }
            last = state.logs.h_offset;
        }
        assert_eq!(state.logs.h_offset, 80);
    }

    #[test]
    fn move_right_is_noop_when_longest_message_fits_in_min_visible() {
        // If every message is at most MIN_VISIBLE chars, the cap saturates
        // to 0 and `l` should never advance — there's nothing hidden to
        // scroll toward.
        let mut state = fixture(ResourceKind::FunctionApp);
        seed_with_message(&mut state, 15);
        for _ in 0..5 {
            assert!(handle(Action::MoveRight, &mut state));
        }
        assert_eq!(state.logs.h_offset, 0);
    }

    #[test]
    fn move_left_right_are_noop_when_wrap_is_on() {
        let mut state = fixture(ResourceKind::FunctionApp);
        seed_with_message(&mut state, 1000);
        state.logs.wrap = true;
        assert!(handle(Action::MoveRight, &mut state));
        assert!(handle(Action::MoveRight, &mut state));
        assert_eq!(state.logs.h_offset, 0);
    }

    #[test]
    fn apply_h_offset_prepends_ellipsis_and_slices_chars() {
        assert_eq!(apply_h_offset("hello world", 0), "hello world");
        assert_eq!(apply_h_offset("hello world", 6), "\u{2026}world");
        // UTF-8 safe: stepping past a multi-byte char.
        assert_eq!(apply_h_offset("naïve", 2), "\u{2026}ïve");
        // Past end of string: still emits the marker so the user can tell
        // the row is fully scrolled off.
        assert_eq!(apply_h_offset("abc", 10), "\u{2026}");
    }

    #[test]
    fn wrap_chars_chunks_long_strings() {
        let out = wrap_chars("abcdefghij", 4);
        assert_eq!(out, vec!["abcd", "efgh", "ij"]);
        let out = wrap_chars("short", 10);
        assert_eq!(out, vec!["short"]);
        let out = wrap_chars("", 4);
        assert_eq!(out, vec![""]);
    }

    #[test]
    fn line_height_is_one_without_wrap() {
        let l = line(0, LogLevel::Info, "src", &"x".repeat(200));
        assert_eq!(line_height(&l, false, 32, 40), 1);
        // With wrap on, a 200-char message in a 40-wide column takes 5 rows.
        assert_eq!(line_height(&l, true, 32, 40), 5);
    }

    #[test]
    fn visible_range_anchors_on_cursor_in_wrap_mode() {
        // Three lines: a, big, c. With wrap and a small viewport, the cursor
        // row must stay visible even if it pushes neighbours out.
        let lines = vec![
            line(1, LogLevel::Info, "s", "a"),
            line(2, LogLevel::Info, "s", &"x".repeat(80)),
            line(3, LogLevel::Info, "s", "c"),
        ];
        // 20-wide message column → middle row is 4 cells tall; data_height 4
        // forces it to be the only row.
        let (start, end) = visible_range(&lines, 1, 4, true, 32, 20);
        assert_eq!((start, end), (1, 1));
    }

    #[test]
    fn find_matches_returns_all_case_insensitive_byte_ranges() {
        let v = "Error: an Error happened (error code 42)";
        let m = find_matches(v, "error");
        assert_eq!(m, vec![(0, 5), (10, 15), (26, 31)]);
        // Slicing must round-trip the original characters.
        for (s, e) in m {
            assert_eq!(v[s..e].to_lowercase(), "error");
        }
    }

    #[test]
    fn find_matches_handles_overlap_and_empty() {
        // No overlap on a 2-letter query repeated in "aaa".
        assert_eq!(find_matches("aaa", "aa"), vec![(0, 2)]);
        // Empty query never matches.
        assert!(find_matches("anything", "").is_empty());
        // Empty value never matches.
        assert!(find_matches("", "x").is_empty());
    }

    #[test]
    fn build_spans_splits_on_match_boundaries() {
        let base = Style::default().fg(Color::Reset);
        let hi = Style::default().bg(Color::Yellow);
        let v = "foo bar foo";
        let matches = find_matches(v, "foo");
        let spans = build_spans(v, &matches, base, hi);
        // foo|" bar "|foo  → 3 spans
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].content, "foo");
        assert_eq!(spans[1].content, " bar ");
        assert_eq!(spans[2].content, "foo");
        assert_eq!(spans[0].style, hi);
        assert_eq!(spans[1].style, base);
        assert_eq!(spans[2].style, hi);
    }

    #[test]
    fn start_search_sets_flag() {
        let mut state = fixture(ResourceKind::FunctionApp);
        assert!(handle(Action::StartSearch, &mut state));
        assert!(state.logs.search_active);
    }

    #[test]
    fn back_while_search_active_clears_and_consumes() {
        let mut state = fixture(ResourceKind::FunctionApp);
        state.logs.search_active = true;
        state.logs.search_input = tui_input::Input::default().with_value("error".to_string());
        assert!(handle(Action::Back, &mut state));
        assert!(!state.logs.search_active);
        assert!(
            state.logs.search_input.value().is_empty(),
            "Esc removes the filter — matches the storage views' behaviour"
        );
        // Back is consumed (does not fall through to the global semantic-parent
        // navigation).
    }

    #[test]
    fn enter_while_search_active_commits_and_jumps() {
        let mut state = fixture(ResourceKind::FunctionApp);
        state.logs.by_resource.insert(
            "/r/one".into(),
            vec![
                line(1, LogLevel::Info, "x", "hello"),
                line(2, LogLevel::Info, "x", "ERROR rate limited"),
                line(3, LogLevel::Info, "x", "world"),
            ],
        );
        state.logs.search_active = true;
        state.logs.search_input = state
            .logs
            .search_input
            .clone()
            .with_value("error".to_string());
        state.logs.scroll = 0;
        assert!(handle(Action::OpenSelected, &mut state));
        assert!(!state.logs.search_active);
        assert_eq!(state.logs.scroll, 1, "cursor jumps to first match");
    }

    #[test]
    fn next_match_wraps_and_prev_match_steps_back() {
        let mut state = fixture(ResourceKind::FunctionApp);
        state.logs.by_resource.insert(
            "/r/one".into(),
            vec![
                line(1, LogLevel::Info, "x", "first match here"),
                line(2, LogLevel::Info, "x", "no hit"),
                line(3, LogLevel::Info, "x", "second match here"),
            ],
        );
        state.logs.search_input = state
            .logs
            .search_input
            .clone()
            .with_value("match".to_string());
        state.logs.scroll = 0;

        // n → next match is index 2.
        assert!(handle(Action::NextMatch, &mut state));
        assert_eq!(state.logs.scroll, 2);
        // n again → wraps back to index 0.
        assert!(handle(Action::NextMatch, &mut state));
        assert_eq!(state.logs.scroll, 0);
        // N → steps backwards (wrap), so it lands on index 2 again.
        assert!(handle(Action::PrevMatch, &mut state));
        assert_eq!(state.logs.scroll, 2);
    }

    #[test]
    fn next_match_with_empty_query_is_noop() {
        let mut state = fixture(ResourceKind::FunctionApp);
        state.logs.by_resource.insert(
            "/r/one".into(),
            vec![
                line(1, LogLevel::Info, "x", "a"),
                line(2, LogLevel::Info, "x", "b"),
            ],
        );
        state.logs.scroll = 0;
        // Action still consumed (true) so the global handler doesn't try to
        // re-handle, but cursor must not move with no query.
        assert!(handle(Action::NextMatch, &mut state));
        assert_eq!(state.logs.scroll, 0);
    }

    #[test]
    fn next_match_no_hits_keeps_cursor() {
        let mut state = fixture(ResourceKind::FunctionApp);
        state.logs.by_resource.insert(
            "/r/one".into(),
            vec![
                line(1, LogLevel::Info, "x", "a"),
                line(2, LogLevel::Info, "x", "b"),
            ],
        );
        state.logs.scroll = 1;
        state.logs.search_input = state
            .logs
            .search_input
            .clone()
            .with_value("zzz".to_string());
        assert!(handle(Action::NextMatch, &mut state));
        assert_eq!(state.logs.scroll, 1);
    }

    #[test]
    fn renders_search_box_when_active() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(120, 12);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture(ResourceKind::FunctionApp);
        state.logs.by_resource.insert(
            "/r/one".into(),
            vec![line(1, LogLevel::Info, "AppTraces", "hello world")],
        );
        state.logs.search_active = true;
        state.logs.search_input = state
            .logs
            .search_input
            .clone()
            .with_value("hello".to_string());
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("/hello"), "expected the search query bar");
        assert!(s.contains("hello"), "expected the matching log line");
    }

    #[test]
    fn goto_bottom_requests_fetch_more_when_more_available() {
        let mut state = fixture(ResourceKind::ContainerApp);
        state.logs.by_resource.insert(
            "/r/one".into(),
            vec![
                line(1, LogLevel::Info, "x", "a"),
                line(2, LogLevel::Info, "x", "b"),
            ],
        );
        state.logs.more_available.insert("/r/one".into(), true);
        assert!(handle(Action::GotoBottom, &mut state));
        assert!(
            state.logs.fetch_more_requested,
            "G with more_available should ask the event loop for an older page"
        );
    }

    #[test]
    fn goto_bottom_does_not_request_when_window_exhausted() {
        let mut state = fixture(ResourceKind::ContainerApp);
        state
            .logs
            .by_resource
            .insert("/r/one".into(), vec![line(1, LogLevel::Info, "x", "a")]);
        state.logs.more_available.insert("/r/one".into(), false);
        assert!(handle(Action::GotoBottom, &mut state));
        assert!(
            !state.logs.fetch_more_requested,
            "G after window-complete must not pester the workspace again"
        );
    }

    #[test]
    fn move_down_at_last_row_requests_fetch_more() {
        let mut state = fixture(ResourceKind::ContainerApp);
        state.logs.by_resource.insert(
            "/r/one".into(),
            vec![
                line(1, LogLevel::Info, "x", "a"),
                line(2, LogLevel::Info, "x", "b"),
            ],
        );
        state.logs.more_available.insert("/r/one".into(), true);
        state.logs.scroll = 1; // already on the last row
        assert!(handle(Action::MoveDown, &mut state));
        assert!(state.logs.fetch_more_requested);
    }

    #[test]
    fn fetch_more_request_suppressed_while_loading_more() {
        let mut state = fixture(ResourceKind::ContainerApp);
        state
            .logs
            .by_resource
            .insert("/r/one".into(), vec![line(1, LogLevel::Info, "x", "a")]);
        state.logs.more_available.insert("/r/one".into(), true);
        state.logs.loading_more = true;
        assert!(handle(Action::GotoBottom, &mut state));
        assert!(
            !state.logs.fetch_more_requested,
            "in-flight fetch must coalesce repeated G presses"
        );
    }

    #[test]
    fn renders_wrapped_message() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(120, 16);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture(ResourceKind::FunctionApp);
        state.logs.wrap = true;
        let long = "Executed Functions.http_app_func (Failed, Id=385960) ".repeat(2);
        state.logs.by_resource.insert(
            "/r/one".into(),
            vec![line(
                1,
                LogLevel::Error,
                "FunctionAppLogs/FunctionAppLogs",
                &long,
            )],
        );
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        // The end of the second copy should be visible after wrapping, which
        // it would not be when the row is truncated to a single line.
        assert!(
            s.contains("Id=385960"),
            "expected wrapped content, got:\n{s}"
        );
    }
}
