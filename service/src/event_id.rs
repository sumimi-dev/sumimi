//! The event id: a 128-bit UUID in the **v8** layout.
//!
//! # Why this shape
//!
//! The id is two things: **which instance** minted it, and **which event**
//! it is. The two halves are kept apart, and the whole thing stays one
//! value (one column, one key, one reference).
//!
//! # The layout
//!
//! ```text
//!  byte   0    1    2    3    4    5    6    7    8    9    10 … 15
//!       ┌───────────────┬────────┬────────┬───┬────────┬─────────────┐
//!       │ instance 48 b │ ver=8  │ inst 8 │var│ inst 6 │ counter 48 b│
//!       └───────────────┴────────┴────────┴───┴────────┴─────────────┘
//! ```
//!
//! - **instance — 74 random bits** (bytes 0-9, minus the six bits the
//!   format takes): **one value for the whole instance**, drawn from the
//!   operating system once and remembered. Two instances that write the
//!   same site must not share it — at 74 bits, a million instances collide
//!   with a probability of 2.6·10⁻¹¹;
//! - **counter — 48 bits** (bytes 10-15, big-endian): a dense counter,
//!   strictly increasing within the instance. 48 bits is 2.8·10¹⁴ events
//!   — at 100 000 events/s that is eighty-nine years, so **the counter
//!   never wraps**;
//! - **the six fixed bits** are the RFC 9562 **version** nibble (8) and the
//!   **variant** bits (`0b10`). Version 8 is the RFC's own slot for
//!   "the layout is the implementer's" — §5.8: *"The only explicitly
//!   defined bits are those of the version and variant fields, leaving
//!   122 bits for implementation-specific UUIDs."* ⇒ ⇒ **the id is a
//!   legal v8 UUID**, and those six bits are the **layout version**: a
//!   later layout can use a different one, and the two can live in the same
//!   table and still be told apart (`layout_of`).
//!
//! # Why the counter is big-endian and last
//!
//! The counter is the only part that changes from row to row, and **how** it
//! is written decides how well the column compresses. Measured (100 万行,
//! `service/examples/id_layout.rs`):
//!
//! | the counter's byte order | DuckDB (hot tier) | Parquet (cold tier) |
//! |---|---|---|
//! | **big-endian** (the low byte last) | **0.26 B/row** | **0.64 B/row** |
//! | little-endian | 8.65 B/row | 1.02 B/row |
//!
//! ⇒ ⇒ **thirty-three times**, for the same bits. Big-endian is also the
//! UUID's own byte order, so it is what a UUID column stores anyway.
//!
//! # The three rules the layout keeps
//!
//! 1. **the instance half is constant** ⇒ it costs nothing (a dictionary
//!    eats it);
//! 2. **the counter is dense and never jumps** ⇒ it costs nothing (a
//!    difference code eats it);
//! 3. **the counter's bits are in the low bytes** ⇒ the bytes that change
//!    every row sit next to each other.
//!
//! ⇒ ⇒ **the whole id costs 0.64 B/row in Parquet** — less than the 1.02
//! B/row of the 64-bit snowflake it replaces, and 0.26 B/row in the
//! DuckDB hot tier (the snowflake cost 2.88).
//!
//! # Uniqueness
//!
//! - **within one instance**: the counter never repeats (it only grows);
//! - **between two instances**: the instance halves must differ. That is
//!   checked where it can be checked — when a site moves (import), the
//!   source's instance half is compared with ours, and if they are the
//!   same we take a new one (the ones we have used are remembered, so we
//!   never take the same one twice);
//! - **two instances that never meet**: 2.6·10⁻¹¹ at a million instances —
//!   and it only matters if they write the *same* site.

use std::sync::atomic::{AtomicU64, Ordering};

use uuid::Uuid;

/// The instance half's width — the random bits.
pub const INSTANCE_BITS: u32 = 74;

/// The counter's width.
pub const COUNTER_BITS: u32 = 48;

/// The version nibble: **8** = "the layout is ours" (RFC 9562 §5.8).
pub const VERSION: u8 = 8;

/// The version nibble of the **visit** ids: **9** — our second layout.
///
/// The event ids and the visit ids are both "the instance's half + a counter",
/// and the two counters are separate spaces: the number 42 means a different
/// thing in each. So the two layouts must not be able to produce the same
/// value — and the version nibble is what tells them apart (`layout_of`).
///
/// Version 9 is one of the six the RFC leaves for exactly this ("reserved for
/// future definition"), and it is ours to define.
pub const VISIT_VERSION: u8 = 9;

/// The largest instance value.
pub const INSTANCE_MAX: u128 = (1u128 << INSTANCE_BITS) - 1;

/// The largest counter.
pub const COUNTER_MAX: u64 = (1u64 << COUNTER_BITS) - 1;

/// The id: sixteen bytes.
pub type Id = Uuid;

/// **Put the two halves together.**
///
/// The instance's bits are placed around the two fixed fields; the counter
/// goes into the last six bytes, big-endian.
pub fn compose(instance: u128, counter: u64) -> Uuid {
    compose_version(VERSION, instance, counter)
}

/// **Put the two halves together, in a named layout.**
///
/// The version nibble is the layout's name: the event ids are version 8, the
/// visit ids are version 9, and the two can never be the same value.
pub fn compose_version(version: u8, instance: u128, counter: u64) -> Uuid {
    let instance = instance & INSTANCE_MAX;
    let counter = counter & COUNTER_MAX;
    let mut bytes = [0u8; 16];

    // The instance's low 48 bits: bytes 0-5.
    let low = (instance as u64).to_be_bytes();
    bytes[0..6].copy_from_slice(&low[2..8]);
    // Byte 6: the version nibble, then four bits of the instance.
    bytes[6] = ((version & 0x0f) << 4) | ((instance >> 48) & 0x0f) as u8;
    // Byte 7: eight bits of the instance.
    bytes[7] = ((instance >> 52) & 0xff) as u8;
    // Byte 8: the variant bits, then six bits of the instance.
    bytes[8] = 0x80 | ((instance >> 60) & 0x3f) as u8;
    // Byte 9: the last eight bits of the instance.
    bytes[9] = ((instance >> 66) & 0xff) as u8;
    // The counter: the last six bytes, big-endian.
    let counter_bytes = counter.to_be_bytes();
    bytes[10..16].copy_from_slice(&counter_bytes[2..8]);

    Uuid::from_bytes(bytes)
}

/// **The instance half of an id.**
pub fn instance_of(id: &Id) -> u128 {
    let bytes = id.as_bytes();
    let mut instance = 0u128;
    let mut low = [0u8; 8];
    low[2..8].copy_from_slice(&bytes[0..6]);
    instance |= u128::from(u64::from_be_bytes(low));
    instance |= u128::from(bytes[6] & 0x0f) << 48;
    instance |= u128::from(bytes[7]) << 52;
    instance |= u128::from(bytes[8] & 0x3f) << 60;
    instance |= u128::from(bytes[9]) << 66;
    instance
}

/// **The counter of an id.**
pub fn counter_of(id: &Id) -> u64 {
    let bytes = id.as_bytes();
    let mut buffer = [0u8; 8];
    buffer[2..8].copy_from_slice(&bytes[10..16]);
    u64::from_be_bytes(buffer)
}

/// **The layout of an id** — its version nibble.
///
/// A later layout gets a different version ⇒ the two can share a table and
/// still be told apart.
pub fn layout_of(id: &Id) -> u8 {
    id.as_bytes()[6] >> 4
}

/// **The instance half as text** — the first twenty hex characters of the
/// id, which is also where the layout version shows up.
pub fn instance_hex(instance: u128) -> String {
    compose(instance, 0)
        .as_bytes()[0..10]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// **Read the instance half back from its text.**
pub fn instance_from_hex(text: &str) -> Option<u128> {
    if text.len() != 20 {
        return None;
    }
    let mut bytes = [0u8; 10];
    for (index, chunk) in text.as_bytes().chunks(2).enumerate() {
        let pair = std::str::from_utf8(chunk).ok()?;
        bytes[index] = u8::from_str_radix(pair, 16).ok()?;
    }
    let id = Uuid::from_bytes({
        let mut full = [0u8; 16];
        full[0..10].copy_from_slice(&bytes);
        full
    });
    Some(instance_of(&id))
}

/// **Draw an instance half** that is not one of the ones in use.
///
/// A draw is not a promise of uniqueness — and it does not have to be. The
/// half only has to differ from the one this instance is on now and from
/// the ones it has been on before; the two instances that would collide
/// are told apart when the site moves, not when the half is drawn.
pub fn pick_instance(used: &[u128]) -> u128 {
    for _ in 0..64 {
        let bytes = crate::config::random_bytes(10);
        let mut candidate = 0u128;
        for byte in bytes {
            candidate = (candidate << 8) | u128::from(byte);
        }
        candidate &= INSTANCE_MAX;
        if !used.contains(&candidate) {
            return candidate;
        }
    }
    // The random draw kept hitting a used half: take the first free one.
    (0..=INSTANCE_MAX).find(|half| !used.contains(half)).unwrap_or(0)
}

/// **Hands out the ids.**
///
/// The state is the layout (which version nibble the ids carry), the instance
/// half (fixed) and the counter (grows).
pub struct Generator {
    version: u8,
    instance: u128,
    counter: AtomicU64,
}

impl Generator {
    /// A generator that starts from scratch.
    pub fn new(instance: u128) -> Self {
        Self::with_version(VERSION, instance)
    }

    /// A generator in a named layout.
    pub fn with_version(version: u8, instance: u128) -> Self {
        Self {
            version: version & 0x0f,
            instance: instance & INSTANCE_MAX,
            counter: AtomicU64::new(0),
        }
    }

    /// A generator that starts above a counter that is already in use.
    pub fn with_counter(instance: u128, counter: u64) -> Self {
        Self::with_version_counter(VERSION, instance, counter)
    }

    /// A generator in a named layout, starting above a counter in use.
    pub fn with_version_counter(version: u8, instance: u128, counter: u64) -> Self {
        Self {
            version: version & 0x0f,
            instance: instance & INSTANCE_MAX,
            counter: AtomicU64::new(counter & COUNTER_MAX),
        }
    }

    /// The layout the ids carry.
    pub fn version(&self) -> u8 {
        self.version
    }

    /// The instance half of the ids this generator hands out.
    pub fn instance(&self) -> u128 {
        self.instance
    }

    /// The counter that was handed out last.
    pub fn counter(&self) -> u64 {
        self.counter.load(Ordering::Acquire)
    }

    /// The next id.
    pub fn next(&self) -> Id {
        let counter = self.counter.fetch_add(1, Ordering::AcqRel) + 1;
        compose_version(self.version, self.instance, counter)
    }

    /// The next id, at a given counter — for the tests.
    pub fn next_at(&self, counter: u64) -> Id {
        compose_version(self.version, self.instance, counter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn the_two_halves_survive_the_round_trip() {
        let id = compose(0x0123_4567_89ab_cdef_0123_4567_89ab, 0x1234_5678_9abc);
        assert_eq!(instance_of(&id), 0x0123_4567_89ab_cdef_0123_4567_89ab & INSTANCE_MAX);
        assert_eq!(counter_of(&id), 0x1234_5678_9abc);
    }

    #[test]
    fn the_id_is_a_legal_v8() {
        let id = compose(12345, 1);
        assert_eq!(layout_of(&id), VERSION);
        // The variant bits: the top two bits of byte 8 are 0b10.
        assert_eq!(id.as_bytes()[8] >> 6, 0b10);
        // The version nibble is the high nibble of byte 6.
        assert_eq!(id.as_bytes()[6] >> 4, VERSION);
    }

    #[test]
    fn the_counter_grows_and_never_repeats() {
        let generator = Generator::new(7);
        let mut last = 0u64;
        for _ in 0..10_000 {
            let id = generator.next();
            let counter = counter_of(&id);
            assert!(counter > last, "the counter must grow: {counter} after {last}");
            last = counter;
        }
    }

    #[test]
    fn the_ids_are_unique() {
        let generator = Generator::new(7);
        let ids: HashSet<Id> = (0..5000).map(|_| generator.next()).collect();
        assert_eq!(ids.len(), 5000);
    }

    #[test]
    fn two_instances_with_different_halves_do_not_collide() {
        let one = Generator::new(1);
        let two = Generator::new(2);
        let ids: HashSet<Id> = (0..1000)
            .flat_map(|_| [one.next(), two.next()])
            .collect();
        assert_eq!(ids.len(), 2000);
    }

    #[test]
    fn the_generator_starts_above_the_counters_that_are_in_use() {
        let generator = Generator::with_counter(7, 12_345);
        assert_eq!(generator.counter(), 12_345);
        assert_eq!(counter_of(&generator.next()), 12_346);
    }

    #[test]
    fn the_counter_never_leaves_its_48_bits() {
        let id = compose(1, COUNTER_MAX);
        assert_eq!(counter_of(&id), COUNTER_MAX);
        // One past the end is the counter's ceiling.
        assert_eq!(COUNTER_MAX, (1u64 << COUNTER_BITS) - 1);
    }

    #[test]
    fn the_instance_half_reads_back_from_its_text() {
        let instance = 0x0123_4567_89ab_cdef_0123 & INSTANCE_MAX;
        let text = instance_hex(instance);
        assert_eq!(text.len(), 20);
        assert_eq!(instance_from_hex(&text), Some(instance));
        assert_eq!(instance_from_hex("nonsense"), None);
    }

    #[test]
    fn the_picked_half_is_not_one_that_is_in_use() {
        let used = vec![1, 2, 3];
        let half = pick_instance(&used);
        assert!(!used.contains(&half));
        assert!(half <= INSTANCE_MAX);
    }
}
