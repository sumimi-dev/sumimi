//! The smallest reproduction we can find.
//!
//! Part one: the DDL alone never loses anything — a plain insert, a
//! checkpoint, and a DDL, all in one connection, keep every row.
//!
//! Part two: the DDL loses the rows that another connection commits **while
//! it is running**. To take the race out of the picture, the writer keeps
//! inserting in a loop for as long as the DDL takes; afterwards we count how
//! many of those rows are gone.
//!
//! Two connections to the same file, in one process, is enough — DuckDB caches
//! the database instance per path, so the second connection joins the first.
//! (A second *process* cannot open the file: DuckDB takes a file lock.)
//!
//! ```bash
//! cargo run --release --example repro_min -p sumimi-service -- 8000000
//! ```

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use duckdb::Connection;

/// The first inserted id of the writer's series.
const WRITER_BASE: i64 = 1_000_000_000;

fn create(path: &PathBuf, rows: i64) -> anyhow::Result<()> {
    let _ = std::fs::remove_file(path);
    let conn = Connection::open(path)?;
    conn.execute_batch("CREATE TABLE probe (id BIGINT, visitor_id BIGINT, payload VARCHAR);")?;
    conn.execute_batch(&format!(
        "INSERT INTO probe SELECT i, i, 'x' FROM range({rows}) t(i);"
    ))?;
    conn.execute_batch("CHECKPOINT;")?;
    Ok(())
}

/// Part one: one connection, no concurrency — everything is kept.
fn sequential(rows: i64) -> anyhow::Result<()> {
    println!("  one connection, no concurrency:");
    for (label, checkpoint, ddl) in [
        (
            "insert → CHECKPOINT → ALTER COLUMN TYPE",
            true,
            "ALTER TABLE probe ALTER COLUMN visitor_id TYPE HUGEINT;",
        ),
        (
            "insert (no checkpoint) → ALTER COLUMN TYPE",
            false,
            "ALTER TABLE probe ALTER COLUMN visitor_id TYPE HUGEINT;",
        ),
        (
            "insert (no checkpoint) → ADD COLUMN",
            false,
            "ALTER TABLE probe ADD COLUMN visitor_id16 HUGEINT;",
        ),
    ] {
        let path = PathBuf::from("/tmp/repro-min-seq.duckdb");
        create(&path, rows)?;
        let conn = Connection::open(&path)?;
        if !checkpoint {
            // undo the checkpoint the create did: write more rows that stay
            // in local storage
            conn.execute_batch(&format!(
                "INSERT INTO probe SELECT i + {rows}, i + {rows}, 'x' FROM range(1000) t(i);"
            ))?;
        }
        let before: i64 = conn.query_row("SELECT count(*) FROM probe", [], |r| r.get(0))?;
        conn.execute_batch(ddl)?;
        let after: i64 = conn.query_row("SELECT count(*) FROM probe", [], |r| r.get(0))?;
        println!(
            "    {label:<44} {before:>9} → {after:>9}   {}",
            if after == before { "kept" } else { "LOST" }
        );
        let _ = std::fs::remove_file(&path);
    }
    Ok(())
}

/// Part two: two connections — the DDL drops what the writer commits meanwhile.
fn concurrent(rows: i64, ddl: &str, label: &str) -> anyhow::Result<()> {
    let path = PathBuf::from("/tmp/repro-min-conc.duckdb");
    create(&path, rows)?;

    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let (path, stop) = (path.clone(), stop.clone());
        std::thread::spawn(move || -> anyhow::Result<(i64, i64)> {
            let conn = Connection::open(&path)?;
            let mut written = 0i64;
            let mut errors = 0i64;
            while !stop.load(Ordering::Relaxed) {
                let id = WRITER_BASE + written;
                match conn.execute_batch(&format!(
                    "INSERT INTO probe VALUES ({id}, {id}, 'x');"
                )) {
                    Ok(()) => written += 1,
                    Err(_) => errors += 1,
                }
            }
            Ok((written, errors))
        })
    };

    // Let the writer get going, then run the DDL while it keeps writing.
    std::thread::sleep(std::time::Duration::from_millis(5));
    let conn = Connection::open(&path)?;
    let started = Instant::now();
    let outcome = conn.execute_batch(ddl);
    let ddl_ms = started.elapsed().as_secs_f64() * 1000.0;
    stop.store(true, Ordering::Relaxed);
    let (written, errors) = writer.join().map_err(|_| anyhow::anyhow!("panic"))??;

    let missing: i64 = conn.query_row(
        "SELECT count(*) FROM probe WHERE id >= ?",
        [WRITER_BASE],
        |r| r.get(0),
    )?;
    let survived = written - missing;

    println!(
        "  {label:<26} the ddl {ddl_ms:>6.1}ms | the writer wrote {written:>6} rows \
         ({errors} errors) | survived {survived:>6} | LOST {missing:>6}",
    );
    println!(
        "    the ddl said: {}",
        match outcome {
            Ok(()) => "ok".to_string(),
            Err(e) => format!("ERROR: {e}"),
        }
    );
    let _ = std::fs::remove_file(&path);
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let rows: i64 = std::env::args()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(8_000_000);
    println!("{rows} rows\n");
    sequential(rows)?;
    println!("\n  two connections, the writer keeps inserting while the ddl runs:");
    concurrent(
        rows,
        "ALTER TABLE probe ALTER COLUMN visitor_id TYPE HUGEINT;",
        "ALTER COLUMN TYPE",
    )?;
    concurrent(
        rows,
        "ALTER TABLE probe ADD COLUMN visitor_id16 HUGEINT;",
        "ADD COLUMN",
    )?;
    Ok(())
}
