//! The Sumimi client — read and analyse a Sumimi database.
//!
//! The same commands work against a local DuckDB file (`--db`) or against a running
//! server over HTTP (`--url`, with the admin account).

#![forbid(unsafe_code)]

pub mod backup;
mod gsc;
mod geoip;
pub mod mcp;
mod output;
pub mod seed;
pub mod service;
mod source;

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};
use serde_json::Value;

use source::{parse_time, Range, Report, Source};

/// The Sumimi command line interface.
#[derive(Debug, Parser)]
#[command(name = "sumimi", version, about = "Sumimi CLI — read and analyse Sumimi data")]
struct Cli {
    /// Path of the DuckDB database file (local mode).
    #[arg(long, global = true, env = "SUMIMI_DB")]
    db: Option<PathBuf>,
    /// Base URL of a running Sumimi server (remote mode).
    #[arg(long, global = true, env = "SUMIMI_URL")]
    url: Option<String>,
    /// Bearer token for remote mode.
    #[arg(long, global = true, env = "SUMIMI_TOKEN")]
    token: Option<String>,
    /// Admin username (remote mode).
    #[arg(long, global = true, env = "SUMIMI_ADMIN_USER")]
    username: Option<String>,
    /// Admin password (remote mode).
    #[arg(long, global = true, env = "SUMIMI_ADMIN_PASSWORD")]
    password: Option<String>,
    /// Print raw JSON instead of a table.
    #[arg(long, global = true)]
    json: bool,
    /// Analyse a Parquet archive instead of the database (e.g. './archive/*').
    #[arg(long, global = true, env = "SUMIMI_PARQUET")]
    parquet: Option<String>,
    /// The data engine: `duckdb` (the default) or `sqlite`.
    #[arg(long, global = true, env = "SUMIMI_BACKEND", default_value = "duckdb")]
    backend: String,
    /// The instance directory: the accounts, the log, the imports and the cache
    /// live in it.
    #[arg(long, global = true, env = "SUMIMI_DIR", default_value = "sumimi")]
    dir: PathBuf,
    #[command(subcommand)]
    command: Command,
}

/// The commands of `sumimi geoip`.
#[derive(Debug, Clone, Subcommand)]
enum GeoipCommand {
    /// Download the geolocation database into the instance directory.
    Download {
        /// The small one: the country only.
        ///
        /// The country database is a tenth of the size, and it is what a small
        /// box can hold in memory. The city one adds the region, the city and
        /// the coordinates.
        #[arg(long)]
        country: bool,
    },
}

/// The commands.
#[derive(Debug, Clone, Subcommand)]
enum Command {
    /// The geolocation database.
    Geoip {
        #[command(subcommand)]
        command: GeoipCommand,
    },
    /// List the websites.
    Websites,
    /// Show the summary metrics.
    Summary {
        /// The website id.
        website: String,
        /// Start of the window (7d, 24h, 2026-01-01, ...).
        #[arg(long)]
        from: Option<String>,
        /// End of the window.
        #[arg(long)]
        to: Option<String>,
    },
    /// Show the top pages.
    Pages(WindowArgs),
    /// Show the traffic sources.
    Referrers(WindowArgs),
    /// Show the browser breakdown.
    Browsers(WindowArgs),
    /// Show the operating system breakdown.
    Os(WindowArgs),
    /// Show the device breakdown.
    Devices(WindowArgs),
    /// Show the country breakdown.
    Countries(WindowArgs),
    /// Show the language breakdown.
    Languages(WindowArgs),
    /// Show the screen size breakdown.
    Screens(WindowArgs),
    /// Show the custom events.
    Events(WindowArgs),
    /// Show the traffic channels.
    Channels(WindowArgs),
    /// Show the regions of the visitors.
    Regions(WindowArgs),
    /// Show the cities of the visitors.
    Cities(WindowArgs),
    /// Show the hostnames of the sites.
    Hosts(WindowArgs),
    /// Show the tags of the custom events.
    Tags(WindowArgs),
    /// Show the query parameters of the visited pages.
    QueryParameters(WindowArgs),
    /// Show the sessions.
    Sessions(WindowArgs),
    /// Show the time series.
    Timeseries(WindowArgs),
    /// Export a report as JSON or CSV.
    Export {
        /// The website id.
        website: String,
        /// The report to export.
        #[arg(value_enum)]
        report: ExportReport,
        /// The output format.
        #[arg(long, value_enum, default_value_t = ExportFormat::Json)]
        format: ExportFormat,
        /// Start of the window.
        #[arg(long)]
        from: Option<String>,
        /// End of the window.
        #[arg(long)]
        to: Option<String>,
    },
    /// Write the database to a Parquet archive (one file per day).
    Backup {
        /// The directory to write the archive to.
        #[arg(long, default_value = "archive")]
        out: PathBuf,
        /// Only archive this website.
        #[arg(long)]
        website: Option<String>,
        /// Start of the range (YYYY-MM-DD).
        #[arg(long)]
        from: Option<String>,
        /// End of the range (YYYY-MM-DD).
        #[arg(long)]
        to: Option<String>,
        /// Keep files that already exist.
        #[arg(long)]
        skip_existing: bool,
    },
    /// Check whether a website is up.
    ///
    /// When the site does not answer, two references are asked as well, so
    /// "the site is down" and "this machine has no internet" are not confused.
    ///
    /// With `--website`, the endpoints of that website can be managed:
    /// `--add` watches one more, `--list` shows them, `--remove` stops watching
    /// one, and `--check-all` checks every one of them now.
    Uptime {
        /// The URL to check.
        #[arg(default_value = "")]
        url: String,
        /// How long to wait for an answer, in seconds.
        #[arg(long, default_value_t = 10)]
        timeout: u64,
        /// The website the check belongs to — it is recorded.
        #[arg(long)]
        website: Option<String>,
        /// Watch this endpoint of the website as well.
        #[arg(long)]
        add: Option<String>,
        /// List the watched endpoints of the website.
        #[arg(long)]
        list: bool,
        /// Stop watching this endpoint.
        #[arg(long)]
        remove: Option<String>,
        /// Check every endpoint of the website now.
        #[arg(long)]
        check_all: bool,
    },
    /// Delete data that is older than the retention window.
    Prune {
        /// The website id (default: every website).
        website: Option<String>,
    },
    /// Delete a website and all of its data.
    Erase {
        /// The website id.
        website: String,
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Fill the database with synthetic demo data.
    Seed {
        /// The name of the website to seed (created when missing).
        #[arg(long, default_value = "Example Blog")]
        website: String,
        /// The domain of the website.
        #[arg(long, default_value = "example.com")]
        domain: String,
        /// How many days of history to generate.
        #[arg(long, default_value_t = 30)]
        days: i64,
        /// Roughly how many visitors per day.
        #[arg(long, default_value_t = 120)]
        visitors: i64,
    },
    /// The Google Search Console import.
    #[command(subcommand)]
    Gsc(GscCommand),
    /// Store the credentials for a server.
    Login {
        /// The base URL of the server.
        url: String,
        /// The admin username.
        #[arg(long, default_value = "admin")]
        username: String,
        /// The admin password (asked for interactively when omitted).
        #[arg(long)]
        password: Option<String>,
    },
    /// Run the MCP server: the analytics as tools an agent can call.
    ///
    /// It speaks JSON-RPC over stdio, the way an MCP client expects, and
    /// offers three tools: `sumimi_report` (the numbers), `sumimi_record`
    /// (log an event) and `sumimi_ask` (ask a question).
    ///
    /// With `--url` it talks to a running server; with `--db` (or the default
    /// instance directory) it reads a local database.
    Mcp {
        /// The website the tools work on, when a call does not name one.
        #[arg(long, env = "SUMIMI_MCP_WEBSITE")]
        website: Option<String>,
    },
    /// Check that a server answers.
    ///
    /// This is the health check of a container: it asks the server for its
    /// heartbeat and says whether it answered.
    Ping,
    /// Run the server as a service — no systemd, no root.
    ///
    /// The server runs under a supervisor: a second process that owns it and
    /// restarts it when it dies. Everything lives inside the instance directory,
    /// so the service can run as the user who installed it.
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
}

/// The service commands.
#[derive(Debug, Clone, Subcommand)]
enum ServiceCommand {
    /// Start the server under a supervisor.
    Start(service::StartArgs),
    /// Stop the running server.
    Stop,
    /// Show what the service is doing.
    Status,
    /// Stop the server and start it again, with the same arguments.
    Restart(service::StartArgs),
    /// Run the supervisor. Started by `sumimi service start`.
    #[command(hide = true)]
    Supervise(service::SuperviseArgs),
}

/// The Search Console commands.
#[derive(Debug, Clone, Subcommand)]
enum GscCommand {
    /// Import a Search Console export into a website.
    Import {
        /// The export (a CSV file).
        file: PathBuf,
        /// The website the rows belong to.
        #[arg(long)]
        website: String,
    },
    /// Show what was imported.
    List {
        /// The website.
        #[arg(long)]
        website: String,
        /// How many rows to show.
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
}

/// Arguments shared by the windowed reports.
#[derive(Debug, Clone, clap::Args)]
struct WindowArgs {
    /// The website id.
    website: String,
    /// Start of the window (7d, 24h, 2026-01-01, ...).
    #[arg(long)]
    from: Option<String>,
    /// End of the window.
    #[arg(long)]
    to: Option<String>,
    /// How many rows to show.
    #[arg(long, default_value_t = 50)]
    limit: i64,
}

/// The reports that can be exported.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum ExportReport {
    /// The summary metrics.
    Summary,
    /// The top pages.
    Pages,
    /// The traffic sources.
    Referrers,
    /// The browser breakdown.
    Browsers,
    /// The custom events.
    Events,
    /// The sessions.
    Sessions,
    /// The time series.
    Timeseries,
}

impl From<ExportReport> for Report {
    fn from(report: ExportReport) -> Self {
        match report {
            ExportReport::Summary => Report::Summary,
            ExportReport::Pages => Report::Pages,
            ExportReport::Referrers => Report::Referrers,
            ExportReport::Browsers => Report::Browsers,
            ExportReport::Events => Report::Events,
            ExportReport::Sessions => Report::Sessions,
            ExportReport::Timeseries => Report::Timeseries,
        }
    }
}

/// The export formats.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum ExportFormat {
    /// JSON.
    Json,
    /// CSV.
    Csv,
}

/// Parse the command line and run the client.
/// The environment variables that were renamed.
///
/// The project was called `sumami` until v0.2.0. The old names are still in
/// people's `.env` files and systemd units, so they keep working — the new name
/// wins when both are set.
const RENAMED_ENV: [(&str, &str); 8] = [
    ("SUMAMI_DB", "SUMIMI_DB"),
    ("SUMAMI_URL", "SUMIMI_URL"),
    ("SUMAMI_TOKEN", "SUMIMI_TOKEN"),
    ("SUMAMI_ADMIN_USER", "SUMIMI_ADMIN_USER"),
    ("SUMAMI_ADMIN_PASSWORD", "SUMIMI_ADMIN_PASSWORD"),
    ("SUMAMI_BACKEND", "SUMIMI_BACKEND"),
    ("SUMAMI_DIR", "SUMIMI_DIR"),
    ("SUMAMI_PARQUET", "SUMIMI_PARQUET"),
];

/// Copy the old environment variable names to the new ones.
fn carry_over_old_env_names() {
    for (old, new) in RENAMED_ENV {
        if std::env::var_os(new).is_none() {
            if let Some(value) = std::env::var_os(old) {
                std::env::set_var(new, value);
            }
        }
    }
}

pub fn run() -> Result<()> {
    carry_over_old_env_names();
    let cli = Cli::parse();

    match &cli.command {
        Command::Login {
            url,
            username,
            password,
        } => cmd_login(url, username, password.as_deref()),
        Command::Mcp { website } => cmd_mcp(&cli, website.as_deref()),
        Command::Gsc(command) => cmd_gsc(&cli, command),
        Command::Seed {
            website,
            domain,
            days,
            visitors,
        } => cmd_seed(&cli, website, domain, *days, *visitors),
        Command::Geoip { command } => match command {
            GeoipCommand::Download { country } => {
                let kind = if *country {
                    geoip::Kind::Country
                } else {
                    geoip::Kind::City
                };
                geoip::download(&cli.dir, kind)
            }
        },
        Command::Backup {
            out,
            website,
            from,
            to,
            skip_existing,
        } => cmd_backup(&cli, out, website.as_deref(), from.as_deref(), to.as_deref(), *skip_existing),
        Command::Uptime {
            url,
            timeout,
            website,
            add,
            list,
            remove,
            check_all,
        } => cmd_uptime(
            &cli,
            Uptime {
                url,
                timeout: *timeout,
                website: website.as_deref(),
                add: add.as_deref(),
                list: *list,
                remove: remove.as_deref(),
                check_all: *check_all,
            },
        ),
        Command::Prune { website } => cmd_prune(&cli, website.as_deref()),
        Command::Erase { website, yes } => cmd_erase(&cli, website, *yes),
        Command::Ping => cmd_ping(&cli),
        Command::Service { command } => match command {
            ServiceCommand::Start(args) => {
                service::start(&cli.dir, cli.db.as_deref(), &cli.backend, args)
            }
            ServiceCommand::Stop => service::stop(&cli.dir),
            ServiceCommand::Status => service::status(&cli.dir),
            ServiceCommand::Restart(args) => {
                service::restart(&cli.dir, cli.db.as_deref(), &cli.backend, args)
            }
            ServiceCommand::Supervise(args) => service::supervise(args),
        },
        command => cmd_report(&cli, command),
    }
}

/// Ask a server for its heartbeat.
///
/// The exit code is the answer: zero when the server answered, one when it did
/// not. That is what a container's health check needs.
fn cmd_ping(cli: &Cli) -> Result<()> {
    let base = cli
        .url
        .clone()
        .unwrap_or_else(|| "http://127.0.0.1:3000".to_string());
    let url = format!("{}/api/heartbeat", base.trim_end_matches('/'));

    let mut response = ureq::get(&url).call().map_err(|err| {
        anyhow::anyhow!("{url} did not answer: {err}")
    })?;

    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|err| anyhow::anyhow!("{url} answered with something unreadable: {err}"))?;

    let version = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|value| {
            value
                .get("version")
                .and_then(|version| version.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "unknown".to_string());

    println!("{base} is up (version {version})");
    Ok(())
}

/// Store the credentials for a server.
fn cmd_login(url: &str, username: &str, password: Option<&str>) -> Result<()> {    let password = match password {
        Some(password) => password.to_string(),
        None => {
            print!("password for {username} at {url}: ");
            use std::io::Write;
            std::io::stdout().flush()?;
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            line.trim().to_string()
        }
    };

    // Verify the credentials before storing them.
    let source = Source::remote(url, None, Some(username), Some(&password))?;
    if let Source::Remote { base, .. } = &source {
        let path = source::store_credentials(username, &password)?;
        println!("credentials saved to {}", path.display());
        println!("logged in to {base}");
    }
    Ok(())
}

/// Run the MCP server: the analytics as tools an agent can call.
fn cmd_mcp(cli: &Cli, website: Option<&str>) -> Result<()> {
    // **stdout belongs to the protocol** — nothing else may be printed
    // there, or the client cannot read the answers.
    let (source, writable) = match &cli.url {
        Some(url) => {
            let source = Source::remote(
                url,
                cli.token.as_deref(),
                cli.username.as_deref(),
                cli.password.as_deref(),
            )?;
            (source, true)
        }
        None => {
            // The data file of the instance — the same one the server
            // writes to.
            let paths = sumimi_service::config::Paths::new(
                Some(cli.dir.clone()),
                cli.db.clone(),
                &cli.backend,
            );
            mcp::open_local(&paths.data)?
        }
    };

    let server = mcp::Server::new(source, writable, website.map(str::to_string));
    server.run()
}

/// The Search Console import.
fn cmd_gsc(cli: &Cli, command: &GscCommand) -> Result<()> {
    match command {
        GscCommand::Import { file, website } => {
            let text = std::fs::read_to_string(file)
                .map_err(|err| anyhow!("reading {}: {err}", file.display()))?;
            let rows = gsc::parse(&text)?;

            // Against a running server, the import goes through the API.
            if let Some(url) = &cli.url {
                let source = Source::remote(
                    url,
                    cli.token.as_deref(),
                    cli.username.as_deref(),
                    cli.password.as_deref(),
                )?;
                if let Source::Remote { base, token } = &source {
                    let body = serde_json::json!({
                        "website": website,
                        "rows": rows,
                        "source": file.display().to_string(),
                    });
                    let agent = ureq::Agent::new_with_defaults();
                    let report: serde_json::Value = agent
                        .post(&format!("{base}/api/gsc/import"))
                        .header("authorization", &format!("Bearer {token}"))
                        .send_json(body)?
                        .body_mut()
                        .read_json()?;
                    println!(
                        "imported {} rows into {website}",
                        report["rows"].as_u64().unwrap_or(0)
                    );
                }
                return Ok(());
            }

            // Offline: straight into the instance store.
            let paths = sumimi_service::config::Paths::new(
                Some(cli.dir.clone()),
                cli.db.clone(),
                &cli.backend,
            );
            let instance = sumimi_service::instance::SqliteInstance::open(&paths.instance)
                .map_err(|err| anyhow!("opening the instance store: {err}"))?;
            let report = instance
                .import_gsc(website, &rows, Some(&file.display().to_string()))
                .map_err(|err| anyhow!("the import failed: {err}"))?;
            println!("imported {} rows into {website}", report.rows);
            println!("the rows are in {}", paths.instance.display());
            Ok(())
        }
        GscCommand::List { website, limit } => {
            let paths = sumimi_service::config::Paths::new(
                Some(cli.dir.clone()),
                cli.db.clone(),
                &cli.backend,
            );
            let instance = sumimi_service::instance::SqliteInstance::open(&paths.instance)
                .map_err(|err| anyhow!("opening the instance store: {err}"))?;
            let rows = instance
                .gsc_rows(website, *limit)
                .map_err(|err| anyhow!("reading the import: {err}"))?;
            let table: Vec<Vec<String>> = rows
                .iter()
                .map(|row| {
                    vec![
                        row.date.clone(),
                        row.query.clone().unwrap_or_default(),
                        row.page.clone().unwrap_or_default(),
                        row.clicks.to_string(),
                        row.impressions.to_string(),
                        row.position
                            .map(|position| format!("{position:.1}"))
                            .unwrap_or_default(),
                    ]
                })
                .collect();
            output::table(
                &["DATE", "QUERY", "PAGE", "CLICKS", "IMPRESSIONS", "POSITION"],
                &table,
            );
            Ok(())
        }
    }
}

/// Fill the database with synthetic demo data.
fn cmd_seed(cli: &Cli, website: &str, domain: &str, days: i64, visitors: i64) -> Result<()> {
    let paths = sumimi_service::config::Paths::new(
        Some(cli.dir.clone()),
        cli.db.clone(),
        &cli.backend,
    );

    let options = seed::SeedOptions {
        website: website.to_string(),
        domain: domain.to_string(),
        days,
        visitors_per_day: visitors,
    };

    // The seed goes through the storage backend, so the same data lands in
    // DuckDB, in SQLite and in PostgreSQL — and the numbers agree.
    let storage: Box<dyn sumimi_service::storage::Storage> =
        match cli.backend.to_ascii_lowercase().as_str() {
            "duckdb" => Box::new(
                sumimi_service::pool::Db::open(&paths.data, "256MB")
                    .map_err(|err| anyhow!("opening {}: {err}", paths.data.display()))?,
            ),
            "sqlite" => Box::new(
                sumimi_service::sqlite::SqliteBackend::open(&paths.data)
                    .map_err(|err| anyhow!("opening {}: {err}", paths.data.display()))?,
            ),
            other => {
                return Err(anyhow!("unknown backend '{other}' (duckdb, sqlite)"));
            }
        };

    let (website_id, views, events) = seed::seed(storage.as_ref(), &options).map_err(|err| anyhow!("{err}"))?;

    println!("seeded {views} page views and {events} events");
    println!("website: {website_id}");
    println!("start the server and open the dashboard to see it");
    Ok(())
}

/// Write the database to a Parquet archive (one file per day).
fn cmd_backup(
    cli: &Cli,
    out: &std::path::Path,
    website: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
    skip_existing: bool,
) -> Result<()> {
    // The data file lives **inside the instance directory**. (Using the
    // default path here meant `sumimi backup --dir <dir>` archived an
    // **empty** database — and said it had archived 0 rows, which looks
    // like a successful backup of an empty site.)
    let path = sumimi_service::config::Paths::new(
        Some(cli.dir.clone()),
        cli.db.clone(),
        &cli.backend,
    )
    .data;

    // The backup reads the database, so the server must be stopped (DuckDB
    // allows a single writer per file). `--url` is not supported here for the
    // same reason.
    let db = sumimi_service::pool::Db::open(&path, "256MB")
        .map_err(|err| anyhow!("opening {}: {err}", path.display()))?;

    let from = from.map(parse_date).transpose()?;
    let to = to.map(parse_date).transpose()?;

    let stats = db
        .with_write(|conn| {
            backup::export(
                conn,
                out,
                website,
                from,
                to,
                skip_existing,
            )
        })
        .map_err(|err| anyhow!("{err}"))?;

    println!(
        "archived {} rows into {} file(s) under {}",
        stats.rows,
        stats.files,
        stats.directory.display()
    );
    println!(
        "read it back with:  sumimi --parquet '{}/' summary <website-id>",
        stats.directory.display()
    );
    Ok(())
}

/// What the `uptime` command was asked to do.
struct Uptime<'a> {
    /// The URL to check.
    url: &'a str,
    /// How long to wait for an answer, in seconds.
    timeout: u64,
    /// The website the check belongs to — it is recorded.
    website: Option<&'a str>,
    /// The endpoint to start watching.
    add: Option<&'a str>,
    /// Whether to list the watched endpoints.
    list: bool,
    /// The endpoint to stop watching.
    remove: Option<&'a str>,
    /// Whether to check every endpoint of the website now.
    check_all: bool,
}

/// Check whether a site is up — and manage the endpoints that are watched.
fn cmd_uptime(cli: &Cli, asked: Uptime<'_>) -> Result<()> {
    let Uptime {
        url,
        timeout,
        website,
        add,
        list,
        remove,
        check_all,
    } = asked;

    let checker = sumimi_service::uptime::Checker::with_timeout(
        std::time::Duration::from_secs(timeout.clamp(1, 60)),
    );

    // --- managing the watched endpoints ---
    if let Some(website) = website {
        let paths = sumimi_service::config::Paths::new(
            Some(cli.dir.clone()),
            cli.db.clone(),
            &cli.backend,
        );

        // Against a running server, the endpoints go through the API.
        let remote = cli.url.as_ref().map(|base| {
            Source::remote(
                base,
                cli.token.as_deref(),
                cli.username.as_deref(),
                cli.password.as_deref(),
            )
        });

        if let Some(Ok(Source::Remote { base, token })) = &remote {
            let agent = ureq::Agent::new_with_defaults();
            if let Some(url) = add {
                agent
                    .post(&format!("{base}/api/websites/{website}/uptime/endpoints"))
                    .header("authorization", &format!("Bearer {token}"))
                    .send_json(serde_json::json!({ "url": url }))?;
                println!("watching {url}");
            }
            if list {
                let value: serde_json::Value = agent
                    .get(&format!("{base}/api/websites/{website}/uptime/endpoints"))
                    .header("authorization", &format!("Bearer {token}"))
                    .call()?
                    .body_mut()
                    .read_json()?;
                for endpoint in value["endpoints"].as_array().unwrap_or(&vec![]) {
                    println!("{}", endpoint.as_str().unwrap_or(""));
                }
            }
            if let Some(url) = remove {
                agent
                    .delete(&format!(
                        "{base}/api/websites/{website}/uptime/endpoints?url={}",
                        encode(url)
                    ))
                    .header("authorization", &format!("Bearer {token}"))
                    .call()?;
                println!("stopped watching {url}");
            }
            if check_all {
                let value: serde_json::Value = agent
                    .post(&format!("{base}/api/websites/{website}/uptime/check"))
                    .header("authorization", &format!("Bearer {token}"))
                    .send_json(serde_json::json!({ "timeout": timeout }))?
                    .body_mut()
                    .read_json()?;
                for check in value["checks"].as_array().unwrap_or(&vec![]) {
                    println!(
                        "{}: {}",
                        check["websiteId"].as_str().unwrap_or(""),
                        if check["ok"].as_bool().unwrap_or(false) {
                            "up"
                        } else {
                            "down"
                        }
                    );
                }
            }
            return Ok(());
        }

        // Offline: straight into the instance store.
        let instance = sumimi_service::instance::SqliteInstance::open(&paths.instance)
            .map_err(|err| anyhow!("opening the instance store: {err}"))?;

        if let Some(url) = add {
            instance.add_uptime_endpoint(website, url)?;
            println!("watching {url}");
        }
        if list {
            for endpoint in instance.uptime_endpoints(website)? {
                println!("{endpoint}");
            }
        }
        if let Some(url) = remove {
            if instance.remove_uptime_endpoint(website, url)? {
                println!("stopped watching {url}");
            } else {
                println!("{url} was not being watched");
            }
        }
        if check_all {
            let mut urls = Vec::new();
            let domain = instance
                .uptime_endpoints(website)?
                .into_iter()
                .collect::<Vec<_>>();
            urls.extend(domain);
            for endpoint in urls {
                let check = checker.record(website, &endpoint);
                instance.record_uptime(&check)?;
                println!(
                    "{endpoint}: {}",
                    if check.ok { "up" } else { "down" }
                );
            }
        }
        if add.is_some() || list || remove.is_some() || check_all {
            return Ok(());
        }
    }

    // --- checking one URL now ---
    if url.is_empty() {
        anyhow::bail!("give a URL, or a --website with --check-all");
    }
    let verdict = checker.check(url);

    if cli.json {
        let value = match &verdict {
            sumimi_service::uptime::Verdict::Up {
                status,
                latency_ms,
            } => serde_json::json!({
                "url": url,
                "up": true,
                "status": status,
                "latencyMs": latency_ms,
            }),
            sumimi_service::uptime::Verdict::Down { error } => serde_json::json!({
                "url": url,
                "up": false,
                "error": error,
                "networkOk": true,
            }),
            sumimi_service::uptime::Verdict::NetworkDown { error, references } => {
                serde_json::json!({
                    "url": url,
                    "up": false,
                    "error": error,
                    "networkOk": false,
                    "references": references,
                })
            }
        };
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!("{url}: {}", verdict.summary());
    }

    // The check is recorded when it belongs to a website.
    if let Some(website) = website {
        let check = checker.record(website, url);

        if let Some(base) = &cli.url {
            // Against a running server, the check goes through the API.
            let source = Source::remote(
                base,
                cli.token.as_deref(),
                cli.username.as_deref(),
                cli.password.as_deref(),
            )?;
            if let Source::Remote { base, token } = &source {
                let agent = ureq::Agent::new_with_defaults();
                let _: serde_json::Value = agent
                    .post(&format!("{base}/api/websites/{website}/uptime"))
                    .header("authorization", &format!("Bearer {token}"))
                    .send_json(serde_json::json!({ "url": url }))?
                    .body_mut()
                    .read_json()?;
                println!("recorded for {website}");
                return Ok(());
            }
        }

        // Offline: straight into the instance store.
        let paths = sumimi_service::config::Paths::new(
            Some(cli.dir.clone()),
            cli.db.clone(),
            &cli.backend,
        );
        let instance = sumimi_service::instance::SqliteInstance::open(&paths.instance)
            .map_err(|err| anyhow!("opening the instance store: {err}"))?;
        instance.record_uptime(&check)?;
        println!("recorded for {website}");
    }
    Ok(())
}

/// Delete the data that is older than the retention window.
fn cmd_prune(cli: &Cli, website: Option<&str>) -> Result<()> {
    let source = open_source(cli, true)?;
    let (events, revenue) = source.prune(website)?;
    println!("removed {events} events and {revenue} revenue records");
    Ok(())
}

/// Delete a website and all of its data.
fn cmd_erase(cli: &Cli, website: &str, yes: bool) -> Result<()> {
    if !yes {
        println!("this deletes the website {website} and all of its data.");
        print!("type the website id to confirm: ");
        use std::io::Write;
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if answer.trim() != website {
            return Err(anyhow!("aborted"));
        }
    }
    let source = open_source(cli, true)?;
    let removed = source.erase(website)?;
    println!("removed {removed} rows");
    Ok(())
}

/// What a report command asks for.
struct ReportRequest {
    /// The report.
    report: Report,
    /// The website it is about.
    website: Option<String>,
    /// The window.
    range: Range,
    /// How many rows to show.
    limit: i64,
}

impl ReportRequest {
    /// The request of a command that asks for a report over a window.
    fn windowed(report: Report, args: &WindowArgs) -> Result<Self> {
        Ok(Self {
            report,
            website: Some(args.website.clone()),
            range: window(args.from.as_deref(), args.to.as_deref())?,
            limit: args.limit,
        })
    }
}

/// What the command asks for: the report, the website, the window, the limit.
fn report_request(command: &Command) -> Result<ReportRequest> {
    let request = match command {
        Command::Websites => ReportRequest {
            report: Report::Websites,
            website: None,
            range: Range::default_window(),
            limit: 50,
        },
        Command::Summary { website, from, to } => ReportRequest {
            report: Report::Summary,
            website: Some(website.clone()),
            range: window(from.as_deref(), to.as_deref())?,
            limit: 50,
        },
        Command::Pages(args) => ReportRequest::windowed(Report::Pages, args)?,
        Command::Referrers(args) => ReportRequest::windowed(Report::Referrers, args)?,
        Command::Browsers(args) => ReportRequest::windowed(Report::Browsers, args)?,
        Command::Os(args) => ReportRequest::windowed(Report::Os, args)?,
        Command::Devices(args) => ReportRequest::windowed(Report::Devices, args)?,
        Command::Countries(args) => ReportRequest::windowed(Report::Countries, args)?,
        Command::Languages(args) => ReportRequest::windowed(Report::Languages, args)?,
        Command::Screens(args) => ReportRequest::windowed(Report::Screens, args)?,
        Command::Events(args) => ReportRequest::windowed(Report::Events, args)?,
        Command::Channels(args) => ReportRequest::windowed(Report::Channels, args)?,
        Command::Regions(args) => ReportRequest::windowed(Report::Regions, args)?,
        Command::Cities(args) => ReportRequest::windowed(Report::Cities, args)?,
        Command::Hosts(args) => ReportRequest::windowed(Report::Hosts, args)?,
        Command::Tags(args) => ReportRequest::windowed(Report::Tags, args)?,
        Command::QueryParameters(args) => ReportRequest::windowed(Report::QueryParameters, args)?,
        Command::Sessions(args) => ReportRequest::windowed(Report::Sessions, args)?,
        Command::Timeseries(args) => ReportRequest::windowed(Report::Timeseries, args)?,
        other => return Err(anyhow!("{other:?} does not ask for a report")),
    };
    Ok(request)
}

/// The report commands: the numbers, as a table or as JSON.
fn cmd_report(cli: &Cli, command: &Command) -> Result<()> {
    let source = open_source(cli, false)?;
    match &source {
        Source::Local { path, .. } => {
            eprintln!("[sumimi] source: {}", path.display());
        }
        Source::Remote { base, .. } => {
            eprintln!("[sumimi] source: {base}");
        }
    }

    // The export writes the report and stops there.
    if let Command::Export {
        website,
        report,
        format,
        from,
        to,
    } = command
    {
        let report: Report = (*report).into();
        let value = source.report(
            report,
            Some(website),
            window(from.as_deref(), to.as_deref())?,
            10_000,
        )?;
        match format {
            ExportFormat::Json => println!("{}", serde_json::to_string_pretty(&value)?),
            ExportFormat::Csv => print_csv(&value),
        }
        return Ok(());
    }

    let request = report_request(command)?;
    let value = source.report(
        request.report,
        request.website.as_deref(),
        request.range,
        request.limit,
    )?;

    if cli.json {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        print_report(request.report, &value);
    }
    Ok(())
}

/// Percent-encode a value for a query string.
///
/// Only the characters a URL may not carry are escaped; everything else is
/// left alone, so the endpoint stays readable in the log.
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Open the configured source.
fn open_source(cli: &Cli, writable: bool) -> Result<Source> {
    // The cold tier: a Parquet archive, queried in place.
    if let Some(pattern) = &cli.parquet {
        return Source::parquet(pattern);
    }

    if let Some(url) = &cli.url {
        return Source::remote(
            url,
            cli.token.as_deref(),
            cli.username.as_deref(),
            cli.password.as_deref(),
        );
    }

    // The data file lives **inside the instance directory** — the same place
    // the seed writes it and the server reads it. (Using the default path here
    // meant `--dir` was ignored on the read path: the CLI seeded one directory
    // and then looked in another.)
    let paths = sumimi_service::config::Paths::new(
        Some(cli.dir.clone()),
        cli.db.clone(),
        &cli.backend,
    );
    let path = paths.data;

    if writable {
        Source::local_writable(&path)
    } else {
        Source::local(&path)
    }
}

/// Parse a `YYYY-MM-DD` date.
fn parse_date(value: &str) -> Result<chrono::NaiveDate> {
    chrono::NaiveDate::parse_from_str(value.trim(), "%Y-%m-%d")
        .map_err(|err| anyhow!("invalid date '{value}': {err}"))
}

/// Build the time window.
fn window(from: Option<&str>, to: Option<&str>) -> Result<Range> {
    let default = Range::default_window();
    let start = match from {
        Some(value) => parse_time(value)?,
        None => default.start,
    };
    let end = match to {
        Some(value) => parse_time(value)?,
        None => default.end,
    };
    Ok(Range { start, end })
}

/// Print a report as a table.
fn print_report(report: Report, value: &Value) {
    match report {
        Report::Websites => {
            let rows: Vec<Vec<String>> = value
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .map(|item| {
                            vec![
                                output::cell(item.get("id").unwrap_or(&Value::Null)),
                                output::cell(item.get("name").unwrap_or(&Value::Null)),
                                output::cell(item.get("domain").unwrap_or(&Value::Null)),
                                output::cell(item.get("createdAt").unwrap_or(&Value::Null)),
                            ]
                        })
                        .collect()
                })
                .unwrap_or_default();
            output::table(&["ID", "NAME", "DOMAIN", "CREATED"], &rows);
        }
        Report::Summary => {
            let mut rows = Vec::new();
            for (key, label) in [
                ("pageviews", "Pageviews"),
                ("visitors", "Visitors"),
                ("views", "Views"),
                ("bounces", "Bounces"),
                ("visitDuration", "Visit duration (s)"),
            ] {
                let metric = value.get(key);
                let count = metric
                    .and_then(|m| m.get("value"))
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                rows.push(vec![label.to_string(), output::number(count)]);
            }
            output::table(&["METRIC", "VALUE"], &rows);
        }
        Report::Timeseries => {
            let rows: Vec<Vec<String>> = value
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .map(|item| {
                            vec![
                                output::cell(item.get("date").or_else(|| item.get("name")).unwrap_or(&Value::Null)),
                                output::number(
                                    item.get("value")
                                        .and_then(|v| v.as_i64())
                                        .unwrap_or(0),
                                ),
                            ]
                        })
                        .collect()
                })
                .unwrap_or_default();
            output::table(&["DATE", "VIEWS"], &rows);
        }
        Report::Sessions => {
            let rows: Vec<Vec<String>> = value
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .map(|item| {
                            vec![
                                output::cell(item.get("createdAt").unwrap_or(&Value::Null)),
                                output::cell(item.get("url").unwrap_or(&Value::Null)),
                                output::cell(item.get("referrer").unwrap_or(&Value::Null)),
                                output::cell(item.get("browser").unwrap_or(&Value::Null)),
                                output::cell(item.get("country").unwrap_or(&Value::Null)),
                                output::number(
                                    item.get("views").and_then(|v| v.as_i64()).unwrap_or(0),
                                ),
                            ]
                        })
                        .collect()
                })
                .unwrap_or_default();
            output::table(&["WHEN", "URL", "REFERRER", "BROWSER", "COUNTRY", "VIEWS"], &rows);
        }
        _ => {
            let rows: Vec<Vec<String>> = value
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .map(|item| {
                            vec![
                                output::cell(item.get("name").unwrap_or(&Value::Null)),
                                output::number(
                                    item.get("value").and_then(|v| v.as_i64()).unwrap_or(0),
                                ),
                            ]
                        })
                        .collect()
                })
                .unwrap_or_default();
            output::table(&["NAME", "VALUE"], &rows);
        }
    }
}

/// Print a report as CSV.
fn print_csv(value: &Value) {
    let items = match value.as_array() {
        Some(items) => items,
        None => {
            println!("{}", serde_json::to_string(value).unwrap_or_default());
            return;
        }
    };

    if let Some(first) = items.first() {
        if let Some(object) = first.as_object() {
            let headers: Vec<String> = object.keys().cloned().collect();
            println!("{}", headers.join(","));
            for item in items {
                let row: Vec<String> = headers
                    .iter()
                    .map(|header| csv_cell(item.get(header).unwrap_or(&Value::Null)))
                    .collect();
                println!("{}", row.join(","));
            }
            return;
        }
    }

    for item in items {
        println!("{}", csv_cell(item));
    }
}

/// Quote a CSV cell when needed.
fn csv_cell(value: &Value) -> String {
    let text = match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    if text.contains(',') || text.contains('"') || text.contains('\n') {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text
    }
}
