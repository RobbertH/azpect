//! Resource-agnostic metric sparkline primitives, shared by the Apis Detail
//! view ([`super::detail`]) and the Azure SQL detail view
//! ([`super::sql_detail`]).
//!
//! Both views render the same shape — a stack of metric rows (a label + a
//! summary line over a sparkline) under one shared time axis — over a
//! `Vec<MetricSeries>`. The pieces that don't depend on *which* resource the
//! series came from live here: the value scaling, the spike-preserving
//! resample to the chart width, the per-kind colour, the time axis, the
//! Azure-error → hint translation, and the row renderer itself. The per-view
//! difference is only the **summary line** (e.g. "total: 1.2k" vs "latest: 40%
//! / peak 92%"), which each caller computes and passes in.

#![allow(dead_code, unused_variables)]

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Sparkline, Wrap};
use ratatui::Frame;

use crate::azure::metrics::{MetricKind, MetricSeries, TimeRange};
use crate::ui::theme::Theme;

/// Render one metric row: a title line (`label` left-padded to 10, then the
/// caller-computed `summary`) above a sparkline of `series`. When `series` is
/// absent or empty, draws a placeholder explaining why (`missing_reason`, if
/// any, run through [`short_missing_reason`]).
///
/// The `summary` is passed in rather than computed here because it's the one
/// resource-specific bit (request totals vs. utilization percentages); see the
/// module docs.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_chart_row(
    frame: &mut Frame,
    area: Rect,
    kind: MetricKind,
    label: &str,
    series: Option<&MetricSeries>,
    summary: &str,
    missing_reason: Option<&String>,
    theme: &Theme,
) {
    // Two stacked lines: title row + sparkline bars.
    let parts = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(area);

    let title = Paragraph::new(Line::from(vec![
        Span::styled(
            format!("{label:<10}"),
            Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
        ),
        Span::styled(summary.to_string(), Style::default().fg(theme.muted)),
    ]));
    frame.render_widget(title, parts[0]);

    match series {
        Some(s) if !s.points.is_empty() => {
            let data = scaled_data(s);
            // Ratatui's Sparkline draws one bar per data point left-to-right
            // and leaves the remaining columns blank. With a 1d/PT15M window
            // (96 points) and a chart wider than that, the latest sample lands
            // mid-area and the right ~25% looks dead. Pre-stretch so bars span
            // the full width and the most recent point sits at `now`.
            let stretched = stretch_to_width(&data, parts[1].width as usize);
            let max = stretched.iter().copied().max().unwrap_or(1).max(1);
            let bars = floor_nonzero_bars(&stretched, max);
            let color = color_for_metric(kind, theme);
            let sparkline = Sparkline::default()
                .data(&bars[..])
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

/// Per-kind sparkline colour. Utilization kinds (Cpu/Dtu/Storage/Workers) get
/// distinct hues outside the red/error register so a busy resource doesn't read
/// as failing.
pub(crate) fn color_for_metric(kind: MetricKind, theme: &Theme) -> Color {
    match kind {
        MetricKind::Traffic => theme.accent,
        // The "real work" series on a Function App — green so it reads as
        // healthy activity next to the platform-noise Requests row above it.
        MetricKind::Executions => theme.healthy,
        MetricKind::Errors => theme.critical,
        MetricKind::ClientErrors => theme.client_error,
        MetricKind::Cpu => theme.healthy,
        MetricKind::Memory => theme.degraded,
        MetricKind::Dtu => theme.accent,
        MetricKind::Storage => theme.idle,
        MetricKind::Workers => theme.degraded,
    }
}

/// Scale a series' f64 values into the `u64` domain the ratatui `Sparkline`
/// widget wants, multiplying by 100 to preserve sub-unit precision. Non-finite
/// and non-positive values collapse to 0.
pub(crate) fn scaled_data(series: &MetricSeries) -> Vec<u64> {
    series
        .points
        .iter()
        .map(|p| {
            let v = p.value;
            if !v.is_finite() || v <= 0.0 {
                0u64
            } else {
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

/// Bump every non-zero bar up to at least one eighth of `max`, so any bucket
/// with activity renders at least one row of pixels instead of rounding away to
/// an empty column. Ratatui's `Sparkline` draws `value * 8 / max` eighths per
/// bar, so a non-zero value below `max / 8` would otherwise be invisible —
/// making, e.g., a bucket that *did* see requests look empty next to its
/// parallel error series, which reads as "500s with no traffic."
pub(crate) fn floor_nonzero_bars(data: &[u64], max: u64) -> Vec<u64> {
    if max == 0 {
        return data.to_vec();
    }
    let min_visible = max.div_ceil(8).max(1);
    data.iter()
        .map(|&v| if v > 0 { v.max(min_visible) } else { 0 })
        .collect()
}

/// Resample `data` so its length matches `width`. Upsampling (sparse data,
/// wide chart) repeats samples nearest-neighbor style: each output column maps
/// back to `data[i * data.len() / width]`, so the leftmost output is `data[0]`
/// and the rightmost is the last sample. Downsampling (lots of points, narrow
/// chart) max-pools each output column over its source bucket range
/// `[i*len/width, (i+1)*len/width)` instead — nearest-neighbor would *skip*
/// input buckets (7d = 168 hourly points on a 100-col chart drops ~40% of
/// them), letting a single-bucket 5xx spike vanish while the summary shows a
/// nonzero total. Max, not average: these sparklines exist to show spikes.
pub(crate) fn stretch_to_width(data: &[u64], width: usize) -> Vec<u64> {
    if width == 0 || data.is_empty() {
        return Vec::new();
    }
    let len = data.len();
    if len <= width {
        return (0..width)
            .map(|i| data[(i * len / width).min(len - 1)])
            .collect();
    }
    (0..width)
        .map(|i| {
            let start = i * len / width;
            // Every bucket is non-empty (len > width ⇒ end > start), but keep
            // the clamp so an off-by-one can never panic on a slice bound.
            let end = ((i + 1) * len / width).clamp(start + 1, len);
            data[start..end].iter().copied().max().unwrap_or(0)
        })
        .collect()
}

/// Render a 1-row time axis aligned under the sparkline grid. Five
/// evenly-spaced labels mark the window's start, the three quarter-points, and
/// `now` (right-anchored). Narrow widths degrade to just the start and end.
pub(crate) fn render_time_axis(frame: &mut Frame, area: Rect, range: TimeRange, theme: &Theme) {
    let line = build_time_axis(range, area.width);
    let p = Paragraph::new(Line::from(Span::styled(
        line,
        Style::default().fg(theme.muted),
    )));
    frame.render_widget(p, area);
}

pub(crate) fn build_time_axis(range: TimeRange, width: u16) -> String {
    let w = width as usize;
    if w < 6 {
        return String::new();
    }
    let total_minutes = match range {
        TimeRange::Hour => 60_i64,
        TimeRange::Day => 24 * 60_i64,
        TimeRange::Week => 7 * 24 * 60_i64,
    };

    // Anchor positions at 0, 1/4, 2/4, 3/4 of the width (left-anchored), and a
    // final "now" right-anchored to the very end. Fall back to two labels on
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
pub(crate) fn format_relative_minutes(m: i64) -> String {
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

/// Translate a raw Azure metrics error into a one-line, plain-language hint.
/// Falls back to the first chunk of the error if we don't recognise the
/// pattern.
pub(crate) fn short_missing_reason(reason: &str) -> String {
    if reason.contains("Failed to find metric configuration") {
        // The most common case: the metric name doesn't exist for this
        // resource's plan/tier (e.g. CpuTime on a non-Consumption Function App,
        // or dtu_consumption_percent on a vCore SQL database).
        "metric not exposed for this plan/tier".to_string()
    } else if reason.contains("SEM0100") || reason.contains("Failed to resolve table") {
        // The Executions row's Log Analytics query references `AppRequests`
        // directly; an app without workspace-based App Insights fails table
        // resolution server-side.
        "no App Insights telemetry (AppRequests) in a workspace".to_string()
    } else if reason.contains("403") || reason.contains("Forbidden") {
        "permission denied (need Monitoring Reader)".to_string()
    } else if reason.contains("404") {
        "resource not found".to_string()
    } else if reason.contains("BadRequest") || reason.contains("400") {
        "request rejected by Azure".to_string()
    } else {
        let one_line: String = reason.chars().take(80).collect();
        if reason.chars().count() > 80 {
            format!("{one_line}…")
        } else {
            one_line
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_relative_minutes_buckets() {
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
    fn build_time_axis_places_now_and_start() {
        let axis = build_time_axis(TimeRange::Day, 60);
        assert!(axis.ends_with("now"), "axis must end with now: {axis:?}");
        assert!(axis.starts_with("-1d"), "start label: {axis:?}");
    }

    #[test]
    fn build_time_axis_narrow_still_ends_now() {
        let axis = build_time_axis(TimeRange::Week, 12);
        assert!(axis.ends_with("now"));
    }

    #[test]
    fn build_time_axis_too_narrow_is_empty() {
        assert_eq!(build_time_axis(TimeRange::Day, 4), "");
    }

    #[test]
    fn stretch_to_width_upsamples_data() {
        let data = vec![1u64, 2, 3, 4];
        let out = stretch_to_width(&data, 8);
        assert_eq!(out, vec![1, 1, 2, 2, 3, 3, 4, 4]);
    }

    #[test]
    fn stretch_to_width_downsamples_by_max_pooling() {
        // Each output column takes the max of its 2-sample source bucket, so
        // no input value can be silently skipped.
        let data = vec![10u64, 20, 30, 40, 50, 60, 70, 80];
        let out = stretch_to_width(&data, 4);
        assert_eq!(out, vec![20, 40, 60, 80]);
    }

    #[test]
    fn stretch_to_width_downsampling_preserves_single_spike() {
        // A lone 5xx spike in a week of hourly buckets must survive any chart
        // width — nearest-neighbor indexing used to skip ~40% of the buckets
        // on a 100-col terminal, contradicting the nonzero summary total.
        for spike_at in [0usize, 47, 100, 167] {
            let mut data = vec![0u64; 168];
            data[spike_at] = 5;
            for width in 1..=200usize {
                let out = stretch_to_width(&data, width);
                assert_eq!(
                    out.iter().copied().max(),
                    Some(5),
                    "spike at {spike_at} lost at width {width}"
                );
            }
        }
    }

    #[test]
    fn floor_nonzero_bars_lifts_small_values_above_render_threshold() {
        // With max=80, ratatui draws value*8/80 eighths, so anything below 10
        // renders as an empty column. A bucket with value 1 must be lifted to
        // at least ceil(80/8)=10 so it shows at least one pixel.
        let out = floor_nonzero_bars(&[0, 1, 5, 40, 80], 80);
        assert_eq!(out, vec![0, 10, 10, 40, 80]);
        for &v in &out[1..] {
            assert!(v * 8 / 80 >= 1, "non-zero bar must render: {v}");
        }
    }

    #[test]
    fn floor_nonzero_bars_keeps_zeros_empty() {
        let out = floor_nonzero_bars(&[0, 0, 0], 50);
        assert_eq!(out, vec![0, 0, 0]);
    }

    #[test]
    fn floor_nonzero_bars_handles_zero_max() {
        assert_eq!(floor_nonzero_bars(&[0, 1, 2], 0), vec![0, 1, 2]);
    }

    #[test]
    fn stretch_to_width_handles_empty_and_zero() {
        assert!(stretch_to_width(&[], 10).is_empty());
        assert!(stretch_to_width(&[1u64, 2], 0).is_empty());
    }

    #[test]
    fn stretch_to_width_last_column_is_last_sample() {
        let mut data: Vec<u64> = (0..96).collect();
        let out = stretch_to_width(&data, 120);
        assert_eq!(out.len(), 120);
        assert_eq!(*out.last().unwrap(), 95);
        data.push(96);
        let out = stretch_to_width(&data, 100);
        assert_eq!(*out.last().unwrap(), 96);
    }

    #[test]
    fn short_missing_reason_truncates_unknown_errors() {
        let long = "x".repeat(200);
        let out = short_missing_reason(&long);
        assert!(out.ends_with('…'));
        assert!(out.chars().count() <= 81);
    }
}
