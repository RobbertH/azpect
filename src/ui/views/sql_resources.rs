//! Top-level Azure SQL entry view: one flat list of elastic pools + single
//! databases visible to the current subscription scope. Pressing Enter pins
//! the row into `state.sql.selected` and opens [`View::SqlDetail`] (the
//! utilization sparklines).

#![allow(dead_code, unused_variables)]

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;

use super::{col_width, truncate_ellipsis};
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
            let layout = plan_columns(&filtered, body_area.width, show_sub_cols, state);

            let mut widths: Vec<Constraint> = vec![Constraint::Length(layout.name_w)];
            let mut headers: Vec<&'static str> = vec!["NAME"];
            for (w, header) in [
                (layout.kind, "KIND"),
                (layout.server, "SERVER"),
                (layout.sku, "SKU"),
                (layout.cap, "CAP"),
                (layout.status, "STATUS"),
                (layout.sub, "SUB NAME"),
            ] {
                if let Some(w) = w {
                    widths.push(Constraint::Length(w));
                    headers.push(header);
                }
            }

            let header_row = Row::new(headers).style(
                Style::default()
                    .fg(theme.muted)
                    .add_modifier(Modifier::BOLD),
            );

            let cursor = state.sql.cursor.min(filtered.len() - 1);
            let body_rows: Vec<Row> = filtered
                .iter()
                .map(|r| build_row(r, state, &layout, theme))
                .collect();

            let table = Table::new(body_rows, widths)
                .header(header_row)
                .row_highlight_style(theme.selection())
                .highlight_symbol("▍ ")
                .column_spacing(2);

            // Offset persisted across frames so the window only scrolls when the
            // cursor pushes against an edge (ratatui reconciles it during render).
            let mut ts = TableState::default().with_offset(state.sql.view_top.get());
            ts.select(Some(cursor));
            frame.render_stateful_widget(table, body_area, &mut ts);
            state.sql.view_top.set(ts.offset());
        }
    }

    render_footer(frame, chunks[1], theme);
}

/// Chosen widths for one render. NAME is mandatory; every other column is
/// `Some(width)` when it earned a slot or `None` when it was dropped to keep the
/// table inside the terminal. See [`plan_columns`].
struct ColLayout {
    name_w: u16,
    kind: Option<u16>,
    server: Option<u16>,
    sku: Option<u16>,
    cap: Option<u16>,
    status: Option<u16>,
    sub: Option<u16>,
}

/// Decide column widths for the SQL table. NAME is sacred — it's the only column
/// that uniquely identifies a row — so it's never ellipsized: it gets its full
/// longest value and the remaining columns are added in importance order, each
/// only if it still fits. The least important (SUB NAME, then SERVER, …) are the
/// first to be dropped on a narrow terminal, rather than chopping NAME into an
/// unreadable stub. NAME is clipped only in the degenerate case where it alone
/// exceeds the terminal width.
fn plan_columns(
    filtered: &[&SqlResource],
    area_width: u16,
    show_sub_cols: bool,
    state: &AppState,
) -> ColLayout {
    let max_len = |f: &dyn Fn(&SqlResource) -> usize| -> u16 {
        filtered.iter().map(|r| f(r) as u16).max().unwrap_or(0)
    };
    let kind_w = col_width("KIND", max_len(&|r| r.kind.short_tag().chars().count()), 10);
    let server_w = col_width("SERVER", max_len(&|r| r.server.chars().count()), 28);
    let sku_w = col_width(
        "SKU",
        max_len(&|r| r.sku_name.as_deref().unwrap_or("").chars().count()),
        16,
    );
    let cap_w = col_width(
        "CAP",
        max_len(&|r| r.capacity.map_or(0, |c| c.to_string().len())),
        8,
    );
    let status_w = col_width(
        "STATUS",
        max_len(&|r| r.status.as_deref().unwrap_or("—").chars().count()),
        12,
    );
    let sub_w = if show_sub_cols {
        col_width(
            "SUB NAME",
            max_len(&|r| {
                subscription_display_name(state, &r.subscription_id)
                    .unwrap_or("")
                    .chars()
                    .count()
            }),
            22,
        )
    } else {
        0
    };

    // Chrome before any cell: the selection symbol "▍ " (2 cols). Each column
    // also costs 2 cols of spacing to its left neighbour.
    let longest_name = max_len(&|r| r.name.chars().count());
    let name_budget = area_width.saturating_sub(2);
    let name_w = longest_name.max(4).min(name_budget.max(4));

    // Display-order slots: 0 KIND, 1 SERVER, 2 SKU, 3 CAP, 4 STATUS, 5 SUB NAME.
    let widths = [kind_w, server_w, sku_w, cap_w, status_w, sub_w];
    let mut kept = [None; 6];
    // Importance order (most important first), as display-slot indices. We add
    // columns until one no longer fits, then stop — so the kept set is always a
    // prefix of this ranking: a less important column never survives while a more
    // important one is dropped. SUB NAME (5) is therefore the first to go.
    let mut used = 2 + name_w;
    for &slot in &[0usize, 4, 2, 3, 1, 5] {
        let w = widths[slot];
        if w == 0 {
            continue; // hidden column (SUB NAME when a single sub is selected)
        }
        if used + 2 + w > area_width {
            break;
        }
        used += 2 + w;
        kept[slot] = Some(w);
    }
    ColLayout {
        name_w,
        kind: kept[0],
        server: kept[1],
        sku: kept[2],
        cap: kept[3],
        status: kept[4],
        sub: kept[5],
    }
}

fn build_row<'a>(
    r: &'a SqlResource,
    state: &'a AppState,
    layout: &ColLayout,
    theme: &Theme,
) -> Row<'a> {
    // Pools and databases get distinct accents so the flat list stays scannable.
    let kind_color = match r.kind {
        SqlKind::ElasticPool => theme.accent,
        SqlKind::Database => theme.fg,
    };
    let status_style = match r.status.as_deref() {
        Some(s) if s.eq_ignore_ascii_case("online") || s.eq_ignore_ascii_case("ready") => {
            Style::default().fg(theme.healthy)
        }
        Some(s) if s.eq_ignore_ascii_case("paused") || s.eq_ignore_ascii_case("disabled") => {
            Style::default().fg(theme.idle)
        }
        Some(_) => Style::default().fg(theme.fg),
        None => Style::default().fg(theme.muted),
    };

    let mut cells: Vec<Cell> = vec![
        Cell::from(truncate_ellipsis(&r.name, layout.name_w as usize))
            .style(Style::default().fg(theme.fg)),
    ];
    if layout.kind.is_some() {
        cells.push(Cell::from(r.kind.short_tag()).style(Style::default().fg(kind_color)));
    }
    if let Some(w) = layout.server {
        cells.push(
            Cell::from(truncate_ellipsis(&r.server, w as usize))
                .style(Style::default().fg(theme.muted)),
        );
    }
    if let Some(w) = layout.sku {
        cells.push(
            Cell::from(truncate_ellipsis(
                r.sku_name.as_deref().unwrap_or(""),
                w as usize,
            ))
            .style(Style::default().fg(theme.muted)),
        );
    }
    if layout.cap.is_some() {
        let cap = r.capacity.map(|c| c.to_string()).unwrap_or_default();
        cells.push(Cell::from(cap).style(Style::default().fg(theme.muted)));
    }
    if layout.status.is_some() {
        cells.push(Cell::from(r.status.as_deref().unwrap_or("—").to_string()).style(status_style));
    }
    if let Some(w) = layout.sub {
        cells.push(
            Cell::from(truncate_ellipsis(
                subscription_display_name(state, &r.subscription_id).unwrap_or(""),
                w as usize,
            ))
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
            state.sql.filter.reset();
            state.sql.cursor = 0;
            state.sql.filter_active = true;
            true
        }
        Action::OpenSelected => {
            if let Some(resource) = state.sql.selected_in_list() {
                state.sql.selected = Some(resource);
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
    fn narrow_terminal_shows_full_name_and_drops_sub_column() {
        // Regression: on a narrow terminal NAME used to be chopped into a 3-char
        // ellipsis ("D36…") while every other column kept its slot. NAME is now
        // never ellipsized — the least important columns (SUB NAME first) are
        // dropped to make room.
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(72, 6);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        // Distinct subscriptions so SUB NAME would otherwise show (show_sub_cols).
        let mut a = pool("database-with-a-long-name");
        a.server = "imecd365replicationv2".into();
        let mut b = pool("other");
        b.subscription_id = "s2".into();
        state.sql.resources = Some(vec![a, b]);
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        // NAME renders in full, with no ellipsis anywhere in the buffer.
        assert!(
            buf.contains("database-with-a-long-name"),
            "name renders in full"
        );
        assert!(!buf.contains('\u{2026}'), "nothing is ellipsized");
        // The least important column was dropped to make room.
        assert!(
            !buf.contains("SUB NAME"),
            "SUB NAME column dropped when cramped"
        );
    }

    #[test]
    fn plan_columns_drops_sub_before_chopping_name() {
        let state = fixture();
        let mut a = pool("a-fairly-long-database-name");
        a.server = "imecd365replicationv2".into();
        let resources = [a];
        let filtered: Vec<&SqlResource> = resources.iter().collect();
        // Wide enough for NAME but not every column.
        let layout = plan_columns(&filtered, 60, true, &state);
        assert_eq!(
            layout.name_w as usize,
            "a-fairly-long-database-name".len(),
            "NAME gets its full width"
        );
        assert!(
            layout.sub.is_none(),
            "SUB NAME dropped first on a tight budget"
        );
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
