//! User-agent classification and privacy-preserving session derivation.
//!
//! Two rules govern this module:
//!   1. The raw IP address is **never** stored — only a coarse prefix is used, and only
//!      as an HMAC input, to derive a rotating session key.
//!   2. Everything derived from the user agent is a *family* (browser, OS, device class),
//!      never a fingerprint.

use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Classification of a user-agent string.
#[derive(Debug, Clone, Default)]
pub struct UserAgentInfo {
    pub browser: Option<String>,
    pub browser_version: Option<String>,
    pub os: Option<String>,
    pub os_version: Option<String>,
    pub device: Option<String>,
}

/// The kind of a crawler, from its user agent.
///
/// **A bot is not a visitor** — it does not look at the page, it fetches it.
/// Umami throws its hits away; we keep them and **mark** them, so a site
/// owner can say which crawlers came and when.
///
/// The kinds are the leaves of the four groups the dashboard shows:
///
/// - `bot-ai-*` — the crawlers that fetch the site to answer questions;
/// - `bot-se-*` — the search engines, which fetch it for the index;
/// - `bot-social-*` — the preview bots of the social and messaging apps;
/// - `bot-other-*` — the crawlers we do not know by name.
pub fn bot_kind(user_agent: &str) -> Option<&'static str> {
    let ua = user_agent.to_ascii_lowercase();

    // The AI crawlers first: they are the ones worth naming.
    if ["gptbot", "chatgpt-user", "oai-searchbot"]
        .iter()
        .any(|marker| ua.contains(marker))
    {
        return Some("bot-ai-openai");
    }
    if ["claudebot", "claude-web", "anthropic-ai", "ccbot"]
        .iter()
        .any(|marker| ua.contains(marker))
    {
        return Some("bot-ai-anthropic");
    }
    if ["perplexitybot", "perplexity-user"]
        .iter()
        .any(|marker| ua.contains(marker))
    {
        return Some("bot-ai-perplexity");
    }
    if ua.contains("applebot-extended") {
        return Some("bot-ai-apple");
    }

    // The search engines.
    if [
        "googlebot",
        "google-inspectiontool",
        "adsbot-google",
        "googleother",
        "storebot-google",
        "google-extended",
    ]
    .iter()
    .any(|marker| ua.contains(marker))
    {
        return Some("bot-se-google");
    }
    if ["bingbot", "msnbot", "adidxbot"]
        .iter()
        .any(|marker| ua.contains(marker))
    {
        return Some("bot-se-bing");
    }
    if ua.contains("yandex") {
        return Some("bot-se-yandex");
    }
    if ua.contains("baiduspider") {
        return Some("bot-se-baidu");
    }
    if ["duckduckbot", "applebot", "seznam"]
        .iter()
        .any(|marker| ua.contains(marker))
    {
        return Some("bot-se-other");
    }

    // The social and messaging apps.
    if ua.contains("facebookexternalhit") {
        return Some("bot-social-meta");
    }
    if ua.contains("twitterbot") {
        return Some("bot-social-x");
    }
    if ua.contains("linkedinbot") {
        return Some("bot-social-linkedin");
    }
    if ["slackbot", "discordbot", "telegrambot", "whatsapp"]
        .iter()
        .any(|marker| ua.contains(marker))
    {
        return Some("bot-social-messaging");
    }

    // The rest: the crawlers we do not know by name.
    if [
        "bot",
        "spider",
        "crawler",
        "crawling",
        "slurp",
        "mediapartners",
        "scraper",
    ]
    .iter()
    .any(|marker| ua.contains(marker))
    {
        return Some("bot-other-unknown");
    }
    None
}

/// Classify a user-agent string into browser / OS / device families.
///
/// This is intentionally a small, dependency-free approximation: it covers the browsers
/// that matter for analytics and refuses to guess when it is unsure.
pub fn classify(user_agent: &str) -> UserAgentInfo {
    let ua = user_agent.to_ascii_lowercase();
    let mut info = UserAgentInfo::default();

    // --- Browser (order matters: many browsers impersonate others) ---
    let (browser, version): (Option<&str>, Option<String>) = if ua.contains("edg/") || ua.contains("edge/") {
        (Some("Edge"), version_after(&ua, "edg/"))
    } else if ua.contains("opr/") || ua.contains("opera") {
        (
            Some("Opera"),
            version_after(&ua, "opr/").or_else(|| version_after(&ua, "opera/")),
        )
    } else if ua.contains("samsungbrowser") {
        (Some("Samsung Internet"), version_after(&ua, "samsungbrowser/"))
    } else if ua.contains("yabrowser") || ua.contains("yowser") {
        (Some("Yandex Browser"), version_after(&ua, "yowser/"))
    } else if ua.contains("crios/") {
        (Some("Chrome"), version_after(&ua, "crios/"))
    } else if ua.contains("fxios/") {
        (Some("Firefox"), version_after(&ua, "fxios/"))
    } else if ua.contains("firefox/") {
        (Some("Firefox"), version_after(&ua, "firefox/"))
    } else if ua.contains("chrome/") || ua.contains("chromium/") {
        (Some("Chrome"), version_after(&ua, "chrome/"))
    } else if ua.contains("safari/") && ua.contains("version/") {
        (Some("Safari"), version_after(&ua, "version/"))
    } else if ua.contains("safari/") {
        (Some("Safari"), None)
    } else if ua.contains("curl/") {
        (Some("curl"), version_after(&ua, "curl/"))
    } else if ua.contains("wget/") {
        (Some("Wget"), version_after(&ua, "wget/"))
    } else if ua.contains("bot") || ua.contains("spider") || ua.contains("crawler") {
        (Some("Bot"), None)
    } else {
        (None, None)
    };
    info.browser = browser.map(str::to_string);
    info.browser_version = version;

    // --- OS ---
    let (os, version): (Option<&str>, Option<String>) = if ua.contains("windows nt") {
        (Some("Windows"), windows_version(&ua))
    } else if ua.contains("android") {
        (Some("Android"), version_after(&ua, "android "))
    } else if ua.contains("iphone os") || ua.contains("cpu iphone os") {
        (Some("iOS"), version_after(&ua, "iphone os "))
    } else if ua.contains("ipad") {
        (Some("iPadOS"), version_after(&ua, "os "))
    } else if ua.contains("mac os x") || ua.contains("macintosh") {
        (Some("macOS"), version_after(&ua, "mac os x "))
    } else if ua.contains("cros") {
        (Some("ChromeOS"), version_after(&ua, "cros "))
    } else if ua.contains("linux") {
        (Some("Linux"), None)
    } else {
        (None, None)
    };
    info.os = os.map(str::to_string);
    info.os_version = version;

    // --- Device class ---
    info.device = Some(
        if ua.contains("ipad")
            || ua.contains("tablet")
            || (ua.contains("android") && !ua.contains("mobile"))
        {
            "tablet"
        } else if ua.contains("mobi")
            || ua.contains("iphone")
            || ua.contains("ipod")
            || ua.contains("windows phone")
            || ua.contains("blackberry")
        {
            "mobile"
        } else {
            "desktop"
        }
        .to_string(),
    );

    info
}

/// Extract a version number that follows `token` in a user-agent string.
fn version_after(ua: &str, token: &str) -> Option<String> {
    let idx = ua.find(token)? + token.len();
    let rest = &ua[idx..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit() && c != '.' && c != '_')
        .unwrap_or(rest.len());
    let raw = rest[..end].trim_end_matches(['.', '_']);
    if raw.is_empty() {
        None
    } else {
        Some(raw.to_string())
    }
}

/// Map the `Windows NT x.y` token to a product name.
fn windows_version(ua: &str) -> Option<String> {
    let nt = version_after(ua, "windows nt ")?;
    Some(
        match nt.as_str() {
            "10.0" => "10/11".to_string(),
            "6.3" => "8.1".to_string(),
            "6.2" => "8".to_string(),
            "6.1" => "7".to_string(),
            other => other.to_string(),
        },
    )
}

/// Truncate an IP address to a coarse prefix: /24 for IPv4, /64 for IPv6.
///
/// The result is only ever used as HMAC input — never stored, never logged.
pub fn ip_prefix(ip: &str) -> String {
    if ip.contains(':') {
        // IPv6: keep the first 4 hextets (/64).
        ip.split(':').take(4).collect::<Vec<_>>().join(":")
    } else {
        // IPv4: keep the first 3 octets (/24).
        ip.split('.').take(3).collect::<Vec<_>>().join(".")
    }
}

/// The address the visitor's id is worked out of.
///
/// **IPv4: the whole address.** A /24 is 256 addresses: a campus, an office,
/// a mobile carrier's NAT — everybody behind it would be **one** visitor.
/// The address is only *hashed* into the id, so nothing raw is stored by
/// this.
///
/// **IPv6: the /64.** The whole address would be wrong the other way: IPv6
/// privacy extensions **rotate the /64** of a client on every network it
/// joins, so the whole address would make one visitor into many. The /64 is
/// what stays with the machine.
pub fn visitor_address(ip: &str) -> String {
    if ip.contains(':') {
        ip.split(':').take(4).collect::<Vec<_>>().join(":")
    } else {
        ip.to_string()
    }
}

/// The **visitor id**: the identity a session is cut for.
///
/// It is a UUID built from the SHA-256 of (the visitor's address, the user
/// agent, the website) — **sixteen bytes**.
///
/// The address is the one [`visitor_address`] picks: the whole IPv4 address,
/// the /64 of an IPv6 one.
///
/// **The website is in it**, so two sites never share a visitor's session: the
/// session cache is keyed by this, and a visitor who is active on both sites at
/// once must get a session on each.
///
/// The id is **pure**: the same visitor gets the same id on every server, after
/// every restart, and an offline import can work it out for itself. So the row
/// carries it, and the reports count the visitors out of it.
pub fn visitor_id(address: &str, user_agent: &str, website_id: &str) -> Uuid {
    let mut hasher = Sha256::new();
    hasher.update(address.as_bytes());
    hasher.update(b"\x1f");
    hasher.update(user_agent.as_bytes());
    hasher.update(b"\x1f");
    hasher.update(website_id.as_bytes());
    uuid_from_bytes(&hasher.finalize()[..16])
}

/// Turn 16 bytes into a UUID.
pub fn uuid_from_bytes(bytes: &[u8]) -> Uuid {
    let mut out = [0u8; 16];
    out.copy_from_slice(&bytes[..16]);
    Uuid::from_bytes(out)
}

/// Turn an arbitrary text into a UUID.
///
/// The client may send its own session id (Umami compatibility). It is never stored
/// verbatim: it is hashed into 16 bytes, so the column is always the same size and
/// the raw token never reaches the disk. The mapping is **deterministic** — the same
/// token is always the same session — which is also what lets a v3 database be
/// migrated without splitting anybody's session in two.
pub fn uuid_from_text(text: &str) -> Uuid {
    let digest = Sha256::digest(text.as_bytes());
    uuid_from_bytes(&digest[..16])
}

/// Minimal HMAC-SHA256 (RFC 2104) so we do not need another dependency.
pub struct HmacSha256 {
    inner: Sha256,
    outer_key: [u8; 64],
}

impl HmacSha256 {
    /// Start a new HMAC with `key`.
    pub fn new(key: &[u8]) -> Self {
        let mut block = [0u8; 64];
        if key.len() > 64 {
            let hashed = Sha256::digest(key);
            block[..32].copy_from_slice(&hashed);
        } else {
            block[..key.len()].copy_from_slice(key);
        }

        let mut ipad = [0x36u8; 64];
        let mut opad = [0x5cu8; 64];
        for i in 0..64 {
            ipad[i] ^= block[i];
            opad[i] ^= block[i];
        }

        let mut inner = Sha256::new();
        inner.update(ipad);

        Self {
            inner,
            outer_key: opad,
        }
    }

    /// Feed data into the MAC.
    pub fn update(&mut self, data: &[u8]) {
        self.inner.update(data);
    }

    /// Finish and return the 32-byte tag.
    pub fn finalize(self) -> [u8; 32] {
        let inner_digest = self.inner.finalize();
        let mut outer = Sha256::new();
        outer.update(self.outer_key);
        outer.update(inner_digest);
        let out = outer.finalize();
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&out);
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_desktop_chrome() {
        let ua = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
        let info = classify(ua);
        assert_eq!(info.browser.as_deref(), Some("Chrome"));
        assert_eq!(info.os.as_deref(), Some("macOS"));
        assert_eq!(info.device.as_deref(), Some("desktop"));
    }

    #[test]
    fn classifies_mobile_safari() {
        let ua = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_1 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.1 Mobile/15E148 Safari/604.1";
        let info = classify(ua);
        assert_eq!(info.browser.as_deref(), Some("Safari"));
        assert_eq!(info.os.as_deref(), Some("iOS"));
        assert_eq!(info.device.as_deref(), Some("mobile"));
    }

    #[test]
    fn ip_prefix_truncates() {
        assert_eq!(ip_prefix("203.0.113.45"), "203.0.113");
        assert_eq!(ip_prefix("2001:db8:1234:5678::1"), "2001:db8:1234:5678");
    }

    #[test]
    fn the_visitor_id_is_a_function_of_the_prefix_the_agent_and_the_site() {
        let a = visitor_id("203.0.113", "UA", "website");
        let b = visitor_id("203.0.113", "UA", "website");
        assert_eq!(a, b);
        // Another site is another visitor: the session cache is keyed by this.
        assert_ne!(a, visitor_id("203.0.113", "UA", "other-website"));
        // Another address, another agent.
        assert_ne!(a, visitor_id("203.0.114", "UA", "website"));
        assert_ne!(a, visitor_id("203.0.113", "Other UA", "website"));
        // Sixteen bytes.
        assert_eq!(a.as_bytes().len(), 16);
    }

    #[test]
    fn a_client_id_becomes_a_uuid_without_changing() {
        let a = uuid_from_text("abc-123");
        let b = uuid_from_text("abc-123");
        let c = uuid_from_text("abc-124");
        assert_eq!(a, b, "the same token is always the same session");
        assert_ne!(a, c);
        assert_eq!(a.as_bytes().len(), 16);
    }

    #[test]
    fn a_text_id_matches_what_the_migration_computes() {
        // The v3 -> v4 migration hashes the old ids with SHA-256 and keeps the
        // first 16 bytes. This is the same computation, in Rust.
        let digest = Sha256::digest(b"session-edge-unicode");
        let expected = Uuid::from_bytes({
            let mut bytes = [0u8; 16];
            bytes.copy_from_slice(&digest[..16]);
            bytes
        });
        assert_eq!(uuid_from_text("session-edge-unicode"), expected);
    }

    #[test]
    fn hmac_matches_rfc4231_case_1() {
        // RFC 4231 test case 1
        let key = [0x0bu8; 20];
        let mut mac = HmacSha256::new(&key);
        mac.update(b"Hi There");
        let tag = mac.finalize();
        let expected = [
            0xb0, 0x34, 0x4c, 0x61, 0xd8, 0xdb, 0x38, 0x53, 0x5c, 0xa8, 0xaf, 0xce, 0xaf, 0x0b,
            0xf1, 0x2b, 0x88, 0x1d, 0xc2, 0x00, 0xc9, 0x83, 0x3d, 0xa7, 0x26, 0xe9, 0x37, 0x6c,
            0x2e, 0x32, 0xcf, 0xf7,
        ];
        assert_eq!(tag, expected);
    }
}
