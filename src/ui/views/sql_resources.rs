//! Top-level Azure SQL entry view: one flat list of elastic pools + single
//! databases visible to the current subscription scope. Pressing Enter pins
//! the row into `state.sql.selected` and opens [`View::SqlDetail`] (the
//! utilization sparklines).

#![allow(dead_code, unused_variables)]

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};
use ratatui::Frame;

use super::{name_col_width, truncate_ellipsis};
use crate::azure::sql::{SqlKind, SqlResource};
use crate::ui::events::Action;
use crate::ui::state::{subscription_display_name, AppState, View};
use crate::ui::theme::Theme;

const FOOTER_HINT: &str =
    "j/k move  Enter metrics  / filter  Esc back  r refresh  o portal  y yank id  ? help  q quit";
const HALF_PAGE: usize = 10;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);

    let filter_value = state.sql.filter.value();
    let filter_active = state.sql.filter_active;
    let total = state.sql.resources.as_ref().map(|v| v.len());
    let filtered = state.sql.filtered_resources();
    let count_label = match total {
        Some(t) if !filter_value.is_empty() => format!("· {} of {} ", filtered.len(), t),
        Some(t) => format!("· {t} "),
        None => String::new(),
    };
    let mut title_spans: Vec<Span> = vec![
        Span::styled(
            " azure sql (pools + databases) ",
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

    if let Some(err) = state.sql.error.as_deref() {
        let p = Paragraph::new(Line::from(Span::styled(
            format!("error: {err}"),
            Style::default().fg(theme.critical),
        )));
        frame.render_widget(p, body_area);
        render_footer(frame, chunks[1], theme);
        return;
    }

    match state.sql.resources.as_deref() {
        None if state.sql.pending => {
            let p = Paragraph::new(Line::from(Span::styled(
                "loading azure sql pools and databases …",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        None => {
            let p = Paragraph::new(Line::from(Span::styled(
                "press r to load azure sql resources.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some([]) => {
            let p = Paragraph::new(Line::from(Span::styled(
                "No SQL elastic pools or databases found in selected subscriptions.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(_) if filtered.is_empty() => {
            let p = Paragraph::new(Line::from(Span::styled(
                "no sql resources match the current filter.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(_) => {
            let show_sub_cols = state.selected_subscription.is_none();

            // NAME absorbs the leftover width; `fixed_w` sums the Length()s
            // below — keep them in sync.
            let fixed_w: u16 = 10 + 28 + 16 + 8 + 12 + if show_sub_cols { 22 } else { 0 };
            let n_cols: u16 = if show_sub_cols { 7 } else { 6 };
            let longest = filtered
                .iter()
                .map(|r| r.name.chars().count() as u16)
                .max()
                .unwrap_or(0);
            let name_w = name_col_width(body_area.width, fixed_w, n_cols, longest);

            let mut widths: Vec<Constraint> = vec![
                Constraint::Length(name_w), // NAME
                Constraint::Length(10),     // KIND
                Constraint::Length(28),     // SERVER
                Constraint::Length(16),     // SKU
                Constraint::Length(8),      // CAP
                Constraint::Length(12),     // STATUS
            ];
            let mut headers: Vec<&'static str> =
                vec!["NAME", "KIND", "SERVER", "SKU", "CAP", "STATUS"];
            if show_sub_cols {
                widths.push(Constraint::Length(22));
                headers.push("SUB NAME");
            }

            let header_row = Row::new(headers).style(
                Style::default()
                    .fg(theme.muted)
                    .add_modifier(Modifier::BOLD),
            );

            let cursor = state.sql.cursor.min(filtered.len() - 1);
            let body_rows: Vec<Row> = filtered
                .iter()
                .map(|r| build_row(r, state, show_sub_cols, name_w, theme))
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
    r: &'a SqlResource,
    state: &'a AppState,
    show_sub_cols: bool,
    name_w: u16,
    theme: &Theme,
) -> Row<'a> {
    // Pools and databases get distinct accents so the flat list stays scannable.
    let kind_color = match r.kind {
        SqlKind::ElasticPool => theme.accent,
        SqlKind::Database => theme.fg,
    };
    let status = match r.status.as_deref() {
        Some(s) if s.eq_ignore_ascii_case("online") || s.eq_ignore_ascii_case("ready") => {
            Cell::from(s.to_string()).style(Style::default().fg(theme.healthy))
        }
        Some(s) if s.eq_ignore_ascii_case("paused") || s.eq_ignore_ascii_case("disabled") => {
            Cell::from(s.to_string()).style(Style::default().fg(theme.idle))
        }
        Some(s) => Cell::from(s.to_string()).style(Style::default().fg(theme.fg)),
        None => Cell::from("—").style(Style::default().fg(theme.muted)),
    };
    let cap = match r.capacity {
        Some(c) => c.to_string(),
        None => String::new(),
    };

    let mut cells: Vec<Cell> = vec![
        Cell::from(truncate_ellipsis(&r.name, name_w as usize))
            .style(Style::default().fg(theme.fg)),
        Cell::from(r.kind.short_tag()).style(Style::default().fg(kind_color)),
        Cell::from(truncate_ellipsis(&r.server, 28)).style(Style::default().fg(theme.muted)),
        Cell::from(r.sku_name.clone().unwrap_or_default()).style(Style::default().fg(theme.muted)),
        Cell::from(cap).style(Style::default().fg(theme.muted)),
        status,
    ];
    if show_sub_cols {
        cells.push(
            Cell::from(
                subscription_display_name(state, &r.subscription_id)
                    .unwrap_or("")
                    .to_string(),
            )
            .style(Style::default().fg(theme.muted)),
        );
    }
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
    let len = state.sql.filtered_resources().len();

    if state.sql.filter_active {
        match action {
            Action::Back => {
                state.sql.filter_active = false;
                state.sql.filter.reset();
                state.sql.cursor = 0;
                return true;
            }
            Action::OpenSelected => {
                state.sql.filter_active = false;
                return true;
            }
            Action::MoveDown => {
                state.sql.filter_active = false;
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
                state.sql.cursor = (state.sql.cursor + 1).min(len - 1);
            }
            true
        }
        Action::MoveUp => {
            state.sql.cursor = state.sql.cursor.saturating_sub(1);
            true
        }
        Action::HalfPageDown => {
            if len > 0 {
                state.sql.cursor = (state.sql.cursor + HALF_PAGE).min(len - 1);
            }
            true
        }
        Action::HalfPageUp => {
            state.sql.cursor = state.sql.cursor.saturating_sub(HALF_PAGE);
            true
        }
        Action::GotoTop => {
            state.sql.cursor = 0;
            true
        }
        Action::GotoBottom => {
            if len > 0 {
                state.sql.cursor = len - 1;
            }
            true
        }
        Action::StartSearch => {
            state.sql.filter_active = true;
            true
        }
        Action::OpenSelected => {
            if let Some(resource) = state.sql.selected_in_list() {
                state.sql.selected = Some(resource);
                state.view_stack.push(state.view);
                state.view = View::SqlDetail;
            }
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::sql::{SqlKind, SqlResource};
    use crate::config::Config;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn fixture() -> AppState {
        let mut state = AppState::new(Config::default());
        state.view = View::SqlResources;
        state
    }

    fn pool(name: &str) -> SqlResource {
        SqlResource {
            id: format!(
                "/subscriptions/s/resourceGroups/rg/providers/Microsoft.Sql/servers/srv/elasticPools/{name}"
            ),
            name: name.into(),
            server: "srv".into(),
            resource_group: "rg".into(),
            subscription_id: "s".into(),
            location: "westeurope".into(),
            kind: SqlKind::ElasticPool,
            sku_name: Some("StandardPool".into()),
            sku_tier: Some("Standard".into()),
            capacity: Some(100),
            status: Some("Ready".into()),
            elastic_pool_id: None,
            max_size_bytes: None,
        }
    }

    #[test]
    fn renders_loading_state() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(160, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.sql.pending = true;
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("loading azure sql"));
    }

    #[test]
    fn renders_pool_row() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(180, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.sql.resources = Some(vec![pool("pool-a")]);
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("pool-a"), "name renders");
        assert!(buf.contains("Pool"), "kind tag renders");
        assert!(buf.contains("Ready"), "status renders");
    }

    #[test]
    fn enter_pins_resource_and_opens_detail() {
        let mut state = fixture();
        state.sql.resources = Some(vec![pool("pool-a")]);
        assert!(handle(Action::OpenSelected, &mut state));
        assert_eq!(state.view, View::SqlDetail);
        assert_eq!(
            state.sql.selected.as_ref().map(|r| r.name.as_str()),
            Some("pool-a")
        );
    }

    #[test]
    fn filter_narrows_and_resets_via_esc() {
        let mut state = fixture();
        state.sql.resources = Some(vec![pool("pool-a"), pool("other")]);
        state.sql.filter = tui_input::Input::new("pool".into());
        assert_eq!(state.sql.filtered_resources().len(), 1);
        state.sql.filter_active = true;
        assert!(handle(Action::Back, &mut state));
        assert_eq!(state.sql.filter.value(), "");
    }
}
