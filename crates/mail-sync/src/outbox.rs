//! Local mutations and the outbox drain (spec §7.4).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mail_domain::{EmailAddress, LabelId, Millis, ThreadId, system_labels};
use mail_store::drafts::{self, DraftState};
use mail_store::outbox::{self, OutboxCounts, OutboxOp};
use mail_store::{Db, MailWriter, ThreadChanges};
use provider_api::{LabelOp, ProviderError};

use crate::engine::SyncEngine;
use crate::error::SyncResult;

/// Retries before an op is given up and rolled back.
pub const MAX_ATTEMPTS: u32 = 5;

/// A change the user (or an agent) makes to threads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalChange {
    Labels { thread_ids: Vec<ThreadId>, add: Vec<LabelId>, remove: Vec<LabelId> },
    Trash { thread_ids: Vec<ThreadId> },
}

impl LocalChange {
    pub fn archive(thread_ids: Vec<ThreadId>) -> Self {
        Self::Labels { thread_ids, add: vec![], remove: vec![LabelId::new(system_labels::INBOX)] }
    }
    pub fn move_to_inbox(thread_ids: Vec<ThreadId>) -> Self {
        Self::Labels { thread_ids, add: vec![LabelId::new(system_labels::INBOX)], remove: vec![] }
    }
    pub fn set_read(thread_ids: Vec<ThreadId>, read: bool) -> Self {
        let unread = vec![LabelId::new(system_labels::UNREAD)];
        if read {
            Self::Labels { thread_ids, add: vec![], remove: unread }
        } else {
            Self::Labels { thread_ids, add: unread, remove: vec![] }
        }
    }
    pub fn set_starred(thread_ids: Vec<ThreadId>, starred: bool) -> Self {
        let star = vec![LabelId::new(system_labels::STARRED)];
        if starred {
            Self::Labels { thread_ids, add: star, remove: vec![] }
        } else {
            Self::Labels { thread_ids, add: vec![], remove: star }
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DrainReport {
    pub sent: usize,
    pub retrying: usize,
    pub failed: usize,
}

pub fn now_millis() -> Millis {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as Millis).unwrap_or_default()
}

fn backoff(attempts: u32) -> Duration {
    Duration::from_secs(2u64.saturating_pow(attempts + 1).min(300))
}

/// Apply a change to the store, queueing it for the provider when `queue`
/// is set, in one transaction. Used directly for accounts with no provider
/// (the demo mailbox) and through [`SyncEngine::apply_change`] otherwise.
pub async fn apply_local_change(db: &Db, change: LocalChange, queue: bool) -> SyncResult<ThreadChanges> {
    let now = now_millis();
    Ok(db
        .write(move |tx| {
            let (op, changes) = match change {
                LocalChange::Labels { thread_ids, add, remove } => {
                    let affected = outbox::affected_messages(tx, &thread_ids, &add, &remove)?;
                    let mut w = MailWriter::new(tx);
                    for (m, _) in &affected {
                        w.modify_message_labels(m, &add, &remove)?;
                    }
                    let changes = w.finish()?;
                    let message_ids = affected.into_iter().map(|(m, _)| m).collect::<Vec<_>>();
                    ((!message_ids.is_empty()).then_some(OutboxOp::ModifyLabels { message_ids, add, remove }), changes)
                }
                LocalChange::Trash { thread_ids } => {
                    let trash = vec![LabelId::new(system_labels::TRASH)];
                    let inbox = vec![LabelId::new(system_labels::INBOX)];
                    let affected = outbox::affected_messages(tx, &thread_ids, &trash, &[])?;
                    let mut w = MailWriter::new(tx);
                    for (m, _) in &affected {
                        w.modify_message_labels(m, &trash, &inbox)?;
                    }
                    let changes = w.finish()?;
                    let message_ids = affected.iter().map(|(m, _)| m.clone()).collect::<Vec<_>>();
                    ((!message_ids.is_empty()).then_some(OutboxOp::Trash { message_ids, previous: affected }), changes)
                }
            };
            if queue && let Some(op) = op {
                outbox::enqueue(tx, &op, now)?;
            }
            Ok(changes)
        })
        .await?)
}

impl SyncEngine {
    /// Apply a change locally and queue it for the provider. Returns what
    /// changed for the UI (also published to the observer).
    pub async fn apply_change(&self, change: LocalChange, queue: bool) -> SyncResult<ThreadChanges> {
        let changes = apply_local_change(self.db(), change, queue).await?;
        self.publish_changes(&changes);
        Ok(changes)
    }

    /// Send every ready op to the provider, oldest first. Transient failures
    /// are retried later with backoff; after [`MAX_ATTEMPTS`], or on a
    /// permanent error, the op is rolled back locally and marked failed.
    pub async fn drain_outbox(&self) -> SyncResult<DrainReport> {
        let _serialized = self.drain_lock.lock().await;
        let mut report = DrainReport::default();
        loop {
            let now = now_millis();
            let Some(queued) = self.db().read(move |c| outbox::next_ready(c, now)).await? else {
                return Ok(report);
            };
            let result = match &queued.op {
                OutboxOp::ModifyLabels { message_ids, add, remove } => {
                    self.provider()
                        .modify_labels(&LabelOp {
                            message_ids: message_ids.clone(),
                            add: add.clone(),
                            remove: remove.clone(),
                        })
                        .await
                }
                OutboxOp::Trash { message_ids, .. } => {
                    let mut result = Ok(());
                    for m in message_ids {
                        if let Err(e) = self.provider().move_to_trash(m).await {
                            result = Err(e);
                            break;
                        }
                    }
                    result
                }
                OutboxOp::Send { raw, thread_id, .. } => match crate::compose::decode_raw(raw) {
                    Some(bytes) => self.provider().send(&bytes, thread_id.as_ref()).await.map(|_| ()),
                    None => Err(ProviderError::Invalid("queued message is corrupt".into())),
                },
                OutboxOp::SyncDraft { draft_id, from } => self.mirror_draft(*draft_id, from).await?,
                OutboxOp::DeleteDraft { gmail_draft_id } => self.provider().delete_draft(gmail_draft_id).await,
            };
            let id = queued.id;
            match result {
                Ok(()) => {
                    self.db().write(move |tx| outbox::complete(tx, id)).await?;
                    report.sent += 1;
                }
                // A message deleted on the server: nothing left to change.
                Err(ProviderError::NotFound(_)) if !matches!(queued.op, OutboxOp::Send { .. }) => {
                    self.db().write(move |tx| outbox::complete(tx, id)).await?;
                    report.sent += 1;
                }
                Err(e) if e.is_transient() && queued.attempts + 1 < MAX_ATTEMPTS => {
                    let at = now + backoff(queued.attempts).as_millis() as Millis;
                    let message = e.to_string();
                    self.db().write(move |tx| outbox::retry_later(tx, id, at, &message)).await?;
                    report.retrying += 1;
                    // Later ops wait: order matters (archive then unarchive).
                    return Ok(report);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "outbox op failed permanently; rolling back");
                    let message = e.to_string();
                    let changes = self.db().write(move |tx| outbox::fail(tx, id, &message)).await?;
                    self.publish_changes(&changes);
                    report.failed += 1;
                    if matches!(e, ProviderError::Unauthorized) {
                        return Err(e.into());
                    }
                }
            }
        }
    }

    /// Create or replace a draft's server copy. A draft deleted or being
    /// sent by now needs nothing; a server copy deleted elsewhere is
    /// recreated.
    async fn mirror_draft(&self, draft_id: i64, from: &EmailAddress) -> SyncResult<Result<(), ProviderError>> {
        let Some(draft) = self.db().read(move |c| drafts::get(c, draft_id)).await? else {
            return Ok(Ok(()));
        };
        if draft.state == DraftState::Sending {
            return Ok(Ok(()));
        }
        let existing = draft.gmail_draft_id.clone();
        let thread = draft.thread_id.clone().map(ThreadId);
        // A draft that cannot be built (an attachment file gone) fails this
        // op rather than stalling the queue behind it.
        let raw = match crate::compose::draft_raw(self.db(), draft, from).await {
            Ok(raw) => raw,
            Err(e) => return Ok(Err(ProviderError::Invalid(e.to_string()))),
        };
        let provider = self.provider();
        let saved = match provider.save_draft(existing.as_deref(), &raw, thread.as_ref()).await {
            Err(ProviderError::NotFound(_)) if existing.is_some() => {
                provider.save_draft(None, &raw, thread.as_ref()).await
            }
            other => other,
        };
        let gmail_id = match saved {
            Ok(id) => id,
            Err(e) => return Ok(Err(e)),
        };
        let now = now_millis();
        self.db()
            .write(move |tx| {
                if drafts::get(tx, draft_id)?.is_some() {
                    drafts::set_gmail_draft_id(tx, draft_id, Some(&gmail_id))
                } else {
                    // Discarded while we were uploading: remove the copy too.
                    outbox::enqueue(tx, &OutboxOp::DeleteDraft { gmail_draft_id: gmail_id }, now).map(|_| ())
                }
            })
            .await?;
        Ok(Ok(()))
    }

    /// When the next op waiting on a retry becomes ready.
    pub async fn next_outbox_retry(&self) -> SyncResult<Option<Millis>> {
        Ok(self.db().read(outbox::next_retry_at).await?)
    }

    pub async fn outbox_counts(&self) -> SyncResult<OutboxCounts> {
        Ok(self.db().read(outbox::counts).await?)
    }
}
