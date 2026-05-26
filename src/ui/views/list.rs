//! Resource list with fuzzy filter, favorites toggle, and a per-row health badge.

#![allow(dead_code, unused_variables)]

use chrono::{DateTime, Utc};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::azure::health::{derive, HealthStatus};
use crate::azure::logs::supports_logs;
use crate::azure::resources::{Resource, ResourceKind};
use crate::ui::events::Action;
use crate::ui::state::{subscription_display_name, AppState, View};
use crate::ui::theme::Theme;

const FOOTER_HINT: &str =
    "j/k move  Enter open  l logs  f fav  F favs-only  / search  s sub  r refresh  ? help  q quit";

const HALF_PAGE: usize = 10;

/// Fixed column widths for the resource list. Hard-coded so that columns
/// don't jump when the visible window changes which long names are on screen.
/// Names longer than this get truncated with an ellipsis; shorter names are
/// space-padded.
const NAME_COL_WIDTH: usize = 36;
const RG_COL_WIDTH: usize = 20;
/// Width of the SUBSCRIPTION column, shown only in all-subscriptions mode
/// (mirrors the Storage / Registries / Cosmos / Key Vault / Service Bus lists).
const SUB_COL_WIDTH: usize = 22;
/// Width of the `CREATED` / `MODIFIED` columns: `YYYY-MM-DD` is 10 chars. We
/// reserve exactly that — older resources with `None` for the timestamp render
/// as an empty column, which keeps the next column aligned.
const DATE_COL_WIDTH: usize = 10;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);

    // All status info (count, filter, favorites flag) folds into the block
    // title so it shares a line with the border instead of consuming its own
    // row. The breadcrumb at the very top of the screen already names the view.
    let mut title_spans: Vec<Span> = vec![Span::styled(
        " api resources ",
        Style::default().fg(theme.fg),
    )];
    title_spans.push(Span::styled(
        format!(
            "· {} of {} ",
            state.filtered_resources().len(),
            state.resources.len()
        ),
        Style::default().fg(theme.muted),
    ));
    if state.favorites_only {
        title_spans.push(Span::styled(
            "★ favorites ",
            Style::default().fg(theme.favorite),
        ));
    }
    if state.list_filter_active || !state.list_filter.value().is_empty() {
        title_spans.push(Span::styled(
            format!("/{} ", state.list_filter.value()),
            Style::default().fg(theme.accent),
        ));
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Line::from(title_spans));
    let inner = block.inner(chunks[0]);
    frame.render_widget(block, chunks[0]);

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
            "loading api resources …",
            Style::default().fg(theme.muted),
        )));
        frame.render_widget(p, list_area);
    } else if filtered.is_empty() {
        let msg = if state.favorites_only {
            "no favorites in this subscription. press f on a row to add one."
        } else if !state.list_filter.value().is_empty() {
            "no api resources match the current filter."
        } else {
            "no Function Apps, APIM instances, or Container Apps found."
        };
        let p = Paragraph::new(Line::from(Span::styled(
            msg,
            Style::default().fg(theme.muted),
        )));
        frame.render_widget(p, list_area);
    } else {
        // Carve off a 1-row header strip; the body gets the rest.
        let (header_area, body_area) = if list_area.height > 1 {
            let parts =
                Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(list_area);
            (Some(parts[0]), parts[1])
        } else {
            (None, list_area)
        };

        let max_name = NAME_COL_WIDTH;
        let max_rg = RG_COL_WIDTH;
        // Only worth a subscription column when viewing across all of them.
        let show_sub = state.selected_subscription.is_none();

        if let Some(ha) = header_area {
            let hdr = |text: &str, width: usize| {
                Span::styled(
                    format!("{text:<width$}"),
                    Style::default()
                        .fg(theme.muted)
                        .add_modifier(Modifier::BOLD),
                )
            };
            let mut header_spans = vec![
                Span::raw("    "), // selection prefix (2) + favorite glyph + space (2)
                hdr("NAME", max_name),
                Span::raw("  "),
                hdr("KIND", 4),
                Span::raw("    "), // badge glyph (●) + space + state column padding
                hdr("STATUS", 8),
                Span::raw("  "),
                hdr("RESOURCE GROUP", max_rg),
            ];
            if show_sub {
                header_spans.push(Span::raw("  "));
                header_spans.push(hdr("SUBSCRIPTION", SUB_COL_WIDTH));
            }
            header_spans.push(Span::raw("  "));
            header_spans.push(hdr("CREATED", DATE_COL_WIDTH));
            frame.render_widget(Paragraph::new(Line::from(header_spans)), ha);
        }

        let cursor = state.list_cursor.min(filtered.len() - 1);
        let visible = body_area.height as usize;
        let scroll = scroll_for(cursor, filtered.len(), visible);

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

                let created = format!(
                    "{:<width$}",
                    format_date(r.created_at.as_ref()),
                    width = DATE_COL_WIDTH
                );

                let mut spans = vec![
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

                if show_sub {
                    // Resolve to display name; fall back to the raw id until the
                    // subscription list arrives.
                    let sub = subscription_display_name(state, &r.subscription_id)
                        .unwrap_or(&r.subscription_id);
                    let sub = format!(
                        "{:<width$}",
                        truncate_right(sub, SUB_COL_WIDTH),
                        width = SUB_COL_WIDTH
                    );
                    spans.push(Span::raw("  "));
                    spans.push(Span::styled(sub, Style::default().fg(theme.muted)));
                }

                spans.push(Span::raw("  "));
                spans.push(Span::styled(created, Style::default().fg(theme.muted)));

                if selected {
                    Line::from(spans).style(theme.selection())
                } else {
                    Line::from(spans)
                }
            })
            .collect();

        frame.render_widget(Paragraph::new(lines), body_area);
    }

    let footer = Paragraph::new(Line::from(Span::styled(
        FOOTER_HINT,
        Style::default().fg(theme.muted),
    )));
    frame.render_widget(footer, chunks[1]);
}

pub(crate) fn badge_for_row(r: &Resource, state: &AppState, theme: &Theme) -> (Color, String) {
    if state.metrics.failures.contains_key(&r.id) {
        return (theme.critical, "ERROR".to_string());
    }
    let metrics = state.metrics.by_resource.get(&r.id);
    let availability = state.health.by_resource.get(&r.id).map(|a| a.state);

    // Both signals still loading: show LOADING. Otherwise feed `derive`
    // whatever's there and let the decision table take over.
    if metrics.is_none() && availability.is_none() {
        return (theme.muted, "LOADING…".to_string());
    }

    let m: &[crate::azure::metrics::MetricSeries] = metrics.map(|v| v.as_slice()).unwrap_or(&[]);
    let status = derive(m, r.state.as_deref(), availability);
    (color_for_health(status, theme), status.label().to_string())
}

fn color_for_health(status: HealthStatus, theme: &Theme) -> Color {
    match status {
        HealthStatus::Healthy => theme.healthy,
        HealthStatus::Idle => theme.idle,
        HealthStatus::Degraded => theme.degraded,
        HealthStatus::Critical => theme.critical,
        HealthStatus::Unknown => theme.unknown,
    }
}

/// Render an optional `DateTime<Utc>` as `YYYY-MM-DD` for the list view. Older
/// ARM resources pre-date `systemData` and surface as `None`; those collapse to
/// an empty string so the column stays blank rather than showing a placeholder.
fn format_date(dt: Option<&DateTime<Utc>>) -> String {
    match dt {
        Some(d) => d.format("%Y-%m-%d").to_string(),
        None => String::new(),
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
                // Vim-style: first Enter just defocuses the search box and hands
                // control to the filtered list. A second Enter (handled below)
                // opens the highlighted row.
                state.list_filter_active = false;
                return true;
            }
            Action::MoveDown => {
                // Vim-style: Down hands focus to the filtered list.
                state.list_filter_active = false;
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
            // Most kinds open the generic Detail view; Application Gateways
            // skip that level and land directly on their backend-pools view,
            // because "what's this gateway hooked up to?" is what the user
            // wants when they hit Enter on an AppGw row.
            if let Some(selected) = state.selected_resource() {
                let id = selected.id.clone();
                let kind = selected.kind;
                state.config.last_resource_id = Some(id);
                state.view_stack.push(state.view);
                state.view = match kind {
                    ResourceKind::AppGateway => {
                        state.appgw.cursor = 0;
                        View::AppGatewayBackends
                    }
                    _ => View::Detail,
                };
            }
            true
        }
        Action::OpenLogs => {
            if let Some(sel) = state.selected_resource() {
                if supports_logs(sel.kind) {
                    let id = sel.id.clone();
                    state.config.last_resource_id = Some(id);
                    state.view_stack.push(state.view);
                    state.view = View::Logs;
                } else {
                    state.set_status("logs are not supported for this resource type");
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
            created_at: None,
            modified_at: None,
            meta: Default::default(),
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
        let backend = TestBackend::new(120, 12);
        let mut term = Terminal::new(backend).unwrap();
        let state = fixture();
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("alpha-func"));
        assert!(s.contains("Func"));
        assert!(s.contains("LOADING"));
        // The renamed block title now reads "api resources".
        assert!(
            s.contains("api resources"),
            "expected api resources title in {s}"
        );
        // CREATED header column shipped — even if every row's date is empty.
        assert!(s.contains("CREATED"), "expected CREATED header in {s}");
    }

    #[test]
    fn renders_created_column_value_when_present() {
        use chrono::TimeZone;

        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(140, 12);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.resources[0].created_at = Some(Utc.with_ymd_and_hms(2024, 3, 15, 8, 30, 0).unwrap());
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(
            s.contains("2024-03-15"),
            "expected ISO date in row, got {s}"
        );
    }

    #[test]
    fn format_date_handles_none_and_some() {
        use chrono::TimeZone;
        assert_eq!(format_date(None), "");
        let d = Utc.with_ymd_and_hms(2026, 5, 21, 12, 0, 0).unwrap();
        assert_eq!(format_date(Some(&d)), "2026-05-21");
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
