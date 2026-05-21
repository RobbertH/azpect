//! Container Registry repositories drill-in: lists Docker repositories
//! (image names) under the pinned registry in
//! [`crate::ui::state::RegistryCache::selected_registry`]. Enter on a row pins
//! the repository name and opens [`View::RegistryTags`].

#![allow(dead_code, unused_variables)]

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};
use ratatui::Frame;

use crate::ui::events::Action;
use crate::ui::state::{AppState, View};
use crate::ui::theme::Theme;

const FOOTER_HINT: &str =
    "j/k move  Enter tags  / filter  Esc back  r refresh  y yank name  ? help  q quit";
const HALF_PAGE: usize = 10;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);

    let filter_value = state.registry.repositories_filter.value();
    let filter_active = state.registry.repositories_filter_active;
    let total = state
        .registry
        .selected_registry
        .as_ref()
        .and_then(|r| state.registry.repositories.get(&r.id))
        .map(|v| v.len());
    let filtered = state
        .registry
        .selected_registry
        .as_ref()
        .map(|r| state.registry.filtered_repositories(&r.id))
        .unwrap_or_default();
    let count_label = match total {
        Some(t) if !filter_value.is_empty() => format!("· {} of {} ", filtered.len(), t),
        Some(t) => format!("· {t} "),
        None => String::new(),
    };
    let mut title_spans: Vec<Span> = vec![
        Span::styled(" repositories ", Style::default().fg(theme.fg)),
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

    let Some(registry) = state.registry.selected_registry.as_ref() else {
        let p = Paragraph::new(Line::from(Span::styled(
            "no registry selected.",
            Style::default().fg(theme.muted),
        )));
        frame.render_widget(p, body_area);
        render_footer(frame, chunks[1], theme);
        return;
    };

    if let Some(err) = state.registry.repositories_error.get(&registry.id) {
        let p = Paragraph::new(Line::from(Span::styled(
            format!("error: {err}"),
            Style::default().fg(theme.critical),
        )));
        frame.render_widget(p, body_area);
        render_footer(frame, chunks[1], theme);
        return;
    }

    let repositories = state.registry.repositories.get(&registry.id);
    let loading = state.registry.repositories_pending.contains(&registry.id);
    match repositories {
        None if loading => {
            let p = Paragraph::new(Line::from(Span::styled(
                "loading repositories …",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        None => {
            let p = Paragraph::new(Line::from(Span::styled(
                "press r to load repositories.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(rows) if rows.is_empty() => {
            let p = Paragraph::new(Line::from(Span::styled(
                "no repositories in this registry.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(_) if filtered.is_empty() => {
            let p = Paragraph::new(Line::from(Span::styled(
                "no repositories match the current filter.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(_) => {
            // Just one column for now — image counts / pushed-at require
            // per-repo manifest fetches that we don't make.
            let widths = [Constraint::Min(20)];
            let header_row = Row::new(vec!["REPOSITORY"]).style(
                Style::default()
                    .fg(theme.muted)
                    .add_modifier(Modifier::BOLD),
            );

            let body_rows: Vec<Row> = filtered
                .iter()
                .map(|r| {
                    Row::new(vec![
                        Cell::from(r.name.as_str()).style(Style::default().fg(theme.fg))
                    ])
                })
                .collect();

            let cursor = state.registry.repositories_cursor.min(filtered.len() - 1);
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

fn render_footer(frame: &mut Frame, area: Rect, theme: &Theme) {
    let p = Paragraph::new(Line::from(Span::styled(
        FOOTER_HINT,
        Style::default().fg(theme.muted),
    )));
    frame.render_widget(p, area);
}

pub fn handle(action: Action, state: &mut AppState) -> bool {
    let Some(registry_id) = state
        .registry
        .selected_registry
        .as_ref()
        .map(|r| r.id.clone())
    else {
        return false;
    };
    let len = state.registry.filtered_repositories(&registry_id).len();

    if state.registry.repositories_filter_active {
        match action {
            Action::Back => {
                state.registry.repositories_filter_active = false;
                state.registry.repositories_filter.reset();
                state.registry.repositories_cursor = 0;
                return true;
            }
            Action::OpenSelected => {
                state.registry.repositories_filter_active = false;
                return true;
            }
            Action::MoveDown => {
                state.registry.repositories_filter_active = false;
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
                state.registry.repositories_cursor =
                    (state.registry.repositories_cursor + 1).min(len - 1);
            }
            true
        }
        Action::MoveUp => {
            state.registry.repositories_cursor =
                state.registry.repositories_cursor.saturating_sub(1);
            true
        }
        Action::HalfPageDown => {
            if len > 0 {
                state.registry.repositories_cursor =
                    (state.registry.repositories_cursor + HALF_PAGE).min(len - 1);
            }
            true
        }
        Action::HalfPageUp => {
            state.registry.repositories_cursor =
                state.registry.repositories_cursor.saturating_sub(HALF_PAGE);
            true
        }
        Action::GotoTop => {
            state.registry.repositories_cursor = 0;
            true
        }
        Action::GotoBottom => {
            if len > 0 {
                state.registry.repositories_cursor = len - 1;
            }
            true
        }
        Action::StartSearch => {
            state.registry.repositories_filter_active = true;
            true
        }
        Action::OpenSelected => {
            let repo = state
                .registry
                .filtered_repositories(&registry_id)
                .get(state.registry.repositories_cursor)
                .map(|r| r.name.clone());
            if let Some(name) = repo {
                state.registry.selected_repository = Some(name);
                state.registry.tags_cursor = 0;
                state.registry.tags_filter = tui_input::Input::default();
                state.view_stack.push(state.view);
                state.view = View::RegistryTags;
            }
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::registries::{Registry, Repository};
    use crate::config::Config;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn registry_fixture() -> Registry {
        Registry {
            id: "/subs/x/rg/y/cr/myreg".into(),
            name: "myreg".into(),
            resource_group: "rg".into(),
            subscription_id: "sub".into(),
            location: "westeurope".into(),
            sku: Some("Premium".into()),
            login_server: Some("myreg.azurecr.io".into()),
            admin_user_enabled: Some(false),
            public_network_access: Some("Enabled".into()),
            anonymous_pull_enabled: Some(false),
            created_at: None,
        }
    }

    fn fixture() -> AppState {
        let mut state = AppState::new(Config::default());
        state.view = View::RegistryRepositories;
        state.registry.selected_registry = Some(registry_fixture());
        state
    }

    fn repo(name: &str) -> Repository {
        Repository { name: name.into() }
    }

    #[test]
    fn renders_loading_when_pending() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(120, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state
            .registry
            .repositories_pending
            .insert("/subs/x/rg/y/cr/myreg".into());
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("loading repositories"));
    }

    #[test]
    fn renders_repositories() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(120, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.registry.repositories.insert(
            "/subs/x/rg/y/cr/myreg".into(),
            vec![repo("alpine"), repo("team/svc")],
        );
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("alpine"));
        assert!(buf.contains("team/svc"));
    }

    #[test]
    fn enter_pins_repository_and_drills_in() {
        let mut state = fixture();
        state
            .registry
            .repositories
            .insert("/subs/x/rg/y/cr/myreg".into(), vec![repo("alpine")]);
        assert!(handle(Action::OpenSelected, &mut state));
        assert_eq!(state.view, View::RegistryTags);
        assert_eq!(
            state.registry.selected_repository.as_deref(),
            Some("alpine")
        );
    }
}
