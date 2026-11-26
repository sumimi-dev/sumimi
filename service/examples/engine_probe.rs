//! **Why is the combined distinct count cheap on a blob and dear on a uuid?**
//!
//! The scale harness: on a blob the dashboard's summary query answered in
//! 22 ms; on a uuid it took 980 ms (and ran out of memory at 20 M rows). But
//! a probe with *every* row its own value said the opposite — the blob was
//! ten times slower. So the variable is not the type alone. Find it.
//!
//! Two candidates:
//!
//! - **the cardinality** — how many distinct values the column holds
//!   (the harness: 4 % of the rows; the probe: 100 %);
//! - **the dictionary** — a blob column is dictionary encoded, and the
//!   dictionary's size is what the blob's behaviour follows.
//!
//! And one thing worth trying: a uuid column *forced* into dictionary
//! compression — if that takes the cheap path, the format can stay a uuid.
//!
//! ```bash
//! cargo run --release --example engine_probe -p sumimi-service
//! ```

use duckdb::Connection;
use std::time::Instant;

fn time(conn: &Connection, sql: &str) -> anyhow::Result<Option<f64>> {
    let mut stmt = conn.prepare(sql)?;
    let mut best = f64::MAX;
    for _ in 0..3 {
        let started = Instant::now();
        let outcome = (|| -> anyhow::Result<f64> {
            let mut rows = stmt.query([])?;
            while rows.next()?.is_some() {}
            Ok(started.elapsed().as_secs_f64() * 1000.0)
        })();
        match outcome {
            Ok(ms) => best = best.min(ms),
            Err(e) => {
                let m = e.to_string();
                if m.contains("Out of Memory") {
                    return Ok(None);
                }
                return Err(e);
            }
        }
    }
    Ok(Some(best))
}

fn main() -> anyhow::Result<()> {
    let dir = std::env::temp_dir().join("sumimi-length-probe");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;

    let rows: i64 = 5_000_000;
    let distinct: i64 = 200_000;

    println!("== {rows} rows, {distinct} distinct values, memory_limit=256MB ==");
    println!(
        "  {:>10} {:>12} {:>14} {:>14}",
        "the carrier", "one distinct", "two distincts", "the engine's word"
    );

    for (label, column, column2) in [
        ("uuid (16B)", "u", "u2"),
        ("blob 16B", "b16", "b16_2"),
        ("blob 12B", "b12", "b12_2"),
        ("blob 8B", "b8", "b8_2"),
    ] {
        let path = dir.join("t.duckdb");
        let _ = std::fs::remove_file(&path);
        let conn = Connection::open(&path)?;
        conn.execute_batch(
            "CREATE TABLE t (u UUID, b16 BLOB, b12 BLOB, b8 BLOB);",
        )?;
        let h = "sha256(CAST(i % 200000 AS VARCHAR))";
        let h2 = "sha256(CAST((i % 200000) + 1 AS VARCHAR))";
        let uuid_of = |x: &str| {
            format!(
                "CAST(substr({x},1,8)||'-'||substr({x},9,4)||'-'||substr({x},13,4)||'-'||substr({x},17,4)||'-'||substr({x},21,12) AS UUID)"
            )
        };
        conn.execute_batch(&format!(
            "INSERT INTO t
             SELECT {u}, from_hex(substr({h},1,32)), from_hex(substr({h},1,24)), from_hex(substr({h},1,16))
             FROM range(0, {rows}) t(i);",
            u = uuid_of(h),
        ))?;
        // the second column of each kind, so "two distincts" is the same shape
        conn.execute_batch(&format!(
            "CREATE TABLE t2 AS SELECT * FROM t LIMIT 0;"
        ))?;
        drop(conn);
        let _ = std::fs::remove_file(&path);

        // build it again with the second column
        let conn = Connection::open(&path)?;
        conn.execute_batch(
            "CREATE TABLE t (u UUID, b16 BLOB, b12 BLOB, b8 BLOB, u2 UUID, b16_2 BLOB, b12_2 BLOB, b8_2 BLOB);",
        )?;
        conn.execute_batch(&format!(
            "INSERT INTO t
             SELECT {u}, from_hex(substr({h},1,32)), from_hex(substr({h},1,24)), from_hex(substr({h},1,16)),
                    {u2}, from_hex(substr({h2},1,32)), from_hex(substr({h2},1,24)), from_hex(substr({h2},1,16))
             FROM range(0, {rows}) t(i);",
            u = uuid_of(h),
            u2 = uuid_of(h2),
        ))?;
        conn.execute_batch("CHECKPOINT;")?;
        let bytes = std::fs::metadata(&path)?.len() as f64 / rows as f64;
        let _ = conn.execute_batch("SET memory_limit='256MB'");

        let one = time(&conn, &format!("SELECT count(DISTINCT {column}) FROM t"))?;
        let two = time(
            &conn,
            &format!("SELECT count(DISTINCT {column}), count(DISTINCT {column2}) FROM t"),
        )?;
        let mut stmt = conn.prepare(&format!(
            "SELECT DISTINCT compression FROM pragma_storage_info('t') WHERE column_name = '{column}' AND segment_type != 'VALIDITY'"
        ))?;
        let mut rows_out = stmt.query([])?;
        let mut how = String::new();
        while let Some(row) = rows_out.next()? {
            how = row.get(0)?;
        }
        let show = |v: Option<f64>| match v {
            Some(ms) => format!("{ms:.1}"),
            None => "oom".to_string(),
        };
        println!(
            "  {label:<10} {one:>12} {two:>14}   {how:<12} ({bytes:.2} B/row)",
            one = show(one),
            two = show(two)
        );
        drop(conn);
        let _ = std::fs::remove_file(&path);
    }
    Ok(())
}
