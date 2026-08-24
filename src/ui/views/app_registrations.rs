//! Top-level App registrations entry view: the tenant's Entra ID
//! applications, with credential health and (best-effort) last sign-in so
//! stale registrations stand out. Enter or `l` on a row pins the app into
//! `state.app_reg.selected_app` and opens [`View::AppRegistrationSignIns`].

#![allow(dead_code, unused_variables)]

use chrono::Utc;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;

use super::{name_col_width, truncate_ellipsis};
use crate::azure::app_registrations::AppRegistration;
use crate::azure::sql_audit::humanize_ago;
use crate::ui::events::Action;
use crate::ui::state::{AppState, View};
use crate::ui::theme::Theme;

const FOOTER_HINT: &str =
    "j/k move  Enter/l sign-in log  / filter  Esc back  r refresh  y yank app id  o portal  ? help  q quit";
const HALF_PAGE: usize = 10;

/// Days after which a last sign-in stops counting as "recent" and starts
/// looking stale in the list (degraded → critical past a year).
const STALE_DAYS: i64 = 30;
const DEAD_DAYS: i64 = 365;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);

    let filter_value = state.app_reg.apps_filter.value();
    let filter_active = state.app_reg.apps_filter_active;
    let total = state.app_reg.apps.as_ref().map(|v| v.len());
    let filtered = state.app_reg.filtered_apps();
    let count_label = match total {
        Some(t) if !filter_value.is_empty() => format!("· {} of {} ", filtered.len(), t),
        Some(t) => format!("· {t} "),
        None => String::new(),
    };
    let mut title_spans: Vec<Span> = vec![
        Span::styled(" app registrations ", Style::default().fg(theme.fg)),
        Span::styled(count_label, Style::default().fg(theme.muted)),
    ];
    if filter_active || !filter_value.is_empty() {
        title_spans.push(Span::styled(
            format!("/{filter_value} "),
            Style::default().fg(theme.accent),
        ));
    }
    if state.app_reg.activity_note.is_some() {
        // Blank LAST SIGN-IN must not read as "unused" — flag why it's blank.
        title_spans.push(Span::styled(
            "· last sign-in unavailable ",
            Style::default().fg(theme.degraded),
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

    if let Some(err) = state.app_reg.apps_error.as_deref() {
        let p = Paragraph::new(Text::styled(
            format!("error: {err}"),
            Style::default().fg(theme.critical),
        ))
        .wrap(Wrap { trim: false });
        frame.render_widget(p, body_area);
        render_footer(frame, chunks[1], theme);
        return;
    }

    match state.app_reg.apps.as_deref() {
        None if state.app_reg.apps_pending => {
            let p = Paragraph::new(Line::from(Span::styled(
                "loading app registrations …",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        None => {
            let p = Paragraph::new(Line::from(Span::styled(
                "press r to load app registrations.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some([]) => {
            let p = Paragraph::new(Line::from(Span::styled(
                "No app registrations found in this tenant.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(_) if filtered.is_empty() => {
            let p = Paragraph::new(Line::from(Span::styled(
                "no app registrations match the current filter.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(_) => {
            // NAME gets whatever width is left after the fixed columns; keep
            // `fixed_w` in sync with the Length()s below.
            let fixed_w: u16 = 12 + 7 + 11 + 10 + 10 + 36;
            let n_cols: u16 = 7;
            let longest = filtered
                .iter()
                .map(|a| a.display_name.chars().count() as u16)
                .max()
                .unwrap_or(0);
            let name_w = name_col_width(body_area.width, fixed_w, n_cols, longest);

            let widths: Vec<Constraint> = vec![
                Constraint::Length(name_w), // NAME
                Constraint::Length(12),     // LAST SIGN-IN
                Constraint::Length(7),      // CREDS
                Constraint::Length(11),     // CRED EXPIRY
                Constraint::Length(10),     // AUDIENCE
                Constraint::Length(10),     // CREATED
                Constraint::Length(36),     // APP ID
            ];
            let headers = vec![
                "NAME",
                "LAST SIGN-IN",
                "CREDS",
                "CRED EXPIRY",
                "AUDIENCE",
                "CREATED",
                "APP ID",
            ];

            let header_row = Row::new(headers).style(
                Style::default()
                    .fg(theme.muted)
                    .add_modifier(Modifier::BOLD),
            );

            let cursor = state.app_reg.apps_cursor.min(filtered.len() - 1);
            let activity_unavailable = state.app_reg.activity_note.is_some();
            let body_rows: Vec<Row> = filtered
                .iter()
                .map(|app| build_row(app, activity_unavailable, name_w, theme))
                .collect();

            let table = Table::new(body_rows, widths)
                .header(header_row)
                .row_highlight_style(theme.selection())
                .highlight_symbol("▍ ")
                .column_spacing(2);

            let mut ts = TableState::default().with_offset(state.app_reg.apps_view_top.get());
            ts.select(Some(cursor));
            frame.render_stateful_widget(table, body_area, &mut ts);
            state.app_reg.apps_view_top.set(ts.offset());
        }
    }

    render_footer(frame, chunks[1], theme);
}

fn build_row<'a>(
    app: &'a AppRegistration,
    activity_unavailable: bool,
    name_w: u16,
    theme: &Theme,
) -> Row<'a> {
    let now = Utc::now();

    // The usage column the view exists for: recency-graded coloring so a
    // scan down the list surfaces the dead weight.
    let last_sign_in = match (app.last_sign_in, activity_unavailable) {
        (Some(ts), _) => {
            let days = (now - ts).num_days();
            let style = if days < STALE_DAYS {
                Style::default().fg(theme.healthy)
            } else if days < DEAD_DAYS {
                Style::default().fg(theme.degraded)
            } else {
                Style::default().fg(theme.critical)
            };
            Cell::from(humanize_ago(ts, now)).style(style)
        }
        (None, true) => Cell::from("?").style(Style::default().fg(theme.muted)),
        // Activity data loaded and this app isn't in it: genuinely never
        // (within the report's lookback) — the strongest "dead?" signal.
        (None, false) => Cell::from("never").style(Style::default().fg(theme.critical)),
    };

    let creds = match (app.secret_count, app.cert_count) {
        (0, 0) => Cell::from("—").style(Style::default().fg(theme.muted)),
        (s, 0) => Cell::from(format!("{s}s")).style(Style::default().fg(theme.fg)),
        (0, c) => Cell::from(format!("{c}c")).style(Style::default().fg(theme.fg)),
        (s, c) => Cell::from(format!("{s}s {c}c")).style(Style::default().fg(theme.fg)),
    };

    let expiry = match app.next_cred_expiry {
        None => Cell::from("—").style(Style::default().fg(theme.muted)),
        Some(ts) if ts < now => Cell::from("expired").style(Style::default().fg(theme.critical)),
        Some(ts) => {
            let days = (ts - now).num_days();
            let style = if days <= 30 {
                Style::default().fg(theme.degraded)
            } else {
                Style::default().fg(theme.muted)
            };
            Cell::from(format!("in {}", humanize_ago(now, ts))).style(style)
        }
    };

    let audience = match app.sign_in_audience.as_deref() {
        Some("AzureADMyOrg") => "MyOrg".to_string(),
        Some("AzureADMultipleOrgs") => "MultiOrg".to_string(),
        Some("AzureADandPersonalMicrosoftAccount") => "Any+MSA".to_string(),
        Some("PersonalMicrosoftAccount") => "MSA".to_string(),
        Some(other) => other.to_string(),
        None => "?".to_string(),
    };

    let created = app
        .created
        .map(|c| c.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "—".to_string());

    Row::new(vec![
        Cell::from(truncate_ellipsis(&app.display_name, name_w as usize))
            .style(Style::default().fg(theme.fg)),
        last_sign_in,
        creds,
        expiry,
        Cell::from(audience).style(Style::default().fg(theme.muted)),
        Cell::from(created).style(Style::default().fg(theme.muted)),
        Cell::from(app.app_id.as_str()).style(Style::default().fg(theme.muted)),
    ])
}

fn render_footer(frame: &mut Frame, area: Rect, theme: &Theme) {
    let p = Paragraph::new(Line::from(Span::styled(
        FOOTER_HINT,
        Style::default().fg(theme.muted),
    )));
    frame.render_widget(p, area);
}

/// Pin the selected app and open its sign-in log. Enter and `l` both land
/// here — an app registration has no deeper "contents" to drill into, its
/// usage *is* the drill-in.
fn open_sign_ins(state: &mut AppState) {
    let app = state
        .app_reg
        .filtered_apps()
        .get(state.app_reg.apps_cursor)
        .copied()
        .cloned();
    if let Some(app) = app {
        state.app_reg.selected_app = Some(app);
        state.app_reg.enter_sign_ins_view(View::AppRegistrations);
        state.view = View::AppRegistrationSignIns;
    }
}

pub fn handle(action: Action, state: &mut AppState) -> bool {
    let len = state.app_reg.filtered_apps().len();

    if state.app_reg.apps_filter_active {
        match action {
            Action::Back => {
                state.app_reg.apps_filter_active = false;
                state.app_reg.apps_filter.reset();
                state.app_reg.apps_cursor = 0;
                return true;
            }
            Action::OpenSelected => {
                state.app_reg.apps_filter_active = false;
                return true;
            }
            Action::MoveDown => {
                state.app_reg.apps_filter_active = false;
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
                state.app_reg.apps_cursor = (state.app_reg.apps_cursor + 1).min(len - 1);
            }
            true
        }
        Action::MoveUp => {
            state.app_reg.apps_cursor = state.app_reg.apps_cursor.saturating_sub(1);
            true
        }
        Action::HalfPageDown => {
            if len > 0 {
                state.app_reg.apps_cursor = (state.app_reg.apps_cursor + HALF_PAGE).min(len - 1);
            }
            true
        }
        Action::HalfPageUp => {
            state.app_reg.apps_cursor = state.app_reg.apps_cursor.saturating_sub(HALF_PAGE);
            true
        }
        Action::GotoTop => {
            state.app_reg.apps_cursor = 0;
            true
        }
        Action::GotoBottom => {
            if len > 0 {
                state.app_reg.apps_cursor = len - 1;
            }
            true
        }
        Action::StartSearch => {
            state.app_reg.apps_filter.reset();
            state.app_reg.apps_cursor = 0;
            state.app_reg.apps_filter_active = true;
            true
        }
        Action::OpenSelected | Action::OpenLogs => {
            open_sign_ins(state);
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use chrono::Duration;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn fixture() -> AppState {
        let mut state = AppState::new(Config::default());
        state.view = View::AppRegistrations;
        state
    }

    fn app(name: &str, app_id: &str, last_sign_in_days: Option<i64>) -> AppRegistration {
        AppRegistration {
            object_id: format!("obj-{app_id}"),
            app_id: app_id.into(),
            display_name: name.into(),
            created: Some(Utc::now() - Duration::days(400)),
            sign_in_audience: Some("AzureADMyOrg".into()),
            secret_count: 1,
            cert_count: 0,
            next_cred_expiry: Some(Utc::now() + Duration::days(90)),
            expired_creds: 0,
            last_sign_in: last_sign_in_days.map(|d| Utc::now() - Duration::days(d)),
        }
    }

    #[test]
    fn renders_loading_state() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(140, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.app_reg.apps_pending = true;
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("loading app registrations"));
    }

    #[test]
    fn renders_rows_with_usage_signal() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(180, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.app_reg.apps = Some(vec![
            app("Contoso Orders API", "aa11-bb22", Some(0)),
            app("contoso-payroll-sync", "cc33-dd44", None),
        ]);
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("Contoso Orders API"));
        assert!(buf.contains("LAST SIGN-IN"), "usage column must render");
        assert!(
            buf.contains("never"),
            "an app absent from the activity report shows `never`"
        );
        assert!(buf.contains("aa11-bb22"), "app id column must render");
    }

    #[test]
    fn activity_note_renders_question_marks_not_never() {
        // When the report call failed, `never` would be a lie — the column
        // degrades to `?` and the title carries a chip.
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(180, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.app_reg.apps = Some(vec![app("Contoso Orders API", "aa11-bb22", None)]);
        state.app_reg.activity_note = Some("no license".into());
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(!buf.contains("never"));
        assert!(buf.contains("last sign-in unavailable"));
    }

    #[test]
    fn enter_and_l_both_open_sign_in_log() {
        for action in [Action::OpenSelected, Action::OpenLogs] {
            let mut state = fixture();
            state.app_reg.apps = Some(vec![app("Contoso Orders API", "aa11-bb22", Some(1))]);
            assert!(handle(action, &mut state));
            assert_eq!(state.view, View::AppRegistrationSignIns);
            assert_eq!(
                state
                    .app_reg
                    .selected_app
                    .as_ref()
                    .map(|a| a.app_id.as_str()),
                Some("aa11-bb22")
            );
            assert_eq!(
                state.app_reg.sign_ins_return_view,
                Some(View::AppRegistrations)
            );
        }
    }

    #[test]
    fn filter_matches_name_and_app_id() {
        let mut state = fixture();
        state.app_reg.apps = Some(vec![
            app("Contoso Orders API", "aa11-bb22", Some(1)),
            app("Contoso Web Portal", "cc33-dd44", Some(1)),
        ]);
        state.app_reg.apps_filter = tui_input::Input::default().with_value("orders".into());
        assert_eq!(state.app_reg.filtered_apps().len(), 1);
        state.app_reg.apps_filter = tui_input::Input::default().with_value("CC33".into());
        let hits = state.app_reg.filtered_apps();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].display_name, "Contoso Web Portal");
    }
}
