//! Claude Code sessions (spec §9.3): one `claude -p` subprocess per turn,
//! continued with `--resume <session_id>`. The CLI gets no built-in tools
//! and no MCP servers but OpenAGC's, and may only call those.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_api::process::{Locator, child_path};
use agent_api::{AgentError, AgentEvent, AgentResult, AgentSession, EventSink, SessionConfig, TurnInput};
use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::oneshot;

use crate::stream::StreamParser;

/// How long a cancelled turn gets to finish before it is killed.
const CANCEL_GRACE: Duration = Duration::from_secs(3);
/// MCP tool calls may wait on the user (approvals): 15 minutes.
const TOOL_TIMEOUT_MS: &str = "900000";

pub(crate) async fn start(
    locator: &Locator,
    cfg: SessionConfig,
    sink: EventSink,
) -> AgentResult<Box<dyn AgentSession>> {
    let binary = locator.find("claude").ok_or(AgentError::NotInstalled("Claude Code"))?;
    sink.emit(AgentEvent::SessionStarted { external_id: cfg.resume.clone() });
    Ok(Box::new(ClaudeSession {
        binary,
        external_id: Arc::new(Mutex::new(cfg.resume.clone())),
        cfg,
        sink,
        running: Arc::new(AtomicBool::new(false)),
        turn: None,
    }))
}

struct Turn {
    /// Set once the turn's `result` arrived; the process may still be exiting.
    answered: Arc<AtomicBool>,
    pid: Option<u32>,
    kill: Option<oneshot::Sender<()>>,
    done: Option<oneshot::Receiver<()>>,
}

pub(crate) struct ClaudeSession {
    binary: PathBuf,
    cfg: SessionConfig,
    sink: EventSink,
    external_id: Arc<Mutex<Option<String>>>,
    running: Arc<AtomicBool>,
    turn: Option<Turn>,
}

/// The `--mcp-config` value: OpenAGC's shim, bound to this session.
pub fn mcp_config(cfg: &SessionConfig) -> String {
    serde_json::json!({
        "mcpServers": {
            "openagc": {
                "type": "stdio",
                "command": cfg.mcp.shim_path,
                "args": ["--socket", cfg.mcp.socket_path, "--session", cfg.session_id.as_str()],
            }
        }
    })
    .to_string()
}

/// Every argument for one turn. The only place Claude's flags live.
pub fn turn_args(cfg: &SessionConfig, prompt: &str, resume: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = [
        "-p",
        prompt,
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--strict-mcp-config",
        "--mcp-config",
        &mcp_config(cfg),
        // No built-in tools: no shell, files or web.
        "--tools",
        "",
        "--allowedTools",
        "mcp__openagc__*",
        // Anything not pre-allowed is denied, never prompted.
        "--permission-mode",
        "dontAsk",
        "--max-turns",
        &cfg.max_turns.to_string(),
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if cfg.system_prompt_file.is_file() {
        args.push("--append-system-prompt-file".into());
        args.push(cfg.system_prompt_file.to_string_lossy().into_owned());
    }
    if let Some(model) = &cfg.model {
        args.extend(["--model".into(), model.clone()]);
    }
    if let Some(budget) = cfg.max_budget_usd {
        args.extend(["--max-budget-usd".into(), format!("{budget:.2}")]);
    }
    if let Some(id) = resume {
        args.extend(["--resume".into(), id.to_owned()]);
    }
    args
}

#[async_trait]
impl AgentSession for ClaudeSession {
    async fn send(&mut self, turn: TurnInput) -> AgentResult<()> {
        // The UI may send the next prompt the moment the last one completed,
        // while that process is still exiting: wait for it briefly.
        if let Some(previous) = self.turn.as_mut()
            && previous.answered.load(Ordering::SeqCst)
            && let Some(done) = previous.done.take()
        {
            let _ = tokio::time::timeout(CANCEL_GRACE, done).await;
        }
        if self.running.swap(true, Ordering::SeqCst) {
            return Err(AgentError::Busy);
        }
        let resume = self.external_id.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let args = turn_args(&self.cfg, &turn.full_prompt(), resume.as_deref());
        let mut cmd = Command::new(&self.binary);
        cmd.args(&args)
            .current_dir(&self.cfg.working_dir)
            .env("PATH", child_path())
            .env("MCP_TOOL_TIMEOUT", TOOL_TIMEOUT_MS)
            .envs(self.cfg.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Launched from inside a Claude Code session (development), these
        // make the child think it is nested and wait for its parent.
        for (key, _) in std::env::vars_os() {
            let k = key.to_string_lossy();
            if k.starts_with("CLAUDECODE") || k.starts_with("CLAUDE_CODE_") {
                cmd.env_remove(&key);
            }
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                self.running.store(false, Ordering::SeqCst);
                return Err(AgentError::Spawn(e.to_string()));
            }
        };
        self.sink.emit(AgentEvent::TurnStarted);
        let stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        let (kill_tx, kill_rx) = oneshot::channel();
        let (done_tx, done_rx) = oneshot::channel();
        let answered = Arc::new(AtomicBool::new(false));
        self.turn =
            Some(Turn { answered: answered.clone(), pid: child.id(), kill: Some(kill_tx), done: Some(done_rx) });

        let sink = self.sink.clone();
        let external = self.external_id.clone();
        let running = self.running.clone();
        tokio::spawn(async move {
            let mut parser = StreamParser::default();
            let mut lines = BufReader::new(stdout).lines();
            let read = async {
                while let Ok(Some(line)) = lines.next_line().await {
                    for event in parser.parse_line(&line) {
                        sink.emit(event);
                    }
                    if parser.finished() {
                        answered.store(true, Ordering::SeqCst);
                    }
                    if let Some(id) = parser.take_session_id() {
                        let mut known = external.lock().unwrap_or_else(|e| e.into_inner());
                        if known.as_deref() != Some(id.as_str()) {
                            *known = Some(id.clone());
                            sink.emit(AgentEvent::SessionStarted { external_id: Some(id) });
                        }
                    }
                }
            };
            let mut kill_rx = kill_rx;
            tokio::select! {
                () = read => {}
                _ = &mut kill_rx => {
                    let _ = child.start_kill();
                }
            }
            let status = child.wait().await.ok();
            let mut err = String::new();
            let _ = stderr.read_to_string(&mut err).await;
            if !parser.finished() {
                let message = err.lines().rev().find(|l| !l.trim().is_empty()).map(str::trim).unwrap_or("");
                let message = match (message, status.and_then(|s| s.code())) {
                    ("", Some(code)) => format!("Claude Code exited with status {code}"),
                    ("", None) => "The turn was stopped.".to_owned(),
                    (m, _) => m.to_owned(),
                };
                sink.emit(AgentEvent::TurnFailed { message });
            }
            running.store(false, Ordering::SeqCst);
            let _ = done_tx.send(());
        });
        Ok(())
    }

    /// SIGINT (Claude records the session so it can be resumed), then
    /// SIGKILL if the turn has not ended within 3 s.
    async fn cancel(&mut self) -> AgentResult<()> {
        let Some(mut turn) = self.turn.take() else { return Ok(()) };
        if !self.running.load(Ordering::SeqCst) {
            return Ok(());
        }
        if let Some(pid) = turn.pid {
            // No libc in this crate (unsafe is denied); kill(1) does it.
            let _ = Command::new("/bin/kill").args(["-INT", &pid.to_string()]).status().await;
        }
        let done = turn.done.take();
        let ended = match done {
            Some(done) => tokio::time::timeout(CANCEL_GRACE, done).await.is_ok(),
            None => true,
        };
        if !ended && let Some(kill) = turn.kill.take() {
            let _ = kill.send(());
        }
        Ok(())
    }

    fn external_id(&self) -> Option<String> {
        self.external_id.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    async fn close(&mut self) {
        let _ = self.cancel().await;
        self.sink.emit(AgentEvent::SessionEnded);
    }
}
