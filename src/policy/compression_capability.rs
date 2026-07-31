//! Advisory scan (issue #97): does an already-stored `archive_sets.compression`
//! value match what the local `dar` binary can actually perform?
//!
//! Validation at `archive-set create/edit/sync` write time is only checked
//! at the moment of writing (issue #97 wires that in `cli::archive_set`).
//! Any row written *before* that guard existed — or written against a
//! different dar binary than is installed now — keeps whatever value it
//! has, and since issue #92 made `archive_sets.compression` reach the dar
//! invocation directly, that stale value now reaches `dar -z` at write
//! time. This scan surfaces such rows so an operator notices before the
//! next archive attempt fails.
//!
//! Same shape as [`crate::policy::decorative`] / [`crate::policy::depth_check`]:
//! advisory only, never rewrites the row, never changes `config check`'s
//! exit code.

use rusqlite::Connection;

use crate::dar::version::{self, DarCapabilities};

/// One `archive_sets` row whose stored `compression` the local dar cannot
/// perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedCompressionHit {
    pub archive_set_name: String,
    pub compression: String,
}

/// Scan every `archive_sets` row for a `compression` value unsupported by
/// `dar_binary`. Fails open exactly like [`version::capabilities`]: if the
/// capability probe itself cannot run (dar missing/unreadable — already
/// reported separately by `config check`'s dar depth-check), this returns
/// no hits rather than piling on a second error or falsely flagging every
/// row.
pub fn scan(conn: &Connection, dar_binary: &str) -> Vec<UnsupportedCompressionHit> {
    let Ok(caps) = version::capabilities(dar_binary) else {
        return Vec::new();
    };
    scan_with_capabilities(conn, &caps)
}

/// Same as [`scan`] but takes pre-probed capabilities, so callers that
/// already ran the dar depth-check don't probe twice, and so this is
/// unit-testable without a real dar binary.
pub fn scan_with_capabilities(
    conn: &Connection,
    caps: &DarCapabilities,
) -> Vec<UnsupportedCompressionHit> {
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

    for row in rows.flatten() {
        let (name, compression) = row;
        if !caps.supports(&compression) {
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
        "warning: archive set \"{}\" has compression \"{}\", which the local dar binary \
         cannot perform — archiving with this set will fail until the value is changed \
         or a dar build with that codec is installed",
        hit.archive_set_name, hit.compression
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps_without(missing: &[&str]) -> DarCapabilities {
        let mut text = String::from(" Using libdar 6.7.1 built with compilation time options:\n");
        for (label, alg) in [
            ("gzip compression (libz)", "gzip"),
            ("bzip2 compression (libbzip2)", "bzip2"),
            ("lzo compression (liblzo2)", "lzo"),
            ("xz compression (liblzma)", "xz"),
            ("zstd compression (libzstd)", "zstd"),
            ("lz4 compression (liblz4)", "lz4"),
        ] {
            let verdict = if missing.contains(&alg) { "NO" } else { "YES" };
            text.push_str(&format!("   {label} : {verdict}\n"));
        }
        version::parse_capabilities(&text)
    }

    fn fresh_conn() -> Connection {
        crate::db::open_memory().unwrap()
    }

    #[test]
    fn flags_a_preexisting_row_with_unsupported_compression() {
        let conn = fresh_conn();
        conn.execute(
            "INSERT INTO archive_sets (name, compression) VALUES ('cold', 'lzo')",
            [],
        )
        .unwrap();

        let caps = caps_without(&["lzo"]);
        let hits = scan_with_capabilities(&conn, &caps);

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].archive_set_name, "cold");
        assert_eq!(hits[0].compression, "lzo");
    }

    #[test]
    fn supported_row_is_not_flagged() {
        let conn = fresh_conn();
        conn.execute(
            "INSERT INTO archive_sets (name, compression) VALUES ('cold', 'gzip')",
            [],
        )
        .unwrap();

        let caps = caps_without(&["lzo"]);
        let hits = scan_with_capabilities(&conn, &caps);
        assert!(hits.is_empty());
    }

    #[test]
    fn null_compression_row_is_never_flagged() {
        let conn = fresh_conn();
        conn.execute("INSERT INTO archive_sets (name) VALUES ('cold')", [])
            .unwrap();

        let caps = caps_without(&["lzo", "zstd", "lz4", "xz", "gzip", "bzip2"]);
        let hits = scan_with_capabilities(&conn, &caps);
        assert!(hits.is_empty());
    }

    #[test]
    fn describe_names_the_set_and_the_codec() {
        let hit = UnsupportedCompressionHit {
            archive_set_name: "cold".to_string(),
            compression: "lzo".to_string(),
        };
        let line = describe(&hit);
        assert!(line.contains("cold"));
        assert!(line.contains("lzo"));
    }
}
