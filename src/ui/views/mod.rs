//! Per-screen rendering. Each view module exposes a `render(frame, area, state, theme)`
//! function and may expose a `handle(action, state)` helper for view-local input.

pub mod detail;
pub mod help;
pub mod list;
pub mod logs;
pub mod logs_detail;
pub mod subscriptions;
