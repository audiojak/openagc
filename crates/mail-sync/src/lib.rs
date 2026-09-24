//! Sync engine, outbox and backfill scheduler (spec §7.4).

mod attachments;
mod compose;
mod convert;
mod engine;
mod error;
mod outbox;

pub use attachments::{AttachmentFile, attachment_file, safe_filename};
pub use compose::{forward_draft, reply_draft, schedule_draft_sync, send_draft};
pub use convert::to_incoming;
pub use engine::{
    BACKFILL_BATCH, INBOX_PHASES, IncrementalReport, NewMail, PHASES, SyncEngine, SyncObserver, SyncPhase, SyncProgress,
};
pub use error::{SyncError, SyncResult};
pub use outbox::{DrainReport, LocalChange, MAX_ATTEMPTS, apply_local_change, now_millis};
