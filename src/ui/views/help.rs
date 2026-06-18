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
            ("h", "left"),
            ("g g", "go to top"),
            ("G", "go to bottom"),
            ("Ctrl-d / Ctrl-u", "half page down / up"),
            ("Esc", "back"),
        ],
    ),
    (
        "API resources",
        &[
            ("Enter", "open detail"),
            ("l", "open logs"),
            ("f", "toggle favorite"),
            ("F", "favorites only"),
            ("/", "search"),
            ("s", "switch subscription"),
        ],
    ),
    (
        "Detail / Logs",
        &[
            ("Enter", "log line detail (logs)"),
            ("0", "window 1h"),
            ("1", "window 1d"),
            ("7", "window 7d"),
            ("w", "wrap (logs)"),
            ("e", "errors only (logs) / env vars (detail)"),
            ("Tab / S-Tab", "cycle source filter (logs)"),
            ("s", "shell into container (Container App detail/logs)"),
            ("x", "reveal / hide env var values (env vars)"),
            ("Ctrl-e / Ctrl-n", "edit / add env var (env vars)"),
            ("/", "search (logs)"),
            ("n / N", "next / prev match (logs)"),
            ("V", "visual-line select for yank (logs)"),
            ("l", "open logs (detail)"),
        ],
    ),
    (
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
            ("note", "verdict is worst-of all signals (pessimistic)"),
        ],
    ),
    (
        "APIM (APIs/Routes)",
        &[
            ("Enter", "drill down: APIs > routes > policy"),
            ("y", "yank API / operation / policy"),
            ("o", "open in Azure Portal"),
            ("r", "refresh current panel"),
        ],
    ),
    (
        "Application Gateway",
        &[
            ("Enter", "show backend pools and their members"),
            ("y", "yank gateway id / FQDN / IP / NIC id"),
            ("o", "open gateway in Azure Portal"),
            ("r", "refresh backend pools"),
        ],
    ),
    (
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
    ),
    (
        "Container registries (ACR)",
        &[
            ("R", "enter registries mode"),
            ("Enter", "drill: registries > repositories > tags"),
            ("/", "filter registries / repos / tags by name (substring)"),
            ("y", "yank registry id / repo name / pull ref"),
            ("o", "open registry in Azure Portal"),
            ("r", "refresh current panel"),
        ],
    ),
    (
        "Cosmos DB (SQL/Core API)",
        &[
            (":cosmos", "enter cosmos mode (palette only — no keybind)"),
            ("Enter", "drill: accounts > databases > containers > items"),
            ("/", "filter accounts / databases / containers by name"),
            ("y", "yank account id / db name / container / item json"),
            ("o", "open account's Data Explorer in Azure Portal"),
            (
                "r",
                "refresh current panel (item preview costs RU — see title bar)",
            ),
        ],
    ),
    (
        "Key Vaults (listing is metadata only)",
        &[
            (":keyvaults / :kv", "enter key vault mode (palette only)"),
            ("Enter", "vaults: drill in to secrets / certificates"),
            ("Enter / x", "secrets: reveal selected value in a modal"),
            ("Tab / S-Tab", "toggle secrets ↔ certificates"),
            ("/", "filter vaults / items by name (substring)"),
            ("y", "yank vault id / item name · in modal: the value"),
            ("o", "open vault in Azure Portal"),
            ("r", "refresh current panel"),
        ],
    ),
    (
        "Service Bus (control plane)",
        &[
            (":servicebus / :sb", "enter service bus mode (palette only)"),
            ("Enter", "drill: namespaces > queues/topics > subs"),
            ("Tab / S-Tab", "toggle queues ↔ topics"),
            ("DLQ", "dead-letter depth, red when non-zero"),
            ("/", "filter by name (substring)"),
            ("y", "yank id / entity / subscription"),
            ("o", "open namespace in Azure Portal"),
            ("r", "refresh current panel"),
        ],
    ),
    (
        "Global",
        &[
            ("r", "refresh"),
            ("y", "yank to clipboard"),
            ("o", "open in Azure Portal"),
            ("?", "toggle help"),
            ("q", "quit"),
        ],
    ),
    (
        "Command palette (:)",
        &[
            (":", "open command palette"),
            ("Tab / S-Tab", "cycle prefix matches"),
            (":storage", "enter storage mode"),
            (":registries / :reg / :acr", "enter registries mode"),
            (":cosmos", "enter cosmos mode"),
            (":keyvaults / :kv / :vaults", "enter key vault mode"),
            (":servicebus / :sb / :bus", "enter service bus mode"),
            (":apis", "back to apis list"),
            (":subscriptions / :subs", "subscription picker"),
            (":help / :h / :?", "open help"),
            (":refresh", "force-refresh current view"),
            (":quit / :q", "quit"),
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

    let mid = SECTIONS.len().div_ceil(2);
    let left_lines = lines_for(&SECTIONS[..mid], theme);
    let right_lines = lines_for(&SECTIONS[mid..], theme);

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
        // Backend has to be tall enough to fit the help popup's column with
        // the most content. Adding new sections (Cosmos, etc.) pushes the
        // right-column footers further down — bump backend height if you add
        // sections that grow either column past ~30 lines.
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(100, 60);
        let mut term = Terminal::new(backend).unwrap();
        let state = AppState::new(Config::default());
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let s = format!("{:?}", term.backend().buffer());
        assert!(s.to_lowercase().contains("help"));
        assert!(s.contains("Navigation"));
        assert!(s.contains("Global"));
        assert!(s.contains("Cosmos"), "Cosmos section should render");
        assert!(
            s.contains("Service Bus"),
            "Service Bus section should render"
        );
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
