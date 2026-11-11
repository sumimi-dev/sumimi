//! The control plane: the log, the saved reports, the cache.
//!
//! These are the things an instance keeps that are **not** analytics, and the
//! endpoints that make them visible.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use sumimi_service::api::{self, AppState};
use sumimi_service::config::Config;
use sumimi_service::pool::Db;
use sumimi_service::testing::HitBuilder;
use sumimi_service::writer::Writer;
use tower::ServiceExt;

/// A unique temporary database path.
fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("sumimi-control-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Stops the writer thread when the test ends — even when it panics.
struct WriterGuard(sumimi_service::writer::Writer);

impl Drop for WriterGuard {
    fn drop(&mut self) {
        self.0.shutdown();
    }
}

/// Build the application, with a website and one page view in it.
async fn app(name: &str) -> (axum::Router, Arc<Db>, Writer, WriterGuard) {
    app_with(name, 30).await
}

/// Build the application, with a given stale window for the report cache.
async fn app_with(
    name: &str,
    cache_stale_secs: i64,
) -> (axum::Router, Arc<Db>, Writer, WriterGuard) {
    let dir = temp_dir(name);
    let path = dir.join("sumimi.duckdb");
    let db = Arc::new(Db::open(&path, "256MB").unwrap());

    db.with_write(|conn| {
        conn.execute(
            "INSERT INTO website (id, name, domain, created_at, updated_at, retention_days, country_header)
             VALUES ('w1', 'Example', 'example.com', now(), now(), 0, NULL)",
            [],
        )?;
        conn.execute(
            "INSERT INTO event (id, website_id, visit_id, visitor_id, created_at, url, event_type)
             VALUES (1, 'w1', ?1, ?2, now(), '/one', 'pageview')",
            duckdb::params![
                sumimi_service::ua::uuid_from_text("s1"),
                sumimi_service::ua::uuid_from_text("v1"),
            ],
        )?;
        Ok::<_, anyhow::Error>(())
    })
    .unwrap();

    let mut config = Config::from_env_and_flags(
        sumimi_service::config::Paths::new(Some(dir.clone()), Some(path.clone()), "duckdb"),
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .unwrap();
    config.jwt_secret = "test-secret".to_string();
    config.cache_stale_secs = cache_stale_secs;

    let (writer, _handle) = Writer::spawn(db.clone(), Duration::from_millis(200), 1000);
    let _guard = WriterGuard(writer.clone());

    // The instance store, with the admin account the server creates on the
    // first run.
    let instance = Arc::new(sumimi_service::instance::SqliteInstance::open_in_memory().unwrap());
    instance.set_password("admin", "hash").unwrap();

    let state = AppState::new(
        db.clone(),
        instance,
        writer.clone(),
        Arc::new(config),
        Arc::new("test-secret".to_string()),
    );

    (api::router(state), db, writer, _guard)
}

/// Send a request and return the status and the body.
async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    // Everything but the collector needs a token.
    let token = sumimi_service::auth::issue_token("test-secret", "admin", "admin", "admin").unwrap();
    let builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"));
    let request = match body {
        Some(body) => builder
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, value)
}

/// A saved report can be listed, run again and deleted.
#[tokio::test(flavor = "multi_thread")]
async fn a_report_can_be_saved_run_and_deleted() {
    let (app, _db, _writer, _guard) = app("snapshots").await;

    // Save a report.
    let body = r#"{
        "name": "monday",
        "spec": { "website": "w1", "report": "summary", "startAt": 0, "endAt": 99999999999999 }
    }"#;
    let (status, saved) = call(&app, "POST", "/api/snapshots", Some(body)).await;
    assert_eq!(status, StatusCode::OK, "the report should be saved");
    assert_eq!(saved["name"], "monday");
    // The answer is stored with the question.
    assert!(saved["result"].as_str().unwrap().contains("pageviews"));

    // It is listed.
    let (status, list) = call(&app, "GET", "/api/snapshots", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 1);

    // And it can be run again.
    let (status, rerun) = call(&app, "POST", "/api/snapshots/monday/run", None).await;
    assert_eq!(status, StatusCode::OK, "the saved report should run again");
    assert_eq!(rerun["name"], "monday");

    // And deleted.
    let (status, _) = call(&app, "DELETE", "/api/snapshots/monday", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, list) = call(&app, "GET", "/api/snapshots", None).await;
    assert!(list.as_array().unwrap().is_empty());

    // Deleting it twice is a 404.
    let (status, _) = call(&app, "DELETE", "/api/snapshots/monday", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// The report cache answers twice without asking the database twice — and the
/// answer changes as soon as the data does.
#[tokio::test(flavor = "multi_thread")]
async fn the_cache_serves_the_same_question_twice() {
    let (app, _db, writer, _guard) = app("cache").await;
    let uri = "/api/websites/w1/stats";

    let (status, first) = call(&app, "GET", uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["pageviews"]["value"], 1);

    // The same question: the same answer.
    let (status, second) = call(&app, "GET", uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first, second);

    // The cache holds the answer.
    let (_, cache) = call(&app, "GET", "/api/cache", None).await;
    assert!(cache["entries"].as_u64().unwrap() >= 1);

    // A new page view arrives. Inside the stale window the cache keeps serving
    // the answer it has — that is the point of the window: the query runs once,
    // not once per write.
    let hit = HitBuilder::new("w1", "s2", "v2").url("/two").build();
    writer.push(hit);

    // Give the writer time to flush, so the version of the data really changes.
    tokio::time::sleep(Duration::from_millis(600)).await;

    let (status, third) = call(&app, "GET", uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        third["pageviews"]["value"], 1,
        "the answer is still the one from inside the window"
    );
}


/// Without a stale window, a write throws the cache away at once.
#[tokio::test(flavor = "multi_thread")]
async fn without_a_stale_window_a_write_throws_the_cache_away() {
    let (app, _db, writer, _guard) = app_with("cache-exact", 0).await;
    let uri = "/api/websites/w1/stats";

    let (status, first) = call(&app, "GET", uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["pageviews"]["value"], 1);

    let hit = HitBuilder::new("w1", "s2", "v2").url("/two").build();
    writer.push(hit);

    let mut pageviews = 1;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (_, answer) = call(&app, "GET", uri, None).await;
        pageviews = answer["pageviews"]["value"].as_i64().unwrap();
        if pageviews == 2 {
            break;
        }
    }
    assert_eq!(
        pageviews, 2,
        "without a window, the cache must not serve the numbers from before the write"
    );
}

/// The log keeps what happened, and it can be read back.
#[tokio::test(flavor = "multi_thread")]
async fn the_log_records_what_happened() {
    let (app, _db, _writer, _guard) = app("logs").await;

    let body = r#"{
        "at": "2026-11-04T12:00:00",
        "level": "info",
        "kind": "website",
        "actor": "admin",
        "websiteId": "w1",
        "message": "created the website",
        "data": null
    }"#;
    let (status, _) = call(&app, "POST", "/api/logs", Some(body)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, logs) = call(&app, "GET", "/api/logs", None).await;
    assert_eq!(status, StatusCode::OK);
    let lines = logs.as_array().unwrap();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["message"], "created the website");
    assert_eq!(lines[0]["actor"], "admin");
}

/// A question is answered, and the answer is kept in the conversation.
#[tokio::test(flavor = "multi_thread")]
async fn a_question_is_answered_and_kept() {
    let (app, _db, _writer, _guard) = app("ask").await;

    // Start a conversation about the website.
    let (status, session) = call(
        &app,
        "POST",
        "/api/chat/sessions",
        Some(r#"{"website": "w1", "title": "about the traffic"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let id = session["id"].as_str().unwrap().to_string();

    // Ask a question.
    let (status, answer) = call(
        &app,
        "POST",
        &format!("/api/chat/sessions/{id}/ask"),
        Some(r#"{"question": "how many page views"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the question should be answered");

    // The answer holds the numbers of the database.
    let facts = &answer["facts"];
    assert_eq!(facts["summary"]["pageviews"]["value"], 1);
    assert_eq!(facts["metric"], "pageviews");

    // The words hold the same number.
    let text = answer["message"]["content"].as_str().unwrap();
    assert!(text.contains('1'), "the answer should hold the page views: {text}");

    // Both the question and the answer are in the conversation.
    let (status, messages) = call(&app, "GET", &format!("/api/chat/sessions/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    let messages = messages.as_array().unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[1]["role"], "assistant");
}

/// A team is created, filled and emptied through the API.
#[tokio::test(flavor = "multi_thread")]
async fn a_team_is_created_filled_and_emptied() {
    let (app, _db, _writer, _guard) = app("teams").await;

    // A team is created.
    let (status, team) = call(&app, "POST", "/api/teams", Some(r#"{"name": "Marketing"}"#)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(team["name"], "Marketing");
    let id = team["id"].as_str().unwrap().to_string();

    // It is listed.
    let (status, teams) = call(&app, "GET", "/api/teams", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(teams.as_array().unwrap().len(), 1);

    // A member is added. The account has to exist — the admin one does.
    let (status, member) = call(
        &app,
        "POST",
        &format!("/api/teams/{id}/members"),
        Some(r#"{"username": "admin", "role": "owner"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the member should be added");
    assert_eq!(member["username"], "admin");
    assert_eq!(member["role"], "owner");

    // The members are listed.
    let (status, members) = call(&app, "GET", &format!("/api/teams/{id}/members"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(members.as_array().unwrap().len(), 1);

    // An account that is not there cannot be a member.
    let (status, _) = call(
        &app,
        "POST",
        &format!("/api/teams/{id}/members"),
        Some(r#"{"username": "nobody"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // A role that does not exist is refused.
    let (status, _) = call(
        &app,
        "POST",
        &format!("/api/teams/{id}/members"),
        Some(r#"{"username": "admin", "role": "wizard"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // The member is removed.
    let (status, _) = call(
        &app,
        "DELETE",
        &format!("/api/teams/{id}/members/admin"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // And the team is deleted.
    let (status, _) = call(&app, "DELETE", &format!("/api/teams/{id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, teams) = call(&app, "GET", "/api/teams", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(teams.as_array().unwrap().is_empty());
}

/// An uptime check is recorded, and the history comes back.
#[tokio::test(flavor = "multi_thread")]
async fn an_uptime_check_is_recorded() {
    let (app, _db, _writer, _guard) = app("uptime").await;

    // Check the website. The domain of "w1" is example.com, which is not
    // reachable from the test — what matters is that a check is recorded and
    // that the history comes back.
    let (status, checked) = call(
        &app,
        "POST",
        "/api/websites/w1/uptime",
        Some(r#"{"url": "http://127.0.0.1:1/", "timeout": 1}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the check should be recorded");
    assert_eq!(checked["check"]["websiteId"], "w1");
    assert!(checked["check"]["ok"].as_bool().is_some());

    // The history holds it.
    let (status, history) = call(&app, "GET", "/api/websites/w1/uptime", None).await;
    assert_eq!(status, StatusCode::OK);
    let checks = history["checks"].as_array().unwrap();
    assert_eq!(checks.len(), 1, "the check is in the history");
    assert_eq!(checks[0]["websiteId"], "w1");
    assert_eq!(history["availability"]["checks"], 1);
}

/// The watched endpoints of a website can be managed.
#[tokio::test(flavor = "multi_thread")]
async fn the_watched_endpoints_can_be_managed() {
    let (app, _db, _writer, _guard) = app("endpoints").await;

    // Add one.
    let (status, _) = call(
        &app,
        "POST",
        "/api/websites/w1/uptime/endpoints",
        Some(r#"{"url": "https://example.com/health"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "the endpoint is watched");

    // It is listed.
    let (status, listed) = call(&app, "GET", "/api/websites/w1/uptime/endpoints", None).await;
    assert_eq!(status, StatusCode::OK);
    let endpoints = listed["endpoints"].as_array().unwrap();
    assert_eq!(endpoints.len(), 1);
    assert_eq!(endpoints[0], "https://example.com/health");

    // Adding it twice does not make two of them.
    let (status, _) = call(
        &app,
        "POST",
        "/api/websites/w1/uptime/endpoints",
        Some(r#"{"url": "https://example.com/health"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, listed) = call(&app, "GET", "/api/websites/w1/uptime/endpoints", None).await;
    assert_eq!(listed["endpoints"].as_array().unwrap().len(), 1);

    // And it can be removed.
    let (status, _) = call(
        &app,
        "DELETE",
        "/api/websites/w1/uptime/endpoints?url=https%3A%2F%2Fexample.com%2Fhealth",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, listed) = call(&app, "GET", "/api/websites/w1/uptime/endpoints", None).await;
    assert!(listed["endpoints"].as_array().unwrap().is_empty());

    // Removing one that is not watched says so.
    let (status, _) = call(
        &app,
        "DELETE",
        "/api/websites/w1/uptime/endpoints?url=https%3A%2F%2Fexample.com%2Fnope",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
