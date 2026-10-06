//! Advisory scan for config keys that are parsed but not consumed by any
//! code — the `#92`/`#50` precedent (`docs/design-errata.md`): surface a
//! dead knob and never change `config check`'s exit code.
//!
//! **One key is reported today: `dirty_on_metadata_change`** (issue #347),
//! when it is `true` in `[defaults]`, in an `[[archive_sets]]` table, or on
//! an `archive_sets` row. `policy::resolve` resolves it and nothing reads
//! the result: dirty detection (`unit::content_match`, shared by `collection
//! sync/status`, `unit status --dirty`, `report dirty`, `audit` and
//! `mark-tape-only`'s guard) compares each file's path, size and mtime
//! only, so a metadata-only change never marks a unit dirty. `false` — the
//! default — describes exactly that, so only `true` is reported; flagging
//! the default would put a note on every fresh config.
//!
//! Be clear about what an empty result means: `scan` is a hand-maintained
//! match, not a detector. It only ever reports a key someone has already
//! noticed and added an arm for; it cannot discover an unwired key by
//! inspecting `Config` itself. The 2026-09-13 post-redesign review
//! (`docs/audits/`) found this the hard way — `scan`'s own doc comment
//! claimed "every decorative-key occurrence in a loaded config" while SIX
//! keys sat in `Config` with no reader and zero of them were listed here.
//! Three (below) were spec W4's; issue #172 found the other three:
//! `packing.strategy`, `packing.fill_threshold` and `defaults.hash` join
//! the delete list below. The audit's count of six also included
//! `logging.level`/`logging.format` — issue #172 WIRES those two instead of
//! deleting them. Issue #347 found four more parsed-but-inert keys and ran
//! the same delete-or-wire decision on each: `preserve_xattrs` and
//! `preserve_fsa` were WIRED (`dar::create`: `false` now drops extended
//! attributes / filesystem attributes), `preserve_acls` stays the ratified
//! visible no-op (`policy::subsumed`), and `dirty_on_metadata_change` — whose
//! wiring belongs to dirty detection, not config — is the one reported here
//! until it is wired or deleted.
//!
//! Every key this module was originally built for is GONE from `Config`
//! altogether
//! (spec W4, operator decision 2026-09-13, extended by issue #172 —
//! tapectl has never been used in production, so a knob that does nothing
//! should be deleted rather than documented forever). They were deleted
//! rather than demoted because in each case the thing the knob claimed to
//! control is not merely unimplemented, it is decided somewhere else and
//! cannot move:
//!
//! - `backends.lto[].block_size` — the write path's block size is a FORMAT
//!   CONSTANT (512 KiB: `collection::plan::BLOCK_SIZE`,
//!   `cli::volume::DEFAULT_BLOCK_SIZE`, `docs/design/volume-format-v2.md`
//!   §1/D7). `src/volume/layout.rs` bakes it into the on-tape recovery text
//!   an heir reads, so a per-drive value could only ever disagree with the
//!   tape it was written to.
//! - `backends.lto[].hardware_compression` — `TapeStore::open`
//!   (`src/store.rs`) calls `dev.disable_compression()` unconditionally on
//!   every write-path open, confirmed landing on real LTO-6 hardware
//!   (`DCE` 1→0) in `docs/lto6-session-journal-2026-09-10.md`. The knob
//!   could never re-enable compression; a `true` value was silently ignored.
//! - `packing.min_free_for_append` — append is rejected outright
//!   (ADR-0003); there is no append path for this knob to gate.
//! - `packing.strategy` (issue #172) — the real batch selector is
//!   alphabetical first-fit (`src/collection/`), not a configurable
//!   best-fit-decreasing strategy.
//! - `packing.fill_threshold` (issue #172) — no bin-packing code ever
//!   consulted a fill threshold.
//! - `labels.format` (issue #172) — volume labels are always
//!   operator-supplied (`--label`), never generated from a template.
//! - `defaults.hash` (issue #172) — every checksum tapectl computes is
//!   sha256, hardcoded; `checksum_mode` is the real knob, and it governs
//!   WHEN a checksum runs, never which algorithm.
//!
//! A config file still carrying any of these is now REJECTED by name, with
//! the reason, by `config::stale_lto_fields_message` — a sharper answer
//! than an advisory note, and the reason deleting them is not a loss of
//! operator-facing surface.

use rusqlite::Connection;

use crate::config::Config;

/// One config key that is parsed but has no reader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecorativeHit {
    /// Dotted key path, e.g. `"defaults.dirty_on_metadata_change"` or
    /// `"archive_sets[\"media\"].dirty_on_metadata_change"`.
    pub key: String,
}

/// Every decorative-key occurrence someone has already added an arm for —
/// NOT every decorative-key occurrence in a loaded config (see the module
/// doc: this cannot discover an unwired key by inspecting `Config`).
///
/// Reads `[defaults]`, each `[[archive_sets]]` table, and the
/// `archive_sets` rows — the rows are what `policy::resolve` actually
/// reads, and `archive-set create/edit` can set the column without the
/// TOML. An archive set named in both is reported once.
pub fn scan(config: &Config, conn: &Connection) -> Vec<DecorativeHit> {
    let mut out = Vec::new();
    if config.defaults.dirty_on_metadata_change {
        out.push(DecorativeHit {
            key: "defaults.dirty_on_metadata_change".to_string(),
        });
    }

    let mut sets: Vec<String> = config
        .archive_sets
        .iter()
        .filter(|set| set.dirty_on_metadata_change == Some(true))
        .map(|set| set.name.clone())
        .collect();
    // A missing table (fresh DB) is not an error for an advisory scan.
    if let Ok(mut stmt) = conn
        .prepare("SELECT name FROM archive_sets WHERE dirty_on_metadata_change = 1 ORDER BY name")
    {
        if let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(0)) {
            for name in rows.flatten() {
                if !sets.contains(&name) {
                    sets.push(name);
                }
            }
        }
    }
    out.extend(sets.into_iter().map(|name| DecorativeHit {
        key: format!("archive_sets[\"{name}\"].dirty_on_metadata_change"),
    }));
    out
}

/// The advisory line for one hit. Pure, so the wording is testable without
/// a `Config` and so `config check`'s `--json` and text arms can never
/// drift apart.
pub fn describe(hit: &DecorativeHit) -> String {
    if hit.key.ends_with("dirty_on_metadata_change") {
        format!(
            "note: {} = true is parsed but not consumed — dirty detection compares each \
             file's path, size and modification time only, so a change to permissions, \
             ownership or extended attributes alone never marks a unit dirty.",
            hit.key
        )
    } else {
        format!("note: {} is parsed but not consumed.", hit.key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LtoBackendConfig;

    fn backend(name: &str) -> LtoBackendConfig {
        LtoBackendConfig {
            name: name.to_string(),
            device_tape: "/dev/nst0".to_string(),
            device_sg: "/dev/sg0".to_string(),
            generation: "LTO-8".to_string(),
            capacity_override: Some("2.5T".to_string()),
            fill_ceiling: 0.92,
            enospc_buffer: "50M".to_string(),
        }
    }

    fn no_db() -> Connection {
        Connection::open_in_memory().unwrap()
    }

    /// The default config has no decorative keys: `dirty_on_metadata_change`
    /// defaults to `false`, which describes what dirty detection does.
    #[test]
    fn a_default_config_reports_nothing() {
        let config = Config::default();
        assert!(scan(&config, &no_db()).is_empty());
    }

    /// Spec W4 deleted both per-backend subjects of this module; a
    /// multi-backend config is the case that would still show them.
    #[test]
    fn backends_no_longer_contribute_any_hits() {
        let mut config = Config::default();
        config.backends.lto.push(backend("lto1"));
        config.backends.lto.push(backend("lto2"));
        assert!(scan(&config, &no_db()).is_empty());
    }

    /// Issue #347: `dirty_on_metadata_change = true` is resolved and read
    /// by nothing, so `config check` names it wherever it is set — once per
    /// archive set even when the TOML and the table both carry it.
    #[test]
    fn dirty_on_metadata_change_true_is_named_at_every_layer() {
        let mut config = Config::default();
        config.defaults.dirty_on_metadata_change = true;
        config.archive_sets.push(crate::config::ArchiveSetConfig {
            name: "media".to_string(),
            min_copies: None,
            required_locations: None,
            encrypt: None,
            compression: None,
            checksum_mode: None,
            verify_interval_days: None,
            slice_size: None,
            preserve_xattrs: None,
            preserve_acls: None,
            preserve_fsa: None,
            dirty_on_metadata_change: Some(true),
        });
        let conn = crate::db::open_memory().unwrap();
        for (name, dirty) in [("media", 1), ("docs", 1), ("cold", 0)] {
            conn.execute(
                "INSERT INTO archive_sets (name, dirty_on_metadata_change) VALUES (?1, ?2)",
                rusqlite::params![name, dirty],
            )
            .unwrap();
        }

        let keys: Vec<String> = scan(&config, &conn).into_iter().map(|h| h.key).collect();
        assert_eq!(
            keys,
            vec![
                "defaults.dirty_on_metadata_change".to_string(),
                "archive_sets[\"media\"].dirty_on_metadata_change".to_string(),
                "archive_sets[\"docs\"].dirty_on_metadata_change".to_string(),
            ]
        );
        let line = describe(&DecorativeHit {
            key: keys[0].clone(),
        });
        assert!(line.contains("not consumed"), "{line}");
        assert!(line.contains("dirty detection"), "{line}");
    }

    /// The mechanism still works — this is what a future unwired key gets.
    #[test]
    fn describe_names_the_key_and_says_it_is_unconsumed() {
        let hit = DecorativeHit {
            key: "backends.lto[\"lto1\"].some_future_knob".to_string(),
        };
        let line = describe(&hit);
        assert!(line.contains("some_future_knob"), "{line}");
        assert!(line.contains("not consumed"), "{line}");
    }
}
