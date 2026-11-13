//! How much does the identity column really cost?
//!
//! The id column's compression is RLE — repeated values in a row run cost the
//! value once plus the run length. So its cost depends on **how long a
//! visitor's rows stay adjacent**, and on the width of the value:
//!
//! - **run length 1**: every row a different visitor (the worst case),
//! - **run length 30**: a visitor's burst of thirty events,
//! - **run length 300**: a very busy visitor,
//! - **interleaved**: five sites' traffic in one table, so a visitor's rows
//!   are broken up by other visitors' (what a multi-tenant file really is).
//!
//! Each is measured at 8 bytes and at 16 bytes, and the difference is what
//! the width decision costs.
//!
//! ```bash
//! cargo run --release --example rle_cost -p sumimi-service -- 10000000
//! ```

use std::path::{Path, PathBuf};
use std::time::Instant;

use sumimi_service::pool::Db;

const VISITORS: i64 = 1_000_000;

/// One layout: how a row's visitor is worked out.
#[derive(Clone, Copy)]
struct Layout {
    name: &'static str,
    /// The visitor's number, from the row number.
    visitor: &'static str,
}

fn layouts() -> Vec<Layout> {
    vec![
        Layout {
            name: "run 1 (every row a different visitor)",
            visitor: "i",
        },
        Layout {
            name: "run 30 (a visitor's burst)",
            visitor: "(i / 30)",
        },
        Layout {
            name: "run 300",
            visitor: "(i / 300)",
        },
        Layout {
            name: "interleaved (five sites)",
            visitor: "((i * 7) % 1000000)",
        },
    ]
}

/// The engine's report on the identity column.
fn compression(path: &Path) -> String {
    let db = match Db::open(path, "512MB") {
        Ok(db) => db,
        Err(_) => return "?".to_string(),
    };
    let sql = "SELECT segment_type || '/' || compression, SUM(\"count\") \
               FROM pragma_storage_info('probe') \
               WHERE column_name = 'visitor_id' GROUP BY 1 ORDER BY 1";
    let text: Result<String, anyhow::Error> = db.with_read(|conn| {
        let mut stmt = conn.prepare(sql)?;
        let mut rows = stmt.query([])?;
        let mut out: Vec<String> = Vec::new();
        while let Some(row) = rows.next()? {
            let how: String = row.get(0)?;
            let count: i64 = row.get(1)?;
            out.push(format!("{how} ({count} rows)"));
        }
        Ok(out.join(", "))
    });
    match text {
        Ok(t) if !t.is_empty() => t,
        _ => "?".to_string(),
    }
}

/// Build one layout at one width; return the bytes per row.
fn measure(path: &PathBuf, rows: i64, layout: &Layout, wide: bool) -> anyhow::Result<(f64, String)> {
    let _ = std::fs::remove_file(path);
    let db = Db::open(path, "512MB")?;
    let ddl = if wide { "visitor_id HUGEINT" } else { "visitor_id BIGINT" };
    db.with_read(|conn| {
        conn.execute_batch(&format!(
            "CREATE TABLE probe (id BIGINT, {ddl});"
        ))?;
        Ok::<_, anyhow::Error>(())
    })?;
    // The value: the same for every row of one visitor.
    let value = if wide {
        // The high part stays under 2^40, so the value is about 103 bits: it
        // does not fit in 64 bits, and it cannot overflow a HUGEINT.
        format!(
            "(CAST(hash('a' || ({v})) % 1099511627776 AS HUGEINT) * \
             9223372036854775808::HUGEINT + \
             CAST(hash('b' || ({v})) % 9223372036854775807 AS HUGEINT))",
            v = layout.visitor
        )
    } else {
        format!("CAST(hash('v' || ({v})) % 9223372036854775807 AS BIGINT)", v = layout.visitor)
    };
    let sql = format!(
        "INSERT INTO probe SELECT i AS id, {value} FROM generate_series(1, {rows}) t(i)"
    );
    let started = Instant::now();
    db.with_write(|conn| {
        conn.execute_batch(&sql)?;
        Ok::<_, anyhow::Error>(())
    })?;
    let write = started.elapsed().as_secs_f64() * 1000.0;
    db.with_read(|conn| {
        conn.execute_batch("CHECKPOINT")?;
        Ok::<_, anyhow::Error>(())
    })?;
    drop(db);
    let bytes = std::fs::metadata(path)?.len() as f64 / rows as f64;
    let segs = compression(path);
    let _ = std::fs::remove_file(path);
    Ok((bytes, format!("{segs} (the write {write:.0}ms)")))
}

fn main() -> anyhow::Result<()> {
    let rows: i64 = std::env::args()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000_000);
    println!("{rows} rows, {VISITORS} visitors, the identity column alone\n");
    println!(
        "  {:<42} {:>9} {:>9} {:>11}   the compression",
        "the layout", "8 bytes", "16 bytes", "the difference"
    );
    for layout in layouts() {
        let path = PathBuf::from("/tmp/rle-cost.duckdb");
        let (narrow, segs_narrow) = measure(&path, rows, &layout, false)?;
        let (wide, segs_wide) = measure(&path, rows, &layout, true)?;
        println!(
            "  {:<42} {:>9.2} {:>9.2} {:>8.2} B/row",
            layout.name,
            narrow,
            wide,
            wide - narrow
        );
        println!("      8 bytes:  {segs_narrow}");
        println!("      16 bytes: {segs_wide}");
    }
    Ok(())
}
