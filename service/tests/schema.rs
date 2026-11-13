//! The schema itself: what happens to an older database, and the promise that
//! the append-only fact tables carry no index.
//!
//! **There is no migration.** The shape of the session id changed (sixteen
//! bytes → eight), and the rows of an old file cannot be carried over — so an
//! older database is **replaced**: the old file is set aside under its own
//! version number, and an empty one of the current version takes its place.
//! The accounts are not in this file (they live in the instance store), so
//! they survive.

use sumimi_service::db::SCHEMA_VERSION;
use sumimi_service::pool::Db;
use sumimi_service::storage::Storage;
use sumimi_service::testing::HitBuilder;

/// A unique temporary database path.
fn temp_db(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("sumimi-schema-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("sumimi.duckdb")
}

/// How many primary keys does this table have?
fn primary_keys(conn: &duckdb::Connection, table: &str) -> i64 {
    conn.query_row(
        "SELECT count(*) FROM duckdb_constraints()
         WHERE table_name = ? AND constraint_type = 'PRIMARY KEY'",
        [table],
        |row| row.get(0),
    )
    .unwrap()
}

/// A fresh database has no index on the fact tables.
///
/// A `PRIMARY KEY` on an append-only table buys nothing — every query is a filtered
/// scan, and DuckDB prunes those with zone maps — but it costs 8 bytes per row on
/// disk (the persisted ART index) and a uniqueness check on every append.
#[test]
fn the_fact_tables_have_no_primary_key() {
    let path = temp_db("no-pk");
    let db = Db::open(&path, "256MB").unwrap();

    db.with_read(|conn| {
        assert_eq!(
            primary_keys(conn, "event"),
            0,
            "the event table must not carry a primary key"
        );
        assert_eq!(
            primary_keys(conn, "revenue"),
            0,
            "the revenue table must not carry a primary key"
        );
        // The small dimension tables keep their keys.
        assert_eq!(primary_keys(conn, "website"), 1);
        assert_eq!(primary_keys(conn, "meta"), 1);
        // The accounts are **not** in the analytics file: they live in the instance
        // store, in their own small file.
        assert_eq!(
            primary_keys(conn, "user"),
            0,
            "the accounts must not be in the analytics file"
        );
        Ok::<_, anyhow::Error>(())
    })
    .unwrap();

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// The ids stay unique without a constraint: the writer hands them out.
#[test]
fn the_ids_are_unique_without_a_constraint() {
    let path = temp_db("ids");
    let db = Db::open(&path, "256MB").unwrap();

    for i in 0..500 {
        db.with_write(|conn| {
            conn.execute(
                "INSERT INTO event (id, website_id, visit_id, visitor_id, ip, ip_prefix, user_agent, created_at, url)
                 VALUES (?1, 'w', ?2, ?3, '', '203.0.113', 'UA', now(), '/x')",
                duckdb::params![
                    i as i64,
                    sumimi_service::session::client_session_id("s"),
                    sumimi_service::ua::visitor_id("203.0.113", "UA", "w"),
                ],
            )?;
            Ok::<_, anyhow::Error>(())
        })
        .unwrap();
    }

    let (rows, distinct) = db
        .with_read(|conn| {
            let rows: i64 = conn.query_row("SELECT count(*) FROM event", [], |row| row.get(0))?;
            let distinct: i64 =
                conn.query_row("SELECT count(DISTINCT id) FROM event", [], |row| row.get(0))?;
            Ok((rows, distinct))
        })
        .unwrap();

    assert_eq!(rows, 500);
    assert_eq!(distinct, 500, "the ids must be unique");

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// Deleting a website deletes its events — the session table is gone, so nothing
/// else may be deleted from it.
#[test]
fn deleting_a_website_removes_its_events() {
    let path = temp_db("delete");
    let db = Db::open(&path, "256MB").unwrap();

    db.with_write(|conn| {
        conn.execute(
            "INSERT INTO website (id, name, domain, created_at, updated_at, retention_days, country_header)
             VALUES ('w1', 'One', 'one.example', now(), now(), 0, NULL),
                    ('w2', 'Two', 'two.example', now(), now(), 0, NULL)",
            [],
        )?;
        for (id, website) in [(1i64, "w1"), (2, "w1"), (3, "w2")] {
            conn.execute(
                "INSERT INTO event (id, website_id, visit_id, visitor_id, ip, ip_prefix, user_agent, created_at, url)
                 VALUES (?1, ?2, ?3, ?4, '', '203.0.113', 'UA', now(), '/x')",
                duckdb::params![
                    id,
                    website,
                    sumimi_service::session::client_session_id("s"),
                    sumimi_service::ua::visitor_id("203.0.113", "UA", website),
                ],
            )?;
        }
        Ok::<_, anyhow::Error>(())
    })
    .unwrap();

    let removed = db.delete_website("w1").unwrap();
    assert!(removed, "the website must be deleted");

    let (events, websites) = db
        .with_read(|conn| {
            let events: i64 = conn.query_row("SELECT count(*) FROM event", [], |row| row.get(0))?;
            let websites: i64 =
                conn.query_row("SELECT count(*) FROM website", [], |row| row.get(0))?;
            Ok((events, websites))
        })
        .unwrap();

    assert_eq!(events, 1, "only the other website's event may remain");
    assert_eq!(websites, 1);

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// The row ids must stay unique when the server restarts.
///
/// The ids are handed out from a counter in memory (the database is asked once,
/// when it is opened), so the counter has to start at the right place — and it
/// has to stay right when the process that wrote the rows is gone.
#[test]
fn the_row_ids_stay_unique_across_a_restart() {
    let dir = std::env::temp_dir().join(format!("sumimi-ids-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("sumimi.duckdb");

    let hit = |url: &str| HitBuilder::new("w1", "s1", "v1").url(url).build();

    // The first run writes three rows.
    {
        let db = Db::open(&path, "256MB").unwrap();
        db.write_batch(&[hit("/one"), hit("/two"), hit("/three")]).unwrap();
    }

    // The second run — a restart — writes three more.
    {
        let db = Db::open(&path, "256MB").unwrap();
        db.write_batch(&[hit("/four"), hit("/five"), hit("/six")]).unwrap();
    }

    // The ids are unique, and there are six of them.
    let db = Db::open(&path, "256MB").unwrap();
    let (count, distinct, smallest, largest) = db
        .with_connection(|conn| -> anyhow::Result<(i64, i64, i64, i64)> {
            let mut stmt = conn.prepare(
                "SELECT count(*), count(DISTINCT id), min(id), max(id) FROM event",
            )?;
            let mut rows = stmt.query([])?;
            let row = rows.next()?.unwrap();
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap();

    assert_eq!(count, 6, "every row is there");
    assert_eq!(distinct, 6, "every row has its own id");
    assert_eq!(smallest, 1, "the ids start at one");
    assert_eq!(largest, 6, "the ids are consecutive");

    let _ = std::fs::remove_dir_all(&dir);
}

/// The ids must stay unique when the database is opened by the CLI, which
/// brings its own connection.
#[test]
fn the_row_ids_stay_unique_when_the_cli_opens_the_database() {
    let dir = std::env::temp_dir().join(format!("sumimi-ids-cli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("sumimi.duckdb");

    let hit = |url: &str| HitBuilder::new("w1", "s1", "v1").url(url).build();

    // The CLI opens the file with its own connection — the schema is already
    // there, because the server created it.
    {
        let conn = duckdb::Connection::open(&path).unwrap();
        sumimi_service::db::init_schema(&conn).unwrap();
        let db = Db::from_connection(&path, conn);
        db.write_batch(&[hit("/one"), hit("/two")]).unwrap();
    }
    {
        let conn = duckdb::Connection::open(&path).unwrap();
        let db = Db::from_connection(&path, conn);
        db.write_batch(&[hit("/three")]).unwrap();
    }

    let conn = duckdb::Connection::open(&path).unwrap();
    let db = Db::from_connection(&path, conn);
    let (count, distinct) = db
        .with_connection(|conn| -> anyhow::Result<(i64, i64)> {
            let mut stmt = conn.prepare("SELECT count(*), count(DISTINCT id) FROM event")?;
            let mut rows = stmt.query([])?;
            let row = rows.next()?.unwrap();
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();

    assert_eq!(count, 3);
    assert_eq!(distinct, 3, "the ids do not collide after a reopen");

    let _ = std::fs::remove_dir_all(&dir);
}

/// The row of an event has one value for every column.
///
/// The appender takes the values positionally, so a row that is one value short
/// — or one too long — would put a page view in the wrong column. This is the
/// check that the schema and the row cannot drift apart.
#[test]
fn the_event_row_has_a_value_for_every_column() {
    let hit = HitBuilder::new("w1", "s1", "v1").build();
    let kind = hit.kind.as_str();
    let row = sumimi_service::db::event_row(&1, &hit, &kind);

    assert_eq!(row.len(), sumimi_service::db::event_columns().len());
}

/// An older database is **replaced**, not migrated.
///
/// The old file is set aside under its own version number — so nothing is
/// destroyed — and an empty database of the current version takes its place.
/// The analysis data and the site registrations are **not** carried over: the
/// session id means something different now, and the registrations live in the
/// same file.
///
/// (The accounts are in the instance store, in their own file, so they are not
/// touched by any of this.)
#[test]
fn an_older_database_is_replaced_by_an_empty_one() {
    let path = temp_db("replace");

    // A database written by schema v6: the fact tables had a primary key, the
    // session id was a UUID, and the site was registered.
    {
        let conn = duckdb::Connection::open(&path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE event (
                id              BIGINT PRIMARY KEY,
                website_id      VARCHAR NOT NULL,
                visit_id        UUID NOT NULL,
                visitor_id      UUID NOT NULL,
                created_at      TIMESTAMP NOT NULL,
                url             VARCHAR,
                event_type      VARCHAR
            );
            CREATE TABLE website (
                id                VARCHAR PRIMARY KEY,
                name              VARCHAR NOT NULL,
                domain            VARCHAR,
                created_at        TIMESTAMP NOT NULL,
                updated_at        TIMESTAMP NOT NULL,
                retention_days    INTEGER NOT NULL DEFAULT 0,
                country_header    VARCHAR
            );
            CREATE TABLE meta (key VARCHAR PRIMARY KEY, value VARCHAR);
            INSERT INTO meta VALUES ('schema_version', '6');
            INSERT INTO website VALUES ('w1', 'One', 'one.example', now(), now(), 0, NULL);
            INSERT INTO event (id, website_id, visit_id, visitor_id, created_at, url, event_type)
            VALUES (1, 'w1', '00000000-0000-0000-0000-000000000001',
                    '00000000-0000-0000-0000-000000000002', now(), '/one', 'pageview');
            "#,
        )
        .unwrap();
    }

    // Opening it replaces it.
    let db = Db::open(&path, "256MB").unwrap();

    db.with_read(|conn| {
        let version: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION.to_string(), "the version must be current");

        let rows: i64 = conn.query_row("SELECT count(*) FROM event", [], |row| row.get(0))?;
        assert_eq!(rows, 0, "the old rows are not carried over");

        let websites: i64 = conn.query_row("SELECT count(*) FROM website", [], |row| row.get(0))?;
        assert_eq!(websites, 0, "the registrations are not carried over either");

        // The new schema is the one the code expects.
        let visit_id: i64 = conn.query_row(
            "SELECT count(*) FROM duckdb_columns()
             WHERE table_name = 'event' AND column_name = 'visit_id'
               AND data_type = 'UUID'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(visit_id, 1, "the visit id is sixteen bytes now");

        let visitor_id: i64 = conn.query_row(
            "SELECT count(*) FROM duckdb_columns()
             WHERE table_name = 'event' AND column_name = 'visitor_id'
               AND data_type = 'UUID'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(visitor_id, 1, "the visitor id is stored again, as sixteen bytes");

        assert_eq!(primary_keys(conn, "event"), 0, "the index must be gone");
        Ok::<_, anyhow::Error>(())
    })
    .unwrap();

    // The old file is still there, under its own version number.
    let backup = sumimi_service::db::versioned_backup_path(&path, 6);
    assert!(backup.exists(), "the old file must be kept as {}", backup.display());

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// The same rule on the SQLite side.
#[test]
fn an_older_sqlite_database_is_replaced_by_an_empty_one() {
    let dir = std::env::temp_dir().join(format!("sumimi-schema-sqlite-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("sumimi.sqlite");

    // A v6 SQLite file: the session id was a 16-byte blob.
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE event (
                id              INTEGER PRIMARY KEY,
                website_id      TEXT NOT NULL,
                visit_id        BLOB NOT NULL,
                visitor_id      BLOB NOT NULL,
                created_at      INTEGER NOT NULL,
                url             TEXT,
                event_type      TEXT
            );
            CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
            INSERT INTO meta VALUES ('schema_version', '6');
            INSERT INTO event (id, website_id, visit_id, visitor_id, created_at, url, event_type)
            VALUES (1, 'w1', x'00000000000000000000000000000001',
                    x'00000000000000000000000000000002', 1700000000000000, '/one', 'pageview');
            "#,
        )
        .unwrap();
    }

    let backend = sumimi_service::sqlite::SqliteBackend::open(&path).unwrap();

    let (version, rows) = backend
        .with_connection(|conn| {
            let version: String = conn.query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )?;
            let rows: i64 = conn.query_row("SELECT count(*) FROM event", [], |row| row.get(0))?;
            Ok((version, rows))
        })
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION.to_string(), "the version must be current");
    assert_eq!(rows, 0, "the old rows are not carried over");

    let backup = sumimi_service::db::versioned_backup_path(&path, 6);
    assert!(backup.exists(), "the old file must be kept as {}", backup.display());

    let _ = std::fs::remove_dir_all(&dir);
}
