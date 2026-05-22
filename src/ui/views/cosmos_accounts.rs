//! Top-level Cosmos DB mode entry view: lists SQL/Core API accounts visible
//! to the current subscription scope. Pressing Enter on a row pins the
//! account into `state.cosmos.selected_account` and opens
//! [`View::CosmosDatabases`].

#![allow(dead_code, unused_variables)]

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};
use ratatui::Frame;

use crate::azure::cosmos::CosmosAccount;
use crate::ui::events::Action;
use crate::ui::state::{subscription_display_name, AppState, View};
use crate::ui::theme::Theme;

const FOOTER_HINT: &str =
    "j/k move  Enter databases  / filter  Esc back  r refresh  y yank id  ? help  q quit";
const HALF_PAGE: usize = 10;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);

    let filter_value = state.cosmos.accounts_filter.value();
    let filter_active = state.cosmos.accounts_filter_active;
    let total = state.cosmos.accounts.as_ref().map(|v| v.len());
    let filtered = state.cosmos.filtered_accounts();
    let count_label = match total {
        Some(t) if !filter_value.is_empty() => format!("· {} of {} ", filtered.len(), t),
        Some(t) => format!("· {t} "),
        None => String::new(),
    };
    let mut title_spans: Vec<Span> = vec![
        Span::styled(" cosmos db (sql/core) ", Style::default().fg(theme.fg)),
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

    if let Some(err) = state.cosmos.accounts_error.as_deref() {
        let p = Paragraph::new(Line::from(Span::styled(
            format!("error: {err}"),
            Style::default().fg(theme.critical),
        )));
        frame.render_widget(p, body_area);
        render_footer(frame, chunks[1], theme);
        return;
    }

    match state.cosmos.accounts.as_deref() {
        None if state.cosmos.accounts_pending => {
            let p = Paragraph::new(Line::from(Span::styled(
                "loading cosmos accounts …",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        None => {
            let p = Paragraph::new(Line::from(Span::styled(
                "press r to load cosmos accounts.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some([]) => {
            let p = Paragraph::new(Line::from(Span::styled(
                "No SQL/Core Cosmos accounts found in selected subscriptions.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(_) if filtered.is_empty() => {
            let p = Paragraph::new(Line::from(Span::styled(
                "no cosmos accounts match the current filter.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(_) => {
            let show_sub_cols = state.selected_subscription.is_none();

            let name_w = filtered
                .iter()
                .map(|a| a.name.chars().count() as u16)
                .max()
                .unwrap_or(0)
                .max(4);

            let mut widths: Vec<Constraint> = vec![
                Constraint::Length(name_w), // NAME
                Constraint::Length(11),     // MODE
                Constraint::Length(10),     // PUBLIC NET
                Constraint::Length(22),     // RG
                Constraint::Length(10),     // CREATED
            ];
            let mut headers: Vec<&'static str> =
                vec!["NAME", "MODE", "PUBLIC NET", "RESOURCE GROUP", "CREATED"];
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

            let cursor = state.cosmos.accounts_cursor.min(filtered.len() - 1);
            let body_rows: Vec<Row> = filtered
                .iter()
                .map(|acc| build_row(acc, state, show_sub_cols, theme))
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
    acc: &'a CosmosAccount,
    state: &'a AppState,
    show_sub_cols: bool,
    theme: &Theme,
) -> Row<'a> {
    let mode = if acc.is_serverless {
        Cell::from("Serverless").style(Style::default().fg(theme.fg))
    } else {
        Cell::from("Provisioned").style(Style::default().fg(theme.muted))
    };
    let public = match acc.public_network_access.as_deref() {
        Some("Disabled") => Cell::from("Disabled").style(Style::default().fg(theme.muted)),
        Some(s) => Cell::from(s.to_string()).style(Style::default().fg(theme.fg)),
        None => Cell::from("?").style(Style::default().fg(theme.muted)),
    };
    let mut cells: Vec<Cell> = vec![
        Cell::from(acc.name.as_str()).style(Style::default().fg(theme.fg)),
        mode,
        public,
        Cell::from(acc.resource_group.as_str()).style(Style::default().fg(theme.muted)),
        Cell::from(format_date(acc.created_at.as_ref())).style(Style::default().fg(theme.muted)),
    ];
    if show_sub_cols {
        cells.push(
            Cell::from(
                subscription_display_name(state, &acc.subscription_id)
                    .unwrap_or("")
                    .to_string(),
            )
            .style(Style::default().fg(theme.muted)),
        );
    }
    cells.push(Cell::from(acc.location.as_str()).style(Style::default().fg(theme.muted)));
    Row::new(cells)
}

fn render_footer(frame: &mut Frame, area: Rect, theme: &Theme) {
    let p = Paragraph::new(Line::from(Span::styled(
        FOOTER_HINT,
        Style::default().fg(theme.muted),
    )));
    frame.render_widget(p, area);
}

fn format_date(dt: Option<&chrono::DateTime<chrono::Utc>>) -> String {
    match dt {
        Some(d) => d.format("%Y-%m-%d").to_string(),
        None => String::new(),
    }
}

pub fn handle(action: Action, state: &mut AppState) -> bool {
    let len = state.cosmos.filtered_accounts().len();

    if state.cosmos.accounts_filter_active {
        match action {
            Action::Back => {
                state.cosmos.accounts_filter_active = false;
                state.cosmos.accounts_filter.reset();
                state.cosmos.accounts_cursor = 0;
                return true;
            }
            Action::OpenSelected => {
                state.cosmos.accounts_filter_active = false;
                return true;
            }
            Action::MoveDown => {
                state.cosmos.accounts_filter_active = false;
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
                state.cosmos.accounts_cursor = (state.cosmos.accounts_cursor + 1).min(len - 1);
            }
            true
        }
        Action::MoveUp => {
            state.cosmos.accounts_cursor = state.cosmos.accounts_cursor.saturating_sub(1);
            true
        }
        Action::HalfPageDown => {
            if len > 0 {
                state.cosmos.accounts_cursor =
                    (state.cosmos.accounts_cursor + HALF_PAGE).min(len - 1);
            }
            true
        }
        Action::HalfPageUp => {
            state.cosmos.accounts_cursor = state.cosmos.accounts_cursor.saturating_sub(HALF_PAGE);
            true
        }
        Action::GotoTop => {
            state.cosmos.accounts_cursor = 0;
            true
        }
        Action::GotoBottom => {
            if len > 0 {
                state.cosmos.accounts_cursor = len - 1;
            }
            true
        }
        Action::StartSearch => {
            state.cosmos.accounts_filter_active = true;
            true
        }
        Action::OpenSelected => {
            let account = state
                .cosmos
                .filtered_accounts()
                .get(state.cosmos.accounts_cursor)
                .copied()
                .cloned();
            if let Some(account) = account {
                state.cosmos.selected_account = Some(account);
                state.cosmos.databases_cursor = 0;
                state.cosmos.databases_filter = tui_input::Input::default();
                state.view_stack.push(state.view);
                state.view = View::CosmosDatabases;
            }
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::cosmos::CosmosAccount;
    use crate::config::Config;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn fixture() -> AppState {
        let mut state = AppState::new(Config::default());
        state.view = View::CosmosAccounts;
        state
    }

    fn account(name: &str) -> CosmosAccount {
        CosmosAccount {
            id: format!(
                "/subscriptions/sub/resourceGroups/rg/providers/Microsoft.DocumentDB/databaseAccounts/{name}",
            ),
            name: name.into(),
            resource_group: "rg".into(),
            subscription_id: "sub".into(),
            location: "westeurope".into(),
            kind: Some("GlobalDocumentDB".into()),
            document_endpoint: Some(format!("https://{name}.documents.azure.com:443/")),
            capabilities: vec!["enableserverless".into()],
            is_serverless: true,
            public_network_access: Some("Enabled".into()),
            created_at: None,
        }
    }

    #[test]
    fn renders_loading_state() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(140, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.cosmos.accounts_pending = true;
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("loading cosmos accounts"));
    }

    #[test]
    fn renders_account_row_with_serverless_mode() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(180, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.cosmos.accounts = Some(vec![account("acc")]);
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("acc"), "name should render");
        assert!(buf.contains("Serverless"), "mode should render");
    }

    #[test]
    fn enter_pins_account_and_drills_in() {
        let mut state = fixture();
        state.cosmos.accounts = Some(vec![account("acc")]);
        assert!(handle(Action::OpenSelected, &mut state));
        assert_eq!(state.view, View::CosmosDatabases);
        assert_eq!(
            state
                .cosmos
                .selected_account
                .as_ref()
                .map(|a| a.name.as_str()),
            Some("acc")
        );
    }
}
