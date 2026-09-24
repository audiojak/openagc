//! Agents in the core (spec §9, §10): the MCP socket agents' tool calls
//! arrive on, the per-session permission state, and the tools themselves.
// Reached from the agent FFI (oagc-doa); until then only tests call it.
#![allow(dead_code)]

mod tools;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock, Weak};

use agent_api::EventSink;
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
    /// Where `mail_present_threads` shows results; set by the agent manager.
    pub sink: Option<EventSink>,
}

#[derive(Default)]
pub(crate) struct AgentHub {
    sessions: Mutex<HashMap<String, ToolSession>>,
    socket: Mutex<Option<McpSocket>>,
    pub(crate) policy: RwLock<Policy>,
    pub(crate) text: RwLock<Option<Arc<dyn TextExtractor>>>,
}

impl AgentHub {
    pub(crate) fn register(&self, session: &str, scope: Scope, sink: Option<EventSink>) {
        let state = ToolSession { guard: SessionGuard::new(scope), sink };
        self.sessions.lock().unwrap_or_else(|e| e.into_inner()).insert(session.to_owned(), state);
    }

    pub(crate) fn unregister(&self, session: &str) {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner()).remove(session);
    }

    pub(crate) fn with_session<T>(&self, session: &str, f: impl FnOnce(&mut ToolSession) -> T) -> Option<T> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner()).get_mut(session).map(f)
    }

    fn has(&self, session: &str) -> bool {
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
    /// Register how the app extracts PDF text (PDFKit).
    pub fn set_text_extractor(&self, extractor: Arc<dyn TextExtractor>) {
        *self.agents.text.write().unwrap_or_else(|e| e.into_inner()) = Some(extractor);
    }
}

#[cfg(test)]
mod tests;
