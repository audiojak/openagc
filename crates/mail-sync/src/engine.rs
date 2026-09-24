//! The sync engine (spec §7.4): bootstrap, prioritized backfill and
//! incremental history sync. Scheduling (poll intervals, app state) lives in
//! the core; this module performs one step at a time so each is testable.

use std::collections::BTreeSet;
use std::sync::Arc;

use mail_domain::{LabelId, MessageId};
use mail_store::{Db, MailWriter, ThreadChanges, queue, read};
use provider_api::{Change, ListFilter, MailProvider, PageToken, Priority, ProviderError};

use crate::convert::to_incoming;
use crate::error::{SyncError, SyncResult};

/// Backfill phases, most urgent first (spec §7.4). Each lists message ids
/// into the queue at its priority.
pub const PHASES: &[(u8, &[&str], Option<&str>)] = &[
    (0, &["INBOX"], Some("is:unread")),
    (1, &["INBOX"], None),
    (2, &[], Some("newer_than:30d")),
    (3, &[], Some("newer_than:365d")),
    (4, &[], None),
];
/// Phases listed before backfill starts, so the inbox fills first.
pub const INBOX_PHASES: usize = 2;
pub const BACKFILL_BATCH: usize = 50;

const KEY_CURSOR: &str = "history_cursor";
const KEY_BOOTSTRAPPED: &str = "bootstrap_listed";
const KEY_EMAIL: &str = "account_email";

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
}

pub struct SyncEngine {
    provider: Arc<dyn MailProvider>,
    db: Db,
    observer: Arc<dyn SyncObserver>,
}

impl SyncEngine {
    pub fn new(provider: Arc<dyn MailProvider>, db: Db, observer: Arc<dyn SyncObserver>) -> Self {
        Self { provider, db, observer }
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
        for phase in &PHASES[..INBOX_PHASES] {
            self.list_phase(*phase).await?;
        }
        self.report(SyncPhase::Listing).await;
        Ok(())
    }

    /// Bootstrap step 2 (slow, can run alongside backfill): queue the rest.
    pub async fn bootstrap_list_rest(&self) -> SyncResult<()> {
        for phase in &PHASES[INBOX_PHASES..] {
            self.list_phase(*phase).await?;
        }
        self.db.write(|tx| read::set_sync_state(tx, KEY_BOOTSTRAPPED, "1")).await?;
        self.report(SyncPhase::Backfilling).await;
        Ok(())
    }

    /// Fetch and store up to `max` queued messages, most urgent first.
    /// Returns how many were processed (0 when the queue is empty).
    pub async fn backfill_batch(&self, max: usize) -> SyncResult<usize> {
        let ids = self.db.read(move |c| queue::peek(c, max)).await?;
        if ids.is_empty() {
            return Ok(0);
        }
        let fetched = self.provider.fetch_messages(&ids, Priority::Background).await?;
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
                    w.upsert_message(m)?;
                    report.added += 1;
                }
                let fetched_ids: BTreeSet<&str> = incoming.iter().map(|m| m.id.as_str()).collect();
                let mut unknown: Vec<MessageId> = Vec::new();
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
                            } else {
                                unknown.push(id.clone());
                            }
                        }
                        Change::LabelsRemoved { id, label_ids } => {
                            if w.modify_message_labels(id, &[], label_ids)? {
                                report.relabeled += 1;
                            } else {
                                unknown.push(id.clone());
                            }
                        }
                    }
                }
                // Label changes for messages not stored yet: make sure a
                // backfill will fetch them with current labels.
                queue::enqueue(tx, 0, &unknown, false)?;
                read::set_sync_state(tx, KEY_CURSOR, &new_cursor)?;
                Ok((w.finish()?, report))
            })
            .await?;
        self.publish(&changes);
        self.report(SyncPhase::Incremental).await;
        Ok(report)
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
        for phase in PHASES {
            self.list_phase_with(*phase, true).await?;
        }
        self.db.write(|tx| read::set_sync_state(tx, KEY_BOOTSTRAPPED, "1")).await?;
        Ok(())
    }

    async fn list_phase(&self, phase: (u8, &[&str], Option<&str>)) -> SyncResult<()> {
        self.list_phase_with(phase, false).await
    }

    async fn list_phase_with(
        &self,
        (priority, labels, query): (u8, &[&str], Option<&str>),
        refetch: bool,
    ) -> SyncResult<()> {
        let filter = ListFilter {
            label_ids: labels.iter().map(|l| LabelId::new(*l)).collect(),
            query: query.map(str::to_owned),
            include_spam_trash: false,
        };
        let mut page: Option<PageToken> = None;
        loop {
            let result = self.provider.list_message_ids(&filter, page.take()).await?;
            let ids: Vec<MessageId> = result.ids.into_iter().map(|(id, _)| id).collect();
            self.db.write(move |tx| queue::enqueue(tx, priority, &ids, refetch)).await?;
            match result.next {
                Some(next) => page = Some(next),
                None => return Ok(()),
            }
        }
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
