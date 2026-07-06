//! Storage blob detail panel. Reached from [`super::storage_blobs`] via Enter.
//! Renders the blob's [`BlobMetadata`] (content type, length, etag,
//! last-modified, MD5) as a header section, then the body preview:
//! [`BlobPreviewBody::Text`] in a scrollable `Paragraph`, or
//! [`BlobPreviewBody::Binary`] as a centered note.
//!
//! Mirrors the shape of [`super::apim_policy`] / [`super::logs_detail`]: a
//! breadcrumb row at the top (`account / container / blob/path`), a bordered
//! body block in the middle, and the standard footer hint at the bottom. The
//! preview is bounded at 64 KB by the backend caller — the view itself just
//! renders whatever it gets.

#![allow(dead_code, unused_variables)]

use chrono::{DateTime, Utc};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::azure::storage::{BlobMetadata, BlobPreview, BlobPreviewBody};
use crate::ui::events::Action;
use crate::ui::state::{AppState, StorageCache};
use crate::ui::theme::Theme;

const FOOTER_HINT: &str = "j/k scroll  g/G top/bottom  Esc back  r refresh  y yank  ? help  q quit";
const HALF_PAGE: u16 = 10;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    // Global breadcrumb (rendered by app::dispatch_view) replaces the old
    // in-view "Storage account / container / blob" header strip — body +
    // footer only here.
    //
    // Assume no scrollable content until the text-preview branch measures
    // some — keeps j/G no-ops (via the handler's `scroll_max` clamp) on the
    // placeholder / error / binary branches.
    state.scroll_max.set(0);
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(" blob ", Style::default().fg(theme.fg)));
    let inner = block.inner(chunks[0]);
    frame.render_widget(block, chunks[0]);

    let (Some(acc), Some(cont), Some(blob_name)) = (
        state.storage.selected_account.as_ref(),
        state.storage.selected_container.as_deref(),
        state.storage.selected_blob.as_deref(),
    ) else {
        let p = Paragraph::new(Line::from(Span::styled(
            "no blob selected.",
            Style::default().fg(theme.muted),
        )));
        frame.render_widget(p, inner);
        render_footer(frame, chunks[1], theme);
        return;
    };

    let key = StorageCache::blob_preview_key(&acc.name, cont, blob_name);

    if let Some(err) = state.storage.blob_preview_error.get(&key) {
        let p = Paragraph::new(Line::from(Span::styled(
            format!("error: {err}"),
            Style::default().fg(theme.critical),
        )));
        frame.render_widget(p, inner);
        render_footer(frame, chunks[1], theme);
        return;
    }

    let loading = state.storage.blob_preview_pending.contains(&key);
    match state.storage.blob_preview.get(&key) {
        None if loading => {
            let p = Paragraph::new(Line::from(Span::styled(
                "loading blob preview …",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, inner);
        }
        None => {
            let p = Paragraph::new(Line::from(Span::styled(
                "press r to load blob preview.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, inner);
        }
        Some(preview) => render_preview(frame, inner, preview, state, theme),
    }

    render_footer(frame, chunks[1], theme);
}

fn render_preview(
    frame: &mut Frame,
    area: Rect,
    preview: &BlobPreview,
    state: &AppState,
    theme: &Theme,
) {
    // Header block: 6 metadata lines + 1 blank separator. Always rendered at
    // the top so the metadata stays visible even while the user scrolls the
    // body below it.
    let meta_height: u16 = 7;
    if area.height <= meta_height {
        // Tiny pane — just dump the metadata, no body.
        let meta = Paragraph::new(metadata_lines(&preview.metadata, theme));
        frame.render_widget(meta, area);
        return;
    }

    let parts = Layout::vertical([Constraint::Length(meta_height), Constraint::Min(0)]).split(area);
    let meta = Paragraph::new(metadata_lines(&preview.metadata, theme));
    frame.render_widget(meta, parts[0]);

    match &preview.body {
        BlobPreviewBody::Text(s) => {
            let body_lines: Vec<Line> = s
                .lines()
                .map(|l| Line::from(Span::styled(l.to_string(), Style::default().fg(theme.fg))))
                .collect();
            // Ceiling in *wrapped* rows minus the viewport — an unwrapped
            // count leaves the tail of long wrapped lines unreachable.
            // Published through `scroll_max` so the key handler clamps the
            // stored offset too; G used to park it at a sentinel where `k`
            // stayed dead for thousands of presses.
            let total: usize = s
                .lines()
                .map(|l| super::detail::wrapped_line_count(l, parts[1].width))
                .sum();
            let max_scroll = total
                .saturating_sub(parts[1].height.max(1) as usize)
                .min(u16::MAX as usize) as u16;
            state.scroll_max.set(max_scroll);
            let clamped = state.storage.blob_preview_scroll.min(max_scroll);
            let p = Paragraph::new(body_lines)
                .scroll((clamped, 0))
                .wrap(Wrap { trim: false });
            frame.render_widget(p, parts[1]);
        }
        BlobPreviewBody::Binary { reason } => {
            let p = Paragraph::new(Line::from(Span::styled(
                reason.clone(),
                Style::default().fg(theme.muted),
            )))
            .alignment(Alignment::Center);
            // Vertically center the note by padding the top with empty lines.
            let pad_top = parts[1].height.saturating_sub(1) / 2;
            let body_area = Rect {
                x: parts[1].x,
                y: parts[1].y + pad_top,
                width: parts[1].width,
                height: parts[1].height.saturating_sub(pad_top),
            };
            frame.render_widget(p, body_area);
        }
    }
}

fn metadata_lines(meta: &BlobMetadata, theme: &Theme) -> Vec<Line<'static>> {
    vec![
        kv("type", meta.content_type.as_deref().unwrap_or("—"), theme),
        kv("size", &human_bytes(meta.content_length), theme),
        kv("etag", meta.etag.as_deref().unwrap_or("—"), theme),
        kv(
            "modified",
            &format_last_modified(meta.last_modified.as_ref()),
            theme,
        ),
        kv("md5", meta.content_md5.as_deref().unwrap_or("—"), theme),
        Line::from(""),
    ]
}

fn kv(key: &str, value: &str, theme: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{:<10} ", format!("{key}:")),
            Style::default().fg(theme.muted),
        ),
        Span::styled(value.to_string(), Style::default().fg(theme.fg)),
    ])
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
        Some(t) => t.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
        None => "—".to_string(),
    }
}

fn human_bytes(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if n >= GB {
        format!("{:.1} GB ({n} B)", n as f64 / GB as f64)
    } else if n >= MB {
        format!("{:.1} MB ({n} B)", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{:.1} KB ({n} B)", n as f64 / KB as f64)
    } else {
        format!("{n} B")
    }
}

pub fn handle(action: Action, state: &mut AppState) -> bool {
    // Every mutation clamps to the render-published `scroll_max` so the stored
    // offset never parks past the content. G used to store a 10_000 sentinel
    // (render clamped it for display only), which left `k` decrementing
    // invisibly for thousands of presses before the view moved.
    let max_scroll = state.scroll_max.get();
    match action {
        Action::MoveDown => {
            state.storage.blob_preview_scroll = state
                .storage
                .blob_preview_scroll
                .saturating_add(1)
                .min(max_scroll);
            true
        }
        Action::MoveUp => {
            state.storage.blob_preview_scroll = state
                .storage
                .blob_preview_scroll
                .min(max_scroll)
                .saturating_sub(1);
            true
        }
        Action::HalfPageDown => {
            state.storage.blob_preview_scroll = state
                .storage
                .blob_preview_scroll
                .saturating_add(HALF_PAGE)
                .min(max_scroll);
            true
        }
        Action::HalfPageUp => {
            state.storage.blob_preview_scroll = state
                .storage
                .blob_preview_scroll
                .min(max_scroll)
                .saturating_sub(HALF_PAGE);
            true
        }
        Action::GotoTop => {
            state.storage.blob_preview_scroll = 0;
            true
        }
        Action::GotoBottom => {
            state.storage.blob_preview_scroll = max_scroll;
            true
        }
        _ => false,
    }
}

/// Plain-text yank payload for the displayed blob: the metadata as `key:
/// value` lines followed by the text body (or a placeholder for binary blobs).
pub fn yank_text(state: &AppState) -> Option<String> {
    let acc = state.storage.selected_account.as_ref()?;
    let container = state.storage.selected_container.as_deref()?;
    let blob = state.storage.selected_blob.as_deref()?;
    let key = StorageCache::blob_preview_key(&acc.name, container, blob);
    let preview = state.storage.blob_preview.get(&key)?;

    let mut s = String::new();
    s.push_str(&format!("path: {}/{}/{}\n", acc.name, container, blob));
    if let Some(t) = preview.metadata.content_type.as_deref() {
        s.push_str(&format!("content-type: {t}\n"));
    }
    s.push_str(&format!(
        "content-length: {}\n",
        preview.metadata.content_length
    ));
    if let Some(e) = preview.metadata.etag.as_deref() {
        s.push_str(&format!("etag: {e}\n"));
    }
    if let Some(lm) = preview.metadata.last_modified.as_ref() {
        s.push_str(&format!(
            "last-modified: {}\n",
            lm.format("%Y-%m-%dT%H:%M:%SZ")
        ));
    }
    if let Some(md5) = preview.metadata.content_md5.as_deref() {
        s.push_str(&format!("content-md5: {md5}\n"));
    }
    s.push('\n');
    match &preview.body {
        BlobPreviewBody::Text(text) => s.push_str(text),
        BlobPreviewBody::Binary { reason } => s.push_str(&format!("[{reason}]")),
    }
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::storage::{BlobMetadata, BlobPreview, BlobPreviewBody, StorageAccount};
    use crate::config::Config;
    use crate::ui::state::View;
    use chrono::{TimeZone, Utc};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn fixture() -> AppState {
        let mut state = AppState::new(Config::default());
        state.view = View::StorageBlobDetail;
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
        state.storage.selected_blob = Some("hello.txt".into());
        state
    }

    fn preview_text(body: &str) -> BlobPreview {
        BlobPreview {
            metadata: BlobMetadata {
                content_type: Some("text/plain".into()),
                content_length: body.len() as u64,
                etag: Some("0xABC".into()),
                last_modified: Some(Utc.with_ymd_and_hms(2026, 5, 20, 12, 0, 0).unwrap()),
                content_md5: Some("dummy==".into()),
            },
            body: BlobPreviewBody::Text(body.to_string()),
        }
    }

    fn preview_binary() -> BlobPreview {
        BlobPreview {
            metadata: BlobMetadata {
                content_type: Some("image/png".into()),
                content_length: 1_500_000,
                etag: None,
                last_modified: None,
                content_md5: None,
            },
            body: BlobPreviewBody::Binary {
                reason: "binary content (image/png, 1.4 MB)".into(),
            },
        }
    }

    #[test]
    fn renders_text_preview_with_metadata() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(120, 20);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        let key = StorageCache::blob_preview_key("acct1", "logs", "hello.txt");
        state
            .storage
            .blob_preview
            .insert(key, preview_text("hello world"));

        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("hello world"));
        assert!(s.contains("text/plain"));
        assert!(s.contains("0xABC"));
    }

    #[test]
    fn renders_binary_preview_note() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(120, 20);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        let key = StorageCache::blob_preview_key("acct1", "logs", "hello.txt");
        state.storage.blob_preview.insert(key, preview_binary());

        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("binary content"));
        assert!(s.contains("image/png"));
    }

    #[test]
    fn renders_loading_state() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(120, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        let key = StorageCache::blob_preview_key("acct1", "logs", "hello.txt");
        state.storage.blob_preview_pending.insert(key);

        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("loading blob preview"));
    }

    #[test]
    fn renders_error_state() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(120, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        let key = StorageCache::blob_preview_key("acct1", "logs", "hello.txt");
        state
            .storage
            .blob_preview_error
            .insert(key, "403 forbidden".into());

        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("403 forbidden"));
    }

    #[test]
    fn handle_scrolls_clamped_at_max() {
        let mut state = fixture();
        // Handlers clamp to the render-published max; simulate a rendered
        // preview with content below the fold.
        state.scroll_max.set(40);
        assert!(handle(Action::MoveDown, &mut state));
        assert_eq!(state.storage.blob_preview_scroll, 1);
        assert!(handle(Action::HalfPageDown, &mut state));
        assert_eq!(state.storage.blob_preview_scroll, 1 + HALF_PAGE);
        assert!(handle(Action::GotoBottom, &mut state));
        assert_eq!(state.storage.blob_preview_scroll, 40);
        assert!(handle(Action::GotoTop, &mut state));
        assert_eq!(state.storage.blob_preview_scroll, 0);
        assert!(handle(Action::MoveUp, &mut state));
        assert_eq!(state.storage.blob_preview_scroll, 0); // saturates at 0
    }

    #[test]
    fn render_survives_goto_bottom_sentinel() {
        // Regression guard mirroring logs_detail's safety net: scroll values up
        // to u16::MAX must not overflow ratatui's Paragraph arithmetic.
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(100, 20);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        let key = StorageCache::blob_preview_key("acct1", "logs", "hello.txt");
        state
            .storage
            .blob_preview
            .insert(key, preview_text("line\nline\n"));
        for s in [10_000, u16::MAX] {
            state.storage.blob_preview_scroll = s;
            term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        }
    }

    #[test]
    fn goto_bottom_then_move_up_responds_immediately() {
        // Regression: G stored a 10_000 sentinel while only the renderer
        // clamped it for display, so `k` decremented invisibly for thousands
        // of presses. After a render, G must land exactly on the content's
        // max scroll and the very next `k` must move the view.
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(100, 16);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        let key = StorageCache::blob_preview_key("acct1", "logs", "hello.txt");
        let body: String = (0..40).map(|i| format!("line {i}\n")).collect();
        state.storage.blob_preview.insert(key, preview_text(&body));
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();

        assert!(handle(Action::GotoBottom, &mut state));
        let max = state.scroll_max.get();
        assert!(
            max > 0 && max < 40,
            "expected a small real max scroll, got {max}"
        );
        assert_eq!(state.storage.blob_preview_scroll, max);
        assert!(handle(Action::MoveUp, &mut state));
        assert_eq!(state.storage.blob_preview_scroll, max - 1);
    }

    #[test]
    fn yank_text_includes_metadata_and_body() {
        let mut state = fixture();
        let key = StorageCache::blob_preview_key("acct1", "logs", "hello.txt");
        state
            .storage
            .blob_preview
            .insert(key, preview_text("body content"));
        let s = yank_text(&state).expect("expected yank text");
        assert!(s.contains("path: acct1/logs/hello.txt"));
        assert!(s.contains("content-type: text/plain"));
        assert!(s.contains("body content"));
    }
}
