//! Codex sessions (spec §9.4): a `codex app-server` per OpenAGC session,
//! JSON-RPC over stdio (newline-delimited, no `"jsonrpc"` field), one
//! Codex thread, one turn at a time.
//!
//! Per session rather than per launch: the MCP server's `--session`
//! binding is process-level configuration.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_api::process::{Locator, child_path};
use agent_api::{
    AgentError, AgentEvent, AgentResult, AgentSession, EventSink, SessionConfig, TurnInput, Usage, shorten,
    summarize_args,
};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::oneshot;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Features that would give the agent anything beyond OpenAGC's tools.
pub const DISABLED_FEATURES: &[&str] = &[
    "shell_tool",
    "unified_exec",
    "apps",
    "browser_use",
    "computer_use",
    "in_app_browser",
    "image_generation",
    "plugins",
    "multi_agent",
    "hooks",
    "tool_suggest",
    "goals",
    "skill_search",
];

/// Every argument for `codex app-server`. The only place Codex's flags
/// live. Verified against codex-cli 0.145 with `--strict-config`.
pub fn server_args(cfg: &SessionConfig) -> Vec<String> {
    let toml_str = |s: &Path| format!("\"{}\"", s.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\""));
    // Replacing the whole table drops the user's own MCP servers.
    let mcp = format!(
        "mcp_servers={{openagc={{command={},args=[\"--socket\",{},\"--session\",\"{}\"],\
         default_tools_approval_mode=\"auto\",tool_timeout_sec=900,startup_timeout_sec=20}}}}",
        toml_str(&cfg.mcp.shim_path),
        toml_str(&cfg.mcp.socket_path),
        cfg.session_id.as_str().replace('"', ""),
    );
    let mut args: Vec<String> = vec!["app-server".into(), "--listen".into(), "stdio://".into()];
    for c in [mcp.as_str(), "sandbox_mode=\"read-only\"", "approval_policy=\"never\"", "web_search=\"disabled\""] {
        args.extend(["-c".into(), c.into()]);
    }
    for f in DISABLED_FEATURES {
        args.extend(["--disable".into(), (*f).into()]);
    }
    args
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

struct TurnState {
    thread_id: String,
    turn_id: Option<String>,
    usage: Option<Usage>,
    last_error: Option<String>,
}

pub(crate) struct CodexSession {
    child: Child,
    stdin: Arc<tokio::sync::Mutex<ChildStdin>>,
    pending: Pending,
    next_id: AtomicU64,
    running: Arc<AtomicBool>,
    state: Arc<Mutex<TurnState>>,
    sink: EventSink,
}

pub(crate) async fn start(
    locator: &Locator,
    cfg: SessionConfig,
    sink: EventSink,
) -> AgentResult<Box<dyn AgentSession>> {
    let binary = locator.find("codex").ok_or(AgentError::NotInstalled("Codex"))?;
    Ok(Box::new(CodexSession::spawn(&binary, cfg, sink).await?))
}

async fn write_line(stdin: &tokio::sync::Mutex<ChildStdin>, message: &Value) -> Result<(), String> {
    let mut line = message.to_string();
    line.push('\n');
    let mut w = stdin.lock().await;
    w.write_all(line.as_bytes()).await.map_err(|e| e.to_string())?;
    w.flush().await.map_err(|e| e.to_string())
}

impl CodexSession {
    async fn spawn(binary: &PathBuf, cfg: SessionConfig, sink: EventSink) -> AgentResult<Self> {
        let mut child = Command::new(binary)
            .args(server_args(&cfg))
            .current_dir(&cfg.working_dir)
            .env("PATH", child_path())
            .envs(cfg.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| AgentError::Spawn(e.to_string()))?;
        let stdin = Arc::new(tokio::sync::Mutex::new(child.stdin.take().expect("piped stdin")));
        let stdout = child.stdout.take().expect("piped stdout");
        let pending: Pending = Arc::default();
        let running = Arc::new(AtomicBool::new(false));
        let state =
            Arc::new(Mutex::new(TurnState { thread_id: String::new(), turn_id: None, usage: None, last_error: None }));

        let reader = Reader {
            pending: pending.clone(),
            running: running.clone(),
            state: state.clone(),
            sink: sink.clone(),
            stdin: stdin.clone(),
        };
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                reader.handle(&line).await;
            }
            reader.closed();
        });

        let session = Self { child, stdin, pending, next_id: AtomicU64::new(1), running, state, sink };
        session
            .request(
                "initialize",
                json!({
                    "clientInfo": { "name": "openagc", "title": "OpenAGC", "version": env!("CARGO_PKG_VERSION") },
                    "capabilities": { "experimentalApi": true },
                }),
            )
            .await
            .map_err(AgentError::Protocol)?;
        write_line(&session.stdin, &json!({ "method": "initialized" })).await.map_err(AgentError::Protocol)?;

        let instructions = std::fs::read_to_string(&cfg.system_prompt_file).ok();
        let mut params = json!({
            "cwd": cfg.working_dir,
            "sandbox": "read-only",
            "approvalPolicy": "never",
        });
        if let Some(text) = instructions {
            params["developerInstructions"] = json!(text);
        }
        if let Some(model) = &cfg.model {
            params["model"] = json!(model);
        }
        let (method, params) = match &cfg.resume {
            Some(thread) => {
                params["threadId"] = json!(thread);
                ("thread/resume", params)
            }
            None => {
                params["ephemeral"] = json!(false);
                ("thread/start", params)
            }
        };
        let result = session.request(method, params).await.map_err(AgentError::Protocol)?;
        let thread_id = result["thread"]["id"]
            .as_str()
            .ok_or_else(|| AgentError::Protocol("Codex did not return a thread id".into()))?
            .to_owned();
        session.state.lock().unwrap_or_else(|e| e.into_inner()).thread_id = thread_id.clone();
        session.sink.emit(AgentEvent::SessionStarted { external_id: Some(thread_id) });
        Ok(session)
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).insert(id, tx);
        write_line(&self.stdin, &json!({ "id": id, "method": method, "params": params })).await?;
        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("Codex stopped".into()),
            Err(_) => Err(format!("Codex did not answer {method}")),
        }
    }

    fn thread_id(&self) -> String {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).thread_id.clone()
    }
}

#[async_trait]
impl AgentSession for CodexSession {
    async fn send(&mut self, turn: TurnInput) -> AgentResult<()> {
        if self.running.swap(true, Ordering::SeqCst) {
            return Err(AgentError::Busy);
        }
        {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            s.usage = None;
            s.last_error = None;
            s.turn_id = None;
        }
        self.sink.emit(AgentEvent::TurnStarted);
        let params = json!({
            "threadId": self.thread_id(),
            "input": [{ "type": "text", "text": turn.full_prompt() }],
        });
        match self.request("turn/start", params).await {
            Ok(result) => {
                if let Some(id) = result["turn"]["id"].as_str() {
                    self.state.lock().unwrap_or_else(|e| e.into_inner()).turn_id = Some(id.to_owned());
                }
                Ok(())
            }
            Err(e) => {
                self.running.store(false, Ordering::SeqCst);
                self.sink.emit(AgentEvent::TurnFailed { message: e.clone() });
                Err(AgentError::Protocol(e))
            }
        }
    }

    async fn cancel(&mut self) -> AgentResult<()> {
        if !self.running.load(Ordering::SeqCst) {
            return Ok(());
        }
        let turn_id = self.state.lock().unwrap_or_else(|e| e.into_inner()).turn_id.clone();
        if let Some(turn_id) = turn_id {
            self.request("turn/interrupt", json!({ "threadId": self.thread_id(), "turnId": turn_id }))
                .await
                .map_err(AgentError::Protocol)?;
        }
        Ok(())
    }

    fn external_id(&self) -> Option<String> {
        let id = self.thread_id();
        (!id.is_empty()).then_some(id)
    }

    async fn close(&mut self) {
        let _ = self.cancel().await;
        let _ = self.child.start_kill();
        self.sink.emit(AgentEvent::SessionEnded);
    }
}

struct Reader {
    pending: Pending,
    running: Arc<AtomicBool>,
    state: Arc<Mutex<TurnState>>,
    sink: EventSink,
    stdin: Arc<tokio::sync::Mutex<ChildStdin>>,
}

impl Reader {
    async fn handle(&self, line: &str) {
        let Ok(msg) = serde_json::from_str::<Value>(line) else { return };
        let method = msg["method"].as_str();
        match (msg.get("id"), method) {
            // A response to one of our requests.
            (Some(id), None) => {
                let Some(id) = id.as_u64() else { return };
                let waiter = self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
                if let Some(w) = waiter {
                    let result = match msg.get("error") {
                        Some(e) => Err(e["message"].as_str().unwrap_or("Codex returned an error").to_owned()),
                        None => Ok(msg["result"].clone()),
                    };
                    let _ = w.send(result);
                }
            }
            // A request from Codex (approvals, elicitations): OpenAGC's own
            // permission engine gates inside the tools, so none are granted.
            (Some(id), Some(method)) => {
                tracing::warn!(method, "refused a Codex server request");
                let reply = json!({ "id": id, "error": { "code": -32601, "message": "OpenAGC does not allow this" } });
                let _ = write_line(&self.stdin, &reply).await;
            }
            (None, Some(method)) => self.notification(method, &msg["params"]),
            (None, None) => {}
        }
    }

    fn notification(&self, method: &str, p: &Value) {
        {
            let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if p["threadId"].as_str().is_some_and(|t| !s.thread_id.is_empty() && t != s.thread_id) {
                return;
            }
        }
        match method {
            "item/agentMessage/delta" => {
                if let Some(text) = p["delta"].as_str().filter(|t| !t.is_empty()) {
                    self.sink.emit(AgentEvent::TextDelta { text: text.to_owned() });
                }
            }
            "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => {
                if let Some(text) = p["delta"].as_str().filter(|t| !t.is_empty()) {
                    self.sink.emit(AgentEvent::ThinkingDelta { text: text.to_owned() });
                }
            }
            "item/started" if p["item"]["type"] == "mcpToolCall" => {
                let item = &p["item"];
                self.sink.emit(AgentEvent::ToolCallStarted {
                    call_id: item["id"].as_str().unwrap_or_default().to_owned(),
                    tool: item["tool"].as_str().unwrap_or_default().to_owned(),
                    args_summary: summarize_args(&item["arguments"]),
                });
            }
            "item/completed" if p["item"]["type"] == "mcpToolCall" => {
                let item = &p["item"];
                let ok = item["status"] == "completed" && item["error"].is_null();
                let summary = if ok {
                    item["result"]["content"]
                        .as_array()
                        .map(|parts| parts.iter().filter_map(|c| c["text"].as_str()).collect::<Vec<_>>().join(" "))
                        .unwrap_or_default()
                } else {
                    item["error"]["message"].as_str().unwrap_or("failed").to_owned()
                };
                self.sink.emit(AgentEvent::ToolCallFinished {
                    call_id: item["id"].as_str().unwrap_or_default().to_owned(),
                    ok,
                    summary: shorten(&summary, 160),
                });
            }
            "thread/tokenUsage/updated" => {
                let last = &p["tokenUsage"]["last"];
                self.state.lock().unwrap_or_else(|e| e.into_inner()).usage = Some(Usage {
                    input_tokens: last["inputTokens"].as_u64().unwrap_or(0),
                    output_tokens: last["outputTokens"].as_u64().unwrap_or(0),
                    cached_input_tokens: last["cachedInputTokens"].as_u64().unwrap_or(0),
                });
            }
            "error" if p["willRetry"] == false => {
                let message = p["error"]["message"].as_str().unwrap_or("Codex reported an error").to_owned();
                self.state.lock().unwrap_or_else(|e| e.into_inner()).last_error = Some(message);
            }
            "turn/completed" => {
                let turn = &p["turn"];
                let (usage, error) = {
                    let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    (s.usage, s.last_error.clone())
                };
                let event = match turn["status"].as_str() {
                    Some("completed") => AgentEvent::TurnCompleted { usage, cost_usd: None },
                    Some("interrupted") => AgentEvent::TurnFailed { message: "The turn was stopped.".into() },
                    _ => AgentEvent::TurnFailed {
                        message: turn["error"]["message"]
                            .as_str()
                            .map(str::to_owned)
                            .or(error)
                            .unwrap_or_else(|| "Codex stopped with an error.".into()),
                    },
                };
                self.running.store(false, Ordering::SeqCst);
                self.sink.emit(event);
            }
            _ => {}
        }
    }

    /// The app-server exited: fail what is waiting.
    fn closed(&self) {
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).clear();
        if self.running.swap(false, Ordering::SeqCst) {
            self.sink.emit(AgentEvent::TurnFailed { message: "Codex stopped unexpectedly.".into() });
        }
    }
}
