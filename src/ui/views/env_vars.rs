//! Dedicated, scrollable page listing an API asset's OS environment variables
//! (Container App template env / Function App app-settings). Opened with `e`
//! from the Detail view. Values are masked by default; `x` reveals them
//! (k9s-style decode). The list itself is read from the per-resource caches via
//! [`crate::ui::views::detail::env_vars_for`]; this view only owns scroll +
//! reveal state ([`crate::ui::state::EnvVarsView`]).

#![allow(dead_code, unused_variables)]

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::azure::resources::ResourceKind;
use crate::ui::events::Action;
use crate::ui::state::AppState;
use crate::ui::theme::Theme;
use crate::ui::views::detail::env_vars_for;

const FOOTER: &str = "x reveal/hide  h/l scroll  y yank  j/k move  Esc back  q quit";
const HALF_PAGE: usize = 10;
/// Characters scrolled per `h`/`l` press for long revealed values.
const H_SCROLL_STEP: usize = 8;
/// Fixed-width mask so a value's length doesn't leak while hidden.
const MASK: &str = "••••••••";
/// Name column width; longer names are ellipsized.
const NAME_COL: usize = 36;
/// Width of the per-container attribution (`in`) column, rendered between the
/// name and the value for multi-container Container Apps. Longer lists are
/// ellipsized. Hidden entirely when no row carries attribution.
const ATTR_COL: usize = 18;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(area);

    let resource = state.selected_resource();
    let name = resource
        .map(|r| r.name.as_str())
        .unwrap_or("(no selection)");
    let title = Paragraph::new(Line::from(vec![
        Span::styled(
            " env vars ",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            name,
            Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            if state.env_vars_view.revealed {
                "  · revealed"
            } else {
                "  · masked (x to reveal)"
            },
            Style::default().fg(theme.muted),
        ),
    ]));
    frame.render_widget(title, chunks[0]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(" values ", Style::default().fg(theme.fg)));
    let inner = block.inner(chunks[1]);
    frame.render_widget(block, chunks[1]);

    render_body(frame, inner, state, resource, theme);

    let footer = Paragraph::new(Line::from(Span::styled(
        FOOTER,
        Style::default().fg(theme.muted),
    )));
    frame.render_widget(footer, chunks[2]);
}

fn render_body(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    resource: Option<&crate::azure::resources::Resource>,
    theme: &Theme,
) {
    let muted = |s: &str| {
        Paragraph::new(Line::from(Span::styled(
            s.to_string(),
            Style::default().fg(theme.muted),
        )))
    };

    let Some(resource) = resource else {
        frame.render_widget(muted("no resource selected."), area);
        return;
    };
    let id = resource.id.as_str();
    let kind = resource.kind;

    let Some(vars) = env_vars_for(state, id, kind) else {
        // Not loaded. Function App settings can be permission-gated; Container
        // App env vars ride on the overview fetch and only ever appear loading.
        let msg =
            if kind == ResourceKind::FunctionApp && state.func_settings.failures.contains_key(id) {
                "env vars unavailable — the signed-in identity needs config/list permission."
            } else {
                "loading environment variables…"
            };
        frame.render_widget(muted(msg), area);
        return;
    };
    if vars.is_empty() {
        frame.render_widget(muted("no environment variables."), area);
        return;
    }

    let cursor = state.env_vars_view.cursor.min(vars.len() - 1);
    let revealed = state.env_vars_view.revealed;
    let visible = area.height as usize;
    let scroll = scroll_for(cursor, vars.len(), visible);

    // The attribution (`in`) column only exists for multi-container Container
    // Apps; when no row carries it (Function Apps, single-container apps) the
    // column is omitted so the page reads exactly as before.
    let show_attr = vars.iter().any(|v| v.attribution.is_some());

    let lines: Vec<Line> = vars
        .iter()
        .enumerate()
        .skip(scroll)
        .take(visible)
        .map(|(i, v)| {
            let selected = i == cursor;
            let name = format!(
                "{:<width$}",
                truncate_right(&v.name, NAME_COL),
                width = NAME_COL
            );
            let value = if revealed {
                let raw = if v.value.is_empty() {
                    "(empty)"
                } else {
                    &v.value
                };
                scroll_value(raw, state.env_vars_view.h_offset)
            } else {
                MASK.to_string()
            };
            // Secret-backed entries (Container App secretRef / Key Vault refs)
            // stand out so it's clear the literal isn't a typed-in value.
            let value_color = if v.is_secret {
                theme.degraded
            } else {
                theme.accent
            };
            let mut spans = vec![
                Span::raw(if selected { "▍ " } else { "  " }),
                Span::styled(name, Style::default().fg(theme.fg)),
            ];
            if show_attr {
                // Pinned LEFT of the value so it's always visible regardless of
                // horizontal value-scroll. `⚠` flags a name whose value differs
                // across containers (the row was exploded per distinct value).
                let attr = format!(
                    "{:<width$}",
                    truncate_right(v.attribution.as_deref().unwrap_or(""), ATTR_COL),
                    width = ATTR_COL
                );
                spans.push(Span::raw(" "));
                spans.push(Span::styled(
                    if v.diverges { "⚠ " } else { "  " },
                    Style::default().fg(theme.degraded),
                ));
                spans.push(Span::styled(attr, Style::default().fg(theme.muted)));
            }
            spans.push(Span::styled(" = ", Style::default().fg(theme.muted)));
            spans.push(Span::styled(value, Style::default().fg(value_color)));
            if selected {
                Line::from(spans).style(theme.selection())
            } else {
                Line::from(spans)
            }
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), area);
}

pub fn handle(action: Action, state: &mut AppState) -> bool {
    let len = selected_env_len(state);
    match action {
        Action::MoveDown => {
            if len > 0 {
                state.env_vars_view.cursor = (state.env_vars_view.cursor + 1).min(len - 1);
            }
            true
        }
        Action::MoveUp => {
            state.env_vars_view.cursor = state.env_vars_view.cursor.saturating_sub(1);
            true
        }
        Action::HalfPageDown => {
            if len > 0 {
                state.env_vars_view.cursor = (state.env_vars_view.cursor + HALF_PAGE).min(len - 1);
            }
            true
        }
        Action::HalfPageUp => {
            state.env_vars_view.cursor = state.env_vars_view.cursor.saturating_sub(HALF_PAGE);
            true
        }
        Action::GotoTop => {
            state.env_vars_view.cursor = 0;
            true
        }
        Action::GotoBottom => {
            if len > 0 {
                state.env_vars_view.cursor = len - 1;
            }
            true
        }
        Action::MoveRight => {
            // Scroll long revealed values into view. Clamp so we never scroll
            // the longest value entirely off the left edge.
            let max = max_value_len(state).saturating_sub(1);
            state.env_vars_view.h_offset = (state.env_vars_view.h_offset + H_SCROLL_STEP).min(max);
            true
        }
        Action::MoveLeft => {
            state.env_vars_view.h_offset =
                state.env_vars_view.h_offset.saturating_sub(H_SCROLL_STEP);
            true
        }
        Action::DecodeSecret => {
            state.env_vars_view.revealed = !state.env_vars_view.revealed;
            // Reset horizontal scroll so re-revealing always starts at column 0.
            state.env_vars_view.h_offset = 0;
            true
        }
        // Back / Yank fall through to the global handler.
        _ => false,
    }
}

/// `NAME=value` of the currently-selected entry, for the global yank handler.
/// Uses the real value (yank is an explicit copy regardless of mask state).
pub fn yank_text(state: &AppState) -> Option<String> {
    let resource = state.selected_resource()?;
    let vars = env_vars_for(state, &resource.id, resource.kind)?;
    let v = vars.get(state.env_vars_view.cursor.min(vars.len().saturating_sub(1)))?;
    Some(format!("{}={}", v.name, v.value))
}

/// Apply the horizontal scroll offset to a value: drop the first `offset`
/// characters and prepend a `…` so it's clear text is scrolled off to the left.
/// The terminal clips the right edge, so this brings the tail into view.
fn scroll_value(s: &str, offset: usize) -> String {
    if offset == 0 {
        return s.to_string();
    }
    let tail: String = s.chars().skip(offset).collect();
    format!("…{tail}")
}

/// Longest value (in characters) among the selected resource's env vars — used
/// to clamp the horizontal scroll offset.
fn max_value_len(state: &AppState) -> usize {
    state
        .selected_resource()
        .and_then(|r| env_vars_for(state, &r.id, r.kind))
        .map(|vars| {
            vars.iter()
                .map(|v| v.value.chars().count())
                .max()
                .unwrap_or(0)
        })
        .unwrap_or(0)
}

fn selected_env_len(state: &AppState) -> usize {
    state
        .selected_resource()
        .and_then(|r| env_vars_for(state, &r.id, r.kind))
        .map(|v| v.len())
        .unwrap_or(0)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::env_vars::EnvVar;
    use crate::azure::resources::{Resource, ResourceKind};
    use crate::config::Config;
    use crate::ui::state::View;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn ev(name: &str, value: &str, is_secret: bool) -> EnvVar {
        EnvVar {
            name: name.into(),
            value: value.into(),
            is_secret,
            ..Default::default()
        }
    }

    fn state_with(kind: ResourceKind, vars: Vec<EnvVar>) -> AppState {
        let mut s = AppState::new(Config::default());
        s.resources = vec![Resource {
            id: "/r/one".into(),
            name: "my-app".into(),
            kind,
            location: "westeurope".into(),
            resource_group: "rg".into(),
            subscription_id: "sub".into(),
            state: Some("Running".into()),
            created_at: None,
            modified_at: None,
            meta: Default::default(),
        }];
        s.list_cursor = 0;
        s.view = View::EnvVars;
        match kind {
            ResourceKind::FunctionApp => {
                s.func_settings.by_resource.insert("/r/one".into(), vars);
            }
            ResourceKind::ContainerApp => {
                s.container_app_overview.by_resource.insert(
                    "/r/one".into(),
                    crate::azure::container_app_overview::ContainerAppOverview {
                        env_vars: vars,
                        ..Default::default()
                    },
                );
            }
            _ => {}
        }
        s
    }

    #[test]
    fn masks_values_until_revealed() {
        let theme = Theme::catppuccin_mocha();
        let mut state = state_with(
            ResourceKind::FunctionApp,
            vec![ev("API_KEY", "supersecret", false)],
        );
        let backend = TestBackend::new(80, 12);
        let mut term = Terminal::new(backend).unwrap();

        // Masked: the name shows, the value does not.
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("API_KEY"));
        assert!(!s.contains("supersecret"), "value leaked while masked");

        // Reveal with `x`.
        assert!(handle(Action::DecodeSecret, &mut state));
        assert!(state.env_vars_view.revealed);
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("supersecret"), "value not shown when revealed");
    }

    #[test]
    fn navigation_clamped_to_list() {
        let mut state = state_with(
            ResourceKind::ContainerApp,
            vec![ev("A", "1", false), ev("B", "2", false)],
        );
        assert!(handle(Action::MoveDown, &mut state));
        assert_eq!(state.env_vars_view.cursor, 1);
        assert!(handle(Action::MoveDown, &mut state));
        assert_eq!(state.env_vars_view.cursor, 1); // clamped
        assert!(handle(Action::GotoTop, &mut state));
        assert_eq!(state.env_vars_view.cursor, 0);
    }

    #[test]
    fn horizontal_scroll_reveals_value_tail_and_resets_on_toggle() {
        let long = format!("{}TAIL", "a".repeat(40)); // 44 chars
        let mut state = state_with(ResourceKind::FunctionApp, vec![ev("CONN", &long, false)]);
        state.env_vars_view.revealed = true;

        // One `l` scrolls by H_SCROLL_STEP characters.
        assert!(handle(Action::MoveRight, &mut state));
        assert_eq!(state.env_vars_view.h_offset, H_SCROLL_STEP);

        // scroll_value drops leading chars and marks the cut with `…`, bringing
        // the tail into view.
        let shown = scroll_value(&long, 40);
        assert!(shown.starts_with('…'));
        assert!(shown.ends_with("TAIL"), "got {shown:?}");

        // Offset clamps so the longest value never scrolls fully off-screen.
        for _ in 0..50 {
            handle(Action::MoveRight, &mut state);
        }
        assert!(state.env_vars_view.h_offset < long.chars().count());

        // `h` retreats; toggling reveal resets to column 0.
        assert!(handle(Action::MoveLeft, &mut state));
        assert!(handle(Action::DecodeSecret, &mut state));
        assert_eq!(state.env_vars_view.h_offset, 0);
    }

    #[test]
    fn yank_text_uses_real_value_even_when_masked() {
        let mut state = state_with(
            ResourceKind::FunctionApp,
            vec![ev("A", "1", false), ev("B", "2", false)],
        );
        state.env_vars_view.cursor = 1;
        assert_eq!(yank_text(&state).as_deref(), Some("B=2"));
    }

    #[test]
    fn renders_container_attribution_column_when_present() {
        let theme = Theme::catppuccin_mocha();
        let vars = vec![
            EnvVar {
                name: "SHARED".into(),
                value: "yes".into(),
                is_secret: false,
                attribution: Some("all (2)".into()),
                diverges: false,
            },
            EnvVar {
                name: "LOG_LEVEL".into(),
                value: "info".into(),
                is_secret: false,
                attribution: Some("files".into()),
                diverges: true,
            },
        ];
        let mut state = state_with(ResourceKind::ContainerApp, vars);
        state.env_vars_view.revealed = true;
        let backend = TestBackend::new(80, 8);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        // Attribution column is rendered between name and value.
        assert!(s.contains("all (2)"), "missing 'all (N)' attribution");
        assert!(s.contains("files"), "missing single-container attribution");
        // The divergent row carries the warning marker.
        assert!(s.contains('⚠'), "missing divergence marker");
    }

    #[test]
    fn no_attribution_column_for_flat_env_vars() {
        // Function App vars carry no attribution, so the column is suppressed and
        // the page reads exactly as before (no stray 'all (' / '⚠').
        let theme = Theme::catppuccin_mocha();
        let mut state = state_with(
            ResourceKind::FunctionApp,
            vec![ev("API_KEY", "v", false), ev("PORT", "8080", false)],
        );
        state.env_vars_view.revealed = true;
        let backend = TestBackend::new(80, 8);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("API_KEY"));
        assert!(!s.contains('⚠'), "divergence marker leaked into flat list");
        assert!(!s.contains("all ("), "attribution leaked into flat list");
    }

    #[test]
    fn empty_list_renders_without_panic() {
        let theme = Theme::catppuccin_mocha();
        let state = state_with(ResourceKind::FunctionApp, vec![]);
        let backend = TestBackend::new(60, 8);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.contains("no environment variables"));
    }
}
