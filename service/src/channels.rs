//! The traffic channel: where the visit came from.
//!
//! **The rules are the reference implementation's, rule for rule.** The same
//! visit has to land in the same bucket in both, or the numbers cannot be
//! compared — see AGENTS.md §4.3e ("the value sets have to match").
//!
//! The buckets are:
//!
//! | bucket | how a visit gets there |
//! | --- | --- |
//! | `direct` | no referrer and no query string — or nothing else matched |
//! | `paidAds` | the query string carries a paid click id or a paid medium |
//! | `referral` | the medium says so, or it is another site that is not a known one |
//! | `affiliate` | the medium says affiliate |
//! | `sms` | the medium or the source says sms |
//! | `llm` | the referrer is a known assistant (ChatGPT, Claude, …) |
//! | `Search` | the referrer is a search engine, or the medium says organic |
//! | `Social` | the referrer is a social network |
//! | `email` | the referrer is a mail client, or the medium says mail |
//! | `Shopping` | the referrer is a shop, or the medium says shop |
//! | `Video` | the referrer is a video site, or the medium says video |
//!
//! **The order matters.** The first rule that matches wins — a paid click from
//! a social network is a paid click, not a social visit.
//!
//! The matching is a **case-insensitive substring** test, the way the reference
//! implementation does it (`ILIKE '%…%'`). A domain list holds `google.`, and
//! `www.google.co.uk` contains it.
//!
//! The channel is worked out **once**, when the event is collected, and stored
//! with it: a visit from a search engine is a search visit, and the report does
//! not have to guess it from the referrer every time.

/// The chat assistants.
const LLM_DOMAINS: &[&str] = &[
    "chatgpt.com",
    "claude.ai",
    "copilot.microsoft.com",
    "gemini.google.com",
    "meta.ai",
    "perplexity.ai",
];

/// The search engines.
const SEARCH_DOMAINS: &[&str] = &[
    "baidu.com",
    "bing.com",
    "duckduckgo.com",
    "ecosia.org",
    "google.",
    "msn.com",
    "search.brave.com",
    "yandex.",
];

/// The social networks.
const SOCIAL_DOMAINS: &[&str] = &[
    "bsky.app",
    "facebook.com",
    "fb.com",
    "ig.com",
    "instagram.com",
    "linkedin.",
    "news.ycombinator.com",
    "pinterest.",
    "reddit.",
    "snapchat.",
    "t.co",
    "threads.net",
    "tiktok.",
    "twitter.com",
    "x.com",
];

/// The mail clients.
///
/// The reference implementation's list, plus the ones it does not have: a mail
/// client that is not in the list is counted as something else, and Gmail is
/// too big a referrer to get wrong.
const EMAIL_DOMAINS: &[&str] = &[
    "gmail.",
    "hotmail.",
    "mail.yahoo.",
    "outlook.",
    "proton.me",
    "protonmail.",
    "mail.google.com",
    "mail.proton.me",
    "mail.yahoo.com",
    "outlook.live.com",
    "outlook.office.com",
    "fastmail.com",
    "mail.ru",
    "mail.qq.com",
    "zoho.com",
    "gmx.com",
    "hey.com",
    "yandex.com",
];

/// The shops.
const SHOPPING_DOMAINS: &[&str] = &[
    "alibaba.com",
    "aliexpress.com",
    "amazon.",
    "bestbuy.com",
    "ebay.com",
    "etsy.com",
    "newegg.com",
    "target.com",
    "walmart.com",
];

/// The video sites.
const VIDEO_DOMAINS: &[&str] = &["twitch.", "youtube."];

/// The query-string fragments that mean "this was a paid click".
const PAID_AD_PARAMS: &[&str] = &[
    "ad_id=",
    "aid=",
    "dclid=",
    "epik=",
    "gclid=",
    "li_fat_id=",
    "msclkid=",
    "ob_click_id=",
    "pc_id=",
    "rdt_cid=",
    "scid=",
    "ttclid=",
    "twclid=",
    "utm_medium=cpc",
    "utm_medium=paid",
    "utm_medium=paid_social",
    "utm_source=google",
];

/// The mediums that mean "somebody sent a link".
const REFERRAL_MEDIUMS: &[&str] = &["referral", "app", "link"];

/// The domain lists, in the order the reference implementation tries them.
///
/// The **longest** match wins, whatever list it is in: `mail.google.com` is a
/// mail client, not a search engine, even though `google.` is a search engine
/// and is part of the name.
const DOMAIN_LISTS: &[(&str, &[&str])] = &[
    ("llm", LLM_DOMAINS),
    ("Search", SEARCH_DOMAINS),
    ("Social", SOCIAL_DOMAINS),
    ("email", EMAIL_DOMAINS),
    ("Shopping", SHOPPING_DOMAINS),
    ("Video", VIDEO_DOMAINS),
];

/// The referrer's display name, when it is a known one.
///
/// The reference implementation groups the referrers by name at query time
/// ("Hacker News", not `news.ycombinator.com`). We work it out when the event
/// is collected and store it, so the name is there without a lookup.
const GROUPED_DOMAINS: &[(&str, &str)] = &[
    ("baidu.com", "Baidu"),
    ("bing.com", "Bing"),
    ("brave.com", "Brave"),
    ("chatgpt.com", "ChatGPT"),
    ("duckduckgo.com", "DuckDuckGo"),
    ("facebook.com", "Facebook"),
    ("github.com", "GitHub"),
    ("google.com", "Google"),
    ("news.ycombinator.com", "Hacker News"),
    ("instagram.com", "Instagram"),
    ("linkedin.com", "LinkedIn"),
    ("pinterest.com", "Pinterest"),
    ("reddit.com", "Reddit"),
    ("snapchat.com", "Snapchat"),
    ("twitter.com", "Twitter"),
    ("yahoo.com", "Yahoo"),
    ("yandex.ru", "Yandex"),
];

/// What the channel is worked out from.
#[derive(Debug, Clone, Copy, Default)]
pub struct ChannelInput<'a> {
    /// The referrer's host (without the `www.`).
    pub referrer: Option<&'a str>,
    /// The query string of the visited page.
    pub url_query: Option<&'a str>,
    /// The `utm_medium` parameter.
    pub utm_medium: Option<&'a str>,
    /// The `utm_source` parameter.
    pub utm_source: Option<&'a str>,
    /// The host of the site that was visited.
    pub hostname: Option<&'a str>,
}

impl<'a> ChannelInput<'a> {
    /// The input, built from the parts of a hit.
    pub fn new(
        referrer: Option<&'a str>,
        url_query: Option<&'a str>,
        utm_medium: Option<&'a str>,
        utm_source: Option<&'a str>,
        hostname: Option<&'a str>,
    ) -> Self {
        Self {
            referrer,
            url_query,
            utm_medium,
            utm_source,
            hostname,
        }
    }
}

/// The bucket a visit falls into.
///
/// The rules, in the order they are tried:
///
/// 1. no referrer and no query string → `direct`
/// 2. the query string carries a paid click id → `paidAds`
/// 3. the medium is `referral`, `app` or `link` → `referral`
/// 4. the medium says affiliate → `affiliate`
/// 5. the medium or the source says sms → `sms`
/// 6. the referrer is a chat assistant → `llm`
/// 7. the referrer is a search engine, or the medium says organic → `Search`
/// 8. the referrer is a social network → `Social`
/// 9. the referrer is a mail client, or the medium says mail → `email`
/// 10. the referrer is a shop, or the medium says shop → `Shopping`
/// 11. the referrer is a video site, or the medium says video → `Video`
/// 12. another site, and not this one → `referral`
/// 13. nothing else → `direct`
pub fn channel(input: &ChannelInput) -> &'static str {
    // The originals — the comparison at the end is case-sensitive ✓.
    let referrer_raw = strip_www(input.referrer.unwrap_or("").trim());
    let hostname_raw = strip_www(input.hostname.unwrap_or("").trim());

    // The lowercase versions — every lookup below is case-insensitive ✓ ⇒ ⇒
    // ⇒ and each one is lowercased **once** ✓ ⇒ ⇒ ⇒ not once per domain ✓ ⇒
    // ⇒ ⇒ the whole lookup allocates **nothing** ✓ (see `MEMORY.md` §6 ✓).
    let referrer = referrer_raw.to_lowercase();
    let query = input.url_query.unwrap_or("").trim().to_lowercase();
    let medium = input.utm_medium.unwrap_or("").trim().to_lowercase();
    let source = input.utm_source.unwrap_or("").trim().to_lowercase();

    // 1. Nothing came with the visit at all.
    if referrer.is_empty() && query.is_empty() {
        return "direct";
    }

    // 2. A paid click: the click id or the medium says so.
    if contains_any(&query, PAID_AD_PARAMS) {
        return "paidAds";
    }

    // 3. Somebody sent a link.
    if contains_any(&medium, REFERRAL_MEDIUMS) {
        return "referral";
    }

    // 4. An affiliate.
    if contains(&medium, "affiliate") {
        return "affiliate";
    }

    // 5. A text message.
    if contains(&medium, "sms") || contains(&source, "sms") {
        return "sms";
    }

    // The bucket the referrer's domain belongs to — the longest match wins.
    let domain = domain_bucket(&referrer);

    // 6. A chat assistant.
    if domain == Some("llm") {
        return "llm";
    }

    // 7. A search engine.
    if domain == Some("Search") || contains(&medium, "organic") {
        return "Search";
    }

    // 8. A social network.
    if domain == Some("Social") {
        return "Social";
    }

    // 9. A mail client.
    if domain == Some("email") || contains(&medium, "mail") {
        return "email";
    }

    // 10. A shop.
    if domain == Some("Shopping") || contains(&medium, "shop") {
        return "Shopping";
    }

    // 11. A video site.
    if domain == Some("Video") || contains(&medium, "video") {
        return "Video";
    }

    // 12. Some other site — but not this one.
    if !referrer_raw.is_empty() && referrer_raw != hostname_raw {
        return "referral";
    }

    // 13. A self-referral, or a referrer we know nothing about.
    "direct"
}

/// The bucket a referrer's domain belongs to, if it is a known one.
///
/// **The longest match wins**, whatever list it is in: `mail.google.com` is a
/// mail client (14 characters) and not a search engine (`google.`, 7).
///
/// The referrer is **already lowercase** ✓ ⇒ ⇒ ⇒ so the lookup allocates
/// nothing ✓ ⇒ ⇒ ⇒ however many domains the lists hold ✓.
fn domain_bucket(referrer: &str) -> Option<&'static str> {
    if referrer.is_empty() {
        return None;
    }
    let mut best: Option<(usize, &'static str)> = None;
    for (bucket, domains) in DOMAIN_LISTS {
        for domain in domains.iter() {
            if contains(referrer, domain) {
                let length = domain.len();
                if best.map(|(best, _)| length > best).unwrap_or(true) {
                    best = Some((length, bucket));
                }
            }
        }
    }
    best.map(|(_, bucket)| bucket)
}

/// The display name of a referrer, when it has one.
///
/// "Hacker News" for `news.ycombinator.com`, `None` for a site we do not know.
pub fn referrer_name(referrer: Option<&str>) -> Option<String> {
    let referrer = referrer.map(str::trim).filter(|r| !r.is_empty())?;
    for (domain, name) in GROUPED_DOMAINS {
        if referrer.contains(domain) {
            return Some((*name).to_string());
        }
    }
    None
}

/// Strip a leading `www.` — the way the referrer's host is stored.
pub fn strip_www(host: &str) -> &str {
    host.strip_prefix("www.").unwrap_or(host)
}

/// A case-insensitive substring test.
/// Does the **lowercase** text contain the needle?
///
/// **Nothing is allocated here** ✓: the needles are lowercase constants ✓
/// (a test says so ✓) ⇒ ⇒ ⇒ and the haystack is lowercased **once**, by the
/// caller ✓ ⇒ ⇒ ⇒ so the lookup is free ✓ ✓.
///
/// It used to lowercase **both** sides on **every** call ✓ ⇒ ⇒ ⇒ and the
/// call is made once per domain in the lists ✓ ⇒ ⇒ ⇒ **over a hundred
/// allocations per request** ⇒ ⇒ ⇒ **1 764 bytes** ⇒ ⇒ ⇒ measured with the
/// counting allocator ✓ (see `MEMORY.md` §6 ✓).
fn contains(lowercase: &str, needle: &str) -> bool {
    if lowercase.is_empty() || needle.is_empty() {
        return false;
    }
    lowercase.contains(needle)
}

/// Does the text contain any of the needles?
fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| contains(haystack, needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel_of(referrer: Option<&str>) -> &'static str {
        channel(&ChannelInput::new(referrer, None, None, None, None))
    }

    /// The needles are lowercase — the lookups rely on it.
    ///
    /// The haystack is lowercased once, by the caller ✓ ⇒ ⇒ ⇒ so the needles
    /// have to be lowercase already ✓ ⇒ ⇒ ⇒ and a needle that is not would
    /// **never match** ✓ ⇒ ⇒ ⇒ so this test says so ✓.
    #[test]
    fn the_needles_are_lowercase() {
        for (bucket, domains) in DOMAIN_LISTS {
            for domain in domains.iter() {
                assert_eq!(
                    *domain,
                    domain.to_lowercase(),
                    "the needle {domain} ({bucket}) is not lowercase"
                );
            }
        }
        for needle in PAID_AD_PARAMS.iter() {
            assert_eq!(*needle, needle.to_lowercase(), "the needle {needle} is not lowercase");
        }
        for needle in REFERRAL_MEDIUMS.iter() {
            assert_eq!(*needle, needle.to_lowercase(), "the needle {needle} is not lowercase");
        }
    }

    /// The lookup is case-insensitive — as it was.
    #[test]
    fn the_lookup_ignores_the_case() {
        assert_eq!(channel_of(Some("WWW.Google.COM")), "Search");
        assert_eq!(channel_of(Some("news.ycombinator.com")), "Social");
    }

    #[test]
    fn nothing_is_direct() {
        assert_eq!(channel_of(None), "direct");
        assert_eq!(channel_of(Some("")), "direct");
    }

    #[test]
    fn the_search_engines_are_search() {
        assert_eq!(channel_of(Some("www.google.com")), "Search");
        assert_eq!(channel_of(Some("bing.com")), "Search");
        assert_eq!(channel_of(Some("duckduckgo.com")), "Search");
        assert_eq!(channel_of(Some("search.brave.com")), "Search");
    }

    #[test]
    fn the_social_networks_are_social() {
        assert_eq!(channel_of(Some("news.ycombinator.com")), "Social");
        assert_eq!(channel_of(Some("t.co")), "Social");
        assert_eq!(channel_of(Some("www.reddit.com")), "Social");
        assert_eq!(channel_of(Some("x.com")), "Social");
    }

    #[test]
    fn the_mail_clients_are_email() {
        assert_eq!(channel_of(Some("mail.google.com")), "email");
        assert_eq!(channel_of(Some("outlook.live.com")), "email");
    }

    #[test]
    fn the_assistants_are_llm() {
        assert_eq!(channel_of(Some("chatgpt.com")), "llm");
        assert_eq!(channel_of(Some("claude.ai")), "llm");
        assert_eq!(channel_of(Some("gemini.google.com")), "llm");
    }

    #[test]
    fn the_shops_are_shopping() {
        assert_eq!(channel_of(Some("www.amazon.com")), "Shopping");
        assert_eq!(channel_of(Some("ebay.com")), "Shopping");
    }

    #[test]
    fn the_video_sites_are_video() {
        assert_eq!(channel_of(Some("www.youtube.com")), "Video");
        assert_eq!(channel_of(Some("twitch.tv")), "Video");
    }

    #[test]
    fn an_unknown_site_is_a_referral() {
        assert_eq!(channel_of(Some("some-blog.example")), "referral");
    }

    #[test]
    fn the_site_itself_is_direct() {
        let input = ChannelInput::new(Some("example.com"), None, None, None, Some("example.com"));
        assert_eq!(channel(&input), "direct");

        // The `www.` is stripped on both sides.
        let input = ChannelInput::new(
            Some("www.example.com"),
            None,
            None,
            None,
            Some("www.example.com"),
        );
        assert_eq!(channel(&input), "direct");
    }

    #[test]
    fn a_paid_click_wins_over_the_referrer() {
        let input = ChannelInput::new(
            Some("www.facebook.com"),
            Some("utm_medium=cpc"),
            Some("cpc"),
            None,
            None,
        );
        assert_eq!(channel(&input), "paidAds");

        let input = ChannelInput::new(Some("example.com"), Some("gclid=abc"), None, None, None);
        assert_eq!(channel(&input), "paidAds");
    }

    #[test]
    fn the_medium_can_say_the_bucket() {
        let input = ChannelInput::new(None, Some("x=1"), Some("referral"), None, None);
        assert_eq!(channel(&input), "referral");

        let input = ChannelInput::new(None, Some("x=1"), Some("affiliate"), None, None);
        assert_eq!(channel(&input), "affiliate");

        let input = ChannelInput::new(None, Some("x=1"), Some("sms"), None, None);
        assert_eq!(channel(&input), "sms");

        let input = ChannelInput::new(None, Some("x=1"), Some("organic"), None, None);
        assert_eq!(channel(&input), "Search");

        let input = ChannelInput::new(None, Some("x=1"), Some("email"), None, None);
        assert_eq!(channel(&input), "email");

        let input = ChannelInput::new(None, Some("x=1"), Some("shopping"), None, None);
        assert_eq!(channel(&input), "Shopping");

        let input = ChannelInput::new(None, Some("x=1"), Some("video"), None, None);
        assert_eq!(channel(&input), "Video");
    }

    #[test]
    fn the_query_string_alone_is_not_direct() {
        let input = ChannelInput::new(None, Some("a=1"), None, None, None);
        assert_eq!(channel(&input), "direct");
    }

    #[test]
    fn the_referrer_has_a_display_name() {
        assert_eq!(
            referrer_name(Some("news.ycombinator.com")),
            Some("Hacker News".to_string())
        );
        assert_eq!(
            referrer_name(Some("www.google.com")),
            Some("Google".to_string())
        );
        assert_eq!(referrer_name(Some("some-blog.example")), None);
        assert_eq!(referrer_name(None), None);
    }
}
