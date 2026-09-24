//! Approvals (spec §10.4) and the audit log (§10.5).
//!
//! A call that needs the user is recorded as a pending action, announced
//! with `ActionProposed`, and parked: the MCP request stays open until the
//! user approves or rejects it, ten minutes pass, or the session ends.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use agent_api::AgentEvent;
use agent_mcp::Outcome;
use permissions::Tool;
use serde_json::Value;
use tokio::sync::oneshot;

use crate::{Core, CoreError, ErrorKind, runtime};

pub(crate) const APPROVAL_TIMEOUT: Duration = Duration::from_secs(10 * 60);

pub(crate) struct Pending {
    session: String,
    decide: oneshot::Sender<bool>,
    /// A draft the agent may not change while the user looks at it.
    frozen_draft: Option<i64>,
}

#[derive(Default)]
pub(crate) struct Approvals {
    pending: Mutex<HashMap<i64, Pending>>,
    /// Tests shorten this.
    pub(crate) timeout: Mutex<Option<Duration>>,
}

impl Approvals {
    pub(crate) fn is_frozen(&self, draft_id: i64) -> bool {
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).values().any(|p| p.frozen_draft == Some(draft_id))
    }

    fn timeout(&self) -> Duration {
        self.timeout.lock().unwrap_or_else(|e| e.into_inner()).unwrap_or(APPROVAL_TIMEOUT)
    }

    /// Reject everything a session is waiting on (it was cancelled or closed).
    pub(crate) fn reject_session(&self, session: &str) {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let ids: Vec<i64> = pending.iter().filter(|(_, p)| p.session == session).map(|(id, _)| *id).collect();
        for id in ids {
            if let Some(p) = pending.remove(&id) {
                let _ = p.decide.send(false);
            }
        }
    }
}

fn risk_name(tool: Tool) -> &'static str {
    match tool.risk() {
        permissions::Risk::ReadOnly => "read_only",
        permissions::Risk::Reversible => "reversible",
        permissions::Risk::External => "external",
    }
}

/// Up to 50 thread and message ids mentioned in a result, so the log can
/// answer "what did the agent see?".
fn ids_in(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                if matches!(k.as_str(), "thread_id" | "message_id" | "draft_id" | "label_id")
                    && let Some(s) = v.as_str().map(str::to_owned).or_else(|| v.as_i64().map(|n| n.to_string()))
                {
                    if out.len() < 50 && !out.contains(&s) {
                        out.push(s);
                    }
                } else {
                    ids_in(v, out);
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|v| ids_in(v, out)),
        _ => {}
    }
}

pub(crate) fn outcome_summary(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Ok { structured: Some(v), text } => {
            let mut ids = Vec::new();
            ids_in(v, &mut ids);
            if ids.is_empty() { mail_mime::truncate_chars(text, 200).0 } else { ids.join(", ") }
        }
        Outcome::Ok { text, .. } => mail_mime::truncate_chars(text, 200).0,
        Outcome::Error { code, message } => format!("{code}: {message}"),
    }
}

impl Core {
    /// Add a tool call to the audit log.
    pub(crate) async fn record_action(&self, session: &str, tool: Tool, args: &Value, state: &str) -> Option<i64> {
        let db = self.db().ok()?;
        let (uuid, name, json, risk, state, now) = (
            session.to_owned(),
            tool.name().to_owned(),
            args.to_string(),
            risk_name(tool),
            state.to_owned(),
            mail_sync::now_millis(),
        );
        db.write(move |tx| mail_store::agents::record_action(tx, &uuid, &name, &json, risk, &state, now))
            .await
            .ok()
            .flatten()
    }

    pub(crate) async fn finish_action(&self, id: Option<i64>, state: &str, summary: Option<String>) {
        let (Some(id), Ok(db)) = (id, self.db()) else { return };
        let (state, now) = (state.to_owned(), mail_sync::now_millis());
        let _ = db.write(move |tx| mail_store::agents::update_action(tx, id, &state, summary.as_deref(), now)).await;
    }

    /// Ask the user and wait. `Ok` if approved; the error is the tool's
    /// answer otherwise.
    pub(crate) async fn await_approval(
        &self,
        session: &str,
        action_id: Option<i64>,
        tool: Tool,
        summary: String,
        draft_id: Option<i64>,
    ) -> Result<(), Outcome> {
        let Some(action_id) = action_id else {
            return Err(Outcome::error("failed", "could not record the proposal"));
        };
        let (tx, rx) = oneshot::channel();
        self.agents
            .approvals
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(action_id, Pending { session: session.to_owned(), decide: tx, frozen_draft: draft_id });
        self.agents.with_session(session, |s| {
            if let Some(sink) = &s.sink {
                sink.emit(AgentEvent::ActionProposed { action_id, tool: tool.name().to_owned(), summary, draft_id });
            }
        });
        let decision = tokio::time::timeout(self.agents.approvals.timeout(), rx).await;
        self.agents.approvals.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&action_id);
        let (approved, state, outcome) = match decision {
            Ok(Ok(true)) => (true, "approved", None),
            Ok(Ok(false)) | Ok(Err(_)) => {
                (false, "rejected", Some(Outcome::error("rejected_by_user", "The user declined this action.")))
            }
            Err(_) => (
                false,
                "expired",
                Some(Outcome::error("approval_timeout", "The user did not answer within 10 minutes.")),
            ),
        };
        self.agents.with_session(session, |s| {
            if let Some(sink) = &s.sink {
                sink.emit(AgentEvent::ActionResolved { action_id, approved });
            }
        });
        self.finish_action(Some(action_id), state, None).await;
        match outcome {
            None => Ok(()),
            Some(o) => Err(o),
        }
    }
}

#[uniffi::export]
impl Core {
    /// The user's answer to a proposed action (spec §10.4).
    pub fn resolve_agent_action(&self, action_id: i64, approve: bool) -> Result<(), CoreError> {
        let pending = self.agents.approvals.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&action_id);
        match pending {
            Some(p) => {
                let _ = p.decide.send(approve);
                Ok(())
            }
            None => Err(CoreError::new(ErrorKind::NotFound, "that action is no longer waiting")),
        }
    }

    /// The audit log, newest first (spec §10.5).
    pub async fn list_agent_actions(&self, limit: u32) -> Result<Vec<AgentActionInfo>, CoreError> {
        let db = self.db()?;
        runtime::run(async move {
            let rows = db.read(move |c| mail_store::agents::list_actions(c, limit)).await?;
            Ok(rows
                .into_iter()
                .map(|r| AgentActionInfo {
                    action_id: r.id,
                    session_id: r.session_uuid,
                    tool: r.tool,
                    arguments_json: r.args_json,
                    risk: r.risk,
                    state: r.state,
                    result_summary: r.result_summary,
                    created_at: r.created_at,
                    resolved_at: r.resolved_at,
                })
                .collect())
        })
        .await
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AgentActionInfo {
    pub action_id: i64,
    pub session_id: String,
    pub tool: String,
    pub arguments_json: String,
    pub risk: String,
    pub state: String,
    pub result_summary: Option<String>,
    pub created_at: i64,
    pub resolved_at: Option<i64>,
}
