//! Rust → Swift events (spec §4.3).
//!
//! Swift implements [`EventListener`]; the core calls it from a dispatcher
//! task on the core runtime. `ThreadsChanged` events are coalesced to at
//! most one per mailbox per [`COALESCE_WINDOW`], so a sync that touches 500
//! messages produces a handful of UI refreshes rather than 500.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep_until};

use crate::ErrorKind;

pub const COALESCE_WINDOW: Duration = Duration::from_millis(50);

/// Above this many ids a merged hint becomes `invalidate`: re-querying is
/// cheaper than patching that many rows.
pub const MAX_HINT_IDS: usize = 200;

#[uniffi::export(with_foreign)]
pub trait EventListener: Send + Sync {
    /// `account_id` is the account the event is about, or `None` for
    /// app-wide events (logs). Swift drops events for accounts the window
    /// is not showing, except the ones it surfaces app-wide (spec §7.7).
    fn on_event(&self, account_id: Option<String>, event: CoreEvent);
}

/// What changed in a mailbox's thread list, so views can patch rows in
/// place instead of reloading.
#[derive(Debug, Clone, Default, PartialEq, Eq, uniffi::Record)]
pub struct ChangeHint {
    pub inserted: Vec<String>,
    pub updated: Vec<String>,
    pub removed: Vec<String>,
    /// Too much changed to describe; re-query the visible window.
    pub invalidate: bool,
}

impl ChangeHint {
    pub fn invalidate() -> Self {
        Self { invalidate: true, ..Self::default() }
    }

    fn merge(self, newer: ChangeHint) -> ChangeHint {
        if self.invalidate || newer.invalidate {
            return ChangeHint::invalidate();
        }
        let mut inserted: BTreeSet<String> = self.inserted.into_iter().collect();
        let mut updated: BTreeSet<String> = self.updated.into_iter().collect();
        let mut removed: BTreeSet<String> = self.removed.into_iter().collect();
        for id in newer.inserted {
            removed.remove(&id);
            inserted.insert(id);
        }
        for id in newer.updated {
            if !inserted.contains(&id) {
                updated.insert(id);
            }
        }
        for id in newer.removed {
            updated.remove(&id);
            // Inserted then removed inside one window: the view never saw it.
            if !inserted.remove(&id) {
                removed.insert(id);
            }
        }
        if inserted.len() + updated.len() + removed.len() > MAX_HINT_IDS {
            return ChangeHint::invalidate();
        }
        ChangeHint {
            inserted: inserted.into_iter().collect(),
            updated: updated.into_iter().collect(),
            removed: removed.into_iter().collect(),
            invalidate: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NewMailInfo {
    pub message_id: String,
    pub thread_id: String,
    pub from: Option<crate::ffi::AddressInfo>,
    pub subject: String,
    pub snippet: String,
}

impl From<mail_sync::NewMail> for NewMailInfo {
    fn from(m: mail_sync::NewMail) -> Self {
        Self {
            message_id: m.id.0,
            thread_id: m.thread_id.0,
            from: m.from.map(Into::into),
            subject: m.subject,
            snippet: m.snippet,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum SyncState {
    Idle,
    Bootstrapping,
    Syncing,
    Offline,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum LogLevel {
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq, uniffi::Enum)]
pub enum CoreEvent {
    ThreadsChanged {
        mailbox_id: String,
        hint: ChangeHint,
    },
    SyncStatus {
        state: SyncState,
        pending: u32,
    },
    OutboxStatus {
        pending: u32,
        failed: u32,
    },
    Error {
        kind: ErrorKind,
        message: String,
    },
    /// Mail that just arrived, unread in the Inbox (spec §14.7). Only from
    /// incremental sync, so a first sync never floods notifications.
    NewMail {
        messages: Vec<NewMailInfo>,
    },
    /// A routine was saved, deleted, ran or finished; re-read the list.
    RoutinesChanged,
    /// One agent session's events from one 16 ms frame (spec §9.5).
    AgentEvents {
        session_id: String,
        events: Vec<crate::agents::AgentEventInfo>,
    },
    /// Warn/error log records from Rust, logged by Swift with `os.Logger`
    /// so unified-logging privacy stays under Swift's control (spec §17).
    Log {
        level: LogLevel,
        target: String,
        message: String,
    },
}

/// Cheap, cloneable handle for emitting events from anywhere in the core.
/// A bus may be tagged with an account; its events carry that id.
#[derive(Clone)]
pub struct EventBus {
    tx: mpsc::UnboundedSender<(Option<String>, CoreEvent)>,
    account: Option<String>,
}

impl EventBus {
    /// Start the dispatcher on `handle`. It stops when every `EventBus`
    /// clone is dropped.
    pub fn start(listener: Arc<dyn EventListener>, handle: &Handle) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        handle.spawn(dispatch(rx, listener));
        Self { tx, account: None }
    }

    /// The same bus, tagging what it emits with `account`.
    pub fn for_account(&self, account: Option<String>) -> Self {
        Self { tx: self.tx.clone(), account }
    }

    /// Never blocks; events emitted after shutdown are dropped.
    pub fn emit(&self, event: CoreEvent) {
        let _ = self.tx.send((self.account.clone(), event));
    }
}

type MailboxKey = (Option<String>, String);

async fn dispatch(mut rx: mpsc::UnboundedReceiver<(Option<String>, CoreEvent)>, listener: Arc<dyn EventListener>) {
    let mut pending: BTreeMap<MailboxKey, ChangeHint> = BTreeMap::new();
    let mut deadline: Option<Instant> = None;

    loop {
        let event = match deadline {
            Some(at) => tokio::select! {
                biased;
                event = rx.recv() => event,
                () = sleep_until(at) => {
                    flush(&mut pending, listener.as_ref());
                    deadline = None;
                    continue;
                }
            },
            None => rx.recv().await,
        };
        let Some((account, event)) = event else {
            flush(&mut pending, listener.as_ref());
            return;
        };
        match event {
            CoreEvent::ThreadsChanged { mailbox_id, hint } => {
                let key = (account, mailbox_id);
                let merged = match pending.remove(&key) {
                    Some(prev) => prev.merge(hint),
                    None => hint,
                };
                pending.insert(key, merged);
                deadline.get_or_insert_with(|| Instant::now() + COALESCE_WINDOW);
            }
            other => listener.on_event(account, other),
        }
    }
}

fn flush(pending: &mut BTreeMap<MailboxKey, ChangeHint>, listener: &dyn EventListener) {
    for ((account, mailbox_id), hint) in std::mem::take(pending) {
        listener.on_event(account, CoreEvent::ThreadsChanged { mailbox_id, hint });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<CoreEvent>>);

    impl EventListener for Recorder {
        fn on_event(&self, _account: Option<String>, event: CoreEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    fn changed(mailbox: &str, hint: ChangeHint) -> CoreEvent {
        CoreEvent::ThreadsChanged { mailbox_id: mailbox.into(), hint }
    }

    fn inserted(v: &[&str]) -> ChangeHint {
        ChangeHint { inserted: ids(v), ..Default::default() }
    }

    async fn settle() {
        tokio::time::sleep(COALESCE_WINDOW * 2).await;
    }

    #[tokio::test(start_paused = true)]
    async fn threads_changed_is_coalesced_per_mailbox_within_the_window() {
        let rec = Arc::new(Recorder::default());
        let bus = EventBus::start(rec.clone(), &Handle::current());
        for i in 0..500 {
            bus.emit(changed("inbox", inserted(&[&format!("t{i:03}")])));
        }
        bus.emit(changed("sent", inserted(&["s1"])));
        settle().await;
        let events = rec.0.lock().unwrap().clone();
        assert_eq!(events.len(), 2, "one event per mailbox, got {events:?}");
        // 500 ids exceed MAX_HINT_IDS, so the inbox hint degrades to invalidate.
        assert!(events.contains(&changed("inbox", ChangeHint::invalidate())));
        assert!(events.contains(&changed("sent", inserted(&["s1"]))));
    }

    #[tokio::test(start_paused = true)]
    async fn events_in_separate_windows_are_delivered_separately() {
        let rec = Arc::new(Recorder::default());
        let bus = EventBus::start(rec.clone(), &Handle::current());
        bus.emit(changed("inbox", inserted(&["a"])));
        settle().await;
        bus.emit(changed("inbox", inserted(&["b"])));
        settle().await;
        assert_eq!(
            *rec.0.lock().unwrap(),
            vec![changed("inbox", inserted(&["a"])), changed("inbox", inserted(&["b"]))]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn other_events_are_not_delayed() {
        let rec = Arc::new(Recorder::default());
        let bus = EventBus::start(rec.clone(), &Handle::current());
        bus.emit(CoreEvent::OutboxStatus { pending: 1, failed: 0 });
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert_eq!(*rec.0.lock().unwrap(), vec![CoreEvent::OutboxStatus { pending: 1, failed: 0 }]);
    }

    #[tokio::test(start_paused = true)]
    async fn pending_changes_flush_when_the_bus_is_dropped() {
        let rec = Arc::new(Recorder::default());
        let bus = EventBus::start(rec.clone(), &Handle::current());
        bus.emit(changed("inbox", inserted(&["a"])));
        drop(bus);
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert_eq!(*rec.0.lock().unwrap(), vec![changed("inbox", inserted(&["a"]))]);
    }

    #[test]
    fn merge_rules() {
        let base = ChangeHint { inserted: ids(&["a"]), updated: ids(&["u"]), ..Default::default() };
        // Inserted then removed in one window cancels out.
        let m = base.clone().merge(ChangeHint { removed: ids(&["a"]), ..Default::default() });
        assert_eq!(m, ChangeHint { updated: ids(&["u"]), ..Default::default() });
        // Updating something just inserted is still just an insert.
        let m = base.clone().merge(ChangeHint { updated: ids(&["a"]), ..Default::default() });
        assert_eq!(m, base);
        // Removing something updated drops the update.
        let m = base.clone().merge(ChangeHint { removed: ids(&["u"]), ..Default::default() });
        assert_eq!(m, ChangeHint { inserted: ids(&["a"]), removed: ids(&["u"]), ..Default::default() });
        // Invalidate wins.
        assert_eq!(base.merge(ChangeHint::invalidate()), ChangeHint::invalidate());
    }
}
