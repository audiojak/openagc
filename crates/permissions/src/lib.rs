//! The permission engine (spec §10.3): what an agent may do to the mailbox.
//!
//! Pure and synchronous. Every agent tool call is checked here inside the
//! core *before* the store is touched; nothing in the agent CLIs is trusted
//! to enforce anything. Two layers:
//!
//! - [`decide`]: the policy decision for one call — allow, ask the user, or
//!   deny — from the tool's risk class and the user's policy.
//! - [`SessionGuard`]: hard limits that hold whatever the policy says: bulk
//!   caps, a call rate, the session's scope, and the rule that only drafts
//!   made in this session (or attached by the user) can be sent.

use std::collections::{BTreeSet, VecDeque};

use mail_domain::{Millis, ThreadId};
use serde::{Deserialize, Serialize};

/// Threads a single Reversible call may touch.
pub const MAX_THREADS_PER_CALL: usize = 200;
/// Threads a session may touch before the user prompts again.
pub const MAX_THREADS_PER_SESSION: usize = 2_000;
/// Tool calls per rolling minute per session.
pub const MAX_CALLS_PER_MINUTE: usize = 60;
const MINUTE: Millis = 60_000;

/// How much harm a tool can do (spec §10.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    /// Reads only.
    ReadOnly,
    /// Changes the mailbox in a way the user can undo.
    Reversible,
    /// Reaches other people or removes mail: always approved by the user.
    External,
}

/// Every tool the MCP server exposes (spec §10.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Tool {
    Search,
    GetThread,
    GetMessage,
    ListLabels,
    GetAttachmentText,
    PresentThreads,
    CreateDraft,
    UpdateDraft,
    Archive,
    MarkRead,
    MarkUnread,
    AddLabel,
    RemoveLabel,
    CreateLabel,
    Send,
    Forward,
    Delete,
}

impl Tool {
    pub const ALL: [Tool; 17] = [
        Tool::Search,
        Tool::GetThread,
        Tool::GetMessage,
        Tool::ListLabels,
        Tool::GetAttachmentText,
        Tool::PresentThreads,
        Tool::CreateDraft,
        Tool::UpdateDraft,
        Tool::Archive,
        Tool::MarkRead,
        Tool::MarkUnread,
        Tool::AddLabel,
        Tool::RemoveLabel,
        Tool::CreateLabel,
        Tool::Send,
        Tool::Forward,
        Tool::Delete,
    ];

    /// The MCP tool name.
    pub fn name(self) -> &'static str {
        match self {
            Tool::Search => "mail.search",
            Tool::GetThread => "mail.get_thread",
            Tool::GetMessage => "mail.get_message",
            Tool::ListLabels => "mail.list_labels",
            Tool::GetAttachmentText => "mail.get_attachment_text",
            Tool::PresentThreads => "mail.present_threads",
            Tool::CreateDraft => "mail.create_draft",
            Tool::UpdateDraft => "mail.update_draft",
            Tool::Archive => "mail.archive",
            Tool::MarkRead => "mail.mark_read",
            Tool::MarkUnread => "mail.mark_unread",
            Tool::AddLabel => "mail.add_label",
            Tool::RemoveLabel => "mail.remove_label",
            Tool::CreateLabel => "mail.create_label",
            Tool::Send => "mail.send",
            Tool::Forward => "mail.forward",
            Tool::Delete => "mail.delete",
        }
    }

    pub fn from_name(name: &str) -> Option<Tool> {
        Tool::ALL.into_iter().find(|t| t.name() == name)
    }

    pub fn risk(self) -> Risk {
        match self {
            Tool::Search
            | Tool::GetThread
            | Tool::GetMessage
            | Tool::ListLabels
            | Tool::GetAttachmentText
            | Tool::PresentThreads => Risk::ReadOnly,
            Tool::CreateDraft
            | Tool::UpdateDraft
            | Tool::Archive
            | Tool::MarkRead
            | Tool::MarkUnread
            | Tool::AddLabel
            | Tool::RemoveLabel
            | Tool::CreateLabel => Risk::Reversible,
            Tool::Send | Tool::Forward | Tool::Delete => Risk::External,
        }
    }
}

/// The user's choices (spec §10.3 table). ReadOnly is always allowed and
/// External always needs approval; only Reversible tools are configurable.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    /// Reversible tools the user wants to approve one by one.
    #[serde(default)]
    pub approve_reversible: BTreeSet<Tool>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    #[error("{0} is not configurable: only reversible tools can require approval")]
    NotConfigurable(&'static str),
}

impl Policy {
    pub fn set_requires_approval(&mut self, tool: Tool, required: bool) -> Result<(), PolicyError> {
        if tool.risk() != Risk::Reversible {
            return Err(PolicyError::NotConfigurable(tool.name()));
        }
        if required {
            self.approve_reversible.insert(tool);
        } else {
            self.approve_reversible.remove(&tool);
        }
        Ok(())
    }
}

/// One tool call, as far as permission is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposedAction {
    pub tool: Tool,
    /// Threads the call would change or read; empty for label/draft-only
    /// calls.
    pub thread_ids: Vec<ThreadId>,
    /// For `mail.send`: the draft to send.
    pub draft_id: Option<i64>,
}

impl ProposedAction {
    pub fn new(tool: Tool) -> Self {
        Self { tool, thread_ids: Vec::new(), draft_id: None }
    }

    pub fn on_threads(tool: Tool, thread_ids: Vec<ThreadId>) -> Self {
        Self { tool, thread_ids, draft_id: None }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "decision", content = "reason")]
pub enum Decision {
    Allow,
    RequireApproval,
    Deny(DenyReason),
}

/// Why a call was refused. Shown to the agent as the tool error, and in
/// the activity log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum DenyReason {
    #[error("a single call may change at most {MAX_THREADS_PER_CALL} threads")]
    TooManyThreadsInCall,
    #[error("this session has changed {MAX_THREADS_PER_SESSION} threads; ask the user before changing more")]
    SessionBulkCap,
    #[error("too many tool calls; at most {MAX_CALLS_PER_MINUTE} per minute")]
    RateLimited,
    #[error("this thread is outside what the user asked about")]
    OutOfScope,
    #[error("only drafts created in this session can be sent")]
    DraftNotFromSession,
    #[error("a draft id is required")]
    MissingDraft,
}

/// The policy decision for one call. Pure: bulk and rate limits that need
/// history live in [`SessionGuard`].
pub fn decide(policy: &Policy, action: &ProposedAction) -> Decision {
    if action.tool.risk() != Risk::ReadOnly && action.thread_ids.len() > MAX_THREADS_PER_CALL {
        return Decision::Deny(DenyReason::TooManyThreadsInCall);
    }
    match action.tool.risk() {
        Risk::ReadOnly => Decision::Allow,
        Risk::Reversible if policy.approve_reversible.contains(&action.tool) => Decision::RequireApproval,
        Risk::Reversible => Decision::Allow,
        Risk::External => Decision::RequireApproval,
    }
}

/// What the agent may look at (spec §10.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "thread_ids")]
pub enum Scope {
    /// All mail (the default).
    Mailbox,
    /// Only these threads, passed with the prompt.
    Selection(BTreeSet<ThreadId>),
}

impl Scope {
    pub fn allows(&self, thread: &ThreadId) -> bool {
        match self {
            Scope::Mailbox => true,
            Scope::Selection(ids) => ids.contains(thread),
        }
    }

    /// Read results outside the scope are dropped, not reported: the agent
    /// sees an empty result rather than learning what exists.
    pub fn filter<T>(&self, items: Vec<T>, thread_of: impl Fn(&T) -> &ThreadId) -> Vec<T> {
        match self {
            Scope::Mailbox => items,
            Scope::Selection(_) => items.into_iter().filter(|i| self.allows(thread_of(i))).collect(),
        }
    }
}

/// Per-session hard limits (spec §10.3), enforced whatever the policy.
#[derive(Debug, Clone)]
pub struct SessionGuard {
    scope: Scope,
    recent_calls: VecDeque<Millis>,
    touched: BTreeSet<ThreadId>,
    /// Drafts this session may send: created by it, or attached by the user.
    sendable_drafts: BTreeSet<i64>,
}

impl SessionGuard {
    pub fn new(scope: Scope) -> Self {
        Self { scope, recent_calls: VecDeque::new(), touched: BTreeSet::new(), sendable_drafts: BTreeSet::new() }
    }

    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    /// Check a call against the hard limits and, if it passes, count it.
    /// Then ask [`decide`] for the policy decision.
    pub fn check(&mut self, action: &ProposedAction, now: Millis) -> Result<(), DenyReason> {
        while self.recent_calls.front().is_some_and(|&t| now - t >= MINUTE) {
            self.recent_calls.pop_front();
        }
        if self.recent_calls.len() >= MAX_CALLS_PER_MINUTE {
            return Err(DenyReason::RateLimited);
        }
        let risk = action.tool.risk();
        if risk != Risk::ReadOnly {
            if action.thread_ids.len() > MAX_THREADS_PER_CALL {
                return Err(DenyReason::TooManyThreadsInCall);
            }
            if action.thread_ids.iter().any(|t| !self.scope.allows(t)) {
                return Err(DenyReason::OutOfScope);
            }
            let new = action.thread_ids.iter().filter(|t| !self.touched.contains(*t)).count();
            if self.touched.len() + new > MAX_THREADS_PER_SESSION {
                return Err(DenyReason::SessionBulkCap);
            }
        }
        if action.tool == Tool::Send {
            let draft = action.draft_id.ok_or(DenyReason::MissingDraft)?;
            if !self.sendable_drafts.contains(&draft) {
                return Err(DenyReason::DraftNotFromSession);
            }
        }
        self.recent_calls.push_back(now);
        if risk != Risk::ReadOnly {
            self.touched.extend(action.thread_ids.iter().cloned());
        }
        Ok(())
    }

    /// A draft this session created (or the user attached) may be sent.
    pub fn allow_draft(&mut self, draft_id: i64) {
        self.sendable_drafts.insert(draft_id);
    }

    /// The user prompted again: the session bulk count starts over.
    pub fn new_user_prompt(&mut self) {
        self.touched.clear();
    }

    pub fn threads_touched(&self) -> usize {
        self.touched.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn threads(n: usize) -> Vec<ThreadId> {
        (0..n).map(|i| ThreadId::new(format!("t{i}"))).collect()
    }

    #[test]
    fn names_round_trip_and_are_unique() {
        let names: BTreeSet<&str> = Tool::ALL.iter().map(|t| t.name()).collect();
        assert_eq!(names.len(), Tool::ALL.len());
        for tool in Tool::ALL {
            assert_eq!(Tool::from_name(tool.name()), Some(tool));
        }
        assert_eq!(Tool::from_name("mail.rm_rf"), None);
    }

    #[test]
    fn default_policy_table() {
        use Decision::*;
        let default = Policy::default();
        let mut strict = Policy::default();
        strict.set_requires_approval(Tool::Archive, true).unwrap();
        #[rustfmt::skip]
        let cases: &[(&Policy, Tool, usize, Decision)] = &[
            (&default, Tool::Search, 0, Allow),
            (&default, Tool::GetThread, 1, Allow),
            (&default, Tool::GetAttachmentText, 1, Allow),
            (&default, Tool::PresentThreads, 500, Allow), // reads are not bulk-capped
            (&default, Tool::Archive, 1, Allow),
            (&default, Tool::Archive, 200, Allow),
            (&default, Tool::Archive, 201, Deny(DenyReason::TooManyThreadsInCall)),
            (&default, Tool::CreateDraft, 0, Allow),
            (&default, Tool::CreateLabel, 0, Allow),
            (&default, Tool::Send, 0, RequireApproval),
            (&default, Tool::Forward, 1, RequireApproval),
            (&default, Tool::Delete, 1, RequireApproval),
            (&default, Tool::Delete, 201, Deny(DenyReason::TooManyThreadsInCall)),
            (&strict, Tool::Archive, 1, RequireApproval),
            (&strict, Tool::MarkRead, 1, Allow),
            (&strict, Tool::Search, 0, Allow),
        ];
        for (policy, tool, n, expected) in cases {
            let action = ProposedAction::on_threads(*tool, threads(*n));
            assert_eq!(&decide(policy, &action), expected, "{} on {n} threads", tool.name());
        }
    }

    #[test]
    fn only_reversible_tools_are_configurable() {
        let mut p = Policy::default();
        assert!(p.set_requires_approval(Tool::Send, false).is_err(), "external stays gated");
        assert!(p.set_requires_approval(Tool::Search, true).is_err());
        p.set_requires_approval(Tool::AddLabel, true).unwrap();
        p.set_requires_approval(Tool::AddLabel, false).unwrap();
        assert_eq!(p, Policy::default());
    }

    #[test]
    fn rate_limit_is_a_rolling_minute() {
        let mut g = SessionGuard::new(Scope::Mailbox);
        let search = ProposedAction::new(Tool::Search);
        for i in 0..MAX_CALLS_PER_MINUTE {
            g.check(&search, 1_000 + i as Millis).unwrap();
        }
        assert_eq!(g.check(&search, 30_000), Err(DenyReason::RateLimited));
        // The first call ages out a minute after it was made.
        g.check(&search, 61_000).unwrap();
        assert_eq!(g.check(&search, 61_000), Err(DenyReason::RateLimited), "only one slot freed");
    }

    #[test]
    fn session_bulk_cap_counts_distinct_threads_until_the_next_prompt() {
        let mut g = SessionGuard::new(Scope::Mailbox);
        let mut now = 0;
        let mut next = |g: &mut SessionGuard, ids: Vec<ThreadId>| {
            now += MINUTE; // stay clear of the rate limit
            g.check(&ProposedAction::on_threads(Tool::Archive, ids), now)
        };
        for batch in 0..10 {
            let ids = (0..200).map(|i| ThreadId::new(format!("t{batch}-{i}"))).collect();
            next(&mut g, ids).unwrap();
        }
        assert_eq!(g.threads_touched(), 2_000);
        // Touching the same threads again is not new.
        next(&mut g, (0..200).map(|i| ThreadId::new(format!("t0-{i}"))).collect()).unwrap();
        assert_eq!(next(&mut g, threads(1)), Err(DenyReason::SessionBulkCap));
        // A denied call changes nothing.
        assert_eq!(g.threads_touched(), 2_000);
        g.new_user_prompt();
        next(&mut g, threads(1)).unwrap();
    }

    #[test]
    fn selection_scope_limits_writes_and_filters_reads() {
        let selected: BTreeSet<ThreadId> = threads(2).into_iter().collect();
        let mut g = SessionGuard::new(Scope::Selection(selected));
        g.check(&ProposedAction::on_threads(Tool::Archive, threads(2)), 0).unwrap();
        assert_eq!(
            g.check(&ProposedAction::on_threads(Tool::Archive, vec![ThreadId::new("elsewhere")]), 1),
            Err(DenyReason::OutOfScope)
        );
        let found = vec![ThreadId::new("t1"), ThreadId::new("elsewhere"), ThreadId::new("t0")];
        assert_eq!(g.scope().filter(found, |t| t), vec![ThreadId::new("t1"), ThreadId::new("t0")]);
        assert_eq!(Scope::Mailbox.filter(threads(3), |t| t).len(), 3);
    }

    #[test]
    fn only_session_drafts_can_be_sent() {
        let mut g = SessionGuard::new(Scope::Mailbox);
        let send = |d| ProposedAction { tool: Tool::Send, thread_ids: vec![], draft_id: d };
        assert_eq!(g.check(&send(None), 0), Err(DenyReason::MissingDraft));
        assert_eq!(g.check(&send(Some(7)), 1), Err(DenyReason::DraftNotFromSession));
        g.allow_draft(7);
        g.check(&send(Some(7)), 2).unwrap();
        assert_eq!(
            decide(&Policy::default(), &send(Some(7))),
            Decision::RequireApproval,
            "and still approved by the user"
        );
    }

    #[test]
    fn decisions_and_policies_serialize_for_the_audit_log() {
        let d = Decision::Deny(DenyReason::OutOfScope);
        assert_eq!(serde_json::to_string(&d).unwrap(), r#"{"decision":"deny","reason":"out_of_scope"}"#);
        let mut p = Policy::default();
        p.set_requires_approval(Tool::Archive, true).unwrap();
        let back: Policy = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert_eq!(back, p);
    }
}
