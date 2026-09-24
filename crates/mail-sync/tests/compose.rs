//! Reply, forward and send through the outbox against the fake provider.

use std::sync::Arc;

use mail_domain::{EmailAddress, LabelId, MessageId, ThreadId};
use mail_store::drafts::{self, DraftState};
use mail_store::{Db, ThreadChanges, consistency, read};
use mail_sync::{SyncEngine, SyncObserver, forward_draft, reply_draft, send_draft};
use provider_api::fake::FakeProvider;
use provider_api::{FetchedBody, FetchedMessage, ProviderError};

const NOW: i64 = 1_790_000_000_000;

struct Quiet;
impl SyncObserver for Quiet {
    fn threads_changed(&self, _: &ThreadChanges) {}
}

fn me() -> EmailAddress {
    EmailAddress::new(Some("Me"), "me@example.com")
}

async fn setup(name: &str) -> (Arc<FakeProvider>, Db, SyncEngine) {
    let dir = std::env::temp_dir().join(format!("openagc-compose-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir.join("mail.sqlite")).unwrap();
    let fake = Arc::new(FakeProvider::new("me@example.com", NOW, 50));
    fake.seed(FetchedMessage {
        id: MessageId::new("m1"),
        thread_id: ThreadId::new("t1"),
        label_ids: vec![LabelId::new("INBOX")],
        internal_date: NOW,
        message_id_header: Some("plan.1@example.org".into()),
        references: vec!["root@example.org".into()],
        from: Some(EmailAddress::new(Some("Alex Rivera"), "alex@example.org")),
        to: vec![me(), EmailAddress::new(None, "sam@example.org")],
        subject: "Q3 planning".into(),
        body: Some(FetchedBody {
            text: Some("Can we meet Thursday?".into()),
            html: Some("<p>Can we meet <b>Thursday</b>?</p>".into()),
            attachments: vec![],
        }),
        ..Default::default()
    });
    let engine = SyncEngine::new(fake.clone(), db.clone(), Arc::new(Quiet));
    engine.bootstrap_prepare().await.unwrap();
    engine.bootstrap_list_rest().await.unwrap();
    engine.backfill_all().await.unwrap();
    (fake, db, engine)
}

async fn save(db: &Db, d: drafts::DraftRecord) -> i64 {
    db.write(move |tx| drafts::save(tx, &d, NOW)).await.unwrap()
}

async fn sent(db: &Db) -> Vec<String> {
    let page = db.read(|c| read::list_threads(c, "SENT", None, 10)).await.unwrap();
    page.rows.into_iter().map(|t| t.id.0).collect()
}

#[tokio::test]
async fn a_reply_draft_is_prefilled_and_threaded() {
    let (_fake, db, _engine) = setup("reply").await;
    let me = vec!["me@example.com".to_owned()];
    let d = reply_draft(&db, &MessageId::new("m1"), false, &me).await.unwrap();
    assert_eq!(d.subject, "Re: Q3 planning");
    assert_eq!(d.to, vec![EmailAddress::new(Some("Alex Rivera"), "alex@example.org")]);
    assert!(d.cc.is_empty());
    assert_eq!(d.thread_id.as_deref(), Some("t1"));
    assert!(d.body_html.is_empty(), "the editable body starts empty");
    assert!(
        d.quoted_html.contains("<blockquote><p>Can we meet <b>Thursday</b>?</p></blockquote>"),
        "{}",
        d.quoted_html
    );
    let all = reply_draft(&db, &MessageId::new("m1"), true, &me).await.unwrap();
    assert_eq!(all.cc, vec![EmailAddress::new(None, "sam@example.org")]);
}

#[tokio::test]
async fn sending_shows_an_optimistic_copy_then_the_real_one_replaces_it() {
    let (fake, db, engine) = setup("send").await;
    let mut d = reply_draft(&db, &MessageId::new("m1"), false, &["me@example.com".to_owned()]).await.unwrap();
    d.body_html = format!("<p>Thursday works.</p>{}", d.quoted_html);
    let id = save(&db, d).await;

    send_draft(&db, id, me(), true).await.unwrap();
    assert_eq!(sent(&db).await, vec!["t1"], "the reply shows in Sent at once, in the same thread");
    assert!(db.read(drafts::list).await.unwrap().is_empty(), "sending drafts are hidden");
    let (_, messages) = db.read(|c| read::get_thread(c, &ThreadId::new("t1"))).await.unwrap().unwrap();
    assert!(messages.iter().any(|m| m.id.0.starts_with("local-")));

    engine.drain_outbox().await.unwrap();
    assert!(db.read(move |c| drafts::get(c, id)).await.unwrap().is_none(), "draft deleted once Gmail accepted it");
    let raw = fake.message(&MessageId::new("sent1")).unwrap();
    assert_eq!(raw.thread_id, ThreadId::new("t1"), "sent into the thread");
    assert_eq!(raw.in_reply_to.as_deref(), Some("plan.1@example.org"));
    assert_eq!(raw.subject, "Re: Q3 planning");

    engine.sync_incremental().await.unwrap();
    let (_, messages) = db.read(|c| read::get_thread(c, &ThreadId::new("t1"))).await.unwrap().unwrap();
    assert!(!messages.iter().any(|m| m.id.0.starts_with("local-")), "placeholder replaced");
    assert!(messages.iter().any(|m| m.id.as_str() == "sent1"));
    assert_eq!(messages.len(), 2);
    assert!(db.read(consistency::check).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_failed_send_brings_the_draft_back_with_the_error() {
    let (fake, db, engine) = setup("fail").await;
    let id = save(
        &db,
        drafts::DraftRecord {
            to: vec![EmailAddress::new(None, "sam@example.org")],
            subject: "Hello".into(),
            body_html: "<p>Hi</p>".into(),
            ..Default::default()
        },
    )
    .await;
    send_draft(&db, id, me(), true).await.unwrap();
    assert_eq!(sent(&db).await.len(), 1);
    fake.fail_next_writes(vec![ProviderError::Forbidden("daily sending limit".into())]);
    engine.drain_outbox().await.unwrap();
    assert!(sent(&db).await.is_empty(), "optimistic copy removed");
    let d = db.read(move |c| drafts::get(c, id)).await.unwrap().unwrap();
    assert_eq!(d.state, DraftState::Failed);
    assert!(d.last_error.unwrap().contains("daily sending limit"));
    assert_eq!(db.read(drafts::list).await.unwrap().len(), 1, "visible again for the user to fix");
    assert!(db.read(consistency::check).await.unwrap().is_empty());
}

#[tokio::test]
async fn forwarding_and_local_only_sending() {
    let (_fake, db, _engine) = setup("forward").await;
    let mut f = forward_draft(&db, &MessageId::new("m1")).await.unwrap();
    assert_eq!(f.subject, "Fwd: Q3 planning");
    assert!(f.quoted_html.contains("Forwarded message"));
    assert!(f.quoted_html.contains("Alex Rivera &lt;alex@example.org&gt;"));
    f.body_html = f.quoted_html.clone();
    assert!(f.to.is_empty());
    f.to = vec![EmailAddress::new(None, "jo@example.net")];
    let id = save(&db, f).await;
    // No provider (demo): sent locally, draft gone, copy kept.
    send_draft(&db, id, me(), false).await.unwrap();
    assert!(db.read(move |c| drafts::get(c, id)).await.unwrap().is_none());
    assert_eq!(sent(&db).await, vec!["t1"]);
}

#[tokio::test]
async fn sending_without_recipients_is_refused_and_changes_nothing() {
    let (_fake, db, _engine) = setup("norecipients").await;
    let id = save(&db, drafts::DraftRecord { subject: "x".into(), body_html: "<p>x</p>".into(), ..Default::default() })
        .await;
    assert!(send_draft(&db, id, me(), true).await.is_err());
    assert!(sent(&db).await.is_empty());
    assert_eq!(db.read(move |c| drafts::get(c, id)).await.unwrap().unwrap().state, DraftState::Editing);
}
