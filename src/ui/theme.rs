//! Color theme. Lane 3 picks the actual ratatui `Style`s. The named themes here
//! are the same set flowrs ships: catppuccin variants plus straight dark/light.

#![allow(dead_code, unused_variables)]

use ratatui::style::{Color, Modifier, Style};

#[derive(Clone, Copy)]
pub struct Theme {
    pub bg: Color,
    pub fg: Color,
    pub muted: Color,
    pub accent: Color,
    pub favorite: Color,
    pub healthy: Color,
    /// Running-but-quiet resources: distinct enough from `healthy` to telegraph
    /// "no traffic" at a glance, but not alarming like `degraded`.
    pub idle: Color,
    pub degraded: Color,
    /// HTTP 4xx client errors: worth seeing but usually the caller's fault —
    /// sits between `degraded` (yellow) and `critical` (red) in severity, and
    /// must stay distinguishable from both since the metric grid shows all
    /// three side by side.
    pub client_error: Color,
    pub critical: Color,
    pub unknown: Color,
    pub border: Color,
    pub selection_bg: Color,
}

impl Theme {
    pub fn by_name(name: &str) -> Theme {
        match name {
            "catppuccin-latte" => Theme::catppuccin_latte(),
            "dark" => Theme::dark(),
            "light" => Theme::light(),
            _ => Theme::catppuccin_mocha(),
        }
    }

    /// Catppuccin Mocha — dark, blue-ish background. Defaults to `Color::Reset`
    /// for `bg` so the user's terminal background shows through.
    pub fn catppuccin_mocha() -> Theme {
        Theme {
            bg: Color::Reset,
            fg: Color::Rgb(0xcd, 0xd6, 0xf4),           // text
            muted: Color::Rgb(0x6c, 0x70, 0x86),        // overlay0
            accent: Color::Rgb(0xcb, 0xa6, 0xf7),       // mauve
            favorite: Color::Rgb(0xf9, 0xe2, 0xaf),     // yellow
            healthy: Color::Rgb(0xa6, 0xe3, 0xa1),      // green
            idle: Color::Rgb(0x89, 0xb4, 0xfa),         // blue
            degraded: Color::Rgb(0xf9, 0xe2, 0xaf),     // yellow
            client_error: Color::Rgb(0xfa, 0xb3, 0x87), // peach
            critical: Color::Rgb(0xf3, 0x8b, 0xa8),     // red
            unknown: Color::Rgb(0x6c, 0x70, 0x86),      // overlay0
            border: Color::Rgb(0x45, 0x47, 0x5a),       // surface1
            selection_bg: Color::Rgb(0x31, 0x32, 0x44), // surface0
        }
    }

    /// Catppuccin Latte — light pastel palette. Hex values from the public
    /// catppuccin palette spec.
    pub fn catppuccin_latte() -> Theme {
        Theme {
            bg: Color::Reset,
            fg: Color::Rgb(0x4c, 0x4f, 0x69),           // text
            muted: Color::Rgb(0x9c, 0xa0, 0xb0),        // overlay0
            accent: Color::Rgb(0x88, 0x39, 0xef),       // mauve
            favorite: Color::Rgb(0xdf, 0x8e, 0x1d),     // yellow
            healthy: Color::Rgb(0x40, 0xa0, 0x2b),      // green
            idle: Color::Rgb(0x1e, 0x66, 0xf5),         // blue
            degraded: Color::Rgb(0xdf, 0x8e, 0x1d),     // yellow
            client_error: Color::Rgb(0xfe, 0x64, 0x0b), // peach
            critical: Color::Rgb(0xd2, 0x0f, 0x39),     // red
            unknown: Color::Rgb(0x9c, 0xa0, 0xb0),      // overlay0
            border: Color::Rgb(0xbc, 0xc0, 0xcc),       // surface1
            selection_bg: Color::Rgb(0xcc, 0xd0, 0xda), // surface0
        }
    }

    /// 16-color "dark" theme that survives non-truecolor terminals. Falls back
    /// to ANSI palette indices.
    pub fn dark() -> Theme {
        Theme {
            bg: Color::Reset,
            fg: Color::Gray,
            muted: Color::DarkGray,
            accent: Color::Magenta,
            favorite: Color::Yellow,
            healthy: Color::Green,
            idle: Color::Blue,
            degraded: Color::Yellow,
            // No orange in the ANSI-16 palette; LightRed is the closest tone
            // that stays apart from both Yellow (degraded) and Red (critical).
            client_error: Color::LightRed,
            critical: Color::Red,
            unknown: Color::DarkGray,
            border: Color::DarkGray,
            selection_bg: Color::Rgb(0x33, 0x33, 0x33),
        }
    }

    /// 16-color "light" theme. Avoids relying on truecolor support.
    pub fn light() -> Theme {
        Theme {
            bg: Color::Reset,
            fg: Color::Black,
            muted: Color::DarkGray,
            accent: Color::Magenta,
            favorite: Color::Yellow,
            healthy: Color::Green,
            idle: Color::Blue,
            degraded: Color::Yellow,
            client_error: Color::LightRed,
            critical: Color::Red,
            unknown: Color::Gray,
            border: Color::Gray,
            selection_bg: Color::Rgb(0xe0, 0xe0, 0xe0),
        }
    }

    pub fn selection(&self) -> Style {
        Style::default()
            .bg(self.selection_bg)
            .add_modifier(Modifier::BOLD)
    }
}
