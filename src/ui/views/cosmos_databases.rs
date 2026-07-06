//! Cosmos DB databases drill-in: lists SQL databases under the pinned
//! account in [`crate::ui::state::CosmosCache::selected_account`]. Enter on a
//! row pins the database name and opens [`View::CosmosContainers`].

#![allow(dead_code, unused_variables)]

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;

use crate::ui::events::Action;
use crate::ui::state::{AppState, View};
use crate::ui::theme::Theme;

const FOOTER_HINT: &str =
    "j/k move  Enter containers  / filter  Esc back  r refresh  y yank name  ? help  q quit";
const HALF_PAGE: usize = 10;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);

    let filter_value = state.cosmos.databases_filter.value();
    let filter_active = state.cosmos.databases_filter_active;
    let total = state
        .cosmos
        .selected_account
        .as_ref()
        .and_then(|a| state.cosmos.databases.get(&a.id))
        .map(|v| v.len());
    let filtered = state
        .cosmos
        .selected_account
        .as_ref()
        .map(|a| state.cosmos.filtered_databases(&a.id))
        .unwrap_or_default();
    let count_label = match total {
        Some(t) if !filter_value.is_empty() => format!("· {} of {} ", filtered.len(), t),
        Some(t) => format!("· {t} "),
        None => String::new(),
    };
    let mut title_spans: Vec<Span> = vec![
        Span::styled(" databases ", Style::default().fg(theme.fg)),
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

    let Some(account) = state.cosmos.selected_account.as_ref() else {
        let p = Paragraph::new(Line::from(Span::styled(
            "no cosmos account selected.",
            Style::default().fg(theme.muted),
        )));
        frame.render_widget(p, body_area);
        render_footer(frame, chunks[1], theme);
        return;
    };

    if let Some(err) = state.cosmos.databases_error.get(&account.id) {
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

    let databases = state.cosmos.databases.get(&account.id);
    let loading = state.cosmos.databases_pending.contains(&account.id);
    match databases {
        None if loading => {
            let p = Paragraph::new(Line::from(Span::styled(
                "loading databases …",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        None => {
            let p = Paragraph::new(Line::from(Span::styled(
                "press r to load databases.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(rows) if rows.is_empty() => {
            let p = Paragraph::new(Line::from(Span::styled(
                "no databases in this account.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(_) if filtered.is_empty() => {
            let p = Paragraph::new(Line::from(Span::styled(
                "no databases match the current filter.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(_) => {
            let widths = [Constraint::Min(20)];
            let header_row = Row::new(vec!["DATABASE"]).style(
                Style::default()
                    .fg(theme.muted)
                    .add_modifier(Modifier::BOLD),
            );

            let body_rows: Vec<Row> = filtered
                .iter()
                .map(|d| {
                    Row::new(vec![
                        Cell::from(d.name.as_str()).style(Style::default().fg(theme.fg))
                    ])
                })
                .collect();

            let cursor = state.cosmos.databases_cursor.min(filtered.len() - 1);
            let table = Table::new(body_rows, widths)
                .header(header_row)
                .row_highlight_style(theme.selection())
                .highlight_symbol("▍ ")
                .column_spacing(2);

            // Offset persisted across frames so the window only scrolls when the
            // cursor pushes against an edge (ratatui reconciles it during render).
            let mut ts = TableState::default().with_offset(state.cosmos.databases_view_top.get());
            ts.select(Some(cursor));
            frame.render_stateful_widget(table, body_area, &mut ts);
            state.cosmos.databases_view_top.set(ts.offset());
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

pub fn handle(action: Action, state: &mut AppState) -> bool {
    let Some(account_id) = state.cosmos.selected_account.as_ref().map(|a| a.id.clone()) else {
        return false;
    };
    let len = state.cosmos.filtered_databases(&account_id).len();

    if state.cosmos.databases_filter_active {
        match action {
            Action::Back => {
                state.cosmos.databases_filter_active = false;
                state.cosmos.databases_filter.reset();
                state.cosmos.databases_cursor = 0;
                return true;
            }
            Action::OpenSelected => {
                state.cosmos.databases_filter_active = false;
                return true;
            }
            Action::MoveDown => {
                state.cosmos.databases_filter_active = false;
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
                state.cosmos.databases_cursor = (state.cosmos.databases_cursor + 1).min(len - 1);
            }
            true
        }
        Action::MoveUp => {
            state.cosmos.databases_cursor = state.cosmos.databases_cursor.saturating_sub(1);
            true
        }
        Action::HalfPageDown => {
            if len > 0 {
                state.cosmos.databases_cursor =
                    (state.cosmos.databases_cursor + HALF_PAGE).min(len - 1);
            }
            true
        }
        Action::HalfPageUp => {
            state.cosmos.databases_cursor = state.cosmos.databases_cursor.saturating_sub(HALF_PAGE);
            true
        }
        Action::GotoTop => {
            state.cosmos.databases_cursor = 0;
            true
        }
        Action::GotoBottom => {
            if len > 0 {
                state.cosmos.databases_cursor = len - 1;
            }
            true
        }
        Action::StartSearch => {
            state.cosmos.databases_filter_active = true;
            true
        }
        Action::OpenSelected => {
            let db = state
                .cosmos
                .filtered_databases(&account_id)
                .get(state.cosmos.databases_cursor)
                .map(|d| d.name.clone());
            if let Some(name) = db {
                state.cosmos.selected_database = Some(name);
                state.cosmos.containers_cursor = 0;
                state.cosmos.containers_filter = tui_input::Input::default();
                state.view = View::CosmosContainers;
            }
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::cosmos::{CosmosAccount, CosmosDatabase};
    use crate::config::Config;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn account_fixture() -> CosmosAccount {
        CosmosAccount {
            id: "/subs/x/rg/y/da/acc".into(),
            name: "acc".into(),
            resource_group: "rg".into(),
            subscription_id: "sub".into(),
            location: "westeurope".into(),
            kind: Some("GlobalDocumentDB".into()),
            document_endpoint: Some("https://acc.documents.azure.com:443/".into()),
            capabilities: vec!["enableserverless".into()],
            is_serverless: true,
            public_network_access: Some("Enabled".into()),
            created_at: None,
        }
    }

    fn fixture() -> AppState {
        let mut state = AppState::new(Config::default());
        state.view = View::CosmosDatabases;
        state.cosmos.selected_account = Some(account_fixture());
        state
    }

    fn db(name: &str) -> CosmosDatabase {
        CosmosDatabase {
            id: format!("/subs/x/rg/y/da/acc/sqlDatabases/{name}"),
            name: name.into(),
        }
    }

    #[test]
    fn renders_databases() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(120, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.cosmos.databases.insert(
            "/subs/x/rg/y/da/acc".into(),
            vec![db("orders"), db("users")],
        );
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("orders"));
        assert!(buf.contains("users"));
    }

    #[test]
    fn enter_pins_database_and_drills_in() {
        let mut state = fixture();
        state
            .cosmos
            .databases
            .insert("/subs/x/rg/y/da/acc".into(), vec![db("orders")]);
        assert!(handle(Action::OpenSelected, &mut state));
        assert_eq!(state.view, View::CosmosContainers);
        assert_eq!(state.cosmos.selected_database.as_deref(), Some("orders"));
    }
}
