//! A scripted agent for tests of everything above the adapters.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;

use crate::{
    AgentEvent, AgentProvider, AgentResult, AgentSession, AgentStatus, EventSink, ProviderId, SessionConfig, TurnInput,
    Usage,
};

/// Replies "You said: <prompt>" to every turn.
pub struct FakeAgent {
    id: ProviderId,
    status: AgentStatus,
    detections: Arc<AtomicUsize>,
}

impl FakeAgent {
    pub fn ready(id: ProviderId) -> Self {
        Self {
            id,
            status: AgentStatus::Ready { version: "1.0.0 (fake)".into(), path: "/fake/agent".into() },
            detections: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn with_status(id: ProviderId, status: AgentStatus) -> Self {
        Self { id, status, detections: Arc::new(AtomicUsize::new(0)) }
    }

    pub fn detections(&self) -> usize {
        self.detections.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl AgentProvider for FakeAgent {
    fn id(&self) -> ProviderId {
        self.id
    }

    async fn detect(&self) -> AgentStatus {
        self.detections.fetch_add(1, Ordering::Relaxed);
        self.status.clone()
    }

    async fn start_session(&self, cfg: SessionConfig, sink: EventSink) -> AgentResult<Box<dyn AgentSession>> {
        let external = format!("fake-{}", cfg.session_id);
        sink.emit(AgentEvent::SessionStarted { external_id: Some(external.clone()) });
        Ok(Box::new(FakeSession { sink, external }))
    }
}

struct FakeSession {
    sink: EventSink,
    external: String,
}

#[async_trait]
impl AgentSession for FakeSession {
    async fn send(&mut self, turn: TurnInput) -> AgentResult<()> {
        self.sink.emit(AgentEvent::TurnStarted);
        self.sink.emit(AgentEvent::TextDelta { text: format!("You said: {}", turn.prompt) });
        self.sink.emit(AgentEvent::TurnCompleted {
            usage: Some(Usage { input_tokens: 10, output_tokens: 5, cached_input_tokens: 0 }),
            cost_usd: None,
        });
        Ok(())
    }

    async fn cancel(&mut self) -> AgentResult<()> {
        self.sink.emit(AgentEvent::TurnFailed { message: "cancelled".into() });
        Ok(())
    }

    fn external_id(&self) -> Option<String> {
        Some(self.external.clone())
    }

    async fn close(&mut self) {
        self.sink.emit(AgentEvent::SessionEnded);
    }
}
