//! Deterministic synthetic mailboxes for tests, the performance fixture
//! (spec §13 rule 10) and UI development. All content is invented: names
//! are generic and every address is under `example.com`/`.org`/`.net`.

use mail_domain::{Body, EmailAddress, Label, LabelColor, LabelId, LabelKind, MessageId, ThreadId};

use crate::db::Db;
use crate::error::StoreResult;
use crate::write::{IncomingAttachment, IncomingMessage, MailWriter};

#[derive(Debug, Clone)]
pub struct DemoSpec {
    pub threads: u32,
    pub seed: u64,
    /// Timestamp of the newest message, ms since epoch.
    pub newest_at: i64,
    /// Spread of message dates back from `newest_at`.
    pub span_days: u32,
    /// Messages written per transaction.
    pub batch: usize,
}

impl Default for DemoSpec {
    fn default() -> Self {
        Self { threads: 200, seed: 7, newest_at: 1_790_000_000_000, span_days: 365, batch: 500 }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DemoStats {
    pub threads: u32,
    pub messages: u32,
}

pub const ME: &str = "me@example.com";

const FIRST: &[&str] = &[
    "Alex", "Sam", "Jordan", "Taylor", "Morgan", "Casey", "Riley", "Jamie", "Avery", "Quinn", "Drew", "Rowan",
    "Parker", "Reese", "Skyler", "Emerson", "Hayden", "Kendall", "Logan", "Peyton",
];
const LAST: &[&str] = &[
    "Rivera",
    "Chen",
    "Okafor",
    "Novak",
    "Silva",
    "Kim",
    "Haddad",
    "Lindqvist",
    "Moreau",
    "Tanaka",
    "Park",
    "Ibrahim",
    "Kowalski",
    "Mendes",
    "Nguyen",
    "Schmidt",
];
const ORGS: &[&str] = &["example.com", "example.org", "example.net"];
const SERVICES: &[(&str, &str)] = &[
    ("Billing", "billing@example.net"),
    ("Status Alerts", "alerts@example.net"),
    ("Weekly Digest", "digest@example.org"),
    ("Events", "events@example.org"),
    ("Careers", "careers@example.com"),
];
const TOPICS: &[&str] = &[
    "Q3 planning",
    "contract review",
    "launch checklist",
    "offsite agenda",
    "hiring loop",
    "design feedback",
    "invoice #4821",
    "budget update",
    "customer escalation",
    "partnership intro",
    "roadmap draft",
    "security review",
    "onboarding plan",
    "board deck",
    "pricing proposal",
    "travel itinerary",
];
const SENTENCES: &[&str] = &[
    "Following up on our conversation from last week.",
    "I've attached the latest version for your review.",
    "Can we find thirty minutes on Thursday to go through this?",
    "The numbers look better than we expected.",
    "Let me know if anything here is unclear.",
    "We need a decision by the end of the month.",
    "Thanks again for the quick turnaround.",
    "I'll send an updated draft once the team has weighed in.",
    "Adding a couple of people who should be in the loop.",
    "Happy to jump on a call if that's easier.",
];
const USER_LABELS: &[(&str, &str, &str)] = &[
    ("Label_1", "Receipts", "#fb4c2f"),
    ("Label_2", "Travel", "#16a766"),
    ("Label_3", "Hiring", "#a479e2"),
    ("Label_4", "Customers", "#4a86e8"),
    ("Label_5", "Newsletters", "#ffad47"),
];
/// Nested labels (Gmail's `/` paths), so the sidebar tree has something to
/// show: children of a label, and a prefix with no label of its own. Kept
/// apart from `USER_LABELS` so the generator's random choices, and with them
/// every existing demo count, stay the same.
const NESTED_LABELS: &[(&str, &str, &str)] = &[
    ("Label_6", "Customers/Acme", "#4a86e8"),
    ("Label_7", "Customers/Globex", "#4a86e8"),
    ("Label_8", "Projects/Launch", "#16a766"),
    ("Label_9", "Projects/Launch/Press", "#16a766"),
];

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 11
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }
}

/// Fill `db` with a synthetic mailbox. Blocking: call from a plain thread
/// or `spawn_blocking`, not from inside an async task.
pub fn generate(db: &Db, spec: &DemoSpec) -> StoreResult<DemoStats> {
    let labels: Vec<Label> = USER_LABELS
        .iter()
        .chain(NESTED_LABELS)
        .map(|(id, name, color)| Label {
            id: LabelId::new(*id),
            name: (*name).to_owned(),
            kind: LabelKind::User,
            color: Some(LabelColor { background: (*color).to_owned(), text: "#ffffff".to_owned() }),
            visible: true,
        })
        .collect();
    db.write_blocking(move |tx| {
        let mut w = MailWriter::new(tx);
        w.upsert_labels(&labels)?;
        w.finish()?;
        Ok(())
    })?;

    let mut rng = Rng(spec.seed.wrapping_add(1));
    let span_ms = i64::from(spec.span_days.max(1)) * 86_400_000;
    let mut pending: Vec<IncomingMessage> = Vec::with_capacity(spec.batch);
    let mut stats = DemoStats::default();

    for t in 0..spec.threads {
        // Newer threads first in id order, spread across the span.
        let thread_at = spec.newest_at - (i64::from(t) * span_ms / i64::from(spec.threads.max(1)));
        let thread_id = ThreadId::new(format!("demo{:08x}", t));
        let automated = rng.chance(35);
        let (sender, topic) = if automated {
            let (name, email) = *rng.pick(SERVICES);
            (EmailAddress::new(Some(name), email), (*rng.pick(TOPICS)).to_owned())
        } else {
            let first = *rng.pick(FIRST);
            let last = *rng.pick(LAST);
            let email = format!("{}.{}@{}", first.to_lowercase(), last.to_lowercase(), rng.pick(ORGS));
            (EmailAddress::new(Some(&format!("{first} {last}")), &email), (*rng.pick(TOPICS)).to_owned())
        };
        let in_inbox = rng.chance(if t < spec.threads / 10 { 80 } else { 25 });
        let unread = in_inbox && rng.chance(40);
        let starred = rng.chance(4);
        let user_label = rng.chance(30).then(|| rng.pick(USER_LABELS).0);
        // Every seventh thread also gets a nested label, without touching rng.
        let nested_label = (t % 7 == 3).then(|| NESTED_LABELS[(t / 7) as usize % NESTED_LABELS.len()].0);
        let message_count = if automated { 1 } else { 1 + rng.below(5) as u32 };

        for i in 0..message_count {
            let from_me = !automated && i % 2 == 1;
            let at = thread_at - i64::from(message_count - 1 - i) * (3_600_000 + rng.below(20) as i64 * 600_000);
            let mut labels: Vec<LabelId> = Vec::new();
            if from_me {
                labels.push(LabelId::new("SENT"));
            }
            if in_inbox && !from_me {
                labels.push(LabelId::new("INBOX"));
            }
            if unread && i + 1 == message_count && !from_me {
                labels.push(LabelId::new("UNREAD"));
            }
            if starred && i == 0 {
                labels.push(LabelId::new("STARRED"));
            }
            if let Some(l) = user_label {
                labels.push(LabelId::new(l));
            }
            if let Some(l) = nested_label {
                labels.push(LabelId::new(l));
            }
            let text = (0..2 + rng.below(4)).map(|_| *rng.pick(SENTENCES)).collect::<Vec<_>>().join(" ");
            let body_text = format!("Hi,\n\n{text}\n\nBest,\n{}", if from_me { "Me" } else { sender.display() });
            let body_html =
                format!("<p>Hi,</p><p>{}</p><p>Best,<br>{}</p>", text, if from_me { "Me" } else { sender.display() });
            let attachments = if rng.chance(12) {
                let pdf = one_page_pdf(&capitalize(&topic));
                vec![IncomingAttachment {
                    part_id: Some("2".into()),
                    provider_attachment_id: None,
                    filename: format!("{}.pdf", topic.replace(' ', "-").replace('#', "")),
                    mime_type: "application/pdf".into(),
                    size: pdf.len() as u64,
                    content_id: None,
                    is_inline: false,
                    // The demo has no server: its attachments come with it.
                    data: Some(pdf),
                }]
            } else {
                vec![]
            };
            let me = EmailAddress::new(Some("Me"), ME);
            let subject = if i == 0 { capitalize(&topic) } else { format!("Re: {}", capitalize(&topic)) };
            pending.push(IncomingMessage {
                id: MessageId::new(format!("demo{:08x}m{i}", t)),
                thread_id: thread_id.clone(),
                rfc822_message_id: Some(format!("<demo.{t}.{i}@example.com>")),
                from: Some(if from_me { me.clone() } else { sender.clone() }),
                to: vec![if from_me { sender.clone() } else { me.clone() }],
                subject,
                date: at,
                internal_date: at,
                snippet: text.chars().take(120).collect(),
                label_ids: labels,
                size_estimate: body_text.len() as u64,
                body: Some(Body {
                    text_plain: Some(body_text),
                    html_sanitized: Some(body_html),
                    has_remote_images: false,
                }),
                attachments,
                ..Default::default()
            });
            stats.messages += 1;
        }
        stats.threads += 1;
        if pending.len() >= spec.batch {
            flush(db, &mut pending)?;
        }
    }
    flush(db, &mut pending)?;
    Ok(stats)
}

fn flush(db: &Db, pending: &mut Vec<IncomingMessage>) -> StoreResult<()> {
    if pending.is_empty() {
        return Ok(());
    }
    let batch = std::mem::take(pending);
    db.write_blocking(move |tx| {
        let mut w = MailWriter::new(tx);
        for m in &batch {
            w.upsert_message(m)?;
        }
        w.finish()?;
        Ok(())
    })
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// A minimal valid one-page PDF showing `title`, so demo attachments open
/// in Quick Look.
fn one_page_pdf(title: &str) -> Vec<u8> {
    let text: String = title.chars().filter(|c| c.is_ascii_alphanumeric() || *c == ' ').collect();
    let stream = format!("BT /F1 28 Tf 72 700 Td ({text}) Tj ET");
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R \
         /Resources << /Font << /F1 5 0 R >> >> >>"
            .to_owned(),
        format!("<< /Length {} >>\nstream\n{stream}\nendstream", stream.len()),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_owned(),
    ];
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::with_capacity(objects.len());
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes());
    for offset in offsets {
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objects.len() + 1).as_bytes(),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{consistency, read};

    #[test]
    fn generates_a_consistent_deterministic_mailbox() {
        let dir = std::env::temp_dir().join(format!("openagc-demo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let db = Db::open(&dir.join("mail.sqlite")).unwrap();
        let stats = generate(&db, &DemoSpec { threads: 300, ..Default::default() }).unwrap();
        assert_eq!(stats.threads, 300);
        assert!(stats.messages > 300);
        assert!(db.read_blocking(consistency::check).unwrap().is_empty());
        let inbox = db.read_blocking(|c| read::list_threads(c, "INBOX", None, 500)).unwrap();
        assert!(!inbox.rows.is_empty());
        assert!(inbox.rows.windows(2).all(|w| w[0].last_message_at >= w[1].last_message_at));
        assert!(inbox.rows.iter().all(|t| t.participants.iter().all(|p| p.email.contains("example."))));

        let dir2 = dir.join("again");
        let db2 = Db::open(&dir2.join("mail.sqlite")).unwrap();
        generate(&db2, &DemoSpec { threads: 300, ..Default::default() }).unwrap();
        let inbox2 = db2.read_blocking(|c| read::list_threads(c, "INBOX", None, 500)).unwrap();
        assert_eq!(inbox, inbox2, "same seed, same mailbox");
    }
}
