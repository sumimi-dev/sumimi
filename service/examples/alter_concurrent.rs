//! Does the hot upgrade lose a row that is written while it runs?
//!
//! The upgrade is claimed to be hot: `ALTER TABLE ... ALTER COLUMN ... TYPE`
//! rewrites one column while the service keeps serving. But "hot" has two
//! halves — readers are not blocked, and **writers are not blocked**. A write
//! that commits while the upgrade is running has to survive it.
//!
//! This writes one row in the middle of the upgrade, checks that the row is
//! there right after it was written, and checks that it is still there after
//! the upgrade finishes.
//!
//! ```bash
//! cargo run --release --example alter_concurrent -p sumimi-service -- 32000000
//! ```

use std::path::PathBuf;
use std::time::{Duration, Instant};

use duckdb::Connection;

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
            (((i // 120000) // 4)::BIGINT << 32) | (hash('v' || (i % 120000)) % 4294967296),
            CAST(hash('v' || (i % 120000)) % 9223372036854775807 AS BIGINT),
            CASE WHEN i % 3 = 0 THEN NULL ELSE 'user-' || (i % 120000) END,
            TIMESTAMP '2026-01-01 00:00:00' + INTERVAL (i * 4) SECOND,
            '/blog/post-' || (i % 5000), 'A title number ' || (i % 1000),
            CASE (i % 120000) % 4 WHEN 0 THEN 'Chrome' WHEN 1 THEN 'Safari'
                 WHEN 2 THEN 'Firefox' ELSE 'Edge' END, 'DE'
         FROM generate_series(1, {rows}) t(i)"
    ))?;
    conn.execute_batch("CHECKPOINT")?;
    Ok(())
}

/// The two routes to the wider column.
const ALTER: &str = "ALTER TABLE probe ALTER COLUMN visitor_id TYPE HUGEINT";
const ADD_COLUMN: &str = "ALTER TABLE probe ADD COLUMN visitor_id16 HUGEINT";

/// One round: write in the middle of the upgrade, and watch that row.
fn round(path: &PathBuf, rows: i64, offset_ms: u64, route: &'static str, label: &str, form: &'static str) -> anyhow::Result<()> {
    build(path, rows)?;

    // The upgrade, on its own connection.
    let upgrader = {
        let path = path.clone();
        std::thread::spawn(move || -> anyhow::Result<(f64, String)> {
            let conn = Connection::open(&path)?;
            let started = Instant::now();
            let outcome = conn.execute_batch(route);
            let ms = started.elapsed().as_secs_f64() * 1000.0;
            Ok((ms, match outcome {
                Ok(()) => "ok".to_string(),
                Err(e) => format!("ERROR: {e}"),
            }))
        })
    };

    // The write, offset into the upgrade.
    let writer = {
        let path = path.clone();
        std::thread::spawn(move || -> anyhow::Result<(f64, String, i64)> {
            std::thread::sleep(Duration::from_millis(offset_ms));
            let conn = Connection::open(&path)?;
            let started = Instant::now();
            let outcome = conn.execute_batch(if form == "values" {
                "INSERT INTO probe (id, website_id, visit_id, visitor_id, user_id, created_at, url, title, browser, country) \
                 VALUES (999999999, 'w1', 1, 1, NULL, now(), '/x', 'x', 'Chrome', 'DE')"
            } else {
                "INSERT INTO probe (id, website_id, visit_id, visitor_id, user_id, created_at, url, title, browser, country) \
                 SELECT 999999999, 'w1', 1, 1, NULL, now(), '/x', 'x', 'Chrome', 'DE'"
            });
            let ms = started.elapsed().as_secs_f64() * 1000.0;
            let err = match outcome {
                Ok(()) => "ok".to_string(),
                Err(e) => format!("ERROR: {e}"),
            };
            // Is the row there, right now — and which table is this writer on?
            let here: i64 = conn.query_row(
                "SELECT count(*) FROM probe WHERE id = 999999999",
                [],
                |r| r.get(0),
            )?;
            let kind: String = conn
                .query_row("SELECT typeof(visitor_id) FROM probe LIMIT 1", [], |r| r.get(0))
                .unwrap_or_else(|_| "?".to_string());
            Ok((ms, format!("{err} [{kind}]"), here))
        })
    };

    let (alter_ms, alter_outcome) = upgrader.join().map_err(|_| anyhow::anyhow!("panic"))??;
    let (write_ms, write_outcome, there) = writer.join().map_err(|_| anyhow::anyhow!("panic"))??;

    // Right after the upgrade, before anything is checkpointed.
    let conn = Connection::open(path)?;
    let (rows_before, alive_before): (i64, i64) = conn.query_row(
        "SELECT count(*), count(*) FILTER (WHERE id = 999999999) FROM probe",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    conn.execute_batch("CHECKPOINT")?;
    let (rows_now, survivors): (i64, i64) = conn.query_row(
        "SELECT count(*), count(*) FILTER (WHERE id = 999999999) FROM probe",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let (rows_before, alive_before) = (rows_before, alive_before);

    println!(
        "  {:<14} {:<7} the write at +{offset_ms:>3}ms | the upgrade {alter_ms:>7.1}ms ({alter_outcome}) | \
         the write took {write_ms:>6.1}ms ({write_outcome}) | the row right after the write: {there} | \
         right after: {rows_before} rows, the row: {alive_before} | \
         after the checkpoint: {rows_now} rows (expected {}), the row: {survivors}",
        label,
        form,
        rows + 1
    );
    let _ = std::fs::remove_file(path);
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let rows: i64 = std::env::args()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(32_000_000);
    let path = PathBuf::from("/tmp/alter-concurrent.duckdb");
    println!("{rows} rows — a row written while the upgrade runs\n");
    for (route, label) in [(ALTER, "alter type"), (ADD_COLUMN, "add column")] {
        for form in ["select", "values"] {
            for offset in [0u64, 60] {
                round(&path, rows, offset, route, label, form)?;
            }
        }
    }
    Ok(())
}
