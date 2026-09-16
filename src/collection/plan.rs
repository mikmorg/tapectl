//! `collection plan` (`docs/design/v2-open-questions.md` §11): batches one
//! collection's pending units against its resolved LTO backend capacity, using
//! the pure `selector::plan_batches`.
//!
//! This is the seam between "planning numbers as pure arithmetic"
//! (`selector`, drilled directly with synthetic sizes at production scale)
//! and "planning numbers as tapectl actually has them" (config's backend
//! capacity figures, and each pending unit's fresh on-disk size estimate
//! from `fingerprint`).
//!
//! Sizes here are a PREVIEW, not a commitment: a pending unit's
//! `estimated_bytes` comes from a live plaintext filesystem walk, not the
//! eventual encrypted/sliced on-tape bytes. The real, authoritative
//! capacity gate is `Layout::validate` at actual write time
//! (`docs/design/v2-implementation-plan.md` T5b) — this is advisory, for
//! review before committing to a stage/write run ("Emit batch manifests for
//! review").

use rusqlite::Connection;

use crate::config::{CollectionConfig, Config};
use crate::error::{Result, TapectlError};

use super::selector::{self, Batch};

/// The format-constant block size every write path pads against
/// (`docs/design/v2-open-questions.md` §8: "block size — format constant,
/// never scales"). There is deliberately no config knob for it:
/// `LtoBackendConfig.block_size` existed, was read by nothing, and was
/// deleted in spec W4 — `src/volume/layout.rs` bakes 512 KiB into the
/// on-tape recovery text an heir reads, so a per-drive value could only
/// ever disagree with the tape. Every real call site hardcodes it (see
/// `cli::volume::DEFAULT_BLOCK_SIZE`); this mirrors that.
const BLOCK_SIZE: u64 = 512 * 1024;

/// Compute one collection's batches against its resolved LTO backend capacity.
pub fn plan_for_collection(
    conn: &Connection,
    config: &Config,
    lib: &CollectionConfig,
    // `--generation <GEN>`: plan for a generation other than the drive's own
    // (ADR-0010) — sizing batches for LTO-5 stock in an LTO-6 drive, say.
    // `None` means the drive's native generation.
    media: Option<&str>,
    // `--device`: WHICH drive to plan against. Resolved strictly, like every
    // other write-adjacent command (ADR-0010, "Backends resolve by device").
    // Before it existed this passed `None` unconditionally, so planning
    // errored outright the moment a second drive was configured rather than
    // asking which one was meant.
    device: Option<&str>,
) -> Result<Vec<Batch>> {
    let pending = super::fingerprint::pending_units_for_collection(
        conn,
        lib,
        &config.defaults.global_excludes,
    )?;
    let synthetic: Vec<selector::PendingUnit> = pending
        .iter()
        .map(|p| selector::PendingUnit {
            name: p.unit.name.clone(),
            size_bytes: p.estimated_bytes,
        })
        .collect();

    let backend = crate::config::resolve_lto_backend(config, device)?;
    // `.max(0)` dropped (issue #59): `parse_size_to_bytes` now rejects a
    // negative value with `Err` rather than letting one flow through as a
    // valid byte count, so a successfully parsed `Ok` is already guaranteed
    // non-negative here.
    let nominal = backend.planning_capacity_bytes(media)?;
    let usable = (nominal as f64 * backend.usable_capacity_factor) as u64;
    let enospc_buffer = crate::staging::parse_size_to_bytes(&backend.enospc_buffer)? as u64;
    let budget = usable.saturating_sub(enospc_buffer);

    selector::plan_batches(synthetic, budget, BLOCK_SIZE).map_err(|oversized| {
        TapectlError::Other(format!(
            "collection \"{}\": {} unit(s) exceed the per-tape budget and can never be \
             batched (units are never split across tapes): {}",
            lib.name,
            oversized.len(),
            oversized
                .iter()
                .map(|o| o.to_string())
                .collect::<Vec<_>>()
                .join("; "),
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{LtoBackendConfig, TapectlPaths};
    use crate::db;

    fn config_with_tiny_backend() -> Config {
        let mut config = Config::default();
        config.backends.lto.push(LtoBackendConfig {
            name: "p".into(),
            device_tape: "/dev/null".into(),
            device_sg: "/dev/null".into(),
            generation: "LTO-8".into(),
            capacity_override: Some("10M".into()),
            usable_capacity_factor: 1.0,
            enospc_buffer: "0".into(),
        });
        config
    }

    #[test]
    fn plan_batches_new_units_by_estimated_on_disk_size() {
        let conn = db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('media', 0, 'active')",
            [],
        )
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        // Two ~3 MiB units — both fit a 10 MiB tape as one batch.
        for name in ["alpha", "beta"] {
            let dir = root.path().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("f.dat"), vec![0u8; 3 * 1024 * 1024]).unwrap();
        }
        let lib = CollectionConfig {
            name: "testlib".into(),
            root: root.path().to_string_lossy().to_string(),
            tenant: "media".into(),
            unit_depth: 1,
            exclude: vec![],
            archive_set: None,
            dotfiles: true,
        };
        let paths = TapectlPaths::new(home.path().to_path_buf());
        super::super::sync::sync_collection(&conn, &paths, &lib, false, &[]).unwrap();

        let config = config_with_tiny_backend();
        let batches = plan_for_collection(&conn, &config, &lib, None, None).unwrap();
        assert_eq!(batches.len(), 1, "two 3 MiB units must fit one 10 MiB tape");
        assert_eq!(
            batches[0].unit_names(),
            vec!["testlib/alpha", "testlib/beta"]
        );
    }

    /// Spec W4 / ADR-0010: `collection plan` resolved with `None`, so a
    /// second configured drive made it error outright instead of asking
    /// which one. `--device` picks, and the batch sizes follow THAT drive's
    /// capacity — not whichever backend happened to be first.
    #[test]
    fn plan_with_two_drives_sizes_batches_for_the_one_device_names() {
        let conn = db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('media', 0, 'active')",
            [],
        )
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        for name in ["alpha", "beta"] {
            let dir = root.path().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("f.dat"), vec![0u8; 3 * 1024 * 1024]).unwrap();
        }
        let lib = CollectionConfig {
            name: "testlib".into(),
            root: root.path().to_string_lossy().to_string(),
            tenant: "media".into(),
            unit_depth: 1,
            exclude: vec![],
            archive_set: None,
            dotfiles: true,
        };
        let paths = TapectlPaths::new(home.path().to_path_buf());
        super::super::sync::sync_collection(&conn, &paths, &lib, false, &[]).unwrap();

        // Two drives: a 10 MiB one (both units fit as one batch) and a 4 MiB
        // one (they cannot share a tape). The batch COUNT is the assertion,
        // so picking the wrong drive cannot pass by coincidence.
        let mut config = config_with_tiny_backend();
        config.backends.lto.push(LtoBackendConfig {
            name: "small".into(),
            device_tape: "/dev/zero".into(),
            device_sg: "/dev/null".into(),
            generation: "LTO-8".into(),
            capacity_override: Some("4M".into()),
            usable_capacity_factor: 1.0,
            enospc_buffer: "0".into(),
        });

        // Without a device, two drives is an error asking for one — not a
        // silent pick.
        let err = plan_for_collection(&conn, &config, &lib, None, None).unwrap_err();
        assert!(err.to_string().contains("--device"), "{err}");

        let big = plan_for_collection(&conn, &config, &lib, None, Some("/dev/null")).unwrap();
        assert_eq!(big.len(), 1, "10 MiB tape holds both 3 MiB units");

        let small = plan_for_collection(&conn, &config, &lib, None, Some("/dev/zero")).unwrap();
        assert_eq!(small.len(), 2, "4 MiB tape cannot hold both 3 MiB units");
    }

    /// Issue #175: `collection run` must budget against the destination
    /// volume's own `capacity_bytes` (ADR-0010), never the drive's
    /// generation. Same fixture as `config_with_tiny_backend` (10 MiB
    /// generation-planned capacity), but the destination volume itself is a
    /// 4 MiB row — a real cartridge smaller than what the drive would plan
    /// for. Two 3 MiB units: fit one 10 MiB (generation) tape, but not one
    /// 4 MiB (destination) tape. The batch COUNT is the assertion, so
    /// budgeting from the wrong source cannot pass by coincidence.
    #[test]
    fn run_budgets_against_the_destination_volume_not_the_drive() {
        let conn = db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('media', 0, 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes, status) \
             VALUES ('L1', 'lto', 'p', ?1, 'initialized')",
            [4 * 1024 * 1024_i64],
        )
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        for name in ["alpha", "beta"] {
            let dir = root.path().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("f.dat"), vec![0u8; 3 * 1024 * 1024]).unwrap();
        }
        let lib = CollectionConfig {
            name: "testlib".into(),
            root: root.path().to_string_lossy().to_string(),
            tenant: "media".into(),
            unit_depth: 1,
            exclude: vec![],
            archive_set: None,
            dotfiles: true,
        };
        let paths = TapectlPaths::new(home.path().to_path_buf());
        super::super::sync::sync_collection(&conn, &paths, &lib, false, &[]).unwrap();

        let config = config_with_tiny_backend();

        let (volume_batches, _budget) =
            plan_for_run(&conn, &config, &lib, "/dev/null", &["L1".to_string()]).unwrap();
        assert_eq!(
            volume_batches.len(),
            2,
            "a 4 MiB destination volume cannot hold both 3 MiB units in one batch"
        );

        let generation_batches = plan_for_collection(&conn, &config, &lib, None, None).unwrap();
        assert_eq!(
            generation_batches.len(),
            1,
            "the drive's 10 MiB generation-planned capacity fits both units in one batch — \
             proving the two budgets really do disagree here"
        );
    }

    /// Issue #175: `--label` repeats once per planned copy and `batch::
    /// execute_batch` stages once and writes to every label, so the batch
    /// must be sized to the SMALLEST destination, not the largest or the
    /// first one named.
    #[test]
    fn run_budgets_against_the_smallest_of_several_destinations() {
        let conn = db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('media', 0, 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes, status) \
             VALUES ('big', 'lto', 'p', ?1, 'initialized')",
            [10 * 1024 * 1024_i64],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes, status) \
             VALUES ('small', 'lto', 'p', ?1, 'initialized')",
            [4 * 1024 * 1024_i64],
        )
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        for name in ["alpha", "beta"] {
            let dir = root.path().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("f.dat"), vec![0u8; 3 * 1024 * 1024]).unwrap();
        }
        let lib = CollectionConfig {
            name: "testlib".into(),
            root: root.path().to_string_lossy().to_string(),
            tenant: "media".into(),
            unit_depth: 1,
            exclude: vec![],
            archive_set: None,
            dotfiles: true,
        };
        let paths = TapectlPaths::new(home.path().to_path_buf());
        super::super::sync::sync_collection(&conn, &paths, &lib, false, &[]).unwrap();

        let config = config_with_tiny_backend();

        let (batches, budget) = plan_for_run(
            &conn,
            &config,
            &lib,
            "/dev/null",
            &["big".to_string(), "small".to_string()],
        )
        .unwrap();
        assert_eq!(
            batches.len(),
            2,
            "the 4 MiB \"small\" destination is the binding constraint, not the 10 MiB \"big\" one"
        );
        assert_eq!(budget.binding_label, "small");
        assert_eq!(budget.binding_capacity_bytes, 4 * 1024 * 1024);
        assert_eq!(budget.num_destinations, 2);
    }

    /// Issue #175: an unknown `--label` must fail before any unit is staged
    /// — `plan_for_run` resolves the destination budget FIRST, so this
    /// never reaches `pending_units_for_collection`, let alone
    /// `batch::execute_batch`'s staging loop. Asserting `snapshots` is
    /// untouched is the check that actually proves it, since a
    /// `VolumeNotFound` returned only after staging would still look like a
    /// correct error to a test that just matched on the error variant.
    #[test]
    fn run_refuses_an_unknown_destination_label_before_staging() {
        let conn = db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('media', 0, 'active')",
            [],
        )
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let dir = root.path().join("alpha");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.dat"), vec![0u8; 3 * 1024 * 1024]).unwrap();
        let lib = CollectionConfig {
            name: "testlib".into(),
            root: root.path().to_string_lossy().to_string(),
            tenant: "media".into(),
            unit_depth: 1,
            exclude: vec![],
            archive_set: None,
            dotfiles: true,
        };
        let paths = TapectlPaths::new(home.path().to_path_buf());
        super::super::sync::sync_collection(&conn, &paths, &lib, false, &[]).unwrap();

        let config = config_with_tiny_backend();

        let before: i64 = conn
            .query_row("SELECT COUNT(*) FROM snapshots", [], |r| r.get(0))
            .unwrap();

        let err = plan_for_run(&conn, &config, &lib, "/dev/null", &["nonexistent".to_string()])
            .unwrap_err();
        assert!(
            matches!(&err, TapectlError::VolumeNotFound(l) if l == "nonexistent"),
            "{err}"
        );

        let after: i64 = conn
            .query_row("SELECT COUNT(*) FROM snapshots", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            before, after,
            "an unknown label must fail before staging ever touches snapshots"
        );
    }

    #[test]
    fn plan_refuses_a_unit_larger_than_the_whole_tape() {
        let conn = db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('media', 0, 'active')",
            [],
        )
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let dir = root.path().join("huge");
        std::fs::create_dir_all(&dir).unwrap();
        // 20 MiB unit against a 10 MiB tape (0 usable-factor loss, 0 enospc
        // buffer, per `config_with_tiny_backend`) — must refuse, not split.
        std::fs::write(dir.join("f.dat"), vec![0u8; 20 * 1024 * 1024]).unwrap();

        let lib = CollectionConfig {
            name: "testlib".into(),
            root: root.path().to_string_lossy().to_string(),
            tenant: "media".into(),
            unit_depth: 1,
            exclude: vec![],
            archive_set: None,
            dotfiles: true,
        };
        let paths = TapectlPaths::new(home.path().to_path_buf());
        super::super::sync::sync_collection(&conn, &paths, &lib, false, &[]).unwrap();

        let config = config_with_tiny_backend();
        let err = plan_for_collection(&conn, &config, &lib, None, None).unwrap_err();
        assert!(
            err.to_string().contains("testlib/huge"),
            "error must name the offending unit: {err}"
        );
    }
}
