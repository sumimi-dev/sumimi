//! The compaction — the dead space given back.
//!
//! **DuckDB frees the blocks of a rewritten row group and never gives them
//! back to the filesystem** ⇒ ⇒ a file that has been written to for a while
//! is bigger than the data in it ⇒ ⇒ and the way to get the space back is to
//! write the data into a new file and swap it in.

use std::path::{Path, PathBuf};

use sumimi_service::db;
use sumimi_service::models::Hit;
use sumimi_service::pool::Db;
use sumimi_service::storage::Storage;

/// A temporary instance directory.
fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sumimi-compact-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A hit of the site's own — enough to make the file hold something.
fn hit(website_id: &str, index: i64) -> Hit {
    let at = chrono::NaiveDate::from_ymd_opt(2026, 1, 1)
        .unwrap()
        .and_hms_opt(0, 0, (index % 60) as u32)
        .unwrap();
    sumimi_service::testing::HitBuilder::new(
        website_id,
        &format!("session-{index}"),
        &format!("visitor-{index}"),
    )
    .at(at)
    .build()
}

/// The file of a site.
fn site_file(path: &Path, website_id: &str) -> PathBuf {
    db::site_path(path, website_id, "duckdb")
}

#[test]
fn the_compaction_gives_the_dead_space_back_and_keeps_the_rows() {
    let dir = temp_dir("site");
    let path = dir.join("sumimi.duckdb");
    let db = Db::open(&path, "256MB").unwrap();
    let website = db.create_website("Example", Some("example.com"), 0, None).unwrap();
    let website_id = website.id.clone();
    db.ensure_site(&website_id).unwrap();

    // **The traffic** — enough rows to make the file hold a few blocks.
    let hits: Vec<Hit> = (0..2_000).map(|index| hit(&website_id, index)).collect();
    for chunk in hits.chunks(100) {
        db.write_batch(chunk).unwrap();
    }
    // **The instance is closed** — the rows are in the write-ahead log until
    // the file is closed, and a file that holds only a log is not the file the
    // operator sees.
    drop(db);
    let before = site_file(&path, &website_id);
    let (on_disk, used) = db::file_bytes(&before).unwrap();
    assert!(on_disk > 0, "the file is there");

    // **The copy, and the swap.**
    let conn = duckdb::Connection::open(&before).unwrap();
    let compacted = db::compact_to_new_file(&before, &conn).unwrap();
    assert!(compacted.exists(), "the copy is beside the file");
    drop(conn);
    db::swap_in_compacted(&before).unwrap();
    assert!(!compacted.exists(), "the copy is gone — it was swapped in");

    let (after, after_used) = db::file_bytes(&before).unwrap();
    assert!(
        after <= on_disk,
        "the file is not bigger after the compaction ({after} against {on_disk})"
    );
    assert!(
        after_used >= used - 262_144,
        "the data is still there ({after_used} against {used})"
    );

    // **The rows** — the same ones.
    let count: i64 = duckdb::Connection::open(&before)
        .unwrap()
        .query_row("SELECT count(*) FROM website_event", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, hits.len() as i64, "every row survived");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_main_file_is_compacted_the_same_way() {
    let dir = temp_dir("main");
    let path = dir.join("sumimi.duckdb");
    let db = Db::open(&path, "256MB").unwrap();
    let website = db.create_website("Example", Some("example.com"), 0, None).unwrap();
    drop(db);

    let (on_disk, used) = db::file_bytes(&path).unwrap();
    assert!(on_disk > 0 && used > 0, "the file holds the site");

    let conn = duckdb::Connection::open(&path).unwrap();
    db::compact_to_new_file(&path, &conn).unwrap();
    drop(conn);
    db::swap_in_compacted(&path).unwrap();

    // **The site is still there** — the copy carries the registration over.
    let count: i64 = duckdb::Connection::open(&path)
        .unwrap()
        .query_row("SELECT count(*) FROM website WHERE id = ?", [&website.id], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 1, "the site survived the compaction");

    let _ = std::fs::remove_dir_all(&dir);
}
