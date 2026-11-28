//! The seed gives the same data twice.
//!
//! **The promise the seed makes**: the same seed and the same clock give the
//! same traffic — the same visitors, the same pages, the same moments. This
//! test is what holds it to that.
//!
//! Two things it does not promise, and the test says so:
//!
//!   - **the visitor ids are the site's own** — they are derived from the
//!     site's id, so two fresh sites have two sets of them;
//!   - **nothing is in the future** — the newest row is at the clock, or
//!     before it.

use std::path::PathBuf;

use sumimi_cli::seed::{seed, SeedOptions};
use sumimi_service::pool::Db;
use sumimi_service::storage::{ReportQuery, Storage, TimeRange};

/// The clock every run in this test is pinned to.
fn clock() -> chrono::NaiveDateTime {
    chrono::NaiveDate::from_ymd_opt(2026, 11, 28)
        .unwrap()
        .and_hms_opt(6, 0, 0)
        .unwrap()
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sumimi-seed-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The rows a seeded database has: the moment, the page and the visit.
fn traffic(dir: &PathBuf, seed_value: u64, as_of: chrono::NaiveDateTime) -> Vec<(String, String, String)> {
    let db = Db::open(&dir.join("data.duckdb"), "128MB").unwrap();
    let options = SeedOptions {
        website: "Example".to_string(),
        domain: "example.com".to_string(),
        days: 5,
        visitors_per_day: 20,
        seed: seed_value,
        as_of,
        replace: true,
    };
    let (website_id, views, events, _) = seed(&db, &options).unwrap();
    assert!(views > 0 && events > 0);

    let range = TimeRange {
        start: as_of - chrono::Duration::days(30),
        end: as_of + chrono::Duration::days(1),
    };
    let query = ReportQuery::new(&website_id, range);
    let rows = db.recent_events(&query, 100_000).unwrap();
    rows.into_iter()
        .map(|row| {
            (
                row.created_at.to_string(),
                row.url.unwrap_or_default(),
                row.visit_id.to_string(),
            )
        })
        .collect()
}

#[test]
fn the_same_seed_and_the_same_clock_give_the_same_traffic() {
    let as_of = clock();
    let one = traffic(&temp_dir("one"), 4242, as_of);
    let two = traffic(&temp_dir("two"), 4242, as_of);

    assert!(!one.is_empty());
    assert_eq!(one.len(), two.len(), "the same seed gave a different number of rows");
    assert_eq!(one, two, "the same seed and the same clock gave different traffic");
}

#[test]
fn another_seed_gives_another_traffic() {
    let as_of = clock();
    let one = traffic(&temp_dir("three"), 4242, as_of);
    let two = traffic(&temp_dir("four"), 4243, as_of);

    // **The size moves too**: how many pages a visit has, and how many visits
    // a day has, are the seed's business.
    assert_ne!(one, two, "two seeds gave the same traffic");
}

#[test]
fn nothing_is_in_the_future() {
    let as_of = clock();
    let rows = traffic(&temp_dir("five"), 4242, as_of);
    assert!(!rows.is_empty());
    for (when, _, _) in &rows {
        let when = chrono::NaiveDateTime::parse_from_str(when, "%Y-%m-%d %H:%M:%S%.f").unwrap();
        assert!(
            when <= as_of,
            "a row is in the future: {when} after {as_of}"
        );
    }
}
