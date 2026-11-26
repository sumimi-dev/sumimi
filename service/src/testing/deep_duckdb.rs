//! The DuckDB backend's deep read.
//!
//! The open-source backend, read row by row — the same check the pro backends
//! get, so "no data loss" means the same thing everywhere.
//!
//! **The statement and the mapping are shared** (see `crate::query`
//! and `crate::row`): the backend only says how a column of its own
//! is fetched.

use std::collections::HashMap;

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
    // The properties are rows of the table of their own ⇒ ⇒ they are read for
    // the whole website first, and every event is given the ones that belong
    // to it.
    let properties = read_properties(conn, website_id, layout)?;

    let statement = crate::query::event_rows(website_id, &Duck::new(layout));
    let mut stmt = conn.prepare(&statement.sql)?;
    let mut rows = stmt.query([website_id])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let event_id = uuid::Uuid::parse_str(&id).unwrap_or_else(|_| uuid::Uuid::nil());
        let owned = properties.get(&event_id).cloned().unwrap_or_default();
        let hit = crate::row::hit_row(&DuckRow(row), owned)?;
        out.push(StoredRow { id, hit });
    }
    Ok(out)
}

/// Every property row of a website, grouped by the event it belongs to.
fn read_properties(
    conn: &Connection,
    website_id: &str,
    layout: crate::storage::Layout,
) -> anyhow::Result<HashMap<uuid::Uuid, Vec<crate::models::Property>>> {
    let statement = crate::query::site_properties(website_id, &Duck::new(layout));
    let mut stmt = conn.prepare(&statement.sql)?;
    let mut rows = stmt.query([website_id])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(crate::row::property_row(&DuckRow(row))?);
    }
    Ok(crate::row::group_properties(out))
}
