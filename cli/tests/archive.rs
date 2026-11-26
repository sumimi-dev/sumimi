//! The hot/cold tiers must agree.
//!
//! The hot tier is a DuckDB file; the cold tier is a Parquet archive. The same report
//! code runs over both, so a query against the archive has to return exactly what the
//! live database returns — that is the whole point of the tiering.

use std::path::PathBuf;

use sumimi_cli::backup;
use sumimi_service::db;
use sumimi_service::pool::Db;
use sumimi_service::storage::{ReportQuery, Storage, TimeRange};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sumimi-archive-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn the_archive_answers_the_same_as_the_database() {
    let dir = temp_dir("tiering");
    let db_path = dir.join("sumimi.duckdb");
    let archive = dir.join("archive");

    let db = Db::open(&db_path, "128MB").unwrap();
    // The site's file is attached — the site was registered after the database was
    // opened, so its file is not there yet.
    let alias = db.ensure_site("w1").unwrap();
    db.with_write(|conn| {
        conn.execute(
            "INSERT INTO website (id, name, domain, created_at, updated_at, retention_days, country_header)
             VALUES ('w1', 'Example', 'example.com', now(), now(), 0, NULL)",
            [],
        )?;
        // **The facts of a site live in a file of that site's own** (since v12),
        // so they are written into the site's file — under the site's alias.
        // One set-based insert: 50 000 individual prepares would take minutes.
        //
        // Both ids are UUIDs (schema v10): a text id is hashed into 16 bytes.
        conn.execute_batch(&format!(
            r#"
CREATE OR REPLACE MACRO uuid_of(s) AS CAST(
    substr(sha256(s), 1, 8) || '-' || substr(sha256(s), 9, 4) || '-' ||
    substr(sha256(s), 13, 4) || '-' || substr(sha256(s), 17, 4) || '-' ||
    substr(sha256(s), 21, 12) AS UUID);

CREATE OR REPLACE MACRO session_of(n) AS uuid_of('session-' || n);

CREATE OR REPLACE MACRO event_of(n) AS uuid_of('event-' || n);

-- The visitor's id is a **blob of eight bytes** (see `crate::ua`), so the
-- seed takes the first eight bytes of the hash — sixteen hex characters.
CREATE OR REPLACE MACRO visitor_of(n) AS
    unhex(substr(sha256('visitor-' || (n)::VARCHAR), 1, 16));

INSERT INTO {alias}.website_event (
    event_id, website_id, visit_id, session_id, ip, ip_prefix, user_agent, created_at, url_path, url_query, page_title,
    referrer_domain, referrer_path, referrer_query, hostname,
    browser, browser_version, os, os_version, device, screen, language,
    country, region, city,
    utm_source, utm_medium, utm_campaign, utm_content, utm_term,
    gclid, fbclid, msclkid, ttclid, li_fat_id, twclid,
    lcp, inp, cls, fcp, ttfb,
    event_type, event_name, tag, ver)
SELECT
  event_of(i),
  'w1',
  session_of(1 + (i / 2)),
  visitor_of(i % 120),
  '203.0.113.' || (i % 120)::VARCHAR,
  '203.0.113',
  'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36',
  now()::TIMESTAMP - INTERVAL (i % 20000) MINUTE,
  '/page/' || (i % 7)::VARCHAR,
  'utm_source=twitter', 'A page title',
  'news.ycombinator.com', '/item/1', '?id=1', 'example.com',
  'Chrome', '120', 'macOS', '10', 'desktop', 'en-US', '1920x1080',
  'DE', NULL, NULL,
  'twitter', 'social', 'spring', 'cta', 'analytics',
  NULL, NULL, NULL, NULL, NULL, NULL,
  1200.0, 80.0, 0.02, 700.0, 90.0,
  'pageview', NULL, NULL, NULL
FROM generate_series(1, 50000) t(i);
"#
        ))?;
        conn.execute(
            &format!(
                "INSERT INTO {alias}.website_event (event_id, website_id, visit_id, session_id, ip, ip_prefix, user_agent,
                    created_at, url_path, hostname,
                    event_type, event_name, tag, ver)
             VALUES (event_of(999999), 'w1', session_of(1), visitor_of(1), '203.0.113.7', '203.0.113',
                     'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36',
                     now()::TIMESTAMP, '/signup',
                     'example.com', 'event', 'signup', 'cta', NULL)"
            ),
            [],
        )?;
        Ok(())
    })
    .unwrap();

    // The window that covers the data we just wrote.
    let range = TimeRange {
        start: chrono::Utc::now().naive_utc() - chrono::Duration::days(2),
        end: chrono::Utc::now().naive_utc() + chrono::Duration::days(1),
    };
    let mut query = ReportQuery::new("w1", range);
    query.limit = 50;

    let hot = db
        .summary(&query)
        .unwrap();

    // Archive it.
    let t1 = std::time::Instant::now();
    let stats = db
        .with_write(|conn| {
            backup::export(
                conn,
                &archive,
                None,
                Some(range.start.date()),
                Some(range.end.date()),
                false,
            )
        })
        .unwrap();
    eprintln!("export took {:.1}s", t1.elapsed().as_secs_f64());
    assert!(stats.files > 0, "the archive should contain files");

    // Read it back and ask the same question.
    let cold_conn = backup::open_archive(&format!("{}/", archive.display())).unwrap();
    let cold = cold_conn.summary(&query).unwrap();

    assert_eq!(hot.pageviews, cold.pageviews, "pageviews differ");
    assert_eq!(hot.visitors, cold.visitors, "visitors differ");
    assert_eq!(hot.views, cold.views, "sessions differ");
    assert_eq!(hot.bounces, cold.bounces, "bounces differ");

    // The archive is smaller than the database it came from (per-day Parquet files
    // with ZSTD beat DuckDB's row-group layout once there is real data in them).
    let archive_size: u64 = std::fs::read_dir(archive.join("events"))
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.metadata().ok())
        .map(|meta| meta.len())
        .sum();
    // **The facts are in the site's file**, not in the main one — so that is the
    // file the archive is compared with.
    let site_file = db::site_path(&db_path, "w1", "duckdb");
    let db_size = std::fs::metadata(&site_file).unwrap().len();
    assert!(
        archive_size < db_size,
        "the archive ({archive_size} bytes) should be smaller than the site's file ({db_size} bytes)"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
