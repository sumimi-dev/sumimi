//! The secrets — the JWT signing key, the password salt, the generated passwords.
//!
//! The point of these tests: a secret that is not random is a secret that everybody
//! knows. The RNG behind them is a small shim over `/dev/urandom`, and a shim is
//! exactly the kind of code that can be wrong in a way nothing notices — the
//! server starts, the login works, and every install shares the same key.

use sumimi_service::config::{random_bytes, random_secret};

/// A secret is not a run of zeros.
#[test]
fn a_secret_is_not_all_zeros() {
    let secret = random_secret();
    assert_eq!(secret.len(), 64, "the secret should be 32 bytes of hex");
    assert!(
        secret.chars().any(|c| c != '0'),
        "the secret is all zeros: {secret}"
    );
}

/// Two secrets are not the same secret.
#[test]
fn two_secrets_differ() {
    let first = random_secret();
    let second = random_secret();
    assert_ne!(first, second, "the RNG returned the same secret twice");
}

/// The bytes are random too — the salt of the password hash comes from here.
#[test]
fn the_bytes_are_not_all_zeros() {
    let bytes = random_bytes(18);
    assert_eq!(bytes.len(), 18);
    assert!(
        bytes.iter().any(|b| *b != 0),
        "the bytes are all zeros: {bytes:?}"
    );
}

/// The bytes differ from call to call.
#[test]
fn two_byte_runs_differ() {
    assert_ne!(random_bytes(18), random_bytes(18));
}

/// A long run of bytes is not all zeros either — this is what catches a buffer
/// that is refilled too rarely.
#[test]
fn a_long_run_is_not_all_zeros() {
    let bytes = random_bytes(256);
    assert!(
        bytes.iter().any(|b| *b != 0),
        "256 bytes came out all zeros"
    );
}
