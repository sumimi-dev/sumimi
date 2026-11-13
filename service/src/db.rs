//! The schema.
//!
//! **One fact table.** Every page view is one row in `event`, carrying the same fields
//! Umami carries — that is the baseline, and the storage engine is what makes it
//! affordable. Sessions, visitors and page views are all *derived* from it, so nothing
//! is stored twice and nothing can drift apart.
//!
//! `revenue` is a separate table because it is written by the owner (imported from
//! Carbon Ads, AdSense, …), not by the tracker.
//!
//! **No index on the fact tables.** `event` and `revenue` are append only, and every
//! query over them is a filtered scan (`WHERE website_id = ? AND created_at BETWEEN ?`).
//! DuckDB answers those with **zone maps** — one min/max per column per row group,
//! maintained automatically and stored with the data — so a `PRIMARY KEY` would buy
//! nothing. It would cost a lot: DuckDB 2.0 persists the ART index of a primary key
//! to disk (measured: **+10.5 bytes per row**, about 12% of a realistic database),
//! slows every append by ~9%, and enforces the uniqueness constraint on every row.
//! The ids are handed out by the writer (from a counter in memory, seeded from
//! the table when the database is opened), so the constraint protects nothing.
//!
//! The small dimension table (`website`, `meta`) keeps its keys: they hold a handful
//! of rows, and `meta` needs its primary key for `ON CONFLICT` to work. The accounts
//! are **not** here — they live in the instance store (`service/src/instance.rs`),
//! in their own small file.

use std::path::{Path, PathBuf};

use anyhow::Result;

use duckdb::Connection;

/// Schema version of the database.
///
/// * v1 — the original session-table model.
/// * v2 — the event-row model (one row per page view).
/// * v3 — the fact tables lost their primary keys (no index on an append-only table).
/// * v4 — the session and visitor ids are **UUIDs**: 16 bytes instead of a
///   36-character string.
/// * v5 — the traffic **channel** is stored with the event (direct, search,
///   social, email, ads) instead of being worked out from the referrer at
///   report time.
/// * v6 — the fields the reference implementation has and we did not: the
///   **visit** id (a visit inside a session), the referrer's **display name**
///   and the **coordinates**. The channel's value set is aligned with it too.
/// * v7 — the ids are named after what they are: `visit_id` is the visit (what
///   we used to call the session id), `visitor_id` is the visitor, and
///   `user_id` is the identity the site owner knows the visitor by (Umami's
///   `distinct_id` column). The hour-long visit id of v6 is gone.
/// * v8 — the **session id is eight bytes**, not sixteen: the segment's start
///   is in the high half, so a report anchors a session's duration on the
///   moment it really started without a second column.
/// * v9 — the **visitor id is not stored**. The address (the raw one and its
///   coarse prefix) and the raw user agent are stored instead, and a visitor
///   is the pair (the prefix, the user agent) — so the reports count the
///   visitors out of the information, not out of an id.
/// * v10 — **both ids are sixteen bytes** (a UUID), like Umami's: the
///   `visitor_id` is stored again, and the `visit_id` is a UUID too. Sixteen
///   bytes have no room for the segment's start, so the start is stored
///   **beside** the id (`session_start`) — it is a fact, and a fact that is
///   worked out of a bucket is not the fact.
pub const SCHEMA_VERSION: i64 = 10;

/// The columns of the `event` table.
///
/// One row per page view (or custom event). The field set mirrors Umami's
/// `website_event` row for row.
pub const EVENT_COLUMNS: &str = "
    id              BIGINT NOT NULL,
    website_id      VARCHAR NOT NULL,
    visit_id        UUID NOT NULL,
    visitor_id      UUID NOT NULL,
    session_start   TIMESTAMP,
    ip              VARCHAR NOT NULL DEFAULT '',
    ip_prefix       VARCHAR NOT NULL DEFAULT '',
    user_agent      VARCHAR NOT NULL DEFAULT '',
    user_id         UUID,
    created_at      TIMESTAMP NOT NULL,

    url             VARCHAR,
    url_query       VARCHAR,
    title           VARCHAR,

    referrer        VARCHAR,
    referrer_path   VARCHAR,
    referrer_query  VARCHAR,
    referrer_name   VARCHAR,
    hostname        VARCHAR,
    channel         VARCHAR,

    browser         VARCHAR,
    browser_version VARCHAR,
    os              VARCHAR,
    os_version      VARCHAR,
    device          VARCHAR,
    screen          VARCHAR,
    language        VARCHAR,
    country         VARCHAR,
    region          VARCHAR,
    city            VARCHAR,
    latitude        DOUBLE,
    longitude       DOUBLE,

    utm_source      VARCHAR,
    utm_medium      VARCHAR,
    utm_campaign    VARCHAR,
    utm_content     VARCHAR,
    utm_term        VARCHAR,

    gclid           VARCHAR,
    fbclid          VARCHAR,
    msclkid         VARCHAR,
    ttclid          VARCHAR,
    lifatid         VARCHAR,
    twclid          VARCHAR,

    lcp             DOUBLE,
    inp             DOUBLE,
    cls             DOUBLE,
    fcp             DOUBLE,
    ttfb            DOUBLE,

    event_type      VARCHAR,
    event_name      VARCHAR,
    tag             VARCHAR,
    data            VARCHAR";

/// The columns of the `revenue` table.
pub const REVENUE_COLUMNS: &str = "
    id          BIGINT NOT NULL,
    website_id  VARCHAR NOT NULL,
    visit_id    UUID,
    event_id    BIGINT,
    event_name  VARCHAR,
    currency    VARCHAR NOT NULL DEFAULT 'USD',
    revenue     DOUBLE NOT NULL,
    created_at  TIMESTAMP NOT NULL";

/// The columns of the `website` table.
pub const WEBSITE_COLUMNS: &str = "
    id                VARCHAR PRIMARY KEY,
    name              VARCHAR NOT NULL,
    domain            VARCHAR,
    created_at        TIMESTAMP NOT NULL,
    updated_at        TIMESTAMP NOT NULL,
    retention_days    INTEGER NOT NULL DEFAULT 0,
    country_header    VARCHAR";

/// The columns of the `meta` table.
pub const META_COLUMNS: &str = "
    key   VARCHAR PRIMARY KEY,
    value VARCHAR";

/// The tables of the schema, with their columns.
const TABLES: &[(&str, &str)] = &[
    ("event", EVENT_COLUMNS),
    ("revenue", REVENUE_COLUMNS),
    ("website", WEBSITE_COLUMNS),
    ("meta", META_COLUMNS),
];

/// Create the schema if it does not exist.
pub fn init_schema(conn: &Connection) -> Result<()> {
    for (table, columns) in TABLES {
        // `user` is a reserved word.
        let quoted = if *table == "user" {
            "\"user\""
        } else {
            *table
        };
        conn.execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS {quoted} ({columns});"
        ))?;
    }

    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('schema_version', ?) \
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        [SCHEMA_VERSION.to_string()],
    )?;

    Ok(())
}

/// The schema version the database was written with (`0` for a fresh file).
pub fn schema_version(conn: &Connection) -> i64 {
    // `meta` does not exist yet on a fresh database.
    let Ok(mut stmt) = conn.prepare("SELECT value FROM meta WHERE key = 'schema_version'") else {
        return 0;
    };
    let Ok(mut rows) = stmt.query([]) else {
        return 0;
    };
    match rows.next() {
        Ok(Some(row)) => row
            .get::<_, Option<String>>(0)
            .ok()
            .flatten()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
        _ => 0,
    }
}

/// The names of the `event` table's columns, in order.
pub fn event_columns() -> Vec<String> {
    column_names(EVENT_COLUMNS)
}

/// Copy the database into a new file, and say where the new file is.
///
/// The caller closes the connection and swaps the files over.
///
/// **Why a new file and not an in-place rewrite.** DuckDB frees the blocks of
/// a dropped table but never gives them back to the filesystem — `VACUUM` does
/// not shrink the file either — so an in-place rewrite of a database with
/// 400 GB of events would leave an 800 GB file with 400 GB of dead space in
/// it. Writing a new file and swapping it in costs the same peak disk space
/// and ends with a compact file.
///
/// **The copy is as it is**: the same schema, the same rows. This is how a
/// database is made smaller after rows have been deleted — it is not a
/// migration, and it changes nothing about what is in the file.
pub fn compact_to_new_file(path: &Path, conn: &Connection) -> Result<PathBuf> {
    let new_path = new_database_path(path);
    let _ = std::fs::remove_file(&new_path);

    let escaped = new_path.to_string_lossy().replace('\'', "''");
    conn.execute_batch(&format!("ATTACH '{escaped}' AS fresh"))?;

    for (table, columns) in TABLES {
        let quoted = if *table == "user" { "\"user\"" } else { *table };
        conn.execute_batch(&format!("CREATE TABLE fresh.{quoted} ({columns});"))?;
        // A table the file never had stays empty rather than failing the copy.
        if table_exists(conn, table) {
            let names = column_names(columns);
            conn.execute_batch(&format!(
                "INSERT INTO fresh.{quoted} ({names}) SELECT {names} FROM main.{quoted};",
                names = names.join(", "),
            ))?;
        }
    }

    // The copy still says it was written by the old version — fix that, or the
    // new file would be replaced again the next time it is opened.
    conn.execute(
        "INSERT INTO fresh.meta (key, value) VALUES ('schema_version', ?) \
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        [SCHEMA_VERSION.to_string()],
    )?;

    conn.execute_batch("DETACH fresh")?;
    Ok(new_path)
}

/// Where the compacted database is written, before it is swapped in.
fn new_database_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "sumimi.duckdb".to_string());
    name.push_str(".compacted");
    path.with_file_name(name)
}

/// Does the database have this table?
fn table_exists(conn: &Connection, table: &str) -> bool {
    conn.query_row(
        "SELECT count(*) FROM information_schema.tables \
         WHERE table_name = ? AND table_schema = 'main'",
        [table],
        |row| row.get::<_, i64>(0),
    )
    .map(|n| n > 0)
    .unwrap_or(false)
}

/// Where an older database is **set aside** when it is replaced.
///
/// The name carries the version it was written with, so two upgrades in a row
/// do not overwrite each other's leftovers, and so it is obvious what the file
/// is if somebody finds it later.
pub fn versioned_backup_path(path: &Path, version: i64) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "sumimi.duckdb".to_string());
    name.push_str(&format!(".v{version}.bak"));
    path.with_file_name(name)
}

/// The values of one row of the `event` table, in the schema's order.
///
/// The appender takes the values **positionally**, so the order has to be the
/// schema's — and there is exactly one place that knows it. The server writes
/// its rows with this, and so does anything that writes rows the way the
/// server does.
///
/// The kind is a `&str` in the row, and a `&str` cannot become a trait object
/// on its own — so the caller hands over a reference to it.
pub fn event_row<'a>(
    id: &'a i64,
    hit: &'a crate::models::Hit,
    kind: &'a &'a str,
) -> Vec<&'a dyn duckdb::ToSql> {
    vec![
        id,
        &hit.website_id,
        &hit.visit_id,
        &hit.visitor_id,
        &hit.session_start,
        &hit.ip,
        &hit.ip_prefix,
        &hit.user_agent,
        &hit.user_id,
        &hit.created_at,
        &hit.url,
        &hit.url_query,
        &hit.title,
        &hit.referrer,
        &hit.referrer_path,
        &hit.referrer_query,
        &hit.referrer_name,
        &hit.hostname,
        &hit.channel,
        &hit.browser,
        &hit.browser_version,
        &hit.os,
        &hit.os_version,
        &hit.device,
        &hit.screen,
        &hit.language,
        &hit.country,
        &hit.region,
        &hit.city,
        &hit.latitude,
        &hit.longitude,
        &hit.utm_source,
        &hit.utm_medium,
        &hit.utm_campaign,
        &hit.utm_content,
        &hit.utm_term,
        &hit.gclid,
        &hit.fbclid,
        &hit.msclkid,
        &hit.ttclid,
        &hit.lifatid,
        &hit.twclid,
        &hit.lcp,
        &hit.inp,
        &hit.cls,
        &hit.fcp,
        &hit.ttfb,
        kind,
        &hit.name,
        &hit.tag,
        &hit.data,
    ]
}

/// The columns of a column definition, in order.
///
/// The definitions are written one per line, so the first word of each part is the
/// column name.
fn column_names(columns: &str) -> Vec<String> {
    columns
        .split(',')
        .filter_map(|part| part.split_whitespace().next())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

/// Now, as a naive UTC timestamp.
pub fn now() -> chrono::NaiveDateTime {
    chrono::Utc::now().naive_utc()
}
