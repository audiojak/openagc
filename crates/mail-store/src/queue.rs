//! The backfill queue (spec §7.4): message ids waiting for a full fetch,
//! drained in priority order (0 = most urgent).

use mail_domain::MessageId;
use rusqlite::{Connection, Transaction, params};

use crate::error::StoreResult;

/// Queue ids at `priority`, keeping the more urgent priority if an id is
/// already queued. Ids whose message is already fully stored are skipped
/// unless `refetch` is set.
pub fn enqueue(tx: &Transaction<'_>, priority: u8, ids: &[MessageId], refetch: bool) -> StoreResult<usize> {
    let mut exists = tx.prepare_cached("SELECT 1 FROM messages WHERE gmail_id = ?1 AND body_state = 'full'")?;
    let mut current = tx.prepare_cached("SELECT priority FROM backfill_queue WHERE gmail_id = ?1")?;
    let mut insert = tx.prepare_cached("INSERT INTO backfill_queue (priority, gmail_id) VALUES (?1, ?2)")?;
    let mut raise = tx.prepare_cached("UPDATE backfill_queue SET priority = ?1 WHERE gmail_id = ?2")?;
    let mut queued = 0;
    for id in ids {
        if !refetch && exists.exists([id.as_str()])? {
            continue;
        }
        let existing: Option<u8> = current.query_row([id.as_str()], |r| r.get(0)).ok();
        match existing {
            None => {
                insert.execute(params![priority, id.as_str()])?;
                queued += 1;
            }
            Some(p) if priority < p => {
                raise.execute(params![priority, id.as_str()])?;
            }
            Some(_) => {}
        }
    }
    Ok(queued)
}

/// The next `limit` ids, most urgent first. Does not remove them.
/// Queue ids at the front of the most urgent priority: mail the user just
/// touched elsewhere, or that just arrived, is fetched before any backlog.
/// Ids already fetched in full are skipped; queued ones move to the front.
pub fn enqueue_urgent(tx: &Transaction<'_>, ids: &[MessageId]) -> StoreResult<usize> {
    let mut exists = tx.prepare_cached("SELECT 1 FROM messages WHERE gmail_id = ?1 AND body_state = 'full'")?;
    let mut remove = tx.prepare_cached("DELETE FROM backfill_queue WHERE gmail_id = ?1")?;
    let mut insert = tx.prepare_cached(
        "INSERT INTO backfill_queue (seq, priority, gmail_id)
         VALUES ((SELECT COALESCE(MIN(seq), 0) - 1 FROM backfill_queue), 0, ?1)",
    )?;
    let mut queued = 0;
    // Inserted last-to-first so the caller's order is kept at the front.
    for id in ids.iter().rev() {
        if exists.exists([id.as_str()])? {
            continue;
        }
        remove.execute([id.as_str()])?;
        insert.execute([id.as_str()])?;
        queued += 1;
    }
    Ok(queued)
}

pub fn peek(conn: &Connection, limit: usize) -> StoreResult<Vec<MessageId>> {
    let mut stmt = conn.prepare_cached("SELECT gmail_id FROM backfill_queue ORDER BY priority, seq LIMIT ?1")?;
    let ids = stmt.query_map([limit as i64], |r| Ok(MessageId(r.get(0)?)))?.collect::<Result<_, _>>()?;
    Ok(ids)
}

pub fn remove(tx: &Transaction<'_>, ids: &[MessageId]) -> StoreResult<()> {
    let mut stmt = tx.prepare_cached("DELETE FROM backfill_queue WHERE gmail_id = ?1")?;
    for id in ids {
        stmt.execute([id.as_str()])?;
    }
    Ok(())
}

/// Drop everything queued at `priority` or lower urgency (a narrower sync
/// window); fetched mail is untouched.
pub fn clear_from_priority(tx: &Transaction<'_>, priority: u8) -> StoreResult<usize> {
    Ok(tx.execute("DELETE FROM backfill_queue WHERE priority >= ?1", [priority])?)
}

pub fn len(conn: &Connection) -> StoreResult<u64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM backfill_queue", [], |r| r.get::<_, i64>(0))?.max(0) as u64)
}

/// Ids of the given ones that are not stored with a full body.
pub fn missing(conn: &Connection, ids: &[MessageId]) -> StoreResult<Vec<MessageId>> {
    let mut stmt = conn.prepare_cached("SELECT 1 FROM messages WHERE gmail_id = ?1 AND body_state = 'full'")?;
    let mut out = Vec::new();
    for id in ids {
        if !stmt.exists([id.as_str()])? {
            out.push(id.clone());
        }
    }
    Ok(out)
}
