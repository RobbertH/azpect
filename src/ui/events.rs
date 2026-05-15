//! Event types flowing through the single `mpsc::UnboundedReceiver<AppEvent>`
//! that drives the UI loop.
//!
//! ## Vim-flavored input model
//!
//! Cursor movement uses `h j k l` and chords like `g g`. Single-letter actions
//! (`L` for logs, `f` favorite, `s` subscription, `r` refresh, `1/7` window,
//! `w` wrap, `e` errors-only, `q` quit) are **uppercase or distinct from hjkl**
//! so they never clobber navigation. Lane 3 is responsible for the chord state
//! machine (e.g. tracking the first `g` of `g g`).

#![allow(dead_code, unused_variables)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::azure::logs::LogLine;
use crate::azure::metrics::MetricsResult;
use crate::azure::resources::Resource;
use crate::azure::subscriptions::Subscription;
use crate::ui::state::View;

/// Everything that can happen to the app.
#[derive(Debug)]
pub enum AppEvent {
    /// Periodic tick (e.g. for clock in title bar, in-flight spinner).
    Tick,
    /// Raw keyboard input from crossterm.
    Key(KeyEvent),
    /// Terminal resize.
    Resize { width: u16, height: u16 },

    /// Background load completion: subscription list.
    SubscriptionsLoaded(Result<Vec<Subscription>, String>),
    /// Background load completion: resource list for the active subscription set.
    ResourcesLoaded(Result<Vec<Resource>, String>),
    /// Background load completion: metrics for a specific resource id. The
    /// success payload carries both the loaded series and a per-metric error
    /// map for ones that the resource's plan doesn't expose.
    MetricsLoaded {
        resource_id: String,
        result: Result<MetricsResult, String>,
    },
    /// Background load completion: logs for a specific resource id.
    LogsLoaded {
        resource_id: String,
        result: Result<Vec<LogLine>, String>,
    },
    /// Background load completion: Azure Resource Health availability for a
    /// specific resource id.
    HealthLoaded {
        resource_id: String,
        result: Result<crate::azure::resource_health::ResourceAvailability, String>,
    },
}

/// Logical actions produced by the input handler. Lane 3 maps `KeyEvent` →
/// `Action` then applies the action to `AppState`. Centralising this keeps the
/// keymap declarative and makes tests easy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    Quit,
    Back,
    MoveLeft,
    MoveDown,
    MoveUp,
    MoveRight,
    HalfPageDown,
    HalfPageUp,
    GotoTop,
    GotoBottom,
    NextPanel,
    PrevPanel,
    OpenSelected,
    OpenLogs,
    ToggleErrorsOnly,
    ToggleFavorite,
    ToggleFavoritesOnly,
    StartSearch,
    SwitchSubscription,
    Refresh,
    SetWindowDay,
    SetWindowWeek,
    /// Toggle word-wrap in the logs view so long source/message cells render
    /// as multi-line rows instead of being truncated. Bound to `w`.
    ToggleWrap,
    Help,
    /// Open the vim/k9s-style command palette (`:`).
    StartCommand,
    /// Vim-style yank: copy something contextual (selected log line, displayed
    /// error, selected resource id, …) to the system clipboard via OSC52.
    Yank,
    /// Open the contextual target (selected resource / subscription) in the
    /// Azure Portal in the system default browser.
    OpenInBrowser,
    /// Sentinel emitted on the *first* `g` of a `g g` chord. The event loop
    /// stashes pending state and waits for the second key. Also returned for
    /// any key the input handler doesn't recognise.
    Noop,
}

/// Translate a single key event into a logical [`Action`].
///
/// `view` is provided so view-specific keymaps can diverge later (currently
/// only used to decide that the help overlay treats every key as Back).
///
/// `search_active` reflects whether `state.list_filter_active` is set. When
/// the search input has focus, all *printable* keys belong to the input field
/// and we surface them as [`Action::Noop`]; only `Esc` (close) and `Enter`
/// (apply) reach the global handler.
///
/// The `g g` chord is **not** resolved here — it requires history. The caller
/// (event loop) holds the chord state and consults [`is_chord_starter`] /
/// [`resolve_chord`].
pub fn key_to_action(key: KeyEvent, view: View, search_active: bool) -> Action {
    // Search-mode capture: the input field eats most keys, but vertical
    // navigation must continue to drive the underlying list so the user can
    // pick a row without leaving the search box.
    if search_active {
        return match key.code {
            KeyCode::Esc => Action::Back,
            KeyCode::Enter => Action::OpenSelected,
            KeyCode::Up => Action::MoveUp,
            KeyCode::Down => Action::MoveDown,
            KeyCode::PageDown => Action::HalfPageDown,
            KeyCode::PageUp => Action::HalfPageUp,
            _ => Action::Noop,
        };
    }

    // Help view: any key dismisses.
    if view == View::Help {
        return match key.code {
            KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q') => Action::Back,
            _ => Action::Back,
        };
    }

    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    match key.code {
        KeyCode::Esc => Action::Back,

        // Navigation
        KeyCode::Char('h') if !ctrl => Action::MoveLeft,
        KeyCode::Char('j') if !ctrl => Action::MoveDown,
        KeyCode::Char('k') if !ctrl => Action::MoveUp,
        KeyCode::Char('l') if !ctrl => Action::MoveRight,
        KeyCode::Down => Action::MoveDown,
        KeyCode::Up => Action::MoveUp,
        KeyCode::Left => Action::MoveLeft,
        KeyCode::Right => Action::MoveRight,

        // Half-page jumps (Ctrl-d / Ctrl-u). NB ratatui PageUp/PageDown also map.
        KeyCode::Char('d') if ctrl => Action::HalfPageDown,
        KeyCode::Char('u') if ctrl => Action::HalfPageUp,
        KeyCode::PageDown => Action::HalfPageDown,
        KeyCode::PageUp => Action::HalfPageUp,

        // Top / bottom. `g` alone is a chord starter; the caller resolves `g g`.
        KeyCode::Char('G') => Action::GotoBottom,
        KeyCode::Char('g') => Action::Noop, // chord starter — handled by caller

        // Panel cycling
        KeyCode::Tab => Action::NextPanel,
        KeyCode::BackTab => Action::PrevPanel,

        KeyCode::Enter => Action::OpenSelected,

        // Action keys (uppercase or distinct from hjkl)
        KeyCode::Char('L') => Action::OpenLogs,
        KeyCode::Char('e') => Action::ToggleErrorsOnly,
        KeyCode::Char('f') => Action::ToggleFavorite,
        KeyCode::Char('F') => Action::ToggleFavoritesOnly,
        KeyCode::Char('/') => Action::StartSearch,
        KeyCode::Char('s') => Action::SwitchSubscription,
        KeyCode::Char('r') => Action::Refresh,
        KeyCode::Char('1') => Action::SetWindowDay,
        KeyCode::Char('7') => Action::SetWindowWeek,
        KeyCode::Char('w') => Action::ToggleWrap,
        KeyCode::Char('?') => Action::Help,
        KeyCode::Char(':') => Action::StartCommand,
        KeyCode::Char('y') => Action::Yank,
        KeyCode::Char('o') => Action::OpenInBrowser,
        KeyCode::Char('q') => Action::Back,

        _ => Action::Noop,
    }
}

/// Returns `true` when this key event is the *first* `g` of a potential `g g`
/// chord. The event loop should stash the time and wait for the next key.
pub fn is_chord_starter(key: KeyEvent, search_active: bool) -> bool {
    !search_active
        && matches!(key.code, KeyCode::Char('g'))
        && !key.modifiers.contains(KeyModifiers::CONTROL)
}

/// Given a pending chord starter and the next key, return the resolved action
/// (and `true` if the chord was consumed). If the second key doesn't complete
/// any known chord, the caller should clear the pending state and process the
/// new key normally.
pub fn resolve_chord(starter: char, next: KeyEvent) -> Option<Action> {
    match (starter, next.code) {
        ('g', KeyCode::Char('g')) => Some(Action::GotoTop),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }
    fn key_shift(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::SHIFT)
    }
    fn key_ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn hjkl_maps_to_directions() {
        let v = View::List;
        assert_eq!(key_to_action(key('h'), v, false), Action::MoveLeft);
        assert_eq!(key_to_action(key('j'), v, false), Action::MoveDown);
        assert_eq!(key_to_action(key('k'), v, false), Action::MoveUp);
        assert_eq!(key_to_action(key('l'), v, false), Action::MoveRight);
    }

    #[test]
    fn arrow_keys_also_navigate() {
        let v = View::List;
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(key_to_action(down, v, false), Action::MoveDown);
        assert_eq!(key_to_action(up, v, false), Action::MoveUp);
    }

    #[test]
    fn capital_l_opens_logs_lowercase_l_moves_right() {
        let v = View::Detail;
        assert_eq!(key_to_action(key_shift('L'), v, false), Action::OpenLogs);
        assert_eq!(key_to_action(key('l'), v, false), Action::MoveRight);
    }

    #[test]
    fn gg_chord_via_resolve_chord() {
        // First `g` is reported as a chord starter, returning Noop.
        assert_eq!(key_to_action(key('g'), View::List, false), Action::Noop);
        assert!(is_chord_starter(key('g'), false));
        // Second `g` resolves to GotoTop.
        assert_eq!(resolve_chord('g', key('g')), Some(Action::GotoTop));
        // `g` followed by something else does not resolve.
        assert!(resolve_chord('g', key('j')).is_none());
    }

    #[test]
    fn capital_g_jumps_to_bottom_directly() {
        assert_eq!(
            key_to_action(key_shift('G'), View::List, false),
            Action::GotoBottom
        );
    }

    #[test]
    fn search_active_captures_letters_but_lets_esc_and_enter_through() {
        let v = View::List;
        // Letters become Noop because the input field consumes them.
        assert_eq!(key_to_action(key('j'), v, true), Action::Noop);
        assert_eq!(key_to_action(key('q'), v, true), Action::Noop);
        assert_eq!(key_to_action(key('L'), v, true), Action::Noop);
        // Esc cancels search → Back.
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(key_to_action(esc, v, true), Action::Back);
        // Enter applies search → OpenSelected sentinel.
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(key_to_action(enter, v, true), Action::OpenSelected);
    }

    #[test]
    fn search_active_lets_arrow_keys_through_for_list_navigation() {
        let v = View::List;
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        let pgdn = KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE);
        let pgup = KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE);
        assert_eq!(key_to_action(down, v, true), Action::MoveDown);
        assert_eq!(key_to_action(up, v, true), Action::MoveUp);
        assert_eq!(key_to_action(pgdn, v, true), Action::HalfPageDown);
        assert_eq!(key_to_action(pgup, v, true), Action::HalfPageUp);
    }

    #[test]
    fn ctrl_d_and_ctrl_u_are_half_page() {
        let v = View::List;
        assert_eq!(key_to_action(key_ctrl('d'), v, false), Action::HalfPageDown);
        assert_eq!(key_to_action(key_ctrl('u'), v, false), Action::HalfPageUp);
        // Lowercase `d` without ctrl is no longer bound — `1` is the day-window
        // action now that `w` is repurposed for wrap.
        assert_eq!(key_to_action(key('d'), v, false), Action::Noop);
        assert_eq!(key_to_action(key('1'), v, false), Action::SetWindowDay);
    }

    #[test]
    fn action_keys_match_table() {
        let v = View::List;
        assert_eq!(key_to_action(key('e'), v, false), Action::ToggleErrorsOnly);
        assert_eq!(key_to_action(key('f'), v, false), Action::ToggleFavorite);
        assert_eq!(
            key_to_action(key_shift('F'), v, false),
            Action::ToggleFavoritesOnly
        );
        assert_eq!(key_to_action(key('/'), v, false), Action::StartSearch);
        assert_eq!(
            key_to_action(key('s'), v, false),
            Action::SwitchSubscription
        );
        assert_eq!(key_to_action(key('r'), v, false), Action::Refresh);
        assert_eq!(key_to_action(key('7'), v, false), Action::SetWindowWeek);
        assert_eq!(key_to_action(key('w'), v, false), Action::ToggleWrap);
        assert_eq!(key_to_action(key('?'), v, false), Action::Help);
        assert_eq!(key_to_action(key(':'), v, false), Action::StartCommand);
        assert_eq!(key_to_action(key('q'), v, false), Action::Back);
        assert_eq!(key_to_action(key('o'), v, false), Action::OpenInBrowser);
    }

    #[test]
    fn tab_cycles_panels() {
        let v = View::List;
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        let backtab = KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT);
        assert_eq!(key_to_action(tab, v, false), Action::NextPanel);
        assert_eq!(key_to_action(backtab, v, false), Action::PrevPanel);
    }

    #[test]
    fn help_view_dismisses_on_any_key() {
        assert_eq!(key_to_action(key('x'), View::Help, false), Action::Back);
        assert_eq!(key_to_action(key('?'), View::Help, false), Action::Back);
    }
}
