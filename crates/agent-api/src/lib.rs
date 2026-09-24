//! The agent abstraction (spec §9.1, §9.5): providers that run a coding
//! agent CLI (Claude Code, Codex) as a mail assistant, their sessions, the
//! events they produce, and the manager that owns live sessions.
//!
//! Nothing outside `agent-claude` / `agent-codex` knows a CLI flag. The only
//! tools an agent gets are OpenAGC's MCP tools (spec §10), reached through
//! the `openagc-mcp` shim described by [`McpEndpoint`].

pub mod fake;
mod manager;
pub mod process;

use std::path::PathBuf;

use async_trait::async_trait;
use mail_domain::ThreadId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;

pub use manager::{AgentManager, SessionId};

/// Which agent CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderId {
    ClaudeCode,
    Codex,
}

impl ProviderId {
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderId::ClaudeCode => "claude-code",
            ProviderId::Codex => "codex",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            ProviderId::ClaudeCode => "Claude",
            ProviderId::Codex => "Codex",
        }
    }
}

/// What detection found (spec §9.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum AgentStatus {
    Ready { version: String, path: PathBuf },
    NotInstalled,
    NotAuthenticated { version: String, path: PathBuf },
    UpdateRequired { version: String, minimum: String, path: PathBuf },
    Error { message: String },
}

impl AgentStatus {
    pub fn is_ready(&self) -> bool {
        matches!(self, AgentStatus::Ready { .. })
    }
}

/// How the agent reaches OpenAGC's tools: the shim binary, the core's
/// per-launch socket, and the session the calls are bound to (spec §10.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpEndpoint {
    pub shim_path: PathBuf,
    pub socket_path: PathBuf,
}

/// Everything a provider needs to start a session.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionConfig {
    /// OpenAGC's id for the session; tool calls carry it.
    pub session_id: SessionId,
    pub mcp: McpEndpoint,
    /// The bundled system-prompt addendum (spec §9.6).
    pub system_prompt_file: PathBuf,
    /// A private, empty working directory for the CLI.
    pub working_dir: PathBuf,
    /// `None` uses the CLI's own default model (no model picker, §20).
    pub model: Option<String>,
    pub max_turns: u32,
    /// Where the provider supports it.
    pub max_budget_usd: Option<f64>,
    /// The provider's id for a session to continue (Claude session id,
    /// Codex thread id).
    pub resume: Option<String>,
    /// Extra environment, e.g. an `ANTHROPIC_API_KEY` from the Keychain.
    pub env: Vec<(String, String)>,
}

impl SessionConfig {
    pub const DEFAULT_MAX_TURNS: u32 = 40;
}

/// What the user is looking at, sent with a prompt as references only; the
/// agent reads content through tools (spec §9.7).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptContext {
    pub mailbox: Option<String>,
    pub selected_thread_ids: Vec<ThreadId>,
    pub search_query: Option<String>,
}

impl PromptContext {
    /// A short preamble for the prompt. Ids and a query, never mail text.
    pub fn render(&self) -> Option<String> {
        let mut lines = Vec::new();
        if let Some(m) = &self.mailbox {
            lines.push(format!("Current mailbox: {m}"));
        }
        if !self.selected_thread_ids.is_empty() {
            let ids: Vec<&str> = self.selected_thread_ids.iter().map(ThreadId::as_str).collect();
            lines.push(format!("Selected thread ids: {}", ids.join(", ")));
        }
        if let Some(q) = self.search_query.as_deref().filter(|q| !q.is_empty()) {
            lines.push(format!("Current search: {q}"));
        }
        (!lines.is_empty()).then(|| format!("[OpenAGC context]\n{}\n[/OpenAGC context]", lines.join("\n")))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnInput {
    pub prompt: String,
    pub context: PromptContext,
}

impl TurnInput {
    /// The text sent to the CLI: context preamble, then the user's words.
    pub fn full_prompt(&self) -> String {
        match self.context.render() {
            Some(ctx) => format!("{ctx}\n\n{}", self.prompt),
            None => self.prompt.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
}

/// What a session reports (spec §9.5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum AgentEvent {
    SessionStarted {
        external_id: Option<String>,
    },
    TurnStarted,
    TextDelta {
        text: String,
    },
    /// Shown collapsed.
    ThinkingDelta {
        text: String,
    },
    ToolCallStarted {
        call_id: String,
        tool: String,
        args_summary: String,
    },
    ToolCallFinished {
        call_id: String,
        ok: bool,
        summary: String,
    },
    /// A gated action awaiting the user (spec §10.4).
    ActionProposed {
        action_id: i64,
        tool: String,
        summary: String,
    },
    /// Threads the agent wants shown as a list.
    ResultsAvailable {
        thread_ids: Vec<ThreadId>,
    },
    TurnCompleted {
        usage: Option<Usage>,
        cost_usd: Option<f64>,
    },
    TurnFailed {
        message: String,
    },
    SessionEnded,
}

/// Merge adjacent text (and thinking) deltas: fewer, larger strings for
/// the UI and the stored transcript.
pub fn coalesce(events: Vec<AgentEvent>) -> Vec<AgentEvent> {
    let mut out: Vec<AgentEvent> = Vec::with_capacity(events.len());
    for e in events {
        match (out.last_mut(), e) {
            (Some(AgentEvent::TextDelta { text }), AgentEvent::TextDelta { text: more }) => text.push_str(&more),
            (Some(AgentEvent::ThinkingDelta { text }), AgentEvent::ThinkingDelta { text: more }) => {
                text.push_str(&more)
            }
            (_, e) => out.push(e),
        }
    }
    out
}

/// Where a session sends its events. Cheap to clone.
#[derive(Debug, Clone)]
pub struct EventSink {
    session: SessionId,
    tx: mpsc::UnboundedSender<(SessionId, AgentEvent)>,
}

impl EventSink {
    pub fn new(session: SessionId, tx: mpsc::UnboundedSender<(SessionId, AgentEvent)>) -> Self {
        Self { session, tx }
    }

    pub fn emit(&self, event: AgentEvent) {
        // The receiver is gone only when the app is shutting down.
        let _ = self.tx.send((self.session.clone(), event));
    }

    pub fn session(&self) -> &SessionId {
        &self.session
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AgentError {
    #[error("{0} is not installed")]
    NotInstalled(&'static str),
    #[error("{0} is not signed in")]
    NotAuthenticated(&'static str),
    #[error("no such agent session")]
    UnknownSession,
    #[error("the agent is already working on a prompt")]
    Busy,
    #[error("the agent could not start: {0}")]
    Spawn(String),
    #[error("the agent stopped unexpectedly: {0}")]
    Protocol(String),
    #[error("{0}")]
    Other(String),
}

pub type AgentResult<T> = Result<T, AgentError>;

#[async_trait]
pub trait AgentProvider: Send + Sync {
    fn id(&self) -> ProviderId;
    async fn detect(&self) -> AgentStatus;
    async fn start_session(&self, cfg: SessionConfig, sink: EventSink) -> AgentResult<Box<dyn AgentSession>>;
}

/// A conversation with an agent. One turn runs at a time; `send` returns
/// once the turn has started, and its events arrive through the sink.
#[async_trait]
pub trait AgentSession: Send + Sync {
    async fn send(&mut self, turn: TurnInput) -> AgentResult<()>;
    async fn cancel(&mut self) -> AgentResult<()>;
    /// The provider's id for resuming, once known.
    fn external_id(&self) -> Option<String>;
    /// Stop for good (app quit, session closed).
    async fn close(&mut self);
}

/// A short, single-line rendering of a tool's arguments for the transcript.
pub fn summarize_args(input: &Value) -> String {
    let text = match input {
        Value::Object(map) if map.is_empty() => String::new(),
        Value::Object(map) => map
            .iter()
            .map(|(k, v)| match v {
                Value::String(s) => format!("{k}: {s}"),
                Value::Array(a) => format!("{k}: {} item{}", a.len(), if a.len() == 1 { "" } else { "s" }),
                other => format!("{k}: {other}"),
            })
            .collect::<Vec<_>>()
            .join(", "),
        other => other.to_string(),
    };
    shorten(&text, 120)
}

pub fn shorten(text: &str, max: usize) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match one_line.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", &one_line[..cut]),
        None => one_line,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_is_references_only() {
        let turn = TurnInput {
            prompt: "What needs a reply?".into(),
            context: PromptContext {
                mailbox: Some("INBOX".into()),
                selected_thread_ids: vec![ThreadId::new("t1"), ThreadId::new("t2")],
                search_query: Some("from:alex".into()),
            },
        };
        assert_eq!(
            turn.full_prompt(),
            "[OpenAGC context]\nCurrent mailbox: INBOX\nSelected thread ids: t1, t2\nCurrent search: from:alex\n\
             [/OpenAGC context]\n\nWhat needs a reply?"
        );
        let bare = TurnInput { prompt: "hi".into(), context: PromptContext::default() };
        assert_eq!(bare.full_prompt(), "hi");
    }

    #[test]
    fn deltas_coalesce() {
        let merged = coalesce(vec![
            AgentEvent::TurnStarted,
            AgentEvent::TextDelta { text: "Hel".into() },
            AgentEvent::TextDelta { text: "lo".into() },
            AgentEvent::ThinkingDelta { text: "a".into() },
            AgentEvent::ThinkingDelta { text: "b".into() },
            AgentEvent::TextDelta { text: "!".into() },
        ]);
        assert_eq!(
            merged,
            vec![
                AgentEvent::TurnStarted,
                AgentEvent::TextDelta { text: "Hello".into() },
                AgentEvent::ThinkingDelta { text: "ab".into() },
                AgentEvent::TextDelta { text: "!".into() },
            ]
        );
    }

    #[test]
    fn events_serialize_tagged() {
        let e = AgentEvent::TextDelta { text: "Hi".into() };
        let json = serde_json::to_string(&e).unwrap();
        assert_eq!(json, r#"{"type":"text_delta","text":"Hi"}"#);
    }
}
