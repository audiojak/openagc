//! Store behavior and consistency tests. Every scenario ends with the
//! invariant checker, so any drift in the denormalized data fails loudly.

use std::path::PathBuf;

use mail_domain::{Body, EmailAddress, Label, LabelId, LabelKind, MailboxKind, MessageId, ThreadId};
use mail_store::{
    ARCHIVE_LABEL, Db, IncomingAttachment, IncomingMessage, MailWriter, ThreadChanges, consistency, read,
};

fn open(name: &str) -> Db {
    let dir: PathBuf = std::env::temp_dir().join(format!("openagc-mail-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    Db::open(&dir.join("mail.sqlite")).unwrap()
}

fn addr(s: &str) -> EmailAddress {
    EmailAddress::new(None, s)
}

fn msg(id: &str, thread: &str, at: i64, labels: &[&str]) -> IncomingMessage {
    IncomingMessage {
        id: MessageId::new(id),
        thread_id: ThreadId::new(thread),
        from: Some(EmailAddress::new(Some(&format!("Sender {thread}")), &format!("{thread}@example.com"))),
        to: vec![addr("me@example.com")],
        subject: format!("Subject {thread}"),
        snippet: format!("snippet {id}"),
        date: at,
        internal_date: at,
        label_ids: labels.iter().map(|l| LabelId::new(*l)).collect(),
        ..Default::default()
    }
}

fn write(db: &Db, f: impl FnOnce(&mut MailWriter<'_>) + Send + 'static) -> ThreadChanges {
    db.write_blocking(move |tx| {
        let mut w = MailWriter::new(tx);
        f(&mut w);
        w.finish()
    })
    .unwrap()
}

fn assert_consistent(db: &Db) {
    let problems = db.read_blocking(consistency::check).unwrap();
    assert!(problems.is_empty(), "inconsistent store:\n{}", problems.join("\n"));
}

fn inbox_ids(db: &Db) -> Vec<String> {
    db.read_blocking(|c| read::list_threads(c, "INBOX", None, 100)).unwrap().rows.into_iter().map(|t| t.id.0).collect()
}

fn mailbox(db: &Db, kind: MailboxKind) -> (u32, u32) {
    let m = db.read_blocking(read::list_mailboxes).unwrap().into_iter().find(|m| m.kind == kind).unwrap();
    (m.total_count, m.unread_count)
}

#[test]
fn inbox_lists_threads_newest_first_with_aggregates() {
    let db = open("inbox");
    let changes = write(&db, |w| {
        w.upsert_message(&msg("m1", "a", 1_000, &["INBOX", "UNREAD"])).unwrap();
        w.upsert_message(&msg("m2", "b", 2_000, &["INBOX"])).unwrap();
        w.upsert_message(&msg("m3", "a", 3_000, &["INBOX"])).unwrap();
    });
    assert_eq!(inbox_ids(&db), vec!["a", "b"]);
    let inbox = &changes.mailboxes["INBOX"];
    assert_eq!(inbox.inserted.iter().collect::<Vec<_>>(), vec!["a", "b"]);

    let (summary, messages) = db.read_blocking(|c| read::get_thread(c, &ThreadId::new("a"))).unwrap().unwrap();
    assert_eq!(summary.message_count, 2);
    assert_eq!(summary.unread_count, 1);
    assert_eq!(summary.last_message_at, 3_000);
    assert_eq!(summary.snippet, "snippet m3", "snippet is from the latest message");
    assert_eq!(summary.participants.len(), 1, "same sender counted once");
    assert_eq!(messages.iter().map(|m| m.id.0.as_str()).collect::<Vec<_>>(), vec!["m1", "m3"]);
    assert_eq!(messages[0].to, vec![addr("me@example.com")]);
    assert_eq!(mailbox(&db, MailboxKind::Inbox), (2, 1));
    assert_consistent(&db);
}

#[test]
fn archiving_moves_a_thread_from_inbox_to_archive() {
    let db = open("archive");
    write(&db, |w| {
        w.upsert_message(&msg("m1", "a", 1_000, &["INBOX", "UNREAD"])).unwrap();
        w.upsert_message(&msg("m2", "b", 2_000, &["INBOX"])).unwrap();
    });
    let changes = write(&db, |w| {
        w.modify_message_labels(&MessageId::new("m1"), &[], &[LabelId::new("INBOX")]).unwrap();
    });
    assert_eq!(inbox_ids(&db), vec!["b"]);
    assert!(changes.mailboxes["INBOX"].removed.contains("a"));
    assert!(changes.mailboxes[ARCHIVE_LABEL].inserted.contains("a"));
    let archived = db.read_blocking(|c| read::list_threads(c, ARCHIVE_LABEL, None, 10)).unwrap();
    assert_eq!(archived.rows.iter().map(|t| t.id.0.as_str()).collect::<Vec<_>>(), vec!["a"]);
    assert_eq!(mailbox(&db, MailboxKind::Inbox), (1, 0));
    assert_eq!(mailbox(&db, MailboxKind::Archive), (1, 1));
    assert_consistent(&db);
}

#[test]
fn marking_read_updates_counts_and_reports_an_update() {
    let db = open("read");
    write(&db, |w| w.upsert_message(&msg("m1", "a", 1_000, &["INBOX", "UNREAD"])).unwrap());
    let changes = write(&db, |w| {
        w.modify_message_labels(&MessageId::new("m1"), &[], &[LabelId::new("UNREAD")]).unwrap();
    });
    assert!(changes.mailboxes["INBOX"].updated.contains("a"));
    assert_eq!(mailbox(&db, MailboxKind::Inbox), (1, 0));
    assert_consistent(&db);
}

#[test]
fn deleting_the_last_message_removes_the_thread() {
    let db = open("delete");
    write(&db, |w| {
        w.upsert_message(&msg("m1", "a", 1_000, &["INBOX", "UNREAD"])).unwrap();
        w.upsert_message(&msg("m2", "a", 2_000, &["INBOX"])).unwrap();
    });
    write(&db, |w| assert!(w.delete_message(&MessageId::new("m2")).unwrap()));
    let (summary, _) = db.read_blocking(|c| read::get_thread(c, &ThreadId::new("a"))).unwrap().unwrap();
    assert_eq!(summary.message_count, 1);
    let changes = write(&db, |w| assert!(w.delete_message(&MessageId::new("m1")).unwrap()));
    assert!(changes.mailboxes["INBOX"].removed.contains("a"));
    assert!(db.read_blocking(|c| read::get_thread(c, &ThreadId::new("a"))).unwrap().is_none());
    assert_eq!(mailbox(&db, MailboxKind::Inbox), (0, 0));
    assert_consistent(&db);
}

#[test]
fn a_metadata_update_keeps_an_existing_body_and_attachments() {
    let db = open("body");
    let mut full = msg("m1", "a", 1_000, &["INBOX"]);
    full.body = Some(Body {
        text_plain: Some("hello body".into()),
        html_sanitized: Some("<p>hello</p>".into()),
        has_remote_images: false,
    });
    full.attachments = vec![IncomingAttachment {
        filename: "report.pdf".into(),
        mime_type: "application/pdf".into(),
        size: 10,
        ..Default::default()
    }];
    write(&db, move |w| w.upsert_message(&full).unwrap());
    write(&db, |w| w.upsert_message(&msg("m1", "a", 1_000, &["INBOX", "STARRED"])).unwrap());
    let body = db.read_blocking(|c| read::get_body(c, &MessageId::new("m1"))).unwrap().unwrap();
    assert_eq!(body.text_plain.as_deref(), Some("hello body"));
    let (summary, messages) = db.read_blocking(|c| read::get_thread(c, &ThreadId::new("a"))).unwrap().unwrap();
    assert!(summary.is_starred);
    assert!(summary.has_attachments);
    assert_eq!(messages[0].attachments[0].filename, "report.pdf");
    assert_eq!(messages[0].body_state, mail_domain::BodyState::Full);
    assert_consistent(&db);
}

#[test]
fn full_text_search_indexes_subject_sender_body_and_attachment_names() {
    let db = open("fts");
    let mut m = msg("m1", "invoices", 1_000, &["INBOX"]);
    m.subject = "Quarterly invoice".into();
    m.body = Some(Body { text_plain: Some("Please find the café receipt attached".into()), ..Default::default() });
    m.attachments = vec![IncomingAttachment { filename: "aws-bill.pdf".into(), ..Default::default() }];
    write(&db, move |w| w.upsert_message(&m).unwrap());
    let hits = |q: &'static str| -> i64 {
        db.read_blocking(move |c| {
            Ok(c.query_row("SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH ?1", [q], |r| r.get(0))?)
        })
        .unwrap()
    };
    assert_eq!(hits("quarterly"), 1);
    assert_eq!(hits("cafe"), 1, "diacritics are folded");
    assert_eq!(hits("from_text:invoices"), 1, "sender address");
    assert_eq!(hits("attachment_names:aws"), 1);
    assert_eq!(hits("nonexistent"), 0);
    write(&db, |w| {
        w.delete_message(&MessageId::new("m1")).unwrap();
    });
    assert_eq!(hits("quarterly"), 0);
    assert_consistent(&db);
}

#[test]
fn keyset_paging_visits_every_thread_once_in_order_even_with_ties() {
    let db = open("paging");
    write(&db, |w| {
        for i in 0..250 {
            // Every fifth pair shares a timestamp to exercise the tiebreak.
            let at = 1_000_000 + (i / 2) * 10;
            w.upsert_message(&msg(&format!("m{i}"), &format!("t{i:03}"), at, &["INBOX"])).unwrap();
        }
    });
    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        let c = cursor.clone();
        let page = db.read_blocking(move |conn| read::list_threads(conn, "INBOX", c.as_deref(), 100)).unwrap();
        pages += 1;
        seen.extend(page.rows.iter().map(|t| (t.last_message_at, t.id.0.clone())));
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(pages, 3);
    assert_eq!(seen.len(), 250);
    let unique: std::collections::BTreeSet<_> = seen.iter().map(|(_, id)| id.clone()).collect();
    assert_eq!(unique.len(), 250, "no duplicates across pages");
    assert!(seen.windows(2).all(|w| w[0].0 >= w[1].0), "newest first");
    assert_consistent(&db);
}

#[test]
fn contacts_count_each_message_once_and_autocomplete_by_substring() {
    let db = open("contacts");
    let mut sent = msg("m1", "a", 1_000, &["SENT"]);
    sent.to = vec![EmailAddress::new(Some("Johnny Appleseed"), "johnny@apple.example")];
    let resent = sent.clone();
    write(&db, move |w| w.upsert_message(&sent).unwrap());
    write(&db, move |w| w.upsert_message(&resent).unwrap());
    let (count, hits): (i64, i64) = db
        .read_blocking(|c| {
            let count =
                c.query_row("SELECT sent_count FROM contacts WHERE email = 'johnny@apple.example'", [], |r| r.get(0))?;
            let hits =
                c.query_row("SELECT COUNT(*) FROM contacts_fts WHERE contacts_fts MATCH 'hnny'", [], |r| r.get(0))?;
            Ok((count, hits))
        })
        .unwrap();
    assert_eq!(count, 1, "re-upserting a message does not recount it");
    assert_eq!(hits, 1);
}

#[test]
fn unknown_labels_get_hidden_placeholders_and_label_refresh_prunes() {
    let db = open("labels");
    write(&db, |w| {
        w.upsert_labels(&[Label {
            id: LabelId::new("Label_1"),
            name: "Receipts".into(),
            kind: LabelKind::User,
            color: None,
            visible: true,
        }])
        .unwrap();
        w.upsert_message(&msg("m1", "a", 1_000, &["INBOX", "Label_1", "Label_9"])).unwrap();
    });
    let labels = db.read_blocking(read::list_labels).unwrap();
    let placeholder = labels.iter().find(|l| l.id.as_str() == "Label_9").unwrap();
    assert!(!placeholder.visible, "unlisted label is hidden until labels.list names it");
    let user_mailboxes: Vec<String> = db
        .read_blocking(read::list_mailboxes)
        .unwrap()
        .into_iter()
        .filter(|m| m.kind == MailboxKind::Label)
        .map(|m| m.name)
        .collect();
    assert_eq!(user_mailboxes, vec!["Receipts"]);

    let changes = write(&db, |w| w.retain_labels(&[LabelId::new("INBOX"), LabelId::new("Label_9")]).unwrap());
    assert!(changes.mailboxes.contains_key("INBOX"), "affected threads recomputed");
    let (summary, _) = db.read_blocking(|c| read::get_thread(c, &ThreadId::new("a"))).unwrap().unwrap();
    assert_eq!(summary.label_ids, vec![LabelId::new("INBOX"), LabelId::new("Label_9")]);
    assert_consistent(&db);
}

#[test]
fn spam_and_trash_threads_are_not_archived() {
    let db = open("spam");
    write(&db, |w| {
        w.upsert_message(&msg("m1", "spam", 1_000, &["SPAM"])).unwrap();
        w.upsert_message(&msg("m2", "sent", 2_000, &["SENT"])).unwrap();
    });
    let archived = db.read_blocking(|c| read::list_threads(c, ARCHIVE_LABEL, None, 10)).unwrap();
    assert_eq!(archived.rows.iter().map(|t| t.id.0.as_str()).collect::<Vec<_>>(), vec!["sent"]);
    assert_consistent(&db);
}

#[test]
fn applying_the_virtual_archive_label_is_rejected() {
    let db = open("virtual");
    let result = db.write_blocking(|tx| {
        let mut w = MailWriter::new(tx);
        w.upsert_message(&msg("m1", "a", 1_000, &[ARCHIVE_LABEL]))?;
        w.finish()
    });
    assert!(result.is_err());
}

/// Tiny deterministic PRNG so the randomized test needs no dependency.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self, n: u64) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) % n
    }
}

#[test]
fn random_mutation_sequences_stay_consistent() {
    let db = open("random");
    let labels = ["INBOX", "UNREAD", "STARRED", "SPAM", "TRASH", "SENT", "Label_1", "Label_2"];
    let mut rng = Lcg(42);
    for round in 0..40 {
        let ops: Vec<(u64, u64, u64, u64)> =
            (0..25).map(|_| (rng.next(4), rng.next(60), rng.next(12), rng.next(256))).collect();
        write(&db, move |w| {
            for (op, m, t, mask) in ops {
                let id = MessageId::new(format!("m{m}"));
                let chosen: Vec<LabelId> = labels
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| mask & (1 << i) != 0)
                    .map(|(_, l)| LabelId::new(*l))
                    .collect();
                match op {
                    0 | 1 => {
                        let mut incoming = msg(&id.0, &format!("t{t}"), (m * 1000 + t) as i64, &[]);
                        incoming.label_ids = chosen;
                        if op == 1 {
                            incoming.body = Some(Body { text_plain: Some(format!("body {m}")), ..Default::default() });
                        }
                        w.upsert_message(&incoming).unwrap();
                    }
                    2 => {
                        let (add, remove) = chosen.split_at(chosen.len() / 2);
                        w.modify_message_labels(&id, add, remove).unwrap();
                    }
                    _ => {
                        w.delete_message(&id).unwrap();
                    }
                }
            }
        });
        let problems = db.read_blocking(consistency::check).unwrap();
        assert!(problems.is_empty(), "round {round}:\n{}", problems.join("\n"));
    }
}

#[test]
fn the_checker_detects_each_kind_of_drift() {
    let db = open("corrupt");
    write(&db, |w| {
        w.upsert_message(&msg("m1", "a", 1_000, &["INBOX", "UNREAD"])).unwrap();
        w.upsert_message(&msg("m2", "b", 2_000, &["INBOX"])).unwrap();
    });
    assert_consistent(&db);
    let corruptions = [
        ("UPDATE threads SET unread_count = 5 WHERE gmail_id = 'a'", "unread_count"),
        ("UPDATE label_stats SET thread_count = 9", "label_stats"),
        (
            "DELETE FROM thread_labels WHERE thread_id = (SELECT id FROM threads WHERE gmail_id = 'b')",
            "thread_labels missing",
        ),
        ("UPDATE thread_labels SET last_message_at = 1", "sort key"),
        ("DELETE FROM messages_fts WHERE rowid = (SELECT id FROM messages WHERE gmail_id = 'm1')", "messages_fts"),
    ];
    for (sql, expected) in corruptions {
        let db = open(&format!("corrupt-{}", expected.replace(' ', "-")));
        write(&db, |w| {
            w.upsert_message(&msg("m1", "a", 1_000, &["INBOX", "UNREAD"])).unwrap();
            w.upsert_message(&msg("m2", "b", 2_000, &["INBOX"])).unwrap();
        });
        db.write_blocking(move |tx| Ok(tx.execute_batch(sql)?)).unwrap();
        let problems = db.read_blocking(consistency::check).unwrap();
        assert!(problems.iter().any(|p| p.contains(expected)), "{sql} not detected; got {problems:?}");
    }
}
