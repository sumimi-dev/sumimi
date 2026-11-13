//! The server entry point: argument parsing and startup.
//!
//! Lives in the library so that the combined `sumimi` binary can offer the same
//! `serve` / `admin` / `config` commands as the standalone `sumimi-server`.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::api::{self, AppState};
use crate::auth;
use crate::config;
use crate::config::Config;
use crate::pool::Db;
use crate::storage::Storage;
use crate::writer::Writer;

/// The Sumimi analytics service.
#[derive(Debug, Parser)]
#[command(name = "sumimi-server", version, about = "Sumimi analytics service")]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the HTTP server.
    Serve(ServeArgs),
    /// Create or update the admin user, then exit.
    Admin(AdminArgs),
    /// Print the resolved configuration, then exit.
    Config(ConfigArgs),
}

/// The flags of the `config` command — the same ones `serve` takes, so the
/// printed configuration is the one that would be used.
#[derive(Debug, clap::Args)]
pub struct ConfigArgs {
    /// Address to bind to.
    #[arg(long, env = "SUMIMI_HOST")]
    host: Option<String>,
    /// Port to bind to.
    #[arg(long, env = "SUMIMI_PORT")]
    port: Option<u16>,
    /// Secret used to sign tokens (generated and stored on first run when omitted).
    #[arg(long, env = "SUMIMI_JWT_SECRET")]
    jwt_secret: Option<String>,
    /// Default retention window in days (0 = keep forever).
    #[arg(long, env = "SUMIMI_RETENTION_DAYS")]
    retention_days: Option<i32>,
    /// Header carrying the country code (e.g. `CF-IPCountry`).
    #[arg(long, env = "SUMIMI_COUNTRY_HEADER")]
    country_header: Option<String>,
    /// Directory holding the documentation site.
    #[arg(long, env = "SUMIMI_DOCS_DIR")]
    docs_dir: Option<std::path::PathBuf>,
    /// The instance directory.
    #[arg(long, env = "SUMIMI_DIR", default_value = "sumimi")]
    dir: std::path::PathBuf,
    /// The data file, when it should not be the default one.
    #[arg(long, env = "SUMIMI_DB")]
    db: Option<std::path::PathBuf>,
    /// The data engine: `duckdb` (the default) or `sqlite`.
    #[arg(long, env = "SUMIMI_BACKEND", default_value = "duckdb")]
    backend: String,
}

#[derive(Debug, clap::Args)]
pub struct ServeArgs {
    /// The instance directory: everything the instance owns lives inside it.
    #[arg(long, env = "SUMIMI_DIR", default_value = "sumimi")]
    dir: std::path::PathBuf,
    /// The data file, when it should not be the default one inside the instance
    /// directory — for keeping the events on a bigger disk than the rest.
    #[arg(long, env = "SUMIMI_DB")]
    db: Option<std::path::PathBuf>,
    /// The data engine: `duckdb` (the default) or `sqlite`.
    ///
    /// Both store the same tables and answer the same reports; they differ in
    /// everything else. DuckDB is a column store: it stores the events in far fewer
    /// bytes and aggregates them faster. SQLite is a row store in a single ordinary
    /// file: it takes more space and is slower on the heavy aggregations, but it
    /// allows many readers next to one writer and it is what most other tools
    /// already know how to open.
    ///
    /// The trade has four axes, and they were measured on the same machine: the
    /// same three-year history (6.2 million page views) seeded into both, and the
    /// same load for the ingestion.
    ///
    /// | | DuckDB | SQLite |
    /// | --- | ---: | ---: |
    /// | the peak memory under load | 214 MB | **33 MB** |
    /// | the data on disk (3 years) | **372 MB** | 3 086 MB |
    /// | the summary of the last year | **145 ms** | 17 452 ms |
    /// | the summary of the last three years | **186 ms** | 24 162 ms |
    /// | the memory a 3-year summary needs | 322 MB | **54 MB** |
    ///
    /// So DuckDB wins the disk and the latency and **loses the memory**: its
    /// ingestion is the peak, and a long aggregate grows with the range. The
    /// engine's own `memory_limit` has to fit both — the traffic and the reports.
    /// SQLite wins the memory and the concurrent readers.
    #[arg(long, env = "SUMIMI_BACKEND", default_value = "duckdb")]
    backend: String,
    /// Address to bind to.
    #[arg(long, env = "SUMIMI_HOST")]
    host: Option<String>,
    /// Port to bind to.
    #[arg(long, env = "SUMIMI_PORT")]
    port: Option<u16>,
    /// Secret used to sign tokens (generated and stored on first run when omitted).
    #[arg(long, env = "SUMIMI_JWT_SECRET")]
    jwt_secret: Option<String>,
    /// Default retention window in days (0 = keep forever).
    #[arg(long, env = "SUMIMI_RETENTION_DAYS")]
    retention_days: Option<i32>,
    /// Header carrying the country code (e.g. `CF-IPCountry`).
    #[arg(long, env = "SUMIMI_COUNTRY_HEADER")]
    country_header: Option<String>,
    /// Directory holding the frontend files.
    #[arg(long, env = "SUMIMI_JS_DIR")]
    js_dir: Option<std::path::PathBuf>,
    /// Directory holding the documentation site.
    #[arg(long, env = "SUMIMI_DOCS_DIR")]
    docs_dir: Option<std::path::PathBuf>,
    /// Serve HTTPS with an automatically issued (and renewed) certificate.
    #[arg(long = "ssl", alias = "ssl-domain", env = "SUMIMI_SSL_DOMAIN")]
    ssl_domain: Option<String>,
    /// Contact address registered with the ACME provider (Let's Encrypt).
    #[arg(long, env = "SUMIMI_SSL_EMAIL")]
    ssl_email: Option<String>,
    /// Directory where the ACME account and certificates are cached.
    #[arg(long, env = "SUMIMI_SSL_CACHE_DIR")]
    ssl_cache_dir: Option<std::path::PathBuf>,
    /// Port used for HTTPS.
    #[arg(long, env = "SUMIMI_SSL_PORT", default_value_t = 443)]
    ssl_port: u16,
}

#[derive(Debug, clap::Args)]
pub struct AdminArgs {
    /// Admin username.
    #[arg(long, env = "SUMIMI_ADMIN_USER", default_value = "admin")]
    username: String,
    /// Admin password (generated and printed when omitted).
    #[arg(long, env = "SUMIMI_ADMIN_PASSWORD")]
    password: Option<String>,
    /// The instance directory.
    #[arg(long, env = "SUMIMI_DIR", default_value = "sumimi")]
    dir: std::path::PathBuf,
    /// The data file, when it should not be the default one.
    #[arg(long, env = "SUMIMI_DB")]
    db: Option<std::path::PathBuf>,
    /// The data engine: `duckdb` (the default) or `sqlite`.
    #[arg(long, env = "SUMIMI_BACKEND", default_value = "duckdb")]
    backend: String,
}

/// Parse the command line and run the server.
pub async fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,sumimi=debug")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Serve(args) => serve(args).await,
        Command::Admin(args) => admin(args),
        Command::Config(args) => {
            let config = Config::from_env_and_flags(
                crate::config::Paths::new(
                    Some(args.dir),
                    args.db,
                    &args.backend,
                ),
                args.host,
                args.port,
                args.jwt_secret,
                args.retention_days,
                args.country_header,
                args.docs_dir,
            )?;
            println!("{config:#?}");
            Ok(())
        }
    }
}

/// Run the server on the current Tokio runtime.
pub async fn serve_on_runtime(args: ServeArgs) -> Result<()> {
    serve(args).await
}

/// Open the storage backend the configuration asks for.
///
/// The open-source build has DuckDB and SQLite. The private `sumimi-pro` build adds
/// PostgreSQL; see `pro/backend.rs`.
fn open_storage(config: &Config, backend: &str) -> Result<Arc<dyn Storage>> {
    let storage: Arc<dyn Storage> = match backend.to_ascii_lowercase().as_str() {
        "duckdb" => Arc::new(Db::open_with(
            &config.paths.data,
            &config.duckdb_memory_limit,
            &config.duckdb_flush_threshold,
            &config.duckdb_temp_limit,
            config.duckdb_threads,
        )?),
        "sqlite" => Arc::new(crate::sqlite::SqliteBackend::open(&config.paths.data)?),
        other => anyhow::bail!("unknown backend '{other}' (duckdb, sqlite)"),
    };
    Ok(storage)
}

/// Open the instance store — the accounts, the log, the cache and the rest.
///
/// It lives in its own small file next to the data, so the accounts survive an
/// analytics rebuild and can be backed up in a second.
fn open_instance(config: &Config) -> Result<Arc<dyn crate::instance::Instance>> {
    let instance: Arc<dyn crate::instance::Instance> =
        Arc::new(crate::instance::SqliteInstance::open(&config.paths.instance)?);
    Ok(instance)
}

/// Build the configuration from the serve arguments.
fn config_from(args: &ServeArgs) -> Result<Config> {
    let paths = crate::config::Paths::new(
        Some(args.dir.clone()),
        args.db.clone(),
        &args.backend,
    );
    let config = Config::from_env_and_flags(
        paths,
        args.host.clone(),
        args.port,
        args.jwt_secret.clone(),
        args.retention_days,
        args.country_header.clone(),
        args.docs_dir.clone(),
    )?;
    Ok(config)
}

/// Create (or update) the admin user.
fn admin(args: AdminArgs) -> Result<()> {
    let config = Config::from_env_and_flags(
        crate::config::Paths::new(
            Some(args.dir.clone()),
            args.db.clone(),
            &args.backend,
        ),
        None,
        None,
        None,
        None,
        None,
        None,
    )?;

    config.paths.prepare()?;
    let instance = open_instance(&config)?;
    let password = args
        .password
        .clone()
        .or_else(|| config.admin_password.clone())
        .unwrap_or_else(config::random_secret);
    let generated = args.password.is_none() && config.admin_password.is_none();

    instance.set_password(&args.username, &auth::hash_password(&password)?)?;

    println!("admin user '{}' ready", args.username);
    if generated {
        println!("password: {password}");
        println!("(store it now — it is not shown again)");
    }

    Ok(())
}


/// Run the HTTP server with the default (DuckDB) backend.
pub async fn serve(args: ServeArgs) -> Result<()> {
    let mut config = config_from(&args)?;
    if let Some(js_dir) = args.js_dir.clone() {
        config.js_dir = js_dir;
    }
    if let Some(domain) = args.ssl_domain.clone() {
        config.ssl_domain = Some(domain);
    }
    if let Some(email) = args.ssl_email.clone() {
        config.ssl_email = Some(email);
    }
    if let Some(dir) = args.ssl_cache_dir.clone() {
        config.ssl_cache_dir = dir;
    }
    config.ssl_port = args.ssl_port;

    config.paths.prepare()?;
    let storage = open_storage(&config, &args.backend)?;
    let instance = open_instance(&config)?;

    // The geolocation database, if the site owner downloaded one.
    //
    // The environment variable wins (it points at a file whatever its name);
    // then the city database in the instance directory; then the country one.
    // **No database is not an error**: the country comes from the header then.
    let geo = crate::geo::from_env()
        .or_else(|| crate::geo::open_if_present(&config.paths.geoip))
        .or_else(|| crate::geo::open_if_present(&config.paths.geoip_country))
        .map(|resolver| Arc::new(resolver) as Arc<dyn crate::geo::Resolver>);

    serve_with_geo(Arc::new(config), storage, instance, geo).await
}

/// Run the HTTP server with a given storage backend.
///
/// The open-source build only has DuckDB; the private `sumimi-pro` build passes a
/// SQLite or PostgreSQL backend here.
pub async fn serve_with(
    config: Arc<Config>,
    storage: Arc<dyn Storage>,
    instance: Arc<dyn crate::instance::Instance>,
) -> Result<()> {
    serve_with_geo(config, storage, instance, None).await
}

/// Serve, with a geolocation resolver.
///
/// The open-source build has none: it uses the country header a proxy sends.
/// The private edition passes one — a MaxMind database, read in memory.
pub async fn serve_with_geo(
    config: Arc<Config>,
    storage: Arc<dyn Storage>,
    instance: Arc<dyn crate::instance::Instance>,
    geo: Option<Arc<dyn crate::geo::Resolver>>,
) -> Result<()> {
    let jwt_secret = config::resolve_jwt_secret(&config.jwt_secret, &config.paths.secret)?;
    let jwt_secret = Arc::new(jwt_secret);

    // The instance directory holds the accounts and the events: nobody else reads it.
    // (A **directory** — 0700, not 0600: without the execute bit nobody could
    // open the files inside it, and the server could not start a second time.)
    config::restrict_directory(&config.paths.dir);

    // Make sure there is always an admin account to log in with. The accounts live
    // in the instance store, not in the analytics file.
    let has_admin = instance.account(&config.admin_user)?.is_some();
    if !has_admin {
        let password = config
            .admin_password
            .clone()
            .unwrap_or_else(config::random_secret);
        let generated = config.admin_password.is_none();
        instance.set_password(&config.admin_user, &auth::hash_password(&password)?)?;
        println!("[sumimi] created the admin user '{}'", config.admin_user);
        if generated {
            println!("[sumimi] password: {password}");
            println!("[sumimi] store it now — it is not shown again");
        }
    }

    let (writer, _writer_handle) = Writer::spawn_with(
        storage.clone(),
        std::time::Duration::from_secs(config.flush_interval_secs),
        config.max_queue,
        config.max_batch,
    );

    let state = AppState::new(
        storage,
        instance,
        writer.clone(),
        config.clone(),
        jwt_secret,
    );

    // The geolocation resolver, when the edition has one.
    let state = match geo {
        Some(resolver) => state.with_geo(resolver),
        None => state,
    };
    let app = api::router(state.clone());

    let addr: SocketAddr = format!("{}:{}", config.host, config.port)
        .parse()
        .context("invalid bind address")?;

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        dir = %config.paths.dir.display(),
        "sumimi is listening on http://{addr}"
    );
    println!(
        "[sumimi] listening on http://{addr} (instance: {})",
        config.paths.dir.display()
    );

    // The uptime checker: every website, every few minutes.
    if config.uptime_interval_secs > 0 {
        let state = state.clone();
        let interval = std::time::Duration::from_secs(config.uptime_interval_secs);
        let timeout = std::time::Duration::from_secs(config.uptime_timeout_secs.clamp(1, 60));
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            loop {
                ticker.tick().await;
                check_the_websites(&state, timeout).await;
            }
        });
    }

    // Make sure the writer thread is told to stop when the server does, and that
    // the instance store puts everything into its file — so a backup taken
    // right after the server stops is complete.
    let writer_for_shutdown = state.writer.clone();
    let instance_for_shutdown = state.instance.clone();

    // HTTPS with an automatically managed certificate, or plain HTTP.
    #[cfg(feature = "tls")]
    if let Some(domain) = config.ssl_domain.clone() {
        drop(listener);
        let ssl_addr: SocketAddr = format!("{}:{}", config.host, config.ssl_port)
            .parse()
            .context("invalid ssl bind address")?;
        let app = app;
        tokio::select! {
            result = crate::tls::serve(
                app,
                domain,
                config.ssl_email.clone(),
                config.ssl_cache_dir.clone(),
                ssl_addr,
            ) => result?,
            _ = shutdown_signal() => {}
        }
        writer_for_shutdown.shutdown();
        let _ = instance_for_shutdown.checkpoint();
        return Ok(());
    }

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;

    writer_for_shutdown.shutdown();
    // The instance store keeps its newest writes in a write-ahead log: put them
    // into the file, so a backup of that file is the whole instance.
    let _ = instance_for_shutdown.checkpoint();
    Ok(())
}


/// Check every website and record the answers.
///
/// The check is a blocking HTTP call, so it happens on a thread that is allowed
/// to block. A website that cannot be reached is recorded too — with the note
/// that **our** network was the problem, when that is what it was.
async fn check_the_websites(state: &AppState, timeout: std::time::Duration) {
    let websites = state.storage.websites().unwrap_or_default();
    for website in websites {
        let id = website.id.clone();

        // The website's own domain, plus every endpoint that was configured
        // for it — the health check, the login page, the API.
        let mut urls: Vec<String> = Vec::new();
        if let Some(domain) = website.domain.as_deref() {
            let domain = domain.trim();
            if !domain.is_empty() {
                urls.push(if domain.starts_with("http://") || domain.starts_with("https://") {
                    domain.to_string()
                } else {
                    format!("https://{domain}")
                });
            }
        }
        urls.extend(state.instance.uptime_endpoints(&id).unwrap_or_default());

        for url in urls {
            let checker = crate::uptime::Checker::with_timeout(timeout);
            let id = id.clone();
            let checked =
                tokio::task::spawn_blocking(move || checker.record(&id, &url)).await;
            let Ok(check) = checked else {
                continue;
            };

            if let Err(err) = state.instance.record_uptime(&check) {
                tracing::error!(error = %err, "could not record the uptime check");
            }
        }
    }
}

/// Wait for Ctrl-C (or SIGTERM) and shut down gracefully.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
        _ = stdin_closed() => {},
    }

    println!("[sumimi] shutting down");
}

/// Wait for the end of standard input.
///
/// The supervisor (`sumimi service start`) starts the server with a pipe on its
/// standard input and closes it to ask for a graceful stop — the same path a
/// SIGTERM takes, without anybody having to send a signal.
///
/// It is **opt-in**: with a terminal on standard input, an end-of-file would be a
/// surprise, so the server only listens when `SUMIMI_EXIT_ON_STDIN_EOF` is set.
async fn stdin_closed() {
    if std::env::var_os("SUMIMI_EXIT_ON_STDIN_EOF").is_none() {
        std::future::pending::<()>().await;
    }

    // The read blocks, so it goes onto a thread that is allowed to block.
    let _ = tokio::task::spawn_blocking(|| {
        use std::io::Read;
        let mut stdin = std::io::stdin();
        let mut buffer = [0u8; 256];
        loop {
            match stdin.read(&mut buffer) {
                // The end of the pipe: the request to stop.
                Ok(0) => return,
                Ok(_) => continue,
                Err(_) => return,
            }
        }
    })
    .await;
}
