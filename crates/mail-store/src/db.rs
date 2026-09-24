//! Connections, pragmas, migrations and the one-writer / N-reader model
//! (spec §6.1, §6.4).
//!
//! - One dedicated writer thread owns the only read-write connection. Writes
//!   are closures sent over a channel and run inside a transaction.
//! - A small pool of read-only connections serves reads. In WAL mode readers
//!   never wait for the writer.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use rusqlite::{Connection, OpenFlags, Transaction};
use tokio::sync::{Semaphore, oneshot};

use crate::error::{StoreError, StoreResult};

/// Migrations in order. `PRAGMA user_version` records how many have run.
const MIGRATIONS: &[&str] =
    &[include_str!("../migrations/0001_initial.sql"), include_str!("../migrations/0002_draft_state.sql")];

pub const READER_COUNT: usize = 4;

pub fn schema_version() -> u32 {
    MIGRATIONS.len() as u32
}

type WriteJob = Box<dyn FnOnce(&mut Connection) + Send>;

/// Handle to an open store. Cheap to clone.
#[derive(Clone)]
pub struct Db {
    inner: Arc<Inner>,
}

struct Inner {
    path: PathBuf,
    writer: Mutex<Option<mpsc::Sender<WriteJob>>>,
    readers: Mutex<Vec<Connection>>,
    permits: Semaphore,
}

impl Db {
    /// Open (creating if needed) and migrate the database at `path`.
    pub fn open(path: &Path) -> StoreResult<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| StoreError::Io(e.to_string()))?;
        }
        let mut writer = Connection::open(path)?;
        configure(&writer, true)?;
        migrate(&mut writer)?;

        let mut readers = Vec::with_capacity(READER_COUNT);
        for _ in 0..READER_COUNT {
            let conn = Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_URI,
            )?;
            configure(&conn, false)?;
            readers.push(conn);
        }

        let (tx, rx) = mpsc::channel::<WriteJob>();
        thread::Builder::new()
            .name("openagc-store-writer".into())
            .spawn(move || {
                let mut conn = writer;
                while let Ok(job) = rx.recv() {
                    job(&mut conn);
                }
            })
            .map_err(|e| StoreError::Io(e.to_string()))?;

        Ok(Self {
            inner: Arc::new(Inner {
                path: path.to_owned(),
                writer: Mutex::new(Some(tx)),
                readers: Mutex::new(readers),
                permits: Semaphore::new(READER_COUNT),
            }),
        })
    }

    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// Run `f` in a write transaction on the writer thread. Commits if `f`
    /// returns `Ok`, rolls back otherwise.
    pub async fn write<T, F>(&self, f: F) -> StoreResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&Transaction<'_>) -> StoreResult<T> + Send + 'static,
    {
        let rx = self.submit(f)?;
        rx.await.map_err(|_| StoreError::Closed)?
    }

    /// Blocking variant of [`Db::write`] for non-async callers and tests.
    /// Must not be called from inside a tokio runtime.
    pub fn write_blocking<T, F>(&self, f: F) -> StoreResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&Transaction<'_>) -> StoreResult<T> + Send + 'static,
    {
        let rx = self.submit(f)?;
        rx.blocking_recv().map_err(|_| StoreError::Closed)?
    }

    fn submit<T, F>(&self, f: F) -> StoreResult<oneshot::Receiver<StoreResult<T>>>
    where
        T: Send + 'static,
        F: FnOnce(&Transaction<'_>) -> StoreResult<T> + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let job: WriteJob = Box::new(move |conn| {
            let result = (|| {
                let txn = conn.transaction()?;
                let value = f(&txn)?;
                txn.commit()?;
                Ok(value)
            })();
            let _ = tx.send(result);
        });
        let guard = self.inner.writer.lock().unwrap_or_else(|e| e.into_inner());
        guard.as_ref().ok_or(StoreError::Closed)?.send(job).map_err(|_| StoreError::Closed)?;
        Ok(rx)
    }

    /// Run `f` on a pooled read-only connection, off the async executor.
    pub async fn read<T, F>(&self, f: F) -> StoreResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> StoreResult<T> + Send + 'static,
    {
        let _permit = self.inner.permits.acquire().await.map_err(|_| StoreError::Closed)?;
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || with_reader(&inner, f))
            .await
            .map_err(|e| StoreError::Io(format!("read task failed: {e}")))?
    }

    /// Blocking variant of [`Db::read`].
    pub fn read_blocking<T, F>(&self, f: F) -> StoreResult<T>
    where
        F: FnOnce(&Connection) -> StoreResult<T>,
    {
        with_reader(&self.inner, f)
    }

    /// Stop accepting writes. Queued writes still run.
    pub fn close(&self) {
        self.inner.writer.lock().unwrap_or_else(|e| e.into_inner()).take();
    }
}

fn with_reader<T>(inner: &Inner, f: impl FnOnce(&Connection) -> StoreResult<T>) -> StoreResult<T> {
    let conn = inner.readers.lock().unwrap_or_else(|e| e.into_inner()).pop();
    // All pooled readers busy (only possible for blocking callers, which
    // skip the semaphore): open a temporary one rather than wait.
    let conn = match conn {
        Some(c) => c,
        None => {
            let c = Connection::open_with_flags(&inner.path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            configure(&c, false)?;
            return f(&c);
        }
    };
    let result = f(&conn);
    inner.readers.lock().unwrap_or_else(|e| e.into_inner()).push(conn);
    result
}

fn configure(conn: &Connection, writer: bool) -> StoreResult<()> {
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    if writer {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "cache_size", -65_536)?; // 64 MB
    } else {
        conn.pragma_update(None, "cache_size", -16_384)?; // 16 MB
    }
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.pragma_update(None, "mmap_size", 256 * 1024 * 1024)?;
    conn.set_prepared_statement_cache_capacity(128);
    Ok(())
}

fn migrate(conn: &mut Connection) -> StoreResult<()> {
    let current: u32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    let target = schema_version();
    if current > target {
        return Err(StoreError::Migration(format!(
            "database schema v{current} is newer than this build (v{target}); update OpenAGC"
        )));
    }
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        let version = i as u32 + 1;
        let txn = conn.transaction()?;
        txn.execute_batch(sql).map_err(|e| StoreError::Migration(format!("v{version}: {e}")))?;
        // user_version cannot be bound as a parameter.
        txn.execute_batch(&format!("PRAGMA user_version = {version}"))?;
        txn.commit()?;
        tracing::info!(version, "store migrated");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn temp_db_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("openagc-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("mail.sqlite")
    }

    #[test]
    fn opens_migrates_and_reports_version() {
        let db = Db::open(&temp_db_path("migrate")).unwrap();
        let version: u32 = db.read_blocking(|c| Ok(c.pragma_query_value(None, "user_version", |r| r.get(0))?)).unwrap();
        assert_eq!(version, schema_version());
        let mode: String = db.read_blocking(|c| Ok(c.pragma_query_value(None, "journal_mode", |r| r.get(0))?)).unwrap();
        assert_eq!(mode, "wal");
        // Seeded virtual label.
        let n: i64 = db
            .read_blocking(|c| {
                Ok(c.query_row("SELECT COUNT(*) FROM labels WHERE gmail_id = '@archive'", [], |r| r.get(0))?)
            })
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn reopening_does_not_rerun_migrations() {
        let path = temp_db_path("reopen");
        Db::open(&path).unwrap().close();
        let db = Db::open(&path).unwrap();
        let n: i64 = db.read_blocking(|c| Ok(c.query_row("SELECT COUNT(*) FROM labels", [], |r| r.get(0))?)).unwrap();
        assert_eq!(n, 1, "seed row inserted once");
    }

    #[test]
    fn newer_schema_is_refused() {
        let path = temp_db_path("newer");
        Db::open(&path).unwrap().close();
        let c = Connection::open(&path).unwrap();
        c.execute_batch("PRAGMA user_version = 999").unwrap();
        drop(c);
        let err = Db::open(&path).err().unwrap();
        assert!(matches!(err, StoreError::Migration(_)), "{err}");
    }

    #[test]
    fn failed_write_rolls_back() {
        let db = Db::open(&temp_db_path("rollback")).unwrap();
        let r: StoreResult<()> = db.write_blocking(|t| {
            t.execute("INSERT INTO sync_state (key, value) VALUES ('k', 'v')", [])?;
            Err(StoreError::NotFound("forced".into()))
        });
        assert!(r.is_err());
        let n: i64 =
            db.read_blocking(|c| Ok(c.query_row("SELECT COUNT(*) FROM sync_state", [], |r| r.get(0))?)).unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn readers_see_committed_writes_and_run_concurrently() {
        let db = Db::open(&temp_db_path("concurrent")).unwrap();
        db.write(|t| {
            t.execute("INSERT INTO sync_state (key, value) VALUES ('history_id', '42')", [])?;
            Ok(())
        })
        .await
        .unwrap();
        let reads = (0..16).map(|_| {
            let db = db.clone();
            tokio::spawn(async move {
                db.read(|c| {
                    Ok(c.query_row("SELECT value FROM sync_state WHERE key = 'history_id'", [], |r| {
                        r.get::<_, String>(0)
                    })?)
                })
                .await
            })
        });
        for r in reads {
            assert_eq!(r.await.unwrap().unwrap(), "42");
        }
    }

    #[test]
    fn fts5_contentless_delete_and_trigram_are_available() {
        let db = Db::open(&temp_db_path("fts")).unwrap();
        db.write_blocking(|t| {
            t.execute("INSERT INTO messages_fts (rowid, subject, body) VALUES (7, 'Quarterly report', 'numbers')", [])?;
            t.execute("DELETE FROM messages_fts WHERE rowid = 7", [])?;
            t.execute("INSERT INTO contacts (email, name) VALUES ('johnny@example.com', 'Johnny')", [])?;
            Ok(())
        })
        .unwrap();
        let (fts, tri): (i64, i64) = db
            .read_blocking(|c| {
                let fts =
                    c.query_row("SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'quarterly'", [], |r| {
                        r.get(0)
                    })?;
                let tri =
                    c.query_row("SELECT COUNT(*) FROM contacts_fts WHERE contacts_fts MATCH 'ohn'", [], |r| r.get(0))?;
                Ok((fts, tri))
            })
            .unwrap();
        assert_eq!(fts, 0, "row deleted by rowid");
        assert_eq!(tri, 1, "trigram substring match");
    }
}
