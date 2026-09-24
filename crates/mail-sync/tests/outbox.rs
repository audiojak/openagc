//! Optimistic mutations and the outbox against the fake provider.

use std::sync::{Arc, Mutex};

use mail_domain::{EmailAddress, LabelId, MessageId, ThreadId};
use mail_store::{Db, ThreadChanges, consistency, outbox, read};
use mail_sync::{LocalChange, SyncEngine, SyncObserver};
use provider_api::fake::FakeProvider;
use provider_api::{FetchedBody, FetchedMessage, ProviderError};

const NOW: i64 = 1_790_000_000_000;

#[derive(Default)]
struct Recorder(Mutex<Vec<ThreadChanges>>);
impl SyncObserver for Recorder {
    fn threads_changed(&self, changes: &ThreadChanges) {
        self.0.lock().unwrap().push(changes.clone());
    }
}

fn message(id: &str, thread: &str, labels: &[&str]) -> FetchedMessage {
    FetchedMessage {
        id: MessageId::new(id),
        thread_id: ThreadId::new(thread),
        label_ids: labels.iter().map(|l| LabelId::new(*l)).collect(),
        internal_date: NOW,
        from: Some(EmailAddress::new(None, "a@example.com")),
        subject: format!("S {thread}"),
        body: Some(FetchedBody { text: Some("x".into()), html: None, attachments: vec![] }),
        ..Default::default()
    }
}

async fn setup(name: &str) -> (Arc<FakeProvider>, Db, Arc<Recorder>, SyncEngine) {
    let dir = std::env::temp_dir().join(format!("openagc-outbox-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir.join("mail.sqlite")).unwrap();
    let fake = Arc::new(FakeProvider::new("me@example.com", NOW, 50));
    // Thread a: two messages, only m1 in the inbox and unread.
    fake.seed(message("m1", "a", &["INBOX", "UNREAD"]));
    fake.seed(message("m2", "a", &["SENT"]));
    fake.seed(message("m3", "b", &["INBOX"]));
    let recorder = Arc::new(Recorder::default());
    let engine = SyncEngine::new(fake.clone(), db.clone(), recorder.clone());
    engine.bootstrap_prepare().await.unwrap();
    engine.bootstrap_list_rest().await.unwrap();
    engine.backfill_all().await.unwrap();
    recorder.0.lock().unwrap().clear();
    (fake, db, recorder, engine)
}

async fn inbox(db: &Db) -> Vec<String> {
    db.read(|c| read::list_threads(c, "INBOX", None, 10)).await.unwrap().rows.into_iter().map(|t| t.id.0).collect()
}

fn labels(fake: &FakeProvider, id: &str) -> Vec<String> {
    let mut l: Vec<String> = fake.message(&MessageId::new(id)).unwrap().label_ids.into_iter().map(|l| l.0).collect();
    l.sort();
    l
}

#[tokio::test]
async fn archive_is_applied_locally_at_once_and_reaches_the_server_on_drain() {
    let (fake, db, recorder, engine) = setup("archive").await;
    let changes = engine.apply_change(LocalChange::archive(vec![ThreadId::new("a")]), true).await.unwrap();
    assert!(changes.mailboxes["INBOX"].removed.contains("a"));
    assert_eq!(inbox(&db).await, vec!["b"], "local store reflects it immediately");
    assert_eq!(labels(&fake, "m1"), vec!["INBOX", "UNREAD"], "server not yet told");
    assert_eq!(recorder.0.lock().unwrap().len(), 1, "UI notified");

    // Only m1 is recorded: m2 was never in the inbox.
    let queued = db.read(|c| outbox::next_ready(c, i64::MAX)).await.unwrap().unwrap();
    assert!(
        matches!(&queued.op, outbox::OutboxOp::ModifyLabels { message_ids, .. } if message_ids == &vec![MessageId::new("m1")])
    );

    let report = engine.drain_outbox().await.unwrap();
    assert_eq!(report.sent, 1);
    assert_eq!(labels(&fake, "m1"), vec!["UNREAD"]);
    assert_eq!(engine.outbox_counts().await.unwrap(), outbox::OutboxCounts::default());
    assert!(db.read(consistency::check).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_permanent_failure_rolls_the_change_back() {
    let (fake, db, recorder, engine) = setup("rollback").await;
    engine.apply_change(LocalChange::set_read(vec![ThreadId::new("a")], true), true).await.unwrap();
    fake.fail_next_writes(vec![ProviderError::Forbidden("insufficient permission".into())]);
    let report = engine.drain_outbox().await.unwrap();
    assert_eq!(report.failed, 1);
    let (summary, _) = db.read(|c| read::get_thread(c, &ThreadId::new("a"))).await.unwrap().unwrap();
    assert_eq!(summary.unread_count, 1, "read state restored");
    assert_eq!(engine.outbox_counts().await.unwrap().failed, 1, "kept so the user can see it");
    assert!(recorder.0.lock().unwrap().len() >= 2, "UI told about both the change and the rollback");
    assert!(db.read(consistency::check).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_transient_failure_is_retried_in_order() {
    let (fake, db, _recorder, engine) = setup("retry").await;
    engine.apply_change(LocalChange::archive(vec![ThreadId::new("a")]), true).await.unwrap();
    engine.apply_change(LocalChange::move_to_inbox(vec![ThreadId::new("a")]), true).await.unwrap();
    fake.fail_next_writes(vec![ProviderError::Network("offline".into())]);
    let report = engine.drain_outbox().await.unwrap();
    assert_eq!((report.sent, report.retrying), (0, 1), "the second op waits behind the first");
    assert_eq!(engine.outbox_counts().await.unwrap().pending, 2);
    // Time passes.
    db.write(|tx| Ok(tx.execute("UPDATE outbox SET next_attempt_at = 0", [])?)).await.unwrap();
    let report = engine.drain_outbox().await.unwrap();
    assert_eq!(report.sent, 2);
    assert_eq!(labels(&fake, "m1"), vec!["INBOX", "UNREAD"], "archive then unarchive, in order");
}

#[tokio::test]
async fn trash_moves_messages_and_can_be_rolled_back() {
    let (fake, db, _recorder, engine) = setup("trash").await;
    engine.apply_change(LocalChange::Trash { thread_ids: vec![ThreadId::new("b")] }, true).await.unwrap();
    assert_eq!(inbox(&db).await, vec!["a"]);
    let trash = db.read(|c| read::list_threads(c, "TRASH", None, 10)).await.unwrap();
    assert_eq!(trash.rows.len(), 1);
    fake.fail_next_writes(vec![ProviderError::Invalid("bad request".into())]);
    engine.drain_outbox().await.unwrap();
    let mut restored = inbox(&db).await;
    restored.sort();
    assert_eq!(restored, vec!["a", "b"], "restored to the inbox");
    assert!(db.read(consistency::check).await.unwrap().is_empty());
}

#[tokio::test]
async fn incremental_sync_pushes_local_changes_before_reading_history() {
    let (fake, db, _recorder, engine) = setup("drain-first").await;
    engine.apply_change(LocalChange::archive(vec![ThreadId::new("b")]), true).await.unwrap();
    engine.sync_incremental().await.unwrap();
    assert_eq!(labels(&fake, "m3"), Vec::<String>::new(), "server archived");
    assert_eq!(inbox(&db).await, vec!["a"], "and history did not bring it back");
}

#[tokio::test]
async fn local_only_changes_are_not_queued() {
    let (_fake, _db, _recorder, engine) = setup("local").await;
    engine.apply_change(LocalChange::set_starred(vec![ThreadId::new("a")], true), false).await.unwrap();
    assert_eq!(engine.outbox_counts().await.unwrap().pending, 0);
}
