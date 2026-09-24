//! The one error type that crosses the FFI boundary (spec §4.2).

/// Machine-readable category, so Swift can branch without parsing messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ErrorKind {
    InvalidInput,
    NotFound,
    Storage,
    Network,
    Auth,
    RateLimited,
    Agent,
    PermissionDenied,
    Cancelled,
    Internal,
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum CoreError {
    #[error("{message}")]
    Failed { kind: ErrorKind, message: String },
}

impl CoreError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self::Failed { kind, message: message.into() }
    }

    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::Failed { kind, .. } => *kind,
        }
    }
}
