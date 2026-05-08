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
    pub degraded: Color,
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

    pub fn catppuccin_mocha() -> Theme {
        Theme {
            bg: Color::Reset,
            fg: Color::Rgb(0xcd, 0xd6, 0xf4),
            muted: Color::Rgb(0x6c, 0x70, 0x86),
            accent: Color::Rgb(0xcb, 0xa6, 0xf7),
            favorite: Color::Rgb(0xf9, 0xe2, 0xaf),
            healthy: Color::Rgb(0xa6, 0xe3, 0xa1),
            degraded: Color::Rgb(0xf9, 0xe2, 0xaf),
            critical: Color::Rgb(0xf3, 0x8b, 0xa8),
            unknown: Color::Rgb(0x6c, 0x70, 0x86),
            border: Color::Rgb(0x45, 0x47, 0x5a),
            selection_bg: Color::Rgb(0x31, 0x32, 0x44),
        }
    }

    pub fn catppuccin_latte() -> Theme {
        // Lane 3 fills in the latte palette.
        Theme::catppuccin_mocha()
    }

    pub fn dark() -> Theme {
        Theme::catppuccin_mocha()
    }

    pub fn light() -> Theme {
        Theme::catppuccin_mocha()
    }

    pub fn selection(&self) -> Style {
        Style::default().bg(self.selection_bg).add_modifier(Modifier::BOLD)
    }
}
