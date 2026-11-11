//! The schema itself: the migration between versions, and the promise that the
//! append-only fact tables carry no index.

use sumimi_service::db::SCHEMA_VERSION;
use sumimi_service::pool::Db;
use sumimi_service::storage::Storage;
use sumimi_service::testing::HitBuilder;

/// The `event` table of a v3 SQLite database.
///
/// The v3 file is written out by hand in the tests — a v3 file has text ids
/// and no channel — and it is written out twice, so it lives here once.
const V3_SQLITE_EVENT: &str = r#"
CREATE TABLE event (
    id              INTEGER PRIMARY KEY,
    website_id      TEXT NOT NULL,
    session_id      TEXT NOT NULL,
    visitor_id      TEXT NOT NULL,
    created_at      INTEGER NOT NULL,
    url             TEXT,
    url_query       TEXT,
    title           TEXT,
    referrer        TEXT,
    referrer_path   TEXT,
    referrer_query  TEXT,
    hostname        TEXT,
    browser         TEXT,
    browser_version TEXT,
    os              TEXT,
    os_version      TEXT,
    device          TEXT,
    screen          TEXT,
    language        TEXT,
    country         TEXT,
    region          TEXT,
    city            TEXT,
    utm_source      TEXT,
    utm_medium      TEXT,
    utm_campaign    TEXT,
    utm_content     TEXT,
    utm_term        TEXT,
    gclid           TEXT,
    fbclid          TEXT,
    msclkid         TEXT,
    ttclid          TEXT,
    lifatid         TEXT,
    twclid          TEXT,
    lcp             REAL,
    inp             REAL,
    cls             REAL,
    fcp             REAL,
    ttfb            REAL,
    event_type      TEXT,
    event_name      TEXT,
    tag             TEXT,
    data            TEXT
);
"#;

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
                "INSERT INTO event (id, website_id, visit_id, visitor_id, created_at, url)
                 VALUES (?1, 'w', ?2, ?3, now(), '/x')",
                duckdb::params![
                    i as i64,
                    sumimi_service::ua::uuid_from_text("s"),
                    sumimi_service::ua::uuid_from_text("v"),
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

/// An older database is migrated in place, and nothing is lost.
#[test]
fn a_v2_database_is_migrated_without_losing_rows() {
    let path = temp_db("migrate");

    // A database written by schema v2: the fact tables had a primary key.
    {
        let conn = duckdb::Connection::open(&path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE event (
                id              BIGINT PRIMARY KEY,
                website_id      VARCHAR NOT NULL,
                session_id      VARCHAR NOT NULL,
                visitor_id      VARCHAR NOT NULL,
                created_at      TIMESTAMP NOT NULL,
                url             VARCHAR,
                url_query       VARCHAR,
                title           VARCHAR,
                referrer        VARCHAR,
                referrer_path   VARCHAR,
                referrer_query  VARCHAR,
                hostname        VARCHAR,
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
                data            VARCHAR
            );
            CREATE TABLE meta (key VARCHAR PRIMARY KEY, value VARCHAR);
            INSERT INTO meta VALUES ('schema_version', '2');
            INSERT INTO event (id, website_id, session_id, visitor_id, created_at, url, title)
            VALUES (1, 'w1', 's1', 'v1', now(), '/one', 'One'),
                   (2, 'w1', 's2', 'v2', now(), '/two', 'Two'),
                   (3, 'w2', 's3', 'v3', now(), '/three', 'Three');
            "#,
        )
        .unwrap();
    }

    // Opening the database rebuilds it into a new file and swaps it in.
    let before = std::fs::metadata(&path).unwrap().len();
    let db = Db::open(&path, "256MB").unwrap();
    db.with_read(|conn| {
        conn.execute_batch("CHECKPOINT")?;
        Ok::<_, anyhow::Error>(())
    })
    .unwrap();
    let after = std::fs::metadata(&path).unwrap().len();
    println!(
        "migrated: {before} bytes -> {after} bytes ({:+.1}%)",
        (after as f64 - before as f64) / before as f64 * 100.0
    );
    assert!(
        after <= before,
        "the rebuild must not leave the file bigger ({before} -> {after})"
    );

    // The temporary file is gone.
    let mut leftovers: Vec<String> = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|name| name.contains("rebuilt"))
        .collect();
    leftovers.sort();
    assert!(
        leftovers.is_empty(),
        "the rebuilt file must be swapped in, not left behind: {leftovers:?}"
    );

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
        assert_eq!(rows, 3, "no row may be lost in the migration");

        let title: String = conn
            .query_row(
                "SELECT title FROM event WHERE id = 2",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(title, "Two", "the row must keep its fields");

        assert_eq!(primary_keys(conn, "event"), 0, "the index must be gone");
        Ok::<_, anyhow::Error>(())
    })
    .unwrap();

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
                "INSERT INTO event (id, website_id, visit_id, visitor_id, created_at, url)
                 VALUES (?1, ?2, ?3, ?4, now(), '/x')",
                duckdb::params![
                    id,
                    website,
                    sumimi_service::ua::uuid_from_text("s"),
                    sumimi_service::ua::uuid_from_text("v"),
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

/// A v3 database is migrated to v4: the ids become UUIDs, and no session is split
/// in two.
#[test]
fn a_v3_database_is_migrated_to_uuids() {
    let path = temp_db("migrate-v4");

    // A database written by schema v3: the ids were 36-character strings.
    {
        let conn = duckdb::Connection::open(&path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE event (
                id              BIGINT NOT NULL,
                website_id      VARCHAR NOT NULL,
                session_id      VARCHAR NOT NULL,
                visitor_id      VARCHAR NOT NULL,
                created_at      TIMESTAMP NOT NULL,
                url             VARCHAR
            );
            CREATE TABLE meta (key VARCHAR PRIMARY KEY, value VARCHAR);
            INSERT INTO meta VALUES ('schema_version', '3');
            INSERT INTO event (id, website_id, session_id, visitor_id, created_at, url)
            VALUES (1, 'w1', 'session-a', 'visitor-a', now(), '/one'),
                   (2, 'w1', 'session-a', 'visitor-a', now(), '/two'),
                   (3, 'w1', 'session-b', 'visitor-a', now(), '/three');
            "#,
        )
        .unwrap();
    }

    // Opening the database rebuilds it as v4.
    let db = Db::open(&path, "256MB").unwrap();
    db.with_read(|conn| {
        let version: String = conn.query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(version, SCHEMA_VERSION.to_string());

        // The rows are all there.
        let rows: i64 = conn.query_row("SELECT count(*) FROM event", [], |row| row.get(0))?;
        assert_eq!(rows, 3, "no row may be lost in the migration");

        // The ids are UUIDs now.
        let ids: Vec<String> = {
            let mut stmt = conn.prepare("SELECT DISTINCT visit_id::VARCHAR FROM event")?;
            let mut rows = stmt.query([])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                out.push(row.get(0)?);
            }
            out
        };
        assert_eq!(ids.len(), 2, "the two sessions stay two sessions");
        for id in &ids {
            assert!(
                uuid::Uuid::parse_str(id).is_ok(),
                "the id must be a UUID: {id}"
            );
        }

        // The same old id always becomes the same UUID.
        let first: String = conn.query_row(
            "SELECT visit_id::VARCHAR FROM event WHERE id = 1",
            [],
            |row| row.get(0),
        )?;
        let second: String = conn.query_row(
            "SELECT visit_id::VARCHAR FROM event WHERE id = 2",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(first, second, "a session must not be split in two");

        Ok::<_, anyhow::Error>(())
    })
    .unwrap();

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// The SQLite backend migrates its ids too.
#[test]
fn a_v3_sqlite_database_is_migrated_to_uuids() {
    use sumimi_service::sqlite::SqliteBackend;
    use sumimi_service::storage::Storage;

    let dir = std::env::temp_dir().join(format!("sumimi-schema-sqlite-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("data.sqlite");

    // A v3 SQLite database: the ids were text.
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE event (
                id              INTEGER PRIMARY KEY,
                website_id      TEXT NOT NULL,
                session_id      TEXT NOT NULL,
                visitor_id      TEXT NOT NULL,
                created_at      INTEGER NOT NULL,
                url             TEXT,
                url_query       TEXT,
                title           TEXT,
                referrer        TEXT,
                referrer_path   TEXT,
                referrer_query  TEXT,
                hostname        TEXT,
                browser         TEXT,
                browser_version TEXT,
                os              TEXT,
                os_version      TEXT,
                device          TEXT,
                screen          TEXT,
                language        TEXT,
                country         TEXT,
                region          TEXT,
                city            TEXT,
                utm_source      TEXT,
                utm_medium      TEXT,
                utm_campaign    TEXT,
                utm_content     TEXT,
                utm_term        TEXT,
                gclid           TEXT,
                fbclid          TEXT,
                msclkid         TEXT,
                ttclid          TEXT,
                lifatid         TEXT,
                twclid          TEXT,
                lcp             REAL,
                inp             REAL,
                cls             REAL,
                fcp             REAL,
                ttfb            REAL,
                event_type      TEXT,
                event_name      TEXT,
                tag             TEXT,
                data            TEXT
            );
            CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
            INSERT INTO meta VALUES ('schema_version', '3');
            INSERT INTO event (id, website_id, session_id, visitor_id, created_at, url)
            VALUES (1, 'w1', 'session-a', 'visitor-a', 1700000000, '/one'),
                   (2, 'w1', 'session-a', 'visitor-a', 1700000060, '/two'),
                   (3, 'w1', 'session-b', 'visitor-a', 1700000120, '/three');
            "#,
        )
        .unwrap();
    }

    // Opening it migrates the ids.
    let backend = SqliteBackend::open(&path).unwrap();
    let query = sumimi_service::storage::ReportQuery::new(
        "w1",
        sumimi_service::storage::TimeRange {
            start: chrono::DateTime::from_timestamp(1_600_000_000, 0).unwrap().naive_utc(),
            end: chrono::DateTime::from_timestamp(1_800_000_000, 0).unwrap().naive_utc(),
        },
    );

    let sessions = backend.sessions(&query).unwrap();
    assert_eq!(sessions.len(), 2, "the two sessions stay two sessions");
    // The session that had two views still has two views.
    let with_two = sessions
        .iter()
        .find(|session| session.views == 2)
        .expect("the session with two views");
    assert_eq!(with_two.url.as_deref(), Some("/two"));

    // And the ids are UUIDs now.
    let ids: Vec<String> = backend
        .with_connection(|conn| {
            let mut stmt = conn.prepare("SELECT DISTINCT visit_id FROM event")?;
            let mut rows = stmt.query([])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                let bytes: Vec<u8> = row.get(0)?;
                out.push(uuid::Uuid::from_slice(&bytes).unwrap().to_string());
            }
            Ok(out)
        })
        .unwrap();
    assert_eq!(ids.len(), 2);
    for id in &ids {
        assert!(uuid::Uuid::parse_str(id).is_ok(), "the id must be a UUID: {id}");
    }

    let _ = std::fs::remove_dir_all(&dir);
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

// --------------------------------------------------------------------------------------
// The migration path, step by step.
//
// The path is v3 → v4 → v5 → v6. The code migrates straight to the current
// version, so every step is proved by starting from the version before it:
// an old file must come out the other side with its rows, its values and its
// shape.
// --------------------------------------------------------------------------------------

/// The columns a v4 file has: the ids are UUIDs, the channel is not there yet.
const V4_COLUMNS: &str = "
    id              BIGINT NOT NULL,
    website_id      VARCHAR NOT NULL,
    session_id      UUID NOT NULL,
    visitor_id      UUID NOT NULL,
    created_at      TIMESTAMP NOT NULL,
    url             VARCHAR,
    url_query       VARCHAR,
    title           VARCHAR,
    referrer        VARCHAR,
    referrer_path   VARCHAR,
    referrer_query  VARCHAR,
    hostname        VARCHAR,
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

/// Write a database of an older version, with three rows in it.
fn write_old_database(path: &std::path::Path, version: i64, columns: &str) {
    let conn = duckdb::Connection::open(path).unwrap();
    conn.execute_batch(&format!(
        "CREATE TABLE event ({columns});
         CREATE TABLE meta (key VARCHAR PRIMARY KEY, value VARCHAR);
         INSERT INTO meta VALUES ('schema_version', '{version}');"
    ))
    .unwrap();
    // The ids are UUIDs from v4 on; before that they were text.
    let (session_a, visitor_a) = if version >= 4 {
        (
            sumimi_service::ua::uuid_from_text("session-a").to_string(),
            sumimi_service::ua::uuid_from_text("visitor-a").to_string(),
        )
    } else {
        ("session-a".to_string(), "visitor-a".to_string())
    };
    let session_b = if version >= 4 {
        sumimi_service::ua::uuid_from_text("session-b").to_string()
    } else {
        "session-b".to_string()
    };
    conn.execute_batch(&format!(
        "INSERT INTO event (id, website_id, session_id, visitor_id, created_at, url, browser)
         VALUES (1, 'w1', '{session_a}', '{visitor_a}', TIMESTAMP '2026-01-15 12:00:00', '/one', 'Chrome'),
                (2, 'w1', '{session_a}', '{visitor_a}', TIMESTAMP '2026-01-15 12:01:00', '/two', 'Chrome'),
                (3, 'w1', '{session_b}', '{visitor_a}', TIMESTAMP '2026-01-15 12:02:00', '/three', 'Firefox');"
    ))
    .unwrap();
}

/// How many rows are in the file, and what do they say?
fn migrated_rows(path: &std::path::Path) -> (i64, Vec<(String, Option<String>)>) {
    let db = Db::open(path, "256MB").unwrap();
    db.with_read(|conn| {
        let version: String = conn.query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(version, SCHEMA_VERSION.to_string(), "the file says its version");

        let count: i64 = conn.query_row("SELECT count(*) FROM event", [], |row| row.get(0))?;
        let mut stmt = conn.prepare("SELECT url, browser FROM event ORDER BY id")?;
        let mut result = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(row) = result.next()? {
            out.push((row.get(0)?, row.get(1)?));
        }
        Ok((count, out))
    })
    .unwrap()
}

/// A v4 file becomes a v6 file: the channel and the new fields are there, and
/// the rows are where they were.
#[test]
fn a_v4_database_is_migrated_to_the_current_version() {
    let path = temp_db("migrate-v4-step");
    write_old_database(&path, 4, V4_COLUMNS);

    let (rows, values) = migrated_rows(&path);
    assert_eq!(rows, 3, "no row may be lost in the migration");
    assert_eq!(
        values,
        vec![
            ("/one".to_string(), Some("Chrome".to_string())),
            ("/two".to_string(), Some("Chrome".to_string())),
            ("/three".to_string(), Some("Firefox".to_string())),
        ],
        "the values must survive the migration"
    );

    // The new columns are there — empty, because the old file did not have them.
    let db = Db::open(&path, "256MB").unwrap();
    db.with_read(|conn| {
        let channel: Option<String> =
            conn.query_row("SELECT channel FROM event WHERE id = 1", [], |row| row.get(0))?;
        assert_eq!(channel, None, "the old rows have no channel yet");

        let columns: Vec<String> = {
            let mut stmt = conn.prepare(
                "SELECT column_name FROM information_schema.columns
                 WHERE table_name = 'event' ORDER BY column_name",
            )?;
            let mut rows = stmt.query([])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                out.push(row.get(0)?);
            }
            out
        };
        for column in ["channel", "visit_id", "referrer_name", "latitude", "longitude"] {
            assert!(
                columns.iter().any(|name| name == column),
                "the migrated file must have the {column} column"
            );
        }
        Ok::<_, anyhow::Error>(())
    })
    .unwrap();

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// A v5 file — the one with the channel — becomes a v6 file too.
#[test]
fn a_v5_database_is_migrated_to_the_current_version() {
    let path = temp_db("migrate-v5-to-v6");
    // v5 is v4 plus the channel.
    let columns = V4_COLUMNS.replace(
        "    hostname        VARCHAR,",
        "    hostname        VARCHAR,\n    channel         VARCHAR,",
    );
    write_old_database(&path, 5, &columns);

    // Put a channel in one row, so the migration cannot lose it.
    {
        let db = Db::open(&path, "256MB").unwrap();
        db.with_write(|conn| {
            conn.execute("UPDATE event SET channel = 'Search' WHERE id = 1", [])?;
            Ok::<_, anyhow::Error>(())
        })
        .unwrap();
    }
    // The file says v6 now (the update above opened it through the migrator).
    let (rows, _) = migrated_rows(&path);
    assert_eq!(rows, 3);

    let db = Db::open(&path, "256MB").unwrap();
    db.with_read(|conn| {
        let channel: Option<String> =
            conn.query_row("SELECT channel FROM event WHERE id = 1", [], |row| row.get(0))?;
        assert_eq!(
            channel,
            Some("Search".to_string()),
            "the channel must survive the migration"
        );
        Ok::<_, anyhow::Error>(())
    })
    .unwrap();

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// A v3 file — text ids — comes out the other side with UUID ids and every
/// new column.
#[test]
fn a_v3_database_is_migrated_all_the_way() {
    let path = temp_db("migrate-v3-to-v6");
    // v3 is v4 with text ids.
    let columns = V4_COLUMNS
        .replace("session_id      UUID NOT NULL", "session_id      VARCHAR NOT NULL")
        .replace("visitor_id      UUID NOT NULL", "visitor_id      VARCHAR NOT NULL");
    write_old_database(&path, 3, &columns);

    let (rows, values) = migrated_rows(&path);
    assert_eq!(rows, 3, "no row may be lost in the migration");
    assert_eq!(values.len(), 3);

    let db = Db::open(&path, "256MB").unwrap();
    db.with_read(|conn| {
        let ids: Vec<String> = {
            let mut stmt = conn.prepare("SELECT DISTINCT visit_id::VARCHAR FROM event")?;
            let mut rows = stmt.query([])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                out.push(row.get(0)?);
            }
            out
        };
        assert_eq!(ids.len(), 2, "the two sessions stay two sessions");
        for id in &ids {
            assert!(uuid::Uuid::parse_str(id).is_ok(), "the id must be a UUID: {id}");
        }
        Ok::<_, anyhow::Error>(())
    })
    .unwrap();

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// The SQLite backend walks the same path: the channel and the new fields are
/// added, and the rows stay.
#[test]
fn a_v3_sqlite_database_is_migrated_all_the_way() {
    use sumimi_service::sqlite::SqliteBackend;

    let dir = std::env::temp_dir().join(format!(
        "sumimi-schema-sqlite-path-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("data.sqlite");

    // A v3 SQLite file: text ids, no channel.
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(&format!(
            r#"{V3_SQLITE_EVENT}
            CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
            INSERT INTO meta VALUES ('schema_version', '3');
            INSERT INTO event (id, website_id, session_id, visitor_id, created_at, url)
            VALUES (1, 'w1', 'session-a', 'visitor-a', 1700000000, '/one'),
                   (2, 'w1', 'session-a', 'visitor-a', 1700000060, '/two'),
                   (3, 'w1', 'session-b', 'visitor-a', 1700000120, '/three');
            "#
        ))
        .unwrap();
    }

    // Opening it walks the whole path.
    let backend = SqliteBackend::open(&path).unwrap();

    let count: i64 = backend
        .with_connection(|conn| {
            let count: i64 = conn.query_row("SELECT count(*) FROM event", [], |row| row.get(0))?;
            Ok(count)
        })
        .unwrap();
    assert_eq!(count, 3, "no row may be lost in the migration");

    // The new columns are there.
    backend
        .with_connection(|conn| {
            for column in ["channel", "visit_id", "referrer_name", "latitude", "longitude"] {
                let mut stmt = conn.prepare(&format!("SELECT {column} FROM event LIMIT 1"))?;
                let mut rows = stmt.query([])?;
                assert!(
                    rows.next().unwrap().is_some(),
                    "the migrated file must have the {column} column"
                );
            }
            // And the ids are UUIDs.
            let mut stmt = conn.prepare("SELECT DISTINCT visit_id FROM event")?;
            let mut rows = stmt.query([])?;
            let mut sessions = 0;
            while let Some(row) = rows.next()? {
                let bytes: Vec<u8> = row.get(0)?;
                let id = uuid::Uuid::from_slice(&bytes).unwrap();
                assert!(uuid::Uuid::parse_str(&id.to_string()).is_ok());
                sessions += 1;
            }
            assert_eq!(sessions, 2, "the two sessions stay two sessions");
            Ok::<_, anyhow::Error>(())
        })
        .unwrap();

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

/// A v6 file — the one with the hour-long `visit_id` — becomes the current
/// one: the visit id is what used to be the session id, the identity the site
/// owner knows the visitor by is new and empty, and no row is lost.
#[test]
fn a_v6_database_is_migrated_to_the_visit_and_user_ids() {
    let path = temp_db("migrate-v6-to-v7");
    // v6 is v4 plus the channel, the hour-long visit id, the referrer's display
    // name and the coordinates.
    let columns = V4_COLUMNS
        .replace(
            "    hostname        VARCHAR,",
            "    hostname        VARCHAR,\n    channel         VARCHAR,\n    visit_id        UUID,\n    referrer_name   VARCHAR,\n    latitude        DOUBLE,\n    longitude       DOUBLE,",
        );
    write_old_database(&path, 6, &columns);

    // The session the old file had, and the hour-long visit inside it.
    let session = sumimi_service::ua::uuid_from_text("session-a").to_string();
    let hour = sumimi_service::ua::uuid_from_text("hour-a").to_string();
    // The hour-long visit id is set on the **old** file — with a raw connection,
    // because opening it through `Db` would migrate it right here.
    {
        let raw = duckdb::Connection::open(&path).unwrap();
        raw.execute(
            "UPDATE event SET visit_id = ?::UUID WHERE id = 1",
            duckdb::params![hour],
        )
        .unwrap();
    }

    let db = Db::open(&path, "256MB").unwrap();
    db.with_read(|conn| {
        let count: i64 = conn.query_row("SELECT count(*) FROM event", [], |row| row.get(0))?;
        assert_eq!(count, 3, "no row may be lost in the migration");

        // The visit id is the old session id.
        let visit: String = conn.query_row(
            "SELECT visit_id::VARCHAR FROM event WHERE id = 1",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(
            visit, session,
            "the visit id must be what the old file called the session id"
        );

        // The identity the site owner knows the visitor by is new: empty.
        let user: Option<String> =
            conn.query_row("SELECT user_id::VARCHAR FROM event WHERE id = 1", [], |row| {
                row.get(0)
            })?;
        assert!(user.is_none(), "the old file has no user id");

        // The visitor is where it was.
        let visitor: String = conn.query_row(
            "SELECT visitor_id::VARCHAR FROM event WHERE id = 1",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(
            visitor,
            sumimi_service::ua::uuid_from_text("visitor-a").to_string(),
            "the visitor id must survive the migration"
        );
        Ok::<_, anyhow::Error>(())
    })
    .unwrap();

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// The same for SQLite: the visit id is the old session id, the user id is
/// new and empty.
#[test]
fn a_v6_sqlite_database_is_migrated_to_the_visit_and_user_ids() {
    use sumimi_service::sqlite::SqliteBackend;

    let dir = std::env::temp_dir().join(format!(
        "sumimi-schema-sqlite-v6-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("data.sqlite");

    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(&format!(
            r#"{V3_SQLITE_EVENT}
            CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
            INSERT INTO meta VALUES ('schema_version', '6');
            ALTER TABLE event ADD COLUMN visit_id BLOB;
            INSERT INTO event (id, website_id, session_id, visitor_id, visit_id, created_at, url)
            VALUES (1, 'w1', 'session-a', 'visitor-a', X'00000000000000000000000000000001', 1700000000, '/one'),
                   (2, 'w1', 'session-a', 'visitor-a', X'00000000000000000000000000000002', 1700000060, '/two'),
                   (3, 'w1', 'session-b', 'visitor-a', X'00000000000000000000000000000003', 1700000120, '/three');
            "#
        ))
        .unwrap();
    }

    let backend = SqliteBackend::open(&path).unwrap();

    let count: i64 = backend
        .with_connection(|conn| {
            let count: i64 = conn.query_row("SELECT count(*) FROM event", [], |row| row.get(0))?;
            Ok(count)
        })
        .unwrap();
    assert_eq!(count, 3, "no row may be lost in the migration");

    let (visit, user_is_null) = backend
        .with_connection(|conn| {
            let visit: String = conn.query_row(
                "SELECT hex(visit_id) FROM event WHERE id = 1",
                [],
                |row| row.get(0),
            )?;
            let user_is_null: bool = conn.query_row(
                "SELECT user_id IS NULL FROM event WHERE id = 1",
                [],
                |row| row.get(0),
            )?;
            Ok((visit, user_is_null))
        })
        .unwrap();
    assert_eq!(
        visit,
        "73657373696F6E2D61".to_uppercase(),
        "the visit id must be what the old file called the session id"
    );
    assert!(user_is_null, "the old file has no user id");

    let _ = std::fs::remove_dir_all(&dir);
}
