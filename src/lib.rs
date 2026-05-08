//! `azpect` — Azure API observability TUI.
//!
//! Module layout (the parallel-build "contract"):
//!
//! - `azure`      — auth, ARM, Resource Graph, Monitor metrics, Log Analytics, derived health
//! - `config`     — favorites + theme + last-subscription persistence (TOML under XDG config)
//! - `ui`         — ratatui app loop, event plumbing, view rendering
//! - `error`      — central error type re-exported for convenience
//!
//! Public function signatures and domain types in each module form the contract
//! that the parallel implementation lanes must not break.

pub mod azure;
pub mod config;
pub mod error;
pub mod ui;
