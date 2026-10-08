//! `volume resume` adopts a volume the catalog lost mid-write (issue #360,
//! ADR-0012's amendment "2026-09-29 (later)").
//!
//! **The state.** A catalog restored from a backup taken after `volume init`
//! but before `volume write` began holds the volume as `initialized` with no
//! `writes` rows, while the tape itself was sealed and confirmed. `catalog
//! rebuild --from-volume` attaches its units — `completed` writes rows, never
//! verified — but never changes a status it finds (the 2026-09-16 ruling,
//! which stands). Before this module nothing could make such a volume count:
//! `volume resume` had no session to adopt, and refused the row as
//! `VolumeHasRecordedWrite`.
//!
//! **The ruling.** Resume adopts such a volume on all of the following,
//! conjunctively, and refuses naming the first unmet one and the command that
//! resolves it:
//!
//! 1. **the same volume, actually written:** the tape's File 0 uuid equals
//!    the catalog row's uuid, and a valid seal marker binds its front index;
//! 2. **every byte read back good:** a passing full `volume verify` of this
//!    volume is recorded after the rebuild — a recorded row, not an
//!    inference;
//! 3. **it holds what this catalog staged:** every ciphertext hash in the
//!    tape's front index equals the hash the catalog recorded when it staged
//!    those slices (`stage_slices`, which the pre-write backup carries). This
//!    is the confirm, against the catalog's own plan — never the tape
//!    vouching for itself, which is why a stage set the rebuild created from
//!    the tape (`stage_sets.origin = 'rebuilt'`) never satisfies it.
//!
//! When all three hold, [`adopt`] writes the session facts a passing confirm
//! would have written — `status = 'sealed'`, `sealed_at`, the write
//! bookkeeping, the snapshot promotions and a `write_completed` event — in
//! one transaction. Resume does not run the verify itself: the ruling asks
//! for a recorded one, as the #280 adoption does, and names `volume verify`
//! when there is none.
//!
//! [`recorded_evidence`] is condition 2 alone, from rows: `volume resume`
//! asks it before the tape moves, so a resume with no verify behind it is
//! refused without a contact. [`adopt`] asks all three in the ruling's order.

use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};

use crate::db::events;
use crate::error::{Result, TapectlError};
use crate::store::Store;
use crate::volume::format;
use crate::volume::layout_model::{CapacityBudget, ContentSource, Layout, LayoutEntry, ZoneKind};

/// What [`adopt`] (or, for condition 2 alone, [`recorded_evidence`]) found.
/// Every variant but `Adopted` is a refusal, in the ruling's order: the first
/// unmet condition is the one reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LostAdoption {
    /// All three conditions held; the volume is now `sealed`.
    Adopted {
        /// Slice files the front index lists, each matched to its staged hash.
        slices: usize,
    },
    /// Condition 1, first half: File 0 is unreadable, unparseable, or names
    /// a different volume than the catalog row (`found` says which).
    IdentityMismatch {
        expected_uuid: String,
        found: String,
    },
    /// Condition 1, second half: no valid seal marker binds the front index
    /// — the write never finished on this tape, or its tail or File 3 does
    /// not read back as what was sealed.
    NotSealed { why: String },
    /// Condition 2's anchor is missing: no `catalog rebuild --from-volume`
    /// of this volume is recorded, so nothing says the writes rows on it
    /// came from this tape.
    NoRecordedRebuild,
    /// Condition 2: no `verification_sessions` row with `verify_type =
    /// 'full'` and `outcome = 'passed'` STARTED strictly after the latest
    /// recorded rebuild.
    NoCleanFullVerifyAfterRebuild { rebuilt_at: String },
    /// Condition 3: the tape's slices are not the ones this catalog staged.
    NotWhatThisCatalogStaged { why: String },
}

/// Whether `volume_id` is in the state #360 is about: `initialized`, its
/// seal never recorded, its condition `ok`, and writes rows that are all
/// `completed` — what a rebuild attaches to a row a pre-write backup left
/// behind. A recorded seal is #280's territory (an interrupted or aborted
/// session of this catalog's own), and any unresolved row is a session
/// `volume resume` continues the ordinary way, so neither is adopted here.
pub fn is_lost_write(conn: &Connection, volume_id: i64) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT v.status = 'initialized' AND v.sealed_at IS NULL
                AND v.observed_condition = 'ok'
                AND EXISTS(SELECT 1 FROM writes WHERE volume_id = v.id)
                AND NOT EXISTS(SELECT 1 FROM writes
                               WHERE volume_id = v.id AND status <> 'completed')
         FROM volumes v WHERE v.id = ?1",
        params![volume_id],
        |r| r.get(0),
    )?)
}

/// Condition 2 from recorded rows only: `None` when it holds, else its
/// refusal. Strictly later, and by `started_at`, as #280's adoption rule
/// reads its verify: `datetime('now')` has one-second resolution, so a
/// verify recorded in the same second as the rebuild is refused — the safe
/// side.
pub fn recorded_evidence(conn: &Connection, volume_id: i64) -> Result<Option<LostAdoption>> {
    let rebuilt_at: Option<String> = conn.query_row(
        "SELECT MAX(timestamp) FROM events
         WHERE entity_type = 'volume' AND entity_id = ?1 AND action = 'catalog_rebuild'",
        params![volume_id],
        |r| r.get(0),
    )?;
    let Some(rebuilt_at) = rebuilt_at else {
        return Ok(Some(LostAdoption::NoRecordedRebuild));
    };
    let verified_after: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM verification_sessions
                       WHERE volume_id = ?1 AND verify_type = 'full'
                         AND outcome = 'passed' AND started_at > ?2)",
        params![volume_id, rebuilt_at],
        |r| r.get(0),
    )?;
    if !verified_after {
        return Ok(Some(LostAdoption::NoCleanFullVerifyAfterRebuild {
            rebuilt_at,
        }));
    }
    Ok(None)
}

/// Adopt `volume_id` — see the module doc. Reads File 0, the front index and
/// the seal marker from `store` (condition 1 and 3's tape half), then the
/// catalog. Returns the first unmet condition as a refusal, or `Adopted`
/// after writing the session facts. Errs only when the volume is not in the
/// lost-write state at all ([`is_lost_write`]) or on a database error.
pub fn adopt(
    conn: &Connection,
    store: &mut dyn Store,
    volume_id: i64,
    label: &str,
    block_size: u64,
) -> Result<LostAdoption> {
    if !is_lost_write(conn, volume_id)? {
        return Err(TapectlError::Other(format!(
            "volume \"{label}\" is not a write this catalog lost: only an `initialized` volume \
             with no recorded seal, an `ok` condition and nothing but completed writes attached \
             is adopted this way (ADR-0012, 2026-09-29 later)"
        )));
    }
    let expected_uuid: Option<String> = conn.query_row(
        "SELECT uuid FROM volumes WHERE id = ?1",
        params![volume_id],
        |r| r.get(0),
    )?;
    let expected_uuid = expected_uuid.unwrap_or_default();

    // Condition 1, first half: the same volume.
    let file0 = match crate::store::read_small_bytes(store, 0, "ID thunk") {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(e) => {
            return Ok(LostAdoption::IdentityMismatch {
                expected_uuid,
                found: format!("File 0 did not read ({e})"),
            })
        }
    };
    let identity = match format::parse_id_thunk_identity(&file0) {
        Ok(identity) => identity,
        Err(e) => {
            return Ok(LostAdoption::IdentityMismatch {
                expected_uuid,
                found: format!("File 0 does not parse ({e})"),
            })
        }
    };
    if identity.uuid != expected_uuid {
        return Ok(LostAdoption::IdentityMismatch {
            expected_uuid,
            found: format!("label {:?}, uuid {:?}", identity.label, identity.uuid),
        });
    }

    // Condition 1, second half: actually written — a seal binds File 3.
    let tape = match sealed_front_index(store, &file0) {
        Ok(tape) => tape,
        Err(why) => return Ok(LostAdoption::NotSealed { why }),
    };

    // Condition 2: every byte read back good, recorded after the rebuild.
    if let Some(refused) = recorded_evidence(conn, volume_id)? {
        return Ok(refused);
    }

    // Condition 3: the tape holds what this catalog staged.
    let slices = match staged_match(conn, volume_id, &tape.entries)? {
        Ok(slices) => slices,
        Err(why) => return Ok(LostAdoption::NotWhatThisCatalogStaged { why }),
    };

    record_adoption(conn, volume_id, label, block_size, &tape, slices)?;
    Ok(LostAdoption::Adopted { slices })
}

/// What condition 1 read off the tape, once a seal marker bound it.
struct SealedIndex {
    entries: Vec<format::ParsedIndexEntry>,
    /// The seal marker's own `sealed_at` (RFC 3339): when the seal was
    /// written, which is the time `volumes.sealed_at` records.
    sealed_at: String,
    front_index_sha256: String,
}

/// Condition 1's second half: File 0's own layout pointers name the front
/// index and the seal marker; the seal marker parses, is one this tapectl
/// may read, and its `front_index_sha256` is the hash of File 3's true bytes
/// (trailing NUL padding stripped, `volume-format-v2.md` §4). `Err` names
/// what failed, for the refusal.
fn sealed_front_index(
    store: &mut dyn Store,
    file0: &str,
) -> std::result::Result<SealedIndex, String> {
    let pointers = format::parse_id_thunk_layout_pointers(file0)
        .map_err(|e| format!("File 0 records no layout pointers ({e})"))?;
    let fi_pos = u32::try_from(pointers.front_index)
        .map_err(|_| format!("File 0 names front index position {}", pointers.front_index))?;
    let seal_pos = u32::try_from(pointers.seal_marker)
        .map_err(|_| format!("File 0 names seal marker position {}", pointers.seal_marker))?;

    let fi_raw = crate::store::read_small_bytes(store, fi_pos, "front index")
        .map_err(|e| format!("the front index (file {fi_pos}) did not read: {e}"))?;
    let fi_true = &fi_raw[..fi_raw.iter().rposition(|b| *b != 0).map_or(0, |i| i + 1)];
    let front_index_sha256 = format!("{:x}", Sha256::digest(fi_true));
    let entries = format::parse_front_index(&String::from_utf8_lossy(fi_true))
        .map_err(|e| format!("the front index (file {fi_pos}) does not parse: {e}"))?;

    let seal_raw = crate::store::read_small_bytes(store, seal_pos, "seal marker").map_err(|e| {
        format!("no seal marker reads at file {seal_pos}, where File 0 says it is: {e}")
    })?;
    let seal = format::parse_seal_marker(&String::from_utf8_lossy(&seal_raw))
        .map_err(|e| format!("file {seal_pos} is not a seal marker: {e}"))?;
    if let Some(why) = seal.refusal() {
        return Err(why);
    }
    if seal.front_index_sha256 != front_index_sha256 {
        return Err(format!(
            "the seal marker binds a front index hashing to {}, but the front index (file \
             {fi_pos}) on this tape hashes to {front_index_sha256}",
            seal.front_index_sha256
        ));
    }
    Ok(SealedIndex {
        entries,
        sealed_at: seal.sealed_at,
        front_index_sha256,
    })
}

/// Condition 3: every slice the tape's front index lists is, at that
/// position, a slice of a stage set THIS catalog staged (`origin =
/// 'staged'`) with the very hash the catalog recorded; every slice position
/// the catalog attached to this volume is one the front index lists; and
/// every stage set written here is on the tape whole. `Ok(Ok(n))` is the
/// number of slices matched; `Ok(Err(why))` names the first disagreement.
fn staged_match(
    conn: &Connection,
    volume_id: i64,
    entries: &[format::ParsedIndexEntry],
) -> Result<std::result::Result<usize, String>> {
    let tape: Vec<(i64, Option<&str>)> = entries
        .iter()
        .filter(|e| {
            matches!(
                ZoneKind::from_type_label(&e.type_label),
                Some(ZoneKind::Slice { .. })
            )
        })
        .map(|e| (i64::from(e.position), e.sha256_encrypted.as_deref()))
        .collect();

    let mut catalog: std::collections::BTreeMap<i64, (String, String, i64)> =
        std::collections::BTreeMap::new();
    let rows: Vec<(i64, String, String, i64)> = conn
        .prepare(
            "SELECT CAST(wp.position AS INTEGER), sl.sha256_encrypted, ss.origin, ss.id
             FROM write_positions wp
             JOIN writes w ON w.id = wp.write_id
             JOIN stage_slices sl ON sl.id = wp.stage_slice_id
             JOIN stage_sets ss ON ss.id = sl.stage_set_id
             WHERE w.volume_id = ?1",
        )?
        .query_map(params![volume_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    for (position, sha, origin, stage_set_id) in rows {
        if catalog
            .insert(position, (sha, origin, stage_set_id))
            .is_some()
        {
            return Ok(Err(format!(
                "the catalog records two slices of this volume at file {position}"
            )));
        }
    }

    for (position, tape_sha) in &tape {
        let Some(tape_sha) = tape_sha else {
            return Ok(Err(format!(
                "the front index records no hash for the slice at file {position}"
            )));
        };
        let Some((staged_sha, origin, stage_set_id)) = catalog.get(position) else {
            return Ok(Err(format!(
                "the slice at file {position} is attached to nothing this catalog staged \
                 (no slice of this volume is recorded at that position)"
            )));
        };
        if origin != "staged" {
            return Ok(Err(format!(
                "the slice at file {position} belongs to stage set {stage_set_id}, which the \
                 rebuild made from the tape itself (origin `{origin}`): this catalog never \
                 staged it"
            )));
        }
        if staged_sha != tape_sha {
            return Ok(Err(format!(
                "the slice at file {position} hashes to {tape_sha} in the tape's front index, \
                 but this catalog staged {staged_sha}"
            )));
        }
    }
    for position in catalog.keys() {
        if !tape.iter().any(|(p, _)| p == position) {
            return Ok(Err(format!(
                "the catalog records a slice of this volume at file {position}, which the \
                 tape's front index does not list as a slice"
            )));
        }
    }
    let whole: Vec<(i64, i64, i64)> = conn
        .prepare(
            "SELECT w.stage_set_id,
                    (SELECT COUNT(*) FROM stage_slices WHERE stage_set_id = w.stage_set_id),
                    (SELECT COUNT(*) FROM write_positions WHERE write_id = w.id)
             FROM writes w WHERE w.volume_id = ?1 ORDER BY w.id",
        )?
        .query_map(params![volume_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    for (stage_set_id, staged, here) in whole {
        if staged != here {
            return Ok(Err(format!(
                "stage set {stage_set_id} staged {staged} slice(s), but {here} of them are on \
                 this tape"
            )));
        }
    }
    Ok(Ok(tape.len()))
}

/// The session facts a passing confirm writes, written for an adopted lost
/// write: `status = 'sealed'` and `sealed_at` (the seal marker's own time),
/// the write bookkeeping (`write::record_write_bookkeeping`, shared), the
/// snapshot promotions and a `write_completed` event, in one transaction.
/// The promotions' audit rows follow the commit and are best-effort, the
/// ordering `SealedPending::confirm` gives its own reason for.
fn record_adoption(
    conn: &Connection,
    volume_id: i64,
    label: &str,
    block_size: u64,
    tape: &SealedIndex,
    slices: usize,
) -> Result<()> {
    let sealed_at = chrono::DateTime::parse_from_rfc3339(&tape.sealed_at)
        .ok()
        .map(|t| {
            t.with_timezone(&chrono::Utc)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        });
    let layout = Layout {
        label: label.to_string(),
        volume_uuid: String::new(),
        media_type: String::new(),
        block_size,
        budget: CapacityBudget {
            available_bytes: 0,
            reserve_bytes: 0,
        },
        entries: tape
            .entries
            .iter()
            .filter_map(|p| {
                Some(LayoutEntry {
                    position: p.position,
                    kind: ZoneKind::from_type_label(&p.type_label)?,
                    size_bytes: p.size_bytes,
                    sha256: p.sha256_encrypted.clone(),
                    source: ContentSource::Generated,
                })
            })
            .collect(),
    };

    let tx = crate::db::busy::immediate_tx(conn)?;
    let sealed = tx.execute(
        "UPDATE volumes SET status = 'sealed', sealed_at = COALESCE(?2, datetime('now'))
         WHERE id = ?1 AND status = 'initialized' AND sealed_at IS NULL",
        params![volume_id, sealed_at],
    )?;
    if sealed != 1 {
        return Err(TapectlError::Other(format!(
            "volume \"{label}\" changed while `volume resume` read its tape; nothing was \
             adopted. Run the same command again."
        )));
    }
    let snapshots: Vec<i64> = tx
        .prepare("SELECT DISTINCT snapshot_id FROM writes WHERE volume_id = ?1 ORDER BY 1")?
        .query_map(params![volume_id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut promoted: Vec<(i64, String, String, i64)> = Vec::new();
    for snapshot_id in snapshots {
        let before: (String, String, i64) = tx.query_row(
            "SELECT s.status, u.name, u.tenant_id
             FROM snapshots s JOIN units u ON u.id = s.unit_id WHERE s.id = ?1",
            params![snapshot_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        if tx.execute(
            "UPDATE snapshots SET status = 'current'
             WHERE id = ?1 AND status IN ('created', 'staged')",
            params![snapshot_id],
        )? > 0
        {
            promoted.push((snapshot_id, before.0, before.1, before.2));
        }
    }
    crate::volume::write::record_write_bookkeeping(&tx, volume_id, &layout, block_size)?;
    events::log_event(
        &tx,
        "volume",
        volume_id,
        Some(label),
        "write_completed",
        None,
        None,
        None,
        Some(&format!(
            "adopted by `volume resume`: a write this catalog lost (restored from a backup \
             taken before it), sealed on the tape at {}. File 0's uuid matched, the seal \
             marker binds front index {}, a full verify passed after the rebuild, and all \
             {slices} slice hash(es) equal the ones this catalog staged (ADR-0012, \
             2026-09-29 later)",
            tape.sealed_at, tape.front_index_sha256
        )),
        None,
    )?;
    tx.commit()?;

    for (snapshot_id, old_status, unit_name, tenant_id) in promoted {
        if let Err(e) = events::log_field_change(
            conn,
            "snapshot",
            snapshot_id,
            &unit_name,
            "sealed_current",
            "status",
            Some(&old_status),
            "current",
            Some(tenant_id),
        ) {
            tracing::warn!(
                snapshot_id,
                unit = %unit_name,
                error = %e,
                "adopted, but the snapshot-promotion audit row could not be written"
            );
        }
    }
    Ok(())
}

/// The error `volume resume` exits with for a refusal: the first unmet
/// condition, and the command that resolves it where one exists.
pub fn refusal(label: &str, verdict: &LostAdoption) -> TapectlError {
    TapectlError::Other(match verdict {
        LostAdoption::Adopted { slices } => {
            format!("volume \"{label}\" was adopted ({slices} slice(s)); this is not a refusal")
        }
        LostAdoption::IdentityMismatch {
            expected_uuid,
            found,
        } => format!(
            "volume \"{label}\": the loaded tape is not this volume — the catalog records uuid \
             {expected_uuid:?}, and File 0 reads {found}. `volume resume` adopts a write this \
             catalog lost only from the volume's own tape (ADR-0012, 2026-09-29 later, \
             condition 1). Load the cartridge \"{label}\" was initialised on and resume again."
        ),
        LostAdoption::NotSealed { why } => format!(
            "volume \"{label}\": no valid seal marker binds this tape's front index ({why}). \
             `volume resume` adopts a write this catalog lost only from a sealed tape \
             (ADR-0012, 2026-09-29 later, condition 1), so nothing makes this one count as a \
             copy: write its units again to another cartridge, or erase this one and reuse it."
        ),
        LostAdoption::NoRecordedRebuild => format!(
            "volume \"{label}\" is `initialized` with completed writes attached, but no `catalog \
             rebuild --from-volume` of it is recorded, so nothing says those writes came from \
             its tape. If this cartridge was written and sealed by a session this catalog lost \
             (it was restored from a backup taken before the write), load it and run `tapectl \
             catalog rebuild --from-volume`, then a full `tapectl volume verify {label}`, then \
             `tapectl volume resume {label}` again (ADR-0012, 2026-09-29 later)."
        ),
        LostAdoption::NoCleanFullVerifyAfterRebuild { rebuilt_at } => format!(
            "volume \"{label}\": no passing full verify is recorded after its rebuild at \
             {rebuilt_at}. `volume resume` adopts a write this catalog lost only once every \
             byte has read back good (ADR-0012, 2026-09-29 later, condition 2): run `tapectl \
             volume verify {label}` (full by default), then resume again."
        ),
        LostAdoption::NotWhatThisCatalogStaged { why } => format!(
            "volume \"{label}\": the tape does not hold what this catalog staged — {why}. \
             `volume resume` adopts a write this catalog lost only when every slice hash in \
             the tape's front index equals the hash this catalog recorded when it staged that \
             slice (ADR-0012, 2026-09-29 later, condition 3): the tape cannot vouch for \
             itself. Its data stays restorable from the tape, but it does not count as a copy; \
             write its units again to another cartridge."
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::crypto::keys::generate_keypair;
    use crate::db;
    use crate::store::{MemStore, Tier};
    use crate::tape::contact::{ContactSite, Medium, Operation};
    use crate::volume::build::{self, BuildInputs, BuildSlice, BuildUnit, TenantInfo};
    use crate::volume::layout_model::{KeyAvailability, SliceCheck};
    use crate::volume::session::{ConfirmOutcome, ExecuteOutcome};
    use tempfile::TempDir;

    const BS: u64 = 65536;
    const LABEL: &str = "LOST01";
    const VOL_UUID: &str = "36000000-0000-4000-8000-000000000360";

    fn sha_hex(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    fn cfg() -> &'static Config {
        static CFG: std::sync::OnceLock<Config> = std::sync::OnceLock::new();
        CFG.get_or_init(Config::default)
    }

    /// A volume written and sealed by the real session, then lost: `conn`
    /// is a copy of the catalog taken after `volume init` and before the
    /// write planned anything (the "backup between init and write"),
    /// reopened as the restored catalog. `store` is the sealed tape.
    struct Lost {
        conn: rusqlite::Connection,
        store: MemStore,
        volume_id: i64,
        operator_secret: String,
        _dirs: Vec<TempDir>,
    }

    fn written_then_lost() -> Lost {
        let db_dir = tempfile::tempdir().unwrap();
        let src = db::open(&db_dir.path().join("src.db")).unwrap();

        let operator = generate_keypair();
        src.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('operator', 1, 'active')",
            [],
        )
        .unwrap();
        let alpha = generate_keypair();
        src.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('alpha', 0, 'active')",
            [],
        )
        .unwrap();
        let tenant_id = src.last_insert_rowid();
        src.execute(
            "INSERT INTO units (uuid, name, tenant_id, current_path, status)
             VALUES ('unit-uuid-360', 'unit-lost', ?1, '/src/lost', 'active')",
            params![tenant_id],
        )
        .unwrap();
        let unit_id = src.last_insert_rowid();
        src.execute(
            "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
             VALUES (?1, 1, 'staged', '/src/lost', 1, 64)",
            params![unit_id],
        )
        .unwrap();
        let snapshot_id = src.last_insert_rowid();
        src.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 1048576)",
            params![snapshot_id],
        )
        .unwrap();
        let stage_set_id = src.last_insert_rowid();
        // What `volume init` leaves: `initialized`, File 0's uuid recorded.
        src.execute(
            "INSERT INTO volumes (label, uuid, backend_type, backend_name, media_type,
                                  capacity_bytes, status)
             VALUES (?1, ?2, 'lto', 'lto0', 'LTO-6', 2500000000000, 'initialized')",
            params![LABEL, VOL_UUID],
        )
        .unwrap();
        let volume_id = src.last_insert_rowid();

        let slices_dir = tempfile::tempdir().unwrap();
        let mut slices = Vec::new();
        for n in 1..=2i64 {
            let content = format!("staged ciphertext of slice {n}, padded a little").into_bytes();
            let sha = sha_hex(&content);
            src.execute(
                "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes, encrypted_bytes,
                                           sha256_plain, sha256_encrypted)
                 VALUES (?1, ?2, ?3, ?3, ?4, ?5)",
                params![
                    stage_set_id,
                    n,
                    content.len() as i64,
                    sha_hex(b"plain"),
                    sha
                ],
            )
            .unwrap();
            let slice_id = src.last_insert_rowid();
            let path = slices_dir.path().join(format!("slice_{slice_id}.age"));
            std::fs::write(&path, &content).unwrap();
            src.execute(
                "UPDATE stage_slices SET staging_path = ?1 WHERE id = ?2",
                params![path.to_string_lossy(), slice_id],
            )
            .unwrap();
            slices.push(BuildSlice {
                slice_id,
                slice_number: n,
                size_bytes: content.len() as i64,
                encrypted_bytes: content.len() as i64,
                sha256_plain: sha_hex(b"plain"),
                sha256_encrypted: sha,
                staging_path: path,
            });
        }
        let unit = BuildUnit {
            stage_set_id,
            snapshot_id,
            unit_name: "unit-lost".to_string(),
            unit_uuid: "unit-uuid-360".to_string(),
            tenant_id,
            dar_version: Some("2.7.20".to_string()),
            dar_command: None,
            catalog_path: None,
            snapshot_version: 1,
            slices,
        };
        let inputs = BuildInputs {
            label: LABEL.to_string(),
            volume_uuid: VOL_UUID.to_string(),
            media_type: "LTO-6".to_string(),
            tapectl_version: "0.1.0-test".to_string(),
            created_at: "2026-09-29T00:00:00Z".to_string(),
            block_size: BS,
            usable_bytes: 64 * 1024 * 1024,
            enospc_buffer: 1024 * 1024,
            nominal_capacity: 2_500_000_000_000,
            mam_capacity: 0,
            mam_manufacturer: String::new(),
            mam_serial: String::new(),
            cartridge_identity_source: None,
            mam_length: 0,
            mam_loads: 0,
            units: vec![unit.clone()],
            tenants: vec![TenantInfo {
                tenant_id,
                tenant_name: "alpha".to_string(),
                public_keys: vec![alpha.public_key.clone()],
            }],
            operator_public_keys: vec![operator.public_key.clone()],
            escrow_public_key: None,
            catalog_db_path: None,
        };
        let session_dir = tempfile::tempdir().unwrap();
        let built = build::build(&inputs, session_dir.path()).unwrap();

        // The backup, taken between `volume init` and `volume write`.
        let backup = db_dir.path().join("backup.db");
        src.execute("VACUUM INTO ?1", params![backup.to_string_lossy()])
            .unwrap();

        let keys = KeyAvailability {
            tenant_ids: vec![tenant_id],
            tenants_with_active_key: [tenant_id].into_iter().collect(),
            operator_key_present: true,
            escrow_recipient_present: None,
            stage_sets_lacking_escrow: None,
        };
        let mut store = MemStore::new(BS as usize);
        let validated = built
            .into_validated(&keys, SliceCheck::Size, &mut store)
            .unwrap();
        let planned = validated.plan(&src, volume_id, &[unit]).unwrap();
        let ExecuteOutcome::Ready(ready) = planned.execute(&src, &mut store).unwrap() else {
            panic!("the write must reach Ready");
        };
        let sealed = ready.seal(&mut store).unwrap();
        let ConfirmOutcome::Sealed(_) = sealed.confirm(&src, &mut store, Tier::Integrity).unwrap()
        else {
            panic!("the write must seal and confirm");
        };
        drop(src);

        let conn = db::open(&backup).unwrap();
        Lost {
            conn,
            store,
            volume_id,
            operator_secret: operator.secret_key,
            _dirs: vec![db_dir, slices_dir, session_dir],
        }
    }

    impl Lost {
        /// `catalog rebuild --from-volume` with the operator key, its event
        /// then dated a minute back so a verify taken now is "after" it at
        /// `datetime('now')`'s one-second resolution.
        fn rebuild(&mut self) {
            let scratch = tempfile::tempdir().unwrap();
            let identity: age::x25519::Identity = self.operator_secret.parse().unwrap();
            crate::volume::rebuild::rebuild_from_store(
                &self.conn,
                &mut self.store,
                &[identity],
                Some(LABEL),
                "recovered",
                Some("lto0"),
                scratch.path(),
                "memstore",
                ContactSite::new(
                    cfg(),
                    Operation::CatalogRebuild,
                    "memstore",
                    Medium::NoBackend,
                ),
            )
            .unwrap();
            self.conn
                .execute(
                    "UPDATE events SET timestamp = datetime(timestamp, '-1 minute')
                     WHERE action = 'catalog_rebuild'",
                    [],
                )
                .unwrap();
        }

        /// `volume verify` at `tier`, through its store seam.
        fn verify(&mut self, tier: Tier) {
            crate::volume::write::volume_verify_with_store(
                &self.conn,
                &mut self.store,
                LABEL,
                self.volume_id,
                BS as usize,
                tier,
                ContactSite::new(
                    cfg(),
                    Operation::VolumeVerify,
                    "memstore",
                    Medium::NoBackend,
                ),
            )
            .unwrap();
        }

        fn adopt(&mut self) -> LostAdoption {
            adopt(&self.conn, &mut self.store, self.volume_id, LABEL, BS).unwrap()
        }

        fn status(&self) -> (String, Option<String>) {
            self.conn
                .query_row(
                    "SELECT status, sealed_at FROM volumes WHERE id = ?1",
                    params![self.volume_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap()
        }

        /// The rebuilt, verified state every refusal arm below starts from,
        /// one fact removed.
        fn ready() -> Lost {
            let mut lost = written_then_lost();
            lost.rebuild();
            lost.verify(Tier::Integrity);
            lost
        }

        /// The front index's raw bytes on this tape.
        fn front_index_mut(&mut self) -> &mut Vec<u8> {
            &mut self.store.files[3]
        }
    }

    fn assert_refused_untouched(lost: &Lost, verdict: &LostAdoption) {
        assert!(
            !matches!(verdict, LostAdoption::Adopted { .. }),
            "must refuse, got {verdict:?}"
        );
        assert_eq!(
            lost.status(),
            ("initialized".to_string(), None),
            "a refusal writes nothing"
        );
    }

    /// The adopt path: rebuilt, verified after the rebuild, and the tape's
    /// slices are the catalog's own staged ones — the volume is sealed, with
    /// the facts a passing confirm would have written.
    #[test]
    fn a_lost_volume_rebuilt_and_verified_is_adopted_as_sealed() {
        let mut lost = written_then_lost();
        assert!(
            !is_lost_write(&lost.conn, lost.volume_id).unwrap(),
            "no writes rows yet"
        );
        lost.rebuild();
        assert_eq!(
            lost.status(),
            ("initialized".to_string(), None),
            "catalog rebuild never changes a status it finds (2026-09-16)"
        );
        assert!(is_lost_write(&lost.conn, lost.volume_id).unwrap());
        lost.verify(Tier::Integrity);
        assert_eq!(recorded_evidence(&lost.conn, lost.volume_id).unwrap(), None);

        assert_eq!(lost.adopt(), LostAdoption::Adopted { slices: 2 });

        let (status, sealed_at) = lost.status();
        assert_eq!(status, "sealed");
        let seal =
            format::parse_seal_marker(&String::from_utf8_lossy(lost.store.files.last().unwrap()))
                .unwrap();
        let tape_sealed = chrono::DateTime::parse_from_rfc3339(&seal.sealed_at)
            .unwrap()
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        assert_eq!(
            sealed_at.as_deref(),
            Some(tape_sealed.as_str()),
            "the tape's own seal time"
        );
        let (snapshot, bytes, files, manifest): (String, i64, i64, i64) = lost
            .conn
            .query_row(
                "SELECT s.status, v.bytes_written, v.num_data_files, v.has_manifest
                 FROM snapshots s, volumes v WHERE v.id = ?1",
                params![lost.volume_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            snapshot, "current",
            "the snapshot is promoted as confirm promotes it"
        );
        assert_eq!((bytes, files, manifest), (2 * BS as i64, 2, 1));
        let completed: i64 = lost
            .conn
            .query_row(
                "SELECT COUNT(*) FROM events
                 WHERE entity_id = ?1 AND entity_type = 'volume' AND action = 'write_completed'",
                params![lost.volume_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(completed, 1);
        assert!(
            !is_lost_write(&lost.conn, lost.volume_id).unwrap(),
            "adopted once"
        );
    }

    #[test]
    fn condition_1_refuses_a_tape_whose_file_0_names_another_volume() {
        let mut lost = Lost::ready();
        lost.conn
            .execute(
                "UPDATE volumes SET uuid = 'another-uuid' WHERE id = ?1",
                params![lost.volume_id],
            )
            .unwrap();
        let verdict = lost.adopt();
        assert!(
            matches!(&verdict, LostAdoption::IdentityMismatch { expected_uuid, found }
                if expected_uuid == "another-uuid" && found.contains(VOL_UUID)),
            "{verdict:?}"
        );
        assert_refused_untouched(&lost, &verdict);
    }

    #[test]
    fn condition_1_refuses_an_unreadable_file_0() {
        let mut lost = Lost::ready();
        lost.store.files[0] = vec![0xff; BS as usize];
        let verdict = lost.adopt();
        assert!(
            matches!(verdict, LostAdoption::IdentityMismatch { .. }),
            "{verdict:?}"
        );
        assert_refused_untouched(&lost, &verdict);
    }

    #[test]
    fn condition_1_refuses_a_tape_with_no_seal_marker() {
        let mut lost = Lost::ready();
        lost.store.files.pop();
        let verdict = lost.adopt();
        assert!(
            matches!(verdict, LostAdoption::NotSealed { .. }),
            "{verdict:?}"
        );
        assert_refused_untouched(&lost, &verdict);
    }

    #[test]
    fn condition_1_refuses_a_seal_that_does_not_bind_this_front_index() {
        let mut lost = Lost::ready();
        // One byte of File 3's human header: still parses, no longer the
        // bytes the seal marker's `front_index_sha256` names.
        let fi = lost.front_index_mut();
        let at = fi.iter().position(|b| *b == b'=').unwrap();
        fi[at] = b'#';
        let verdict = lost.adopt();
        assert!(
            matches!(&verdict, LostAdoption::NotSealed { why } if why.contains("front index")),
            "{verdict:?}"
        );
        assert_refused_untouched(&lost, &verdict);
    }

    #[test]
    fn condition_2_refuses_with_no_recorded_rebuild() {
        let mut lost = Lost::ready();
        lost.conn
            .execute("DELETE FROM events WHERE action = 'catalog_rebuild'", [])
            .unwrap();
        let verdict = lost.adopt();
        assert_eq!(verdict, LostAdoption::NoRecordedRebuild);
        assert_refused_untouched(&lost, &verdict);
    }

    #[test]
    fn condition_2_refuses_with_no_verify_after_the_rebuild() {
        let mut lost = written_then_lost();
        lost.rebuild();
        let verdict = lost.adopt();
        assert!(
            matches!(verdict, LostAdoption::NoCleanFullVerifyAfterRebuild { .. }),
            "{verdict:?}"
        );
        assert_eq!(
            recorded_evidence(&lost.conn, lost.volume_id).unwrap(),
            Some(verdict.clone())
        );
        assert_refused_untouched(&lost, &verdict);
    }

    #[test]
    fn condition_2_refuses_a_quick_verify() {
        let mut lost = written_then_lost();
        lost.rebuild();
        lost.verify(Tier::Navigable);
        let verdict = lost.adopt();
        assert!(
            matches!(verdict, LostAdoption::NoCleanFullVerifyAfterRebuild { .. }),
            "{verdict:?}"
        );
        assert_refused_untouched(&lost, &verdict);
    }

    #[test]
    fn condition_2_refuses_a_full_verify_that_started_before_the_rebuild() {
        let mut lost = written_then_lost();
        lost.verify(Tier::Integrity);
        lost.conn
            .execute(
                "UPDATE verification_sessions SET started_at = datetime('now', '-1 hour')",
                [],
            )
            .unwrap();
        lost.rebuild();
        let verdict = lost.adopt();
        assert!(
            matches!(verdict, LostAdoption::NoCleanFullVerifyAfterRebuild { .. }),
            "{verdict:?}"
        );
        assert_refused_untouched(&lost, &verdict);
    }

    #[test]
    fn condition_2_refuses_a_failed_full_verify() {
        let mut lost = written_then_lost();
        lost.rebuild();
        lost.conn
            .execute(
                "INSERT INTO verification_sessions (volume_id, verify_type, outcome)
                 VALUES (?1, 'full', 'failed')",
                params![lost.volume_id],
            )
            .unwrap();
        let verdict = lost.adopt();
        assert!(
            matches!(verdict, LostAdoption::NoCleanFullVerifyAfterRebuild { .. }),
            "{verdict:?}"
        );
        assert_refused_untouched(&lost, &verdict);
    }

    #[test]
    fn condition_3_refuses_a_slice_whose_hash_is_not_the_one_staged() {
        let mut lost = Lost::ready();
        lost.conn
            .execute(
                "UPDATE stage_slices SET sha256_encrypted = ?1 WHERE slice_number = 2",
                params![sha_hex(b"what the catalog staged instead")],
            )
            .unwrap();
        let verdict = lost.adopt();
        assert!(
            matches!(verdict, LostAdoption::NotWhatThisCatalogStaged { .. }),
            "{verdict:?}"
        );
        assert_refused_untouched(&lost, &verdict);
    }

    /// The tape vouching for itself: a stage set the rebuild created from
    /// the tape's own manifest carries the tape's own hashes, and is never
    /// "what this catalog staged".
    #[test]
    fn condition_3_refuses_a_stage_set_the_rebuild_made_from_the_tape() {
        let mut lost = Lost::ready();
        lost.conn
            .execute("UPDATE stage_sets SET origin = 'rebuilt'", [])
            .unwrap();
        let verdict = lost.adopt();
        assert!(
            matches!(&verdict, LostAdoption::NotWhatThisCatalogStaged { why }
                if why.contains("rebuilt")),
            "{verdict:?}"
        );
        assert_refused_untouched(&lost, &verdict);
    }

    #[test]
    fn condition_3_refuses_a_tape_slice_the_catalog_has_no_position_for() {
        let mut lost = Lost::ready();
        lost.conn
            .execute(
                "DELETE FROM write_positions WHERE id = (SELECT MAX(id) FROM write_positions)",
                [],
            )
            .unwrap();
        let verdict = lost.adopt();
        assert!(
            matches!(verdict, LostAdoption::NotWhatThisCatalogStaged { .. }),
            "{verdict:?}"
        );
        assert_refused_untouched(&lost, &verdict);
    }

    /// A slice of this volume the catalog records at a file the front index
    /// does not list as a slice (here File 1, the system guide): the tape
    /// does not hold everything the catalog says it does.
    #[test]
    fn condition_3_refuses_a_catalog_position_the_front_index_has_no_slice_at() {
        let mut lost = Lost::ready();
        lost.conn
            .execute_batch(
                "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes, encrypted_bytes,
                                           sha256_plain, sha256_encrypted)
                 SELECT stage_set_id, 99, 1, 1, 'p', 'e' FROM stage_slices LIMIT 1;
                 INSERT INTO write_positions (write_id, stage_slice_id, position, status)
                 SELECT (SELECT MIN(id) FROM writes), MAX(id), '1', 'written' FROM stage_slices;",
            )
            .unwrap();
        let verdict = lost.adopt();
        assert!(
            matches!(&verdict, LostAdoption::NotWhatThisCatalogStaged { why }
                if why.contains("at file 1") && why.contains("does not list")),
            "{verdict:?}"
        );
        assert_refused_untouched(&lost, &verdict);
    }

    /// A stage set only partly on this tape: every slice on the tape is one
    /// the catalog staged, but the catalog staged one more, so the write
    /// this tape would complete is not whole.
    #[test]
    fn condition_3_refuses_a_stage_set_not_whole_on_the_tape() {
        let mut lost = Lost::ready();
        lost.conn
            .execute(
                "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes, encrypted_bytes,
                                           sha256_plain, sha256_encrypted)
                 SELECT stage_set_id, 99, 1, 1, 'p', 'e' FROM stage_slices LIMIT 1",
                [],
            )
            .unwrap();
        let verdict = lost.adopt();
        assert!(
            matches!(&verdict, LostAdoption::NotWhatThisCatalogStaged { why }
                if why.contains("staged 3 slice(s), but 2 of them")),
            "{verdict:?}"
        );
        assert_refused_untouched(&lost, &verdict);
    }

    /// The ruling's order: with condition 1 AND condition 2 unmet, the
    /// refusal names condition 1.
    #[test]
    fn the_first_unmet_condition_is_the_one_named() {
        let mut lost = written_then_lost();
        lost.rebuild();
        lost.store.files.pop();
        assert!(matches!(lost.adopt(), LostAdoption::NotSealed { .. }));
    }

    /// A recorded seal is #280's territory (a session of this catalog's
    /// own), never this adoption's.
    #[test]
    fn a_volume_whose_seal_is_recorded_is_not_a_lost_write() {
        let lost = Lost::ready();
        lost.conn
            .execute(
                "UPDATE volumes SET sealed_at = datetime('now') WHERE id = ?1",
                params![lost.volume_id],
            )
            .unwrap();
        assert!(!is_lost_write(&lost.conn, lost.volume_id).unwrap());
        let mut lost = lost;
        assert!(adopt(&lost.conn, &mut lost.store, lost.volume_id, LABEL, BS).is_err());
    }

    /// Each refusal names what resolves it, where something does.
    #[test]
    fn each_refusal_names_its_remedy() {
        let rebuild = refusal(LABEL, &LostAdoption::NoRecordedRebuild).to_string();
        assert!(
            rebuild.contains("tapectl catalog rebuild --from-volume"),
            "{rebuild}"
        );
        let verify = refusal(
            LABEL,
            &LostAdoption::NoCleanFullVerifyAfterRebuild {
                rebuilt_at: "2026-10-07 00:00:00".to_string(),
            },
        )
        .to_string();
        assert!(
            verify.contains(&format!("tapectl volume verify {LABEL}")),
            "{verify}"
        );
        assert!(verify.contains("2026-10-07 00:00:00"), "{verify}");
        for v in [
            LostAdoption::IdentityMismatch {
                expected_uuid: "u".into(),
                found: "v".into(),
            },
            LostAdoption::NotSealed { why: "w".into() },
            LostAdoption::NotWhatThisCatalogStaged { why: "w".into() },
        ] {
            let msg = refusal(LABEL, &v).to_string();
            assert!(msg.contains(LABEL) && msg.contains("ADR-0012"), "{msg}");
        }
    }
}
