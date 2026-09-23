//! The contact record: one row per time a cartridge was in a drive.
//!
//! ADR-0013 §2 — **the contact row is the spine.** Every hardware observation
//! happens *during* a contact between a drive and a cartridge, and a reading
//! that cannot name its contact cannot be differenced against its own pair.
//! Time-correlation across separate tables works right up until two contacts
//! land in the same second, which is not hypothetical on a machine that runs a
//! thirteen-scenario lifecycle suite.
//!
//! Until migration 020 (issue #296) thirteen code paths put a cartridge in a
//! drive and **none of them wrote a row saying it happened**. The two
//! near-misses are near-misses for structural reasons, not from neglect:
//! `cartridge_volumes` is a BINDING record with `UNIQUE(volume_id)` — one row
//! per volume for the life of that volume — and `cartridges.total_load_count`
//! is the chip's own counter with exactly one writer (`volume init` →
//! `bind_cartridge`), so a tape verified quarterly for five years still reads
//! `total_load_count = 1`.
//!
//! # The seam is a guard, and the guard never reads the medium
//!
//! [`ContactGuard::open`] inserts the row; [`ContactGuard::finish`] closes it;
//! `Drop` without a `finish` leaves `closed_at` NULL — crash-honest by
//! construction, and the same typestate discipline the write session
//! ([`crate::volume::session`]) already uses. Thirteen hand-placed close calls
//! would be thirteen chances to forget one, and the one forgotten is the crash
//! most worth recording.
//!
//! **No MAM read happens in here.** The `st` driver refuses a second
//! concurrent open, which is why every call site already performs its MAM read
//! *before* opening its store; the opener takes the [`MamInfo`] the caller is
//! already holding ([`Medium::Observed`]).
//!
//! **The drive, on the other hand, is asked who it is — at every contact
//! (issue #314).** ADR-0013 §1 says every record takes a drive FK, and until
//! #314 only the paths that also collected sg_logs health ever attached one,
//! so `volume init` and every read path recorded contacts with no drive. The
//! identity read ([`drive_identity::read_identity`]: sysfs, VPD page 0x80, an
//! `sg_inq` fallback on the sg node) is an INQUIRY and reads **no log page**,
//! so it cannot disturb a read-to-clear counter — the health sweep stays the
//! one log-page reader (issue #298). It uses the backend the caller already
//! resolved ([`Medium::Observed`]); with none (`NoBackend`, the DR machine)
//! nothing is asked and `drive_id` stays NULL. Ungated tests supply the
//! identity instead ([`ContactSite::with_drive_identity`],
//! [`ContactSlot::with_drive_identity`]); a test that opens an `Observed`
//! contact WITHOUT one really runs the read, against its fixture's
//! nonexistent device paths, and gets no serial.
//!
//! # Recording a contact can never refuse a command
//!
//! [`ContactGuard::open`] is infallible: a database error yields an inert
//! guard that warns and writes nothing, exactly the discipline
//! `collect_health_best_effort` follows for health collection. Bookkeeping
//! must not become a new way for a tape operation to fail. The positive
//! controls in this module's tests are what prove the guard is *not* inert in
//! the normal case — a test asserting only "no error" could not tell
//! "recorded" from "recorded nothing" (the shape that bit issues #282, #284,
//! #285 and #293).
//!
//! # What is deliberately not here
//!
//! - **No `mam_journal_id`.** ADR-0013 §5: the journal points at the contact,
//!   never the reverse, because one read-path command performs TWO MAM reads
//!   inside ONE contact and one column cannot hold two.
//! - **No drive column.** ADR-0013 §1: every record takes a foreign key to
//!   `drives` and grows none of its own.
//! - **No sweep of `closed_at IS NULL` in `db::open`.** Issue #98 is the scar:
//!   a status-only sweep there once marked a *live* invocation's staging row
//!   `failed` out from under it. `closed_at IS NULL` means "did not close",
//!   and `outcome` stays NULL with it.

use rusqlite::{params, Connection, OptionalExtension};
use tracing::warn;

use crate::config::{Config, LtoBackendConfig};
use crate::error::Result;
use crate::tape::drive_identity::{self, DriveIdentity};
use crate::tape::log_pages::LogSource;
use crate::tape::mam::{MamCapture, MamInfo};
use crate::tape::mam_journal::{Hook, MamReads};

// ── The identity reasons ──────────────────────────────────────────────────
//
// `cartridge_id` may be NULL, but it is NEVER NULL without one of these: a
// NULL with no reason is the data loss this whole suite exists to stop. They
// are `const` rather than inline literals so the tests can assert them BY
// VALUE — an `is_some()` assertion cannot tell one reason from another, and
// these are four different operator situations with four different fixes.

/// No `[[backends.lto]]` exists at all — the rebuilt machine with keys and no
/// `backend add` yet (ADR-0005). No backend means no `device_sg`, so no MAM
/// read was even possible.
pub const REASON_NO_BACKEND_CONFIGURED: &str = "no LTO backend is configured on this host";

/// Backends exist, but none of them claims this device path, so again no MAM
/// read was possible.
///
/// Distinct from [`REASON_NO_BACKEND_CONFIGURED`] because the fixes differ:
/// one operator needs `backend add`, the other has a `--device` that names
/// something their config does not. Issue #313 was the same absence arriving
/// by a different route on the health path, whose lookup was a raw string
/// compare; it now resolves through `config::device_matches` too — note that
/// `config::resolve_device` canonicalizes, so a by-id path *does* match the
/// `/dev/nstN` it resolves to here whenever both exist; this reason therefore
/// states the fact and does not assert a cause.
pub const REASON_DEVICE_MATCHED_NO_BACKEND: &str = "device path matched no configured backend";

/// The MAM was read and carried no medium serial — a blank or unreadable
/// chip, or a medium whose `Medium serial number` attribute is absent (the
/// mhvtl sample in [`crate::tape::mam`] carries none).
pub const REASON_NO_MEDIUM_SERIAL: &str = "MAM read yielded no medium serial";

/// The medium named itself and the catalog does not know that name — a
/// foreign tape under `volume identify`, or a blank in the instant before
/// `volume init` auto-registers it.
///
/// Not a failure: it is the honest state of a contact that happened before
/// the cartridge was identified, which ADR-0013 §2 names as exactly why
/// `cartridge_id` is nullable. [`ContactGuard::record_cartridge`] replaces it
/// the moment the command itself establishes the identity.
///
/// There is deliberately no fallback to `cartridge_volumes` here: attributing
/// a contact from the catalog's belief rather than the chip's own serial
/// would be a guess that reads exactly like an observation (ADR-0012 — a
/// cartridge's identity is its chip serial).
pub const REASON_SERIAL_UNREGISTERED: &str = "medium serial matches no registered cartridge";

/// Every reason, for the tests that assert the set is closed, each member
/// distinct, and each one written by production code.
///
/// Removed (issue #318): `"no MAM read is attempted on this path"`, once
/// `REASON_MAM_NOT_ATTEMPTED`, written by `restore raw-volume` until it began
/// taking a MAM read (#316) and by nothing after. A database written before
/// then may still hold that string; nothing validates `identity_reason`
/// against this list (no CHECK in migration 020, no reader), so such a row
/// stays readable exactly as stored.
pub const IDENTITY_REASONS: &[&str] = &[
    REASON_NO_BACKEND_CONFIGURED,
    REASON_DEVICE_MATCHED_NO_BACKEND,
    REASON_NO_MEDIUM_SERIAL,
    REASON_SERIAL_UNREGISTERED,
];

// ── The outcomes ──────────────────────────────────────────────────────────

/// The contact ended the way its command intended.
pub const OUTCOME_OK: &str = "ok";
/// The contact ended in an error — `detail` carries the error text verbatim.
pub const OUTCOME_FAILED: &str = "failed";

// ── The operation vocabulary ──────────────────────────────────────────────

/// The command that made this contact, verbatim.
///
/// ADR-0013 §4 rules this vocabulary **free TEXT in the schema**: migrations
/// are forward-only and the set grows with every new tape-touching command, so
/// a closed `CHECK` would turn each new command into a schema change. The
/// `health_logs.operation` CHECK (dropped by migration 021) was the argument
/// against itself — it permitted `read` and `clean`, neither of which any
/// code has ever written.
///
/// An enum here gives the typo protection the CHECK was supposed to give, at
/// no migration cost, **and one thing the CHECK never could**: an unused
/// variant is a `dead_code` warning under `clippy --all-targets -D warnings`,
/// so a value with no writer cannot survive the gate. That is precisely how
/// `read` and `clean` should have been caught.
///
/// Note this is a DIFFERENT vocabulary from `health_logs.operation`, which
/// says what *kind* of reading a row is (`write`/`resume`/`verify`,
/// [`crate::tape::health::Reading`]). Neither list may
/// stand in for the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    VolumeInit,
    /// Also the operation recorded for `volume compact-write`,
    /// `collection run` and `quick-archive`, which reach the drive *through*
    /// `volume::write::volume_write` and so share its one contact.
    VolumeWrite,
    VolumeResume,
    VolumeVerify,
    VolumeIdentify,
    VolumeReadSlices,
    VolumeCompactRead,
    /// The interactive three-step `volume compact`. Its step 1 is this
    /// contact; its step 2 makes a SECOND, separate contact through
    /// `volume_write` (the read-only store is closed in between, because the
    /// `st` driver refuses a second concurrent open), which is physically
    /// what happens.
    VolumeCompact,
    /// Also the operation recorded for `restore file`, which reaches the
    /// drive through `restore_unit` and so shares its one contact.
    RestoreUnit,
    RestoreRawVolume,
    CatalogRebuild,
}

impl Operation {
    pub fn as_str(self) -> &'static str {
        match self {
            Operation::VolumeInit => "volume init",
            Operation::VolumeWrite => "volume write",
            Operation::VolumeResume => "volume resume",
            Operation::VolumeVerify => "volume verify",
            Operation::VolumeIdentify => "volume identify",
            Operation::VolumeReadSlices => "volume read-slices",
            Operation::VolumeCompactRead => "volume compact-read",
            Operation::VolumeCompact => "volume compact",
            Operation::RestoreUnit => "restore unit",
            Operation::RestoreRawVolume => "restore raw-volume",
            Operation::CatalogRebuild => "catalog rebuild",
        }
    }

    /// Every operation, for the pinning test.
    pub const ALL: &'static [Operation] = &[
        Operation::VolumeInit,
        Operation::VolumeWrite,
        Operation::VolumeResume,
        Operation::VolumeVerify,
        Operation::VolumeIdentify,
        Operation::VolumeReadSlices,
        Operation::VolumeCompactRead,
        Operation::VolumeCompact,
        Operation::RestoreUnit,
        Operation::RestoreRawVolume,
        Operation::CatalogRebuild,
    ];
}

// ── What the caller already observed ──────────────────────────────────────

/// What the caller learned about the loaded medium **before** it opened its
/// store — never a fresh read.
///
/// The two arms are the two ways a contact can arrive at the guard, and
/// they are not interchangeable: each maps to a different `identity_reason`,
/// which is what makes "why is `cartridge_id` NULL" answerable from the row
/// rather than only from a code comment.
#[derive(Clone, Copy)]
pub enum Medium<'a> {
    /// A backend resolved and the MAM was read through it. `mam` is exactly
    /// what it yielded, including the `load_count` that becomes this
    /// contact's `chip_load_count`.
    Observed {
        backend: &'a LtoBackendConfig,
        mam: &'a MamInfo,
    },
    /// A backend had to resolve before a MAM read was possible, and none did.
    NoBackend,
}

impl<'a> Medium<'a> {
    /// The read paths' shape: `None` from the pre-store MAM read means no
    /// backend resolved, `Some` means it did and this is what it said.
    pub fn from_read(observed: Option<(&'a LtoBackendConfig, &'a MamInfo)>) -> Self {
        match observed {
            Some((backend, mam)) => Medium::Observed { backend, mam },
            None => Medium::NoBackend,
        }
    }

    /// The medium serial this reading carried, if any.
    ///
    /// The serial the corroboration path checks and the serial the contact
    /// records are ONE reading. Two parameters carrying the same observation
    /// is how they drift, and a drifted pair reads as two independent
    /// witnesses agreeing.
    pub fn serial(&self) -> Option<&'a str> {
        match self {
            Medium::Observed { mam, .. } => mam.serial.as_deref(),
            Medium::NoBackend => None,
        }
    }
}

// ── The site, and the slot ────────────────────────────────────────────────

/// Everything a function needs to open its own contact, as ONE parameter.
///
/// It replaces the bare `medium_serial: Option<&str>` the store-seam
/// functions used to take rather than adding four parameters to each of
/// them. That substitution is not merely tidier: the serial corroboration
/// checks IS the serial the contact records ([`Medium::serial`]), so a seam
/// that takes the site cannot be handed one reading for the check and
/// another for the record.
///
/// Taking it at the SEAM rather than at the entry point is what makes every
/// wiring in this issue testable: `volume_verify_with_store`,
/// `restore_unit_from_store`, `read_slices`, `compact_read`,
/// `rebuild_from_store` and their peers all run against a `MemStore`, so the
/// contact row each one writes is asserted by value with no hardware
/// anywhere (issues #282/#284/#285/#293 are all the shape where the artefact
/// meant to be the evidence was never driven).
#[derive(Clone, Copy)]
pub struct ContactSite<'a> {
    config: &'a Config,
    operation: Operation,
    device: &'a str,
    medium: Medium<'a>,
    /// The MAM reads this command took before its store opened, journalled
    /// against this contact the moment it opens (issue #297). `None` for a
    /// site whose caller took no MAM read.
    mam_reads: Option<&'a MamReads<'a>>,
    /// The drive's identity AS IF the drive had said it — the test seam
    /// (issue #314). `None`, the production default, asks the drive itself
    /// ([`drive_identity::read_identity`]).
    drive_identity: Option<&'a DriveIdentity>,
    /// Where the post-command log-page sweep reads from AS IF it were the
    /// drive — the test seam for a read path's health reading (issue #320),
    /// the same shape as `drive_identity`. `None`, the production default,
    /// runs `sg_logs` on the backend's sg node.
    log_source: Option<&'a std::cell::RefCell<dyn LogSource + 'a>>,
}

impl<'a> ContactSite<'a> {
    pub fn new(
        config: &'a Config,
        operation: Operation,
        device: &'a str,
        medium: Medium<'a>,
    ) -> Self {
        ContactSite {
            config,
            operation,
            device,
            medium,
            mam_reads: None,
            drive_identity: None,
            log_source: None,
        }
    }

    /// Answer "which drive is this?" with `identity` instead of asking the
    /// drive — how an ungated test, which has no drive, drives the
    /// attribution [`open`] makes (issue #314). Production never calls it.
    ///
    /// [`open`]: ContactSite::open
    pub fn with_drive_identity(mut self, identity: &'a DriveIdentity) -> Self {
        self.drive_identity = Some(identity);
        self
    }

    /// Answer the post-command log-page sweep (issue #320) from `source`
    /// instead of the drive — how an ungated test drives a read path's
    /// health reading with no drive. Production never calls it.
    pub fn with_log_source(mut self, source: &'a std::cell::RefCell<dyn LogSource + 'a>) -> Self {
        self.log_source = Some(source);
        self
    }

    /// The configuration this contact resolves its drive through.
    pub(crate) fn config(&self) -> &'a Config {
        self.config
    }

    /// The device the command was given, spelled as given.
    pub(crate) fn device(&self) -> &'a str {
        self.device
    }

    /// See [`ContactSite::with_drive_identity`].
    pub(crate) fn injected_drive_identity(&self) -> Option<&'a DriveIdentity> {
        self.drive_identity
    }

    /// See [`ContactSite::with_log_source`].
    pub(crate) fn injected_log_source(&self) -> Option<&'a std::cell::RefCell<dyn LogSource + 'a>> {
        self.log_source
    }

    /// Carry the MAM captures this command is holding, so [`open`] journals
    /// them with the contact's id (ADR-0013 §5: the journal points at the
    /// contact). A read path's contact opens inside its store seam, after
    /// both of its MAM reads — this is how those reads reach it.
    ///
    /// [`open`]: ContactSite::open
    pub fn with_mam_reads(mut self, reads: &'a MamReads<'a>) -> Self {
        self.mam_reads = Some(reads);
        self
    }

    /// The command this contact is being made for. `volume compact` and
    /// `volume compact-read` share one function and differ only here, which
    /// is why the operation travels with the site instead of being a
    /// constant inside each seam.
    pub fn operation(&self) -> Operation {
        self.operation
    }

    /// The medium serial the caller already read — never a second MAM read.
    pub fn medium_serial(&self) -> Option<&'a str> {
        self.medium.serial()
    }

    /// Open the contact this site describes.
    ///
    /// Journals any MAM reads the site carries against the new contact's id
    /// — NULL if the contact's own INSERT failed: the reads happened either
    /// way, and a journal row with no contact beats no journal row.
    pub fn open<'c>(&self, conn: &'c Connection, volume_id: Option<i64>) -> ContactGuard<'c> {
        let guard = ContactGuard::open_with_identity(
            conn,
            self.config,
            self.operation,
            self.device,
            volume_id,
            self.medium,
            self.drive_identity,
        );
        if let Some(reads) = self.mam_reads {
            reads.attach(guard.id());
        }
        guard
    }
}

/// A contact that has not happened yet, and may never.
///
/// The three write paths refuse a dozen times over — a missing volume, a
/// sealed one, an unresolved session, no staged data, no backend — before
/// they read the MAM, and **none of those refusals is a contact**: as far as
/// the command is concerned nothing was ever in the drive. So the guard
/// cannot simply be opened at the top of the function. But a `?` anywhere
/// BELOW the MAM read must still close the contact, or an ordinary refusal
/// records as a crash ([`ContactGuard::finish_result`]'s reason).
///
/// The slot is the only arrangement where both hold: the entry point creates
/// it [`empty`](ContactSlot::empty), hands it down, the inner function
/// [`fill`](ContactSlot::fill)s it at the MAM read, and the entry point
/// closes whatever is in it on the way out — including on the paths that
/// never filled it, where closing nothing is exactly right.
pub struct ContactSlot<'a> {
    guard: Option<ContactGuard<'a>>,
    /// The test seam [`ContactSite::with_drive_identity`] is for the read
    /// paths, here for the three write paths (issue #314). `None`, the
    /// production default, asks the drive itself.
    drive_identity: Option<DriveIdentity>,
}

impl<'a> ContactSlot<'a> {
    pub fn empty() -> Self {
        ContactSlot {
            guard: None,
            drive_identity: None,
        }
    }

    /// See [`ContactSite::with_drive_identity`]. Production never calls it.
    pub fn with_drive_identity(mut self, identity: DriveIdentity) -> Self {
        self.drive_identity = Some(identity);
        self
    }

    /// Record that the contact has begun ([`ContactGuard::open`]), and hand
    /// back the guard so the caller can still
    /// [`record_cartridge`](ContactGuard::record_cartridge) on it.
    pub fn open(
        &mut self,
        conn: &'a Connection,
        config: &Config,
        operation: Operation,
        device: &str,
        volume_id: Option<i64>,
        medium: Medium<'_>,
    ) -> &ContactGuard<'a> {
        let guard = ContactGuard::open_with_identity(
            conn,
            config,
            operation,
            device,
            volume_id,
            medium,
            self.drive_identity.as_ref(),
        );
        self.guard.insert(guard)
    }

    /// Close whatever contact was made, returning the result unchanged.
    /// A slot that was never opened closes nothing.
    pub fn finish_result<T>(self, r: Result<T>) -> Result<T> {
        match self.guard {
            Some(guard) => guard.finish_result(r),
            None => r,
        }
    }
}

impl Default for ContactSlot<'_> {
    fn default() -> Self {
        ContactSlot::empty()
    }
}

// ── The guard ─────────────────────────────────────────────────────────────

/// An open contact. Closes with [`finish`](ContactGuard::finish); dropped
/// without one, it leaves `closed_at` NULL.
///
/// Holds a shared `&Connection`, which composes with everything: every call
/// site takes `&Connection`, and `Connection::unchecked_transaction` — which
/// `volume_init` and the write session use — takes `&self` too. The contact
/// row is inserted outside any of those transactions, so a write that rolls
/// back still leaves the contact recorded. That is correct: the contact
/// happened.
pub struct ContactGuard<'a> {
    conn: &'a Connection,
    /// `None` when the INSERT failed — an inert guard. Bookkeeping must never
    /// refuse a tape command.
    id: Option<i64>,
    finished: bool,
}

impl<'a> ContactGuard<'a> {
    /// Record that a contact has begun. Infallible by design.
    ///
    /// Asks the drive who it is (see [`open_with_identity`]).
    ///
    /// [`open_with_identity`]: ContactGuard::open_with_identity
    pub fn open(
        conn: &'a Connection,
        config: &Config,
        operation: Operation,
        device: &str,
        volume_id: Option<i64>,
        medium: Medium<'_>,
    ) -> ContactGuard<'a> {
        Self::open_with_identity(conn, config, operation, device, volume_id, medium, None)
    }

    /// [`open`](ContactGuard::open), with the drive's identity supplied
    /// rather than read when `given` is `Some` — the test seam.
    #[allow(clippy::too_many_arguments)]
    pub fn open_with_identity(
        conn: &'a Connection,
        config: &Config,
        operation: Operation,
        device: &str,
        volume_id: Option<i64>,
        medium: Medium<'_>,
        given: Option<&DriveIdentity>,
    ) -> ContactGuard<'a> {
        // Which drive (ADR-0013 §1, issue #314) — asked of the backend the
        // caller already resolved, never looked up again here. Only an
        // Observed medium carries one: `NoBackend` is the DR machine with
        // no `device_sg` to ask, and its contact names no drive — unknown,
        // recorded by absence.
        let drive_id = match medium {
            Medium::Observed { backend, .. } => identify_drive(conn, backend, given),
            Medium::NoBackend => None,
        };
        let (backend_name, chip_load_count, cartridge_id, identity_reason) = match medium {
            Medium::Observed { backend, mam } => {
                let (cartridge_id, reason) = match mam.serial.as_deref() {
                    Some(serial) => match cartridge_for_serial(conn, serial) {
                        Some(id) => (Some(id), None),
                        None => (None, Some(REASON_SERIAL_UNREGISTERED)),
                    },
                    None => (None, Some(REASON_NO_MEDIUM_SERIAL)),
                };
                (
                    Some(backend.name.clone()),
                    mam.load_count,
                    cartridge_id,
                    reason,
                )
            }
            Medium::NoBackend => {
                let reason = if config.backends.lto.is_empty() {
                    REASON_NO_BACKEND_CONFIGURED
                } else {
                    REASON_DEVICE_MATCHED_NO_BACKEND
                };
                (None, None, None, Some(reason))
            }
        };

        let inserted = conn.execute(
            "INSERT INTO cartridge_contacts
                 (cartridge_id, volume_id, drive_id, operation, device, backend_name,
                  identity_reason, chip_load_count)
             VALUES (?1, ?2, ?8, ?3, ?4, ?5, ?6, ?7)",
            params![
                cartridge_id,
                volume_id,
                operation.as_str(),
                device,
                backend_name,
                identity_reason,
                chip_load_count,
                drive_id,
            ],
        );
        let id = match inserted {
            Ok(_) => Some(conn.last_insert_rowid()),
            Err(e) => {
                warn!(err = %e, operation = operation.as_str(), "cartridge_contacts insert failed");
                None
            }
        };
        ContactGuard {
            conn,
            id,
            finished: false,
        }
    }

    /// Close the contact.
    pub fn finish(mut self, outcome: &str, detail: Option<&str>) {
        self.finished = true;
        let Some(id) = self.id else {
            return;
        };
        if let Err(e) = self.conn.execute(
            "UPDATE cartridge_contacts
                SET closed_at = datetime('now'), outcome = ?2, detail = ?3
              WHERE id = ?1",
            params![id, outcome, detail],
        ) {
            warn!(err = %e, contact_id = id, "cartridge_contacts close failed");
        }
    }

    /// Close from a `Result`, returning it unchanged.
    ///
    /// The idiom every call site uses, because a refusal that left
    /// `closed_at` NULL would read exactly like a crash — and `?` is a
    /// refusal the code can see, which is not the unwind the `Drop` case
    /// exists for.
    pub fn finish_result<T>(self, r: Result<T>) -> Result<T> {
        match &r {
            Ok(_) => self.finish(OUTCOME_OK, None),
            Err(e) => self.finish(OUTCOME_FAILED, Some(&e.to_string())),
        }
        r
    }

    /// Attach the cartridge this contact turned out to be with, once the
    /// command itself has established it (`volume init`'s binding).
    ///
    /// Clears `identity_reason` in the same statement: the reason recorded at
    /// open said the serial matched no registered cartridge, which was true
    /// *then* and is a lie the moment the auto-registration commits. A row
    /// carrying both an identity and a reason for having none would be a
    /// contradiction the schema has no way to resolve.
    /// [`Self::record_cartridge`] for a command that may have REGISTERED the
    /// cartridge this contact's chip serial names after the contact opened
    /// (issue #335: `catalog rebuild` auto-registers it, or learns the serial
    /// onto an existing row). Looks the observed serial up again; attaches
    /// the cartridge if one now matches, and changes nothing otherwise. Only
    /// a chip-witnessed serial is ever passed here -- the same rule `volume
    /// init` applies before it calls [`Self::record_cartridge`].
    pub fn record_cartridge_by_serial(&self, serial: &str) {
        if let Some(cartridge_id) = cartridge_for_serial(self.conn, serial) {
            self.record_cartridge(cartridge_id);
        }
    }

    pub fn record_cartridge(&self, cartridge_id: i64) {
        let Some(id) = self.id else {
            return;
        };
        if let Err(e) = self.conn.execute(
            "UPDATE cartridge_contacts SET cartridge_id = ?2, identity_reason = NULL WHERE id = ?1",
            params![id, cartridge_id],
        ) {
            warn!(err = %e, contact_id = id, "cartridge_contacts cartridge update failed");
        }
    }

    /// Attach the drive this contact talked to (ADR-0013 §1), once
    /// `drive_identity::upsert` has produced a row for it.
    ///
    /// Every contact with a resolved backend already names its drive from
    /// the moment it opens (issue #314, [`ContactGuard::open`]); this is the
    /// by-guard form of [`record_drive_for`], which the health path uses to
    /// attach the same drive again — the same serial upserts to the same
    /// `drives` row, so that second write agrees with the first. A contact
    /// whose drive gave no serial stays NULL: unknown is recorded by
    /// absence, never guessed.
    pub fn record_drive(&self, drive_id: i64) {
        let Some(id) = self.id else {
            return;
        };
        record_drive_for(self.conn, id, drive_id);
    }

    /// Journal the MAM read this contact was opened from (issue #297) — the
    /// write paths' seam, where the contact opens right after its one read.
    ///
    /// Unlike every other method here this does NOT go quiet on an inert
    /// guard: the read happened whether or not the contact row could be
    /// written, so the journal row is written with `contact_id` NULL.
    /// Best-effort all the same — a journal failure warns and never fails
    /// the tape command.
    pub fn journal_mam(&self, trigger: Operation, hook: Hook, capture: &MamCapture) {
        crate::tape::mam_journal::record(self.conn, self.id, trigger.as_str(), hook, capture);
    }

    /// The `cartridge_contacts` row this guard opened, or `None` for an
    /// inert guard whose INSERT failed.
    ///
    /// Surfaced for the reason `VerifyReport.session_id` was (issue #295):
    /// a reading taken during this contact must be able to NAME it
    /// (`health_logs.contact_id`, ADR-0013 §2), and on the verify path the
    /// guard has already closed by the time sg_logs runs — so the id has to
    /// be carried out of the seam rather than the guard kept alive.
    pub fn id(&self) -> Option<i64> {
        self.id
    }
}

/// Attach a drive to a contact by id — [`ContactGuard::record_drive`] for a
/// contact whose guard has already closed (issue #296).
///
/// `volume verify` closes its contact inside the store-injectable seam and
/// only THEN collects drive health and asks the drive who it is, so there is
/// no guard left to call `record_drive` on. Setting `drive_id` after
/// `closed_at` is correct, not a race: which drive the contact was made
/// with is a fact about the contact however late it is learned, and
/// `finish` never touches this column. Best-effort, like every other
/// contact write: bookkeeping never refuses a tape command.
pub fn record_drive_for(conn: &Connection, contact_id: i64, drive_id: i64) {
    if let Err(e) = conn.execute(
        "UPDATE cartridge_contacts SET drive_id = ?2 WHERE id = ?1",
        params![contact_id, drive_id],
    ) {
        warn!(err = %e, contact_id, "cartridge_contacts drive update failed");
    }
}

impl Drop for ContactGuard<'_> {
    /// Deliberately writes nothing. `closed_at IS NULL` is the record of a
    /// contact that did not close, and nothing anywhere sweeps it into a
    /// guessed outcome (issue #98). The `warn!` exists so the case is visible
    /// in a log at the time, not so that it is repaired.
    fn drop(&mut self) {
        if !self.finished {
            if let Some(id) = self.id {
                warn!(
                    contact_id = id,
                    "contact was not closed; closed_at stays NULL"
                );
            }
        }
    }
}

/// The `drives` row for the drive `backend` names, if it will say who it is.
///
/// [`drive_identity::read_identity`] is sysfs + VPD page 0x80 (with an
/// `sg_inq --page=0x80` fallback): an INQUIRY, and **no log page**. That is
/// the whole reason it may run at every contact — the ADR-0013 hazard is a
/// second read of a read-to-clear page such as 0x2E, and the health sweep
/// stays the only log-page reader (issue #298).
///
/// Best-effort like everything here: no serial is no row and `None` (an
/// unidentifiable drive is unknown, never guessed — [`drive_identity::upsert`]),
/// and an upsert error warns and is `None`. Neither refuses the command.
fn identify_drive(
    conn: &Connection,
    backend: &LtoBackendConfig,
    given: Option<&DriveIdentity>,
) -> Option<i64> {
    let identity = match given {
        Some(identity) => identity.clone(),
        None => drive_identity::read_identity(backend),
    };
    match drive_identity::upsert(conn, &identity) {
        Ok(Some(drive_id)) => Some(drive_id),
        Ok(None) => {
            warn!(
                device = %backend.device_tape,
                "drive identity unavailable (no serial); this contact is recorded without a drive"
            );
            None
        }
        Err(e) => {
            warn!(err = %e, device = %backend.device_tape, "drives upsert failed");
            None
        }
    }
}

/// The cartridge a medium serial names, if the catalog knows it.
///
/// A query error is an absence, not a failure: the contact must be recorded
/// either way, and an unidentified contact is exactly what
/// [`REASON_SERIAL_UNREGISTERED`] is for.
fn cartridge_for_serial(conn: &Connection, serial: &str) -> Option<i64> {
    match conn
        .query_row(
            "SELECT id FROM cartridges WHERE serial_number = ?1",
            params![serial],
            |row| row.get::<_, i64>(0),
        )
        .optional()
    {
        Ok(found) => found,
        Err(e) => {
            warn!(err = %e, "cartridge lookup by medium serial failed");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LtoBackendConfig;

    fn backend() -> LtoBackendConfig {
        // `/dev/null` and `/dev/sg-nonexistent`: the microcosm convention
        // (`config::tests::backend_with`). Nothing in this module opens
        // either — the guard never reads the drive.
        LtoBackendConfig {
            name: "lto0".to_string(),
            device_tape: "/dev/null".to_string(),
            device_sg: "/dev/sg-nonexistent".to_string(),
            generation: "LTO-6".to_string(),
            capacity_override: None,
            usable_capacity_factor: 0.95,
            enospc_buffer: "1GiB".to_string(),
        }
    }

    fn config_with_backend() -> Config {
        let mut config = Config::default();
        config.backends.lto.push(backend());
        config
    }

    fn mam_with_serial(serial: Option<&str>, load_count: Option<i64>) -> MamInfo {
        MamInfo {
            serial: serial.map(str::to_string),
            load_count,
            ..MamInfo::default()
        }
    }

    /// One contact row, as the database holds it: `(cartridge_id, volume_id,
    /// drive_id, operation, device, backend_name, identity_reason,
    /// chip_load_count, closed_at, outcome)`.
    ///
    /// A tuple, not a struct, because every assertion on it is positional
    /// and by value — but named here so `clippy::type_complexity` has
    /// something to hold on to.
    type ContactRow = (
        Option<i64>,
        Option<i64>,
        Option<i64>,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<String>,
        Option<String>,
    );

    fn only_row(conn: &Connection) -> ContactRow {
        conn.query_row(
            "SELECT cartridge_id, volume_id, drive_id, operation, device, backend_name,
                    identity_reason, chip_load_count, closed_at, outcome
               FROM cartridge_contacts",
            [],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                    r.get(9)?,
                ))
            },
        )
        .unwrap()
    }

    fn register_cartridge(conn: &Connection, barcode: &str, serial: &str) -> i64 {
        conn.execute(
            "INSERT INTO cartridges (barcode, media_type, serial_number, nominal_capacity)
             VALUES (?1, 'LTO-6', ?2, 2500000000000)",
            params![barcode, serial],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    /// A real `volumes` row, because `volume_id REFERENCES volumes(id)` is
    /// ENFORCED here (`db::open`'s `configure()` turns foreign keys on), so a
    /// made-up id is not a stand-in for one — the INSERT is rejected, the
    /// guard goes inert, and no row exists for a test to read.
    fn register_volume(conn: &Connection, label: &str) -> i64 {
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                  capacity_bytes, status)
             VALUES (?1, 'lto', 'lto0', 'LTO-6', 2500000000000, 'active')",
            params![label],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn migration_020_applies_from_001_forward_and_fsck_passes() {
        // `open_memory` runs the full ordered migration chain.
        let conn = crate::db::open_memory().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' \
                 AND name = 'cartridge_contacts'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 1,
            "migration 020 must create the cartridge_contacts table"
        );

        let indexes: Vec<String> = {
            let mut stmt = conn
                .prepare(
                    "SELECT name FROM sqlite_master WHERE type = 'index' \
                     AND tbl_name = 'cartridge_contacts' AND name NOT LIKE 'sqlite_%' \
                     ORDER BY name",
                )
                .unwrap();
            let v = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .map(|n| n.unwrap())
                .collect();
            v
        };
        assert_eq!(
            indexes,
            vec![
                "idx_cartridge_contacts_cartridge",
                "idx_cartridge_contacts_volume"
            ]
        );

        let report = crate::cli::operations::db_fsck(&conn, false, false).unwrap();
        assert!(report.integrity_ok, "integrity_check after 020");
        assert!(
            report.issues.is_empty(),
            "db fsck must be clean after 020: {:?}",
            report.issues
        );
    }

    /// The pinning test ADR-0013 §4 requires in place of a CHECK — and the
    /// half a CHECK could never do.
    ///
    /// Two halves, because either alone is the defect the ADR describes:
    ///
    /// 1. The strings are exactly these (typo protection, what the CHECK
    ///    gave).
    /// 2. **Every one of them has a production writer.** The
    ///    `health_logs.operation` CHECK permits `read` and `clean`, which no
    ///    code has ever written — a closed vocabulary already wrong in two of
    ///    its four values. A list asserted only against itself would enshrine
    ///    exactly that lie in the artefact meant to be the evidence, so each
    ///    variant is proved to appear in a source file OTHER than this one.
    #[test]
    fn the_operation_vocabulary_is_what_code_actually_writes() {
        let names: Vec<&str> = Operation::ALL.iter().map(|o| o.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "volume init",
                "volume write",
                "volume resume",
                "volume verify",
                "volume identify",
                "volume read-slices",
                "volume compact-read",
                "volume compact",
                "restore unit",
                "restore raw-volume",
                "catalog rebuild",
            ],
            "cartridge_contacts.operation is the command VERBATIM — a different \
             vocabulary from health_logs.operation, which says what kind of reading \
             a row is"
        );

        // Half two: the writers. Read every source file except this one.
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let this_file = src.join("tape").join("contact.rs");
        let mut corpus = String::new();
        for entry in walkdir::WalkDir::new(&src)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if entry.path() == this_file || entry.path().extension().is_none_or(|x| x != "rs") {
                continue;
            }
            // Issue #332: the production half only, comment lines dropped,
            // exactly as the identity-reason scan below reads it — a test
            // module or a doc cross-reference is not a writer.
            if let Ok(text) = std::fs::read_to_string(entry.path()) {
                let prod = match text.find("#[cfg(test)]\nmod tests") {
                    Some(i) => &text[..i],
                    None => &text[..],
                };
                for line in prod.lines().filter(|l| !l.trim_start().starts_with("//")) {
                    corpus.push_str(line);
                    corpus.push('\n');
                }
            }
        }
        // Positive control on the scan itself: a corpus that read nothing
        // would make every assertion below vacuous (issues #282/#284/#285).
        assert!(
            corpus.contains("ContactSite::new("),
            "positive control: the source scan must actually have read the production call \
             sites (every command opens its contact through ContactSite::new)"
        );
        for op in Operation::ALL {
            let variant = format!("Operation::{op:?}");
            assert!(
                contains_identifier(&corpus, &variant),
                "{variant} has no writer outside tape/contact.rs — a vocabulary value \
                 with no writer is exactly the `read`/`clean` defect ADR-0013 §4 names"
            );
        }
    }

    /// Whether `needle` occurs in `corpus` as a WHOLE identifier path: the
    /// character after it must not continue an identifier. Issue #332:
    /// `Operation::VolumeCompact` is a prefix of `Operation::VolumeCompactRead`,
    /// so a bare `contains` let the latter's writers stand in for the former.
    fn contains_identifier(corpus: &str, needle: &str) -> bool {
        corpus.match_indices(needle).any(|(i, _)| {
            corpus[i + needle.len()..]
                .chars()
                .next()
                .is_none_or(|c| !(c.is_alphanumeric() || c == '_'))
        })
    }

    #[test]
    fn contains_identifier_does_not_accept_a_longer_identifier() {
        let corpus = "x(Operation::VolumeCompactRead);\ny(Operation::VolumeCompact, 1);\n";
        assert!(contains_identifier(corpus, "Operation::VolumeCompact"));
        assert!(!contains_identifier(
            "x(Operation::VolumeCompactRead);",
            "Operation::VolumeCompact"
        ));
        assert!(
            contains_identifier("REASON_A", "REASON_A"),
            "at end of input"
        );
        assert!(!contains_identifier("REASON_AB", "REASON_A"));
    }

    /// ADR-0013 §4's writer rule, applied to the identity reasons (issue
    /// #318): **every entry of [`IDENTITY_REASONS`] has a production
    /// writer.** The operation scan above never covered them, and
    /// `REASON_MAM_NOT_ATTEMPTED` outlived its last writer (`restore
    /// raw-volume` began taking a MAM read, #316) with nothing to say so.
    ///
    /// Reasons are written INSIDE this file (by [`ContactGuard::open`]), so
    /// unlike the operation scan this one reads this file too — its
    /// production half only, with the vocabulary's own declarations (each
    /// `pub const REASON_*` line and the `IDENTITY_REASONS` array) and every
    /// comment line removed, so neither a definition nor a doc
    /// cross-reference can pass for a writer. Every other source file's
    /// production half is read as well.
    #[test]
    fn every_identity_reason_has_a_production_writer() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let this_file = src.join("tape").join("contact.rs");
        let production = |text: &str| -> String {
            match text.find("#[cfg(test)]\nmod tests") {
                Some(i) => text[..i].to_string(),
                None => text.to_string(),
            }
        };

        // The names behind the values: parsed from the declarations, and
        // required to be exactly the list — a declared reason missing from
        // the list, or a listed value with no declaration, fails here.
        let this = std::fs::read_to_string(&this_file).unwrap();
        let declared: Vec<(String, String)> = production(&this)
            .lines()
            .filter_map(|l| {
                let rest = l.trim_start().strip_prefix("pub const REASON_")?;
                let name = format!("REASON_{}", rest.split(':').next()?);
                let value = l.split('"').nth(1)?.to_string();
                Some((name, value))
            })
            .collect();
        let mut declared_values: Vec<&str> = declared.iter().map(|(_, v)| v.as_str()).collect();
        let mut listed: Vec<&str> = IDENTITY_REASONS.to_vec();
        declared_values.sort();
        listed.sort();
        assert_eq!(
            declared_values, listed,
            "IDENTITY_REASONS must be exactly the declared REASON_* constants"
        );

        let mut corpus = String::new();
        for entry in walkdir::WalkDir::new(&src)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if entry.path().extension().is_none_or(|x| x != "rs") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            let mut prod = production(&text);
            if entry.path() == this_file {
                let start = prod
                    .find("pub const IDENTITY_REASONS")
                    .expect("the list is declared here");
                let end = start + prod[start..].find("];").unwrap() + 2;
                prod.replace_range(start..end, "");
            }
            for line in prod.lines() {
                let t = line.trim_start();
                if t.starts_with("//") || t.starts_with("pub const REASON_") {
                    continue;
                }
                corpus.push_str(line);
                corpus.push('\n');
            }
        }
        // Positive controls on the scan itself: it read THIS file's
        // production half (the reasons' writer) and other files' too.
        assert!(
            corpus.contains("fn open_with_identity") && corpus.contains("Medium::from_read("),
            "positive control: the source scan must actually have read the writers"
        );
        assert!(
            !declared.is_empty(),
            "positive control: the declarations were parsed"
        );
        for (name, _) in &declared {
            assert!(
                contains_identifier(&corpus, name),
                "{name} has no production writer — an identity reason no code can \
                 record is the vocabulary-with-no-writer defect ADR-0013 §4 names"
            );
        }
    }

    #[test]
    fn finish_records_the_closing_time_and_the_outcome() {
        let conn = crate::db::open_memory().unwrap();
        let config = config_with_backend();
        let mam = mam_with_serial(None, Some(7));
        let guard = ContactGuard::open(
            &conn,
            &config,
            Operation::VolumeVerify,
            "/dev/null",
            None,
            Medium::Observed {
                backend: &backend(),
                mam: &mam,
            },
        );
        guard.finish(OUTCOME_FAILED, Some("2 slices mismatched"));

        let row = only_row(&conn);
        assert_eq!(row.3, "volume verify");
        assert!(row.8.is_some(), "finish must set closed_at");
        // By VALUE: a test asserting only `is_some()` would pass with the
        // column hardcoded to a single outcome.
        assert_eq!(row.9.as_deref(), Some(OUTCOME_FAILED));
        let detail: Option<String> = conn
            .query_row("SELECT detail FROM cartridge_contacts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(detail.as_deref(), Some("2 slices mismatched"));
    }

    /// The crash case, asserted directly rather than left untested because
    /// "it only happens on a crash".
    ///
    /// Positive control in the same test: the identical guard, finished,
    /// DOES set `closed_at`. Without it this could pass on a guard that never
    /// wrote a row at all.
    #[test]
    fn a_contact_dropped_without_finish_leaves_closed_at_null() {
        let conn = crate::db::open_memory().unwrap();
        let config = config_with_backend();
        let mam = mam_with_serial(None, None);

        {
            let _guard = ContactGuard::open(
                &conn,
                &config,
                Operation::VolumeWrite,
                "/dev/null",
                None,
                Medium::Observed {
                    backend: &backend(),
                    mam: &mam,
                },
            );
            // Dropped here with no `finish` — the interrupted contact.
        }

        let (closed_at, outcome): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT closed_at, outcome FROM cartridge_contacts",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            closed_at, None,
            "a contact that did not close must leave closed_at NULL — nothing sweeps it \
             into a guessed outcome (issue #98)"
        );
        assert_eq!(outcome, None, "outcome stays NULL with closed_at");

        // Positive control: the same guard, finished, closes.
        let guard = ContactGuard::open(
            &conn,
            &config,
            Operation::VolumeWrite,
            "/dev/null",
            None,
            Medium::NoBackend,
        );
        guard.finish(OUTCOME_OK, None);
        let closed: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM cartridge_contacts WHERE closed_at IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(closed, 1, "positive control: finish does close a contact");
    }

    #[test]
    fn a_contact_with_no_configured_backend_names_that_reason() {
        let conn = crate::db::open_memory().unwrap();
        let config = Config::default();
        ContactGuard::open(
            &conn,
            &config,
            Operation::CatalogRebuild,
            "/dev/null",
            None,
            Medium::NoBackend,
        )
        .finish(OUTCOME_OK, None);

        let row = only_row(&conn);
        assert_eq!(row.0, None, "cartridge_id must be NULL");
        assert_eq!(row.6.as_deref(), Some(REASON_NO_BACKEND_CONFIGURED));
        assert_eq!(row.5, None, "no backend resolved, so no backend_name");
    }

    #[test]
    fn a_contact_whose_device_matched_no_backend_names_that_reason() {
        let conn = crate::db::open_memory().unwrap();
        // Backends EXIST — that is the whole discriminator against the test
        // above, which uses the identical `Medium::NoBackend`.
        let config = config_with_backend();
        ContactGuard::open(
            &conn,
            &config,
            Operation::VolumeIdentify,
            "/dev/tape/by-id/scsi-NOSUCH-nst",
            None,
            Medium::NoBackend,
        )
        .finish(OUTCOME_OK, None);

        let row = only_row(&conn);
        assert_eq!(row.0, None);
        assert_eq!(row.6.as_deref(), Some(REASON_DEVICE_MATCHED_NO_BACKEND));
        assert_ne!(
            row.6.as_deref(),
            Some(REASON_NO_BACKEND_CONFIGURED),
            "these are different operator situations with different fixes and must not \
             share a string"
        );
        assert_eq!(row.4, "/dev/tape/by-id/scsi-NOSUCH-nst", "device verbatim");
    }

    #[test]
    fn a_contact_whose_mam_yielded_no_serial_names_that_reason() {
        let conn = crate::db::open_memory().unwrap();
        let config = config_with_backend();
        let mam = mam_with_serial(None, Some(42));
        ContactGuard::open(
            &conn,
            &config,
            Operation::VolumeIdentify,
            "/dev/null",
            None,
            Medium::Observed {
                backend: &backend(),
                mam: &mam,
            },
        )
        .finish(OUTCOME_OK, None);

        let row = only_row(&conn);
        assert_eq!(row.0, None);
        assert_eq!(row.6.as_deref(), Some(REASON_NO_MEDIUM_SERIAL));
        // The chip's counter is recorded even when the chip did not name
        // itself — it is the observation this issue exists to stop losing.
        assert_eq!(row.7, Some(42));
    }

    #[test]
    fn a_contact_whose_serial_is_unregistered_names_that_reason() {
        let conn = crate::db::open_memory().unwrap();
        let config = config_with_backend();
        let mam = mam_with_serial(Some("UNKNOWN99"), Some(3));
        ContactGuard::open(
            &conn,
            &config,
            Operation::VolumeIdentify,
            "/dev/null",
            None,
            Medium::Observed {
                backend: &backend(),
                mam: &mam,
            },
        )
        .finish(OUTCOME_OK, None);

        let row = only_row(&conn);
        assert_eq!(row.0, None);
        assert_eq!(row.6.as_deref(), Some(REASON_SERIAL_UNREGISTERED));
    }

    /// The positive control for every reason test above: with a serial the
    /// catalog knows, `cartridge_id` is SET and `identity_reason` is NULL.
    #[test]
    fn a_contact_whose_serial_matches_a_cartridge_records_it_with_no_reason() {
        let conn = crate::db::open_memory().unwrap();
        let config = config_with_backend();
        let cartridge_id = register_cartridge(&conn, "BC001", "XYZZY_M1");
        let volume_id = register_volume(&conn, "L6-CONTACT");
        let mam = mam_with_serial(Some("XYZZY_M1"), Some(11));
        ContactGuard::open(
            &conn,
            &config,
            Operation::VolumeVerify,
            "/dev/null",
            Some(volume_id),
            Medium::Observed {
                backend: &backend(),
                mam: &mam,
            },
        )
        .finish(OUTCOME_OK, None);

        let row = only_row(&conn);
        assert_eq!(row.0, Some(cartridge_id), "the cartridge the chip named");
        assert_eq!(row.1, Some(volume_id), "the volume the command named");
        assert_eq!(
            row.6, None,
            "an identified contact carries no identity_reason"
        );
        assert_eq!(row.7, Some(11));
    }

    /// A `volume_id` no `volumes` row carries is refused by the foreign key,
    /// and the guard goes INERT — it warns, writes nothing, and the tape
    /// operation it was bookkeeping for carries on unharmed.
    ///
    /// This shape was previously exercised only by ACCIDENT, by the test
    /// above passing `volume_id = Some(0)`; fixing that fixture would have
    /// removed the only coverage of the inert path, so it is asserted here
    /// deliberately instead. Both halves matter: bookkeeping must never
    /// become a new way for a tape command to fail, AND a failed insert must
    /// not leave a half-row behind.
    #[test]
    fn a_contact_whose_volume_id_is_refused_goes_inert_and_refuses_nothing() {
        let conn = crate::db::open_memory().unwrap();
        let config = config_with_backend();
        let mam = mam_with_serial(Some("XYZZY_M1"), Some(11));
        let guard = ContactGuard::open(
            &conn,
            &config,
            Operation::VolumeVerify,
            "/dev/null",
            Some(99999),
            Medium::Observed {
                backend: &backend(),
                mam: &mam,
            },
        );
        // `finish`, `record_cartridge` and `record_drive` are all no-ops on
        // an inert guard rather than panics — the whole point.
        guard.record_drive(4242);
        guard.finish(OUTCOME_OK, None);
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM cartridge_contacts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0, "a refused insert leaves no row at all");

        // Positive control: the identical open with a REAL volume id does
        // write its row, so the zero above is the foreign key and not a
        // guard that never writes anything.
        let volume_id = register_volume(&conn, "L6-REAL");
        ContactGuard::open(
            &conn,
            &config,
            Operation::VolumeVerify,
            "/dev/null",
            Some(volume_id),
            Medium::Observed {
                backend: &backend(),
                mam: &mam,
            },
        )
        .finish(OUTCOME_OK, None);
        assert_eq!(only_row(&conn).1, Some(volume_id));
    }

    /// The discriminating check from the issue: N contacts against ONE
    /// cartridge produce N rows, while `cartridge_volumes` — a binding
    /// record with `UNIQUE(volume_id)` — cannot hold more than one.
    ///
    /// A test that only counted contact rows could not tell this proposal
    /// from a second binding table; this one can.
    #[test]
    fn repeated_contacts_with_one_cartridge_accumulate_rows() {
        let conn = crate::db::open_memory().unwrap();
        let config = config_with_backend();
        let cartridge_id = register_cartridge(&conn, "BC001", "XYZZY_M1");
        for load in 1..=4 {
            let mam = mam_with_serial(Some("XYZZY_M1"), Some(load));
            ContactGuard::open(
                &conn,
                &config,
                Operation::VolumeVerify,
                "/dev/null",
                None,
                Medium::Observed {
                    backend: &backend(),
                    mam: &mam,
                },
            )
            .finish(OUTCOME_OK, None);
        }

        let counts: Vec<i64> = {
            let mut stmt = conn
                .prepare(
                    "SELECT chip_load_count FROM cartridge_contacts \
                     WHERE cartridge_id = ?1 ORDER BY id",
                )
                .unwrap();
            let v = stmt
                .query_map(params![cartridge_id], |r| r.get::<_, i64>(0))
                .unwrap()
                .map(|c| c.unwrap())
                .collect();
            v
        };
        assert_eq!(
            counts,
            vec![1, 2, 3, 4],
            "each contact records the chip's reading AT THAT contact — the wear history \
             `cartridges.total_load_count` freezes at its init-time value"
        );
    }

    #[test]
    fn record_cartridge_replaces_the_reason_with_the_identity() {
        let conn = crate::db::open_memory().unwrap();
        let config = config_with_backend();
        // The blank-tape shape: at open the serial names nothing, and
        // `volume init` auto-registers the cartridge a moment later.
        let mam = mam_with_serial(Some("XYZZY_M1"), Some(1));
        let guard = ContactGuard::open(
            &conn,
            &config,
            Operation::VolumeInit,
            "/dev/null",
            None,
            Medium::Observed {
                backend: &backend(),
                mam: &mam,
            },
        );
        assert_eq!(
            only_row(&conn).6.as_deref(),
            Some(REASON_SERIAL_UNREGISTERED),
            "precondition: at open the catalog did not know this serial"
        );

        let cartridge_id = register_cartridge(&conn, "BC001", "XYZZY_M1");
        guard.record_cartridge(cartridge_id);
        guard.finish(OUTCOME_OK, None);

        let row = only_row(&conn);
        assert_eq!(row.0, Some(cartridge_id));
        assert_eq!(
            row.6, None,
            "a row carrying both an identity and a reason for having none is a \
             contradiction the schema cannot resolve"
        );
    }

    #[test]
    fn record_drive_attaches_the_drive_to_the_contact() {
        let conn = crate::db::open_memory().unwrap();
        let config = config_with_backend();
        conn.execute(
            "INSERT INTO drives (serial, vendor, model) VALUES ('XYZZY_A1', 'IBM', 'ULT3580-TD8')",
            [],
        )
        .unwrap();
        let drive_id = conn.last_insert_rowid();

        // NoBackend: no drive is asked at open (issue #314), so the
        // attachment below is the only one.
        let guard = ContactGuard::open(
            &conn,
            &config,
            Operation::VolumeWrite,
            "/dev/null",
            None,
            Medium::NoBackend,
        );
        assert_eq!(
            only_row(&conn).2,
            None,
            "precondition: a contact with no backend opens with no drive attached"
        );
        guard.record_drive(drive_id);
        guard.finish(OUTCOME_OK, None);

        assert_eq!(only_row(&conn).2, Some(drive_id));
    }

    fn drive(serial: Option<&str>) -> DriveIdentity {
        DriveIdentity {
            serial: serial.map(str::to_string),
            vendor: Some("IBM".to_string()),
            model: Some("ULT3580-TD8".to_string()),
            firmware_rev: Some("0107".to_string()),
        }
    }

    fn contact_drive_serial(conn: &Connection) -> Option<String> {
        conn.query_row(
            "SELECT d.serial FROM cartridge_contacts c LEFT JOIN drives d ON d.id = c.drive_id",
            [],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// Issue #314, at the guard: a contact names its drive the moment it
    /// opens, from the identity read alone — not only when the command
    /// also collects health. Asserted by the drive row's SERIAL, through
    /// the foreign key.
    #[test]
    fn a_contact_opens_naming_the_drive_that_identified_itself() {
        let conn = crate::db::open_memory().unwrap();
        let config = config_with_backend();
        let mam = mam_with_serial(None, None);
        let identity = drive(Some("XYZZY_A1"));
        ContactGuard::open_with_identity(
            &conn,
            &config,
            Operation::VolumeIdentify,
            "/dev/null",
            None,
            Medium::Observed {
                backend: &backend(),
                mam: &mam,
            },
            Some(&identity),
        )
        .finish(OUTCOME_OK, None);
        assert_eq!(contact_drive_serial(&conn).as_deref(), Some("XYZZY_A1"));
    }

    /// No serial ⇒ NULL `drive_id` and no `drives` row: unknown is recorded
    /// by absence, never a row keyed on what the drive DID say.
    #[test]
    fn a_contact_whose_drive_gave_no_serial_names_no_drive() {
        let conn = crate::db::open_memory().unwrap();
        let config = config_with_backend();
        let mam = mam_with_serial(None, None);
        let identity = drive(None);
        ContactGuard::open_with_identity(
            &conn,
            &config,
            Operation::VolumeIdentify,
            "/dev/null",
            None,
            Medium::Observed {
                backend: &backend(),
                mam: &mam,
            },
            Some(&identity),
        )
        .finish(OUTCOME_OK, None);
        assert_eq!(only_row(&conn).2, None);
        let drives: i64 = conn
            .query_row("SELECT COUNT(*) FROM drives", [], |r| r.get(0))
            .unwrap();
        assert_eq!(drives, 0);
    }

    /// No backend (the DR machine) ⇒ nothing to ask, so the identity is not
    /// consulted even when one is on offer. The test two above is the
    /// positive control: the SAME identity, with a backend, is recorded.
    #[test]
    fn a_contact_with_no_backend_names_no_drive() {
        let conn = crate::db::open_memory().unwrap();
        let identity = drive(Some("XYZZY_A1"));
        ContactGuard::open_with_identity(
            &conn,
            &Config::default(),
            Operation::CatalogRebuild,
            "/dev/null",
            None,
            Medium::NoBackend,
            Some(&identity),
        )
        .finish(OUTCOME_OK, None);
        assert_eq!(only_row(&conn).2, None);
        assert_eq!(
            only_row(&conn).6.as_deref(),
            Some(REASON_NO_BACKEND_CONFIGURED)
        );
    }

    /// Issue #297: an inert guard still journals the MAM read it was opened
    /// from — with `contact_id` NULL — because the read happened whether or
    /// not the contact row could be written. Positive control: a live guard
    /// journals against its own id.
    #[test]
    fn journal_mam_writes_even_from_an_inert_guard() {
        let conn = crate::db::open_memory().unwrap();
        let config = config_with_backend();
        let mam = mam_with_serial(None, None);
        let capture = crate::tape::mam::MamCapture {
            captured_at: "2026-09-22 00:00:00".into(),
            device_sg: "/dev/sg-nonexistent".into(),
            tool_argv: vec!["sg_read_attr".into(), "/dev/sg-nonexistent".into()],
            stdout: Some(b"Attribute values:\n  Load count: 2\n".to_vec()),
            ..Default::default()
        };
        let site = |volume_id| {
            ContactGuard::open(
                &conn,
                &config,
                Operation::VolumeWrite,
                "/dev/null",
                volume_id,
                Medium::Observed {
                    backend: &backend(),
                    mam: &mam,
                },
            )
        };

        let inert = site(Some(99_999));
        assert_eq!(
            inert.id(),
            None,
            "precondition: the FK made this guard inert"
        );
        inert.journal_mam(Operation::VolumeWrite, Hook::VolumeWrite, &capture);
        inert.finish(OUTCOME_OK, None);

        let live = site(None);
        let live_id = live.id().expect("a live guard");
        live.journal_mam(Operation::VolumeWrite, Hook::VolumeWrite, &capture);
        live.finish(OUTCOME_OK, None);

        let rows: Vec<(Option<i64>, String, i64)> = {
            let mut stmt = conn
                .prepare("SELECT contact_id, hook, ok FROM mam_journal ORDER BY id")
                .unwrap();
            let v = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .map(|x| x.unwrap())
                .collect();
            v
        };
        assert_eq!(
            rows,
            vec![
                (None, "volume_write".to_string(), 1),
                (Some(live_id), "volume_write".to_string(), 1),
            ]
        );
    }

    /// The #227 lesson: `PRAGMA table_info` reports neither foreign keys nor
    /// CHECK constraints, so enumerating the declarations is not proof they
    /// are enforced. Both halves are asserted.
    #[test]
    fn the_foreign_keys_are_enforced_not_merely_declared() {
        let conn = crate::db::open_memory().unwrap();

        let mut declared: Vec<(String, String)> = {
            let mut stmt = conn
                .prepare("PRAGMA foreign_key_list(cartridge_contacts)")
                .unwrap();
            let v = stmt
                .query_map([], |r| Ok((r.get::<_, String>(2)?, r.get::<_, String>(3)?)))
                .unwrap()
                .map(|x| x.unwrap())
                .collect();
            v
        };
        declared.sort();
        assert_eq!(
            declared,
            vec![
                ("cartridges".to_string(), "cartridge_id".to_string()),
                ("drives".to_string(), "drive_id".to_string()),
                ("volumes".to_string(), "volume_id".to_string()),
            ],
            "three foreign keys, and no fourth — in particular no mam_journal_id \
             (ADR-0013 §5) and no private drive column (§1)"
        );

        // Enforced: `db::open`'s `configure()` turns foreign_keys ON.
        let err = conn.execute(
            "INSERT INTO cartridge_contacts (drive_id, operation, device)
             VALUES (99999, 'volume verify', '/dev/null')",
            [],
        );
        assert!(
            err.is_err(),
            "a bad drive_id must be refused, not merely declared"
        );
        // Positive control: the identical insert with a NULL drive_id is
        // accepted, so the refusal above is the FK and not a broken INSERT.
        conn.execute(
            "INSERT INTO cartridge_contacts (drive_id, operation, device)
             VALUES (NULL, 'volume verify', '/dev/null')",
            [],
        )
        .expect("a NULL drive_id is the documented unknown, and must be accepted");
    }
}
