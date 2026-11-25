//! The DuckDB backend's deep read.
//!
//! The open-source backend, read row by row — the same check the pro backends
//! get, so "no data loss" means the same thing everywhere.
//!
//! **The statement and the mapping are shared** (see `crate::query`
//! and `crate::row`): the backend only says how a column of its own
//! is fetched.

use duckdb::Connection;
use crate::pool::Db;
use crate::reports::{Duck, DuckRow};

use super::deep::{DeepRead, StoredRow};

impl DeepRead for Db {
    fn rows(&self, website_id: &str) -> anyhow::Result<Vec<StoredRow>> {
        self.with_connection(|conn| read_rows(conn, website_id, self.layout()))
    }
}

/// Read every row of a website, in insertion order.
pub fn read_rows(
    conn: &Connection,
    website_id: &str,
    layout: crate::storage::Layout,
) -> anyhow::Result<Vec<StoredRow>> {
    let statement = crate::query::event_rows(website_id, &Duck::new(layout));
    let mut stmt = conn.prepare(&statement.sql)?;
    let mut rows = stmt.query([website_id])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let hit = crate::row::hit_row(&DuckRow(row))?;
        out.push(StoredRow { id, hit });
    }
    Ok(out)
}
