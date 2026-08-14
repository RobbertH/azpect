//! Message content viewer for one Logic App run action / trigger firing: the
//! downloaded `inputs` and `outputs` payloads, pretty-printed (the backend
//! already formats JSON), in one scrollable pane. Same slicing scroll engine
//! as `cosmos_item.rs` — render cost stays O(viewport).

#![allow(dead_code, unused_variables)]

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::azure::logic_apps::ActionContent;
use crate::ui::events::Action;
use crate::ui::state::AppState;
use crate::ui::theme::Theme;

const FOOTER_HINT: &str = "j/k scroll  g/G top/bottom  Esc back  r refresh  y yank  ? help  q quit";
const HALF_PAGE: u16 = 10;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    // Reset first so `j`/`G` are no-ops on the placeholder/error branches; the
    // content branch below overwrites it with the real ceiling.
    state.scroll_max.set(0);
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);

    let Some(source) = state.logic_apps.selected_content.as_ref() else {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.border))
            .title(" content ");
        let inner = block.inner(chunks[0]);
        frame.render_widget(block, chunks[0]);
        let p = Paragraph::new(Line::from(Span::styled(
            "no action selected.",
            Style::default().fg(theme.muted),
        )));
        frame.render_widget(p, inner);
        render_footer(frame, chunks[1], theme);
        return;
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Line::from(Span::styled(
            format!(" content · {} ", source.title),
            Style::default().fg(theme.fg),
        )));
    let inner = block.inner(chunks[0]);
    frame.render_widget(block, chunks[0]);

    if let Some(err) = state.logic_apps.content_error.get(&source.key) {
        let p = Paragraph::new(ratatui::text::Text::styled(
            format!("error: {err}"),
            Style::default().fg(theme.critical),
        ))
        .wrap(Wrap { trim: false });
        frame.render_widget(p, inner);
        render_footer(frame, chunks[1], theme);
        return;
    }

    let loading = state.logic_apps.content_pending.contains(&source.key);
    match state.logic_apps.content.get(&source.key) {
        None if loading => {
            let p = Paragraph::new(Line::from(Span::styled(
                "downloading content …",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, inner);
        }
        None => {
            let p = Paragraph::new(Line::from(Span::styled(
                "press r to download content.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, inner);
        }
        Some(content) => render_content(frame, inner, content, state, theme),
    }

    render_footer(frame, chunks[1], theme);
}

fn render_content(
    frame: &mut Frame,
    area: Rect,
    content: &ActionContent,
    state: &AppState,
    theme: &Theme,
) {
    let text = format_content_text(content);
    let all: Vec<&str> = text.lines().collect();
    let view_h = area.height.max(1) as usize;
    // Slice to the visible window (no `.scroll()`): handing ratatui a large
    // offset makes `Paragraph` wrap every line above the window each frame.
    let max_scroll = all.len().saturating_sub(view_h);
    state
        .scroll_max
        .set(max_scroll.min(u16::MAX as usize) as u16);
    let start = (state.logic_apps.content_scroll as usize).min(max_scroll);
    let end = (start + view_h).min(all.len());
    let lines: Vec<Line> = all[start..end]
        .iter()
        .map(|l| {
            // Section separators get the accent color so inputs/outputs are
            // scannable while scrolling through a long payload.
            let style = if l.starts_with("── ") {
                Style::default().fg(theme.accent)
            } else {
                Style::default().fg(theme.fg)
            };
            Line::from(Span::styled(l.to_string(), style))
        })
        .collect();
    let p = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(p, area);
}

/// The pane text: an `inputs` section and an `outputs` section, each either
/// the (pretty-printed) payload or a placeholder for a side the source row
/// never had.
fn format_content_text(content: &ActionContent) -> String {
    let mut out = String::new();
    out.push_str("── inputs ──\n");
    out.push_str(content.inputs.as_deref().unwrap_or("(none)"));
    out.push_str("\n\n── outputs ──\n");
    out.push_str(content.outputs.as_deref().unwrap_or("(none)"));
    out
}

fn render_footer(frame: &mut Frame, area: Rect, theme: &Theme) {
    let p = Paragraph::new(Line::from(Span::styled(
        FOOTER_HINT,
        Style::default().fg(theme.muted),
    )));
    frame.render_widget(p, area);
}

/// Full pane text for `y`, prefixed with the source title.
pub fn yank_text(state: &AppState) -> Option<String> {
    let source = state.logic_apps.selected_content.as_ref()?;
    let content = state.logic_apps.content.get(&source.key)?;
    Some(format!(
        "{}\n{}",
        source.title,
        format_content_text(content)
    ))
}

pub fn handle(action: Action, state: &mut AppState) -> bool {
    let max_scroll = state.scroll_max.get();
    match action {
        Action::MoveDown => {
            state.logic_apps.content_scroll = state
                .logic_apps
                .content_scroll
                .saturating_add(1)
                .min(max_scroll);
            true
        }
        Action::MoveUp => {
            state.logic_apps.content_scroll = state
                .logic_apps
                .content_scroll
                .min(max_scroll)
                .saturating_sub(1);
            true
        }
        Action::HalfPageDown => {
            state.logic_apps.content_scroll = state
                .logic_apps
                .content_scroll
                .saturating_add(HALF_PAGE)
                .min(max_scroll);
            true
        }
        Action::HalfPageUp => {
            state.logic_apps.content_scroll = state
                .logic_apps
                .content_scroll
                .min(max_scroll)
                .saturating_sub(HALF_PAGE);
            true
        }
        Action::GotoTop => {
            state.logic_apps.content_scroll = 0;
            true
        }
        Action::GotoBottom => {
            state.logic_apps.content_scroll = max_scroll;
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::ui::state::{LogicContentSource, View};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn fixture() -> AppState {
        let mut state = AppState::new(Config::default());
        state.view = View::LogicAppContent;
        state.logic_apps.selected_content = Some(LogicContentSource {
            key: "wf/runs/r1/actions/Parse_JSON".into(),
            title: "Parse_JSON".into(),
            inputs: None,
            outputs: None,
            origin: View::LogicAppRunDetail,
        });
        state
    }

    #[test]
    fn renders_loading_state() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(120, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state
            .logic_apps
            .content_pending
            .insert("wf/runs/r1/actions/Parse_JSON".into());
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("downloading content"));
    }

    #[test]
    fn renders_sections_and_placeholders() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(120, 14);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.logic_apps.content.insert(
            "wf/runs/r1/actions/Parse_JSON".into(),
            ActionContent {
                inputs: Some("{\n  \"orderId\": 42\n}".into()),
                outputs: None,
            },
        );
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("inputs"));
        assert!(buf.contains("orderId"));
        assert!(buf.contains("outputs"));
        assert!(buf.contains("(none)"));
    }

    #[test]
    fn goto_bottom_then_move_up_responds_immediately() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(60, 8);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        let long = (0..100)
            .map(|i| format!("\"line{i}\": {i}"))
            .collect::<Vec<_>>()
            .join(",\n");
        state.logic_apps.content.insert(
            "wf/runs/r1/actions/Parse_JSON".into(),
            ActionContent {
                inputs: Some(long),
                outputs: None,
            },
        );
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        assert!(handle(Action::GotoBottom, &mut state));
        let max = state.scroll_max.get();
        assert!(max > 0, "long content must publish a scroll ceiling");
        assert_eq!(state.logic_apps.content_scroll, max);
        assert!(handle(Action::MoveUp, &mut state));
        assert_eq!(state.logic_apps.content_scroll, max - 1);
    }

    #[test]
    fn yank_includes_title_and_sections() {
        let mut state = fixture();
        state.logic_apps.content.insert(
            "wf/runs/r1/actions/Parse_JSON".into(),
            ActionContent {
                inputs: Some("{}".into()),
                outputs: Some("{\"ok\":true}".into()),
            },
        );
        let text = yank_text(&state).expect("yank text");
        assert!(text.starts_with("Parse_JSON\n"));
        assert!(text.contains("── inputs ──"));
        assert!(text.contains("{\"ok\":true}"));
    }
}
