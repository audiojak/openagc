//! An in-memory provider implementing the full [`MailProvider`] contract,
//! for sync-engine tests and for running the app without a real account.
//! Supports a small subset of Gmail search (`is:unread`, `newer_than:Nd`)
//! and a history log that can be expired to force a resync.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use mail_domain::{Label, LabelId, MessageId, Millis, ThreadId};

use crate::{
    Change, ChangeSet, FetchedMessage, IdPage, LabelOp, ListFilter, MailProvider, PageToken, Priority, Profile,
    ProviderError, ProviderResult, SyncCursor,
};

pub struct FakeProvider {
    state: Mutex<State>,
    page_size: usize,
    /// "Now" for `newer_than:` queries.
    now: Millis,
    pub fetch_calls: AtomicU64,
    pub fetched_messages: AtomicU64,
    /// Errors to return from the next write calls (modify, trash, send).
    write_failures: Mutex<Vec<ProviderError>>,
}

#[derive(Default)]
struct State {
    email: String,
    labels: Vec<Label>,
    messages: BTreeMap<String, FetchedMessage>,
    history: Vec<(u64, Change)>,
    next_history: u64,
    oldest_history: u64,
    sent_counter: u64,
    /// Server drafts: id → (raw, thread).
    drafts: BTreeMap<String, (Vec<u8>, Option<ThreadId>)>,
    draft_counter: u64,
}

impl FakeProvider {
    pub fn new(email: &str, now: Millis, page_size: usize) -> Self {
        Self {
            state: Mutex::new(State {
                email: email.to_owned(),
                next_history: 1000,
                oldest_history: 1000,
                ..Default::default()
            }),
            page_size: page_size.max(1),
            now,
            fetch_calls: AtomicU64::new(0),
            fetched_messages: AtomicU64::new(0),
            write_failures: Mutex::new(Vec::new()),
        }
    }

    /// Make the next `errors.len()` write calls fail, in order.
    pub fn fail_next_writes(&self, errors: Vec<ProviderError>) {
        let mut f = self.write_failures.lock().unwrap_or_else(|e| e.into_inner());
        *f = errors.into_iter().rev().collect();
    }

    fn injected_failure(&self) -> ProviderResult<()> {
        match self.write_failures.lock().unwrap_or_else(|e| e.into_inner()).pop() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The server's drafts, by id: raw bytes and thread.
    pub fn drafts(&self) -> BTreeMap<String, (Vec<u8>, Option<ThreadId>)> {
        self.state().drafts.clone()
    }

    pub fn set_labels(&self, labels: Vec<Label>) {
        self.state().labels = labels;
    }

    /// Add a message as if it had always been there (no history entry).
    pub fn seed(&self, m: FetchedMessage) {
        self.state().messages.insert(m.id.0.clone(), m);
    }

    /// A new message arrives (recorded in history).
    pub fn deliver(&self, m: FetchedMessage) {
        let mut s = self.state();
        let change =
            Change::MessageAdded { id: m.id.clone(), thread_id: m.thread_id.clone(), label_ids: m.label_ids.clone() };
        s.messages.insert(m.id.0.clone(), m);
        record(&mut s, change);
    }

    /// Labels change on the server (e.g. the user reads mail on the web).
    pub fn relabel(&self, id: &MessageId, add: &[LabelId], remove: &[LabelId]) {
        let mut s = self.state();
        apply_labels(&mut s, id, add, remove);
    }

    pub fn delete(&self, id: &MessageId) {
        let mut s = self.state();
        if s.messages.remove(id.as_str()).is_some() {
            record(&mut s, Change::MessageDeleted { id: id.clone() });
        }
    }

    /// Drop all history so any older cursor is rejected (as after a week).
    pub fn expire_history(&self) {
        let mut s = self.state();
        s.oldest_history = s.next_history;
        s.history.clear();
    }

    pub fn message(&self, id: &MessageId) -> Option<FetchedMessage> {
        self.state().messages.get(id.as_str()).cloned()
    }

    pub fn message_count(&self) -> usize {
        self.state().messages.len()
    }
}

fn record(s: &mut State, change: Change) {
    s.next_history += 1;
    let id = s.next_history;
    s.history.push((id, change));
}

fn apply_labels(s: &mut State, id: &MessageId, add: &[LabelId], remove: &[LabelId]) {
    let Some(m) = s.messages.get_mut(id.as_str()) else { return };
    let added: Vec<LabelId> = add.iter().filter(|l| !m.label_ids.contains(l)).cloned().collect();
    let removed: Vec<LabelId> = remove.iter().filter(|l| m.label_ids.contains(l)).cloned().collect();
    m.label_ids.retain(|l| !removed.contains(l));
    m.label_ids.extend(added.iter().cloned());
    if !added.is_empty() {
        record(s, Change::LabelsAdded { id: id.clone(), label_ids: added });
    }
    if !removed.is_empty() {
        record(s, Change::LabelsRemoved { id: id.clone(), label_ids: removed });
    }
}

fn matches(m: &FetchedMessage, filter: &ListFilter, now: Millis) -> bool {
    let has = |l: &str| m.label_ids.iter().any(|x| x.as_str() == l);
    if !filter.include_spam_trash && (has("SPAM") || has("TRASH")) {
        return false;
    }
    if !filter.label_ids.iter().all(|l| has(l.as_str())) {
        return false;
    }
    for term in filter.query.as_deref().unwrap_or_default().split_whitespace() {
        if term == "is:unread" && !has("UNREAD") {
            return false;
        }
        let days =
            term.strip_prefix("newer_than:").and_then(|d| d.strip_suffix('d')).and_then(|d| d.parse::<i64>().ok());
        if days.is_some_and(|days| m.internal_date < now - days * 86_400_000) {
            return false;
        }
    }
    true
}

#[async_trait]
impl MailProvider for FakeProvider {
    async fn profile(&self) -> ProviderResult<Profile> {
        let s = self.state();
        Ok(Profile {
            email: s.email.clone(),
            messages_total: s.messages.len() as u64,
            cursor: SyncCursor(s.next_history.to_string()),
        })
    }

    async fn list_labels(&self) -> ProviderResult<Vec<Label>> {
        Ok(self.state().labels.clone())
    }

    async fn list_message_ids(&self, filter: &ListFilter, page: Option<PageToken>) -> ProviderResult<IdPage> {
        let s = self.state();
        let mut matching: Vec<&FetchedMessage> = s.messages.values().filter(|m| matches(m, filter, self.now)).collect();
        matching.sort_by(|a, b| b.internal_date.cmp(&a.internal_date).then_with(|| b.id.cmp(&a.id)));
        let offset: usize = page.map(|p| p.0.parse().unwrap_or(0)).unwrap_or(0);
        let ids: Vec<(MessageId, ThreadId)> =
            matching.iter().skip(offset).take(self.page_size).map(|m| (m.id.clone(), m.thread_id.clone())).collect();
        let next = (offset + self.page_size < matching.len()).then(|| PageToken((offset + self.page_size).to_string()));
        Ok(IdPage { ids, next, estimated_total: Some(matching.len() as u64) })
    }

    async fn fetch_messages(&self, ids: &[MessageId], _priority: Priority) -> ProviderResult<Vec<FetchedMessage>> {
        self.fetch_calls.fetch_add(1, Ordering::SeqCst);
        let s = self.state();
        let out: Vec<FetchedMessage> = ids.iter().filter_map(|id| s.messages.get(id.as_str()).cloned()).collect();
        self.fetched_messages.fetch_add(out.len() as u64, Ordering::SeqCst);
        Ok(out)
    }

    async fn changes_since(&self, cursor: &SyncCursor) -> ProviderResult<ChangeSet> {
        let s = self.state();
        let from: u64 = cursor.0.parse().map_err(|_| ProviderError::Invalid(format!("bad cursor {}", cursor.0)))?;
        if from < s.oldest_history {
            return Err(ProviderError::CursorExpired);
        }
        let changes = s.history.iter().filter(|(id, _)| *id > from).map(|(_, c)| c.clone()).collect();
        Ok(ChangeSet { changes, cursor: SyncCursor(s.next_history.to_string()) })
    }

    async fn modify_labels(&self, op: &LabelOp) -> ProviderResult<()> {
        self.injected_failure()?;
        let mut s = self.state();
        for id in &op.message_ids {
            apply_labels(&mut s, id, &op.add, &op.remove);
        }
        Ok(())
    }

    async fn move_to_trash(&self, id: &MessageId) -> ProviderResult<()> {
        self.injected_failure()?;
        let mut s = self.state();
        apply_labels(&mut s, id, &[LabelId::new("TRASH")], &[LabelId::new("INBOX")]);
        Ok(())
    }

    async fn send(&self, raw: &[u8], thread: Option<&ThreadId>) -> ProviderResult<MessageId> {
        self.injected_failure()?;
        let mut s = self.state();
        s.sent_counter += 1;
        let id = MessageId(format!("sent{}", s.sent_counter));
        let thread_id = thread.cloned().unwrap_or_else(|| ThreadId(id.0.clone()));
        let text = String::from_utf8_lossy(raw).into_owned();
        // Just enough header reading to behave like Gmail for tests.
        let header = |name: &str| {
            text.lines()
                .take_while(|l| !l.is_empty())
                .find(|l| {
                    l.len() > name.len()
                        && l[..name.len()].eq_ignore_ascii_case(name)
                        && l[name.len()..].starts_with(':')
                })
                .map(|l| l[name.len() + 1..].trim().to_owned())
        };
        let m = FetchedMessage {
            id: id.clone(),
            thread_id: thread_id.clone(),
            label_ids: vec![LabelId::new("SENT")],
            internal_date: self.now,
            message_id_header: header("Message-ID").map(|v| v.trim_matches(['<', '>']).to_owned()),
            in_reply_to: header("In-Reply-To").map(|v| v.trim_matches(['<', '>']).to_owned()),
            subject: header("Subject").unwrap_or_default(),
            snippet: text.chars().take(100).collect(),
            body: Some(crate::FetchedBody { text: Some(text), html: None, attachments: vec![] }),
            ..Default::default()
        };
        let change = Change::MessageAdded { id: id.clone(), thread_id, label_ids: m.label_ids.clone() };
        s.messages.insert(id.0.clone(), m);
        record(&mut s, change);
        Ok(id)
    }

    async fn fetch_attachment(&self, message: &MessageId, attachment_id: &str) -> ProviderResult<Vec<u8>> {
        let s = self.state();
        let m = s.messages.get(message.as_str()).ok_or_else(|| ProviderError::NotFound(message.0.clone()))?;
        let found = m
            .body
            .as_ref()
            .and_then(|b| b.attachments.iter().find(|a| a.attachment_id.as_deref() == Some(attachment_id)))
            .ok_or_else(|| ProviderError::NotFound(attachment_id.to_owned()))?;
        Ok(format!("fake bytes of {}", found.filename).into_bytes())
    }

    async fn create_label(&self, name: &str, color: Option<(&str, &str)>) -> ProviderResult<Label> {
        self.injected_failure()?;
        let mut s = self.state();
        if s.labels.iter().any(|l| l.name.eq_ignore_ascii_case(name)) {
            return Err(ProviderError::Invalid(format!("label {name} exists")));
        }
        let label = Label {
            id: mail_domain::LabelId(format!("Label_{}", s.labels.len() + 1)),
            name: name.to_owned(),
            kind: mail_domain::LabelKind::User,
            color: color.map(|(bg, fg)| mail_domain::LabelColor { background: bg.into(), text: fg.into() }),
            visible: true,
        };
        s.labels.push(label.clone());
        Ok(label)
    }

    async fn save_draft(
        &self,
        existing: Option<&str>,
        raw: &[u8],
        thread: Option<&ThreadId>,
    ) -> ProviderResult<String> {
        self.injected_failure()?;
        let mut s = self.state();
        let id = match existing {
            Some(id) if !s.drafts.contains_key(id) => return Err(ProviderError::NotFound(format!("draft {id}"))),
            Some(id) => id.to_owned(),
            None => {
                s.draft_counter += 1;
                format!("r-draft{}", s.draft_counter)
            }
        };
        s.drafts.insert(id.clone(), (raw.to_vec(), thread.cloned()));
        Ok(id)
    }

    async fn delete_draft(&self, draft_id: &str) -> ProviderResult<()> {
        self.injected_failure()?;
        match self.state().drafts.remove(draft_id) {
            Some(_) => Ok(()),
            None => Err(ProviderError::NotFound(format!("draft {draft_id}"))),
        }
    }
}
