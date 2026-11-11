//! How much does the timestamp's precision cost?
//!
//! Two questions, one probe:
//!
//! 1. **Microseconds, milliseconds or seconds?** The event row's `created_at` is
//!    stored in **microseconds** (DuckDB `TIMESTAMP`, SQLite `INTEGER` of unix
//!    microseconds). The **window start** — the number that goes into the visit
//!    id — could be any of the three. The same rows are written four times,
//!    once per unit, and the file sizes are compared.
//!
//! 2. **What does that precision cost today?** The same rows are written with
//!    one column family dropped each time, and the file sizes are compared:
//!    what the timestamp costs, what the `DOUBLE`s cost (eight bytes each, for
//!    numbers nobody needs at that precision), what the ids cost.
//!
//! ```bash
//! cargo run --release --example precision_cost -p sumimi-service -- synthetic 20000000
//! cargo run --release --example precision_cost -p sumimi-service -- real ~/.cache/sumimi-scratch/big/data.duckdb
//! ```

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use duckdb::Connection;

/// The variants of the same data, one per unit.
const UNITS: [(&str, &str); 4] = [
    ("TIMESTAMP (us)", "make_timestamp(us)"),
    ("BIGINT us", "us"),
    ("BIGINT ms", "us // 1000"),
    ("BIGINT s", "us // 1000000"),
];

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("all");

    if mode == "synthetic" || mode == "all" {
        let rows: i64 = args.get(2).and_then(|a| a.parse().ok()).unwrap_or(20_000_000);
        synthetic(rows)?;
    }

    if mode == "real" || mode == "all" {
        let path = args.get(2).map(PathBuf::from).unwrap_or_else(|| {
            PathBuf::from(std::env::var("HOME").unwrap_or_default())
                .join(".cache/sumimi-scratch/big/data.duckdb")
        });
        real(&path)?;
    }

    Ok(())
}

/// The same rows, written once per unit. The difference between the file sizes
/// is what the precision costs.
fn synthetic(rows: i64) -> Result<()> {
    let dir = std::env::temp_dir().join("sumimi-precision-cost");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;

    println!("== the same {rows} rows, four units ==\n");
    println!("{:<18} {:>12} {:>12}", "unit", "file", "bytes/row");

    for (name, expr) in UNITS {
        let path = dir.join(format!("{}.duckdb", name.replace([' ', '(', ')'], "_")));
        {
            let conn = Connection::open(&path)?;
            // The timestamps are spread over three years and hashed, so they are
            // as unordered as real traffic — a sorted column would compress far
            // better, and would make the answer a lie.
            conn.execute_batch(&format!(
                "CREATE TABLE t AS
                 SELECT i AS id, {expr} AS created_at
                 FROM (
                     SELECT i,
                            1704067200000000::BIGINT
                              + (hash(i) % 94608000000000)::BIGINT AS us
                     FROM range({rows}) t(i)
                 );"
            ))?;
            let n: i64 = conn.query_row("SELECT count(*) FROM t", [], |r| r.get(0))?;
            assert_eq!(n, rows, "the generator did not make {rows} rows");
        }
        let size = std::fs::metadata(&path)?.len();
        println!(
            "{name:<18} {:>9.2} MB {:>12.3}",
            size as f64 / 1_048_576.0,
            size as f64 / rows as f64
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// What every column family costs on disk, measured by writing the real rows
/// without it and comparing the file sizes.
fn real(path: &Path) -> Result<()> {
    if !path.exists() {
        return Err(anyhow!("{} does not exist", path.display()));
    }
    println!("== {} ==", path.display());
    println!(
        "file: {:.2} MB",
        std::fs::metadata(path)?.len() as f64 / 1_048_576.0
    );

    let config = duckdb::Config::default().access_mode(duckdb::AccessMode::ReadOnly)?;
    let src = Connection::open_with_flags(path, config)?;

    let mut stmt = src.prepare(
        "SELECT column_name, data_type FROM information_schema.columns
         WHERE table_name = 'event' ORDER BY ordinal_position",
    )?;
    let columns: Vec<(String, String)> = stmt
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let names: Vec<String> = columns.iter().map(|(n, _)| n.clone()).collect();

    let rows: i64 = src.query_row("SELECT count(*) FROM event", [], |r| r.get(0))?;
    println!("event: {rows} rows, {} columns\n", names.len());

    // The families worth pricing.
    let doubles: Vec<&str> = ["latitude", "longitude", "lcp", "inp", "cls", "fcp", "ttfb"]
        .into_iter()
        .filter(|c| names.iter().any(|n| n == c))
        .collect();
    let ids: Vec<&str> = ["session_id", "visit_id", "visitor_id", "user_id"]
        .into_iter()
        .filter(|c| names.iter().any(|n| n == c))
        .collect();
    let page: Vec<&str> = ["url", "url_query", "title"]
        .into_iter()
        .filter(|c| names.iter().any(|n| n == c))
        .collect();

    let mut variants: Vec<(String, Vec<&str>)> = vec![("everything".to_string(), Vec::new())];
    if names.iter().any(|n| n == "created_at") {
        variants.push(("no created_at".to_string(), vec!["created_at"]));
    }
    if !doubles.is_empty() {
        variants.push(("no DOUBLEs".to_string(), doubles.clone()));
    }
    if !ids.is_empty() {
        variants.push(("no ids".to_string(), ids.clone()));
    }
    if !page.is_empty() {
        variants.push(("no page".to_string(), page.clone()));
    }

    let dir = std::env::temp_dir().join("sumimi-precision-real");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;

    println!(
        "{:<16} {:>12} {:>12} {:>12}",
        "variant", "file", "bytes/row", "saved/row"
    );
    let mut baseline: Option<u64> = None;
    for (name, drop) in &variants {
        let out = dir.join("t.duckdb");
        let _ = std::fs::remove_file(&out);
        let escaped = path.to_string_lossy().replace('\'', "''");
        {
            let dest = Connection::open(&out)?;
            dest.execute_batch(&format!("ATTACH '{escaped}' AS src (READ_ONLY);"))?;
            let selection = if drop.is_empty() {
                "*".to_string()
            } else {
                format!("* EXCLUDE ({})", drop.join(", "))
            };
            dest.execute_batch(&format!(
                "CREATE TABLE t AS SELECT {selection} FROM src.event;"
            ))?;
        }
        let size = std::fs::metadata(&out)?.len();
        let base = *baseline.get_or_insert(size);
        println!(
            "{name:<16} {:>9.2} MB {:>12.3} {:>12.3}",
            size as f64 / 1_048_576.0,
            size as f64 / rows as f64,
            (base - size) as f64 / rows as f64
        );
        let _ = std::fs::remove_file(&out);
    }

    let _ = std::fs::remove_dir_all(&dir);
    println!("\ncolumns: {columns:?}");
    Ok(())
}
