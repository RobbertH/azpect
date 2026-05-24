//! Application Gateway backends panel. Drill-in from the resource list: Enter
//! on a row whose kind is `AppGateway` lands here instead of the generic
//! Detail view, so the operator can see (at a glance) what targets the gateway
//! is wired to.
//!
//! The view renders pools and their members as one flat list of rows. Each
//! pool contributes a header row (`▼ {pool}`) followed by one row per address
//! / NIC reference, indented. An empty pool gets a single `(empty pool)`
//! placeholder so the user can still see that the pool exists.
//!
//! `H` toggles into a second mode showing live backend *health*: the same
//! pool grouping, but each row is a probed server with a colored status badge
//! (green up / red down / amber partial) and its probe-log reason. The health
//! data is fetched lazily (it's an async ARM operation) and cached alongside
//! the pools in [`AppState::appgw`](crate::ui::state::AppGatewayBackendsCache).

#![allow(dead_code, unused_variables)]

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::azure::appgw_backends::BackendPool;
use crate::azure::appgw_health::{summarize, HealthStatus, PoolHealth};
use crate::azure::resources::ResourceKind;
use crate::ui::events::Action;
use crate::ui::state::AppState;
use crate::ui::theme::Theme;

const FOOTER_CONFIG: &str =
    "j/k move  H health  r refresh  y yank  o portal  Esc back  ? help  q quit";
const FOOTER_HEALTH: &str =
    "j/k move  H config  r re-probe  y yank  o portal  Esc back  ? help  q quit";
const HALF_PAGE: usize = 10;

/// Resource id of the Application Gateway we're drilling into. Resolved off
/// the currently selected resource — kept here (not in state) because the
/// cursor in the list view always points at the parent AppGw while this view
/// is on the stack.
pub fn gateway_id(state: &AppState) -> Option<String> {
    state
        .selected_resource()
        .filter(|r| r.kind == ResourceKind::AppGateway)
        .map(|r| r.id.clone())
}

/// One renderable row, post-flatten. Kept as a tagged enum (rather than a
/// pre-formatted `String`) so the renderer can style the role differently
/// (FQDN / IP / NIC) and `yank_text` can recover the underlying identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Row {
    PoolHeader {
        pool_index: usize,
        name: String,
    },
    EmptyMarker {
        pool_index: usize,
    },
    Address {
        pool_index: usize,
        kind: AddressKind,
        value: String,
    },
    NicRef {
        pool_index: usize,
        nic_name: String,
        config_name: String,
        full_id: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AddressKind {
    Fqdn,
    Ip,
}

/// Flatten `pools` into the row list the view renders. Empty pools still get
/// a row so the user can tell the pool exists but has no targets.
pub(crate) fn flatten(pools: &[BackendPool]) -> Vec<Row> {
    let mut out = Vec::new();
    for (pi, pool) in pools.iter().enumerate() {
        out.push(Row::PoolHeader {
            pool_index: pi,
            name: pool.name.clone(),
        });
        let mut any = false;
        for addr in &pool.addresses {
            // ARM almost always sets exactly one of fqdn/ip_address per entry.
            // When both are set (rare), we emit two rows so neither identity is
            // lost on yank. When neither is set (also rare), the entry is
            // skipped — a totally empty address row would just confuse the
            // operator.
            if let Some(fqdn) = addr.fqdn.as_deref() {
                out.push(Row::Address {
                    pool_index: pi,
                    kind: AddressKind::Fqdn,
                    value: fqdn.to_string(),
                });
                any = true;
            }
            if let Some(ip) = addr.ip_address.as_deref() {
                out.push(Row::Address {
                    pool_index: pi,
                    kind: AddressKind::Ip,
                    value: ip.to_string(),
                });
                any = true;
            }
        }
        for nic in &pool.nic_ip_config_refs {
            out.push(Row::NicRef {
                pool_index: pi,
                nic_name: nic.nic_name.clone(),
                config_name: nic.config_name.clone(),
                full_id: nic.full_id.clone(),
            });
            any = true;
        }
        if !any {
            out.push(Row::EmptyMarker { pool_index: pi });
        }
    }
    out
}

/// One renderable row in *health* mode. Mirrors the config-mode [`Row`] split
/// (header vs member) so navigation/scroll logic can be shared.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HealthRow {
    PoolHeader {
        name: String,
        healthy: usize,
        unhealthy: usize,
        other: usize,
    },
    /// Pool exists but the gateway reported no servers to probe.
    EmptyMarker,
    Server {
        address: String,
        health: HealthStatus,
        http_setting: Option<String>,
        probe_log: Option<String>,
    },
}

/// Flatten per-pool health into the row list the health view renders. A pool
/// with no probed servers still gets a header + `(no servers reported)` marker.
pub(crate) fn flatten_health(pools: &[PoolHealth]) -> Vec<HealthRow> {
    let mut out = Vec::new();
    for pool in pools {
        let c = summarize(std::slice::from_ref(pool));
        out.push(HealthRow::PoolHeader {
            name: pool.name.clone(),
            healthy: c.healthy,
            unhealthy: c.unhealthy,
            other: c.other,
        });
        if pool.servers.is_empty() {
            out.push(HealthRow::EmptyMarker);
        } else {
            for s in &pool.servers {
                out.push(HealthRow::Server {
                    address: s.address.clone(),
                    health: s.health,
                    http_setting: s.http_setting.clone(),
                    probe_log: s.probe_log.clone(),
                });
            }
        }
    }
    out
}

/// Map a probe verdict to its theme color so the badge telegraphs status at a
/// glance (green up, red down, amber partial, …).
fn health_color(theme: &Theme, status: HealthStatus) -> Color {
    match status {
        HealthStatus::Healthy => theme.healthy,
        HealthStatus::Unhealthy => theme.critical,
        HealthStatus::Partial => theme.degraded,
        HealthStatus::Draining => theme.idle,
        HealthStatus::Unknown => theme.unknown,
    }
}

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    if state.appgw.show_health {
        render_health(frame, area, state, theme);
        return;
    }
    // Global breadcrumb (rendered by app::dispatch_view) replaces the old
    // in-view " backends {gateway}" header strip — body + footer only here.
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(
            " backend pools ",
            Style::default().fg(theme.fg),
        ));
    let inner = block.inner(chunks[0]);
    frame.render_widget(block, chunks[0]);

    let Some(gw_id) = gateway_id(state) else {
        let p = Paragraph::new(Line::from(Span::styled(
            "no Application Gateway selected.",
            Style::default().fg(theme.muted),
        )));
        frame.render_widget(p, inner);
        render_footer(frame, chunks[1], theme, FOOTER_CONFIG);
        return;
    };

    if let Some(err) = state.appgw.pools_error.get(&gw_id) {
        let p = Paragraph::new(Line::from(Span::styled(
            format!("error: {err}"),
            Style::default().fg(theme.critical),
        )));
        frame.render_widget(p, inner);
        render_footer(frame, chunks[1], theme, FOOTER_CONFIG);
        return;
    }

    let pools = state.appgw.pools.get(&gw_id);
    let loading = state.appgw.pools_pending.contains(&gw_id);
    match pools {
        None if loading => {
            let p = Paragraph::new(Line::from(Span::styled(
                "loading backend pools …",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, inner);
        }
        None => {
            let p = Paragraph::new(Line::from(Span::styled(
                "press r to load backend pools.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, inner);
        }
        Some(rows) if rows.is_empty() => {
            let p = Paragraph::new(Line::from(Span::styled(
                "this Application Gateway has no backend pools configured.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, inner);
        }
        Some(rows) => {
            let flat = flatten(rows);
            if flat.is_empty() {
                // Unreachable given the pool list isn't empty, but guard anyway.
                return;
            }
            let cursor = state.appgw.cursor.min(flat.len() - 1);
            let visible = inner.height as usize;
            let scroll = scroll_for(cursor, flat.len(), visible);

            let lines: Vec<Line> = flat
                .iter()
                .enumerate()
                .skip(scroll)
                .take(visible)
                .map(|(i, row)| render_row(row, i == cursor, theme))
                .collect();
            frame.render_widget(Paragraph::new(lines), inner);
        }
    }

    render_footer(frame, chunks[1], theme, FOOTER_CONFIG);
}

/// Health-mode render: live per-server probe verdicts grouped by pool, with a
/// summary in the title bar. Mirrors the config render's loading / error /
/// empty handling but reads from the `health*` caches.
fn render_health(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);

    // Title carries the at-a-glance summary; it turns red the moment any server
    // is down so the operator doesn't have to scan the list to know.
    let (title, title_color) =
        match gateway_id(state).and_then(|id| state.appgw.health.get(&id).map(|h| summarize(h))) {
            Some(c) if c.total() > 0 => {
                let mut s = format!(" backend health  {} up", c.healthy);
                if c.unhealthy > 0 {
                    s.push_str(&format!(" / {} down", c.unhealthy));
                }
                if c.other > 0 {
                    s.push_str(&format!(" / {} other", c.other));
                }
                s.push(' ');
                let color = if c.unhealthy > 0 {
                    theme.critical
                } else {
                    theme.fg
                };
                (s, color)
            }
            _ => (" backend health ".to_string(), theme.fg),
        };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(title, Style::default().fg(title_color)));
    let inner = block.inner(chunks[0]);
    frame.render_widget(block, chunks[0]);

    let Some(gw_id) = gateway_id(state) else {
        let p = Paragraph::new(Line::from(Span::styled(
            "no Application Gateway selected.",
            Style::default().fg(theme.muted),
        )));
        frame.render_widget(p, inner);
        render_footer(frame, chunks[1], theme, FOOTER_HEALTH);
        return;
    };

    if let Some(err) = state.appgw.health_error.get(&gw_id) {
        let p = Paragraph::new(Line::from(Span::styled(
            format!("error: {err}"),
            Style::default().fg(theme.critical),
        )));
        frame.render_widget(p, inner);
        render_footer(frame, chunks[1], theme, FOOTER_HEALTH);
        return;
    }

    let health = state.appgw.health.get(&gw_id);
    let loading = state.appgw.health_pending.contains(&gw_id);
    match health {
        None if loading => {
            let p = Paragraph::new(Line::from(Span::styled(
                "probing backend health … (async — may take a few seconds)",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, inner);
        }
        None => {
            let p = Paragraph::new(Line::from(Span::styled(
                "press r to probe backend health.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, inner);
        }
        Some(pools) if pools.is_empty() => {
            let p = Paragraph::new(Line::from(Span::styled(
                "this Application Gateway reported no backend health.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, inner);
        }
        Some(pools) => {
            let flat = flatten_health(pools);
            if flat.is_empty() {
                return;
            }
            let cursor = state.appgw.health_cursor.min(flat.len() - 1);
            let visible = inner.height as usize;
            let scroll = scroll_for(cursor, flat.len(), visible);

            let lines: Vec<Line> = flat
                .iter()
                .enumerate()
                .skip(scroll)
                .take(visible)
                .map(|(i, row)| render_health_row(row, i == cursor, theme))
                .collect();
            frame.render_widget(Paragraph::new(lines), inner);
        }
    }

    render_footer(frame, chunks[1], theme, FOOTER_HEALTH);
}

fn render_health_row(row: &HealthRow, selected: bool, theme: &Theme) -> Line<'static> {
    let cursor_marker = if selected { "▍ " } else { "  " };
    let spans = match row {
        HealthRow::PoolHeader {
            name,
            healthy,
            unhealthy,
            other,
        } => {
            let mut spans = vec![
                Span::raw(cursor_marker),
                Span::styled("▼ ".to_string(), Style::default().fg(theme.accent)),
                Span::styled(
                    name.clone(),
                    Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(format!("{healthy} up"), Style::default().fg(theme.healthy)),
            ];
            if *unhealthy > 0 {
                spans.push(Span::styled(
                    " / ".to_string(),
                    Style::default().fg(theme.muted),
                ));
                spans.push(Span::styled(
                    format!("{unhealthy} down"),
                    Style::default().fg(theme.critical),
                ));
            }
            if *other > 0 {
                spans.push(Span::styled(
                    " / ".to_string(),
                    Style::default().fg(theme.muted),
                ));
                spans.push(Span::styled(
                    format!("{other} other"),
                    Style::default().fg(theme.degraded),
                ));
            }
            spans
        }
        HealthRow::EmptyMarker => vec![
            Span::raw(cursor_marker),
            Span::raw("    "),
            Span::styled(
                "(no servers reported)".to_string(),
                Style::default().fg(theme.muted),
            ),
        ],
        HealthRow::Server {
            address,
            health,
            http_setting,
            probe_log,
        } => {
            let color = health_color(theme, *health);
            let mut spans = vec![
                Span::raw(cursor_marker),
                Span::raw("    "),
                Span::styled("● ".to_string(), Style::default().fg(color)),
                Span::styled(
                    format!("{:<9}", health.as_str()),
                    Style::default().fg(color),
                ),
                Span::raw(" "),
                Span::styled(address.clone(), Style::default().fg(theme.fg)),
            ];
            if let Some(setting) = http_setting {
                spans.push(Span::styled(
                    format!("  ({setting})"),
                    Style::default().fg(theme.muted),
                ));
            }
            // Surface the probe reason inline for anything that isn't cleanly
            // healthy — that's where the operator actually needs the detail.
            // (It clips at the panel edge; the full text is available via yank.)
            if *health != HealthStatus::Healthy {
                if let Some(log) = probe_log {
                    spans.push(Span::styled(
                        format!("  — {log}"),
                        Style::default().fg(theme.muted),
                    ));
                }
            }
            spans
        }
    };
    if selected {
        Line::from(spans).style(theme.selection())
    } else {
        Line::from(spans)
    }
}

fn render_row(row: &Row, selected: bool, theme: &Theme) -> Line<'static> {
    let cursor_marker = if selected { "▍ " } else { "  " };
    let spans = match row {
        Row::PoolHeader { name, .. } => vec![
            Span::raw(cursor_marker),
            Span::styled("▼ ".to_string(), Style::default().fg(theme.accent)),
            Span::styled(
                name.clone(),
                Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
            ),
        ],
        Row::EmptyMarker { .. } => vec![
            Span::raw(cursor_marker),
            Span::raw("    "),
            Span::styled("(empty pool)".to_string(), Style::default().fg(theme.muted)),
        ],
        Row::Address { kind, value, .. } => {
            let tag = match kind {
                AddressKind::Fqdn => "FQDN",
                AddressKind::Ip => "IP  ",
            };
            vec![
                Span::raw(cursor_marker),
                Span::raw("    "),
                Span::styled(value.clone(), Style::default().fg(theme.fg)),
                Span::raw("  "),
                Span::styled(
                    format!("({})", tag.trim()),
                    Style::default().fg(theme.muted),
                ),
            ]
        }
        Row::NicRef {
            nic_name,
            config_name,
            ..
        } => vec![
            Span::raw(cursor_marker),
            Span::raw("    "),
            Span::styled(nic_name.clone(), Style::default().fg(theme.fg)),
            Span::styled(" / ".to_string(), Style::default().fg(theme.muted)),
            Span::styled(config_name.clone(), Style::default().fg(theme.fg)),
            Span::raw("  "),
            Span::styled("(NIC)".to_string(), Style::default().fg(theme.muted)),
        ],
    };
    if selected {
        Line::from(spans).style(theme.selection())
    } else {
        Line::from(spans)
    }
}

fn render_footer(frame: &mut Frame, area: Rect, theme: &Theme, hint: &str) {
    let p = Paragraph::new(Line::from(Span::styled(
        hint.to_string(),
        Style::default().fg(theme.muted),
    )));
    frame.render_widget(p, area);
}

pub fn handle(action: Action, state: &mut AppState) -> bool {
    let Some(gw_id) = gateway_id(state) else {
        return false;
    };

    // `H` flips between the config listing and live health. Valid in either
    // mode regardless of what's loaded; `after_action` kicks off the health
    // probe once we've switched into it. Reset the health cursor so we don't
    // land on a stale row from a previous gateway.
    if let Action::ToggleBackendHealth = action {
        state.appgw.show_health = !state.appgw.show_health;
        state.appgw.health_cursor = 0;
        return true;
    }

    let show_health = state.appgw.show_health;
    let len = if show_health {
        state
            .appgw
            .health
            .get(&gw_id)
            .map(|h| flatten_health(h).len())
            .unwrap_or(0)
    } else {
        state
            .appgw
            .pools
            .get(&gw_id)
            .map(|p| flatten(p).len())
            .unwrap_or(0)
    };
    let cursor = if show_health {
        &mut state.appgw.health_cursor
    } else {
        &mut state.appgw.cursor
    };

    match action {
        Action::MoveDown => {
            if len > 0 {
                *cursor = (*cursor + 1).min(len - 1);
            }
            true
        }
        Action::MoveUp => {
            *cursor = cursor.saturating_sub(1);
            true
        }
        Action::HalfPageDown => {
            if len > 0 {
                *cursor = (*cursor + HALF_PAGE).min(len - 1);
            }
            true
        }
        Action::HalfPageUp => {
            *cursor = cursor.saturating_sub(HALF_PAGE);
            true
        }
        Action::GotoTop => {
            *cursor = 0;
            true
        }
        Action::GotoBottom => {
            if len > 0 {
                *cursor = len - 1;
            }
            true
        }
        // OpenSelected is a no-op: there's no deeper drill-in level. Swallow
        // it so Enter doesn't bubble up to global handlers and accidentally
        // do something surprising.
        Action::OpenSelected => true,
        _ => false,
    }
}

/// Resolve what `y` should copy when this view is up. Header (no selection or
/// cursor on a pool header) yields the gateway resource id; otherwise the
/// selected member's natural identity (FQDN / IP / NIC id). In health mode the
/// cursor's server yields its probe log (the diagnostic you'd paste into a
/// ticket) when present, else the probed address.
pub fn yank_text(state: &AppState) -> Option<String> {
    let gw_id = gateway_id(state)?;

    if state.appgw.show_health {
        let health = state.appgw.health.get(&gw_id)?;
        let flat = flatten_health(health);
        if flat.is_empty() {
            return Some(gw_id);
        }
        let cursor = state.appgw.health_cursor.min(flat.len() - 1);
        return match &flat[cursor] {
            HealthRow::PoolHeader { name, .. } => Some(format!("{gw_id} :: {name}")),
            HealthRow::EmptyMarker => Some(gw_id),
            HealthRow::Server {
                address, probe_log, ..
            } => Some(probe_log.clone().unwrap_or_else(|| address.clone())),
        };
    }

    let pools = state.appgw.pools.get(&gw_id)?;
    let flat = flatten(pools);
    if flat.is_empty() {
        return Some(gw_id);
    }
    let cursor = state.appgw.cursor.min(flat.len() - 1);
    match &flat[cursor] {
        Row::PoolHeader { name, .. } => Some(format!("{gw_id} :: {name}")),
        Row::EmptyMarker { .. } => Some(gw_id),
        Row::Address { value, .. } => Some(value.clone()),
        Row::NicRef { full_id, .. } => Some(full_id.clone()),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::appgw_backends::{BackendAddress, BackendPool, NicIpConfigRef};
    use crate::azure::resources::{Resource, ResourceKind};
    use crate::config::Config;
    use crate::ui::state::View;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn fixture() -> AppState {
        let mut state = AppState::new(Config::default());
        state.resources = vec![Resource {
            id: "/subscriptions/s/resourceGroups/rg/providers/Microsoft.Network/applicationGateways/gw".into(),
            name: "my-appgw".into(),
            kind: ResourceKind::AppGateway,
            location: "westeurope".into(),
            resource_group: "rg".into(),
            subscription_id: "s".into(),
            state: Some("Running".into()),
            created_at: None,
            modified_at: None,
        }];
        state.list_cursor = 0;
        state.view = View::AppGatewayBackends;
        state
    }

    fn sample_pools() -> Vec<BackendPool> {
        vec![
            BackendPool {
                name: "pool-a".into(),
                addresses: vec![
                    BackendAddress {
                        fqdn: Some("api.example.com".into()),
                        ip_address: None,
                    },
                    BackendAddress {
                        fqdn: None,
                        ip_address: Some("10.0.1.4".into()),
                    },
                ],
                nic_ip_config_refs: vec![NicIpConfigRef {
                    nic_name: "nic-web-01".into(),
                    config_name: "ipconfig1".into(),
                    full_id:
                        "/subscriptions/s/resourceGroups/rg/providers/Microsoft.Network/networkInterfaces/nic-web-01/ipConfigurations/ipconfig1"
                            .into(),
                }],
            },
            BackendPool {
                name: "pool-b".into(),
                addresses: vec![],
                nic_ip_config_refs: vec![],
            },
        ]
    }

    #[test]
    fn flatten_emits_header_then_members_with_empty_marker() {
        let rows = flatten(&sample_pools());
        // pool-a: 1 header + 1 fqdn + 1 ip + 1 nic = 4
        // pool-b: 1 header + 1 empty marker = 2
        assert_eq!(rows.len(), 6);
        assert!(matches!(rows[0], Row::PoolHeader { .. }));
        assert!(matches!(
            rows[1],
            Row::Address {
                kind: AddressKind::Fqdn,
                ..
            }
        ));
        assert!(matches!(
            rows[2],
            Row::Address {
                kind: AddressKind::Ip,
                ..
            }
        ));
        assert!(matches!(rows[3], Row::NicRef { .. }));
        assert!(matches!(rows[4], Row::PoolHeader { .. }));
        assert!(matches!(rows[5], Row::EmptyMarker { .. }));
    }

    #[test]
    fn renders_pools_and_members_without_panic() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(100, 16);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        let gw_id = gateway_id(&state).unwrap();
        state.appgw.pools.insert(gw_id, sample_pools());
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("pool-a"));
        assert!(buf.contains("api.example.com"));
        assert!(buf.contains("10.0.1.4"));
        assert!(buf.contains("nic-web-01"));
        assert!(buf.contains("pool-b"));
        assert!(buf.contains("empty pool"));
    }

    #[test]
    fn renders_loading_state() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(80, 8);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state
            .appgw
            .pools_pending
            .insert(gateway_id(&state).unwrap());
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("loading backend pools"));
    }

    #[test]
    fn renders_error_state() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(80, 8);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state
            .appgw
            .pools_error
            .insert(gateway_id(&state).unwrap(), "boom".into());
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("error: boom"));
    }

    #[test]
    fn navigation_clamps_to_flat_row_count() {
        let mut state = fixture();
        let gw_id = gateway_id(&state).unwrap();
        state.appgw.pools.insert(gw_id, sample_pools());

        // 6 rows total → max cursor is 5.
        for _ in 0..20 {
            handle(Action::MoveDown, &mut state);
        }
        assert_eq!(state.appgw.cursor, 5);
        handle(Action::GotoTop, &mut state);
        assert_eq!(state.appgw.cursor, 0);
        handle(Action::GotoBottom, &mut state);
        assert_eq!(state.appgw.cursor, 5);
    }

    #[test]
    fn yank_returns_member_identity_when_cursor_on_member() {
        let mut state = fixture();
        let gw_id = gateway_id(&state).unwrap();
        state.appgw.pools.insert(gw_id, sample_pools());

        // Row 0 is pool header.
        state.appgw.cursor = 0;
        assert!(yank_text(&state).unwrap().contains("pool-a"));

        // Row 1 is the FQDN.
        state.appgw.cursor = 1;
        assert_eq!(yank_text(&state).as_deref(), Some("api.example.com"));

        // Row 2 is the IP.
        state.appgw.cursor = 2;
        assert_eq!(yank_text(&state).as_deref(), Some("10.0.1.4"));

        // Row 3 is the NIC ref.
        state.appgw.cursor = 3;
        let yanked = yank_text(&state).unwrap();
        assert!(yanked.contains("/networkInterfaces/nic-web-01/"));
    }

    #[test]
    fn open_selected_is_swallowed_without_view_change() {
        let mut state = fixture();
        let gw_id = gateway_id(&state).unwrap();
        state.appgw.pools.insert(gw_id, sample_pools());
        assert!(handle(Action::OpenSelected, &mut state));
        assert_eq!(state.view, View::AppGatewayBackends);
    }

    fn sample_health() -> Vec<PoolHealth> {
        use crate::azure::appgw_health::ServerHealth;
        vec![
            PoolHealth {
                name: "web-pool".into(),
                servers: vec![
                    ServerHealth {
                        address: "10.0.1.4".into(),
                        health: HealthStatus::Healthy,
                        http_setting: Some("https-setting".into()),
                        probe_log: Some("Success. Received 200".into()),
                    },
                    ServerHealth {
                        address: "10.0.1.5".into(),
                        health: HealthStatus::Unhealthy,
                        http_setting: Some("https-setting".into()),
                        probe_log: Some("Backend server timed out".into()),
                    },
                ],
            },
            PoolHealth {
                name: "idle-pool".into(),
                servers: vec![],
            },
        ]
    }

    #[test]
    fn flatten_health_emits_header_servers_and_empty_marker() {
        let rows = flatten_health(&sample_health());
        // web-pool: 1 header + 2 servers = 3; idle-pool: 1 header + 1 marker = 2
        assert_eq!(rows.len(), 5);
        assert!(matches!(rows[0], HealthRow::PoolHeader { .. }));
        assert!(matches!(
            rows[1],
            HealthRow::Server {
                health: HealthStatus::Healthy,
                ..
            }
        ));
        assert!(matches!(
            rows[2],
            HealthRow::Server {
                health: HealthStatus::Unhealthy,
                ..
            }
        ));
        assert!(matches!(rows[3], HealthRow::PoolHeader { .. }));
        assert!(matches!(rows[4], HealthRow::EmptyMarker));
    }

    #[test]
    fn toggle_switches_mode_and_resets_health_cursor() {
        let mut state = fixture();
        assert!(!state.appgw.show_health);
        state.appgw.health_cursor = 7;
        assert!(handle(Action::ToggleBackendHealth, &mut state));
        assert!(state.appgw.show_health);
        assert_eq!(state.appgw.health_cursor, 0);
        // Toggle back to config mode.
        assert!(handle(Action::ToggleBackendHealth, &mut state));
        assert!(!state.appgw.show_health);
    }

    #[test]
    fn renders_health_with_status_and_summary() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(120, 16);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        let gw_id = gateway_id(&state).unwrap();
        state.appgw.show_health = true;
        state.appgw.health.insert(gw_id, sample_health());
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("backend health"));
        assert!(buf.contains("web-pool"));
        assert!(buf.contains("Healthy"));
        assert!(buf.contains("Unhealthy"));
        assert!(buf.contains("10.0.1.4"));
        // Probe reason shows inline for the unhealthy server.
        assert!(buf.contains("timed out"));
        // Summary in the title bar.
        assert!(buf.contains("1 up"));
        assert!(buf.contains("1 down"));
    }

    #[test]
    fn renders_health_loading_state() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(80, 8);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.appgw.show_health = true;
        state
            .appgw
            .health_pending
            .insert(gateway_id(&state).unwrap());
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("probing backend health"));
    }

    #[test]
    fn health_navigation_uses_separate_cursor() {
        let mut state = fixture();
        let gw_id = gateway_id(&state).unwrap();
        state.appgw.show_health = true;
        state.appgw.health.insert(gw_id, sample_health());

        // 5 health rows → max cursor 4. The config cursor must stay untouched.
        for _ in 0..20 {
            handle(Action::MoveDown, &mut state);
        }
        assert_eq!(state.appgw.health_cursor, 4);
        assert_eq!(state.appgw.cursor, 0);
        handle(Action::GotoTop, &mut state);
        assert_eq!(state.appgw.health_cursor, 0);
    }

    #[test]
    fn yank_in_health_mode_returns_probe_log_then_address() {
        let mut state = fixture();
        let gw_id = gateway_id(&state).unwrap();
        state.appgw.show_health = true;
        state.appgw.health.insert(gw_id, sample_health());

        // Row 0 = pool header → gateway :: pool.
        state.appgw.health_cursor = 0;
        assert!(yank_text(&state).unwrap().contains("web-pool"));

        // Row 1 = healthy server → its probe log.
        state.appgw.health_cursor = 1;
        assert_eq!(yank_text(&state).as_deref(), Some("Success. Received 200"));

        // Row 2 = unhealthy server → its probe log.
        state.appgw.health_cursor = 2;
        assert_eq!(
            yank_text(&state).as_deref(),
            Some("Backend server timed out")
        );
    }
}
