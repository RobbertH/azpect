//! Cosmos DB containers drill-in: lists SQL containers (collections) under
//! the pinned `{account, database}`. Enter on a row pins the container name
//! and opens [`View::CosmosItem`].

#![allow(dead_code, unused_variables)]

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;

use super::{name_col_width, truncate_ellipsis};
use crate::azure::cosmos::CosmosContainer;
use crate::ui::events::Action;
use crate::ui::state::{AppState, CosmosCache, View};
use crate::ui::theme::Theme;

const FOOTER_HINT: &str =
    "j/k move  Enter items  / filter  Esc back  r refresh  y yank name  ? help  q quit";
const HALF_PAGE: usize = 10;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);

    let filter_value = state.cosmos.containers_filter.value();
    let filter_active = state.cosmos.containers_filter_active;
    let cache_key = state
        .cosmos
        .selected_account
        .as_ref()
        .zip(state.cosmos.selected_database.as_deref())
        .map(|(a, db)| CosmosCache::containers_key(&a.id, db));
    let total = cache_key
        .as_ref()
        .and_then(|k| state.cosmos.containers.get(k))
        .map(|v| v.len());
    let filtered: Vec<&CosmosContainer> = match (
        state.cosmos.selected_account.as_ref(),
        state.cosmos.selected_database.as_deref(),
    ) {
        (Some(a), Some(db)) => state.cosmos.filtered_containers(&a.id, db),
        _ => Vec::new(),
    };
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
            "no database selected.",
            Style::default().fg(theme.muted),
        )));
        frame.render_widget(p, body_area);
        render_footer(frame, chunks[1], theme);
        return;
    };

    if let Some(err) = state.cosmos.containers_error.get(&key) {
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

    let containers = state.cosmos.containers.get(&key);
    let loading = state.cosmos.containers_pending.contains(&key);
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
                "no containers in this database.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(_) if filtered.is_empty() => {
            let p = Paragraph::new(Line::from(Span::styled(
                "no containers match the current filter.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(_) => {
            // CONTAINER absorbs the leftover width; on a narrow terminal it
            // caps to the budget and truncates with an ellipsis rather than
            // pushing the partition-key/TTL columns off-screen. `fixed_w` sums
            // the non-NAME widths below (the Min(20) counts its minimum); keep
            // the two in sync. Floor at 9 so the "CONTAINER" header always
            // reads.
            let fixed_w: u16 = 20 + 7 + 10 + 8;
            let longest = filtered
                .iter()
                .map(|c| c.name.chars().count() as u16)
                .max()
                .unwrap_or(0);
            let name_w = name_col_width(body_area.width, fixed_w, 5, longest).max(9);

            let widths = [
                Constraint::Length(name_w), // NAME
                Constraint::Min(20),        // PARTITION KEY
                Constraint::Length(7),      // PK KIND
                Constraint::Length(10),     // INDEXING
                Constraint::Length(8),      // TTL
            ];
            let header_row = Row::new(vec![
                "CONTAINER",
                "PARTITION KEY",
                "PK KIND",
                "INDEXING",
                "TTL",
            ])
            .style(
                Style::default()
                    .fg(theme.muted)
                    .add_modifier(Modifier::BOLD),
            );

            let body_rows: Vec<Row> = filtered
                .iter()
                .map(|c| {
                    let pk = if c.partition_key_paths.is_empty() {
                        "—".to_string()
                    } else {
                        c.partition_key_paths.join(", ")
                    };
                    let pk_kind = c.partition_key_kind.as_deref().unwrap_or("—").to_string();
                    let indexing = c.indexing_mode.as_deref().unwrap_or("—").to_string();
                    let ttl = format_ttl(c.default_ttl);
                    Row::new(vec![
                        Cell::from(truncate_ellipsis(&c.name, name_w as usize))
                            .style(Style::default().fg(theme.fg)),
                        Cell::from(pk).style(Style::default().fg(theme.muted)),
                        Cell::from(pk_kind).style(Style::default().fg(theme.muted)),
                        Cell::from(indexing).style(Style::default().fg(theme.muted)),
                        Cell::from(ttl).style(Style::default().fg(theme.muted)),
                    ])
                })
                .collect();

            let cursor = state.cosmos.containers_cursor.min(filtered.len() - 1);
            let table = Table::new(body_rows, widths)
                .header(header_row)
                .row_highlight_style(theme.selection())
                .highlight_symbol("▍ ")
                .column_spacing(2);

            // Offset persisted across frames so the window only scrolls when the
            // cursor pushes against an edge (ratatui reconciles it during render).
            let mut ts = TableState::default().with_offset(state.cosmos.containers_view_top.get());
            ts.select(Some(cursor));
            frame.render_stateful_widget(table, body_area, &mut ts);
            state.cosmos.containers_view_top.set(ts.offset());
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

/// Cosmos defaultTtl semantics: `None` / 0 = TTL disabled, `-1` = enabled but
/// no default per-item expiry (items only expire when they set `ttl`),
/// positive = seconds-until-expiry default.
fn format_ttl(ttl: Option<i64>) -> String {
    match ttl {
        None | Some(0) => "off".to_string(),
        Some(-1) => "on".to_string(),
        Some(s) => format!("{s}s"),
    }
}

pub fn handle(action: Action, state: &mut AppState) -> bool {
    let Some((account_id, db)) = state
        .cosmos
        .selected_account
        .as_ref()
        .map(|a| a.id.clone())
        .zip(state.cosmos.selected_database.clone())
    else {
        return false;
    };
    let len = state.cosmos.filtered_containers(&account_id, &db).len();

    if state.cosmos.containers_filter_active {
        match action {
            Action::Back => {
                state.cosmos.containers_filter_active = false;
                state.cosmos.containers_filter.reset();
                state.cosmos.containers_cursor = 0;
                return true;
            }
            Action::OpenSelected => {
                state.cosmos.containers_filter_active = false;
                return true;
            }
            Action::MoveDown => {
                state.cosmos.containers_filter_active = false;
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
                state.cosmos.containers_cursor = (state.cosmos.containers_cursor + 1).min(len - 1);
            }
            true
        }
        Action::MoveUp => {
            state.cosmos.containers_cursor = state.cosmos.containers_cursor.saturating_sub(1);
            true
        }
        Action::HalfPageDown => {
            if len > 0 {
                state.cosmos.containers_cursor =
                    (state.cosmos.containers_cursor + HALF_PAGE).min(len - 1);
            }
            true
        }
        Action::HalfPageUp => {
            state.cosmos.containers_cursor =
                state.cosmos.containers_cursor.saturating_sub(HALF_PAGE);
            true
        }
        Action::GotoTop => {
            state.cosmos.containers_cursor = 0;
            true
        }
        Action::GotoBottom => {
            if len > 0 {
                state.cosmos.containers_cursor = len - 1;
            }
            true
        }
        Action::StartSearch => {
            state.cosmos.containers_filter.reset();
            state.cosmos.containers_cursor = 0;
            state.cosmos.containers_filter_active = true;
            true
        }
        Action::OpenSelected => {
            let coll = state
                .cosmos
                .filtered_containers(&account_id, &db)
                .get(state.cosmos.containers_cursor)
                .map(|c| c.name.clone());
            if let Some(name) = coll {
                state.cosmos.selected_container = Some(name);
                state.cosmos.items_scroll = 0;
                state.view = View::CosmosItem;
            }
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::cosmos::{CosmosAccount, CosmosContainer};
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
            capabilities: Vec::new(),
            is_serverless: false,
            public_network_access: Some("Enabled".into()),
            created_at: None,
        }
    }

    fn fixture() -> AppState {
        let mut state = AppState::new(Config::default());
        state.view = View::CosmosContainers;
        state.cosmos.selected_account = Some(account_fixture());
        state.cosmos.selected_database = Some("orders".into());
        state
    }

    fn container(name: &str) -> CosmosContainer {
        CosmosContainer {
            id: format!("/subs/x/rg/y/da/acc/sqlDatabases/orders/containers/{name}"),
            name: name.into(),
            partition_key_paths: vec!["/userId".into()],
            partition_key_kind: Some("Hash".into()),
            default_ttl: Some(3600),
            indexing_mode: Some("consistent".into()),
        }
    }

    #[test]
    fn renders_containers_with_partition_key_and_ttl() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(120, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        let key = CosmosCache::containers_key("/subs/x/rg/y/da/acc", "orders");
        state
            .cosmos
            .containers
            .insert(key, vec![container("items")]);
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("items"));
        assert!(buf.contains("/userId"));
        assert!(buf.contains("Hash"));
        assert!(buf.contains("consistent"));
        assert!(buf.contains("3600s"));
    }

    #[test]
    fn enter_pins_container_and_drills_in() {
        let mut state = fixture();
        let key = CosmosCache::containers_key("/subs/x/rg/y/da/acc", "orders");
        state
            .cosmos
            .containers
            .insert(key, vec![container("items")]);
        assert!(handle(Action::OpenSelected, &mut state));
        assert_eq!(state.view, View::CosmosItem);
        assert_eq!(state.cosmos.selected_container.as_deref(), Some("items"));
    }

    #[test]
    fn formats_ttl_modes() {
        assert_eq!(format_ttl(None), "off");
        assert_eq!(format_ttl(Some(0)), "off");
        assert_eq!(format_ttl(Some(-1)), "on");
        assert_eq!(format_ttl(Some(86400)), "86400s");
    }
}
