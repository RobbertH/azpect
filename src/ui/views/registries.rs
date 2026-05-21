//! Top-level Container Registries mode entry view: lists ACR registries
//! visible to the current subscription scope. Pressing Enter on a row pins the
//! registry into `state.registry.selected_registry` and opens
//! [`View::RegistryRepositories`].

#![allow(dead_code, unused_variables)]

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};
use ratatui::Frame;

use crate::azure::registries::Registry;
use crate::ui::events::Action;
use crate::ui::state::{subscription_display_name, AppState, View};
use crate::ui::theme::Theme;

const FOOTER_HINT: &str =
    "j/k move  Enter repos  / filter  Esc back  r refresh  y yank id  ? help  q quit";
const HALF_PAGE: usize = 10;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);

    let filter_value = state.registry.registries_filter.value();
    let filter_active = state.registry.registries_filter_active;
    let total = state.registry.registries.as_ref().map(|v| v.len());
    let filtered = state.registry.filtered_registries();
    let count_label = match total {
        Some(t) if !filter_value.is_empty() => format!("· {} of {} ", filtered.len(), t),
        Some(t) => format!("· {t} "),
        None => String::new(),
    };
    let mut title_spans: Vec<Span> = vec![
        Span::styled(" container registries ", Style::default().fg(theme.fg)),
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

    if let Some(err) = state.registry.registries_error.as_deref() {
        let p = Paragraph::new(Line::from(Span::styled(
            format!("error: {err}"),
            Style::default().fg(theme.critical),
        )));
        frame.render_widget(p, body_area);
        render_footer(frame, chunks[1], theme);
        return;
    }

    match state.registry.registries.as_deref() {
        None if state.registry.registries_pending => {
            let p = Paragraph::new(Line::from(Span::styled(
                "loading container registries …",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        None => {
            let p = Paragraph::new(Line::from(Span::styled(
                "press r to load container registries.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some([]) => {
            let p = Paragraph::new(Line::from(Span::styled(
                "No container registries found in selected subscriptions.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(_) if filtered.is_empty() => {
            let p = Paragraph::new(Line::from(Span::styled(
                "no container registries match the current filter.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(_) => {
            // SUB columns only appear when looking at >1 subscription.
            let show_sub_cols = state.selected_subscription.is_none();
            let show_anon = filtered
                .iter()
                .any(|r| r.anonymous_pull_enabled == Some(true));

            let name_w = filtered
                .iter()
                .map(|r| r.name.chars().count() as u16)
                .max()
                .unwrap_or(0)
                .max(4);

            let mut widths: Vec<Constraint> = vec![
                Constraint::Length(name_w), // NAME
                Constraint::Length(10),     // SKU
                Constraint::Length(7),      // ADMIN
                Constraint::Length(10),     // PUBLIC NET
            ];
            let mut headers: Vec<&'static str> = vec!["NAME", "SKU", "ADMIN", "PUBLIC NET"];
            if show_anon {
                widths.push(Constraint::Length(7));
                headers.push("ANON");
            }
            // LOGIN SERVER is the bit users want to copy/paste, so give it
            // generous room; it absorbs leftover width.
            widths.push(Constraint::Min(20));
            headers.push("LOGIN SERVER");
            widths.push(Constraint::Length(22)); // RG
            headers.push("RESOURCE GROUP");
            widths.push(Constraint::Length(10)); // CREATED
            headers.push("CREATED");
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

            let cursor = state.registry.registries_cursor.min(filtered.len() - 1);
            let body_rows: Vec<Row> = filtered
                .iter()
                .map(|registry| build_row(registry, state, show_sub_cols, show_anon, theme))
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
    registry: &'a Registry,
    state: &'a AppState,
    show_sub_cols: bool,
    show_anon: bool,
    theme: &Theme,
) -> Row<'a> {
    let admin = match registry.admin_user_enabled {
        // Admin user enabled means the registry accepts a static
        // username/password — surface it as a security signal.
        Some(true) => Cell::from("on").style(Style::default().fg(theme.critical)),
        Some(false) => Cell::from("off").style(Style::default().fg(theme.muted)),
        None => Cell::from("?").style(Style::default().fg(theme.muted)),
    };
    let public = match registry.public_network_access.as_deref() {
        // Disabled = private endpoint only → quietest signal.
        Some("Disabled") => Cell::from("Disabled").style(Style::default().fg(theme.muted)),
        Some(s) => Cell::from(s.to_string()).style(Style::default().fg(theme.fg)),
        None => Cell::from("?").style(Style::default().fg(theme.muted)),
    };
    let mut cells: Vec<Cell> = vec![
        Cell::from(registry.name.as_str()).style(Style::default().fg(theme.fg)),
        Cell::from(registry.sku.as_deref().unwrap_or("—").to_string())
            .style(Style::default().fg(theme.muted)),
        admin,
        public,
    ];
    if show_anon {
        let anon = match registry.anonymous_pull_enabled {
            // Anonymous pull == anyone on the internet can `docker pull`.
            // Flag it loud.
            Some(true) => Cell::from("on").style(Style::default().fg(theme.critical)),
            Some(false) => Cell::from("off").style(Style::default().fg(theme.muted)),
            None => Cell::from("—").style(Style::default().fg(theme.muted)),
        };
        cells.push(anon);
    }
    cells.push(
        Cell::from(registry.login_server_or_default()).style(Style::default().fg(theme.muted)),
    );
    cells
        .push(Cell::from(registry.resource_group.as_str()).style(Style::default().fg(theme.muted)));
    cells.push(
        Cell::from(format_date(registry.created_at.as_ref()))
            .style(Style::default().fg(theme.muted)),
    );
    if show_sub_cols {
        cells.push(
            Cell::from(
                subscription_display_name(state, &registry.subscription_id)
                    .unwrap_or("")
                    .to_string(),
            )
            .style(Style::default().fg(theme.muted)),
        );
    }
    cells.push(Cell::from(registry.location.as_str()).style(Style::default().fg(theme.muted)));
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
    let len = state.registry.filtered_registries().len();

    if state.registry.registries_filter_active {
        match action {
            Action::Back => {
                state.registry.registries_filter_active = false;
                state.registry.registries_filter.reset();
                state.registry.registries_cursor = 0;
                return true;
            }
            Action::OpenSelected => {
                state.registry.registries_filter_active = false;
                return true;
            }
            Action::MoveDown => {
                state.registry.registries_filter_active = false;
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
                state.registry.registries_cursor =
                    (state.registry.registries_cursor + 1).min(len - 1);
            }
            true
        }
        Action::MoveUp => {
            state.registry.registries_cursor = state.registry.registries_cursor.saturating_sub(1);
            true
        }
        Action::HalfPageDown => {
            if len > 0 {
                state.registry.registries_cursor =
                    (state.registry.registries_cursor + HALF_PAGE).min(len - 1);
            }
            true
        }
        Action::HalfPageUp => {
            state.registry.registries_cursor =
                state.registry.registries_cursor.saturating_sub(HALF_PAGE);
            true
        }
        Action::GotoTop => {
            state.registry.registries_cursor = 0;
            true
        }
        Action::GotoBottom => {
            if len > 0 {
                state.registry.registries_cursor = len - 1;
            }
            true
        }
        Action::StartSearch => {
            state.registry.registries_filter_active = true;
            true
        }
        Action::OpenSelected => {
            let registry = state
                .registry
                .filtered_registries()
                .get(state.registry.registries_cursor)
                .copied()
                .cloned();
            if let Some(registry) = registry {
                state.registry.selected_registry = Some(registry);
                state.registry.repositories_cursor = 0;
                state.registry.repositories_filter = tui_input::Input::default();
                state.view_stack.push(state.view);
                state.view = View::RegistryRepositories;
            }
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::registries::Registry;
    use crate::config::Config;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn fixture() -> AppState {
        let mut state = AppState::new(Config::default());
        state.view = View::Registries;
        state
    }

    fn registry(name: &str) -> Registry {
        Registry {
            id: format!(
                "/subscriptions/sub/resourceGroups/rg/providers/Microsoft.ContainerRegistry/registries/{name}",
            ),
            name: name.into(),
            resource_group: "rg".into(),
            subscription_id: "sub".into(),
            location: "westeurope".into(),
            sku: Some("Premium".into()),
            login_server: Some(format!("{name}.azurecr.io")),
            admin_user_enabled: Some(false),
            public_network_access: Some("Enabled".into()),
            anonymous_pull_enabled: Some(false),
            created_at: None,
        }
    }

    #[test]
    fn renders_loading_state() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(120, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.registry.registries_pending = true;
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("loading container registries"));
    }

    #[test]
    fn renders_registry_row() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(180, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.registry.registries = Some(vec![registry("myreg")]);
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("myreg"), "name should render");
        assert!(buf.contains("Premium"), "sku should render");
        assert!(
            buf.contains("myreg.azurecr.io"),
            "login server should render"
        );
    }

    #[test]
    fn enter_pins_registry_and_drills_in() {
        let mut state = fixture();
        state.registry.registries = Some(vec![registry("myreg")]);
        assert!(handle(Action::OpenSelected, &mut state));
        assert_eq!(state.view, View::RegistryRepositories);
        assert_eq!(
            state
                .registry
                .selected_registry
                .as_ref()
                .map(|r| r.name.as_str()),
            Some("myreg")
        );
    }

    #[test]
    fn filter_substring_match_is_case_insensitive() {
        let mut state = fixture();
        state.registry.registries = Some(vec![
            registry("prod-acr"),
            registry("Dev-Acr"),
            registry("other"),
        ]);
        state.registry.registries_filter = tui_input::Input::default().with_value("ACR".into());
        let names: Vec<&str> = state
            .registry
            .filtered_registries()
            .iter()
            .map(|r| r.name.as_str())
            .collect();
        assert_eq!(names, vec!["prod-acr", "Dev-Acr"]);
    }

    #[test]
    fn anon_column_hidden_when_no_outlier() {
        // When no registry has anonymous pull on, the ANON header should not
        // appear so the table doesn't waste a column on a noisy default.
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(200, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.registry.registries = Some(vec![registry("a"), registry("b")]);
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(!buf.contains("ANON"), "ANON header should be hidden");
    }

    #[test]
    fn anon_column_visible_when_outlier_present() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(220, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        let mut risky = registry("public");
        risky.anonymous_pull_enabled = Some(true);
        state.registry.registries = Some(vec![registry("safe"), risky]);
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("ANON"), "ANON header should be visible");
    }
}
