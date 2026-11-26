//! The properties: written as rows of their own, read back as the JSON the
//! API shows.
//!
//! The properties of an event live in a table of their own — one row per
//! property — so the log has to **pivot** them back: read the rows of the page
//! and put them into the events they belong to.
//!
//! **Both engines have to do it the same way.** The log's `data` is a JSON
//! string, and it is the same string in either engine — a text property is
//! quoted, a number is not, and an event without properties has none.

use std::path::PathBuf;

use sumimi_service::models::{HitKind, Property, PropertyValue};
use sumimi_service::pool::Db;
use sumimi_service::sqlite::SqliteBackend;
use sumimi_service::storage::{ReportQuery, Storage, TimeRange};
use sumimi_service::testing::HitBuilder;

/// A temporary database path.
fn temp_path(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sumimi-properties-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("sumimi.duckdb")
}

/// The window the log asks about.
fn window() -> TimeRange {
    TimeRange {
        start: chrono::Utc::now().naive_utc() - chrono::Duration::days(1),
        end: chrono::Utc::now().naive_utc() + chrono::Duration::days(1),
    }
}

/// The events: one with properties of both kinds, one without any.
fn events(website: &str) -> Vec<sumimi_service::models::Hit> {
    vec![
        HitBuilder::new(website, "s1", "v1")
            .kind(HitKind::Event)
            .name("signup")
            .url("/signup")
            .properties([
                Property {
                    key: "plan".to_string(),
                    value: PropertyValue::Text("pro".to_string()),
                },
                Property {
                    key: "seats".to_string(),
                    value: PropertyValue::Number(3.0),
                },
            ])
            .build(),
        HitBuilder::new(website, "s2", "v2").url("/plain").build(),
    ]
}

/// The log of a website, with the properties put back into it.
fn log(storage: &dyn Storage, website: &str) -> Vec<sumimi_service::models::EventRow> {
    let query = ReportQuery::new(website, window());
    storage.event_log(&query).unwrap()
}

/// **The properties come back as the JSON the API shows** — a text property is
/// quoted, a number is not.
#[test]
fn the_properties_come_back_as_json() {
    let path = temp_path("duckdb");
    let db = Db::open(&path, "256MB").unwrap();
    let website = db
        .create_website("Example", Some("example.com"), 0, None)
        .unwrap()
        .id;
    db.write_batch(&events(&website)).unwrap();

    let rows = log(&db, &website);
    assert_eq!(rows.len(), 2, "{rows:?}");

    // The event with the properties.
    let with = rows
        .iter()
        .find(|row| row.name.as_deref() == Some("signup"))
        .expect("the custom event is in the log");
    let data = with.data.as_deref().expect("the event has properties");
    assert_eq!(data, r#"{"plan":"pro","seats":3.0}"#, "{data}");

    // The event without any has none — the old column was null for it too.
    let without = rows
        .iter()
        .find(|row| row.name.is_none())
        .expect("the page view is in the log");
    assert_eq!(without.data, None, "{without:?}");
}

/// **The SQLite backend gives the same JSON** — the same string, byte for byte.
#[test]
fn the_two_engines_give_the_same_json() {
    let duck = Db::open(&temp_path("duckdb"), "256MB").unwrap();
    let sqlite = SqliteBackend::open(&temp_path("sqlite")).unwrap();

    let duck_site = duck
        .create_website("Example", Some("example.com"), 0, None)
        .unwrap()
        .id;
    let sqlite_site = sqlite
        .create_website("Example", Some("example.com"), 0, None)
        .unwrap()
        .id;

    duck.write_batch(&events(&duck_site)).unwrap();
    sqlite.write_batch(&events(&sqlite_site)).unwrap();

    let duck_data: Vec<Option<String>> = log(&duck, &duck_site)
        .into_iter()
        .map(|row| row.data)
        .collect();
    let sqlite_data: Vec<Option<String>> = log(&sqlite, &sqlite_site)
        .into_iter()
        .map(|row| row.data)
        .collect();

    assert_eq!(
        duck_data, sqlite_data,
        "the two engines must show the same properties"
    );
}

/// **The properties survive the round trip** — what goes in comes back, in the
/// same order, with the same types.
#[test]
fn the_properties_survive_the_round_trip() {
    let path = temp_path("duckdb");
    let db = Db::open(&path, "256MB").unwrap();
    let website = db
        .create_website("Example", Some("example.com"), 0, None)
        .unwrap()
        .id;
    let hits = events(&website);
    db.write_batch(&hits).unwrap();

    // The deep read hands back every stored row — the properties with it.
    let rows = sumimi_service::testing::DeepRead::rows(&db, &website).unwrap();
    let with = rows
        .iter()
        .find(|row| row.hit.name.as_deref() == Some("signup"))
        .expect("the custom event is in the rows");

    assert_eq!(
        with.hit.properties, hits[0].properties,
        "the properties must come back as they went in"
    );
}
