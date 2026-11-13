//! The token the client carries.
//!
//! The flow is Umami's: the server signs a small token, hands it to the client
//! in the response, and the client sends it back with the next hit. **The
//! token is how the server knows which session a hit belongs to** — so the
//! server does not have to remember the sessions itself.
//!
//! Why a token and not a row:
//!
//! - the client is told something anyway (the response carries it), so the
//!   state costs nothing extra on the wire;
//! - only the server can read it, so a client cannot invent a session, and
//!   cannot move its hits into somebody else's;
//! - it is **signed, not encrypted** — the client can read what is in it, it
//!   just cannot change it. The same as Umami's.
//!
//! What is in it:
//!
//! - the website it belongs to — a token of one site is never used on another;
//! - the visitor's id — so a token that belongs to somebody else is not used;
//! - the session's id — the session the hits belong to;
//! - the moment of the last hit — so the idle timeout can be applied without
//!   the server remembering anything.

use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::session::{format_visit_id, parse_visit_id, VisitId};

/// The type of the token.
///
/// The login tokens and these are signed with the same secret, so the type is
/// what tells them apart: a login token is not a cache token, and the claims
/// of one do not even fit the other.
const CACHE_TYPE: &str = "cache";

/// The claims of the token the client carries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    /// The website the token belongs to.
    pub website_id: String,
    /// The visitor's id.
    pub visitor_id: String,
    /// The session's id.
    pub visit_id: String,
    /// The moment the session started, in seconds since the epoch.
    pub start: i64,
    /// The moment of the last hit, in seconds since the epoch.
    pub last: i64,
    /// The type of the token.
    #[serde(rename = "type")]
    pub token_type: String,
}

impl Claims {
    /// The claims of a session.
    pub fn new(
        website_id: &str,
        visitor_id: Uuid,
        visit_id: VisitId,
        start: i64,
        last: i64,
    ) -> Self {
        Self {
            website_id: website_id.to_string(),
            visitor_id: visitor_id.to_string(),
            visit_id: format_visit_id(visit_id),
            start,
            last,
            token_type: CACHE_TYPE.to_string(),
        }
    }

    /// The session's id, back from its text.
    pub fn session(&self) -> Option<VisitId> {
        parse_visit_id(&self.visit_id)
    }

    /// The visitor's id, back from its text.
    pub fn visitor(&self) -> Option<Uuid> {
        Uuid::parse_str(&self.visitor_id).ok()
    }

    /// The moment the session started.
    pub fn started_at(&self) -> Option<chrono::NaiveDateTime> {
        chrono::DateTime::from_timestamp(self.start, 0).map(|moment| moment.naive_utc())
    }
}

/// Sign the token.
///
/// The claims are the whole payload — the token is small, and it is read on
/// every hit, so there is nothing to gain by making it shorter.
pub fn sign(secret: &str, claims: &Claims) -> anyhow::Result<String> {
    encode(
        &Header::new(Algorithm::HS256),
        claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(|e| anyhow::anyhow!("signing the cache token: {e}"))
}

/// Read the token back.
///
/// `None` when it is not one of ours: a token signed with another secret, a
/// token that has expired, a token of another kind. **A token that cannot be
/// read is the same as no token at all** — the hit simply starts a session.
pub fn verify(secret: &str, token: &str) -> Option<Claims> {
    let mut validation = Validation::new(Algorithm::HS256);
    // The token has no `exp`: it does not expire on the clock, it goes stale
    // when the visitor is idle for longer than the timeout — which is the
    // session rule, not the token's business.
    validation.required_spec_claims.clear();
    validation.validate_exp = false;
    validation.validate_aud = false;
    decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    )
    .ok()
    .map(|data| data.claims)
    .filter(|claims| claims.token_type == CACHE_TYPE)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "the-secret";

    fn claims() -> Claims {
        Claims::new(
            "site-1",
            Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef),
            Uuid::from_u128(42),
            1_699_999_000,
            1_700_000_000,
        )
    }

    #[test]
    fn a_token_comes_back_as_it_went_in() {
        let token = sign(SECRET, &claims()).unwrap();
        let read = verify(SECRET, &token).expect("the token is ours");
        assert_eq!(read.website_id, "site-1");
        assert_eq!(read.visitor(), Some(Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef)));
        assert_eq!(read.session(), Some(Uuid::from_u128(42)));
        assert_eq!(read.last, 1_700_000_000);
        assert_eq!(read.start, 1_699_999_000);
        assert!(read.started_at().is_some());
    }

    #[test]
    fn a_token_signed_with_another_secret_is_not_read() {
        let token = sign("another-secret", &claims()).unwrap();
        assert!(verify(SECRET, &token).is_none());
    }

    #[test]
    fn a_token_that_is_not_a_token_is_not_read() {
        assert!(verify(SECRET, "not-a-token").is_none());
        assert!(verify(SECRET, "").is_none());
    }

    #[test]
    fn a_login_token_is_not_a_cache_token() {
        // The two are signed with the same secret, so the type is what tells
        // them apart.
        let mut other = claims();
        other.token_type = "login".to_string();
        let token = sign(SECRET, &other).unwrap();
        assert!(verify(SECRET, &token).is_none());
    }

    #[test]
    fn the_client_cannot_change_what_is_in_the_token() {
        let token = sign(SECRET, &claims()).unwrap();
        // The payload is readable — it is signed, not encrypted.
        let payload = token.split('.').nth(1).unwrap();
        assert!(!payload.is_empty());
        // But the signature does not survive a change.
        let mut broken = token.clone();
        let last = broken.len() - 1;
        let replacement = if broken.as_bytes()[last] == b'A' { 'B' } else { 'A' };
        broken.replace_range(last.., &replacement.to_string());
        assert!(verify(SECRET, &broken).is_none());
    }
}
