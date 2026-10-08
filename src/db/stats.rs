//! What each catalog table weighs (issue #310, ADR-0012 amendment
//! 2026-10-07 item 28).
//!
//! The append-only journals (`health_logs`, `verification_results`,
//! `events`, and the forensic journals) are never pruned, so the catalog
//! only grows; `db stats` reports each table's size so the growth can be
//! seen. A size is measured when the SQLite build has the `dbstat` virtual
//! table (the bundled build is compiled with it) and estimated otherwise,
//! and the report always says which: an estimate presented as a measurement
//! is the confusion the journals exist to end.

use rusqlite::Connection;

use crate::error::Result;

/// How [`table_sizes`] arrived at its byte counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeMethod {
    /// Measured: the pages SQLite's `dbstat` virtual table reports each
    /// table and its indexes occupy in the file.
    Dbstat,
    /// Estimated: the sum of the stored values' lengths, with no page,
    /// row-header or index overhead — a floor, not the file's figure.
    Estimate,
}

impl SizeMethod {
    /// The name `--json` carries (`size_method`).
    pub fn as_str(self) -> &'static str {
        match self {
            SizeMethod::Dbstat => "dbstat",
            SizeMethod::Estimate => "estimate",
        }
    }

    /// One line saying what the byte counts are.
    pub fn describe(self) -> &'static str {
        match self {
            SizeMethod::Dbstat => {
                "measured by SQLite's dbstat: the pages each table and its indexes occupy"
            }
            SizeMethod::Estimate => {
                "ESTIMATED (this SQLite has no dbstat): the stored values' lengths, without \
                 page or index overhead"
            }
        }
    }
}

/// One table's row count and size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableSize {
    pub name: String,
    pub rows: i64,
    pub bytes: i64,
}

/// Every user table (not SQLite's own `sqlite_*` tables), largest first.
#[derive(Debug, Clone)]
pub struct TableSizes {
    pub method: SizeMethod,
    pub tables: Vec<TableSize>,
}

/// Whether this connection's SQLite has the `dbstat` virtual table.
pub fn has_dbstat(conn: &Connection) -> bool {
    conn.query_row("SELECT 1 FROM dbstat LIMIT 1", [], |_| Ok(()))
        .is_ok()
}

/// Every user table's rows and bytes, largest first (ties by name), by
/// `dbstat` when the build has it and by [`SizeMethod::Estimate`] otherwise.
pub fn table_sizes(conn: &Connection) -> Result<TableSizes> {
    let method = if has_dbstat(conn) {
        SizeMethod::Dbstat
    } else {
        SizeMethod::Estimate
    };
    table_sizes_by(conn, method)
}

fn user_tables(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
         ORDER BY name",
    )?;
    let names = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(names)
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// [`table_sizes`] with the method chosen by the caller, so the estimate is
/// testable on a build that has `dbstat`.
pub fn table_sizes_by(conn: &Connection, method: SizeMethod) -> Result<TableSizes> {
    let mut tables = Vec::new();
    // Bytes per table, indexes counted with the table they index.
    let measured: std::collections::HashMap<String, i64> = match method {
        SizeMethod::Dbstat => {
            let mut stmt = conn.prepare(
                "SELECT m.tbl_name, SUM(s.pgsize) FROM dbstat s \
                 JOIN sqlite_master m ON m.name = s.name GROUP BY m.tbl_name",
            )?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                .collect::<std::result::Result<_, _>>()?;
            rows
        }
        SizeMethod::Estimate => Default::default(),
    };
    for name in user_tables(conn)? {
        let q = quote_ident(&name);
        let rows: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM {q}"), [], |r| r.get(0))?;
        let bytes = match method {
            SizeMethod::Dbstat => measured.get(&name).copied().unwrap_or(0),
            SizeMethod::Estimate => {
                let mut stmt = conn
                    .prepare(&format!("SELECT name FROM pragma_table_info({})", {
                        format!("'{}'", name.replace('\'', "''"))
                    }))?;
                let cols: Vec<String> = stmt
                    .query_map([], |r| r.get::<_, String>(0))?
                    .collect::<std::result::Result<_, _>>()?;
                if cols.is_empty() {
                    0
                } else {
                    let sum = cols
                        .iter()
                        .map(|c| format!("COALESCE(length({}), 0)", quote_ident(c)))
                        .collect::<Vec<_>>()
                        .join(" + ");
                    conn.query_row(
                        &format!("SELECT COALESCE(SUM({sum}), 0) FROM {q}"),
                        [],
                        |r| r.get(0),
                    )?
                }
            }
        };
        tables.push(TableSize { name, rows, bytes });
    }
    tables.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.name.cmp(&b.name)));
    Ok(TableSizes { method, tables })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A catalog with a deliberately heavy `health_logs` and one tenant row:
    /// the journal must come first and the tenants after it, by name — a
    /// test that only asked for a non-empty list would pass against a
    /// report that ordered nothing.
    fn heavy_journal() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::TempDir::new().unwrap();
        let conn = crate::db::open(&dir.path().join("t.db")).unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator) VALUES ('family', 0)",
            [],
        )
        .unwrap();
        let raw = "=== page 0x02 ===\n".repeat(200);
        for _ in 0..200 {
            conn.execute(
                "INSERT INTO health_logs (operation, raw_log) VALUES ('write', ?1)",
                [&raw],
            )
            .unwrap();
        }
        (dir, conn)
    }

    fn position(sizes: &TableSizes, name: &str) -> usize {
        sizes
            .tables
            .iter()
            .position(|t| t.name == name)
            .unwrap_or_else(|| panic!("{name} missing from {:?}", sizes.tables))
    }

    #[test]
    fn the_bundled_sqlite_measures_with_dbstat() {
        let (_dir, conn) = heavy_journal();
        assert!(has_dbstat(&conn), "the bundled SQLite is built with dbstat");
        assert_eq!(table_sizes(&conn).unwrap().method, SizeMethod::Dbstat);
    }

    #[test]
    fn a_heavy_journal_comes_first_by_either_method() {
        let (_dir, conn) = heavy_journal();
        for method in [SizeMethod::Dbstat, SizeMethod::Estimate] {
            let sizes = table_sizes_by(&conn, method).unwrap();
            assert_eq!(
                sizes.tables[0].name, "health_logs",
                "{method:?}: {:?}",
                sizes.tables
            );
            assert!(position(&sizes, "health_logs") < position(&sizes, "tenants"));
            let logs = &sizes.tables[0];
            assert_eq!(logs.rows, 200);
            assert!(logs.bytes > 200 * 3600, "{method:?}: {logs:?}");
            let tenants = &sizes.tables[position(&sizes, "tenants")];
            assert!(tenants.rows >= 1);
            assert!(tenants.bytes < logs.bytes);
        }
    }

    #[test]
    fn sqlite_internal_tables_are_not_listed() {
        let (_dir, conn) = heavy_journal();
        let sizes = table_sizes(&conn).unwrap();
        assert!(sizes.tables.iter().all(|t| !t.name.starts_with("sqlite_")));
        let total: usize = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' \
                 AND name NOT LIKE 'sqlite_%'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap() as usize;
        assert_eq!(sizes.tables.len(), total, "every user table is listed");
    }
}
