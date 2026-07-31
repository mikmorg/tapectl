//! Streaming JSON export of the live database (issue #61).
//!
//! `db export` must emit one complete JSON document to stdout: the schema
//! version from the `meta` table, plus every user table (enumerated at
//! runtime from `sqlite_master`, never hardcoded) as an array of row
//! objects keyed by column name. Tables can hold millions of rows (`files`,
//! `manifest_entries`), so this writes incrementally to a `BufWriter` around
//! a locked stdout handle instead of building a `serde_json::Value` (or a
//! `String`) of the whole database in memory — peak memory tracks one row,
//! not one table or the whole dump.
//!
//! Nothing may be written to stdout on this path except the JSON document
//! itself (issue #56's defect class): no progress lines, no summary. Use
//! `tracing::warn!` (stderr) if something needs saying.

use std::io::Write;

use rusqlite::types::ValueRef;
use rusqlite::Connection;

use crate::error::Result;

/// Escape and write a JSON string literal (including the surrounding
/// quotes) for `s` to `out`.
fn write_json_string<W: Write>(out: &mut W, s: &str) -> std::io::Result<()> {
    out.write_all(b"\"")?;
    for c in s.chars() {
        match c {
            '"' => out.write_all(b"\\\"")?,
            '\\' => out.write_all(b"\\\\")?,
            '\n' => out.write_all(b"\\n")?,
            '\r' => out.write_all(b"\\r")?,
            '\t' => out.write_all(b"\\t")?,
            c if (c as u32) < 0x20 => {
                write!(out, "\\u{:04x}", c as u32)?;
            }
            c => {
                let mut buf = [0u8; 4];
                out.write_all(c.encode_utf8(&mut buf).as_bytes())?;
            }
        }
    }
    out.write_all(b"\"")
}

/// Render a single SQLite column value as a JSON value string, writing it
/// to `out`.
///
/// Type mapping: INTEGER -> JSON number, REAL -> JSON number, TEXT -> JSON
/// string, NULL -> `null`, BLOB -> a lowercase hex string (documented here
/// since JSON has no native binary type).
pub fn write_json_value<W: Write>(out: &mut W, value: ValueRef<'_>) -> std::io::Result<()> {
    match value {
        ValueRef::Null => out.write_all(b"null"),
        ValueRef::Integer(i) => write!(out, "{i}"),
        ValueRef::Real(f) => {
            if f.is_finite() {
                write!(out, "{f}")
            } else {
                // JSON has no NaN/Infinity; fall back to null rather than
                // emitting an invalid document.
                out.write_all(b"null")
            }
        }
        ValueRef::Text(t) => {
            let s = String::from_utf8_lossy(t);
            write_json_string(out, &s)
        }
        ValueRef::Blob(b) => {
            let mut hex = String::with_capacity(b.len() * 2);
            for byte in b {
                hex.push_str(&format!("{byte:02x}"));
            }
            write_json_string(out, &hex)
        }
    }
}

/// Stream a full JSON dump of the database to `out`.
///
/// Enumerates tables from `sqlite_master` at runtime (never a hardcoded
/// list), reads `schema_version` from the `meta` table, and streams each
/// table's rows one at a time so peak memory tracks a single row.
pub fn export_json<W: Write>(conn: &Connection, out: &mut W) -> Result<()> {
    // schema_version: read from the real `meta` table, not a constant.
    let schema_version: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .ok();

    out.write_all(b"{\"schema_version\":")?;
    match &schema_version {
        Some(v) => write_json_string(out, v)?,
        None => out.write_all(b"null")?,
    }
    out.write_all(b",\"tables\":{")?;

    let mut table_stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")?;
    let table_names: Vec<String> = table_stmt
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    drop(table_stmt);

    for (ti, table) in table_names.iter().enumerate() {
        if ti > 0 {
            out.write_all(b",")?;
        }
        write_json_string(out, table)?;
        out.write_all(b":[")?;

        // Table names come from sqlite_master, not user input, so this is
        // not string-interpolated SQL injection — same pattern the old
        // hardcoded-list export used.
        let mut row_stmt = conn.prepare(&format!("SELECT * FROM \"{table}\""))?;
        let col_count = row_stmt.column_count();
        let col_names: Vec<String> = (0..col_count)
            .map(|i| row_stmt.column_name(i).unwrap_or("").to_string())
            .collect();

        let mut rows = row_stmt.query([])?;
        let mut ri = 0usize;
        while let Some(row) = rows.next()? {
            if ri > 0 {
                out.write_all(b",")?;
            }
            out.write_all(b"{")?;
            for (ci, col_name) in col_names.iter().enumerate() {
                if ci > 0 {
                    out.write_all(b",")?;
                }
                write_json_string(out, col_name)?;
                out.write_all(b":")?;
                let value = row.get_ref(ci)?;
                write_json_value(out, value)?;
            }
            out.write_all(b"}")?;
            ri += 1;
        }

        out.write_all(b"]")?;
    }

    out.write_all(b"}}")?;
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(value: ValueRef<'_>) -> String {
        let mut buf = Vec::new();
        write_json_value(&mut buf, value).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn integer_maps_to_number() {
        assert_eq!(render(ValueRef::Integer(42)), "42");
        assert_eq!(render(ValueRef::Integer(-7)), "-7");
    }

    #[test]
    fn real_maps_to_number() {
        assert_eq!(render(ValueRef::Real(3.5)), "3.5");
    }

    #[test]
    fn text_maps_to_string() {
        assert_eq!(render(ValueRef::Text(b"hello")), "\"hello\"");
    }

    #[test]
    fn text_with_special_chars_is_escaped() {
        assert_eq!(render(ValueRef::Text(b"a\"b\\c\nd")), "\"a\\\"b\\\\c\\nd\"");
    }

    #[test]
    fn null_maps_to_json_null() {
        assert_eq!(render(ValueRef::Null), "null");
    }

    #[test]
    fn blob_maps_to_hex_string() {
        assert_eq!(
            render(ValueRef::Blob(&[0xde, 0xad, 0xbe, 0xef])),
            "\"deadbeef\""
        );
    }

    #[test]
    fn blob_empty_maps_to_empty_hex_string() {
        assert_eq!(render(ValueRef::Blob(&[])), "\"\"");
    }

    #[test]
    fn export_json_produces_parseable_document_with_runtime_enumerated_table() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO meta (key, value) VALUES ('schema_version', '6');
             CREATE TABLE locations (id INTEGER PRIMARY KEY, name TEXT, blob_col BLOB, notes TEXT);
             INSERT INTO locations (id, name, blob_col, notes) VALUES (1, 'vault', X'ab', NULL);",
        )
        .unwrap();
        let _ = &mut conn;

        let mut buf = Vec::new();
        export_json(&conn, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();

        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["schema_version"], "6");
        let locations = &parsed["tables"]["locations"];
        assert_eq!(locations[0]["id"], 1);
        assert_eq!(locations[0]["name"], "vault");
        assert_eq!(locations[0]["blob_col"], "ab");
        assert!(locations[0]["notes"].is_null());
    }
}
