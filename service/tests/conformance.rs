//! The conformance suite: the numbers a correct implementation must produce.
//!
//! The suite does not compare the engines with each other — it compares them
//! with **the truth**, which the dataset computes for itself in plain Rust
//! (see [`sumimi_service::testing::dataset`]). A test that only compares two
//! backends proves they agree; both could be wrong in the same way, and the
//! test would be green.
//!
//! ⇒ ⇒ This is the suite that caught the parenthesis bug in the reports' `WHERE`
//! clause: both engines had it, so every cross-backend test passed, and the
//! numbers were simply too big.

use std::path::PathBuf;

use sumimi_service::pool::Db;
use sumimi_service::sqlite::SqliteBackend;
use sumimi_service::testing;

/// How many visitors a day the dataset has. Small enough to run in seconds.
const VISITORS_PER_DAY: i64 = 40;

/// The open-source DuckDB backend.
#[test]
fn duckdb_conforms() {
    let path = temp_path("duckdb");
    let db = Db::open(&path, "256MB").unwrap();

    testing::run_all(&db, Some(&db), VISITORS_PER_DAY).unwrap();
}

/// The SQLite backend.
#[test]
fn sqlite_conforms() {
    let path = temp_path("sqlite");
    let backend = SqliteBackend::open(&path).unwrap();

    testing::run_all(&backend, Some(&backend), VISITORS_PER_DAY).unwrap();
}

/// A temporary database path.
fn temp_path(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sumimi-conformance-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("sumimi.duckdb")
}
