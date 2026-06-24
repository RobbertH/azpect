//! Azure SQL detail view: a header describing the pinned elastic pool / single
//! database, plus a four-row utilization sparkline grid (CPU %, eDTU/DTU %,
//! storage %, workers %) over a selectable time window. The chart primitives
//! are shared with the Apis Detail view via [`super::metric_chart`]; only the
//! header and the percentage summary line are SQL-specific.

#![allow(dead_code, unused_variables)]

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use super::metric_chart;
use crate::azure::health::find;
use crate::azure::metrics::{MetricKind, MetricSeries, TimeRange};
use crate::azure::sql::SqlResource;
use crate::ui::events::Action;
use crate::ui::state::AppState;
use crate::ui::theme::Theme;

const FOOTER_HINT: &str =
    "0 1h  1 1d  7 7d  r refresh  Esc back  o portal  y yank id  ? help  q quit";

/// The utilization rows, in render order: `(kind, label)`. Labels match what
/// [`crate::azure::sql`] stamps on each `MetricSeries`.
const ROWS: &[(MetricKind, &str)] = &[
    (MetricKind::Cpu, "CPU"),
    (MetricKind::Dtu, "eDTU"),
    (MetricKind::Storage, "Storage"),
    (MetricKind::Workers, "Workers"),
];

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);

    let Some(resource) = state.sql.selected.as_ref() else {
        let p = Paragraph::new(Line::from(Span::styled(
            "no sql resource selected.",
            Style::default().fg(theme.muted),
        )));
        frame.render_widget(p, chunks[0]);
        render_footer(frame, chunks[1], theme);
        return;
    };

    let range = state.sql.metrics_range;
    let pending = state.sql.metrics_pending.contains(&resource.id);
    let mut title_spans = vec![Span::styled(
        format!(" {} / {} ", resource.server, resource.name),
        Style::default().fg(theme.fg),
    )];
    title_spans.push(Span::styled(
        format!(
            "· {} · window {} ",
            resource.kind.short_tag(),
            range.label()
        ),
        Style::default().fg(theme.muted),
    ));
    if pending {
        title_spans.push(Span::styled(
            "· loading… ",
            Style::default().fg(theme.accent),
        ));
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Line::from(title_spans));
    let inner = block.inner(chunks[0]);
    frame.render_widget(block, chunks[0]);

    // Header lines (fixed) on top, then the metric grid fills the rest.
    let parts =
        Layout::vertical([Constraint::Length(header_height()), Constraint::Min(1)]).split(inner);
    render_header(frame, parts[0], resource, theme);

    // Metric grid: 4 rows (label + 2 bars) + one shared time-axis row.
    let metric_rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .split(parts[1]);

    let series = state.sql.metrics.get(&resource.id);
    let missing = state.sql.metrics_missing.get(&resource.id);
    let failure = state.sql.metrics_failures.get(&resource.id);

    for (i, (kind, label)) in ROWS.iter().enumerate() {
        let row_area = metric_rows[i];
        if row_area.height == 0 {
            continue;
        }
        let s = series.and_then(|m| find(m, *kind));
        // Surface a whole-fetch failure on every row so the cause is visible
        // wherever the eye lands; otherwise fall back to the per-metric reason.
        let missing_reason = failure.or_else(|| missing.and_then(|m| m.get(kind)));
        let summary = match s {
            Some(s) => summary_for(s),
            None if pending => "loading…".to_string(),
            None if missing_reason.is_some() => "n/a".to_string(),
            None => "—".to_string(),
        };
        metric_chart::render_chart_row(
            frame,
            row_area,
            *kind,
            label,
            s,
            &summary,
            missing_reason,
            theme,
        );
    }

    if metric_rows[4].height > 0 {
        metric_chart::render_time_axis(frame, metric_rows[4], range, theme);
    }

    render_footer(frame, chunks[1], theme);
}

/// Number of fixed header lines rendered by [`render_header`].
fn header_height() -> u16 {
    2
}

fn render_header(frame: &mut Frame, area: Rect, r: &SqlResource, theme: &Theme) {
    let label = |s: &str| Span::styled(format!("{s} "), Style::default().fg(theme.muted));
    let value = |s: String| Span::styled(s, Style::default().fg(theme.fg));

    let sku = match (r.sku_name.as_deref(), r.sku_tier.as_deref()) {
        (Some(name), Some(tier)) => format!("{name} ({tier})"),
        (Some(name), None) => name.to_string(),
        (None, Some(tier)) => tier.to_string(),
        (None, None) => "—".to_string(),
    };
    let cap = match r.capacity {
        Some(c) => c.to_string(),
        None => "—".to_string(),
    };
    let line1 = Line::from(vec![
        label("sku:"),
        value(sku),
        Span::raw("   "),
        label("capacity:"),
        value(cap),
        Span::raw("   "),
        label("status:"),
        value(r.status.clone().unwrap_or_else(|| "—".to_string())),
    ]);

    let pool_note = if r.is_pooled() {
        " (in elastic pool)".to_string()
    } else {
        String::new()
    };
    let max_size = match r.max_size_bytes {
        Some(b) if b > 0 => format_bytes(b as f64),
        _ => "—".to_string(),
    };
    let line2 = Line::from(vec![
        label("rg:"),
        value(r.resource_group.clone()),
        Span::raw("   "),
        label("max size:"),
        value(max_size),
        Span::styled(pool_note, Style::default().fg(theme.muted)),
    ]);

    frame.render_widget(Paragraph::new(vec![line1, line2]), area);
}

/// Percentage summary for a SQL utilization series: most-recent and window-peak.
fn summary_for(s: &MetricSeries) -> String {
    let latest = s.latest().unwrap_or(0.0);
    let peak = s.max().max(0.0);
    let suffix = if s.unit == "%" { "%" } else { "" };
    format!(
        "latest: {}{suffix} / peak: {}{suffix}",
        format_value(latest),
        format_value(peak)
    )
}

fn format_value(v: f64) -> String {
    if v >= 100.0 {
        format!("{v:.0}")
    } else {
        format!("{v:.1}")
    }
}

fn format_bytes(v: f64) -> String {
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    if v >= GB {
        format!("{:.1} GB", v / GB)
    } else if v >= MB {
        format!("{:.1} MB", v / MB)
    } else {
        format!("{v:.0} B")
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
    match action {
        Action::SetWindowHour => set_window(state, TimeRange::Hour),
        Action::SetWindowDay => set_window(state, TimeRange::Day),
        Action::SetWindowWeek => set_window(state, TimeRange::Week),
        _ => false,
    }
}

/// Change the chart window and drop the cached series for the pinned resource
/// so the (non-force) reload in `after_action` refetches at the new range.
/// Mirrors the Apis Detail view's `set_window`.
fn set_window(state: &mut AppState, range: TimeRange) -> bool {
    if state.sql.metrics_range == range {
        return true;
    }
    state.sql.metrics_range = range;
    if let Some(id) = state.sql.selected.as_ref().map(|r| r.id.clone()) {
        state.sql.metrics.remove(&id);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::metrics::{MetricPoint, MetricSeries};
    use crate::azure::sql::{SqlKind, SqlResource};
    use crate::config::Config;
    use chrono::Utc;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn resource() -> SqlResource {
        SqlResource {
            id: "/subscriptions/s/resourceGroups/rg/providers/Microsoft.Sql/servers/srv/elasticPools/pool-a".into(),
            name: "pool-a".into(),
            server: "srv".into(),
            resource_group: "rg".into(),
            subscription_id: "s".into(),
            location: "westeurope".into(),
            kind: SqlKind::ElasticPool,
            sku_name: Some("StandardPool".into()),
            sku_tier: Some("Standard".into()),
            capacity: Some(100),
            status: Some("Ready".into()),
            elastic_pool_id: None,
            max_size_bytes: Some(268435456000),
        }
    }

    fn cpu_series() -> MetricSeries {
        MetricSeries {
            kind: MetricKind::Cpu,
            label: "CPU".into(),
            unit: "%".into(),
            points: vec![
                MetricPoint {
                    ts: Utc::now(),
                    value: 12.0,
                },
                MetricPoint {
                    ts: Utc::now(),
                    value: 47.5,
                },
                MetricPoint {
                    ts: Utc::now(),
                    value: 30.0,
                },
            ],
            peak_replica: None,
        }
    }

    fn fixture() -> AppState {
        let mut state = AppState::new(Config::default());
        state.view = crate::ui::state::View::SqlDetail;
        state.sql.selected = Some(resource());
        state
    }

    #[test]
    fn renders_header_and_metric_labels() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(160, 24);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.sql.metrics.insert(
            state.sql.selected.as_ref().unwrap().id.clone(),
            vec![cpu_series()],
        );
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("pool-a"), "title shows resource");
        assert!(buf.contains("CPU"), "cpu row label");
        assert!(buf.contains("eDTU"), "dtu row label");
        assert!(buf.contains("Storage"), "storage row label");
        assert!(buf.contains("Workers"), "workers row label");
        assert!(buf.contains("Standard"), "sku in header");
    }

    #[test]
    fn set_window_changes_range_and_clears_cache() {
        let mut state = fixture();
        let id = state.sql.selected.as_ref().unwrap().id.clone();
        state.sql.metrics.insert(id.clone(), vec![cpu_series()]);
        state.sql.metrics_range = TimeRange::Hour;
        assert!(handle(Action::SetWindowDay, &mut state));
        assert_eq!(state.sql.metrics_range, TimeRange::Day);
        assert!(
            !state.sql.metrics.contains_key(&id),
            "cache dropped so the reload refetches at the new range"
        );
    }

    #[test]
    fn summary_reports_latest_and_peak() {
        let out = summary_for(&cpu_series());
        assert!(out.contains("latest: 30.0%"), "{out}");
        assert!(out.contains("peak: 47.5%"), "{out}");
    }
}
