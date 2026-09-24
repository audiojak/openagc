//! Claude Code adapter (spec §9.2, §9.3): detection, and one `claude -p`
//! subprocess per turn resumed by session id.

mod detect;
pub mod routines;
mod session;
mod stream;

use std::sync::Arc;

use agent_api::process::Locator;
use agent_api::{AgentProvider, AgentResult, AgentSession, AgentStatus, EventSink, ProviderId, SessionConfig};
use async_trait::async_trait;

pub use detect::{MINIMUM_VERSION, auth_probe_args};
pub use session::{mcp_config, turn_args};
pub use stream::StreamParser;

pub struct ClaudeProvider {
    locator: Locator,
}

impl ClaudeProvider {
    pub fn new(locator: Locator) -> Self {
        Self { locator }
    }

    pub fn standard() -> Arc<Self> {
        Arc::new(Self::new(Locator::standard()))
    }
}

#[async_trait]
impl AgentProvider for ClaudeProvider {
    fn id(&self) -> ProviderId {
        ProviderId::ClaudeCode
    }

    async fn detect(&self) -> AgentStatus {
        detect::detect(&self.locator).await
    }

    async fn start_session(&self, cfg: SessionConfig, sink: EventSink) -> AgentResult<Box<dyn AgentSession>> {
        session::start(&self.locator, cfg, sink).await
    }
}
