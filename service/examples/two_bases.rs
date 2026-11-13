//! Two counting bases: what does splitting the count cost?
//!
//! When a site turns the mode on, anonymous visitors write `ANA-<baseurl>`
//! into `user_id` and logged-in visitors write their own id. The visitor count
//! then has to come from two places: the anonymous ones counted by
//! `visitor_id`, the logged-in ones counted by `user_id`.
//!
//! This measures that against counting everything by `visitor_id` alone —
//! one query with two aggregates, and two separate queries.
//!
//! ```bash
//! cargo run --release --example two_bases -p sumimi-service -- 8000000
//! ```

use std::path::PathBuf;
use std::time::Instant;

use sumimi_service::pool::Db;

const VISITORS: i64 = 120_000;

/// One query, two bases: the anonymous counted by the hash, the logged-in by
/// their own id.
const TWO_BASES: &str = "
    SELECT CAST(count(DISTINCT visitor_id) FILTER (WHERE user_id LIKE 'ANA-%') AS VARCHAR)
        || ' / ' ||
        CAST(count(DISTINCT user_id) FILTER (WHERE user_id NOT LIKE 'ANA-%' AND user_id IS NOT NULL) AS VARCHAR)
    FROM probe";

/// The same, as two queries.
const ANON_ONLY: &str =
    "SELECT CAST(count(DISTINCT visitor_id) AS VARCHAR) FROM probe WHERE user_id LIKE 'ANA-%'";
const LOGGED_ONLY: &str =
    "SELECT CAST(count(DISTINCT user_id) AS VARCHAR) FROM probe \
     WHERE user_id IS NOT NULL AND user_id NOT LIKE 'ANA-%'";

/// One basis: everything by the hash.
const ONE_BASIS: &str = "SELECT CAST(count(DISTINCT visitor_id) AS VARCHAR) FROM probe";

/// The breakdown, which is what a dashboard actually shows.
const BREAKDOWN: &str = "
    SELECT COALESCE(browser, 'unknown') AS name, count(DISTINCT visitor_id) AS value
    FROM probe GROUP BY name ORDER BY value DESC LIMIT 50";

fn build(rows: i64, logged_in_share: i64) -> anyhow::Result<PathBuf> {
    let path = PathBuf::from("/tmp/two-bases.duckdb");
    let _ = std::fs::remove_file(&path);
    let db = Db::open(&path, "256MB")?;
    db.with_read(|conn| {
        conn.execute_batch(
            "CREATE TABLE probe (
                 id BIGINT, website_id VARCHAR, visit_id BIGINT, visitor_id BIGINT,
                 user_id VARCHAR, created_at TIMESTAMP, url VARCHAR, title VARCHAR,
                 browser VARCHAR, browser_version VARCHAR, os VARCHAR, os_version VARCHAR,
                 device VARCHAR, screen VARCHAR, language VARCHAR, country VARCHAR,
                 region VARCHAR, city VARCHAR, latitude DOUBLE, longitude DOUBLE,
                 event_type VARCHAR);",
        )?;
        Ok::<_, anyhow::Error>(())
    })?;
    db.with_write(|conn| {
        conn.execute_batch(&format!(
            "INSERT INTO probe
             SELECT i AS id, 'w1' AS website_id,
                ((i // {VISITORS}) // 4)::BIGINT << 32 | (hash('v' || (i % {VISITORS})) % 4294967296) AS visit_id,
                CAST(hash('v' || (i % {VISITORS})) % 9223372036854775807 AS BIGINT) AS visitor_id,
                CASE
                    WHEN (i % 100) < {logged_in_share} THEN 'user-' || (i % {VISITORS})
                    WHEN (i % 100) < {logged_in_share} + 20 THEN 'ANA-example.com'
                    ELSE NULL
                END AS user_id,
                TIMESTAMP '2026-01-01 00:00:00' + INTERVAL (i * 4) SECOND AS created_at,
                '/blog/post-' || (i % 5000) AS url,
                'A reasonably long article title number ' || (i % 1000) AS title,
                CASE (i % {VISITORS}) % 4 WHEN 0 THEN 'Chrome' WHEN 1 THEN 'Safari'
                     WHEN 2 THEN 'Firefox' ELSE 'Edge' END AS browser,
                '120' AS browser_version, 'macOS' AS os, '10' AS os_version, 'desktop' AS device,
                '1920x1080' AS screen, 'en-US' AS language, 'DE' AS country, 'Berlin' AS region,
                'Berlin' AS city, 52.52 AS latitude, 13.405 AS longitude, 'pageview' AS event_type
             FROM generate_series(1, {rows}) t(i)"
        ))?;
        Ok::<_, anyhow::Error>(())
    })?;
    db.with_read(|conn| {
        conn.execute_batch("CHECKPOINT")?;
        Ok::<_, anyhow::Error>(())
    })?;
    Ok(path)
}

fn time(conn: &duckdb::Connection, sql: &str) -> (f64, String) {
    let mut best = f64::MAX;
    let mut out = String::new();
    for _ in 0..3 {
        let started = Instant::now();
        out = conn
            .query_row(sql, [], |r| r.get(0))
            .unwrap_or_default();
        best = best.min(started.elapsed().as_secs_f64() * 1000.0);
    }
    (best, out)
}

fn main() -> anyhow::Result<()> {
    for rows in [2_000_000i64, 8_000_000] {
        for share in [20i64, 40] {
            let path = build(rows, share)?;
            let bytes = std::fs::metadata(&path)?.len();
            let conn = duckdb::Connection::open(&path)?;
            let (one, one_out) = time(&conn, ONE_BASIS);
            let (two, two_out) = time(&conn, TWO_BASES);
            let (anon, anon_out) = time(&conn, ANON_ONLY);
            let (logged, logged_out) = time(&conn, LOGGED_ONLY);
            let (breakdown, _) = time(&conn, BREAKDOWN);
            println!("== {rows} rows, {share}% 登录 ==");
            println!("  the file: {:.2} B/row", bytes as f64 / rows as f64);
            println!("  one basis      : {one:>7.1}ms  {one_out}");
            println!("  two, one query : {two:>7.1}ms  {two_out}");
            println!("  two, two querys: {:>7.1}ms + {:>7.1}ms  {anon_out} / {logged_out}", anon, logged);
            println!("  the breakdown  : {breakdown:>7.1}ms");
            let _ = std::fs::remove_file(&path);
        }
    }
    Ok(())
}
