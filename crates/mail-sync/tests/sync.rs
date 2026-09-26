//! Sync engine against the in-memory fake provider.

use std::sync::{Arc, Mutex};

use mail_domain::{EmailAddress, Label, LabelId, LabelKind, MessageId, ThreadId};
use mail_store::{ARCHIVE_LABEL, Db, ThreadChanges, consistency, queue, read};
use mail_sync::{SyncEngine, SyncError, SyncObserver, SyncPhase, SyncProgress, SyncWindow};
use provider_api::fake::FakeProvider;
use provider_api::{FetchedBody, FetchedMessage};

const NOW: i64 = 1_790_000_000_000;
const DAY: i64 = 86_400_000;

#[derive(Default)]
struct Recorder {
    changes: Mutex<Vec<ThreadChanges>>,
    progress: Mutex<Vec<SyncProgress>>,
}

impl SyncObserver for Recorder {
    fn threads_changed(&self, changes: &ThreadChanges) {
        self.changes.lock().unwrap().push(changes.clone());
    }
    fn progress(&self, progress: SyncProgress) {
        self.progress.lock().unwrap().push(progress);
    }
}

fn message(id: &str, thread: &str, age_days: i64, labels: &[&str]) -> FetchedMessage {
    FetchedMessage {
        id: MessageId::new(id),
        thread_id: ThreadId::new(thread),
        label_ids: labels.iter().map(|l| LabelId::new(*l)).collect(),
        snippet: format!("snippet {id}"),
        internal_date: NOW - age_days * DAY,
        from: Some(EmailAddress::new(Some("Sender"), "sender@example.com")),
        to: vec![EmailAddress::new(None, "me@example.com")],
        subject: format!("Subject {thread}"),
        body: Some(FetchedBody {
            text: Some(format!("body of {id}")),
            html: Some(format!("<p>body of {id}</p><script>alert(1)</script><img src=\"https://t.example/p.gif\">")),
            attachments: vec![],
        }),
        ..Default::default()
    }
}

fn setup(name: &str) -> (Arc<FakeProvider>, Db, Arc<Recorder>, SyncEngine) {
    let dir = std::env::temp_dir().join(format!("openagc-sync-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir.join("mail.sqlite")).unwrap();
    let fake = Arc::new(FakeProvider::new("me@example.com", NOW, 3));
    fake.set_labels(vec![
        Label { id: LabelId::new("INBOX"), name: "INBOX".into(), kind: LabelKind::System, color: None, visible: true },
        Label {
            id: LabelId::new("Label_1"),
            name: "Receipts".into(),
            kind: LabelKind::User,
            color: None,
            visible: true,
        },
    ]);
    let recorder = Arc::new(Recorder::default());
    let engine = SyncEngine::new(fake.clone(), db.clone(), recorder.clone());
    (fake, db, recorder, engine)
}

fn seed_mailbox(fake: &FakeProvider) {
    fake.seed(message("inbox-unread", "t1", 1, &["INBOX", "UNREAD"]));
    fake.seed(message("inbox-read", "t2", 2, &["INBOX"]));
    fake.seed(message("recent", "t3", 10, &["Label_1"]));
    fake.seed(message("this-year", "t4", 100, &[]));
    fake.seed(message("ancient", "t5", 900, &[]));
    fake.seed(message("spam", "t6", 1, &["SPAM"]));
}

fn assert_consistent(db: &Db) {
    let problems = db.read_blocking(consistency::check).unwrap();
    assert!(problems.is_empty(), "{problems:?}");
}

#[tokio::test]
async fn bootstrap_queues_by_priority_and_backfill_fills_the_store() {
    let (fake, db, recorder, engine) = setup("bootstrap");
    engine.set_window(SyncWindow::Everything).await.unwrap();
    seed_mailbox(&fake);
    assert!(engine.needs_bootstrap().await.unwrap());

    engine.bootstrap_prepare().await.unwrap();
    // Only inbox phases are queued so far, unread first.
    let queued = db.read(|c| queue::peek(c, 10)).await.unwrap();
    assert_eq!(queued.iter().map(|m| m.as_str()).collect::<Vec<_>>(), vec!["inbox-unread", "inbox-read"]);

    engine.bootstrap_list_rest().await.unwrap();
    assert!(!engine.needs_bootstrap().await.unwrap());
    let queued = db.read(|c| queue::peek(c, 10)).await.unwrap();
    assert_eq!(
        queued.iter().map(|m| m.as_str()).collect::<Vec<_>>(),
        vec!["inbox-unread", "inbox-read", "recent", "this-year", "ancient"],
        "priority order; spam is never listed"
    );

    let fetched = engine.backfill_all().await.unwrap();
    assert_eq!(fetched, 5);
    assert_eq!(db.read(queue::len).await.unwrap(), 0);
    let inbox = db.read(|c| read::list_threads(c, "INBOX", None, 10)).await.unwrap();
    assert_eq!(inbox.rows.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), vec!["t1", "t2"]);
    let receipts = db.read(|c| read::list_threads(c, "Label_1", None, 10)).await.unwrap();
    assert_eq!(receipts.rows.len(), 1);
    assert_eq!(engine.account_email().await.unwrap().as_deref(), Some("me@example.com"));

    // Bodies were sanitized on the way in.
    let body = db.read(|c| read::get_body(c, &MessageId::new("inbox-unread"))).await.unwrap().unwrap();
    let html = body.html_sanitized.unwrap();
    assert!(!html.contains("script"), "{html}");
    assert!(html.contains("openagc-remote:https://t.example/p.gif"));
    assert!(body.has_remote_images);

    // The UI heard about it, and the final progress is idle.
    assert!(recorder.changes.lock().unwrap().iter().any(|c| c.mailboxes.contains_key("INBOX")));
    assert_eq!(recorder.progress.lock().unwrap().last().unwrap().phase, SyncPhase::Idle);
    assert_consistent(&db);
}

#[tokio::test]
async fn backfill_fetches_newest_first_within_a_phase() {
    let (fake, db, _recorder, engine) = setup("newest-first");
    engine.set_window(SyncWindow::Everything).await.unwrap();
    // Seeded oldest first; the provider lists newest first regardless.
    fake.seed(message("old", "t1", 300, &[]));
    fake.seed(message("mid", "t2", 200, &[]));
    fake.seed(message("new", "t3", 100, &[]));
    engine.bootstrap_prepare().await.unwrap();
    engine.bootstrap_list_rest().await.unwrap();
    let queued = db.read(|c| queue::peek(c, 10)).await.unwrap();
    assert_eq!(queued.iter().map(|m| m.as_str()).collect::<Vec<_>>(), vec!["new", "mid", "old"]);
}

#[tokio::test]
async fn the_sync_window_bounds_the_backfill_and_can_be_widened_or_narrowed() {
    let (fake, db, _recorder, engine) = setup("window");
    seed_mailbox(&fake);
    assert_eq!(engine.window().await.unwrap(), SyncWindow::HalfYear, "default");
    engine.bootstrap_prepare().await.unwrap();
    engine.bootstrap_list_rest().await.unwrap();
    let queued = |db: &Db| {
        let q = db.read_blocking(|c| queue::peek(c, 10)).unwrap();
        q.iter().map(|m| m.as_str().to_owned()).collect::<Vec<_>>()
    };
    // 100-day-old mail is inside six months; 900-day-old mail is not.
    assert_eq!(queued(&db), vec!["inbox-unread", "inbox-read", "recent", "this-year"]);

    engine.set_window(SyncWindow::Month).await.unwrap();
    assert_eq!(queued(&db), vec!["inbox-unread", "inbox-read", "recent"], "narrowing drops queued older mail");

    engine.set_window(SyncWindow::Everything).await.unwrap();
    assert_eq!(queued(&db), vec!["inbox-unread", "inbox-read", "recent", "this-year", "ancient"]);
    assert_eq!(engine.window().await.unwrap(), SyncWindow::Everything);

    // An account from before windows existed gets the default applied once.
    db.write(|tx| Ok(tx.execute("DELETE FROM sync_state WHERE key = 'sync_window'", [])?)).await.unwrap();
    engine.ensure_window().await.unwrap();
    assert_eq!(queued(&db), vec!["inbox-unread", "inbox-read", "recent", "this-year"], "trimmed to six months");
    engine.ensure_window().await.unwrap();
    assert_eq!(engine.window().await.unwrap(), SyncWindow::HalfYear);
}

/// A backfill source that answers from the fake provider's data but
/// counts its calls, standing in for a bulk transport.
struct CountingSource(Arc<FakeProvider>, std::sync::atomic::AtomicUsize);

#[async_trait::async_trait]
impl provider_api::BackfillSource for CountingSource {
    async fn fetch(&self, ids: &[MessageId]) -> provider_api::ProviderResult<Vec<FetchedMessage>> {
        self.1.fetch_add(ids.len(), std::sync::atomic::Ordering::SeqCst);
        use provider_api::MailProvider;
        self.0.fetch_messages(ids, provider_api::Priority::Background).await
    }
    async fn fetch_headers(&self, ids: &[MessageId]) -> provider_api::ProviderResult<Option<Vec<FetchedMessage>>> {
        use provider_api::MailProvider;
        let mut all = self.0.fetch_messages(ids, provider_api::Priority::Background).await?;
        for m in &mut all {
            m.body = None;
        }
        Ok(Some(all))
    }
    fn name(&self) -> &'static str {
        "counting"
    }
}

#[tokio::test]
async fn a_headers_pass_fills_the_list_before_bodies_and_leaves_them_queued() {
    let (fake, db, _recorder, engine) = setup("headers");
    engine.set_window(SyncWindow::Everything).await.unwrap();
    seed_mailbox(&fake);
    engine.bootstrap_prepare().await.unwrap();
    engine.bootstrap_list_rest().await.unwrap();
    assert_eq!(engine.headers_pass(100).await.unwrap(), 0, "REST: headers cost as much as bodies, so no pass");

    engine.set_backfill_source(Arc::new(CountingSource(fake.clone(), Default::default())));
    assert_eq!(engine.headers_pass(3).await.unwrap(), 3);
    assert_eq!(engine.headers_pass(100).await.unwrap(), 2);
    assert_eq!(engine.headers_pass(100).await.unwrap(), 0, "every queued message has a row");
    let inbox = db.read(|c| read::list_threads(c, "INBOX", None, 10)).await.unwrap();
    assert_eq!(inbox.rows.len(), 2, "browsable already");
    assert!(db.read(|c| read::get_body(c, &MessageId::new("inbox-unread"))).await.unwrap().is_none(), "no body yet");
    assert_eq!(db.read(queue::len).await.unwrap(), 5, "still queued for bodies");

    // Opening one moves it to the front.
    engine.prioritize(vec![MessageId::new("ancient")]).await.unwrap();
    assert_eq!(db.read(|c| queue::peek(c, 1)).await.unwrap(), vec![MessageId::new("ancient")]);
    assert_eq!(engine.backfill_all().await.unwrap(), 5);
    assert!(db.read(|c| read::get_body(c, &MessageId::new("inbox-unread"))).await.unwrap().is_some());
    assert_consistent(&db);
}

#[tokio::test]
async fn backfill_bodies_come_from_the_configured_source() {
    let (fake, db, _recorder, engine) = setup("source");
    engine.set_window(SyncWindow::Everything).await.unwrap();
    seed_mailbox(&fake);
    assert_eq!(engine.backfill_source_name(), "rest");
    let source = Arc::new(CountingSource(fake.clone(), Default::default()));
    engine.set_backfill_source(source.clone());
    assert_eq!(engine.backfill_source_name(), "counting");
    engine.bootstrap_prepare().await.unwrap();
    engine.bootstrap_list_rest().await.unwrap();
    assert_eq!(engine.backfill_all().await.unwrap(), 5);
    assert_eq!(source.1.load(std::sync::atomic::Ordering::SeqCst), 5, "every body came through the source");
    engine.use_rest_backfill();
    assert_eq!(engine.backfill_source_name(), "rest");
    assert_eq!(db.read(queue::len).await.unwrap(), 0);
}

#[tokio::test]
async fn incremental_sync_applies_new_mail_label_changes_and_deletions() {
    let (fake, db, recorder, engine) = setup("incremental");
    seed_mailbox(&fake);
    engine.bootstrap_prepare().await.unwrap();
    engine.bootstrap_list_rest().await.unwrap();
    engine.backfill_all().await.unwrap();
    recorder.changes.lock().unwrap().clear();

    fake.deliver(message("new", "t7", 0, &["INBOX", "UNREAD"]));
    fake.relabel(&MessageId::new("inbox-unread"), &[], &[LabelId::new("UNREAD"), LabelId::new("INBOX")]);
    fake.delete(&MessageId::new("inbox-read"));

    let report = engine.sync_incremental().await.unwrap();
    assert_eq!((report.added, report.relabeled, report.deleted), (1, 1, 1));
    assert_eq!(report.new_mail.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), vec!["new"]);
    assert_eq!(report.new_mail[0].thread_id.as_str(), "t7");

    let inbox = db.read(|c| read::list_threads(c, "INBOX", None, 10)).await.unwrap();
    assert_eq!(inbox.rows.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), vec!["t7"]);
    let archive = db.read(|c| read::list_threads(c, ARCHIVE_LABEL, None, 10)).await.unwrap();
    assert!(archive.rows.iter().any(|t| t.id.as_str() == "t1"), "archived on the server shows as archived here");
    {
        let changes = recorder.changes.lock().unwrap();
        let inbox_change = &changes.last().unwrap().mailboxes["INBOX"];
        assert!(inbox_change.inserted.contains("t7"));
        assert!(inbox_change.removed.contains("t1") && inbox_change.removed.contains("t2"));
    }

    // Running again with nothing new is a no-op.
    let again = engine.sync_incremental().await.unwrap();
    assert_eq!((again.added, again.relabeled, again.deleted), (0, 0, 0));
    assert!(again.new_mail.is_empty());
    assert_consistent(&db);
}

#[tokio::test]
async fn mail_arriving_during_bootstrap_is_not_lost() {
    let (fake, db, _recorder, engine) = setup("during");
    seed_mailbox(&fake);
    engine.bootstrap_prepare().await.unwrap();
    // Arrives after the cursor was recorded but before listing finished.
    fake.deliver(message("mid-bootstrap", "t8", 0, &["INBOX"]));
    engine.bootstrap_list_rest().await.unwrap();
    engine.backfill_all().await.unwrap();
    let report = engine.sync_incremental().await.unwrap();
    assert!(report.new_mail.is_empty(), "already stored by the bootstrap: not announced again");
    let inbox = db.read(|c| read::list_threads(c, "INBOX", None, 10)).await.unwrap();
    assert!(inbox.rows.iter().any(|t| t.id.as_str() == "t8"));
    assert_consistent(&db);
}

#[tokio::test]
async fn label_changes_for_unfetched_messages_queue_a_fetch() {
    let (fake, db, _recorder, engine) = setup("unfetched");
    engine.set_window(SyncWindow::Everything).await.unwrap();
    seed_mailbox(&fake);
    engine.bootstrap_prepare().await.unwrap(); // inbox queued, nothing fetched
    fake.relabel(&MessageId::new("ancient"), &[LabelId::new("STARRED")], &[]);
    engine.sync_incremental().await.unwrap();
    let queued = db.read(|c| queue::peek(c, 1)).await.unwrap();
    assert_eq!(queued, vec![MessageId::new("ancient")], "urgent: the user just touched it elsewhere");
    engine.backfill_batch(1).await.unwrap();
    let (summary, _) = db.read(|c| read::get_thread(c, &ThreadId::new("t5"))).await.unwrap().unwrap();
    assert!(summary.is_starred, "fetched with current labels");
}

#[tokio::test]
async fn an_expired_cursor_triggers_a_full_resync() {
    let (fake, db, _recorder, engine) = setup("expired");
    engine.set_window(SyncWindow::Everything).await.unwrap();
    seed_mailbox(&fake);
    engine.bootstrap_prepare().await.unwrap();
    engine.bootstrap_list_rest().await.unwrap();
    engine.backfill_all().await.unwrap();

    // A week offline: history is gone and labels changed meanwhile.
    fake.relabel(&MessageId::new("inbox-read"), &[LabelId::new("STARRED")], &[]);
    fake.expire_history();
    let err = engine.sync_incremental().await.unwrap_err();
    assert!(matches!(err, SyncError::ResyncStarted));
    assert_eq!(db.read(queue::len).await.unwrap(), 5, "everything re-queued");
    engine.backfill_all().await.unwrap();
    let (summary, _) = db.read(|c| read::get_thread(c, &ThreadId::new("t2"))).await.unwrap().unwrap();
    assert!(summary.is_starred, "resync picked up the missed change");
    // And incremental works again from the fresh cursor.
    engine.sync_incremental().await.unwrap();
    assert_consistent(&db);
}

#[tokio::test]
async fn incremental_before_bootstrap_is_an_error() {
    let (_fake, _db, _recorder, engine) = setup("early");
    assert!(matches!(engine.sync_incremental().await.unwrap_err(), SyncError::NotBootstrapped));
}

#[tokio::test]
async fn only_unread_inbox_mail_from_others_is_new_mail() {
    let (fake, _db, _recorder, engine) = setup("new-mail");
    seed_mailbox(&fake);
    engine.bootstrap_prepare().await.unwrap();
    engine.bootstrap_list_rest().await.unwrap();
    engine.backfill_all().await.unwrap();
    fake.deliver(message("hello", "t20", 0, &["INBOX", "UNREAD"]));
    fake.deliver(message("already-read", "t21", 0, &["INBOX"]));
    fake.deliver(message("mine", "t22", 0, &["SENT", "INBOX", "UNREAD"]));
    fake.deliver(message("junk", "t23", 0, &["SPAM", "UNREAD"]));
    fake.deliver(message("filtered", "t24", 0, &["UNREAD", "Label_1"]));
    let report = engine.sync_incremental().await.unwrap();
    assert_eq!(report.new_mail.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), vec!["hello"]);
}

#[tokio::test]
async fn label_changes_made_elsewhere_are_reported_and_our_own_are_not() {
    let (fake, _db, _recorder, engine) = setup("external");
    seed_mailbox(&fake);
    engine.bootstrap_prepare().await.unwrap();
    engine.bootstrap_list_rest().await.unwrap();
    engine.backfill_all().await.unwrap();

    // Something else (a cloud routine) files a message and archives it.
    fake.relabel(&MessageId::new("inbox-unread"), &[LabelId::new("Label_7")], &[LabelId::new("INBOX")]);
    // OpenAGC archives another through its outbox.
    engine.apply_change(mail_sync::LocalChange::archive(vec![ThreadId::new("t2")]), true).await.unwrap();
    engine.drain_outbox().await.unwrap();

    let report = engine.sync_incremental().await.unwrap();
    assert_eq!(report.external_label_changes.len(), 1, "{:?}", report.external_label_changes);
    let change = &report.external_label_changes[0];
    assert_eq!(change.message.as_str(), "inbox-unread");
    assert_eq!(change.thread.as_str(), "t1");
    assert_eq!(change.added, vec![LabelId::new("Label_7")]);
    assert_eq!(change.removed, vec![LabelId::new("INBOX")]);
}
