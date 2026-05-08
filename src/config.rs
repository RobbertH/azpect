//! Persistent user state: favorites, last subscription, time-window default,
//! theme. Lives under `${XDG_CONFIG_HOME:-~/.config}/azpect/config.toml`.
//!
//! The file is created on first run. Missing or unparseable files yield
//! [`Config::default`] rather than an error so the TUI always boots.

#![allow(dead_code, unused_variables)]

use serde::{Deserialize, Serialize};

use crate::azure::metrics::TimeRange;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    /// Last subscription the user was viewing. The TUI restores this on launch.
    #[serde(default)]
    pub last_subscription_id: Option<String>,

    /// Full Azure resource IDs (`/subscriptions/.../resourceGroups/.../providers/...`).
    /// Stored as IDs because they are stable and globally unique.
    #[serde(default)]
    pub favorites: Vec<String>,

    /// Default time window in the detail view.
    #[serde(default)]
    pub default_window: TimeRange,

    /// One of: `"catppuccin-mocha"`, `"catppuccin-latte"`, `"dark"`, `"light"`.
    #[serde(default = "default_theme")]
    pub theme: String,
}

fn default_theme() -> String {
    "catppuccin-mocha".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            last_subscription_id: None,
            favorites: Vec::new(),
            default_window: TimeRange::default(),
            theme: default_theme(),
        }
    }
}

impl Config {
    pub fn is_favorite(&self, resource_id: &str) -> bool {
        self.favorites.iter().any(|id| id == resource_id)
    }

    /// Returns `true` if the favorite was added (false if it was already present and got removed).
    pub fn toggle_favorite(&mut self, resource_id: &str) -> bool {
        if let Some(pos) = self.favorites.iter().position(|id| id == resource_id) {
            self.favorites.remove(pos);
            false
        } else {
            self.favorites.push(resource_id.to_string());
            true
        }
    }
}

/// Read config from disk. Missing/bad file → `Config::default()` (logs a warning).
pub fn load() -> anyhow::Result<Config> {
    todo!("Lane 1: resolve XDG path via `directories`, read TOML, fall back to default on parse error")
}

/// Write config to disk, creating the parent directory if needed.
pub fn save(cfg: &Config) -> anyhow::Result<()> {
    todo!("Lane 1: serialize to TOML, write atomically (tmp file + rename)")
}

/// Resolve the config file path. Pure function so tests can inspect it.
pub fn config_path() -> anyhow::Result<std::path::PathBuf> {
    todo!("Lane 1: directories::ProjectDirs::from(\"\", \"\", \"azpect\")")
}
