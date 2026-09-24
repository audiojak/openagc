//! Codex adapter (spec §9.2, §9.4): detection, and a long-lived
//! `codex app-server` speaking JSON-RPC over stdio.

mod detect;
mod session;

use std::sync::Arc;

use agent_api::process::Locator;
use agent_api::{AgentProvider, AgentResult, AgentSession, AgentStatus, EventSink, ProviderId, SessionConfig};
use async_trait::async_trait;

pub use detect::MINIMUM_VERSION;

pub struct CodexProvider {
    locator: Locator,
}

impl CodexProvider {
    pub fn new(locator: Locator) -> Self {
        Self { locator }
    }

    pub fn standard() -> Arc<Self> {
        Arc::new(Self::new(Locator::standard()))
    }
}

#[async_trait]
impl AgentProvider for CodexProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Codex
    }

    async fn detect(&self) -> AgentStatus {
        detect::detect(&self.locator).await
    }

    async fn start_session(&self, cfg: SessionConfig, sink: EventSink) -> AgentResult<Box<dyn AgentSession>> {
        session::start(&self.locator, cfg, sink).await
    }
}
