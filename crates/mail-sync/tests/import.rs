//! mbox import against synthetic fixtures only (spec §7.8 Testing).

use std::sync::atomic::AtomicBool;

use mail_mime::mbox::fixture::{FixtureMessage, build};
use mail_store::{Db, consistency, read};
use mail_sync::import::{ImportOptions, ImportStats, import_mbox, most_frequent_address};

fn store(name: &str) -> Db {
    let dir = std::env::temp_dir().join(format!("openagc-import-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    Db::open(&dir.join("mail.sqlite")).unwrap()
}

fn run(db: &Db, mbox: &[u8], mine: &[&str]) -> ImportStats {
    let options = ImportOptions { my_addresses: mine.iter().map(|s| (*s).to_owned()).collect() };
    import_mbox(db, mbox, mbox.len() as u64, &options, &AtomicBool::new(false), |_| {}).unwrap()
}

fn assert_consistent(db: &Db) {
    let problems = db.read_blocking(consistency::check).unwrap();
    assert!(problems.is_empty(), "{problems:?}");
}

fn takeout(n: usize, labels: &str, thrid: &str) -> FixtureMessage {
    FixtureMessage { gmail_labels: Some(labels.into()), gm_thrid: Some(thrid.into()), ..FixtureMessage::simple(n) }
}

#[test]
fn takeout_labels_and_threads_come_across() {
    let db = store("takeout");
    let mbox = build(&[
        takeout(1, "Inbox,Unread,Important,Category Updates", "1001"),
        takeout(2, "Opened,Archived,\"Clients/Acme, Inc\",Clients/Globex", "1001"),
        takeout(3, "Sent,Opened", "2002"),
        takeout(4, "Starred,Inbox,Opened", "3003"),
    ]);
    let stats = run(&db, &mbox, &[]);
    assert_eq!(stats, ImportStats { imported: 4, ..Default::default() });

    let inbox = db.read_blocking(|c| read::list_threads(c, "INBOX", None, 50)).unwrap();
    assert_eq!(inbox.rows.len(), 2, "threads 1001 and 3003");
    let t1001 = inbox.rows.iter().find(|t| t.id.as_str() == format!("{:x}", 1001)).expect("thread id from X-GM-THRID");
    assert_eq!(t1001.message_count, 2);
    assert!(t1001.unread_count > 0);
    assert_eq!(db.read_blocking(|c| read::list_threads(c, "SENT", None, 50)).unwrap().rows.len(), 1);
    assert_eq!(db.read_blocking(|c| read::list_threads(c, "STARRED", None, 50)).unwrap().rows.len(), 1);
    let labels = db.read_blocking(read::list_labels).unwrap();
    let names: Vec<&str> = labels.iter().map(|l| l.name.as_str()).collect();
    assert!(names.contains(&"Clients/Acme, Inc"), "quoted names keep their comma: {names:?}");
    assert!(names.contains(&"Clients/Globex"));
    assert!(labels.iter().any(|l| l.id.as_str() == "CATEGORY_UPDATES"));
    assert!(!names.contains(&"Opened") && !names.contains(&"Archived"), "states, not labels");
    assert_consistent(&db);
}

#[test]
fn without_gmail_labels_mail_from_me_is_sent_and_the_rest_is_inbox_and_replies_thread() {
    let db = store("plain");
    let mut original = FixtureMessage::simple(1);
    original.from = "Me Myself <me@example.com>".into();
    let mut reply = FixtureMessage::simple(2);
    reply.in_reply_to = Some(original.message_id.clone());
    // The reply comes first in the file: threading must not depend on order.
    let stats = run(&db, &build(&[reply, original, FixtureMessage::simple(3)]), &["ME@example.com"]);
    assert_eq!(stats.imported, 3);
    let sent = db.read_blocking(|c| read::list_threads(c, "SENT", None, 50)).unwrap();
    assert_eq!(sent.rows.len(), 1);
    assert_eq!(sent.rows[0].message_count, 2, "the reply joined its original's thread");
    let inbox = db.read_blocking(|c| read::list_threads(c, "INBOX", None, 50)).unwrap();
    assert_eq!(inbox.rows.len(), 2);
    assert_consistent(&db);
}

#[test]
fn importing_again_changes_nothing_and_duplicates_are_skipped() {
    let db = store("again");
    let mbox = build(&[FixtureMessage::simple(1), FixtureMessage::simple(2)]);
    run(&db, &mbox, &[]);
    let second = run(&db, &mbox, &[]);
    assert_eq!(second.imported, 2, "same content hash: rewritten in place");
    let inbox = db.read_blocking(|c| read::list_threads(c, "INBOX", None, 50)).unwrap();
    assert_eq!(inbox.rows.len(), 2);

    // The same Message-ID with different bytes (another export): skipped.
    let mut copy = FixtureMessage::simple(1);
    copy.body = "Same message, different export.\n".into();
    let third = run(&db, &build(&[copy.clone(), copy]), &[]);
    assert_eq!(third.imported, 0);
    assert_eq!(third.duplicates, 2, "one against the store, one within the file");
    assert_consistent(&db);
}

#[test]
fn unreadable_entries_are_counted_and_skipped() {
    let db = store("corrupt");
    let mut mbox = build(&[FixtureMessage::simple(1)]);
    mbox.extend_from_slice(b"From garbage@example.com Mon Sep  1 10:00:00 2025\n\x00\x01\x02 not a message\n\n");
    mbox.extend_from_slice(&build(&[FixtureMessage::simple(2)]));
    let stats = run(&db, &mbox, &[]);
    assert_eq!(stats.imported, 2);
    assert_eq!(stats.unreadable, 1);
}

#[test]
fn a_large_attachment_is_stored_with_the_message() {
    let db = store("attachment");
    let mut m = FixtureMessage::simple(1);
    let bytes: Vec<u8> = (0..20 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    m.attachment = Some(("big.bin".into(), "application/octet-stream".into(), bytes.clone()));
    let stats = run(&db, &build(&[m]), &[]);
    assert_eq!(stats.imported, 1);
    let thread = db.read_blocking(|c| read::list_threads(c, "INBOX", None, 1)).unwrap().rows.remove(0);
    assert!(thread.has_attachments);
    let stored: Vec<u8> = db
        .read_blocking(|c| {
            Ok(c.query_row("SELECT data FROM attachments WHERE filename = 'big.bin'", [], |r| r.get::<_, Vec<u8>>(0))?)
        })
        .unwrap();
    assert_eq!(stored.len(), bytes.len());
    assert_eq!(stored, bytes);
}

#[test]
fn cancelling_keeps_what_was_written_and_a_rerun_completes() {
    let db = store("cancel");
    let messages: Vec<FixtureMessage> = (1..=450).map(FixtureMessage::simple).collect();
    let mbox = build(&messages);
    let cancel = AtomicBool::new(false);
    let mut reports = Vec::new();
    let stats = import_mbox(&db, &mbox[..], mbox.len() as u64, &ImportOptions::default(), &cancel, |p| {
        reports.push(p);
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    })
    .unwrap();
    assert!(stats.cancelled);
    assert_eq!(stats.imported, 200, "stopped after the first batch");
    assert_eq!(reports.len(), 1);
    assert!(reports[0].bytes > 0 && reports[0].bytes < mbox.len() as u64);

    let rest = run(&db, &mbox, &[]);
    assert!(!rest.cancelled);
    assert_eq!(rest.imported, 450);
    let count: i64 = db.read_blocking(|c| Ok(c.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))?)).unwrap();
    assert_eq!(count, 450);
    assert_consistent(&db);
}

#[test]
fn the_owner_is_the_most_frequent_address() {
    let mut messages: Vec<FixtureMessage> = (1..=5).map(FixtureMessage::simple).collect();
    messages[0].from = "Me <me@example.com>".into();
    messages[0].to = "friend@example.com".into();
    let owner = most_frequent_address(&build(&messages)[..], 100).unwrap();
    assert_eq!(owner.email, "me@example.com");
}
