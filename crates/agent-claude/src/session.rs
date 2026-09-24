//! Claude Code sessions (spec §9.3). Filled in by oagc-5b3.

use agent_api::process::Locator;
use agent_api::{AgentError, AgentResult, AgentSession, EventSink, SessionConfig};

pub(crate) async fn start(
    _locator: &Locator,
    _cfg: SessionConfig,
    _sink: EventSink,
) -> AgentResult<Box<dyn AgentSession>> {
    Err(AgentError::Spawn("Claude sessions are not available yet".into()))
}
