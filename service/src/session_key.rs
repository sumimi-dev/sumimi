//! The instance's secret — the value that makes a visitor id **un-recomputable**.
//!
//! # What a secret is here
//!
//! **The formula is public; the value is not.** The privacy page may say
//! *how* a visitor id is derived — that is not a secret, and hiding it helps
//! nobody. What must not be published is the **key**: with the key, anybody
//! who knows a visitor's address and agent can work out their id; without it,
//! the id is a number that means nothing on its own.
//!
//! # Where it comes from
//!
//! | | |
//! |---|---|
//! | [`Source::None`] | **no key** — the derivation is pure, the formula is all there is |
//! | [`Source::Generated`] | **the instance makes one** on first run and keeps it (the default) |
//! | [`Source::Custom`] | **the owner brings one** — only they know it |
//!
//! # Where it lives
//!
//! **Not in the database.** A key kept beside the events travels with every
//! backup of them, and a backup that carries the key carries no protection at
//! all. It is a **file** — next to the signing secret, in the instance
//! directory — and it is written with `0600`.
//!
//! ⇒ ⇒ The honest limit: a backup of the **whole directory** carries it too ⇒ ⇒
//! an owner who wants that protection moves it out of the way (an environment
//! variable, a secret store) and points [`Source::Custom`] at it.
//!
//! # Rotation
//!
//! The key may be **replaced** on a schedule ([`Rotation`]). A visitor's id
//! then changes at the boundary, so ids from different segments cannot be
//! correlated — the same trade Plausible makes, with a salt it rotates every
//! night.
//!
//! ⇒ ⇒ The **previous** key is kept for two days ([`PREVIOUS_LIFETIME`]),
//! so a visit that straddles the boundary stays one visit instead of two.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// How long the **previous** key is kept after a rotation.
///
/// Two days — the same window Plausible keeps its old salt for, and long
/// enough for a visit that straddles the boundary to be recognised.
pub const PREVIOUS_LIFETIME: i64 = 48 * 3600;

/// Where the secret comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// **No secret.** The derivation is pure: the formula is public, so
    /// anybody can work out a visitor's id from their address and agent.
    /// The two design intents of v0.6 live here — the same visitor has the
    /// same id on every server, and an offline import can work it out itself.
    None,
    /// **The instance generates one** on first run and keeps it.
    Generated,
    /// **The owner brings one.** Only they know it, and only they can rotate it.
    Custom,
}

impl Source {
    /// The name of a source — the value of `SUMIMI_SESSION_KEY`.
    pub fn as_str(self) -> &'static str {
        match self {
            Source::None => "none",
            Source::Generated => "generated",
            Source::Custom => "custom",
        }
    }

    /// Read a source from a name. `None` when the name is not ours.
    pub fn parse(text: &str) -> Option<Source> {
        match text.trim().to_ascii_lowercase().as_str() {
            "none" | "off" | "pure" => Some(Source::None),
            "generated" | "auto" | "instance" => Some(Source::Generated),
            "custom" | "own" => Some(Source::Custom),
            _ => None,
        }
    }
}

/// When the secret is replaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rotation {
    /// **Never** — the secret stays for the life of the instance (the default).
    ///
    /// ⇒ ⇒ A visitor's id is then stable across days, and the visitor count
    /// of a window is a real count of people, not a sum of daily counts.
    Never,
    /// **Every N days**, on the UTC day boundary.
    Every(u64),
}

impl Rotation {
    /// The name of a rotation — the value of `SUMIMI_SESSION_ROTATION`.
    pub fn as_str(self) -> String {
        match self {
            Rotation::Never => "never".to_string(),
            Rotation::Every(days) => format!("{days}d"),
        }
    }

    /// Read a rotation from a name: `never`, `day`, `1d`, `7d`, `30d` …
    pub fn parse(text: &str) -> Option<Rotation> {
        let text = text.trim().to_ascii_lowercase();
        match text.as_str() {
            "never" | "off" | "none" => Some(Rotation::Never),
            "day" | "1d" => Some(Rotation::Every(1)),
            _ => {
                let days = text.strip_suffix('d')?.parse::<u64>().ok()?;
                if days == 0 {
                    None
                } else {
                    Some(Rotation::Every(days))
                }
            }
        }
    }

    /// The length of a period, in days. `None` when there is no rotation.
    pub fn days(self) -> Option<u64> {
        match self {
            Rotation::Never => None,
            Rotation::Every(days) => Some(days),
        }
    }

    /// The **segment** a moment falls in — the boundaries are multiples of the
    /// period, counted from the epoch, so everybody (the server and the
    /// reports) works the number out the same way.
    pub fn segment(self, at: i64) -> u64 {
        match self {
            Rotation::Never => 0,
            Rotation::Every(days) => (at / (days as i64 * 86400)).max(0) as u64,
        }
    }

    /// The next boundary after a moment.
    pub fn next_boundary(self, at: i64) -> Option<i64> {
        let days = self.days()? as i64;
        Some((self.segment(at) as i64 + 1) * days * 86400)
    }
}

/// The file the secret is kept in — on disk, never in the database.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct KeyFile {
    /// The shape of the file, so a later one can be told apart.
    version: u32,
    /// Where the key came from.
    source: String,
    /// When it is replaced.
    rotation: String,
    /// The key itself, in hex.
    #[serde(default)]
    current: String,
    /// The key before the last rotation, and the day it stops being used.
    #[serde(default)]
    previous: Option<Previous>,
    /// The instance's own id — the scope of the `instance` tier.
    #[serde(default)]
    instance: String,
    /// When the key was last replaced (unix seconds).
    #[serde(default)]
    rotated_at: i64,
}

/// The key before the last rotation.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Previous {
    /// The key, in hex.
    key: String,
    /// The day it stops being used (unix seconds).
    until: i64,
}

/// The instance's secret, and the rules around it.
#[derive(Debug, Clone)]
pub struct SessionKey {
    /// Where the key came from.
    source: Source,
    /// When the key is replaced.
    rotation: Rotation,
    /// The key in use.
    current: Vec<u8>,
    /// The key before the last rotation, and the day it stops being used.
    previous: Option<(Vec<u8>, i64)>,
    /// The instance's own id — the scope of the `instance` tier.
    instance: Vec<u8>,
    /// When the key was last replaced.
    rotated_at: i64,
    /// Where the file lives.
    path: PathBuf,
}

impl SessionKey {
    /// Load the secret, writing the file when it is not there yet.
    ///
    /// `custom` is the owner's own key: it comes from `value` (the contents of
    /// the file they pointed at, or the string they set) and it is never
    /// written to our file — only the instance's id is.
    pub fn load(
        source: Source,
        value: Option<Vec<u8>>,
        rotation: Rotation,
        path: &Path,
    ) -> anyhow::Result<SessionKey> {
        let mut file = read_file(path)?;
        let fresh = file.is_none();

        if fresh {
            file = Some(KeyFile {
                version: 1,
                source: source.as_str().to_string(),
                rotation: rotation.as_str(),
                current: String::new(),
                previous: None,
                instance: hex(&crate::config::random_bytes(16)),
                // **The key is made now** ⇒ ⇒ the first replacement is at the
                // next boundary, not at the moment the server starts.
                rotated_at: chrono::Utc::now().timestamp(),
            });
        }
        let mut file = file.expect("the file is there");

        // The key itself.
        let current = match source {
            Source::None => Vec::new(),
            Source::Custom => value.context(
                "the session key is custom, but no value was given (set SUMIMI_SESSION_KEY_FILE or SUMIMI_SESSION_KEY_VALUE)",
            )?,
            Source::Generated => {
                if file.current.is_empty() {
                    file.current = hex(&crate::config::random_bytes(32));
                }
                unhex(&file.current).context("the session key in the file is not hex")?
            }
        };

        let previous = match &file.previous {
            Some(previous) => match unhex(&previous.key) {
                Ok(key) => Some((key, previous.until)),
                Err(_) => None,
            },
            None => None,
        };

        let mut key = SessionKey {
            source,
            rotation,
            current,
            previous,
            instance: unhex(&file.instance).unwrap_or_default(),
            rotated_at: file.rotated_at,
            path: path.to_path_buf(),
        };

        if key.instance.is_empty() {
            key.instance = crate::config::random_bytes(16);
            file.instance = hex(&key.instance);
        }
        if fresh || source == Source::Generated {
            key.write(&mut file)?;
        }
        Ok(key)
    }

    /// A key with no file — for the tests and for the pure mode.
    pub fn none() -> SessionKey {
        SessionKey {
            source: Source::None,
            rotation: Rotation::Never,
            current: Vec::new(),
            previous: None,
            instance: Vec::new(),
            rotated_at: 0,
            path: PathBuf::new(),
        }
    }

    /// Where the key came from.
    pub fn source(&self) -> Source {
        self.source
    }

    /// When the key is replaced.
    pub fn rotation(&self) -> Rotation {
        self.rotation
    }

    /// The key in use — `None` when there is none.
    pub fn secret(&self) -> Option<&[u8]> {
        if self.current.is_empty() {
            None
        } else {
            Some(&self.current)
        }
    }

    /// The instance's own id — the scope of the `instance` tier.
    pub fn instance_id(&self) -> String {
        hex(&self.instance)
    }

    /// The visitor's id, derived with the key in use.
    ///
    /// **With a key**: `HMAC-SHA256(key, scope ‖ address ‖ agent)`.
    /// **Without one**: the pure derivation — the same bytes the server has
    /// always written, so a site that turns the key off gets its old ids back.
    pub fn derive(&self, scope: &str, address: &str, agent: &str) -> Uuid {
        match self.secret() {
            Some(key) => keyed_id(key, scope, address, agent),
            None => pure_id(scope, address, agent),
        }
    }

    /// The visitor's id as the **previous** key would derive it — `None` when
    /// there is no previous key, or when it has expired.
    ///
    /// The collection path uses it to keep a visit that straddles a rotation
    /// boundary whole: the visitor keeps the id they had, so they are not
    /// counted twice.
    pub fn derive_previous(&self, scope: &str, address: &str, agent: &str, now: i64) -> Option<Uuid> {
        let (key, until) = self.previous.as_ref()?;
        if now > *until {
            return None;
        }
        Some(keyed_id(key, scope, address, agent))
    }

    /// Replace the key when the period has passed. `true` when it did.
    ///
    /// The **old** key is kept for [`PREVIOUS_LIFETIME`] — long enough for a
    /// visit that straddles the boundary to be recognised as one visit.
    pub fn rotate_if_due(&mut self, now: i64) -> bool {
        if self.rotation.days().is_none() {
            return false;
        }
        if self.source != Source::Generated {
            // Only a key we made is ours to replace.
            return false;
        }
        let segment = self.rotation.segment(now);
        let last = self.rotation.segment(self.rotated_at);
        if segment == last {
            return false;
        }
        self.previous = Some((self.current.clone(), now + PREVIOUS_LIFETIME));
        self.current = crate::config::random_bytes(32);
        self.rotated_at = now;
        let _ = self.write_file();
        true
    }

    /// Write the file — the key, the instance's id and the rotation state.
    fn write(&mut self, file: &mut KeyFile) -> anyhow::Result<()> {
        file.source = self.source.as_str().to_string();
        file.rotation = self.rotation.as_str();
        file.current = hex(&self.current);
        file.instance = hex(&self.instance);
        file.rotated_at = self.rotated_at;
        file.previous = self.previous.as_ref().map(|(key, until)| Previous {
            key: hex(key),
            until: *until,
        });
        self.write_file()
    }

    /// Write the file, with `0600`.
    fn write_file(&self) -> anyhow::Result<()> {
        let file = KeyFile {
            version: 1,
            source: self.source.as_str().to_string(),
            rotation: self.rotation.as_str(),
            current: hex(&self.current),
            previous: self.previous.as_ref().map(|(key, until)| Previous {
                key: hex(key),
                until: *until,
            }),
            instance: hex(&self.instance),
            rotated_at: self.rotated_at,
        };
        let text = serde_json::to_string_pretty(&file)?;
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).ok();
            }
        }
        // **Write beside it, then move it in** — a half-written key file is a
        // key nobody can derive with.
        let temporary = self.path.with_extension("tmp");
        std::fs::write(&temporary, text)
            .with_context(|| format!("writing the session key file {}", temporary.display()))?;
        crate::config::restrict_permissions(&temporary);
        std::fs::rename(&temporary, &self.path).with_context(|| {
            format!("moving the session key file into {}", self.path.display())
        })?;
        Ok(())
    }
}

/// Read the file — `None` when it is not there.
fn read_file(path: &Path) -> anyhow::Result<Option<KeyFile>> {
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading the session key file {}", path.display()))?;
    match serde_json::from_str(&text) {
        Ok(file) => Ok(Some(file)),
        Err(error) => Err(anyhow::anyhow!(
            "the session key file {} is not readable ({error}) — move it away to start over",
            path.display()
        )),
    }
}

/// The visitor's id, **with** a key: `HMAC-SHA256(key, scope ‖ address ‖ agent)`.
pub fn keyed_id(key: &[u8], scope: &str, address: &str, agent: &str) -> Uuid {
    let mut message = Vec::new();
    message.extend_from_slice(scope.as_bytes());
    message.push(0x1f);
    message.extend_from_slice(address.as_bytes());
    message.push(0x1f);
    message.extend_from_slice(agent.as_bytes());
    let digest = hmac_sha256(key, &message);
    uuid_from_bytes(&digest[..16])
}

/// The visitor's id, **without** a key — the pure derivation.
///
/// **The same bytes the server has always written** ⇒ ⇒ a site that turns the
/// key off gets the ids it had before ⇒ ⇒ the offline import can work them
/// out for itself.
pub fn pure_id(scope: &str, address: &str, agent: &str) -> Uuid {
    let mut hasher = Sha256::new();
    hasher.update(address.as_bytes());
    hasher.update(b"\x1f");
    hasher.update(agent.as_bytes());
    hasher.update(b"\x1f");
    hasher.update(scope.as_bytes());
    uuid_from_bytes(&hasher.finalize()[..16])
}

/// **HMAC-SHA256** — written out, so the build keeps its dependency list short.
///
/// The construction is the one in RFC 2104: the key is padded to a block, the
/// inner hash is taken over `ipad ‖ message`, and the outer hash over
/// `opad ‖ inner`.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        let digest = Sha256::digest(key);
        block[..32].copy_from_slice(&digest);
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for index in 0..BLOCK {
        ipad[index] ^= block[index];
        opad[index] ^= block[index];
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(message);
    let inner = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner);
    outer.finalize().into()
}

/// A UUID from sixteen bytes.
fn uuid_from_bytes(bytes: &[u8]) -> Uuid {
    let mut raw = [0u8; 16];
    raw.copy_from_slice(&bytes[..16]);
    Uuid::from_bytes(raw)
}

/// Bytes as hex.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Hex as bytes.
fn unhex(text: &str) -> anyhow::Result<Vec<u8>> {
    let text = text.trim();
    if text.len() % 2 != 0 {
        anyhow::bail!("not hex: an odd number of digits");
    }
    let mut bytes = Vec::with_capacity(text.len() / 2);
    for pair in text.as_bytes().chunks(2) {
        let text = std::str::from_utf8(pair)?;
        bytes.push(u8::from_str_radix(text, 16)?);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first test case of RFC 4231 — the HMAC is the one everybody uses.
    #[test]
    fn the_hmac_is_the_one_of_rfc_4231() {
        let key = [0x0bu8; 20];
        let digest = hmac_sha256(&key, b"Hi There");
        let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(
            hex,
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn a_key_changes_the_id() {
        let scope = "website-1";
        let address = "203.0.113.9";
        let agent = "Mozilla/5.0";
        let with = keyed_id(b"a-key", scope, address, agent);
        let without = pure_id(scope, address, agent);
        assert_ne!(with, without);
        assert_eq!(with, keyed_id(b"a-key", scope, address, agent));
    }

    #[test]
    fn the_scope_is_in_the_id() {
        let a = keyed_id(b"a-key", "website-1", "203.0.113.9", "Mozilla/5.0");
        let b = keyed_id(b"a-key", "website-2", "203.0.113.9", "Mozilla/5.0");
        assert_ne!(a, b);
    }

    #[test]
    fn the_pure_id_is_the_one_the_server_has_always_written() {
        // SHA-256(address ‖ 0x1f ‖ agent ‖ 0x1f ‖ website_id), the first
        // sixteen bytes.
        let mut hasher = Sha256::new();
        hasher.update(b"203.0.113.9");
        hasher.update(b"\x1f");
        hasher.update(b"Mozilla/5.0");
        hasher.update(b"\x1f");
        hasher.update(b"website-1");
        let expected = uuid_from_bytes(&hasher.finalize()[..16]);
        assert_eq!(pure_id("website-1", "203.0.113.9", "Mozilla/5.0"), expected);
    }

    #[test]
    fn a_source_is_read_from_its_name() {
        assert_eq!(Source::parse("none"), Some(Source::None));
        assert_eq!(Source::parse("Generated"), Some(Source::Generated));
        assert_eq!(Source::parse("custom"), Some(Source::Custom));
        assert_eq!(Source::parse("whatever"), None);
    }

    #[test]
    fn a_rotation_is_read_from_its_name() {
        assert_eq!(Rotation::parse("never"), Some(Rotation::Never));
        assert_eq!(Rotation::parse("day"), Some(Rotation::Every(1)));
        assert_eq!(Rotation::parse("1d"), Some(Rotation::Every(1)));
        assert_eq!(Rotation::parse("30d"), Some(Rotation::Every(30)));
        assert_eq!(Rotation::parse("0d"), None);
        assert_eq!(Rotation::parse("soon"), None);
    }

    #[test]
    fn the_boundaries_are_the_same_for_everybody() {
        let rotation = Rotation::Every(1);
        let midnight = 1_700_000_000; // a moment in the day
        let segment = rotation.segment(midnight);
        let next = rotation.next_boundary(midnight).unwrap();
        assert!(next > midnight);
        assert_eq!(rotation.segment(next - 1), segment);
        assert_eq!(rotation.segment(next), segment + 1);
    }

    #[test]
    fn a_generated_key_is_kept_in_the_file_and_read_back() {
        let dir = std::env::temp_dir().join(format!("sumimi-key-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session-key");

        let key = SessionKey::load(Source::Generated, None, Rotation::Never, &path).unwrap();
        assert!(path.exists());
        assert!(key.secret().is_some());

        let again = SessionKey::load(Source::Generated, None, Rotation::Never, &path).unwrap();
        assert_eq!(again.secret(), key.secret(), "the same key is read back");
        assert_eq!(again.instance_id(), key.instance_id());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_custom_key_is_the_owners_own() {
        let dir = std::env::temp_dir().join(format!("sumimi-key-custom-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session-key");

        let key =
            SessionKey::load(Source::Custom, Some(b"mine".to_vec()), Rotation::Never, &path).unwrap();
        assert_eq!(key.secret(), Some(&b"mine"[..]));
        // The file does not hold the owner's key.
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("mine"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rotation_keeps_the_old_key_for_two_days() {
        let dir = std::env::temp_dir().join(format!("sumimi-key-rotate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session-key");

        let mut key =
            SessionKey::load(Source::Generated, None, Rotation::Every(1), &path).unwrap();
        let first = key.secret().unwrap().to_vec();
        let now = chrono::Utc::now().timestamp();
        assert!(!key.rotate_if_due(now), "the same day does not rotate");
        assert!(key.rotate_if_due(now + 86400), "the next day does");
        assert_ne!(key.secret().unwrap(), &first[..]);

        // The old key still derives the visitor's id for two days.
        let previous = key.derive_previous("website-1", "203.0.113.9", "Mozilla/5.0", now + 86400);
        assert!(previous.is_some());
        assert!(key
            .derive_previous("website-1", "203.0.113.9", "Mozilla/5.0", now + 86400 + PREVIOUS_LIFETIME + 1)
            .is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
