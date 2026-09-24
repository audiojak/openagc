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
    #[error("unexpected response: {0}")]
    Invalid(String),
    #[error("permission denied: {0}")]
    Forbidden(String),
}

impl ProviderError {
    /// Worth retrying later with backoff.
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::RateLimited { .. } | Self::Network(_) | Self::Server { .. })
    }
}

pub type ProviderResult<T> = Result<T, ProviderError>;
