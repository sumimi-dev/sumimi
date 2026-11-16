//! The schema — **one** list, three spellings.
//!
//! The columns of the tables are written down **once**, here, as what they
//! *hold*; each engine spells them in its own dialect ([`Types`]). The reason
//! is not tidiness: the columns **are** the 口径 — the shape of the data every
//! report reads — and a column that one engine has and the other does not is
//! a report that answers a different question on each of them.
//!
//! That is not a theory. When v10 added the visitor id and `session_start`, the
//! two engines of this repository were changed by hand and the third
//! (PostgreSQL, in the private repository) was missed: its reports were still
//! counting the visitors of v9 and were still anchoring the session's length
//! on the eight-byte id of v8 — SQL that does not even run against a UUID.
//!
//! ⇒ ⇒ So the list is the one place a column is added, and every engine's DDL
//! is **generated** from it. A column that exists in one engine exists in all
//! of them, and the column that a report reads is the column the report's
//! author wrote down.
//!
//! **The row id is the engine's own** ([`Kind::RowId`]): DuckDB counts the
//! rows in memory, SQLite uses the rowid, PostgreSQL uses a sequence. That is
//! an engine's business, not the application's — so the application does not
//! name it, and the engine spells it.

use std::fmt::Write as _;

/// What a column holds.
///
/// The kinds are **logical**: how an engine spells them is the engine's
/// business (see [`Types`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The engine's own row id — it counts the rows.
    RowId,
    /// Text.
    Text,
    /// A sixteen-byte id.
    Uuid,
    /// A whole number.
    Int,
    /// A small whole number — a count of days, a flag.
    Small,
    /// A number with a fraction.
    Real,
    /// A moment in time — microseconds since the epoch.
    Time,
}

/// One column of a table.
#[derive(Debug, Clone, Copy)]
pub struct Column {
    /// The column's name — the same in every engine.
    pub name: &'static str,
    /// What it holds.
    pub kind: Kind,
    /// May it be null?
    pub nullable: bool,
    /// Is it the table's key?
    pub key: bool,
    /// The default value, spelled as SQL.
    pub default: Option<&'static str>,
}

impl Column {
    /// A column that may be null.
    pub const fn new(name: &'static str, kind: Kind) -> Self {
        Self {
            name,
            kind,
            nullable: true,
            key: false,
            default: None,
        }
    }

    /// A column that may not be null.
    pub const fn required(name: &'static str, kind: Kind) -> Self {
        Self {
            name,
            kind,
            nullable: false,
            key: false,
            default: None,
        }
    }

    /// A column that may not be null and has a default.
    pub const fn with_default(
        name: &'static str,
        kind: Kind,
        default: &'static str,
    ) -> Self {
        Self {
            name,
            kind,
            nullable: false,
            key: false,
            default: Some(default),
        }
    }

    /// The column is the table's key.
    pub const fn as_key(mut self) -> Self {
        self.key = true;
        self
    }
}

/// How an engine spells the kinds.
///
/// Every engine answers the same questions; only the spelling differs.
pub trait Types {
    /// The engine's own row id, with everything that goes with it
    /// (the counter, the key, the `NOT NULL`).
    fn row_id(&self) -> String;

    /// Text.
    fn text(&self) -> String;

    /// A sixteen-byte id.
    fn uuid(&self) -> String;

    /// A whole number.
    fn int(&self) -> String;

    /// A small whole number.
    fn small(&self) -> String;

    /// A number with a fraction.
    fn real(&self) -> String;

    /// A moment in time.
    fn time(&self) -> String;
}

/// One column, in the engine's spelling.
pub fn column_ddl(column: &Column, types: &dyn Types) -> String {
    // The row id is the engine's own: it says everything about itself.
    if column.kind == Kind::RowId {
        return format!("{} {}", column.name, types.row_id());
    }

    let mut out = String::new();
    let _ = write!(out, "{} ", column.name);
    out.push_str(&match column.kind {
        Kind::RowId => unreachable!("the row id is handled above"),
        Kind::Text => types.text(),
        Kind::Uuid => types.uuid(),
        Kind::Int => types.int(),
        Kind::Small => types.small(),
        Kind::Real => types.real(),
        Kind::Time => types.time(),
    });

    if column.key {
        // A key is never null — that is what a key means.
        out.push_str(" PRIMARY KEY");
    } else {
        if !column.nullable {
            out.push_str(" NOT NULL");
        }
        if let Some(default) = column.default {
            let _ = write!(out, " DEFAULT {default}");
        }
    }

    out
}

/// The columns of a table, in the engine's spelling.
pub fn columns_ddl(columns: &[Column], types: &dyn Types) -> String {
    columns_ddl_with(columns, types, &[])
}

/// The columns of a table, in the engine's spelling, with **some of them
/// spelled differently**.
///
/// The measurement examples want the real schema with one column changed — the
/// identity's width, a primary key — and a string they can cut up cannot say
/// that: the cut stops matching the day the schema changes, and the example
/// then measures something else without saying so.
///
/// The override is the **whole** declaration of the column, so the engine's
/// own spelling of the rest of it does not get in the way.
pub fn columns_ddl_with(
    columns: &[Column],
    types: &dyn Types,
    overrides: &[(&str, &str)],
) -> String {
    let parts: Vec<String> = columns
        .iter()
        .map(|column| {
            match overrides.iter().find(|(name, _)| *name == column.name) {
                Some((_, declaration)) => (*declaration).to_string(),
                None => column_ddl(column, types),
            }
        })
        .collect();
    parts.join(", ")
}

/// The `CREATE TABLE` of one table.
pub fn table_ddl(table: &str, columns: &[Column], types: &dyn Types) -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {table} ({});",
        columns_ddl(columns, types)
    )
}

/// The names of the columns, in order.
pub fn names(columns: &[Column]) -> Vec<&'static str> {
    columns.iter().map(|column| column.name).collect()
}

/// The columns of the `website_event` table — **the** list.
///
/// One row per page view (or custom event). The field set mirrors Umami's
/// `website_event` row for row. The row id is the engine's own; the two ids
/// are sixteen bytes (a UUID); the session's start is a fact, stored beside
/// the id.
///
/// **The names are Umami's** (since v12): the table is `website_event`, the
/// row id is `event_id`, the page is `url_path`, its title is `page_title`,
/// the referrer's host is `referrer_domain`, and the click id is `li_fat_id`.
/// ⇒ ⇒ A row that came from Umami sits in this table under the same names
/// ⇒ ⇒ ⇒ and the two tables can be read as one.
///
/// **The two ids are named the way Umami names them.** Umami's `session_id`
/// is the identity that spans many visits (their "session" is our visitor),
/// and their `visit_id` is one stretch of that identity's activity. Ours are
/// the same two things — the identity is our **visitor**, the stretch is our
/// **session** — so the columns are `session_id` and `visit_id`, and the two
/// tables can be read as one.
///
/// **What an id is made of is not in the table.** A column of UUIDs holds the
/// id whoever made it: ours are made by our rule (see [`crate::ua`]), and a
/// row that came from Umami carries the id Umami's rule made (see
/// [`crate::session`]). That is why the two can share the column — and why
/// the numbers of two datasets are only comparable within one of them.
pub const EVENT: &[Column] = &[
    Column::required("event_id", Kind::RowId),
    Column::required("website_id", Kind::Text),
    Column::required("visit_id", Kind::Uuid),
    Column::required("session_id", Kind::Uuid),
    Column::new("session_start", Kind::Time),
    Column::with_default("ip", Kind::Text, "''"),
    Column::with_default("ip_prefix", Kind::Text, "''"),
    Column::with_default("user_agent", Kind::Text, "''"),
    Column::new("user_id", Kind::Uuid),
    Column::required("created_at", Kind::Time),
    Column::new("url_path", Kind::Text),
    Column::new("url_query", Kind::Text),
    Column::new("page_title", Kind::Text),
    Column::new("referrer_domain", Kind::Text),
    Column::new("referrer_path", Kind::Text),
    Column::new("referrer_query", Kind::Text),
    Column::new("referrer_name", Kind::Text),
    Column::new("hostname", Kind::Text),
    Column::new("channel", Kind::Text),
    Column::new("browser", Kind::Text),
    Column::new("browser_version", Kind::Text),
    Column::new("os", Kind::Text),
    Column::new("os_version", Kind::Text),
    Column::new("device", Kind::Text),
    Column::new("screen", Kind::Text),
    Column::new("language", Kind::Text),
    Column::new("country", Kind::Text),
    Column::new("region", Kind::Text),
    Column::new("city", Kind::Text),
    Column::new("latitude", Kind::Real),
    Column::new("longitude", Kind::Real),
    Column::new("utm_source", Kind::Text),
    Column::new("utm_medium", Kind::Text),
    Column::new("utm_campaign", Kind::Text),
    Column::new("utm_content", Kind::Text),
    Column::new("utm_term", Kind::Text),
    Column::new("gclid", Kind::Text),
    Column::new("fbclid", Kind::Text),
    Column::new("msclkid", Kind::Text),
    Column::new("ttclid", Kind::Text),
    Column::new("li_fat_id", Kind::Text),
    Column::new("twclid", Kind::Text),
    Column::new("lcp", Kind::Real),
    Column::new("inp", Kind::Real),
    Column::new("cls", Kind::Real),
    Column::new("fcp", Kind::Real),
    Column::new("ttfb", Kind::Real),
    Column::new("event_type", Kind::Text),
    Column::new("event_name", Kind::Text),
    Column::new("tag", Kind::Text),
    Column::new("data", Kind::Text),
];

/// The columns of the `revenue` table.
///
/// The revenue is written by the site owner (imported from Carbon Ads,
/// AdSense, …), not by the tracker — so it is a table of its own.
///
/// **The row id stays `id`** — the `event_id` here is the **event's** id (the
/// one the revenue belongs to), not this row's.
pub const REVENUE: &[Column] = &[
    Column::required("id", Kind::RowId),
    Column::required("website_id", Kind::Text),
    Column::new("visit_id", Kind::Uuid),
    Column::new("event_id", Kind::Int),
    Column::new("event_name", Kind::Text),
    Column::with_default("currency", Kind::Text, "'USD'"),
    Column::required("revenue", Kind::Real),
    Column::required("created_at", Kind::Time),
];

/// The columns of the `website` table.
pub const WEBSITE: &[Column] = &[
    Column::required("id", Kind::Text).as_key(),
    Column::required("name", Kind::Text),
    Column::new("domain", Kind::Text),
    Column::required("created_at", Kind::Time),
    Column::required("updated_at", Kind::Time),
    Column::with_default("retention_days", Kind::Small, "0"),
    Column::new("country_header", Kind::Text),
];

/// The columns of the `meta` table.
pub const META: &[Column] = &[
    Column::required("key", Kind::Text).as_key(),
    Column::new("value", Kind::Text),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The DuckDB spelling.
    struct Duck;

    impl Types for Duck {
        fn row_id(&self) -> String {
            "BIGINT NOT NULL".to_string()
        }
        fn text(&self) -> String {
            "VARCHAR".to_string()
        }
        fn uuid(&self) -> String {
            "UUID".to_string()
        }
        fn int(&self) -> String {
            "BIGINT".to_string()
        }
        fn small(&self) -> String {
            "INTEGER".to_string()
        }
        fn real(&self) -> String {
            "DOUBLE".to_string()
        }
        fn time(&self) -> String {
            "TIMESTAMP".to_string()
        }
    }

    /// The SQLite spelling.
    struct Sqlite;

    impl Types for Sqlite {
        fn row_id(&self) -> String {
            "INTEGER PRIMARY KEY".to_string()
        }
        fn text(&self) -> String {
            "TEXT".to_string()
        }
        fn uuid(&self) -> String {
            "TEXT".to_string()
        }
        fn int(&self) -> String {
            "INTEGER".to_string()
        }
        fn small(&self) -> String {
            "INTEGER".to_string()
        }
        fn real(&self) -> String {
            "REAL".to_string()
        }
        fn time(&self) -> String {
            "INTEGER".to_string()
        }
    }

    /// The PostgreSQL spelling.
    struct Pg;

    impl Types for Pg {
        fn row_id(&self) -> String {
            "BIGSERIAL NOT NULL".to_string()
        }
        fn text(&self) -> String {
            "TEXT".to_string()
        }
        fn uuid(&self) -> String {
            "UUID".to_string()
        }
        fn int(&self) -> String {
            "BIGINT".to_string()
        }
        fn small(&self) -> String {
            "INTEGER".to_string()
        }
        fn real(&self) -> String {
            "DOUBLE PRECISION".to_string()
        }
        fn time(&self) -> String {
            "BIGINT".to_string()
        }
    }

    #[test]
    fn the_row_id_is_the_engines_own() {
        assert_eq!(column_ddl(&EVENT[0], &Duck), "event_id BIGINT NOT NULL");
        assert_eq!(
            column_ddl(&EVENT[0], &Sqlite),
            "event_id INTEGER PRIMARY KEY"
        );
        assert_eq!(column_ddl(&EVENT[0], &Pg), "event_id BIGSERIAL NOT NULL");
    }

    #[test]
    fn a_key_is_never_null() {
        assert_eq!(
            column_ddl(&WEBSITE[0], &Duck),
            "id VARCHAR PRIMARY KEY"
        );
        assert_eq!(
            column_ddl(&WEBSITE[0], &Sqlite),
            "id TEXT PRIMARY KEY"
        );
        assert_eq!(column_ddl(&WEBSITE[0], &Pg), "id TEXT PRIMARY KEY");
    }

    #[test]
    fn a_default_comes_after_not_null() {
        assert_eq!(
            column_ddl(&EVENT[5], &Duck),
            "ip VARCHAR NOT NULL DEFAULT ''"
        );
        assert_eq!(
            column_ddl(&EVENT[5], &Sqlite),
            "ip TEXT NOT NULL DEFAULT ''"
        );
        assert_eq!(column_ddl(&EVENT[5], &Pg), "ip TEXT NOT NULL DEFAULT ''");
    }

    #[test]
    fn the_three_engines_agree_on_the_columns() {
        // The same columns, in the same order — only the spelling differs.
        let duck = columns_ddl(EVENT, &Duck);
        let sqlite = columns_ddl(EVENT, &Sqlite);
        let pg = columns_ddl(EVENT, &Pg);

        let names_of = |ddl: &str| {
            ddl.split(", ")
                .map(|part| part.split(' ').next().unwrap().to_string())
                .collect::<Vec<String>>()
        };
        assert_eq!(names_of(&duck), names_of(&sqlite));
        assert_eq!(names_of(&duck), names_of(&pg));
        assert_eq!(names_of(&duck), names(EVENT));
    }
}
