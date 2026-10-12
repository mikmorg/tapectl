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
//!
//! [`journal_growth`] says how fast each journal grew over the trailing
//! [`GROWTH_WINDOW_DAYS`]: its rows dated in the window, exactly, and the
//! bytes they take, which are DERIVED (the bytes in use on the table's
//! pages, shared among its rows by stored length) because no SQLite figure
//! sizes a row.

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
                let sum = stored_length_expr(conn, &name)?;
                conn.query_row(
                    &format!("SELECT COALESCE(SUM({sum}), 0) FROM {q}"),
                    [],
                    |r| r.get(0),
                )?
            }
        };
        tables.push(TableSize { name, rows, bytes });
    }
    tables.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.name.cmp(&b.name)));
    Ok(TableSizes { method, tables })
}

/// The append-only journals, each with the column that dates its rows
/// (ADR-0012 2026-10-07 item 28: never pruned). A table here that a
/// migration renames or drops fails `every_journal_has_its_date_column`,
/// not the operator's `db stats`.
///
/// `readback_checkpoints` is not one: its rows are a working set, deleted
/// with their session. `verification_sessions.started_at` of a continued
/// readback is dated from its oldest checkpoint (ADR-0012 2026-10-07 item
/// 1), so a session written today can fall before the window.
pub const JOURNALS: &[(&str, &str)] = &[
    ("cartridge_contacts", "opened_at"),
    ("dar_create_reports", "captured_at"),
    ("events", "timestamp"),
    ("health_logs", "logged_at"),
    ("log_page_journal", "captured_at"),
    ("mam_journal", "captured_at"),
    ("mtget_journal", "captured_at"),
    ("phase_timings", "started_at"),
    ("restores", "started_at"),
    ("st_stats_journal", "captured_at"),
    ("verification_results", "verified_at"),
    ("verification_sessions", "started_at"),
];

/// How far back `db stats` looks to say how fast each journal grows.
pub const GROWTH_WINDOW_DAYS: i64 = 90;

/// One journal's growth over the window.
#[derive(Debug, Clone, PartialEq)]
pub struct JournalGrowth {
    pub name: String,
    /// The column the window is judged by.
    pub date_column: &'static str,
    /// Rows dated inside the window.
    pub rows_in_window: i64,
    /// DERIVED, not measured — see [`JournalGrowths::method`]. No SQLite
    /// figure sizes a row: `dbstat` sizes pages.
    pub bytes_in_window: i64,
    /// `bytes_in_window` scaled to 365 days of the window's pace.
    pub bytes_per_year: i64,
}

/// Every journal's growth, fastest first (ties by name).
#[derive(Debug, Clone, PartialEq)]
pub struct JournalGrowths {
    /// [`SizeMethod::Dbstat`]: the bytes the table's and its indexes'
    /// pages hold (page size less free space, so the empty pages of a
    /// near-empty table are not charged to its few rows), shared among its
    /// rows by their stored length. [`SizeMethod::Estimate`]: the rows'
    /// stored values' lengths alone.
    pub method: SizeMethod,
    /// The days the window actually covers: [`GROWTH_WINDOW_DAYS`], or
    /// fewer when the catalog's oldest journal row is younger than that —
    /// a young catalog's pace is its own age's, not diluted over days it
    /// did not exist. Never under one day, so an hour-old catalog is not
    /// extrapolated by a factor of thousands.
    pub window_days: f64,
    pub journals: Vec<JournalGrowth>,
}

impl JournalGrowths {
    /// One line saying what the byte counts are.
    pub fn describe(&self) -> &'static str {
        match self.method {
            SizeMethod::Dbstat => {
                "rows exact; bytes derived: the bytes in use on each table's and its indexes' \
                 pages, shared among its rows by stored length"
            }
            SizeMethod::Estimate => {
                "rows exact; bytes ESTIMATED (this SQLite has no dbstat): the rows' stored \
                 values' lengths, without page or index overhead"
            }
        }
    }
}

/// Every journal's growth over the trailing [`GROWTH_WINDOW_DAYS`], by
/// `dbstat` when the build has it and by [`SizeMethod::Estimate`] otherwise.
pub fn journal_growth(conn: &Connection) -> Result<JournalGrowths> {
    let method = if has_dbstat(conn) {
        SizeMethod::Dbstat
    } else {
        SizeMethod::Estimate
    };
    journal_growth_by(conn, method, "now")
}

/// [`journal_growth`] with the method chosen by the caller and as of `now`
/// (any SQLite time value: `'now'`, or `'YYYY-MM-DD HH:MM:SS'` in UTC), so
/// both the estimate and the window are testable.
pub fn journal_growth_by(
    conn: &Connection,
    method: SizeMethod,
    now: &str,
) -> Result<JournalGrowths> {
    // Bytes in use per table, indexes counted with the table they index.
    let used: std::collections::HashMap<String, i64> = match method {
        SizeMethod::Dbstat => {
            let mut stmt = conn.prepare(
                "SELECT m.tbl_name, SUM(s.pgsize - s.unused) FROM dbstat s \
                 JOIN sqlite_master m ON m.name = s.name GROUP BY m.tbl_name",
            )?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                .collect::<std::result::Result<_, _>>()?;
            rows
        }
        SizeMethod::Estimate => Default::default(),
    };
    // The window's start in the spelling every journal's dates use
    // ('YYYY-MM-DD HH:MM:SS', UTC, some with a fractional tail), so a plain
    // text comparison selects the rows and can use a date index.
    let start: String = conn.query_row(
        &format!("SELECT datetime(?1, '-{GROWTH_WINDOW_DAYS} days')"),
        [now],
        |r| r.get(0),
    )?;

    struct Raw {
        name: &'static str,
        date_column: &'static str,
        rows_in_window: i64,
        len_total: i64,
        len_in_window: i64,
        oldest: Option<String>,
    }
    let mut raws = Vec::with_capacity(JOURNALS.len());
    for &(name, date_column) in JOURNALS {
        let len = stored_length_expr(conn, name)?;
        let dc = quote_ident(date_column);
        let (rows_in_window, len_total, len_in_window, oldest) = conn.query_row(
            &format!(
                "SELECT COALESCE(SUM({dc} >= ?1), 0), COALESCE(SUM({len}), 0), \
                 COALESCE(SUM(CASE WHEN {dc} >= ?1 THEN {len} ELSE 0 END), 0), MIN({dc}) \
                 FROM {}",
                quote_ident(name)
            ),
            [&start],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            },
        )?;
        raws.push(Raw {
            name,
            date_column,
            rows_in_window,
            len_total,
            len_in_window,
            oldest,
        });
    }

    // A catalog younger than the window is paced over its own age: from
    // its oldest journal row, at least one day. Whole seconds, so the
    // per-year scaling is exact integer arithmetic.
    const DAY: i64 = 86_400;
    let full = GROWTH_WINDOW_DAYS * DAY;
    let window_secs = match raws.iter().filter_map(|r| r.oldest.as_deref()).min() {
        Some(oldest) if oldest > start.as_str() => {
            let secs: Option<i64> = conn.query_row(
                "SELECT CAST(round((julianday(?1) - julianday(?2)) * 86400) AS INTEGER)",
                [now, oldest],
                |r| r.get(0),
            )?;
            secs.unwrap_or(full).clamp(DAY, full)
        }
        _ => full,
    };
    let window_days = window_secs as f64 / DAY as f64;

    let mut journals: Vec<JournalGrowth> = raws
        .into_iter()
        .map(|r| {
            let bytes_in_window = match method {
                SizeMethod::Estimate => r.len_in_window,
                SizeMethod::Dbstat if r.len_total > 0 => {
                    let in_use = used.get(r.name).copied().unwrap_or(0);
                    (in_use as i128 * r.len_in_window as i128 / r.len_total as i128) as i64
                }
                SizeMethod::Dbstat => 0,
            };
            JournalGrowth {
                name: r.name.to_string(),
                date_column: r.date_column,
                rows_in_window: r.rows_in_window,
                bytes_in_window,
                bytes_per_year: (bytes_in_window as i128 * 365 * DAY as i128 / window_secs as i128)
                    as i64,
            }
        })
        .collect();
    journals.sort_by(|a, b| {
        b.bytes_per_year
            .cmp(&a.bytes_per_year)
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(JournalGrowths {
        method,
        window_days,
        journals,
    })
}

/// An SQL expression summing one row's stored values' lengths (`0` for a
/// table with no columns), the measure [`SizeMethod::Estimate`] sizes a
/// table by.
fn stored_length_expr(conn: &Connection, table: &str) -> Result<String> {
    let mut stmt = conn.prepare("SELECT name FROM pragma_table_info(?1)")?;
    let cols: Vec<String> = stmt
        .query_map([table], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<_, _>>()?;
    if cols.is_empty() {
        return Ok("0".to_string());
    }
    Ok(cols
        .iter()
        .map(|c| format!("COALESCE(length({}), 0)", quote_ident(c)))
        .collect::<Vec<_>>()
        .join(" + "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn growth_of<'a>(g: &'a JournalGrowths, name: &str) -> &'a JournalGrowth {
        g.journals
            .iter()
            .find(|j| j.name == name)
            .unwrap_or_else(|| panic!("{name} missing from {:?}", g.journals))
    }

    fn empty_journals(conn: &Connection) {
        for (t, _) in JOURNALS {
            conn.execute(&format!("DELETE FROM {}", quote_ident(t)), [])
                .unwrap();
        }
    }

    #[test]
    fn every_journal_has_its_date_column() {
        let dir = tempfile::TempDir::new().unwrap();
        let conn = crate::db::open(&dir.path().join("t.db")).unwrap();
        for (table, column) in JOURNALS {
            let found: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM pragma_table_info(?1) WHERE name = ?2",
                    [table, column],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(found, 1, "{table}.{column} is not in the schema");
        }
        // The positive control, and the drift guard: every table named a
        // journal is listed, so a new one is not left out of the report.
        let named: Vec<String> = user_tables(&conn)
            .unwrap()
            .into_iter()
            .filter(|t| t.ends_with("_journal"))
            .collect();
        assert!(named.len() >= 4, "{named:?}");
        for t in named {
            assert!(
                JOURNALS.iter().any(|(j, _)| *j == t),
                "{t} is a journal not listed in JOURNALS"
            );
        }
    }

    /// Rows dated before the window are not growth; rows inside it are.
    /// The old rows make the catalog older than the window, so the window
    /// is the whole 90 days.
    #[test]
    fn only_rows_dated_inside_the_window_count() {
        let (_dir, conn) = heavy_journal();
        let raw = "=== page 0x02 ===\n".repeat(200);
        for _ in 0..50 {
            conn.execute(
                "INSERT INTO health_logs (operation, raw_log, logged_at) \
                 VALUES ('write', ?1, '2020-01-01 00:00:00')",
                [&raw],
            )
            .unwrap();
        }
        // The stored length of the 200 recent rows: the estimate's figure
        // exactly, and a floor under the measured one.
        let recent_len: i64 = conn
            .query_row(
                &format!(
                    "SELECT SUM({}) FROM health_logs WHERE logged_at > '2021'",
                    stored_length_expr(&conn, "health_logs").unwrap()
                ),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(recent_len > 200 * 3600);
        for method in [SizeMethod::Dbstat, SizeMethod::Estimate] {
            let table = table_sizes_by(&conn, method)
                .unwrap()
                .tables
                .into_iter()
                .find(|t| t.name == "health_logs")
                .unwrap();
            let g = journal_growth_by(&conn, method, "now").unwrap();
            assert_eq!(g.window_days, GROWTH_WINDOW_DAYS as f64);
            let logs = growth_of(&g, "health_logs");
            assert_eq!(logs.date_column, "logged_at");
            assert_eq!(logs.rows_in_window, 200, "{method:?}: {logs:?}");
            match method {
                SizeMethod::Estimate => assert_eq!(logs.bytes_in_window, recent_len),
                SizeMethod::Dbstat => assert!(
                    logs.bytes_in_window >= recent_len,
                    "the record and page overhead is counted: {logs:?}"
                ),
            }
            // Four fifths of the rows, so well under the whole table.
            assert!(
                logs.bytes_in_window < table.bytes * 9 / 10,
                "{method:?}: {logs:?} of {table:?}"
            );
            assert_eq!(logs.bytes_per_year, logs.bytes_in_window * 365 / 90);
            // Fastest first.
            assert_eq!(g.journals[0].name, "health_logs", "{:?}", g.journals);
            assert_eq!(g.journals.len(), JOURNALS.len());
        }
    }

    #[test]
    fn a_catalog_younger_than_the_window_is_paced_by_its_own_age() {
        let dir = tempfile::TempDir::new().unwrap();
        let conn = crate::db::open(&dir.path().join("t.db")).unwrap();
        empty_journals(&conn);
        for _ in 0..10 {
            conn.execute(
                "INSERT INTO events (entity_type, entity_id, action, timestamp) \
                 VALUES ('unit', 1, 'created', '2026-10-02 00:00:00')",
                [],
            )
            .unwrap();
        }
        let sizes = table_sizes_by(&conn, SizeMethod::Estimate).unwrap();
        let g = journal_growth_by(&conn, SizeMethod::Estimate, "2026-10-12 00:00:00").unwrap();
        assert_eq!(g.method, SizeMethod::Estimate);
        assert_eq!(g.window_days, 10.0);
        let events = growth_of(&g, "events");
        assert_eq!(events.rows_in_window, 10);
        // Under the estimate a row's bytes ARE its stored length, which is
        // what the table-size estimate sums too: the window holds all of it.
        let table = sizes.tables.iter().find(|t| t.name == "events").unwrap();
        assert_eq!(events.bytes_in_window, table.bytes);
        assert_eq!(events.bytes_per_year, table.bytes * 365 / 10);
    }

    /// A near-empty table's pages are mostly empty structure (a page per
    /// index at least). Those bytes are not its rows', so a few rows on a
    /// new catalog must not project as if each had cost a page.
    #[test]
    fn a_few_rows_are_not_charged_their_tables_empty_pages() {
        let dir = tempfile::TempDir::new().unwrap();
        let conn = crate::db::open(&dir.path().join("t.db")).unwrap();
        empty_journals(&conn);
        for _ in 0..3 {
            conn.execute(
                "INSERT INTO events (entity_type, entity_id, action) VALUES ('unit', 1, 'created')",
                [],
            )
            .unwrap();
        }
        let sizes = table_sizes(&conn).unwrap();
        let table = sizes.tables.iter().find(|t| t.name == "events").unwrap();
        let g = journal_growth(&conn).unwrap();
        assert_eq!(g.method, SizeMethod::Dbstat);
        let events = growth_of(&g, "events");
        assert_eq!(events.rows_in_window, 3);
        assert!(events.bytes_in_window > 0, "{events:?}");
        assert!(
            events.bytes_in_window < 1024 && events.bytes_in_window * 10 < table.bytes,
            "three small rows, not their table's {} pages' worth: {events:?}",
            table.bytes
        );
    }

    #[test]
    fn an_hour_old_catalog_is_paced_over_one_day() {
        let dir = tempfile::TempDir::new().unwrap();
        let conn = crate::db::open(&dir.path().join("t.db")).unwrap();
        empty_journals(&conn);
        conn.execute(
            "INSERT INTO events (entity_type, entity_id, action, timestamp) \
             VALUES ('unit', 1, 'created', '2026-10-11 23:00:00')",
            [],
        )
        .unwrap();
        let g = journal_growth_by(&conn, SizeMethod::Dbstat, "2026-10-12 00:00:00").unwrap();
        assert_eq!(g.window_days, 1.0);
    }

    #[test]
    fn empty_journals_grow_by_nothing_over_the_whole_window() {
        let dir = tempfile::TempDir::new().unwrap();
        let conn = crate::db::open(&dir.path().join("t.db")).unwrap();
        empty_journals(&conn);
        let g = journal_growth(&conn).unwrap();
        assert_eq!(g.window_days, GROWTH_WINDOW_DAYS as f64);
        assert_eq!(g.journals.len(), JOURNALS.len());
        assert!(g
            .journals
            .iter()
            .all(|j| j.rows_in_window == 0 && j.bytes_in_window == 0 && j.bytes_per_year == 0));
    }

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
