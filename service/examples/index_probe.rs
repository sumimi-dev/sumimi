//! What does the `PRIMARY KEY` on `website_event` actually cost?
//!
//! DuckDB builds an ART (adaptive radix tree) index for every PRIMARY KEY / UNIQUE
//! constraint. That index is maintained on every append — and, for an append-only
//! table whose only access path is a filtered scan, it buys nothing: DuckDB's own
//! pruning mechanism (zone maps: one min/max per column per row group) is what makes
//! `WHERE website_id = ? AND created_at BETWEEN ? AND ?` fast, and zone maps are free.
//!
//! This probe measures the difference: same data, same queries, with and without the
//! constraint. It also answers three follow-up questions:
//!
//!   1. does the ART index live on disk, or only in memory?
//!   2. is it rebuilt when the database is reopened?
//!   3. how does a user-created `CREATE INDEX` behave?
//!
//! ```bash
//! cargo run --release --example index_probe -p sumimi-service
//! ```

use std::time::Instant;

use duckdb::Connection;

/// How many rows to write.
const ROWS: usize = 2_000_000;

fn main() -> anyhow::Result<()> {
    let dir = std::env::temp_dir().join("sumimi-index-probe");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;

    println!("duckdb version: {}", version());
    println!();

    probe_catalog()?;
    probe_enforcement()?;
    probe_create_index()?;

    let with_pk = build(&dir.join("with_pk.duckdb"), true)?;
    let without = build(&dir.join("no_pk.duckdb"), false)?;

    println!("{ROWS} page views each, identical data\n");
    println!(
        "{:<22} {:>10} {:>14} {:>10} {:>10}",
        "", "file", "mem (in-use)", "append", "scan"
    );
    println!("{}", "-".repeat(70));
    println!(
        "{:<22} {:>10} {:>14} {:>10} {:>10}",
        "PRIMARY KEY event_id",
        fmt_bytes(with_pk.file_size),
        fmt_bytes(with_pk.memory.max(0) as u64),
        format!("{:.2}s", with_pk.write_secs),
        format!("{:.3}s", with_pk.scan_secs)
    );
    println!(
        "{:<22} {:>10} {:>14} {:>10} {:>10}",
        "no constraint",
        fmt_bytes(without.file_size),
        fmt_bytes(without.memory.max(0) as u64),
        format!("{:.2}s", without.write_secs),
        format!("{:.3}s", without.scan_secs)
    );
    println!();
    println!(
        "the index costs: {:+.1} MB of memory, {:+.1} MB on disk ({:+.1} B/row), append {:+.2}x, scan {:+.2}x",
        (with_pk.memory as f64 - without.memory as f64) / 1e6,
        (with_pk.file_size as f64 - without.file_size as f64) / 1e6,
        (with_pk.file_size as f64 - without.file_size as f64) / ROWS as f64,
        with_pk.write_secs / without.write_secs.max(0.0001),
        with_pk.scan_secs / without.scan_secs.max(0.0001),
    );
    println!();
    println!("same answers? {}", same_numbers(&with_pk.path, &without.path)?);

    // What does the plan look like? Zone maps are what prune the scan.
    println!();
    show_plan(&with_pk.path, "with PK")?;
    show_plan(&without.path, "no PK  ")?;

    // ------------------------------------------------------------------
    // Is the index on disk, or only in memory?
    // ------------------------------------------------------------------
    println!();
    probe_persistence(&with_pk.path, &without.path)?;

    // ------------------------------------------------------------------
    // What does a user-created index cost?
    // ------------------------------------------------------------------
    println!();
    probe_user_index(&dir.join("no_pk.duckdb"))?;

    println!();
    probe_sql_features(&dir.join("no_pk.duckdb"))?;

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// The measurements of one database.
struct Probe {
    path: std::path::PathBuf,
    file_size: u64,
    memory: i64,
    write_secs: f64,
    scan_secs: f64,
}

/// Build a database with (or without) the PRIMARY KEY and measure it.
fn build(path: &std::path::PathBuf, pk: bool) -> anyhow::Result<Probe> {
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "SET memory_limit='1GB';
         SET threads=4;",
    )?;

    let id_def = if pk { "BIGINT PRIMARY KEY" } else { "BIGINT" };
    // The two ids are the visitor's and the session's; the v12 schema names
    // them `session_id` (the visitor) and `visit_id` (the session).
    conn.execute_batch(&format!(
        "CREATE TABLE website_event (
            event_id       {id_def},
            website_id     VARCHAR NOT NULL,
            visit_id       VARCHAR NOT NULL,
            session_id     VARCHAR NOT NULL,
            created_at     TIMESTAMP NOT NULL,
            url_path       VARCHAR,
            page_title     VARCHAR,
            referrer_domain VARCHAR,
            browser        VARCHAR,
            os             VARCHAR,
            device         VARCHAR,
            country        VARCHAR,
            utm_source     VARCHAR,
            utm_medium     VARCHAR,
            lcp            DOUBLE,
            inp            DOUBLE,
            cls            DOUBLE
        )"
    ))?;

    let base = chrono::NaiveDate::from_ymd_opt(2026, 1, 1)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();
    let year = 365 * 86_400;

    // The real write path: the Appender, in batches, exactly like `write_batch`.
    let started = Instant::now();
    {
        let mut appender = conn.appender("website_event")?;
        for i in 0..ROWS as i64 {
            let at = base + chrono::Duration::seconds((i * 7919) % year);
            let site = format!("site-{}", i % 3);
            // The session's id and the visitor's id — the columns are
            // `visit_id` and `session_id` in that order.
            let visit = format!("session-{}-{}", i / 4, i % 100_000);
            let visitor = format!("visitor-{:08x}", (i * 2654435761) % 1_000_000);
            let url = format!("/blog/post-{}", i % 500);
            let title = format!("Post number {}", i % 200);
            let referrer = match i % 4 {
                0 => None,
                1 => Some("www.google.com".to_string()),
                2 => Some("news.ycombinator.com".to_string()),
                _ => Some("duckduckgo.com".to_string()),
            };
            appender.append_row(duckdb::params![
                i,
                site,
                visit,
                visitor,
                at,
                url,
                title,
                referrer,
                "Chrome",
                "macOS",
                "desktop",
                "US",
                "twitter",
                "social",
                1200.0f64,
                80.0f64,
                0.02f64,
            ])?;
        }
        appender.flush()?;
    }
    let write_secs = started.elapsed().as_secs_f64();

    conn.execute_batch("CHECKPOINT")?;

    let file_size = std::fs::metadata(path)?.len();
    let memory = memory_used(&conn)?;
    print_memory(&conn, "  ")?;

    // The scan every dashboard query does: a website, a date range, a group by.
    // February, where the data actually is.
    let started = Instant::now();
    let n: i64 = conn.query_row(
        "SELECT count(DISTINCT visit_id) FROM website_event
         WHERE website_id = 'site-0'
           AND created_at >= TIMESTAMP '2026-02-01' AND created_at < TIMESTAMP '2026-03-01'",
        [],
        |row| row.get(0),
    )?;
    let scan_secs = started.elapsed().as_secs_f64();
    println!("  (site-0 Feb sessions: {n})");

    Ok(Probe {
        path: path.to_path_buf(),
        file_size,
        memory,
        write_secs,
        scan_secs,
    })
}

/// What does the catalog say about indexes and constraints?
fn probe_catalog() -> anyhow::Result<()> {
    let conn = Connection::open_in_memory()?;
    conn.execute_batch(
        "CREATE TABLE a (id BIGINT PRIMARY KEY, v VARCHAR);
         CREATE TABLE b (id BIGINT, v VARCHAR);
         CREATE TABLE c (id VARCHAR UNIQUE, v VARCHAR);",
    )?;

    println!("=== duckdb_indexes() ===");
    let mut stmt = conn.prepare(
        "SELECT table_name, is_unique, is_primary, sql FROM duckdb_indexes() ORDER BY table_name",
    )?;
    let mut rows = stmt.query([])?;
    let mut n = 0;
    while let Some(row) = rows.next()? {
        n += 1;
        println!(
            "  {} unique={} primary={}  sql={}",
            row.get::<_, String>(0)?,
            row.get::<_, bool>(1)?,
            row.get::<_, bool>(2)?,
            row.get::<_, Option<String>>(3)?.unwrap_or_default()
        );
    }
    if n == 0 {
        println!("  (none — DuckDB reports no index for a PRIMARY KEY)");
    }
    drop(rows);
    drop(stmt);

    println!();
    println!("=== duckdb_constraints() ===");
    let mut stmt = conn.prepare(
        "SELECT table_name, constraint_type, constraint_text FROM duckdb_constraints()
         ORDER BY table_name",
    )?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        println!(
            "  {}  {}  {}",
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?.unwrap_or_default()
        );
    }
    println!();
    Ok(())
}

/// Is the PRIMARY KEY actually enforced?
fn probe_enforcement() -> anyhow::Result<()> {
    let conn = Connection::open_in_memory()?;
    conn.execute_batch("CREATE TABLE a (id BIGINT PRIMARY KEY, v VARCHAR);")?;

    conn.execute("INSERT INTO a VALUES (1, 'x')", [])?;
    let duplicate = conn.execute("INSERT INTO a VALUES (1, 'y')", []);
    let rows: i64 = conn.query_row("SELECT count(*) FROM a", [], |row| row.get(0))?;

    println!("=== is the PRIMARY KEY enforced? ===");
    match duplicate {
        Ok(_) => println!("  duplicate id accepted  -> NOT enforced"),
        Err(err) => println!("  duplicate id rejected  -> enforced: {err}"),
    }
    println!("  rows after the duplicate insert: {rows}");
    println!();
    Ok(())
}

/// Can DuckDB 2.0 index an ordinary column?
fn probe_create_index() -> anyhow::Result<()> {
    let conn = Connection::open_in_memory()?;
    conn.execute_batch("CREATE TABLE a (id BIGINT, website_id VARCHAR, created_at TIMESTAMP);")?;
    let result = conn.execute_batch("CREATE INDEX i ON a (website_id)");
    println!("=== CREATE INDEX on an ordinary column ===");
    match result {
        Ok(_) => println!("  supported"),
        Err(err) => println!("  not supported: {err}"),
    }
    println!();
    Ok(())
}

/// Is the index on disk, or only in memory? Is it rebuilt on open?
fn probe_persistence(with_pk: &std::path::Path, without: &std::path::Path) -> anyhow::Result<()> {
    println!("=== is the index persisted? ===");
    for (label, path) in [("with PK", with_pk), ("no PK  ", without)] {
        let size_before = std::fs::metadata(path)?.len();
        let conn = Connection::open(path)?;
        // Touch the table so the index has to be there (or be rebuilt).
        let n: i64 = conn.query_row("SELECT count(*) FROM website_event", [], |row| row.get(0))?;
        let art = art_index_bytes(&conn)?;
        let size_after = std::fs::metadata(path)?.len();
        println!(
            "  {label}: file {size_before} -> {size_after} bytes (rows={n}), ART index in memory = {art} bytes"
        );
        // A second checkpoint: does the reopened database write the index back out?
        conn.execute_batch("CHECKPOINT")?;
        println!(
            "            after a second checkpoint: {} bytes",
            std::fs::metadata(path)?.len()
        );
    }
    println!();
    Ok(())
}

/// What does a user-created index cost, and does it land on disk?
fn probe_user_index(path: &std::path::Path) -> anyhow::Result<()> {
    println!("=== a user-created CREATE INDEX ===");
    let conn = Connection::open(path)?;
    let before = std::fs::metadata(path)?.len();
    conn.execute_batch("CREATE INDEX idx_website ON website_event (website_id)")?;
    conn.execute_batch("CHECKPOINT")?;
    let after = std::fs::metadata(path)?.len();
    let art = art_index_bytes(&conn)?;
    println!("  file {before} -> {after} bytes ({:+} bytes)", after as i64 - before as i64);
    println!("  ART index in memory: {art} bytes");

    // The decisive test: the same column, the same kind of index, created by hand.
    let before_id = std::fs::metadata(path)?.len();
    conn.execute_batch("CREATE INDEX idx_id ON website_event (event_id)")?;
    conn.execute_batch("CHECKPOINT")?;
    let after_id = std::fs::metadata(path)?.len();
    println!(
        "  CREATE INDEX on the id column: {before_id} -> {after_id} bytes ({:+} bytes, {:+.1} B/row)",
        after_id as i64 - before_id as i64,
        (after_id as i64 - before_id as i64) as f64 / ROWS as f64
    );

    // Is it used by the dashboard query?
    let started = Instant::now();
    let n: i64 = conn.query_row(
        "SELECT count(DISTINCT visit_id) FROM website_event
         WHERE website_id = 'site-0'
           AND created_at >= TIMESTAMP '2026-02-01' AND created_at < TIMESTAMP '2026-03-01'",
        [],
        |row| row.get(0),
    )?;
    println!(
        "  the dashboard query with the index: {:.3}s ({n} sessions)",
        started.elapsed().as_secs_f64()
    );
    println!();
    Ok(())
}


/// The plan of the dashboard query — what actually prunes the scan.
fn show_plan(path: &std::path::Path, label: &str) -> anyhow::Result<()> {
    let conn = Connection::open(path)?;
    let mut stmt = conn.prepare(
        "EXPLAIN SELECT count(DISTINCT visit_id) FROM website_event
         WHERE website_id = 'site-0'
           AND created_at >= TIMESTAMP '2026-02-01' AND created_at < TIMESTAMP '2026-03-01'",
    )?;
    let mut rows = stmt.query([])?;
    println!("  plan ({label}):");
    while let Some(row) = rows.next()? {
        let v: duckdb::types::Value = row.get(1)?;
        let text = format!("{v:?}");
        for line in text.lines() {
            let line = line.trim();
            if !line.is_empty() {
                println!("    {line}");
            }
        }
    }
    Ok(())
}


/// The SQL we need for the implementation: how fast is MAX(id), and how do
/// sequences behave?
fn probe_sql_features(path: &std::path::Path) -> anyhow::Result<()> {
    let conn = Connection::open(path)?;
    println!("=== MAX(event_id) on a big table ===");
    for _ in 0..3 {
        let started = Instant::now();
        let max: i64 = conn.query_row("SELECT max(event_id) FROM website_event", [], |row| row.get(0))?;
        println!("  max(event_id) = {max} in {:.3}s", started.elapsed().as_secs_f64());
    }
    let started = Instant::now();
    let max: i64 = conn.query_row("SELECT max(event_id) FROM website_event", [], |row| row.get(0))?;
    println!("  (warm: {max} in {:.3}s)", started.elapsed().as_secs_f64());

    println!();
    println!("=== sequences ===");
    conn.execute_batch(
        "CREATE SEQUENCE s START 1;
         CREATE TABLE t (id BIGINT DEFAULT nextval('s'), v VARCHAR);",
    )?;
    conn.execute_batch("INSERT INTO t (v) VALUES ('a'), ('b'), ('c')")?;
    let ids: Vec<i64> = {
        let mut stmt = conn.prepare("SELECT id FROM t ORDER BY id")?;
        let mut rows = stmt.query([])?;
        let mut v = Vec::new();
        while let Some(row) = rows.next()? {
            v.push(row.get(0)?);
        }
        v
    };
    println!("  default nextval -> ids {ids:?}");

    // Reserve a block: nextval once, then use it for N rows.
    let start: i64 = conn.query_row("SELECT nextval('s')", [], |row| row.get(0))?;
    println!("  nextval -> {start}");
    conn.execute_batch(&format!(
        "INSERT INTO t (id, v) VALUES ({}, 'x'), ({}, 'y')",
        start + 1,
        start + 2
    ))?;
    let setval = conn.execute_batch(&format!("SELECT setval('s', {})", start + 2));
    println!("  setval -> {:?}", setval.map(|_| "ok").map_err(|e| e.to_string()));
    let after: i64 = conn.query_row("SELECT nextval('s')", [], |row| row.get(0))?;
    println!("  nextval after setval -> {after} (want {})", start + 3);

    let restart = conn.execute_batch("ALTER SEQUENCE s RESTART WITH 100");
    println!("  ALTER SEQUENCE ... RESTART WITH -> {:?}", restart.map(|_| "ok").map_err(|e| e.to_string()));
    let after: i64 = conn.query_row("SELECT nextval('s')", [], |row| row.get(0))?;
    println!("  nextval after restart -> {after} (want 100)");

    println!();
    Ok(())
}

/// The bytes the ART index holds in memory.
fn art_index_bytes(conn: &Connection) -> anyhow::Result<i64> {
    let mut stmt = conn.prepare("SELECT * FROM duckdb_memory()")?;
    let mut rows = stmt.query([])?;
    let mut total = 0i64;
    while let Some(row) = rows.next()? {
        let tag: String = row.get(0)?;
        let bytes: i64 = row.get(1)?;
        if tag == "ART_INDEX" {
            total += bytes;
        }
    }
    Ok(total)
}

/// The memory the buffer manager holds, in bytes.
fn memory_used(conn: &Connection) -> anyhow::Result<i64> {
    for sql in [
        "SELECT coalesce(sum(memory_usage_bytes), 0) FROM duckdb_memory()",
        "SELECT coalesce(sum(memory_usage), 0) FROM duckdb_memory()",
    ] {
        if let Ok(v) = conn.query_row(sql, [], |row| row.get::<_, i64>(0)) {
            return Ok(v);
        }
    }
    Ok(0)
}

/// Print the memory breakdown, non-zero tags only.
fn print_memory(conn: &Connection, indent: &str) -> anyhow::Result<()> {
    let mut stmt = conn.prepare("SELECT * FROM duckdb_memory()")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let tag: String = row.get(0)?;
        let bytes: i64 = row.get(1)?;
        if bytes != 0 {
            println!("{indent}mem {tag}: {bytes} bytes");
        }
    }
    Ok(())
}

/// Do both databases answer the same questions?
fn same_numbers(with_pk: &std::path::Path, without: &std::path::Path) -> anyhow::Result<bool> {
    let a = Connection::open(with_pk)?;
    let b = Connection::open(without)?;
    let queries = [
        "SELECT count(*) FROM website_event",
        "SELECT count(DISTINCT session_id) FROM website_event",
        "SELECT count(DISTINCT visit_id) FROM website_event",
        "SELECT sum(lcp) FROM website_event WHERE website_id='site-1'",
        "SELECT url_path, count(*) FROM website_event GROUP BY url_path ORDER BY url_path LIMIT 5",
    ];
    let mut same = true;
    for q in queries {
        let left = fetch(&a, q)?;
        let right = fetch(&b, q)?;
        if left != right {
            same = false;
            println!("  MISMATCH on {q}\n    pk:    {left:?}\n    no-pk: {right:?}");
        }
    }
    Ok(same)
}

/// Run a query and collect the rows as strings.
fn fetch(conn: &Connection, sql: &str) -> anyhow::Result<Vec<String>> {
    let mut stmt = conn.prepare(sql)?;
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let n = row.as_ref().column_count();
        let mut parts = Vec::new();
        for i in 0..n {
            let v: duckdb::types::Value = row.get(i)?;
            parts.push(format!("{v:?}"));
        }
        out.push(parts.join("|"));
    }
    Ok(out)
}

/// The DuckDB version.
fn version() -> String {
    Connection::open_in_memory()
        .and_then(|c| c.query_row("SELECT version()", [], |row| row.get(0)))
        .unwrap_or_else(|_| "?".to_string())
}

/// A byte count as a human string.
fn fmt_bytes(bytes: u64) -> String {
    if bytes >= 1_000_000_000 {
        format!("{:.2} GB", bytes as f64 / 1e9)
    } else if bytes >= 1_000_000 {
        format!("{:.1} MB", bytes as f64 / 1e6)
    } else {
        format!("{:.0} KB", bytes as f64 / 1e3)
    }
}
