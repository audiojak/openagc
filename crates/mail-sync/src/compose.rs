//! Compose and send (spec §7.5, §14.5): reply/forward drafts built from a
//! stored message, and sending a draft through the outbox.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use mail_domain::{Body, EmailAddress, LabelId, MessageId, Millis, ThreadId, system_labels};
use mail_mime::{OutgoingAttachment, OutgoingMessage};
use mail_store::drafts::{self, DraftRecord, DraftState};
use mail_store::outbox::{self, OutboxOp};
use mail_store::{Db, IncomingMessage, LOCAL_PREFIX, MailWriter, StoreError, ThreadChanges, read};

use crate::error::{SyncError, SyncResult};
use crate::outbox::now_millis;

fn random_token() -> String {
    let mut b = [0u8; 12];
    let _ = getrandom::fill(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn quoted_header(date: Millis, from: Option<&EmailAddress>) -> String {
    let who = from.map(|f| escape(f.display())).unwrap_or_else(|| "someone".into());
    // ISO date keeps this locale-neutral; the composer shows it as text.
    let secs = date / 1000;
    let days = secs.div_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!("On {y:04}-{m:02}-{d:02}, {who} wrote:")
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn parent_html(body: &Option<Body>) -> String {
    match body {
        Some(Body { html_sanitized: Some(html), .. }) => html.clone(),
        Some(Body { text_plain: Some(text), .. }) => mail_mime::text_to_html(text),
        _ => String::new(),
    }
}

/// A reply (or reply-all) draft to `message_id`, not yet saved.
pub async fn reply_draft(db: &Db, message_id: &MessageId, all: bool, me: &[String]) -> SyncResult<DraftRecord> {
    let id = message_id.clone();
    let (parent, body) = db.read(move |c| Ok((read::get_message(c, &id)?, read::get_body(c, &id)?))).await?;
    let parent = parent.ok_or_else(|| StoreError::NotFound(format!("message {message_id}")))?;
    let (to, cc) = mail_mime::reply_recipients(parent.from.as_ref(), &parent.reply_to, &parent.to, &parent.cc, me, all);
    let html = format!(
        "<p><br></p><div>{}</div><blockquote>{}</blockquote>",
        quoted_header(parent.date, parent.from.as_ref()),
        parent_html(&body)
    );
    Ok(DraftRecord {
        thread_id: Some(parent.thread_id.0.clone()),
        in_reply_to: Some(parent.id.0.clone()),
        to,
        cc,
        subject: mail_mime::reply_subject(&parent.subject),
        body_html: html,
        ..Default::default()
    })
}

/// A forward draft of `message_id`, not yet saved. Recipients are empty.
pub async fn forward_draft(db: &Db, message_id: &MessageId) -> SyncResult<DraftRecord> {
    let id = message_id.clone();
    let (parent, body) = db.read(move |c| Ok((read::get_message(c, &id)?, read::get_body(c, &id)?))).await?;
    let parent = parent.ok_or_else(|| StoreError::NotFound(format!("message {message_id}")))?;
    let list = |v: &[EmailAddress]| escape(&v.iter().map(|a| a.display().to_owned()).collect::<Vec<_>>().join(", "));
    let header = format!(
        "<div>---------- Forwarded message ----------<br>From: {}<br>Subject: {}<br>To: {}</div>",
        parent.from.as_ref().map(|f| escape(&format!("{} <{}>", f.display(), f.email))).unwrap_or_default(),
        escape(&parent.subject),
        list(&parent.to),
    );
    Ok(DraftRecord {
        thread_id: Some(parent.thread_id.0.clone()),
        subject: mail_mime::forward_subject(&parent.subject),
        body_html: format!("<p><br></p>{header}<br>{}", parent_html(&body)),
        ..Default::default()
    })
}

/// Freeze a saved draft into MIME and queue it (or, with no provider,
/// "send" it locally). An optimistic copy appears in Sent at once.
pub async fn send_draft(db: &Db, draft_id: i64, from: EmailAddress, queue: bool) -> SyncResult<ThreadChanges> {
    let draft = db
        .read(move |c| drafts::get(c, draft_id))
        .await?
        .ok_or_else(|| StoreError::NotFound(format!("draft {draft_id}")))?;
    if draft.state == DraftState::Sending {
        return Err(StoreError::Invalid("this draft is already being sent".into()).into());
    }
    let parent = match &draft.in_reply_to {
        Some(id) => {
            let id = MessageId(id.clone());
            db.read(move |c| read::get_message(c, &id)).await?
        }
        None => None,
    };
    let mut attachments = Vec::with_capacity(draft.attachments.len());
    for a in &draft.attachments {
        let data = std::fs::read(&a.path).map_err(|e| StoreError::Io(format!("{}: {e}", a.filename)))?;
        attachments.push(OutgoingAttachment { filename: a.filename.clone(), mime_type: a.mime_type.clone(), data });
    }
    let domain = from.email.rsplit('@').next().unwrap_or("localhost").to_owned();
    let rfc822_id = format!("{}.openagc@{domain}", random_token());
    let now = now_millis();
    let outgoing = OutgoingMessage {
        from: from.clone(),
        to: draft.to.clone(),
        cc: draft.cc.clone(),
        bcc: draft.bcc.clone(),
        subject: draft.subject.clone(),
        html: draft.body_html.clone(),
        text: None,
        message_id: rfc822_id.clone(),
        in_reply_to: parent.as_ref().and_then(|p| p.rfc822_message_id.clone()),
        references: parent
            .as_ref()
            .map(|p| mail_mime::reply_references(&p.references, p.rfc822_message_id.as_deref()))
            .unwrap_or_default(),
        attachments,
        date: now,
    };
    let raw = mail_mime::build(&outgoing).map_err(|e| SyncError::Store(StoreError::Invalid(e.to_string())))?;
    let text = mail_mime::html_to_text(&draft.body_html);
    let sanitized = mail_mime::sanitize_html(&draft.body_html);
    let token = random_token();
    let thread_id = ThreadId(draft.thread_id.clone().unwrap_or_else(|| format!("{LOCAL_PREFIX}thread-{token}")));
    // Queued: a placeholder id replaced when the real copy syncs back.
    // Local-only (demo): a permanent id.
    let local_id = MessageId(if queue { format!("{LOCAL_PREFIX}{token}") } else { format!("sent-{token}") });
    let copy = IncomingMessage {
        id: local_id.clone(),
        thread_id: thread_id.clone(),
        rfc822_message_id: Some(rfc822_id.clone()),
        in_reply_to: outgoing.in_reply_to.clone(),
        references: outgoing.references.clone(),
        from: Some(from),
        to: draft.to.clone(),
        cc: draft.cc.clone(),
        bcc: draft.bcc.clone(),
        subject: draft.subject.clone(),
        date: now,
        internal_date: now,
        snippet: text.chars().take(120).collect(),
        label_ids: vec![LabelId::new(system_labels::SENT)],
        size_estimate: raw.len() as u64,
        body: Some(Body { text_plain: Some(text), html_sanitized: Some(sanitized.html), has_remote_images: false }),
        ..Default::default()
    };
    let op = OutboxOp::Send {
        draft_id,
        raw: STANDARD.encode(&raw),
        thread_id: draft.thread_id.clone().map(ThreadId),
        local_message_id: local_id,
    };
    Ok(db
        .write(move |tx| {
            let mut w = MailWriter::new(tx);
            w.upsert_message(&copy)?;
            let changes = w.finish()?;
            if queue {
                drafts::set_state(tx, draft_id, DraftState::Sending, None)?;
                drafts::set_rfc822_id(tx, draft_id, &rfc822_id)?;
                outbox::enqueue(tx, &op, now)?;
            } else {
                drafts::delete(tx, draft_id)?;
            }
            Ok(changes)
        })
        .await?)
}

/// Decode a queued Send op's raw bytes.
pub(crate) fn decode_raw(raw: &str) -> Option<Vec<u8>> {
    STANDARD.decode(raw).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_header_uses_the_parent_date() {
        assert_eq!(
            quoted_header(1_789_489_800_000, Some(&EmailAddress::new(Some("Alex"), "a@example.com"))),
            "On 2026-09-15, Alex wrote:"
        );
    }
}
