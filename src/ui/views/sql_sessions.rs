//! Open sessions on the pinned SQL target — who is connected *right now*,
//! since when, and how long idle. The live counterpart of the audit roll-up:
//! an open session from a login is an immediate "no" to deleting it, and no
//! audit window can show it.
//!
//! ⚠ This is one of the two features backed by **live T-SQL** (a TDS
//! connection running the fixed read-only [`SESSIONS_SQL`]), not REST. The
//! header carries a permanent warning chip, the loading / error states show
//! the exact statement, and `sql_live_queries = false` in config.toml
//! disables it entirely.

#![allow(dead_code, unused_variables)]

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;

use super::sql_audit::display_principal;
use crate::azure::sql_audit::humanize_ago;
use crate::azure::sql_tds::{DbSession, SESSIONS_SQL};
use crate::ui::events::Action;
use crate::ui::state::{AppState, View};
use crate::ui::theme::Theme;

const FOOTER_HINT: &str = "j/k move  r refresh (runs the query again)  y yank  Esc back  ? help";
const HALF_PAGE: usize = 10;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let sessions = &state.sql.sessions;
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);

    let mut title_spans: Vec<Span> = vec![Span::styled(
        " open sessions ",
        Style::default().fg(theme.fg),
    )];
    if let Some(target) = sessions.target.as_ref() {
        title_spans.push(Span::styled(
            format!("· {} ", target.label()),
            Style::default().fg(theme.accent),
        ));
    }
    if let Some(rows) = sessions.rows.as_ref() {
        title_spans.push(Span::styled(
            format!("· {} sessions ", rows.len()),
            Style::default().fg(theme.muted),
        ));
    }
    // Permanent, non-negotiable: this view speaks live T-SQL to the database.
    title_spans.push(Span::styled(
        "· ⚠ live T-SQL ",
        Style::default()
            .fg(theme.degraded)
            .add_modifier(Modifier::BOLD),
    ));
    if sessions.pending && sessions.rows.is_some() {
        title_spans.push(Span::styled(
            "· refreshing… ",
            Style::default().fg(theme.muted),
        ));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Line::from(title_spans));
    let inner = block.inner(chunks[0]);
    frame.render_widget(block, chunks[0]);

    // Warning line at the top of the window: what is being queried, and how
    // to turn the capability off.
    let (warn_area, body_area) = {
        let parts = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(inner);
        (parts[0], parts[1])
    };
    let target_label = sessions
        .target
        .as_ref()
        .map(|t| t.label())
        .unwrap_or_default();
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(
                "⚠ runs a read-only SELECT over a live SQL connection to {target_label} — disable with sql_live_queries = false"
            ),
            Style::default().fg(theme.degraded),
        ))),
        warn_area,
    );

    if let Some(err) = sessions.error.as_ref() {
        let p = Paragraph::new(Text::styled(
            format!("error: {err}\n\nquery:\n{SESSIONS_SQL}"),
            Style::default().fg(theme.critical),
        ))
        .wrap(Wrap { trim: false });
        frame.render_widget(p, body_area);
        render_footer(frame, chunks[1], theme);
        return;
    }

    match sessions.rows.as_deref() {
        None if sessions.pending => {
            let p = Paragraph::new(Text::styled(
                format!("loading open sessions…\n\nquery:\n{SESSIONS_SQL}"),
                Style::default().fg(theme.muted),
            ))
            .wrap(Wrap { trim: false });
            frame.render_widget(p, body_area);
        }
        None => {
            let p = Paragraph::new(Line::from(Span::styled(
                "press r to query open sessions.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some([]) => {
            let p = Paragraph::new(Text::styled(
                "no open sessions visible.\n\nnote: seeing sessions other than your own needs VIEW DATABASE STATE\n(or server-admin); a plain database user sees only itself.",
                Style::default().fg(theme.muted),
            ));
            frame.render_widget(p, body_area);
        }
        Some(rows) => {
            let now = chrono::Utc::now();
            let logins: Vec<String> = rows
                .iter()
                .map(|s| display_principal(state, &s.login))
                .collect();
            let login_w = logins
                .iter()
                .map(|l| l.chars().count() as u16)
                .max()
                .unwrap_or(0)
                .clamp(5, 40);
            let db_w = rows
                .iter()
                .map(|s| s.database.chars().count() as u16)
                .max()
                .unwrap_or(0)
                .clamp(2, 20);
            // Hosts are the row's most identifying cell (container app /
            // machine names run long) — give them real room before clamping.
            let host_w = rows
                .iter()
                .map(|s| s.host.chars().count() as u16)
                .max()
                .unwrap_or(0)
                .clamp(4, 36);
            let widths = [
                Constraint::Length(login_w), // LOGIN
                Constraint::Length(8),       // STATUS
                Constraint::Length(19),      // SINCE
                Constraint::Length(5),       // AGE
                Constraint::Length(5),       // IDLE
                Constraint::Length(db_w),    // DB
                Constraint::Length(host_w),  // HOST
                Constraint::Length(15),      // IP
                Constraint::Min(7),          // PROGRAM
            ];
            let header_row = Row::new(vec![
                "LOGIN", "STATUS", "SINCE", "AGE", "IDLE", "DB", "HOST", "IP", "PROGRAM",
            ])
            .style(
                Style::default()
                    .fg(theme.muted)
                    .add_modifier(Modifier::BOLD),
            );
            let cursor = sessions.cursor.min(rows.len() - 1);
            let body_rows: Vec<Row> = rows
                .iter()
                .zip(logins)
                .map(|(s, login)| build_row(s, login, now, theme))
                .collect();
            let table = Table::new(body_rows, widths)
                .header(header_row)
                .row_highlight_style(theme.selection())
                .highlight_symbol("▍ ")
                .column_spacing(2);
            let mut ts = TableState::default().with_offset(sessions.view_top.get());
            ts.select(Some(cursor));
            frame.render_stateful_widget(table, body_area, &mut ts);
            sessions.view_top.set(ts.offset());
        }
    }

    render_footer(frame, chunks[1], theme);
}

fn build_row<'a>(
    s: &'a DbSession,
    login: String,
    now: chrono::DateTime<chrono::Utc>,
    theme: &Theme,
) -> Row<'a> {
    let since = s
        .login_time
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| "—".to_string());
    let age = s
        .login_time
        .map(|t| humanize_ago(t, now))
        .unwrap_or_else(|| "—".to_string());
    let idle = s
        .idle_since
        .map(|t| humanize_ago(t, now))
        .unwrap_or_else(|| "—".to_string());
    let status_style = if s.status.eq_ignore_ascii_case("running") {
        Style::default().fg(theme.healthy)
    } else {
        Style::default().fg(theme.muted)
    };
    Row::new(vec![
        Cell::from(login).style(Style::default().fg(theme.accent)),
        Cell::from(s.status.as_str()).style(status_style),
        Cell::from(since).style(Style::default().fg(theme.muted)),
        Cell::from(age).style(Style::default().fg(theme.fg)),
        Cell::from(idle).style(Style::default().fg(theme.muted)),
        Cell::from(s.database.as_str()).style(Style::default().fg(theme.fg)),
        Cell::from(s.host.as_str()).style(Style::default().fg(theme.muted)),
        Cell::from(s.ip.as_str()).style(Style::default().fg(theme.muted)),
        Cell::from(s.program.as_str()).style(Style::default().fg(theme.muted)),
    ])
}

fn render_footer(frame: &mut Frame, area: Rect, theme: &Theme) {
    let p = Paragraph::new(Line::from(Span::styled(
        FOOTER_HINT,
        Style::default().fg(theme.muted),
    )));
    frame.render_widget(p, area);
}

/// Yank the selected session line: raw login (plus resolved name), timings,
/// client details.
pub fn yank_text(state: &AppState) -> Option<String> {
    let sessions = &state.sql.sessions;
    let rows = sessions.rows.as_deref()?;
    let s = rows.get(sessions.cursor.min(rows.len().checked_sub(1)?))?;
    let resolved = display_principal(state, &s.login);
    let name = if resolved == s.login {
        s.login.clone()
    } else {
        format!("{} ({resolved})", s.login)
    };
    Some(format!(
        "session {}  {}  {}  login={}  idle_since={}  db={}  host={}  ip={}  program={}",
        s.id,
        name,
        s.status,
        s.login_time.map(|t| t.to_rfc3339()).unwrap_or_default(),
        s.idle_since.map(|t| t.to_rfc3339()).unwrap_or_default(),
        s.database,
        s.host,
        s.ip,
        s.program,
    ))
}

pub fn handle(action: Action, state: &mut AppState) -> bool {
    let len = state.sql.sessions.rows.as_deref().map_or(0, <[_]>::len);
    let sessions = &mut state.sql.sessions;
    match action {
        Action::MoveDown => {
            if len > 0 {
                sessions.cursor = (sessions.cursor + 1).min(len - 1);
            }
            true
        }
        Action::MoveUp => {
            sessions.cursor = sessions.cursor.saturating_sub(1);
            true
        }
        Action::HalfPageDown => {
            if len > 0 {
                sessions.cursor = (sessions.cursor + HALF_PAGE).min(len - 1);
            }
            true
        }
        Action::HalfPageUp => {
            sessions.cursor = sessions.cursor.saturating_sub(HALF_PAGE);
            true
        }
        Action::GotoTop => {
            sessions.cursor = 0;
            true
        }
        Action::GotoBottom => {
            sessions.cursor = len.saturating_sub(1);
            true
        }
        Action::Back => {
            state.view = sessions.return_view.take().unwrap_or(View::SqlResources);
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::sql_audit::AuditTarget;
    use crate::config::Config;
    use chrono::{Duration, Utc};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn session(login: &str, status: &str, hours_ago: i64, idle_min: i64) -> DbSession {
        DbSession {
            id: 51,
            login: login.to_string(),
            status: status.to_string(),
            login_time: Some(Utc::now() - Duration::hours(hours_ago)),
            idle_since: Some(Utc::now() - Duration::minutes(idle_min)),
            host: "aks-node-04".to_string(),
            program: "orders-api".to_string(),
            ip: "10.0.1.12".to_string(),
            database: "orders".to_string(),
        }
    }

    fn fixture() -> AppState {
        let mut state = AppState::new(Config::default());
        state.view = View::SqlSessions;
        state.sql.sessions.target = Some(AuditTarget {
            master_id: "/subscriptions/s/resourceGroups/rg/providers/Microsoft.Sql/servers/srv/databases/master".to_string(),
            database_id: None,
            server: "srv".to_string(),
            database: Some("orders".to_string()),
        });
        state
    }

    #[test]
    fn renders_sessions_with_warning_banner() {
        let theme = Theme::catppuccin_mocha();
        let mut term = Terminal::new(TestBackend::new(170, 14)).unwrap();
        let mut state = fixture();
        state.sql.sessions.rows = Some(vec![
            session("app-orders", "running", 30, 1),
            session("dana@contoso.com", "sleeping", 2, 65),
        ]);
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("live T-SQL"), "warning chip missing");
        assert!(
            s.contains("sql_live_queries = false"),
            "disable hint missing"
        );
        assert!(s.contains("app-orders"));
        assert!(s.contains("running"));
        assert!(s.contains("LOGIN"), "header missing");
        assert!(s.contains("srv/orders"), "target chip missing");
    }

    #[test]
    fn loading_and_error_states_show_the_tsql() {
        let theme = Theme::catppuccin_mocha();
        let mut state = fixture();
        state.sql.sessions.pending = true;
        let mut term = Terminal::new(TestBackend::new(170, 20)).unwrap();
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("sys.dm_exec_sessions"), "T-SQL preview missing");

        state.sql.sessions.pending = false;
        state.sql.sessions.error = Some("firewall says no".to_string());
        let mut term = Terminal::new(TestBackend::new(170, 20)).unwrap();
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("firewall says no"));
        assert!(s.contains("sys.dm_exec_sessions"));
    }

    #[test]
    fn empty_result_explains_view_state_permission() {
        let theme = Theme::catppuccin_mocha();
        let mut state = fixture();
        state.sql.sessions.rows = Some(Vec::new());
        let mut term = Terminal::new(TestBackend::new(150, 14)).unwrap();
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("VIEW DATABASE STATE"));
    }

    #[test]
    fn esc_returns_to_recorded_origin_and_yank_reads_row() {
        let mut state = fixture();
        state.sql.sessions.rows = Some(vec![session("app-orders", "running", 30, 1)]);
        state.sql.sessions.return_view = Some(View::SqlDetail);
        let y = yank_text(&state).unwrap();
        assert!(y.contains("app-orders"));
        assert!(y.contains("session 51"));
        assert!(handle(Action::Back, &mut state));
        assert_eq!(state.view, View::SqlDetail);
    }
}
