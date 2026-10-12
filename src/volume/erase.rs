//! `tapectl cartridge erase` — overwrite a tape, with careful verification
//! (ADR-0012, 2026-10-12 items 51 and 52).
//!
//! Six steps, in this order, and nothing is written before the fourth:
//!
//! 1. **Identify, read-only.** The drive is opened read-only and the one
//!    medium-contact check every write path runs ([`session::contact_report`])
//!    reads File 0 and probes the seal marker at the position File 0 itself
//!    names. What it found is reported: the chip serial; File 0's label, uuid,
//!    layout version, writer and dates (#305); the seal and the front-index
//!    totals its embedded copy carries; or that File 0 is empty, unreadable,
//!    or not a tapectl ID thunk at all. A blank tape is reported blank and the
//!    command succeeds having done nothing — and so does an EMPTY File 0
//!    under the default short erase, which is exactly the state that erase
//!    leaves.
//! 2. **Cross-check the catalog** by File 0's uuid and by the chip serial
//!    (the cartridge it names, and the volumes mounted on it). A volume the
//!    catalog has not given up ([`coverage::given_up`]: anything but
//!    `retired`/`erased`) is refused, naming `volume retire` — the only way to
//!    give one up. A tape and catalog that disagree about which cartridge a
//!    volume is on are refused too. `--dry-run` stops here.
//! 3. **Consent** is the label just read from File 0, typed (for a tape
//!    tapectl did not write, the chip serial): at a terminal, or as
//!    `--confirm-label`, which must equal it. `--yes` never gives it
//!    ([`crate::cli::consent::confirm_typed`]).
//!    No erase runs without a chip serial read in step 1: nothing else
//!    proves afterwards which cartridge was erased, or records it.
//! 4. **Erase.** The drive is reopened read-write; File 0 is read again and
//!    must be what step 1 read, and the chip read again must report step 1's
//!    serial (the same tape is still loaded — File 0 alone cannot tell two
//!    unreadable tapes apart); then the default short erase writes one
//!    filemark at the beginning of the tape, or `--long` runs the st
//!    driver's long erase of the whole medium.
//! 5. **Verify.** Reopened read-only: File 0 must read back EMPTY (short) or
//!    BLANK (long), and the chip serial, read again, must be unchanged. Any
//!    failure from here on is never exit 2: the tape has been written.
//! 6. **Record**, only after a passing verify: the cartridge — registered by
//!    its chip serial as `available` if the catalog did not know it, or, if
//!    it did, its retired volumes marked `erased` and the cartridge returned
//!    to `available` the way `cartridge mark-erased` does (a
//!    `retired_permanent` cartridge keeps that status) — and one `erased`
//!    event. The whole command is one `cartridge_contacts` row (`cartridge
//!    erase`), with all three chip reads journalled against it and one
//!    post-command health sweep.
//!
//! The exit table is [`crate::error::EXIT_ERASE_INCONCLUSIVE`]'s.

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use crate::config::Config;
use crate::error::{Result, TapectlError};
use crate::policy::coverage;
use crate::store::{EraseMode, SmallRead, Store, TapeStore};
use crate::tape::contact::{ContactSlot, Medium, Operation};
use crate::tape::health;
use crate::tape::mam_journal::Hook;
use crate::tape::media_detect::{self, Detected};
use crate::volume::binding;
use crate::volume::format;
use crate::volume::session::{self, ContactOutcome};
use crate::volume::write::{collect_health_best_effort, HealthProbe};

/// What the command line asked for.
#[derive(Debug, Clone, Copy)]
pub struct EraseOptions<'a> {
    pub mode: EraseMode,
    /// `--confirm-label`: the consent for an unattended run.
    pub confirm_label: Option<&'a str>,
    /// `--dry-run`: identify and cross-check, then stop.
    pub dry_run: bool,
    pub block_size: usize,
}

/// What File 0 of the loaded tape is.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MediumKind {
    /// Nothing recorded: end of data at the beginning of the tape.
    Blank,
    /// File 0 is a filemark and nothing else — what the short erase leaves.
    Empty,
    /// File 0 is a tapectl ID thunk.
    Tapectl,
    /// File 0 was read and is not a tapectl ID thunk.
    Foreign,
    /// File 0 could not be read, and the tape is not provably blank.
    Unreadable,
}

impl MediumKind {
    fn describe(self) -> &'static str {
        match self {
            MediumKind::Blank => "blank (end of data at the beginning of the tape)",
            MediumKind::Empty => "File 0 is EMPTY (a filemark at the beginning of the tape)",
            MediumKind::Tapectl => "a volume tapectl wrote",
            MediumKind::Foreign => "not written by tapectl",
            MediumKind::Unreadable => "File 0 unreadable",
        }
    }
}

/// What File 0 says, when it is a tapectl ID thunk. Every field is an
/// absence when missing — a damaged or older File 0 may lack any of them.
#[derive(Debug, Clone, Default, Serialize)]
pub struct File0Report {
    pub label: Option<String>,
    pub uuid: Option<String>,
    pub layout_version: Option<i64>,
    pub tapectl_version: Option<String>,
    pub created_at: Option<String>,
    pub load_count_at_write: Option<i64>,
    pub cartridge_serial: Option<String>,
    pub cartridge_identity_source: Option<String>,
}

/// The seal marker the tape's own File 0 points at, when it parsed.
#[derive(Debug, Clone, Serialize)]
pub struct SealReport {
    pub position: u32,
    pub volume: String,
    pub sealed_at: String,
    pub file_count: i32,
    /// Files and bytes the seal's embedded front-index copy lists.
    pub front_index_files: usize,
    pub front_index_bytes: u64,
}

/// A catalog volume the tape in the drive is, or is mounted on.
#[derive(Debug, Clone, Serialize)]
pub struct CatalogVolume {
    #[serde(skip)]
    pub id: i64,
    pub label: String,
    pub uuid: Option<String>,
    pub status: String,
    pub condition: String,
    /// `"uuid"` (File 0's) or `"chip serial"` (an open mount on the
    /// cartridge the serial names).
    pub matched_by: &'static str,
}

/// The catalog's cartridge row for the chip serial.
#[derive(Debug, Clone, Serialize)]
pub struct CatalogCartridge {
    pub barcode: String,
    pub status: String,
}

/// What the catalog was changed to say (step 6).
#[derive(Debug, Clone, Default, Serialize)]
pub struct Recorded {
    pub cartridge: Option<String>,
    pub registered: bool,
    pub cartridge_status: Option<String>,
    pub volumes_erased: Vec<String>,
}

/// How the command ended, when it ended well.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EraseOutcome {
    /// Blank, or EMPTY under the short erase: nothing was written.
    NothingToErase,
    /// `--dry-run`: identified and cross-checked; nothing was written.
    WouldErase,
    /// Erased, verified and recorded.
    Erased,
}

/// Everything the command found and did — printed as text or `--json`.
#[derive(Debug, Clone, Serialize)]
pub struct EraseReport {
    pub device: String,
    pub mode: &'static str,
    pub chip_serial: Option<String>,
    pub medium: MediumKind,
    pub file0: Option<File0Report>,
    pub sealed: Option<SealReport>,
    /// Why a non-tapectl or unreadable File 0 is what it is.
    pub detail: Option<String>,
    pub catalog_volumes: Vec<CatalogVolume>,
    pub catalog_cartridge: Option<CatalogCartridge>,
    pub notes: Vec<String>,
    /// The word consent is given with: File 0's label, else the chip serial.
    pub confirm_label: Option<String>,
    pub outcome: EraseOutcome,
    /// What File 0 read as after the erase.
    pub medium_after: Option<MediumKind>,
    pub chip_serial_after: Option<String>,
    pub recorded: Option<Recorded>,
    /// A step after a verified erase that did not finish (ADR-0012
    /// 2026-10-07 item 33: the erase stands, the command exits 0).
    pub warnings: Vec<String>,
}

impl EraseReport {
    /// The report as an operator reads it, one line each — what is shown
    /// before consent is asked, and what the command prints.
    pub fn lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        out.push(format!("tape in {}:", self.device));
        out.push(format!(
            "  chip serial:  {}",
            self.chip_serial.as_deref().unwrap_or("(none reported)")
        ));
        out.push(format!("  medium:       {}", self.medium.describe()));
        if let Some(f) = &self.file0 {
            let opt = |v: &Option<String>| v.clone().unwrap_or_else(|| "(absent)".into());
            out.push(format!("  File 0 label: {}", opt(&f.label)));
            out.push(format!("  File 0 uuid:  {}", opt(&f.uuid)));
            out.push(format!(
                "  layout:       {}, written by tapectl {}",
                f.layout_version
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "(absent)".into()),
                opt(&f.tapectl_version)
            ));
            out.push(format!("  created:      {}", opt(&f.created_at)));
            if let Some(n) = f.load_count_at_write {
                out.push(format!("  chip loads when written: {n}"));
            }
            if let Some(s) = &f.cartridge_serial {
                out.push(format!(
                    "  File 0 names cartridge {s} ({})",
                    f.cartridge_identity_source
                        .as_deref()
                        .unwrap_or("identity source unknown")
                ));
            }
        }
        match &self.sealed {
            Some(s) => {
                out.push(format!(
                    "  SEALED:       volume {} at file {}, sealed {}",
                    s.volume, s.position, s.sealed_at
                ));
                out.push(format!(
                    "  front index:  {} files, {} bytes ({} files on tape)",
                    s.front_index_files, s.front_index_bytes, s.file_count
                ));
            }
            None if self.medium == MediumKind::Tapectl => {
                out.push("  sealed:       no seal marker found where File 0 points".into());
            }
            None => {}
        }
        if let Some(d) = &self.detail {
            out.push(format!("  {d}"));
        }
        if self.catalog_volumes.is_empty() && self.catalog_cartridge.is_none() {
            out.push("  catalog:      knows neither this volume nor this cartridge".into());
        }
        if let Some(c) = &self.catalog_cartridge {
            out.push(format!(
                "  catalog:      cartridge {} ({})",
                c.barcode, c.status
            ));
        }
        for v in &self.catalog_volumes {
            out.push(format!(
                "  catalog:      volume {} — {}, condition {} (matched by {})",
                v.label, v.status, v.condition, v.matched_by
            ));
        }
        for n in &self.notes {
            out.push(format!("  note: {n}"));
        }
        out
    }
}

/// `tapectl cartridge erase` on `device`, the production seams: the drive's
/// own chip, the real contact.
pub fn cartridge_erase(
    conn: &Connection,
    config: &Config,
    device: &str,
    opts: EraseOptions<'_>,
) -> Result<EraseReport> {
    let backend = crate::config::resolve_lto_backend(config, Some(device))?;
    let sg = backend.device_sg.clone();
    let mut chip = || media_detect::detect(device, &sg);
    let mut contact = ContactSlot::empty();
    let r = erase_with(conn, config, device, opts, &mut contact, &mut chip);
    contact.finish_result(r)
}

/// [`cartridge_erase`] with the chip and the contact's test seams injected.
/// The store is opened through `TapeStore::open`/`open_read`, which a test
/// replaces with an in-memory tape (`store::injected`).
pub(crate) fn erase_with<'c>(
    conn: &'c Connection,
    config: &Config,
    device: &str,
    opts: EraseOptions<'_>,
    contact: &mut ContactSlot<'c>,
    chip: &mut dyn FnMut() -> Detected,
) -> Result<EraseReport> {
    // STRICT, as every write path (ADR-0010): an erase writes.
    let backend = crate::config::resolve_lto_backend(config, Some(device))?;
    let drive_gen = backend.native_generation()?;
    crate::tape::media_detect::ensure_medium_loaded(device)?;
    let before = chip();

    let injected_identity = contact.injected_drive_identity();
    let probe = HealthProbe {
        log_source: contact.injected_log_source(),
        identity: injected_identity.as_ref(),
    };
    // THE CONTACT BEGINS HERE, at the first chip read, as `volume init`'s.
    let guard = contact.open(
        conn,
        config,
        Operation::CartridgeErase,
        device,
        None,
        Medium::Observed {
            backend,
            mam: &before.mam,
        },
    );
    guard.journal_mam(
        Operation::CartridgeErase,
        Hook::CartridgeErase,
        &before.capture,
    );

    let r = erase_in_contact(conn, device, opts, drive_gen, &before, chip, guard);

    // One post-command sweep, on every outcome the contact saw (ADR-0013's
    // 2026-09-23 evening amendment). Best-effort.
    collect_health_best_effort(
        conn,
        config,
        device,
        None,
        guard.id(),
        health::Reading::Erase,
        Operation::CartridgeErase,
        probe,
    );
    r
}

/// What step 1 read, beside the report.
struct Identified {
    report: EraseReport,
    /// File 0 as read: its bytes, `Oversized`, or unreadable (`None`) — what
    /// step 4 compares the read-write open's own read against.
    file0: File0Read,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum File0Read {
    Bytes(Vec<u8>),
    Oversized,
    Unreadable,
}

fn read_file0(store: &mut dyn Store) -> File0Read {
    match crate::store::read_small(store, 0) {
        Ok(SmallRead::Bytes(b)) => File0Read::Bytes(b),
        Ok(SmallRead::Oversized) => File0Read::Oversized,
        Err(_) => File0Read::Unreadable,
    }
}

/// A key no ID thunk carries, so [`session::contact_report`]'s identity
/// comparison never matches and every File 0 is reported as what it is.
const NO_EXPECTED_IDENTITY: &str = "\u{0}";

/// Step 1, over an open store.
fn identify(
    store: &mut dyn Store,
    device: &str,
    mode: EraseMode,
    serial: Option<&str>,
) -> Identified {
    let (outcome, facts) =
        session::contact_report(store, NO_EXPECTED_IDENTITY, NO_EXPECTED_IDENTITY, None);
    let file0_read = match (&outcome, facts.file0()) {
        (_, Some(b)) => File0Read::Bytes(b.to_vec()),
        (ContactOutcome::IdentityMismatch { found: None }, None) => File0Read::Oversized,
        _ => File0Read::Unreadable,
    };
    let mut report = EraseReport {
        device: device.to_string(),
        mode: mode.as_str(),
        chip_serial: serial.map(str::to_string),
        medium: MediumKind::Unreadable,
        file0: None,
        sealed: None,
        detail: None,
        catalog_volumes: Vec::new(),
        catalog_cartridge: None,
        notes: Vec::new(),
        confirm_label: None,
        outcome: EraseOutcome::NothingToErase,
        medium_after: None,
        chip_serial_after: None,
        recorded: None,
        warnings: Vec::new(),
    };
    match &outcome {
        ContactOutcome::Blank => report.medium = MediumKind::Blank,
        ContactOutcome::EmptyFileZero => report.medium = MediumKind::Empty,
        ContactOutcome::FileZeroUnreadable { error } => {
            report.medium = MediumKind::Unreadable;
            report.detail = Some(format!(
                "File 0 could not be read in tapectl's fixed blocks ({error}) and the tape is \
                 not provably blank: it was written in another block size (not by tapectl), \
                 or the medium or the drive has an error. The first block's size is not \
                 measured."
            ));
        }
        ContactOutcome::IdentityMismatch { found: None } => {
            report.medium = MediumKind::Foreign;
            report.detail = Some(match &file0_read {
                File0Read::Bytes(b) => format!(
                    "File 0 holds {} bytes in tapectl's fixed blocks and is not a tapectl ID \
                     thunk: this tape was not written by tapectl",
                    b.len()
                ),
                _ => format!(
                    "File 0 is larger than {} bytes, so it is no tapectl ID thunk: this tape \
                     was not written by tapectl",
                    crate::store::SMALL_FILE_CAP
                ),
            });
        }
        ContactOutcome::IdentityMismatch { found: Some(_) }
        | ContactOutcome::Matches
        | ContactOutcome::AlreadySealed { .. } => {
            report.medium = MediumKind::Tapectl;
        }
    }
    if let File0Read::Bytes(bytes) = &file0_read {
        if report.medium == MediumKind::Tapectl {
            let text = String::from_utf8_lossy(bytes);
            let ids = binding::file0_facts_from_text(&text);
            let claims = format::parse_id_thunk_claims(&text).ok();
            let prov = format::parse_id_thunk_provenance(&text).unwrap_or_default();
            report.file0 = Some(File0Report {
                label: ids.label,
                uuid: ids.uuid,
                layout_version: claims.as_ref().and_then(|c| c.layout_version),
                tapectl_version: claims.and_then(|c| c.tapectl_version),
                created_at: prov.created_at,
                load_count_at_write: prov.load_count_at_write,
                cartridge_serial: ids.media.as_ref().map(|m| m.cartridge_serial.clone()),
                cartridge_identity_source: ids.media.and_then(|m| m.cartridge_identity_source),
            });
        }
    }
    if let ContactOutcome::AlreadySealed { seal_position } = outcome {
        if let Some(seal) = facts
            .seal_bytes(seal_position)
            .and_then(|b| format::parse_seal_marker(&String::from_utf8_lossy(&b)).ok())
        {
            report.sealed = Some(SealReport {
                position: seal_position,
                volume: seal.volume,
                sealed_at: seal.sealed_at,
                file_count: seal.file_count,
                front_index_files: seal.files.len(),
                front_index_bytes: seal.files.iter().filter_map(|f| f.size_bytes).sum(),
            });
        }
    }
    report.confirm_label = match (&report.file0, serial) {
        (Some(File0Report { label: Some(l), .. }), _) => Some(l.clone()),
        (_, Some(s)) => Some(s.to_string()),
        _ => None,
    };
    Identified {
        report,
        file0: file0_read,
    }
}

/// What step 2 found, with the session locks it holds.
struct CrossChecked {
    cartridge: Option<binding::CartridgeRow>,
    volumes: Vec<CatalogVolume>,
    _locks: Vec<crate::staging::lock::VolumeLock>,
}

/// Step 2: the catalog, by File 0's uuid and by the chip serial. Read-only;
/// every refusal here leaves the tape untouched.
fn cross_check(
    conn: &Connection,
    device: &str,
    report: &mut EraseReport,
    serial: Option<&str>,
) -> Result<CrossChecked> {
    // The cartridge the chip names — by its confirmed serial, else by one
    // operator claim — through the one lookup `volume init` uses; it refuses
    // an ambiguous claim itself.
    let cartridge = binding::lookup_cartridge(conn, serial, None)?.row;
    if let (None, Some(s)) = (&cartridge, serial) {
        // The cartridge will be registered under its serial in step 6; a row
        // already wearing that serial as its BARCODE would collide there,
        // after the tape is erased. Refuse now, with the remedies
        // `register_from_chip` names.
        if binding::select_cartridge(conn, "barcode", s)?.is_some() {
            return Err(TapectlError::Other(format!(
                "cartridge erase refused: the chip reports serial {s}, which no cartridge \
                 carries as its serial, but a cartridge is registered under the BARCODE \
                 \"{s}\". tapectl cannot tell whether it is this cartridge. If it is: \
                 `tapectl cartridge edit {s} --serial {s}`, then erase. If it is not: \
                 `tapectl cartridge relabel {s} <new-barcode>`. Nothing was written."
            )));
        }
    }
    report.catalog_cartridge = cartridge.as_ref().map(|c| CatalogCartridge {
        barcode: c.barcode.clone(),
        status: c.status.clone(),
    });

    let mut volumes: Vec<CatalogVolume> = Vec::new();
    let file0_uuid = report.file0.as_ref().and_then(|f| f.uuid.clone());
    if let Some(uuid) = &file0_uuid {
        let row = conn
            .query_row(
                "SELECT id, label, uuid, status, observed_condition FROM volumes WHERE uuid = ?1",
                params![uuid],
                |r| {
                    Ok(CatalogVolume {
                        id: r.get(0)?,
                        label: r.get(1)?,
                        uuid: r.get(2)?,
                        status: r.get(3)?,
                        condition: r.get(4)?,
                        matched_by: "uuid",
                    })
                },
            )
            .optional()?;
        if let Some(v) = row {
            // Which cartridge the catalog has this volume on. A recorded
            // serial that is not the chip's is two KNOWN identities
            // disagreeing: refused, as every contact refuses it (#193).
            let mounted_on: Option<(String, Option<String>)> = conn
                .query_row(
                    "SELECT c.barcode, c.serial_number FROM cartridge_volumes cv
                     JOIN cartridges c ON c.id = cv.cartridge_id
                     WHERE cv.volume_id = ?1 AND cv.unmounted_at IS NULL",
                    params![v.id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if let (Some((barcode, Some(recorded))), Some(loaded)) = (&mounted_on, serial) {
                if recorded != loaded {
                    return Err(TapectlError::Other(format!(
                        "cartridge erase refused: File 0 names volume \"{}\" (uuid {uuid}), \
                         which the catalog has on cartridge {barcode} (serial {recorded}), \
                         but the chip in {device} reports serial {loaded}. The tape and the \
                         catalog disagree about which cartridge this is, and tapectl will \
                         not erase on a contradiction. `tapectl volume identify --device \
                         {device}` shows the tape; `tapectl cartridge info {barcode}` the \
                         catalog's side. Nothing was written.",
                        v.label
                    )));
                }
            }
            volumes.push(v);
        }
    }
    if let Some(c) = &cartridge {
        let mut stmt = conn.prepare(
            "SELECT v.id, v.label, v.uuid, v.status, v.observed_condition
             FROM cartridge_volumes cv JOIN volumes v ON v.id = cv.volume_id
             WHERE cv.cartridge_id = ?1 AND cv.unmounted_at IS NULL ORDER BY v.id",
        )?;
        let mounted = stmt
            .query_map(params![c.id], |r| {
                Ok(CatalogVolume {
                    id: r.get(0)?,
                    label: r.get(1)?,
                    uuid: r.get(2)?,
                    status: r.get(3)?,
                    condition: r.get(4)?,
                    matched_by: "chip serial",
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for v in mounted {
            if !volumes.iter().any(|known| known.id == v.id) {
                volumes.push(v);
            }
        }
    }
    // A label that coincides with a different volume of this catalog is not
    // the same volume (the uuid is the identity); said, not refused.
    if let Some(f) = &report.file0 {
        if let Some(label) = &f.label {
            let other: Option<Option<String>> = conn
                .query_row(
                    "SELECT uuid FROM volumes WHERE label = ?1",
                    params![label],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(other_uuid) = other {
                if other_uuid != f.uuid {
                    report.notes.push(format!(
                        "this catalog has a volume also labelled \"{label}\", with a different \
                         uuid: it is not the volume on this tape, and erasing this tape does not \
                         touch it"
                    ));
                }
            }
        }
    }
    report.catalog_volumes = volumes.clone();

    for v in &volumes {
        if !coverage::is_given_up(conn, v.id)? {
            let copy = if coverage::counts_as_copy(conn, &v.label)? {
                "it counts as a live copy"
            } else {
                "the catalog has not given it up"
            };
            return Err(TapectlError::Other(format!(
                "cartridge erase refused: the tape in {device} holds volume \"{}\" (matched by \
                 {}), which the catalog holds as {} (condition {}) — {copy}. `volume retire` is \
                 the only way to give a volume up (ADR-0012, 2026-10-12 item 51): run \
                 `tapectl volume retire {}`, then erase. Nothing was written.",
                v.label, v.matched_by, v.status, v.condition, v.label
            )));
        }
    }
    // A live session on one of them refuses here (issue #376): nothing may
    // erase a tape under its writer, or under a verify.
    let mut locks = Vec::new();
    for v in &volumes {
        locks.push(crate::staging::lock::acquire_volume(conn, v.id, &v.label)?);
    }
    Ok(CrossChecked {
        cartridge,
        volumes,
        _locks: locks,
    })
}

fn erase_in_contact(
    conn: &Connection,
    device: &str,
    opts: EraseOptions<'_>,
    drive_gen: crate::media::Generation,
    before: &Detected,
    chip: &mut dyn FnMut() -> Detected,
    guard: &crate::tape::contact::ContactGuard<'_>,
) -> Result<EraseReport> {
    let serial = before.mam.serial.as_deref();

    // ── 1. identify, read-only ──
    let mut store = TapeStore::open_read(device, opts.block_size)?;
    let Identified { mut report, file0 } = identify(&mut store, device, opts.mode, serial);
    drop(store);

    let nothing_to_do = match report.medium {
        MediumKind::Blank => true,
        MediumKind::Empty => opts.mode == EraseMode::Short,
        _ => false,
    };
    if nothing_to_do {
        report.notes.push(match report.medium {
            MediumKind::Blank => "the tape is blank: there is nothing to erase".to_string(),
            _ => "File 0 is already EMPTY, which is what the short erase leaves: nothing to \
                  erase (--long erases the whole medium)"
                .to_string(),
        });
        report.outcome = EraseOutcome::NothingToErase;
        return Ok(report);
    }

    // ── 2. cross-check the catalog ──
    let checked = cross_check(conn, device, &mut report, serial)?;
    // Without the chip serial nothing proves afterwards that the cartridge
    // erased is the one identified, and nothing records it by serial (item
    // 51), so no erase runs. A dry run still reports what the tape is, with
    // no word to confirm it by.
    let sg = &before.capture.device_sg;
    if serial.is_none() {
        report.confirm_label = None;
        report.notes.push(format!(
            "the chip serial could not be read (the MAM read on {sg} failed): \
             `cartridge erase` refuses without it"
        ));
    }
    if opts.dry_run {
        report.outcome = EraseOutcome::WouldErase;
        return Ok(report);
    }
    let Some(serial) = serial else {
        return Err(TapectlError::Other(format!(
            "cartridge erase refused: the chip serial of the cartridge in {device} could not \
             be read (the MAM read on device_sg {sg} failed: sg permissions, sg3-utils not \
             installed, or a device_sg that is not this drive's). Without it nothing proves \
             after the erase that the cartridge erased is the one identified, and nothing \
             records it by serial. `tapectl volume identify --device {device}` shows what \
             the chip reports; `tapectl config check` checks the drive's device_sg. Nothing \
             was written."
        )));
    };

    // ── 3. consent: the word read from the tape, typed ──
    // File 0's label, else the chip serial — always one, the serial now read.
    let word = report
        .confirm_label
        .clone()
        .unwrap_or_else(|| serial.to_string());
    let action = format!(
        "{} ERASE the tape in {device} ({})",
        opts.mode.as_str(),
        report.medium.describe()
    );
    crate::cli::consent::confirm_typed(&action, &report.lines(), &word, opts.confirm_label)?;

    // ── 4. erase ──
    let mut store = TapeStore::open(device, opts.block_size, 0)?;
    // The tape that was identified and consented to is the one still loaded:
    // File 0 reads exactly as it did. Cheap — one block at BOT.
    if read_file0(&mut store) != file0 {
        return Err(TapectlError::Other(format!(
            "cartridge erase refused: File 0 of the tape in {device} no longer reads as it did \
             when it was identified — a different cartridge may have been loaded. Nothing was \
             written. Run the command again to identify what is loaded now."
        )));
    }
    // ... and so is its chip. File 0 alone cannot show a swap between two
    // tapes it cannot read (or two too large to read whole): both compare
    // equal. The drive is held open read-write from here to the erase.
    let at_erase = chip();
    guard.journal_mam(
        Operation::CartridgeErase,
        Hook::CartridgeEraseRecheck,
        &at_erase.capture,
    );
    if at_erase.mam.serial.as_deref() != Some(serial) {
        return Err(TapectlError::Other(format!(
            "cartridge erase refused: the chip in {device} reported serial {serial} when the \
             tape was identified, and {} now, just before the erase — the cartridge loaded \
             may not be the one identified and consented to. Nothing was written. Run the \
             command again to identify what is loaded now.",
            match at_erase.mam.serial.as_deref() {
                Some(s) => format!("serial {s}"),
                None => "no serial (the chip could not be read)".to_string(),
            }
        )));
    }
    let erased = crate::progress::waited(
        || format!("{} erase of the tape in {device}", opts.mode.as_str()),
        || store.erase(opts.mode),
    );
    if let Err(e) = erased {
        return Err(TapectlError::EraseIncomplete(format!(
            "the {} erase of the tape in {device} failed part-way: {e}. The tape may be \
             partly erased, and nothing was recorded. Run `tapectl cartridge erase` again.",
            opts.mode.as_str()
        )));
    }
    drop(store);

    // ── 5. verify: File 0 blank, the chip unchanged ──
    // The tape has been written from here on: no failure may be read as
    // exit 2's "the tape untouched".
    let mut store = TapeStore::open_read(device, opts.block_size).map_err(|e| {
        TapectlError::EraseInconclusive(format!(
            "cartridge erase: the tape in {device} was erased ({}), but reopening it to read \
             it back failed: {e}. The erase is unverified and nothing was recorded. Check the \
             drive, then run `tapectl cartridge erase` again.",
            opts.mode.as_str()
        ))
    })?;
    let (after, _) =
        session::contact_report(&mut store, NO_EXPECTED_IDENTITY, NO_EXPECTED_IDENTITY, None);
    drop(store);
    let after_det = chip();
    guard.journal_mam(
        Operation::CartridgeErase,
        Hook::CartridgeEraseVerify,
        &after_det.capture,
    );
    let medium_after = match &after {
        ContactOutcome::Blank => Some(MediumKind::Blank),
        ContactOutcome::EmptyFileZero => Some(MediumKind::Empty),
        ContactOutcome::FileZeroUnreadable { .. } => Some(MediumKind::Unreadable),
        ContactOutcome::IdentityMismatch { found: None } => Some(MediumKind::Foreign),
        _ => Some(MediumKind::Tapectl),
    };
    report.medium_after = medium_after;
    report.chip_serial_after = after_det.mam.serial.clone();
    verify_after(
        device,
        opts.mode,
        &after,
        Some(serial),
        after_det.mam.serial.as_deref(),
    )?;

    // ── 6. record ──
    let mut recorded = Recorded::default();
    let details = serde_json::json!({
        "mode": opts.mode.as_str(),
        "contact_id": guard.id(),
        "medium_before": report.medium,
        "file0_label": report.file0.as_ref().and_then(|f| f.label.clone()),
        "file0_uuid": report.file0.as_ref().and_then(|f| f.uuid.clone()),
        "volumes_erased": checked.volumes.iter().map(|v| v.label.clone()).collect::<Vec<_>>(),
    })
    .to_string();
    let r = record(
        conn,
        &checked,
        Some(serial),
        before,
        drive_gen,
        &details,
        &mut recorded,
    );
    match r {
        Ok(cartridge_id) => {
            if let Some(id) = cartridge_id {
                guard.record_cartridge(id);
            }
            report.recorded = Some(recorded);
        }
        Err(e) => {
            // ADR-0012 2026-10-07 item 33: the erase stands; say which step
            // did not finish and what finishes it.
            report.warnings.push(format!(
                "warning: the tape is erased and verified, but recording it in the catalog did \
                 not finish: {e}. Nothing was changed in the catalog. To record it: `tapectl \
                 cartridge mark-erased <barcode>` for a registered cartridge (`volume init` \
                 registers an unknown one when it is next written)."
            ));
        }
    }
    report.outcome = EraseOutcome::Erased;
    Ok(report)
}

/// Step 5's verdict: `Ok` only when File 0 reads as the erase leaves it and
/// the chip serial is unchanged.
fn verify_after(
    device: &str,
    mode: EraseMode,
    after: &ContactOutcome,
    serial_before: Option<&str>,
    serial_after: Option<&str>,
) -> Result<()> {
    // Short: EMPTY (or blank); long: blank only.
    let file0_ok = matches!(
        (mode, after),
        (_, ContactOutcome::Blank) | (EraseMode::Short, ContactOutcome::EmptyFileZero)
    );
    let file0_contradicts =
        !file0_ok && !matches!(after, ContactOutcome::FileZeroUnreadable { .. });
    if file0_contradicts {
        let what = match after {
            ContactOutcome::EmptyFileZero => {
                "File 0 is EMPTY, not blank, after a long erase".into()
            }
            other => format!("File 0 still reads as recorded data ({other:?})"),
        };
        return Err(TapectlError::EraseVerifyFailed(format!(
            "cartridge erase: the drive reported the {} erase done, but reading the tape in \
             {device} back says otherwise: {what}. Nothing was recorded. Check the drive (a \
             cleaning cartridge, its log pages), then run the erase again.",
            mode.as_str()
        )));
    }
    if let (Some(b), Some(a)) = (serial_before, serial_after) {
        if a != b {
            return Err(TapectlError::EraseVerifyFailed(format!(
                "cartridge erase: the chip in {device} reported serial {b} before the erase and \
                 {a} after it — the cartridge in the drive is not the one identified. Nothing \
                 was recorded. Load {b} and run `tapectl cartridge erase` again; `tapectl \
                 volume identify` says what the tape now in the drive holds."
            )));
        }
    }
    if let ContactOutcome::FileZeroUnreadable { error } = after {
        return Err(TapectlError::EraseInconclusive(format!(
            "cartridge erase: the {} erase ran, but File 0 of the tape in {device} could not \
             be read back ({error}) and the tape does not prove itself blank. The erase is \
             unverified and nothing was recorded. Run `tapectl cartridge erase` again.",
            mode.as_str()
        )));
    }
    if serial_before.is_some() != serial_after.is_some() {
        return Err(TapectlError::EraseInconclusive(format!(
            "cartridge erase: the chip serial could not be compared ({} before the erase, {} \
             after it), so the cartridge erased cannot be proved to be the one identified. \
             Nothing was recorded. Run `tapectl cartridge erase` again.",
            serial_before.unwrap_or("none"),
            serial_after.unwrap_or("none")
        )));
    }
    Ok(())
}

/// Step 6, in one transaction: the cartridge (registered by its chip serial
/// if the catalog did not know it), its given-up volumes marked `erased`,
/// one `erased` event. Returns the cartridge's id when there is one.
fn record(
    conn: &Connection,
    checked: &CrossChecked,
    serial: Option<&str>,
    before: &Detected,
    drive_gen: crate::media::Generation,
    details: &str,
    recorded: &mut Recorded,
) -> Result<Option<i64>> {
    let tx = conn.unchecked_transaction()?;
    let volume_ids: Vec<i64> = checked
        .volumes
        .iter()
        .filter(|v| v.status != "erased")
        .map(|v| v.id)
        .collect();
    recorded.volumes_erased = checked
        .volumes
        .iter()
        .filter(|v| v.status != "erased")
        .map(|v| v.label.clone())
        .collect();
    let cartridge = match (&checked.cartridge, serial) {
        (Some(row), _) => {
            // A row matched by the operator's claim learns the chip's serial
            // now, once — the same writer `volume init` uses.
            if row.serial_number.is_none() {
                if let Some(s) = serial {
                    binding::record_medium_serial(&tx, row.id, &row.barcode, s)?;
                }
            }
            Some((row.id, row.barcode.clone(), row.status.clone()))
        }
        (None, Some(s)) => {
            let (generation, _) = media_detect::resolve_media(before, None, None, drive_gen)?;
            let id = binding::register_from_chip(&tx, s, generation, &before.mam, "available")?;
            recorded.registered = true;
            Some((id, s.to_string(), "available".to_string()))
        }
        (None, None) => None,
    };
    let id = match cartridge {
        Some((id, barcode, prior)) => {
            // Erasing a medium declared unfit does not make it fit
            // (ADR-0011): `retired_permanent` stays.
            let new_status = if prior == "retired_permanent" {
                prior.clone()
            } else {
                "available".to_string()
            };
            crate::cli::operations::record_cartridge_erased(
                &tx,
                id,
                &barcode,
                &prior,
                &volume_ids,
                &new_status,
                Some(details),
            )?;
            recorded.cartridge = Some(barcode);
            recorded.cartridge_status = Some(new_status);
            Some(id)
        }
        None => {
            for (vid, label) in checked
                .volumes
                .iter()
                .filter(|v| v.status != "erased")
                .map(|v| (v.id, v.label.as_str()))
            {
                tx.execute(
                    "UPDATE volumes SET status = 'erased' WHERE id = ?1",
                    params![vid],
                )?;
                crate::db::events::log_event(
                    &tx,
                    "volume",
                    vid,
                    Some(label),
                    "erased",
                    Some("status"),
                    Some("retired"),
                    Some("erased"),
                    Some(details),
                    None,
                )?;
            }
            None
        }
    };
    tx.commit()?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    //! Every step on the in-memory tape (`FakeTape` under the real
    //! `TapeStore`, installed through `store::injected`), with the chip
    //! answered by the test: every refusal leaves the tape untouched and the
    //! catalog unchanged; an erase is verified by reading back, never by the
    //! drive's word; and the records land by value.
    use super::*;
    use crate::error::ErrorContract;
    use crate::store::injected::InjectedDrive;
    use crate::store::OpenMode;
    use crate::tape::drive_identity::DriveIdentity;
    use crate::tape::fake::{FakeTape, Op};
    use crate::tape::mam::{MamCapture, MamInfo};
    use crate::tape::media_detect::DetectSource;
    use crate::volume::layout;

    const BS: usize = 4096;
    const DEVICE: &str = "/nonexistent/tapectl-erase-nst";
    const SERIAL: &str = "EW7VWMVKF6";
    const UUID: &str = "11111111-2222-3333-4444-555555555555";

    fn config() -> Config {
        let mut config = Config::default();
        config.backends.lto.push(crate::config::LtoBackendConfig {
            name: "lto6".into(),
            device_tape: DEVICE.into(),
            device_sg: "/nonexistent/tapectl-erase-sg".into(),
            generation: "LTO-6".into(),
            capacity_override: None,
            fill_ceiling: 1.0,
            enospc_buffer: "0".into(),
        });
        config
    }

    fn pad(mut b: Vec<u8>) -> Vec<u8> {
        b.resize(b.len().div_ceil(BS) * BS, 0);
        b
    }

    /// A sealed volume `label` of tapectl's (uuid [`UUID`]): File 0, guide,
    /// RESTORE.sh, front index, one slice, the seal marker — from the write
    /// path's own generators.
    fn sealed_tape(label: &str) -> FakeTape {
        let id_thunk = layout::generate_id_thunk_v2(&layout::IdThunkV2Params {
            label,
            uuid: UUID,
            media_type: "LTO-6",
            tapectl_version: "1.0.0",
            nominal_capacity: 2_500_000_000_000,
            mam_capacity: 2_400_000_000_000,
            total_files: 6,
            mam_manufacturer: "TESTCO",
            mam_serial: SERIAL,
            mam_length: 846,
            mam_loads: 7,
            created_at: "2026-10-01T00:00:00Z",
            cartridge_identity_source: Some("mam"),
        })
        .into_bytes();
        let h = "0".repeat(64);
        let mut files = Vec::new();
        for (position, type_label, size) in [
            (0, "id_thunk", Some(id_thunk.len() as u64)),
            (1, "system_guide", Some(10)),
            (2, "restore_sh", Some(10)),
            (3, "front_index", None),
            (4, "data_slice", Some(1000)),
            (5, "seal_marker", None),
        ] {
            files.push(layout::FrontIndexFile {
                position,
                type_label,
                size_bytes: size,
                sha256_encrypted: size.map(|_| h.clone()),
            });
        }
        let fi = layout::generate_front_index(label, &files).into_bytes();
        let seal = layout::generate_seal_marker(label, 6, &h, &files).into_bytes();
        FakeTape::with_files(
            vec![
                pad(id_thunk),
                pad(b"guide".to_vec()),
                pad(b"#!/bin/sh".to_vec()),
                pad(fi),
                pad(vec![7u8; 1000]),
                pad(seal),
            ],
            BS,
        )
    }

    fn detected(serial: Option<&str>) -> Detected {
        Detected {
            generation: crate::media::Generation::parse("LTO-6"),
            code: Some(0x5a),
            source: DetectSource::MamMedium,
            mam: MamInfo {
                serial: serial.map(str::to_string),
                load_count: Some(8),
                manufacturer: Some("TESTCO".into()),
                ..MamInfo::default()
            },
            capture: MamCapture {
                captured_at: "2026-10-12 00:00:00".into(),
                device_sg: "/nonexistent/tapectl-erase-sg".into(),
                stdout: Some(Vec::new()),
                ..MamCapture::default()
            },
        }
    }

    fn opts(mode: EraseMode, confirm: Option<&str>, dry_run: bool) -> EraseOptions<'_> {
        EraseOptions {
            mode,
            confirm_label: confirm,
            dry_run,
            block_size: BS,
        }
    }

    /// Run the erase over `fake`, the chip answering `serials` in turn (the
    /// last one repeated).
    fn run(
        conn: &Connection,
        fake: &FakeTape,
        o: EraseOptions<'_>,
        serials: &[Option<&str>],
    ) -> Result<EraseReport> {
        let _drive = InjectedDrive::install(fake);
        let mut reads: std::collections::VecDeque<Option<&str>> = serials.iter().copied().collect();
        let mut last = serials[0];
        let mut chip = || {
            if let Some(s) = reads.pop_front() {
                last = s;
            }
            detected(last)
        };
        let config = config();
        let mut slot = ContactSlot::empty().with_drive_identity(DriveIdentity {
            serial: Some("HUJ808A5L4".into()),
            vendor: Some("HP".into()),
            model: Some("Ultrium 6-SCSI".into()),
            firmware_rev: Some("35GD".into()),
        });
        let r = erase_with(conn, &config, DEVICE, o, &mut slot, &mut chip);
        slot.finish_result(r)
    }

    fn exit_code(e: TapectlError) -> i32 {
        ErrorContract::Erase.exit_code(&anyhow::Error::from(e))
    }

    fn writes(fake: &FakeTape) -> Vec<Op> {
        fake.ops()
            .into_iter()
            .filter(|op| matches!(op, Op::Write(_) | Op::Erase(_)))
            .collect()
    }

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    fn contact(conn: &Connection) -> (String, Option<String>, Option<i64>) {
        conn.query_row(
            "SELECT operation, outcome, cartridge_id FROM cartridge_contacts
             ORDER BY id DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
    }

    fn add_volume(conn: &Connection, label: &str, uuid: &str, status: &str) -> i64 {
        conn.execute(
            "INSERT INTO volumes (label, uuid, backend_type, backend_name, media_type,
                                  capacity_bytes, status)
             VALUES (?1, ?2, 'lto', 'lto6', 'LTO-6', 2500000000000, ?3)",
            params![label, uuid, status],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn add_cartridge(conn: &Connection, barcode: &str, serial: &str, status: &str) -> i64 {
        conn.execute(
            "INSERT INTO cartridges (barcode, media_type, nominal_capacity, status, serial_number)
             VALUES (?1, 'LTO-6', 2500000000000, ?2, ?3)",
            params![barcode, status, serial],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn mount(conn: &Connection, cartridge: i64, volume: i64) {
        conn.execute(
            "INSERT INTO cartridge_volumes (cartridge_id, volume_id) VALUES (?1, ?2)",
            params![cartridge, volume],
        )
        .unwrap();
    }

    fn status_of(conn: &Connection, table: &str, id: i64) -> String {
        conn.query_row(
            &format!("SELECT status FROM {table} WHERE id = ?1"),
            [id],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// The handoff's case: a sealed volume of a catalog that no longer
    /// exists, on a cartridge this catalog has never seen. Identified,
    /// confirmed by the label File 0 carries, short-erased, read back EMPTY
    /// with the chip unchanged, and recorded: the cartridge registered by
    /// its serial as available, one `erased` event, one `cartridge erase`
    /// contact naming it, all three chip reads journalled against it.
    #[test]
    fn an_unknown_sealed_tape_is_identified_erased_verified_and_recorded() {
        let conn = crate::db::open_memory().unwrap();
        let fake = sealed_tape("L6-0001");
        let report = run(
            &conn,
            &fake,
            opts(EraseMode::Short, Some("L6-0001"), false),
            &[Some(SERIAL)],
        )
        .expect("erased");

        assert_eq!(report.outcome, EraseOutcome::Erased);
        assert_eq!(report.medium, MediumKind::Tapectl);
        let f = report.file0.as_ref().unwrap();
        assert_eq!(f.label.as_deref(), Some("L6-0001"));
        assert_eq!(f.uuid.as_deref(), Some(UUID));
        assert_eq!(f.layout_version, Some(2));
        assert_eq!(f.tapectl_version.as_deref(), Some("1.0.0"));
        assert_eq!(f.created_at.as_deref(), Some("2026-10-01T00:00:00Z"));
        assert_eq!(f.load_count_at_write, Some(7));
        let seal = report.sealed.as_ref().expect("the seal was read");
        assert_eq!((seal.position, seal.volume.as_str()), (5, "L6-0001"));
        assert_eq!(seal.front_index_files, 6);
        assert_eq!(report.confirm_label.as_deref(), Some("L6-0001"));
        assert_eq!(report.medium_after, Some(MediumKind::Empty));
        assert_eq!(report.chip_serial_after.as_deref(), Some(SERIAL));
        assert!(
            report.lines().iter().any(|l| l.contains("SEALED")),
            "{:?}",
            report.lines()
        );

        // The tape: one filemark at BOT and nothing after it.
        assert_eq!(fake.state().files, vec![Vec::<u8>::new()]);
        assert_eq!(writes(&fake), vec![Op::Write(0)]);
        assert_eq!(
            fake.opens(),
            vec![OpenMode::ReadOnly, OpenMode::ReadWrite, OpenMode::ReadOnly],
            "identify read-only, erase read-write, verify read-only"
        );

        // The records.
        let (id, barcode, status): (i64, String, String) = conn
            .query_row(
                "SELECT id, barcode, status FROM cartridges WHERE serial_number = ?1",
                [SERIAL],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .expect("registered by its chip serial");
        assert_eq!((barcode.as_str(), status.as_str()), (SERIAL, "available"));
        let details: String = conn
            .query_row(
                "SELECT details FROM events WHERE entity_type = 'cartridge' AND action = 'erased'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(details.contains("\"mode\":\"short\""), "{details}");
        assert!(details.contains("L6-0001"), "{details}");
        let (op, outcome, cartridge) = contact(&conn);
        assert_eq!(op, "cartridge erase");
        assert_eq!(outcome.as_deref(), Some("ok"));
        assert_eq!(
            cartridge,
            Some(id),
            "the contact names the cartridge it registered"
        );
        let hooks: Vec<String> = conn
            .prepare("SELECT hook FROM mam_journal ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(
            hooks,
            vec![
                "cartridge_erase",
                "cartridge_erase_recheck",
                "cartridge_erase_verify"
            ]
        );
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM mam_journal WHERE contact_id IS NULL"
            ),
            0,
            "every read names the contact"
        );
    }

    /// `--dry-run` identifies and cross-checks, then stops: the report and
    /// the word to confirm with, the tape never opened read-write, nothing
    /// recorded but the contact.
    #[test]
    fn a_dry_run_reports_and_touches_nothing() {
        let conn = crate::db::open_memory().unwrap();
        let fake = sealed_tape("L6-0001");
        let before = fake.state().files.clone();
        let report = run(
            &conn,
            &fake,
            opts(EraseMode::Short, None, true),
            &[Some(SERIAL)],
        )
        .expect("a dry run succeeds");
        assert_eq!(report.outcome, EraseOutcome::WouldErase);
        assert_eq!(report.confirm_label.as_deref(), Some("L6-0001"));
        assert_eq!(fake.state().files, before);
        assert!(writes(&fake).is_empty());
        assert_eq!(fake.opens(), vec![OpenMode::ReadOnly]);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM cartridges"), 0);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM events"), 0);
        assert_eq!(contact(&conn).0, "cartridge erase");
    }

    /// The consent is the label the tape carries: a different one refuses
    /// (exit 2) before the drive is opened read-write.
    #[test]
    fn a_label_that_does_not_match_the_tape_refuses_and_touches_nothing() {
        let conn = crate::db::open_memory().unwrap();
        let fake = sealed_tape("L6-0001");
        let before = fake.state().files.clone();
        let err = run(
            &conn,
            &fake,
            opts(EraseMode::Short, Some("L6-0002"), false),
            &[Some(SERIAL)],
        )
        .unwrap_err();
        assert!(err.to_string().contains("does not match"), "{err}");
        assert_eq!(exit_code(err), 2);
        assert_eq!(fake.state().files, before);
        assert_eq!(fake.opens(), vec![OpenMode::ReadOnly]);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM cartridges"), 0);
        assert_eq!(contact(&conn).1.as_deref(), Some("failed"));
    }

    /// Unattended with no `--confirm-label` (a test's stdin is never a
    /// terminal): refused, naming the flag and the word.
    #[test]
    fn no_confirmation_refuses_and_names_the_label() {
        let conn = crate::db::open_memory().unwrap();
        let fake = sealed_tape("L6-0001");
        let err = run(
            &conn,
            &fake,
            opts(EraseMode::Short, None, false),
            &[Some(SERIAL)],
        )
        .unwrap_err();
        assert!(err.to_string().contains("--confirm-label L6-0001"), "{err}");
        assert_eq!(exit_code(err), 2);
        assert!(writes(&fake).is_empty());
    }

    /// A volume the catalog counts as a live copy — found by File 0's uuid
    /// — is refused, naming `volume retire`, with or without `--dry-run`.
    #[test]
    fn a_live_copy_found_by_uuid_is_refused_naming_volume_retire() {
        let conn = crate::db::open_memory().unwrap();
        add_volume(&conn, "L6-0001", UUID, "sealed");
        let fake = sealed_tape("L6-0001");
        for dry_run in [true, false] {
            let err = run(
                &conn,
                &fake,
                opts(EraseMode::Short, Some("L6-0001"), dry_run),
                &[Some(SERIAL)],
            )
            .unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("tapectl volume retire L6-0001"), "{msg}");
            assert!(msg.contains("live copy"), "{msg}");
            assert_eq!(exit_code(err), 2);
        }
        assert!(writes(&fake).is_empty());
    }

    /// ... and so is one found only through the chip: a volume of a
    /// DIFFERENT uuid mounted on the cartridge the serial names. Not only
    /// copies: an `initialized` volume mid-write, or an `active` import, is
    /// not given up either.
    #[test]
    fn a_volume_the_catalog_still_holds_on_this_cartridge_is_refused() {
        for status in ["sealed", "initialized", "active"] {
            let conn = crate::db::open_memory().unwrap();
            let c = add_cartridge(&conn, "L6-0009", SERIAL, "in_use");
            let v = add_volume(&conn, "L6-0009", "other-uuid", status);
            mount(&conn, c, v);
            let fake = sealed_tape("L6-0001");
            let err = run(
                &conn,
                &fake,
                opts(EraseMode::Short, Some("L6-0001"), false),
                &[Some(SERIAL)],
            )
            .unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("tapectl volume retire L6-0009"),
                "{status}: {msg}"
            );
            assert!(msg.contains("chip serial"), "{status}: {msg}");
            assert!(writes(&fake).is_empty(), "{status}");
        }
    }

    /// A RETIRED volume on a known cartridge: erased, and recorded the way
    /// `cartridge mark-erased` records it — the volume `erased`, the mount
    /// closed, the cartridge `available`.
    #[test]
    fn a_retired_volume_is_erased_and_marked_erased_like_mark_erased() {
        let conn = crate::db::open_memory().unwrap();
        let c = add_cartridge(&conn, "L6-0001", SERIAL, "pending_erase");
        let v = add_volume(&conn, "L6-0001", UUID, "retired");
        mount(&conn, c, v);
        let fake = sealed_tape("L6-0001");
        let report = run(
            &conn,
            &fake,
            opts(EraseMode::Short, Some("L6-0001"), false),
            &[Some(SERIAL)],
        )
        .expect("a retired volume may be erased");
        let r = report.recorded.expect("recorded");
        assert_eq!(r.volumes_erased, vec!["L6-0001".to_string()]);
        assert!(!r.registered);
        assert_eq!(status_of(&conn, "volumes", v), "erased");
        assert_eq!(status_of(&conn, "cartridges", c), "available");
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM cartridge_volumes WHERE unmounted_at IS NULL"
            ),
            0
        );
        assert_eq!(contact(&conn).2, Some(c));
    }

    /// A `retired_permanent` cartridge may be erased — before it leaves the
    /// operator's hands — and stays retired: erasing a medium declared unfit
    /// does not make it fit (ADR-0011).
    #[test]
    fn a_retired_permanent_cartridge_is_erased_and_stays_retired() {
        let conn = crate::db::open_memory().unwrap();
        let c = add_cartridge(&conn, "L6-0001", SERIAL, "retired_permanent");
        let v = add_volume(&conn, "L6-0001", UUID, "retired");
        mount(&conn, c, v);
        let fake = sealed_tape("L6-0001");
        let report = run(
            &conn,
            &fake,
            opts(EraseMode::Long, Some("L6-0001"), false),
            &[Some(SERIAL)],
        )
        .expect("erased");
        assert_eq!(report.medium_after, Some(MediumKind::Blank));
        assert_eq!(status_of(&conn, "cartridges", c), "retired_permanent");
        assert_eq!(status_of(&conn, "volumes", v), "erased");
        assert!(
            fake.state().files.is_empty(),
            "the long erase leaves nothing"
        );
    }

    /// File 0 names a volume the catalog has on ANOTHER cartridge (its
    /// recorded serial is not the chip's): a contradiction, refused.
    #[test]
    fn a_tape_the_catalog_places_on_another_cartridge_is_refused() {
        let conn = crate::db::open_memory().unwrap();
        let c = add_cartridge(&conn, "L6-0007", "OTHERSERIAL", "in_use");
        let v = add_volume(&conn, "L6-0001", UUID, "retired");
        mount(&conn, c, v);
        let fake = sealed_tape("L6-0001");
        let err = run(
            &conn,
            &fake,
            opts(EraseMode::Short, Some("L6-0001"), false),
            &[Some(SERIAL)],
        )
        .unwrap_err();
        assert!(err.to_string().contains("disagree"), "{err}");
        assert_eq!(exit_code(err), 2);
        assert!(writes(&fake).is_empty());
    }

    /// A tape tapectl did not write is reported as such, and is confirmed by
    /// its chip serial — the only name it has.
    #[test]
    fn a_foreign_tape_is_reported_and_confirmed_by_its_chip_serial() {
        let conn = crate::db::open_memory().unwrap();
        let fake = FakeTape::with_files(vec![pad(b"tar archive, not ours".to_vec())], BS);
        let err = run(
            &conn,
            &fake,
            opts(EraseMode::Short, Some("L6-0001"), false),
            &[Some(SERIAL)],
        )
        .unwrap_err();
        assert!(err.to_string().contains(SERIAL), "{err}");
        assert!(writes(&fake).is_empty());

        let report = run(
            &conn,
            &fake,
            opts(EraseMode::Short, Some(SERIAL), false),
            &[Some(SERIAL)],
        )
        .expect("erased");
        assert_eq!(report.medium, MediumKind::Foreign);
        assert!(
            report
                .detail
                .as_deref()
                .unwrap()
                .contains("not written by tapectl"),
            "{:?}",
            report.detail
        );
        assert_eq!(report.outcome, EraseOutcome::Erased);
    }

    /// A tape whose File 0 cannot be read and that is not provably blank
    /// (another block size, a medium error) is reported so, and confirmed
    /// by its chip serial.
    #[test]
    fn an_unreadable_tape_is_reported_and_confirmed_by_its_serial() {
        let conn = crate::db::open_memory().unwrap();
        let fake = FakeTape::with_files(vec![pad(vec![1u8; 10]), pad(vec![2u8; 10])], BS);
        fake.state().unreadable = vec![0];
        let report = run(
            &conn,
            &fake,
            opts(EraseMode::Short, None, true),
            &[Some(SERIAL)],
        )
        .expect("dry run");
        assert_eq!(report.medium, MediumKind::Unreadable);
        assert_eq!(report.confirm_label.as_deref(), Some(SERIAL));
        assert!(report
            .detail
            .as_deref()
            .unwrap()
            .contains("not provably blank"));
    }

    /// A blank tape: reported blank, nothing written, nothing recorded, and
    /// a success.
    #[test]
    fn a_blank_tape_is_a_no_op_success() {
        let conn = crate::db::open_memory().unwrap();
        let fake = FakeTape::with_files(Vec::new(), BS);
        for mode in [EraseMode::Short, EraseMode::Long] {
            let report = run(&conn, &fake, opts(mode, None, false), &[Some(SERIAL)])
                .expect("nothing to erase is a success");
            assert_eq!(report.medium, MediumKind::Blank);
            assert_eq!(report.outcome, EraseOutcome::NothingToErase);
        }
        assert!(writes(&fake).is_empty());
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM cartridges"), 0);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM events"), 0);
    }

    /// An EMPTY File 0 is what the short erase leaves: a no-op under it, and
    /// erased whole under `--long`, reading back blank.
    #[test]
    fn an_empty_file_zero_is_a_no_op_short_and_erased_long() {
        let conn = crate::db::open_memory().unwrap();
        let fake = FakeTape::with_files(vec![Vec::new()], BS);
        let report = run(
            &conn,
            &fake,
            opts(EraseMode::Short, None, false),
            &[Some(SERIAL)],
        )
        .expect("no-op");
        assert_eq!(report.outcome, EraseOutcome::NothingToErase);
        assert!(writes(&fake).is_empty());

        let report = run(
            &conn,
            &fake,
            opts(EraseMode::Long, Some(SERIAL), false),
            &[Some(SERIAL)],
        )
        .expect("long erase");
        assert_eq!(report.outcome, EraseOutcome::Erased);
        assert_eq!(report.medium_after, Some(MediumKind::Blank));
        assert!(fake.state().files.is_empty());
        assert!(writes(&fake).contains(&Op::Erase(0)));
    }

    /// A drive that says done and erases nothing: the read-back finds the
    /// volume still there — exit 6, nothing recorded.
    #[test]
    fn a_drive_that_does_not_erase_fails_the_verify_and_records_nothing() {
        for mode in [EraseMode::Short, EraseMode::Long] {
            let conn = crate::db::open_memory().unwrap();
            let fake = sealed_tape("L6-0001");
            fake.state().writes_vanish = true;
            let err = run(
                &conn,
                &fake,
                opts(mode, Some("L6-0001"), false),
                &[Some(SERIAL)],
            )
            .unwrap_err();
            assert!(err.to_string().contains("says otherwise"), "{err}");
            assert_eq!(exit_code(err), 6, "{mode:?}");
            assert_eq!(count(&conn, "SELECT COUNT(*) FROM cartridges"), 0);
            assert_eq!(count(&conn, "SELECT COUNT(*) FROM events"), 0);
            assert_eq!(contact(&conn).1.as_deref(), Some("failed"));
        }
    }

    /// The chip reports a different serial after the erase: the cartridge in
    /// the drive is not the one identified — exit 6, nothing recorded.
    #[test]
    fn a_chip_serial_that_changes_fails_the_verify() {
        let conn = crate::db::open_memory().unwrap();
        let fake = sealed_tape("L6-0001");
        let err = run(
            &conn,
            &fake,
            opts(EraseMode::Short, Some("L6-0001"), false),
            // Before, at the erase, and after it: the third read differs.
            &[Some(SERIAL), Some(SERIAL), Some("SOMEONEELSE")],
        )
        .unwrap_err();
        assert!(err.to_string().contains("SOMEONEELSE"), "{err}");
        assert_eq!(exit_code(err), 6);
        assert_eq!(writes(&fake), vec![Op::Write(0)], "the erase did run");
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM cartridges"), 0);
    }

    /// The chip unreadable after the erase: no verdict — exit 3, nothing
    /// recorded.
    #[test]
    fn a_chip_unreadable_after_the_erase_is_inconclusive() {
        let conn = crate::db::open_memory().unwrap();
        let fake = sealed_tape("L6-0001");
        let err = run(
            &conn,
            &fake,
            opts(EraseMode::Short, Some("L6-0001"), false),
            &[Some(SERIAL), Some(SERIAL), None],
        )
        .unwrap_err();
        assert_eq!(exit_code(err), 3);
        assert_eq!(writes(&fake), vec![Op::Write(0)], "the erase did run");
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM cartridges"), 0);
    }

    /// A write-protected cartridge is identified (read-only), then refused
    /// at the read-write open, as st refuses it: exit 2, untouched.
    #[test]
    fn a_write_protected_cartridge_is_refused_at_the_erase_untouched() {
        let conn = crate::db::open_memory().unwrap();
        let fake = sealed_tape("L6-0001");
        fake.write_protect();
        let before = fake.state().files.clone();
        let err = run(
            &conn,
            &fake,
            opts(EraseMode::Short, Some("L6-0001"), false),
            &[Some(SERIAL)],
        )
        .unwrap_err();
        assert!(err.to_string().contains("Read-only file system"), "{err}");
        assert_eq!(exit_code(err), 2);
        assert_eq!(fake.state().files, before);
        assert_eq!(fake.opens(), vec![OpenMode::ReadOnly, OpenMode::ReadWrite]);
    }

    /// An erase that fails part-way (a long erase st fails with EIO): exit
    /// 4, nothing recorded.
    #[test]
    fn an_erase_that_fails_part_way_exits_4() {
        let conn = crate::db::open_memory().unwrap();
        let fake = sealed_tape("L6-0001");
        fake.state().erase_fails = true;
        let err = run(
            &conn,
            &fake,
            opts(EraseMode::Long, Some("L6-0001"), false),
            &[Some(SERIAL)],
        )
        .unwrap_err();
        assert!(err.to_string().contains("part-way"), "{err}");
        assert_eq!(exit_code(err), 4);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM cartridges"), 0);
    }

    /// No File 0 label and no chip serial: refused, untouched.
    #[test]
    fn no_label_and_no_serial_refuses() {
        let conn = crate::db::open_memory().unwrap();
        let fake = FakeTape::with_files(vec![pad(b"foreign".to_vec())], BS);
        let err = run(&conn, &fake, opts(EraseMode::Short, None, false), &[None]).unwrap_err();
        assert!(err.to_string().contains("chip serial"), "{err}");
        assert_eq!(exit_code(err), 2);
        assert!(writes(&fake).is_empty());
    }

    /// The chip unreadable BEFORE the erase (sg permissions, no sg3-utils, a
    /// wrong device_sg), on a tape whose File 0 label would otherwise give
    /// consent: nothing could prove afterwards that the cartridge erased is
    /// the one identified, nor record it by serial (item 51). Refused before
    /// consent, exit 2, the drive never opened read-write. A dry run still
    /// identifies the tape, but offers no word to confirm with and says why.
    #[test]
    fn a_chip_unreadable_before_the_erase_refuses_untouched() {
        let conn = crate::db::open_memory().unwrap();
        let fake = sealed_tape("L6-0001");
        let before = fake.state().files.clone();
        let err = run(
            &conn,
            &fake,
            opts(EraseMode::Short, Some("L6-0001"), false),
            &[None],
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("chip serial"), "{msg}");
        assert!(msg.contains("tapectl volume identify"), "{msg}");
        assert!(msg.contains("Nothing was written"), "{msg}");
        assert_eq!(exit_code(err), 2);
        assert!(writes(&fake).is_empty());
        assert_eq!(fake.state().files, before);
        assert_eq!(fake.opens(), vec![OpenMode::ReadOnly]);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM cartridges"), 0);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM events"), 0);

        let report = run(&conn, &fake, opts(EraseMode::Short, None, true), &[None])
            .expect("a dry run still identifies");
        assert_eq!(report.outcome, EraseOutcome::WouldErase);
        assert_eq!(report.confirm_label, None, "no word will be accepted");
        assert!(
            report.notes.iter().any(|n| n.contains("chip serial")),
            "{:?}",
            report.notes
        );
        assert!(writes(&fake).is_empty());
    }

    /// A cartridge swapped while consent was being asked, from one tape
    /// whose File 0 cannot be read to another (any two of those read the
    /// same, so the File 0 recheck cannot tell them apart): the chip, read
    /// again with the drive open read-write, names another cartridge.
    /// Refused before the erase — exit 2, nothing written — and the read is
    /// journalled against the contact.
    #[test]
    fn a_cartridge_swapped_before_the_erase_is_refused_untouched() {
        let conn = crate::db::open_memory().unwrap();
        let fake = FakeTape::with_files(vec![pad(vec![1u8; 10]), pad(vec![2u8; 10])], BS);
        fake.state().unreadable = vec![0];
        let before = fake.state().files.clone();
        let err = run(
            &conn,
            &fake,
            opts(EraseMode::Short, Some(SERIAL), false),
            &[Some(SERIAL), Some("OTHERTAPE")],
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("OTHERTAPE"), "{msg}");
        assert!(msg.contains("Nothing was written"), "{msg}");
        assert_eq!(exit_code(err), 2);
        assert!(writes(&fake).is_empty(), "{:?}", fake.ops());
        assert_eq!(fake.state().files, before);
        assert_eq!(fake.opens(), vec![OpenMode::ReadOnly, OpenMode::ReadWrite]);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM cartridges"), 0);
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM mam_journal
                 WHERE hook = 'cartridge_erase_recheck' AND contact_id IS NOT NULL"
            ),
            1
        );

        // ... and a chip that cannot be read at that point refuses too.
        let err = run(
            &conn,
            &fake,
            opts(EraseMode::Short, Some(SERIAL), false),
            &[Some(SERIAL), None],
        )
        .unwrap_err();
        assert_eq!(exit_code(err), 2);
        assert!(writes(&fake).is_empty());
    }

    /// The tape is written, then the read-only reopen for the read-back
    /// fails (a drive or transport error): exit 3, saying the tape WAS
    /// erased — never 2, whose meaning is "the tape untouched".
    #[test]
    fn a_verify_reopen_that_fails_after_the_erase_is_inconclusive() {
        let conn = crate::db::open_memory().unwrap();
        let fake = sealed_tape("L6-0001");
        fake.state().fail_opens = vec![3];
        let err = run(
            &conn,
            &fake,
            opts(EraseMode::Short, Some("L6-0001"), false),
            &[Some(SERIAL)],
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("was erased"), "{msg}");
        assert!(msg.contains("nothing was recorded"), "{msg}");
        assert_eq!(exit_code(err), 3);
        assert_eq!(writes(&fake), vec![Op::Write(0)], "the erase did run");
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM cartridges"), 0);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM events"), 0);
    }

    /// The registration step's collision is found BEFORE the erase: a row
    /// wearing the chip's serial as its barcode refuses, untouched.
    #[test]
    fn a_barcode_that_is_the_serial_refuses_before_the_erase() {
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO cartridges (barcode, media_type, nominal_capacity, status)
             VALUES (?1, 'LTO-6', 2500000000000, 'available')",
            [SERIAL],
        )
        .unwrap();
        let fake = sealed_tape("L6-0001");
        let err = run(
            &conn,
            &fake,
            opts(EraseMode::Short, Some("L6-0001"), false),
            &[Some(SERIAL)],
        )
        .unwrap_err();
        assert!(err.to_string().contains("BARCODE"), "{err}");
        assert!(writes(&fake).is_empty());
    }
}
