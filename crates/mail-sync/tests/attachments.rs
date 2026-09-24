//! Attachment bytes on demand against the fake provider.

use std::sync::Arc;

use mail_domain::{EmailAddress, LabelId, MessageId, ThreadId};
use mail_store::{Db, ThreadChanges};
use mail_sync::{SyncEngine, SyncObserver, attachment_file};
use provider_api::fake::FakeProvider;
use provider_api::{FetchedAttachment, FetchedBody, FetchedMessage};

struct Quiet;
impl SyncObserver for Quiet {
    fn threads_changed(&self, _: &ThreadChanges) {}
}

#[tokio::test]
async fn attachments_download_once_and_inline_bytes_need_no_request() {
    let dir = std::env::temp_dir().join(format!("openagc-attachments-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir.join("mail.sqlite")).unwrap();
    let fake = Arc::new(FakeProvider::new("me@example.com", 1_790_000_000_000, 50));
    fake.seed(FetchedMessage {
        id: MessageId::new("m1"),
        thread_id: ThreadId::new("t1"),
        label_ids: vec![LabelId::new("INBOX")],
        internal_date: 1_790_000_000_000,
        from: Some(EmailAddress::new(None, "a@example.com")),
        body: Some(FetchedBody {
            text: Some("see attached".into()),
            html: None,
            attachments: vec![
                FetchedAttachment {
                    part_id: Some("1".into()),
                    attachment_id: Some("ANGj_deck".into()),
                    filename: "../deck.pdf".into(),
                    mime_type: "application/pdf".into(),
                    size: 10,
                    ..Default::default()
                },
                FetchedAttachment {
                    part_id: Some("2".into()),
                    filename: "logo.png".into(),
                    mime_type: "image/png".into(),
                    size: 4,
                    content_id: Some("logo@x".into()),
                    is_inline: true,
                    data: Some(b"\x89PNG".to_vec()),
                    ..Default::default()
                },
            ],
        }),
        ..Default::default()
    });
    let engine = SyncEngine::new(fake.clone(), db.clone(), Arc::new(Quiet));
    engine.bootstrap_prepare().await.unwrap();
    engine.bootstrap_list_rest().await.unwrap();
    engine.backfill_all().await.unwrap();

    let ids: Vec<i64> = db
        .read(|c| {
            Ok(c.prepare("SELECT id FROM attachments ORDER BY part_id")?
                .query_map([], |r| r.get(0))?
                .collect::<Result<_, _>>()?)
        })
        .await
        .unwrap();
    let cache = dir.join("Attachments");

    let deck = attachment_file(&db, Some(fake.as_ref()), &cache, ids[0]).await.unwrap();
    assert!(deck.downloaded);
    assert!(deck.path.starts_with(&cache), "the ../ in the name cannot escape: {:?}", deck.path);
    assert_eq!(deck.filename, "_deck.pdf");
    assert_eq!(std::fs::read(&deck.path).unwrap(), b"fake bytes of ../deck.pdf");
    let again = attachment_file(&db, None, &cache, ids[0]).await.unwrap();
    assert!(!again.downloaded, "cached: no provider needed");

    let logo = attachment_file(&db, None, &cache, ids[1]).await.unwrap();
    assert_eq!(std::fs::read(&logo.path).unwrap(), b"\x89PNG", "inline bytes came with the message");
    assert_eq!(logo.content_id.as_deref(), Some("logo@x"));

    let fresh = dir.join("Other");
    assert!(attachment_file(&db, None, &fresh, ids[0]).await.is_err(), "offline and not cached");
}
