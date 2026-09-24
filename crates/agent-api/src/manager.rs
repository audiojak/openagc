//! Owns live agent sessions and routes prompts, cancels and events.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, mpsc};

use crate::{
    AgentError, AgentEvent, AgentProvider, AgentResult, AgentSession, AgentStatus, EventSink, ProviderId,
    SessionConfig, TurnInput,
};

/// OpenAGC's id for a session (not the provider's).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub String);

impl SessionId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

struct Live {
    provider: ProviderId,
    session: Mutex<Box<dyn AgentSession>>,
}

/// The registry of providers and the sessions started from them.
pub struct AgentManager {
    providers: Vec<Arc<dyn AgentProvider>>,
    sessions: Mutex<HashMap<SessionId, Arc<Live>>>,
    statuses: Mutex<HashMap<ProviderId, AgentStatus>>,
    events: mpsc::UnboundedSender<(SessionId, AgentEvent)>,
    next_id: AtomicU64,
    id_prefix: String,
}

impl AgentManager {
    /// `events` receives every session's events, tagged with its id.
    pub fn new(
        providers: Vec<Arc<dyn AgentProvider>>,
        id_prefix: impl Into<String>,
        events: mpsc::UnboundedSender<(SessionId, AgentEvent)>,
    ) -> Self {
        Self {
            providers,
            sessions: Mutex::new(HashMap::new()),
            statuses: Mutex::new(HashMap::new()),
            events,
            next_id: AtomicU64::new(1),
            id_prefix: id_prefix.into(),
        }
    }

    fn provider(&self, id: ProviderId) -> Option<&Arc<dyn AgentProvider>> {
        self.providers.iter().find(|p| p.id() == id)
    }

    /// Detection results, cached for the app session unless `refresh`.
    pub async fn statuses(&self, refresh: bool) -> Vec<(ProviderId, AgentStatus)> {
        let mut out = Vec::with_capacity(self.providers.len());
        for p in &self.providers {
            let cached = if refresh { None } else { self.statuses.lock().await.get(&p.id()).cloned() };
            let status = match cached {
                Some(s) => s,
                None => {
                    let s = p.detect().await;
                    self.statuses.lock().await.insert(p.id(), s.clone());
                    s
                }
            };
            out.push((p.id(), status));
        }
        out
    }

    /// A fresh session id, unique for this launch.
    pub fn new_session_id(&self) -> SessionId {
        SessionId(format!("{}-{}", self.id_prefix, self.next_id.fetch_add(1, Ordering::Relaxed)))
    }

    /// Start a session. `cfg.session_id` should come from
    /// [`new_session_id`](Self::new_session_id).
    pub async fn start(&self, provider: ProviderId, cfg: SessionConfig) -> AgentResult<SessionId> {
        let p = self.provider(provider).ok_or(AgentError::NotInstalled(provider.display_name()))?.clone();
        let id = cfg.session_id.clone();
        let sink = EventSink::new(id.clone(), self.events.clone());
        let session = p.start_session(cfg, sink).await?;
        self.sessions.lock().await.insert(id.clone(), Arc::new(Live { provider, session: Mutex::new(session) }));
        Ok(id)
    }

    async fn live(&self, id: &SessionId) -> AgentResult<Arc<Live>> {
        self.sessions.lock().await.get(id).cloned().ok_or(AgentError::UnknownSession)
    }

    pub async fn send(&self, id: &SessionId, turn: TurnInput) -> AgentResult<()> {
        let live = self.live(id).await?;
        let mut session = live.session.lock().await;
        session.send(turn).await
    }

    pub async fn cancel(&self, id: &SessionId) -> AgentResult<()> {
        let live = self.live(id).await?;
        let mut session = live.session.lock().await;
        session.cancel().await
    }

    pub async fn external_id(&self, id: &SessionId) -> AgentResult<Option<String>> {
        let live = self.live(id).await?;
        let session = live.session.lock().await;
        Ok(session.external_id())
    }

    pub async fn provider_of(&self, id: &SessionId) -> AgentResult<ProviderId> {
        Ok(self.live(id).await?.provider)
    }

    /// Close a session and forget it.
    pub async fn close(&self, id: &SessionId) -> AgentResult<()> {
        let live = self.sessions.lock().await.remove(id).ok_or(AgentError::UnknownSession)?;
        live.session.lock().await.close().await;
        Ok(())
    }

    /// Close everything (app quit).
    pub async fn close_all(&self) {
        let all: Vec<_> = self.sessions.lock().await.drain().map(|(_, l)| l).collect();
        for live in all {
            live.session.lock().await.close().await;
        }
    }

    pub async fn session_count(&self) -> usize {
        self.sessions.lock().await.len()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::fake::FakeAgent;
    use crate::{McpEndpoint, PromptContext};

    fn config(id: SessionId) -> SessionConfig {
        SessionConfig {
            session_id: id,
            mcp: McpEndpoint { shim_path: PathBuf::from("/x/openagc-mcp"), socket_path: PathBuf::from("/x/sock") },
            system_prompt_file: PathBuf::from("/x/prompt.md"),
            working_dir: std::env::temp_dir(),
            model: None,
            max_turns: SessionConfig::DEFAULT_MAX_TURNS,
            max_budget_usd: None,
            resume: None,
            env: vec![],
        }
    }

    #[tokio::test]
    async fn sessions_route_turns_and_events() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let fake = Arc::new(FakeAgent::ready(ProviderId::ClaudeCode));
        let manager = AgentManager::new(vec![fake.clone()], "s", tx);

        let statuses = manager.statuses(false).await;
        assert!(statuses[0].1.is_ready());
        manager.statuses(false).await;
        assert_eq!(fake.detections(), 1, "cached");
        manager.statuses(true).await;
        assert_eq!(fake.detections(), 2);

        let id = manager.new_session_id();
        assert_eq!(id.as_str(), "s-1");
        manager.start(ProviderId::ClaudeCode, config(id.clone())).await.unwrap();
        manager.send(&id, TurnInput { prompt: "hello".into(), context: PromptContext::default() }).await.unwrap();
        let mut got = Vec::new();
        while let Some((sid, e)) = rx.recv().await {
            assert_eq!(sid, id);
            let done = matches!(e, AgentEvent::TurnCompleted { .. });
            got.push(e);
            if done {
                break;
            }
        }
        assert_eq!(got[0], AgentEvent::SessionStarted { external_id: Some("fake-s-1".into()) });
        assert!(got.contains(&AgentEvent::TextDelta { text: "You said: hello".into() }));
        assert_eq!(manager.external_id(&id).await.unwrap().as_deref(), Some("fake-s-1"));

        assert_eq!(
            manager.start(ProviderId::Codex, config(manager.new_session_id())).await.unwrap_err(),
            AgentError::NotInstalled("Codex")
        );
        manager.close(&id).await.unwrap();
        assert_eq!(manager.cancel(&id).await.unwrap_err(), AgentError::UnknownSession);
        assert_eq!(manager.session_count().await, 0);
    }
}
