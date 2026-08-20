//! Detail view: five sparklines (Requests, Http 5xx, Http 4xx, CPU, Memory)
//! plus a header with the resource name + RG + health badge + window label.

#![allow(dead_code, unused_variables)]

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::azure::health::{derive, find, HealthStatus};
use crate::azure::logs::supports_logs;
use crate::azure::metrics::{MetricKind, MetricSeries, TimeRange};
use crate::azure::resources::{Resource, ResourceKind};
use crate::ui::events::Action;
use crate::ui::state::{AppState, DetailModal, View};
use crate::ui::theme::Theme;

/// One Detail row's worth of selection metadata: what `y` should yank, what
/// the Enter modal should show, and (for env-vars-style rows) whether Enter
/// should dispatch a different action instead of opening the modal.
///
/// Built by [`selectable_metas`] in the same order the rows appear on screen,
/// so the cursor index in [`crate::ui::state::DetailView`] maps 1:1 to a slot
/// in the returned Vec.
#[derive(Clone)]
struct SelectableMeta {
    /// What `y` copies. The plain value, not the label.
    yank: String,
    /// Modal window title.
    modal_title: String,
    /// Modal body lines (one entry per visible line, wrapped at modal width).
    modal_lines: Vec<String>,
    /// When `Some`, Enter dispatches this action instead of opening the modal.
    /// Currently only used by the env-vars teaser row, which routes Enter to
    /// the dedicated EnvVars page (the modal is redundant when a real page
    /// exists).
    enter_action: Option<Action>,
}

/// Base footer hint without a resource-kind-specific Enter clue. The drill-in
/// segment is appended per-render by [`footer_hint_for`] so Function Apps /
/// Container Apps don't show an Enter that does nothing.
const FOOTER_HINT_BASE: &str = "0 1h  1 1d  7 7d  l logs  Esc back  r refresh  ? help  q quit";

fn footer_hint_for(kind: crate::azure::resources::ResourceKind) -> String {
    use crate::azure::resources::ResourceKind;
    let enter_clue = match kind {
        ResourceKind::Apim => "Enter apis",
        ResourceKind::AppGateway => "Enter backends",
        // Enter on a section pops its details modal (see `render_modal`),
        // expanding any inline `+N more`. j/k moves between sections, so it's
        // worth surfacing here too.
        ResourceKind::FunctionApp | ResourceKind::WebApp | ResourceKind::ContainerApp => {
            "j/k section  Enter details"
        }
    };
    format!("0 1h  1 1d  7 7d  l logs  {enter_clue}  Esc back  r refresh  ? help  q quit")
}

const ROW_KINDS: [MetricKind; 5] = [
    MetricKind::Traffic,
    MetricKind::Errors,
    MetricKind::ClientErrors,
    MetricKind::Cpu,
    MetricKind::Memory,
];

/// Function Apps add an Executions row (App Insights invocation counts, any
/// trigger type) right under Requests — the platform Requests metric never
/// sees event-triggered work, so on a blob/queue-triggered app it shows only
/// Always On / probe noise. See [`crate::azure::executions`].
const FUNCTION_APP_ROW_KINDS: [MetricKind; 6] = [
    MetricKind::Traffic,
    MetricKind::Executions,
    MetricKind::Errors,
    MetricKind::ClientErrors,
    MetricKind::Cpu,
    MetricKind::Memory,
];

fn row_kinds(kind: ResourceKind) -> &'static [MetricKind] {
    match kind {
        ResourceKind::FunctionApp => &FUNCTION_APP_ROW_KINDS,
        _ => &ROW_KINDS,
    }
}

/// Row label for a metric. Container App CPU/Memory are tagged `avg/replica`
/// because the plotted series is the average across replicas (not a sum) — see
/// [`crate::azure::metrics::MetricSeries::peak_replica`] for the busiest-replica
/// counterpart shown in the summary.
fn metric_row_label(kind: MetricKind, resource_kind: ResourceKind) -> &'static str {
    match (kind, resource_kind) {
        (MetricKind::Cpu, ResourceKind::ContainerApp) => "CPU (avg/replica)",
        (MetricKind::Memory, ResourceKind::ContainerApp) => "Memory (avg/replica)",
        (MetricKind::Traffic, _) => "Requests",
        (MetricKind::Executions, _) => "Executions",
        (MetricKind::Errors, _) => "Http 5xx",
        (MetricKind::ClientErrors, _) => "Http 4xx",
        (MetricKind::Cpu, _) => "CPU",
        (MetricKind::Memory, _) => "Memory",
        // SQL-only kinds are never charted in the Apis Detail view (they belong
        // to `super::sql_detail`), but the match must stay total.
        (MetricKind::Dtu, _) => "eDTU",
        (MetricKind::Storage, _) => "Storage",
        (MetricKind::Workers, _) => "Workers",
    }
}

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(area);

    let selected = state.selected_resource();

    // Header
    let title = Paragraph::new(Line::from(vec![
        Span::styled(
            " detail ",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            selected
                .map(|r| r.name.as_str())
                .unwrap_or("(no selection)"),
            Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
        ),
    ]));
    frame.render_widget(title, chunks[0]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(" overview ", Style::default().fg(theme.fg)));
    let inner = block.inner(chunks[1]);
    frame.render_widget(block, chunks[1]);

    let Some(resource) = selected else {
        let p = Paragraph::new(Line::from(Span::styled(
            "no resource selected.",
            Style::default().fg(theme.muted),
        )));
        frame.render_widget(p, inner);
        render_footer(frame, chunks[2], theme, FOOTER_HINT_BASE);
        return;
    };

    let metrics_opt = state.metrics.by_resource.get(&resource.id);
    let failure = state.metrics.failures.get(&resource.id);
    let availability = state.health.by_resource.get(&resource.id).map(|a| a.state);
    // The badge derives from the fixed-24h health metrics + Resource Health, NOT
    // the chart series (`metrics_opt`, which follow the selected range). Same
    // RH-first rule as the list rows (`list::badge_for_row`): lead on the fast
    // availability signal and render its verdict as soon as it lands, then refine
    // once the slower metrics resolve. `badge_settled` is false while still
    // provisional so the dot renders hollow, like the list.
    let health_metrics = state.health.metrics.get(&resource.id);
    let metrics_resolved =
        health_metrics.is_some() || state.health.metrics_failures.contains_key(&resource.id);
    let availability_resolved = state.health.by_resource.contains_key(&resource.id)
        || state.health.failures.contains_key(&resource.id);
    let (badge_color, badge_label, badge_settled) = if !availability_resolved {
        (theme.muted, "LOADING", false)
    } else if state.health.metrics_failures.contains_key(&resource.id) {
        (theme.critical, "ERROR", metrics_resolved)
    } else {
        let m: &[MetricSeries] = health_metrics.map(|v| v.as_slice()).unwrap_or(&[]);
        let h = derive(m, resource.state.as_deref(), availability);
        (color_for_health(h, theme), h.label(), metrics_resolved)
    };

    // The second header line either reports an error or surfaces the resource's
    // lifecycle state. When it's a state line, the label stays muted but the
    // value picks up a colour via `state_color` so the user can clock "is this
    // thing actually running?" at a glance instead of squinting at grey text.
    let second_line: Line = match failure {
        Some(msg) => Line::from(Span::styled(
            format!("metrics error: {msg}"),
            Style::default().fg(theme.critical),
        )),
        None => {
            let raw_state = resource.state.as_deref().unwrap_or("unknown");
            Line::from(vec![
                Span::styled("state: ", Style::default().fg(theme.muted)),
                Span::styled(
                    raw_state.to_string(),
                    Style::default()
                        .fg(state_color(raw_state, theme))
                        .add_modifier(if raw_state == "Running" {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
            ])
        }
    };
    // Plain-text form used only to reserve enough wrapped rows in the layout
    // below. Must match the visible text shape of `second_line`.
    let second_line_text = match failure {
        Some(msg) => format!("metrics error: {msg}"),
        None => format!("state: {}", resource.state.as_deref().unwrap_or("unknown")),
    };

    // Container-App-only extras: pulled from the revisions + container app
    // fetches. None of these are critical; missing data just collapses the
    // corresponding line.
    let revision_meta = state.revision_meta.by_resource.get(&resource.id);
    let limits = state.container_app_overview.by_resource.get(&resource.id);
    // For Container Apps the overview + revision metadata loads after the first
    // frame. Render skeleton placeholders for the meta block while either is
    // missing so the rest of the page (tags / created / modified) doesn't get
    // pushed down once the data lands. Skeleton rows render in muted gray.
    let is_ca = resource.kind == ResourceKind::ContainerApp;
    // Web Apps share the Function App meta block (image / runtime / network) —
    // same ARM surface — but not the triggers section below.
    let is_site = matches!(
        resource.kind,
        ResourceKind::FunctionApp | ResourceKind::WebApp
    );
    let is_fa = resource.kind == ResourceKind::FunctionApp;
    let is_apim = resource.kind == ResourceKind::Apim;
    let ca_meta_loading = is_ca && (revision_meta.is_none() || limits.is_none());
    let (meta_lines, meta_is_skeleton) = if ca_meta_loading {
        (container_app_skeleton_meta_rows(), true)
    } else if is_site {
        // Function Apps / Web Apps get their own (lighter) meta block: deployed
        // image + runtime + network posture. No skeleton — the rows appear as
        // their backing data lands.
        (function_app_meta_lines(state, resource), false)
    } else if is_apim {
        // APIM: gateway URL + virtual IP addresses, straight off the list fetch.
        (apim_meta_lines(resource), false)
    } else {
        (container_app_meta_lines(revision_meta, limits), false)
    };
    // Live replica status (per-replica container readiness / restarts). Fed by
    // the `…/revisions/{rev}/replicas` fetch that chains off the revision-meta
    // load. Only Container Apps populate this cache, so non-CAs collapse to no
    // rows regardless of the kind check.
    let cached_replicas = state.replica_instances.by_resource.get(&resource.id);
    let replicas_loading_skeleton = is_ca && cached_replicas.is_none();
    let (replica_lines, replica_is_skeleton) = if replicas_loading_skeleton {
        (
            vec![(
                "instances:".to_string(),
                "loading\u{2026}".to_string(),
                "instances: loading\u{2026}".to_string(),
            )],
            true,
        )
    } else {
        (
            replica_status_lines(
                cached_replicas,
                state.replica_instances.pending.contains(&resource.id),
                state.replica_instances.failures.get(&resource.id),
            ),
            false,
        )
    };
    // Function App per-function triggers (kind + what they listen to), from the
    // functions list. FA-only; collapses to nothing for other kinds and when the
    // app has no synced functions. Loading / failure surface as a single hint.
    let trigger_lines = if is_fa {
        function_trigger_lines(
            state.func_triggers.by_resource.get(&resource.id),
            state.func_triggers.pending.contains(&resource.id),
            state.func_triggers.failures.get(&resource.id),
        )
    } else {
        Vec::new()
    };
    // Env vars (Container Apps + Function Apps), masked unless revealed, then
    // the kind-agnostic tags + ownership lines. Skip env_var_rows entirely for
    // Container Apps while the overview is loading — the skeleton meta block
    // above already reserved a row for it, so re-emitting would double the line.
    let env_rows = if ca_meta_loading {
        Vec::new()
    } else {
        env_var_rows(state, resource, theme)
    };
    let general_lines = general_meta_lines(resource, &state.principals);

    // Reserve enough rows for the header line + however many rows the second
    // line needs after wrapping at the available width. Without this, long
    // error messages get clipped and the user can't read the diagnostic.
    let mut context_height = 1 + wrapped_line_count(&second_line_text, inner.width).max(1);
    // Each meta line already wraps independently; reserve worst-case rows so
    // none clip.
    for (_, _, plain_text) in &meta_lines {
        context_height += wrapped_line_count(plain_text, inner.width).max(1);
    }
    for (_, _, plain_text) in &replica_lines {
        context_height += wrapped_line_count(plain_text, inner.width).max(1);
    }
    for (_, _, plain_text) in &trigger_lines {
        context_height += wrapped_line_count(plain_text, inner.width).max(1);
    }
    for (_, plain_text) in &env_rows {
        context_height += wrapped_line_count(plain_text, inner.width).max(1);
    }
    for (_, _, plain_text) in &general_lines {
        context_height += wrapped_line_count(plain_text, inner.width).max(1);
    }
    // Clamp so a long *revealed* env-var list can't starve the sparkline grid
    // entirely — keep at least a few rows for the metrics area. The header /
    // state lines always fit since the clamp floor (3) covers them.
    let max_context = (inner.height.saturating_sub(6)).max(3) as usize;
    let context_height = context_height.min(max_context);
    let body = Layout::vertical([
        Constraint::Length(context_height as u16),
        Constraint::Min(0),
    ])
    .split(inner);

    // 5xx presence flag, shown next to the badge whenever the 24h window had any
    // server errors — independent of the HEALTHY/DEGRADED verdict (an app can be
    // under the error-ratio thresholds yet still throwing 500s worth a look).
    let errors_5xx = state
        .health
        .metrics
        .get(&resource.id)
        .map(|m| crate::azure::health::errors_total(m))
        .unwrap_or(0.0);

    let mut header_spans: Vec<Span> = vec![
        Span::styled(&resource.resource_group, Style::default().fg(theme.muted)),
        Span::styled(" · ", Style::default().fg(theme.muted)),
        Span::styled(resource.kind.short_tag(), Style::default().fg(theme.accent)),
        Span::styled(" · ", Style::default().fg(theme.muted)),
        Span::styled(
            if badge_settled { "●" } else { "◌" },
            Style::default().fg(badge_color),
        ),
        Span::raw(" "),
        Span::styled(badge_label, Style::default().fg(badge_color)),
    ];
    if errors_5xx > 0.0 {
        header_spans.push(Span::styled(" · ", Style::default().fg(theme.muted)));
        header_spans.push(Span::styled(
            format!("5xx {}", format_count(errors_5xx)),
            Style::default().fg(theme.degraded),
        ));
    }
    header_spans.push(Span::styled(" · ", Style::default().fg(theme.muted)));
    header_spans.push(Span::styled(
        format!(
            "window {} · per {}",
            state.metrics.range.label(),
            state.metrics.range.pretty_interval()
        ),
        Style::default().fg(theme.fg),
    ));
    header_spans.push(Span::styled(
        // Stays up while *any* of the detail's data is reloading (metrics,
        // health, the Container App overview/revision, or per-replica status),
        // so a refresh keeps the old rows visible behind this hint rather than
        // collapsing them.
        if state.metrics.loading
            || state.health.pending.contains(&resource.id)
            || state.container_app_overview.pending.contains(&resource.id)
            || state.replica_instances.pending.contains(&resource.id)
        {
            "  · refreshing…"
        } else {
            ""
        },
        Style::default().fg(theme.muted),
    ));

    let mut context_lines: Vec<Line> = vec![Line::from(header_spans), second_line];
    // Selectable rows are grouped into *sections*: a labelled head row plus any
    // blank-label continuation rows that follow it (that's how the multi-line
    // triggers / replicas / containers blocks are emitted). `j`/`k` move between
    // sections — not individual lines — and the whole section highlights as one.
    // Each entry holds the context-line indices its section spans. The order and
    // grouping here MUST match [`selectable_metas`] — same conditions, same data
    // sources, same blank-label rule.
    let mut selectable_sections: Vec<Vec<usize>> = Vec::new();
    // State row is its own section when there's no metrics-error overlay.
    if failure.is_none() {
        selectable_sections.push(vec![1]);
    }
    for (label, value, _) in meta_lines {
        let line_idx = context_lines.len();
        let is_head = !is_continuation_label(&label);
        let line = if meta_is_skeleton {
            styled_skeleton_line(label, value, theme)
        } else if label == CA_ISSUE_LABEL {
            // Platform error for a failed revision — paint it critical, like the
            // badge, so "what went wrong" reads as a problem and not a fact.
            styled_meta_line_valued(label, value, theme.critical, theme)
        } else if label == CA_STATUS_LABEL {
            let color = revision_state_color(&value, theme);
            styled_meta_line_valued(label, value, color, theme)
        } else {
            styled_meta_line(label, value, theme)
        };
        context_lines.push(line);
        if !meta_is_skeleton {
            add_selectable_line(&mut selectable_sections, is_head, line_idx);
        }
    }
    for (label, value, _) in replica_lines {
        let line_idx = context_lines.len();
        let is_head = !is_continuation_label(&label);
        let line = if replica_is_skeleton {
            styled_skeleton_line(label, value, theme)
        } else {
            styled_meta_line(label, value, theme)
        };
        context_lines.push(line);
        if !replica_is_skeleton {
            add_selectable_line(&mut selectable_sections, is_head, line_idx);
        }
    }
    for (label, value, _) in trigger_lines {
        let line_idx = context_lines.len();
        let is_head = !is_continuation_label(&label);
        context_lines.push(styled_meta_line(label, value, theme));
        add_selectable_line(&mut selectable_sections, is_head, line_idx);
    }
    for (line, _) in env_rows {
        let line_idx = context_lines.len();
        context_lines.push(line);
        // The env-vars teaser is always its own single-line section.
        add_selectable_line(&mut selectable_sections, true, line_idx);
    }
    for (label, value, _) in general_lines {
        let line_idx = context_lines.len();
        let is_head = !is_continuation_label(&label);
        context_lines.push(styled_meta_line(label, value, theme));
        add_selectable_line(&mut selectable_sections, is_head, line_idx);
    }

    // Highlight every line of the section under the cursor. The cursor lives in
    // `DetailView` and indexes the *section* list, so we clamp here and patch
    // spans on each line the section spans. Clamping is read-only — the handler
    // keeps `state.detail_view.cursor` in range across data shape changes.
    if !selectable_sections.is_empty() {
        let cursor = state.detail_view.cursor.min(selectable_sections.len() - 1);
        if let Some(line_idxs) = selectable_sections.get(cursor) {
            let hl = theme.selection();
            for &idx in line_idxs {
                if let Some(line) = context_lines.get_mut(idx) {
                    for span in line.spans.iter_mut() {
                        span.style = span.style.patch(hl);
                    }
                }
            }
        }
    }

    // The context pane is a fixed-height Paragraph with no implicit scroll, so on
    // short terminals a richly-populated Container App overflows `max_context`
    // (above) and its lower sections clip — yet `j`/`k` still move the cursor
    // into them. Compute a scroll offset that pulls the cursor's section into
    // view. Row positions use the same char-based wrap approximation as the
    // height reservation, so the offset tracks what's actually drawn.
    let context_scroll = if selectable_sections.is_empty() {
        0u16
    } else {
        let cursor = state.detail_view.cursor.min(selectable_sections.len() - 1);
        // Wrapped-row offset at the start of each context line (plus a final
        // total), built from each line's rendered text.
        let mut acc = 0usize;
        let mut row_starts: Vec<usize> = Vec::with_capacity(context_lines.len() + 1);
        for line in &context_lines {
            row_starts.push(acc);
            let plain: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            acc += wrapped_line_count(&plain, inner.width).max(1);
        }
        row_starts.push(acc);
        let visible = body[0].height as usize;
        selectable_sections
            .get(cursor)
            .and_then(|idxs| {
                let top = row_starts[*idxs.first()?];
                let bottom = row_starts[*idxs.last()? + 1];
                // Scroll down just enough to reveal the section's bottom, then
                // back up if that would hide its top (taller-than-pane sections
                // pin to their top).
                let mut scroll = bottom.saturating_sub(visible);
                if top < scroll {
                    scroll = top;
                }
                Some(scroll.min(u16::MAX as usize) as u16)
            })
            .unwrap_or(0)
    };

    let context = Paragraph::new(context_lines)
        .wrap(Wrap { trim: false })
        .scroll((context_scroll, 0));
    frame.render_widget(context, body[0]);

    // Sparkline grid: one fixed-height row per metric (1 label line + 2 bars),
    // plus a single shared time-axis row at the bottom. All sparklines span
    // the same window, so one axis serves the whole grid. Function Apps carry
    // one extra row (Executions), hence the dynamic constraint list.
    let kinds = row_kinds(resource.kind);
    let mut row_constraints = vec![Constraint::Length(3); kinds.len()];
    row_constraints.push(Constraint::Length(1));
    let metric_rows = Layout::vertical(row_constraints).split(body[1]);

    let missing_for_resource = state.metrics.missing.get(&resource.id);
    let limits = state.container_app_overview.by_resource.get(&resource.id);
    for (i, kind) in kinds.iter().enumerate() {
        let area = metric_rows[i];
        if area.height == 0 {
            continue;
        }
        let missing_reason = missing_for_resource.and_then(|m| m.get(kind));
        let label = metric_row_label(*kind, resource.kind);
        render_metric_row(
            frame,
            area,
            *kind,
            label,
            metrics_opt,
            missing_reason,
            limits,
            state,
            theme,
        );
    }

    if metric_rows[kinds.len()].height > 0 {
        super::metric_chart::render_time_axis(
            frame,
            metric_rows[kinds.len()],
            state.metrics.range,
            theme,
        );
    }

    let mut hint = footer_hint_for(resource.kind);
    if matches!(
        resource.kind,
        ResourceKind::ContainerApp | ResourceKind::FunctionApp | ResourceKind::WebApp
    ) {
        hint = format!("e env vars  {hint}");
    }
    render_footer(frame, chunks[2], theme, &hint);
}

/// How many terminal rows `text` will occupy after `Paragraph` wrapping with
/// `Wrap { trim: false }` at the given width.
///
/// Mirrors ratatui's `WordWrapper`: greedy wrap at whitespace (a word that
/// would overflow moves wholesale to the next row, and whitespace spilling
/// past the break is dropped), words wider than the pane hard-split at the
/// width boundary, and everything is measured in *display* columns so
/// double-width glyphs (CJK) count as two. The old `ceil(chars / width)`
/// estimate under-counted both cases — word wrap breaks *earlier* than a char
/// count suggests on space-heavy lines (tags), and wide glyphs occupy two
/// columns each — which clipped the bottom meta rows and desynced the
/// cursor-follow offsets built from these counts. Hard `\n`s each start a new
/// row. Shared with the sibling scrollable panes/modals so every scroll clamp
/// agrees with what `Paragraph` actually draws.
pub(crate) fn wrapped_line_count(text: &str, width: u16) -> usize {
    let max_w = width.max(1) as usize;
    text.split('\n')
        .map(|line| wrapped_rows_one_line(line, max_w))
        .sum::<usize>()
        .max(1)
}

/// One hard line's wrapped row count — the greedy word-wrap simulation behind
/// [`wrapped_line_count`].
fn wrapped_rows_one_line(line: &str, max_w: usize) -> usize {
    let mut rows = 1usize;
    let mut col = 0usize;
    // Walk alternating whitespace / word tokens (maximal same-class runs).
    let mut rest = line;
    while let Some(first) = rest.chars().next() {
        let is_ws = first.is_whitespace();
        let split = rest
            .char_indices()
            .find(|&(_, c)| c.is_whitespace() != is_ws)
            .map_or(rest.len(), |(i, _)| i);
        let (token, tail) = rest.split_at(split);
        rest = tail;
        let tw = display_width(token);
        if is_ws {
            // Whitespace flows up to the row edge; the spill is exactly what
            // ratatui drops at a wrap point, so it never opens a new row.
            col = (col + tw).min(max_w);
        } else if col + tw <= max_w {
            col += tw;
        } else if tw <= max_w {
            // Greedy word wrap: an overflowing word moves wholesale to a
            // fresh row (the break's whitespace was absorbed above).
            rows += 1;
            col = tw;
        } else {
            // Word wider than the pane: starts on a fresh row, then splits at
            // display-width boundaries. Counted per char because a wide glyph
            // that would straddle the edge wraps early — dividing the total
            // width would under-count those rows.
            if col > 0 {
                rows += 1;
                col = 0;
            }
            for ch in token.chars() {
                let cw = display_width(ch.encode_utf8(&mut [0u8; 4]));
                // ratatui skips zero-width symbols and ones wider than the
                // whole line rather than wrapping on them.
                if cw == 0 || cw > max_w {
                    continue;
                }
                if col + cw > max_w {
                    rows += 1;
                    col = cw;
                } else {
                    col += cw;
                }
            }
        }
    }
    rows
}

/// Display-column width of `s`, measured through ratatui's own unicode-width
/// (`Span::width`) so the estimate can't disagree with what `Paragraph` draws.
fn display_width(s: &str) -> usize {
    Span::raw(s).width()
}

fn render_footer(frame: &mut Frame, area: Rect, theme: &Theme, hint: &str) {
    let p = Paragraph::new(Line::from(Span::styled(
        hint,
        Style::default().fg(theme.muted),
    )));
    frame.render_widget(p, area);
}

#[allow(clippy::too_many_arguments)]
fn render_metric_row(
    frame: &mut Frame,
    area: Rect,
    kind: MetricKind,
    label: &str,
    metrics: Option<&Vec<MetricSeries>>,
    missing_reason: Option<&String>,
    limits: Option<&crate::azure::container_app_overview::ContainerAppOverview>,
    state: &AppState,
    theme: &Theme,
) {
    let series = metrics.and_then(|m| find(m, kind));

    // The summary line is the only resource-specific bit; the rest (title
    // layout, sparkline, placeholder) is shared with the SQL detail view via
    // `metric_chart`.
    let summary = match series {
        Some(s) => summary_for(kind, s, limits),
        None if state.metrics.loading => "loading…".to_string(),
        None if missing_reason.is_some() => "n/a".to_string(),
        None => "—".to_string(),
    };

    super::metric_chart::render_chart_row(
        frame,
        area,
        kind,
        label,
        series,
        &summary,
        missing_reason,
        theme,
    );
}

/// Meta-row labels for a failing active revision (running status + the
/// platform's error detail). Kept as consts so [`container_app_meta_lines`]
/// (the producer) and [`render`]'s colour branch can't drift — both match on
/// these exact strings to paint the rows in the badge's severity colour rather
/// than the neutral accent every other meta row uses.
const CA_STATUS_LABEL: &str = "status:";
const CA_ISSUE_LABEL: &str = "issue:";

/// Build the Container-App-only meta lines that sit below `state:` in the
/// Detail header. Each entry is `(label, value, plain_text)`: the first two
/// drive styled rendering (bold muted label + accent value), the third is the
/// concatenated plain string used only for wrap-aware height reservation.
/// Labels are owned strings so multi-row sections (containers, replicas) can
/// emit blank-padded continuation labels for column alignment without `'static`
/// lifetime gymnastics.
///
/// Missing pieces are skipped: no revision data → no lines; no image → no
/// image line; no ingress fqdn → no fqdn line.
fn container_app_meta_lines(
    revision_meta: Option<&crate::azure::container_app_revisions::ActiveRevisionMeta>,
    limits: Option<&crate::azure::container_app_overview::ContainerAppOverview>,
) -> Vec<(String, String, String)> {
    let mut out: Vec<(String, String, String)> = Vec::new();

    if let Some(m) = revision_meta {
        if !m.name.is_empty() {
            out.push(("rev:".into(), m.name.clone(), format!("rev: {}", m.name)));
        }
        // Surface a failing revision right under its name, the way the portal's
        // "Revisions with Issues" table does: the running status, then the
        // platform's "running status details". Skipped entirely for a healthy
        // `Running` revision so steady-state apps stay uncluttered. These two
        // labels are colour-keyed by [`render`]'s meta loop.
        if !m.running_state.is_empty() && m.running_state != "Running" {
            out.push((
                CA_STATUS_LABEL.into(),
                m.running_state.clone(),
                format!("{CA_STATUS_LABEL} {}", m.running_state),
            ));
        }
        if let Some(err) = &m.provisioning_error {
            out.push((
                CA_ISSUE_LABEL.into(),
                err.clone(),
                format!("{CA_ISSUE_LABEL} {err}"),
            ));
        }
        if let Some(img) = &m.image {
            // The header shows the primary container's image. When the revision
            // defines more containers (sidecars / init), tag a `(+N more)` so a
            // multi-container app isn't misrepresented as single-image — the
            // full per-container list lives in the `container config:` block.
            let extra = limits
                .map(|l| l.containers.len().saturating_sub(1))
                .unwrap_or(0);
            let value = if extra > 0 {
                format!("{img}  (+{extra} more)")
            } else {
                img.clone()
            };
            let plain = format!("image: {value}");
            out.push(("image:".into(), value, plain));
        }
        let replicas_value = match (m.min_replicas, m.max_replicas) {
            (0, 0) => format!("{}", m.replicas),
            (min, max) => format!("{} of {min}\u{2013}{max}", m.replicas),
        };
        let plain = format!("scale: {replicas_value}");
        out.push(("scale:".into(), replicas_value, plain));
    }

    if let Some(l) = limits {
        if !l.containers.is_empty() {
            push_template_container_rows(&mut out, &l.containers);
        }
        if let Some(fqdn) = l.fqdn.as_deref() {
            out.push(("fqdn:".into(), fqdn.to_string(), format!("fqdn: {fqdn}")));
        }
        // Ingress posture — the Container App network exposure (the analogue of
        // the Function App `network:` row, expressed via ingress).
        let network = match l.ingress_external {
            None => "no ingress",
            Some(false) => "internal ingress (VNet only)",
            Some(true) if l.access_restricted => "external ingress (IP restricted)",
            Some(true) => "external ingress (public)",
        };
        out.push((
            "network:".into(),
            network.to_string(),
            format!("network: {network}"),
        ));
        if let Some(env) = l.managed_environment.as_deref() {
            out.push((
                "environment:".into(),
                env.to_string(),
                format!("environment: {env}"),
            ));
        }
        if let Some(id) = l.managed_identity.as_deref() {
            out.push((
                "identity:".into(),
                id.to_string(),
                format!("identity: {id}"),
            ));
        }
    }

    out
}

/// Emit a block of rows per template container: a name *header* row that owns
/// the `container config:` label (blank-padded on later containers so they stack
/// in one column), followed by indented `image` / `cpu/mem` / `ephemeral`
/// attribute sub-rows (the last only when the container reports ephemeral
/// storage).
/// Breaking each container across sub-rows keeps the values readable instead of
/// packing name + image + resources onto one wide line. These are the
/// *configured* containers of the active revision (the template) — distinct from
/// the live `instances:` block, which reflects the running replicas.
fn push_template_container_rows(
    out: &mut Vec<(String, String, String)>,
    containers: &[crate::azure::container_app_overview::ContainerSpec],
) {
    const LABEL: &str = "container config:";
    // Attribute names for the indented sub-rows, padded to a common column so
    // their values line up. `ephemeral` only appears for containers that report
    // it, but it widens the column for the whole block when any do.
    const ATTR_IMAGE: &str = "image";
    const ATTR_RES: &str = "cpu/mem";
    const ATTR_EPH: &str = "ephemeral";
    let blank_label: String = " ".repeat(LABEL.chars().count());
    let mut attr_width = ATTR_IMAGE.chars().count().max(ATTR_RES.chars().count());
    if containers.iter().any(|c| c.ephemeral_storage.is_some()) {
        attr_width = attr_width.max(ATTR_EPH.chars().count());
    }

    // Push one sub-row: a 2-space indent, the padded attribute name, then value.
    let push_sub = |out: &mut Vec<(String, String, String)>, attr: &str, val: &str| {
        let value = format!("  {attr:<attr_width$}  {val}");
        let plain = format!("{blank_label} {value}");
        out.push((blank_label.clone(), value, plain));
    };

    for (idx, c) in containers.iter().enumerate() {
        // Name header row. Tag init containers so a `something ✓` in the
        // replica's container list that comes from `initContainers` doesn't
        // look like a missing entry up here in the template.
        let suffix = if c.is_init { "  (init)" } else { "" };
        let name_value = format!("{}{suffix}", c.name);
        let label = if idx == 0 {
            LABEL.to_string()
        } else {
            blank_label.clone()
        };
        let plain = format!("{label} {name_value}");
        out.push((label, name_value, plain));

        // Indented attribute sub-rows beneath the name.
        let image = c.image.as_deref().unwrap_or("\u{2014}"); // em dash for missing
        push_sub(out, ATTR_IMAGE, image);
        let res = format!(
            "{} mCores \u{00b7} {}",
            c.cpu_millicores,
            format_bytes(c.memory_bytes as f64)
        );
        push_sub(out, ATTR_RES, &res);
        // Ephemeral storage is per container; only emit a row when the container
        // actually reports it (some payloads omit it entirely).
        if let Some(eph) = c.ephemeral_storage.as_deref() {
            push_sub(out, ATTR_EPH, eph);
        }
    }
}

/// Emit a block of rows per live replica: a header row carrying the replica
/// name minus its app-name prefix (which repeats on every replica and is
/// already the page title) plus its aggregate `runningState`
/// in parens, followed by one indented sub-row per container with its readiness
/// glyph and restart count. This mirrors the `container config:` block above so
/// the live runtime reads the same way as the configured template. Sorted
/// newest-first by `created_at`;
/// capped at 10 replicas so a scaled-out app can't blow the Detail header.
///
/// Returns an empty Vec when there are no replicas (e.g. revision with
/// `replicas: 0`); a single hint row when the fetch is pending or has failed.
fn replica_status_lines(
    replicas: Option<&Vec<crate::azure::container_app_replicas::ReplicaInstance>>,
    pending: bool,
    failure: Option<&String>,
) -> Vec<(String, String, String)> {
    // Labelled `instances:` (not `replicas:`) to keep the live running pods
    // distinct from the configured `scale:` count — a scaled-out app would
    // otherwise show two `replicas:` rows.
    const LABEL: &str = "instances:";
    // Inline preview is capped at this many *replicas* (each now spans a header
    // row plus a sub-row per container); the rest fold into a `+N more` row and
    // the full list lives in the Enter modal.
    const CAP: usize = 10;

    // Cached data wins over a transient error so a stale-but-useful row
    // sticks around across blips; pure-error state shows the hint.
    if let Some(list) = replicas {
        if list.is_empty() {
            return Vec::new();
        }

        let mut sorted: Vec<&crate::azure::container_app_replicas::ReplicaInstance> =
            list.iter().collect();
        // Newest first: `None` created_at sorts to the end.
        sorted.sort_by_key(|r| std::cmp::Reverse(r.created_at));

        let total = sorted.len();
        let shown = total.min(CAP);
        let blank_label: String = " ".repeat(LABEL.chars().count());

        // Column-align the container names across every shown replica so the
        // readiness glyphs and restart counts stack in one column down the whole
        // block (the same alignment trick the `container config:` block uses).
        let name_width = sorted
            .iter()
            .take(shown)
            .flat_map(|r| r.containers.iter())
            .map(|c| container_display_name(c).chars().count())
            .max()
            .unwrap_or(0);

        let mut out: Vec<(String, String, String)> = Vec::new();
        for (idx, r) in sorted.iter().take(shown).enumerate() {
            // Replica header row — owns the `instances:` label on the first
            // replica, blank-padded thereafter so the block stacks in one column.
            // The replica's aggregate `runningState` is appended in parens. This
            // is Azure's *replica*-level state, distinct from the per-container
            // readiness glyphs below it, and the two can disagree — e.g. a
            // scaled-to-zero app parks a replica as `NotRunning` while its
            // container's last snapshot is still `Running`/ready.
            let suffix = short_replica_name(&r.name);
            let name = match r.running_state.as_deref() {
                Some(state) => format!("{suffix}  ({state})"),
                None => suffix,
            };
            let row_label = if idx == 0 {
                LABEL.to_string()
            } else {
                blank_label.clone()
            };
            let plain = format!("{row_label} {name}");
            out.push((row_label, name, plain));

            // One indented sub-row per container: padded name, readiness glyph,
            // its own restart count, and — when a container is stuck — a short
            // reason (e.g. `image pull failed`) so the "why" is visible without
            // opening the portal.
            for c in &r.containers {
                let cname = container_display_name(c);
                let glyph = ready_glyph(c.ready);
                let mut value = format!(
                    "  {cname:<name_width$}  {glyph}  restarts {}",
                    c.restart_count
                );
                if let Some(reason) = container_blocked_reason(c) {
                    value.push_str(&format!("  \u{00b7} {reason}"));
                }
                let plain = format!("{blank_label} {value}");
                out.push((blank_label.clone(), value, plain));
            }
        }
        if total > shown {
            let extra = total - shown;
            let value = format!("\u{2026} +{extra} more");
            out.push((
                blank_label.clone(),
                value.clone(),
                format!("{blank_label} {value}"),
            ));
        }
        return out;
    }

    if pending {
        let plain = format!("{LABEL} loading\u{2026}");
        return vec![(LABEL.into(), "loading\u{2026}".into(), plain)];
    }
    if let Some(msg) = failure {
        let value = short_replica_failure(msg);
        let plain = format!("{LABEL} {value}");
        return vec![(LABEL.into(), value, plain)];
    }
    Vec::new()
}

/// Trim a full replica name (`<app>--<revsuffix>-<hash>-<random>`) to the part
/// after the `--` separator. The app-name prefix repeats on every replica and
/// is already the page title, so dropping it loses nothing — while the kept
/// `<revsuffix>-<hash>-<random>` still says which revision the replica belongs
/// to and needs no ellipsis. Falls back to the full name if there's no `--`.
fn short_replica_name(full: &str) -> String {
    match full.split_once("--") {
        Some((_, tail)) if !tail.is_empty() => tail.to_string(),
        _ => full.to_string(),
    }
}

/// Glyph for a container's readiness probe: `✓` Ready, `✗` not Ready, `?` when
/// the state is unknown (container still initialising or no probe configured).
fn ready_glyph(ready: Option<bool>) -> char {
    match ready {
        Some(true) => '\u{2713}',  // ✓
        Some(false) => '\u{2717}', // ✗
        None => '?',
    }
}

/// Short, human reason a container isn't up — distilled from its
/// `runningStateDetails`. `None` for a `Running` container or when Azure
/// reported no detail. Used to annotate the inline instances rows so a stuck
/// replica explains itself (most usefully: an image-pull failure) instead of
/// just showing `✗ restarts 0`.
fn container_blocked_reason(
    c: &crate::azure::container_app_replicas::ReplicaContainer,
) -> Option<String> {
    if c.running_state
        .as_deref()
        .unwrap_or("")
        .eq_ignore_ascii_case("Running")
    {
        return None;
    }
    let details = c.running_state_details.as_deref()?.trim();
    if details.is_empty() {
        return None;
    }
    Some(summarize_container_state_details(details))
}

/// Map a raw `runningStateDetails` string to a short tag, falling back to a
/// truncated snippet of the original when it doesn't match a known failure
/// shape. Kept conservative: only collapses the well-known kubelet phrasings.
fn summarize_container_state_details(details: &str) -> String {
    let lower = details.to_lowercase();
    let image_pull = lower.contains("imagepull")
        || lower.contains("errimagepull")
        || lower.contains("pulling image")
        || lower.contains("manifest")
        || (lower.contains("image")
            && (lower.contains("not found") || lower.contains("access denied")));
    if image_pull {
        return "image pull failed".to_string();
    }
    if lower.contains("crashloop") {
        return "crash loop".to_string();
    }
    if lower.contains("oomkill") || lower.contains("out of memory") {
        return "out of memory".to_string();
    }
    // Unknown shape: show a trimmed snippet so the user still sees *something*.
    let snippet: String = details.chars().take(60).collect();
    if details.chars().count() > 60 {
        format!("{snippet}\u{2026}")
    } else {
        snippet
    }
}

/// A container's display name, falling back to `?` for the (degenerate) empty
/// name some replica payloads carry.
fn container_display_name(c: &crate::azure::container_app_replicas::ReplicaContainer) -> &str {
    if c.name.is_empty() {
        "?"
    } else {
        c.name.as_str()
    }
}

/// Translate a raw replicas-endpoint failure into a short, plain-language
/// hint. Same one-line philosophy as `short_missing_reason` for metrics.
fn short_replica_failure(reason: &str) -> String {
    if reason.contains("403") || reason.contains("Forbidden") {
        "unavailable (permission denied)".to_string()
    } else if reason.contains("404") {
        "unavailable (revision not found)".to_string()
    } else {
        let one_line: String = reason.chars().take(80).collect();
        if reason.chars().count() > 80 {
            format!("unavailable ({one_line}\u{2026})")
        } else {
            format!("unavailable ({one_line})")
        }
    }
}

/// Cap on the number of per-function trigger rows shown in the Detail overview;
/// an app with more functions than this gets a `+N more` summary row so a
/// sprawling app can't blow the header height. Shared by [`render`]'s display
/// and [`selectable_metas`] so the two stay aligned.
const TRIGGER_CAP: usize = 12;

/// Function-App-only meta lines for the Detail header: the deployed container
/// image (container-deployed apps) and a runtime summary. Same `(label, value,
/// plain)` tuple shape as [`container_app_meta_lines`]; absent data collapses
/// (no line). Both rows are free — they read from caches already populated for
/// other reasons (`func_image` for the list's VERSION column, `func_settings`
/// for the env-vars page) rather than firing a dedicated fetch.
fn function_app_meta_lines(state: &AppState, resource: &Resource) -> Vec<(String, String, String)> {
    let mut out: Vec<(String, String, String)> = Vec::new();
    let id = resource.id.as_str();

    // Deployed image. `func_image` caches `Option<String>`: `Some(img)` for
    // container-deployed apps, `Some(None)` for code-deployed (no image line),
    // absent while still loading.
    if let Some(Some(img)) = state.func_image.by_resource.get(id) {
        out.push(("image:".into(), img.clone(), format!("image: {img}")));
    }

    // Worker runtime + Functions host version, distilled from the app settings
    // we already lazy-load. Absent when settings aren't readable (the env-vars
    // row already signals that permission gap, so we don't repeat it here).
    if let Some(vars) = state.func_settings.by_resource.get(id) {
        if let Some(rt) = function_runtime_summary(vars) {
            out.push(("runtime:".into(), rt.clone(), format!("runtime: {rt}")));
        }
    }

    // Public network access posture, mirroring the portal's three states. The
    // Enabled/Disabled toggle rides on the list fetch (no extra call); the
    // "with/without restrictions" detail rides on the same `config/web` fetch
    // that feeds the image (so it's free), and is omitted until that lands.
    let access = if !resource.meta.public_network_enabled() {
        // publicNetworkAccess = Disabled → reachable only via private endpoints.
        "public access disabled"
    } else {
        match state.func_image.access_restricted.get(id).copied() {
            // "Enabled from select virtual networks and IP addresses".
            Some(true) => "public access enabled (IP/VNet restricted)",
            // "Enabled with no access restrictions".
            Some(false) => "public access enabled (no restrictions)",
            // config/web not back yet — show posture without the detail.
            None => "public access enabled",
        }
    };
    out.push((
        "network:".into(),
        access.to_string(),
        format!("network: {access}"),
    ));

    out
}

/// APIM-only meta lines for the Detail header: the gateway endpoint URL plus the
/// service's virtual IP addresses — public VIP(s) always, and private VIP(s)
/// only for internal-VNet services. Same `(label, value, plain)` tuple shape as
/// the other meta builders; absent data collapses (no line). All read off the
/// list fetch — no dedicated APIM call.
fn apim_meta_lines(resource: &Resource) -> Vec<(String, String, String)> {
    let mut out: Vec<(String, String, String)> = Vec::new();
    let m = &resource.meta;

    if let Some(url) = m.gateway_url.as_deref().filter(|s| !s.is_empty()) {
        out.push((
            "gateway:".into(),
            url.to_string(),
            format!("gateway: {url}"),
        ));
    }
    if !m.public_ips.is_empty() {
        let joined = m.public_ips.join(", ");
        out.push((
            "public IP:".into(),
            joined.clone(),
            format!("public IP: {joined}"),
        ));
    }
    if !m.private_ips.is_empty() {
        let joined = m.private_ips.join(", ");
        out.push((
            "private IP:".into(),
            joined.clone(),
            format!("private IP: {joined}"),
        ));
    }

    out
}

/// Distil the worker runtime and Functions host version out of a Function App's
/// app settings into one line, e.g. `python · ~4`. Returns `None` when neither
/// `FUNCTIONS_WORKER_RUNTIME` nor `FUNCTIONS_EXTENSION_VERSION` is present.
fn function_runtime_summary(vars: &[crate::azure::env_vars::EnvVar]) -> Option<String> {
    let find = |key: &str| {
        vars.iter()
            .find(|v| v.name.eq_ignore_ascii_case(key))
            .map(|v| v.value.trim())
            .filter(|s| !s.is_empty())
    };
    match (
        find("FUNCTIONS_WORKER_RUNTIME"),
        find("FUNCTIONS_EXTENSION_VERSION"),
    ) {
        (Some(r), Some(v)) => Some(format!("{r} \u{00b7} {v}")),
        (Some(r), None) => Some(r.to_string()),
        (None, Some(v)) => Some(format!("Functions {v}")),
        (None, None) => None,
    }
}

/// Build the Function-App triggers block for the Detail header: one row per
/// function, sharing a `triggers:` label on the first row (blank continuation
/// labels keep the values in a column). Caps at [`TRIGGER_CAP`] with a `+N more`
/// row. A single hint row is returned while the fetch is pending or after it
/// failed; an empty Vec when the app has no synced functions. Same `(label,
/// value, plain)` tuple shape as the other meta builders.
fn function_trigger_lines(
    triggers: Option<&Vec<crate::azure::function_app_triggers::FunctionTrigger>>,
    pending: bool,
    failure: Option<&String>,
) -> Vec<(String, String, String)> {
    const LABEL: &str = "triggers:";
    let blank_label: String = " ".repeat(LABEL.chars().count());

    // Cached data wins over a transient error so a stale-but-useful list sticks
    // around across blips.
    if let Some(list) = triggers {
        if list.is_empty() {
            return Vec::new();
        }
        let shown = list.len().min(TRIGGER_CAP);
        let max_name = list
            .iter()
            .take(shown)
            .map(|t| t.function.chars().count())
            .max()
            .unwrap_or(0);

        let mut out: Vec<(String, String, String)> = Vec::new();
        for (idx, t) in list.iter().take(shown).enumerate() {
            let name_col = format!("{:<width$}", t.function, width = max_name);
            let kind = if t.kind.is_empty() {
                "\u{2014}".to_string() // em dash: function with no trigger binding
            } else {
                t.kind.clone()
            };
            let value = match &t.detail {
                Some(d) => format!("{name_col}  {kind}: {d}"),
                None => format!("{name_col}  {kind}"),
            };
            let label = if idx == 0 {
                LABEL.to_string()
            } else {
                blank_label.clone()
            };
            let plain = format!("{label} {value}");
            out.push((label, value, plain));
        }
        if list.len() > shown {
            let value = format!("\u{2026} +{} more", list.len() - shown);
            out.push((
                blank_label.clone(),
                value.clone(),
                format!("{blank_label} {value}"),
            ));
        }
        return out;
    }

    if pending {
        return vec![(
            LABEL.into(),
            "loading\u{2026}".into(),
            format!("{LABEL} loading\u{2026}"),
        )];
    }
    if let Some(msg) = failure {
        let value = short_trigger_failure(msg);
        return vec![(LABEL.into(), value.clone(), format!("{LABEL} {value}"))];
    }
    Vec::new()
}

/// One-line, plain-language hint for a failed functions-list fetch. Mirrors
/// [`short_replica_failure`]'s philosophy.
fn short_trigger_failure(reason: &str) -> String {
    if reason.contains("403") || reason.contains("Forbidden") {
        "unavailable (permission denied)".to_string()
    } else {
        let one_line: String = reason.chars().take(80).collect();
        if reason.chars().count() > 80 {
            format!("unavailable ({one_line}\u{2026})")
        } else {
            format!("unavailable ({one_line})")
        }
    }
}

/// Build the meta lines that apply to every resource kind: tags and `systemData`
/// ownership (created/modified by whom). Same `(label, value, plain)` tuple
/// shape as [`container_app_meta_lines`]. Absent data collapses (no line).
///
/// For `Application` / `ManagedIdentity` authors the value is a GUID; we show
/// the Graph-resolved display name when `principals` has it, falling back to the
/// raw GUID otherwise. `User` entries are UPNs/emails and shown verbatim.
fn general_meta_lines(
    resource: &crate::azure::resources::Resource,
    principals: &crate::ui::state::PrincipalCache,
) -> Vec<(String, String, String)> {
    let mut out: Vec<(String, String, String)> = Vec::new();
    let m = &resource.meta;

    if !m.tags.is_empty() {
        let joined = m
            .tags
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(", ");
        out.push(("tags:".into(), joined.clone(), format!("tags: {joined}")));
    }

    let resolved =
        |id: Option<&str>| -> Option<String> { id.and_then(|i| principals.by_id.get(i)).cloned() };
    if let Some(v) = ownership_value(
        resource.created_at.as_ref(),
        m.created_by.as_deref(),
        m.created_by_type.as_deref(),
        resolved(m.created_by.as_deref()).as_deref(),
    ) {
        out.push(("created:".into(), v.clone(), format!("created: {v}")));
    }
    if let Some(v) = ownership_value(
        resource.modified_at.as_ref(),
        m.modified_by.as_deref(),
        m.modified_by_type.as_deref(),
        resolved(m.modified_by.as_deref()).as_deref(),
    ) {
        out.push(("modified:".into(), v.clone(), format!("modified: {v}")));
    }

    out
}

/// Format a `YYYY-MM-DD HH:MM:SS UTC by <who> (<type>)` ownership string.
/// `resolved` is the Graph-resolved display name for GUID principals, if known.
/// Returns `None` when there's neither a timestamp nor an author to show.
fn ownership_value(
    date: Option<&chrono::DateTime<chrono::Utc>>,
    by: Option<&str>,
    by_type: Option<&str>,
    resolved: Option<&str>,
) -> Option<String> {
    // Full UTC timestamp (not just the date) — created/modified are precise.
    let date_s = date.map(|d| d.format("%Y-%m-%d %H:%M:%S UTC").to_string());
    let who = match (by, by_type) {
        // UPN/email is readable — show it with the type.
        (Some(b), Some("User")) => Some(format!("by {b} (User)")),
        // GUID principals: prefer the resolved name, else the GUID itself.
        (Some(b), Some(t @ ("Application" | "ManagedIdentity"))) => {
            Some(format!("by {} ({t})", resolved.unwrap_or(b)))
        }
        // Any other type (e.g. Key): show the value with its type.
        (Some(b), Some(t)) => Some(format!("by {b} ({t})")),
        (Some(b), None) => Some(format!("by {b}")),
        _ => None,
    };
    match (date_s, who) {
        (Some(d), Some(w)) => Some(format!("{d} {w}")),
        (Some(d), None) => Some(d),
        (None, Some(w)) => Some(w),
        (None, None) => None,
    }
}

/// A bold-muted label + accent value line, matching the rest of the overview.
fn styled_meta_line(
    label: impl Into<String>,
    value: impl Into<String>,
    theme: &Theme,
) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            label.into(),
            Style::default()
                .fg(theme.muted)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(value.into(), Style::default().fg(theme.accent)),
    ])
}

/// Same shape as [`styled_meta_line`] but with a caller-chosen value colour.
/// Used for the failing-revision `status:` / `issue:` rows so a broken Container
/// App's reason reads in the health badge's red, not the neutral accent.
fn styled_meta_line_valued(
    label: impl Into<String>,
    value: impl Into<String>,
    value_color: Color,
    theme: &Theme,
) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            label.into(),
            Style::default()
                .fg(theme.muted)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(value.into(), Style::default().fg(value_color)),
    ])
}

/// Same shape as [`styled_meta_line`] but the value is rendered in `theme.muted`
/// rather than `theme.accent`. Used for "loading…" placeholders so the layout
/// reserves space ahead of the real data arriving without those rows screaming
/// for attention in the meantime.
fn styled_skeleton_line(
    label: impl Into<String>,
    value: impl Into<String>,
    theme: &Theme,
) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            label.into(),
            Style::default()
                .fg(theme.muted)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(value.into(), Style::default().fg(theme.muted)),
    ])
}

/// Placeholder rows shown for the Container-App meta block while the
/// `container_app_overview` and/or `revision_meta` caches are still being
/// populated. Mirrors the *shape* of the real meta block (rev / image / scale /
/// container config / env vars / fqdn) so it doesn't pop in from nothing when
/// data lands. It under-reserves height: the loaded view is taller — the
/// container-config and instances blocks each span several rows — so the rows
/// below (tags / created / modified) still shift down somewhat on arrival.
fn container_app_skeleton_meta_rows() -> Vec<(String, String, String)> {
    const FIELDS: &[&str] = &[
        "rev:",
        "image:",
        "scale:",
        "container config:",
        "env vars:",
        "fqdn:",
    ];
    FIELDS
        .iter()
        .map(|label| {
            let value = "loading\u{2026}".to_string();
            let plain = format!("{label} {value}");
            ((*label).to_string(), value, plain)
        })
        .collect()
}

/// Resolve the env vars to display for a resource, regardless of kind. Container
/// Apps carry them on the overview cache (same GET); Function Apps on the
/// dedicated settings cache. `None` means "not loaded / not applicable";
/// `Some(&[])` means the resource genuinely has none. Shared with the dedicated
/// env-vars page ([`crate::ui::views::env_vars`]).
pub(crate) fn env_vars_for<'a>(
    state: &'a AppState,
    id: &str,
    kind: ResourceKind,
) -> Option<&'a [crate::azure::env_vars::EnvVar]> {
    match kind {
        ResourceKind::ContainerApp => state
            .container_app_overview
            .by_resource
            .get(id)
            .map(|l| l.env_vars.as_slice()),
        ResourceKind::FunctionApp | ResourceKind::WebApp => state
            .func_settings
            .by_resource
            .get(id)
            .map(|v| v.as_slice()),
        _ => None,
    }
}

/// One-line env-vars *teaser* for the Detail overview: a count plus a hint to
/// open the dedicated page with `e`. The values themselves live on that page —
/// see [`crate::ui::views::env_vars`]. Returns `(styled line, plain)` pairs
/// (0 or 1 entry) so it slots into the same height accounting as the meta lines.
fn env_var_rows(
    state: &AppState,
    resource: &crate::azure::resources::Resource,
    theme: &Theme,
) -> Vec<(Line<'static>, String)> {
    let id = resource.id.as_str();
    let kind = resource.kind;

    let Some(vars) = env_vars_for(state, id, kind) else {
        // Not loaded yet. Container App env vars ride on the overview fetch
        // (other lines already signal its progress), so only Function Apps /
        // Web Apps get an explicit hint here.
        if matches!(kind, ResourceKind::FunctionApp | ResourceKind::WebApp) {
            if state.func_settings.failures.contains_key(id) {
                return vec![meta_hint_row(
                    "env vars:",
                    "unavailable (needs config/list permission)",
                    theme,
                )];
            }
            if state.func_settings.pending.contains(id) {
                return vec![meta_hint_row("env vars:", "loading…", theme)];
            }
        }
        return Vec::new();
    };
    if vars.is_empty() {
        return Vec::new();
    }

    let value = format!("{}   [e to view]", vars.len());
    vec![(
        styled_meta_line("env vars:", value.clone(), theme),
        format!("env vars: {value}"),
    )]
}

/// A label + muted value row (for hints like "loading…" / permission denied).
fn meta_hint_row(label: &str, value: &str, theme: &Theme) -> (Line<'static>, String) {
    let line = Line::from(vec![
        Span::styled(
            label.to_string(),
            Style::default()
                .fg(theme.muted)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(value.to_string(), Style::default().fg(theme.muted)),
    ]);
    (line, format!("{label} {value}"))
}

fn summary_for(
    kind: MetricKind,
    s: &MetricSeries,
    limits: Option<&crate::azure::container_app_overview::ContainerAppOverview>,
) -> String {
    match kind {
        MetricKind::Traffic
        | MetricKind::Executions
        | MetricKind::Errors
        | MetricKind::ClientErrors => {
            let total = s.sum();
            format!("total: {}{}", format_count(total), unit_suffix(s))
        }
        // CPU / Memory read `latest / highest / max`: the most recent sample,
        // the window peak (what the tallest sparkline bar represents — without
        // it the chart has no readable scale), and the configured limit.
        MetricKind::Cpu => {
            let latest = s.latest().unwrap_or(0.0);
            // Window peak; the `.max(0.0)` turns an empty series' -inf into 0.
            let highest = s.max().max(0.0);
            let suffix = unit_suffix(s);
            let mut base = format!(
                "latest: {}{suffix} / highest: {}{suffix}",
                format_value(latest),
                format_value(highest),
            );
            // Busiest single replica over the window — only meaningful when it
            // exceeds the across-replica average peak (a 1-replica app has them
            // equal, so it's omitted there).
            if let Some(peak) = s.peak_replica.filter(|p| *p > highest) {
                base = format!("{base} / peak-replica: {}{suffix}", format_value(peak));
            }
            match limits.map(|l| l.cpu_millicores).filter(|m| *m > 0) {
                Some(max_mc) => format!("{base} / max {max_mc} mCores"),
                None => base,
            }
        }
        MetricKind::Memory => {
            let latest = s.latest().unwrap_or(0.0);
            let highest = s.max().max(0.0);
            let suffix = unit_suffix(s);
            let mut base = format!(
                "latest: {}{suffix} / highest: {}{suffix}",
                format_bytes(latest),
                format_bytes(highest),
            );
            if let Some(peak) = s.peak_replica.filter(|p| *p > highest) {
                base = format!("{base} / peak-replica: {}", format_bytes(peak));
            }
            match limits.map(|l| l.memory_bytes).filter(|b| *b > 0) {
                Some(max_b) => format!("{base} / max {}", format_bytes(max_b as f64)),
                None => base,
            }
        }
        // SQL utilization kinds never reach the Apis summary (they're rendered
        // by `super::sql_detail` with its own percentage summary), but the
        // match must stay total.
        MetricKind::Dtu | MetricKind::Storage | MetricKind::Workers => {
            let latest = s.latest().unwrap_or(0.0);
            let highest = s.max().max(0.0);
            let suffix = unit_suffix(s);
            format!(
                "latest: {}{suffix} / highest: {}{suffix}",
                format_value(latest),
                format_value(highest),
            )
        }
    }
}

fn unit_suffix(s: &MetricSeries) -> String {
    let unit = s.unit.trim();
    if unit.is_empty() || unit.eq_ignore_ascii_case("count") || unit.eq_ignore_ascii_case("bytes") {
        String::new()
    } else if unit == "%" {
        "%".to_string()
    } else {
        format!(" {unit}")
    }
}

pub(crate) fn format_count(v: f64) -> String {
    // Short-circuit non-positive / NaN before any arithmetic. `v.max(0.0)`
    // doesn't reliably strip negative zero on every platform, which leaks
    // through as `-0` from `format!("{:.0}", -0.0)`.
    if v.is_nan() || v <= 0.0 {
        return "0".to_string();
    }
    if v >= 1_000_000.0 {
        format!("{:.1}M", v / 1_000_000.0)
    } else if v >= 1_000.0 {
        format!("{:.1}k", v / 1_000.0)
    } else {
        format!("{v:.0}")
    }
}

fn format_value(v: f64) -> String {
    if v >= 100.0 {
        format!("{v:.0}")
    } else {
        format!("{v:.1}")
    }
}

fn format_bytes(v: f64) -> String {
    let v = v.max(0.0);
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const KB: f64 = 1024.0;
    if v >= GB {
        format!("{:.1} GB", v / GB)
    } else if v >= MB {
        format!("{:.1} MB", v / MB)
    } else if v >= KB {
        format!("{:.1} KB", v / KB)
    } else {
        format!("{v:.0} B")
    }
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

/// Pick a colour for the `state:` value shown on the Detail header. Matches the
/// vocabulary across Function Apps (`Running`/`Stopped`), Container Apps
/// (`Running`/`Progressing`/`Stopped`/`Suspended`), and App Gateways
/// (`Running`/`Starting`/`Stopping`/`Stopped`). The catppuccin palette has no
/// dedicated "good" green for state, so `Running` reuses `theme.fg` (the caller
/// adds a bold modifier separately) — distinct from the badge colours and still
/// reads as "fine" against muted siblings.
fn state_color(state: &str, theme: &Theme) -> Color {
    match state {
        "Running" => theme.fg,
        "Stopped" => theme.critical,
        "Suspended" => theme.degraded,
        "Starting" | "Stopping" | "Progressing" => theme.accent,
        _ => theme.muted,
    }
}

/// Colour for a revision `runningState` shown on the Detail `status:` row.
/// Terminal failures read critical (matching the health badge that drove the
/// user here); transient in-progress states read degraded. `Running` never
/// reaches this — [`container_app_meta_lines`] only emits the row for abnormal
/// states — so the accent fallback is just defensive.
fn revision_state_color(running_state: &str, theme: &Theme) -> Color {
    match running_state {
        "ActivationFailed" | "Failed" | "Stopped" | "Degraded" => theme.critical,
        "Processing" | "Activating" | "Unknown" => theme.degraded,
        _ => theme.accent,
    }
}

/// True when a meta row's label is a blank continuation (all whitespace, or
/// empty) rather than a fresh section head. The multi-line blocks (triggers,
/// replicas, containers) repeat a spaces-only label on every row after the
/// first; [`group_sections`] and [`render`] both use this to fold such a block
/// into a single navigable section.
fn is_continuation_label(label: &str) -> bool {
    label.trim().is_empty()
}

/// Append a rendered line to the section list: `is_head` starts a new section,
/// otherwise the line extends the current one (a blank-label continuation row).
/// The empty-list fallback treats a stray continuation as a head so a section
/// always exists to land on.
fn add_selectable_line(sections: &mut Vec<Vec<usize>>, is_head: bool, line_idx: usize) {
    if is_head {
        sections.push(vec![line_idx]);
    } else if let Some(last) = sections.last_mut() {
        last.push(line_idx);
    } else {
        sections.push(vec![line_idx]);
    }
}

/// Replacement modal content for a section, returned by the `modal_override`
/// passed to [`group_sections`]. `modal_lines` is the (uncapped) body shown on
/// Enter; `yank` overrides what `y` copies, falling back to the body joined
/// with newlines when `None`. The split lets a section show explanatory prose
/// in its modal without that prose leaking into the copied text.
struct ModalOverride {
    modal_lines: Vec<String>,
    yank: Option<String>,
}

/// Fold a flat list of `(label, value, _)` meta rows into navigable sections:
/// a non-blank label starts a section, blank-label rows extend the current one
/// (see [`is_continuation_label`]). Each section becomes one [`SelectableMeta`]
/// whose modal lists the whole block. `modal_override(title)` may replace a
/// section's modal body (and optionally its yank) with fuller, uncapped
/// content — used to expand the inline `+N more` preview into every trigger /
/// replica.
fn group_sections(
    rows: &[(String, String, String)],
    modal_override: impl Fn(&str) -> Option<ModalOverride>,
) -> Vec<SelectableMeta> {
    let mut out: Vec<SelectableMeta> = Vec::new();
    for (label, value, _) in rows {
        if is_continuation_label(label) {
            if let Some(last) = out.last_mut() {
                last.modal_lines.push(value.clone());
                last.yank.push('\n');
                last.yank.push_str(value);
                continue;
            }
        }
        out.push(SelectableMeta {
            yank: value.clone(),
            modal_title: label.trim_end_matches(':').trim().to_string(),
            modal_lines: vec![value.clone()],
            enter_action: None,
        });
    }
    for m in out.iter_mut() {
        if let Some(ov) = modal_override(&m.modal_title) {
            m.yank = ov.yank.unwrap_or_else(|| ov.modal_lines.join("\n"));
            m.modal_lines = ov.modal_lines;
        }
    }
    out
}

/// Full, uncapped trigger list for the `triggers` section modal — one line per
/// function, names column-aligned across the *whole* set. Unlike the inline
/// [`function_trigger_lines`] preview (capped at [`TRIGGER_CAP`] with a `+N
/// more` summary), this never truncates, so Enter reveals every function —
/// including the ones hidden behind that summary.
fn trigger_modal_lines(
    list: &[crate::azure::function_app_triggers::FunctionTrigger],
) -> Vec<String> {
    let max_name = list
        .iter()
        .map(|t| t.function.chars().count())
        .max()
        .unwrap_or(0);
    list.iter()
        .map(|t| {
            let name_col = format!("{:<width$}", t.function, width = max_name);
            let kind = if t.kind.is_empty() {
                "\u{2014}".to_string()
            } else {
                t.kind.clone()
            };
            match &t.detail {
                Some(d) => format!("{name_col}  {kind}: {d}"),
                None => format!("{name_col}  {kind}"),
            }
        })
        .collect()
}

/// Full per-replica detail for the `replicas` section modal: every replica
/// (newest first, uncapped), each rendered as a [`replica_modal_lines`] block
/// separated by a blank line. The inline preview caps at 10 with a `+N more`
/// row; Enter here shows all of them.
fn replicas_modal_lines(
    replicas: &[crate::azure::container_app_replicas::ReplicaInstance],
) -> Vec<String> {
    // Lead with a one-line reminder that this block is runtime, not config — the
    // `scale:` / `container config:` rows above describe the desired state. This
    // preamble is modal-only chrome; [`replicas_detail_lines`] omits it so `y`
    // copies the data alone.
    let mut out: Vec<String> = vec![
        "Live replica instances — the pods actually running now".to_string(),
        "(vs. the configured scale / container config above).".to_string(),
        String::new(),
    ];
    out.extend(replicas_detail_lines(replicas));
    out
}

/// The per-replica detail blocks without the explanatory preamble: every replica
/// (newest first, uncapped) rendered via [`replica_modal_lines`] and separated
/// by a blank line. This is what `y` copies for the instances section.
fn replicas_detail_lines(
    replicas: &[crate::azure::container_app_replicas::ReplicaInstance],
) -> Vec<String> {
    let mut sorted: Vec<&crate::azure::container_app_replicas::ReplicaInstance> =
        replicas.iter().collect();
    sorted.sort_by_key(|r| std::cmp::Reverse(r.created_at));
    let mut out: Vec<String> = Vec::new();
    for (idx, r) in sorted.iter().enumerate() {
        if idx > 0 {
            out.push(String::new());
        }
        out.extend(replica_modal_lines(r));
    }
    out
}

/// Build the Detail view's selection list — one [`SelectableMeta`] per
/// *section* (a labelled row plus its blank-label continuations), in display
/// order. Render uses the order/grouping to highlight the cursor's section; the
/// input handler uses the contents to wire `y` and Enter. Stay in sync with
/// [`render`]'s push order and blank-label grouping or the cursor will point at
/// the wrong section.
fn selectable_metas(state: &AppState, resource: &Resource) -> Vec<SelectableMeta> {
    let mut out: Vec<SelectableMeta> = Vec::new();

    // State section. Skipped when a metrics fetch error is overlaying the slot —
    // the row in that case is a pure error string with nothing to drill into.
    let metrics_failure = state.metrics.failures.get(&resource.id);
    if metrics_failure.is_none() {
        let raw_state = resource.state.as_deref().unwrap_or("unknown");
        out.push(SelectableMeta {
            yank: raw_state.to_string(),
            modal_title: "state".to_string(),
            modal_lines: vec![raw_state.to_string()],
            enter_action: None,
        });
    }

    // Meta block. Container Apps draw rev/image/replicas/containers; Function
    // Apps draw their own image/runtime lines. The CA block is skipped while its
    // cache is still loading (render shows non-selectable skeletons then); the FA
    // block has no skeleton. The multi-row CA `containers:` block folds into one
    // section. Stay in sync with [`render`]'s `meta_lines` selection.
    let revision_meta = state.revision_meta.by_resource.get(&resource.id);
    let limits = state.container_app_overview.by_resource.get(&resource.id);
    let is_ca = resource.kind == ResourceKind::ContainerApp;
    let is_site = matches!(
        resource.kind,
        ResourceKind::FunctionApp | ResourceKind::WebApp
    );
    let is_fa = resource.kind == ResourceKind::FunctionApp;
    let is_apim = resource.kind == ResourceKind::Apim;
    let ca_meta_loading = is_ca && (revision_meta.is_none() || limits.is_none());
    let meta_lines = if ca_meta_loading {
        Vec::new()
    } else if is_site {
        function_app_meta_lines(state, resource)
    } else if is_apim {
        apim_meta_lines(resource)
    } else {
        container_app_meta_lines(revision_meta, limits)
    };
    out.extend(group_sections(&meta_lines, |_| None));

    // Replica block. Same skeleton skip. The whole block is one `replicas`
    // section; Enter shows every replica's full record (name, created, per-
    // container status with restart counts) — uncapped, expanding the inline
    // `+N more`. The pending/failure hint stays a plain single-line section.
    let cached_replicas = state.replica_instances.by_resource.get(&resource.id);
    let replicas_loading_skeleton = is_ca && cached_replicas.is_none();
    if !replicas_loading_skeleton {
        let replica_lines = replica_status_lines(
            cached_replicas,
            state.replica_instances.pending.contains(&resource.id),
            state.replica_instances.failures.get(&resource.id),
        );
        out.extend(group_sections(&replica_lines, |_| {
            cached_replicas
                .filter(|list| !list.is_empty())
                .map(|list| ModalOverride {
                    modal_lines: replicas_modal_lines(list),
                    // `y` copies the per-replica detail only — the modal's prose
                    // preamble is on-screen chrome, not data worth pasting.
                    yank: Some(replicas_detail_lines(list).join("\n")),
                })
        }));
    }

    // Function App triggers. FA-only, one `triggers` section. Enter expands the
    // inline `+N more` into the full per-function list; a loading / failure hint
    // stays a plain single-line section (no cache to expand).
    if is_fa {
        let trigger_lines = function_trigger_lines(
            state.func_triggers.by_resource.get(&resource.id),
            state.func_triggers.pending.contains(&resource.id),
            state.func_triggers.failures.get(&resource.id),
        );
        let cached_triggers = state.func_triggers.by_resource.get(&resource.id);
        out.extend(group_sections(&trigger_lines, |_| {
            cached_triggers
                .filter(|list| !list.is_empty())
                .map(|list| ModalOverride {
                    modal_lines: trigger_modal_lines(list),
                    yank: None,
                })
        }));
    }

    // Env-vars teaser. Enter on this row jumps to the dedicated EnvVars page
    // instead of opening a modal; the page already gives a full view, so a
    // modal would be redundant.
    let env_rows_count = env_var_rows_count(state, resource, ca_meta_loading);
    for _ in 0..env_rows_count {
        out.push(SelectableMeta {
            yank: format!("{} env vars", env_vars_count_for(state, resource)),
            modal_title: "env vars".to_string(),
            modal_lines: vec!["Press Enter or e to open the env vars page.".to_string()],
            enter_action: Some(Action::OpenEnvVars),
        });
    }

    // General lines (tags, created, modified). Tags get a one-per-line modal so
    // a long list reads without the inline column truncation.
    let general_lines = general_meta_lines(resource, &state.principals);
    out.extend(group_sections(&general_lines, |title| {
        if title == "tags" {
            Some(ModalOverride {
                modal_lines: tag_modal_lines(resource),
                yank: None,
            })
        } else {
            None
        }
    }));

    out
}

/// Portal-blade suffix for the Detail row currently under the cursor, when it
/// points at a more specific blade than the resource overview. Appended to the
/// resource id by the `o` handler (see [`crate::ui::app`]'s `portal_url_for`);
/// `None` falls back to the overview.
///
/// Keyed off the selected row's `modal_title` (the meta label, trimmed) plus the
/// resource kind, and built from the same [`selectable_metas`] list the cursor
/// indexes — so it always tracks the highlighted row. Currently the only
/// row-specific target is the Function App `network:` row → the Networking
/// blade (`networkingHub` on `Microsoft.Web/sites`).
pub(crate) fn selected_meta_portal_suffix(
    state: &AppState,
    resource: &Resource,
) -> Option<&'static str> {
    let metas = selectable_metas(state, resource);
    if metas.is_empty() {
        return None;
    }
    let cursor = state.detail_view.cursor.min(metas.len() - 1);
    match (resource.kind, metas[cursor].modal_title.as_str()) {
        // App Service (Function App / Web App) "Networking" blade.
        (ResourceKind::FunctionApp | ResourceKind::WebApp, "network") => Some("/networkingHub"),
        // Container App "Ingress" blade — where its exposure (external/internal,
        // IP restrictions) is configured.
        (ResourceKind::ContainerApp, "network") => Some("/ingress"),
        _ => None,
    }
}

/// Count how many env-vars teaser rows the render would emit for this
/// resource. Mirrors the carve-outs in [`env_var_rows`] (skipped for CA while
/// the overview is loading, returns 0 if env vars aren't applicable, etc.) so
/// the selection list stays aligned with what the user actually sees.
fn env_var_rows_count(state: &AppState, resource: &Resource, ca_meta_loading: bool) -> usize {
    if ca_meta_loading {
        return 0;
    }
    let id = resource.id.as_str();
    let kind = resource.kind;
    match env_vars_for(state, id, kind) {
        Some(vars) if !vars.is_empty() => 1,
        Some(_) => 0,
        None => {
            // Function App / Web App only: emit a hint row when the settings
            // fetch is in flight or has already failed.
            if matches!(kind, ResourceKind::FunctionApp | ResourceKind::WebApp)
                && (state.func_settings.failures.contains_key(id)
                    || state.func_settings.pending.contains(id))
            {
                return 1;
            }
            0
        }
    }
}

/// Count of env vars (best-effort, returns 0 when not loaded) — used to make
/// the env-vars row's yank text reflect what the user sees.
fn env_vars_count_for(state: &AppState, resource: &Resource) -> usize {
    env_vars_for(state, &resource.id, resource.kind)
        .map(|v| v.len())
        .unwrap_or(0)
}

/// Build the modal body for a tag row: one `key=value` per line so long lists
/// don't get truncated like they do inline.
fn tag_modal_lines(resource: &Resource) -> Vec<String> {
    if resource.meta.tags.is_empty() {
        return vec!["(no tags)".to_string()];
    }
    resource
        .meta
        .tags
        .iter()
        .map(|(k, v)| format!("{k} = {v}"))
        .collect()
}

/// Build the modal body for a replica row: name, timing, top-level state, and
/// one line per container with its readiness probe + restart count + per-
/// container running state.
fn replica_modal_lines(
    replica: &crate::azure::container_app_replicas::ReplicaInstance,
) -> Vec<String> {
    let mut lines = vec![format!("name: {}", replica.name)];
    if let Some(ts) = replica.created_at {
        lines.push(format!("created: {}", ts.format("%Y-%m-%d %H:%M:%S UTC")));
    }
    if let Some(rs) = replica.running_state.as_deref() {
        lines.push(format!("running state: {rs}"));
    }
    if !replica.containers.is_empty() {
        lines.push(String::new());
        lines.push("containers:".to_string());
        for c in &replica.containers {
            let glyph = ready_glyph(c.ready);
            let name = container_display_name(c);
            let running = c.running_state.as_deref().unwrap_or("?");
            lines.push(format!(
                "  {name} {glyph}  restarts {}  ({running})",
                c.restart_count
            ));
            // Full reason (not the inline short tag) so the modal explains a
            // stuck container — e.g. the exact image that failed to pull.
            if !running.eq_ignore_ascii_case("Running") {
                if let Some(details) = c.running_state_details.as_deref() {
                    lines.push(format!("      \u{21b3} {details}"));
                }
            }
        }
    }
    lines
}

pub fn handle(action: Action, state: &mut AppState) -> bool {
    // While the Enter modal is open, route navigation/dismissal there first.
    // Any other action (refresh, window switch, …) falls through to the
    // normal handler so the user can still drive the page underneath.
    if state.detail_view.modal.is_some() {
        // Every mutation clamps to the render-published `scroll_max` so the
        // stored offset never parks past the content — a stale over-scroll
        // (e.g. after the pane grew) self-heals on the next key press instead
        // of making `k` look dead.
        let max_scroll = state.scroll_max.get();
        match action {
            Action::Back => {
                state.detail_view.modal = None;
                return true;
            }
            Action::MoveDown => {
                if let Some(m) = state.detail_view.modal.as_mut() {
                    m.scroll = m.scroll.saturating_add(1).min(max_scroll);
                }
                return true;
            }
            Action::MoveUp => {
                if let Some(m) = state.detail_view.modal.as_mut() {
                    m.scroll = m.scroll.min(max_scroll).saturating_sub(1);
                }
                return true;
            }
            Action::HalfPageDown => {
                if let Some(m) = state.detail_view.modal.as_mut() {
                    m.scroll = m.scroll.saturating_add(8).min(max_scroll);
                }
                return true;
            }
            Action::HalfPageUp => {
                if let Some(m) = state.detail_view.modal.as_mut() {
                    m.scroll = m.scroll.min(max_scroll).saturating_sub(8);
                }
                return true;
            }
            Action::GotoTop => {
                if let Some(m) = state.detail_view.modal.as_mut() {
                    m.scroll = 0;
                }
                return true;
            }
            Action::GotoBottom => {
                if let Some(m) = state.detail_view.modal.as_mut() {
                    m.scroll = max_scroll;
                }
                return true;
            }
            // All other keys are swallowed so they don't steer the underlying
            // view while the modal owns the foreground. Esc/Back closes; the
            // explicit nav arms above scroll.
            _ => return true,
        }
    }

    match action {
        Action::SetWindowHour => set_window(state, TimeRange::Hour),
        Action::SetWindowDay => set_window(state, TimeRange::Day),
        Action::SetWindowWeek => set_window(state, TimeRange::Week),
        Action::MoveDown => {
            if let Some(resource) = state.selected_resource().cloned() {
                let count = selectable_metas(state, &resource).len();
                if count > 0 {
                    let next = state.detail_view.cursor.saturating_add(1);
                    state.detail_view.cursor = next.min(count - 1);
                }
            }
            true
        }
        Action::MoveUp => {
            state.detail_view.cursor = state.detail_view.cursor.saturating_sub(1);
            true
        }
        Action::GotoTop => {
            state.detail_view.cursor = 0;
            true
        }
        Action::GotoBottom => {
            if let Some(resource) = state.selected_resource().cloned() {
                let count = selectable_metas(state, &resource).len();
                state.detail_view.cursor = count.saturating_sub(1);
            }
            true
        }
        Action::Yank => {
            // Override the global yank target with the selected row's yank
            // text — that's the whole point of j/k navigation in this view.
            let resource = match state.selected_resource().cloned() {
                Some(r) => r,
                None => return false,
            };
            let metas = selectable_metas(state, &resource);
            if metas.is_empty() {
                return false;
            }
            let cursor = state.detail_view.cursor.min(metas.len() - 1);
            let text = metas[cursor].yank.clone();
            if text.is_empty() {
                state.set_status("nothing to copy");
                return true;
            }
            match crate::ui::clipboard::copy(&text) {
                Ok(n) => state.set_status(format!("copied {n} bytes to clipboard")),
                Err(e) => state.set_status(format!("clipboard write failed: {e}")),
            }
            true
        }
        Action::OpenEnvVars => {
            let kind = state.selected_resource().map(|r| r.kind);
            match kind {
                Some(
                    ResourceKind::ContainerApp | ResourceKind::FunctionApp | ResourceKind::WebApp,
                ) => {
                    // Fresh page: cursor at top, values masked.
                    state.env_vars_view = crate::ui::state::EnvVarsView::default();
                    state.view = View::EnvVars;
                }
                _ => state.set_status("no environment variables for this resource type"),
            }
            true
        }
        Action::OpenLogs => {
            let supports = state
                .selected_resource()
                .map(|r| supports_logs(r.kind))
                .unwrap_or(false);
            if supports {
                // Drop the previous resource's source/search filters so the new
                // app's logs aren't hidden behind a stale container-name filter.
                state.logs.reset_view_filters();
                state.view = View::Logs;
            } else {
                state.set_status("logs are not supported for this resource type");
            }
            true
        }
        Action::OpenSelected => {
            // APIM drill-in still wins: Enter on an APIM service detail opens
            // the APIs panel regardless of which meta row the cursor is on.
            let is_apim = state
                .selected_resource()
                .map(|r| r.kind == ResourceKind::Apim)
                .unwrap_or(false);
            if is_apim {
                state.apim.apis_cursor = 0;
                state.apim.selected_api_id = None;
                state.view = View::ApimApis;
                return true;
            }
            // Otherwise: open the selected meta row's modal (or dispatch the
            // row's `enter_action` for the env-vars teaser).
            let resource = match state.selected_resource().cloned() {
                Some(r) => r,
                None => return true,
            };
            let metas = selectable_metas(state, &resource);
            if metas.is_empty() {
                return true;
            }
            let cursor = state.detail_view.cursor.min(metas.len() - 1);
            let meta = &metas[cursor];
            if let Some(act) = meta.enter_action {
                // Dispatch the carried action through `handle` again so the
                // existing implementation runs (OpenEnvVars → push view, etc.).
                return handle(act, state);
            }
            state.detail_view.modal = Some(DetailModal {
                title: meta.modal_title.clone(),
                lines: meta.modal_lines.clone(),
                scroll: 0,
            });
            true
        }
        _ => false,
    }
}

/// Render the Enter modal over the Detail view when one is open. Called from
/// the top-level dispatcher (`app::dispatch_view`) so the modal stacks above
/// the underlying view but below the global status / command bars.
pub fn render_modal(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    use ratatui::layout::Alignment;
    use ratatui::widgets::{Clear, Wrap};

    let Some(modal) = state.detail_view.modal.as_ref() else {
        return;
    };

    // Size: fit roughly two thirds of the screen, capped so very tall
    // terminals don't render an awkward strip. Min width keeps it readable
    // on a small splitter pane.
    let target_w = ((area.width as u32 * 2 / 3) as u16).max(40).min(area.width);
    let target_h = ((area.height as u32 * 2 / 3) as u16)
        .max(8)
        .min(area.height);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(target_w) / 2,
        y: area.y + area.height.saturating_sub(target_h) / 2,
        width: target_w,
        height: target_h,
    };
    if popup.width == 0 || popup.height == 0 {
        return;
    }

    frame.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(
            format!(" {} ", modal.title),
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

    // Reserve the bottom row for the help hint so it always stays in view as
    // the user scrolls long content.
    let body_height = inner.height.saturating_sub(1);
    let body_area = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: body_height,
    };
    let hint_area = Rect {
        x: inner.x,
        y: inner.y + body_height,
        width: inner.width,
        height: 1,
    };

    let lines: Vec<Line> = modal
        .lines
        .iter()
        .map(|l| Line::from(Span::styled(l.clone(), Style::default().fg(theme.fg))))
        .collect();
    // `scroll((y, x))` skips the first `y` *wrapped* rows, so the ceiling must
    // count wrapped rows minus the viewport — clamping to `lines.len()` let G
    // scroll every line out of view (blank modal) and left long wrapped lines
    // unreachable. Publish the max through `scroll_max` so the key handler can
    // clamp the stored offset too; otherwise `k` after G walks down invisibly
    // from a huge sentinel before anything moves.
    let total_rows: usize = modal
        .lines
        .iter()
        .map(|l| wrapped_line_count(l, body_area.width))
        .sum();
    let max_scroll = total_rows
        .saturating_sub(body_area.height as usize)
        .min(u16::MAX as usize) as u16;
    state.scroll_max.set(max_scroll);
    let scroll_y = modal.scroll.min(max_scroll);
    let body = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll_y, 0));
    frame.render_widget(body, body_area);

    let hint = Paragraph::new(Line::from(Span::styled(
        "j/k scroll · g top · Esc close",
        Style::default().fg(theme.muted),
    )))
    .alignment(Alignment::Center);
    frame.render_widget(hint, hint_area);
}

fn set_window(state: &mut AppState, range: TimeRange) -> bool {
    if state.metrics.range == range {
        return true;
    }
    state.metrics.range = range;
    // Drop cached series for the selected resource so Lane 3 reloads.
    if let Some(id) = state.selected_resource().map(|r| r.id.clone()) {
        state.metrics.by_resource.remove(&id);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::metrics::{MetricPoint, MetricSeries};
    use crate::azure::resources::{Resource, ResourceKind};
    use crate::config::Config;
    use chrono::{Duration, Utc};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    // NOTE: time-axis / stretch_to_width / missing-reason tests moved to
    // `super::metric_chart` alongside the functions they exercise.

    #[test]
    fn meta_lines_full_shape_emits_expected_rows() {
        use crate::azure::container_app_overview::ContainerAppOverview;
        use crate::azure::container_app_revisions::ActiveRevisionMeta;
        use crate::ui::theme::Theme;

        let theme = Theme::catppuccin_mocha();
        let meta = ActiveRevisionMeta {
            name: "files-api--0000004".into(),
            image: Some("myacr/files-api:abc123".into()),
            replicas: 2,
            min_replicas: 1,
            max_replicas: 10,
            ..Default::default()
        };
        let limits = ContainerAppOverview {
            cpu_millicores: 500,
            memory_bytes: 0,
            fqdn: Some("files-api.example.azurecontainerapps.io".into()),
            ingress_external: Some(true),
            ..Default::default()
        };
        let lines = container_app_meta_lines(Some(&meta), Some(&limits));
        let labels: Vec<&str> = lines.iter().map(|(l, _, _)| l.as_str()).collect();
        assert_eq!(
            labels,
            vec!["rev:", "image:", "scale:", "fqdn:", "network:"]
        );
        assert_eq!(lines[0].1, "files-api--0000004");
        assert_eq!(lines[1].1, "myacr/files-api:abc123");
        assert_eq!(lines[2].1, "2 of 1\u{2013}10");
        assert_eq!(lines[3].1, "files-api.example.azurecontainerapps.io");
        assert_eq!(lines[4].1, "external ingress (public)");
    }

    #[test]
    fn container_app_meta_surfaces_failing_revision_status_and_error() {
        use crate::azure::container_app_revisions::ActiveRevisionMeta;
        let meta = ActiveRevisionMeta {
            name: "edc-api--0000005".into(),
            running_state: "ActivationFailed".into(),
            provisioning_error: Some(
                "Deployment Progress Deadline Exceeded. 0/1 replicas ready.".into(),
            ),
            ..Default::default()
        };
        let lines = container_app_meta_lines(Some(&meta), None);
        let labels: Vec<&str> = lines.iter().map(|(l, _, _)| l.as_str()).collect();
        // status + issue land right under rev:, before scale: — mirroring the
        // portal's "Revisions with Issues" running-status / details columns.
        assert_eq!(labels, vec!["rev:", "status:", "issue:", "scale:"]);
        assert_eq!(lines[1].1, "ActivationFailed");
        assert_eq!(
            lines[2].1,
            "Deployment Progress Deadline Exceeded. 0/1 replicas ready."
        );
    }

    #[test]
    fn container_app_meta_omits_status_and_issue_when_healthy() {
        use crate::azure::container_app_revisions::ActiveRevisionMeta;
        let meta = ActiveRevisionMeta {
            name: "edc-api--0000006".into(),
            running_state: "Running".into(),
            provisioning_error: None,
            ..Default::default()
        };
        let labels: Vec<String> = container_app_meta_lines(Some(&meta), None)
            .into_iter()
            .map(|(l, _, _)| l)
            .collect();
        assert!(!labels.iter().any(|l| l == "status:" || l == "issue:"));
    }

    #[test]
    fn container_app_network_row_reflects_ingress_posture() {
        use crate::azure::container_app_overview::ContainerAppOverview;
        let net = |ov: &ContainerAppOverview| -> String {
            container_app_meta_lines(None, Some(ov))
                .into_iter()
                .find(|(l, _, _)| l == "network:")
                .map(|(_, v, _)| v)
                .expect("network row present")
        };
        // No ingress block → no inbound endpoint.
        assert_eq!(net(&ContainerAppOverview::default()), "no ingress");
        // Internal ingress → VNet-only.
        assert_eq!(
            net(&ContainerAppOverview {
                ingress_external: Some(false),
                ..Default::default()
            }),
            "internal ingress (VNet only)"
        );
        // External, no restrictions → public.
        assert_eq!(
            net(&ContainerAppOverview {
                ingress_external: Some(true),
                ..Default::default()
            }),
            "external ingress (public)"
        );
        // External + ipSecurityRestrictions → restricted.
        assert_eq!(
            net(&ContainerAppOverview {
                ingress_external: Some(true),
                access_restricted: true,
                ..Default::default()
            }),
            "external ingress (IP restricted)"
        );
    }

    #[test]
    fn container_app_network_row_o_targets_the_ingress_blade() {
        use crate::azure::container_app_overview::ContainerAppOverview;
        use crate::azure::container_app_revisions::ActiveRevisionMeta;
        let mut state = AppState::new(Config::default());
        let mut res = r();
        res.kind = ResourceKind::ContainerApp;
        state.resources = vec![res.clone()];
        state.list_cursor = 0;
        // Both caches present so the CA meta block (incl. the network row) builds.
        state
            .revision_meta
            .by_resource
            .insert(res.id.clone(), ActiveRevisionMeta::default());
        state.container_app_overview.by_resource.insert(
            res.id.clone(),
            ContainerAppOverview {
                ingress_external: Some(true),
                ..Default::default()
            },
        );
        let metas = selectable_metas(&state, &res);
        let idx = metas
            .iter()
            .position(|m| m.modal_title == "network")
            .expect("container app detail has a network row");
        state.detail_view.cursor = idx;
        assert_eq!(selected_meta_portal_suffix(&state, &res), Some("/ingress"));
    }

    #[test]
    fn meta_lines_collapses_missing_image_scale_and_fqdn() {
        use crate::azure::container_app_revisions::ActiveRevisionMeta;
        use crate::ui::theme::Theme;

        let theme = Theme::catppuccin_mocha();
        let meta = ActiveRevisionMeta {
            name: "rev".into(),
            image: None,
            replicas: 1,
            min_replicas: 0,
            max_replicas: 0,
            ..Default::default()
        };
        let lines = container_app_meta_lines(Some(&meta), None);
        let labels: Vec<&str> = lines.iter().map(|(l, _, _)| l.as_str()).collect();
        assert_eq!(labels, vec!["rev:", "scale:"]);
        assert_eq!(lines[1].1, "1");
    }

    #[test]
    fn meta_lines_empty_when_no_data() {
        use crate::ui::theme::Theme;
        let theme = Theme::catppuccin_mocha();
        assert!(container_app_meta_lines(None, None).is_empty());
    }

    /// Build an APIM Resource with the given networking metadata.
    fn apim_resource(gateway: Option<&str>, public_ips: &[&str], private_ips: &[&str]) -> Resource {
        use crate::azure::resources::ResourceMeta;
        Resource {
            id: "/r/apim".into(),
            name: "myapim".into(),
            kind: ResourceKind::Apim,
            location: "westeurope".into(),
            resource_group: "rg".into(),
            subscription_id: "sub".into(),
            state: None,
            created_at: None,
            modified_at: None,
            meta: ResourceMeta {
                gateway_url: gateway.map(str::to_string),
                public_ips: public_ips.iter().map(|s| s.to_string()).collect(),
                private_ips: private_ips.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            },
        }
    }

    #[test]
    fn apim_meta_lines_emits_gateway_and_virtual_ips() {
        let res = apim_resource(
            Some("https://myapim.azure-api.net"),
            &["20.1.2.3"],
            &["10.0.0.4", "10.0.0.5"],
        );
        let lines = apim_meta_lines(&res);
        let labels: Vec<&str> = lines.iter().map(|(l, _, _)| l.as_str()).collect();
        assert_eq!(labels, vec!["gateway:", "public IP:", "private IP:"]);
        assert_eq!(lines[0].1, "https://myapim.azure-api.net");
        assert_eq!(lines[1].1, "20.1.2.3");
        // Multiple VIPs join on one line.
        assert_eq!(lines[2].1, "10.0.0.4, 10.0.0.5");
    }

    #[test]
    fn apim_meta_lines_collapses_absent_private_ips() {
        let res = apim_resource(Some("https://myapim.azure-api.net"), &["20.1.2.3"], &[]);
        let labels: Vec<String> = apim_meta_lines(&res)
            .iter()
            .map(|(l, _, _)| l.clone())
            .collect();
        assert_eq!(labels, vec!["gateway:", "public IP:"]);
        // Nothing at all → no rows (e.g. an APIM whose gateway hasn't resolved).
        assert!(apim_meta_lines(&apim_resource(None, &[], &[])).is_empty());
    }

    #[test]
    fn apim_detail_renders_gateway_and_ips() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(100, 24);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = AppState::new(Config::default());
        state.resources = vec![apim_resource(
            Some("https://myapim.azure-api.net"),
            &["20.1.2.3"],
            &["10.0.0.4"],
        )];
        state.list_cursor = 0;
        state.view = View::Detail;
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("gateway"), "expected gateway row in {s}");
        assert!(
            s.contains("myapim.azure-api.net"),
            "expected gateway url in {s}"
        );
        assert!(s.contains("20.1.2.3"), "expected public VIP in {s}");
        assert!(s.contains("10.0.0.4"), "expected private VIP in {s}");
    }

    #[test]
    fn function_app_meta_lines_includes_network_posture() {
        let mut state = AppState::new(Config::default());
        // r() is a Function App with default meta → public access (Azure default).
        let res = r();
        let net = |state: &AppState, res: &Resource| -> String {
            function_app_meta_lines(state, res)
                .into_iter()
                .find(|(l, _, _)| l == "network:")
                .map(|(_, v, _)| v)
                .expect("network row present")
        };

        // Enabled, restriction state not yet fetched → posture without detail.
        assert_eq!(net(&state, &res), "public access enabled");

        // Same `config/web` fetch that feeds the image reports the restriction
        // state; the row then distinguishes wide-open from gated public access.
        state
            .func_image
            .access_restricted
            .insert(res.id.clone(), false);
        assert_eq!(net(&state, &res), "public access enabled (no restrictions)");
        state
            .func_image
            .access_restricted
            .insert(res.id.clone(), true);
        assert_eq!(
            net(&state, &res),
            "public access enabled (IP/VNet restricted)"
        );

        // publicNetworkAccess = Disabled wins regardless of restriction rules.
        let mut private = r();
        private.meta.public_network_access = Some("Disabled".into());
        state
            .func_image
            .access_restricted
            .insert(private.id.clone(), true);
        assert_eq!(net(&state, &private), "public access disabled");
    }

    #[test]
    fn network_row_o_targets_the_networking_blade() {
        let mut state = fixture_no_metrics(); // r() is a Function App
        let resource = state.resources[0].clone();
        let metas = selectable_metas(&state, &resource);
        // `o` on the network row deep-links to the Networking blade.
        let net_idx = metas
            .iter()
            .position(|m| m.modal_title == "network")
            .expect("function app detail has a network row");
        state.detail_view.cursor = net_idx;
        assert_eq!(
            selected_meta_portal_suffix(&state, &resource),
            Some("/networkingHub")
        );
        // A row without a specific blade (the state row) falls back to overview.
        let state_idx = metas
            .iter()
            .position(|m| m.modal_title == "state")
            .expect("there is a state row");
        state.detail_view.cursor = state_idx;
        assert_eq!(selected_meta_portal_suffix(&state, &resource), None);
    }

    #[test]
    fn meta_lines_emits_one_row_per_template_container() {
        use crate::azure::container_app_overview::{ContainerAppOverview, ContainerSpec};
        use crate::azure::container_app_revisions::ActiveRevisionMeta;
        use crate::ui::theme::Theme;
        let theme = Theme::catppuccin_mocha();
        let meta = ActiveRevisionMeta {
            name: "rev".into(),
            image: Some("primary:1.0".into()),
            replicas: 1,
            min_replicas: 1,
            max_replicas: 1,
            ..Default::default()
        };
        let limits = ContainerAppOverview {
            containers: vec![
                ContainerSpec {
                    name: "files".into(),
                    image: Some("myacr/files:abc".into()),
                    cpu_millicores: 250,
                    memory_bytes: 512 * 1024 * 1024,
                    ephemeral_storage: Some("4Gi".into()),
                    env_vars: Vec::new(),
                    is_init: false,
                },
                ContainerSpec {
                    name: "http-auth".into(),
                    image: Some("myacr/http-auth:abc".into()),
                    cpu_millicores: 500,
                    memory_bytes: 1024 * 1024 * 1024,
                    ephemeral_storage: None,
                    env_vars: Vec::new(),
                    is_init: false,
                },
            ],
            ..Default::default()
        };
        let lines = container_app_meta_lines(Some(&meta), Some(&limits));
        let labels: Vec<&str> = lines.iter().map(|(l, _, _)| l.as_str()).collect();
        // Each container is a name header row (the first owns the
        // `container config:` label; later rows use a spaces-only label of the
        // same width) followed by indented `image` / `cpu/mem` (/ `ephemeral`)
        // sub-rows. `files` reports ephemeral storage so it gets a third
        // sub-row; `http-auth` doesn't, so it has only two.
        let blank = " ".repeat("container config:".len()); // 17 spaces
        assert_eq!(labels[0..3], ["rev:", "image:", "scale:"]);
        // Two containers ⇒ the primary image header is tagged `(+1 more)`.
        assert_eq!(lines[1].1, "primary:1.0  (+1 more)");
        assert_eq!(labels[3], "container config:"); // files name header
        assert_eq!(labels[4], blank); // image sub-row
        assert_eq!(labels[5], blank); // cpu/mem sub-row
        assert_eq!(labels[6], blank); // ephemeral sub-row
        assert_eq!(labels[7], blank); // http-auth name header
        assert_eq!(labels[8], blank); // image sub-row
        assert_eq!(labels[9], blank); // cpu/mem sub-row
        assert_eq!(lines[3].1, "files");
        assert!(lines[4].1.contains("image") && lines[4].1.contains("myacr/files:abc"));
        assert!(lines[5].1.contains("250 mCores"));
        assert!(lines[5].1.contains("512.0 MB"));
        assert!(lines[6].1.contains("ephemeral") && lines[6].1.contains("4Gi"));
        assert_eq!(lines[7].1, "http-auth");
        assert!(lines[9].1.contains("500 mCores"));
        // http-auth has no ephemeral row, so the block ends at the cpu/mem row:
        // 3 meta (rev/image/scale) + 7 container rows + 1 network (no fqdn).
        assert_eq!(labels[10], "network:");
        assert_eq!(lines.len(), 11);
    }

    #[test]
    fn replica_status_lines_renders_header_plus_container_rows() {
        use crate::azure::container_app_replicas::{ReplicaContainer, ReplicaInstance};
        use chrono::TimeZone;
        let replicas = vec![ReplicaInstance {
            name: "ca--rev-suffix-r58pz".into(),
            created_at: Some(
                chrono::Utc
                    .with_ymd_and_hms(2026, 5, 26, 16, 37, 49)
                    .unwrap(),
            ),
            running_state: Some("Running".into()),
            containers: vec![
                ReplicaContainer {
                    name: "files".into(),
                    ready: Some(true),
                    started: Some(true),
                    restart_count: 0,
                    running_state: Some("Running".into()),
                    running_state_details: None,
                },
                ReplicaContainer {
                    name: "files-api".into(),
                    ready: Some(true),
                    started: Some(true),
                    restart_count: 0,
                    running_state: Some("Running".into()),
                    running_state_details: None,
                },
                ReplicaContainer {
                    name: "http-auth".into(),
                    ready: Some(true),
                    started: Some(true),
                    restart_count: 0,
                    running_state: Some("Running".into()),
                    running_state_details: None,
                },
            ],
        }];
        let lines = replica_status_lines(Some(&replicas), false, None);
        // One header row (the replica suffix) + one sub-row per container.
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0].0, "instances:");
        // Header carries everything after the app-name prefix (revision suffix
        // included, no ellipsis) and the replica's aggregate running state in
        // parens.
        assert!(lines[0].1.contains("rev-suffix-r58pz"));
        assert!(!lines[0].1.contains('\u{2026}'));
        assert!(lines[0].1.contains("(Running)"));
        // Container sub-rows are blank-label continuations, each with its own
        // readiness glyph (✓ for Ready=true) and restart count.
        assert_eq!(lines[1].0.trim(), "");
        assert!(lines[1].1.contains("files") && lines[1].1.contains('\u{2713}'));
        assert!(lines[1].1.contains("restarts 0"));
        assert!(lines[3].1.contains("http-auth") && lines[3].1.contains('\u{2713}'));
    }

    #[test]
    fn replica_status_lines_shows_loading_when_pending_with_no_cache() {
        let lines = replica_status_lines(None, true, None);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].0, "instances:");
        assert!(lines[0].1.contains("loading"));
    }

    #[test]
    fn replica_status_lines_shows_permission_hint_on_403_failure() {
        let err = "403 Forbidden: not authorised".to_string();
        let lines = replica_status_lines(None, false, Some(&err));
        assert_eq!(lines.len(), 1);
        assert!(lines[0].1.contains("permission denied"));
    }

    #[test]
    fn replica_status_lines_collapses_when_no_replicas_running() {
        // An app with replicas=0 yields an empty vec from the fetch; the
        // section should disappear entirely rather than emit a stub row.
        let lines = replica_status_lines(Some(&vec![]), false, None);
        assert!(lines.is_empty());
    }

    #[test]
    fn replica_status_lines_caps_at_ten_with_more_indicator() {
        use crate::azure::container_app_replicas::{ReplicaContainer, ReplicaInstance};
        let make = |i: u32| ReplicaInstance {
            name: format!("ca--rev-r{i:04}"),
            created_at: None,
            running_state: None,
            containers: vec![ReplicaContainer {
                name: "files".into(),
                ready: Some(true),
                started: Some(true),
                restart_count: 0,
                running_state: None,
                running_state_details: None,
            }],
        };
        let replicas: Vec<_> = (0..15).map(make).collect();
        let lines = replica_status_lines(Some(&replicas), false, None);
        // 10 replicas × (header + 1 container sub-row) + 1 "+5 more" summary.
        assert_eq!(lines.len(), 21);
        assert!(lines.last().unwrap().1.contains("+5 more"));
    }

    #[test]
    fn replica_status_lines_omits_parens_when_running_state_absent() {
        use crate::azure::container_app_replicas::{ReplicaContainer, ReplicaInstance};
        let replicas = vec![ReplicaInstance {
            name: "ca--rev-suffix-abc12".into(),
            created_at: None,
            running_state: None,
            containers: vec![ReplicaContainer {
                name: "files".into(),
                ready: Some(true),
                started: Some(true),
                restart_count: 0,
                running_state: None,
                running_state_details: None,
            }],
        }];
        let lines = replica_status_lines(Some(&replicas), false, None);
        // Header carries the suffix but no `(state)` parens when Azure didn't
        // report a replica-level runningState (avoids an empty `()`).
        assert!(lines[0].1.contains("rev-suffix-abc12"));
        assert!(
            !lines[0].1.contains('('),
            "no empty parens: {:?}",
            lines[0].1
        );
    }

    #[test]
    fn container_blocked_reason_classifies_image_pull_and_skips_running() {
        use crate::azure::container_app_replicas::ReplicaContainer;
        let waiting = ReplicaContainer {
            name: "maintenance".into(),
            ready: Some(false),
            started: Some(false),
            restart_count: 0,
            running_state: Some("Waiting".into()),
            running_state_details: Some(
                "Back-off pulling image \"acr.io/api:bad\" — manifest not found".into(),
            ),
        };
        assert_eq!(
            container_blocked_reason(&waiting).as_deref(),
            Some("image pull failed")
        );

        // A Running container never reports a blocked reason, even if a stale
        // detail lingers.
        let running = ReplicaContainer {
            running_state: Some("Running".into()),
            running_state_details: Some("whatever".into()),
            ..waiting.clone()
        };
        assert_eq!(container_blocked_reason(&running), None);

        // Unknown detail falls back to a trimmed snippet rather than a tag.
        let other = ReplicaContainer {
            running_state: Some("Waiting".into()),
            running_state_details: Some("something unfamiliar happened".into()),
            ..waiting.clone()
        };
        assert_eq!(
            container_blocked_reason(&other).as_deref(),
            Some("something unfamiliar happened")
        );
    }

    #[test]
    fn replica_status_lines_annotates_stuck_container_with_reason() {
        use crate::azure::container_app_replicas::{ReplicaContainer, ReplicaInstance};
        let replicas = vec![ReplicaInstance {
            name: "ca--rev-d4f9k".into(),
            created_at: None,
            running_state: Some("NotRunning".into()),
            containers: vec![ReplicaContainer {
                name: "maintenance".into(),
                ready: Some(false),
                started: Some(false),
                restart_count: 0,
                running_state: Some("Waiting".into()),
                running_state_details: Some("Back-off pulling image \"acr.io/api:bad\"".into()),
            }],
        }];
        let lines = replica_status_lines(Some(&replicas), false, None);
        // The container sub-row spells out why it's stuck.
        let sub = &lines[1].1;
        assert!(sub.contains("restarts 0"), "got {sub:?}");
        assert!(sub.contains("image pull failed"), "got {sub:?}");
    }

    #[test]
    fn replica_modal_lines_lists_name_timing_and_containers() {
        use crate::azure::container_app_replicas::{ReplicaContainer, ReplicaInstance};
        use chrono::TimeZone;
        let replica = ReplicaInstance {
            name: "ca--rev-suffix-r58pz".into(),
            created_at: Some(
                chrono::Utc
                    .with_ymd_and_hms(2026, 5, 26, 16, 37, 49)
                    .unwrap(),
            ),
            running_state: Some("Running".into()),
            containers: vec![
                ReplicaContainer {
                    name: "files".into(),
                    ready: Some(true),
                    started: Some(true),
                    restart_count: 2,
                    running_state: Some("Running".into()),
                    running_state_details: None,
                },
                ReplicaContainer {
                    name: "http-auth".into(),
                    ready: Some(false),
                    started: Some(true),
                    restart_count: 0,
                    running_state: Some("Waiting".into()),
                    running_state_details: None,
                },
            ],
        };
        let lines = replica_modal_lines(&replica);
        // Full name (not the trimmed suffix), full timestamp, replica-level state.
        assert!(lines.iter().any(|l| l == "name: ca--rev-suffix-r58pz"));
        assert!(lines
            .iter()
            .any(|l| l.contains("created: 2026-05-26 16:37:49 UTC")));
        assert!(lines.iter().any(|l| l == "running state: Running"));
        assert!(lines.iter().any(|l| l == "containers:"));
        // Per-container: readiness glyph + its own restart count + running state.
        let files = lines.iter().find(|l| l.contains("files")).unwrap();
        assert!(files.contains('\u{2713}'), "ready glyph: {files:?}");
        assert!(files.contains("restarts 2"), "restart count: {files:?}");
        assert!(files.contains("(Running)"), "container state: {files:?}");
        let auth = lines.iter().find(|l| l.contains("http-auth")).unwrap();
        assert!(auth.contains('\u{2717}'), "not-ready glyph: {auth:?}");
        assert!(auth.contains("(Waiting)"), "container state: {auth:?}");
    }

    #[test]
    fn replicas_modal_keeps_preamble_but_detail_lines_drop_it() {
        use crate::azure::container_app_replicas::ReplicaInstance;
        use chrono::TimeZone;
        let older = ReplicaInstance {
            name: "ca--rev-older".into(),
            created_at: Some(chrono::Utc.with_ymd_and_hms(2026, 5, 26, 10, 0, 0).unwrap()),
            running_state: Some("Running".into()),
            containers: vec![],
        };
        let newer = ReplicaInstance {
            name: "ca--rev-newer".into(),
            created_at: Some(chrono::Utc.with_ymd_and_hms(2026, 5, 26, 12, 0, 0).unwrap()),
            running_state: Some("Running".into()),
            containers: vec![],
        };
        let modal = replicas_modal_lines(&[older.clone(), newer.clone()]);
        // Modal leads with the runtime-vs-config prose.
        assert!(modal[0].contains("Live replica instances"));
        // Newest replica precedes the older one.
        let pos_newer = modal
            .iter()
            .position(|l| l.contains("ca--rev-newer"))
            .unwrap();
        let pos_older = modal
            .iter()
            .position(|l| l.contains("ca--rev-older"))
            .unwrap();
        assert!(pos_newer < pos_older, "newest-first ordering");

        // The detail-only helper (what `y` copies) omits the prose entirely.
        let detail = replicas_detail_lines(&[older, newer]);
        assert!(!detail.iter().any(|l| l.contains("Live replica instances")));
        assert!(detail.iter().any(|l| l.contains("ca--rev-newer")));
    }

    #[test]
    fn instances_section_yank_excludes_prose_but_modal_keeps_it() {
        use crate::azure::container_app_overview::ContainerAppOverview;
        use crate::azure::container_app_replicas::{ReplicaContainer, ReplicaInstance};
        use crate::azure::container_app_revisions::ActiveRevisionMeta;
        let mut state = fixture_no_metrics();
        state.resources[0].kind = ResourceKind::ContainerApp;
        let res = state.resources[0].clone();
        // Both caches present so the meta + replica blocks build (not skeleton).
        state
            .revision_meta
            .by_resource
            .insert(res.id.clone(), ActiveRevisionMeta::default());
        state
            .container_app_overview
            .by_resource
            .insert(res.id.clone(), ContainerAppOverview::default());
        state.replica_instances.by_resource.insert(
            res.id.clone(),
            vec![ReplicaInstance {
                name: "ca--rev-r58pz".into(),
                created_at: None,
                running_state: Some("Running".into()),
                containers: vec![ReplicaContainer {
                    name: "files".into(),
                    ready: Some(true),
                    started: Some(true),
                    restart_count: 0,
                    running_state: Some("Running".into()),
                    running_state_details: None,
                }],
            }],
        );
        let metas = selectable_metas(&state, &res);
        let inst = metas
            .iter()
            .find(|m| m.modal_title == "instances")
            .expect("instances section present");
        // Modal body keeps the explanatory preamble…
        assert!(inst
            .modal_lines
            .iter()
            .any(|l| l.contains("Live replica instances")));
        // …but the yank is data-only, per SelectableMeta's "plain value" contract.
        assert!(
            !inst.yank.contains("Live replica instances"),
            "yank leaked prose: {:?}",
            inst.yank
        );
        assert!(inst.yank.contains("ca--rev-r58pz"));
    }

    #[test]
    fn container_app_detail_renders_all_sections_and_scrolls_to_cursor() {
        // Loaded Container App with the richest header (multi-row container-config
        // + instances blocks + tags). Guards two things at once: render builds the
        // same sections selectable_metas counts (the handler clamps the cursor
        // against that count), and the context pane scrolls so a low section under
        // the cursor is actually visible on a short terminal.
        use crate::azure::container_app_overview::{ContainerAppOverview, ContainerSpec};
        use crate::azure::container_app_replicas::{ReplicaContainer, ReplicaInstance};
        use crate::azure::container_app_revisions::ActiveRevisionMeta;
        use crate::azure::resources::ResourceMeta;
        let theme = Theme::catppuccin_mocha();
        let mut state = fixture_no_metrics();
        state.resources[0].kind = ResourceKind::ContainerApp;
        // A tag gives us a low general section to scroll to.
        state.resources[0].meta = ResourceMeta {
            tags: vec![("Domain".into(), "files".into())],
            ..Default::default()
        };
        let res = state.resources[0].clone();
        state.revision_meta.by_resource.insert(
            res.id.clone(),
            ActiveRevisionMeta {
                name: "files--0000004".into(),
                image: Some("acr/files:abc".into()),
                replicas: 1,
                min_replicas: 1,
                max_replicas: 10,
                ..Default::default()
            },
        );
        state.container_app_overview.by_resource.insert(
            res.id.clone(),
            ContainerAppOverview {
                cpu_millicores: 500,
                memory_bytes: 512 * 1024 * 1024,
                fqdn: Some("files.example.azurecontainerapps.io".into()),
                ingress_external: Some(true),
                containers: vec![
                    ContainerSpec {
                        name: "files".into(),
                        image: Some("acr/files:abc".into()),
                        cpu_millicores: 250,
                        memory_bytes: 256 * 1024 * 1024,
                        ephemeral_storage: Some("2Gi".into()),
                        env_vars: Vec::new(),
                        is_init: false,
                    },
                    ContainerSpec {
                        name: "http-auth".into(),
                        image: Some("acr/http-auth:abc".into()),
                        cpu_millicores: 250,
                        memory_bytes: 256 * 1024 * 1024,
                        ephemeral_storage: None,
                        env_vars: Vec::new(),
                        is_init: false,
                    },
                ],
                ..Default::default()
            },
        );
        state.replica_instances.by_resource.insert(
            res.id.clone(),
            vec![ReplicaInstance {
                name: "files--0000004-abc-r58pz".into(),
                created_at: None,
                running_state: Some("Running".into()),
                containers: vec![
                    ReplicaContainer {
                        name: "files".into(),
                        ready: Some(true),
                        started: Some(true),
                        restart_count: 0,
                        running_state: Some("Running".into()),
                        running_state_details: None,
                    },
                    ReplicaContainer {
                        name: "http-auth".into(),
                        ready: Some(true),
                        started: Some(true),
                        restart_count: 0,
                        running_state: Some("Running".into()),
                        running_state_details: None,
                    },
                ],
            }],
        );

        let count = selectable_metas(&state, &res).len();
        assert!(
            count >= 4,
            "state + meta + instances + tags at minimum, got {count}"
        );

        // Short terminal so the rich CA overflows the context pane.
        let backend = TestBackend::new(80, 22);
        let mut term = Terminal::new(backend).unwrap();
        state.view = View::Detail;

        // Cursor on the last section (the tags row, well below the fold): the
        // scroll offset must pull it into view. Without it this row clips.
        state.detail_view.cursor = count - 1;
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let bottom = format!("{:?}", term.backend().buffer());
        assert!(
            bottom.contains("Domain"),
            "last section scrolled into view: {bottom}"
        );

        // Cursor at the top: the upper meta rows are visible (scroll resets to 0).
        state.detail_view.cursor = 0;
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let top = format!("{:?}", term.backend().buffer());
        assert!(top.contains("rev:"), "top sections visible: {top}");
    }

    #[test]
    fn summary_for_cpu_appends_max_when_limits_present() {
        use crate::azure::container_app_overview::ContainerAppOverview;
        use crate::azure::metrics::{MetricKind, MetricPoint, MetricSeries};
        use chrono::Utc;

        let series = MetricSeries {
            kind: MetricKind::Cpu,
            label: String::new(),
            unit: "mCores".into(),
            points: vec![
                MetricPoint {
                    ts: Utc::now(),
                    value: 86.4,
                },
                MetricPoint {
                    ts: Utc::now(),
                    value: 12.5,
                },
            ],
            peak_replica: None,
        };
        let limits = ContainerAppOverview {
            cpu_millicores: 500,
            memory_bytes: 0,
            fqdn: None,
            ..Default::default()
        };
        let out = summary_for(MetricKind::Cpu, &series, Some(&limits));
        // `latest / highest / max`: last sample, window peak, configured limit.
        assert!(out.contains("latest: 12.5"));
        assert!(out.contains("/ highest: 86.4"), "got {out:?}");
        assert!(out.contains("/ max 500 mCores"), "got {out:?}");
    }

    #[test]
    fn summary_for_cpu_shows_peak_replica_when_above_highest() {
        use crate::azure::metrics::{MetricKind, MetricPoint, MetricSeries};
        use chrono::Utc;

        let series = MetricSeries {
            kind: MetricKind::Cpu,
            label: String::new(),
            unit: "mCores".into(),
            points: vec![
                MetricPoint {
                    ts: Utc::now(),
                    value: 86.4,
                },
                MetricPoint {
                    ts: Utc::now(),
                    value: 42.3,
                },
            ],
            // Busiest replica well above the across-replica peak (86.4).
            peak_replica: Some(210.0),
        };
        let out = summary_for(MetricKind::Cpu, &series, None);
        assert!(out.contains("/ peak-replica: 210"), "got {out:?}");
    }

    #[test]
    fn summary_for_cpu_omits_peak_replica_when_not_above_highest() {
        use crate::azure::metrics::{MetricKind, MetricPoint, MetricSeries};
        use chrono::Utc;

        // Single-replica app: peak == highest, so the redundant line is dropped.
        let series = MetricSeries {
            kind: MetricKind::Cpu,
            label: String::new(),
            unit: "mCores".into(),
            points: vec![MetricPoint {
                ts: Utc::now(),
                value: 86.4,
            }],
            peak_replica: Some(86.4),
        };
        let out = summary_for(MetricKind::Cpu, &series, None);
        assert!(!out.contains("peak-replica"), "got {out:?}");
    }

    #[test]
    fn container_app_cpu_memory_labels_flag_per_replica_average() {
        assert_eq!(
            metric_row_label(MetricKind::Cpu, ResourceKind::ContainerApp),
            "CPU (avg/replica)"
        );
        assert_eq!(
            metric_row_label(MetricKind::Memory, ResourceKind::ContainerApp),
            "Memory (avg/replica)"
        );
        // Other resource kinds keep the plain labels.
        assert_eq!(
            metric_row_label(MetricKind::Cpu, ResourceKind::FunctionApp),
            "CPU"
        );
    }

    #[test]
    fn summary_for_cpu_omits_max_when_limits_zero() {
        use crate::azure::container_app_overview::ContainerAppOverview;
        use crate::azure::metrics::{MetricKind, MetricPoint, MetricSeries};
        use chrono::Utc;

        let series = MetricSeries {
            kind: MetricKind::Cpu,
            label: String::new(),
            unit: "mCores".into(),
            points: vec![MetricPoint {
                ts: Utc::now(),
                value: 4.7,
            }],
            peak_replica: None,
        };
        let limits = ContainerAppOverview {
            cpu_millicores: 0,
            memory_bytes: 0,
            fqdn: None,
            ..Default::default()
        };
        let out = summary_for(MetricKind::Cpu, &series, Some(&limits));
        assert!(!out.contains("/ max"), "got {out:?}");
    }

    #[test]
    fn format_count_renders_zero_without_negative_sign() {
        // Regression: `format!("{:.0}", -0.0_f64)` yields "-0", and
        // `f64::max(-0.0, 0.0)` does not reliably strip the negative sign
        // across platforms.
        assert_eq!(format_count(-0.0), "0");
        assert_eq!(format_count(0.0), "0");
        assert_eq!(format_count(f64::NAN), "0");
        assert_eq!(format_count(-5.0), "0");
        assert_eq!(format_count(42.0), "42");
        assert_eq!(format_count(2_500.0), "2.5k");
    }

    fn r() -> Resource {
        Resource {
            id: "/r/one".into(),
            name: "alpha-func".into(),
            kind: ResourceKind::FunctionApp,
            location: "westeurope".into(),
            resource_group: "rg-demo".into(),
            subscription_id: "sub".into(),
            state: Some("Running".into()),
            created_at: None,
            modified_at: None,
            meta: Default::default(),
        }
    }

    fn series(kind: MetricKind, label: &str, vals: &[f64]) -> MetricSeries {
        let now = Utc::now();
        MetricSeries {
            kind,
            label: label.into(),
            unit: match kind {
                MetricKind::Traffic
                | MetricKind::Executions
                | MetricKind::Errors
                | MetricKind::ClientErrors => "count".into(),
                MetricKind::Cpu => "%".into(),
                MetricKind::Memory => "bytes".into(),
                MetricKind::Dtu | MetricKind::Storage | MetricKind::Workers => "%".into(),
            },
            points: vals
                .iter()
                .enumerate()
                .map(|(i, v)| MetricPoint {
                    ts: now - Duration::minutes((vals.len() - i) as i64 * 5),
                    value: *v,
                })
                .collect(),
            peak_replica: None,
        }
    }

    fn fixture_no_metrics() -> AppState {
        let mut s = AppState::new(Config::default());
        s.resources = vec![r()];
        s.list_cursor = 0;
        s.view = View::Detail;
        s
    }

    #[test]
    fn renders_without_metrics() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(80, 20);
        let mut term = Terminal::new(backend).unwrap();
        let state = fixture_no_metrics();
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("alpha-func"));
        assert!(s.contains("Requests"));
        assert!(s.contains("Memory"));
        assert!(s.contains("LOADING"));
    }

    #[test]
    fn renders_no_selection() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(60, 12);
        let mut term = Terminal::new(backend).unwrap();
        let state = AppState::new(Config::default());
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
    }

    #[test]
    fn back_is_not_consumed_by_view() {
        // Detail view must NOT consume Action::Back — it falls through to the
        // global handler which pops the view_stack. Consuming it here would
        // re-introduce bug_009: stamping previous_view = Some(Detail) when
        // leaving Detail caused the next Esc to bounce right back in.
        let mut state = fixture_no_metrics();
        assert!(!handle(Action::Back, &mut state));
        assert_eq!(
            state.view,
            View::Detail,
            "view-local handler must not transition on Back"
        );
    }

    #[test]
    fn set_window_day_clears_cache_for_selected() {
        let mut state = fixture_no_metrics();
        state.metrics.range = TimeRange::Week;
        state.metrics.by_resource.insert(
            "/r/one".into(),
            vec![series(MetricKind::Traffic, "Requests", &[1.0])],
        );
        assert!(handle(Action::SetWindowDay, &mut state));
        assert_eq!(state.metrics.range, TimeRange::Day);
        assert!(!state.metrics.by_resource.contains_key("/r/one"));
    }

    #[test]
    fn open_logs_function_app_transitions() {
        let mut state = fixture_no_metrics();
        assert!(handle(Action::OpenLogs, &mut state));
        assert_eq!(state.view, View::Logs);
    }

    #[test]
    fn open_logs_resets_stale_source_and_search_filters() {
        // Regression: a source filter set on a previous app (e.g. a container
        // name) carried into another app's logs and hid every line. Opening
        // Logs must clear the resource-specific filters.
        let mut state = fixture_no_metrics();
        state.logs.source_filter = Some("maintenance".into());
        state.logs.search_active = true;
        state.logs.search_input = tui_input::Input::default().with_value("boom".into());
        state.logs.visual_anchor = Some(3);

        assert!(handle(Action::OpenLogs, &mut state));
        assert_eq!(state.view, View::Logs);
        assert_eq!(state.logs.source_filter, None);
        assert!(!state.logs.search_active);
        assert_eq!(state.logs.search_input.value(), "");
        assert_eq!(state.logs.visual_anchor, None);
    }

    #[test]
    fn open_logs_apim_transitions() {
        // APIM gateway request logs are supported, so `l` opens the logs view.
        let mut state = fixture_no_metrics();
        state.resources[0].kind = ResourceKind::Apim;
        assert!(handle(Action::OpenLogs, &mut state));
        assert_eq!(state.view, View::Logs);
    }

    #[test]
    fn open_logs_appgw_blocks() {
        let mut state = fixture_no_metrics();
        state.resources[0].kind = ResourceKind::AppGateway;
        assert!(handle(Action::OpenLogs, &mut state));
        assert_eq!(state.view, View::Detail);
        assert!(state.status_message.is_some());
    }

    #[test]
    fn state_color_maps_known_states() {
        let theme = Theme::catppuccin_mocha();
        assert_eq!(state_color("Running", &theme), theme.fg);
        assert_eq!(state_color("Stopped", &theme), theme.critical);
        assert_eq!(state_color("Suspended", &theme), theme.degraded);
        assert_eq!(state_color("Starting", &theme), theme.accent);
        assert_eq!(state_color("Stopping", &theme), theme.accent);
        assert_eq!(state_color("Progressing", &theme), theme.accent);
        // Anything unfamiliar falls back to muted rather than misleadingly
        // colouring an unknown lifecycle state.
        assert_eq!(state_color("Wibble", &theme), theme.muted);
        assert_eq!(state_color("unknown", &theme), theme.muted);
    }

    #[test]
    fn formatters() {
        assert_eq!(format_count(0.4), "0");
        assert_eq!(format_count(999.0), "999");
        assert_eq!(format_count(12_500.0), "12.5k");
        assert_eq!(format_count(2_400_000.0), "2.4M");
        assert!(format_bytes(2.0 * 1024.0 * 1024.0).contains("MB"));
    }

    #[test]
    fn ownership_value_full_timestamp_and_principal_forms() {
        use chrono::TimeZone;
        let d = Utc.with_ymd_and_hms(2026, 5, 8, 14, 29, 55).unwrap();
        // Application principal: GUID shown when unresolved, full timestamp.
        assert_eq!(
            ownership_value(
                Some(&d),
                Some("11111111-2222-3333-4444-555555555555"),
                Some("Application"),
                None,
            ),
            Some(
                "2026-05-08 14:29:55 UTC by 11111111-2222-3333-4444-555555555555 (Application)"
                    .to_string()
            )
        );
        // …and the resolved display name when Graph has it.
        assert_eq!(
            ownership_value(
                Some(&d),
                Some("11111111-2222-3333-4444-555555555555"),
                Some("Application"),
                Some("di-sp-adp-devops-agent"),
            ),
            Some("2026-05-08 14:29:55 UTC by di-sp-adp-devops-agent (Application)".to_string())
        );
        // User principal is a readable UPN — show it verbatim.
        assert_eq!(
            ownership_value(Some(&d), Some("someone@example.com"), Some("User"), None),
            Some("2026-05-08 14:29:55 UTC by someone@example.com (User)".to_string())
        );
        assert_eq!(ownership_value(None, None, None, None), None);
    }

    #[test]
    fn general_meta_lines_emits_tags_and_ownership() {
        use crate::azure::resources::ResourceMeta;
        use crate::ui::state::PrincipalCache;
        let mut res = r();
        res.meta = ResourceMeta {
            created_by: Some("someone@example.com".into()),
            created_by_type: Some("User".into()),
            modified_by: None,
            modified_by_type: None,
            tags: vec![
                ("Domain".into(), "tool".into()),
                ("Terraform".into(), "true".into()),
            ],
            ..Default::default()
        };
        let lines = general_meta_lines(&res, &PrincipalCache::default());
        let labels: Vec<&str> = lines.iter().map(|(l, _, _)| l.as_str()).collect();
        assert_eq!(labels, vec!["tags:", "created:"]);
        let tags = &lines.iter().find(|(l, _, _)| l == "tags:").unwrap().1;
        assert_eq!(tags, "Domain=tool, Terraform=true");
    }

    #[test]
    fn general_meta_lines_uses_resolved_principal_name() {
        use crate::azure::resources::ResourceMeta;
        use crate::ui::state::PrincipalCache;
        let guid = "11111111-2222-3333-4444-555555555555";
        let mut res = r();
        res.meta = ResourceMeta {
            created_by: Some(guid.into()),
            created_by_type: Some("Application".into()),
            ..Default::default()
        };
        let mut principals = PrincipalCache::default();
        principals
            .by_id
            .insert(guid.into(), "di-sp-adp-devops-agent".into());
        let lines = general_meta_lines(&res, &principals);
        let created = &lines.iter().find(|(l, _, _)| l == "created:").unwrap().1;
        assert!(
            created.contains("di-sp-adp-devops-agent (Application)"),
            "got {created:?}"
        );
        assert!(!created.contains(guid), "GUID should be replaced by name");
    }

    #[test]
    fn general_meta_lines_empty_without_meta() {
        // r() has Default::default() meta — no tags, no authorship, no dates.
        assert!(general_meta_lines(&r(), &crate::ui::state::PrincipalCache::default()).is_empty());
    }

    fn ca_state_with_env(vars: Vec<crate::azure::env_vars::EnvVar>) -> (AppState, Resource) {
        use crate::azure::container_app_overview::ContainerAppOverview;
        let mut state = fixture_no_metrics();
        state.resources[0].kind = ResourceKind::ContainerApp;
        let id = state.resources[0].id.clone();
        state.container_app_overview.by_resource.insert(
            id,
            ContainerAppOverview {
                env_vars: vars,
                ..Default::default()
            },
        );
        let resource = state.resources[0].clone();
        (state, resource)
    }

    fn ev(name: &str, value: &str, is_secret: bool) -> crate::azure::env_vars::EnvVar {
        crate::azure::env_vars::EnvVar {
            name: name.into(),
            value: value.into(),
            is_secret,
            ..Default::default()
        }
    }

    #[test]
    fn env_var_rows_is_a_count_teaser_pointing_at_the_page() {
        let theme = Theme::catppuccin_mocha();
        let (state, resource) =
            ca_state_with_env(vec![ev("A", "1", false), ev("B", "(secret: s)", true)]);
        // The overview shows a one-line teaser; the values live on the page.
        let rows = env_var_rows(&state, &resource, &theme);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].1.contains("env vars: 2"));
        assert!(rows[0].1.contains("[e to view]"));
        // Crucially, no values leak into the overview teaser.
        assert!(!rows[0].1.contains("A=1"));
    }

    #[test]
    fn env_var_rows_empty_when_no_vars() {
        let theme = Theme::catppuccin_mocha();
        let (state, resource) = ca_state_with_env(vec![]);
        assert!(env_var_rows(&state, &resource, &theme).is_empty());
    }

    #[test]
    fn env_var_rows_function_app_permission_hint() {
        let theme = Theme::catppuccin_mocha();
        let mut state = fixture_no_metrics(); // r() is a Function App
        let resource = state.resources[0].clone();
        state
            .func_settings
            .failures
            .insert(resource.id.clone(), "azure api error 403: Forbidden".into());
        let rows = env_var_rows(&state, &resource, &theme);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].1.contains("config/list permission"));
    }

    #[test]
    fn open_env_vars_transitions_to_page_for_function_app() {
        let mut state = fixture_no_metrics(); // Function App
        state.env_vars_view.cursor = 5; // stale state from a prior visit
        state.env_vars_view.revealed = true;
        assert!(handle(Action::OpenEnvVars, &mut state));
        assert_eq!(state.view, View::EnvVars);
        // Page opens fresh: masked, cursor at top.
        assert_eq!(state.env_vars_view.cursor, 0);
        assert!(!state.env_vars_view.revealed);
    }

    #[test]
    fn open_env_vars_blocks_unsupported_kind() {
        let mut state = fixture_no_metrics();
        state.resources[0].kind = ResourceKind::Apim;
        assert!(handle(Action::OpenEnvVars, &mut state));
        assert_eq!(state.view, View::Detail);
        assert!(state.status_message.is_some());
    }

    #[test]
    fn function_runtime_summary_combines_runtime_and_version() {
        let vars = vec![
            ev("FUNCTIONS_EXTENSION_VERSION", "~4", false),
            ev("FUNCTIONS_WORKER_RUNTIME", "python", false),
        ];
        assert_eq!(
            function_runtime_summary(&vars).as_deref(),
            Some("python \u{00b7} ~4")
        );
    }

    #[test]
    fn function_runtime_summary_partials_and_none() {
        assert_eq!(
            function_runtime_summary(&[ev("FUNCTIONS_WORKER_RUNTIME", "node", false)]).as_deref(),
            Some("node")
        );
        assert_eq!(
            function_runtime_summary(&[ev("FUNCTIONS_EXTENSION_VERSION", "~4", false)]).as_deref(),
            Some("Functions ~4")
        );
        assert!(function_runtime_summary(&[ev("OTHER", "x", false)]).is_none());
    }

    #[test]
    fn function_app_meta_lines_shows_image_then_runtime() {
        let mut state = fixture_no_metrics(); // r() is a Function App
        let resource = state.resources[0].clone();
        state.func_image.by_resource.insert(
            resource.id.clone(),
            Some("myacr.azurecr.io/api:abc123".into()),
        );
        state.func_settings.by_resource.insert(
            resource.id.clone(),
            vec![ev("FUNCTIONS_WORKER_RUNTIME", "dotnet-isolated", false)],
        );
        let lines = function_app_meta_lines(&state, &resource);
        let labels: Vec<&str> = lines.iter().map(|(l, _, _)| l.as_str()).collect();
        // The network posture row is appended after image + runtime.
        assert_eq!(labels, vec!["image:", "runtime:", "network:"]);
        assert_eq!(lines[0].1, "myacr.azurecr.io/api:abc123");
        assert_eq!(lines[1].1, "dotnet-isolated");
        assert_eq!(lines[2].1, "public access enabled");
    }

    #[test]
    fn function_app_meta_lines_code_deployed_has_no_image() {
        let mut state = fixture_no_metrics();
        let resource = state.resources[0].clone();
        // Code-deployed apps cache `Some(None)` — no image line, no panic.
        state
            .func_image
            .by_resource
            .insert(resource.id.clone(), None);
        let lines = function_app_meta_lines(&state, &resource);
        assert!(lines.iter().all(|(l, _, _)| l != "image:"));
    }

    #[test]
    fn function_trigger_lines_one_row_per_function() {
        use crate::azure::function_app_triggers::FunctionTrigger;
        let triggers = vec![
            FunctionTrigger {
                function: "Api".into(),
                kind: "http".into(),
                detail: Some("GET, POST".into()),
            },
            FunctionTrigger {
                function: "Cleanup".into(),
                kind: "timer".into(),
                detail: Some("0 0 * * * *".into()),
            },
        ];
        let lines = function_trigger_lines(Some(&triggers), false, None);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].0, "triggers:");
        // Continuation row shares a blank label for column alignment.
        assert_eq!(lines[1].0.trim(), "");
        assert!(lines[0].1.contains("Api"));
        assert!(lines[0].1.contains("http: GET, POST"));
        assert!(lines[1].1.contains("timer: 0 0 * * * *"));
    }

    #[test]
    fn function_trigger_lines_empty_collapses_pending_and_failure_hint() {
        assert!(function_trigger_lines(Some(&vec![]), false, None).is_empty());
        let loading = function_trigger_lines(None, true, None);
        assert_eq!(loading.len(), 1);
        assert!(loading[0].1.contains("loading"));
        let err = "azure api error 403: Forbidden".to_string();
        let failed = function_trigger_lines(None, false, Some(&err));
        assert_eq!(failed.len(), 1);
        assert!(failed[0].1.contains("permission denied"));
    }

    #[test]
    fn renders_function_app_image_runtime_and_triggers() {
        use crate::azure::function_app_triggers::FunctionTrigger;
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(80, 30);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture_no_metrics(); // r() is a Function App
        let id = state.resources[0].id.clone();
        state
            .func_image
            .by_resource
            .insert(id.clone(), Some("myacr.azurecr.io/api:abc".into()));
        state.func_settings.by_resource.insert(
            id.clone(),
            vec![ev("FUNCTIONS_WORKER_RUNTIME", "python", false)],
        );
        state.func_triggers.by_resource.insert(
            id.clone(),
            vec![FunctionTrigger {
                function: "Ingest".into(),
                kind: "kafka".into(),
                detail: Some("events".into()),
            }],
        );
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("image:"));
        assert!(s.contains("runtime:"));
        assert!(s.contains("triggers:"));
        assert!(s.contains("Ingest"));
        assert!(s.contains("kafka"));
    }

    #[test]
    fn function_trigger_lines_caps_with_more_row() {
        use crate::azure::function_app_triggers::FunctionTrigger;
        let triggers: Vec<FunctionTrigger> = (0..TRIGGER_CAP + 3)
            .map(|i| FunctionTrigger {
                function: format!("fn{i}"),
                kind: "queue".into(),
                detail: Some("q".into()),
            })
            .collect();
        let lines = function_trigger_lines(Some(&triggers), false, None);
        // TRIGGER_CAP rows + one overflow summary.
        assert_eq!(lines.len(), TRIGGER_CAP + 1);
        assert!(lines.last().unwrap().1.contains("+3 more"));
    }

    #[test]
    fn triggers_collapse_to_one_section_whose_modal_expands_every_function() {
        use crate::azure::function_app_triggers::FunctionTrigger;
        let n = TRIGGER_CAP + 3;
        let mut state = fixture_no_metrics(); // r() is a Function App
        let resource = state.resources[0].clone();
        state.func_triggers.by_resource.insert(
            resource.id.clone(),
            (0..n)
                .map(|i| FunctionTrigger {
                    function: format!("fn{i}"),
                    kind: "queue".into(),
                    detail: Some(format!("q{i}")),
                })
                .collect(),
        );

        let metas = selectable_metas(&state, &resource);
        // The whole trigger block is a single navigable section, not one stop
        // per line — j/k lands on it exactly once.
        let trigger_sections: Vec<&SelectableMeta> = metas
            .iter()
            .filter(|m| m.modal_title == "triggers")
            .collect();
        assert_eq!(trigger_sections.len(), 1, "triggers fold into one section");

        // Enter expands the inline `+N more`: the modal lists every function and
        // never shows the literal overflow summary.
        let modal = &trigger_sections[0].modal_lines;
        assert_eq!(modal.len(), n, "modal shows all {n} triggers uncapped");
        assert!(modal.iter().all(|l| !l.contains("more")));
        assert!(modal.iter().any(|l| l.contains("fn0")));
        assert!(modal.iter().any(|l| l.contains(&format!("fn{}", n - 1))));
    }

    #[test]
    fn render_section_count_matches_selectable_metas_for_overflowing_triggers() {
        // Guards the render/selectable_metas alignment invariant: the number of
        // highlightable sections render builds must equal `selectable_metas`'
        // length, even when triggers overflow into a `+N more` row.
        use crate::azure::function_app_triggers::FunctionTrigger;
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(100, 40);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture_no_metrics();
        let resource = state.resources[0].clone();
        state.func_triggers.by_resource.insert(
            resource.id.clone(),
            (0..TRIGGER_CAP + 5)
                .map(|i| FunctionTrigger {
                    function: format!("fn{i}"),
                    kind: "timer".into(),
                    detail: None,
                })
                .collect(),
        );
        state.view = View::Detail;

        // Park the cursor on the last section and render: a clamp/alignment bug
        // would panic or mis-highlight. `selectable_metas` is the source of truth
        // for the count the handler clamps against.
        let count = selectable_metas(&state, &resource).len();
        assert!(count >= 2, "state + triggers at minimum");
        state.detail_view.cursor = count - 1;
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        // Inline preview still teases the overflow; the section is one stop.
        assert!(s.contains("+5 more"));
    }

    #[test]
    fn wrapped_line_count_word_wraps_like_ratatui() {
        // Word wrap breaks *earlier* than a char count suggests: each word
        // that would straddle the edge moves wholesale to the next row. The
        // old `ceil(chars / width)` estimate under-counted this, clipping the
        // bottom meta rows.
        //
        // width 10: "tags:" | "env=prod" | "team=platf" | "orm" → 4 rows,
        // where ceil(28 / 10) would claim 3.
        assert_eq!(wrapped_line_count("tags: env=prod team=platform", 10), 4);
        // Exact fits don't wrap.
        assert_eq!(wrapped_line_count("aaaa bbbb", 9), 1);
        // The break's whitespace is dropped, not carried onto the next row.
        assert_eq!(wrapped_line_count("aaaa bbbb", 5), 2);
        // Long unbroken words hard-split at the width boundary.
        assert_eq!(wrapped_line_count(&"x".repeat(25), 10), 3);
        // Hard newlines each start a row; empty text still occupies one.
        assert_eq!(wrapped_line_count("a\nb\nc", 10), 3);
        assert_eq!(wrapped_line_count("", 10), 1);
    }

    #[test]
    fn wrapped_line_count_uses_display_width_for_wide_glyphs() {
        // 8 CJK chars are 16 display columns: 2 rows at width 10, where a
        // char-based count (8 chars) would claim a single row.
        assert_eq!(wrapped_line_count("日本語のテキスト", 10), 2);
        // A wide glyph that would straddle the row edge wraps early, so rows
        // can't be derived by dividing total width: at width 3 only one
        // 2-column glyph fits per row → 8 rows, where ceil(16 / 3) claims 6.
        assert_eq!(wrapped_line_count("日本語のテキスト", 3), 8);
    }

    #[test]
    fn modal_goto_bottom_reaches_tail_and_move_up_responds_immediately() {
        // Regression, twofold: (1) the modal's render clamp was `min(len)` on
        // unwrapped lines — G scrolled the entire content out of view (blank
        // modal); (2) G stored u16::MAX while only the renderer clamped it, so
        // `k` decremented invisibly for thousands of presses.
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(80, 20);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture_no_metrics();
        state.detail_view.modal = Some(crate::ui::state::DetailModal {
            title: "image".into(),
            lines: (0..40).map(|i| format!("line {i}")).collect(),
            scroll: 0,
        });
        term.draw(|f| render_modal(f, f.area(), &state, &theme))
            .unwrap();

        assert!(handle(Action::GotoBottom, &mut state));
        let max = state.scroll_max.get();
        assert!(
            max > 0 && max < 40,
            "expected a small real max scroll, got {max}"
        );
        assert_eq!(state.detail_view.modal.as_ref().unwrap().scroll, max);

        // The tail must still be on screen at the clamped offset.
        term.draw(|f| render_modal(f, f.area(), &state, &theme))
            .unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("line 39"), "tail line must stay visible after G");

        assert!(handle(Action::MoveUp, &mut state));
        assert_eq!(state.detail_view.modal.as_ref().unwrap().scroll, max - 1);
    }

    #[test]
    fn open_logs_resets_previous_resource_scroll() {
        // Regression: `reset_view_filters` cleared the filters but not the
        // cursor/viewport, so opening resource B's logs after deep-scrolling
        // resource A's left `k` apparently dead for hundreds of presses.
        let mut state = fixture_no_metrics();
        state.logs.scroll = 500;
        state.logs.view_top.set(480);
        state.logs.h_offset = 24;
        assert!(handle(Action::OpenLogs, &mut state));
        assert_eq!(state.view, View::Logs);
        assert_eq!(state.logs.scroll, 0);
        assert_eq!(state.logs.view_top.get(), 0);
        assert_eq!(state.logs.h_offset, 0);
    }
}
