//! Central error type. We use [`anyhow::Error`] in public signatures across the
//! crate for ergonomics; this module exists so individual modules can reach for
//! a richer typed error if they need to expose specific failure modes (e.g. the
//! Logs view distinguishing "no diagnostic settings configured" from a 401).

use thiserror::Error;

#[derive(Debug, Error)]
pub enum AzpectError {
    #[error("authentication failed: {0}")]
    Auth(String),

    #[error("azure api error ({status}): {message}")]
    Api { status: u16, message: String },

    #[error("resource has no log destination configured")]
    NoLogDestination,

    #[error("metric {0} not available for this resource type")]
    UnsupportedMetric(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("config parse: {0}")]
    Config(String),
}

pub type AzpectResult<T> = std::result::Result<T, AzpectError>;
