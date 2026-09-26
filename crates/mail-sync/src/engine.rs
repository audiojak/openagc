//! The sync engine (spec §7.4): bootstrap, prioritized backfill and
//! incremental history sync. Scheduling (poll intervals, app state) lives in
//! the core; this module performs one step at a time so each is testable.

use std::collections::BTreeSet;
use std::sync::Arc;

use mail_domain::{EmailAddress, LabelId, MessageId, Millis, ThreadId, system_labels};
use mail_store::{Db, IncomingMessage, MailWriter, ThreadChanges, queue, read};
use provider_api::{Change, ListFilter, MailProvider, PageToken, Priority, ProviderError};

use crate::convert::to_incoming;
use crate::error::{SyncError, SyncResult};

/// How far back the initial sync downloads mail (spec §7.4 amendment).
/// The inbox and the last 30 days always come down; older mail only within
/// the window. Everything else stays on the server until the window widens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SyncWindow {
    Month,
    #[default]
    HalfYear,
    Year,
    Everything,
}

impl SyncWindow {
    pub const ALL: [SyncWindow; 4] = [Self::Month, Self::HalfYear, Self::Year, Self::Everything];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Month => "1m",
            Self::HalfYear => "6m",
            Self::Year => "1y",
            Self::Everything => "all",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|w| w.as_str() == s)
    }
}

/// One backfill phase: a priority and the provider list filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Phase {
    pub priority: u8,
    pub labels: &'static [&'static str],
    pub query: Option<String>,
}

/// Backfill phases for a window, most urgent first (spec §7.4). Each lists
/// message ids into the queue at its priority.
pub fn phases_for(window: SyncWindow) -> Vec<Phase> {
    let mut phases = vec![
        Phase { priority: 0, labels: &["INBOX"], query: Some("is:unread".into()) },
        Phase { priority: 1, labels: &["INBOX"], query: None },
        Phase { priority: 2, labels: &[], query: Some("newer_than:30d".into()) },
    ];
    match window {
        SyncWindow::Month => {}
        SyncWindow::HalfYear => phases.push(Phase { priority: 3, labels: &[], query: Some("newer_than:180d".into()) }),
        SyncWindow::Year => phases.push(Phase { priority: 3, labels: &[], query: Some("newer_than:365d".into()) }),
        SyncWindow::Everything => {
            phases.push(Phase { priority: 3, labels: &[], query: Some("newer_than:365d".into()) });
            phases.push(Phase { priority: 4, labels: &[], query: None });
        }
    }
    phases
}

/// Phases listed before backfill starts, so the inbox fills first.
pub const INBOX_PHASES: usize = 2;
/// Phases every window shares; the rest depend on the window.
const FIXED_PHASES: usize = 3;
pub const BACKFILL_BATCH: usize = 50;

const KEY_CURSOR: &str = "history_cursor";
const KEY_BOOTSTRAPPED: &str = "bootstrap_listed";
const KEY_EMAIL: &str = "account_email";
pub const KEY_WINDOW: &str = "sync_window";

/// Receives what sync changed; the core turns it into UI events.
pub trait SyncObserver: Send + Sync {
    fn threads_changed(&self, changes: &ThreadChanges);
    fn progress(&self, _progress: SyncProgress) {}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncPhase {
    Listing,
    Backfilling,
    Incremental,
    Idle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncProgress {
    pub phase: SyncPhase,
    /// Messages still waiting for a full fetch.
    pub queued: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IncrementalReport {
    pub added: usize,
    pub deleted: usize,
    pub relabeled: usize,
    /// Messages that arrived since the last sync, unread in the Inbox and
    /// not sent by the user: what a new-mail notification is about.
    pub new_mail: Vec<NewMail>,
    /// Label changes made outside OpenAGC.
    pub external_label_changes: Vec<ExternalLabelChange>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMail {
    pub id: MessageId,
    pub thread_id: ThreadId,
    pub from: Option<EmailAddress>,
    pub subject: String,
    pub snippet: String,
}

impl NewMail {
    /// `m` is new mail worth announcing if it is unread in the Inbox, not
    /// from the user, and was not already stored (e.g. by a backfill).
    fn from_incoming(m: &IncomingMessage, already_stored: bool) -> Option<Self> {
        let has = |l: &str| m.label_ids.iter().any(|x| x.as_str() == l);
        let fresh = !already_stored
            && has(system_labels::INBOX)
            && has(system_labels::UNREAD)
            && !has(system_labels::SENT)
            && !has(system_labels::SPAM)
            && !has(system_labels::TRASH);
        fresh.then(|| Self {
            id: m.id.clone(),
            thread_id: m.thread_id.clone(),
            from: m.from.clone(),
            subject: m.subject.clone(),
            snippet: m.snippet.clone(),
        })
    }
}

pub struct SyncEngine {
    provider: Arc<dyn MailProvider>,
    db: Db,
    observer: Arc<dyn SyncObserver>,
    /// Serializes outbox drains so one op is never sent twice.
    pub(crate) drain_lock: tokio::sync::Mutex<()>,
    /// Label changes OpenAGC itself pushed recently, so history sync can
    /// tell them from changes made elsewhere (spec §11.6).
    pub(crate) own_changes: std::sync::Mutex<Vec<OwnChange>>,
}

/// One label change the outbox pushed.
#[derive(Debug, Clone)]
pub(crate) struct OwnChange {
    pub message: MessageId,
    pub label: LabelId,
    pub added: bool,
    pub at: Millis,
}

/// A label change seen in history that OpenAGC did not make: another
/// client, a filter, or a cloud routine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalLabelChange {
    pub message: MessageId,
    pub thread: ThreadId,
    pub added: Vec<LabelId>,
    pub removed: Vec<LabelId>,
}

/// How long an own change is remembered.
const OWN_CHANGE_WINDOW: Millis = 2 * 60 * 60 * 1000;

impl SyncEngine {
    pub fn new(provider: Arc<dyn MailProvider>, db: Db, observer: Arc<dyn SyncObserver>) -> Self {
        Self {
            provider,
            db,
            observer,
            drain_lock: tokio::sync::Mutex::new(()),
            own_changes: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// True until the first bootstrap has listed every phase.
    pub async fn needs_bootstrap(&self) -> SyncResult<bool> {
        Ok(self.db.read(|c| read::sync_state(c, KEY_BOOTSTRAPPED)).await?.is_none())
    }

    pub async fn account_email(&self) -> SyncResult<Option<String>> {
        Ok(self.db.read(|c| read::sync_state(c, KEY_EMAIL)).await?)
    }

    /// Bootstrap step 1 (fast): refresh labels, record the history cursor
    /// before listing anything, and queue the inbox phases. Incremental sync
    /// may run as soon as this returns.
    pub async fn bootstrap_prepare(&self) -> SyncResult<()> {
        self.refresh_labels().await?;
        let profile = self.provider.profile().await?;
        let cursor = profile.cursor.0.clone();
        let email = profile.email.clone();
        self.db
            .write(move |tx| {
                read::set_sync_state(tx, KEY_EMAIL, &email)?;
                // Keep an existing cursor on resume; a fresh one on first run.
                if read::sync_state(tx, KEY_CURSOR)?.is_none() {
                    read::set_sync_state(tx, KEY_CURSOR, &cursor)?;
                }
                Ok(())
            })
            .await?;
        let window = self.window().await?;
        // Record the window so a later default change does not widen it.
        self.db.write(move |tx| read::set_sync_state(tx, KEY_WINDOW, window.as_str())).await?;
        let phases = phases_for(window);
        for phase in &phases[..INBOX_PHASES] {
            self.list_phase(phase).await?;
        }
        self.report(SyncPhase::Listing).await;
        Ok(())
    }

    /// An account synced before windows existed has none recorded: apply
    /// the default once, which trims its queue to the window.
    pub async fn ensure_window(&self) -> SyncResult<()> {
        if self.db.read(|c| read::sync_state(c, KEY_WINDOW)).await?.is_none() {
            tracing::info!(window = SyncWindow::default().as_str(), "applying the default sync window");
            self.set_window(SyncWindow::default()).await?;
        }
        Ok(())
    }

    /// Bootstrap step 2 (slow, can run alongside backfill): queue the rest.
    pub async fn bootstrap_list_rest(&self) -> SyncResult<()> {
        let phases = phases_for(self.window().await?);
        for phase in &phases[INBOX_PHASES..] {
            self.list_phase(phase).await?;
        }
        self.db.write(|tx| read::set_sync_state(tx, KEY_BOOTSTRAPPED, "1")).await?;
        self.report(SyncPhase::Backfilling).await;
        Ok(())
    }

    /// How far back this account downloads mail.
    pub async fn window(&self) -> SyncResult<SyncWindow> {
        let stored = self.db.read(|c| read::sync_state(c, KEY_WINDOW)).await?;
        Ok(stored.as_deref().and_then(SyncWindow::parse).unwrap_or_default())
    }

    /// Change the window. Queued fetches beyond the shared phases are
    /// dropped and the window's own phases re-listed, so widening
    /// downloads more and narrowing stops downloading older mail. Mail
    /// already stored is kept either way.
    pub async fn set_window(&self, window: SyncWindow) -> SyncResult<()> {
        let bootstrapped = self
            .db
            .write(move |tx| {
                read::set_sync_state(tx, KEY_WINDOW, window.as_str())?;
                queue::clear_from_priority(tx, FIXED_PHASES as u8)?;
                read::sync_state(tx, KEY_BOOTSTRAPPED)
            })
            .await?
            .is_some();
        if bootstrapped {
            for phase in &phases_for(window)[FIXED_PHASES..] {
                self.list_phase(phase).await?;
            }
            self.report(SyncPhase::Backfilling).await;
        }
        Ok(())
    }

    /// Fetch and store up to `max` queued messages, most urgent first.
    /// Returns how many were processed (0 when the queue is empty).
    pub async fn backfill_batch(&self, max: usize) -> SyncResult<usize> {
        let ids = self.db.read(move |c| queue::peek(c, max)).await?;
        if ids.is_empty() {
            return Ok(0);
        }
        tracing::debug!(count = ids.len(), "backfill batch: fetching");
        let fetched = self.provider.fetch_messages(&ids, Priority::Background).await?;
        tracing::debug!(count = fetched.len(), "backfill batch: storing");
        let incoming: Vec<_> = fetched.into_iter().map(to_incoming).collect();
        let processed = ids.len();
        let changes = self
            .db
            .write(move |tx| {
                let mut w = MailWriter::new(tx);
                for m in &incoming {
                    w.upsert_message(m)?;
                }
                // Ids the provider no longer has are dropped from the queue too.
                queue::remove(tx, &ids)?;
                w.finish()
            })
            .await?;
        self.publish(&changes);
        self.report(SyncPhase::Backfilling).await;
        Ok(processed)
    }

    /// Drain the queue completely (tests and small mailboxes).
    pub async fn backfill_all(&self) -> SyncResult<usize> {
        let mut total = 0;
        loop {
            let n = self.backfill_batch(BACKFILL_BATCH).await?;
            if n == 0 {
                return Ok(total);
            }
            total += n;
        }
    }

    /// Apply the provider's history since the stored cursor. On an expired
    /// cursor the store is queued for a full resync and
    /// [`SyncError::ResyncStarted`] is returned.
    pub async fn sync_incremental(&self) -> SyncResult<IncrementalReport> {
        let Some(cursor) = self.db.read(|c| read::sync_state(c, KEY_CURSOR)).await? else {
            return Err(SyncError::NotBootstrapped);
        };
        // Push local intent first so history does not appear to undo it.
        self.drain_outbox().await?;
        let set = match self.provider.changes_since(&provider_api::SyncCursor(cursor)).await {
            Ok(set) => set,
            Err(ProviderError::CursorExpired) => {
                self.start_resync().await?;
                return Err(SyncError::ResyncStarted);
            }
            Err(e) => return Err(e.into()),
        };

        // Fetch every message added since the cursor (and any we are told
        // about but do not have), then apply all changes in one transaction.
        let mut to_fetch: BTreeSet<MessageId> = BTreeSet::new();
        for change in &set.changes {
            if let Change::MessageAdded { id, .. } = change {
                to_fetch.insert(id.clone());
            }
        }
        let deleted: BTreeSet<MessageId> = set
            .changes
            .iter()
            .filter_map(|c| match c {
                Change::MessageDeleted { id } => Some(id.clone()),
                _ => None,
            })
            .collect();
        to_fetch.retain(|id| !deleted.contains(id));
        let fetch_ids: Vec<MessageId> = to_fetch.into_iter().collect();
        let fetched = if fetch_ids.is_empty() {
            vec![]
        } else {
            self.provider.fetch_messages(&fetch_ids, Priority::Background).await?
        };
        let incoming: Vec<_> = fetched.into_iter().map(to_incoming).collect();
        let new_cursor = set.cursor.0.clone();
        let changes_in = set.changes;

        let (changes, report) = self
            .db
            .write(move |tx| {
                let mut report = IncrementalReport::default();
                let mut w = MailWriter::new(tx);
                for m in &incoming {
                    let stored =
                        tx.prepare_cached("SELECT 1 FROM messages WHERE gmail_id = ?1")?.exists([m.id.as_str()])?;
                    report.new_mail.extend(NewMail::from_incoming(m, stored));
                    w.upsert_message(m)?;
                    report.added += 1;
                }
                let fetched_ids: BTreeSet<&str> = incoming.iter().map(|m| m.id.as_str()).collect();
                let mut unknown: Vec<MessageId> = Vec::new();
                let mut external: Vec<(MessageId, Vec<LabelId>, bool)> = Vec::new();
                for change in &changes_in {
                    match change {
                        Change::MessageAdded { .. } => {}
                        Change::MessageDeleted { id } => {
                            if w.delete_message(id)? {
                                report.deleted += 1;
                            }
                            queue::remove(tx, std::slice::from_ref(id))?;
                        }
                        // A freshly fetched message already has current labels.
                        Change::LabelsAdded { id, .. } | Change::LabelsRemoved { id, .. }
                            if fetched_ids.contains(id.as_str()) => {}
                        Change::LabelsAdded { id, label_ids } => {
                            if w.modify_message_labels(id, label_ids, &[])? {
                                report.relabeled += 1;
                                external.push((id.clone(), label_ids.clone(), true));
                            } else {
                                unknown.push(id.clone());
                            }
                        }
                        Change::LabelsRemoved { id, label_ids } => {
                            if w.modify_message_labels(id, &[], label_ids)? {
                                report.relabeled += 1;
                                external.push((id.clone(), label_ids.clone(), false));
                            } else {
                                unknown.push(id.clone());
                            }
                        }
                    }
                }
                // Label changes for messages not stored yet: make sure a
                // backfill will fetch them with current labels.
                queue::enqueue_urgent(tx, &unknown)?;
                read::set_sync_state(tx, KEY_CURSOR, &new_cursor)?;
                // Thread ids for the label changes, while we hold the store.
                let mut threads: Vec<(MessageId, ThreadId, Vec<LabelId>, bool)> = Vec::new();
                for (id, labels, added) in external {
                    if let Some(m) = read::get_message(tx, &id)? {
                        threads.push((id, m.thread_id, labels, added));
                    }
                }
                Ok((w.finish()?, (report, threads)))
            })
            .await?;
        let (mut report, labeled) = report;
        report.external_label_changes = self.not_ours(labeled);
        self.publish(&changes);
        self.report(SyncPhase::Incremental).await;
        Ok(report)
    }

    /// Drop the changes OpenAGC's outbox made; group the rest per message.
    fn not_ours(&self, labeled: Vec<(MessageId, ThreadId, Vec<LabelId>, bool)>) -> Vec<ExternalLabelChange> {
        let now = crate::outbox::now_millis();
        let mut own = self.own_changes.lock().unwrap_or_else(|e| e.into_inner());
        own.retain(|c| now - c.at < OWN_CHANGE_WINDOW);
        let mut out: Vec<ExternalLabelChange> = Vec::new();
        for (message, thread, labels, added) in labeled {
            let theirs: Vec<LabelId> = labels
                .into_iter()
                .filter(|l| !own.iter().any(|c| c.message == message && c.label == *l && c.added == added))
                .collect();
            if theirs.is_empty() {
                continue;
            }
            let entry = match out.iter_mut().position(|e| e.message == message) {
                Some(i) => &mut out[i],
                None => {
                    out.push(ExternalLabelChange { message: message.clone(), thread, added: vec![], removed: vec![] });
                    out.last_mut().expect("just pushed")
                }
            };
            if added { entry.added.extend(theirs) } else { entry.removed.extend(theirs) }
        }
        out
    }

    /// Remember label changes the outbox just pushed.
    pub(crate) fn remember_own(&self, messages: &[MessageId], add: &[LabelId], remove: &[LabelId]) {
        let at = crate::outbox::now_millis();
        let mut own = self.own_changes.lock().unwrap_or_else(|e| e.into_inner());
        for m in messages {
            own.extend(add.iter().map(|l| OwnChange { message: m.clone(), label: l.clone(), added: true, at }));
            own.extend(remove.iter().map(|l| OwnChange { message: m.clone(), label: l.clone(), added: false, at }));
        }
    }

    /// Label list changes are not in history; refresh them wholesale.
    pub async fn refresh_labels(&self) -> SyncResult<()> {
        let labels = self.provider.list_labels().await?;
        let keep: Vec<LabelId> = labels.iter().map(|l| l.id.clone()).collect();
        let changes = self
            .db
            .write(move |tx| {
                let mut w = MailWriter::new(tx);
                w.upsert_labels(&labels)?;
                w.retain_labels(&keep)?;
                w.finish()
            })
            .await?;
        self.publish(&changes);
        Ok(())
    }

    /// The history cursor expired: take a fresh cursor and re-list
    /// everything. Stored messages are refetched so labels are current.
    async fn start_resync(&self) -> SyncResult<()> {
        tracing::warn!("history cursor expired; starting full resync");
        let profile = self.provider.profile().await?;
        let cursor = profile.cursor.0;
        self.db
            .write(move |tx| {
                read::set_sync_state(tx, KEY_CURSOR, &cursor)?;
                tx.execute("DELETE FROM sync_state WHERE key = ?1", [KEY_BOOTSTRAPPED])?;
                Ok(())
            })
            .await?;
        for phase in &phases_for(self.window().await?) {
            self.list_phase_with(phase, true).await?;
        }
        self.db.write(|tx| read::set_sync_state(tx, KEY_BOOTSTRAPPED, "1")).await?;
        Ok(())
    }

    async fn list_phase(&self, phase: &Phase) -> SyncResult<()> {
        self.list_phase_with(phase, false).await
    }

    async fn list_phase_with(&self, phase: &Phase, refetch: bool) -> SyncResult<()> {
        let priority = phase.priority;
        let filter = ListFilter {
            label_ids: phase.labels.iter().map(|l| LabelId::new(*l)).collect(),
            query: phase.query.clone(),
            include_spam_trash: false,
        };
        let mut page: Option<PageToken> = None;
        loop {
            tracing::debug!(priority, has_page = page.is_some(), "listing phase page");
            let result = self.provider.list_message_ids(&filter, page.take()).await?;
            let ids: Vec<MessageId> = result.ids.into_iter().map(|(id, _)| id).collect();
            self.db.write(move |tx| queue::enqueue(tx, priority, &ids, refetch)).await?;
            match result.next {
                Some(next) => page = Some(next),
                None => return Ok(()),
            }
        }
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    pub fn provider(&self) -> &dyn MailProvider {
        self.provider.as_ref()
    }

    pub(crate) fn publish_changes(&self, changes: &ThreadChanges) {
        self.publish(changes);
    }

    fn publish(&self, changes: &ThreadChanges) {
        if !changes.is_empty() {
            self.observer.threads_changed(changes);
        }
    }

    async fn report(&self, phase: SyncPhase) {
        let queued = self.db.read(queue::len).await.unwrap_or(0);
        let phase = if queued == 0 && phase == SyncPhase::Backfilling { SyncPhase::Idle } else { phase };
        self.observer.progress(SyncProgress { phase, queued });
    }
}
