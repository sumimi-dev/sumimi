//! Integration tests for the write pipeline and the HTTP API.

use std::sync::Arc;
use std::time::Duration;

use sumimi_service::models::{Hit, HitKind};
use sumimi_service::pool::Db;
use sumimi_service::testing::HitBuilder;
use sumimi_service::writer::Writer;

/// A unique temporary database path.
fn temp_db(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("sumimi-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("sumimi.duckdb")
}

/// Stops the writer thread when the test ends — **even when it panics**.
///
/// The writer owns a blocking thread. If a test panics before it stops the
/// writer, the thread stays alive, the Tokio runtime never finishes and the
/// whole test binary hangs instead of failing. A guard makes that impossible.
struct WriterGuard(sumimi_service::writer::Writer);

impl Drop for WriterGuard {
    fn drop(&mut self) {
        self.0.shutdown();
    }
}

/// A synthetic hit — every field set, so the test can prove nothing is lost.
fn hit(website_id: &str, url: &str) -> Hit {
    HitBuilder::new(website_id, "session-1", "visitor-1")
        .url(url)
        .url_query("utm_source=twitter")
        .title("Example")
        .referrer("news.ycombinator.com")
        .referrer_name("Hacker News")
        .channel("Social")
        .referrer_path("/item")
        .referrer_query("?id=1")
        .hostname("example.com")
        .browser("Chrome")
        .browser_version("120")
        .os("macOS")
        .os_version("10.15")
        .device("desktop")
        .screen("1920x1080")
        .language("en-US")
        .country("DE")
        .utm_source("twitter")
        .utm_medium("social")
        .utm_campaign("spring")
        .gclid("gclid-1")
        .lcp(1200.0)
        .inp(80.0)
        .cls(0.02)
        .fcp(700.0)
        .ttfb(90.0)
        .build()
}

/// The writer must persist queued hits.
#[tokio::test(flavor = "multi_thread")]
async fn writer_flushes_queued_hits() {
    let path = temp_db("writer");
    let db = Arc::new(Db::open(&path, "256MB").unwrap());

    let (writer, _handle) = Writer::spawn(db.clone(), Duration::from_millis(200), 1000);
    let _guard = WriterGuard(writer.clone());
    writer.push(hit("website-1", "/one"));
    writer.push(hit("website-1", "/two"));

    // Wait for **both** hits: the writer may wake up between the two pushes and
    // write them in two batches, so "something was written" is not enough.
    let mut events = 0;
    for _ in 0..100 {
        events = db
            .with_read(|conn| {
                let mut stmt = conn.prepare("SELECT count(*) FROM event")?;
                let mut rows = stmt.query([])?;
                match rows.next()? {
                    Some(row) => Ok(row.get::<_, i64>(0)?),
                    None => Ok(0),
                }
            })
            .unwrap();
        if events >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    assert_eq!(events, 2, "the writer should have flushed both hits");

    // Nothing is lost: every field of the first hit is stored.
    let stored = db
        .with_read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT url, url_query, title, referrer, referrer_path, referrer_query, hostname,
                        browser, os, device, screen, language, country,
                        utm_source, utm_medium, utm_campaign, gclid,
                        lcp, inp, cls, fcp, ttfb, visit_id, visitor_id
                 FROM event WHERE url = '/one'",
            )?;
            let mut rows = stmt.query([])?;
            match rows.next()? {
                Some(row) => Ok(Some((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, Option<String>>(10)?,
                    row.get::<_, Option<String>>(11)?,
                    row.get::<_, Option<String>>(12)?,
                    row.get::<_, Option<String>>(13)?,
                    row.get::<_, Option<String>>(14)?,
                    row.get::<_, Option<String>>(15)?,
                    row.get::<_, Option<String>>(16)?,
                    row.get::<_, Option<f64>>(17)?,
                    row.get::<_, Option<f64>>(18)?,
                    row.get::<_, Option<f64>>(19)?,
                    row.get::<_, Option<f64>>(20)?,
                    row.get::<_, Option<f64>>(21)?,
                    row.get::<_, uuid::Uuid>(22)?,
                    row.get::<_, uuid::Uuid>(23)?,
                ))),
                None => Ok(None),
            }
        })
        .unwrap();

    let stored = stored.expect("the hit should be stored");
    assert_eq!(stored.0.as_deref(), Some("/one"));
    assert_eq!(stored.1.as_deref(), Some("utm_source=twitter"));
    assert_eq!(stored.2.as_deref(), Some("Example"));
    assert_eq!(stored.3.as_deref(), Some("news.ycombinator.com"));
    assert_eq!(stored.4.as_deref(), Some("/item"));
    assert_eq!(stored.5.as_deref(), Some("?id=1"));
    assert_eq!(stored.6.as_deref(), Some("example.com"));
    assert_eq!(stored.7.as_deref(), Some("Chrome"));
    assert_eq!(stored.8.as_deref(), Some("macOS"));
    assert_eq!(stored.9.as_deref(), Some("desktop"));
    assert_eq!(stored.10.as_deref(), Some("1920x1080"));
    assert_eq!(stored.11.as_deref(), Some("en-US"));
    assert_eq!(stored.12.as_deref(), Some("DE"));
    assert_eq!(stored.13.as_deref(), Some("twitter"));
    assert_eq!(stored.14.as_deref(), Some("social"));
    assert_eq!(stored.15.as_deref(), Some("spring"));
    assert_eq!(stored.16.as_deref(), Some("gclid-1"));
    assert_eq!(stored.17, Some(1200.0));
    assert_eq!(stored.18, Some(80.0));
    assert_eq!(stored.19, Some(0.02));
    assert_eq!(stored.20, Some(700.0));
    assert_eq!(stored.21, Some(90.0));
    assert_eq!(stored.22, sumimi_service::ua::uuid_from_text("session-1"));
    assert_eq!(stored.23, sumimi_service::ua::uuid_from_text("visitor-1"));

    writer.shutdown();
    // Stop the writer thread so the test runtime can shut down.
    writer.shutdown();

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// Custom events are stored in the event table.
#[tokio::test(flavor = "multi_thread")]
async fn writer_stores_custom_events() {
    let path = temp_db("events");
    let db = Arc::new(Db::open(&path, "256MB").unwrap());

    let (writer, _handle) = Writer::spawn(db.clone(), Duration::from_millis(200), 1000);
    let _guard = WriterGuard(writer.clone());

    let mut event = hit("website-1", "/signup");
    event.kind = HitKind::Event;
    event.name = Some("signup".to_string());
    event.data = Some("{\"plan\":\"pro\"}".to_string());
    writer.push(event);

    let mut events = 0;
    for _ in 0..100 {
        events = db
            .with_read(|conn| {
                let mut stmt = conn.prepare("SELECT count(*) FROM event")?;
                let mut rows = stmt.query([])?;
                match rows.next()? {
                    Some(row) => Ok(row.get::<_, i64>(0)?),
                    None => Ok(0),
                }
            })
            .unwrap();
        if events > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    assert_eq!(events, 1);

    writer.shutdown();
    // Stop the writer thread so the test runtime can shut down.
    writer.shutdown();

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// The API must be a thin wrapper around the shared query layer: the numbers it
/// returns have to be exactly the ones `reports` produces, and the shape has to stay
/// Umami-compatible.
#[tokio::test(flavor = "multi_thread")]
async fn api_matches_the_shared_report_layer() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use sumimi_service::api::{self, AppState};
    use sumimi_service::config::Config;
    use sumimi_service::pool::Db;
    use sumimi_service::storage::{ReportQuery, Storage, TimeRange};
    use tower::ServiceExt;

    let path = temp_db("api");
    let db = std::sync::Arc::new(Db::open(&path, "256MB").unwrap());

    let website_id = "website-1";
    db.with_write(|conn| {
        conn.execute(
            "INSERT INTO website (id, name, domain, created_at, updated_at, retention_days, country_header)
             VALUES ('website-1', 'Example', 'example.com', now(), now(), 0, NULL)",
            [],
        )?;
        // Three page views: /a twice in one session, /b once in another.
        for (id, session, visitor, url) in [
            (1i64, "s1", "v1", "/a"),
            (2, "s1", "v1", "/a"),
            (3, "s2", "v2", "/b"),
        ] {
            let session = sumimi_service::ua::uuid_from_text(session);
            let visitor = sumimi_service::ua::uuid_from_text(visitor);
            conn.execute(
                "INSERT INTO event (id, website_id, visit_id, visitor_id, created_at, url,
                        event_type, browser, os, device)
                 VALUES (?1, 'website-1', ?2, ?3, now(), ?4, 'pageview', 'Chrome', 'macOS', 'desktop')",
                duckdb::params![id, session, visitor, url],
            )?;
        }
        Ok(())
    })
    .unwrap();

    let mut config = Config::from_env_and_flags(
        sumimi_service::config::Paths::new(Some(path.clone().parent().unwrap().to_path_buf()), Some(path.clone()), "duckdb"),
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .unwrap();
    config.jwt_secret = "test-secret".to_string();

    let (writer, _handle) = sumimi_service::writer::Writer::spawn(
        db.clone(),
        std::time::Duration::from_millis(200),
        1000,
    );
    let _guard = WriterGuard(writer.clone());

    let state = AppState::new(
        db.clone(),
        std::sync::Arc::new(sumimi_service::instance::SqliteInstance::open_in_memory().unwrap()),
        writer.clone(),
        std::sync::Arc::new(config),
        std::sync::Arc::new("test-secret".to_string()),
    );

    let app = api::router(state);

    // Every route but the collector needs a token.
    let token = sumimi_service::auth::issue_token(
        "test-secret",
        "user-1",
        "admin",
        "admin",
    )
    .unwrap();

    // Without one, the request is refused.
    let anonymous = Request::builder()
        .uri("/api/websites/website-1/stats")
        .body(Body::empty())
        .unwrap();
    let refused = app.clone().oneshot(anonymous).await.unwrap();
    assert_eq!(
        refused.status(),
        StatusCode::UNAUTHORIZED,
        "a request without a token must be refused"
    );

    let request = Request::builder()
        .uri("/api/websites/website-1/stats")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let api_json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    // The same report, computed by the shared layer.
    let range = TimeRange {
        start: chrono::Utc::now().naive_utc() - chrono::Duration::days(1),
        end: chrono::Utc::now().naive_utc() + chrono::Duration::days(1),
    };
    let query = ReportQuery::new(website_id, range);
    let shared = db
        .summary_with_change(&query)
        .unwrap();

    // The Umami-compatible shape.
    for key in ["pageviews", "visitors", "views", "bounces", "visitDuration"] {
        assert!(api_json.get(key).is_some(), "missing metric {key}");
        assert!(api_json[key].get("value").is_some(), "{key} has no value");
        assert!(api_json[key].get("change").is_some(), "{key} has no change");
    }

    // The numbers agree.
    assert_eq!(
        api_json["pageviews"]["value"].as_i64().unwrap(),
        shared.pageviews.value
    );
    assert_eq!(
        api_json["visitors"]["value"].as_i64().unwrap(),
        shared.visitors.value
    );
    assert_eq!(api_json["views"]["value"].as_i64().unwrap(),
        shared.views.value
    );
    assert_eq!(
        api_json["bounces"]["value"].as_i64().unwrap(),
        shared.bounces.value
    );

    // Stop the writer thread so the test runtime can shut down.
    writer.shutdown();

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// The dashboard must see what the writer has just written.
///
/// **DuckDB snapshots the database when a connection is opened**, not when it
/// queries: a connection that was open before a write keeps showing the numbers
/// from before it — even after a checkpoint. A pooled reader therefore has to be
/// replaced when the data changes, or the dashboard would show yesterday's
/// numbers.
#[tokio::test(flavor = "multi_thread")]
async fn the_dashboard_sees_what_the_writer_wrote() {
    let path = temp_db("fresh");
    let db = Arc::new(Db::open(&path, "256MB").unwrap());

    // The reader is opened *before* the write — it is the one that would go
    // stale.
    let before = db
        .with_read(|conn| {
            let mut stmt = conn.prepare("SELECT count(*) FROM event")?;
            let mut rows = stmt.query([])?;
            match rows.next()? {
                Some(row) => Ok(row.get::<_, i64>(0)?),
                None => Ok(0),
            }
        })
        .unwrap();
    assert_eq!(before, 0);

    let (writer, _handle) = Writer::spawn(db.clone(), Duration::from_millis(100), 1000);
    let _guard = WriterGuard(writer.clone());
    writer.push(hit("website-1", "/one"));

    let mut after = 0;
    for _ in 0..100 {
        after = db
            .with_read(|conn| {
                let mut stmt = conn.prepare("SELECT count(*) FROM event")?;
                let mut rows = stmt.query([])?;
                match rows.next()? {
                    Some(row) => Ok(row.get::<_, i64>(0)?),
                    None => Ok(0),
                }
            })
            .unwrap();
        if after > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(after, 1, "the dashboard must see the hit that was just written");

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// The same, for the SQLite backend: its readers get a fresh snapshot on every
/// read, so a pooled connection is never stale.
#[tokio::test(flavor = "multi_thread")]
async fn the_sqlite_dashboard_sees_what_the_writer_wrote() {
    use sumimi_service::sqlite::SqliteBackend;
    use sumimi_service::storage::Storage;

    let dir = std::env::temp_dir().join(format!("sumimi-test-fresh-sqlite-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("data.sqlite");

    let backend = Arc::new(SqliteBackend::open(&path).unwrap());

    // The reader is opened before the write.
    let before = backend
        .with_reader(|conn| {
            let mut stmt = conn.prepare("SELECT count(*) FROM event")?;
            let mut rows = stmt.query([])?;
            match rows.next()? {
                Some(row) => Ok(row.get::<_, i64>(0)?),
                None => Ok(0),
            }
        })
        .unwrap();
    assert_eq!(before, 0);

    backend.write_batch(&[hit("website-1", "/one")]).unwrap();

    let after = backend
        .with_reader(|conn| {
            let mut stmt = conn.prepare("SELECT count(*) FROM event")?;
            let mut rows = stmt.query([])?;
            match rows.next()? {
                Some(row) => Ok(row.get::<_, i64>(0)?),
                None => Ok(0),
            }
        })
        .unwrap();
    assert_eq!(after, 1, "the dashboard must see the hit that was just written");

    let _ = std::fs::remove_dir_all(&dir);
}

/// The collector tells the client when the server is shedding.
///
/// The queue is full, so an **older** event is dropped to make room for the
/// new one. A `200` would say "everything is fine" while data is being lost —
/// the answer has to be a `503` with a `Retry-After`, so the client can slow
/// down.
#[tokio::test(flavor = "multi_thread")]
async fn the_collector_says_when_it_is_shedding() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use sumimi_service::api::{self, AppState};
    use sumimi_service::config::Config;
    use sumimi_service::pool::Db;
    use tower::ServiceExt;

    let path = temp_db("shed");
    let db = std::sync::Arc::new(Db::open(&path, "256MB").unwrap());
    db.with_write(|conn| {
        conn.execute(
            "INSERT INTO website (id, name, domain, created_at, updated_at, retention_days, country_header)
             VALUES ('website-1', 'Example', 'example.com', now(), now(), 0, NULL)",
            [],
        )?;
        Ok(())
    })
    .unwrap();

    let mut config = Config::from_env_and_flags(
        sumimi_service::config::Paths::new(
            Some(path.clone().parent().unwrap().to_path_buf()),
            Some(path.clone()),
            "duckdb",
        ),
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .unwrap();
    config.jwt_secret = "test-secret".to_string();

    // A queue of one: the second event pushes the first one out.
    let (writer, _handle) = Writer::spawn_with(
        db.clone(),
        std::time::Duration::from_secs(60),
        1,
        10,
    );
    let _guard = WriterGuard(writer.clone());

    // Fill the queue: the pushes are microseconds, the write is milliseconds,
    // so the queue stays full while the pushes go on.
    let fill = || {
        for _ in 0..200_000 {
            writer.push(hit("website-1", "/filler"));
        }
    };
    fill();
    assert!(writer.dropped() > 0, "the queue overflowed");
    assert!(
        writer.queued() >= 1,
        "the queue is full — the next event has to be refused"
    );

    let state = AppState::new(
        db.clone(),
        std::sync::Arc::new(sumimi_service::instance::SqliteInstance::open_in_memory().unwrap()),
        writer.clone(),
        std::sync::Arc::new(config),
        std::sync::Arc::new("test-secret".to_string()),
    );
    let app = api::router(state);

    let body = r#"{"website":"website-1","payload":{"url":"/one"}}"#;
    // The writer drains the queue between the pushes and the request, so the
    // queue is filled again for every attempt — the server is shedding either
    // way, and the answer has to be the same every time.
    let mut answer = None;
    for _ in 0..5 {
        fill();
        let request = Request::builder()
            .method("POST")
            .uri("/api/send")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
        answer = Some(app.clone().oneshot(request).await.unwrap());
    }
    let answer = answer.unwrap();

    assert_eq!(
        answer.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "the server is shedding — the client has to be told"
    );
    assert_eq!(
        answer
            .headers()
            .get(axum::http::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok()),
        Some("1"),
        "the client is told how long to wait"
    );

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// The heartbeat says **what machine this is** — and what this process is
/// using.
///
/// It is what makes a measurement portable: a load test that runs on another
/// machine can still say which machine was measured, because the server
/// says so itself.
#[tokio::test(flavor = "multi_thread")]
async fn the_heartbeat_says_what_machine_this_is() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use sumimi_service::api::{self, AppState};
    use sumimi_service::config::Config;
    use sumimi_service::pool::Db;
    use tower::ServiceExt;

    let path = temp_db("heartbeat");
    let db = std::sync::Arc::new(Db::open(&path, "256MB").unwrap());

    let mut config = Config::from_env_and_flags(
        sumimi_service::config::Paths::new(
            Some(path.clone().parent().unwrap().to_path_buf()),
            Some(path.clone()),
            "duckdb",
        ),
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .unwrap();
    config.jwt_secret = "test-secret".to_string();

    let (writer, _handle) = Writer::spawn(
        db.clone(),
        std::time::Duration::from_millis(200),
        1000,
    );
    let _guard = WriterGuard(writer.clone());

    let state = AppState::new(
        db.clone(),
        std::sync::Arc::new(sumimi_service::instance::SqliteInstance::open_in_memory().unwrap()),
        writer.clone(),
        std::sync::Arc::new(config),
        std::sync::Arc::new("test-secret".to_string()),
    );
    let app = api::router(state);

    let request = Request::builder()
        .uri("/api/heartbeat")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert!(
        json["machine"]["os"].is_string(),
        "the operating system is always known: {json}"
    );
    assert!(
        json["machine"]["arch"].is_string(),
        "the architecture is always known: {json}"
    );
    assert!(
        json["machine"]["cores"].as_u64().unwrap_or(0) >= 1,
        "the CPUs are always known: {json}"
    );
    assert!(
        json["rssBytes"].as_u64().unwrap_or(0) > 0,
        "a running process has a resident set: {json}"
    );

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}
