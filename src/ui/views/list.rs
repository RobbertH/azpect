//! Resource list with fuzzy filter, favorites toggle, and a per-row health badge.

#![allow(dead_code, unused_variables)]

use chrono::{DateTime, Utc};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use super::edge_scroll;
use crate::azure::health::{derive, HealthStatus};
use crate::azure::logs::supports_logs;
use crate::azure::resources::{Resource, ResourceKind};
use crate::ui::events::Action;
use crate::ui::state::{subscription_display_name, AppState, View};
use crate::ui::theme::Theme;

const FOOTER_HINT: &str =
    "j/k move  Enter open  l logs  f fav  F favs-only  / search  y yank  o portal  s sub  r refresh  ? help  q quit";

const HALF_PAGE: usize = 10;

/// Minimum column widths for the resource list. Fixed bases so that columns
/// don't jump when the visible window changes which long names are on screen;
/// when the terminal is wider than the base layout needs, the truncating
/// columns grow toward their longest content (see `flex_widths`).
/// Names longer than the resolved width get truncated with an ellipsis;
/// shorter names are space-padded.
const NAME_COL_WIDTH: usize = 36;
/// Width of the `KIND` column. Sized for the longest tag (`FuncApp` / `ContApp`,
/// 7 chars); shorter tags (`APIM`, `AppGW`) are space-padded.
const KIND_COL_WIDTH: usize = 7;
/// Width of the `VERSION` column: the deployed image *tag* (the version/hash),
/// not the full image. Most tags (short git SHAs, semver, dates) fit; longer
/// ones truncate with an ellipsis. Blank for APIM / App Gateway rows and for
/// code-deployed Function Apps with no container image.
const VERSION_COL_WIDTH: usize = 14;
const RG_COL_WIDTH: usize = 20;
/// Width of the SUBSCRIPTION column, shown only in all-subscriptions mode
/// (mirrors the Storage / Registries / Cosmos / Key Vault / Service Bus lists).
const SUB_COL_WIDTH: usize = 22;
/// Width of the `CREATED` / `MODIFIED` columns: `YYYY-MM-DD` is 10 chars. We
/// reserve exactly that — older resources with `None` for the timestamp render
/// as an empty column, which keeps the next column aligned.
const DATE_COL_WIDTH: usize = 10;
/// Width of the `NETWORK` column: a single column that means different things by
/// kind — the APIM gateway host (scheme stripped, e.g. `myapim.azure-api.net`)
/// or a Function App / Container App's exposure posture (`public`,
/// `public (restricted)`, `private`). Sized to fit the longest posture label;
/// longer gateway hosts truncate with an ellipsis. Network exposure is worth a
/// glance, so it sits ahead of the (less critical) `CREATED` column.
const NETWORK_COL_WIDTH: usize = 20;

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
    // Freshness indicator: "updating…" while a (re)load is in flight, otherwise
    // "updated Xs ago" off the last completed load. Live-updates because the
    // 250ms tick redraws. Tells the user how stale the badges/versions are and
    // confirms auto-refresh (or a manual `r`) actually fired.
    if state.loading_resources {
        title_spans.push(Span::styled(
            "· updating… ",
            Style::default().fg(theme.accent),
        ));
    } else if let Some(loaded) = state.resources_loaded_at {
        title_spans.push(Span::styled(
            format!("· updated {} ", format_ago(loaded.elapsed())),
            Style::default().fg(theme.muted),
        ));
    }
    // Per-row fetch sweep: the badges/versions arrive from throttled
    // background calls that can take 10s+ on a large subscription, so show
    // how many are back out of how many were launched while any is in
    // flight. Live-updates off the 250ms tick; disappears once settled.
    if let Some((back, launched)) = state.list_fetch_progress() {
        title_spans.push(Span::styled(
            format!("· fetches {back}/{launched} "),
            Style::default().fg(theme.accent),
        ));
    }
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

        // Only worth a subscription column when viewing across all of them.
        let show_sub = state.selected_subscription.is_none();
        let (max_name, max_version, max_rg, max_sub, max_network) =
            flex_widths(state, theme, show_sub, list_area.width as usize);

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
                hdr("KIND", KIND_COL_WIDTH),
                Span::raw("    "), // badge glyph (●) + space + state column padding
                hdr("STATUS", 8),
                // Extra padding spans the 5xx-flag slot (" " + 3 + "  ") that
                // each row renders after the badge, keeping VERSION aligned.
                Span::raw("      "),
                hdr("VERSION", max_version),
                Span::raw("  "),
                hdr("RESOURCE GROUP", max_rg),
            ];
            if show_sub {
                header_spans.push(Span::raw("  "));
                header_spans.push(hdr("SUBSCRIPTION", max_sub));
            }
            header_spans.push(Span::raw("  "));
            header_spans.push(hdr("NETWORK", max_network));
            header_spans.push(Span::raw("  "));
            header_spans.push(hdr("CREATED", DATE_COL_WIDTH));
            header_spans.push(Span::raw("  "));
            header_spans.push(hdr("MODIFIED", DATE_COL_WIDTH));
            let header_spans = clip_spans_to_width(header_spans, ha.width as usize, theme);
            frame.render_widget(Paragraph::new(Line::from(header_spans)), ha);
        }

        let cursor = state.list_cursor.min(filtered.len() - 1);
        let visible = body_area.height as usize;
        let scroll = edge_scroll(&state.list_view_top, cursor, filtered.len(), visible);

        // Sampled once per frame so every CREATED / MODIFIED cell tints against
        // the same "now" (see `date_cell`).
        let now = Utc::now();

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

                let kind_tag = format!("{:<KIND_COL_WIDTH$}", r.kind.short_tag());

                let (badge_color, badge_label, badge_settled) = badge_for_row(r, state, theme);
                // Hollow dot while the verdict is still provisional (availability
                // in, metrics not yet — or nothing in at all); solid once settled.
                let badge_glyph = if badge_settled { "●" } else { "◌" };
                // 5xx presence flag (see `errors_5xx_for_row`): a marker next to
                // the badge, independent of the verdict.
                let five_xx = match errors_5xx_for_row(r, state) {
                    Some(n) if n > 0.0 => "5xx",
                    _ => "",
                };

                let version = format!(
                    "{:<width$}",
                    truncate_right(&version_text(r, state), max_version),
                    width = max_version
                );

                let rg = format!(
                    "{:<width$}",
                    truncate_right(&r.resource_group, max_rg),
                    width = max_rg
                );

                let (created_text, created_color) = date_cell(r.created_at.as_ref(), now, theme);
                let created = format!("{created_text:<DATE_COL_WIDTH$}");
                let (modified_text, modified_color) = date_cell(r.modified_at.as_ref(), now, theme);
                let modified = format!("{modified_text:<DATE_COL_WIDTH$}");

                let mut spans = vec![
                    Span::raw(if selected { "▍ " } else { "  " }),
                    fav_glyph,
                    Span::raw(" "),
                    Span::styled(name, Style::default().fg(theme.fg)),
                    Span::raw("  "),
                    Span::styled(kind_tag, Style::default().fg(theme.accent)),
                    Span::raw("  "),
                    Span::styled(badge_glyph, Style::default().fg(badge_color)),
                    Span::raw(" "),
                    Span::styled(
                        format!("{:<8}", badge_label),
                        Style::default().fg(badge_color),
                    ),
                    Span::raw(" "),
                    Span::styled(format!("{five_xx:<3}"), Style::default().fg(theme.degraded)),
                    Span::raw("  "),
                    Span::styled(version, Style::default().fg(theme.fg)),
                    Span::raw("  "),
                    Span::styled(rg, Style::default().fg(theme.muted)),
                ];

                if show_sub {
                    // Resolve to display name; fall back to the raw id until the
                    // subscription list arrives.
                    let sub = subscription_display_name(state, &r.subscription_id)
                        .unwrap_or(&r.subscription_id);
                    let sub = format!("{:<width$}", truncate_right(sub, max_sub), width = max_sub);
                    spans.push(Span::raw("  "));
                    spans.push(Span::styled(sub, Style::default().fg(theme.muted)));
                }

                // NETWORK before CREATED: exposure posture is worth a glance, so
                // when the row overflows the terminal it's CREATED that gets
                // clipped, not the network state.
                let (net_text, net_color) = network_cell(r, state, theme);
                let network = format!(
                    "{:<width$}",
                    truncate_right(&net_text, max_network),
                    width = max_network
                );
                spans.push(Span::raw("  "));
                spans.push(Span::styled(network, Style::default().fg(net_color)));

                spans.push(Span::raw("  "));
                spans.push(Span::styled(created, Style::default().fg(created_color)));
                spans.push(Span::raw("  "));
                spans.push(Span::styled(modified, Style::default().fg(modified_color)));

                // Clip the assembled row to the visible width with a trailing `…`
                // so a chopped-off column reads as truncated, not silently cut.
                let spans = clip_spans_to_width(spans, body_area.width as usize, theme);
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

/// 5xx count over the fixed-24h health window for a row, once the health
/// metrics have loaded (`None` while loading / on failure). Surfaced as a `5xx`
/// flag next to the badge — a *presence* signal, independent of the verdict, so
/// an app that's HEALTHY by error-ratio but still throwing 500s is visible.
pub(crate) fn errors_5xx_for_row(r: &Resource, state: &AppState) -> Option<f64> {
    state
        .health
        .metrics
        .get(&r.id)
        .map(|m| crate::azure::health::errors_total(m))
}

/// The health badge for a row: its `(color, label)` plus whether the verdict is
/// *settled* (both underlying signals in) or still *provisional* (the renderer
/// draws a hollow `◌` for provisional, a solid `●` once settled).
///
/// The badge derives from two fetches that resolve at different times: Resource
/// Health availability (the fast, authoritative up/degraded/down signal — same
/// thing the portal shows "instantly") and the fixed-24h Errors+Traffic metrics
/// (the slow one). We **lead on availability**: as soon as it lands we render its
/// verdict, then silently upgrade once metrics refine it. This is what makes the
/// list feel as fast as the portal instead of sitting at LOADING for seconds.
///
/// Metrics arriving *first* don't promote a verdict on their own — availability
/// is the lead signal, and a metrics-only read used to flash a wrong IDLE before
/// settling. So while availability is still pending we hold at LOADING regardless
/// of metrics.
///
/// A failure counts as resolved: a 403 on Resource Health drops the availability
/// state (we fall back to the metric verdict) rather than pinning the row; an
/// all-metrics-failed read surfaces as ERROR.
pub(crate) fn badge_for_row(
    r: &Resource,
    state: &AppState,
    theme: &Theme,
) -> (Color, String, bool) {
    let metrics_resolved = state.health.metrics.contains_key(&r.id)
        || state.health.metrics_failures.contains_key(&r.id);
    let availability_resolved =
        state.health.by_resource.contains_key(&r.id) || state.health.failures.contains_key(&r.id);

    // Lead on availability: until the fast signal lands there's nothing to show.
    if !availability_resolved {
        return (theme.muted, "LOADING…".to_string(), false);
    }
    // Availability is in; the verdict is provisional until metrics refine it.
    let settled = metrics_resolved;
    if state.health.metrics_failures.contains_key(&r.id) {
        return (theme.critical, "ERROR".to_string(), settled);
    }

    let metrics = state.health.metrics.get(&r.id);
    let availability = state.health.by_resource.get(&r.id).map(|a| a.state);
    let m: &[crate::azure::metrics::MetricSeries] = metrics.map(|v| v.as_slice()).unwrap_or(&[]);
    let status = derive(m, r.state.as_deref(), availability);
    (
        color_for_health(status, theme),
        status.label().to_string(),
        settled,
    )
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

/// Compact "time since" for the list's freshness indicator: `just now` under
/// 5s, then `Ns ago`, `Nm ago`, `Nh ago`. Coarse on purpose — it answers "how
/// stale is this" at a glance, not to-the-second precision.
fn format_ago(elapsed: std::time::Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 5 {
        "just now".to_string()
    } else if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else {
        format!("{}h ago", secs / 3600)
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

/// `YYYY-MM-DD` plus a recency-tinted colour for the CREATED / MODIFIED columns,
/// so a freshly-deployed or just-touched resource catches the eye against the
/// muted older rows. Tiers (by age relative to `now`): under a week reads in
/// `accent`, under a month in the normal `fg`, and older — or a `None`
/// timestamp from a pre-`systemData` resource — stays `muted`. A timestamp that
/// lands slightly in the future (minor clock skew) counts as freshest.
fn date_cell(dt: Option<&DateTime<Utc>>, now: DateTime<Utc>, theme: &Theme) -> (String, Color) {
    match dt {
        Some(d) => {
            let days = now.signed_duration_since(*d).num_days();
            let color = if days < 7 {
                theme.accent
            } else if days < 30 {
                theme.fg
            } else {
                theme.muted
            };
            (format_date(Some(d)), color)
        }
        None => (String::new(), theme.muted),
    }
}

/// Content + colour for the kind-aware NETWORK column. APIM rows show the
/// gateway host (scheme stripped, in `accent` like a link); Function App rows
/// show their public/private posture in three states, mirroring the portal:
/// `private` (`healthy`) when public access is disabled, `public (restricted)`
/// (`idle`) when reachable but gated by IP/VNet rules, and a bare `public`
/// (`degraded` — worth a glance for an internal API tool) when wide open. The
/// restriction detail rides on the same eager `config/web` fetch as the image;
/// until it lands, an Enabled app reads as plain `public`. Every other kind
/// (and an APIM service with no gateway yet) renders blank.
fn network_cell(r: &Resource, state: &AppState, theme: &Theme) -> (String, Color) {
    match r.kind {
        ResourceKind::Apim => match r.meta.gateway_url.as_deref() {
            Some(url) if !url.is_empty() => (strip_scheme(url).to_string(), theme.accent),
            _ => (String::new(), theme.muted),
        },
        ResourceKind::FunctionApp => {
            if !r.meta.public_network_enabled() {
                ("private".to_string(), theme.healthy)
            } else {
                match state.func_image.access_restricted.get(&r.id).copied() {
                    Some(true) => ("public (restricted)".to_string(), theme.idle),
                    Some(false) => ("public".to_string(), theme.degraded),
                    // Restriction state not known yet. While the config/web fetch
                    // is in flight show a loading dash rather than committing to
                    // the wide-open "public" label — a restricted app must not
                    // flash as fully public. A finished-but-dataless fetch (e.g.
                    // a 403) falls back to the bare posture.
                    None if image_pending(r, state) => ("…".to_string(), theme.muted),
                    None => ("public".to_string(), theme.degraded),
                }
            }
        }
        // Container Apps express exposure through ingress, not publicNetworkAccess:
        // external ingress is internet-facing, internal/none is not. Same vocab as
        // Function Apps so the column reads consistently; the Detail row carries
        // the finer internal-vs-no-ingress distinction.
        ResourceKind::ContainerApp => match state.container_app_overview.by_resource.get(&r.id) {
            Some(ov) => match ov.ingress_external {
                Some(true) if ov.access_restricted => {
                    ("public (restricted)".to_string(), theme.idle)
                }
                Some(true) => ("public".to_string(), theme.degraded),
                Some(false) | None => ("private".to_string(), theme.healthy),
            },
            // Overview fetch (eager, like the FA image) still in flight.
            None if state.container_app_overview.pending.contains(&r.id) => {
                ("…".to_string(), theme.muted)
            }
            None => (String::new(), theme.muted),
        },
        _ => (String::new(), theme.muted),
    }
}

/// Strip a leading `https://` / `http://` so the gateway host fits the column.
/// The scheme is always `https` for an APIM gateway, so dropping it loses no
/// signal; the Detail view still shows the full URL for copy.
fn strip_scheme(url: &str) -> &str {
    url.strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url)
}

/// The deployed container image for a row, when known. Container Apps read the
/// active-revision image (already fetched for every row alongside the health
/// badge); Function Apps read the `config/web` image fetched in the background.
/// APIM and App Gateways have no container image.
fn deployed_image(r: &Resource, state: &AppState) -> Option<String> {
    match r.kind {
        ResourceKind::ContainerApp => state
            .revision_meta
            .by_resource
            .get(&r.id)
            .and_then(|m| m.image.clone()),
        ResourceKind::FunctionApp => state.func_image.by_resource.get(&r.id).cloned().flatten(),
        _ => None,
    }
}

/// Whether the fetch backing a row's deployed image is still in flight, so the
/// VERSION column can show `…` rather than a misleading blank.
fn image_pending(r: &Resource, state: &AppState) -> bool {
    match r.kind {
        // Container App image rides on the revisions fetch that drives health.
        ResourceKind::ContainerApp => state.health.pending.contains(&r.id),
        ResourceKind::FunctionApp => state.func_image.pending.contains(&r.id),
        _ => false,
    }
}

/// Extract the tag — the version/hash — from a full image reference. Splits off
/// the final path segment first so a registry port (`host:port/img:tag`) isn't
/// mistaken for the tag; a digest reference (`img@sha256:hex`) yields the hex.
/// Returns the empty string for an untagged image.
fn image_tag(image: &str) -> &str {
    let last_segment = image.rsplit('/').next().unwrap_or(image);
    match last_segment.rsplit_once(':') {
        Some((_, tag)) => tag,
        None => "",
    }
}

/// The VERSION cell's text: the deployed image tag (the version/hash), `…`
/// while the backing fetch is in flight, blank when there's no image to show.
fn version_text(r: &Resource, state: &AppState) -> String {
    match deployed_image(r, state) {
        Some(image) => image_tag(&image).to_string(),
        None if image_pending(r, state) => "…".to_string(),
        None => String::new(),
    }
}

/// Resolve the widths of the five truncating columns — NAME, VERSION,
/// RESOURCE GROUP, SUBSCRIPTION, NETWORK — for a list area `width` columns
/// wide. Each starts at its base `*_COL_WIDTH`; when the terminal leaves
/// slack beyond the base layout, columns grow toward their longest content,
/// most-important column first, so wide terminals show fewer ellipses.
/// Content is measured over *all* resources (not the visible window or the
/// filtered set), so widths stay put while scrolling and filtering.
fn flex_widths(
    state: &AppState,
    theme: &Theme,
    show_sub: bool,
    width: usize,
) -> (usize, usize, usize, usize, usize) {
    let mut want_name = 0usize;
    let mut want_version = 0usize;
    let mut want_rg = 0usize;
    let mut want_sub = 0usize;
    let mut want_network = 0usize;
    for r in &state.resources {
        want_name = want_name.max(r.name.chars().count());
        want_version = want_version.max(version_text(r, state).chars().count());
        want_rg = want_rg.max(r.resource_group.chars().count());
        if show_sub {
            let sub =
                subscription_display_name(state, &r.subscription_id).unwrap_or(&r.subscription_id);
            want_sub = want_sub.max(sub.chars().count());
        }
        want_network = want_network.max(network_cell(r, state, theme).0.chars().count());
    }

    let mut name = NAME_COL_WIDTH;
    let mut version = VERSION_COL_WIDTH;
    let mut rg = RG_COL_WIDTH;
    let mut sub = SUB_COL_WIDTH;
    let mut network = NETWORK_COL_WIDTH;

    // Everything in a row besides the five flexible columns: the selection /
    // favorite prefix (4), the gap after NAME, KIND with its trailing gap,
    // the badge + STATUS + 5xx block including its trailing gap (16), the
    // gaps after VERSION / RESOURCE GROUP / NETWORK, the extra gap before
    // SUBSCRIPTION when shown, and the two trailing date columns (CREATED +
    // MODIFIED), each with its leading gap.
    let fixed = 4
        + 2
        + KIND_COL_WIDTH
        + 2
        + 16
        + 2
        + 2
        + if show_sub { 2 } else { 0 }
        + 2
        + DATE_COL_WIDTH
        + 2
        + DATE_COL_WIDTH;
    let base = fixed + name + version + rg + network + if show_sub { sub } else { 0 };
    let mut slack = width.saturating_sub(base);
    for (col, want) in [
        (&mut name, want_name),
        (&mut version, want_version),
        (&mut network, want_network),
        (&mut sub, if show_sub { want_sub } else { 0 }),
        (&mut rg, want_rg),
    ] {
        let grow = want.saturating_sub(*col).min(slack);
        *col += grow;
        slack -= grow;
    }
    (name, version, rg, sub, network)
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

/// Clip a row's spans to `width` display columns, appending a muted `…` when
/// content is dropped. Columns are left-aligned and fixed-width, so the cut only
/// ever lands in the rightmost (least important) columns; the ellipsis makes a
/// chopped-off column read as truncated rather than silently missing. Counted in
/// chars, matching [`truncate_right`]. A no-op when everything already fits.
fn clip_spans_to_width(
    spans: Vec<Span<'static>>,
    width: usize,
    theme: &Theme,
) -> Vec<Span<'static>> {
    let total: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    if total <= width {
        return spans;
    }
    if width == 0 {
        return Vec::new();
    }
    let budget = width - 1; // leave a column for the ellipsis
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for s in spans {
        let len = s.content.chars().count();
        if used + len <= budget {
            used += len;
            out.push(s);
        } else {
            let take = budget - used;
            if take > 0 {
                let truncated: String = s.content.chars().take(take).collect();
                out.push(Span::styled(truncated, s.style));
            }
            break;
        }
    }
    out.push(Span::styled(
        "\u{2026}".to_string(),
        Style::default().fg(theme.muted),
    ));
    out
}

pub fn handle(action: Action, state: &mut AppState) -> bool {
    let filtered_len = state.filtered_resources().len();

    // Esc clears the search filter — whether the box is still focused or the
    // filter was applied then defocused (via Enter/Down) — and returns to the
    // full list. Only an already-clear list lets Esc fall through to navigation.
    if matches!(action, Action::Back)
        && (state.list_filter_active || !state.list_filter.value().is_empty())
    {
        state.list_filter_active = false;
        state.list_filter.reset();
        state.list_cursor = 0;
        return true;
    }

    // While the search box is active, swallow nav/special actions and let Lane 3
    // forward raw key events into `list_filter` for editing. The set we still
    // handle here is limited to ones that should affect the underlying list.
    if state.list_filter_active {
        match action {
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
                state.view = match kind {
                    ResourceKind::AppGateway => {
                        state.appgw.cursor = 0;
                        View::AppGatewayBackends
                    }
                    _ => {
                        // Fresh Detail entry: reset the meta-row cursor + any
                        // lingering modal from a previous visit. Esc-back-then-
                        // re-Enter should land the cursor at the top, not
                        // wherever the user left it on the previous resource.
                        state.detail_view = crate::ui::state::DetailView::default();
                        View::Detail
                    }
                };
            }
            true
        }
        Action::OpenLogs => {
            if let Some(sel) = state.selected_resource() {
                if supports_logs(sel.kind) {
                    let id = sel.id.clone();
                    state.config.last_resource_id = Some(id);
                    // Drop the previous resource's source/search filters so the
                    // new app's logs aren't hidden behind a stale filter.
                    state.logs.reset_view_filters();
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
            state.list_filter.reset();
            state.list_cursor = 0;
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
    fn window_scrolls_only_when_cursor_pushes_an_edge() {
        let theme = Theme::catppuccin_mocha();
        // Short terminal so the list overflows the viewport.
        let backend = TestBackend::new(180, 12);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.resources = (0..50)
            .map(|i| {
                r(
                    &format!("/r/{i}"),
                    &format!("res-{i:02}"),
                    ResourceKind::FunctionApp,
                )
            })
            .collect();

        // First frame pins the window to the top.
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        assert_eq!(state.list_view_top.get(), 0);

        // Drive the cursor past the bottom edge: the window follows it down.
        for _ in 0..30 {
            handle(Action::MoveDown, &mut state);
        }
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let top = state.list_view_top.get();
        assert!(top > 0, "window should have scrolled down, top={top}");

        // Stepping back up from the bottom edge must NOT scroll — the cursor
        // moves freely inside the window until it pushes against the top.
        handle(Action::MoveUp, &mut state);
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        assert_eq!(state.list_view_top.get(), top);

        // Jumping to the top pushes the edge → window follows back to row 0.
        handle(Action::GotoTop, &mut state);
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        assert_eq!(state.list_view_top.get(), 0);
    }

    #[test]
    fn renders_without_panic() {
        let theme = Theme::catppuccin_mocha();
        // Wide enough for all columns (NAME…CREATED, incl. the SUBSCRIPTION and
        // VERSION columns) so header assertions below see the trailing ones.
        let backend = TestBackend::new(180, 12);
        let mut term = Terminal::new(backend).unwrap();
        let state = fixture();
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("alpha-func"));
        assert!(s.contains("FuncApp"));
        assert!(s.contains("LOADING"));
        // VERSION column shipped.
        assert!(s.contains("VERSION"), "expected VERSION header in {s}");
        // The renamed block title now reads "api resources".
        assert!(
            s.contains("api resources"),
            "expected api resources title in {s}"
        );
        // CREATED header column shipped — even if every row's date is empty.
        assert!(s.contains("CREATED"), "expected CREATED header in {s}");
    }

    #[test]
    fn title_shows_fetch_progress_while_sweep_in_flight() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(180, 12);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        // Sweep mid-flight: one health result back (a failure counts — the
        // fetch IS finished), two still pending, plus the Container App's
        // overview pending → 1 back of 4 launched.
        state
            .health
            .failures
            .insert("/r/one".into(), "denied".into());
        state.health.pending.insert("/r/two".into());
        state.health.pending.insert("/r/three".into());
        state
            .container_app_overview
            .pending
            .insert("/r/three".into());
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("fetches 1/4"), "expected fetch progress in {s}");

        // Everything landed: the indicator must disappear, not stick at n/n.
        state.health.pending.clear();
        state.container_app_overview.pending.clear();
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(!s.contains("fetches"), "indicator should hide once settled");
    }

    /// Build a metrics vec with `traffic` requests and zero errors across the
    /// trailing window, so `derive` resolves to HEALTHY once both signals land.
    #[test]
    fn format_ago_buckets_by_magnitude() {
        use std::time::Duration;
        assert_eq!(format_ago(Duration::from_secs(0)), "just now");
        assert_eq!(format_ago(Duration::from_secs(4)), "just now");
        assert_eq!(format_ago(Duration::from_secs(5)), "5s ago");
        assert_eq!(format_ago(Duration::from_secs(59)), "59s ago");
        assert_eq!(format_ago(Duration::from_secs(60)), "1m ago");
        assert_eq!(format_ago(Duration::from_secs(3599)), "59m ago");
        assert_eq!(format_ago(Duration::from_secs(3600)), "1h ago");
        assert_eq!(format_ago(Duration::from_secs(7200)), "2h ago");
    }

    fn healthy_metrics() -> Vec<crate::azure::metrics::MetricSeries> {
        use crate::azure::metrics::{MetricKind, MetricPoint, MetricSeries};
        let now = Utc::now();
        let pts = |v: f64| {
            (0..4)
                .map(|i| MetricPoint {
                    ts: now - chrono::Duration::minutes(15 * (4 - i)),
                    value: v,
                })
                .collect::<Vec<_>>()
        };
        vec![
            MetricSeries {
                kind: MetricKind::Errors,
                label: "Http 5xx".into(),
                unit: "count".into(),
                points: pts(0.0),
                peak_replica: None,
            },
            MetricSeries {
                kind: MetricKind::Traffic,
                label: "Requests".into(),
                unit: "count".into(),
                points: pts(100.0),
                peak_replica: None,
            },
        ]
    }

    fn avail_available() -> crate::azure::resource_health::ResourceAvailability {
        use crate::azure::resource_health::{AvailabilityState, ResourceAvailability};
        ResourceAvailability {
            state: AvailabilityState::Available,
            reason: None,
        }
    }

    #[test]
    fn badge_holds_loading_until_both_signals_resolve() {
        let theme = Theme::catppuccin_mocha();
        let mut state = fixture();
        let res = state.resources[0].clone();

        // Nothing loaded yet.
        assert_eq!(badge_for_row(&res, &state, &theme).1, "LOADING…");

        // Health metrics arrived but Resource Health still pending → still
        // LOADING (this is the case that used to flash IDLE).
        state
            .health
            .metrics
            .insert(res.id.clone(), healthy_metrics());
        assert_eq!(
            badge_for_row(&res, &state, &theme).1,
            "LOADING…",
            "metrics-only must not derive a badge"
        );

        // Resource Health lands → both resolved → real verdict.
        state
            .health
            .by_resource
            .insert(res.id.clone(), avail_available());
        assert_eq!(badge_for_row(&res, &state, &theme).1, "HEALTHY");
    }

    #[test]
    fn badge_shows_provisional_rh_verdict_before_metrics() {
        // We lead on Resource Health: once availability lands we render its
        // verdict immediately (Available → HEALTHY) even though metrics haven't
        // loaded, but mark it *un*settled so the renderer draws a hollow dot.
        let theme = Theme::catppuccin_mocha();
        let mut state = fixture();
        let res = state.resources[0].clone();
        state
            .health
            .by_resource
            .insert(res.id.clone(), avail_available());
        let (_, label, settled) = badge_for_row(&res, &state, &theme);
        assert_eq!(label, "HEALTHY");
        assert!(!settled, "verdict is provisional until metrics resolve");

        // Metrics land → same verdict, now settled (solid dot).
        state
            .health
            .metrics
            .insert(res.id.clone(), healthy_metrics());
        let (_, label, settled) = badge_for_row(&res, &state, &theme);
        assert_eq!(label, "HEALTHY");
        assert!(settled, "both signals in → settled");
    }

    #[test]
    fn badge_treats_health_failure_as_resolved() {
        // A 403 on Resource Health must not pin the row at LOADING forever — we
        // fall back to the metric-only verdict.
        let theme = Theme::catppuccin_mocha();
        let mut state = fixture();
        let res = state.resources[0].clone();
        state
            .health
            .metrics
            .insert(res.id.clone(), healthy_metrics());
        state
            .health
            .failures
            .insert(res.id.clone(), "403 Forbidden".into());
        assert_eq!(badge_for_row(&res, &state, &theme).1, "HEALTHY");
    }

    #[test]
    fn renders_5xx_flag_when_health_window_has_errors() {
        // A HEALTHY row that nonetheless had 5xx in the 24h window must show the
        // `5xx` flag next to the badge.
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(160, 12);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        let id = state.resources[0].id.clone();
        // Healthy ratio (1 error / 1000 req) but errors_total > 0.
        let mut metrics = healthy_metrics();
        if let Some(errors) = metrics
            .iter_mut()
            .find(|m| m.kind == crate::azure::metrics::MetricKind::Errors)
        {
            errors.points.last_mut().unwrap().value = 1.0;
        }
        state.health.metrics.insert(id.clone(), metrics);
        state.health.by_resource.insert(id, avail_available());

        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("HEALTHY"), "expected HEALTHY badge, got {s}");
        assert!(
            s.contains("5xx"),
            "expected 5xx flag next to badge, got {s}"
        );
    }

    #[test]
    fn no_5xx_flag_when_window_is_clean() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(160, 12);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        let id = state.resources[0].id.clone();
        // healthy_metrics() has zero errors.
        state.health.metrics.insert(id.clone(), healthy_metrics());
        state.health.by_resource.insert(id, avail_available());
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("HEALTHY"));
        assert!(
            !s.contains("5xx"),
            "clean window must not show the 5xx flag"
        );
    }

    #[test]
    fn renders_created_column_value_when_present() {
        use chrono::TimeZone;

        let theme = Theme::catppuccin_mocha();
        // Wide enough to reach the CREATED column, which now trails NETWORK and
        // the all-subscriptions SUBSCRIPTION column.
        let backend = TestBackend::new(180, 12);
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
    fn image_tag_extracts_the_version() {
        assert_eq!(image_tag("nginx:latest"), "latest");
        assert_eq!(image_tag("myacr.azurecr.io/files-api:abc123"), "abc123");
        // A registry port must not be mistaken for the tag.
        assert_eq!(image_tag("host:5000/img:v1.2.3"), "v1.2.3");
        // Digest-pinned image yields the digest hex.
        assert_eq!(image_tag("acr/app@sha256:deadbeef"), "deadbeef");
        // Untagged images have no version to show.
        assert_eq!(image_tag("nginx"), "");
        assert_eq!(image_tag("acr/nginx"), "");
    }

    #[test]
    fn renders_container_app_version_tag() {
        use crate::azure::container_app_revisions::ActiveRevisionMeta;
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(160, 12);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        // fixture()[2] (/r/three) is the Container App. Its image rides on the
        // revision-meta cache, already populated by the health fetch.
        state.revision_meta.by_resource.insert(
            "/r/three".into(),
            ActiveRevisionMeta {
                image: Some("myacr.azurecr.io/files-api:abc123".into()),
                ..Default::default()
            },
        );
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(
            s.contains("abc123"),
            "expected container app image tag in row, got {s}"
        );
    }

    #[test]
    fn renders_function_app_version_tag() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(160, 12);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        // fixture()[0] (/r/one) is the Function App. Its image comes from the
        // background config/web fetch.
        state
            .func_image
            .by_resource
            .insert("/r/one".into(), Some("acr/f:fnver99".into()));
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(
            s.contains("fnver99"),
            "expected function app image tag in row, got {s}"
        );
    }

    #[test]
    fn strip_scheme_drops_https() {
        assert_eq!(
            strip_scheme("https://myapim.azure-api.net"),
            "myapim.azure-api.net"
        );
        assert_eq!(strip_scheme("http://x.example"), "x.example");
        // No scheme to strip — returned unchanged.
        assert_eq!(strip_scheme("myapim.azure-api.net"), "myapim.azure-api.net");
    }

    #[test]
    fn network_cell_apim_shows_gateway_host() {
        let theme = Theme::catppuccin_mocha();
        let state = AppState::new(Config::default());
        let mut res = r("/r/apim", "an-apim", ResourceKind::Apim);
        res.meta.gateway_url = Some("https://an-apim.azure-api.net".into());
        let (text, color) = network_cell(&res, &state, &theme);
        assert_eq!(text, "an-apim.azure-api.net");
        assert_eq!(color, theme.accent);

        // No gateway resolved yet → blank, not a stray scheme.
        let bare = r("/r/apim2", "bare-apim", ResourceKind::Apim);
        assert_eq!(network_cell(&bare, &state, &theme).0, "");
    }

    #[test]
    fn network_cell_function_app_shows_access_posture() {
        let theme = Theme::catppuccin_mocha();
        let mut state = AppState::new(Config::default());

        // Unset → Azure default is public. While the config/web fetch is still
        // in flight the restriction state is unknown → loading dash, not a
        // premature "public" that could mislabel a restricted app.
        let public = r("/r/f1", "open-func", ResourceKind::FunctionApp);
        state.func_image.pending.insert(public.id.clone());
        let (text, color) = network_cell(&public, &state, &theme);
        assert_eq!(text, "…");
        assert_eq!(color, theme.muted);

        // Fetch finished without restriction data (e.g. 403) → bare public.
        state.func_image.pending.remove(&public.id);
        let (text, color) = network_cell(&public, &state, &theme);
        assert_eq!(text, "public");
        assert_eq!(color, theme.degraded);

        // Same app once config/web reports IP/VNet restrictions → distinct label.
        state
            .func_image
            .access_restricted
            .insert(public.id.clone(), true);
        let (text, color) = network_cell(&public, &state, &theme);
        assert_eq!(text, "public (restricted)");
        assert_eq!(color, theme.idle);

        // A known-unrestricted app stays bare public.
        state
            .func_image
            .access_restricted
            .insert(public.id.clone(), false);
        assert_eq!(network_cell(&public, &state, &theme).0, "public");

        let mut private = r("/r/f2", "locked-func", ResourceKind::FunctionApp);
        private.meta.public_network_access = Some("Disabled".into());
        let (text, color) = network_cell(&private, &state, &theme);
        assert_eq!(text, "private");
        assert_eq!(color, theme.healthy);
    }

    #[test]
    fn network_cell_container_app_reflects_ingress() {
        use crate::azure::container_app_overview::ContainerAppOverview;
        let theme = Theme::catppuccin_mocha();
        let mut state = AppState::new(Config::default());
        let ca = r("/r/ca", "some-ca", ResourceKind::ContainerApp);

        // Overview fetch in flight → loading dash.
        state.container_app_overview.pending.insert(ca.id.clone());
        assert_eq!(network_cell(&ca, &state, &theme), ("…".into(), theme.muted));
        state.container_app_overview.pending.remove(&ca.id);

        let put = |state: &mut AppState, ext: Option<bool>, restricted: bool| {
            state.container_app_overview.by_resource.insert(
                ca.id.clone(),
                ContainerAppOverview {
                    ingress_external: ext,
                    access_restricted: restricted,
                    ..Default::default()
                },
            );
        };
        put(&mut state, Some(true), false);
        assert_eq!(
            network_cell(&ca, &state, &theme),
            ("public".into(), theme.degraded)
        );
        put(&mut state, Some(true), true);
        assert_eq!(
            network_cell(&ca, &state, &theme),
            ("public (restricted)".into(), theme.idle)
        );
        put(&mut state, Some(false), false); // internal ingress → not public
        assert_eq!(
            network_cell(&ca, &state, &theme),
            ("private".into(), theme.healthy)
        );
        put(&mut state, None, false); // no ingress → not public
        assert_eq!(
            network_cell(&ca, &state, &theme),
            ("private".into(), theme.healthy)
        );
    }

    #[test]
    fn network_cell_blank_for_other_kinds() {
        let theme = Theme::catppuccin_mocha();
        let state = AppState::new(Config::default());
        let agw = r("/r/agw", "some-gw", ResourceKind::AppGateway);
        assert_eq!(network_cell(&agw, &state, &theme).0, "");
    }

    #[test]
    fn clip_spans_appends_ellipsis_only_on_overflow() {
        let theme = Theme::catppuccin_mocha();
        let spans = vec![
            Span::raw("hello "),
            Span::styled("world!!".to_string(), Style::default().fg(theme.fg)),
        ]; // 13 chars total
        let text = |out: &[Span]| -> String { out.iter().map(|s| s.content.as_ref()).collect() };

        // Fits exactly → untouched, no ellipsis.
        let out = clip_spans_to_width(spans.clone(), 13, &theme);
        assert_eq!(text(&out), "hello world!!");

        // Overflows → truncated to width-1 chars plus a trailing ellipsis, and
        // span styles are preserved up to the cut.
        let out = clip_spans_to_width(spans, 10, &theme);
        let t = text(&out);
        assert_eq!(t, "hello wor\u{2026}");
        assert_eq!(t.chars().count(), 10);
    }

    #[test]
    fn flex_widths_grow_into_slack() {
        let theme = Theme::catppuccin_mocha();
        let mut state = AppState::new(Config::default());
        let mut long = r("/r/long", &"n".repeat(50), ResourceKind::ContainerApp);
        long.resource_group = "x".repeat(30);
        state.resources = vec![long];

        // Narrower than the base layout → every column stays at its minimum.
        let widths = flex_widths(&state, &theme, true, 80);
        assert_eq!(
            widths,
            (
                NAME_COL_WIDTH,
                VERSION_COL_WIDTH,
                RG_COL_WIDTH,
                SUB_COL_WIDTH,
                NETWORK_COL_WIDTH
            )
        );

        // Plenty of slack → NAME and RESOURCE GROUP stretch to their longest
        // content; columns whose content already fits keep their base width.
        let (name, version, rg, sub, network) = flex_widths(&state, &theme, true, 400);
        assert_eq!(name, 50);
        assert_eq!(rg, 30);
        assert_eq!(
            (version, sub, network),
            (VERSION_COL_WIDTH, SUB_COL_WIDTH, NETWORK_COL_WIDTH)
        );

        // Slack covers only part of the deficit → NAME (highest priority)
        // absorbs all of it; RESOURCE GROUP waits its turn.
        let base = 61 // fixed overhead with SUBSCRIPTION shown, see `flex_widths`
            + NAME_COL_WIDTH
            + VERSION_COL_WIDTH
            + RG_COL_WIDTH
            + SUB_COL_WIDTH
            + NETWORK_COL_WIDTH;
        let (name, _, rg, _, _) = flex_widths(&state, &theme, true, base + 5);
        assert_eq!(name, NAME_COL_WIDTH + 5);
        assert_eq!(rg, RG_COL_WIDTH);
    }

    /// Pins `flex_widths`' fixed-overhead constant to the real row layout: at
    /// a width that *exactly* fits the grown NAME column, the full name and
    /// both the CREATED and the trailing MODIFIED date must render. If the
    /// constant overcounts, NAME comes up short and keeps its ellipsis; if it
    /// undercounts, the row overflows and the trailing MODIFIED date gets
    /// clipped.
    #[test]
    fn flex_widths_fixed_overhead_matches_row_layout() {
        use chrono::TimeZone;
        let theme = Theme::catppuccin_mocha();
        let mut state = AppState::new(Config::default());
        let mut res = r("/r/long", &"n".repeat(50), ResourceKind::ContainerApp);
        res.created_at = Some(Utc.with_ymd_and_hms(2024, 1, 15, 0, 0, 0).unwrap());
        res.modified_at = Some(Utc.with_ymd_and_hms(2024, 2, 20, 0, 0, 0).unwrap());
        state.resources = vec![res];
        // Inner width exactly fits NAME grown to 50: base layout with the
        // SUBSCRIPTION column (61 fixed + 112 base columns) + NAME's 14-char
        // deficit = 187, plus 2 for the block borders.
        let backend = TestBackend::new(189, 8);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains(&"n".repeat(50)), "expected full name in {s}");
        assert!(
            s.contains("2024-01-15"),
            "expected full CREATED date in {s}"
        );
        assert!(
            s.contains("2024-02-20"),
            "expected full MODIFIED date in {s}"
        );
    }

    #[test]
    fn date_cell_tints_by_recency() {
        use chrono::TimeZone;
        let theme = Theme::catppuccin_mocha();
        let now = Utc.with_ymd_and_hms(2026, 6, 23, 12, 0, 0).unwrap();

        // Under a week → accent; under a month → fg; older → muted.
        assert_eq!(
            date_cell(Some(&(now - chrono::Duration::days(2))), now, &theme).1,
            theme.accent
        );
        assert_eq!(
            date_cell(Some(&(now - chrono::Duration::days(20))), now, &theme).1,
            theme.fg
        );
        assert_eq!(
            date_cell(Some(&(now - chrono::Duration::days(90))), now, &theme).1,
            theme.muted
        );

        // Missing timestamp → blank text, muted.
        let (text, color) = date_cell(None, now, &theme);
        assert!(text.is_empty());
        assert_eq!(color, theme.muted);
    }

    #[test]
    fn renders_modified_column() {
        use chrono::TimeZone;
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(200, 12);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.resources[0].modified_at = Some(Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap());
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("MODIFIED"), "expected MODIFIED header in {s}");
        assert!(s.contains("2026-06-01"), "expected modified date in {s}");
    }

    #[test]
    fn renders_network_column() {
        let theme = Theme::catppuccin_mocha();
        // Wide enough that the appended NETWORK column is fully visible even with
        // the SUBSCRIPTION column shown (all-subscriptions mode).
        let backend = TestBackend::new(200, 12);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        // fixture()[1] (/r/two) is the APIM; give it a gateway URL.
        state.resources[1].meta.gateway_url = Some("https://beta-apim.azure-api.net".into());
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("NETWORK"), "expected NETWORK header in {s}");
        // The gateway host is longer than the column, so it shows truncated
        // (`…`); assert on the visible prefix rather than the full host.
        assert!(
            s.contains("beta-apim.azure-api"),
            "expected gateway host in {s}"
        );
        // fixture()[0] (/r/one) is a Function App with no access set → public.
        assert!(
            s.contains("public"),
            "expected funcapp access posture in {s}"
        );
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
    fn start_search_discards_committed_filter() {
        let mut state = fixture();
        state.list_filter = "alpha".into();
        state.list_cursor = 1;
        assert!(handle(Action::StartSearch, &mut state));
        assert!(state.list_filter_active);
        assert_eq!(state.list_filter.value(), "");
        assert_eq!(state.list_cursor, 0);
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
    fn open_logs_allows_apim() {
        let mut state = fixture();
        state.view = View::List;
        // cursor 1 is APIM — its gateway request logs are supported.
        state.list_cursor = 1;
        assert!(handle(Action::OpenLogs, &mut state));
        assert_eq!(state.view, View::Logs);
    }

    #[test]
    fn open_logs_blocks_unsupported_kind() {
        let mut state = fixture();
        state.view = View::List;
        // Application Gateway has no log template — opening logs must no-op
        // with a status message rather than transition.
        state.resources[1].kind = ResourceKind::AppGateway;
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
    fn esc_clears_active_filter() {
        let mut state = fixture();
        state.list_filter = tui_input::Input::new("beta".to_string());
        state.list_filter_active = true;
        state.list_cursor = 1;
        assert!(handle(Action::Back, &mut state));
        assert!(!state.list_filter_active);
        assert_eq!(state.list_filter.value(), "");
        assert_eq!(state.list_cursor, 0);
    }

    #[test]
    fn esc_clears_applied_filter_after_defocus() {
        // /foo, Enter to browse (defocus), then Esc should still clear the filter
        // rather than navigating away.
        let mut state = fixture();
        state.list_filter = tui_input::Input::new("beta".to_string());
        state.list_filter_active = false;
        assert!(handle(Action::Back, &mut state));
        assert_eq!(state.list_filter.value(), "");
    }

    #[test]
    fn esc_on_clear_list_falls_through_to_navigation() {
        // No filter → list::handle must NOT consume Esc, so the global handler
        // can navigate back.
        let mut state = fixture();
        assert!(!handle(Action::Back, &mut state));
    }

    #[test]
    fn favorites_only_toggle() {
        let mut state = fixture();
        assert!(handle(Action::ToggleFavoritesOnly, &mut state));
        assert!(state.favorites_only);
    }

    #[test]
    fn restore_list_cursor_reanchors_in_filtered_space() {
        // Regression: the 60s autorefresh (and manual `r`) restores the cursor
        // to the last-selected resource after replacing `resources`. With a
        // filter active, restoring/clamping against the *full* list left the
        // cursor past the last visible row — the highlight pinned to the bottom
        // and needed many `k` presses to climb out. The restore must happen in
        // filtered-index space.
        let mut state = fixture();
        // Filter to a single match ("beta-apim" at full index 1) so the full
        // and filtered index spaces diverge.
        state.list_filter = tui_input::Input::new("beta".to_string());
        assert_eq!(state.filtered_resources().len(), 1);

        // Anchor on the resource at full index 2, which is *not* in the
        // filtered set: the cursor must clamp to the single filtered row, not
        // jump to full index 2 (which would render as a bottom-clamped ghost).
        state.list_cursor = 99;
        state.restore_list_cursor(Some("/r/three"));
        assert_eq!(state.list_cursor, 0);
        assert!(state.selected_resource().is_some());

        // Anchor on the resource that *is* in the filtered set → restore to its
        // filtered index (0 here), regardless of its full-list index (1).
        state.list_cursor = 99;
        state.restore_list_cursor(Some("/r/two"));
        assert_eq!(state.list_cursor, 0);

        // No filter, no anchor: a stale out-of-range cursor still clamps to the
        // last row of the full list.
        state.list_filter.reset();
        state.list_cursor = 99;
        state.restore_list_cursor(None);
        assert_eq!(state.list_cursor, state.resources.len() - 1);
    }
}
