//! Cosmos DB item preview panel. Reached from [`super::cosmos_containers`] via
//! Enter. Runs `SELECT * FROM c` (first 20 rows) against the pinned container and
//! renders the rows as pretty-printed JSON in a scrollable `Paragraph`. The
//! request charge (`x-ms-request-charge`) is surfaced in the title bar so the
//! user can see the exploratory read cost at a glance.

#![allow(dead_code, unused_variables)]

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::azure::cosmos::CosmosItemPreview;
use crate::ui::events::Action;
use crate::ui::state::{AppState, CosmosCache};
use crate::ui::theme::Theme;

const FOOTER_HINT: &str = "j/k scroll  g/G top/bottom  Esc back  r refresh  y yank  ? help  q quit";
const HALF_PAGE: u16 = 10;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    // Assume no scrollable content until `render_items` measures some — keeps
    // j/G no-ops (via the handler's `scroll_max` clamp) on the placeholder /
    // error / empty branches below.
    state.scroll_max.set(0);
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);

    let (Some(acc), Some(db), Some(coll)) = (
        state.cosmos.selected_account.as_ref(),
        state.cosmos.selected_database.as_deref(),
        state.cosmos.selected_container.as_deref(),
    ) else {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.border))
            .title(Span::styled(" items ", Style::default().fg(theme.fg)));
        let inner = block.inner(chunks[0]);
        frame.render_widget(block, chunks[0]);
        let p = Paragraph::new(Line::from(Span::styled(
            "no container selected.",
            Style::default().fg(theme.muted),
        )));
        frame.render_widget(p, inner);
        render_footer(frame, chunks[1], theme);
        return;
    };

    let key = CosmosCache::items_key(&acc.id, db, coll);
    let preview = state.cosmos.items.get(&key);

    // Title surfaces the SELECT and the RU charge so the cost of the
    // exploratory read is visible at a glance.
    let title = if let Some(p) = preview {
        let mut t = format!(" SELECT * · {} rows", p.items.len());
        if let Some(ru) = p.request_charge {
            t.push_str(&format!(" · {ru:.2} RU"));
        }
        if p.partial {
            t.push_str(" · more available");
        }
        t.push(' ');
        t
    } else {
        " items ".to_string()
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(title, Style::default().fg(theme.fg)));
    let inner = block.inner(chunks[0]);
    frame.render_widget(block, chunks[0]);

    if let Some(err) = state.cosmos.items_error.get(&key) {
        let p = Paragraph::new(Line::from(Span::styled(
            format!("error: {err}"),
            Style::default().fg(theme.critical),
        )))
        .wrap(Wrap { trim: false });
        frame.render_widget(p, inner);
        render_footer(frame, chunks[1], theme);
        return;
    }

    let loading = state.cosmos.items_pending.contains(&key);
    match preview {
        None if loading => {
            let p = Paragraph::new(Line::from(Span::styled(
                "loading items …",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, inner);
        }
        None => {
            let p = Paragraph::new(Line::from(Span::styled(
                "press r to load items.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, inner);
        }
        Some(p) if p.items.is_empty() => {
            let para = Paragraph::new(Line::from(Span::styled(
                "container is empty.",
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(para, inner);
        }
        Some(preview) => render_items(frame, inner, preview, state, theme),
    }

    render_footer(frame, chunks[1], theme);
}

fn render_items(
    frame: &mut Frame,
    area: Rect,
    preview: &CosmosItemPreview,
    state: &AppState,
    theme: &Theme,
) {
    let text = format_items_text(&preview.items);
    let all: Vec<&str> = text.lines().collect();
    let view_h = area.height.max(1) as usize;
    // Slice to just the visible window and render with no scroll offset. Handing
    // ratatui a large `.scroll()` offset makes its `Paragraph` word-wrap every
    // line above the window on every frame (cost ∝ offset), which pegged a CPU
    // when scrolling up from `G`. Slicing keeps render cost O(viewport).
    let max_scroll = all.len().saturating_sub(view_h);
    // Publish the real ceiling so the key handler clamps the stored offset —
    // G must land on the last window, not park at a sentinel that leaves `k`
    // apparently dead until the counter walks back into range.
    state
        .scroll_max
        .set(max_scroll.min(u16::MAX as usize) as u16);
    let start = (state.cosmos.items_scroll as usize).min(max_scroll);
    let end = (start + view_h).min(all.len());
    let lines: Vec<Line> = all[start..end]
        .iter()
        .map(|l| Line::from(Span::styled(l.to_string(), Style::default().fg(theme.fg))))
        .collect();
    let p = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(p, area);
}

/// Render the items as pretty-printed JSON separated by `---` lines. Items
/// past the first one get a leading separator so the boundary is obvious when
/// scrolling.
fn format_items_text(items: &[serde_json::Value]) -> String {
    let mut out = String::new();
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push_str("\n---\n");
        }
        let pretty = serde_json::to_string_pretty(item).unwrap_or_else(|_| item.to_string());
        out.push_str(&pretty);
    }
    out
}

fn render_footer(frame: &mut Frame, area: Rect, theme: &Theme) {
    let p = Paragraph::new(Line::from(Span::styled(
        FOOTER_HINT,
        Style::default().fg(theme.muted),
    )));
    frame.render_widget(p, area);
}

pub fn handle(action: Action, state: &mut AppState) -> bool {
    // Every mutation clamps to the render-published `scroll_max` so the stored
    // offset never parks past the content. G used to store a 10_000 sentinel
    // (render clamped it for display only), which left `k` decrementing
    // invisibly for thousands of presses before the window moved.
    let max_scroll = state.scroll_max.get();
    match action {
        Action::MoveDown => {
            state.cosmos.items_scroll = state.cosmos.items_scroll.saturating_add(1).min(max_scroll);
            true
        }
        Action::MoveUp => {
            state.cosmos.items_scroll = state.cosmos.items_scroll.min(max_scroll).saturating_sub(1);
            true
        }
        Action::HalfPageDown => {
            state.cosmos.items_scroll = state
                .cosmos
                .items_scroll
                .saturating_add(HALF_PAGE)
                .min(max_scroll);
            true
        }
        Action::HalfPageUp => {
            state.cosmos.items_scroll = state
                .cosmos
                .items_scroll
                .min(max_scroll)
                .saturating_sub(HALF_PAGE);
            true
        }
        Action::GotoTop => {
            state.cosmos.items_scroll = 0;
            true
        }
        Action::GotoBottom => {
            state.cosmos.items_scroll = max_scroll;
            true
        }
        _ => false,
    }
}

/// Plain-text yank payload for the displayed item preview: the pretty-printed
/// JSON for all rows separated by `---`. Returns `None` if no preview has been
/// loaded yet.
pub fn yank_text(state: &AppState) -> Option<String> {
    let acc = state.cosmos.selected_account.as_ref()?;
    let db = state.cosmos.selected_database.as_deref()?;
    let coll = state.cosmos.selected_container.as_deref()?;
    let key = CosmosCache::items_key(&acc.id, db, coll);
    let preview = state.cosmos.items.get(&key)?;
    Some(format_items_text(&preview.items))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::cosmos::{CosmosAccount, CosmosItemPreview};
    use crate::config::Config;
    use crate::ui::state::View;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use serde_json::json;

    fn account_fixture() -> CosmosAccount {
        CosmosAccount {
            id: "/subs/x/rg/y/da/acc".into(),
            name: "acc".into(),
            resource_group: "rg".into(),
            subscription_id: "sub".into(),
            location: "westeurope".into(),
            kind: Some("GlobalDocumentDB".into()),
            document_endpoint: Some("https://acc.documents.azure.com:443/".into()),
            capabilities: Vec::new(),
            is_serverless: false,
            public_network_access: Some("Enabled".into()),
            created_at: None,
        }
    }

    fn fixture() -> AppState {
        let mut state = AppState::new(Config::default());
        state.view = View::CosmosItem;
        state.cosmos.selected_account = Some(account_fixture());
        state.cosmos.selected_database = Some("orders".into());
        state.cosmos.selected_container = Some("items".into());
        state
    }

    #[test]
    fn renders_loading_state() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(120, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        let key = CosmosCache::items_key("/subs/x/rg/y/da/acc", "orders", "items");
        state.cosmos.items_pending.insert(key);
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("loading items"));
    }

    #[test]
    fn renders_items_and_ru_in_title() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(120, 20);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        let key = CosmosCache::items_key("/subs/x/rg/y/da/acc", "orders", "items");
        state.cosmos.items.insert(
            key,
            CosmosItemPreview {
                items: vec![json!({ "id": "a", "name": "alpha" })],
                request_charge: Some(2.34),
                partial: false,
            },
        );
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("alpha"), "item content should render");
        assert!(buf.contains("2.34 RU"), "RU charge should appear in title");
    }

    #[test]
    fn goto_bottom_then_move_up_responds_immediately() {
        // Regression: G stored a 10_000 sentinel while only the renderer
        // clamped it for display, so `k` decremented invisibly for thousands
        // of presses. After a render, G must land exactly on the last window
        // and the very next `k` must move the view.
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(60, 10);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        let key = CosmosCache::items_key("/subs/x/rg/y/da/acc", "orders", "items");
        let items: Vec<serde_json::Value> = (0..10).map(|i| json!({ "id": i })).collect();
        state.cosmos.items.insert(
            key,
            CosmosItemPreview {
                items,
                request_charge: None,
                partial: false,
            },
        );
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();

        assert!(handle(Action::GotoBottom, &mut state));
        let max = state.scroll_max.get();
        assert!(max > 0, "fixture must overflow the viewport");
        assert_eq!(state.cosmos.items_scroll, max);
        assert!(handle(Action::MoveUp, &mut state));
        assert_eq!(state.cosmos.items_scroll, max - 1);
    }

    #[test]
    fn format_items_text_joins_with_separator() {
        let s = format_items_text(&[json!({ "a": 1 }), json!({ "b": 2 })]);
        assert!(s.contains("\n---\n"), "expected separator in: {s}");
        assert!(s.contains("\"a\""));
        assert!(s.contains("\"b\""));
    }
}
