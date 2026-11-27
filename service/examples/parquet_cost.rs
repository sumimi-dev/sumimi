//! How much does a **Parquet shard** cost, and what does a query over many of them cost?
//!
//! The cold tier of the storage layout is Parquet (issue #14), sharded by time
//! (issue #163). Before we can decide anything about the shard length — a day, a
//! week, a natural month — three numbers have to exist:
//!
//! 1. **the fixed cost of one shard** (a Parquet file with almost no rows);
//! 2. **the bytes per row** (compared with the 47.2 B/row the DuckDB file costs);
//! 3. **what a query over N shards costs** — the schema inference over a glob, and
//!    how much a narrow time filter actually saves (predicate pushdown).
//!
//! It also reports the row-group size, because **the row group, not the shard,
//! is what decides the scan precision**.
//!
//! ```bash
//! cargo run --release --example parquet_cost -p sumimi-service
//! ```

use std::time::Instant;

use duckdb::Connection;

/// The event schema — the real one, generated the same way the server writes it.
fn event_ddl(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(&format!(
        "CREATE TABLE website_event ({ddl});",
        ddl = sumimi_service::db::event_columns_ddl(),
    ))?;
    Ok(())
}

/// Fill the table with rows that look like real traffic: 200 URLs, a handful of
/// referrers, and events spread over `days` days.
///
/// The columns are the schema's own, looked up **by name** — so a column the
/// schema has and this row does not is an error, not a misplaced value.
fn fill(conn: &Connection, rows: i64, days: i64) -> anyhow::Result<()> {
    let uuid = "CAST(substr(md5(i::VARCHAR), 1, 8) || '-' || substr(md5(i::VARCHAR), 9, 4) || '-' || \
                substr(md5(i::VARCHAR), 13, 4) || '-' || substr(md5(i::VARCHAR), 17, 4) || '-' || \
                substr(md5(i::VARCHAR), 21, 12) AS UUID)";
    let values: Vec<(&str, String)> = vec![
        ("event_id", "i".to_string()),
        ("website_id", "'w1'".to_string()),
        ("visit_id", uuid.to_string()),
        ("session_id", uuid.to_string()),
        (
            "created_at",
            "TIMESTAMP '2026-01-01' + INTERVAL (i % ".to_string()
                + &days.to_string()
                + ") DAY + INTERVAL (i % 86400) SECOND",
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
        // A column we do not fill is NULL — the raw ones (ip, user_agent) are
        // NULL in the default configuration anyway (#145).
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

fn file_size(path: &std::path::Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// The fixed cost of one shard, and the bytes per row.
fn shard_cost(dir: &std::path::Path) -> anyhow::Result<()> {
    println!("=== one Parquet shard: fixed cost and bytes per row ===");
    let db = dir.join("shard-cost.duckdb");
    let _ = std::fs::remove_file(&db);
    let conn = Connection::open(&db)?;
    event_ddl(&conn)?;

    for rows in [0i64, 1, 1_000, 100_000, 1_000_000] {
        conn.execute_batch("DELETE FROM website_event;")?;
        if rows > 0 {
            fill(&conn, rows, 90)?;
        }
        let out = dir.join(format!("shard-{rows}.parquet"));
        let _ = std::fs::remove_file(&out);
        conn.execute_batch(&format!(
            "COPY (SELECT * FROM website_event) TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD);",
            out.display()
        ))?;
        let size = file_size(&out);
        let per_row = if rows > 0 { size as f64 / rows as f64 } else { 0.0 };
        let (groups, rows_in_group): (i64, i64) = if rows > 0 {
            (
                conn.query_row(
                    "SELECT count(DISTINCT row_group_id) FROM parquet_metadata(?)",
                    [out.to_string_lossy().as_ref()],
                    |r| r.get(0),
                )?,
                conn.query_row(
                    "SELECT max(row_group_num_rows) FROM parquet_metadata(?)",
                    [out.to_string_lossy().as_ref()],
                    |r| r.get(0),
                )?,
            )
        } else {
            (0, 0)
        };
        println!(
            "  rows {:>9}  →  file {:>12} B   per row {:>7.2} B   row groups {:>4}   max group rows {:>9}",
            rows, size, per_row, groups, rows_in_group
        );
    }
    Ok(())
}

/// The same data, three shard layouts, the same query.
///
/// This is the measurement the shard-length decision actually needs: not
/// "how fast is a scan of N files" but "how much does **the same window**
/// cost when the data is cut one way or another".
fn query_cost(dir: &std::path::Path) -> anyhow::Result<()> {
    println!();
    println!("=== the same query (a 30-day window) over three shard layouts ===");

    let db = dir.join("query-cost.duckdb");
    let _ = std::fs::remove_file(&db);
    let conn = Connection::open(&db)?;
    event_ddl(&conn)?;
    // 365 days x 10 000 rows — a mid-sized store, not a toy.
    fill(&conn, 3_650_000, 365)?;

    // The window every layout is asked for.
    let from = "TIMESTAMP '2026-06-01'";
    let to = "TIMESTAMP '2026-07-01'";

    for (name, bucket) in [
        ("by day", "CAST(created_at AS DATE)"),
        ("by 7 days", "CAST(date_trunc('week', created_at) AS DATE)"),
        ("by 30 days", "CAST(TIMESTAMP '1970-01-01' + INTERVAL (CAST(date_diff('day', TIMESTAMP '1970-01-01', created_at) / 30 AS INT)) DAY AS DATE)"),
        ("by month", "CAST(date_trunc('month', created_at) AS DATE)"),
        ("by 60 days", "CAST(TIMESTAMP '1970-01-01' + INTERVAL (CAST(date_diff('day', TIMESTAMP '1970-01-01', created_at) / 60 AS INT)) DAY AS DATE)"),
        ("by 120 days", "CAST(TIMESTAMP '1970-01-01' + INTERVAL (CAST(date_diff('day', TIMESTAMP '1970-01-01', created_at) / 120 AS INT)) DAY AS DATE)"),
        ("by 180 days", "CAST(TIMESTAMP '1970-01-01' + INTERVAL (CAST(date_diff('day', TIMESTAMP '1970-01-01', created_at) / 180 AS INT)) DAY AS DATE)"),
    ] {
        let root = dir.join(format!("layout-{}", name.replace(' ', "-")));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root)?;

        // One pass, hive-partitioned — DuckDB's own way of writing shards.
        let t = Instant::now();
        conn.execute_batch(&format!(
            "COPY (SELECT *, {bucket} AS dt FROM website_event)
             TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD, PARTITION_BY (dt));",
            root.display()
        ))?;
        let write = t.elapsed();

        let pattern = format!("{}/dt=*/*.parquet", root.display());
        let files = walk(&root);

        let t = Instant::now();
        let n: i64 = conn.query_row(&format!("SELECT count(*) FROM read_parquet('{pattern}', hive_partitioning=true)"), [], |r| r.get(0))?;
        let full = t.elapsed();

        let t = Instant::now();
        let m: i64 = conn.query_row(
            &format!(
                "SELECT count(*) FROM read_parquet('{pattern}', hive_partitioning=true)
                 WHERE created_at >= {from} AND created_at < {to}"
            ),
            [],
            |r| r.get(0),
        )?;
        let window = t.elapsed();

        let bytes: u64 = dir_size(&root);

        let groups: i64 = conn.query_row(
            &format!("SELECT count(DISTINCT row_group_id) FROM parquet_metadata('{pattern}')"),
            [],
            |r| r.get(0),
        )?;
        println!(
            "  {:<12}  files {:>4}   row groups {:>4}   write {:>7.0} ms   full scan {:>8.0} ms ({n} rows)   30-day window {:>7.1} ms ({m} rows)   on disk {:>7.1} MB",
            name,
            files,
            groups,
            write.as_secs_f64() * 1000.0,
            full.as_secs_f64() * 1000.0,
            window.as_secs_f64() * 1000.0,
            bytes as f64 / (1024.0 * 1024.0)
        );
    }
    Ok(())
}

/// The bytes under a directory.
fn dir_size(dir: &std::path::Path) -> u64 {
    let mut total = 0;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                total += dir_size(&p);
            } else {
                total += file_size(&p);
            }
        }
    }
    total
}

/// How many files are under a directory.
fn walk(dir: &std::path::Path) -> usize {
    let mut n = 0;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                n += walk(&p);
            } else {
                n += 1;
            }
        }
    }
    n
}


/// The two finalists, measured properly: many repetitions, two scales, and the
/// query the merchant actually asks for — **a calendar month**, not a fixed
/// 30-day window.
fn finals(dir: &std::path::Path) -> anyhow::Result<()> {
    println!();
    println!("=== the two finalists: repeated runs, two scales, a calendar-month query ===");

    for rows_per_day in [10_000i64, 100_000] {
        let rows = rows_per_day * 365;
        let db = dir.join("finals.duckdb");
        let _ = std::fs::remove_file(&db);
        let conn = Connection::open(&db)?;
        event_ddl(&conn)?;
        fill(&conn, rows, 365)?;

        for (name, bucket) in [
            ("by 30 days", "CAST(TIMESTAMP '1970-01-01' + INTERVAL (CAST(date_diff('day', TIMESTAMP '1970-01-01', created_at) / 30 AS INT)) DAY AS DATE)"),
            ("by month", "CAST(date_trunc('month', created_at) AS DATE)"),
        ] {
            let root = dir.join(format!("final-{}", name.replace(' ', "-")));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root)?;
            let t = Instant::now();
            conn.execute_batch(&format!(
                "COPY (SELECT *, {bucket} AS dt FROM website_event)
                 TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD, PARTITION_BY (dt));",
                root.display()
            ))?;
            let write = t.elapsed();
            let pattern = format!("{}/dt=*/*.parquet", root.display());
            let files = walk(&root);

            // How many shards does a calendar month touch?
            let touched: i64 = conn.query_row(
                &format!(
                    "SELECT count(DISTINCT dt) FROM read_parquet('{pattern}', hive_partitioning=true)
                     WHERE created_at >= TIMESTAMP '2026-06-01' AND created_at < TIMESTAMP '2026-07-01'"
                ),
                [],
                |r| r.get(0),
            )?;

            // The query the merchant asks for: the calendar month of June.
            let mut window_times = Vec::new();
            let mut m: i64 = 0;
            for _ in 0..20 {
                let t = Instant::now();
                m = conn.query_row(
                    &format!(
                        "SELECT count(*) FROM read_parquet('{pattern}', hive_partitioning=true)
                         WHERE created_at >= TIMESTAMP '2026-06-01' AND created_at < TIMESTAMP '2026-07-01'"
                    ),
                    [],
                    |r| r.get(0),
                )?;
                window_times.push(t.elapsed().as_secs_f64() * 1000.0);
            }
            let mut full_times = Vec::new();
            for _ in 0..10 {
                let t = Instant::now();
                let _: i64 = conn.query_row(&format!("SELECT count(*) FROM read_parquet('{pattern}', hive_partitioning=true)"), [], |r| r.get(0))?;
                full_times.push(t.elapsed().as_secs_f64() * 1000.0);
            }
            window_times.sort_by(|a, b| a.partial_cmp(b).unwrap());
            full_times.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let med = |v: &Vec<f64>| v[v.len() / 2];
            let min = |v: &Vec<f64>| v[0];

            println!(
                "  {:>9} rows/day  {:<9}  files {:>3}   a calendar month touches {:>2} shards   write {:>6.0} ms   month query: min {:>6.2} ms  median {:>6.2} ms ({m} rows)   full scan: min {:>6.1} ms  median {:>6.1} ms   disk {:>6.1} MB",
                rows_per_day,
                name,
                files,
                touched,
                write.as_secs_f64() * 1000.0,
                min(&window_times),
                med(&window_times),
                min(&full_times),
                med(&full_times),
                dir_size(&root) as f64 / (1024.0 * 1024.0)
            );
        }
    }
    Ok(())
}


/// The owner's objection, measured: **a calendar month in the report's timezone**
/// (not in UTC) spans the shard boundary, so it touches one more shard.
///
/// The question is what that extra shard costs: a whole month of I/O, or one
/// row group? The second one is under our control — `ROW_GROUP_SIZE` is a write
/// option — so this measures the layouts at three row-group sizes.
fn local_month(dir: &std::path::Path) -> anyhow::Result<()> {
    println!();
    println!("=== a calendar month in UTC+8 (it straddles the shard boundary) ===");

    let rows_per_day = 100_000i64;
    let db = dir.join("local.duckdb");
    let _ = std::fs::remove_file(&db);
    let conn = Connection::open(&db)?;
    event_ddl(&conn)?;
    fill(&conn, rows_per_day * 365, 365)?;

    // June, as a UTC+8 merchant sees it: 2026-06-01 00:00 +08:00 .. 2026-07-01 00:00 +08:00
    let from = "TIMESTAMP '2026-05-31 16:00:00'";
    let to = "TIMESTAMP '2026-06-30 16:00:00'";

    for rg in [0usize, 10_000, 1_000] {
        let rg_opt = if rg == 0 { String::new() } else { format!(", ROW_GROUP_SIZE {rg}") };
        for (name, bucket) in [
            ("by day", "CAST(created_at AS DATE)"),
            ("by 7 days", "CAST(date_trunc('week', created_at) AS DATE)"),
            ("by 30 days", "CAST(TIMESTAMP '1970-01-01' + INTERVAL (CAST(date_diff('day', TIMESTAMP '1970-01-01', created_at) / 30 AS INT)) DAY AS DATE)"),
            ("by month", "CAST(date_trunc('month', created_at) AS DATE)"),
            ("by 60 days", "CAST(TIMESTAMP '1970-01-01' + INTERVAL (CAST(date_diff('day', TIMESTAMP '1970-01-01', created_at) / 60 AS INT)) DAY AS DATE)"),
        ] {
            let root = dir.join(format!("local-{}-{rg}", name.replace(' ', "-")));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root)?;
            conn.execute_batch(&format!(
                "COPY (SELECT *, {bucket} AS dt FROM website_event)
                 TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD, PARTITION_BY (dt){rg_opt});",
                root.display()
            ))?;
            let pattern = format!("{}/dt=*/*.parquet", root.display());

            let touched: i64 = conn.query_row(
                &format!(
                    "SELECT count(DISTINCT dt) FROM read_parquet('{pattern}', hive_partitioning=true)
                     WHERE created_at >= {from} AND created_at < {to}"
                ),
                [],
                |r| r.get(0),
            )?;
            let groups: i64 = conn.query_row(
                &format!("SELECT count(DISTINCT row_group_id) FROM parquet_metadata('{pattern}')"),
                [],
                |r| r.get(0),
            )?;

            let mut times = Vec::new();
            let mut m: i64 = 0;
            for _ in 0..20 {
                let t = Instant::now();
                m = conn.query_row(
                    &format!(
                        "SELECT count(*) FROM read_parquet('{pattern}', hive_partitioning=true)
                         WHERE created_at >= {from} AND created_at < {to}"
                    ),
                    [],
                    |r| r.get(0),
                )?;
                times.push(t.elapsed().as_secs_f64() * 1000.0);
            }
            times.sort_by(|a, b| a.partial_cmp(b).unwrap());
            println!(
                "  row group {:>7}   {:<9}  files {:>4}  groups {:>5}  a local month touches {:>2} shards   query min {:>6.2} ms  median {:>6.2} ms ({m} rows)",
                if rg == 0 { "default".to_string() } else { rg.to_string() },
                name,
                walk(&root),
                groups,
                touched,
                times[0],
                times[times.len() / 2]
            );
        }
        println!();
    }
    Ok(())
}


/// The candidates, measured on the two queries a merchant actually asks for:
/// **a calendar month** and **the last 30 days** — both in the report's timezone.
fn candidates(dir: &std::path::Path) -> anyhow::Result<()> {
    println!();
    println!("=== the candidates: a local calendar month, and a rolling 30 days ===");

    let rows_per_day = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(100_000i64);
    let db = dir.join("candidates.duckdb");
    let _ = std::fs::remove_file(&db);
    let conn = Connection::open(&db)?;
    event_ddl(&conn)?;
    fill(&conn, rows_per_day * 365, 365)?;

    // Weeks are natural weeks (Monday), counted from 1970-01-05 (the first Monday).
    let week_idx = "floor(date_diff('day', DATE '1970-01-05', date_trunc('week', created_at)) / 7)";
    let layouts: Vec<(&str, String)> = vec![
        ("7 days (1 natural week)", "CAST(date_trunc('week', created_at) AS DATE)".to_string()),
        ("14 days (2 weeks)", format!("CAST(DATE '1970-01-05' + INTERVAL (CAST({week_idx} / 2 AS INT) * 7) DAY AS DATE)")),
        ("21 days (3 weeks)", format!("CAST(DATE '1970-01-05' + INTERVAL (CAST({week_idx} / 3 AS INT) * 7) DAY AS DATE)")),
        ("28 days (4 weeks)", format!("CAST(DATE '1970-01-05' + INTERVAL (CAST({week_idx} / 4 AS INT) * 7) DAY AS DATE)")),
        ("30 days (fixed)", "CAST(TIMESTAMP '1970-01-01' + INTERVAL (CAST(date_diff('day', TIMESTAMP '1970-01-01', created_at) / 30 AS INT)) DAY AS DATE)".to_string()),
        ("60 days (fixed)", "CAST(TIMESTAMP '1970-01-01' + INTERVAL (CAST(date_diff('day', TIMESTAMP '1970-01-01', created_at) / 60 AS INT)) DAY AS DATE)".to_string()),
        ("natural month", "CAST(date_trunc('month', created_at) AS DATE)".to_string()),
    ];

    // Two windows, both in UTC+8 as a merchant in Shanghai would ask for them.
    let windows: Vec<(&str, &str, &str)> = vec![
        ("calendar month (June)", "TIMESTAMP '2026-05-31 16:00:00'", "TIMESTAMP '2026-06-30 16:00:00'"),
        ("rolling 30 days", "TIMESTAMP '2026-06-15 16:00:00'", "TIMESTAMP '2026-07-15 16:00:00'"),
    ];

    for (name, bucket) in &layouts {
        let root = dir.join(format!("cand-{}", name.replace(|c: char| !c.is_ascii_alphanumeric(), "-")));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root)?;
        conn.execute_batch(&format!(
            "COPY (SELECT *, {bucket} AS dt FROM website_event)
             TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD, PARTITION_BY (dt));",
            root.display()
        ))?;
        let pattern = format!("{}/dt=*/*.parquet", root.display());
        let files = walk(&root);

        print!("  {:<22} files {:>4}", name, files);
        for (label, from, to) in &windows {
            let touched: i64 = conn.query_row(
                &format!(
                    "SELECT count(DISTINCT dt) FROM read_parquet('{pattern}', hive_partitioning=true)
                     WHERE created_at >= {from} AND created_at < {to}"
                ),
                [],
                |r| r.get(0),
            )?;
            let mut times = Vec::new();
            let mut _m: i64 = 0;
            for _ in 0..20 {
                let t = Instant::now();
                _m = conn.query_row(
                    &format!(
                        "SELECT count(*) FROM read_parquet('{pattern}', hive_partitioning=true)
                         WHERE created_at >= {from} AND created_at < {to}"
                    ),
                    [],
                    |r| r.get(0),
                )?;
                times.push(t.elapsed().as_secs_f64() * 1000.0);
            }
            times.sort_by(|a, b| a.partial_cmp(b).unwrap());
            print!("   {}: {} shards {:>6.2} ms", label, touched, times[times.len() / 2]);
        }
        println!();
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    // `parquet_cost [rows_per_day]` — the scale of the site we pretend to be.
    let rows_per_day: i64 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(100_000);
    println!("(site scale: {rows_per_day} rows/day)");
    let dir = std::env::temp_dir().join(format!("sumimi-parquet-cost-{rows_per_day}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;

    if rows_per_day <= 100_000 {
        shard_cost(&dir)?;
        query_cost(&dir)?;
        finals(&dir)?;
        local_month(&dir)?;
    }
    candidates(&dir)?;

    println!();
    println!("（对照：#145 实测的 DuckDB 原生文件是 47.2 B/行；一个站文件的空底是 268 KB）");
    Ok(())
}
