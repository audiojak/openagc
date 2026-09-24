//! The provider abstraction sync is written against (spec §7.6), plus the
//! HTTP, retry, token and rate-limit plumbing providers share. Gmail is the
//! only implementation in the MVP; cursors and page tokens are opaque so an
//! IMAP provider (UIDVALIDITY/MODSEQ) fits later.

mod error;
pub mod http;
pub mod rate_limit;
pub mod token;

use async_trait::async_trait;
use mail_domain::{EmailAddress, Label, LabelId, MessageId, Millis, ThreadId};

pub use error::{ProviderError, ProviderResult};
pub use http::{HttpClient, RetryPolicy};
pub use rate_limit::{Priority, RateLimiter};
pub use token::{AccessToken, TokenSource};

/// Opaque position in the provider's change stream (Gmail: a historyId).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SyncCursor(pub String);

/// Opaque continuation token for a listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageToken(pub String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub email: String,
    pub messages_total: u64,
    /// Where incremental sync starts; recorded before bootstrap lists
    /// anything so nothing arriving during bootstrap is missed (spec §7.4).
    pub cursor: SyncCursor,
}

/// Which messages to list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListFilter {
    pub label_ids: Vec<LabelId>,
    /// Provider search syntax, e.g. `newer_than:30d`.
    pub query: Option<String>,
    pub include_spam_trash: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdPage {
    pub ids: Vec<(MessageId, ThreadId)>,
    pub next: Option<PageToken>,
    /// The provider's estimate of the total, when it gives one.
    pub estimated_total: Option<u64>,
}

/// A message as fetched, before sanitizing. `html` is the original HTML.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FetchedMessage {
    pub id: MessageId,
    pub thread_id: ThreadId,
    pub label_ids: Vec<LabelId>,
    pub snippet: String,
    /// When the provider received it (Gmail `internalDate`).
    pub internal_date: Millis,
    pub size_estimate: u64,
    pub message_id_header: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub from: Option<EmailAddress>,
    pub to: Vec<EmailAddress>,
    pub cc: Vec<EmailAddress>,
    pub bcc: Vec<EmailAddress>,
    pub reply_to: Vec<EmailAddress>,
    pub subject: String,
    /// The `Date` header, if parseable.
    pub date: Option<Millis>,
    /// `None` when only metadata was fetched.
    pub body: Option<FetchedBody>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FetchedBody {
    pub text: Option<String>,
    pub html: Option<String>,
    pub attachments: Vec<FetchedAttachment>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FetchedAttachment {
    pub part_id: Option<String>,
    /// Provider id for fetching the bytes on demand.
    pub attachment_id: Option<String>,
    pub filename: String,
    pub mime_type: String,
    pub size: u64,
    pub content_id: Option<String>,
    pub is_inline: bool,
}

/// One change in the provider's history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    MessageAdded { id: MessageId, thread_id: ThreadId, label_ids: Vec<LabelId> },
    MessageDeleted { id: MessageId },
    LabelsAdded { id: MessageId, label_ids: Vec<LabelId> },
    LabelsRemoved { id: MessageId, label_ids: Vec<LabelId> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeSet {
    pub changes: Vec<Change>,
    /// Cursor to resume from after applying `changes`.
    pub cursor: SyncCursor,
}

/// Add/remove labels on a set of messages (archive = remove INBOX).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelOp {
    pub message_ids: Vec<MessageId>,
    pub add: Vec<LabelId>,
    pub remove: Vec<LabelId>,
}

#[async_trait]
pub trait MailProvider: Send + Sync {
    async fn profile(&self) -> ProviderResult<Profile>;
    async fn list_labels(&self) -> ProviderResult<Vec<Label>>;
    async fn list_message_ids(&self, filter: &ListFilter, page: Option<PageToken>) -> ProviderResult<IdPage>;
    /// Full messages (headers, bodies, attachment metadata; not attachment
    /// bytes). Missing ids are omitted from the result, not errors.
    async fn fetch_messages(&self, ids: &[MessageId], priority: Priority) -> ProviderResult<Vec<FetchedMessage>>;
    /// Changes since `cursor`. [`ProviderError::CursorExpired`] means a full
    /// resync is needed.
    async fn changes_since(&self, cursor: &SyncCursor) -> ProviderResult<ChangeSet>;
    async fn modify_labels(&self, op: &LabelOp) -> ProviderResult<()>;
    async fn move_to_trash(&self, id: &MessageId) -> ProviderResult<()>;
    /// Send raw RFC 5322 bytes, threaded into `thread` when given.
    async fn send(&self, raw: &[u8], thread: Option<&ThreadId>) -> ProviderResult<MessageId>;
    async fn fetch_attachment(&self, message: &MessageId, attachment_id: &str) -> ProviderResult<Vec<u8>>;
}
