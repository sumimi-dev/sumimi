//! The DDL of every engine, from the one list of columns.
//!
//! The measurement across the engines needs the **same** tables in each of
//! them — the same columns, the same names, the same order — and the only
//! thing that may differ is how the engine spells them. That is what this
//! prints: the `CREATE TABLE` of the fact table, the property table and the
//! revenue table, for every engine.
//!
//! ```text
//! cargo run --example engine_ddl
//! ```
//!
//! The ClickHouse spelling is here rather than in `schema.rs` because
//! ClickHouse is not an engine this product runs on: it is one the numbers
//! are compared against. **Its byte string is a `String`** — ClickHouse has
//! no blob, and the visitor's id goes in as its hex, which is why that
//! column is wider there than here.

use sumimi_service::schema::{self, Column, Types};

/// How ClickHouse spells the kinds.
struct ClickHouse;

impl Types for ClickHouse {
    fn row_id(&self) -> String {
        "UInt64".to_string()
    }
    fn text(&self) -> String {
        "String".to_string()
    }
    fn uuid(&self) -> String {
        "UUID".to_string()
    }
    fn blob(&self) -> String {
        "String".to_string()
    }
    fn event_id(&self) -> String {
        "UUID".to_string()
    }
    fn int(&self) -> String {
        "Int64".to_string()
    }
    fn small(&self) -> String {
        "UInt16".to_string()
    }
    fn real(&self) -> String {
        "Float64".to_string()
    }
    fn time(&self) -> String {
        "DateTime64(3)".to_string()
    }
}

/// How SQLite spells the kinds.
///
/// **The same as `service/src/sqlite.rs`'s** — it is spelled here because that
/// one is private to the module; keep the two in step.
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
    fn blob(&self) -> String {
        "BLOB".to_string()
    }
    fn event_id(&self) -> String {
        "BLOB".to_string()
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

/// The tables of the fact table's file, as one engine spells them.
fn tables(types: &dyn Types) -> String {
    let mut out = String::new();
    for (name, columns) in [
        ("website_event", schema::EVENT),
        ("event_data", schema::EVENT_DATA),
        ("revenue", schema::REVENUE),
    ] {
        out.push_str(&format!(
            "CREATE TABLE {name} (\n  {}\n);\n\n",
            schema::columns_ddl(columns, types)
        ));
    }
    out
}

/// The names of the columns, in order — for the measurement's inserts.
fn names(columns: &[Column]) -> String {
    schema::names(columns).join(", ")
}

fn main() {
    println!("=== DuckDB ===\n{}", tables(&sumimi_service::db::Duck));
    println!("=== SQLite ===\n{}", tables(&Sqlite));
    println!("=== ClickHouse ===\n{}", tables(&ClickHouse));
    println!("=== the columns, in order ===");
    println!("website_event: {}", names(schema::EVENT));
    println!("event_data: {}", names(schema::EVENT_DATA));
    println!("revenue: {}", names(schema::REVENUE));
}
