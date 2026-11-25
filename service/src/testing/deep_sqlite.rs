//! The SQLite backend's deep read.
//!
//! The backend itself lives in the open-source crate — there is one
//! implementation of it. This is the private half: the field-level check that
//! the suite runs.
//!
//! **The statement and the mapping are shared** (see `crate::query`
//! and `crate::row`): the backend only says how a column of its own
//! is fetched.

use crate::sqlite::{SqliteBackend, SqliteDialect};

use super::deep::{DeepRead, StoredRow};

impl DeepRead for SqliteBackend {
    /// Every row of a website, in insertion order.
    fn rows(&self, website_id: &str) -> anyhow::Result<Vec<StoredRow>> {
        self.with_connection(|conn| {
            let statement =
                crate::query::event_rows(website_id, &SqliteDialect::new(self.layout()));
            let mut stmt = conn.prepare(&statement.sql)?;
            let mut rows = stmt.query(rusqlite::params![website_id])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                let id: String = row.get(0)?;
                let hit = crate::row::hit_row(&crate::sqlite::SqliteRow(row))?;
                out.push(StoredRow { id, hit });
            }
            Ok(out)
        })
    }
}
