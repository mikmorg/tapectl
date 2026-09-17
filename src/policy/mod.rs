use rusqlite::{params, Connection};

use crate::config::Config;
use crate::db::models::Unit;
use crate::error::{PolicyLayer, Result, TapectlError};

pub mod compression_capability;
pub mod coverage;
pub mod decorative;
pub mod depth_check;
pub mod escrow;
pub mod evidence;
pub mod lenient_config;
pub mod reclaimable;
pub mod shadowing;
pub mod subsumed;
pub mod unknown_keys;

/// Resolved policy for a unit after 3-level resolution:
/// unit dotfile [policy] > archive_set > system [defaults].
#[derive(Debug, Clone)]
pub struct ResolvedPolicy {
    pub min_copies: i64,
    pub required_locations: Vec<String>,
    pub encrypt: bool,
    pub compression: String,
    pub checksum_mode: String,
    pub slice_size: i64,
    pub verify_interval_days: Option<i64>,
    pub preserve_xattrs: bool,
    pub preserve_acls: bool,
    pub preserve_fsa: bool,
    pub dirty_on_metadata_change: bool,
    /// ADR-0006: how many recorded WAREHOUSE deposits this unit should
    /// carry, on top of its tape copies. 0 (the default) means none are
    /// expected and `audit` says nothing about warehouses for this unit.
    pub warehouse_copies: i64,
}

/// Resolve the effective policy for a unit.
///
/// Resolution order (first non-NULL wins):
/// 1. Unit dotfile [policy] section (read from disk if available)
/// 2. Archive set (from DB via unit.archive_set_id)
/// 3. System defaults (from config.toml)
///
/// Returns `Err` if the unit dotfile's own `[policy]` section (the only
/// layer parsed at USE time, not at config load — see below) is not valid.
/// `config.defaults.slice_size`/`compression`/`checksum_mode` and any
/// `archive_sets` equivalents are already validated at config load /
/// archive-set write time respectively, so in practice this can only fail on
/// a bad operator-authored dotfile value: an invalid `slice_size` string
/// (issue #59), a `compression`/`checksum_mode` outside the closed set
/// `config::validate_compression`/`validate_checksum_mode` accept (issue
/// #213 — the same rule ADR-0012 states for every other closed-set field,
/// applied here for the first time), or an unrecognized `[policy]` key
/// (issue #211 — `PolicySection`'s `#[serde(deny_unknown_fields)]` refuses a
/// misspelled key by name instead of silently deferring upward forever).
///
/// **Every layer now fails loudly rather than falling through (issue #105.)**
/// Each layer used to be wrapped in `if let Ok(..)`, so a database error, a
/// dangling `archive_set_id`, a corrupt `required_locations` JSON, or an
/// unreadable/unparseable dotfile all degraded *silently* to the weaker
/// system defaults — and the unit then read as compliant against a policy
/// its operator never chose. That is strictly worse than a known-violating
/// unit, because the tool cannot tell. `audit` catches the `Err` and reports
/// `policy_unresolvable` as a VIOLATION naming the checks it skipped (the
/// shape #59 established); do not add a second error style here.
///
/// The one deliberate silence: **an ABSENT dotfile is normal** and defers
/// upward (the #92 contract). Only a dotfile that is *present* and cannot be
/// read or parsed is an error. Conflating the two makes every unit without a
/// dotfile start failing.
pub fn resolve(conn: &Connection, config: &Config, unit: &Unit) -> Result<ResolvedPolicy> {
    let defaults = &config.defaults;

    // Start with system defaults
    let mut policy = ResolvedPolicy {
        min_copies: defaults.min_copies_for_tape_only as i64,
        required_locations: Vec::new(),
        encrypt: defaults.encrypt,
        compression: defaults.compression.clone(),
        checksum_mode: defaults.checksum_mode.clone(),
        slice_size: crate::staging::parse_size_to_bytes(&defaults.slice_size).map_err(|e| {
            TapectlError::PolicyUnresolvable {
                layer: PolicyLayer::Defaults,
                detail: format!("config.toml [defaults] slice_size is invalid: {e}"),
            }
        })?,
        verify_interval_days: None,
        preserve_xattrs: defaults.preserve_xattrs,
        preserve_acls: defaults.preserve_acls,
        preserve_fsa: defaults.preserve_fsa,
        dirty_on_metadata_change: defaults.dirty_on_metadata_change,
        warehouse_copies: defaults.warehouse_copies,
    };

    // Layer 2: Archive set (if unit has one)
    if let Some(as_id) = unit.archive_set_id {
        // A dangling `archive_set_id` is corruption, not a normal state:
        // nothing in tapectl deletes an `archive_sets` row, and `db::open*`
        // runs with `PRAGMA foreign_keys = ON`, so the reference cannot go
        // stale through any supported path. Silently skipping the layer
        // would hand the unit the system defaults it was explicitly moved
        // off — so it is mapped to a message that names the unit and the id
        // rather than propagating rusqlite's bare "Query returned no rows",
        // which an operator cannot act on.
        let row = conn
            .query_row(
                "SELECT min_copies, required_locations, encrypt, compression, checksum_mode,
                    slice_size, verify_interval_days, preserve_xattrs, preserve_acls,
                    preserve_fsa, dirty_on_metadata_change, warehouse_copies
             FROM archive_sets WHERE id = ?1",
                params![as_id],
                |row| {
                    Ok((
                        row.get::<_, Option<i64>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<i64>>(5)?,
                        row.get::<_, Option<i64>>(6)?,
                        row.get::<_, Option<i64>>(7)?,
                        row.get::<_, Option<i64>>(8)?,
                        row.get::<_, Option<i64>>(9)?,
                        row.get::<_, Option<i64>>(10)?,
                        row.get::<_, Option<i64>>(11)?,
                    ))
                },
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => TapectlError::PolicyUnresolvable {
                    layer: PolicyLayer::ArchiveSet,
                    detail: format!(
                        "unit \"{}\" references archive_set_id {} but no such archive set \
                         exists — the catalog is inconsistent",
                        unit.name, as_id
                    ),
                },
                other => TapectlError::Database(other),
            })?;

        let (
            min_copies,
            locations_json,
            encrypt,
            compression,
            checksum_mode,
            slice_size,
            verify_days,
            preserve_xattrs,
            preserve_acls,
            preserve_fsa,
            dirty_on_meta,
            warehouse_copies,
        ) = row;

        if let Some(v) = min_copies {
            policy.min_copies = v;
        }
        if let Some(locs) = locations_json {
            // Every writer (`archive-set create`, `edit`, `sync`) stores this
            // via `serde_json::to_string` on a `Vec`, and stores NULL when the
            // operator sets none — so a non-NULL value that will not parse is
            // corruption, never a legitimate shape. Swallowing it yielded an
            // EMPTY vec, i.e. "no locations required", which is precisely the
            // silent downgrade this function no longer performs. (Not named by
            // issue #105; same class, same function.)
            policy.required_locations =
                serde_json::from_str::<Vec<String>>(&locs).map_err(|e| {
                    TapectlError::PolicyUnresolvable {
                        layer: PolicyLayer::ArchiveSet,
                        detail: format!(
                            "archive set {as_id} has an unparseable required_locations value \
                             ({e}); expected a JSON array of location names, found: {locs}"
                        ),
                    }
                })?;
        }
        if let Some(v) = encrypt {
            policy.encrypt = v != 0;
        }
        if let Some(v) = compression {
            policy.compression = v;
        }
        if let Some(v) = checksum_mode {
            policy.checksum_mode = v;
        }
        if let Some(v) = slice_size {
            policy.slice_size = v;
        }
        if let Some(v) = verify_days {
            policy.verify_interval_days = Some(v);
        }
        if let Some(v) = preserve_xattrs {
            policy.preserve_xattrs = v != 0;
        }
        if let Some(v) = preserve_acls {
            policy.preserve_acls = v != 0;
        }
        if let Some(v) = preserve_fsa {
            policy.preserve_fsa = v != 0;
        }
        if let Some(v) = dirty_on_meta {
            policy.dirty_on_metadata_change = v != 0;
        }
        if let Some(v) = warehouse_copies {
            policy.warehouse_copies = v;
        }
    }

    // Layer 1: Unit dotfile [policy] section (highest priority)
    // Read from disk if the unit has a path
    if let Some(ref path) = unit.current_path {
        let dotfile_path = std::path::Path::new(path).join(".tapectl-unit.toml");
        // `exists()` is the whole absent-vs-present split: a unit with no
        // dotfile defers upward in silence (#92). Past this point the file
        // IS there, so both an IO failure and a parse failure are real —
        // including the TOCTOU case where it is removed between the check
        // and the read, which is NOT the same as never having existed and
        // must not be quietly folded back into "absent".
        if dotfile_path.exists() {
            let contents = std::fs::read_to_string(&dotfile_path).map_err(|e| {
                TapectlError::PolicyUnresolvable {
                    layer: PolicyLayer::Dotfile,
                    detail: format!(
                        "unit \"{}\" has a .tapectl-unit.toml at {} that cannot be read ({e})",
                        unit.name,
                        dotfile_path.display()
                    ),
                }
            })?;
            let toml =
                contents
                    .parse::<toml::Table>()
                    .map_err(|e| TapectlError::PolicyUnresolvable {
                        layer: PolicyLayer::Dotfile,
                        detail: format!(
                            "unit \"{}\" has a malformed .tapectl-unit.toml at {} ({e})",
                            unit.name,
                            dotfile_path.display()
                        ),
                    })?;
            if let Some(pol_value) = toml.get("policy") {
                // Issue #211: deserialize straight into `PolicySection`
                // instead of hand-picking keys off the raw table. That
                // struct carries `#[serde(deny_unknown_fields)]`, so a
                // misspelled or non-existent key (`slize_size`, `min_copies`
                // -- the latter belongs to an archive_set, never a unit
                // dotfile) is refused by name here, at the highest-priority
                // layer, instead of silently deferring upward forever
                // (ADR-0012). This is the SAME struct `read_dotfile` uses,
                // so the two paths can never disagree about what a
                // dotfile's `[policy]` table may contain.
                let section: crate::unit::dotfile::PolicySection = pol_value
                    .clone()
                    .try_into()
                    .map_err(|e: toml::de::Error| TapectlError::PolicyUnresolvable {
                        layer: PolicyLayer::Dotfile,
                        detail: format!(
                            "unit \"{}\" has an invalid [policy] section in {} ({e})",
                            unit.name,
                            dotfile_path.display()
                        ),
                    })?;

                // Issue #213: `checksum_mode`/`compression` used to be
                // assigned straight through with no validation, so a typo
                // surfaced hours later as a raw `dar` failure (compression)
                // or an opaque SQLite CHECK-constraint rejection (checksum
                // mode) at stage time. Closed-set-validated here, at the
                // same boundary `config.toml`'s `[defaults]` and
                // `archive-set create/edit` already use the same two
                // validators for (ADR-0012) -- one rule stated once, not
                // three times.
                if let Some(v) = section.checksum_mode {
                    crate::config::validate_checksum_mode(&v).map_err(|e| {
                        TapectlError::PolicyUnresolvable {
                            layer: PolicyLayer::Dotfile,
                            detail: format!(
                                "unit \"{}\" has an invalid [policy] checksum_mode in {} ({e})",
                                unit.name,
                                dotfile_path.display()
                            ),
                        }
                    })?;
                    policy.checksum_mode = v;
                }
                if let Some(v) = section.compression {
                    crate::config::validate_compression(&v).map_err(|e| {
                        TapectlError::PolicyUnresolvable {
                            layer: PolicyLayer::Dotfile,
                            detail: format!(
                                "unit \"{}\" has an invalid [policy] compression in {} ({e})",
                                unit.name,
                                dotfile_path.display()
                            ),
                        }
                    })?;
                    policy.compression = v;
                }
                if let Some(v) = section.slice_size {
                    policy.slice_size = crate::staging::parse_size_to_bytes(&v).map_err(|e| {
                        TapectlError::PolicyUnresolvable {
                            layer: PolicyLayer::Dotfile,
                            detail: format!(
                                "unit \"{}\" has an invalid [policy] slice_size in {} ({e})",
                                unit.name,
                                dotfile_path.display()
                            ),
                        }
                    })?;
                }
                // ADR-0006 / issue #73. Absent means "defer to the archive
                // set", so there is deliberately no default filled in
                // anywhere on the way in (issue #92 -- a filled default is
                // indistinguishable from an operator choice and would
                // silently outrank the archive set).
                if let Some(v) = section.warehouse_copies {
                    policy.warehouse_copies = v;
                }
            }
        }
    }

    Ok(policy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::Unit;
    use tempfile::TempDir;

    fn fresh_conn() -> Connection {
        // Full ordered migration chain (issue #44) — was a hand-applied
        // 001-only snapshot.
        crate::db::open_memory().unwrap()
    }

    fn make_unit(archive_set_id: Option<i64>, path: Option<String>) -> Unit {
        Unit {
            id: 1,
            uuid: "u".into(),
            name: "unit".into(),
            tenant_id: 1,
            archive_set_id,
            current_path: path,
            checksum_mode: "mtime_size".into(),
            encrypt: true,
            status: "active".into(),
            created_at: "2026-01-01".into(),
            last_scanned: None,
            notes: None,
        }
    }

    #[test]
    fn resolve_defaults_when_no_archive_set_no_dotfile() {
        let conn = fresh_conn();
        let config = Config::default();
        let unit = make_unit(None, None);

        let p = resolve(&conn, &config, &unit).unwrap();
        assert_eq!(p.min_copies, 2);
        assert_eq!(p.checksum_mode, "mtime_size");
        assert_eq!(p.compression, "none");
        assert!(p.encrypt);
        assert!(p.required_locations.is_empty());
        assert!(p.verify_interval_days.is_none());
    }

    #[test]
    fn resolve_archive_set_overrides_defaults() {
        let conn = fresh_conn();
        conn.execute(
            "INSERT INTO archive_sets (name, min_copies, required_locations, compression, checksum_mode, verify_interval_days)
             VALUES ('media', 3, '[\"home\",\"offsite\"]', 'lzma', 'sha256', 90)",
            [],
        )
        .unwrap();
        let as_id = conn.last_insert_rowid();

        let config = Config::default();
        let unit = make_unit(Some(as_id), None);
        let p = resolve(&conn, &config, &unit).unwrap();

        assert_eq!(p.min_copies, 3);
        assert_eq!(p.compression, "lzma");
        assert_eq!(p.checksum_mode, "sha256");
        assert_eq!(p.required_locations, vec!["home", "offsite"]);
        assert_eq!(p.verify_interval_days, Some(90));
    }

    #[test]
    fn resolve_archive_set_null_fields_inherit_defaults() {
        let conn = fresh_conn();
        // Only min_copies set; everything else NULL must fall through to defaults.
        conn.execute(
            "INSERT INTO archive_sets (name, min_copies) VALUES ('partial', 4)",
            [],
        )
        .unwrap();
        let as_id = conn.last_insert_rowid();

        let config = Config::default();
        let unit = make_unit(Some(as_id), None);
        let p = resolve(&conn, &config, &unit).unwrap();

        assert_eq!(p.min_copies, 4);
        assert_eq!(p.compression, "none"); // from defaults
        assert_eq!(p.checksum_mode, "mtime_size"); // from defaults
    }

    #[test]
    fn resolve_dotfile_overrides_archive_set() {
        let conn = fresh_conn();
        conn.execute(
            "INSERT INTO archive_sets (name, compression, checksum_mode)
             VALUES ('media', 'lzma', 'sha256')",
            [],
        )
        .unwrap();
        let as_id = conn.last_insert_rowid();

        let tmp = TempDir::new().unwrap();
        let unit_path = tmp.path().to_str().unwrap().to_string();
        std::fs::write(
            tmp.path().join(".tapectl-unit.toml"),
            r#"
[policy]
checksum_mode = "sha256_on_archive"
compression = "gzip"
slice_size = "500M"
"#,
        )
        .unwrap();

        let config = Config::default();
        let unit = make_unit(Some(as_id), Some(unit_path));
        let p = resolve(&conn, &config, &unit).unwrap();

        // Dotfile wins. `sha256_on_archive` is deliberately a DIFFERENT
        // valid value from the archive set's `sha256` (issue #213 closed-set
        // validation would now reject the old placeholder `"full_hash"`,
        // which was never a real checksum_mode to begin with).
        assert_eq!(p.checksum_mode, "sha256_on_archive");
        assert_eq!(p.compression, "gzip");
        assert_eq!(p.slice_size, 500 * 1024 * 1024);
    }

    /// ADR-0006 / issue #73: `warehouse_copies` resolves through the same
    /// three layers as every other knob, and each layer only speaks when
    /// it has actually been set.
    #[test]
    fn warehouse_copies_resolves_dotfile_over_archive_set_over_defaults() {
        let conn = fresh_conn();
        let config = Config::default();

        // Layer 3: system default, nothing else set.
        let p = resolve(&conn, &config, &make_unit(None, None)).unwrap();
        assert_eq!(p.warehouse_copies, 0, "default is opt-in, i.e. none");

        // Layer 2: archive set.
        conn.execute(
            "INSERT INTO archive_sets (name, warehouse_copies) VALUES ('core', 2)",
            [],
        )
        .unwrap();
        let as_id = conn.last_insert_rowid();
        let p = resolve(&conn, &config, &make_unit(Some(as_id), None)).unwrap();
        assert_eq!(p.warehouse_copies, 2);

        // A NULL archive-set column must defer, not read as 0.
        conn.execute(
            "INSERT INTO archive_sets (name, min_copies) VALUES ('silent', 3)",
            [],
        )
        .unwrap();
        let silent = conn.last_insert_rowid();
        let mut cfg = Config::default();
        cfg.defaults.warehouse_copies = 1;
        let p = resolve(&conn, &cfg, &make_unit(Some(silent), None)).unwrap();
        assert_eq!(
            p.warehouse_copies, 1,
            "NULL means defer to the next layer, never 0"
        );

        // Layer 1: the unit dotfile outranks the archive set.
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".tapectl-unit.toml"),
            "[policy]\nwarehouse_copies = 4\n",
        )
        .unwrap();
        let unit = make_unit(Some(as_id), Some(tmp.path().to_str().unwrap().to_string()));
        let p = resolve(&conn, &config, &unit).unwrap();
        assert_eq!(p.warehouse_copies, 4);
    }

    /// Issue #105. This test previously asserted that a dangling
    /// `archive_set_id` **falls back to defaults**, which is the exact
    /// silent downgrade #105 exists to remove. Its own comment shows the
    /// real intent was panic-safety ("resolver must not panic") — an `Err`
    /// satisfies that just as well as an `Ok`, so the assertion is
    /// tightened rather than the behavior loosened: a unit pointed at an
    /// archive set that is not there must be REPORTED, not quietly handed
    /// the weaker system defaults it was deliberately moved off.
    #[test]
    fn resolve_dangling_archive_set_id_errors_instead_of_downgrading() {
        let conn = fresh_conn();
        let config = Config::default();
        let unit = make_unit(Some(999), None);

        let err = resolve(&conn, &config, &unit)
            .expect_err("a dangling archive_set_id must not resolve to system defaults");
        let msg = err.to_string();
        assert!(
            msg.contains("999") && msg.contains("archive_set_id"),
            "the error must name the dangling id so an operator can act on it, \
             not propagate rusqlite's bare \"Query returned no rows\"; got: {msg}"
        );
    }

    /// The absent-vs-present split, which is the one thing #105 must NOT
    /// change: a unit with no dotfile defers upward in silence (#92). If
    /// this ever starts erroring, every unit without a dotfile fails.
    #[test]
    fn resolve_with_no_dotfile_present_is_silent_not_an_error() {
        let conn = fresh_conn();
        let config = Config::default();
        let tmp = TempDir::new().unwrap();
        let unit = make_unit(None, Some(tmp.path().to_str().unwrap().to_string()));

        let p = resolve(&conn, &config, &unit).expect("an absent dotfile defers upward");
        assert_eq!(
            p.min_copies,
            config.defaults.min_copies_for_tape_only as i64
        );
    }

    /// A dotfile that is PRESENT but not valid TOML is corruption, and the
    /// resolver must say so rather than handing back system defaults.
    #[test]
    fn resolve_with_a_malformed_dotfile_errors() {
        let conn = fresh_conn();
        let config = Config::default();
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".tapectl-unit.toml"),
            "[policy\nthis is not valid toml = = =\n",
        )
        .unwrap();
        let unit = make_unit(None, Some(tmp.path().to_str().unwrap().to_string()));

        let err = resolve(&conn, &config, &unit)
            .expect_err("a malformed dotfile must not resolve to system defaults");
        let msg = err.to_string();
        assert!(
            msg.contains(".tapectl-unit.toml"),
            "the error must name the file the operator has to fix; got: {msg}"
        );
    }

    /// A corrupt `required_locations` used to yield an EMPTY vec — i.e. "no
    /// locations required" — the same silent downgrade in a third place
    /// (not named by issue #105). Every writer stores a `serde_json`
    /// array or NULL, so an unparseable non-NULL value is only ever
    /// corruption.
    #[test]
    fn resolve_with_corrupt_required_locations_errors_instead_of_requiring_none() {
        let conn = fresh_conn();
        let config = Config::default();
        conn.execute(
            "INSERT INTO archive_sets (name, required_locations) VALUES ('broken', ?1)",
            params!["{not json at all"],
        )
        .unwrap();
        let as_id = conn.last_insert_rowid();
        let unit = make_unit(Some(as_id), None);

        let err = resolve(&conn, &config, &unit).expect_err(
            "corrupt required_locations must not silently resolve to \"none required\"",
        );
        assert!(err.to_string().contains("required_locations"), "got: {err}");
    }

    /// Issue #211: `PolicySection`'s `#[serde(deny_unknown_fields)]` (via
    /// `policy::resolve`'s dotfile layer) must refuse a misspelled or
    /// nonexistent `[policy]` key by name -- not silently ignore it and
    /// defer upward to the archive set/defaults as though the operator had
    /// never written it.
    #[test]
    fn resolve_rejects_an_unknown_dotfile_policy_key() {
        let conn = fresh_conn();
        let config = Config::default();
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".tapectl-unit.toml"),
            "[policy]\nslize_size = \"500M\"\n",
        )
        .unwrap();
        let unit = make_unit(None, Some(tmp.path().to_str().unwrap().to_string()));

        let err = resolve(&conn, &config, &unit).expect_err(
            "a misspelled [policy] key must not silently defer to the archive set/defaults \
             (issue #211) -- it must be refused by name",
        );
        let msg = err.to_string();
        assert!(
            msg.contains(".tapectl-unit.toml"),
            "the error must name the file the operator has to fix; got: {msg}"
        );
    }

    /// Issue #211, the `min_copies` variant named explicitly in the
    /// evidence: a real archive_set-level field, plausible to copy into a
    /// unit dotfile by mistake, is not a legal `[policy]` key there either
    /// and must be refused the same way.
    #[test]
    fn resolve_rejects_min_copies_in_a_dotfile_policy_table() {
        let conn = fresh_conn();
        let config = Config::default();
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".tapectl-unit.toml"),
            "[policy]\nmin_copies = 5\n",
        )
        .unwrap();
        let unit = make_unit(None, Some(tmp.path().to_str().unwrap().to_string()));

        resolve(&conn, &config, &unit).expect_err(
            "min_copies is an archive_set field, not a unit dotfile [policy] key -- it \
             must be refused, not silently ignored (issue #211)",
        );
    }

    /// The rule #211 must NOT break (issue #92): a dotfile can be PRESENT
    /// with a `[policy]` table that sets some keys and leaves others
    /// completely absent. An absent key is a legitimate choice not to
    /// override anything, and must keep deferring upward in silence --
    /// unknown-key rejection must never be confused with "this field was
    /// never set." This is the regression guard that matters most: if it
    /// starts failing, the fix has resurrected the #92 bug one field at a
    /// time.
    #[test]
    fn resolve_leaves_absent_dotfile_policy_keys_deferring_upward() {
        let conn = fresh_conn();
        conn.execute(
            "INSERT INTO archive_sets (name, compression, checksum_mode, warehouse_copies) \
             VALUES ('media', 'lzma', 'sha256', 3)",
            [],
        )
        .unwrap();
        let as_id = conn.last_insert_rowid();

        let tmp = TempDir::new().unwrap();
        // Sets ONLY compression. checksum_mode, slice_size and
        // warehouse_copies are all absent from this table and must defer to
        // the archive set, not error and not silently fill a default.
        std::fs::write(
            tmp.path().join(".tapectl-unit.toml"),
            "[policy]\ncompression = \"gzip\"\n",
        )
        .unwrap();

        let config = Config::default();
        let unit = make_unit(Some(as_id), Some(tmp.path().to_str().unwrap().to_string()));
        let p = resolve(&conn, &config, &unit).expect(
            "a dotfile present with SOME [policy] keys set and others left absent must \
             still resolve -- an absent key is a legitimate silence, not an error (#92)",
        );

        assert_eq!(
            p.compression, "gzip",
            "the key that WAS set in the dotfile wins (dotfile > archive_set)"
        );
        assert_eq!(
            p.checksum_mode, "sha256",
            "the ABSENT checksum_mode key must defer to the archive set, not error and \
             not silently pick a filled-in default"
        );
        assert_eq!(
            p.warehouse_copies, 3,
            "the ABSENT warehouse_copies key must defer to the archive set too"
        );
    }

    /// Issue #213: an invalid `[policy] compression` must be refused at
    /// resolve time -- the same closed-set validation ADR-0012 already
    /// requires for `config.toml [defaults]` and `archive-set create/edit`
    /// -- rather than assigned straight through to surface hours later as a
    /// raw `dar -z` failure mid-`stage create`.
    #[test]
    fn resolve_rejects_an_invalid_dotfile_compression_value() {
        let conn = fresh_conn();
        let config = Config::default();
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".tapectl-unit.toml"),
            "[policy]\ncompression = \"banana\"\n",
        )
        .unwrap();
        let unit = make_unit(None, Some(tmp.path().to_str().unwrap().to_string()));

        let err = resolve(&conn, &config, &unit).expect_err(
            "an invalid [policy] compression must be refused at resolve time (issue #213), \
             not assigned straight through to surface later as a raw dar failure",
        );
        assert!(err.to_string().contains("compression"), "got: {err}");
    }

    /// Issue #213, the `checksum_mode` half: today this surfaces as an
    /// opaque SQLite CHECK-constraint rejection when the unit is finally
    /// written with it -- ADR-0012's other named motivating case.
    #[test]
    fn resolve_rejects_an_invalid_dotfile_checksum_mode_value() {
        let conn = fresh_conn();
        let config = Config::default();
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".tapectl-unit.toml"),
            "[policy]\nchecksum_mode = \"not_a_real_mode\"\n",
        )
        .unwrap();
        let unit = make_unit(None, Some(tmp.path().to_str().unwrap().to_string()));

        let err = resolve(&conn, &config, &unit).expect_err(
            "an invalid [policy] checksum_mode must be refused at resolve time (issue #213), \
             not assigned straight through to surface later as an opaque CHECK-constraint \
             failure",
        );
        assert!(err.to_string().contains("checksum_mode"), "got: {err}");
    }
}
