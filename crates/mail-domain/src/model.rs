//! Mail domain types (spec §5). Plain data: no I/O, no provider specifics.

use serde::{Deserialize, Serialize};

use crate::ids::{AttachmentId, DraftId, LabelId, MessageId, Millis, ThreadId};

/// Canonical system label ids. Providers map their own concepts onto these
/// (Gmail's are identical; an IMAP provider would map folders).
pub mod system_labels {
    pub const INBOX: &str = "INBOX";
    pub const SENT: &str = "SENT";
    pub const DRAFT: &str = "DRAFT";
    pub const SPAM: &str = "SPAM";
    pub const TRASH: &str = "TRASH";
    pub const STARRED: &str = "STARRED";
    pub const IMPORTANT: &str = "IMPORTANT";
    pub const UNREAD: &str = "UNREAD";

    /// Labels an agent may never add or remove directly (spec §10.2).
    pub const PROTECTED: &[&str] = &[SPAM, TRASH, DRAFT, SENT];
}

/// An address as it appears in a header: optional display name + address.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EmailAddress {
    pub name: Option<String>,
    pub email: String,
}

impl EmailAddress {
    pub fn new(name: Option<&str>, email: &str) -> Self {
        let name = name.map(str::trim).filter(|n| !n.is_empty()).map(str::to_owned);
        Self { name, email: email.trim().to_owned() }
    }

    /// Lowercased address for matching. Gmail treats the local part
    /// case-insensitively; so does everything user-facing here.
    pub fn normalized(&self) -> String {
        self.email.to_lowercase()
    }

    /// Name if present, else the address; what a list row shows.
    pub fn display(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.email)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LabelKind {
    System,
    User,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LabelColor {
    pub background: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Label {
    pub id: LabelId,
    pub name: String,
    pub kind: LabelKind,
    pub color: Option<LabelColor>,
    /// Whether the provider shows it in the label list.
    pub visible: bool,
}

impl Label {
    pub fn is_protected(&self) -> bool {
        system_labels::PROTECTED.contains(&self.id.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MailboxKind {
    Inbox,
    Starred,
    Important,
    Sent,
    Drafts,
    /// Everything not in Inbox, Spam or Trash.
    Archive,
    Spam,
    Trash,
    /// A user label.
    Label,
}

/// A sidebar entry. System mailboxes are backed by a system label except
/// Archive, which is a query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mailbox {
    pub kind: MailboxKind,
    /// `None` only for Archive.
    pub label_id: Option<LabelId>,
    pub name: String,
    pub unread_count: u32,
    pub total_count: u32,
}

impl MailboxKind {
    /// The system label backing this mailbox, if any.
    pub fn system_label(self) -> Option<&'static str> {
        use system_labels::*;
        match self {
            Self::Inbox => Some(INBOX),
            Self::Starred => Some(STARRED),
            Self::Important => Some(IMPORTANT),
            Self::Sent => Some(SENT),
            Self::Drafts => Some(DRAFT),
            Self::Spam => Some(SPAM),
            Self::Trash => Some(TRASH),
            Self::Archive | Self::Label => None,
        }
    }
}

/// One row of a thread list: everything needed to render it without joins
/// (spec §13 rule 4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadSummary {
    pub id: ThreadId,
    pub subject: String,
    pub snippet: String,
    pub last_message_at: Millis,
    pub message_count: u32,
    pub unread_count: u32,
    pub has_attachments: bool,
    pub is_starred: bool,
    /// Distinct senders, oldest first, for the "Alice, Bob (3)" line.
    pub participants: Vec<EmailAddress>,
    pub label_ids: Vec<LabelId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BodyState {
    /// Headers and snippet only; body not fetched yet.
    Metadata,
    Full,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub id: AttachmentId,
    pub filename: String,
    pub mime_type: String,
    pub size: u64,
    /// For `cid:` references from HTML bodies.
    pub content_id: Option<String>,
    pub is_inline: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: MessageId,
    pub thread_id: ThreadId,
    /// RFC 5322 `Message-ID`, used for threading replies.
    pub rfc822_message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub from: Option<EmailAddress>,
    pub to: Vec<EmailAddress>,
    pub cc: Vec<EmailAddress>,
    pub bcc: Vec<EmailAddress>,
    pub reply_to: Vec<EmailAddress>,
    pub subject: String,
    /// The `Date` header, falling back to `internal_date`.
    pub date: Millis,
    /// When the provider received it; the sort key.
    pub internal_date: Millis,
    pub snippet: String,
    pub is_read: bool,
    pub is_starred: bool,
    pub is_draft: bool,
    pub is_sent_by_me: bool,
    pub label_ids: Vec<LabelId>,
    pub body_state: BodyState,
    pub size_estimate: u64,
    pub attachments: Vec<Attachment>,
}

/// A message body as stored after sync: plain text for search and agents,
/// sanitized HTML for display (spec §14.4).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Body {
    pub text_plain: Option<String>,
    pub html_sanitized: Option<String>,
    pub has_remote_images: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Draft {
    pub id: DraftId,
    pub thread_id: Option<ThreadId>,
    pub in_reply_to: Option<MessageId>,
    pub to: Vec<EmailAddress>,
    pub cc: Vec<EmailAddress>,
    pub bcc: Vec<EmailAddress>,
    pub subject: String,
    pub body_html: String,
    pub body_text: String,
    pub attachment_ids: Vec<AttachmentId>,
    pub updated_at: Millis,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_address_trims_and_normalizes() {
        let a = EmailAddress::new(Some("  "), " Alice@Example.COM ");
        assert_eq!(a.name, None);
        assert_eq!(a.email, "Alice@Example.COM");
        assert_eq!(a.normalized(), "alice@example.com");
        assert_eq!(a.display(), "Alice@Example.COM");
        assert_eq!(EmailAddress::new(Some("Alice"), "a@x").display(), "Alice");
    }

    #[test]
    fn protected_labels() {
        let label =
            |id: &str| Label { id: id.into(), name: id.into(), kind: LabelKind::System, color: None, visible: true };
        assert!(label("TRASH").is_protected());
        assert!(label("SPAM").is_protected());
        assert!(!label("INBOX").is_protected());
        assert!(!label("Label_3").is_protected());
    }

    #[test]
    fn mailbox_kinds_map_to_system_labels() {
        assert_eq!(MailboxKind::Inbox.system_label(), Some("INBOX"));
        assert_eq!(MailboxKind::Drafts.system_label(), Some("DRAFT"));
        assert_eq!(MailboxKind::Archive.system_label(), None);
    }

    #[test]
    fn ids_serialize_as_plain_strings() {
        let s = serde_json::to_string(&ThreadId::new("18c2f")).unwrap();
        assert_eq!(s, "\"18c2f\"");
    }
}
