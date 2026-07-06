//! Per-screen rendering. Each view module exposes a `render(frame, area, state, theme)`
//! function and may expose a `handle(action, state)` helper for view-local input.

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
    use super::{edge_scroll, name_col_width, truncate_ellipsis};
    use std::cell::Cell;

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
pub mod appgw_backends;
pub mod cosmos_accounts;
pub mod cosmos_containers;
pub mod cosmos_databases;
pub mod cosmos_item;
pub mod detail;
pub mod env_var_edit;
pub mod env_vars;
pub mod help;
pub mod key_vault_items;
pub mod key_vaults;
pub mod list;
pub mod logs;
pub mod logs_detail;
pub mod metric_chart;
pub mod registries;
pub mod registry_repositories;
pub mod registry_tags;
pub mod service_bus_entities;
pub mod service_bus_namespaces;
pub mod service_bus_subscriptions;
pub mod sql_detail;
pub mod sql_resources;
pub mod storage_account_overview;
pub mod storage_accounts;
pub mod storage_blob_detail;
pub mod storage_blobs;
pub mod storage_containers;
pub mod subscriptions;
