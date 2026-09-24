//! The mail write API. Every change to messages or labels goes through a
//! [`MailWriter`], which records the threads it touched and, in
//! [`MailWriter::finish`], recomputes their aggregates, `thread_labels`
//! rows and `label_stats` in the same transaction. The returned
//! [`ThreadChanges`] says, per mailbox, which threads appeared, changed or
//! disappeared, which is exactly what the UI's change events need.

use std::collections::{BTreeMap, BTreeSet};

use mail_domain::{Body, EmailAddress, Label, LabelId, LabelKind, MessageId, Millis, ThreadId, system_labels};
use rusqlite::{OptionalExtension, Transaction, params};
use serde::Serialize;

use crate::error::{StoreError, StoreResult};

/// Local id of the virtual Archive label (see the migration).
pub const ARCHIVE_LABEL: &str = "@archive";

/// Id prefix of optimistic copies of sent mail, replaced when the provider's
/// copy syncs back.
pub const LOCAL_PREFIX: &str = "local-";

/// Maximum distinct senders kept per thread for the list row.
const MAX_THREAD_PARTICIPANTS: usize = 10;

/// A message as a provider delivers it. Flags are derived from labels
/// (Gmail semantics: no `UNREAD` label means read, and so on).
#[derive(Debug, Clone, Default)]
pub struct IncomingMessage {
    pub id: MessageId,
    pub thread_id: ThreadId,
    pub rfc822_message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub from: Option<EmailAddress>,
    pub to: Vec<EmailAddress>,
    pub cc: Vec<EmailAddress>,
    pub bcc: Vec<EmailAddress>,
    pub reply_to: Vec<EmailAddress>,
    pub subject: String,
    pub date: Millis,
    pub internal_date: Millis,
    pub snippet: String,
    pub label_ids: Vec<LabelId>,
    pub size_estimate: u64,
    /// `None` for a metadata-only fetch; an existing body is then kept.
    pub body: Option<Body>,
    /// Replaced only when `body` is `Some` (a full fetch).
    pub attachments: Vec<IncomingAttachment>,
    pub headers_json: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct IncomingAttachment {
    pub part_id: Option<String>,
    pub provider_attachment_id: Option<String>,
    pub filename: String,
    pub mime_type: String,
    pub size: u64,
    pub content_id: Option<String>,
    pub is_inline: bool,
}

/// Per mailbox (label id, including `@archive`): thread ids that entered,
/// changed within, or left it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ThreadChanges {
    pub mailboxes: BTreeMap<String, MailboxChange>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct MailboxChange {
    pub inserted: BTreeSet<String>,
    pub updated: BTreeSet<String>,
    pub removed: BTreeSet<String>,
}

impl ThreadChanges {
    pub fn is_empty(&self) -> bool {
        self.mailboxes.is_empty()
    }
}

pub struct MailWriter<'t> {
    tx: &'t Transaction<'t>,
    /// Thread rowid → provider thread id, for everything touched.
    dirty: BTreeMap<i64, String>,
}

impl<'t> MailWriter<'t> {
    pub fn new(tx: &'t Transaction<'t>) -> Self {
        Self { tx, dirty: BTreeMap::new() }
    }

    /// Insert or update labels by provider id. Does not delete missing ones;
    /// see [`MailWriter::retain_labels`].
    pub fn upsert_labels(&mut self, labels: &[Label]) -> StoreResult<()> {
        let mut stmt = self.tx.prepare_cached(
            "INSERT INTO labels (gmail_id, name, kind, color_bg, color_fg, visible)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (gmail_id) DO UPDATE SET
               name = excluded.name, kind = excluded.kind, color_bg = excluded.color_bg,
               color_fg = excluded.color_fg, visible = excluded.visible",
        )?;
        for l in labels {
            let kind = match l.kind {
                LabelKind::System => "system",
                LabelKind::User => "user",
            };
            let (bg, fg) = l.color.as_ref().map(|c| (Some(&c.background), Some(&c.text))).unwrap_or((None, None));
            stmt.execute(params![l.id.as_str(), l.name, kind, bg, fg, l.visible])?;
        }
        Ok(())
    }

    /// Delete provider labels not in `keep` (a full label refresh). Threads
    /// that carried them are recomputed.
    pub fn retain_labels(&mut self, keep: &[LabelId]) -> StoreResult<()> {
        let keep: BTreeSet<&str> = keep.iter().map(LabelId::as_str).collect();
        let existing: Vec<(i64, String)> = self
            .tx
            .prepare_cached("SELECT id, gmail_id FROM labels WHERE kind != 'virtual'")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        for (rowid, gmail_id) in existing {
            if keep.contains(gmail_id.as_str()) {
                continue;
            }
            self.mark_threads_with_label(rowid)?;
            self.tx.execute("DELETE FROM labels WHERE id = ?1", [rowid])?;
        }
        Ok(())
    }

    /// Insert or replace a message and everything hanging off it.
    pub fn upsert_message(&mut self, m: &IncomingMessage) -> StoreResult<()> {
        let thread_rowid = self.ensure_thread(&m.thread_id)?;
        let has = |id: &str| m.label_ids.iter().any(|l| l.as_str() == id);
        let is_read = !has(system_labels::UNREAD);
        let is_starred = has(system_labels::STARRED);
        let is_draft = has(system_labels::DRAFT);
        let is_sent_by_me = has(system_labels::SENT);
        let has_attachments = m.attachments.iter().any(|a| !a.is_inline);

        let existing: Option<(i64, i64, String)> = self
            .tx
            .prepare_cached("SELECT id, thread_id, body_state FROM messages WHERE gmail_id = ?1")?
            .query_row([m.id.as_str()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()?;
        let body_state = match (&m.body, &existing) {
            (Some(_), _) => "full",
            (None, Some((_, _, state))) => state.as_str(),
            (None, None) => "metadata",
        }
        .to_owned();
        let from = m.from.as_ref();

        let message_rowid = match existing {
            Some((rowid, old_thread, _)) => {
                if old_thread != thread_rowid {
                    self.mark_thread_rowid(old_thread)?;
                }
                self.tx
                    .prepare_cached(
                        "UPDATE messages SET thread_id = ?2, rfc822_message_id = ?3, in_reply_to = ?4,
                           references_json = ?5, from_name = ?6, from_email = ?7, subject = ?8, snippet = ?9,
                           date = ?10, internal_date = ?11, size_estimate = ?12, body_state = ?13,
                           is_read = ?14, is_starred = ?15, is_draft = ?16, is_sent_by_me = ?17,
                           has_attachments = CASE WHEN ?18 THEN ?19 ELSE has_attachments END,
                           headers_json = COALESCE(?20, headers_json)
                         WHERE id = ?1",
                    )?
                    .execute(params![
                        rowid,
                        thread_rowid,
                        m.rfc822_message_id,
                        m.in_reply_to,
                        serde_json::to_string(&m.references)?,
                        from.and_then(|f| f.name.as_deref()),
                        from.map(|f| f.email.as_str()),
                        m.subject,
                        m.snippet,
                        m.date,
                        m.internal_date,
                        m.size_estimate as i64,
                        body_state,
                        is_read,
                        is_starred,
                        is_draft,
                        is_sent_by_me,
                        m.body.is_some(),
                        has_attachments,
                        m.headers_json,
                    ])?;
                rowid
            }
            None => {
                self.tx
                    .prepare_cached(
                        "INSERT INTO messages (thread_id, gmail_id, rfc822_message_id, in_reply_to, references_json,
                           from_name, from_email, subject, snippet, date, internal_date, size_estimate, body_state,
                           is_read, is_starred, is_draft, is_sent_by_me, has_attachments, headers_json)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
                    )?
                    .execute(params![
                        thread_rowid,
                        m.id.as_str(),
                        m.rfc822_message_id,
                        m.in_reply_to,
                        serde_json::to_string(&m.references)?,
                        from.and_then(|f| f.name.as_deref()),
                        from.map(|f| f.email.as_str()),
                        m.subject,
                        m.snippet,
                        m.date,
                        m.internal_date,
                        m.size_estimate as i64,
                        body_state,
                        is_read,
                        is_starred,
                        is_draft,
                        is_sent_by_me,
                        has_attachments,
                        m.headers_json,
                    ])?;
                let rowid = self.tx.last_insert_rowid();
                self.record_contacts(m, is_sent_by_me)?;
                self.replace_local_copies(m)?;
                rowid
            }
        };

        self.replace_participants(message_rowid, m)?;
        self.replace_message_labels(message_rowid, &m.label_ids)?;
        if let Some(body) = &m.body {
            self.tx
                .prepare_cached(
                    "INSERT INTO bodies (message_id, text_plain, html_sanitized, has_remote_images)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT (message_id) DO UPDATE SET text_plain = excluded.text_plain,
                       html_sanitized = excluded.html_sanitized, has_remote_images = excluded.has_remote_images",
                )?
                .execute(params![message_rowid, body.text_plain, body.html_sanitized, body.has_remote_images])?;
            self.replace_attachments(message_rowid, &m.attachments)?;
        }
        self.index_message(message_rowid)?;
        self.dirty.insert(thread_rowid, m.thread_id.0.clone());
        Ok(())
    }

    /// A sent message arriving from the provider replaces the optimistic
    /// local copy made when it was sent (same RFC 5322 Message-ID).
    fn replace_local_copies(&mut self, m: &IncomingMessage) -> StoreResult<()> {
        let Some(rfc) = &m.rfc822_message_id else { return Ok(()) };
        if m.id.as_str().starts_with(LOCAL_PREFIX) {
            return Ok(());
        }
        let stale: Vec<String> = self
            .tx
            .prepare_cached("SELECT gmail_id FROM messages WHERE rfc822_message_id = ?1 AND gmail_id LIKE 'local-%'")?
            .query_map([rfc], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        for id in stale {
            self.delete_message(&MessageId(id))?;
        }
        Ok(())
    }

    /// Apply label additions/removals to a message (history sync, outbox).
    /// Unknown messages are ignored and reported as `false`.
    pub fn modify_message_labels(&mut self, id: &MessageId, add: &[LabelId], remove: &[LabelId]) -> StoreResult<bool> {
        let Some((rowid, thread_rowid)) = self.message_rowid(id)? else { return Ok(false) };
        let mut labels: BTreeSet<String> = self.message_label_ids(rowid)?.into_iter().collect();
        for l in remove {
            labels.remove(l.as_str());
        }
        for l in add {
            labels.insert(l.0.clone());
        }
        let labels: Vec<LabelId> = labels.into_iter().map(LabelId).collect();
        self.replace_message_labels(rowid, &labels)?;
        let has = |id: &str| labels.iter().any(|l| l.as_str() == id);
        self.tx.execute(
            "UPDATE messages SET is_read = ?2, is_starred = ?3, is_draft = ?4, is_sent_by_me = ?5 WHERE id = ?1",
            params![
                rowid,
                !has(system_labels::UNREAD),
                has(system_labels::STARRED),
                has(system_labels::DRAFT),
                has(system_labels::SENT)
            ],
        )?;
        self.mark_thread_rowid(thread_rowid)?;
        Ok(true)
    }

    /// Delete a message; its thread is recomputed or, if now empty, removed.
    pub fn delete_message(&mut self, id: &MessageId) -> StoreResult<bool> {
        let Some((rowid, thread_rowid)) = self.message_rowid(id)? else { return Ok(false) };
        self.tx.execute("DELETE FROM messages_fts WHERE rowid = ?1", [rowid])?;
        self.tx.execute("DELETE FROM messages WHERE id = ?1", [rowid])?;
        self.mark_thread_rowid(thread_rowid)?;
        Ok(true)
    }

    /// Recompute everything for the touched threads and report the changes.
    pub fn finish(self) -> StoreResult<ThreadChanges> {
        let mut changes = ThreadChanges::default();
        for (thread_rowid, gmail_id) in &self.dirty {
            recompute_thread(self.tx, *thread_rowid, gmail_id, &mut changes)?;
        }
        Ok(changes)
    }

    // --- internals -------------------------------------------------------

    fn ensure_thread(&mut self, id: &ThreadId) -> StoreResult<i64> {
        let rowid: Option<i64> = self
            .tx
            .prepare_cached("SELECT id FROM threads WHERE gmail_id = ?1")?
            .query_row([id.as_str()], |r| r.get(0))
            .optional()?;
        match rowid {
            Some(r) => Ok(r),
            None => {
                self.tx.prepare_cached("INSERT INTO threads (gmail_id) VALUES (?1)")?.execute([id.as_str()])?;
                Ok(self.tx.last_insert_rowid())
            }
        }
    }

    fn message_rowid(&self, id: &MessageId) -> StoreResult<Option<(i64, i64)>> {
        Ok(self
            .tx
            .prepare_cached("SELECT id, thread_id FROM messages WHERE gmail_id = ?1")?
            .query_row([id.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?)
    }

    fn message_label_ids(&self, message_rowid: i64) -> StoreResult<Vec<String>> {
        Ok(self
            .tx
            .prepare_cached(
                "SELECT l.gmail_id FROM message_labels ml JOIN labels l ON l.id = ml.label_id WHERE ml.message_id = ?1",
            )?
            .query_map([message_rowid], |r| r.get(0))?
            .collect::<Result<_, _>>()?)
    }

    fn mark_thread_rowid(&mut self, thread_rowid: i64) -> StoreResult<()> {
        if !self.dirty.contains_key(&thread_rowid) {
            let gmail_id: Option<String> = self
                .tx
                .prepare_cached("SELECT gmail_id FROM threads WHERE id = ?1")?
                .query_row([thread_rowid], |r| r.get(0))
                .optional()?;
            if let Some(g) = gmail_id {
                self.dirty.insert(thread_rowid, g);
            }
        }
        Ok(())
    }

    fn mark_threads_with_label(&mut self, label_rowid: i64) -> StoreResult<()> {
        let threads: Vec<i64> = self
            .tx
            .prepare_cached("SELECT thread_id FROM thread_labels WHERE label_id = ?1")?
            .query_map([label_rowid], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        for t in threads {
            self.mark_thread_rowid(t)?;
        }
        Ok(())
    }

    /// Label rowid for a provider id, creating a hidden placeholder if the
    /// provider referenced a label we have not listed yet.
    fn label_rowid(&self, id: &str) -> StoreResult<i64> {
        let found: Option<i64> = self
            .tx
            .prepare_cached("SELECT id FROM labels WHERE gmail_id = ?1")?
            .query_row([id], |r| r.get(0))
            .optional()?;
        if let Some(r) = found {
            return Ok(r);
        }
        let kind = if id.chars().all(|c| c.is_ascii_uppercase() || c == '_') { "system" } else { "user" };
        self.tx
            .prepare_cached("INSERT INTO labels (gmail_id, name, kind, visible) VALUES (?1, ?1, ?2, 0)")?
            .execute(params![id, kind])?;
        Ok(self.tx.last_insert_rowid())
    }

    fn replace_message_labels(&self, message_rowid: i64, labels: &[LabelId]) -> StoreResult<()> {
        self.tx.prepare_cached("DELETE FROM message_labels WHERE message_id = ?1")?.execute([message_rowid])?;
        let mut insert =
            self.tx.prepare_cached("INSERT OR IGNORE INTO message_labels (message_id, label_id) VALUES (?1, ?2)")?;
        for l in labels {
            if l.as_str() == ARCHIVE_LABEL {
                return Err(StoreError::Invalid("@archive is virtual and cannot be applied".into()));
            }
            insert.execute(params![message_rowid, self.label_rowid(l.as_str())?])?;
        }
        Ok(())
    }

    fn replace_participants(&self, message_rowid: i64, m: &IncomingMessage) -> StoreResult<()> {
        self.tx.prepare_cached("DELETE FROM participants WHERE message_id = ?1")?.execute([message_rowid])?;
        let mut insert = self.tx.prepare_cached(
            "INSERT INTO participants (message_id, role, position, name, email) VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        let from: Vec<EmailAddress> = m.from.iter().cloned().collect();
        for (role, list) in [("from", &from), ("to", &m.to), ("cc", &m.cc), ("bcc", &m.bcc), ("reply_to", &m.reply_to)]
        {
            for (i, a) in list.iter().enumerate() {
                insert.execute(params![message_rowid, role, i as i64, a.name, a.email])?;
            }
        }
        Ok(())
    }

    fn replace_attachments(&self, message_rowid: i64, attachments: &[IncomingAttachment]) -> StoreResult<()> {
        self.tx.prepare_cached("DELETE FROM attachments WHERE message_id = ?1")?.execute([message_rowid])?;
        let mut insert = self.tx.prepare_cached(
            "INSERT INTO attachments (message_id, part_id, gmail_attachment_id, filename, mime_type, size,
               content_id, is_inline) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?;
        for a in attachments {
            insert.execute(params![
                message_rowid,
                a.part_id,
                a.provider_attachment_id,
                a.filename,
                if a.mime_type.is_empty() { "application/octet-stream" } else { &a.mime_type },
                a.size as i64,
                a.content_id,
                a.is_inline
            ])?;
        }
        Ok(())
    }

    /// Contacts count each message once, on first insert.
    fn record_contacts(&self, m: &IncomingMessage, sent_by_me: bool) -> StoreResult<()> {
        let mut upsert = self.tx.prepare_cached(
            "INSERT INTO contacts (email, name, sent_count, received_count, last_seen) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (email) DO UPDATE SET
               name = COALESCE(excluded.name, contacts.name),
               sent_count = contacts.sent_count + excluded.sent_count,
               received_count = contacts.received_count + excluded.received_count,
               last_seen = MAX(contacts.last_seen, excluded.last_seen)",
        )?;
        if sent_by_me {
            for a in m.to.iter().chain(&m.cc).chain(&m.bcc) {
                upsert.execute(params![a.email, a.name, 1, 0, m.internal_date])?;
            }
        } else if let Some(a) = &m.from {
            upsert.execute(params![a.email, a.name, 0, 1, m.internal_date])?;
        }
        Ok(())
    }

    /// (Re)index a message in `messages_fts` from the current rows.
    fn index_message(&self, message_rowid: i64) -> StoreResult<()> {
        self.tx.prepare_cached("DELETE FROM messages_fts WHERE rowid = ?1")?.execute([message_rowid])?;
        self.tx
            .prepare_cached(
                "INSERT INTO messages_fts (rowid, subject, from_text, to_text, body, attachment_names)
                 SELECT m.id, m.subject,
                   TRIM(COALESCE(m.from_name, '') || ' ' || COALESCE(m.from_email, '')),
                   (SELECT group_concat(TRIM(COALESCE(p.name, '') || ' ' || p.email), ' ')
                      FROM participants p WHERE p.message_id = m.id AND p.role IN ('to', 'cc', 'bcc')),
                   COALESCE(b.text_plain, m.snippet),
                   (SELECT group_concat(a.filename, ' ') FROM attachments a WHERE a.message_id = m.id)
                 FROM messages m LEFT JOIN bodies b ON b.message_id = m.id WHERE m.id = ?1",
            )?
            .execute([message_rowid])?;
        Ok(())
    }
}

#[derive(Debug, Default, Clone, Serialize)]
struct ParticipantJson {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    email: String,
}

struct MessageRow {
    internal_date: Millis,
    subject: String,
    snippet: String,
    from: Option<EmailAddress>,
    is_read: bool,
    is_starred: bool,
    is_draft: bool,
    has_attachments: bool,
}

/// Recompute one thread's aggregates, `thread_labels` and `label_stats`, and
/// add its per-mailbox transitions to `changes`.
fn recompute_thread(
    tx: &Transaction<'_>,
    thread_rowid: i64,
    gmail_id: &str,
    changes: &mut ThreadChanges,
) -> StoreResult<()> {
    // Old state: mailboxes and whether it was unread.
    let old_labels: BTreeMap<i64, String> = tx
        .prepare_cached(
            "SELECT tl.label_id, l.gmail_id FROM thread_labels tl JOIN labels l ON l.id = tl.label_id
             WHERE tl.thread_id = ?1",
        )?
        .query_map([thread_rowid], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    let old_unread: bool = tx
        .prepare_cached("SELECT unread_count > 0 FROM threads WHERE id = ?1")?
        .query_row([thread_rowid], |r| r.get(0))
        .optional()?
        .unwrap_or(false);

    let messages: Vec<MessageRow> = tx
        .prepare_cached(
            "SELECT internal_date, subject, snippet, from_name, from_email, is_read, is_starred, is_draft,
                    has_attachments
             FROM messages WHERE thread_id = ?1 ORDER BY internal_date, id",
        )?
        .query_map([thread_rowid], |r| {
            let email: Option<String> = r.get(4)?;
            Ok(MessageRow {
                internal_date: r.get(0)?,
                subject: r.get(1)?,
                snippet: r.get(2)?,
                from: email.map(|e| EmailAddress::new(r.get::<_, Option<String>>(3).ok().flatten().as_deref(), &e)),
                is_read: r.get(5)?,
                is_starred: r.get(6)?,
                is_draft: r.get(7)?,
                has_attachments: r.get(8)?,
            })
        })?
        .collect::<Result<_, _>>()?;

    for label_rowid in old_labels.keys() {
        remove_thread_label(tx, *label_rowid, thread_rowid, old_unread)?;
    }

    if messages.is_empty() {
        tx.execute("DELETE FROM threads WHERE id = ?1", [thread_rowid])?;
        for label in old_labels.values() {
            changes.mailboxes.entry(label.clone()).or_default().removed.insert(gmail_id.to_owned());
        }
        return Ok(());
    }

    // Drafts only count when the thread has nothing else.
    let non_draft: Vec<&MessageRow> = messages.iter().filter(|m| !m.is_draft).collect();
    let counted: Vec<&MessageRow> = if non_draft.is_empty() { messages.iter().collect() } else { non_draft };
    let first = counted.first().copied().unwrap_or(&messages[0]);
    let last = counted.last().copied().unwrap_or(&messages[messages.len() - 1]);
    let unread = messages.iter().filter(|m| !m.is_read).count() as i64;
    let has_attachments = messages.iter().any(|m| m.has_attachments);
    let is_starred = messages.iter().any(|m| m.is_starred);

    let mut seen = BTreeSet::new();
    let mut participants: Vec<ParticipantJson> = Vec::new();
    for f in messages.iter().filter_map(|m| m.from.as_ref()) {
        if seen.insert(f.normalized()) && participants.len() < MAX_THREAD_PARTICIPANTS {
            participants.push(ParticipantJson { name: f.name.clone(), email: f.email.clone() });
        }
    }

    // Labels on any message, excluding virtual ones.
    let labels: Vec<(i64, String)> = tx
        .prepare_cached(
            "SELECT DISTINCT l.id, l.gmail_id FROM message_labels ml
             JOIN messages m ON m.id = ml.message_id JOIN labels l ON l.id = ml.label_id
             WHERE m.thread_id = ?1 AND l.kind != 'virtual' ORDER BY l.gmail_id",
        )?
        .query_map([thread_rowid], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    let label_ids: Vec<&str> = labels.iter().map(|(_, g)| g.as_str()).collect();

    tx.prepare_cached(
        "UPDATE threads SET subject = ?2, snippet = ?3, first_message_at = ?4, last_message_at = ?5,
           message_count = ?6, unread_count = ?7, has_attachments = ?8, is_starred = ?9,
           participants_json = ?10, label_ids_json = ?11
         WHERE id = ?1",
    )?
    .execute(params![
        thread_rowid,
        first.subject,
        last.snippet,
        first.internal_date,
        last.internal_date,
        counted.len() as i64,
        unread,
        has_attachments,
        is_starred,
        serde_json::to_string(&participants)?,
        serde_json::to_string(&label_ids)?,
    ])?;

    // New mailboxes: every label, plus Archive when not in the inbox and not
    // wholly spam/trash.
    let mut new_labels: BTreeMap<i64, String> = labels.iter().cloned().collect();
    let in_inbox = label_ids.contains(&system_labels::INBOX);
    if !in_inbox && !wholly_spam_or_trash(tx, thread_rowid)? {
        let archive: i64 =
            tx.prepare_cached("SELECT id FROM labels WHERE gmail_id = ?1")?.query_row([ARCHIVE_LABEL], |r| r.get(0))?;
        new_labels.insert(archive, ARCHIVE_LABEL.to_owned());
    }
    let new_unread = unread > 0;
    for label_rowid in new_labels.keys() {
        add_thread_label(tx, *label_rowid, last.internal_date, thread_rowid, new_unread)?;
    }

    for (rowid, label) in &old_labels {
        let entry = changes.mailboxes.entry(label.clone()).or_default();
        if new_labels.contains_key(rowid) {
            entry.updated.insert(gmail_id.to_owned());
        } else {
            entry.removed.insert(gmail_id.to_owned());
        }
    }
    for (rowid, label) in &new_labels {
        if !old_labels.contains_key(rowid) {
            changes.mailboxes.entry(label.clone()).or_default().inserted.insert(gmail_id.to_owned());
        }
    }
    Ok(())
}

fn wholly_spam_or_trash(tx: &Transaction<'_>, thread_rowid: i64) -> StoreResult<bool> {
    let outside: i64 = tx
        .prepare_cached(
            "SELECT COUNT(*) FROM messages m WHERE m.thread_id = ?1 AND NOT EXISTS (
               SELECT 1 FROM message_labels ml JOIN labels l ON l.id = ml.label_id
               WHERE ml.message_id = m.id AND l.gmail_id IN ('SPAM', 'TRASH'))",
        )?
        .query_row([thread_rowid], |r| r.get(0))?;
    Ok(outside == 0)
}

fn add_thread_label(tx: &Transaction<'_>, label: i64, at: Millis, thread: i64, unread: bool) -> StoreResult<()> {
    tx.prepare_cached("INSERT INTO thread_labels (label_id, last_message_at, thread_id) VALUES (?1, ?2, ?3)")?
        .execute(params![label, at, thread])?;
    tx.prepare_cached(
        "INSERT INTO label_stats (label_id, thread_count, unread_thread_count) VALUES (?1, 1, ?2)
         ON CONFLICT (label_id) DO UPDATE SET thread_count = thread_count + 1,
           unread_thread_count = unread_thread_count + excluded.unread_thread_count",
    )?
    .execute(params![label, unread as i64])?;
    Ok(())
}

fn remove_thread_label(tx: &Transaction<'_>, label: i64, thread: i64, unread: bool) -> StoreResult<()> {
    tx.prepare_cached("DELETE FROM thread_labels WHERE label_id = ?1 AND thread_id = ?2")?
        .execute(params![label, thread])?;
    tx.prepare_cached(
        "UPDATE label_stats SET thread_count = thread_count - 1, unread_thread_count = unread_thread_count - ?2
         WHERE label_id = ?1",
    )?
    .execute(params![label, unread as i64])?;
    Ok(())
}
