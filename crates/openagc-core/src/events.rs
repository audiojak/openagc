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
    fn on_event(&self, event: CoreEvent);
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
    /// Warn/error log records from Rust, logged by Swift with `os.Logger`
    /// so unified-logging privacy stays under Swift's control (spec §17).
    Log {
        level: LogLevel,
        target: String,
        message: String,
    },
}

/// Cheap, cloneable handle for emitting events from anywhere in the core.
#[derive(Clone)]
pub struct EventBus {
    tx: mpsc::UnboundedSender<CoreEvent>,
}

impl EventBus {
    /// Start the dispatcher on `handle`. It stops when every `EventBus`
    /// clone is dropped.
    pub fn start(listener: Arc<dyn EventListener>, handle: &Handle) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        handle.spawn(dispatch(rx, listener));
        Self { tx }
    }

    /// Never blocks; events emitted after shutdown are dropped.
    pub fn emit(&self, event: CoreEvent) {
        let _ = self.tx.send(event);
    }
}

async fn dispatch(mut rx: mpsc::UnboundedReceiver<CoreEvent>, listener: Arc<dyn EventListener>) {
    let mut pending: BTreeMap<String, ChangeHint> = BTreeMap::new();
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
        let Some(event) = event else {
            flush(&mut pending, listener.as_ref());
            return;
        };
        match event {
            CoreEvent::ThreadsChanged { mailbox_id, hint } => {
                let merged = match pending.remove(&mailbox_id) {
                    Some(prev) => prev.merge(hint),
                    None => hint,
                };
                pending.insert(mailbox_id, merged);
                deadline.get_or_insert_with(|| Instant::now() + COALESCE_WINDOW);
            }
            other => listener.on_event(other),
        }
    }
}

fn flush(pending: &mut BTreeMap<String, ChangeHint>, listener: &dyn EventListener) {
    for (mailbox_id, hint) in std::mem::take(pending) {
        listener.on_event(CoreEvent::ThreadsChanged { mailbox_id, hint });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<CoreEvent>>);

    impl EventListener for Recorder {
        fn on_event(&self, event: CoreEvent) {
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
