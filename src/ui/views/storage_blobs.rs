//! Storage blobs drill-in: a folder browser over the pinned
//! `(selected_account, selected_container)` pair.
//!
//! The view shows one virtual-directory level at a time
//! (`StorageCache::blobs_prefix`, `""` at the container root): the folders
//! directly under the prefix first, then the blobs directly under it. Enter
//! or Right on a folder descends; `h` / Left / Esc go up one level (Esc only
//! leaves the view at the root). Enter on a blob pins its full name and opens
//! [`View::StorageBlobDetail`].
//!
//! Listings are fetched one page (1000 rows) at a time. Moving the cursor
//! near the end of a listing the server has more of raises
//! `blobs_fetch_more`; `app.rs::after_action` drains the flag and appends
//! the next page, so a big folder streams in as you scroll.
//!
//! `Tab` / `Shift+Tab` sort by the next / previous column, `i` inverts the
//! direction. Folders always stay above blobs; the direction only reorders
//! them on the NAME column.
//!
//! The filter input has the same focus-then-commit lifecycle as the
//! storage-accounts / storage-containers views: `/` activates an empty input
//! (a previously committed value is discarded, vim-style), Enter commits and
//! deactivates (the filter value persists until the next `/`), Esc cancels
//! and clears. Matching is client-side over the rows of the current folder
//! that have been loaded so far.

#![allow(dead_code, unused_variables)]

use chrono::{DateTime, Utc};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;

use crate::ui::events::Action;
use crate::ui::state::{AppState, BlobRow, BlobSortColumn, StorageCache, View};
use crate::ui::theme::Theme;

const FOOTER_HINT: &str =
    "j/k move  Enter open  h up  / filter  Tab sort  i invert  Esc back  r refresh  y yank  ? help";
const HALF_PAGE: usize = 10;
/// Ask for the next page once the cursor is this close to the end of the
/// loaded rows, so a steady `j` never stalls on the page boundary.
const PREFETCH_ROWS: usize = 20;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    // Global breadcrumb (rendered by app::dispatch_view) carries the account,
    // container and folder path — body + footer only here. The filter
    // indicator lives in the block title.
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);

    let filter_value = state.storage.blobs_filter.value();
    let filter_active = state.storage.blobs_filter_active;
    let selected = (
        state.storage.selected_account.as_ref(),
        state.storage.selected_container.as_deref(),
    );
    let listing = match selected {
        (Some(acc), Some(cont)) => state.storage.current_blob_listing(&acc.name, cont),
        _ => None,
    };
    let key = match selected {
        (Some(acc), Some(cont)) => Some(StorageCache::blob_listing_key(
            &acc.name,
            cont,
            &state.storage.blobs_prefix,
        )),
        _ => None,
    };
    let rows = match selected {
        (Some(acc), Some(cont)) => state.storage.visible_blob_rows(&acc.name, cont),
        _ => Vec::new(),
    };
    let loading_more = key
        .as_ref()
        .is_some_and(|k| state.storage.blobs_loading_more.contains(k));

    // Title: `· N` loaded rows, `N+` while the server has more, `N of M`
    // while filtering, plus a `/{filter}` chip. Mirrors the accounts /
    // containers views and the Cosmos items paging cue.
    let mut count_label = String::new();
    if let Some(p) = listing {
        let total = p.prefixes.len() + p.blobs.len();
        let more = if p.next_marker.is_some() { "+" } else { "" };
        if filter_value.is_empty() {
            count_label.push_str(&format!("· {total}{more} "));
        } else {
            count_label.push_str(&format!("· {} of {total}{more} ", rows.len()));
        }
        if loading_more {
            count_label.push_str("· loading more … ");
        }
    }
    let mut title_spans: Vec<Span> = vec![
        Span::styled(" blobs ", Style::default().fg(theme.fg)),
        Span::styled(count_label, Style::default().fg(theme.muted)),
    ];
    if let Some(err) = key
        .as_ref()
        .and_then(|k| state.storage.blobs_more_error.get(k))
    {
        title_spans.push(Span::styled(
            format!("next page failed: {} ", truncate_line(err, 80)),
            Style::default().fg(theme.critical),
        ));
    }
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

    // Optional filter input row at the top of the inner area, mirroring the
    // pattern used by the storage-accounts / storage-containers views.
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

    let Some(key) = key else {
        let p = Paragraph::new(Line::from(Span::styled(
            "no container selected.",
            Style::default().fg(theme.muted),
        )));
        frame.render_widget(p, body_area);
        render_footer(frame, chunks[1], theme);
        return;
    };

    if let Some(err) = state.storage.blobs_error.get(&key) {
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

    let loading = state.storage.blobs_pending.contains(&key);
    let prefix = state.storage.blobs_prefix.as_str();
    match listing {
        None if loading => {
            let p = Paragraph::new(Line::from(Span::styled(
                "loading blobs …",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        None => {
            let p = Paragraph::new(Line::from(Span::styled(
                "press r to load blobs.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(p) if p.prefixes.is_empty() && p.blobs.is_empty() => {
            let msg = if prefix.is_empty() {
                "Container is empty."
            } else {
                "Folder is empty."
            };
            let p = Paragraph::new(Line::from(Span::styled(
                msg,
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(p) if rows.is_empty() => {
            // Loaded rows are non-empty but the active filter matched
            // nothing. Say so, and point at the next page when there is one.
            let msg = if loading_more {
                "no loaded blobs match the current filter — loading more …"
            } else if p.next_marker.is_some() {
                "no loaded blobs match the current filter — j loads more."
            } else {
                "no blobs match the current filter."
            };
            let p = Paragraph::new(Line::from(Span::styled(
                msg,
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(_) => {
            let widths = [
                Constraint::Min(20),    // NAME
                Constraint::Length(10), // SIZE (right-aligned)
                Constraint::Length(24), // CONTENT-TYPE
                Constraint::Length(18), // LAST MODIFIED
                Constraint::Length(10), // TYPE
            ];

            let sort = state.storage.blobs_sort;
            let header = |label: &str, col: BlobSortColumn| {
                if sort.column == col {
                    let arrow = if sort.descending { " ▼" } else { " ▲" };
                    format!("{label}{arrow}")
                } else {
                    label.to_string()
                }
            };
            let header_row = Row::new(vec![
                Cell::from(header("NAME", BlobSortColumn::Name)),
                Cell::from(
                    Line::from(header("SIZE", BlobSortColumn::Size)).alignment(Alignment::Right),
                ),
                Cell::from(header("CONTENT-TYPE", BlobSortColumn::ContentType)),
                Cell::from(header("LAST MODIFIED", BlobSortColumn::LastModified)),
                Cell::from(header("TYPE", BlobSortColumn::BlobType)),
            ])
            .style(
                Style::default()
                    .fg(theme.muted)
                    .add_modifier(Modifier::BOLD),
            );

            let body_rows: Vec<Row> = rows
                .iter()
                .map(|row| match row {
                    BlobRow::Folder(_) => Row::new(vec![
                        Cell::from(row.display_name(prefix).to_string())
                            .style(Style::default().fg(theme.accent)),
                        Cell::from(Line::from("—").alignment(Alignment::Right))
                            .style(Style::default().fg(theme.muted)),
                        Cell::from("—").style(Style::default().fg(theme.muted)),
                        Cell::from("—").style(Style::default().fg(theme.muted)),
                        Cell::from("folder").style(Style::default().fg(theme.muted)),
                    ]),
                    BlobRow::Blob(b) => Row::new(vec![
                        Cell::from(row.display_name(prefix).to_string())
                            .style(Style::default().fg(theme.fg)),
                        // Right-align inside the cell so digits line up with
                        // the right-aligned `SIZE` header.
                        Cell::from(Line::from(human_bytes(b.size)).alignment(Alignment::Right))
                            .style(Style::default().fg(theme.accent)),
                        Cell::from(b.content_type.as_deref().unwrap_or("—").to_string())
                            .style(Style::default().fg(theme.muted)),
                        Cell::from(format_last_modified(b.last_modified.as_ref()))
                            .style(Style::default().fg(theme.muted)),
                        Cell::from(b.blob_type.as_str()).style(Style::default().fg(theme.muted)),
                    ]),
                })
                .collect();

            let cursor = state.storage.blobs_cursor.min(rows.len() - 1);
            let table = Table::new(body_rows, widths)
                .header(header_row)
                .row_highlight_style(theme.selection())
                .highlight_symbol("▍ ")
                .column_spacing(2);

            // Offset persisted across frames so the window only scrolls when the
            // cursor pushes against an edge (ratatui reconciles it during render).
            let mut ts = TableState::default().with_offset(state.storage.blobs_view_top.get());
            ts.select(Some(cursor));
            frame.render_stateful_widget(table, body_area, &mut ts);
            state.storage.blobs_view_top.set(ts.offset());
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

fn format_last_modified(dt: Option<&DateTime<Utc>>) -> String {
    match dt {
        Some(t) => t.format("%Y-%m-%d %H:%M").to_string(),
        None => "—".to_string(),
    }
}

fn human_bytes(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if n >= GB {
        format!("{:.1} GB", n as f64 / GB as f64)
    } else if n >= MB {
        format!("{:.1} MB", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{:.1} KB", n as f64 / KB as f64)
    } else {
        format!("{n} B")
    }
}

fn truncate_line(s: &str, max: usize) -> String {
    let one_line = s.lines().next().unwrap_or("");
    let mut out: String = one_line.chars().take(max).collect();
    if one_line.chars().count() > max {
        out.push('…');
    }
    out
}

pub fn handle(action: Action, state: &mut AppState) -> bool {
    // Resolve the parent (account, container) pair early — without it the
    // view can't navigate at all. Returning false lets the global handler
    // process Esc / Quit.
    let (acc_name, container) = match (
        state
            .storage
            .selected_account
            .as_ref()
            .map(|a| a.name.clone()),
        state.storage.selected_container.clone(),
    ) {
        (Some(a), Some(c)) => (a, c),
        _ => return false,
    };

    // Navigation operates on the visible rows so the cursor never points
    // past the end of what's rendered. Mirrors `storage_accounts::handle`.
    let len = state.storage.visible_blob_rows(&acc_name, &container).len();
    let has_more = state
        .storage
        .current_blob_listing(&acc_name, &container)
        .is_some_and(|p| p.next_marker.is_some());

    // Esc clears the filter whether the box is still focused or it was
    // applied then defocused (Enter / Down); with no filter it climbs out of
    // the current folder; only at the container root does it fall through
    // to breadcrumb navigation. Mirrors the resource list.
    if matches!(action, Action::Back)
        && (state.storage.blobs_filter_active || !state.storage.blobs_filter.value().is_empty())
    {
        state.storage.blobs_filter_active = false;
        state.storage.blobs_filter.reset();
        state.storage.blobs_cursor = 0;
        return true;
    }

    // While the filter input has focus, swallow most actions but let the
    // dispatcher's filter-forwarding gate push raw chars into the buffer.
    // Enter commits (deactivates, keeps the value). Down hands focus back to
    // the filtered list.
    if state.storage.blobs_filter_active {
        match action {
            Action::OpenSelected => {
                state.storage.blobs_filter_active = false;
                return true;
            }
            Action::MoveDown => {
                state.storage.blobs_filter_active = false;
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

    // Downward moves near the end of a listing the server has more of ask
    // `after_action` for the next page. With a filter hiding every loaded
    // row (`len == 0`) any downward move asks, so the user can keep pressing
    // `j` to pull more candidates in.
    let request_more_if_near_end = |state: &mut AppState, target: usize| {
        if has_more && (len == 0 || target.saturating_add(PREFETCH_ROWS) >= len.saturating_sub(1)) {
            state.storage.blobs_fetch_more = true;
        }
    };

    match action {
        Action::MoveDown => {
            if len > 0 {
                state.storage.blobs_cursor = (state.storage.blobs_cursor + 1).min(len - 1);
            }
            request_more_if_near_end(state, state.storage.blobs_cursor);
            true
        }
        Action::MoveUp => {
            state.storage.blobs_cursor = state.storage.blobs_cursor.saturating_sub(1);
            true
        }
        Action::HalfPageDown => {
            if len > 0 {
                state.storage.blobs_cursor = (state.storage.blobs_cursor + HALF_PAGE).min(len - 1);
            }
            request_more_if_near_end(state, state.storage.blobs_cursor);
            true
        }
        Action::HalfPageUp => {
            state.storage.blobs_cursor = state.storage.blobs_cursor.saturating_sub(HALF_PAGE);
            true
        }
        Action::GotoTop => {
            state.storage.blobs_cursor = 0;
            true
        }
        Action::GotoBottom => {
            if len > 0 {
                state.storage.blobs_cursor = len - 1;
            }
            request_more_if_near_end(state, state.storage.blobs_cursor);
            true
        }
        Action::StartSearch => {
            state.storage.blobs_filter.reset();
            state.storage.blobs_cursor = 0;
            state.storage.blobs_filter_active = true;
            true
        }
        Action::CycleSort => {
            state.storage.blobs_sort.cycle_next();
            state.storage.blobs_cursor = 0;
            announce_sort(state);
            true
        }
        Action::CycleSortBack => {
            state.storage.blobs_sort.cycle_prev();
            state.storage.blobs_cursor = 0;
            announce_sort(state);
            true
        }
        Action::InvertSort => {
            state.storage.blobs_sort.invert();
            state.storage.blobs_cursor = 0;
            announce_sort(state);
            true
        }
        Action::MoveLeft | Action::Back => go_up(state, &acc_name, &container),
        Action::MoveRight => {
            // Right only descends; on a blob it's inert (Enter previews).
            match row_under_cursor(state, &acc_name, &container) {
                Some(Target::Folder(prefix)) => {
                    descend(state, prefix);
                    true
                }
                _ => false,
            }
        }
        Action::OpenSelected => {
            // Resolve via the visible rows so the cursor's row matches what
            // the user actually sees on screen.
            match row_under_cursor(state, &acc_name, &container) {
                Some(Target::Folder(prefix)) => descend(state, prefix),
                Some(Target::Blob(name)) => {
                    state.storage.selected_blob = Some(name);
                    state.storage.blob_preview_scroll = 0;
                    state.view = View::StorageBlobDetail;
                }
                None => {}
            }
            true
        }
        _ => false,
    }
}

/// What the cursor is on, owned so the borrow of `state` ends before the
/// handler mutates it.
enum Target {
    Folder(String),
    Blob(String),
}

fn row_under_cursor(state: &AppState, acc_name: &str, container: &str) -> Option<Target> {
    let rows = state.storage.visible_blob_rows(acc_name, container);
    rows.get(state.storage.blobs_cursor).map(|row| match row {
        BlobRow::Folder(p) => Target::Folder(p.to_string()),
        BlobRow::Blob(b) => Target::Blob(b.name.clone()),
    })
}

/// Enter the folder `prefix` (a full, `/`-terminated prefix). The filter is
/// dropped — it was scoped to the folder being left — and the cursor starts
/// at the top. `after_action` kicks the fetch if the folder isn't cached.
fn descend(state: &mut AppState, prefix: String) {
    state.storage.blobs_prefix = prefix;
    state.storage.blobs_cursor = 0;
    state.storage.blobs_view_top.set(0);
    state.storage.blobs_filter_active = false;
    state.storage.blobs_filter.reset();
}

/// Climb to the parent folder, landing the cursor on the folder just left
/// so `Enter`, `h`, `Enter` round-trips. `false` at the container root so
/// Esc falls through to the breadcrumb (containers view).
fn go_up(state: &mut AppState, acc_name: &str, container: &str) -> bool {
    let Some(parent) = state.storage.parent_blob_prefix() else {
        return false;
    };
    let left = std::mem::replace(&mut state.storage.blobs_prefix, parent);
    state.storage.blobs_filter_active = false;
    state.storage.blobs_filter.reset();
    state.storage.blobs_view_top.set(0);
    state.storage.blobs_cursor = state
        .storage
        .visible_blob_rows(acc_name, container)
        .iter()
        .position(|row| row.full_name() == left)
        .unwrap_or(0);
    true
}

fn announce_sort(state: &mut AppState) {
    let sort = state.storage.blobs_sort;
    let dir = if sort.descending {
        "descending"
    } else {
        "ascending"
    };
    state.set_status(format!("sort: {} {dir}", sort.column.label()));
}

/// Public helper used by `app.rs::yank_target` so `y` in this view copies the
/// currently-highlighted row (`account/container/path`, a folder's path
/// keeping its trailing `/`) or, with no rows loaded, the breadcrumb path of
/// the folder being shown. The cursor indexes into the visible rows so `y`
/// always matches the row the user sees on screen.
pub fn yank_text(state: &AppState) -> Option<String> {
    let acc_name = state
        .storage
        .selected_account
        .as_ref()
        .map(|a| a.name.clone())?;
    let container = state.storage.selected_container.clone()?;
    let rows = state.storage.visible_blob_rows(&acc_name, &container);
    let line = rows
        .get(state.storage.blobs_cursor)
        .map(|row| format!("{}/{}/{}", acc_name, container, row.full_name()));
    line.or_else(|| {
        let prefix = &state.storage.blobs_prefix;
        Some(if prefix.is_empty() {
            format!("{acc_name}/{container}")
        } else {
            format!("{acc_name}/{container}/{prefix}")
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::storage::{Blob, BlobPage, StorageAccount};
    use crate::config::Config;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn fixture() -> AppState {
        let mut state = AppState::new(Config::default());
        state.view = View::StorageBlobs;
        state.storage.selected_account = Some(StorageAccount {
            id: "/subs/x/rg/y/sa/acct1".into(),
            name: "acct1".into(),
            resource_group: "rg".into(),
            subscription_id: "sub".into(),
            location: "westeurope".into(),
            kind: None,
            sku: None,
            access_tier: None,
            is_hns_enabled: None,
            https_only: None,
            allow_blob_public_access: None,
            created_at: None,
        });
        state.storage.selected_container = Some("logs".into());
        state
    }

    fn blob(name: &str, size: u64) -> Blob {
        Blob {
            name: name.into(),
            size,
            content_type: Some("text/plain".into()),
            last_modified: None,
            blob_type: "BlockBlob".into(),
        }
    }

    fn page(prefixes: &[&str], blobs: Vec<Blob>) -> BlobPage {
        BlobPage {
            prefixes: prefixes.iter().map(|p| p.to_string()).collect(),
            blobs,
            next_marker: None,
        }
    }

    /// Cache `page` as the listing of `prefix` under the fixture's pair.
    fn insert(state: &mut AppState, prefix: &str, page: BlobPage) {
        let key = StorageCache::blob_listing_key("acct1", "logs", prefix);
        state.storage.blobs.insert(key, page);
    }

    fn visible_names(state: &AppState) -> Vec<String> {
        state
            .storage
            .visible_blob_rows("acct1", "logs")
            .iter()
            .map(|r| r.display_name(&state.storage.blobs_prefix).to_string())
            .collect()
    }

    #[test]
    fn renders_column_headers() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(160, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        insert(&mut state, "", page(&[], vec![blob("hello.txt", 2048)]));
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("NAME"), "header should include NAME");
        assert!(buf.contains("SIZE"), "header should include SIZE");
        assert!(
            buf.contains("CONTENT-TYPE"),
            "header should include CONTENT-TYPE"
        );
        assert!(
            buf.contains("LAST MODIFIED"),
            "header should include LAST MODIFIED"
        );
        assert!(buf.contains("TYPE"), "header should include TYPE");
    }

    #[test]
    fn renders_blob_rows() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(140, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        insert(&mut state, "", page(&[], vec![blob("hello.txt", 2048)]));
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("hello.txt"));
        assert!(buf.contains("2.0 KB"));
    }

    #[test]
    fn renders_empty_container() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(120, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        insert(&mut state, "", page(&[], Vec::new()));
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("Container is empty"));
    }

    #[test]
    fn renders_folders_relative_to_prefix_above_blobs() {
        // Inside `app/`, the sub-folder `app/2026/` shows as `2026/` and the
        // blob `app/readme.txt` as `readme.txt`; folders sort first whatever
        // the name order says.
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(120, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.storage.blobs_prefix = "app/".into();
        insert(
            &mut state,
            "app/",
            page(&["app/2026/"], vec![blob("app/1-first.txt", 1)]),
        );
        assert_eq!(visible_names(&state), vec!["2026/", "1-first.txt"]);
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("2026/"), "folder row: {buf}");
        assert!(!buf.contains("app/2026/"), "prefix must be stripped: {buf}");
        assert!(buf.contains("folder"), "TYPE column says folder: {buf}");
    }

    #[test]
    fn slash_opens_filter_input() {
        let mut state = fixture();
        assert!(handle(Action::StartSearch, &mut state));
        assert!(state.storage.blobs_filter_active);
    }

    #[test]
    fn enter_in_filter_keeps_value_and_deactivates() {
        // Enter commits: the filter value persists so the narrowed list keeps
        // applying, but focus returns to the body. A second Enter (filter
        // inactive) drills into the highlighted blob.
        let mut state = fixture();
        state.storage.blobs_filter_active = true;
        state.storage.blobs_filter = tui_input::Input::default().with_value("foo".to_string());
        assert!(handle(Action::OpenSelected, &mut state));
        assert!(!state.storage.blobs_filter_active);
        assert_eq!(state.storage.blobs_filter.value(), "foo");
        assert_eq!(state.view, View::StorageBlobs);
    }

    #[test]
    fn esc_in_filter_clears_buffer() {
        // Esc on an active filter mirrors the storage-accounts view:
        // deactivate AND clear the buffer.
        let mut state = fixture();
        state.storage.blobs_filter_active = true;
        state.storage.blobs_filter = tui_input::Input::default().with_value("foo".to_string());
        assert!(handle(Action::Back, &mut state));
        assert!(!state.storage.blobs_filter_active);
        assert_eq!(state.storage.blobs_filter.value(), "");
    }

    #[test]
    fn esc_clears_applied_filter_after_defocus() {
        // `/foo`, Enter to browse the narrowed list, then Esc clears the
        // filter instead of leaving the view.
        let mut state = fixture();
        state.storage.blobs_filter = tui_input::Input::default().with_value("foo".to_string());
        assert!(handle(Action::Back, &mut state));
        assert_eq!(state.storage.blobs_filter.value(), "");
        // With the filter gone and no folder to climb out of, Esc falls
        // through to breadcrumb navigation.
        assert!(!handle(Action::Back, &mut state));
    }

    #[test]
    fn enter_on_row_pins_blob_and_drills_in() {
        let mut state = fixture();
        insert(&mut state, "", page(&[], vec![blob("a.txt", 1)]));
        assert!(handle(Action::OpenSelected, &mut state));
        assert_eq!(state.view, View::StorageBlobDetail);
        assert_eq!(state.storage.selected_blob.as_deref(), Some("a.txt"));
    }

    #[test]
    fn enter_on_folder_descends_and_h_climbs_back_onto_it() {
        let mut state = fixture();
        insert(
            &mut state,
            "",
            page(&["app/", "img/"], vec![blob("readme.md", 1)]),
        );
        insert(&mut state, "img/", page(&[], vec![blob("img/cat.png", 1)]));
        // `/i` narrows the root to `img/` alone; Enter on it descends.
        state.storage.blobs_filter = tui_input::Input::default().with_value("i".to_string());
        assert_eq!(visible_names(&state), vec!["img/"]);
        state.storage.blobs_cursor = 0;

        assert!(handle(Action::OpenSelected, &mut state));
        assert_eq!(
            state.view,
            View::StorageBlobs,
            "folders don't open the preview"
        );
        assert_eq!(state.storage.blobs_prefix, "img/");
        assert_eq!(state.storage.blobs_cursor, 0);
        assert_eq!(
            state.storage.blobs_filter.value(),
            "",
            "the filter was scoped to the folder we left"
        );
        assert_eq!(visible_names(&state), vec!["cat.png"]);

        assert!(handle(Action::MoveLeft, &mut state));
        assert_eq!(state.storage.blobs_prefix, "");
        assert_eq!(
            state.storage.blobs_cursor, 1,
            "climbing out lands on the folder just left"
        );
    }

    #[test]
    fn esc_climbs_out_of_folder_before_leaving_view() {
        let mut state = fixture();
        state.storage.blobs_prefix = "app/2026/".into();
        assert!(handle(Action::Back, &mut state));
        assert_eq!(state.storage.blobs_prefix, "app/");
        assert!(handle(Action::Back, &mut state));
        assert_eq!(state.storage.blobs_prefix, "");
        assert!(
            !handle(Action::Back, &mut state),
            "at the root Esc is the breadcrumb's"
        );
    }

    #[test]
    fn right_descends_into_folder_but_not_into_blob() {
        let mut state = fixture();
        insert(&mut state, "", page(&["app/"], vec![blob("a.txt", 1)]));
        state.storage.blobs_cursor = 1;
        assert!(!handle(Action::MoveRight, &mut state));
        assert_eq!(state.storage.blobs_prefix, "");
        state.storage.blobs_cursor = 0;
        assert!(handle(Action::MoveRight, &mut state));
        assert_eq!(state.storage.blobs_prefix, "app/");
    }

    #[test]
    fn yank_returns_breadcrumb_path_for_blob_and_folder() {
        let mut state = fixture();
        insert(
            &mut state,
            "",
            page(&["dir/"], vec![blob("dir/file.json", 1)]),
        );
        assert_eq!(yank_text(&state).unwrap(), "acct1/logs/dir/");
        state.storage.blobs_cursor = 1;
        assert_eq!(yank_text(&state).unwrap(), "acct1/logs/dir/file.json");
    }

    #[test]
    fn filter_substring_match_is_case_insensitive() {
        // Substring (not prefix!) match on the display name: typing `log`
        // matches `2026-logs.txt` even though the name doesn't start with it.
        let mut state = fixture();
        insert(
            &mut state,
            "",
            page(
                &["catalog/"],
                vec![
                    blob("2026-logs.txt", 1),
                    blob("readme.md", 1),
                    blob("LOGFILE.bin", 1),
                ],
            ),
        );
        state.storage.blobs_filter = tui_input::Input::default().with_value("log".to_string());
        assert_eq!(
            visible_names(&state),
            vec!["catalog/", "2026-logs.txt", "LOGFILE.bin"]
        );
    }

    #[test]
    fn navigation_uses_filtered_length() {
        let mut state = fixture();
        insert(
            &mut state,
            "",
            page(
                &[],
                vec![
                    blob("alpha.txt", 1),
                    blob("beta.txt", 1),
                    blob("alphabet.txt", 1),
                ],
            ),
        );
        state.storage.blobs_filter = tui_input::Input::default().with_value("alpha".to_string());
        assert!(handle(Action::GotoBottom, &mut state));
        assert_eq!(
            state.storage.blobs_cursor, 1,
            "GotoBottom clamps to filtered len-1, not raw len-1",
        );
    }

    #[test]
    fn enter_after_filter_drills_into_filtered_row() {
        // Cursor row in the filtered slice must be the one drilled into,
        // not the same index into the raw blobs Vec.
        let mut state = fixture();
        insert(
            &mut state,
            "",
            page(
                &[],
                vec![blob("alpha", 1), blob("beta", 1), blob("zeta", 1)],
            ),
        );
        state.storage.blobs_filter = tui_input::Input::default().with_value("zeta".to_string());
        state.storage.blobs_cursor = 0;
        assert!(handle(Action::OpenSelected, &mut state));
        assert_eq!(state.view, View::StorageBlobDetail);
        assert_eq!(state.storage.selected_blob.as_deref(), Some("zeta"));
    }

    #[test]
    fn renders_filter_chip_in_title() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(120, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        insert(
            &mut state,
            "",
            page(&[], vec![blob("alpha.txt", 1), blob("beta.txt", 1)]),
        );
        state.storage.blobs_filter = tui_input::Input::default().with_value("al".to_string());
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(
            buf.contains("/al"),
            "title chip should show /al, got: {buf}"
        );
        assert!(
            buf.contains("1 of 2"),
            "title count should switch to `N of M` when filtering, got: {buf}",
        );
    }

    #[test]
    fn title_flags_partial_listing_and_loading_more() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(120, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        let mut p = page(&[], vec![blob("a", 1), blob("b", 1)]);
        p.next_marker = Some("tok".into());
        insert(&mut state, "", p);
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("· 2+"), "`+` marks a partial listing: {buf}");

        state
            .storage
            .blobs_loading_more
            .insert(StorageCache::blob_listing_key("acct1", "logs", ""));
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("loading more"), "got: {buf}");
    }

    #[test]
    fn scrolling_near_the_end_requests_next_page_only_when_more_exists() {
        let mut state = fixture();
        let blobs: Vec<Blob> = (0..100).map(|i| blob(&format!("b{i:03}"), 1)).collect();
        let mut p = page(&[], blobs.clone());
        p.next_marker = Some("tok".into());
        insert(&mut state, "", p);

        assert!(handle(Action::MoveDown, &mut state));
        assert!(!state.storage.blobs_fetch_more, "top of list: no prefetch");
        assert!(handle(Action::GotoBottom, &mut state));
        assert!(
            state.storage.blobs_fetch_more,
            "bottom: next page requested"
        );

        // Within PREFETCH_ROWS of the end counts too.
        state.storage.blobs_fetch_more = false;
        state.storage.blobs_cursor = 100 - PREFETCH_ROWS - 2;
        assert!(handle(Action::MoveDown, &mut state));
        assert!(state.storage.blobs_fetch_more);

        // Exhausted listing never asks.
        state.storage.blobs_fetch_more = false;
        insert(&mut state, "", page(&[], blobs));
        assert!(handle(Action::GotoBottom, &mut state));
        assert!(!state.storage.blobs_fetch_more);
    }

    #[test]
    fn filter_hiding_every_loaded_row_still_pulls_more() {
        let mut state = fixture();
        let mut p = page(&[], vec![blob("alpha", 1)]);
        p.next_marker = Some("tok".into());
        insert(&mut state, "", p);
        state.storage.blobs_filter = tui_input::Input::default().with_value("zzz".to_string());
        assert!(handle(Action::MoveDown, &mut state));
        assert!(state.storage.blobs_fetch_more);
    }

    #[test]
    fn tab_cycles_sort_column_and_i_inverts() {
        let mut state = fixture();
        insert(
            &mut state,
            "",
            page(
                &["z-folder/", "a-folder/"],
                vec![
                    blob("small.txt", 10),
                    blob("large.txt", 3000),
                    blob("medium.txt", 200),
                ],
            ),
        );
        // Default: name ascending, folders first.
        assert_eq!(
            visible_names(&state),
            vec![
                "a-folder/",
                "z-folder/",
                "large.txt",
                "medium.txt",
                "small.txt"
            ]
        );

        assert!(handle(Action::CycleSort, &mut state));
        assert_eq!(state.storage.blobs_sort.column, BlobSortColumn::Size);
        assert_eq!(
            visible_names(&state),
            vec![
                "a-folder/",
                "z-folder/",
                "small.txt",
                "medium.txt",
                "large.txt"
            ]
        );

        assert!(handle(Action::InvertSort, &mut state));
        assert!(state.storage.blobs_sort.descending);
        assert_eq!(
            visible_names(&state),
            vec![
                "a-folder/",
                "z-folder/",
                "large.txt",
                "medium.txt",
                "small.txt"
            ],
            "size descending keeps folders on top, by name"
        );

        // Back to NAME and invert: folders reverse too.
        assert!(handle(Action::CycleSortBack, &mut state));
        assert_eq!(state.storage.blobs_sort.column, BlobSortColumn::Name);
        assert!(
            !state.storage.blobs_sort.descending,
            "cycling resets direction"
        );
        assert!(handle(Action::InvertSort, &mut state));
        assert_eq!(
            visible_names(&state),
            vec![
                "z-folder/",
                "a-folder/",
                "small.txt",
                "medium.txt",
                "large.txt"
            ]
        );
    }

    #[test]
    fn sort_by_last_modified_orders_dates_with_missing_last() {
        use chrono::TimeZone;
        let mut state = fixture();
        let dated = |name: &str, day: u32| Blob {
            last_modified: Some(Utc.with_ymd_and_hms(2026, 6, day, 0, 0, 0).unwrap()),
            ..blob(name, 1)
        };
        insert(
            &mut state,
            "",
            page(
                &[],
                vec![
                    dated("mid", 5),
                    blob("undated", 1),
                    dated("new", 9),
                    dated("old", 1),
                ],
            ),
        );
        state.storage.blobs_sort.column = BlobSortColumn::LastModified;
        assert_eq!(visible_names(&state), vec!["undated", "old", "mid", "new"]);
        state.storage.blobs_sort.descending = true;
        assert_eq!(visible_names(&state), vec!["new", "mid", "old", "undated"]);
    }

    #[test]
    fn sorted_header_carries_direction_arrow() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(160, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        insert(&mut state, "", page(&[], vec![blob("a", 1)]));
        state.storage.blobs_sort.column = BlobSortColumn::Size;
        state.storage.blobs_sort.descending = true;
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("SIZE ▼"), "got: {buf}");
        assert!(!buf.contains("NAME ▲"), "only the active column: {buf}");
    }
}
