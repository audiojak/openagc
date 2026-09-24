//! A Claude session against fake `claude` scripts.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_api::process::Locator;
use agent_api::{
    AgentError, AgentEvent, AgentProvider, EventSink, McpEndpoint, PromptContext, SessionConfig, SessionId, TurnInput,
};
use agent_claude::ClaudeProvider;
use tokio::sync::mpsc;

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("oagc-claude-session-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn install(dir: &Path, body: &str) {
    let path = dir.join("claude");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn config(dir: &Path) -> SessionConfig {
    let prompt = dir.join("prompt.md");
    std::fs::write(&prompt, "be careful").unwrap();
    SessionConfig {
        session_id: SessionId("agent-7".into()),
        mcp: McpEndpoint {
            shim_path: "/Apps/OpenAGC.app/Contents/MacOS/openagc-mcp".into(),
            socket_path: "/tmp/s.sock".into(),
        },
        system_prompt_file: prompt,
        working_dir: dir.to_owned(),
        model: None,
        max_turns: 40,
        max_budget_usd: None,
        resume: None,
        env: vec![("ANTHROPIC_API_KEY".into(), "sk-test".into())],
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

fn turn(prompt: &str) -> TurnInput {
    TurnInput { prompt: prompt.into(), context: PromptContext::default() }
}

#[tokio::test]
async fn turns_run_the_cli_with_locked_down_flags_and_resume() {
    let d = dir("turns");
    let log = d.join("args.log");
    // Record each invocation's arguments and environment, then reply.
    install(
        &d,
        &format!(
            r#"for a in "$@"; do printf '%s\n' "$a"; done > "{log}.$$"
echo "KEY=$ANTHROPIC_API_KEY TIMEOUT=$MCP_TOOL_TIMEOUT NESTED=$CLAUDECODE" >> "{log}.$$"
mv "{log}.$$" "{log}.$(ls {dir} | grep -c args.log)"
echo '{{"type":"system","subtype":"init","session_id":"sess-42","mcp_servers":[{{"name":"openagc","status":"connected"}}]}}'
echo '{{"type":"stream_event","event":{{"type":"content_block_delta","delta":{{"type":"text_delta","text":"Hi"}}}}}}'
echo '{{"type":"result","subtype":"success","is_error":false,"session_id":"sess-42","usage":{{"input_tokens":3,"output_tokens":1}}}}'"#,
            log = log.display(),
            dir = d.display()
        ),
    );
    let (tx, mut rx) = mpsc::unbounded_channel();
    let provider = ClaudeProvider::new(Locator::only(vec![d.clone()]));
    let sink = EventSink::new(SessionId("agent-7".into()), tx);
    let mut session = provider.start_session(config(&d), sink).await.unwrap();
    assert_eq!(rx.recv().await.unwrap().1, AgentEvent::SessionStarted { external_id: None });

    session.send(turn("first")).await.unwrap();
    let events = until_done(&mut rx).await;
    assert_eq!(events[0], AgentEvent::TurnStarted);
    assert!(events.contains(&AgentEvent::SessionStarted { external_id: Some("sess-42".into()) }));
    assert!(events.contains(&AgentEvent::TextDelta { text: "Hi".into() }));
    assert_eq!(session.external_id().as_deref(), Some("sess-42"));

    session.send(turn("second")).await.unwrap();
    until_done(&mut rx).await;

    // Numbered by count including the in-progress temp file: .1 then .2.
    let first = std::fs::read_to_string(format!("{}.1", log.display())).unwrap();
    let args: Vec<&str> = first.lines().collect();
    let after = |flag: &str| args.iter().position(|a| *a == flag).map(|i| args[i + 1]);
    assert_eq!(after("-p"), Some("first"));
    assert_eq!(after("--output-format"), Some("stream-json"));
    assert_eq!(after("--tools"), Some(""), "no built-in tools");
    assert_eq!(after("--allowedTools"), Some("mcp__openagc__*"));
    assert_eq!(after("--permission-mode"), Some("dontAsk"));
    assert!(args.contains(&"--strict-mcp-config"));
    let mcp: serde_json::Value = serde_json::from_str(after("--mcp-config").unwrap()).unwrap();
    assert_eq!(
        mcp["mcpServers"]["openagc"]["args"],
        serde_json::json!(["--socket", "/tmp/s.sock", "--session", "agent-7"])
    );
    assert!(after("--append-system-prompt-file").unwrap().ends_with("prompt.md"));
    assert!(!args.contains(&"--resume"), "a new session");
    assert!(first.contains("KEY=sk-test TIMEOUT=900000 NESTED="), "{first}");

    let second = std::fs::read_to_string(format!("{}.2", log.display())).unwrap();
    let args: Vec<&str> = second.lines().collect();
    let i = args.iter().position(|a| *a == "--resume").expect("second turn resumes");
    assert_eq!(args[i + 1], "sess-42");
}

#[tokio::test]
async fn cancel_interrupts_and_a_busy_session_refuses_a_second_prompt() {
    let d = dir("cancel");
    install(
        &d,
        r#"trap 'echo "{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true,\"result\":\"Interrupted by user\",\"session_id\":\"s\"}"; exit 130' INT
echo '{"type":"system","subtype":"init","session_id":"s","mcp_servers":[{"name":"openagc","status":"connected"}]}'
while true; do sleep 0.05; done"#,
    );
    let (tx, mut rx) = mpsc::unbounded_channel();
    let provider = ClaudeProvider::new(Locator::only(vec![d.clone()]));
    let mut session = provider.start_session(config(&d), EventSink::new(SessionId("a".into()), tx)).await.unwrap();
    session.send(turn("long")).await.unwrap();
    // Wait until the CLI is running.
    loop {
        let (_, e) = rx.recv().await.unwrap();
        if matches!(e, AgentEvent::SessionStarted { external_id: Some(_) }) {
            break;
        }
    }
    assert_eq!(session.send(turn("again")).await.unwrap_err(), AgentError::Busy);
    session.cancel().await.unwrap();
    let events = until_done(&mut rx).await;
    assert_eq!(events.last(), Some(&AgentEvent::TurnFailed { message: "Interrupted by user".into() }));
    // The session is usable again.
    install(&d, r#"echo '{"type":"result","subtype":"success","is_error":false,"session_id":"s"}'"#);
    session.send(turn("next")).await.unwrap();
    assert!(matches!(until_done(&mut rx).await.last(), Some(AgentEvent::TurnCompleted { .. })));
}

#[tokio::test]
async fn a_cli_that_dies_reports_its_error() {
    let d = dir("dies");
    install(&d, "echo 'Error: something broke' >&2\nexit 3");
    let (tx, mut rx) = mpsc::unbounded_channel();
    let provider = ClaudeProvider::new(Locator::only(vec![d.clone()]));
    let mut session = provider.start_session(config(&d), EventSink::new(SessionId("a".into()), tx)).await.unwrap();
    session.send(turn("x")).await.unwrap();
    let events = until_done(&mut rx).await;
    assert_eq!(events.last(), Some(&AgentEvent::TurnFailed { message: "Error: something broke".into() }));

    let missing = ClaudeProvider::new(Locator::only(vec![dir("empty")]));
    let (tx, _rx) = mpsc::unbounded_channel();
    assert!(matches!(
        missing.start_session(config(&d), EventSink::new(SessionId("b".into()), tx)).await.err(),
        Some(AgentError::NotInstalled(_))
    ));
}
