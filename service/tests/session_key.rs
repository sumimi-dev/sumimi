//! The visitor id: the instance's key, the scope, and the word a site may set.

use axum::body::Body;
use axum::http::Request;
use sumimi_service::api::{self, AppState};
use sumimi_service::config::Config;
use sumimi_service::pool::Db;
use sumimi_service::writer::Writer;
use tower::ServiceExt;

/// A unique temporary database path.
fn temp_db(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("sumimi-key-{name}-{}", std::process::id()));
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
) -> (axum::Router, std::sync::Arc<Db>, WriterGuard, std::sync::Arc<Config>) {
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
    let config = std::sync::Arc::new(config);

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
        config.clone(),
        std::sync::Arc::new("test-secret".to_string()),
    );
    (api::router(state), db, guard, config)
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

/// The request the Umami SDK sends — **with the address and the agent spelled
/// out**, so the test knows what the visitor id was made of.
fn request() -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/send")
        .header("content-type", "application/json")
        .header("x-umami-website-id", "website-1")
        .header("x-umami-hostname", "example.com")
        .header("x-forwarded-for", "203.0.113.9")
        .header("user-agent", "Mozilla/5.0")
        .body(Body::from(
            serde_json::json!({
                "type": "event",
                "payload": {
                    "url": "https://example.com/post",
                    "hostname": "example.com",
                    "name": "pageview",
                }
            })
            .to_string(),
        ))
        .unwrap()
}

/// The visitor's id, as the row carries it — eight bytes of hex.
fn stored_id(db: &Db) -> Option<String> {
    let sql = facts("SELECT hex(session_id) FROM website_event LIMIT 1");
    db.with_read(|conn| {
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query([])?;
        match rows.next()? {
            // DuckDB writes the hex in capitals; the id is compared in lower case.
            Some(row) => Ok(row
                .get::<_, Option<String>>(0)?
                .map(|hex| hex.to_lowercase())),
            None => Ok(None),
        }
    })
    .unwrap()
}

/// The first eight bytes of an id, as hex — the width the column keeps.
fn eight_bytes(id: uuid::Uuid) -> String {
    id.as_bytes()[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_visitor_id_is_derived_with_the_instance_key() {
    let (app, db, _guard, config) = app("keyed", "{}");
    let response = app.oneshot(request()).await.unwrap();
    assert!(response.status().is_success());
    assert_eq!(written(&db, &facts("SELECT count(*) FROM website_event")).await, 1);

    // The id is the one the instance's key derives — **not** the keyless one.
    let expected = {
        let key = config
            .session_key
            .read()
            .unwrap_or_else(|e| e.into_inner());
        key.derive("website-1", "203.0.113.9", "Mozilla/5.0")
    };
    assert_eq!(stored_id(&db), Some(eight_bytes(expected)));

    // The keyless derivation is a different id — the two must not be confused.
    let pure = sumimi_service::session_key::pure_id("website-1", "203.0.113.9", "Mozilla/5.0");
    assert_ne!(eight_bytes(pure), eight_bytes(expected));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_site_may_name_the_scope_of_its_own() {
    let (app, db, _guard, config) = app("scope", r#"{"session_scope": "shared"}"#);
    let response = app.oneshot(request()).await.unwrap();
    assert!(response.status().is_success());
    assert_eq!(written(&db, &facts("SELECT count(*) FROM website_event")).await, 1);

    // The scope the site named is the one the id is derived in.
    let expected = {
        let key = config
            .session_key
            .read()
            .unwrap_or_else(|e| e.into_inner());
        key.derive("shared", "203.0.113.9", "Mozilla/5.0")
    };
    assert_eq!(stored_id(&db), Some(eight_bytes(expected)));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_scope_is_the_sites_own_by_default() {
    let (app, db, _guard, config) = app("default-scope", "{}");
    let response = app.oneshot(request()).await.unwrap();
    assert!(response.status().is_success());
    assert_eq!(written(&db, &facts("SELECT count(*) FROM website_event")).await, 1);

    // The site said nothing ⇒ the scope is the site's own id.
    let expected = {
        let key = config
            .session_key
            .read()
            .unwrap_or_else(|e| e.into_inner());
        key.derive("website-1", "203.0.113.9", "Mozilla/5.0")
    };
    assert_eq!(stored_id(&db), Some(eight_bytes(expected)));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_settings_take_a_word_and_refuse_a_flag() {
    let (app, _db, _guard, _config) = app("word", "{}");
    let token = sumimi_service::auth::issue_token("test-secret", "admin", "admin", "admin").unwrap();

    // A word is accepted.
    let body = serde_json::json!({"settings": {"session_scope": "my-own"}}).to_string();
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/websites/website-1")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {}", token))
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        response.status().is_success(),
        "a scope of the site's own is a setting like any other"
    );

    // A flag is not — the scope is a word, not a switch.
    let body = serde_json::json!({"settings": {"session_scope": true}}).to_string();
    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/websites/website-1")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {}", token))
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        !response.status().is_success(),
        "a flag where a word belongs must be refused"
    );
}
