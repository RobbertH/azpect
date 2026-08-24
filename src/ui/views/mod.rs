//! Per-screen rendering. Each view module exposes a `render(frame, area, state, theme)`
//! function and may expose a `handle(action, state)` helper for view-local input.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::ui::theme::Theme;

/// The `0`/`1`/`7` window-rung keys shared by every view with a windowed
/// fetch, paired with the labels their windows report (`TimeRange::label()` /
/// `AccessWindow::label()`). Views with more rungs (30d, 1y) extend this.
pub(crate) const WINDOW_RUNGS: &[(&str, &str)] = &[("0", "1h"), ("1", "1d"), ("7", "7d")];

/// Style for one footer hint token: muted like the rest of the bar, or
/// accent + bold when the token names the *currently active* choice (e.g. the
/// selected time window) — the shortcut bar then doubles as a state readout.
pub(crate) fn hint_style(theme: &Theme, active: bool) -> Style {
    if active {
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.muted)
    }
}

/// Assemble a footer line from `(text, active)` segments, joined with the
/// two-space separator the plain string footers use — the rendered text is
/// identical to the old constants, only the active token's style differs.
pub(crate) fn footer_line(theme: &Theme, segments: &[(String, bool)]) -> Line<'static> {
    let mut spans = Vec::with_capacity(segments.len() * 2);
    for (i, (text, active)) in segments.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ", hint_style(theme, false)));
        }
        spans.push(Span::styled(text.clone(), hint_style(theme, *active)));
    }
    Line::from(spans)
}

/// The `0 1h  1 1d  …` window-shortcut segments with the active rung marked.
/// `current` is the active window's label; a label matching no rung means a
/// custom window is in effect, which lights `custom_token` (e.g.
/// `"t custom window"`) instead, when the view offers one.
pub(crate) fn window_rung_segments(
    current: &str,
    rungs: &[(&str, &str)],
    custom_token: Option<&str>,
) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = rungs
        .iter()
        .map(|(key, label)| (format!("{key} {label}"), current == *label))
        .collect();
    if let Some(token) = custom_token {
        let on_rung = rungs.iter().any(|(_, label)| current == *label);
        out.push((token.to_string(), !on_rung));
    }
    out
}

/// Compact `0/1/7/t`-style key ladder: one span per key so the active
/// window's key can light up inside an otherwise muted token; the `/`
/// separators and the trailing `suffix` (e.g. `" pulls window"`) stay muted.
/// `active_key` is `None` when no key matches (a custom window a separate
/// `t custom` token accounts for).
pub(crate) fn key_ladder_spans(
    theme: &Theme,
    keys: &[&str],
    active_key: Option<&str>,
    suffix: &str,
) -> Vec<Span<'static>> {
    let mut spans = Vec::with_capacity(keys.len() * 2 + 1);
    for (i, key) in keys.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("/", hint_style(theme, false)));
        }
        spans.push(Span::styled(
            key.to_string(),
            hint_style(theme, active_key == Some(*key)),
        ));
    }
    spans.push(Span::styled(suffix.to_string(), hint_style(theme, false)));
    spans
}

/// Truncate `s` to at most `max` display columns (counted in chars), replacing
/// the dropped tail's last column with an ellipsis.
///
/// The table-style views (key vaults, registries, …) size their NAME column to
/// whatever's left after the fixed columns; on a narrow terminal that budget can
/// fall below the longest name. ratatui's `Table` clips an over-long cell with
/// *no* ellipsis, so a chopped name reads as if that truncated string were its
/// real value — pre-truncating here makes the cut legible instead.
pub(crate) fn truncate_ellipsis(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    if max <= 1 {
        return "\u{2026}".to_string();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('\u{2026}');
    out
}

/// Width to give the NAME column in a table-style view: the longest name,
/// capped to whatever the terminal leaves after the fixed columns so the table
/// fits without ratatui squeezing (and silently clipping) NAME. `fixed_w` is the
/// summed width of every non-NAME column; `n_cols` is the total column count
/// (NAME included); `longest` is the longest name in chars. Floors at 4 so the
/// "NAME" header always reads. Pair with [`truncate_ellipsis`] on the cell text
/// so an over-budget name shows the cut.
/// Width for a fixed (non-NAME) table column: the wider of its header and its
/// widest value, capped at `cap`. Sizing to the actual content frees slack for
/// the NAME column instead of reserving a generous fixed width that starves NAME
/// into an unreadable 3-char ellipsis on a narrow terminal. `longest_value` is
/// the widest value in chars; pass 0 when the column is empty.
pub(crate) fn col_width(header: &str, longest_value: u16, cap: u16) -> u16 {
    longest_value.max(header.chars().count() as u16).min(cap)
}

pub(crate) fn name_col_width(area_width: u16, fixed_w: u16, n_cols: u16, longest: u16) -> u16 {
    // Chrome the table eats before columns: the selection symbol ("▍ ", 2 cols)
    // plus 2 cols of spacing between each adjacent column.
    let chrome = 2 + 2 * n_cols.saturating_sub(1);
    let budget = area_width.saturating_sub(fixed_w + chrome);
    longest.min(budget).max(4)
}

/// Reconcile a persisted viewport top with the cursor under the **edge-scroll**
/// policy (same as the logs view): the window stays where it was, the cursor
/// moves freely inside it, and the window only shifts once the cursor pushes
/// against an edge — up so the cursor lands on the first visible row, down so
/// it lands on the last. Returns the reconciled top and writes it back to
/// `top` for the next frame; render is the only place that knows `visible`,
/// hence the `Cell` (views take `&AppState`). A stale top self-heals here: it
/// is clamped to the last full window, and a cursor reset to 0 drags it back up
/// on the next frame.
pub(crate) fn edge_scroll(
    top: &std::cell::Cell<usize>,
    cursor: usize,
    len: usize,
    visible: usize,
) -> usize {
    if visible == 0 || len <= visible {
        top.set(0);
        return 0;
    }
    let mut t = top.get().min(len - visible);
    if cursor < t {
        t = cursor;
    } else if cursor >= t + visible {
        t = cursor + 1 - visible;
    }
    top.set(t);
    t
}

#[cfg(test)]
mod tests {
    use super::{
        edge_scroll, footer_line, hint_style, key_ladder_spans, name_col_width, truncate_ellipsis,
        window_rung_segments, Theme, WINDOW_RUNGS,
    };
    use std::cell::Cell;

    #[test]
    fn window_rung_segments_mark_the_active_rung() {
        let segs = window_rung_segments("1d", WINDOW_RUNGS, Some("t custom window"));
        assert_eq!(
            segs,
            vec![
                ("0 1h".to_string(), false),
                ("1 1d".to_string(), true),
                ("7 7d".to_string(), false),
                ("t custom window".to_string(), false),
            ]
        );
    }

    #[test]
    fn window_rung_segments_fall_back_to_the_custom_token() {
        // A user-typed window ("6m") matches no rung — the custom token
        // lights up instead, so the bar always shows *some* active window.
        let segs = window_rung_segments("6m", WINDOW_RUNGS, Some("t custom window"));
        assert!(segs
            .iter()
            .all(|(text, active)| { (text == "t custom window") == *active }));
        // Without a custom token (chart views: 0/1/7 only), nothing extra is
        // appended and an unmatched label simply marks no rung.
        let segs = window_rung_segments("1h", WINDOW_RUNGS, None);
        assert_eq!(segs.len(), 3);
        assert!(segs[0].1 && !segs[1].1 && !segs[2].1);
    }

    #[test]
    fn footer_line_joins_segments_and_styles_the_active_one() {
        let theme = Theme::catppuccin_mocha();
        let line = footer_line(
            &theme,
            &[
                ("j/k move".to_string(), false),
                ("1 1d".to_string(), true),
                ("? help".to_string(), false),
            ],
        );
        // Rendered text is identical to the old plain constant.
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "j/k move  1 1d  ? help");
        // Exactly one token carries the active style.
        let active = hint_style(&theme, true);
        let highlighted: Vec<_> = line
            .spans
            .iter()
            .filter(|s| s.style == active)
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(highlighted, vec!["1 1d"]);
    }

    #[test]
    fn key_ladder_highlights_only_the_active_key() {
        let theme = Theme::catppuccin_mocha();
        let spans = key_ladder_spans(&theme, &["0", "1", "7", "t"], Some("7"), " pulls window");
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "0/1/7/t pulls window");
        let active = hint_style(&theme, true);
        let highlighted: Vec<_> = spans
            .iter()
            .filter(|s| s.style == active)
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(highlighted, vec!["7"]);
        // No match (custom window): everything stays muted.
        let spans = key_ladder_spans(&theme, &["0", "1", "7"], None, " window");
        assert!(spans.iter().all(|s| s.style != active));
    }

    #[test]
    fn edge_scroll_stays_put_while_cursor_moves_inside_the_window() {
        // Window at rows 10..20 of a 100-row list. Moving the cursor anywhere
        // inside — including off the bottom row back up — must not scroll.
        let top = Cell::new(10);
        assert_eq!(edge_scroll(&top, 19, 100, 10), 10); // bottom row
        assert_eq!(edge_scroll(&top, 18, 100, 10), 10); // up from the bottom
        assert_eq!(edge_scroll(&top, 10, 100, 10), 10); // top row
    }

    #[test]
    fn edge_scroll_shifts_only_at_the_edges() {
        let top = Cell::new(10);
        // Cursor pushes past the bottom → window slides down one, cursor on
        // the last visible row.
        assert_eq!(edge_scroll(&top, 20, 100, 10), 11);
        // Cursor pushes past the top → window slides up, cursor on the first.
        top.set(10);
        assert_eq!(edge_scroll(&top, 9, 100, 10), 9);
        // Far jumps (e.g. `G`/`gg`) land the cursor on the nearest edge.
        assert_eq!(edge_scroll(&top, 99, 100, 10), 90);
        assert_eq!(edge_scroll(&top, 0, 100, 10), 0);
    }

    #[test]
    fn edge_scroll_self_heals_a_stale_top() {
        // List shrank under a stale high top → clamp to the last full window,
        // then the cursor (above it) drags the window up onto itself.
        let top = Cell::new(90);
        assert_eq!(edge_scroll(&top, 5, 20, 10), 5);
        // Whole list fits → no scrolling at all, stored top resets.
        top.set(7);
        assert_eq!(edge_scroll(&top, 3, 8, 10), 0);
        assert_eq!(top.get(), 0);
    }

    #[test]
    fn truncate_ellipsis_is_noop_when_it_fits() {
        assert_eq!(
            truncate_ellipsis("kv-adp-onefab-qa", 36),
            "kv-adp-onefab-qa"
        );
        // Exact fit — no ellipsis.
        assert_eq!(truncate_ellipsis("abcd", 4), "abcd");
    }

    #[test]
    fn truncate_ellipsis_cuts_and_marks_overflow() {
        // 20 chars into a 19-wide column → 18 chars + ellipsis = 19 columns.
        assert_eq!(
            truncate_ellipsis("kv-adp-onefab-egress", 19),
            "kv-adp-onefab-egre\u{2026}"
        );
        // Counted in chars, not bytes — multibyte names truncate cleanly.
        assert_eq!(truncate_ellipsis("café-vault", 5), "café\u{2026}");
    }

    #[test]
    fn truncate_ellipsis_degenerate_widths() {
        assert_eq!(truncate_ellipsis("anything", 1), "\u{2026}");
        assert_eq!(truncate_ellipsis("anything", 0), "\u{2026}");
    }

    #[test]
    fn name_col_width_uses_longest_when_room() {
        // Wide terminal: NAME gets the full longest name, no shrink.
        // 7 cols → chrome = 2 + 2*6 = 14. 200 - (89 + 14) = 97 budget ≥ 26.
        assert_eq!(name_col_width(200, 89, 7, 26), 26);
    }

    #[test]
    fn name_col_width_shrinks_to_budget_on_narrow_terminal() {
        // Narrow terminal: budget falls below the longest name, so NAME is
        // capped (and the caller truncates the cell with an ellipsis).
        // 120 - (89 + 14) = 17 budget < 26.
        assert_eq!(name_col_width(120, 89, 7, 26), 17);
    }

    #[test]
    fn name_col_width_never_below_header() {
        // Even when nothing fits, keep room for the "NAME" header (4 cols).
        assert_eq!(name_col_width(20, 89, 7, 26), 4);
    }
}

pub mod apim_apis;
pub mod apim_operations;
pub mod apim_policy;
pub mod app_registration_sign_ins;
pub mod app_registrations;
pub mod appgw_backends;
pub mod cosmos_accounts;
pub mod cosmos_containers;
pub mod cosmos_databases;
pub mod cosmos_item;
pub mod detail;
pub mod env_var_edit;
pub mod env_vars;
pub mod help;
pub mod key_vault_access;
pub mod key_vault_items;
pub mod key_vaults;
pub mod list;
pub mod logic_app_content;
pub mod logic_app_run_detail;
pub mod logic_app_runs;
pub mod logic_app_trigger_history;
pub mod logic_apps;
pub mod logs;
pub mod logs_detail;
pub mod metric_chart;
pub mod registries;
pub mod registry_access;
pub mod registry_repositories;
pub mod registry_tags;
pub mod service_bus_entities;
pub mod service_bus_namespaces;
pub mod service_bus_subscriptions;
pub mod sql_audit;
pub mod sql_detail;
pub mod sql_resources;
pub mod sql_sessions;
pub mod storage_access;
pub mod storage_account_overview;
pub mod storage_accounts;
pub mod storage_blob_detail;
pub mod storage_blobs;
pub mod storage_containers;
pub mod subscriptions;
