//! The schema itself: what happens to an older database, and the promise that
//! the append-only fact tables carry no index.
//!
//! **Most older databases are replaced, not migrated.** The shape of the
//! session id changed (sixteen bytes → eight), and the rows of an old file
//! cannot be carried over — so an older database is **replaced**: the old file
//! is set aside under its own version number, and an empty one of the current
//! version takes its place. The accounts are not in this file (they live in the
//! instance store), so they survive.
//!
//! **v11 is the exception.** The change to v12 is a change of *shape* — the
//! facts move into a file per site, and five columns are renamed — and neither
//! of those changes what a row *means*. ⇒ ⇒ So the rows are **carried over**:
//! they move into their site's file, with the columns renamed on the way. See
//! [`a_v11_database_is_split_into_one_file_per_site`].

use sumimi_service::db::{self, SCHEMA_VERSION};
use sumimi_service::pool::Db;
use sumimi_service::storage::{Layout, Storage};
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

/// The name of the site's file — the catalog its tables live under.
fn alias(website_id: &str) -> String {
    db::site_alias(website_id)
}

/// Register a site under a chosen id.
///
/// ⇒ ⇒ A file per site is attached when the database is opened, and the sites
/// it attaches are the **registered** ones — so a test that writes to a site
/// has to register it, or its file is not found after a restart.
fn register(db: &Db, website_id: &str) {
    db.with_write(|conn| {
        conn.execute(
            "INSERT INTO website (id, name, domain, created_at, updated_at, retention_days, country_header)
             VALUES (?1, ?1, ?1, now(), now(), 0, NULL)",
            [website_id],
        )?;
        Ok::<_, anyhow::Error>(())
    })
    .unwrap();
}

/// How many rows does a site's file hold?
///
/// ⇒ ⇒ The facts live in the site's own file since v12, so a question about
/// the rows is a question about that file.
fn site_rows(db: &Db, website_id: &str) -> i64 {
    let alias = alias(website_id);
    db.with_read(|conn| {
        Ok(conn.query_row(
            &format!("SELECT count(*) FROM {alias}.website_event"),
            [],
            |row| row.get(0),
        )?)
    })
    .unwrap()
}

/// A fresh database has no index on the fact tables.
///
/// A `PRIMARY KEY` on an append-only table buys nothing — every query is a filtered
/// scan, and DuckDB prunes those with zone maps — but it costs 8 bytes per row on
/// disk (the persisted ART index) and a uniqueness check on every append.
///
/// ⇒ ⇒ The fact tables are in the **sites' files** now, so they are checked there.
#[test]
fn the_fact_tables_have_no_primary_key() {
    let path = temp_db("no-pk");
    let db = Db::open(&path, "256MB").unwrap();

    let website = db.create_website("One", Some("one.example"), 0, None).unwrap();

    db.with_read(|conn| {
        let alias = alias(&website.id);
        assert_eq!(
            primary_keys(conn, "website_event"),
            0,
            "the fact table must not carry a primary key"
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
        // The fact tables are the site's own — the main file holds only the
        // sites and the bookkeeping.
        assert_eq!(
            primary_keys(conn, &format!("{alias}.website_event")),
            0,
            "the site's fact table must not carry a primary key"
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

    let website = db.create_website("One", Some("one.example"), 0, None).unwrap();
    let alias = alias(&website.id);

    for i in 0..500 {
        db.with_write(|conn| {
            conn.execute(
                &format!(
                    "INSERT INTO {alias}.website_event (event_id, website_id, visit_id, session_id, ip, ip_prefix, user_agent, created_at, url_path)
                     VALUES (?1, ?2, ?3, ?4, '', '203.0.113', 'UA', now(), '/x')"
                ),
                duckdb::params![
                    i as i64,
                    website.id,
                    sumimi_service::session::client_session_id("s"),
                    sumimi_service::ua::visitor_id("203.0.113", "UA", &website.id),
                ],
            )?;
            Ok::<_, anyhow::Error>(())
        })
        .unwrap();
    }

    let (rows, distinct) = db
        .with_read(|conn| {
            let rows: i64 = conn.query_row(
                &format!("SELECT count(*) FROM {alias}.website_event"),
                [],
                |row| row.get(0),
            )?;
            let distinct: i64 = conn.query_row(
                &format!("SELECT count(DISTINCT event_id) FROM {alias}.website_event"),
                [],
                |row| row.get(0),
            )?;
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

    let one = db.create_website("One", Some("one.example"), 0, None).unwrap();
    let two = db.create_website("Two", Some("two.example"), 0, None).unwrap();

    for (id, website) in [(1i64, &one.id), (2, &one.id), (3, &two.id)] {
        let alias = alias(website);
        db.with_write(|conn| {
            conn.execute(
                &format!(
                    "INSERT INTO {alias}.website_event (event_id, website_id, visit_id, session_id, ip, ip_prefix, user_agent, created_at, url_path)
                     VALUES (?1, ?2, ?3, ?4, '', '203.0.113', 'UA', now(), '/x')"
                ),
                duckdb::params![
                    id,
                    website,
                    sumimi_service::session::client_session_id("s"),
                    sumimi_service::ua::visitor_id("203.0.113", "UA", website),
                ],
            )?;
            Ok::<_, anyhow::Error>(())
        })
        .unwrap();
    }

    let removed = db.delete_website(&one.id).unwrap();
    assert!(removed, "the website must be deleted");

    // **Deleting it again is not an error** — it is nothing to do. ⇒ ⇒ The
    // facts live in the site's own file, and that file is gone with the site,
    // so the question has to be asked before the file is touched.
    assert!(
        !db.delete_website(&one.id).unwrap(),
        "a site that is not there is not deleted again"
    );

    assert_eq!(site_rows(&db, &two.id), 1, "only the other website's event may remain");

    let websites: i64 = db
        .with_read(|conn| Ok(conn.query_row("SELECT count(*) FROM website", [], |row| row.get(0))?))
        .unwrap();
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

    // The first run registers the site and writes three rows.
    //
    // ⇒ ⇒ The site has to be **registered**: a file per site is attached when
    // the database is opened, and the sites it attaches are the registered
    // ones.
    {
        let db = Db::open(&path, "256MB").unwrap();
        register(&db, "w1");
        db.write_batch(&[hit("/one"), hit("/two"), hit("/three")]).unwrap();
    }

    // The second run — a restart — writes three more.
    {
        let db = Db::open(&path, "256MB").unwrap();
        db.write_batch(&[hit("/four"), hit("/five"), hit("/six")]).unwrap();
    }

    // The ids are unique, and there are six of them.
    let db = Db::open(&path, "256MB").unwrap();
    let alias = alias("w1");
    let (count, distinct, smallest, largest) = db
        .with_connection(|conn| -> anyhow::Result<(i64, i64, i64, i64)> {
            let mut stmt = conn.prepare(&format!(
                "SELECT count(*), count(DISTINCT event_id), min(event_id), max(event_id) FROM {alias}.website_event",
            ))?;
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

    // The server creates the instance — the main file and the site's file.
    {
        let db = Db::open(&path, "256MB").unwrap();
        register(&db, "w1");
        db.write_batch(&[hit("/one"), hit("/two")]).unwrap();
    }

    // The CLI opens the file with its own connection — the schema is already
    // there, because the server created it.
    {
        let conn = duckdb::Connection::open(&path).unwrap();
        let db = Db::from_connection(&path, conn).unwrap();
        db.write_batch(&[hit("/three")]).unwrap();
    }

    let conn = duckdb::Connection::open(&path).unwrap();
    let db = Db::from_connection(&path, conn).unwrap();
    let alias = alias("w1");
    let (count, distinct) = db
        .with_connection(|conn| -> anyhow::Result<(i64, i64)> {
            let mut stmt = conn.prepare(&format!(
                "SELECT count(*), count(DISTINCT event_id) FROM {alias}.website_event"
            ))?;
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

/// The facts live in the site's own file, and the main file holds only the
/// sites and the bookkeeping.
///
/// ⇒ ⇒ This is the shape `D-046` decided on: a query for one site reads that
/// site's file and nothing else.
#[test]
fn the_facts_live_in_the_sites_own_file() {
    let path = temp_db("per-site");
    let db = Db::open(&path, "256MB").unwrap();

    let website = db.create_website("One", Some("one.example"), 0, None).unwrap();
    db.write_batch(&[HitBuilder::new(&website.id, "s1", "v1").build()])
        .unwrap();

    assert_eq!(db.layout(), Layout::PerSite, "the server's shape is one file per site");

    // The site's file is there, beside the main file.
    let site_file = db::site_path(&path, &website.id, "duckdb");
    assert!(site_file.exists(), "the site's file must be at {}", site_file.display());
    assert_eq!(
        site_file.parent().unwrap(),
        db::sites_dir(&path).as_path(),
        "the site's file lives in the sites directory"
    );

    // The instance is the main file and every site's file.
    let files = db::instance_files(&path, "duckdb");
    assert!(files.contains(&path), "the main file is part of the instance");
    assert!(files.contains(&site_file), "the site's file is part of the instance");

    // The main file holds no facts.
    let facts: i64 = db
        .with_read(|conn| {
            Ok(conn.query_row(
                "SELECT count(*) FROM information_schema.tables \
                 WHERE table_catalog = 'sumimi' AND table_name = 'website_event'",
                [],
                |row| row.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(facts, 0, "the main file must not hold the facts");

    // The site's file holds them.
    assert_eq!(site_rows(&db, &website.id), 1);

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// A v11 database is **split**, not replaced: the rows are carried over.
///
/// ⇒ ⇒ This is the first change of the schema that does not throw the data
/// away: the sites' rows move into their own files (with the columns renamed
/// on the way), the old file is kept beside the new one, and the row count and
/// the row ids do not change.
#[test]
fn a_v11_database_is_split_into_one_file_per_site() {
    let path = temp_db("split-v11");

    // A database written by schema v11: the facts are in the main file, the
    // table is called `event`, and the columns have their old names.
    {
        let conn = duckdb::Connection::open(&path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE event (
                id              BIGINT NOT NULL,
                website_id      VARCHAR NOT NULL,
                visit_id        UUID NOT NULL,
                session_id      UUID NOT NULL,
                session_start   TIMESTAMP,
                ip              VARCHAR DEFAULT '',
                ip_prefix       VARCHAR DEFAULT '',
                user_agent      VARCHAR DEFAULT '',
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
                data            VARCHAR
            );
            CREATE TABLE revenue (
                id              BIGINT NOT NULL,
                website_id      VARCHAR NOT NULL,
                visit_id        UUID,
                event_id        BIGINT,
                event_name      VARCHAR,
                currency        VARCHAR,
                revenue         DOUBLE,
                created_at      TIMESTAMP NOT NULL
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
            INSERT INTO meta VALUES ('schema_version', '11');
            INSERT INTO website VALUES ('w1', 'One', 'one.example', now(), now(), 0, NULL);
            INSERT INTO website VALUES ('w2', 'Two', 'two.example', now(), now(), 0, NULL);
            INSERT INTO event (id, website_id, visit_id, session_id, ip, ip_prefix, user_agent, created_at, url, url_query, title, referrer, referrer_path, referrer_query, hostname, browser, os, device, country, city, event_type)
            VALUES (1, 'w1', '00000000-0000-0000-0000-000000000001',
                    '00000000-0000-0000-0000-000000000002', '', '203.0.113', 'UA', now(),
                    '/one', 'a=1', 'One', 'news.ycombinator.com', '/item', '?id=1', 'example.com',
                    'Chrome', 'macOS', 'desktop', 'DE', 'Berlin', 'pageview'),
                   (2, 'w1', '00000000-0000-0000-0000-000000000003',
                    '00000000-0000-0000-0000-000000000004', '', '203.0.113', 'UA', now(),
                    '/two', NULL, 'Two', NULL, NULL, NULL, 'example.com',
                    'Chrome', 'macOS', 'desktop', 'DE', 'Berlin', 'pageview'),
                   (3, 'w2', '00000000-0000-0000-0000-000000000005',
                    '00000000-0000-0000-0000-000000000006', '', '203.0.113', 'UA', now(),
                    '/other', NULL, NULL, NULL, NULL, NULL, 'two.example',
                    'Firefox', 'Linux', 'desktop', 'FR', 'Paris', 'pageview');
            INSERT INTO revenue (id, website_id, visit_id, event_id, event_name, currency, revenue, created_at)
            VALUES (1, 'w1', '00000000-0000-0000-0000-000000000001', 1, 'sale', 'EUR', 1.5, now());
            "#,
        )
        .unwrap();
    }

    // Opening it splits it.
    let db = Db::open(&path, "256MB").unwrap();

    // The version is current.
    let version: String = db
        .with_read(|conn| {
            Ok(conn.query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )?)
        })
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION.to_string(), "the version must be current");

    // The registrations are carried over — they are facts about the sites, not
    // about the traffic.
    let websites: i64 = db
        .with_read(|conn| Ok(conn.query_row("SELECT count(*) FROM website", [], |row| row.get(0))?))
        .unwrap();
    assert_eq!(websites, 2, "the registrations are carried over");

    // The rows are carried over, split by site.
    assert_eq!(site_rows(&db, "w1"), 2, "w1's rows are in w1's file");
    assert_eq!(site_rows(&db, "w2"), 1, "w2's rows are in w2's file");

    // The ids did not change — they are the ids the old file had.
    let ids: Vec<i64> = db
        .with_read(|conn| {
            let alias = alias("w1");
            let mut stmt = conn.prepare(&format!(
                "SELECT event_id FROM {alias}.website_event ORDER BY event_id"
            ))?;
            let mut rows = stmt.query([])?;
            let mut ids = Vec::new();
            while let Some(row) = rows.next()? {
                ids.push(row.get(0)?);
            }
            Ok(ids)
        })
        .unwrap();
    assert_eq!(ids, vec![1, 2], "the row ids are the ones the old file had");

    // The columns are renamed — the old names are gone, the new ones are there.
    db.with_read(|conn| {
        for (old, new) in [
            ("id", "event_id"),
            ("url", "url_path"),
            ("title", "page_title"),
            ("referrer", "referrer_domain"),
            ("lifatid", "li_fat_id"),
        ] {
            // The catalog says which file is which: every site has a file of
            // its own, so the count is per file.
            let old_columns: i64 = conn.query_row(
                "SELECT count(*) FROM information_schema.columns \
                 WHERE table_catalog = ? AND table_name = 'website_event' AND column_name = ?",
                duckdb::params![alias("w1"), old],
                |row| row.get(0),
            )?;
            assert_eq!(old_columns, 0, "the column `{old}` must be gone");

            let new_columns: i64 = conn.query_row(
                "SELECT count(*) FROM information_schema.columns \
                 WHERE table_catalog = ? AND table_name = 'website_event' AND column_name = ?",
                duckdb::params![alias("w1"), new],
                |row| row.get(0),
            )?;
            assert_eq!(new_columns, 1, "the column `{new}` must be there");
        }
        Ok::<_, anyhow::Error>(())
    })
    .unwrap();

    // The values came across too — the page, its title and its referrer.
    let row = db
        .with_read(|conn| {
            let alias = alias("w1");
            Ok(conn.query_row(
                &format!(
                    "SELECT url_path, url_query, page_title, referrer_domain, referrer_path \
                     FROM {alias}.website_event WHERE event_id = 1"
                ),
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                },
            )?)
        })
        .unwrap();
    assert_eq!(row.0, "/one");
    assert_eq!(row.1.as_deref(), Some("a=1"));
    assert_eq!(row.2.as_deref(), Some("One"));
    assert_eq!(row.3.as_deref(), Some("news.ycombinator.com"));
    assert_eq!(row.4.as_deref(), Some("/item"));

    // The revenue came across as well.
    let revenue: i64 = db
        .with_read(|conn| {
            let alias = alias("w1");
            Ok(conn.query_row(
                &format!("SELECT count(*) FROM {alias}.revenue"),
                [],
                |row| row.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(revenue, 1, "the revenue is carried over too");

    // The old file is still there, under its own version number.
    let backup = db::versioned_backup_path(&path, 11);
    assert!(backup.exists(), "the old file must be kept as {}", backup.display());

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
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

        // The old table is gone — the facts live in the sites' files now, and
        // the replaced file has no site to attach.
        let old_table: i64 = conn.query_row(
            "SELECT count(*) FROM information_schema.tables \
             WHERE table_catalog = 'sumimi' AND table_name = 'event'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(old_table, 0, "the old fact table must not be carried over");

        let websites: i64 = conn.query_row("SELECT count(*) FROM website", [], |row| row.get(0))?;
        assert_eq!(websites, 0, "the registrations are not carried over either");

        // The new schema is the one the code expects: the facts are in the
        // sites' files, so the columns are checked against the site's schema.
        let probe = path.parent().unwrap().join("probe.duckdb");
        let _ = std::fs::remove_file(&probe);
        db::attach_site(conn, &probe, "w-probe")?;
        let visit_id: i64 = conn.query_row(
            "SELECT count(*) FROM duckdb_columns()
             WHERE table_name = 'website_event' AND column_name = 'visit_id'
               AND data_type = 'UUID'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(visit_id, 1, "the visit id is sixteen bytes now");

        let visitor_id: i64 = conn.query_row(
            "SELECT count(*) FROM duckdb_columns()
             WHERE table_name = 'website_event' AND column_name = 'session_id'
               AND data_type = 'UUID'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(
            visitor_id, 1,
            "the visitor id is stored again, as sixteen bytes, under Umami's name"
        );

        // The fact table carries no primary key — the ids are handed out.
        assert_eq!(primary_keys(conn, "website_event"), 0, "the index must be gone");

        Ok::<_, anyhow::Error>(())
    })
    .unwrap();

    // The old file is still there, under its own version number.
    let backup = db::versioned_backup_path(&path, 6);
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
            // The old table is gone — the facts live in the sites' files now.
            let rows: i64 = conn.query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'event'",
                [],
                |row| row.get(0),
            )?;
            Ok((version, rows))
        })
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION.to_string(), "the version must be current");
    assert_eq!(rows, 0, "the old rows are not carried over");

    let backup = db::versioned_backup_path(&path, 6);
    assert!(backup.exists(), "the old file must be kept as {}", backup.display());

    let _ = std::fs::remove_dir_all(&dir);
}
