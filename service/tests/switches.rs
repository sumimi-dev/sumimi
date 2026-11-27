//! The switches — the site's own setting over the instance's default.
//!
//! **A switch is tri-state**: absent (the instance's default), on, off. The
//! site says which, and the collection path obeys it.

use axum::body::Body;
use axum::http::Request;
use sumimi_service::api::{self, AppState};
use sumimi_service::config::Config;
use sumimi_service::pool::Db;
use sumimi_service::writer::Writer;
use tower::ServiceExt;

/// A unique temporary database path.
fn temp_db(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("sumimi-switches-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("sumimi.duckdb")
}

/// Stops the writer thread when the test ends — **even when it panics**.
struct WriterGuard(sumimi_service::writer::Writer);

impl Drop for WriterGuard {
    fn drop(&mut self) {
        self.0.shutdown();
    }
}

/// The application, with one website — **and the settings it carries**.
fn app(
    name: &str,
    settings: &str,
    tweak: impl FnOnce(&mut Config),
) -> (axum::Router, std::sync::Arc<Db>, WriterGuard) {
    let path = temp_db(name);
    let db = std::sync::Arc::new(Db::open(&path, "256MB").unwrap());
    db.with_write(|conn| {
        conn.execute(
            "INSERT INTO website (id, name, domain, created_at, updated_at, retention_days, country_header, settings)
             VALUES ('website-1', 'Example', 'example.com', now(), now(), 0, NULL, ?)",
            [settings],
        )?;
        Ok(())
    })
    .unwrap();
    db.ensure_site("website-1").unwrap();

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
    tweak(&mut config);

    let (writer, _handle) = Writer::spawn_with(
        db.clone(),
        std::time::Duration::from_millis(200),
        64,
        100,
    );
    let guard = WriterGuard(writer.clone());
    let state = AppState::new(
        db.clone(),
        std::sync::Arc::new(sumimi_service::instance::SqliteInstance::open_in_memory().unwrap()),
        writer,
        std::sync::Arc::new(config),
        std::sync::Arc::new("test-secret".to_string()),
    );
    (api::router(state), db, guard)
}

/// Wait until the writer has written what it was given.
async fn written(db: &Db, sql: &str) -> i64 {
    for _ in 0..100 {
        let count = db
            .with_read(|conn| {
                let mut stmt = conn.prepare(sql)?;
                let mut rows = stmt.query([])?;
                match rows.next()? {
                    Some(row) => Ok(row.get::<_, i64>(0)?),
                    None => Ok(0),
                }
            })
            .unwrap();
        if count > 0 {
            return count;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    0
}

/// The site's facts, in the site's own file.
fn facts(sql: &str) -> String {
    let alias = sumimi_service::db::site_alias("website-1");
    sql.replace("website_event", &format!("{alias}.website_event"))
}

/// The token the control plane needs.
fn token() -> String {
    sumimi_service::auth::issue_token("test-secret", "admin", "admin", "admin").unwrap()
}

/// The request the Umami SDK sends.
fn request(body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/send")
        .header("content-type", "application/json")
        .header("x-umami-website-id", "website-1")
        .header("x-umami-hostname", "example.com")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// A hit that carries everything a switch can turn off.
fn hit() -> String {
    serde_json::json!({
        "type": "event",
        "payload": {
            "url": "https://example.com/post?utm_source=news&gclid=abc123",
            "referrer": "https://news.example.com/item?id=7",
            "title": "The page",
            "name": "pageview",
            "hostname": "example.com",
            "language": "en-US",
            "screen": "1920x1080",
            "lcp": 1200.5,
            "inp": 80.0,
            "cls": 0.02,
            "fcp": 700.0,
            "ttfb": 90.0,
        }
    })
    .to_string()
}

/// The value of one column of the first row — **`None` when it is not set**.
fn column(db: &Db, name: &str) -> Option<String> {
    let sql = facts(&format!("SELECT {name} FROM website_event LIMIT 1"));
    db.with_read(|conn| {
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query([])?;
        match rows.next()? {
            Some(row) => Ok(row.get::<_, Option<String>>(0)?),
            None => Ok(None),
        }
    })
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_site_that_says_nothing_inherits_the_instance() {
    let (app, db, _guard) = app("inherit", "{}", |_| {});
    let response = app.oneshot(request(&hit())).await.unwrap();
    assert!(response.status().is_success());
    assert_eq!(written(&db, &facts("SELECT count(*) FROM website_event")).await, 1);

    // The instance's defaults: the title is kept, the click ids are not
    // (D-064's default), the web vitals are kept.
    assert_eq!(column(&db, "page_title").as_deref(), Some("The page"));
    assert_eq!(column(&db, "gclid"), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_site_can_turn_the_click_ids_off() {
    let (app, db, _guard) = app("click-ids", r#"{"store_click_ids": false}"#, |config| {
        // The instance stores them — the site is the one that says no.
        config.store_click_ids = true;
    });
    let response = app.oneshot(request(&hit())).await.unwrap();
    assert!(response.status().is_success());
    assert_eq!(written(&db, &facts("SELECT count(*) FROM website_event")).await, 1);

    assert_eq!(column(&db, "gclid"), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_site_can_turn_the_click_ids_on() {
    let (app, db, _guard) = app("click-ids-on", r#"{"store_click_ids": true}"#, |config| {
        config.store_click_ids = false;
    });
    let response = app.oneshot(request(&hit())).await.unwrap();
    assert!(response.status().is_success());
    assert_eq!(written(&db, &facts("SELECT count(*) FROM website_event")).await, 1);

    assert_eq!(column(&db, "gclid").as_deref(), Some("abc123"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_site_can_turn_the_web_vitals_off() {
    let (app, db, _guard) = app("vitals", r#"{"store_web_vitals": false}"#, |_| {});
    let response = app.oneshot(request(&hit())).await.unwrap();
    assert!(response.status().is_success());
    assert_eq!(written(&db, &facts("SELECT count(*) FROM website_event")).await, 1);

    assert_eq!(column(&db, "lcp"), None);
    assert_eq!(column(&db, "inp"), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_site_can_turn_the_title_off() {
    let (app, db, _guard) = app("titles", r#"{"store_titles": false}"#, |config| {
        config.store_titles = true;
    });
    let response = app.oneshot(request(&hit())).await.unwrap();
    assert!(response.status().is_success());
    assert_eq!(written(&db, &facts("SELECT count(*) FROM website_event")).await, 1);

    assert_eq!(column(&db, "page_title"), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_settings_survive_a_round_trip() {
    let (app, db, _guard) = app("round-trip", r#"{"store_click_ids": false}"#, |_| {});

    // The site is asked what its settings are.
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/websites/website-1")
                .header("authorization", format!("Bearer {}", token()))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(response.status().is_success());
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let answer: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(answer["settings"]["store_click_ids"], serde_json::json!(false));

    // And the row behind it says the same.
    let stored: Option<String> = db
        .with_read(|conn| {
            let mut stmt = conn.prepare("SELECT settings FROM website WHERE id = 'website-1'")?;
            let mut rows = stmt.query([])?;
            match rows.next()? {
                Some(row) => Ok(row.get::<_, Option<String>>(0)?),
                None => Ok(None),
            }
        })
        .unwrap();
    let stored: serde_json::Value =
        serde_json::from_str(stored.as_deref().expect("the settings are stored")).unwrap();
    assert_eq!(
        stored,
        serde_json::json!({"store_click_ids": false}),
        "the settings are stored as they were given"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_switch_that_is_not_ours_is_refused() {
    let (app, _db, _guard) = app("unknown", "{}", |_| {});

    let body = serde_json::json!({"settings": {"store_everything": true}}).to_string();
    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/websites/website-1")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {}", token()))
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        !response.status().is_success(),
        "a name that is not a switch must be refused"
    );
}
