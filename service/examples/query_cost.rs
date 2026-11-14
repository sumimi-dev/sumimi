//! Does dropping the `PRIMARY KEY` cost anything at query time?
//!
//! The index of a primary key is never used by our queries — every one of them is a
//! filtered scan or an aggregate, and DuckDB prunes those with zone maps. But
//! "never used" is a claim; this measures it: the **same data** in two databases,
//! one with a primary key and one without, and every query the API serves timed on
//! both.
//!
//! ```bash
//! cargo run --release --example query_cost -p sumimi-service -- 20000000
//! ```

use std::time::Instant;

use duckdb::Connection;
use sumimi_service::pool::Db;
use sumimi_service::storage::{Dimension, ReportQuery, Storage, TimeRange};

/// How many rows to write.
const ROWS: i64 = 20_000_000;

fn main() -> anyhow::Result<()> {
    let rows: i64 = std::env::args()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(ROWS);
    let dir = std::env::temp_dir().join("sumimi-query-cost");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;

    println!("{rows} page views, the same data in two databases\n");

    let with_pk = build(&dir.join("with_pk.duckdb"), true, rows)?;
    let without = build(&dir.join("no_pk.duckdb"), false, rows)?;

    println!(
        "{:<26} {:>12} {:>12} {:>9}",
        "query", "with PK", "without", "ratio"
    );
    println!("{}", "-".repeat(63));

    // Every dimension the API serves.
    let dimensions = [
        ("pages", Dimension::Url),
        ("referrers", Dimension::Referrer),
        ("browsers", Dimension::Browser),
        ("os", Dimension::Os),
        ("devices", Dimension::Device),
        ("countries", Dimension::Country),
        ("languages", Dimension::Language),
        ("screens", Dimension::Screen),
        ("urls", Dimension::Url),
    ];

    let range = TimeRange {
        start: chrono::NaiveDate::from_ymd_opt(2024, 12, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap(),
        end: chrono::NaiveDate::from_ymd_opt(2024, 12, 31)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap(),
    };
    let mut query = ReportQuery::new("w1", range);
    query.limit = 50;

    let mut comparisons: Vec<(&str, f64, f64)> = Vec::new();

    macro_rules! compare {
        ($label:expr, $body:expr) => {{
            let a = best(&with_pk, $body);
            let b = best(&without, $body);
            comparisons.push(($label, a, b));
        }};
    }

    compare!("summary", &|db: &Db| {
        let _ = db.summary(&query)?;
        Ok::<_, anyhow::Error>(())
    });
    compare!("summary_with_change", &|db: &Db| {
        let _ = db.summary_with_change(&query)?;
        Ok::<_, anyhow::Error>(())
    });
    compare!("timeseries", &|db: &Db| {
        let _ = db.timeseries(&query)?;
        Ok::<_, anyhow::Error>(())
    });
    for (label, dimension) in dimensions {
        compare!(label, &|db: &Db| {
            let _ = db.breakdown(&query, dimension)?;
            Ok::<_, anyhow::Error>(())
        });
    }
    compare!("events", &|db: &Db| {
        let _ = db.events(&query)?;
        Ok::<_, anyhow::Error>(())
    });
    compare!("sessions", &|db: &Db| {
        let _ = db.sessions(&query)?;
        Ok::<_, anyhow::Error>(())
    });
    compare!("event log", &|db: &Db| {
        let _ = db.event_log(&query)?;
        Ok::<_, anyhow::Error>(())
    });

    for (label, a, b) in &comparisons {
        println!(
            "{:<26} {:>10.1} ms {:>10.1} ms {:>8.2}x",
            label,
            a * 1000.0,
            b * 1000.0,
            a / b.max(0.0001)
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// Run a query three times and keep the fastest — the noise is what we are after.
fn best(db: &Db, body: &dyn Fn(&Db) -> anyhow::Result<()>) -> f64 {
    let mut best = f64::MAX;
    for _ in 0..3 {
        let started = Instant::now();
        let _ = body(db);
        let seconds = started.elapsed().as_secs_f64();
        if seconds < best {
            best = seconds;
        }
    }
    best
}

/// Build a database with (or without) the primary key, filled with the same data.
fn build(path: &std::path::Path, pk: bool, rows: i64) -> anyhow::Result<Db> {
    let conn = Connection::open(path)?;
    // Build with headroom: the primary key's ART index is held in memory while the
    // rows are written, so the write path needs more room than the query path.
    conn.execute_batch(
        "SET memory_limit='2GB';
         SET threads=2;",
    )?;

    let ddl = if pk {
        sumimi_service::schema::columns_ddl_with(
            sumimi_service::schema::EVENT,
            &sumimi_service::db::Duck,
            &[("id", "id BIGINT PRIMARY KEY")],
        )
    } else {
        sumimi_service::db::event_columns_ddl()
    };
    conn.execute_batch(&format!("CREATE TABLE event ({ddl});"))?;

    conn.execute_batch(&format!(
        r#"
INSERT INTO event (
    id, website_id, session_id, visitor_id, created_at,
    url, url_query, title, referrer, referrer_path, hostname,
    browser, browser_version, os, os_version, device, screen, language, country,
    utm_source, utm_medium, utm_campaign,
    lcp, inp, cls, fcp, ttfb,
    event_type
)
SELECT
  i,
  'w1',
  md5((1 + (i / 3))::text || '-s'),
  md5((i % 300000)::text || '-v'),
  TIMESTAMP '2024-01-01' + to_seconds((i * 7919) % 31536000),
  '/blog/post-' || (i % 500000)::VARCHAR || '-slug-' || (i % 977)::VARCHAR,
  'utm_source=twitter&utm_medium=cpc&utm_campaign=campaign-' || (i % 200)::VARCHAR,
  'A blog post about topic number ' || (i % 100000)::VARCHAR || ' — with a longer title',
  (ARRAY['news.ycombinator.com','www.google.com','github.com','x.com'])[1 + (i % 4)],
  '/search?q=some+search+term+number+' || (i % 2000000)::VARCHAR,
  'example.com',
  (ARRAY['Chrome','Safari','Firefox','Edge'])[1 + (i % 4)],
  '120',
  (ARRAY['Windows','macOS','iOS','Android','Linux'])[1 + (i % 5)::INT],
  '10',
  (ARRAY['desktop','mobile','tablet'])[1 + (i % 3)::INT],
  (ARRAY['1920x1080','1536x864','390x844'])[1 + (i % 4)::INT],
  (ARRAY['en-US','zh-CN','en-GB','ja-JP'])[1 + (i % 4)::INT],
  (ARRAY['US','CN','DE','GB','JP','IN','FR','BR'])[1 + (i % 8)::INT],
  CASE WHEN i % 4 = 0 THEN 'twitter' ELSE NULL END,
  CASE WHEN i % 4 = 0 THEN 'social' ELSE NULL END,
  'campaign-' || (i % 50)::VARCHAR,
  600.0 + (i % 1800), 30.0 + (i % 260), (i % 100) / 1000.0,
  400.0 + (i % 900), 60.0 + (i % 220),
  'pageview'
FROM generate_series(1, {rows}) t(i);
"#
    ))?;
    conn.execute_batch("CHECKPOINT")?;

    let size = std::fs::metadata(path)?.len();
    println!(
        "  {}: {:.1} MB ({:.1} B/row)",
        if pk { "with PK   " } else { "without PK" },
        size as f64 / 1e6,
        size as f64 / rows as f64
    );

    let db = Db::from_connection(path, conn);
    // Now the production budget: 128 MB, two threads.
    db.with_connection(|conn| {
        conn.execute_batch("SET memory_limit='128MB'; SET threads=2;")?;
        Ok::<_, anyhow::Error>(())
    })?;
    Ok(db)
}
