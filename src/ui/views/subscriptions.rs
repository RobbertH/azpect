//! Subscription picker. Shown on first launch, and again when the user presses `s`.

#![allow(dead_code, unused_variables)]

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::ui::events::Action;
use crate::ui::state::{AppState, View};
use crate::ui::theme::Theme;

const FOOTER_HINT: &str = "j/k move  Enter open  r refresh  ? help  q quit";

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(area);

    // Header
    let header = Paragraph::new(Line::from(vec![
        Span::styled(" subscriptions ", Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
        Span::styled(
            format!("· {}", state.subscriptions.len()),
            Style::default().fg(theme.muted),
        ),
    ]));
    frame.render_widget(header, chunks[0]);

    // Body: list inside a bordered block.
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(
            " select subscription ",
            Style::default().fg(theme.fg),
        ));
    let inner = block.inner(chunks[1]);
    frame.render_widget(block, chunks[1]);

    if state.loading_subscriptions && state.subscriptions.is_empty() {
        let p = Paragraph::new(Line::from(Span::styled(
            "loading subscriptions …",
            Style::default().fg(theme.muted),
        )));
        frame.render_widget(p, inner);
    } else if state.subscriptions.is_empty() {
        let p = Paragraph::new(Line::from(Span::styled(
            "no subscriptions visible to this credential.",
            Style::default().fg(theme.muted),
        )));
        frame.render_widget(p, inner);
    } else {
        let cursor = state
            .subscription_cursor
            .min(state.subscriptions.len().saturating_sub(1));

        let max_name = state
            .subscriptions
            .iter()
            .map(|s| s.display_name.chars().count())
            .max()
            .unwrap_or(0)
            .min(40);

        let lines: Vec<Line> = state
            .subscriptions
            .iter()
            .enumerate()
            .map(|(i, sub)| {
                let selected = i == cursor;
                let name = truncate_right(&sub.display_name, max_name);
                let pad_name = format!("{:<width$}", name, width = max_name);
                let prefix: String = sub.id.chars().take(8).collect();
                let state_color = match sub.state.as_str() {
                    "Enabled" => theme.healthy,
                    "Disabled" | "Deleted" | "Warned" => theme.degraded,
                    _ => theme.muted,
                };

                let mut spans = vec![
                    Span::raw(if selected { " ▍ " } else { "   " }),
                    Span::styled(pad_name, Style::default().fg(theme.fg)),
                    Span::raw("  "),
                    Span::styled(format!("({})", sub.state), Style::default().fg(state_color)),
                    Span::raw("  "),
                    Span::styled(format!("[{prefix}…]"), Style::default().fg(theme.muted)),
                ];

                if Some(&sub.id) == state.config.last_subscription_id.as_ref() {
                    spans.push(Span::raw("  "));
                    spans.push(Span::styled("· last", Style::default().fg(theme.accent)));
                }

                if selected {
                    Line::from(spans).style(theme.selection())
                } else {
                    Line::from(spans)
                }
            })
            .collect();

        let p = Paragraph::new(lines);
        frame.render_widget(p, inner);
    }

    let footer = Paragraph::new(Line::from(Span::styled(
        FOOTER_HINT,
        Style::default().fg(theme.muted),
    )));
    frame.render_widget(footer, chunks[2]);
}

/// View-local input handler. Returns `true` if the action was consumed.
pub fn handle(action: Action, state: &mut AppState) -> bool {
    let len = state.subscriptions.len();
    match action {
        Action::MoveDown => {
            if len > 0 {
                state.subscription_cursor = (state.subscription_cursor + 1).min(len - 1);
            }
            true
        }
        Action::MoveUp => {
            state.subscription_cursor = state.subscription_cursor.saturating_sub(1);
            true
        }
        Action::HalfPageDown => {
            if len > 0 {
                state.subscription_cursor = (state.subscription_cursor + 10).min(len - 1);
            }
            true
        }
        Action::HalfPageUp => {
            state.subscription_cursor = state.subscription_cursor.saturating_sub(10);
            true
        }
        Action::GotoTop => {
            state.subscription_cursor = 0;
            true
        }
        Action::GotoBottom => {
            if len > 0 {
                state.subscription_cursor = len - 1;
            }
            true
        }
        Action::OpenSelected => {
            if let Some(sub) = state.subscriptions.get(state.subscription_cursor) {
                state.selected_subscription = Some(sub.id.clone());
                state.config.last_subscription_id = Some(sub.id.clone());
                state.view_stack.push(state.view);
                state.view = View::List;
                state.list_cursor = 0;
                state.resources.clear();
            }
            true
        }
        _ => false,
    }
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
    use crate::azure::subscriptions::Subscription;
    use crate::config::Config;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn fixture() -> AppState {
        let mut state = AppState::new(Config::default());
        state.loading_subscriptions = false;
        state.subscriptions = vec![
            Subscription {
                id: "11111111-1111-1111-1111-111111111111".into(),
                display_name: "alpha".into(),
                state: "Enabled".into(),
                tenant_id: "t1".into(),
            },
            Subscription {
                id: "22222222-2222-2222-2222-222222222222".into(),
                display_name: "beta".into(),
                state: "Disabled".into(),
                tenant_id: "t1".into(),
            },
        ];
        state
    }

    #[test]
    fn renders_without_panic() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(80, 12);
        let mut term = Terminal::new(backend).unwrap();
        let state = fixture();
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf_str = format!("{:?}", term.backend().buffer());
        assert!(buf_str.contains("alpha"));
        assert!(buf_str.contains("beta"));
    }

    #[test]
    fn handles_navigation() {
        let mut state = fixture();
        assert!(handle(Action::MoveDown, &mut state));
        assert_eq!(state.subscription_cursor, 1);
        assert!(handle(Action::MoveDown, &mut state));
        assert_eq!(state.subscription_cursor, 1, "clamped to last");
        assert!(handle(Action::MoveUp, &mut state));
        assert_eq!(state.subscription_cursor, 0);
        assert!(handle(Action::GotoBottom, &mut state));
        assert_eq!(state.subscription_cursor, 1);
    }

    #[test]
    fn open_selected_transitions_view() {
        let mut state = fixture();
        state.subscription_cursor = 1;
        assert!(handle(Action::OpenSelected, &mut state));
        assert_eq!(state.view, View::List);
        assert_eq!(
            state.selected_subscription.as_deref(),
            Some("22222222-2222-2222-2222-222222222222")
        );
        assert_eq!(
            state.config.last_subscription_id.as_deref(),
            Some("22222222-2222-2222-2222-222222222222"),
            "picking a sub should update the persisted last_subscription_id",
        );
        assert!(state.resources.is_empty());
    }

    #[test]
    fn unrelated_action_not_consumed() {
        let mut state = fixture();
        assert!(!handle(Action::Help, &mut state));
    }
}
