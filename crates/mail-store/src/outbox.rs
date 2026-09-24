//! Local mutations waiting to reach the provider (spec §7.4). A mutation is
//! applied to the store and queued here in the same transaction, so the UI
//! (which reads the store) and the queue never disagree. Each op records
//! exactly the messages it changed, so a permanent failure can be undone.

use mail_domain::{LabelId, MessageId, Millis};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};

use crate::error::StoreResult;
use crate::write::{MailWriter, ThreadChanges};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OutboxOp {
    /// Add/remove labels on these messages (archive, read, star, label).
    ModifyLabels { message_ids: Vec<MessageId>, add: Vec<LabelId>, remove: Vec<LabelId> },
    /// Move these messages to Trash.
    Trash { message_ids: Vec<MessageId>, previous: Vec<(MessageId, Vec<LabelId>)> },
    /// Send a frozen message. `local_message_id` is the optimistic copy
    /// shown in Sent until the real one syncs back.
    Send {
        draft_id: i64,
        /// RFC 5322 bytes, base64.
        raw: String,
        thread_id: Option<mail_domain::ThreadId>,
        local_message_id: MessageId,
    },
}

impl OutboxOp {
    fn kind(&self) -> &'static str {
        match self {
            Self::ModifyLabels { .. } => "modify_labels",
            Self::Trash { .. } => "trash",
            Self::Send { .. } => "send",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedOp {
    pub id: i64,
    pub op: OutboxOp,
    pub attempts: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OutboxCounts {
    pub pending: u32,
    pub failed: u32,
}

pub fn enqueue(tx: &Transaction<'_>, op: &OutboxOp, now: Millis) -> StoreResult<i64> {
    tx.prepare_cached("INSERT INTO outbox (kind, payload_json, created_at) VALUES (?1, ?2, ?3)")?.execute(params![
        op.kind(),
        serde_json::to_string(op)?,
        now
    ])?;
    Ok(tx.last_insert_rowid())
}

/// The oldest pending op whose retry time has come, FIFO.
pub fn next_ready(conn: &Connection, now: Millis) -> StoreResult<Option<QueuedOp>> {
    let row: Option<(i64, String, i64)> = conn
        .prepare_cached(
            "SELECT id, payload_json, attempts FROM outbox
             WHERE state = 'pending' AND (next_attempt_at IS NULL OR next_attempt_at <= ?1)
             ORDER BY id LIMIT 1",
        )?
        .query_row([now], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?;
    match row {
        Some((id, json, attempts)) => {
            Ok(Some(QueuedOp { id, op: serde_json::from_str(&json)?, attempts: attempts.max(0) as u32 }))
        }
        None => Ok(None),
    }
}

/// When the next pending op becomes ready, if any is waiting on a retry.
pub fn next_retry_at(conn: &Connection) -> StoreResult<Option<Millis>> {
    Ok(conn.query_row("SELECT MIN(next_attempt_at) FROM outbox WHERE state = 'pending'", [], |r| r.get(0))?)
}

/// The provider accepted the op. A sent draft is deleted now.
pub fn complete(tx: &Transaction<'_>, id: i64) -> StoreResult<()> {
    let json: Option<String> =
        tx.query_row("SELECT payload_json FROM outbox WHERE id = ?1", [id], |r| r.get(0)).optional()?;
    if let Some(OutboxOp::Send { draft_id, .. }) = json.map(|j| serde_json::from_str(&j)).transpose()? {
        crate::drafts::delete(tx, draft_id)?;
    }
    tx.execute("DELETE FROM outbox WHERE id = ?1", [id])?;
    Ok(())
}

pub fn retry_later(tx: &Transaction<'_>, id: i64, next_attempt_at: Millis, error: &str) -> StoreResult<()> {
    tx.execute(
        "UPDATE outbox SET attempts = attempts + 1, next_attempt_at = ?2, last_error = ?3 WHERE id = ?1",
        params![id, next_attempt_at, error],
    )?;
    Ok(())
}

/// Give up on an op: undo its local effect and keep it as `failed` so the
/// user can see what did not reach the server.
pub fn fail(tx: &Transaction<'_>, id: i64, error: &str) -> StoreResult<ThreadChanges> {
    let json: String = tx.query_row("SELECT payload_json FROM outbox WHERE id = ?1", [id], |r| r.get(0))?;
    let op: OutboxOp = serde_json::from_str(&json)?;
    let mut w = MailWriter::new(tx);
    match &op {
        OutboxOp::ModifyLabels { message_ids, add, remove } => {
            for m in message_ids {
                w.modify_message_labels(m, remove, add)?;
            }
        }
        OutboxOp::Trash { previous, .. } => {
            for (m, labels) in previous {
                let current = current_labels(tx, m)?;
                w.modify_message_labels(m, labels, &current)?;
            }
        }
        // The optimistic Sent copy goes; the draft comes back with the error.
        OutboxOp::Send { draft_id, local_message_id, .. } => {
            w.delete_message(local_message_id)?;
            crate::drafts::set_state(tx, *draft_id, crate::drafts::DraftState::Failed, Some(error))?;
        }
    }
    let changes = w.finish()?;
    tx.execute("UPDATE outbox SET state = 'failed', last_error = ?2 WHERE id = ?1", params![id, error])?;
    Ok(changes)
}

pub fn counts(conn: &Connection) -> StoreResult<OutboxCounts> {
    let (pending, failed): (i64, i64) = conn.query_row(
        "SELECT COALESCE(SUM(state = 'pending'), 0), COALESCE(SUM(state = 'failed'), 0) FROM outbox",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(OutboxCounts { pending: pending.max(0) as u32, failed: failed.max(0) as u32 })
}

/// Drop failed ops the user has acknowledged.
pub fn clear_failed(tx: &Transaction<'_>) -> StoreResult<usize> {
    Ok(tx.execute("DELETE FROM outbox WHERE state = 'failed'", [])?)
}

fn current_labels(tx: &Transaction<'_>, m: &MessageId) -> StoreResult<Vec<LabelId>> {
    Ok(tx
        .prepare_cached(
            "SELECT l.gmail_id FROM message_labels ml JOIN labels l ON l.id = ml.label_id
             JOIN messages m ON m.id = ml.message_id WHERE m.gmail_id = ?1",
        )?
        .query_map([m.as_str()], |r| Ok(LabelId(r.get(0)?)))?
        .collect::<Result<_, _>>()?)
}

/// What a thread-level label change actually touches: the messages of these
/// threads on which it would alter labels, with their current labels.
pub fn affected_messages(
    tx: &Transaction<'_>,
    thread_ids: &[mail_domain::ThreadId],
    add: &[LabelId],
    remove: &[LabelId],
) -> StoreResult<Vec<(MessageId, Vec<LabelId>)>> {
    let mut out = Vec::new();
    let mut messages = tx.prepare_cached(
        "SELECT m.gmail_id FROM messages m JOIN threads t ON t.id = m.thread_id WHERE t.gmail_id = ?1 ORDER BY m.id",
    )?;
    for t in thread_ids {
        let ids: Vec<MessageId> =
            messages.query_map([t.as_str()], |r| Ok(MessageId(r.get(0)?)))?.collect::<Result<_, _>>()?;
        for m in ids {
            let labels = current_labels(tx, &m)?;
            let changes = add.iter().any(|l| !labels.contains(l)) || remove.iter().any(|l| labels.contains(l));
            if changes {
                out.push((m, labels));
            }
        }
    }
    Ok(out)
}
