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
pub(crate) fn name_col_width(area_width: u16, fixed_w: u16, n_cols: u16, longest: u16) -> u16 {
    // Chrome the table eats before columns: the selection symbol ("▍ ", 2 cols)
    // plus 2 cols of spacing between each adjacent column.
    let chrome = 2 + 2 * n_cols.saturating_sub(1);
    let budget = area_width.saturating_sub(fixed_w + chrome);
    longest.min(budget).max(4)
}

#[cfg(test)]
mod tests {
    use super::{name_col_width, truncate_ellipsis};

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
pub mod env_vars;
pub mod help;
pub mod key_vault_items;
pub mod key_vaults;
pub mod list;
pub mod logs;
pub mod logs_detail;
pub mod registries;
pub mod registry_repositories;
pub mod registry_tags;
pub mod service_bus_entities;
pub mod service_bus_namespaces;
pub mod service_bus_subscriptions;
pub mod storage_account_overview;
pub mod storage_accounts;
pub mod storage_blob_detail;
pub mod storage_blobs;
pub mod storage_containers;
pub mod subscriptions;
