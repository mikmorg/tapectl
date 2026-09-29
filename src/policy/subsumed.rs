//! Advisory scan for policy knobs that cannot be honored independently
//! (CTO decision 2026-07-31, issue #50; see `docs/design-errata.md`).
//!
//! `preserve_acls` is documented (v4.0 §7 / §1363) and has an
//! `archive_sets` column, but **dar exposes no independent ACL switch**.
//! On Linux, POSIX ACLs ARE Extended Attributes (`system.posix_acl_*`), and
//! dar carries them with every other EA. So ACLs follow `preserve_xattrs`
//! exactly: kept while it is on, dropped when it is off — `false` passes
//! dar `-u "*"`, which excludes every EA (`dar::create`, issue #347; before
//! #347 `preserve_xattrs` did nothing at all and EAs were kept regardless).
//!
//! The ratified resolution is to keep the knob and make the no-op
//! **visible** rather than silent — the #92 precedent: surface a dead
//! knob, do not quietly delete operator-facing surface. A layer is
//! reported when its effective `preserve_acls` DISAGREES with its effective
//! `preserve_xattrs`, in either direction:
//!
//! - `preserve_acls = false` beside `preserve_xattrs = true`: the ACLs are
//!   kept anyway (the #50 case).
//! - `preserve_acls = true` beside `preserve_xattrs = false`: the ACLs are
//!   dropped anyway (possible since #347 made `false` real).
//!
//! When the two agree, what the operator asked for is what happens, and
//! saying anything would be noise.
//!
//! Like [`crate::policy::shadowing`], this advises and never rewrites,
//! and it must never affect `config check`'s exit code.

use std::collections::BTreeMap;

use rusqlite::Connection;

use crate::config::Config;

/// One policy layer whose `preserve_acls` cannot take effect because its
/// `preserve_xattrs` says the opposite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubsumedAcls {
    /// Human-facing origin, e.g. `"defaults"` or `"archive set \"media\""`.
    pub source: String,
    /// The `preserve_acls` value in force at that layer — the one that
    /// cannot take effect.
    pub preserve_acls: bool,
}

/// Every layer whose effective `preserve_acls` disagrees with its effective
/// `preserve_xattrs`.
///
/// Reads `config.defaults`, the `archive_sets` rows and each
/// `[[archive_sets]]` table. A set is judged on the pair the next
/// `archive-set sync` would leave it with: a key its table names wins (sync
/// writes exactly the keys present, issue #346), then the row's column, and
/// a NULL column inherits `[defaults]`, exactly as `policy::resolve` does —
/// so a set that turns only `preserve_xattrs` off is judged on the
/// `preserve_acls` it inherits, and a table not yet synced is judged too
/// (as `policy::decorative` reads both). A set that sets neither key is
/// covered by the `defaults` line. Dotfiles are not scanned: a dotfile's
/// `[policy]` table accepts neither key (`unit::dotfile::PolicySection`).
pub fn scan(config: &Config, conn: &Connection) -> Vec<SubsumedAcls> {
    let mut out = Vec::new();
    let defaults = &config.defaults;

    if defaults.preserve_acls != defaults.preserve_xattrs {
        out.push(SubsumedAcls {
            source: "defaults".to_string(),
            preserve_acls: defaults.preserve_acls,
        });
    }

    // name -> (preserve_xattrs, preserve_acls), `None` = inherit [defaults].
    let mut sets: BTreeMap<String, (Option<bool>, Option<bool>)> = BTreeMap::new();
    // A missing table (fresh DB) is not an error for an advisory scan.
    if let Ok(mut stmt) = conn.prepare(
        "SELECT name, preserve_xattrs, preserve_acls FROM archive_sets
         WHERE preserve_xattrs IS NOT NULL OR preserve_acls IS NOT NULL",
    ) {
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, Option<i64>>(2)?,
            ))
        });
        if let Ok(rows) = rows {
            for (name, xattrs, acls) in rows.flatten() {
                sets.insert(name, (xattrs.map(|v| v != 0), acls.map(|v| v != 0)));
            }
        }
    }
    for table in &config.archive_sets {
        let pair = sets.entry(table.name.clone()).or_insert((None, None));
        if table.preserve_xattrs.is_some() {
            pair.0 = table.preserve_xattrs;
        }
        if table.preserve_acls.is_some() {
            pair.1 = table.preserve_acls;
        }
    }

    for (name, (xattrs, acls)) in sets {
        let xattrs = xattrs.unwrap_or(defaults.preserve_xattrs);
        let acls = acls.unwrap_or(defaults.preserve_acls);
        if acls != xattrs {
            out.push(SubsumedAcls {
                source: format!("archive set \"{name}\""),
                preserve_acls: acls,
            });
        }
    }

    out
}

/// The advisory line for one hit. Pure, so the wording is testable
/// without a `Connection` — and so `config check`'s `--json` arm and its
/// text arm can never drift apart.
pub fn describe(hit: &SubsumedAcls) -> String {
    if hit.preserve_acls {
        format!(
            "note: {} has preserve_acls = true, which cannot take effect — preserve_xattrs = \
             false drops every extended attribute, and on Linux ACLs are extended attributes \
             (dar has no separate ACL switch). Set preserve_xattrs = true to keep them, or \
             set preserve_acls = false to say they go (that also silences this note).",
            hit.source
        )
    } else {
        format!(
            "note: {} sets preserve_acls = false, which cannot take effect — dar has no \
             independent ACL switch, so ACLs ride Extended Attributes and are preserved \
             whenever preserve_xattrs is on. Use preserve_xattrs to control this (false drops \
             every extended attribute, not only ACLs).",
            hit.source
        )
    }
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

    /// Issue #347 made `preserve_xattrs = false` real (dar `-u "*"`), which
    /// drops ACLs too — so `preserve_acls = true` beside it cannot take
    /// effect either, and saying nothing would let ACLs vanish silently.
    #[test]
    fn preserve_acls_true_beside_preserve_xattrs_false_is_reported() {
        let mut config = Config::default();
        config.defaults.preserve_xattrs = false;
        config.defaults.preserve_acls = true;
        let hits = scan(&config, &conn_without_archive_sets());
        assert_eq!(
            hits,
            vec![SubsumedAcls {
                source: "defaults".to_string(),
                preserve_acls: true,
            }]
        );
        let line = describe(&hits[0]);
        assert!(line.contains("preserve_acls = true"), "{line}");
        assert!(line.contains("preserve_xattrs = false"), "{line}");
        // `init` writes `preserve_acls = true`, so an operator who turns
        // only xattrs off meant to drop them and gets this note on every
        // `config check`: it must also name the edit that says so and
        // silences it, not only the one that undoes their choice.
        assert!(
            line.contains("set preserve_acls = false"),
            "must offer the other way to make the two agree: {line}"
        );
    }

    /// Both off agree — ACLs are dropped, as asked — so nothing to say.
    #[test]
    fn preserve_acls_and_preserve_xattrs_both_false_report_nothing() {
        let mut config = Config::default();
        config.defaults.preserve_xattrs = false;
        config.defaults.preserve_acls = false;
        assert!(scan(&config, &conn_without_archive_sets()).is_empty());
    }

    /// An archive set is judged on its EFFECTIVE pair: a set that only
    /// turns `preserve_xattrs` off inherits `preserve_acls = true` from
    /// `[defaults]`, and that combination cannot take effect.
    #[test]
    fn an_archive_set_is_judged_on_what_it_inherits_too() {
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO archive_sets (name, preserve_xattrs) VALUES ('bare', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO archive_sets (name, preserve_xattrs, preserve_acls) \
             VALUES ('consistent', 0, 0)",
            [],
        )
        .unwrap();
        let hits = scan(&Config::default(), &conn);
        assert_eq!(
            hits,
            vec![SubsumedAcls {
                source: "archive set \"bare\"".to_string(),
                preserve_acls: true,
            }]
        );
    }

    /// A `[[archive_sets]]` table is judged too, not only the row: an
    /// operator who writes `preserve_xattrs = false` into config.toml and
    /// runs `config check` before `archive-set sync` must see the note the
    /// sync will make true (`policy::decorative` reads both the same way).
    #[test]
    fn an_unsynced_archive_sets_table_is_judged_too() {
        let mut config = Config::default();
        let mut media = crate::config::ArchiveSetConfig {
            name: "media".to_string(),
            min_copies: None,
            required_locations: None,
            encrypt: None,
            compression: None,
            checksum_mode: None,
            verify_interval_days: None,
            slice_size: None,
            preserve_xattrs: Some(false),
            preserve_acls: None,
            preserve_fsa: None,
            dirty_on_metadata_change: None,
        };
        config.archive_sets.push(media.clone());
        let expected = vec![SubsumedAcls {
            source: "archive set \"media\"".to_string(),
            preserve_acls: true,
        }];
        assert_eq!(scan(&config, &conn_without_archive_sets()), expected);
        let conn = crate::db::open_memory().unwrap();
        assert_eq!(scan(&config, &conn), expected);

        // A key the table names wins over the row, as the next sync will
        // make it (issue #346); a key it omits keeps the row's value.
        conn.execute(
            "INSERT INTO archive_sets (name, preserve_xattrs, preserve_acls) \
             VALUES ('media', 1, 0)",
            [],
        )
        .unwrap();
        media.preserve_xattrs = Some(false);
        config.archive_sets[0] = media.clone();
        assert!(
            scan(&config, &conn).is_empty(),
            "table xattrs=false + row acls=false agree, once reported: {:?}",
            scan(&config, &conn)
        );
        media.preserve_xattrs = None;
        config.archive_sets[0] = media;
        assert_eq!(
            scan(&config, &conn),
            vec![SubsumedAcls {
                source: "archive set \"media\"".to_string(),
                preserve_acls: false,
            }],
            "a table naming neither key leaves the row's disagreement standing"
        );
    }

    #[test]
    fn the_advisory_names_the_source_and_the_real_control() {
        let line = describe(&SubsumedAcls {
            source: "defaults".to_string(),
            preserve_acls: false,
        });
        assert!(line.contains("defaults"));
        assert!(
            line.contains("preserve_xattrs"),
            "must point at the knob that actually works: {line}"
        );
    }
}
