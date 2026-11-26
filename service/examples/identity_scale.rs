//! **The cost of the self-describing id, measured at scale.**
//!
//! The proposal (mneme #165): the identity columns carry a **constant marker
//! in the high bytes** and the entropy in the low ones, so the engine's
//! bit packing eats the marker and the column costs only its entropy.
//! #165 measured that at 2 M rows on a machine with no memory limit, and
//! found "disk −17…38 %, queries 9…11 % slower".
//!
//! This measures it where the decision is made:
//!
//! - **at scale** (millions of rows, not thousands);
//! - **with the real schema** (`schema::EVENT`, the 51 columns the service
//!   creates), the real `event_id` (the v8 layout), and the real report
//!   queries (`query.rs`, verbatim);
//! - **with real cardinality** (visitors repeat, visits belong to visitors);
//! - **under the product's own memory ceiling** (`config.rs`: a quarter of
//!   RAM, capped at 256 MB) — a machine with 128 GB of RAM never spills, and
//!   spilling is where the memory question is decided;
//! - **on the three axes the owner named**: disk, memory, cpu.
//!
//! ```bash
//! cargo run --release --example identity_scale -p sumimi-service -- --rows 20000000
//! # one layout per process, so the peak RSS is attributable:
//! cargo run --release --example identity_scale -p sumimi-service -- --rows 20000000 --layouts selfdesc8
//! ```

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use duckdb::Connection;
use sumimi_service::db::Duck;
use sumimi_service::event_id;
use sumimi_service::schema;

/// The product's rule for "this row is a page view" (`storage::PAGEVIEWS`).
const PAGEVIEWS: &str = "event_type NOT IN ('event', 'identify', 'performance')";

/// The ceiling the product gives DuckDB (`config::MEMORY_LIMIT_MAX`).
const PRODUCT_MEMORY_LIMIT: &str = "256MB";

/// One shape of the two identity columns.
#[derive(Clone, Copy)]
struct Layout {
    name: &'static str,
    /// the column's declaration — the type is part of the shape
    ddl: &'static str,
    /// the SQL that makes the value: given an expression for the entropy's
    /// thirty-two hex characters and the visit's number
    value: fn(&str, &str) -> String,
    /// how many bits of entropy the value carries
    entropy_bits: usize,
}

/// The thirty-two hex characters as a uuid.
///
/// DuckDB 2.0's runtime cast from a blob to a uuid reads the blob's bytes as
/// *text*, so a uuid built out of `from_hex` only works when the engine
/// folds it at plan time. The hyphenated text form is the one that always
/// works — and it is the form a uuid is written in anyway.
fn uuid_text(hex: &str) -> String {
    format!(
        "CAST(substr({hex},1,8)||'-'||substr({hex},9,4)||'-'||substr({hex},13,4)||'-'\
         ||substr({hex},17,4)||'-'||substr({hex},21,12) AS UUID)"
    )
}

/// The self-describing marker: ones down to a single zero, whose position
/// says how wide the entropy is. The marker is (16 − entropy) bytes long and
/// the entropy follows it.
///
/// The zero is the lowest bit of the marker's last byte, so the width can be
/// read off the value without a side column.
fn marker_hex(entropy_bytes: usize) -> String {
    let marker_bytes = 16 - entropy_bytes;
    let mut hex = "f".repeat((marker_bytes - 1) * 2);
    hex.push_str("fe");
    hex
}

/// Sixteen bytes of entropy — the shape the service writes today.
fn today(hex: &str, _visit: &str) -> String {
    uuid_text(hex)
}

/// The marker layout, one function per entropy width.
fn selfdesc6(hex: &str, _visit: &str) -> String {
    uuid_text(&format!("'{}'||substr({hex},1,12)", marker_hex(6)))
}
fn selfdesc8(hex: &str, _visit: &str) -> String {
    uuid_text(&format!("'{}'||substr({hex},1,16)", marker_hex(8)))
}
fn selfdesc10(hex: &str, _visit: &str) -> String {
    uuid_text(&format!("'{}'||substr({hex},1,20)", marker_hex(10)))
}
fn selfdesc12(hex: &str, _visit: &str) -> String {
    uuid_text(&format!("'{}'||substr({hex},1,24)", marker_hex(12)))
}

/// The same bytes, in a blob — a carrier that is not a uuid, so it is never
/// bit packed, but whose width is not in its type either: **the length of a
/// blob says how wide it is** ⇒ the marker is not needed.
fn blob6(hex: &str, _visit: &str) -> String {
    format!("from_hex(substr({hex},1,12))")
}
fn blob8(hex: &str, _visit: &str) -> String {
    format!("from_hex(substr({hex},1,16))")
}
fn blob10(hex: &str, _visit: &str) -> String {
    format!("from_hex(substr({hex},1,20))")
}
fn blob12(hex: &str, _visit: &str) -> String {
    format!("from_hex(substr({hex},1,24))")
}

/// The marker's layout in a **blob** carrier: sixteen bytes, the high half
/// the marker, the low half the entropy. The value is a uuid's bytes, but
/// the column is not a uuid — so the engine never bit packs it (⇒ no
/// unpack) and the length is not the width (⇒ the marker still has to say).
fn blob16marker(hex: &str, _visit: &str) -> String {
    format!("from_hex('ffffffffffffffff'||substr({hex},1,16))")
}

/// The same, with the self-describing marker.
fn blob16selfdesc(hex: &str, _visit: &str) -> String {
    format!("from_hex('fffffffffffffffffffe'||substr({hex},1,16))")
}

/// **A plain counter**: one number per visit, the way the identity line's
/// design wants it (a per-site counter; the site's scope comes from the
/// report's `website_id` filter, so it is not in the value).
fn counter4(_hex: &str, visit: &str) -> String {
    format!("CAST({visit} AS INTEGER)")
}

/// The identity as one constant value: the file is then the row without the
/// identity's cost, which is how the identity's share of the row is found.
fn constant(_hex: &str, _visit: &str) -> String {
    "CAST('ffffffff-ffff-ffff-ffff-ffffffffffff' AS UUID)".to_string()
}

/// The competitors: the marker layout and the blob, each at the four entropy
/// widths, plus today's uuid as the baseline to measure against.
fn layouts() -> Vec<Layout> {
    vec![
        Layout { name: "today", ddl: "UUID NOT NULL", value: today, entropy_bits: 128 },
        Layout { name: "selfdesc6", ddl: "UUID NOT NULL", value: selfdesc6, entropy_bits: 48 },
        Layout { name: "selfdesc8", ddl: "UUID NOT NULL", value: selfdesc8, entropy_bits: 64 },
        Layout { name: "selfdesc10", ddl: "UUID NOT NULL", value: selfdesc10, entropy_bits: 80 },
        Layout { name: "selfdesc12", ddl: "UUID NOT NULL", value: selfdesc12, entropy_bits: 96 },
        Layout { name: "blob6", ddl: "BLOB NOT NULL", value: blob6, entropy_bits: 48 },
        Layout { name: "blob8", ddl: "BLOB NOT NULL", value: blob8, entropy_bits: 64 },
        Layout { name: "blob10", ddl: "BLOB NOT NULL", value: blob10, entropy_bits: 80 },
        Layout { name: "blob12", ddl: "BLOB NOT NULL", value: blob12, entropy_bits: 96 },
        Layout { name: "blob16marker", ddl: "BLOB NOT NULL", value: blob16marker, entropy_bits: 64 },
        Layout { name: "blob16selfdesc", ddl: "BLOB NOT NULL", value: blob16selfdesc, entropy_bits: 64 },
        Layout { name: "int4", ddl: "INTEGER NOT NULL", value: counter4, entropy_bits: 32 },
    ]
}

/// **A pair of layouts** — the identity line's design gives the two columns
/// different shapes, so the harness has to be able to say that.
#[derive(Clone, Copy)]
struct Pair {
    name: &'static str,
    session: Layout,
    visit: Layout,
}

fn pairs() -> Vec<Pair> {
    let find = |name: &str| -> Layout {
        layouts()
            .into_iter()
            .find(|layout| layout.name == name)
            .unwrap_or_else(|| panic!("no layout {name}"))
    };
    vec![
        // today's shape: both columns are sixteen-byte uuids
        Pair { name: "uuid16_uuid16", session: find("today"), visit: find("today") },
        // the carrier discussion: both columns are twelve-byte blobs
        Pair { name: "blob12_blob12", session: find("blob12"), visit: find("blob12") },
        // **the identity line's target shape**: the visitor is a twelve-byte
        // blob, the visit is a per-site counter
        Pair { name: "blob12_int4", session: find("blob12"), visit: find("int4") },
        // the two halves of the target, isolated
        Pair { name: "blob12_uuid16", session: find("blob12"), visit: find("today") },
        Pair { name: "uuid16_int4", session: find("today"), visit: find("int4") },
    ]
}

/// The traffic: the real schema's columns, filled with plausible traffic.
///
/// The shape of the traffic matters for the numbers, so it is modelled
/// rather than invented:
///
/// - a **visit** (the product's `visit_id`) is four events on average;
/// - a **visitor** (the product's `session_id`) has a couple of visits, and
///   one visitor in five is a heavy one (a fifth of the visits);
/// - the visitor's browser, os and device do not change between their visits.
fn traffic(
    pair: &Pair,
    rows: i64,
    events_per_visit: i64,
    visitors: i64,
    instance_hex: &str,
    interleave: i64,
    days: i64,
) -> String {
    // **Which visit a row belongs to.**
    //
    // A visit is four events. Where those four events land in the file is
    // the thing that decides the compression: if they are neighbours, the
    // engine's run length encoding eats the identity columns, and the
    // marker layout buys nothing. If they are scattered — a busy site, where
    // other visitors' events arrive in between — there are no runs, and the
    // marker layout's bit packing is what saves the disk.
    //
    // `interleave` is how many visits are in flight at once: 1 is a quiet
    // site (the four events of a visit are neighbours), 4 is a busy one.
    let block = events_per_visit * interleave;
    let visit_expr = format!(
        "((i // {block}) * {interleave} + (i % {block}) % {interleave})"
    );
    // One visitor in five gets a fifth of the visits; the rest are spread out.
    // The same expression twice: once over the range, once over the subquery.
    let visitors_expr = format!(
        "CASE WHEN ({visit_expr}) % 5 = 0 THEN 0 ELSE (({visit_expr}) * 2654435761) % {{VISITORS}} END"
    );
    let visitors_expr_outer = visitors_expr.replace("(i //", "(s.i //");
    // The two hashes are computed once per row, in the subquery.
    // The visit's number, in the outer query (a counter's value).
    let visit_expr_outer = visit_expr.replace("(i //", "(s.i //");
    let session_value = (pair.session.value)("s.hs", &visit_expr_outer);
    let visit_value = (pair.visit.value)("s.hv", &visit_expr_outer);

    format!(
        "INSERT INTO website_event
         (event_id, website_id, visit_id, session_id, ip, ip_prefix, user_agent, user_id,
          created_at, url_path, url_query, page_title, referrer_domain, referrer_path,
          referrer_query, referrer_name, hostname, channel, browser, browser_version, os,
          os_version, device, screen, language, country, region, city, latitude, longitude,
          utm_source, utm_medium, utm_campaign, utm_content, utm_term, gclid, fbclid,
          msclkid, ttclid, li_fat_id, twclid, lcp, inp, cls, fcp, ttfb, event_type,
          event_name, tag, data)
         SELECT {event_id_value} AS event_id,
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
        event_id_value = uuid_text(&format!("'{instance_hex}'||lpad(to_hex(s.i), 12, '0')")),
        visit_expr = visit_expr,
        SECONDS_PER_ROW = (days * 86_400) as f64 / rows as f64,
        visitors_expr = visitors_expr,
        visitors_expr_outer = visitors_expr_outer
    )
}

/// The report's queries, verbatim from `query.rs` (`summary`,
/// `summary_sessions`, `breakdown`, `event_log`), plus the two that split
/// the cost apart: a bare scan, and a scan with only the distinct count.
fn queries(days: i64, table: &str) -> Vec<(&'static str, String)> {
    let window = format!(
        "created_at >= TIMESTAMP '2026-01-01' AND created_at < TIMESTAMP '2026-01-01' + INTERVAL {days} DAY"
    );
    let month = "created_at >= TIMESTAMP '2026-01-01' AND created_at < TIMESTAMP '2026-01-01' + INTERVAL 30 DAY";
    let t = table;
    vec![
        ("scan", format!("SELECT count(*) FROM {t}")),
        (
            // The scan alone: the biggest value, no hashing, no grouping.
            // What it costs beyond the metadata is the **unpack** — the
            // engine widening the packed 64 bits back to 128.
            "scan_max",
            format!("SELECT max(session_id) FROM {t}"),
        ),
        (
            // The scan plus the hash the aggregate would pay anyway.
            "scan_hash",
            format!("SELECT sum(hash(session_id)) FROM {t}"),
        ),
        (
            "distinct_only",
            format!("SELECT count(DISTINCT session_id) FROM {t} WHERE {window}"),
        ),
        (
            "summary_split",
            format!(
                "SELECT COUNT(DISTINCT CASE WHEN {PAGEVIEWS} THEN session_id END) FROM {t} WHERE {window}"
            ),
        ),
        (
            "summary_split_2",
            format!(
                "SELECT COUNT(DISTINCT CASE WHEN {PAGEVIEWS} THEN visit_id END) FROM {t} WHERE {window}"
            ),
        ),
        (
            "summary_30d",
            format!(
                "SELECT COUNT(*) FILTER (WHERE {PAGEVIEWS}),
                        COUNT(DISTINCT CASE WHEN {PAGEVIEWS} THEN session_id END),
                        COUNT(DISTINCT CASE WHEN {PAGEVIEWS} THEN visit_id END)
                 FROM {t} WHERE {month}"
            ),
        ),
        (
            "summary",
            format!(
                "SELECT COUNT(*) FILTER (WHERE {PAGEVIEWS}),
                        COUNT(DISTINCT CASE WHEN {PAGEVIEWS} THEN session_id END),
                        COUNT(DISTINCT CASE WHEN {PAGEVIEWS} THEN visit_id END)
                 FROM {t} WHERE {window}"
            ),
        ),
        (
            "bounces",
            format!(
                "SELECT COUNT(*) FILTER (WHERE n = 1 AND custom = 0), COALESCE(AVG(d), 0)
                 FROM (
                     SELECT visit_id,
                            COUNT(*) FILTER (WHERE {PAGEVIEWS}) AS n,
                            MAX(CASE WHEN event_type = 'event' THEN 1 ELSE 0 END) AS custom,
                            CASE WHEN MAX(created_at) FILTER (WHERE {PAGEVIEWS}) IS NULL THEN NULL
                                 ELSE GREATEST(
                                     epoch_us(MAX(created_at) FILTER (WHERE {PAGEVIEWS}))
                                     - CASE WHEN MIN(session_start) IS NULL
                                            THEN epoch_us(MIN(created_at) FILTER (WHERE {PAGEVIEWS}))
                                            ELSE epoch_us(MIN(session_start)) END, 0) / 1000000.0
                            END AS d
                     FROM {t} WHERE {window}
                     GROUP BY visit_id
                 )"
            ),
        ),
        (
            "breakdown",
            format!(
                "SELECT COALESCE(browser, 'unknown') AS name, COUNT(DISTINCT session_id) AS value
                 FROM {t} WHERE {window} AND {PAGEVIEWS}
                 GROUP BY name ORDER BY value DESC, name ASC LIMIT 50"
            ),
        ),
        (
            "event_log",
            format!(
                "SELECT event_id AS event_id, website_id, visit_id, session_id, created_at, event_name, url_path, hostname, channel, data,
                        referrer_domain, referrer_path, referrer_query, country, city, browser, os,
                        utm_source, utm_medium, utm_campaign, lcp, inp, cls, device
                 FROM {t} WHERE {window}
                 ORDER BY created_at DESC, event_id DESC LIMIT 50 OFFSET 0"
            ),
        ),
    ]
}

/// A time, or the word for the query that did not fit.
fn show(ms: f64) -> String {
    if ms.is_finite() {
        format!("{ms:.1}")
    } else {
        "oom".to_string()
    }
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

/// How the engine stores the two identity columns.
fn storage(conn: &Connection) -> anyhow::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT column_name, compression FROM pragma_storage_info('website_event')
         WHERE column_name IN ('session_id', 'visit_id') ORDER BY 1, 2",
    )?;
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
fn timed(
    conn: &Connection,
    sql: &str,
    repeats: usize,
) -> anyhow::Result<(f64, i64, i64)> {
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

/// One layout, measured.
struct Row {
    layout: String,
    entropy_bits: usize,
    duckdb_b_per_row: f64,
    parquet_b_per_row: f64,
    without_b_per_row: f64,
    column_costs: Vec<(String, f64)>,
    parquet_write_ms: f64,
    parquet_timings: Vec<(String, f64)>,
    write_ms: f64,
    visitors: i64,
    visits: i64,
    how: String,
    timings: Vec<(String, f64)>,
    mem_bytes: i64,
    spill_bytes: i64,
}

fn measure(
    pair: &Pair,
    rows: i64,
    events_per_visit: i64,
    visitors: i64,
    memory_limit: &str,
    dir: &Path,
    parquet: bool,
    repeats: usize,
    interleave: i64,
    days: i64,
    threads: i64,
) -> anyhow::Result<Row> {
    let path = dir.join(format!("{}.duckdb", pair.name));
    let _ = fs::remove_file(&path);
    let _ = fs::remove_file(dir.join(format!("{}.duckdb.wal", pair.name)));

    let instance = 0x0f1e2d3c4b5a69788796u128 & event_id::INSTANCE_MAX;
    let instance_hex = event_id::instance_hex(instance);

    let conn = Connection::open(&path)?;
    conn.execute_batch(&format!("SET temp_directory='{}'", dir.display()))?;
    if threads > 0 {
        conn.execute_batch(&format!("SET threads={threads}"))?;
    }
    // The load is given room: the service writes row by row through an
    // appender, so it never holds a table's worth of rows, while this
    // harness's one statement does. The queries below run under the
    // product's own ceiling — that is the one being measured.
    conn.execute_batch("SET memory_limit='8GB'")?;
    let columns = schema::columns_ddl_with(
        schema::EVENT,
        &Duck,
        &[
            ("session_id", &format!("session_id {}", pair.session.ddl)),
            ("visit_id", &format!("visit_id {}", pair.visit.ddl)),
        ],
    );
    conn.execute_batch(&format!("CREATE TABLE website_event ({columns});"))?;
    let sql = traffic(&pair, rows, events_per_visit, visitors, &instance_hex, interleave, days)
        .replace("{ROWS}", &rows.to_string())
        .replace("{VISITORS}", &visitors.to_string());

    let started = Instant::now();
    conn.execute_batch(&sql)?;
    let write_ms = started.elapsed().as_secs_f64() * 1000.0;
    conn.execute_batch("CHECKPOINT;")?;
    // From here on the ceiling is the product's.
    conn.execute_batch(&format!("SET memory_limit='{memory_limit}'"))?;

    let duckdb_b_per_row = fs::metadata(&path)?.len() as f64 / rows as f64;
    let how = storage(&conn)?.join(" ");

    // The same table without the two identity columns, in a **duckdb** file
    // of its own: the difference is what the identity costs (the way #145
    // measured it — and it has to be the same engine, or the difference is
    // the difference between two formats).
    let without = dir.join(format!("{}-without.duckdb", pair.name));
    let _ = fs::remove_file(&without);
    let _ = conn.execute_batch("SET memory_limit='8GB'");
    // DuckDB 2.0 has no `COPY ... (FORMAT DUCKDB)`, so the comparison copy is
    // a database file of its own, attached.
    let without_b_per_row = (|| -> anyhow::Result<f64> {
        conn.execute_batch(&format!("ATTACH '{}' AS without_db", without.display()))?;
        conn.execute_batch(
            "CREATE TABLE without_db.website_event AS \
             SELECT * EXCLUDE (session_id, visit_id) FROM website_event",
        )?;
        conn.execute_batch("DETACH without_db")?;
        Ok(fs::metadata(&without)?.len() as f64 / rows as f64)
    })()
    .unwrap_or(f64::NAN);
    let _ = fs::remove_file(&without);
    let _ = conn.execute_batch(&format!("SET memory_limit='{memory_limit}'"));

    // **The two identity columns, one at a time.** They repeat heavily in the
    // event table, so their cost is not a per-row cost: it is the width of
    // each distinct value plus the per-row index. Taking each column away
    // and comparing the files says what each one costs.
    let mut column_costs: Vec<(String, f64)> = Vec::new();
    for column in ["session_id", "visit_id"] {
        let file = dir.join(format!("{}-without-{column}.duckdb", pair.name));
        let _ = fs::remove_file(&file);
        let _ = conn.execute_batch("SET memory_limit='8GB'");
        let outcome = (|| -> anyhow::Result<f64> {
            conn.execute_batch(&format!("ATTACH '{}' AS one_db", file.display()))?;
            conn.execute_batch(&format!(
                "CREATE TABLE one_db.website_event AS \
                 SELECT * EXCLUDE ({column}) FROM website_event"
            ))?;
            conn.execute_batch("DETACH one_db")?;
            Ok(fs::metadata(&file)?.len() as f64 / rows as f64)
        })();
        let _ = fs::remove_file(&file);
        let _ = conn.execute_batch(&format!("SET memory_limit='{memory_limit}'"));
        column_costs.push((column.to_string(), outcome.unwrap_or(f64::NAN)));
    }

    // The cold tier: the backup's copy, and the queries the archive answers.
    let mut parquet_write_ms = f64::NAN;
    let mut parquet_timings: Vec<(String, f64)> = Vec::new();
    let parquet_b_per_row = if parquet {
        let out = dir.join(format!("{}.parquet", pair.name));
        let _ = fs::remove_file(&out);
        // The export is a cold-tier copy, not the thing being measured: it
        // gets the room, and a failure is reported, not fatal.
        let _ = conn.execute_batch("SET memory_limit='8GB'");
        let started = Instant::now();
        let outcome = conn.execute_batch(&format!(
            "COPY (SELECT * FROM website_event) TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD);",
            out.display()
        ));
        parquet_write_ms = started.elapsed().as_secs_f64() * 1000.0;
        let _ = conn.execute_batch(&format!("SET memory_limit='{memory_limit}'"));
        match outcome {
            Ok(()) => {
                let bytes = fs::metadata(&out)?.len() as f64 / rows as f64;
                // The archive's own read path: a view over the parquet.
                let _ = conn.execute_batch(&format!(
                    "CREATE OR REPLACE VIEW pq AS SELECT * FROM read_parquet('{}');",
                    out.display()
                ));
                for (name, sql) in queries(days, "pq") {
                    if matches!(name, "scan_max" | "scan_hash" | "summary_split_2" | "bounces" | "breakdown" | "event_log") {
                        continue;
                    }
                    let (ms, _, _) = timed(&conn, &sql, repeats)?;
                    parquet_timings.push((name.to_string(), ms));
                }
                let _ = fs::remove_file(&out);
                bytes
            }
            Err(_) => f64::NAN,
        }
    } else {
        f64::NAN
    };

    // The counts — the invariant every layout has to keep. One at a time:
    // two distinct aggregates in one statement are the expensive shape (see
    // the bisect), and they are the thing that runs out of memory.
    let count_of = |column: &str| -> i64 {
        conn.query_row(
            &format!("SELECT count(DISTINCT {column}) FROM website_event"),
            [],
            |row| row.get(0),
        )
        .unwrap_or(-1)
    };
    let visitors_seen = count_of("session_id");
    let visits_seen = count_of("visit_id");

    let mut timings = Vec::new();
    let mut mem_bytes = 0i64;
    let mut spill_bytes = 0i64;
    for (name, sql) in queries(days, "website_event") {
        let (ms, mem, spill) = timed(&conn, &sql, repeats)?;
        timings.push((name.to_string(), ms));
        mem_bytes = mem_bytes.max(mem);
        spill_bytes = spill_bytes.max(spill);
    }
    // The same numbers asked for as two statements instead of one.
    let halves: Vec<&str> = vec!["summary_split", "summary_split_2"];
    let mut split = 0.0;
    let mut split_ok = true;
    for half in halves {
        let sql = queries(days, "website_event")
            .into_iter()
            .find(|(n, _)| *n == half)
            .map(|(_, s)| s)
            .unwrap_or_default();
        let (ms, _, _) = timed(&conn, &sql, repeats)?;
        if !ms.is_finite() {
            split_ok = false;
        }
        split += ms;
    }
    timings.push(("summary_split".to_string(), if split_ok { split } else { f64::INFINITY }));

    drop(conn);
    let _ = fs::remove_file(&path);

    Ok(Row {
        layout: pair.name.to_string(),
        entropy_bits: pair.session.entropy_bits + pair.visit.entropy_bits,
        duckdb_b_per_row,
        parquet_b_per_row,
        without_b_per_row,
        column_costs,
        parquet_write_ms,
        parquet_timings,
        write_ms,
        visitors: visitors_seen,
        visits: visits_seen,
        how,
        timings,
        mem_bytes,
        spill_bytes,
    })
}

/// The engine's handholds: can the bit packing's cost be taken away?
fn knobs(
    dir: &Path,
    rows: i64,
    visitors: i64,
    memory_limit: &str,
    interleave: i64,
    days: i64,
    events_per_visit: i64,
) -> anyhow::Result<()> {
    let layout = layouts()
        .into_iter()
        .find(|l| l.name == "selfdesc8")
        .expect("selfdesc8");
    let instance = 0x0f1e2d3c4b5a69788796u128 & event_id::INSTANCE_MAX;
    let instance_hex = event_id::instance_hex(instance);

    println!("== the engine's handholds (selfdesc8, {rows} rows, {memory_limit}) ==");
    println!(
        "  {:<34} {:>10} {:>12} {:>12}",
        "the setting", "B/row", "summary ms", "bounces ms"
    );

    let settings: Vec<(&str, Vec<&str>)> = vec![
        ("the default", vec![]),
        ("force_compression=uncompressed", vec!["SET force_compression='uncompressed'"]),
        ("force_compression=bitpacking", vec!["SET force_compression='bitpacking'"]),
        ("disabled_compression_methods=bitpacking", vec!["SET disabled_compression_methods='bitpacking'"]),
        ("threads=1", vec!["SET threads=1"]),
        ("threads=4", vec!["SET threads=4"]),
        ("threads=16", vec!["SET threads=16"]),
        ("operator_memory_limit=64MB", vec!["SET operator_memory_limit='64MB'"]),
    ];

    for (label, sets) in settings {
        let path = dir.join("knob.duckdb");
        let _ = fs::remove_file(&path);
        let conn = Connection::open(&path)?;
        conn.execute_batch("SET memory_limit='8GB'")?;
        for set in &sets {
            conn.execute_batch(set)?;
        }
        let columns = schema::columns_ddl_with(
            schema::EVENT,
            &Duck,
            &[
                ("session_id", &format!("session_id {}", layout.ddl)),
                ("visit_id", &format!("visit_id {}", layout.ddl)),
            ],
        );
        conn.execute_batch(&format!("CREATE TABLE website_event ({columns});"))?;
        let pair = Pair { name: "knobs", session: layout, visit: layout };
        let sql = traffic(&pair, rows, events_per_visit, visitors, &instance_hex, interleave, days)
            .replace("{ROWS}", &rows.to_string())
            .replace("{VISITORS}", &visitors.to_string());
        conn.execute_batch(&sql)?;
        conn.execute_batch("CHECKPOINT;")?;
        // The ceiling the product gives, for the queries.
        conn.execute_batch(&format!("SET memory_limit='{memory_limit}'"))?;
        let bytes = fs::metadata(&path)?.len() as f64 / rows as f64;

        let window = "created_at >= TIMESTAMP '2026-01-01' AND created_at < TIMESTAMP '2030-01-01'";
        let summary = format!(
            "SELECT COUNT(*) FILTER (WHERE {PAGEVIEWS}),
                    COUNT(DISTINCT CASE WHEN {PAGEVIEWS} THEN session_id END),
                    COUNT(DISTINCT CASE WHEN {PAGEVIEWS} THEN visit_id END)
             FROM website_event WHERE {window}"
        );
        let bounces = format!(
            "SELECT COUNT(*) FILTER (WHERE n = 1 AND custom = 0), COALESCE(AVG(d), 0)
             FROM (SELECT visit_id, COUNT(*) FILTER (WHERE {PAGEVIEWS}) AS n,
                          MAX(CASE WHEN event_type = 'event' THEN 1 ELSE 0 END) AS custom,
                          CASE WHEN MAX(created_at) FILTER (WHERE {PAGEVIEWS}) IS NULL THEN NULL
                               ELSE GREATEST(epoch_us(MAX(created_at) FILTER (WHERE {PAGEVIEWS}))
                                   - CASE WHEN MIN(session_start) IS NULL
                                          THEN epoch_us(MIN(created_at) FILTER (WHERE {PAGEVIEWS}))
                                          ELSE epoch_us(MIN(session_start)) END, 0) / 1000000.0 END AS d
                   FROM website_event WHERE {window} GROUP BY visit_id)"
        );
        let (summary_ms, _, _) = timed(&conn, &summary, 3)?;
        let (bounces_ms, _, _) = timed(&conn, &bounces, 3)?;
        let how = storage(&conn)?.join(" ");
        println!(
            "  {label:<34} {bytes:>10.2} {summary_ms:>12.1} {bounces_ms:>12.1}   {how}",
        );
        drop(conn);
        let _ = fs::remove_file(&path);
    }
    Ok(())
}

/// Which part of the summary statement costs what.
fn debug_bisect(
    dir: &Path,
    rows: i64,
    visitors: i64,
    memory_limit: &str,
    interleave: i64,
    days: i64,
    events_per_visit: i64,
) -> anyhow::Result<()> {
    let layout = layouts().into_iter().find(|l| l.name == "today").expect("today");
    let pair = Pair { name: "today", session: layout, visit: layout };
    let instance = 0x0f1e2d3c4b5a69788796u128 & event_id::INSTANCE_MAX;
    let instance_hex = event_id::instance_hex(instance);
    let path = dir.join("debug.duckdb");
    let _ = fs::remove_file(&path);
    let conn = Connection::open(&path)?;
    conn.execute_batch("SET memory_limit='8GB'")?;
    let columns = schema::columns_ddl_with(
        schema::EVENT,
        &Duck,
        &[
            ("session_id", &format!("session_id {}", layout.ddl)),
            ("visit_id", &format!("visit_id {}", layout.ddl)),
        ],
    );
    conn.execute_batch(&format!("CREATE TABLE website_event ({columns});"))?;
    let pair = Pair { name: "debug", session: layout, visit: layout };
    let sql = traffic(&pair, rows, events_per_visit, visitors, &instance_hex, interleave, days)
        .replace("{ROWS}", &rows.to_string())
        .replace("{VISITORS}", &visitors.to_string());
    conn.execute_batch(&sql)?;
    conn.execute_batch("CHECKPOINT;")?;
    conn.execute_batch(&format!("SET memory_limit='{memory_limit}'"))?;

    let window = format!(
        "created_at >= TIMESTAMP '2026-01-01' AND created_at < TIMESTAMP '2026-01-01' + INTERVAL {days} DAY"
    );
    let cases: Vec<(&str, String)> = vec![
        ("count(*)", "SELECT count(*) FROM website_event".to_string()),
        ("count(*) FILTER", format!("SELECT count(*) FILTER (WHERE {PAGEVIEWS}) FROM website_event WHERE {window}")),
        ("distinct session", format!("SELECT count(DISTINCT session_id) FROM website_event WHERE {window}")),
        ("distinct visit", format!("SELECT count(DISTINCT visit_id) FROM website_event WHERE {window}")),
        ("distinct both", format!("SELECT count(DISTINCT session_id), count(DISTINCT visit_id) FROM website_event WHERE {window}")),
        ("summary (the product's)", format!(
            "SELECT COUNT(*) FILTER (WHERE {PAGEVIEWS}),
                    COUNT(DISTINCT CASE WHEN {PAGEVIEWS} THEN session_id END),
                    COUNT(DISTINCT CASE WHEN {PAGEVIEWS} THEN visit_id END)
             FROM website_event WHERE {window}")),
        ("summary without the CASE", format!(
            "SELECT COUNT(*) FILTER (WHERE {PAGEVIEWS}),
                    COUNT(DISTINCT session_id), COUNT(DISTINCT visit_id)
             FROM website_event WHERE {window}")),
        ("summary without the FILTER", format!(
            "SELECT COUNT(*),
                    COUNT(DISTINCT CASE WHEN {PAGEVIEWS} THEN session_id END),
                    COUNT(DISTINCT CASE WHEN {PAGEVIEWS} THEN visit_id END)
             FROM website_event WHERE {window}")),
        ("the case alone, session", format!(
            "SELECT COUNT(DISTINCT CASE WHEN {PAGEVIEWS} THEN session_id END) FROM website_event WHERE {window}")),
        ("the case alone, visit", format!(
            "SELECT COUNT(DISTINCT CASE WHEN {PAGEVIEWS} THEN visit_id END) FROM website_event WHERE {window}")),
        ("group by visit", format!("SELECT count(*) FROM (SELECT visit_id FROM website_event WHERE {window} GROUP BY visit_id)")),
        ("group by session", format!("SELECT count(*) FROM (SELECT session_id FROM website_event WHERE {window} GROUP BY session_id)")),
        ("the same column twice", format!("SELECT count(DISTINCT session_id), count(DISTINCT session_id) FROM website_event WHERE {window}")),
        ("session + a constant", format!("SELECT count(DISTINCT session_id), count(DISTINCT 'x') FROM website_event WHERE {window}")),
        ("session + visit, no where", "SELECT count(DISTINCT session_id), count(DISTINCT visit_id) FROM website_event".to_string()),
    ];

    // The same data in a narrow table: only the columns the statement reads.
    conn.execute_batch("DROP TABLE IF EXISTS narrow;")?;
    conn.execute_batch("CREATE TABLE narrow AS SELECT session_id, visit_id, event_type, created_at FROM website_event;")?;
    conn.execute_batch("CHECKPOINT;")?;
    println!("  {:<32} {:>10} {:>12}", "the statement", "ms", "mem MiB");
    for (label, sql) in &cases {
        let (ms, mem, _) = timed(&conn, sql, 3)?;
        println!("  {label:<32} {ms:>10.1} {:>12.1}", mem as f64 / 1_048_576.0);
    }
    println!();
    println!("  the same statements on a narrow table (four columns only)");
    for (label, sql) in &cases {
        let narrow = sql.replace("website_event", "narrow");
        let (ms, mem, _) = timed(&conn, &narrow, 3)?;
        println!("  {label:<32} {ms:>10.1} {:>12.1}", mem as f64 / 1_048_576.0);
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut rows: i64 = 20_000_000;
    let mut visitors: i64 = 1_000_000;
    let mut only: Option<Vec<String>> = None;
    let mut memory_limit = PRODUCT_MEMORY_LIMIT.to_string();
    let mut parquet = true;
    let mut repeats = 3usize;
    let mut want_knobs = false;
    let mut want_debug = false;
    let mut interleave = 4i64;
    let mut days = 90i64;
    let mut baseline = false;
    let mut threads = 0i64;
    let mut events_per_visit = 4i64;

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
            "--layouts" => {
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
            "--knobs" => {
                want_knobs = true;
                i += 1;
            }
            "--debug" => {
                want_debug = true;
                i += 1;
            }
            "--baseline" => {
                baseline = true;
                i += 1;
            }
            "--days" => {
                days = args[i + 1].parse()?;
                i += 2;
            }
            "--threads" => {
                threads = args[i + 1].parse()?;
                i += 2;
            }
            "--events-per-visit" => {
                events_per_visit = args[i + 1].parse()?;
                i += 2;
            }
            "--interleave" => {
                interleave = args[i + 1].parse()?;
                i += 2;
            }
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }

    let dir = std::env::temp_dir().join("sumimi-identity-scale");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir)?;

    if want_debug {
        debug_bisect(&dir, rows, visitors, &memory_limit, interleave, days, events_per_visit)?;
        return Ok(());
    }

    if want_knobs {
        knobs(&dir, rows, visitors, &memory_limit, interleave, days, events_per_visit)?;
        return Ok(());
    }

    println!(
        "# {rows} rows, {visitors} visitors, memory_limit={memory_limit}, \
         interleave={interleave}, {days} days"
    );
    let mut results = Vec::new();
    if baseline {
        let layout = Layout {
            name: "baseline",
            ddl: "UUID NOT NULL",
            value: constant,
            entropy_bits: 0,
        };
        let pair = Pair { name: "baseline", session: layout, visit: layout };
        results.push(measure(
            &pair,
            rows,
            events_per_visit,
            visitors,
            &memory_limit,
            &dir,
            parquet,
            repeats,
            interleave,
            days,
            threads,
        )?);
    }
    // A name is either a pair (the two columns get different shapes) or a
    // single layout (both columns get it).
    let wanted: Vec<Pair> = match &only {
        Some(names) => names
            .iter()
            .filter_map(|name| {
                pairs()
                    .into_iter()
                    .find(|pair| pair.name == *name)
                    .or_else(|| {
                        layouts()
                            .into_iter()
                            .find(|layout| layout.name == *name)
                            .map(|layout| Pair { name: layout.name, session: layout, visit: layout })
                    })
            })
            .collect(),
        None => pairs(),
    };
    for pair in wanted {
        let row = measure(
            &pair,
            rows,
            events_per_visit,
            visitors,
            &memory_limit,
            &dir,
            parquet,
            repeats,
            interleave,
            days,
            threads,
        )?;
        results.push(row);
    }

    // The report.
    let names: Vec<String> = results
        .first()
        .map(|r| r.timings.iter().map(|(n, _)| n.clone()).collect())
        .unwrap_or_default();
    println!();
    println!(
        "  {:<12} {:>6} {:>9} {:>9} {:>9} {:>9} {:>10}",
        "the layout", "entropy", "duckdb B", "parquet B", "no-ids B", "ids cost", "write s"
    );
    for r in &results {
        println!(
            "  {:<12} {:>6} {:>9.2} {:>9.2} {:>9.2} {:>9.2} {:>10.1}",
            r.layout,
            r.entropy_bits,
            r.duckdb_b_per_row,
            r.parquet_b_per_row,
            r.without_b_per_row,
            r.duckdb_b_per_row - r.without_b_per_row,
            r.write_ms / 1000.0
        );
    }

    println!();
    println!(
        "  {:<12} {:>9} {:>9} {:>9} {:>10} {:>12} {:>12} {:>10}",
        "the layout", "scan", "scanhash", "distinct", "summary", "summary×2", "bounces", "summary30d"
    );
    for r in &results {
        let get = |n: &str| {
            r.timings
                .iter()
                .find(|(k, _)| k == n)
                .map(|(_, v)| *v)
                .unwrap_or(f64::NAN)
        };
        println!(
            "  {:<12} {:>9} {:>9} {:>9} {:>10} {:>12} {:>12} {:>10}",
            r.layout,
            show(get("scan_max")),
            show(get("scan_hash")),
            show(get("distinct_only")),
            show(get("summary")),
            show(get("summary_split")),
            show(get("bounces")),
            show(get("summary_30d"))
        );
    }

    println!();
    println!(
        "  {:<12} {:>10} {:>10} {:>10} {:>8} {:>8}",
        "the layout", "breakdown", "event log", "mem MiB", "spill MiB", "visitors"
    );
    for r in &results {
        let get = |n: &str| {
            r.timings
                .iter()
                .find(|(k, _)| k == n)
                .map(|(_, v)| *v)
                .unwrap_or(f64::NAN)
        };
        println!(
            "  {:<12} {:>10} {:>10} {:>10.1} {:>8.1} {:>8}",
            r.layout,
            show(get("breakdown")),
            show(get("event_log")),
            r.mem_bytes as f64 / 1_048_576.0,
            r.spill_bytes as f64 / 1_048_576.0,
            r.visitors
        );
    }

    println!();
    println!("== the cold tier: the export, and the archive's read path ==");
    println!(
        "  {:<12} {:>10} {:>10} {:>10} {:>12}",
        "the layout", "export s", "scan ms", "distinct ms", "summary ms"
    );
    for r in &results {
        let get = |n: &str| {
            r.parquet_timings
                .iter()
                .find(|(k, _)| k == n)
                .map(|(_, v)| *v)
                .unwrap_or(f64::NAN)
        };
        println!(
            "  {:<12} {:>10.1} {:>10} {:>10} {:>12}",
            r.layout,
            r.parquet_write_ms / 1000.0,
            show(get("scan_max")),
            show(get("distinct_only")),
            show(get("summary"))
        );
    }

    println!();
    println!("== how the engine stores the identity columns ==");
    for r in &results {
        println!("  {:<12} {}", r.layout, r.how);
    }

    // The invariant: every layout must count the same visitors and visits.
    let base = results.first();
    if let Some(base) = base {
        let mut same = true;
        for r in &results {
            if r.visitors != base.visitors || r.visits != base.visits {
                same = false;
                println!(
                    "  !! {} counts {} visitors / {} visits, {} counts {} / {}",
                    base.layout, base.visitors, base.visits, r.layout, r.visitors, r.visits
                );
            }
        }
        if same {
            println!(
                "\n== the identity columns, one at a time (B/row) ==",
            );
            for row in &results {
                let parts: Vec<String> = row
                    .column_costs
                    .iter()
                    .map(|(column, size)| format!("{column} {size:.2}"))
                    .collect();
                println!("  {:<16} {}", row.layout, parts.join("  ·  "));
            }
            println!(
                "\n(the counts agree across every layout: {} visitors, {} visits)",
                base.visitors, base.visits
            );
        }
    }

    let _ = names;
    let _ = PathBuf::new();
    Ok(())
}
