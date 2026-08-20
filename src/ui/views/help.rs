//! Help overlay. Toggled by `?`. Shows a keymap popup **scoped to the view it
//! was opened from**: navigation and global keys always, plus the sections
//! for the current category — cosmos controls are noise on a SQL audit page
//! and vice versa. The "Go to" section lists the palette commands that jump
//! between categories, so every other mode is one `:command` away.

#![allow(dead_code, unused_variables)]

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::ui::events::Action;
use crate::ui::state::{AppState, Category, View};
use crate::ui::theme::Theme;

type Section = (&'static str, &'static [(&'static str, &'static str)]);

const NAVIGATION: Section = (
    "Navigation",
    &[
        ("j / k", "down / up"),
        ("h", "left"),
        ("g g", "go to top"),
        ("G", "go to bottom"),
        ("Ctrl-d / Ctrl-u", "half page down / up"),
        ("Esc", "back"),
    ],
);

const API_RESOURCES: Section = (
    "API resources",
    &[
        ("Enter", "open detail"),
        ("l", "open logs"),
        ("f", "toggle favorite"),
        ("F", "favorites only"),
        ("/", "search"),
        ("s", "switch subscription"),
    ],
);

const DETAIL_LOGS: Section = (
    "Detail / Logs",
    &[
        ("Enter", "log line detail (logs)"),
        ("0", "window 1h"),
        ("1", "window 1d"),
        ("7", "window 7d"),
        ("w", "wrap (logs)"),
        ("e", "errors only (logs) / env vars (detail)"),
        ("E", "jump to next error, fetching older rows (logs)"),
        (
            "H",
            "hide health-probe requests (/health, /healthz, /warmup, …) (logs)",
        ),
        ("Tab / S-Tab", "cycle source filter (logs)"),
        ("s", "shell into container (Container App detail/logs)"),
        ("x", "reveal / hide env var values (env vars)"),
        ("Ctrl-e / Ctrl-n", "edit / add env var (env vars)"),
        ("/", "search (logs)"),
        ("n / N", "next / prev match (logs)"),
        ("V", "visual-line select for yank (logs)"),
        ("l", "open logs (detail)"),
        (
            "Requests",
            "HTTP hits on the site's front end, not fn executions",
        ),
        (
            "⚠ fn apps",
            "event-triggered apps still count Always On / probe pings",
        ),
        (
            "Executions",
            "actual fn invocations, any trigger (App Insights, fn apps)",
        ),
    ],
);

const KV_ACCESS_LOG: Section = (
    "Key vault access log (l on a vault or item)",
    &[
        (
            "l",
            "access log — who accessed what, when (vault-wide or one item)",
        ),
        ("0 / 1 / 7", "window 1h / 1d / 7d"),
        ("t", "custom window (e.g. 12h, 30d, 6m, 1y)"),
        ("m", "hide your own accesses (your user / sign-in IP)"),
        ("Tab / S-Tab", "cycle operation filter (SecretGet, …)"),
        ("y", "yank row (incl. full managed-identity id)"),
    ],
);

const HEALTH_BADGE: Section = (
    "Health badge",
    &[
        ("window", "computed over a fixed 24h, not the chart range"),
        ("HEALTHY", "<1% 5xx and no error spikes"),
        ("DEGRADED", "sustained >1% 5xx, or a single-bin spike"),
        ("CRITICAL", "stopped, platform down, or >5% / sharp spike"),
        ("IDLE", "running but no traffic in the last 24h"),
        ("UNKNOWN", "no data / not loaded yet"),
        ("ERROR", "couldn't fetch the health metrics"),
        ("5xx", "had server errors in 24h (flag, not the verdict)"),
        (
            "◌ vs ●",
            "hollow = provisional (still loading metrics); solid = settled",
        ),
        (
            "refresh",
            "list self-updates on a timer (refresh_secs); r forces it",
        ),
        ("note", "verdict is worst-of all signals (pessimistic)"),
    ],
);

const APIM: Section = (
    "APIM (APIs/Routes)",
    &[
        ("Enter", "drill down: APIs > routes > policy"),
        ("y", "yank API / operation / policy"),
        ("o", "open in Azure Portal"),
        ("r", "refresh current panel"),
    ],
);

const APP_GATEWAY: Section = (
    "Application Gateway",
    &[
        ("Enter", "show backend pools and their members"),
        ("y", "yank gateway id / FQDN / IP / NIC id"),
        ("o", "open gateway in Azure Portal"),
        ("r", "refresh backend pools"),
    ],
);

const STORAGE: Section = (
    "Storage (blobs)",
    &[
        ("S", "enter storage mode"),
        (
            "Enter",
            "drill: accounts > overview > containers > blobs > preview",
        ),
        (
            "overview",
            "per-account stats (blobs/files/queues/tables, ~24h lag)",
        ),
        (
            "/",
            "filter accounts / containers / blobs by name (substring)",
        ),
        ("j/k", "scroll preview (detail)"),
        ("g/G", "preview top / bottom"),
        ("y", "yank account / container / blob / body"),
        ("o", "open account in Azure Portal"),
        ("r", "refresh current panel"),
    ],
);

const STORAGE_ACCESS_LOG: Section = (
    "Storage access log (l on an account or container)",
    &[
        (
            "l",
            "blob access log — who read/wrote what, when, how (OAuth / SAS / account key)",
        ),
        ("0 / 1 / 7", "window 1h / 1d / 7d"),
        ("t", "custom window (e.g. 12h, 30d, 6m, 1y)"),
        (
            "m",
            "hide your own accesses (your user / object id / sign-in IP)",
        ),
        ("Tab / S-Tab", "cycle operation filter (GetBlob, …)"),
        ("y", "yank row (incl. raw identity)"),
    ],
);

const REGISTRIES: Section = (
    "Container registries (ACR)",
    &[
        ("R", "enter registries mode"),
        ("Enter", "drill: registries > repositories > tags"),
        ("/", "filter registries / repos / tags by name (substring)"),
        (
            "0 / 1 / 7 / t",
            "PULLS column window: 1h / 1d / 7d (default) / custom, e.g. 30d",
        ),
        ("y", "yank registry id / repo name / pull ref"),
        ("o", "open registry in Azure Portal"),
        ("r", "refresh current panel"),
    ],
);

const ACR_ACCESS_LOG: Section = (
    "Registry access log (l on a registry or repository)",
    &[
        (
            "l",
            "access log — who pulled/pushed which image, when (registry-wide or one repo); pulls/pushes chart on top comes from Monitor metrics (always on)",
        ),
        ("0 / 1 / 7", "window 1h / 1d / 7d"),
        ("t", "custom window (e.g. 12h, 30d, 6m, 1y)"),
        (
            "m",
            "hide your own pulls (your user / object id / sign-in IP)",
        ),
        ("Tab / S-Tab", "cycle operation filter (Pull, Push, …)"),
        ("y", "yank row (incl. raw identity and full digest)"),
    ],
);

const COSMOS: Section = (
    "Cosmos DB (SQL/Core API)",
    &[
        ("Enter", "drill: accounts > databases > containers > items"),
        ("/", "filter accounts / databases / containers by name"),
        ("y", "yank account id / db name / container / item json"),
        ("o", "open account's Data Explorer in Azure Portal"),
        (
            "r",
            "refresh current panel (item preview costs RU — see title bar)",
        ),
    ],
);

const KEY_VAULTS: Section = (
    "Key Vaults (listing is metadata only)",
    &[
        ("Enter", "vaults: drill in to secrets / certificates"),
        ("Enter / x", "secrets: reveal selected value in a modal"),
        ("Tab / S-Tab", "toggle secrets ↔ certificates"),
        ("/", "filter vaults / items by name (substring)"),
        ("y", "yank vault id / item name · in modal: the value"),
        ("o", "open vault in Azure Portal"),
        ("r", "refresh current panel"),
    ],
);

const SERVICE_BUS: Section = (
    "Service Bus (control plane)",
    &[
        ("Enter", "drill: namespaces > queues/topics > subs"),
        ("Tab / S-Tab", "toggle queues ↔ topics"),
        ("DLQ", "dead-letter depth, red when non-zero"),
        ("/", "filter by name (substring)"),
        ("y", "yank id / entity / subscription"),
        ("o", "open namespace in Azure Portal"),
        ("r", "refresh current panel"),
    ],
);

const AZURE_SQL: Section = (
    "Azure SQL (pools + databases)",
    &[
        ("Enter", "open utilization sparklines for the pool/database"),
        ("0 / 1 / 7", "chart window: 1h / 1d / 7d"),
        ("/", "filter pools & databases by name / server"),
        ("y", "yank resource id"),
        ("o", "open pool / database in Azure Portal"),
        ("r", "refresh"),
    ],
);

const SQL_AUDIT: Section = (
    "SQL audit log (l on a pool / database)",
    &[
        (
            "l",
            "principal roll-up — last seen / event counts per login",
        ),
        (
            "Enter",
            "drill: principal > events > full event (statement + error)",
        ),
        (
            "0 / 1 / 7 / 3 / 9",
            "window 1h / 1d / 7d / 30d / 1y (default 30d)",
        ),
        ("t", "custom window (e.g. 12h, 30d, 6m, 1y)"),
        ("/", "filter principals by name (raw or resolved)"),
        ("Tab / S-Tab", "cycle action filter (batch, login, tx-…)"),
        ("e", "errors only (events — refetches server-side)"),
        ("bottom j / G", "fetch rows older than the page (events)"),
        ("y", "yank roll-up row / full statement"),
        (
            "note",
            "needs auditing → Log Analytics; pools audit server-wide",
        ),
        (
            "⚠ users",
            "db-scoped roll-up lists users via live T-SQL (silent at bottom)",
        ),
    ],
);

const SQL_SESSIONS: Section = (
    "SQL open sessions (u on a pool / database) ⚠ live T-SQL",
    &[
        ("u", "who's connected now — since when, idle how long"),
        ("query", "read-only SELECT on sys.dm_exec_sessions over TDS"),
        ("y", "yank session row (login, timings, client)"),
        (
            "off-switch",
            "sql_live_queries = false in config.toml disables all live T-SQL",
        ),
        (
            "note",
            "needs the firewall to admit you + a db user mapping",
        ),
    ],
);

/// Action-code legend for the SQL audit views (codes per
/// `sys.dm_audit_actions`) — SQL mode only, it's noise everywhere else.
const SQL_AUDIT_CODES: Section = (
    "SQL audit action codes",
    &[
        ("BCM", "batch completed — an actual query"),
        ("RCM", "rpc completed — a stored-proc / parameterized call"),
        ("DBAS / DBAF", "database authentication succeeded / failed"),
        ("TRBC", "transaction begin completed"),
        ("TRCC / TRRC", "transaction commit / rollback completed"),
        (
            "AUSC",
            "audit session changed (auditing itself (re)started)",
        ),
    ],
);

const GLOBAL: Section = (
    "Global",
    &[
        ("r", "refresh"),
        ("y", "yank to clipboard"),
        ("o", "open in Azure Portal"),
        ("?", "toggle help"),
        ("q", "quit"),
    ],
);

const LOGIC_APPS: Section = (
    "Logic apps (read-only)",
    &[
        ("Enter", "drill: logic apps > runs > actions > content"),
        ("t", "trigger firing history (runs view)"),
        ("/", "filter logic apps by name (substring)"),
        ("y", "yank workflow id / run id / action / content"),
        ("o", "open workflow in Azure Portal"),
        (
            "r",
            "refresh current panel (content links expire — refresh the list first)",
        ),
    ],
);

/// How to get everywhere else — always shown, since the scoped help hides the
/// other categories' sections.
const GO_TO: Section = (
    "Go to (command palette :)",
    &[
        (":", "open command palette (Tab cycles matches)"),
        (":apis", "API resources (the start view)"),
        (":storage", "storage / blobs (also: S)"),
        (":registries / :acr", "container registries (also: R)"),
        (":cosmos", "cosmos db"),
        (":keyvaults / :kv", "key vaults"),
        (":servicebus / :sb", "service bus"),
        (":sql / :sqldb", "azure sql"),
        (":logicapps / :logic", "logic apps (run & trigger history)"),
        (":subscriptions", "subscription picker (also: s)"),
        (":refresh", "force-refresh current view"),
        (":quit / :q", "quit"),
    ],
);

/// The sections relevant to where help was opened from: navigation first,
/// then the origin category's own sections, then the global keys and the
/// "Go to" palette list that replaces the hidden categories.
fn sections_for(origin: Option<View>) -> Vec<Section> {
    let mut out = vec![NAVIGATION];
    match origin.and_then(Category::of) {
        Some(Category::Apis) => {
            out.extend([API_RESOURCES, DETAIL_LOGS, HEALTH_BADGE, APIM, APP_GATEWAY])
        }
        Some(Category::Storage) => out.extend([STORAGE, STORAGE_ACCESS_LOG]),
        Some(Category::Registries) => out.extend([REGISTRIES, ACR_ACCESS_LOG]),
        Some(Category::Cosmos) => out.push(COSMOS),
        Some(Category::KeyVaults) => out.extend([KEY_VAULTS, KV_ACCESS_LOG]),
        Some(Category::ServiceBus) => out.push(SERVICE_BUS),
        Some(Category::Sql) => out.extend([AZURE_SQL, SQL_AUDIT, SQL_SESSIONS, SQL_AUDIT_CODES]),
        Some(Category::LogicApps) => out.push(LOGIC_APPS),
        // Subscriptions picker / unknown origin: just the shared sections.
        None => {}
    }
    out.extend([GLOBAL, GO_TO]);
    out
}

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let popup = centered_rect(74, 80, area);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(
            " help ",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(theme.bg).fg(theme.fg));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    // Two-column layout: split sections roughly in half so the popup uses
    // both columns even as new sections are added.
    let cols =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(inner);

    // Scope to the origin view — `?` pushed it onto `view_stack`.
    let sections = sections_for(state.view_stack.last().copied());

    let mid = sections.len().div_ceil(2);
    let left_lines = lines_for(&sections[..mid], theme);
    let right_lines = lines_for(&sections[mid..], theme);

    frame.render_widget(Paragraph::new(left_lines), cols[0]);
    frame.render_widget(Paragraph::new(right_lines), cols[1]);

    // Footer hint inside the popup.
    if inner.height >= 2 {
        let hint_area = Rect {
            x: inner.x,
            y: inner.y + inner.height - 1,
            width: inner.width,
            height: 1,
        };
        let p = Paragraph::new(Line::from(Span::styled(
            "press ? or Esc to dismiss",
            Style::default().fg(theme.muted),
        )))
        .alignment(ratatui::layout::Alignment::Center);
        frame.render_widget(p, hint_area);
    }
}

fn lines_for(sections: &[Section], theme: &Theme) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for (i, (heading, entries)) in sections.iter().enumerate() {
        if i > 0 {
            out.push(Line::from(""));
        }
        out.push(Line::from(Span::styled(
            format!(" {} ", heading),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )));
        for (key, desc) in *entries {
            out.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    format!("{:<18}", key),
                    Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
                ),
                Span::styled(desc.to_string(), Style::default().fg(theme.muted)),
            ]));
        }
    }
    out
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let h_layout = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .split(area);
    let v_layout = Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(h_layout[1]);
    v_layout[1]
}

pub fn handle(action: Action, state: &mut AppState) -> bool {
    let _ = action;
    let target = state.view_stack.pop().unwrap_or(View::List);
    state.view = target;
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn render_from(origin: Option<View>) -> String {
        let theme = Theme::catppuccin_mocha();
        let mut state = AppState::new(Config::default());
        if let Some(v) = origin {
            state.view_stack.push(v);
        }
        let mut term = Terminal::new(TestBackend::new(120, 90)).unwrap();
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        format!("{:?}", term.backend().buffer())
    }

    #[test]
    fn help_is_scoped_to_the_origin_view() {
        // From a SQL view: SQL sections (incl. the action-code legend), no
        // cosmos / storage / service-bus noise.
        let s = render_from(Some(View::SqlAuditEvents));
        assert!(s.contains("Azure SQL"));
        assert!(s.contains("SQL audit log"));
        assert!(s.contains("open sessions"));
        assert!(s.contains("audit action codes"));
        assert!(s.contains("TRBC"));
        assert!(!s.contains("Cosmos DB"), "cosmos is noise in SQL mode");
        assert!(!s.contains("Service Bus (control plane)"));
        assert!(!s.contains("Storage (blobs)"));

        // From cosmos: the reverse.
        let s = render_from(Some(View::CosmosDatabases));
        assert!(s.contains("Cosmos DB"));
        assert!(!s.contains("SQL audit log"));
        assert!(!s.contains("audit action codes"));

        // Key vault views bring the vault + access-log pair.
        let s = render_from(Some(View::KeyVaultAccessLogs));
        assert!(s.contains("Key Vaults"));
        assert!(s.contains("Key vault access log"));
        assert!(!s.contains("Application Gateway"));
    }

    #[test]
    fn shared_sections_and_go_to_always_render() {
        for origin in [
            None,
            Some(View::List),
            Some(View::SqlSessions),
            Some(View::StorageBlobs),
        ] {
            let s = render_from(origin);
            assert!(s.contains("Navigation"), "origin {origin:?}");
            assert!(s.contains("Global"), "origin {origin:?}");
            assert!(s.contains("Go to (command palette"), "origin {origin:?}");
            assert!(s.contains(":sql / :sqldb"), "origin {origin:?}");
            assert!(s.contains(":cosmos"), "origin {origin:?}");
        }
        // Apis origin carries its full section family.
        let s = render_from(Some(View::Detail));
        assert!(s.contains("API resources"));
        assert!(s.contains("Detail / Logs"));
        assert!(s.contains("HTTP hits"));
        assert!(s.contains("Health badge"));
        assert!(s.contains("APIM"));
        assert!(s.contains("Application Gateway"));
    }

    #[test]
    fn renders_without_panic() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(100, 60);
        let mut term = Terminal::new(backend).unwrap();
        let state = AppState::new(Config::default());
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.to_lowercase().contains("help"));
        assert!(s.contains("Navigation"));
        assert!(s.contains("Global"));
    }

    #[test]
    fn handle_dismisses_to_previous_view() {
        let mut state = AppState::new(Config::default());
        state.view = View::Help;
        state.view_stack.push(View::Detail);
        assert!(handle(Action::Back, &mut state));
        assert_eq!(state.view, View::Detail);
        assert!(state.view_stack.is_empty());
    }

    #[test]
    fn handle_falls_back_to_list() {
        let mut state = AppState::new(Config::default());
        state.view = View::Help;
        assert!(state.view_stack.is_empty());
        assert!(handle(Action::Help, &mut state));
        assert_eq!(state.view, View::List);
        assert!(state.view_stack.is_empty());
    }

    #[test]
    fn handle_does_not_bounce_back_into_help() {
        // Simulates: start in List -> ? to Help -> key to dismiss.
        // After dismiss, the stack must not contain Help so a subsequent
        // Esc/q from List does not warp the user back into Help.
        let mut state = AppState::new(Config::default());
        state.view = View::Help;
        state.view_stack.push(View::List);
        assert!(handle(Action::Back, &mut state));
        assert_eq!(state.view, View::List);
        assert!(!state.view_stack.contains(&View::Help));
        assert!(state.view_stack.is_empty());
    }

    #[test]
    fn renders_in_tiny_area_without_panic() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(20, 6);
        let mut term = Terminal::new(backend).unwrap();
        let state = AppState::new(Config::default());
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
    }
}
