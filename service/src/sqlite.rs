//! The SQLite backend — the alternative to DuckDB.
//!
//! SQLite is a row store: it stores more bytes than DuckDB for the same data, but it
//! is a single file with no server, it allows many readers next to one writer, and it
//! is available everywhere. For teams that already run everything on SQLite — or that
//! want a backend with no surprises — this is the backend to use.
//!
//! The **same** [`Storage`](crate::storage::Storage) trait as DuckDB, the same
//! reports, the same numbers: the conformance suite runs against both.
//!
//! **One fact table**, exactly like the DuckDB backend: every page view is one row
//! in `website_event`, and the sessions and visitors are derived from it. Timestamps
//! are stored as **epoch microseconds** (INTEGER), which is SQLite's natural
//! representation and makes range filters a plain integer comparison.
//!
//! **One file per site** (since v12), the same shape the DuckDB backend has: the
//! main file holds the sites (`website`) and the bookkeeping (`meta`); each site's
//! facts live in a file of that site's own, attached under the site's name. SQLite
//! has no `USE`, so the tables of another file are named with the alias in front
//! (`s_….website_event`) — the same shape DuckDB writes.
//!
//! The SQL is SQLite's dialect on purpose — bucketing, distinct counting and the
//! "the row of the newest hit" lookups all differ between engines.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};

use crate::models::{
    BreakdownRow, EventRow, Hit, MetricsSummary, MetricValue, Revenue, SessionRow, Website,
};
use crate::pool::{Pool, Pooled};
use crate::query::{Dialect, Granularity, Param};
use crate::storage::{
    group_sessions, session_scan_limit, Dimension, Layout, ReportQuery, SessionEvent, Storage,
    StorageStats, SummaryMetrics, WebsitePatch,
};

/// How SQLite spells the kinds.
///
/// **A uuid is text.** SQLite has no uuid type, so the id is written as its
/// text — the same way the session and the visitor's id are.
struct Sqlite;

impl crate::schema::Types for Sqlite {
    fn row_id(&self) -> String {
        // The rowid alias: SQLite counts the rows itself.
        "INTEGER PRIMARY KEY".to_string()
    }
    fn text(&self) -> String {
        "TEXT".to_string()
    }
    fn uuid(&self) -> String {
        "TEXT".to_string()
    }
    fn blob(&self) -> String {
        // SQLite has no uuid type either: the visitor's id goes in as raw
        // bytes, so the column is eight bytes and not thirty-six characters.
        "BLOB".to_string()
    }
    fn event_id(&self) -> String {
        // SQLite has no uuid type: the event id goes in as sixteen raw
        // bytes. **Not** as text (a 36-character string costs 72 bytes a
        // row more than the blob — measured) and **not** as the primary
        // key (the index would cost 42 bytes a row more, and nothing ever
        // looks an event up by its id).
        "BLOB".to_string()
    }
    fn int(&self) -> String {
        "INTEGER".to_string()
    }
    fn small(&self) -> String {
        "INTEGER".to_string()
    }
    fn real(&self) -> String {
        "REAL".to_string()
    }
    fn time(&self) -> String {
        // Microseconds since the epoch — an integer comparison.
        "INTEGER".to_string()
    }
}

/// The tables of the **main** file: the sites and the bookkeeping.
///
/// **The columns are the shared list** (see [`crate::schema`]) — one list for
/// every engine, spelled here in SQLite's dialect.
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

/// The indexes the reports read by.
///
/// The facts are read by `(website_id, created_at)` — a filtered scan — so the
/// index is on that pair and nothing else.
///
/// **The index's name carries the alias; the table's does not.** SQLite writes
/// an index into the database the table is in, and the only way to say which
/// database that is, is to qualify the index's name — qualifying the table's
/// name is a syntax error (`near "."`).
fn index_ddl(alias: &str) -> String {
    format!(
        "CREATE INDEX IF NOT EXISTS {alias}.website_event_website_created ON website_event (website_id, created_at);\n\
         CREATE INDEX IF NOT EXISTS {alias}.revenue_website_created ON revenue (website_id, created_at);\n"
    )
}

/// How SQLite spells the things that differ.
///
/// Everything else — the columns, the conditions, the counts — is in the
/// shared queries (see [`crate::query`]).
///
/// **The table's name is here too**, because where the rows live is the
/// engine's business: in the shape of an instance every site has a file of
/// its own, so the table is written `s_….website_event`; in the flat shape
/// (an in-memory database) it is written plain.
pub struct SqliteDialect {
    /// How the fact tables are reached.
    layout: Layout,
}

impl SqliteDialect {
    /// The dialect of the given shape.
    pub fn new(layout: Layout) -> Self {
        Self { layout }
    }

    /// The alias a site's tables live under.
    fn alias(&self, website_id: &str) -> Option<String> {
        match self.layout {
            Layout::PerSite => Some(crate::db::site_alias(website_id)),
            Layout::Flat => None,
        }
    }
}

impl Dialect for SqliteDialect {
    /// SQLite has no uuid type: the id is sixteen bytes in a blob, and this
    /// puts them into the **standard text form** — the same string DuckDB
    /// prints for its uuid column.
    ///
    /// `hex()` gives the thirty-two hex characters; the dashes go back in
    /// where they belong (8-4-4-4-12).
    fn event_id_text(&self, expr: &str) -> String {
        let hex = format!("lower(hex({expr}))");
        format!(
            "(substr({hex}, 1, 8) || '-' || substr({hex}, 9, 4) || '-' || substr({hex}, 13, 4) || '-' || \
             substr({hex}, 17, 4) || '-' || substr({hex}, 21, 12))",
            hex = hex
        )
    }

    /// SQLite's parameters are numbered: `?1`, `?2`, …
    fn placeholder(&self, n: usize) -> String {
        format!("?{n}")
    }

    /// The moments are microseconds already.
    fn epoch_us(&self, expr: &str) -> String {
        expr.to_string()
    }

    /// SQLite has no `GREATEST`; the scalar `MAX` is the same thing.
    fn greatest(&self, a: &str, b: &str) -> String {
        format!("MAX({a}, {b})")
    }

    fn cast_double(&self, expr: &str) -> String {
        format!("CAST({expr} AS REAL)")
    }

    /// `strftime` reads **seconds**, so the microseconds are divided first.
    fn bucket(&self, granularity: Granularity, expr: &str) -> String {
        let format = match granularity {
            Granularity::Hour => "%Y-%m-%dT%H:00:00Z",
            Granularity::Day => "%Y-%m-%dT00:00:00Z",
        };
        format!("strftime('{format}', {expr} / 1000000.0, 'unixepoch')")
    }

    fn event_table(&self, website_id: &str) -> String {
        match self.alias(website_id) {
            Some(alias) => format!("{alias}.website_event"),
            None => "website_event".to_string(),
        }
    }

    fn revenue_table(&self, website_id: &str) -> String {
        match self.alias(website_id) {
            Some(alias) => format!("{alias}.revenue"),
            None => "revenue".to_string(),
        }
    }

    /// The property table, in the site's own file.
    fn event_data_table(&self, website_id: &str) -> String {
        match self.alias(website_id) {
            Some(alias) => format!("{alias}.event_data"),
            None => "event_data".to_string(),
        }
    }
}

/// A row of SQLite, read by the name of its column.
pub struct SqliteRow<'a>(pub &'a rusqlite::Row<'a>);

impl crate::row::Row for SqliteRow<'_> {
    fn text(&self, name: &str) -> Result<Option<String>> {
        Ok(self.0.get::<_, Option<String>>(name)?)
    }
    fn int(&self, name: &str) -> Result<Option<i64>> {
        Ok(self.0.get::<_, Option<i64>>(name)?)
    }
    fn float(&self, name: &str) -> Result<Option<f64>> {
        Ok(self.0.get::<_, Option<f64>>(name)?)
    }
    /// A uuid is stored as its text.
    fn uuid(&self, name: &str) -> Result<Option<uuid::Uuid>> {
        Ok(self
            .0
            .get::<_, Option<String>>(name)?
            .and_then(|text| uuid::Uuid::parse_str(&text).ok()))
    }
    fn blob(&self, name: &str) -> Result<Option<Vec<u8>>> {
        Ok(self.0.get::<_, Option<Vec<u8>>>(name)?)
    }
    /// SQLite has no uuid type: the event id is the **sixteen bytes** the
    /// column holds, and they are the id.
    fn event_id(&self, name: &str) -> Result<Option<uuid::Uuid>> {
        Ok(self.0.get::<_, Option<Vec<u8>>>(name)?.map(|bytes| {
            uuid::Uuid::from_bytes(<[u8; 16]>::try_from(bytes.as_slice()).unwrap_or([0; 16]))
        }))
    }
    /// A moment is stored as microseconds since the epoch.
    fn time(&self, name: &str) -> Result<Option<chrono::NaiveDateTime>> {
        Ok(self.0.get::<_, Option<i64>>(name)?.map(from_ts))
    }
}

/// The values of a statement, as SQLite wants them.
fn values(statement: &crate::query::Statement) -> Vec<Box<dyn rusqlite::ToSql>> {
    statement
        .values
        .iter()
        .map(|param| -> Box<dyn rusqlite::ToSql> {
            match param {
                Param::Text(text) => Box::new(text.clone()),
                Param::Int(number) => Box::new(*number),
                Param::Float(number) => Box::new(*number),
                Param::Uuid(id) => Box::new(id.to_string()),
                // The event id goes in as the **sixteen bytes** it is made of —
                // SQLite has no uuid type, and the column is a blob.
                Param::EventId(id) => Box::new(id.as_bytes().to_vec()),
                Param::Time(moment) => Box::new(ts(*moment)),
                Param::Null => Box::new(Option::<String>::None),
            }
        })
        .collect()
}

/// The SQLite backend.
///
/// **One writer, a pool of readers.** SQLite in WAL mode lets many readers work
/// while the writer is busy — the opposite of DuckDB, which takes an exclusive
/// lock on the file. So the dashboard's requests each take a connection from the
/// pool, and a slow report does not hold up the writer.
pub struct SqliteBackend {
    /// The one connection that writes.
    writer: Mutex<Connection>,
    /// The connections the requests read on. `None` for an in-memory database:
    /// every `open_in_memory()` is a *different* database, so there is only the
    /// one connection.
    readers: Option<Pool<Connection>>,
    path: PathBuf,
    /// How the fact tables are reached.
    layout: Layout,
    /// The sites whose file is attached.
    ///
    /// **Every connection has to attach the site's file for itself** — the
    /// attachment belongs to the connection, not to the database — so the set
    /// says which files exist, not which connections have them.
    sites: Mutex<HashSet<String>>,
    /// The visit ids — the counter that mints them.
    ///
    /// It is a **second counter**, in the visit layout (version 9): the event
    /// ids and the visit ids are separate spaces.
    visit_ids: crate::event_id::Generator,
    /// The ids of the rows of the fact table.
    ///
    /// The same shape as the DuckDB backend's (`crate::event_id`) — the two
    /// engines have to answer the same, down to the id in the log.
    event_ids: crate::event_id::Generator,
}

impl SqliteBackend {
    /// Open (or create) a database and make sure the schema exists.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }

        // An older database is **replaced**, not migrated — the same rule as the
        // DuckDB backend (`pool.rs`): the session id's shape changed, and the
        // rows of an old file cannot be carried over. The old file is set aside
        // under its version number, so nothing is destroyed; the analysis data
        // and the site registrations are simply not carried over.
        //
        // **v11 is the exception**: the change to v12 is a change of *shape* —
        // the facts move into a file per site, and five columns are renamed —
        // and neither of those changes what a row *means*. ⇒ ⇒ So the rows
        // are carried over: they move into their site's file, and the old file
        // is kept beside the new one.
        let mut conn = Connection::open(path)?;
        let version = read_schema_version(&conn)?;
        if version > 0 && version < SCHEMA_VERSION {
            let backup = crate::db::versioned_backup_path(path, version);
            if version == 11 {
                tracing::warn!(
                    "the database {} was written by schema v{version}: it is split into one file per \
                     site — the rows are carried over, and the old file is kept as {}",
                    path.display(),
                    backup.display()
                );
                // The half the carried-over rows get — the new file keeps it.
                let instance = crate::event_id::pick_instance(&[]);
                let rebuilt = split_into_sites(path, &conn, instance)?;
                // Fold the write-ahead log into the file first, so the copy that
                // is kept aside really holds everything the old database had.
                conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
                drop(conn);
                std::fs::rename(path, &backup).map_err(|e| {
                    anyhow::anyhow!(
                        "setting the old database {} aside as {}: {e}",
                        path.display(),
                        backup.display()
                    )
                })?;
                // The log and the index belong to the old file.
                remove_side_files(path);
                std::fs::rename(&rebuilt, path).map_err(|e| {
                    anyhow::anyhow!(
                        "putting the new database {} in place: {e}",
                        path.display()
                    )
                })?;
                conn = Connection::open(path)?;
            } else {
                tracing::warn!(
                    "the database {} was written by schema v{version}: it is replaced by an empty v{} one — \
                     the analysis data and the site registrations are not carried over; \
                     the old file is kept as {}",
                    path.display(),
                    SCHEMA_VERSION,
                    backup.display()
                );
                // Fold the write-ahead log into the file first, so the copy that is
                // kept aside really holds everything the old database had.
                conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
                drop(conn);
                std::fs::rename(path, &backup).map_err(|e| {
                    anyhow::anyhow!(
                        "setting the old database {} aside as {}: {e}",
                        path.display(),
                        backup.display()
                    )
                })?;
                // The log and the index belong to the old file.
                remove_side_files(path);
                conn = Connection::open(path)?;
            }
        }

        let mut backend = Self {
            writer: Mutex::new(conn),
            readers: Some(Pool::new(4)),
            path: path.to_path_buf(),
            layout: Layout::PerSite,
            sites: Mutex::new(HashSet::new()),
            event_ids: crate::event_id::Generator::new(0),
            visit_ids: crate::event_id::Generator::with_version(
                crate::event_id::VISIT_VERSION,
                0,
            ),
        };

        // The main file holds the sites and the bookkeeping; the facts live in a
        // file per site.
        backend.prepare_main()?;
        // Every site's file is attached — and created, if it is not there yet.
        backend.attach_all_sites()?;

        // The instance field of the ids, and the seed they start from.
        let instance = {
            let conn = backend.writer.lock().unwrap_or_else(|e| e.into_inner());
            event_id_instance(&conn)
        };
        let counter = backend.largest_counter()?;
        backend.event_ids = crate::event_id::Generator::with_counter(instance, counter);
        let visits = backend.largest_visit_counter()?;
        backend.visit_ids = crate::event_id::Generator::with_version_counter(
            crate::event_id::VISIT_VERSION,
            instance,
            visits,
        );

        Ok(backend)
    }

    /// Open an in-memory database (tests).
    ///
    /// **The flat shape.** An in-memory database has no file to hang a site's
    /// file on, so the facts stay in the database at hand — the same shape the
    /// archive and the tools that measure things read.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let mut ddl = String::new();
        for (table, columns) in MAIN_TABLES.iter().chain(SITE_TABLES.iter()) {
            ddl.push_str(&crate::schema::table_ddl(table, columns, &Sqlite));
            ddl.push('\n');
        }
        ddl.push_str(&index_ddl("main"));
        conn.execute_batch(&ddl)?;
        write_schema_version(&conn)?;

        Ok(Self {
            writer: Mutex::new(conn),
            readers: None,
            path: PathBuf::from(":memory:"),
            layout: Layout::Flat,
            sites: Mutex::new(HashSet::new()),
            event_ids: crate::event_id::Generator::new(0),
            visit_ids: crate::event_id::Generator::with_version(
                crate::event_id::VISIT_VERSION,
                0,
            ),
        })
    }

    /// The largest counter in use — in the main file or in any site's file.
    ///
    /// It is the **seed** of the generator, not the counter to hand out next:
    /// the generator continues from it, and the counter it hands out is
    /// always above this one.
    ///
    /// **Only the rows of this instance count** — the ids of another instance
    /// (rows that came with a site that moved here) carry that instance's
    /// half, and their counters are not ours to continue.
    fn largest_counter(&self) -> Result<u64> {
        let conn = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let instance = self.event_ids.instance();
        let low = crate::event_id::compose(instance, 0);
        let high = crate::event_id::compose(instance, crate::event_id::COUNTER_MAX);
        let largest_of = |table: &str| -> u64 {
            let id: Option<Vec<u8>> = conn
                .query_row(
                    &format!("SELECT MAX(event_id) FROM {table} WHERE event_id BETWEEN ?1 AND ?2"),
                    params![low.as_bytes().as_slice(), high.as_bytes().as_slice()],
                    |row| row.get(0),
                )
                .unwrap_or(None);
            id.and_then(|bytes| uuid::Uuid::from_slice(&bytes).ok())
                .map(|id| crate::event_id::counter_of(&id))
                .unwrap_or(0)
        };
        let mut largest = largest_of("website_event");
        if self.layout == Layout::PerSite {
            for site in self.sites.lock().unwrap_or_else(|e| e.into_inner()).iter() {
                let table = event_table(self.layout, site);
                largest = largest.max(largest_of(&table));
            }
        }
        Ok(largest)
    }

    /// The largest visit counter in use — in the main file or in any site's file.
    ///
    /// The visit ids are text here, so the range is not a range of bytes: it is
    /// the **prefix** that says "this instance's visit ids" — the instance's
    /// bits and the version nibble, and nothing of the counter. The biggest of
    /// those rows is the biggest counter, because the counter is the last group
    /// of the text and the groups are the same width.
    fn largest_visit_counter(&self) -> Result<u64> {
        let conn = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let instance = self.event_ids.instance();
        let prefix: String = crate::event_id::compose_version(
            crate::event_id::VISIT_VERSION,
            instance,
            0,
        )
        .as_hyphenated()
        .to_string()
        .chars()
        .take(23)
        .collect();
        let largest_of = |table: &str| -> u64 {
            let id: Option<String> = conn
                .query_row(
                    &format!("SELECT MAX(visit_id) FROM {table} WHERE visit_id LIKE ?1"),
                    params![format!("{prefix}%")],
                    |row| row.get(0),
                )
                .unwrap_or(None);
            id.and_then(|text| {
                let tail = text.rsplit('-').next()?;
                u64::from_str_radix(tail, 16).ok()
            })
            .unwrap_or(0)
        };
        let mut largest = largest_of("website_event");
        if self.layout == Layout::PerSite {
            for site in self.sites.lock().unwrap_or_else(|e| e.into_inner()).iter() {
                let table = event_table(self.layout, site);
                largest = largest.max(largest_of(&table));
            }
        }
        Ok(largest)
    }

    /// The pragmas of a connection.
    ///
    /// WAL lets readers work while the writer is busy — the opposite of DuckDB,
    /// which takes the exclusive lock on the file.
    fn prepare(conn: &Connection) -> Result<()> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "busy_timeout", 5_000)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        Ok(())
    }

    /// The pragmas and the **main** file's schema.
    fn prepare_main(&self) -> Result<()> {
        let conn = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        Self::prepare(&conn)?;

        // The schema is **the shared list** (see [`crate::schema`]) in SQLite's
        // dialect — one row per page view (or custom event), the field set
        // mirroring Umami's `website_event` row for row.
        let mut ddl = String::new();
        for (table, columns) in MAIN_TABLES {
            ddl.push_str(&crate::schema::table_ddl(table, columns, &Sqlite));
            ddl.push('\n');
        }
        conn.execute_batch(&ddl)?;
        write_schema_version(&conn)?;
        Ok(())
    }

    /// How the fact tables are reached.
    pub fn layout(&self) -> Layout {
        self.layout
    }

    /// The dialect of the given shape.
    fn dialect(&self) -> SqliteDialect {
        SqliteDialect::new(self.layout)
    }

    /// Attach every site's file — and create it, if it is not there yet.
    ///
    /// ⇒ ⇒ The operator sees **one file per site** beside the main file, which
    /// is the shape the decision (`D-046`) is about.
    fn attach_all_sites(&self) -> Result<()> {
        if self.layout != Layout::PerSite {
            return Ok(());
        }
        let ids = {
            let conn = self.writer.lock().unwrap_or_else(|e| e.into_inner());
            site_ids(&conn)?
        };
        for id in ids {
            self.attach_site(&id)?;
        }
        Ok(())
    }

    /// Make sure the site's file is attached to the writer, and return its alias.
    ///
    /// The file is created if it is not there — a site that has never been
    /// written to still gets a file of its own.
    fn attach_site(&self, website_id: &str) -> Result<String> {
        let alias = crate::db::site_alias(website_id);
        if self.layout != Layout::PerSite {
            return Ok(alias);
        }
        {
            let sites = self.sites.lock().unwrap_or_else(|e| e.into_inner());
            if sites.contains(website_id) {
                return Ok(alias);
            }
        }

        let file = site_file(&self.path, website_id)?;

        {
            let conn = self.writer.lock().unwrap_or_else(|e| e.into_inner());
            attach_site(&conn, &file, &alias)?;
            init_site_schema(&conn, &alias)?;
        }

        let mut sites = self.sites.lock().unwrap_or_else(|e| e.into_inner());
        sites.insert(website_id.to_string());
        Ok(alias)
    }

    /// Attach the site's file to a connection that does not have it yet.
    ///
    /// **Every connection has to do this for itself** — an attachment belongs to
    /// the connection that made it, so a reader that was opened before the site
    /// existed cannot see the site's file.
    fn ensure_attached(&self, conn: &Connection, website_id: &str) -> Result<()> {
        if self.layout != Layout::PerSite {
            return Ok(());
        }
        let alias = crate::db::site_alias(website_id);
        if is_attached(conn, &alias)? {
            return Ok(());
        }
        self.attach_to(conn, website_id)
    }

    /// Take a site's file away: the site is gone, and so is its file.
    fn drop_site(&self, website_id: &str) -> Result<()> {
        if self.layout != Layout::PerSite {
            return Ok(());
        }
        let alias = crate::db::site_alias(website_id);
        {
            let conn = self.writer.lock().unwrap_or_else(|e| e.into_inner());
            let _ = conn.execute_batch(&format!("DETACH {alias}"));
        }
        self.sites
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(website_id);
        let _ = std::fs::remove_file(crate::db::site_path(&self.path, website_id, "sqlite"));
        Ok(())
    }

    /// Run `f` with the writer's connection.
    pub fn with_connection<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        let mut conn = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut conn)
    }

    /// Run `f` with the writer's connection.
    fn with<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        self.with_connection(f)
    }

    /// Move the write-ahead log into the database file.
    ///
    /// SQLite keeps the recent writes in a separate file (`…-wal`) until it is
    /// checkpointed. That is what makes it fast, but it means **the file on disk
    /// is not the whole database** — a copy of it can be missing the newest rows.
    /// Checkpointing is what a backup does before it copies the file.
    pub fn checkpoint(&self) -> Result<()> {
        self.with_connection(|conn| {
            conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
            Ok(())
        })
    }

    /// Run `f` with a connection from the reader pool.
    ///
    /// This is what the dashboard uses: the request takes a connection, reads and
    /// gives it back. Two requests do not wait for each other.
    ///
    /// **Every site's file is attached to it** — the attachment belongs to the
    /// connection, not to the database, so a connection that hands out a site's
    /// rows has to have that site's file attached to it.
    pub fn with_reader<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        self.with_reader_for("", f)
    }

    /// Run `f` with a connection from the reader pool, with the site's file
    /// attached to it.
    ///
    /// **The attachment belongs to the connection, not to the database** — so a
    /// reader that was opened before the site existed cannot see the site's
    /// file, and it has to be attached again. The check is cheap (a row in
    /// `pragma_database_list`), and it happens on every read because a report
    /// may ask about a site that was created a moment ago.
    fn with_reader_for<T>(
        &self,
        website_id: &str,
        f: impl FnOnce(&Connection) -> Result<T>,
    ) -> Result<T> {
        let Some(readers) = &self.readers else {
            // No pool: an in-memory database has exactly one connection.
            let conn = self.writer.lock().unwrap_or_else(|e| e.into_inner());
            return f(&conn);
        };
        // A SQLite reader gets a fresh snapshot on every read, so a pooled
        // connection is never stale.
        let connection = readers.take(|| self.open_reader())?;
        let pooled = Pooled {
            pool: readers,
            connection: Some(connection),
        };

        // Every site's file is attached — a pooled connection may have been
        // opened before a site existed.
        //
        // The set is the backend's own: it holds the sites that have a file,
        // whether or not they are registered — a site that got rows before it
        // was registered has one too.
        let ids: Vec<String> = {
            let writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
            let mut ids = site_ids(&writer)?;
            for id in self.sites.lock().unwrap_or_else(|e| e.into_inner()).iter() {
                if !ids.contains(id) {
                    ids.push(id.clone());
                }
            }
            ids
        };

        // **What the connection already has is read once** — the list is a
        // query, and a query per site per read would be a query per site per
        // read on a busy instance.
        let mut attached = attached_aliases(pooled.get())?;
        for id in &ids {
            let alias = crate::db::site_alias(id);
            if attached.contains(&alias) {
                continue;
            }
            self.attach_to(pooled.get(), id)?;
            attached.push(alias);
        }
        // And the site the read is about, if it is not registered yet.
        if !website_id.is_empty() {
            let alias = crate::db::site_alias(website_id);
            if !attached.contains(&alias) {
                self.attach_to(pooled.get(), website_id)?;
            }
        }

        f(pooled.get())
    }

    /// Attach the site's file to a connection, and make sure its tables are
    /// there.
    fn attach_to(&self, conn: &Connection, website_id: &str) -> Result<()> {
        if self.layout != Layout::PerSite {
            return Ok(());
        }
        let alias = crate::db::site_alias(website_id);
        let file = site_file(&self.path, website_id)?;
        attach_site(conn, &file, &alias)?;
        init_site_schema(conn, &alias)?;
        Ok(())
    }

    /// Open one more connection to the same file.
    fn open_reader(&self) -> Result<Connection> {
        let conn = Connection::open(&self.path)?;
        Self::prepare(&conn)?;
        // A new connection has to attach the sites' files for itself — the
        // attachment belongs to the connection, not to the database.
        if self.layout == Layout::PerSite {
            let ids = {
                let writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
                site_ids(&writer)?
            };
            for id in ids {
                self.ensure_attached(&conn, &id)?;
            }
        }
        Ok(conn)
    }
}

/// Epoch seconds of a timestamp.
/// A timestamp, in **microseconds** since the epoch.
///
/// **Microseconds, not seconds.** The seconds lost the fraction of a second,
/// and with it two things that have to be the same on both engines: the
/// timestamp the API serves (DuckDB keeps the microseconds) and the boundary
/// of a window — an event in the same second as the window's end fell
/// outside it here and inside it there.
fn ts(value: chrono::NaiveDateTime) -> i64 {
    value.and_utc().timestamp_micros()
}

/// A UUID out of the text the column holds.
///
/// SQLite has no UUID type, so the ids are stored as their text — the same
/// text they are shown in.
/// Schema version of the SQLite database.
///
/// * v3 — the event-row model, the ids as text.
/// * v4 — the session and visitor ids are **UUIDs**: 16 bytes, not a string.
/// * v5 — the traffic channel is stored with the event.
/// * v6 — the visit id, the referrer's display name and the coordinates.
/// * v7 — the timestamps are **microseconds**, not seconds.
/// * v8 — the **session id is eight bytes**, not sixteen: the segment's start is
///   in the high half, so a report anchors a session's duration on the moment
///   it really started without a second column.
/// * v9 — the **visitor id is not stored**. The address (the raw one and its
///   coarse prefix) and the raw user agent are stored instead, and a visitor
///   is the pair (the prefix, the user agent).
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
const SCHEMA_VERSION: i64 = 13;

/// The schema version the database was written with (`0` for a fresh file).
fn read_schema_version(conn: &Connection) -> Result<i64> {
    // A fresh file has no `meta` table yet — that is not an error, it is a
    // database with nothing in it.
    let has_meta: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'meta'",
        [],
        |row| row.get(0),
    )?;
    if has_meta == 0 {
        return Ok(0);
    }
    let version: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    Ok(version.and_then(|v| v.parse().ok()).unwrap_or(0))
}

/// Write the schema version into the database.
///
/// There is **no migration** — except the one to v12, which carries the rows
/// over (see [`SqliteBackend::open`]). So this only says which version the file
/// was written with.
fn write_schema_version(conn: &Connection) -> Result<()> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('schema_version', ?1) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![SCHEMA_VERSION.to_string()],
    )?;
    Ok(())
}

/// The instance field of the event ids.
///
/// The same rule as the DuckDB backend's (`pool.rs`): the environment wins, then
/// the instance's own bookkeeping, and an instance that has neither draws one
/// from the operating system and records it.
fn event_id_instance(conn: &Connection) -> u128 {
    if let Some(half) = crate::config::event_id_instance_override() {
        remember_event_id_instance(conn, half);
        return half;
    }
    if let Some(half) = read_event_id_instance(conn) {
        return half;
    }
    let used = used_event_id_instances(conn);
    let half = crate::event_id::pick_instance(&used);
    remember_event_id_instance(conn, half);
    half
}

/// Record the instance field, and say so when it cannot be recorded.
///
/// The ids are still unique without the record — the field is in them — but
/// the record is what keeps the **next** start on the same field, and what
/// tells an import that the field is taken.
fn remember_event_id_instance(conn: &Connection, half: u128) {
    if let Err(error) = remember_event_id_instance_(conn, half) {
        tracing::warn!(
            %error,
            half = crate::event_id::instance_hex(half),
            "the instance half of the event ids could not be recorded — the next start will draw another one"
        );
    }
}

/// The instance field of the event ids, as the instance's bookkeeping has it.
fn read_event_id_instance(conn: &Connection) -> Option<u128> {
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            params![crate::db::EVENT_ID_INSTANCE],
            |row| row.get(0),
        )
        .ok()
        .flatten();
    value.and_then(|value| crate::event_id::instance_from_hex(value.trim()))
}

/// The instance fields this instance has used.
fn used_event_id_instances(conn: &Connection) -> Vec<u128> {
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            params![crate::db::EVENT_ID_INSTANCES_USED],
            |row| row.get(0),
        )
        .ok()
        .flatten();
    value
        .unwrap_or_default()
        .split(',')
        .filter_map(|part| crate::event_id::instance_from_hex(part.trim()))
        .collect()
}

/// Record an instance field as used — the current one, and every one before it.
fn remember_event_id_instance_(conn: &Connection, instance: u128) -> Result<()> {
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
        "INSERT INTO meta (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![crate::db::EVENT_ID_INSTANCES_USED, list],
    )?;
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![crate::db::EVENT_ID_INSTANCE, crate::event_id::instance_hex(instance)],
    )?;
    Ok(())
}

/// The ids of the sites in the main file.
fn site_ids(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT id FROM website ORDER BY created_at ASC")?;
    let mut rows = stmt.query([])?;
    let mut ids = Vec::new();
    while let Some(row) = rows.next()? {
        ids.push(row.get(0)?);
    }
    Ok(ids)
}

/// Attach a site's file to a connection, under the given alias.
///
/// The alias is a valid identifier (see [`crate::db::site_alias`]), so the
/// statements can say `s_….website_event` and mean **that site's** rows.
fn attach_site(conn: &Connection, file: &Path, alias: &str) -> Result<()> {
    let escaped = file.to_string_lossy().replace('\'', "''");
    conn.execute_batch(&format!("ATTACH DATABASE '{escaped}' AS {alias}"))?;
    Ok(())
}

/// The file of a site — and the directory that holds it.
///
/// ⇒ ⇒ The directory is made here, because a site that is registered but was
/// never written to still gets a file of its own, and SQLite cannot open a
/// file whose directory is not there.
fn site_file(path: &Path, website_id: &str) -> Result<PathBuf> {
    let file = crate::db::site_path(path, website_id, "sqlite");
    if let Some(parent) = file.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| {
                anyhow::anyhow!("creating the sites directory {}: {e}", parent.display())
            })?;
        }
    }
    Ok(file)
}

/// Is this database already attached under this alias?
fn is_attached(conn: &Connection, alias: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM pragma_database_list WHERE name = ?1",
        params![alias],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// The aliases this connection has attached.
///
/// ⇒ ⇒ The attachment belongs to the connection, so the list is the
/// connection's own — a reader that was opened before a site existed does not
/// have that site's file.
fn attached_aliases(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT name FROM pragma_database_list")?;
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(row.get(0)?);
    }
    Ok(out)
}

/// Create a **site's** schema in the database attached under `alias`.
///
/// **The index is the odd one out.** SQLite writes a table's name with the
/// alias in front (`s_….website_event`), but an index's **table** may not be
/// qualified — it is a syntax error. The index's **own** name may be: naming
/// it `s_….website_event_website_created` is what tells SQLite which database
/// the table is in, and where the index is to live.
fn init_site_schema(conn: &Connection, alias: &str) -> Result<()> {
    let mut ddl = String::new();
    for (table, columns) in SITE_TABLES {
        ddl.push_str(&crate::schema::table_ddl(
            &format!("{alias}.{table}"),
            columns,
            &Sqlite,
        ));
        ddl.push('\n');
    }
    ddl.push_str(&index_ddl(alias));
    conn.execute_batch(&ddl)?;
    Ok(())
}

/// Take away the log and the index that belong to a file.
///
/// They are the old file's own — a `-wal` left behind would be replayed into
/// whatever took the file's place.
fn remove_side_files(path: &Path) {
    for suffix in ["-wal", "-shm"] {
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(suffix);
        let _ = std::fs::remove_file(path.with_file_name(name));
    }
}

/// Where the new main file is written, before it is swapped in.
fn new_main_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "sumimi.sqlite".to_string());
    name.push_str(".v12.new");
    path.with_file_name(name)
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
fn split_into_sites(path: &Path, conn: &Connection, instance: u128) -> Result<PathBuf> {
    let ids = site_ids(conn)?;

    // **The new ids are made first, for the whole table** — the counters have
    // to be unique across the whole instance, not per site.
    if table_exists(conn, "event") {
        conn.execute_batch(&format!(
            "CREATE TEMP TABLE id_map AS \
             SELECT id AS old_id, unhex('{half}' || substr('000000000000' || hex(n), -12)) AS new_id FROM (\
                 SELECT id, row_number() OVER () - 1 AS n FROM main.event\
             )",
            half = crate::event_id::instance_hex(instance),
        ))?;
    }

    // Every site's rows move into a file of its own.
    for id in &ids {
        let alias = crate::db::site_alias(id);
        let file = site_file(path, id)?;
        attach_site(conn, &file, &alias)?;
        init_site_schema(conn, &alias)?;
        copy_site_rows(conn, &alias, id)?;
    }

    // The new main file: the sites and the bookkeeping, and nothing else.
    let new_path = new_main_path(path);
    let _ = std::fs::remove_file(&new_path);
    {
        let fresh = Connection::open(&new_path).map_err(|e| {
            anyhow::anyhow!("creating the new database {}: {e}", new_path.display())
        })?;
        let mut ddl = String::new();
        for (table, columns) in MAIN_TABLES {
            ddl.push_str(&crate::schema::table_ddl(table, columns, &Sqlite));
            ddl.push('\n');
        }
        fresh.execute_batch(&ddl)?;

        // The registrations are read out of the **old** file — the new one is
        // empty, and it is the new one that ends up in place.
        let mut stmt = conn.prepare("SELECT id, name, domain, created_at, updated_at, retention_days, country_header FROM website")?;
        let mut rows = stmt.query([])?;
        let mut insert = fresh.prepare(
            "INSERT INTO website (id, name, domain, created_at, updated_at, retention_days, country_header)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        while let Some(row) = rows.next()? {
            insert.execute(params![
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, Option<String>>(6)?,
            ])?;
        }
        drop(insert);
        drop(rows);
        drop(stmt);

        write_schema_version(&fresh)?;

        // **The half the carried-over rows got is the instance's half.**
        fresh.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![
                crate::db::EVENT_ID_INSTANCE,
                crate::event_id::instance_hex(instance)
            ],
        )?;
    }

    Ok(new_path)
}

/// Copy one site's rows into its own file.
///
/// The old file's table is `event`; the new one is `website_event`, and five
/// of the columns were renamed on the way ⇒ ⇒ the copy names the old column
/// on the right-hand side and the new one on the left-hand side.
fn copy_site_rows(conn: &Connection, alias: &str, website_id: &str) -> Result<()> {
    if table_exists(conn, "event") {
        // **The ids are new** — the map was built over the whole table.
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
                 JOIN id_map m ON e.id = m.old_id WHERE e.website_id = ?1",
                new = new_names.join(", "),
                old = columns.join(", "),
            ),
            params![website_id],
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
                            CASE WHEN json_type(j.value) = 'text' THEN j.value END, \
                            CASE WHEN json_type(j.value) IN ('real', 'integer') \
                                 THEN CAST(j.value AS REAL) END, \
                            NULL, \
                            CASE WHEN json_type(j.value) IN ('real', 'integer') \
                                 THEN 'number' ELSE 'string' END \
                     FROM main.event e \
                     JOIN id_map m ON e.id = m.old_id, \
                          json_each(e.data) j \
                     WHERE e.website_id = ?1 AND e.data IS NOT NULL AND json_valid(e.data)",
                ),
                params![website_id],
            )?;
        }
    }

    if table_exists(conn, "revenue") {
        // The revenue's own id is the engine's row id (an integer); the
        // **reference** to the event is the new uuid.
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
                 LEFT JOIN id_map m ON r.event_id = m.old_id WHERE r.website_id = ?1",
                new = names.join(", "),
                old = columns.join(", "),
            ),
            params![website_id],
        )?;
    }

    Ok(())
}

/// Does the database have this table?
fn table_exists(conn: &Connection, table: &str) -> bool {
    conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        params![table],
        |row| row.get::<_, i64>(0),
    )
    .map(|n| n > 0)
    .unwrap_or(false)
}

/// Does the table have the column?
///
/// **The old file's shape is not known in advance** — a column that v0.6
/// added (`ver`) is not in a file written before it, and a column it removed
/// (`data`) is only in one written before it ⇒ ⇒ the migration asks.
fn column_exists(conn: &Connection, table: &str, column: &str) -> bool {
    conn.query_row(
        "SELECT count(*) FROM pragma_table_info(?1) WHERE name = ?2",
        params![table, column],
        |row| row.get::<_, i64>(0),
    )
    .map(|n| n > 0)
    .unwrap_or(false)
}

/// A timestamp from epoch seconds.
fn from_ts(value: i64) -> chrono::NaiveDateTime {
    chrono::DateTime::from_timestamp_micros(value)
        .map(|d| d.naive_utc())
        .unwrap_or_default()
}

/// Now, as a naive UTC timestamp.
fn crate_now() -> chrono::NaiveDateTime {
    chrono::Utc::now().naive_utc()
}

impl Storage for SqliteBackend {
    fn backend(&self) -> &'static str {
        "sqlite"
    }

    fn websites(&self) -> Result<Vec<Website>> {
        self.with_reader(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, name, domain, created_at, updated_at, retention_days, country_header
                 FROM website ORDER BY created_at DESC",
            )?;
            let mut rows = stmt.query([])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                out.push(Website {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    domain: row.get(2)?,
                    created_at: from_ts(row.get(3)?),
                    updated_at: from_ts(row.get(4)?),
                    retention_days: row.get(5)?,
                    country_header: row.get(6)?,
                });
            }
            Ok(out)
        })
    }

    fn website(&self, id: &str) -> Result<Option<Website>> {
        self.with_reader(|conn| {
            Ok(conn
                .query_row(
                    "SELECT id, name, domain, created_at, updated_at, retention_days, country_header
                     FROM website WHERE id = ?1",
                    params![id],
                    |row| {
                        Ok(Website {
                            id: row.get(0)?,
                            name: row.get(1)?,
                            domain: row.get(2)?,
                            created_at: from_ts(row.get(3)?),
                            updated_at: from_ts(row.get(4)?),
                            retention_days: row.get(5)?,
                            country_header: row.get(6)?,
                        })
                    },
                )
                .optional()?)
        })
    }

    fn create_website(
        &self,
        name: &str,
        domain: Option<&str>,
        retention_days: i32,
        country_header: Option<&str>,
    ) -> Result<Website> {
        let now = crate_now();
        let website = Website {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.to_string(),
            domain: domain.map(|d| d.to_string()),
            created_at: now,
            updated_at: now,
            retention_days,
            country_header: country_header.map(|c| c.to_string()),
        };
        self.with(|conn| {
            conn.execute(
                "INSERT INTO website (id, name, domain, created_at, updated_at, retention_days, country_header)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    website.id,
                    website.name,
                    website.domain,
                    ts(website.created_at),
                    ts(website.updated_at),
                    website.retention_days,
                    website.country_header
                ],
            )?;
            Ok(())
        })?;
        // The site gets a file of its own — attached, and created if it is not
        // there yet, so the operator sees one file per site.
        self.attach_site(&website.id)?;
        Ok(website)
    }

    fn update_website(&self, id: &str, patch: &WebsitePatch) -> Result<Website> {
        self.with(|conn| {
            let mut website = conn
                .query_row(
                    "SELECT id, name, domain, created_at, updated_at, retention_days, country_header
                     FROM website WHERE id = ?1",
                    params![id],
                    |row| {
                        Ok(Website {
                            id: row.get(0)?,
                            name: row.get(1)?,
                            domain: row.get(2)?,
                            created_at: from_ts(row.get(3)?),
                            updated_at: from_ts(row.get(4)?),
                            retention_days: row.get(5)?,
                            country_header: row.get(6)?,
                        })
                    },
                )
                .optional()?
                .ok_or_else(|| anyhow::anyhow!("no such website: {id}"))?;

            if let Some(name) = patch.name.as_ref() {
                website.name = name.clone();
            }
            if let Some(domain) = patch.domain.as_ref() {
                // The domain is trimmed and lowercased, exactly like the other
                // backends: a domain is case insensitive, and the tracker
                // matches it case insensitively.
                let value = domain.trim().to_lowercase();
                website.domain = if value.is_empty() { None } else { Some(value) };
            }
            if let Some(retention_days) = patch.retention_days {
                website.retention_days = retention_days;
            }
            if let Some(country_header) = patch.country_header.as_ref() {
                website.country_header = Some(country_header.clone());
            }
            website.updated_at = crate_now();

            conn.execute(
                "UPDATE website SET name = ?2, domain = ?3, updated_at = ?4,
                        retention_days = ?5, country_header = ?6
                 WHERE id = ?1",
                params![
                    website.id,
                    website.name,
                    website.domain,
                    ts(website.updated_at),
                    website.retention_days,
                    website.country_header
                ],
            )?;
            Ok(website)
        })
    }

    fn delete_website(&self, id: &str) -> Result<bool> {
        // **A site that is not there is not an error** — it is nothing to do.
        // ⇒ ⇒ The question is asked *before* the facts are touched: they live
        // in the site's own file, and that file is gone with the site ⇒ ⇒ ⇒
        // a second delete would be talking about a file that is not there.
        //
        // The question is asked on the **writer**: the `website` table is in
        // the main file, and the reader would attach every site's file to
        // answer a question about the main one.
        let known = self.with(|conn| {
            let count: i64 = conn.query_row(
                "SELECT count(*) FROM website WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )?;
            Ok(count > 0)
        })?;
        if !known {
            return Ok(false);
        }
        let removed = self.with(|conn| {
            // The event table is the only fact table: the sessions and visitors are
            // derived from it, so deleting the events deletes them too.
            let (events, revenue) = erase_site(conn, id, self.layout())?;
            let _ = (events, revenue);
            let removed = conn.execute("DELETE FROM website WHERE id = ?1", params![id])?;
            Ok(removed > 0)
        })?;
        // The site's file is empty now — take it away.
        self.drop_site(id)?;
        Ok(removed)
    }

    fn summary(&self, query: &ReportQuery) -> Result<SummaryMetrics> {
        self.with_reader_for(&query.website_id, |conn| summary(conn, query, &self.dialect()))
    }

    fn summary_with_change(&self, query: &ReportQuery) -> Result<MetricsSummary> {
        let current = self.summary(query)?;
        let mut previous_query = query.clone();
        previous_query.range = query.range.previous();
        let previous = self.summary(&previous_query)?;

        Ok(MetricsSummary {
            pageviews: metric(current.pageviews, previous.pageviews as f64),
            visitors: metric(current.visitors, previous.visitors as f64),
            views: metric(current.views, previous.views as f64),
            bounces: metric(current.bounces, previous.bounces as f64),
            visit_duration: metric(current.visit_duration as i64, previous.visit_duration),
        })
    }

    fn breakdown(&self, query: &ReportQuery, dimension: Dimension) -> Result<Vec<BreakdownRow>> {
        self.with_reader_for(&query.website_id, |conn| {
            breakdown(conn, query, dimension, &self.dialect())
        })
    }

    fn timeseries(&self, query: &ReportQuery) -> Result<Vec<BreakdownRow>> {
        self.with_reader_for(&query.website_id, |conn| timeseries(conn, query, &self.dialect()))
    }

    fn events(&self, query: &ReportQuery) -> Result<Vec<BreakdownRow>> {
        self.with_reader_for(&query.website_id, |conn| events(conn, query, &self.dialect()))
    }

    fn query_strings(&self, query: &ReportQuery) -> Result<Vec<(String, i64)>> {
        self.with_reader_for(&query.website_id, |conn| {
            query_strings(conn, query, &self.dialect())
        })
    }

    fn event_log(&self, query: &ReportQuery) -> Result<Vec<EventRow>> {
        self.with_reader_for(&query.website_id, |conn| event_log(conn, query, &self.dialect()))
    }

    fn sessions(&self, query: &ReportQuery) -> Result<Vec<SessionRow>> {
        let events = self.recent_events(query, session_scan_limit(query.limit))?;
        Ok(group_sessions(
            &query.website_id,
            &events,
            query.limit.clamp(1, 1000),
            query.offset,
        ))
    }

    fn recent_events(&self, query: &ReportQuery, limit: i64) -> Result<Vec<SessionEvent>> {
        self.with_reader_for(&query.website_id, |conn| {
            recent_events(conn, query, limit, &self.dialect())
        })
    }

    fn next_visit_id(&self) -> uuid::Uuid {
        self.visit_ids.next()
    }

    fn write_batch(&self, hits: &[Hit]) -> Result<()> {
        let now = crate_now();
        let kept: Vec<&Hit> = hits
            .iter()
            .filter(|hit| {
                if hit.retention_days > 0 {
                    let cutoff = now - chrono::Duration::days(hit.retention_days as i64);
                    hit.created_at >= cutoff
                } else {
                    true
                }
            })
            .collect();

        // Every site in the batch gets its file attached before the rows go
        // into it.
        for hit in &kept {
            self.attach_site(&hit.website_id)?;
        }

        self.with(|conn| {
            let tx = conn.transaction()?;

            // The hits, grouped by the site they belong to — in the order the
            // sites came in.
            let mut by_site: Vec<(String, Vec<&Hit>)> = Vec::new();
            for hit in &kept {
                match by_site.iter_mut().find(|(id, _)| id == &hit.website_id) {
                    Some((_, hits)) => hits.push(hit),
                    None => by_site.push((hit.website_id.clone(), vec![hit])),
                }
            }

            for (website_id, hits) in &by_site {
                let table = event_table(self.layout(), website_id);
                let data_table = event_data_table(self.layout(), website_id);
                // Prepare once: re-preparing per row dominated the write path.
                //
                // **Not `prepare_cached`** — the statement names the site's own
                // table, so every site has a statement of its own and the cache
                // would hold one entry per site and evict the others. It is a
                // cache of sixteen; a busy instance has more sites than that.
                let mut insert = tx.prepare(&format!(
                    r#"
INSERT INTO {table} (
    event_id, website_id, visit_id, session_id, session_start, ip, ip_prefix, user_agent, user_id, created_at,
    url_path, url_query, page_title,
    referrer_domain, referrer_path, referrer_query, referrer_name, hostname, channel,
    browser, browser_version, os, os_version, device, screen, language,
    country, region, city, latitude, longitude,
    utm_source, utm_medium, utm_campaign, utm_content, utm_term,
    gclid, fbclid, msclkid, ttclid, li_fat_id, twclid,
    lcp, inp, cls, fcp, ttfb,
    event_type, event_name, tag, ver
) VALUES (
    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
    ?12, ?13, ?14,
    ?15, ?16, ?17, ?18, ?19, ?20,
    ?21, ?22, ?23, ?24, ?25, ?26, ?27,
    ?28, ?29, ?30, ?31, ?32,
    ?33, ?34, ?35, ?36, ?37, ?38,
    ?39, ?40, ?41, ?42, ?43,
    ?44, ?45, ?46, ?47, ?48, ?49,
    ?50, ?51
)"#
                ))?;
                // The properties go into the table of their own — one row per
                // property, in the order the properties came.
                let mut insert_property = tx.prepare(&format!(
                    r#"
INSERT INTO {data_table} (
    event_id, website_id, session_id, url_path, event_name, created_at,
    data_key, string_value, number_value, date_value, data_type
) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)"#
                ))?;
                for hit in hits {
                    // The id is minted here and used for the event row **and**
                    // for the rows of its properties — they belong to it.
                    let event_id = self.event_ids.next();
                    insert.execute(params![
                        event_id.as_bytes().to_vec(),
                        hit.website_id,
                        hit.visit_id.to_string(),
                        // The visitor's id: the first eight bytes of the hash.
                        crate::ua::visitor_bytes(&hit.visitor_id),
                        hit.session_start.map(ts),
                        hit.ip,
                        hit.ip_prefix,
                        hit.user_agent,
                        hit.user_id.map(|id| id.to_string()),
                        ts(hit.created_at),
                        hit.url,
                        hit.url_query,
                        hit.title,
                        hit.referrer,
                        hit.referrer_path,
                        hit.referrer_query,
                        hit.referrer_name,
                        hit.hostname,
                        hit.channel,
                        hit.browser,
                        hit.browser_version,
                        hit.os,
                        hit.os_version,
                        hit.device,
                        hit.screen,
                        hit.language,
                        hit.country,
                        hit.region,
                        hit.city,
                        hit.latitude,
                        hit.longitude,
                        hit.utm_source,
                        hit.utm_medium,
                        hit.utm_campaign,
                        hit.utm_content,
                        hit.utm_term,
                        hit.gclid,
                        hit.fbclid,
                        hit.msclkid,
                        hit.ttclid,
                        hit.lifatid,
                        hit.twclid,
                        hit.lcp,
                        hit.inp,
                        hit.cls,
                        hit.fcp,
                        hit.ttfb,
                        hit.kind.as_str(),
                        hit.name,
                        hit.tag,
                        hit.ver,
                    ])?;

                    // The properties of the event — one row per property, in
                    // the order the properties came.
                    //
                    // **The rows of one event are written next to each other**
                    // — that is what makes the context columns cheap: they
                    // are the same for every row of the event.
                    if hit.properties.is_empty() {
                        continue;
                    }
                    // The visitor's id: the first eight bytes of the hash.
                    let session = crate::ua::visitor_bytes(&hit.visitor_id);
                    for property in &hit.properties {
                        let (text, number, date): (
                            Option<String>,
                            Option<f64>,
                            Option<chrono::NaiveDateTime>,
                        ) = match &property.value {
                            crate::models::PropertyValue::Text(value) => {
                                (Some(value.clone()), None, None)
                            }
                            crate::models::PropertyValue::Number(value) => {
                                (None, Some(*value), None)
                            }
                            crate::models::PropertyValue::Date(value) => {
                                (None, None, Some(*value))
                            }
                        };
                        let data_type: &'static str = property.value.data_type();
                        insert_property.execute(params![
                            event_id.as_bytes().to_vec(),
                            hit.website_id,
                            session,
                            hit.url,
                            hit.name,
                            ts(hit.created_at),
                            property.key,
                            text,
                            number,
                            // SQLite keeps the moments as microseconds since
                            // the epoch — the same as every other column.
                            date.map(ts),
                            data_type,
                        ])?;
                    }
                }
                drop(insert);
                drop(insert_property);
            }

            tx.commit()?;
            Ok(())
        })
    }

    fn write_revenue(&self, records: &[Revenue]) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }

        // Every site in the batch gets its file attached before the rows go
        // into it.
        for record in records {
            self.attach_site(&record.website_id)?;
        }

        self.with(|conn| {
            let tx = conn.transaction()?;

            // The records, grouped by the site they belong to.
            let mut by_site: Vec<(String, Vec<&Revenue>)> = Vec::new();
            for record in records {
                match by_site.iter_mut().find(|(id, _)| id == &record.website_id) {
                    Some((_, group)) => group.push(record),
                    None => by_site.push((record.website_id.clone(), vec![record])),
                }
            }

            for (website_id, records) in &by_site {
                let table = revenue_table(self.layout(), website_id);
                let mut insert = tx.prepare(&format!(
                    "INSERT INTO {table} (id, website_id, visit_id, event_id, event_name, currency, revenue, created_at)
                     VALUES (NULL, ?1, ?2, ?3, ?4, ?5, ?6, ?7)"
                ))?;
                for record in records {
                    insert.execute(params![
                        record.website_id,
                        record.visit_id.map(|id| id.to_string()),
                        record.event_id,
                        record.event_name,
                        record.currency,
                        record.revenue,
                        ts(record.created_at),
                    ])?;
                }
                drop(insert);
            }

            tx.commit()?;
            Ok(())
        })
    }

    fn revenue(&self, query: &ReportQuery) -> Result<Vec<Revenue>> {
        self.with_reader_for(&query.website_id, |conn| revenue(conn, query, &self.dialect()))
    }

    fn prune(&self, website: Option<&str>) -> Result<(u64, u64)> {
        self.with(|conn| {
            let now = crate_now();
            let mut stmt = conn.prepare(
                "SELECT id, retention_days FROM website WHERE (?1 IS NULL OR id = ?1)",
            )?;
            let rows = stmt
                .query_map(params![website], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            drop(stmt);

            let mut events = 0u64;
            let mut revenue = 0u64;
            for (id, retention_days) in rows {
                if retention_days <= 0 {
                    continue;
                }
                let cutoff = now - chrono::Duration::days(retention_days);
                let (site_events, site_revenue) = delete_before_site(
                    conn,
                    &id,
                    ts(cutoff),
                    self.layout(),
                )?;
                events += site_events;
                revenue += site_revenue;
            }
            Ok((events, revenue))
        })
    }

    fn delete_before(
        &self,
        before: chrono::NaiveDateTime,
        website: Option<&str>,
    ) -> Result<(u64, u64)> {
        self.with(|conn| {
            let mut events = 0u64;
            let mut revenue = 0u64;
            if let Some(website) = website {
                let (site_events, site_revenue) =
                    delete_before_site(conn, website, ts(before), self.layout())?;
                events += site_events;
                revenue += site_revenue;
            } else {
                // Every site: the facts are in the sites' own files, so each one
                // has to be asked in turn.
                for id in site_ids(conn)? {
                    let (site_events, site_revenue) =
                        delete_before_site(conn, &id, ts(before), self.layout())?;
                    events += site_events;
                    revenue += site_revenue;
                }
            }
            Ok((events, revenue))
        })
    }


    fn erase(&self, id: &str) -> Result<u64> {
        // A site that is not there has nothing to erase (see `delete_website`).
        //
        // ⇒ ⇒ The question is asked *before* the facts are touched: they live
        // in the site's own file, and that file is gone with the site ⇒ ⇒ ⇒
        // an erase would be talking about a file that is not there.
        let known = self.with(|conn| {
            let count: i64 = conn.query_row(
                "SELECT count(*) FROM website WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )?;
            Ok(count > 0)
        })?;
        if !known {
            return Ok(0);
        }
        let removed = self.with(|conn| {
            let (events, revenue) = erase_site(conn, id, self.layout())?;
            let websites = conn.execute("DELETE FROM website WHERE id = ?1", params![id])?;
            Ok(revenue + events + websites as u64)
        })?;
        // The site's file is empty now — take it away.
        self.drop_site(id)?;
        Ok(removed)
    }

    fn stats(&self) -> Result<StorageStats> {
        let websites = self.with_reader(|conn| {
            let mut stmt = conn.prepare("SELECT count(*) FROM website")?;
            let mut rows = stmt.query([])?;
            match rows.next()? {
                Some(row) => Ok(row.get::<_, i64>(0)?),
                None => Ok(0),
            }
        })?;

        // The size of the **instance**: the main file and every site's file.
        let size_bytes: i64 = crate::db::instance_files(&self.path, "sqlite")
            .iter()
            .filter_map(|file| std::fs::metadata(file).ok())
            .map(|meta| meta.len() as i64)
            .sum();

        Ok(StorageStats {
            backend: "sqlite".to_string(),
            memory_bytes: 0,
            memory_limit: "0B".to_string(),
            threads: 1,
            size_bytes,
            websites,
        })
    }
}

/// The tables of a site, as the statements name them.
fn tables(layout: Layout, website_id: &str) -> (String, String) {
    (
        event_table(layout, website_id),
        revenue_table(layout, website_id),
    )
}

/// The fact table of a site, as the statements name it.
fn event_table(layout: Layout, website_id: &str) -> String {
    match layout {
        Layout::PerSite => format!("{}.website_event", crate::db::site_alias(website_id)),
        Layout::Flat => "website_event".to_string(),
    }
}

/// The revenue table of a site, as the statements name it.
fn revenue_table(layout: Layout, website_id: &str) -> String {
    match layout {
        Layout::PerSite => format!("{}.revenue", crate::db::site_alias(website_id)),
        Layout::Flat => "revenue".to_string(),
    }
}

/// The property table of a site, as the statements name it.
fn event_data_table(layout: Layout, website_id: &str) -> String {
    match layout {
        Layout::PerSite => format!("{}.event_data", crate::db::site_alias(website_id)),
        Layout::Flat => "event_data".to_string(),
    }
}

/// Delete everything a site stored.
///
/// ⇒ ⇒ The events and the revenue are in the site's own file; the site's row
/// is in the main file and is not touched here.
fn erase_site(conn: &Connection, website_id: &str, layout: Layout) -> Result<(u64, u64)> {
    let (events, revenue) = tables(layout, website_id);
    let events = conn.execute(
        &format!("DELETE FROM {events} WHERE website_id = ?1"),
        params![website_id],
    )? as u64;
    let revenue = conn.execute(
        &format!("DELETE FROM {revenue} WHERE website_id = ?1"),
        params![website_id],
    )? as u64;
    Ok((events, revenue))
}

/// Delete one site's rows that are older than a moment.
fn delete_before_site(
    conn: &Connection,
    website_id: &str,
    before: i64,
    layout: Layout,
) -> Result<(u64, u64)> {
    let (events, revenue) = tables(layout, website_id);
    let events = conn.execute(
        &format!("DELETE FROM {events} WHERE website_id = ?1 AND created_at < ?2"),
        params![website_id, before],
    )? as u64;
    let revenue = conn.execute(
        &format!("DELETE FROM {revenue} WHERE website_id = ?1 AND created_at < ?2"),
        params![website_id, before],
    )? as u64;
    Ok((events, revenue))
}

// --------------------------------------------------------------------------------------
// The reports
// --------------------------------------------------------------------------------------

/// Compute the summary metrics for a window.
fn summary(conn: &Connection, query: &ReportQuery, dialect: &SqliteDialect) -> Result<SummaryMetrics> {
    // The statement is the shared one (see `crate::query`).
    let statement = crate::query::summary(query, dialect);
    let bound = values(&statement);
    let params: Vec<&dyn rusqlite::ToSql> = bound.iter().map(|v| v.as_ref()).collect();
    let mut stmt = conn.prepare(&statement.sql)?;
    let (pageviews, visitors, views) = match stmt.query(params.as_slice())?.next()? {
        Some(row) => (
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
        ),
        None => (0, 0, 0),
    };
    drop(stmt);

    // The bounces and the average session length.
    let statement = crate::query::summary_sessions(query, dialect);
    let bound = values(&statement);
    let params: Vec<&dyn rusqlite::ToSql> = bound.iter().map(|v| v.as_ref()).collect();
    let mut stmt = conn.prepare(&statement.sql)?;
    let (bounces, duration) = match stmt.query(params.as_slice())?.next()? {
        Some(row) => (row.get::<_, i64>(0)?, row.get::<_, f64>(1)?),
        None => (0, 0.0),
    };
    drop(stmt);

    Ok(SummaryMetrics {
        pageviews,
        visitors,
        views,
        bounces,
        visit_duration: duration,
    })
}

/// Break a website down over one dimension.
fn breakdown(
    conn: &Connection,
    query: &ReportQuery,
    dimension: Dimension,
    dialect: &SqliteDialect,
) -> Result<Vec<BreakdownRow>> {
    let statement = crate::query::breakdown(query, dimension, dialect);
    let bound = values(&statement);
    let params: Vec<&dyn rusqlite::ToSql> = bound.iter().map(|v| v.as_ref()).collect();

    let mut stmt = conn.prepare(&statement.sql)?;
    let mut rows = stmt.query(params.as_slice())?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let name: String = row.get(0)?;
        out.push(BreakdownRow {
            url: if dimension == Dimension::Url {
                Some(name.clone())
            } else {
                None
            },
            name,
            value: row.get(1)?,
            date: None,
        });
    }
    Ok(out)
}

/// The query strings of the visited pages, with how often each was seen.
fn query_strings(
    conn: &Connection,
    query: &ReportQuery,
    dialect: &SqliteDialect,
) -> Result<Vec<(String, i64)>> {
    let statement = crate::query::query_strings(query, dialect);
    let bound = values(&statement);
    let params: Vec<&dyn rusqlite::ToSql> = bound.iter().map(|v| v.as_ref()).collect();

    let mut stmt = conn.prepare(&statement.sql)?;
    let mut rows = stmt.query(params.as_slice())?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push((row.get(0)?, row.get(1)?));
    }
    Ok(out)
}

/// The page views over time.
fn timeseries(
    conn: &Connection,
    query: &ReportQuery,
    dialect: &SqliteDialect,
) -> Result<Vec<BreakdownRow>> {
    let statement = crate::query::timeseries(query, dialect);
    let bound = values(&statement);
    let params: Vec<&dyn rusqlite::ToSql> = bound.iter().map(|v| v.as_ref()).collect();

    let mut stmt = conn.prepare(&statement.sql)?;
    let mut rows = stmt.query(params.as_slice())?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let name: String = row.get(0)?;
        out.push(BreakdownRow {
            name: name.clone(),
            value: row.get(1)?,
            url: None,
            date: Some(name),
        });
    }
    Ok(out)
}

/// The custom events, grouped by name.
fn events(
    conn: &Connection,
    query: &ReportQuery,
    dialect: &SqliteDialect,
) -> Result<Vec<BreakdownRow>> {
    let statement = crate::query::events(query, dialect);
    let bound = values(&statement);
    let params: Vec<&dyn rusqlite::ToSql> = bound.iter().map(|v| v.as_ref()).collect();

    let mut stmt = conn.prepare(&statement.sql)?;
    let mut rows = stmt.query(params.as_slice())?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(BreakdownRow {
            name: row.get(0)?,
            value: row.get(1)?,
            url: None,
            date: None,
        });
    }
    Ok(out)
}

/// The raw event log.
fn event_log(
    conn: &Connection,
    query: &ReportQuery,
    dialect: &SqliteDialect,
) -> Result<Vec<EventRow>> {
    let statement = crate::query::event_log(query, dialect);
    let bound = values(&statement);
    let params: Vec<&dyn rusqlite::ToSql> = bound.iter().map(|v| v.as_ref()).collect();

    let mut stmt = conn.prepare(&statement.sql)?;
    let mut rows = stmt.query(params.as_slice())?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(crate::row::event_log_row(&SqliteRow(row))?);
    }
    drop(rows);
    drop(stmt);

    // The properties are rows of the table of their own ⇒ ⇒ they are read
    // for the events of this page and put back into the rows they belong to.
    let properties = read_properties(conn, query, &out, dialect)?;
    for row in &mut out {
        let id = match uuid::Uuid::parse_str(&row.id) {
            Ok(id) => id,
            Err(_) => continue,
        };
        if let Some(list) = properties.get(&id) {
            row.data = crate::row::properties_json(list);
        }
    }
    Ok(out)
}

/// The properties of the events of a page of the log.
///
/// **One query for the whole page** — the log is at most a thousand rows, and
/// a query per event would be a thousand round trips.
fn read_properties(
    conn: &Connection,
    query: &ReportQuery,
    rows: &[EventRow],
    dialect: &SqliteDialect,
) -> Result<HashMap<uuid::Uuid, Vec<crate::models::Property>>> {
    let ids: Vec<uuid::Uuid> = rows
        .iter()
        .filter_map(|row| uuid::Uuid::parse_str(&row.id).ok())
        .collect();
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let statement = crate::query::event_properties(&query.website_id, &ids, dialect);
    let bound = values(&statement);
    let params: Vec<&dyn rusqlite::ToSql> = bound.iter().map(|v| v.as_ref()).collect();

    let mut stmt = conn.prepare(&statement.sql)?;
    let mut rows = stmt.query(params.as_slice())?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(crate::row::property_row(&SqliteRow(row))?);
    }
    Ok(crate::row::group_properties(out))
}

/// The most recent events of a window, newest first.
///
/// The session log is built from these: the log reads the last few thousand
/// rows, not the whole window, so its cost does not grow with the size of the
/// database.
fn recent_events(
    conn: &Connection,
    query: &ReportQuery,
    limit: i64,
    dialect: &SqliteDialect,
) -> Result<Vec<SessionEvent>> {
    let statement = crate::query::recent_events(query, limit, dialect);
    let bound = values(&statement);
    let params: Vec<&dyn rusqlite::ToSql> = bound.iter().map(|v| v.as_ref()).collect();

    let mut stmt = conn.prepare(&statement.sql)?;
    let mut rows = stmt.query(params.as_slice())?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(crate::row::session_event_row(&SqliteRow(row))?);
    }
    Ok(out)
}

/// The revenue of a website over a window.
fn revenue(
    conn: &Connection,
    query: &ReportQuery,
    dialect: &SqliteDialect,
) -> Result<Vec<Revenue>> {
    let statement = crate::query::revenue(query, dialect);
    let bound = values(&statement);
    let params: Vec<&dyn rusqlite::ToSql> = bound.iter().map(|v| v.as_ref()).collect();

    let mut stmt = conn.prepare(&statement.sql)?;
    let mut rows = stmt.query(params.as_slice())?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let row = SqliteRow(row);
        out.push(Revenue {
            website_id: crate::row::Row::text(&row, "website_id")?.unwrap_or_default(),
            visit_id: crate::row::Row::uuid(&row, "visit_id")?,
            event_id: crate::row::Row::text(&row, "event_id")?,
            event_name: crate::row::Row::text(&row, "event_name")?,
            currency: crate::row::Row::text(&row, "currency")?.unwrap_or_default(),
            revenue: crate::row::Row::float(&row, "revenue")?.unwrap_or_default(),
            created_at: crate::row::Row::time(&row, "created_at")?.unwrap_or_default(),
        });
    }
    Ok(out)
}

/// A metric with its period-over-period change.
fn metric(value: i64, previous: f64) -> MetricValue {
    let change = if previous == 0.0 {
        if value == 0 {
            0.0
        } else {
            100.0
        }
    } else {
        ((value as f64 - previous) / previous) * 100.0
    };
    MetricValue {
        value,
        change: (change * 10.0).round() / 10.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::testing::HitBuilder;

    /// A temporary directory of the test's own.
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sumimi-sqlite-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The names of the tables a database has, in `main`.
    fn main_tables(path: &Path) -> Vec<String> {
        let conn = Connection::open(path).unwrap();
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap();
        let mut rows = stmt.query([]).unwrap();
        let mut out = Vec::new();
        while let Some(row) = rows.next().unwrap() {
            out.push(row.get(0).unwrap());
        }
        out
    }

    /// **Every site has a file of its own**, and the main file holds only the
    /// sites and the bookkeeping.
    #[test]
    fn every_site_has_a_file_of_its_own() {
        let dir = temp_dir("per-site");
        let path = dir.join("sumimi.sqlite");
        let backend = SqliteBackend::open(&path).unwrap();

        let one = backend
            .create_website("One", Some("one.example"), 0, None)
            .unwrap();
        let two = backend
            .create_website("Two", Some("two.example"), 0, None)
            .unwrap();

        // One file per site, beside the main file.
        let sites = crate::db::sites_dir(&path);
        let files: Vec<PathBuf> = std::fs::read_dir(&sites)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .collect();
        assert_eq!(files.len(), 2, "every site has a file of its own");
        assert!(crate::db::site_path(&path, &one.id, "sqlite").exists());
        assert!(crate::db::site_path(&path, &two.id, "sqlite").exists());

        // The main file holds the sites and the bookkeeping — and nothing else.
        assert_eq!(main_tables(&path), vec!["meta", "website"]);

        // The site's file holds the facts — under Umami's names, and the
        // properties in the table of their own.
        let site_file = crate::db::site_path(&path, &one.id, "sqlite");
        assert_eq!(
            main_tables(&site_file),
            vec!["event_data", "revenue", "website_event"]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The rows go into the site's own file** — the main file does not hold
    /// them.
    #[test]
    fn the_rows_go_into_the_sites_own_file() {
        let dir = temp_dir("rows");
        let path = dir.join("sumimi.sqlite");
        let backend = SqliteBackend::open(&path).unwrap();
        let site = backend
            .create_website("One", Some("one.example"), 0, None)
            .unwrap();

        let hit = HitBuilder::new(&site.id, "s1", "v1")
            .url("/one")
            .title("A page")
            .referrer("news.ycombinator.com")
            .build();
        backend.write_batch(&[hit]).unwrap();

        // The site's file holds the row.
        let site_file = crate::db::site_path(&path, &site.id, "sqlite");
        let conn = Connection::open(&site_file).unwrap();
        let count: i64 = conn
            .query_row("SELECT count(*) FROM website_event", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1, "the row is in the site's own file");

        // The main file holds none.
        let conn = Connection::open(&path).unwrap();
        let has_facts: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'website_event'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(has_facts, 0, "the main file holds no facts");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A batch that spans two sites is written to two files.**
    #[test]
    fn a_batch_that_spans_two_sites_goes_into_two_files() {
        let dir = temp_dir("two-sites");
        let path = dir.join("sumimi.sqlite");
        let backend = SqliteBackend::open(&path).unwrap();
        let one = backend
            .create_website("One", Some("one.example"), 0, None)
            .unwrap();
        let two = backend
            .create_website("Two", Some("two.example"), 0, None)
            .unwrap();

        let hits = vec![
            HitBuilder::new(&one.id, "s1", "v1").url("/one").build(),
            HitBuilder::new(&two.id, "s2", "v2").url("/two").build(),
            HitBuilder::new(&one.id, "s3", "v3").url("/three").build(),
        ];
        backend.write_batch(&hits).unwrap();

        let count = |website_id: &str| -> i64 {
            let file = crate::db::site_path(&path, website_id, "sqlite");
            let conn = Connection::open(&file).unwrap();
            conn.query_row("SELECT count(*) FROM website_event", [], |row| row.get(0))
                .unwrap()
        };
        assert_eq!(count(&one.id), 2);
        assert_eq!(count(&two.id), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **An old file (v11) is split, and the rows are carried over.**
    ///
    /// The facts move into a file per site, the five renamed columns are
    /// renamed on the way, and the old file is kept beside the new one.
    #[test]
    fn an_older_file_is_split_and_the_rows_are_carried_over() {
        let dir = temp_dir("migrate");
        let path = dir.join("sumimi.sqlite");

        // A v11 file: the fact table is `event`, and the columns have their
        // old names — `id`, `url`, `title`, `referrer`, `lifatid`.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                r#"
                CREATE TABLE event (
                    id             INTEGER PRIMARY KEY,
                    website_id     TEXT NOT NULL,
                    visit_id       TEXT NOT NULL,
                    session_id     TEXT NOT NULL,
                    session_start  INTEGER,
                    ip             TEXT NOT NULL DEFAULT '',
                    ip_prefix      TEXT NOT NULL DEFAULT '',
                    user_agent     TEXT NOT NULL DEFAULT '',
                    user_id        TEXT,
                    created_at     INTEGER NOT NULL,
                    url            TEXT,
                    url_query      TEXT,
                    title          TEXT,
                    referrer       TEXT,
                    referrer_path  TEXT,
                    referrer_query TEXT,
                    referrer_name  TEXT,
                    hostname       TEXT,
                    channel        TEXT,
                    browser        TEXT,
                    browser_version TEXT,
                    os             TEXT,
                    os_version     TEXT,
                    device         TEXT,
                    screen         TEXT,
                    language       TEXT,
                    country        TEXT,
                    region         TEXT,
                    city           TEXT,
                    latitude       REAL,
                    longitude      REAL,
                    utm_source     TEXT,
                    utm_medium     TEXT,
                    utm_campaign   TEXT,
                    utm_content    TEXT,
                    utm_term       TEXT,
                    gclid          TEXT,
                    fbclid         TEXT,
                    msclkid        TEXT,
                    ttclid         TEXT,
                    lifatid        TEXT,
                    twclid         TEXT,
                    lcp            REAL,
                    inp            REAL,
                    cls            REAL,
                    fcp            REAL,
                    ttfb           REAL,
                    event_type     TEXT,
                    event_name     TEXT,
                    tag            TEXT,
                    data           TEXT
                );
                CREATE TABLE website (
                    id                TEXT PRIMARY KEY,
                    name              TEXT NOT NULL,
                    domain            TEXT,
                    created_at        INTEGER NOT NULL,
                    updated_at        INTEGER NOT NULL,
                    retention_days    INTEGER NOT NULL DEFAULT 0,
                    country_header    TEXT
                );
                CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
                INSERT INTO meta VALUES ('schema_version', '11');
                INSERT INTO website VALUES ('w1', 'One', 'one.example', 1, 1, 0, NULL);
                INSERT INTO event (id, website_id, visit_id, session_id, created_at, url, title, referrer, referrer_name, event_type)
                VALUES (7, 'w1', 'v1', 's1', 1700000000000000, '/one', 'A page', 'news.ycombinator.com', 'Hacker News', 'pageview');
                "#,
            )
            .unwrap();
        }

        let backend = SqliteBackend::open(&path).unwrap();

        // The old file is kept beside the new one.
        let backup = crate::db::versioned_backup_path(&path, 11);
        assert!(backup.exists(), "the old file must be kept");

        // The new main file holds the sites and the bookkeeping.
        assert_eq!(main_tables(&path), vec!["meta", "website"]);

        // The site's file holds the row — with the new names.
        let site_file = crate::db::site_path(&path, "w1", "sqlite");
        assert!(site_file.exists(), "the site must have a file of its own");
        let conn = Connection::open(&site_file).unwrap();
        let (url, title, referrer, name, count): (String, String, String, String, i64) = conn
            .query_row(
                "SELECT url_path, page_title, referrer_domain, referrer_name, count(*)
                 FROM website_event",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(count, 1, "the row is carried over");
        assert_eq!(url, "/one");
        assert_eq!(title, "A page");
        assert_eq!(referrer, "news.ycombinator.com");
        assert_eq!(name, "Hacker News", "the display name is not the host");

        // The registration is carried over too.
        let websites = backend.websites().unwrap();
        assert_eq!(websites.len(), 1);
        assert_eq!(websites[0].id, "w1");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **An in-memory database is flat** — there is no file to hang a site's
    /// file on.
    #[test]
    fn an_in_memory_database_is_flat() {
        let backend = SqliteBackend::open_in_memory().unwrap();
        assert_eq!(backend.layout(), Layout::Flat);

        let site = backend
            .create_website("One", Some("one.example"), 0, None)
            .unwrap();
        let hit = HitBuilder::new(&site.id, "s1", "v1").url("/one").build();
        backend.write_batch(&[hit]).unwrap();

        // The facts are in the database at hand.
        backend
            .with_connection(|conn| {
                let count: i64 = conn.query_row(
                    "SELECT count(*) FROM website_event",
                    [],
                    |row| row.get(0),
                )?;
                Ok(count)
            })
            .map(|count| assert_eq!(count, 1))
            .unwrap();
    }

    /// **The rows are read back through the reader pool** — and a site that
    /// was created after a reader was opened is still readable.
    ///
    /// ⇒ ⇒ The attachment belongs to the connection, so the reader has to
    /// attach the site's file for itself.
    #[test]
    fn a_reader_sees_a_site_that_was_created_after_it_was_opened() {
        let dir = temp_dir("reader");
        let path = dir.join("sumimi.sqlite");
        let backend = SqliteBackend::open(&path).unwrap();

        // Take a connection out of the pool, so it is open before the site
        // exists.
        let site = backend
            .create_website("One", Some("one.example"), 0, None)
            .unwrap();
        let hit = HitBuilder::new(&site.id, "s1", "v1").url("/one").build();
        backend.write_batch(&[hit]).unwrap();

        // Read it back through the pool — twice, so the second read comes off
        // the **same** connection as the first.
        for _ in 0..2 {
            let count = backend
                .with_reader_for(&site.id, |conn| {
                    let count: i64 = conn.query_row(
                        "SELECT count(*) FROM website_event",
                        [],
                        |row| row.get(0),
                    )?;
                    Ok(count)
                })
                .unwrap();
            assert_eq!(count, 1, "the reader must see the site's file");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **Deleting a site takes its file away.**
    #[test]
    fn deleting_a_site_takes_its_file_away() {
        let dir = temp_dir("delete");
        let path = dir.join("sumimi.sqlite");
        let backend = SqliteBackend::open(&path).unwrap();
        let site = backend
            .create_website("One", Some("one.example"), 0, None)
            .unwrap();
        let hit = HitBuilder::new(&site.id, "s1", "v1").url("/one").build();
        backend.write_batch(&[hit]).unwrap();

        let file = crate::db::site_path(&path, &site.id, "sqlite");
        assert!(file.exists());

        assert!(backend.delete_website(&site.id).unwrap());
        assert!(!file.exists(), "the site's file must be taken away");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
