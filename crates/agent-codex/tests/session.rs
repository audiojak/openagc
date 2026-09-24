//! A Codex session against a fake app-server.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_api::process::Locator;
use agent_api::{
    AgentError, AgentEvent, AgentProvider, EventSink, McpEndpoint, PromptContext, SessionConfig, SessionId, TurnInput,
    Usage,
};
use agent_codex::CodexProvider;
use serde_json::Value;
use tokio::sync::mpsc;

fn setup(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("oagc-codex-session-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let fake = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake_app_server.py");
    std::fs::copy(&fake, d.join("codex")).unwrap();
    std::fs::set_permissions(d.join("codex"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(d.join("prompt.md"), "mail rules").unwrap();
    d
}

fn config(d: &Path, resume: Option<&str>) -> SessionConfig {
    SessionConfig {
        session_id: SessionId("agent-3".into()),
        mcp: McpEndpoint {
            shim_path: "/Apps/OpenAGC.app/Contents/MacOS/openagc-mcp".into(),
            socket_path: "/tmp/s.sock".into(),
        },
        system_prompt_file: d.join("prompt.md"),
        working_dir: d.to_owned(),
        model: None,
        max_turns: 40,
        max_budget_usd: None,
        resume: resume.map(str::to_owned),
        env: vec![],
    }
}

async fn until_done(rx: &mut mpsc::UnboundedReceiver<(SessionId, AgentEvent)>) -> Vec<AgentEvent> {
    let mut out = Vec::new();
    loop {
        let (_, e) = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.expect("events in time").unwrap();
        let end = matches!(e, AgentEvent::TurnCompleted { .. } | AgentEvent::TurnFailed { .. });
        out.push(e);
        if end {
            return out;
        }
    }
}

fn turn(text: &str) -> TurnInput {
    TurnInput { prompt: text.into(), context: PromptContext::default() }
}

fn received(d: &Path) -> Vec<Value> {
    std::fs::read_to_string(d.join("received.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[tokio::test]
async fn a_turn_maps_codex_notifications_and_refuses_approval_requests() {
    let d = setup("turn");
    let (tx, mut rx) = mpsc::unbounded_channel();
    let provider = CodexProvider::new(Locator::only(vec![d.clone()]));
    let mut session =
        provider.start_session(config(&d, None), EventSink::new(SessionId("agent-3".into()), tx)).await.unwrap();
    assert_eq!(rx.recv().await.unwrap().1, AgentEvent::SessionStarted { external_id: Some("thr_1".into()) });
    assert_eq!(session.external_id().as_deref(), Some("thr_1"));

    session.send(turn("anything unread?")).await.unwrap();
    let events = until_done(&mut rx).await;
    assert_eq!(
        events,
        vec![
            AgentEvent::TurnStarted,
            AgentEvent::ToolCallStarted {
                call_id: "call_1".into(),
                tool: "mail_search".into(),
                args_summary: "query: is:unread".into()
            },
            AgentEvent::ToolCallFinished { call_id: "call_1".into(), ok: true, summary: "{\"threads\":[]}".into() },
            AgentEvent::ThinkingDelta { text: "Checking.".into() },
            AgentEvent::TextDelta { text: "No unread ".into() },
            AgentEvent::TextDelta { text: "mail.".into() },
            AgentEvent::TurnCompleted {
                usage: Some(Usage { input_tokens: 50, output_tokens: 7, cached_input_tokens: 10 }),
                cost_usd: None,
            },
        ]
    );

    let argv: Vec<String> = serde_json::from_str(&std::fs::read_to_string(d.join("argv.json")).unwrap()).unwrap();
    assert_eq!(&argv[..3], ["app-server", "--listen", "stdio://"]);
    let joined = argv.join(" ");
    assert!(joined.contains(r#"mcp_servers={openagc={command="/Apps/OpenAGC.app/Contents/MacOS/openagc-mcp",args=["--socket","/tmp/s.sock","--session","agent-3"]"#), "{joined}");
    assert!(joined.contains(r#"sandbox_mode="read-only""#) && joined.contains(r#"approval_policy="never""#));
    assert!(joined.contains("--disable shell_tool") && joined.contains("--disable unified_exec"));

    let got = received(&d);
    assert_eq!(got[0]["method"], "initialize");
    assert!(got[0].get("jsonrpc").is_none(), "the protocol omits the jsonrpc field");
    assert_eq!(got[0]["params"]["clientInfo"]["name"], "openagc");
    assert_eq!(got[1]["method"], "initialized");
    assert_eq!(got[2]["method"], "thread/start");
    assert_eq!(got[2]["params"]["sandbox"], "read-only");
    assert_eq!(got[2]["params"]["approvalPolicy"], "never");
    assert_eq!(got[2]["params"]["developerInstructions"], "mail rules");
    assert_eq!(got[3]["method"], "turn/start");
    assert_eq!(got[3]["params"]["threadId"], "thr_1");
    let refusal = got.iter().find(|m| m["id"] == 900).expect("answered the approval request");
    assert_eq!(refusal["error"]["code"], -32601);
    session.close().await;
}

#[tokio::test]
async fn interrupt_resume_and_busy() {
    let d = setup("interrupt");
    let (tx, mut rx) = mpsc::unbounded_channel();
    let provider = CodexProvider::new(Locator::only(vec![d.clone()]));
    let mut session = provider
        .start_session(config(&d, Some("thr_old")), EventSink::new(SessionId("agent-3".into()), tx))
        .await
        .unwrap();
    assert_eq!(rx.recv().await.unwrap().1, AgentEvent::SessionStarted { external_id: Some("thr_old".into()) });

    session.send(turn("please hang")).await.unwrap();
    assert_eq!(session.send(turn("again")).await.unwrap_err(), AgentError::Busy);
    session.cancel().await.unwrap();
    let events = until_done(&mut rx).await;
    assert_eq!(events.last(), Some(&AgentEvent::TurnFailed { message: "The turn was stopped.".into() }));

    session.send(turn("fine")).await.unwrap();
    assert!(matches!(until_done(&mut rx).await.last(), Some(AgentEvent::TurnCompleted { .. })));
    let got = received(&d);
    assert!(got.iter().any(|m| m["method"] == "thread/resume" && m["params"]["threadId"] == "thr_old"));
    assert!(got.iter().any(|m| m["method"] == "turn/interrupt" && m["params"]["turnId"] == "turn_1"));
    session.close().await;
}
