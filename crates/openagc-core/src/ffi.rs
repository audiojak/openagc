//! Records Swift sees (spec §4.2). Mirrors of `mail_domain` types, flattened
//! for UniFFI: ids are strings (they are newtypes everywhere inside Rust),
//! timestamps are milliseconds since the epoch.

use mail_domain as d;

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AddressInfo {
    pub name: Option<String>,
    pub email: String,
}

impl From<d::EmailAddress> for AddressInfo {
    fn from(a: d::EmailAddress) -> Self {
        Self { name: a.name, email: a.email }
    }
}

impl From<AddressInfo> for d::EmailAddress {
    fn from(a: AddressInfo) -> Self {
        d::EmailAddress::new(a.name.as_deref(), &a.email)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum MailboxKind {
    Inbox,
    Starred,
    Important,
    Sent,
    Drafts,
    Archive,
    Spam,
    Trash,
    Label,
}

impl From<d::MailboxKind> for MailboxKind {
    fn from(k: d::MailboxKind) -> Self {
        match k {
            d::MailboxKind::Inbox => Self::Inbox,
            d::MailboxKind::Starred => Self::Starred,
            d::MailboxKind::Important => Self::Important,
            d::MailboxKind::Sent => Self::Sent,
            d::MailboxKind::Drafts => Self::Drafts,
            d::MailboxKind::Archive => Self::Archive,
            d::MailboxKind::Spam => Self::Spam,
            d::MailboxKind::Trash => Self::Trash,
            d::MailboxKind::Label => Self::Label,
        }
    }
}

impl From<MailboxKind> for d::MailboxKind {
    fn from(k: MailboxKind) -> Self {
        match k {
            MailboxKind::Inbox => Self::Inbox,
            MailboxKind::Starred => Self::Starred,
            MailboxKind::Important => Self::Important,
            MailboxKind::Sent => Self::Sent,
            MailboxKind::Drafts => Self::Drafts,
            MailboxKind::Archive => Self::Archive,
            MailboxKind::Spam => Self::Spam,
            MailboxKind::Trash => Self::Trash,
            MailboxKind::Label => Self::Label,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct MailboxInfo {
    /// What `list_threads` takes: the label id, or `@archive`.
    pub id: String,
    pub kind: MailboxKind,
    /// `None` only for Archive.
    pub label_id: Option<String>,
    pub name: String,
    pub unread_count: u32,
    pub total_count: u32,
}

impl From<d::Mailbox> for MailboxInfo {
    fn from(m: d::Mailbox) -> Self {
        Self {
            id: mail_store::read::mailbox_label(&m).to_owned(),
            kind: m.kind.into(),
            label_id: m.label_id.map(|l| l.0),
            name: m.name,
            unread_count: m.unread_count,
            total_count: m.total_count,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct LabelInfo {
    pub id: String,
    pub name: String,
    pub is_system: bool,
    pub background_color: Option<String>,
    pub text_color: Option<String>,
    pub visible: bool,
}

impl From<d::Label> for LabelInfo {
    fn from(l: d::Label) -> Self {
        let (background_color, text_color) = match l.color {
            Some(c) => (Some(c.background), Some(c.text)),
            None => (None, None),
        };
        Self {
            id: l.id.0,
            name: l.name,
            is_system: l.kind == d::LabelKind::System,
            background_color,
            text_color,
            visible: l.visible,
        }
    }
}

/// One thread-list row; renders without further lookups.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ThreadRow {
    pub id: String,
    pub subject: String,
    pub snippet: String,
    pub last_message_at: i64,
    pub message_count: u32,
    pub unread_count: u32,
    pub has_attachments: bool,
    pub is_starred: bool,
    pub participants: Vec<AddressInfo>,
    pub label_ids: Vec<String>,
}

impl From<d::ThreadSummary> for ThreadRow {
    fn from(t: d::ThreadSummary) -> Self {
        Self {
            id: t.id.0,
            subject: t.subject,
            snippet: t.snippet,
            last_message_at: t.last_message_at,
            message_count: t.message_count,
            unread_count: t.unread_count,
            has_attachments: t.has_attachments,
            is_starred: t.is_starred,
            participants: t.participants.into_iter().map(Into::into).collect(),
            label_ids: t.label_ids.into_iter().map(|l| l.0).collect(),
        }
    }
}

/// A page of rows plus an opaque keyset cursor for the next page.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ThreadPage {
    pub rows: Vec<ThreadRow>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AttachmentInfo {
    pub id: String,
    pub filename: String,
    pub mime_type: String,
    pub size: u64,
    pub content_id: Option<String>,
    pub is_inline: bool,
}

impl From<d::Attachment> for AttachmentInfo {
    fn from(a: d::Attachment) -> Self {
        Self {
            id: a.id.0,
            filename: a.filename,
            mime_type: a.mime_type,
            size: a.size,
            content_id: a.content_id,
            is_inline: a.is_inline,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct MessageInfo {
    pub id: String,
    pub thread_id: String,
    pub from: Option<AddressInfo>,
    pub to: Vec<AddressInfo>,
    pub cc: Vec<AddressInfo>,
    pub reply_to: Vec<AddressInfo>,
    pub subject: String,
    pub date: i64,
    pub snippet: String,
    pub is_read: bool,
    pub is_starred: bool,
    pub is_draft: bool,
    pub is_sent_by_me: bool,
    pub label_ids: Vec<String>,
    /// False until the body has been fetched by sync.
    pub has_body: bool,
    pub attachments: Vec<AttachmentInfo>,
}

impl From<d::Message> for MessageInfo {
    fn from(m: d::Message) -> Self {
        let addrs = |v: Vec<d::EmailAddress>| v.into_iter().map(Into::into).collect();
        Self {
            id: m.id.0,
            thread_id: m.thread_id.0,
            from: m.from.map(Into::into),
            to: addrs(m.to),
            cc: addrs(m.cc),
            reply_to: addrs(m.reply_to),
            subject: m.subject,
            date: m.date,
            snippet: m.snippet,
            is_read: m.is_read,
            is_starred: m.is_starred,
            is_draft: m.is_draft,
            is_sent_by_me: m.is_sent_by_me,
            label_ids: m.label_ids.into_iter().map(|l| l.0).collect(),
            has_body: m.body_state == d::BodyState::Full,
            attachments: m.attachments.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ThreadDetail {
    pub thread: ThreadRow,
    /// Oldest first.
    pub messages: Vec<MessageInfo>,
}

/// A message body ready for the web view (spec §14.4).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct RenderedBody {
    pub message_id: String,
    /// Sanitized HTML fragment; plain-text mail is converted, so this is
    /// `None` only for an empty body.
    pub html: Option<String>,
    pub text: Option<String>,
    pub has_remote_images: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_summary_converts_to_row() {
        let row: ThreadRow = d::ThreadSummary {
            id: "t1".into(),
            subject: "Hi".into(),
            snippet: "Hello".into(),
            last_message_at: 1_700_000_000_000,
            message_count: 2,
            unread_count: 1,
            has_attachments: true,
            is_starred: false,
            participants: vec![d::EmailAddress::new(Some("Ann"), "ann@example.com")],
            label_ids: vec!["INBOX".into()],
        }
        .into();
        assert_eq!(row.id, "t1");
        assert_eq!(row.participants, vec![AddressInfo { name: Some("Ann".into()), email: "ann@example.com".into() }]);
        assert_eq!(row.label_ids, vec!["INBOX".to_owned()]);
    }

    #[test]
    fn mailbox_kind_round_trips() {
        for k in [
            d::MailboxKind::Inbox,
            d::MailboxKind::Starred,
            d::MailboxKind::Important,
            d::MailboxKind::Sent,
            d::MailboxKind::Drafts,
            d::MailboxKind::Archive,
            d::MailboxKind::Spam,
            d::MailboxKind::Trash,
            d::MailboxKind::Label,
        ] {
            assert_eq!(d::MailboxKind::from(MailboxKind::from(k)), k);
        }
    }
}
