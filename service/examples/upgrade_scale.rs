//! What does the hot upgrade actually cost, as the data grows?
//!
//! The claim under test: "the upgrade is hot" — `ALTER TABLE ... ALTER COLUMN
//! ... TYPE HUGEINT` is a metadata-cheap operation that rewrites one column.
//!
//! Three things decide whether that claim survives contact with a real
//! deployment:
//!
//! 1. **Does the time scale?** A rewrite is O(rows). If it is 3.7 ms per
//!    million rows, then a billion rows is four seconds and a hundred
//!    billion is seven minutes — and that number is the one that goes into
//!    the issue.
//! 2. **Does the disk spike?** The new column is written before the old one
//!    is freed, and DuckDB never gives freed blocks back to the filesystem —
//!    so the file does not shrink afterwards either.
//! 3. **Is it hot?** A service ingesting events must keep writing while the
//!    upgrade runs. If the write blocks for the whole rewrite, then "hot"
//!    means "hot for readers" only.
//!
//! ```bash
//! cargo run --release --example upgrade_scale -p sumimi-service -- 32000000
//! ```

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use duckdb::Connection;

const VISITORS: i64 = 120_000;

/// Build a table with the identity column, the way the real schema has it.
fn build(path: &PathBuf, rows: i64) -> anyhow::Result<()> {
    let _ = std::fs::remove_file(path);
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "CREATE TABLE probe (
             id BIGINT, website_id VARCHAR, visit_id BIGINT, visitor_id BIGINT, user_id VARCHAR,
             created_at TIMESTAMP, url VARCHAR, title VARCHAR, browser VARCHAR, country VARCHAR);",
    )?;
    conn.execute_batch(&format!(
        "INSERT INTO probe
         SELECT i AS id, 'w1' AS website_id,
            (((i // {VISITORS}) // 4)::BIGINT << 32) | (hash('v' || (i % {VISITORS})) % 4294967296),
            CAST(hash('v' || (i % {VISITORS})) % 9223372036854775807 AS BIGINT),
            CASE WHEN i % 3 = 0 THEN NULL ELSE 'user-' || (i % {VISITORS}) END,
            TIMESTAMP '2026-01-01 00:00:00' + INTERVAL (i * 4) SECOND,
            '/blog/post-' || (i % 5000), 'A title number ' || (i % 1000),
            CASE (i % {VISITORS}) % 4 WHEN 0 THEN 'Chrome' WHEN 1 THEN 'Safari'
                 WHEN 2 THEN 'Firefox' ELSE 'Edge' END, 'DE'
         FROM generate_series(1, {rows}) t(i)"
    ))?;
    conn.execute_batch("CHECKPOINT")?;
    Ok(())
}

/// The size of the file, in bytes.
fn size(path: &PathBuf) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// Watch the file while it is being rewritten: what is the peak?
fn watch_peak(path: &Path, stop: &AtomicBool, peak: &AtomicU64) {
    // Every file in the directory: the WAL and any temporary file count too.
    let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    while !stop.load(Ordering::Relaxed) {
        let mut total = 0u64;
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                total += entry.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
        peak.fetch_max(total, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// One size: build, widen, and report.
fn one(path: &PathBuf, rows: i64) -> anyhow::Result<()> {
    build(path, rows)?;
    let before = size(path);

    let stop = Arc::new(AtomicBool::new(false));
    let peak = Arc::new(AtomicU64::new(0));
    let watcher = {
        let (stop, peak) = (stop.clone(), peak.clone());
        let path = path.clone();
        std::thread::spawn(move || watch_peak(&path, &stop, &peak))
    };

    let conn = Connection::open(path)?;
    let started = Instant::now();
    conn.execute_batch("ALTER TABLE probe ALTER COLUMN visitor_id TYPE HUGEINT")?;
    let alter = started.elapsed().as_secs_f64() * 1000.0;

    let started = Instant::now();
    conn.execute_batch("CHECKPOINT")?;
    let checkpoint = started.elapsed().as_secs_f64() * 1000.0;

    stop.store(true, Ordering::Relaxed);
    let _ = watcher.join();

    let after = size(path);
    let peak = peak.load(Ordering::Relaxed);

    // The data is still there.
    let (rows_now, visitors): (i64, i64) = conn.query_row(
        "SELECT count(*), count(DISTINCT visitor_id) FROM probe",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;

    // The logical size and the size on the disk: a sparse file looks huge
    // and takes nothing.
    let allocated = std::fs::metadata(path).map(|m| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            m.blocks() * 512
        }
        #[cfg(not(unix))]
        {
            m.len()
        }
    }).unwrap_or(0);

    println!(
        "  {rows:>10} rows | the alter {alter:>9.1}ms | the checkpoint {checkpoint:>8.1}ms | \
         {:>7.2} ms/million | the file {:>7.2} → {:>7.2} GB (on the disk {:>7.2} GB, the peak {:>7.2} GB) | \
         {rows_now} rows, {visitors} visitors",
        before as f64 / 1_073_741_824.0,
        after as f64 / 1_073_741_824.0,
        allocated as f64 / 1_073_741_824.0,
        peak as f64 / 1_073_741_824.0,
        alter / (rows as f64 / 1_000_000.0),
    );
    let _ = std::fs::remove_file(path);
    Ok(())
}

/// The hot part: a service writing while the upgrade runs.
fn concurrent(path: &PathBuf, rows: i64) -> anyhow::Result<()> {
    build(path, rows)?;
    println!("\n  the hot part: {rows} rows, a writer and a reader while the upgrade runs\n");

    // A reader that keeps working for a while.
    let reader = {
        let path = path.clone();
        std::thread::spawn(move || -> anyhow::Result<(usize, f64)> {
            let conn = Connection::open(&path)?;
            let started = Instant::now();
            let mut scans = 0usize;
            while started.elapsed() < Duration::from_secs(4) {
                let sum: i64 = conn.query_row("SELECT SUM(id) FROM probe", [], |r| r.get(0))?;
                if sum < 0 {
                    return Err(anyhow::anyhow!("impossible"));
                }
                scans += 1;
            }
            Ok((scans, started.elapsed().as_secs_f64()))
        })
    };

    std::thread::sleep(Duration::from_millis(300));

    // The upgrade, on its own connection.
    let upgrader = {
        let path = path.clone();
        std::thread::spawn(move || -> anyhow::Result<f64> {
            let conn = Connection::open(&path)?;
            let started = Instant::now();
            conn.execute_batch("ALTER TABLE probe ALTER COLUMN visitor_id TYPE HUGEINT")?;
            Ok(started.elapsed().as_secs_f64() * 1000.0)
        })
    };

    // A write, right in the middle of it — the ALTER must still be running.
    let writer = {
        let path = path.clone();
        std::thread::spawn(move || -> anyhow::Result<f64> {
            let conn = Connection::open(&path)?;
            std::thread::sleep(Duration::from_millis(20));
            let started = Instant::now();
            conn.execute_batch(
                "INSERT INTO probe SELECT 999999999, 'w1', 1, 1, NULL, now(), '/x', 'x', 'Chrome', 'DE'",
            )?;
            Ok(started.elapsed().as_secs_f64() * 1000.0)
        })
    };

    let alter = upgrader.join().map_err(|_| anyhow::anyhow!("the upgrade panicked"))??;
    let write = writer.join().map_err(|_| anyhow::anyhow!("the write panicked"))??;
    let (scans, seconds) = reader.join().map_err(|_| anyhow::anyhow!("the reader panicked"))??;

    // Did the row that was written during the upgrade survive it?
    let conn = Connection::open(path)?;
    let (rows_now, survivors): (i64, i64) = conn.query_row(
        "SELECT count(*), count(*) FILTER (WHERE id = 999999999) FROM probe",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;

    println!("  the upgrade       : {alter:.1}ms");
    println!("  the write, during : {write:.1}ms  ← it waited this long for the upgrade");
    println!("  the reader        : {scans} full scans in {seconds:.1}s, without an error");
    println!("  after: {rows_now} rows (expected {}), the row written during it: {survivors}",
        rows + 1);
    let _ = std::fs::remove_file(path);
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let max: i64 = std::env::args()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(16_000_000);
    let path = PathBuf::from(
        std::env::args().nth(2).unwrap_or_else(|| "/tmp/upgrade-scale.duckdb".to_string()),
    );

    println!("the hot upgrade, as the data grows\n");
    println!(
        "  {:>10} | {:>28} | {:>22} | the data",
        "the size", "the alter", "the file"
    );
    let mut rows = 1_000_000i64;
    while rows <= max {
        one(&path, rows)?;
        rows *= 2;
    }

    concurrent(&path, 32_000_000)?;
    Ok(())
}
