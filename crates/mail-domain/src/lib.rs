//! Plain mail domain types shared by every crate (spec §5).

mod ids;
mod model;
mod redacted;

pub use ids::{AccountId, AttachmentId, DraftId, LabelId, MessageId, Millis, ThreadId};
pub use model::{
    Attachment, Body, BodyState, Draft, EmailAddress, Label, LabelColor, LabelKind, Mailbox, MailboxKind, Message,
    ThreadSummary, system_labels,
};
pub use redacted::Redacted;
