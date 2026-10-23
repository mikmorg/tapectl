//! `volume resume` adopts a volume the catalog lost mid-write (issue #360,
//! ADR-0012's amendment "2026-09-29 (later)").
//!
//! **The state.** A catalog restored from a backup taken after `volume init`
//! but before `volume write` began holds the volume as `initialized` with no
//! `writes` rows, while the tape itself was sealed and confirmed. `catalog
//! rebuild --from-volume` attaches its units — `completed` writes rows, never
//! verified, for a tape whose seal marker it read (#458: a tape without one
//! gets `aborted` rows, which this never adopts) — but never changes a status
//! it finds (the 2026-09-16 ruling, which stands). Before this module nothing could make such a volume count:
//! `volume resume` had no session to adopt, and refused the row as
//! `VolumeHasRecordedWrite`.
//!
//! **The ruling.** Resume adopts such a volume on all of the following,
//! conjunctively, and refuses naming the first unmet one and the command that
//! resolves it:
//!
//! 1. **the same volume, actually written:** the tape's File 0 uuid equals
//!    the catalog row's uuid, File 0 does not say this tapectl cannot read the
//!    tape (a `requires` it does not know, another magic or layout; #476), and
//!    a valid seal marker binds its front index;
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
//! **A tape that ended early** (#420; layout-session.md's condition 3): the
//! seal is located by `volume-format-v2.md` §1.2 rule 6, the hashes are
//! compared only at or before its `ended_after`, and a stage set with a slice
//! after it is not whole but cut: the rebuild recorded it `cut` (and the
//! ending on this row), adoption checks the rows say what the tape says, and
//! records `ended_after` with the seal and promotes only the whole versions.
//!
//! **A range** (#478, ADR-0012 items 121, 128 and 130): a write that may span
//! splits three ways, as confirm splits it and the rebuild recorded it
//! (`volume-format-v2.md` §4.4): `completed` when the tape holds every slice
//! of the set, `range` when it holds some, `cut` when it holds none. A
//! `range` row is adopted when its positions are one unbroken run of the
//! set's slices at consecutive tape positions, the run the rows plan; its
//! version is promoted only once every slice of the set is on a sealed
//! volume, this one counted (item 128), and the `write_completed` event
//! names the range units with their runs, as confirm's does.
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
    /// Condition 1, first half: File 0 reads and is this volume's, but says
    /// this tapectl cannot read the tape — it requires a feature this tapectl
    /// does not know, or states another magic or layout (`why` is the text
    /// `volume::raw` and `catalog rebuild` give; #476). Adopting would record
    /// as a whole sealed copy a tape this tapectl cannot show is whole.
    UnreadableFile0 { why: String },
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
/// `completed`, `range` or `cut` — what a rebuild attaches to a row a
/// pre-write backup left behind (`cut` for a unit a tape that ended early
/// does not hold whole, #420; `range` for a spannable set the tape holds
/// some of, #478, ADR-0012 item 130). A recorded seal is #280's territory
/// (an interrupted or aborted session of this catalog's own), and any
/// unresolved row is a session `volume resume` continues the ordinary way,
/// so neither is adopted here.
pub fn is_lost_write(conn: &Connection, volume_id: i64) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT v.status = 'initialized' AND v.sealed_at IS NULL
                AND v.observed_condition = 'ok'
                AND EXISTS(SELECT 1 FROM writes WHERE volume_id = v.id)
                AND NOT EXISTS(SELECT 1 FROM writes
                               WHERE volume_id = v.id
                                 AND status NOT IN ('completed', 'range', 'cut'))
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
             with no recorded seal, an `ok` condition and nothing but completed (or range, or \
             cut) writes attached is adopted this way (ADR-0012, 2026-09-29 later)"
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
    // The same File 0, asked what it says about the reader it needs (#476,
    // ADR-0012 2026-10-06 item 15): before the front index or the seal is
    // read, so a tape this tapectl cannot show is whole is never adopted as
    // a sealed copy. A File 0 that does not read or parse was refused above,
    // as it always was; this is only for one that does and says no.
    if let Err(e) = format::check_id_thunk_readable(&file0) {
        return Ok(LostAdoption::UnreadableFile0 { why: e.to_string() });
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
    let slices = match staged_match(conn, volume_id, &tape.entries, tape.ended_after)? {
        Ok(slices) => slices,
        Err(why) => return Ok(LostAdoption::NotWhatThisCatalogStaged { why }),
    };

    record_adoption(conn, volume_id, label, block_size, &tape, slices)?;
    Ok(LostAdoption::Adopted { slices })
}

/// What condition 1 read off the tape, once a seal marker bound it.
struct SealedIndex {
    /// The front index's entries: the plan, every planned file.
    entries: Vec<format::ParsedIndexEntry>,
    /// The seal marker's own `sealed_at` (RFC 3339): when the seal was
    /// written, which is the time `volumes.sealed_at` records.
    sealed_at: String,
    front_index_sha256: String,
    /// `Some(K)` when the tape sealed short (#420): only files 0 to K are on
    /// it (`volume-format-v2.md` §4.2).
    ended_after: Option<u32>,
}

/// Condition 1's second half: File 0's own layout pointers name the front
/// index and the planned seal; the seal is located by `volume-format-v2.md`
/// §1.2 rule 6 (at that pointer, or as the last file on a tape that ended
/// early, #420), parses, is one this tapectl may read and rule 6 accepts
/// where it lies, and its `front_index_sha256` is the hash of File 3's true
/// bytes (trailing NUL padding stripped, §4). `Err` names what failed, for
/// the refusal.
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
    let fi_true = format::without_nul_padding(&fi_raw);
    let entries = format::parse_front_index(&String::from_utf8_lossy(fi_true))
        .map_err(|e| format!("the front index (file {fi_pos}) does not parse: {e}"))?;

    // The judgement is `rebuild::locate_and_judge_seal`'s, shared with
    // `catalog rebuild` (#458, #420): a seal this tapectl may not read, and
    // a seal that binds nothing here, are both a seal that does not seal
    // THIS tape, for this adoption's purposes.
    let located =
        crate::volume::rebuild::locate_and_judge_seal(store, file0, fi_true, fi_pos, seal_pos);
    match located.verdict {
        format::SealVerdict::Sealed(seal) => Ok(SealedIndex {
            entries,
            sealed_at: seal.sealed_at,
            front_index_sha256: seal.front_index_sha256,
            ended_after: located.ended_after,
        }),
        format::SealVerdict::Unsealed(why) | format::SealVerdict::Refused(why) => Err(why),
    }
}

/// Condition 3: every slice the tape's front index lists is, at that
/// position, a slice of a stage set THIS catalog staged (`origin =
/// 'staged'`) with the very hash the catalog recorded; every slice position
/// the catalog attached to this volume is one the front index lists; and
/// every stage set written here is on the tape whole. `Ok(Ok(n))` is the
/// number of slices matched; `Ok(Err(why))` names the first disagreement.
///
/// On a tape that ended early (#420, `ended_after = Some(K)`;
/// layout-session.md's condition 3): the front index is the plan, so only
/// its slices at or before K are on the tape and compared, and a stage set
/// with a slice after K is not whole but cut. The catalog's rows must say
/// the same — `completed` for a set wholly at or before K, `cut` for every
/// other, as the rebuild recorded them and a passing short confirm would —
/// and its recorded ending must be the seal's.
///
/// A row that may span (#478) is matched by [`span_matches`] instead: a
/// `range` holds fewer slices than its set has, by definition.
fn staged_match(
    conn: &Connection,
    volume_id: i64,
    entries: &[format::ParsedIndexEntry],
    ended_after: Option<u32>,
) -> Result<std::result::Result<usize, String>> {
    let on_tape = |position: i64| ended_after.is_none_or(|k| position <= i64::from(k));
    let planned: Vec<(i64, Option<&str>)> = entries
        .iter()
        .filter(|e| {
            matches!(
                ZoneKind::from_type_label(&e.type_label),
                Some(ZoneKind::Slice { .. })
            )
        })
        .map(|e| (i64::from(e.position), e.sha256_encrypted.as_deref()))
        .collect();

    let recorded_ending: Option<i64> = conn.query_row(
        "SELECT ended_after FROM volumes WHERE id = ?1",
        params![volume_id],
        |r| r.get(0),
    )?;
    let sealed_ending = ended_after.map(i64::from);
    if recorded_ending != sealed_ending {
        let say = |k: Option<i64>| {
            k.map_or_else(
                || "it sealed full".to_string(),
                |k| format!("it ended after file {k}"),
            )
        };
        return Ok(Err(format!(
            "the catalog records that {}, but the tape's seal says {}",
            say(recorded_ending),
            say(sealed_ending)
        )));
    }

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

    let mut matched = 0usize;
    for (position, tape_sha) in planned.iter().filter(|(p, _)| on_tape(*p)) {
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
        matched += 1;
    }
    for position in catalog.keys() {
        if !planned.iter().any(|(p, _)| p == position) {
            return Ok(Err(format!(
                "the catalog records a slice of this volume at file {position}, which the \
                 tape's front index does not list as a slice"
            )));
        }
    }
    #[allow(clippy::type_complexity)]
    let whole: Vec<(i64, i64, String, bool, i64, i64, Option<i64>)> = conn
        .prepare(
            "SELECT w.id, w.stage_set_id, w.status, w.may_span,
                    (SELECT COUNT(*) FROM stage_slices WHERE stage_set_id = w.stage_set_id),
                    (SELECT COUNT(*) FROM write_positions WHERE write_id = w.id),
                    (SELECT MAX(CAST(position AS INTEGER)) FROM write_positions
                      WHERE write_id = w.id)
             FROM writes w WHERE w.volume_id = ?1 ORDER BY w.id",
        )?
        .query_map(params![volume_id], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    for (write_id, stage_set_id, status, may_span, staged, here, last) in whole {
        if may_span {
            // #478 (ADR-0012 items 121 and 130, `volume-format-v2.md` §4.4):
            // the three-way split, as confirm's `span_split` makes it.
            if let Err(why) = span_matches(conn, write_id, stage_set_id, &status, staged, &on_tape)?
            {
                return Ok(Err(why));
            }
            continue;
        }
        // #420: a set with a slice after the end is cut, and is not on this
        // tape whole by definition.
        let cut = last.is_some_and(|last| !on_tape(last));
        match (cut, status.as_str()) {
            (true, "cut") => continue,
            (false, "completed") => {}
            (true, _) => {
                return Ok(Err(format!(
                    "stage set {stage_set_id} is recorded `{status}`, but its slice at file {} \
                     lies after the end of the tape (it ended after file {}): it is cut",
                    last.unwrap_or_default(),
                    sealed_ending.unwrap_or_default()
                )))
            }
            (false, _) => {
                return Ok(Err(format!(
                    "stage set {stage_set_id} is recorded `{status}`, but every slice of it is \
                     on the tape"
                )))
            }
        }
        if staged != here {
            return Ok(Err(format!(
                "stage set {stage_set_id} staged {staged} slice(s), but {here} of them are on \
                 this tape"
            )));
        }
    }
    Ok(Ok(matched))
}

/// Condition 3 for a write that may span (#478; ADR-0012 items 121 and 130):
/// the row says what the tape holds of its stage set. The slices its rows
/// plan (its `write_positions`) must be one unbroken run in slice order at
/// consecutive tape positions (`volume-format-v2.md` §4.4: "a volume holds at
/// most one unbroken range of a stage set"), and the row's status the split
/// confirm makes (`span_split` in `volume::session`, mirrored here, as the
/// rebuild's `tape_split` mirrors it from the manifest): of the `staged`
/// slices of the set, those planned at or before the end (`on_tape`) — all
/// of them `completed` (item 93's middle tape), some `range`, none `cut`.
/// The front index and the hashes were matched position by position before
/// this. `Ok(Err(why))` names the disagreement.
fn span_matches(
    conn: &Connection,
    write_id: i64,
    stage_set_id: i64,
    status: &str,
    staged: i64,
    on_tape: &dyn Fn(i64) -> bool,
) -> Result<std::result::Result<(), String>> {
    let planned: Vec<(i64, i64)> = conn
        .prepare(
            "SELECT sl.slice_number, CAST(wp.position AS INTEGER)
               FROM write_positions wp
               JOIN stage_slices sl ON sl.id = wp.stage_slice_id
              WHERE wp.write_id = ?1
              ORDER BY sl.slice_number",
        )?
        .query_map(params![write_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    if let Some(pair) = planned
        .windows(2)
        .find(|w| w[1].0 != w[0].0 + 1 || w[1].1 != w[0].1 + 1)
    {
        return Ok(Err(format!(
            "stage set {stage_set_id} has slice {} at file {} and slice {} at file {} on this \
             volume: a volume holds one unbroken run of a stage set, in slice order at \
             consecutive positions",
            pair[0].0, pair[0].1, pair[1].0, pair[1].1
        )));
    }
    let held: Vec<i64> = planned
        .iter()
        .filter(|(_, position)| on_tape(*position))
        .map(|(number, _)| *number)
        .collect();
    let expected = if held.len() as i64 == staged {
        "completed"
    } else if held.is_empty() {
        "cut"
    } else {
        "range"
    };
    if status != expected {
        let holds = match (held.first(), held.last()) {
            (Some(first), Some(last)) if first == last => {
                format!("slice {first} of its {staged}")
            }
            (Some(first), Some(last)) => format!("slices {first}-{last} of its {staged}"),
            _ => "none of its slices".to_string(),
        };
        return Ok(Err(format!(
            "stage set {stage_set_id} is recorded `{status}`, but the tape holds {holds}: it is \
             `{expected}`"
        )));
    }
    Ok(Ok(()))
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
            plan_bound_bytes: None,
            span_tail_from: None,
        },
        // #420: only the files at or before a short seal's `ended_after` are
        // on the tape, and they are what the write's figures count.
        entries: tape
            .entries
            .iter()
            .filter(|p| {
                tape.ended_after
                    .is_none_or(|k| i64::from(p.position) <= i64::from(k))
            })
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
    // #420: `ended_after` with the seal, as a passing short confirm records
    // it (the rebuild recorded the same; `staged_match` checked they agree).
    let sealed = tx.execute(
        "UPDATE volumes SET status = 'sealed', sealed_at = COALESCE(?2, datetime('now')),
                            ended_after = ?3
         WHERE id = ?1 AND status = 'initialized' AND sealed_at IS NULL",
        params![volume_id, sealed_at, tape.ended_after],
    )?;
    if sealed != 1 {
        return Err(TapectlError::Other(format!(
            "volume \"{label}\" changed while `volume resume` read its tape; nothing was \
             adopted. Run the same command again."
        )));
    }
    // Only a version this tape holds whole is promoted: a cut one's is not
    // written yet (#420), as a passing short confirm leaves it. A range
    // (#478) promotes its version as confirm's does (ADR-0012 item 128):
    // once every slice of its stage set is on a sealed volume, this one
    // counted (sealed above, in this transaction, and named all the same).
    let snapshots: Vec<i64> = tx
        .prepare(&format!(
            "SELECT DISTINCT w.snapshot_id FROM writes w
              WHERE w.volume_id = ?1
                AND (w.status = 'completed'
                     OR (w.status = 'range' AND {}))
              ORDER BY 1",
            crate::policy::coverage::every_slice_on_sealed_volume_sql("w.stage_set_id", Some("?1"))
        ))?
        .query_map(params![volume_id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    // The three-way split the rows record (`staged_match` checked they say
    // what the tape says), each in plan order, for the `write_completed`
    // event a confirm of this write would have recorded.
    let cut_units: Vec<String> = tx
        .prepare(
            "SELECT u.name FROM writes w
               JOIN snapshots s ON s.id = w.snapshot_id
               JOIN units u ON u.id = s.unit_id
              WHERE w.volume_id = ?1 AND w.status = 'cut' ORDER BY w.id",
        )?
        .query_map(params![volume_id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    // A range's run is its row's `written` positions (item 121): the
    // rebuild left the positions past the end `pending`.
    let range_units: Vec<crate::volume::session::RangeUnit> = tx
        .prepare(
            "SELECT u.name, MIN(sl.slice_number), MAX(sl.slice_number)
               FROM writes w
               JOIN snapshots s ON s.id = w.snapshot_id
               JOIN units u ON u.id = s.unit_id
               JOIN write_positions wp ON wp.write_id = w.id AND wp.status = 'written'
               JOIN stage_slices sl ON sl.id = wp.stage_slice_id
              WHERE w.volume_id = ?1 AND w.status = 'range'
              GROUP BY w.id ORDER BY w.id",
        )?
        .query_map(params![volume_id], |r| {
            Ok(crate::volume::session::RangeUnit {
                unit: r.get(0)?,
                first_slice: r.get(1)?,
                last_slice: r.get(2)?,
            })
        })?
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
    let adopted = format!(
        "adopted by `volume resume`: a write this catalog lost (restored from a backup taken \
         before it), sealed on the tape at {}. File 0's uuid matched, the seal marker binds \
         front index {}, a full verify passed after the rebuild, and all {slices} slice \
         hash(es) equal the ones this catalog staged (ADR-0012, 2026-09-29 later)",
        tape.sealed_at, tape.front_index_sha256
    );
    // #420: on a tape that sealed short, the details a short write's own
    // `write_completed` event carries, from the one function that makes them
    // (`write::write_completed_details`: the ending, `ended_after` and the
    // cut units by name), with the adoption's account beside them under
    // `adopted`; on a full seal the account alone, as before 1.6.0. #478:
    // with the range units and their runs, on either ending, as confirm's.
    let as_written =
        crate::volume::write::write_completed_details(&crate::volume::session::SealedSession {
            volume_id,
            label: label.to_string(),
            ending: match tape.ended_after {
                Some(ended_after) => crate::volume::session::Ending::Short { ended_after },
                None => crate::volume::session::Ending::Full,
            },
            cut_units,
            range_units,
        });
    let details = match as_written
        .as_deref()
        .and_then(|d| serde_json::from_str::<serde_json::Value>(d).ok())
    {
        Some(mut short) => {
            short["adopted"] = serde_json::Value::String(adopted);
            short.to_string()
        }
        None => adopted,
    };
    events::log_event(
        &tx,
        "volume",
        volume_id,
        Some(label),
        "write_completed",
        None,
        None,
        None,
        Some(&details),
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
        LostAdoption::UnreadableFile0 { why } => format!(
            "volume \"{label}\": {why}. `volume resume` adopts a write this catalog lost only \
             from a tape this tapectl can read (ADR-0012, 2026-09-29 later, condition 1), so \
             nothing was adopted and the volume's status is unchanged."
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
             catalog rebuild --from-volume --key <operator or escrow secret key file>`, then a \
             full `tapectl volume verify {label}`, then `tapectl volume resume {label}` again \
             (ADR-0012, 2026-09-29 later)."
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
    use crate::pass_count::Sha256;
    use crate::store::{MemStore, Tier};
    use crate::tape::contact::{ContactSite, Medium, Operation};
    use crate::volume::build::{self, BuildInputs, BuildSlice, BuildUnit, TenantInfo};
    use crate::volume::layout_model::{KeyAvailability, SliceCheck};
    use crate::volume::session::{ConfirmOutcome, ExecuteOutcome};
    use sha2::Digest;
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
            span: None,
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
            index: None,
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

    /// `Lost`'s sealed tape on a `FakeTape`, so the head motion is recorded;
    /// File 0's `requires` line replaced by `to` (`STATED` leaves the tape as
    /// written, and proves the fixture states 1.6.0's
    /// `requires = ["short-seal"]`, ADR-0012 item 111).
    fn on_fake_tape(
        lost: &Lost,
        to: &str,
    ) -> (crate::tape::fake::FakeTape, crate::store::TapeStore) {
        let fake = crate::tape::fake::FakeTape::with_files(lost.store.files.clone(), BS as usize);
        let old = fake.state().files[0].clone();
        fake.state().files[0] = format::rewrite_file0(&old, STATED, to);
        let store = crate::store::TapeStore::from_ops(fake.boxed(), 0).unwrap();
        (fake, store)
    }

    const STATED: &str = "\nrequires = [\"short-seal\"]\n";

    /// #476 (ADR-0012 2026-10-06 item 15): File 0 is this volume's and reads,
    /// but states a feature this tapectl does not know — a tape it cannot
    /// show is whole must not be adopted as a sealed copy. Refused with the
    /// text `restore raw-volume` and `catalog rebuild` give, naming the
    /// feature, from the File 0 already read: File 0 is the only file the
    /// tape was asked for, and nothing is written.
    #[test]
    fn condition_1_refuses_a_tape_that_requires_an_unknown_feature() {
        use crate::tape::fake::Op;
        let lost = Lost::ready();
        let (fake, mut tape) = on_fake_tape(&lost, "\nrequires = [\"unknown-feature\"]\n");

        let verdict = adopt(&lost.conn, &mut tape, lost.volume_id, LABEL, BS).unwrap();

        let expected =
            format::check_id_thunk_readable(&String::from_utf8_lossy(&fake.state().files[0]))
                .unwrap_err()
                .to_string();
        assert_eq!(
            verdict,
            LostAdoption::UnreadableFile0 {
                why: expected.clone()
            }
        );
        assert!(expected.contains("\"unknown-feature\""), "{expected}");
        assert_refused_untouched(&lost, &verdict);
        let message = refusal(LABEL, &verdict).to_string();
        assert!(message.contains(&expected), "{message}");
        assert!(message.contains(LABEL), "{message}");
        let ops = fake.ops();
        assert_eq!(
            ops.iter().filter(|o| **o == Op::ReadHead(0)).count(),
            1,
            "{ops:?}"
        );
        assert!(
            ops.iter()
                .all(|o| matches!(o, Op::Rewind | Op::ReadHead(0))),
            "the front index and the seal were not read: {ops:?}"
        );
    }

    /// A `requires` that is not a list cannot be shown to be readable.
    #[test]
    fn condition_1_refuses_a_requires_that_is_not_a_list() {
        let lost = Lost::ready();
        let (_fake, mut tape) = on_fake_tape(&lost, "\nrequires = \"parts\"\n");
        let verdict = adopt(&lost.conn, &mut tape, lost.volume_id, LABEL, BS).unwrap();
        assert!(
            matches!(&verdict, LostAdoption::UnreadableFile0 { why }
                if why.contains("TOML parse failed")),
            "{verdict:?}"
        );
        assert_refused_untouched(&lost, &verdict);
    }

    /// The positive control: the same tape on the same fake, requiring only
    /// what this tapectl knows (`short-seal`), is adopted as before — and the
    /// front index and seal ARE read.
    #[test]
    fn a_tape_that_requires_nothing_is_adopted_through_the_same_store() {
        use crate::tape::fake::Op;
        let lost = Lost::ready();
        let (fake, mut tape) = on_fake_tape(&lost, STATED);
        let verdict = adopt(&lost.conn, &mut tape, lost.volume_id, LABEL, BS).unwrap();
        assert_eq!(verdict, LostAdoption::Adopted { slices: 2 });
        assert!(
            fake.ops()
                .iter()
                .any(|o| matches!(o, Op::ReadHead(n) if *n > 0)),
            "the front index and the seal were read: {:?}",
            fake.ops()
        );
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
        // #468: `catalog rebuild` refuses to parse without `--key`.
        crate::cli::operations::remedy_commands::assert_names_parsing_commands(
            &rebuild,
            &[("<operator or escrow secret key file>", "op.key")],
            &[
                "tapectl catalog rebuild --from-volume --key op.key",
                &format!("tapectl volume verify {LABEL}"),
                &format!("tapectl volume resume {LABEL}"),
            ],
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
            LostAdoption::UnreadableFile0 { why: "w".into() },
            LostAdoption::NotSealed { why: "w".into() },
            LostAdoption::NotWhatThisCatalogStaged { why: "w".into() },
        ] {
            let msg = refusal(LABEL, &v).to_string();
            assert!(msg.contains(LABEL) && msg.contains("ADR-0012"), "{msg}");
        }
    }

    /// #478 (ADR-0012 items 121, 128 and 130): the lost write of a tape that
    /// holds a range of a spannable set. One unit of three slices that may
    /// span; the restored catalog is a backup taken between the lost tape's
    /// `volume init` and its write.
    mod spanning {
        use super::*;
        use std::path::PathBuf;

        /// Three blocks less a little: a slice the early warning can come in.
        const SLICE: usize = 3 * BS as usize - 100;
        const UNIT: &str = "unit-span";

        /// The writing catalog and the spannable unit staged in it.
        struct World {
            src: rusqlite::Connection,
            unit: BuildUnit,
            tenant_id: i64,
            tenant_public: String,
            operator: crate::crypto::keys::GeneratedKeypair,
            dirs: Vec<TempDir>,
        }

        fn world() -> World {
            let db_dir = tempfile::tempdir().unwrap();
            let src = db::open(&db_dir.path().join("src.db")).unwrap();
            let operator = generate_keypair();
            let alpha = generate_keypair();
            src.execute(
                "INSERT INTO tenants (name, is_operator, status) VALUES ('operator', 1, 'active')",
                [],
            )
            .unwrap();
            src.execute(
                "INSERT INTO tenants (name, is_operator, status) VALUES ('alpha', 0, 'active')",
                [],
            )
            .unwrap();
            let tenant_id = src.last_insert_rowid();
            src.execute(
                "INSERT INTO units (uuid, name, tenant_id, current_path, status)
                 VALUES ('unit-uuid-478', ?1, ?2, '/src/span', 'active')",
                params![UNIT, tenant_id],
            )
            .unwrap();
            let unit_id = src.last_insert_rowid();
            src.execute(
                "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
                 VALUES (?1, 1, 'staged', '/src/span', 1, 64)",
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
            let slices_dir = tempfile::tempdir().unwrap();
            let mut slices = Vec::new();
            for n in 1..=3i64 {
                // Every block distinct, so a block written twice would show.
                let content: Vec<u8> = (0..SLICE)
                    .map(|o| (o / BS as usize + 31 * n as usize) as u8)
                    .collect();
                let sha = sha_hex(&content);
                src.execute(
                    "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes,
                                               encrypted_bytes, sha256_plain, sha256_encrypted)
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
            let span = build::UnitSpan {
                slices: slices.iter().map(build::SpanSlice::from).collect(),
            };
            World {
                src,
                unit: BuildUnit {
                    stage_set_id,
                    snapshot_id,
                    unit_name: UNIT.to_string(),
                    unit_uuid: "unit-uuid-478".to_string(),
                    tenant_id,
                    dar_version: Some("2.7.20".to_string()),
                    dar_command: None,
                    catalog_path: None,
                    snapshot_version: 1,
                    slices,
                    span: Some(span),
                },
                tenant_id,
                tenant_public: alpha.public_key,
                operator,
                dirs: vec![db_dir, slices_dir],
            }
        }

        /// One tape of the span, written and confirmed by the real session:
        /// the unit's slices `planned` (indexes into its slices) on a new
        /// volume, the early warning one byte into the slice at index
        /// `warning` when given. Returns the tape, its volume id, and the
        /// backup taken after its `volume init` and before its write.
        fn write_tape(
            w: &mut World,
            label: &str,
            uuid: &str,
            planned: std::ops::Range<usize>,
            warning: Option<usize>,
        ) -> (MemStore, i64, PathBuf) {
            w.src
                .execute(
                    "INSERT INTO volumes (label, uuid, backend_type, backend_name, media_type,
                                          capacity_bytes, status)
                     VALUES (?1, ?2, 'lto', 'lto0', 'LTO-6', 2500000000000, 'initialized')",
                    params![label, uuid],
                )
                .unwrap();
            let volume_id = w.src.last_insert_rowid();
            let unit = BuildUnit {
                slices: w.unit.slices[planned].to_vec(),
                ..w.unit.clone()
            };
            let inputs = BuildInputs {
                label: label.to_string(),
                volume_uuid: uuid.to_string(),
                media_type: "LTO-6".to_string(),
                tapectl_version: "0.1.0-test".to_string(),
                created_at: "2026-10-21T00:00:00Z".to_string(),
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
                    tenant_id: w.tenant_id,
                    tenant_name: "alpha".to_string(),
                    public_keys: vec![w.tenant_public.clone()],
                }],
                operator_public_keys: vec![w.operator.public_key.clone()],
                escrow_public_key: None,
                catalog_db_path: None,
                index: None,
            };
            let session_dir = tempfile::tempdir().unwrap();
            let built = build::build(&inputs, session_dir.path()).unwrap();
            let mut store = match warning {
                Some(i) => {
                    let id = w.unit.slices[i].slice_id;
                    let at = built
                        .layout
                        .entries
                        .iter()
                        .find(|e| matches!(e.kind, ZoneKind::Slice { stage_slice_id } if stage_slice_id == id))
                        .unwrap()
                        .position;
                    let before: u64 = built
                        .layout
                        .entries
                        .iter()
                        .filter(|e| e.position < at)
                        .map(|e| e.on_tape_bytes(BS).unwrap())
                        .sum();
                    MemStore::new(BS as usize).with_early_warning_after(before + 1)
                }
                None => MemStore::new(BS as usize),
            };
            let backup = w.dirs[0].path().join(format!("backup-{label}.db"));
            w.src
                .execute("VACUUM INTO ?1", params![backup.to_string_lossy()])
                .unwrap();
            let keys = KeyAvailability {
                tenant_ids: vec![w.tenant_id],
                tenants_with_active_key: [w.tenant_id].into_iter().collect(),
                operator_key_present: true,
                escrow_recipient_present: None,
                stage_sets_lacking_escrow: None,
            };
            let validated = built
                .into_validated(&keys, SliceCheck::Size, &mut store)
                .unwrap();
            let planned = validated
                .plan(&w.src, volume_id, &[unit])
                .unwrap()
                .with_post_warning_bytes(Some(64 * 1024 * 1024));
            let ExecuteOutcome::Ready(ready) = planned.execute(&w.src, &mut store).unwrap() else {
                panic!("the write must reach Ready");
            };
            let sealed = ready.seal(&mut store).unwrap();
            // What `write::finish_session` records once `seal()` returns.
            w.src
                .execute(
                    "UPDATE volumes SET sealed_at = datetime('now'), ended_after = ?2
                      WHERE id = ?1",
                    params![volume_id, sealed.ending().recorded_ended_after()],
                )
                .unwrap();
            let ConfirmOutcome::Sealed(_) =
                sealed.confirm(&w.src, &mut store, Tier::Integrity).unwrap()
            else {
                panic!("the write must seal and confirm");
            };
            w.dirs.push(session_dir);
            (store, volume_id, backup)
        }

        /// The restored catalog `backup`, rebuilt from `store` (its event
        /// aged a minute) and read back in full: ready for the adoption.
        fn restored_rebuilt_verified(
            backup: &std::path::Path,
            store: &mut MemStore,
            label: &str,
            volume_id: i64,
            operator_secret: &str,
        ) -> (rusqlite::Connection, crate::volume::rebuild::RebuildReport) {
            let conn = db::open(backup).unwrap();
            let scratch = tempfile::tempdir().unwrap();
            let identity: age::x25519::Identity = operator_secret.parse().unwrap();
            let report = crate::volume::rebuild::rebuild_from_store(
                &conn,
                store,
                &[identity],
                Some(label),
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
            conn.execute(
                "UPDATE events SET timestamp = datetime(timestamp, '-1 minute')
                 WHERE action = 'catalog_rebuild'",
                [],
            )
            .unwrap();
            let r = crate::volume::write::volume_verify_with_store(
                &conn,
                store,
                label,
                volume_id,
                BS as usize,
                Tier::Integrity,
                ContactSite::new(
                    cfg(),
                    Operation::VolumeVerify,
                    "memstore",
                    Medium::NoBackend,
                ),
            )
            .unwrap();
            assert!(r.mismatches.is_empty(), "{:?}", r.mismatches);
            (conn, report)
        }

        /// `(writes.status, may_span, snapshots.status)` of the unit's row on
        /// `volume_id`.
        fn row(conn: &rusqlite::Connection, volume_id: i64) -> (String, bool, String) {
            conn.query_row(
                "SELECT w.status, w.may_span, s.status FROM writes w
                   JOIN snapshots s ON s.id = w.snapshot_id WHERE w.volume_id = ?1",
                params![volume_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap()
        }

        fn last_write_completed(conn: &rusqlite::Connection) -> serde_json::Value {
            let detail: String = conn
                .query_row(
                    "SELECT details FROM events WHERE action = 'write_completed'
                      ORDER BY id DESC LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            serde_json::from_str(&detail).unwrap()
        }

        /// Tape 1 of a span, lost: the early warning came in slice 2, so the
        /// tape holds the range 1..2 and slice 3 is planned past its end. The
        /// rebuild records the `range` row; the adoption accepts it, seals
        /// the volume with its ending, leaves the version `staged` (slice 3
        /// is on no sealed volume, item 128), and its `write_completed` event
        /// names the range with its run, as the confirm's would have.
        #[test]
        fn a_lost_range_write_is_adopted_and_its_event_names_the_range() {
            let mut w = world();
            let (mut store, volume_id, backup) = write_tape(
                &mut w,
                "SPAN01",
                "47800000-0000-4000-8000-000000000360",
                0..3,
                Some(1),
            );
            let (conn, report) = restored_rebuilt_verified(
                &backup,
                &mut store,
                "SPAN01",
                volume_id,
                &w.operator.secret_key,
            );
            assert_eq!(report.range_units.len(), 1, "{report:?}");
            assert!(report.ended_after_recorded);
            assert_eq!(
                row(&conn, volume_id),
                ("range".to_string(), true, "staged".to_string())
            );
            assert!(is_lost_write(&conn, volume_id).unwrap());

            let verdict = adopt(&conn, &mut store, volume_id, "SPAN01", BS).unwrap();
            assert_eq!(verdict, LostAdoption::Adopted { slices: 2 });
            let (status, ended_after): (String, Option<i64>) = conn
                .query_row(
                    "SELECT status, ended_after FROM volumes WHERE id = ?1",
                    params![volume_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(status, "sealed");
            assert_eq!(ended_after, report.ended_after.map(|k| k as i64));
            assert_eq!(
                row(&conn, volume_id),
                ("range".to_string(), true, "staged".to_string()),
                "slice 3 is on no sealed volume: the version is not promoted"
            );
            let detail = last_write_completed(&conn);
            assert_eq!(detail["ending"], "short");
            assert_eq!(detail["cut_units"], serde_json::json!([]));
            assert_eq!(
                detail["range_units"],
                serde_json::json!([{"unit": UNIT, "first_slice": 1, "last_slice": 2}])
            );
            assert!(
                detail["adopted"]
                    .as_str()
                    .unwrap()
                    .starts_with("adopted by `volume resume`"),
                "{detail}"
            );
        }

        /// The tail of a span, lost (item 128): tape 1 sealed the range 1..2
        /// in the catalog the backup holds, and the lost tape holds slice 3,
        /// sealed full. Its row is a `range` and, with it, every slice is on
        /// a sealed volume: the version is current once the lost write is
        /// adopted, and the event of the full seal names the range.
        #[test]
        fn a_lost_tail_of_a_span_is_adopted_and_its_version_is_current() {
            let mut w = world();
            let _first = write_tape(
                &mut w,
                "SPAN01",
                "47800000-0000-4000-8000-000000000361",
                0..3,
                Some(1),
            );
            let (mut store, volume_id, backup) = write_tape(
                &mut w,
                "SPAN02",
                "47800000-0000-4000-8000-000000000362",
                2..3,
                None,
            );
            let (conn, report) = restored_rebuilt_verified(
                &backup,
                &mut store,
                "SPAN02",
                volume_id,
                &w.operator.secret_key,
            );
            assert_eq!(report.ended_after, None, "the tail sealed full");
            assert_eq!(row(&conn, volume_id).0, "range");

            let verdict = adopt(&conn, &mut store, volume_id, "SPAN02", BS).unwrap();
            assert_eq!(verdict, LostAdoption::Adopted { slices: 1 });
            assert_eq!(
                row(&conn, volume_id),
                ("range".to_string(), true, "current".to_string())
            );
            let detail = last_write_completed(&conn);
            assert_eq!(detail["ending"], "full");
            assert_eq!(
                detail["range_units"],
                serde_json::json!([{"unit": UNIT, "first_slice": 3, "last_slice": 3}])
            );
        }

        /// The rows must say what the tape says (condition 3): a `range`
        /// recorded `completed` claims the whole set, and the tape holds two
        /// of its three slices, so it is refused and nothing is adopted.
        #[test]
        fn a_range_recorded_as_completed_is_refused() {
            let mut w = world();
            let (mut store, volume_id, backup) = write_tape(
                &mut w,
                "SPAN01",
                "47800000-0000-4000-8000-000000000363",
                0..3,
                Some(1),
            );
            let (conn, _) = restored_rebuilt_verified(
                &backup,
                &mut store,
                "SPAN01",
                volume_id,
                &w.operator.secret_key,
            );
            conn.execute(
                "UPDATE writes SET status = 'completed' WHERE volume_id = ?1",
                params![volume_id],
            )
            .unwrap();
            let LostAdoption::NotWhatThisCatalogStaged { why } =
                adopt(&conn, &mut store, volume_id, "SPAN01", BS).unwrap()
            else {
                panic!("a completed row over a range must be refused");
            };
            assert!(
                why.contains("slices 1-2 of its 3") && why.contains("`range`"),
                "{why}"
            );
            let status: String = conn
                .query_row(
                    "SELECT status FROM volumes WHERE id = ?1",
                    params![volume_id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(status, "initialized", "nothing adopted");
        }
    }

    /// #420: the lost write of a tape that sealed short. The restored
    /// catalog is the backup the shared fixture took between `volume init`
    /// and the write; the tape ended after `bravo`'s first slice.
    mod short_ending {
        use super::*;
        use crate::volume::restore::short_tape::{self, ShortTape, BS, LABEL};

        struct ShortLost {
            conn: rusqlite::Connection,
            t: ShortTape,
        }

        fn site(operation: Operation) -> ContactSite<'static> {
            ContactSite::new(cfg(), operation, "memstore", Medium::NoBackend)
        }

        /// Restored from the backup, rebuilt from the tape (its event aged
        /// a minute), and read back in full.
        fn rebuilt_and_verified() -> ShortLost {
            let mut t = short_tape::short_tape();
            let conn = db::open(&t.backup).unwrap();
            let scratch = tempfile::tempdir().unwrap();
            let report = crate::volume::rebuild::rebuild_from_store(
                &conn,
                &mut t.store,
                std::slice::from_ref(&t.operator),
                Some(LABEL),
                "recovered",
                Some("lto0"),
                scratch.path(),
                "memstore",
                site(Operation::CatalogRebuild),
            )
            .unwrap();
            assert_eq!(report.ended_after, Some(u64::from(t.ended_after)));
            assert!(
                report.ended_after_recorded,
                "the found row takes the tape's ending: {report:?}"
            );
            conn.execute(
                "UPDATE events SET timestamp = datetime(timestamp, '-1 minute')
                 WHERE action = 'catalog_rebuild'",
                [],
            )
            .unwrap();
            // `volume verify` reads the seal at ended_after + 1: the passing
            // full readback condition 2 asks for is reachable on a short tape
            // only because the rebuild recorded where it ended.
            let r = crate::volume::write::volume_verify_with_store(
                &conn,
                &mut t.store,
                LABEL,
                t.volume_id,
                BS as usize,
                Tier::Integrity,
                site(Operation::VolumeVerify),
            )
            .unwrap();
            assert!(r.mismatches.is_empty(), "{:?}", r.mismatches);
            ShortLost { conn, t }
        }

        #[test]
        fn a_lost_write_that_sealed_short_is_adopted_with_its_cut_units_cut() {
            let mut lost = rebuilt_and_verified();
            let k = lost.t.ended_after;
            assert!(is_lost_write(&lost.conn, lost.t.volume_id).unwrap());
            let verdict =
                adopt(&lost.conn, &mut lost.t.store, lost.t.volume_id, LABEL, BS).unwrap();
            let on_tape = lost
                .t
                .positions
                .iter()
                .flat_map(|(_, p)| p.iter())
                .filter(|p| **p <= k)
                .count();
            assert_eq!(verdict, LostAdoption::Adopted { slices: on_tape });
            assert_eq!(
                lost.t.rows(&lost.conn),
                vec![
                    ("alpha".into(), "completed".into(), "current".into()),
                    ("bravo".into(), "cut".into(), "staged".into()),
                    ("charlie".into(), "cut".into(), "staged".into()),
                ],
                "a cut unit's version is not promoted"
            );
            let (status, ended_after, files): (String, Option<i64>, i64) = lost
                .conn
                .query_row(
                    "SELECT status, ended_after, num_data_files FROM volumes WHERE id = ?1",
                    params![lost.t.volume_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .unwrap();
            assert_eq!(status, "sealed");
            assert_eq!(ended_after, Some(i64::from(k)));
            assert_eq!(
                files, on_tape as i64,
                "only the slices on the tape are counted"
            );
            let detail: String = lost
                .conn
                .query_row(
                    "SELECT details FROM events WHERE action = 'write_completed'
                      ORDER BY id DESC LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            let detail: serde_json::Value = serde_json::from_str(&detail).unwrap();
            assert_eq!(detail["ending"], "short");
            assert_eq!(detail["ended_after"], k);
            assert_eq!(detail["cut_units"], serde_json::json!(["bravo", "charlie"]));
            assert!(
                detail["adopted"]
                    .as_str()
                    .unwrap()
                    .starts_with("adopted by `volume resume`"),
                "{detail}"
            );
        }

        /// The catalog's rows must say what the tape says: a unit with a
        /// slice past the end recorded `completed` is refused, as is a
        /// recorded ending other than the seal's.
        #[test]
        fn a_catalog_that_disagrees_with_where_the_tape_ended_is_refused() {
            let mut lost = rebuilt_and_verified();
            lost.conn
                .execute(
                    "UPDATE writes SET status = 'completed' WHERE stage_set_id =
                       (SELECT ss.id FROM stage_sets ss JOIN snapshots s ON s.id = ss.snapshot_id
                          JOIN units u ON u.id = s.unit_id WHERE u.name = 'bravo')",
                    [],
                )
                .unwrap();
            let LostAdoption::NotWhatThisCatalogStaged { why } =
                adopt(&lost.conn, &mut lost.t.store, lost.t.volume_id, LABEL, BS).unwrap()
            else {
                panic!("a completed row past the end must be refused");
            };
            assert!(why.contains("it is cut"), "{why}");

            let mut lost = rebuilt_and_verified();
            lost.conn
                .execute(
                    "UPDATE volumes SET ended_after = ended_after - 1 WHERE id = ?1",
                    params![lost.t.volume_id],
                )
                .unwrap();
            let LostAdoption::NotWhatThisCatalogStaged { why } =
                adopt(&lost.conn, &mut lost.t.store, lost.t.volume_id, LABEL, BS).unwrap()
            else {
                panic!("a recorded ending other than the seal's must be refused");
            };
            assert!(why.contains("the tape's seal says"), "{why}");
            assert_eq!(
                lost.conn
                    .query_row(
                        "SELECT status FROM volumes WHERE id = ?1",
                        params![lost.t.volume_id],
                        |r| r.get::<_, String>(0)
                    )
                    .unwrap(),
                "initialized",
                "nothing adopted"
            );
        }
    }
}
