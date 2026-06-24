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

const FOOTER_HINT: &str =
    "j/k move  / search  Enter select  y yank id  o portal  r refresh  ? help  q quit";

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
    let mut header_spans = vec![
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
    ];
    // Active or non-empty `/`-search: echo the query as a chip, matching the
    // resource list's header treatment.
    if state.subscription_filter_active || !state.subscription_filter.value().is_empty() {
        header_spans.push(Span::styled(
            format!("  /{} ", state.subscription_filter.value()),
            Style::default().fg(theme.accent),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(header_spans)), chunks[0]);

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
        // Carve off a top row for the `/`-search input when it's focused.
        let (search_area, list_area) = if state.subscription_filter_active {
            let parts = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(inner);
            (Some(parts[0]), parts[1])
        } else {
            (None, inner)
        };
        if let Some(sa) = search_area {
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("> ", Style::default().fg(theme.accent)),
                    Span::styled(
                        state.subscription_filter.value(),
                        Style::default().fg(theme.fg),
                    ),
                    Span::styled("█", Style::default().fg(theme.accent)),
                ])),
                sa,
            );
        }

        let filtered = state.filtered_subscription_list();
        // Row 0 is the synthetic "All subscriptions" scope (always shown — it's
        // the scope-reset, not a filterable row); the matching subscriptions
        // follow at cursor index +1.
        let total = filtered.len() + 1;
        let cursor = state.subscription_cursor.min(total - 1);

        let max_name = filtered
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

        for (i, sub) in filtered.iter().enumerate() {
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

        // A query that matches nothing still shows the "All" row above; add a
        // muted hint so the empty space reads as "no matches", not "no subs".
        if filtered.is_empty() && !state.subscription_filter.value().is_empty() {
            lines.push(Line::from(Span::styled(
                "   no subscriptions match the current filter.",
                Style::default().fg(theme.muted),
            )));
        }

        frame.render_widget(Paragraph::new(lines), list_area);
    }

    let footer = Paragraph::new(Line::from(Span::styled(
        FOOTER_HINT,
        Style::default().fg(theme.muted),
    )));
    frame.render_widget(footer, chunks[2]);
}

/// View-local input handler. Returns `true` if the action was consumed.
pub fn handle(action: Action, state: &mut AppState) -> bool {
    // +1 for the synthetic "All subscriptions" row at cursor 0. Nav/open operate
    // over the *filtered* list so `/`-search narrows what j/k and Enter see.
    let total = state.filtered_subscription_list().len() + 1;

    // Esc clears the search filter — whether the box is still focused or the
    // filter was applied then defocused (via Enter/Down) — and returns to the
    // full list. Only an already-clear list lets Esc fall through to navigation
    // (the root quit-confirm modal). Mirrors the resource list.
    if matches!(action, Action::Back)
        && (state.subscription_filter_active || !state.subscription_filter.value().is_empty())
    {
        state.subscription_filter_active = false;
        state.subscription_filter.reset();
        state.subscription_cursor = 0;
        return true;
    }

    // While the search box is focused, swallow most actions and let Lane 3
    // forward raw keys into `subscription_filter`. Mirrors the resource list.
    if state.subscription_filter_active {
        match action {
            Action::OpenSelected => {
                // First Enter defocuses; a second Enter (below) pins the row.
                state.subscription_filter_active = false;
                return true;
            }
            Action::MoveDown => {
                // Down hands focus to the filtered list, then navigates.
                state.subscription_filter_active = false;
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
        Action::StartSearch => {
            state.subscription_filter_active = true;
            true
        }
        Action::OpenSelected => {
            // Cursor 0 = "All subscriptions" → clear the filter; any other row
            // pins that subscription (indexing the *filtered* list). Persist the
            // choice so the next launch honors it (`None` for All).
            let new_selection = if state.subscription_cursor == 0 {
                None
            } else {
                state
                    .filtered_subscription_list()
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

    /// Set the picker's filter text (the event loop feeds it raw keys outside
    /// the action handler; tests just seed the value directly).
    fn type_filter(state: &mut AppState, text: &str) {
        state.subscription_filter = tui_input::Input::default().with_value(text.to_string());
    }

    #[test]
    fn slash_activates_search_box() {
        let mut state = fixture();
        assert!(handle(Action::StartSearch, &mut state));
        assert!(state.subscription_filter_active);
    }

    #[test]
    fn filter_narrows_the_list_and_open_indexes_the_filtered_set() {
        let mut state = fixture();
        type_filter(&mut state, "bet"); // matches "beta" only
        assert_eq!(state.filtered_subscription_list().len(), 1);
        // Rows are [All, beta]; cursor 1 = beta even though beta was index 2
        // in the unfiltered list.
        state.subscription_cursor = 1;
        assert!(handle(Action::OpenSelected, &mut state));
        assert_eq!(
            state.selected_subscription.as_deref(),
            Some("22222222-2222-2222-2222-222222222222")
        );
    }

    #[test]
    fn filter_matches_on_id_substring_too() {
        let mut state = fixture();
        type_filter(&mut state, "2222-2222");
        let matched = state.filtered_subscription_list();
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].display_name, "beta");
    }

    #[test]
    fn nav_is_bounded_by_the_filtered_length() {
        let mut state = fixture();
        type_filter(&mut state, "alpha"); // 1 match → rows [All, alpha]
                                          // Down from All lands on alpha and clamps there (no phantom beta row).
        assert!(handle(Action::MoveDown, &mut state));
        assert_eq!(state.subscription_cursor, 1);
        assert!(handle(Action::MoveDown, &mut state));
        assert_eq!(state.subscription_cursor, 1, "clamped to filtered last");
    }

    #[test]
    fn esc_defocuses_search_box_without_leaving_picker() {
        let mut state = fixture();
        handle(Action::StartSearch, &mut state);
        assert!(state.subscription_filter_active);
        // Back is consumed (returns true) so it doesn't bubble to a view pop.
        assert!(handle(Action::Back, &mut state));
        assert!(!state.subscription_filter_active);
    }

    #[test]
    fn renders_filter_chip_and_search_row_when_active() {
        let theme = Theme::catppuccin_mocha();
        let backend = TestBackend::new(80, 12);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = fixture();
        state.subscription_filter_active = true;
        type_filter(&mut state, "bet");
        term.draw(|f| render(f, f.area(), &state, &theme)).unwrap();
        let buf = format!("{:?}", term.backend().buffer());
        assert!(buf.contains("/bet"), "filter chip should echo the query");
        assert!(buf.contains("beta"));
        assert!(!buf.contains("alpha"), "non-matching subs are hidden");
    }
}
