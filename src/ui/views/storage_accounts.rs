//! Top-level Storage mode entry view: lists all blob storage accounts visible
//! to the current subscription scope. Pressing Enter on a row pins the account
//! into `state.storage.selected_account` and opens [`View::StorageContainers`].

#![allow(dead_code, unused_variables)]

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};
use ratatui::Frame;

use crate::azure::storage::StorageAccount;
use crate::ui::events::Action;
use crate::ui::state::{subscription_display_name, AppState, View};
use crate::ui::theme::Theme;

const FOOTER_HINT: &str =
    "j/k move  Enter overview  / filter  Esc back  r refresh  y yank id  ? help  q quit";
const HALF_PAGE: usize = 10;

/// Above this width, the SUB ID column has enough room to spell out the full
/// GUID (36 chars) without crowding NAME / RESOURCE GROUP. Picked to leave
/// breathing room for the other fixed columns plus the column-spacing gaps.
const WIDE_SUB_THRESHOLD: u16 = 160;

// --- column widths ---------------------------------------------------------
//
// Single source of truth: the same `Constraint` array is fed to both
// `Table::widths` *and* `Table::header`. The widget renders the header strip
// using its own column layout, so headers can no longer drift from row cells.

// NAME is sized to the longest actual name in the data at render time so it
// never truncates. Floor at the header label width.
const NAME_W_MIN: u16 = 4; // "NAME"
const LOCATION_W: u16 = 14;
const KIND_W: u16 = 16;
const SKU_W: u16 = 14;
const ACCESS_TIER_W: u16 = 6; // header "TIER" (4) + slack for "Hot"/"Cool"
const HNS_W: u16 = 5;
const HTTPS_W: u16 = 6;
const PUBLIC_BLOB_W: u16 = 8; // header "PUBLIC" (6) + slack for "allowed"/"blocked"
const RG_W_MIN: u16 = 22;
const CREATED_W: u16 = 10; // "YYYY-MM-DD"
const SUB_NAME_W: u16 = 22; // typical org sub-name length (e.g. "prod-shared-services")
const SUB_SHORT_W: u16 = 12; // "01234567…"
const SUB_FULL_W: u16 = 36; // full GUID

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);

    // Block title shows total count plus a `· N of M ` ratio whenever the
    // filter is narrowing the set, and a `/{filter}` chip when there's a
    // value. Mirrors the resource list view's title layout.
    let filter_value = state.storage.accounts_filter.value();
    let filter_active = state.storage.accounts_filter_active;
    let total = state.storage.accounts.as_ref().map(|v| v.len());
    let filtered = state.storage.filtered_accounts();
    let count_label = match total {
        Some(t) if !filter_value.is_empty() => format!("· {} of {} ", filtered.len(), t),
        Some(t) => format!("· {t} "),
        None => String::new(),
    };
    let mut title_spans: Vec<Span> = vec![
        Span::styled(" storage accounts ", Style::default().fg(theme.fg)),
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

    if let Some(err) = state.storage.accounts_error.as_deref() {
        let p = Paragraph::new(Line::from(Span::styled(
            format!("error: {err}"),
            Style::default().fg(theme.critical),
        )));
        frame.render_widget(p, body_area);
        render_footer(frame, chunks[1], theme);
        return;
    }

    match state.storage.accounts.as_deref() {
        None if state.storage.accounts_pending => {
            let p = Paragraph::new(Line::from(Span::styled(
                "loading storage accounts …",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        None => {
            let p = Paragraph::new(Line::from(Span::styled(
                "press r to load storage accounts.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        // clippy: `as_deref()` yields a slice so `Some([])` matches empty here.
        Some([]) => {
            let p = Paragraph::new(Line::from(Span::styled(
                "No storage accounts found in selected subscriptions.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(_) if filtered.is_empty() => {
            // Underlying list is non-empty but the active filter matched nothing.
            // Surface a message in the same style as the list view's filter miss.
            let p = Paragraph::new(Line::from(Span::styled(
                "no storage accounts match the current filter.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(_) => {
            let wide = body_area.width >= WIDE_SUB_THRESHOLD;
            let sub_w = if wide { SUB_FULL_W } else { SUB_SHORT_W };
            let name_w = filtered
                .iter()
                .map(|a| a.name.chars().count() as u16)
                .max()
                .unwrap_or(0)
                .max(NAME_W_MIN);
            // SUB NAME + SUB ID only earn their column slots when the user is
            // looking at every subscription at once. With a single sub pinned,
            // both columns would print the same value on every row.
            let show_sub_cols = state.selected_subscription.is_none();
            // HTTPS-only is the default since 2019; render the column only when
            // at least one account is the exception, turning a usually-noisy
            // column into an "outlier worth fixing" indicator.
            let show_https = filtered.iter().any(|a| a.https_only == Some(false));
            let visible = pick_columns(body_area.width, name_w, sub_w, show_sub_cols, show_https);

            let widths: Vec<Constraint> = visible.iter().map(|c| c.constraint()).collect();
            let header_row = Row::new(visible.iter().map(|c| c.header()).collect::<Vec<_>>())
                .style(
                    Style::default()
                        .fg(theme.muted)
                        .add_modifier(Modifier::BOLD),
                );

            let cursor = state.storage.accounts_cursor.min(filtered.len() - 1);
            let body_rows: Vec<Row> = filtered
                .iter()
                .map(|account| account_row(account, state, &visible, wide, theme))
                .collect();

            let table = Table::new(body_rows, widths)
                .header(header_row)
                .row_highlight_style(theme.selection())
                .highlight_symbol("▍ ")
                .column_spacing(2);

            let mut ts = TableState::default();
            ts.select(Some(cursor));
            frame.render_stateful_widget(table, body_area, &mut ts);
        }
    }

    render_footer(frame, chunks[1], theme);
}

/// Which columns the storage accounts table is currently rendering. The set
/// is decided per-frame by [`pick_columns`] based on the available width so
/// NAME never gets squeezed by the layout solver.
#[derive(Clone, Copy)]
enum Column {
    Name(u16),
    Location,
    Kind,
    Sku,
    AccessTier,
    Hns,
    Https,
    PublicBlob,
    Rg,
    Created,
    SubName,
    SubId(u16),
}

impl Column {
    fn constraint(self) -> Constraint {
        match self {
            Column::Name(w) => Constraint::Length(w),
            Column::Location => Constraint::Length(LOCATION_W),
            Column::Kind => Constraint::Length(KIND_W),
            Column::Sku => Constraint::Length(SKU_W),
            Column::AccessTier => Constraint::Length(ACCESS_TIER_W),
            Column::Hns => Constraint::Length(HNS_W),
            Column::Https => Constraint::Length(HTTPS_W),
            Column::PublicBlob => Constraint::Length(PUBLIC_BLOB_W),
            Column::Rg => Constraint::Min(RG_W_MIN),
            Column::Created => Constraint::Length(CREATED_W),
            Column::SubName => Constraint::Length(SUB_NAME_W),
            Column::SubId(w) => Constraint::Length(w),
        }
    }

    fn header(self) -> &'static str {
        match self {
            Column::Name(_) => "NAME",
            Column::Location => "LOCATION",
            Column::Kind => "KIND",
            Column::Sku => "SKU",
            Column::AccessTier => "TIER",
            Column::Hns => "HNS",
            Column::Https => "HTTPS",
            Column::PublicBlob => "PUBLIC",
            Column::Rg => "RESOURCE GROUP",
            Column::Created => "CREATED",
            Column::SubName => "SUB NAME",
            Column::SubId(_) => "SUB ID",
        }
    }

    fn cell<'a>(
        self,
        account: &'a StorageAccount,
        state: &'a AppState,
        wide_sub: bool,
        theme: &Theme,
    ) -> Cell<'a> {
        match self {
            Column::Name(_) => {
                Cell::from(account.name.as_str()).style(Style::default().fg(theme.fg))
            }
            // Reference info, not a signal — keep it dim so the eye skips it
            // when scanning for security flags above.
            Column::Location => {
                Cell::from(account.location.as_str()).style(Style::default().fg(theme.muted))
            }
            Column::Kind => Cell::from(account.kind.as_deref().unwrap_or("—").to_string())
                .style(Style::default().fg(theme.muted)),
            Column::Sku => Cell::from(account.sku.as_deref().unwrap_or("").to_string())
                .style(Style::default().fg(theme.muted)),
            Column::AccessTier => {
                Cell::from(account.access_tier.as_deref().unwrap_or("").to_string())
                    .style(Style::default().fg(theme.muted))
            }
            // ADLS Gen2 (HNS=yes) has different data-plane semantics (POSIX
            // ACLs, no flat blob list semantics) — accent it so the user
            // notices before they pick the wrong tool.
            Column::Hns => {
                let (label, color) =
                    hns_label_and_color(account.is_hns_enabled, account.kind.as_deref(), theme);
                Cell::from(label).style(Style::default().fg(color))
            }
            Column::Https => {
                Cell::from(yes_no(account.https_only)).style(Style::default().fg(theme.muted))
            }
            // PUBLIC = `allowed` means anonymous reads could be enabled on
            // any container — surface that in critical colour so a sweep of
            // the list flags risky accounts at a glance.
            Column::PublicBlob => {
                let (label, color) =
                    public_blob_label_and_color(account.allow_blob_public_access, theme);
                Cell::from(label).style(Style::default().fg(color))
            }
            Column::Rg => {
                Cell::from(account.resource_group.as_str()).style(Style::default().fg(theme.muted))
            }
            Column::Created => Cell::from(format_date(account.created_at.as_ref()))
                .style(Style::default().fg(theme.muted)),
            Column::SubName => Cell::from(
                subscription_display_name(state, &account.subscription_id)
                    .unwrap_or("")
                    .to_string(),
            )
            .style(Style::default().fg(theme.muted)),
            Column::SubId(_) => Cell::from(format_subscription(&account.subscription_id, wide_sub))
                .style(Style::default().fg(theme.muted)),
        }
    }
}

/// Choose the visible column set for the current width. NAME is
/// non-negotiable and sized to the longest actual name; everything else is
/// added only when there's enough room so the layout solver doesn't squeeze
/// NAME into a truncating Length constraint. Each entry's width budget
/// includes a `+2` for the column gap.
///
/// Priority order (highest first):
///   NAME → PUBLIC → KIND → SKU → TIER → HNS → HTTPS → RG → SUB NAME →
///   SUB ID → LOCATION.
///
/// Reasoning:
///   - NAME first (always) — sized to longest actual name so it never truncates.
///   - PUBLIC right after NAME — the most security-critical signal in the
///     view (anonymous-blob exposure surfaces here before anywhere else).
///   - Identification columns (KIND/SKU/TIER, HNS/HTTPS, RG) come next so the
///     operator can tell the accounts apart at half-screen widths.
///   - SUB NAME and SUB ID are paired and only appear in the multi-sub case
///     (i.e. `state.selected_subscription` is `None`). When a single sub is
///     selected, both are redundant and waste columns.
///   - LOCATION comes last because it's almost always the same value for any
///     given tenant; only show it when there's leftover room.
fn pick_columns(
    area_w: u16,
    name_w: u16,
    sub_w: u16,
    show_sub_cols: bool,
    show_https: bool,
) -> Vec<Column> {
    let mut cols = vec![Column::Name(name_w)];
    let mut used = name_w;
    let add = |col: Column, cost: u16, cols: &mut Vec<Column>, used: &mut u16| {
        if *used + 2 + cost <= area_w {
            cols.push(col);
            *used += 2 + cost;
        }
    };
    add(Column::PublicBlob, PUBLIC_BLOB_W, &mut cols, &mut used);
    add(Column::Kind, KIND_W, &mut cols, &mut used);
    add(Column::Sku, SKU_W, &mut cols, &mut used);
    add(Column::AccessTier, ACCESS_TIER_W, &mut cols, &mut used);
    add(Column::Hns, HNS_W, &mut cols, &mut used);
    if show_https {
        add(Column::Https, HTTPS_W, &mut cols, &mut used);
    }
    add(Column::Rg, RG_W_MIN, &mut cols, &mut used);
    add(Column::Created, CREATED_W, &mut cols, &mut used);
    if show_sub_cols {
        add(Column::SubName, SUB_NAME_W, &mut cols, &mut used);
        add(Column::SubId(sub_w), sub_w, &mut cols, &mut used);
    }
    add(Column::Location, LOCATION_W, &mut cols, &mut used);
    cols
}

fn account_row<'a>(
    account: &'a StorageAccount,
    state: &'a AppState,
    visible: &[Column],
    wide_sub: bool,
    theme: &Theme,
) -> Row<'a> {
    Row::new(
        visible
            .iter()
            .map(|c| c.cell(account, state, wide_sub, theme))
            .collect::<Vec<_>>(),
    )
}

fn render_footer(frame: &mut Frame, area: Rect, theme: &Theme) {
    let p = Paragraph::new(Line::from(Span::styled(
        FOOTER_HINT,
        Style::default().fg(theme.muted),
    )));
    frame.render_widget(p, area);
}

pub fn handle(action: Action, state: &mut AppState) -> bool {
    // Navigation operates on the filtered slice so the cursor never points
    // past the end of what's rendered. `len` mirrors the visible list.
    let len = state.storage.filtered_accounts().len();

    // While the filter input has focus, swallow most actions but let the
    // dispatcher's filter-forwarding gate push raw chars into the buffer.
    // Esc cancels (deactivates AND clears); Enter commits (deactivates,
    // keeps the value). Down hands focus back to the filtered list, the
    // same Vim-ish handoff the resource list does.
    if state.storage.accounts_filter_active {
        match action {
            Action::Back => {
                state.storage.accounts_filter_active = false;
                state.storage.accounts_filter.reset();
                state.storage.accounts_cursor = 0;
                return true;
            }
            Action::OpenSelected => {
                state.storage.accounts_filter_active = false;
                return true;
            }
            Action::MoveDown => {
                state.storage.accounts_filter_active = false;
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
                state.storage.accounts_cursor = (state.storage.accounts_cursor + 1).min(len - 1);
            }
            true
        }
        Action::MoveUp => {
            state.storage.accounts_cursor = state.storage.accounts_cursor.saturating_sub(1);
            true
        }
        Action::HalfPageDown => {
            if len > 0 {
                state.storage.accounts_cursor =
                    (state.storage.accounts_cursor + HALF_PAGE).min(len - 1);
            }
            true
        }
        Action::HalfPageUp => {
            state.storage.accounts_cursor = state.storage.accounts_cursor.saturating_sub(HALF_PAGE);
            true
        }
        Action::GotoTop => {
            state.storage.accounts_cursor = 0;
            true
        }
        Action::GotoBottom => {
            if len > 0 {
                state.storage.accounts_cursor = len - 1;
            }
            true
        }
        Action::StartSearch => {
            state.storage.accounts_filter_active = true;
            true
        }
        Action::OpenSelected => {
            // Resolve via the filtered slice so the cursor's row matches what
            // the user actually sees on screen.
            let account = state
                .storage
                .filtered_accounts()
                .get(state.storage.accounts_cursor)
                .copied()
                .cloned();
            if let Some(account) = account {
                state.storage.selected_account = Some(account);
                state.storage.containers_cursor = 0;
                state.view_stack.push(state.view);
                // Drill into the per-account overview panel first; Enter from
                // there opens the containers list. See `View::StorageAccountOverview`.
                state.view = View::StorageAccountOverview;
            }
            true
        }
        _ => false,
    }
}

/// Render an `Option<bool>` as `yes` / `no` / `?`. Used for the boolean
/// metadata columns (HTTPS) where a missing value means "not projected by
/// Resource Graph" rather than a real default.
fn yes_no(b: Option<bool>) -> String {
    match b {
        Some(true) => "yes".to_string(),
        Some(false) => "no".to_string(),
        None => "?".to_string(),
    }
}

/// `YYYY-MM-DD` for the CREATED column. Empty when no date was parsed —
/// rendering blank is less misleading than a placeholder.
fn format_date(dt: Option<&chrono::DateTime<chrono::Utc>>) -> String {
    match dt {
        Some(d) => d.format("%Y-%m-%d").to_string(),
        None => String::new(),
    }
}

/// PUBLIC BLOB column: `allowed` (true) / `blocked` (false) / `?` (missing).
/// Wording matches the Azure portal toggle so users recognise it. The colour
/// mirrors the blast-radius pattern used by the container view's PUBLIC
/// column — `allowed` is red because anyone could be reading these blobs.
fn public_blob_label_and_color(
    b: Option<bool>,
    theme: &Theme,
) -> (&'static str, ratatui::style::Color) {
    match b {
        Some(true) => ("allowed", theme.critical),
        Some(false) => ("blocked", theme.muted),
        None => ("?", theme.muted),
    }
}

/// HNS column. ADLS Gen2 has different data-plane semantics (POSIX ACLs,
/// hierarchical namespace), so render `yes` in `accent` to make it noticeable.
///
/// Missing data is interpreted from `kind`: V1 (`Storage`) doesn't support
/// HNS at all — render `n/a`. Anything else with a missing bit is treated as
/// `no` (HNS is opt-in at account creation, absence ≡ not opted in). We never
/// render `?` here because the question mark misleads users into thinking the
/// fetch failed.
fn hns_label_and_color(
    is_hns_enabled: Option<bool>,
    kind: Option<&str>,
    theme: &Theme,
) -> (&'static str, ratatui::style::Color) {
    match is_hns_enabled {
        Some(true) => ("yes", theme.accent),
        Some(false) => ("no", theme.muted),
        None => match kind {
            Some("Storage") => ("n/a", theme.muted),
            _ => ("no", theme.muted),
        },
    }
}

/// Choose between the full GUID (when the table has room) and the
/// `{first8}…` short form. Strips the optional `/subscriptions/…` prefix that
/// callers sometimes hand in. Discipline from MEMORY.md: still never *log* or
/// *yank* the full sub id — this is render-only.
fn format_subscription(id: &str, wide: bool) -> String {
    let trimmed = id.trim_start_matches("/subscriptions/");
    let head = trimmed.split('/').next().unwrap_or(trimmed);
    if wide || head.len() <= 8 {
        head.to_string()
    } else {
        format!("{}…", &head[..8])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::storage::StorageAccount;
    use crate::azure::subscriptions::Subscription;
    use crate::config::Config;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn fixture() -> AppState {
        let mut state = AppState::new(Config::default());
        state.view = View::StorageAccounts;
        // Default config leaves `selected_subscription = None` (multi-sub
        // mode), which is what most rendering tests want — the SUB NAME / SUB
        // ID columns are gated on that.
        state
    }

    fn sub(id: &str, name: &str) -> Subscription {
        Subscription {
            id: id.into(),
            display_name: name.into(),
            state: "Enabled".into(),
            tenant_id: "tenant".into(),
        }
    }

    fn account(name: &str) -> StorageAccount {
        StorageAccount {
            id: format!(
                "/subscriptions/11112222-3333-4444-5555-666677778888/resourceGroups/rg/providers/Microsoft.Storage/storageAccounts/{name}",
            ),
            name: name.into(),
            resource_group: "rg".into(),
            subscription_id: "11112222-3333-4444-5555-666677778888".into(),
            location: "westeurope".into(),
            kind: Some("StorageV2".into()),
            sku: Some("Standard_GRS".into()),
            access_tier: Some("Hot".into()),
            is_hns_enabled: Some(true),
            https_only: Some(true),
            allow_blob_public_access: Some(false),
            created_at: None,
        }
    }

    #[test]
    fn renders_loading_state() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(100, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.storage.accounts_pending = true;
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("loading storage accounts"));
    }

    #[test]
    fn renders_empty_subscriptions_message() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(100, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.storage.accounts = Some(Vec::new());
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(
            buf.contains("No storage accounts"),
            "expected empty-state copy, got: {buf}"
        );
    }

    #[test]
    fn renders_account_rows() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(180, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.storage.accounts = Some(vec![account("acct1")]);
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("acct1"));
        assert!(buf.contains("westeurope"));
        assert!(buf.contains("StorageV2"));
    }

    #[test]
    fn renders_column_headers() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(200, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        // Seed an HTTPS-only=false outlier so the HTTPS column unhides for the
        // assertion below.
        let mut outlier = account("outlier");
        outlier.https_only = Some(false);
        state.storage.accounts = Some(vec![account("acct1"), outlier]);
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("NAME"), "header should include NAME");
        assert!(buf.contains("LOCATION"), "header should include LOCATION");
        assert!(buf.contains("KIND"), "header should include KIND");
        assert!(buf.contains("SKU"), "header should include SKU");
        assert!(buf.contains("TIER"), "header should include TIER");
        assert!(buf.contains("HNS"), "header should include HNS");
        assert!(buf.contains("HTTPS"), "header should include HTTPS");
        assert!(buf.contains("PUBLIC"), "header should include PUBLIC");
        assert!(
            buf.contains("RESOURCE GROUP"),
            "header should include RESOURCE GROUP"
        );
        assert!(
            buf.contains("SUB NAME"),
            "header should include SUB NAME (multi-sub mode)"
        );
        assert!(
            buf.contains("SUB ID"),
            "header should include SUB ID (multi-sub mode)"
        );
    }

    #[test]
    fn https_column_hidden_when_no_outlier() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(200, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        // All accounts have https_only = Some(true) — nothing to call out.
        state.storage.accounts = Some(vec![account("a"), account("b")]);
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(
            !buf.contains("HTTPS"),
            "HTTPS column should be hidden when every account is HTTPS-only"
        );
    }

    #[test]
    fn column_order_matches_priority_chain() {
        // The header strip should read NAME, PUBLIC, KIND, SKU, TIER, HNS,
        // HTTPS, RG, CREATED, SUB NAME, SUB ID, LOCATION at a width that fits
        // them all. Seed an outlier so HTTPS column is visible.
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(200, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        let mut outlier = account("outlier");
        outlier.https_only = Some(false);
        state.storage.accounts = Some(vec![account("acct1"), outlier]);
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());

        // Find positions of each header label and confirm the relative order.
        let order = [
            "NAME",
            "PUBLIC",
            "KIND",
            "SKU",
            "TIER",
            "HNS",
            "HTTPS",
            "RESOURCE GROUP",
            "CREATED",
            "SUB NAME",
            "SUB ID",
            "LOCATION",
        ];
        let mut positions = Vec::new();
        for label in order {
            let p = buf.find(label).unwrap_or_else(|| {
                panic!("header `{label}` missing from buffer: {buf}");
            });
            positions.push((label, p));
        }
        let mut sorted = positions.clone();
        sorted.sort_by_key(|(_, p)| *p);
        assert_eq!(
            sorted, positions,
            "header columns appeared out of expected order: {positions:?}",
        );
    }

    #[test]
    fn renders_metadata_values() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(200, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.storage.accounts = Some(vec![account("acct1")]);
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("Standard_GRS"), "SKU value missing");
        assert!(buf.contains("Hot"), "ACCESS TIER value missing");
        assert!(buf.contains("yes"), "HNS / HTTPS yes missing");
        assert!(buf.contains("blocked"), "PUBLIC BLOB blocked missing");
    }

    #[test]
    fn renders_missing_metadata_as_question_mark() {
        // An account with no SKU/tier/booleans (e.g. legacy classic account
        // where Resource Graph omits the fields) must not panic and should
        // surface the `?` / blank placeholders so the operator can spot it.
        let mut sparse = account("legacy");
        sparse.sku = None;
        sparse.access_tier = None;
        sparse.is_hns_enabled = None;
        sparse.https_only = None;
        sparse.allow_blob_public_access = None;

        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(200, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.storage.accounts = Some(vec![sparse]);
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("legacy"));
        assert!(buf.contains("?"));
    }

    #[test]
    fn wide_terminal_shows_full_subscription_guid() {
        // At width >= WIDE_SUB_THRESHOLD the table renders the full 36-char
        // GUID instead of the `{first8}…` short form.
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(200, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.storage.accounts = Some(vec![account("acct1")]);
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(
            buf.contains("11112222-3333-4444-5555-666677778888"),
            "wide layout should render the full GUID, got: {buf}"
        );
    }

    #[test]
    fn narrow_terminal_shows_short_subscription_id() {
        // Mid-width (140 cols, below WIDE_SUB_THRESHOLD): SUB ID column is
        // present but renders the `{first8}…` short form, not the full GUID.
        // Multi-sub mode (selected_subscription = None) is required for the
        // SUB columns to be added at all.
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(140, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.selected_subscription = None;
        state.storage.accounts = Some(vec![account("acct1")]);
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(
            buf.contains("11112222…"),
            "narrow layout should render the short sub id, got: {buf}"
        );
        assert!(
            !buf.contains("666677778888"),
            "narrow layout must not leak the full GUID, got: {buf}"
        );
    }

    #[test]
    fn single_subscription_mode_hides_sub_columns() {
        // When the user has pinned a single subscription, both SUB NAME and
        // SUB ID become redundant (every row repeats the same value) — they
        // should be dropped even at widths that would otherwise fit them.
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(120, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.selected_subscription = Some("11112222-3333-4444-5555-666677778888".to_string());
        state.subscriptions = vec![sub(
            "11112222-3333-4444-5555-666677778888",
            "prod-shared-services",
        )];
        state.storage.accounts = Some(vec![account("acct1")]);
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(
            !buf.contains("SUB NAME"),
            "single-sub mode must hide SUB NAME header, got: {buf}",
        );
        assert!(
            !buf.contains("SUB ID"),
            "single-sub mode must hide SUB ID header, got: {buf}",
        );
        assert!(
            !buf.contains("11112222…") && !buf.contains("666677778888"),
            "single-sub mode must not render any form of the sub id, got: {buf}",
        );
    }

    #[test]
    fn multi_subscription_mode_renders_sub_name_from_lookup() {
        // In multi-sub mode SUB NAME pulls from `state.subscriptions`; rows
        // whose sub id resolves get the display name, rows that don't render
        // an empty cell instead of echoing the id back. We size the terminal
        // narrow enough that only SUB NAME fits (SUB ID drops off) so the
        // "no echo" assertion can't be fooled by the SUB ID column showing
        // the same guid in its own cell.
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(128, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.selected_subscription = None;
        state.subscriptions = vec![sub(
            "11112222-3333-4444-5555-666677778888",
            "prod-shared-services",
        )];
        let mut unknown = account("orphan");
        unknown.subscription_id = "99998888-7777-6666-5555-444433332222".into();
        state.storage.accounts = Some(vec![account("known"), unknown]);
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(
            buf.contains("SUB NAME"),
            "test width should keep SUB NAME visible, got: {buf}",
        );
        assert!(
            !buf.contains("SUB ID"),
            "test width should drop SUB ID so the lookup assertion is meaningful, got: {buf}",
        );
        assert!(
            buf.contains("prod-shared-services"),
            "resolved sub id should print its display name, got: {buf}",
        );
        // Unresolved sub id must NOT have its guid leaked back into the SUB
        // NAME cell — empty fallback only.
        assert!(
            !buf.contains("99998888"),
            "unresolved sub should render empty, not echo the id, got: {buf}",
        );
    }

    #[test]
    fn public_allowed_renders_in_critical_color() {
        // PUBLIC = allowed is the headline security signal in this view; the
        // critical theme colour must reach the rendered buffer so the cell
        // stands out from the muted defaults.
        use ratatui::style::Color;
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(200, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        let mut risky = account("public-acct");
        risky.allow_blob_public_access = Some(true);
        state.storage.accounts = Some(vec![risky]);
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = term.backend().buffer().clone();
        let dump = format!("{buf:?}");
        assert!(
            dump.contains("allowed"),
            "PUBLIC cell should render `allowed`, got: {dump}",
        );

        // Sweep every cell in the buffer and confirm at least one cell has
        // the critical foreground — the `allowed` glyphs do.
        let critical: Color = theme.critical;
        let mut found = false;
        for y in 0..buf.area().height {
            for x in 0..buf.area().width {
                if buf[(x, y)].fg == critical {
                    found = true;
                    break;
                }
            }
            if found {
                break;
            }
        }
        assert!(
            found,
            "expected at least one cell painted in theme.critical for PUBLIC=allowed",
        );
    }

    #[test]
    fn enter_pins_account_and_drills_in() {
        // Enter now lands on the per-account overview screen (not directly on
        // containers). The pinned account moves forward intact so subsequent
        // Enter on the overview opens the containers list under the same acct.
        let mut state = fixture();
        let acct = account("acct1");
        state.storage.accounts = Some(vec![acct.clone()]);
        assert!(handle(Action::OpenSelected, &mut state));
        assert_eq!(state.view, View::StorageAccountOverview);
        assert_eq!(
            state
                .storage
                .selected_account
                .as_ref()
                .map(|a| a.name.as_str()),
            Some("acct1")
        );
    }

    #[test]
    fn navigation_clamps_to_loaded_rows() {
        let mut state = fixture();
        state.storage.accounts = Some(vec![account("a"), account("b")]);
        assert!(handle(Action::MoveDown, &mut state));
        assert_eq!(state.storage.accounts_cursor, 1);
        assert!(handle(Action::MoveDown, &mut state));
        assert_eq!(state.storage.accounts_cursor, 1, "clamped to last row");
        assert!(handle(Action::GotoTop, &mut state));
        assert_eq!(state.storage.accounts_cursor, 0);
    }

    #[test]
    fn start_search_sets_filter_active() {
        let mut state = fixture();
        state.storage.accounts = Some(vec![account("acct1")]);
        assert!(handle(Action::StartSearch, &mut state));
        assert!(state.storage.accounts_filter_active);
    }

    #[test]
    fn esc_while_filtering_clears_filter() {
        // Esc on an active filter mirrors the resource list: deactivate AND
        // clear the buffer so the next `/` starts fresh.
        let mut state = fixture();
        state.storage.accounts = Some(vec![account("acct1")]);
        state.storage.accounts_filter_active = true;
        state.storage.accounts_filter = tui_input::Input::default().with_value("ac".to_string());
        assert!(handle(Action::Back, &mut state));
        assert!(!state.storage.accounts_filter_active);
        assert_eq!(state.storage.accounts_filter.value(), "");
    }

    #[test]
    fn enter_while_filtering_keeps_value_and_deactivates() {
        // First Enter just defocuses the filter box; the value persists so the
        // narrowed list keeps applying. A second Enter (filter inactive) drills
        // into the highlighted account.
        let mut state = fixture();
        state.storage.accounts = Some(vec![account("acct1")]);
        state.storage.accounts_filter_active = true;
        state.storage.accounts_filter = tui_input::Input::default().with_value("ac".to_string());
        assert!(handle(Action::OpenSelected, &mut state));
        assert!(!state.storage.accounts_filter_active);
        assert_eq!(state.storage.accounts_filter.value(), "ac");
        // View did not transition on this Enter (still on the accounts view).
        assert_eq!(state.view, View::StorageAccounts);
    }

    #[test]
    fn filter_substring_match_is_case_insensitive() {
        let mut state = fixture();
        state.storage.accounts = Some(vec![
            account("alpha"),
            account("Beta"),
            account("gamma-alpha"),
        ]);
        state.storage.accounts_filter = tui_input::Input::default().with_value("AL".to_string());
        let names: Vec<&str> = state
            .storage
            .filtered_accounts()
            .iter()
            .map(|a| a.name.as_str())
            .collect();
        assert_eq!(names, vec!["alpha", "gamma-alpha"]);
    }

    #[test]
    fn navigation_uses_filtered_length() {
        // Two of three accounts match; MoveDown must stop at the filtered
        // length, not the raw length.
        let mut state = fixture();
        state.storage.accounts = Some(vec![account("alpha"), account("beta"), account("alphabet")]);
        state.storage.accounts_filter = tui_input::Input::default().with_value("alpha".to_string());
        assert!(handle(Action::GotoBottom, &mut state));
        assert_eq!(
            state.storage.accounts_cursor, 1,
            "GotoBottom clamps to filtered len-1, not raw len-1",
        );
        assert!(handle(Action::MoveDown, &mut state));
        assert_eq!(state.storage.accounts_cursor, 1, "MoveDown clamped");
    }

    #[test]
    fn enter_after_filter_drills_into_filtered_row() {
        // Cursor row in the filtered slice must be the one drilled into,
        // not the same index into the raw `accounts` Vec.
        let mut state = fixture();
        let target = account("zeta");
        state.storage.accounts = Some(vec![account("alpha"), account("beta"), target.clone()]);
        state.storage.accounts_filter = tui_input::Input::default().with_value("zeta".to_string());
        // Filter is inactive here (committed); cursor 0 == only filtered row.
        state.storage.accounts_cursor = 0;
        assert!(handle(Action::OpenSelected, &mut state));
        // Enter now drills into the overview screen (one step before containers).
        assert_eq!(state.view, View::StorageAccountOverview);
        assert_eq!(
            state
                .storage
                .selected_account
                .as_ref()
                .map(|a| a.name.as_str()),
            Some("zeta"),
        );
    }

    #[test]
    fn renders_filter_chip_in_title() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(120, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.storage.accounts = Some(vec![account("alpha"), account("beta")]);
        state.storage.accounts_filter = tui_input::Input::default().with_value("al".to_string());
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
    fn renders_no_match_message() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(120, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.storage.accounts = Some(vec![account("alpha"), account("beta")]);
        state.storage.accounts_filter = tui_input::Input::default().with_value("zzz".to_string());
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(
            buf.contains("no storage accounts match the current filter"),
            "expected no-match copy, got: {buf}",
        );
    }

    #[test]
    fn renders_filter_input_row_when_active() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(120, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.storage.accounts = Some(vec![account("alpha")]);
        state.storage.accounts_filter_active = true;
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains(">"), "search input row should render");
    }

    #[test]
    fn format_subscription_modes() {
        assert_eq!(
            format_subscription("11112222-3333-4444-5555-666677778888", false),
            "11112222…"
        );
        assert_eq!(
            format_subscription("11112222-3333-4444-5555-666677778888", true),
            "11112222-3333-4444-5555-666677778888"
        );
        assert_eq!(format_subscription("short", false), "short");
        assert_eq!(format_subscription("short", true), "short");
    }
}
