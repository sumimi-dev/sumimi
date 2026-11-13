//! What does one column cost, once it is compressed?
//!
//! The matrix compares whole files. This one measures **one column at a time**:
//! a table with the column and nothing else, so the file's size minus the
//! baseline is what that column really costs — the id columns against the rest
//! of the row, and the shapes the identity can take against each other.
//!
//! ```bash
//! cargo run --release --example column_cost -p sumimi-service -- 2000000
//! ```

use std::path::PathBuf;
use std::time::Instant;

use sumimi_service::pool::Db;

const ROWS: i64 = 2_000_000;
const VISITORS: i64 = 120_000;

/// The visitor of a row.
fn visitor() -> String {
    format!("(i % {VISITORS})")
}

/// The segment a row belongs to — a ten-minute window shared by everybody.
fn segment() -> String {
    format!(
        "(1600000000 + ((i // {VISITORS}) // 4) * 600 + ({v} % 3600))",
        v = visitor()
    )
}

/// The low 32 bits of the visitor's hash.
fn low32() -> String {
    format!("(hash('v' || {}) % 4294967296)", visitor())
}

/// The visitor's hash, as hex.
fn hash16() -> String {
    format!("sha256('v' || {})", visitor())
}

/// The columns: the name, the type, and the expression that fills it.
fn columns() -> Vec<(String, String, String)> {
    let uuid = format!(
        "CAST(substr({h}, 1, 8) || '-' || substr({h}, 9, 4) || '-' || substr({h}, 13, 4) || \
         '-' || substr({h}, 17, 4) || '-' || substr({h}, 21, 12) AS UUID)",
        h = hash16()
    );
    let huge = format!(
        "CAST(hash('v' || {v}) % 9223372036854775807 AS HUGEINT) * 18446744073709551616::HUGEINT \
         + CAST(hash('w' || {v}) % 9223372036854775807 AS HUGEINT)",
        v = visitor()
    );
    vec![
        // The session id, in the shapes it can take.
        (
            "visit_id (packed)".to_string(),
            "BIGINT".to_string(),
            format!("(({})::BIGINT << 32) | {}", segment(), low32()),
        ),
        (
            "visit_id (dense counter)".to_string(),
            "BIGINT".to_string(),
            "(i / 4)".to_string(),
        ),
        (
            "segment_start alone".to_string(),
            "BIGINT".to_string(),
            segment(),
        ),
        (
            "visitor low 32 alone".to_string(),
            "BIGINT".to_string(),
            low32(),
        ),
        // The identity, in the shapes it can take.
        ("visitor_id UUID".to_string(), "UUID".to_string(), uuid),
        (
            "visitor_id HUGEINT".to_string(),
            "HUGEINT".to_string(),
            huge,
        ),
        (
            "visitor_id BLOB (16 bytes)".to_string(),
            "BLOB".to_string(),
            format!("unhex(substr({}, 1, 32))", hash16()),
        ),
        (
            "visitor_id BIGINT (8 bytes)".to_string(),
            "BIGINT".to_string(),
            format!("CAST(hash('v' || {}) % 9223372036854775807 AS BIGINT)", visitor()),
        ),
        // What else is in the row, for the comparison.
        // The owner's idea: the column is sixteen bytes wide, and today only
        // eight of them carry anything — the other eight are zero.
        (
            "visitor_id UUID (8 bytes + 8 zeros)".to_string(),
            "UUID".to_string(),
            format!(
                "CAST(substr({h}, 1, 8) || '-0000-0000-0000-000000000000' AS UUID)",
                h = hash16()
            ),
        ),
        (
            "visitor_id BLOB (8 bytes)".to_string(),
            "BLOB".to_string(),
            format!("unhex(substr({}, 1, 16))", hash16()),
        ),
        (
            "user_id (mostly null)".to_string(),
            "VARCHAR".to_string(),
            format!("CASE WHEN i % 3 = 0 THEN NULL ELSE 'user-{}' END", visitor()),
        ),
        (
            "ip_prefix".to_string(),
            "VARCHAR".to_string(),
            format!("('203.0.113.' || CAST({} % 40000 AS VARCHAR))", visitor()),
        ),
        (
            "user_agent".to_string(),
            "VARCHAR".to_string(),
            format!(
                "CASE {v} % 4 WHEN 0 THEN 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36' \
                 WHEN 1 THEN 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36' \
                 WHEN 2 THEN 'Mozilla/5.0 (iPhone; CPU iPhone OS 17_2 like Mac OS X) AppleWebKit/605.1.15' \
                 ELSE 'Mozilla/5.0 (X11; Linux x86_64; rv:121.0) Gecko/20100101 Firefox/121.0' END",
                v = visitor()
            ),
        ),
        (
            "user_agent (5000 kinds)".to_string(),
            "VARCHAR".to_string(),
            format!(
                "('Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_' || {v} || ') AppleWebKit/537.36 Safari/537.36')",
                v = visitor()
            ),
        ),
        (
            "url".to_string(),
            "VARCHAR".to_string(),
            "'/blog/post-' || (i % 5000)".to_string(),
        ),
        (
            "title".to_string(),
            "VARCHAR".to_string(),
            "'A reasonably long article title number ' || (i % 1000)".to_string(),
        ),
        (
            "country".to_string(),
            "VARCHAR".to_string(),
            format!(
                "CASE {v} % 8 WHEN 0 THEN 'DE' WHEN 1 THEN 'US' WHEN 2 THEN 'FR' WHEN 3 THEN 'GB' \
                 ELSE 'JP' END",
                v = visitor()
            ),
        ),
        (
            "created_at".to_string(),
            "TIMESTAMP".to_string(),
            "TIMESTAMP '2026-01-01 00:00:00' + INTERVAL (i * 4) SECOND".to_string(),
        ),
    ]
}

/// Build a table with one column, and say how many bytes the file took.
fn measure(rows: i64, ddl: &str, values: &str) -> anyhow::Result<(f64, f64, i64)> {
    let path = PathBuf::from(format!(
        "/tmp/column-cost-{}.duckdb",
        ddl.replace(' ', "_").replace(['(', ')'], "")
    ));
    let _ = std::fs::remove_file(&path);
    let db = Db::open(&path, "256MB")?;
    db.with_read(|conn| {
        conn.execute_batch(&format!("CREATE TABLE probe (id BIGINT, c {ddl});"))?;
        Ok::<_, anyhow::Error>(())
    })?;
    db.with_write(|conn| {
        conn.execute_batch(&format!(
            "INSERT INTO probe SELECT i, {values} FROM generate_series(1, {rows}) t(i)"
        ))?;
        Ok::<_, anyhow::Error>(())
    })?;
    db.with_read(|conn| {
        conn.execute_batch("CHECKPOINT")?;
        Ok::<_, anyhow::Error>(())
    })?;
    drop(db);

    let bytes = std::fs::metadata(&path)?.len();

    // How long the visitor count takes over this column alone.
    let db = Db::open(&path, "256MB")?;
    let mut best = f64::MAX;
    let mut distinct = 0i64;
    for _ in 0..3 {
        let started = Instant::now();
        distinct = db.with_read(|conn| {
            let value: i64 = conn.query_row("SELECT COUNT(DISTINCT c) FROM probe", [], |r| r.get(0))?;
            Ok::<_, anyhow::Error>(value)
        })?;
        best = best.min(started.elapsed().as_secs_f64() * 1000.0);
    }
    let _ = std::fs::remove_file(&path);
    Ok((bytes as f64 / rows as f64, best, distinct))
}

fn main() -> anyhow::Result<()> {
    let rows: i64 = std::env::args()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(ROWS);
    println!("{rows} rows — one column at a time (the file holds the column and nothing else)\n");

    // The baseline: the row id alone.
    let (baseline, _, _) = measure(rows, "BIGINT", "i")?;
    println!("  the baseline (the row id alone): {baseline:.2} B/row\n");

    println!(
        "  {:<28} {:>10} {:>12} {:>12} {:>14}",
        "the column", "B/row", "the id", "the column", "distinct (ms)"
    );
    for (name, ddl, values) in columns() {
        let (bytes, ms, distinct) = measure(rows, &ddl, &values)?;
        println!(
            "  {:<28} {:>10.2} {:>12.2} {:>10.2} B {:>10.1}ms  ({distinct})",
            name,
            bytes,
            baseline,
            bytes - baseline,
            ms
        );
    }
    Ok(())
}
