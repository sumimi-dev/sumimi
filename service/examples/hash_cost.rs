//! What does one hash cost?
//!
//! The visitor identity is a hash of the address prefix and the user agent.
//! The question is *when* it is computed: once when the event is written, or
//! on every row of every report.
//!
//! This measures the first half — what one hash costs on this machine.
use sha2::{Digest, Sha256};
use std::time::Instant;

fn main() {
    let agent = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
    let prefix = "203.0.113";
    let n = 1_000_000u32;

    // The plain hash: what the identity would be.
    let started = Instant::now();
    let mut sink = 0u64;
    for _ in 0..n {
        let mut hasher = Sha256::new();
        hasher.update(prefix.as_bytes());
        hasher.update(b"\x1f");
        hasher.update(agent.as_bytes());
        let digest = hasher.finalize();
        sink = sink.wrapping_add(u64::from_le_bytes(digest[..8].try_into().unwrap()));
    }
    let plain = started.elapsed().as_secs_f64() / f64::from(n) * 1e9;

    // The same, with a key: an HMAC costs two passes.
    let key = b"a 32-byte instance secret........";
    let started = Instant::now();
    for _ in 0..n {
        let mut inner = Sha256::new();
        inner.update(key);
        inner.update(prefix.as_bytes());
        inner.update(b"\x1f");
        inner.update(agent.as_bytes());
        let inner = inner.finalize();
        let mut outer = Sha256::new();
        outer.update(key);
        outer.update(inner);
        let digest = outer.finalize();
        sink = sink.wrapping_add(u64::from_le_bytes(digest[..8].try_into().unwrap()));
    }
    let hmac = started.elapsed().as_secs_f64() / f64::from(n) * 1e9;

    println!("the agent is {} bytes", agent.len());
    println!("a plain hash   {plain:.1} ns per event");
    println!("an HMAC        {hmac:.1} ns per event");
    println!();
    println!(
        "at 20 000 events/s:  {:.2}% of one core (plain),  {:.2}% (HMAC)",
        plain / 1e9 * 20_000.0 * 100.0,
        hmac / 1e9 * 20_000.0 * 100.0
    );
    println!(
        "at 100 000 events/s: {:.2}% of one core (plain),  {:.2}% (HMAC)",
        plain / 1e9 * 100_000.0 * 100.0,
        hmac / 1e9 * 100_000.0 * 100.0
    );
    println!();
    println!("(sink {sink})");
}
