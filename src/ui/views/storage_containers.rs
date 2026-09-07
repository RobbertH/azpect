//! Storage containers drill-in: lists blob containers under the pinned
//! account in [`crate::ui::state::StorageCache::selected_account`]. Enter on a
//! row pins the container name and opens [`View::StorageBlobs`]. `/` filters
//! the visible list with a case-insensitive substring match on the container
//! name (the same client-side filter shape used by the storage-accounts view).
//!
//! The BLOBS / SIZE columns are on demand: Azure has no per-container
//! capacity figure, so `c` (selected) / `C` (all listed) walk the container's
//! entire blob listing in the background — the portal's "Calculate size".
//! Cells count up live per page, then settle; the spawn lives in `app.rs`
//! because it needs auth, so this module's `handle` deliberately lets the
//! two actions fall through to the global handler.

#![allow(dead_code, unused_variables)]

use chrono::{DateTime, Utc};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;

use crate::ui::events::Action;
use crate::ui::state::{AppState, StorageCache, View};
use crate::ui::theme::Theme;
use crate::ui::views::detail::format_count;
use crate::ui::views::storage_account_overview::human_bytes;

const FOOTER_HINT: &str =
    "j/k move  Enter blobs  l access log  c size  C size all  / filter  Esc back  r refresh  y yank name  ? help  q quit";
const HALF_PAGE: usize = 10;

// Header strip is rendered by `Table::header(...)` and shares the same
// `Constraint` array as the body cells — there is no separate `Paragraph`
// pretending to know the column widths. That was the source of the previous
// "PUBLIC / LAST MODIFIED / IMMUTABLE" misalignment.

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);

    // Block title shows total count plus a `· N of M ` ratio whenever the
    // filter is narrowing the set, and a `/{filter}` chip when there's a
    // value. Mirrors the storage-accounts view's title layout.
    let filter_value = state.storage.containers_filter.value();
    let filter_active = state.storage.containers_filter_active;
    let total = state
        .storage
        .selected_account
        .as_ref()
        .and_then(|a| state.storage.containers.get(&a.id))
        .map(|v| v.len());
    let filtered = state
        .storage
        .selected_account
        .as_ref()
        .map(|a| state.storage.filtered_containers(&a.id))
        .unwrap_or_default();
    let count_label = match total {
        Some(t) if !filter_value.is_empty() => format!("· {} of {} ", filtered.len(), t),
        Some(t) => format!("· {t} "),
        None => String::new(),
    };
    let mut title_spans: Vec<Span> = vec![
        Span::styled(" containers ", Style::default().fg(theme.fg)),
        Span::styled(count_label, Style::default().fg(theme.muted)),
    ];
    if filter_active || !filter_value.is_empty() {
        title_spans.push(Span::styled(
            format!("/{filter_value} "),
            Style::default().fg(theme.accent),
        ));
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Line::from(title_spans));
    let inner = block.inner(chunks[0]);
    frame.render_widget(block, chunks[0]);

    // Carve off a 1-row strip for the filter input when active.
    let (search_area, body_area) = if filter_active {
        let parts = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(inner);
        (Some(parts[0]), parts[1])
    } else {
        (None, inner)
    };
    if let Some(sa) = search_area {
        let p = Paragraph::new(Line::from(vec![
            Span::styled("> ", Style::default().fg(theme.accent)),
            Span::styled(filter_value, Style::default().fg(theme.fg)),
            Span::styled("█", Style::default().fg(theme.accent)),
        ]));
        frame.render_widget(p, sa);
    }

    let Some(account) = state.storage.selected_account.as_ref() else {
        let p = Paragraph::new(Line::from(Span::styled(
            "no storage account selected.",
            Style::default().fg(theme.muted),
        )));
        frame.render_widget(p, body_area);
        render_footer(frame, chunks[1], theme);
        return;
    };

    if let Some(err) = state.storage.containers_error.get(&account.id) {
        // `Text` keeps any line breaks from a pretty-printed JSON error body;
        // `wrap` folds long lines so nothing runs off the right edge.
        let p = Paragraph::new(Text::styled(
            format!("error: {err}"),
            Style::default().fg(theme.critical),
        ))
        .wrap(Wrap { trim: false });
        frame.render_widget(p, body_area);
        render_footer(frame, chunks[1], theme);
        return;
    }

    let containers = state.storage.containers.get(&account.id);
    let loading = state.storage.containers_pending.contains(&account.id);
    match containers {
        None if loading => {
            let p = Paragraph::new(Line::from(Span::styled(
                "loading containers …",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        None => {
            let p = Paragraph::new(Line::from(Span::styled(
                "press r to load containers.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(rows) if rows.is_empty() => {
            let p = Paragraph::new(Line::from(Span::styled(
                "no containers defined on this account.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(_) if filtered.is_empty() => {
            // Underlying list is non-empty but the active filter matched
            // nothing. Mirrors the storage-accounts no-match copy.
            let p = Paragraph::new(Line::from(Span::styled(
                "no containers match the current filter.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(_) => {
            let widths = [
                Constraint::Min(20),    // NAME — absorbs extra width
                Constraint::Length(10), // PUBLIC ("None" / "Blob" / "Container")
                Constraint::Length(18), // LAST MODIFIED ("YYYY-MM-DD HH:MM")
                Constraint::Length(10), // IMMUTABLE
                Constraint::Length(8),  // BLOBS ("4.4M", "1.3k…")
                Constraint::Length(12), // SIZE ("123.45 GiB…")
            ];

            let header_row = Row::new(vec![
                Cell::from("NAME"),
                Cell::from("PUBLIC"),
                Cell::from("LAST MODIFIED"),
                Cell::from("IMMUTABLE"),
                Cell::from(Line::from("BLOBS").alignment(Alignment::Right)),
                Cell::from(Line::from("SIZE").alignment(Alignment::Right)),
            ])
            .style(
                Style::default()
                    .fg(theme.muted)
                    .add_modifier(Modifier::BOLD),
            );

            let body_rows: Vec<Row> = filtered
                .iter()
                .map(|c| {
                    let immut = match c.has_immutability_policy {
                        Some(true) => "lock",
                        Some(false) | None => "—",
                    };
                    let (public_label, public_color) =
                        public_access_label_and_color(c.public_access.as_deref(), theme);
                    let (blobs_cell, size_cell) = size_cells(state, &account.name, &c.name, theme);
                    Row::new(vec![
                        Cell::from(c.name.as_str()).style(Style::default().fg(theme.fg)),
                        Cell::from(public_label).style(Style::default().fg(public_color)),
                        Cell::from(format_last_modified(c.last_modified.as_ref()))
                            .style(Style::default().fg(theme.muted)),
                        Cell::from(immut).style(Style::default().fg(theme.accent)),
                        blobs_cell,
                        size_cell,
                    ])
                })
                .collect();

            let cursor = state.storage.containers_cursor.min(filtered.len() - 1);
            let table = Table::new(body_rows, widths)
                .header(header_row)
                .row_highlight_style(theme.selection())
                .highlight_symbol("▍ ")
                .column_spacing(2);

            // Offset persisted across frames so the window only scrolls when the
            // cursor pushes against an edge (ratatui reconciles it during render).
            let mut ts = TableState::default().with_offset(state.storage.containers_view_top.get());
            ts.select(Some(cursor));
            frame.render_stateful_widget(table, body_area, &mut ts);
            state.storage.containers_view_top.set(ts.offset());
        }
    }

    render_footer(frame, chunks[1], theme);
}

fn render_footer(frame: &mut Frame, area: Rect, theme: &Theme) {
    let p = Paragraph::new(Line::from(Span::styled(
        FOOTER_HINT,
        Style::default().fg(theme.muted),
    )));
    frame.render_widget(p, area);
}

/// BLOBS / SIZE cells for one container row. Four states, per row, because
/// each walk is its own background task:
///   - never measured → `—` (press `c`),
///   - walking → running total with a `…` suffix, counting up per page,
///   - done → final count / GiB figure,
///   - failed → `error` in the SIZE cell (the message itself is too long for
///     a table; the status line carried it when the walk was started).
fn size_cells<'a>(
    state: &AppState,
    account_name: &str,
    container: &str,
    theme: &Theme,
) -> (Cell<'a>, Cell<'a>) {
    let key = StorageCache::blobs_key(account_name, container);
    let right = |text: String, color: ratatui::style::Color| {
        Cell::from(Line::from(text).alignment(Alignment::Right)).style(Style::default().fg(color))
    };
    if state.storage.container_sizes_error.contains_key(&key) {
        return (
            right("—".into(), theme.muted),
            right("error".into(), theme.critical),
        );
    }
    let pending = state.storage.container_sizes_pending.contains(&key);
    match state.storage.container_sizes.get(&key) {
        Some(size) if pending => (
            right(
                format!("{}…", format_count(size.blob_count as f64)),
                theme.muted,
            ),
            right(format!("{}…", human_bytes(size.total_bytes)), theme.muted),
        ),
        Some(size) => (
            right(format_count(size.blob_count as f64), theme.fg),
            right(human_bytes(size.total_bytes), theme.fg),
        ),
        None if pending => (
            right("…".into(), theme.muted),
            right("…".into(), theme.muted),
        ),
        None => (
            right("—".into(), theme.muted),
            right("—".into(), theme.muted),
        ),
    }
}

/// `YYYY-MM-DD HH:MM` UTC. Compact enough to keep the column narrow while
/// still carrying the bit users actually care about (recency).
fn format_last_modified(dt: Option<&DateTime<Utc>>) -> String {
    match dt {
        Some(t) => t.format("%Y-%m-%d %H:%M").to_string(),
        None => "—".to_string(),
    }
}

/// Map the Azure container `publicAccess` value to a human label + colour.
/// "None" is renamed to "Private" because the raw word reads like "couldn't
/// fetch" in this table. Colour signals blast radius at a glance:
///   - `Container` (anyone can list AND read) → critical
///   - `Blob` (anyone with a blob URL can read) → degraded
///   - `Private` → muted (safe default)
fn public_access_label_and_color(
    raw: Option<&str>,
    theme: &Theme,
) -> (&'static str, ratatui::style::Color) {
    match raw {
        Some("Container") => ("Container", theme.critical),
        Some("Blob") => ("Blob", theme.degraded),
        Some("None") | Some("") | None => ("Private", theme.muted),
        // Unknown / future value: render as-is via the slow path. Should not
        // happen in practice — Azure has only documented the three above.
        Some(_) => ("?", theme.muted),
    }
}

pub fn handle(action: Action, state: &mut AppState) -> bool {
    let Some(account_id) = state
        .storage
        .selected_account
        .as_ref()
        .map(|a| a.id.clone())
    else {
        return false;
    };

    // Navigation operates on the filtered slice so the cursor never points
    // past the end of what's rendered. Mirrors `storage_accounts::handle`.
    let len = state.storage.filtered_containers(&account_id).len();

    // While the filter input has focus, swallow most actions but let the
    // dispatcher's filter-forwarding gate push raw chars into the buffer.
    // Esc cancels (deactivates AND clears); Enter commits (deactivates,
    // keeps the value). Down hands focus back to the filtered list.
    if state.storage.containers_filter_active {
        match action {
            Action::Back => {
                state.storage.containers_filter_active = false;
                state.storage.containers_filter.reset();
                state.storage.containers_cursor = 0;
                return true;
            }
            Action::OpenSelected => {
                state.storage.containers_filter_active = false;
                return true;
            }
            Action::MoveDown => {
                state.storage.containers_filter_active = false;
                // fall through to navigation handling below
            }
            Action::MoveUp
            | Action::HalfPageDown
            | Action::HalfPageUp
            | Action::GotoTop
            | Action::GotoBottom => {
                // fall through to navigation handling below
            }
            _ => return false,
        }
    }

    match action {
        Action::MoveDown => {
            if len > 0 {
                state.storage.containers_cursor =
                    (state.storage.containers_cursor + 1).min(len - 1);
            }
            true
        }
        Action::MoveUp => {
            state.storage.containers_cursor = state.storage.containers_cursor.saturating_sub(1);
            true
        }
        Action::HalfPageDown => {
            if len > 0 {
                state.storage.containers_cursor =
                    (state.storage.containers_cursor + HALF_PAGE).min(len - 1);
            }
            true
        }
        Action::HalfPageUp => {
            state.storage.containers_cursor =
                state.storage.containers_cursor.saturating_sub(HALF_PAGE);
            true
        }
        Action::GotoTop => {
            state.storage.containers_cursor = 0;
            true
        }
        Action::GotoBottom => {
            if len > 0 {
                state.storage.containers_cursor = len - 1;
            }
            true
        }
        Action::StartSearch => {
            state.storage.containers_filter.reset();
            state.storage.containers_cursor = 0;
            state.storage.containers_filter_active = true;
            true
        }
        Action::OpenSelected => {
            // Resolve via the filtered slice so the cursor's row matches what
            // the user actually sees on screen.
            let container_name = state
                .storage
                .filtered_containers(&account_id)
                .get(state.storage.containers_cursor)
                .map(|c| c.name.clone());
            if let Some(name) = container_name {
                state.storage.selected_container = Some(name);
                state.storage.blobs_cursor = 0;
                state.storage.blobs_filter = tui_input::Input::default();
                state.view = View::StorageBlobs;
            }
            true
        }
        Action::OpenLogs => {
            // `l` on a container: the account's blob access log pre-scoped
            // to that container.
            let container_name = state
                .storage
                .filtered_containers(&account_id)
                .get(state.storage.containers_cursor)
                .map(|c| c.name.clone());
            if let Some(name) = container_name {
                state
                    .storage
                    .enter_access_view(Some(name), View::StorageContainers);
                state.view = View::StorageAccessLogs;
            }
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::storage::{BlobContainer, ContainerSize, StorageAccount};
    use crate::config::Config;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn account_fixture() -> StorageAccount {
        StorageAccount {
            id: "/subs/x/rg/y/sa/acct1".into(),
            name: "acct1".into(),
            resource_group: "rg".into(),
            subscription_id: "sub".into(),
            location: "westeurope".into(),
            kind: Some("StorageV2".into()),
            sku: None,
            access_tier: None,
            is_hns_enabled: None,
            https_only: None,
            allow_blob_public_access: None,
            created_at: None,
        }
    }

    fn fixture() -> AppState {
        let mut state = AppState::new(Config::default());
        state.view = View::StorageContainers;
        state.storage.selected_account = Some(account_fixture());
        state
    }

    fn container(name: &str) -> BlobContainer {
        BlobContainer {
            name: name.into(),
            public_access: Some("None".into()),
            last_modified: None,
            has_immutability_policy: Some(false),
        }
    }

    #[test]
    fn renders_column_headers() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(140, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state
            .storage
            .containers
            .insert("/subs/x/rg/y/sa/acct1".into(), vec![container("logs")]);
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("NAME"), "header should include NAME");
        assert!(buf.contains("PUBLIC"), "header should include PUBLIC");
        assert!(
            buf.contains("LAST MODIFIED"),
            "header should include LAST MODIFIED"
        );
        assert!(buf.contains("IMMUTABLE"), "header should include IMMUTABLE");
    }

    #[test]
    fn renders_size_column_with_per_row_states() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(160, 12);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.storage.containers.insert(
            "/subs/x/rg/y/sa/acct1".into(),
            vec![
                container("done"),
                container("walking"),
                container("failed"),
                container("untouched"),
            ],
        );
        let key = |c: &str| StorageCache::blobs_key("acct1", c);
        state.storage.container_sizes.insert(
            key("done"),
            ContainerSize {
                blob_count: 4_360_000,
                total_bytes: 5_012_316_192_768,
            },
        );
        state.storage.container_sizes.insert(
            key("walking"),
            ContainerSize {
                blob_count: 1_300,
                total_bytes: 2 * 1024 * 1024 * 1024,
            },
        );
        state.storage.container_sizes_pending.insert(key("walking"));
        state
            .storage
            .container_sizes_error
            .insert(key("failed"), "403 forbidden".into());

        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("BLOBS"), "header should include BLOBS");
        assert!(buf.contains("SIZE"), "header should include SIZE");
        assert!(buf.contains("4.4M"), "finished count, got: {buf}");
        assert!(buf.contains("4.56 TiB"), "finished size, got: {buf}");
        assert!(
            buf.contains("1.3k…"),
            "in-flight count with ellipsis, got: {buf}"
        );
        assert!(
            buf.contains("2.00 GiB…"),
            "in-flight size with ellipsis, got: {buf}"
        );
        assert!(buf.contains("error"), "failed walk shows error, got: {buf}");
        assert!(
            !buf.contains("403 forbidden"),
            "raw error text stays out of the table, got: {buf}"
        );
    }

    #[test]
    fn size_actions_fall_through_to_global_handler() {
        // The spawn needs auth/tx, which live in app.rs — the view must not
        // swallow the actions or the walk would never start.
        let mut state = fixture();
        state
            .storage
            .containers
            .insert("/subs/x/rg/y/sa/acct1".into(), vec![container("logs")]);
        assert!(!handle(Action::CalculateSize, &mut state));
        assert!(!handle(Action::CalculateAllSizes, &mut state));
    }

    #[test]
    fn renders_containers() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(100, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state
            .storage
            .containers
            .insert("/subs/x/rg/y/sa/acct1".into(), vec![container("logs")]);
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("logs"));
    }

    #[test]
    fn enter_pins_container_and_drills_in() {
        let mut state = fixture();
        state
            .storage
            .containers
            .insert("/subs/x/rg/y/sa/acct1".into(), vec![container("logs")]);
        assert!(handle(Action::OpenSelected, &mut state));
        assert_eq!(state.view, View::StorageBlobs);
        assert_eq!(state.storage.selected_container.as_deref(), Some("logs"));
    }

    #[test]
    fn start_search_sets_filter_active() {
        let mut state = fixture();
        state
            .storage
            .containers
            .insert("/subs/x/rg/y/sa/acct1".into(), vec![container("logs")]);
        assert!(handle(Action::StartSearch, &mut state));
        assert!(state.storage.containers_filter_active);
    }

    #[test]
    fn esc_while_filtering_clears_filter() {
        // Esc on an active filter mirrors the storage-accounts view:
        // deactivate AND clear the buffer so the next `/` starts fresh.
        let mut state = fixture();
        state
            .storage
            .containers
            .insert("/subs/x/rg/y/sa/acct1".into(), vec![container("logs")]);
        state.storage.containers_filter_active = true;
        state.storage.containers_filter = tui_input::Input::default().with_value("lo".to_string());
        assert!(handle(Action::Back, &mut state));
        assert!(!state.storage.containers_filter_active);
        assert_eq!(state.storage.containers_filter.value(), "");
    }

    #[test]
    fn enter_while_filtering_keeps_value_and_deactivates() {
        let mut state = fixture();
        state
            .storage
            .containers
            .insert("/subs/x/rg/y/sa/acct1".into(), vec![container("logs")]);
        state.storage.containers_filter_active = true;
        state.storage.containers_filter = tui_input::Input::default().with_value("lo".to_string());
        assert!(handle(Action::OpenSelected, &mut state));
        assert!(!state.storage.containers_filter_active);
        assert_eq!(state.storage.containers_filter.value(), "lo");
        assert_eq!(state.view, View::StorageContainers);
    }

    #[test]
    fn filter_substring_match_is_case_insensitive() {
        let mut state = fixture();
        state.storage.containers.insert(
            "/subs/x/rg/y/sa/acct1".into(),
            vec![
                container("logs"),
                container("Backup"),
                container("audit-logs"),
            ],
        );
        state.storage.containers_filter = tui_input::Input::default().with_value("LOG".to_string());
        let names: Vec<&str> = state
            .storage
            .filtered_containers("/subs/x/rg/y/sa/acct1")
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(names, vec!["logs", "audit-logs"]);
    }

    #[test]
    fn navigation_uses_filtered_length() {
        // Two of three containers match; GotoBottom must stop at the
        // filtered length, not the raw length.
        let mut state = fixture();
        state.storage.containers.insert(
            "/subs/x/rg/y/sa/acct1".into(),
            vec![
                container("logs"),
                container("backup"),
                container("audit-logs"),
            ],
        );
        state.storage.containers_filter = tui_input::Input::default().with_value("log".to_string());
        assert!(handle(Action::GotoBottom, &mut state));
        assert_eq!(
            state.storage.containers_cursor, 1,
            "GotoBottom clamps to filtered len-1, not raw len-1",
        );
        assert!(handle(Action::MoveDown, &mut state));
        assert_eq!(state.storage.containers_cursor, 1, "MoveDown clamped");
    }

    #[test]
    fn enter_after_filter_drills_into_filtered_row() {
        // Cursor row in the filtered slice must be the one drilled into,
        // not the same index into the raw containers Vec.
        let mut state = fixture();
        state.storage.containers.insert(
            "/subs/x/rg/y/sa/acct1".into(),
            vec![container("logs"), container("backup"), container("zeta")],
        );
        state.storage.containers_filter =
            tui_input::Input::default().with_value("zeta".to_string());
        state.storage.containers_cursor = 0;
        assert!(handle(Action::OpenSelected, &mut state));
        assert_eq!(state.view, View::StorageBlobs);
        assert_eq!(state.storage.selected_container.as_deref(), Some("zeta"));
    }

    #[test]
    fn renders_filter_chip_in_title() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(120, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.storage.containers.insert(
            "/subs/x/rg/y/sa/acct1".into(),
            vec![container("logs"), container("backup")],
        );
        state.storage.containers_filter = tui_input::Input::default().with_value("lo".to_string());
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(
            buf.contains("/lo"),
            "title chip should show /lo, got: {buf}"
        );
        assert!(
            buf.contains("1 of 2"),
            "title count should switch to `N of M` when filtering, got: {buf}",
        );
    }

    #[test]
    fn renders_no_match_message() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(120, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.storage.containers.insert(
            "/subs/x/rg/y/sa/acct1".into(),
            vec![container("logs"), container("backup")],
        );
        state.storage.containers_filter = tui_input::Input::default().with_value("zzz".to_string());
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(
            buf.contains("no containers match the current filter"),
            "expected no-match copy, got: {buf}",
        );
    }
}
