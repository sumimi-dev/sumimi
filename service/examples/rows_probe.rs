//! The rows a server wrote, measured: the properties the traffic must have.
//!
//! The traffic that reaches a server comes from outside — a browser, a
//! tracker, a load generator — so what the server writes is what the *real*
//! collect path does, not what a fixture in this repository says. This probe
//! opens the database a server wrote and prints the properties the traffic
//! has:
//!
//! 1. **one start per visit** — the id and the start are minted together;
//! 2. **the visit ids follow the clock** — the counter grows as the requests
//!    arrive, so the ids are handed out in the order the visits happen;
//! 3. **the rows of a visit are in the order they happened** — the row id is
//!    the insertion order, and the insertion order is the arrival order;
//! 4. **the properties have one shape** — the same keys, the same kinds.
//!
//! ```bash
//! cargo run --release --example rows_probe -p sumimi-service -- ./sumimi.duckdb
//! ```

use std::path::PathBuf;

use sumimi_service::pool::Db;

fn main() -> anyhow::Result<()> {
    let path: PathBuf = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "./sumimi.duckdb".to_string())
        .into();
    let db = Db::open(&path, "256MB")?;

    // One number, as text. Every statement below answers with a single value.
    let one = |sql: &str| -> String {
        db.with_read(|conn| {
            let value: String = conn.query_row(sql, [], |row| row.get(0))?;
            Ok::<_, anyhow::Error>(value)
        })
        .unwrap_or_else(|err| format!("ERR {err}"))
    };

    // A question of its own: `rows_probe <file> --sql "SELECT ..."`. The
    // `{events}` and `{data}` placeholders are the site's own tables.
    if let Some(position) = std::env::args().position(|arg| arg == "--sql") {
        let sql = std::env::args().nth(position + 1).unwrap_or_default();
        let rows = db.with_read(|conn| {
            let mut statement = conn.prepare(&sql)?;
            let columns = statement.column_count();
            let mut result = statement.query([])?;
            let mut out = String::new();
            while let Some(row) = result.next()? {
                let mut cells: Vec<String> = Vec::new();
                for index in 0..columns {
                    let cell: String = row.get(index)?;
                    cells.push(cell);
                }
                out.push_str(&cells.join(" · "));
                out.push('\n');
            }
            Ok::<_, anyhow::Error>(out)
        })?;
        print!("{rows}");
        return Ok(());
    }

    // The tables live in a file per website, attached under the site's name.
    let schemas: Vec<String> = db.with_read(|conn| {
        let mut rows = conn.prepare(
            "SELECT database_name FROM duckdb_databases() \
             WHERE starts_with(database_name, 's_') ORDER BY database_name",
        )?;
        let mut out = Vec::new();
        let mut result = rows.query([])?;
        while let Some(row) = result.next()? {
            out.push(row.get::<usize, String>(0)?);
        }
        Ok::<_, anyhow::Error>(out)
    })?;
    if schemas.is_empty() {
        println!("{}: no rows", path.display());
        return Ok(());
    }

    for schema in &schemas {
        let events = format!("\"{schema}\".website_event");
        let data = format!("\"{schema}\".event_data");
        let rows = one(&format!("SELECT CAST(count(*) AS VARCHAR) FROM {events}"));
        if rows == "0" {
            continue;
        }

        // The counter of an id: the last twelve hex characters of its text form.
        let counter = "substring(CAST(visit_id AS VARCHAR), 25, 12)";

        println!("the file: {} · the schema {schema} · {rows} rows\n", path.display());

        println!("the traffic");
        println!(
            "   the visits: {}",
            one(&format!(
                "SELECT CAST(count(DISTINCT visit_id) AS VARCHAR) FROM {events}"
            ))
        );
        println!(
            "   the visitors: {}",
            one(&format!(
                "SELECT CAST(count(DISTINCT session_id) AS VARCHAR) FROM {events}"
            ))
        );

        println!("\n1. one start per visit");
        println!(
            "   the visits with more than one start: {}",
            one(&format!(
                "SELECT CAST(count(*) AS VARCHAR) FROM (SELECT visit_id, \
                 count(DISTINCT session_start) n FROM {events} GROUP BY visit_id HAVING n > 1)"
            ))
        );

        println!("\n2. the visit ids follow the clock");
        println!(
            "   a visit that starts earlier than the one before it: {}",
            one(&format!(
                "SELECT CAST(count(*) AS VARCHAR) FROM (SELECT start, lag(start) OVER (ORDER BY c) p \
                 FROM (SELECT {counter} AS c, min(session_start) AS start FROM {events} \
                 WHERE session_start IS NOT NULL GROUP BY visit_id)) WHERE start < p"
            ))
        );

        println!("\n3. the rows of a visit are in the order they happened");
        println!(
            "   the rows whose time goes backwards: {}",
            one(&format!(
                "SELECT CAST(count(*) AS VARCHAR) FROM (SELECT visit_id, created_at, \
                 lag(created_at) OVER (PARTITION BY visit_id \
                 ORDER BY substring(CAST(event_id AS VARCHAR), 25, 12)) p FROM {events}) \
                 WHERE created_at < p"
            ))
        );

        println!("\n3b. the rows that go backwards, and the starts of one visit");
        println!(
            "   {}",
            one(&format!(
                "SELECT string_agg(CAST(visit_id AS VARCHAR) || ' @ ' || CAST(created_at AS VARCHAR) || ' / ' || \
                 CAST(session_start AS VARCHAR), ' | ') FROM (SELECT visit_id, created_at, session_start, \
                 lag(created_at) OVER (PARTITION BY visit_id \
                 ORDER BY substring(CAST(event_id AS VARCHAR), 25, 12)) p FROM {events}) WHERE created_at < p"
            ))
        );
        println!(
            "   the last rows: {}",
            one(&format!(
                "SELECT string_agg(CAST(substring(CAST(event_id AS VARCHAR), 25, 12) AS VARCHAR) || ' @ ' || \
                 CAST(created_at AS VARCHAR), ' | ') FROM (SELECT event_id, created_at FROM {events} \
                 ORDER BY substring(CAST(event_id AS VARCHAR), 25, 12) DESC LIMIT 6)"
            ))
        );
        println!(
            "   the starts of the visits: {}",
            one(&format!(
                "SELECT CAST(count(*) AS VARCHAR) FROM (SELECT visit_id, count(DISTINCT session_start) n \
                 FROM {events} GROUP BY visit_id HAVING n > 1)"
            ))
        );

        println!("\n4. the properties have one shape");
        println!(
            "   the kinds: {}",
            one(&format!(
                "SELECT string_agg(DISTINCT data_type, ', ') FROM {data}"
            ))
        );
        println!();
    }

    Ok(())
}
