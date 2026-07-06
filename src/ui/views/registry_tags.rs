//! Container Registry tags drill-in: lists tags for the pinned repository
//! inside the pinned registry. Terminal leaf in the registry chain.

#![allow(dead_code, unused_variables)]

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;

use crate::ui::events::Action;
use crate::ui::state::{AppState, RegistryCache};
use crate::ui::theme::Theme;

const FOOTER_HINT: &str = "j/k move  / filter  Esc back  r refresh  y yank ref  ? help  q quit";
const HALF_PAGE: usize = 10;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);

    let filter_value = state.registry.tags_filter.value();
    let filter_active = state.registry.tags_filter_active;
    let registry = state.registry.selected_registry.as_ref();
    let repository = state.registry.selected_repository.as_deref();
    let key = registry
        .zip(repository)
        .map(|(r, repo)| RegistryCache::tags_key(&r.id, repo));
    let total = key
        .as_ref()
        .and_then(|k| state.registry.tags.get(k))
        .map(|v| v.len());
    let filtered = match (registry, repository) {
        (Some(r), Some(repo)) => state.registry.filtered_tags(&r.id, repo),
        _ => Vec::new(),
    };
    let count_label = match total {
        Some(t) if !filter_value.is_empty() => format!("· {} of {} ", filtered.len(), t),
        Some(t) => format!("· {t} "),
        None => String::new(),
    };
    let title_repo = repository.unwrap_or("?");
    let mut title_spans: Vec<Span> = vec![
        Span::styled(
            format!(" tags · {title_repo} "),
            Style::default().fg(theme.fg),
        ),
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

    let Some(key) = key else {
        let p = Paragraph::new(Line::from(Span::styled(
            "no repository selected.",
            Style::default().fg(theme.muted),
        )));
        frame.render_widget(p, body_area);
        render_footer(frame, chunks[1], theme);
        return;
    };

    if let Some(err) = state.registry.tags_error.get(&key) {
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

    let tags = state.registry.tags.get(&key);
    let loading = state.registry.tags_pending.contains(&key);
    match tags {
        None if loading => {
            let p = Paragraph::new(Line::from(Span::styled(
                "loading tags …",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        None => {
            let p = Paragraph::new(Line::from(Span::styled(
                "press r to load tags.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(rows) if rows.is_empty() => {
            let p = Paragraph::new(Line::from(Span::styled(
                "no tags pushed for this repository.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(_) if filtered.is_empty() => {
            let p = Paragraph::new(Line::from(Span::styled(
                "no tags match the current filter.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(_) => {
            let widths = [Constraint::Min(8)];
            let header_row = Row::new(vec!["TAG"]).style(
                Style::default()
                    .fg(theme.muted)
                    .add_modifier(Modifier::BOLD),
            );

            let body_rows: Vec<Row> = filtered
                .iter()
                .map(|t| {
                    Row::new(vec![
                        Cell::from(t.name.as_str()).style(Style::default().fg(theme.fg))
                    ])
                })
                .collect();

            let cursor = state.registry.tags_cursor.min(filtered.len() - 1);
            let table = Table::new(body_rows, widths)
                .header(header_row)
                .row_highlight_style(theme.selection())
                .highlight_symbol("▍ ")
                .column_spacing(2);

            // Offset persisted across frames so the window only scrolls when the
            // cursor pushes against an edge (ratatui reconciles it during render).
            let mut ts = TableState::default().with_offset(state.registry.tags_view_top.get());
            ts.select(Some(cursor));
            frame.render_stateful_widget(table, body_area, &mut ts);
            state.registry.tags_view_top.set(ts.offset());
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

/// Fully-qualified `{login_server}/{repo}:{tag}` for the currently-highlighted
/// tag. Returned by the global yank handler as the natural copy target — this
/// is the string a user is most likely to paste into a `docker pull`.
pub fn yank_text(state: &AppState) -> Option<String> {
    let registry = state.registry.selected_registry.as_ref()?;
    let repository = state.registry.selected_repository.as_deref()?;
    let tag = state
        .registry
        .filtered_tags(&registry.id, repository)
        .get(state.registry.tags_cursor)
        .map(|t| t.name.clone())?;
    Some(format!(
        "{}/{}:{}",
        registry.login_server_or_default(),
        repository,
        tag
    ))
}

pub fn handle(action: Action, state: &mut AppState) -> bool {
    let (Some(registry_id), Some(repository)) = (
        state
            .registry
            .selected_registry
            .as_ref()
            .map(|r| r.id.clone()),
        state.registry.selected_repository.clone(),
    ) else {
        return false;
    };
    let len = state
        .registry
        .filtered_tags(&registry_id, &repository)
        .len();

    if state.registry.tags_filter_active {
        match action {
            Action::Back => {
                state.registry.tags_filter_active = false;
                state.registry.tags_filter.reset();
                state.registry.tags_cursor = 0;
                return true;
            }
            Action::OpenSelected => {
                state.registry.tags_filter_active = false;
                return true;
            }
            Action::MoveDown => {
                state.registry.tags_filter_active = false;
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
                state.registry.tags_cursor = (state.registry.tags_cursor + 1).min(len - 1);
            }
            true
        }
        Action::MoveUp => {
            state.registry.tags_cursor = state.registry.tags_cursor.saturating_sub(1);
            true
        }
        Action::HalfPageDown => {
            if len > 0 {
                state.registry.tags_cursor = (state.registry.tags_cursor + HALF_PAGE).min(len - 1);
            }
            true
        }
        Action::HalfPageUp => {
            state.registry.tags_cursor = state.registry.tags_cursor.saturating_sub(HALF_PAGE);
            true
        }
        Action::GotoTop => {
            state.registry.tags_cursor = 0;
            true
        }
        Action::GotoBottom => {
            if len > 0 {
                state.registry.tags_cursor = len - 1;
            }
            true
        }
        Action::StartSearch => {
            state.registry.tags_filter_active = true;
            true
        }
        // `Enter` is a leaf no-op for now: there's no detail view past tags.
        // We swallow it to prevent the global handler from doing something
        // surprising (e.g. closing a modal we don't own).
        Action::OpenSelected => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::registries::{Registry, Tag};
    use crate::config::Config;
    use crate::ui::state::View;
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
        state.view = View::RegistryTags;
        state.registry.selected_registry = Some(registry_fixture());
        state.registry.selected_repository = Some("alpine".into());
        state
    }

    fn tag(name: &str) -> Tag {
        Tag { name: name.into() }
    }

    #[test]
    fn renders_tags() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(100, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.registry.tags.insert(
            RegistryCache::tags_key("/subs/x/rg/y/cr/myreg", "alpine"),
            vec![tag("latest"), tag("3.18")],
        );
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("latest"));
        assert!(buf.contains("3.18"));
        assert!(buf.contains("alpine"), "title should include the repo name");
    }

    #[test]
    fn yank_returns_docker_pull_ref() {
        let mut state = fixture();
        state.registry.tags.insert(
            RegistryCache::tags_key("/subs/x/rg/y/cr/myreg", "alpine"),
            vec![tag("latest")],
        );
        assert_eq!(
            yank_text(&state).as_deref(),
            Some("myreg.azurecr.io/alpine:latest"),
        );
    }

    #[test]
    fn renders_loading_state() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(100, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state
            .registry
            .tags_pending
            .insert(RegistryCache::tags_key("/subs/x/rg/y/cr/myreg", "alpine"));
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("loading tags"));
    }
}
