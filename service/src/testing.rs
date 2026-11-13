//! The traffic a test or a benchmark needs.
//!
//! A [`Hit`] has more than forty fields, and most callers care about a handful
//! of them. Spelling the rest out (`..Default::default()`) is noisy; spelling
//! **all** of them out is worse. The builder names the fields, so a caller
//! writes
//!
//! ```
//! use sumimi_service::testing::HitBuilder;
//!
//! let hit = HitBuilder::new("w1", "s1", "v1")
//!     .url("/one")
//!     .referrer("news.ycombinator.com")
//!     .country("DE")
//!     .build();
//!
//! assert_eq!(hit.url.as_deref(), Some("/one"));
//! assert_eq!(hit.referrer.as_deref(), Some("news.ycombinator.com"));
//! // The fields nobody named keep the defaults they have everywhere else.
//! assert_eq!(hit.title, None);
//! ```
//!
//! and the fields it does not name keep the defaults they have everywhere else.

use chrono::NaiveDateTime;
use uuid::Uuid;

use crate::models::{Hit, HitKind};
use crate::ua::uuid_from_text;

/// The text fields of a hit.
///
/// Each one is set the same way, so they are written once.
macro_rules! text_fields {
    ($($name:ident),* $(,)?) => {
        $(
            /// Set the field.
            pub fn $name(mut self, value: impl Text) -> Self {
                self.hit.$name = value.text();
                self
            }
        )*
    };
}

/// The number fields of a hit.
macro_rules! number_fields {
    ($($name:ident),* $(,)?) => {
        $(
            /// Set the field.
            pub fn $name(mut self, value: impl Into<f64>) -> Self {
                self.hit.$name = Some(value.into());
                self
            }
        )*
    };
}

/// A value for a text field.
///
/// A field may have **no** value, and that is not the same as having an empty
/// one: a row with no referrer and a row with an empty referrer are two
/// different rows, and the deep-read check says so.
pub trait Text {
    /// The value, or nothing.
    fn text(self) -> Option<String>;
}

impl Text for &str {
    fn text(self) -> Option<String> {
        Some(self.to_string())
    }
}

impl Text for String {
    fn text(self) -> Option<String> {
        Some(self)
    }
}

impl Text for &String {
    fn text(self) -> Option<String> {
        Some((*self).clone())
    }
}

impl Text for Option<&str> {
    fn text(self) -> Option<String> {
        self.map(str::to_string)
    }
}

/// A hit under construction.
///
/// The identity of a visitor: the address, its prefix and the user agent.
///
/// A visitor is the pair (the prefix, the user agent) — that is what the
/// reports count, and it is what is stored. A test names its visitors, so a
/// name is turned into a pair that stands for it: the same name always gives
/// the same pair, and two names give two different ones.
pub fn visitor_identity(id: &Uuid) -> (String, String, String) {
    let bytes = id.as_bytes();
    let prefix = format!("{}.{}.{}", bytes[0], bytes[1], bytes[2]);
    let agent = format!(
        "sumimi-test/{:02x}{:02x}{:02x}{:02x}",
        bytes[3], bytes[4], bytes[5], bytes[6]
    );
    let ip = format!("{prefix}.1");
    (ip, prefix, agent)
}

/// Every setter returns the builder, so the fields a caller cares about are
/// named in a row and the rest are left alone.
#[derive(Debug, Clone)]
pub struct HitBuilder {
    hit: Hit,
}

impl HitBuilder {
    /// A page view for a website, in a session, seen by a visitor, happening now.
    ///
    /// The session and the visitor are named, not generated: the same text
    /// always means the same session, so a test can put two hits in one
    /// session by naming the same session twice.
    pub fn new(website_id: &str, session: &str, visitor: &str) -> Self {
        let visitor_id = uuid_from_text(visitor);
        let (ip, ip_prefix, user_agent) = visitor_identity(&visitor_id);
        Self {
            hit: Hit {
                website_id: website_id.to_string(),
                visit_id: crate::session::client_session_id(session),
                visitor_id,
                ip,
                ip_prefix,
                user_agent,
                kind: HitKind::Pageview,
                created_at: chrono::Utc::now().naive_utc(),
                ..Hit::default()
            },
        }
    }

    /// When it happened.
    pub fn at(mut self, at: NaiveDateTime) -> Self {
        self.hit.created_at = at;
        self
    }

    /// When the session started.
    pub fn session_start(mut self, at: NaiveDateTime) -> Self {
        self.hit.session_start = Some(at);
        self
    }

    /// What kind of hit it is.
    pub fn kind(mut self, kind: HitKind) -> Self {
        self.hit.kind = kind;
        self
    }

    /// How long the website keeps its data.
    pub fn retention_days(mut self, days: i32) -> Self {
        self.hit.retention_days = days;
        self
    }

    /// The session, by id.
    pub fn visit_id(mut self, id: crate::session::VisitId) -> Self {
        self.hit.visit_id = id;
        self
    }

    /// The visitor, by id.
    ///
    /// A visitor is the pair (the address prefix, the user agent) — the id is
    /// the hash of it — so an id is turned into the pair that stands for it.
    pub fn visitor_id(mut self, id: Uuid) -> Self {
        let (ip, ip_prefix, user_agent) = visitor_identity(&id);
        self.hit.visitor_id = id;
        self.hit.ip = ip;
        self.hit.ip_prefix = ip_prefix;
        self.hit.user_agent = user_agent;
        self
    }

    /// The visitor's address.
    pub fn ip(mut self, ip: &str) -> Self {
        self.hit.ip = ip.to_string();
        self
    }

    /// The visitor's address prefix — half of the visitor's identity.
    pub fn ip_prefix(mut self, prefix: &str) -> Self {
        self.hit.ip_prefix = prefix.to_string();
        self
    }

    /// The visitor's user agent — the other half of the visitor's identity.
    pub fn user_agent(mut self, agent: &str) -> Self {
        self.hit.user_agent = agent.to_string();
        self
    }

    /// The identity the site owner knows the visitor by, by name.
    pub fn user(mut self, user: &str) -> Self {
        self.hit.user_id = Some(uuid_from_text(user));
        self
    }

    /// The identity the site owner knows the visitor by, by id.
    pub fn user_id(mut self, id: Uuid) -> Self {
        self.hit.user_id = Some(id);
        self
    }

    text_fields!(
        url,
        url_query,
        title,
        referrer,
        referrer_path,
        referrer_query,
        referrer_name,
        hostname,
        channel,
        browser,
        browser_version,
        os,
        os_version,
        device,
        screen,
        language,
        country,
        region,
        city,
        utm_source,
        utm_medium,
        utm_campaign,
        utm_content,
        utm_term,
        gclid,
        fbclid,
        msclkid,
        ttclid,
        lifatid,
        twclid,
        name,
        tag,
        data,
    );

    number_fields!(latitude, longitude, lcp, inp, cls, fcp, ttfb);

    /// The hit.
    pub fn build(self) -> Hit {
        self.hit
    }
}

/// The traffic the probes and the benchmarks write.
///
/// A site with five pages, five referrers (one of them empty — that is direct
/// traffic), four browsers, four systems and five countries. Three views per
/// session, and the sessions spread over thirty days.
///
/// Every field a page view carries is filled, so the rows on disk are the size
/// a real site's rows are.
pub fn traffic(views: i64) -> Vec<Hit> {
    const PAGES: [&str; 5] = [
        "/",
        "/posts/duckdb-vs-postgres",
        "/posts/self-hosting-on-a-budget",
        "/docs/install",
        "/tags/rust",
    ];
    const REFERRERS: [&str; 5] = [
        "news.ycombinator.com",
        "www.google.com",
        "duckduckgo.com",
        "github.com",
        "",
    ];
    const BROWSERS: [&str; 4] = ["Chrome", "Safari", "Firefox", "Edge"];
    const SYSTEMS: [&str; 4] = ["Windows", "macOS", "iOS", "Android"];
    const COUNTRIES: [&str; 5] = ["US", "CN", "DE", "GB", "JP"];
    /// A handful of user agents — the real world has a few thousand, not one
    /// per visitor, and the storage cost depends on that.
    const AGENTS: [&str; 4] = [
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
        "Mozilla/5.0 (iPhone; CPU iPhone OS 17_2 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.2 Mobile/15E148 Safari/604.1",
        "Mozilla/5.0 (X11; Linux x86_64; rv:121.0) Gecko/20100101 Firefox/121.0",
    ];

    let base = chrono::NaiveDate::from_ymd_opt(2026, 3, 1)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();

    (0..views)
        .map(|number| {
            let index = number as usize;
            let session = number / 3;
            let referrer = REFERRERS[index % REFERRERS.len()];
            let channel = crate::channels::channel(&crate::channels::ChannelInput::new(
                Some(referrer),
                Some("utm_source=twitter&utm_medium=cpc"),
                Some("cpc"),
                Some("twitter"),
                Some("example.com"),
            ));

            // A session is three page views, a few minutes apart, and the
            // session id is the one the **server** writes.
            let visitor = uuid_from_text(&format!("visitor-{:08x}", session % 40_000));
            let (ip, ip_prefix, _) = visitor_identity(&visitor);
            // The user agent has a **low cardinality** in the real world: a
            // handful of browsers, not one per visitor. The address, on the
            // other hand, is nearly one per visitor.
            let user_agent = AGENTS[index % AGENTS.len()].to_string();
            let started = base + chrono::Duration::seconds((session * 97) % (30 * 86_400));
            let at = started + chrono::Duration::seconds((index % 3) as i64 * 90);
            // The session id is the one the **server** writes: it is a hash of
            // the visitor and the moment the session started.
            let visitor_id =
                crate::ua::visitor_id(&crate::ua::visitor_address(&ip), &user_agent, "w1");
            let visit_id =
                crate::session::session_id(visitor_id, started.and_utc().timestamp_micros());

            HitBuilder::new("w1", &format!("session-{session:08x}"), "v")
                .visit_id(visit_id)
                .visitor_id(visitor_id)
                .ip(&ip)
                .ip_prefix(&ip_prefix)
                .user_agent(&user_agent)
                .session_start(started)
                .at(at)
            .url(PAGES[index % PAGES.len()])
            .url_query("utm_source=twitter&utm_medium=cpc")
            .title(format!("A post about topic {}", number % 500))
            .referrer(if referrer.is_empty() {
                None
            } else {
                Some(referrer)
            })
            .channel(channel)
            .referrer_path("/item?id=1")
            .referrer_query("?id=1")
            .referrer_name(crate::channels::referrer_name(Some(referrer)).unwrap_or_default())
            .hostname("example.com")
            .browser(BROWSERS[index % BROWSERS.len()])
            .browser_version("120")
            .os(SYSTEMS[index % SYSTEMS.len()])
            .os_version("11")
            .device(if number % 3 == 0 { "mobile" } else { "desktop" })
            .screen("1920x1080")
            .language("en-US")
            .country(COUNTRIES[index % COUNTRIES.len()])
            .region("California")
            .city("San Francisco")
            .utm_source("twitter")
            .utm_medium("cpc")
            .utm_campaign("spring")
            .utm_content("cta")
            .utm_term("analytics")
            .lcp(600.0 + (number % 2000) as f64)
            .inp(20.0 + (number % 300) as f64)
            .cls((number % 100) as f64 / 1000.0)
            .fcp(400.0 + (number % 900) as f64)
            .ttfb(60.0 + (number % 220) as f64)
            .build()
        })
        .collect()
}
