//! When does DuckDB pick RLE for an id column?
//!
//! #66 measured the id columns as **RLE** ("repeated values in a row run cost
//! the value once plus the run length") and concluded the width costs
//! "the width × the number of runs". On DuckDB v2.0.0 that did not reproduce:
//! with the same rows — long runs of one visitor — the engine picked
//! `Uncompressed`, and the column cost its full width.
//!
//! This probes when RLE (or Constant) does get picked: the type of the
//! column, and the length of the runs.
//!
//! ```bash
//! cargo run --release --example rle_probe -p sumimi-service -- 10000000
//! ```

use std::path::PathBuf;

use sumimi_service::pool::Db;

/// One case: a type, a run length, and how the value is made.
struct Case {
    name: &'static str,
    ddl: &'static str,
    value: String,
}

fn cases() -> Vec<Case> {
    vec![
        Case {
            name: "BIGINT, every row different",
            ddl: "v BIGINT",
            value: "CAST(hash('v' || i) % 9223372036854775807 AS BIGINT)".to_string(),
        },
        Case {
            name: "BIGINT, runs of 30",
            ddl: "v BIGINT",
            value: "CAST(hash('v' || (i / 30)) % 9223372036854775807 AS BIGINT)".to_string(),
        },
        Case {
            name: "BIGINT, runs of 3000",
            ddl: "v BIGINT",
            value: "CAST(hash('v' || (i / 3000)) % 9223372036854775807 AS BIGINT)".to_string(),
        },
        Case {
            name: "BIGINT, one value for everything",
            ddl: "v BIGINT",
            value: "42".to_string(),
        },
        Case {
            name: "UUID, runs of 3000",
            ddl: "v UUID",
            value: "CAST(substr(sha256('v' || (i / 3000)), 1, 8) || '-' || \
                 substr(sha256('v' || (i / 3000)), 9, 4) || '-' || \
                 substr(sha256('v' || (i / 3000)), 13, 4) || '-' || \
                 substr(sha256('v' || (i / 3000)), 17, 4) || '-' || \
                 substr(sha256('v' || (i / 3000)), 21, 12) AS UUID)".to_string(),
        },
        Case {
            name: "VARCHAR, runs of 3000",
            ddl: "v VARCHAR",
            value: "'visitor-' || (i / 3000)".to_string(),
        },
    ]
}

fn main() -> anyhow::Result<()> {
    let rows: i64 = std::env::args()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000_000);
    println!("{rows} rows, one column\n");
    for (index, case) in cases().into_iter().enumerate() {
        let forced = index == 2 || index == 4;
        let path = PathBuf::from("/tmp/rle-probe.duckdb");
        let _ = std::fs::remove_file(&path);
        let db = Db::open(&path, "512MB")?;
        db.with_read(|conn| {
            conn.execute_batch(&format!("CREATE TABLE probe ({ddl});", ddl = case.ddl))?;
            Ok::<_, anyhow::Error>(())
        })?;
        db.with_write(|conn| {
            conn.execute_batch(&format!(
                "INSERT INTO probe SELECT {value} FROM generate_series(1, {rows}) t(i)",
                value = case.value
            ))?;
            Ok::<_, anyhow::Error>(())
        })?;
        // Try forcing RLE on the same data: what would it cost if the
        // engine picked it?
        if forced {
            let _ = db.with_read(|conn| {
                conn.execute_batch("SET force_compression = 'rle'")?;
                Ok::<_, anyhow::Error>(())
            });
            let forced_path = PathBuf::from("/tmp/rle-probe-forced.duckdb");
            let _ = std::fs::remove_file(&forced_path);
            let db2 = Db::open(&forced_path, "512MB")?;
            db2.with_read(|conn| {
                conn.execute_batch(&format!("CREATE TABLE probe ({ddl});", ddl = case.ddl))?;
                Ok::<_, anyhow::Error>(())
            })?;
            db2.with_write(|conn| {
                conn.execute_batch("SET force_compression = 'rle'")?;
                conn.execute_batch(&format!(
                    "INSERT INTO probe SELECT {value} FROM generate_series(1, {rows}) t(i)",
                    value = case.value
                ))?;
                Ok::<_, anyhow::Error>(())
            })?;
            db2.with_read(|conn| {
                conn.execute_batch("CHECKPOINT")?;
                Ok::<_, anyhow::Error>(())
            })?;
            let forced_bytes = std::fs::metadata(&forced_path)?.len() as f64 / rows as f64;
            let forced_segs: String = db2.with_read(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT segment_type || '/' || compression FROM pragma_storage_info('probe') \
                     WHERE column_name = 'v' GROUP BY 1 ORDER BY 1",
                )?;
                let mut rows = stmt.query([])?;
                let mut out: Vec<String> = Vec::new();
                while let Some(row) = rows.next()? {
                    out.push(row.get(0)?);
                }
                Ok(out.join(", "))
            })?;
            println!("      forced RLE: {forced_bytes:.2} B/row   {forced_segs}");
            drop(db2);
            let _ = std::fs::remove_file(&forced_path);
        }
        db.with_read(|conn| {
            conn.execute_batch("CHECKPOINT")?;
            Ok::<_, anyhow::Error>(())
        })?;
        drop(db);
        let bytes = std::fs::metadata(&path)?.len() as f64 / rows as f64;

        let db = Db::open(&path, "512MB")?;
        let segs: String = db.with_read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT segment_type || '/' || compression FROM pragma_storage_info('probe') \
                 WHERE column_name = 'v' GROUP BY 1 ORDER BY 1",
            )?;
            let mut rows = stmt.query([])?;
            let mut out: Vec<String> = Vec::new();
            while let Some(row) = rows.next()? {
                out.push(row.get(0)?);
            }
            Ok(out.join(", "))
        })?;
        println!("  {:<36} {:>7.2} B/row   {segs}", case.name, bytes);
        let _ = std::fs::remove_file(&path);
    }
    Ok(())
}
