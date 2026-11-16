//! How much memory does the primary key's index need?
//!
//! DuckDB keeps an index **in memory**, always. The ART index of a primary key
//! holds one entry per row — so a table with hundreds of millions of rows needs
//! an index that does not fit on a small VPS, no matter how small the data is.
//!
//! This writes rows through the **same Appender path the server uses** and prints
//! the peak resident memory, so the two schemas can be compared:
//!
//! ```bash
//! /usr/bin/time -l cargo run --release --example idx_memory -p sumimi-service -- 20000000 pk
//! /usr/bin/time -l cargo run --release --example idx_memory -p sumimi-service -- 20000000 nopk
//! ```
//!
//! The third argument picks the write path: `appender` (the server) or `bulk`
//! (one big `INSERT ... SELECT`, which is what the migration and the seed do).
//! The fourth is the DuckDB memory limit.
//!
//! **The shape it measures is the flat one** — one file, one table — because
//! that is the shape of the *write path*: what costs memory is the row that
//! goes in, not where the file sits. (The server's shape is one file per site
//! since v12 — see `D-046` and `examples/site_split.rs`.)

use std::time::Instant;

use duckdb::Connection;

/// The uuid of a text id — the way the server makes one.
///
/// ⇒ ⇒ The two ids are sixteen bytes since v10, so a measurement that writes
/// text into them measures the wrong thing.
fn uuid_of(value: &str) -> uuid::Uuid {
    sumimi_service::ua::uuid_from_text(value)
}

/// The macro the bulk statements use — DuckDB has no `uuid_of` of its own.
const UUID_MACRO: &str = r#"
CREATE OR REPLACE MACRO uuid_of(s) AS CAST(
    substr(sha256(s), 1, 8) || '-' || substr(sha256(s), 9, 4) || '-' ||
    substr(sha256(s), 13, 4) || '-' || substr(sha256(s), 17, 4) || '-' ||
    substr(sha256(s), 21, 12) AS UUID);
"#;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let rows: i64 = args.get(1).and_then(|v| v.parse().ok()).unwrap_or(20_000_000);
    let with_pk = args.get(2).map(|v| v == "pk").unwrap_or(false);
    let bulk = args.get(3).map(|v| v == "bulk").unwrap_or(false);
    let chunked = args.get(3).map(|v| v == "chunked").unwrap_or(false);
    // In chunked mode the fourth argument is the batch size and the fifth is the limit.
    let limit = if chunked {
        args.get(5).cloned().unwrap_or_else(|| "2GB".to_string())
    } else {
        args.get(4).cloned().unwrap_or_else(|| "2GB".to_string())
    };

    let path = std::env::temp_dir().join(format!(
        "sumimi-idx-memory-{rows}-{}.duckdb",
        if with_pk { "pk" } else { "nopk" }
    ));
    let _ = std::fs::remove_file(&path);

    let conn = Connection::open(&path)?;
    conn.execute_batch(&format!("SET memory_limit='{limit}'; SET threads=2;"))?;
    conn.execute_batch(UUID_MACRO)?;

    // The same columns — the row id spelled with a primary key, or without.
    let plain_ddl = sumimi_service::db::event_columns_ddl();
    let pk_ddl = sumimi_service::schema::columns_ddl_with(
        sumimi_service::schema::EVENT,
        &sumimi_service::db::Duck,
        &[("event_id", "event_id BIGINT PRIMARY KEY")],
    );
    let ddl = if with_pk { pk_ddl.clone() } else { plain_ddl.clone() };
    conn.execute_batch(&format!("CREATE TABLE website_event ({ddl});"))?;

    println!(
        "{} rows, {} the primary key, {} path, {limit} limit",
        rows,
        if with_pk { "with" } else { "without" },
        if bulk { "bulk" } else { "appender" }
    );

    let started = Instant::now();

    if bulk {
        // One statement: this is what the migration and the seed do.
        let result = conn.execute_batch(&format!(
            "INSERT INTO website_event\n             (event_id, website_id, visit_id, session_id, session_start, created_at,
             url_path, url_query, page_title, referrer_domain, referrer_path, referrer_query,
             referrer_name, hostname, channel, browser, browser_version, os, os_version,
             device, screen, language, country, region, city, latitude, longitude,
             utm_source, utm_medium, utm_campaign, utm_content, utm_term,
             gclid, fbclid, msclkid, ttclid, li_fat_id, twclid,
             lcp, inp, cls, fcp, ttfb, event_type, event_name, tag, data)\n             SELECT
               i,
               'w1',
               CAST(uuid_of('session-' || (i / 3)::VARCHAR) AS UUID),
               CAST(uuid_of('visitor-' || (i % 300000)::VARCHAR) AS UUID),
               NULL,
               TIMESTAMP '2024-01-01' + to_seconds((i * 7919) % 31536000),
               '/blog/post-1',
               'utm_source=twitter&utm_medium=cpc&utm_campaign=spring&utm_content=cta&utm_term=term',
               'A title',
               'news.ycombinator.com',
               '/item',
               '?q=search',
               NULL,
               'example.com',
               NULL,
               'Chrome',
               '120',
               'macOS',
               '10',
               'desktop',
               '1920x1080',
               'en-US',
               'US',
               NULL,
               NULL,
               NULL,
               NULL,
               'twitter',
               'social',
               'spring',
               NULL,
               NULL,
               NULL, NULL, NULL, NULL, NULL, NULL,
               1200.0,
               80.0,
               0.02,
               700.0,
               90.0,
               'pageview',
               NULL,
               NULL,
               NULL
             FROM generate_series(1, {rows}) t(i);"
        ));
        match result {
            Ok(_) => println!("  bulk insert ok"),
            Err(err) => {
                println!("  bulk insert FAILED: {err}");
                let _ = std::fs::remove_file(&path);
                return Ok(());
            }
        }
        let seconds = started.elapsed().as_secs_f64();
        conn.execute_batch("CHECKPOINT")?;
        let size = std::fs::metadata(&path)?.len();
        println!("  wrote in {seconds:.1}s, file {:.1} MB", size as f64 / 1e6);
        let _ = std::fs::remove_file(&path);
        return Ok(());
    }

    if args.get(3).map(|v| v == "chunked").unwrap_or(false) {
        // The third way: the same bulk copy, but in batches instead of one
        // statement. This is what "keep the primary key" would have to do.
        let batch: i64 = args.get(4).and_then(|v| v.parse().ok()).unwrap_or(1_000_000);
        let started = Instant::now();
        let mut from = 1i64;
        let mut ok = true;
        while from <= rows {
            let to = (from + batch - 1).min(rows);
            let result = conn.execute_batch(&format!(
                "INSERT INTO website_event\n                 (event_id, website_id, visit_id, session_id, session_start, created_at,
                 url_path, url_query, page_title, referrer_domain, referrer_path, referrer_query,
                 referrer_name, hostname, channel, browser, browser_version, os, os_version,
                 device, screen, language, country, region, city, latitude, longitude,
                 utm_source, utm_medium, utm_campaign, utm_content, utm_term,
                 gclid, fbclid, msclkid, ttclid, li_fat_id, twclid,
                 lcp, inp, cls, fcp, ttfb, event_type, event_name, tag, data)\n                 SELECT\n                   i,\n                   'w1',\n                   CAST(uuid_of('session-' || (i / 3)::VARCHAR) AS UUID),\n                   CAST(uuid_of('visitor-' || (i % 300000)::VARCHAR) AS UUID),\n                   NULL,\n                   TIMESTAMP '2024-01-01' + to_seconds((i * 7919) % 31536000),\n                   '/blog/post-1', 'utm_source=twitter&utm_medium=cpc&utm_campaign=spring&utm_content=cta&utm_term=term',\n                   'A title', 'news.ycombinator.com', '/item', '?q=search', NULL, 'example.com', NULL,\n                   'Chrome', '120', 'macOS', '10', 'desktop', '1920x1080', 'en-US', 'US',\n                   NULL, NULL, NULL, NULL, 'twitter', 'social', 'spring', NULL, NULL,\n                   NULL, NULL, NULL, NULL, NULL, NULL,\n                   1200.0, 80.0, 0.02, 700.0, 90.0, 'pageview', NULL, NULL, NULL\n                 FROM generate_series({from}, {to}) t(i);"
            ));
            if let Err(err) = result {
                println!("  chunked insert FAILED at row {from}: {err}");
                ok = false;
                break;
            }
            from = to + 1;
        }
        if ok {
            let seconds = started.elapsed().as_secs_f64();
            conn.execute_batch("CHECKPOINT")?;
            let size = std::fs::metadata(&path)?.len();
            println!("  chunked insert ok in {seconds:.1}s, file {:.1} MB", size as f64 / 1e6);
        }
        let _ = std::fs::remove_file(&path);
        return Ok(());
    }

    if args.get(3).map(|v| v == "buildsrc").unwrap_or(false) {
        // Build the source database (with a primary key) in its own process, so the
        // memory of the migration copy can be measured on its own.
        let source = std::env::temp_dir().join(format!("sumimi-migrate-src-{rows}.duckdb"));
        let _ = std::fs::remove_file(&source);
        let src = Connection::open(&source)?;
        src.execute_batch("SET memory_limit='2GB';")?;
        src.execute_batch(UUID_MACRO)?;
        src.execute_batch(&format!("CREATE TABLE website_event ({pk_ddl});"))?;
        src.execute_batch(&format!(
            "INSERT INTO website_event\n             (event_id, website_id, visit_id, session_id, session_start, created_at,
             url_path, url_query, page_title, referrer_domain, referrer_path, referrer_query,
             referrer_name, hostname, channel, browser, browser_version, os, os_version,
             device, screen, language, country, region, city, latitude, longitude,
             utm_source, utm_medium, utm_campaign, utm_content, utm_term,
             gclid, fbclid, msclkid, ttclid, li_fat_id, twclid,
             lcp, inp, cls, fcp, ttfb, event_type, event_name, tag, data)\n             SELECT
               i,
               'w1',
               CAST(uuid_of('session-' || (i / 3)::VARCHAR) AS UUID),
               CAST(uuid_of('visitor-' || (i % 300000)::VARCHAR) AS UUID),
               NULL,
               TIMESTAMP '2024-01-01' + to_seconds((i * 7919) % 31536000),
               '/blog/post-1',
               'utm_source=twitter&utm_medium=cpc&utm_campaign=spring&utm_content=cta&utm_term=term',
               'A title',
               'news.ycombinator.com',
               '/item',
               '?q=search',
               NULL,
               'example.com',
               NULL,
               'Chrome',
               '120',
               'macOS',
               '10',
               'desktop',
               '1920x1080',
               'en-US',
               'US',
               NULL,
               NULL,
               NULL,
               NULL,
               'twitter',
               'social',
               'spring',
               NULL,
               NULL,
               NULL, NULL, NULL, NULL, NULL, NULL,
               1200.0,
               80.0,
               0.02,
               700.0,
               90.0,
               'pageview',
               NULL,
               NULL,
               NULL
             FROM generate_series(1, {rows}) t(i);"
        ))?;
        src.execute_batch("CHECKPOINT")?;
        println!("  source built: {} MB", std::fs::metadata(&source)?.len() as f64 / 1e6);
        return Ok(());
    }

    if args.get(3).map(|v| v == "migrate").unwrap_or(false) {
        // The schema migration: the source has a primary key, the destination does
        // not. This is what `db::rebuild_to_new_file` does.
        let source = std::env::temp_dir().join(format!("sumimi-migrate-src-{rows}.duckdb"));
        let dest = std::env::temp_dir().join(format!("sumimi-migrate-dst-{rows}.duckdb"));
        let _ = std::fs::remove_file(&dest);
        // Exactly what `db::rebuild_to_new_file` does: open the old database,
        // attach the new file, copy across.
        let conn2 = Connection::open(&source)?;
        conn2.execute_batch(&format!("SET memory_limit='{limit}'; SET threads=2;"))?;
        let escaped = dest.to_string_lossy().replace('\'', "''");
        conn2.execute_batch(&format!("ATTACH '{escaped}' AS fresh"))?;
        conn2.execute_batch(&format!("CREATE TABLE fresh.website_event ({plain_ddl});"))?;
        let started = std::time::Instant::now();
        let result =
            conn2.execute_batch("INSERT INTO fresh.website_event SELECT * FROM main.website_event;");
        match result {
            Ok(_) => {
                let seconds = started.elapsed().as_secs_f64();
                conn2.execute_batch("DETACH fresh; CHECKPOINT;")?;
                let size = std::fs::metadata(&dest)?.len();
                println!("  migration copy ok in {seconds:.1}s, file {:.1} MB", size as f64 / 1e6);
            }
            Err(err) => println!("  migration copy FAILED: {err}"),
        }
        let _ = std::fs::remove_file(&source);
        let _ = std::fs::remove_file(&dest);
        return Ok(());
    }

    // The server's own write path: the Appender, in batches.
    let mut id = 0i64;
    while id < rows {
        let mut appender = conn.appender("website_event")?;
        let batch = 50_000.min(rows - id);
        for _ in 0..batch {
            // The values are **positional**, so they are in the schema's order.
            appender.append_row(duckdb::params![
                id,
                "w1",
                uuid_of(&format!("session-{}", id / 3)),
                uuid_of(&format!("visitor-{:08x}", id % 300_000)),
                Option::<chrono::NaiveDateTime>::None,
                "",
                "",
                "",
                Option::<uuid::Uuid>::None,
                chrono::NaiveDate::from_ymd_opt(2024, 1, 1).unwrap().and_hms_opt(0, 0, 0).unwrap()
                    + chrono::Duration::seconds((id * 7919) % 31_536_000),
                "/blog/post-1",
                "utm_source=twitter&utm_medium=cpc&utm_campaign=spring&utm_content=cta&utm_term=term",
                "A title",
                "news.ycombinator.com",
                "/item",
                "?q=search",
                Option::<String>::None,
                "example.com",
                Option::<String>::None,
                "Chrome",
                "120",
                "macOS",
                "10",
                "desktop",
                "1920x1080",
                "en-US",
                "US",
                Option::<String>::None,
                Option::<String>::None,
                Option::<String>::None,
                Option::<f64>::None,
                Option::<f64>::None,
                "twitter",
                "social",
                "spring",
                Option::<String>::None,
                Option::<String>::None,
                Option::<String>::None,
                Option::<String>::None,
                Option::<String>::None,
                Option::<String>::None,
                Option::<String>::None,
                1200.0f64,
                80.0f64,
                0.02f64,
                700.0f64,
                90.0f64,
                "pageview",
                Option::<String>::None,
                Option::<String>::None,
                Option::<String>::None,
            ])?;
            id += 1;
        }
        appender.flush()?;
    }
    let seconds = started.elapsed().as_secs_f64();

    conn.execute_batch("CHECKPOINT")?;
    let size = std::fs::metadata(&path)?.len();
    println!("  wrote in {seconds:.1}s, file {:.1} MB", size as f64 / 1e6);

    let _ = std::fs::remove_file(&path);
    Ok(())
}
