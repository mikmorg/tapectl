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
//! `tenants`, no `key_fingerprints`, no `sha256_plain`) is told apart from a
//! current one — probed from the file's actual shape, never from a version
//! number (see that module's doc for why).
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
use std::io::Cursor;
use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};

use crate::db::ontape_catalog::{self, Generation};
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
    /// Set (to the status found) when this label already had a `volumes` row
    /// and that row's status was not `sealed` — an imported `active` row, for
    /// instance. `None` when the row was freshly inserted here (always
    /// `sealed`, see `insert_all`) or was already `sealed`.
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
            && !self.cartridge_registered
            && !self.cartridge_bound
            && !self.serial_learned
            && self.displaced.is_empty()
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
    // Only for resolving the drive's SCSI generic node, so the loaded
    // medium's serial can be read and corroborated (issue #193). Optional by
    // construction: no matching backend means no serial, which is an
    // absence, which proceeds — the DR machine, exactly.
    config: &crate::config::Config,
    device: &str,
    block_size: usize,
    key_path: &Path,
    expect_label: Option<&str>,
    fallback_tenant: &str,
    backend_name: Option<&str>,
    scratch: &Path,
    reads: &crate::tape::mam_journal::MamReads<'_>,
) -> Result<RebuildReport> {
    let secret = crate::crypto::keys::read_secret_key(key_path)?;
    let identity: age::x25519::Identity = secret.parse().map_err(|e| {
        TapectlError::Encryption(format!("invalid key in {}: {e}", key_path.display()))
    })?;
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
        &[identity],
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
    guard.finish_result(rebuild_contacted(
        conn,
        store,
        identities,
        expect_label,
        fallback_tenant,
        backend_name,
        scratch,
        device_label,
        site.medium_serial(),
    ))
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
    let mut thunk = Vec::new();
    store.read_file(0, &mut thunk)?;
    let thunk_text = String::from_utf8_lossy(&thunk).to_string();
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
    attest_escrow(&tx, store, identities, &operator.manifest, &mut report)?;
    report.unknown_remaining = tx.query_row(
        "SELECT COUNT(*) FROM stage_sets ss
         JOIN writes w ON w.stage_set_id = ss.id
         WHERE w.volume_id = ?1 AND ss.origin = 'rebuilt' AND ss.key_fingerprints IS NULL",
        params![volume_id],
        |r| r.get(0),
    )?;
    record_event(&tx, &report, volume_id, device_label)?;
    tx.commit()?;

    Ok(report)
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
    tx: &Connection,
    store: &mut dyn Store,
    identities: &[age::x25519::Identity],
    manifest: &EnvelopeManifest,
    report: &mut RebuildReport,
) -> Result<()> {
    let Some(registered) = crate::db::queries::escrow_public_key(tx)? else {
        return Ok(());
    };
    let Some(escrow_id) = identities
        .iter()
        .find(|id| id.to_public().to_string() == registered)
    else {
        return Ok(());
    };
    report.key_is_escrow = true;
    let receipt = serde_json::to_string(&[registered.as_str()])
        .map_err(|e| TapectlError::Other(format!("receipt json: {e}")))?;

    for unit in &manifest.units {
        let Some(first) = unit.slices.iter().min_by_key(|s| s.number) else {
            continue;
        };
        let stage_set_id: Option<i64> = tx
            .query_row(
                "SELECT ss.id FROM stage_sets ss
                 JOIN snapshots s ON s.id = ss.snapshot_id
                 JOIN units u ON u.id = s.unit_id
                 WHERE u.name = ?1 AND s.version = ?2
                   AND ss.origin = 'rebuilt' AND ss.key_fingerprints IS NULL
                 LIMIT 1",
                params![unit.name, unit.snapshot_version],
                |r| r.get(0),
            )
            .optional()?;
        let Some(stage_set_id) = stage_set_id else {
            continue; // already has a receipt (from the tape, or an earlier attestation)
        };

        let mut head = Vec::new();
        if let Err(e) =
            store.read_file_head(first.tape_position as u32, ATTEST_HEAD_BYTES, &mut head)
        {
            tracing::warn!(unit = %unit.name, position = first.tape_position, error = %e,
                "attest: could not read the slice header; leaving coverage unknown");
            report.escrow_attest_unreadable += 1;
            continue;
        }
        let opened = age::Decryptor::new(Cursor::new(head))
            .and_then(|d| d.decrypt(std::iter::once(escrow_id as &dyn age::Identity)));
        match opened {
            Ok(_) => {
                tx.execute(
                    "UPDATE stage_sets SET key_fingerprints = ?1
                     WHERE id = ?2 AND key_fingerprints IS NULL",
                    params![receipt, stage_set_id],
                )?;
                report.attested += 1;
            }
            Err(age::DecryptError::NoMatchingKeys) => {
                tracing::warn!(unit = %unit.name, position = first.tape_position,
                    "attest: the escrow key is not a recipient of this slice — the #115 shape; \
                     coverage stays unknown");
                report.escrow_attest_not_recipient += 1;
            }
            Err(e) => {
                tracing::warn!(unit = %unit.name, position = first.tape_position, error = %e,
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
/// diagnosis, because it is what a tenant key looks like from here, so it
/// falls through to `operator_envelope_backup` (the redundant copy exists
/// precisely for a damaged primary) before giving up.
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
    format!(
        "this key cannot open the operator envelope, so it is neither an \
         operator key nor the escrow key.\n\n\
         A tenant key restores that tenant's own data without a catalog at \
         all: run RESTORE.sh from tape file 2 —\n\n    \
         tapectl restore raw-volume --device DEV --to DIR\n    \
         bash DIR/{restore_sh} --restore --unit UNIT --key KEYFILE --to DIR\n\n\
         `catalog rebuild` reconstructs the operator's catalog and needs the \
         operator's view of the tape."
    )
}

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
        return Err(TapectlError::Other(tenant_key_refusal_message()));
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
    /// Read every table via `ontape_catalog::read`, then derive exactly the
    /// maps `insert_all`/`tenant_index`/`ensure_*` already consume, keyed by
    /// unit name (and version, where a fact is per-snapshot) the way the
    /// hand-written joins used to key them.
    fn load(path: &Path) -> Result<Self> {
        let cat = ontape_catalog::read(path)?;
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

        for f in &cat.files {
            let Some(key) = snapshot_key.get(&f.snapshot_id) else {
                continue;
            };
            let size_bytes = f.size_bytes.ok_or_else(|| {
                TapectlError::Other(format!(
                    "catalog.db files row {} ({:?}) has NULL size_bytes",
                    f.id, f.path
                ))
            })?;
            out.files.entry(key.clone()).or_default().push(FileRow {
                path: f.path.clone(),
                size_bytes,
                sha256: f.sha256.clone(),
                modified_at: f.modified_at.clone(),
                is_directory: f.is_directory,
            });
        }

        out.has_tenants = cat.generation == Generation::WithOwnershipAndReceipts;
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
    let existing: Option<(i64, String, String)> = tx
        .query_row(
            "SELECT id, status, observed_condition FROM volumes WHERE label = ?1",
            params![label],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let volume_id = match existing {
        Some((id, status, condition)) => {
            // Same tape, same evidence — but a row this rebuild merely
            // FOUND is never edited (issue #158). `sealed`/`ok` is what the
            // `None` arm below would have inserted, so anything else is a
            // fact worth surfacing: an imported `active` row, for instance.
            // Reported, not repaired — see `RebuildReport::
            // volume_status_mismatch`.
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

    let detail = format!(
        "catalog rebuild from volume {} (uuid {}) on {}: {} envelope(s) opened; \
         inserted {} tenant(s), {} unit(s), {} snapshot(s), {} stage set(s), {} slice(s), \
         {} write(s), {} position(s), {} file row(s); {} escrow receipt(s) from tape, {} attested; catalog.db {}{cartridge_clause}{displaced_clause}",
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
             cartridge_identity_source (written before issue #192) — it cannot be told \
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
                 `tapectl cartridge edit <barcode> --generation <G>` (issue #167), which \
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
            tape_position: position,
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
}
