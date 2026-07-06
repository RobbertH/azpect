//! Storage blobs drill-in: lists the blobs in the pinned
//! `(selected_account, selected_container)` pair, optionally filtered by a
//! case-insensitive substring match on the blob name. Enter on a row pins the
//! blob name and opens [`View::StorageBlobDetail`].
//!
//! The filter input has the same focus-then-commit lifecycle as the
//! storage-accounts / storage-containers views: `/` activates it, Enter
//! commits and deactivates (the filter value persists), Esc cancels and
//! clears. Matching is purely client-side over the full container — no
//! server-side prefix is sent to the Blob REST API.

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

const FOOTER_HINT: &str =
    "j/k move  Enter preview  / filter  Esc back  r refresh  y yank name  ? help  q quit";
const HALF_PAGE: usize = 10;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    // Global breadcrumb (rendered by app::dispatch_view) replaces the old
    // in-view "Storage account / container · prefix /…" header — body +
    // footer only here. The filter indicator lives in the block title.
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);

    // Title shows total + `· N of M ` while filtering and a `/{filter}` chip
    // when the filter value is non-empty. Mirrors the accounts/containers
    // views.
    let filter_value = state.storage.blobs_filter.value();
    let filter_active = state.storage.blobs_filter_active;
    let (total, filtered_len) = match (
        state.storage.selected_account.as_ref(),
        state.storage.selected_container.as_deref(),
    ) {
        (Some(acc), Some(cont)) => {
            let key = StorageCache::blobs_key(&acc.name, cont);
            let total = state.storage.blobs.get(&key).map(|v| v.len());
            let filtered_len = state.storage.filtered_blobs(&acc.name, cont).len();
            (total, filtered_len)
        }
        _ => (None, 0),
    };
    let count_label = match total {
        Some(t) if !filter_value.is_empty() => format!("· {} of {} ", filtered_len, t),
        Some(t) => format!("· {t} "),
        None => String::new(),
    };
    let mut title_spans: Vec<Span> = vec![
        Span::styled(" blobs ", Style::default().fg(theme.fg)),
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

    let (Some(acc), Some(cont)) = (
        state.storage.selected_account.as_ref(),
        state.storage.selected_container.as_deref(),
    ) else {
        let p = Paragraph::new(Line::from(Span::styled(
            "no container selected.",
            Style::default().fg(theme.muted),
        )));
        frame.render_widget(p, body_area);
        render_footer(frame, chunks[1], theme);
        return;
    };

    let key = StorageCache::blobs_key(&acc.name, cont);

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

    let blobs = state.storage.blobs.get(&key);
    let loading = state.storage.blobs_pending.contains(&key);
    let filtered = state.storage.filtered_blobs(&acc.name, cont);
    match blobs {
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
        Some(rows) if rows.is_empty() => {
            let p = Paragraph::new(Line::from(Span::styled(
                "Container is empty.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(_) if filtered.is_empty() => {
            // Underlying list is non-empty but the active filter matched
            // nothing. Mirrors the accounts/containers no-match copy.
            let p = Paragraph::new(Line::from(Span::styled(
                "no blobs match the current filter.",
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

            let header_row = Row::new(vec![
                Cell::from("NAME"),
                Cell::from(Line::from("SIZE").alignment(Alignment::Right)),
                Cell::from("CONTENT-TYPE"),
                Cell::from("LAST MODIFIED"),
                Cell::from("TYPE"),
            ])
            .style(
                Style::default()
                    .fg(theme.muted)
                    .add_modifier(Modifier::BOLD),
            );

            let body_rows: Vec<Row> = filtered
                .iter()
                .map(|b| {
                    Row::new(vec![
                        Cell::from(b.name.as_str()).style(Style::default().fg(theme.fg)),
                        // Right-align inside the cell so digits line up with
                        // the right-aligned `SIZE` header.
                        Cell::from(Line::from(human_bytes(b.size)).alignment(Alignment::Right))
                            .style(Style::default().fg(theme.accent)),
                        Cell::from(b.content_type.as_deref().unwrap_or("—").to_string())
                            .style(Style::default().fg(theme.muted)),
                        Cell::from(format_last_modified(b.last_modified.as_ref()))
                            .style(Style::default().fg(theme.muted)),
                        Cell::from(b.blob_type.as_str()).style(Style::default().fg(theme.muted)),
                    ])
                })
                .collect();

            let cursor = state.storage.blobs_cursor.min(filtered.len() - 1);
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

    // Navigation operates on the filtered slice so the cursor never points
    // past the end of what's rendered. Mirrors `storage_accounts::handle`.
    let len = state.storage.filtered_blobs(&acc_name, &container).len();

    // While the filter input has focus, swallow most actions but let the
    // dispatcher's filter-forwarding gate push raw chars into the buffer.
    // Esc cancels (deactivates AND clears); Enter commits (deactivates,
    // keeps the value). Down hands focus back to the filtered list.
    if state.storage.blobs_filter_active {
        match action {
            Action::Back => {
                state.storage.blobs_filter_active = false;
                state.storage.blobs_filter.reset();
                state.storage.blobs_cursor = 0;
                return true;
            }
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

    match action {
        Action::MoveDown => {
            if len > 0 {
                state.storage.blobs_cursor = (state.storage.blobs_cursor + 1).min(len - 1);
            }
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
            true
        }
        Action::StartSearch => {
            state.storage.blobs_filter_active = true;
            true
        }
        Action::OpenSelected => {
            // Resolve via the filtered slice so the cursor's row matches what
            // the user actually sees on screen.
            let blob_name = state
                .storage
                .filtered_blobs(&acc_name, &container)
                .get(state.storage.blobs_cursor)
                .map(|b| b.name.clone());
            if let Some(name) = blob_name {
                state.storage.selected_blob = Some(name);
                state.storage.blob_preview_scroll = 0;
                state.view = View::StorageBlobDetail;
            }
            true
        }
        _ => false,
    }
}

/// Public helper used by `app.rs::yank_target` so `y` in this view copies the
/// currently-highlighted blob name (or, if no rows are loaded, the breadcrumb
/// pair `account/container`). The cursor indexes into the filtered slice so
/// `y` always matches the row the user sees on screen.
pub fn yank_text(state: &AppState) -> Option<String> {
    let acc_name = state
        .storage
        .selected_account
        .as_ref()
        .map(|a| a.name.clone())?;
    let container = state.storage.selected_container.clone()?;
    let filtered = state.storage.filtered_blobs(&acc_name, &container);
    let line = filtered
        .get(state.storage.blobs_cursor)
        .map(|b| format!("{}/{}/{}", acc_name, container, b.name));
    line.or_else(|| Some(format!("{acc_name}/{container}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::storage::{Blob, StorageAccount};
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

    #[test]
    fn renders_column_headers() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(160, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        let key = StorageCache::blobs_key("acct1", "logs");
        state
            .storage
            .blobs
            .insert(key, vec![blob("hello.txt", 2048)]);
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
        let key = StorageCache::blobs_key("acct1", "logs");
        state
            .storage
            .blobs
            .insert(key, vec![blob("hello.txt", 2048)]);
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
        let key = StorageCache::blobs_key("acct1", "logs");
        state.storage.blobs.insert(key, Vec::new());
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("Container is empty"));
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
    fn enter_on_row_pins_blob_and_drills_in() {
        let mut state = fixture();
        let key = StorageCache::blobs_key("acct1", "logs");
        state.storage.blobs.insert(key, vec![blob("a.txt", 1)]);
        assert!(handle(Action::OpenSelected, &mut state));
        assert_eq!(state.view, View::StorageBlobDetail);
        assert_eq!(state.storage.selected_blob.as_deref(), Some("a.txt"));
    }

    #[test]
    fn yank_returns_breadcrumb_path_for_blob() {
        let mut state = fixture();
        let key = StorageCache::blobs_key("acct1", "logs");
        state
            .storage
            .blobs
            .insert(key, vec![blob("dir/file.json", 1)]);
        let yanked = yank_text(&state).unwrap();
        assert_eq!(yanked, "acct1/logs/dir/file.json");
    }

    #[test]
    fn filter_substring_match_is_case_insensitive() {
        // Substring (not prefix!) match: typing `log` matches
        // `app/logs/2026.txt` even though the blob name doesn't start with it.
        let mut state = fixture();
        let key = StorageCache::blobs_key("acct1", "logs");
        state.storage.blobs.insert(
            key,
            vec![
                blob("app/logs/2026.txt", 1),
                blob("readme.md", 1),
                blob("LOGFILE.bin", 1),
            ],
        );
        state.storage.blobs_filter = tui_input::Input::default().with_value("log".to_string());
        let names: Vec<&str> = state
            .storage
            .filtered_blobs("acct1", "logs")
            .iter()
            .map(|b| b.name.as_str())
            .collect();
        assert_eq!(names, vec!["app/logs/2026.txt", "LOGFILE.bin"]);
    }

    #[test]
    fn navigation_uses_filtered_length() {
        let mut state = fixture();
        let key = StorageCache::blobs_key("acct1", "logs");
        state.storage.blobs.insert(
            key,
            vec![
                blob("alpha.txt", 1),
                blob("beta.txt", 1),
                blob("alphabet.txt", 1),
            ],
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
        let key = StorageCache::blobs_key("acct1", "logs");
        state.storage.blobs.insert(
            key,
            vec![blob("alpha", 1), blob("beta", 1), blob("zeta", 1)],
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
        let key = StorageCache::blobs_key("acct1", "logs");
        state
            .storage
            .blobs
            .insert(key, vec![blob("alpha.txt", 1), blob("beta.txt", 1)]);
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
}
