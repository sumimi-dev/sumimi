//! The schema.
//!
//! **One fact table.** Every page view is one row in `website_event`, carrying the same fields
//! Umami carries — that is the baseline, and the storage engine is what makes it
//! affordable. Sessions, visitors and page views are all *derived* from it, so nothing
//! is stored twice and nothing can drift apart.
//!
//! `revenue` is a separate table because it is written by the owner (imported from
//! Carbon Ads, AdSense, …), not by the tracker.
//!
//! **One file per site** (since v12). The main file holds the sites (`website`)
//! and the bookkeeping (`meta`); the facts of each site live in a file of that
//! site's own, attached under the site's name. The reason is measured, not
//! guessed — see `D-046` and `examples/site_split.rs`: a query for one site
//! over a table that holds every site's rows reads the whole table's
//! aggregates, and the buffer manager holds every site's rows, not just the
//! one being asked about.
//!
//! ⇒ ⇒ The **shape** of the storage is the engine's business; the **口径**
//! (the numbers) is not — the statements are written once and the engine says
//! how it spells the table's name (see [`crate::query`]).
//!
//! **No index on the fact tables.** `website_event` and `revenue` are append only, and every
//! query over them is a filtered scan (`WHERE website_id = ? AND created_at BETWEEN ?`).
//! DuckDB answers those with **zone maps** — one min/max per column per row group,
//! maintained automatically and stored with the data — so a `PRIMARY KEY` would buy
//! nothing. It would cost a lot: DuckDB 2.0 persists the ART index of a primary key
//! to disk (measured: **+10.5 bytes per row**, about 12% of a realistic database),
//! slows every append by ~9%, and enforces the uniqueness constraint on every row.
//! The ids are handed out by the writer (from a counter in memory, seeded from
//! the tables when the database is opened), so the constraint protects nothing.
//!
//! The small dimension table (`website`, `meta`) keeps its keys: they hold a handful
//! of rows, and `meta` needs its primary key for `ON CONFLICT` to work. The accounts
//! are **not** here — they live in the instance store (`service/src/instance.rs`),
//! in their own small file.

use std::path::{Path, PathBuf};

use anyhow::Result;
use sha2::{Digest, Sha256};

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
/// * v11 — the visitor's column is called **`session_id`**, the name Umami
///   gives it: Umami's `session_id` is the visitor and their `visit_id` is
///   the stretch of activity, which is what our `visit_id` is. The ids
///   themselves did not change — the column did.
/// * v12 — **every site has a file of its own** (the facts), and the names are
///   **Umami's**: the table is `website_event`, the row id is `event_id`, the
///   page is `url_path`, its title is `page_title`, the referrer's host is
///   `referrer_domain`, and the click id is `li_fat_id`. ⇒ ⇒ The main file
///   holds the sites and the bookkeeping; each site's file holds its facts.
pub const SCHEMA_VERSION: i64 = 13;

/// How DuckDB spells the kinds.
pub struct Duck;

impl crate::schema::Types for Duck {
    fn row_id(&self) -> String {
        // **No key.** The fact tables are append only and are only ever read
        // as a filtered scan, so a primary key would buy nothing and cost a
        // lot (see the module's note).
        "BIGINT NOT NULL".to_string()
    }
    fn text(&self) -> String {
        "VARCHAR".to_string()
    }
    fn uuid(&self) -> String {
        "UUID".to_string()
    }
    fn blob(&self) -> String {
        "BLOB".to_string()
    }
    fn event_id(&self) -> String {
        "UUID".to_string()
    }
    fn int(&self) -> String {
        "BIGINT".to_string()
    }
    fn small(&self) -> String {
        "INTEGER".to_string()
    }
    fn real(&self) -> String {
        "DOUBLE".to_string()
    }
    fn time(&self) -> String {
        "TIMESTAMP".to_string()
    }
}

/// The tables of the **main** file: the sites and the bookkeeping.
///
/// **The columns are the shared list** (see [`crate::schema`]) — one list for
/// every engine, spelled here in DuckDB's dialect.
const MAIN_TABLES: &[(&str, &[crate::schema::Column])] = &[
    ("website", crate::schema::WEBSITE),
    ("meta", crate::schema::META),
];

/// The tables of a **site's** file: the facts.
///
/// The names are Umami's — `website_event` is the table their rows come from
/// and their tools read.
const SITE_TABLES: &[(&str, &[crate::schema::Column])] = &[
    ("website_event", crate::schema::EVENT),
    ("event_data", crate::schema::EVENT_DATA),
    ("revenue", crate::schema::REVENUE),
];

/// The columns of the fact table that were **renamed** in v12, and the names
/// they had before.
///
/// ⇒ ⇒ The migration reads the old file with the old names and writes the new
/// files with the new ones ⇒ ⇒ ⇒ the rows are carried over, not recomputed.
const RENAMED: &[(&str, &str)] = &[
    ("event_id", "id"),
    ("url_path", "url"),
    ("page_title", "title"),
    ("referrer_domain", "referrer"),
    ("li_fat_id", "lifatid"),
];

/// The name a column had before v12.
fn old_name(name: &str) -> &str {
    RENAMED
        .iter()
        .find(|(new, _)| *new == name)
        .map(|(_, old)| *old)
        .unwrap_or(name)
}

/// The columns of a table, in DuckDB's spelling.
fn columns_of(columns: &[crate::schema::Column]) -> String {
    crate::schema::columns_ddl(columns, &Duck)
}

/// The name of a table, as it is written in a statement.
///
/// A table in another database is written `catalog.table`; a table in this one
/// is written plain.
fn table_name(catalog: Option<&str>, table: &str) -> String {
    match catalog {
        Some(catalog) => format!("{catalog}.{table}"),
        None => table.to_string(),
    }
}

/// Create a table if it does not exist.
fn create_table(
    conn: &Connection,
    catalog: Option<&str>,
    table: &str,
    columns: &[crate::schema::Column],
) -> Result<()> {
    // `user` is a reserved word.
    let quoted = if table == "user" {
        "\"user\""
    } else {
        table
    };
    conn.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS {} ({});",
        table_name(catalog, quoted),
        columns_of(columns)
    ))?;
    Ok(())
}

/// Write the schema version into the `meta` table.
fn write_version(conn: &Connection, catalog: Option<&str>) -> Result<()> {
    conn.execute(
        &format!(
            "INSERT INTO {} (key, value) VALUES ('schema_version', ?) \
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            table_name(catalog, "meta")
        ),
        [SCHEMA_VERSION.to_string()],
    )?;
    Ok(())
}

/// The meta key of the instance field of the event ids.
pub const EVENT_ID_INSTANCE: &str = "event_id_instance";

/// The meta key of the fields this instance has used.
pub const EVENT_ID_INSTANCES_USED: &str = "event_id_instances_used";

/// The instance half of the event ids, as the instance's bookkeeping has it.
pub fn event_id_instance(conn: &Connection) -> Option<u128> {
    let value: String = conn
        .query_row(
            "SELECT value FROM meta WHERE key = ?",
            [EVENT_ID_INSTANCE],
            |row| row.get(0),
        )
        .ok()?;
    crate::event_id::instance_from_hex(value.trim())
}

/// The instance fields this instance has used.
///
/// A field that is in here belongs to this instance (or to one it took rows
/// from) — so no other field may be handed out for it.
pub fn used_event_id_instances(conn: &Connection) -> Vec<u128> {
    let value: String = conn
        .query_row(
            "SELECT value FROM meta WHERE key = ?",
            [EVENT_ID_INSTANCES_USED],
            |row| row.get(0),
        )
        .unwrap_or_default();
    value
        .split(',')
        .filter_map(|part| crate::event_id::instance_from_hex(part.trim()))
        .collect()
}

/// Record an instance half as used — the current one, and every one before it.
pub fn remember_event_id_instance(conn: &Connection, instance: u128) -> Result<()> {
    let mut used = used_event_id_instances(conn);
    if !used.contains(&instance) {
        used.push(instance);
    }
    used.sort_unstable();
    let list = used
        .iter()
        .map(|half| crate::event_id::instance_hex(*half))
        .collect::<Vec<_>>()
        .join(",");
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?, ?) \
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        [EVENT_ID_INSTANCES_USED, &list],
    )?;
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?, ?) \
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        [EVENT_ID_INSTANCE, &crate::event_id::instance_hex(instance)],
    )?;
    Ok(())
}

/// Move this instance to another field.
///
/// The field is what keeps two instances from handing out the same id. When a
/// site moves here from an instance that has the same field, the target moves
/// out of the way — **the ids of the rows that came with the site are not
/// touched**, so the references to them (`revenue`, the EAV rows, whatever the
/// owner kept) stay valid.
///
/// The new field is one this instance has never used, so its own older rows —
/// which carry the old field — cannot collide with the rows that are coming in.
pub fn switch_event_id_instance(conn: &Connection) -> Result<u128> {
    let mut used = used_event_id_instances(conn);
    if let Some(current) = event_id_instance(conn) {
        used.push(current);
    }
    let half = crate::event_id::pick_instance(&used);
    remember_event_id_instance(conn, half)?;
    Ok(half)
}

/// Create the **whole** schema in one database.
///
/// This is the **single-file** shape: the sites, the bookkeeping and the facts
/// side by side. The tools that measure things use it (they want one file to
/// look at), and so does the archive reader.
///
/// ⇒ ⇒ The server does **not** use this — it uses [`init_main_schema`] and
/// [`init_site_schema`], one file per site (see [`attach_site`]).
pub fn init_schema(conn: &Connection) -> Result<()> {
    for (table, columns) in MAIN_TABLES.iter().chain(SITE_TABLES.iter()) {
        create_table(conn, None, table, columns)?;
    }
    write_version(conn, None)?;
    Ok(())
}

/// Create the **main** file's schema: the sites and the bookkeeping.
pub fn init_main_schema(conn: &Connection) -> Result<()> {
    init_main_schema_at(conn, None)
}

/// Create the main file's schema in the database attached under `catalog`.
pub fn init_main_schema_at(conn: &Connection, catalog: Option<&str>) -> Result<()> {
    for (table, columns) in MAIN_TABLES {
        create_table(conn, catalog, table, columns)?;
    }
    write_version(conn, catalog)?;
    Ok(())
}

/// Create a **site's** schema in the database attached under `catalog`.
pub fn init_site_schema(conn: &Connection, catalog: &str) -> Result<()> {
    for (table, columns) in SITE_TABLES {
        create_table(conn, Some(catalog), table, columns)?;
    }
    Ok(())
}

/// The directory that holds the sites' files.
///
/// `data.duckdb` ⇒ `data.sites/`. The files live beside the main file, in a
/// directory of their own, so the instance is still **one directory** to back
/// up, move or delete.
pub fn sites_dir(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "sumimi.duckdb".to_string());
    name.push_str(".sites");
    path.with_file_name(name)
}

/// The alias a site's file is attached under.
///
/// **It has to be a valid identifier** — the site's id is a uuid, and a uuid
/// has dashes in it — and it has to be **the same every time**, so a restart
/// finds the file that was written before.
///
/// ⇒ ⇒ The alias is the site's id with everything that is not a letter or a
/// digit turned into an underscore, plus a short hash of the id ⇒ ⇒ ⇒ it is
/// always a valid identifier, always stable, and never two sites' alike.
pub fn site_alias(website_id: &str) -> String {
    let mut alias = String::from("s_");
    for character in website_id.chars() {
        if character.is_ascii_alphanumeric() {
            alias.push(character);
        } else {
            alias.push('_');
        }
    }
    alias.push('_');
    alias.push_str(&short_hash(website_id));
    alias
}

/// A short hash of a string — the first eight bytes of its SHA-256, in hex.
fn short_hash(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    let digest = hasher.finalize();
    digest[..8].iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The file of a site.
///
/// The extension is the engine's (`duckdb`, `sqlite`) — the site's file is
/// beside the main file, in the directory of [`sites_dir`].
pub fn site_path(path: &Path, website_id: &str, extension: &str) -> PathBuf {
    sites_dir(path).join(format!("{}.{extension}", site_alias(website_id)))
}

/// The files of an instance: the main file and every site's file.
pub fn instance_files(path: &Path, extension: &str) -> Vec<PathBuf> {
    let mut files = vec![path.to_path_buf()];
    if let Ok(entries) = std::fs::read_dir(sites_dir(path)) {
        for entry in entries.filter_map(|entry| entry.ok()) {
            let file = entry.path();
            if file.extension().and_then(|e| e.to_str()) == Some(extension) {
                files.push(file);
            }
        }
    }
    files
}

/// Attach a site's file, and make sure its tables are there.
///
/// **The file is created if it is not there** — a site that has never been
/// written to still gets a file of its own, so the operator can see one file
/// per site.
///
/// The alias is the site's own: `ATTACH … AS s_<the site's id>` ⇒ ⇒ the
/// statements can say `s_….website_event` and mean **that site's** rows.
///
/// **A read-only connection attaches read-only.** DuckDB gives an `ATTACH` the
/// access mode of the database it is attached to — so a plain `ATTACH` under a
/// read-only connection is read-only too. The mode is spelled out anyway
/// (`READ_ONLY`), because the alternative — letting it be inherited — means
/// the mode of the site files is decided by the main file's, and a reader
/// that forgot to say so would open the facts **writable**.
pub fn attach_site(conn: &Connection, path: &Path, website_id: &str) -> Result<String> {
    let alias = site_alias(website_id);
    let file = site_path(path, website_id, "duckdb");
    let read_only = is_read_only(conn);

    if let Some(parent) = file.parent() {
        if !parent.as_os_str().is_empty() {
            // A read-only connection must not make a directory either — the
            // file it attaches has to be there already.
            if read_only && !parent.exists() {
                anyhow::bail!(
                    "the site's directory {} is not there — the instance looks incomplete",
                    parent.display()
                );
            }
            std::fs::create_dir_all(parent).map_err(|e| {
                anyhow::anyhow!("creating the sites directory {}: {e}", parent.display())
            })?;
        }
    }

    let escaped = file.to_string_lossy().replace('\'', "''");
    let mode = if read_only { " (READ_ONLY)" } else { "" };
    conn.execute_batch(&format!("ATTACH IF NOT EXISTS '{escaped}' AS {alias}{mode}"))?;
    // **The tables are only made when they can be.** A read-only connection
    // cannot run `CREATE TABLE` at all — DuckDB refuses the statement before
    // it looks at whether the table is there — so a reader that needs the
    // tables to exist is a writer, and the two cannot be the same connection.
    if !read_only {
        init_site_schema(conn, &alias)?;
    }
    Ok(alias)
}

/// Is this connection read-only?
///
/// DuckDB keeps the access mode as a setting of the connection, so it can be
/// asked: `current_setting('access_mode')` answers `read_only` or `read_write`.
pub fn is_read_only(conn: &Connection) -> bool {
    conn.query_row("SELECT current_setting('access_mode')", [], |row| {
        row.get::<_, String>(0)
    })
    .map(|mode| mode.eq_ignore_ascii_case("read_only"))
    .unwrap_or(false)
}

/// The ids of the sites in the main file.
pub fn site_ids(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT id FROM website ORDER BY created_at ASC")?;
    let mut rows = stmt.query([])?;
    let mut ids = Vec::new();
    while let Some(row) = rows.next()? {
        ids.push(row.get(0)?);
    }
    Ok(ids)
}

/// The largest counter in a fact table — in the database attached under
/// `catalog`.
///
/// The counter that hands the ids out starts above this, so the ids stay
/// unique across the sites and across restarts.
///
/// **Only the rows of this instance count.** The ids of another instance
/// (rows that came with a site that moved here) carry that instance's half,
/// and their counters are not ours to continue — so the range is the one
/// this instance's half covers.
pub fn largest_counter(conn: &Connection, catalog: Option<&str>, instance: u128) -> Result<u64> {
    let low = crate::event_id::compose(instance, 0);
    let high = crate::event_id::compose(instance, crate::event_id::COUNTER_MAX);
    let sql = format!(
        "SELECT MAX(event_id) FROM {} WHERE event_id BETWEEN ? AND ?",
        table_name(catalog, "website_event")
    );
    let id: Option<uuid::Uuid> = conn
        .query_row(&sql, [&low, &high], |row| row.get(0))
        .unwrap_or(None);
    Ok(id.map(|id| crate::event_id::counter_of(&id)).unwrap_or(0))
}

/// The largest visit counter this instance has used.
///
/// **Only the rows of this instance count**, and only the ones in the visit
/// layout: the ids of another instance (rows that came with a site that moved
/// here) carry that instance's half, and a client's own session id is not ours
/// at all. The version nibble tells the two layouts apart, so the range below
/// covers the visit ids of this instance and nothing else.
pub fn largest_visit_counter(
    conn: &Connection,
    catalog: Option<&str>,
    instance: u128,
) -> Result<u64> {
    let low = crate::event_id::compose_version(crate::event_id::VISIT_VERSION, instance, 0);
    let high = crate::event_id::compose_version(
        crate::event_id::VISIT_VERSION,
        instance,
        crate::event_id::COUNTER_MAX,
    );
    let sql = format!(
        "SELECT MAX(visit_id) FROM {} WHERE visit_id BETWEEN ? AND ?",
        table_name(catalog, "website_event")
    );
    let id: Option<uuid::Uuid> = conn
        .query_row(&sql, [&low, &high], |row| row.get(0))
        .unwrap_or(None);
    Ok(id.map(|id| crate::event_id::counter_of(&id)).unwrap_or(0))
}

/// Split a database of the old shape into the new one: **one file per site**.
///
/// The old file holds the sites, the bookkeeping and the facts in one place.
/// The new one holds the sites and the bookkeeping in the main file, and the
/// facts of each site in a file of its own.
///
/// **The rows are carried over.** This is the first change of the schema that
/// does not throw the data away: the sites' rows move into their own files
/// (with the columns renamed on the way), and the old file is kept beside the
/// new one as `….v11.bak`.
///
/// Returns the path of the **new main file** — the caller closes the old
/// connection and swaps the two over.
pub fn split_into_sites(path: &Path, conn: &Connection, instance: u128) -> Result<PathBuf> {
    let ids = site_ids(conn)?;

    // **The new ids are made first, for the whole table.**
    //
    // The old ids were integers; the new ones are 128 bits ⇒ ⇒ the rows get
    // a fresh id. The map is built **once**, over every row, because the
    // counters have to be unique across the whole instance — not per site.
    // The revenue rows that point at an event are rewritten with the same map.
    if table_exists(conn, "event") {
        conn.execute_batch(&format!(
            "CREATE OR REPLACE TEMP TABLE id_map AS \
             SELECT id AS old_id, {new_id} AS new_id FROM (\
                 SELECT id, row_number() OVER () - 1 AS n FROM main.event\
             )",
            new_id = uuid_of_counter(instance, "n"),
        ))?;
    }

    // Every site's rows move into a file of its own.
    for id in &ids {
        let alias = attach_site(conn, path, id)?;
        copy_site_rows(conn, &alias, id)?;
    }

    // The new main file: the sites and the bookkeeping, and nothing else.
    //
    // **It is written from this connection** — the old file is locked by it, so
    // it cannot be opened a second time to be read from.
    let new_path = new_main_path(path);
    let _ = std::fs::remove_file(&new_path);
    let escaped = new_path.to_string_lossy().replace('\'', "''");
    conn.execute_batch(&format!("ATTACH '{escaped}' AS new"))?;
    init_main_schema_at(conn, Some("new"))?;
    conn.execute("INSERT INTO new.website SELECT * FROM main.website", [])?;
    // **The half the migration used is the instance's half** — so the rows
    // that were carried over and the rows written from now on are in one
    // namespace.
    conn.execute(
        "INSERT INTO new.meta (key, value) VALUES (?, ?) \
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        [EVENT_ID_INSTANCE, &crate::event_id::instance_hex(instance)],
    )?;
    conn.execute_batch("DETACH new")?;
    conn.execute_batch("DROP TABLE IF EXISTS id_map;")?;

    Ok(new_path)
}

/// Where the new main file is written, before it is swapped in.
fn new_main_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "sumimi.duckdb".to_string());
    name.push_str(".v12.new");
    path.with_file_name(name)
}

/// Copy one site's rows into its own file.
///
/// The old file's table is `event`; the new one is `website_event`, and five
/// of the columns were renamed on the way ⇒ ⇒ the copy names the old column
/// on the right-hand side and the new one on the left-hand side.
fn copy_site_rows(conn: &Connection, alias: &str, website_id: &str) -> Result<()> {
    if table_exists(conn, "event") {
        // **The ids are new** — the map was built over the whole table (see
        // `split_into_sites`), so the events and the revenue rows agree on
        // which new id belongs to which old one.
        let new_names: Vec<&str> = crate::schema::EVENT.iter().map(|c| c.name).collect();
        let old_names: Vec<&str> = crate::schema::EVENT.iter().map(|c| old_name(c.name)).collect();
        let columns: Vec<String> = new_names
            .iter()
            .zip(old_names.iter())
            .map(|(new, old)| {
                if *new == "event_id" {
                    "m.new_id".to_string()
                } else if *new == "ver" {
                    // **The old table has no `ver`** — the column was added
                    // in v0.6, so an old file cannot have it ⇒ ⇒ the rows that
                    // are carried over do not know which version served them.
                    "NULL".to_string()
                } else {
                    format!("e.{old}")
                }
            })
            .collect();
        conn.execute(
            &format!(
                "INSERT INTO {alias}.website_event ({new}) SELECT {old} FROM main.event e \
                 JOIN id_map m ON e.id = m.old_id WHERE e.website_id = ?",
                new = new_names.join(", "),
                old = columns.join(", "),
            ),
            [website_id],
        )?;

        // The old `data` column — the properties of an event, as one JSON
        // object — becomes the **rows of the property table** ⇒ ⇒ the
        // properties survive the move instead of being dropped with the
        // column.
        if column_exists(conn, "event", "data") {
            conn.execute(
                &format!(
                    "INSERT INTO {alias}.event_data \
                     (event_id, website_id, session_id, url_path, event_name, created_at, \
                      data_key, string_value, number_value, date_value, data_type) \
                     SELECT m.new_id, e.website_id, e.session_id, e.url, e.event_name, e.created_at, \
                            j.key, \
                            CASE WHEN json_type(j.value) = 'VARCHAR' THEN j.value END, \
                            CASE WHEN json_type(j.value) IN ('DOUBLE', 'BIGINT', 'INTEGER') \
                                 THEN CAST(j.value AS DOUBLE) END, \
                            NULL, \
                            CASE WHEN json_type(j.value) IN ('DOUBLE', 'BIGINT', 'INTEGER') \
                                 THEN 'number' ELSE 'string' END \
                     FROM main.event e \
                     JOIN id_map m ON e.id = m.old_id, \
                          LATERAL json_each(e.data) j \
                     WHERE e.website_id = ? AND e.data IS NOT NULL AND json_valid(e.data)",
                ),
                [website_id],
            )?;
        }
    }

    if table_exists(conn, "revenue") {
        let names: Vec<&str> = crate::schema::REVENUE.iter().map(|c| c.name).collect();
        let columns: Vec<String> = names
            .iter()
            .map(|name| {
                if *name == "event_id" {
                    "m.new_id".to_string()
                } else {
                    format!("r.{name}")
                }
            })
            .collect();
        conn.execute(
            &format!(
                "INSERT INTO {alias}.revenue ({new}) SELECT {old} FROM main.revenue r \
                 LEFT JOIN id_map m ON r.event_id = m.old_id WHERE r.website_id = ?",
                new = names.join(", "),
                old = columns.join(", "),
            ),
            [website_id],
        )?;
    }

    Ok(())
}

/// Does the table have the column?
///
/// **The old file's shape is not known in advance** — a column that v0.6
/// added (`ver`) is not in a file written before it, and a column it removed
/// (`data`) is only in one written before it ⇒ ⇒ the migration asks.
fn column_exists(conn: &Connection, table: &str, column: &str) -> bool {
    conn.query_row(
        "SELECT count(*) FROM information_schema.columns \
         WHERE table_name = ? AND column_name = ?",
        [table, column],
        |row| row.get::<_, i64>(0),
    )
    .map(|n| n > 0)
    .unwrap_or(false)
}

/// **The uuid of the n-th event** — the instance's half, and the number.
///
/// The instance's half is the twenty hex characters of the id's first ten
/// bytes ⇒ ⇒ the counter goes into the last six bytes, big-endian, the way
/// `crate::event_id` lays it out.
fn uuid_of_counter(instance: u128, counter: &str) -> String {
    format!(
        "CAST(from_hex('{half}' || lpad(to_hex({counter}), 12, '0')) AS UUID)",
        half = crate::event_id::instance_hex(instance)
    )
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
    crate::schema::names(crate::schema::EVENT)
        .into_iter()
        .map(str::to_string)
        .collect()
}

/// The columns of the `event` table, in DuckDB's spelling.
///
/// The measurement examples ask for the schema this way — they need the same
/// columns with **one** thing changed (a primary key, a different id type), and
/// a string they can cut up cannot say that.
pub fn event_columns_ddl() -> String {
    columns_of(crate::schema::EVENT)
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

    for (table, columns) in MAIN_TABLES.iter().chain(SITE_TABLES.iter()) {
        // A table the file never had stays empty rather than failing the copy.
        if !table_exists(conn, table) {
            continue;
        }
        let quoted = if *table == "user" { "\"user\"" } else { *table };
        conn.execute_batch(&format!(
            "CREATE TABLE fresh.{quoted} ({});",
            columns_of(columns)
        ))?;
        let names = crate::schema::names(columns).join(", ");
        conn.execute_batch(&format!(
            "INSERT INTO fresh.{quoted} ({names}) SELECT {names} FROM main.{quoted};",
        ))?;
    }

    // The copy still says it was written by the old version — fix that, or the
    // new file would be replaced again the next time it is opened.
    //
    // ⇒ ⇒ A file that has no `meta` — **a site's file** — has no version to
    // write, and the write would fail on a table that is not there.
    if table_exists(conn, "meta") {
        conn.execute(
            "INSERT INTO fresh.meta (key, value) VALUES ('schema_version', ?) \
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            [SCHEMA_VERSION.to_string()],
        )?;
    }

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
// **The visitor's id is a `Vec<u8>`, not a slice** — the duckdb crate
// implements `ToSql` for `Vec<u8>` and for nothing else, so a `&[u8]` would
// have nothing to become.
#[allow(clippy::ptr_arg)]
pub fn event_row<'a>(
    id: &'a uuid::Uuid,
    hit: &'a crate::models::Hit,
    kind: &'a &'a str,
    session_id: &'a Vec<u8>,
) -> Vec<&'a dyn duckdb::ToSql> {
    vec![
        id,
        &hit.website_id,
        &hit.visit_id,
        session_id,
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
        &hit.ver,
    ]
}

/// Now, as a naive UTC timestamp.
pub fn now() -> chrono::NaiveDateTime {
    chrono::Utc::now().naive_utc()
}
