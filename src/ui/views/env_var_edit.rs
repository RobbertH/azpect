//! Renders the guarded add/edit-env-var modal opened from the env-vars page
//! (`Ctrl+E` / `Ctrl+N`). Two phases, mirroring [`crate::ui::state::EnvVarEdit`]:
//!
//! - **Editing** — a small form with a name field (editable only when *adding*;
//!   you can't rename a setting) and a value field. Tab switches fields in Add
//!   mode; Enter advances to the confirm step.
//! - **Confirming** — the deliberate gate: shows the `old → new` diff (or the new
//!   entry being added), warns that a Container App write deploys a new revision,
//!   and defaults focus to **Cancel** so an accidental Enter never writes.
//!
//! The key handling + write spawn live in `app.rs` (they need `auth`/`tx`); this
//! module is render-only, like the other modal renderers.

#![allow(dead_code, unused_variables)]

use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::azure::resources::ResourceKind;
use crate::ui::state::{AppState, EnvVarEdit, EnvVarEditMode, EnvVarEditPhase, EnvVarField};
use crate::ui::theme::Theme;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let Some(edit) = state.env_var_edit.as_ref() else {
        return;
    };
    let resource_name = state
        .selected_resource()
        .map(|r| r.name.clone())
        .unwrap_or_else(|| {
            edit.resource_id
                .rsplit('/')
                .next()
                .unwrap_or("resource")
                .to_string()
        });

    match edit.phase {
        EnvVarEditPhase::Editing => render_editing(frame, area, edit, &resource_name, theme),
        EnvVarEditPhase::Confirming => render_confirming(frame, area, edit, &resource_name, theme),
    }
}

fn render_editing(frame: &mut Frame, area: Rect, edit: &EnvVarEdit, resource: &str, theme: &Theme) {
    let adding = matches!(edit.mode, EnvVarEditMode::Add);
    let is_container = edit.resource_kind == ResourceKind::ContainerApp;
    // Title + (container line) + name + value + (error) + blank + hint.
    let mut height = 8u16;
    if is_container {
        height += 1;
    }
    if edit.error.is_some() {
        height += 1;
    }
    let popup = centered(70, height, area);
    if popup.width == 0 || popup.height == 0 {
        return;
    }
    frame.render_widget(Clear, popup);

    let title = if adding {
        " add env var "
    } else {
        " edit env var "
    };
    let block = modal_block(title, theme);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let muted = Style::default().fg(theme.muted);
    let mut lines: Vec<Line> = vec![Line::from(Span::styled(resource.to_string(), muted))];

    if is_container {
        lines.push(Line::from(vec![
            Span::styled("container  ", muted),
            Span::styled(
                edit.attribution.clone().unwrap_or_default(),
                Style::default().fg(theme.fg),
            ),
        ]));
    }

    // Name field — editable only in Add mode.
    lines.push(field_line(
        "name ",
        edit.name.value(),
        adding && edit.focus == EnvVarField::Name,
        /* editable */ adding,
        theme,
    ));
    // Value field.
    lines.push(field_line(
        "value",
        edit.value.value(),
        edit.focus == EnvVarField::Value,
        /* editable */ true,
        theme,
    ));

    if let Some(err) = edit.error.as_deref() {
        lines.push(Line::from(Span::styled(
            err.to_string(),
            Style::default().fg(theme.degraded),
        )));
    }

    lines.push(Line::from(""));
    let hint = if adding {
        "Tab switch field · Enter continue · Esc cancel"
    } else {
        "Enter continue · Esc cancel"
    };
    lines.push(Line::from(Span::styled(hint, muted)));

    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Left), inner);
}

fn render_confirming(
    frame: &mut Frame,
    area: Rect,
    edit: &EnvVarEdit,
    resource: &str,
    theme: &Theme,
) {
    let is_container = edit.resource_kind == ResourceKind::ContainerApp;
    let muted = Style::default().fg(theme.muted);
    let name = edit.name.value().trim();
    let new_value = edit.value.value();

    // Header + name + diff line(s) + (revision warn) + (error) + blank + buttons.
    let mut lines: Vec<Line> = vec![
        Line::from(Span::styled(format!("{resource}  ·  {name}"), muted)),
        Line::from(""),
    ];

    match &edit.mode {
        EnvVarEditMode::Edit { original_value } => {
            lines.push(Line::from(vec![
                Span::styled("old  ", muted),
                Span::styled(display_value(original_value), Style::default().fg(theme.fg)),
            ]));
            lines.push(Line::from(vec![
                Span::styled("new  ", muted),
                Span::styled(
                    display_value(new_value),
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
        }
        EnvVarEditMode::Add => {
            lines.push(Line::from(vec![
                Span::styled("new  ", muted),
                Span::styled(
                    display_value(new_value),
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
        }
    }

    if is_container {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "⚠ deploys a new revision",
            Style::default().fg(theme.degraded),
        )));
    }

    if let Some(err) = edit.error.as_deref() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            err.to_string(),
            Style::default().fg(theme.degraded),
        )));
    }

    lines.push(Line::from(""));
    if edit.in_flight {
        lines.push(Line::from(Span::styled(
            "writing…",
            Style::default().fg(theme.accent),
        )));
    } else {
        lines.push(button_row(edit.confirm_yes, theme));
        lines.push(Line::from(Span::styled(
            "←/→ choose · Enter confirm · y write · n/Esc back",
            muted,
        )));
    }

    let height = lines.len() as u16 + 2; // borders
    let popup = centered(70, height, area);
    if popup.width == 0 || popup.height == 0 {
        return;
    }
    frame.render_widget(Clear, popup);
    let block = modal_block(" confirm write ", theme);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.height == 0 || inner.width == 0 {
        return;
    }
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Left), inner);
}

/// One labelled input row. `focused` draws a cursor block; a non-editable field
/// (the locked name in Edit mode) is shown muted with no cursor.
fn field_line<'a>(
    label: &'a str,
    value: &'a str,
    focused: bool,
    editable: bool,
    theme: &Theme,
) -> Line<'a> {
    let label_span = Span::styled(format!("{label}  "), Style::default().fg(theme.muted));
    let value_style = if editable {
        Style::default().fg(theme.fg)
    } else {
        Style::default().fg(theme.muted)
    };
    let mut spans = vec![
        label_span,
        Span::styled(
            if focused { "▍ " } else { "  " },
            Style::default().fg(theme.accent),
        ),
        Span::styled(value.to_string(), value_style),
    ];
    if focused {
        spans.push(Span::styled("█", Style::default().fg(theme.accent)));
    }
    Line::from(spans)
}

/// The `[ Cancel ] [ Write ]` row; the focused button is reverse-highlighted.
fn button_row(confirm_yes: bool, theme: &Theme) -> Line<'static> {
    let focused = Style::default()
        .bg(theme.accent)
        .fg(theme.bg)
        .add_modifier(Modifier::BOLD);
    let normal = Style::default().fg(theme.fg);
    let (cancel_style, write_style) = if confirm_yes {
        (normal, focused)
    } else {
        (focused, normal)
    };
    Line::from(vec![
        Span::styled("  Cancel  ", cancel_style),
        Span::raw("   "),
        Span::styled("  Write  ", write_style),
    ])
}

fn modal_block(title: &str, theme: &Theme) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(
            title.to_string(),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(theme.bg).fg(theme.fg))
}

/// Render an empty value as a visible marker so the confirm step doesn't look
/// blank when writing `KEY=`.
fn display_value(v: &str) -> String {
    if v.is_empty() {
        "(empty)".to_string()
    } else {
        v.to_string()
    }
}

/// Centered fixed-size rect, clamped to `area`.
fn centered(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}
