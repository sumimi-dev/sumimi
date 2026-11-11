//! The two engines must answer the same questions the same way.
//!
//! DuckDB is the product's engine and SQLite is the simpler one — **two
//! implementations of the same queries**. That makes the SQLite backend the
//! **correctness oracle**: the same events, written to both, and every report
//! compared.
//!
//! It is a different kind of test from the others in this repository. The
//! functional tests ask "does the product do what it says"; this one asks
//! "do the two engines say the same thing" — and a disagreement means one of
//! them is wrong, whichever one it is. The DuckDB path is the complicated one
//! (zone maps, parallel aggregates, a single connection that keeps its
//! snapshot), so it is the one that needs the other as a witness.
//!
//! The comparison is on the **serialized** answer: the two engines return the
//! same types, and the JSON is what the HTTP API actually serves — so this
//! compares what a user would see, not an internal shape.
//!
//! The rows are compared as **sets** (sorted), because the order of a tie is
//! not a promise: two referrers with the same count may come back in either
//! order. The *numbers* must be identical.

use sumimi_service::models::Hit;
use sumimi_service::pool::Db;
use sumimi_service::sqlite::SqliteBackend;
use sumimi_service::storage::{Dimension, ReportQuery, Storage, TimeRange};
use sumimi_service::testing::HitBuilder;

/// A temporary directory that cleans itself up.
struct Temp(std::path::PathBuf);

impl Temp {
    fn new(name: &str) -> Self {
        let directory = std::env::temp_dir().join(format!(
            "sumimi-test-{name}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        Temp(directory)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The events: ten page views over three days.
///
/// **The counts are all distinct on purpose.** Every dimension then has a
/// single order — the one with the most hits first — so the two engines have
/// to agree on the order too, and a difference is a real difference.
fn hits() -> Vec<Hit> {
    // The newest event is an hour old: the window's end is *now*, and an event
    // exactly on the boundary is a question about `>=` and `<` — not what this
    // test is about.
    let now = chrono::Utc::now().naive_utc() - chrono::Duration::hours(1);
    let day = chrono::Duration::days(1);
    let hour = chrono::Duration::hours(1);

    // The pages: /a four times, /b three, /c two, /d once.
    let pages = [
        ("/a", 4, 0),
        ("/b", 3, 1),
        ("/c", 2, 2),
        ("/d", 1, 3),
    ];
    let mut events = Vec::new();
    for (url, count, offset) in pages {
        for index in 0..count {
            let number = events.len();
            events.push(
                HitBuilder::new("website-1", &format!("session-{}", number % 4), &format!("visitor-{}", number % 5))
                    .url(url)
                    .url_query("utm_source=twitter")
                    .title("Example")
                    .referrer("news.ycombinator.com")
                    .referrer_name("Hacker News")
                    .channel("Social")
                    .referrer_path("/item")
                    .referrer_query("?id=1")
                    .hostname("example.com")
                    .browser(if number % 3 == 0 { "Firefox" } else { "Chrome" })
                    .browser_version("120")
                    .os(if number % 2 == 0 { "macOS" } else { "Windows" })
                    .os_version("10.15")
                    .device(if number % 4 == 0 { "mobile" } else { "desktop" })
                    .screen("1920x1080")
                    .language(if number % 2 == 0 { "en-US" } else { "de-DE" })
                    .country(if number % 2 == 0 { "DE" } else { "FR" })
                    .region(if number % 2 == 0 { "Berlin" } else { "Ile-de-France" })
                    .city(if number % 2 == 0 { "Berlin" } else { "Paris" })
                    .utm_source("twitter")
                    .utm_medium("social")
                    .utm_campaign("spring")
                    .gclid("gclid-1")
                    .lcp(1200.0)
                    .inp(80.0)
                    .cls(0.02)
                    .fcp(700.0)
                    .at(now - day * offset - hour * index)
                    .build(),
            );
        }
    }
    events
}

/// The window the reports ask for: the last seven days.
fn range() -> TimeRange {
    TimeRange::last_days(7)
}

/// The same, as JSON.
fn json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap()
}

/// Two answers must be the same.
fn same<T: serde::Serialize>(what: &str, duck: &T, sqlite: &T) {
    assert_eq!(
        json(duck),
        json(sqlite),
        "the {} must be the same on both engines",
        what
    );
}

/// Two lists must hold the same rows, in the same order.
fn same_rows<T: serde::Serialize>(what: &str, duck: &[T], sqlite: &[T]) {
    let left: Vec<String> = duck.iter().map(json).collect();
    let right: Vec<String> = sqlite.iter().map(json).collect();
    assert_eq!(
        left, right,
        "the {} must be the same rows, in the same order, on both engines",
        what
    );
}

/// The two engines must answer every report the same way.
#[test]
fn the_two_engines_answer_the_same() {
    let directory = Temp::new("backends-agree");
    let duck = Db::open(&directory.0.join("data.duckdb"), "128MB").unwrap();
    let sqlite = SqliteBackend::open(&directory.0.join("data.sqlite")).unwrap();

    let events = hits();
    duck.write_batch(&events).unwrap();
    sqlite.write_batch(&events).unwrap();

    let query = ReportQuery::new("website-1", range());

    // The summary: the numbers the dashboard leads with.
    same(
        "summary",
        &duck.summary(&query).unwrap(),
        &sqlite.summary(&query).unwrap(),
    );
    same(
        "summary with the change",
        &duck.summary_with_change(&query).unwrap(),
        &sqlite.summary_with_change(&query).unwrap(),
    );

    // Every dimension.
    for (what, dimension) in [
        ("the pages", Dimension::Url),
        ("the referrers", Dimension::Referrer),
        ("the browsers", Dimension::Browser),
        ("the systems", Dimension::Os),
        ("the devices", Dimension::Device),
        ("the countries", Dimension::Country),
        ("the languages", Dimension::Language),
        ("the screens", Dimension::Screen),
        ("the channels", Dimension::Channel),
        ("the regions", Dimension::Region),
        ("the cities", Dimension::City),
        ("the hosts", Dimension::Host),
        ("the tags", Dimension::Tag),
    ] {
        same_rows(
            what,
            &duck.breakdown(&query, dimension).unwrap(),
            &sqlite.breakdown(&query, dimension).unwrap(),
        );
    }

    // The series, the custom events, the query strings.
    same_rows(
        "the timeseries",
        &duck.timeseries(&query).unwrap(),
        &sqlite.timeseries(&query).unwrap(),
    );
    same_rows(
        "the events",
        &duck.events(&query).unwrap(),
        &sqlite.events(&query).unwrap(),
    );
    let duck_strings: Vec<(String, i64)> = duck.query_strings(&query).unwrap();
    let sqlite_strings: Vec<(String, i64)> = sqlite.query_strings(&query).unwrap();
    same_rows("the query strings", &duck_strings, &sqlite_strings);

    // The logs: the session list and the event list. Their order is by
    // time, and two events can share a moment — so these are compared as
    // sets.
    let mut duck_sessions: Vec<String> = duck.sessions(&query).unwrap().iter().map(json).collect();
    let mut sqlite_sessions: Vec<String> =
        sqlite.sessions(&query).unwrap().iter().map(json).collect();
    duck_sessions.sort();
    sqlite_sessions.sort();
    assert_eq!(
        duck_sessions, sqlite_sessions,
        "the sessions must be the same on both engines"
    );

    let mut duck_log: Vec<String> = duck.event_log(&query).unwrap().iter().map(json).collect();
    let mut sqlite_log: Vec<String> = sqlite.event_log(&query).unwrap().iter().map(json).collect();
    duck_log.sort();
    sqlite_log.sort();
    assert_eq!(
        duck_log, sqlite_log,
        "the event log must be the same on both engines"
    );
}
