//! How much does each way of counting visitors cost?
//!
//! The dashboard wants two numbers: the visitors and the visits. There are
//! several ways to ask for them, and they are not equally cheap.
//!
//! ```bash
//! cargo run --release --example distinct_cost -p sumimi-service -- 2000000
//! ```

use std::path::PathBuf;
use std::time::Instant;

use sumimi_service::pool::Db;

const ROWS: i64 = 2_000_000;
const VISITORS: i64 = 120_000;

/// The table: the v10 shape, with the traffic the other examples use.
fn build(path: &PathBuf) -> anyhow::Result<()> {
    let _ = std::fs::remove_file(path);
    let db = Db::open(path, "256MB")?;
    db.with_read(|conn| {
        conn.execute_batch(
            "CREATE TABLE probe (
                 id BIGINT, website_id VARCHAR, visit_id BIGINT, visitor_id BIGINT,
                 user_id VARCHAR, created_at TIMESTAMP, url VARCHAR, title VARCHAR,
                 browser VARCHAR, ip_prefix VARCHAR, user_agent VARCHAR,
                 event_type VARCHAR);",
        )?;
        Ok::<_, anyhow::Error>(())
    })?;
    db.with_write(|conn| {
        conn.execute_batch(&format!(
            "INSERT INTO probe
             SELECT i AS id, 'w1' AS website_id,
                ((1600000000 + ((i // {VISITORS}) // 4) * 600 + ((i % {VISITORS}) % 3600))::BIGINT << 32)
                    | (hash('v' || (i % {VISITORS})) % 4294967296) AS visit_id,
                CAST(hash('v' || (i % {VISITORS})) % 9223372036854775807 AS BIGINT) AS visitor_id,
                CASE WHEN i % 3 = 0 THEN NULL ELSE 'user-' || (i % {VISITORS}) END AS user_id,
                TIMESTAMP '2026-01-01 00:00:00' + INTERVAL (i * 4) SECOND AS created_at,
                '/blog/post-' || (i % 5000) AS url,
                'A reasonably long article title number ' || (i % 1000) AS title,
                CASE (i % {VISITORS}) % 4 WHEN 0 THEN 'Chrome' WHEN 1 THEN 'Safari'
                     WHEN 2 THEN 'Firefox' ELSE 'Edge' END AS browser,
                '203.0.113.' || CAST((i % {VISITORS}) % 40000 AS VARCHAR) AS ip_prefix,
                CASE (i % {VISITORS}) % 4 WHEN 0 THEN 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36'
                     WHEN 1 THEN 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36'
                     WHEN 2 THEN 'Mozilla/5.0 (iPhone; CPU iPhone OS 17_2 like Mac OS X) AppleWebKit/605.1.15'
                     ELSE 'Mozilla/5.0 (X11; Linux x86_64; rv:121.0) Gecko/20100101 Firefox/121.0' END AS user_agent,
                CASE WHEN i % 50 = 0 THEN 'event' ELSE 'pageview' END AS event_type
             FROM generate_series(1, {ROWS}) t(i)"
        ))?;
        Ok::<_, anyhow::Error>(())
    })?;
    db.with_read(|conn| {
        conn.execute_batch("CHECKPOINT")?;
        Ok::<_, anyhow::Error>(())
    })?;
    Ok(())
}

/// The ways to count, and what each of them is for.
fn queries() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "COUNT(DISTINCT visitor_id)",
            "SELECT CAST(count(DISTINCT visitor_id) AS VARCHAR) FROM probe",
        ),
        (
            "COUNT(DISTINCT visit_id)",
            "SELECT CAST(count(DISTINCT visit_id) AS VARCHAR) FROM probe",
        ),
        (
            "both, side by side",
            "SELECT CAST(count(DISTINCT visitor_id) AS VARCHAR) || ' / ' \
             || CAST(count(DISTINCT visit_id) AS VARCHAR) FROM probe",
        ),
        (
            "both, through the session grouping",
            "SELECT CAST(count(DISTINCT v) AS VARCHAR) || ' / ' || CAST(count(*) AS VARCHAR) \
             FROM (SELECT visit_id, any_value(visitor_id) AS v FROM probe GROUP BY visit_id)",
        ),
        (
            "COUNT(DISTINCT (visit_id, visitor_id))",
            "SELECT CAST(count(DISTINCT (visit_id, visitor_id)) AS VARCHAR) FROM probe",
        ),
        (
            "COUNT(DISTINCT (ip_prefix, user_agent))",
            "SELECT CAST(count(DISTINCT (ip_prefix, user_agent)) AS VARCHAR) FROM probe",
        ),
    ]
}

/// The two queries the summary runs today: the numbers, and the bounces.
fn summary_pair() -> (String, String) {
    (
        "SELECT CAST(count(DISTINCT visitor_id) AS VARCHAR) || ' / ' \
         || CAST(count(DISTINCT visit_id) AS VARCHAR) FROM probe".to_string(),
        "SELECT CAST(count(*) FILTER (WHERE n = 1 AND custom = 0) AS VARCHAR) || ' / ' \
         || CAST(COALESCE(AVG(d), 0) AS VARCHAR) FROM ( \
             SELECT visit_id, COUNT(*) AS n, \
                    MAX(CASE WHEN event_type = 'event' THEN 1 ELSE 0 END) AS custom, \
                    GREATEST(date_diff('microsecond', MIN(created_at), MAX(created_at)), 0) / 1000000.0 AS d \
             FROM probe GROUP BY visit_id) g".to_string(),
    )
}

/// Everything the summary wants, out of one grouping.
fn summary_one() -> String {
    "SELECT CAST(SUM(n) AS VARCHAR) || ' / ' || CAST(count(DISTINCT v) AS VARCHAR) \
     || ' / ' || CAST(count(*) AS VARCHAR) || ' / ' \
     || CAST(count(*) FILTER (WHERE n = 1 AND custom = 0) AS VARCHAR) || ' / ' \
     || CAST(COALESCE(AVG(d), 0) AS VARCHAR) FROM ( \
         SELECT visit_id, any_value(visitor_id) AS v, COUNT(*) AS n, \
                MAX(CASE WHEN event_type = 'event' THEN 1 ELSE 0 END) AS custom, \
                GREATEST(date_diff('microsecond', MIN(created_at), MAX(created_at)), 0) / 1000000.0 AS d \
         FROM probe GROUP BY visit_id) g"
        .to_string()
}

fn main() -> anyhow::Result<()> {
    let rows: i64 = std::env::args()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(ROWS);
    let path = PathBuf::from("/tmp/distinct-cost.duckdb");
    build(&path)?;

    println!("{rows} rows, {VISITORS} visitors\n");
    println!("  {:<40} {:>12} {:>14}", "the count", "time", "the number");
    for (label, sql) in queries() {
        let db = Db::open(&path, "256MB")?;
        let mut best = f64::MAX;
        let mut value = String::new();
        for _ in 0..3 {
            let started = Instant::now();
            value = db.with_read(|conn| {
                let v: String = conn.query_row(sql, [], |r| r.get(0))?;
                Ok::<_, anyhow::Error>(v)
            })?;
            best = best.min(started.elapsed().as_secs_f64() * 1000.0);
        }
        println!("  {label:<40} {best:>10.1}ms  {value:>14}");
    }
    // The summary as it runs today: two queries, two scans.
    let (numbers, bounces) = summary_pair();
    let db = Db::open(&path, "256MB")?;
    let mut best = f64::MAX;
    let mut out = String::new();
    for _ in 0..3 {
        let started = Instant::now();
        let a: String = db.with_read(|conn| {
            let v: String = conn.query_row(&numbers, [], |r| r.get(0))?;
            Ok::<_, anyhow::Error>(v)
        })?;
        let b: String = db.with_read(|conn| {
            let v: String = conn.query_row(&bounces, [], |r| r.get(0))?;
            Ok::<_, anyhow::Error>(v)
        })?;
        best = best.min(started.elapsed().as_secs_f64() * 1000.0);
        out = format!("{a} | {b}");
    }
    println!("\n  {label:<40} {best:>10.1}ms  {out:>14}", label = "the summary, two queries");

    // The same numbers out of one grouping.
    let sql = summary_one();
    let db = Db::open(&path, "256MB")?;
    let mut best = f64::MAX;
    let mut out = String::new();
    for _ in 0..3 {
        let started = Instant::now();
        out = db.with_read(|conn| {
            let v: String = conn.query_row(&sql, [], |r| r.get(0))?;
            Ok::<_, anyhow::Error>(v)
        })?;
        best = best.min(started.elapsed().as_secs_f64() * 1000.0);
    }
    println!("  {label:<40} {best:>10.1}ms  {out:>14}", label = "the summary, one grouping");

    let _ = std::fs::remove_file(&path);
    Ok(())
}
