//! SQLite mail store: schema, migrations, queries, full-text search
//! (spec §6). The store is the only writer of the database; every write
//! that changes messages also maintains the denormalized thread data and
//! search index in the same transaction.

pub mod consistency;
mod db;
pub mod demo;
mod error;
pub mod queue;
pub mod read;
mod write;

pub use db::{Db, READER_COUNT, schema_version};
pub use error::{StoreError, StoreResult};
pub use read::ThreadPage;
pub use write::{ARCHIVE_LABEL, IncomingAttachment, IncomingMessage, MailWriter, MailboxChange, ThreadChanges};
