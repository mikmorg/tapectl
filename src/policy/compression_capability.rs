//! Advisory scan (issue #97): does an already-stored `archive_sets.compression`
//! value name one of the algorithms tapectl carries?
//!
//! tapectl compresses in-process, each slice alone, with gzip, xz or zstd
//! (ADR-0012 items 285, 286), so whether a value can be performed no longer
//! depends on the host: there is no probe of any installed tool. What is
//! left is the closed set itself ([`crate::config::VALID_COMPRESSION_VALUES`]).
//! Writing a row checks it (`cli::archive_set`), so this finds only a row some
//! other path wrote, and says so before the next staging fails on it.
//!
//! Same shape as [`crate::policy::decorative`] / [`crate::policy::depth_check`]:
//! advisory only, never rewrites the row, never changes `config check`'s
//! exit code.

use rusqlite::Connection;

use crate::config::VALID_COMPRESSION_VALUES;

/// One `archive_sets` row whose stored `compression` is not a carried
/// algorithm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedCompressionHit {
    pub archive_set_name: String,
    pub compression: String,
}

/// Scan every `archive_sets` row for a `compression` value outside the
/// carried set.
pub fn scan(conn: &Connection) -> Vec<UnsupportedCompressionHit> {
    let mut hits = Vec::new();

    let mut stmt = match conn
        .prepare("SELECT name, compression FROM archive_sets WHERE compression IS NOT NULL")
    {
        Ok(s) => s,
        Err(_) => return hits,
    };

    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    });
    let Ok(rows) = rows else {
        return hits;
    };

    for (name, compression) in rows.flatten() {
        if !VALID_COMPRESSION_VALUES.contains(&compression.as_str()) {
            hits.push(UnsupportedCompressionHit {
                archive_set_name: name,
                compression,
            });
        }
    }

    hits
}

/// The advisory line for one hit. Pure, so text and `--json` arms of
/// `config check` can never drift apart.
pub fn describe(hit: &UnsupportedCompressionHit) -> String {
    format!(
        "warning: archive set \"{}\" has compression \"{}\", which tapectl does not carry \
         (it carries {}) — staging with this set will fail until the value is changed",
        hit.archive_set_name,
        hit.compression,
        VALID_COMPRESSION_VALUES.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_conn() -> Connection {
        crate::db::open_memory().unwrap()
    }

    #[test]
    fn flags_a_row_with_a_value_tapectl_does_not_carry() {
        let conn = fresh_conn();
        conn.execute(
            "INSERT INTO archive_sets (name, compression) VALUES ('cold', 'lzo')",
            [],
        )
        .unwrap();

        let hits = scan(&conn);

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].archive_set_name, "cold");
        assert_eq!(hits[0].compression, "lzo");
    }

    #[test]
    fn every_carried_value_is_not_flagged() {
        let conn = fresh_conn();
        for (i, alg) in VALID_COMPRESSION_VALUES.iter().enumerate() {
            conn.execute(
                "INSERT INTO archive_sets (name, compression) VALUES (?1, ?2)",
                rusqlite::params![format!("set{i}"), alg],
            )
            .unwrap();
        }
        assert!(scan(&conn).is_empty());
    }

    #[test]
    fn null_compression_row_is_never_flagged() {
        let conn = fresh_conn();
        conn.execute("INSERT INTO archive_sets (name) VALUES ('cold')", [])
            .unwrap();
        assert!(scan(&conn).is_empty());
    }

    #[test]
    fn describe_names_the_set_the_value_and_the_carried_ones() {
        let hit = UnsupportedCompressionHit {
            archive_set_name: "cold".to_string(),
            compression: "lzo".to_string(),
        };
        let line = describe(&hit);
        assert!(line.contains("cold"));
        assert!(line.contains("lzo"));
        assert!(line.contains("gzip, xz, zstd"));
    }
}
