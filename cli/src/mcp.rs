//! The MCP server — Sumimi's numbers, as tools an agent can call.
//!
//! An [MCP](https://modelcontextprotocol.io) server: JSON-RPC 2.0 over stdio,
//! the way Claude Code, Claude Desktop and the other clients expect. It offers
//! **three** tools:
//!
//! | tool | what it does |
//! | --- | --- |
//! | `sumimi_report` | run a report — the summary, the top pages, the traffic sources, … |
//! | `sumimi_record` | record a page view or a custom event |
//! | `sumimi_ask` | ask a question, get the answer computed from the database |
//!
//! ## There are no write tools
//!
//! Nothing here creates a website, changes a setting, prunes data or erases
//! anything. The server can look, count and record — and nothing else.
//! Recording is the one thing that writes, because it is what the tracker
//! does, and it is the point of the tool.
//!
//! ## Where the numbers come from
//!
//! The same code the dashboard uses (`service::reports`, `service::analyst`).
//! There is one implementation of "the top pages", and both the agent and the
//! dashboard call it, so they cannot disagree.

use std::io::{BufRead, Write};
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use sumimi_service::models::{Hit, HitKind};

use crate::source::{parse_time, Range, Report, Source};

/// The protocol version this server speaks.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// The protocol versions this server can speak.
///
/// A client that asks for one of these gets it back; a client that asks for
/// something else gets the newest one this server knows — and decides whether
/// it can live with it.
pub const SUPPORTED_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

/// The reports the `sumimi_report` tool can run, and what each one is.
pub const REPORTS: [(&str, &str); 19] = [
    (
        "summary",
        "The headline numbers: page views, visitors, sessions, bounces and the average visit, with the change against the period before.",
    ),
    (
        "timeseries",
        "The page views over time, one row per hour or day.",
    ),
    (
        "pages",
        "The pages that were visited, most visited first.",
    ),
    (
        "referrers",
        "Where the traffic came from.",
    ),
    (
        "browsers",
        "The browser breakdown.",
    ),
    (
        "os",
        "The operating system breakdown.",
    ),
    (
        "devices",
        "The device breakdown: desktop, mobile, tablet.",
    ),
    (
        "countries",
        "The country breakdown.",
    ),
    (
        "languages",
        "The interface language breakdown.",
    ),
    (
        "screens",
        "The screen size breakdown.",
    ),
    (
        "events",
        "The custom events, by name.",
    ),
    (
        "channels",
        "The traffic channels: direct, Search, Social, email, paidAds, referral, affiliate, sms, llm, Shopping, Video.",
    ),
    (
        "regions",
        "The regions of the visitors.",
    ),
    (
        "cities",
        "The cities of the visitors.",
    ),
    (
        "hosts",
        "The hostnames of the sites.",
    ),
    (
        "tags",
        "The tags of the custom events.",
    ),
    (
        "query-parameters",
        "The query parameters of the visited pages.",
    ),
    (
        "sessions",
        "The session log.",
    ),
    (
        "websites",
        "The websites of this instance.",
    ),
];

/// The website of an instance: what the tools need to know about it.
#[derive(Debug, Clone)]
pub struct WebsiteInfo {
    /// The id.
    pub id: String,
    /// The domain, if it has one.
    pub domain: Option<String>,
    /// How long the data is kept.
    pub retention_days: i32,
}

/// The MCP server.
pub struct Server {
    /// Where the data comes from: a running server or a local database.
    source: Source,
    /// Whether the local database can be written to.
    writable: bool,
    /// The website the tools work on, when the call does not name one.
    default_website: Option<String>,
}

impl Server {
    /// Build the server.
    pub fn new(source: Source, writable: bool, default_website: Option<String>) -> Self {
        Self {
            source,
            writable,
            default_website,
        }
    }

    /// Build the server over a local database file.
    ///
    /// The database is opened writable when nothing else holds it, so the
    /// server can record; when a running server holds it, the file is read as
    /// a snapshot and recording is off.
    pub fn open_local(path: &Path, default_website: Option<String>) -> Result<Self> {
        let (source, writable) = open_local(path)?;
        Ok(Self::new(source, writable, default_website))
    }

    /// Run the server: read the requests from stdin, write the answers to stdout.
    ///
    /// **stdout belongs to the protocol** — nothing else may be printed there.
    /// Diagnostics go to stderr.
    pub fn run(&self) -> Result<()> {
        let stdin = std::io::stdin();
        let mut stdout = std::io::stdout();

        for line in stdin.lock().lines() {
            let line = line.context("reading a request")?;
            if line.trim().is_empty() {
                continue;
            }
            let Some(response) = self.handle_line(&line) else {
                continue;
            };
            writeln!(stdout, "{response}")?;
            stdout.flush()?;
        }

        Ok(())
    }

    /// Handle one line of the protocol.
    ///
    /// Returns the answer, or `None` when the message was a notification (a
    /// message without an id) — those get no answer.
    pub fn handle_line(&self, line: &str) -> Option<String> {
        let request: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(err) => {
                return Some(
                    json!({
                        "jsonrpc": "2.0",
                        "id": Value::Null,
                        "error": { "code": -32700, "message": format!("invalid json: {err}") },
                    })
                    .to_string(),
                )
            }
        };

        let id = match request.get("id") {
            None | Some(Value::Null) => return None,
            Some(id) => id.clone(),
        };

        let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = request.get("params").cloned().unwrap_or(Value::Null);

        let outcome = match method {
            "initialize" => Ok(self.initialize(&params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(self.tools()),
            "tools/call" => self.call_tool(&params),
            other => Err(( -32601, format!("unknown method: {other}"))),
        };

        Some(match outcome {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string(),
            Err((code, message)) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": code, "message": message },
            })
            .to_string(),
        })
    }

    /// The `initialize` answer.
    fn initialize(&self, params: &Value) -> Value {
        let asked = params
            .get("protocolVersion")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let version = if SUPPORTED_VERSIONS.contains(&asked) {
            asked
        } else {
            PROTOCOL_VERSION
        };

        json!({
            "protocolVersion": version,
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": {
                "name": "sumimi",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "instructions": "Sumimi is a self-hosted web analytics service. \
                Use sumimi_report for the numbers, sumimi_ask for a question in \
                plain language, and sumimi_record to log a page view or a custom \
                event. Every number comes from the same database the dashboard reads.",
        })
    }

    /// The tools this server offers.
    fn tools(&self) -> Value {
        let report_names: Vec<&str> = REPORTS.iter().map(|(name, _)| *name).collect();
        let report_help: Vec<String> = REPORTS
            .iter()
            .map(|(name, help)| format!("{name}: {help}"))
            .collect();

        json!({
            "tools": [
                {
                    "name": "sumimi_report",
                    "description": "Run a Sumimi report and get the numbers as JSON. \
                        It is the same report the dashboard runs, so the answer cannot \
                        disagree with what the owner sees.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "website": {
                                "type": "string",
                                "description": "The website id, or its domain. Optional when the instance has one website.",
                            },
                            "report": {
                                "type": "string",
                                "enum": report_names,
                                "description": report_help.join(" — "),
                            },
                            "from": {
                                "type": "string",
                                "description": "The start of the window: 7d, 24h, 2026-01-01 or an RFC 3339 timestamp. The last 7 days by default.",
                            },
                            "to": {
                                "type": "string",
                                "description": "The end of the window. Now by default.",
                            },
                            "limit": {
                                "type": "integer",
                                "description": "How many rows to return. 50 by default.",
                            },
                        },
                    },
                },
                {
                    "name": "sumimi_record",
                    "description": "Record a page view or a custom event for a website — \
                        the same endpoint the tracker script posts to. Use it to log a \
                        signup, a download or a purchase that happened outside the browser.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "website": {
                                "type": "string",
                                "description": "The website id, or its domain. Optional when the instance has one website.",
                            },
                            "type": {
                                "type": "string",
                                "enum": ["pageview", "event", "screenview", "identify"],
                                "description": "What to record. A pageview by default; an event needs a name.",
                            },
                            "name": {
                                "type": "string",
                                "description": "The name of the event — signup, download, purchase. It is what the events report counts.",
                            },
                            "value": {
                                "type": "string",
                                "description": "A value for the event — the price, the plan, the version.",
                            },
                            "url": {
                                "type": "string",
                                "description": "The path that was visited, e.g. /pricing.",
                            },
                            "referrer": {
                                "type": "string",
                                "description": "Where the visit came from.",
                            },
                            "hostname": {
                                "type": "string",
                                "description": "The hostname of the site.",
                            },
                            "title": {
                                "type": "string",
                                "description": "The title of the page.",
                            },
                            "screen": {
                                "type": "string",
                                "description": "The screen size, e.g. 1920x1080.",
                            },
                            "language": {
                                "type": "string",
                                "description": "The interface language, e.g. en-US.",
                            },
                            "country": {
                                "type": "string",
                                "description": "The two-letter country code, e.g. DE.",
                            },
                        },
                    },
                },
                {
                    "name": "sumimi_ask",
                    "description": "Ask a question in plain language — 'how many visitors \
                        last 30 days', 'where do my visitors come from', 'why did traffic \
                        drop' — and get the answer computed from the database. It cannot \
                        invent a number: every figure in the answer is a figure the \
                        database returned.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "website": {
                                "type": "string",
                                "description": "The website id, or its domain. Optional when the instance has one website.",
                            },
                            "question": {
                                "type": "string",
                                "description": "The question, in any language.",
                            },
                        },
                        "required": ["question"],
                    },
                },
            ]
        })
    }

    /// Run a tool.
    fn call_tool(&self, params: &Value) -> Result<Value, (i64, String)> {
        let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

        let outcome = match name {
            "sumimi_report" => self.report(&arguments),
            "sumimi_record" => self.record(&arguments),
            "sumimi_ask" => self.ask(&arguments),
            other => {
                return Err((-32602, format!("unknown tool: {other}")));
            }
        };

        // A tool that fails is not a broken protocol: the failure is the
        // answer, and the model gets to read it.
        Ok(match outcome {
            Ok(text) => json!({
                "content": [{ "type": "text", "text": text }],
                "isError": false,
            }),
            Err(err) => json!({
                "content": [{ "type": "text", "text": format!("{err:#}") }],
                "isError": true,
            }),
        })
    }

    /// `sumimi_report` — run a report.
    fn report(&self, arguments: &Value) -> Result<String> {
        let name = arguments
            .get("report")
            .and_then(|v| v.as_str())
            .unwrap_or("summary")
            .trim()
            .to_string();
        let report = parse_report(&name)?;

        let website = self.website(arguments)?;
        let range = self.range(arguments)?;
        let limit = arguments
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(50)
            .clamp(1, 1000) as i64;

        let value = self.source.report(report, Some(&website), range, limit)?;
        Ok(serde_json::to_string_pretty(&value)?)
    }

    /// `sumimi_record` — record a page view or an event.
    fn record(&self, arguments: &Value) -> Result<String> {
        let website = self.website(arguments)?;
        let kind = arguments
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("pageview")
            .trim()
            .to_string();

        let field = |name: &str| -> Option<String> {
            arguments
                .get(name)
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };

        let name = field("name");
        let url = field("url");
        let referrer = field("referrer");
        let hostname = field("hostname");
        let title = field("title");
        let screen = field("screen");
        let language = field("language");
        let country = field("country");
        let value = field("value");

        match &self.source {
            Source::Remote { base, .. } => {
                // The same endpoint the tracker posts to.
                let mut payload = serde_json::Map::new();
                payload.insert("website".into(), json!(website));
                for (key, item) in [
                    ("url", &url),
                    ("referrer", &referrer),
                    ("hostname", &hostname),
                    ("title", &title),
                    ("screen", &screen),
                    ("language", &language),
                    ("country", &country),
                    ("name", &name),
                    ("value", &value),
                ] {
                    if let Some(item) = item {
                        payload.insert(key.into(), json!(item));
                    }
                }

                let agent = ureq::Agent::new_with_defaults();
                agent
                    .post(&format!("{base}/api/send"))
                    .send_json(json!({
                        "type": if kind == "pageview" { "event" } else { &kind },
                        "payload": payload,
                    }))
                    .map_err(|err| anyhow!("recording: {err}"))?;

                Ok(match &name {
                    Some(name) => format!("recorded the event '{name}' for {website}"),
                    None => format!("recorded a page view for {website}"),
                })
            }
            Source::Local { storage, .. } => {
                if !self.writable {
                    return Err(anyhow!(
                        "the database is open read-only (a server is using it) — \
                         recording needs a server that is not running, or a copy of the data"
                    ));
                }

                let retention = self
                    .websites()?
                    .into_iter()
                    .find(|site| site.id == website)
                    .map(|site| site.retention_days)
                    .unwrap_or(0);

                let hit = Hit {
                    website_id: website.clone(),
                    visit_id: session_id(&website),
                    ip: ip(&website),
                    ip_prefix: ip_prefix(&website),
                    user_agent: user_agent(&website),
                    retention_days: retention,
                    kind: match kind.as_str() {
                        "event" => HitKind::Event,
                        "screenview" => HitKind::Screenview,
                        "identify" => HitKind::Identify,
                        _ => HitKind::Pageview,
                    },
                    created_at: chrono::Utc::now().naive_utc(),
                    url: url.clone(),
                    title: title.clone(),
                    referrer: referrer.clone(),
                    hostname: hostname.clone(),
                    screen: screen.clone(),
                    language: language.clone(),
                    country: country.clone(),
                    name: name.clone(),
                    data: value.clone(),
                    ..Hit::default()
                };

                storage
                    .write_batch(&[hit])
                    .map_err(|err| anyhow!("recording: {err}"))?;

                Ok(match &name {
                    Some(name) => format!("recorded the event '{name}' for {website}"),
                    None => format!("recorded a page view for {website}"),
                })
            }
        }
    }

    /// `sumimi_ask` — ask a question, get an answer.
    fn ask(&self, arguments: &Value) -> Result<String> {
        let question = arguments
            .get("question")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if question.is_empty() {
            return Err(anyhow!("the question is empty"));
        }

        let website = self.website(arguments)?;

        match &self.source {
            Source::Local { storage, .. } => {
                let answer = sumimi_service::analyst::ask(
                    storage.as_ref(),
                    &website,
                    &question,
                    chrono::Utc::now().naive_utc(),
                )
                .map_err(|err| anyhow!("{err:?}"))?;
                Ok(answer.text)
            }
            Source::Remote { base, token } => {
                let agent = ureq::Agent::new_with_defaults();
                let value: Value = agent
                    .post(&format!("{base}/api/websites/{website}/ask"))
                    .header("authorization", &format!("Bearer {token}"))
                    .send_json(json!({ "question": question }))
                    .map_err(|err| anyhow!("asking: {err}"))?
                    .body_mut()
                    .read_json()
                    .map_err(|err| anyhow!("reading the answer: {err}"))?;

                Ok(value
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string())
            }
        }
    }

    /// The website a call is about.
    ///
    /// The call may name it; otherwise the server's default; otherwise the one
    /// website of the instance — and when there is more than one, the caller is
    /// told which ones there are.
    fn website(&self, arguments: &Value) -> Result<String> {
        let named = arguments
            .get("website")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());

        if let Some(key) = named.or(self.default_website.as_deref()) {
            return self.resolve_website(key);
        }

        let websites = self.websites()?;
        match websites.len() {
            0 => Err(anyhow!("this instance has no website yet")),
            1 => Ok(websites[0].id.clone()),
            _ => {
                let names: Vec<&str> = websites.iter().map(|site| site.id.as_str()).collect();
                Err(anyhow!(
                    "which website? name one of: {}",
                    names.join(", ")
                ))
            }
        }
    }

    /// The id of a website — the key may be the id or the domain.
    fn resolve_website(&self, key: &str) -> Result<String> {
        let websites = self.websites()?;
        for site in &websites {
            if site.id == key {
                return Ok(site.id.clone());
            }
            if let Some(domain) = site.domain.as_deref() {
                if domain.eq_ignore_ascii_case(key) {
                    return Ok(site.id.clone());
                }
            }
        }
        Err(anyhow!("unknown website: {key}"))
    }

    /// The websites of the instance.
    fn websites(&self) -> Result<Vec<WebsiteInfo>> {
        match &self.source {
            Source::Local { storage, .. } => Ok(storage
                .websites()
                .map_err(|err| anyhow!("listing the websites: {err}"))?
                .into_iter()
                .map(|site| WebsiteInfo {
                    id: site.id,
                    domain: site.domain,
                    retention_days: site.retention_days,
                })
                .collect()),
            Source::Remote { base, token } => {
                let agent = ureq::Agent::new_with_defaults();
                let value: Value = agent
                    .get(&format!("{base}/api/websites"))
                    .header("authorization", &format!("Bearer {token}"))
                    .call()
                    .map_err(|err| anyhow!("listing the websites: {err}"))?
                    .body_mut()
                    .read_json()
                    .map_err(|err| anyhow!("reading the websites: {err}"))?;

                Ok(value
                    .as_array()
                    .map(|list| {
                        list.iter()
                            .map(|site| WebsiteInfo {
                                id: site
                                    .get("id")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or_default()
                                    .to_string(),
                                domain: site
                                    .get("domain")
                                    .and_then(|v| v.as_str())
                                    .map(str::to_string),
                                retention_days: site
                                    .get("retentionDays")
                                    .and_then(|v| v.as_i64())
                                    .unwrap_or(0) as i32,
                            })
                            .collect()
                    })
                    .unwrap_or_default())
            }
        }
    }

    /// The window a call is about.
    fn range(&self, arguments: &Value) -> Result<Range> {
        let end = match arguments.get("to").and_then(|v| v.as_str()) {
            Some(value) => parse_time(value)?,
            None => Range::default_window().end,
        };
        let start = match arguments.get("from").and_then(|v| v.as_str()) {
            Some(value) => parse_time(value)?,
            None => end - chrono::Duration::days(7),
        };
        Ok(Range { start, end })
    }
}

/// The session an MCP-recorded event belongs to.
///
/// One session per website per half hour — the same 30-minute bucket the
/// tracker uses — so the events recorded by an agent do not each become a
/// session of their own.
fn session_id(website: &str) -> sumimi_service::session::VisitId {
    let bucket = chrono::Utc::now().timestamp() / (30 * 60);
    sumimi_service::session::client_session_id(&format!("sumimi-mcp-{website}-{bucket}"))
}

/// The visitor an MCP-recorded event belongs to.
///
/// One stable visitor per website: the agent does not know who the person is,
/// and counting every recorded event as a new visitor would be a lie.
fn visitor_id(website: &str) -> uuid::Uuid {
    sumimi_service::ua::uuid_from_text(&format!("sumimi-mcp-{website}"))
}

/// The address the agent's events come from.
///
/// The agent does not know who the person is: one stable address and one
/// stable user agent per website, so the events of an agent are one visitor.
fn ip(website: &str) -> String {
    let (ip, _, _) = sumimi_service::testing::visitor_identity(&visitor_id(website));
    ip
}

/// The prefix of that address — half of the visitor's identity.
fn ip_prefix(website: &str) -> String {
    let (_, prefix, _) = sumimi_service::testing::visitor_identity(&visitor_id(website));
    prefix
}

/// The user agent of that address — the other half of the visitor's identity.
fn user_agent(website: &str) -> String {
    let (_, _, agent) = sumimi_service::testing::visitor_identity(&visitor_id(website));
    agent
}

/// The report a name refers to.
fn parse_report(name: &str) -> Result<Report> {
    Ok(match name {
        "summary" => Report::Summary,
        "timeseries" => Report::Timeseries,
        "pages" => Report::Pages,
        "referrers" => Report::Referrers,
        "browsers" => Report::Browsers,
        "os" => Report::Os,
        "devices" => Report::Devices,
        "countries" => Report::Countries,
        "languages" => Report::Languages,
        "screens" => Report::Screens,
        "events" => Report::Events,
        "sessions" => Report::Sessions,
        "websites" => Report::Websites,
        other => {
            let names: Vec<&str> = REPORTS.iter().map(|(name, _)| *name).collect();
            return Err(anyhow!(
                "unknown report '{other}' (one of: {})",
                names.join(", ")
            ));
        }
    })
}

/// Open a local database for the MCP server.
///
/// The database is opened **writable** when nothing else holds it, so the
/// server can record. When a running server holds it, DuckDB refuses the
/// second writer — the file is then read as a snapshot, and recording is off.
pub fn open_local(path: &Path) -> Result<(Source, bool)> {
    match Source::local_writable(path) {
        Ok(source) => Ok((source, true)),
        Err(err) => {
            let source = Source::local(path)
                .with_context(|| format!("opening {}", path.display()))?;
            eprintln!(
                "[sumimi] the database is in use by a running server ({err}); \
                 it is read-only, so recording is off"
            );
            Ok((source, false))
        }
    }
}
