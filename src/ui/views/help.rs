//! Help overlay. Toggled by `?`. Shows the keymap in a centered popup.

#![allow(dead_code, unused_variables)]

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::ui::events::Action;
use crate::ui::state::{AppState, View};
use crate::ui::theme::Theme;

const SECTIONS: &[(&str, &[(&str, &str)])] = &[
    (
        "Navigation",
        &[
            ("j / k", "down / up"),
            ("h / l", "left / right"),
            ("g g", "go to top"),
            ("G", "go to bottom"),
            ("Ctrl-d / Ctrl-u", "half page down / up"),
            ("Esc", "back"),
        ],
    ),
    (
        "Resources",
        &[
            ("Enter", "open detail"),
            ("L", "open logs"),
            ("f", "toggle favorite"),
            ("F", "favorites only"),
            ("/", "search"),
            ("s", "switch subscription"),
        ],
    ),
    (
        "Detail / Logs",
        &[
            ("d", "window 1d"),
            ("w", "window 7d"),
            ("e", "errors only (logs)"),
            ("L", "open logs (detail)"),
        ],
    ),
    (
        "Global",
        &[
            ("r", "refresh"),
            ("y", "yank to clipboard"),
            ("?", "toggle help"),
            ("q", "quit"),
        ],
    ),
];

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let popup = centered_rect(74, 80, area);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(
            " help ",
            Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(theme.bg).fg(theme.fg));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    // Two-column layout: left = first two sections, right = last two.
    let cols = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(inner);

    let left_lines = lines_for(&[SECTIONS[0], SECTIONS[1]], theme);
    let right_lines = lines_for(&[SECTIONS[2], SECTIONS[3]], theme);

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

fn lines_for(sections: &[(&str, &[(&str, &str)])], theme: &Theme) -> Vec<Line<'static>> {
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

    #[test]
    fn renders_without_panic() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(100, 30);
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
