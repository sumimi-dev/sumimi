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
pub const SCHEMA_VERSION: i64 = 7;

/// The columns of the `event` table.
///
/// One row per page view (or custom event). The field set mirrors Umami's
/// `website_event` row for row.
pub const EVENT_COLUMNS: &str = "
    id              BIGINT NOT NULL,
    website_id      VARCHAR NOT NULL,
    visit_id        UUID NOT NULL,
    visitor_id      UUID NOT NULL,
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

/// Rewrite an older database into a new file, and say where the new file is.
///
/// The caller closes the connection and swaps the files over.
///
/// **Why a new file and not an in-place rebuild.** DuckDB cannot drop a constraint,
/// so the fact tables have to be recreated. DuckDB frees the blocks of a dropped
/// table but never gives them back to the filesystem — `VACUUM` does not shrink the
/// file either — so an in-place rebuild of a database with 400 GB of events would
/// leave a 800 GB file with 400 GB of dead space in it. Writing a new file and
/// swapping it in costs the same peak disk space and ends with a compact file.
///
/// The peak disk use is therefore about twice the size of the database. That is
/// unavoidable for any rewrite, and it is a one-off.
pub fn rebuild_to_new_file(path: &Path, conn: &Connection) -> Result<PathBuf> {
    let new_path = new_database_path(path);
    let _ = std::fs::remove_file(&new_path);

    let escaped = new_path.to_string_lossy().replace('\'', "''");

    // Which tables does the old database actually have, and how is each column
    // read out of them? This is worked out **before** the new database is
    // attached: `information_schema` spans every attached database, and the
    // attached one already has the new columns — so a check made afterwards
    // would find the *new* table's columns and read them out of the old table.
    let existing: Vec<&str> = TABLES
        .iter()
        .map(|(table, _)| *table)
        .filter(|table| table_exists(conn, table))
        .collect();
    let selections: Vec<String> = TABLES
        .iter()
        .map(|(table, columns)| migration_select_list(conn, table, &column_names(columns)))
        .collect();

    conn.execute_batch(&format!("ATTACH '{escaped}' AS fresh"))?;

    for ((table, columns), selection) in TABLES.iter().zip(selections.iter()) {
        let quoted = if *table == "user" {
            "\"user\""
        } else {
            *table
        };
        conn.execute_batch(&format!("CREATE TABLE fresh.{quoted} ({columns});"))?;
        // A table the old database never had (an early v2 file has no `revenue`)
        // stays empty rather than failing the migration.
        if existing.contains(table) {
            let names = column_names(columns);
            conn.execute_batch(&format!(
                "INSERT INTO fresh.{quoted} ({names}) SELECT {selection} FROM main.{quoted};",
                names = names.join(", "),
            ))?;
        }
    }

    // The copy still says it was written by the old version — fix that, or the new
    // file would be migrated again the next time it is opened.
    conn.execute(
        "INSERT INTO fresh.meta (key, value) VALUES ('schema_version', ?) \
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        [SCHEMA_VERSION.to_string()],
    )?;

    conn.execute_batch("DETACH fresh")?;
    Ok(new_path)
}

/// The names of the `event` table's columns, in order.
pub fn event_columns() -> Vec<String> {
    column_names(EVENT_COLUMNS)
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

/// How to read each column of a table out of the old database.
///
/// Most columns are copied as they are. The session and visitor ids are not: before
/// v4 they were 36-character strings, and they are now 16-byte UUIDs.
///
/// The conversion is **deterministic**: an id that is already UUID-shaped is kept
/// as it is (the ids the server derived were a hash formatted as a UUID, so they
/// survive the migration unchanged), and anything else — a session id the client
/// sent — is hashed with SHA-256 and the first 16 bytes are kept. The same old id
/// always becomes the same UUID, so no session is split in two.
fn migration_select_list(conn: &Connection, table: &str, names: &[String]) -> String {
    names
        .iter()
        .map(|name| {
            // The old `visit_id` was a half-hour window inside the session; the
            // new `visit_id` is that session itself, which the old file stored
            // as `session_id`. The old hour-long window is dropped.
            let source = match (table, name.as_str()) {
                ("event", "visit_id") | ("revenue", "visit_id") => "session_id",
                _ => name.as_str(),
            };
            // A column the old database never had stays empty.
            if !column_exists(conn, table, source) {
                return "NULL".to_string();
            }
            let is_id = matches!(
                (table, source),
                ("event", "session_id")
                    | ("event", "visit_id")
                    | ("event", "visitor_id")
                    | ("revenue", "session_id")
                    | ("revenue", "visit_id")
            );
            if is_id {
                uuid_from_text(source)
            } else {
                source.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The SQL that turns an old text id into a UUID.
///
/// The hash is taken over the **text** of the column: `sha256` has no UUID
/// overload, and DuckDB resolves the function when it binds the statement — so
/// `sha256(session_id)` on a column that is already `UUID` is a binder error,
/// even when the `CASE` would never take that branch. A v4 file has UUID
/// columns, so the cast is not optional.
fn uuid_from_text(column: &str) -> String {
    let hash = format!("sha256(CAST({column} AS VARCHAR))");
    format!(
        "CASE WHEN try_cast({column} AS UUID) IS NOT NULL \
           THEN try_cast({column} AS UUID) \
           ELSE CAST(substr({hash}, 1, 8) || '-' || substr({hash}, 9, 4) || '-' \
                     || substr({hash}, 13, 4) || '-' || substr({hash}, 17, 4) || '-' \
                     || substr({hash}, 21, 12) AS UUID) END"
    )
}

/// Does the table have this column?
fn column_exists(conn: &Connection, table: &str, column: &str) -> bool {
    conn.query_row(
        "SELECT count(*) FROM information_schema.columns \
         WHERE table_name = ?1 AND table_schema = 'main' AND column_name = ?2",
        duckdb::params![table, column],
        |row| row.get::<_, i64>(0),
    )
    .map(|n| n > 0)
    .unwrap_or(false)
}

/// Where the rebuilt database is written, before it is swapped in.
fn new_database_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "sumimi.duckdb".to_string());
    name.push_str(".rebuilt");
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

/// Now, as a naive UTC timestamp.
pub fn now() -> chrono::NaiveDateTime {
    chrono::Utc::now().naive_utc()
}
