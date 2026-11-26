//! **The event id's shapes, measured side by side.**
//!
//! mneme #171. D-060 made the event id one packed 128-bit value: the
//! instance's half (74 constant bits) and a dense counter (48 bits). The
//! engine bit packs that column — the instance half costs nothing, which is
//! where "the same price as the engine's own row id" came from — but the
//! read side pays for it: every row is expanded back to 128 bits.
//!
//! This measures the shapes side by side, on the three axes the owner named
//! (disk, memory, cpu), at the product's own ceiling:
//!
//! | shape | what is stored |
//! |---|---|
//! | `uuid16` | one `UUID` column — today's shape |
//! | `blob16` | one `BLOB` column, the same sixteen bytes |
//! | `blob12_blob4` | `BLOB` instance + `BLOB` counter |
//! | `blob12_int4` | `BLOB` instance + `INTEGER` counter |
//! | `blob16_bigint` | `BLOB` instance + `BIGINT` counter |
//!
//! The composite shapes hold the instance and the counter apart so that the
//! engine picks a method for each: the instance column has one value (or a
//! few, when a site moved and kept the rows of the instance it came from),
//! and the counter column is an integer.
//!
//! **The id is composed on read** in the composite shapes — the API's id and
//! the report's `ORDER BY event_id` both need it — so `scan_id` and
//! `distinct_id` carry that cost, and `scan_stored` (the stored columns
//! alone) is the number to compare with the single-column shapes.
//!
//! ```bash
//! cargo run --release --example event_id_shapes -p sumimi-service -- \
//!   --rows 20000000 --shapes uuid16,blob16,blob12_blob4,blob12_int4,blob16_bigint
//! ```

use std::fs;
use std::path::Path;
use std::time::Instant;

use duckdb::Connection;
use sumimi_service::db::Duck;
use sumimi_service::event_id;
use sumimi_service::schema;

/// The product's rule for "this row is a page view" (`storage::PAGEVIEWS`).
const PAGEVIEWS: &str = "event_type NOT IN ('event', 'identify', 'performance')";

/// The ceiling the product gives DuckDB (`config::MEMORY_LIMIT_MAX`).
const PRODUCT_MEMORY_LIMIT: &str = "256MB";

/// One shape of the event id.
struct Shape {
    name: &'static str,
    /// The columns that replace `event_id`, in order: (name, declaration).
    columns: Vec<(String, String)>,
    /// The select list that fills them, in the same order.
    select: Vec<String>,
    /// The id as one value — the API's id, and what a distinct is taken over.
    id: String,
    /// The stored column a distinct is taken over when the id is composed.
    stored: String,
    /// The two columns that make the id, as one expression — what a distinct
    /// is taken over when the id is never composed.
    pair: String,
    /// The order the event log sorts by. For the composite shapes it is the
    /// two stored columns: the id's order **is** their order, so the log can
    /// sort without composing the id for every row.
    order: String,
}

/// The thirty-two hex characters as a uuid.
///
/// DuckDB 2.0's runtime cast from a blob to a uuid reads the blob's bytes as
/// *text*, so a uuid built out of `from_hex` only works when the engine
/// folds it at plan time. The hyphenated text form is the one that always
/// works — and it is the form a uuid is written in anyway.
fn uuid_text(hex: &str) -> String {
    format!(
        "CAST(substr({hex},1,8)||'-'||substr({hex},9,4)||'-'||substr({hex},13,4)||'-'||\
         substr({hex},17,4)||'-'||substr({hex},21,12) AS UUID)"
    )
}

/// The instance half of each instance, as the twenty hex characters of the
/// id's first ten bytes (they carry the half and the version and variant
/// bits).
///
/// The first instance is the real one; the others are drawn from the same
/// source, so a file can hold more than one value — a site that moved keeps
/// the rows of the instance it came from.
fn instance_hexes(instances: i64) -> Vec<String> {
    (0..instances)
        .map(|index| {
            let half = if index == 0 {
                0x0f1e2d3c4b5a69788796u128
            } else {
                0x0f1e2d3c4b5a69788796u128 ^ ((index as u128) * 0x9e3779b97f4a7c15)
            };
            event_id::instance_hex(half & event_id::INSTANCE_MAX)
        })
        .collect()
}

/// The instance half of a row, as an expression: the rows are split into
/// equal blocks, one per instance.
fn instance_expr(instances: i64, hexes: &[String], rows: i64) -> String {
    if instances <= 1 {
        return format!("'{}'", hexes[0]);
    }
    let block = rows / instances;
    let arms: Vec<String> = (1..instances)
        .map(|index| {
            format!(
                "WHEN s.i < {} THEN '{}'",
                block * index,
                hexes[index as usize]
            )
        })
        .collect();
    format!("CASE {} ELSE '{}' END", arms.join(" "), hexes[0])
}

/// The five shapes.
fn shapes(instances: i64, rows: i64) -> Vec<Shape> {
    let hexes = instance_hexes(instances);
    let inst = instance_expr(instances, &hexes, rows);

    // The id composed from the two stored columns: the instance's first ten
    // bytes and the counter's six bytes, big-endian.
    let composed = "from_hex(substr(hex(instance_id), 1, 20) || lpad(to_hex(num), 12, '0'))";

    vec![
        // auto int — a plain sequence in a `BIGINT` column: **what the id
        // was before it was ours** (`service/examples/id_disk.rs` 的
        // `rowid_expression` 就是 `CAST(i AS BIGINT)`). It gives up three
        // things (the id repeats after a site moves, it is not stable
        // against deletes, and it is not a uuid to hand out) — this measures
        // what it buys.
        Shape {
            name: "autoint",
            columns: vec![("event_id".to_string(), "BIGINT NOT NULL".to_string())],
            select: vec!["CAST(s.i + 1 AS BIGINT)".to_string()],
            id: "event_id".to_string(),
            stored: "event_id".to_string(),
            pair: "event_id".to_string(),
            order: "event_id DESC".to_string(),
        },
        // 乙 — today's shape: one packed 128-bit uuid.
        Shape {
            name: "uuid16",
            columns: vec![("event_id".to_string(), "UUID NOT NULL".to_string())],
            select: vec![uuid_text(&format!("{inst}||lpad(to_hex(s.i), 12, '0')"))],
            id: "event_id".to_string(),
            stored: "event_id".to_string(),
            pair: "event_id".to_string(),
            order: "event_id DESC".to_string(),
        },
        // 丙 — the same sixteen bytes, in a blob.
        Shape {
            name: "blob16",
            columns: vec![("event_id".to_string(), "BLOB NOT NULL".to_string())],
            select: vec![format!("from_hex({inst}||lpad(to_hex(s.i), 12, '0'))")],
            id: "event_id".to_string(),
            stored: "event_id".to_string(),
            pair: "event_id".to_string(),
            order: "event_id DESC".to_string(),
        },
        // 甲 — composite: the instance in a twelve-byte blob, the counter in
        // a four-byte blob.
        Shape {
            name: "blob12_blob4",
            columns: vec![
                ("instance_id".to_string(), "BLOB NOT NULL".to_string()),
                ("num".to_string(), "BLOB NOT NULL".to_string()),
            ],
            select: vec![
                format!("from_hex({inst}||'0000')"),
                "from_hex(lpad(to_hex(s.i), 8, '0'))".to_string(),
            ],
            id: composed.to_string(),
            stored: "num".to_string(),
            pair: "(instance_id, num)".to_string(),
            order: "instance_id DESC, num DESC".to_string(),
        },
        // 甲二 — the same, with the counter in an integer column.
        Shape {
            name: "blob12_int4",
            columns: vec![
                ("instance_id".to_string(), "BLOB NOT NULL".to_string()),
                ("num".to_string(), "INTEGER NOT NULL".to_string()),
            ],
            select: vec![
                format!("from_hex({inst}||'0000')"),
                "CAST(s.i AS INTEGER)".to_string(),
            ],
            id: composed.to_string(),
            stored: "num".to_string(),
            pair: "(instance_id, num)".to_string(),
            order: "instance_id DESC, num DESC".to_string(),
        },
        // 丁 — the instance in a sixteen-byte blob, the counter a bigint.
        Shape {
            name: "blob16_bigint",
            columns: vec![
                ("instance_id".to_string(), "BLOB NOT NULL".to_string()),
                ("num".to_string(), "BIGINT NOT NULL".to_string()),
            ],
            select: vec![
                format!("from_hex({inst}||'000000000000')"),
                "CAST(s.i AS BIGINT)".to_string(),
            ],
            id: composed.to_string(),
            stored: "num".to_string(),
            pair: "(instance_id, num)".to_string(),
            order: "instance_id DESC, num DESC".to_string(),
        },
    ]
}

/// The traffic: the real schema's columns, filled with plausible traffic.
///
/// The shape of the traffic matters for the numbers, so it is modelled
/// rather than invented:
///
/// - a **visit** is four events on average;
/// - a **visitor** has a couple of visits, and one visitor in five is a
///   heavy one (a fifth of the visits).
///
/// The two identity columns (`session_id`, `visit_id`) keep the shape the
/// service writes today: they are the control. Every shape is measured on
/// the same traffic, so the numbers are comparable.
fn traffic(shape: &Shape, rows: i64, interleave: i64, days: i64) -> String {
    let events_per_visit = 4i64;
    let block = events_per_visit * interleave;
    let visit_expr = format!("((i // {block}) * {interleave} + (i % {block}) % {interleave})");
    let visitors_expr = format!(
        "CASE WHEN ({visit_expr}) % 5 = 0 THEN 0 ELSE (({visit_expr}) * 2654435761) % {{VISITORS}} END"
    );
    let visitors_expr_outer = visitors_expr.replace("(i //", "(s.i //");

    let id_columns: Vec<String> = shape.columns.iter().map(|(name, _)| name.clone()).collect();
    let id_values: Vec<String> = shape.select.clone();

    // The id's columns come first; with the rowid shape there are none, so
    // the list is built rather than pasted.
    let mut insert_columns = id_columns.clone();
    insert_columns.extend(
        [
            "website_id", "visit_id", "session_id", "ip", "ip_prefix", "user_agent", "user_id",
            "created_at", "url_path", "url_query", "page_title", "referrer_domain", "referrer_path",
            "referrer_query", "referrer_name", "hostname", "channel", "browser", "browser_version",
            "os", "os_version", "device", "screen", "language", "country", "region", "city",
            "latitude", "longitude", "utm_source", "utm_medium", "utm_campaign", "utm_content",
            "utm_term", "gclid", "fbclid", "msclkid", "ttclid", "li_fat_id", "twclid", "lcp", "inp",
            "cls", "fcp", "ttfb", "event_type", "event_name", "tag", "data",
        ]
        .iter()
        .map(|name| name.to_string()),
    );

    format!(
        "INSERT INTO website_event
         ({id_columns})
         SELECT {id_values}
            'w1' AS website_id,
            {visit_value} AS visit_id,
            {session_value} AS session_id,
            '' AS ip,
            '203.0.' || CAST(s.i % 256 AS VARCHAR) || '.0' AS ip_prefix,
            CASE (s.i % 50)
                WHEN 0 THEN 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36'
                WHEN 1 THEN 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36'
                WHEN 2 THEN 'Mozilla/5.0 (iPhone; CPU iPhone OS 17_2 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.2 Mobile/15E148 Safari/604.1'
                WHEN 3 THEN 'Mozilla/5.0 (X11; Linux x86_64; rv:121.0) Gecko/20100101 Firefox/121.0'
                ELSE 'Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Mobile Safari/537.36'
            END AS user_agent,
            NULL AS user_id,
            TIMESTAMP '2026-01-01 00:00:00'
              + INTERVAL (s.i * {SECONDS_PER_ROW}) SECOND AS created_at,
            '/blog/post-' || (s.i % 5000) AS url_path,
            CASE WHEN s.i % 7 = 0 THEN 'utm_source=twitter&utm_campaign=spring' ELSE '' END AS url_query,
            'A reasonably long article title number ' || (s.i % 1000) AS page_title,
            CASE WHEN s.i % 3 = 0 THEN 'https://www.google.com/search?q=some+term' ELSE '' END AS referrer_domain,
            CASE WHEN s.i % 3 = 0 THEN '/search' ELSE '' END AS referrer_path,
            '' AS referrer_query, '' AS referrer_name,
            'example.com' AS hostname, 'Search' AS channel,
            CASE ({visitors_expr_outer}) % 4 WHEN 0 THEN 'Chrome' WHEN 1 THEN 'Safari'
                 WHEN 2 THEN 'Firefox' ELSE 'Edge' END AS browser,
            '120.0.0.0' AS browser_version,
            CASE ({visitors_expr_outer}) % 4 WHEN 0 THEN 'macOS' WHEN 1 THEN 'Windows'
                 WHEN 2 THEN 'iOS' ELSE 'Linux' END AS os,
            '10.15.7' AS os_version,
            CASE ({visitors_expr_outer}) % 4 WHEN 0 THEN 'desktop' WHEN 1 THEN 'desktop'
                 WHEN 2 THEN 'mobile' ELSE 'tablet' END AS device,
            '1920x1080' AS screen, 'en-US' AS language, 'DE' AS country, 'Berlin' AS region,
            'Berlin' AS city, 52.52 AS latitude, 13.405 AS longitude,
            '' AS utm_source, '' AS utm_medium, '' AS utm_campaign, '' AS utm_content, '' AS utm_term,
            '' AS gclid, '' AS fbclid, '' AS msclkid, '' AS ttclid, '' AS li_fat_id, '' AS twclid,
            0.01 AS lcp, 0.02 AS inp, 0.03 AS cls, 0.4 AS fcp, 0.8 AS ttfb,
            'pageview' AS event_type, '' AS event_name, '' AS tag, '' AS data
         FROM (SELECT i,
                      sha256('v' || CAST({visitors_expr} AS VARCHAR)) AS hs,
                      sha256('s' || CAST({visit_expr} AS VARCHAR)) AS hv
               FROM range(0, {{ROWS}}) t(i)) s",
        id_columns = insert_columns.join(", "),
        // Each id value carries its own trailing comma, so the rowid shape
        // (no id column at all) leaves the list empty rather than starting
        // it with a comma.
        id_values = id_values
            .iter()
            .map(|value| format!("{value}, "))
            .collect::<Vec<String>>()
            .join(""),
        visit_value = uuid_text("s.hv"),
        session_value = uuid_text("s.hs"),
        visit_expr = visit_expr,
        SECONDS_PER_ROW = (days * 86_400) as f64 / rows as f64,
        visitors_expr = visitors_expr,
        visitors_expr_outer = visitors_expr_outer
    )
}

/// The statements that are measured.
fn queries(shape: &Shape, days: i64) -> Vec<(&'static str, String)> {
    let window = format!(
        "created_at >= TIMESTAMP '2026-01-01' AND created_at < TIMESTAMP '2026-01-01' + INTERVAL {days} DAY"
    );
    vec![
        // The stored columns alone: the cost of reading what is stored.
        (
            "scan_stored",
            format!("SELECT max({stored}) FROM website_event", stored = shape.stored),
        ),
        // The id as the API sees it — for the composite shapes this composes
        // the two columns, so it carries that cost too.
        (
            "scan_id",
            format!("SELECT max({id}) FROM website_event", id = shape.id),
        ),
        // A scan the optimizer cannot skip: the sum of the hashes is the
        // hash the aggregate would pay anyway, but the column is read.
        (
            "scan_hash_stored",
            format!(
                "SELECT sum(hash({stored})) FROM website_event",
                stored = shape.stored
            ),
        ),
        (
            "scan_hash_id",
            format!("SELECT sum(hash({id})) FROM website_event", id = shape.id),
        ),
        // The hash table over the stored column: the memory story.
        (
            "distinct_stored",
            format!(
                "SELECT count(DISTINCT {stored}) FROM website_event",
                stored = shape.stored
            ),
        ),
        // The hash table over the id.
        (
            "distinct_id",
            format!("SELECT count(DISTINCT {id}) FROM website_event", id = shape.id),
        ),
        // The product's event log (`query.rs`), with the id the API returns.
        (
            "event_log",
            format!(
                "SELECT {id} AS event_id, website_id, visit_id, session_id, created_at, event_name, \
                 url_path, hostname, channel, data, referrer_domain, referrer_path, referrer_query, \
                 country, city, browser, os, utm_source, utm_medium, utm_campaign, lcp, inp, cls, device \
                 FROM website_event WHERE {window} \
                 ORDER BY created_at DESC, {id} DESC LIMIT 50",
                id = shape.id
            ),
        ),
        // The distinct over the **pair**, without composing an id at all:
        // the pair is what makes the id unique.
        (
            "distinct_pair",
            format!(
                "SELECT count(DISTINCT ({pair})) FROM website_event",
                pair = shape.pair
            ),
        ),
        // The same log, sorted by the stored columns instead of by the
        // composed id: the id's order is their order, so the composition is
        // only needed for the rows that come back.
        (
            "event_log_cols",
            format!(
                "SELECT {id} AS event_id, website_id, visit_id, session_id, created_at, event_name, \
                 url_path, hostname, channel, data, referrer_domain, referrer_path, referrer_query, \
                 country, city, browser, os, utm_source, utm_medium, utm_campaign, lcp, inp, cls, device \
                 FROM website_event WHERE {window} \
                 ORDER BY created_at DESC, {order} LIMIT 50",
                id = shape.id,
                order = shape.order
            ),
        ),
        // The product's summary (`query.rs`) — the control: it does not read
        // the event id, so it must come out the same for every shape.
        (
            "summary",
            format!(
                "SELECT COUNT(*) FILTER (WHERE {PAGEVIEWS}),
                        COUNT(DISTINCT CASE WHEN {PAGEVIEWS} THEN session_id END),
                        COUNT(DISTINCT CASE WHEN {PAGEVIEWS} THEN visit_id END)
                 FROM website_event WHERE {window}"
            ),
        ),
        // The product's bounces (`query.rs`) — the control.
        (
            "bounces",
            format!(
                "SELECT COUNT(*) FILTER (WHERE n = 1 AND custom = 0)
                 FROM (
                     SELECT visit_id,
                            COUNT(*) FILTER (WHERE {PAGEVIEWS}) AS n,
                            MAX(CASE WHEN event_type = 'event' THEN 1 ELSE 0 END) AS custom
                     FROM website_event WHERE {window}
                     GROUP BY visit_id
                 )"
            ),
        ),
    ]
}

/// What the engine's own accounting says about the memory in use.
fn memory(conn: &Connection) -> anyhow::Result<(i64, i64)> {
    let mut stmt = conn.prepare(
        "SELECT coalesce(sum(memory_usage_bytes), 0), coalesce(sum(temporary_storage_bytes), 0)
         FROM duckdb_memory()",
    )?;
    let mut rows = stmt.query([])?;
    let mut out = (0i64, 0i64);
    if let Some(row) = rows.next()? {
        out = (row.get(0)?, row.get(1)?);
    }
    Ok(out)
}

/// How the engine stores the id's columns. **After the checkpoint**: before
/// it, the engine reports what is in memory, not what is on disk.
fn storage(conn: &Connection, columns: &[String]) -> anyhow::Result<Vec<String>> {
    let list = columns
        .iter()
        .map(|name| format!("'{name}'"))
        .collect::<Vec<String>>()
        .join(", ");
    let mut stmt = conn.prepare(&format!(
        "SELECT DISTINCT column_name, compression FROM pragma_storage_info('website_event')
         WHERE column_name IN ({list}) AND column_path NOT LIKE '%,%' ORDER BY 1, 2"
    ))?;
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let column: String = row.get(0)?;
        let compression: String = row.get(1)?;
        out.push(format!("{column}={compression}"));
    }
    Ok(out)
}

/// Run a query, the best of three, and report the engine's memory after it.
fn timed(conn: &Connection, sql: &str, repeats: usize) -> anyhow::Result<(f64, i64, i64)> {
    // The statement is prepared once: the plan is not what is being measured.
    let mut stmt = conn.prepare(sql)?;
    let mut best = f64::MAX;
    let mut mem = (0i64, 0i64);
    for _ in 0..repeats {
        let started = Instant::now();
        let outcome = (|| -> anyhow::Result<f64> {
            let mut rows = stmt.query([])?;
            while rows.next()?.is_some() {}
            Ok(started.elapsed().as_secs_f64() * 1000.0)
        })();
        match outcome {
            Ok(elapsed) => {
                best = best.min(elapsed);
                mem = memory(conn)?;
            }
            // A query that cannot fit under the ceiling is a **result**, not a
            // crash: that is the product's memory story, and it is the point.
            Err(e) => {
                let message = e.to_string();
                if message.contains("Out of Memory") || message.contains("out of memory") {
                    return Ok((f64::INFINITY, mem.0, mem.1));
                }
                return Err(e);
            }
        }
    }
    Ok((best, mem.0, mem.1))
}

/// A time, or the word for the query that did not fit.
fn show(ms: f64) -> String {
    if ms.is_finite() {
        format!("{ms:.1}")
    } else {
        "oom".to_string()
    }
}

/// One shape, measured.
struct Row {
    name: String,
    hot_b_per_row: f64,
    id_b_per_row: f64,
    cold_b_per_row: f64,
    write_ms: f64,
    how: String,
    timings: Vec<(String, f64)>,
    mem_bytes: i64,
    spill_bytes: i64,
    events: i64,
}

fn measure(
    shape: &Shape,
    rows: i64,
    visitors: i64,
    memory_limit: &str,
    dir: &Path,
    parquet: bool,
    repeats: usize,
    interleave: i64,
    days: i64,
    threads: i64,
) -> anyhow::Result<Row> {
    let path = dir.join(format!("{}.duckdb", shape.name));
    let _ = fs::remove_file(&path);
    let _ = fs::remove_file(dir.join(format!("{}.duckdb.wal", shape.name)));

    let conn = Connection::open(&path)?;
    conn.execute_batch(&format!("SET temp_directory='{}'", dir.display()))?;
    // The load is given room: the service writes row by row through an
    // appender, so it never holds a table's worth of rows, while this
    // harness's one statement does. The queries below run under the
    // product's own ceiling — that is the one being measured.
    conn.execute_batch("SET memory_limit='8GB'")?;
    if threads > 0 {
        conn.execute_batch(&format!("SET threads={threads}"))?;
    }

    // The real schema, with the event id's columns in the shape's place.
    // The id's columns come first, the way the schema's own first column
    // (the event id) does.
    let mut ordered: Vec<String> = shape
        .columns
        .iter()
        .map(|(name, declaration)| format!("{name} {declaration}"))
        .collect();
    ordered.extend(
        schema::EVENT
            .iter()
            .filter(|column| column.name != "event_id")
            .map(|column| schema::column_ddl(column, &Duck)),
    );
    conn.execute_batch(&format!(
        "CREATE TABLE website_event ({});",
        ordered.join(", ")
    ))?;

    let sql = traffic(shape, rows, interleave, days)
        .replace("{ROWS}", &rows.to_string())
        .replace("{VISITORS}", &visitors.to_string());

    let started = Instant::now();
    conn.execute_batch(&sql)?;
    let write_ms = started.elapsed().as_secs_f64() * 1000.0;
    conn.execute_batch("CHECKPOINT;")?;
    // From here on the ceiling is the product's.
    conn.execute_batch(&format!("SET memory_limit='{memory_limit}'"))?;

    let hot_b_per_row = fs::metadata(&path)?.len() as f64 / rows as f64;
    let id_names: Vec<String> = shape.columns.iter().map(|(name, _)| name.clone()).collect();
    let how = storage(&conn, &id_names)?.join(" ");

    // The same table without the id's columns, in a **duckdb** file of its
    // own: the difference is what the id costs (the way #145 measured it —
    // and it has to be the same engine, or the difference is the difference
    // between two formats).
    let without = dir.join(format!("{}-without.duckdb", shape.name));
    let _ = fs::remove_file(&without);
    let _ = conn.execute_batch("SET memory_limit='8GB'");
    let without_b_per_row = match (|| -> anyhow::Result<f64> {
        // A database file of its own, holding the same rows without the id:
        // the difference between the two files is what the id costs. (It has
        // to be the same engine, or the difference is the difference between
        // two formats.)
        conn.execute_batch(&format!("ATTACH '{}' AS without_db", without.display()))?;
        conn.execute_batch(&format!(
            "CREATE TABLE without_db.website_event AS SELECT * EXCLUDE ({}) FROM website_event",
            id_names.join(", ")
        ))?;
        conn.execute_batch("DETACH without_db")?;
        Ok(fs::metadata(&without)?.len() as f64 / rows as f64)
    })() {
        Ok(value) => value,
        Err(e) => {
            println!("    (the copy without the id failed: {e})");
            f64::NAN
        }
    };
    let _ = fs::remove_file(&without);
    let _ = conn.execute_batch(&format!("SET memory_limit='{memory_limit}'"));
    let id_b_per_row = hot_b_per_row - without_b_per_row;

    // The cold tier: the archive's copy.
    let mut cold_b_per_row = f64::NAN;
    if parquet {
        let out = dir.join(format!("{}.parquet", shape.name));
        let _ = fs::remove_file(&out);
        let _ = conn.execute_batch("SET memory_limit='8GB'");
        let outcome = conn.execute_batch(&format!(
            "COPY (SELECT * FROM website_event) TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD);",
            out.display()
        ));
        let _ = conn.execute_batch(&format!("SET memory_limit='{memory_limit}'"));
        if outcome.is_ok() {
            cold_b_per_row = fs::metadata(&out)?.len() as f64 / rows as f64;
        }
        let _ = fs::remove_file(&out);
    }

    // The counts — the invariant every shape has to keep.
    let events: i64 = conn
        .query_row("SELECT count(*) FROM website_event", [], |row| row.get(0))
        .unwrap_or(-1);

    let mut timings = Vec::new();
    let mut mem_bytes = 0i64;
    let mut spill_bytes = 0i64;
    for (name, sql) in queries(shape, days) {
        let (ms, mem, spill) = timed(&conn, &sql, repeats)?;
        timings.push((name.to_string(), ms));
        mem_bytes = mem_bytes.max(mem);
        spill_bytes = spill_bytes.max(spill);
    }

    drop(conn);
    let _ = fs::remove_file(&path);

    Ok(Row {
        name: shape.name.to_string(),
        hot_b_per_row,
        id_b_per_row,
        cold_b_per_row,
        write_ms,
        how,
        timings,
        mem_bytes,
        spill_bytes,
        events,
    })
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut rows: i64 = 20_000_000;
    let mut visitors: i64 = 1_000_000;
    let mut only: Option<Vec<String>> = None;
    let mut memory_limit = PRODUCT_MEMORY_LIMIT.to_string();
    let mut parquet = true;
    let mut repeats = 3usize;
    let mut interleave = 4i64;
    let mut days = 90i64;
    let mut instances = 1i64;
    let mut threads = 0i64;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--rows" => {
                rows = args[i + 1].parse()?;
                i += 2;
            }
            "--visitors" => {
                visitors = args[i + 1].parse()?;
                i += 2;
            }
            "--shapes" => {
                only = Some(
                    args[i + 1]
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .collect(),
                );
                i += 2;
            }
            "--memory-limit" => {
                memory_limit = args[i + 1].clone();
                i += 2;
            }
            "--no-parquet" => {
                parquet = false;
                i += 1;
            }
            "--repeats" => {
                repeats = args[i + 1].parse()?;
                i += 2;
            }
            "--interleave" => {
                interleave = args[i + 1].parse()?;
                i += 2;
            }
            "--days" => {
                days = args[i + 1].parse()?;
                i += 2;
            }
            "--instances" => {
                instances = args[i + 1].parse()?;
                i += 2;
            }
            "--threads" => {
                threads = args[i + 1].parse()?;
                i += 2;
            }
            other => anyhow::bail!("unknown argument: {other}"),
        }
    }

    let dir = std::env::temp_dir().join("sumimi-event-id-shapes");
    fs::create_dir_all(&dir)?;

    let all = shapes(instances, rows);
    let wanted: Vec<Shape> = match &only {
        Some(names) => all
            .into_iter()
            .filter(|shape| names.iter().any(|name| name == shape.name))
            .collect(),
        None => all,
    };
    if wanted.is_empty() {
        anyhow::bail!("no shape matched");
    }

    println!(
        "rows {rows}, visitors {visitors}, instances {instances}, ceiling {memory_limit}, threads {threads}"
    );
    println!();

    for shape in &wanted {
        let row = measure(
            shape,
            rows,
            visitors,
            &memory_limit,
            &dir,
            parquet,
            repeats,
            interleave,
            days,
            threads,
        )?;
        println!("  {}", row.name);
        let cold = if row.cold_b_per_row.is_finite() {
            format!("{:.2}", row.cold_b_per_row)
        } else {
            "-".to_string()
        };
        println!(
            "    hot {hot:>8.2} B/row   the id {id:>7.2}   cold {cold:>8}   write {write:.0} ms",
            hot = row.hot_b_per_row,
            id = row.id_b_per_row,
            write = row.write_ms
        );
        println!("    stored as: {}", row.how);
        println!("    events: {}", row.events);
        for (name, ms) in &row.timings {
            println!("    {name:<16} {:>10} ms", show(*ms));
        }
        println!(
            "    memory {:>8.1} MiB   spill {:>8.1} MiB",
            row.mem_bytes as f64 / 1_048_576.0,
            row.spill_bytes as f64 / 1_048_576.0
        );
        println!();
    }

    Ok(())
}
