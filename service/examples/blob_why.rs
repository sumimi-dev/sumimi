//! Why is a variable-length column slower to search?
//!
//! DuckDB keeps every value of a string column in a **16-byte slot** in the row:
//! four bytes of length, and twelve bytes of the value itself. A value that
//! fits in twelve bytes lives in the row; a longer one leaves a pointer behind
//! and the bytes sit in a heap somewhere else.
//!
//! This measures the column at every length around that boundary — how big it
//! is, how long a plain scan takes, and how long counting the distinct values
//! takes — so the cost can be split into the scan and the counting.
//!
//! ```bash
//! cargo run --release --example blob_why -p sumimi-service -- 2000000
//! ```

use std::path::PathBuf;
use std::time::Instant;

use sumimi_service::pool::Db;

const ROWS: i64 = 2_000_000;

/// A column of `length` **random bytes**, every row a different value.
///
/// `from_hex` turns two hex characters into one byte, so this is really
/// `length` bytes of entropy — not `length` hex characters.
fn values(length: usize) -> String {
    format!("from_hex(substr(sha256(CAST(i AS VARCHAR)), 1, {}))", length * 2)
}

/// A column of short values, with a share of the rows carrying a long one —
/// the shape a real column has: our eight-byte hashes, and the client's id.
fn mixed_values(short: usize, long: usize, permille: i64) -> String {
    format!(
        "CASE WHEN (hash(CAST(i AS VARCHAR)) % 1000) < {permille} THEN CAST(substr(sha256(CAST(i AS VARCHAR)), 1, {long}) AS BLOB) ELSE CAST(substr(sha256(CAST(i AS VARCHAR)), 1, {short}) AS BLOB) END",
        permille = permille,
        long = long,
        short = short,
    )
}


fn measure_mixed(
    short: usize,
    long: usize,
    permille: i64,
) -> anyhow::Result<(f64, f64, f64)> {
    let path = PathBuf::from(format!("/tmp/blob-why-mixed-{short}-{long}.duckdb"));
    let _ = std::fs::remove_file(&path);
    let db = Db::open(&path, "256MB")?;
    db.with_read(|conn| {
        conn.execute_batch("CREATE TABLE probe (id BIGINT, c BLOB);")?;
        Ok::<_, anyhow::Error>(())
    })?;
    db.with_write(|conn| {
        conn.execute_batch(&format!(
            "INSERT INTO probe SELECT i, {} FROM generate_series(1, {ROWS}) t(i)",
            mixed_values(short, long, permille)
        ))?;
        Ok::<_, anyhow::Error>(())
    })?;
    db.with_read(|conn| {
        conn.execute_batch("CHECKPOINT")?;
        Ok::<_, anyhow::Error>(())
    })?;
    drop(db);
    let bytes = std::fs::metadata(&path)?.len();

    let db = Db::open(&path, "256MB")?;
    let mut distinct = f64::MAX;
    for _ in 0..3 {
        let started = Instant::now();
        let _: i64 = db.with_read(|conn| {
            let v: i64 = conn.query_row("SELECT count(DISTINCT c) FROM probe", [], |r| r.get(0))?;
            Ok::<_, anyhow::Error>(v)
        })?;
        distinct = distinct.min(started.elapsed().as_secs_f64() * 1000.0);
    }
    let _ = std::fs::remove_file(&path);
    Ok((bytes as f64 / ROWS as f64, distinct, 0.0))
}

fn measure(length: usize) -> anyhow::Result<(f64, f64, f64, i64)> {
    let path = PathBuf::from(format!("/tmp/blob-why-{length}.duckdb"));
    let _ = std::fs::remove_file(&path);
    let db = Db::open(&path, "256MB")?;
    db.with_read(|conn| {
        conn.execute_batch("CREATE TABLE probe (id BIGINT, c BLOB);")?;
        Ok::<_, anyhow::Error>(())
    })?;
    db.with_write(|conn| {
        conn.execute_batch(&format!(
            "INSERT INTO probe SELECT i, {} FROM generate_series(1, {ROWS}) t(i)",
            values(length)
        ))?;
        Ok::<_, anyhow::Error>(())
    })?;
    db.with_read(|conn| {
        conn.execute_batch("CHECKPOINT")?;
        Ok::<_, anyhow::Error>(())
    })?;
    drop(db);
    let bytes = std::fs::metadata(&path)?.len();

    let db = Db::open(&path, "256MB")?;
    let mut scan = f64::MAX;
    let mut distinct = f64::MAX;
    let mut count = 0i64;
    for _ in 0..3 {
        let started = Instant::now();
        count = db.with_read(|conn| {
            let v: i64 = conn.query_row("SELECT count(*) FROM probe", [], |r| r.get(0))?;
            Ok::<_, anyhow::Error>(v)
        })?;
        scan = scan.min(started.elapsed().as_secs_f64() * 1000.0);

        let started = Instant::now();
        let _: i64 = db.with_read(|conn| {
            let v: i64 = conn.query_row("SELECT count(DISTINCT c) FROM probe", [], |r| r.get(0))?;
            Ok::<_, anyhow::Error>(v)
        })?;
        distinct = distinct.min(started.elapsed().as_secs_f64() * 1000.0);
    }
    let _ = std::fs::remove_file(&path);
    Ok((bytes as f64 / ROWS as f64, scan, distinct, count))
}

/// The fixed-width columns, for the comparison.
fn measure_fixed(kind: &str, ddl: &str, expr: &str) -> anyhow::Result<(f64, f64, f64)> {
    let path = PathBuf::from(format!("/tmp/blob-why-{kind}.duckdb"));
    let _ = std::fs::remove_file(&path);
    let db = Db::open(&path, "256MB")?;
    db.with_read(|conn| {
        conn.execute_batch(&format!("CREATE TABLE probe (id BIGINT, c {ddl});"))?;
        Ok::<_, anyhow::Error>(())
    })?;
    db.with_write(|conn| {
        conn.execute_batch(&format!(
            "INSERT INTO probe SELECT i, {expr} FROM generate_series(1, {ROWS}) t(i)"
        ))?;
        Ok::<_, anyhow::Error>(())
    })?;
    db.with_read(|conn| {
        conn.execute_batch("CHECKPOINT")?;
        Ok::<_, anyhow::Error>(())
    })?;
    drop(db);
    let bytes = std::fs::metadata(&path)?.len();

    let db = Db::open(&path, "256MB")?;
    let mut scan = f64::MAX;
    let mut distinct = f64::MAX;
    for _ in 0..3 {
        let started = Instant::now();
        let _: String = db.with_read(|conn| {
            let v: String =
                conn.query_row("SELECT CAST(max(c) AS VARCHAR) FROM probe", [], |r| r.get(0))?;
            Ok::<_, anyhow::Error>(v)
        })?;
        scan = scan.min(started.elapsed().as_secs_f64() * 1000.0);

        let started = Instant::now();
        let _: i64 = db.with_read(|conn| {
            let v: i64 = conn.query_row("SELECT count(DISTINCT c) FROM probe", [], |r| r.get(0))?;
            Ok::<_, anyhow::Error>(v)
        })?;
        distinct = distinct.min(started.elapsed().as_secs_f64() * 1000.0);
    }
    let _ = std::fs::remove_file(&path);
    Ok((bytes as f64 / ROWS as f64, scan, distinct))
}

fn main() -> anyhow::Result<()> {
    let rows: i64 = std::env::args()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(ROWS);
    println!("{rows} rows — a column of blobs, every length around the twelve-byte boundary\n");
    println!(
        "  {:<26} {:>10} {:>12} {:>12} {:>14}",
        "the column", "B/row", "scan (ms)", "distinct (ms)", "the counting"
    );

    for length in [6usize, 8, 9, 10, 11, 12, 13, 16] {
        let (bytes, scan, distinct, _) = measure(length)?;
        println!(
            "  {:<26} {:>10.2} {:>10.1}ms {:>10.1}ms {:>12.1}ms",
            format!("BLOB, {length} bytes"),
            bytes,
            scan,
            distinct,
            distinct - scan
        );
    }

    println!();
    // The text shape: the same eleven characters, as a VARCHAR.
    for length in [8usize, 11, 12, 13] {
        let path = PathBuf::from(format!("/tmp/blob-why-text-{length}.duckdb"));
        let _ = std::fs::remove_file(&path);
        let db = Db::open(&path, "256MB")?;
        db.with_read(|conn| {
            conn.execute_batch("CREATE TABLE probe (id BIGINT, c VARCHAR);")?;
            Ok::<_, anyhow::Error>(())
        })?;
        db.with_write(|conn| {
            conn.execute_batch(&format!(
                "INSERT INTO probe SELECT i, substr(CAST(base64(unhex(substr(sha256(CAST(i AS VARCHAR)), 1, 16))) AS VARCHAR), 1, {length}) \
                 FROM generate_series(1, {ROWS}) t(i)"
            ))?;
            Ok::<_, anyhow::Error>(())
        })?;
        db.with_read(|conn| {
            conn.execute_batch("CHECKPOINT")?;
            Ok::<_, anyhow::Error>(())
        })?;
        drop(db);
        let bytes = std::fs::metadata(&path)?.len();
        let db = Db::open(&path, "256MB")?;
        let mut distinct = f64::MAX;
        for _ in 0..3 {
            let started = Instant::now();
            let _: i64 = db.with_read(|conn| {
                let v: i64 = conn.query_row("SELECT count(DISTINCT c) FROM probe", [], |r| r.get(0))?;
                Ok::<_, anyhow::Error>(v)
            })?;
            distinct = distinct.min(started.elapsed().as_secs_f64() * 1000.0);
        }
        println!(
            "  {:<26} {:>10.2} {:>10} {:>10.1}ms",
            format!("VARCHAR, {length} chars"),
            bytes as f64 / ROWS as f64,
            "",
            distinct
        );
        let _ = std::fs::remove_file(&path);
    }

    println!();
    // Which compression each column ended up with — and how much of the file
    // each column's data takes.
    for length in [8usize, 12, 13, 16, 32] {
        let path = PathBuf::from(format!("/tmp/blob-why-compression-{length}.duckdb"));
        let _ = std::fs::remove_file(&path);
        let db = Db::open(&path, "256MB")?;
        db.with_read(|conn| {
            conn.execute_batch("CREATE TABLE probe (id BIGINT, c BLOB);")?;
            Ok::<_, anyhow::Error>(())
        })?;
        db.with_write(|conn| {
            conn.execute_batch(&format!(
                "INSERT INTO probe SELECT i, {} FROM generate_series(1, {ROWS}) t(i)",
                values(length)
            ))?;
            Ok::<_, anyhow::Error>(())
        })?;
        db.with_read(|conn| {
            conn.execute_batch("CHECKPOINT")?;
            Ok::<_, anyhow::Error>(())
        })?;
        drop(db);
        let db = Db::open(&path, "256MB")?;
        let stmt = db.with_read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT column_name, compression, count, start, block_id, block_offset \
                 FROM pragma_storage_info('probe') WHERE column_name = 'c'",
            )?;
            let mut rows = stmt.query([])?;
            let mut out = String::new();
            while let Some(row) = rows.next()? {
                let compression: String = row.get(1)?;
                let count: i64 = row.get(2)?;
                let start: i64 = row.get(3)?;
                let block: i64 = row.get(4)?;
                out = format!(
                    "{compression} · {count} segment(s) · rows {start} · block {block}"
                );
            }
            Ok::<_, anyhow::Error>(out)
        })?;
        println!("  {:<26} {}", format!("BLOB, {length} bytes"), stmt);
        let _ = std::fs::remove_file(&path);
    }

    println!();
    for (short, long, permille) in [(8usize, 36usize, 1000i64), (8, 36, 100), (8, 36, 10), (8, 36, 1)] {
        let (bytes, distinct, _) = measure_mixed(short, long, permille)?;
        println!(
            "  {:<26} {:>10.2} {:>10} {:>10.1}ms",
            format!(
                "BLOB, {short} bytes, {share}% are {long}",
                share = permille as f64 / 10.0
            ),
            bytes,
            "",
            distinct
        );
    }

    println!();
    let (bytes, scan, distinct) = measure_fixed(
        "bigint",
        "BIGINT",
        "CAST(hash(CAST(i AS VARCHAR)) % 9223372036854775807 AS BIGINT)",
    )?;
    println!(
        "  {:<26} {:>10.2} {:>10.1}ms {:>10.1}ms {:>12.1}ms",
        "BIGINT, 8 bytes (fixed)",
        bytes,
        scan,
        distinct,
        distinct - scan
    );
    let (bytes, scan, distinct) = measure_fixed(
        "hugeint",
        "HUGEINT",
        "CAST(hash(CAST(i AS VARCHAR)) % 9223372036854775807 AS HUGEINT)",
    )?;
    println!(
        "  {:<26} {:>10.2} {:>10.1}ms {:>10.1}ms {:>12.1}ms",
        "HUGEINT, 16 bytes (fixed)",
        bytes,
        scan,
        distinct,
        distinct - scan
    );
    let (bytes, scan, distinct) = measure_fixed(
        "uuid",
        "UUID",
        "CAST(substr(sha256(CAST(i AS VARCHAR)), 1, 8) || '-' || \
             substr(sha256(CAST(i AS VARCHAR)), 9, 4) || '-' || \
             substr(sha256(CAST(i AS VARCHAR)), 13, 4) || '-' || \
             substr(sha256(CAST(i AS VARCHAR)), 17, 4) || '-' || \
             substr(sha256(CAST(i AS VARCHAR)), 21, 12) AS UUID)",
    )?;
    println!(
        "  {:<26} {:>10.2} {:>10.1}ms {:>10.1}ms {:>12.1}ms",
        "UUID, 16 bytes (fixed)",
        bytes,
        scan,
        distinct,
        distinct - scan
    );
    Ok(())
}
