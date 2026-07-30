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
/// never scales"). `LtoBackendConfig.block_size` is NOT this figure — that
/// field is unread anywhere in the write path (dead config; T10 leaves it
/// alone per the "decorative keys" R&D-exit note). Every real call site
/// hardcodes 512 KiB (see `cli::volume::DEFAULT_BLOCK_SIZE`); this mirrors
/// that rather than reading the unread field.
const BLOCK_SIZE: u64 = 512 * 1024;

/// Compute one collection's batches against its resolved LTO backend capacity.
pub fn plan_for_collection(
    conn: &Connection,
    config: &Config,
    lib: &CollectionConfig,
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

    let backend = config
        .backends
        .lto
        .first()
        .ok_or_else(|| TapectlError::Config("no LTO backend configured".into()))?;
    let nominal = crate::staging::parse_size_to_bytes(&backend.nominal_capacity).max(0) as u64;
    let usable = (nominal as f64 * backend.usable_capacity_factor) as u64;
    let enospc_buffer = crate::staging::parse_size_to_bytes(&backend.enospc_buffer).max(0) as u64;
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
            media_type: "LTO-6".into(),
            nominal_capacity: "10M".into(),
            usable_capacity_factor: 1.0,
            enospc_buffer: "0".into(),
            block_size: "512K".into(),
            hardware_compression: false,
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
        let batches = plan_for_collection(&conn, &config, &lib).unwrap();
        assert_eq!(batches.len(), 1, "two 3 MiB units must fit one 10 MiB tape");
        assert_eq!(
            batches[0].unit_names(),
            vec!["testlib/alpha", "testlib/beta"]
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
        let err = plan_for_collection(&conn, &config, &lib).unwrap_err();
        assert!(
            err.to_string().contains("testlib/huge"),
            "error must name the offending unit: {err}"
        );
    }
}
