//! Agent sessions across the FFI (spec §4.2, §9.5): providers, start,
//! prompt, cancel, close, and the event stream batched every 16 ms so a
//! fast token stream costs Swift one main-actor hop per frame.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use agent_api::{
    AgentError, AgentEvent, AgentManager, AgentProvider, AgentStatus, EventSink, McpEndpoint, PromptContext,
    ProviderId, SessionConfig, SessionId, TurnInput,
};
use mail_domain::ThreadId;
use permissions::Scope;
use tokio::sync::mpsc;

use crate::events::CoreEvent;
use crate::{Core, CoreError, ErrorKind, EventBus, runtime};

/// Events from one session within one frame are sent together.
pub const AGENT_EVENT_BATCH: Duration = Duration::from_millis(16);

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum AgentStatusInfo {
    Ready { version: String },
    NotInstalled,
    NotAuthenticated { version: String },
    UpdateRequired { version: String, minimum: String },
    Error { message: String },
}

impl From<AgentStatus> for AgentStatusInfo {
    fn from(s: AgentStatus) -> Self {
        match s {
            AgentStatus::Ready { version, .. } => Self::Ready { version },
            AgentStatus::NotInstalled => Self::NotInstalled,
            AgentStatus::NotAuthenticated { version, .. } => Self::NotAuthenticated { version },
            AgentStatus::UpdateRequired { version, minimum, .. } => Self::UpdateRequired { version, minimum },
            AgentStatus::Error { message } => Self::Error { message },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AgentProviderInfo {
    /// `claude-code` or `codex`.
    pub id: String,
    pub name: String,
    pub status: AgentStatusInfo,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, uniffi::Record)]
pub struct PromptContextInfo {
    pub mailbox_id: Option<String>,
    pub selected_thread_ids: Vec<String>,
    pub search_query: Option<String>,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct AgentSessionInfo {
    pub session_id: String,
    pub provider: String,
    /// The first prompt.
    pub title: String,
    pub started_at: i64,
    pub prompt_count: u32,
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, uniffi::Enum)]
pub enum AgentTranscriptItem {
    Prompt { text: String },
    Event { event: AgentEventInfo },
}

#[derive(Debug, Clone, PartialEq, uniffi::Enum)]
pub enum AgentEventInfo {
    SessionStarted { external_id: Option<String> },
    TurnStarted,
    TextDelta { text: String },
    ThinkingDelta { text: String },
    ToolCallStarted { call_id: String, tool: String, args_summary: String },
    ToolCallFinished { call_id: String, ok: bool, summary: String },
    ActionProposed { action_id: i64, tool: String, summary: String },
    ResultsAvailable { thread_ids: Vec<String> },
    TurnCompleted { input_tokens: Option<u64>, output_tokens: Option<u64>, cost_usd: Option<f64> },
    TurnFailed { message: String },
    SessionEnded,
}

impl From<AgentEvent> for AgentEventInfo {
    fn from(e: AgentEvent) -> Self {
        match e {
            AgentEvent::SessionStarted { external_id } => Self::SessionStarted { external_id },
            AgentEvent::TurnStarted => Self::TurnStarted,
            AgentEvent::TextDelta { text } => Self::TextDelta { text },
            AgentEvent::ThinkingDelta { text } => Self::ThinkingDelta { text },
            AgentEvent::ToolCallStarted { call_id, tool, args_summary } => {
                Self::ToolCallStarted { call_id, tool, args_summary }
            }
            AgentEvent::ToolCallFinished { call_id, ok, summary } => Self::ToolCallFinished { call_id, ok, summary },
            AgentEvent::ActionProposed { action_id, tool, summary } => {
                Self::ActionProposed { action_id, tool, summary }
            }
            AgentEvent::ResultsAvailable { thread_ids } => {
                Self::ResultsAvailable { thread_ids: thread_ids.into_iter().map(|t| t.0).collect() }
            }
            AgentEvent::TurnCompleted { usage, cost_usd } => Self::TurnCompleted {
                input_tokens: usage.map(|u| u.input_tokens),
                output_tokens: usage.map(|u| u.output_tokens),
                cost_usd,
            },
            AgentEvent::TurnFailed { message } => Self::TurnFailed { message },
            AgentEvent::SessionEnded => Self::SessionEnded,
        }
    }
}

/// Forward agent events to Swift, one `AgentEvents` per session per frame,
/// storing each session's transcript on the way.
async fn forward(mut rx: mpsc::UnboundedReceiver<(SessionId, AgentEvent)>, events: EventBus, core: Weak<Core>) {
    while let Some(first) = rx.recv().await {
        let mut batch = vec![first];
        let deadline = tokio::time::Instant::now() + AGENT_EVENT_BATCH;
        while let Ok(Some(e)) = tokio::time::timeout_at(deadline, rx.recv()).await {
            batch.push(e);
        }
        // Per session, in arrival order.
        let mut sessions: Vec<(SessionId, Vec<AgentEvent>)> = Vec::new();
        for (sid, e) in batch {
            match sessions.iter_mut().find(|(s, _)| *s == sid) {
                Some((_, list)) => list.push(e),
                None => sessions.push((sid, vec![e])),
            }
        }
        for (sid, list) in sessions {
            let list = agent_api::coalesce(list);
            if let Some(core) = core.upgrade() {
                core.persist_agent_events(&sid, &list).await;
            }
            events
                .emit(CoreEvent::AgentEvents { session_id: sid.0, events: list.into_iter().map(Into::into).collect() });
        }
    }
}

/// Paths the app provides: the bundled shim and system prompt.
#[derive(Debug, Clone, Default)]
pub(crate) struct AgentResources {
    pub shim_path: PathBuf,
    pub system_prompt_path: PathBuf,
}

pub(crate) struct AgentRuntime {
    pub manager: AgentManager,
    pub tx: mpsc::UnboundedSender<(SessionId, AgentEvent)>,
}

impl Core {
    /// The providers the app offers: the real adapters, or fakes installed
    /// for UI development and tests.
    fn agent_providers(&self) -> Vec<Arc<dyn AgentProvider>> {
        if let Some(fakes) = self.agents.fake_providers.get() {
            return fakes.clone();
        }
        crate::agents::adapters()
    }

    pub(crate) fn agent_runtime(self: &Arc<Self>) -> &AgentRuntime {
        self.agents.runtime.get_or_init(|| {
            let (tx, rx) = mpsc::unbounded_channel();
            runtime::runtime().spawn(forward(rx, self.events.clone(), Arc::downgrade(self)));
            // Unique across launches: the id keys the stored transcript.
            let mut b = [0u8; 4];
            let _ = getrandom::fill(&mut b);
            let prefix = format!("agent-{}", b.iter().map(|x| format!("{x:02x}")).collect::<String>());
            AgentRuntime { manager: AgentManager::new(self.agent_providers(), prefix, tx.clone()), tx }
        })
    }

    /// Store a batch of a session's events in its transcript. Deltas were
    /// already merged; start and end markers are kept as columns instead.
    async fn persist_agent_events(&self, sid: &SessionId, events: &[AgentEvent]) {
        let Ok(db) = self.db() else { return };
        let uuid = sid.0.clone();
        let events = events.to_vec();
        let written = db
            .write(move |tx| {
                for e in &events {
                    match e {
                        AgentEvent::SessionStarted { external_id: Some(id) } => {
                            mail_store::agents::set_external_id(tx, &uuid, id)?
                        }
                        AgentEvent::SessionStarted { .. } | AgentEvent::SessionEnded | AgentEvent::TurnStarted => {}
                        other => {
                            if let AgentEvent::TurnCompleted { cost_usd: Some(cost), .. } = other {
                                mail_store::agents::add_cost(tx, &uuid, *cost)?;
                            }
                            let json = serde_json::to_string(other).unwrap_or_default();
                            mail_store::agents::append(tx, &uuid, "agent", &json)?;
                        }
                    }
                }
                Ok(())
            })
            .await;
        if let Err(e) = written {
            tracing::warn!(error = %e, "storing the agent transcript failed");
        }
    }
}

fn provider_id(id: &str) -> Result<ProviderId, CoreError> {
    match id {
        "claude-code" => Ok(ProviderId::ClaudeCode),
        "codex" => Ok(ProviderId::Codex),
        other => Err(CoreError::new(ErrorKind::InvalidInput, format!("unknown agent {other:?}"))),
    }
}

impl From<AgentError> for CoreError {
    fn from(e: AgentError) -> Self {
        let kind = match e {
            AgentError::NotInstalled(_) | AgentError::UnknownSession => ErrorKind::NotFound,
            AgentError::NotAuthenticated(_) => ErrorKind::Auth,
            AgentError::Busy => ErrorKind::InvalidInput,
            _ => ErrorKind::Agent,
        };
        CoreError::new(kind, e.to_string())
    }
}

#[uniffi::export]
impl Core {
    /// Where the bundled `openagc-mcp` and `agent-system-prompt.md` are.
    /// Called once at launch.
    pub fn configure_agents(&self, shim_path: String, system_prompt_path: String) {
        *self.agents.resources.write().unwrap_or_else(|e| e.into_inner()) =
            AgentResources { shim_path: shim_path.into(), system_prompt_path: system_prompt_path.into() };
    }

    /// Development and UI tests: replace the real agents with scripted ones
    /// that echo prompts. Only effective before the first agent call.
    pub fn debug_use_fake_agents(&self) {
        let fakes: Vec<Arc<dyn AgentProvider>> = vec![
            Arc::new(agent_api::fake::FakeAgent::ready(ProviderId::ClaudeCode)),
            Arc::new(agent_api::fake::FakeAgent::with_status(ProviderId::Codex, AgentStatus::NotInstalled)),
        ];
        let _ = self.agents.fake_providers.set(fakes);
    }

    /// Installed agents and whether they can be used. Cached per launch
    /// unless `refresh`.
    pub async fn list_agent_providers(self: Arc<Self>, refresh: bool) -> Vec<AgentProviderInfo> {
        let core = self.clone();
        runtime::run(async move {
            Ok::<_, CoreError>(
                core.agent_runtime()
                    .manager
                    .statuses(refresh)
                    .await
                    .into_iter()
                    .map(|(id, status)| AgentProviderInfo {
                        id: id.as_str().to_owned(),
                        name: id.display_name().to_owned(),
                        status: status.into(),
                    })
                    .collect(),
            )
        })
        .await
        .unwrap_or_default()
    }

    /// Start a session with `provider`. With `selection`, the agent can only
    /// see those threads (spec §10.3). Returns OpenAGC's session id.
    pub async fn start_agent_session(
        self: Arc<Self>,
        provider: String,
        selection: Option<Vec<String>>,
        resume: Option<String>,
    ) -> Result<String, CoreError> {
        let provider = provider_id(&provider)?;
        let id = self.agent_runtime().manager.new_session_id();
        self.start_session_with_id(provider, id, selection, resume).await
    }

    /// Continue a stored conversation (spec §9.3, §9.4): the agent CLI
    /// resumes its own session and the transcript keeps growing.
    pub async fn resume_agent_session(self: Arc<Self>, session_id: String) -> Result<String, CoreError> {
        let rt = self.agent_runtime();
        if rt.manager.provider_of(&SessionId(session_id.clone())).await.is_ok() {
            return Ok(session_id); // still live
        }
        let db = self.db()?;
        let uuid = session_id.clone();
        let row = runtime::run(async move { Ok(db.read(move |c| mail_store::agents::get_session(c, &uuid)).await?) })
            .await?
            .ok_or_else(|| CoreError::new(ErrorKind::NotFound, "no such conversation"))?;
        let provider = provider_id(&row.provider)?;
        self.start_session_with_id(provider, SessionId(session_id), None, row.external_id).await
    }

    /// Stored conversations, newest first.
    pub async fn list_agent_history(&self, limit: u32) -> Result<Vec<AgentSessionInfo>, CoreError> {
        let db = self.db()?;
        runtime::run(async move {
            let rows = db.read(move |c| mail_store::agents::list_sessions(c, limit)).await?;
            Ok(rows
                .into_iter()
                .map(|r| AgentSessionInfo {
                    session_id: r.uuid,
                    provider: r.provider,
                    title: r.title,
                    started_at: r.started_at,
                    prompt_count: r.prompt_count,
                    cost_usd: r.cost_usd,
                })
                .collect())
        })
        .await
    }

    /// A stored conversation's prompts and events, for showing it again.
    pub async fn agent_transcript(&self, session_id: String) -> Result<Vec<AgentTranscriptItem>, CoreError> {
        let db = self.db()?;
        runtime::run(async move {
            let rows = db.read(move |c| mail_store::agents::transcript(c, &session_id)).await?;
            Ok(rows
                .into_iter()
                .filter_map(|r| match r.role.as_str() {
                    "user" => serde_json::from_str::<String>(&r.content_json)
                        .ok()
                        .map(|text| AgentTranscriptItem::Prompt { text }),
                    _ => serde_json::from_str::<AgentEvent>(&r.content_json)
                        .ok()
                        .map(|e| AgentTranscriptItem::Event { event: e.into() }),
                })
                .collect())
        })
        .await
    }
    /// Send a prompt; the reply streams as `AgentEvents`.
    pub async fn send_agent_prompt(
        self: Arc<Self>,
        session_id: String,
        prompt: String,
        context: PromptContextInfo,
    ) -> Result<(), CoreError> {
        let turn = TurnInput {
            prompt,
            context: PromptContext {
                mailbox: context.mailbox_id,
                selected_thread_ids: context.selected_thread_ids.into_iter().map(ThreadId).collect(),
                search_query: context.search_query,
            },
        };
        // A new prompt from the user resets the session's bulk count.
        self.agents.with_session(&session_id, |s| s.guard.new_user_prompt());
        if let Ok(db) = self.db() {
            let (uuid, text) = (session_id.clone(), serde_json::to_string(&turn.prompt).unwrap_or_default());
            let _ = runtime::run(async move {
                Ok::<_, CoreError>(db.write(move |tx| mail_store::agents::append(tx, &uuid, "user", &text)).await?)
            })
            .await;
        }
        let core = self.clone();
        runtime::run(async move { Ok(core.agent_runtime().manager.send(&SessionId(session_id), turn).await?) }).await
    }

    pub async fn cancel_agent_turn(self: Arc<Self>, session_id: String) -> Result<(), CoreError> {
        let core = self.clone();
        runtime::run(async move { Ok(core.agent_runtime().manager.cancel(&SessionId(session_id)).await?) }).await
    }

    /// End a session; its pending tool calls are refused from now on.
    pub async fn close_agent_session(self: Arc<Self>, session_id: String) -> Result<(), CoreError> {
        self.agents.unregister(&session_id);
        if let Ok(db) = self.db() {
            let (uuid, now) = (session_id.clone(), mail_sync::now_millis());
            let _ = runtime::run(async move {
                Ok::<_, CoreError>(db.write(move |tx| mail_store::agents::end_session(tx, &uuid, now)).await?)
            })
            .await;
        }
        let core = self.clone();
        runtime::run(async move { Ok(core.agent_runtime().manager.close(&SessionId(session_id)).await?) }).await
    }
}

impl Core {
    async fn start_session_with_id(
        self: Arc<Self>,
        provider: ProviderId,
        id: SessionId,
        selection: Option<Vec<String>>,
        resume: Option<String>,
    ) -> Result<String, CoreError> {
        let db = self.db()?; // an account must be open: the tools read it
        let socket_path = self.mcp_socket_path()?;
        let resources = self.agents.resources.read().unwrap_or_else(|e| e.into_inner()).clone();
        let rt = self.agent_runtime();
        let working_dir = PathBuf::from(&self.config.data_dir).join("agents").join(id.as_str());
        std::fs::create_dir_all(&working_dir).map_err(|e| CoreError::new(ErrorKind::Storage, e.to_string()))?;
        let scope = match selection {
            Some(ids) => Scope::Selection(ids.into_iter().map(ThreadId).collect::<BTreeSet<_>>()),
            None => Scope::Mailbox,
        };
        // Registered before the CLI starts, so its first tool call finds it.
        self.agents.register(id.as_str(), scope, Some(EventSink::new(id.clone(), rt.tx.clone())));
        let (uuid, name, now) = (id.0.clone(), provider.as_str().to_owned(), mail_sync::now_millis());
        runtime::run(async move {
            Ok::<_, CoreError>(db.write(move |tx| mail_store::agents::start_session(tx, &uuid, &name, now)).await?)
        })
        .await?;
        let env = self.agent_environment(provider);
        let cfg = SessionConfig {
            session_id: id.clone(),
            mcp: McpEndpoint { shim_path: resources.shim_path, socket_path },
            system_prompt_file: resources.system_prompt_path,
            working_dir,
            model: None,
            max_turns: SessionConfig::DEFAULT_MAX_TURNS,
            max_budget_usd: None,
            resume,
            env,
        };
        let core = self.clone();
        let started = runtime::run(async move { Ok(core.agent_runtime().manager.start(provider, cfg).await?) }).await;
        if started.is_err() {
            self.agents.unregister(id.as_str());
        }
        started.map(|id| id.0)
    }

    /// Environment for the agent CLI: the optional API key fallback from
    /// the Keychain (spec §9.3).
    fn agent_environment(&self, provider: ProviderId) -> Vec<(String, String)> {
        if provider != ProviderId::ClaudeCode {
            return vec![];
        }
        match self.secrets.get(crate::secrets::keys::ANTHROPIC_API_KEY.to_owned()) {
            Ok(Some(key)) if !key.is_empty() => vec![("ANTHROPIC_API_KEY".to_owned(), key)],
            _ => vec![],
        }
    }
}

/// Set once per launch.
pub(crate) type FakeProviders = OnceLock<Vec<Arc<dyn AgentProvider>>>;
