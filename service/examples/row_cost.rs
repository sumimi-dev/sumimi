//! How many bytes does one page view cost?
//!
//! "How big is an analytics database" depends entirely on how repetitive the traffic
//! is, so this probe measures three levels of realism — the same dataset, generated
//! three ways — and reports the cost per page view.
//!
//! It also measures what the index of a `PRIMARY KEY` would add, because that is
//! the one overhead an append-only design does not need.
//!
//! ```bash
//! cargo run --release --example row_cost -p sumimi-service -- real 100000
//! ```

use std::time::Instant;

use anyhow::anyhow;
use duckdb::Connection;


/// A UUID from a text expression — the same shape the server hands out.
///
/// The ids are 128-bit and stored in **every** row, so how they are written
/// matters more than anything else in this file.
fn uuid(text: &str) -> String {
    format!(
        "CAST(substr(md5({text}), 1, 8) || '-' || substr(md5({text}), 9, 4) || '-' || \
         substr(md5({text}), 13, 4) || '-' || substr(md5({text}), 17, 4) || '-' || \
         substr(md5({text}), 21, 12) AS UUID)"
    )
}

/// The apples-to-apples number.
///
/// The PostgreSQL comparison (issue #23) was measured on a dataset whose ids are
/// `md5(...)` — 32 characters each. This measures **the same dataset** on our side,
/// so the ratio means something.
fn parity() -> anyhow::Result<()> {
    println!("=== the same dataset the PostgreSQL comparison used ===");
    let dir = std::env::temp_dir().join("sumimi-row-cost-parity");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;

    let mut sizes = Vec::new();
    for rows in [100_000i64, 1_000_000] {
        let path = dir.join(format!("parity-{rows}.duckdb"));
        let conn = Connection::open(&path)?;
        // The real schema: the same columns, the same expressions.
        conn.execute_batch(&format!(
            "CREATE TABLE website_event ({ddl});",
            ddl = sumimi_service::db::event_columns_ddl(),
        ))?;
        let row = row_with(vec![
            // The visitor's id is `session_id`; the session's own id is `visit_id`.
            ("session_id", uuid("md5((i % 3000)::text || '-v')")),
            ("visit_id", uuid("md5((1 + (i / 2))::text || '-s')")),
            ("url_path", "'/post/' || (i % 5000)::VARCHAR".to_string()),
            (
                "url_query",
                "'utm_source=twitter&utm_campaign=spring&utm_medium=cpc'".to_string(),
            ),
            (
                "page_title",
                "'A reasonably long article title number ' || (i % 2000)::VARCHAR".to_string(),
            ),
            (
                "referrer_path",
                "'/ref/path/' || (i % 1000)::VARCHAR".to_string(),
            ),
            (
                "referrer_query",
                "'?q=search+' || (i % 500)::VARCHAR".to_string(),
            ),
            ("hostname", "'example.com'".to_string()),
            (
                "utm_source",
                "(ARRAY['twitter','newsletter','google','hn'])[1 + (i % 4)]".to_string(),
            ),
            ("utm_campaign", "'spring'".to_string()),
            ("utm_content", "'cta-' || (i % 5)::VARCHAR".to_string()),
            ("utm_term", "'analytics'".to_string()),
            (
                "gclid",
                "CASE WHEN i % 9 = 0 THEN 'gclid-' || i::VARCHAR ELSE NULL END".to_string(),
            ),
        ]);
        conn.execute_batch(&fill_sql(
            &row,
            rows,
            "website_event",
            &sumimi_service::db::event_columns(),
        )?)?;
        conn.execute_batch("CHECKPOINT")?;
        let size = std::fs::metadata(&path)?.len();
        println!("  {rows:>9} views -> {:>8.2} MB", size as f64 / 1e6);
        sizes.push(size);
    }

    let marginal = (sizes[1] as f64 - sizes[0] as f64) / (1_000_000 - 100_000) as f64;
    println!();
    println!("  our marginal cost:      {marginal:.1} B/浏览");
    println!("  PostgreSQL marginal:    4750.0 B/浏览");
    println!("  ratio:                  {:.0}x", 4750.0 / marginal);
    println!();

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// The expressions of one synthetic row, by column name.
type Row = Vec<(&'static str, String)>;

/// The columns that are the same in every measurement.
///
/// The names are the schema's, so a column the schema has and this row does
/// not is an error rather than a silently misplaced value.
fn standard_row() -> Row {
    vec![
        ("event_id", "i".to_string()),
        ("website_id", "'w1'".to_string()),
        ("visit_id", "NULL".to_string()),
        ("session_id", "NULL".to_string()),
        ("session_start", "NULL".to_string()),
        ("ip", "''".to_string()),
        ("ip_prefix", "''".to_string()),
        ("user_agent", "''".to_string()),
        ("user_id", "NULL".to_string()),
        (
            "created_at",
            "now()::TIMESTAMP - INTERVAL (i % 30000000) MINUTE".to_string(),
        ),
        (
            "referrer_domain",
            "(ARRAY['news.ycombinator.com','www.google.com','github.com','x.com'])[1 + (i % 4)]"
                .to_string(),
        ),
        ("referrer_name", "NULL".to_string()),
        ("channel", "NULL".to_string()),
        (
            "browser",
            "(ARRAY['Chrome','Safari','Firefox','Edge'])[1 + (i % 4)]".to_string(),
        ),
        ("browser_version", "'120'".to_string()),
        (
            "os",
            "(ARRAY['Windows','macOS','iOS','Android'])[1 + (i % 4)]".to_string(),
        ),
        ("os_version", "'10'".to_string()),
        (
            "device",
            "(ARRAY['desktop','mobile','tablet'])[1 + (i % 3)]".to_string(),
        ),
        (
            "screen",
            "(ARRAY['1920x1080','390x844','768x1024'])[1 + (i % 3)]".to_string(),
        ),
        (
            "language",
            "(ARRAY['en-US','zh-CN','de-DE'])[1 + (i % 3)]".to_string(),
        ),
        (
            "country",
            "(ARRAY['US','CN','DE','JP'])[1 + (i % 4)]".to_string(),
        ),
        ("region", "NULL".to_string()),
        ("city", "NULL".to_string()),
        ("latitude", "NULL".to_string()),
        ("longitude", "NULL".to_string()),
        ("utm_medium", "'cpc'".to_string()),
        (
            "utm_campaign",
            "'campaign-' || (i % 200)::VARCHAR".to_string(),
        ),
        ("utm_content", "'cta-' || (i % 20)::VARCHAR".to_string()),
        ("utm_term", "'term-' || (i % 50)::VARCHAR".to_string()),
        (
            "fbclid",
            "CASE WHEN i % 11 = 0 THEN 'fbclid-' || i::VARCHAR ELSE NULL END".to_string(),
        ),
        ("msclkid", "NULL".to_string()),
        ("ttclid", "NULL".to_string()),
        ("li_fat_id", "NULL".to_string()),
        ("twclid", "NULL".to_string()),
        ("lcp", "600.0 + (i % 1800)".to_string()),
        ("inp", "30.0 + (i % 260)".to_string()),
        ("cls", "(i % 100) / 1000.0".to_string()),
        ("fcp", "400.0 + (i % 900)".to_string()),
        ("ttfb", "60.0 + (i % 220)".to_string()),
        ("event_type", "'pageview'".to_string()),
        ("event_name", "NULL".to_string()),
        ("tag", "NULL".to_string()),
        ("data", "NULL".to_string()),
    ]
}

/// The row of a measurement: the standard one with a few columns changed.
fn row_with(overrides: Vec<(&'static str, String)>) -> Row {
    let mut row = standard_row();
    for (column, expression) in overrides {
        match row.iter_mut().find(|(name, _)| *name == column) {
            Some(entry) => entry.1 = expression,
            None => row.push((column, expression)),
        }
    }
    row
}

/// The SQL that fills the `website_event` table with one row per generated id.
///
/// The columns are the schema's, in the schema's order, and every value is
/// looked up **by column name** — so a column the schema has and this row
/// does not is an error, not a silently misplaced value.
///
/// The table and its column names are given, because the **old** shape (the
/// one a v11 file holds) called the table `event` and five of its columns
/// something else.
fn fill_sql(row: &Row, rows: i64, table: &str, columns: &[String]) -> anyhow::Result<String> {
    let mut values = Vec::with_capacity(columns.len());
    for column in columns {
        let expression = row
            .iter()
            .find(|(name, _)| name == column)
            .map(|(_, expression)| expression.as_str())
            .ok_or_else(|| anyhow!("no expression for the column {column}"))?;
        values.push(expression);
    }
    Ok(format!(
        "INSERT INTO {table} ({}) SELECT {} FROM generate_series(1, {rows}) t(i);",
        columns.join(", "),
        values.join(", "),
    ))
}

/// The columns of the **old** shape — the ones a v11 file holds.
///
/// Five of them were renamed in v12; the rest kept their names.
fn old_columns() -> Vec<String> {
    sumimi_service::db::event_columns()
        .into_iter()
        .map(|name| match name.as_str() {
            "event_id" => "id".to_string(),
            "url_path" => "url".to_string(),
            "page_title" => "title".to_string(),
            "referrer_domain" => "referrer".to_string(),
            "li_fat_id" => "lifatid".to_string(),
            _ => name,
        })
        .collect()
}

/// How the ids are written.
struct Ids {
    /// The expression for the session's id — the column is `visit_id`.
    session: String,
    /// The expression for the visitor's id — the column is `session_id`.
    visitor: String,
}

/// The three levels of realism.
#[derive(Clone)]
struct Level {
    name: &'static str,
    /// The SQL expression that builds the URL.
    url: String,
    /// The SQL expression that builds the title.
    title: String,
    /// The SQL expression that builds the referrer path.
    referrer: String,
    /// The SQL expression that builds the query string.
    query: String,
    /// The SQL expression for `utm_source`.
    utm: String,
    /// The SQL expression for `gclid`.
    click: String,
    /// The SQL expression for the session id.
    session: String,
    /// The SQL expression for the visitor id.
    visitor: String,
    /// The hostname.
    host: &'static str,
}


/// The ids the server really hands out are 128-bit UUIDs — 36 characters each, and
/// they are stored in **every** row. How much does that cost, and what would the
/// DuckDB `UUID` type (16 bytes) cost instead?
fn ids() -> anyhow::Result<()> {
    println!("=== the cost of the session/visitor ids ===");
    let dir = std::env::temp_dir().join("sumimi-row-cost-ids");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let level = levels()[1].clone();
    let rows = 100_000i64;

    // A short id: what the seed and the benchmarks use.
    let short = build_ids(&dir.join("short.duckdb"), &level, rows, "VARCHAR", "short")?;
    // The real thing: a 36-character UUID.
    let uuid_text = build_ids(&dir.join("uuid.duckdb"), &level, rows, "VARCHAR", "uuid")?;
    // The same value, but stored as the 16-byte DuckDB UUID type.
    let uuid_bin = build_ids(&dir.join("uuidbin.duckdb"), &level, rows, "UUID", "uuid")?;

    println!();
    println!("  short ids (the seed):        {:>7.1} B/浏览", short / rows as f64);
    println!("  UUID text (what the server does): {:>7.1} B/浏览", uuid_text / rows as f64);
    println!("  UUID type (16 bytes):        {:>7.1} B/浏览", uuid_bin / rows as f64);
    println!(
        "  -> the id format costs {:+.1} B/浏览; the UUID type saves {:+.1} B/浏览",
        uuid_text / rows as f64 - short / rows as f64,
        uuid_text / rows as f64 - uuid_bin / rows as f64
    );

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// Build the `website_event` table with the id columns in a given type.
fn build_ids(
    path: &std::path::Path,
    level: &Level,
    rows: i64,
    id_type: &str,
    style: &str,
) -> anyhow::Result<f64> {
    let conn = Connection::open(path)?;
    // The same columns, with the ids spelled in another type.
    // The visitor's id is `session_id`; the session's own id is `visit_id`.
    let ddl = sumimi_service::schema::columns_ddl_with(
        sumimi_service::schema::EVENT,
        &sumimi_service::db::Duck,
        &[
            ("visit_id", &format!("visit_id {id_type} NOT NULL")),
            ("session_id", &format!("session_id {id_type} NOT NULL")),
        ],
    );
    conn.execute_batch(&format!("CREATE TABLE website_event ({ddl});"))?;

    // The three forms the ids are measured in: a short text, the 36-character
    // text the server sends, and the 16 bytes it stores.
    let (session, visitor) = match style {
        "uuid" => (
            // The same shape the server produces: 8-4-4-4-12 hex.
            "substr(md5('s' || (1 + (i / 2))::VARCHAR), 1, 8) || '-' ||
             substr(md5('s' || (1 + (i / 2))::VARCHAR), 9, 4) || '-' ||
             substr(md5('s' || (1 + (i / 2))::VARCHAR), 13, 4) || '-' ||
             substr(md5('s' || (1 + (i / 2))::VARCHAR), 17, 4) || '-' ||
             substr(md5('s' || (1 + (i / 2))::VARCHAR), 21, 12)",
            "substr(md5('v' || (i % 30000)::VARCHAR), 1, 8) || '-' ||
             substr(md5('v' || (i % 30000)::VARCHAR), 9, 4) || '-' ||
             substr(md5('v' || (i % 30000)::VARCHAR), 13, 4) || '-' ||
             substr(md5('v' || (i % 30000)::VARCHAR), 17, 4) || '-' ||
             substr(md5('v' || (i % 30000)::VARCHAR), 21, 12)",
        ),
        _ => (
            "'s' || (1 + (i / 2))::VARCHAR",
            "'v' || (i % 30000)::VARCHAR",
        ),
    };
    let (session, visitor) = (session.to_string(), visitor.to_string());

    let row = row_with(vec![
        // The session's id is `visit_id`; the visitor's is `session_id`.
        ("visit_id", session),
        ("session_id", visitor),
        ("url_path", level.url.clone()),
        (
            "url_query",
            "'utm_source=twitter&utm_medium=cpc&utm_campaign=campaign-' || (i % 200)::VARCHAR
             || '&utm_content=cta&utm_term=term'"
                .to_string(),
        ),
        ("page_title", level.title.clone()),
        ("referrer_path", level.referrer.clone()),
        ("referrer_query", level.query.clone()),
        ("hostname", format!("'{}'", level.host)),
        ("utm_source", "'twitter'".to_string()),
        (
            "gclid",
            "CASE WHEN i % 3 = 0 THEN 'gclid-' || i::VARCHAR ELSE NULL END".to_string(),
        ),
    ]);
    conn.execute_batch(&fill_sql(
        &row,
        rows,
        "website_event",
        &sumimi_service::db::event_columns(),
    )?)?;
    conn.execute_batch("CHECKPOINT")?;
    Ok(std::fs::metadata(path)?.len() as f64)
}

/// The three levels.
fn levels() -> Vec<Level> {
    vec![
        Level {
            name: "synthetic",
            url: "'/post/' || (i % 5000)::VARCHAR".to_string(),
            title: "'A reasonably long article title number ' || (i % 2000)::VARCHAR".to_string(),
            referrer: "'/ref/path/' || (i % 1000)::VARCHAR".to_string(),
            query: "'?q=search+' || (i % 500)::VARCHAR".to_string(),
            utm: "CASE WHEN i % 10 = 0 THEN NULL ELSE 'twitter' END".to_string(),
            click: "CASE WHEN i % 9 = 0 THEN 'gclid-' || i::VARCHAR ELSE NULL END".to_string(),
            session: "'s' || (1 + (i / 2))::VARCHAR".to_string(),
            visitor: "'v' || (i % 3000)::VARCHAR".to_string(),
            host: "example.com",
        },
        Level {
            name: "real",
            url: "'/blog/2026/11/a-really-long-article-slug-about-analytics-' || (i % 50000)::VARCHAR || '-tail'".to_string(),
            title: "'Why your analytics database grows faster than your traffic — a long, realistic article title ' || (i % 20000)::VARCHAR".to_string(),
            referrer: "'/item?id=' || (i % 100000)::VARCHAR || '&utm_source=hn'".to_string(),
            query: "'?q=' || (i % 100000)::VARCHAR".to_string(),
            utm: "'twitter'".to_string(),
            click: "CASE WHEN i % 3 = 0 THEN 'gclid-' || i::VARCHAR ELSE NULL END".to_string(),
            session: "'s' || (1 + (i / 2))::VARCHAR".to_string(),
            visitor: "'v' || (i % 30000)::VARCHAR".to_string(),
            host: "blog.example.com",
        },
        Level {
            name: "worst",
            url: "'/p/' || md5(i::text)::VARCHAR || '/' || md5((i+1)::text)::VARCHAR".to_string(),
            title: "'Title ' || md5((i+2)::text)::VARCHAR || ' for page ' || md5((i+3)::text)::VARCHAR".to_string(),
            referrer: "'/item/' || md5((i+4)::text)::VARCHAR".to_string(),
            query: "'?q=' || md5((i+5)::text)::VARCHAR".to_string(),
            utm: "'src-' || (i % 5000)::VARCHAR".to_string(),
            click: "CASE WHEN i % 3 = 0 THEN 'gclid-' || md5((i+6)::text)::VARCHAR ELSE NULL END".to_string(),
            session: "md5((1 + (i / 2))::text)::VARCHAR".to_string(),
            visitor: "md5((i % 30000)::text)::VARCHAR".to_string(),
            host: "blog.example.com",
        },
    ]
}

fn main() -> anyhow::Result<()> {
    let dir = std::env::temp_dir().join("sumimi-row-cost");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;

    println!("{:<12} {:>12} {:>14} {:>14}", "level", "page views", "file", "per view");
    println!("{}", "-".repeat(56));

    for level in levels() {
        let path = dir.join(format!("{}.duckdb", level.name));
        let conn = Connection::open(&path)?;
        // The real schema — the one the server creates.
        sumimi_service::db::init_schema(&conn)?;

        let started = Instant::now();
        fill(
            &conn,
            &level,
            100_000,
            Ids {
                session: uuid(&level.session),
                visitor: uuid(&level.visitor),
            },
        )?;
        let seconds = started.elapsed().as_secs_f64();

        conn.execute_batch("CHECKPOINT")?;
        let size = std::fs::metadata(&path)?.len();
        let per_view = size as f64 / 100_000.0;

        println!(
            "{:<12} {:>12} {:>14} {:>14}",
            level.name,
            "100000",
            fmt_bytes(size),
            format!("{per_view:.1} B")
        );

        // What the index of a PRIMARY KEY would have cost, on this exact schema.
        let before = size;
        conn.execute_batch("CREATE INDEX idx_event_id ON website_event (event_id)")?;
        conn.execute_batch("CHECKPOINT")?;
        let after = std::fs::metadata(&path)?.len();
        println!(
            "{:<12} {:>12} {:>14} {:>14}   <- with a PRIMARY KEY: +{:.1} B/view ({:.1}%)",
            "",
            "",
            "",
            fmt_bytes(after - before),
            (after - before) as f64 / 100_000.0,
            (after - before) as f64 / size as f64 * 100.0
        );
        println!("{:<12} fill took {seconds:.1}s\n", "");
    }

    let _ = std::fs::remove_dir_all(&dir);
    migration()?;
    ids()?;
    parity()?;
    Ok(())
}

/// What does the v11 → v12 migration do to the file?
///
/// **The migration v12 has is the split** (one file per site, since `D-046`):
/// the old file is rewritten, the facts move into a file of their own, and the
/// old file is kept beside the new one. DuckDB frees the blocks of a dropped
/// table instead of truncating the file, so the question is whether the space
/// is actually given back — now in the file that holds the rows.
///
/// The old file is written in the **v11 shape**: the table is `event` and five
/// of its columns have their old names (`id`, `url`, `title`, `referrer`,
/// `lifatid`); the two ids are the sixteen-byte UUIDs they have always been.
fn migration() -> anyhow::Result<()> {
    println!("=== the v11 -> v12 migration ===");
    let dir = std::env::temp_dir().join("sumimi-row-cost-migrate");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;

    let path = dir.join("v11.duckdb");
    {
        let conn = Connection::open(&path)?;
        // The v11 shape: the same columns, with the names they had before v12.
        let ddl = sumimi_service::schema::columns_ddl_with(
            sumimi_service::schema::EVENT,
            &sumimi_service::db::Duck,
            &[
                ("event_id", "id BIGINT NOT NULL"),
                ("url_path", "url VARCHAR"),
                ("page_title", "title VARCHAR"),
                ("referrer_domain", "referrer VARCHAR"),
                ("li_fat_id", "lifatid VARCHAR"),
            ],
        );
        conn.execute_batch(&format!("CREATE TABLE event ({ddl});"))?;
        // The site the rows belong to — the split needs to know which file
        // each row goes into. The `revenue` table is here too, because the
        // split copies it as well.
        conn.execute_batch(
            "CREATE TABLE website (id VARCHAR PRIMARY KEY, name VARCHAR, domain VARCHAR,
                created_at TIMESTAMP, updated_at TIMESTAMP, retention_days INTEGER,
                country_header VARCHAR);
             INSERT INTO website VALUES ('w1', 'the site', 'example.com', now(), now(), 0, NULL);
             CREATE TABLE revenue (id BIGINT NOT NULL, website_id VARCHAR NOT NULL,
                visit_id UUID, event_id BIGINT, event_name VARCHAR, currency VARCHAR,
                revenue DOUBLE NOT NULL, created_at TIMESTAMP NOT NULL);
             CREATE TABLE meta (key VARCHAR PRIMARY KEY, value VARCHAR);
             INSERT INTO meta VALUES ('schema_version', '11');",
        )?;
        fill_text(&conn, &levels()[1], 100_000)?;
        conn.execute_batch("CHECKPOINT")?;
    }

    let before = std::fs::metadata(&path)?.len();
    println!("  a v11 database with 100000 views: {}", fmt_bytes(before));

    // Opening it migrates it: the file is split and the facts move out.
    {
        let db = sumimi_service::pool::Db::open(&path, "256MB")?;
        db.with_read(|conn| {
            conn.execute_batch("CHECKPOINT")?;
            Ok::<_, anyhow::Error>(())
        })?;
    }
    // The rows are in the site's own file now.
    let site_file = sumimi_service::db::site_path(&path, "w1", "duckdb");
    let after = std::fs::metadata(&site_file)?.len();
    println!("  after the migration:              {}", fmt_bytes(after));

    // And what a fresh database of the same data costs.
    let fresh_path = dir.join("fresh.duckdb");
    {
        let conn = Connection::open(&fresh_path)?;
        sumimi_service::db::init_schema(&conn)?;
        let level = levels()[1].clone();
        fill(
            &conn,
            &level,
            100_000,
            Ids {
                session: uuid(&level.session),
                visitor: uuid(&level.visitor),
            },
        )?;
        conn.execute_batch("CHECKPOINT")?;
    }
    let fresh = std::fs::metadata(&fresh_path)?.len();
    println!("  a fresh v12 database:             {}", fmt_bytes(fresh));
    println!(
        "  -> the migrated file is {:+.1} B/view against a fresh database",
        (after as f64 - fresh as f64) / 100_000.0
    );

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// Fill the `event` table with a dataset of the given level.
///
/// The table is the **old** one — `event`, with the names the columns had
/// before v12.
fn fill_text(conn: &Connection, level: &Level, rows: i64) -> anyhow::Result<()> {
    fill_old(
        conn,
        level,
        rows,
        Ids {
            session: uuid(&level.session),
            visitor: uuid(&level.visitor),
        },
    )
}

/// Fill the table with the dataset of a level.
///
/// The ids are given as expressions, because the measurements differ in how
/// the ids are written — as the 16-byte UUID the server writes today, or as
/// text (what [`fill_text`] writes into the old shape).
fn fill(conn: &Connection, level: &Level, rows: i64, ids: Ids) -> anyhow::Result<()> {
    fill_shape(
        conn,
        level,
        rows,
        ids,
        "website_event",
        &sumimi_service::db::event_columns(),
        false,
    )
}

/// Fill the table in the **old** shape — the one a v11 file holds.
fn fill_old(conn: &Connection, level: &Level, rows: i64, ids: Ids) -> anyhow::Result<()> {
    fill_shape(conn, level, rows, ids, "event", &old_columns(), true)
}

/// Fill a table whose columns are spelled the way the target spells them.
///
/// `old` says whether the target is the **old** shape — then the row's keys
/// are spelled the way that shape spells them.
fn fill_shape(
    conn: &Connection,
    level: &Level,
    rows: i64,
    ids: Ids,
    table: &str,
    columns: &[String],
    old: bool,
) -> anyhow::Result<()> {
    // The row is written in the **new** names; they are translated to the
    // target's spelling here, so the two shapes cannot drift apart.
    let row = row_with(vec![
        // The session's id is `visit_id`; the visitor's is `session_id`.
        ("visit_id", ids.session),
        ("session_id", ids.visitor),
        ("url_path", level.url.clone()),
        (
            "url_query",
            format!(
                "'utm_source=' || ({utm}) || '&utm_medium=cpc&utm_campaign=campaign-' || (i % 200)::VARCHAR
                 || '&utm_content=cta&utm_term=term'",
                utm = level.utm,
            ),
        ),
        ("page_title", level.title.clone()),
        ("referrer_path", level.referrer.clone()),
        ("referrer_query", level.query.clone()),
        ("hostname", format!("'{}'", level.host)),
        ("utm_source", level.utm.clone()),
        ("gclid", level.click.clone()),
    ]);
    let row = if old {
        row.into_iter()
            .map(|(name, expression)| {
                let name = match name {
                    "event_id" => "id",
                    "url_path" => "url",
                    "page_title" => "title",
                    "referrer_domain" => "referrer",
                    "li_fat_id" => "lifatid",
                    other => other,
                };
                (name, expression)
            })
            .collect()
    } else {
        row
    };
    conn.execute_batch(&fill_sql(&row, rows, table, columns)?)?;
    Ok(())
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
