//! How much **memory** does a shard layout cost, and how should the shard length
//! be chosen?
//!
//! The owner settled the shape: **the natural month is out** (it is an analysis
//! concept — the report's month depends on its timezone, and a month cannot be
//! subdivided further), the unit is the **day**, and the shard length can vary
//! (**7-day natural weeks by default, down to 1 day for very large datasets**).
//!
//! What is left to measure is the second half of the question: **query time and
//! memory overhead — 7 days? 30 days? 35 days?**
//!
//! Two numbers decide it:
//!
//! 1. **the query time** (already measured elsewhere, repeated here);
//! 2. **the memory the query needs** — measured as *the smallest
//!    `memory_limit` at which the query still succeeds*. It is deterministic,
//!    comparable across layouts, and it is the number that actually matters
//!    when many sites share one machine (#129).
//!
//! Plus the structural numbers that explain both: how many shards a query
//! touches, how many rows it reads, and how big the **footer** (the row-group
//! metadata of a shard) is — the per-shard fixed memory.
//!
//! ```bash
//! cargo run --release --example parquet_mem -p sumimi-service -- 200000
//! ```

use std::time::Instant;

use anyhow::anyhow;
use duckdb::Connection;

fn event_ddl(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(&format!(
        "CREATE TABLE website_event ({ddl});",
        ddl = sumimi_service::db::event_columns_ddl(),
    ))?;
    Ok(())
}

/// Fill the table with `rows` rows spread over `days` days, in time order —
/// what a real ingest does.
fn fill(conn: &Connection, rows: i64, days: i64) -> anyhow::Result<()> {
    let uuid = "CAST(substr(md5(i::VARCHAR), 1, 8) || '-' || substr(md5(i::VARCHAR), 9, 4) || '-' || \
                substr(md5(i::VARCHAR), 13, 4) || '-' || substr(md5(i::VARCHAR), 17, 4) || '-' || \
                substr(md5(i::VARCHAR), 21, 12) AS UUID)";
    let per_day = rows / days;
    let values: Vec<(&str, String)> = vec![
        ("event_id", "i".to_string()),
        ("website_id", "'w1'".to_string()),
        ("visit_id", uuid.to_string()),
        ("session_id", uuid.to_string()),
        (
            "created_at",
            format!(
                "TIMESTAMP '2026-01-01' + INTERVAL (i / {per_day}) DAY + INTERVAL ((i % {per_day}) * 86400 / {per_day}) SECOND"
            ),
        ),
        ("url_path", "'/product/' || (i % 200)".to_string()),
        ("page_title", "'/page-title-' || (i % 200)".to_string()),
        ("hostname", "'shop.example.com'".to_string()),
        ("language", "'en'".to_string()),
        ("device", "'desktop'".to_string()),
        ("referrer_name", "'google'".to_string()),
        ("url_query", "'utm_source=google&utm_medium=cpc&utm_campaign=spring'".to_string()),
        ("session_start", "NULL".to_string()),
        ("ip", "''".to_string()),
        ("ip_prefix", "''".to_string()),
        ("user_agent", "''".to_string()),
        ("event_type", "'pageview'".to_string()),
        ("event_name", "NULL".to_string()),
        ("tag", "NULL".to_string()),
        ("data", "NULL".to_string()),
    ];
    let columns = sumimi_service::db::event_columns();
    let mut exprs = Vec::with_capacity(columns.len());
    for column in &columns {
        let expression = values
            .iter()
            .find(|(name, _)| name == column)
            .map(|(_, expression)| expression.as_str())
            .unwrap_or("NULL");
        exprs.push(expression);
    }
    conn.execute_batch(&format!(
        "INSERT INTO website_event ({}) SELECT {} FROM range(0, {rows}) t(i);",
        columns.join(", "),
        exprs.join(", "),
    ))?;
    Ok(())
}

/// The bucket expression of a shard length in days.
fn bucket(days: i64) -> String {
    if days == 7 {
        // Natural weeks (Monday), counted from 1970-01-05 — the first Monday.
        "CAST(DATE '1970-01-05' + INTERVAL (CAST(floor(date_diff('day', DATE '1970-01-05', date_trunc('week', created_at)) / 7) AS INT) * 7) DAY AS DATE)".to_string()
    } else {
        format!(
            "CAST(TIMESTAMP '1970-01-01' + INTERVAL ((CAST(date_diff('day', TIMESTAMP '1970-01-01', created_at) / {days} AS INT)) * {days}) DAY AS DATE)"
        )
    }
}

/// The smallest `memory_limit` at which the query still succeeds.
///
/// DuckDB fails the query with an out-of-memory error once the limit is
/// exceeded, so a binary search over the limit gives the peak memory the query
/// needs — the same number for every layout, so they can be compared.
fn min_memory_limit(conn: &Connection, query: &str) -> anyhow::Result<(u64, bool)> {
    let mut low: u64 = 1 << 20; // 1 MB
    let mut high: u64 = 1 << 30; // 1 GB
    let fits = |limit: u64| -> bool {
        if conn.execute_batch(&format!("SET memory_limit='{limit}'")).is_err() {
            return false;
        }
        match conn.execute(query, []) {
            Ok(_) => true,
            Err(e) => !format!("{e}").contains("Out of Memory"),
        }
    };
    if !fits(high) {
        return Ok((0, false));
    }
    while low < high {
        let mid = low + (high - low) / 2;
        if fits(mid) {
            high = mid;
        } else {
            low = mid + 1;
        }
    }
    conn.execute_batch("SET memory_limit='16GB'")?;
    Ok((low, true))
}

/// The **fixed cost of one file** — on disk and in a query.
///
/// Two things have to be separated:
/// - **on disk**: what an (almost) empty shard costs to keep;
/// - **in a query**: what opening one file costs, independent of how many
///   rows are in it (the footer has to be parsed to know what is inside).
fn file_overhead(dir: &std::path::Path) -> anyhow::Result<()> {
    println!();
    println!("=== the fixed cost of one file ===");

    let db = dir.join("overhead.duckdb");
    let _ = std::fs::remove_file(&db);
    let conn = Connection::open(&db)?;
    event_ddl(&conn)?;

    // On disk: an almost empty shard.
    println!("  on disk (the real 51-column schema):");
    for rows in [0i64, 1, 10, 100, 1_000, 10_000] {
        conn.execute_batch("DELETE FROM website_event;")?;
        if rows > 0 {
            fill(&conn, rows, 1)?;
        }
        let out = dir.join(format!("empty-{rows}.parquet"));
        let _ = std::fs::remove_file(&out);
        conn.execute_batch(&format!(
            "COPY (SELECT * FROM website_event) TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD);",
            out.display()
        ))?;
        let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
        println!(
            "    {:>6} rows  →  {:>7} B   ({:.1} B/row)",
            rows,
            size,
            if rows > 0 { size as f64 / rows as f64 } else { 0.0 }
        );
    }

    // In a query: K small files, and a filter that matches none of them.
    // DuckDB still has to open every file and read its footer, so the time
    // grows with K — the slope is the per-file cost.
    println!("  in a query (each file holds 1 000 rows; the filter matches nothing):");
    let root = dir.join("overhead-files");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root)?;
    conn.execute_batch("DELETE FROM website_event;")?;
    fill(&conn, 1_000, 1)?;

    let mut previous: Option<(usize, f64)> = None;
    for k in [1usize, 5, 10, 20, 50, 100, 200] {
        let sub = root.join(format!("k-{k}"));
        let _ = std::fs::remove_dir_all(&sub);
        std::fs::create_dir_all(&sub)?;
        for i in 0..k {
            let out = sub.join(format!("f{i}.parquet"));
            conn.execute_batch(&format!(
                "COPY (SELECT * FROM website_event) TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD);",
                out.display()
            ))?;
        }
        let pattern = format!("{}/*.parquet", sub.display());
        let mut times = Vec::new();
        for _ in 0..20 {
            let t = Instant::now();
            conn.execute(
                &format!(
                    "SELECT count(*) FROM read_parquet('{pattern}') WHERE created_at >= TIMESTAMP '2030-01-01'"
                ),
                [],
            )?;
            times.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median = times[times.len() / 2];
        let per_file = match previous {
            Some((pk, pt)) => (median - pt) / (k as f64 - pk as f64),
            None => 0.0,
        };
        println!(
            "    {:>4} files  →  {:>7.2} ms   ({:.3} ms per file)",
            k,
            median,
            per_file
        );
        previous = Some((k, median));
    }
    Ok(())
}

/// **Why does the footer grow?** The footer holds one entry per **row group**
/// per **column** — the column chunk's offset and size, the min/max statistics,
/// the null count, the encodings. So its size is set by
/// `rows / 122 880 (the row-group size) × columns`.
///
/// This measures it directly: the same rows written with a growing number of
/// columns, and the row groups per shard for each layout.
fn footer_why(dir: &std::path::Path) -> anyhow::Result<()> {
    println!();
    println!("=== why the footer grows ===");

    let db = dir.join("footer.duckdb");
    let _ = std::fs::remove_file(&db);
    let conn = Connection::open(&db)?;
    event_ddl(&conn)?;
    fill(&conn, 6_000_000, 30)?;

    // The same rows, with a growing number of columns.
    println!("  the column count decides it (6 000 000 rows in one file):");
    for columns in [1usize, 8, 16, 32, 51] {
        let mut ddl = String::new();
        for i in 0..columns {
            ddl.push_str(&format!("c{i} BIGINT, d{i} VARCHAR, t{i} TIMESTAMP, "));
        }
        ddl.pop();
        conn.execute_batch("DROP TABLE IF EXISTS wide;")?;
        conn.execute_batch(&format!("CREATE TABLE wide ({ddl});"))?;
        let exprs: Vec<String> = (0..columns)
            .flat_map(|i| {
                [
                    format!("CAST(i AS BIGINT)"),
                    format!("'v' || CAST(i % 100 AS VARCHAR)"),
                    format!("TIMESTAMP '2026-01-01' + INTERVAL (i) SECOND"),
                ]
                .iter()
                .map(|e| e.replace("i ", &format!("{i} ")).to_string())
                .collect::<Vec<_>>()
            })
            .collect();
        let names: Vec<String> = (0..columns)
            .flat_map(|i| [format!("c{i}"), format!("d{i}"), format!("t{i}")])
            .collect();
        conn.execute_batch(&format!(
            "INSERT INTO wide ({}) SELECT {} FROM range(0, 6000000) t(i);",
            names.join(", "),
            exprs.join(", ")
        ))?;
        let out = dir.join(format!("cols-{columns}.parquet"));
        let _ = std::fs::remove_file(&out);
        conn.execute_batch(&format!(
            "COPY (SELECT * FROM wide) TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD);",
            out.display()
        ))?;
        let file_bytes = std::fs::metadata(&out).map(|m| m.len() as i64).unwrap_or(0);
        let (data, groups): (i64, i64) = conn.query_row(
            &format!(
                "SELECT sum(total_compressed_size)::BIGINT, count(DISTINCT row_group_id)
                 FROM parquet_metadata('{}')",
                out.display()
            ),
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        println!(
            "    {:>2} columns  →  file {:>8} B   data {:>8} B   footer {:>7} B   row groups {:>3}   footer per group {:>6.0} B",
            columns * 3,
            file_bytes,
            data,
            file_bytes - data,
            groups,
            (file_bytes - data) as f64 / groups.max(1) as f64
        );
    }

    // The row groups per shard, for the layouts that matter.
    println!("  the row groups per shard (rows / 122 880):");
    for length in [1i64, 7, 30, 60] {
        let bucket = bucket(length);
        let root = dir.join(format!("fg-{length}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root)?;
        let mut stmt = conn.prepare(&format!(
            "SELECT DISTINCT {bucket} AS dt FROM website_event ORDER BY dt"
        ))?;
        let dts: Vec<(String,)> = stmt
            .query_map([], |r| Ok((r.get(0)?,)))?
            .collect::<Result<Vec<_>, _>>()?;
        for (dt,) in &dts {
            let out = root.join(format!("shard-{dt}.parquet"));
            conn.execute_batch(&format!(
                "COPY (SELECT * FROM website_event WHERE {bucket} = DATE '{dt}' ORDER BY created_at)
                 TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD);",
                out.display()
            ))?;
        }
        let pattern = format!("{}/*.parquet", root.display());
        // `row_group_id` restarts at 0 for every file, so the number of row
        // groups of a shard is the number of rows the metadata has for its
        // `created_at` column — one row per group.
        let (groups, rows, data): (i64, i64, i64) = conn.query_row(
            &format!(
                "SELECT count(*) FILTER (WHERE path_in_schema = 'created_at'),
                        sum(row_group_num_rows) FILTER (WHERE path_in_schema = 'created_at')::BIGINT,
                        sum(total_compressed_size)::BIGINT
                 FROM parquet_metadata('{pattern}')"
            ),
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let files = std::fs::read_dir(&root).map(|d| d.count()).unwrap_or(0) as i64;
        let file_bytes: i64 = std::fs::read_dir(&root)
            .map(|d| {
                d.flatten()
                    .map(|e| e.metadata().map(|m| m.len() as i64).unwrap_or(0))
                    .sum()
            })
            .unwrap_or(0);
        println!(
            "    {:>2} day shards: {:>3} shards, {:>3} groups each, {:>8} rows each, footer {:>7} B each ({:.0} B per group)",
            length,
            files,
            groups / files.max(1),
            rows / files.max(1),
            (file_bytes - data) / files.max(1),
            (file_bytes - data) as f64 / groups.max(1) as f64
        );
    }
    Ok(())
}

/// The first day of a 7-day group, plus `day` days.
fn group_date(group: &str, day: i64) -> String {
    let start = chrono::NaiveDate::parse_from_str(group, "%Y-%m-%d")
        .unwrap_or_else(|_| chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap());
    (start + chrono::Duration::days(day)).format("%Y-%m-%d").to_string()
}

/// The metadata of every file of a layout.
fn shards_of(conn: &Connection, pattern: &str) -> anyhow::Result<Vec<(String, String, String, i64, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT file_name,
                min(stats_min_value) FILTER (WHERE path_in_schema = 'created_at'),
                max(stats_max_value) FILTER (WHERE path_in_schema = 'created_at'),
                sum(row_group_num_rows) FILTER (WHERE path_in_schema = 'created_at')::BIGINT,
                sum(total_compressed_size)::BIGINT
         FROM parquet_metadata(?) GROUP BY file_name",
    )?;
    let rows = stmt.query_map([pattern], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn main() -> anyhow::Result<()> {
    let rows_per_day: i64 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(200_000);
    let days: i64 = std::env::args()
        .nth(2)
        .and_then(|a| a.parse().ok())
        .unwrap_or(120);
    let rows = rows_per_day * days;

    println!("(site scale: {rows_per_day} rows/day × {days} days = {rows} rows)");
    println!("(every shard is one file, written in time order — the way that works)");

    let dir = std::env::temp_dir().join(format!("sumimi-parquet-mem-{rows_per_day}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;

    let db = dir.join("mem.duckdb");
    let conn = Connection::open(&db)?;
    event_ddl(&conn)?;
    fill(&conn, rows, days)?;

    let windows: Vec<(&str, &str, &str)> = vec![
        ("7 days", "TIMESTAMP '2026-03-01 00:00:00'", "TIMESTAMP '2026-03-08 00:00:00'"),
        ("30 days", "TIMESTAMP '2026-02-10 12:00:00'", "TIMESTAMP '2026-03-12 12:00:00'"),
        ("90 days", "TIMESTAMP '2025-12-15 12:00:00'", "TIMESTAMP '2026-03-12 12:00:00'"),
    ];
    // The shard lengths to sweep. `parquet_mem <rows_per_day> <days> <lengths> [quick]`
    // `split7` = a 7-day shard written as 7 daily files inside one directory.
    let lengths: Vec<String> = std::env::args()
        .nth(3)
        .map(|s| s.split(',').map(|x| x.trim().to_string()).collect())
        .unwrap_or_else(|| vec!["1".into(), "7".into(), "14".into(), "30".into(), "35".into(), "60".into()]);
    let quick = std::env::args().nth(4).as_deref() == Some("quick");

    if !quick {
        file_overhead(&dir)?;
        footer_why(&dir)?;
    }

    println!();
    println!("=== query time and the memory it needs ===");
    println!("(memory = the smallest memory_limit the query still runs under)");

    for spec in lengths {
        let split = spec == "split7";
        let length: i64 = if split { 7 } else { spec.parse().unwrap_or(7) };
        let bucket = bucket(length);
        let root = dir.join(format!("shard-{length}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root)?;

        let pattern = if split {
            let mut stmt = conn.prepare(&format!(
                "SELECT DISTINCT {bucket} AS dt FROM website_event ORDER BY dt"
            ))?;
            let groups: Vec<(String,)> = stmt
                .query_map([], |r| Ok((r.get(0)?,)))?
                .collect::<Result<Vec<_>, _>>()?;
            for (group,) in &groups {
                let sub = root.join(format!("shard-{group}"));
                std::fs::create_dir_all(&sub)?;
                for day in 0..7i64 {
                    let dt = group_date(group, day);
                    let out = sub.join(format!("day-{dt}.parquet"));
                    conn.execute_batch(&format!(
                        "COPY (SELECT * FROM website_event WHERE CAST(created_at AS DATE) = DATE '{dt}' ORDER BY created_at)
                         TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD);",
                        out.display()
                    ))?;
                }
            }
            format!("{}/*/*.parquet", root.display())
        } else {
            let mut stmt = conn.prepare(&format!(
                "SELECT DISTINCT {bucket} AS dt FROM website_event ORDER BY dt"
            ))?;
            let dts: Vec<(String,)> = stmt
                .query_map([], |r| Ok((r.get(0)?,)))?
                .collect::<Result<Vec<_>, _>>()?;
            for (dt,) in &dts {
                let out = root.join(format!("shard-{dt}.parquet"));
                conn.execute_batch(&format!(
                    "COPY (SELECT * FROM website_event WHERE {bucket} = DATE '{dt}' ORDER BY created_at)
                     TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD);",
                    out.display()
                ))?;
            }
            format!("{}/*.parquet", root.display())
        };

        // Every shard: its footer (the row-group metadata — the per-shard
        // fixed memory), its rows, and the time range it covers.
        struct Shard {
            min: String,
            max: String,
            rows: i64,
            data_bytes: i64,
            file_bytes: i64,
        }
        let shards: Vec<Shard> = {
            // The data bytes of a shard = the compressed size of all its column
            // chunks; what is left of the file is the footer — the row-group
            // metadata DuckDB has to hold to decide what it can skip.
            let mut stmt = conn.prepare(
                "SELECT file_name,
                        min(stats_min_value) FILTER (WHERE path_in_schema = 'created_at'),
                        max(stats_max_value) FILTER (WHERE path_in_schema = 'created_at'),
                        sum(row_group_num_rows) FILTER (WHERE path_in_schema = 'created_at')::BIGINT,
                        sum(total_compressed_size)::BIGINT
                 FROM parquet_metadata(?) GROUP BY file_name",
            )?;
            let rows = stmt.query_map([pattern.as_str()], |r| {
                let file: String = r.get(0)?;
                let data: i64 = r.get(4)?;
                let file_bytes = std::fs::metadata(&file).map(|m| m.len() as i64).unwrap_or(0);
                Ok(Shard {
                    min: r.get(1)?,
                    max: r.get(2)?,
                    rows: r.get(3)?,
                    data_bytes: data,
                    file_bytes,
                })
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            out
        };

        // The footer of a shard — the row-group metadata it carries. This is
        // the per-shard fixed memory: DuckDB has to hold it to decide which
        // row groups can match.
        let footer_of = |s: &Shard| s.file_bytes - s.data_bytes;
        print!(
            "  {:<18} ({} files, footer {:>6} KB each)",
            if split { "7 days, 7 files" } else { "shards" },
            shards.len(),
            shards.first().map(footer_of).unwrap_or(0) / 1024
        );
        for (label, from, to) in &windows {
            // Which shards does the window touch, and what do they hold?
            let from_s = from.replace("TIMESTAMP '", "").replace("'", "");
            let to_s = to.replace("TIMESTAMP '", "").replace("'", "");
            let mut touched_shards = 0i64;
            let mut rows_read = 0i64;
            let mut footer_read = 0i64;
            for s in &shards {
                if s.min.as_str() < to_s.as_str() && s.max.as_str() >= from_s.as_str() {
                    touched_shards += 1;
                    rows_read += s.rows;
                    footer_read += footer_of(s);
                }
            }

            let query = format!(
                "SELECT date_trunc('day', created_at) AS d, count(*) AS n
                 FROM read_parquet('{pattern}')
                 WHERE created_at >= {from} AND created_at < {to}
                 GROUP BY d ORDER BY d"
            );

            let mut times = Vec::new();
            for _ in 0..20 {
                let t = Instant::now();
                conn.execute(&query, [])?;
                times.push(t.elapsed().as_secs_f64() * 1000.0);
            }
            times.sort_by(|a, b| a.partial_cmp(b).unwrap());

            let (limit, ok) = min_memory_limit(&conn, &query)?;

            // What DuckDB is holding after the query: the reader metadata of
            // the shards it opened, and the buffer manager.
            let held: i64 = conn.query_row(
                "SELECT coalesce(sum(memory_usage_bytes), 0) FROM duckdb_memory()
                 WHERE tag IN ('PARQUET_READER', 'EXTERNAL_FILE_CACHE', 'OBJECT_CACHE', 'COLUMN_DATA', 'METADATA')",
                [],
                |r| r.get(0),
            )?;

            print!(
                "   {}: {:>2} shards  rows {:>9}  meta {:>6} KB  held {:>6} KB  {:>6.1} ms  (limit {:>4} MB)",
                label,
                touched_shards,
                rows_read,
                footer_read / 1024,
                held / 1024,
                times[times.len() / 2],
                if ok { limit / (1024 * 1024) } else { 0 },
            );
        }
        println!();
    }

    Ok(())
}
