//! One store for every site, or one store per site?
//!
//! **The question.** The reports always filter by `website_id`. When the sites
//! share one table, a query for one site reads the whole table's filter column
//! and — because the traffic is interleaved — the row groups of *every* site.
//! When each site has its own file, the query reads only that site's rows.
//!
//! ⇒ ⇒ **How much is that worth?** Measured, not argued:
//!
//! 1. the same rows, the same schema, the same settings;
//! 2. **mixed**: one file, one `website_event` table, the sites interleaved;
//! 3. **split**: one file per site, one `website_event` table each;
//! 4. the three queries the dashboard asks first, timed on both.
//!
//! # What it found (2026-11-15, 10 sites × 1 000 000 rows, 861 MB)
//!
//! | query | mixed | split (10 instances) | attached (1 instance) |
//! | --- | ---: | ---: | ---: |
//! | summary | 27.7 ms | 13.8 ms | 14.4 ms |
//! | breakdown | 4.8 ms | 0.7 ms | 0.6 ms |
//! | event log | 1.0 ms | 1.3 ms | 1.3 ms |
//! | timeseries | 6.0 ms | 1.4 ms | 1.4 ms |
//! | the buffer manager holds | **211 MB** | 37 MB | **39 MB** |
//!
//! ⇒ ⇒ **The speed comes from the table being per site, not from the instance
//! being per site** ⇒ ⇒ ⇒ **one instance with every site attached is as fast as
//! one instance per site — and it keeps one buffer manager**.
//!
//! ```bash
//! cargo run --release --example site_split -p sumimi-service
//! ```

use std::time::Instant;

use anyhow::Result;
use duckdb::Connection;

/// How many sites.
const SITES: usize = 10;
/// How many rows each site has — the full measurement; the argument can make
/// it smaller, for a quick look.
const ROWS_PER_SITE: usize = 1_000_000;

/// The queries the dashboard asks first.
const QUERIES: &[(&str, &str)] = &[
    (
        "summary",
        "SELECT count(*), count(DISTINCT session_id), count(DISTINCT visit_id)
         FROM website_event WHERE website_id = ?1",
    ),
    (
        "breakdown",
        "SELECT browser, count(*) FROM website_event WHERE website_id = ?1 GROUP BY 1 ORDER BY 2 DESC",
    ),
    (
        "event log",
        "SELECT event_id, website_id, visit_id, session_id, created_at, url_path, page_title,
                referrer_domain, referrer_path, hostname, browser, os, device, country, city,
                lcp, inp, cls
         FROM website_event WHERE website_id = ?1 ORDER BY created_at DESC, event_id DESC LIMIT 50",
    ),
    (
        "timeseries",
        "SELECT date_trunc('day', created_at) AS d, count(*) FROM website_event
         WHERE website_id = ?1 GROUP BY 1 ORDER BY 1",
    ),
];

fn main() -> Result<()> {
    // The rows each site gets; the argument makes the measurement smaller, for
    // a quick look. The recorded numbers are the full one.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rows_per_site: usize = args
        .first()
        .and_then(|v| v.parse().ok())
        .unwrap_or(ROWS_PER_SITE);

    let dir = std::env::temp_dir().join("sumimi-site-split");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;

    let mixed_path = dir.join("mixed.duckdb");
    let split_dir = dir.join("split");
    std::fs::create_dir_all(&split_dir)?;

    println!("== building ==");
    let start = Instant::now();
    build_mixed(&mixed_path, rows_per_site)?;
    println!("  mixed: one file, {SITES} sites, {rows_per_site} rows each");
    build_split(&split_dir, rows_per_site)?;
    println!("  split: {SITES} files, {rows_per_site} rows each");
    println!("  built in {:.1}s\n", start.elapsed().as_secs_f64());

    let mixed_size = std::fs::metadata(&mixed_path)?.len();
    let split_size: u64 = (0..SITES)
        .map(|i| std::fs::metadata(site_path(&split_dir, i)).map(|m| m.len()).unwrap_or(0))
        .sum();
    println!(
        "== on disk ==\n  mixed: {:.1} MB\n  split: {:.1} MB ({:.2}x)\n",
        mixed_size as f64 / 1_048_576.0,
        split_size as f64 / 1_048_576.0,
        split_size as f64 / mixed_size as f64,
    );

    // The site the queries ask for: the middle one, so it is not the first
    // row group and not the last.
    let site = SITES / 2;
    let website_id = format!("w{site}");

    println!("== the queries, on site {website_id} ==");
    println!(
        "{:<12} {:>10} {:>10} {:>10} {:>8}",
        "query", "mixed", "split", "attached", "split/mixed"
    );
    for (name, sql) in QUERIES {
        let mixed = time(&mixed_path, sql, &website_id)?;
        let split = time(&site_path(&split_dir, site), sql, &website_id)?;
        let attached = time_attached(&split_dir, site, sql, &website_id)?;
        println!(
            "{name:<12} {mixed:>8.1} ms {split:>8.1} ms {attached:>8.1} ms {:>7.1}x",
            mixed as f64 / split as f64
        );
    }


    // The memory of the two shapes: how much the buffer manager holds, and how
    // much the process holds.
    println!("== the memory ==");
    memory_of(&mixed_path, &website_id)?;
    memory_of_split(&split_dir, site, &website_id)?;
    memory_of_attached(&split_dir, site, &website_id)?;

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}


/// The third shape: **one instance**, every site's file attached, and `USE` to
/// pick the site.
///
/// ⇒ ⇒ One buffer manager, one memory limit — and one table per site.
fn time_attached(dir: &std::path::Path, site: usize, sql: &str, website_id: &str) -> Result<f64> {
    let conn = Connection::open_in_memory()?;
    conn.execute_batch("SET threads=4; SET memory_limit='256MB'")?;
    for s in 0..SITES {
        let path = site_path(dir, s).to_string_lossy().replace('\'', "''");
        conn.execute_batch(&format!("ATTACH '{path}' AS site{s}"))?;
    }
    let mut best = f64::MAX;
    for _ in 0..4 {
        // The statement is prepared **after** the `USE`: a statement binds its
        // tables when it is prepared, so the site has to be chosen first.
        conn.execute_batch(&format!("USE site{site}"))?;
        let mut stmt = conn.prepare(sql)?;
        let start = Instant::now();
        let mut rows = stmt.query([website_id])?;
        while rows.next()?.is_some() {}
        let took = start.elapsed().as_secs_f64() * 1000.0;
        if took < best {
            best = took;
        }
    }
    Ok(best)
}

/// What one connection to the mixed file holds after the queries.
fn memory_of(path: &std::path::Path, website_id: &str) -> Result<()> {
    let conn = Connection::open(path)?;
    conn.execute_batch("SET threads=4; SET memory_limit='256MB'")?;
    for (_, sql) in QUERIES {
        let mut stmt = conn.prepare(sql)?;
        let mut rows = stmt.query([website_id])?;
        while rows.next()?.is_some() {}
    }
    let _ = report(&conn, "mixed (1 instance)");
    Ok(())
}

/// What ten connections — one per site — hold after the same queries.
fn memory_of_split(dir: &std::path::Path, site: usize, website_id: &str) -> Result<()> {
    let mut held = Vec::new();
    for s in 0..SITES {
        held.push(Connection::open(site_path(dir, s))?);
    }
    for conn in &held {
        conn.execute_batch("SET threads=4; SET memory_limit='256MB'")?;
    }
    for (_, sql) in QUERIES {
        let mut stmt = held[site].prepare(sql)?;
        let mut rows = stmt.query([website_id])?;
        while rows.next()?.is_some() {}
    }
    let _ = report(&held[site], "split (10 instances)");
    Ok(())
}


/// What one instance with every site attached holds — one buffer manager.
fn memory_of_attached(dir: &std::path::Path, site: usize, website_id: &str) -> Result<()> {
    let conn = Connection::open_in_memory()?;
    conn.execute_batch("SET threads=4; SET memory_limit='256MB'")?;
    for s in 0..SITES {
        let path = site_path(dir, s).to_string_lossy().replace('\'', "''");
        conn.execute_batch(&format!("ATTACH '{path}' AS site{s}"))?;
    }
    for (_, sql) in QUERIES {
        conn.execute_batch(&format!("USE site{site}"))?;
        let mut stmt = conn.prepare(sql)?;
        let mut rows = stmt.query([website_id])?;
        while rows.next()?.is_some() {}
    }
    let _ = report(&conn, "attached (1 instance, 10 files)");
    Ok(())
}

/// What the buffer manager holds, and what the process holds.
fn report(conn: &Connection, label: &str) -> Result<()> {
    let mut stmt = conn.prepare("SELECT tag, memory_usage_bytes FROM duckdb_memory()")?;
    let mut rows = stmt.query([])?;
    let mut total: i64 = 0;
    let mut detail = String::new();
    while let Some(row) = rows.next()? {
        let tag: String = row.get(0)?;
        let bytes: i64 = row.get(1)?;
        total += bytes;
        if bytes > 0 {
            detail.push_str(&format!("\n    {tag}: {:.1} MB", bytes as f64 / 1_048_576.0));
        }
    }
    println!("  {label}: the buffer manager holds {:.1} MB{detail}", total as f64 / 1_048_576.0);
    println!("    the process holds {:.1} MB", rss()?);
    Ok(())
}

/// The resident size of this process, in MB.
fn rss() -> Result<f64> {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()?;
    let kb: f64 = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0.0);
    Ok(kb / 1024.0)
}

/// The file of one site.
fn site_path(dir: &std::path::Path, site: usize) -> std::path::PathBuf {
    dir.join(format!("site{site}.duckdb"))
}

/// One file, one table, every site in it — the traffic interleaved.
///
/// ⇒ ⇒ This is the **pre-D-046 shape**: the server no longer keeps the sites
/// in one file, but the shape is still what the *other* engines (and the
/// PostgreSQL one) look like, so the comparison needs it. The measured
/// question — "is a table per site faster than one table for all?" — is
/// unchanged by where the file lives.
fn build_mixed(path: &std::path::Path, rows_per_site: usize) -> Result<()> {
    let conn = Connection::open(path)?;
    sumimi_service::db::init_schema(&conn)?;
    // The rows are interleaved by the order they arrive: site 0's first row,
    // site 1's first row, … — the way the traffic really lands. Every row
    // group therefore holds rows of every site.
    conn.execute_batch(&format!(
        "INSERT INTO website_event (event_id, website_id, visit_id, session_id, session_start, created_at,
                            url_path, url_query, page_title, referrer_domain, referrer_path, referrer_name,
                            hostname, browser, browser_version, os, os_version, device, screen,
                            language, country, region, city, ip_prefix, user_agent, channel,
                            utm_source, utm_medium, utm_campaign, gclid, lcp, inp, cls, fcp, ttfb,
                            event_type, event_name)
         SELECT i, 'w' || (i % {SITES})::VARCHAR, uuid(), uuid(),
                TIMESTAMP '2024-01-01' + to_seconds(i),
                TIMESTAMP '2024-01-01' + to_seconds(i),
                '/blog/post-' || (i % 50000)::VARCHAR || '-a-title-that-is-a-bit-longer-than-usual',
                'utm_source=twitter&utm_medium=social&utm_campaign=post-' || (i % 97)::VARCHAR,
                'A post about topic ' || (i % 50000)::VARCHAR || ' — with a longer title than usual',
                'https://news.ycombinator.com/item?id=' || (i % 50000)::VARCHAR,
                '/item?id=' || (i % 50000)::VARCHAR,
                'news.ycombinator.com',
                'example.com',
                list_extract(['Chrome','Safari','Firefox','Edge'], 1 + (i % 4)::INT),
                '120.0.0.0',
                list_extract(['Windows','macOS','iOS','Android','Linux'], 1 + (i % 5)::INT),
                '15.2.0',
                list_extract(['desktop','mobile','tablet'], 1 + (i % 3)::INT),
                '1920x1080',
                list_extract(['en-US','zh-CN','de-DE','ja-JP'], 1 + (i % 4)::INT),
                list_extract(['DE','US','CN','JP'], 1 + (i % 4)::INT),
                'Bavaria', 'Munich',
                '203.0.113.' || (i % 250)::VARCHAR,
                'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36',
                'social',
                'twitter', 'social', 'post-' || (i % 97)::VARCHAR,
                'gclid-' || (i % 1000)::VARCHAR,
                1200.0 + (i % 1800), 30.0 + (i % 260), (i % 100) / 1000.0,
                400.0 + (i % 900), 60.0 + (i % 220),
                'pageview', NULL
         FROM range({}) t(i)",
        rows_per_site * SITES
    ))?;
    conn.execute_batch("CHECKPOINT")?;
    Ok(())
}

/// One file per site, one table each.
fn build_split(dir: &std::path::Path, rows_per_site: usize) -> Result<()> {
    for site in 0..SITES {
        let conn = Connection::open(site_path(dir, site))?;
        sumimi_service::db::init_schema(&conn)?;
        conn.execute_batch(&format!(
            "INSERT INTO website_event (event_id, website_id, visit_id, session_id, session_start, created_at,
                                url_path, url_query, page_title, referrer_domain, referrer_path, referrer_name,
                                hostname, browser, browser_version, os, os_version, device, screen,
                                language, country, region, city, ip_prefix, user_agent, channel,
                                utm_source, utm_medium, utm_campaign, gclid, lcp, inp, cls, fcp, ttfb,
                                event_type, event_name)
             SELECT i, 'w{site}', uuid(), uuid(),
                    TIMESTAMP '2024-01-01' + to_seconds(i),
                    TIMESTAMP '2024-01-01' + to_seconds(i),
                    '/blog/post-' || (i % 50000)::VARCHAR || '-a-title-that-is-a-bit-longer-than-usual',
                    'utm_source=twitter&utm_medium=social&utm_campaign=post-' || (i % 97)::VARCHAR,
                    'A post about topic ' || (i % 50000)::VARCHAR || ' — with a longer title than usual',
                    'https://news.ycombinator.com/item?id=' || (i % 50000)::VARCHAR,
                    '/item?id=' || (i % 50000)::VARCHAR,
                    'news.ycombinator.com',
                    'example.com',
                    list_extract(['Chrome','Safari','Firefox','Edge'], 1 + (i % 4)::INT),
                    '120.0.0.0',
                    list_extract(['Windows','macOS','iOS','Android','Linux'], 1 + (i % 5)::INT),
                    '15.2.0',
                    list_extract(['desktop','mobile','tablet'], 1 + (i % 3)::INT),
                    '1920x1080',
                    list_extract(['en-US','zh-CN','de-DE','ja-JP'], 1 + (i % 4)::INT),
                    list_extract(['DE','US','CN','JP'], 1 + (i % 4)::INT),
                    'Bavaria', 'Munich',
                    '203.0.113.' || (i % 250)::VARCHAR,
                    'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36',
                    'social',
                    'twitter', 'social', 'post-' || (i % 97)::VARCHAR,
                    'gclid-' || (i % 1000)::VARCHAR,
                    1200.0 + (i % 1800), 30.0 + (i % 260), (i % 100) / 1000.0,
                    400.0 + (i % 900), 60.0 + (i % 220),
                    'pageview', NULL
             FROM range({rows_per_site}) t(i)"
        ))?;
        conn.execute_batch("CHECKPOINT")?;
    }
    Ok(())
}

/// Time one query: the best of three runs, after a warm-up.
fn time(path: &std::path::Path, sql: &str, website_id: &str) -> Result<f64> {
    let conn = Connection::open(path)?;
    conn.execute_batch("SET threads=4; SET memory_limit='256MB'")?;
    let mut stmt = conn.prepare(sql)?;
    let mut best = f64::MAX;
    for _ in 0..4 {
        let start = Instant::now();
        let mut rows = stmt.query([website_id])?;
        while rows.next()?.is_some() {}
        let took = start.elapsed().as_secs_f64() * 1000.0;
        if took < best {
            best = took;
        }
    }
    Ok(best)
}
