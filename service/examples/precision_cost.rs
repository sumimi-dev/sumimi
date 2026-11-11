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

/// One way of writing the same rows.
struct Variant {
    /// What the variant is called.
    name: &'static str,
    /// The `SELECT` that builds the table out of `src.event`.
    select: String,
    /// The expression that identifies a session — `None` when the variant has
    /// no session key (it dropped the column).
    key: Option<String>,
    /// The expression that buckets a day out of `created_at`.
    bucket: &'static str,
    /// Time the report queries on this variant?
    time: bool,
}

/// The variants of the same data, one per unit.
const UNITS: [(&str, &str); 4] = [
    ("TIMESTAMP (us)", "make_timestamp(us)"),
    ("BIGINT us", "us"),
    ("BIGINT ms", "us // 1000"),
    ("BIGINT s", "us // 1000000"),
];


/// The id the report works out for itself when the pair is stored instead of
/// an id: a 128-bit hash of the visitor and the window start, as a UUID.
const MD5_KEY: &str = "CAST(substr(md5(visitor_id::VARCHAR || '-' || window_start::VARCHAR), 1, 8) \
 || '-' || substr(md5(visitor_id::VARCHAR || '-' || window_start::VARCHAR), 9, 4) \
 || '-' || substr(md5(visitor_id::VARCHAR || '-' || window_start::VARCHAR), 13, 4) \
 || '-' || substr(md5(visitor_id::VARCHAR || '-' || window_start::VARCHAR), 17, 4) \
 || '-' || substr(md5(visitor_id::VARCHAR || '-' || window_start::VARCHAR), 21, 12) AS UUID)";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("all");

    if mode == "synthetic" || mode == "all" {
        let rows: i64 = args.get(2).and_then(|a| a.parse().ok()).unwrap_or(20_000_000);
        synthetic(rows)?;
    }

    if mode == "compute" || mode == "all" {
        let rows: i64 = args.get(2).and_then(|a| a.parse().ok()).unwrap_or(20_000_000);
        compute(rows)?;
    }

    if mode == "real" || mode == "all" {
        let path = args.get(2).map(PathBuf::from).unwrap_or_else(|| {
            PathBuf::from(std::env::var("HOME").unwrap_or_default())
                .join(".cache/sumimi-scratch/big/data.duckdb")
        });
        real(&path)?;
    }

    if mode == "variants" || mode == "all" {
        let path = args.get(2).map(PathBuf::from).unwrap_or_else(|| {
            PathBuf::from(std::env::var("HOME").unwrap_or_default())
                .join(".cache/sumimi-scratch/big/data.duckdb")
        });
        variants(&path)?;
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

/// What each column costs on its own, and what the **alternatives** cost.
///
/// The families are priced together in [`real`]; here every id is dropped on
/// its own, and the two designs that would replace the stored session id are
/// written out and measured:
///
/// * **derived key** — the session id is not stored; the row keeps the *window
///   start* (one value per session) and the session key is the pair
///   (visitor, window start), worked out when the report runs.
/// * **8-byte id** — the session id is stored, but as 64 bits instead of 128.
///
/// The window start is stood in for by `hash(visit_id)`: one value per session,
/// constant across that session's rows — the same cardinality structure the real
/// column would have. It is *not* monotonic like a real window start, so the
/// measurement is the pessimistic one.
fn variants(path: &Path) -> Result<()> {
    if !path.exists() {
        return Err(anyhow!("{} does not exist", path.display()));
    }

    let config = duckdb::Config::default().access_mode(duckdb::AccessMode::ReadOnly)?;
    let src = Connection::open_with_flags(path, config)?;
    let rows: i64 = src.query_row("SELECT count(*) FROM event", [], |r| r.get(0))?;

    // Which columns does this file actually have? The scratch dataset is older
    // than the current schema: it still has the v6 `session_id` (the session)
    // next to the v6 `visit_id` (an hour-long window inside it), and it has no
    // `user_id`. So the column the session id lives in is worked out here
    // instead of being assumed.
    let mut stmt = src.prepare(
        "SELECT column_name FROM information_schema.columns
         WHERE table_name = 'event' AND table_schema = 'main'",
    )?;
    let names: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let has = |name: &str| names.iter().any(|n| n == name);
    let session_col = if has("session_id") {
        "session_id"
    } else if has("visit_id") {
        "visit_id"
    } else {
        return Err(anyhow!("the event table has no session id column"));
    };
    println!(
        "session id column: {session_col}; columns: {}",
        names.len()
    );
    println!();

    // Which way does DuckDB let a **pair** be counted as one value? The
    // composite key has to be exact: a hash of it would collide, and a
    // session count that is off by one in a million is still wrong.
    println!("== how a pair can be counted as one value ==");
    let probe = Connection::open_in_memory()?;
    for sql in [
        "SELECT count(DISTINCT (a, b)) FROM (SELECT 1 a, 2 b UNION ALL SELECT 1, 3) t",
        "SELECT count(DISTINCT struct_pack(a := a, b := b)) FROM (SELECT 1 a, 2 b UNION ALL SELECT 1, 3) t",
        "SELECT count(DISTINCT a || '-' || b) FROM (SELECT 1 a, 2 b UNION ALL SELECT 1, 3) t",
        "SELECT count(DISTINCT hash(a, b)) FROM (SELECT 1 a, 2 b UNION ALL SELECT 1, 3) t",
    ] {
        match probe.query_row(sql, [], |r| r.get::<_, i64>(0)) {
            Ok(n) => println!("  ok  {n}  {sql}"),
            Err(e) => println!("  no      {sql}\n           {e}"),
        }
    }
    println!();

    let mut variants = vec![
        Variant {
            name: "everything",
            select: "*".to_string(),
            key: Some(session_col.to_string()),
            bucket: "created_at",
            time: true,
        },
        Variant {
            name: "8-byte id only",
            select: format!("* EXCLUDE ({session_col}), hash({session_col})::UBIGINT AS visit_id"),
            key: Some("visit_id".to_string()),
            bucket: "created_at",
            time: true,
        },
        Variant {
            // The pair, kept whole: the visitor id and the second the session
            // started, as one string. No hash — the id says what it is.
            name: "visitor+second",
            select: format!(
                "* EXCLUDE ({session_col}), visitor_id::VARCHAR || '-' || \
                          hash({session_col})::VARCHAR AS visit_id"
            ),
            key: Some("visit_id".to_string()),
            bucket: "created_at",
            time: true,
        },
        Variant {
            // The pair, not stored as an id: the report works the id out
            // itself, with a 128-bit hash (the same width as today's UUID).
            name: "pair + md5 key",
            select: format!(
                "* EXCLUDE ({session_col}), hash({session_col})::UBIGINT AS window_start"
            ),
            key: Some(MD5_KEY.to_string()),
            bucket: "created_at",
            time: true,
        },
        Variant {
            // The same pair, with a 64-bit hash as the key: the cheapest
            // aggregate, but a hash that can collide.
            name: "pair + hash key",
            select: format!(
                "* EXCLUDE ({session_col}), hash({session_col})::UBIGINT AS window_start"
            ),
            key: Some("hash(visitor_id, window_start)".to_string()),
            bucket: "created_at",
            time: true,
        },
        Variant {
            name: "derived key",
            select: format!(
                "* EXCLUDE ({session_col}), hash({session_col})::UBIGINT AS window_start"
            ),
            key: Some("(visitor_id, window_start)".to_string()),
            bucket: "created_at",
            time: true,
        },
    ];

    let dir = std::env::temp_dir().join("sumimi-precision-variants");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let escaped = path.to_string_lossy().replace('\'', "''");

    println!("== {rows} rows ==\n");
    println!("{:<18} {:>12} {:>12} {:>12}", "variant", "file", "bytes/row", "saved/row");
    let mut baseline: Option<u64> = None;
    for variant in &variants {
        let out = dir.join("t.duckdb");
        let _ = std::fs::remove_file(&out);
        let mut times: Vec<(String, f64)> = Vec::new();
        let mut notes: Vec<String> = Vec::new();
        {
            let dest = Connection::open(&out)?;
            dest.execute_batch(&format!("ATTACH '{escaped}' AS src (READ_ONLY);"))?;
            dest.execute_batch(&format!("CREATE TABLE t AS SELECT {} FROM src.event;", variant.select))?;
            if variant.time {
                if let Some(key) = &variant.key {
                    let summary = format!(
                        "SELECT COUNT(*), COUNT(DISTINCT visitor_id), COUNT(DISTINCT {key}) FROM t"
                    );
                    let group = format!(
                        "SELECT COUNT(*) FROM (SELECT {key} k, COUNT(*) n FROM t GROUP BY 1)"
                    );
                    let buckets = format!(
                        "SELECT date_trunc('day', {}) d, COUNT(*) FROM t GROUP BY 1",
                        variant.bucket
                    );
                    times.push(("summary".to_string(), time_query_n(&dest, &summary, 2)));
                    times.push(("group by key".to_string(), time_query_n(&dest, &group, 2)));
                    times.push(("day buckets".to_string(), time_query_n(&dest, &buckets, 2)));
                    // The same summary under the server's memory limit: the
                    // hash table has to fit, and the rest spills to disk. The
                    // key's size decides how much has to spill.
                    let _ = dest.execute_batch(
                        "SET memory_limit='256MB';
                         SET max_temp_directory_size='2GB';",
                    );
                    match try_time_query(&dest, &summary) {
                        Ok(ms) => times.push(("summary @256MB".to_string(), ms)),
                        Err(e) => notes.push(format!("summary @256MB: {e}")),
                    }
                }
            }
        }
        let size = std::fs::metadata(&out)?.len();
        let base = *baseline.get_or_insert(size);
        println!(
            "{:<18} {:>9.2} MB {:>12.3} {:>12.3}",
            variant.name,
            size as f64 / 1_048_576.0,
            size as f64 / rows as f64,
            (base - size) as f64 / rows as f64
        );
        for (name, ms) in &times {
            println!("    {name:<16} {ms:>10.1} ms");
        }
        for note in &notes {
            println!("    {note}");
        }
        let _ = std::fs::remove_file(&out);
    }

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// What the timestamp's **unit** costs on the CPU, not on the disk.
///
/// The same rows are written twice — once as a `TIMESTAMP` (microseconds, what
/// we have) and once as a `BIGINT` of milliseconds — and the queries the
/// reports actually run are timed on both.
fn compute(rows: i64) -> Result<()> {
    let dir = std::env::temp_dir().join("sumimi-precision-compute");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;

    let conn = Connection::open(dir.join("t.duckdb"))?;
    conn.execute_batch(&format!(
        "CREATE TABLE ts AS
         SELECT i AS id, make_timestamp(1704067200000000::BIGINT
                              + (hash(i) % 94608000000000)::BIGINT) AS created_at
         FROM range({rows}) t(i);
         CREATE TABLE ms AS
         SELECT i AS id, (1704067200000000::BIGINT
                              + (hash(i) % 94608000000000)::BIGINT) // 1000 AS created_at
         FROM range({rows}) t(i);"
    ))?;

    // (name, sql on TIMESTAMP, sql on BIGINT ms)
    let queries: [(&str, &str, &str); 4] = [
        (
            "scan",
            "SELECT count(*), sum(id) FROM ts",
            "SELECT count(*), sum(id) FROM ms",
        ),
        (
            "duration",
            "SELECT max(created_at) - min(created_at) FROM ts",
            "SELECT max(created_at) - min(created_at) FROM ms",
        ),
        (
            "day buckets",
            "SELECT date_trunc('day', created_at) d, count(*) FROM ts GROUP BY 1",
            "SELECT date_trunc('day', to_timestamp(created_at / 1000.0)) d, count(*)
             FROM ms GROUP BY 1",
        ),
        (
            "hour range",
            "SELECT date_trunc('hour', created_at) h, count(*) FROM ts
             WHERE created_at >= TIMESTAMP '2025-01-01' AND created_at < TIMESTAMP '2025-04-01'
             GROUP BY 1",
            "SELECT date_trunc('hour', to_timestamp(created_at / 1000.0)) h, count(*)
             FROM ms
             WHERE created_at >= 1735689600000 AND created_at < 1743465600000
             GROUP BY 1",
        ),
    ];

    println!("== {rows} rows: TIMESTAMP (us) vs BIGINT (ms) ==\n");
    println!("{:<14} {:>12} {:>12} {:>10}", "query", "TIMESTAMP us", "BIGINT ms", "ratio");
    for (name, q_ts, q_ms) in queries {
        let a = time_query(&conn, q_ts);
        let b = time_query(&conn, q_ms);
        println!(
            "{name:<14} {:>10.1} ms {:>10.1} ms {:>9.2}x",
            a,
            b,
            b / a.max(0.001)
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// The fastest of three runs, in milliseconds.
fn time_query(conn: &Connection, sql: &str) -> f64 {
    time_query_n(conn, sql, 3)
}

/// The fastest of `runs` runs, in milliseconds.
fn time_query_n(conn: &Connection, sql: &str, runs: usize) -> f64 {
    let mut best = f64::MAX;
    for _ in 0..runs {
        let start = std::time::Instant::now();
        let mut stmt = conn.prepare(sql).expect("prepare");
        let mut rows = stmt.query_map([], |_r| Ok(())).expect("query");
        while rows.next().is_some() {}
        drop(rows);
        drop(stmt);
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        if ms < best {
            best = ms;
        }
    }
    best
}

/// One run, in milliseconds — and the error instead of a panic, because under
/// a memory limit the query may not fit at all.
fn try_time_query(conn: &Connection, sql: &str) -> Result<f64> {
    let start = std::time::Instant::now();
    let mut stmt = conn.prepare(sql)?;
    let mut rows = stmt.query_map([], |_r| Ok(()))?;
    while rows.next().is_some() {}
    drop(rows);
    drop(stmt);
    Ok(start.elapsed().as_secs_f64() * 1000.0)
}
