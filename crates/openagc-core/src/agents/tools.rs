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
        Tool::CreateDraft => create_draft(core, session, arguments).await,
        Tool::UpdateDraft => update_draft(core, session, arguments).await,
        Tool::Archive => change_threads(core, arguments, ThreadChange::Archive).await,
        Tool::MarkRead => change_threads(core, arguments, ThreadChange::Read(true)).await,
        Tool::MarkUnread => change_threads(core, arguments, ThreadChange::Read(false)).await,
        Tool::AddLabel => label_threads(core, arguments, true).await,
        Tool::RemoveLabel => label_threads(core, arguments, false).await,
        Tool::CreateLabel => create_label(core, arguments).await,
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

// MARK: Changing the mailbox (Reversible)

/// "Name <a@b.c>" or "a@b.c".
fn parse_address(text: &str) -> Result<crate::ffi::AddressInfo, Outcome> {
    let t = text.trim();
    let (name, email) = match (t.rfind('<'), t.rfind('>')) {
        (Some(open), Some(close)) if open < close => {
            let name = t[..open].trim().trim_matches('"').trim();
            (if name.is_empty() { None } else { Some(name.to_owned()) }, t[open + 1..close].trim().to_owned())
        }
        _ => (None, t.to_owned()),
    };
    let valid = email.matches('@').count() == 1
        && !email.starts_with('@')
        && !email.ends_with('@')
        && !email.chars().any(|c| c.is_whitespace() || matches!(c, '<' | '>' | ',' | ';' | '"'));
    if !valid {
        return Err(Outcome::error("invalid_arguments", format!("{text:?} is not an email address")));
    }
    Ok(crate::ffi::AddressInfo { name, email })
}

fn addresses(list: Option<Vec<String>>) -> Result<Option<Vec<crate::ffi::AddressInfo>>, Outcome> {
    list.map(|l| l.iter().map(|a| parse_address(a)).collect()).transpose()
}

fn draft_json(d: &crate::DraftInfo) -> Value {
    json!({
        "draft_id": d.id,
        "subject": d.subject,
        "to": d.to.iter().map(|a| a.email.clone()).collect::<Vec<_>>(),
        "cc": d.cc.iter().map(|a| a.email.clone()).collect::<Vec<_>>(),
        "reply_to_message_id": d.in_reply_to_message_id,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateDraftArgs {
    reply_to_message_id: Option<String>,
    #[serde(default)]
    reply_all: bool,
    to: Option<Vec<String>>,
    cc: Option<Vec<String>>,
    subject: Option<String>,
    body_markdown: String,
}

async fn create_draft(core: &Arc<Core>, session: &str, arguments: Value) -> Result<Outcome, Outcome> {
    let a: CreateDraftArgs = args(arguments)?;
    let mut draft = match &a.reply_to_message_id {
        Some(id) => {
            let not_found = || Outcome::error("not_found", "no such message");
            let db = core.db().map_err(failed)?;
            let mid = MessageId(id.clone());
            let m = db
                .read(move |c| read::get_message(c, &mid))
                .await
                .map_err(|e| failed(e.into()))?
                .ok_or_else(not_found)?;
            if !in_scope(core, session, &m.thread_id) {
                return Err(not_found());
            }
            core.reply_draft(id.clone(), a.reply_all).await.map_err(failed)?
        }
        None => crate::DraftInfo {
            id: 0,
            thread_id: None,
            in_reply_to_message_id: None,
            to: vec![],
            cc: vec![],
            bcc: vec![],
            subject: String::new(),
            body_html: String::new(),
            quoted_html: String::new(),
            attachments: vec![],
            status: crate::DraftStatus::Editing,
            error: None,
            updated_at: 0,
        },
    };
    if let Some(to) = addresses(a.to)? {
        draft.to = to;
    }
    if let Some(cc) = addresses(a.cc)? {
        draft.cc = cc;
    }
    if let Some(subject) = a.subject {
        draft.subject = subject;
    }
    draft.body_html = mail_mime::markdown_to_html(&a.body_markdown);
    let quote = draft.quoted_html.clone();
    let id = core.save_draft(draft.clone()).await.map_err(failed)?;
    draft.id = id;
    core.agents.with_session(session, |s| {
        s.guard.allow_draft(id);
        s.draft_quotes.insert(id, quote);
    });
    Ok(Outcome::json(draft_json(&draft)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateDraftArgs {
    draft_id: i64,
    to: Option<Vec<String>>,
    cc: Option<Vec<String>>,
    subject: Option<String>,
    body_markdown: Option<String>,
}

async fn update_draft(core: &Arc<Core>, session: &str, arguments: Value) -> Result<Outcome, Outcome> {
    let a: UpdateDraftArgs = args(arguments)?;
    let not_yours = || Outcome::error("denied", "only drafts created in this conversation can be changed");
    let quote = core
        .agents
        .with_session(session, |s| {
            s.guard.owns_draft(a.draft_id).then(|| s.draft_quotes.get(&a.draft_id).cloned().unwrap_or_default())
        })
        .flatten()
        .ok_or_else(not_yours)?;
    let mut draft = core
        .get_draft(a.draft_id)
        .await
        .map_err(failed)?
        .ok_or_else(|| Outcome::error("not_found", "that draft no longer exists"))?;
    if let Some(to) = addresses(a.to)? {
        draft.to = to;
    }
    if let Some(cc) = addresses(a.cc)? {
        draft.cc = cc;
    }
    if let Some(subject) = a.subject {
        draft.subject = subject;
    }
    if let Some(body) = a.body_markdown {
        // The stored body includes the quote; rebuild it around the new text.
        draft.body_html = mail_mime::markdown_to_html(&body);
        draft.quoted_html = quote;
    }
    core.save_draft(draft.clone()).await.map_err(failed)?;
    Ok(Outcome::json(draft_json(&draft)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ThreadsArgs {
    thread_ids: Vec<String>,
}

enum ThreadChange {
    Archive,
    Read(bool),
}

async fn change_threads(core: &Arc<Core>, arguments: Value, change: ThreadChange) -> Result<Outcome, Outcome> {
    let a: ThreadsArgs = args(arguments)?;
    let n = a.thread_ids.len();
    let (done, key) = match change {
        ThreadChange::Archive => (core.archive(a.thread_ids).await, "archived"),
        ThreadChange::Read(true) => (core.set_read(a.thread_ids, true).await, "marked_read"),
        ThreadChange::Read(false) => (core.set_read(a.thread_ids, false).await, "marked_unread"),
    };
    done.map_err(failed)?;
    Ok(Outcome::json(json!({ key: n })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LabelArgs {
    thread_ids: Vec<String>,
    label: String,
}

async fn label_threads(core: &Arc<Core>, arguments: Value, add: bool) -> Result<Outcome, Outcome> {
    let a: LabelArgs = args(arguments)?;
    let db = core.db().map_err(failed)?;
    let labels = db.read(read::list_labels).await.map_err(|e| failed(e.into()))?;
    let wanted = a.label.trim();
    let label = labels
        .iter()
        .find(|l| l.id.as_str() == wanted)
        .or_else(|| labels.iter().find(|l| l.name.eq_ignore_ascii_case(wanted)))
        .ok_or_else(|| {
            Outcome::error("not_found", format!("there is no label {wanted:?}; create it with mail_create_label"))
        })?;
    if label.kind != mail_domain::LabelKind::User {
        return Err(Outcome::error(
            "invalid_arguments",
            format!("{} is a system label; only user labels can be set here", label.name),
        ));
    }
    let n = a.thread_ids.len();
    let (plus, minus) = if add { (vec![label.id.0.clone()], vec![]) } else { (vec![], vec![label.id.0.clone()]) };
    core.modify_labels(a.thread_ids, plus, minus).await.map_err(failed)?;
    Ok(Outcome::json(json!({ "label": label.name, if add { "labeled" } else { "unlabeled" }: n })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateLabelArgs {
    name: String,
    color: Option<String>,
}

async fn create_label(core: &Arc<Core>, arguments: Value) -> Result<Outcome, Outcome> {
    let a: CreateLabelArgs = args(arguments)?;
    if let Some(c) = &a.color
        && !(c.len() == 7 && c.starts_with('#') && c[1..].chars().all(|ch| ch.is_ascii_hexdigit()))
    {
        return Err(Outcome::error("invalid_arguments", "color must look like #rrggbb"));
    }
    let label = core.create_label(a.name, a.color).await.map_err(failed)?;
    Ok(Outcome::json(json!({ "label_id": label.id, "name": label.name })))
}
