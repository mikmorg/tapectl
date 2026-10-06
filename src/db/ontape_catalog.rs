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
//! A `catalog.db` written between #83's original landing (2026-07) and the
//! 2026-09-11 decision carries `units`, `snapshots`, `stage_sets`,
//! `stage_slices` and `files` only. One written after the 2026-09-11
//! decision (review finding 2) additionally
//! carries `tenants` (unit ownership), `stage_sets.key_fingerprints` (the
//! escrow receipt #137 could not otherwise recover) and
//! `stage_slices.sha256_plain`. `PRAGMA user_version` cannot tell them apart
//! — it is stamped from the SOURCE database's schema level, which moves
//! independently of when this file's own shape changed. [`detect_generation`]
//! probes the actual shape instead; see its doc for why an operator database
//! at the same `user_version` can still carry either shape of `catalog.db`.

use std::collections::HashMap;
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

/// The `files` rows of one on-tape snapshot (`?1`), in `id` order: what
/// [`file_row`] maps. A rebuild streams them one version at a time (issue
/// #413) instead of holding every row of the tape in memory.
///
/// `?2`/`?3` are the snapshot's lowest and highest `id` from
/// [`file_id_spans`]. `files` has no index on `snapshot_id` (and this file's
/// shape is fixed until its 1.2.0 change), so without the bounds each
/// version would be a scan of every row on the tape, once per unit; with
/// them it is one range of the table's own b-tree. Both writers put a
/// version's rows in one run of ids, and a row of another version inside
/// the range is still excluded by `snapshot_id`.
pub const FILES_OF_SNAPSHOT: &str =
    "SELECT id, snapshot_id, path, size_bytes, sha256, modified_at, is_directory
     FROM files WHERE id BETWEEN ?2 AND ?3 AND snapshot_id = ?1 ORDER BY id";

/// Each snapshot's lowest and highest `files.id`, for [`FILES_OF_SNAPSHOT`]:
/// one pass over `files`.
pub fn file_id_spans(conn: &Connection) -> Result<HashMap<i64, (i64, i64)>> {
    let mut stmt =
        conn.prepare("SELECT snapshot_id, MIN(id), MAX(id) FROM files GROUP BY snapshot_id")?;
    let spans = stmt
        .query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))?
        .collect::<std::result::Result<HashMap<_, _>, _>>()?;
    Ok(spans)
}

/// One row of a `files` SELECT in [`FILES_OF_SNAPSHOT`]'s column order.
pub fn file_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<FileRow> {
    Ok(FileRow {
        id: r.get(0)?,
        snapshot_id: r.get(1)?,
        path: r.get(2)?,
        size_bytes: r.get(3)?,
        sha256: r.get(4)?,
        modified_at: r.get(5)?,
        is_directory: r.get(6)?,
    })
}

/// Read every table of the `catalog.db` at `path`, tolerating
/// [`Generation::Original`] (no `tenants`; `key_fingerprints`/`sha256_plain`
/// come back as `None`). Rows are returned in `id` order, which is also
/// insertion order for every table this module writes.
pub fn read(path: &Path) -> Result<OnTapeCatalog> {
    let (conn, mut cat) = read_all_but_files(path)?;
    let mut stmt = conn.prepare(
        "SELECT id, snapshot_id, path, size_bytes, sha256, modified_at, is_directory
         FROM files ORDER BY id",
    )?;
    cat.files = stmt
        .query_map([], file_row)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(cat)
}

/// [`read`] without the `files` table (left empty), and the open connection,
/// for a reader that streams the files itself with [`FILES_OF_SNAPSHOT`]:
/// `files` is nearly all of a `catalog.db` (about 250 MB of rows for a
/// million-file tape, issue #413).
pub fn read_all_but_files(path: &Path) -> Result<(Connection, OnTapeCatalog)> {
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

    let cat = OnTapeCatalog {
        generation,
        tenants,
        units,
        snapshots,
        stage_sets,
        slices,
        files: Vec::new(),
    };
    Ok((conn, cat))
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

    let out = open_output(out_path)?;
    fill(conn, stage_set_ids, &out)
}

/// Open the output file for a build that nothing has to survive (issue
/// #399): no rollback journal, no syncs. The file is throwaway — `volume
/// write` tars and age-encrypts it into the operator envelope, and a new
/// session rebuilds it from the catalog after a crash — so SQLite's
/// durability work buys nothing here. With the defaults (`journal_mode =
/// DELETE`, `synchronous = FULL`) every commit cost about four fsyncs on the
/// staging disk, while the cartridge sat loaded and idle.
fn open_output(out_path: &Path) -> Result<Connection> {
    let out = Connection::open(out_path)?;
    // Both before any transaction: SQLite cannot change the journal mode
    // inside one.
    out.pragma_update(None, "journal_mode", "OFF")?;
    out.pragma_update(None, "synchronous", "OFF")?;
    Ok(out)
}

/// The rows of [`write`], into an already-open `out`: the schema, the
/// `user_version` stamp and every row, in ONE transaction with one prepared
/// INSERT per table (issue #399). It used to be one autocommit per row —
/// about 100 rows/s, so ~5 hours for L6-0001's ~181k `files` rows.
///
/// Same rows in the same order as before: each table's SELECT is unchanged,
/// and the tables are filled in the same sequence. Nothing pins this file's
/// bytes; `read` and `catalog rebuild` read it by content.
pub(crate) fn fill(conn: &Connection, stage_set_ids: &[i64], out: &Connection) -> Result<()> {
    let tx = out.unchecked_transaction()?;
    tx.execute_batch(SCHEMA)?;

    let user_version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    tx.execute(&format!("PRAGMA user_version = {user_version}"), [])?;

    if !stage_set_ids.is_empty() {
        copy_rows(conn, stage_set_ids, &tx)?;
    }
    tx.commit()?;
    Ok(())
}

/// Every table's rows for `stage_set_ids`, from `conn` into `out` — called
/// only inside [`fill`]'s transaction.
fn copy_rows(conn: &Connection, stage_set_ids: &[i64], out: &Connection) -> Result<()> {
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
        let mut insert = out.prepare(
            "INSERT INTO stage_sets (id, snapshot_id, slice_size, num_slices, total_dar_size, total_encrypted_size, key_fingerprints)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
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
            insert.execute(rusqlite::params![
                id,
                snapshot_id,
                slice_size,
                num_slices,
                total_dar_size,
                total_encrypted_size,
                key_fingerprints
            ])?;
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
        let mut insert = out.prepare(
            "INSERT INTO snapshots (id, unit_id, version, snapshot_type, source_path, total_size, file_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        for r in rows {
            let (id, unit_id, version, snapshot_type, source_path, total_size, file_count) = r?;
            insert.execute(rusqlite::params![
                id,
                unit_id,
                version,
                snapshot_type,
                source_path,
                total_size,
                file_count
            ])?;
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
        let mut insert = out.prepare(
            "INSERT INTO units (id, uuid, name, tenant_id, status) VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for r in rows {
            let (id, uuid, name, tenant_id, status) = r?;
            insert.execute(rusqlite::params![id, uuid, name, tenant_id, status])?;
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
        let mut insert = out.prepare("INSERT INTO tenants (id, name) VALUES (?1, ?2)")?;
        for r in rows {
            let (id, name) = r?;
            insert.execute(rusqlite::params![id, name])?;
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
        let mut insert = out.prepare(
            "INSERT INTO stage_slices (id, stage_set_id, slice_number, size_bytes, encrypted_bytes, sha256_plain, sha256_encrypted)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
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
            insert.execute(rusqlite::params![
                id,
                stage_set_id,
                slice_number,
                size_bytes,
                encrypted_bytes,
                sha256_plain,
                sha256_encrypted
            ])?;
        }
    }

    // files reachable via those stage_sets' snapshots. The catalog keeps
    // them interned (`paths` + `file_versions`, migration 030); this file
    // keeps its own shape (ADR-0012 item 7: its change is 1.2.0), so each
    // row is mapped back: the sha256 as lowercase hex, the mtime as the
    // RFC 3339 text the walk always wrote, the kind as `is_directory`. `id`
    // numbers the rows in (version, interning) order; nothing reads it but
    // an operator's sqlite3.
    {
        let sql = format!(
            "SELECT fv.snapshot_id, p.path, fv.size_bytes, fv.sha256, fv.mtime_ns, fv.kind
             FROM {}
             WHERE fv.snapshot_id IN (SELECT snapshot_id FROM stage_sets WHERE id IN ({}))
             ORDER BY fv.snapshot_id, fv.path_id",
            super::files::VERSION_FILES,
            ph()
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(stage_set_ids.iter()), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<Vec<u8>>>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, super::files::FileKind>(5)?,
            ))
        })?;
        let mut insert = out.prepare(
            "INSERT INTO files (id, snapshot_id, path, size_bytes, sha256, modified_at, is_directory)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        for (n, r) in rows.enumerate() {
            let (snapshot_id, path, size_bytes, sha256, mtime_ns, kind) = r?;
            insert.execute(rusqlite::params![
                n as i64 + 1,
                snapshot_id,
                path,
                size_bytes,
                super::files::sha256_column(sha256),
                mtime_ns.and_then(super::files::mtime_ns_to_rfc3339),
                kind.is_dir() as i64,
            ])?;
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

        crate::db::files::fixture::insert(
            conn,
            snap_id,
            "a.txt",
            10,
            "regular",
            Some(&"de".repeat(32)),
        );

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

    /// Issue #399: the build is ONE transaction, whatever the row count. It
    /// used to be one autocommit per row — about four fsyncs each, ~100
    /// rows/s on the staging disk, so ~5 h for L6-0001's ~181k file rows
    /// while the cartridge sat loaded. Counted with SQLite's commit hook on
    /// the output connection: deterministic, unlike a timing bound.
    #[test]
    fn the_build_commits_once_whatever_the_row_count() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        const FILES: usize = 500;
        let conn = crate::db::open_memory().unwrap();
        let (_unit, ss_id) = insert_unit_snapshot_stageset_slice_file(&conn, "unit-a");
        let snap_id: i64 = conn
            .query_row(
                "SELECT snapshot_id FROM stage_sets WHERE id = ?1",
                [ss_id],
                |r| r.get(0),
            )
            .unwrap();
        {
            let tx = conn.unchecked_transaction().unwrap();
            crate::db::files::insert_version(
                &tx,
                snap_id,
                (0..FILES).map(|i| {
                    Ok(crate::db::files::FileEntry {
                        path: format!("dir/f{i:04}"),
                        kind: crate::db::files::FileKind::Regular,
                        size_bytes: 1,
                        mtime_ns: None,
                        sha256: Some([0x11; 32]),
                        link_target: None,
                    })
                }),
            )
            .unwrap();
            tx.commit().unwrap();
        }

        let dir = tempfile::tempdir().unwrap();
        let out = Connection::open(dir.path().join("catalog.db")).unwrap();
        let commits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&commits);
        out.commit_hook(Some(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            false
        }))
        .unwrap();

        fill(&conn, &[ss_id], &out).unwrap();

        let rows: i64 = out
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, FILES as i64 + 1, "positive control: every row landed");
        assert_eq!(
            commits.load(Ordering::SeqCst),
            1,
            "the catalog.db build must commit once, not once per row"
        );
    }

    /// Issue #399: the throwaway output file is written with no rollback
    /// journal and no syncs — it is tarred and age-encrypted, and a new
    /// session rebuilds it after a crash. Both settings belong to the
    /// connection, not the file, so they are read off the connection
    /// `write` builds through.
    #[test]
    fn the_output_connection_keeps_no_journal_and_never_syncs() {
        let dir = tempfile::tempdir().unwrap();
        let out = open_output(&dir.path().join("catalog.db")).unwrap();
        let mode: String = out
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        let sync: i64 = out
            .query_row("PRAGMA synchronous", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "off");
        assert_eq!(sync, 0);
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
        crate::db::files::fixture::insert(
            &conn,
            snap_b_id,
            "b.txt",
            20,
            "regular",
            Some(&"be".repeat(32)),
        );
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

    /// ADR-0012 item 7: the catalog's interned rows (migration 030) go onto
    /// tape in the shape `catalog.db` has always had — the sha256 as
    /// lowercase hex, the mtime as the walk's RFC 3339 text, the kind as
    /// `is_directory` — so readers of either generation see what they always
    /// saw.
    #[test]
    fn the_interned_rows_go_on_tape_in_the_old_shape() {
        use crate::db::files::{fixture, FileEntry, FileKind};
        let conn = crate::db::open_memory().unwrap();
        let (_unit, ss_id) = insert_unit_snapshot_stageset_slice_file(&conn, "unit-a");
        let snap_id: i64 = conn
            .query_row(
                "SELECT snapshot_id FROM stage_sets WHERE id = ?1",
                [ss_id],
                |r| r.get(0),
            )
            .unwrap();
        let mtime = 1_788_264_000_i64;
        fixture::insert_entry(
            &conn,
            snap_id,
            FileEntry {
                path: "docs".into(),
                kind: FileKind::Dir,
                size_bytes: 0,
                mtime_ns: Some(mtime * 1_000_000_000),
                sha256: None,
                link_target: None,
            },
        );
        fixture::insert_entry(
            &conn,
            snap_id,
            FileEntry {
                path: "docs/b.bin".into(),
                kind: FileKind::Regular,
                size_bytes: 3,
                mtime_ns: Some(mtime * 1_000_000_000),
                sha256: Some([0xAB; 32]),
                link_target: None,
            },
        );

        let dir = tempfile::tempdir().unwrap();
        let out_path = dir.path().join("catalog.db");
        write(&conn, &[ss_id], &out_path).unwrap();
        let cat = read(&out_path).unwrap();
        type Shape = (String, Option<i64>, Option<String>, Option<String>, i64);
        let rows: Vec<Shape> = cat
            .files
            .iter()
            .map(|f| {
                (
                    f.path.clone(),
                    f.size_bytes,
                    f.sha256.clone(),
                    f.modified_at.clone(),
                    f.is_directory,
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                ("a.txt".into(), Some(10), Some("de".repeat(32)), None, 0),
                (
                    "docs".into(),
                    Some(0),
                    None,
                    Some("2026-09-01T12:00:00+00:00".into()),
                    1
                ),
                (
                    "docs/b.bin".into(),
                    Some(3),
                    Some("ab".repeat(32)),
                    Some("2026-09-01T12:00:00+00:00".into()),
                    0
                ),
            ]
        );
        let ids: Vec<i64> = cat.files.iter().map(|f| f.id).collect();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    /// Issue #413: a rebuild streams one version's `files` rows at a time,
    /// and each version is one range of the table's b-tree, not a scan of
    /// every row on the tape (there is no index on `snapshot_id`, and a
    /// rebuild reads one version per unit). Streamed version by version,
    /// the rows are exactly what [`read`] returns, in the same order.
    #[test]
    fn a_version_streams_as_one_range_of_ids() {
        let conn = crate::db::open_memory().unwrap();
        let ss: Vec<i64> = ["unit-a", "unit-b", "unit-c"]
            .iter()
            .map(|u| insert_unit_snapshot_stageset_slice_file(&conn, u).1)
            .collect();
        for (n, &ss_id) in ss.iter().enumerate() {
            let snap: i64 = conn
                .query_row(
                    "SELECT snapshot_id FROM stage_sets WHERE id = ?1",
                    [ss_id],
                    |r| r.get(0),
                )
                .unwrap();
            for i in 0..=n {
                crate::db::files::fixture::insert(
                    &conn,
                    snap,
                    &format!("more/{i}.bin"),
                    i as i64,
                    "regular",
                    None,
                );
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let out_path = dir.path().join("catalog.db");
        write(&conn, &ss, &out_path).unwrap();
        let whole = read(&out_path).unwrap();

        let (ontape, cat) = read_all_but_files(&out_path).unwrap();
        assert!(cat.files.is_empty(), "the files are left to the stream");
        let spans = file_id_spans(&ontape).unwrap();
        assert_eq!(spans.len(), 3, "one span per version");
        let mut stmt = ontape.prepare(FILES_OF_SNAPSHOT).unwrap();
        let mut streamed = Vec::new();
        let mut snapshots: Vec<i64> = spans.keys().copied().collect();
        snapshots.sort();
        for snap in snapshots {
            let (first, last) = spans[&snap];
            let rows = stmt
                .query_map(rusqlite::params![snap, first, last], file_row)
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            assert!(rows.iter().all(|f| f.snapshot_id == snap));
            streamed.extend(rows);
        }
        let key = |f: &FileRow| (f.id, f.snapshot_id, f.path.clone(), f.size_bytes);
        assert_eq!(
            streamed.iter().map(key).collect::<Vec<_>>(),
            whole.files.iter().map(key).collect::<Vec<_>>(),
            "streamed version by version, the rows are the whole table's"
        );

        let plan: Vec<String> = ontape
            .prepare(&format!("EXPLAIN QUERY PLAN {FILES_OF_SNAPSHOT}"))
            .unwrap()
            .query_map(rusqlite::params![1, 1, 1], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(
            plan.iter().any(|d| d.contains("INTEGER PRIMARY KEY"))
                && !plan.iter().any(|d| d.starts_with("SCAN files")),
            "one version must be a range of ids, not a scan of every row: {plan:?}"
        );
    }
}
