//! The service commands — `start`, `stop`, `status`.
//!
//! The supervisor is a second process, so this test drives the real binary: it
//! starts a service, asks the server a question over HTTP, kills the server to
//! see whether the supervisor brings it back, and stops the service again.
//!
//! Nothing here needs root, a system manager or a network: the port is a free
//! one on the loopback interface, and the instance directory is a temporary one.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// The binary under test.
fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_sumimi"))
}

/// A temporary directory for the instance.
fn instance(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "sumimi-service-test-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("creating the instance directory");
    path
}

/// A port nobody is listening on.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("binding a port");
    let port = listener.local_addr().expect("the local address").port();
    drop(listener);
    port
}

/// Run the binary with the instance directory.
fn run(instance: &Path, args: &[&str]) -> (bool, String) {
    let output = Command::new(binary())
        .args(args)
        .arg("--dir")
        .arg(instance)
        .output()
        .expect("running the binary");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.success(), text)
}

/// Whether the server answers on the port.
fn answers(port: u16) -> bool {
    let address = format!("127.0.0.1:{port}");
    let Ok(mut stream) = TcpStream::connect(&address) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let request = "GET /api/heartbeat HTTP/1.0\r\nHost: localhost\r\n\r\n";
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    response.starts_with("HTTP/1.1 200") || response.starts_with("HTTP/1.0 200")
}

/// Wait for a condition, up to the timeout.
fn wait_for(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

/// The service starts, serves, survives a kill and stops.
#[test]
fn the_service_starts_serves_and_stops() {
    let dir = instance("start");
    let port = free_port();

    // Not running yet.
    let (ok, text) = run(&dir, &["service", "status"]);
    assert!(ok, "status failed: {text}");
    assert!(text.contains("not running"), "unexpected status: {text}");

    // Start it.
    let (ok, text) = run(&dir, &["service", "start", "--port", &port.to_string()]);
    assert!(ok, "start failed: {text}");
    assert!(text.contains("started."), "unexpected start output: {text}");

    // It serves.
    assert!(
        wait_for(Duration::from_secs(30), || answers(port)),
        "the server never answered on port {port}"
    );

    // The status says so.
    let (ok, text) = run(&dir, &["service", "status"]);
    assert!(ok, "status failed: {text}");
    assert!(text.contains("running"), "unexpected status: {text}");
    assert!(
        text.contains(&format!("port:      {port}")),
        "the status does not mention the port: {text}"
    );

    // Starting it twice is refused.
    let (ok, text) = run(&dir, &["service", "start", "--port", &port.to_string()]);
    assert!(!ok, "a second start was allowed: {text}");
    assert!(text.contains("already running"), "unexpected text: {text}");

    // Stop it.
    let (ok, text) = run(&dir, &["service", "stop"]);
    assert!(ok, "stop failed: {text}");
    assert!(text.contains("stopped."), "unexpected stop output: {text}");

    // And it is gone.
    let (ok, text) = run(&dir, &["service", "status"]);
    assert!(ok, "status failed: {text}");
    assert!(text.contains("not running"), "unexpected status: {text}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// The supervisor brings the server back when it dies.
#[test]
fn the_supervisor_restarts_a_server_that_dies() {
    let dir = instance("restart");
    let port = free_port();

    let (ok, text) = run(&dir, &["service", "start", "--port", &port.to_string()]);
    assert!(ok, "start failed: {text}");
    assert!(
        wait_for(Duration::from_secs(30), || answers(port)),
        "the server never answered on port {port}"
    );

    // The pid of the server, from the state file.
    let state = std::fs::read_to_string(dir.join("service").join("status.json")).expect("the state file");
    let pid = state
        .lines()
        .find_map(|line| line.strip_prefix("  \"server_pid\": "))
        .and_then(|value| value.trim_end_matches(',').parse::<u32>().ok())
        .expect("the server pid in the state file");

    // Kill it the hard way — the way the OOM killer does.
    let killed = Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .status()
        .expect("killing the server");
    assert!(killed.success(), "could not kill the server");

    // The supervisor notices and starts a new one.
    assert!(
        wait_for(Duration::from_secs(30), || {
            let Ok(state) = std::fs::read_to_string(dir.join("service").join("status.json")) else {
                return false;
            };
            let pid = state
                .lines()
                .find_map(|line| line.strip_prefix("  \"server_pid\": "))
                .and_then(|value| value.trim_end_matches(',').parse::<u32>().ok());
            matches!(pid, Some(pid) if pid != 0)
                && state.contains("\"restarts\": 1")
        }),
        "the supervisor did not restart the server"
    );

    assert!(
        wait_for(Duration::from_secs(30), || answers(port)),
        "the server did not come back on port {port}"
    );

    let (ok, text) = run(&dir, &["service", "stop"]);
    assert!(ok, "stop failed: {text}");

    let _ = std::fs::remove_dir_all(&dir);
}
