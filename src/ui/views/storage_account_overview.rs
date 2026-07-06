//! Storage account overview drill-in: aggregate stats for the pinned storage
//! account, modeled after the Azure portal's "Storage browser → account
//! overview" panel.
//!
//! Sits between [`View::StorageAccounts`] and [`View::StorageContainers`] —
//! Enter from accounts opens this view; Enter here opens the containers list.
//! Esc walks back to accounts via the semantic-parent chain.
//!
//! Data sources: five concurrent Azure Monitor metrics calls (account scope
//! plus blob / file / queue / table services). The same data the portal shows,
//! which means daily-resolution metrics with a 1-2 day reporting lag — the
//! footer call-out makes that explicit so users don't read the numbers as
//! real-time.

#![allow(dead_code, unused_variables)]

use chrono::{DateTime, Utc};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::azure::storage::StorageAccountStats;
use crate::ui::events::Action;
use crate::ui::state::{AppState, View};
use crate::ui::theme::Theme;

const FOOTER_HINT: &str = "Enter containers  Esc back  r refresh  ? help  q quit";
const LAG_NOTE: &str = "Data updated 2-4× daily by Azure Monitor, may lag up to 24h.";

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);

    let Some(account) = state.storage.selected_account.as_ref() else {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.border))
            .title(Span::styled(" overview ", Style::default().fg(theme.fg)));
        let inner = block.inner(chunks[0]);
        frame.render_widget(block, chunks[0]);
        let p = Paragraph::new(Line::from(Span::styled(
            "no storage account selected.",
            Style::default().fg(theme.muted),
        )));
        frame.render_widget(p, inner);
        render_footer(frame, chunks[1], theme);
        return;
    };

    // The "as of <timestamp>" goes in the title, right-aligned. ratatui's
    // Block::title_top with alignment fits this exactly.
    let stats = state.storage.overview_stats.get(&account.id);
    let pending = state.storage.overview_pending.contains(&account.id);
    let error = state.storage.overview_error.get(&account.id);

    let title_left = vec![
        Span::styled(" overview ", Style::default().fg(theme.fg)),
        Span::styled(
            format!("· {} ", account.name),
            Style::default().fg(theme.muted),
        ),
    ];
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Line::from(title_left));
    if let Some(ts) = stats.and_then(|s| s.as_of.as_ref()) {
        block = block.title_top(
            Line::from(Span::styled(
                format!(" as of {} ", format_as_of(ts)),
                Style::default().fg(theme.muted),
            ))
            .alignment(Alignment::Right),
        );
    }
    let inner = block.inner(chunks[0]);
    frame.render_widget(block, chunks[0]);

    if let Some(err) = error {
        let p = Paragraph::new(Line::from(Span::styled(
            format!("error: {err}"),
            Style::default().fg(theme.critical),
        )));
        frame.render_widget(p, inner);
        render_footer(frame, chunks[1], theme);
        return;
    }

    let Some(stats) = stats else {
        let msg = if pending {
            "loading stats …"
        } else {
            "press r to load stats."
        };
        let p = Paragraph::new(Line::from(Span::styled(
            msg,
            Style::default().fg(theme.muted),
        )));
        frame.render_widget(p, inner);
        render_footer(frame, chunks[1], theme);
        return;
    };

    render_body(frame, inner, account.name.as_str(), stats, theme);
    render_footer(frame, chunks[1], theme);
}

/// Lay out the four service tiles plus the header / footer copy.
///
/// Vertical layout:
///   - header lines (account name + used-capacity)
///   - 2x2 grid of tiles (Blobs | File shares / Queues | Tables)
///   - blank spacer
///   - muted "data updated 2-4× daily" note
fn render_body(
    frame: &mut Frame,
    area: Rect,
    account_name: &str,
    stats: &StorageAccountStats,
    theme: &Theme,
) {
    if area.height < 3 || area.width < 10 {
        return;
    }
    // Header takes 2 lines, lag note takes 1, leave the rest for the grid.
    let parts = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(area);

    // --- Header lines ------------------------------------------------------
    let header_lines = vec![
        Line::from(vec![
            Span::styled("Account:       ", Style::default().fg(theme.muted)),
            Span::styled(
                account_name.to_string(),
                Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("Used capacity: ", Style::default().fg(theme.muted)),
            Span::styled(
                opt_bytes(stats.used_capacity_bytes),
                Style::default().fg(theme.fg),
            ),
        ]),
    ];
    frame.render_widget(Paragraph::new(header_lines), parts[0]);

    // --- 2x2 grid of tiles -------------------------------------------------
    // Use 50/50 columns; each column splits into top/bottom tiles. Tiles are
    // sized to take exactly 6 rows when possible — header + 3 stat lines +
    // border padding. If the grid area is shorter we just let Min(0) absorb.
    let cols = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(parts[1]);
    let left_tiles = Layout::vertical([Constraint::Length(6), Constraint::Min(0)]).split(cols[0]);
    let right_tiles = Layout::vertical([Constraint::Length(6), Constraint::Min(0)]).split(cols[1]);

    render_tile(
        frame,
        left_tiles[0],
        " Blobs ",
        &[
            ("Containers", opt_count(stats.container_count)),
            ("Blobs", opt_count(stats.blob_count)),
            ("Data", opt_bytes(stats.blob_capacity_bytes)),
        ],
        theme,
    );
    render_tile(
        frame,
        right_tiles[0],
        " File shares ",
        &[
            ("File shares", opt_count(stats.file_share_count)),
            ("Files", opt_count(stats.file_count)),
            ("Data", opt_bytes(stats.file_capacity_bytes)),
        ],
        theme,
    );
    render_tile(
        frame,
        left_tiles[1],
        " Queues ",
        &[
            ("Queues", opt_count(stats.queue_count)),
            ("Messages", opt_count(stats.queue_message_count)),
            ("Data", opt_bytes(stats.queue_capacity_bytes)),
        ],
        theme,
    );
    render_tile(
        frame,
        right_tiles[1],
        " Tables ",
        &[
            ("Tables", opt_count(stats.table_count)),
            ("Entities", opt_count(stats.table_entity_count)),
            ("Data", opt_bytes(stats.table_capacity_bytes)),
        ],
        theme,
    );

    // --- Lag note ----------------------------------------------------------
    let note = Paragraph::new(Line::from(Span::styled(
        LAG_NOTE,
        Style::default().fg(theme.muted),
    )))
    .alignment(Alignment::Left);
    frame.render_widget(note, parts[2]);
}

/// Render one stat tile: a bordered box with a title and `label: value` rows.
/// `rows` is rendered with the labels left-aligned and values right-aligned —
/// matches the visual rhythm of the portal's tiles.
fn render_tile(frame: &mut Frame, area: Rect, title: &str, rows: &[(&str, String)], theme: &Theme) {
    if area.height < 3 || area.width < 10 {
        return;
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(
            title.to_string(),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    // Find the longest label so we can pad once and let values align cleanly.
    let label_w = rows
        .iter()
        .map(|(l, _)| l.chars().count())
        .max()
        .unwrap_or(0);
    let lines: Vec<Line> = rows
        .iter()
        .map(|(label, value)| {
            Line::from(vec![
                Span::styled(
                    format!("{label:<width$} ", label = label, width = label_w),
                    Style::default().fg(theme.muted),
                ),
                Span::styled(value.clone(), Style::default().fg(theme.fg)),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_footer(frame: &mut Frame, area: Rect, theme: &Theme) {
    let p = Paragraph::new(Line::from(Span::styled(
        FOOTER_HINT,
        Style::default().fg(theme.muted),
    )));
    frame.render_widget(p, area);
}

/// `YYYY-MM-DD HH:MM UTC`. Compact enough to fit comfortably in the block
/// title strip on a 60-col terminal while still naming the timezone.
fn format_as_of(ts: &DateTime<Utc>) -> String {
    ts.format("%Y-%m-%d %H:%M UTC").to_string()
}

/// Convert an `Option<u64>` byte count to a display string, rendering missing
/// data as `—`. Binary units (KiB / MiB / GiB / TiB / PiB) match the portal's
/// "1.5 GiB" shape — easier to cross-check than base-10 GB.
pub(crate) fn opt_bytes(n: Option<u64>) -> String {
    match n {
        Some(n) => human_bytes(n),
        None => "—".to_string(),
    }
}

/// Convert an `Option<u64>` integer count to a display string. SI suffixes
/// (k / M / B) so a 4_360_000 blob count reads as `4.36 M` instead of
/// `4360000` and doesn't blow out the tile width.
pub(crate) fn opt_count(n: Option<u64>) -> String {
    match n {
        Some(n) => human_count(n),
        None => "—".to_string(),
    }
}

/// Binary-prefix byte formatter. Goes up to PiB — enough for any Azure
/// storage account; multi-exabyte accounts simply don't exist on the platform.
pub(crate) fn human_bytes(n: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    const TIB: u64 = GIB * 1024;
    const PIB: u64 = TIB * 1024;
    if n >= PIB {
        format!("{:.2} PiB", n as f64 / PIB as f64)
    } else if n >= TIB {
        format!("{:.2} TiB", n as f64 / TIB as f64)
    } else if n >= GIB {
        format!("{:.2} GiB", n as f64 / GIB as f64)
    } else if n >= MIB {
        format!("{:.2} MiB", n as f64 / MIB as f64)
    } else if n >= KIB {
        format!("{:.2} KiB", n as f64 / KIB as f64)
    } else {
        format!("{n} B")
    }
}

/// SI-suffix integer formatter — k for thousands, M for millions, B for
/// billions. Two significant decimals so the value is informative without
/// being noisy ("4.36 M" rather than "4.4 M" or "4360000").
pub(crate) fn human_count(n: u64) -> String {
    const K: u64 = 1_000;
    const M: u64 = 1_000_000;
    const B: u64 = 1_000_000_000;
    if n >= B {
        format!("{:.2} B", n as f64 / B as f64)
    } else if n >= M {
        format!("{:.2} M", n as f64 / M as f64)
    } else if n >= K {
        format!("{:.2} k", n as f64 / K as f64)
    } else {
        n.to_string()
    }
}

pub fn handle(action: Action, state: &mut AppState) -> bool {
    // Most actions fall through to the global handler (Refresh, Yank, Browser,
    // Back, Help, etc.). The only thing this view consumes locally is the
    // forward-drill Enter, which transitions to the containers list while
    // keeping the pinned account intact.
    match action {
        Action::OpenSelected => {
            if state.storage.selected_account.is_some() {
                state.storage.containers_cursor = 0;
                state.storage.containers_filter = tui_input::Input::default();
                state.view = View::StorageContainers;
            }
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::storage::StorageAccount;
    use crate::config::Config;
    use chrono::TimeZone;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn account_fixture() -> StorageAccount {
        StorageAccount {
            id: "/subs/x/rg/y/sa/myacct".into(),
            name: "myacct".into(),
            resource_group: "rg".into(),
            subscription_id: "sub".into(),
            location: "westeurope".into(),
            kind: Some("StorageV2".into()),
            sku: Some("Standard_GRS".into()),
            access_tier: Some("Hot".into()),
            is_hns_enabled: Some(false),
            https_only: Some(true),
            allow_blob_public_access: Some(false),
            created_at: None,
        }
    }

    fn fixture() -> AppState {
        let mut state = AppState::new(Config::default());
        state.view = View::StorageAccountOverview;
        state.storage.selected_account = Some(account_fixture());
        state
    }

    fn full_stats() -> StorageAccountStats {
        StorageAccountStats {
            used_capacity_bytes: Some(5_012_316_192_768), // ~4.56 TiB
            container_count: Some(49),
            blob_count: Some(4_360_000),
            blob_capacity_bytes: Some(5_012_316_192_768),
            file_share_count: Some(1),
            file_count: Some(9),
            file_capacity_bytes: Some(789_504), // ~771 KiB
            queue_count: Some(3),
            queue_message_count: Some(360),
            queue_capacity_bytes: Some(662_528), // ~647 KiB
            table_count: Some(26),
            table_entity_count: Some(16_240),
            table_capacity_bytes: Some(6_375_342), // ~6.08 MiB
            as_of: Utc.with_ymd_and_hms(2026, 5, 20, 14, 0, 0).single(),
        }
    }

    #[test]
    fn human_bytes_uses_binary_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(900), "900 B");
        assert_eq!(human_bytes(1024), "1.00 KiB");
        assert_eq!(human_bytes(1_572_864), "1.50 MiB");
        // 5_012_316_192_768 ÷ 2^40 ≈ 4.56
        let tib = human_bytes(5_012_316_192_768);
        assert!(tib.ends_with(" TiB"), "got {tib}");
        assert!(tib.starts_with("4.5"), "expected ~4.56 TiB, got {tib}");
    }

    #[test]
    fn human_count_uses_si_suffixes() {
        assert_eq!(human_count(0), "0");
        assert_eq!(human_count(999), "999");
        assert_eq!(human_count(1_500), "1.50 k");
        // 4_360_000 → 4.36 M, the actual portal-shape number from the brief.
        assert_eq!(human_count(4_360_000), "4.36 M");
        assert_eq!(human_count(2_500_000_000), "2.50 B");
    }

    #[test]
    fn renders_loading_state() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(80, 20);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.storage.overview_pending.insert(account_fixture().id);
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("loading stats"));
    }

    #[test]
    fn renders_error_state_verbatim() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(80, 20);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state
            .storage
            .overview_error
            .insert(account_fixture().id, "401 unauthorized".into());
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(
            buf.contains("401 unauthorized"),
            "error message should surface verbatim, got: {buf}",
        );
    }

    #[test]
    fn renders_all_four_tiles_with_stats() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(100, 24);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state
            .storage
            .overview_stats
            .insert(account_fixture().id, full_stats());
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        // Tile headers
        assert!(buf.contains("Blobs"), "Blobs tile missing: {buf}");
        assert!(
            buf.contains("File shares"),
            "File shares tile missing: {buf}",
        );
        assert!(buf.contains("Queues"), "Queues tile missing: {buf}");
        assert!(buf.contains("Tables"), "Tables tile missing: {buf}");
        // Specific values from the brief.
        assert!(buf.contains("4.36 M"), "blob count missing: {buf}");
        assert!(buf.contains("Containers"), "Containers row missing");
        assert!(buf.contains("Entities"), "Entities row missing");
        // Lag note.
        assert!(
            buf.contains("Azure Monitor"),
            "lag note should be in body: {buf}",
        );
    }

    #[test]
    fn renders_missing_values_as_em_dash() {
        // Account with file/queue/table services disabled — only blob stats
        // came back. Each missing field must render `—` rather than `0`.
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(100, 24);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        let mut partial = full_stats();
        partial.file_share_count = None;
        partial.file_count = None;
        partial.file_capacity_bytes = None;
        partial.queue_count = None;
        partial.queue_message_count = None;
        partial.queue_capacity_bytes = None;
        partial.table_count = None;
        partial.table_entity_count = None;
        partial.table_capacity_bytes = None;
        state
            .storage
            .overview_stats
            .insert(account_fixture().id, partial);
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(
            buf.contains("—"),
            "expected em-dash placeholder, got: {buf}"
        );
        // Blob values still present.
        assert!(buf.contains("4.36 M"));
    }

    #[test]
    fn renders_as_of_timestamp_in_title_strip() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(100, 24);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state
            .storage
            .overview_stats
            .insert(account_fixture().id, full_stats());
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(
            buf.contains("as of 2026-05-20"),
            "expected `as of …` in title strip, got: {buf}",
        );
    }

    #[test]
    fn enter_drills_into_containers() {
        let mut state = fixture();
        state.storage.containers_filter =
            tui_input::Input::default().with_value("stale".to_string());
        assert!(handle(Action::OpenSelected, &mut state));
        assert_eq!(state.view, View::StorageContainers);
        assert_eq!(
            state
                .storage
                .selected_account
                .as_ref()
                .map(|a| a.name.as_str()),
            Some("myacct"),
        );
        // Drilling in must not carry a stale containers filter along.
        assert_eq!(state.storage.containers_filter.value(), "");
    }

    #[test]
    fn handle_passes_unhandled_actions_to_global() {
        // Anything other than OpenSelected must NOT be consumed locally so the
        // global handler can route Refresh / Yank / Back / Help / etc.
        let mut state = fixture();
        for a in [
            Action::Refresh,
            Action::Yank,
            Action::Back,
            Action::Help,
            Action::OpenInBrowser,
            Action::MoveDown,
        ] {
            assert!(
                !handle(a, &mut state),
                "{:?} must fall through to globals",
                a
            );
        }
    }
}
