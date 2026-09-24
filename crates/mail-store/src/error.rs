//! Store errors.

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("migration failed: {0}")]
    Migration(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid data: {0}")]
    Invalid(String),
    #[error("i/o error: {0}")]
    Io(String),
    #[error("store is closed")]
    Closed,
}

impl From<serde_json::Error> for StoreError {
    fn from(e: serde_json::Error) -> Self {
        Self::Invalid(e.to_string())
    }
}

pub type StoreResult<T> = Result<T, StoreError>;
