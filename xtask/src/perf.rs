//! `cargo xtask fixture` and `cargo xtask perf` (spec §13 rule 10).
//!
//! The fixture is a deterministic synthetic mailbox of about 100k messages.
//! `perf` measures the store operations behind every §1.3 interaction and
//! fails if a p95 exceeds its budget. Budgets are the store's share of the
//! end-to-end targets; the rest is FFI, SwiftUI/AppKit and WebKit.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use mail_domain::{MessageId, ThreadId};
use mail_store::demo::{DemoSpec, generate};
use mail_store::{Db, read};

pub const DEFAULT_MESSAGES: u32 = 100_000;

pub fn fixture_path(root: &Path, messages: u32) -> PathBuf {
    root.join("build/fixtures").join(format!("mail-{messages}.sqlite"))
}

/// Build (or reuse) the fixture database; returns its path.
pub fn fixture(root: &Path, messages: u32, force: bool) -> Result<PathBuf> {
    let path = fixture_path(root, messages);
    if path.exists() && !force {
        println!("fixture: reusing {}", path.display());
        return Ok(path);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    let db = Db::open(&path)?;
    // The demo generator averages ~1.75 messages per thread.
    let threads = (f64::from(messages) / 1.75).round() as u32;
    let started = Instant::now();
    let stats = generate(&db, &DemoSpec { threads, span_days: 5 * 365, ..Default::default() })?;
    db.close();
    println!(
        "fixture: {} threads, {} messages in {:.1}s → {}",
        stats.threads,
        stats.messages,
        started.elapsed().as_secs_f64(),
        path.display()
    );
    Ok(path)
}

struct Measure {
    name: &'static str,
    budget: Duration,
    samples: Vec<Duration>,
}

impl Measure {
    fn p(&self, q: f64) -> Duration {
        let mut s = self.samples.clone();
        s.sort();
        s[((s.len() as f64 - 1.0) * q).round() as usize]
    }
}

fn time<T>(runs: usize, mut f: impl FnMut(usize) -> Result<T>) -> Result<Vec<Duration>> {
    // One untimed warm-up, as the app's first access warms the page cache.
    f(0)?;
    let mut out = Vec::with_capacity(runs);
    for i in 0..runs {
        let t = Instant::now();
        f(i)?;
        out.push(t.elapsed());
    }
    Ok(out)
}

pub fn perf(root: &Path, messages: u32) -> Result<()> {
    let path = fixture(root, messages, false)?;
    let runs = 60;
    let mut results = Vec::new();

    // Cold launch: open (migrations are a no-op) and read the first page.
    results.push(Measure {
        name: "open store + first inbox page",
        budget: Duration::from_millis(60),
        samples: time(10, |_| {
            let db = Db::open(&path)?;
            let page = db.read_blocking(|c| read::list_threads(c, "INBOX", None, 150))?;
            db.close();
            Ok(page)
        })?,
    });

    let db = Db::open(&path)?;
    let inbox = db.read_blocking(|c| read::list_threads(c, "INBOX", None, 150))?;
    anyhow::ensure!(!inbox.rows.is_empty(), "fixture inbox is empty");

    results.push(Measure {
        name: "sidebar mailboxes with counts",
        budget: Duration::from_millis(3),
        samples: time(runs, |_| db.read_blocking(read::list_mailboxes).map_err(Into::into))?,
    });
    results.push(Measure {
        name: "inbox first page (150 rows)",
        budget: Duration::from_millis(8),
        samples: time(runs, |_| db.read_blocking(|c| read::list_threads(c, "INBOX", None, 150)).map_err(Into::into))?,
    });

    // Deep paging: walk the archive to its end once, then time pages from
    // cursors spread across it.
    let mut cursors = vec![None];
    let mut cursor: Option<String> = None;
    let mut archive_rows = 0usize;
    loop {
        let c = cursor.clone();
        let page =
            db.read_blocking(move |conn| read::list_threads(conn, mail_store::ARCHIVE_LABEL, c.as_deref(), 500))?;
        archive_rows += page.rows.len();
        match page.next_cursor {
            Some(next) => {
                cursors.push(Some(next.clone()));
                cursor = Some(next);
            }
            None => break,
        }
    }
    results.push(Measure {
        name: "archive page at any depth (150 rows)",
        budget: Duration::from_millis(8),
        samples: time(runs, |i| {
            let c = cursors[i * 7 % cursors.len()].clone();
            db.read_blocking(move |conn| read::list_threads(conn, mail_store::ARCHIVE_LABEL, c.as_deref(), 150))
                .map_err(Into::into)
        })?,
    });

    let ids: Vec<ThreadId> = inbox.rows.iter().map(|t| t.id.clone()).collect();
    results.push(Measure {
        name: "open thread: detail + all bodies",
        budget: Duration::from_millis(5),
        samples: time(runs, |i| {
            let id = ids[i % ids.len()].clone();
            let (_, messages) = db.read_blocking(|c| read::get_thread(c, &id))?.context("thread vanished")?;
            for m in &messages {
                let mid: MessageId = m.id.clone();
                db.read_blocking(|c| read::get_body(c, &mid))?;
            }
            Ok(())
        })?,
    });

    // Search: each query as typed, the store's share of "keystroke →
    // results < 30 ms".
    for (name, query) in [
        ("search: free text \"roadmap\"", "roadmap"),
        ("search: prefix as typed \"quar\"", "quar"),
        ("search: from:rivera", "from:rivera"),
        ("search: structured is:unread in:inbox", "is:unread in:inbox"),
        ("search: text + filters", "invoice has:attachment newer_than:1y"),
    ] {
        let expr = mail_store::search::parse(query)?;
        let now = inbox.rows[0].last_message_at;
        results.push(Measure {
            name,
            budget: Duration::from_millis(20),
            samples: time(20, |_| {
                db.read_blocking(|c| mail_store::search::search(c, &expr, now, None, 100)).map_err(Into::into)
            })?,
        });
    }

    let (total_threads, total_messages): (i64, i64) = db.read_blocking(|c| {
        Ok(c.query_row("SELECT (SELECT COUNT(*) FROM threads), (SELECT COUNT(*) FROM messages)", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?)
    })?;
    println!(
        "\nperf: {total_messages} messages, {total_threads} threads, {archive_rows} archived threads, {} inbox rows",
        inbox.rows.len()
    );
    println!("{:<40} {:>9} {:>9} {:>9}  result", "operation", "p50", "p95", "budget");
    let mut failed = 0;
    for m in &results {
        let ok = m.p(0.95) <= m.budget;
        failed += usize::from(!ok);
        println!(
            "{:<40} {:>7.2}ms {:>7.2}ms {:>7.2}ms  {}",
            m.name,
            m.p(0.5).as_secs_f64() * 1e3,
            m.p(0.95).as_secs_f64() * 1e3,
            m.budget.as_secs_f64() * 1e3,
            if ok { "ok" } else { "OVER BUDGET" }
        );
    }
    if failed > 0 {
        bail!("{failed} operation(s) over budget");
    }
    Ok(())
}
