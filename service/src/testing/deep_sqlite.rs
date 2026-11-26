//! The SQLite backend's deep read.
//!
//! The backend itself lives in the open-source crate — there is one
//! implementation of it. This is the private half: the field-level check that
//! the suite runs.
//!
//! **The statement and the mapping are shared** (see `crate::query`
//! and `crate::row`): the backend only says how a column of its own
//! is fetched.

use std::collections::HashMap;

use crate::sqlite::{SqliteBackend, SqliteDialect};

use super::deep::{DeepRead, StoredRow};

impl DeepRead for SqliteBackend {
    /// Every row of a website, in insertion order.
    fn rows(&self, website_id: &str) -> anyhow::Result<Vec<StoredRow>> {
        self.with_connection(|conn| {
            let dialect = SqliteDialect::new(self.layout());

            // The properties are rows of the table of their own ⇒ ⇒ they are
            // read for the whole website first, and every event is given the
            // ones that belong to it.
            let properties = read_properties(conn, website_id, &dialect)?;

            let statement = crate::query::event_rows(website_id, &dialect);
            let mut stmt = conn.prepare(&statement.sql)?;
            let mut rows = stmt.query(rusqlite::params![website_id])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                let id: String = row.get(0)?;
                let event_id = uuid::Uuid::parse_str(&id).unwrap_or_else(|_| uuid::Uuid::nil());
                let owned = properties.get(&event_id).cloned().unwrap_or_default();
                let hit = crate::row::hit_row(&crate::sqlite::SqliteRow(row), owned)?;
                out.push(StoredRow { id, hit });
            }
            Ok(out)
        })
    }
}

/// Every property row of a website, grouped by the event it belongs to.
fn read_properties(
    conn: &rusqlite::Connection,
    website_id: &str,
    dialect: &SqliteDialect,
) -> anyhow::Result<HashMap<uuid::Uuid, Vec<crate::models::Property>>> {
    let statement = crate::query::site_properties(website_id, dialect);
    let mut stmt = conn.prepare(&statement.sql)?;
    let mut rows = stmt.query(rusqlite::params![website_id])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(crate::row::property_row(&crate::sqlite::SqliteRow(row))?);
    }
    Ok(crate::row::group_properties(out))
}
