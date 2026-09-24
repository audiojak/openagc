//! Sync errors.

use mail_store::StoreError;
use provider_api::ProviderError;

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error("account has not been bootstrapped")]
    NotBootstrapped,
    /// The history cursor expired; everything was re-queued. Not a failure:
    /// the caller should keep backfilling.
    #[error("sync history expired; full resync started")]
    ResyncStarted,
}

impl SyncError {
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Provider(p) if p.is_transient())
    }
}

pub type SyncResult<T> = Result<T, SyncError>;
