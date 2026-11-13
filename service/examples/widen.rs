//! Can a column be widened in place — BIGINT to HUGEINT — without moving the data?
//!
//! That is the hinge: if the upgrade is a hot one (no export, no import, no
//! downtime), the width can be a per-site choice instead of a global one.
//!
//! ```bash
//! cargo run --release --example widen -p sumimi-service -- 2000000
//! ```

use std::path::PathBuf;
use std::time::Instant;

use sumimi_service::pool::Db;

fn build(path: &PathBuf, rows: i64) -> anyhow::Result<()> {
    let _ = std::fs::remove_file(path);
    let db = Db::open(path, "256MB")?;
    db.with_read(|conn| {
        conn.execute_batch(
            "CREATE TABLE probe (
                 id BIGINT, website_id VARCHAR, visit_id BIGINT, visitor_id BIGINT,
                 user_id VARCHAR, created_at TIMESTAMP, url VARCHAR);",
        )?;
        Ok::<_, anyhow::Error>(())
    })?;
    db.with_write(|conn| {
        conn.execute_batch(&format!(
            "INSERT INTO probe
             SELECT i AS id, 'w1', ((i // 120000) // 4)::BIGINT << 32 | (hash('v' || (i % 120000)) % 4294967296),
                CAST(hash('v' || (i % 120000)) % 9223372036854775807 AS BIGINT),
                CASE WHEN i % 3 = 0 THEN 'user-' || (i % 120000) ELSE NULL END,
                TIMESTAMP '2026-01-01 00:00:00' + INTERVAL (i * 4) SECOND,
                '/blog/post-' || (i % 5000)
             FROM generate_series(1, {rows}) t(i)"
        ))?;
        Ok::<_, anyhow::Error>(())
    })?;
    db.with_read(|conn| {
        conn.execute_batch("CHECKPOINT")?;
        Ok::<_, anyhow::Error>(())
    })?;
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let rows: i64 = std::env::args()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000_000);
    let path = PathBuf::from("/tmp/widen.duckdb");
    build(&path, rows)?;
    let before = std::fs::metadata(&path)?.len();
    println!("{rows} rows, the file is {:.2} MB\n", before as f64 / 1_048_576.0);

    // 1. Widen the column in place.
    let db = Db::open(&path, "256MB")?;
    let started = Instant::now();
    let outcome = db.with_write(|conn| {
        let result = conn.execute_batch(
            "ALTER TABLE probe ALTER COLUMN visitor_id TYPE HUGEINT",
        );
        Ok::<_, anyhow::Error>(result)
    });
    let widen = started.elapsed().as_secs_f64() * 1000.0;
    match &outcome {
        Ok(Ok(())) => println!("ALTER COLUMN TYPE BIGINT -> HUGEINT : {widen:.1}ms  ok"),
        Ok(Err(e)) => println!("ALTER COLUMN TYPE BIGINT -> HUGEINT : {widen:.1}ms  ERROR {e}"),
        Err(e) => println!("ALTER COLUMN TYPE BIGINT -> HUGEINT : {widen:.1}ms  ERROR {e}"),
    }
    db.with_read(|conn| {
        conn.execute_batch("CHECKPOINT")?;
        Ok::<_, anyhow::Error>(())
    })?;
    drop(db);
    let after = std::fs::metadata(&path)?.len();
    println!("  the file is now {:.2} MB", after as f64 / 1_048_576.0);

    // 2. Is the data still there, and does the old value survive the cast?
    let db = Db::open(&path, "256MB")?;
    let (rows_now, visitors, widest): (i64, i64, i64) = db.with_read(|conn| {
        let rows: i64 = conn.query_row("SELECT count(*) FROM probe", [], |r| r.get(0))?;
        let visitors: i64 = conn.query_row("SELECT count(DISTINCT visitor_id) FROM probe", [], |r| r.get(0))?;
        let widest: i64 =
            conn.query_row("SELECT count(DISTINCT visitor_id) FROM probe WHERE visitor_id > 0", [], |r| r.get(0))?;
        Ok::<_, anyhow::Error>((rows, visitors, widest))
    })?;
    println!("  after the widening: {rows_now} rows, {visitors} visitors, {widest} positive");

    // 3. Parquet: does the file format care about the width?
    let parquet = PathBuf::from("/tmp/widen.parquet");
    let _ = std::fs::remove_file(&parquet);
    let db = Db::open(&path, "256MB")?;
    let written: anyhow::Result<()> = (|| {
        db.with_read(|conn| {
            conn.execute_batch("INSTALL parquet; LOAD parquet;")?;
            conn.execute_batch(&format!(
                "COPY (SELECT id, website_id, visit_id, visitor_id, user_id, created_at, url FROM probe) \
                 TO '{p}' (FORMAT PARQUET)",
                p = parquet.display()
            ))?;
            Ok::<_, anyhow::Error>(())
        })?;
        Ok(())
    })();
    match written {
        Ok(()) => {
            let size = std::fs::metadata(&parquet).map(|m| m.len()).unwrap_or(0);
            println!("\nthe Parquet file: {:.2} MB", size as f64 / 1_048_576.0);
            let db = Db::open(&path, "256MB")?;
            let read: Result<String, anyhow::Error> = db.with_read(|conn| {
                let mut stmt = conn.prepare(
                    &format!(
                        "SELECT CAST(count(*) AS VARCHAR) || ' rows, ' || \
                         CAST(count(DISTINCT visitor_id) AS VARCHAR) || ' visitors, type ' || \
                         CAST(typeof(visitor_id) AS VARCHAR) \
                         FROM read_parquet('{p}')",
                        p = parquet.display()
                    ),
                )?;
                let mut rows = stmt.query([])?;
                let mut out = String::new();
                while let Some(row) = rows.next()? {
                    out = row.get(0)?;
                }
                Ok(out)
            });
            match read {
                Ok(text) => println!("read back from Parquet: {text}"),
                Err(e) => println!("read back from Parquet: ERROR {e}"),
            }
            // And the same file read as a wider column.
            let wide: Result<String, anyhow::Error> = db.with_read(|conn| {
                let mut stmt = conn.prepare(
                    &format!(
                        "SELECT CAST(count(DISTINCT CAST(visitor_id AS HUGEINT)) AS VARCHAR) \
                         FROM read_parquet('{p}')",
                        p = parquet.display()
                    ),
                )?;
                let mut rows = stmt.query([])?;
                let mut out = String::new();
                while let Some(row) = rows.next()? {
                    out = row.get(0)?;
                }
                Ok(out)
            });
            match wide {
                Ok(text) => println!("the same file read as HUGEINT: {text} distinct"),
                Err(e) => println!("the same file read as HUGEINT: ERROR {e}"),
            }
        }
        Err(e) => println!("\nParquet: ERROR {e}"),
    }

    let _ = std::fs::remove_file(&parquet);
    let _ = std::fs::remove_file(&path);
    Ok(())
}
