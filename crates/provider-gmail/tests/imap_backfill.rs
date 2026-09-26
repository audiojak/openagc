//! IMAP backfill against the in-process fake server and the REST fake;
//! nothing connects to Google (spec §7.4 IMAP amendment, Testing).

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use mail_domain::{LabelId, MessageId, ThreadId};
use mail_mime::mbox::fixture::FixtureMessage;
use provider_api::fake::FakeProvider;
use provider_api::token::StaticToken;
use provider_api::{BackfillSource, FetchedMessage};
use provider_gmail::imap::{ImapBackfill, ImapConfig, ImapEndpoint};
use provider_gmail::imap_fake::{FakeImapMessage, FakeImapServer};

const MSG_A: u64 = 0x18a1_0000_0000_0001;
const MSG_B: u64 = 0x18a1_0000_0000_0002;
const MSG_BIG: u64 = 0x18a1_0000_0000_0003;
const THREAD: u64 = 0x18a1_0000_0000_0000;

fn hex(n: u64) -> MessageId {
    MessageId(format!("{n:x}"))
}

async fn setup(token_accepted: &str) -> (FakeImapServer, Arc<FakeProvider>, ImapBackfill) {
    let server = FakeImapServer::start("good-token").await;
    let mut a = FixtureMessage::simple(1);
    a.attachment = Some(("notes.txt".into(), "text/plain".into(), b"attached words".to_vec()));
    server.add(FakeImapMessage {
        uid: 10,
        msgid: MSG_A,
        thrid: THREAD,
        labels: vec!["\\Inbox".into(), "\\Important".into(), "Clients/Acme".into(), "Unknown label".into()],
        flags: vec![],
        raw: a.to_rfc822(),
    });
    server.add(FakeImapMessage {
        uid: 11,
        msgid: MSG_B,
        thrid: THREAD,
        labels: vec!["\\Sent".into()],
        flags: vec!["\\Seen".into(), "\\Flagged".into()],
        raw: FixtureMessage::simple(2).to_rfc822(),
    });
    let mut big = FixtureMessage::simple(3);
    big.attachment = Some(("big.bin".into(), "application/octet-stream".into(), vec![7u8; 3 * 1024 * 1024]));
    server.add(FakeImapMessage {
        uid: 12,
        msgid: MSG_BIG,
        thrid: THREAD,
        labels: vec![],
        flags: vec![],
        raw: big.to_rfc822(),
    });

    // The REST fake holds what IMAP should not serve.
    let rest = Arc::new(FakeProvider::new("me@example.com", 1_790_000_000_000, 50));
    for id in [MSG_BIG, 0x99] {
        rest.seed(FetchedMessage {
            id: hex(id),
            thread_id: ThreadId(format!("{THREAD:x}")),
            subject: "from REST".into(),
            ..Default::default()
        });
    }
    let labels = Arc::new(RwLock::new(HashMap::from([("Clients/Acme".to_owned(), LabelId::new("Label_7"))])));
    let config = ImapConfig {
        email: "me@example.com".into(),
        endpoint: ImapEndpoint::Plain(server.addr),
        max_message_bytes: 2 * 1024 * 1024,
        daily_budget_bytes: 1 << 30,
        batch: 200,
    };
    let source = ImapBackfill::new(config, Arc::new(StaticToken(token_accepted.into())), rest.clone(), labels);
    (server, rest, source)
}

#[tokio::test]
async fn bodies_come_over_imap_with_api_ids_labels_and_flags() {
    let (server, _rest, source) = setup("good-token").await;
    let mut fetched = source.fetch(&[hex(MSG_A), hex(MSG_B)]).await.unwrap();
    fetched.sort_by(|a, b| a.id.0.cmp(&b.id.0));
    assert_eq!(fetched.len(), 2);
    let a = &fetched[0];
    assert_eq!(a.id, hex(MSG_A));
    assert_eq!(a.thread_id, ThreadId(format!("{THREAD:x}")), "X-GM-THRID in hex, as the API writes it");
    let labels: Vec<&str> = a.label_ids.iter().map(|l| l.as_str()).collect();
    assert_eq!(labels, ["IMPORTANT", "INBOX", "Label_7", "UNREAD"], "unknown names are skipped, unseen is unread");
    assert_eq!(a.subject, "Subject 1");
    let body = a.body.as_ref().unwrap();
    assert!(body.text.as_deref().unwrap().contains("Body of message 1"));
    assert_eq!(body.attachments.len(), 1);
    assert_eq!(body.attachments[0].data.as_deref(), Some(&b"attached words"[..]), "bytes come with the message");
    assert_eq!(a.internal_date, 1_756_720_800_000, "INTERNALDATE, as the API's internalDate");
    let b = &fetched[1];
    let labels: Vec<&str> = b.label_ids.iter().map(|l| l.as_str()).collect();
    assert_eq!(labels, ["SENT", "STARRED"], "seen and flagged");
    assert_eq!(server.body_fetches(), 2);
    assert_eq!(source.name(), "imap");
    assert!(source.bytes_today().await > 0);

    // The session is kept: a second batch does not log in again.
    source.fetch(&[hex(MSG_A)]).await.unwrap();
    assert_eq!(server.logins(), 1);
}

#[tokio::test]
async fn big_and_unknown_messages_go_over_the_api() {
    let (server, rest, source) = setup("good-token").await;
    let fetched = source.fetch(&[hex(MSG_A), hex(MSG_BIG), hex(0x99)]).await.unwrap();
    let mut subjects: Vec<&str> = fetched.iter().map(|m| m.subject.as_str()).collect();
    subjects.sort();
    assert_eq!(subjects, ["Subject 1", "from REST", "from REST"]);
    assert_eq!(server.body_fetches(), 1, "only the small, known message over IMAP");
    assert_eq!(rest.fetch_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_refused_login_means_the_api_from_then_on() {
    let (server, rest, source) = setup("wrong-token").await;
    let fetched = source.fetch(&[hex(MSG_BIG)]).await.unwrap();
    assert_eq!(fetched.len(), 1);
    assert!(source.is_refused());
    assert_eq!(source.name(), "imap-refused");
    source.fetch(&[hex(MSG_BIG)]).await.unwrap();
    assert_eq!(server.logins(), 0);
    assert_eq!(rest.fetch_calls.load(std::sync::atomic::Ordering::SeqCst), 2, "no second login attempt");
}

#[tokio::test]
async fn the_daily_budget_hands_over_to_the_api() {
    let (server, _rest, source) = {
        let (server, rest, _) = setup("good-token").await;
        let labels = Arc::new(RwLock::new(HashMap::new()));
        let config = ImapConfig {
            email: "me@example.com".into(),
            endpoint: ImapEndpoint::Plain(server.addr),
            max_message_bytes: 2 * 1024 * 1024,
            daily_budget_bytes: 10, // spent by the first message
            batch: 200,
        };
        let source = ImapBackfill::new(config, Arc::new(StaticToken("good-token".into())), rest.clone(), labels);
        (server, rest, source)
    };
    source.fetch(&[hex(MSG_A)]).await.unwrap();
    assert_eq!(server.body_fetches(), 1);
    let again = source.fetch(&[hex(MSG_BIG)]).await.unwrap();
    assert_eq!(again[0].subject, "from REST");
    assert_eq!(server.body_fetches(), 1, "over budget: no more IMAP today");
}
