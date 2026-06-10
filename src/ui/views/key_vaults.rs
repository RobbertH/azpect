//! Top-level Key Vaults mode entry view: lists vaults visible to the current
//! subscription scope. Pressing Enter on a row pins the vault into
//! `state.key_vault.selected_vault` and opens [`View::KeyVaultItems`].

#![allow(dead_code, unused_variables)]

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;

use super::{name_col_width, truncate_ellipsis};
use crate::azure::key_vault::KeyVault;
use crate::ui::events::Action;
use crate::ui::state::{subscription_display_name, AppState, View};
use crate::ui::theme::Theme;

const FOOTER_HINT: &str =
    "j/k move  Enter items  / filter  Esc back  r refresh  y yank id  ? help  q quit";
const HALF_PAGE: usize = 10;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);

    let filter_value = state.key_vault.vaults_filter.value();
    let filter_active = state.key_vault.vaults_filter_active;
    let total = state.key_vault.vaults.as_ref().map(|v| v.len());
    let filtered = state.key_vault.filtered_vaults();
    let count_label = match total {
        Some(t) if !filter_value.is_empty() => format!("· {} of {} ", filtered.len(), t),
        Some(t) => format!("· {t} "),
        None => String::new(),
    };
    let mut title_spans: Vec<Span> = vec![
        Span::styled(" key vaults ", Style::default().fg(theme.fg)),
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

    if let Some(err) = state.key_vault.vaults_error.as_deref() {
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

    match state.key_vault.vaults.as_deref() {
        None if state.key_vault.vaults_pending => {
            let p = Paragraph::new(Line::from(Span::styled(
                "loading key vaults …",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        None => {
            let p = Paragraph::new(Line::from(Span::styled(
                "press r to load key vaults.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some([]) => {
            let p = Paragraph::new(Line::from(Span::styled(
                "No key vaults found in selected subscriptions.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(_) if filtered.is_empty() => {
            let p = Paragraph::new(Line::from(Span::styled(
                "no key vaults match the current filter.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(_) => {
            let show_sub_cols = state.selected_subscription.is_none();

            // NAME gets whatever width is left after the fixed columns; on a
            // narrow terminal it shrinks and the name truncates *with* an
            // ellipsis (see `name`/`build_row`) rather than the table silently
            // clipping it. `fixed_w` is the sum of the Length()s below; keep the
            // two in sync.
            let fixed_w: u16 = 9 + 6 + 6 + 10 + 22 + if show_sub_cols { 22 } else { 0 } + 14;
            let n_cols: u16 = if show_sub_cols { 8 } else { 7 };
            let longest = filtered
                .iter()
                .map(|v| v.name.chars().count() as u16)
                .max()
                .unwrap_or(0);
            let name_w = name_col_width(body_area.width, fixed_w, n_cols, longest);

            let mut widths: Vec<Constraint> = vec![
                Constraint::Length(name_w), // NAME
                Constraint::Length(9),      // SKU
                Constraint::Length(6),      // AUTH (RBAC / policy)
                Constraint::Length(6),      // PURGE protection
                Constraint::Length(10),     // PUBLIC NET
                Constraint::Length(22),     // RG
            ];
            let mut headers: Vec<&'static str> = vec![
                "NAME",
                "SKU",
                "AUTH",
                "PURGE",
                "PUBLIC NET",
                "RESOURCE GROUP",
            ];
            if show_sub_cols {
                widths.push(Constraint::Length(22));
                headers.push("SUB NAME");
            }
            widths.push(Constraint::Length(14)); // LOCATION
            headers.push("LOCATION");

            let header_row = Row::new(headers).style(
                Style::default()
                    .fg(theme.muted)
                    .add_modifier(Modifier::BOLD),
            );

            let cursor = state.key_vault.vaults_cursor.min(filtered.len() - 1);
            let body_rows: Vec<Row> = filtered
                .iter()
                .map(|vault| build_row(vault, state, show_sub_cols, name_w, theme))
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

fn build_row<'a>(
    vault: &'a KeyVault,
    state: &'a AppState,
    show_sub_cols: bool,
    name_w: u16,
    theme: &Theme,
) -> Row<'a> {
    let auth = match vault.rbac_authorization_enabled {
        Some(true) => Cell::from("RBAC").style(Style::default().fg(theme.fg)),
        Some(false) => Cell::from("policy").style(Style::default().fg(theme.muted)),
        None => Cell::from("?").style(Style::default().fg(theme.muted)),
    };
    let purge = match vault.purge_protection_enabled {
        // Purge protection enabled is a compliance signal — surface it brightly.
        Some(true) => Cell::from("on").style(Style::default().fg(theme.healthy)),
        Some(false) => Cell::from("off").style(Style::default().fg(theme.muted)),
        None => Cell::from("?").style(Style::default().fg(theme.muted)),
    };
    let public = match vault.public_network_access.as_deref() {
        Some("Disabled") => Cell::from("Disabled").style(Style::default().fg(theme.muted)),
        Some(s) => Cell::from(s.to_string()).style(Style::default().fg(theme.fg)),
        None => Cell::from("?").style(Style::default().fg(theme.muted)),
    };
    let mut cells: Vec<Cell> = vec![
        Cell::from(truncate_ellipsis(&vault.name, name_w as usize))
            .style(Style::default().fg(theme.fg)),
        Cell::from(vault.sku.as_deref().unwrap_or("—").to_string())
            .style(Style::default().fg(theme.muted)),
        auth,
        purge,
        public,
        Cell::from(vault.resource_group.as_str()).style(Style::default().fg(theme.muted)),
    ];
    if show_sub_cols {
        cells.push(
            Cell::from(
                subscription_display_name(state, &vault.subscription_id)
                    .unwrap_or("")
                    .to_string(),
            )
            .style(Style::default().fg(theme.muted)),
        );
    }
    cells.push(Cell::from(vault.location.as_str()).style(Style::default().fg(theme.muted)));
    Row::new(cells)
}

fn render_footer(frame: &mut Frame, area: Rect, theme: &Theme) {
    let p = Paragraph::new(Line::from(Span::styled(
        FOOTER_HINT,
        Style::default().fg(theme.muted),
    )));
    frame.render_widget(p, area);
}

pub fn handle(action: Action, state: &mut AppState) -> bool {
    let len = state.key_vault.filtered_vaults().len();

    if state.key_vault.vaults_filter_active {
        match action {
            Action::Back => {
                state.key_vault.vaults_filter_active = false;
                state.key_vault.vaults_filter.reset();
                state.key_vault.vaults_cursor = 0;
                return true;
            }
            Action::OpenSelected => {
                state.key_vault.vaults_filter_active = false;
                return true;
            }
            Action::MoveDown => {
                state.key_vault.vaults_filter_active = false;
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
                state.key_vault.vaults_cursor = (state.key_vault.vaults_cursor + 1).min(len - 1);
            }
            true
        }
        Action::MoveUp => {
            state.key_vault.vaults_cursor = state.key_vault.vaults_cursor.saturating_sub(1);
            true
        }
        Action::HalfPageDown => {
            if len > 0 {
                state.key_vault.vaults_cursor =
                    (state.key_vault.vaults_cursor + HALF_PAGE).min(len - 1);
            }
            true
        }
        Action::HalfPageUp => {
            state.key_vault.vaults_cursor = state.key_vault.vaults_cursor.saturating_sub(HALF_PAGE);
            true
        }
        Action::GotoTop => {
            state.key_vault.vaults_cursor = 0;
            true
        }
        Action::GotoBottom => {
            if len > 0 {
                state.key_vault.vaults_cursor = len - 1;
            }
            true
        }
        Action::StartSearch => {
            state.key_vault.vaults_filter_active = true;
            true
        }
        Action::OpenSelected => {
            let vault = state
                .key_vault
                .filtered_vaults()
                .get(state.key_vault.vaults_cursor)
                .copied()
                .cloned();
            if let Some(vault) = vault {
                state.key_vault.selected_vault = Some(vault);
                state.key_vault.items_cursor = 0;
                state.key_vault.items_filter = tui_input::Input::default();
                state.view_stack.push(state.view);
                state.view = View::KeyVaultItems;
            }
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::key_vault::KeyVault;
    use crate::config::Config;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn fixture() -> AppState {
        let mut state = AppState::new(Config::default());
        state.view = View::KeyVaults;
        state
    }

    fn vault(name: &str) -> KeyVault {
        KeyVault {
            id: format!(
                "/subscriptions/sub/resourceGroups/rg/providers/Microsoft.KeyVault/vaults/{name}",
            ),
            name: name.into(),
            resource_group: "rg".into(),
            subscription_id: "sub".into(),
            location: "westeurope".into(),
            sku: Some("standard".into()),
            vault_uri: Some(format!("https://{name}.vault.azure.net/")),
            rbac_authorization_enabled: Some(true),
            soft_delete_enabled: Some(true),
            purge_protection_enabled: Some(false),
            public_network_access: Some("Enabled".into()),
        }
    }

    #[test]
    fn renders_loading_state() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(140, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.key_vault.vaults_pending = true;
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("loading key vaults"));
    }

    #[test]
    fn renders_vault_row_with_rbac_label() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(180, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.key_vault.vaults = Some(vec![vault("myvault")]);
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("myvault"), "name should render");
        assert!(buf.contains("RBAC"), "auth model should render");
    }

    #[test]
    fn long_name_truncates_with_ellipsis_on_narrow_terminal() {
        let theme = Theme::catppuccin_mocha();
        // Too narrow for the full name once the fixed columns take their share —
        // the squeeze the user hit on a half-width terminal.
        let backend = TestBackend::new(90, 8);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.key_vault.vaults = Some(vec![vault("kv-adp-onefab-egress-prod1")]);
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(
            buf.contains('\u{2026}'),
            "a clipped name must show an ellipsis, not a silent cut"
        );
        assert!(
            !buf.contains("kv-adp-onefab-egress-prod1"),
            "the full over-long name should not render"
        );
    }

    #[test]
    fn enter_pins_vault_and_drills_in() {
        let mut state = fixture();
        state.key_vault.vaults = Some(vec![vault("myvault")]);
        assert!(handle(Action::OpenSelected, &mut state));
        assert_eq!(state.view, View::KeyVaultItems);
        assert_eq!(
            state
                .key_vault
                .selected_vault
                .as_ref()
                .map(|v| v.name.as_str()),
            Some("myvault")
        );
    }

    #[test]
    fn filter_substring_match_is_case_insensitive() {
        let mut state = fixture();
        state.key_vault.vaults = Some(vec![vault("prod-kv"), vault("Dev-KV"), vault("other")]);
        state.key_vault.vaults_filter = tui_input::Input::default().with_value("KV".into());
        let names: Vec<&str> = state
            .key_vault
            .filtered_vaults()
            .iter()
            .map(|v| v.name.as_str())
            .collect();
        assert_eq!(names, vec!["prod-kv", "Dev-KV"]);
    }
}
