//! The on-tape `catalog.db` (issue #83): schema, row types, the shape
//! check, and the write/read halves both sides use.
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
//! # One shape
//!
//! `docs/design/volume-format-v2.md` §1.4 is the normative statement. This
//! tapectl reads the one shape it writes ([`SCHEMA`]): `tenants` (unit
//! ownership), `stage_sets.key_fingerprints` (the escrow receipt),
//! `stage_slices.sha256_plain`, and the main catalog's narrow per-file shape,
//! `paths` + `file_versions`, which records each entry's kind and link
//! target. A `catalog.db` of any other shape (the ones tapectl wrote before
//! 1.1.0 among them) is refused, naming what was found ([`check_shape`]):
//! there is no compatibility with them (ADR-0012 items 242 and 258(d)).
//!
//! `PRAGMA user_version` does not say which shape a file has — it is stamped
//! from the SOURCE database's schema level — so [`check_shape`] probes the
//! actual tables and columns instead.

use std::collections::HashMap;
use std::path::Path;

use rusqlite::{params_from_iter, Connection};

use super::files::{FileEntry, FileKind};
use crate::error::{Result, TapectlError};

/// Table/column subset carried into the snapshot: enough to answer "what
/// units/snapshots/files landed on this volume" without needing the source
/// `tapectl.db` — see `RECOVERY.md`'s "Querying `catalog.db`" section for
/// the schema as documented to the operator, and `volume-format-v2.md` §1.4
/// for the normative one. It is the only shape this tapectl reads.
///
/// `paths` carries no `UNIQUE(unit_id, path)` index, unlike the main
/// catalog's: nothing reads this file by name, a rebuild interns paths into
/// its own catalog's index, and the index would store every path twice.
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
CREATE TABLE paths (
    id      INTEGER PRIMARY KEY,
    unit_id INTEGER NOT NULL,
    path    TEXT NOT NULL
);
CREATE TABLE file_versions (
    snapshot_id INTEGER NOT NULL,
    path_id     INTEGER NOT NULL,
    kind        INTEGER NOT NULL CHECK(kind IN (0, 1, 2, 3)),
    size_bytes  INTEGER NOT NULL,
    mtime_ns    INTEGER,
    sha256      BLOB CHECK(sha256 IS NULL
                           OR (typeof(sha256) = 'blob' AND length(sha256) = 32)),
    link_target TEXT,
    PRIMARY KEY (snapshot_id, path_id)
) WITHOUT ROWID;
";

/// True when `table` has a column named `column`, probed via
/// `PRAGMA table_info` rather than trusting any version number.
fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let has = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .filter_map(|r| r.ok())
        .any(|c| c == column);
    Ok(has)
}

fn has_table(conn: &Connection, table: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |r| r.get::<_, i64>(0),
    )? > 0)
}

/// The tables, and the columns of them that an older shape lacked, that
/// every `catalog.db` this tapectl writes carries ([`SCHEMA`]).
const REQUIRED: &[(&str, Option<&str>)] = &[
    ("tenants", None),
    ("units", None),
    ("snapshots", None),
    ("stage_sets", Some("key_fingerprints")),
    ("stage_slices", Some("sha256_plain")),
    ("paths", None),
    ("file_versions", None),
];

/// Refuse an open `catalog.db` that is not the shape this tapectl writes
/// ([`SCHEMA`], `volume-format-v2.md` §1.4), naming what was found: every
/// table or column it lacks, and a `files` table, the per-file table of the
/// shapes tapectl wrote before 1.1.0. No other shape is read (ADR-0012 item
/// 258(d)); a reader does not guess.
pub fn check_shape(conn: &Connection) -> Result<()> {
    let mut missing: Vec<String> = Vec::new();
    for &(table, column) in REQUIRED {
        if !has_table(conn, table)? {
            missing.push(format!("no `{table}` table"));
        } else if let Some(column) = column {
            if !has_column(conn, table, column)? {
                missing.push(format!("no `{table}.{column}` column"));
            }
        }
    }
    if has_table(conn, "files")? {
        missing.push(
            "a `files` table (the per-file table of a catalog.db tapectl wrote before 1.1.0)"
                .to_string(),
        );
    }
    if missing.is_empty() {
        return Ok(());
    }
    Err(TapectlError::Other(format!(
        "catalog.db is not the shape this tapectl writes (volume-format-v2.md §1.4): it has {}. \
         This tapectl reads no other shape",
        missing.join(", ")
    )))
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

/// One entry of one on-tape version, in the catalog's own terms
/// ([`FileEntry`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRow {
    pub snapshot_id: i64,
    pub entry: FileEntry,
}

/// Every table of a `catalog.db`, read back typed.
#[derive(Debug, Clone, PartialEq)]
pub struct OnTapeCatalog {
    pub tenants: Vec<TenantRow>,
    pub units: Vec<UnitRow>,
    pub snapshots: Vec<SnapshotRow>,
    pub stage_sets: Vec<StageSetRow>,
    pub slices: Vec<SliceRow>,
    pub files: Vec<FileRow>,
}

/// One version's rows (`?1`) with their paths, in `path_id` order: the
/// order the writing catalog first interned each path — walk order for a
/// unit's first version; in a later version, paths new to the unit follow
/// all earlier ones. A range of `file_versions`' primary key; the bounds
/// keep it one even if a planner chose otherwise.
const FILE_VERSIONS_OF_SNAPSHOT: &str =
    "SELECT fv.path_id, p.path, fv.kind, fv.size_bytes, fv.mtime_ns, fv.sha256, fv.link_target
     FROM file_versions fv JOIN paths p ON p.id = fv.path_id
     WHERE fv.snapshot_id = ?1 AND fv.path_id BETWEEN ?2 AND ?3
     ORDER BY fv.path_id";

const FILE_VERSIONS_SPANS: &str =
    "SELECT snapshot_id, MIN(path_id), MAX(path_id) FROM file_versions GROUP BY snapshot_id";

/// Where each version's file rows lie in an open `catalog.db`, so a reader
/// streams one version at a time (issue #413) instead of holding every row of
/// the tape in memory: the per-file rows are nearly all of the file. One pass
/// over `file_versions` builds it.
#[derive(Debug, Clone)]
pub struct FileSpans {
    /// snapshot id -> the lowest and highest `file_versions.path_id` its
    /// rows occupy. A version with no rows has no entry.
    spans: HashMap<i64, (i64, i64)>,
}

impl FileSpans {
    pub fn read(conn: &Connection) -> Result<Self> {
        let mut stmt = conn.prepare(FILE_VERSIONS_SPANS)?;
        let spans = stmt
            .query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))?
            .collect::<std::result::Result<HashMap<_, _>, _>>()?;
        Ok(FileSpans { spans })
    }

    /// Whether `snapshot_id` has any file rows in this file.
    pub fn contains(&self, snapshot_id: i64) -> bool {
        self.spans.contains_key(&snapshot_id)
    }

    /// The versions with file rows, ascending.
    pub fn snapshot_ids(&self) -> Vec<i64> {
        let mut ids: Vec<i64> = self.spans.keys().copied().collect();
        ids.sort_unstable();
        ids
    }

    /// Hand `f` one version's entries as the catalog's own [`FileEntry`], in
    /// `path_id` order: the order the writing catalog first interned each
    /// path (walk order for a unit's first version; in a later version, paths
    /// new to the unit follow all earlier ones). A version with no rows is an
    /// empty iterator.
    ///
    /// A value that would not come back as what the writer recorded is
    /// refused, naming the row: a reader does not guess. A `kind` outside 0–3
    /// or a `sha256` that is not 32 bytes is refused; everything else is the
    /// catalog's own value.
    pub fn stream<R>(
        &self,
        conn: &Connection,
        snapshot_id: i64,
        f: impl FnOnce(&mut dyn Iterator<Item = Result<FileEntry>>) -> Result<R>,
    ) -> Result<R> {
        let Some(&(first, last)) = self.spans.get(&snapshot_id) else {
            return f(&mut std::iter::empty());
        };
        let bounds = rusqlite::params![snapshot_id, first, last];
        let mut stmt = conn.prepare(FILE_VERSIONS_OF_SNAPSHOT)?;
        let rows = stmt.query_map(bounds, |r| {
            Ok(FileVersionRow {
                path_id: r.get(0)?,
                path: r.get(1)?,
                kind: r.get(2)?,
                size_bytes: r.get(3)?,
                mtime_ns: r.get(4)?,
                sha256: r.get(5)?,
                link_target: r.get(6)?,
            })
        })?;
        let mut entries = rows.map(|row| row?.into_entry(snapshot_id));
        f(&mut entries)
    }
}

/// A `file_versions` row with its path, as stored. `kind` and
/// `sha256` are read untyped so a bad value is refused naming the row, not
/// as a bare type error.
struct FileVersionRow {
    path_id: i64,
    path: String,
    kind: rusqlite::types::Value,
    size_bytes: i64,
    mtime_ns: Option<i64>,
    sha256: rusqlite::types::Value,
    link_target: Option<String>,
}

impl FileVersionRow {
    fn into_entry(self, snapshot_id: i64) -> Result<FileEntry> {
        use rusqlite::types::Value;
        let bad = |what: String| {
            TapectlError::Other(format!(
                "catalog.db file_versions row (snapshot {snapshot_id}, path_id {}, {:?}): {what}",
                self.path_id, self.path
            ))
        };
        let kind = match &self.kind {
            Value::Integer(code) => FileKind::from_code(*code),
            _ => None,
        }
        .ok_or_else(|| bad(format!("kind {:?} is not 0, 1, 2 or 3", self.kind)))?;
        let sha256 = match &self.sha256 {
            Value::Null => None,
            Value::Blob(b) if b.len() == 32 => {
                let mut h = [0u8; 32];
                h.copy_from_slice(b);
                Some(h)
            }
            other => {
                return Err(bad(format!(
                    "sha256 is not 32 bytes: {}",
                    match other {
                        Value::Blob(b) => format!("a {}-byte blob", b.len()),
                        v => format!("{v:?}"),
                    }
                )))
            }
        };
        Ok(FileEntry {
            path: self.path,
            kind,
            size_bytes: self.size_bytes,
            mtime_ns: self.mtime_ns,
            sha256,
            link_target: self.link_target,
        })
    }
}

/// Read every table of the `catalog.db` at `path`, refusing any shape but
/// the one this tapectl writes ([`check_shape`]). Rows are returned in `id`
/// order, which is also insertion order for every table this module writes,
/// and files version by version in the order [`FileSpans::stream`] gives
/// them.
pub fn read(path: &Path) -> Result<OnTapeCatalog> {
    let (conn, mut cat) = read_all_but_files(path)?;
    let spans = FileSpans::read(&conn)?;
    for snapshot_id in spans.snapshot_ids() {
        spans.stream(&conn, snapshot_id, |entries| {
            for entry in entries {
                cat.files.push(FileRow {
                    snapshot_id,
                    entry: entry?,
                });
            }
            Ok(())
        })?;
    }
    Ok(cat)
}

/// [`read`] without the per-file rows (left empty), and the open connection,
/// for a reader that streams them itself through [`FileSpans`]: they are
/// nearly all of a `catalog.db` (issue #413).
pub fn read_all_but_files(path: &Path) -> Result<(Connection, OnTapeCatalog)> {
    let conn = Connection::open(path)?;
    check_shape(&conn)?;

    let tenants = {
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

    let stage_sets = {
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
    };

    let slices = {
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
    };

    let cat = OnTapeCatalog {
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
/// Nothing pins this file's bytes; `read` and `catalog rebuild` read it by
/// content, and `volume-format-v2.md` §1.4 specifies its shape.
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

    // Generation 3's per-file rows (ADR-0012 2026-10-12 item 48): the
    // catalog's own `paths` + `file_versions` (migration 030), verbatim and
    // under the catalog's own ids, for the versions those stage sets hold.
    // `paths` gets only the paths those versions use — a unit's other
    // versions are not on this tape — and a version's rows go in `path_id`
    // order: the order the catalog first interned each path, which is walk
    // order for a unit's first version; in a later version, paths new to the
    // unit follow all earlier ones.
    let versions = format!("SELECT snapshot_id FROM stage_sets WHERE id IN ({})", ph());
    {
        let sql = format!(
            "SELECT p.id, p.unit_id, p.path FROM paths p
             WHERE p.id IN (SELECT fv.path_id FROM file_versions fv
                             WHERE fv.snapshot_id IN ({versions}))
             ORDER BY p.id"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(stage_set_ids.iter()), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut insert =
            out.prepare("INSERT INTO paths (id, unit_id, path) VALUES (?1, ?2, ?3)")?;
        for r in rows {
            let (id, unit_id, path) = r?;
            insert.execute(rusqlite::params![id, unit_id, path])?;
        }
    }
    {
        let sql = format!(
            "SELECT snapshot_id, path_id, kind, size_bytes, mtime_ns, sha256, link_target
             FROM file_versions
             WHERE snapshot_id IN ({versions})
             ORDER BY snapshot_id, path_id"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(stage_set_ids.iter()), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<Vec<u8>>>(5)?,
                row.get::<_, Option<String>>(6)?,
            ))
        })?;
        let mut insert = out.prepare(
            "INSERT INTO file_versions (snapshot_id, path_id, kind, size_bytes, mtime_ns,
                                        sha256, link_target)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        for r in rows {
            let (snapshot_id, path_id, kind, size_bytes, mtime_ns, sha256, link_target) = r?;
            insert.execute(rusqlite::params![
                snapshot_id,
                path_id,
                kind,
                size_bytes,
                mtime_ns,
                sha256,
                link_target
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
        let mut out = Connection::open(dir.path().join("catalog.db")).unwrap();
        let commits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&commits);
        out.commit_hook(Some(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            false
        }))
        .unwrap();

        fill(&conn, &[ss_id], &out).unwrap();

        let rows: i64 = out
            .query_row("SELECT COUNT(*) FROM file_versions", [], |r| r.get(0))
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

    fn snapshot_of(conn: &Connection, ss_id: i64) -> i64 {
        conn.query_row(
            "SELECT snapshot_id FROM stage_sets WHERE id = ?1",
            [ss_id],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn entry(path: &str, kind: FileKind, link_target: Option<&str>) -> FileEntry {
        FileEntry {
            path: path.into(),
            kind,
            size_bytes: if kind == FileKind::Dir { 0 } else { 7 },
            mtime_ns: Some(1_788_264_000_000_000_000),
            sha256: (kind == FileKind::Regular).then_some([0xAB; 32]),
            link_target: link_target.map(str::to_string),
        }
    }

    /// ADR-0012 item 258(d): a `catalog.db` of any shape but the one this
    /// tapectl writes is refused, by both readers, naming what was found.
    /// The shapes tapectl wrote before 1.1.0 (`files` for the per-file rows;
    /// before 2026-09-11 also no `tenants`, no `key_fingerprints` and no
    /// `sha256_plain`) are among them, as is a current file with one marker
    /// removed or a stray `files` table added.
    #[test]
    fn a_catalog_db_of_any_other_shape_is_refused_naming_what_was_found() {
        let conn = crate::db::open_memory().unwrap();
        let (_unit_a, ss_a) = insert_unit_snapshot_stageset_slice_file(&conn, "unit-a");
        let dir = tempfile::tempdir().unwrap();
        const FILES: &str = "CREATE TABLE files (
                                 id INTEGER PRIMARY KEY, snapshot_id INTEGER NOT NULL,
                                 path TEXT NOT NULL, size_bytes INTEGER, sha256 TEXT,
                                 modified_at TEXT, is_directory INTEGER);";
        let before_1_1 = format!("DROP TABLE file_versions; DROP TABLE paths; {FILES}");
        let before_09_11 = format!(
            "{before_1_1} DROP TABLE tenants;
             ALTER TABLE stage_sets DROP COLUMN key_fingerprints;
             ALTER TABLE stage_slices DROP COLUMN sha256_plain;"
        );
        let cases: Vec<(&str, String, Vec<&str>)> = vec![
            (
                "before_1_1.db",
                before_1_1.clone(),
                vec![
                    "no `paths` table",
                    "no `file_versions` table",
                    "a `files` table",
                ],
            ),
            (
                "before_09_11.db",
                before_09_11,
                vec![
                    "no `tenants` table",
                    "no `stage_sets.key_fingerprints` column",
                    "no `stage_slices.sha256_plain` column",
                    "no `paths` table",
                    "a `files` table",
                ],
            ),
            (
                "no_tenants.db",
                "DROP TABLE tenants;".into(),
                vec!["no `tenants` table"],
            ),
            (
                "no_receipts.db",
                "ALTER TABLE stage_sets DROP COLUMN key_fingerprints;".into(),
                vec!["no `stage_sets.key_fingerprints` column"],
            ),
            (
                "no_plain_hashes.db",
                "ALTER TABLE stage_slices DROP COLUMN sha256_plain;".into(),
                vec!["no `stage_slices.sha256_plain` column"],
            ),
            ("and_files.db", FILES.into(), vec!["a `files` table"]),
            (
                "no_paths.db",
                "DROP TABLE paths;".into(),
                vec!["no `paths` table"],
            ),
        ];
        for (name, sql, needles) in cases {
            let p = dir.path().join(name);
            write(&conn, &[ss_a], &p).unwrap();
            Connection::open(&p).unwrap().execute_batch(&sql).unwrap();
            for err in [
                read(&p).unwrap_err().to_string(),
                read_all_but_files(&p).unwrap_err().to_string(),
            ] {
                assert!(
                    err.contains("catalog.db is not the shape this tapectl writes"),
                    "{name}: {err}"
                );
                for needle in &needles {
                    assert!(err.contains(needle), "{name}: {needle:?} not in {err}");
                }
            }
        }
        // The positive control: the file `write` makes is read.
        let p = dir.path().join("current.db");
        write(&conn, &[ss_a], &p).unwrap();
        assert_eq!(read(&p).unwrap().files.len(), 1);
    }

    /// ADR-0012 2026-10-12 item 48 (#381's on-tape half): the file records
    /// each entry exactly as the catalog holds it — kind, link target, the
    /// raw hash and the nanosecond mtime — so a rebuild keeps them.
    #[test]
    fn generation_3_records_each_kind_and_link_target() {
        let conn = crate::db::open_memory().unwrap();
        let (_unit, ss_id) = insert_unit_snapshot_stageset_slice_file(&conn, "unit-a");
        let snap_id = snapshot_of(&conn, ss_id);
        let written = vec![
            entry("docs", FileKind::Dir, None),
            entry("docs/b.bin", FileKind::Regular, None),
            entry("docs/latest", FileKind::Symlink, Some("b.bin")),
            entry("docs/pipe", FileKind::Special, None),
        ];
        for e in &written {
            crate::db::files::fixture::insert_entry(&conn, snap_id, e.clone());
        }
        let dir = tempfile::tempdir().unwrap();
        let out_path = dir.path().join("catalog.db");
        write(&conn, &[ss_id], &out_path).unwrap();

        let cat = read(&out_path).unwrap();
        let entries: Vec<FileEntry> = cat.files.iter().map(|f| f.entry.clone()).collect();
        let mut expected = vec![FileEntry {
            path: "a.txt".into(),
            kind: FileKind::Regular,
            size_bytes: 10,
            mtime_ns: None,
            sha256: Some([0xde; 32]),
            link_target: None,
        }];
        expected.extend(written.iter().cloned());
        assert_eq!(entries, expected);
    }

    /// `paths` carries only the paths of the versions on this tape, under
    /// the catalog's own ids: a unit's other versions are not on it.
    #[test]
    fn paths_carry_only_the_written_versions_paths() {
        let conn = crate::db::open_memory().unwrap();
        let (unit_id, ss_v1) = insert_unit_snapshot_stageset_slice_file(&conn, "unit-a");
        let v1 = snapshot_of(&conn, ss_v1);
        crate::db::files::fixture::insert(&conn, v1, "only-in-v1", 1, "regular", None);
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
             VALUES (?1, 2, 'staged', '/tmp', 2, 10)",
            [unit_id],
        )
        .unwrap();
        let v2 = conn.last_insert_rowid();
        crate::db::files::fixture::insert(&conn, v2, "a.txt", 10, "regular", None);
        crate::db::files::fixture::insert(&conn, v2, "only-in-v2", 1, "regular", None);
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size, num_slices)
             VALUES (?1, 'staged', 1000, 1)",
            [v2],
        )
        .unwrap();
        let ss_v2 = conn.last_insert_rowid();

        let dir = tempfile::tempdir().unwrap();
        let out_path = dir.path().join("catalog.db");
        write(&conn, &[ss_v2], &out_path).unwrap();

        let catalog_ids: Vec<(i64, String)> = conn
            .prepare("SELECT id, path FROM paths WHERE path IN ('a.txt', 'only-in-v2') ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let on_tape: Vec<(i64, String)> = Connection::open(&out_path)
            .unwrap()
            .prepare("SELECT id, path FROM paths ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(on_tape, catalog_ids);
        let cat = read(&out_path).unwrap();
        assert!(cat.files.iter().all(|f| f.snapshot_id == v2));
        assert_eq!(cat.files.len(), 2);
    }

    /// A value the writer never records is refused, naming
    /// the row, not guessed at. The file's own CHECKs stop most hand edits;
    /// a file built without them must still not be misread.
    #[test]
    fn a_bad_kind_or_hash_on_tape_is_refused_naming_the_row() {
        let conn = crate::db::open_memory().unwrap();
        let (_unit, ss_id) = insert_unit_snapshot_stageset_slice_file(&conn, "unit-a");
        let dir = tempfile::tempdir().unwrap();
        for (name, edit, needle) in [
            (
                "kind.db",
                "UPDATE file_versions SET kind = 7",
                "kind Integer(7)",
            ),
            (
                "hash.db",
                "UPDATE file_versions SET sha256 = x'00'",
                "a 1-byte blob",
            ),
            (
                "hextext.db",
                "UPDATE file_versions SET sha256 = 'abcd'",
                "sha256 is not 32 bytes",
            ),
        ] {
            let p = dir.path().join(name);
            write(&conn, &[ss_id], &p).unwrap();
            let out = Connection::open(&p).unwrap();
            out.execute_batch(&format!("PRAGMA ignore_check_constraints = 1; {edit};"))
                .unwrap();
            drop(out);
            let err = read(&p).unwrap_err().to_string();
            assert!(
                err.contains("catalog.db file_versions row")
                    && err.contains("\"a.txt\"")
                    && err.contains(needle),
                "{name}: {err}"
            );
        }
    }

    /// Issue #413: a rebuild streams one version's rows at a time, and each
    /// version is one range of a b-tree, not a scan of every row on the tape
    /// (a rebuild reads one version per unit). Streamed version by version,
    /// the rows are exactly what [`read`] returns, in the same order.
    #[test]
    fn a_version_streams_as_one_range() {
        let conn = crate::db::open_memory().unwrap();
        let ss: Vec<i64> = ["unit-a", "unit-b", "unit-c"]
            .iter()
            .map(|u| insert_unit_snapshot_stageset_slice_file(&conn, u).1)
            .collect();
        for (n, &ss_id) in ss.iter().enumerate() {
            let snap = snapshot_of(&conn, ss_id);
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
        assert_eq!(whole.files.len(), 3 + 1 + 2 + 3);

        let (ontape, cat) = read_all_but_files(&out_path).unwrap();
        assert!(cat.files.is_empty(), "the files are left to the stream");
        let spans = FileSpans::read(&ontape).unwrap();
        assert_eq!(spans.snapshot_ids().len(), 3, "one span per version");
        let mut streamed = Vec::new();
        for snap in spans.snapshot_ids() {
            spans
                .stream(&ontape, snap, |entries| {
                    for e in entries {
                        streamed.push(FileRow {
                            snapshot_id: snap,
                            entry: e?,
                        });
                    }
                    Ok(())
                })
                .unwrap();
        }
        assert_eq!(streamed, whole.files);

        let plan: Vec<String> = ontape
            .prepare(&format!("EXPLAIN QUERY PLAN {FILE_VERSIONS_OF_SNAPSHOT}"))
            .unwrap()
            .query_map(rusqlite::params![1, 1, 1], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(
            plan.iter().all(|d| d.starts_with("SEARCH")),
            "one version must be a range of keys, not a scan of every row: {plan:?}"
        );
    }
}
