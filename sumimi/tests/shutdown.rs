//! The server, as a process: what happens when it is asked to stop.
//!
//! `systemctl stop sumimi` sends SIGTERM. The hits that are in the buffer at
//! that moment — the last couple of seconds of traffic — have to be on disk
//! when the process is gone. This is the only test that can prove it: it runs
//! the real executable, collects real traffic, sends the real signal and then
//! looks at the file.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A port nothing is listening on.
///
/// The port is picked by binding one and letting it go: a leftover server from
/// an earlier run would otherwise answer the requests and the test would be
/// measuring the wrong process.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

/// Kills the server when the test ends — **even when it panics**.
///
/// A server left behind holds its port, and the next run then talks to it
/// instead of to its own.
struct ChildGuard(std::process::Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A unique temporary directory.
fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sumimi-shutdown-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Wait for the server to answer on its port.
fn wait_for_port(port: u16, child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if let Ok(Some(status)) = child.try_wait() {
            panic!("the server exited early: {status}");
        }
        // A connection that succeeds means the listener is up.
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("the server did not start");
}

/// The server must not lose the traffic that was in the buffer when it stopped.
#[test]
fn the_server_writes_what_is_buffered_when_it_stops() {
    let dir = temp_dir("sigterm");
    let port = free_port();
    let password = "test-password-1234";

    // The server.
    let child = Command::new(env!("CARGO_BIN_EXE_sumimi"))
        .args([
            "serve",
            "--dir",
            dir.to_str().unwrap(),
            "--port",
            &port.to_string(),
        ])
        .env("SUMIMI_ADMIN_PASSWORD", password)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the server starts");
    let mut guard = ChildGuard(child);
    let child = &mut guard.0;
    wait_for_port(port, child);

    // A website and a hit.
    let token = login(port, password);
    let website = create_website(port, &token);

    // Collect a hit — and do not wait for it to be flushed.
    let sent = collect(port, &website);
    assert!(sent, "the hit was accepted");

    // Stop it the way systemd does.
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }

    // Wait for it to go.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut status: Option<std::process::ExitStatus> = None;
    while Instant::now() < deadline {
        if let Ok(Some(exit)) = child.try_wait() {
            status = Some(exit);
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let status = status.expect("the server did not stop");
    assert!(
        status.success(),
        "the server stopped cleanly: {status}"
    );

    // And the hit is in the file.
    let rows = count_rows(&dir.join("data.duckdb"), &website);
    assert_eq!(
        rows, 1,
        "the hit was written before the process left — the file holds {rows} rows"
    );
}

/// Log in and return the token.
fn login(port: u16, password: &str) -> String {
    let body = format!(
        r#"{{"username":"admin","password":"{password}"}}"#,
    );
    let response = post(port, "/api/login", &body, None);
    serde_json::from_str::<serde_json::Value>(&response)
        .expect("the login answer is JSON")
        .get("token")
        .and_then(|token| token.as_str())
        .expect("the login answer has a token")
        .to_string()
}

/// Create a website and return its id.
fn create_website(port: u16, token: &str) -> String {
    let response = post(
        port,
        "/api/websites",
        r#"{"name":"Test","domain":"test.example.com"}"#,
        Some(token),
    );
    serde_json::from_str::<serde_json::Value>(&response)
        .expect("the answer is JSON")
        .get("id")
        .and_then(|id| id.as_str())
        .expect("the answer has an id")
        .to_string()
}

/// Send a hit to the collector.
fn collect(port: u16, website: &str) -> bool {
    let body = format!(
        r#"{{"website":"{website}","payload":{{"url":"/hello","title":"Hello"}}}}"#
    );
    let response = post(port, "/api/send", &body, None);
    !response.contains("error")
}

/// A plain HTTP POST, without a client library.
fn post(port: u16, path: &str, body: &str, token: Option<&str>) -> String {
    use std::io::{Read, Write};

    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).expect("the server answers");
    let auth = match token {
        Some(token) => format!("authorization: Bearer {token}\r\n"),
        None => String::new(),
    };
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
         {auth}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).expect("the request goes out");
    let mut answer = String::new();
    stream.read_to_string(&mut answer).expect("the answer comes back");
    // Only the body — the headers are not interesting here.
    answer
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_string())
        .unwrap_or(answer)
}

/// How many rows the site's file holds.
///
/// ⇒ ⇒ Since v12 the facts of a site live in a file of that site's own, so the
/// rows are counted in **that** file.
fn count_rows(path: &std::path::Path, website_id: &str) -> i64 {
    use sumimi_service::db;
    use sumimi_service::pool::Db;

    let db = Db::open(path, "256MB").expect("the data file opens");
    let alias = db::site_alias(website_id);
    db.with_read(|conn| {
        let mut stmt =
            conn.prepare(&format!("SELECT count(*) FROM {alias}.website_event"))?;
        let mut rows = stmt.query([])?;
        match rows.next()? {
            Some(row) => Ok(row.get(0)?),
            None => Ok(0),
        }
    })
    .unwrap()
}

/// The server must be able to start again.
///
/// The instance directory is made private when the server starts — and a
/// **directory** needs the execute bit. With `0600` the directory could not be
/// entered, so the data file was unreachable and the second start failed with
/// "permission denied".
#[test]
fn the_server_can_start_twice() {
    let dir = temp_dir("restart");
    let password = "test-password-1234";

    for round in 1..=2 {
        let port = free_port();
        let child = Command::new(env!("CARGO_BIN_EXE_sumimi"))
            .args([
                "serve",
                "--dir",
                dir.to_str().unwrap(),
                "--port",
                &port.to_string(),
            ])
            .env("SUMIMI_ADMIN_PASSWORD", password)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("the server starts");
        let mut guard = ChildGuard(child);
        let child = &mut guard.0;
        wait_for_port(port, child);

        // The directory is private.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode, 0o700,
                "the instance directory is private to its owner (round {round})"
            );
        }

        // Stop it.
        unsafe {
            libc::kill(child.id() as i32, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut stopped = false;
        while Instant::now() < deadline {
            if let Ok(Some(_)) = child.try_wait() {
                stopped = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(stopped, "the server stopped (round {round})");
    }
}

/// When the server stops, the instance file holds everything.
///
/// The control plane is a SQLite database, and SQLite keeps the newest writes
/// in a write-ahead log next to it. A backup that copies only the database
/// would miss them — so the store puts everything back into the file when the
/// server stops.
#[test]
fn the_instance_file_holds_everything_when_the_server_stops() {
    let dir = temp_dir("instance");
    let port = free_port();
    let password = "test-password-1234";

    let child = Command::new(env!("CARGO_BIN_EXE_sumimi"))
        .args([
            "serve",
            "--dir",
            dir.to_str().unwrap(),
            "--port",
            &port.to_string(),
        ])
        .env("SUMIMI_ADMIN_PASSWORD", password)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the server starts");
    let mut guard = ChildGuard(child);
    let child = &mut guard.0;
    wait_for_port(port, child);

    // Log in, so the instance store has something in it.
    let token = login(port, password);
    assert!(!token.is_empty());

    // Stop it.
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut stopped = false;
    while Instant::now() < deadline {
        if let Ok(Some(_)) = child.try_wait() {
            stopped = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(stopped, "the server stopped");

    // The account is in the file itself, and the log is empty.
    let store = sumimi_service::instance::SqliteInstance::open(&dir.join("instance.sqlite"))
        .expect("the instance file opens");
    let account = store.account("admin").expect("the accounts are readable");
    assert!(account.is_some(), "the admin account is in the file");
    // The write-ahead log is empty: everything is in the file itself.
    let wal = dir.join("instance.sqlite-wal");
    if wal.exists() {
        let size = std::fs::metadata(&wal).unwrap().len();
        assert_eq!(size, 0, "the write-ahead log was emptied into the file");
    }
}
