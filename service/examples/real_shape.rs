//! The real schema, measured: what does one row cost, and what does the
//! identity's width add?
//!
//! The storage numbers elsewhere were measured on a test table with a
//! plausible-but-invented column set. This one uses **the real
//! `EVENT_COLUMNS`** (the same constant the service creates its table with),
//! filled with plausible traffic — a realistic spread of user agents, urls,
//! referrers, sites — and measures what a row costs with the identity column
//! at three widths.
//!
//! It then answers the question the numbers are for: **how big is a
//! deployment with a billion visitors, and what does the width cost there?**
//!
//! ```bash
//! cargo run --release --example real_shape -p sumimi-service -- 10000000
//! ```

use std::path::PathBuf;
use std::time::Instant;

use sumimi_service::db::EVENT_COLUMNS;
use sumimi_service::pool::Db;

/// The traffic: a plausible spread.
const VISITORS: i64 = 1_000_000;
const SITES: i64 = 5;

/// The three shapes of the identity column.
struct Shape {
    name: &'static str,
    /// The column type, and the expression that fills it.
    ddl: &'static str,
    values: String,
}

fn shapes() -> Vec<Shape> {
    let narrow = "CAST(hash('v' || (i % {VISITORS})) % 9223372036854775807 AS BIGINT)".to_string();
    let wide = "CAST(hash('a' || (i % {VISITORS})) % 9223372036854775807 AS HUGEINT) * \
                9223372036854775808::HUGEINT + \
                CAST(hash('b' || (i % {VISITORS})) % 9223372036854775807 AS HUGEINT)"
        .to_string();
    vec![
        Shape {
            name: "visitor_id BIGINT (8 bytes)",
            ddl: "visitor_id BIGINT",
            values: narrow.clone(),
        },
        Shape {
            name: "visitor_id HUGEINT (16 bytes, real 128-bit values)",
            ddl: "visitor_id HUGEINT",
            values: wide.clone(),
        },
        Shape {
            name: "visitor_id HUGEINT (8-byte values in a wide column)",
            ddl: "visitor_id HUGEINT",
            values: narrow,
        },
    ]
}

/// The traffic, in the real columns.
fn traffic(visitor: &str, save_all: bool) -> String {
    let ip = if save_all {
        "'203.0.113.' || CAST(i % 40000 AS VARCHAR)".to_string()
    } else {
        "''".to_string()
    };
    let ua = if save_all {
        "CASE (i % 50)
                WHEN 0 THEN 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36'
                WHEN 1 THEN 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36'
                WHEN 2 THEN 'Mozilla/5.0 (iPhone; CPU iPhone OS 17_2 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.2 Mobile/15E148 Safari/604.1'
                WHEN 3 THEN 'Mozilla/5.0 (X11; Linux x86_64; rv:121.0) Gecko/20100101 Firefox/121.0'
                ELSE 'Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Mobile Safari/537.36'
            END"
            .to_string()
    } else {
        "''".to_string()
    };
    format!(
        "INSERT INTO probe
         SELECT i AS id,
            'site-' || (i % {SITES}) AS website_id,
            (((i // {VISITORS}) // 4)::BIGINT << 32) | (hash('v' || (i % {VISITORS})) % 4294967296) AS visit_id,
            {visitor} AS visitor_id,
            {ip} AS ip,
            '203.0.' || CAST(i % 256 AS VARCHAR) || '.0' AS ip_prefix,
            {ua} AS user_agent,
            CASE WHEN i % 3 = 0 THEN NULL ELSE uuid() END AS user_id,
            TIMESTAMP '2026-01-01 00:00:00' + INTERVAL (i * 4) SECOND AS created_at,
            '/blog/post-' || (i % 5000) AS url,
            CASE WHEN i % 7 = 0 THEN 'utm_source=twitter&utm_campaign=spring' ELSE '' END AS url_query,
            'A reasonably long article title number ' || (i % 1000) AS title,
            CASE WHEN i % 3 = 0 THEN 'https://www.google.com/search?q=some+term&x=1' ELSE '' END AS referrer,
            CASE WHEN i % 3 = 0 THEN '/search' ELSE '' END AS referrer_path,
            '' AS referrer_query, '' AS referrer_name,
            'example.com' AS hostname, 'Search' AS channel,
            CASE (i % {VISITORS}) % 4 WHEN 0 THEN 'Chrome' WHEN 1 THEN 'Safari'
                 WHEN 2 THEN 'Firefox' ELSE 'Edge' END AS browser,
            '120.0.0.0' AS browser_version,
            CASE (i % {VISITORS}) % 4 WHEN 0 THEN 'macOS' WHEN 1 THEN 'Windows'
                 WHEN 2 THEN 'iOS' ELSE 'Linux' END AS os,
            '10.15.7' AS os_version,
            CASE (i % {VISITORS}) % 4 WHEN 0 THEN 'desktop' WHEN 1 THEN 'desktop'
                 WHEN 2 THEN 'mobile' ELSE 'tablet' END AS device,
            '1920x1080' AS screen, 'en-US' AS language, 'DE' AS country, 'Berlin' AS region,
            'Berlin' AS city, 52.52 AS latitude, 13.405 AS longitude,
            '' AS utm_source, '' AS utm_medium, '' AS utm_campaign, '' AS utm_content, '' AS utm_term,
            '' AS gclid, '' AS fbclid, '' AS msclkid, '' AS ttclid, '' AS lifatid, '' AS twclid,
            0.01 AS lcp, 0.02 AS inp, 0.03 AS cls, 0.4 AS fcp, 0.8 AS ttfb,
            'pageview' AS event_type, '' AS event_name, '' AS tag, '' AS data
        FROM generate_series(1, {{rows}}) t(i)"
    )
}

fn main() -> anyhow::Result<()> {
    let rows: i64 = std::env::args()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000_000);
    println!("{rows} rows, the real EVENT_COLUMNS ({SITES} sites, {VISITORS} visitors)\n");
    println!(
        "  {:<48} {:>9} {:>12} {:>12}",
        "the shape", "B/row", "the column", "1B visitors"
    );

    let mut per_row = Vec::new();
    for save_all in [false, true] {
    for shape in shapes() {
        let path = PathBuf::from("/tmp/real-shape.duckdb");
        let _ = std::fs::remove_file(&path);
        let db = Db::open(&path, "512MB")?;
        // The real columns, plus the identity column.
        let columns = EVENT_COLUMNS.replace("visit_id        BIGINT NOT NULL,", &format!("visit_id BIGINT NOT NULL, {},", shape.ddl));
        db.with_read(|conn| {
            conn.execute_batch(&format!("CREATE TABLE probe ({columns});"))?;
            Ok::<_, anyhow::Error>(())
        })?;
        let sql = traffic(&shape.values, save_all)
            .replace("{VISITORS}", &VISITORS.to_string())
            .replace("{SITES}", &SITES.to_string())
            .replace("{rows}", &rows.to_string());
        let started = Instant::now();
        db.with_write(|conn| {
            conn.execute_batch(&sql)?;
            Ok::<_, anyhow::Error>(())
        })?;
        let write = started.elapsed().as_secs_f64() * 1000.0;
        let count: i64 = db.with_read(|conn| {
            let v: i64 = conn.query_row("SELECT count(*) FROM probe", [], |r| r.get(0))?;
            Ok::<_, anyhow::Error>(v)
        })?;
        let before_checkpoint = std::fs::metadata(&path)?.len();
        db.with_read(|conn| {
            conn.execute_batch("CHECKPOINT")?;
            Ok::<_, anyhow::Error>(())
        })?;
        drop(db);
        let bytes = std::fs::metadata(&path)?.len() as f64 / rows as f64;
        let _ = std::fs::remove_file(&path);
        let name = if save_all {
            format!("{}  [--save all]", shape.name)
        } else {
            format!("{}  [default]", shape.name)
        };
        per_row.push((name, bytes, write, count, before_checkpoint));
    }
    }

    let base = per_row[0].1;
    let base_all = per_row[3].1;
    for (name, bytes, write, count, _before_checkpoint) in &per_row {
        // A billion visitors, at 20 events each.
        let events = 20_000_000_000i64;
        let base = if name.contains("save all") { base_all } else { base };
        println!(
            "  {:<58} {count:>10} rows | {bytes:>9.2} B/row | the column {:>6.2} B | {:>6.0} GB at 20B events | \
             (the write {write:.0}ms)",
            name,
            bytes - base,
            bytes * events as f64 / 1_073_741_824.0,
        );
    }
    Ok(())
}
