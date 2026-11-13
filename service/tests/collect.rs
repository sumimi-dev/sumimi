//! The collection protocol: the shape of the request, the shape of the answer,
//! and the token the client carries.
//!
//! **The Umami SDK has to work against us as it is.** That is the point of
//! these tests: the request Umami sends is accepted, the answer Umami reads is
//! there, and the token it carries back is the one we signed.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use sumimi_service::api::{self, AppState};
use sumimi_service::config::Config;
use sumimi_service::pool::Db;
use sumimi_service::writer::Writer;
use tower::ServiceExt;

/// A unique temporary database path.
fn temp_db(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("sumimi-collect-{name}-{}", std::process::id()));
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

/// The application, with one website registered.
///
/// The writer's guard comes back with it: the thread has to live as long as
/// the test, and stop when the test ends.
fn app(name: &str) -> (axum::Router, std::sync::Arc<Db>, std::path::PathBuf, WriterGuard) {
    let path = temp_db(name);
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

    // A short flush interval: the tests look at what was written.
    let (writer, _handle) = Writer::spawn_with(db.clone(), std::time::Duration::from_millis(200), 64, 100);
    // The writer's thread has to be stopped when the test ends.
    let guard = WriterGuard(writer.clone());

    let state = AppState::new(
        db.clone(),
        std::sync::Arc::new(sumimi_service::instance::SqliteInstance::open_in_memory().unwrap()),
        writer,
        std::sync::Arc::new(config),
        std::sync::Arc::new("test-secret".to_string()),
    );
    (api::router(state), db, path, guard)
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

/// The body of a request, as a string.
async fn body_of(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    String::from_utf8_lossy(&bytes).to_string()
}

/// The request the Umami SDK sends: `{type, payload}` and the three headers.
fn umami_request(body: &str, cache: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/api/send")
        .header("content-type", "application/json")
        .header("x-umami-website-id", "website-1")
        .header("x-umami-hostname", "example.com");
    if let Some(cache) = cache {
        builder = builder.header("x-umami-cache", cache);
    }
    builder.body(Body::from(body.to_string())).unwrap()
}

/// The answer, and the three fields of it.
#[derive(Debug, serde::Deserialize)]
struct Answer {
    cache: String,
    #[serde(rename = "sessionId")]
    visitor_id: String,
    #[serde(rename = "visitId")]
    visit_id: String,
}

/// The cross-origin preflight is answered — otherwise the browser drops the
/// hit before it reaches us.
#[tokio::test(flavor = "multi_thread")]
async fn the_preflight_is_answered() {
    let (app, _db, path, _guard) = app("preflight");

    let request = Request::builder()
        .method("OPTIONS")
        .uri("/api/send")
        .header("origin", "https://example.com")
        .header("access-control-request-method", "POST")
        .header("access-control-request-headers", "content-type,x-umami-cache")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert!(
        response.status().is_success(),
        "the preflight has to be answered: {}",
        response.status()
    );
    let headers = response.headers();
    assert_eq!(
        headers
            .get("access-control-allow-origin")
            .and_then(|value| value.to_str().ok()),
        Some("*"),
        "the page on the other origin has to be allowed"
    );
    let allowed = headers
        .get("access-control-allow-headers")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_lowercase();
    assert!(
        allowed.contains("x-umami-cache"),
        "the token's header has to be allowed: {allowed}"
    );
    assert!(
        allowed.contains("x-sumimi-cache"),
        "our own header has to be allowed too: {allowed}"
    );

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// The answer carries the token, and the ids.
#[tokio::test(flavor = "multi_thread")]
async fn the_answer_carries_the_token() {
    let (app, _db, path, _guard) = app("answer");

    let response = app
        .oneshot(umami_request(
            r#"{"type":"event","payload":{"website":"website-1","url":"/one"}}"#,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let answer: Answer = serde_json::from_str(&body_of(response).await).unwrap();
    assert!(!answer.cache.is_empty(), "the token has to be there");
    assert_eq!(answer.visitor_id.len(), 36, "the visitor id is a UUID");
    assert_eq!(answer.visit_id.len(), 36, "the visit id is a UUID");

    // The token is ours: it carries the ids it is about.
    let claims = sumimi_service::token::verify("test-secret", &answer.cache).unwrap();
    assert_eq!(claims.website_id, "website-1");
    assert_eq!(
        claims.visitor(),
        Some(sumimi_service::ua::visitor_id(
            &sumimi_service::ua::visitor_address("0.0.0.0"),
            "",
            "website-1",
        )),
        "the visitor is the hash of the address and the agent"
    );
    assert_eq!(claims.session().map(|id| id.to_string()), Some(answer.visit_id));

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// The token keeps the visitor in the same session.
#[tokio::test(flavor = "multi_thread")]
async fn the_token_keeps_the_session() {
    let (app, _db, path, _guard) = app("token");

    let first = app
        .clone()
        .oneshot(umami_request(
            r#"{"type":"event","payload":{"website":"website-1","url":"/one"}}"#,
            None,
        ))
        .await
        .unwrap();
    let first: Answer = serde_json::from_str(&body_of(first).await).unwrap();

    // The next hit carries the token back: it is the same session.
    let second = app
        .clone()
        .oneshot(umami_request(
            r#"{"type":"event","payload":{"website":"website-1","url":"/two"}}"#,
            Some(&first.cache),
        ))
        .await
        .unwrap();
    let second: Answer = serde_json::from_str(&body_of(second).await).unwrap();

    assert_eq!(
        first.visit_id, second.visit_id,
        "the token keeps the visitor in the same session"
    );
    assert_eq!(first.visitor_id, second.visitor_id);

    // And the session's start is the one the first hit had — the start is a
    // fact, and it does not move.
    let claims = sumimi_service::token::verify("test-secret", &second.cache).unwrap();
    let first_claims = sumimi_service::token::verify("test-secret", &first.cache).unwrap();
    assert_eq!(
        claims.start, first_claims.start,
        "the session keeps the start it had"
    );

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// A token that is not ours is ignored — the hit starts a session of its own.
#[tokio::test(flavor = "multi_thread")]
async fn a_token_that_is_not_ours_is_ignored() {
    let (app, _db, path, _guard) = app("foreign");

    let response = app
        .oneshot(umami_request(
            r#"{"type":"event","payload":{"website":"website-1","url":"/one"}}"#,
            Some("not-a-token"),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let answer: Answer = serde_json::from_str(&body_of(response).await).unwrap();
    assert!(!answer.cache.is_empty(), "the client gets a token of its own");

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// Our own header wins over Umami's.
#[tokio::test(flavor = "multi_thread")]
async fn our_header_wins_over_umamis() {
    let (app, _db, path, _guard) = app("headers");

    let first = app
        .clone()
        .oneshot(umami_request(
            r#"{"type":"event","payload":{"website":"website-1","url":"/one"}}"#,
            None,
        ))
        .await
        .unwrap();
    let first: Answer = serde_json::from_str(&body_of(first).await).unwrap();

    // The token goes out in **our** header; Umami's carries somebody else's.
    let request = Request::builder()
        .method("POST")
        .uri("/api/send")
        .header("content-type", "application/json")
        .header("x-sumimi-cache", &first.cache)
        .header("x-umami-cache", "somebody-elses-token")
        .body(Body::from(
            r#"{"type":"event","payload":{"website":"website-1","url":"/two"}}"#,
        ))
        .unwrap();
    let second = app.clone().oneshot(request).await.unwrap();
    let second: Answer = serde_json::from_str(&body_of(second).await).unwrap();

    assert_eq!(
        first.visit_id, second.visit_id,
        "our header is the one that is read"
    );

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// The website can come in the header — the payload is not the only way.
#[tokio::test(flavor = "multi_thread")]
async fn the_website_can_come_in_the_header() {
    let (app, _db, path, _guard) = app("website-header");

    let request = Request::builder()
        .method("POST")
        .uri("/api/send")
        .header("content-type", "application/json")
        .header("x-umami-website-id", "website-1")
        .body(Body::from(r#"{"type":"event","payload":{"url":"/one"}}"#))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let answer: Answer = serde_json::from_str(&body_of(response).await).unwrap();
    assert!(!answer.cache.is_empty());

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// The web vitals are a hit of their own — and they are not a page view.
#[tokio::test(flavor = "multi_thread")]
async fn the_vitals_are_not_a_page_view() {
    let (app, db, path, _guard) = app("performance");

    let response = app
        .clone()
        .oneshot(umami_request(
            r#"{"type":"performance","payload":{"website":"website-1","url":"/one","lcp":"1200"}}"#,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let answer: Answer = serde_json::from_str(&body_of(response).await).unwrap();
    assert!(!answer.cache.is_empty(), "the client keeps its token");

    let vitals = written(&db, "SELECT count(*) FROM event WHERE event_type = 'performance'").await;
    let pageviews = written(&db, "SELECT count(*) FROM event WHERE event_type = 'pageview'").await;
    assert_eq!(vitals, 1, "the vitals are stored");
    assert_eq!(pageviews, 0, "the vitals are not a page view");

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// An error report is not an event — but the client still gets its token.
#[tokio::test(flavor = "multi_thread")]
async fn an_error_report_is_not_an_event() {
    let (app, db, path, _guard) = app("error");

    let response = app
        .clone()
        .oneshot(umami_request(
            r#"{"type":"error","payload":{"website":"website-1","url":"/one"}}"#,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let answer: Answer = serde_json::from_str(&body_of(response).await).unwrap();
    assert!(!answer.cache.is_empty(), "the client keeps its token");

    // Nothing is written — not now, not later.
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    let rows = db
        .with_read(|conn| {
            let mut stmt = conn.prepare("SELECT count(*) FROM event")?;
            let mut rows = stmt.query([])?;
            match rows.next()? {
                Some(row) => Ok(row.get::<_, i64>(0)?),
                None => Ok(0),
            }
        })
        .unwrap();
    assert_eq!(rows, 0, "an error report is not stored");

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// The Umami tracker sends the **whole** URL, so the origin comes off and the
/// path is what is kept.
#[tokio::test(flavor = "multi_thread")]
async fn the_url_is_resolved_the_way_umami_resolves_it() {
    let (app, db, path, _guard) = app("url");
    let website = "e5b404ff-d4e4-4abc-8ab1-0b5ed959e33d";

    let response = app
        .clone()
        .oneshot(umami_request(
            r#"{"type":"event","payload":{"website":"website-1","url":"http://example.com/blog/post?a=1#top"}}"#,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let stored = written(
        &db,
        "SELECT count(*) FROM event WHERE url = '/blog/post#top' AND url_query = 'a=1'",
    )
    .await;
    assert_eq!(stored, 1, "the path is kept, the origin is not");

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
    let _ = website;
}

/// A site is not a referrer to itself: the traffic of a site does not show up
/// in its own referrer breakdown.
#[tokio::test(flavor = "multi_thread")]
async fn a_self_referral_is_not_stored() {
    let (app, db, path, _guard) = app("self-referral");

    let response = app
        .clone()
        .oneshot(umami_request(
            r#"{"type":"event","payload":{"website":"website-1","url":"/one","hostname":"example.com","referrer":"https://example.com/other"}}"#,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let stored = written(&db, "SELECT count(*) FROM event WHERE referrer IS NULL").await;
    assert_eq!(stored, 1, "the self-referral is not a referrer");

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// The client may say who the visitor is — a server-side import knows the
/// visitor's address better than the server does.
#[tokio::test(flavor = "multi_thread")]
async fn the_client_may_say_who_the_visitor_is() {
    let (app, db, path, _guard) = app("identity");

    let response = app
        .clone()
        .oneshot(umami_request(
            r#"{"type":"event","payload":{"website":"website-1","url":"/one","ip":"198.51.100.7","userAgent":"Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X)","browser":"Firefox","os":"iOS","device":"mobile"}}"#,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // The client's own words about the visitor's **environment** are used.
    let stored = written(
        &db,
        "SELECT count(*) FROM event WHERE browser = 'Firefox' \
         AND os = 'iOS' AND device = 'mobile'",
    )
    .await;
    assert_eq!(stored, 1, "the client's own words about the visitor are used");

    // The raw address is **not** stored — not by default.
    let raw = written(&db, "SELECT count(*) FROM event WHERE ip = '198.51.100.7'").await;
    assert_eq!(raw, 0, "the raw address is not stored by default");

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// A bot is not a visitor: its hit is **stored and tagged**, not dropped.
#[tokio::test(flavor = "multi_thread")]
async fn a_bot_is_stored_not_dropped() {
    let (app, db, path, _guard) = app("bot");

    let response = app
        .clone()
        .oneshot(umami_request(
            r#"{"type":"event","payload":{"website":"website-1","url":"/one","userAgent":"Mozilla/5.0 (compatible; Googlebot/2.1)"}}"#,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // The hit is stored — the rows are there, they are just marked.
    let stored = written(&db, "SELECT count(*) FROM event").await;
    assert_eq!(stored, 1, "the bot's hit is stored");

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// A bot's hit is **tagged**, not dropped — and the tagged traffic is not part
/// of the numbers.
#[tokio::test(flavor = "multi_thread")]
async fn a_bot_is_tagged() {
    let (app, db, path, _guard) = app("bot-tag");

    let response = app
        .clone()
        .oneshot(umami_request(
            r#"{"type":"event","payload":{"website":"website-1","url":"/one","userAgent":"Mozilla/5.0 (compatible; Googlebot/2.1)"}}"#,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // The marker says **which** crawler it was — Googlebot is not the same
    // crawler as the one we do not know by name.
    let stored = written(&db, "SELECT count(*) FROM event WHERE tag = 'bot-se-google'").await;
    assert_eq!(stored, 1, "the bot's hit is stored, with the marker");

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// A crawler we do not know by name carries the plain `bot` marker.
#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_crawler_is_marked_bot() {
    let (app, db, path, _guard) = app("bot-generic");

    let response = app
        .clone()
        .oneshot(umami_request(
            r#"{"type":"event","payload":{"website":"website-1","url":"/one","userAgent":"SomeUnknownCrawler/1.0 (+http://example.org/bot)"}}"#,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let stored = written(&db, "SELECT count(*) FROM event WHERE tag = 'bot-other-unknown'").await;
    assert_eq!(stored, 1, "the crawler is marked as a bot");

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// The AI crawlers are marked apart: a site owner wants to know they came.
#[tokio::test(flavor = "multi_thread")]
async fn an_ai_crawler_is_marked_apart() {
    let (app, db, path, _guard) = app("bot-ai");

    let response = app
        .clone()
        .oneshot(umami_request(
            r#"{"type":"event","payload":{"website":"website-1","url":"/one","userAgent":"Mozilla/5.0 AppleWebKit/537.36 (KHTML, like Gecko); compatible; ChatGPT-User/1.0"}}"#,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let stored = written(&db, "SELECT count(*) FROM event WHERE tag = 'bot-ai-openai'").await;
    assert_eq!(stored, 1, "the AI crawler is marked apart");

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// The developer's own traffic is tagged with the id it was sent under.
#[tokio::test(flavor = "multi_thread")]
async fn the_developers_own_traffic_is_tagged() {
    let (app, db, path, _guard) = app("dev-tag");

    let response = app
        .clone()
        .oneshot(umami_request(
            r#"{"type":"event","payload":{"website":"website-1","url":"/one","id":"developer"}}"#,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let stored = written(&db, "SELECT count(*) FROM event WHERE tag = 'dev'").await;
    assert_eq!(stored, 1, "the developer's own traffic carries the marker");

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// The site's own tag is a **label**, not a marker: it does not change the
/// numbers.
#[tokio::test(flavor = "multi_thread")]
async fn the_sites_own_tag_is_only_a_label() {
    let (app, db, path, _guard) = app("label");

    let response = app
        .clone()
        .oneshot(umami_request(
            r#"{"type":"event","payload":{"website":"website-1","url":"/one","tag":"campaign"}}"#,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let stored = written(&db, "SELECT count(*) FROM event WHERE tag = 'campaign'").await;
    assert_eq!(stored, 1, "the label is stored as it is");

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}
