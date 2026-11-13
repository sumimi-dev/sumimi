//! The marked traffic: the bots and the developer's own clicks.
//!
//! A **marker** is a tag the server writes itself — `bot` for the crawlers,
//! and the id the developer's own traffic was sent under. The marked traffic
//! is **not part of the numbers**, and the dashboard can ask for it.
//!
//! The site's own tags are **labels**, not markers: they group the events and
//! they change nothing in the numbers.

use sumimi_service::pool::Db;
use sumimi_service::storage::{ReportQuery, Storage, TimeRange};
use sumimi_service::testing::HitBuilder;

/// The window the reports ask about.
fn window() -> TimeRange {
    TimeRange::last_days(7)
}

/// A database with one plain hit, one bot hit and one developer hit.
fn database(name: &str) -> (Db, String) {
    let dir = std::env::temp_dir().join(format!("sumimi-marked-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = Db::open(&dir.join("sumimi.duckdb"), "256MB").unwrap();

    let website = db
        .create_website("Example", Some("example.com"), 0, None)
        .unwrap()
        .id;

    let now = chrono::Utc::now().naive_utc();
    let hits = vec![
        // A visitor: no marker.
        HitBuilder::new(&website, "s1", "visitor-1")
            .at(now)
            .url("/one")
            .build(),
        // A crawler we do not know by name: the marker is `bot-other-unknown`.
        HitBuilder::new(&website, "s2", "visitor-2")
            .at(now)
            .url("/one")
            .tag("bot-other-unknown")
            .build(),
        // A search engine: the marker is `bot-se-google`.
        HitBuilder::new(&website, "s5", "visitor-5")
            .at(now)
            .url("/one")
            .tag("bot-se-google")
            .build(),
        // The developer's own traffic: the marker is its kind.
        HitBuilder::new(&website, "s3", "visitor-3")
            .at(now)
            .url("/one")
            .tag("dev")
            .build(),
        // The site's own label: not a marker.
        HitBuilder::new(&website, "s4", "visitor-4")
            .at(now)
            .url("/one")
            .tag("campaign")
            .build(),
    ];
    db.write_batch(&hits).unwrap();

    (db, website)
}

/// The marked traffic is not part of the numbers — and the dashboard can ask
/// for it.
#[test]
fn the_marked_traffic_is_not_part_of_the_numbers() {
    let (db, website) = database("counted");

    let query = ReportQuery::new(&website, window());
    let summary = db.summary(&query).unwrap();
    // Three of the five hits are marked (the two crawlers and the developer)
    // ⇒ two are left: the plain visitor and the site's own label.
    assert_eq!(summary.pageviews, 2, "the marked traffic is not counted");
    assert_eq!(summary.visitors, 2, "nor are the marked visitors");

    // The dashboard's dropdown brings them back — **one kind at a time**.
    let mut query = ReportQuery::new(&website, window());
    query.include_markers = vec!["bot-other-unknown".to_string()];
    let summary = db.summary(&query).unwrap();
    assert_eq!(summary.pageviews, 3, "the crawler is back in the numbers");
    assert_eq!(summary.visitors, 3);

    // And the developer's own traffic on its own.
    let mut query = ReportQuery::new(&website, window());
    query.include_markers = vec!["dev".to_string()];
    let summary = db.summary(&query).unwrap();
    assert_eq!(summary.pageviews, 3, "the developer is back in the numbers");

    // A **group** stands for the kinds in it.
    let mut query = ReportQuery::new(&website, window());
    query.include_markers = vec!["bot-se".to_string()];
    let summary = db.summary(&query).unwrap();
    assert_eq!(summary.pageviews, 3, "the search engine is back in the numbers");

    // And everything at once — the groups and the kinds.
    let mut query = ReportQuery::new(&website, window());
    query.include_markers = vec![
        "bot-other".to_string(),
        "bot-se".to_string(),
        "dev".to_string(),
    ];
    let summary = db.summary(&query).unwrap();
    assert_eq!(summary.pageviews, 5, "all of them are back");
}

/// The logs are logs: they show what happened, marked or not.
#[test]
fn the_logs_keep_the_marked_traffic() {
    let (db, website) = database("logs");

    let query = ReportQuery::new(&website, window());
    let events = db.recent_events(&query, 100).unwrap();
    assert_eq!(events.len(), 5, "the session log shows everything");
}

/// A group stands for the kinds in it — picking the group picks all of them.
#[test]
fn a_group_stands_for_its_kinds() {
    use sumimi_service::storage::expand_markers;

    let group = expand_markers(&["bot-ai".to_string()]);
    assert_eq!(
        group,
        vec![
            "bot-ai-openai".to_string(),
            "bot-ai-anthropic".to_string(),
            "bot-ai-perplexity".to_string(),
            "bot-ai-apple".to_string(),
        ],
        "the group stands for its kinds"
    );

    // A kind stands for itself.
    assert_eq!(
        expand_markers(&["bot-se-google".to_string()]),
        vec!["bot-se-google".to_string()]
    );

    // A group with no kinds of its own is a kind.
    assert_eq!(expand_markers(&["dev".to_string()]), vec!["dev".to_string()]);

    // A name the server does not know stands for nothing — it must not widen
    // the numbers by accident.
    assert!(expand_markers(&["nonsense".to_string()]).is_empty());
}
