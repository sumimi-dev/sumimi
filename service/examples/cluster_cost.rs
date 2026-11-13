//! What does the identity column really cost? It depends on how the rows sit.
//!
//! The identity column's compression is RLE — repeated values in a row run
//! cost the value once plus the run length. So its cost depends on **how
//! adjacent a visitor's events are**:
//!
//! - **clustered**: one visitor's events land next to each other (what a real
//!   single-site burst looks like),
//! - **interleaved**: five sites' traffic lands in the same table, so a
//!   visitor's rows are broken up by other visitors',
//! - **scattered**: maximally apart — what the other measurements used.
//!
//! Each layout is measured with the identity at 8 bytes and at 16 bytes; the
//! difference is what the width decision costs.
//!
//! ```bash
//! cargo run --release --example cluster_cost -p sumimi-service -- 10000000
//! ```

use std::path::{Path, PathBuf};
use std::time::Instant;

use sumimi_service::db::EVENT_COLUMNS;
use sumimi_service::pool::Db;

const VISITORS: i64 = 1_000_000;
const EVENTS_PER_VISITOR: i64 = 30;
const SITES: i64 = 5;

/// How the rows sit.
#[derive(Clone, Copy)]
struct Layout {
    name: &'static str,
    /// The expression that gives the row's visitor number.
    visitor: &'static str,
}

fn layouts() -> Vec<Layout> {
    vec![
        Layout {
            name: "clustered (one visitor's events are adjacent)",
            visitor: "(i / {E})",
        },
        Layout {
            name: "interleaved (five sites in one table)",
            visitor: "((i * 7) % {V})",
        },
        Layout {
            name: "scattered (maximally apart)",
            visitor: "(i % {V})",
        },
    ]
}

/// The traffic, in the real columns.
fn traffic(visitor: &str) -> String {
    format!(
        "INSERT INTO probe
         SELECT i AS id,
            'site-' || (i % {SITES}) AS website_id,
            (((i / {{E}}) / 4)::BIGINT << 32) | (hash('v' || ({visitor})) % 4294967296) AS visit_id,
            CAST(hash('v' || ({visitor})) % 9223372036854775807 AS BIGINT) AS visitor_id,
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
            CASE (i % {{V}}) % 4 WHEN 0 THEN 'Chrome' WHEN 1 THEN 'Safari'
                 WHEN 2 THEN 'Firefox' ELSE 'Edge' END AS browser,
            '120.0.0.0' AS browser_version,
            CASE (i % {{V}}) % 4 WHEN 0 THEN 'macOS' WHEN 1 THEN 'Windows'
                 WHEN 2 THEN 'iOS' ELSE 'Linux' END AS os,
            '10.15.7' AS os_version,
            CASE (i % {{V}}) % 4 WHEN 0 THEN 'desktop' WHEN 1 THEN 'desktop'
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

/// The engine's report on the identity column.
fn compression(path: &Path) -> String {
    let db = match Db::open(path, "512MB") {
        Ok(db) => db,
        Err(_) => return "?".to_string(),
    };
    let sql = "SELECT compression, COUNT(*), SUM(\"count\") FROM pragma_storage_info('probe') \
               WHERE column_name = 'visitor_id' GROUP BY 1 ORDER BY 1";
    let text: Result<String, anyhow::Error> = db.with_read(|conn| {
        let mut stmt = conn.prepare(sql)?;
        let mut rows = stmt.query([])?;
        let mut out: Vec<String> = Vec::new();
        while let Some(row) = rows.next()? {
            let how: String = row.get(0)?;
            let segments: i64 = row.get(1)?;
            let count: i64 = row.get(2)?;
            out.push(format!("{how} ({segments} segments, {count} rows)"));
        }
        Ok(out.join(", "))
    });
    match text {
        Ok(t) if !t.is_empty() => t,
        _ => "?".to_string(),
    }
}

/// Build one layout at one width, and return the bytes per row.
fn measure(path: &PathBuf, rows: i64, layout: &Layout, wide: bool) -> anyhow::Result<f64> {
    let _ = std::fs::remove_file(path);
    let db = Db::open(path, "512MB")?;
    let ddl = if wide { "visitor_id HUGEINT" } else { "visitor_id BIGINT" };
    let columns = EVENT_COLUMNS.replace(
        "visit_id        BIGINT NOT NULL,",
        &format!("visit_id BIGINT NOT NULL, {ddl},"),
    );
    db.with_read(|conn| {
        conn.execute_batch(&format!("CREATE TABLE probe ({columns});"))?;
        Ok::<_, anyhow::Error>(())
    })?;
    // The value of the identity column: the layout decides which visitor each
    // row belongs to, and the width decides how many bytes that takes.
    let visitor = if wide {
        // The high part stays under 2^40, so the value is about 103 bits:
        // it does not fit in 64 bits, and it cannot overflow a HUGEINT.
        format!(
            "(CAST(hash('a' || {v}) % 1099511627776 AS HUGEINT) * \
             9223372036854775808::HUGEINT + \
             CAST(hash('b' || {v}) % 9223372036854775807 AS HUGEINT))",
            v = layout.visitor
        )
    } else {
        format!(
            "CAST(hash('v' || {v}) % 9223372036854775807 AS BIGINT)",
            v = layout.visitor
        )
    };
    let sql = traffic(&visitor)
        .replace("{VISITORS}", &VISITORS.to_string())
        .replace("{SITES}", &SITES.to_string())
        .replace("{E}", &EVENTS_PER_VISITOR.to_string())
        .replace("{V}", &VISITORS.to_string())
        .replace("{rows}", &rows.to_string());
    if wide {
        let at = sql.find("AS visitor_id").unwrap_or(0);
        let from = at.saturating_sub(260);
        let head: String = sql.chars().skip(from).take(300).collect();
        println!("      the SQL: …{head}…");
    }
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
    let bytes = std::fs::metadata(path)?.len() as f64 / rows as f64;
    let range: String = {
        let db = Db::open(path, "512MB")?;
        let text: String = db.with_read(|conn| {
            let (lo, hi): (i64, i64) = conn.query_row(
                "SELECT min(visitor_id), max(visitor_id) FROM probe",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            Ok::<_, anyhow::Error>(format!("{lo} … {hi}"))
        })?;
        text
    };
    println!(
        "      the write {write:>7.0}ms | the column: {} | the values: {range}",
        compression(path)
    );
    let _ = std::fs::remove_file(path);
    Ok(bytes)
}

fn main() -> anyhow::Result<()> {
    let rows: i64 = std::env::args()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000_000);
    println!("{rows} rows, the real EVENT_COLUMNS, {VISITORS} visitors, {EVENTS_PER_VISITOR} events each\n");
    println!(
        "  {:<46} {:>10} {:>10} {:>12}",
        "the layout", "8 bytes", "16 bytes", "the difference"
    );
    for layout in layouts() {
        let path = PathBuf::from("/tmp/cluster-cost.duckdb");
        let narrow = measure(&path, rows, &layout, false)?;
        let wide = measure(&path, rows, &layout, true)?;
        println!(
            "  {:<46} {:>10.2} {:>10.2} {:>9.2} B/row",
            layout.name,
            narrow,
            wide,
            wide - narrow
        );
    }
    Ok(())
}
