//! Two versions of the ids, measured: **16 bytes** (the default) and **8 bytes**
//! (`--eco`).
//!
//! - **16 bytes**: `visitor_id` and `visit_id` are UUIDs — what Umami does,
//!   and what the old v7 schema had.
//! - **8 bytes**: both ids are 64-bit values out of a SHA-256 — what the
//!   current `visitor_key()` does (the first eight bytes of the digest).
//!
//! The queries are the ones a dashboard runs (the v10 shape: the visitor is
//! counted by `COUNT(DISTINCT visitor_id)`, the sessions by
//! `COUNT(DISTINCT visit_id)`), so the numbers say what the width costs at
//! the report's desk, not just on disk.
//!
//! ```bash
//! cargo run --release --example id_shape_speed -p sumimi-service -- 10000000
//! ```

use std::path::PathBuf;
use std::time::Instant;

use sumimi_service::db::EVENT_COLUMNS;
use sumimi_service::pool::Db;

const VISITORS: i64 = 1_000_000;
const EVENTS_PER_VISITOR: i64 = 30;
const SITES: i64 = 5;

/// One version of the ids.
struct Version {
    name: &'static str,
    /// The types of the two columns.
    visitor: &'static str,
    visit: &'static str,
    /// The expressions that fill them.
    visitor_value: String,
    visit_value: String,
}

fn versions() -> Vec<Version> {
    // The visitor's number, and the session it is in.
    let v = format!("(i / {EVENTS_PER_VISITOR})");
    let session = format!("(i / {EVENTS_PER_VISITOR})");
    let uuid_of = |seed: &str, key: &str| {
        format!(
            "CAST(substr(sha256('{seed}' || {key}), 1, 8) || '-' || \
             substr(sha256('{seed}' || {key}), 9, 4) || '-' || \
             substr(sha256('{seed}' || {key}), 13, 4) || '-' || \
             substr(sha256('{seed}' || {key}), 17, 4) || '-' || \
             substr(sha256('{seed}' || {key}), 21, 12) AS UUID)"
        )
    };
    vec![
        Version {
            name: "16 bytes (UUID + UUID)",
            visitor: "UUID",
            visit: "UUID",
            visitor_value: uuid_of("v", &v),
            visit_value: uuid_of("s", &session),
        },
        Version {
            name: "8 bytes (SHA-256 + SHA-256)",
            visitor: "BIGINT",
            visit: "BIGINT",
            visitor_value: format!(
                "CAST(hash('v' || {v}) % 9223372036854775807 AS BIGINT)"
            ),
            // The shape v8 uses: the session's start in the high half, the
            // visitor's low 32 bits in the low half.
            visit_value: format!(
                "((((i / {EVENTS_PER_VISITOR}) * 600)::BIGINT << 32) | \
                 (hash('v' || {v}) % 4294967296))"
            ),
        },
    ]
}

/// The traffic, in the real columns.
fn traffic(version: &Version) -> String {
    format!(
        "INSERT INTO probe
         SELECT i AS id,
            'site-' || (i % {SITES}) AS website_id,
            {visit} AS visit_id,
            {visitor} AS visitor_id,
            '203.0.113.' || CAST(i % 40000 AS VARCHAR) AS ip,
            '203.0.' || CAST(i % 256 AS VARCHAR) || '.0' AS ip_prefix,
            CASE (i % 50)
                WHEN 0 THEN 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36'
                WHEN 1 THEN 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36'
                WHEN 2 THEN 'Mozilla/5.0 (iPhone; CPU iPhone OS 17_2 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.2 Mobile/15E148 Safari/604.1'
                WHEN 3 THEN 'Mozilla/5.0 (X11; Linux x86_64; rv:121.0) Gecko/20100101 Firefox/121.0'
                ELSE 'Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Mobile Safari/537.36'
            END AS user_agent,
            CASE WHEN i % 3 = 0 THEN NULL ELSE uuid() END AS user_id,
            TIMESTAMP '2026-01-01 00:00:00' + INTERVAL (i * 4) SECOND AS created_at,
            '/blog/post-' || (i % 5000) AS url,
            CASE WHEN i % 7 = 0 THEN 'utm_source=twitter&utm_campaign=spring' ELSE '' END AS url_query,
            'A reasonably long article title number ' || (i % 1000) AS title,
            CASE WHEN i % 3 = 0 THEN 'https://www.google.com/search?q=some+term&x=1' ELSE '' END AS referrer,
            CASE WHEN i % 3 = 0 THEN '/search' ELSE '' END AS referrer_path,
            '' AS referrer_query, '' AS referrer_name,
            'example.com' AS hostname, 'Search' AS channel,
            CASE (i % 40000) % 4 WHEN 0 THEN 'Chrome' WHEN 1 THEN 'Safari'
                 WHEN 2 THEN 'Firefox' ELSE 'Edge' END AS browser,
            '120.0.0.0' AS browser_version,
            CASE (i % 40000) % 4 WHEN 0 THEN 'macOS' WHEN 1 THEN 'Windows'
                 WHEN 2 THEN 'iOS' ELSE 'Linux' END AS os,
            '10.15.7' AS os_version,
            CASE (i % 40000) % 4 WHEN 0 THEN 'desktop' WHEN 1 THEN 'desktop'
                 WHEN 2 THEN 'mobile' ELSE 'tablet' END AS device,
            '1920x1080' AS screen, 'en-US' AS language, 'DE' AS country, 'Berlin' AS region,
            'Berlin' AS city, 52.52 AS latitude, 13.405 AS longitude,
            '' AS utm_source, '' AS utm_medium, '' AS utm_campaign, '' AS utm_content, '' AS utm_term,
            '' AS gclid, '' AS fbclid, '' AS msclkid, '' AS ttclid, '' AS lifatid, '' AS twclid,
            0.01 AS lcp, 0.02 AS inp, 0.03 AS cls, 0.4 AS fcp, 0.8 AS ttfb,
            'pageview' AS event_type, '' AS event_name, '' AS tag, '' AS data
        FROM generate_series(1, {{rows}}) t(i)",
        visit = version.visit_value,
        visitor = version.visitor_value,
    )
}

/// The queries a dashboard runs.
const QUERIES: [(&str, &str); 6] = [
    (
        "the summary",
        "SELECT COUNT(*), COUNT(DISTINCT visitor_id), COUNT(DISTINCT visit_id) FROM probe",
    ),
    (
        "the timeseries",
        "SELECT date_trunc('hour', created_at), COUNT(DISTINCT visitor_id), COUNT(*) \
         FROM probe GROUP BY 1 ORDER BY 1",
    ),
    (
        "the pages",
        "SELECT url, COUNT(DISTINCT visitor_id), COUNT(*) FROM probe GROUP BY 1 ORDER BY 3 DESC LIMIT 50",
    ),
    (
        "the browsers",
        "SELECT browser, COUNT(DISTINCT visitor_id), COUNT(*) FROM probe GROUP BY 1 ORDER BY 3 DESC",
    ),
    (
        "the sessions (the first 50)",
        "SELECT visit_id, COUNT(*), MIN(created_at), MAX(created_at) FROM probe \
         GROUP BY 1 ORDER BY 2 DESC LIMIT 50",
    ),
    (
        "the event log (the first 50)",
        "SELECT id, url, created_at FROM probe ORDER BY created_at DESC LIMIT 50",
    ),
];

fn time(conn: &duckdb::Connection, sql: &str) -> f64 {
    let mut best = f64::MAX;
    for _ in 0..3 {
        let started = Instant::now();
        let mut stmt = match conn.prepare(sql) {
            Ok(stmt) => stmt,
            Err(_) => return f64::NAN,
        };
        let mut rows = match stmt.query([]) {
            Ok(rows) => rows,
            Err(_) => return f64::NAN,
        };
        loop {
            match rows.next() {
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => return f64::NAN,
            }
        }
        best = best.min(started.elapsed().as_secs_f64() * 1000.0);
    }
    best
}

fn main() -> anyhow::Result<()> {
    let rows: i64 = std::env::args()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000_000);
    println!("{rows} rows, {VISITORS} visitors, {EVENTS_PER_VISITOR} events each, {SITES} sites\n");

    for version in versions() {
        let path = PathBuf::from("/tmp/id-shape-speed.duckdb");
        let _ = std::fs::remove_file(&path);
        let db = Db::open(&path, "512MB")?;
        // The real columns, with the two ids in this version's shape.
        // The real columns, with the visitor id put in where the row has it:
        // right after the visit id.
        let columns = EVENT_COLUMNS.replace(
            "visit_id        BIGINT NOT NULL,",
            &format!(
                "visit_id {} NOT NULL, visitor_id {},",
                version.visit, version.visitor
            ),
        );
        db.with_read(|conn| {
            conn.execute_batch(&format!("CREATE TABLE probe ({columns});"))?;
            Ok::<_, anyhow::Error>(())
        })?;
        let sql = traffic(&version).replace("{rows}", &rows.to_string());
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
        let bytes = std::fs::metadata(&path)?.len() as f64 / rows as f64;

        let conn = duckdb::Connection::open(&path)?;
        println!(
            "  === {} ===   {bytes:.2} B/row   the write {write:.0}ms",
            version.name
        );
        for (label, query) in QUERIES {
            let ms = time(&conn, query);
            println!("      {label:<30} {ms:>9.1}ms");
        }
        let _ = std::fs::remove_file(&path);
        println!();
    }
    Ok(())
}
