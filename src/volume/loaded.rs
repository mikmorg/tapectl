//! The loaded tape as a volume reference (issue #435).
//!
//! A volume label on a command line is a cross-check of intent against File
//! 0's uuid. For a WRITE that cross-check is the point: it ties the data to
//! the volume `volume init` consented to (ADR-0003). For a READ it adds
//! little, because File 0 identifies the volume anyway — so `volume verify`
//! and `restore volume|unit|file` accept no label and find the volume in
//! this catalog from the File 0 read they make at contact in any case, and
//! say which one they chose. A label that IS given is checked against File 0
//! exactly as before; this module is not reached.
//!
//! Three pieces, one per question:
//!
//! - [`locate`], [`resolve`] and [`account`]: File 0's uuid -> what the
//!   catalog makes of it (one lookup), read as the catalog's volume or a
//!   refusal naming what File 0 says ([`resolve`]), or as `volume identify`'s
//!   note and contradiction ([`account`]). The uuid decides, never the label:
//!   a label can be reused after a retire, the uuid cannot.
//! - [`identify`] and [`Peeked`]: the READ half. The File 0 read is made once
//!   and its bytes handed to the store, so the first read of File 0 that the
//!   path makes afterwards (`corroborate_volume`'s, which every read contact
//!   takes) is served from them. `Peekable` for a tape: peeking File 0 does
//!   not cost the path its own read, because the path never makes a second
//!   one. A second read would mean a rewind and another pass over the head
//!   of the tape — the cost `volume::cost_budget` pins.
//! - [`require_writable`] and [`require_resumable`]: the WRITE half. `volume
//!   write --loaded` and `volume resume --loaded` write the loaded tape only
//!   when it is one of this catalog's volumes in the state that command
//!   works on, and refuse naming the state otherwise. They run on the result
//!   of a `volume identify` contact, and the write that follows takes the
//!   same File 0 consent check it always does (ADR-0003) on its own handle;
//!   a tape swapped in between is refused there.

use std::io::{Read, Write};

use rusqlite::{params, Connection, OptionalExtension};

use crate::error::{Result, TapectlError};
use crate::policy::coverage;
use crate::store::{
    read_small_bytes, CapacityReport, ConfirmPlan, EarlyWarning, EraseMode, Evidence, Store,
    SMALL_FILE_CAP,
};
use crate::volume::binding::{self, File0Facts};
use crate::volume::layout_model::Layout;
use crate::volume::session::{self, AbortedAdoption, ResumeAdmission};

/// One volume of this catalog, found by what the loaded tape's File 0 says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LoadedVolume {
    pub volume_id: i64,
    pub label: String,
    pub uuid: String,
    pub status: String,
    pub observed_condition: String,
}

impl LoadedVolume {
    /// The volume's state in words, for a refusal: `sealed`, or
    /// `initialized, condition quarantined`.
    fn state(&self) -> String {
        if self.observed_condition == "ok" {
            self.status.clone()
        } else {
            format!("{}, condition {}", self.status, self.observed_condition)
        }
    }
}

/// What File 0 says, for a refusal: `volume "L6-0009" (uuid ...)`, or that it
/// names nothing.
fn what_file0_says(file0: &File0Facts) -> String {
    match (&file0.label, &file0.uuid) {
        (Some(label), Some(uuid)) => format!("volume \"{label}\" (uuid {uuid})"),
        (Some(label), None) => format!("volume \"{label}\" (no uuid)"),
        (None, Some(uuid)) => format!("a volume with uuid {uuid} and no label"),
        (None, None) => "no volume identity".to_string(),
    }
}

/// What this catalog makes of the tape whose File 0 is `file0`, looked up by
/// uuid and nothing else — the ONE derivation behind every question the
/// catalog is asked about a loaded tape (issue #443):
///
/// - [`resolve`], for `volume verify` / `restore` / `--loaded`, which turn
///   everything but [`Located::Known`] into a refusal;
/// - [`account`], for `volume identify`, which turns the same value into the
///   note it prints AND the contradiction its exit code reports.
///
/// Before it, `identify` printed a note from this lookup and took its exit
/// code from a second one, by label, and the two could disagree: a uuid the
/// catalog knows under another label was "wrong tape" in the note and exit 0
/// in the status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Located {
    /// File 0 carries no uuid (a blank, damaged or foreign tape): nothing to
    /// look up.
    NoIdentity,
    /// No volume of this catalog has that uuid. For `identify` an absence —
    /// the answer — and for a read that must act on a volume, a refusal.
    Unknown,
    /// The catalog's volume with that uuid, and File 0 agrees with it (or
    /// names no label).
    Known(LoadedVolume),
    /// The catalog knows the uuid, as `volume`, and File 0 names it
    /// `on_tape`: two known facts disagreeing, a wrong tape however it came
    /// about, not guessed through.
    UnderAnotherLabel {
        on_tape: String,
        volume: LoadedVolume,
    },
}

/// Look up the loaded tape's File 0 in this catalog, by uuid only. The uuid
/// decides, never the label: a label can be reused after a retire, the uuid
/// cannot. `Err` is only a database error.
pub(crate) fn locate(conn: &Connection, file0: &File0Facts) -> Result<Located> {
    let Some(uuid) = file0.uuid.as_deref().filter(|u| !u.is_empty()) else {
        return Ok(Located::NoIdentity);
    };
    let found: Option<LoadedVolume> = conn
        .query_row(
            "SELECT id, label, uuid, status, observed_condition FROM volumes WHERE uuid = ?1",
            params![uuid],
            |r| {
                Ok(LoadedVolume {
                    volume_id: r.get(0)?,
                    label: r.get(1)?,
                    uuid: r.get(2)?,
                    status: r.get(3)?,
                    observed_condition: r.get(4)?,
                })
            },
        )
        .optional()?;
    let Some(volume) = found else {
        return Ok(Located::Unknown);
    };
    if let Some(on_tape) = &file0.label {
        if on_tape != &volume.label {
            return Ok(Located::UnderAnotherLabel {
                on_tape: on_tape.clone(),
                volume,
            });
        }
    }
    Ok(Located::Known(volume))
}

impl Located {
    /// The refusal text for everything but [`Located::Known`], naming what
    /// File 0 says. One wording, whoever shows it: a refusal, or the note
    /// and contradiction `volume identify` reports.
    fn refusal(&self, file0: &File0Facts) -> Option<String> {
        match self {
            Located::Known(_) => None,
            Located::NoIdentity => Some(format!(
                "the loaded tape does not identify a volume: its File 0 carries {} — it is \
                 blank, damaged or not a tapectl tape. Name the volume on the command line \
                 instead. `tapectl volume identify` prints File 0 as it is.",
                what_file0_says(file0)
            )),
            Located::Unknown => Some(format!(
                "the loaded tape is {} by its File 0, and this catalog has no volume with that \
                 uuid. If this catalog lost it, `tapectl catalog rebuild --from-volume --key \
                 <KEY>` attaches it from the tape; otherwise this is another catalog's tape.",
                what_file0_says(file0)
            )),
            Located::UnderAnotherLabel { on_tape, volume } => Some(format!(
                "wrong tape: the loaded tape's File 0 names volume \"{on_tape}\", but this \
                 catalog knows that uuid ({}) as \"{}\". There is no --force for this — it is \
                 a fact tapectl cannot resolve on its own.",
                volume.uuid, volume.label
            )),
        }
    }
}

/// Find the volume of this catalog that the loaded tape's File 0 identifies.
///
/// By uuid only ([`locate`]). Refuses, naming what File 0 says, when File 0
/// carries no uuid (a blank, damaged or foreign tape), when no volume of this
/// catalog has that uuid (a tape this catalog does not know — after a lost
/// catalog, `catalog rebuild --from-volume` attaches it), and when the uuid
/// belongs to a volume this catalog calls something other than File 0 does,
/// which is a wrong tape however it came about and is not guessed through.
pub(crate) fn resolve(conn: &Connection, file0: &File0Facts) -> Result<LoadedVolume> {
    match locate(conn, file0)? {
        Located::Known(volume) => Ok(volume),
        other => Err(TapectlError::Other(
            other
                .refusal(file0)
                .expect("every Located but Known has a refusal"),
        )),
    }
}

/// What `volume identify` says about the catalog's view of the tape — the
/// same [`Located`] [`resolve`] refuses on, read the way `identify` reads it
/// (issue #443).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct CatalogAccount {
    /// The line for stderr: which volume this tape is, or that the catalog
    /// does not have it. Not a contradiction. `None` when File 0 names no
    /// uuid to look up.
    pub note: Option<String>,
    /// A fact that contradicts the catalog, which `identify` reports through
    /// its exit status: the uuid is the catalog's but under another label.
    /// The catalog NOT knowing a tape is not one — `identify` is what an
    /// operator runs precisely when they do not know what is loaded
    /// (ADR-0012, issue #193) — and neither is File 0 naming nothing.
    pub contradiction: Option<String>,
}

/// [`CatalogAccount`] for `located`, the lookup of `file0`.
pub(crate) fn account(located: &Located, file0: &File0Facts) -> CatalogAccount {
    match located {
        Located::NoIdentity => CatalogAccount::default(),
        Located::Unknown => CatalogAccount {
            note: located.refusal(file0).map(|why| format!("catalog: {why}")),
            contradiction: None,
        },
        Located::Known(v) => CatalogAccount {
            note: Some(format!(
                "catalog: this tape is volume \"{}\" (status {})",
                v.label, v.status
            )),
            contradiction: None,
        },
        Located::UnderAnotherLabel { .. } => CatalogAccount {
            note: None,
            contradiction: located.refusal(file0),
        },
    }
}

/// Say which volume a command chose, on stderr (stdout stays the command's
/// own output, `--json` included): `verifying L6-0002 (uuid ...), the loaded
/// tape`.
pub(crate) fn announce(doing: &str, volume: &LoadedVolume) {
    eprintln!("{}", announcement(doing, volume));
}

fn announcement(doing: &str, volume: &LoadedVolume) -> String {
    format!(
        "{doing} {} (uuid {}), the loaded tape",
        volume.label, volume.uuid
    )
}

/// Read File 0 off `store` ONCE, find the volume it names in this catalog,
/// and return it with the store to carry on with: a [`Peeked`] that serves
/// the first read of File 0 from the bytes just read.
///
/// The read path's own contact check ([`binding::corroborate_volume`], after
/// `read_file0_facts`) then finds File 0 where it expects it and makes no
/// read of its own — one File 0 read for the command, as with a label.
/// `announce` is not called here: the verb belongs to the caller.
///
/// A File 0 that cannot be read (a blank tape fails the read with EIO) or
/// is oversized refuses, naming that; so does a File 0 naming a volume this
/// catalog does not have ([`resolve`]).
pub(crate) fn identify<'s>(
    conn: &Connection,
    store: &'s mut dyn Store,
) -> Result<(LoadedVolume, Peeked<'s>)> {
    let raw = read_small_bytes(store, 0, "ID thunk").map_err(|e| {
        TapectlError::Other(format!(
            "could not read File 0 of the loaded tape to find which volume it is: {e}. Is the \
             tape blank or not yet initialised? Name the volume on the command line instead."
        ))
    })?;
    let file0 = binding::file0_facts_from_text(&String::from_utf8_lossy(&raw));
    let volume = resolve(conn, &file0)?;
    Ok((volume, Peeked::new(store, raw)))
}

/// A store whose File 0 has already been read: the first request for File
/// 0's head is answered from those bytes, and every other call goes to the
/// store untouched. See the module comment for why.
///
/// Only [`Store::read_file_head`] at position 0 is served from the bytes —
/// the bounded read every contact's File 0 look uses (`read_small`). A whole
/// `read_file(0)` is not: it is a content read (a full verify hashes File 0
/// against the front index) and must reach the medium. Served once, then
/// gone, so a later File 0 read is a real one.
///
/// **Delegates every method of [`Store`].** A method added to the trait with
/// a default body would otherwise run that default here instead of the
/// store's own override; `delegates_every_store_method` fails when one is
/// missed.
pub(crate) struct Peeked<'s> {
    inner: &'s mut dyn Store,
    file0: Option<Vec<u8>>,
}

impl<'s> Peeked<'s> {
    fn new(inner: &'s mut dyn Store, file0: Vec<u8>) -> Self {
        Self {
            inner,
            file0: Some(file0),
        }
    }

    /// `inner` with File 0's head already in hand (what [`Self::into_file0`]
    /// gave back): the restore keeps those bytes while its bay may swap
    /// the store under it, and lends them to the first contact check.
    ///
    pub(crate) fn primed(inner: &'s mut dyn Store, file0: Vec<u8>) -> Self {
        Self::new(inner, file0)
    }

    /// The File 0 bytes not yet served, so they can outlive the borrow of
    /// the store this wraps.
    pub(crate) fn into_file0(self) -> Option<Vec<u8>> {
        self.file0
    }
}

impl Store for Peeked<'_> {
    fn capacity(&mut self) -> Result<CapacityReport> {
        self.inner.capacity()
    }

    fn execute(&mut self, src: &mut dyn Read, len: u64, sync: bool) -> Result<u64> {
        self.inner.execute(src, len, sync)
    }

    fn confirm(&mut self, layout: &Layout, tier: crate::store::Tier) -> Result<Evidence> {
        self.inner.confirm(layout, tier)
    }

    fn confirm_with(&mut self, layout: &Layout, plan: ConfirmPlan<'_>) -> Result<Evidence> {
        self.inner.confirm_with(layout, plan)
    }

    fn read_file(&mut self, position: u32, sink: &mut dyn Write) -> Result<u64> {
        self.inner.read_file(position, sink)
    }

    fn read_file_head(
        &mut self,
        position: u32,
        max_bytes: u64,
        sink: &mut dyn Write,
    ) -> Result<u64> {
        if position == 0 {
            if let Some(bytes) = self.file0.take() {
                // `read_small` asks for the cap plus one byte to tell an
                // oversized file; the bytes held are at most the cap.
                debug_assert!(bytes.len() as u64 <= SMALL_FILE_CAP);
                let take = (bytes.len() as u64).min(max_bytes) as usize;
                sink.write_all(&bytes[..take])
                    .map_err(|e| TapectlError::Other(format!("sink write: {e}")))?;
                return Ok(take as u64);
            }
        }
        self.inner.read_file_head(position, max_bytes, sink)
    }

    fn reposition_for_resume(&mut self, file_index: u32) -> Result<()> {
        self.inner.reposition_for_resume(file_index)
    }

    fn reposition_at_most(&mut self, file_index: u32) -> Result<u32> {
        self.inner.reposition_at_most(file_index)
    }

    fn blank_at_bot(&mut self) -> Result<bool> {
        self.inner.blank_at_bot()
    }

    fn eod_at_or_before(&mut self, position: u32) -> Result<bool> {
        self.inner.eod_at_or_before(position)
    }

    fn early_warning(&self) -> Option<EarlyWarning> {
        self.inner.early_warning()
    }

    fn files_on_medium(&mut self) -> Result<Option<u32>> {
        self.inner.files_on_medium()
    }

    fn erase(&mut self, mode: EraseMode) -> Result<()> {
        self.inner.erase(mode)
    }
}

/// The refusal `--loaded` gives a volume in the wrong state: names the
/// state and what to do instead.
fn wrong_state(volume: &LoadedVolume, wants: &str, hint: &str) -> TapectlError {
    TapectlError::Other(format!(
        "volume \"{}\" is the loaded tape, but it is {}: {wants} {hint}",
        volume.label,
        volume.state()
    ))
}

/// How many `writes` rows the volume has in each of `statuses`, as `2
/// interrupted, 1 completed`; empty when it has none.
fn session_summary(conn: &Connection, volume_id: i64) -> Result<String> {
    let mut stmt = conn.prepare(
        "SELECT status, COUNT(*) FROM writes WHERE volume_id = ?1 GROUP BY status ORDER BY status",
    )?;
    let rows: Vec<String> = stmt
        .query_map(params![volume_id], |r| {
            Ok(format!(
                "{} {}",
                r.get::<_, i64>(1)?,
                r.get::<_, String>(0)?
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows.join(", "))
}

/// `volume write --loaded`: refuse unless the volume is `initialized`, its
/// condition is ok, and nothing has been written to it — no completed
/// write, no write session still open, no seal recorded. The gates
/// `volume write` itself applies, asked first so the refusal names the state
/// in `--loaded`'s terms.
pub(crate) fn require_writable(conn: &Connection, volume: &LoadedVolume) -> Result<()> {
    let wants = "--loaded writes only an initialized volume that nothing has been written to.";
    if !coverage::is_write_target(&volume.status, &volume.observed_condition) {
        return Err(wrong_state(
            volume,
            wants,
            "A sealed, retired, erased or quarantined volume is not a write target.",
        ));
    }
    if coverage::has_completed_write(conn, volume.volume_id)? {
        return Err(wrong_state(
            volume,
            wants,
            "It already holds a recorded write.",
        ));
    }
    let unfinished: i64 = conn.query_row(
        "SELECT COUNT(*) FROM writes
         WHERE volume_id = ?1 AND status IN ('planned','in_progress','interrupted')",
        params![volume.volume_id],
        |r| r.get(0),
    )?;
    if unfinished > 0 {
        return Err(wrong_state(
            volume,
            wants,
            &format!(
                "It has an unfinished write session ({}); `tapectl volume resume --loaded` \
                 continues it.",
                session_summary(conn, volume.volume_id)?
            ),
        ));
    }
    let sealed: bool = conn.query_row(
        "SELECT sealed_at IS NOT NULL FROM volumes WHERE id = ?1",
        params![volume.volume_id],
        |r| r.get(0),
    )?;
    if sealed {
        return Err(wrong_state(
            volume,
            wants,
            "Its seal is recorded; `tapectl volume resume --loaded` re-confirms it once a \
             passing full `tapectl volume verify` is recorded.",
        ));
    }
    Ok(())
}

/// `volume resume --loaded`: refuse unless the volume is one `volume resume`
/// would pick up — an interrupted write session, an aborted one that is
/// adoptable, or a write this catalog lost. The admission `volume resume`
/// itself applies ([`session::resume_admission`], [`crate::volume::adopt_lost`]),
/// asked first so the refusal names the state.
pub(crate) fn require_resumable(conn: &Connection, volume: &LoadedVolume) -> Result<()> {
    let wants = "--loaded resumes only a volume with an interrupted write session.";
    if !coverage::is_write_target(&volume.status, &volume.observed_condition) {
        return Err(wrong_state(
            volume,
            wants,
            "A sealed, retired, erased or quarantined volume has nothing to resume.",
        ));
    }
    if crate::volume::adopt_lost::is_lost_write(conn, volume.volume_id)? {
        return Ok(());
    }
    if coverage::has_completed_write(conn, volume.volume_id)? {
        return Err(wrong_state(
            volume,
            wants,
            "It already holds a recorded write.",
        ));
    }
    match session::resume_admission(conn, volume.volume_id)? {
        ResumeAdmission::Interrupted | ResumeAdmission::Aborted(AbortedAdoption::Adoptable) => {
            Ok(())
        }
        ResumeAdmission::Aborted(_) => Err(wrong_state(
            volume,
            wants,
            &format!(
                "Its write session was aborted and cannot be re-confirmed yet ({}); `tapectl \
                 volume resume {}` names the first unmet condition.",
                session_summary(conn, volume.volume_id)?,
                volume.label
            ),
        )),
        ResumeAdmission::Nothing => {
            let summary = session_summary(conn, volume.volume_id)?;
            let sessions = if summary.is_empty() {
                "it has no write session".to_string()
            } else {
                format!("its write sessions: {summary}")
            };
            Err(wrong_state(
                volume,
                wants,
                &format!(
                    "There is nothing to resume ({sessions}); `tapectl volume write --loaded` \
                     writes an initialized volume."
                ),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file0(label: Option<&str>, uuid: Option<&str>) -> File0Facts {
        File0Facts {
            label: label.map(str::to_string),
            uuid: uuid.map(str::to_string),
            media: None,
            unreadable: None,
        }
    }

    fn catalog_with(label: &str, uuid: &str, status: &str, condition: &str) -> (Connection, i64) {
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO volumes (label, uuid, backend_type, backend_name, media_type,
                                  capacity_bytes, status, observed_condition)
             VALUES (?1, ?2, 'lto', 'lto0', 'LTO-6', 2500000000000, ?3, ?4)",
            params![label, uuid, status, condition],
        )
        .unwrap();
        let id = conn.last_insert_rowid();
        (conn, id)
    }

    fn loaded(conn: &Connection, uuid: &str) -> LoadedVolume {
        resolve(conn, &file0(None, Some(uuid))).unwrap()
    }

    #[test]
    fn resolve_finds_the_volume_by_uuid_and_names_it() {
        let (conn, id) = catalog_with("L6-0002", "uuid-2", "sealed", "ok");
        let v = resolve(&conn, &file0(Some("L6-0002"), Some("uuid-2"))).unwrap();
        assert_eq!(v.volume_id, id);
        assert_eq!(v.label, "L6-0002");
        assert_eq!(v.status, "sealed");
        assert_eq!(
            announcement("verifying", &v),
            "verifying L6-0002 (uuid uuid-2), the loaded tape"
        );
        // File 0 carrying no label is an absence, not a contradiction.
        assert_eq!(
            resolve(&conn, &file0(None, Some("uuid-2"))).unwrap().label,
            "L6-0002"
        );
    }

    /// A tape this catalog does not know is refused naming what File 0 says,
    /// and what to do about it.
    #[test]
    fn resolve_refuses_an_unknown_tape_naming_what_file0_says() {
        let (conn, _) = catalog_with("L6-0002", "uuid-2", "sealed", "ok");
        let err = resolve(&conn, &file0(Some("L6-0009"), Some("uuid-9")))
            .unwrap_err()
            .to_string();
        assert!(err.contains("volume \"L6-0009\" (uuid uuid-9)"), "{err}");
        assert!(err.contains("no volume with that uuid"), "{err}");
        assert!(err.contains("catalog rebuild --from-volume"), "{err}");
    }

    #[test]
    fn resolve_refuses_a_file0_that_identifies_nothing() {
        let (conn, _) = catalog_with("L6-0002", "uuid-2", "sealed", "ok");
        for facts in [file0(None, None), file0(Some("L6-0002"), None)] {
            let err = resolve(&conn, &facts).unwrap_err().to_string();
            assert!(err.contains("does not identify a volume"), "{err}");
        }
        // Positive control: the same catalog resolves a File 0 that does.
        assert!(resolve(&conn, &file0(Some("L6-0002"), Some("uuid-2"))).is_ok());
    }

    /// The uuid decides; a label that disagrees with it is a wrong tape,
    /// not a tape to guess through.
    #[test]
    fn resolve_refuses_a_uuid_the_catalog_knows_under_another_label() {
        let (conn, _) = catalog_with("L6-0002", "uuid-2", "sealed", "ok");
        let err = resolve(&conn, &file0(Some("L6-0007"), Some("uuid-2")))
            .unwrap_err()
            .to_string();
        assert!(err.contains("wrong tape"), "{err}");
        assert!(
            err.contains("\"L6-0007\"") && err.contains("\"L6-0002\""),
            "{err}"
        );
    }

    /// `locate` is the one lookup, and `resolve` (refusals) and `account`
    /// (`volume identify`'s note and contradiction) are two readings of it
    /// (issue #443): for every File 0 a catalog can be asked about, the
    /// reading that refuses is the reading that contradicts, and never does a
    /// note stand beside a contradiction.
    #[test]
    fn resolve_and_identifys_account_are_two_readings_of_one_lookup() {
        let (conn, _) = catalog_with("L6-0002", "uuid-2", "sealed", "ok");
        let cases = [
            // (File 0, located, refused by resolve, contradicts, has a note)
            (
                file0(Some("L6-0002"), Some("uuid-2")),
                "known",
                false,
                false,
                true,
            ),
            (file0(None, Some("uuid-2")), "known", false, false, true),
            (
                file0(Some("L6-0009"), Some("uuid-9")),
                "unknown",
                true,
                false,
                true,
            ),
            (
                file0(Some("L6-0007"), Some("uuid-2")),
                "another label",
                true,
                true,
                false,
            ),
            (
                file0(Some("L6-0002"), None),
                "no identity",
                true,
                false,
                false,
            ),
            (file0(None, None), "no identity", true, false, false),
        ];
        for (facts, what, refused, contradicts, noted) in cases {
            let located = locate(&conn, &facts).unwrap();
            let label = match &located {
                Located::Known(_) => "known",
                Located::Unknown => "unknown",
                Located::UnderAnotherLabel { .. } => "another label",
                Located::NoIdentity => "no identity",
            };
            assert_eq!(label, what, "{facts:?}");
            let account = account(&located, &facts);
            assert_eq!(resolve(&conn, &facts).is_err(), refused, "{what}: resolve");
            assert_eq!(
                account.contradiction.is_some(),
                contradicts,
                "{what}: identify's exit status"
            );
            assert_eq!(account.note.is_some(), noted, "{what}: identify's note");
            // A tape that is refused only for being unknown or unreadable is
            // an absence for `identify`; a tape refused as a WRONG tape is a
            // contradiction, in the resolver's own words.
            if contradicts {
                assert_eq!(
                    account.contradiction,
                    Some(resolve(&conn, &facts).unwrap_err().to_string()),
                    "{what}"
                );
            }
        }
    }

    #[test]
    fn a_writable_volume_passes_and_every_other_state_is_refused_naming_it() {
        let (conn, _) = catalog_with("W1", "u-w1", "initialized", "ok");
        require_writable(&conn, &loaded(&conn, "u-w1")).unwrap();

        for (status, condition, says) in [
            ("sealed", "ok", "it is sealed"),
            ("retired", "ok", "it is retired"),
            ("erased", "ok", "it is erased"),
            (
                "initialized",
                "quarantined",
                "initialized, condition quarantined",
            ),
        ] {
            let (conn, _) = catalog_with("W2", "u-w2", status, condition);
            let err = require_writable(&conn, &loaded(&conn, "u-w2"))
                .unwrap_err()
                .to_string();
            assert!(err.contains(says), "{status}/{condition}: {err}");
            assert!(err.contains("\"W2\""), "{err}");
            assert!(err.contains("--loaded writes only an initialized"), "{err}");
        }
    }

    fn add_write(conn: &Connection, volume_id: i64, status: &str) {
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('t', 0, 'active')",
            [],
        )
        .unwrap();
        let tenant = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
             VALUES ('un', 'un', ?1, 'mtime_size', 1, 'active')",
            params![tenant],
        )
        .unwrap();
        let unit = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, status, source_path, file_count, total_size)
             VALUES (?1, 1, 'staged', '/tmp', 1, 16)",
            params![unit],
        )
        .unwrap();
        let snap = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (?1, 'staged', 524288)",
            params![snap],
        )
        .unwrap();
        let ss = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status)
             VALUES (?1, ?2, ?3, ?4)",
            params![ss, snap, volume_id, status],
        )
        .unwrap();
    }

    #[test]
    fn write_loaded_refuses_a_volume_with_a_recorded_or_unfinished_write() {
        let (conn, id) = catalog_with("W3", "u-w3", "initialized", "ok");
        add_write(&conn, id, "interrupted");
        let err = require_writable(&conn, &loaded(&conn, "u-w3"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("unfinished write session (1 interrupted)"),
            "{err}"
        );
        assert!(err.contains("volume resume --loaded"), "{err}");

        let (conn, id) = catalog_with("W4", "u-w4", "initialized", "ok");
        add_write(&conn, id, "completed");
        let err = require_writable(&conn, &loaded(&conn, "u-w4"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("already holds a recorded write"), "{err}");
    }

    #[test]
    fn write_loaded_refuses_a_volume_whose_seal_is_recorded() {
        let (conn, id) = catalog_with("W5", "u-w5", "initialized", "ok");
        conn.execute(
            "UPDATE volumes SET sealed_at = '2026-10-15 00:00:00' WHERE id = ?1",
            params![id],
        )
        .unwrap();
        let err = require_writable(&conn, &loaded(&conn, "u-w5"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("Its seal is recorded"), "{err}");
    }

    #[test]
    fn resume_loaded_passes_an_interrupted_session_and_refuses_the_rest_naming_the_state() {
        let (conn, id) = catalog_with("R1", "u-r1", "initialized", "ok");
        add_write(&conn, id, "interrupted");
        require_resumable(&conn, &loaded(&conn, "u-r1")).unwrap();

        // Nothing to resume: initialized, never written.
        let (conn, _) = catalog_with("R2", "u-r2", "initialized", "ok");
        let err = require_resumable(&conn, &loaded(&conn, "u-r2"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("it is initialized"), "{err}");
        assert!(
            err.contains("nothing to resume (it has no write session)"),
            "{err}"
        );
        assert!(err.contains("volume write --loaded"), "{err}");

        // Sealed: not a write target at all.
        let (conn, _) = catalog_with("R3", "u-r3", "sealed", "ok");
        let err = require_resumable(&conn, &loaded(&conn, "u-r3"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("it is sealed"), "{err}");

        // A completed write with no lost-write evidence is a recorded one.
        let (conn, id) = catalog_with("R4", "u-r4", "initialized", "ok");
        add_write(&conn, id, "completed");
        conn.execute(
            "UPDATE volumes SET sealed_at = '2026-10-15 00:00:00' WHERE id = ?1",
            params![id],
        )
        .unwrap();
        let err = require_resumable(&conn, &loaded(&conn, "u-r4"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("already holds a recorded write"), "{err}");
    }

    /// A tape whose File 0 is a real v2 ID thunk for `label` / `uuid`.
    fn tape(label: &str, uuid: &str) -> crate::store::MemStore {
        let thunk =
            crate::volume::layout::generate_id_thunk_v2(&crate::volume::layout::IdThunkV2Params {
                label,
                uuid,
                media_type: "LTO-6",
                tapectl_version: "0.0.0-test",
                nominal_capacity: 2_500_000_000_000,
                mam_capacity: 2_400_000_000_000,
                total_files: 6,
                mam_manufacturer: "TESTCO",
                mam_serial: "",
                mam_length: 846,
                mam_loads: 1,
                created_at: "2026-10-15T00:00:00Z",
                cartridge_identity_source: None,
            })
            .into_bytes();
        let mut store = crate::store::MemStore::new(4096);
        store
            .execute(
                &mut std::io::Cursor::new(thunk.clone()),
                thunk.len() as u64,
                false,
            )
            .unwrap();
        store
    }

    /// `identify` reads File 0 once, and the store it hands back serves the
    /// NEXT read of File 0's head from those bytes: the path that carries on
    /// (every read contact's `read_file0_facts`) finds File 0 where it looks
    /// and the drive is not asked again. Only that one read: a second one,
    /// and a whole-file read of File 0 (a content read), reach the medium.
    #[test]
    fn identify_reads_file0_once_and_the_peeked_store_serves_only_the_next_head_read() {
        use crate::tape::fake::{FakeTape, Op};
        let (conn, _) = catalog_with("P1", "uuid-p1", "sealed", "ok");
        let mem = tape("P1", "uuid-p1");
        let fake = FakeTape::with_files(mem.files.clone(), 4096);
        let mut store = crate::store::TapeStore::from_ops(fake.boxed(), 0).unwrap();

        let (volume, mut peeked) = identify(&conn, &mut store).unwrap();
        assert_eq!(volume.label, "P1");
        let file0_reads =
            |fake: &FakeTape| fake.ops().iter().filter(|o| **o == Op::ReadHead(0)).count();
        assert_eq!(file0_reads(&fake), 1, "identify read File 0");

        // The contact check's own read of File 0: served, the drive is idle.
        let facts = binding::read_file0_facts(&mut peeked);
        assert_eq!(facts.uuid.as_deref(), Some("uuid-p1"));
        assert_eq!(facts.label.as_deref(), Some("P1"));
        assert_eq!(file0_reads(&fake), 1, "served from the held bytes");

        // Served once, then gone: the next read is a real one.
        let again = binding::read_file0_facts(&mut peeked);
        assert_eq!(again.uuid.as_deref(), Some("uuid-p1"));
        assert_eq!(file0_reads(&fake), 2, "a later read reaches the medium");

        // A whole-file read of File 0 is a content read and always does.
        let mut sink = Vec::new();
        let mut store2 = crate::store::TapeStore::from_ops(fake.boxed(), 0).unwrap();
        let (_, mut peeked2) = identify(&conn, &mut store2).unwrap();
        fake.clear_ops();
        peeked2.read_file(0, &mut sink).unwrap();
        assert!(fake.ops().contains(&Op::Read(0)), "{:?}", fake.ops());
        assert!(!sink.is_empty());
    }

    /// A File 0 that cannot be read refuses, naming that; a File 0 naming a
    /// volume the catalog lacks refuses, naming it.
    #[test]
    fn identify_refuses_an_unreadable_or_unknown_file0() {
        let (conn, _) = catalog_with("P2", "uuid-p2", "sealed", "ok");
        let mut empty = crate::store::MemStore::new(4096);
        let err = identify(&conn, &mut empty)
            .err()
            .expect("blank tape")
            .to_string();
        assert!(err.contains("could not read File 0"), "{err}");

        let mut stranger = tape("STRANGER", "uuid-stranger");
        let err = identify(&conn, &mut stranger)
            .err()
            .expect("unknown")
            .to_string();
        assert!(
            err.contains("STRANGER") && err.contains("uuid-stranger"),
            "{err}"
        );
    }

    /// `Peeked` implements every method of `Store`, so none runs the trait's
    /// default body where the store has an override of its own. Read from the
    /// trait's source: a method added there without being added here fails
    /// this test.
    #[test]
    fn delegates_every_store_method() {
        const STORE: &str = include_str!("../store.rs");
        const HERE: &str = include_str!("loaded.rs");
        let trait_start = STORE.find("pub trait Store {").unwrap();
        let trait_body =
            &STORE[trait_start..trait_start + STORE[trait_start..].find("\n}\n").unwrap()];
        let methods: Vec<&str> = trait_body
            .lines()
            .filter_map(|l| l.strip_prefix("    fn "))
            .filter_map(|l| l.split(['(', '<']).next())
            .collect();
        assert!(
            methods.len() >= 10,
            "positive control: the trait's methods were found: {methods:?}"
        );
        let impl_start = HERE.find("impl Store for Peeked").unwrap();
        let impl_body = &HERE[impl_start..impl_start + HERE[impl_start..].find("\n}\n").unwrap()];
        for m in &methods {
            assert!(
                impl_body.contains(&format!("    fn {m}(")),
                "Peeked does not implement Store::{m}: it would run the trait default"
            );
        }
    }

    /// An aborted session that cannot be re-confirmed yet is named as such,
    /// with the command that names the unmet condition.
    #[test]
    fn resume_loaded_names_an_aborted_session_that_is_not_adoptable_yet() {
        let (conn, id) = catalog_with("R5", "u-r5", "initialized", "ok");
        add_write(&conn, id, "aborted");
        let err = require_resumable(&conn, &loaded(&conn, "u-r5"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("write session was aborted"), "{err}");
        assert!(err.contains("(1 aborted)"), "{err}");
        assert!(err.contains("volume resume R5"), "{err}");
    }
}
