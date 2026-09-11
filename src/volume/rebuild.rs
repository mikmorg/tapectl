//! `catalog rebuild --from-volume` (#136): reconstruct catalog rows from a
//! sealed tape when the database is gone and there is no backup.
//!
//! Before this, `volume import` registered a bare `volumes` row and nothing
//! else — enough to know a cartridge exists, not enough to restore from it.
//! `restore unit` resolves slices through `write_positions -> writes ->
//! stage_slices -> stage_sets -> snapshots -> volumes`, so a catalog missing
//! those rows cannot serve a restore however intact the tape is.
//!
//! # Where each fact comes from
//!
//! | Fact | Source | Why not elsewhere |
//! |---|---|---|
//! | label, uuid, media, capacity | ID thunk (File 0) | the tape's own claim |
//! | envelope positions | front index (File 3) | the plaintext map |
//! | unit name/uuid, version, slice map, `sha256_plain` | envelope `MANIFEST.toml` | the front index may carry none of it (sacred invariant), and the on-tape `catalog.db` omits `sha256_plain` |
//! | tenant name per unit | each tenant envelope's manifest | the operator manifest files every unit under the placeholder tenant `"operator"` |
//! | per-file index, `source_path`, sizes | operator envelope's `catalog.db` (#83) | no manifest carries files |
//!
//! # What it deliberately does not do
//!
//! **It does not verify the tape.** `volume verify` exists and does it
//! properly; bundling a second, weaker verification into a rebuild would
//! produce a command whose success means something different from either.
//! The slice hashes recorded here are the ones the tape claims for itself —
//! run `volume verify` afterwards to make them a checked claim.
//!
//! **It inserts only what is missing, and never edits what it finds.** Run it
//! over ten cartridges in any order, twice, and the result is the same
//! catalog. This is what makes a rebuild safe to run against a catalog that
//! is damaged rather than absent: it can only ever add.
//!
//! **It records provenance as an `events` row, not a column.** A rebuilt row
//! is not a different kind of row — ADR-0001 has the catalog as a ledger of
//! claims, and "tapectl claims this because the tape said so" is exactly the
//! sort of claim the events log exists to hold.
//!
//! # Escrow coverage
//!
//! For tapes written after 2026-09-11 the recorded recipient list rides in
//! `catalog.db` and is copied into `stage_sets.key_fingerprints`, so coverage
//! is recorded, not guessed. For older tapes it is NULL: `origin = 'rebuilt'`
//! (migration 010) lets `policy::escrow` report that as **unknown** — still
//! not covered, every gate still refuses — with the attest path named
//! (#137). Attestation is `catalog rebuild --key <escrow>`.

use std::collections::HashMap;
use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};

use crate::error::{Result, TapectlError};
use crate::store::{Store, TapeStore};
use crate::volume::envelope::{self, EnvelopeManifest, OpenError, OpenedEnvelope};
use crate::volume::format;

/// What a rebuild inserted. Every counter is rows actually written — a second
/// run over the same cartridge reports zeroes, which is the idempotence
/// claim made observable rather than asserted.
#[derive(Debug, Clone, Default)]
pub struct RebuildReport {
    pub label: String,
    pub uuid: String,
    pub envelopes_opened: usize,
    pub tenants: usize,
    pub units: usize,
    pub snapshots: usize,
    pub stage_sets: usize,
    pub slices: usize,
    pub writes: usize,
    pub positions: usize,
    pub files: usize,
    pub volume_inserted: bool,
    /// False when the operator envelope carried no `catalog.db` — a tape
    /// written before #83. The restore path is fully rebuilt either way;
    /// what is lost is the per-file index (`catalog ls`/`search`) and each
    /// snapshot's original `source_path`.
    pub had_catalog_db: bool,
    /// True when `catalog.db` carried a `tenants` table (written after the
    /// 2026-09-11 decision), so ownership came from one file rather than from
    /// decrypting every tenant envelope.
    pub tenants_from_catalog_db: bool,
    /// Stage sets whose escrow receipt (`key_fingerprints`) rode the tape in
    /// `catalog.db`. For those, #137 does not arise: coverage is recorded,
    /// not unknown.
    pub receipts_from_tape: usize,
    /// Units whose tenant could not be read from any tenant envelope on this
    /// cartridge, and were therefore filed under `--tenant`'s fallback.
    pub units_without_tenant_envelope: Vec<String>,
}

impl RebuildReport {
    /// True when the run changed nothing — the catalog already knew this
    /// cartridge whole.
    pub fn is_noop(&self) -> bool {
        !self.volume_inserted
            && self.tenants == 0
            && self.units == 0
            && self.snapshots == 0
            && self.stage_sets == 0
            && self.slices == 0
            && self.writes == 0
            && self.positions == 0
            && self.files == 0
    }
}

/// Read a sealed volume and insert whatever catalog rows are missing.
///
/// `key_path` must be an operator or the escrow secret key. A tenant key
/// cannot open the operator envelope (its recipients are operator + escrow
/// only) and is refused with a pointer at `RESTORE.sh`, which is the heir
/// path and does not need a catalog at all.
#[allow(clippy::too_many_arguments)]
pub fn rebuild_from_volume(
    conn: &Connection,
    device: &str,
    block_size: usize,
    key_path: &Path,
    expect_label: Option<&str>,
    fallback_tenant: &str,
    backend_name: Option<&str>,
    scratch: &Path,
) -> Result<RebuildReport> {
    let secret = crate::crypto::keys::read_secret_key(key_path)?;
    let identity: age::x25519::Identity = secret.parse().map_err(|e| {
        TapectlError::Encryption(format!("invalid key in {}: {e}", key_path.display()))
    })?;
    let mut store = TapeStore::open_read(device, block_size)?;
    rebuild_from_store(
        conn,
        &mut store,
        &[identity],
        expect_label,
        fallback_tenant,
        backend_name,
        scratch,
        device,
    )
}

/// The whole of the rebuild, over any [`Store`].
///
/// The store is a parameter of the PRODUCTION function, not a shape the tests
/// re-create for themselves: `MemStore` and `TapeStore` share one chain-walk
/// implementation, so a test that drives this drives the real logic. A test
/// harness that instead re-implemented the walk would be a fixture simpler
/// than the artifact — it would pass while the shipped path was broken.
#[allow(clippy::too_many_arguments)]
pub fn rebuild_from_store(
    conn: &Connection,
    store: &mut dyn Store,
    identities: &[age::x25519::Identity],
    expect_label: Option<&str>,
    fallback_tenant: &str,
    // The configured LTO backend's name; `None` falls back to the backend
    // type, matching `volume_import`.
    backend_name: Option<&str>,
    scratch: &Path,
    device_label: &str,
) -> Result<RebuildReport> {
    let mut thunk = Vec::new();
    store.read_file(0, &mut thunk)?;
    let thunk_text = String::from_utf8_lossy(&thunk).to_string();
    let ident = format::parse_id_thunk_identity(&thunk_text)?;
    let pointers = format::parse_id_thunk_layout_pointers(&thunk_text)?;
    let meta = format::parse_id_thunk_volume_meta(&thunk_text)?;

    if let Some(expected) = expect_label {
        if expected != ident.label {
            return Err(TapectlError::Other(format!(
                "wrong tape: expected label \"{expected}\", found \"{}\" (uuid {})",
                ident.label, ident.uuid
            )));
        }
    }

    let mut fi = Vec::new();
    store.read_file(pointers.front_index as u32, &mut fi)?;
    let entries = format::parse_front_index(&String::from_utf8_lossy(&fi))?;

    let opened = open_all_envelopes(store, &entries, identities, scratch)?;
    let operator = opened
        .iter()
        .find(|e| e.manifest.is_operator())
        .ok_or_else(|| {
            TapectlError::Other(
                "no operator envelope could be opened on this tape — \
                 a rebuild needs the complete unit list it carries"
                    .to_string(),
            )
        })?;

    let mut report = RebuildReport {
        label: ident.label.clone(),
        uuid: ident.uuid.clone(),
        envelopes_opened: opened.len(),
        had_catalog_db: operator.catalog_db.is_some(),
        ..Default::default()
    };

    let supplement = match operator.catalog_db.as_deref() {
        Some(path) => Supplement::load(path)?,
        None => Supplement::default(),
    };
    let tenant_of = tenant_index(&opened, &supplement);
    report.tenants_from_catalog_db = supplement.has_tenants;

    // `unchecked_transaction` matches the codebase's convention (session,
    // write, key, operations): the CLI holds a shared `Connection`, and
    // requiring `&mut` here would ripple through every caller for nothing.
    let tx = conn.unchecked_transaction()?;
    let volume_id = insert_all(
        &tx,
        &ident.label,
        &ident.uuid,
        &meta,
        &operator.manifest,
        &tenant_of,
        fallback_tenant,
        backend_name,
        &supplement,
        &mut report,
    )?;
    record_event(&tx, &report, volume_id, device_label)?;
    tx.commit()?;

    Ok(report)
}

/// Open every envelope the front index names, skipping the ones this key is
/// not a recipient of.
///
/// A tenant envelope that will not open is routine and silent — an operator
/// key opens all of them, but running with a tenant key opens exactly one.
/// The operator envelope failing to open is the one case worth a specific
/// diagnosis, because it is what a tenant key looks like from here, so it
/// falls through to `operator_envelope_backup` (the redundant copy exists
/// precisely for a damaged primary) before giving up.
fn open_all_envelopes(
    store: &mut dyn Store,
    entries: &[format::ParsedIndexEntry],
    identities: &[age::x25519::Identity],
    scratch: &Path,
) -> Result<Vec<OpenedEnvelope>> {
    let mut out = Vec::new();
    let mut operator_refused = false;

    for entry in entries {
        let is_envelope = matches!(
            entry.type_label.as_str(),
            "tenant_envelope" | "operator_envelope" | "operator_envelope_backup"
        );
        if !is_envelope {
            continue;
        }
        // The backup is byte-identical to the primary; opening it when the
        // primary already parsed would only duplicate every unit.
        if entry.type_label == "operator_envelope_backup"
            && out
                .iter()
                .any(|e: &OpenedEnvelope| e.manifest.is_operator())
        {
            continue;
        }

        match envelope::open_envelope(store, entry, identities, scratch) {
            Ok(env) => out.push(env),
            Err(OpenError::NoMatchingKey) => {
                if entry.type_label.starts_with("operator_envelope") {
                    operator_refused = true;
                }
                tracing::debug!(
                    position = entry.position,
                    type_label = %entry.type_label,
                    "rebuild: key is not a recipient of this envelope, skipping"
                );
            }
            Err(OpenError::Failed(e)) => {
                if entry.type_label == "operator_envelope" {
                    // Damaged primary: the backup is the whole reason there
                    // are two, so carry on rather than aborting the rebuild.
                    tracing::warn!(
                        position = entry.position,
                        error = %e,
                        "rebuild: operator envelope unreadable, falling back to the backup copy"
                    );
                    continue;
                }
                return Err(e);
            }
        }
    }

    if operator_refused && !out.iter().any(|e| e.manifest.is_operator()) {
        return Err(TapectlError::Other(
            "this key cannot open the operator envelope, so it is neither an \
             operator key nor the escrow key.\n\n\
             A tenant key restores that tenant's own data without a catalog at \
             all: run RESTORE.sh from tape file 2 —\n\n    \
             tapectl restore raw-volume --device DEV --dest DIR\n    \
             bash DIR/0002_restore_script.bin --restore --unit UNIT --key KEYFILE --to DIR\n\n\
             `catalog rebuild` reconstructs the operator's catalog and needs the \
             operator's view of the tape."
                .to_string(),
        ));
    }

    Ok(out)
}

/// Map each unit name to the tenant that owns it.
///
/// From `catalog.db` when it carries `tenants` (tapes written after
/// 2026-09-11), else from the tenant envelopes — the operator envelope files
/// every unit under the placeholder tenant `"operator"` and so cannot answer
/// this. On a new-shape tape the envelopes are still opened (they are how a
/// tenant key would be refused, and they are cheap), but ownership is taken
/// from the one file that states it.
fn tenant_index(opened: &[OpenedEnvelope], supplement: &Supplement) -> HashMap<String, String> {
    let mut map = supplement.tenant_of.clone();
    for env in opened {
        if env.manifest.is_operator() {
            continue;
        }
        for unit in &env.manifest.units {
            map.entry(unit.name.clone())
                .or_insert_with(|| env.manifest.manifest.tenant.clone());
        }
    }
    map
}

/// The facts only the operator envelope's `catalog.db` (#83) carries.
#[derive(Debug, Default)]
struct Supplement {
    /// unit name -> (source_path, total_size, file_count) of each snapshot
    /// version, keyed by version.
    snapshots: HashMap<(String, i64), SnapshotFacts>,
    /// unit name -> slice_size for the stage set.
    slice_size: HashMap<String, i64>,
    /// (unit name, version) -> the file rows of that snapshot.
    files: HashMap<(String, i64), Vec<FileRow>>,
    /// unit name -> owning tenant name, when `catalog.db` carries `tenants`.
    tenant_of: HashMap<String, String>,
    /// unit name -> recorded recipient list JSON, when `catalog.db` carries
    /// `stage_sets.key_fingerprints`.
    key_fingerprints: HashMap<String, String>,
    /// Whether the `tenants` table was present at all — distinguishes "no
    /// tenants on this write" from "an older catalog.db".
    has_tenants: bool,
}

#[derive(Debug, Clone)]
struct SnapshotFacts {
    source_path: String,
    snapshot_type: String,
    total_size: Option<i64>,
    file_count: Option<i64>,
}

#[derive(Debug, Clone)]
struct FileRow {
    path: String,
    size_bytes: i64,
    sha256: Option<String>,
    modified_at: Option<String>,
    is_directory: i64,
}

impl Supplement {
    fn load(path: &Path) -> Result<Self> {
        let db = Connection::open(path)?;
        let mut out = Supplement::default();

        {
            let mut stmt = db.prepare(
                "SELECT u.name, s.version, s.source_path, s.snapshot_type, s.total_size, s.file_count
                 FROM snapshots s JOIN units u ON u.id = s.unit_id",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    SnapshotFacts {
                        source_path: r.get(2)?,
                        snapshot_type: r.get(3)?,
                        total_size: r.get(4)?,
                        file_count: r.get(5)?,
                    },
                ))
            })?;
            for row in rows {
                let (name, version, facts) = row?;
                out.snapshots.insert((name, version), facts);
            }
        }

        {
            let mut stmt = db.prepare(
                "SELECT u.name, ss.slice_size
                 FROM stage_sets ss
                 JOIN snapshots s ON s.id = ss.snapshot_id
                 JOIN units u ON u.id = s.unit_id",
            )?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
            for row in rows {
                let (name, size) = row?;
                out.slice_size.insert(name, size);
            }
        }

        {
            let mut stmt = db.prepare(
                "SELECT u.name, s.version, f.path, f.size_bytes, f.sha256, f.modified_at,
                        f.is_directory
                 FROM files f
                 JOIN snapshots s ON s.id = f.snapshot_id
                 JOIN units u ON u.id = s.unit_id",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    FileRow {
                        path: r.get(2)?,
                        size_bytes: r.get(3)?,
                        sha256: r.get(4)?,
                        modified_at: r.get(5)?,
                        is_directory: r.get(6)?,
                    },
                ))
            })?;
            for row in rows {
                let (name, version, file) = row?;
                out.files.entry((name, version)).or_default().push(file);
            }
        }

        // Written after 2026-09-11? Probe the shape rather than trust a
        // version number: a `tenants` table and a `key_fingerprints` column.
        let has_tenants: bool = db.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'tenants'",
            [],
            |r| r.get::<_, i64>(0),
        )? > 0;
        let has_receipts: bool = db
            .prepare("PRAGMA table_info(stage_sets)")?
            .query_map([], |r| r.get::<_, String>(1))?
            .filter_map(|r| r.ok())
            .any(|c| c == "key_fingerprints");
        out.has_tenants = has_tenants;

        if has_tenants {
            let mut stmt = db.prepare(
                "SELECT u.name, t.name FROM units u JOIN tenants t ON t.id = u.tenant_id",
            )?;
            let rows =
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            for row in rows {
                let (unit, tenant) = row?;
                out.tenant_of.insert(unit, tenant);
            }
        }
        if has_receipts {
            let mut stmt = db.prepare(
                "SELECT u.name, ss.key_fingerprints
                 FROM stage_sets ss
                 JOIN snapshots s ON s.id = ss.snapshot_id
                 JOIN units u ON u.id = s.unit_id
                 WHERE ss.key_fingerprints IS NOT NULL",
            )?;
            let rows =
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            for row in rows {
                let (unit, json) = row?;
                out.key_fingerprints.insert(unit, json);
            }
        }

        Ok(out)
    }
}

/// Insert every missing row for one cartridge. Called inside a transaction:
/// a rebuild that fails partway leaves the catalog exactly as it found it,
/// rather than half-knowing a tape.
#[allow(clippy::too_many_arguments)]
fn insert_all(
    tx: &Connection,
    label: &str,
    uuid: &str,
    meta: &format::IdThunkVolumeMeta,
    operator: &EnvelopeManifest,
    tenant_of: &HashMap<String, String>,
    fallback_tenant: &str,
    backend_name: Option<&str>,
    supplement: &Supplement,
    report: &mut RebuildReport,
) -> Result<i64> {
    // Resolved the same way `cli::operations::volume_import` does, falling
    // back to the type string — never an invented name like "rebuilt", which
    // would put a backend in the catalog that no config declares.
    let backend_name = backend_name.unwrap_or("lto");
    let volume_id = match existing_id(
        tx,
        "SELECT id FROM volumes WHERE label = ?1",
        params![label],
    )? {
        Some(id) => id,
        None => {
            tx.execute(
                "INSERT INTO volumes (label, uuid, backend_type, backend_name, media_type,
                                      capacity_bytes, mam_capacity_bytes, has_manifest, status)
                 VALUES (?1, ?2, 'lto', ?3, ?4, ?5, ?6, 1, 'sealed')",
                params![
                    label,
                    uuid,
                    backend_name,
                    meta.media_type,
                    meta.nominal_capacity_bytes,
                    meta.mam_capacity_bytes,
                ],
            )?;
            report.volume_inserted = true;
            tx.last_insert_rowid()
        }
    };

    for unit in &operator.units {
        // Resolve the unit BEFORE touching `tenants`: a unit the catalog
        // already knows keeps the tenant it already has, and creating that
        // tenant first would leave an orphan row behind for a unit whose
        // ownership was never in question.
        let unit_id = match existing_unit(tx, unit)? {
            Some(id) => id,
            None => {
                let tenant_name = match tenant_of.get(&unit.name) {
                    Some(t) => t.clone(),
                    None => {
                        report.units_without_tenant_envelope.push(unit.name.clone());
                        fallback_tenant.to_string()
                    }
                };
                let tenant_id = ensure_tenant(tx, &tenant_name, report)?;
                insert_unit(tx, unit, tenant_id, report)?
            }
        };
        let snapshot_id = ensure_snapshot(tx, unit, unit_id, supplement, report)?;
        let stage_set_id = ensure_stage_set(tx, unit, snapshot_id, supplement, report)?;

        let mut slice_ids = Vec::with_capacity(unit.slices.len());
        for slice in &unit.slices {
            slice_ids.push((slice, ensure_slice(tx, slice, stage_set_id, report)?));
        }

        let write_id = ensure_write(tx, stage_set_id, snapshot_id, volume_id, report)?;
        for (slice, slice_id) in slice_ids {
            ensure_position(tx, write_id, slice_id, slice, report)?;
        }

        ensure_files(tx, unit, snapshot_id, supplement, report)?;
    }

    Ok(volume_id)
}

fn existing_id(tx: &Connection, sql: &str, p: impl rusqlite::Params) -> Result<Option<i64>> {
    Ok(tx.query_row(sql, p, |r| r.get::<_, i64>(0)).optional()?)
}

fn ensure_tenant(tx: &Connection, name: &str, report: &mut RebuildReport) -> Result<i64> {
    if let Some(id) = existing_id(tx, "SELECT id FROM tenants WHERE name = ?1", params![name])? {
        return Ok(id);
    }
    // A tenant row carries no key material — `public_key` lives on
    // `encryption_keys`, and `restore` loads identities from `keys/` on
    // disk, never from the DB. So a rebuilt tenant is structurally complete,
    // not a stub with a hole in it; what it lacks is key ROWS, which is the
    // truth (the tape records no recipient list — see #137).
    tx.execute(
        "INSERT INTO tenants (name, notes) VALUES (?1, ?2)",
        params![
            name,
            "rebuilt from a volume's envelope by `catalog rebuild` — no encryption_keys \
             rows were reconstructed, because no tape records a recipient list"
        ],
    )?;
    report.tenants += 1;
    Ok(tx.last_insert_rowid())
}

/// Find a unit the catalog already has.
///
/// Matched on uuid first: the uuid is the unit's identity, and the name can
/// legitimately have been changed by `unit rename` since the tape was
/// written. Falling back to name catches the reverse — a unit re-created
/// locally under the same name with a fresh uuid — where inserting would hit
/// the `UNIQUE(name)` constraint and abort the whole rebuild.
fn existing_unit(tx: &Connection, unit: &envelope::ManifestUnit) -> Result<Option<i64>> {
    if let Some(id) = existing_id(
        tx,
        "SELECT id FROM units WHERE uuid = ?1",
        params![unit.uuid],
    )? {
        return Ok(Some(id));
    }
    existing_id(
        tx,
        "SELECT id FROM units WHERE name = ?1",
        params![unit.name],
    )
}

fn insert_unit(
    tx: &Connection,
    unit: &envelope::ManifestUnit,
    tenant_id: i64,
    report: &mut RebuildReport,
) -> Result<i64> {
    // `current_path` stays NULL: the tape says where the data CAME from, not
    // where it lives now, and a rebuilt unit pointing at a path that may no
    // longer exist would invite `snapshot create` to archive the wrong thing.
    //
    // Status is `active`, NOT `tape_only`, for two independent reasons.
    //
    // On the merits: `tape_only` is a POLICY state that `unit mark-tape-only`
    // sets deliberately after checking enforced preconditions (min_copies,
    // min_locations) and it asserts "the source is deleted, the tape is all
    // there is". A rebuild knows nothing of the sort — it has read a tape,
    // not looked at anyone's disk.
    //
    // And in effect: `audit` scopes its per-unit checks to `status = 'active'`
    // (`cli::audit`'s `list_units(conn, None, Some("active"))`). A rebuilt
    // unit marked `tape_only` is invisible to every one of them, so a catalog
    // rebuilt after a disaster reported ZERO violations where the catalog it
    // replaced reported three `copy_count` violations for the same units on
    // the same single tape. Under-reporting risk to an operator who has just
    // lost their database is the worst possible direction for this to fail
    // in — the #105 lesson, in a new place: a silent downgrade is strictly
    // worse than a known violation, because the tool cannot tell.
    tx.execute(
        "INSERT INTO units (uuid, name, tenant_id, status) VALUES (?1, ?2, ?3, 'active')",
        params![unit.uuid, unit.name, tenant_id],
    )?;
    report.units += 1;
    Ok(tx.last_insert_rowid())
}

fn ensure_snapshot(
    tx: &Connection,
    unit: &envelope::ManifestUnit,
    unit_id: i64,
    supplement: &Supplement,
    report: &mut RebuildReport,
) -> Result<i64> {
    if let Some(id) = existing_id(
        tx,
        "SELECT id FROM snapshots WHERE unit_id = ?1 AND version = ?2",
        params![unit_id, unit.snapshot_version],
    )? {
        return Ok(id);
    }
    let facts = supplement
        .snapshots
        .get(&(unit.name.clone(), unit.snapshot_version));
    // `source_path` is NOT NULL and is only in `catalog.db`. On a pre-#83
    // tape it is genuinely unknown, and an invented path would be a lie a
    // later `snapshot create` could act on — so say so in the column.
    let source_path = facts
        .map(|f| f.source_path.clone())
        .unwrap_or_else(|| "(unknown: rebuilt from a tape carrying no catalog.db)".to_string());
    let snapshot_type = facts
        .map(|f| f.snapshot_type.clone())
        .unwrap_or_else(|| "full".to_string());
    // 'current' matches what the write path itself produces: `session`'s seal
    // promotes each written snapshot to 'current' and never demotes its
    // predecessor, so several 'current' versions per unit is the normal
    // shape of this catalog, not an anomaly a rebuild introduces.
    tx.execute(
        "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path,
                                total_size, file_count)
         VALUES (?1, ?2, ?3, 'current', ?4, ?5, ?6)",
        params![
            unit_id,
            unit.snapshot_version,
            snapshot_type,
            source_path,
            facts.and_then(|f| f.total_size),
            facts.and_then(|f| f.file_count),
        ],
    )?;
    report.snapshots += 1;
    Ok(tx.last_insert_rowid())
}

fn ensure_stage_set(
    tx: &Connection,
    unit: &envelope::ManifestUnit,
    snapshot_id: i64,
    supplement: &Supplement,
    report: &mut RebuildReport,
) -> Result<i64> {
    // The manifest's `stage_set_id` is an id in the DB that WROTE the tape
    // and means nothing here; reusing it would collide with live rows. The
    // stable key is the snapshot.
    if let Some(id) = existing_id(
        tx,
        "SELECT id FROM stage_sets WHERE snapshot_id = ?1 ORDER BY id LIMIT 1",
        params![snapshot_id],
    )? {
        return Ok(id);
    }
    let slice_size = supplement
        .slice_size
        .get(&unit.name)
        .copied()
        .or_else(|| unit.slices.iter().map(|s| s.size_bytes).max())
        .unwrap_or(0);
    // The escrow receipt rides the tape in `catalog.db` for volumes written
    // after 2026-09-11 (review finding 2); for older tapes it is NULL and
    // `origin = 'rebuilt'` lets `policy::escrow` say "unknown — attest it"
    // rather than "no recorded recipient list" (#137).
    let receipt = supplement.key_fingerprints.get(&unit.name).cloned();
    if receipt.is_some() {
        report.receipts_from_tape += 1;
    }
    tx.execute(
        "INSERT INTO stage_sets (snapshot_id, status, origin, dar_version, dar_command, slice_size,
                                 num_slices, total_dar_size, total_encrypted_size, staged_at, notes,
                                 key_fingerprints)
         VALUES (?1, 'cleaned', 'rebuilt', ?2, ?3, ?4, ?5, ?6, ?7, datetime('now'), ?8, ?9)",
        params![
            snapshot_id,
            unit.dar_version,
            unit.dar_command,
            slice_size,
            unit.slices.len() as i64,
            unit.slices.iter().map(|s| s.size_bytes).sum::<i64>(),
            unit.slices.iter().map(|s| s.encrypted_bytes).sum::<i64>(),
            "rebuilt from the volume's envelope manifest; slices live on tape only",
            receipt,
        ],
    )?;
    report.stage_sets += 1;
    Ok(tx.last_insert_rowid())
}

fn ensure_slice(
    tx: &Connection,
    slice: &envelope::ManifestSlice,
    stage_set_id: i64,
    report: &mut RebuildReport,
) -> Result<i64> {
    if let Some(id) = existing_id(
        tx,
        "SELECT id FROM stage_slices WHERE stage_set_id = ?1 AND slice_number = ?2",
        params![stage_set_id, slice.number],
    )? {
        return Ok(id);
    }
    // `staging_path` stays NULL: the slice is on tape, not in staging, and a
    // path here would send `staging clean` hunting for a file that never
    // existed on this machine.
    tx.execute(
        "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes, encrypted_bytes,
                                   sha256_plain, sha256_encrypted)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            stage_set_id,
            slice.number,
            slice.size_bytes,
            slice.encrypted_bytes,
            slice.sha256_plain,
            slice.sha256_encrypted,
        ],
    )?;
    report.slices += 1;
    Ok(tx.last_insert_rowid())
}

fn ensure_write(
    tx: &Connection,
    stage_set_id: i64,
    snapshot_id: i64,
    volume_id: i64,
    report: &mut RebuildReport,
) -> Result<i64> {
    if let Some(id) = existing_id(
        tx,
        "SELECT id FROM writes WHERE stage_set_id = ?1 AND volume_id = ?2",
        params![stage_set_id, volume_id],
    )? {
        return Ok(id);
    }
    // `write_verified = 0`: the tape has not been read back and checked.
    // `volume verify` is what sets that, and a rebuild claiming it would
    // make a verified volume indistinguishable from an assumed one.
    tx.execute(
        "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status, write_verified,
                             completed_at, notes)
         VALUES (?1, ?2, ?3, 'completed', 0, datetime('now'),
                 'rebuilt from the volume itself; never verified by a read-back')",
        params![stage_set_id, snapshot_id, volume_id],
    )?;
    report.writes += 1;
    Ok(tx.last_insert_rowid())
}

fn ensure_position(
    tx: &Connection,
    write_id: i64,
    stage_slice_id: i64,
    slice: &envelope::ManifestSlice,
    report: &mut RebuildReport,
) -> Result<()> {
    if existing_id(
        tx,
        "SELECT id FROM write_positions WHERE write_id = ?1 AND stage_slice_id = ?2",
        params![write_id, stage_slice_id],
    )?
    .is_some()
    {
        return Ok(());
    }
    // `position` is the TAPE POSITION, never the slice number: slices begin
    // after the envelopes and the two diverge further on a multi-unit
    // volume. Confusing them yields a restore that reads the wrong files.
    tx.execute(
        "INSERT INTO write_positions (write_id, stage_slice_id, position, status, written_at,
                                      sha256_on_volume)
         VALUES (?1, ?2, ?3, 'written', datetime('now'), ?4)",
        params![
            write_id,
            stage_slice_id,
            slice.tape_position.to_string(),
            slice.sha256_encrypted,
        ],
    )?;
    report.positions += 1;
    Ok(())
}

fn ensure_files(
    tx: &Connection,
    unit: &envelope::ManifestUnit,
    snapshot_id: i64,
    supplement: &Supplement,
    report: &mut RebuildReport,
) -> Result<()> {
    let Some(files) = supplement
        .files
        .get(&(unit.name.clone(), unit.snapshot_version))
    else {
        return Ok(());
    };
    let mut stmt = tx.prepare(
        "INSERT OR IGNORE INTO files (snapshot_id, path, size_bytes, sha256, modified_at,
                                      is_directory)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    for f in files {
        let changed = stmt.execute(params![
            snapshot_id,
            f.path,
            f.size_bytes,
            f.sha256,
            f.modified_at,
            f.is_directory,
        ])?;
        report.files += changed;
    }
    Ok(())
}

/// Provenance as an `events` row, per ADR-0001 — the catalog is a ledger of
/// claims, and this records which claims came from a tape rather than from
/// having done the work.
fn record_event(
    tx: &Connection,
    report: &RebuildReport,
    volume_id: i64,
    device: &str,
) -> Result<()> {
    let detail = format!(
        "catalog rebuild from volume {} (uuid {}) on {}: {} envelope(s) opened; \
         inserted {} tenant(s), {} unit(s), {} snapshot(s), {} stage set(s), {} slice(s), \
         {} write(s), {} position(s), {} file row(s); {} escrow receipt(s) from tape; catalog.db {}",
        report.label,
        report.uuid,
        device,
        report.envelopes_opened,
        report.tenants,
        report.units,
        report.snapshots,
        report.stage_sets,
        report.slices,
        report.writes,
        report.positions,
        report.files,
        report.receipts_from_tape,
        if report.had_catalog_db {
            "present"
        } else {
            "absent (pre-#83 tape): no file index, no source paths"
        },
    );
    crate::db::events::log_event(
        tx,
        "volume",
        volume_id,
        Some(&report.label),
        "catalog_rebuild",
        None,
        None,
        None,
        Some(&detail),
        None,
    )?;
    Ok(())
}
