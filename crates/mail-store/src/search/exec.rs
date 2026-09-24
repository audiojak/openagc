//! Compile a [`SearchExpr`] to SQL and run it (spec §6.3, §8).
//!
//! Each text-bearing term becomes a lookup in `messages_fts`; structured
//! terms become column checks or `EXISTS` subqueries on the message. A
//! thread matches if any of its messages match. User text never reaches the
//! FTS query syntax raw: values are split into word tokens and re-quoted.

use rusqlite::types::Value;
use rusqlite::{Connection, params_from_iter};

use mail_domain::Millis;

use super::parse::{Flag, Mailbox, SearchExpr};
use crate::error::{StoreError, StoreResult};
use crate::read::ThreadPage;

const DAY: Millis = 86_400_000;
pub const MAX_RESULTS: u32 = 500;

struct Sql {
    params: Vec<Value>,
}

impl Sql {
    fn bind(&mut self, v: impl Into<Value>) -> String {
        self.params.push(v.into());
        format!("?{}", self.params.len())
    }

    fn fts(&mut self, query: String) -> String {
        let p = self.bind(query);
        format!("m.id IN (SELECT rowid FROM messages_fts WHERE messages_fts MATCH {p})")
    }

    fn has_label(&mut self, gmail_id: &str) -> String {
        let p = self.bind(gmail_id.to_owned());
        format!(
            "EXISTS (SELECT 1 FROM message_labels ml JOIN labels l ON l.id = ml.label_id \
             WHERE ml.message_id = m.id AND l.gmail_id = {p})"
        )
    }

    fn compile(&mut self, e: &SearchExpr, now: Millis) -> String {
        match e {
            SearchExpr::And { all } if all.is_empty() => "1".into(),
            SearchExpr::And { all } => join(all.iter().map(|x| self.compile(x, now)), " AND "),
            SearchExpr::Or { any } => join(any.iter().map(|x| self.compile(x, now)), " OR "),
            SearchExpr::Not { expr } => format!("NOT ({})", self.compile(expr, now)),
            SearchExpr::Text { value } => match fts_phrase(value, true) {
                Some(q) => self.fts(q),
                None => "1".into(),
            },
            SearchExpr::Phrase { value } => match fts_phrase(value, false) {
                Some(q) => self.fts(q),
                None => "1".into(),
            },
            SearchExpr::Subject { value } => self.column("subject", value),
            SearchExpr::From { value } => self.column("from_text", value),
            SearchExpr::To { value } => self.column("to_text", value),
            SearchExpr::Filename { value } => self.column("attachment_names", value),
            SearchExpr::Cc { value } => self.participant("cc", value),
            SearchExpr::Bcc { value } => self.participant("bcc", value),
            SearchExpr::Label { value } => {
                let p = self.bind(value.clone());
                format!(
                    "EXISTS (SELECT 1 FROM message_labels ml JOIN labels l ON l.id = ml.label_id \
                     WHERE ml.message_id = m.id AND (l.gmail_id = {p} OR l.name = {p} COLLATE NOCASE \
                     OR replace(l.name, ' ', '-') = {p} COLLATE NOCASE))"
                )
            }
            SearchExpr::In { mailbox } => match mailbox {
                Mailbox::Inbox => self.has_label("INBOX"),
                Mailbox::Sent => self.has_label("SENT"),
                Mailbox::Drafts => self.has_label("DRAFT"),
                Mailbox::Trash => self.has_label("TRASH"),
                Mailbox::Spam => self.has_label("SPAM"),
                Mailbox::Starred => "m.is_starred = 1".into(),
                Mailbox::Anywhere => "1".into(),
                Mailbox::Archive => {
                    let inbox = self.has_label("INBOX");
                    let spam = self.has_label("SPAM");
                    let trash = self.has_label("TRASH");
                    format!("NOT ({inbox}) AND NOT ({spam}) AND NOT ({trash})")
                }
            },
            SearchExpr::Is { flag } => match flag {
                Flag::Unread => "m.is_read = 0".into(),
                Flag::Read => "m.is_read = 1".into(),
                Flag::Starred => "m.is_starred = 1".into(),
                Flag::Important => self.has_label("IMPORTANT"),
            },
            SearchExpr::HasAttachment => "m.has_attachments = 1".into(),
            SearchExpr::After { at } => format!("m.internal_date >= {}", self.bind(*at)),
            SearchExpr::Before { at } => format!("m.internal_date < {}", self.bind(*at)),
            SearchExpr::NewerThan { days } => {
                format!("m.internal_date >= {}", self.bind(now - Millis::from(*days) * DAY))
            }
            SearchExpr::OlderThan { days } => {
                format!("m.internal_date < {}", self.bind(now - Millis::from(*days) * DAY))
            }
            SearchExpr::Larger { bytes } => format!("m.size_estimate > {}", self.bind(*bytes as i64)),
            SearchExpr::Smaller { bytes } => format!("m.size_estimate < {}", self.bind(*bytes as i64)),
        }
    }

    fn column(&mut self, column: &str, value: &str) -> String {
        match fts_phrase(value, true) {
            Some(q) => self.fts(format!("{column} : {q}")),
            None => "1".into(),
        }
    }

    fn participant(&mut self, role: &str, value: &str) -> String {
        let pattern = format!("%{}%", value.replace(['%', '_'], ""));
        let p = self.bind(pattern);
        format!(
            "EXISTS (SELECT 1 FROM participants p WHERE p.message_id = m.id AND p.role = '{role}' \
             AND (p.email LIKE {p} OR p.name LIKE {p}))"
        )
    }
}

fn join(parts: impl Iterator<Item = String>, sep: &str) -> String {
    let parts: Vec<String> = parts.map(|p| format!("({p})")).collect();
    parts.join(sep)
}

/// Turn user text into a safe FTS5 phrase: word tokens only, re-quoted.
/// `prefix` marks the last token as a prefix (as-you-type matching).
fn fts_phrase(value: &str, prefix: bool) -> Option<String> {
    let tokens: Vec<&str> = value.split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()).collect();
    if tokens.is_empty() {
        return None;
    }
    let phrase = format!("\"{}\"", tokens.join(" "));
    Some(if prefix { format!("{phrase}*") } else { phrase })
}

/// Threads with any message matching `expr`, newest first, keyset-paged by
/// the thread's last message time.
pub fn search(
    conn: &Connection,
    expr: &SearchExpr,
    now: Millis,
    cursor: Option<&str>,
    limit: u32,
) -> StoreResult<ThreadPage> {
    let limit = limit.clamp(1, MAX_RESULTS);
    let mut sql = Sql { params: Vec::new() };
    let mut predicate = sql.compile(expr, now);
    if !expr.mentions_spam_or_trash() {
        let spam = sql.has_label("SPAM");
        let trash = sql.has_label("TRASH");
        predicate = format!("({predicate}) AND NOT ({spam}) AND NOT ({trash})");
    }
    let (after_at, after_id) = match cursor {
        Some(c) => crate::read::decode_cursor(c)?,
        None => (i64::MAX, i64::MAX),
    };
    let at = sql.bind(after_at);
    let id = sql.bind(after_id);
    let lim = sql.bind(i64::from(limit) + 1);
    let query = format!(
        "SELECT t.id, t.gmail_id, t.subject, t.snippet, t.last_message_at, t.message_count, t.unread_count,
                t.has_attachments, t.is_starred, t.participants_json, t.label_ids_json
         FROM threads t
         WHERE t.id IN (SELECT m.thread_id FROM messages m WHERE {predicate})
           AND (t.last_message_at, t.id) < ({at}, {id})
         ORDER BY t.last_message_at DESC, t.id DESC
         LIMIT {lim}"
    );
    let mut stmt = conn.prepare(&query).map_err(|e| StoreError::Invalid(format!("search: {e}")))?;
    let rows = stmt
        .query_map(params_from_iter(sql.params.iter()), |r| {
            let key = (r.get::<_, i64>(4)?, r.get::<_, i64>(0)?);
            Ok((key, crate::read::thread_summary_row(r)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let has_more = rows.len() > limit as usize;
    let mut out = Vec::with_capacity(limit as usize);
    let mut last = None;
    for (key, row) in rows.into_iter().take(limit as usize) {
        last = Some(key);
        out.push(row?);
    }
    let next_cursor = if has_more { last.map(|(a, i)| crate::read::encode_cursor(a, i)) } else { None };
    Ok(ThreadPage { rows: out, next_cursor })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fts_phrases_are_sanitized() {
        assert_eq!(fts_phrase("alice", true).as_deref(), Some("\"alice\"*"));
        assert_eq!(fts_phrase("alex.rivera@example.com", true).as_deref(), Some("\"alex rivera example com\"*"));
        assert_eq!(fts_phrase(r#"x" OR 1=1 NEAR(y) -z*"#, false).as_deref(), Some("\"x OR 1 1 NEAR y z\""));
        assert_eq!(fts_phrase("--", true), None);
    }
}
