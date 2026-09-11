//! Portable `catalog.db` subset for the operator envelope (issue #83).
//!
//! `docs/design/volume-format-v2.md` line 89 says the catalog "rides each
//! volume encrypted (the operator envelope's catalog snapshot, #83)". This
//! module is the "generate the filtered `catalog.db`" half of that: it runs
//! against the live `Connection` (which `src/volume/build.rs` deliberately
//! never sees — `build()` stays pure over `BuildInputs`), producing a small,
//! standalone SQLite file that `volume_write` (which HAS a `Connection`)
//! hands to `build()` via `BuildInputs::catalog_db_path`. `build()` appends
//! it to the OPERATOR envelope only, exactly like `PLAN.toml`.
//!
//! # Scope: this volume's write only
//!
//! `write.rs::find_staged_data` already enumerates precisely the stage_sets
//! going onto THIS write (every `stage_sets` row with `status = 'staged'`,
//! which is what becomes this write's `writes`/`write_positions` rows a few
//! lines later in `volume_write` — nothing else is staged at generation
//! time, and a second concurrent write on the same volume is refused
//! upstream). So the same `stage_set_id` list `BuildUnit`/`BuildSlice`
//! already carries is the exact scoping key here — no separate query needed
//! to "find this volume's rows", and no dependency on `writes`/
//! `write_positions` rows that do not exist yet at `build()` time (`plan()`
//! is what inserts them, and it runs after `build()`).
//!
//! Join path, from the given `stage_set_ids`:
//! `stage_sets` (given ids) → `snapshots` (`stage_sets.snapshot_id`) →
//! `units` (`snapshots.unit_id`); `stage_slices` via `stage_slices.stage_set_id`;
//! `files` via `files.snapshot_id`.
//!
//! # What it is for (settled 2026-09-11, review finding 2)
//!
//! A **complete rebuild source** for `catalog rebuild` (#136): everything the
//! operator's catalog knows about this write *at staging time* rides here —
//! including, since that decision, the `tenants` that own the units, each
//! stage set's recorded recipient list (`key_fingerprints`, the escrow
//! receipt #137 could not otherwise recover), and each slice's
//! `sha256_plain`. Tape positions are NOT here and cannot be: this file is
//! generated before `build()` assigns them, so the envelope `MANIFEST.toml`
//! stays authoritative for the slice map. Unit and snapshot *statuses* are
//! copied for the heir's reading only; a rebuild applies its own rules.
//!
//! All of it is inside an age envelope encrypted to operator + escrow. The
//! isolation invariant governs plaintext files; nothing here is plaintext.
//!
//! A `catalog.db` written before this change lacks `tenants`,
//! `key_fingerprints` and `sha256_plain`. `catalog rebuild` probes for them
//! (`PRAGMA table_info`) rather than trusting a number, and falls back to
//! the tenant envelopes and the manifest for what is missing.
//!
//! # Isolation
//!
//! The output covers every tenant's units on this write — it is appended to
//! the OPERATOR envelope only (`src/volume/build.rs`), never a tenant
//! envelope, exactly like `PLAN.toml`.

use std::path::Path;

use rusqlite::{params_from_iter, Connection};

use crate::error::Result;

/// Table/column subset carried into the snapshot: enough to answer "what
/// units/snapshots/files landed on this volume" without needing the source
/// `tapectl.db` — see `RECOVERY.md`'s "Querying `catalog.db`" section for
/// the schema as documented to the operator.
const SCHEMA: &str = "
CREATE TABLE tenants (
    id          INTEGER PRIMARY KEY,
    name        TEXT NOT NULL
);
CREATE TABLE units (
    id          INTEGER PRIMARY KEY,
    uuid        TEXT NOT NULL,
    name        TEXT NOT NULL,
    tenant_id   INTEGER NOT NULL,
    status      TEXT
);
CREATE TABLE snapshots (
    id            INTEGER PRIMARY KEY,
    unit_id       INTEGER NOT NULL,
    version       INTEGER NOT NULL,
    snapshot_type TEXT,
    source_path   TEXT,
    total_size    INTEGER,
    file_count    INTEGER
);
CREATE TABLE stage_sets (
    id                   INTEGER PRIMARY KEY,
    snapshot_id          INTEGER NOT NULL,
    slice_size           INTEGER,
    num_slices           INTEGER,
    total_dar_size       INTEGER,
    total_encrypted_size INTEGER,
    key_fingerprints     TEXT
);
CREATE TABLE stage_slices (
    id               INTEGER PRIMARY KEY,
    stage_set_id     INTEGER NOT NULL,
    slice_number     INTEGER NOT NULL,
    size_bytes       INTEGER,
    encrypted_bytes  INTEGER,
    sha256_plain     TEXT,
    sha256_encrypted TEXT
);
CREATE TABLE files (
    id           INTEGER PRIMARY KEY,
    snapshot_id  INTEGER NOT NULL,
    path         TEXT NOT NULL,
    size_bytes   INTEGER,
    sha256       TEXT,
    modified_at  TEXT,
    is_directory INTEGER
);
";

/// Build the filtered `catalog.db` for exactly `stage_set_ids` (this write's
/// stage sets) at `out_path`, overwriting any stale file left by a prior
/// attempt at the same session directory. Never mutates `conn`.
///
/// The schema version is read from `PRAGMA user_version` on the SOURCE
/// connection — never `meta.schema_version`, a relic frozen at `'1'` while
/// the real level has moved on (issue #61) — and stamped onto the output
/// database's own `PRAGMA user_version` so the operator can tell which
/// generation of `tapectl.db`'s schema this snapshot was taken from.
pub fn build_catalog_snapshot(
    conn: &Connection,
    stage_set_ids: &[i64],
    out_path: &Path,
) -> Result<()> {
    if out_path.exists() {
        std::fs::remove_file(out_path)?;
    }

    let out = Connection::open(out_path)?;
    out.execute_batch(SCHEMA)?;

    let user_version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    out.execute(&format!("PRAGMA user_version = {user_version}"), [])?;

    if stage_set_ids.is_empty() {
        return Ok(());
    }

    let ph = || {
        (0..stage_set_ids.len())
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(",")
    };

    // stage_sets: the given ids, verbatim.
    {
        let sql = format!(
            "SELECT id, snapshot_id, slice_size, num_slices, total_dar_size, total_encrypted_size,
                    key_fingerprints
             FROM stage_sets WHERE id IN ({})",
            ph()
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(stage_set_ids.iter()), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, Option<String>>(6)?,
            ))
        })?;
        for r in rows {
            let (
                id,
                snapshot_id,
                slice_size,
                num_slices,
                total_dar_size,
                total_encrypted_size,
                key_fingerprints,
            ) = r?;
            out.execute(
                "INSERT INTO stage_sets (id, snapshot_id, slice_size, num_slices, total_dar_size, total_encrypted_size, key_fingerprints)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![id, snapshot_id, slice_size, num_slices, total_dar_size, total_encrypted_size, key_fingerprints],
            )?;
        }
    }

    // snapshots reachable from those stage_sets.
    {
        let sql = format!(
            "SELECT DISTINCT s.id, s.unit_id, s.version, s.snapshot_type, s.source_path, s.total_size, s.file_count
             FROM snapshots s JOIN stage_sets ss ON ss.snapshot_id = s.id
             WHERE ss.id IN ({})",
            ph()
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(stage_set_ids.iter()), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, Option<i64>>(6)?,
            ))
        })?;
        for r in rows {
            let (id, unit_id, version, snapshot_type, source_path, total_size, file_count) = r?;
            out.execute(
                "INSERT INTO snapshots (id, unit_id, version, snapshot_type, source_path, total_size, file_count)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![id, unit_id, version, snapshot_type, source_path, total_size, file_count],
            )?;
        }
    }

    // units reachable from those snapshots.
    {
        let sql = format!(
            "SELECT DISTINCT u.id, u.uuid, u.name, u.tenant_id, u.status
             FROM units u
             JOIN snapshots s ON s.unit_id = u.id
             JOIN stage_sets ss ON ss.snapshot_id = s.id
             WHERE ss.id IN ({})",
            ph()
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(stage_set_ids.iter()), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?;
        for r in rows {
            let (id, uuid, name, tenant_id, status) = r?;
            out.execute(
                "INSERT INTO units (id, uuid, name, tenant_id, status) VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![id, uuid, name, tenant_id, status],
            )?;
        }
    }

    // tenants owning those units — name only. A `tenants` row carries no key
    // material (keys live on `encryption_keys` and, privately, under
    // `keys/`), so this is ownership, not secrets. Without it a rebuild has
    // to decrypt every tenant envelope just to learn who owns what.
    {
        let sql = format!(
            "SELECT DISTINCT t.id, t.name
             FROM tenants t
             JOIN units u ON u.tenant_id = t.id
             JOIN snapshots s ON s.unit_id = u.id
             JOIN stage_sets ss ON ss.snapshot_id = s.id
             WHERE ss.id IN ({})",
            ph()
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(stage_set_ids.iter()), |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        for r in rows {
            let (id, name) = r?;
            out.execute(
                "INSERT INTO tenants (id, name) VALUES (?1, ?2)",
                rusqlite::params![id, name],
            )?;
        }
    }

    // stage_slices for those stage_sets.
    {
        let sql = format!(
            "SELECT id, stage_set_id, slice_number, size_bytes, encrypted_bytes, sha256_plain, sha256_encrypted
             FROM stage_slices WHERE stage_set_id IN ({})",
            ph()
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(stage_set_ids.iter()), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
            ))
        })?;
        for r in rows {
            let (
                id,
                stage_set_id,
                slice_number,
                size_bytes,
                encrypted_bytes,
                sha256_plain,
                sha256_encrypted,
            ) = r?;
            out.execute(
                "INSERT INTO stage_slices (id, stage_set_id, slice_number, size_bytes, encrypted_bytes, sha256_plain, sha256_encrypted)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![id, stage_set_id, slice_number, size_bytes, encrypted_bytes, sha256_plain, sha256_encrypted],
            )?;
        }
    }

    // files reachable via those stage_sets' snapshots.
    {
        let sql = format!(
            "SELECT DISTINCT f.id, f.snapshot_id, f.path, f.size_bytes, f.sha256, f.modified_at, f.is_directory
             FROM files f
             JOIN stage_sets ss ON ss.snapshot_id = f.snapshot_id
             WHERE ss.id IN ({})",
            ph()
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(stage_set_ids.iter()), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, i64>(6)?,
            ))
        })?;
        for r in rows {
            let (id, snapshot_id, path, size_bytes, sha256, modified_at, is_directory) = r?;
            out.execute(
                "INSERT INTO files (id, snapshot_id, path, size_bytes, sha256, modified_at, is_directory)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![id, snapshot_id, path, size_bytes, sha256, modified_at, is_directory],
            )?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insert_unit_snapshot_stageset_slice_file(conn: &Connection, unit_name: &str) -> (i64, i64) {
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('t1', 0, 'active')",
            [],
        )
        .ok(); // may already exist across calls in the same test
        let tenant_id: i64 = conn
            .query_row("SELECT id FROM tenants WHERE name = 't1'", [], |r| r.get(0))
            .unwrap();

        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
             VALUES (?1, ?1, ?2, 'mtime_size', 1, 'active')",
            rusqlite::params![unit_name, tenant_id],
        )
        .unwrap();
        let unit_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
             VALUES (?1, 1, 'staged', '/tmp', 1, 10)",
            rusqlite::params![unit_id],
        )
        .unwrap();
        let snap_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO files (snapshot_id, path, size_bytes, sha256, is_directory)
             VALUES (?1, 'a.txt', 10, 'deadbeef', 0)",
            rusqlite::params![snap_id],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size, num_slices)
             VALUES (?1, 'staged', 1000, 1)",
            rusqlite::params![snap_id],
        )
        .unwrap();
        let ss_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes, encrypted_bytes, sha256_plain, sha256_encrypted, staging_path)
             VALUES (?1, 1, 10, 20, 'plainhash', 'cipherhash', '/tmp/slice')",
            rusqlite::params![ss_id],
        )
        .unwrap();

        (unit_id, ss_id)
    }

    #[test]
    fn snapshot_covers_only_the_given_stage_sets() {
        let conn = crate::db::open_memory().unwrap();
        let (_unit_a, ss_a) = insert_unit_snapshot_stageset_slice_file(&conn, "unit-a");
        let (_unit_b, ss_b) = insert_unit_snapshot_stageset_slice_file(&conn, "unit-b");

        let dir = tempfile::tempdir().unwrap();
        let out_path = dir.path().join("catalog.db");
        build_catalog_snapshot(&conn, &[ss_a], &out_path).unwrap();

        let out = Connection::open(&out_path).unwrap();
        let names: Vec<String> = out
            .prepare("SELECT name FROM units ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(names, vec!["unit-a".to_string()]);
        assert!(!names.contains(&"unit-b".to_string()));

        let ss_ids: Vec<i64> = out
            .prepare("SELECT id FROM stage_sets")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(ss_ids, vec![ss_a]);
        assert!(!ss_ids.contains(&ss_b));
    }

    /// Review finding 2 (2026-09-11): the three facts a rebuild could not
    /// get from the old subset — who owns the unit, the escrow receipt, and
    /// the plaintext hash — ride along now.
    #[test]
    fn snapshot_carries_tenants_receipts_and_plain_hashes() {
        let conn = crate::db::open_memory().unwrap();
        let (unit_id, ss_id) = insert_unit_snapshot_stageset_slice_file(&conn, "unit-a");
        conn.execute(
            "UPDATE stage_sets SET key_fingerprints = ?1 WHERE id = ?2",
            rusqlite::params![r#"["age1alice","age1escrow"]"#, ss_id],
        )
        .unwrap();
        let owner: String = conn
            .query_row(
                "SELECT t.name FROM tenants t JOIN units u ON u.tenant_id = t.id WHERE u.id = ?1",
                rusqlite::params![unit_id],
                |r| r.get(0),
            )
            .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let out_path = dir.path().join("catalog.db");
        build_catalog_snapshot(&conn, &[ss_id], &out_path).unwrap();
        let out = Connection::open(&out_path).unwrap();

        let (tenant_name, tenant_count): (String, i64) = out
            .query_row(
                "SELECT (SELECT t.name FROM tenants t JOIN units u ON u.tenant_id = t.id LIMIT 1),
                        (SELECT COUNT(*) FROM tenants)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(tenant_name, owner);
        assert_eq!(tenant_count, 1);

        let receipt: Option<String> = out
            .query_row("SELECT key_fingerprints FROM stage_sets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(receipt.as_deref(), Some(r#"["age1alice","age1escrow"]"#));

        let plain: String = out
            .query_row("SELECT sha256_plain FROM stage_slices", [], |r| r.get(0))
            .unwrap();
        assert_eq!(plain, "plainhash");
    }

    #[test]
    fn user_version_is_taken_from_pragma_not_meta_schema_version() {
        let conn = crate::db::open_memory().unwrap();
        let expected: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let out_path = dir.path().join("catalog.db");
        build_catalog_snapshot(&conn, &[], &out_path).unwrap();

        let out = Connection::open(&out_path).unwrap();
        let got: i64 = out
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(got, expected);
    }
}
