//! Scale benchmark: how big is the data, and is a small memory limit enough?
//!
//! ```bash
//! cargo run --release -p sumimi-cli --example scale -- <rows> <memory-limit> [realistic|lean]
//! ```
//!
//! Builds a synthetic table with the **real schema** (the one
//! `sumimi_service::db::init_schema` creates, so it never drifts), then runs the
//! **actual dashboard queries** (the shared report layer) against it and times them.

use std::time::Instant;

use duckdb::Connection;
use sumimi_service::pool::Db;
use sumimi_service::storage::{Dimension, ReportQuery, Storage, TimeRange};

/// A UUID from a text expression — the same shape the server hands out.
fn uuid(text: &str) -> String {
    format!(
        "CAST(substr(md5({text}), 1, 8) || '-' || substr(md5({text}), 9, 4) || '-' || \
         substr(md5({text}), 13, 4) || '-' || substr(md5({text}), 17, 4) || '-' || \
         substr(md5({text}), 21, 12) AS UUID)"
    )
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let rows: i64 = args.get(1).and_then(|v| v.parse().ok()).unwrap_or(10_000_000);
    let memory_limit = args.get(2).cloned().unwrap_or_else(|| "128MB".to_string());
    // "realistic" gives every row its own URL, referrer and title — the cardinality a
    // real site has. The default is the low-cardinality case (a handful of pages).
    let realistic = args.get(3).map(|v| v == "realistic").unwrap_or(false);
    // "lean" drops the page title, keeps only the referrer's host and drops the version
    // columns — the three fields a long-term archive does not need.
    let lean = args.get(3).map(|v| v == "lean").unwrap_or(false);
    // The file name carries the parameters, so two runs never share a database.
    let path = std::env::temp_dir().join(format!(
        "sumimi-scale-{rows}-{memory_limit}-{}.duckdb",
        args.get(3).map(String::as_str).unwrap_or("default")
    ));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("duckdb.wal"));

    let conn = Connection::open(&path)?;
    conn.execute_batch(&format!("SET memory_limit='{memory_limit}'"))?;
    conn.execute_batch("SET checkpoint_threshold='256MB'")?;
    // The thread count is derived from the memory budget, exactly like the server
    // does it: a parallel hash aggregate needs one hash table per thread.
    let threads = sumimi_service::config::threads_for(
        &memory_limit,
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
    );
    conn.execute_batch(&format!("SET threads={threads}"))?;

    // The real schema: one row per page view, no index on the fact table.
    sumimi_service::db::init_schema(&conn)?;

    let (url_expr, title_expr, referrer_expr, referrer_path_expr) = if lean {
        (
            "'/blog/post-' || (i % 500000)::VARCHAR || '-slug-' || (i % 977)::VARCHAR",
            "NULL",
            "list_extract(['www.google.com','duckduckgo.com','news.ycombinator.com','github.com','x.com','www.bing.com','reddit.com'], 1 + (i % 7)::INT)",
            "NULL",
        )
    } else if realistic {
        (
            "'/blog/post-' || (i % 500000)::VARCHAR || '-slug-' || (i % 977)::VARCHAR",
            "'A blog post about topic number ' || (i % 100000)::VARCHAR || ' — with a longer title'",
            "list_extract(['www.google.com','duckduckgo.com','news.ycombinator.com','github.com','x.com','www.bing.com','reddit.com'], 1 + (i % 7)::INT)",
            "'/search?q=some+search+term+number+' || (i % 2000000)::VARCHAR",
        )
    } else {
        (
            "list_extract(['/','/blog/one','/blog/two','/blog/three','/docs','/docs/install','/pricing','/download','/blog/four','/blog/five','/about','/changelog','/tags/rust','/tags/duckdb','/feed.xml'], 1 + (i % 15)::INT)",
            "'Blog post ' || (i % 15)::VARCHAR",
            "list_extract(['https://www.google.com/','https://duckduckgo.com/','https://news.ycombinator.com/','https://github.com/','https://x.com/','https://www.bing.com/','https://reddit.com/'], 1 + (i % 7)::INT)",
            "'/item?id=' || (i % 100000)::VARCHAR",
        )
    };

    println!(
        "building {rows} page views{}…",
        if lean {
            " (lean schema)"
        } else if realistic {
            " (realistic cardinality)"
        } else {
            ""
        }
    );
    let start = Instant::now();
    conn.execute_batch(&format!(
        r#"
INSERT INTO event (
    id, website_id, visit_id, session_id, created_at,
    url, title, referrer, referrer_path, hostname,
    browser, browser_version, os, os_version, device, screen, language, country,
    utm_source, utm_medium, utm_campaign,
    lcp, inp, cls, fcp, ttfb,
    event_type
)
SELECT
  i,
  'w1',
  {session},
  {visitor},
  TIMESTAMP '2024-01-01' + to_seconds((i * 7919) % 31536000),
  {url_expr},
  {title_expr},
  {referrer_expr},
  {referrer_path_expr},
  'example.com',
  list_extract(['Chrome','Safari','Firefox','Edge'], 1 + (i % 4)::INT),
  '120',
  list_extract(['Windows','macOS','iOS','Android','Linux'], 1 + (i % 5)::INT),
  '10',
  list_extract(['desktop','mobile','tablet'], 1 + (i % 3)::INT),
  list_extract(['1920x1080','1536x864','1366x768','390x844'], 1 + (i % 4)::INT),
  list_extract(['en-US','zh-CN','en-GB','ja-JP'], 1 + (i % 4)::INT),
  list_extract(['US','CN','DE','GB','JP','IN','FR','BR'], 1 + (i % 8)::INT),
  CASE WHEN i % 4 = 0 THEN 'twitter' ELSE NULL END,
  CASE WHEN i % 4 = 0 THEN 'social' ELSE NULL END,
  'campaign-' || (i % 50)::VARCHAR,
  600.0 + (i % 1800), 30.0 + (i % 260), (i % 100) / 1000.0,
  400.0 + (i % 900), 60.0 + (i % 220),
  'pageview'
FROM generate_series(1, {rows}) t(i);
"#,
        // The ids are 128-bit UUIDs, the way the server hands them out.
        session = uuid("'session-' || (i / 3)::VARCHAR"),
        visitor = uuid("'visitor-' || (i % 3000000)::VARCHAR"),
    ))?;
    conn.execute_batch("CHECKPOINT")?;
    // The report layer works on the backend, not on the raw connection.
    let backend = Db::from_connection(&path, conn);
    println!("  built in {:.1}s", start.elapsed().as_secs_f64());

    let file = std::fs::metadata(&path)?.len();
    let per_row = file as f64 / rows as f64;
    println!("\n=== 数据体积 ===");
    println!("  文件大小        {:.1} MB", file as f64 / 1048576.0);
    println!("  每行            {per_row:.0} 字节（压缩后，含元数据，无索引）");

    // The dashboard's time window: the last 30 days of the synthetic year.
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

    println!("\n=== 仪表盘查询耗时（30 天窗口，{rows} 行全表）===");
    let timed = |label: &str, f: &dyn Fn() -> anyhow::Result<String>| {
        let start = Instant::now();
        let out = f().unwrap_or_else(|e| format!("<{e}>"));
        println!(
            "  {label:22} {:>7.1} ms   {out}",
            start.elapsed().as_secs_f64() * 1000.0
        );
    };

    timed("summary", &|| {
        let m = backend.summary(&query)?;
        Ok(format!("{} 次浏览 / {} 访客", m.pageviews, m.visitors))
    });
    timed("timeseries", &|| {
        let rows = backend.timeseries(&query)?;
        Ok(format!("{} 个数据点", rows.len()))
    });
    timed("pages (top 50)", &|| {
        let rows = backend.breakdown(&query, Dimension::Url)?;
        Ok(format!("{} 行", rows.len()))
    });
    timed("referrers", &|| {
        let rows = backend.breakdown(&query, Dimension::Referrer)?;
        Ok(format!("{} 行", rows.len()))
    });
    timed("browsers", &|| {
        let rows = backend.breakdown(&query, Dimension::Browser)?;
        Ok(format!("{} 行", rows.len()))
    });
    timed("sessions (50)", &|| {
        let rows = backend.sessions(&query)?;
        Ok(format!("{} 行", rows.len()))
    });

    // The summary query is the one the dashboard asks for first, and the one that
    // builds the biggest hash tables (two DISTINCT aggregates over the window).
    // DuckDB preserves the insertion order by default, which costs memory on every
    // aggregate. Measure what turning that off buys.
    println!();
    timed("summary (no order)", &|| {
        backend.with_connection(|conn| {
            conn.execute_batch("SET preserve_insertion_order=false")?;
            let m = conn.query_row(
                "SELECT COUNT(*), COUNT(DISTINCT session_id), COUNT(DISTINCT visit_id)
                 FROM event WHERE website_id = 'w1'
                   AND created_at >= TIMESTAMP '2024-12-01' AND created_at < TIMESTAMP '2024-12-31'",
                [],
                |row| {
                    Ok(format!(
                        "{} 次浏览 / {} 访客 / {} 会话",
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?
                    ))
                },
            )?;
            Ok::<_, anyhow::Error>(m)
        })
    });

    // Extrapolation to the user's scenario.
    println!("\n=== 外推 ===");
    let visits_per_day = 10_000_000f64;
    let daily_bytes = visits_per_day * per_row;
    println!("  10M 浏览/天");
    println!("  每天约            {:.1} GB", daily_bytes / 1e9);
    println!("  1 年              {:.0} GB", daily_bytes * 365.0 / 1e9);
    println!("  10 年             {:.0} GB", daily_bytes * 3650.0 / 1e9);
    println!("  20GB 磁盘能存     {:.0} 天", 20e9 / daily_bytes);

    Ok(())
}
