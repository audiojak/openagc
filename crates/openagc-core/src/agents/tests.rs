use std::collections::BTreeSet;
use std::sync::Arc;

use agent_api::{AgentEvent, EventSink, SessionId};
use agent_mcp::Outcome;
use futures::executor::block_on;
use mail_domain::ThreadId;
use permissions::{Scope, Tool};
use serde_json::{Value, json};

use crate::{Core, CoreConfig, CoreEvent, EventListener};

struct Noop;
impl EventListener for Noop {
    fn on_event(&self, _: CoreEvent) {}
}

fn demo(name: &str) -> Arc<Core> {
    let dir = std::env::temp_dir().join(format!("openagc-core-tools-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let core = Core::new(
        CoreConfig { data_dir: dir.to_string_lossy().into_owned(), log_dir: None },
        Arc::new(crate::secrets::MemorySecrets::default()),
        Arc::new(Noop),
    )
    .unwrap();
    block_on(core.clone().open_account("demo".into())).unwrap();
    block_on(core.debug_seed_demo_mailbox(120)).unwrap();
    core
}

fn call(core: &Arc<Core>, session: &str, tool: Tool, args: Value) -> Result<Value, (String, String)> {
    match crate::runtime::runtime().block_on(super::tools::call(core, session, tool, args)) {
        Outcome::Ok { structured: Some(v), .. } => Ok(v),
        Outcome::Ok { text, .. } => Ok(Value::String(text)),
        Outcome::Error { code, message } => Err((code, message)),
    }
}

#[test]
fn search_read_and_list_labels() {
    let core = demo("read");
    core.agents.register("s1", Scope::Mailbox, None);

    let found = call(&core, "s1", Tool::Search, json!({ "query": "has:attachment", "limit": 3 })).unwrap();
    let threads = found["threads"].as_array().unwrap();
    assert!(!threads.is_empty() && threads.len() <= 3);
    let first = &threads[0];
    assert!(first["date"].as_str().unwrap().ends_with('Z'));
    assert!(first["labels"].as_array().unwrap().iter().any(|l| l == "Inbox" || l.is_string()));
    let thread_id = first["thread_id"].as_str().unwrap().to_owned();

    let thread = call(&core, "s1", Tool::GetThread, json!({ "thread_id": thread_id })).unwrap();
    let messages = thread["messages"].as_array().unwrap();
    assert!(!messages.is_empty());
    let last = messages.last().unwrap();
    assert!(last["body"].as_str().unwrap().contains("Hi"));
    assert_eq!(last["truncated"], false);
    let attachment = messages.iter().flat_map(|m| m["attachments"].as_array().unwrap().clone()).next().unwrap();

    let message_id = messages[0]["message_id"].as_str().unwrap();
    let one = call(&core, "s1", Tool::GetMessage, json!({ "message_id": message_id })).unwrap();
    assert_eq!(one["thread_id"], thread_id.as_str());

    let labels = call(&core, "s1", Tool::ListLabels, json!({})).unwrap();
    assert!(labels["labels"].as_array().unwrap().iter().any(|l| l["label_id"] == "INBOX" && l["system"] == true));

    // The demo's PDFs need the app's extractor; without one, a clear error.
    let owner = messages.iter().find(|m| !m["attachments"].as_array().unwrap().is_empty()).unwrap();
    let err = call(
        &core,
        "s1",
        Tool::GetAttachmentText,
        json!({ "message_id": owner["message_id"], "attachment_id": attachment["attachment_id"] }),
    )
    .unwrap_err();
    assert_eq!(err.0, "unsupported");

    struct FakePdf;
    impl super::TextExtractor for FakePdf {
        fn pdf_text(&self, path: String) -> Option<String> {
            Some(format!("text of {}", path.rsplit('/').next().unwrap()))
        }
    }
    core.set_text_extractor(Arc::new(FakePdf));
    let text = call(
        &core,
        "s1",
        Tool::GetAttachmentText,
        json!({ "message_id": owner["message_id"], "attachment_id": attachment["attachment_id"] }),
    )
    .unwrap();
    assert!(text["text"].as_str().unwrap().starts_with("text of "));

    // An attachment id from another message is refused.
    let err = call(
        &core,
        "s1",
        Tool::GetAttachmentText,
        json!({ "message_id": "not-the-owner", "attachment_id": attachment["attachment_id"] }),
    )
    .unwrap_err();
    assert_eq!(err.0, "not_found");
}

#[test]
fn bad_arguments_unknown_sessions_and_gated_tools() {
    let core = demo("errors");
    core.agents.register("s1", Scope::Mailbox, None);
    assert_eq!(call(&core, "s1", Tool::Search, json!({ "q": "x" })).unwrap_err().0, "invalid_arguments");
    assert_eq!(call(&core, "nobody", Tool::Search, json!({ "query": "" })).unwrap_err().0, "unknown_session");
    assert_eq!(call(&core, "s1", Tool::GetThread, json!({ "thread_id": "nope" })).unwrap_err().0, "not_found");
    assert_eq!(
        call(&core, "s1", Tool::Delete, json!({ "thread_ids": ["t"] })).unwrap_err().0,
        "failed",
        "a proposal that cannot be recorded is refused, never run"
    );
    let many: Vec<String> = (0..201).map(|i| format!("t{i}")).collect();
    assert_eq!(call(&core, "s1", Tool::Archive, json!({ "thread_ids": many })).unwrap_err().0, "denied");
}

#[test]
fn selection_scope_hides_everything_else() {
    let core = demo("scope");
    core.agents.register("all", Scope::Mailbox, None);
    let found = call(&core, "all", Tool::Search, json!({ "query": "" })).unwrap();
    let ids: Vec<String> =
        found["threads"].as_array().unwrap().iter().map(|t| t["thread_id"].as_str().unwrap().to_owned()).collect();
    assert!(ids.len() >= 3);

    let selected: BTreeSet<ThreadId> = [ThreadId::new(ids[0].clone())].into();
    core.agents.register("sel", Scope::Selection(selected), None);
    let found = call(&core, "sel", Tool::Search, json!({ "query": "" })).unwrap();
    let visible: Vec<&str> =
        found["threads"].as_array().unwrap().iter().map(|t| t["thread_id"].as_str().unwrap()).collect();
    assert_eq!(visible, vec![ids[0].as_str()]);
    assert_eq!(call(&core, "sel", Tool::GetThread, json!({ "thread_id": ids[1] })).unwrap_err().0, "not_found");
    call(&core, "sel", Tool::GetThread, json!({ "thread_id": ids[0] })).unwrap();
}

#[test]
fn present_threads_reaches_the_session() {
    let core = demo("present");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let sink = EventSink::new(SessionId("s1".into()), tx);
    core.agents.register("s1", Scope::Mailbox, Some(sink));
    let shown =
        call(&core, "s1", Tool::PresentThreads, json!({ "thread_ids": ["a", "b"], "title": "Needs reply" })).unwrap();
    assert_eq!(shown["shown"], 2);
    let (session, event) = rx.try_recv().unwrap();
    assert_eq!(session.as_str(), "s1");
    assert_eq!(event, AgentEvent::ResultsAvailable { thread_ids: vec![ThreadId::new("a"), ThreadId::new("b")] });
}

#[test]
fn tool_calls_arrive_over_the_socket() {
    let core = demo("socket");
    core.agents.register("s1", Scope::Mailbox, None);
    let path = core.mcp_socket_path().unwrap();
    // Under the data dir, or /tmp/openagc-<uid> when that path is too long.
    assert!(path.starts_with(core.data_dir()) || path.to_string_lossy().starts_with("/tmp/openagc-"), "{path:?}");
    let outcome = crate::runtime::runtime().block_on(async move {
        let client = agent_mcp::ShimClient::connect(&path, "s1").await.unwrap();
        client.call("mail_list_labels", json!({})).await
    });
    assert!(matches!(outcome, Outcome::Ok { .. }), "{outcome:?}");
    assert_eq!(core.mcp_socket_path().unwrap(), core.mcp_socket_path().unwrap(), "bound once");
}

#[derive(Default)]
struct Recorder(std::sync::Mutex<Vec<CoreEvent>>);
impl EventListener for Recorder {
    fn on_event(&self, event: CoreEvent) {
        self.0.lock().unwrap().push(event);
    }
}

#[test]
fn a_fake_agent_session_round_trips_through_the_ffi() {
    use super::AgentEventInfo as E;
    let dir = std::env::temp_dir().join(format!("openagc-core-agent-ffi-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let recorder = Arc::new(Recorder::default());
    let core = Core::new(
        CoreConfig { data_dir: dir.to_string_lossy().into_owned(), log_dir: None },
        Arc::new(crate::secrets::MemorySecrets::default()),
        recorder.clone(),
    )
    .unwrap();
    core.debug_use_fake_agents();
    let providers = block_on(core.clone().list_agent_providers(false));
    assert_eq!(providers.len(), 2);
    assert_eq!(providers[0].id, "claude-code");
    assert!(matches!(providers[0].status, super::AgentStatusInfo::Ready { .. }));
    assert_eq!(providers[1].status, super::AgentStatusInfo::NotInstalled);

    let err = block_on(core.clone().start_agent_session("claude-code".into(), None, None)).unwrap_err();
    assert_eq!(err.kind(), crate::ErrorKind::NotFound, "an account must be open");
    block_on(core.clone().open_account("demo".into())).unwrap();
    let err = block_on(core.clone().start_agent_session("gpt".into(), None, None)).unwrap_err();
    assert_eq!(err.kind(), crate::ErrorKind::InvalidInput);

    let session = block_on(core.clone().start_agent_session("claude-code".into(), None, None)).unwrap();
    assert!(core.agents.has(&session), "tool calls can bind to it");
    block_on(core.clone().send_agent_prompt(session.clone(), "hi".into(), super::PromptContextInfo::default()))
        .unwrap();

    let mut seen: Vec<E> = Vec::new();
    for _ in 0..100 {
        seen = recorder
            .0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                CoreEvent::AgentEvents { session_id, events } if *session_id == session => Some(events.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        if seen.iter().any(|e| matches!(e, E::TurnCompleted { .. })) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(matches!(seen[0], E::SessionStarted { .. }), "{seen:?}");
    assert!(seen.contains(&E::TextDelta { text: "You said: hi".into() }));
    assert!(seen.iter().any(|e| matches!(e, E::TurnCompleted { input_tokens: Some(10), .. })));

    block_on(core.clone().close_agent_session(session.clone())).unwrap();
    assert!(!core.agents.has(&session));
    let err = block_on(core.clone().cancel_agent_turn(session.clone())).unwrap_err();
    assert_eq!(err.kind(), crate::ErrorKind::NotFound);

    // The conversation was stored and can be continued.
    let history = block_on(core.list_agent_history(10)).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!((history[0].session_id.as_str(), history[0].title.as_str()), (session.as_str(), "hi"));
    assert!(session.starts_with("agent-") && session.len() > "agent-1".len(), "unique across launches: {session}");
    let transcript = block_on(core.agent_transcript(session.clone())).unwrap();
    assert_eq!(transcript[0], super::AgentTranscriptItem::Prompt { text: "hi".into() });
    assert!(
        transcript.contains(&super::AgentTranscriptItem::Event { event: E::TextDelta { text: "You said: hi".into() } })
    );

    let resumed = block_on(core.clone().resume_agent_session(session.clone())).unwrap();
    assert_eq!(resumed, session, "same conversation id");
    assert!(core.agents.has(&session));
    block_on(core.clone().send_agent_prompt(session.clone(), "again".into(), super::PromptContextInfo::default()))
        .unwrap();
    for _ in 0..100 {
        let t = block_on(core.agent_transcript(session.clone())).unwrap();
        if t.iter()
            .any(|i| *i == super::AgentTranscriptItem::Event { event: E::TextDelta { text: "You said: again".into() } })
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let history = block_on(core.list_agent_history(10)).unwrap();
    assert_eq!(history[0].prompt_count, 2);
    assert_eq!(
        block_on(core.clone().resume_agent_session("nope".into())).unwrap_err().kind(),
        crate::ErrorKind::NotFound
    );
}

fn inbox(core: &Arc<Core>) -> Vec<String> {
    block_on(core.list_threads("INBOX".into(), None, 500)).unwrap().rows.into_iter().map(|t| t.id).collect()
}

#[test]
fn drafts_written_by_the_agent() {
    let core = demo("drafts");
    core.agents.register("s1", Scope::Mailbox, None);
    core.agents.register("s2", Scope::Mailbox, None);

    let made = call(
        &core,
        "s1",
        Tool::CreateDraft,
        json!({ "to": ["Alex Rivera <alex@example.org>"], "subject": "Plan", "body_markdown": "Hi **Alex**" }),
    )
    .unwrap();
    let id = made["draft_id"].as_i64().unwrap();
    let stored = block_on(core.get_draft(id)).unwrap().unwrap();
    assert!(stored.body_html.contains("<strong>Alex</strong>"), "{}", stored.body_html);
    assert_eq!(stored.to[0].name.as_deref(), Some("Alex Rivera"));

    call(&core, "s1", Tool::UpdateDraft, json!({ "draft_id": id, "subject": "Plan v2" })).unwrap();
    assert_eq!(block_on(core.get_draft(id)).unwrap().unwrap().subject, "Plan v2");
    assert_eq!(
        call(&core, "s2", Tool::UpdateDraft, json!({ "draft_id": id, "subject": "hijack" })).unwrap_err().0,
        "denied",
        "another session cannot touch it"
    );
    assert_eq!(
        call(&core, "s1", Tool::CreateDraft, json!({ "to": ["not an address"], "body_markdown": "x" })).unwrap_err().0,
        "invalid_arguments"
    );

    // A reply keeps its quote when the body is rewritten.
    let thread = inbox(&core)[0].clone();
    let detail = block_on(core.get_thread(thread)).unwrap().unwrap();
    let parent = detail.messages.last().unwrap().id.clone();
    let reply = call(&core, "s1", Tool::CreateDraft, json!({ "reply_to_message_id": parent, "body_markdown": "Yes." }))
        .unwrap();
    let rid = reply["draft_id"].as_i64().unwrap();
    assert!(reply["subject"].as_str().unwrap().starts_with("Re:"));
    call(&core, "s1", Tool::UpdateDraft, json!({ "draft_id": rid, "body_markdown": "Actually, no." })).unwrap();
    let body = block_on(core.get_draft(rid)).unwrap().unwrap().body_html;
    assert!(body.starts_with("<p>Actually, no.</p>"), "{body}");
    assert_eq!(body.matches("<blockquote>").count(), 1, "the quote is kept once");
}

#[test]
fn archive_read_state_and_labels() {
    let core = demo("changes");
    core.agents.register("s1", Scope::Mailbox, None);
    let ids = inbox(&core);
    let (a, b) = (ids[0].clone(), ids[1].clone());

    assert_eq!(call(&core, "s1", Tool::Archive, json!({ "thread_ids": [a] })).unwrap()["archived"], 1);
    assert!(!inbox(&core).contains(&a));

    call(&core, "s1", Tool::MarkUnread, json!({ "thread_ids": [b] })).unwrap();
    assert!(block_on(core.get_thread(b.clone())).unwrap().unwrap().thread.unread_count > 0);
    call(&core, "s1", Tool::MarkRead, json!({ "thread_ids": [b] })).unwrap();
    assert_eq!(block_on(core.get_thread(b.clone())).unwrap().unwrap().thread.unread_count, 0);

    let made = call(&core, "s1", Tool::CreateLabel, json!({ "name": "Sorted/Important", "color": "#fb4c2f" })).unwrap();
    let again = call(&core, "s1", Tool::CreateLabel, json!({ "name": "sorted/important" })).unwrap();
    assert_eq!(made["label_id"], again["label_id"], "idempotent by name");
    assert_eq!(call(&core, "s1", Tool::CreateLabel, json!({ "name": "TRASH" })).unwrap_err().0, "invalid_arguments");
    assert_eq!(
        call(&core, "s1", Tool::CreateLabel, json!({ "name": "x", "color": "red" })).unwrap_err().0,
        "invalid_arguments"
    );

    call(&core, "s1", Tool::AddLabel, json!({ "thread_ids": [b], "label": "Sorted/Important" })).unwrap();
    let labels = block_on(core.get_thread(b.clone())).unwrap().unwrap().thread.label_ids;
    assert!(labels.contains(&made["label_id"].as_str().unwrap().to_owned()));
    call(&core, "s1", Tool::RemoveLabel, json!({ "thread_ids": [b], "label": made["label_id"] })).unwrap();
    assert!(
        !block_on(core.get_thread(b.clone()))
            .unwrap()
            .unwrap()
            .thread
            .label_ids
            .contains(&made["label_id"].as_str().unwrap().to_owned())
    );

    assert_eq!(
        call(&core, "s1", Tool::AddLabel, json!({ "thread_ids": [b], "label": "INBOX" })).unwrap_err().0,
        "invalid_arguments"
    );
    assert_eq!(
        call(&core, "s1", Tool::AddLabel, json!({ "thread_ids": [b], "label": "Nope" })).unwrap_err().0,
        "not_found"
    );
}

#[test]
fn a_selection_session_cannot_change_other_threads() {
    let core = demo("scoped-writes");
    let ids = inbox(&core);
    core.agents.register("sel", Scope::Selection([ThreadId::new(ids[0].clone())].into()), None);
    assert_eq!(call(&core, "sel", Tool::Archive, json!({ "thread_ids": [ids[1]] })).unwrap_err().0, "denied");
    call(&core, "sel", Tool::Archive, json!({ "thread_ids": [ids[0]] })).unwrap();
}

/// A tool session backed by a stored session row, as a real one is.
fn session_with_events(core: &Arc<Core>, id: &str) -> tokio::sync::mpsc::UnboundedReceiver<(SessionId, AgentEvent)> {
    let db = core.db().unwrap();
    let uuid = id.to_owned();
    db.write_blocking(move |tx| mail_store::agents::start_session(tx, &uuid, "claude-code", 1)).unwrap();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    core.agents.register(id, Scope::Mailbox, Some(EventSink::new(SessionId(id.into()), tx)));
    rx
}

/// Run a call in the background; returns its eventual outcome.
fn spawn_call(core: &Arc<Core>, session: &str, tool: Tool, args: Value) -> tokio::task::JoinHandle<Outcome> {
    let (core, session) = (core.clone(), session.to_owned());
    crate::runtime::runtime().spawn(async move { super::tools::call(&core, &session, tool, args).await })
}

fn next_proposal(rx: &mut tokio::sync::mpsc::UnboundedReceiver<(SessionId, AgentEvent)>) -> (i64, String, Option<i64>) {
    loop {
        let (_, e) = crate::runtime::runtime()
            .block_on(async { tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await })
            .expect("a proposal in time")
            .unwrap();
        if let AgentEvent::ActionProposed { action_id, summary, draft_id, .. } = e {
            return (action_id, summary, draft_id);
        }
    }
}

fn outcome(handle: tokio::task::JoinHandle<Outcome>) -> Outcome {
    crate::runtime::runtime().block_on(handle).unwrap()
}

#[test]
fn sending_waits_for_the_user_and_the_draft_is_frozen_meanwhile() {
    let core = demo("approve-send");
    let mut rx = session_with_events(&core, "s1");
    let made = call(
        &core,
        "s1",
        Tool::CreateDraft,
        json!({ "to": ["sam@example.org"], "subject": "Lunch", "body_markdown": "Noon?" }),
    )
    .unwrap();
    let draft = made["draft_id"].as_i64().unwrap();

    let sending = spawn_call(&core, "s1", Tool::Send, json!({ "draft_id": draft }));
    let (action, summary, draft_id) = next_proposal(&mut rx);
    assert_eq!(summary, "Send “Lunch” to sam@example.org");
    assert_eq!(draft_id, Some(draft));
    assert!(!sending.is_finished(), "parked until the user answers");
    assert_eq!(
        call(&core, "s1", Tool::UpdateDraft, json!({ "draft_id": draft, "to": ["attacker@evil.test"] })).unwrap_err().0,
        "denied",
        "the agent cannot change what the user is reviewing"
    );
    core.resolve_agent_action(action, true).unwrap();
    assert!(matches!(outcome(sending), Outcome::Ok { .. }));
    assert!(block_on(core.get_draft(draft)).unwrap().is_none(), "sent");
    let sent = block_on(core.list_threads("SENT".into(), None, 50)).unwrap().rows;
    assert!(sent.iter().any(|t| t.subject == "Lunch"));

    let log = block_on(core.list_agent_actions(10)).unwrap();
    let send = log.iter().find(|a| a.tool == "mail_send").unwrap();
    assert_eq!((send.risk.as_str(), send.state.as_str()), ("external", "done"));
    assert!(log.iter().any(|a| a.tool == "mail_create_draft" && a.state == "done"));
    assert_eq!(core.resolve_agent_action(action, true).unwrap_err().kind(), crate::ErrorKind::NotFound);
}

#[test]
fn rejected_timed_out_and_cancelled_proposals_do_nothing() {
    let core = demo("approve-reject");
    let mut rx = session_with_events(&core, "s1");
    let ids = inbox(&core);

    let deleting = spawn_call(&core, "s1", Tool::Delete, json!({ "thread_ids": [ids[0]] }));
    let (action, summary, _) = next_proposal(&mut rx);
    assert_eq!(summary, "Move 1 thread to Trash");
    core.resolve_agent_action(action, false).unwrap();
    assert!(matches!(outcome(deleting), Outcome::Error { code, .. } if code == "rejected_by_user"));
    assert!(inbox(&core).contains(&ids[0]), "nothing was trashed");

    // A declined forward leaves no draft.
    let detail = block_on(core.get_thread(ids[1].clone())).unwrap().unwrap();
    let message = detail.messages.last().unwrap().id.clone();
    let forwarding = spawn_call(
        &core,
        "s1",
        Tool::Forward,
        json!({ "message_id": message, "to": ["pat@example.com"], "note_markdown": "FYI" }),
    );
    let (action, summary, draft) = next_proposal(&mut rx);
    assert!(summary.starts_with("Forward “") && summary.ends_with("to pat@example.com"), "{summary}");
    assert!(block_on(core.get_draft(draft.unwrap())).unwrap().is_some(), "reviewable while pending");
    core.resolve_agent_action(action, false).unwrap();
    outcome(forwarding);
    assert!(block_on(core.get_draft(draft.unwrap())).unwrap().is_none());

    // Unanswered: expires.
    *core.agents.approvals.timeout.lock().unwrap() = Some(std::time::Duration::from_millis(100));
    let expiring = spawn_call(&core, "s1", Tool::Delete, json!({ "thread_ids": [ids[2]] }));
    next_proposal(&mut rx);
    assert!(matches!(outcome(expiring), Outcome::Error { code, .. } if code == "approval_timeout"));
    *core.agents.approvals.timeout.lock().unwrap() = None;

    // Cancelling the turn rejects what is pending. (Bounded, so a
    // regression fails instead of hanging for ten minutes.)
    *core.agents.approvals.timeout.lock().unwrap() = Some(std::time::Duration::from_secs(5));
    let pending = spawn_call(&core, "s1", Tool::Delete, json!({ "thread_ids": [ids[3]] }));
    next_proposal(&mut rx);
    let _ = block_on(core.clone().cancel_agent_turn("s1".into()));
    assert!(matches!(outcome(pending), Outcome::Error { code, .. } if code == "rejected_by_user"));
    *core.agents.approvals.timeout.lock().unwrap() = None;
    assert!(inbox(&core).contains(&ids[3]));

    let states: Vec<String> = block_on(core.list_agent_actions(20)).unwrap().into_iter().map(|a| a.state).collect();
    assert!(states.contains(&"rejected".to_owned()) && states.contains(&"expired".to_owned()), "{states:?}");
}

#[test]
fn a_user_policy_can_require_approval_for_reversible_tools() {
    let core = demo("policy");
    let mut rx = session_with_events(&core, "s1");
    core.agents.policy.write().unwrap().set_requires_approval(Tool::Archive, true).unwrap();
    let id = inbox(&core)[0].clone();
    let archiving = spawn_call(&core, "s1", Tool::Archive, json!({ "thread_ids": [id] }));
    let (action, summary, _) = next_proposal(&mut rx);
    assert_eq!(summary, "mail_archive on 1 thread");
    core.resolve_agent_action(action, true).unwrap();
    assert!(matches!(outcome(archiving), Outcome::Ok { .. }));
    assert!(!inbox(&core).contains(&id));
}
