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
//! | per-file index, `source_path`, sizes | operator envelope's `catalog.db` (#83), via `db::ontape_catalog` | no manifest carries files |
//!
//! `db::ontape_catalog::Generation` is how an old tape's `catalog.db` (no
//! `tenants`, no `key_fingerprints`, no `sha256_plain`; or, before 1.1.0, no
//! file kinds or link targets) is told apart from a current one — probed
//! from the file's actual shape, never from a version number (see that
//! module's doc for why, and `volume-format-v2.md` §1.4).
//!
//! # What it deliberately does not do
//!
//! **It does not verify the tape.** `volume verify` exists and does it
//! properly; bundling a second, weaker verification into a rebuild would
//! produce a command whose success means something different from either.
//! The slice hashes recorded here are the ones the tape claims for itself —
//! run `volume verify` afterwards to make them a checked claim.
//!
//! **It asserts `sealed` only for a tape it read the seal of** (ADR-0012,
//! #458). It does verify that one thing, cheaply: the seal marker at File 0's
//! `[layout] seal_marker` must read, parse, be one this tapectl may read,
//! name this volume and bind the front index beside it
//! ([`format::judge_seal`], the judgement `volume resume`'s adoption of a
//! lost write shares). Without it the tape is an unsealed one — an
//! interrupted or aborted write, whose slices follow the envelopes and may
//! never have been laid down — and the rows it inserts are the catalog's own
//! shape for that: the volume `initialized`, its writes `aborted`, its
//! positions `pending`. Present, so the data is findable, and ineligible, so
//! nothing counts it as a copy. A seal this tapectl must not interpret (an
//! unknown `requires`) refuses the rebuild instead.
//!
//! **A tape that ended early is read by its seal** (#420; `volume-format-v2.md`
//! §1.2 rule 6, §4.2). The seal is located as the last file when the medium
//! ends before File 0's pointer ([`locate_and_judge_seal`]), and a short seal
//! is accepted only on a tape that requires `short-seal`. Its `ended_after`
//! K is recorded on an inserted volume row, a unit with every slice at or
//! before K is `completed`, every other is `cut`, positions after K are
//! `pending`, and nothing after K is read. A full seal rebuilds as before.
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

use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};

use crate::db::files as files_db;
use crate::db::ontape_catalog;
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
    /// Stage sets whose coverage was DEMONSTRATED this run: the supplied key
    /// is the registered escrow key and it decrypted a slice header. Proof,
    /// not a receipt (CTO grilling Q2/Q10).
    pub attested: usize,
    /// True when the supplied key's public half is the escrow recipient this
    /// catalog has registered. Attestation happens only then — an operator
    /// key opening a slice proves operator coverage, which is not the claim.
    pub key_is_escrow: bool,
    /// Rebuilt stage sets on this volume still without a receipt after this
    /// run — the ones that read `escrow: ?` until attested.
    pub unknown_remaining: i64,
    /// Issue #236 finding 1: `attest_escrow` leaves a stage set unattested
    /// on three distinct arms, and printing the SAME sentence for all three
    /// collapses `Unknown` into `Gap` at exactly the moment #137 invented
    /// the distinction for. This counts the one arm that IS actually a Gap
    /// statement: the escrow key was read successfully and demonstrably is
    /// not a recipient of the slice (`age::DecryptError::NoMatchingKeys`) --
    /// permanent, no re-run changes it.
    pub escrow_attest_not_recipient: usize,
    /// A stage set left unattested because its slice header could not be
    /// READ at all (`Store::read_file_head` errored -- a damaged patch of
    /// tape, an I/O error). This is `Unknown`, not `Gap`: the bytes may be
    /// perfectly escrow-covered and merely unreadable at this position, a
    /// different problem with a different remedy (`volume verify`, another
    /// copy, a different block size).
    pub escrow_attest_unreadable: usize,
    /// A stage set left unattested because the bytes were read but did not
    /// parse as an age header (any `age::DecryptError` other than
    /// `NoMatchingKeys`). Also `Unknown`, not `Gap` -- a parse failure says
    /// nothing about whether the escrow key is a recipient.
    pub escrow_attest_unparseable: usize,
    /// Units whose tenant could not be read from any tenant envelope on this
    /// cartridge, and were therefore filed under `--tenant`'s fallback.
    pub units_without_tenant_envelope: Vec<String>,
    /// Tenant envelopes no copy of which could be read (issue #451), one
    /// description each (kind, position, the backup's position, why). The
    /// rebuild went on without them only because the operator envelope's
    /// `catalog.db` names every unit's owner (generation 2 or 3): nothing
    /// else a tenant envelope holds is needed to rebuild the catalog. Empty
    /// when every envelope read. An unreadable OPERATOR envelope, or any
    /// unreadable envelope on a tape whose `catalog.db` cannot name owners,
    /// refuses the rebuild instead.
    pub unreadable_tenant_envelopes: Vec<String>,
    /// Set (to the status found) when this label already had a `volumes` row
    /// and that row's status was not `sealed` — an imported `active` row, for
    /// instance. `None` when the row was freshly inserted here (`sealed` for a
    /// sealed tape, see `insert_all`; for an unsealed one see
    /// [`Self::unsealed_reason`], which is the report of that, not this) or was
    /// already `sealed`.
    ///
    /// This is report-only (issue #158): the rebuild proved the tape
    /// readable and complete, but it never edits a row it merely finds — see
    /// this module's "What it deliberately does not do". Overwriting a
    /// status an operator established on purpose would destroy a fact,
    /// not a mistake, so the fix is to surface the mismatch, not repair it.
    ///
    /// Since ADR-0012's 2026-09-17 amendment (issue #242) this field no
    /// longer catches a verify-quarantined row: that row's status now stays
    /// `sealed` (only `observed_condition` moves), so it passes THIS check.
    /// [`RebuildReport::volume_condition_mismatch`] is the sibling that
    /// catches it — additive, not a replacement, because a write-path
    /// quarantine (never sealed) can still leave a non-`sealed` status here
    /// too, and both facts can in principle be true of the same row at once.
    pub volume_status_mismatch: Option<String>,
    /// Set (to the condition found) when this label already had a `volumes`
    /// row and that row's `observed_condition` was not `ok` — most often a
    /// `quarantined` one a failed `volume verify` produced on purpose,
    /// before the database that recorded why was lost. `None` when the row
    /// was freshly inserted here (always `ok`, see `insert_all`) or was
    /// already `ok`.
    ///
    /// Same report-only discipline as [`RebuildReport::volume_status_mismatch`]
    /// (issue #158): a condition an operator's own verify established is a
    /// fact, not a mistake, so this surfaces it rather than repairing it.
    pub volume_condition_mismatch: Option<String>,

    // --- the tape's own dates (issue #305) ----------------------------------
    //
    // What the medium says about when it was written, in the catalog's
    // `YYYY-MM-DD HH:MM:SS` form — reported whether or not a row was
    // inserted, so an operator rebuilding onto a row they already had can
    // still read them. `None` is "the tape does not say" (a File 0 without
    // the key, or no valid seal marker), never a stand-in date.
    /// File 0's `created_at`: when `volume write` planned the write that made
    /// this tape (before the tape moved; a resumed write keeps the first
    /// attempt's). Written to an INSERTED row's `created_at` and
    /// `first_write`.
    pub written_at: Option<String>,
    /// The seal marker's `sealed_at`. Written to an INSERTED row's
    /// `sealed_at` and `last_write`.
    pub sealed_at: Option<String>,
    /// File 0's `load_count_at_write`: the chip's load count when the write
    /// was planned. Reported only — `cartridges.total_load_count` is the
    /// chip's live figure and a years-old one must not overwrite it.
    pub load_count_at_write: Option<i64>,
    /// Set (to the catalog's own value) when this label already had a row
    /// whose `sealed_at` disagrees with the seal marker's by more than
    /// [`SEALED_AT_TOLERANCE_SECS`]. Report-only, like
    /// [`Self::volume_status_mismatch`] (issue #158): the row keeps its own.
    /// A row with no `sealed_at` (sealed before migration 018) is not a
    /// disagreement — it recorded nothing to disagree with.
    pub volume_sealed_at_mismatch: Option<String>,

    // --- the seal (issue #458) ----------------------------------------------
    //
    // ADR-0012: the rebuild "asserts `sealed` only after reading the seal
    // marker; otherwise the row is present-but-unverified and ineligible".
    /// `None` when the seal marker at File 0's `[layout] seal_marker` read,
    /// parsed, is one this tapectl may read, names this volume and binds the
    /// front index beside it: the tape is sealed, and a row this run inserts
    /// is `sealed` with `completed` writes. `Some(why)` when it did not: the
    /// tape is an unsealed one (an interrupted or aborted write). A row this
    /// run inserts is then `initialized` with `aborted` writes and `pending`
    /// positions — what the catalog holds for a native aborted write, and
    /// not a copy — and `why` names the part that failed. A seal this
    /// tapectl must NOT interpret (an unknown `requires`) is not this: it
    /// refuses the rebuild, as every other reader of the tape does.
    pub unsealed_reason: Option<String>,
    /// The status of the `volumes` row this run FOUND, when it found one.
    /// `None` when the row was inserted here. Report-only: a found row is
    /// never edited (#158), so this is how an unsealed tape under a row the
    /// catalog holds as `sealed` is told apart from one under `initialized`.
    pub found_volume_status: Option<String>,

    // --- where the tape ended (#420, `volume-format-v2.md` §4.2) -------------
    //
    // A 1.6.0 tape may end early, at a short seal; the seal is found by §1.2
    // rule 6 and trusted over the plan.
    /// `Some(K)` when the tape sealed SHORT, its seal at K+1 stating
    /// `ended_after = K`: every planned position after K is not on it. `None`
    /// for a full seal, and for an unsealed tape. An inserted row records it
    /// as `volumes.ended_after`.
    pub ended_after: Option<u64>,
    /// Units, as `<unit> v<N>`, that the early end of the tape cut: a slice
    /// of theirs is planned after [`Self::ended_after`]. Their writes are
    /// `cut` (ADR-0012 item 112): never a copy, positions after the end not
    /// `written`. A tape fact, listed whether or not this run inserted rows.
    pub cut_units: Vec<String>,

    // --- spanning (#478, ADR-0012 items 121, 128 and 130) -------------------
    //
    // `volume-format-v2.md` §4.4: a block that states `may_span` splits three
    // ways, as confirm splits the row (`layout-session.md`, "A write that may
    // span"): `completed` when the tape holds every slice of the set,
    // `range` when it holds some, `cut` when it holds none.
    /// The spannable sets this tape holds as a range: some, not all, of the
    /// set's slices have a `tape_position` at or before `ended_after` (every
    /// planned position, on a full seal). Their writes are `range`: never a
    /// copy on their own (item 127). A tape fact, listed whether or not this
    /// run inserted rows. Empty for every tape with no spannable set.
    pub range_units: Vec<RebuiltRange>,
    /// `<unit> v<N>` of each version whose snapshot this run FOUND `staged`
    /// and made `current`, because with this tape every slice of a stage set
    /// of it is on a volume the catalog records sealed (item 128, the confirm
    /// rule; `layout-session.md` "Promotion and the retire floor"). Only for a
    /// spannable set: a snapshot row this run inserts is inserted with the
    /// status the rule gives it, and a whole unit's found snapshot is left
    /// as it was found, as before 1.7.0. Each also gets the snapshot's
    /// `sealed_current` status-change event, as confirm's promotion does
    /// (ADR-0012 item 144).
    pub versions_promoted: Vec<String>,
    /// The spannable versions on this tape that some slice of is still on
    /// no tape this catalog records, after this run: the to-do list's span
    /// half (item 130, `volume-format-v2.md` §1.5). Each names the missing
    /// slices and the volumes this tape's cross-volume index says hold them.
    /// Such a version stays `staged` and reads 0 copies until they are
    /// rebuilt (item 128).
    pub spans_in_progress: Vec<SpanInProgress>,

    /// Set when this label already had a row whose `ended_after` disagrees
    /// with the seal's (`"none"` when the row records a full seal or no seal
    /// and the tape is short, or the row's K otherwise). Report-only (#158):
    /// a row whose seal this catalog recorded keeps its own.
    pub volume_ended_after_mismatch: Option<String>,
    /// True when this run recorded the tape's `ended_after` on a row it FOUND
    /// with no seal recorded and no ending (`sealed_at` and `ended_after`
    /// NULL; the #360 lost write): not a status (#158 stands) but the fact
    /// `volume verify` needs to find that tape's seal, and the one
    /// layout-session.md has a rebuild record "from the seal it located".
    pub ended_after_recorded: bool,

    // --- cartridge identity (issue #165) -----------------------------------
    //
    // Before this, `insert_all` wrote a `volumes` row and its unit chain and
    // nothing else: no `cartridges` row, no `cartridge_volumes` mount, so a
    // recovered tape could never be re-bound (`cartridge mark-erased`,
    // `cartridge move`/`info` all die with no row to name). ADR-0012's
    // ruling: "`catalog rebuild --from-volume` binds the cartridge it
    // observed (from File 0's identity)". These fields are additive; every
    // key above is unchanged.
    /// The cartridge this volume was bound (or would be bound) to, once
    /// resolved — set even when nothing NEW was written this run (an
    /// idempotent second rebuild still names it), unlike the write counters
    /// below.
    pub cartridge_barcode: Option<String>,
    /// A new `cartridges` row was inserted this run, from File 0's identity
    /// alone (no catalog and no `--cartridge` — rebuild resolves its own
    /// identity claim, never `binding::lookup_cartridge`'s).
    pub cartridge_registered: bool,
    /// A `cartridge_volumes` mount was opened this run. `false` on an
    /// idempotent re-run that found the volume already mounted to the same
    /// cartridge, and `false` when [`Self::unbound_reason`] is set — a
    /// legacy tape whose identity File 0 cannot attest is inserted unbound,
    /// on purpose (do not force one).
    pub cartridge_bound: bool,
    /// The bound cartridge row had no `serial_number` recorded, and this
    /// contact separately observed one and recorded it (`NULL` → value,
    /// once — ADR-0012).
    ///
    /// Set from any of THREE sources. Two are the `operator`-identity ones
    /// (issue #221): `resolve_operator_identity`'s own learn branch, when the
    /// row is still found by BARCODE and only then discovered to lack a
    /// serial; or the pre-transaction `binding::corroborate_volume` contact
    /// check in `rebuild_from_store`, when this same tape's label already
    /// names a bound cartridge and this contact is the first to observe its
    /// serial — which happens BEFORE the transaction, so it can teach the row
    /// its serial and make `resolve_operator_identity` find it BY that serial
    /// instead, skipping its own learn branch entirely.
    ///
    /// The third is on the `mam`-identity path (issue #214):
    /// `resolve_mam_identity` finding the row by the operator's unconfirmed
    /// CLAIM (`operator_serial`) and promoting it. That path used to be
    /// unreachable — the note that once stood here, "the `mam`-identity path
    /// only ever finds a row BY its serial, so it always already has one",
    /// was exactly the asymmetry #214 closed.
    ///
    /// Either way the row changed, so this field must be true.
    pub serial_learned: bool,
    /// Whether THIS contact observed a medium serial at all (a real drive
    /// read returned `Some`, whether or not it matched anything) — distinct
    /// from [`Self::serial_learned`], which is whether that observation was
    /// recorded onto a row. `false` covers two different facts a report
    /// reader needs told apart: no backend was configured for this device at
    /// all (ADR-0010's DR leniency — `catalog rebuild` stays usable with keys
    /// and no `backend add`), or a backend was configured and its drive
    /// simply reported no serial. `catalog rebuild` on a `MemStore` (every
    /// test in this module) also reads `false` — there is no MAM to read.
    pub serial_checked: bool,
    /// Cartridge `barcode` the resolved identity SUPERSEDED: File 0's
    /// `operator`-sourced barcode named one cartridge, but the serial this
    /// contact observed matched a DIFFERENT row — ADR-0012: "the loaded tape
    /// *is* that other cartridge". `None` in every other case, including
    /// every `mam`-identity rebuild (there is no barcode to supersede).
    pub cartridge_barcode_superseded: Option<String>,
    /// Volumes displaced from the bound cartridge this run — the same
    /// ADR-0010 record-don't-refuse displacement `volume init` performs,
    /// reachable here only when a chip serial proves the medium (a `mam`
    /// identity match, or an `operator` identity the observed serial
    /// superseded); an unwitnessed displacement is refused before any write
    /// (ADR-0012, issue #155's rule, rebuild's own version of it).
    ///
    /// Carries the ADR-0004 impact evidence, not just the label (issue
    /// #235). This was `Vec<String>`, and the impacts `mount_and_record`
    /// computes BEFORE it flips the row were dropped in the same statement
    /// that received them — so a unit could reach ZERO copies during an
    /// irreversible step on the disaster-recovery path and nothing said so.
    pub displaced: Vec<DisplacedVolume>,
    /// The bound cartridge's status was (and remains) `retired_permanent`.
    /// Rebuild records the mount as a physical fact — the tape IS on that
    /// cartridge — but never calls anything resembling `refuse_retired`, and
    /// never flips the status back to `in_use`: no amount of contact makes a
    /// medium declared permanently unfit fit again (ADR-0011).
    pub cartridge_retired: bool,
    /// Why the volume was inserted with NO cartridge binding at all: File 0
    /// carries no `[media]` table (pre-ADR-0010), an empty `cartridge_serial`
    /// (a legacy write with nothing to attest), or a non-empty serial with no
    /// `cartridge_identity_source` (written in the ADR-0010-to-#192 window —
    /// UNKNOWN, and never assumed to be `"mam"`; `volume-format-v2.md` §1.1).
    /// `None` whenever a cartridge WAS resolved (bound, registered, or
    /// refused outright).
    pub unbound_reason: Option<String>,
    /// Stage sets that got their dar catalogue back this run (issue #473):
    /// the operator envelope's `catalogs/<uuid8>_v<N>.<k>.dar` files copied
    /// into `<home>/catalogs/<uuid8>/s<id>/` and `stage_sets.catalog_path`
    /// set, exactly as `stage create` leaves them — so a copy written after
    /// `volume read-slices` carries the catalogue in its envelopes again.
    pub catalogues_restored: usize,
    /// `<unit> v<N>` of each rebuilt stage set of this volume that has no
    /// catalogue after this run (issue #473): the operator envelope carried
    /// none for it (a tape written from a set that had none), it could not be
    /// placed, or the catalog's home is unknown. Their next copy has no
    /// catalogue in its envelopes (RESTORE.sh `--list` says so, and `--path`
    /// is refused); `volume write` says so too. Whole-unit restore is
    /// unaffected.
    pub sets_without_catalogue: Vec<String>,

    // --- the cross-volume index (#418 R4, `volume-format-v2.md` §1.5) -------
    //
    // A to-do list, never a source of rows: ADR-0001 has a claim about a tape
    // need contact with that tape, and this tape's index is a claim about
    // other tapes. Nothing below creates or changes a catalog row.
    /// True when the operator envelope carried the index's `index/units.tsv`
    /// (a tape written by 1.3.0 or later). A tape without it is read as it
    /// always was.
    pub had_index: bool,
    /// Why the index could not be used, when it was there and unreadable. The
    /// rebuild itself is not affected: the index is an aid, and a damaged
    /// aid must not stop the recovery it exists to speed.
    pub index_problem: Option<String>,
    /// The volumes the index names (other than this tape) that the catalog
    /// has no row for, after this rebuild: each is a tape to bring in and
    /// rebuild from. Sorted by label.
    pub index_volumes_missing: Vec<IndexedVolume>,
}

/// A volume a tape's cross-volume index names that the catalog lacks, with
/// what the index says it holds: a line of a disaster-recovery to-do list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedVolume {
    pub label: String,
    pub uuid: String,
    /// The chip serial of its cartridge; empty when none was read.
    pub cartridge_serial: String,
    /// Its medium's generation, so the reader knows which drives read it.
    pub generation: String,
    /// `sealed` or `quarantined`, as the index marked it.
    pub state: String,
    /// When it sealed (RFC 3339 UTC), as the index says; empty when unknown.
    pub sealed_at: String,
    /// The units and versions it holds, by tenant and unit.
    pub units: Vec<IndexedUnit>,
}

/// One unit's versions on an [`IndexedVolume`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedUnit {
    pub tenant: String,
    pub unit: String,
    pub versions: Vec<i64>,
}

/// A spannable set this tape holds as a range (#478, ADR-0012 item 121;
/// `volume-format-v2.md` §4.4): the run of its stage set's slices on the tape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebuiltRange {
    pub unit: String,
    pub version: i64,
    /// The lowest slice number of the run (dar's, 1-based).
    pub first_slice: i64,
    /// The highest slice number of the run.
    pub last_slice: i64,
    /// N: the stage set's slices, every one listed in the manifest block.
    pub num_slices: i64,
}

impl RebuiltRange {
    /// `<unit> v<N> (slices F-L of N)`, or `(slice F of N)` for a run of one.
    pub fn describe(&self) -> String {
        format!(
            "{} v{} ({} of {})",
            self.unit,
            self.version,
            describe_slices(&[(self.first_slice, self.last_slice)]),
            self.num_slices
        )
    }
}

/// A spannable version some slices of which are on no tape this catalog
/// records (#478, ADR-0012 item 130): a line of the disaster-recovery to-do
/// list, as [`IndexedVolume`] is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanInProgress {
    pub unit: String,
    pub version: i64,
    /// N: the stage set's slices.
    pub num_slices: i64,
    /// The slices no tape this catalog records holds, in slice order.
    pub missing: Vec<i64>,
    /// The volumes other than this tape that this tape's cross-volume index
    /// (`index/units.tsv`, §1.5) names as holding one of [`Self::missing`],
    /// with the run each holds. Empty when the tape has no index or its
    /// index names none (a tape never names a tape written after it,
    /// ADR-0001).
    pub holders: Vec<SpanHolder>,
}

impl SpanInProgress {
    /// The missing slices as runs: `slices 1-2`, `slices 1-2, 5`.
    pub fn describe_missing(&self) -> String {
        describe_slices(&slice_runs(&self.missing))
    }
}

/// A volume an index names as holding slices of a [`SpanInProgress`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanHolder {
    pub label: String,
    pub uuid: String,
    /// The chip serial of its cartridge; empty when none was read.
    pub cartridge_serial: String,
    /// `sealed` or `quarantined`, as the index marked it.
    pub state: String,
    /// The run it holds, as the index states it (`first_slice`,
    /// `last_slice`); `None` where the index does not say.
    pub first_slice: Option<i64>,
    pub last_slice: Option<i64>,
}

impl SpanHolder {
    /// `L6-0001 (slices 1-2)`, or the label alone when the run is unknown.
    pub fn describe(&self) -> String {
        match (self.first_slice, self.last_slice) {
            (Some(first), Some(last)) => {
                format!("{} ({})", self.label, describe_slices(&[(first, last)]))
            }
            _ => self.label.clone(),
        }
    }
}

/// Consecutive slice numbers as `(first, last)` runs, in order.
pub fn slice_runs(numbers: &[i64]) -> Vec<(i64, i64)> {
    let mut sorted = numbers.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut runs: Vec<(i64, i64)> = Vec::new();
    for n in sorted {
        match runs.last_mut() {
            Some((_, last)) if *last + 1 == n => *last = n,
            _ => runs.push((n, n)),
        }
    }
    runs
}

/// `slice 3`, `slices 1-2`, `slices 1-2, 5`.
fn describe_slices(runs: &[(i64, i64)]) -> String {
    let one = runs.len() == 1 && runs[0].0 == runs[0].1;
    let parts: Vec<String> = runs
        .iter()
        .map(|&(first, last)| {
            if first == last {
                first.to_string()
            } else {
                format!("{first}-{last}")
            }
        })
        .collect();
    format!("slice{} {}", if one { "" } else { "s" }, parts.join(", "))
}

/// One unit with a completed write on a volume this rebuild displaced, and
/// how many ADR-0004-eligible copies it still has ELSEWHERE — the count
/// `cli::operations::retire_impacts` derives, taken BEFORE the displaced
/// volume flipped to `erased`. `other_copies == 0` means this displacement
/// took the unit to zero.
#[derive(Debug, Clone)]
pub struct DisplacedUnit {
    pub unit_name: String,
    pub unit_status: String,
    pub other_copies: i64,
}

/// One volume displaced from the bound cartridge by this rebuild, with the
/// evidence `binding::mount_and_record` computed for it.
///
/// A public mirror of `binding::Displaced` rather than that type itself:
/// `Displaced` and `RetireImpact` are `pub(crate)`, and [`RebuildReport`] is
/// a public type integration tests read, so carrying them directly would put
/// a crate-private type in a public interface.
#[derive(Debug, Clone)]
pub struct DisplacedVolume {
    pub label: String,
    /// Every unit with a completed write on the displaced volume, in
    /// `retire_impacts` order (by unit name). Empty when the displaced
    /// volume held no completed write — a displacement that costs nothing.
    pub units: Vec<DisplacedUnit>,
    /// The warning, already rendered — the SAME lines `volume init` prints,
    /// from `binding::render_displacement` (issue #235). Rendered once, in
    /// the library, so the CLI only has to choose a stream and an indent.
    pub warning: Vec<String>,
}

impl DisplacedVolume {
    /// The units this displacement left with no eligible copy anywhere —
    /// ADR-0004 Tier 1's "the one fact that matters at the irreversible
    /// moment".
    pub fn zero_copy_units(&self) -> Vec<&str> {
        self.units
            .iter()
            .filter(|u| u.other_copies == 0)
            .map(|u| u.unit_name.as_str())
            .collect()
    }
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
            && self.attested == 0
            && self.catalogues_restored == 0
            && !self.cartridge_registered
            && !self.cartridge_bound
            && !self.serial_learned
            && !self.ended_after_recorded
            && self.versions_promoted.is_empty()
            && self.displaced.is_empty()
    }
}

/// Read a sealed volume and insert whatever catalog rows are missing.
///
/// `keys` are the `--key` files, read by the caller before the drive was
/// opened (ADR-0012 2026-10-19 item 107): one operator or the escrow secret
/// key opens the operator envelope, and several may be given (the escrow key
/// among them attests, as with one). A tenant key cannot open the operator
/// envelope (its recipients are operator + escrow only) and is refused with
/// a pointer at `RESTORE.sh`, which is the heir path and does not need a
/// catalog at all. The keys are used for this call only: nothing is copied
/// into the home or registered.
#[allow(clippy::too_many_arguments)]
pub fn rebuild_from_volume(
    conn: &Connection,
    // Only for resolving the drive's SCSI generic node, so the loaded
    // medium's serial can be read and corroborated (issue #193). Optional by
    // construction: no matching backend means no serial, which is an
    // absence, which proceeds — the DR machine, exactly.
    config: &crate::config::Config,
    device: &str,
    block_size: usize,
    keys: &[crate::crypto::keys::KeyFile],
    expect_label: Option<&str>,
    fallback_tenant: &str,
    backend_name: Option<&str>,
    scratch: &Path,
    reads: &crate::tape::mam_journal::MamReads<'_>,
) -> Result<RebuildReport> {
    let identities = crate::crypto::keys::KeyFile::all_identities(keys);
    // Before the store open: reading the MAM opens the device read-only and
    // drops the fd, and the st driver refuses a second concurrent open.
    // LENIENT (ADR-0010): rebuild is THE disaster-recovery read path, so a
    // machine with keys and no `backend add` yields `None` — an absence,
    // which corroborates against nothing and proceeds.
    let observed = crate::volume::binding::loaded_medium(config, device, reads);
    let mut store = TapeStore::open_read(device, block_size)?;
    rebuild_from_store(
        conn,
        &mut store,
        &identities,
        expect_label,
        fallback_tenant,
        backend_name,
        scratch,
        device,
        // The REAL reading, not the `MamInfo::default()` the rebuild hands
        // `mount_and_record` further down: that default exists so a
        // `total_load_count` COALESCE is a no-op rather than asserting a
        // reading nobody took, and reusing it here would record the same
        // invention as an observation.
        crate::tape::contact::ContactSite::new(
            config,
            crate::tape::contact::Operation::CatalogRebuild,
            device,
            crate::tape::contact::Medium::from_read(observed.as_ref().map(|(b, m)| (*b, m))),
        )
        .with_mam_reads(reads),
    )
    .map_err(|e| name_keys_given(e, keys))
}

/// The opening of the refusal [`tenant_key_refusal_message`] makes: the
/// words [`name_keys_given`] swaps for the files the operator gave.
const TENANT_KEY_REFUSAL_LEAD: &str = "this key cannot open the operator envelope, so it is \
     neither an operator key nor the escrow key.";

/// The wrong-key refusal, naming the `--key` files that were given (item
/// 107): the same refusal, its first sentence about the files rather than
/// "this key". Any other error passes through untouched.
fn name_keys_given(e: TapectlError, keys: &[crate::crypto::keys::KeyFile]) -> TapectlError {
    let message = e.to_string();
    if keys.is_empty() || !message.contains(TENANT_KEY_REFUSAL_LEAD) {
        return e;
    }
    let named = crate::crypto::keys::KeyFile::describe(keys);
    let lead = if keys.len() == 1 {
        format!(
            "{named} cannot open the operator envelope, so it is neither an operator key nor \
             the escrow key."
        )
    } else {
        format!(
            "none of the key files given ({named}) can open the operator envelope, so none is \
             an operator key or the escrow key."
        )
    };
    TapectlError::Other(message.replacen(TENANT_KEY_REFUSAL_LEAD, &lead, 1))
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
    site: crate::tape::contact::ContactSite<'_>,
) -> Result<RebuildReport> {
    // `volume_id` is NULL: the rebuild's whole premise is that the catalog
    // does not yet have a row for this volume — creating one is the
    // command's OUTPUT, not an input the contact can reference.
    let guard = site.open(conn, None);
    let contact_id = guard.id();
    let r = rebuild_contacted(
        conn,
        store,
        identities,
        expect_label,
        fallback_tenant,
        backend_name,
        scratch,
        device_label,
        site.medium_serial(),
    );
    // Issue #335: the contact opened before the rebuild registered the
    // cartridge its chip serial names (or learned that serial onto a row),
    // so it recorded "medium serial matches no registered cartridge" -- true
    // then, false once this rebuild commits. Attach it, as `volume init`
    // does after binding. Only the chip-observed serial is used, never a
    // File 0 claim.
    if r.is_ok() {
        if let Some(serial) = site.medium_serial() {
            guard.record_cartridge_by_serial(serial);
        }
    }
    let r = guard.finish_result(r);
    // The post-command health reading (issue #320), on every outcome. It
    // names no volume, as the contact names none: the row a rebuild inserts
    // is its output. On the DR machine this rebuild exists for there is
    // usually no backend, and then there is no reading and nothing fails.
    crate::volume::write::health_after_read_contact(conn, &site, None, contact_id);
    r
}

/// [`rebuild_from_store`] minus the contact bookkeeping.
#[allow(clippy::too_many_arguments)]
fn rebuild_contacted(
    conn: &Connection,
    store: &mut dyn Store,
    identities: &[age::x25519::Identity],
    expect_label: Option<&str>,
    fallback_tenant: &str,
    backend_name: Option<&str>,
    scratch: &Path,
    device_label: &str,
    medium_serial: Option<&str>,
) -> Result<RebuildReport> {
    // Bounded (issue #400): read no further than `SMALL_FILE_CAP`.
    let thunk = crate::store::read_small_bytes(store, 0, "ID thunk")?;
    let thunk_text = String::from_utf8_lossy(&thunk).to_string();
    // Before anything else of it is interpreted (ADR-0012 item 15).
    format::check_id_thunk_readable(&thunk_text)?;
    let ident = format::parse_id_thunk_identity(&thunk_text)?;
    let pointers = format::parse_id_thunk_layout_pointers(&thunk_text)?;
    let meta = format::parse_id_thunk_volume_meta(&thunk_text)?;
    // Absent/malformed `[media]` is an ABSENCE (`format::parse_id_thunk_media`'s
    // own fail-safe convention), never a reason to fail the rebuild — the
    // legacy tapes this covers are exactly what issue #165's "insert unbound"
    // path exists for.
    let media = format::parse_id_thunk_media(&thunk_text).ok();
    let identity = classify_media(media.as_ref());

    // Item 4 (issue #165): a `mam`-sourced File 0 that disagrees with a
    // serial THIS CONTACT actually observed is refused before any write.
    // `corroborate_volume` below only fires when `claim_volume_id` already
    // exists — the DR case (no row for this label yet) is the NORMAL one for
    // a rebuild, and it returns `Agreed` unconditionally on an absent claim,
    // so it never sees this disagreement on a fresh rebuild.
    if let RebuildIdentity::Mam(s) = &identity {
        if let Some(observed) = medium_serial {
            if observed != s {
                return Err(TapectlError::Other(format!(
                    "wrong cartridge: this tape's File 0 records medium serial {s}, but the \
                     drive holds {observed}. Load the cartridge this tape's own identity \
                     names, or run without a configured backend for this device if you \
                     cannot. There is no --force for this — it is a fact tapectl cannot \
                     resolve on its own, not a risk to accept."
                )));
            }
        }
    }

    if let Some(expected) = expect_label {
        if expected != ident.label {
            return Err(TapectlError::Other(format!(
                "wrong tape: expected label \"{expected}\", found \"{}\" (uuid {})",
                ident.label, ident.uuid
            )));
        }
    }

    // Corroborate at contact (ADR-0012, issue #193), against the volume row
    // this tape says it is — when there IS one.
    //
    // This is the contact where the absence rule matters most: rebuild
    // exists for the catalog that does not know this tape, so `None` here is
    // the NORMAL case, not a failure. It corroborates only a cartridge
    // binding the catalog already holds, which is the one thing a rebuild
    // can contradict: a row saying this volume lives on cartridge A while
    // the drive reports B.
    let claim_volume_id: Option<i64> = conn
        .query_row(
            "SELECT id FROM volumes WHERE label = ?1",
            rusqlite::params![&ident.label],
            |r| r.get(0),
        )
        .optional()?;
    // Issue #221: this can itself teach a bound cartridge row its serial
    // (`record_medium_serial`, committed straight to `conn` — there is no
    // `tx` yet) and print the "learnt" note to stderr. `report` does not
    // exist yet at this point (it is built below, once the operator
    // envelope is open), so the outcome is captured here and folded into
    // the struct literal instead of moving this call — corroboration must
    // stay the very first fact check, before any envelope is opened.
    let serial_learned_at_contact = match claim_volume_id {
        Some(volume_id) => {
            let medium = crate::volume::binding::MediumFacts::new(
                medium_serial.map(str::to_string),
                crate::volume::binding::file0_facts_from_text(&thunk_text),
            );
            matches!(
                crate::volume::binding::corroborate_volume(conn, volume_id, &ident.label, &medium)?,
                crate::volume::binding::Corroboration::SerialLearned { .. }
            )
        }
        None => false,
    };

    let fi = crate::store::read_small_bytes(store, pointers.front_index as u32, "front index")?;
    let entries = format::parse_front_index(&String::from_utf8_lossy(&fi))?;

    let EnvelopeWalk { opened, unreadable } =
        open_all_envelopes(store, &entries, identities, scratch)?;
    // The operator envelope is the one a rebuild cannot do without, copy or
    // backup: it names every unit and carries the `catalog.db`.
    if let Some(op) = unreadable.iter().find(|u| u.is_operator()) {
        return Err(TapectlError::Other(op.message.clone()));
    }
    // Issue #458: whether the tape is SEALED, read before any row is written
    // and after the envelopes (the seal marker is the last file, so reading
    // it earlier would pass the head over the slices twice; `volume::
    // cost_budget` pins the motion). The same read dates the tape (#305).
    // #420: located by `volume-format-v2.md` §1.2 rule 6, so a tape that
    // ended early is read as sealed at its short seal, with `ended_after`.
    let LocatedSeal {
        verdict: seal,
        ended_after,
        ..
    } = read_seal(store, &pointers, &thunk_text, &fi, &ident.label);
    if let format::SealVerdict::Refused(why) = &seal {
        // Before the transaction, as every other reader of a tape refuses
        // one it must not interpret (`volume-format-v2.md` §1.2 rule 1).
        return Err(TapectlError::Other(why.clone()));
    }
    let dates = tape_dates(&thunk_text, &entries, &seal, ended_after);
    let unsealed_reason = match &seal {
        format::SealVerdict::Unsealed(why) => Some(why.clone()),
        _ => None,
    };
    let operator = opened.iter().find(|e| e.is_operator()).ok_or_else(|| {
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
        serial_checked: medium_serial.is_some(),
        // Issue #221: a serial can be learnt at the pre-transaction
        // corroboration above, before `resolve_operator_identity` (which
        // sets this same field on its own learn branch) ever runs — once
        // the row is found BY that serial, `resolve_operator_identity`
        // takes its "already witnessed" branch instead, so its own
        // learn-branch write to this field never fires. Seeded here so a
        // rebuild that changed `cartridges.serial_number` before the
        // transaction even opened is not reported as `no_changes: true`.
        serial_learned: serial_learned_at_contact,
        written_at: dates.written_at.clone(),
        sealed_at: dates.sealed_at.clone(),
        load_count_at_write: dates.load_count_at_write,
        unsealed_reason,
        ended_after: ended_after.map(u64::from),
        ..Default::default()
    };

    let supplement = match operator.catalog_db.as_deref() {
        Some(path) => Supplement::load(path)?,
        None => Supplement::default(),
    };
    // Issue #451: a tenant envelope that no copy of could be read is not
    // needed when the operator envelope's `catalog.db` names each unit's
    // owner (generation 2 or 3): the operator manifest lists every unit and
    // every slice, and the receipts ride in `catalog.db`. Without that
    // table the tenant envelopes are the only record of ownership, and the
    // rebuild would silently file those units under `--tenant`: refuse, as
    // before.
    if let Some(first) = unreadable.first() {
        if !supplement.has_tenants {
            return Err(TapectlError::Other(format!(
                "{} A catalog.db this old (or none at all) does not name which tenant owns \
                 each unit, so the tenant envelopes are the only record of it and the rebuild \
                 needs every one. Use another copy of this volume; RESTORE.sh (tape file 2) \
                 with a tenant key or the escrow key still restores what opens.",
                first.message
            )));
        }
        for u in &unreadable {
            tracing::warn!(
                position = u.position,
                type_label = %u.type_label,
                error = %u.message,
                "rebuild: a tenant envelope is unreadable in every copy; going on without it, \
                 the operator catalog.db names every unit's owner"
            );
        }
        report.unreadable_tenant_envelopes = unreadable.iter().map(|u| u.describe()).collect();
    }
    let tenant_of = tenant_index(&opened, &supplement);
    report.tenants_from_catalog_db = supplement.has_tenants;

    // `unchecked_transaction` matches the codebase's convention (session,
    // write, key, operations): the CLI holds a shared `Connection`, and
    // requiring `&mut` here would ripple through every caller for nothing.
    //
    // Issue #413: no tape is read while this holds the write lock. The rows
    // the envelopes and `catalog.db` describe (all read above) and the
    // cartridge binding commit together; attestation, which reads one slice
    // header per unit off the tape, runs after the commit with each receipt
    // its own guarded UPDATE; the event is a second short transaction.
    let tx = crate::db::busy::immediate_tx(conn)?;
    let inserted = insert_all(
        &tx,
        &ident.label,
        &ident.uuid,
        &meta,
        &dates,
        &operator.manifest,
        &tenant_of,
        fallback_tenant,
        backend_name,
        &supplement,
        &mut report,
    )?;
    let volume_id = inserted.volume_id;
    resolve_and_bind_cartridge(
        &tx,
        volume_id,
        &ident.label,
        &identity,
        medium_serial,
        &meta,
        media.as_ref(),
        &mut report,
    )?;
    tx.commit()?;

    // #478 (item 128): a snapshot this run found `staged` and made `current`
    // gets the status-change event confirm's promotion records, after the
    // commit and best-effort, for confirm's reason: an advisory row must not
    // roll back the rows it describes.
    for p in &inserted.promoted {
        if let Err(e) = crate::db::events::log_field_change(
            conn,
            "snapshot",
            p.snapshot_id,
            &p.unit_name,
            "sealed_current",
            "status",
            Some(&p.old_status),
            "current",
            Some(p.tenant_id),
        ) {
            tracing::warn!(
                snapshot_id = p.snapshot_id,
                unit = %p.unit_name,
                error = %e,
                "rebuilt, but the snapshot-promotion audit row could not be written"
            );
        }
    }

    // Issue #473: the dar catalogues, after the rows are committed (files
    // placed inside the transaction would be orphaned by a rollback, and
    // `staging clean` would not know them). Best effort per set: a set that
    // cannot get its catalogue is named in the report, never a failed rebuild.
    restore_catalogues(conn, operator, &inserted.sets, &mut report);

    // The cross-volume index (§1.5): after the rows above are committed, so
    // this tape itself is never listed as missing. Read-only on the catalog.
    let index = index_to_do(conn, operator, &ident.label, &ident.uuid, &mut report);
    // #478 (item 130): and the slices of a spanning version still on no tape
    // this catalog records, with the tapes the index names for them.
    span_to_do(
        conn,
        &inserted.sets,
        &index,
        &ident.label,
        &ident.uuid,
        &mut report,
    );

    // The rows above are committed, so the provenance event is recorded even
    // when attestation fails partway (a busy catalog, say); its error is
    // returned after. Re-running the rebuild attests what is left.
    let attested = attest_escrow(conn, store, identities, &operator.manifest, &mut report);

    let tx = crate::db::busy::immediate_tx(conn)?;
    // A `cut` set (#420) is not counted: it is no copy on this tape, and its
    // slice header may lie past where the tape ended, so attestation from
    // this tape is not the remedy for it.
    report.unknown_remaining = tx.query_row(
        "SELECT COUNT(*) FROM stage_sets ss
         JOIN writes w ON w.stage_set_id = ss.id
         WHERE w.volume_id = ?1 AND ss.origin = 'rebuilt' AND ss.key_fingerprints IS NULL
           AND w.status <> 'cut'",
        params![volume_id],
        |r| r.get(0),
    )?;
    record_event(&tx, &report, volume_id, device_label)?;
    tx.commit()?;

    attested?;
    Ok(report)
}

/// Fill the report's cross-volume index fields from the operator envelope's
/// `index/units.tsv`: the volumes it names that the catalog has no row for.
///
/// **Reads the catalog, never writes it** (ADR-0001; §1.5): an index row is
/// one tape's claim about another, and a claim about a tape needs contact
/// with that tape, which only `catalog rebuild --from-volume` on that tape
/// has. A volume is known to the catalog by its uuid, or, for a catalog row
/// that never recorded one, by its label. An index that cannot be read is
/// reported and set aside: the rebuild it accompanies has already succeeded.
///
/// Returns the index's rows, for [`span_to_do`]: none for a tape without an
/// index, or with one that could not be used.
fn index_to_do(
    conn: &Connection,
    operator: &OpenedEnvelope,
    this_label: &str,
    this_uuid: &str,
    report: &mut RebuildReport,
) -> Vec<crate::volume::index::UnitsRow> {
    let Some(path) = &operator.index_units else {
        return Vec::new();
    };
    report.had_index = true;
    let rows = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read the extracted index/units.tsv: {e}"))
        .and_then(|text| crate::volume::index::parse_units(&text).map_err(|e| e.to_string()));
    let rows = match rows {
        Ok(rows) => rows,
        Err(why) => {
            tracing::warn!("rebuild: this tape's cross-volume index is unusable: {why}");
            report.index_problem = Some(why);
            return Vec::new();
        }
    };

    // The state this tape's own rows carry is `this-tape`: that is this
    // volume, rebuilt above. Anything else is another tape.
    let mut by_volume: std::collections::BTreeMap<(String, String), IndexedVolume> =
        std::collections::BTreeMap::new();
    for row in &rows {
        if is_this_tape(row, this_label, this_uuid) || row.volume.is_empty() {
            continue;
        }
        let volume = by_volume
            .entry((row.volume.clone(), row.volume_uuid.clone()))
            .or_insert_with(|| IndexedVolume {
                label: row.volume.clone(),
                uuid: row.volume_uuid.clone(),
                cartridge_serial: row.cartridge_serial.clone(),
                generation: row.generation.clone(),
                state: row.state.clone(),
                sealed_at: row.sealed_at.clone(),
                units: Vec::new(),
            });
        let unit = match volume
            .units
            .iter_mut()
            .position(|u| u.tenant == row.tenant && u.unit == row.unit)
        {
            Some(i) => &mut volume.units[i],
            None => {
                volume.units.push(IndexedUnit {
                    tenant: row.tenant.clone(),
                    unit: row.unit.clone(),
                    versions: Vec::new(),
                });
                volume.units.last_mut().expect("just pushed")
            }
        };
        if !unit.versions.contains(&row.version) {
            unit.versions.push(row.version);
        }
    }

    let known = |v: &IndexedVolume| -> Result<bool> {
        Ok(if v.uuid.is_empty() {
            conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM volumes WHERE label = ?1)",
                params![v.label],
                |r| r.get(0),
            )?
        } else {
            conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM volumes
                  WHERE uuid = ?1 OR (uuid IS NULL AND label = ?2))",
                params![v.uuid, v.label],
                |r| r.get(0),
            )?
        })
    };
    for (_, mut volume) in by_volume {
        match known(&volume) {
            Ok(true) => {}
            Ok(false) => {
                volume
                    .units
                    .sort_by(|a, b| (&a.tenant, &a.unit).cmp(&(&b.tenant, &b.unit)));
                for u in &mut volume.units {
                    u.versions.sort_unstable();
                }
                report.index_volumes_missing.push(volume);
            }
            Err(e) => {
                tracing::warn!(
                    "rebuild: cannot check the index's volumes against the catalog: {e}"
                );
                report.index_problem = Some(format!("cannot check the catalog: {e}"));
                report.index_volumes_missing.clear();
                return rows;
            }
        }
    }
    rows
}

/// Whether an index row is this tape's own: `this-tape`, or this label or
/// uuid.
fn is_this_tape(row: &crate::volume::index::UnitsRow, this_label: &str, this_uuid: &str) -> bool {
    row.state == "this-tape"
        || row.volume == this_label
        || (!row.volume_uuid.is_empty() && row.volume_uuid == this_uuid)
}

/// The to-do list's span half (#478, ADR-0012 item 130; `volume-format-v2.md`
/// §1.5 and §4.4): for each spannable set on this tape, the slices no tape
/// this catalog records holds after this rebuild, and the volumes this tape's
/// index (`index`, its rows) names as holding them. A version with such a
/// slice stays `staged` and reads 0 copies until they are rebuilt (item 128).
///
/// A slice is held when its copies ([`crate::policy::coverage::slice_copies`],
/// the one per-slice derivation) are not 0, or when this tape holds it: the
/// rebuild records this tape's seal without changing a found row's status
/// (#360's lost write is `initialized`), so it is counted here as the
/// promotion rule counts it. Reads the catalog, never writes it, and, as the
/// index half, never adds a row from an index (ADR-0001). A tape never names a
/// tape written after it, so the first tape of a span names no holder for its
/// tail: [`SpanInProgress::holders`] is then empty.
fn span_to_do(
    conn: &Connection,
    sets: &[RebuiltSet],
    index: &[crate::volume::index::UnitsRow],
    this_label: &str,
    this_uuid: &str,
    report: &mut RebuildReport,
) {
    for set in sets.iter().filter(|s| s.may_span) {
        let copies = match crate::policy::coverage::slice_copies(conn, set.stage_set_id) {
            Ok(copies) => copies,
            Err(e) => {
                tracing::warn!(
                    unit = %set.unit_name,
                    "rebuild: cannot read which slices of a spanning version are held: {e}"
                );
                continue;
            }
        };
        let missing: Vec<i64> = copies
            .iter()
            .filter(|c| c.copies == 0 && !set.held_here.contains(&c.slice_number))
            .map(|c| c.slice_number)
            .collect();
        if missing.is_empty() {
            continue;
        }
        let mut holders: Vec<SpanHolder> = Vec::new();
        for row in index {
            let same_unit = if row.unit_uuid.is_empty() {
                row.unit == set.unit_name
            } else {
                row.unit_uuid == set.unit_uuid
            };
            if !same_unit
                || row.version != set.version
                || row.volume.is_empty()
                || is_this_tape(row, this_label, this_uuid)
            {
                continue;
            }
            // A row that does not state its run may hold any of them.
            let holds_one = match (row.first_slice, row.last_slice) {
                (Some(first), Some(last)) => missing.iter().any(|n| (first..=last).contains(n)),
                _ => true,
            };
            if holds_one
                && !holders
                    .iter()
                    .any(|h| h.label == row.volume && h.uuid == row.volume_uuid)
            {
                holders.push(SpanHolder {
                    label: row.volume.clone(),
                    uuid: row.volume_uuid.clone(),
                    cartridge_serial: row.cartridge_serial.clone(),
                    state: row.state.clone(),
                    first_slice: row.first_slice,
                    last_slice: row.last_slice,
                });
            }
        }
        holders.sort_by(|a, b| (&a.label, &a.uuid).cmp(&(&b.label, &b.uuid)));
        report.spans_in_progress.push(SpanInProgress {
            unit: set.unit_name.clone(),
            version: set.version,
            num_slices: set.num_slices,
            missing,
            holders,
        });
    }
}

/// How much of a slice to read for attestation. An age header is a few
/// hundred bytes per recipient plus a MAC line; one tape block (512 KiB) is
/// far more than enough, and `MemStore` pads to its block size anyway.
const ATTEST_HEAD_BYTES: u64 = 64 * 1024;

/// Attest escrow coverage by demonstration (#137, CTO grilling Q2/Q10).
///
/// A recorded recipient list is a claim tapectl wrote down. Stronger proof
/// exists when the escrow key is in hand: if it decrypts the slice, the slice
/// is escrow-covered. age puts the recipient stanzas in the header and
/// unwraps the file key before touching payload, so this reads one slice
/// *header* per stage set — `Store::read_file_head` — never the slice.
///
/// Applies only when the supplied key's public half is the escrow recipient
/// this catalog has REGISTERED. That is what makes "import the original
/// escrow key first" (the DR procedure's step one) load-bearing rather than
/// advisory: with a replacement identity registered, nothing attests. An
/// operator key is never used here — it opening a slice proves operator
/// coverage, which is not the claim being recorded.
///
/// It must be a SLICE, not the envelope: #115 showed the two can differ
/// (staged before the escrow key was registered, envelope written after).
///
/// Records `[<escrow public key>]` — only the key that was demonstrated; the
/// other recipients are unknown and are not invented. `origin` stays
/// `rebuilt`. Any failure other than "not a recipient" is logged and skipped:
/// attestation is an add-on, and a rebuild must not fail because of it.
fn attest_escrow(
    conn: &Connection,
    store: &mut dyn Store,
    identities: &[age::x25519::Identity],
    manifest: &EnvelopeManifest,
    report: &mut RebuildReport,
) -> Result<()> {
    let Some(registered) = crate::db::queries::escrow_public_key(conn)? else {
        return Ok(());
    };
    let Some(escrow_id) = identities
        .iter()
        .find(|id| id.to_public().to_string() == registered)
    else {
        return Ok(());
    };
    report.key_is_escrow = true;
    // Issue #458: no slice header is read off a tape whose seal was not read.
    // Its slices may never have been laid down (the envelopes and the front
    // index precede them), so a head read would position past what the tape
    // holds, and coverage of a write that is not a copy is not in question.
    if report.unsealed_reason.is_some() {
        return Ok(());
    }
    let receipt = serde_json::to_string(&[registered.as_str()])
        .map_err(|e| TapectlError::Other(format!("receipt json: {e}")))?;

    // Each unit's first slice, in tape order, so the head reads move only
    // forward (issue #389: `TapeStore` spaces forward to a file ahead of the
    // head and rewinds for one behind it). The manifest lists units in the
    // order they were laid out, so this is normally that order already.
    //
    // #420 (`volume-format-v2.md` §4.2 R3): on a tape that ended early no
    // position after `ended_after` is read. A unit whose first slice lies
    // past it is not on this tape at all, and stays unknown here.
    //
    // #478 (ADR-0012 items 125 and 130, `volume-format-v2.md` §4.4 R6): a
    // slice with no `tape_position` (a range's slice planned on another
    // tape) is not on this tape either, so the slice read is the lowest-
    // numbered one THIS tape holds: slice 1 for a whole unit and for a range
    // that starts at it, the first of the run for a range that starts later.
    // Any slice of the set demonstrates the set: every slice of one staging
    // is encrypted to the same recipients.
    let ended_after = report.ended_after;
    let on_tape =
        |position: i64| ended_after.is_none_or(|k| u64::try_from(position).is_ok_and(|p| p <= k));
    let mut firsts: Vec<(&envelope::ManifestUnit, &envelope::ManifestSlice, i64)> = manifest
        .units
        .iter()
        .filter_map(|unit| {
            unit.slices
                .iter()
                .filter_map(|s| Some((s, s.tape_position.filter(|&p| on_tape(p))?)))
                .min_by_key(|(s, _)| s.number)
                .map(|(first, position)| (unit, first, position))
        })
        .collect();
    firsts.sort_by_key(|&(_, _, position)| position);

    for (unit, first, position) in firsts {
        // The stage set whose first slice IS the one about to be read (issue
        // #379): a Version can have several stage sets, and attesting one
        // from another's slice header would record a claim no slice of it
        // demonstrated.
        let stage_set_id: Option<i64> = conn
            .query_row(
                "SELECT ss.id FROM stage_sets ss
                 JOIN snapshots s ON s.id = ss.snapshot_id
                 JOIN units u ON u.id = s.unit_id
                 JOIN stage_slices sl ON sl.stage_set_id = ss.id
                 WHERE u.name = ?1 AND s.version = ?2
                   AND sl.slice_number = ?3 AND sl.sha256_encrypted = ?4
                   AND ss.origin = 'rebuilt' AND ss.key_fingerprints IS NULL
                 LIMIT 1",
                params![
                    unit.name,
                    unit.snapshot_version,
                    first.number,
                    first.sha256_encrypted
                ],
                |r| r.get(0),
            )
            .optional()?;
        let Some(stage_set_id) = stage_set_id else {
            continue; // already has a receipt (from the tape, or an earlier attestation)
        };

        let mut head = Vec::new();
        if let Err(e) = store.read_file_head(position as u32, ATTEST_HEAD_BYTES, &mut head) {
            tracing::warn!(unit = %unit.name, position, error = %e,
                "attest: could not read the slice header; leaving coverage unknown");
            report.escrow_attest_unreadable += 1;
            continue;
        }
        let opened = age::Decryptor::new(Cursor::new(head))
            .and_then(|d| d.decrypt(std::iter::once(escrow_id as &dyn age::Identity)));
        match opened {
            Ok(_) => {
                // Autocommit, and guarded: a receipt is never overwritten,
                // and the rows it completes committed before any tape read.
                conn.execute(
                    "UPDATE stage_sets SET key_fingerprints = ?1
                     WHERE id = ?2 AND key_fingerprints IS NULL",
                    params![receipt, stage_set_id],
                )?;
                report.attested += 1;
            }
            Err(age::DecryptError::NoMatchingKeys) => {
                tracing::warn!(unit = %unit.name, position,
                    "attest: the escrow key is not a recipient of this slice, so it cannot \
                     attest this stage set; coverage stays unknown");
                report.escrow_attest_not_recipient += 1;
            }
            Err(e) => {
                tracing::warn!(unit = %unit.name, position, error = %e,
                    "attest: slice header did not parse; leaving coverage unknown");
                report.escrow_attest_unparseable += 1;
            }
        }
    }
    Ok(())
}

/// Open every envelope the front index names, skipping the ones this key is
/// not a recipient of.
///
/// A tenant envelope that will not open is routine and silent — an operator
/// key opens all of them, but running with a tenant key opens exactly one.
/// The operator envelope failing to open is the one case worth a specific
/// diagnosis, because it is what a tenant key looks like from here.
///
/// Every envelope has a backup copy (`volume-format-v2.md` §1.3: the
/// operator's since v2's first write, each tenant's since 1.1.0), the same
/// bytes under the same front-index hash. A backup is opened only when the
/// envelope it copies could not be — it did not read, did not match the
/// front index's hash, or did not decrypt — and is skipped once a copy with
/// its hash has opened or, its bytes matching that hash, proved the key is
/// not a recipient. A copy that failed is an error only when no copy with
/// its hash opened by the end of the walk.
/// The HEIR's recipe, and the reason it is a function rather than an inline
/// string: it names a command, a flag and a FILENAME, and all three are
/// facts about other modules that can drift. Issue #214's sibling sweep
/// found two of the three already wrong — `restore raw-volume` takes `--to`,
/// not `--dest`, and the dumped file is named from the zone's own
/// `type_label` (`restore_sh`), so `0002_restore_script.bin` never existed.
///
/// This message is printed when the supplied key opens no operator envelope,
/// i.e. the operator key is gone and a tenant is recovering their own data
/// with no catalog. It is read exactly once, mid-disaster, by someone who
/// cannot ask anyone what the right spelling was.
///
/// The filename is DERIVED from `ZoneKind::RestoreSh::type_label()` and the
/// `{:04}_{}.bin` shape `raw::dump` uses, so renaming the zone updates this
/// message instead of silently re-breaking it.
fn tenant_key_refusal_message() -> String {
    // Position 2 is fixed by the format itself (`volume-format-v2.md`: 0 ID
    // thunk, 1 system guide, 2 RESTORE.sh, 3 front index) and is pinned by
    // `tests/on_tape_golden.rs`, so it is a literal. The LABEL is the half
    // that drifted, so it is derived.
    let restore_sh = format!(
        "{:04}_{}.bin",
        2,
        crate::volume::layout_model::ZoneKind::RestoreSh.type_label()
    );
    // Issue #417: `--only restore_sh` — the bare command dumps every file,
    // a terabyte on a full cartridge, to fetch a 50 KB script. File 1's own
    // two-line `mt`/`dd` recipe needs no tapectl at all.
    format!(
        "{TENANT_KEY_REFUSAL_LEAD}\n\n\
         A tenant key restores that tenant's own data without a catalog at \
         all: run RESTORE.sh from tape file 2 —\n\n    \
         tapectl restore raw-volume --device DEV --to DIR --only restore_sh\n    \
         bash DIR/{restore_sh} --restore --unit UNIT --key KEYFILE --to NEWDIR\n\n\
         NEWDIR is a different directory from DIR: DIR now holds the dumped \
         script, and RESTORE.sh refuses a --to that is not empty before it \
         reads the tape. NEWDIR may be empty or not exist yet.\n\n\
         or, with no tapectl, as the tape's own guide (file 1) says:\n\n    \
         mt -f DEV rewind && mt -f DEV fsf 2\n    \
         dd if=DEV bs=512k | tr -d '\\0' > RESTORE.sh\n\n\
         `catalog rebuild` reconstructs the operator's catalog and needs the \
         operator's view of the tape."
    )
}

/// The front-index kinds that are envelopes: every primary and its backup
/// (`volume-format-v2.md` §1.3).
fn is_envelope_label(label: &str) -> bool {
    matches!(
        label,
        "tenant_envelope"
            | "operator_envelope"
            | "operator_envelope_backup"
            | "tenant_envelope_backup"
    )
}

/// What [`open_all_envelopes`] found: the envelopes that opened, and the
/// ones no copy of which could be read (issue #451).
struct EnvelopeWalk {
    opened: Vec<OpenedEnvelope>,
    unreadable: Vec<UnreadableEnvelope>,
}

/// An envelope whose primary and every backup copy failed to read, match the
/// front index's hash, or decrypt.
#[derive(Debug, Clone)]
struct UnreadableEnvelope {
    /// The front-index kind of the first copy that failed.
    type_label: String,
    position: i32,
    /// The positions of its backup copies, which failed too.
    backup_positions: Vec<i32>,
    /// What went wrong, worded as the refusal that names it.
    message: String,
}

impl UnreadableEnvelope {
    /// The operator envelope is the one the rebuild cannot do without.
    fn is_operator(&self) -> bool {
        matches!(
            crate::volume::layout_model::ZoneKind::from_type_label(&self.type_label),
            Some(
                crate::volume::layout_model::ZoneKind::OperatorEnvelope
                    | crate::volume::layout_model::ZoneKind::OperatorEnvelopeBackup
            )
        )
    }

    /// One line for the report: which envelope, where, and why.
    fn describe(&self) -> String {
        let backups = if self.backup_positions.is_empty() {
            "no backup copy".to_string()
        } else {
            format!(
                "backup copy at position {} too",
                self.backup_positions
                    .iter()
                    .map(i32::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        format!(
            "{} at position {} ({backups}): {}",
            self.type_label, self.position, self.message
        )
    }
}

/// Group the failed copies by front-index hash, in tape order: one
/// [`UnreadableEnvelope`] per envelope, its first copy the primary.
fn unreadable_envelopes(
    failed: Vec<(String, i32, String, TapectlError)>,
) -> Vec<UnreadableEnvelope> {
    let mut keys: Vec<String> = Vec::new();
    for (key, ..) in &failed {
        if !keys.contains(key) {
            keys.push(key.clone());
        }
    }
    keys.into_iter()
        .map(|key| {
            let group: Vec<_> = failed.iter().filter(|(k, ..)| k == &key).collect();
            let (_, position, type_label, e) = group[0];
            let backups = &group[1..];
            let message = if backups.is_empty() {
                format!(
                    "the {type_label} at position {position} could not be opened, \
                     and the tape holds no other copy of it: {e}"
                )
            } else {
                let at: Vec<String> = backups.iter().map(|(_, p, ..)| p.to_string()).collect();
                let why: Vec<String> = backups.iter().map(|(.., e)| e.to_string()).collect();
                format!(
                    "the {type_label} at position {position} could not be opened, \
                     and neither could its backup copy at position {}: {e}; the backup: {}",
                    at.join(", "),
                    why.join("; ")
                )
            };
            UnreadableEnvelope {
                type_label: type_label.clone(),
                position: *position,
                backup_positions: backups.iter().map(|(_, p, ..)| *p).collect(),
                message,
            }
        })
        .collect()
}

fn open_all_envelopes(
    store: &mut dyn Store,
    entries: &[format::ParsedIndexEntry],
    identities: &[age::x25519::Identity],
    scratch: &Path,
) -> Result<EnvelopeWalk> {
    let mut out = Vec::new();
    let mut operator_refused = false;
    // Front-index hashes of the copies already settled: one opened, or the
    // key proved not a recipient. A later copy with the same hash is the
    // same bytes and would only duplicate (or repeat) that answer. Both
    // outcomes are answers about the KEY only because `open_envelope` checks
    // the bytes against that hash before age sees them: a copy that does not
    // match is `Failed`, whatever age would have said, since age reports a
    // damaged recipient stanza as `NoMatchingKeys`.
    let mut settled: HashSet<String> = HashSet::new();
    // Copies that failed (hash, position, kind, why), in tape order: an
    // error only if no copy with that hash opens by the end of the walk.
    let mut failed: Vec<(String, i32, String, TapectlError)> = Vec::new();

    for entry in entries {
        if !is_envelope_label(&entry.type_label) {
            continue;
        }
        // An envelope with no recorded hash cannot be paired with a copy;
        // it is its own key, by position.
        let copy_key = entry
            .sha256_encrypted
            .clone()
            .unwrap_or_else(|| format!("@{}", entry.position));
        if settled.contains(&copy_key) {
            continue;
        }

        match envelope::open_envelope(store, entry, identities, scratch) {
            Ok(env) => {
                if entry.type_label.ends_with("_backup") {
                    tracing::warn!(
                        position = entry.position,
                        type_label = %entry.type_label,
                        "rebuild: opened the backup copy of an envelope that could not be used"
                    );
                }
                failed.retain(|(k, ..)| k != &copy_key);
                settled.insert(copy_key);
                out.push(env);
            }
            Err(OpenError::NoMatchingKey) => {
                if entry.type_label.starts_with("operator_envelope") {
                    operator_refused = true;
                }
                tracing::debug!(
                    position = entry.position,
                    type_label = %entry.type_label,
                    "rebuild: key is not a recipient of this envelope, skipping"
                );
                failed.retain(|(k, ..)| k != &copy_key);
                settled.insert(copy_key);
            }
            Err(OpenError::Failed(e)) => {
                // A damaged or unreadable copy: its backup (further on, the
                // same hash) is the whole reason there are two, so carry on.
                tracing::warn!(
                    position = entry.position,
                    type_label = %entry.type_label,
                    error = %e,
                    "rebuild: envelope unreadable, falling back to its backup copy if it has one"
                );
                failed.push((copy_key, entry.position, entry.type_label.clone(), e));
            }
        }
    }

    // A key that opens no operator envelope is a tenant's: that is the
    // answer, whatever else on the tape is damaged.
    if operator_refused && !out.iter().any(|e| e.is_operator()) {
        return Err(TapectlError::Other(tenant_key_refusal_message()));
    }

    // Issue #451: an envelope whose every copy failed is DATA here, not an
    // error. Whether the rebuild can do without it depends on what the
    // operator envelope's `catalog.db` carries, which is not known until it
    // is open; the caller decides.
    Ok(EnvelopeWalk {
        opened: out,
        unreadable: unreadable_envelopes(failed),
    })
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
        // By the envelope's kind, not its manifest's tenant string (#459): a
        // real tenant named `operator` owns its units like any other.
        if env.is_operator() {
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
    /// (unit name, version) -> that snapshot's id in `db`, whose file rows
    /// `ensure_files` streams one version at a time through `file_spans`
    /// (issue #413). A version with no rows on tape has no entry.
    file_snapshot: HashMap<(String, i64), i64>,
    /// The open `catalog.db`, when the tape carries one, and where each
    /// version's file rows lie in it, whatever its generation.
    db: Option<(Connection, ontape_catalog::FileSpans)>,
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

impl Supplement {
    /// Read every table but `files` via `ontape_catalog::read_all_but_files`,
    /// then derive exactly the maps `insert_all`/`tenant_index`/`ensure_*`
    /// already consume, keyed by unit name (and version, where a fact is
    /// per-snapshot) the way the hand-written joins used to key them. The
    /// `files` rows, nearly all of the file (about 250 MB of rows for a
    /// million-file tape), stay on disk until `ensure_files` streams each
    /// version's (issue #413).
    fn load(path: &Path) -> Result<Self> {
        let (db, cat) = ontape_catalog::read_all_but_files(path)?;
        let mut out = Supplement::default();

        let unit_name: HashMap<i64, String> =
            cat.units.iter().map(|u| (u.id, u.name.clone())).collect();
        let tenant_name: HashMap<i64, String> =
            cat.tenants.iter().map(|t| (t.id, t.name.clone())).collect();
        // snapshot id -> (owning unit's name, version), the stable key every
        // consumer below joins on.
        let snapshot_key: HashMap<i64, (String, i64)> = cat
            .snapshots
            .iter()
            .filter_map(|s| {
                unit_name
                    .get(&s.unit_id)
                    .map(|n| (s.id, (n.clone(), s.version)))
            })
            .collect();

        for s in &cat.snapshots {
            let Some(key) = snapshot_key.get(&s.id) else {
                continue;
            };
            let snapshot_type = s.snapshot_type.clone().ok_or_else(|| {
                TapectlError::Other(format!(
                    "catalog.db snapshots row {} has NULL snapshot_type",
                    s.id
                ))
            })?;
            out.snapshots.insert(
                key.clone(),
                SnapshotFacts {
                    source_path: s.source_path.clone(),
                    snapshot_type,
                    total_size: s.total_size,
                    file_count: s.file_count,
                },
            );
        }

        for ss in &cat.stage_sets {
            let Some((name, _version)) = snapshot_key.get(&ss.snapshot_id) else {
                continue;
            };
            let slice_size = ss.slice_size.ok_or_else(|| {
                TapectlError::Other(format!(
                    "catalog.db stage_sets row {} has NULL slice_size",
                    ss.id
                ))
            })?;
            out.slice_size.insert(name.clone(), slice_size);
            if let Some(fp) = &ss.key_fingerprints {
                out.key_fingerprints.insert(name.clone(), fp.clone());
            }
        }

        // Only where each version's rows lie: `ensure_files` streams them.
        let spans = ontape_catalog::FileSpans::read(&db, cat.generation)?;
        out.file_snapshot = snapshot_key
            .into_iter()
            .filter(|(id, _)| spans.contains(*id))
            .map(|(id, key)| (key, id))
            .collect();
        out.db = Some((db, spans));

        out.has_tenants = cat.generation.carries_ownership_and_receipts();
        if out.has_tenants {
            for u in &cat.units {
                if let (Some(uname), Some(tname)) =
                    (unit_name.get(&u.id), tenant_name.get(&u.tenant_id))
                {
                    out.tenant_of.insert(uname.clone(), tname.clone());
                }
            }
        }

        Ok(out)
    }
}

/// How far a found row's `sealed_at` may sit from the seal marker's before
/// the rebuild reports them as disagreeing (issue #305). They are two clocks
/// on one host a few seconds apart: the marker's is taken as it is
/// generated, the row's when `seal()` returns, after the marker and its
/// synchronous filemark are on tape. Ten minutes is far past that and far
/// short of any real disagreement (a different write, a different tape).
pub const SEALED_AT_TOLERANCE_SECS: i64 = 600;

/// The tape's own dates and data-file count (issue #305), in the catalog's
/// form.
#[derive(Debug, Clone, Default)]
struct TapeDates {
    written_at: Option<String>,
    sealed_at: Option<String>,
    load_count_at_write: Option<i64>,
    data_files: i64,
}

/// What `volume-format-v2.md` §1.2 rule 6 found at the end of a tape and
/// accepts as its seal, judged against the front index beside it (#420): the
/// one judgement `catalog rebuild`, the adoption of a lost write
/// ([`crate::volume::adopt_lost`]) and `restore raw-volume` share, so the
/// three can never disagree about where a tape ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocatedSeal {
    /// [`format::SealVerdict::Sealed`] only for a seal rule 6 accepts where
    /// it was found and that binds the front index.
    pub verdict: format::SealVerdict,
    /// Where the seal was read: the planned position, or the last file on a
    /// medium that ends before it. `None` when no file was there at all.
    pub position: Option<u32>,
    /// `Some(K)` for an accepted SHORT seal at K+1 (§4.1): every planned
    /// position after K is not on the tape (§4.2). `None` for a full seal,
    /// and for no seal.
    pub ended_after: Option<u32>,
}

impl LocatedSeal {
    fn unsealed(position: Option<u32>, why: String) -> Self {
        Self {
            verdict: format::SealVerdict::Unsealed(why),
            position,
            ended_after: None,
        }
    }
}

/// Find the seal by `volume-format-v2.md` §1.2 rule 6 ([`crate::store::
/// locate_seal`]: at `planned`, File 0's `[layout] seal_marker`, or, on a
/// medium that ends before it, the last file), judge it against the front
/// index (`front_index` = File 3's true bytes, at `front_index_position`) by
/// [`format::judge_seal`], and accept it as rule 6 does:
///
/// - **at the planned position**, a seal whose `file_count` is the plan's
///   (`planned + 1`): the full seal (§4.1 "Classification");
/// - **before it**, only on a tape that requires `short-seal` (File 0's
///   `requires`; the seal's own when File 0's claims do not parse), a seal
///   that itself requires it, keeps §4.1's parse rules (it parsed) and states
///   `file_count = N` for the N files on the medium. Its `ended_after` is K.
///
/// Anything else is [`format::SealVerdict::Unsealed`], naming why: a reader
/// of a catalog row only ever records a tape sealed on a seal it accepts. A
/// seal this tapectl must not interpret is [`format::SealVerdict::Refused`]
/// wherever it is (§1.2 rule 1: refused by name, never misread). A read that
/// fails is unsealed here, as it was before 1.6.0: neither caller verifies a
/// tape, and an unsealed reading records no copy (#458).
pub(crate) fn locate_and_judge_seal(
    store: &mut dyn Store,
    file0: &str,
    front_index: &[u8],
    front_index_position: u32,
    planned: u32,
) -> LocatedSeal {
    let (position, bytes) = match crate::store::locate_seal(store, planned) {
        crate::store::SealLocation::File { position, bytes } => (position, bytes),
        crate::store::SealLocation::Oversized { position } => {
            return LocatedSeal::unsealed(
                Some(position),
                format!(
                    "file {position}, where the seal marker would be, is larger than any seal \
                     marker, so it is not one"
                ),
            )
        }
        crate::store::SealLocation::Absent => {
            return LocatedSeal::unsealed(
                None,
                format!(
                    "no seal marker reads at file {planned}, where File 0 says it is: the \
                     medium holds no file at all"
                ),
            )
        }
        crate::store::SealLocation::Unreadable { position, error } => {
            return LocatedSeal::unsealed(
                Some(position),
                format!("no seal marker reads at file {position}: {error}"),
            )
        }
    };
    let seal = match format::judge_seal(Ok(&bytes), front_index, front_index_position, position) {
        format::SealVerdict::Sealed(seal) => seal,
        verdict => {
            return LocatedSeal {
                verdict,
                position: Some(position),
                ended_after: None,
            }
        }
    };
    let planned_count = u64::from(planned) + 1;
    if position >= planned {
        // The planned position: only the full seal (rule 6). A 1.6.0 seal
        // states its own position (`file_count - 1`, rule S1), so one found
        // here that states another is the tape's two ends disagreeing.
        return match seal.ending(planned_count) {
            format::SealEnding::Full => LocatedSeal {
                verdict: format::SealVerdict::Sealed(seal),
                position: Some(position),
                ended_after: None,
            },
            _ => LocatedSeal::unsealed(
                Some(position),
                format!(
                    "the seal marker at file {position} states file_count = {}, but the plan \
                     (File 0) has {planned_count} files with the seal last: the tape's two ends \
                     disagree",
                    seal.file_count
                ),
            ),
        };
    }
    // Before the planned position: the short seal, on a tape that says it may
    // end early, and only one that says the same and counts the medium.
    let tape_requires_short_seal = match format::parse_id_thunk_claims(file0) {
        Ok(claims) => claims.requires.iter().any(|f| f == format::SHORT_SEAL),
        Err(_) => seal.requires_short_seal(),
    };
    if !tape_requires_short_seal {
        return LocatedSeal::unsealed(
            Some(position),
            format!(
                "the tape ends at file {position}, before the seal marker File 0 plans at file \
                 {planned}, and File 0 does not require \"{}\": a tape that does not say it may \
                 end early has no seal there",
                format::SHORT_SEAL
            ),
        );
    }
    if !seal.requires_short_seal() || i64::from(seal.file_count) != i64::from(position) + 1 {
        return LocatedSeal::unsealed(
            Some(position),
            format!(
                "the last file on the tape (file {position}) is a seal marker that does not seal \
                 a tape ending there (it states file_count = {}{}), so it is no short seal \
                 (volume-format-v2.md §1.2 rule 6)",
                seal.file_count,
                if seal.requires_short_seal() {
                    String::new()
                } else {
                    format!(" and does not require \"{}\"", format::SHORT_SEAL)
                }
            ),
        );
    }
    match seal.ending(planned_count) {
        format::SealEnding::Short { ended_after } => match u32::try_from(ended_after) {
            Ok(k) if k.checked_add(1) == Some(position) => LocatedSeal {
                verdict: format::SealVerdict::Sealed(seal),
                position: Some(position),
                ended_after: Some(k),
            },
            _ => LocatedSeal::unsealed(
                Some(position),
                format!(
                    "the seal marker at file {position} states ended_after = {ended_after}, \
                     which is not the file before it"
                ),
            ),
        },
        _ => LocatedSeal::unsealed(
            Some(position),
            format!(
                "the seal marker at file {position} states file_count = {}, which the plan \
                 (File 0, {planned_count} files) does not end short at",
                seal.file_count
            ),
        ),
    }
}

/// Read the seal marker (issue #458): located by `volume-format-v2.md` §1.2
/// rule 6 from File 0's `[layout]` pointer, and judged by
/// [`locate_and_judge_seal`] against the front index already in hand (`fi`,
/// as read, block padding included).
///
/// Every failure to read or to bind is [`format::SealVerdict::Unsealed`]: a
/// rebuild does not verify the tape (module doc), and a tape with no
/// readable seal marker is an unsealed one. A seal marker that names
/// another volume is not this tape's seal either. Only a seal this tapectl
/// must not interpret is [`format::SealVerdict::Refused`]. A short seal
/// (#420) comes back with its `ended_after`.
fn read_seal(
    store: &mut dyn Store,
    pointers: &format::IdThunkLayoutPointers,
    thunk_text: &str,
    fi: &[u8],
    label: &str,
) -> LocatedSeal {
    let Ok(planned) = u32::try_from(pointers.seal_marker) else {
        return LocatedSeal::unsealed(
            None,
            format!("File 0 names seal marker position {}", pointers.seal_marker),
        );
    };
    let located = locate_and_judge_seal(
        store,
        thunk_text,
        format::without_nul_padding(fi),
        pointers.front_index as u32,
        planned,
    );
    match located.verdict {
        format::SealVerdict::Sealed(seal) if seal.volume != label => LocatedSeal::unsealed(
            located.position,
            format!(
                "the seal marker at file {} names volume \"{}\", not this tape's \"{label}\"",
                located.position.unwrap_or(planned),
                seal.volume
            ),
        ),
        _ => located,
    }
}

/// What the tape says about when it was written: File 0's provenance (already
/// in hand) and the seal marker's `sealed_at`, which only a seal that bound
/// this tape's front index gives ([`read_seal`]). Every failure is an
/// absence: a File 0 without the keys rebuilds as it always did, with those
/// dates unknown.
fn tape_dates(
    thunk_text: &str,
    entries: &[format::ParsedIndexEntry],
    seal: &format::SealVerdict,
    // #420: a short seal's `ended_after`; the slices after it are not on
    // the tape, so they are not its data files.
    ended_after: Option<u32>,
) -> TapeDates {
    let provenance = format::parse_id_thunk_provenance(thunk_text).unwrap_or_default();
    let sealed_at = match seal {
        format::SealVerdict::Sealed(seal) => format::catalog_timestamp(&seal.sealed_at),
        _ => None,
    };
    TapeDates {
        written_at: provenance
            .created_at
            .as_deref()
            .and_then(format::catalog_timestamp),
        sealed_at,
        load_count_at_write: provenance.load_count_at_write,
        data_files: entries
            .iter()
            .filter(|e| {
                matches!(
                    crate::volume::layout_model::ZoneKind::from_type_label(&e.type_label),
                    Some(crate::volume::layout_model::ZoneKind::Slice { .. })
                ) && ended_after.is_none_or(|k| i64::from(e.position) <= i64::from(k))
            })
            .count() as i64,
    }
}

/// Whether a catalog `sealed_at` and the seal marker's agree to within
/// [`SEALED_AT_TOLERANCE_SECS`]. A value that does not parse as the
/// catalog's form disagrees: it is not a seal time this rebuild can vouch for.
fn sealed_at_agrees(recorded: &str, tape: &str) -> bool {
    let parse = |s: &str| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").ok();
    match (parse(recorded), parse(tape)) {
        (Some(a), Some(b)) => (a - b).num_seconds().abs() <= SEALED_AT_TOLERANCE_SECS,
        _ => false,
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
    dates: &TapeDates,
    operator: &EnvelopeManifest,
    tenant_of: &HashMap<String, String>,
    fallback_tenant: &str,
    backend_name: Option<&str>,
    supplement: &Supplement,
    report: &mut RebuildReport,
) -> Result<Inserted> {
    // Resolved the same way `cli::operations::volume_import` does, falling
    // back to the type string — never an invented name like "rebuilt", which
    // would put a backend in the catalog that no config declares.
    let backend_name = backend_name.unwrap_or("lto");
    #[allow(clippy::type_complexity)]
    let existing: Option<(i64, String, String, Option<String>, Option<i64>)> = tx
        .query_row(
            "SELECT id, status, observed_condition, sealed_at, ended_after
             FROM volumes WHERE label = ?1",
            params![label],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    // Issue #458: what this run read of the seal. Everything it inserts below
    // is `sealed`/`completed` only on a seal that bound the front index.
    let sealed = report.unsealed_reason.is_none();
    // #420: where a sealed tape ended. `None` is a full seal (or no seal).
    let ended_after: Option<i64> = report
        .ended_after
        .filter(|_| sealed)
        .and_then(|k| i64::try_from(k).ok());
    let volume_id = match existing {
        Some((id, status, condition, recorded_sealed_at, recorded_ended_after)) => {
            report.found_volume_status = Some(status.clone());
            // Issue #305: the third report-only fact. Never edited (#158).
            if let (Some(recorded), Some(tape)) = (&recorded_sealed_at, &dates.sealed_at) {
                if !sealed_at_agrees(recorded, tape) {
                    report.volume_sealed_at_mismatch = Some(recorded.clone());
                }
            }
            // #420: where the tape ended. A row with no seal recorded and no
            // ending of its own (the #360 lost write: `initialized`, its
            // seal never recorded) takes the tape's: `volume verify` reads
            // the seal at `ended_after + 1` (§1.2 rule 6, §5), and without
            // it the verify `volume resume`'s adoption needs could never
            // pass. That is a fact about the tape, not a status (#158). A
            // row that recorded its seal keeps its own ending, reported when
            // it disagrees.
            if sealed {
                match (recorded_sealed_at.is_some(), recorded_ended_after) {
                    (false, None) => {
                        if let Some(k) = ended_after {
                            tx.execute(
                                "UPDATE volumes SET ended_after = ?2
                                 WHERE id = ?1 AND sealed_at IS NULL AND ended_after IS NULL",
                                params![id, k],
                            )?;
                            report.ended_after_recorded = true;
                        }
                    }
                    (_, recorded) if recorded != ended_after => {
                        report.volume_ended_after_mismatch =
                            Some(recorded.map_or_else(|| "none".to_string(), |k| k.to_string()));
                    }
                    _ => {}
                }
            }
            // Same tape, same evidence — but a row this rebuild merely
            // FOUND is never edited (issue #158). `sealed`/`ok` is what the
            // `None` arm below would have inserted for a sealed tape, so
            // anything else is a fact worth surfacing: an imported `active`
            // row, for instance. Reported, not repaired — see
            // `RebuildReport::volume_status_mismatch`. (For an UNSEALED tape
            // the found row is the report's `found_volume_status` and
            // `unsealed_reason`; the two are not the same question.)
            if status != "sealed" {
                report.volume_status_mismatch = Some(status);
            }
            // Issue #242: the sibling check for `observed_condition` — most
            // often a `quarantined` one a failed `volume verify` produced
            // on purpose. Independent of the status check above: since the
            // 2026-09-17 amendment a verify-quarantined row stays `sealed`,
            // so only THIS check catches it.
            if condition != "ok" {
                report.volume_condition_mismatch = Some(condition);
            }
            id
        }
        None => {
            // Issue #305: dated by the tape, not by the rebuild. File 0's
            // `created_at` is when the write was planned — the closest the
            // medium comes to when it began, and before it the volume was
            // never written — so it stands for both `created_at` and
            // `first_write`. The seal marker's `sealed_at` is the tape's last
            // timestamp: the confirm that sets a live write's `last_write`
            // leaves no mark on the tape. With no date on the tape,
            // `created_at` keeps its default (now) and the rest stay NULL.
            //
            // `num_data_files` is exact: the front index lists every slice.
            // `bytes_written` is NOT reconstructed: the live writer stores
            // block-padded bytes and the write's block size is not on the
            // tape, so any figure here would depend on the rebuilding
            // machine (#305's coordinator note). It stays 0, as before.
            //
            // Issue #458: `sealed` only on a seal marker that was read and
            // bound the front index. Without one this is the catalog's own
            // shape for an aborted write: `initialized`, no seal recorded,
            // no data file proven (`num_data_files` 0 — the front index
            // lists the plan, not what was laid down).
            //
            // #420: a short seal's `ended_after` is recorded with the seal
            // (migration 038), and `num_data_files` counts only the slices
            // at or before it ([`tape_dates`]).
            tx.execute(
                "INSERT INTO volumes (label, uuid, backend_type, backend_name, media_type,
                                      capacity_bytes, mam_capacity_bytes, has_manifest, status,
                                      created_at, first_write, sealed_at, last_write,
                                      num_data_files, ended_after)
                 VALUES (?1, ?2, 'lto', ?3, ?4, ?5, ?6, 1, ?10,
                         COALESCE(?7, datetime('now')), ?7, ?8, ?8, ?9, ?11)",
                params![
                    label,
                    uuid,
                    backend_name,
                    meta.media_type,
                    meta.nominal_capacity_bytes,
                    meta.mam_capacity_bytes,
                    dates.written_at,
                    dates.sealed_at,
                    if sealed { dates.data_files } else { 0 },
                    if sealed { "sealed" } else { "initialized" },
                    ended_after,
                ],
            )?;
            report.volume_inserted = true;
            tx.last_insert_rowid()
        }
    };

    let mut sets = Vec::with_capacity(operator.units.len());
    let mut promoted: Vec<Promotion> = Vec::new();
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
        // #420 (`volume-format-v2.md` §4.2; layout-session.md "Later
        // contacts"): on a short tape a unit is on it whole only when every
        // slice lies at or before `ended_after`, as a passing short confirm
        // splits the rows; every other unit is cut. #478 (§4.4, ADR-0012
        // items 121 and 130): a block that states `may_span` splits three
        // ways instead, as confirm splits its row ([`tape_split`]).
        let written = tape_split(unit, sealed, ended_after);
        match written {
            Written::Cut => report
                .cut_units
                .push(format!("{} v{}", unit.name, unit.snapshot_version)),
            Written::Range {
                first_slice,
                last_slice,
            } => report.range_units.push(RebuiltRange {
                unit: unit.name.clone(),
                version: unit.snapshot_version,
                first_slice,
                last_slice,
                num_slices: unit.slices.len() as i64,
            }),
            Written::Completed | Written::Aborted => {}
        }

        let (snapshot_id, snapshot_inserted) =
            ensure_snapshot(tx, unit, unit_id, supplement, report)?;
        let stage_set_id = ensure_stage_set(tx, unit, snapshot_id, volume_id, supplement, report)?;

        let mut slice_ids = Vec::with_capacity(unit.slices.len());
        for slice in &unit.slices {
            slice_ids.push((slice, ensure_slice(tx, unit, slice, stage_set_id, report)?));
        }

        let write_id = ensure_write(
            tx,
            stage_set_id,
            snapshot_id,
            volume_id,
            written,
            unit.may_span,
            report,
        )?;
        for (slice, slice_id) in slice_ids {
            let laid_down = sealed && slice_on_tape(slice, ended_after);
            ensure_position(tx, write_id, slice_id, slice, laid_down, report)?;
        }
        // X24: after the positions, which are what the rule reads. The order
        // matters: the volume row is written at the top of this function, so
        // a tape this run records `sealed` is a sealed volume by the time the
        // rule is asked; a found row keeps the status it had (#158), and
        // counts only if that is `sealed`.
        if unit.may_span {
            promote_spanning_version(
                tx,
                unit,
                snapshot_id,
                stage_set_id,
                snapshot_inserted,
                report,
                &mut promoted,
            )?;
        }

        ensure_files(tx, unit, snapshot_id, supplement, report)?;
        sets.push(RebuiltSet {
            stage_set_id,
            unit_name: unit.name.clone(),
            unit_uuid: unit.uuid.clone(),
            version: unit.snapshot_version,
            may_span: unit.may_span,
            num_slices: unit.slices.len() as i64,
            held_here: unit
                .slices
                .iter()
                .filter(|s| sealed && slice_on_tape(s, ended_after))
                .map(|s| s.number)
                .collect(),
        });
    }

    Ok(Inserted {
        volume_id,
        sets,
        promoted,
    })
}

/// What [`insert_all`] settled: the volume's row and, for each manifest unit,
/// the stage set it landed on.
struct Inserted {
    volume_id: i64,
    sets: Vec<RebuiltSet>,
    /// Found snapshots item 128's rule made `current` (#478).
    promoted: Vec<Promotion>,
}

/// A stage set of the volume being rebuilt, with what names its catalogue
/// among the operator envelope's `catalogs/` members.
struct RebuiltSet {
    stage_set_id: i64,
    unit_name: String,
    /// The manifest's unit uuid — the one the writing catalogue named the
    /// catalogue file by (`<uuid8>_v<version>`), which a local unit matched
    /// by NAME may not share.
    unit_uuid: String,
    version: i64,
    /// The block states `may_span` (#478).
    may_span: bool,
    /// N: the slices the block lists.
    num_slices: i64,
    /// The slice numbers this tape holds, on a tape whose seal was read: the
    /// ones its rows record `written`.
    held_here: Vec<i64>,
}

/// The tapectl home the catalog lives in, as `<home>/tapectl.db`'s directory
/// — the rule `staging clean` uses for the lockfiles and its leftovers.
/// `None` for an in-memory catalog, which has no home.
fn catalog_home(conn: &Connection) -> Option<std::path::PathBuf> {
    let db = conn.path().filter(|p| !p.is_empty())?;
    let parent = Path::new(db).parent()?;
    Some(if parent.as_os_str().is_empty() {
        std::path::PathBuf::from(".")
    } else {
        parent.to_path_buf()
    })
}

/// Give each rebuilt stage set the dar catalogue its tape carried (issue
/// #473): the operator envelope's `catalogs/<uuid8>_v<N>.<k>.dar` members,
/// copied into `<home>/catalogs/<uuid8>/s<id>/` at 0600 (0700 directories)
/// and recorded in `stage_sets.catalog_path` as the stem `stage create`
/// records.
///
/// `catalog rebuild` used to leave `catalog_path` NULL and write nothing, so
/// a set brought back with `volume read-slices` and written to a second tape
/// had no catalogue in its envelopes — though that tape's RECOVERY.md still
/// says it does — and RESTORE.sh `--path` died on it.
///
/// A set that already has a catalogue on disk is left alone (never
/// overwritten: it may be a live set's own). A set the catalog knew before
/// and that still lacks one gets it now, which is how a second `catalog
/// rebuild` of the same tape heals a catalog rebuilt before this existed.
/// Sets still without one are named in `report.sets_without_catalogue`.
fn restore_catalogues(
    conn: &Connection,
    operator: &OpenedEnvelope,
    sets: &[RebuiltSet],
    report: &mut RebuildReport,
) {
    let home = catalog_home(conn);
    let paths = home.map(crate::config::TapectlPaths::new);
    let mut done: HashSet<i64> = HashSet::new();
    for set in sets {
        if !done.insert(set.stage_set_id) {
            continue;
        }
        let label = format!("{} v{}", set.unit_name, set.version);
        match place_catalogue(conn, operator, set, paths.as_ref()) {
            Ok(Placed::Restored) => report.catalogues_restored += 1,
            Ok(Placed::AlreadyThere) => {}
            Ok(Placed::NotOnTape) => report.sets_without_catalogue.push(label),
            Err(why) => {
                tracing::warn!(
                    unit = %set.unit_name,
                    version = set.version,
                    error = %why,
                    "rebuild: could not put the dar catalogue back; the set has none"
                );
                report.sets_without_catalogue.push(label);
            }
        }
    }
}

enum Placed {
    Restored,
    AlreadyThere,
    NotOnTape,
}

fn place_catalogue(
    conn: &Connection,
    operator: &OpenedEnvelope,
    set: &RebuiltSet,
    paths: Option<&crate::config::TapectlPaths>,
) -> Result<Placed> {
    let existing: Option<String> = conn
        .query_row(
            "SELECT catalog_path FROM stage_sets WHERE id = ?1",
            params![set.stage_set_id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    if existing.as_deref().is_some_and(envelope::catalogue_on_disk) {
        return Ok(Placed::AlreadyThere);
    }

    // The tape's name for this set's catalogue: `<uuid8>_v<version>`.
    let stem = format!(
        "{}_v{}",
        set.unit_uuid.get(..8).unwrap_or(&set.unit_uuid),
        set.version
    );
    let members: Vec<&envelope::ExtractedCatalogue> = operator
        .catalogues
        .iter()
        .filter(|c| envelope::catalogue_stem(&c.file_name) == Some(stem.as_str()))
        .collect();
    if members.is_empty() {
        return Ok(Placed::NotOnTape);
    }
    let Some(paths) = paths else {
        return Err(TapectlError::Other(
            "the catalog is in memory, so there is no tapectl home to put a catalogue in"
                .to_string(),
        ));
    };

    // The directory is named by the LOCAL unit's uuid, as `stage create`
    // and `staging clean` derive it.
    let local_uuid: String = conn.query_row(
        "SELECT u.uuid FROM stage_sets ss
         JOIN snapshots sn ON sn.id = ss.snapshot_id
         JOIN units u ON u.id = sn.unit_id
         WHERE ss.id = ?1",
        params![set.stage_set_id],
        |r| r.get(0),
    )?;
    let dir = crate::staging::stage_set_catalogue_dir(paths, &local_uuid, set.stage_set_id);
    // Named by the LOCAL unit's uuid too, not the tape's: the next copy's
    // MANIFEST.toml lists the local uuid, and its RECOVERY.md and RESTORE.sh
    // look for `catalogs/<first 8 of that uuid>_v<N>.1.dar`. The two differ
    // only for a unit this catalog already had under the same NAME with a
    // different uuid (`existing_unit`'s fallback). dar does not bind a
    // catalogue to its file name.
    let local_stem = format!(
        "{}_v{}",
        local_uuid.get(..8).unwrap_or(&local_uuid),
        set.version
    );
    let created = !dir.exists();
    let copied = (|| -> Result<()> {
        std::fs::create_dir_all(&dir)?;
        if let Some(unit_dir) = dir.parent() {
            crate::config::secure_path(unit_dir, 0o700);
        }
        crate::config::secure_path(&dir, 0o700);
        for member in &members {
            // `.<k>.dar`: the tape's stem is a prefix of every member name.
            let suffix = &member.file_name[stem.len()..];
            let to = dir.join(format!("{local_stem}{suffix}"));
            std::fs::copy(&member.path, &to)?;
            crate::config::secure_path(&to, 0o600);
            std::fs::File::open(&to)?.sync_all()?;
        }
        Ok(())
    })();
    if let Err(e) = copied {
        if created {
            let _ = std::fs::remove_dir_all(&dir);
        }
        return Err(e);
    }
    let catalog_path = dir.join(&local_stem);
    crate::db::busy::retry(
        crate::db::busy::BusyPolicy::DEFAULT,
        "the rebuilt stage set's catalogue",
        || {
            Ok(conn.execute(
                "UPDATE stage_sets SET catalog_path = ?1 WHERE id = ?2",
                params![catalog_path.to_string_lossy().to_string(), set.stage_set_id],
            )?)
        },
    )?;
    Ok(Placed::Restored)
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

/// The snapshot row of `unit`'s version, and whether this run inserted it.
fn ensure_snapshot(
    tx: &Connection,
    unit: &envelope::ManifestUnit,
    unit_id: i64,
    supplement: &Supplement,
    report: &mut RebuildReport,
) -> Result<(i64, bool)> {
    if let Some(id) = existing_id(
        tx,
        "SELECT id FROM snapshots WHERE unit_id = ?1 AND version = ?2",
        params![unit_id, unit.snapshot_version],
    )? {
        return Ok((id, false));
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
    //
    // #478 (ADR-0012 items 128 and 130, X24): a spannable set's version is
    // `current` only once every slice of it is on a sealed volume, which
    // only the positions inserted after this row can show. It is inserted
    // `staged`, as the writing catalog holds it before its tapes seal, and
    // [`promote_spanning_version`] makes it `current` when the rule holds.
    let status = if unit.may_span { "staged" } else { "current" };
    tx.execute(
        "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path,
                                total_size, file_count)
         VALUES (?1, ?2, ?3, ?7, ?4, ?5, ?6)",
        params![
            unit_id,
            unit.snapshot_version,
            snapshot_type,
            source_path,
            facts.and_then(|f| f.total_size),
            facts.and_then(|f| f.file_count),
            status,
        ],
    )?;
    report.snapshots += 1;
    Ok((tx.last_insert_rowid(), true))
}

/// Item 128's promotion, for a rebuild (`layout-session.md` "Promotion and
/// the retire floor"; ADR-0012 item 130, X24): a spanning version becomes
/// `current` once every slice of one of its stage sets is on a sealed
/// volume ([`crate::policy::coverage::every_slice_on_sealed_volume_sql`]).
///
/// Only volumes the catalog records `sealed` count. "The volume being
/// sealed counted" is confirm's and the adoption's, which seal it in the
/// same transaction; a rebuild never changes a volume's status, so its tape
/// counts only when this run inserted its row `sealed` (a fresh rebuild of a
/// sealed tape). A tape under a found `initialized` row (#360's lost write)
/// does not: its version stays `staged` until the adoption seals the volume
/// and promotes it. A spannable set held whole on a sealed volume meets the
/// rule, every slice being on it.
///
/// Asked of a snapshot this run inserted (`staged`) and of one it found,
/// which an earlier rebuild of another tape of the span left `staged`: the
/// rebuild of the span's last tape, in whichever order the tapes come,
/// promotes it, as the confirm of the last tape to seal did. Guarded as
/// confirm's update is: only from `created` or `staged`, and never a
/// demotion. A found row it promotes is reported
/// ([`RebuildReport::versions_promoted`]) and pushed to `promoted`, whose
/// status-change events are logged after the commit, as confirm logs its
/// own. The promotion of a snapshot the rebuild finds is ADR-0012 item 144:
/// upward only, by item 128's rule over volumes the catalog records sealed,
/// never a change to a volume's status.
fn promote_spanning_version(
    tx: &Connection,
    unit: &envelope::ManifestUnit,
    snapshot_id: i64,
    stage_set_id: i64,
    inserted: bool,
    report: &mut RebuildReport,
    promoted: &mut Vec<Promotion>,
) -> Result<()> {
    let before: Option<(String, String, i64)> = tx
        .query_row(
            "SELECT s.status, u.name, u.tenant_id
               FROM snapshots s JOIN units u ON u.id = s.unit_id WHERE s.id = ?1",
            params![snapshot_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let changed = tx.execute(
        &format!(
            "UPDATE snapshots SET status = 'current'
              WHERE id = ?1 AND status IN ('created', 'staged') AND {}",
            crate::policy::coverage::every_slice_on_sealed_volume_sql("?2", None)
        ),
        params![snapshot_id, stage_set_id],
    )?;
    if changed > 0 && !inserted {
        report
            .versions_promoted
            .push(format!("{} v{}", unit.name, unit.snapshot_version));
        if let Some((old_status, unit_name, tenant_id)) = before {
            promoted.push(Promotion {
                snapshot_id,
                old_status,
                unit_name,
                tenant_id,
            });
        }
    }
    Ok(())
}

/// A found snapshot [`promote_spanning_version`] made `current`, with what
/// its status-change event records.
struct Promotion {
    snapshot_id: i64,
    old_status: String,
    unit_name: String,
    tenant_id: i64,
}

fn ensure_stage_set(
    tx: &Connection,
    unit: &envelope::ManifestUnit,
    snapshot_id: i64,
    volume_id: i64,
    supplement: &Supplement,
    report: &mut RebuildReport,
) -> Result<i64> {
    // The manifest's `stage_set_id` is an id in the DB that WROTE the tape
    // and means nothing here; reusing it would collide with live rows. The
    // snapshot (unit uuid + version) narrows it to one Version, but a
    // Version can have several stage sets (issue #379): re-staged for a
    // later copy, each staging encrypts under fresh age file keys, so two
    // tapes carrying identical content carry different ciphertext. Keyed on
    // the snapshot alone, tape B's positions attached to tape A's slice rows
    // and tape B then read as corrupt against them.
    //
    // So the stage set is matched on evidence, in this order, and then
    // `ensure_slice` compares EVERY slice of it with the tape, refusing on
    // any mismatch, whichever slice it is:
    //
    // 1. This volume: a stage set of this Version the catalog already
    //    records as written to THIS volume. A volume is written once
    //    (ADR-0003), so that is the stage set on this tape -- whatever its
    //    slice rows now say.
    // 2. The ciphertext: a stage set of this Version holding ANY of this
    //    tape's slices (same number, same `sha256_encrypted`). Each staging
    //    encrypts under fresh file keys, so one shared hash proves the same
    //    stage set; a different staging shares none and gets its own.
    //
    // Matching on the first slice alone (as #379 first did) made a slice-1
    // disagreement miss the stage set, mint a sibling of the same Version
    // and record a second write of it to this volume, where a disagreement
    // on any later slice refused. Now the two refuse alike.
    let on_this_volume = existing_id(
        tx,
        "SELECT ss.id FROM stage_sets ss
         JOIN writes w ON w.stage_set_id = ss.id
         WHERE ss.snapshot_id = ?1 AND w.volume_id = ?2
         ORDER BY ss.id LIMIT 1",
        params![snapshot_id, volume_id],
    )?;
    if let Some(id) = on_this_volume {
        return Ok(id);
    }
    let mut slices: Vec<_> = unit.slices.iter().collect();
    slices.sort_by_key(|s| s.number);
    for slice in &slices {
        if let Some(id) = existing_id(
            tx,
            "SELECT ss.id FROM stage_sets ss
             JOIN stage_slices sl ON sl.stage_set_id = ss.id
             WHERE ss.snapshot_id = ?1 AND sl.slice_number = ?2 AND sl.sha256_encrypted = ?3
             ORDER BY ss.id LIMIT 1",
            params![snapshot_id, slice.number, slice.sha256_encrypted],
        )? {
            return Ok(id);
        }
    }
    // A unit with no slices carries no ciphertext to tell its stage sets
    // apart, and no slice row a mismatch could corrupt.
    if slices.is_empty() {
        if let Some(id) = existing_id(
            tx,
            "SELECT id FROM stage_sets WHERE snapshot_id = ?1 ORDER BY id LIMIT 1",
            params![snapshot_id],
        )? {
            return Ok(id);
        }
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
    unit: &envelope::ManifestUnit,
    slice: &envelope::ManifestSlice,
    stage_set_id: i64,
    report: &mut RebuildReport,
) -> Result<i64> {
    let existing: Option<(i64, String)> = tx
        .query_row(
            "SELECT id, sha256_encrypted FROM stage_slices
             WHERE stage_set_id = ?1 AND slice_number = ?2",
            params![stage_set_id, slice.number],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((id, sha256_encrypted)) = existing {
        // Issue #379: a slice row is reused only when it is the SAME
        // ciphertext. A matching number with a different hash means the
        // catalog and this tape disagree about what the stage set holds;
        // attaching the tape's position to the catalog's row would make the
        // tape read as corrupt against it. Refused, and the transaction
        // leaves the catalog as it was.
        if sha256_encrypted != slice.sha256_encrypted {
            return Err(TapectlError::Other(format!(
                "{} version {}, slice {} of stage set {stage_set_id}: the catalog records \
                 ciphertext sha256 {} but this tape carries {} — the catalog's stage set is \
                 not the one on this tape; the rebuild was rolled back",
                unit.name,
                unit.snapshot_version,
                slice.number,
                sha256_encrypted,
                slice.sha256_encrypted
            )));
        }
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

/// What a rebuilt `writes` row records of one unit's write on the tape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Written {
    /// The tape is sealed and holds the unit whole: `completed`.
    Completed,
    /// The tape is sealed and holds some, not all, of a spannable set's
    /// slices (#478, ADR-0012 items 121 and 130): `range`, the run from
    /// `first_slice` to `last_slice`. Only for a block that states
    /// `may_span`.
    Range { first_slice: i64, last_slice: i64 },
    /// The tape sealed short before the unit's last slice (#420), or, for a
    /// block that states `may_span`, holds none of its slices: `cut`.
    Cut,
    /// No seal marker sealed the tape (#458): `aborted`.
    Aborted,
}

/// Whether `slice` is on this tape (`volume-format-v2.md` §4.2 R3, §4.4 R6):
/// it has a `tape_position` (a range's slice planned on another tape has
/// none, ADR-0012 item 125), at or before a short seal's `ended_after`.
fn slice_on_tape(slice: &envelope::ManifestSlice, ended_after: Option<i64>) -> bool {
    slice
        .tape_position
        .is_some_and(|p| ended_after.is_none_or(|k| p <= k))
}

/// The row a sealed tape gives one manifest block, by the rule confirm
/// records (`layout-session.md` "Confirm of a short ending" and "A write
/// that may span"; `volume-format-v2.md` §4.2, §4.4).
///
/// - A tape with no seal it read: `aborted` (#458).
/// - Every slice of the block on the tape: `completed`. For a block that
///   states `may_span` that is every slice of the stage set, 1 to N (item
///   93's middle tape); a set with no slices is whole, as confirm's
///   `span_split` reads one.
/// - Otherwise a whole unit's block is `cut` (#420), and a spannable set's
///   is `range` when the tape holds some of its slices, the run from the
///   lowest to the highest of them, and `cut` when it holds none (X2).
fn tape_split(unit: &envelope::ManifestUnit, sealed: bool, ended_after: Option<i64>) -> Written {
    if !sealed {
        return Written::Aborted;
    }
    let held: Vec<i64> = unit
        .slices
        .iter()
        .filter(|s| slice_on_tape(s, ended_after))
        .map(|s| s.number)
        .collect();
    if held.len() == unit.slices.len() {
        return Written::Completed;
    }
    if !unit.may_span {
        return Written::Cut;
    }
    match (held.iter().min(), held.iter().max()) {
        (Some(&first_slice), Some(&last_slice)) => Written::Range {
            first_slice,
            last_slice,
        },
        _ => Written::Cut,
    }
}

fn ensure_write(
    tx: &Connection,
    stage_set_id: i64,
    snapshot_id: i64,
    volume_id: i64,
    written: Written,
    // The block's `may_span` (ADR-0012 item 121): frozen into the row as the
    // writer froze it, so a later write of the set reads it from the row.
    may_span: bool,
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
    //
    // Issue #458: `completed` is what a passing confirm sets after a seal,
    // and the copy counts read it. A tape whose seal marker was not read
    // gets the status of a write that did not finish: `aborted`, with no
    // completion time. `docs/design/layout-session.md`: an aborted session's
    // tape "is not a copy".
    //
    // #420 (ADR-0012 item 112): a unit the early end of a sealed tape cut is
    // `cut`, as a passing short confirm records it: never a copy, and with
    // no completion time either.
    //
    // #478 (item 121): a range is terminal like a completed write and gets
    // its completion time as confirm gives it one. It is not a copy on its
    // own: its slices count one by one (item 127).
    let (status, notes) = match written {
        Written::Completed => (
            "completed",
            "rebuilt from the volume itself; never verified by a read-back",
        ),
        Written::Range { .. } => (
            "range",
            "rebuilt from the volume itself; the tape holds a range of this spannable set's \
             slices, not the whole set, so the write is not a copy on its own; never verified \
             by a read-back",
        ),
        Written::Cut => (
            "cut",
            "rebuilt from the volume itself; the tape sealed short, ending before this unit's \
             last slice, so the write is recorded as cut and is not a copy",
        ),
        Written::Aborted => (
            "aborted",
            "rebuilt from the volume itself; the tape carries no valid seal marker, so the \
             write is recorded as aborted and is not a copy",
        ),
    };
    tx.execute(
        "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status, write_verified,
                             completed_at, notes, may_span)
         VALUES (?1, ?2, ?3, ?4, 0,
                 CASE WHEN ?4 IN ('completed', 'range') THEN datetime('now') END, ?5, ?6)",
        params![
            stage_set_id,
            snapshot_id,
            volume_id,
            status,
            notes,
            may_span
        ],
    )?;
    report.writes += 1;
    Ok(tx.last_insert_rowid())
}

/// `laid_down`: the slice is on the tape — the tape is sealed and, on a
/// short tape (#420), the slice lies at or before `ended_after`.
fn ensure_position(
    tx: &Connection,
    write_id: i64,
    stage_slice_id: i64,
    slice: &envelope::ManifestSlice,
    laid_down: bool,
    report: &mut RebuildReport,
) -> Result<()> {
    // A slice this tape does not plan (a range's slice on another tape,
    // ADR-0012 items 125, 130) has no position here, and so no row: the
    // column is NOT NULL, and a rebuild records only what the tape shows.
    let Some(position) = slice.tape_position else {
        return Ok(());
    };
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
    //
    // Issue #458: on a sealed tape every slice the manifest lists is on it
    // (the seal marker's presence means every file before it is present).
    // On an unsealed one the manifest and the front index are the PLAN —
    // envelopes precede the slices — so the position is recorded `pending`,
    // with nothing measured on the volume, not `written`. So is a position
    // after a short seal's `ended_after` (#420, §4.2): not on this tape,
    // `pending` as the session that wrote the tape left it.
    if laid_down {
        tx.execute(
            "INSERT INTO write_positions (write_id, stage_slice_id, position, status, written_at,
                                          sha256_on_volume)
             VALUES (?1, ?2, ?3, 'written', datetime('now'), ?4)",
            params![
                write_id,
                stage_slice_id,
                position.to_string(),
                slice.sha256_encrypted,
            ],
        )?;
    } else {
        tx.execute(
            "INSERT INTO write_positions (write_id, stage_slice_id, position, status)
             VALUES (?1, ?2, ?3, 'pending')",
            params![write_id, stage_slice_id, position.to_string()],
        )?;
    }
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
    let (Some((db, spans)), Some(&ontape_snapshot)) = (
        supplement.db.as_ref(),
        supplement
            .file_snapshot
            .get(&(unit.name.clone(), unit.snapshot_version)),
    ) else {
        return Ok(());
    };
    // `db::ontape_catalog` maps every generation's rows into the catalog's
    // own shape (migration 030), refusing, by row, a value it would have to
    // guess at; its `FileSpans::stream` doc lists what it refuses and the
    // one far-off-mtime exception.
    //
    // Generation 3 (tapectl 1.1.0 on) records each entry's kind and link
    // target, so the rebuilt rows keep them. Generations 1 and 2 record
    // only `is_directory` (#381): a symlink or special file on those tapes
    // is rebuilt as regular — dar's catalogue still holds its real type.
    //
    // HAZARD for rows rebuilt from such a tape: a rebuilt row says `Regular`
    // for what may be a FIFO, a device or a symlink, so whatever trusts the
    // kind treats it as a file with data. A reader that opens the source
    // path of every non-directory row (`kind <> 0`) to hash it or read it
    // blocks forever on a FIFO with no writer, and fails on a dangling
    // symlink or follows a live one out of the unit — where the walk's own
    // `Special`/`Symlink` kind would have told it to skip the row.
    //
    // Streamed (issue #413): one on-tape row at a time, straight into the
    // bulk insert, never the whole tape's rows in memory.
    let in_version = |e: TapectlError| {
        TapectlError::Other(format!("{} v{}: {e}", unit.name, unit.snapshot_version))
    };
    let written = spans.stream(db, ontape_snapshot, |entries| {
        files_db::insert_version(tx, snapshot_id, entries.map(|e| e.map_err(in_version)))
    })?;
    report.files += written;
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
    // The cartridge clause (issue #165): names what File 0's identity
    // resolved to, so the events row — a ledger of claims, ADR-0001 — records
    // WHICH cartridge this rebuild bound as plainly as it already records
    // which tenants/units/snapshots it inserted.
    let cartridge_clause = match (&report.cartridge_barcode, &report.unbound_reason) {
        (Some(barcode), _) if report.cartridge_registered => {
            format!("; registered and bound cartridge \"{barcode}\"")
        }
        (Some(barcode), _) if report.cartridge_bound => {
            format!("; bound to cartridge \"{barcode}\"")
        }
        (Some(barcode), _) => format!("; already on cartridge \"{barcode}\" (unchanged)"),
        (None, Some(reason)) => format!("; left unbound ({reason})"),
        (None, None) => String::new(),
    };
    // Issue #235: the units, not just the labels. The events row is a ledger
    // of claims (ADR-0001), and "unit X now has ZERO copies" is the claim a
    // displacement actually makes — an operator reading `report events` after
    // a disaster-recovery run should not have to re-derive it from a label.
    let displaced_clause = if report.displaced.is_empty() {
        String::new()
    } else {
        let each: Vec<String> = report
            .displaced
            .iter()
            .map(|d| {
                let units: Vec<String> = d
                    .units
                    .iter()
                    .map(|u| {
                        if u.other_copies == 0 {
                            format!(
                                "unit \"{}\" [{}] now has ZERO copies",
                                u.unit_name, u.unit_status
                            )
                        } else {
                            format!(
                                "unit \"{}\" [{}]: {} other copy/copies remain",
                                u.unit_name, u.unit_status, u.other_copies
                            )
                        }
                    })
                    .collect();
                if units.is_empty() {
                    d.label.clone()
                } else {
                    format!("{} ({})", d.label, units.join("; "))
                }
            })
            .collect();
        format!("; displaced from that cartridge: {}", each.join(", "))
    };

    // Issue #458: the claim the rows make, in the ledger. A rebuild that
    // asserted `sealed` says it read a seal; one that did not says so.
    let seal_clause = match &report.unsealed_reason {
        // #420: a short seal is a seal; the ledger says where the tape ended
        // and which units it cut.
        None => match report.ended_after {
            None => String::new(),
            Some(k) => format!(
                "; SEALED SHORT after file {k}{}{}",
                if report.cut_units.is_empty() {
                    String::new()
                } else {
                    format!(": cut (not a copy here) {}", report.cut_units.join(", "))
                },
                if report.ended_after_recorded {
                    "; ended_after recorded on the existing volume row"
                } else {
                    ""
                }
            ),
        },
        Some(why) => format!(
            "; NO VALID SEAL MARKER ({why}): {} — not a copy",
            if report.volume_inserted {
                "recorded as an unsealed volume, its writes aborted"
            } else {
                "the existing volume row was left alone, the writes this run added are aborted"
            }
        ),
    };
    // #478 (ADR-0012 items 121, 128 and 130): what the tape holds of each
    // spannable set, when it holds one. Absent for every other tape, whose
    // ledger line is what it was before 1.7.0.
    let mut span_clause = String::new();
    if !report.range_units.is_empty() {
        let ranges: Vec<String> = report.range_units.iter().map(|r| r.describe()).collect();
        span_clause.push_str(&format!(
            "; held as a range (not a copy on its own): {}",
            ranges.join(", ")
        ));
    }
    if !report.versions_promoted.is_empty() {
        span_clause.push_str(&format!(
            "; made current, every slice now on a sealed volume (ADR-0012 item 144): {}",
            report.versions_promoted.join(", ")
        ));
    }
    if !report.spans_in_progress.is_empty() {
        let spans: Vec<String> = report
            .spans_in_progress
            .iter()
            .map(|s| format!("{} v{} ({})", s.unit, s.version, s.describe_missing()))
            .collect();
        span_clause.push_str(&format!(
            "; spanning, staged until its other slices are rebuilt: {}",
            spans.join(", ")
        ));
    }
    let unreadable_clause = if report.unreadable_tenant_envelopes.is_empty() {
        String::new()
    } else {
        format!(
            "; {} tenant envelope(s) unreadable in every copy, rebuilt without them from the \
             operator envelope's catalog.db: {}",
            report.unreadable_tenant_envelopes.len(),
            report.unreadable_tenant_envelopes.join(" | ")
        )
    };
    let detail = format!(
        "catalog rebuild from volume {} (uuid {}) on {}: {} envelope(s) opened; \
         inserted {} tenant(s), {} unit(s), {} snapshot(s), {} stage set(s), {} slice(s), \
         {} write(s), {} position(s), {} file row(s); {} escrow receipt(s) from tape, {} attested; catalog.db {}{seal_clause}{span_clause}{unreadable_clause}{cartridge_clause}{displaced_clause}",
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
        report.attested,
        if report.had_catalog_db {
            "present"
        } else {
            "absent (a tape written before catalog.db rode on tape): no file index, no source paths"
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

// ── Cartridge identity (issue #165) ──────────────────────────────────────
//
// Everything below reconstructs the one thing `insert_all` never touched:
// which physical cartridge this tape is on. ADR-0012's ruling: "`catalog
// rebuild --from-volume` binds the cartridge it observed (from File 0's
// identity)". Deliberately its own resolution, not `binding::lookup_cartridge`
// or `binding::resolve_or_register_cartridge`: rebuild has no `--cartridge`
// to prefer or fall back from, and exactly ONE identity claim — read off the
// tape itself, tagged `mam` or `operator` by File 0's own
// `cartridge_identity_source` (issue #192). It shares
// `binding::mount_and_record` for everything after a row is settled on:
// displacement, the `cartridge_volumes` mount, ADR-0011 location
// inheritance, and the `in_use` transition are the one implementation
// ADR-0010 already established, and rebuild does not get a second copy.

/// What File 0's `[media]` table lets a rebuild claim about the cartridge —
/// classified once so every branch below reasons about ONE of three shapes
/// rather than re-deriving it from `Option<format::IdThunkMedia>` each time.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RebuildIdentity {
    /// A chip-reported serial. Proves which physical cartridge this is —
    /// established at WRITE time by a real MAM read (migration 014's
    /// write-once rule), so a rebuild trusts it without needing its own live
    /// drive access (ADR-0010's DR leniency: keys and no `backend add`).
    Mam(String),
    /// An operator-typed barcode. Corroborated through the catalog binding,
    /// never string-matched against it (ADR-0012) — `cartridge relabel` is
    /// legitimate and must not break this.
    Operator(String),
    /// No identity this rebuild can safely assume, with the reason so the
    /// operator-facing warning can say which. Never treated as `Mam`:
    /// `docs/design/volume-format-v2.md` §1.1 forbids defaulting an absent
    /// `cartridge_identity_source` to `"mam"`.
    Unknown(String),
}

/// Classify File 0's `[media]` claim. `media = None` covers both "no
/// `[media]` table at all" (pre-ADR-0010) and "the table was unparseable" —
/// `rebuild_from_store` folds `parse_id_thunk_media`'s `Err` into `None` via
/// `.ok()` before calling this, matching every other absence in this format
/// (`format.rs`'s fail-safe convention: absent/malformed is an `Err`, and the
/// caller decides what absence means).
fn classify_media(media: Option<&format::IdThunkMedia>) -> RebuildIdentity {
    let Some(media) = media else {
        return RebuildIdentity::Unknown(
            "this tape's File 0 carries no [media] table (written before ADR-0010)".to_string(),
        );
    };
    if media.cartridge_serial.is_empty() {
        return RebuildIdentity::Unknown(
            "this tape's File 0 [media] table records no cartridge identity \
             (cartridge_serial is empty)"
                .to_string(),
        );
    }
    match media.cartridge_identity_source.as_deref() {
        Some("mam") => RebuildIdentity::Mam(media.cartridge_serial.clone()),
        Some("operator") => RebuildIdentity::Operator(media.cartridge_serial.clone()),
        _ => RebuildIdentity::Unknown(format!(
            "this tape's File 0 records cartridge_serial \"{}\" but no \
             cartridge_identity_source (written by an older tapectl) — it cannot be told \
             apart from an operator-typed barcode, so it is not assumed to be a \
             chip-verified serial",
            media.cartridge_serial
        )),
    }
}

/// A resolved (found, or freshly registered) cartridge row, ready for
/// [`crate::volume::binding::mount_and_record`].
struct ResolvedCartridge {
    id: i64,
    barcode: String,
    prior_status: String,
    /// A serial match PROVES this is the right physical cartridge.
    ///
    /// True for a `mam` identity resolved by a `serial_number` the catalog
    /// ALREADY held (the row was found BY that serial, which was itself
    /// corroborated against any live drive read before the transaction
    /// opened) or freshly registered, and for an `operator` identity a live
    /// serial match SUPERSEDED.
    ///
    /// False for an `operator` identity resolved by barcode alone with no
    /// live serial to corroborate it against — and, since issue #214, for a
    /// `mam` identity resolved by the operator's unconfirmed CLAIM
    /// (`operator_serial`): that serial is learned by THIS contact, and a
    /// value written for the first time by the very contact now weighing a
    /// displacement witnesses nothing.
    witnessed: bool,
}

/// Resolve File 0's cartridge claim to a row (auto-registering when nothing
/// matches) and, on success, mount `volume_id` onto it via
/// [`crate::volume::binding::mount_and_record`] — the whole of issue #165
/// items 3 through 5.
///
/// Called inside `rebuild_from_store`'s transaction, after the `volumes`
/// INSERT: a volume that exists but is not bound, or a mount recorded for a
/// volume that was never created, are both worse than either change alone.
///
/// Never reads or writes `volumes.status` (item 5) — every status touched
/// here belongs to some OTHER volume (a displaced one, flipped to `erased`
/// by `mount_and_record`, exactly as `volume init` already does) or to the
/// bound `cartridges` row. The rebuilt volume's own status is #158's, landed
/// separately, and this function does not look at it.
#[allow(clippy::too_many_arguments)]
fn resolve_and_bind_cartridge(
    tx: &Connection,
    volume_id: i64,
    label: &str,
    identity: &RebuildIdentity,
    observed_serial: Option<&str>,
    meta: &format::IdThunkVolumeMeta,
    media: Option<&format::IdThunkMedia>,
    report: &mut RebuildReport,
) -> Result<()> {
    let unknown_reason = match identity {
        RebuildIdentity::Unknown(reason) => Some(reason.clone()),
        _ => None,
    };
    if let Some(reason) = unknown_reason {
        // The explicit trap this item calls out: a legacy File 0 must not
        // fail the rebuild. The volume `insert_all` just wrote stays
        // unbound; warn (via the report — `cli::catalog` prints it) and say
        // why. A later run that observes a real identity binds it, because
        // rebuild "only inserts what is missing" (this module's own "What it
        // deliberately does not do").
        report.unbound_reason = Some(reason);
        return Ok(());
    }

    let resolved = match identity {
        RebuildIdentity::Unknown(_) => unreachable!("handled above"),
        RebuildIdentity::Mam(serial) => resolve_mam_identity(tx, serial, meta, media, report)?,
        RebuildIdentity::Operator(barcode) => {
            resolve_operator_identity(tx, barcode, observed_serial, meta, media, report)?
        }
    };
    report.cartridge_barcode = Some(resolved.barcode.clone());

    // Idempotence (acceptance: "rebuilding the same volume twice changes
    // nothing the second time") and "never edits a row it finds" in one
    // check: does `volume_id` already carry an open mount?
    let already_mounted: Option<i64> = tx
        .query_row(
            "SELECT cartridge_id FROM cartridge_volumes
             WHERE volume_id = ?1 AND unmounted_at IS NULL",
            params![volume_id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(existing) = already_mounted {
        if existing != resolved.id {
            let existing_barcode: String = tx.query_row(
                "SELECT barcode FROM cartridges WHERE id = ?1",
                params![existing],
                |r| r.get(0),
            )?;
            return Err(TapectlError::Other(format!(
                "volume \"{label}\" is already bound to cartridge \"{existing_barcode}\" in \
                 this catalog, but this tape's own identity names cartridge \"{}\". `catalog \
                 rebuild` never edits a row it finds — resolve the mismatch by hand before \
                 re-running.",
                resolved.barcode
            )));
        }
        // Same cartridge: an idempotent re-run. Nothing left to do — and
        // nothing counted as newly written, matching `is_noop()`'s contract.
        return Ok(());
    }

    // The unwitnessed-displacement refusal (item 3), rebuild's own version of
    // `binding::refuse_unwitnessed_displacement` (issue #155's rule): that
    // function assumes a PRE-transaction caller with no self-mount to
    // exclude and its message says "re-run this init" — wrong on both counts
    // here, so this is its own query and its own wording, naming both
    // volumes.
    if !resolved.witnessed {
        let sql = format!(
            "SELECT v.label FROM cartridge_volumes cv
             JOIN volumes v ON v.id = cv.volume_id
             WHERE cv.cartridge_id = ?1 AND cv.unmounted_at IS NULL AND cv.volume_id != ?2
               AND {}",
            crate::policy::coverage::in_service("v")
        );
        let mut stmt = tx.prepare(&sql)?;
        let live: Vec<String> = stmt
            .query_map(params![resolved.id, volume_id], |r| r.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        if !live.is_empty() {
            let (is_are, volume_s) = if live.len() == 1 {
                ("is", "volume")
            } else {
                ("are", "volumes")
            };
            let labels = live
                .iter()
                .map(|l| format!("\"{l}\""))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(TapectlError::Other(format!(
                "wrong cartridge: volume \"{label}\"'s tape names cartridge \"{}\" by \
                 barcode, with no chip serial to prove it, but this catalog already binds \
                 \"{}\" to {volume_s} {labels}, which {is_are} still live. Say the bytes are \
                 gone first (`tapectl volume retire <label>` or `tapectl cartridge \
                 mark-erased {}`), or, if the registered row is a DIFFERENT physical \
                 cartridge that merely wears the same sticker, free the barcode with \
                 `tapectl cartridge relabel {} <new-barcode>` so this rebuild can \
                 register the tape's own. `catalog rebuild` has no flag that names a \
                 cartridge, so there is no third route. There is no --force for this — \
                 it is a fact tapectl cannot resolve on its own, not a risk to accept.",
                resolved.barcode, resolved.barcode, resolved.barcode, resolved.barcode
            )));
        }
    }

    let is_retired = resolved.prior_status == "retired_permanent";
    report.cartridge_retired = is_retired;

    // No live `MamInfo` here — rebuild has, at most, a bare medium serial
    // (`loaded_medium_serial`, only when a backend resolved); `default()`
    // means `mount_and_record`'s `total_load_count` COALESCE is a no-op,
    // preserving whatever the row already had rather than asserting a
    // reading nobody took.
    let mam = crate::tape::mam::MamInfo::default();
    let serial_for_mount = match identity {
        RebuildIdentity::Mam(s) => Some(s.as_str()),
        // The binding's `identity_source` must read `"operator"`, matching
        // what File 0 already claims — even when this contact separately
        // learned a serial onto the row. `mount_and_record` decides
        // `identity_source` from this argument alone.
        RebuildIdentity::Operator(_) => None,
        RebuildIdentity::Unknown(_) => unreachable!("handled above"),
    };
    // `update_status = !is_retired` (item 3's `retired_permanent` carve-out):
    // the mount is a physical fact, recorded either way, but no amount of
    // contact makes a medium declared permanently unfit fit again — never
    // call anything resembling `refuse_retired`, and never flip the status
    // back to `in_use` (ADR-0011).
    let outcome = crate::volume::binding::mount_and_record(
        tx,
        volume_id,
        resolved.id,
        &resolved.barcode,
        &resolved.prior_status,
        serial_for_mount,
        &mam,
        "catalog rebuild",
        !is_retired,
    )?;
    report.cartridge_bound = true;
    // Issue #235: the impacts travel with the label. This line used to be
    // `.map(|d| d.label)` — discarding, in the statement that received it,
    // the one fact ADR-0004 Tier 1 requires at an irreversible moment.
    // Rendering goes through the same `binding::render_displacement` that
    // `volume init` uses, so the two paths cannot diverge again.
    let now = chrono::Utc::now().naive_utc();
    report.displaced = outcome
        .displaced
        .iter()
        .map(|d| DisplacedVolume {
            label: d.label.clone(),
            units: d
                .impacts
                .iter()
                .map(|i| DisplacedUnit {
                    unit_name: i.unit_name.clone(),
                    unit_status: i.unit_status.clone(),
                    other_copies: i.other_copies,
                })
                .collect(),
            warning: crate::volume::binding::render_displacement(&resolved.barcode, d, now),
        })
        .collect();
    Ok(())
}

/// The capacity to record on a `cartridges` row this module auto-registers
/// (issue #210): the GENERATION TABLE's native figure for the medium File 0
/// names, never `meta.nominal_capacity_bytes` — that field is whatever
/// `media::resolve_capacity` decided at write time, drive
/// `capacity_override` first (ADR-0010 decision 3, issue #183). A
/// `capacity_override` sits ABOVE the cartridge row in that decision's
/// precedence ladder precisely so it can lie about ONE volume on ONE drive
/// (mhvtl's 2400 MB fiction); writing that resolved figure onto a brand-new
/// row would make the next `volume init` — on any drive, override or none —
/// read the drive's lie back as an operator's declaration. The row
/// describes the plastic — exactly the rule `binding.rs`'s own auto-register
/// arm states and applies for `volume init`; this is that same rule, applied
/// here because `binding.rs` is not the caller on this path.
///
/// `cartridges.nominal_capacity` is `NOT NULL` (`001_initial.sql`), so an
/// unparseable `media_type` cannot fall back to NULL as the issue's own text
/// suggests. In practice this branch is unreachable from any tape tapectl
/// itself wrote: File 0's `media_type` is always `Generation::as_str()`
/// (`write.rs`'s `generate_id_thunk_v2` call), which always round-trips
/// through `Generation::parse`. Only a hand-edited or foreign tape could
/// reach it, and refusing the whole rebuild over one unparseable string
/// would make a worse trade than recording the volume's own resolved figure
/// (the pre-#210 behavior) with a warning — the same "ignore and say so"
/// convention `write.rs`'s own generation resolution already uses for a
/// `media_type` it cannot arbitrate.
fn cartridge_capacity_bytes(meta: &format::IdThunkVolumeMeta) -> i64 {
    match crate::media::Generation::parse(&meta.media_type) {
        Some(g) => g.native_capacity_bytes() as i64,
        None => {
            // Warn and fall back rather than refuse (coordinator ruling,
            // issue #210). `cartridges.nominal_capacity` is INTEGER NOT NULL
            // so there is no "unknown" to record, and the alternatives are
            // worse on the path this runs on: refusing to register would
            // leave the volume UNBOUND, and an unbound rebuilt volume can
            // never be bound by a later run on any drive (issue #216) — a
            // permanent state traded for an advisory figure. The volume's
            // own `capacity_bytes`, which is what actually gates writes, is
            // decided at init and untouched here (ADR-0010 decision 3).
            //
            // Unreachable from any tapectl-written tape: `volume init`
            // always writes `media_type: generation.as_str()`, which always
            // round-trips through `Generation::parse`. Only a hand-edited or
            // foreign File 0 reaches this.
            tracing::warn!(
                media_type = %meta.media_type,
                "rebuild: File 0's media_type is not a recognised LTO generation, so the \
                 new cartridge row records this volume's resolved capacity instead of the \
                 generation-table figure. Correct it with \
                 `tapectl cartridge edit <barcode> --generation <G>`, which \
                 re-defaults the capacity when the stored figure was a table value"
            );
            meta.nominal_capacity_bytes
        }
    }
}

/// The `mam`-identity resolve, in three probes: find by `serial_number`;
/// else find by the operator's unconfirmed CLAIM, `operator_serial` (issue
/// #214); else, if nothing already wears S as a barcode, auto-register
/// with `barcode = serial_number = S` — the same convention
/// `binding::resolve_or_register_cartridge`'s auto-register uses, but never
/// calling it directly: that function only ever auto-registers FROM a
/// serial (never a barcode, and rebuild's operator path needs exactly that),
/// so sharing it for one identity and not the other would read as more
/// coupled than two short, obviously-parallel functions actually are.
fn resolve_mam_identity(
    tx: &Connection,
    serial: &str,
    meta: &format::IdThunkVolumeMeta,
    media: Option<&format::IdThunkMedia>,
    report: &mut RebuildReport,
) -> Result<ResolvedCartridge> {
    if let Some(row) = crate::volume::binding::select_cartridge(tx, "serial_number", serial)? {
        return Ok(ResolvedCartridge {
            id: row.id,
            barcode: row.barcode,
            prior_status: row.status,
            witnessed: true,
        });
    }

    // The operator's own CLAIM (issue #214), probed between the
    // chip-confirmed hit above and the barcode-collision refusal below.
    //
    // `resolve_mam_identity` was the ONE of `record_medium_serial`'s call
    // paths that could not find a row by operator claim at all:
    // `binding::lookup_cartridge` (issue #197's own work),
    // `binding::corroborate_contact` and `resolve_operator_identity` all
    // promote a claim once they have FOUND the row, while this function
    // consulted `serial_number` and then `barcode` and nothing else. That
    // asymmetry is what made the refusal below a dead end on the one path an
    // heir has — its remediation named a command ADR-0003 structurally
    // refuses on the sealed tape a rebuild is by construction reading. This
    // arm closes it, and is what makes that refusal's new recipe executable.
    //
    // Ordered BEFORE the collision check deliberately, not incidentally: it
    // mirrors `binding.rs`, where the claim lookup in `lookup_cartridge`
    // likewise precedes `bind_cartridge`'s barcode-collision arm. A row that
    // CLAIMS this serial is a positive identification; a row merely wearing
    // it as a sticker is not.
    let claimants = crate::volume::binding::select_cartridges_by_operator_serial(tx, serial)?;
    match claimants.len() {
        // Nothing claims it — fall through to the collision refusal below.
        0 => {}
        1 => {
            let row = claimants
                .into_iter()
                .next()
                .expect("length checked to be exactly one");
            // `record_medium_serial` is the one writer of `serial_number`,
            // and the one place the confirm/contradict rule lives. It cannot
            // contradict here (this row was SELECTed BY a claim equal to
            // `serial`), but it is still the only way this column is written
            // — which is exactly what keeps that rule structural.
            crate::volume::binding::record_medium_serial(tx, row.id, &row.barcode, serial)?;
            report.serial_learned = true;
            return Ok(ResolvedCartridge {
                id: row.id,
                barcode: row.barcode,
                prior_status: row.status,
                // NOT `true`, unlike the `serial_number` arm above. Computed
                // the way `resolve_operator_identity` computes it and for the
                // same stated reason: this serial was recorded for the FIRST
                // TIME by this very contact, and proves nothing about a
                // displacement this same contact is about to consider. The
                // arm above may say `true` because the row was found BY a
                // serial the catalog already held; this row was found by an
                // unconfirmed claim.
                witnessed: false,
            });
        }
        _ => {
            // `operator_serial` carries no UNIQUE index, so two hand-typed
            // claims can name one serial. Same shape `lookup_cartridge`
            // raises — count, barcodes, name one — except that `catalog
            // rebuild` has no flag for naming one, so the way out is to
            // correct the claims instead.
            let barcodes: Vec<&str> = claimants.iter().map(|r| r.barcode.as_str()).collect();
            return Err(TapectlError::Other(format!(
                "this tape's File 0 reports medium serial {serial}, which matches no \
                 registered cartridge's serial_number — and matches the operator-claimed \
                 serial of {} pre-registered cartridges ({}), none of them chip-confirmed \
                 yet. `catalog rebuild` cannot tell which one holds this tape, and has no \
                 flag for being told.\n\
                 \n\
                 Correct the claim on each row that is NOT this cartridge, leaving exactly \
                 one claiming {serial}, then re-run this rebuild:\n    \
                 tapectl cartridge edit <barcode> --serial <that cartridge's own serial>",
                claimants.len(),
                barcodes.join(", ")
            )));
        }
    }

    // Same collision `bind_cartridge`'s own auto-register arm refuses: a row
    // already registered under barcode = S (an operator who hand-registered
    // a cartridge using the serial printed on its shell, before it was ever
    // loaded) and NOT claiming S above. Refuse by name rather than surface
    // the raw UNIQUE constraint failure, or silently adopt a row that might
    // be a different physical cartridge.
    if crate::volume::binding::select_cartridge(tx, "barcode", serial)?.is_some() {
        return Err(TapectlError::Other(format!(
            "this tape's File 0 reports medium serial {serial}, which matches no registered \
             cartridge's serial_number — but a cartridge is already registered under the \
             barcode \"{serial}\". `catalog rebuild` cannot tell whether that is this same \
             physical cartridge, registered by hand before it was ever loaded, or a \
             different one whose sticker happens to read the same. It will not guess.\n\
             \n\
             If it IS this cartridge, record that on the row, then re-run this rebuild:\n    \
             tapectl cartridge edit \"{serial}\" --serial {serial}\n\
             That writes the OPERATOR's claim (`operator_serial`) — the only serial column \
             an operator command may write — and this rebuild then finds the row by that \
             claim and lets the chip's own reading confirm it. Nothing need be written to \
             the tape: it is already sealed, and a fresh-write contact with a sealed tape \
             is refused regardless of --force (ADR-0003).\n\
             \n\
             If it is a DIFFERENT cartridge, give the registered one a barcode of its own:\n    \
             tapectl cartridge relabel {serial} <new-barcode>"
        )));
    }

    let manufacturer = media.and_then(|m| m.cartridge_manufacturer.as_deref());
    let length = media.and_then(|m| m.tape_length_meters);
    // `total_load_count` bound explicitly to `NULL`, not left to the
    // schema's `DEFAULT 0` (issue #184): rebuild has no live load-count
    // reading at all, and the schema default would assert a false "zero
    // loads" rather than "never observed".
    tx.execute(
        "INSERT INTO cartridges
            (barcode, media_type, manufacturer, serial_number, tape_length_meters,
             nominal_capacity, status, total_load_count)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'in_use', NULL)",
        params![
            serial,
            meta.media_type,
            manufacturer,
            serial,
            length,
            cartridge_capacity_bytes(meta),
        ],
    )?;
    let id = tx.last_insert_rowid();
    crate::db::events::log_created(tx, "cartridge", id, serial, None)?;
    report.cartridge_registered = true;
    Ok(ResolvedCartridge {
        id,
        barcode: serial.to_string(),
        prior_status: "in_use".to_string(),
        witnessed: true,
    })
}

/// The `operator`-identity resolve: a serial this contact separately
/// observed (if any) wins over the barcode — exactly the priority
/// `binding::lookup_cartridge` gives `--cartridge` at `volume init`: the
/// serial is read off THIS medium, while the barcode is a sticker File 0
/// recorded at write time and may since have been relabelled (ADR-0012).
fn resolve_operator_identity(
    tx: &Connection,
    barcode: &str,
    observed_serial: Option<&str>,
    meta: &format::IdThunkVolumeMeta,
    media: Option<&format::IdThunkMedia>,
    report: &mut RebuildReport,
) -> Result<ResolvedCartridge> {
    if let Some(observed) = observed_serial {
        if let Some(row) = crate::volume::binding::select_cartridge(tx, "serial_number", observed)?
        {
            // ADR-0012: "the loaded tape *is* that other cartridge" — the
            // medium's own serial outranks a sticker File 0 merely recorded.
            if row.barcode != barcode {
                report.cartridge_barcode_superseded = Some(barcode.to_string());
            }
            return Ok(ResolvedCartridge {
                id: row.id,
                barcode: row.barcode,
                prior_status: row.status,
                witnessed: true,
            });
        }
    }

    match crate::volume::binding::select_cartridge(tx, "barcode", barcode)? {
        Some(row) => {
            if let (Some(recorded), Some(observed)) = (&row.serial_number, observed_serial) {
                if recorded != observed {
                    return Err(TapectlError::Other(format!(
                        "wrong cartridge: this tape's File 0 names cartridge \"{barcode}\" by \
                         barcode, but this catalog records \"{barcode}\" as medium serial \
                         {recorded}, and the drive holds {observed}. Another cartridge is \
                         wearing \"{barcode}\"'s sticker. There is no --force for this — it \
                         is a fact tapectl cannot resolve on its own, not a risk to accept."
                    )));
                }
            }
            // Computed from the row as SELECTed, before any learning below —
            // a serial recorded FOR THE FIRST TIME by this very contact
            // proves nothing about a displacement this same contact is about
            // to consider (item 3: "a row whose serial was NULL ... proves
            // nothing about it").
            let witnessed = matches!(
                (&row.serial_number, observed_serial),
                (Some(r), Some(o)) if r == o
            );
            if row.serial_number.is_none() {
                if let Some(observed) = observed_serial {
                    crate::volume::binding::record_medium_serial(
                        tx,
                        row.id,
                        &row.barcode,
                        observed,
                    )?;
                    report.serial_learned = true;
                }
            }
            Ok(ResolvedCartridge {
                id: row.id,
                barcode: row.barcode,
                prior_status: row.status,
                witnessed,
            })
        }
        None => {
            // `total_load_count` explicit `NULL`, same reasoning as
            // `resolve_mam_identity`'s auto-register arm.
            tx.execute(
                "INSERT INTO cartridges
                    (barcode, media_type, manufacturer, serial_number, tape_length_meters,
                     nominal_capacity, status, total_load_count)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'in_use', NULL)",
                params![
                    barcode,
                    meta.media_type,
                    media.and_then(|m| m.cartridge_manufacturer.as_deref()),
                    observed_serial,
                    media.and_then(|m| m.tape_length_meters),
                    cartridge_capacity_bytes(meta),
                ],
            )?;
            let id = tx.last_insert_rowid();
            crate::db::events::log_created(tx, "cartridge", id, barcode, None)?;
            report.cartridge_registered = true;
            Ok(ResolvedCartridge {
                id,
                barcode: barcode.to_string(),
                prior_status: "in_use".to_string(),
                witnessed: true,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Item 107: the tenant-key refusal names the `--key` files that were
    /// given — one in the singular, several in the plural — and keeps the
    /// heir recipe after it; any other error, and a call with no key, is
    /// returned untouched.
    #[test]
    fn the_wrong_key_refusal_names_the_key_files_given() {
        use crate::crypto::keys::KeyFile;
        let dir = tempfile::tempdir().unwrap();
        let mut given = Vec::new();
        for name in ["stranger-a.key", "stranger-b.key"] {
            let path = dir.path().join(name);
            std::fs::write(
                &path,
                format!("{}\n", crate::crypto::keys::generate_keypair().secret_key),
            )
            .unwrap();
            given.push(KeyFile::read(&path).unwrap());
        }
        let refusal = || TapectlError::Other(tenant_key_refusal_message());

        let one = name_keys_given(refusal(), &given[..1]).to_string();
        assert!(one.contains("stranger-a.key"), "{one}");
        assert!(one.contains("cannot open the operator envelope"), "{one}");
        assert!(
            one.contains("neither an operator key nor the escrow key"),
            "{one}"
        );
        assert!(!one.contains("this key"), "{one}");
        assert!(one.contains("RESTORE.sh"), "the heir recipe stays: {one}");

        let two = name_keys_given(refusal(), &given).to_string();
        assert!(
            two.contains("stranger-a.key") && two.contains("stranger-b.key"),
            "{two}"
        );
        assert!(two.contains("none of the key files given"), "{two}");
        assert!(two.contains("RESTORE.sh"), "{two}");

        // Untouched: no key given (the library's own callers), another error.
        assert_eq!(
            name_keys_given(refusal(), &[]).to_string(),
            tenant_key_refusal_message()
        );
        let other = name_keys_given(TapectlError::Other("tape unreadable".into()), &given);
        assert_eq!(other.to_string(), "tape unreadable");
    }

    /// The assumption attestation rests on: age unwraps the file key from
    /// the header alone, so a bounded prefix of a large ciphertext is enough
    /// to tell "this identity is a recipient" from "it is not". If a future
    /// age release read payload before deciding, this is the test that
    /// would say so.
    #[test]
    fn an_age_header_prefix_is_enough_to_test_a_recipient() {
        let right = crate::crypto::keys::generate_keypair();
        let wrong = crate::crypto::keys::generate_keypair();
        let plaintext = vec![7u8; 1024 * 1024];
        let ct = crate::staging::encrypt_data(&plaintext, std::slice::from_ref(&right.public_key))
            .unwrap();
        assert!(ct.len() > 4096);
        let head = ct[..4096].to_vec();

        let right_id: age::x25519::Identity = right.secret_key.parse().unwrap();
        let wrong_id: age::x25519::Identity = wrong.secret_key.parse().unwrap();

        let ok = age::Decryptor::new(Cursor::new(head.clone()))
            .and_then(|d| d.decrypt(std::iter::once(&right_id as &dyn age::Identity)));
        assert!(
            ok.is_ok(),
            "the right identity must unwrap from the header alone"
        );

        let no = age::Decryptor::new(Cursor::new(head))
            .and_then(|d| d.decrypt(std::iter::once(&wrong_id as &dyn age::Identity)));
        assert!(
            matches!(no, Err(age::DecryptError::NoMatchingKeys)),
            "the wrong identity must be refused at the header: {:?}",
            no.as_ref().err()
        );
    }

    /// The File 0 `[volume]` meta every `resolve_mam_identity` drill below
    /// needs — none of them look at it, but the auto-register arm binds it
    /// into the `cartridges` INSERT, so it has to be real.
    fn mam_drill_meta() -> format::IdThunkVolumeMeta {
        format::IdThunkVolumeMeta {
            media_type: "LTO-6".to_string(),
            nominal_capacity_bytes: 2_500_000_000_000,
            mam_capacity_bytes: 2_500_000_000_000,
        }
    }

    /// Issue #236 finding 1: `attest_escrow` leaves a stage set unattested
    /// on THREE different arms -- a slice this key demonstrably is NOT a
    /// recipient of (`NoMatchingKeys`, the permanent Gap statement), a slice
    /// whose header could not be READ at all (an I/O error -- Unknown, not
    /// Gap: the bytes may be perfectly covered and merely unreadable at this
    /// position), and a slice whose header did not PARSE as age at all (also
    /// Unknown). The pre-fix code counted only `report.attested` and folded
    /// all three failure arms into nothing more specific than "not attested
    /// this run" -- `cli::catalog` then asserted the Gap sentence for all
    /// three. This proves the three counters this fix adds move
    /// independently, one per arm, on one run.
    #[test]
    fn attest_escrow_counts_each_unattested_arm_separately() {
        let conn = crate::db::open_memory().unwrap();
        let escrow = crate::crypto::keys::generate_keypair();
        let other = crate::crypto::keys::generate_keypair();
        let op_id = crate::db::queries::insert_tenant(&conn, "op", None, true).unwrap();
        crate::db::queries::insert_escrow_key(
            &conn,
            op_id,
            "escrow",
            "fp",
            &escrow.public_key,
            None,
        )
        .unwrap();

        let slice_at = |position: i64| envelope::ManifestSlice {
            number: 1,
            tape_position: Some(position),
            size_bytes: 100,
            encrypted_bytes: 200,
            sha256_plain: "a".repeat(64),
            sha256_encrypted: "b".repeat(64),
        };
        let unit_at = |name: &str, position: i64| envelope::ManifestUnit {
            name: name.to_string(),
            uuid: format!("{name}-uuid"),
            snapshot_version: 1,
            stage_set_id: 0,
            may_span: false,
            dar_version: None,
            dar_command: None,
            slices: vec![slice_at(position)],
        };
        let manifest = EnvelopeManifest {
            manifest: envelope::ManifestHeader {
                volume: "TEST-VOL".to_string(),
                tenant: "operator".to_string(),
                created_at: "2026-01-01T00:00:00Z".to_string(),
            },
            units: vec![
                unit_at("attested-unit", 0),
                unit_at("gap-unit", 1),
                unit_at("garbage-unit", 2),
                unit_at("unreadable-unit", 99), // never written to the store
            ],
        };

        let tx = conn.unchecked_transaction().unwrap();
        let mut report = RebuildReport::default();
        insert_all(
            &tx,
            "TEST-VOL",
            "vol-uuid",
            &mam_drill_meta(),
            &TapeDates::default(),
            &manifest,
            &HashMap::new(),
            "recovered",
            None,
            &Supplement::default(),
            &mut report,
        )
        .unwrap();

        let mut store = crate::store::MemStore::new(512 * 1024);
        // Position 0: encrypted to the escrow recipient -- attests.
        let escrow_ct =
            crate::staging::encrypt_data(b"payload", std::slice::from_ref(&escrow.public_key))
                .unwrap();
        store
            .execute(
                &mut Cursor::new(escrow_ct.clone()),
                escrow_ct.len() as u64,
                false,
            )
            .unwrap();
        // Position 1: a real age header, but for a DIFFERENT recipient --
        // NoMatchingKeys, the permanent Gap arm.
        let other_ct =
            crate::staging::encrypt_data(b"payload", std::slice::from_ref(&other.public_key))
                .unwrap();
        store
            .execute(
                &mut Cursor::new(other_ct.clone()),
                other_ct.len() as u64,
                false,
            )
            .unwrap();
        // Position 2: not an age file at all -- the header fails to parse.
        let garbage = b"not an age file at all".to_vec();
        store
            .execute(
                &mut Cursor::new(garbage.clone()),
                garbage.len() as u64,
                false,
            )
            .unwrap();
        // Position 99 is never written -- `read_file_head` errors outright.

        let escrow_id: age::x25519::Identity = escrow.secret_key.parse().unwrap();
        attest_escrow(&tx, &mut store, &[escrow_id], &manifest, &mut report).unwrap();

        assert_eq!(report.attested, 1, "the escrow-recipient slice must attest");
        assert_eq!(
            report.escrow_attest_not_recipient, 1,
            "a slice encrypted to someone else is the permanent Gap arm"
        );
        assert_eq!(
            report.escrow_attest_unreadable, 1,
            "a slice this store cannot read at all is Unknown, not Gap"
        );
        assert_eq!(
            report.escrow_attest_unparseable, 1,
            "a slice whose header will not parse is Unknown, not Gap"
        );
    }

    /// The post-disaster shape issue #214 is about, built DIRECTLY rather
    /// than through `volume init`: a fresh catalog in which the heir
    /// hand-registered a cartridge under the serial printed on its shell
    /// (`barcode = S`), nothing has ever chip-confirmed it
    /// (`serial_number IS NULL`), and `operator_serial` carries whatever
    /// claim (if any) has been recorded since.
    ///
    /// Deliberately NOT built by the route the issue's prose sketches
    /// ("hand-register barcode = S, then `volume init` binds it by MAM"):
    /// that sequence is refused EARLIER, by `binding.rs`'s own
    /// barcode-collision check in `bind_cartridge`'s auto-register arm, so a
    /// test following it would never reach `resolve_mam_identity` at all and
    /// would pass against unfixed code.
    fn hand_registered(conn: &Connection, barcode: &str, operator_serial: Option<&str>) -> i64 {
        conn.execute(
            "INSERT INTO cartridges
                (barcode, media_type, nominal_capacity, serial_number, operator_serial, status)
             VALUES (?1, 'LTO-6', 2500000000000, NULL, ?2, 'available')",
            params![barcode, operator_serial],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn chip_serial(conn: &Connection, id: i64) -> Option<String> {
        conn.query_row(
            "SELECT serial_number FROM cartridges WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// Issue #214: with NOTHING claiming this serial the refusal stands —
    /// but it must name a command that can actually RUN. `volume init` is
    /// structurally refused on the tape a rebuild is reading (it is sealed
    /// by construction, and `decide_fresh_write_contact` refuses an
    /// `AlreadySealed` contact regardless of `--force`, ADR-0003), so the
    /// substring must not appear.
    /// The tenant-key refusal at `open_all_envelopes` is the HEIR's recipe:
    /// the operator key is gone, and this two-line message is how a tenant
    /// gets their own data back without a catalog. Both lines were wrong
    /// (issue #214's sibling sweep): `restore raw-volume` takes `--to`, not
    /// `--dest`, and `raw.rs` names the dumped file from the zone's own
    /// `type_label`, which is `restore_sh` — so `0002_restore_script.bin`
    /// never existed. A recipe read mid-disaster failed at step one.
    ///
    /// Derived, not transcribed: the filename is built from
    /// `ZoneKind::RestoreSh.type_label()` and the same `{:04}_{}.bin` shape
    /// `raw.rs` uses, so renaming the zone breaks this test rather than
    /// silently re-breaking the heir.
    #[test]
    fn the_tenant_key_refusal_names_the_file_raw_volume_actually_writes() {
        let label = crate::volume::layout_model::ZoneKind::RestoreSh.type_label();
        let expected = format!("{:04}_{}.bin", 2, label);

        let err = super::tenant_key_refusal_message();

        assert!(
            err.contains(&expected),
            "the heir recipe must name the file `restore raw-volume` writes \
             ({expected}), got: {err}"
        );
        assert!(
            !err.contains("0002_restore_script.bin"),
            "the stale filename must not come back: {err}"
        );
        assert!(
            err.contains("--to DIR"),
            "`restore raw-volume` takes --to, not --dest: {err}"
        );
        assert!(
            !err.contains("--dest"),
            "--dest is not a flag on any restore subcommand: {err}"
        );
        // Issue #417: the recipe fetches RESTORE.sh alone, not the whole
        // tape, and names File 1's no-tapectl route.
        assert!(
            err.contains("--only restore_sh"),
            "the recipe must dump only RESTORE.sh: {err}"
        );
        assert!(err.contains("fsf 2"), "File 1's mt/dd route: {err}");
        assert!(
            crate::volume::raw::RAW_TYPES.contains(&label),
            "--only accepts the label the recipe names"
        );
    }

    /// Issue #459 item 1: the recipe's second command restored into the
    /// directory its first had dumped RESTORE.sh into, and RESTORE.sh's
    /// `check_destination` (#405) refuses a `--to` that is not empty before
    /// it reads the tape — so the heir's recipe failed at its second step.
    /// The two commands are parsed out of the message, so rewording it cannot
    /// quietly bring the collision back.
    #[test]
    fn the_tenant_key_recipe_restores_into_a_different_directory_than_it_dumped_into() {
        let msg = super::tenant_key_refusal_message();
        let line = |starts: &str| -> String {
            msg.lines()
                .map(str::trim)
                .find(|l| l.starts_with(starts))
                .unwrap_or_else(|| panic!("no `{starts}` command in: {msg}"))
                .to_string()
        };
        let arg_after = |cmd: &str, flag: &str| -> String {
            let mut words = cmd.split_whitespace();
            words
                .find(|w| *w == flag)
                .unwrap_or_else(|| panic!("{cmd}: no {flag}"));
            words
                .next()
                .unwrap_or_else(|| panic!("{cmd}: {flag} has no value"))
                .to_string()
        };
        let dump = line("tapectl restore raw-volume");
        let restore = line("bash ");
        let dumped_into = arg_after(&dump, "--to");
        let restored_into = arg_after(&restore, "--to");
        assert_ne!(
            dumped_into, restored_into,
            "RESTORE.sh refuses a non-empty --to; the script sits in the dump directory:\n{msg}"
        );
        // The script the second command runs lives in the first's directory,
        // so the destination is not the directory that holds it.
        let script = restore.split_whitespace().nth(1).unwrap();
        let script_dir = std::path::Path::new(script).parent().unwrap();
        assert_eq!(script_dir, std::path::Path::new(&dumped_into));
        assert_ne!(std::path::Path::new(&restored_into), script_dir);
    }

    #[test]
    fn mam_identity_collision_names_cartridge_edit_not_volume_init() {
        let conn = crate::db::open_memory().unwrap();
        hand_registered(&conn, "HU1234ABCD", None);

        let mut report = RebuildReport::default();
        let err = resolve_mam_identity(&conn, "HU1234ABCD", &mam_drill_meta(), None, &mut report);
        let err = match err {
            Ok(r) => panic!(
                "expected a refusal, but it resolved to cartridge \"{}\" (id {})",
                r.barcode, r.id
            ),
            Err(e) => e.to_string(),
        };

        assert!(
            err.contains("tapectl cartridge edit"),
            "the refusal must name the command that actually repairs this: {err}"
        );
        assert!(
            !err.contains("volume init"),
            "`volume init` is refused on the sealed tape being rebuilt from (ADR-0003), \
             so naming it leaves the heir with no executable step: {err}"
        );
        assert!(
            err.contains("tapectl cartridge relabel"),
            "the different-cartridge branch must survive: {err}"
        );
    }

    /// Issue #214: once the operator's claim is on the row,
    /// `resolve_mam_identity` finds it and promotes the claim to
    /// chip-confirmed — the arm that makes the refusal above a repairable
    /// state rather than a dead end.
    ///
    /// `witnessed` must be FALSE: the serial was learned by THIS contact, so
    /// it proves nothing about a displacement this same contact is about to
    /// consider (the rule `resolve_operator_identity` already states).
    #[test]
    fn mam_identity_binds_a_row_whose_operator_claim_names_this_serial() {
        let conn = crate::db::open_memory().unwrap();
        let id = hand_registered(&conn, "HU1234ABCD", Some("HU1234ABCD"));

        let mut report = RebuildReport::default();
        let resolved =
            resolve_mam_identity(&conn, "HU1234ABCD", &mam_drill_meta(), None, &mut report)
                .unwrap();

        assert_eq!(resolved.id, id, "it must adopt the hand-registered row");
        assert_eq!(resolved.barcode, "HU1234ABCD");
        assert!(
            !resolved.witnessed,
            "a serial learned by THIS contact witnesses nothing"
        );
        assert!(report.serial_learned, "the promotion must be reported");
        assert!(
            !report.cartridge_registered,
            "it must adopt the existing row, never register a second one"
        );
        assert_eq!(
            chip_serial(&conn, id).as_deref(),
            Some("HU1234ABCD"),
            "the claim must be promoted to the chip-confirmed column"
        );
    }

    /// Issue #214: two rows claiming the same serial is the one case the new
    /// arm must NOT guess at. `catalog rebuild` has no `--cartridge` flag, so
    /// the remediation named has to be one that exists.
    #[test]
    fn mam_identity_refuses_when_two_rows_claim_the_same_serial() {
        let conn = crate::db::open_memory().unwrap();
        hand_registered(&conn, "BC-ONE", Some("HU1234ABCD"));
        hand_registered(&conn, "BC-TWO", Some("HU1234ABCD"));

        let mut report = RebuildReport::default();
        let err = resolve_mam_identity(&conn, "HU1234ABCD", &mam_drill_meta(), None, &mut report);
        let err = match err {
            Ok(r) => panic!(
                "expected a refusal, but it resolved to cartridge \"{}\" (id {})",
                r.barcode, r.id
            ),
            Err(e) => e.to_string(),
        };

        assert!(err.contains("BC-ONE") && err.contains("BC-TWO"), "{err}");
        assert!(
            err.contains("tapectl cartridge edit"),
            "the way out has to be a command that exists — `catalog rebuild` has no \
             --cartridge: {err}"
        );
        assert!(
            !err.contains("--cartridge"),
            "`catalog rebuild` has no --cartridge flag: {err}"
        );
        assert!(
            !report.cartridge_registered,
            "an ambiguous claim must never auto-register a third row: {err}"
        );
    }

    /// #420: `catalog rebuild --from-volume` of a tape that ended early,
    /// against the write that made it (`volume::restore::short_tape`).
    mod short_ending {
        use super::*;
        use crate::store::{MemStore, Store};
        use crate::tape::contact::{ContactSite, Medium, Operation};
        use crate::volume::restore::short_tape::{self, ShortTape, LABEL};

        fn cfg() -> &'static crate::config::Config {
            static CFG: std::sync::OnceLock<crate::config::Config> = std::sync::OnceLock::new();
            CFG.get_or_init(crate::config::Config::default)
        }

        fn rebuild_into(conn: &Connection, t: &mut ShortTape) -> RebuildReport {
            let scratch = tempfile::tempdir().unwrap();
            rebuild_from_store(
                conn,
                &mut t.store,
                std::slice::from_ref(&t.operator),
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
            .unwrap()
        }

        /// `(unit, writes.status)` and every `(unit, position, status)` of
        /// the volume's writes, in a stable order.
        #[allow(clippy::type_complexity)]
        fn rows_and_positions(
            conn: &Connection,
        ) -> (Vec<(String, String)>, Vec<(String, i64, String)>) {
            let writes = conn
                .prepare(
                    "SELECT u.name, w.status FROM writes w
                       JOIN snapshots s ON s.id = w.snapshot_id
                       JOIN units u ON u.id = s.unit_id
                       JOIN volumes v ON v.id = w.volume_id
                      WHERE v.label = ?1 ORDER BY u.name",
                )
                .unwrap()
                .query_map(params![LABEL], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            let positions = conn
                .prepare(
                    "SELECT u.name, CAST(wp.position AS INTEGER), wp.status
                       FROM write_positions wp
                       JOIN writes w ON w.id = wp.write_id
                       JOIN snapshots s ON s.id = w.snapshot_id
                       JOIN units u ON u.id = s.unit_id
                       JOIN volumes v ON v.id = w.volume_id
                      WHERE v.label = ?1 ORDER BY 2",
                )
                .unwrap()
                .query_map(params![LABEL], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            (writes, positions)
        }

        fn volume_ending(conn: &Connection) -> (String, Option<i64>, i64) {
            conn.query_row(
                "SELECT status, ended_after, num_data_files FROM volumes WHERE label = ?1",
                params![LABEL],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap()
        }

        /// THE pin: a rebuild of a short tape gives the rows the write that
        /// made it gave — each unit `completed` or `cut` alike, the same
        /// positions `written` (at or before K) and `pending` (after it),
        /// and the same `ended_after` — so the catalog says the same thing
        /// about the tape whether it wrote it or read it back
        /// (layout-session.md "Later contacts").
        #[test]
        fn a_rebuild_of_a_short_tape_gives_the_rows_the_write_gave() {
            let mut t = short_tape::short_tape();
            let k = t.ended_after;
            let fresh = crate::db::open_memory().unwrap();
            let report = rebuild_into(&fresh, &mut t);

            assert_eq!(report.unsealed_reason, None, "a short seal is a seal");
            assert_eq!(report.ended_after, Some(u64::from(k)));
            assert_eq!(report.cut_units, vec!["bravo v1", "charlie v1"]);
            assert!(report.volume_inserted);

            let written = rows_and_positions(&t.conn);
            let rebuilt = rows_and_positions(&fresh);
            assert_eq!(
                rebuilt.0,
                vec![
                    ("alpha".to_string(), "completed".to_string()),
                    ("bravo".to_string(), "cut".to_string()),
                    ("charlie".to_string(), "cut".to_string()),
                ]
            );
            assert_eq!(rebuilt, written, "the rebuild and the write agree");
            for (unit, position, status) in &rebuilt.1 {
                let expect = if *position <= i64::from(k) {
                    "written"
                } else {
                    "pending"
                };
                assert_eq!(status, expect, "{unit} at {position}");
            }
            // The volume: sealed, its short ending recorded, and only the
            // slices at or before K counted as its data files.
            let slices_on_tape = t
                .positions
                .iter()
                .flat_map(|(_, p)| p.iter())
                .filter(|p| **p <= k)
                .count() as i64;
            assert_eq!(
                volume_ending(&fresh),
                ("sealed".to_string(), Some(i64::from(k)), slices_on_tape)
            );
            assert_eq!(
                volume_ending(&t.conn).1,
                Some(i64::from(k)),
                "the write recorded the same ending"
            );

            // Nothing about a cut unit counts as a copy.
            let copies = |unit: &str| -> i64 {
                let id: i64 = fresh
                    .query_row("SELECT id FROM units WHERE name = ?1", [unit], |r| r.get(0))
                    .unwrap();
                crate::cli::audit::copy_count_for_unit(&fresh, id).unwrap()
            };
            assert_eq!(copies("alpha"), 1);
            assert_eq!(copies("bravo"), 0);
            assert_eq!(copies("charlie"), 0);

            // A second run adds nothing.
            let again = rebuild_into(&fresh, &mut t);
            assert!(again.is_noop(), "{again:?}");
            assert_eq!(again.ended_after, Some(u64::from(k)));
        }

        /// The control: the same write sealed full rebuilds as every tape
        /// did before 1.6.0 — no ending recorded, every write completed,
        /// every position written.
        #[test]
        fn a_rebuild_of_a_full_tape_is_unchanged() {
            let mut t = short_tape::full_tape();
            let fresh = crate::db::open_memory().unwrap();
            let report = rebuild_into(&fresh, &mut t);
            assert_eq!(report.unsealed_reason, None);
            assert_eq!(report.ended_after, None);
            assert!(report.cut_units.is_empty());
            let (writes, positions) = rows_and_positions(&fresh);
            assert!(writes.iter().all(|(_, s)| s == "completed"), "{writes:?}");
            assert!(
                positions.iter().all(|(_, _, s)| s == "written"),
                "{positions:?}"
            );
            assert_eq!(positions.len(), 6);
            assert_eq!(volume_ending(&fresh), ("sealed".to_string(), None, 6));
            assert_eq!(rows_and_positions(&t.conn), (writes, positions));
        }

        /// R3 for attestation: the escrow key is asked of each unit's first
        /// slice header, and on a short tape a unit whose first slice lies
        /// past `ended_after` is not read at all (the MemStore holds nothing
        /// there, so a read would count as unreadable).
        #[test]
        fn attestation_reads_no_slice_past_the_end_of_a_short_tape() {
            let mut t = short_tape::short_tape();
            let fresh = crate::db::open_memory().unwrap();
            rebuild_into(&fresh, &mut t);
            // As for a tape whose catalog.db carried no receipts, with the
            // key in hand registered as the escrow recipient.
            fresh
                .execute("UPDATE stage_sets SET key_fingerprints = NULL", [])
                .unwrap();
            fresh
                .execute(
                    "INSERT INTO tenants (name, is_operator, status) VALUES ('escrowee', 1, 'active')",
                    [],
                )
                .unwrap();
            let public = t.operator.to_public().to_string();
            fresh
                .execute(
                    "INSERT INTO encryption_keys (tenant_id, alias, fingerprint, public_key,
                                                  key_type, is_active, is_escrow)
                     VALUES (last_insert_rowid(), 'escrow', ?1, ?1, 'primary', 1, 1)",
                    params![public],
                )
                .unwrap();
            let report = rebuild_into(&fresh, &mut t);
            assert!(report.key_is_escrow);
            assert_eq!(report.escrow_attest_unreadable, 0, "{report:?}");
            assert_eq!(
                report.attested, 2,
                "alpha's and bravo's first slices: {report:?}"
            );
            assert_eq!(
                report.unknown_remaining, 0,
                "a cut set is not counted as unknown"
            );
        }

        /// §1.2 rule 6's acceptance, through [`locate_and_judge_seal`]: an
        /// early seal is a short seal only on a tape whose File 0 requires
        /// `short-seal`; on any other tape it is no seal.
        #[test]
        fn an_early_seal_on_a_tape_that_does_not_require_short_seal_is_no_seal() {
            let mut t = short_tape::short_tape();
            let k = t.ended_after;
            let file0 = String::from_utf8(
                crate::store::read_small_bytes(&mut t.store, 0, "File 0").unwrap(),
            )
            .unwrap();
            let fi_raw = crate::store::read_small_bytes(&mut t.store, 3, "File 3").unwrap();
            let fi = format::without_nul_padding(&fi_raw).to_vec();

            let located = locate_and_judge_seal(&mut t.store, &file0, &fi, 3, t.planned_seal);
            assert!(matches!(located.verdict, format::SealVerdict::Sealed(_)));
            assert_eq!(located.ended_after, Some(k));
            assert_eq!(located.position, Some(k + 1));

            // The same tape, its File 0 stating it requires nothing.
            let file0_old = file0.replace("requires = [\"short-seal\"]", "requires = []");
            assert_ne!(
                file0_old, file0,
                "positive control: File 0 requires short-seal"
            );
            let located = locate_and_judge_seal(&mut t.store, &file0_old, &fi, 3, t.planned_seal);
            let format::SealVerdict::Unsealed(why) = &located.verdict else {
                panic!("an early seal there is no seal: {located:?}");
            };
            assert!(why.contains("does not require"), "{why}");
            assert_eq!(located.ended_after, None);

            // A seal found at the planned position that states another
            // count is the two ends disagreeing, not a full seal.
            let mut store = MemStore::new(short_tape::BS as usize);
            for f in 0..t.planned_seal {
                let bytes = if f <= k + 1 {
                    crate::store::read_small_bytes(&mut t.store, f, "file").unwrap_or_default()
                } else {
                    b"stand-in".to_vec()
                };
                store
                    .execute(
                        &mut std::io::Cursor::new(bytes.clone()),
                        bytes.len() as u64,
                        false,
                    )
                    .unwrap();
            }
            let short_seal = crate::store::read_small_bytes(&mut t.store, k + 1, "seal").unwrap();
            store
                .execute(
                    &mut std::io::Cursor::new(short_seal.clone()),
                    short_seal.len() as u64,
                    false,
                )
                .unwrap();
            let located = locate_and_judge_seal(&mut store, &file0, &fi, 3, t.planned_seal);
            let format::SealVerdict::Unsealed(why) = &located.verdict else {
                panic!("a short seal at the planned position is no full seal: {located:?}");
            };
            assert!(why.contains("disagree"), "{why}");
        }
    }
}
