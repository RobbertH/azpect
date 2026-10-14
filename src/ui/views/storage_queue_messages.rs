//! Storage queue message peek. Reached from [`super::storage_queues`] via
//! Enter. Shows up to 32 messages from the front of the pinned queue as a
//! scrollable document: one header line per message (inserted / expires /
//! dequeue count / id) followed by its body — base64-decoded when the body is
//! base64 text (the Functions binding's default) and pretty-printed when it
//! is JSON.
//!
//! Strictly read-only: the fetch is a *peek*, so nothing is dequeued and the
//! real consumer is unaffected. Peek only sees visible messages, which is why
//! the empty state explains a non-zero approximate count.

#![allow(dead_code, unused_variables)]

use chrono::{DateTime, Datelike, Utc};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::azure::storage::QueueMessage;
use crate::ui::events::Action;
use crate::ui::state::{AppState, StorageCache};
use crate::ui::theme::Theme;

const FOOTER_HINT: &str = "j/k scroll  g/G top/bottom  Esc back  r refresh  y yank  ? help";
const HALF_PAGE: u16 = 10;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    // No scrollable content until `render_messages` measures some.
    state.scroll_max.set(0);
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);

    let (Some(acc), Some(queue)) = (
        state.storage.selected_account.as_ref(),
        state.storage.selected_queue.as_deref(),
    ) else {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.border))
            .title(Span::styled(" messages ", Style::default().fg(theme.fg)));
        let inner = block.inner(chunks[0]);
        frame.render_widget(block, chunks[0]);
        let p = Paragraph::new(Line::from(Span::styled(
            "no queue selected.",
            Style::default().fg(theme.muted),
        )));
        frame.render_widget(p, inner);
        render_footer(frame, chunks[1], theme);
        return;
    };

    let key = StorageCache::messages_key(&acc.name, queue);
    let messages = state.storage.messages.get(&key);
    let approx = approximate_count(state, &acc.id, queue);

    // Title: how much of the queue the peek covers, and that it's read-only.
    let mut title = format!(" {queue} · peek");
    if let Some(m) = messages {
        title.push_str(&format!(" {}", m.len()));
        if let Some(n) = approx {
            title.push_str(&format!(" of ~{n}"));
        }
    }
    title.push_str(" · read-only ");
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(title, Style::default().fg(theme.fg)));
    let inner = block.inner(chunks[0]);
    frame.render_widget(block, chunks[0]);

    if let Some(err) = state.storage.messages_error.get(&key) {
        let p = Paragraph::new(Line::from(Span::styled(
            format!("error: {err}"),
            Style::default().fg(theme.critical),
        )))
        .wrap(Wrap { trim: false });
        frame.render_widget(p, inner);
        render_footer(frame, chunks[1], theme);
        return;
    }

    let loading = state.storage.messages_pending.contains(&key);
    match messages {
        None => {
            let msg = if loading {
                "peeking messages …"
            } else {
                "press r to peek messages."
            };
            let p = Paragraph::new(Line::from(Span::styled(
                msg,
                Style::default().fg(theme.muted),
            )));
            frame.render_widget(p, inner);
        }
        Some(m) if m.is_empty() => {
            let msg = match approx {
                Some(n) if n > 0 => format!(
                    "no visible messages, but ~{n} counted — they're in flight (being processed) \
                     or delayed, and peek skips invisible messages."
                ),
                _ => "queue is empty.".to_string(),
            };
            let p = Paragraph::new(Line::from(Span::styled(
                msg,
                Style::default().fg(theme.muted),
            )))
            .wrap(Wrap { trim: false });
            frame.render_widget(p, inner);
        }
        Some(m) => render_messages(frame, inner, m, state, theme),
    }

    render_footer(frame, chunks[1], theme);
}

/// The queue's approximate count from the queues listing, when known.
fn approximate_count(state: &AppState, account_id: &str, queue: &str) -> Option<u64> {
    state
        .storage
        .queues
        .get(account_id)?
        .queues
        .iter()
        .find(|q| q.name == queue)?
        .approximate_message_count
}

fn render_messages(
    frame: &mut Frame,
    area: Rect,
    messages: &[QueueMessage],
    state: &AppState,
    theme: &Theme,
) {
    let all: Vec<Line> = message_lines(messages)
        .into_iter()
        .map(|parts| {
            Line::from(
                parts
                    .into_iter()
                    .map(|(text, tone)| Span::styled(text, tone.style(theme)))
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    // Slice to the visible window rather than handing ratatui a large
    // `.scroll()` offset — same reasoning as the Cosmos item view.
    let view_h = area.height.max(1) as usize;
    let max_scroll = all.len().saturating_sub(view_h);
    state
        .scroll_max
        .set(max_scroll.min(u16::MAX as usize) as u16);
    let start = (state.storage.messages_scroll as usize).min(max_scroll);
    let end = (start + view_h).min(all.len());
    let p = Paragraph::new(all[start..end].to_vec()).wrap(Wrap { trim: false });
    frame.render_widget(p, area);
}

/// How a run of text is coloured; resolved against the theme at render time
/// so [`message_lines`] can also feed the plain-text yank.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tone {
    Heading,
    Muted,
    /// A dequeue count above zero: something received the message and didn't
    /// delete it — a failing or slow consumer.
    Warn,
    Body,
}

impl Tone {
    fn style(self, theme: &Theme) -> Style {
        match self {
            Tone::Heading => Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
            Tone::Muted => Style::default().fg(theme.muted),
            Tone::Warn => Style::default().fg(theme.degraded),
            Tone::Body => Style::default().fg(theme.fg),
        }
    }
}

/// Every display line for `messages`, each as `(text, tone)` runs: a header
/// per message, its body lines, and a blank line between messages.
fn message_lines(messages: &[QueueMessage]) -> Vec<Vec<(String, Tone)>> {
    let mut out = Vec::new();
    for (i, m) in messages.iter().enumerate() {
        if i > 0 {
            out.push(Vec::new());
        }
        let (body, decoded) = display_body(m);
        let mut header = vec![
            (format!("#{}", i + 1), Tone::Heading),
            (
                format!(
                    "  inserted {}  expires {}  ",
                    format_time(m.insertion_time.as_ref()),
                    format_expiry(m.expiration_time.as_ref()),
                ),
                Tone::Muted,
            ),
            (
                format!("dequeued {}×", m.dequeue_count),
                if m.dequeue_count > 0 {
                    Tone::Warn
                } else {
                    Tone::Muted
                },
            ),
        ];
        if decoded {
            header.push(("  base64".to_string(), Tone::Muted));
        }
        header.push((format!("  id {}", m.message_id), Tone::Muted));
        out.push(header);
        out.extend(body.lines().map(|l| vec![(l.to_string(), Tone::Body)]));
    }
    out
}

/// The body to show and whether it was base64-decoded. JSON (decoded or
/// raw) is pretty-printed; anything else is shown as-is.
fn display_body(m: &QueueMessage) -> (String, bool) {
    let (text, decoded) = match m.decoded_text() {
        Some(t) => (t, true),
        None => (m.text.clone(), false),
    };
    let pretty = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .filter(|v| v.is_object() || v.is_array())
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or(text);
    (pretty, decoded)
}

/// `YYYY-MM-DD HH:MM` UTC, or `—` when absent.
fn format_time(dt: Option<&DateTime<Utc>>) -> String {
    dt.map(|t| t.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "—".to_string())
}

/// Like [`format_time`], but a message enqueued with TTL `-1` comes back
/// with a year-9999 expiry — say "never" instead.
fn format_expiry(dt: Option<&DateTime<Utc>>) -> String {
    match dt {
        Some(t) if t.year() >= 9999 => "never".to_string(),
        other => format_time(other),
    }
}

fn render_footer(frame: &mut Frame, area: Rect, theme: &Theme) {
    let p = Paragraph::new(Line::from(Span::styled(
        FOOTER_HINT,
        Style::default().fg(theme.muted),
    )));
    frame.render_widget(p, area);
}

pub fn handle(action: Action, state: &mut AppState) -> bool {
    // Clamp to the render-published ceiling, as in the Cosmos item view.
    let max_scroll = state.scroll_max.get();
    let scroll = &mut state.storage.messages_scroll;
    match action {
        Action::MoveDown => *scroll = scroll.saturating_add(1).min(max_scroll),
        Action::MoveUp => *scroll = (*scroll).min(max_scroll).saturating_sub(1),
        Action::HalfPageDown => *scroll = scroll.saturating_add(HALF_PAGE).min(max_scroll),
        Action::HalfPageUp => *scroll = (*scroll).min(max_scroll).saturating_sub(HALF_PAGE),
        Action::GotoTop => *scroll = 0,
        Action::GotoBottom => *scroll = max_scroll,
        _ => return false,
    }
    true
}

/// `y` payload: the peeked messages exactly as displayed (headers + decoded,
/// pretty-printed bodies). `None` until a peek has landed.
pub fn yank_text(state: &AppState) -> Option<String> {
    let acc = state.storage.selected_account.as_ref()?;
    let queue = state.storage.selected_queue.as_deref()?;
    let messages = state
        .storage
        .messages
        .get(&StorageCache::messages_key(&acc.name, queue))?;
    let lines: Vec<String> = message_lines(messages)
        .into_iter()
        .map(|parts| parts.into_iter().map(|(t, _)| t).collect())
        .collect();
    Some(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure::storage::{QueueListing, StorageAccount, StorageQueue};
    use crate::config::Config;
    use crate::ui::state::View;
    use chrono::TimeZone;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn account_fixture() -> StorageAccount {
        StorageAccount {
            id: "/subs/x/rg/y/sa/acct1".into(),
            name: "acct1".into(),
            resource_group: "rg".into(),
            subscription_id: "sub".into(),
            location: "westeurope".into(),
            kind: Some("StorageV2".into()),
            sku: None,
            access_tier: None,
            is_hns_enabled: None,
            https_only: None,
            allow_blob_public_access: None,
            created_at: None,
        }
    }

    fn message(id: &str, dequeues: u64, text: &str) -> QueueMessage {
        QueueMessage {
            message_id: id.into(),
            insertion_time: Utc.with_ymd_and_hms(2026, 10, 13, 9, 30, 0).single(),
            expiration_time: Utc.with_ymd_and_hms(9999, 12, 31, 23, 59, 59).single(),
            dequeue_count: dequeues,
            text: text.into(),
        }
    }

    fn fixture(messages: Option<Vec<QueueMessage>>, approx: Option<u64>) -> AppState {
        let mut state = AppState::new(Config::default());
        state.view = View::StorageQueueMessages;
        state.storage.selected_account = Some(account_fixture());
        state.storage.selected_queue = Some("orders".into());
        state.storage.queues.insert(
            "/subs/x/rg/y/sa/acct1".into(),
            QueueListing {
                queues: vec![StorageQueue {
                    name: "orders".into(),
                    approximate_message_count: approx,
                }],
                count_error: None,
            },
        );
        if let Some(m) = messages {
            state
                .storage
                .messages
                .insert(StorageCache::messages_key("acct1", "orders"), m);
        }
        state
    }

    fn draw(state: &AppState) -> String {
        let theme = Theme::catppuccin_mocha();
        let mut term = Terminal::new(TestBackend::new(140, 20)).unwrap();
        term.draw(|f| render(f, f.area(), state, &theme)).unwrap();
        format!("{:?}", term.backend().buffer())
    }

    #[test]
    fn renders_decoded_pretty_json_with_headers() {
        // `{"orderId":42}` base64-encoded, as the Functions binding stores it.
        let state = fixture(
            Some(vec![
                message("m-1", 3, "eyJvcmRlcklkIjo0Mn0="),
                message("m-2", 0, "plain text body"),
            ]),
            Some(5),
        );
        let buf = draw(&state);
        assert!(buf.contains("peek 2 of ~5"), "got: {buf}");
        assert!(buf.contains("read-only"), "got: {buf}");
        assert!(buf.contains("\"orderId\": 42"), "pretty JSON, got: {buf}");
        assert!(buf.contains("dequeued 3×"), "got: {buf}");
        assert!(buf.contains("base64"), "got: {buf}");
        assert!(buf.contains("expires never"), "got: {buf}");
        assert!(buf.contains("plain text body"), "got: {buf}");
        assert!(buf.contains("id m-2"), "got: {buf}");
    }

    #[test]
    fn empty_peek_with_nonzero_count_explains_invisible_messages() {
        let state = fixture(Some(Vec::new()), Some(7));
        let buf = draw(&state);
        assert!(buf.contains("no visible messages"), "got: {buf}");
        assert!(buf.contains("~7"), "got: {buf}");

        let state = fixture(Some(Vec::new()), Some(0));
        assert!(draw(&state).contains("queue is empty"));
    }

    #[test]
    fn renders_loading_and_error_states() {
        let mut state = fixture(None, None);
        state
            .storage
            .messages_pending
            .insert(StorageCache::messages_key("acct1", "orders"));
        assert!(draw(&state).contains("peeking messages"));

        let mut state = fixture(None, None);
        state.storage.messages_error.insert(
            StorageCache::messages_key("acct1", "orders"),
            "403 Storage Queue Data Reader".into(),
        );
        assert!(draw(&state).contains("403 Storage Queue Data Reader"));
    }

    #[test]
    fn yank_matches_displayed_text() {
        let state = fixture(Some(vec![message("m-1", 0, "eyJvcmRlcklkIjo0Mn0=")]), None);
        let text = yank_text(&state).unwrap();
        assert!(
            text.starts_with("#1  inserted 2026-10-13 09:30"),
            "got: {text}"
        );
        assert!(text.contains("\"orderId\": 42"), "got: {text}");
        assert!(!text.contains("eyJvcmRlcklkIjo0Mn0="), "got: {text}");
    }

    #[test]
    fn scrolling_clamps_to_published_max() {
        let mut state = fixture(Some(vec![message("m-1", 0, "x")]), None);
        state.scroll_max.set(4);
        assert!(handle(Action::GotoBottom, &mut state));
        assert_eq!(state.storage.messages_scroll, 4);
        assert!(handle(Action::MoveDown, &mut state));
        assert_eq!(state.storage.messages_scroll, 4);
        assert!(handle(Action::HalfPageUp, &mut state));
        assert_eq!(state.storage.messages_scroll, 0);
        assert!(!handle(Action::Refresh, &mut state));
    }
}
