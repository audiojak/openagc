//! Tool implementations (spec §10.2). Every call passes the session's hard
//! limits and the permission decision before it touches the store.

use std::collections::HashMap;
use std::sync::Arc;

use agent_api::AgentEvent;
use agent_mcp::Outcome;
use mail_domain::{EmailAddress, LabelId, MessageId, ThreadId, ThreadSummary, iso8601_utc};
use mail_store::read;
use permissions::{Decision, ProposedAction, Tool, decide};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::{Core, CoreError};

/// Most threads one search returns.
pub(crate) const MAX_SEARCH: u32 = 50;
/// Characters of body per message in `mail_get_thread`.
pub(crate) const MAX_BODY_CHARS: usize = 20_000;
/// Characters of extracted attachment text.
pub(crate) const MAX_ATTACHMENT_CHARS: usize = 100_000;

fn args<T: DeserializeOwned>(arguments: Value) -> Result<T, Outcome> {
    let arguments = if arguments.is_null() { json!({}) } else { arguments };
    serde_json::from_value(arguments).map_err(|e| Outcome::error("invalid_arguments", e.to_string()))
}

fn failed(e: CoreError) -> Outcome {
    match e.kind() {
        crate::ErrorKind::NotFound => Outcome::error("not_found", e.to_string()),
        crate::ErrorKind::InvalidInput => Outcome::error("invalid_arguments", e.to_string()),
        _ => Outcome::error("failed", e.to_string()),
    }
}

/// Thread ids a call names, for the per-call and session caps.
fn thread_ids_of(tool: Tool, arguments: &Value) -> Vec<ThreadId> {
    if tool.risk() == permissions::Risk::ReadOnly {
        return vec![];
    }
    arguments["thread_ids"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).map(ThreadId::new).collect())
        .unwrap_or_default()
}

pub(crate) async fn call(core: &Arc<Core>, session: &str, tool: Tool, arguments: Value) -> Outcome {
    let action =
        ProposedAction { tool, thread_ids: thread_ids_of(tool, &arguments), draft_id: arguments["draft_id"].as_i64() };
    let now = mail_sync::now_millis();
    let checked = core.agents.with_session(session, |s| s.guard.check(&action, now));
    match checked {
        None => return Outcome::error("unknown_session", "this agent session has ended"),
        Some(Err(reason)) => return Outcome::error("denied", reason.to_string()),
        Some(Ok(())) => {}
    }
    let policy = core.agents.policy.read().unwrap_or_else(|e| e.into_inner()).clone();
    match decide(&policy, &action) {
        Decision::Allow => {}
        Decision::Deny(reason) => return Outcome::error("denied", reason.to_string()),
        // The approval flow arrives with the write tools (spec §10.4).
        Decision::RequireApproval => {
            return Outcome::error(
                "approval_unavailable",
                "this action needs the user's approval, which is not available yet",
            );
        }
    }
    let result = match tool {
        Tool::Search => search(core, session, arguments).await,
        Tool::GetThread => get_thread(core, session, arguments).await,
        Tool::GetMessage => get_message(core, session, arguments).await,
        Tool::ListLabels => list_labels(core).await,
        Tool::GetAttachmentText => attachment_text(core, session, arguments).await,
        Tool::PresentThreads => present_threads(core, session, arguments),
        _ => Err(Outcome::error("not_available", format!("{} is not available yet", tool.name()))),
    };
    result.unwrap_or_else(|e| e)
}

fn in_scope(core: &Core, session: &str, thread: &ThreadId) -> bool {
    core.agents.with_session(session, |s| s.guard.scope().allows(thread)).unwrap_or(false)
}

fn address(a: &EmailAddress) -> String {
    match &a.name {
        Some(name) if !name.is_empty() => format!("{name} <{}>", a.email),
        _ => a.email.clone(),
    }
}

async fn label_names(core: &Core) -> Result<HashMap<String, String>, Outcome> {
    let db = core.db().map_err(failed)?;
    let labels = db.read(read::list_labels).await.map_err(|e| failed(e.into()))?;
    Ok(labels.into_iter().map(|l| (l.id.0, l.name)).collect())
}

fn labels_json(ids: &[LabelId], names: &HashMap<String, String>) -> Vec<String> {
    ids.iter().map(|l| names.get(l.as_str()).cloned().unwrap_or_else(|| l.0.clone())).collect()
}

fn thread_json(t: &ThreadSummary, names: &HashMap<String, String>) -> Value {
    json!({
        "thread_id": t.id,
        "subject": t.subject,
        "participants": t.participants.iter().map(address).collect::<Vec<_>>(),
        "date": iso8601_utc(t.last_message_at),
        "snippet": t.snippet,
        "labels": labels_json(&t.label_ids, names),
        "unread": t.unread_count > 0,
        "messages": t.message_count,
        "has_attachments": t.has_attachments,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    query: String,
    limit: Option<u32>,
    cursor: Option<String>,
}

async fn search(core: &Arc<Core>, session: &str, arguments: Value) -> Result<Outcome, Outcome> {
    let a: SearchArgs = args(arguments)?;
    let query = if a.query.trim().is_empty() { "in:inbox".to_owned() } else { a.query };
    let limit = a.limit.unwrap_or(20).clamp(1, MAX_SEARCH);
    let page = core.search_threads(query, a.cursor, limit).await.map_err(failed)?;
    let names = label_names(core).await?;
    let db = core.db().map_err(failed)?;
    let mut threads = Vec::with_capacity(page.rows.len());
    for row in page.rows {
        let id = ThreadId(row.id);
        if !in_scope(core, session, &id) {
            continue;
        }
        let lookup = id.clone();
        if let Some(t) = db.read(move |c| read::get_thread_summary(c, &lookup)).await.map_err(|e| failed(e.into()))? {
            threads.push(thread_json(&t, &names));
        }
    }
    Ok(Outcome::json(json!({ "threads": threads, "next_cursor": page.next_cursor })))
}

fn message_json(m: &mail_domain::Message, body: Option<&str>, include_quoted: bool) -> Value {
    let (text, truncated) = match body {
        Some(b) => {
            let b = if include_quoted { b.to_owned() } else { mail_mime::strip_quoted(b) };
            let (t, cut) = mail_mime::truncate_chars(&b, MAX_BODY_CHARS);
            (Some(t), cut)
        }
        None => (None, false),
    };
    json!({
        "message_id": m.id,
        "from": m.from.as_ref().map(address),
        "to": m.to.iter().map(address).collect::<Vec<_>>(),
        "cc": m.cc.iter().map(address).collect::<Vec<_>>(),
        "date": iso8601_utc(m.date),
        "subject": m.subject,
        "unread": !m.is_read,
        "sent_by_me": m.is_sent_by_me,
        "body": text,
        "body_available": body.is_some(),
        "truncated": truncated,
        "attachments": m.attachments.iter().filter(|a| !a.is_inline).map(|a| json!({
            "attachment_id": a.id,
            "filename": a.filename,
            "mime_type": a.mime_type,
            "size": a.size,
        })).collect::<Vec<_>>(),
    })
}

async fn body_text(core: &Core, id: &MessageId) -> Result<Option<String>, Outcome> {
    let db = core.db().map_err(failed)?;
    let id = id.clone();
    let body = db.read(move |c| read::get_body(c, &id)).await.map_err(|e| failed(e.into()))?;
    Ok(body.and_then(|b| b.text_plain))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ThreadArgs {
    thread_id: String,
}

async fn get_thread(core: &Arc<Core>, session: &str, arguments: Value) -> Result<Outcome, Outcome> {
    let a: ThreadArgs = args(arguments)?;
    let id = ThreadId(a.thread_id);
    let not_found = || Outcome::error("not_found", "no such thread");
    if !in_scope(core, session, &id) {
        return Err(not_found());
    }
    let db = core.db().map_err(failed)?;
    let lookup = id.clone();
    let (summary, messages) =
        db.read(move |c| read::get_thread(c, &lookup)).await.map_err(|e| failed(e.into()))?.ok_or_else(not_found)?;
    let names = label_names(core).await?;
    let mut out = Vec::with_capacity(messages.len());
    for m in &messages {
        let body = body_text(core, &m.id).await?;
        out.push(message_json(m, body.as_deref(), false));
    }
    Ok(Outcome::json(json!({
        "thread_id": summary.id,
        "subject": summary.subject,
        "labels": labels_json(&summary.label_ids, &names),
        "messages": out,
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageArgs {
    message_id: String,
    #[serde(default)]
    include_quoted: bool,
}

async fn get_message(core: &Arc<Core>, session: &str, arguments: Value) -> Result<Outcome, Outcome> {
    let a: MessageArgs = args(arguments)?;
    let id = MessageId(a.message_id);
    let not_found = || Outcome::error("not_found", "no such message");
    let db = core.db().map_err(failed)?;
    let lookup = id.clone();
    let m =
        db.read(move |c| read::get_message(c, &lookup)).await.map_err(|e| failed(e.into()))?.ok_or_else(not_found)?;
    if !in_scope(core, session, &m.thread_id) {
        return Err(not_found());
    }
    let body = body_text(core, &id).await?;
    let mut value = message_json(&m, body.as_deref(), a.include_quoted);
    value["thread_id"] = json!(m.thread_id);
    Ok(Outcome::json(value))
}

async fn list_labels(core: &Arc<Core>) -> Result<Outcome, Outcome> {
    let mailboxes = core.list_mailboxes().await.map_err(failed)?;
    let labels: Vec<Value> = mailboxes
        .iter()
        .filter(|m| m.label_id.is_some())
        .map(|m| {
            json!({
                "label_id": m.label_id,
                "name": m.name,
                "system": m.kind != crate::ffi::MailboxKind::Label,
                "unread": m.unread_count,
                "total": m.total_count,
            })
        })
        .collect();
    Ok(Outcome::json(json!({ "labels": labels })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AttachmentArgs {
    message_id: String,
    attachment_id: String,
}

async fn attachment_text(core: &Arc<Core>, session: &str, arguments: Value) -> Result<Outcome, Outcome> {
    let a: AttachmentArgs = args(arguments)?;
    let not_found = || Outcome::error("not_found", "no such attachment on that message");
    let row: i64 = a.attachment_id.parse().map_err(|_| not_found())?;
    let db = core.db().map_err(failed)?;
    let source = db.read(move |c| read::attachment_source(c, row)).await.map_err(|e| failed(e.into()))?;
    // The attachment must belong to the message named, and be in scope.
    let source = source.filter(|s| s.message_id.as_str() == a.message_id).ok_or_else(not_found)?;
    let message_id = source.message_id.clone();
    let m = db.read(move |c| read::get_message(c, &message_id)).await.map_err(|e| failed(e.into()))?;
    if !m.is_some_and(|m| in_scope(core, session, &m.thread_id)) {
        return Err(not_found());
    }
    let file = core.attachment_file(a.attachment_id).await.map_err(failed)?;
    let text = if mail_mime::is_pdf(&file.mime_type, &file.filename) {
        let extractor = core.agents.text.read().unwrap_or_else(|e| e.into_inner()).clone();
        let extractor = extractor.ok_or_else(|| Outcome::error("unsupported", "PDF text is not available"))?;
        let path = file.path.clone();
        tokio::task::spawn_blocking(move || extractor.pdf_text(path))
            .await
            .map_err(|e| Outcome::error("failed", e.to_string()))?
            .unwrap_or_default()
    } else {
        let path = file.path.clone();
        let bytes = tokio::fs::read(&path).await.map_err(|e| Outcome::error("failed", e.to_string()))?;
        mail_mime::extract_attachment_text(&bytes, &file.mime_type, &file.filename)
            .map_err(|e| Outcome::error("unsupported", e.to_string()))?
    };
    let (text, truncated) = mail_mime::truncate_chars(&text, MAX_ATTACHMENT_CHARS);
    Ok(Outcome::json(json!({
        "filename": file.filename,
        "mime_type": file.mime_type,
        "text": text,
        "truncated": truncated,
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PresentArgs {
    thread_ids: Vec<String>,
    #[serde(default)]
    #[allow(dead_code)]
    title: Option<String>,
}

fn present_threads(core: &Arc<Core>, session: &str, arguments: Value) -> Result<Outcome, Outcome> {
    let a: PresentArgs = args(arguments)?;
    let ids: Vec<ThreadId> = a.thread_ids.into_iter().map(ThreadId).filter(|t| in_scope(core, session, t)).collect();
    let shown = ids.len();
    core.agents.with_session(session, |s| {
        if let Some(sink) = &s.sink {
            sink.emit(AgentEvent::ResultsAvailable { thread_ids: ids });
        }
    });
    Ok(Outcome::json(json!({ "shown": shown })))
}
