//! Provider errors, classified for the sync engine's retry decisions.

use std::time::Duration;

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum ProviderError {
    /// The access token was rejected even after one refresh.
    #[error("not authorized; sign in again")]
    Unauthorized,
    /// The sync cursor is too old; a full resync is required (Gmail 404 on
    /// `history.list`).
    #[error("sync cursor expired")]
    CursorExpired,
    #[error("not found: {0}")]
    NotFound(String),
    #[error("rate limited{}", .retry_after.map(|d| format!("; retry after {}s", d.as_secs())).unwrap_or_default())]
    RateLimited { retry_after: Option<Duration> },
    #[error("network error: {0}")]
    Network(String),
    #[error("server error {status}: {message}")]
    Server { status: u16, message: String },
    /// The body could not be read or decoded (a dropped connection mid
    /// body looks the same as bad JSON); retried like a network error.
    #[error("undecodable response: {0}")]
    Decode(String),
    #[error("unexpected response: {0}")]
    Invalid(String),
    #[error("permission denied: {0}")]
    Forbidden(String),
}

impl ProviderError {
    /// Worth retrying later with backoff.
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::RateLimited { .. } | Self::Network(_) | Self::Server { .. } | Self::Decode(_))
    }
}

pub type ProviderResult<T> = Result<T, ProviderError>;
