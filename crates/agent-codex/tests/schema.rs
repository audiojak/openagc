//! The requests the adapter sends match the checked-in Codex schema: every
//! required field present, and no property the schema does not know.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use agent_api::process::Locator;
use agent_api::{
    AgentEvent, AgentProvider, EventSink, McpEndpoint, PromptContext, SessionConfig, SessionId, TurnInput,
};
use agent_codex::CodexProvider;
use serde_json::Value;
use tokio::sync::mpsc;

fn schema(file: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("schema/0.145.0").join(file);
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn conforms(params: &Value, schema: &Value, what: &str) {
    let props = schema["properties"].as_object().unwrap();
    for required in schema["required"].as_array().into_iter().flatten() {
        assert!(params.get(required.as_str().unwrap()).is_some(), "{what}: missing required {required}");
    }
    for key in params.as_object().unwrap().keys() {
        assert!(props.contains_key(key), "{what}: {key} is not in the schema");
    }
}

#[tokio::test]
async fn requests_match_the_codex_schema() {
    let d = std::env::temp_dir().join(format!("oagc-codex-schema-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::copy(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake_app_server.py"), d.join("codex")).unwrap();
    std::fs::set_permissions(d.join("codex"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(d.join("prompt.md"), "x").unwrap();
    let cfg = SessionConfig {
        session_id: SessionId("s".into()),
        mcp: McpEndpoint { shim_path: "/x".into(), socket_path: "/y".into() },
        system_prompt_file: d.join("prompt.md"),
        working_dir: d.clone(),
        model: Some("gpt-5.5".into()),
        max_turns: 40,
        max_budget_usd: None,
        resume: None,
        env: vec![],
    };
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut session = CodexProvider::new(Locator::only(vec![d.clone()]))
        .start_session(cfg, EventSink::new(SessionId("s".into()), tx))
        .await
        .unwrap();
    session.send(TurnInput { prompt: "hang".into(), context: PromptContext::default() }).await.unwrap();
    session.cancel().await.unwrap();
    loop {
        let (_, e) = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.unwrap().unwrap();
        if matches!(e, AgentEvent::TurnFailed { .. }) {
            break;
        }
    }
    session.close().await;

    let received: Vec<Value> = std::fs::read_to_string(d.join("received.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let by_method =
        |m: &str| received.iter().find(|r| r["method"] == m).unwrap_or_else(|| panic!("no {m}"))["params"].clone();
    conforms(&by_method("initialize"), &schema("v1/InitializeParams.json"), "initialize");
    conforms(&by_method("thread/start"), &schema("v2/ThreadStartParams.json"), "thread/start");
    conforms(&by_method("turn/start"), &schema("v2/TurnStartParams.json"), "turn/start");
    conforms(&by_method("turn/interrupt"), &schema("v2/TurnInterruptParams.json"), "turn/interrupt");
    let notifications = schema("ClientNotification.json");
    let methods: Vec<&str> = notifications["oneOf"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v["properties"]["method"]["enum"][0].as_str())
        .collect();
    assert!(methods.contains(&"initialized"));
}
