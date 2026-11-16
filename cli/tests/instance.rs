//! The instance directory: what lives in it, and what the CLI finds there.
//!
//! The instance is a **directory**: the data file, the control plane and the
//! secret all live inside it. Every command has to agree on where that is —
//! and the CLI's `--dir` flag has to be the same answer the seed used, or the
//! CLI seeds one directory and reads another.

use std::path::PathBuf;

use sumimi_cli::source::Range;
use sumimi_cli::{backup, seed};
use sumimi_service::config::Paths;
use sumimi_service::pool::Db;
use sumimi_service::storage::{ReportQuery, Storage, TimeRange};

/// A unique temporary directory.
fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sumimi-instance-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The instance directory holds everything, and the CLI finds it by `--dir`.
#[test]
fn the_instance_directory_holds_the_data_and_the_cli_finds_it() {
    let dir = temp_dir("layout");
    let paths = Paths::new(Some(dir.clone()), None, "duckdb");

    assert_eq!(paths.data, dir.join("data.duckdb"), "the data file");
    assert_eq!(
        paths.instance,
        dir.join("instance.sqlite"),
        "the control plane"
    );
    assert_eq!(paths.secret, dir.join("jwt-secret"), "the secret");
    assert_eq!(paths.logs, dir.join("logs"), "the logs");
    assert_eq!(paths.acme, dir.join("acme"), "the certificates");

    // The SQLite backend puts its data file there too.
    let sqlite = Paths::new(Some(dir.clone()), None, "sqlite");
    assert_eq!(sqlite.data, dir.join("data.sqlite"));
}

/// A backup must contain the rows — not an empty database.
///
/// The backup reads the file **inside the instance directory**. When it looked
/// at the default path instead, it archived nothing and said so with a cheerful
/// "archived 0 rows" — which reads like a successful backup of an empty site.
#[test]
fn a_backup_of_an_instance_directory_contains_its_rows() {
    let dir = temp_dir("backup");
    let paths = Paths::new(Some(dir.clone()), None, "duckdb");

    let db = Db::open(&paths.data, "256MB").unwrap();
    let (website, views, _events) = seed::seed(
        &db,
        &seed::SeedOptions {
            website: "Example".to_string(),
            domain: "example.com".to_string(),
            days: 3,
            visitors_per_day: 50,
        },
    )
    .unwrap();
    assert!(views > 0, "the seed wrote something");

    // The backup.
    let archive = dir.join("archive");
    let stats = db
        .with_write(|conn| backup::export(conn, &archive, None, None, None, false))
        .unwrap();
    assert!(
        stats.rows >= views as i64,
        "the archive holds the rows: {} (the seed wrote {views})",
        stats.rows
    );
    assert!(stats.files > 0, "the archive holds files");

    // And the archive answers the same as the database.
    let range = TimeRange {
        start: chrono::DateTime::from_timestamp(0, 0)
            .map(|d| d.naive_utc())
            .unwrap(),
        end: chrono::DateTime::from_timestamp(4_000_000_000, 0)
            .map(|d| d.naive_utc())
            .unwrap(),
    };
    let hot = db.summary(&ReportQuery::new(&website, range)).unwrap();
    let cold = backup::open_archive(&format!("{}/", archive.display())).unwrap();
    let cold = cold.summary(&ReportQuery::new(&website, range)).unwrap();
    assert_eq!(hot.pageviews, cold.pageviews, "the same page views");
    assert_eq!(hot.visitors, cold.visitors, "the same visitors");
    assert_eq!(hot.views, cold.views, "the same sessions");
}

/// The CLI reads the facts of a site out of the site's own file.
///
/// **Since v12 the main file holds no facts at all** — they are in a file per
/// site, in a directory beside it. So a reader that opens only the main file
/// answers "0 page views" with the same confidence as a reader of a site that
/// really had none. ⇒ ⇒ The CLI has to find the site's file, and this is the
/// test that says it does.
#[test]
fn the_cli_reads_the_facts_out_of_the_sites_files() {
    let dir = temp_dir("read");
    let paths = Paths::new(Some(dir.clone()), None, "duckdb");

    let db = Db::open(&paths.data, "256MB").unwrap();
    let (website, views, _events) = seed::seed(
        &db,
        &seed::SeedOptions {
            website: "Example".to_string(),
            domain: "example.com".to_string(),
            days: 3,
            visitors_per_day: 50,
        },
    )
    .unwrap();
    assert!(views > 0, "the seed wrote something");

    // The facts are **not** in the main file — they are in the site's file, which
    // is attached under the site's own name. (The catalog says which is which:
    // the main file's catalog is `main_db`.)
    let main_rows: i64 = db
        .with_read(|conn| {
            Ok(conn.query_row(
                "SELECT count(*) FROM information_schema.tables \
                 WHERE table_name = 'website_event' AND table_catalog = 'main_db'",
                [],
                |row| row.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(main_rows, 0, "the main file holds no facts");

    // The site's file is there, beside it.
    let site_file = sumimi_service::db::site_path(&paths.data, &website, "duckdb");
    assert!(site_file.exists(), "the site's file: {}", site_file.display());

    // The CLI reads it — read-only.
    let source = sumimi_cli::source::Source::local(&paths.data).unwrap();
    let range = Range {
        start: chrono::DateTime::from_timestamp(0, 0)
            .map(|d| d.naive_utc())
            .unwrap(),
        end: chrono::DateTime::from_timestamp(4_000_000_000, 0)
            .map(|d| d.naive_utc())
            .unwrap(),
    };
    let report = source
        .report(
            sumimi_cli::source::Report::Summary,
            Some(&website),
            range,
            50,
        )
        .unwrap();
    assert_eq!(
        report["pageviews"]["value"].as_i64(),
        Some(views as i64),
        "the CLI sees the page views: {report}"
    );
}

/// The reader does not write.
///
/// The CLI opens the instance read-only, and the facts of a site are in a
/// file of that site's own — so **that file has to be read-only too**. The
/// check is the file's own: a file that was opened for writing is changed by
/// the reader, and the operator's data is not the CLI's to touch.
#[test]
fn the_cli_does_not_write_to_the_instance() {
    let dir = temp_dir("readonly");
    let paths = Paths::new(Some(dir.clone()), None, "duckdb");

    let db = Db::open(&paths.data, "256MB").unwrap();
    let (website, _views, _events) = seed::seed(
        &db,
        &seed::SeedOptions {
            website: "Example".to_string(),
            domain: "example.com".to_string(),
            days: 1,
            visitors_per_day: 10,
        },
    )
    .unwrap();

    let site_file = sumimi_service::db::site_path(&paths.data, &website, "duckdb");
    let before = std::fs::metadata(&site_file).unwrap().len();

    let source = sumimi_cli::source::Source::local(&paths.data).unwrap();
    let range = Range {
        start: chrono::DateTime::from_timestamp(0, 0)
            .map(|d| d.naive_utc())
            .unwrap(),
        end: chrono::DateTime::from_timestamp(4_000_000_000, 0)
            .map(|d| d.naive_utc())
            .unwrap(),
    };
    source
        .report(
            sumimi_cli::source::Report::Summary,
            Some(&website),
            range,
            50,
        )
        .unwrap();

    let after = std::fs::metadata(&site_file).unwrap().len();
    assert_eq!(before, after, "the reader did not change the site's file");
}

/// An instance of the old shape says so — it does not answer "0".
///
/// A main file without the sites' directory beside it holds **no facts**, and
/// a report over it would say "0 page views" with the same confidence as a
/// report over a site that really had none. ⇒ ⇒ So the shape is checked, and
/// an instance that cannot be read says what is wrong with it.
#[test]
fn an_instance_without_the_sites_files_says_so() {
    let dir = temp_dir("oldshape");
    let paths = Paths::new(Some(dir.clone()), None, "duckdb");

    // A main file with the sites and nothing else — the shape of v12 without
    // the site's file beside it.
    {
        let db = Db::open(&paths.data, "256MB").unwrap();
        db.with_write(|conn| {
            conn.execute(
                "INSERT INTO website (id, name, domain, created_at, updated_at, retention_days, country_header)
                 VALUES ('w1', 'Example', 'example.com', now(), now(), 0, NULL)",
                [],
            )?;
            Ok::<_, anyhow::Error>(())
        })
        .unwrap();
    }
    // The site's file is taken away.
    let site_file = sumimi_service::db::site_path(&paths.data, "w1", "duckdb");
    let _ = std::fs::remove_file(&site_file);
    let _ = std::fs::remove_dir_all(sumimi_service::db::sites_dir(&paths.data));

    let message = match sumimi_cli::source::Source::local(&paths.data) {
        Ok(_) => panic!("an instance without the site's file must not open"),
        Err(err) => format!("{err}"),
    };
    assert!(
        message.contains("facts") || message.contains("sites"),
        "the error says what is wrong: {message}"
    );
}

/// A snapshot holds the facts too.
///
/// **The snapshot is the fallback when a running server holds the file** — and
/// since v12 the facts are not in the file the server holds but in a file per
/// site, beside it. ⇒ ⇒ So a snapshot that copied only the main file would be
/// a snapshot of an **empty site**: the report would say "0 page views" and
/// the operator would believe it.
#[test]
fn a_snapshot_holds_the_facts_too() {
    let dir = temp_dir("snapshot");
    let paths = Paths::new(Some(dir.clone()), None, "duckdb");

    let db = Db::open(&paths.data, "256MB").unwrap();
    let (website, views, _events) = seed::seed(
        &db,
        &seed::SeedOptions {
            website: "Example".to_string(),
            domain: "example.com".to_string(),
            days: 3,
            visitors_per_day: 50,
        },
    )
    .unwrap();
    // The database is closed, so the copy is of a settled file.
    drop(db);

    // The snapshot.
    let snapshot = sumimi_cli::source::Source::snapshot(&paths.data).unwrap();

    // **The sites' directory came along**, and it kept its name — the site's
    // file is found through the **main file's** path.
    let sites = sumimi_service::db::sites_dir(&snapshot);
    assert!(sites.is_dir(), "the snapshot holds the sites' directory");
    let site_file = sumimi_service::db::site_path(&snapshot, &website, "duckdb");
    assert!(
        site_file.exists(),
        "the snapshot holds the site's file: {}",
        site_file.display()
    );

    // And the snapshot answers the same as the instance.
    let source = sumimi_cli::source::Source::local(&snapshot).unwrap();
    let range = Range {
        start: chrono::DateTime::from_timestamp(0, 0)
            .map(|d| d.naive_utc())
            .unwrap(),
        end: chrono::DateTime::from_timestamp(4_000_000_000, 0)
            .map(|d| d.naive_utc())
            .unwrap(),
    };
    let report = source
        .report(
            sumimi_cli::source::Report::Summary,
            Some(&website),
            range,
            50,
        )
        .unwrap();
    assert_eq!(
        report["pageviews"]["value"].as_i64(),
        Some(views as i64),
        "the snapshot holds the facts: {report}"
    );
}
