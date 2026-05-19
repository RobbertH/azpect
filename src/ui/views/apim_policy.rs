//! APIM operation policy panel. Reached from [`super::apim_operations`] via
//! Enter. Renders the operation's policy XML in a scrollable paragraph; vim
//! keys move the cursor, `g`/`G` jump to top/bottom.

#![allow(dead_code, unused_variables)]

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::ui::events::Action;
use crate::ui::state::AppState;
use crate::ui::theme::Theme;

const FOOTER_HINT: &str = "j/k scroll  g/G top/bottom  Esc back  r refresh  y yank  ? help  q quit";
const HALF_PAGE: u16 = 10;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(area);

    let op_id = state.apim.selected_operation_id.as_deref();
    let header_label = op_id
        .and_then(short_operation_label)
        .unwrap_or_else(|| "(no operation)".to_string());

    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            " policy ",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            header_label,
            Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
        ),
    ]));
    frame.render_widget(header, chunks[0]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(" xml ", Style::default().fg(theme.fg)));
    let inner = block.inner(chunks[1]);
    frame.render_widget(block, chunks[1]);

    let Some(op_id) = op_id else {
        let p = Paragraph::new(Line::from(Span::styled(
            "no operation selected.",
            Style::default().fg(theme.muted),
        )));
        frame.render_widget(p, inner);
        render_footer(frame, chunks[2], theme);
        return;
    };

    if let Some(err) = state.apim.policy_error.get(op_id) {
        let p = Paragraph::new(Line::from(Span::styled(
            format!("error: {err}"),
            Style::default().fg(theme.critical),
        )));
        frame.render_widget(p, inner);
        render_footer(frame, chunks[2], theme);
        return;
    }

    let loading = state.apim.policy_pending.contains(op_id);
    match state.apim.policy.get(op_id) {
        None if loading => {
            let p = Paragraph::new(Line::from(Span::styled(
                "loading policy …",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, inner);
        }
        None => {
            let p = Paragraph::new(Line::from(Span::styled(
                "press r to load policy.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, inner);
        }
        Some(None) => {
            let p = Paragraph::new(Line::from(Span::styled(
                "no policy configured for this operation.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, inner);
        }
        Some(Some(xml)) => {
            // Render as a scrollable block of lines. Paragraph's built-in
            // scroll is by visual line and counts wrapped lines independently,
            // which matches the user's mental model when toggling wrap.
            let lines: Vec<Line> = xml
                .lines()
                .map(|l| Line::from(Span::styled(l.to_string(), Style::default().fg(theme.fg))))
                .collect();
            let p = Paragraph::new(lines)
                .scroll((state.apim.policy_scroll, 0))
                .wrap(Wrap { trim: false });
            frame.render_widget(p, inner);
        }
    }

    render_footer(frame, chunks[2], theme);
}

fn render_footer(frame: &mut Frame, area: Rect, theme: &Theme) {
    let p = Paragraph::new(Line::from(Span::styled(
        FOOTER_HINT,
        Style::default().fg(theme.muted),
    )));
    frame.render_widget(p, area);
}

/// Pull the last two segments off a `…/apis/{api}/operations/{op}` id so the
/// header reads `{api}/{op}` instead of the full ARM path.
fn short_operation_label(op_id: &str) -> Option<String> {
    let trimmed = op_id.trim_end_matches('/');
    let parts: Vec<&str> = trimmed.split('/').collect();
    let mut rev = parts.iter().rev();
    let op = rev.next()?;
    // skip "operations"
    rev.next()?;
    let api = rev.next()?;
    Some(format!("{api}/{op}"))
}

pub fn handle(action: Action, state: &mut AppState) -> bool {
    let total_lines = state
        .apim
        .selected_operation_id
        .as_deref()
        .and_then(|id| state.apim.policy.get(id))
        .and_then(|maybe| maybe.as_ref())
        .map(|s| s.lines().count() as u16)
        .unwrap_or(0);

    match action {
        Action::MoveDown => {
            state.apim.policy_scroll = state
                .apim
                .policy_scroll
                .saturating_add(1)
                .min(total_lines.saturating_sub(1));
            true
        }
        Action::MoveUp => {
            state.apim.policy_scroll = state.apim.policy_scroll.saturating_sub(1);
            true
        }
        Action::HalfPageDown => {
            state.apim.policy_scroll = state
                .apim
                .policy_scroll
                .saturating_add(HALF_PAGE)
                .min(total_lines.saturating_sub(1));
            true
        }
        Action::HalfPageUp => {
            state.apim.policy_scroll = state.apim.policy_scroll.saturating_sub(HALF_PAGE);
            true
        }
        Action::GotoTop => {
            state.apim.policy_scroll = 0;
            true
        }
        Action::GotoBottom => {
            state.apim.policy_scroll = total_lines.saturating_sub(1);
            true
        }
        _ => false,
    }
}

/// Selected operation's raw policy XML, for yank purposes. `None` when no
/// policy is loaded or APIM reported the operation has no policy configured.
pub fn yank_text(state: &AppState) -> Option<String> {
    let id = state.apim.selected_operation_id.as_deref()?;
    state.apim.policy.get(id)?.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::ui::state::View;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn fixture() -> AppState {
        let mut state = AppState::new(Config::default());
        state.view = View::ApimPolicy;
        state.apim.selected_operation_id =
            Some("/svc/myapim/apis/echo/operations/get-resource".into());
        state
    }

    #[test]
    fn short_label_extracts_api_and_op() {
        assert_eq!(
            short_operation_label("/svc/myapim/apis/echo/operations/get-resource"),
            Some("echo/get-resource".to_string())
        );
        assert!(short_operation_label("not-an-arm-id").is_none());
    }

    #[test]
    fn renders_policy_text() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(80, 12);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.apim.policy.insert(
            state.apim.selected_operation_id.clone().unwrap(),
            Some("<policies><inbound>hello</inbound></policies>".to_string()),
        );
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("policies"));
    }

    #[test]
    fn renders_no_policy_placeholder() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(80, 12);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state
            .apim
            .policy
            .insert(state.apim.selected_operation_id.clone().unwrap(), None);
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("no policy configured"));
    }

    #[test]
    fn scrolls_clamped_to_line_count() {
        let mut state = fixture();
        state.apim.policy.insert(
            state.apim.selected_operation_id.clone().unwrap(),
            Some("a\nb\nc".to_string()),
        );
        assert!(handle(Action::GotoBottom, &mut state));
        assert_eq!(state.apim.policy_scroll, 2);
        assert!(handle(Action::MoveDown, &mut state));
        assert_eq!(state.apim.policy_scroll, 2, "clamped");
        assert!(handle(Action::GotoTop, &mut state));
        assert_eq!(state.apim.policy_scroll, 0);
    }
}
