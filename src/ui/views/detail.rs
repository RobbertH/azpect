//! Detail view: four sparklines (Requests, Http 5xx, CPU, Memory) plus a header
//! with the resource name + RG + health badge + window label.

#![allow(dead_code, unused_variables)]

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Sparkline, Wrap};
use ratatui::Frame;

use crate::azure::health::{derive, find, HealthStatus};
use crate::azure::logs::supports_logs;
use crate::azure::metrics::{MetricKind, MetricSeries, TimeRange};
use crate::ui::events::Action;
use crate::ui::state::{AppState, View};
use crate::ui::theme::Theme;

const FOOTER_HINT: &str = "d 1d  w 7d  L logs  Esc back  r refresh  ? help  q quit";

const ROW_KINDS: [(MetricKind, &str); 4] = [
    (MetricKind::Traffic, "Requests"),
    (MetricKind::Errors, "Http 5xx"),
    (MetricKind::Cpu, "CPU"),
    (MetricKind::Memory, "Memory"),
];

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(area);

    let selected = state.selected_resource();

    // Header
    let title = Paragraph::new(Line::from(vec![
        Span::styled(
            " detail ",
            Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            selected.map(|r| r.name.as_str()).unwrap_or("(no selection)"),
            Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
        ),
    ]));
    frame.render_widget(title, chunks[0]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(" overview ", Style::default().fg(theme.fg)));
    let inner = block.inner(chunks[1]);
    frame.render_widget(block, chunks[1]);

    let Some(resource) = selected else {
        let p = Paragraph::new(Line::from(Span::styled(
            "no resource selected.",
            Style::default().fg(theme.muted),
        )));
        frame.render_widget(p, inner);
        render_footer(frame, chunks[2], theme);
        return;
    };

    let metrics_opt = state.metrics.by_resource.get(&resource.id);
    let failure = state.metrics.failures.get(&resource.id);
    let availability = state.health.by_resource.get(&resource.id).map(|a| a.state);
    let (badge_color, badge_label) = if failure.is_some() {
        (theme.critical, "ERROR")
    } else if metrics_opt.is_none() && availability.is_none() {
        (theme.muted, "LOADING")
    } else {
        let m: &[MetricSeries] = metrics_opt.map(|v| v.as_slice()).unwrap_or(&[]);
        let h = derive(m, resource.state.as_deref(), availability);
        (color_for_health(h, theme), h.label())
    };

    let second_line_text = match failure {
        Some(msg) => format!("metrics error: {msg}"),
        None => resource
            .state
            .as_deref()
            .map(|s| format!("state: {s}"))
            .unwrap_or_else(|| "state: unknown".to_string()),
    };
    let second_line_color = if failure.is_some() { theme.critical } else { theme.muted };

    // Reserve enough rows for the header line + however many rows the second
    // line needs after wrapping at the available width. Without this, long
    // error messages get clipped and the user can't read the diagnostic.
    let context_height = 1 + wrapped_line_count(&second_line_text, inner.width).max(1);
    let body = Layout::vertical([
        Constraint::Length(context_height as u16),
        Constraint::Min(0),
    ])
    .split(inner);

    let second_line = Line::from(Span::styled(
        second_line_text,
        Style::default().fg(second_line_color),
    ));

    let context = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(&resource.resource_group, Style::default().fg(theme.muted)),
            Span::styled(" · ", Style::default().fg(theme.muted)),
            Span::styled(resource.kind.short_tag(), Style::default().fg(theme.accent)),
            Span::styled(" · ", Style::default().fg(theme.muted)),
            Span::styled("●", Style::default().fg(badge_color)),
            Span::raw(" "),
            Span::styled(badge_label, Style::default().fg(badge_color)),
            Span::styled(" · ", Style::default().fg(theme.muted)),
            Span::styled(
                format!("window {}", state.metrics.range.label()),
                Style::default().fg(theme.fg),
            ),
            Span::styled(
                if state.metrics.loading { "  · refreshing…" } else { "" },
                Style::default().fg(theme.muted),
            ),
        ]),
        second_line,
    ])
    .wrap(Wrap { trim: false });
    frame.render_widget(context, body[0]);

    // Sparkline grid: 4 rows of equal fixed height (1 label line + 2 bars),
    // plus a single shared time-axis row at the bottom. All sparklines span
    // the same window, so one axis serves the whole grid.
    let metric_rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .split(body[1]);

    let missing_for_resource = state.metrics.missing.get(&resource.id);
    for (i, (kind, label)) in ROW_KINDS.iter().enumerate() {
        let area = metric_rows[i];
        if area.height == 0 {
            continue;
        }
        let missing_reason = missing_for_resource.and_then(|m| m.get(kind));
        render_metric_row(
            frame,
            area,
            *kind,
            label,
            metrics_opt,
            missing_reason,
            state,
            theme,
        );
    }

    if metric_rows[4].height > 0 {
        render_time_axis(frame, metric_rows[4], state.metrics.range, theme);
    }

    render_footer(frame, chunks[2], theme);
}

/// Render a 1-row time axis aligned under the sparkline grid. Five
/// evenly-spaced labels are placed by column, marking the window's start, the
/// three quarter-points, and `now` (right-anchored). For very narrow widths,
/// degrades to just the start and end labels.
fn render_time_axis(frame: &mut Frame, area: Rect, range: TimeRange, theme: &Theme) {
    let line = build_time_axis(range, area.width);
    let p = Paragraph::new(Line::from(Span::styled(
        line,
        Style::default().fg(theme.muted),
    )));
    frame.render_widget(p, area);
}

fn build_time_axis(range: TimeRange, width: u16) -> String {
    let w = width as usize;
    if w < 6 {
        return String::new();
    }
    let total_minutes = match range {
        TimeRange::Day => 24 * 60_i64,
        TimeRange::Week => 7 * 24 * 60_i64,
    };

    // Anchor positions at 0, 1/4, 2/4, 3/4 of the width (left-anchored), and
    // a final "now" right-anchored to the very end. Fall back to two labels on
    // narrow widths so we never render overlapping garbage.
    let mut row: Vec<char> = vec![' '; w];

    let place = |row: &mut Vec<char>, start: usize, label: &str| {
        for (i, c) in label.chars().enumerate() {
            if start + i < row.len() {
                row[start + i] = c;
            }
        }
    };

    let now = "now";
    let now_start = w.saturating_sub(now.chars().count());

    if w >= 40 {
        let positions = [0usize, w / 4, w / 2, (3 * w) / 4];
        for p in positions.iter() {
            let frac = *p as f64 / w as f64;
            let mins_ago = ((1.0 - frac) * total_minutes as f64).round() as i64;
            let label = format_relative_minutes(mins_ago);
            // Don't bleed into the "now" label.
            let max_label_end = now_start.saturating_sub(1);
            if *p < max_label_end {
                let truncated_end = (p + label.chars().count()).min(max_label_end);
                let trimmed: String = label.chars().take(truncated_end - p).collect();
                place(&mut row, *p, &trimmed);
            }
        }
    } else {
        // Just the start label.
        let label = format_relative_minutes(total_minutes);
        let max_end = now_start.saturating_sub(1);
        let trimmed: String = label.chars().take(max_end).collect();
        place(&mut row, 0, &trimmed);
    }

    place(&mut row, now_start, now);
    row.into_iter().collect()
}

/// Format a "minutes ago" count as a short relative timestamp (`-15m`, `-3h`,
/// `-2d`). Zero collapses to `now`.
fn format_relative_minutes(m: i64) -> String {
    if m <= 0 {
        return "now".to_string();
    }
    if m < 60 {
        return format!("-{m}m");
    }
    if m < 24 * 60 {
        return format!("-{}h", m / 60);
    }
    let days = m / (24 * 60);
    let rem_h = (m % (24 * 60)) / 60;
    if rem_h == 0 {
        format!("-{days}d")
    } else {
        format!("-{days}d{rem_h}h")
    }
}

/// Approximate how many terminal rows `text` will occupy after Paragraph
/// wrapping at the given width. Counts by char (close enough for ASCII error
/// messages; double-width glyphs would slightly over-reserve, which is fine).
fn wrapped_line_count(text: &str, width: u16) -> usize {
    let w = width.max(1) as usize;
    let chars = text.chars().count().max(1);
    chars.div_ceil(w)
}

fn render_footer(frame: &mut Frame, area: Rect, theme: &Theme) {
    let p = Paragraph::new(Line::from(Span::styled(
        FOOTER_HINT,
        Style::default().fg(theme.muted),
    )));
    frame.render_widget(p, area);
}

fn render_metric_row(
    frame: &mut Frame,
    area: Rect,
    kind: MetricKind,
    label: &str,
    metrics: Option<&Vec<MetricSeries>>,
    missing_reason: Option<&String>,
    state: &AppState,
    theme: &Theme,
) {
    let series = metrics.and_then(|m| find(m, kind));

    // Two stacked lines: title row + sparkline bars.
    let parts = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(area);

    let summary = match series {
        Some(s) => summary_for(kind, s),
        None if state.metrics.loading => "loading…".to_string(),
        None if missing_reason.is_some() => "n/a".to_string(),
        None => "—".to_string(),
    };

    let title = Paragraph::new(Line::from(vec![
        Span::styled(
            format!("{label:<10}"),
            Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
        ),
        Span::styled(summary, Style::default().fg(theme.muted)),
    ]));
    frame.render_widget(title, parts[0]);

    match series {
        Some(s) if !s.points.is_empty() => {
            let data = scaled_data(s);
            let max = data.iter().copied().max().unwrap_or(1).max(1);
            let color = color_for_metric(kind, theme);
            let sparkline = Sparkline::default()
                .data(&data[..])
                .max(max)
                .style(Style::default().fg(color));
            frame.render_widget(sparkline, parts[1]);
        }
        _ => {
            let placeholder = match missing_reason {
                Some(reason) => format!("not available · {}", short_missing_reason(reason)),
                None => "—".to_string(),
            };
            let p = Paragraph::new(Line::from(Span::styled(
                placeholder,
                Style::default().fg(theme.muted),
            )))
            .wrap(Wrap { trim: false });
            frame.render_widget(p, parts[1]);
        }
    }
}

/// Translate the raw Azure error into a one-line, plain-language hint. Falls
/// back to the first chunk of the error if we don't recognise the pattern.
fn short_missing_reason(reason: &str) -> String {
    if reason.contains("Failed to find metric configuration") {
        // The most common case: the metric name doesn't exist for this
        // resource's plan tier (e.g. CpuTime on a non-Consumption Function App).
        "metric not exposed for this plan/tier".to_string()
    } else if reason.contains("403") || reason.contains("Forbidden") {
        "permission denied (need Monitoring Reader)".to_string()
    } else if reason.contains("404") {
        "resource not found".to_string()
    } else if reason.contains("BadRequest") || reason.contains("400") {
        "request rejected by Azure".to_string()
    } else {
        // Generic fallback: trim to a single line, cap length.
        let one_line: String = reason.chars().take(80).collect();
        if reason.chars().count() > 80 {
            format!("{one_line}…")
        } else {
            one_line
        }
    }
}

fn scaled_data(series: &MetricSeries) -> Vec<u64> {
    series
        .points
        .iter()
        .map(|p| {
            let v = p.value;
            if !v.is_finite() || v <= 0.0 {
                0u64
            } else {
                // Multiply by 100 for sub-unit precision; clamp to u64.
                let scaled = (v * 100.0).round();
                if scaled >= u64::MAX as f64 {
                    u64::MAX
                } else {
                    scaled as u64
                }
            }
        })
        .collect()
}

fn summary_for(kind: MetricKind, s: &MetricSeries) -> String {
    match kind {
        MetricKind::Traffic | MetricKind::Errors => {
            let total = s.sum();
            format!("total: {}{}", format_count(total), unit_suffix(s))
        }
        MetricKind::Cpu => {
            let latest = s.latest().unwrap_or(0.0);
            format!("latest: {}{}", format_value(latest), unit_suffix(s))
        }
        MetricKind::Memory => {
            let latest = s.latest().unwrap_or(0.0);
            format!("latest: {}{}", format_bytes(latest), unit_suffix(s))
        }
    }
}

fn unit_suffix(s: &MetricSeries) -> String {
    let unit = s.unit.trim();
    if unit.is_empty() || unit.eq_ignore_ascii_case("count") || unit.eq_ignore_ascii_case("bytes") {
        String::new()
    } else if unit == "%" {
        "%".to_string()
    } else {
        format!(" {unit}")
    }
}

fn format_count(v: f64) -> String {
    let v = v.max(0.0);
    if v >= 1_000_000.0 {
        format!("{:.1}M", v / 1_000_000.0)
    } else if v >= 1_000.0 {
        format!("{:.1}k", v / 1_000.0)
    } else {
        format!("{v:.0}")
    }
}

fn format_value(v: f64) -> String {
    if v >= 100.0 {
        format!("{v:.0}")
    } else {
        format!("{v:.1}")
    }
}

fn format_bytes(v: f64) -> String {
    let v = v.max(0.0);
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const KB: f64 = 1024.0;
    if v >= GB {
        format!("{:.1} GB", v / GB)
    } else if v >= MB {
        format!("{:.1} MB", v / MB)
    } else if v >= KB {
        format!("{:.1} KB", v / KB)
    } else {
        format!("{v:.0} B")
    }
}

fn color_for_metric(kind: MetricKind, theme: &Theme) -> Color {
    match kind {
        MetricKind::Traffic => theme.accent,
        MetricKind::Errors => theme.critical,
        MetricKind::Cpu => theme.healthy,
        MetricKind::Memory => theme.degraded,
    }
}

fn color_for_health(status: HealthStatus, theme: &Theme) -> Color {
    match status {
        HealthStatus::Healthy => theme.healthy,
        HealthStatus::Idle => theme.idle,
        HealthStatus::Degraded => theme.degraded,
        HealthStatus::Critical => theme.critical,
        HealthStatus::Unknown => theme.unknown,
    }
}

pub fn handle(action: Action, state: &mut AppState) -> bool {
    match action {
        Action::SetWindowDay => set_window(state, TimeRange::Day),
        Action::SetWindowWeek => set_window(state, TimeRange::Week),
        Action::OpenLogs => {
            let supports = state
                .selected_resource()
                .map(|r| supports_logs(r.kind))
                .unwrap_or(false);
            if supports {
                state.view_stack.push(state.view);
                state.view = View::Logs;
            } else {
                state.status_message =
                    Some("logs are not supported for this resource type".to_string());
            }
            true
        }
        _ => false,
    }
}

fn set_window(state: &mut AppState, range: TimeRange) -> bool {
    if state.metrics.range == range {
        return true;
    }
    state.metrics.range = range;
    // Drop cached series for the selected resource so Lane 3 reloads.
    if let Some(id) = state.selected_resource().map(|r| r.id.clone()) {
        state.metrics.by_resource.remove(&id);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::metrics::{MetricPoint, MetricSeries};
    use crate::azure::resources::{Resource, ResourceKind};
    use crate::config::Config;
    use chrono::{Duration, Utc};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn relative_minutes_formatting() {
        assert_eq!(format_relative_minutes(0), "now");
        assert_eq!(format_relative_minutes(-5), "now");
        assert_eq!(format_relative_minutes(15), "-15m");
        assert_eq!(format_relative_minutes(60), "-1h");
        assert_eq!(format_relative_minutes(150), "-2h");
        assert_eq!(format_relative_minutes(24 * 60), "-1d");
        assert_eq!(format_relative_minutes(24 * 60 + 180), "-1d3h");
        assert_eq!(format_relative_minutes(7 * 24 * 60), "-7d");
    }

    #[test]
    fn time_axis_anchors_now_at_right_edge() {
        let axis = build_time_axis(TimeRange::Day, 60);
        assert!(axis.ends_with("now"));
        // Day start label should appear at the very left.
        assert!(axis.starts_with("-1d") || axis.starts_with("-24h"));
        // Should be exactly the column width.
        assert_eq!(axis.chars().count(), 60);
    }

    #[test]
    fn time_axis_degrades_on_narrow_widths() {
        let axis = build_time_axis(TimeRange::Week, 12);
        assert!(axis.ends_with("now"));
        assert_eq!(axis.chars().count(), 12);
    }

    #[test]
    fn time_axis_returns_empty_below_threshold() {
        assert_eq!(build_time_axis(TimeRange::Day, 4), "");
    }

    #[test]
    fn missing_reason_recognises_metric_not_exposed() {
        let raw = "azure api error 400: {\"code\":\"BadRequest\",\"message\":\
            \"Failed to find metric configuration for provider: \
            Microsoft.Web, resource Type: sites, metric: CpuTime, ...\"}";
        assert_eq!(
            short_missing_reason(raw),
            "metric not exposed for this plan/tier"
        );
    }

    #[test]
    fn missing_reason_recognises_permission_denied() {
        let raw = "azure api error 403: Forbidden";
        assert_eq!(
            short_missing_reason(raw),
            "permission denied (need Monitoring Reader)"
        );
    }

    #[test]
    fn missing_reason_truncates_unknown_errors() {
        let long = "x".repeat(200);
        let out = short_missing_reason(&long);
        assert!(out.ends_with('…'));
        assert!(out.chars().count() <= 81);
    }

    fn r() -> Resource {
        Resource {
            id: "/r/one".into(),
            name: "alpha-func".into(),
            kind: ResourceKind::FunctionApp,
            location: "westeurope".into(),
            resource_group: "rg-demo".into(),
            subscription_id: "sub".into(),
            state: Some("Running".into()),
        }
    }

    fn series(kind: MetricKind, label: &str, vals: &[f64]) -> MetricSeries {
        let now = Utc::now();
        MetricSeries {
            kind,
            label: label.into(),
            unit: match kind {
                MetricKind::Traffic | MetricKind::Errors => "count".into(),
                MetricKind::Cpu => "%".into(),
                MetricKind::Memory => "bytes".into(),
            },
            points: vals
                .iter()
                .enumerate()
                .map(|(i, v)| MetricPoint {
                    ts: now - Duration::minutes((vals.len() - i) as i64 * 5),
                    value: *v,
                })
                .collect(),
        }
    }

    fn fixture_no_metrics() -> AppState {
        let mut s = AppState::new(Config::default());
        s.resources = vec![r()];
        s.list_cursor = 0;
        s.view = View::Detail;
        s
    }

    #[test]
    fn renders_without_metrics() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(80, 20);
        let mut term = Terminal::new(backend).unwrap();
        let state = fixture_no_metrics();
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("alpha-func"));
        assert!(s.contains("Requests"));
        assert!(s.contains("Memory"));
        assert!(s.contains("LOADING"));
    }

    #[test]
    fn renders_no_selection() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(60, 12);
        let mut term = Terminal::new(backend).unwrap();
        let state = AppState::new(Config::default());
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
    }

    #[test]
    fn back_is_not_consumed_by_view() {
        // Detail view must NOT consume Action::Back — it falls through to the
        // global handler which pops the view_stack. Consuming it here would
        // re-introduce bug_009: stamping previous_view = Some(Detail) when
        // leaving Detail caused the next Esc to bounce right back in.
        let mut state = fixture_no_metrics();
        assert!(!handle(Action::Back, &mut state));
        assert_eq!(state.view, View::Detail, "view-local handler must not transition on Back");
    }

    #[test]
    fn set_window_day_clears_cache_for_selected() {
        let mut state = fixture_no_metrics();
        state.metrics.range = TimeRange::Week;
        state.metrics.by_resource.insert(
            "/r/one".into(),
            vec![series(MetricKind::Traffic, "Requests", &[1.0])],
        );
        assert!(handle(Action::SetWindowDay, &mut state));
        assert_eq!(state.metrics.range, TimeRange::Day);
        assert!(!state.metrics.by_resource.contains_key("/r/one"));
    }

    #[test]
    fn open_logs_function_app_transitions() {
        let mut state = fixture_no_metrics();
        assert!(handle(Action::OpenLogs, &mut state));
        assert_eq!(state.view, View::Logs);
    }

    #[test]
    fn open_logs_apim_blocks() {
        let mut state = fixture_no_metrics();
        state.resources[0].kind = ResourceKind::Apim;
        assert!(handle(Action::OpenLogs, &mut state));
        assert_eq!(state.view, View::Detail);
        assert!(state.status_message.is_some());
    }

    #[test]
    fn formatters() {
        assert_eq!(format_count(0.4), "0");
        assert_eq!(format_count(999.0), "999");
        assert_eq!(format_count(12_500.0), "12.5k");
        assert_eq!(format_count(2_400_000.0), "2.4M");
        assert!(format_bytes(2.0 * 1024.0 * 1024.0).contains("MB"));
    }
}
