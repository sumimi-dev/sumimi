//! Running Sumimi as a service — without systemd, without root.
//!
//! `sumimi service start` runs the server under a **supervisor**: a second process
//! that owns the server as a child process and restarts it when it dies. Two
//! processes, because a process that is killed by the kernel (the OOM killer, a
//! segfault) cannot restart itself, and the whole point of the promise is that a
//! traffic spike does not take the box with it.
//!
//! The design has three rules:
//!
//! * **no system manager.** Nothing here needs systemd, runit, s6 or an init
//!   script. The supervisor is the same executable, started with its own process
//!   group so that closing the terminal does not take it down.
//! * **no root.** Everything lives inside the instance directory, so a service can
//!   run as the user who installed it.
//! * **no signals.** The supervisor asks the server to stop by closing the pipe on
//!   its standard input; the server treats the end of that pipe as a request to
//!   shut down gracefully. A stop request is a file, and the state is a file too,
//!   so `status` works without any privileges at all.
//!
//! The layout inside the instance directory:
//!
//! ```text
//! <dir>/service/status.json   the state, rewritten every heartbeat
//! <dir>/service/log           the server's output
//! <dir>/service/stop          the stop request
//! ```

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// The directory inside the instance directory that holds the service's files.
const SERVICE_DIR: &str = "service";

/// The file the supervisor writes its state to.
const STATUS_FILE: &str = "status.json";

/// The file `sumimi service stop` creates.
const STOP_FILE: &str = "stop";

/// The file the server's output goes to.
const LOG_FILE: &str = "log";

/// How long the heartbeat may be stale before the service counts as stopped.
const HEARTBEAT_STALE_SECONDS: u64 = 10;

/// How often the heartbeat is written.
const HEARTBEAT_SECONDS: u64 = 2;

/// How long the supervisor waits for the server to exit after asking it to.
const STOP_TIMEOUT_SECONDS: u64 = 30;

/// How long the server has to stay up before its next exit stops counting as a
/// failure. A server that dies after ten minutes was working; a server that dies
/// after ten seconds is broken.
const HEALTHY_SECONDS: u64 = 10;

/// How many failures in a row the supervisor tolerates before it gives up.
const MAX_FAILURES: u32 = 5;

/// The first backoff, and the longest one.
const BACKOFF_FIRST: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// The arguments of `sumimi service start`.
#[derive(Debug, Clone, clap::Args)]
pub struct StartArgs {
    /// The domain to serve. A certificate is issued for it automatically by
    /// Let's Encrypt, and renewed in the background.
    #[arg(long, env = "SUMIMI_SSL_DOMAIN")]
    pub domain: Option<String>,
    /// The contact address registered with Let's Encrypt.
    #[arg(long, env = "SUMIMI_SSL_EMAIL")]
    pub email: Option<String>,
    /// The port the server listens on.
    #[arg(long, env = "SUMIMI_PORT")]
    pub port: Option<u16>,
    /// The address to bind to.
    #[arg(long, env = "SUMIMI_HOST")]
    pub host: Option<String>,
    /// The port used for HTTPS.
    #[arg(long, env = "SUMIMI_SSL_PORT")]
    pub ssl_port: Option<u16>,
}

/// The arguments of the supervisor — the process `start` spawns.
#[derive(Debug, Clone, clap::Args)]
pub struct SuperviseArgs {
    /// The file the state is written to.
    #[arg(long)]
    pub status_file: PathBuf,
    /// The file the stop request appears in.
    #[arg(long)]
    pub stop_file: PathBuf,
    /// The file the server's output goes to.
    #[arg(long)]
    pub log_file: PathBuf,
    /// The port the server listens on — for the report.
    #[arg(long)]
    pub port: Option<u16>,
    /// The domain the server serves — for the report.
    #[arg(long)]
    pub domain: Option<String>,
    /// The command the supervisor runs, after the `--`.
    #[arg(last = true)]
    pub command: Vec<String>,
}

/// The state of the service.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    /// The process that supervises the server.
    pub supervisor_pid: u32,
    /// The process that serves the traffic.
    pub server_pid: Option<u32>,
    /// `running`, `stopping` or `failed`.
    pub state: String,
    /// When the supervisor started, in seconds since the epoch.
    pub started_at: u64,
    /// When the heartbeat was last written — the liveness check.
    pub updated_at: u64,
    /// How many times the server has been restarted.
    pub restarts: u32,
    /// Why the server exited last.
    pub last_exit: Option<String>,
    /// The port the server listens on.
    pub port: Option<u16>,
    /// The domain the server serves.
    pub domain: Option<String>,
    /// The file the server's output goes to.
    pub log: PathBuf,
    /// The command the supervisor runs.
    pub command: Vec<String>,
}

/// The directory that holds the service's files, inside the instance directory.
fn service_dir(instance: &Path) -> PathBuf {
    instance.join(SERVICE_DIR)
}

/// The file the state is written to.
fn status_file(service: &Path) -> PathBuf {
    service.join(STATUS_FILE)
}

/// The file the stop request appears in.
fn stop_file(service: &Path) -> PathBuf {
    service.join(STOP_FILE)
}

/// The file the server's output goes to.
fn log_file(service: &Path) -> PathBuf {
    service.join(LOG_FILE)
}

/// The seconds since the epoch.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

/// Read the state, if there is any.
fn read_status(service: &Path) -> Option<Status> {
    let text = std::fs::read_to_string(status_file(service)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Whether the service is still alive: the heartbeat is fresh.
fn is_alive(status: &Status) -> bool {
    now().saturating_sub(status.updated_at) <= HEARTBEAT_STALE_SECONDS
}

/// Write the state, atomically — a reader never sees half a file.
fn write_status(path: &Path, status: &Status) -> Result<()> {
    let temporary = path.with_extension("tmp");
    let text = serde_json::to_string_pretty(status)?;
    std::fs::write(&temporary, text)
        .with_context(|| format!("writing {}", temporary.display()))?;
    std::fs::rename(&temporary, path)
        .with_context(|| format!("renaming to {}", path.display()))?;
    Ok(())
}

/// The server's command line, built from the arguments.
fn server_command(
    dir: &Path,
    db: Option<&Path>,
    backend: &str,
    args: &StartArgs,
) -> Vec<String> {
    let mut command = vec![
        "serve".to_string(),
        "--dir".to_string(),
        dir.display().to_string(),
        "--backend".to_string(),
        backend.to_string(),
    ];

    if let Some(db) = db {
        command.push("--db".to_string());
        command.push(db.display().to_string());
    }
    if let Some(host) = &args.host {
        command.push("--host".to_string());
        command.push(host.clone());
    }
    if let Some(port) = args.port {
        command.push("--port".to_string());
        command.push(port.to_string());
    }

    // The domain is what turns on TLS: the server asks Let's Encrypt for a
    // certificate and renews it in the background.
    if let Some(domain) = &args.domain {
        command.push("--ssl".to_string());
        command.push(domain.clone());
        if let Some(email) = &args.email {
            command.push("--ssl-email".to_string());
            command.push(email.clone());
        }
        if let Some(port) = args.ssl_port {
            command.push("--ssl-port".to_string());
            command.push(port.to_string());
        }
    }

    command
}

/// Start the service.
pub fn start(
    dir: &Path,
    db: Option<&Path>,
    backend: &str,
    args: &StartArgs,
) -> Result<()> {
    let service = service_dir(dir);
    std::fs::create_dir_all(&service)
        .with_context(|| format!("creating {}", service.display()))?;

    // A stale state file from a supervisor that died is not a running service.
    if let Some(status) = read_status(&service) {
        if is_alive(&status) {
            bail!(
                "already running (pid {}) — use `sumimi service stop` first",
                status.supervisor_pid
            );
        }
        let _ = std::fs::remove_file(status_file(&service));
    }
    let _ = std::fs::remove_file(stop_file(&service));

    let command = server_command(dir, db, backend, args);

    // The supervisor is this same executable, started in its own process group so
    // that closing the terminal does not take it down.
    let executable = std::env::current_exe().context("finding this executable")?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_file(&service))
        .with_context(|| format!("opening {}", log_file(&service).display()))?;

    let mut command_line = std::process::Command::new(executable);
    command_line
        .arg("service")
        .arg("supervise")
        .arg("--status-file")
        .arg(status_file(&service))
        .arg("--stop-file")
        .arg(stop_file(&service))
        .arg("--log-file")
        .arg(log_file(&service));
    if let Some(port) = args.port {
        command_line.arg("--port").arg(port.to_string());
    }
    if let Some(domain) = &args.domain {
        command_line.arg("--domain").arg(domain);
    }
    command_line
        .arg("--")
        .args(&command)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command_line.process_group(0);
    }

    command_line
        .spawn()
        .context("starting the supervisor")?;

    // Wait for the supervisor to say it is up.
    for _ in 0..100 {
        std::thread::sleep(Duration::from_millis(100));
        if let Some(status) = read_status(&service) {
            if is_alive(&status) {
                // The supervisor is up; the server needs a moment more to bind
                // its port. Waiting for the port is the honest answer to "is it
                // running?", and it fails loudly when it never binds.
                let port = listening_port(args);
                for _ in 0..100 {
                    if listening(port) {
                        report(&status, dir);
                        return Ok(());
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                report(&status, dir);
                println!(
                    "  note:       nothing is listening on port {port} yet — see the log",
                );
                return Ok(());
            }
        }
    }

    bail!(
        "the supervisor did not come up — see {}",
        log_file(&service).display()
    );
}

/// The port the server listens on: the HTTPS one when there is a domain.
fn listening_port(args: &StartArgs) -> u16 {
    if args.domain.is_some() {
        args.ssl_port.unwrap_or(443)
    } else {
        args.port.unwrap_or(3000)
    }
}

/// Whether something is listening on the port.
fn listening(port: u16) -> bool {
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    std::net::TcpStream::connect_timeout(&address, Duration::from_millis(200)).is_ok()
}

/// Stop the service.
pub fn stop(dir: &Path) -> Result<()> {
    let service = service_dir(dir);
    let status = read_status(&service).ok_or_else(|| {
        anyhow::anyhow!("not running — there is no service in {}", dir.display())
    })?;

    std::fs::write(stop_file(&service), b"stop\n")
        .with_context(|| format!("writing {}", stop_file(&service).display()))?;

    // The supervisor notices the request, closes the server's standard input and
    // waits for it to shut down.
    let deadline = now() + STOP_TIMEOUT_SECONDS;
    while now() < deadline {
        std::thread::sleep(Duration::from_millis(250));
        match read_status(&service) {
            None => {
                println!("stopped.");
                return Ok(());
            }
            Some(status) if !is_alive(&status) => {
                let _ = std::fs::remove_file(status_file(&service));
                println!("stopped.");
                return Ok(());
            }
            Some(_) => {}
        }
    }

    bail!(
        "the server did not stop within {STOP_TIMEOUT_SECONDS}s — it is still running (pid {})",
        status.supervisor_pid
    );
}

/// Restart the service, with the same arguments it was started with.
pub fn restart(
    dir: &Path,
    db: Option<&Path>,
    backend: &str,
    args: &StartArgs,
) -> Result<()> {
    let previous = read_status(&service_dir(dir));
    if previous.as_ref().is_some_and(is_alive) {
        stop(dir)?;
    }
    // The arguments of the running service win: a restart should not silently
    // change the domain or the port.
    let args = match previous {
        Some(status) => args_from_status(&status, args),
        None => args.clone(),
    };
    start(dir, db, backend, &args)
}

/// Take the domain and the port from a running service, when the new arguments
/// do not say otherwise.
fn args_from_status(status: &Status, args: &StartArgs) -> StartArgs {
    StartArgs {
        domain: args.domain.clone().or_else(|| status.domain.clone()),
        email: args.email.clone(),
        port: args.port.or(status.port),
        host: args.host.clone(),
        ssl_port: args.ssl_port,
    }
}

/// Show what the service is doing.
pub fn status(dir: &Path) -> Result<()> {
    let Some(status) = read_status(&service_dir(dir)) else {
        println!("not running.");
        return Ok(());
    };

    if !is_alive(&status) {
        println!("not running (the last supervisor, pid {}, is gone).", status.supervisor_pid);
        return Ok(());
    }

    println!("state:     {}", status.state);
    println!("pid:       {} (supervisor)", status.supervisor_pid);
    if let Some(pid) = status.server_pid {
        println!("server:    {pid}");
    }
    println!("uptime:    {}s", now().saturating_sub(status.started_at));
    println!("restarts:  {}", status.restarts);
    if let Some(port) = status.port {
        println!("port:      {port}");
    }
    if let Some(domain) = &status.domain {
        println!("domain:    https://{domain}/");
    }
    println!("instance:  {}", dir.display());
    println!("log:       {}", status.log.display());

    Ok(())
}

/// Print the summary after a start.
fn report(status: &Status, instance: &Path) {
    println!("started.");
    println!(
        "  dashboard:  http://127.0.0.1:{}/",
        status.port.unwrap_or(3000)
    );
    if let Some(domain) = &status.domain {
        println!("  domain:     https://{domain}/");
    }
    println!("  instance:   {}", instance.display());
    println!("  log:        {}", status.log.display());
    println!("  stop it:    sumimi service stop");
}

/// The supervisor: the process that owns the server.
///
/// It is started by `sumimi service start` and is not meant to be run by hand.
pub fn supervise(args: &SuperviseArgs) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building the runtime")?;
    runtime.block_on(run_supervisor(args))
}

/// The supervisor's loop.
async fn run_supervisor(args: &SuperviseArgs) -> Result<()> {
    let directory = args
        .status_file
        .parent()
        .map(|parent| parent.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("creating {}", directory.display()))?;

    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&args.log_file)
        .with_context(|| format!("opening {}", args.log_file.display()))?;

    let executable = std::env::current_exe().context("finding this executable")?;
    let started_at = now();
    let mut restarts: u32 = 0;
    let mut failures: u32 = 0;
    let mut last_exit: Option<String> = None;

    loop {
        let mut child = tokio::process::Command::new(&executable)
            .args(&args.command)
            // The server reads this: when the pipe closes, it shuts down.
            .env("SUMIMI_EXIT_ON_STDIN_EOF", "1")
            .stdin(Stdio::piped())
            .stdout(log.try_clone()?)
            .stderr(log.try_clone()?)
            .kill_on_drop(false)
            .spawn()
            .with_context(|| format!("running {:?}", args.command))?;

        let server_pid = child.id().unwrap_or(0);
        let child_started_at = now();
        let mut stdin = child.stdin.take();
        let mut asked_to_stop_at: Option<u64> = None;
        let mut last_state = "running";
        let mut last_write = now();

        write_status(
            &args.status_file,
            &Status {
                supervisor_pid: std::process::id(),
                server_pid: Some(server_pid),
                state: "running".to_string(),
                started_at,
                updated_at: now(),
                restarts,
                last_exit: last_exit.clone(),
                port: args.port,
                domain: args.domain.clone(),
                log: args.log_file.clone(),
                command: args.command.clone(),
            },
        )?;

        // Watch the child until it exits or it is asked to.
        let exit = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {}
                Err(err) => {
                    eprintln!("waiting for the server: {err}");
                    break None;
                }
            }

            let stopping = args.stop_file.exists();
            if stopping {
                if stdin.is_some() {
                    // Closing the pipe is the request to stop: the server shuts
                    // down gracefully, the same way it does on SIGTERM.
                    drop(stdin.take());
                    asked_to_stop_at = Some(now());
                } else if let Some(asked) = asked_to_stop_at {
                    if now().saturating_sub(asked) > STOP_TIMEOUT_SECONDS {
                        // It did not go. Take it down.
                        let _ = child.start_kill();
                    }
                }
            }

            // The heartbeat: the state is rewritten every few seconds, not on
            // every poll — a service that sits idle for months should not write
            // a file four times a second.
            let state = if stopping { "stopping" } else { "running" };
            if state != last_state || now().saturating_sub(last_write) >= HEARTBEAT_SECONDS {
                last_state = state;
                last_write = now();
                write_status(
                    &args.status_file,
                    &Status {
                        supervisor_pid: std::process::id(),
                        server_pid: Some(server_pid),
                        state: state.to_string(),
                        started_at,
                        updated_at: now(),
                        restarts,
                        last_exit: last_exit.clone(),
                        port: args.port,
                        domain: args.domain.clone(),
                        log: args.log_file.clone(),
                        command: args.command.clone(),
                    },
                )?;
            }

            tokio::time::sleep(Duration::from_millis(250)).await;
        };

        let stopping = args.stop_file.exists();

        // The exit itself.
        let description = match &exit {
            Some(status) => match status.code() {
                Some(code) => format!("exited with code {code}"),
                None => "was killed".to_string(),
            },
            None => "stopped".to_string(),
        };
        last_exit = Some(description.clone());

        if stopping {
            // A stop that was asked for: not a failure.
            break;
        }

        // Failure protection: a server that dies again and again is a server that
        // is broken, and restarting it forever hides that.
        if now().saturating_sub(child_started_at) < HEALTHY_SECONDS {
            failures += 1;
        } else {
            failures = 0;
        }

        if failures >= MAX_FAILURES {
            let _ = write_status(
                &args.status_file,
                &Status {
                    supervisor_pid: std::process::id(),
                    server_pid: None,
                    state: "failed".to_string(),
                    started_at,
                    updated_at: now(),
                    restarts,
                    last_exit: Some(description.clone()),
                    port: args.port,
                    domain: args.domain.clone(),
                    log: args.log_file.clone(),
                    command: args.command.clone(),
                },
            );
            bail!("the server failed {failures} times in a row ({description}) — not restarting it");
        }

        // The backoff: the longer it keeps failing, the longer it waits.
        let delay = BACKOFF_FIRST
            .saturating_mul(1u32.saturating_pow(restarts.min(5)))
            .min(BACKOFF_MAX);
        restarts += 1;

        eprintln!(
            "the server {description} — restarting it in {}s (restart {restarts})",
            delay.as_secs()
        );

        write_status(
            &args.status_file,
            &Status {
                supervisor_pid: std::process::id(),
                server_pid: None,
                state: "restarting".to_string(),
                started_at,
                updated_at: now(),
                restarts,
                last_exit: Some(description.clone()),
                port: args.port,
                domain: args.domain.clone(),
                log: args.log_file.clone(),
                command: args.command.clone(),
            },
        )?;

        // The backoff waits — but a stop request interrupts it, otherwise a
        // service that is failing would take the whole timeout to stop.
        let deadline = std::time::Instant::now() + delay;
        while std::time::Instant::now() < deadline {
            if args.stop_file.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        if args.stop_file.exists() {
            break;
        }
    }

    // The service is over: leave no state behind that says otherwise.
    let _ = std::fs::remove_file(&args.status_file);
    let _ = std::fs::remove_file(&args.stop_file);
    Ok(())
}
