//! Runs the sync engine for the open account (spec §7.4): bootstrap, a
//! backfill loop and an incremental poll, all on the core runtime.
//!
//! Poll interval: 30 s while the app is active, 5 min in the background,
//! immediately on `sync_now` (foreground, wake, network regained). Transient
//! failures back off; an authorization failure stops sync and tells Swift.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use mail_store::ThreadChanges;
use mail_sync::{BACKFILL_BATCH, SyncEngine, SyncError, SyncObserver, SyncPhase, SyncProgress};
use provider_api::ProviderError;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::events::{ChangeHint, CoreEvent, EventBus, SyncState};
use crate::{CoreError, ErrorKind};

pub const ACTIVE_POLL: Duration = Duration::from_secs(30);
pub const BACKGROUND_POLL: Duration = Duration::from_secs(300);
pub const DRAFT_MIRROR_INTERVAL: Duration = Duration::from_secs(30);
const MAX_BACKOFF: Duration = Duration::from_secs(300);

/// Turns engine output into UI events.
pub(crate) struct EventObserver {
    pub events: EventBus,
}

impl SyncObserver for EventObserver {
    fn threads_changed(&self, changes: &ThreadChanges) {
        for (mailbox, change) in &changes.mailboxes {
            self.events.emit(CoreEvent::ThreadsChanged {
                mailbox_id: mailbox.clone(),
                hint: ChangeHint {
                    inserted: change.inserted.iter().cloned().collect(),
                    updated: change.updated.iter().cloned().collect(),
                    removed: change.removed.iter().cloned().collect(),
                    invalidate: false,
                },
            });
        }
    }

    fn progress(&self, progress: SyncProgress) {
        let state = match progress.phase {
            SyncPhase::Listing => SyncState::Bootstrapping,
            SyncPhase::Backfilling | SyncPhase::Incremental => SyncState::Syncing,
            SyncPhase::Idle => SyncState::Idle,
        };
        self.events.emit(CoreEvent::SyncStatus { state, pending: progress.queued.min(u32::MAX as u64) as u32 });
    }
}

pub(crate) struct SyncService {
    engine: Arc<SyncEngine>,
    events: EventBus,
    active: AtomicBool,
    poll_now: Notify,
    backfill_wake: Notify,
    outbox_wake: Notify,
    drafts_wake: Notify,
    tasks: std::sync::Mutex<Vec<JoinHandle<()>>>,
}

impl SyncService {
    pub fn start(engine: Arc<SyncEngine>, events: EventBus, handle: &tokio::runtime::Handle) -> Arc<Self> {
        let service = Arc::new(Self {
            engine,
            events,
            active: AtomicBool::new(true),
            poll_now: Notify::new(),
            backfill_wake: Notify::new(),
            outbox_wake: Notify::new(),
            drafts_wake: Notify::new(),
            tasks: std::sync::Mutex::new(Vec::new()),
        });
        let main = handle.spawn(service.clone().run());
        service.tasks.lock().unwrap_or_else(|e| e.into_inner()).push(main);
        service
    }

    pub fn set_active(&self, active: bool) {
        let was = self.active.swap(active, Ordering::SeqCst);
        if active && !was {
            self.poll_now.notify_one();
        }
    }

    pub fn engine(&self) -> &SyncEngine {
        &self.engine
    }

    /// A change was queued; push it now.
    pub fn outbox_changed(&self) {
        self.outbox_wake.notify_one();
    }

    /// Mirror edited drafts to the server now (the composer closed).
    pub fn flush_drafts(&self) {
        self.drafts_wake.notify_one();
    }

    pub fn sync_now(&self) {
        self.poll_now.notify_one();
        self.backfill_wake.notify_one();
    }

    pub fn stop(&self) {
        for task in self.tasks.lock().unwrap_or_else(|e| e.into_inner()).drain(..) {
            task.abort();
        }
    }

    async fn run(self: Arc<Self>) {
        // Bootstrap (resumable: prepare keeps an existing cursor).
        match self.engine.needs_bootstrap().await {
            Ok(true) => {
                self.status(SyncState::Bootstrapping, 0);
                if let Err(e) = self.retrying(|| self.engine.bootstrap_prepare()).await {
                    self.fail(e);
                    return;
                }
                let lister = self.clone();
                let task = tokio::spawn(async move {
                    match lister.retrying(|| lister.engine.bootstrap_list_rest()).await {
                        Ok(()) => lister.backfill_wake.notify_one(),
                        Err(e) => lister.fail(e),
                    }
                });
                self.tasks.lock().unwrap_or_else(|e| e.into_inner()).push(task);
            }
            Ok(false) => {}
            Err(e) => {
                self.fail(e);
                return;
            }
        }

        let backfiller = self.clone();
        let backfill = tokio::spawn(async move { backfiller.backfill_loop().await });
        let pusher = self.clone();
        let outbox = tokio::spawn(async move { pusher.outbox_loop().await });
        let mirror = self.clone();
        let drafts = tokio::spawn(async move { mirror.drafts_loop().await });
        self.tasks.lock().unwrap_or_else(|e| e.into_inner()).extend([backfill, outbox, drafts]);
        self.poll_loop().await;
    }

    async fn backfill_loop(self: Arc<Self>) {
        let mut backoff = Duration::from_secs(2);
        loop {
            match self.engine.backfill_batch(BACKFILL_BATCH).await {
                Ok(0) => {
                    backoff = Duration::from_secs(2);
                    // Nothing queued: sleep until something is.
                    tokio::select! {
                        () = self.backfill_wake.notified() => {}
                        () = tokio::time::sleep(Duration::from_secs(60)) => {}
                    }
                }
                Ok(_) => backoff = Duration::from_secs(2),
                Err(e) if e.is_transient() => {
                    self.offline_or_error(&e);
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
                Err(e) => {
                    self.fail(e);
                    return;
                }
            }
        }
    }

    /// Push queued changes as they are made; wake again at the next retry.
    async fn outbox_loop(self: Arc<Self>) {
        loop {
            match self.engine.drain_outbox().await {
                Ok(_) => {}
                Err(e) if matches!(e, SyncError::Provider(ProviderError::Unauthorized)) => {
                    self.fail(e);
                    return;
                }
                Err(e) => tracing::warn!(error = %e, "outbox drain failed"),
            }
            if let Ok(c) = self.engine.outbox_counts().await {
                self.events.emit(CoreEvent::OutboxStatus { pending: c.pending, failed: c.failed });
            }
            let wait = self
                .engine
                .next_outbox_retry()
                .await
                .ok()
                .flatten()
                .map(|at| Duration::from_millis((at - mail_sync::now_millis()).max(250) as u64))
                .unwrap_or(Duration::from_secs(60));
            tokio::select! {
                () = self.outbox_wake.notified() => {}
                () = tokio::time::sleep(wait) => {}
            }
        }
    }

    /// Every 30 s (or when a composer closes), queue a server update for
    /// each draft edited since the last one (spec §14.5).
    async fn drafts_loop(self: Arc<Self>) {
        loop {
            tokio::select! {
                () = self.drafts_wake.notified() => {}
                () = tokio::time::sleep(DRAFT_MIRROR_INTERVAL) => {}
            }
            let db = self.engine.db().clone();
            let email: String = match db.read(|c| mail_store::read::sync_state(c, "account_email")).await {
                Ok(Some(email)) => email,
                Ok(None) => continue,
                Err(e) => {
                    tracing::warn!(error = %e, "reading the account address failed");
                    continue;
                }
            };
            match mail_sync::schedule_draft_sync(&db, mail_domain::EmailAddress::new(None, &email)).await {
                Ok(0) => {}
                Ok(_) => self.outbox_wake.notify_one(),
                Err(e) => tracing::warn!(error = %e, "scheduling draft sync failed"),
            }
        }
    }

    async fn poll_loop(self: Arc<Self>) {
        let mut backoff = Duration::from_secs(5);
        loop {
            match self.engine.sync_incremental().await {
                Ok(_) => backoff = Duration::from_secs(5),
                Err(SyncError::ResyncStarted) => self.backfill_wake.notify_one(),
                Err(e) if e.is_transient() => {
                    self.offline_or_error(&e);
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                    continue;
                }
                Err(e) => {
                    self.fail(e);
                    return;
                }
            }
            // New mail may have queued unfetched ids.
            self.backfill_wake.notify_one();
            let interval = if self.active.load(Ordering::SeqCst) { ACTIVE_POLL } else { BACKGROUND_POLL };
            tokio::select! {
                () = self.poll_now.notified() => {}
                () = tokio::time::sleep(interval) => {}
            }
        }
    }

    /// Retry a transient-failing step with backoff; return other errors.
    async fn retrying<F, Fut>(&self, mut step: F) -> Result<(), SyncError>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<(), SyncError>>,
    {
        let mut backoff = Duration::from_secs(2);
        loop {
            match step().await {
                Err(e) if e.is_transient() => {
                    self.offline_or_error(&e);
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
                other => return other,
            }
        }
    }

    fn status(&self, state: SyncState, pending: u32) {
        self.events.emit(CoreEvent::SyncStatus { state, pending });
    }

    fn offline_or_error(&self, e: &SyncError) {
        tracing::warn!(error = %e, "sync step failed; will retry");
        let offline = matches!(e, SyncError::Provider(ProviderError::Network(_)));
        self.status(if offline { SyncState::Offline } else { SyncState::Error }, 0);
    }

    fn fail(&self, e: SyncError) {
        tracing::error!(error = %e, "sync stopped");
        self.status(SyncState::Error, 0);
        let kind = match &e {
            SyncError::Provider(ProviderError::Unauthorized) => ErrorKind::Auth,
            SyncError::Provider(ProviderError::Forbidden(_)) => ErrorKind::PermissionDenied,
            SyncError::Store(_) => ErrorKind::Storage,
            _ => ErrorKind::Network,
        };
        let CoreError::Failed { kind, message } = CoreError::new(kind, e.to_string());
        self.events.emit(CoreEvent::Error { kind, message });
    }
}
