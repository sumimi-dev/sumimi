//! Database access.
//!
//! DuckDB is a single-writer, embedded database, and one of its behaviours shapes this
//! design completely: **a connection keeps the snapshot it was opened with**. A
//! connection that queried before a write never sees that write.
//!
//! That was measured, not guessed (`examples/snapshot_probe.rs`):
//!
//! | |
//! | --- |
//! | a connection that queried before the write | never sees the write — not even after a checkpoint |
//! | a connection opened after the write | sees it — until the older connection is closed, when it stops |
//! | after everything is closed and reopened | the rows are there (nothing is lost) |
//!
//! So a second connection would show the dashboard **stale numbers**, and the
//! numbers would change under the reader's feet depending on when it happened to
//! look. That is not a thing a dashboard can live with.
//! So the DuckDB backend uses **one connection**: the writer and the dashboard share
//! it, which is what makes the dashboard's numbers fresh and the writes durable.
//! The requests are serialised — that is the price of the engine, and the reason
//! the report cache (the instance store, in SQLite) matters: the dashboard's
//! repeated questions are answered without touching DuckDB at all.
//!
//! The other two engines do not have this problem and do use a pool of readers:
//! SQLite in WAL mode gives every read a fresh snapshot, and PostgreSQL has had
//! proper MVCC for thirty years.
//!
//! DuckDB's own settings (the memory limit, the thread count, the temporary
//! directory) are **global** to the database, so a pool of connections would cost
//! connections, not memory.
//!
//! The CLI opens the file read-only instead (see `cli/src/main.rs`), because a snapshot
//! is exactly what an offline analysis wants.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

use anyhow::Result;
use duckdb::Connection;

/// The settings every connection is opened with.
///
/// They are global in DuckDB, so setting them on one connection sets them for
/// all of them — but they are applied to each new connection anyway, so a
/// connection opened on its own behaves the same.
#[derive(Debug, Clone)]
struct Settings {
    memory_limit: String,
    flush_threshold: String,
    temp_limit: String,
    threads: usize,
}

/// A pool of connections.
///
/// The connections are opened lazily and handed out one at a time. When the
/// pool is empty and the limit is reached, the caller waits for a connection to
/// come back.
///
/// This is what the **SQLite** backend uses: in WAL mode a reader gets a fresh
/// snapshot on every read, so a pooled connection is never stale, and many
/// readers can work next to the one writer.
pub struct Pool<T> {
    /// The connections that are free.
    idle: Mutex<Vec<T>>,
    /// Woken up when a connection comes back.
    signal: Condvar,
    /// How many connections exist in total.
    live: AtomicUsize,
    /// How many there may be.
    max: usize,
}

impl<T> Pool<T> {
    /// A pool of at most `max` connections.
    pub fn new(max: usize) -> Self {
        Self {
            idle: Mutex::new(Vec::new()),
            signal: Condvar::new(),
            live: AtomicUsize::new(0),
            max: max.max(1),
        }
    }

    /// Take a connection, opening a new one if the pool allows it.
    pub fn take(&self, open: impl Fn() -> Result<T>) -> Result<T> {
        let mut idle = self.idle.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(connection) = idle.pop() {
                return Ok(connection);
            }
            if self.live.load(Ordering::SeqCst) < self.max {
                self.live.fetch_add(1, Ordering::SeqCst);
                drop(idle);
                return match open() {
                    Ok(connection) => Ok(connection),
                    Err(err) => {
                        self.live.fetch_sub(1, Ordering::SeqCst);
                        Err(err)
                    }
                };
            }
            // Every connection is busy: wait for one to come back.
            idle = self.signal.wait(idle).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Give a connection back.
    pub fn give(&self, connection: T) {
        self.idle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(connection);
        self.signal.notify_one();
    }
}

/// A connection that goes back into the pool when it is dropped.
///
/// The connection comes back even when the query panics — otherwise a panic
/// would quietly shrink the pool.
pub struct Pooled<'a, T> {
    /// The pool the connection goes back into.
    pub pool: &'a Pool<T>,
    /// The connection itself.
    pub connection: Option<T>,
}

impl<T> Pooled<'_, T> {
    /// The connection that was taken.
    pub fn get(&self) -> &T {
        self.connection.as_ref().expect("the connection is there")
    }
}

impl<T> Drop for Pooled<'_, T> {
    fn drop(&mut self) {
        if let Some(connection) = self.connection.take() {
            self.pool.give(connection);
        }
    }
}

/// The Sumimi database handle.
///
/// One connection, guarded by a mutex: the writer and the dashboard share it.
/// See the module documentation for why DuckDB cannot have more than one.
pub struct Db {
    path: PathBuf,
    memory_limit: String,
    /// The one and only connection.
    conn: Mutex<Connection>,
    /// The id of the next row of the `event` table.
    ///
    /// The id is handed out from memory instead of being asked for
    /// (`SELECT COALESCE(MAX(id), 0) + 1 FROM event`) on every batch. That query
    /// costs **0.24 ms**, which is 18% of the cost of a small batch, and it grows
    /// with the table (0.25 ms at a thousand rows, 0.58 ms at a million).
    ///
    /// There is exactly one writer — the file is locked by this process — so a
    /// counter cannot race. It is seeded from the table when the database is
    /// opened, so the ids stay unique across restarts.
    pub(crate) next_id: AtomicI64,
}

impl Db {
    /// Wrap an existing connection (used by the CLI, which opens the file itself).
    pub fn from_connection(path: &Path, conn: Connection) -> Self {
        let next_id = Self::seed_next_id(&conn);
        Self {
            path: path.to_path_buf(),
            memory_limit: "0B".to_string(),
            conn: Mutex::new(conn),
            next_id: AtomicI64::new(next_id),
        }
    }

    /// Open the database (creating it if needed) and make sure the schema exists.
    pub fn open(path: &Path, memory_limit: &str) -> Result<Self> {
        Self::open_with(path, memory_limit, "32MB", "2GB", 0)
    }

    /// Open the database and apply the full set of DuckDB settings.
    pub fn open_with(
        path: &Path,
        memory_limit: &str,
        flush_threshold: &str,
        temp_limit: &str,
        threads: usize,
    ) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    anyhow::anyhow!("creating database directory {}: {e}", parent.display())
                })?;
            }
        }

        let settings = Settings {
            memory_limit: memory_limit.to_string(),
            flush_threshold: flush_threshold.to_string(),
            temp_limit: temp_limit.to_string(),
            threads,
        };

        let mut conn = Connection::open(path)
            .map_err(|e| anyhow::anyhow!("opening duckdb database {}: {e}", path.display()))?;
        Self::configure(&conn, &settings)?;

        // An older database is **replaced**, not migrated.
        //
        // The shape of the session id changed (sixteen bytes → eight), and the
        // rows of an old file cannot be carried over: the id means something
        // different now, and the site's registrations live in the same file. So
        // the old file is **set aside** under its own version number and an
        // empty one takes its place — the analysis data and the registrations
        // are not carried over, but nothing is destroyed either.
        let version = crate::db::schema_version(&conn);
        if version > 0 && version < crate::db::SCHEMA_VERSION {
            let backup = crate::db::versioned_backup_path(path, version);
            tracing::warn!(
                "the database {} was written by schema v{version}: it is replaced by an empty v{} one — \
                 the analysis data and the site registrations are not carried over; \
                 the old file is kept as {}",
                path.display(),
                crate::db::SCHEMA_VERSION,
                backup.display()
            );
            // The old file is locked by this connection — close it before the swap.
            drop(conn);
            std::fs::rename(path, &backup).map_err(|e| {
                anyhow::anyhow!(
                    "setting the old database {} aside as {}: {e}",
                    path.display(),
                    backup.display()
                )
            })?;
            conn = Connection::open(path).map_err(|e| {
                anyhow::anyhow!("opening the new duckdb database {}: {e}", path.display())
            })?;
            Self::configure(&conn, &settings)?;
        }

        crate::db::init_schema(&conn)?;

        let next_id = Self::seed_next_id(&conn);
        Ok(Self {
            path: path.to_path_buf(),
            memory_limit: memory_limit.to_string(),
            conn: Mutex::new(conn),
            next_id: AtomicI64::new(next_id),
        })
    }

    /// Apply the full set of DuckDB settings.
    fn configure(conn: &Connection, settings: &Settings) -> Result<()> {
        conn.execute_batch(&format!("SET memory_limit='{}'", settings.memory_limit))?;
        // Hand freed memory back to the OS sooner than DuckDB's 128 MB default, so a
        // small box does not stay big after a traffic burst.
        conn.execute_batch(&format!(
            "SET allocator_flush_threshold='{}'",
            settings.flush_threshold
        ))?;
        // A query that spills must not fill the disk.
        conn.execute_batch(&format!(
            "SET max_temp_directory_size='{}'",
            settings.temp_limit
        ))?;
        // A small WAL keeps a crash from losing the last flush, without huge files.
        //
        // Measured (2026-11-09): the threshold does **not** move the peak — over a
        // 60-second run 16 MB and 32 MB both peak at ~235 MB, because the peak is
        // the ingestion's own memory and the WAL's size only decides how often it
        // is written. See `MEMORY.md` §6.
        conn.execute_batch("SET checkpoint_threshold='16MB'")?;

        // Threads are derived from the memory budget: a parallel DISTINCT aggregate
        // needs one hash table per thread.
        let threads = if settings.threads > 0 {
            settings.threads
        } else {
            crate::config::threads_for(
                &settings.memory_limit,
                std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
            )
        };
        conn.execute_batch(&format!("SET threads={threads}"))?;
        Ok(())
    }

    /// The database path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The configured DuckDB memory limit.
    pub fn memory_limit(&self) -> &str {
        &self.memory_limit
    }

    /// Open a Parquet archive as an in-memory database.
    ///
    /// The archive is queried in place through `read_parquet()`; it is never imported.
    /// The views carry the same names as the tables, so the DuckDB backend's SQL runs
    /// over an archive unchanged.
    pub fn open_archive(pattern: &str) -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let escape = |value: &str| value.replace('\'', "''");
        let root = pattern.trim_end_matches("/*").trim_end_matches('/');

        for (table, sub) in [("event", "events"), ("revenue", "revenue")] {
            let directory = format!("{root}/{sub}");
            if !has_parquet_files(std::path::Path::new(&directory)) {
                continue;
            }
            let glob = format!("{directory}/**/*.parquet");
            conn.execute_batch(&format!(
                "CREATE VIEW {table} AS SELECT * FROM read_parquet('{}')",
                escape(&glob)
            ))?;
        }

        let websites = format!("{root}/websites.parquet");
        let _ = conn.execute_batch(&format!(
            "CREATE VIEW website AS SELECT * FROM read_parquet('{}')",
            escape(&websites)
        ));

        Ok(Self {
            path: PathBuf::from(pattern),
            memory_limit: "0B".to_string(),
            conn: Mutex::new(conn),
            // An archive is read-only; nothing is ever appended to it.
            next_id: AtomicI64::new(1),
        })
    }

    /// The id of the next row of the `event` table.
    ///
    /// The counter is the only thing that hands out ids, so it has to start at
    /// the right place: the first id after the largest one already in the table.
    pub fn seed_next_id(conn: &Connection) -> i64 {
        conn.query_row(
            "SELECT COALESCE(MAX(id), 0) + 1 FROM event",
            [],
            |row| row.get(0),
        )
        .unwrap_or(1)
    }

    /// The id of the next row of the `event` table.
    ///
    /// Handed out from memory: the writer is the only one that writes, so the
    /// counter cannot race.
    pub fn next_event_id(&self) -> i64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Run `f` with the connection.
    ///
    /// The dashboard reads on the same connection the writer appends on, so the
    /// numbers it shows are the numbers that were just written.
    pub fn with_read<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        f(&conn)
    }

    /// Run `f` with the connection.
    ///
    /// The same as [`Db::with_read`], under the name the archival job uses.
    pub fn with_reader<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        self.with_read(f)
    }

    /// Run `f` with the connection.
    ///
    /// This is the one that appends. Everything that writes goes through it, so
    /// there is exactly one writer — DuckDB's model anyway.
    pub fn with_write<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        f(&conn)
    }

    /// Run `f` with the connection.
    ///
    /// DuckDB connections cannot be shared between threads, and the database file
    /// is locked by this process anyway.
    pub fn with_connection<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        self.with_write(f)
    }
}

/// Does a directory (recursively) hold any parquet file?
fn has_parquet_files(directory: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return false;
    };
    for entry in entries.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        if path.is_dir() {
            if has_parquet_files(&path) {
                return true;
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("parquet") {
            return true;
        }
    }
    false
}
