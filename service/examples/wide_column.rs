//! The width of the column vs the width of the values — measured properly.
//!
//! The identity's width has two layers that get mixed up: the **column**
//! (BIGINT = 8 bytes, HUGEINT = 16) and the **value** (how many bytes of
//! entropy the id actually carries). DuckDB compresses integers per vector —
//! if a vector's values all fit in 64 bits, it can pack it as 64 bits even
//! though the column is HUGEINT.
//!
//! So the question is: what does the column's *type* cost, on its own, with
//! the value's width held fixed? And is the column's type a *query* cost as
//! well as a *disk* cost?
//!
//! Every shape is measured in **its own process** (`--shape N`), so no
//! shape can inherit another's page cache, allocator or thermal state. Each
//! shape is measured twice — in a one-column table (so the column's own cost
//! is the file minus the baseline) and in the real 27-column table (so the
//! dashboard's queries are the real ones).
//!
//! The timings separate the **scan** (SUM over the column) from the **hash**
//! (COUNT DISTINCT over it) — a wide column can cost either.
//!
//! ```bash
//! cargo run --release --example wide_column -p sumimi-service -- --shape 1 8000000
//! cargo run --release --example wide_column -p sumimi-service -- --all 8000000
//! ```

use std::path::{Path, PathBuf};
use std::time::Instant;

use sumimi_service::pool::Db;

const VISITORS: i64 = 120_000;

/// The summary: the three numbers at the top of the dashboard.
const SUMMARY: &str = "SELECT COUNT(*) FILTER (WHERE event_type NOT IN ('event', 'identify')),
                              COUNT(DISTINCT visitor_id),
                              COUNT(DISTINCT visit_id)
                       FROM probe WHERE website_id = 'w1'";

/// The audience breakdown: the visitors per browser — the real report query.
const BREAKDOWN: &str = "SELECT COALESCE(browser, 'unknown') AS name, COUNT(DISTINCT visitor_id) AS value
                          FROM probe WHERE website_id = 'w1' AND event_type NOT IN ('event', 'identify')
                          GROUP BY name ORDER BY value DESC LIMIT 50";

/// The columns of the identity, in the shapes it can take.
struct Shape {
    /// What to call it.
    name: &'static str,
    /// The column types, in the order they are declared.
    ddl: &'static str,
    /// The expression that fills the narrow column.
    narrow: String,
    /// The expression that fills the wide column (if there is one).
    wide: Option<String>,
    /// Widen the column after the build — the hot upgrade.
    widen: bool,
}

/// A 64-bit value: the identity of today.
fn narrow_value() -> String {
    format!("CAST(hash('v' || (i % {VISITORS})) % 9223372036854775807 AS BIGINT)")
}

/// A 128-bit value: the identity of tomorrow — it does not fit in 64 bits.
fn wide_value() -> String {
    format!(
        "CAST(hash('a' || (i % {VISITORS})) % 9223372036854775807 AS HUGEINT) \
         * 9223372036854775808::HUGEINT \
         + CAST(hash('b' || (i % {VISITORS})) % 9223372036854775807 AS HUGEINT)"
    )
}

/// The shapes to try.
fn shapes() -> Vec<Shape> {
    let narrow = narrow_value();
    let wide = wide_value();
    vec![
        Shape {
            name: "BIGINT / 8 bytes",
            ddl: "visitor_id BIGINT",
            narrow: narrow.clone(),
            wide: None,
            widen: false,
        },
        Shape {
            name: "HUGEINT / 8 bytes",
            ddl: "visitor_id HUGEINT",
            narrow: narrow.clone(),
            wide: None,
            widen: false,
        },
        Shape {
            name: "HUGEINT / 16 bytes",
            ddl: "visitor_id HUGEINT",
            narrow: wide.clone(),
            wide: None,
            widen: false,
        },
        Shape {
            name: "UUID / 16 bytes",
            ddl: "visitor_id UUID",
            narrow: format!(
                "CAST(substr(sha256('v' || (i % {VISITORS})), 1, 8) || '-' || \
                 substr(sha256('v' || (i % {VISITORS})), 9, 4) || '-' || \
                 substr(sha256('v' || (i % {VISITORS})), 13, 4) || '-' || \
                 substr(sha256('v' || (i % {VISITORS})), 17, 4) || '-' || \
                 substr(sha256('v' || (i % {VISITORS})), 21, 12) AS UUID)"
            ),
            wide: None,
            widen: false,
        },
        Shape {
            name: "HUGEINT / 4 bytes",
            ddl: "visitor_id HUGEINT",
            narrow: format!("CAST(hash('v' || (i % {VISITORS})) % 4294967296 AS HUGEINT)"),
            wide: None,
            widen: false,
        },
        Shape {
            name: "BIGINT / 4 bytes",
            ddl: "visitor_id BIGINT",
            narrow: format!("CAST(hash('v' || (i % {VISITORS})) % 4294967296 AS BIGINT)"),
            wide: None,
            widen: false,
        },
        Shape {
            name: "HUGEINT / mixed (1 in 10 wide)",
            ddl: "visitor_id HUGEINT",
            narrow: format!("CASE WHEN i % 10 = 0 THEN {w} ELSE {n} END", n = narrow, w = wide),
            wide: None,
            widen: false,
        },
        Shape {
            name: "two columns (8 + 16, 1 in 10 wide)",
            ddl: "visitor_id BIGINT, visitor_id16 HUGEINT",
            narrow: format!("CASE WHEN i % 10 = 0 THEN NULL ELSE {n} END", n = narrow),
            wide: Some(format!("CASE WHEN i % 10 = 0 THEN {w} ELSE NULL END", w = wide)),
            widen: false,
        },
        Shape {
            name: "BIGINT, then widened (the hot upgrade)",
            ddl: "visitor_id BIGINT",
            narrow: narrow.clone(),
            wide: None,
            widen: true,
        },
    ]
}

/// Build the table with this shape's identity column(s).
fn build(path: &PathBuf, rows: i64, shape: &Shape, full: bool) -> anyhow::Result<()> {
    let _ = std::fs::remove_file(path);
    let db = Db::open(path, "256MB")?;
    if full {
        db.with_read(|conn| {
            conn.execute_batch(&format!(
                "CREATE TABLE probe (
                     id BIGINT, website_id VARCHAR, visit_id BIGINT, {ddl}, user_id VARCHAR,
                     created_at TIMESTAMP, url VARCHAR, url_query VARCHAR, title VARCHAR,
                     referrer VARCHAR, referrer_path VARCHAR, hostname VARCHAR, channel VARCHAR,
                     browser VARCHAR, browser_version VARCHAR, os VARCHAR, os_version VARCHAR,
                     device VARCHAR, screen VARCHAR, language VARCHAR, country VARCHAR,
                     region VARCHAR, city VARCHAR, latitude DOUBLE, longitude DOUBLE,
                     event_type VARCHAR);",
                ddl = shape.ddl
            ))?;
            Ok::<_, anyhow::Error>(())
        })?;
    } else {
        // The column alone, so the file minus the baseline is the column's cost.
        let first = shape.ddl.split(',').next().unwrap().trim();
        db.with_read(|conn| {
            conn.execute_batch(&format!(
                "CREATE TABLE probe (id BIGINT, {first});"
            ))?;
            Ok::<_, anyhow::Error>(())
        })?;
    }
    let wide = match &shape.wide {
        Some(expr) => format!(", {expr} AS visitor_id16"),
        None => String::new(),
    };
    let (narrow, wide) = if full {
        (shape.narrow.clone(), wide)
    } else {
        // In the one-column table the wide column has no home; the wide
        // values go into the single column instead.
        let mut expr = shape.narrow.clone();
        if shape.ddl.contains("visitor_id16") {
            expr = shape.narrow.clone();
        }
        (expr, String::new())
    };
    let insert = if full {
        format!(
            "INSERT INTO probe
             SELECT i AS id, 'w1' AS website_id,
                (((i // {VISITORS}) // 4)::BIGINT << 32) | (hash('v' || (i % {VISITORS})) % 4294967296),
                {narrow}{wide},
                CASE WHEN i % 3 = 0 THEN NULL ELSE 'user-' || (i % {VISITORS}) END,
                TIMESTAMP '2026-01-01 00:00:00' + INTERVAL (i * 4) SECOND,
                '/blog/post-' || (i % 5000),
                CASE WHEN i % 7 = 0 THEN 'utm_source=twitter&utm_campaign=spring' ELSE '' END,
                'A reasonably long article title number ' || (i % 1000),
                CASE WHEN i % 3 = 0 THEN 'https://www.google.com/search?q=some+term' ELSE '' END,
                '', 'example.com', 'Search',
                CASE (i % {VISITORS}) % 4 WHEN 0 THEN 'Chrome' WHEN 1 THEN 'Safari'
                     WHEN 2 THEN 'Firefox' ELSE 'Edge' END,
                '120', 'macOS', '10', 'desktop', '1920x1080', 'en-US', 'DE', 'Berlin', 'Berlin',
                52.52, 13.405, 'pageview'
             FROM generate_series(1, {rows}) t(i)"
        )
    } else {
        format!(
            "INSERT INTO probe
             SELECT i AS id, {narrow}
             FROM generate_series(1, {rows}) t(i)"
        )
    };
    db.with_write(|conn| {
        conn.execute_batch(&insert)?;
        Ok::<_, anyhow::Error>(())
    })?;
    db.with_read(|conn| {
        conn.execute_batch("CHECKPOINT")?;
        Ok::<_, anyhow::Error>(())
    })?;
    Ok(())
}

/// The hot upgrade: widen the column in place.
fn widen(path: &Path) -> anyhow::Result<f64> {
    let db = Db::open(path, "256MB")?;
    let started = Instant::now();
    db.with_write(|conn| {
        conn.execute_batch("ALTER TABLE probe ALTER COLUMN visitor_id TYPE HUGEINT")?;
        Ok::<_, anyhow::Error>(())
    })?;
    let ms = started.elapsed().as_secs_f64() * 1000.0;
    db.with_read(|conn| {
        conn.execute_batch("CHECKPOINT")?;
        Ok::<_, anyhow::Error>(())
    })?;
    Ok(ms)
}

/// The engine's own report: how it stored the identity column.
fn segments(path: &Path) -> String {
    let db = match Db::open(path, "256MB") {
        Ok(db) => db,
        Err(_) => return "?".to_string(),
    };
    let sql = "SELECT segment_type, compression, COUNT(*), SUM(\"count\") \
               FROM pragma_storage_info('probe') \
               WHERE column_name LIKE 'visitor_id%' \
               GROUP BY segment_type, compression ORDER BY 1, 2";
    let text: Result<String, anyhow::Error> = db.with_read(|conn| {
        let mut stmt = conn.prepare(sql)?;
        let mut rows = stmt.query([])?;
        let mut out: Vec<String> = Vec::new();
        while let Some(row) = rows.next()? {
            let kind: String = row.get(0)?;
            let how: String = row.get(1)?;
            let segments: i64 = row.get(2)?;
            let rows: i64 = row.get(3)?;
            out.push(format!("{kind}/{how}: {segments} segments, {rows} rows"));
        }
        Ok(out.join(" | "))
    });
    match text {
        Ok(t) if !t.is_empty() => t,
        Ok(_) => "nothing".to_string(),
        Err(e) => {
            eprintln!("      (the storage report failed: {e})");
            "?".to_string()
        }
    }
}

/// The four probes: they separate the **scan** from the **arithmetic** and
/// from the **hash**.
///
/// - `sum(c::BIGINT)` reads the column and adds 64-bit numbers.
/// - `sum(c)` reads the column and adds 128-bit numbers.
/// - `count(distinct c::BIGINT)` reads the column and hashes 8-byte keys.
/// - `count(distinct c)` reads the column and hashes 16-byte keys.
///
/// For a BIGINT column the casts are free, so the four lines collapse to two.
const PROBES: [&str; 4] = [
    // The scan: read the column, compare, no arithmetic that can overflow.
    "SELECT MIN(visitor_id) FROM probe",
    // The arithmetic: read the column, add 64-bit numbers.
    "SELECT SUM(visitor_id::BIGINT) FROM probe",
    // The hash: read the column, hash 8-byte keys.
    "SELECT COUNT(DISTINCT visitor_id::BIGINT) FROM probe",
    // The hash: read the column, hash 16-byte keys.
    "SELECT COUNT(DISTINCT visitor_id) FROM probe",
];

/// How long a query takes — the best of five.
///
/// A query that fails (an overflow, say) is **not** a fast query: it aborts
/// early. So a failure is reported as NaN, never as a time.
fn time(conn: &duckdb::Connection, sql: &str) -> f64 {
    let mut best = f64::MAX;
    for _ in 0..5 {
        let started = Instant::now();
        let mut stmt = match conn.prepare(sql) {
            Ok(stmt) => stmt,
            Err(_) => return f64::NAN,
        };
        let mut rows = match stmt.query([]) {
            Ok(rows) => rows,
            Err(_) => return f64::NAN,
        };
        let mut failed = false;
        loop {
            match rows.next() {
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => {
                    failed = true;
                    break;
                }
            }
        }
        if failed {
            return f64::NAN;
        }
        best = best.min(started.elapsed().as_secs_f64() * 1000.0);
    }
    best
}

/// How to print a time — a failure says so.
fn show(ms: f64) -> String {
    if ms.is_nan() {
        "failed".to_string()
    } else {
        format!("{ms:.1}ms")
    }
}

/// The measurements of one shape.
struct Numbers {
    /// The whole file, per row.
    bytes_per_row: f64,
    /// The identity column's own cost, per row (file minus the baseline).
    column_per_row: f64,
    /// The scan: sum over the column — the column is read, nothing else happens.
    scan: f64,

    /// The scan and the hash: the visitor count.
    distinct: f64,
    /// The report query.
    breakdown: f64,
    /// The whole summary.
    summary: f64,
    /// How the engine stored the column — the column alone, and the whole table.
    segments: String,
    segments_full: String,
    /// The two-column design's real query.
    two: f64,
    /// How long the column took to widen, when it was widened.
    widen_ms: Option<f64>,
    /// The four probes, on the column alone.
    probes: [f64; 4],
}

/// Measure one shape, on the full table and on the column alone.
fn measure(shape: &Shape, rows: i64) -> anyhow::Result<Numbers> {
    // The column alone.
    let path = PathBuf::from("/tmp/wide-column-one.duckdb");
    build(&path, rows, shape, false)?;
    let one = std::fs::metadata(&path)?.len() as f64 / rows as f64;
    let segs = segments(&path);
    let conn = duckdb::Connection::open(&path)?;
    let mut probes = [0.0; 4];
    for (index, sql) in PROBES.iter().enumerate() {
        probes[index] = time(&conn, sql);
    }
    drop(conn);
    let _ = std::fs::remove_file(&path);

    // The baseline: the row id alone.
    let path = PathBuf::from("/tmp/wide-column-base.duckdb");
    let _ = std::fs::remove_file(&path);
    let db = Db::open(&path, "256MB")?;
    db.with_read(|conn| {
        conn.execute_batch("CREATE TABLE probe (id BIGINT);")?;
        Ok::<_, anyhow::Error>(())
    })?;
    db.with_write(|conn| {
        conn.execute_batch(&format!(
            "INSERT INTO probe SELECT i FROM generate_series(1, {rows}) t(i)"
        ))?;
        Ok::<_, anyhow::Error>(())
    })?;
    db.with_read(|conn| {
        conn.execute_batch("CHECKPOINT")?;
        Ok::<_, anyhow::Error>(())
    })?;
    drop(db);
    let base = std::fs::metadata(&path)?.len() as f64 / rows as f64;
    let _ = std::fs::remove_file(&path);

    // The whole table.
    let path = PathBuf::from("/tmp/wide-column.duckdb");
    build(&path, rows, shape, true)?;
    let mut widen_ms = None;
    if shape.widen {
        widen_ms = Some(widen(&path)?);
    }
    let bytes = std::fs::metadata(&path)?.len() as f64 / rows as f64;
    let segs_full = segments(&path);
    let conn = duckdb::Connection::open(&path)?;
    let scan = time(&conn, "SELECT MIN(visitor_id) FROM probe");
    let distinct = time(&conn, "SELECT COUNT(DISTINCT visitor_id) FROM probe");
    // The two-column design: the two halves are disjoint by row, so the
    // visitor count is the sum of two aggregates.
    let two = if shape.wide.is_some() {
        time(
            &conn,
            "SELECT COUNT(DISTINCT visitor_id) + COUNT(DISTINCT visitor_id16) FROM probe",
        )
    } else {
        f64::NAN
    };
    let breakdown = time(&conn, BREAKDOWN);
    let summary = time(&conn, SUMMARY);
    drop(conn);
    let _ = std::fs::remove_file(&path);

    Ok(Numbers {
        bytes_per_row: bytes,
        column_per_row: one - base,
        scan,
        probes,
        distinct,
        breakdown,
        summary,
        segments: segs,
        segments_full: segs_full,
        two,
        widen_ms,
    })
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut rows = 8_000_000i64;
    let mut which: Option<usize> = None;
    let mut all = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--shape" => {
                i += 1;
                which = args.get(i).and_then(|v| v.parse().ok());
            }
            "--all" => all = true,
            v => {
                if let Ok(parsed) = v.parse::<i64>() {
                    rows = parsed;
                }
            }
        }
        i += 1;
    }

    let shapes = shapes();
    let picked: Vec<usize> = if all {
        (0..shapes.len()).collect()
    } else {
        vec![which.unwrap_or(0)]
    };

    println!("{rows} rows\n");
    println!(
        "  {:<36} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9}",
        "the shape", "B/row", "the col", "the scan", "distinct", "browsers", "summary"
    );
    for index in picked {
        let shape = &shapes[index];
        let n = measure(shape, rows)?;
        println!(
            "  {:<36} {:>9.2} {:>9.2} {:>8.1}ms {:>8.1}ms {:>8.1}ms {:>8.1}ms",
            shape.name,
            n.bytes_per_row,
            n.column_per_row,
            n.scan,
            n.distinct,
            n.breakdown,
            n.summary
        );
        if !n.two.is_nan() {
            println!("      the two-column count (both halves): {}", show(n.two));
        }
        println!(
            "      the column alone: the scan {}, sum(64) {}, distinct(8) {}, distinct(16) {}",
            show(n.probes[0]),
            show(n.probes[1]),
            show(n.probes[2]),
            show(n.probes[3]),
        );
        if let Some(ms) = n.widen_ms {
            println!("      widening the column took: {ms:.1}ms");
        }
        println!("      the engine stored it as: {}", n.segments);
        if n.segments_full != n.segments {
            println!("      in the whole table:              {}", n.segments_full);
        }
    }
    Ok(())
}
