//! How much of the stored data is the UUID?
//!
//! The same rows, written five times: with everything, without the visitor id,
//! without both ids, without the ids and the page, and with **only the ids**.
//! The difference between the file sizes is what those columns cost on disk.
//!
//! The last variant answers the other question: **what do the ~35 narrow
//! columns cost?** If they cost about as much as the two UUIDs, then squeezing
//! the row is not worth it — and merging the columns (which would break the
//! dimensions and the filters) is not on the table either.

use duckdb::Connection;

fn main() {
    let dir = std::env::temp_dir().join("sumimi-uuid-share");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let variants = [
        ("everything", "*"),
        ("no session_id", "* EXCLUDE (session_id)"),
        ("no ids", "* EXCLUDE (session_id, visit_id)"),
        ("no ids, no page", "* EXCLUDE (session_id, visit_id, url_path, url_query, page_title)"),
        (
            "only the ids",
            "event_id, website_id, visit_id, session_id, created_at",
        ),
    ];

    let mut sizes = Vec::new();
    for (name, columns) in variants {
        let path = dir.join(format!("{name}.duckdb"));
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE MACRO uuid_of(s) AS CAST(
                substr(sha256(s), 1, 8) || '-' || substr(sha256(s), 9, 4) || '-' ||
                substr(sha256(s), 13, 4) || '-' || substr(sha256(s), 17, 4) || '-' ||
                substr(sha256(s), 21, 12) AS UUID);",
        )
        .unwrap();
        conn.execute_batch(
            "CREATE TABLE website_event (
                event_id BIGINT, website_id VARCHAR, visit_id UUID, session_id UUID,
                created_at TIMESTAMP, url_path VARCHAR, url_query VARCHAR, page_title VARCHAR,
                referrer_domain VARCHAR, referrer_path VARCHAR, referrer_query VARCHAR,
                hostname VARCHAR, channel VARCHAR,
                browser VARCHAR, browser_version VARCHAR, os VARCHAR, os_version VARCHAR,
                device VARCHAR, screen VARCHAR, language VARCHAR,
                country VARCHAR, region VARCHAR, city VARCHAR,
                utm_source VARCHAR, utm_medium VARCHAR, utm_campaign VARCHAR,
                utm_content VARCHAR, utm_term VARCHAR,
                event_type VARCHAR, event_name VARCHAR, tag VARCHAR, data VARCHAR);
            INSERT INTO website_event SELECT
                i, 'w1', uuid_of('session-' || (i / 3)::VARCHAR),
                uuid_of('visitor-' || (i % 50000)::VARCHAR),
                TIMESTAMP '2024-01-01' + to_seconds((i * 7919) % 31536000),
                '/blog/post-' || (i % 5000)::VARCHAR,
                'utm_source=twitter&utm_medium=cpc&utm_campaign=spring&utm_content=cta&utm_term=term',
                'A title about topic ' || (i % 500)::VARCHAR,
                'news.ycombinator.com', '/item', '?q=search', 'example.com', 'social',
                'Chrome', '120', 'macOS', '10', 'desktop', '1920x1080', 'en-US',
                'US', 'BE', 'Berlin', 'twitter', 'social', 'spring', 'cta', 'term',
                'pageview', NULL, NULL, NULL
            FROM generate_series(1, 200000) t(i);",
        )
        .unwrap();
        let sql = format!("CREATE TABLE kept AS SELECT {columns} FROM website_event; CHECKPOINT;");
        conn.execute_batch(&sql).unwrap();
        let size = std::fs::metadata(&path).unwrap().len();
        sizes.push((name.to_string(), size));
    }

    let rows = 200_000.0_f64;
    let base = sizes[0].1 as f64;
    println!("| columns | size | per view | share |");
    println!("| --- | ---: | ---: | ---: |");
    for (name, size) in &sizes {
        println!(
            "| {} | {:.1} MB | {:.1} B | {:.1}% |",
            name,
            *size as f64 / 1048576.0,
            *size as f64 / rows,
            *size as f64 / base * 100.0
        );
    }
    // Every file holds the same full `website_event` table plus the variant's
    // `kept` table, so the **differences** are what the columns cost.
    // The first excluded column is `session_id` (the visitor's id), the
    // second is `visit_id` (the session's id).
    let visitor = (base - sizes[1].1 as f64) / rows;
    let session = (sizes[1].1 as f64 - sizes[2].1 as f64) / rows;
    let page = (sizes[2].1 as f64 - sizes[3].1 as f64) / rows;
    // The two ids together: everything minus everything without them.
    let ids = (base - sizes[2].1 as f64) / rows;
    // The ~30 narrow columns: everything minus everything without them.
    let narrow = (base - sizes[4].1 as f64) / rows;
    println!("\nsession_id (the visitor): {visitor:.1} B per view");
    println!("visit_id (the session): {session:.1} B per view");
    println!("the two ids together: {ids:.1} B per view");
    println!("the ~35 narrow columns: {narrow:.1} B per view");
    println!("the page (url, query, title): {page:.1} B per view");
    println!(
        "\n**The narrow columns are cheap and the ids are not.** All the narrow columns \
         together cost {narrow:.1} B a view; the two ids cost {ids:.1} B — {:.1}x more. \
         So the hypothesis that the row is fat because of the many columns is **wrong**: \
         they compress (the same values repeat), while the ids are pure entropy.",
        ids / narrow.max(0.001)
    );
    println!(
        "\n**What follows.** Merging the narrow columns would save nothing and would break \
         the dimensions, the filters and the shape of the API — so it is off the table. \
         The ids are the fat part, but truncating them makes the visitor count \
         'probably right' instead of exact, and the product promises exact. So: \
         **nothing changes.** The disk is managed by the retention window and the backups."
    );
}
