//! Which session a visitor's activity belongs to.
//!
//! A **session** is a stretch of activity with no gap longer than the idle
//! timeout. The cut is made **when the event arrives**, not when the report
//! runs: the row carries the moment its session started, so every report sees
//! the same sessions, and no report has to sort a visitor's events to find
//! where the gaps were.
//!
//! **The state is a cache, not a store.** It holds the visitors whose session
//! the *server* is managing — the ones seen within the idle timeout — and
//! nothing else. A visitor who is not in it has been quiet for longer than the
//! timeout, so the next event starts a new session, which is what the rule says
//! anyway. So there is nothing to look up in the database, and no index to pay
//! for.
//!
//! **The client's own token wins.** When the client carries one, that *is* the
//! session: the server does not cut it and keeps no state for it (see
//! [`crate::token`]). This module is the server's own answer, for the events
//! that arrive without one.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use sha2::{Digest, Sha256};
use uuid::Uuid;

/// The id of a session: **sixteen bytes**, a UUID.
///
/// It is a pure function of the visitor and the moment the session started —
/// so it can be worked out again by anyone holding the row, and worked out the
/// same way every time. That is what makes it checkable: read the row, work
/// out the id, and if it does not match the one the report used, the row was
/// filed under the wrong session.
///
/// **Nothing depends on that property.** It is a property of the rows *we*
/// wrote, not of the type: a session the **client** named has an id that is a
/// hash of its token and carries no start at all, and the rows imported from
/// another system carry that system's ids. The reports group by the id and take
/// the anchor of a duration from the stored `session_start` — they never work
/// the id out again. ⇒ ⇒ So an id that came from elsewhere is **as good as one
/// we made**: nothing has to be recomputed for it to be right.
///
/// The **start** itself is not in the id — sixteen bytes have no room for it —
/// so it is **stored beside it** (`session_start`). The start is a fact: it is
/// not worked out of a bucket.
pub type VisitId = Uuid;

/// The idle timeout: how long a visitor may be quiet before their next event
/// starts a new session.
///
/// Thirty minutes — the number Umami uses (`now - iat > 1800`), and the one
/// Matomo (`visit_standard_length`) and Plausible (30 minutes) use.
pub const IDLE_TIMEOUT_SECONDS: i64 = 30 * 60;

/// How many visitors are remembered before the quiet ones are dropped.
pub const DEFAULT_CAPACITY: usize = 100_000;

/// What is known about a visitor who is still active.
#[derive(Debug, Clone, Copy)]
struct Active {
    /// When the visitor was last seen — microseconds since the epoch.
    last: i64,
    /// The session they are in.
    visit_id: VisitId,
}

/// The sessions of the visitors who are active right now.
///
/// The key is the **visitor id** (see [`crate::ua::visitor_id`]): a hash of
/// the address prefix, the user agent and the website.
#[derive(Debug)]
pub struct Sessions {
    active: Mutex<HashMap<Uuid, Active>>,
    /// The idle timeout, in microseconds.
    timeout: i64,
    /// How many visitors are remembered.
    capacity: usize,
}

impl Sessions {
    /// Build the state.
    ///
    /// The timeout is how long a visitor may be quiet; the capacity is how many
    /// visitors are remembered (the ones who are quiet are dropped, so this is
    /// a ceiling on the memory, not on the traffic).
    pub fn new(timeout: Duration, capacity: usize) -> Self {
        Self {
            active: Mutex::new(HashMap::with_capacity(1024)),
            timeout: timeout.as_micros() as i64,
            capacity: capacity.max(1),
        }
    }

    /// The session the event belongs to.
    ///
    /// The event happened at `at` (microseconds since the epoch). It joins the
    /// session the visitor is in when the gap since their last event is within
    /// the timeout; otherwise it starts a new one.
    ///
    /// An event that arrives **out of order** — a late import, a clock that
    /// moved — joins the session that is open now. The cut follows the order
    /// the events arrived in, which is the only order the server saw.
    pub fn session(&self, visitor: Uuid, at: i64) -> VisitId {
        let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
        match active.get_mut(&visitor) {
            // Still in the same stretch of activity: the session goes on.
            Some(entry) if at >= entry.last && at - entry.last <= self.timeout => {
                entry.last = at;
                entry.visit_id
            }
            // Out of order: the session stays where it is.
            Some(entry) if at < entry.last => entry.visit_id,
            // Quiet for longer than the timeout: a new session starts here.
            _ => {
                let visit_id = session_id(visitor, at);
                active.insert(visitor, Active { last: at, visit_id });
                if active.len() > self.capacity {
                    self.evict(&mut active, at);
                }
                visit_id
            }
        }
    }

    /// Drop the visitors who have been quiet for longer than the timeout.
    ///
    /// Called only when the map is over its capacity, so the sweep is paid for
    /// once per `capacity` visitors — the cost of the map stays proportional
    /// to its size, not to the traffic.
    fn evict(&self, active: &mut HashMap<Uuid, Active>, now: i64) {
        active.retain(|_, entry| now - entry.last <= self.timeout);
        // Still over: drop the ones who have been quiet the longest.
        while active.len() > self.capacity {
            let Some(oldest) = active
                .iter()
                .min_by_key(|(_, entry)| entry.last)
                .map(|(id, _)| *id)
            else {
                break;
            };
            active.remove(&oldest);
        }
    }

    /// How many visitors are being remembered.
    pub fn len(&self) -> usize {
        self.active.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Is nobody being remembered?
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The id of a session: a pure function of the visitor and the moment the
/// session started.
///
/// **No secret is involved.** The visitor id is already one-way (it is a
/// hash), and the moment is on the row — so the id can be worked out again by
/// anyone holding the row, and worked out the same way every time. That is
/// what makes it checkable: read the row, work out the id, and if it does not
/// match the one the report used, the row was filed under the wrong session.
///
/// **The shape is sixteen bytes**, the same as the visitor's id:
///
/// ```text
/// visit_id = sha256(visitor_id ‖ the second the session started)
/// ```
///
/// ⇒ Two sessions can only collide if their visitors are the same and they
/// start in the same second — which is the same session anyway.
/// ⇒ The same visitor's two segments never start in the same second (there are
/// thirty minutes between them), so they never collide.
pub fn session_id(visitor_id: Uuid, window_start: i64) -> VisitId {
    // The segment start, in seconds.
    let seconds = window_start.div_euclid(1_000_000);

    let mut hasher = Sha256::new();
    hasher.update(visitor_id.as_bytes());
    hasher.update(b"\x1f");
    hasher.update(seconds.to_le_bytes());
    let digest = hasher.finalize();

    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    Uuid::from_bytes(bytes)
}

/// The id of a session the **client** named.
///
/// The client may send its own session id (Umami compatibility). The token is
/// never stored verbatim: it is hashed into 16 bytes, so the column is always
/// the same size and the raw token never reaches the disk. The mapping is
/// **deterministic** — the same token is always the same session.
///
/// A session the client named has **no start**: there is no moment to
/// remember — so its rows carry no `session_start`, and their duration is
/// measured from the first to the last event of the group instead.
pub fn client_session_id(token: &str) -> VisitId {
    crate::ua::uuid_from_text(token)
}

/// The session id as it is shown and passed around: the UUID's own text.
///
/// It is an opaque string — sixteen bytes, fixed length, and short enough that
/// no JavaScript number can lose it (a `BigInt` would not be needed, so the
/// dashboard's own code does not have to care).
pub fn format_visit_id(visit_id: VisitId) -> String {
    visit_id.to_string()
}

/// The session id back from its text.
pub fn parse_visit_id(text: &str) -> Option<VisitId> {
    Uuid::parse_str(text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn visitor(n: u8) -> Uuid {
        // A visitor id is a hash, so the tests make one the same way the
        // server does: out of the bytes.
        let mut bytes = [0u8; 16];
        bytes[0] = n;
        Uuid::from_bytes(bytes)
    }

    const SECOND: i64 = 1_000_000;
    const MINUTE: i64 = 60 * SECOND;
    /// The idle timeout, in microseconds (30 minutes).
    const TIMEOUT: i64 = 1800 * SECOND;

    #[test]
    fn an_event_joins_the_session_it_is_inside() {
        let sessions = Sessions::new(Duration::from_secs(1800), 100);
        let start = 1_700_000_000_000_000;
        let first = sessions.session(visitor(1), start);
        // A minute later: still the same session.
        assert_eq!(sessions.session(visitor(1), start + MINUTE), first);
        // Another visitor has a session of its own.
        assert_ne!(sessions.session(visitor(2), start + MINUTE), first);
    }

    #[test]
    fn a_gap_longer_than_the_timeout_starts_a_new_session() {
        let sessions = Sessions::new(Duration::from_secs(1800), 100);
        let start = 1_700_000_000_000_000;
        let first = sessions.session(visitor(1), start);
        // Exactly the timeout: still the same session (`>` cuts, not `>=`).
        assert_eq!(sessions.session(visitor(1), start + TIMEOUT), first);
        // A timeout later than *that* event: a new one.
        let next = start + TIMEOUT + TIMEOUT + SECOND;
        assert_ne!(sessions.session(visitor(1), next), first);
    }

    #[test]
    fn a_quiet_visitor_is_forgotten() {
        let sessions = Sessions::new(Duration::from_secs(1800), 100);
        let start = 1_700_000_000_000_000;
        let first = sessions.session(visitor(1), start);
        assert_eq!(sessions.len(), 1);
        // Long after: a new session, and the old one is not remembered.
        assert_ne!(sessions.session(visitor(1), start + 3600 * MINUTE), first);
    }

    #[test]
    fn the_map_stays_within_its_capacity() {
        let sessions = Sessions::new(Duration::from_secs(1800), 10);
        let start = 1_700_000_000_000_000;
        for n in 0..100u8 {
            sessions.session(visitor(n), start + i64::from(n) * MINUTE);
        }
        assert!(sessions.len() <= 10, "the map grew to {}", sessions.len());
    }

    #[test]
    fn the_session_id_is_a_function_of_the_visitor_and_the_moment() {
        let a = session_id(visitor(1), 1_700_000_000_000_000);
        let b = session_id(visitor(1), 1_700_000_000_000_000);
        // The same second is the same segment — the id is built out of whole
        // seconds, so a fraction of a second does not make a new session.
        let c = session_id(visitor(1), 1_700_000_000_000_001);
        // A second later is a new segment.
        let d = session_id(visitor(1), 1_700_000_001_000_000);
        let e = session_id(visitor(2), 1_700_000_000_000_000);
        assert_eq!(a, b);
        assert_eq!(a, c);
        assert_ne!(a, d);
        assert_ne!(a, e);
        // Sixteen bytes.
        assert_eq!(a.as_bytes().len(), 16);
    }

    #[test]
    fn a_client_named_session_is_the_same_session_every_time() {
        let id = client_session_id("client-token");
        // The same token is the same session; another token is another one.
        assert_eq!(id, client_session_id("client-token"));
        assert_ne!(id, client_session_id("other-token"));
        assert_eq!(id.as_bytes().len(), 16);
    }

    #[test]
    fn the_id_survives_the_round_trip_through_its_text() {
        for id in [
            session_id(visitor(1), 1_700_000_000_000_000),
            client_session_id("client-token"),
        ] {
            let text = format_visit_id(id);
            assert_eq!(parse_visit_id(&text), Some(id));
        }
        assert_eq!(parse_visit_id("not-a-uuid"), None);
    }
}
