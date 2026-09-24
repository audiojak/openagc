//! The real shim binary, driven over MCP stdio against a test socket.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_mcp::{McpSocket, Outcome, ToolHandler};
use async_trait::async_trait;
use permissions::Tool;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

#[derive(Default)]
struct Recorder {
    calls: Mutex<Vec<(String, Tool, Value)>>,
}

#[async_trait]
impl ToolHandler for Recorder {
    fn has_session(&self, session: &str) -> bool {
        session == "s-1"
    }

    async fn call(&self, session: &str, tool: Tool, arguments: Value) -> Outcome {
        self.calls.lock().unwrap().push((session.to_owned(), tool, arguments.clone()));
        match tool {
            Tool::Search => Outcome::json(json!({ "threads": [{ "id": "t1", "subject": "Hello" }] })),
            Tool::Send => Outcome::error("rejected_by_user", "The user declined."),
            _ => Outcome::Ok { text: "done".into(), structured: None },
        }
    }
}

struct Mcp {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Mcp {
    fn spawn(socket: &std::path::Path, session: &str) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_openagc-mcp"))
            .args(["--socket", socket.to_str().unwrap(), "--session", session])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self { child, stdin, stdout }
    }

    async fn send(&mut self, message: Value) {
        self.stdin.write_all(format!("{message}\n").as_bytes()).await.unwrap();
    }

    async fn response(&mut self, id: u64) -> Value {
        loop {
            let mut line = String::new();
            let n = tokio::time::timeout(Duration::from_secs(10), self.stdout.read_line(&mut line))
                .await
                .expect("shim answered in time")
                .unwrap();
            assert!(n > 0, "shim closed stdout");
            let v: Value = serde_json::from_str(&line).unwrap();
            if v["id"] == id {
                return v;
            }
        }
    }

    async fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })).await;
        self.response(id).await
    }
}

#[tokio::test]
async fn the_shim_lists_the_catalog_and_forwards_calls_with_its_session() {
    let dir = std::env::temp_dir().join(format!("oagc-shim-{}", std::process::id()));
    let recorder = Arc::new(Recorder::default());
    let socket = McpSocket::bind(&dir, recorder.clone()).unwrap();
    let meta = std::fs::metadata(socket.path()).unwrap();
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(meta.permissions().mode() & 0o777, 0o600, "only the user can connect");

    let mut mcp = Mcp::spawn(socket.path(), "s-1");
    let init = mcp
        .request(
            1,
            "initialize",
            json!({ "protocolVersion": "2025-06-18", "capabilities": {},
                    "clientInfo": { "name": "test", "version": "0" } }),
        )
        .await;
    assert_eq!(init["result"]["serverInfo"]["name"], "openagc");
    mcp.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).await;

    let list = mcp.request(2, "tools/list", json!({})).await;
    let tools = list["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 17);
    let search = tools.iter().find(|t| t["name"] == "mail_search").unwrap();
    assert_eq!(search["inputSchema"]["required"], json!(["query"]));
    assert_eq!(search["annotations"]["readOnlyHint"], true);

    let found =
        mcp.request(3, "tools/call", json!({ "name": "mail_search", "arguments": { "query": "from:alex" } })).await;
    assert_eq!(found["result"]["structuredContent"]["threads"][0]["id"], "t1");
    assert_eq!(found["result"]["isError"], false);

    let declined = mcp.request(4, "tools/call", json!({ "name": "mail_send", "arguments": { "draft_id": 3 } })).await;
    assert_eq!(declined["result"]["isError"], true);
    assert!(declined["result"]["content"][0]["text"].as_str().unwrap().starts_with("rejected_by_user"));

    let unknown = mcp.request(5, "tools/call", json!({ "name": "rm_rf", "arguments": {} })).await;
    assert!(unknown["result"]["content"][0]["text"].as_str().unwrap().starts_with("unknown_tool"));

    let calls = recorder.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 2, "unknown tools never reach the handler");
    assert_eq!(calls[0], ("s-1".to_owned(), Tool::Search, json!({ "query": "from:alex" })));
    assert_eq!(calls[1].1, Tool::Send);
    drop(mcp);
}

#[tokio::test]
async fn unknown_sessions_are_refused_and_the_shim_exits() {
    let dir = std::env::temp_dir().join(format!("oagc-shim-refuse-{}", std::process::id()));
    let socket = McpSocket::bind(&dir, Arc::new(Recorder::default())).unwrap();
    let mut mcp = Mcp::spawn(socket.path(), "someone-else");
    let status = tokio::time::timeout(Duration::from_secs(10), mcp.child.wait()).await.unwrap().unwrap();
    assert!(!status.success());
    let mut err = String::new();
    use tokio::io::AsyncReadExt;
    mcp.child.stderr.take().unwrap().read_to_string(&mut err).await.unwrap();
    assert!(err.contains("unknown agent session"), "{err}");
}

#[tokio::test]
async fn calls_fail_cleanly_when_the_app_goes_away() {
    let dir = std::env::temp_dir().join(format!("oagc-shim-gone-{}", std::process::id()));
    let socket = McpSocket::bind(&dir, Arc::new(Recorder::default())).unwrap();
    let client = agent_mcp::ShimClient::connect(socket.path(), "s-1").await.unwrap();
    drop(socket);
    // The accept loop is gone; existing connections close with it shortly.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let outcome = tokio::time::timeout(Duration::from_secs(5), client.call("mail_search", json!({"query": ""}))).await;
    match outcome {
        Ok(Outcome::Error { code, .. }) => assert_eq!(code, "app_unavailable"),
        other => panic!("expected app_unavailable, got {other:?}"),
    }
}
