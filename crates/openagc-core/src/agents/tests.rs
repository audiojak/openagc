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
        "approval_unavailable",
        "external tools never run without the user"
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
    assert!(path.starts_with(core.data_dir()));
    let outcome = crate::runtime::runtime().block_on(async move {
        let client = agent_mcp::ShimClient::connect(&path, "s1").await.unwrap();
        client.call("mail_list_labels", json!({})).await
    });
    assert!(matches!(outcome, Outcome::Ok { .. }), "{outcome:?}");
    assert_eq!(core.mcp_socket_path().unwrap(), core.mcp_socket_path().unwrap(), "bound once");
}
