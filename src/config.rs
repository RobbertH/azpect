//! Persistent user state: favorites, last subscription, time-window default,
//! theme. Lives under `${XDG_CONFIG_HOME:-~/.config}/azpect/config.toml`.
//!
//! The file is created on first run. Missing or unparseable files yield
//! [`Config::default`] rather than an error so the TUI always boots.

use std::fs;
use std::path::PathBuf;

use anyhow::{anyhow, Context};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::azure::metrics::TimeRange;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    /// Last subscription the user was viewing. The TUI restores this on launch.
    #[serde(default)]
    pub last_subscription_id: Option<String>,

    /// Last resource the user opened (Detail or Logs). The TUI restores the
    /// list cursor to this id when the resource list loads, if the id is
    /// present in the loaded set.
    #[serde(default)]
    pub last_resource_id: Option<String>,

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
            last_resource_id: None,
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
    let path = config_path()?;
    let raw = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Config::default());
        }
        Err(err) => {
            return Err(err).with_context(|| format!("reading config file at {}", path.display()));
        }
    };
    match toml::from_str::<Config>(&raw) {
        Ok(cfg) => Ok(cfg),
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "config file is unparseable; falling back to defaults"
            );
            Ok(Config::default())
        }
    }
}

/// Write config to disk, creating the parent directory if needed.
pub fn save(cfg: &Config) -> anyhow::Result<()> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating config directory at {}", parent.display()))?;
    }
    let serialized = toml::to_string_pretty(cfg).context("serializing config to TOML")?;

    // Atomic write: write to a sibling tmp file, then rename over the target.
    let tmp = match path.file_name() {
        Some(name) => {
            let mut t = name.to_os_string();
            t.push(".tmp");
            path.with_file_name(t)
        }
        None => return Err(anyhow!("config path has no file name: {}", path.display())),
    };
    fs::write(&tmp, serialized)
        .with_context(|| format!("writing temporary config at {}", tmp.display()))?;
    fs::rename(&tmp, &path)
        .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Resolve the config file path. Pure function so tests can inspect it.
pub fn config_path() -> anyhow::Result<PathBuf> {
    let dirs = ProjectDirs::from("", "", "azpect")
        .ok_or_else(|| anyhow!("could not determine a config directory for this OS"))?;
    Ok(dirs.config_dir().join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_round_trips_through_toml() {
        let cfg = Config::default();
        let serialized = toml::to_string_pretty(&cfg).expect("serialize default config");
        let parsed: Config = toml::from_str(&serialized).expect("parse default config");

        assert_eq!(parsed.last_subscription_id, cfg.last_subscription_id);
        assert_eq!(parsed.last_resource_id, cfg.last_resource_id);
        assert_eq!(parsed.favorites, cfg.favorites);
        assert_eq!(parsed.default_window, cfg.default_window);
        assert_eq!(parsed.theme, cfg.theme);
    }

    #[test]
    fn toggle_favorite_adds_and_removes() {
        let mut cfg = Config::default();
        let id = "/subscriptions/00000000-0000-0000-0000-000000000000/resourceGroups/example-rg/providers/Microsoft.Web/sites/example-app";

        assert!(!cfg.is_favorite(id));

        // First toggle adds.
        let added = cfg.toggle_favorite(id);
        assert!(added, "first toggle should add the favorite");
        assert!(cfg.is_favorite(id));
        assert_eq!(cfg.favorites.len(), 1);

        // Second toggle removes.
        let added_again = cfg.toggle_favorite(id);
        assert!(!added_again, "second toggle should remove the favorite");
        assert!(!cfg.is_favorite(id));
        assert!(cfg.favorites.is_empty());
    }

    #[test]
    fn config_path_is_under_azpect_directory() {
        let path = config_path().expect("resolve config path");
        assert_eq!(path.file_name().and_then(|s| s.to_str()), Some("config.toml"));
        let dir = path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .expect("config path has a parent directory");
        assert_eq!(dir, "azpect");
    }
}
