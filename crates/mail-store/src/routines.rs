//! Stored routines and their runs (spec §11.2, §11.6).

use mail_domain::Millis;
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::error::StoreResult;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutineRow {
    pub uuid: String,
    pub name: String,
    pub enabled: bool,
    pub runner: String,
    pub template_id: Option<String>,
    pub template_version: Option<u32>,
    pub definition_json: String,
    pub sync_fingerprint: Option<String>,
    pub cloud_url: Option<String>,
    pub created_at: Millis,
    pub updated_at: Millis,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRow {
    pub id: i64,
    pub routine_uuid: String,
    pub inferred: bool,
    pub session_uuid: Option<String>,
    pub started_at: Millis,
    pub ended_at: Option<Millis>,
    /// `running`, `succeeded`, `failed`, `missed`, `inferred`, `undone`…
    pub status: String,
    /// `{bucket_id: count}`.
    pub counts_json: String,
    pub report_text: Option<String>,
    pub undone_at: Option<Millis>,
}

/// Insert or replace a routine by uuid; `created_at` is kept on update.
pub fn save(tx: &Transaction<'_>, r: &RoutineRow) -> StoreResult<()> {
    tx.execute(
        "INSERT INTO routines (uuid, name, enabled, runner, template_id, template_version, definition_json,
           sync_fingerprint, cloud_url, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT (uuid) DO UPDATE SET name = excluded.name, enabled = excluded.enabled,
           runner = excluded.runner, template_id = excluded.template_id,
           template_version = excluded.template_version, definition_json = excluded.definition_json,
           sync_fingerprint = excluded.sync_fingerprint, cloud_url = excluded.cloud_url,
           updated_at = excluded.updated_at",
        params![
            r.uuid,
            r.name,
            r.enabled,
            r.runner,
            r.template_id,
            r.template_version,
            r.definition_json,
            r.sync_fingerprint,
            r.cloud_url,
            r.created_at,
            r.updated_at
        ],
    )?;
    Ok(())
}

const COLUMNS: &str = "uuid, name, enabled, runner, template_id, template_version, definition_json, sync_fingerprint,
                       cloud_url, created_at, updated_at";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RoutineRow> {
    Ok(RoutineRow {
        uuid: r.get(0)?,
        name: r.get(1)?,
        enabled: r.get(2)?,
        runner: r.get(3)?,
        template_id: r.get(4)?,
        template_version: r.get(5)?,
        definition_json: r.get(6)?,
        sync_fingerprint: r.get(7)?,
        cloud_url: r.get(8)?,
        created_at: r.get(9)?,
        updated_at: r.get(10)?,
    })
}

pub fn list(conn: &Connection) -> StoreResult<Vec<RoutineRow>> {
    Ok(conn
        .prepare_cached(&format!("SELECT {COLUMNS} FROM routines ORDER BY created_at, id"))?
        .query_map([], row)?
        .collect::<Result<_, _>>()?)
}

pub fn get(conn: &Connection, uuid: &str) -> StoreResult<Option<RoutineRow>> {
    Ok(conn
        .prepare_cached(&format!("SELECT {COLUMNS} FROM routines WHERE uuid = ?1"))?
        .query_row([uuid], row)
        .optional()?)
}

pub fn delete(tx: &Transaction<'_>, uuid: &str) -> StoreResult<()> {
    tx.execute("DELETE FROM routines WHERE uuid = ?1", [uuid])?;
    Ok(())
}

fn routine_rowid(tx: &Transaction<'_>, uuid: &str) -> StoreResult<Option<i64>> {
    Ok(tx.query_row("SELECT id FROM routines WHERE uuid = ?1", [uuid], |r| r.get(0)).optional()?)
}

/// Start (or record) a run; returns its id.
pub fn start_run(
    tx: &Transaction<'_>,
    routine_uuid: &str,
    inferred: bool,
    session_uuid: Option<&str>,
    status: &str,
    now: Millis,
) -> StoreResult<Option<i64>> {
    let Some(routine) = routine_rowid(tx, routine_uuid)? else { return Ok(None) };
    tx.execute(
        "INSERT INTO routine_runs (routine_id, inferred, session_uuid, started_at, status) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![routine, inferred, session_uuid, now, status],
    )?;
    Ok(Some(tx.last_insert_rowid()))
}

pub fn finish_run(
    tx: &Transaction<'_>,
    run: i64,
    status: &str,
    counts_json: &str,
    report: Option<&str>,
    now: Millis,
) -> StoreResult<()> {
    tx.execute(
        "UPDATE routine_runs SET status = ?2, counts_json = ?3, report_text = COALESCE(?4, report_text), ended_at = ?5
         WHERE id = ?1",
        params![run, status, counts_json, report, now],
    )?;
    Ok(())
}

/// Record which threads a run sorted, and into which bucket.
pub fn add_run_threads(tx: &Transaction<'_>, run: i64, threads: &[(String, Option<String>)]) -> StoreResult<()> {
    let mut stmt = tx.prepare_cached(
        "INSERT INTO routine_run_threads (run_id, thread_id, bucket_id) VALUES (?1, ?2, ?3)
         ON CONFLICT (run_id, thread_id) DO UPDATE SET bucket_id = excluded.bucket_id",
    )?;
    for (thread, bucket) in threads {
        stmt.execute(params![run, thread, bucket])?;
    }
    Ok(())
}

/// Record a cloud run, or update the one with this session id.
pub fn upsert_cloud_run(
    tx: &Transaction<'_>,
    routine_uuid: &str,
    session_uuid: &str,
    status: &str,
    started_at: Millis,
    ended_at: Option<Millis>,
) -> StoreResult<Option<i64>> {
    let Some(routine) = routine_rowid(tx, routine_uuid)? else { return Ok(None) };
    let existing: Option<i64> = tx
        .query_row(
            "SELECT id FROM routine_runs WHERE routine_id = ?1 AND session_uuid = ?2",
            params![routine, session_uuid],
            |r| r.get(0),
        )
        .optional()?;
    match existing {
        Some(id) => {
            tx.execute(
                "UPDATE routine_runs SET status = ?2, ended_at = COALESCE(?3, ended_at) WHERE id = ?1",
                params![id, status, ended_at],
            )?;
            Ok(Some(id))
        }
        None => {
            tx.execute(
                "INSERT INTO routine_runs (routine_id, inferred, session_uuid, started_at, ended_at, status)
                 VALUES (?1, 0, ?2, ?3, ?4, ?5)",
                params![routine, session_uuid, started_at, ended_at, status],
            )?;
            Ok(Some(tx.last_insert_rowid()))
        }
    }
}

pub fn run_report(conn: &Connection, run: i64) -> StoreResult<Option<String>> {
    Ok(conn.query_row("SELECT report_text FROM routine_runs WHERE id = ?1", [run], |r| r.get(0)).optional()?.flatten())
}

pub fn set_report(tx: &Transaction<'_>, run: i64, report: &str) -> StoreResult<()> {
    tx.execute("UPDATE routine_runs SET report_text = ?2 WHERE id = ?1", params![run, report])?;
    Ok(())
}

/// The inferred run to add changes to: the routine's latest one if it
/// ended within `gap` of `now` (one sorting pass), else a new one.
pub fn open_inferred_run(
    tx: &Transaction<'_>,
    routine_uuid: &str,
    now: Millis,
    gap: Millis,
) -> StoreResult<Option<i64>> {
    let Some(routine) = routine_rowid(tx, routine_uuid)? else { return Ok(None) };
    let recent: Option<i64> = tx
        .query_row(
            "SELECT id FROM routine_runs WHERE routine_id = ?1 AND inferred = 1 AND undone_at IS NULL
               AND ended_at >= ?2 ORDER BY ended_at DESC LIMIT 1",
            params![routine, now - gap],
            |r| r.get(0),
        )
        .optional()?;
    if recent.is_some() {
        return Ok(recent);
    }
    tx.execute(
        "INSERT INTO routine_runs (routine_id, inferred, started_at, ended_at, status) VALUES (?1, 1, ?2, ?2, 'inferred')",
        params![routine, now],
    )?;
    Ok(Some(tx.last_insert_rowid()))
}

/// Recompute a run's per-bucket counts from its threads; `ended_at` moves
/// to `now` when given.
pub fn recount(tx: &Transaction<'_>, run: i64, now: Option<Millis>) -> StoreResult<()> {
    let counts: Vec<(String, i64)> = tx
        .prepare_cached(
            "SELECT bucket_id, COUNT(*) FROM routine_run_threads WHERE run_id = ?1 AND bucket_id IS NOT NULL
             GROUP BY bucket_id ORDER BY bucket_id",
        )?
        .query_map([run], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    let json = format!(
        "{{{}}}",
        counts
            .iter()
            .map(|(b, n)| format!("{}:{n}", serde_json::to_string(b).unwrap_or_default()))
            .collect::<Vec<_>>()
            .join(",")
    );
    tx.execute("UPDATE routine_runs SET counts_json = ?2 WHERE id = ?1", params![run, json])?;
    if let Some(now) = now {
        tx.execute(
            "UPDATE routine_runs SET ended_at = MAX(COALESCE(ended_at, 0), ?2) WHERE id = ?1",
            params![run, now],
        )?;
    }
    Ok(())
}

/// Inferred runs of a routine that overlap `[start, end]`.
pub fn inferred_runs_between(
    conn: &Connection,
    routine_uuid: &str,
    start: Millis,
    end: Millis,
) -> StoreResult<Vec<i64>> {
    Ok(conn
        .prepare_cached(
            "SELECT r.id FROM routine_runs r JOIN routines ro ON ro.id = r.routine_id
             WHERE ro.uuid = ?1 AND r.inferred = 1 AND r.started_at <= ?3 AND COALESCE(r.ended_at, r.started_at) >= ?2",
        )?
        .query_map(params![routine_uuid, start, end], |r| r.get(0))?
        .collect::<Result<_, _>>()?)
}

/// Fold run `from` into run `into` (an inferred run into the cloud run it
/// turned out to be).
pub fn merge_runs(tx: &Transaction<'_>, from: i64, into: i64) -> StoreResult<()> {
    tx.execute(
        "INSERT OR IGNORE INTO routine_run_threads (run_id, thread_id, bucket_id)
         SELECT ?2, thread_id, bucket_id FROM routine_run_threads WHERE run_id = ?1",
        params![from, into],
    )?;
    tx.execute("DELETE FROM routine_runs WHERE id = ?1", [from])?;
    recount(tx, into, None)
}

/// The routine a run belongs to.
pub fn run_routine(conn: &Connection, run: i64) -> StoreResult<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT ro.uuid FROM routine_runs r JOIN routines ro ON ro.id = r.routine_id WHERE r.id = ?1",
            [run],
            |r| r.get(0),
        )
        .optional()?)
}

pub fn mark_undone(tx: &Transaction<'_>, run: i64, now: Millis) -> StoreResult<()> {
    tx.execute("UPDATE routine_runs SET undone_at = ?2, status = 'undone' WHERE id = ?1", params![run, now])?;
    Ok(())
}

/// A routine's runs, newest first.
pub fn runs(conn: &Connection, routine_uuid: &str, limit: u32) -> StoreResult<Vec<RunRow>> {
    Ok(conn
        .prepare_cached(
            "SELECT r.id, ro.uuid, r.inferred, r.session_uuid, r.started_at, r.ended_at, r.status, r.counts_json,
                    r.report_text, r.undone_at
             FROM routine_runs r JOIN routines ro ON ro.id = r.routine_id
             WHERE ro.uuid = ?1 ORDER BY r.started_at DESC, r.id DESC LIMIT ?2",
        )?
        .query_map(params![routine_uuid, limit], |r| {
            Ok(RunRow {
                id: r.get(0)?,
                routine_uuid: r.get(1)?,
                inferred: r.get(2)?,
                session_uuid: r.get(3)?,
                started_at: r.get(4)?,
                ended_at: r.get(5)?,
                status: r.get(6)?,
                counts_json: r.get(7)?,
                report_text: r.get(8)?,
                undone_at: r.get(9)?,
            })
        })?
        .collect::<Result<_, _>>()?)
}

pub fn run_threads(conn: &Connection, run: i64) -> StoreResult<Vec<(String, Option<String>)>> {
    Ok(conn
        .prepare_cached("SELECT thread_id, bucket_id FROM routine_run_threads WHERE run_id = ?1 ORDER BY thread_id")?
        .query_map([run], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Db;

    #[test]
    fn routines_and_runs() {
        let dir = std::env::temp_dir().join(format!("openagc-routines-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let db = Db::open(&dir.join("mail.sqlite")).unwrap();
        let row = RoutineRow {
            uuid: "r1".into(),
            name: "Sort important mail".into(),
            enabled: true,
            runner: "local".into(),
            template_id: Some("sort-important".into()),
            template_version: Some(1),
            definition_json: "{}".into(),
            sync_fingerprint: None,
            cloud_url: None,
            created_at: 10,
            updated_at: 10,
        };
        db.write_blocking({
            let row = row.clone();
            move |tx| save(tx, &row)
        })
        .unwrap();
        let mut changed = row.clone();
        changed.name = "Sort".into();
        changed.created_at = 99;
        changed.updated_at = 20;
        db.write_blocking(move |tx| save(tx, &changed)).unwrap();
        let got = db.read_blocking(|c| get(c, "r1")).unwrap().unwrap();
        assert_eq!((got.name.as_str(), got.created_at, got.updated_at), ("Sort", 10, 20));
        assert_eq!(db.read_blocking(list).unwrap().len(), 1);

        let run =
            db.write_blocking(|tx| start_run(tx, "r1", false, Some("agent-x-1"), "running", 30)).unwrap().unwrap();
        db.write_blocking(move |tx| {
            add_run_threads(tx, run, &[("t1".into(), Some("daily".into())), ("t2".into(), None)])?;
            finish_run(tx, run, "succeeded", r#"{"daily":1}"#, Some("Sorted 1 thread."), 40)
        })
        .unwrap();
        let runs_ = db.read_blocking(|c| runs(c, "r1", 10)).unwrap();
        assert_eq!(runs_[0].status, "succeeded");
        assert_eq!(runs_[0].report_text.as_deref(), Some("Sorted 1 thread."));
        assert_eq!(db.read_blocking(move |c| run_threads(c, run)).unwrap().len(), 2);
        db.write_blocking(move |tx| mark_undone(tx, run, 50)).unwrap();
        assert_eq!(db.read_blocking(|c| runs(c, "r1", 10)).unwrap()[0].undone_at, Some(50));
        assert!(db.write_blocking(|tx| start_run(tx, "nope", true, None, "inferred", 1)).unwrap().is_none());
        db.write_blocking(|tx| delete(tx, "r1")).unwrap();
        assert!(db.read_blocking(|c| runs(c, "r1", 10)).unwrap().is_empty(), "runs go with the routine");
    }
}
