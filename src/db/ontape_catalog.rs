//! The on-tape `catalog.db` (issue #83): schema, row types, generation
//! detection, and the write/read halves both sides use.
//!
//! # What this IS
//!
//! The format of the small, standalone SQLite file that rides inside the
//! OPERATOR envelope on every volume — `docs/design/volume-format-v2.md`
//! line 89's "the operator envelope's catalog snapshot, #83". It has an
//! external contract: `RECOVERY.md`'s "Querying `catalog.db`" section
//! documents this schema to an operator armed with nothing but `sqlite3` and
//! a decrypted envelope, and `catalog rebuild` (#136, `src/volume/rebuild.rs`)
//! reads it back to reconstruct catalog rows from a sealed tape when the
//! database is gone. Both readers need the same shape kept in one place —
//! before this module, the writer (`SCHEMA` + copy loops) and the reader
//! (four hand-written SELECTs over a partial re-declaration of the same
//! schema) drifted independently, and adding a column meant editing both.
//!
//! # What this is NOT
//!
//! This is not the Heir Kit's full `tapectl.db` (ADR-0009). The kit ships
//! the complete database — including `locations` and `cartridges`, which the
//! sacred plaintext-isolation invariant forbids from ever riding on tape —
//! because an heir needs to find a cartridge, not just prove one exists. This
//! module's file is deliberately narrower: everything a rebuild needs at
//! staging time for *this volume's write only*, inside an age envelope
//! encrypted to operator + escrow, and never a tenant envelope.
//!
//! # Two generations exist on shipped tape
//!
//! A `catalog.db` written before 2026-07 (#83's original landing) carries
//! `units`, `snapshots`, `stage_sets`, `stage_slices` and `files` only. One
//! written after the 2026-09-11 decision (review finding 2) additionally
//! carries `tenants` (unit ownership), `stage_sets.key_fingerprints` (the
//! escrow receipt #137 could not otherwise recover) and
//! `stage_slices.sha256_plain`. `PRAGMA user_version` cannot tell them apart
//! — it is stamped from the SOURCE database's schema level, which moves
//! independently of when this file's own shape changed. [`detect_generation`]
//! probes the actual shape instead; see its doc for why an operator database
//! at the same `user_version` can still carry either shape of `catalog.db`.

use std::path::Path;

use rusqlite::{params_from_iter, Connection};

use crate::error::{Result, TapectlError};

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

/// Which shape a `catalog.db` file carries. Named for what each carries, not
/// for a version number — `PRAGMA user_version` is stamped from the SOURCE
/// database and does not track this file's own shape (see the module doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Generation {
    /// #83's original shape (landed 2026-07): `units`, `snapshots`,
    /// `stage_sets`, `stage_slices`, `files` only. No `tenants` table, no
    /// `stage_sets.key_fingerprints`, no `stage_slices.sha256_plain`.
    Original,
    /// The 2026-09-11 shape (review finding 2): adds `tenants` (unit
    /// ownership), `stage_sets.key_fingerprints` (the escrow receipt) and
    /// `stage_slices.sha256_plain`.
    WithOwnershipAndReceipts,
}

/// True when `table` has a column named `column`, probed via
/// `PRAGMA table_info` rather than trusting any version number — a tape
/// written by the same schema level can carry either generation.
fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let has = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .filter_map(|r| r.ok())
        .any(|c| c == column);
    Ok(has)
}

/// Probe the shape of an already-open `catalog.db` connection: does it carry
/// a `tenants` table, and does `stage_sets` carry `key_fingerprints`? The two
/// columns shipped together in the 2026-09-11 change, so they must agree —
/// if they don't, this is a corrupt or hand-edited file, not a recognized
/// generation, and this returns an error naming which one is present rather
/// than guessing.
pub fn detect_generation(conn: &Connection) -> Result<Generation> {
    let has_tenants: bool = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'tenants'",
        [],
        |r| r.get::<_, i64>(0),
    )? > 0;
    let has_receipts = has_column(conn, "stage_sets", "key_fingerprints")?;

    match (has_tenants, has_receipts) {
        (true, true) => Ok(Generation::WithOwnershipAndReceipts),
        (false, false) => Ok(Generation::Original),
        (true, false) => Err(TapectlError::Other(
            "catalog.db has a `tenants` table but no `stage_sets.key_fingerprints` \
             column — a corrupt or hand-edited file, not a recognized generation \
             (the two shipped together on 2026-09-11)"
                .to_string(),
        )),
        (false, true) => Err(TapectlError::Other(
            "catalog.db has `stage_sets.key_fingerprints` but no `tenants` table — \
             a corrupt or hand-edited file, not a recognized generation \
             (the two shipped together on 2026-09-11)"
                .to_string(),
        )),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantRow {
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitRow {
    pub id: i64,
    pub uuid: String,
    pub name: String,
    pub tenant_id: i64,
    pub status: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotRow {
    pub id: i64,
    pub unit_id: i64,
    pub version: i64,
    pub snapshot_type: Option<String>,
    pub source_path: String,
    pub total_size: Option<i64>,
    pub file_count: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageSetRow {
    pub id: i64,
    pub snapshot_id: i64,
    pub slice_size: Option<i64>,
    pub num_slices: Option<i64>,
    pub total_dar_size: Option<i64>,
    pub total_encrypted_size: Option<i64>,
    pub key_fingerprints: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliceRow {
    pub id: i64,
    pub stage_set_id: i64,
    pub slice_number: i64,
    pub size_bytes: i64,
    pub encrypted_bytes: i64,
    pub sha256_plain: Option<String>,
    pub sha256_encrypted: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRow {
    pub id: i64,
    pub snapshot_id: i64,
    pub path: String,
    pub size_bytes: Option<i64>,
    pub sha256: Option<String>,
    pub modified_at: Option<String>,
    pub is_directory: i64,
}

/// Every table of a `catalog.db`, read back typed. `tenants` is empty and
/// every `key_fingerprints`/`sha256_plain` is `None` when `generation` is
/// [`Generation::Original`] — that is a real, expected shape, not an error.
#[derive(Debug, Clone, PartialEq)]
pub struct OnTapeCatalog {
    pub generation: Generation,
    pub tenants: Vec<TenantRow>,
    pub units: Vec<UnitRow>,
    pub snapshots: Vec<SnapshotRow>,
    pub stage_sets: Vec<StageSetRow>,
    pub slices: Vec<SliceRow>,
    pub files: Vec<FileRow>,
}

/// Read every table of the `catalog.db` at `path`, tolerating
/// [`Generation::Original`] (no `tenants`; `key_fingerprints`/`sha256_plain`
/// come back as `None`). Rows are returned in `id` order, which is also
/// insertion order for every table this module writes.
pub fn read(path: &Path) -> Result<OnTapeCatalog> {
    let conn = Connection::open(path)?;
    let generation = detect_generation(&conn)?;
    let has_new = generation == Generation::WithOwnershipAndReceipts;
    let has_sha_plain = has_column(&conn, "stage_slices", "sha256_plain")?;

    let tenants = if has_new {
        let mut stmt = conn.prepare("SELECT id, name FROM tenants ORDER BY id")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(TenantRow {
                    id: r.get(0)?,
                    name: r.get(1)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows
    } else {
        Vec::new()
    };

    let units = {
        let mut stmt =
            conn.prepare("SELECT id, uuid, name, tenant_id, status FROM units ORDER BY id")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(UnitRow {
                    id: r.get(0)?,
                    uuid: r.get(1)?,
                    name: r.get(2)?,
                    tenant_id: r.get(3)?,
                    status: r.get(4)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows
    };

    let snapshots = {
        let mut stmt = conn.prepare(
            "SELECT id, unit_id, version, snapshot_type, source_path, total_size, file_count
             FROM snapshots ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(SnapshotRow {
                    id: r.get(0)?,
                    unit_id: r.get(1)?,
                    version: r.get(2)?,
                    snapshot_type: r.get(3)?,
                    source_path: r.get(4)?,
                    total_size: r.get(5)?,
                    file_count: r.get(6)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows
    };

    let stage_sets = if has_new {
        let mut stmt = conn.prepare(
            "SELECT id, snapshot_id, slice_size, num_slices, total_dar_size,
                    total_encrypted_size, key_fingerprints
             FROM stage_sets ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(StageSetRow {
                    id: r.get(0)?,
                    snapshot_id: r.get(1)?,
                    slice_size: r.get(2)?,
                    num_slices: r.get(3)?,
                    total_dar_size: r.get(4)?,
                    total_encrypted_size: r.get(5)?,
                    key_fingerprints: r.get(6)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows
    } else {
        let mut stmt = conn.prepare(
            "SELECT id, snapshot_id, slice_size, num_slices, total_dar_size,
                    total_encrypted_size
             FROM stage_sets ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(StageSetRow {
                    id: r.get(0)?,
                    snapshot_id: r.get(1)?,
                    slice_size: r.get(2)?,
                    num_slices: r.get(3)?,
                    total_dar_size: r.get(4)?,
                    total_encrypted_size: r.get(5)?,
                    key_fingerprints: None,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows
    };

    let slices = if has_sha_plain {
        let mut stmt = conn.prepare(
            "SELECT id, stage_set_id, slice_number, size_bytes, encrypted_bytes,
                    sha256_plain, sha256_encrypted
             FROM stage_slices ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(SliceRow {
                    id: r.get(0)?,
                    stage_set_id: r.get(1)?,
                    slice_number: r.get(2)?,
                    size_bytes: r.get(3)?,
                    encrypted_bytes: r.get(4)?,
                    sha256_plain: r.get(5)?,
                    sha256_encrypted: r.get(6)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows
    } else {
        let mut stmt = conn.prepare(
            "SELECT id, stage_set_id, slice_number, size_bytes, encrypted_bytes,
                    sha256_encrypted
             FROM stage_slices ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(SliceRow {
                    id: r.get(0)?,
                    stage_set_id: r.get(1)?,
                    slice_number: r.get(2)?,
                    size_bytes: r.get(3)?,
                    encrypted_bytes: r.get(4)?,
                    sha256_plain: None,
                    sha256_encrypted: r.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows
    };

    let files = {
        let mut stmt = conn.prepare(
            "SELECT id, snapshot_id, path, size_bytes, sha256, modified_at, is_directory
             FROM files ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(FileRow {
                    id: r.get(0)?,
                    snapshot_id: r.get(1)?,
                    path: r.get(2)?,
                    size_bytes: r.get(3)?,
                    sha256: r.get(4)?,
                    modified_at: r.get(5)?,
                    is_directory: r.get(6)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows
    };

    Ok(OnTapeCatalog {
        generation,
        tenants,
        units,
        snapshots,
        stage_sets,
        slices,
        files,
    })
}

/// Build the filtered `catalog.db` for exactly `stage_set_ids` (this write's
/// stage sets) at `out_path`, overwriting any stale file left by a prior
/// attempt at the same session directory. Never mutates `conn`.
///
/// The schema version is read from `PRAGMA user_version` on the SOURCE
/// connection — never `meta.schema_version`, a relic frozen at `'1'` while
/// the real level has moved on (issue #61) — and stamped onto the output
/// database's own `PRAGMA user_version` so the operator can tell which
/// generation of `tapectl.db`'s schema this snapshot was taken from. This is
/// documented to the operator in `RECOVERY.md` and must keep being written.
pub fn write(conn: &Connection, stage_set_ids: &[i64], out_path: &Path) -> Result<()> {
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
        write(&conn, &[ss_a], &out_path).unwrap();

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
        write(&conn, &[ss_id], &out_path).unwrap();
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
        write(&conn, &[], &out_path).unwrap();

        let out = Connection::open(&out_path).unwrap();
        let got: i64 = out
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(got, expected);
    }

    /// `write` then `read` round-trips typed rows for a fixture spanning two
    /// units across two tenants — the shape `catalog rebuild` actually reads.
    #[test]
    fn write_then_read_round_trips_typed_rows_across_two_tenants() {
        let conn = crate::db::open_memory().unwrap();
        let (_unit_a, ss_a) = insert_unit_snapshot_stageset_slice_file(&conn, "unit-a");
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('t2', 0, 'active')",
            [],
        )
        .unwrap();
        let t2_id: i64 = conn
            .query_row("SELECT id FROM tenants WHERE name = 't2'", [], |r| r.get(0))
            .unwrap();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
             VALUES ('unit-b', 'unit-b', ?1, 'mtime_size', 1, 'active')",
            rusqlite::params![t2_id],
        )
        .unwrap();
        let unit_b_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
             VALUES (?1, 1, 'staged', '/tmp/b', 1, 20)",
            rusqlite::params![unit_b_id],
        )
        .unwrap();
        let snap_b_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO files (snapshot_id, path, size_bytes, sha256, is_directory)
             VALUES (?1, 'b.txt', 20, 'beefdead', 0)",
            rusqlite::params![snap_b_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size, num_slices)
             VALUES (?1, 'staged', 2000, 1)",
            rusqlite::params![snap_b_id],
        )
        .unwrap();
        let ss_b = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes, encrypted_bytes, sha256_plain, sha256_encrypted, staging_path)
             VALUES (?1, 1, 20, 30, 'plainhash2', 'cipherhash2', '/tmp/slice2')",
            rusqlite::params![ss_b],
        )
        .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let out_path = dir.path().join("catalog.db");
        write(&conn, &[ss_a, ss_b], &out_path).unwrap();

        let cat = read(&out_path).unwrap();
        assert_eq!(cat.generation, Generation::WithOwnershipAndReceipts);
        assert_eq!(cat.tenants.len(), 2);
        assert_eq!(cat.units.len(), 2);
        assert_eq!(cat.snapshots.len(), 2);
        assert_eq!(cat.stage_sets.len(), 2);
        assert_eq!(cat.slices.len(), 2);
        assert_eq!(cat.files.len(), 2);

        let unit_a = cat.units.iter().find(|u| u.name == "unit-a").unwrap();
        let unit_b = cat.units.iter().find(|u| u.name == "unit-b").unwrap();
        assert_ne!(unit_a.tenant_id, unit_b.tenant_id);
        let tenant_names: Vec<&str> = cat.tenants.iter().map(|t| t.name.as_str()).collect();
        assert!(tenant_names.contains(&"t1"));
        assert!(tenant_names.contains(&"t2"));

        let ss_for_a = cat
            .stage_sets
            .iter()
            .find(|s| s.id == ss_a)
            .expect("stage set a present");
        assert!(ss_for_a.key_fingerprints.is_none());
        let slice_for_a = cat
            .slices
            .iter()
            .find(|s| s.stage_set_id == ss_a)
            .expect("slice a present");
        assert_eq!(slice_for_a.sha256_plain.as_deref(), Some("plainhash"));
    }

    /// The same derivation `tests/catalog_rebuild.rs` uses for its "Old
    /// shape" fixture: drop the three 2026-09-11 additions from a freshly
    /// written file. `detect_generation` must call that `Original`.
    #[test]
    fn detect_generation_returns_original_after_dropping_the_new_columns() {
        let conn = crate::db::open_memory().unwrap();
        let (_unit_a, ss_a) = insert_unit_snapshot_stageset_slice_file(&conn, "unit-a");

        let dir = tempfile::tempdir().unwrap();
        let out_path = dir.path().join("catalog.db");
        write(&conn, &[ss_a], &out_path).unwrap();

        let out = Connection::open(&out_path).unwrap();
        out.execute_batch(
            "DROP TABLE tenants;
             ALTER TABLE stage_sets DROP COLUMN key_fingerprints;
             ALTER TABLE stage_slices DROP COLUMN sha256_plain;",
        )
        .unwrap();

        assert_eq!(detect_generation(&out).unwrap(), Generation::Original);
        let cat = read(&out_path).unwrap();
        assert_eq!(cat.generation, Generation::Original);
        assert!(cat.tenants.is_empty());
        assert!(cat.stage_sets[0].key_fingerprints.is_none());
        assert!(cat.slices[0].sha256_plain.is_none());
    }

    /// A file with `tenants` but not `key_fingerprints` (or vice versa) is
    /// corrupt or hand-edited, not a recognized generation — the two shipped
    /// together and must agree.
    #[test]
    fn a_disagreeing_shape_is_an_error_naming_what_is_present() {
        let conn = crate::db::open_memory().unwrap();
        let (_unit_a, ss_a) = insert_unit_snapshot_stageset_slice_file(&conn, "unit-a");

        let dir = tempfile::tempdir().unwrap();

        let tenants_only = dir.path().join("tenants_only.db");
        write(&conn, &[ss_a], &tenants_only).unwrap();
        let out = Connection::open(&tenants_only).unwrap();
        out.execute_batch("ALTER TABLE stage_sets DROP COLUMN key_fingerprints;")
            .unwrap();
        let err = detect_generation(&out).unwrap_err().to_string();
        assert!(err.contains("tenants"), "error should name tenants: {err}");

        let receipts_only = dir.path().join("receipts_only.db");
        write(&conn, &[ss_a], &receipts_only).unwrap();
        let out = Connection::open(&receipts_only).unwrap();
        out.execute_batch("DROP TABLE tenants;").unwrap();
        let err = detect_generation(&out).unwrap_err().to_string();
        assert!(
            err.contains("key_fingerprints"),
            "error should name key_fingerprints: {err}"
        );
    }
}
