//! Subscription picker. Shown on first launch, and again when the user presses `s`.

#![allow(dead_code, unused_variables)]

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::ui::events::Action;
use crate::ui::state::AppState;
use crate::ui::theme::Theme;

const FOOTER_HINT: &str = "j/k move  Enter filter  y yank id  o portal  r refresh  ? help  q quit";

/// Label for the synthetic top row that clears the subscription filter.
const ALL_LABEL: &str = "All subscriptions";

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(area);

    // Header
    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            " subscriptions ",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
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
        // Row 0 is the synthetic "All subscriptions" scope; the real
        // subscriptions follow at cursor index +1.
        let total = state.subscriptions.len() + 1;
        let cursor = state.subscription_cursor.min(total - 1);

        let max_name = state
            .subscriptions
            .iter()
            .map(|s| s.display_name.chars().count())
            .chain(std::iter::once(ALL_LABEL.len()))
            .max()
            .unwrap_or(0)
            .min(40);

        let mut lines: Vec<Line> = Vec::with_capacity(total);

        // "All subscriptions" row.
        {
            let selected = cursor == 0;
            let pad = format!("{:<width$}", ALL_LABEL, width = max_name);
            let mut spans = vec![
                Span::raw(if selected { " ▍ " } else { "   " }),
                Span::styled(
                    pad,
                    Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
                ),
            ];
            if state.selected_subscription.is_none() {
                spans.push(Span::raw("  "));
                spans.push(Span::styled("· active", Style::default().fg(theme.accent)));
            }
            lines.push(if selected {
                Line::from(spans).style(theme.selection())
            } else {
                Line::from(spans)
            });
        }

        for (i, sub) in state.subscriptions.iter().enumerate() {
            let selected = cursor == i + 1;
            let name = truncate_right(&sub.display_name, max_name);
            let pad_name = format!("{:<width$}", name, width = max_name);
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
                Span::styled(sub.id.as_str(), Style::default().fg(theme.muted)),
            ];

            if state.selected_subscription.as_ref() == Some(&sub.id) {
                spans.push(Span::raw("  "));
                spans.push(Span::styled("· active", Style::default().fg(theme.accent)));
            }

            lines.push(if selected {
                Line::from(spans).style(theme.selection())
            } else {
                Line::from(spans)
            });
        }

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
    // +1 for the synthetic "All subscriptions" row at cursor 0.
    let total = state.subscriptions.len() + 1;
    match action {
        Action::MoveDown => {
            state.subscription_cursor = (state.subscription_cursor + 1).min(total - 1);
            true
        }
        Action::MoveUp => {
            state.subscription_cursor = state.subscription_cursor.saturating_sub(1);
            true
        }
        Action::HalfPageDown => {
            state.subscription_cursor = (state.subscription_cursor + 10).min(total - 1);
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
            state.subscription_cursor = total - 1;
            true
        }
        Action::OpenSelected => {
            // Cursor 0 = "All subscriptions" → clear the filter; any other row
            // pins that subscription. Persist the choice so the next launch
            // honors it (`None` for All).
            let new_selection = if state.subscription_cursor == 0 {
                None
            } else {
                state
                    .subscriptions
                    .get(state.subscription_cursor - 1)
                    .map(|s| s.id.clone())
            };
            state.selected_subscription = new_selection.clone();
            state.config.last_subscription_id = new_selection;
            state.view_stack.push(state.view);
            // Every subscription-scoped cache is stale for the new scope. Loop
            // over `Category::ALL` so adding a new resource type automatically
            // gets its cache flushed here — no risk of a future ACR-style
            // "stale data sticks around" bug.
            for category in crate::ui::state::Category::ALL {
                category.clear_cache(state);
            }
            // Land back on whichever category the user was most recently inside
            // so re-scoping feels like "re-scope what I'm looking at" rather
            // than "throw me back to apis". Defaults to `Category::Apis`.
            state.view = state.last_category.root_view();
            state.list_cursor = 0;
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
    use crate::ui::state::View;
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
        // Rows: [All, alpha, beta] → cursor clamps to 2.
        let mut state = fixture();
        assert!(handle(Action::MoveDown, &mut state));
        assert_eq!(state.subscription_cursor, 1);
        assert!(handle(Action::MoveDown, &mut state));
        assert_eq!(state.subscription_cursor, 2);
        assert!(handle(Action::MoveDown, &mut state));
        assert_eq!(state.subscription_cursor, 2, "clamped to last");
        assert!(handle(Action::MoveUp, &mut state));
        assert_eq!(state.subscription_cursor, 1);
        assert!(handle(Action::GotoBottom, &mut state));
        assert_eq!(state.subscription_cursor, 2);
    }

    #[test]
    fn open_selected_pins_subscription() {
        // Cursor 2 = beta (row 0 is "All", row 1 is alpha).
        let mut state = fixture();
        state.subscription_cursor = 2;
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
    fn open_selected_all_row_clears_the_filter() {
        let mut state = fixture();
        // Pretend a sub was pinned, then pick the "All subscriptions" row.
        state.selected_subscription = Some("22222222-2222-2222-2222-222222222222".into());
        state.config.last_subscription_id = Some("22222222-2222-2222-2222-222222222222".into());
        state.subscription_cursor = 0;
        assert!(handle(Action::OpenSelected, &mut state));
        assert_eq!(state.view, View::List);
        assert!(
            state.selected_subscription.is_none(),
            "All clears the scope"
        );
        assert!(
            state.config.last_subscription_id.is_none(),
            "All persists as no-pin"
        );
    }

    #[test]
    fn open_selected_lands_on_remembered_top_level_view() {
        use crate::ui::state::Category;
        // User was last viewing Registries; switching subscription should
        // route them back to Registries (under the new scope) instead of
        // dumping them into the apis list.
        let mut state = fixture();
        state.last_category = Category::Registries;
        state.subscription_cursor = 1;
        assert!(handle(Action::OpenSelected, &mut state));
        assert_eq!(state.view, View::Registries);

        // Same shape for storage.
        let mut state = fixture();
        state.last_category = Category::Storage;
        assert!(handle(Action::OpenSelected, &mut state));
        assert_eq!(state.view, View::StorageAccounts);
    }

    #[test]
    fn open_selected_flushes_every_subscription_scoped_cache() {
        // Picking a sub must wipe ALL three category caches — leaving stale
        // entries from the previous scope is the bug the user reported for
        // ACR (and the same bug applied to storage/appgw before).
        use crate::azure::registries::Registry;
        use crate::azure::storage::StorageAccount;

        let mut state = fixture();
        state.registry.registries = Some(vec![Registry {
            id: "/subs/old/.../myreg".into(),
            name: "myreg".into(),
            resource_group: "rg".into(),
            subscription_id: "11111111-1111-1111-1111-111111111111".into(),
            location: "westeurope".into(),
            sku: None,
            login_server: None,
            admin_user_enabled: None,
            public_network_access: None,
            anonymous_pull_enabled: None,
            created_at: None,
        }]);
        state.storage.accounts = Some(vec![StorageAccount {
            id: "/subs/old/.../sa".into(),
            name: "sa".into(),
            resource_group: "rg".into(),
            subscription_id: "11111111-1111-1111-1111-111111111111".into(),
            location: "westeurope".into(),
            kind: None,
            sku: None,
            access_tier: None,
            is_hns_enabled: None,
            https_only: None,
            allow_blob_public_access: None,
            created_at: None,
        }]);
        state.subscription_cursor = 1;
        assert!(handle(Action::OpenSelected, &mut state));
        assert!(
            state.registry.registries.is_none(),
            "registry cache must be cleared on subscription switch"
        );
        assert!(
            state.storage.accounts.is_none(),
            "storage cache must be cleared on subscription switch"
        );
    }

    #[test]
    fn unrelated_action_not_consumed() {
        let mut state = fixture();
        assert!(!handle(Action::Help, &mut state));
    }
}
