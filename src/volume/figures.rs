//! A sealed copy's write figures, recorded after the fact by a passing full
//! `volume verify` (ADR-0012 2026-10-07 item 33, issue #423).
//!
//! A write that seals and confirms a copy records its figures afterwards
//! (`write::record_after_sealed`): `bytes_written`, `num_data_files`,
//! `has_manifest`, `first_write`, `last_write`, and the volume's
//! `write_completed` event. A catalog error there does not fail the write —
//! the copy stands — and the warning it prints names `tapectl volume verify
//! <label>` as the command that finishes it. This is what that command does.
//!
//! Every figure comes from what the verify read or from the catalog's own
//! rows, never from the clock: the slice count and the slices' on-tape bytes
//! from the front index the verify just proved (bound by the seal marker,
//! every content file hashed against it), `first_write` and `last_write`
//! from the volume's own `writes` rows (when its write began and when the
//! confirm completed it). `bytes_written` pads each slice to the verify's
//! block size, the format's one block size (`cli::volume::DEFAULT_BLOCK_SIZE`;
//! the config key is refused), which a fixed-block read of the tape at that
//! size has just matched.
//!
//! Only what is missing or provably wrong is written: a recorded volume is
//! left byte-for-byte as it was, and nothing is recorded at all when the
//! tape's slices are not the ones the catalog staged for this volume.

use std::collections::BTreeMap;

use rusqlite::{params, Connection, OptionalExtension};

use crate::db::events;
use crate::error::Result;
use crate::policy::coverage;

use super::layout_model::{Layout, ZoneKind};

/// What a passing full verify did about the volume's write figures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Figures {
    /// Figures that were missing or wrong, now recorded: one
    /// `"<figure>: <old> -> <new>"` per change, in a fixed order.
    Recorded(Vec<String>),
    /// Figures are missing or wrong, but the tape and the catalog's staged
    /// rows disagree about the volume's slices, so nothing was recorded.
    /// The reason, for the operator.
    Withheld(String),
}

/// The `events.action` that says a verify recorded a volume's figures.
pub const EVENT_FIGURES_RECORDED: &str = "write_figures_recorded";

/// Record what a sealed copy's write left unrecorded, after a passing FULL
/// verify of it. `conn` is the verify's own transaction, so the figures and
/// the event saying so commit with the verification session or not at all.
///
/// `None` when there is nothing to do: the volume does not count as a copy,
/// no write of this catalog completed it (a row `catalog rebuild` made keeps
/// that command's figures), or every figure is already what the tape says.
///
/// "Written by this catalog" is every completed (or, on a tape that ended
/// early, cut: #420; or a range: #478) write on the volume having a start: a
/// write session stamps `writes.started_at` when it begins, and `catalog
/// rebuild` inserts its rows with none. On a short tape the figures count
/// only the slices at or before `volumes.ended_after`, the ones on the tape.
pub(crate) fn record_after_verify(
    conn: &Connection,
    volume_id: i64,
    label: &str,
    layout: &Layout,
    session_id: i64,
) -> Result<Option<Figures>> {
    let counts_as_copy: bool = conn
        .query_row(
            &format!(
                "SELECT {} FROM volumes v WHERE v.id = ?1",
                coverage::eligible("v")
            ),
            params![volume_id],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(false);
    if !counts_as_copy {
        return Ok(None);
    }
    // #420: a `cut` write (the early end of a short tape cut its unit) is
    // as much this catalog's write as a `completed` one, and a sealed volume
    // whose warning came inside its first unit holds only cut writes: its
    // figures are still the tape's. A cut write records no completion time.
    // #478 (ADR-0012 item 127): so is a `range` write, whose slices on the
    // tape are its `written` positions; a continuation tape may hold nothing
    // else.
    let (written, begun_here, last_completed): (i64, i64, Option<String>) = conn.query_row(
        "SELECT COUNT(*), COUNT(started_at), MAX(completed_at)
         FROM writes WHERE volume_id = ?1 AND status IN ('completed', 'cut', 'range')",
        params![volume_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    if written == 0 || begun_here != written {
        return Ok(None);
    }
    // #420 (`volume-format-v2.md` §4.2): where a short tape ended. The
    // front index is the plan; only its slices at or before `ended_after`
    // are on the tape, and the verify read only those.
    let ended_after: Option<i64> = conn.query_row(
        "SELECT ended_after FROM volumes WHERE id = ?1",
        params![volume_id],
        |r| r.get(0),
    )?;
    // The same `first_write` the write records (`record_write_bookkeeping`,
    // issue #392): when the volume's first write began.
    let first_began: Option<String> = conn.query_row(
        "SELECT MIN(started_at) FROM writes WHERE volume_id = ?1",
        params![volume_id],
        |r| r.get(0),
    )?;

    // What the tape proves: the front index this verify bound to the seal
    // and hashed every content file against.
    let slices: Vec<TapeSlice<'_>> = layout
        .entries
        .iter()
        .filter(|e| matches!(e.kind, ZoneKind::Slice { .. }))
        .filter(|e| ended_after.is_none_or(|k| i64::from(e.position) <= k))
        .map(|e| TapeSlice {
            position: i64::from(e.position),
            on_tape_bytes: e.on_tape_bytes(layout.block_size),
            sha256: e.sha256.as_deref(),
        })
        .collect();
    let data_files = slices.len() as i64;
    let tape_bytes: Option<i64> = slices
        .iter()
        .map(|s| s.on_tape_bytes)
        .sum::<Option<u64>>()
        .map(|b| b as i64);
    let tape_has_manifest = layout
        .entries
        .iter()
        .any(|e| matches!(e.kind, ZoneKind::OperatorEnvelope));

    let (bytes_written, num_data_files, has_manifest, first_write, last_write): (
        i64,
        i64,
        i64,
        Option<String>,
        Option<String>,
    ) = conn.query_row(
        "SELECT bytes_written, num_data_files, has_manifest, first_write, last_write
         FROM volumes WHERE id = ?1",
        params![volume_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )?;
    let has_completed_event: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM events
                       WHERE entity_type = 'volume' AND entity_id = ?1
                         AND action = 'write_completed')",
        params![volume_id],
        |r| r.get(0),
    )?;

    // Missing (the column's unrecorded value) or provably wrong (a figure
    // the tape contradicts). A recorded date is never replaced: nothing on
    // the tape or in the catalog proves one wrong.
    let mut changes: Vec<String> = Vec::new();
    let new_bytes = tape_bytes.filter(|b| *b != bytes_written);
    if let Some(b) = new_bytes {
        changes.push(format!("bytes_written: {bytes_written} -> {b}"));
    }
    if num_data_files != data_files {
        changes.push(format!("num_data_files: {num_data_files} -> {data_files}"));
    }
    let new_manifest = has_manifest == 0 && tape_has_manifest;
    if new_manifest {
        changes.push("has_manifest: 0 -> 1".to_string());
    }
    let new_first = first_began.filter(|_| first_write.is_none());
    if let Some(t) = &new_first {
        changes.push(format!("first_write: (none) -> {t}"));
    }
    let new_last = last_completed.clone().filter(|_| last_write.is_none());
    if let Some(t) = &new_last {
        changes.push(format!("last_write: (none) -> {t}"));
    }
    if !has_completed_event {
        changes.push("write_completed event: (none) -> recorded".to_string());
    }
    if changes.is_empty() {
        return Ok(None);
    }

    if let Some(why) = disagreement(conn, volume_id, &slices, ended_after)? {
        return Ok(Some(Figures::Withheld(why)));
    }

    conn.execute(
        "UPDATE volumes SET bytes_written = COALESCE(?2, bytes_written),
                            num_data_files = ?3,
                            has_manifest = CASE WHEN ?4 THEN 1 ELSE has_manifest END,
                            first_write = COALESCE(first_write, ?5),
                            last_write = COALESCE(last_write, ?6)
         WHERE id = ?1",
        params![
            volume_id,
            new_bytes,
            data_files,
            new_manifest,
            new_first,
            new_last
        ],
    )?;
    if !has_completed_event {
        events::log_event(
            conn,
            "volume",
            volume_id,
            Some(label),
            "write_completed",
            None,
            None,
            None,
            Some(&format!(
                "recorded after the fact by `volume verify` (verification session \
                 {session_id}): the write that sealed this copy completed at {}, and recording \
                 this event then failed (ADR-0012 2026-10-07 item 33)",
                last_completed
                    .as_deref()
                    .unwrap_or("a time the catalog does not hold")
            )),
            None,
        )?;
    }
    events::log_event(
        conn,
        "volume",
        volume_id,
        Some(label),
        EVENT_FIGURES_RECORDED,
        None,
        None,
        None,
        Some(&format!(
            "a passing full verify (verification session {session_id}) recorded the write \
             figures this sealed copy was missing or had wrong, from its front index and this \
             catalog's writes (ADR-0012 2026-10-07 item 33): {}",
            changes.join("; ")
        )),
        None,
    )?;
    Ok(Some(Figures::Recorded(changes)))
}

/// One slice as the verified front index lists it.
struct TapeSlice<'a> {
    position: i64,
    /// Its true size padded to whole blocks; `None` when the entry is
    /// unsized.
    on_tape_bytes: Option<u64>,
    sha256: Option<&'a str>,
}

/// Why the tape's slices are not the ones this catalog wrote to the volume,
/// or `None` when they are: the same files, at the same positions, with the
/// hashes it staged. A slice's hash may match either `write_positions.
/// sha256_on_volume` or `stage_slices.sha256_encrypted`, which paths have
/// filled differently (the either-match `read_slices` uses, issue #86).
///
/// On a tape that ended early (#420, `ended_after = Some(K)`), the slices
/// this catalog wrote there are those of its `completed`, `cut` and `range`
/// (#478) writes at or before K: a cut write's slices that reached the tape
/// are on it (and stay recorded, ADR-0012 item 112), and its positions after
/// K never were. A range's positions after K are `pending` (item 121): they
/// never reached the tape either.
fn disagreement(
    conn: &Connection,
    volume_id: i64,
    tape: &[TapeSlice<'_>],
    ended_after: Option<i64>,
) -> Result<Option<String>> {
    let rows: Vec<(i64, Option<String>, String)> = conn
        .prepare(
            "SELECT CAST(wp.position AS INTEGER), wp.sha256_on_volume, sl.sha256_encrypted
             FROM write_positions wp
             JOIN writes w ON w.id = wp.write_id
             JOIN stage_slices sl ON sl.id = wp.stage_slice_id
             WHERE w.volume_id = ?1 AND w.status IN ('completed', 'cut', 'range')
               AND (?2 IS NULL OR CAST(wp.position AS INTEGER) <= ?2)",
        )?
        .query_map(params![volume_id, ended_after], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut catalog: BTreeMap<i64, (Option<String>, String)> = BTreeMap::new();
    for (position, on_volume, staged) in rows {
        if catalog.insert(position, (on_volume, staged)).is_some() {
            return Ok(Some(format!(
                "the catalog records two slices of this volume at file {position}"
            )));
        }
    }
    for slice in tape {
        let position = slice.position;
        let Some(tape_sha) = slice.sha256 else {
            return Ok(Some(format!(
                "the front index records no hash for the slice at file {position}"
            )));
        };
        let Some((on_volume, staged)) = catalog.get(&position) else {
            return Ok(Some(format!(
                "the tape carries a slice at file {position}, and this catalog records none \
                 of this volume there"
            )));
        };
        if on_volume.as_deref() != Some(tape_sha) && staged != tape_sha {
            return Ok(Some(format!(
                "the slice at file {position} hashes to {tape_sha} in the tape's front index, \
                 but this catalog staged {staged}"
            )));
        }
    }
    for position in catalog.keys() {
        if !tape.iter().any(|s| s.position == *position) {
            return Ok(Some(format!(
                "the catalog records a slice of this volume at file {position}, which the \
                 tape's front index does not list as a slice"
            )));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    //! #420: the figures a full verify records for a tape that ended early,
    //! on the shared fixture (`volume::restore::short_tape`).
    use super::*;
    use crate::config::Config;
    use crate::store::Tier;
    use crate::tape::contact::{ContactSite, Medium, Operation};
    use crate::volume::restore::short_tape::{self, ShortTape, BS, LABEL};

    fn cfg() -> &'static Config {
        static CFG: std::sync::OnceLock<Config> = std::sync::OnceLock::new();
        CFG.get_or_init(Config::default)
    }

    fn full_verify(t: &mut ShortTape) {
        let r = crate::volume::write::volume_verify_with_store(
            &t.conn,
            &mut t.store,
            LABEL,
            t.volume_id,
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
    }

    fn data_files(t: &ShortTape) -> i64 {
        t.conn
            .query_row(
                "SELECT num_data_files FROM volumes WHERE id = ?1",
                params![t.volume_id],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn on_tape(t: &ShortTape) -> i64 {
        t.positions
            .iter()
            .flat_map(|(_, p)| p.iter())
            .filter(|p| **p <= t.ended_after)
            .count() as i64
    }

    fn recorded(t: &ShortTape) -> bool {
        t.conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM events WHERE action = ?1 AND entity_id = ?2)",
                params![EVENT_FIGURES_RECORDED, t.volume_id],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// A short tape's figures count the slices on it, never the plan's: a
    /// cut write's slice at K is on the tape, its later ones are not.
    #[test]
    fn a_full_verify_of_a_short_tape_records_only_the_slices_on_it() {
        let mut t = short_tape::short_tape();
        assert_eq!(
            data_files(&t),
            0,
            "the fixture leaves the figures unrecorded"
        );
        full_verify(&mut t);
        assert!(recorded(&t), "the figures were recorded, not withheld");
        assert_eq!(data_files(&t), on_tape(&t));
        assert_eq!(on_tape(&t), 3, "alpha's two slices and bravo's first");
    }

    /// The warning inside the first unit: a sealed volume holding no unit
    /// whole, every write `cut`. It is still a sealed tape with figures.
    #[test]
    fn a_sealed_volume_with_only_cut_writes_gets_its_figures() {
        let mut t = short_tape::cut_only_tape();
        assert_eq!(
            t.rows(&t.conn)
                .into_iter()
                .map(|(_, w, _)| w)
                .collect::<Vec<_>>(),
            vec!["cut", "cut", "cut"]
        );
        full_verify(&mut t);
        assert!(recorded(&t));
        assert_eq!(
            data_files(&t),
            1,
            "alpha's first slice is the one on the tape"
        );
    }

    /// The control: a full tape records every planned slice.
    #[test]
    fn a_full_verify_of_a_full_tape_records_every_slice() {
        let mut t = short_tape::full_tape();
        full_verify(&mut t);
        assert_eq!(data_files(&t), 6);
    }
}
