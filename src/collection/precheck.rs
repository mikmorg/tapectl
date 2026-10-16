//! The two catalog-only refusals `volume write` makes before it touches a
//! backend, a MAM or the drive, which `collection run`'s destination check
//! must make too (issue #475).
//!
//! `collection run` stages a whole batch, hours of dar plus age and a tape's
//! worth of staging disk, before it calls `volume write`. Every refusal
//! `volume write` can make from the catalog alone is therefore made again in
//! [`super::plan::destination_budget`], before a single unit is staged (the
//! late refusal issue #224 removed). `destination_budget` already applied the
//! status and recorded-write halves; these are the other two:
//!
//! - an unresolved write session (`planned`/`in_progress`/`interrupted`): a
//!   retry after an interrupted write is `volume resume`'s job, never a fresh
//!   write ([`unresolved_write_sessions`]);
//! - a recorded seal (`volumes.sealed_at`): ADR-0003, ADR-0012 2026-09-21
//!   (issue #330), absolute, `--force` is not consulted ([`seal_recorded`]).
//!
//! The predicates and the refusal texts live in this one module so the two
//! entry points can name one fact the same way. `volume::write::volume_write`
//! still carries its own inline copies, because `volume/write.rs` was not
//! this change's to edit; `tests::the_run_refusals_are_volume_writes_own`
//! drives both entry points over one catalog and fails if the texts drift.
//! The coordinator hunk for `volume_write` to call these instead is in the
//! issue #475 report.

use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

use crate::error::Result;
use crate::volume::{build, session};

/// How many write sessions of `volume_id` are unresolved
/// (`planned`/`in_progress`/`interrupted`). Non-zero means `volume write`
/// refuses the volume and `volume resume` is the way on.
pub fn unresolved_write_sessions(conn: &Connection, volume_id: i64) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM writes WHERE volume_id = ?1 AND status IN ('planned','in_progress','interrupted')",
        params![volume_id],
        |r| r.get(0),
    )?)
}

/// The refusal for [`unresolved_write_sessions`] being non-zero: the text
/// `volume write` gives, so the operator reads one remedy whichever command
/// found the session.
pub fn unresolved_session_refusal(label: &str, volume_id: i64) -> String {
    format!(
        "volume \"{label}\" already has an unresolved write session \
         (status planned/in_progress/interrupted). If it was interrupted, reload the \
         same cartridge and run `tapectl volume resume {label}` — it continues that \
         session from its frozen staging files rather than rebuilding it. Otherwise \
         inspect the `writes`/`write_positions` rows for volume_id {volume_id}."
    )
}

/// Whether the catalog RECORDS this volume's seal (`volumes.sealed_at`), a
/// fact and not an inference from the tape (issue #330).
pub fn seal_recorded(conn: &Connection, volume_id: i64) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT sealed_at IS NOT NULL FROM volumes WHERE id = ?1",
        params![volume_id],
        |r| r.get(0),
    )?)
}

/// The refusal for a recorded seal: the text `volume write` gives. It names
/// `volume verify` then `volume resume` only when the aborted session is
/// adoptable after a clean full verify
/// ([`session::aborted_session_reconfirmable_after_verify`]) AND its frozen
/// layout is still on disk; the retire path is named always.
pub fn seal_recorded_refusal(conn: &Connection, label: &str, volume_id: i64) -> Result<String> {
    let reconfirmable = session::aborted_session_reconfirmable_after_verify(conn, volume_id)? && {
        let dir: Option<String> = conn
            .query_row(
                "SELECT session_dir FROM writes WHERE volume_id = ?1 AND status = 'aborted' \
                 AND session_dir IS NOT NULL ORDER BY id LIMIT 1",
                params![volume_id],
                |r| r.get(0),
            )
            .optional()?;
        dir.is_some_and(|d| Path::new(&d).join(build::LAYOUT_SIDECAR).is_file())
    };
    let mut msg = format!(
        "refusing to write volume \"{label}\": the catalog RECORDS its seal (a write session \
         sealed this cartridge; `volumes.sealed_at` is set). ADR-0003: sealed volumes are \
         immutable, there is no append, and --force cannot override this. The tape's own seal \
         marker may not read back, but the seal is recorded, not inferred (ADR-0012)."
    );
    if reconfirmable {
        msg.push_str(&format!(
            " Its aborted session can still be re-confirmed without writing: run a full verify \
             (`tapectl volume verify {label}`), then `tapectl volume resume {label}`."
        ));
    }
    msg.push_str(&format!(
        " To reuse the cartridge instead: retire this volume (`tapectl volume retire {label}`), \
         then `tapectl cartridge erase`, which erases the tape in the drive, reads it back and \
         records it — or {} yourself and run `tapectl cartridge mark-erased` \
         before writing to it again. {}",
        crate::volume::write::ERASE_IN_THE_DRIVE,
        crate::volume::write::NEVER_DEGAUSS,
    ));
    Ok(msg)
}

/// Both refusals in `volume write`'s order (the unresolved session first, so
/// an `interrupted` sealed session still gets resume's advice): `None` when
/// the catalog lets the volume through to the tape's own checks.
pub fn refusal(conn: &Connection, label: &str, volume_id: i64) -> Result<Option<String>> {
    if unresolved_write_sessions(conn, volume_id)? > 0 {
        return Ok(Some(unresolved_session_refusal(label, volume_id)));
    }
    if seal_recorded(conn, volume_id)? {
        return Ok(Some(seal_recorded_refusal(conn, label, volume_id)?));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, TapectlPaths};
    use crate::db;

    /// `L1`, `initialized`, with one write session of `write_status` and its
    /// seal recorded or not.
    fn catalog(write_status: &str, sealed: bool) -> (Connection, i64) {
        let conn = db::open_memory().unwrap();
        conn.execute_batch(
            "INSERT INTO tenants (id, name, is_operator, status) VALUES (1, 'media', 0, 'active');
             INSERT INTO units (id, uuid, name, tenant_id, checksum_mode, encrypt, status)
                 VALUES (1, 'u-old', 'old', 1, 'mtime_size', 1, 'active');
             INSERT INTO snapshots (id, unit_id, version, status, source_path, file_count, total_size)
                 VALUES (1, 1, 1, 'staged', '/tmp', 1, 10);
             INSERT INTO stage_sets (id, snapshot_id, status, slice_size) VALUES (1, 1, 'staged', 524288);
             INSERT INTO volumes (id, label, backend_type, backend_name, capacity_bytes, status)
                 VALUES (1, 'L1', 'lto', 'p', 10485760, 'initialized');",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
             VALUES (1, 1, 1, ?1)",
            params![write_status],
        )
        .unwrap();
        if sealed {
            conn.execute(
                "UPDATE volumes SET sealed_at = datetime('now') WHERE id = 1",
                [],
            )
            .unwrap();
        }
        (conn, 1)
    }

    /// What `volume write` itself says of the catalog, with no device behind
    /// it: refused before the device is named.
    fn volume_write_says(conn: &Connection) -> String {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = TapectlPaths::new(tmp.path().join("home"));
        crate::volume::write::volume_write(
            conn,
            &paths,
            &Config::default(),
            "L1",
            "/nonexistent/tapectl-precheck-test-nst",
            512 * 1024,
            false,
            false,
            false,
            false,
            true,
        )
        .unwrap_err()
        .to_string()
    }

    /// The point of the module (issue #475): `collection run` and `volume
    /// write` refuse a volume in the same words. Driven over the same
    /// catalog through both entry points, for every refusal this module
    /// makes, so a change to one text without the other fails here.
    #[test]
    fn the_run_refusals_are_volume_writes_own() {
        for (status, sealed) in [
            ("planned", false),
            ("in_progress", false),
            ("interrupted", false),
            ("interrupted", true),
            ("aborted", true),
        ] {
            let (conn, volume_id) = catalog(status, sealed);
            let ours = refusal(&conn, "L1", volume_id)
                .unwrap()
                .unwrap_or_else(|| panic!("{status}/sealed={sealed}: not refused"));
            assert_eq!(
                ours,
                volume_write_says(&conn),
                "{status}/sealed={sealed}: the two refusals drifted"
            );
        }
    }

    /// The positive control for the test above: a resolved, unsealed volume
    /// is no refusal of this module's, and `volume write` does not name
    /// either of them for it.
    #[test]
    fn a_resolved_unsealed_volume_passes_both_checks() {
        let (conn, volume_id) = catalog("aborted", false);
        assert_eq!(refusal(&conn, "L1", volume_id).unwrap(), None);
        let theirs = volume_write_says(&conn);
        assert!(
            !theirs.contains("unresolved write session") && !theirs.contains("RECORDS its seal"),
            "{theirs}"
        );
    }
}
