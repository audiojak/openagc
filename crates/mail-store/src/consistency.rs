//! Invariant checks over the denormalized data. Used by tests after every
//! mutation sequence, and available for diagnostics.

use rusqlite::Connection;

use crate::error::StoreResult;
use crate::write::ARCHIVE_LABEL;

/// Every way the denormalized data disagrees with the base tables. Empty
/// means consistent.
pub fn check(conn: &Connection) -> StoreResult<Vec<String>> {
    let mut problems = Vec::new();
    let mut check = |sql: &str, what: &str| -> StoreResult<()> {
        let mut stmt = conn.prepare(sql)?;
        let rows: Vec<String> = stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<_, _>>()?;
        for r in rows {
            problems.push(format!("{what}: {r}"));
        }
        Ok(())
    };

    // Every thread has messages; aggregates match.
    check(
        "SELECT t.gmail_id FROM threads t WHERE NOT EXISTS (SELECT 1 FROM messages m WHERE m.thread_id = t.id)",
        "empty thread",
    )?;
    check(
        "SELECT t.gmail_id || ' stored ' || t.unread_count || ' actual ' || x.n
         FROM threads t JOIN (SELECT thread_id, SUM(NOT is_read) AS n FROM messages GROUP BY thread_id) x
           ON x.thread_id = t.id WHERE t.unread_count != x.n",
        "unread_count",
    )?;
    check(
        "SELECT t.gmail_id FROM threads t JOIN (SELECT thread_id, MAX(is_starred) AS s FROM messages GROUP BY thread_id) x
           ON x.thread_id = t.id WHERE t.is_starred != x.s",
        "is_starred",
    )?;

    // thread_labels = labels on any message of the thread (+ @archive rule).
    check(
        "SELECT t.gmail_id || ' missing ' || l.gmail_id
         FROM (SELECT DISTINCT m.thread_id, ml.label_id FROM message_labels ml JOIN messages m ON m.id = ml.message_id) x
         JOIN threads t ON t.id = x.thread_id JOIN labels l ON l.id = x.label_id
         WHERE NOT EXISTS (SELECT 1 FROM thread_labels tl WHERE tl.thread_id = x.thread_id AND tl.label_id = x.label_id)",
        "thread_labels missing",
    )?;
    check(
        &format!(
            "SELECT t.gmail_id || ' extra ' || l.gmail_id FROM thread_labels tl
             JOIN threads t ON t.id = tl.thread_id JOIN labels l ON l.id = tl.label_id
             WHERE l.gmail_id != '{ARCHIVE_LABEL}' AND NOT EXISTS (
               SELECT 1 FROM message_labels ml JOIN messages m ON m.id = ml.message_id
               WHERE m.thread_id = tl.thread_id AND ml.label_id = tl.label_id)"
        ),
        "thread_labels extra",
    )?;
    check(
        "SELECT t.gmail_id FROM thread_labels tl JOIN threads t ON t.id = tl.thread_id
         WHERE tl.last_message_at != t.last_message_at",
        "thread_labels sort key",
    )?;
    check(
        &format!(
            "SELECT t.gmail_id FROM threads t WHERE
               (EXISTS (SELECT 1 FROM thread_labels tl JOIN labels l ON l.id = tl.label_id
                        WHERE tl.thread_id = t.id AND l.gmail_id = '{ARCHIVE_LABEL}'))
               != (NOT EXISTS (SELECT 1 FROM message_labels ml JOIN messages m ON m.id = ml.message_id
                               JOIN labels l ON l.id = ml.label_id WHERE m.thread_id = t.id AND l.gmail_id = 'INBOX')
                   AND EXISTS (SELECT 1 FROM messages m WHERE m.thread_id = t.id AND NOT EXISTS (
                     SELECT 1 FROM message_labels ml JOIN labels l ON l.id = ml.label_id
                     WHERE ml.message_id = m.id AND l.gmail_id IN ('SPAM', 'TRASH'))))"
        ),
        "archive membership",
    )?;

    // label_stats match thread_labels.
    check(
        "SELECT l.gmail_id || ' stored ' || COALESCE(s.thread_count, 0) || '/' || COALESCE(s.unread_thread_count, 0)
                || ' actual ' || COALESCE(x.n, 0) || '/' || COALESCE(x.u, 0)
         FROM labels l LEFT JOIN label_stats s ON s.label_id = l.id
         LEFT JOIN (SELECT tl.label_id, COUNT(*) AS n, SUM(t.unread_count > 0) AS u
                    FROM thread_labels tl JOIN threads t ON t.id = tl.thread_id GROUP BY tl.label_id) x
           ON x.label_id = l.id
         WHERE COALESCE(s.thread_count, 0) != COALESCE(x.n, 0)
            OR COALESCE(s.unread_thread_count, 0) != COALESCE(x.u, 0)",
        "label_stats",
    )?;

    // Search index covers exactly the messages.
    let (indexed, messages): (i64, i64) =
        conn.query_row("SELECT (SELECT COUNT(*) FROM messages_fts), (SELECT COUNT(*) FROM messages)", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
    if indexed != messages {
        problems.push(format!("messages_fts has {indexed} rows for {messages} messages"));
    }
    Ok(problems)
}
