//! Resource list with fuzzy filter, favorites toggle, and a per-row health badge.

#![allow(dead_code, unused_variables)]

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::azure::health::{derive, HealthStatus};
use crate::azure::logs::supports_logs;
use crate::azure::resources::Resource;
use crate::ui::events::Action;
use crate::ui::state::{AppState, View};
use crate::ui::theme::Theme;

const FOOTER_HINT: &str =
    "j/k move  Enter open  L logs  f fav  F favs-only  / search  s sub  r refresh  ? help  q quit";

const HALF_PAGE: usize = 10;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(area);

    // Header line with mode chips.
    let mut header_spans = vec![Span::styled(
        " APIs ",
        Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
    )];
    if state.favorites_only {
        header_spans.push(Span::styled(
            "★ favorites only ",
            Style::default().fg(theme.favorite),
        ));
    }
    if state.list_filter_active || !state.list_filter.value().is_empty() {
        header_spans.push(Span::styled(
            format!("/{} ", state.list_filter.value()),
            Style::default().fg(theme.fg),
        ));
    }
    header_spans.push(Span::styled(
        format!(
            "· {} of {}",
            state.filtered_resources().len(),
            state.resources.len()
        ),
        Style::default().fg(theme.muted),
    ));
    frame.render_widget(Paragraph::new(Line::from(header_spans)), chunks[0]);

    // Body: bordered block.
    let title = if state.list_filter_active {
        " resources (search) ".to_string()
    } else {
        " resources ".to_string()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(title, Style::default().fg(theme.fg)));
    let inner = block.inner(chunks[1]);
    frame.render_widget(block, chunks[1]);

    // Optionally split off a top row for the search input.
    let (search_area, list_area) = if state.list_filter_active {
        let parts = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(inner);
        (Some(parts[0]), parts[1])
    } else {
        (None, inner)
    };

    if let Some(sa) = search_area {
        let p = Paragraph::new(Line::from(vec![
            Span::styled("> ", Style::default().fg(theme.accent)),
            Span::styled(state.list_filter.value(), Style::default().fg(theme.fg)),
            Span::styled("█", Style::default().fg(theme.accent)),
        ]));
        frame.render_widget(p, sa);
    }

    let filtered = state.filtered_resources();

    if state.loading_resources && filtered.is_empty() {
        let p = Paragraph::new(Line::from(Span::styled(
            "loading resources …",
            Style::default().fg(theme.muted),
        )));
        frame.render_widget(p, list_area);
    } else if filtered.is_empty() {
        let msg = if state.favorites_only {
            "no favorites in this subscription. press f on a row to add one."
        } else if !state.list_filter.value().is_empty() {
            "no resources match the current filter."
        } else {
            "no Function Apps, APIM instances, or Container Apps found."
        };
        let p = Paragraph::new(Line::from(Span::styled(
            msg,
            Style::default().fg(theme.muted),
        )));
        frame.render_widget(p, list_area);
    } else {
        let cursor = state.list_cursor.min(filtered.len() - 1);
        let visible = list_area.height as usize;
        let scroll = scroll_for(cursor, filtered.len(), visible);

        // Compute name column width based on visible rows.
        let max_name = filtered
            .iter()
            .skip(scroll)
            .take(visible)
            .map(|r| r.name.chars().count())
            .max()
            .unwrap_or(0)
            .clamp(8, 36);

        let max_rg = filtered
            .iter()
            .skip(scroll)
            .take(visible)
            .map(|r| r.resource_group.chars().count())
            .max()
            .unwrap_or(0)
            .clamp(6, 24);

        let lines: Vec<Line> = filtered
            .iter()
            .enumerate()
            .skip(scroll)
            .take(visible)
            .map(|(i, r)| {
                let selected = i == cursor;
                let fav_glyph = if state.config.is_favorite(&r.id) {
                    Span::styled("★", Style::default().fg(theme.favorite))
                } else {
                    Span::raw(" ")
                };

                let name = format!(
                    "{:<width$}",
                    truncate_right(&r.name, max_name),
                    width = max_name
                );

                let kind_tag = format!("{:<4}", r.kind.short_tag());

                let (badge_color, badge_label) = badge_for_row(r, state, theme);

                let rg = format!(
                    "{:<width$}",
                    truncate_right(&r.resource_group, max_rg),
                    width = max_rg
                );

                let spans = vec![
                    Span::raw(if selected { "▍ " } else { "  " }),
                    fav_glyph,
                    Span::raw(" "),
                    Span::styled(name, Style::default().fg(theme.fg)),
                    Span::raw("  "),
                    Span::styled(kind_tag, Style::default().fg(theme.accent)),
                    Span::raw("  "),
                    Span::styled("●", Style::default().fg(badge_color)),
                    Span::raw(" "),
                    Span::styled(
                        format!("{:<8}", badge_label),
                        Style::default().fg(badge_color),
                    ),
                    Span::raw("  "),
                    Span::styled(rg, Style::default().fg(theme.muted)),
                ];

                if selected {
                    Line::from(spans).style(theme.selection())
                } else {
                    Line::from(spans)
                }
            })
            .collect();

        frame.render_widget(Paragraph::new(lines), list_area);
    }

    let footer = Paragraph::new(Line::from(Span::styled(
        FOOTER_HINT,
        Style::default().fg(theme.muted),
    )));
    frame.render_widget(footer, chunks[2]);
}

fn badge_for_row(r: &Resource, state: &AppState, theme: &Theme) -> (Color, String) {
    if state.metrics.failures.contains_key(&r.id) {
        return (theme.critical, "ERROR".to_string());
    }
    match state.metrics.by_resource.get(&r.id) {
        Some(metrics) => {
            let status = derive(metrics, r.state.as_deref());
            (color_for_health(status, theme), status.label().to_string())
        }
        None => (theme.muted, "LOADING…".to_string()),
    }
}

fn color_for_health(status: HealthStatus, theme: &Theme) -> Color {
    match status {
        HealthStatus::Healthy => theme.healthy,
        HealthStatus::Degraded => theme.degraded,
        HealthStatus::Critical => theme.critical,
        HealthStatus::Unknown => theme.unknown,
    }
}

fn truncate_right(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    if max <= 1 {
        return "…".to_string();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

fn scroll_for(cursor: usize, len: usize, visible: usize) -> usize {
    if visible == 0 || len <= visible {
        return 0;
    }
    if cursor < visible {
        return 0;
    }
    (cursor + 1).saturating_sub(visible).min(len - visible)
}

pub fn handle(action: Action, state: &mut AppState) -> bool {
    let filtered_len = state.filtered_resources().len();

    // While the search box is active, swallow nav/special actions and let Lane 3
    // forward raw key events into `list_filter` for editing. The set we still
    // handle here is limited to ones that should affect the underlying list.
    if state.list_filter_active {
        match action {
            Action::Back => {
                state.list_filter_active = false;
                return true;
            }
            Action::OpenSelected => {
                // Pressing Enter while searching commits the filter and opens.
                state.list_filter_active = false;
                if state.selected_resource().is_some() {
                    state.view_stack.push(state.view);
                    state.view = View::Detail;
                }
                return true;
            }
            Action::MoveDown
            | Action::MoveUp
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
            if filtered_len > 0 {
                state.list_cursor = (state.list_cursor + 1).min(filtered_len - 1);
            }
            true
        }
        Action::MoveUp => {
            state.list_cursor = state.list_cursor.saturating_sub(1);
            true
        }
        Action::HalfPageDown => {
            if filtered_len > 0 {
                state.list_cursor = (state.list_cursor + HALF_PAGE).min(filtered_len - 1);
            }
            true
        }
        Action::HalfPageUp => {
            state.list_cursor = state.list_cursor.saturating_sub(HALF_PAGE);
            true
        }
        Action::GotoTop => {
            state.list_cursor = 0;
            true
        }
        Action::GotoBottom => {
            if filtered_len > 0 {
                state.list_cursor = filtered_len - 1;
            }
            true
        }
        Action::OpenSelected => {
            if state.selected_resource().is_some() {
                state.view_stack.push(state.view);
                state.view = View::Detail;
            }
            true
        }
        Action::OpenLogs => {
            if let Some(sel) = state.selected_resource() {
                if supports_logs(sel.kind) {
                    state.view_stack.push(state.view);
                    state.view = View::Logs;
                } else {
                    state.status_message =
                        Some("logs are not supported for this resource type".to_string());
                }
            }
            true
        }
        Action::ToggleFavorite => {
            if let Some(sel) = state.selected_resource() {
                let id = sel.id.clone();
                state.config.toggle_favorite(&id);
            }
            true
        }
        Action::ToggleFavoritesOnly => {
            state.favorites_only = !state.favorites_only;
            state.list_cursor = 0;
            true
        }
        Action::StartSearch => {
            state.list_filter_active = true;
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::resources::{Resource, ResourceKind};
    use crate::config::Config;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn r(id: &str, name: &str, kind: ResourceKind) -> Resource {
        Resource {
            id: id.into(),
            name: name.into(),
            kind,
            location: "westeurope".into(),
            resource_group: "rg-demo".into(),
            subscription_id: "sub".into(),
            state: Some("Running".into()),
        }
    }

    fn fixture() -> AppState {
        let mut state = AppState::new(Config::default());
        state.resources = vec![
            r("/r/one", "alpha-func", ResourceKind::FunctionApp),
            r("/r/two", "beta-apim", ResourceKind::Apim),
            r("/r/three", "gamma-ctra", ResourceKind::ContainerApp),
        ];
        state
    }

    #[test]
    fn renders_without_panic() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(100, 12);
        let mut term = Terminal::new(backend).unwrap();
        let state = fixture();
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("alpha-func"));
        assert!(s.contains("Func"));
        assert!(s.contains("LOADING"));
    }

    #[test]
    fn renders_search_box() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(100, 12);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.list_filter_active = true;
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains(">"));
    }

    #[test]
    fn renders_empty_state() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(80, 8);
        let mut term = Terminal::new(backend).unwrap();
        let state = AppState::new(Config::default());
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
    }

    #[test]
    fn navigation_clamped() {
        let mut state = fixture();
        assert!(handle(Action::MoveDown, &mut state));
        assert_eq!(state.list_cursor, 1);
        assert!(handle(Action::GotoBottom, &mut state));
        assert_eq!(state.list_cursor, 2);
        assert!(handle(Action::MoveDown, &mut state));
        assert_eq!(state.list_cursor, 2);
        assert!(handle(Action::GotoTop, &mut state));
        assert_eq!(state.list_cursor, 0);
    }

    #[test]
    fn toggle_favorite_mutates_config() {
        let mut state = fixture();
        assert!(!state.config.is_favorite("/r/one"));
        assert!(handle(Action::ToggleFavorite, &mut state));
        assert!(state.config.is_favorite("/r/one"));
        assert!(handle(Action::ToggleFavorite, &mut state));
        assert!(!state.config.is_favorite("/r/one"));
    }

    #[test]
    fn open_logs_blocks_apim() {
        let mut state = fixture();
        state.view = View::List;
        // cursor 0 is FunctionApp (supports logs); cursor 1 is APIM (not).
        state.list_cursor = 1;
        assert!(handle(Action::OpenLogs, &mut state));
        assert_eq!(state.view, View::List);
        assert!(state.status_message.is_some());
    }

    #[test]
    fn open_logs_allows_function_app() {
        let mut state = fixture();
        state.list_cursor = 0;
        assert!(handle(Action::OpenLogs, &mut state));
        assert_eq!(state.view, View::Logs);
    }

    #[test]
    fn open_logs_preserves_list_cursor() {
        // Regression: previously, OpenLogs reset list_cursor to 0, which caused
        // the Logs view to dispatch loads against filtered_resources[0] instead
        // of the user's highlighted row. The cursor must survive the trip.
        let mut state = fixture();
        // cursor 2 -> gamma-ctra (ContainerApp, supports logs)
        state.list_cursor = 2;
        assert!(handle(Action::OpenLogs, &mut state));
        assert_eq!(state.view, View::Logs);
        assert_eq!(state.list_cursor, 2);
    }

    #[test]
    fn start_search_sets_flag() {
        let mut state = fixture();
        assert!(handle(Action::StartSearch, &mut state));
        assert!(state.list_filter_active);
    }

    #[test]
    fn favorites_only_toggle() {
        let mut state = fixture();
        assert!(handle(Action::ToggleFavoritesOnly, &mut state));
        assert!(state.favorites_only);
    }
}
