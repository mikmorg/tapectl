//! Advisory scan for policy knobs that cannot be honored independently
//! (CTO decision 2026-07-31, issue #50; see `docs/design-errata.md`).
//!
//! `preserve_acls` is documented (v4.0 §7 / §1363) and has an
//! `archive_sets` column, but **dar exposes no independent ACL switch**.
//! On Linux, dar carries ACLs as Extended Attributes whenever EA support
//! is compiled in, and tapectl passes no `-u`/`-U` EA-exclusion mask — so
//! ACLs are preserved unconditionally, and `preserve_acls = false` cannot
//! be honored without also discarding every xattr the operator never
//! asked to lose.
//!
//! The ratified resolution is to keep the knob and make the no-op
//! **visible** rather than silent — the #92 precedent: surface a dead
//! knob, do not quietly delete operator-facing surface. Only `false` is
//! reported: `true` already matches what actually happens, so saying
//! anything about it would be noise.
//!
//! Like [`crate::policy::shadowing`], this advises and never rewrites,
//! and it must never affect `config check`'s exit code.

use rusqlite::Connection;

use crate::config::Config;

/// One place `preserve_acls = false` is set but cannot take effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubsumedAcls {
    /// Human-facing origin, e.g. `"defaults"` or `"archive set \"media\""`.
    pub source: String,
}

/// Every layer that sets `preserve_acls = false`, which is not achievable.
///
/// Reads `config.defaults` and the `archive_sets` table. Dotfiles are not
/// scanned: `preserve_acls` is not among the fields `unit init` writes,
/// so a dotfile carrying it is a hand-edit, and walking every unit's
/// filesystem path for one advisory line is not worth the I/O here —
/// `shadowing::scan` already owns the dotfile walk if that changes.
pub fn scan(config: &Config, conn: &Connection) -> Vec<SubsumedAcls> {
    let mut out = Vec::new();

    if !config.defaults.preserve_acls {
        out.push(SubsumedAcls {
            source: "defaults".to_string(),
        });
    }

    // A missing table (fresh DB) is not an error for an advisory scan.
    let mut stmt = match conn.prepare("SELECT name FROM archive_sets WHERE preserve_acls = 0") {
        Ok(s) => s,
        Err(_) => return out,
    };
    if let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(0)) {
        for name in rows.flatten() {
            out.push(SubsumedAcls {
                source: format!("archive set \"{name}\""),
            });
        }
    }

    out
}

/// The advisory line for one hit. Pure, so the wording is testable
/// without a `Connection` — and so `config check`'s `--json` arm and its
/// text arm can never drift apart.
pub fn describe(hit: &SubsumedAcls) -> String {
    format!(
        "note: {} sets preserve_acls = false, which cannot take effect — dar has no independent \
         ACL switch, so ACLs ride Extended Attributes and are preserved whenever preserve_xattrs \
         is on. Use preserve_xattrs to control this.",
        hit.source
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn_without_archive_sets() -> Connection {
        Connection::open_in_memory().unwrap()
    }

    #[test]
    fn preserve_acls_true_reports_nothing() {
        let mut config = Config::default();
        config.defaults.preserve_acls = true;
        assert!(scan(&config, &conn_without_archive_sets()).is_empty());
    }

    #[test]
    fn preserve_acls_false_in_defaults_is_reported() {
        let mut config = Config::default();
        config.defaults.preserve_acls = false;
        let hits = scan(&config, &conn_without_archive_sets());
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].source, "defaults");
    }

    #[test]
    fn a_missing_archive_sets_table_is_not_an_error() {
        // Advisory scans run against fresh/partial DBs; they must degrade
        // to "nothing to report", never propagate a rusqlite error.
        let mut config = Config::default();
        config.defaults.preserve_acls = true;
        assert!(scan(&config, &conn_without_archive_sets()).is_empty());
    }

    #[test]
    fn archive_set_rows_with_false_are_reported_by_name() {
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO archive_sets (name, preserve_acls) VALUES ('media', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO archive_sets (name, preserve_acls) VALUES ('docs', 1)",
            [],
        )
        .unwrap();

        let mut config = Config::default();
        config.defaults.preserve_acls = true;
        let hits = scan(&config, &conn);
        assert_eq!(hits.len(), 1, "only the false row: {hits:?}");
        assert_eq!(hits[0].source, "archive set \"media\"");
    }

    #[test]
    fn the_advisory_names_the_source_and_the_real_control() {
        let line = describe(&SubsumedAcls {
            source: "defaults".to_string(),
        });
        assert!(line.contains("defaults"));
        assert!(
            line.contains("preserve_xattrs"),
            "must point at the knob that actually works: {line}"
        );
    }
}
