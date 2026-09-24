//! Read queries. All run on pooled read-only connections and never touch
//! the network (spec §13 rule 1).

use mail_domain::{
    Attachment, AttachmentId, Body, BodyState, EmailAddress, Label, LabelColor, LabelId, LabelKind, Mailbox,
    MailboxKind, Message, MessageId, ThreadId, ThreadSummary,
};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Deserialize;

use crate::error::{StoreError, StoreResult};
use crate::write::ARCHIVE_LABEL;

pub const DEFAULT_PAGE_SIZE: u32 = 100;
pub const MAX_PAGE_SIZE: u32 = 500;

/// A page of thread rows with an opaque cursor for the next one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadPage {
    pub rows: Vec<ThreadSummary>,
    pub next_cursor: Option<String>,
}

pub fn list_labels(conn: &Connection) -> StoreResult<Vec<Label>> {
    let mut stmt = conn.prepare_cached(
        "SELECT gmail_id, name, kind, color_bg, color_fg, visible FROM labels WHERE kind != 'virtual'
         ORDER BY kind DESC, name COLLATE NOCASE",
    )?;
    let rows = stmt.query_map([], |r| {
        let bg: Option<String> = r.get(3)?;
        let fg: Option<String> = r.get(4)?;
        Ok(Label {
            id: LabelId(r.get(0)?),
            name: r.get(1)?,
            kind: if r.get::<_, String>(2)? == "user" { LabelKind::User } else { LabelKind::System },
            color: bg.zip(fg).map(|(background, text)| LabelColor { background, text }),
            visible: r.get(5)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Sidebar entries: system mailboxes in a fixed order, then visible user
/// labels by name. Counts come from `label_stats`, never `COUNT(*)`.
pub fn list_mailboxes(conn: &Connection) -> StoreResult<Vec<Mailbox>> {
    const SYSTEM: &[(MailboxKind, &str, &str)] = &[
        (MailboxKind::Inbox, "INBOX", "Inbox"),
        (MailboxKind::Starred, "STARRED", "Starred"),
        (MailboxKind::Important, "IMPORTANT", "Important"),
        (MailboxKind::Sent, "SENT", "Sent"),
        (MailboxKind::Drafts, "DRAFT", "Drafts"),
        (MailboxKind::Archive, ARCHIVE_LABEL, "Archive"),
        (MailboxKind::Spam, "SPAM", "Spam"),
        (MailboxKind::Trash, "TRASH", "Trash"),
    ];
    let mut counts = conn.prepare_cached(
        "SELECT COALESCE(s.thread_count, 0), COALESCE(s.unread_thread_count, 0)
         FROM labels l LEFT JOIN label_stats s ON s.label_id = l.id WHERE l.gmail_id = ?1",
    )?;
    let mut out = Vec::new();
    for (kind, id, name) in SYSTEM {
        let (total, unread): (i64, i64) =
            counts.query_row([id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?.unwrap_or((0, 0));
        out.push(Mailbox {
            kind: *kind,
            label_id: (*kind != MailboxKind::Archive).then(|| LabelId::new(*id)),
            name: (*name).to_owned(),
            unread_count: unread.max(0) as u32,
            total_count: total.max(0) as u32,
        });
    }
    let mut user = conn.prepare_cached(
        "SELECT l.gmail_id, l.name, COALESCE(s.thread_count, 0), COALESCE(s.unread_thread_count, 0)
         FROM labels l LEFT JOIN label_stats s ON s.label_id = l.id
         WHERE l.kind = 'user' AND l.visible ORDER BY l.name COLLATE NOCASE",
    )?;
    let rows = user.query_map([], |r| {
        Ok(Mailbox {
            kind: MailboxKind::Label,
            label_id: Some(LabelId(r.get(0)?)),
            name: r.get(1)?,
            total_count: r.get::<_, i64>(2)?.max(0) as u32,
            unread_count: r.get::<_, i64>(3)?.max(0) as u32,
        })
    })?;
    for m in rows {
        out.push(m?);
    }
    Ok(out)
}

/// The mailbox id used by thread queries: a label id, or `@archive`.
pub fn mailbox_label(mailbox: &Mailbox) -> &str {
    match &mailbox.label_id {
        Some(l) => l.as_str(),
        None => ARCHIVE_LABEL,
    }
}

/// Threads in a mailbox, newest first, keyset-paged: cost is O(page)
/// however deep the user scrolls (spec §4.2).
pub fn list_threads(conn: &Connection, label: &str, cursor: Option<&str>, limit: u32) -> StoreResult<ThreadPage> {
    let limit = limit.clamp(1, MAX_PAGE_SIZE);
    let (after_at, after_id) = match cursor {
        Some(c) => decode_cursor(c)?,
        None => (i64::MAX, i64::MAX),
    };
    let mut stmt = conn.prepare_cached(
        "SELECT t.id, t.gmail_id, t.subject, t.snippet, t.last_message_at, t.message_count, t.unread_count,
                t.has_attachments, t.is_starred, t.participants_json, t.label_ids_json, tl.last_message_at
         FROM thread_labels tl JOIN threads t ON t.id = tl.thread_id
         WHERE tl.label_id = (SELECT id FROM labels WHERE gmail_id = ?1)
           AND (tl.last_message_at, tl.thread_id) < (?2, ?3)
         ORDER BY tl.last_message_at DESC, tl.thread_id DESC
         LIMIT ?4",
    )?;
    let mut last_key = None;
    let rows = stmt
        .query_map(params![label, after_at, after_id, limit + 1], |r| {
            let key = (r.get::<_, i64>(11)?, r.get::<_, i64>(0)?);
            Ok((key, thread_summary(r, 1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let has_more = rows.len() > limit as usize;
    let mut out = Vec::with_capacity(limit as usize);
    for (key, row) in rows.into_iter().take(limit as usize) {
        last_key = Some(key);
        out.push(row?);
    }
    let next_cursor = if has_more { last_key.map(|(at, id)| encode_cursor(at, id)) } else { None };
    Ok(ThreadPage { rows: out, next_cursor })
}

pub fn get_thread_summary(conn: &Connection, id: &ThreadId) -> StoreResult<Option<ThreadSummary>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, gmail_id, subject, snippet, last_message_at, message_count, unread_count, has_attachments,
                is_starred, participants_json, label_ids_json
         FROM threads WHERE gmail_id = ?1",
    )?;
    match stmt.query_row([id.as_str()], |r| thread_summary(r, 1)).optional()? {
        Some(summary) => Ok(Some(summary?)),
        None => Ok(None),
    }
}

/// A thread with its messages, oldest first.
pub fn get_thread(conn: &Connection, id: &ThreadId) -> StoreResult<Option<(ThreadSummary, Vec<Message>)>> {
    let Some(summary) = get_thread_summary(conn, id)? else { return Ok(None) };
    let mut stmt = conn.prepare_cached(
        "SELECT m.id, m.gmail_id, m.rfc822_message_id, m.in_reply_to, m.references_json, m.subject, m.date,
                m.internal_date, m.snippet, m.is_read, m.is_starred, m.is_draft, m.is_sent_by_me, m.body_state,
                m.size_estimate
         FROM messages m JOIN threads t ON t.id = m.thread_id WHERE t.gmail_id = ?1
         ORDER BY m.internal_date, m.id",
    )?;
    let rows = stmt
        .query_map([id.as_str()], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                Message {
                    id: MessageId(r.get(1)?),
                    thread_id: id.clone(),
                    rfc822_message_id: r.get(2)?,
                    in_reply_to: r.get(3)?,
                    references: serde_json::from_str(&r.get::<_, String>(4)?).unwrap_or_default(),
                    from: None,
                    to: vec![],
                    cc: vec![],
                    bcc: vec![],
                    reply_to: vec![],
                    subject: r.get(5)?,
                    date: r.get(6)?,
                    internal_date: r.get(7)?,
                    snippet: r.get(8)?,
                    is_read: r.get(9)?,
                    is_starred: r.get(10)?,
                    is_draft: r.get(11)?,
                    is_sent_by_me: r.get(12)?,
                    label_ids: vec![],
                    body_state: if r.get::<_, String>(13)? == "full" { BodyState::Full } else { BodyState::Metadata },
                    size_estimate: r.get::<_, i64>(14)?.max(0) as u64,
                    attachments: vec![],
                },
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut messages = Vec::with_capacity(rows.len());
    for (rowid, mut m) in rows {
        fill_participants(conn, rowid, &mut m)?;
        m.label_ids = conn
            .prepare_cached(
                "SELECT l.gmail_id FROM message_labels ml JOIN labels l ON l.id = ml.label_id
                 WHERE ml.message_id = ?1 ORDER BY l.gmail_id",
            )?
            .query_map([rowid], |r| Ok(LabelId(r.get(0)?)))?
            .collect::<Result<_, _>>()?;
        m.attachments = conn
            .prepare_cached(
                "SELECT id, filename, mime_type, size, content_id, is_inline FROM attachments
                 WHERE message_id = ?1 ORDER BY id",
            )?
            .query_map([rowid], |r| {
                Ok(Attachment {
                    id: AttachmentId(r.get::<_, i64>(0)?.to_string()),
                    filename: r.get(1)?,
                    mime_type: r.get(2)?,
                    size: r.get::<_, i64>(3)?.max(0) as u64,
                    content_id: r.get(4)?,
                    is_inline: r.get(5)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        messages.push(m);
    }
    Ok(Some((summary, messages)))
}

/// One message with its thread id, by provider message id.
pub fn get_message(conn: &Connection, id: &MessageId) -> StoreResult<Option<Message>> {
    let thread: Option<String> = conn
        .prepare_cached("SELECT t.gmail_id FROM messages m JOIN threads t ON t.id = m.thread_id WHERE m.gmail_id = ?1")?
        .query_row([id.as_str()], |r| r.get(0))
        .optional()?;
    let Some(thread) = thread else { return Ok(None) };
    Ok(get_thread(conn, &ThreadId(thread))?.and_then(|(_, messages)| messages.into_iter().find(|m| &m.id == id)))
}

pub fn get_body(conn: &Connection, id: &MessageId) -> StoreResult<Option<Body>> {
    Ok(conn
        .prepare_cached(
            "SELECT b.text_plain, b.html_sanitized, b.has_remote_images
             FROM bodies b JOIN messages m ON m.id = b.message_id WHERE m.gmail_id = ?1",
        )?
        .query_row([id.as_str()], |r| {
            Ok(Body { text_plain: r.get(0)?, html_sanitized: r.get(1)?, has_remote_images: r.get(2)? })
        })
        .optional()?)
}

/// Recipient suggestions for the composer: people the user writes to most
/// and most recently first (spec §14.5). Three or more characters match
/// anywhere in a name or address (trigram index); fewer match a prefix.
pub fn suggest_contacts(conn: &Connection, text: &str, limit: u32) -> StoreResult<Vec<EmailAddress>> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(vec![]);
    }
    let order = "ORDER BY (c.sent_count * 3 + c.received_count) DESC, c.last_seen DESC LIMIT ?2";
    let rows = if text.chars().count() >= 3 {
        let quoted = format!("\"{}\"", text.replace('"', ""));
        conn.prepare_cached(&format!(
            "SELECT c.name, c.email FROM contacts_fts f JOIN contacts c ON c.id = f.rowid
             WHERE contacts_fts MATCH ?1 {order}"
        ))?
        .query_map(params![quoted, limit], |r| Ok(EmailAddress { name: r.get(0)?, email: r.get(1)? }))?
        .collect::<Result<Vec<_>, _>>()?
    } else {
        let like = format!("{}%", text.replace(['%', '_'], ""));
        conn.prepare_cached(&format!(
            "SELECT c.name, c.email FROM contacts c WHERE c.email LIKE ?1 OR c.name LIKE ?1 {order}"
        ))?
        .query_map(params![like, limit], |r| Ok(EmailAddress { name: r.get(0)?, email: r.get(1)? }))?
        .collect::<Result<Vec<_>, _>>()?
    };
    Ok(rows)
}

pub fn sync_state(conn: &Connection, key: &str) -> StoreResult<Option<String>> {
    Ok(conn.prepare_cached("SELECT value FROM sync_state WHERE key = ?1")?.query_row([key], |r| r.get(0)).optional()?)
}

pub fn set_sync_state(tx: &rusqlite::Transaction<'_>, key: &str, value: &str) -> StoreResult<()> {
    tx.prepare_cached(
        "INSERT INTO sync_state (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
    )?
    .execute([key, value])?;
    Ok(())
}

// --- helpers --------------------------------------------------------------

#[derive(Deserialize)]
struct ParticipantJson {
    name: Option<String>,
    email: String,
}

/// A summary from a row shaped `id, gmail_id, subject, … label_ids_json`.
pub(crate) fn thread_summary_row(r: &Row<'_>) -> rusqlite::Result<StoreResult<ThreadSummary>> {
    thread_summary(r, 1)
}

/// Build a summary from columns starting at `offset` (gmail_id first).
fn thread_summary(r: &Row<'_>, offset: usize) -> rusqlite::Result<StoreResult<ThreadSummary>> {
    let participants: String = r.get(offset + 8)?;
    let labels: String = r.get(offset + 9)?;
    let parse = || -> StoreResult<ThreadSummary> {
        let participants: Vec<ParticipantJson> = serde_json::from_str(&participants)?;
        let labels: Vec<String> = serde_json::from_str(&labels)?;
        Ok(ThreadSummary {
            id: ThreadId(r.get(offset)?),
            subject: r.get(offset + 1)?,
            snippet: r.get(offset + 2)?,
            last_message_at: r.get(offset + 3)?,
            message_count: r.get::<_, i64>(offset + 4)?.max(0) as u32,
            unread_count: r.get::<_, i64>(offset + 5)?.max(0) as u32,
            has_attachments: r.get(offset + 6)?,
            is_starred: r.get(offset + 7)?,
            participants: participants.into_iter().map(|p| EmailAddress { name: p.name, email: p.email }).collect(),
            label_ids: labels.into_iter().map(LabelId).collect(),
        })
    };
    Ok(parse())
}

fn fill_participants(conn: &Connection, message_rowid: i64, m: &mut Message) -> StoreResult<()> {
    let mut stmt = conn
        .prepare_cached("SELECT role, name, email FROM participants WHERE message_id = ?1 ORDER BY role, position")?;
    let rows = stmt.query_map([message_rowid], |r| {
        Ok((r.get::<_, String>(0)?, EmailAddress { name: r.get(1)?, email: r.get(2)? }))
    })?;
    for row in rows {
        let (role, addr) = row?;
        match role.as_str() {
            "from" => m.from = Some(addr),
            "to" => m.to.push(addr),
            "cc" => m.cc.push(addr),
            "bcc" => m.bcc.push(addr),
            "reply_to" => m.reply_to.push(addr),
            _ => {}
        }
    }
    Ok(())
}

pub(crate) fn encode_cursor(at: i64, id: i64) -> String {
    format!("{at}:{id}")
}

pub(crate) fn decode_cursor(c: &str) -> StoreResult<(i64, i64)> {
    let (a, b) = c.split_once(':').ok_or_else(|| StoreError::Invalid(format!("bad cursor {c:?}")))?;
    let parse = |s: &str| s.parse::<i64>().map_err(|_| StoreError::Invalid(format!("bad cursor {c:?}")));
    Ok((parse(a)?, parse(b)?))
}
