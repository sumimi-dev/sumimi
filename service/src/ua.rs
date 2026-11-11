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

/// Derive a rotating, non-reversible visit id.
///
/// The id is a HMAC over (server secret, coarse IP prefix, user agent, website, time
/// bucket). It cannot be reversed into an IP address, is not stable across time buckets
/// (so it is not a durable identifier), and is scoped to one website.
///
/// The result is a **UUID**: 16 bytes, not a 36-character string. At 60 million page
/// views that is the difference between 1.1 GB and 2.2 GB of session ids alone.
pub fn derive_visit_id(
    secret: &[u8],
    ip_prefix: &str,
    user_agent: &str,
    website_id: &str,
    bucket: u64,
) -> Uuid {
    let mut mac = HmacSha256::new(secret);
    mac.update(ip_prefix.as_bytes());
    mac.update(b"\x1f");
    mac.update(user_agent.as_bytes());
    mac.update(b"\x1f");
    mac.update(website_id.as_bytes());
    mac.update(b"\x1f");
    mac.update(&bucket.to_le_bytes());
    let digest = mac.finalize();
    uuid_from_bytes(&digest[..16])
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
    fn visit_id_is_stable_within_a_bucket_and_rotates() {
        let args = (b"secret", "203.0.113", "UA", "website", 42);
        let a = derive_visit_id(args.0, args.1, args.2, args.3, args.4);
        let b = derive_visit_id(args.0, args.1, args.2, args.3, args.4);
        let c = derive_visit_id(args.0, args.1, args.2, args.3, 43);
        assert_eq!(a, b);
        assert_ne!(a, c);
        // 16 bytes, not a 36-character string.
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
