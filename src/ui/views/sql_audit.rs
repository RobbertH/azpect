//! SQL audit-log drill-in, two levels:
//!
//! - **Principals** ([`View::SqlAuditPrincipals`]): one row per
//!   `server_principal_name` with last-seen / event counts — the "does
//!   anything still use this login, can I delete it?" screen. Opened with `l`
//!   on a pool / database row (database rows scope to that database;
//!   pools audit server-wide).
//! - **Events** ([`View::SqlAuditEvents`]): the newest audit rows for one
//!   principal, statement text included. Opened with Enter on a principal.
//!
//! `0` / `1` / `7` / `t` pick the query window (default 30d — deletion
//! questions need lookback); changing it drops both buffers and refetches,
//! guarded by `SqlAuditState::generation` like the KV access view.

#![allow(dead_code, unused_variables)]

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;

use crate::azure::key_vault_logs::AccessWindow;
use crate::azure::sql_audit::{
    action_label, build_events_kql, build_principals_kql, graph_candidate, humanize_ago,
    AuditEvent, PrincipalSummary, PRINCIPALS_PAGE_SIZE,
};
use crate::azure::sql_tds::{DbUser, DB_USERS_SQL};
use crate::ui::events::Action;
use crate::ui::state::{AppState, View};
use crate::ui::theme::Theme;

const PRINCIPALS_FOOTER: &str =
    "j/k move  Enter events  u sessions ⚠  / filter  0/1/7/3/9 window (1h…1y)  t custom  r refresh  y yank  Esc back  ? help";
const EVENTS_FOOTER: &str =
    "j/k move (bottom fetches older)  Enter detail  Tab action  e errors  0/1/7/3/9 window  t custom  r refresh  y yank  Esc back  ? help";
const HALF_PAGE: usize = 10;

/// Display form of a raw principal: the Graph-resolved display name when the
/// raw value is a GUID / `clientId@tenantId` / `S-1-9-3-…` SID we've resolved,
/// the raw value otherwise. Resolution is async and best-effort — until (and
/// unless) it lands, the raw identity is what's on screen.
pub(crate) fn display_principal(state: &AppState, raw: &str) -> String {
    graph_candidate(raw)
        .and_then(|id| state.principals.by_id.get(&id).cloned())
        .unwrap_or_else(|| raw.to_string())
}

/// Database users (⚠ fetched via live T-SQL) with **no audit rows** in the
/// window — appended to the roll-up as muted rows so a fully-dead user is
/// visible instead of invisible. Matched against audit principals by name
/// (case-insensitive); the `/` filter applies here too.
fn silent_users(state: &AppState) -> Vec<&DbUser> {
    let Some(users) = state.sql.audit.db_users.as_deref() else {
        return Vec::new();
    };
    // The audited set carries each principal under BOTH identities: the raw
    // audit form (GUID / `clientId@tenantId` / SID) *and* its Graph-resolved
    // display name — `sys.database_principals` names Entra users by display
    // name, so an active app would otherwise reappear as "never seen".
    // (Until resolution lands the two coincide; the set self-heals on the
    // next render once the Graph result arrives.)
    let audited: std::collections::HashSet<String> = state
        .sql
        .audit
        .principals
        .iter()
        .flatten()
        .flat_map(|p| {
            [
                p.principal.to_lowercase(),
                display_principal(state, &p.principal).to_lowercase(),
            ]
        })
        .collect();
    let needle = state.sql.audit.principals_filter.value().to_lowercase();
    users
        .iter()
        .filter(|u| !audited.contains(&u.name.to_lowercase()))
        .filter(|u| needle.is_empty() || u.name.to_lowercase().contains(&needle))
        .collect()
}

/// Roll-up rows surviving the `/` filter: a case-insensitive substring match
/// against the raw principal *and* its resolved display name — the user
/// filters what's on screen, whichever form that is. The cursor indexes this
/// list (followed by [`silent_users`]), same as the table render.
fn filtered_principals(state: &AppState) -> Vec<&PrincipalSummary> {
    let needle = state.sql.audit.principals_filter.value().to_lowercase();
    match state.sql.audit.principals.as_deref() {
        Some(rows) => rows
            .iter()
            .filter(|r| {
                needle.is_empty()
                    || r.principal.to_lowercase().contains(&needle)
                    || display_principal(state, &r.principal)
                        .to_lowercase()
                        .contains(&needle)
            })
            .collect(),
        None => Vec::new(),
    }
}

/// The KQL the current query scope produces — shown verbatim in the loading
/// and error states so what's being asked of Log Analytics is never a mystery.
fn kql_preview(state: &AppState) -> Option<String> {
    let audit = &state.sql.audit;
    let target = audit.target.as_ref()?;
    let db = target.database.as_deref();
    match state.view {
        View::SqlAuditEvents => {
            let principal = audit.selected_principal.as_deref()?;
            Some(build_events_kql(
                db,
                principal,
                audit.events_errors_only,
                None,
            ))
        }
        _ => {
            let mut q = build_principals_kql(db);
            // The roll-up may also fire the (⚠) database-users T-SQL — show
            // it right next to the KQL so nothing runs unannounced.
            if db.is_some() && state.config.sql_live_queries {
                q.push_str(&format!(
                    "\n-- plus, via live T-SQL (database users):\n{DB_USERS_SQL}\n"
                ));
            }
            Some(q)
        }
    }
}

/// `{lead}` plus the query being (or last) run, for the loading/error bodies.
fn with_kql_preview(state: &AppState, lead: &str) -> String {
    match kql_preview(state) {
        Some(kql) => format!("{lead}\n\nquery:\n{kql}"),
        None => lead.to_string(),
    }
}

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    match state.view {
        View::SqlAuditEvents => render_events(frame, area, state, theme),
        View::SqlAuditEventDetail => render_event_detail(frame, area, state, theme),
        _ => render_principals(frame, area, state, theme),
    }
}

/// The event under the events-view cursor — what Enter drills into and what
/// the detail view shows. Indexes the *visible* (action-filtered) list, same
/// as the table render.
fn selected_event(state: &AppState) -> Option<&AuditEvent> {
    let audit = &state.sql.audit;
    let visible = audit.visible_events();
    visible
        .get(audit.events_cursor.min(visible.len().checked_sub(1)?))
        .copied()
}

fn render_principals(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let audit = &state.sql.audit;
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);

    let mut title_spans: Vec<Span> = vec![Span::styled(
        " audit: principals ",
        Style::default().fg(theme.fg),
    )];
    if let Some(target) = audit.target.as_ref() {
        title_spans.push(Span::styled(
            format!("· {} ", target.label()),
            Style::default().fg(theme.accent),
        ));
    }
    let filtered = filtered_principals(state);
    let silent = silent_users(state);
    if let Some(total) = audit.principals.as_ref().map(Vec::len) {
        let count = if filtered.len() != total {
            format!("· {} of {} principals ", filtered.len(), total)
        } else {
            format!("· {total} principals ")
        };
        title_spans.push(Span::styled(count, Style::default().fg(theme.muted)));
    }
    if !silent.is_empty() {
        title_spans.push(Span::styled(
            format!("· {} never seen ", silent.len()),
            Style::default().fg(theme.degraded),
        ));
    }
    let filter_value = audit.principals_filter.value();
    if audit.principals_filter_active || !filter_value.is_empty() {
        title_spans.push(Span::styled(
            format!("/{filter_value} "),
            Style::default().fg(theme.accent),
        ));
    }
    title_spans.push(Span::styled(
        format!("· {} ", audit.window.label()),
        Style::default().fg(theme.fg),
    ));
    if audit.principals_truncated {
        // The roll-up keeps the N *most recently active* principals — past the
        // cap it's exactly the stalest (deletion-candidate) ones that fall
        // off, so be loud about which end got cut.
        title_spans.push(Span::styled(
            format!("· capped at {PRINCIPALS_PAGE_SIZE} most recent — stalest cut off "),
            Style::default().fg(theme.degraded),
        ));
    }
    if audit.pending && audit.principals.is_some() {
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

    let body_area = render_window_input(frame, inner, state, theme);
    let body_area = render_filter_input(frame, body_area, state, theme);
    let body_area = render_users_note(frame, body_area, state, theme);

    if let Some(err) = audit.error.as_ref() {
        let p = Paragraph::new(Text::styled(
            with_kql_preview(state, &format!("error: {err}")),
            Style::default().fg(theme.critical),
        ))
        .wrap(Wrap { trim: false });
        frame.render_widget(p, body_area);
        render_footer(frame, chunks[1], PRINCIPALS_FOOTER, theme);
        return;
    }

    match audit.principals.as_deref() {
        None if audit.pending => {
            let p = Paragraph::new(Text::styled(
                with_kql_preview(state, "loading audit roll-up…"),
                Style::default().fg(theme.muted),
            ))
            .wrap(Wrap { trim: false });
            frame.render_widget(p, body_area);
        }
        None => {
            let p = Paragraph::new(Line::from(Span::styled(
                "press r to load the audit roll-up.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(rows) if rows.is_empty() && silent.is_empty() && audit.db_users.is_none() => {
            // Zero rows is ambiguous: nothing happened, or nothing is being
            // recorded. A server without auditing→Log Analytics sits empty
            // forever — say so.
            let p = Paragraph::new(Text::styled(
                "no audit events in this window.\n\nif this is unexpected, check the server's Auditing settings — audit logs must be\nsent to a Log Analytics workspace for rows to appear here (server-level auditing\nlands under the server's master database).",
                Style::default().fg(theme.muted),
            ));
            frame.render_widget(p, body_area);
        }
        Some(_) if filtered.is_empty() && silent.is_empty() => {
            let p = Paragraph::new(Line::from(Span::styled(
                "no principals match the current filter.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(_) => {
            let rows = &filtered;
            let now = chrono::Utc::now();
            // GUID-ish principals render under their Graph-resolved display
            // name once resolution lands (the raw identity stays in yank).
            let names: Vec<String> = rows
                .iter()
                .map(|r| display_principal(state, &r.principal))
                .collect();
            let principal_w = names
                .iter()
                .map(|n| n.chars().count() as u16)
                .chain(silent.iter().map(|u| u.name.chars().count() as u16))
                .max()
                .unwrap_or(0)
                .clamp(9, 40);
            let dbs_w = rows
                .iter()
                .map(|r| r.databases.join(",").chars().count() as u16)
                .max()
                .unwrap_or(0)
                .clamp(9, 30);
            let widths = [
                Constraint::Length(principal_w), // PRINCIPAL
                Constraint::Length(19),          // LAST SEEN
                Constraint::Length(5),           // AGO
                Constraint::Length(7),           // QUERIES
                Constraint::Length(6),           // LOGINS
                Constraint::Length(6),           // FAILED
                Constraint::Length(3),           // IPS
                Constraint::Length(dbs_w),       // DATABASES
                Constraint::Min(8),              // APPS
            ];
            let header_row = Row::new(vec![
                "PRINCIPAL",
                "LAST SEEN",
                "AGO",
                "QUERIES",
                "LOGINS",
                "FAILED",
                "IPS",
                "DATABASES",
                "APPS",
            ])
            .style(
                Style::default()
                    .fg(theme.muted)
                    .add_modifier(Modifier::BOLD),
            );
            let cursor = audit.cursor.min(rows.len() + silent.len() - 1);
            let mut body_rows: Vec<Row> = rows
                .iter()
                .zip(names)
                .map(|(r, name)| build_principal_row(r, name, now, theme))
                .collect();
            // Users that exist but never appear in the window's audit rows —
            // muted, at the bottom, judged explicitly against the window.
            let window_label = audit.window.label();
            body_rows.extend(
                silent
                    .iter()
                    .map(|u| build_silent_row(u, &window_label, theme)),
            );
            let table = Table::new(body_rows, widths)
                .header(header_row)
                .row_highlight_style(theme.selection())
                .highlight_symbol("▍ ")
                .column_spacing(2);
            let mut ts = TableState::default().with_offset(audit.view_top.get());
            ts.select(Some(cursor));
            frame.render_stateful_widget(table, body_area, &mut ts);
            audit.view_top.set(ts.offset());
        }
    }

    render_footer(frame, chunks[1], PRINCIPALS_FOOTER, theme);
}

fn build_principal_row<'a>(
    r: &'a PrincipalSummary,
    display_name: String,
    now: chrono::DateTime<chrono::Utc>,
    theme: &Theme,
) -> Row<'a> {
    let when = r
        .last_seen
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    let ago = humanize_ago(r.last_seen, now);
    // The deletion question is about staleness — a principal idle for 30+
    // days gets its AGO flagged so candidates jump out of the list.
    let stale = (now - r.last_seen).num_days() >= 30;
    let ago_style = if stale {
        Style::default().fg(theme.degraded)
    } else {
        Style::default().fg(theme.muted)
    };
    let failed_style = if r.failed > 0 {
        Style::default().fg(theme.critical)
    } else {
        Style::default().fg(theme.muted)
    };
    Row::new(vec![
        Cell::from(display_name).style(Style::default().fg(theme.accent)),
        Cell::from(when).style(Style::default().fg(theme.muted)),
        Cell::from(ago).style(ago_style),
        Cell::from(r.queries.to_string()).style(Style::default().fg(theme.fg)),
        Cell::from(r.logins.to_string()).style(Style::default().fg(theme.muted)),
        Cell::from(r.failed.to_string()).style(failed_style),
        Cell::from(r.distinct_ips.to_string()).style(Style::default().fg(theme.muted)),
        Cell::from(r.databases.join(",")).style(Style::default().fg(theme.fg)),
        Cell::from(r.apps.join(",")).style(Style::default().fg(theme.muted)),
    ])
}

/// A database user with **no audit rows in the window** — exists (per
/// `sys.database_principals`) but the audit trail is silent about it. The
/// AGO cell names the window so "never" is never read as "never ever".
fn build_silent_row<'a>(u: &'a DbUser, window_label: &str, theme: &Theme) -> Row<'a> {
    let muted = Style::default().fg(theme.muted);
    Row::new(vec![
        Cell::from(u.name.as_str()).style(muted),
        Cell::from("no audit rows").style(muted),
        Cell::from(format!("∅ {window_label}")).style(Style::default().fg(theme.degraded)),
        Cell::from("—").style(muted),
        Cell::from("—").style(muted),
        Cell::from("—").style(muted),
        Cell::from("—").style(muted),
        Cell::from("—").style(muted),
        Cell::from(u.kind_tag()).style(muted),
    ])
}

fn render_events(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let audit = &state.sql.audit;
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);

    let mut title_spans: Vec<Span> = vec![Span::styled(
        " audit: events ",
        Style::default().fg(theme.fg),
    )];
    if let Some(principal) = audit.selected_principal.as_deref() {
        title_spans.push(Span::styled(
            format!("· {} ", display_principal(state, principal)),
            Style::default().fg(theme.accent),
        ));
    }
    if let Some(total) = audit.events.as_ref().map(Vec::len) {
        let shown = audit.visible_events().len();
        let count = if shown != total {
            format!("· {shown} of {total} rows ")
        } else {
            format!("· {total} rows ")
        };
        title_spans.push(Span::styled(count, Style::default().fg(theme.muted)));
    }
    title_spans.push(Span::styled(
        format!("· {} ", audit.window.label()),
        Style::default().fg(theme.fg),
    ));
    if audit.events_errors_only {
        title_spans.push(Span::styled(
            "· errors only ✓ ",
            Style::default().fg(theme.accent),
        ));
    }
    if let Some(a) = audit.events_action_filter.as_deref() {
        title_spans.push(Span::styled(
            format!("· action: {} ", action_label(a)),
            Style::default().fg(theme.accent),
        ));
    }
    if audit.events_truncated {
        title_spans.push(Span::styled(
            "· scroll past bottom for older rows ",
            Style::default().fg(theme.degraded),
        ));
    }
    if audit.events_loading_more {
        title_spans.push(Span::styled(
            "· loading older… ",
            Style::default().fg(theme.muted),
        ));
    }
    if audit.events_pending && audit.events.is_some() {
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

    let body_area = render_window_input(frame, inner, state, theme);

    if let Some(err) = audit.events_error.as_ref() {
        let p = Paragraph::new(Text::styled(
            with_kql_preview(state, &format!("error: {err}")),
            Style::default().fg(theme.critical),
        ))
        .wrap(Wrap { trim: false });
        frame.render_widget(p, body_area);
        render_footer(frame, chunks[1], EVENTS_FOOTER, theme);
        return;
    }

    let visible = audit.visible_events();
    match audit.events.as_deref() {
        None if audit.events_pending => {
            let p = Paragraph::new(Text::styled(
                with_kql_preview(state, "loading audit events…"),
                Style::default().fg(theme.muted),
            ))
            .wrap(Wrap { trim: false });
            frame.render_widget(p, body_area);
        }
        None => {
            let p = Paragraph::new(Line::from(Span::styled(
                "press r to load audit events.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some([]) => {
            let p = Paragraph::new(Line::from(Span::styled(
                "no audit events for this principal in this window.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(_) if visible.is_empty() => {
            let p = Paragraph::new(Line::from(Span::styled(
                "no rows match the current action filter.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, body_area);
        }
        Some(_) => {
            let rows = &visible;
            let action_w = rows
                .iter()
                .map(|e| e.action_label().chars().count() as u16)
                .max()
                .unwrap_or(0)
                .clamp(6, 12);
            let db_w = rows
                .iter()
                .map(|e| e.database.chars().count() as u16)
                .max()
                .unwrap_or(0)
                .clamp(2, 20);
            let app_w = rows
                .iter()
                .map(|e| e.app.chars().count() as u16)
                .max()
                .unwrap_or(0)
                .clamp(3, 24);
            let widths = [
                Constraint::Length(19),       // WHEN
                Constraint::Length(action_w), // ACTION
                Constraint::Length(db_w),     // DB
                Constraint::Length(app_w),    // APP
                Constraint::Length(15),       // IP
                Constraint::Length(6),        // ROWS
                Constraint::Min(12),          // STATEMENT
            ];
            let header_row = Row::new(vec![
                "WHEN",
                "ACTION",
                "DB",
                "APP",
                "IP",
                "ROWS",
                "STATEMENT",
            ])
            .style(
                Style::default()
                    .fg(theme.muted)
                    .add_modifier(Modifier::BOLD),
            );
            let cursor = audit.events_cursor.min(rows.len() - 1);
            let body_rows: Vec<Row> = rows.iter().map(|e| build_event_row(e, theme)).collect();
            let table = Table::new(body_rows, widths)
                .header(header_row)
                .row_highlight_style(theme.selection())
                .highlight_symbol("▍ ")
                .column_spacing(2);
            let mut ts = TableState::default().with_offset(audit.events_view_top.get());
            ts.select(Some(cursor));
            frame.render_stateful_widget(table, body_area, &mut ts);
            audit.events_view_top.set(ts.offset());
        }
    }

    render_footer(frame, chunks[1], EVENTS_FOOTER, theme);
}

const DETAIL_FOOTER: &str = "j/k scroll  y yank statement  Esc back  ? help";

/// Full-screen single-event detail: every field, the complete statement with
/// its original line breaks (wrapped, scrollable), and
/// `additional_information` — the column that names the actual error on
/// failed events.
fn render_event_detail(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let audit = &state.sql.audit;
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Line::from(Span::styled(
            " audit event ",
            Style::default().fg(theme.fg),
        )));
    let inner = block.inner(chunks[0]);
    frame.render_widget(block, chunks[0]);

    let Some(e) = selected_event(state) else {
        let p = Paragraph::new(Line::from(Span::styled(
            "no event selected.",
            Style::default().fg(theme.muted),
        )));
        frame.render_widget(p, inner);
        render_footer(frame, chunks[1], DETAIL_FOOTER, theme);
        return;
    };

    let label = |s: &str| Span::styled(format!("{s:<12}"), Style::default().fg(theme.muted));
    let value = |s: String| Span::styled(s, Style::default().fg(theme.fg));
    let mut lines: Vec<Line> = vec![
        Line::from(vec![
            label("when:"),
            value(
                e.ts.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string(),
            ),
        ]),
        Line::from(vec![
            label("principal:"),
            value(
                audit
                    .selected_principal
                    .as_deref()
                    .map(|p| {
                        let resolved = display_principal(state, p);
                        if resolved == p {
                            p.to_string()
                        } else {
                            format!("{p} ({resolved})")
                        }
                    })
                    .unwrap_or_default(),
            ),
        ]),
        Line::from(vec![
            label("action:"),
            value(format!("{} ({})", e.action_label(), e.action)),
        ]),
        Line::from(vec![
            label("result:"),
            if e.succeeded {
                Span::styled("succeeded", Style::default().fg(theme.healthy))
            } else {
                Span::styled("FAILED", Style::default().fg(theme.critical))
            },
        ]),
        Line::from(vec![label("database:"), value(e.database.clone())]),
        Line::from(vec![
            label("client:"),
            value(format!(
                "{}  {}  {}",
                e.app,
                e.ip,
                if e.host.is_empty() { "—" } else { &e.host }
            )),
        ]),
        Line::from(vec![
            label("rows:"),
            value(format!(
                "returned {}  affected {}",
                e.response_rows.map_or("—".into(), |n| n.to_string()),
                e.affected_rows.map_or("—".into(), |n| n.to_string()),
            )),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "statement:",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )),
    ];
    if e.statement.is_empty() {
        lines.push(Line::from(Span::styled(
            "(none — login / session event)",
            Style::default().fg(theme.muted),
        )));
    } else {
        for l in e.statement.lines() {
            lines.push(Line::from(Span::styled(
                l.to_string(),
                Style::default().fg(theme.fg),
            )));
        }
    }
    if !e.info.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "additional information:",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )));
        // Failed events carry the actual error here (invalid object name,
        // permission denied, …) as raw XML — shown verbatim.
        for l in e.info.lines() {
            lines.push(Line::from(Span::styled(
                l.to_string(),
                Style::default().fg(theme.fg),
            )));
        }
    }

    let p = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((audit.detail_scroll, 0));
    frame.render_widget(p, inner);
    render_footer(frame, chunks[1], DETAIL_FOOTER, theme);
}

fn build_event_row<'a>(e: &'a AuditEvent, theme: &Theme) -> Row<'a> {
    let when =
        e.ts.with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
    // Failures (mostly failed logins) are what an auditor is hunting — red.
    let action_style = if e.succeeded {
        Style::default().fg(theme.fg)
    } else {
        Style::default().fg(theme.critical)
    };
    let rows = e
        .response_rows
        .or(e.affected_rows)
        .map(|n| n.to_string())
        .unwrap_or_else(|| "—".to_string());
    Row::new(vec![
        Cell::from(when).style(Style::default().fg(theme.muted)),
        Cell::from(e.action_label()).style(action_style),
        Cell::from(e.database.as_str()).style(Style::default().fg(theme.fg)),
        Cell::from(e.app.as_str()).style(Style::default().fg(theme.muted)),
        Cell::from(e.ip.as_str()).style(Style::default().fg(theme.muted)),
        Cell::from(rows).style(Style::default().fg(theme.muted)),
        Cell::from(flatten_statement(&e.statement)).style(Style::default().fg(theme.fg)),
    ])
}

/// Single-line display form of a statement: whitespace runs (newlines, tabs,
/// indentation) collapsed to single spaces so the table cell reads as one row.
fn flatten_statement(stmt: &str) -> String {
    stmt.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Warning / status line for the database-users merge (⚠ live T-SQL). Shown
/// only when the audit target is a single database — where the merge applies.
/// Explicit by design: the user asked that any live SQL be announced on the
/// window itself, with the off-switch named.
fn render_users_note(frame: &mut Frame, inner: Rect, state: &AppState, theme: &Theme) -> Rect {
    let audit = &state.sql.audit;
    if audit.target.as_ref().is_none_or(|t| t.database.is_none()) {
        return inner;
    }
    let (text, style) = if let Some(users) = audit.db_users.as_deref() {
        (
            format!(
                "⚠ merged {} database users via live T-SQL (sys.database_principals) — never-seen ones at the bottom · disable: sql_live_queries = false",
                users.len()
            ),
            Style::default().fg(theme.degraded),
        )
    } else if audit.db_users_pending {
        (
            "⚠ listing database users via live T-SQL…".to_string(),
            Style::default().fg(theme.degraded),
        )
    } else if let Some(note) = audit.db_users_note.as_deref() {
        (
            format!("database users not merged: {note}"),
            Style::default().fg(theme.muted),
        )
    } else {
        return inner;
    };
    let parts = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(inner);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(text, style))),
        parts[0],
    );
    parts[1]
}

/// The `/`-filter entry row (principals view), shown only while the filter
/// input has focus. Returns the body area left below it.
fn render_filter_input(frame: &mut Frame, inner: Rect, state: &AppState, theme: &Theme) -> Rect {
    if !state.sql.audit.principals_filter_active {
        return inner;
    }
    let parts = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(inner);
    let p = Paragraph::new(Line::from(vec![
        Span::styled("> ", Style::default().fg(theme.accent)),
        Span::styled(
            state.sql.audit.principals_filter.value(),
            Style::default().fg(theme.fg),
        ),
        Span::styled("█", Style::default().fg(theme.accent)),
    ]));
    frame.render_widget(p, parts[0]);
    parts[1]
}

/// Custom-window entry row, shown only while `t` has focus. Returns the body
/// area left below it.
fn render_window_input(frame: &mut Frame, inner: Rect, state: &AppState, theme: &Theme) -> Rect {
    if !state.sql.audit.window_input_active {
        return inner;
    }
    let parts = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(inner);
    let p = Paragraph::new(Line::from(vec![
        Span::styled("window> ", Style::default().fg(theme.accent)),
        Span::styled(
            state.sql.audit.window_input.value(),
            Style::default().fg(theme.fg),
        ),
        Span::styled("█", Style::default().fg(theme.accent)),
        Span::styled(
            "  (e.g. 12h, 30d, 6m, 1y — Enter applies, Esc cancels)",
            Style::default().fg(theme.muted),
        ),
    ]));
    frame.render_widget(p, parts[0]);
    parts[1]
}

fn render_footer(frame: &mut Frame, area: Rect, hint: &'static str, theme: &Theme) {
    let p = Paragraph::new(Line::from(Span::styled(
        hint,
        Style::default().fg(theme.muted),
    )));
    frame.render_widget(p, area);
}

/// The `3` key's 30-day window — identical to the view's default, so pressing
/// `3` also reads as "back to the default lookback".
fn month_window() -> AccessWindow {
    AccessWindow::Custom {
        hours: 30 * 24,
        label: "30d".to_string(),
    }
}

/// The `9` key's 1-year window (matches `AccessWindow::parse("1y")`, so `9`
/// and typing `1y` via `t` are the same scope — no spurious refetch between
/// them). Whether a year of rows actually exists depends on the workspace's
/// retention.
fn year_window() -> AccessWindow {
    AccessWindow::Custom {
        hours: 365 * 24,
        label: "1y".to_string(),
    }
}

fn set_window(state: &mut AppState, window: AccessWindow) -> bool {
    if state.sql.audit.window == window {
        return true;
    }
    state.sql.audit.window = window;
    state.sql.audit.invalidate_fetch();
    true
}

/// Yank text for the selected row: the roll-up line on the principals view,
/// the full (unflattened) statement plus metadata on the events view.
pub fn yank_text(state: &AppState) -> Option<String> {
    let audit = &state.sql.audit;
    match state.view {
        View::SqlAuditEvents | View::SqlAuditEventDetail => {
            let e = selected_event(state)?;
            let mut parts = vec![
                e.ts.to_rfc3339(),
                e.action_label().to_string(),
                e.database.clone(),
                e.app.clone(),
                e.ip.clone(),
            ];
            if !e.host.is_empty() {
                parts.push(e.host.clone());
            }
            if !e.statement.is_empty() {
                parts.push(e.statement.clone());
            }
            if !e.info.is_empty() {
                parts.push(e.info.clone());
            }
            Some(parts.join("  "))
        }
        _ => {
            let rows = filtered_principals(state);
            // Past the audited rows sit the silent database users.
            if audit.cursor >= rows.len() {
                let silent = silent_users(state);
                let u = silent.get(audit.cursor.saturating_sub(rows.len()))?;
                return Some(format!(
                    "{}  {}  created={}  no audit rows in {}",
                    u.name,
                    u.kind_tag(),
                    u.created.map(|c| c.to_rfc3339()).unwrap_or_default(),
                    audit.window.label(),
                ));
            }
            let r = rows.get(audit.cursor.min(rows.len().checked_sub(1)?))?;
            // The raw identity leads — that's what GRANT/DROP statements need;
            // the resolved directory name rides along for humans.
            let resolved = display_principal(state, &r.principal);
            let resolved_suffix = if resolved == r.principal {
                String::new()
            } else {
                format!(" ({resolved})")
            };
            Some(format!(
                "{}{}  last_seen={}  events={}  queries={}  logins={}  failed={}  ips={}  dbs={}  apps={}",
                r.principal,
                resolved_suffix,
                r.last_seen.to_rfc3339(),
                r.events,
                r.queries,
                r.logins,
                r.failed,
                r.distinct_ips,
                r.databases.join(","),
                r.apps.join(","),
            ))
        }
    }
}

pub fn handle(action: Action, state: &mut AppState) -> bool {
    // The event detail is a leaf: j/k scroll it, window keys don't apply.
    if state.view == View::SqlAuditEventDetail {
        return handle_event_detail(action, state);
    }

    // Custom-window input focus: raw keys flow into the input via app.rs;
    // only Enter (apply) and Esc (cancel) land here as actions.
    if state.sql.audit.window_input_active {
        match action {
            Action::Back => {
                state.sql.audit.window_input_active = false;
                state.sql.audit.window_input.reset();
                return true;
            }
            Action::OpenSelected => {
                let raw = state.sql.audit.window_input.value().to_string();
                match AccessWindow::parse(&raw) {
                    Some(window) => {
                        state.sql.audit.window_input_active = false;
                        state.sql.audit.window_input.reset();
                        set_window(state, window);
                    }
                    None => {
                        state
                            .set_status(format!("can't parse \"{raw}\" — try 12h, 30d, 6m, or 1y"));
                    }
                }
                return true;
            }
            _ => return false,
        }
    }

    match action {
        Action::SetWindowHour => return set_window(state, AccessWindow::Hour),
        Action::SetWindowDay => return set_window(state, AccessWindow::Day),
        Action::SetWindowWeek => return set_window(state, AccessWindow::Week),
        Action::SetWindowMonth => return set_window(state, month_window()),
        Action::SetWindowYear => return set_window(state, year_window()),
        Action::SetCustomWindow => {
            state.sql.audit.window_input.reset();
            state.sql.audit.window_input_active = true;
            return true;
        }
        _ => {}
    }

    match state.view {
        View::SqlAuditEvents => handle_events(action, state),
        _ => handle_principals(action, state),
    }
}

fn handle_principals(action: Action, state: &mut AppState) -> bool {
    // Filter-input focus: printable keys flow into the input via app.rs; only
    // the carved-out keys land here as actions (same contract as the SQL list).
    if state.sql.audit.principals_filter_active {
        match action {
            Action::Back => {
                state.sql.audit.principals_filter_active = false;
                state.sql.audit.principals_filter.reset();
                state.sql.audit.cursor = 0;
                return true;
            }
            Action::OpenSelected => {
                // Enter applies: close the input, keep the narrowing.
                state.sql.audit.principals_filter_active = false;
                return true;
            }
            Action::MoveDown => {
                state.sql.audit.principals_filter_active = false;
            }
            Action::MoveUp
            | Action::HalfPageDown
            | Action::HalfPageUp
            | Action::GotoTop
            | Action::GotoBottom => {}
            _ => return false,
        }
    }
    if action == Action::StartSearch {
        state.sql.audit.principals_filter.reset();
        state.sql.audit.cursor = 0;
        state.sql.audit.principals_filter_active = true;
        return true;
    }

    // The cursor indexes the *filtered* list plus the appended silent users,
    // same as the table render. Enter's drill target is resolved here too,
    // before the borrow below — drilling into a silent user opens its (empty)
    // events view, which is itself the proof of silence.
    let filtered = filtered_principals(state);
    let silent = silent_users(state);
    let len = filtered.len() + silent.len();
    let cursor = state.sql.audit.cursor;
    let drill_target = if cursor < filtered.len() {
        filtered.get(cursor).map(|r| r.principal.clone())
    } else {
        silent
            .get(cursor.saturating_sub(filtered.len()))
            .map(|u| u.name.clone())
    };
    let audit = &mut state.sql.audit;
    match action {
        Action::MoveDown => {
            if len > 0 {
                audit.cursor = (audit.cursor + 1).min(len - 1);
            }
            true
        }
        Action::MoveUp => {
            audit.cursor = audit.cursor.saturating_sub(1);
            true
        }
        Action::HalfPageDown => {
            if len > 0 {
                audit.cursor = (audit.cursor + HALF_PAGE).min(len - 1);
            }
            true
        }
        Action::HalfPageUp => {
            audit.cursor = audit.cursor.saturating_sub(HALF_PAGE);
            true
        }
        Action::GotoTop => {
            audit.cursor = 0;
            true
        }
        Action::GotoBottom => {
            audit.cursor = len.saturating_sub(1);
            true
        }
        Action::OpenSelected => {
            if let Some(principal) = drill_target {
                audit.selected_principal = Some(principal);
                audit.drop_events();
                state.view = View::SqlAuditEvents;
            }
            true
        }
        Action::OpenSessions => {
            // `u`: open sessions on the same target (⚠ live T-SQL).
            super::sql_resources::open_sessions_view(state, View::SqlAuditPrincipals);
            true
        }
        Action::Back => {
            // Return to wherever `l` was pressed: the SQL list or the detail.
            state.view = audit.return_view.take().unwrap_or(View::SqlResources);
            true
        }
        _ => false,
    }
}

fn handle_events(action: Action, state: &mut AppState) -> bool {
    match action {
        Action::OpenSelected => {
            if selected_event(state).is_some() {
                state.sql.audit.detail_scroll = 0;
                state.view = View::SqlAuditEventDetail;
            }
            return true;
        }
        Action::CycleSourceFilter => {
            cycle_action(state, 1);
            return true;
        }
        Action::CycleSourceFilterBack => {
            cycle_action(state, -1);
            return true;
        }
        _ => {}
    }
    // The cursor indexes the *visible* (action-filtered) list, same as render.
    let len = state.sql.audit.visible_events().len();
    let audit = &mut state.sql.audit;
    match action {
        Action::MoveDown => {
            // At the last row with more in the window: request the older-than
            // page (drained by `after_action` — handlers can't spawn).
            if len > 0 && audit.events_cursor >= len - 1 {
                request_older_page(audit);
            }
            if len > 0 {
                audit.events_cursor = (audit.events_cursor + 1).min(len - 1);
            }
            true
        }
        Action::MoveUp => {
            audit.events_cursor = audit.events_cursor.saturating_sub(1);
            true
        }
        Action::HalfPageDown => {
            if len > 0 {
                audit.events_cursor = (audit.events_cursor + HALF_PAGE).min(len - 1);
            }
            true
        }
        Action::HalfPageUp => {
            audit.events_cursor = audit.events_cursor.saturating_sub(HALF_PAGE);
            true
        }
        Action::GotoTop => {
            audit.events_cursor = 0;
            true
        }
        Action::GotoBottom => {
            audit.events_cursor = len.saturating_sub(1);
            request_older_page(audit);
            true
        }
        Action::ToggleErrorsOnly => {
            // A query parameter, like the window: the buffer no longer
            // matches the header, so drop it — `after_action`'s force branch
            // spawns the refetch under the new filter.
            audit.events_errors_only = !audit.events_errors_only;
            audit.drop_events();
            audit.events_generation = audit.events_generation.wrapping_add(1);
            true
        }
        Action::Back => {
            state.view = View::SqlAuditPrincipals;
            true
        }
        _ => false,
    }
}

/// Ask for the page older than the loaded buffer, if there is one and no
/// fetch is already running.
fn request_older_page(audit: &mut crate::ui::state::SqlAuditState) {
    if audit.events_truncated
        && !audit.events_loading_more
        && !audit.events_pending
        && audit.events.is_some()
    {
        audit.events_fetch_older = true;
    }
}

/// Cycle the client-side action filter: all → action₁ → action₂ → … → all, in
/// the sorted order of action codes present in the fetched page (mirrors the
/// KV access view's operation filter).
fn cycle_action(state: &mut AppState, direction: i32) {
    let actions = state.sql.audit.event_actions();
    if actions.is_empty() {
        return;
    }
    let all = actions.len() as i32;
    let next = match state.sql.audit.events_action_filter.as_deref() {
        None => Some((all + direction).rem_euclid(all + 1)),
        Some(current) => actions
            .iter()
            .position(|a| a == current)
            .map(|i| (i as i32 + direction).rem_euclid(all + 1)),
    };
    state.sql.audit.events_action_filter = match next {
        Some(n) if n != all => Some(actions[n as usize].clone()),
        _ => None,
    };
    state.sql.audit.events_cursor = 0;
    state.sql.audit.events_view_top.set(0);
}

/// Event-detail leaf: scroll and leave — everything else is inert.
fn handle_event_detail(action: Action, state: &mut AppState) -> bool {
    let audit = &mut state.sql.audit;
    match action {
        Action::MoveDown => {
            audit.detail_scroll = audit.detail_scroll.saturating_add(1);
            true
        }
        Action::MoveUp => {
            audit.detail_scroll = audit.detail_scroll.saturating_sub(1);
            true
        }
        Action::HalfPageDown => {
            audit.detail_scroll = audit.detail_scroll.saturating_add(HALF_PAGE as u16);
            true
        }
        Action::HalfPageUp => {
            audit.detail_scroll = audit.detail_scroll.saturating_sub(HALF_PAGE as u16);
            true
        }
        Action::GotoTop => {
            audit.detail_scroll = 0;
            true
        }
        Action::Back => {
            audit.detail_scroll = 0;
            state.view = View::SqlAuditEvents;
            true
        }
        // Enter is a no-op here (already at the leaf) — consuming it keeps
        // the global handler from re-kicking loads.
        Action::OpenSelected => true,
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

    fn principal(name: &str, days_ago: i64, events: u64, failed: u64) -> PrincipalSummary {
        PrincipalSummary {
            principal: name.to_string(),
            last_seen: Utc::now() - Duration::days(days_ago),
            events,
            queries: events.saturating_sub(failed) / 2,
            logins: events / 4,
            failed,
            databases: vec!["orders".to_string()],
            distinct_ips: 2,
            apps: vec!["orders-api".to_string()],
        }
    }

    fn event(min_ago: i64, action: &str, succeeded: bool, stmt: &str) -> AuditEvent {
        AuditEvent {
            ts: Utc::now() - Duration::minutes(min_ago),
            action: action.to_string(),
            succeeded,
            database: "orders".to_string(),
            ip: "10.0.0.4".to_string(),
            app: "orders-api".to_string(),
            host: "host-1".to_string(),
            statement: stmt.to_string(),
            info: String::new(),
            affected_rows: None,
            response_rows: Some(12),
        }
    }

    fn fixture() -> AppState {
        let mut state = AppState::new(Config::default());
        state.view = View::SqlAuditPrincipals;
        state.sql.audit.target = Some(AuditTarget {
            master_id: "/subscriptions/s/resourceGroups/rg/providers/Microsoft.Sql/servers/srv/databases/master".to_string(),
            database_id: None,
            server: "srv".to_string(),
            database: None,
        });
        state.sql.audit.principals = Some(vec![
            principal("app-orders", 0, 4211, 0),
            principal("legacy_readonly", 190, 12, 12),
        ]);
        state
    }

    #[test]
    fn window_keys_change_window_and_drop_buffers() {
        let mut state = fixture();
        state.sql.audit.events = Some(vec![event(1, "BCM", true, "SELECT 1")]);
        let gen0 = state.sql.audit.generation;
        assert!(handle(Action::SetWindowWeek, &mut state));
        assert_eq!(state.sql.audit.window, AccessWindow::Week);
        assert!(state.sql.audit.principals.is_none());
        assert!(state.sql.audit.events.is_none());
        assert_eq!(state.sql.audit.generation, gen0 + 1);
        // Same window again: no invalidation.
        state.sql.audit.principals = Some(vec![principal("x", 0, 1, 0)]);
        assert!(handle(Action::SetWindowWeek, &mut state));
        assert!(state.sql.audit.principals.is_some());
    }

    #[test]
    fn custom_window_input_parses_and_applies() {
        let mut state = fixture();
        assert!(handle(Action::SetCustomWindow, &mut state));
        assert!(state.sql.audit.window_input_active);
        state.sql.audit.window_input = "6m".into();
        assert!(handle(Action::OpenSelected, &mut state));
        assert!(!state.sql.audit.window_input_active);
        assert_eq!(state.sql.audit.window.label(), "6m");
        assert!(
            state.sql.audit.principals.is_none(),
            "scope changed → refetch"
        );
        // Junk keeps the input open for a retry.
        handle(Action::SetCustomWindow, &mut state);
        state.sql.audit.window_input = "sixmonths".into();
        assert!(handle(Action::OpenSelected, &mut state));
        assert!(state.sql.audit.window_input_active);
        assert!(state.status_message.is_some());
    }

    #[test]
    fn enter_drills_into_principal_and_esc_walks_back() {
        let mut state = fixture();
        state.sql.audit.return_view = Some(View::SqlDetail);
        state.sql.audit.cursor = 1;
        assert!(handle(Action::OpenSelected, &mut state));
        assert_eq!(state.view, View::SqlAuditEvents);
        assert_eq!(
            state.sql.audit.selected_principal.as_deref(),
            Some("legacy_readonly")
        );
        assert!(state.sql.audit.events.is_none(), "fresh principal → fetch");
        // Esc: events → principals → recorded origin.
        assert!(handle(Action::Back, &mut state));
        assert_eq!(state.view, View::SqlAuditPrincipals);
        assert!(handle(Action::Back, &mut state));
        assert_eq!(state.view, View::SqlDetail);
        // Without a recorded origin, fall back to the SQL list.
        state.view = View::SqlAuditPrincipals;
        assert!(handle(Action::Back, &mut state));
        assert_eq!(state.view, View::SqlResources);
    }

    #[test]
    fn renders_principal_rollup_with_stale_ago() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(170, 16);
        let mut term = Terminal::new(backend).unwrap();
        let state = fixture();
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("app-orders"), "principal column missing");
        assert!(s.contains("legacy_readonly"));
        assert!(s.contains("PRINCIPAL"), "header missing");
        assert!(s.contains("QUERIES"), "queries column missing");
        assert!(s.contains("LOGINS"), "logins column missing");
        assert!(s.contains("6mo"), "stale principal shows months-ago");
        assert!(s.contains("srv"), "target chip missing");
        assert!(s.contains("30d"), "default window chip missing");
    }

    #[test]
    fn resolved_principals_render_their_directory_name() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(170, 16);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.sql.audit.principals = Some(vec![principal(
            "f3c9a2e1-0d4b-4f7e-9a1c-2b5d8e7f6a3c@9188040d-6c67-4c5b-b112-36a304b66dad",
            0,
            10,
            0,
        )]);
        state.principals.by_id.insert(
            "f3c9a2e1-0d4b-4f7e-9a1c-2b5d8e7f6a3c".to_string(),
            "sp-orders-deploy".to_string(),
        );
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("sp-orders-deploy"), "resolved name missing");
        // Yank keeps the raw identity (what a DROP USER needs) plus the name.
        let y = yank_text(&state).unwrap();
        assert!(y.contains("f3c9a2e1-0d4b-4f7e-9a1c-2b5d8e7f6a3c@"));
        assert!(y.contains("(sp-orders-deploy)"));
    }

    #[test]
    fn loading_state_shows_the_kql() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(170, 40);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.sql.audit.principals = None;
        state.sql.audit.pending = true;
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("union isfuzzy=true"), "KQL preview missing");
        assert!(s.contains("summarize last_seen"), "roll-up tail missing");
        // The events view previews its own query.
        state.view = View::SqlAuditEvents;
        state.sql.audit.selected_principal = Some("app-orders".to_string());
        state.sql.audit.events_pending = true;
        let mut term = Terminal::new(TestBackend::new(170, 40)).unwrap();
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(
            s.contains(r#"principal_ =~ "app-orders""#),
            "events KQL missing"
        );
    }

    #[test]
    fn errors_toggle_is_a_query_param_and_drops_the_buffer() {
        let mut state = fixture();
        state.view = View::SqlAuditEvents;
        state.sql.audit.events = Some(vec![event(1, "BCM", true, "SELECT 1")]);
        let gen0 = state.sql.audit.events_generation;
        assert!(handle(Action::ToggleErrorsOnly, &mut state));
        assert!(state.sql.audit.events_errors_only);
        assert!(state.sql.audit.events.is_none(), "buffer dropped → refetch");
        assert_eq!(state.sql.audit.events_generation, gen0 + 1);
        // Toggling back keeps the same contract.
        assert!(handle(Action::ToggleErrorsOnly, &mut state));
        assert!(!state.sql.audit.events_errors_only);
    }

    #[test]
    fn enter_opens_event_detail_with_statement_and_error_info() {
        let theme = Theme::catppuccin_mocha();
        let mut state = fixture();
        state.view = View::SqlAuditEvents;
        state.sql.audit.selected_principal = Some("legacy_readonly".to_string());
        let mut failed = event(1, "BCM", false, "select * from sys.users");
        failed.info = "<action_info><error>Invalid object name 'sys.users'.</error></action_info>"
            .to_string();
        state.sql.audit.events = Some(vec![failed]);

        assert!(handle(Action::OpenSelected, &mut state));
        assert_eq!(state.view, View::SqlAuditEventDetail);

        let mut term = Terminal::new(TestBackend::new(120, 24)).unwrap();
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("select * from sys.users"), "statement missing");
        assert!(s.contains("Invalid object name"), "error info missing");
        assert!(s.contains("FAILED"), "result line missing");
        // Yank includes statement and the error info.
        let y = yank_text(&state).unwrap();
        assert!(y.contains("sys.users") && y.contains("Invalid object name"));
        // j scrolls, Esc returns to the events table.
        assert!(handle(Action::MoveDown, &mut state));
        assert_eq!(state.sql.audit.detail_scroll, 1);
        assert!(handle(Action::Back, &mut state));
        assert_eq!(state.view, View::SqlAuditEvents);
        assert_eq!(state.sql.audit.detail_scroll, 0);
    }

    #[test]
    fn tab_cycles_action_filter_through_present_codes() {
        let mut state = fixture();
        state.view = View::SqlAuditEvents;
        state.sql.audit.events = Some(vec![
            event(1, "BCM", true, "SELECT 1"),
            event(2, "TRCC", true, ""),
            event(3, "BCM", true, "SELECT 2"),
        ]);
        // all → BCM → TRCC → all
        assert!(handle(Action::CycleSourceFilter, &mut state));
        assert_eq!(state.sql.audit.events_action_filter.as_deref(), Some("BCM"));
        assert_eq!(state.sql.audit.visible_events().len(), 2);
        assert!(handle(Action::CycleSourceFilter, &mut state));
        assert_eq!(
            state.sql.audit.events_action_filter.as_deref(),
            Some("TRCC")
        );
        assert_eq!(state.sql.audit.visible_events().len(), 1);
        assert!(handle(Action::CycleSourceFilter, &mut state));
        assert_eq!(state.sql.audit.events_action_filter, None);
        assert_eq!(state.sql.audit.visible_events().len(), 3);
        // The detail drill-in follows the filtered cursor.
        handle(Action::CycleSourceFilter, &mut state); // → BCM
        state.sql.audit.events_cursor = 1;
        assert!(handle(Action::OpenSelected, &mut state));
        assert_eq!(state.view, View::SqlAuditEventDetail);
        assert_eq!(
            selected_event(&state).map(|e| e.statement.as_str()),
            Some("SELECT 2"),
            "second *visible* (BCM) row, not the TRCC row"
        );
    }

    #[test]
    fn slash_filter_narrows_matches_resolved_names_and_esc_resets() {
        let theme = Theme::catppuccin_mocha();
        let mut state = fixture();
        state.sql.audit.principals = Some(vec![
            principal("app-orders", 0, 10, 0),
            principal("legacy_readonly", 190, 12, 0),
            principal("f3c9a2e1-0d4b-4f7e-9a1c-2b5d8e7f6a3c", 1, 5, 0),
        ]);
        state.principals.by_id.insert(
            "f3c9a2e1-0d4b-4f7e-9a1c-2b5d8e7f6a3c".to_string(),
            "sp-orders-deploy".to_string(),
        );

        // `/` opens the filter; typed value narrows (input fed via app.rs).
        assert!(handle(Action::StartSearch, &mut state));
        assert!(state.sql.audit.principals_filter_active);
        state.sql.audit.principals_filter = "orders".into();
        // Matches app-orders (raw) AND the GUID row via its resolved name.
        assert_eq!(filtered_principals(&state).len(), 2);

        // The search row and chip render while active.
        let mut term = Terminal::new(TestBackend::new(170, 16)).unwrap();
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("/orders"), "filter chip missing");
        assert!(!s.contains("legacy_readonly"), "filtered row still renders");
        assert!(s.contains("2 of 3 principals"), "count chip missing");

        // Enter closes the input but keeps the narrowing; the drill-in
        // follows the filtered cursor.
        assert!(handle(Action::OpenSelected, &mut state));
        assert!(!state.sql.audit.principals_filter_active);
        assert_eq!(filtered_principals(&state).len(), 2);
        state.sql.audit.cursor = 1; // second *visible* row = the GUID one
        assert!(handle(Action::OpenSelected, &mut state));
        assert_eq!(state.view, View::SqlAuditEvents);
        assert_eq!(
            state.sql.audit.selected_principal.as_deref(),
            Some("f3c9a2e1-0d4b-4f7e-9a1c-2b5d8e7f6a3c")
        );

        // Esc while the input is focused clears everything.
        state.view = View::SqlAuditPrincipals;
        handle(Action::StartSearch, &mut state);
        state.sql.audit.principals_filter = "orders".into();
        assert!(handle(Action::Back, &mut state));
        assert!(!state.sql.audit.principals_filter_active);
        assert_eq!(state.sql.audit.principals_filter.value(), "");
        assert_eq!(filtered_principals(&state).len(), 3);
    }

    #[test]
    fn silent_db_users_merge_below_audited_principals() {
        use crate::azure::sql_tds::DbUser;
        let theme = Theme::catppuccin_mocha();
        let mut state = fixture();
        // Scope the target to a database — the merge only applies there.
        state.sql.audit.target.as_mut().unwrap().database = Some("orders".to_string());
        state.sql.audit.db_users = Some(vec![
            DbUser {
                // Already audited (case differs — match is case-insensitive).
                name: "APP-ORDERS".to_string(),
                kind: "EXTERNAL_USER".to_string(),
                auth: "EXTERNAL".to_string(),
                created: None,
            },
            DbUser {
                name: "temp_migration_user".to_string(),
                kind: "SQL_USER".to_string(),
                auth: "DATABASE".to_string(),
                created: Some(Utc::now() - Duration::days(800)),
            },
        ]);

        let silent = silent_users(&state);
        assert_eq!(silent.len(), 1, "audited user filtered out of the merge");
        assert_eq!(silent[0].name, "temp_migration_user");

        // An Entra app audited under its raw GUID identity but resolved (via
        // Graph) to the same display name the database user carries must NOT
        // reappear as never-seen — same identity, two spellings.
        state.sql.audit.principals.as_mut().unwrap().push(principal(
            "f3c9a2e1-0d4b-4f7e-9a1c-2b5d8e7f6a3c@9188040d-6c67-4c5b-b112-36a304b66dad",
            0,
            50,
            0,
        ));
        state.sql.audit.db_users.as_mut().unwrap().push(DbUser {
            name: "sp-orders-deploy".to_string(),
            kind: "EXTERNAL_USER".to_string(),
            auth: "EXTERNAL".to_string(),
            created: None,
        });
        // Before resolution lands the names differ → briefly listed…
        assert_eq!(silent_users(&state).len(), 2);
        // …and once Graph resolves the GUID to the matching display name the
        // duplicate collapses.
        state.principals.by_id.insert(
            "f3c9a2e1-0d4b-4f7e-9a1c-2b5d8e7f6a3c".to_string(),
            "sp-orders-deploy".to_string(),
        );
        let silent = silent_users(&state);
        assert_eq!(silent.len(), 1, "resolved identity deduped");
        assert_eq!(silent[0].name, "temp_migration_user");

        let mut term = Terminal::new(TestBackend::new(180, 18)).unwrap();
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("temp_migration_user"), "silent row missing");
        assert!(s.contains("no audit rows"), "silence marker missing");
        assert!(s.contains("∅ 30d"), "window-scoped never marker missing");
        assert!(s.contains("sql user"), "kind tag missing");
        assert!(
            s.contains("live T-SQL") && s.contains("sql_live_queries = false"),
            "warning line with off-switch missing"
        );
        assert!(s.contains("1 never seen"), "header chip missing");

        // Cursor walks past the audited rows into the silent block; Enter
        // drills into the (empty) events view for that user; y yanks it.
        let audited = filtered_principals(&state).len();
        state.sql.audit.cursor = audited; // first silent row
        let y = yank_text(&state).unwrap();
        assert!(y.contains("temp_migration_user"));
        assert!(y.contains("no audit rows in 30d"));
        assert!(handle(Action::OpenSelected, &mut state));
        assert_eq!(state.view, View::SqlAuditEvents);
        assert_eq!(
            state.sql.audit.selected_principal.as_deref(),
            Some("temp_migration_user")
        );

        // The `/` filter applies to silent users too.
        state.view = View::SqlAuditPrincipals;
        state.sql.audit.principals_filter = "migration".into();
        assert_eq!(filtered_principals(&state).len(), 0);
        assert_eq!(silent_users(&state).len(), 1);
    }

    #[test]
    fn db_target_kql_preview_announces_the_tsql() {
        let theme = Theme::catppuccin_mocha();
        let mut state = fixture();
        state.sql.audit.target.as_mut().unwrap().database = Some("orders".to_string());
        state.sql.audit.principals = None;
        state.sql.audit.pending = true;
        let mut term = Terminal::new(TestBackend::new(170, 60)).unwrap();
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("union isfuzzy=true"), "KQL missing");
        assert!(
            s.contains("sys.database_principals"),
            "T-SQL announcement missing"
        );

        // Config off: no T-SQL announced (none will run).
        state.config.sql_live_queries = false;
        let mut term = Terminal::new(TestBackend::new(170, 60)).unwrap();
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(!s.contains("sys.database_principals"));
    }

    #[test]
    fn three_key_selects_the_30d_window() {
        let mut state = fixture();
        // Narrow to 1h first, then jump back to 30d with `3`.
        assert!(handle(Action::SetWindowHour, &mut state));
        assert_eq!(state.sql.audit.window.label(), "1h");
        assert!(handle(Action::SetWindowMonth, &mut state));
        assert_eq!(state.sql.audit.window.label(), "30d");
        assert_eq!(state.sql.audit.window.duration().num_days(), 30);
        // Same window again: no invalidation churn.
        state.sql.audit.principals = Some(vec![principal("x", 0, 1, 0)]);
        assert!(handle(Action::SetWindowMonth, &mut state));
        assert!(state.sql.audit.principals.is_some());
    }

    #[test]
    fn nine_key_selects_the_1y_window() {
        let mut state = fixture();
        assert!(handle(Action::SetWindowYear, &mut state));
        assert_eq!(state.sql.audit.window.label(), "1y");
        assert_eq!(state.sql.audit.window.duration().num_days(), 365);
        assert!(
            state.sql.audit.principals.is_none(),
            "scope changed → refetch"
        );
        // `9` and typing "1y" via `t` are the same scope — no refetch between.
        state.sql.audit.principals = Some(vec![principal("x", 0, 1, 0)]);
        assert_eq!(
            state.sql.audit.window,
            crate::azure::key_vault_logs::AccessWindow::parse("1y").unwrap()
        );
        assert!(handle(Action::SetWindowYear, &mut state));
        assert!(
            state.sql.audit.principals.is_some(),
            "no invalidation churn"
        );
    }

    #[test]
    fn bottom_of_a_truncated_page_requests_older_rows() {
        let mut state = fixture();
        state.view = View::SqlAuditEvents;
        state.sql.audit.events = Some(vec![
            event(1, "BCM", true, "SELECT 1"),
            event(2, "BCM", true, "SELECT 2"),
        ]);
        state.sql.audit.events_truncated = true;
        state.sql.audit.events_cursor = 0;
        // Not at the bottom yet: plain move, no request.
        assert!(handle(Action::MoveDown, &mut state));
        assert!(!state.sql.audit.events_fetch_older);
        // At the last row: the next MoveDown asks for the older page.
        assert!(handle(Action::MoveDown, &mut state));
        assert!(state.sql.audit.events_fetch_older);
        // G asks too; but a page that isn't truncated never does.
        state.sql.audit.events_fetch_older = false;
        state.sql.audit.events_truncated = false;
        assert!(handle(Action::GotoBottom, &mut state));
        assert!(!state.sql.audit.events_fetch_older);
    }

    #[test]
    fn renders_empty_state_with_auditing_hint() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(120, 14);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.sql.audit.principals = Some(Vec::new());
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("Auditing settings"));
    }

    #[test]
    fn renders_events_with_flattened_statement() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(170, 16);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.view = View::SqlAuditEvents;
        state.sql.audit.selected_principal = Some("app-orders".to_string());
        state.sql.audit.events = Some(vec![
            event(1, "BCM", true, "SELECT *\n  FROM orders\n  WHERE id = 1"),
            event(2, "DBAF", false, ""),
        ]);
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("SELECT * FROM orders"), "statement flattened");
        assert!(s.contains("login-failed"), "action label missing");
        assert!(s.contains("app-orders"), "principal chip missing");
    }

    #[test]
    fn yank_principals_and_events() {
        let mut state = fixture();
        let y = yank_text(&state).unwrap();
        assert!(y.contains("app-orders"));
        assert!(y.contains("events=4211"));

        state.view = View::SqlAuditEvents;
        state.sql.audit.events = Some(vec![event(1, "BCM", true, "SELECT *\nFROM orders")]);
        let y = yank_text(&state).unwrap();
        assert!(
            y.contains("SELECT *\nFROM orders"),
            "yank keeps the original (unflattened) statement"
        );
        assert!(y.contains("batch"));
    }
}
