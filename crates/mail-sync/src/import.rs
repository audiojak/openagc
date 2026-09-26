//! Import an mbox into a store (spec §7.8): the archive account's only
//! writer. Blocking; run it on a blocking thread. Streams the file,
//! parses each message, and writes batches through the same `MailWriter`
//! path sync uses, so search, threads and sanitizing behave identically.

use std::collections::{BTreeMap, HashSet};
use std::io::BufRead;
use std::sync::atomic::{AtomicBool, Ordering};

use mail_domain::{Body, EmailAddress, Label, LabelId, LabelKind, MessageId, ThreadId, system_labels};
use mail_mime::mbox::MboxReader;
use mail_store::{Db, IncomingAttachment, IncomingMessage, MailWriter, StoreResult};
use sha2::{Digest, Sha256};

/// Messages per store transaction.
pub const IMPORT_BATCH: usize = 200;
const SNIPPET_CHARS: usize = 160;

#[derive(Debug, Clone, Default)]
pub struct ImportOptions {
    /// The user's addresses: mail from them is `SENT` when the file has no
    /// Gmail labels, and it is never counted as unread.
    pub my_addresses: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImportProgress {
    pub bytes: u64,
    pub total_bytes: u64,
    pub imported: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportStats {
    /// Messages written (new or re-imported).
    pub imported: u64,
    /// Skipped: the same `Message-ID` is already in the store as another
    /// message (a copy in a second file, say).
    pub duplicates: u64,
    /// Skipped: not parseable as a message.
    pub unreadable: u64,
    pub cancelled: bool,
}

/// Import `input` (an mbox of `total_bytes`) into `db`, reporting progress
/// after each batch. Stops at the next batch boundary once `cancel` is set;
/// what was written stays, and importing again resumes because message ids
/// are content hashes.
pub fn import_mbox<R: BufRead>(
    db: &Db,
    input: R,
    total_bytes: u64,
    options: &ImportOptions,
    cancel: &AtomicBool,
    mut progress: impl FnMut(ImportProgress),
) -> StoreResult<ImportStats> {
    let mine: HashSet<String> = options.my_addresses.iter().map(|a| a.trim().to_lowercase()).collect();
    let mut reader = MboxReader::new(input);
    let mut stats = ImportStats::default();
    let mut batch: Vec<IncomingMessage> = Vec::with_capacity(IMPORT_BATCH);
    let mut labels: BTreeMap<String, Label> = BTreeMap::new();
    let mut seen_in_file: HashSet<String> = HashSet::new();

    loop {
        let next = reader.next();
        let at_end = next.is_none();
        match next {
            Some(Ok(message)) => match convert(&message.raw, &mine) {
                Some((incoming, user_labels)) => {
                    let key = incoming.rfc822_message_id.clone().unwrap_or_else(|| incoming.id.0.clone());
                    if seen_in_file.insert(key) {
                        for label in user_labels {
                            labels.entry(label.id.0.clone()).or_insert(label);
                        }
                        batch.push(incoming);
                    } else {
                        stats.duplicates += 1;
                    }
                }
                None => stats.unreadable += 1,
            },
            Some(Err(e)) => return Err(mail_store::StoreError::Invalid(format!("reading the mbox failed: {e}"))),
            None => {}
        }
        if batch.len() >= IMPORT_BATCH || (at_end && !batch.is_empty()) {
            let (written, dupes) = write_batch(db, std::mem::take(&mut batch), std::mem::take(&mut labels))?;
            stats.imported += written;
            stats.duplicates += dupes;
            progress(ImportProgress { bytes: reader.position(), total_bytes, imported: stats.imported });
            if cancel.load(Ordering::Relaxed) && !at_end {
                stats.cancelled = true;
                return Ok(stats);
            }
        }
        if at_end {
            progress(ImportProgress { bytes: reader.position(), total_bytes, imported: stats.imported });
            return Ok(stats);
        }
    }
}

/// Write one batch; messages whose `Message-ID` already belongs to a
/// different stored message are skipped as duplicates.
fn write_batch(db: &Db, batch: Vec<IncomingMessage>, labels: BTreeMap<String, Label>) -> StoreResult<(u64, u64)> {
    db.write_blocking(move |tx| {
        let mut w = MailWriter::new(tx);
        if !labels.is_empty() {
            w.upsert_labels(&labels.into_values().collect::<Vec<_>>())?;
        }
        let mut other = tx.prepare_cached("SELECT 1 FROM messages WHERE rfc822_message_id = ?1 AND gmail_id != ?2")?;
        let (mut written, mut dupes) = (0, 0);
        for m in &batch {
            if let Some(mid) = &m.rfc822_message_id
                && other.exists(rusqlite_params(mid, &m.id.0))?
            {
                dupes += 1;
                continue;
            }
            w.upsert_message(m)?;
            written += 1;
        }
        drop(other);
        w.finish()?;
        Ok((written, dupes))
    })
}

fn rusqlite_params<'a>(a: &'a str, b: &'a str) -> [&'a str; 2] {
    [a, b]
}

/// One raw message → a store message and the user labels it needs.
fn convert(raw: &[u8], mine: &HashSet<String>) -> Option<(IncomingMessage, Vec<Label>)> {
    if raw.iter().all(u8::is_ascii_whitespace) {
        return None;
    }
    let parsed = mail_mime::parse(raw).ok()?;
    let headers = &parsed.headers;
    // Something that is not a message at all parses into an empty shell.
    if headers.from.is_none() && headers.message_id.is_none() && headers.date.is_none() && headers.subject.is_empty() {
        return None;
    }
    let raw_headers = header_block(raw);
    let from_me = headers.from.as_ref().is_some_and(|f| mine.contains(&f.email.to_lowercase()));

    let (mut label_ids, user_labels) = match raw_header(&raw_headers, "x-gmail-labels") {
        Some(value) => gmail_labels(&value),
        None => (vec![LabelId::new(if from_me { system_labels::SENT } else { system_labels::INBOX })], vec![]),
    };
    if from_me {
        label_ids.retain(|l| l.as_str() != system_labels::UNREAD);
    }
    label_ids.sort();
    label_ids.dedup();

    let id = MessageId(hex(&Sha256::digest(raw)[..16]));
    let thread_id = match raw_header(&raw_headers, "x-gm-thrid").and_then(|t| t.trim().parse::<u64>().ok()) {
        Some(thrid) => ThreadId(format!("{thrid:x}")),
        None => {
            // The conversation's root: the first reference, else the
            // parent, else this message. Order-independent, so replies
            // imported before their original still meet it.
            let root = headers
                .references
                .first()
                .or(headers.in_reply_to.as_ref())
                .or(headers.message_id.as_ref())
                .map(|s| s.to_lowercase())
                .unwrap_or_else(|| id.0.clone());
            ThreadId(format!("t{}", hex(&Sha256::digest(root.as_bytes())[..12])))
        }
    };

    let text = parsed.text.clone().or_else(|| parsed.html.as_deref().map(mail_mime::html_to_text));
    let sanitized = parsed.html.as_deref().map(mail_mime::sanitize_html);
    let snippet = snippet(text.as_deref().unwrap_or(""));
    let date = headers.date.unwrap_or(0);
    let attachments = parsed
        .attachments
        .iter()
        .enumerate()
        .map(|(i, a)| IncomingAttachment {
            part_id: Some(format!("{}", i + 1)),
            provider_attachment_id: None,
            filename: a.filename.clone(),
            mime_type: a.mime_type.clone(),
            size: a.size,
            content_id: a.content_id.clone(),
            is_inline: a.is_inline,
            // Nothing to fetch later: an archive has no server.
            data: Some(a.data.clone()),
        })
        .collect();
    let incoming = IncomingMessage {
        id,
        thread_id,
        rfc822_message_id: headers.message_id.clone(),
        in_reply_to: headers.in_reply_to.clone(),
        references: headers.references.clone(),
        from: headers.from.clone(),
        to: headers.to.clone(),
        cc: headers.cc.clone(),
        bcc: headers.bcc.clone(),
        reply_to: headers.reply_to.clone(),
        subject: headers.subject.clone(),
        date,
        internal_date: date,
        snippet,
        label_ids,
        size_estimate: raw.len() as u64,
        body: Some(Body {
            text_plain: text,
            has_remote_images: sanitized.as_ref().is_some_and(|s| s.has_remote_images),
            html_sanitized: sanitized.map(|s| s.html),
        }),
        attachments,
        headers_json: None,
    };
    Some((incoming, user_labels))
}

/// Takeout's `X-Gmail-Labels` (comma-separated, names possibly quoted) →
/// label ids, plus the user labels to create. System names map to Gmail's
/// ids; `Opened` and `Archived` are states, not labels; categories become
/// Gmail's category labels.
fn gmail_labels(value: &str) -> (Vec<LabelId>, Vec<Label>) {
    let mut ids = Vec::new();
    let mut users = Vec::new();
    for name in split_labels(value) {
        let system = match name.as_str() {
            "Inbox" => Some(system_labels::INBOX),
            "Sent" => Some(system_labels::SENT),
            "Starred" => Some(system_labels::STARRED),
            "Important" => Some(system_labels::IMPORTANT),
            "Unread" => Some(system_labels::UNREAD),
            "Drafts" | "Draft" => Some(system_labels::DRAFT),
            "Spam" => Some(system_labels::SPAM),
            "Trash" => Some(system_labels::TRASH),
            "Opened" | "Archived" | "" => continue,
            _ => None,
        };
        if let Some(system) = system {
            ids.push(LabelId::new(system));
            continue;
        }
        if let Some(category) = name.strip_prefix("Category ") {
            ids.push(LabelId(format!("CATEGORY_{}", category.trim().to_uppercase().replace(' ', "_"))));
            continue;
        }
        let id = LabelId(format!("Label_archive_{}", hex(&Sha256::digest(name.as_bytes())[..8])));
        users.push(Label { id: id.clone(), name, kind: LabelKind::User, color: None, visible: true });
        ids.push(id);
    }
    (ids, users)
}

/// Split on commas outside double quotes; unquote.
fn split_labels(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for c in value.chars() {
        match c {
            '"' => quoted = !quoted,
            ',' if !quoted => out.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    out.push(current);
    out.into_iter().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect()
}

/// The header block (up to the first blank line), unfolded.
fn header_block(raw: &[u8]) -> String {
    let end = raw
        .windows(2)
        .position(|w| w == b"\n\n")
        .or_else(|| raw.windows(4).position(|w| w == b"\r\n\r\n"))
        .unwrap_or(raw.len());
    String::from_utf8_lossy(&raw[..end]).replace("\r\n", "\n").replace("\n ", " ").replace("\n\t", " ")
}

fn raw_header(block: &str, name: &str) -> Option<String> {
    block.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim().eq_ignore_ascii_case(name).then(|| value.trim().to_owned())
    })
}

fn snippet(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(SNIPPET_CHARS).collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The address that appears most across From, To and Cc: in a personal
/// mailbox, the owner's. Pre-fills "your addresses" (spec §7.8). Reads at
/// most `limit` messages.
pub fn most_frequent_address<R: BufRead>(input: R, limit: usize) -> Option<EmailAddress> {
    let mut counts: BTreeMap<String, (usize, EmailAddress)> = BTreeMap::new();
    for message in MboxReader::new(input).take(limit).flatten() {
        let block = header_block(&message.raw);
        let fields: Vec<(&str, String)> =
            ["From", "To", "Cc"].into_iter().filter_map(|h| raw_header(&block, h).map(|v| (h, v))).collect();
        let headers = mail_mime::parse_headers(fields.iter().map(|(k, v)| (*k, v.as_str())));
        let mut seen = HashSet::new();
        for address in headers.from.into_iter().chain(headers.to).chain(headers.cc) {
            let key = address.email.to_lowercase();
            if seen.insert(key.clone()) {
                counts.entry(key).or_insert((0, address)).0 += 1;
            }
        }
    }
    counts.into_values().max_by_key(|(n, _)| *n).map(|(_, a)| a)
}
