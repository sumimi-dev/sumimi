//! The matrix: three visitor-identity designs × two engines.
//!
//! For each design, on each engine: the file on disk, and the three queries
//! the dashboard asks (the summary, the bounces with the average session
//! length, and an audience breakdown).
//!
//! | design | what the row carries | what the report counts over |
//! | --- | --- | --- |
//! | `the pair` | the address prefix and the user agent | the pair |
//! | `a hash` | one 16-byte value derived from the pair | that value |
//! | `the pair + a key` | the pair **and** an 8-byte value | that value |
//!
//! **The visitor count has to be the same in all of them** — that is the
//! invariant the whole thing rests on, so it is checked on every row of the
//! matrix.

use std::path::PathBuf;
use std::time::Instant;

use sumimi_service::db::SCHEMA_VERSION;
use sumimi_service::pool::Db;
use sumimi_service::sqlite::SqliteBackend;
use sumimi_service::storage::Storage;

/// The designs.
const DESIGNS: [(&str, &str); 15] = [
    ("the pair (prefix + agent)", "pair"),
    ("a 16-byte hash", "hash"),
    ("an 8-byte hash", "hash8"),
    ("the pair + an 8-byte key", "pair+key"),
    ("the address + the agent's hash", "packed"),
    ("two ids (network + agent)", "ids"),
    ("12 bytes, two columns (8+4)", "split8+4"),
    ("12 bytes, two columns (4+8)", "split4+8"),
    ("an 8-byte hash, as a blob", "blob8"),
    ("an 8-byte hash, as hex text", "hex8"),
    ("an 8-byte hash, as base64", "b64"),
    ("an 8-byte hash, base64, no pad", "b64np"),
    ("a 16-byte hash, as a blob", "blob16"),
    ("a 32-byte hash, as a blob", "blob32"),
    ("a 16-byte hash, as uuid text", "uuidtext"),
];

/// The summary's first query: the three numbers at the top of the dashboard.
const SUMMARY_DUCK: &str = "SELECT COUNT(*) FILTER (WHERE event_type NOT IN ('event', 'identify')),
                              COUNT(DISTINCT {key}),
                              COUNT(DISTINCT visit_id)
                       FROM event WHERE website_id = 'w1'";

const SUMMARY_SQLITE: &str =
    "SELECT COUNT(*) FILTER (WHERE event_type NOT IN ('event', 'identify')),
                                 COUNT(DISTINCT {key}),
                                 COUNT(DISTINCT visit_id)
                          FROM event WHERE website_id = 'w1'";

/// The summary's second query: the bounces and the average session length.
const BOUNCES_DUCK: &str = "SELECT COUNT(*) FILTER (WHERE n = 1 AND custom = 0),
                               COALESCE(AVG(d), 0)
                        FROM (
                            SELECT visit_id,
                                   COUNT(*) FILTER (WHERE event_type NOT IN ('event', 'identify')) AS n,
                                   MAX(CASE WHEN event_type = 'event' THEN 1 ELSE 0 END) AS custom,
                                   CASE
                                       WHEN MAX(created_at) FILTER (WHERE event_type NOT IN ('event', 'identify')) IS NULL THEN NULL
                                       WHEN visit_id < 0
                                           THEN GREATEST(
                                               date_diff('microsecond',
                                                   MIN(created_at) FILTER (WHERE event_type NOT IN ('event', 'identify')),
                                                   MAX(created_at) FILTER (WHERE event_type NOT IN ('event', 'identify'))
                                               ), 0) / 1000000.0
                                       ELSE GREATEST(
                                               epoch_us(MAX(created_at) FILTER (WHERE event_type NOT IN ('event', 'identify')))
                                               - (visit_id >> 32) * 1000000, 0) / 1000000.0
                                   END AS d
                            FROM event WHERE website_id = 'w1'
                            GROUP BY visit_id
                        )";

const BOUNCES_SQLITE: &str = "SELECT COUNT(*) FILTER (WHERE n = 1 AND custom = 0),
                                  COALESCE(AVG(d), 0)
                           FROM (
                               SELECT visit_id,
                                      COUNT(*) FILTER (WHERE event_type NOT IN ('event', 'identify')) AS n,
                                      MAX(CASE WHEN event_type = 'event' THEN 1 ELSE 0 END) AS custom,
                                      CASE
                                          WHEN MAX(created_at) FILTER (WHERE event_type NOT IN ('event', 'identify')) IS NULL THEN NULL
                                          WHEN visit_id < 0
                                              THEN MAX(
                                                  MAX(created_at) FILTER (WHERE event_type NOT IN ('event', 'identify'))
                                                  - MIN(created_at) FILTER (WHERE event_type NOT IN ('event', 'identify')), 0) / 1000000.0
                                          ELSE MAX(
                                              MAX(created_at) FILTER (WHERE event_type NOT IN ('event', 'identify'))
                                              - (visit_id >> 32) * 1000000, 0) / 1000000.0
                                      END AS d
                               FROM event WHERE website_id = 'w1'
                               GROUP BY visit_id
                           )";

/// The breakdown of an audience dimension — the visitors per browser.
const BREAKDOWN: &str = "SELECT COALESCE(browser, 'unknown') AS name, COUNT(DISTINCT {key}) AS value
                          FROM event WHERE website_id = 'w1' AND event_type NOT IN ('event', 'identify')
                          GROUP BY name ORDER BY value DESC LIMIT 50";

/// The SQL that makes a 16-byte hash out of the pair (DuckDB has sha256).
fn uuid_of_pair() -> String {
    let digest = "sha256(ip_prefix || chr(31) || user_agent)";
    format!(
        "CAST(substr({digest}, 1, 8) || '-' || substr({digest}, 9, 4) || '-' || \
         substr({digest}, 13, 4) || '-' || substr({digest}, 17, 4) || '-' || \
         substr({digest}, 21, 12) AS UUID)"
    )
}

/// The IPv4 address as one number.
fn address_of() -> String {
    let part = |n: u32| format!("CAST(split_part(ip, '.', {n}) AS BIGINT)");
    format!(
        "{} * 16777216 + {} * 65536 + {} * 256 + {}",
        part(1),
        part(2),
        part(3),
        part(4)
    )
}

/// The key each design's report counts over.
fn key_of(design: &str, engine: &str) -> String {
    match (design, engine) {
        ("pair", "duckdb") => "(ip_prefix, user_agent)".to_string(),
        ("pair", _) => "ip_prefix || char(31) || user_agent".to_string(),
        ("hash", _) | ("hash8", _) | ("packed", _) => "visitor_id".to_string(),
        // SQLite has no row values in COUNT(DISTINCT) — it concatenates.
        ("ids", "duckdb") => "(network_id, agent_id)".to_string(),
        ("ids", _) => "network_id || char(31) || agent_id".to_string(),
        ("blob8", _) | ("hex8", _) | ("b64", _) | ("b64np", _) | ("blob16", _) | ("blob32", _)
        | ("uuidtext", _) => "visitor_id".to_string(),
        ("split8+4", "duckdb") => "(ip_num, ua_hash)".to_string(),
        ("split8+4", _) => "ip_num || char(31) || ua_hash".to_string(),
        ("split4+8", "duckdb") => "(ip_num, ua_hash)".to_string(),
        ("split4+8", _) => "ip_num || char(31) || ua_hash".to_string(),
        _ => "visitor_key".to_string(),
    }
}

/// The hash function for SQLite: 16 bytes, deterministic, random-looking.
///
/// SQLite has no hash of its own, so the example brings one. It is FNV-1a with
/// two seeds — the point is only that it is stable and does not look like the
/// text it came from.
fn register_hash(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    let flags = rusqlite::functions::FunctionFlags::SQLITE_UTF8
        | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC;
    // Sixteen bytes: two FNV-1a hashes with different seeds.
    conn.create_scalar_function(
        "sumimi_hash",
        1,
        flags,
        |ctx: &rusqlite::functions::Context<'_>| -> rusqlite::Result<Vec<u8>> {
            let text = ctx.get::<String>(0)?;
            let mut out = Vec::with_capacity(16);
            for seed in [0xcbf29ce484222325u64, 0x9e3779b97f4a7c15u64] {
                out.extend_from_slice(&fnv(&text, seed).to_le_bytes());
            }
            Ok(out)
        },
    )?;
    // Eight bytes: one FNV-1a hash.
    conn.create_scalar_function(
        "sumimi_hash8",
        1,
        flags,
        |ctx: &rusqlite::functions::Context<'_>| -> rusqlite::Result<Vec<u8>> {
            let text = ctx.get::<String>(0)?;
            Ok(fnv(&text, 0xcbf29ce484222325u64).to_le_bytes().to_vec())
        },
    )
}

/// The packed identity: the address and the agent's hash.
///
/// Four bytes of address and eight of the agent's hash for IPv4; eight of
/// address and four of the hash for IPv6 — twelve bytes either way.
fn register_identity(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.create_scalar_function(
        "sumimi_identity",
        2,
        rusqlite::functions::FunctionFlags::SQLITE_UTF8
            | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx: &rusqlite::functions::Context<'_>| -> rusqlite::Result<Vec<u8>> {
            let ip = ctx.get::<String>(0)?;
            let agent = ctx.get::<String>(1)?;
            let mut out = Vec::with_capacity(12);
            if ip.contains(':') {
                // IPv6: the first eight bytes of the address, then four of hash.
                let mut bytes = [0u8; 16];
                for (index, part) in ip.split(':').enumerate().take(8) {
                    bytes[index * 2..index * 2 + 2].copy_from_slice(
                        &(u16::from_str_radix(part, 16).unwrap_or(0)).to_be_bytes(),
                    );
                }
                out.extend_from_slice(&bytes[..8]);
                out.extend_from_slice(&fnv(&agent, 0x9e3779b97f4a7c15u64).to_be_bytes()[..4]);
            } else {
                // IPv4: four bytes of address, then eight of hash.
                let mut address = 0u32;
                for part in ip.split('.').take(4) {
                    address = (address << 8) | part.parse::<u32>().unwrap_or(0);
                }
                out.extend_from_slice(&address.to_be_bytes());
                out.extend_from_slice(&fnv(&agent, 0xcbf29ce484222325u64).to_le_bytes());
            }
            Ok(out)
        },
    )
}

/// The address as one number.
fn register_address(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.create_scalar_function(
        "sumimi_address",
        1,
        rusqlite::functions::FunctionFlags::SQLITE_UTF8
            | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx: &rusqlite::functions::Context<'_>| -> rusqlite::Result<i64> {
            let ip = ctx.get::<String>(0)?;
            let mut address = 0i64;
            for part in ip.split('.').take(4) {
                address = (address << 8) | part.parse::<i64>().unwrap_or(0);
            }
            Ok(address)
        },
    )
}

/// Four bytes of hash.
fn register_hash4(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.create_scalar_function(
        "sumimi_hash4",
        1,
        rusqlite::functions::FunctionFlags::SQLITE_UTF8
            | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx: &rusqlite::functions::Context<'_>| -> rusqlite::Result<i64> {
            let text = ctx.get::<String>(0)?;
            Ok(fnv(&text, 0x9e3779b97f4a7c15u64) as i32 as i64)
        },
    )
}

/// Sixteen bytes of hash — the width of a UUID.
fn register_hash16(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.create_scalar_function(
        "sumimi_hash16",
        1,
        rusqlite::functions::FunctionFlags::SQLITE_UTF8
            | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx: &rusqlite::functions::Context<'_>| -> rusqlite::Result<Vec<u8>> {
            let text = ctx.get::<String>(0)?;
            let mut out = Vec::with_capacity(16);
            for seed in [0xcbf29ce484222325u64, 0x9e3779b97f4a7c15u64] {
                out.extend_from_slice(&fnv(&text, seed).to_le_bytes());
            }
            Ok(out)
        },
    )
}

/// Sixteen bytes of hash, in the canonical text form (eight-four-four-four-twelve).
fn register_hash16_text(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.create_scalar_function(
        "sumimi_hash16_text",
        1,
        rusqlite::functions::FunctionFlags::SQLITE_UTF8
            | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx: &rusqlite::functions::Context<'_>| -> rusqlite::Result<String> {
            let text = ctx.get::<String>(0)?;
            let mut out = Vec::with_capacity(16);
            for seed in [0xcbf29ce484222325u64, 0x9e3779b97f4a7c15u64] {
                out.extend_from_slice(&fnv(&text, seed).to_le_bytes());
            }
            Ok(format!(
                "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
                out[0], out[1], out[2], out[3], out[4], out[5], out[6], out[7], out[8], out[9], out[10],
                out[11], out[12], out[13], out[14], out[15]
            ))
        },
    )
}

/// Thirty-two bytes of hash — the whole of a SHA-256.
fn register_hash32(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.create_scalar_function(
        "sumimi_hash32",
        1,
        rusqlite::functions::FunctionFlags::SQLITE_UTF8
            | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx: &rusqlite::functions::Context<'_>| -> rusqlite::Result<Vec<u8>> {
            let text = ctx.get::<String>(0)?;
            let mut out = Vec::with_capacity(32);
            for seed in [
                0xcbf29ce484222325u64,
                0x9e3779b97f4a7c15u64,
                0x84222325cbf29ce4u64,
                0x165667b19e3779f9u64,
            ] {
                out.extend_from_slice(&fnv(&text, seed).to_le_bytes());
            }
            Ok(out)
        },
    )
}

/// FNV-1a, with a seed.
fn fnv(text: &str, seed: u64) -> u64 {
    let mut hash = seed;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Base64, the way DuckDB's `base64` does it: padded, with `+` and `/`.
fn base64(bytes: &[u8], pad: bool) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(n >> 6 & 63) as usize] as char);
        } else if pad {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(n & 63) as usize] as char);
        } else if pad {
            out.push('=');
        }
    }
    out
}

/// Base64, padded — twelve characters for eight bytes.
fn register_b64(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.create_scalar_function(
        "sumimi_b64",
        1,
        rusqlite::functions::FunctionFlags::SQLITE_UTF8
            | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx: &rusqlite::functions::Context<'_>| -> rusqlite::Result<String> {
            let bytes = ctx.get::<Vec<u8>>(0)?;
            Ok(base64(&bytes, true))
        },
    )
}

/// Base64 without the padding — eleven characters for eight bytes.
fn register_b64np(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.create_scalar_function(
        "sumimi_b64np",
        1,
        rusqlite::functions::FunctionFlags::SQLITE_UTF8
            | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx: &rusqlite::functions::Context<'_>| -> rusqlite::Result<String> {
            let bytes = ctx.get::<Vec<u8>>(0)?;
            Ok(base64(&bytes, false))
        },
    )
}

fn main() -> anyhow::Result<()> {
    let rows: i64 = std::env::args()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000_000);
    let hits = sumimi_service::testing::traffic(rows);

    println!("{rows} rows, schema v{SCHEMA_VERSION}\n");

    for (engine, label) in [("duckdb", "DuckDB"), ("sqlite", "SQLite")] {
        println!("== {label} ==");
        println!(
            "  {:<28} {:>10} {:>12} {:>12} {:>12} {:>10}",
            "the identity", "B/row", "summary", "bounces", "browsers", "visitors"
        );

        for (name, design) in DESIGNS {
            let path = PathBuf::from(format!("/tmp/visitor-matrix-{engine}-{design}.db"));
            let _ = std::fs::remove_file(&path);

            // The source: the real table, the real traffic.
            let source = PathBuf::from(format!("/tmp/visitor-matrix-{engine}-source.db"));
            let _ = std::fs::remove_file(&source);
            if engine == "duckdb" {
                let db = Db::open(&source, "256MB")?;
                db.create_website("Example", Some("example.com"), 0, None)?;
                db.write_batch(&hits)?;
            } else {
                let db = SqliteBackend::open(&source)?;
                db.create_website("Example", Some("example.com"), 0, None)?;
                db.write_batch(&hits)?;
            }

            // The design's own file — written once, so the size is the size.
            let (summary_sql, bounces_sql, create_sql) = match engine {
                "duckdb" => {
                    let sql = match design {
                        "pair" => "CREATE TABLE v.event AS SELECT * FROM event;".to_string(),
                        "hash" => format!(
                            "CREATE TABLE v.event AS SELECT * EXCLUDE (ip_prefix, user_agent), \
                             {} AS visitor_id FROM event;",
                            uuid_of_pair()
                        ),
                        "hash8" => "CREATE TABLE v.event AS SELECT * EXCLUDE (ip_prefix, user_agent), \
                             hash(ip_prefix || chr(31) || user_agent) AS visitor_id FROM event;"
                            .to_string(),
                        "blob8" => "CREATE TABLE v.event AS SELECT * EXCLUDE (ip_prefix, user_agent), \
                             unhex(hex(hash(ip_prefix || chr(31) || user_agent))) AS visitor_id FROM event;"
                            .to_string(),
                        "hex8" => "CREATE TABLE v.event AS SELECT * EXCLUDE (ip_prefix, user_agent), \
                             lpad(hex(hash(ip_prefix || chr(31) || user_agent)), 16, '0') AS visitor_id FROM event;"
                            .to_string(),
                        // Base64 of the same eight bytes: twelve characters with
                        // the padding, eleven without. DuckDB's inline limit is
                        // twelve bytes, so the unpadded one still sits in the
                        // row while the hex one (sixteen characters) does not.
                        "b64" => "CREATE TABLE v.event AS SELECT * EXCLUDE (ip_prefix, user_agent), \
                             base64(unhex(lpad(hex(hash(ip_prefix || chr(31) || user_agent)), 16, '0'))) AS visitor_id FROM event;"
                            .to_string(),
                        "b64np" => "CREATE TABLE v.event AS SELECT * EXCLUDE (ip_prefix, user_agent), \
                             rtrim(base64(unhex(lpad(hex(hash(ip_prefix || chr(31) || user_agent)), 16, '0'))), '=') AS visitor_id FROM event;"
                            .to_string(),
                        // Sixteen bytes: the width of a UUID, the headroom the
                        // identity wants. Thirty-two: the whole of a SHA-256.
                        "blob16" => "CREATE TABLE v.event AS SELECT * EXCLUDE (ip_prefix, user_agent), \
                             unhex(substr(sha256(ip_prefix || chr(31) || user_agent), 1, 32)) AS visitor_id FROM event;"
                            .to_string(),
                        "blob32" => "CREATE TABLE v.event AS SELECT * EXCLUDE (ip_prefix, user_agent), \
                             unhex(sha256(ip_prefix || chr(31) || user_agent)) AS visitor_id FROM event;"
                            .to_string(),
                        // The same sixteen bytes, in the canonical text form —
                        // what SQLite has to store, since it has no uuid type.
                        "uuidtext" => format!(
                            "CREATE TABLE v.event AS SELECT * EXCLUDE (ip_prefix, user_agent), \
                             CAST({} AS VARCHAR) AS visitor_id FROM event;",
                            uuid_of_pair()
                        ),
                        "split8+4" => format!(
                            "CREATE TABLE v.event AS SELECT * EXCLUDE (ip_prefix, user_agent), \
                             CAST({} AS UBIGINT) AS ip_num, \
                             (hash(user_agent) % 4294967296)::BIGINT AS ua_hash FROM event;",
                            address_of()
                        ),
                        "split4+8" => format!(
                            "CREATE TABLE v.event AS SELECT * EXCLUDE (ip_prefix, user_agent), \
                             CAST({} AS UINTEGER) AS ip_num, \
                             hash(user_agent) AS ua_hash FROM event;",
                            address_of()
                        ),
                        "ids" => "CREATE TABLE v.networks AS SELECT ip_prefix, \
                             ROW_NUMBER() OVER (ORDER BY ip_prefix) AS id FROM (SELECT DISTINCT ip_prefix FROM event);\
                             CREATE TABLE v.agents AS SELECT user_agent, \
                             ROW_NUMBER() OVER (ORDER BY user_agent) AS id FROM (SELECT DISTINCT user_agent FROM event);\
                             CREATE TABLE v.event AS SELECT * EXCLUDE (ip_prefix, user_agent), \
                             n.id AS network_id, a.id AS agent_id FROM event \
                             JOIN v.networks n ON n.ip_prefix = event.ip_prefix \
                             JOIN v.agents a ON a.user_agent = event.user_agent;"
                            .to_string(),
                        "packed" => format!(
                            "CREATE TABLE v.event AS SELECT * EXCLUDE (ip_prefix, user_agent), \
                             unhex(lpad(hex({}), 8, '0') || hex(hash(user_agent))) AS visitor_id \
                             FROM event;",
                            address_of()
                        ),
                        _ => "CREATE TABLE v.event AS SELECT *, \
                              hash(ip_prefix || chr(31) || user_agent) AS visitor_key FROM event;"
                            .to_string(),
                    };
                    (
                        SUMMARY_DUCK.replace("{key}", &key_of(design, engine)),
                        BOUNCES_DUCK.to_string(),
                        sql,
                    )
                }
                _ => {
                    // SQLite has no `* EXCLUDE`: the column list is written out.
                    let conn = rusqlite::Connection::open(&source)?;
                    register_hash(&conn)?;
                    register_hash4(&conn)?;
                    register_address(&conn)?;
                    register_identity(&conn)?;
                    register_b64(&conn)?;
                    register_b64np(&conn)?;
                    register_hash16(&conn)?;
                    register_hash32(&conn)?;
                    register_hash16_text(&conn)?;
                    let mut columns: Vec<String> = Vec::new();
                    let mut stmt = conn.prepare("PRAGMA table_info(event)")?;
                    let mut info = stmt.query([])?;
                    while let Some(row) = info.next()? {
                        let name: String = row.get(1)?;
                        columns.push(name);
                    }
                    let list = columns.join(", ");
                    let without_pair = list.replace("ip_prefix, ", "").replace(", user_agent", "");
                    let list_without = without_pair
                        .split(", ")
                        .map(|name| format!("event.{name}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let sql = match design {
                        "pair" => format!("CREATE TABLE v.event AS SELECT {list} FROM event;"),
                        "hash" => format!(
                            "CREATE TABLE v.event AS SELECT {without_pair}, \
                             sumimi_hash(ip_prefix || char(31) || user_agent) AS visitor_id FROM event;"
                        ),
                        "hash8" => format!(
                            "CREATE TABLE v.event AS SELECT {without_pair}, \
                             sumimi_hash8(ip_prefix || char(31) || user_agent) AS visitor_id FROM event;"
                        ),
                        "packed" => format!(
                            "CREATE TABLE v.event AS SELECT {without_pair}, \
                             sumimi_identity(ip, user_agent) AS visitor_id FROM event;"
                        ),
                        "blob8" => format!(
                            "CREATE TABLE v.event AS SELECT {without_pair}, \
                             sumimi_hash8(ip_prefix || char(31) || user_agent) AS visitor_id FROM event;"
                        ),
                        "hex8" => format!(
                            "CREATE TABLE v.event AS SELECT {without_pair}, \
                             lower(hex(sumimi_hash8(ip_prefix || char(31) || user_agent))) AS visitor_id FROM event;"
                        ),
                        "b64" => format!(
                            "CREATE TABLE v.event AS SELECT {without_pair}, \
                             sumimi_b64(sumimi_hash8(ip_prefix || char(31) || user_agent)) AS visitor_id FROM event;"
                        ),
                        "b64np" => format!(
                            "CREATE TABLE v.event AS SELECT {without_pair}, \
                             sumimi_b64np(sumimi_hash8(ip_prefix || char(31) || user_agent)) AS visitor_id FROM event;"
                        ),
                        "blob16" => format!(
                            "CREATE TABLE v.event AS SELECT {without_pair}, \
                             sumimi_hash16(ip_prefix || char(31) || user_agent) AS visitor_id FROM event;"
                        ),
                        "blob32" => format!(
                            "CREATE TABLE v.event AS SELECT {without_pair}, \
                             sumimi_hash32(ip_prefix || char(31) || user_agent) AS visitor_id FROM event;"
                        ),
                        "uuidtext" => format!(
                            "CREATE TABLE v.event AS SELECT {without_pair}, \
                             sumimi_hash16_text(ip_prefix || char(31) || user_agent) AS visitor_id FROM event;"
                        ),
                        "split8+4" => format!(
                            "CREATE TABLE v.event AS SELECT {without_pair}, \
                             sumimi_address(ip) AS ip_num, \
                             sumimi_hash4(user_agent) AS ua_hash FROM event;"
                        ),
                        "split4+8" => format!(
                            "CREATE TABLE v.event AS SELECT {without_pair}, \
                             sumimi_address(ip) AS ip_num, \
                             sumimi_hash8(user_agent) AS ua_hash FROM event;"
                        ),
                        "ids" => format!(
                            "CREATE TABLE v.networks AS SELECT ip_prefix, \
                             ROW_NUMBER() OVER (ORDER BY ip_prefix) AS id FROM (SELECT DISTINCT ip_prefix FROM event);\
                             CREATE TABLE v.agents AS SELECT user_agent, \
                             ROW_NUMBER() OVER (ORDER BY user_agent) AS id FROM (SELECT DISTINCT user_agent FROM event);\
                             CREATE TABLE v.event AS SELECT {list_without}, n.id AS network_id, a.id AS agent_id \
                             FROM event JOIN v.networks n ON n.ip_prefix = event.ip_prefix \
                             JOIN v.agents a ON a.user_agent = event.user_agent;"
                        ),
                        _ => format!(
                            "CREATE TABLE v.event AS SELECT {list}, \
                             sumimi_hash(ip_prefix || char(31) || user_agent) AS visitor_key FROM event;"
                        ),
                    };
                    (
                        SUMMARY_SQLITE.replace("{key}", &key_of(design, engine)),
                        BOUNCES_SQLITE.to_string(),
                        sql,
                    )
                }
            };

            if engine == "duckdb" {
                let db = Db::open(&source, "256MB")?;
                db.with_read(|conn| {
                    conn.execute_batch(&format!("ATTACH '{}' AS v;", path.display()))?;
                    conn.execute_batch(&create_sql)?;
                    conn.execute_batch("DETACH v;")?;
                    Ok::<_, anyhow::Error>(())
                })?;
            } else {
                let conn = rusqlite::Connection::open(&source)?;
                register_hash(&conn)?;
                register_hash4(&conn)?;
                register_address(&conn)?;
                register_identity(&conn)?;
                register_b64(&conn)?;
                register_b64np(&conn)?;
                register_hash16(&conn)?;
                register_hash32(&conn)?;
                register_hash16_text(&conn)?;
                conn.execute_batch(&format!("ATTACH '{}' AS v;", path.display()))?;
                conn.execute_batch(&create_sql)?;
                conn.execute_batch(
                    "CREATE INDEX v.event_website_created ON event (website_id, created_at);",
                )?;
                conn.execute_batch("DETACH v;")?;
            }

            let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            let breakdown_sql = BREAKDOWN.replace("{key}", &key_of(design, engine));

            // The three queries, on the design's own file.
            let mut summary_ms = f64::MAX;
            let mut bounces_ms = f64::MAX;
            let mut browsers_ms = f64::MAX;
            let mut visitors = 0i64;
            for _ in 0..3 {
                if engine == "duckdb" {
                    let file = Db::open(&path, "256MB")?;
                    let started = Instant::now();
                    file.with_read(|conn| {
                        let _: i64 = conn.query_row(&summary_sql, [], |r| r.get(0))?;
                        Ok::<_, anyhow::Error>(())
                    })?;
                    summary_ms = summary_ms.min(started.elapsed().as_secs_f64() * 1000.0);

                    let started = Instant::now();
                    file.with_read(|conn| {
                        let _: i64 = conn.query_row(&bounces_sql, [], |r| r.get(0))?;
                        Ok::<_, anyhow::Error>(())
                    })?;
                    bounces_ms = bounces_ms.min(started.elapsed().as_secs_f64() * 1000.0);

                    let started = Instant::now();
                    file.with_read(|conn| {
                        let mut stmt = conn.prepare(&breakdown_sql)?;
                        let mut rows = stmt.query([])?;
                        while rows.next()?.is_some() {}
                        Ok::<_, anyhow::Error>(())
                    })?;
                    browsers_ms = browsers_ms.min(started.elapsed().as_secs_f64() * 1000.0);

                    visitors = file.with_read(|conn| {
                        let value: i64 = conn.query_row(&summary_sql, [], |r| r.get(1))?;
                        Ok::<_, anyhow::Error>(value)
                    })?;
                } else {
                    let file = rusqlite::Connection::open(&path)?;
                    register_hash(&file)?;
                    register_hash4(&file)?;
                    register_address(&file)?;
                    register_identity(&file)?;
                    register_b64(&file)?;
                    register_b64np(&file)?;
                    register_hash16(&file)?;
                    register_hash32(&file)?;
                    register_hash16_text(&file)?;
                    let started = Instant::now();
                    let _: i64 = file.query_row(&summary_sql, [], |r| r.get(0))?;
                    summary_ms = summary_ms.min(started.elapsed().as_secs_f64() * 1000.0);

                    let started = Instant::now();
                    let _: i64 = file.query_row(&bounces_sql, [], |r| r.get(0))?;
                    bounces_ms = bounces_ms.min(started.elapsed().as_secs_f64() * 1000.0);

                    let started = Instant::now();
                    {
                        let mut stmt = file.prepare(&breakdown_sql)?;
                        let mut rows = stmt.query([])?;
                        while rows.next()?.is_some() {}
                    }
                    browsers_ms = browsers_ms.min(started.elapsed().as_secs_f64() * 1000.0);

                    visitors = file.query_row(&summary_sql, [], |r| r.get(1))?;
                }
            }

            println!(
                "  {:<28} {:>10.1} {:>11.1}ms {:>11.1}ms {:>11.1}ms {:>10}",
                name,
                bytes as f64 / rows as f64,
                summary_ms,
                bounces_ms,
                browsers_ms,
                visitors
            );
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(&source);
        }
        println!();
    }
    Ok(())
}
