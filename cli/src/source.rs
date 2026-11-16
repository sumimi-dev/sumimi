//! Where the CLI gets its data: a local database, a Parquet archive, or a running server.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use duckdb::{AccessMode, Config, Connection};
use serde_json::{json, Value};

use sumimi_service::pool::Db;
use sumimi_service::storage::{Dimension, ReportQuery, Storage};

/// A time window.
#[derive(Debug, Clone, Copy)]
pub struct Range {
    /// Start of the window (inclusive).
    pub start: chrono::NaiveDateTime,
    /// End of the window (exclusive).
    pub end: chrono::NaiveDateTime,
}

impl Range {
    /// The default window: the last 7 days.
    pub fn default_window() -> Self {
        let end = chrono::Utc::now().naive_utc();
        Range {
            start: end - chrono::Duration::days(7),
            end,
        }
    }

    /// Unix milliseconds, for the HTTP API.
    pub fn as_millis(&self) -> (i64, i64) {
        let to_ms = |t: chrono::NaiveDateTime| t.and_utc().timestamp_millis();
        (to_ms(self.start), to_ms(self.end))
    }
}

/// Parse a time expression.
///
/// Accepts `7d`, `24h`, `30m`, `now`, a date (`2026-01-01`) or an RFC 3339 timestamp.
pub fn parse_time(value: &str) -> Result<chrono::NaiveDateTime> {
    let value = value.trim();
    let now = chrono::Utc::now().naive_utc();

    if let Some(number) = value.strip_suffix('d') {
        let days: i64 = number.parse().context("invalid number of days")?;
        return Ok(now - chrono::Duration::days(days));
    }
    if let Some(number) = value.strip_suffix('h') {
        let hours: i64 = number.parse().context("invalid number of hours")?;
        return Ok(now - chrono::Duration::hours(hours));
    }
    if let Some(number) = value.strip_suffix('m') {
        let minutes: i64 = number.parse().context("invalid number of minutes")?;
        return Ok(now - chrono::Duration::minutes(minutes));
    }
    if value == "now" {
        return Ok(now);
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        return Ok(date.and_hms_opt(0, 0, 0).unwrap_or(now));
    }
    if let Ok(date) = chrono::DateTime::parse_from_rfc3339(value) {
        return Ok(date.naive_utc());
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S") {
        return Ok(dt);
    }

    Err(anyhow!(
        "cannot understand the time '{value}' (try 7d, 24h, 2026-01-01 or an RFC 3339 timestamp)"
    ))
}

/// A report the CLI can produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Report {
    /// The list of websites.
    Websites,
    /// The summary metrics.
    Summary,
    /// The top pages.
    Pages,
    /// The traffic sources.
    Referrers,
    /// The browser breakdown.
    Browsers,
    /// The operating system breakdown.
    Os,
    /// The device breakdown.
    Devices,
    /// The country breakdown.
    Countries,
    /// The language breakdown.
    Languages,
    /// The screen size breakdown.
    Screens,
    /// Custom events.
    Events,
    /// The traffic channels.
    Channels,
    /// The regions of the visitors.
    Regions,
    /// The cities of the visitors.
    Cities,
    /// The hostnames of the sites.
    Hosts,
    /// The tags of the custom events.
    Tags,
    /// The query parameters of the visited pages.
    QueryParameters,
    /// The session log.
    Sessions,
    /// The time series.
    Timeseries,
}

/// Where the data comes from.
pub enum Source {
    /// A local database or Parquet archive, read through the storage backend.
    Local {
        /// The backend.
        storage: Arc<dyn Storage>,
        /// The path, for messages.
        path: PathBuf,
    },
    /// A running Sumimi server.
    Remote {
        /// The base URL, without a trailing slash.
        base: String,
        /// The bearer token.
        token: String,
    },
}

impl Source {
    /// Open a local database file (read-only).
    ///
    /// DuckDB takes an exclusive lock on the file, so a database that a running server
    /// holds cannot be opened directly. In that case we work on a snapshot copy, which
    /// is exactly what an offline analysis wants.
    pub fn local(path: &Path) -> Result<Self> {
        match Self::open_local(path, AccessMode::ReadOnly) {
            Ok(storage) => Ok(Source::Local {
                storage: Arc::new(storage),
                path: path.to_path_buf(),
            }),
            Err(err) => {
                // **Only a lock is worth a snapshot.** An instance whose shape is
                // wrong will be just as wrong in a copy — so the error the
                // operator needs is the one about the shape, not the one about a
                // lock that was never there.
                if !is_locked(&err) {
                    return Err(err);
                }
                let snapshot = Self::snapshot(path)?;
                let storage = Self::open_local(&snapshot, AccessMode::ReadOnly)?;
                eprintln!(
                    "[sumimi] the database is in use by a running server ({err}); \
                     analysing a snapshot copy instead"
                );
                Ok(Source::Local {
                    storage: Arc::new(storage),
                    path: snapshot,
                })
            }
        }
    }

    /// Open a Parquet archive as a source (read-only, queried in place).
    pub fn parquet(pattern: &str) -> Result<Self> {
        let storage = Db::open_archive(pattern)?;
        Ok(Source::Local {
            storage: Arc::new(storage),
            path: PathBuf::from(pattern),
        })
    }

    /// Open a local database file for writing.
    pub fn local_writable(path: &Path) -> Result<Self> {
        let storage = Self::open_local(path, AccessMode::ReadWrite)?;
        Ok(Source::Local {
            storage: Arc::new(storage),
            path: path.to_path_buf(),
        })
    }

    /// Open a backend with the given access mode.
    ///
    /// **The site's files come with it.** Since v12 the facts of a site live in
    /// a file of that site's own (see [`sumimi_service::db`]), so the instance
    /// is the main file **and** the directory beside it. The main file is
    /// opened with the given mode and the site's files are attached under the
    /// same mode — DuckDB gives an `ATTACH` the access mode of the database it
    /// is attached to, so a read-only reader gets read-only facts.
    fn open_local(path: &Path, mode: AccessMode) -> Result<Db> {
        let config = Config::default().access_mode(mode)?;
        let conn = Connection::open_with_flags(path, config)
            .with_context(|| format!("opening {}", path.display()))?;
        // **The shape is checked before the facts are attached.** An instance
        // whose site's files are missing cannot be attached at all — and the
        // error that comes out of the attach says nothing about what is really
        // wrong. So the shape is asked about first, and the operator gets the
        // answer they can act on.
        check_shape(path, &conn)?;
        Db::from_connection(path, conn)
    }

    /// Copy the instance (the main file, its WAL and the sites' files) to a
    /// temporary directory.
    ///
    /// **The sites' directory comes along**, and it keeps its name: the site's
    /// file is found through the **main file's** path (`<main>.sites/`), and
    /// the alias is worked out from the site's id. So the snapshot has to
    /// hold the main file and a directory of that name beside it — a snapshot
    /// without the facts is a snapshot of an empty site.
    pub fn snapshot(path: &Path) -> Result<PathBuf> {
        let dir = std::env::temp_dir().join(format!("sumimi-snapshot-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let target = dir.join("sumimi.duckdb");
        let _ = std::fs::remove_file(&target);
        std::fs::copy(path, &target)
            .with_context(|| format!("copying {} to {}", path.display(), target.display()))?;
        // The write-ahead log holds the most recent commits.
        let wal = path.with_extension("duckdb.wal");
        if wal.exists() {
            let _ = std::fs::copy(&wal, target.with_extension("duckdb.wal"));
        }
        copy_sites(path, &target)?;
        Ok(target)
    }

    /// Log in to a remote server and return a source.
    pub fn remote(
        base: &str,
        token: Option<&str>,
        username: Option<&str>,
        password: Option<&str>,
    ) -> Result<Self> {
        let base = base.trim_end_matches('/').to_string();

        if let Some(token) = token {
            return Ok(Source::Remote {
                base,
                token: token.to_string(),
            });
        }

        let (username, password) = match (username, password) {
            (Some(u), Some(p)) => (u.to_string(), p.to_string()),
            _ => {
                let stored = stored_credentials()?;
                match stored {
                    Some((u, p)) => (u, p),
                    None => {
                        return Err(anyhow!(
                            "no credentials: pass --username/--password, or --token, \
                             or log in once with `sumimi login`"
                        ))
                    }
                }
            }
        };

        let token = login(&base, &username, &password)?;
        Ok(Source::Remote { base, token })
    }

    /// Run a report and return the result as JSON.
    pub fn report(
        &self,
        report: Report,
        website: Option<&str>,
        range: Range,
        limit: i64,
    ) -> Result<Value> {
        match self {
            Source::Local { storage, .. } => {
                local_report(storage.as_ref(), report, website, range, limit)
            }
            Source::Remote { base, token } => {
                remote_report(base, token, report, website, range, limit)
            }
        }
    }

    /// Delete data that is older than the retention window of its website.
    pub fn prune(&self, website: Option<&str>) -> Result<(u64, u64)> {
        match self {
            Source::Local { storage, .. } => storage.prune(website),
            Source::Remote { .. } => Err(anyhow!(
                "pruning over HTTP is not supported in this version; \
                 stop the server and run `sumimi prune --db <path>`"
            )),
        }
    }

    /// Erase a website and all of its data.
    pub fn erase(&self, website: &str) -> Result<u64> {
        match self {
            Source::Local { storage, .. } => storage.erase(website),
            Source::Remote { .. } => Err(anyhow!(
                "erasing over HTTP is not supported in this version; \
                 run `sumimi erase --db <path> <website> against a stopped server"
            )),
        }
    }
}

/// Read the stored credentials.
fn stored_credentials() -> Result<Option<(String, String)>> {
    let path = credentials_path();
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&path)?;
    let value: Value = serde_json::from_str(&content)?;
    let username = value.get("username").and_then(|v| v.as_str()).map(str::to_string);
    let password = value.get("password").and_then(|v| v.as_str()).map(str::to_string);
    Ok(username.zip(password))
}

/// Is this the failure of a **lock** — a running server holding the file?
///
/// **Only a lock is worth a snapshot.** Every other failure — a shape that is
/// wrong, a file that is not there — will be just as wrong in a copy, and
/// hiding it behind "the server is running" sends the operator looking in the
/// wrong place.
///
/// DuckDB says so in the message: `Could not set lock on file …`.
fn is_locked(err: &anyhow::Error) -> bool {
    let message = format!("{err}");
    let message = message.to_lowercase();
    message.contains("lock") && (message.contains("could not set lock") || message.contains("conflicting lock"))
}

/// Is the instance the shape we expect?
///
/// **A database of the old shape is not opened silently.** The facts of a
/// site live in a file of that site's own (since v12), so a main file
/// without the sites' directory beside it holds **no facts at all** — and a
/// report over it would answer "0 page views" with the same confidence as a
/// report over a site that really had none. ⇒ ⇒ So the shape is checked, and
/// an instance that cannot be read says so.
///
/// The flat shape — the facts in the file itself (the archive, the tools that
/// measure things) — is left alone: there is nothing to look for.
fn check_shape(path: &Path, conn: &Connection) -> Result<()> {
    // Does the file hold the facts itself? Then it is the flat shape.
    let flat: i64 = conn
        .query_row(
            "SELECT count(*) FROM information_schema.tables \
             WHERE table_name = 'website_event' AND table_catalog = 'main_db'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    if flat > 0 {
        return Ok(());
    }

    let dir = sumimi_service::db::sites_dir(path);
    if !dir.is_dir() {
        return Err(anyhow!(
            "the database {} has no facts in it: the sites' files are missing ({} is not there).\n\
             Since v12 the facts of a site live in a file of that site's own — a main file \
             without them holds the sites and nothing else. Start the server once so it can \
             write the files, or point --db at the right file.",
            path.display(),
            dir.display()
        ));
    }
    Ok(())
}

/// Copy the sites' directory of an instance beside a copy of its main file.
///
/// **The directory keeps its name.** The site's file is found through the
/// **main file's** path — `sites_dir()` appends `.sites` to the main file's
/// name — so the copy has to be named the same way, or the copied facts are
/// not the ones that are opened.
fn copy_sites(path: &Path, target: &Path) -> Result<()> {
    let from = sumimi_service::db::sites_dir(path);
    if !from.is_dir() {
        return Ok(());
    }
    let to = sumimi_service::db::sites_dir(target);
    let _ = std::fs::remove_dir_all(&to);
    std::fs::create_dir_all(&to)?;
    copy_dir(&from, &to)
}

/// Copy a directory, file by file.
fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let source = entry.path();
        let target = to.join(entry.file_name());
        if source.is_dir() {
            std::fs::create_dir_all(&target)?;
            copy_dir(&source, &target)?;
        } else {
            std::fs::copy(&source, &target).with_context(|| {
                format!("copying {} to {}", source.display(), target.display())
            })?;
        }
    }
    Ok(())
}

/// Where the credentials are stored.
pub fn credentials_path() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home).join(".config")
        });
    base.join("sumimi").join("credentials.json")
}

/// Store the credentials (mode 0600).
pub fn store_credentials(username: &str, password: &str) -> Result<PathBuf> {
    let path = credentials_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&json!({ "username": username, "password": password }))?,
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(path)
}

/// Log in and return a token.
fn login(base: &str, username: &str, password: &str) -> Result<String> {
    let agent = ureq::Agent::new_with_defaults();
    let mut response = agent
        .post(&format!("{base}/api/login"))
        .send_json(json!({ "username": username, "password": password }))
        .map_err(|e| anyhow!("login failed: {e}"))?;

    let body: Value = response
        .body_mut()
        .read_json()
        .map_err(|e| anyhow!("reading the login response: {e}"))?;

    body.get("token")
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("the server did not return a token"))
}

/// Run a report against the HTTP API.
fn remote_report(
    base: &str,
    token: &str,
    report: Report,
    website: Option<&str>,
    range: Range,
    limit: i64,
) -> Result<Value> {
    let agent = ureq::Agent::new_with_defaults();
    let (start, end) = range.as_millis();

    let path = match report {
        Report::Websites => "/api/websites".to_string(),
        _ => {
            let website = website.ok_or_else(|| anyhow!("this report needs a website id"))?;
            let suffix = match report {
                Report::Summary => "/stats",
                Report::Pages => "/stats/pages",
                Report::Referrers => "/stats/referrers",
                Report::Browsers => "/stats/browsers",
                Report::Os => "/stats/os",
                Report::Devices => "/stats/devices",
                Report::Countries => "/stats/countries",
                Report::Languages => "/stats/languages",
                Report::Screens => "/stats/screens",
                Report::Events => "/stats/events",
                Report::Channels => "/stats/channels",
                Report::Regions => "/stats/regions",
                Report::Cities => "/stats/cities",
                Report::Hosts => "/stats/hosts",
                Report::Tags => "/stats/tags",
                Report::QueryParameters => "/stats/query-parameters",
                Report::Sessions => "/sessions",
                Report::Timeseries => "/stats/timeseries",
                Report::Websites => unreachable!(),
            };
            format!("/api/websites/{website}{suffix}")
        }
    };

    let url = format!("{base}{path}?startAt={start}&endAt={end}&limit={limit}");
    let mut response = agent
        .get(&url)
        .header("authorization", &format!("Bearer {token}"))
        .call()
        .map_err(|e| anyhow!("request failed: {e}"))?;

    response
        .body_mut()
        .read_json()
        .map_err(|e| anyhow!("reading the response: {e}"))
}

/// Run a report against a local backend.
///
/// This calls the **same** functions the HTTP API calls, so the local mode and the API
/// can never disagree.
fn local_report(
    storage: &dyn Storage,
    report: Report,
    website: Option<&str>,
    range: Range,
    limit: i64,
) -> Result<Value> {
    let website_id = if report == Report::Websites {
        String::new()
    } else {
        website
            .map(str::to_string)
            .ok_or_else(|| anyhow!("this report needs a website id"))?
    };

    let mut query = ReportQuery::new(&website_id, sumimi_service::storage::TimeRange {
        start: range.start,
        end: range.end,
    });
    query.limit = limit.clamp(1, 1000);

    let value = match report {
        Report::Websites => serde_json::to_value(storage.websites()?)?,
        Report::Summary => serde_json::to_value(storage.summary_with_change(&query)?)?,
        Report::Timeseries => serde_json::to_value(storage.timeseries(&query)?)?,
        Report::Pages => serde_json::to_value(storage.breakdown(&query, Dimension::Url)?)?,
        Report::Referrers => {
            let mut rows = storage.breakdown(&query, Dimension::Referrer)?;
            for row in &mut rows {
                if row.name == "unknown" {
                    row.name = "(direct)".to_string();
                }
            }
            serde_json::to_value(rows)?
        }
        Report::Browsers => serde_json::to_value(storage.breakdown(&query, Dimension::Browser)?)?,
        Report::Os => serde_json::to_value(storage.breakdown(&query, Dimension::Os)?)?,
        Report::Devices => serde_json::to_value(storage.breakdown(&query, Dimension::Device)?)?,
        Report::Countries => serde_json::to_value(storage.breakdown(&query, Dimension::Country)?)?,
        Report::Languages => {
            serde_json::to_value(storage.breakdown(&query, Dimension::Language)?)?
        }
        Report::Screens => serde_json::to_value(storage.breakdown(&query, Dimension::Screen)?)?,
        Report::Events => serde_json::to_value(storage.events(&query)?)?,
        Report::Channels => serde_json::to_value(storage.breakdown(&query, Dimension::Channel)?)?,
        Report::Regions => serde_json::to_value(storage.breakdown(&query, Dimension::Region)?)?,
        Report::Cities => serde_json::to_value(storage.breakdown(&query, Dimension::City)?)?,
        Report::Hosts => serde_json::to_value(storage.breakdown(&query, Dimension::Host)?)?,
        Report::Tags => serde_json::to_value(storage.breakdown(&query, Dimension::Tag)?)?,
        Report::QueryParameters => serde_json::to_value(
            sumimi_service::storage::parse_query_parameters(&storage.query_strings(&query)?),
        )?,
        Report::Sessions => serde_json::to_value(storage.sessions(&query)?)?,
    };

    Ok(value)
}
