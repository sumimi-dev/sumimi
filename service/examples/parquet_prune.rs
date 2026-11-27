//! Does a **coarse shard** make DuckDB read more than it has to?
//!
//! The owner objected to the claim "the bigger the site, the coarser the shard":
//! a coarse shard covers a longer time range, so a query window that lands
//! inside it reads the whole shard. That is true — but how much depends on
//! **how the shard was written**, not on the shard length alone.
//!
//! Parquet stores statistics per **row group** (min/max of every column). DuckDB
//! skips the row groups that cannot match the filter — but only if the rows
//! inside a row group share a narrow time range. Two ways to write a shard:
//!
//! - **`PARTITION_BY (dt)`** — DuckDB writes the partitions in parallel, and
//!   the rows of a shard end up scattered over the threads. Every row group
//!   then spans the whole shard, the statistics cannot rule anything out, and
//!   **the whole shard is read**;
//! - **one file per shard, written in time order** — the row groups are narrow,
//!   the statistics work, and only the row groups that can match are read.
//!
//! So the shard length is not the only lever: **the write mode** is one, and it
//! is ours to pick.
//!
//! This measures, for each shard layout and each write mode:
//!
//! 1. **shards touched** by a window that straddles the shard boundary;
//! 2. **rows read if the statistics work** (the groups that can match) vs
//!    **rows read if they do not** (the whole touched shards);
//! 3. the **over-read factor** = rows read / rows that match;
//! 4. the **query time**.
//!
//! ```bash
//! cargo run --release --example parquet_prune -p sumimi-service -- 50000
//! ```

use std::time::Instant;

use duckdb::Connection;

fn event_ddl(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(&format!(
        "CREATE TABLE website_event ({ddl});",
        ddl = sumimi_service::db::event_columns_ddl(),
    ))?;
    Ok(())
}

/// Fill the table with `rows` rows spread over `days` days, in time order —
/// what a real ingest does: the server stamps `created_at` when the event
/// arrives, and the events arrive in that order.
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

/// One row group of one shard.
struct Group {
    file: String,
    rows: i64,
    min: String,
    max: String,
}

/// The row groups of a layout, with their `created_at` statistics.
fn groups(conn: &Connection, pattern: &str) -> anyhow::Result<Vec<Group>> {
    let mut stmt = conn.prepare(
        "SELECT file_name, row_group_num_rows, stats_min_value, stats_max_value
         FROM parquet_metadata(?) WHERE path_in_schema = 'created_at'",
    )?;
    let rows = stmt.query_map([pattern], |r| {
        Ok(Group {
            file: r.get(0)?,
            rows: r.get(1)?,
            min: r.get(2)?,
            max: r.get(3)?,
        })
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
        .unwrap_or(50_000);
    let days: i64 = std::env::args()
        .nth(2)
        .and_then(|a| a.parse().ok())
        .unwrap_or(120);
    let rows = rows_per_day * days;

    println!("(site scale: {rows_per_day} rows/day × {days} days = {rows} rows)");

    let dir = std::env::temp_dir().join(format!("sumimi-parquet-prune-{rows_per_day}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;

    let db = dir.join("prune.duckdb");
    let conn = Connection::open(&db)?;
    event_ddl(&conn)?;

    // The window: 30 days, **not** aligned with any shard boundary — it starts
    // in the middle of a day and ends in the middle of another one.
    let from = "TIMESTAMP '2026-02-10 12:00:00'";
    let to = "TIMESTAMP '2026-03-12 12:00:00'";
    let from_s = "2026-02-10 12:00:00";
    let to_s = "2026-03-12 12:00:00";

    let layouts: Vec<(&str, String)> = vec![
        ("by day", "CAST(created_at AS DATE)".to_string()),
        (
            "by 7 days",
            "CAST(date_trunc('week', created_at) AS DATE)".to_string(),
        ),
        (
            "by 30 days",
            "CAST(TIMESTAMP '1970-01-01' + INTERVAL (CAST(date_diff('day', TIMESTAMP '1970-01-01', created_at) / 30 AS INT)) DAY AS DATE)".to_string(),
        ),
        (
            "by 60 days",
            "CAST(TIMESTAMP '1970-01-01' + INTERVAL (CAST(date_diff('day', TIMESTAMP '1970-01-01', created_at) / 60 AS INT)) DAY AS DATE)".to_string(),
        ),
        (
            "by natural month",
            "CAST(date_trunc('month', created_at) AS DATE)".to_string(),
        ),
    ];

    println!();
    println!("=== a 30-day window that straddles the shard boundary ===");
    println!("(over-read = rows DuckDB reads / rows that match)");

    fill(&conn, rows, days)?;

    for (name, bucket) in &layouts {
        for mode in ["partition_by", "one file", "one file, hive dirs"] {
            let root = dir.join(format!("{}-{}", name.replace(' ', "-"), mode.replace(' ', "-")));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root)?;

            let t = Instant::now();
            let pattern = match mode {
                "partition_by" => {
                    conn.execute_batch(&format!(
                        "COPY (SELECT *, {bucket} AS dt FROM website_event)
                         TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD, PARTITION_BY (dt));",
                        root.display()
                    ))?;
                    format!("{}/dt=*/*.parquet", root.display())
                }
                _ => {
                    // One file per shard, written from a time-ordered source, so
                    // a row group covers a narrow time range and its statistics
                    // can rule rows out. `hive dirs` puts each shard in its own
                    // `dt=` directory — the same layout DuckDB's partitioned
                    // write produces, but written one file at a time.
                    let mut stmt = conn.prepare(&format!(
                        "SELECT DISTINCT {bucket} AS dt FROM website_event ORDER BY dt"
                    ))?;
                    let dts: Vec<(String,)> = stmt
                        .query_map([], |r| Ok((r.get(0)?,)))?
                        .collect::<Result<Vec<_>, _>>()?;
                    for (dt,) in dts {
                        let out = if mode == "one file, hive dirs" {
                            let dir = root.join(format!("dt={dt}"));
                            std::fs::create_dir_all(&dir)?;
                            dir.join("data_0.parquet")
                        } else {
                            root.join(format!("shard-{dt}.parquet"))
                        };
                        conn.execute_batch(&format!(
                            "COPY (SELECT * FROM website_event WHERE {bucket} = DATE '{dt}' ORDER BY created_at)
                             TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD);",
                            out.display()
                        ))?;
                    }
                    if mode == "one file, hive dirs" {
                        format!("{}/dt=*/*.parquet", root.display())
                    } else {
                        format!("{}/*.parquet", root.display())
                    }
                }
            };
            let write = t.elapsed();

            let gs = groups(&conn, &pattern)?;

            // The shards the window touches, and the rows they hold.
            let mut touched_files: Vec<String> = Vec::new();
            let mut rows_in_touched = 0i64;
            let mut rows_after_pruning = 0i64;
            let mut groups_kept = 0i64;
            for g in &gs {
                if g.min.as_str() < to_s && g.max.as_str() >= from_s {
                    rows_after_pruning += g.rows;
                    groups_kept += 1;
                    if !touched_files.contains(&g.file) {
                        touched_files.push(g.file.clone());
                    }
                }
            }
            for g in &gs {
                if touched_files.contains(&g.file) {
                    rows_in_touched += g.rows;
                }
            }

            let mut times = Vec::new();
            let mut matched = 0i64;
            for _ in 0..20 {
                let t = Instant::now();
                matched = conn.query_row(
                    &format!(
                        "SELECT count(*) FROM read_parquet('{pattern}') WHERE created_at >= {from} AND created_at < {to}"
                    ),
                    [],
                    |r| r.get(0),
                )?;
                times.push(t.elapsed().as_secs_f64() * 1000.0);
            }
            times.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let median = times[times.len() / 2];

            println!(
                "  {:<16} {:<18}  shards {:>2}   groups {:>4} (kept {:>4})   rows read {:>9}   matching {:>9}   over-read {:>5.2}x   without pruning {:>9} ({:>4.2}x)   query {:>6.1} ms   write {:>6.0} ms",
                name,
                mode,
                touched_files.len(),
                gs.len(),
                groups_kept,
                rows_after_pruning,
                matched,
                rows_after_pruning as f64 / matched.max(1) as f64,
                rows_in_touched,
                rows_in_touched as f64 / matched.max(1) as f64,
                median,
                write.as_secs_f64() * 1000.0,
            );
        }
    }

    // The same layouts, but for the three windows a merchant actually asks
    // for — written the way that works (one file per shard, in time order).
    println!();
    println!("=== the windows a merchant asks for (one file per shard, in time order) ===");
    let windows: Vec<(&str, &str, &str)> = vec![
        ("7 days", "TIMESTAMP '2026-03-01 00:00:00'", "TIMESTAMP '2026-03-08 00:00:00'"),
        ("30 days", "TIMESTAMP '2026-02-10 12:00:00'", "TIMESTAMP '2026-03-12 12:00:00'"),
        ("90 days", "TIMESTAMP '2025-12-15 12:00:00'", "TIMESTAMP '2026-03-12 12:00:00'"),
    ];
    for (name, bucket) in &layouts {
        let root = dir.join(format!("win-{}", name.replace(' ', "-")));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root)?;
        let mut stmt = conn.prepare(&format!(
            "SELECT DISTINCT {bucket} AS dt FROM website_event ORDER BY dt"
        ))?;
        let dts: Vec<(String,)> = stmt
            .query_map([], |r| Ok((r.get(0)?,)))?
            .collect::<Result<Vec<_>, _>>()?;
        for (dt,) in dts {
            let out = root.join(format!("shard-{dt}.parquet"));
            conn.execute_batch(&format!(
                "COPY (SELECT * FROM website_event WHERE {bucket} = DATE '{dt}' ORDER BY created_at)
                 TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD);",
                out.display()
            ))?;
        }
        let pattern = format!("{}/*.parquet", root.display());
        let gs = groups(&conn, &pattern)?;

        print!("  {:<16}", name);
        for (label, from, to) in &windows {
            let from_s = from.replace("TIMESTAMP '", "").replace("'", "");
            let to_s = to.replace("TIMESTAMP '", "").replace("'", "");
            let mut rows_after_pruning = 0i64;
            let mut touched = 0i64;
            for g in &gs {
                if g.min.as_str() < to_s.as_str() && g.max.as_str() >= from_s.as_str() {
                    rows_after_pruning += g.rows;
                    touched += 1;
                }
            }
            let mut times = Vec::new();
            let mut matched = 0i64;
            for _ in 0..20 {
                let t = Instant::now();
                matched = conn.query_row(
                    &format!(
                        "SELECT count(*) FROM read_parquet('{pattern}') WHERE created_at >= {from} AND created_at < {to}"
                    ),
                    [],
                    |r| r.get(0),
                )?;
                times.push(t.elapsed().as_secs_f64() * 1000.0);
            }
            times.sort_by(|a, b| a.partial_cmp(b).unwrap());
            print!(
                "   {}: {:>2} shards {:>6.2} ms ({:.2}x)",
                label,
                touched,
                times[times.len() / 2],
                rows_after_pruning as f64 / matched.max(1) as f64
            );
        }
        println!();
    }

    Ok(())
}
