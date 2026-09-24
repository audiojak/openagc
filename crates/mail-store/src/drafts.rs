//! Local drafts (spec §14.5). Autosaved by the composer; sending freezes a
//! draft into MIME and queues it in the outbox. The draft is deleted only
//! once the provider accepts the send, so a failed send brings it back.

use mail_domain::{EmailAddress, Millis};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use serde::{Deserialize, Serialize};

use crate::error::{StoreError, StoreResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DraftState {
    #[default]
    Editing,
    Sending,
    Failed,
}

impl DraftState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Editing => "editing",
            Self::Sending => "sending",
            Self::Failed => "failed",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "sending" => Self::Sending,
            "failed" => Self::Failed,
            _ => Self::Editing,
        }
    }
}

/// A file attached to a draft, copied into the app's data directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftAttachment {
    pub path: String,
    pub filename: String,
    pub mime_type: String,
    pub size: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DraftRecord {
    /// 0 for a draft not saved yet.
    pub id: i64,
    /// The server copy's id, once mirrored.
    pub gmail_draft_id: Option<String>,
    /// Provider thread id when this is a reply or forward.
    pub thread_id: Option<String>,
    /// Provider message id of the message being replied to.
    pub in_reply_to: Option<String>,
    pub to: Vec<EmailAddress>,
    pub cc: Vec<EmailAddress>,
    pub bcc: Vec<EmailAddress>,
    pub subject: String,
    pub body_html: String,
    /// The quoted original of a new reply or forward, kept apart from the
    /// editable body until the draft is first saved (not persisted: saved
    /// drafts carry it inside `body_html`).
    pub quoted_html: String,
    pub attachments: Vec<DraftAttachment>,
    pub updated_at: Millis,
    pub state: DraftState,
    pub last_error: Option<String>,
}

const COLUMNS: &str = "id, thread_id, in_reply_to_message_id, to_json, cc_json, bcc_json, subject, body_html, \
                       attachments_json, updated_at, state, last_error, gmail_draft_id";

fn from_row(r: &Row<'_>) -> rusqlite::Result<(DraftRecord, [String; 4])> {
    Ok((
        DraftRecord {
            id: r.get(0)?,
            thread_id: r.get(1)?,
            in_reply_to: r.get(2)?,
            subject: r.get(6)?,
            body_html: r.get(7)?,
            updated_at: r.get(9)?,
            state: DraftState::parse(&r.get::<_, String>(10)?),
            last_error: r.get(11)?,
            gmail_draft_id: r.get(12)?,
            ..Default::default()
        },
        [r.get(3)?, r.get(4)?, r.get(5)?, r.get(8)?],
    ))
}

fn finish((mut d, [to, cc, bcc, att]): (DraftRecord, [String; 4])) -> StoreResult<DraftRecord> {
    d.to = serde_json::from_str(&to)?;
    d.cc = serde_json::from_str(&cc)?;
    d.bcc = serde_json::from_str(&bcc)?;
    d.attachments = serde_json::from_str(&att)?;
    Ok(d)
}

/// Insert (id 0) or update a draft that is still being edited. Returns its
/// id. A quote still held apart is stored as part of the body.
pub fn save(tx: &Transaction<'_>, d: &DraftRecord, now: Millis) -> StoreResult<i64> {
    let body_html = format!("{}{}", d.body_html, d.quoted_html);
    let values = (
        serde_json::to_string(&d.to)?,
        serde_json::to_string(&d.cc)?,
        serde_json::to_string(&d.bcc)?,
        serde_json::to_string(&d.attachments)?,
    );
    if d.id == 0 {
        tx.prepare_cached(
            "INSERT INTO drafts (thread_id, in_reply_to_message_id, to_json, cc_json, bcc_json, subject, body_html,
               body_text, attachments_json, updated_at, dirty, state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, '', ?8, ?9, 1, 'editing')",
        )?
        .execute(params![
            d.thread_id,
            d.in_reply_to,
            values.0,
            values.1,
            values.2,
            d.subject,
            body_html,
            values.3,
            now
        ])?;
        return Ok(tx.last_insert_rowid());
    }
    let state: Option<String> =
        tx.query_row("SELECT state FROM drafts WHERE id = ?1", [d.id], |r| r.get(0)).optional()?;
    match state.as_deref() {
        None => return Err(StoreError::NotFound(format!("draft {}", d.id))),
        Some("sending") => return Err(StoreError::Invalid("this draft is being sent".into())),
        _ => {}
    }
    tx.prepare_cached(
        "UPDATE drafts SET to_json = ?2, cc_json = ?3, bcc_json = ?4, subject = ?5, body_html = ?6,
           attachments_json = ?7, updated_at = ?8, dirty = 1, state = 'editing', last_error = NULL
         WHERE id = ?1",
    )?
    .execute(params![d.id, values.0, values.1, values.2, d.subject, body_html, values.3, now])?;
    Ok(d.id)
}

pub fn get(conn: &Connection, id: i64) -> StoreResult<Option<DraftRecord>> {
    let row = conn
        .prepare_cached(&format!("SELECT {COLUMNS} FROM drafts WHERE id = ?1"))?
        .query_row([id], from_row)
        .optional()?;
    row.map(finish).transpose()
}

/// Drafts being edited or that failed to send, newest first.
pub fn list(conn: &Connection) -> StoreResult<Vec<DraftRecord>> {
    let rows = conn
        .prepare_cached(&format!(
            "SELECT {COLUMNS} FROM drafts WHERE state != 'sending' ORDER BY updated_at DESC, id DESC"
        ))?
        .query_map([], from_row)?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter().map(finish).collect()
}

pub fn delete(tx: &Transaction<'_>, id: i64) -> StoreResult<()> {
    tx.execute("DELETE FROM drafts WHERE id = ?1", [id])?;
    Ok(())
}

/// Delete a draft, queueing deletion of its server copy if it has one.
pub fn discard(tx: &Transaction<'_>, id: i64, now: Millis) -> StoreResult<()> {
    let gmail_id: Option<String> =
        tx.query_row("SELECT gmail_draft_id FROM drafts WHERE id = ?1", [id], |r| r.get(0)).optional()?.flatten();
    delete(tx, id)?;
    if let Some(gmail_draft_id) = gmail_id {
        crate::outbox::enqueue(tx, &crate::outbox::OutboxOp::DeleteDraft { gmail_draft_id }, now)?;
    }
    Ok(())
}

/// Drafts edited since they were last mirrored, marked clean. Drafts being
/// sent are left alone: the send replaces the server copy.
pub fn take_dirty(tx: &Transaction<'_>) -> StoreResult<Vec<i64>> {
    let ids: Vec<i64> = tx
        .prepare_cached("SELECT id FROM drafts WHERE dirty = 1 AND state != 'sending' ORDER BY id")?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    tx.execute("UPDATE drafts SET dirty = 0 WHERE dirty = 1 AND state != 'sending'", [])?;
    Ok(ids)
}

pub fn set_gmail_draft_id(tx: &Transaction<'_>, id: i64, gmail_draft_id: Option<&str>) -> StoreResult<()> {
    tx.execute("UPDATE drafts SET gmail_draft_id = ?2 WHERE id = ?1", params![id, gmail_draft_id])?;
    Ok(())
}

pub fn set_state(tx: &Transaction<'_>, id: i64, state: DraftState, error: Option<&str>) -> StoreResult<()> {
    tx.execute("UPDATE drafts SET state = ?2, last_error = ?3 WHERE id = ?1", params![id, state.as_str(), error])?;
    Ok(())
}

pub fn set_rfc822_id(tx: &Transaction<'_>, id: i64, message_id: &str) -> StoreResult<()> {
    tx.execute("UPDATE drafts SET rfc822_message_id = ?2 WHERE id = ?1", params![id, message_id])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Db;

    fn db() -> Db {
        let dir = std::env::temp_dir().join(format!("openagc-drafts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Db::open(&dir.join("mail.sqlite")).unwrap()
    }

    #[test]
    fn save_get_update_list_delete() {
        let db = db();
        let mut d = DraftRecord {
            to: vec![EmailAddress::new(Some("Alex"), "alex@example.com")],
            subject: "Hello".into(),
            body_html: "<p>Hi</p>".into(),
            attachments: vec![DraftAttachment {
                path: "/tmp/a.pdf".into(),
                filename: "a.pdf".into(),
                mime_type: "application/pdf".into(),
                size: 3,
            }],
            ..Default::default()
        };
        let id = db
            .write_blocking({
                let d = d.clone();
                move |tx| save(tx, &d, 1)
            })
            .unwrap();
        d.id = id;
        let got = db.read_blocking(|c| get(c, id)).unwrap().unwrap();
        assert_eq!(got.subject, "Hello");
        assert_eq!(got.to, d.to);
        assert_eq!(got.attachments, d.attachments);
        d.subject = "Hello again".into();
        db.write_blocking({
            let d = d.clone();
            move |tx| save(tx, &d, 2)
        })
        .unwrap();
        assert_eq!(db.read_blocking(list).unwrap()[0].subject, "Hello again");

        db.write_blocking(move |tx| set_state(tx, id, DraftState::Sending, None)).unwrap();
        assert!(db.read_blocking(list).unwrap().is_empty(), "sending drafts are not listed");
        let err = db
            .write_blocking({
                let d = d.clone();
                move |tx| save(tx, &d, 3)
            })
            .unwrap_err();
        assert!(matches!(err, StoreError::Invalid(_)), "cannot edit while sending");
        db.write_blocking(move |tx| set_state(tx, id, DraftState::Failed, Some("offline"))).unwrap();
        let failed = db.read_blocking(|c| get(c, id)).unwrap().unwrap();
        assert_eq!((failed.state, failed.last_error.as_deref()), (DraftState::Failed, Some("offline")));
        db.write_blocking(move |tx| delete(tx, id)).unwrap();
        assert!(db.read_blocking(|c| get(c, id)).unwrap().is_none());
    }
}
