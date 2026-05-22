//! Key Vault items drill-in: metadata-only listing of secrets *or*
//! certificates in the pinned vault. Tab / Shift-Tab toggles between kinds.
//!
//! **No secret values are ever fetched.** The list API only returns
//! attributes (enabled flag, expiry, created/updated timestamps, content
//! type). For the rare case where a user actually needs a value, the global
//! `o` keybind opens the Azure portal page for the vault.

#![allow(dead_code, unused_variables)]

use chrono::{DateTime, Duration, Utc};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};
use ratatui::Frame;

use crate::azure::key_vault::{ItemKind, KeyVaultItem};
use crate::ui::events::Action;
use crate::ui::state::{AppState, KeyVaultCache};
use crate::ui::theme::Theme;

const FOOTER_HINT: &str =
    "j/k move  Tab toggle kind  / filter  Esc back  r refresh  y yank name  o portal  ? help  q quit";
const HALF_PAGE: usize = 10;

/// Items expiring within this window are flagged red. Matches typical secret
/// rotation cadence — anything inside a month needs attention now.
const URGENT_EXPIRY: Duration = Duration::days(30);

/// Items expiring within this window are flagged yellow. Three months is the
/// usual "you have time but plan it" buffer.
const SOON_EXPIRY: Duration = Duration::days(90);

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);

    let filter_value = state.key_vault.items_filter.value();
    let filter_active = state.key_vault.items_filter_active;
    let kind = state.key_vault.items_kind;
    let cache_key = state
        .key_vault
        .selected_vault
        .as_ref()
        .map(|v| KeyVaultCache::items_key(&v.id, kind));
    let total = cache_key
        .as_ref()
        .and_then(|k| state.key_vault.items.get(k))
        .map(|v| v.len());
    let filtered: Vec<&KeyVaultItem> = match state.key_vault.selected_vault.as_ref() {
        Some(v) => state.key_vault.filtered_items(&v.id),
        None => Vec::new(),
    };
    let count_label = match total {
        Some(t) if !filter_value.is_empty() => format!("· {} of {} ", filtered.len(), t),
        Some(t) => format!("· {t} "),
        None => String::new(),
    };
    let kind_label = match kind {
        ItemKind::Secret => "secrets",
        ItemKind::Certificate => "certificates",
    };
    let mut title_spans: Vec<Span> = vec![
        Span::styled(format!(" {kind_label} "), Style::default().fg(theme.fg)),
        Span::styled(count_label, Style::default().fg(theme.muted)),
        Span::styled("[Tab: switch]", Style::default().fg(theme.muted)),
        Span::raw(" "),
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

    let Some(key) = cache_key else {
        let p = Paragraph::new(Line::from(Span::styled(
            "no vault selected.",
            Style::default().fg(theme.muted),
        )));
        frame.render_widget(p, body_area);
        render_footer(frame, chunks[1], theme);
        return;
    };

    if let Some(err) = state.key_vault.items_error.get(&key) {
        let p = Paragraph::new(Line::from(Span::styled(
            format!("error: {err}"),
            Style::default().fg(theme.critical),
        )));
        frame.render_widget(p, body_area);
        render_footer(frame, chunks[1], theme);
        return;
    }

    let items = state.key_vault.items.get(&key);
    let loading = state.key_vault.items_pending.contains(&key);
    match items {
        None if loading => {
            let p = Paragraph::new(Line::from(Span::styled(
                format!("loading {kind_label} …"),
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        None => {
            let p = Paragraph::new(Line::from(Span::styled(
                format!("press r to load {kind_label}."),
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(rows) if rows.is_empty() => {
            let p = Paragraph::new(Line::from(Span::styled(
                format!("no {kind_label} in this vault."),
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(_) if filtered.is_empty() => {
            let p = Paragraph::new(Line::from(Span::styled(
                format!("no {kind_label} match the current filter."),
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(_) => {
            let now = Utc::now();
            let name_w = filtered
                .iter()
                .map(|i| i.name.chars().count() as u16)
                .max()
                .unwrap_or(0)
                .max(4);

            let widths = [
                Constraint::Length(name_w), // NAME
                Constraint::Length(8),      // ENABLED
                Constraint::Length(20),     // EXPIRES (+ days remaining)
                Constraint::Length(10),     // UPDATED
                Constraint::Min(10),        // CONTENT TYPE
            ];
            let header_row = Row::new(vec!["NAME", "ENABLED", "EXPIRES", "UPDATED", "TYPE"]).style(
                Style::default()
                    .fg(theme.muted)
                    .add_modifier(Modifier::BOLD),
            );

            let cursor = state.key_vault.items_cursor.min(filtered.len() - 1);
            let body_rows: Vec<Row> = filtered
                .iter()
                .map(|item| build_row(item, now, theme))
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

fn build_row<'a>(item: &'a KeyVaultItem, now: DateTime<Utc>, theme: &Theme) -> Row<'a> {
    let enabled = match item.enabled {
        Some(true) => Cell::from("yes").style(Style::default().fg(theme.fg)),
        Some(false) => Cell::from("no").style(Style::default().fg(theme.critical)),
        None => Cell::from("?").style(Style::default().fg(theme.muted)),
    };
    let (expires_text, expires_color) = format_expiry(item.expires.as_ref(), now, theme);
    let expires_cell = Cell::from(expires_text).style(Style::default().fg(expires_color));
    let updated =
        Cell::from(format_date(item.updated.as_ref())).style(Style::default().fg(theme.muted));
    let content_type = Cell::from(item.content_type.as_deref().unwrap_or("—").to_string())
        .style(Style::default().fg(theme.muted));

    Row::new(vec![
        Cell::from(item.name.as_str()).style(Style::default().fg(theme.fg)),
        enabled,
        expires_cell,
        updated,
        content_type,
    ])
}

/// Format the expiry timestamp with a "in N days" / "expired N days ago"
/// suffix, and return the color for the cell: red < 30 days, yellow < 90,
/// green otherwise, muted gray when no expiry is set.
pub(crate) fn format_expiry(
    expires: Option<&DateTime<Utc>>,
    now: DateTime<Utc>,
    theme: &Theme,
) -> (String, Color) {
    let Some(exp) = expires else {
        return ("—".to_string(), theme.muted);
    };
    let date = exp.format("%Y-%m-%d").to_string();
    let delta = *exp - now;
    let days = delta.num_days();
    let (suffix, color) = if delta < Duration::zero() {
        let ago = -days;
        let suffix = if ago == 1 {
            " (expired 1d ago)".to_string()
        } else {
            format!(" (expired {ago}d ago)")
        };
        (suffix, theme.critical)
    } else if delta < URGENT_EXPIRY {
        (format!(" (in {days}d)"), theme.critical)
    } else if delta < SOON_EXPIRY {
        (format!(" (in {days}d)"), theme.degraded)
    } else {
        (format!(" (in {days}d)"), theme.healthy)
    };
    (format!("{date}{suffix}"), color)
}

fn render_footer(frame: &mut Frame, area: Rect, theme: &Theme) {
    let p = Paragraph::new(Line::from(Span::styled(
        FOOTER_HINT,
        Style::default().fg(theme.muted),
    )));
    frame.render_widget(p, area);
}

fn format_date(dt: Option<&DateTime<Utc>>) -> String {
    match dt {
        Some(d) => d.format("%Y-%m-%d").to_string(),
        None => String::new(),
    }
}

/// Toggle between Secret and Certificate. Resets cursor + filter so the user
/// lands at the top of the other list cleanly.
fn toggle_kind(state: &mut AppState) {
    state.key_vault.items_kind = match state.key_vault.items_kind {
        ItemKind::Secret => ItemKind::Certificate,
        ItemKind::Certificate => ItemKind::Secret,
    };
    state.key_vault.items_cursor = 0;
    state.key_vault.items_filter = tui_input::Input::default();
}

/// Yank target for the items view: the selected item's name. The vault URI
/// fallback is provided by the global yank handler.
pub fn yank_text(state: &AppState) -> Option<String> {
    let vault = state.key_vault.selected_vault.as_ref()?;
    let item = state
        .key_vault
        .filtered_items(&vault.id)
        .get(state.key_vault.items_cursor)
        .copied()?;
    Some(item.name.clone())
}

pub fn handle(action: Action, state: &mut AppState) -> bool {
    let Some(vault_id) = state
        .key_vault
        .selected_vault
        .as_ref()
        .map(|v| v.id.clone())
    else {
        return false;
    };
    let len = state.key_vault.filtered_items(&vault_id).len();

    if state.key_vault.items_filter_active {
        match action {
            Action::Back => {
                state.key_vault.items_filter_active = false;
                state.key_vault.items_filter.reset();
                state.key_vault.items_cursor = 0;
                return true;
            }
            Action::OpenSelected => {
                state.key_vault.items_filter_active = false;
                return true;
            }
            Action::MoveDown => {
                state.key_vault.items_filter_active = false;
            }
            Action::MoveUp
            | Action::HalfPageDown
            | Action::HalfPageUp
            | Action::GotoTop
            | Action::GotoBottom => {}
            _ => return false,
        }
    }

    match action {
        Action::MoveDown => {
            if len > 0 {
                state.key_vault.items_cursor = (state.key_vault.items_cursor + 1).min(len - 1);
            }
            true
        }
        Action::MoveUp => {
            state.key_vault.items_cursor = state.key_vault.items_cursor.saturating_sub(1);
            true
        }
        Action::HalfPageDown => {
            if len > 0 {
                state.key_vault.items_cursor =
                    (state.key_vault.items_cursor + HALF_PAGE).min(len - 1);
            }
            true
        }
        Action::HalfPageUp => {
            state.key_vault.items_cursor = state.key_vault.items_cursor.saturating_sub(HALF_PAGE);
            true
        }
        Action::GotoTop => {
            state.key_vault.items_cursor = 0;
            true
        }
        Action::GotoBottom => {
            if len > 0 {
                state.key_vault.items_cursor = len - 1;
            }
            true
        }
        Action::StartSearch => {
            state.key_vault.items_filter_active = true;
            true
        }
        Action::NextPanel | Action::PrevPanel => {
            toggle_kind(state);
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::key_vault::{ItemKind, KeyVault, KeyVaultItem};
    use crate::config::Config;
    use crate::ui::state::View;
    use chrono::TimeZone;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn vault_fixture() -> KeyVault {
        KeyVault {
            id: "/subs/x/rg/y/providers/Microsoft.KeyVault/vaults/v".into(),
            name: "v".into(),
            resource_group: "rg".into(),
            subscription_id: "sub".into(),
            location: "westeurope".into(),
            sku: Some("standard".into()),
            vault_uri: Some("https://v.vault.azure.net/".into()),
            rbac_authorization_enabled: Some(true),
            soft_delete_enabled: Some(true),
            purge_protection_enabled: Some(false),
            public_network_access: Some("Enabled".into()),
        }
    }

    fn fixture() -> AppState {
        let mut state = AppState::new(Config::default());
        state.view = View::KeyVaultItems;
        state.key_vault.selected_vault = Some(vault_fixture());
        state
    }

    fn secret(name: &str, expires: Option<DateTime<Utc>>) -> KeyVaultItem {
        KeyVaultItem {
            kind: ItemKind::Secret,
            name: name.into(),
            enabled: Some(true),
            expires,
            not_before: None,
            created: None,
            updated: Some(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()),
            content_type: Some("application/json".into()),
        }
    }

    #[test]
    fn renders_secret_row_with_expiry() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(140, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        let key = KeyVaultCache::items_key(
            "/subs/x/rg/y/providers/Microsoft.KeyVault/vaults/v",
            ItemKind::Secret,
        );
        state.key_vault.items.insert(
            key,
            vec![secret(
                "api-key",
                Some(Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0).unwrap()),
            )],
        );
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("api-key"), "name should render");
        assert!(buf.contains("2030-01-01"), "expiry date should render");
    }

    #[test]
    fn tab_toggles_kind_and_resets_cursor() {
        let mut state = fixture();
        state.key_vault.items_cursor = 5;
        assert_eq!(state.key_vault.items_kind, ItemKind::Secret);
        assert!(handle(Action::NextPanel, &mut state));
        assert_eq!(state.key_vault.items_kind, ItemKind::Certificate);
        assert_eq!(state.key_vault.items_cursor, 0);

        assert!(handle(Action::PrevPanel, &mut state));
        assert_eq!(state.key_vault.items_kind, ItemKind::Secret);
    }

    #[test]
    fn yank_returns_selected_item_name() {
        let mut state = fixture();
        let key = KeyVaultCache::items_key(
            "/subs/x/rg/y/providers/Microsoft.KeyVault/vaults/v",
            ItemKind::Secret,
        );
        state
            .key_vault
            .items
            .insert(key, vec![secret("first", None), secret("second", None)]);
        state.key_vault.items_cursor = 1;
        assert_eq!(yank_text(&state).as_deref(), Some("second"));
    }

    #[test]
    fn format_expiry_color_buckets() {
        let theme = Theme::catppuccin_mocha();
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();

        // No expiry → muted "—"
        let (text, color) = format_expiry(None, now, &theme);
        assert_eq!(text, "—");
        assert_eq!(color, theme.muted);

        // Already expired → critical with "ago" suffix
        let past = Utc.with_ymd_and_hms(2025, 12, 1, 0, 0, 0).unwrap();
        let (text, color) = format_expiry(Some(&past), now, &theme);
        assert!(text.contains("expired"));
        assert_eq!(color, theme.critical);

        // Within 30 days → critical
        let soon = now + Duration::days(10);
        let (_text, color) = format_expiry(Some(&soon), now, &theme);
        assert_eq!(color, theme.critical);

        // Between 30 and 90 days → degraded
        let warn = now + Duration::days(60);
        let (_text, color) = format_expiry(Some(&warn), now, &theme);
        assert_eq!(color, theme.degraded);

        // Far future → healthy
        let safe = now + Duration::days(180);
        let (_text, color) = format_expiry(Some(&safe), now, &theme);
        assert_eq!(color, theme.healthy);
    }
}
