//! Codex sessions (spec §9.4). Filled in by oagc-3lu.

use agent_api::process::Locator;
use agent_api::{AgentError, AgentResult, AgentSession, EventSink, SessionConfig};

pub(crate) async fn start(
    _locator: &Locator,
    _cfg: SessionConfig,
    _sink: EventSink,
) -> AgentResult<Box<dyn AgentSession>> {
    Err(AgentError::Spawn("Codex sessions are not available yet".into()))
}
