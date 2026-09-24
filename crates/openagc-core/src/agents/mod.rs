//! Agents in the core (spec §9, §10): the MCP socket agents' tool calls
//! arrive on, the per-session permission state, and the tools themselves.

mod approvals;
mod sessions;
mod tools;

pub use approvals::AgentActionInfo;

pub use sessions::{
    AgentEventInfo, AgentProviderInfo, AgentSessionInfo, AgentStatusInfo, AgentTranscriptItem, PromptContextInfo,
};

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};

use agent_api::{AgentProvider, EventSink};
use agent_mcp::{McpSocket, Outcome, ToolHandler};
use async_trait::async_trait;
use permissions::{Policy, Scope, SessionGuard, Tool};

use crate::{Core, CoreError, ErrorKind, runtime};

/// Text the app extracts for the core; PDFs go through PDFKit (spec §10.2).
#[uniffi::export(with_foreign)]
pub trait TextExtractor: Send + Sync {
    /// The text of the PDF at `path`, or `None` if it has none.
    fn pdf_text(&self, path: String) -> Option<String>;
}

/// One agent session's tool-side state.
pub(crate) struct ToolSession {
    pub guard: SessionGuard,
    /// The quoted original of reply drafts this session made, so updating
    /// the body keeps it.
    pub draft_quotes: HashMap<i64, String>,
    /// A routine preview: only read tools (spec §11.5 dry run).
    pub read_only: bool,
    /// Where `mail_present_threads` shows results; set by the agent manager.
    pub sink: Option<EventSink>,
}

#[derive(Default)]
pub(crate) struct AgentHub {
    sessions: Mutex<HashMap<String, ToolSession>>,
    socket: Mutex<Option<McpSocket>>,
    pub(crate) policy: RwLock<Policy>,
    pub(crate) text: RwLock<Option<Arc<dyn TextExtractor>>>,
    pub(crate) resources: RwLock<sessions::AgentResources>,
    pub(crate) runtime: OnceLock<sessions::AgentRuntime>,
    pub(crate) fake_providers: sessions::FakeProviders,
    pub(crate) approvals: approvals::Approvals,
    /// Agent sessions that are routine runs or previews.
    pub(crate) routine_sessions: Mutex<HashMap<String, crate::routines::RoutineSession>>,
    /// Finished previews, by session.
    pub(crate) previews: Mutex<HashMap<String, Vec<crate::routines::RoutinePreviewRow>>>,
    pub(crate) scheduler: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// The real agent adapters (spec §9.3, §9.4).
pub(crate) fn adapters() -> Vec<Arc<dyn AgentProvider>> {
    vec![agent_claude::ClaudeProvider::standard(), agent_codex::CodexProvider::standard()]
}

impl AgentHub {
    pub(crate) fn register(&self, session: &str, scope: Scope, sink: Option<EventSink>) {
        let state =
            ToolSession { guard: SessionGuard::new(scope), draft_quotes: HashMap::new(), read_only: false, sink };
        self.sessions.lock().unwrap_or_else(|e| e.into_inner()).insert(session.to_owned(), state);
    }

    pub(crate) fn unregister(&self, session: &str) {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner()).remove(session);
    }

    pub(crate) fn with_session<T>(&self, session: &str, f: impl FnOnce(&mut ToolSession) -> T) -> Option<T> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner()).get_mut(session).map(f)
    }

    pub(crate) fn has(&self, session: &str) -> bool {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner()).contains_key(session)
    }
}

/// Routes the socket's tool calls into the core.
struct ToolRouter {
    core: Weak<Core>,
}

#[async_trait]
impl ToolHandler for ToolRouter {
    fn has_session(&self, session: &str) -> bool {
        self.core.upgrade().is_some_and(|c| c.agents.has(session))
    }

    async fn call(&self, session: &str, tool: Tool, arguments: serde_json::Value) -> Outcome {
        match self.core.upgrade() {
            Some(core) => tools::call(&core, session, tool, arguments).await,
            None => Outcome::error("app_unavailable", "OpenAGC is shutting down"),
        }
    }
}

impl Core {
    /// The MCP socket, bound on first use under `<data_dir>/run`.
    pub(crate) fn mcp_socket_path(self: &Arc<Self>) -> Result<PathBuf, CoreError> {
        let mut socket = self.agents.socket.lock().unwrap_or_else(|e| e.into_inner());
        if socket.is_none() {
            let dir = PathBuf::from(&self.config.data_dir).join("run");
            let router = Arc::new(ToolRouter { core: Arc::downgrade(self) });
            let _entered = runtime::runtime().handle().enter();
            let bound = McpSocket::bind(&dir, router)
                .map_err(|e| CoreError::new(ErrorKind::Internal, format!("agent socket: {e}")))?;
            *socket = Some(bound);
        }
        Ok(socket.as_ref().map(|s| s.path().to_owned()).unwrap_or_default())
    }
}

#[uniffi::export]
impl Core {
    /// Reversible tools the user wants to approve one by one (spec §10.3).
    /// Read-only tools are always allowed and external ones always asked.
    pub fn set_agent_policy(&self, approve_tools: Vec<String>) -> Result<(), CoreError> {
        let mut policy = Policy::default();
        for name in approve_tools {
            let tool = Tool::from_name(&name)
                .ok_or_else(|| CoreError::new(ErrorKind::InvalidInput, format!("unknown tool {name}")))?;
            policy
                .set_requires_approval(tool, true)
                .map_err(|e| CoreError::new(ErrorKind::InvalidInput, e.to_string()))?;
        }
        *self.agents.policy.write().unwrap_or_else(|e| e.into_inner()) = policy;
        Ok(())
    }

    /// The reversible tools currently requiring approval.
    pub fn agent_policy(&self) -> Vec<String> {
        let policy = self.agents.policy.read().unwrap_or_else(|e| e.into_inner());
        policy.approve_reversible.iter().map(|t| t.name().to_owned()).collect()
    }

    /// Tools whose approval the user can choose, with their risk.
    pub fn configurable_agent_tools(&self) -> Vec<String> {
        Tool::ALL
            .into_iter()
            .filter(|t| t.risk() == permissions::Risk::Reversible)
            .map(|t| t.name().to_owned())
            .collect()
    }

    /// Register how the app extracts PDF text (PDFKit).
    pub fn set_text_extractor(&self, extractor: Arc<dyn TextExtractor>) {
        *self.agents.text.write().unwrap_or_else(|e| e.into_inner()) = Some(extractor);
    }
}

/// Test hook for other modules' tests.
#[cfg(test)]
pub(crate) async fn tools_call_for_tests(
    core: &Arc<Core>,
    session: &str,
    tool: Tool,
    arguments: serde_json::Value,
) -> Outcome {
    tools::call(core, session, tool, arguments).await
}

#[cfg(test)]
mod injection_tests;
#[cfg(test)]
mod tests;
