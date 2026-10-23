//! Layout build: the "build" half of the §9 typestate flow
//! (`docs/design/v2-implementation-plan.md` T5b; `docs/design/v2-open-questions.md`
//! §9 — `Layout::build(...) -> BuiltLayout`). Turns plain data ([`BuildInputs`])
//! into a [`BuiltLayout`]: every generated zone materialized to a session
//! staging directory exactly once (§2.2 "materialize-to-staging"), tenant
//! envelopes permuted per §2.1, and the front index / seal marker built from
//! the completed entry list per `docs/design/volume-format-v2.md` §§1, 3, 4.
//!
//! `build()` takes no DB connection and touches no tape — it is pure enough
//! to unit-test on its own (plan T5b: "build must be pure enough to
//! unit-test without a DB"). The session half (`ValidatedLayout::plan`
//! onward) is T6, not here; `BuiltLayout::validate` below is the pre-write
//! predicate this task owns (`layout-session.md`'s validation points,
//! `Layout::validate` in `layout_model.rs` plus the parts that need file
//! reads and a subprocess).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::pass_count::Sha256;
use sha2::Digest;
use uuid::Uuid;

use crate::error::{Result, TapectlError};
use crate::staging;

use super::format;
use super::index::{self, IndexFiles};
use super::layout::{self, FrontIndexFile, IdThunkV2Params};
use super::layout_model::{
    CapacityBudget, ContentSource, KeyAvailability, Layout, LayoutEntry, LayoutError, Placement,
    PlanClass, PlanSplit, SliceCheck, ZoneKind,
};

// ── Plain-data inputs ──

/// One staged, already-encrypted data slice — mirrors the shape of
/// `write.rs`'s private `SliceInfo` (v1) so a future orchestrator can convert
/// between them trivially, without this module depending on `write.rs` or a
/// DB connection (scope fence, plan T5b).
#[derive(Debug, Clone)]
pub struct BuildSlice {
    pub slice_id: i64,
    /// dar's slice number — the `restore.N.dar` ordinal.
    pub slice_number: i64,
    /// Plaintext size, before encryption (carried through into the
    /// per-tenant manifest only; not the on-tape size).
    pub size_bytes: i64,
    /// On-tape (ciphertext) size — this is what the Layout entry's
    /// `size_bytes` becomes.
    pub encrypted_bytes: i64,
    pub sha256_plain: String,
    /// Taken verbatim as the Layout entry's `sha256` and the front index's
    /// `sha256_encrypted` for this position — never recomputed here
    /// (`volume-format-v2.md` §3: "not recomputed while building the
    /// index"; `Layout::validate` is the layer that re-hashes from disk).
    pub sha256_encrypted: String,
    /// Where the already-encrypted slice bytes live on disk.
    pub staging_path: PathBuf,
}

/// One staged unit (folder) with its ordered slices — mirrors `write.rs`'s
/// private `StagedUnit` (v1).
#[derive(Debug, Clone)]
pub struct BuildUnit {
    pub stage_set_id: i64,
    pub snapshot_id: i64,
    pub unit_name: String,
    pub unit_uuid: String,
    pub tenant_id: i64,
    pub dar_version: Option<String>,
    pub dar_command: Option<String>,
    pub catalog_path: Option<String>,
    pub snapshot_version: i64,
    /// In the order they will be written (dar's slice-number order). For a
    /// unit whose write may span, the slices THIS tape plans; every slice of
    /// the stage set is in [`BuildUnit::span`].
    pub slices: Vec<BuildSlice>,
    /// `Some` when the write may span (ADR-0012 items 121, 124-125, #478):
    /// the unit's MANIFEST.toml block states `may_span = true` and lists
    /// every slice of the stage set from here, with a `tape_position` only
    /// on the slices [`BuildUnit::slices`] plans. Frozen when the write is
    /// planned. `None` for a whole unit, whose block is the 1.6.0 block.
    pub span: Option<UnitSpan>,
}

/// Every slice of a stage set whose write may span, as the unit's
/// MANIFEST.toml block states them (ADR-0012 item 125): from any tape of a
/// span a reader can accept any other tape of it by hash (item 88), and a
/// rebuild records every slice (item 130).
#[derive(Debug, Clone)]
pub struct UnitSpan {
    /// Every slice of the stage set, in slice-number order, planned on this
    /// tape or not. [`build`] refuses a span that leaves out a slice
    /// [`BuildUnit::slices`] plans, or states it differently.
    pub slices: Vec<SpanSlice>,
}

/// One slice's manifest facts. No staging path: a slice planned on another
/// tape may already be released from staging (item 129).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanSlice {
    /// `stage_slices.id`: how [`build`] finds the slices it places.
    pub slice_id: i64,
    pub slice_number: i64,
    pub size_bytes: i64,
    pub encrypted_bytes: i64,
    pub sha256_plain: String,
    pub sha256_encrypted: String,
}

impl From<&BuildSlice> for SpanSlice {
    fn from(slice: &BuildSlice) -> Self {
        SpanSlice {
            slice_id: slice.slice_id,
            slice_number: slice.slice_number,
            size_bytes: slice.size_bytes,
            encrypted_bytes: slice.encrypted_bytes,
            sha256_plain: slice.sha256_plain.clone(),
            sha256_encrypted: slice.sha256_encrypted.clone(),
        }
    }
}

/// A tenant's identity and recipient keys, as plain data.
#[derive(Debug, Clone)]
pub struct TenantInfo {
    pub tenant_id: i64,
    pub tenant_name: String,
    /// This tenant's own active public keys. Operator keys and the escrow
    /// key are supplied separately on `BuildInputs` and appended by `build()`
    /// itself — this module does its own local recipient-list assembly
    /// rather than depending on `src/staging/mod.rs`'s recipient-list code,
    /// which is out of scope for T5b (scope fence).
    pub public_keys: Vec<String>,
}

/// Everything `build()` needs, as plain data — no DB connection, no tape
/// (plan T5b: "Take plain data, NOT a db connection — build must be pure
/// enough to unit-test without a DB").
#[derive(Debug, Clone)]
pub struct BuildInputs {
    // --- volume identity ---
    pub label: String,
    /// The volume's UUID, string form. `build()` parses it to get the raw
    /// bytes the §2.1 envelope permutation hashes.
    pub volume_uuid: String,
    pub media_type: String,
    /// The writer's PACKAGE version (`build_info::PKG_VERSION`, i.e.
    /// `CARGO_PKG_VERSION`) — never the build identity
    /// (`build_info::VERSION`, commit + day). This reaches `MANIFEST.toml`,
    /// which `tests/on_tape_golden.rs` pins byte for byte, and ADR-0012
    /// (2026-09-24 amendment, item 1) rules that build identity changes no
    /// on-tape byte. `build_info::tests::on_tape_writer_string_is_the_package_version`
    /// pins the `volume::write` call site.
    pub tapectl_version: String,
    /// RFC 3339 generation timestamp for the ID thunk's `created_at` (T6
    /// review finding #5). The caller renders this once (normally
    /// `chrono::Utc::now().to_rfc3339()`) rather than `build()` reading the
    /// clock, so a test can hold it constant across two `build()` calls and
    /// check the ID thunk for byte-identity like every other deterministic
    /// zone — see `IdThunkV2Params::created_at`'s doc comment.
    pub created_at: String,

    // --- capacity (becomes Layout::budget) ---
    pub block_size: u64,
    pub usable_bytes: u64,
    pub enospc_buffer: u64,

    // --- ID-thunk-only informational fields (sheet §2.3) ---
    pub nominal_capacity: i64,
    pub mam_capacity: i64,
    pub mam_manufacturer: String,
    /// The cartridge's identity string, as the BINDING established it —
    /// `cartridges.serial_number` for a chip-reported one, `.barcode` for one
    /// the operator named (ADR-0012). Named `mam_serial` for continuity with
    /// the on-tape key `cartridge_serial`; `cartridge_identity_source` below
    /// is what says which it is.
    pub mam_serial: String,
    /// How `mam_serial` was established: `Some("mam")`, `Some("operator")`,
    /// or `None` for unknown — read back from
    /// `cartridge_volumes.identity_source` by the caller (which has a
    /// `Connection`; `build()` deliberately does not). `None` OMITS the key
    /// from File 0 entirely: absent means unknown and never "mam"
    /// (`docs/design/volume-format-v2.md` §1.1).
    pub cartridge_identity_source: Option<String>,
    pub mam_length: i64,
    pub mam_loads: i64,

    // --- content ---
    /// Unit-contiguous, in the order they will be written (v1's
    /// `find_staged_data` uses `ORDER BY u.name`; `build()` never reorders —
    /// the caller decides unit order).
    pub units: Vec<BuildUnit>,
    /// One entry per distinct tenant_id appearing in `units`.
    pub tenants: Vec<TenantInfo>,
    pub operator_public_keys: Vec<String>,
    /// The permanent Escrow Recipient (ADR-0005), appended to every
    /// encryption's recipient list when present. Escrow wiring landed in T2;
    /// `None` here now means only "the caller intentionally omits it" (test
    /// fixtures do; `write::volume_write` supplies the registered escrow key).
    /// `BuiltLayout::validate`'s
    /// `KeyAvailability.escrow_recipient_present` is a separate, independent
    /// check (matching the existing `Layout::validate` design — it does not
    /// re-derive from what `build()` actually used).
    pub escrow_public_key: Option<String>,
    /// Path to a pre-generated, filtered `catalog.db` (issue #83) covering
    /// this write's units/snapshots/stage_sets/stage_slices/files —
    /// `db::catalog_snapshot::build_catalog_snapshot`'s output, generated by
    /// `write::volume_write` (which HAS a `Connection`) before `build()` is
    /// called. `build()` stays `Connection`-free by design: it just streams
    /// this file into the OPERATOR envelope only, the same way per-unit dar
    /// catalogues already reach it. `None` means no snapshot was generated
    /// (e.g. test fixtures that don't exercise #83).
    pub catalog_db_path: Option<PathBuf>,
    /// The cross-volume index (`volume-format-v2.md` §1.5, #418 R4),
    /// pre-generated by `volume::index::generate_files` in the tapectl home's
    /// work dir, where `write::volume_write` HAS a `Connection` and `build()`
    /// deliberately has none. `build()` streams each file into the envelope it
    /// belongs to, after `catalogs/`: a tenant's `index/units.tsv` and
    /// `index/paths.tsv` into that tenant's envelope, the operator's
    /// `index/units.tsv` into the operator envelope. Both are built into the
    /// envelope BEFORE its size is read, so the capacity gate counts them
    /// like every other byte. `None` writes no `index/` members (a test
    /// fixture that does not exercise R4).
    pub index: Option<IndexFiles>,
}

/// The build half of the §9 typestate: a fully assembled, materialized
/// [`Layout`] plus the staging directory its generated zones live under.
#[derive(Debug, Clone)]
pub struct BuiltLayout {
    pub layout: Layout,
    pub session_dir: PathBuf,
}

// ── build() ──

/// Assemble a [`BuiltLayout`] from `inputs`: every generated zone written to
/// `session_dir` exactly once, tenant envelopes permuted per §2.1, front
/// index and seal marker built last from the completed entry list
/// (`docs/design/volume-format-v2.md` §§1, 3, 4). Generators run ONCE here —
/// nothing on this path is ever re-invoked on a later read; resume re-reads
/// these frozen files (plan T5b, sheet §2.2).
///
/// The Layout's plan stops at the fill ceiling (no plan bound); a write whose
/// drive declares `post_warning_bytes` builds with
/// [`build_with_plan_bound`] instead.
pub fn build(inputs: &BuildInputs, session_dir: &Path) -> Result<BuiltLayout> {
    build_with_plan_bound(inputs, None, session_dir)
}

/// [`build`], with the plan bound of ADR-0012 item 113 frozen into the
/// Layout's budget ([`CapacityBudget::plan_bound_bytes`]): `Some(capacity)`,
/// 100% of the cartridge's nominal capacity, when the writing drive declares
/// `post_warning_bytes` ([`crate::config::LtoBackendConfig::plan_bound_bytes`]),
/// and `None` to stop at the fill ceiling as [`build`] does. The bound is an
/// input to no generator, only to the Layout's budget and its `layout.json`:
/// the units are the caller's, every one is laid out whole, and the front
/// index lists them all — the ones past the ceiling too
/// ([`BuiltLayout::split`] says which those are).
///
/// The units are laid out in the caller's order, on a span tape too. The
/// plan order of `layout-session.md` "Spanning" (owed runs, whole units,
/// spannable sets, and which units wait for a later tape) is decided before
/// the build, by [`CapacityBudget::place`], because only the caller knows an
/// owed run from a fresh spannable set and a deferred unit must not reach
/// the envelopes; [`BuiltLayout::place`] reads the built plan back. When the
/// last unit may span, its first slice's position is frozen as the budget's
/// [`CapacityBudget::span_tail_from`]: the tail the capacity gate lets pass
/// the plan bound (ADR-0012 item 123).
pub fn build_with_plan_bound(
    inputs: &BuildInputs,
    plan_bound_bytes: Option<u64>,
    session_dir: &Path,
) -> Result<BuiltLayout> {
    for unit in &inputs.units {
        check_span(unit)?;
    }
    fs::create_dir_all(session_dir)?;

    let volume_uuid = Uuid::parse_str(&inputs.volume_uuid).map_err(|e| {
        TapectlError::Other(format!(
            "build: volume_uuid {:?} is not a valid UUID: {e}",
            inputs.volume_uuid
        ))
    })?;

    // Positions are pure arithmetic over counts, fixed before any byte is
    // generated (format §1: 4 front files, permuted tenant envelopes,
    // operator + backup, the tenant envelopes' backups in the primaries'
    // order (§1.3), unit-contiguous slices, seal marker last).
    let mut distinct_tenant_ids: Vec<i64> = inputs.units.iter().map(|u| u.tenant_id).collect();
    distinct_tenant_ids.sort_unstable();
    distinct_tenant_ids.dedup();

    let mut permuted_tenants = distinct_tenant_ids;
    // §2.1: stable sort by hex(sha256(volume_uuid_bytes || 0x00 || le64(tenant_id))).
    permuted_tenants
        .sort_by_cached_key(|&tenant_id| tenant_permutation_key(&volume_uuid, tenant_id));

    let front_index_pos: i32 = 3;
    let first_envelope_pos: i32 = 4;
    let num_tenants = permuted_tenants.len() as i32;
    let op_pos = first_envelope_pos + num_tenants;
    let op_backup_pos = op_pos + 1;
    let first_tenant_backup_pos = op_backup_pos + 1;
    let data_start = first_tenant_backup_pos + num_tenants;

    let all_slices: Vec<&BuildSlice> = inputs.units.iter().flat_map(|u| u.slices.iter()).collect();
    let num_slices = all_slices.len() as i32;
    let total_files = data_start + num_slices + 1; // +1: the seal marker
    let seal_pos = total_files - 1;

    let mut slice_positions: HashMap<i64, i32> = HashMap::with_capacity(all_slices.len());
    for (i, slice) in all_slices.iter().enumerate() {
        slice_positions.insert(slice.slice_id, data_start + i as i32);
    }

    let mut entries: Vec<LayoutEntry> = Vec::with_capacity(total_files as usize);

    // Files 0-2: identity + guide + heir tool. `system_guide`/`restore_sh`
    // are pure functions of (label, total_files); the ID thunk's `created_at`
    // is injected from `inputs` (T6 review finding #5) rather than read from
    // the clock in here, so build-twice byte-identity is testable for it too.
    let id_thunk_params = IdThunkV2Params {
        label: &inputs.label,
        uuid: &inputs.volume_uuid,
        media_type: &inputs.media_type,
        tapectl_version: &inputs.tapectl_version,
        nominal_capacity: inputs.nominal_capacity,
        mam_capacity: inputs.mam_capacity,
        total_files,
        mam_manufacturer: &inputs.mam_manufacturer,
        mam_serial: &inputs.mam_serial,
        mam_length: inputs.mam_length,
        mam_loads: inputs.mam_loads,
        created_at: &inputs.created_at,
        cartridge_identity_source: inputs.cartridge_identity_source.as_deref(),
    };
    // `requires` (ADR-0012 item 124, `volume-format-v2.md` §1.2): a tape whose
    // plan holds a slice of a spannable set states `spanning` beside
    // `short-seal`, in File 0 and in the seal (the placeholder below states the
    // same). A unit with `span` set is exactly one whose MANIFEST block states
    // `may_span = true` (`to_manifest_unit`), the block the fence is for.
    let requires = layout::tape_requires(inputs.units.iter().any(|u| u.span.is_some()));
    let id_thunk_bytes = layout::generate_id_thunk_v2_requiring(&id_thunk_params, requires);
    let (id_thunk_path, id_thunk_size, id_thunk_hash) =
        materialize(session_dir, "0000_id_thunk", id_thunk_bytes.as_bytes())?;
    entries.push(LayoutEntry {
        position: 0,
        kind: ZoneKind::IdThunk,
        size_bytes: Some(id_thunk_size),
        sha256: Some(id_thunk_hash),
        source: ContentSource::Materialized(id_thunk_path),
    });

    let guide_bytes =
        layout::generate_system_guide_v2_for_media(&inputs.label, total_files, &inputs.media_type);
    let (guide_path, guide_size, guide_hash) =
        materialize(session_dir, "0001_system_guide", guide_bytes.as_bytes())?;
    entries.push(LayoutEntry {
        position: 1,
        kind: ZoneKind::SystemGuide,
        size_bytes: Some(guide_size),
        sha256: Some(guide_hash),
        source: ContentSource::Materialized(guide_path),
    });

    let restore_sh_bytes = layout::generate_restore_script_v2(&inputs.label, total_files);
    let (restore_sh_path, restore_sh_size, restore_sh_hash) =
        materialize(session_dir, "0002_restore_sh", restore_sh_bytes.as_bytes())?;
    entries.push(LayoutEntry {
        position: 2,
        kind: ZoneKind::RestoreSh,
        size_bytes: Some(restore_sh_size),
        sha256: Some(restore_sh_hash),
        source: ContentSource::Materialized(restore_sh_path),
    });

    // Tenant envelopes, in permuted order (§2.1). Recipients: this tenant's
    // own keys + operator + escrow (`volume-format-v2.md` §1: enc(t+op+esc)).
    for (i, &tenant_id) in permuted_tenants.iter().enumerate() {
        let tenant = inputs.tenants.iter().find(|t| t.tenant_id == tenant_id).ok_or_else(|| {
            TapectlError::Other(format!(
                "build: unit(s) reference tenant {tenant_id}, but BuildInputs.tenants has no entry for it"
            ))
        })?;

        let manifest_units = build_manifest_units(inputs, tenant_id, &slice_positions);
        let manifest =
            layout::generate_manifest_toml(&inputs.label, &tenant.tenant_name, &manifest_units);
        let catalogs = catalogs_for_tenant(inputs, tenant_id);
        // RECOVERY.md names a unit's dar catalogue only if this envelope
        // carries it (#449): the members are what `catalogs` puts in the tar.
        let recovery = layout::generate_recovery_md(
            &inputs.label,
            &tenant.tenant_name,
            &manifest_units,
            &catalogue_member_names(&catalogs),
        );
        // PLAN.toml is operator-only (`volume-format-v2.md` §1's middle-zone
        // table: "PLAN.toml (op only)") — tenant envelopes never carry it.

        let mut recipients: Vec<String> = tenant.public_keys.clone();
        recipients.extend(inputs.operator_public_keys.iter().cloned());
        let recipients = with_escrow(recipients, &inputs.escrow_public_key);

        let pos = first_envelope_pos + i as i32;
        let index_members = tenant_index_members(inputs, tenant_id, &tenant.tenant_name)?;
        let (path, size, hash) = materialize_envelope_streaming(
            session_dir,
            &format!("{pos:04}_tenant_envelope_{tenant_id}"),
            &manifest,
            &recovery,
            &catalogs,
            None,
            None,
            &index_members,
            &recipients,
        )?;
        entries.push(LayoutEntry {
            position: pos,
            kind: ZoneKind::TenantEnvelope { tenant_id },
            size_bytes: Some(size),
            sha256: Some(hash.clone()),
            source: ContentSource::Materialized(path.clone()),
        });

        // Its backup (`volume-format-v2.md` §1.3, #418 item 2): the same
        // bytes, copied, after the operator pair and in the primaries'
        // order — a byte-for-byte copy for the reason the operator backup
        // below is one (a second encryption would be an unrelated file).
        let backup_pos = first_tenant_backup_pos + i as i32;
        let backup_path = session_dir.join(format!(
            "{backup_pos:04}_tenant_envelope_backup_{tenant_id}"
        ));
        fs::copy(&path, &backup_path)?;
        entries.push(LayoutEntry {
            position: backup_pos,
            kind: ZoneKind::TenantEnvelopeBackup { tenant_id },
            size_bytes: Some(size),
            sha256: Some(hash),
            source: ContentSource::Materialized(backup_path),
        });
    }

    // Operator envelope + backup: recipients are operator + escrow only, no
    // tenant keys (`volume-format-v2.md` §1: enc(op+esc)). The backup is
    // written as the SAME ciphertext bytes as the primary, not a second
    // independent encryption: `age::Encryptor` is randomized per call (fresh
    // ephemeral key exchange each time), so re-encrypting identical
    // plaintext to identical recipients would NOT reproduce the same bytes
    // (confirmed empirically during T5b — see the report) — it would just be
    // a second, unrelated ciphertext that happens to decrypt to the same
    // plaintext, defeating the point of a redundant copy. This mirrors
    // write.rs v1, which also clones `op_env_encrypted` for both positions.
    let all_manifest_units = build_manifest_units_all(inputs, &slice_positions);
    let op_manifest =
        layout::generate_manifest_toml(&inputs.label, "operator", &all_manifest_units);
    let all_catalogs = catalogs_for_all(inputs);
    let mut op_recovery = layout::generate_recovery_md(
        &inputs.label,
        "operator",
        &all_manifest_units,
        &catalogue_member_names(&all_catalogs),
    );
    if inputs.catalog_db_path.is_some() {
        // Issue #83: make the operator-only catalog subset discoverable
        // rather than a mystery file next to MANIFEST.toml/PLAN.toml.
        op_recovery.push_str(RECOVERY_CATALOG_DB);
    }
    // PLAN.toml survives the standalone v1 planning-header zone as an
    // operator-envelope tar member (`volume-format-v2.md` §8's "What v2
    // removes": "the standalone zone... [is] removed at the write flip"; the
    // generator itself, `generate_planning_header`, is unchanged — only its
    // caller and packaging change here).
    let plan_toml = layout::generate_planning_header(&inputs.label, &plan_units_all(inputs));

    let op_recipients = with_escrow(
        inputs.operator_public_keys.clone(),
        &inputs.escrow_public_key,
    );
    let op_index_members: Vec<(&'static str, PathBuf)> = inputs
        .index
        .iter()
        .map(|ix| (index::UNITS_MEMBER, ix.operator_units.clone()))
        .collect();
    let (op_path, op_size, op_hash) = materialize_envelope_streaming(
        session_dir,
        &format!("{op_pos:04}_operator_envelope"),
        &op_manifest,
        &op_recovery,
        &all_catalogs,
        Some(&plan_toml),
        inputs.catalog_db_path.as_deref(),
        &op_index_members,
        &op_recipients,
    )?;
    entries.push(LayoutEntry {
        position: op_pos,
        kind: ZoneKind::OperatorEnvelope,
        size_bytes: Some(op_size),
        sha256: Some(op_hash.clone()),
        source: ContentSource::Materialized(op_path.clone()),
    });

    // Backup: byte-for-byte copy of the primary, NOT a second streaming
    // encrypt. `age::Encryptor` is randomized per call and the tar layer
    // stamps `set_mtime(now)`, so re-running `materialize_envelope_streaming`
    // would produce a second, unrelated ciphertext that happens to decrypt
    // to the same plaintext — defeating the point of a redundant copy (see
    // the comment above on the pre-#87 `op_encrypted` bytes-reuse, and T5b's
    // report). Reusing the primary's already-verified size/hash keeps that
    // invariant under streaming.
    let op_backup_path = session_dir.join(format!("{op_backup_pos:04}_operator_envelope_backup"));
    fs::copy(&op_path, &op_backup_path)?;
    entries.push(LayoutEntry {
        position: op_backup_pos,
        kind: ZoneKind::OperatorEnvelopeBackup,
        size_bytes: Some(op_size),
        sha256: Some(op_hash),
        source: ContentSource::Materialized(op_backup_path),
    });

    // Data slices: unit-contiguous, in the given order. Verbatim from
    // `inputs` — no re-read, no re-hash (`volume-format-v2.md` §3: reuses
    // `stage_slices.sha256_encrypted` verbatim; `Layout::validate` is the
    // layer that re-hashes from disk, tri-layer L1).
    for slice in &all_slices {
        let pos = slice_positions[&slice.slice_id];
        entries.push(LayoutEntry {
            position: pos,
            kind: ZoneKind::Slice {
                stage_slice_id: slice.slice_id,
            },
            size_bytes: Some(slice.encrypted_bytes.max(0) as u64),
            sha256: Some(slice.sha256_encrypted.clone()),
            source: ContentSource::Staged(slice.staging_path.clone()),
        });
    }

    // Front index (File 3): every file's position/type/size/hash from the
    // completed entries above, EXCEPT the front index's own entry and the
    // seal marker's entry, which carry neither (format §3's mutual-reference
    // exclusion; `format::validate_consistency` enforces exactly this shape).
    let mut fi_files: Vec<FrontIndexFile> = Vec::with_capacity(total_files as usize);
    for pos in 0..total_files {
        if pos == front_index_pos {
            fi_files.push(FrontIndexFile {
                position: pos,
                type_label: "front_index",
                size_bytes: None,
                sha256_encrypted: None,
            });
            continue;
        }
        if pos == seal_pos {
            fi_files.push(FrontIndexFile {
                position: pos,
                type_label: "seal_marker",
                size_bytes: None,
                sha256_encrypted: None,
            });
            continue;
        }
        let entry = entries.iter().find(|e| e.position == pos).ok_or_else(|| {
            TapectlError::Other(format!("build: no entry assembled for position {pos}"))
        })?;
        fi_files.push(FrontIndexFile {
            position: pos,
            type_label: entry.kind.type_label(),
            size_bytes: entry.size_bytes,
            sha256_encrypted: entry.sha256.clone(),
        });
    }

    let fi_bytes = layout::generate_front_index(&inputs.label, &fi_files);
    let (fi_path, fi_size, fi_hash) = materialize(
        session_dir,
        &format!("{front_index_pos:04}_front_index"),
        fi_bytes.as_bytes(),
    )?;
    entries.push(LayoutEntry {
        position: front_index_pos,
        kind: ZoneKind::FrontIndex,
        // The Layout knows what File 3's own [[files]] entry omits:
        // Store::confirm's chain walk (store.rs) needs this exact on-tape
        // length to trim File 3's padded tape bytes before hashing
        // (`volume-format-v2.md` §4 byte contract; verified against
        // store.rs's `chain_walk`, which errors without it).
        size_bytes: Some(fi_size),
        sha256: Some(fi_hash.clone()),
        source: ContentSource::Materialized(fi_path),
    });

    // Seal marker (File M, last): the embedded copy is File 3's list with
    // File 3's OWN entry upgraded to carry its real size+hash (now known);
    // the seal's own entry stays bare, unchanged — still neither size nor
    // hash, exactly as it already was in File 3's list, avoiding the
    // self-reference fixpoint the same way File 3 does (format §4: "only
    // the seal marker's own entry stays hash-less").
    let mut seal_files = fi_files.clone();
    if let Some(fi_list_entry) = seal_files
        .iter_mut()
        .find(|f| f.position == front_index_pos)
    {
        fi_list_entry.size_bytes = Some(fi_size);
        fi_list_entry.sha256_encrypted = Some(fi_hash.clone());
    }

    // §9 micro-decision, as corrected 2026-07-22: the seal is generated once
    // here as a SIZE PLACEHOLDER; a later seal() regenerates it with the real
    // `sealed_at` and must produce byte-identical length. That holds because
    // `generate_seal_marker` renders its timestamp at fixed width
    // (SecondsFormat::Secs + use_z -> exactly 20 bytes) — see the comment on
    // that function for why plain `to_rfc3339()` (AutoSi, variable-width) was
    // wrong and would have failed a real reseal roughly once per ~900 writes.
    // The regenerate-and-compare below is now a genuine invariant check
    // rather than a probabilistic canary: with a fixed-width timestamp ANY
    // length difference is a real defect (a changed generator, a non-UTC
    // clock), so failing the build here is correct.
    let seal_bytes = layout::generate_seal_marker_requiring(
        &inputs.label,
        total_files,
        &fi_hash,
        &seal_files,
        requires,
    );
    let seal_bytes_recheck = layout::generate_seal_marker_requiring(
        &inputs.label,
        total_files,
        &fi_hash,
        &seal_files,
        requires,
    );
    if seal_bytes.len() != seal_bytes_recheck.len() {
        return Err(TapectlError::Other(format!(
            "build: seal marker placeholder sizing broke — two generate_seal_marker calls \
             produced different byte lengths ({} vs {}). The seal timestamp must render at \
             fixed width (SecondsFormat::Secs); a later reseal would fail its length-identity \
             check. See generate_seal_marker in layout.rs.",
            seal_bytes.len(),
            seal_bytes_recheck.len()
        )));
    }

    let (seal_path, seal_size, _placeholder_hash_would_be_meaningless) = materialize(
        session_dir,
        &format!("{seal_pos:04}_seal_marker"),
        seal_bytes.as_bytes(),
    )?;
    entries.push(LayoutEntry {
        position: seal_pos,
        kind: ZoneKind::SealMarker,
        size_bytes: Some(seal_size),
        // No hash: the real on-tape bytes will differ once a real seal step
        // substitutes the true `sealed_at` for this placeholder, so hashing
        // the placeholder would record a claim the eventual tape bytes can
        // never satisfy. Capacity accounting uses size only
        // (`Layout::on_tape_bytes`); the length-identity check above is what
        // keeps that honest across the later swap.
        sha256: None,
        source: ContentSource::Materialized(seal_path),
    });

    entries.sort_by_key(|e| e.position);

    // The span's tail (ADR-0012 item 123, `CapacityBudget::span_tail_from`):
    // the last unit, when it may span, is the one unit the capacity gate lets
    // pass the plan bound. The plan order puts the tail last
    // (`CapacityBudget::place`), and a whole unit last is never a tail.
    let span_tail_from = inputs
        .units
        .last()
        .filter(|u| u.span.is_some())
        .and_then(|u| u.slices.first())
        .map(|s| slice_positions[&s.slice_id]);

    let layout = Layout {
        label: inputs.label.clone(),
        volume_uuid: inputs.volume_uuid.clone(),
        media_type: inputs.media_type.clone(),
        block_size: inputs.block_size,
        budget: CapacityBudget {
            available_bytes: inputs.usable_bytes,
            reserve_bytes: inputs.enospc_buffer,
            plan_bound_bytes,
            span_tail_from,
        },
        entries,
    };

    // The rehydration sidecar (issue #25). Written AFTER every entry is
    // materialized, so it describes the frozen bytes exactly as they landed.
    //
    // This is a staging-side artifact: it is deliberately NOT a
    // `LayoutEntry`, occupies no tape position, and never reaches the medium
    // — `entries` is already final above and nothing here appends to it. The
    // name `layout.json` cannot collide with a generated zone's filename
    // either, since those are all `{position:04}_{name}`.
    //
    // Its reason for existing: a resume after a process restart must re-hash
    // these frozen files, never re-run `build()`. `BuildInputs::created_at`
    // (`chrono::Utc::now()` at call time, persisted nowhere) and `mam_loads`
    // (increments on every cartridge load) both drift across a restart, so a
    // regenerated Layout would carry different ID-thunk bytes than the tape
    // already holds and `SealedPending::confirm` would quarantine a good
    // volume. `docs/design/layout-session.md`'s Resume rule requires the
    // frozen zones to "re-hash byte-identical"; this is what makes that
    // reachable from a cold start.
    let sidecar = session_dir.join(LAYOUT_SIDECAR);
    let json = serde_json::to_vec_pretty(&layout)
        .map_err(|e| TapectlError::Other(format!("build: serializing the layout sidecar: {e}")))?;
    fs::write(&sidecar, &json)
        .map_err(|e| TapectlError::Other(format!("build: writing {}: {e}", sidecar.display())))?;

    Ok(BuiltLayout {
        layout,
        session_dir: session_dir.to_path_buf(),
    })
}

/// Filename of the rehydration sidecar `build()` writes into the session
/// staging directory, and that `InterruptedSession::rehydrate` reads back
/// (issue #25). Not a tape file — see the write site above.
pub const LAYOUT_SIDECAR: &str = "layout.json";

// ── BuiltLayout::validate ──

impl BuiltLayout {
    /// The full pre-write predicate: `Layout::validate`'s points (capacity,
    /// staged-slice size — plus full hash under [`SliceCheck::FullHash`],
    /// `--prewrite-hash` — materialized-zone size/hash, keys —
    /// `layout_model.rs`) plus the parts that need a file read and a
    /// subprocess, which don't belong in that dependency-light module
    /// (`layout-session.md` validation point 4 / plan T5b point 3): the
    /// front index parses and is internally consistent
    /// (`format::parse_front_index` + `format::validate_consistency`, must
    /// be violation-free), the seal marker parses
    /// (`format::parse_seal_marker`), and RESTORE.sh passes `bash -n`.
    /// Collects every failure — never stops at the first, matching
    /// `Layout::validate`'s pre-flight-report convention.
    pub fn validate(
        &self,
        keys: &KeyAvailability,
        slice_check: SliceCheck,
    ) -> std::result::Result<(), Vec<LayoutError>> {
        let mut errs = Vec::new();
        if let Err(mut layout_errs) = self.layout.validate(keys, slice_check) {
            errs.append(&mut layout_errs);
        }
        self.check_front_index_parses(&mut errs);
        self.check_seal_marker_parses(&mut errs);
        self.check_restore_sh_syntax(&mut errs);
        if errs.is_empty() {
            Ok(())
        } else {
            Err(errs)
        }
    }

    /// How `units` — the ones this Layout was built from, in the order
    /// given to [`build`] — sit against its budget (ADR-0012 item 113,
    /// [`Layout::split_units`]): `units[split.past_ceiling()]` are planned
    /// past the fill ceiling ("the tape may end before them"), and a split
    /// that does not [`PlanSplit::fits`] is a Layout `validate` refuses.
    pub fn split(&self, units: &[BuildUnit]) -> PlanSplit {
        self.layout.split_units(&unit_slice_ids(units))
    }

    /// The data slices of the units planned past the fill ceiling, in
    /// position order ([`Layout::entries_past_ceiling`]).
    pub fn entries_past_ceiling(&self, units: &[BuildUnit]) -> Vec<&LayoutEntry> {
        self.layout.entries_past_ceiling(&unit_slice_ids(units))
    }

    /// Where `units` sit in a span tape's plan order (`layout-session.md`
    /// "Spanning"; [`CapacityBudget::place`], [`Layout::place_units`]), each
    /// with its [`PlanClass`] from the caller: `classes[i]` is `units[i]`'s.
    /// The class is the caller's to give because only the catalog knows it:
    /// an owed run and a fresh spannable set can be the same slices (item
    /// 93's second tape owes the whole set), and a [`BuildUnit`] says only
    /// that a unit may span. The placement describes this Layout when it was
    /// built from the units in [`Placement::order`], with none deferred.
    pub fn place(&self, units: &[BuildUnit], classes: &[PlanClass]) -> Placement {
        self.layout.place_units(&unit_slice_ids(units), classes)
    }
}

/// Each unit's slices as `stage_slice_id`s, in the units' order — the unit
/// boundaries [`Layout::split_units`] needs and a Layout does not carry.
pub fn unit_slice_ids(units: &[BuildUnit]) -> Vec<Vec<i64>> {
    units
        .iter()
        .map(|u| u.slices.iter().map(|s| s.slice_id).collect())
        .collect()
}

impl BuiltLayout {
    /// The position + materialized path of the (at most one) entry whose
    /// kind matches `kind_matches`, if it is `Materialized`. Staged entries
    /// never match any of this function's callers' predicates (none of
    /// `FrontIndex`/`SealMarker`/`RestoreSh` is ever a slice), so this never
    /// silently skips a real generated-zone check.
    fn materialized_path_for(
        &self,
        kind_matches: impl Fn(&ZoneKind) -> bool,
    ) -> Option<(i32, &Path)> {
        self.layout.entries.iter().find_map(|e| {
            if !kind_matches(&e.kind) {
                return None;
            }
            match &e.source {
                ContentSource::Materialized(path) => Some((e.position, path.as_path())),
                _ => None,
            }
        })
    }

    fn check_front_index_parses(&self, errs: &mut Vec<LayoutError>) {
        let Some((position, path)) =
            self.materialized_path_for(|k| matches!(k, ZoneKind::FrontIndex))
        else {
            return;
        };
        let raw = match fs::read_to_string(path) {
            Ok(r) => r,
            Err(e) => {
                errs.push(LayoutError::Io {
                    path: path.to_path_buf(),
                    message: e.to_string(),
                });
                return;
            }
        };
        match format::parse_front_index(&raw) {
            Ok(parsed) => {
                let violations = format::validate_consistency(&parsed);
                if !violations.is_empty() {
                    errs.push(LayoutError::GeneratedZoneInconsistent {
                        position,
                        message: violations
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join("; "),
                    });
                }
            }
            Err(e) => errs.push(LayoutError::GeneratedZoneUnparseable {
                position,
                label: "front_index",
                message: e.to_string(),
            }),
        }
    }

    fn check_seal_marker_parses(&self, errs: &mut Vec<LayoutError>) {
        let Some((position, path)) =
            self.materialized_path_for(|k| matches!(k, ZoneKind::SealMarker))
        else {
            return;
        };
        let raw = match fs::read_to_string(path) {
            Ok(r) => r,
            Err(e) => {
                errs.push(LayoutError::Io {
                    path: path.to_path_buf(),
                    message: e.to_string(),
                });
                return;
            }
        };
        if let Err(e) = format::parse_seal_marker(&raw) {
            errs.push(LayoutError::GeneratedZoneUnparseable {
                position,
                label: "seal_marker",
                message: e.to_string(),
            });
        }
    }

    fn check_restore_sh_syntax(&self, errs: &mut Vec<LayoutError>) {
        let Some((_, path)) = self.materialized_path_for(|k| matches!(k, ZoneKind::RestoreSh))
        else {
            return;
        };
        match std::process::Command::new("bash")
            .arg("-n")
            .arg(path)
            .output()
        {
            Ok(out) if out.status.success() => {}
            Ok(out) => errs.push(LayoutError::RestoreScriptSyntaxError(
                String::from_utf8_lossy(&out.stderr).into_owned(),
            )),
            Err(e) => errs.push(LayoutError::Io {
                path: path.to_path_buf(),
                message: e.to_string(),
            }),
        }
    }
}

// ── helpers ──

/// Append the escrow public key to `recipients` if one is present — every
/// recipient list `build()` assembles (tenant envelopes, the operator
/// envelope) ends with it when it exists (`volume-format-v2.md` §1: "esc =
/// the permanent Escrow Recipient (ADR-0005) — in every encryption").
/// Extracted so the tenant-envelope and operator-envelope recipient
/// assemblies can't drift on this (T6 review finding #2: both previously
/// repeated this identical 3-liner). `build.rs` stays DB-free by design
/// (no `conn`), so this can't reuse `queries::recipient_list_with_escrow`
/// (which does its own DB lookup) — it operates on the already-resolved
/// `Option<String>` `BuildInputs.escrow_public_key` instead.
/// The operator envelope's `RECOVERY.md` section on its `catalog.db`
/// (issue #83): what the file holds, for an operator with only `sqlite3`,
/// and queries that work on it. It describes generation 3
/// (`volume-format-v2.md` §1.4), the shape `db::ontape_catalog::write`
/// makes; `recovery_md_catalog_db_queries_run_on_what_write_makes` runs each
/// query against that writer's output.
const RECOVERY_CATALOG_DB: &str = "\n## Querying catalog.db\n\n\
     This envelope also carries `catalog.db`, a small standalone SQLite \
     database (no tapectl or the source `tapectl.db` needed) covering every \
     unit written to THIS volume, across all tenants — operator-only, never \
     in a tenant envelope. Tables: `tenants`, `units`, `snapshots`, \
     `stage_sets` (with each set's recorded recipient list, `key_fingerprints`), \
     `stage_slices` (with `sha256_plain`), `paths` (each path once per unit, \
     relative to the snapshot's `source_path`) and `file_versions` (one row per \
     entry of each version: `kind` 0 directory, 1 regular file, 2 symlink, \
     3 special file such as a FIFO or device; `size_bytes`; `mtime_ns`, Unix \
     time in nanoseconds; `sha256` as 32 raw bytes, `lower(hex(sha256))` \
     prints it; `link_target`, a symlink's target). Permissions, owners and \
     extended attributes are in the dar catalogues beside this file, not here. \
     Tape positions are NOT here; MANIFEST.toml beside this file has them. \
     `PRAGMA user_version` on this file mirrors the schema level of the \
     `tapectl.db` it was generated from. With the operator or escrow key, \
     `tapectl catalog rebuild --from-volume` reconstructs the catalog from \
     this envelope. Examples: each unit's versions and their entry counts, \
     then one unit's entries (put its name in place of UNIT):\n\n\
     ```bash\n\
     sqlite3 catalog.db 'SELECT u.name, s.version, count(fv.path_id) \
     FROM units u JOIN snapshots s ON s.unit_id = u.id \
     LEFT JOIN file_versions fv ON fv.snapshot_id = s.id GROUP BY s.id;'\n\
     sqlite3 catalog.db \"SELECT p.path, CASE fv.kind WHEN 0 THEN 'dir' \
     WHEN 1 THEN 'regular' WHEN 2 THEN 'symlink' ELSE 'special' END, \
     fv.size_bytes, lower(hex(fv.sha256)), \
     datetime(fv.mtime_ns / 1000000000, 'unixepoch'), fv.link_target \
     FROM file_versions fv JOIN paths p ON p.id = fv.path_id \
     JOIN snapshots s ON s.id = fv.snapshot_id JOIN units u ON u.id = s.unit_id \
     WHERE u.name = 'UNIT' ORDER BY s.version, p.path;\"\n\
     ```\n";

fn with_escrow(mut recipients: Vec<String>, escrow_public_key: &Option<String>) -> Vec<String> {
    if let Some(escrow) = escrow_public_key {
        recipients.push(escrow.clone());
    }
    recipients
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

/// Write `bytes` to `session_dir/filename`, returning its path, exact size,
/// and sha256 — the materialize-to-staging step every generated zone goes
/// through exactly once (v2-open-questions.md §2.2).
fn materialize(session_dir: &Path, filename: &str, bytes: &[u8]) -> Result<(PathBuf, u64, String)> {
    let path = session_dir.join(filename);
    fs::write(&path, bytes)?;
    Ok((path, bytes.len() as u64, sha256_hex(bytes)))
}

/// The §2.1 permutation key: hex(sha256(volume_uuid_bytes || 0x00 ||
/// le64(tenant_id))). Plain SHA-256, not HMAC — `volume_uuid` is already on
/// the tape, so there is no secret to protect; the goal is only to
/// decorrelate on-tape envelope order from the raw `tenant_id` sequence.
/// Returned as a hex string (rather than the raw 32 bytes) to match the
/// spec's "hex compare" wording literally, even though comparing the raw
/// bytes lexicographically would give an identical order.
fn tenant_permutation_key(volume_uuid: &Uuid, tenant_id: i64) -> String {
    let mut buf = Vec::with_capacity(16 + 1 + 8);
    buf.extend_from_slice(volume_uuid.as_bytes());
    buf.push(0u8);
    buf.extend_from_slice(&(tenant_id as u64).to_le_bytes());
    sha256_hex(&buf)
}

/// A span must list every slice the unit plans on this tape, as the unit
/// states it, in slice-number order (ADR-0012 item 125): otherwise the
/// MANIFEST would omit a slice the tape holds, or state hashes the tape's
/// bytes do not have.
fn check_span(unit: &BuildUnit) -> Result<()> {
    let Some(span) = &unit.span else {
        return Ok(());
    };
    let refuse = |why: String| {
        Err(TapectlError::Other(format!(
            "build: unit {} (stage set {}): {why}",
            unit.unit_name, unit.stage_set_id
        )))
    };
    if let Some(pair) = span
        .slices
        .windows(2)
        .find(|w| w[0].slice_number >= w[1].slice_number)
    {
        return refuse(format!(
            "its span lists slice {} before slice {}, not in slice-number order",
            pair[0].slice_number, pair[1].slice_number
        ));
    }
    for planned in &unit.slices {
        match span.slices.iter().find(|s| s.slice_id == planned.slice_id) {
            None => {
                return refuse(format!(
                    "its span does not list slice {} (id {}), which this tape plans",
                    planned.slice_number, planned.slice_id
                ))
            }
            Some(listed) if *listed != SpanSlice::from(planned) => {
                return refuse(format!(
                    "its span states slice {} (id {}) differently from the slice this tape \
                     plans",
                    planned.slice_number, planned.slice_id
                ))
            }
            Some(_) => {}
        }
    }
    Ok(())
}

/// A unit's MANIFEST.toml block. A whole unit's lists the slices it plans,
/// each with its position; a span's (`may_span = true`) lists every slice of
/// the set, with a position only where this tape plans one (ADR-0012 item
/// 125).
fn to_manifest_unit(unit: &BuildUnit, positions: &HashMap<i64, i32>) -> layout::ManifestUnit {
    let manifest_slice = |sl: SpanSlice| layout::ManifestSlice {
        number: sl.slice_number,
        tape_position: positions.get(&sl.slice_id).copied().map(i64::from),
        size_bytes: sl.size_bytes,
        encrypted_bytes: sl.encrypted_bytes,
        sha256_plain: sl.sha256_plain,
        sha256_encrypted: sl.sha256_encrypted,
    };
    let slices = match &unit.span {
        Some(span) => span.slices.iter().cloned().map(manifest_slice).collect(),
        None => unit
            .slices
            .iter()
            .map(SpanSlice::from)
            .map(manifest_slice)
            .collect(),
    };
    layout::ManifestUnit {
        name: unit.unit_name.clone(),
        uuid: unit.unit_uuid.clone(),
        snapshot_version: unit.snapshot_version,
        stage_set_id: unit.stage_set_id,
        may_span: unit.span.is_some(),
        dar_version: unit.dar_version.clone(),
        dar_command: unit.dar_command.clone(),
        slices,
    }
}

fn build_manifest_units(
    inputs: &BuildInputs,
    tenant_id: i64,
    positions: &HashMap<i64, i32>,
) -> Vec<layout::ManifestUnit> {
    inputs
        .units
        .iter()
        .filter(|u| u.tenant_id == tenant_id)
        .map(|u| to_manifest_unit(u, positions))
        .collect()
}

fn build_manifest_units_all(
    inputs: &BuildInputs,
    positions: &HashMap<i64, i32>,
) -> Vec<layout::ManifestUnit> {
    inputs
        .units
        .iter()
        .map(|u| to_manifest_unit(u, positions))
        .collect()
}

/// The bare `catalogs/` member names an envelope will carry, for RECOVERY.md.
fn catalogue_member_names(catalogs: &[(String, PathBuf)]) -> Vec<String> {
    catalogs.iter().map(|(name, _)| name.clone()).collect()
}

fn catalogs_for_tenant(inputs: &BuildInputs, tenant_id: i64) -> Vec<(String, PathBuf)> {
    inputs
        .units
        .iter()
        .filter(|u| u.tenant_id == tenant_id)
        .filter_map(|u| u.catalog_path.as_deref())
        .flat_map(catalog_file_paths)
        .collect()
}

fn catalogs_for_all(inputs: &BuildInputs) -> Vec<(String, PathBuf)> {
    inputs
        .units
        .iter()
        .filter_map(|u| u.catalog_path.as_deref())
        .flat_map(catalog_file_paths)
        .collect()
}

/// The index members of `tenant_id`'s envelope: that tenant's own
/// `index/units.tsv` and `index/paths.tsv` and nothing of another tenant's
/// (§1.5). Empty when the build carries no index. A build that carries one
/// but has no files for a tenant with an envelope is a defect, refused here
/// rather than written as a tape whose tenant index silently is not there.
fn tenant_index_members(
    inputs: &BuildInputs,
    tenant_id: i64,
    tenant_name: &str,
) -> Result<Vec<(&'static str, PathBuf)>> {
    let Some(ix) = &inputs.index else {
        return Ok(Vec::new());
    };
    let files = ix.tenants.get(&tenant_id).ok_or_else(|| {
        TapectlError::Other(format!(
            "build: the cross-volume index has no files for tenant {tenant_id} \
             (\"{tenant_name}\"), which has an envelope on this tape"
        ))
    })?;
    Ok(vec![
        (index::UNITS_MEMBER, files.units.clone()),
        (index::PATHS_MEMBER, files.paths.clone()),
    ])
}

/// `(unit_name, uuid, num_slices, total_bytes)` for every unit in this batch —
/// `generate_planning_header`'s input shape, mirroring v1's `write.rs`
/// `plan_units` construction. Operator-only (the whole-batch view), never
/// per-tenant.
fn plan_units_all(inputs: &BuildInputs) -> Vec<(String, String, i64, i64)> {
    inputs
        .units
        .iter()
        .map(|u| {
            (
                u.unit_name.clone(),
                u.unit_uuid.clone(),
                u.slices.len() as i64,
                u.slices.iter().map(|sl| sl.encrypted_bytes).sum(),
            )
        })
        .collect()
}

/// Find a unit's isolated dar catalogue slice files (`catalog_base.N.dar`)
/// for inclusion in an envelope tar, returning names and paths rather than
/// bytes (issue #87: catalogue bytes stream straight into the envelope tar
/// at materialize time, never collected here). Mirrors
/// `write.rs::read_catalog_files`'s discovery logic (private there —
/// replicated here per the T5b scope fence rather than widening write.rs's
/// visibility, which is out of scope for this task).
fn catalog_file_paths(catalog_path: &str) -> Vec<(String, PathBuf)> {
    let base = std::path::Path::new(catalog_path);
    let (Some(dir), Some(stem)) = (base.parent(), base.file_name().and_then(|f| f.to_str())) else {
        return Vec::new();
    };
    let prefix = format!("{stem}.");
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let fname = e.file_name().to_string_lossy().into_owned();
            if fname.starts_with(&prefix) && fname.ends_with(".dar") {
                out.push((fname, e.path()));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Build an envelope tar directly into the age encryption stream, which
/// writes directly to `session_dir/filename` — issue #87's single-pass
/// streaming redesign of the old buffer-the-whole-tar-then-encrypt-the-
/// whole-thing path (`staging::encrypt_data`, which survives only as a
/// test-facing reference implementation — see its doc comment for why it
/// could not be `#[cfg(test)]`-gated). The wrapping order, innermost-to-disk
/// first, is:
///
/// `File` → [`crate::util::HashingWriter`] → age `StreamWriter` →
/// `tar::Builder`
///
/// `HashingWriter` sits on the *ciphertext* side (closest to the file), so
/// the sha256 this returns is the ciphertext hash the front index must
/// record (`docs/design/volume-format-v2.md` §1/§3) — matching
/// `staging::encrypt_file_streaming`'s same layering for data slices (H9
/// fix, issue #35). Only MANIFEST.toml, RECOVERY.md, and PLAN.toml (all
/// small, generated strings) are buffered as `&str`; every catalogue member
/// streams straight from its file on disk via `append_tar_file_member`, so
/// peak RAM tracks `staging::STREAM_COPY_BUFFER` plus age's own constant
/// STREAM chunk buffers, never total catalogue size.
///
/// Recipients determine which envelope this is: tenant envelopes pass this
/// tenant's keys + operator + escrow with `plan_toml: None` (PLAN.toml is
/// operator-only); the operator envelope passes operator + escrow with
/// `plan_toml: Some(..)`.
///
/// `catalog_db_path`, likewise, is operator-only (issue #83): tenant
/// envelopes always pass `None` here — the filtered `catalog.db` covers
/// every tenant's units on this volume, so it may only ever land in the
/// OPERATOR envelope. See `BuildInputs::catalog_db_path`'s doc comment.
///
/// `index_members` are the cross-volume index's files (§1.5): `(tar member
/// name, file on disk)`, streamed in last, after `catalogs/` and
/// `catalog.db`. They are written before the envelope's size and hash are
/// read, so everything downstream (the front index, the capacity gate) sees
/// the envelope as it will be on the tape.
#[allow(clippy::too_many_arguments)]
fn materialize_envelope_streaming(
    session_dir: &Path,
    filename: &str,
    manifest: &str,
    recovery: &str,
    catalogs: &[(String, PathBuf)],
    plan_toml: Option<&str>,
    catalog_db_path: Option<&Path>,
    index_members: &[(&'static str, PathBuf)],
    recipients: &[String],
) -> Result<(PathBuf, u64, String)> {
    let path = session_dir.join(filename);
    let result = materialize_envelope_streaming_inner(
        &path,
        manifest,
        recovery,
        catalogs,
        plan_toml,
        catalog_db_path,
        index_members,
        recipients,
    );
    if result.is_err() {
        // Mirrors `staging::encrypt_file_streaming`'s cleanup: the buffered
        // predecessor could never leave a partial envelope behind, because it
        // only wrote once the whole ciphertext existed in RAM. Streaming can,
        // so remove it rather than leaving a truncated `.age` in the session
        // dir for a later resume to trip over. Not an integrity concern
        // either way — an error propagates out of `build`, so no LayoutEntry
        // ever references this path.
        let _ = fs::remove_file(&path);
    }
    result.map(|(size, hash)| (path, size, hash))
}

#[allow(clippy::too_many_arguments)]
fn materialize_envelope_streaming_inner(
    path: &Path,
    manifest: &str,
    recovery: &str,
    catalogs: &[(String, PathBuf)],
    plan_toml: Option<&str>,
    catalog_db_path: Option<&Path>,
    index_members: &[(&'static str, PathBuf)],
    recipients: &[String],
) -> Result<(u64, String)> {
    let file = fs::File::create(path)?;
    let hashing_output = crate::util::HashingWriter::new(file);

    let encryptor = staging::build_encryptor(recipients)?;
    let mut age_writer = encryptor
        .wrap_output(hashing_output)
        .map_err(|e| TapectlError::Encryption(format!("wrap_output failed: {e}")))?;

    {
        let mut builder = tar::Builder::new(&mut age_writer);
        append_tar_member(&mut builder, "MANIFEST.toml", manifest.as_bytes())?;
        append_tar_member(&mut builder, "RECOVERY.md", recovery.as_bytes())?;
        if let Some(plan) = plan_toml {
            append_tar_member(&mut builder, "PLAN.toml", plan.as_bytes())?;
        }
        for (name, catalog_path) in catalogs {
            append_tar_file_member(&mut builder, &format!("catalogs/{name}"), catalog_path)?;
        }
        if let Some(catalog_db) = catalog_db_path {
            // Operator-only (gated by the caller passing `None` for tenant
            // envelopes) — never `append_tar_member`/`append_file`: this file
            // is unbounded (one `files` row per archived file), so it must
            // stream from disk exactly like the per-unit dar catalogues
            // above (issue #87's H9-class RAM concern).
            append_tar_file_member(&mut builder, "catalog.db", catalog_db)?;
        }
        // The cross-volume index (§1.5): after `catalogs/`, streamed from the
        // work dir like the members above (`paths.tsv` has a row per file of
        // every listed version).
        for (member, file) in index_members {
            append_tar_file_member(&mut builder, member, file)?;
        }
        builder
            .finish()
            .map_err(|e| TapectlError::Other(format!("envelope tar finish: {e}")))?;
    }

    // Mandatory: without this the STREAM's final chunk is never written,
    // producing a file that hashes fine but cannot be decrypted (mirrors
    // `staging::encrypt_file_streaming_inner`'s identical comment).
    let hashing_output = age_writer
        .finish()
        .map_err(|e| TapectlError::Encryption(format!("finish failed: {e}")))?;
    let hash = hashing_output.finalize_hex();
    let size = hashing_output.bytes_written();
    Ok((size, hash))
}

fn append_tar_member<W: std::io::Write>(
    builder: &mut tar::Builder<W>,
    path: &str,
    bytes: &[u8],
) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header
        .set_path(path)
        .map_err(|e| TapectlError::Other(format!("tar path {path}: {e}")))?;
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(chrono::Utc::now().timestamp() as u64);
    header.set_cksum();
    builder
        .append(&header, bytes)
        .map_err(|e| TapectlError::Other(format!("tar append {path}: {e}")))?;
    Ok(())
}

/// File-backed sibling of `append_tar_member`: streams `file_path`'s bytes
/// into the tar member instead of taking them as an in-memory slice, so
/// catalogue members (which scale with file count, unboundedly for the
/// operator envelope) never sit fully in RAM. Uses the exact same
/// `tar::Header::new_gnu()` hand-rolled header shape as `append_tar_member`
/// — deliberately *not* `tar::Builder::append_file`/`append_path_with_name`,
/// which can emit pax/ustar extension records the bash-`tar` heir recovery
/// path (`RESTORE.sh`) was never proven against (issue #87).
fn append_tar_file_member<W: std::io::Write>(
    builder: &mut tar::Builder<W>,
    path: &str,
    file_path: &Path,
) -> Result<()> {
    let file = fs::File::open(file_path)?;
    let size = file.metadata()?.len();

    let mut header = tar::Header::new_gnu();
    header
        .set_path(path)
        .map_err(|e| TapectlError::Other(format!("tar path {path}: {e}")))?;
    header.set_size(size);
    header.set_mode(0o644);
    header.set_mtime(chrono::Utc::now().timestamp() as u64);
    header.set_cksum();
    builder
        .append(&header, file)
        .map_err(|e| TapectlError::Other(format!("tar append {path}: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const BS: u64 = 512 * 1024;

    fn sha_hex(bytes: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(bytes);
        format!("{:x}", h.finalize())
    }

    /// Write a tiny fake staged (already "encrypted") slice file to disk and
    /// return a `BuildSlice` describing it, with size/hash matching the
    /// actual bytes on disk (so `Layout::validate`'s full-hash check
    /// passes).
    fn fake_slice(dir: &Path, slice_id: i64, slice_number: i64, content: &[u8]) -> BuildSlice {
        let path = dir.join(format!("slice_{slice_id}.age"));
        std::fs::File::create(&path)
            .unwrap()
            .write_all(content)
            .unwrap();
        BuildSlice {
            slice_id,
            slice_number,
            size_bytes: content.len() as i64,
            encrypted_bytes: content.len() as i64,
            sha256_plain: sha_hex(b"plaintext hash is not exercised by this fixture"),
            sha256_encrypted: sha_hex(content),
            staging_path: path,
        }
    }

    fn fake_tenant_and_unit(
        dir: &Path,
        tenant_id: i64,
        name: &str,
        slices: Vec<(i64, i64, &[u8])>,
    ) -> (TenantInfo, BuildUnit) {
        let keypair = crate::crypto::keys::generate_keypair();
        let built_slices = slices
            .into_iter()
            .map(|(slice_id, slice_number, content)| {
                fake_slice(dir, slice_id, slice_number, content)
            })
            .collect();
        let tenant = TenantInfo {
            tenant_id,
            tenant_name: name.to_string(),
            public_keys: vec![keypair.public_key],
        };
        let unit = BuildUnit {
            stage_set_id: tenant_id * 100,
            snapshot_id: tenant_id * 100,
            unit_name: format!("unit-{name}"),
            unit_uuid: Uuid::new_v4().to_string(),
            tenant_id,
            dar_version: Some("2.7.20".to_string()),
            dar_command: Some("dar -c base -R /src".to_string()),
            catalog_path: None,
            snapshot_version: 1,
            slices: built_slices,
            span: None,
        };
        (tenant, unit)
    }

    fn base_inputs(
        volume_uuid: &str,
        tenants: Vec<TenantInfo>,
        units: Vec<BuildUnit>,
    ) -> BuildInputs {
        let op_key = crate::crypto::keys::generate_keypair();
        BuildInputs {
            label: "T5BTEST".to_string(),
            volume_uuid: volume_uuid.to_string(),
            media_type: "LTO-6".to_string(),
            tapectl_version: "0.1.0-test".to_string(),
            created_at: "2026-07-22T20:09:00Z".to_string(),
            block_size: BS,
            usable_bytes: 100 * BS,
            enospc_buffer: BS,
            nominal_capacity: 2_400_000_000_000,
            mam_capacity: 0,
            mam_manufacturer: String::new(),
            mam_serial: String::new(),
            cartridge_identity_source: None,
            mam_length: 0,
            mam_loads: 0,
            units,
            tenants,
            operator_public_keys: vec![op_key.public_key],
            escrow_public_key: None,
            catalog_db_path: None,
            index: None,
        }
    }

    /// A small 2-tenant fixture (tenant 1 "alpha" with one slice, tenant 2
    /// "bravo" with one slice), with tiny fake staged slice files on disk.
    /// Returns the inputs plus the `TempDir` guard for the slice source
    /// files (must outlive any `build()`/`validate()` call against the
    /// returned inputs).
    fn two_tenant_inputs(volume_uuid: &str) -> (BuildInputs, tempfile::TempDir) {
        let src = tempfile::tempdir().unwrap();
        let (t1, u1) =
            fake_tenant_and_unit(src.path(), 1, "alpha", vec![(1, 1, b"slice one bytes")]);
        let (t2, u2) =
            fake_tenant_and_unit(src.path(), 2, "bravo", vec![(2, 1, b"slice two bytes")]);
        let inputs = base_inputs(volume_uuid, vec![t1, t2], vec![u1, u2]);
        (inputs, src)
    }

    fn ok_keys(inputs: &BuildInputs) -> KeyAvailability {
        let mut tenant_ids: Vec<i64> = inputs.tenants.iter().map(|t| t.tenant_id).collect();
        tenant_ids.sort_unstable();
        KeyAvailability {
            tenant_ids: tenant_ids.clone(),
            tenants_with_active_key: tenant_ids.into_iter().collect(),
            operator_key_present: true,
            escrow_recipient_present: None,
            stage_sets_lacking_escrow: None,
        }
    }

    // ── entry order (format §1) ──

    #[test]
    fn entry_order_matches_format_section_1() {
        // uuid chosen with no regard to permutation direction here — this
        // test checks structural order (kinds at positions), not which
        // tenant_id lands in which envelope slot (that's the permutation
        // test below).
        let (inputs, _src) = two_tenant_inputs("550e8400-e29b-41d4-a716-446655440000");
        let session = tempfile::tempdir().unwrap();
        let built = build(&inputs, session.path()).expect("build succeeds");
        let entries = &built.layout.entries;

        // 0 id_thunk, 1 guide, 2 restore_sh, 3 front_index, 4-5 tenant
        // envelopes, 6 operator, 7 operator backup, 8-9 the tenant
        // envelopes' backups (§1.3), 10-11 slices, 12 seal.
        assert_eq!(entries.len(), 13);
        for (i, e) in entries.iter().enumerate() {
            assert_eq!(
                e.position, i as i32,
                "position gap/out-of-order at index {i}"
            );
        }

        assert_eq!(entries[0].kind, ZoneKind::IdThunk);
        assert_eq!(entries[1].kind, ZoneKind::SystemGuide);
        assert_eq!(entries[2].kind, ZoneKind::RestoreSh);
        assert_eq!(entries[3].kind, ZoneKind::FrontIndex);
        assert!(matches!(entries[4].kind, ZoneKind::TenantEnvelope { .. }));
        assert!(matches!(entries[5].kind, ZoneKind::TenantEnvelope { .. }));
        assert_eq!(entries[6].kind, ZoneKind::OperatorEnvelope);
        assert_eq!(entries[7].kind, ZoneKind::OperatorEnvelopeBackup);
        assert!(matches!(
            entries[8].kind,
            ZoneKind::TenantEnvelopeBackup { .. }
        ));
        assert!(matches!(
            entries[9].kind,
            ZoneKind::TenantEnvelopeBackup { .. }
        ));
        assert_eq!(entries[10].kind, ZoneKind::Slice { stage_slice_id: 1 });
        assert_eq!(entries[11].kind, ZoneKind::Slice { stage_slice_id: 2 });
        assert_eq!(entries[12].kind, ZoneKind::SealMarker);

        // Each backup is its primary's bytes, in the primaries' order.
        for i in 0..2 {
            let (primary, backup) = (&entries[4 + i], &entries[8 + i]);
            let (
                ZoneKind::TenantEnvelope { tenant_id: p },
                ZoneKind::TenantEnvelopeBackup { tenant_id: b },
            ) = (&primary.kind, &backup.kind)
            else {
                panic!("expected a tenant envelope and its backup");
            };
            assert_eq!(p, b, "backup {i} is not in the primaries' order");
            assert_eq!(primary.size_bytes, backup.size_bytes);
            assert_eq!(primary.sha256, backup.sha256);
            let read = |e: &LayoutEntry| match &e.source {
                ContentSource::Materialized(path) => fs::read(path).unwrap(),
                other => panic!("an envelope is materialized, not {other:?}"),
            };
            assert_eq!(read(primary), read(backup), "a backup is a byte copy");
        }

        // Envelopes strictly precede slices; seal marker is strictly last.
        let first_slice_idx = entries
            .iter()
            .position(|e| matches!(e.kind, ZoneKind::Slice { .. }))
            .unwrap();
        let last_envelope_idx = entries
            .iter()
            .rposition(|e| {
                matches!(
                    e.kind,
                    ZoneKind::TenantEnvelope { .. }
                        | ZoneKind::OperatorEnvelope
                        | ZoneKind::OperatorEnvelopeBackup
                        | ZoneKind::TenantEnvelopeBackup { .. }
                )
            })
            .unwrap();
        assert!(last_envelope_idx < first_slice_idx);
        assert_eq!(entries.last().unwrap().kind, ZoneKind::SealMarker);
    }

    // ── permutation (sheet §2.1) ──

    #[test]
    fn tenant_envelope_permutation_is_deterministic_and_uuid_sensitive() {
        // Precomputed (Python hashlib, cross-checked against the algorithm
        // here): sha256(uuid_bytes || 0x00 || le64(tenant_id)) hex-sorted
        // over tenant_ids [1, 2] gives:
        //   uuid "1111...1111" -> [2, 1]  (flips the natural [1, 2] order)
        //   uuid "aaaa...eeee" -> [1, 2]  (natural order; different from the
        //                                  first uuid's [2, 1])
        let uuid_a = "11111111-1111-1111-1111-111111111111";
        let uuid_b = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";

        let envelope_tenant_order = |built: &BuiltLayout| -> Vec<i64> {
            built
                .layout
                .entries
                .iter()
                .filter_map(|e| match e.kind {
                    ZoneKind::TenantEnvelope { tenant_id } => Some(tenant_id),
                    _ => None,
                })
                .collect()
        };

        let (inputs_a1, _src_a1) = two_tenant_inputs(uuid_a);
        let session_a1 = tempfile::tempdir().unwrap();
        let order_a1 = envelope_tenant_order(&build(&inputs_a1, session_a1.path()).unwrap());

        let (inputs_a2, _src_a2) = two_tenant_inputs(uuid_a);
        let session_a2 = tempfile::tempdir().unwrap();
        let order_a2 = envelope_tenant_order(&build(&inputs_a2, session_a2.path()).unwrap());

        let (inputs_b, _src_b) = two_tenant_inputs(uuid_b);
        let session_b = tempfile::tempdir().unwrap();
        let order_b = envelope_tenant_order(&build(&inputs_b, session_b.path()).unwrap());

        assert_eq!(
            order_a1,
            vec![2, 1],
            "permutation must match the §2.1 hex-sort algorithm"
        );
        assert_ne!(
            order_a1,
            vec![1, 2],
            "must not be a no-op identity permutation for this fixture (proves permutation \
             actually happens, not just tenant_id order by coincidence)"
        );
        assert_eq!(
            order_a1, order_a2,
            "same volume_uuid must give the same order every build"
        );
        assert_eq!(order_b, vec![1, 2]);
        assert_ne!(
            order_a1, order_b,
            "a different volume_uuid must change the order"
        );
    }

    // ── build-twice byte-identity (narrowed — see comment) ──

    #[test]
    fn deterministic_zones_are_byte_identical_across_two_builds() {
        // system_guide and restore_sh are pure functions of (label,
        // total_files) with no clock read and no cryptographic randomness.
        // id_thunk is ALSO now covered (T6 review finding #5): its
        // `created_at` is injected via `BuildInputs.created_at` rather than
        // read from the clock inside the generator, and `two_tenant_inputs`
        // below builds one `inputs` value reused for both `build()` calls —
        // so with the same injected timestamp, the ID thunk is a pure
        // function of `inputs` too. This is what makes
        // layout-session.md's "same inputs + same generation timestamp ⇒
        // reproducible Layout" clause checkable for 3 of ~9 zone kinds
        // instead of 2 — the resume path depends on exactly this (frozen
        // zones must re-read byte-identical, never regenerate).
        //
        // Everything else materialized is NOT expected to match
        // byte-for-byte across separate builds, for reasons verified
        // empirically during T5b (see the report):
        //   - tenant/operator envelopes: `age::Encryptor` is randomized per
        //     call (fresh ephemeral key exchange) — encrypting IDENTICAL
        //     plaintext to the IDENTICAL recipient twice produces different
        //     ciphertext AND a different length (confirmed empirically);
        //     MANIFEST.toml's `created_at` also varies.
        //   - front_index / seal_marker: transitively non-deterministic,
        //     since their content embeds the envelopes' (non-deterministic)
        //     sizes and hashes.
        // This is narrower than plan T5b's literal wording ("build-twice
        // byte-identity for every materialized zone except the id thunk")
        // assumed — flagged to the PM in the T5b report. Finding #5 closes
        // the id_thunk half of that gap; the envelope/front-index/seal
        // non-determinism is inherent to age's randomized encryption, not a
        // missing injection point, and stands as-is.
        let (inputs, _src) = two_tenant_inputs("550e8400-e29b-41d4-a716-446655440000");

        let session1 = tempfile::tempdir().unwrap();
        let built1 = build(&inputs, session1.path()).unwrap();
        let session2 = tempfile::tempdir().unwrap();
        let built2 = build(&inputs, session2.path()).unwrap();

        for kind in [
            ZoneKind::IdThunk,
            ZoneKind::SystemGuide,
            ZoneKind::RestoreSh,
        ] {
            let e1 = built1
                .layout
                .entries
                .iter()
                .find(|e| e.kind == kind)
                .unwrap();
            let e2 = built2
                .layout
                .entries
                .iter()
                .find(|e| e.kind == kind)
                .unwrap();
            assert_eq!(
                e1.size_bytes, e2.size_bytes,
                "{kind:?} size differs across builds"
            );
            assert_eq!(e1.sha256, e2.sha256, "{kind:?} hash differs across builds");
            let (ContentSource::Materialized(p1), ContentSource::Materialized(p2)) =
                (&e1.source, &e2.source)
            else {
                panic!("expected Materialized sources for {kind:?}");
            };
            assert_eq!(
                std::fs::read(p1).unwrap(),
                std::fs::read(p2).unwrap(),
                "{kind:?} bytes differ across builds"
            );
        }
    }

    // ── front index / File 3 shape ──

    #[test]
    fn front_index_passes_validate_consistency() {
        let (inputs, _src) = two_tenant_inputs("550e8400-e29b-41d4-a716-446655440000");
        let session = tempfile::tempdir().unwrap();
        let built = build(&inputs, session.path()).unwrap();

        let fi_entry = built
            .layout
            .entries
            .iter()
            .find(|e| matches!(e.kind, ZoneKind::FrontIndex))
            .unwrap();
        let ContentSource::Materialized(path) = &fi_entry.source else {
            panic!("expected Materialized");
        };
        let raw = std::fs::read_to_string(path).unwrap();
        let parsed = format::parse_front_index(&raw).expect("front index parses");
        let violations = format::validate_consistency(&parsed);
        assert!(
            violations.is_empty(),
            "front index has consistency violations: {violations:?}"
        );

        // File 3's own entry carries neither size nor hash; same for the
        // seal marker's entry within File 3's own list.
        let fi_self = parsed
            .iter()
            .find(|p| p.type_label == "front_index")
            .unwrap();
        assert_eq!(fi_self.size_bytes, None);
        assert_eq!(fi_self.sha256_encrypted, None);
        let seal_self = parsed
            .iter()
            .find(|p| p.type_label == "seal_marker")
            .unwrap();
        assert_eq!(seal_self.size_bytes, None);
        assert_eq!(seal_self.sha256_encrypted, None);
    }

    #[test]
    fn front_index_layout_entry_carries_its_true_size_and_hash() {
        let (inputs, _src) = two_tenant_inputs("550e8400-e29b-41d4-a716-446655440000");
        let session = tempfile::tempdir().unwrap();
        let built = build(&inputs, session.path()).unwrap();

        let fi_entry = built
            .layout
            .entries
            .iter()
            .find(|e| matches!(e.kind, ZoneKind::FrontIndex))
            .unwrap();
        let ContentSource::Materialized(path) = &fi_entry.source else {
            panic!("expected Materialized");
        };
        let on_disk_bytes = std::fs::read(path).unwrap();

        // Unlike File 3's own [[files]] entry (checked above), the Layout
        // entry carries File 3's true on-tape size and hash — store.rs's
        // `chain_walk` needs this to trim File 3's padded tape bytes before
        // hashing (`volume-format-v2.md` §4 byte contract).
        assert_eq!(fi_entry.size_bytes, Some(on_disk_bytes.len() as u64));
        assert_eq!(fi_entry.sha256, Some(sha_hex(&on_disk_bytes)));
    }

    #[test]
    fn seal_marker_placeholder_and_real_timestamp_regeneration_match_length() {
        // §9 micro-decision: build() sizes the seal marker with a placeholder
        // timestamp, trusting a later real reseal (T6's `session::seal`) to
        // regenerate at an identical byte length. This USED to be unsound:
        // chrono's `to_rfc3339()` is `SecondsFormat::AutoSi`, which renders
        // 25/29/32/35 bytes depending on the timestamp's trailing-zero
        // fractional digits (observed over 2M samples: ~99.89% at 35 bytes,
        // ~0.11% at 32, ~0.0002% at 29 — see the T5b report), so a real
        // reseal would have failed its own length-identity check roughly
        // once per ~900 writes.
        //
        // Fixed in commit 5d4f27c: `generate_seal_marker` (layout.rs) now
        // renders `sealed_at` via `SecondsFormat::Secs` with `use_z = true`,
        // which is exactly 20 bytes ("2026-07-22T20:09:00Z") for every
        // possible instant — there is no trailing-fraction case left to
        // vary. This test's assertion is therefore no longer a probabilistic
        // canary that happens to pass almost always; it is a genuine
        // invariant check (any length difference here would mean the
        // generator regressed to a variable-width format or stopped
        // rendering UTC) — exactly what T6's `seal()` step depends on when
        // it asserts the real reseal's length matches this placeholder's.
        let files = vec![FrontIndexFile {
            position: 0,
            type_label: "id_thunk",
            size_bytes: Some(100),
            sha256_encrypted: Some("deadbeef".into()),
        }];
        let a = layout::generate_seal_marker("SEALTEST", 2, "front-hash", &files);
        let b = layout::generate_seal_marker("SEALTEST", 2, "front-hash", &files);
        assert_eq!(
            a.len(),
            b.len(),
            "two back-to-back generate_seal_marker calls produced different byte lengths — \
             the fixed-width sealed_at rendering (SecondsFormat::Secs) regressed; the \
             placeholder-sizing trick T6's seal() depends on requires this to always hold"
        );
    }

    // ── BuiltLayout::validate ──

    #[test]
    fn validate_passes_for_a_well_formed_build() {
        let (inputs, _src) = two_tenant_inputs("550e8400-e29b-41d4-a716-446655440000");
        let session = tempfile::tempdir().unwrap();
        let built = build(&inputs, session.path()).unwrap();
        assert!(built.validate(&ok_keys(&inputs), SliceCheck::Size).is_ok());
    }

    #[test]
    fn validate_catches_over_capacity() {
        let (mut inputs, _src) = two_tenant_inputs("550e8400-e29b-41d4-a716-446655440000");
        inputs.usable_bytes = 1; // absurdly small — everything overflows it
        let session = tempfile::tempdir().unwrap();
        let built = build(&inputs, session.path()).unwrap();
        let errs = built
            .validate(&ok_keys(&inputs), SliceCheck::Size)
            .unwrap_err();
        assert!(errs
            .iter()
            .any(|e| matches!(e, LayoutError::CapacityExceeded { .. })));
    }

    /// The capacity gate counts the tenant envelopes' backups
    /// (`volume-format-v2.md` §1.3): a budget that fits every file but them
    /// is refused, and one that fits them too is not. The gate is the
    /// Layout's own sum over its entries, so the backups count because
    /// they are entries; this pins that they are.
    #[test]
    fn the_capacity_gate_counts_the_tenant_envelope_backups() {
        let (mut inputs, _src) = two_tenant_inputs("550e8400-e29b-41d4-a716-446655440000");
        let probe = tempfile::tempdir().unwrap();
        let layout = build(&inputs, probe.path()).unwrap().layout;
        let bs = layout.block_size;
        let total = layout.on_tape_bytes().unwrap();
        let backups: u64 = layout
            .entries
            .iter()
            .filter(|e| matches!(e.kind, ZoneKind::TenantEnvelopeBackup { .. }))
            .map(|e| e.on_tape_bytes(bs).unwrap())
            .sum();
        assert!(backups > 0, "the fixture has tenant envelope backups");
        let reserve = layout.budget.reserve_bytes;

        inputs.usable_bytes = total - backups + reserve;
        let session = tempfile::tempdir().unwrap();
        let errs = build(&inputs, session.path())
            .unwrap()
            .validate(&ok_keys(&inputs), SliceCheck::Size)
            .unwrap_err();
        assert!(errs
            .iter()
            .any(|e| matches!(e, LayoutError::CapacityExceeded { .. })));

        inputs.usable_bytes = total + reserve;
        let session = tempfile::tempdir().unwrap();
        build(&inputs, session.path())
            .unwrap()
            .validate(&ok_keys(&inputs), SliceCheck::Size)
            .expect("a budget that fits the backups too is enough");
    }

    /// Hand-made index files for `two_tenant_inputs` (tenants 1 and 2),
    /// `paths_bytes` long each for a tenant's `paths.tsv`: the index as
    /// `build()` is handed it, whatever made it.
    fn index_files(dir: &Path, paths_bytes: usize) -> IndexFiles {
        use std::collections::BTreeMap;
        let write = |name: &str, bytes: Vec<u8>| {
            let path = dir.join(name);
            fs::write(&path, bytes).unwrap();
            path
        };
        let row = b"photos\t1\tf\t1234\talbum/some/where/a-reasonably-long-file-name.jpg\n";
        let mut tenants = BTreeMap::new();
        for id in [1i64, 2] {
            let mut paths = index::PATHS_COLUMNS.join("\t").into_bytes();
            paths.push(b'\n');
            while paths.len() < paths_bytes {
                paths.extend_from_slice(row);
            }
            tenants.insert(
                id,
                index::TenantIndexFiles {
                    units: write(
                        &format!("t{id}_units.tsv"),
                        b"unit\tversion\tvolume\n".to_vec(),
                    ),
                    paths: write(&format!("t{id}_paths.tsv"), paths),
                },
            );
        }
        IndexFiles {
            operator_units: write(
                "operator_units.tsv",
                b"tenant\tunit\tversion\tvolume\n".to_vec(),
            ),
            tenants,
            generated: index::Generated::default(),
        }
    }

    /// The capacity gate counts the cross-volume index (`volume-format-v2.md`
    /// §1.5, "Size"): it is in the envelopes before their size and hash are
    /// read, so a budget that fits the tape without it and not with it is
    /// refused, and one that fits the index too is not. The index adds to
    /// BOTH copies of an envelope, the backup being a byte copy.
    #[test]
    fn the_capacity_gate_counts_the_cross_volume_index() {
        let (mut inputs, _src) = two_tenant_inputs("550e8400-e29b-41d4-a716-446655440000");
        let without = tempfile::tempdir().unwrap();
        let before = build(&inputs, without.path()).unwrap().layout;
        let total_before = before.on_tape_bytes().unwrap();

        let work = tempfile::tempdir().unwrap();
        let paths_bytes = 3 * 512 * 1024;
        inputs.index = Some(index_files(work.path(), paths_bytes));
        let with = tempfile::tempdir().unwrap();
        let after = build(&inputs, with.path()).unwrap().layout;
        let total_after = after.on_tape_bytes().unwrap();

        // Every envelope, primary and backup, grew by at least the tenant's
        // paths.tsv (the operator's by its small units.tsv).
        for (b, a) in before.entries.iter().zip(&after.entries) {
            assert_eq!(b.kind, a.kind);
            let grew = a.size_bytes.unwrap() - b.size_bytes.unwrap().min(a.size_bytes.unwrap());
            match b.kind {
                ZoneKind::TenantEnvelope { .. } | ZoneKind::TenantEnvelopeBackup { .. } => {
                    assert!(grew >= paths_bytes as u64, "{:?} grew {grew}", b.kind)
                }
                ZoneKind::OperatorEnvelope | ZoneKind::OperatorEnvelopeBackup => {
                    assert!(grew > 0, "{:?} grew {grew}", b.kind)
                }
                _ => {}
            }
        }
        assert!(total_after >= total_before + 4 * paths_bytes as u64);

        let reserve = after.budget.reserve_bytes;
        inputs.usable_bytes = total_before + reserve;
        let session = tempfile::tempdir().unwrap();
        let errs = build(&inputs, session.path())
            .unwrap()
            .validate(&ok_keys(&inputs), SliceCheck::Size)
            .unwrap_err();
        assert!(
            errs.iter()
                .any(|e| matches!(e, LayoutError::CapacityExceeded { .. })),
            "a budget that fits the tape without its index must refuse it with: {errs:?}"
        );
        inputs.usable_bytes = total_after + reserve;
        let session = tempfile::tempdir().unwrap();
        build(&inputs, session.path())
            .unwrap()
            .validate(&ok_keys(&inputs), SliceCheck::Size)
            .expect("a budget that fits the index too is enough");
    }

    /// A build handed an index with no files for a tenant that has an
    /// envelope refuses, rather than writing a tape whose tenant index is
    /// silently not there. (The members' names and order are read back from
    /// decrypted envelopes in `tests/format_v2.rs`.)
    #[test]
    fn a_tenant_with_an_envelope_but_no_index_files_is_refused() {
        let (mut inputs, _src) = two_tenant_inputs("550e8400-e29b-41d4-a716-446655440000");
        let work = tempfile::tempdir().unwrap();
        let mut files = index_files(work.path(), 4096);
        inputs.index = Some(files.clone());
        let session = tempfile::tempdir().unwrap();
        build(&inputs, session.path()).expect("positive control: both tenants have files");

        files.tenants.remove(&2);
        inputs.index = Some(files);
        let session = tempfile::tempdir().unwrap();
        let err = build(&inputs, session.path()).unwrap_err().to_string();
        assert!(err.contains("no files for tenant 2"), "{err}");
    }

    // ── the plan bound (ADR-0012 item 113) ──

    /// Three units in the caller's order, each slice one block on tape:
    /// "unit-alpha" (slices 1 and 2), "unit-bravo" (slice 3), "unit-charlie"
    /// (slice 4).
    fn three_unit_inputs() -> (BuildInputs, tempfile::TempDir) {
        let src = tempfile::tempdir().unwrap();
        let (t1, a) = fake_tenant_and_unit(
            src.path(),
            1,
            "alpha",
            vec![(1, 1, b"alpha one"), (2, 2, b"alpha two")],
        );
        let (t2, b) = fake_tenant_and_unit(src.path(), 2, "bravo", vec![(3, 1, b"bravo one")]);
        let (t3, c) = fake_tenant_and_unit(src.path(), 3, "charlie", vec![(4, 1, b"charlie one")]);
        let inputs = base_inputs(
            "550e8400-e29b-41d4-a716-446655440000",
            vec![t1, t2, t3],
            vec![a, b, c],
        );
        (inputs, src)
    }

    fn slice_position(layout: &Layout, slice_id: i64) -> i32 {
        layout
            .entries
            .iter()
            .find(|e| {
                e.kind
                    == ZoneKind::Slice {
                        stage_slice_id: slice_id,
                    }
            })
            .unwrap()
            .position
    }

    /// (position, type, block-padded size) of every entry: what the bound must not
    /// move. Padded, because an envelope's exact size varies a little between two
    /// builds of the same inputs (a generated timestamp is not of fixed width).
    fn shape(layout: &Layout) -> Vec<(i32, &'static str, Option<u64>)> {
        layout
            .entries
            .iter()
            .map(|e| {
                (
                    e.position,
                    e.kind.type_label(),
                    e.on_tape_bytes(layout.block_size),
                )
            })
            .collect()
    }

    #[test]
    fn with_a_plan_bound_whole_units_are_planned_past_the_fill_ceiling_up_to_nominal() {
        let (mut inputs, _src) = three_unit_inputs();
        let probe = tempfile::tempdir().unwrap();
        let probe = build(&inputs, probe.path()).unwrap().layout;
        let fixed = probe.fixed_on_tape_bytes();
        let total = probe.on_tape_bytes().unwrap();
        assert_eq!(total, fixed + 4 * BS, "four one-block slices");
        // The fill budget holds the files that are not slices, alpha's two
        // slices and the reserve: bravo and charlie are past the ceiling.
        inputs.usable_bytes = fixed + 2 * BS + inputs.enospc_buffer;

        // Without a bound: refused, as before 1.6.0, and nothing is past the
        // ceiling.
        let unbounded_dir = tempfile::tempdir().unwrap();
        let unbounded = build(&inputs, unbounded_dir.path()).unwrap();
        let errs = unbounded
            .validate(&ok_keys(&inputs), SliceCheck::Size)
            .unwrap_err();
        assert!(
            errs.iter()
                .any(|e| matches!(e, LayoutError::CapacityExceeded { .. })),
            "{errs:?}"
        );
        let split = unbounded.split(&inputs.units);
        assert_eq!((split.committed, split.planned), (1, 1));
        assert!(unbounded.entries_past_ceiling(&inputs.units).is_empty());
        let sidecar = std::fs::read_to_string(unbounded_dir.path().join(LAYOUT_SIDECAR)).unwrap();
        assert!(!sidecar.contains("plan_bound_bytes"), "{sidecar}");

        // With a bound of exactly the plan's end: it validates, and the two
        // later units are planned past the ceiling, whole and in order.
        let session = tempfile::tempdir().unwrap();
        let built = build_with_plan_bound(&inputs, Some(total), session.path()).unwrap();
        built.validate(&ok_keys(&inputs), SliceCheck::Size).unwrap();
        let split = built.split(&inputs.units);
        assert_eq!(split.past_ceiling(), 1..3);
        assert!(split.fits());
        let names: Vec<&str> = inputs.units[split.past_ceiling()]
            .iter()
            .map(|u| u.unit_name.as_str())
            .collect();
        assert_eq!(names, ["unit-bravo", "unit-charlie"]);
        let past: Vec<i32> = built
            .entries_past_ceiling(&inputs.units)
            .iter()
            .map(|e| e.position)
            .collect();
        let alpha_last = slice_position(&built.layout, 2);
        assert_eq!(past, vec![alpha_last + 1, alpha_last + 2]);
        assert_eq!(slice_position(&built.layout, 3), alpha_last + 1);

        // They are ordinary plan entries: the bound moves no position or
        // size, and the front index lists every slice.
        assert_eq!(shape(&built.layout), shape(&unbounded.layout));
        let fi = built
            .layout
            .entries
            .iter()
            .find(|e| e.kind == ZoneKind::FrontIndex)
            .unwrap();
        let ContentSource::Materialized(fi_path) = &fi.source else {
            panic!("the front index is materialized");
        };
        let parsed =
            format::parse_front_index_document(&std::fs::read_to_string(fi_path).unwrap()).unwrap();
        for p in &past {
            assert!(
                parsed
                    .files
                    .iter()
                    .any(|f| f.position == *p && f.type_label == "data_slice"),
                "File 3 lists the slice at {p}"
            );
        }

        // The bound is frozen with the Layout, so a resume revalidates
        // against the bound the plan was made with.
        let frozen: Layout =
            serde_json::from_slice(&std::fs::read(session.path().join(LAYOUT_SIDECAR)).unwrap())
                .unwrap();
        assert_eq!(frozen.budget.plan_bound_bytes, Some(total));
        assert_eq!(frozen, built.layout);

        // One byte under the plan's end: refused by the bound, and charlie,
        // the unit that crosses it, is wholly beyond it.
        let session = tempfile::tempdir().unwrap();
        let built = build_with_plan_bound(&inputs, Some(total - 1), session.path()).unwrap();
        let errs = built
            .validate(&ok_keys(&inputs), SliceCheck::Size)
            .unwrap_err();
        assert_eq!(
            errs,
            vec![LayoutError::PlanBoundExceeded {
                needed: total,
                bound: total - 1
            }]
        );
        let split = built.split(&inputs.units);
        assert_eq!((split.committed, split.planned), (1, 2));
        assert_eq!(split.beyond_bound(), 2..3);
    }

    #[test]
    fn a_unit_that_crosses_the_fill_ceiling_is_wholly_past_it() {
        let (mut inputs, _src) = three_unit_inputs();
        let probe = tempfile::tempdir().unwrap();
        let probe = build(&inputs, probe.path()).unwrap().layout;
        let fixed = probe.fixed_on_tape_bytes();
        let total = probe.on_tape_bytes().unwrap();
        // Alpha's first slice fits under the ceiling and its second does not:
        // alpha is not committed in part. Nothing is committed, and every
        // unit is past the ceiling (an empty committed plan is what the
        // arithmetic gives; the files that are not slices still meet today's
        // gate).
        inputs.usable_bytes = fixed + BS + inputs.enospc_buffer;
        let session = tempfile::tempdir().unwrap();
        let built = build_with_plan_bound(&inputs, Some(total), session.path()).unwrap();
        built.validate(&ok_keys(&inputs), SliceCheck::Size).unwrap();
        let split = built.split(&inputs.units);
        assert_eq!(split.past_ceiling(), 0..3);
        let past: Vec<i32> = built
            .entries_past_ceiling(&inputs.units)
            .iter()
            .map(|e| e.position)
            .collect();
        let first = slice_position(&built.layout, 1);
        assert_eq!(past, vec![first, first + 1, first + 2, first + 3]);
    }

    #[test]
    fn validate_catches_a_missing_staged_slice() {
        let (inputs, src) = two_tenant_inputs("550e8400-e29b-41d4-a716-446655440000");
        let session = tempfile::tempdir().unwrap();
        let built = build(&inputs, session.path()).unwrap();

        // The source slice file vanishes between stage and build/validate
        // time.
        std::fs::remove_file(src.path().join("slice_1.age")).unwrap();

        let errs = built
            .validate(&ok_keys(&inputs), SliceCheck::Size)
            .unwrap_err();
        assert!(errs
            .iter()
            .any(|e| matches!(e, LayoutError::SliceFileMissing(_))));
    }

    #[test]
    fn validate_catches_a_corrupted_staged_slice() {
        let (inputs, src) = two_tenant_inputs("550e8400-e29b-41d4-a716-446655440000");
        let session = tempfile::tempdir().unwrap();
        let built = build(&inputs, session.path()).unwrap();

        // Flip a byte in the on-disk staged slice after build() recorded
        // its hash. Size is unchanged, so the default size-only L1 passes it
        // (L2 catches it while streaming, ADR-0012 2026-09-30 later); under
        // `--prewrite-hash` validate() full-hashes from disk and refuses.
        let path = src.path().join("slice_1.age");
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[0] ^= 0xFF;
        std::fs::write(&path, bytes).unwrap();

        assert!(built.validate(&ok_keys(&inputs), SliceCheck::Size).is_ok());
        let errs = built
            .validate(&ok_keys(&inputs), SliceCheck::FullHash)
            .unwrap_err();
        assert!(errs
            .iter()
            .any(|e| matches!(e, LayoutError::SliceChecksumMismatch { .. })));
    }

    #[test]
    fn validate_catches_missing_escrow() {
        let (inputs, _src) = two_tenant_inputs("550e8400-e29b-41d4-a716-446655440000");
        let session = tempfile::tempdir().unwrap();
        let built = build(&inputs, session.path()).unwrap();

        let mut keys = ok_keys(&inputs);
        keys.escrow_recipient_present = Some(false);
        let errs = built.validate(&keys, SliceCheck::Size).unwrap_err();
        assert!(errs.contains(&LayoutError::EscrowRecipientMissing));
    }

    #[test]
    fn validate_catches_a_hollow_front_index() {
        // Constructed through the public surface: rewrite the materialized
        // File 3 on disk with a deliberately hollow entry (a data slice
        // missing its size/hash), and update the Layout entry's OWN
        // recorded size+hash to match the new bytes — so the
        // materialized-zone size/hash check (layout_model.rs) passes, and
        // this isolates the thing that actually catches a *parseable but
        // hollow* map: `format::validate_consistency`, wired in through
        // `BuiltLayout::validate`.
        let (inputs, _src) = two_tenant_inputs("550e8400-e29b-41d4-a716-446655440000");
        let session = tempfile::tempdir().unwrap();
        let mut built = build(&inputs, session.path()).unwrap();

        let hollow_files = vec![
            FrontIndexFile {
                position: 0,
                type_label: "id_thunk",
                size_bytes: Some(10),
                sha256_encrypted: Some("a".into()),
            },
            FrontIndexFile {
                position: 1,
                type_label: "system_guide",
                size_bytes: Some(20),
                sha256_encrypted: Some("b".into()),
            },
            FrontIndexFile {
                position: 2,
                type_label: "restore_sh",
                size_bytes: Some(30),
                sha256_encrypted: Some("c".into()),
            },
            FrontIndexFile {
                position: 3,
                type_label: "front_index",
                size_bytes: None,
                sha256_encrypted: None,
            },
            FrontIndexFile {
                // Deliberately hollow: a content entry with neither size nor
                // hash — format §3 says every content file must carry both.
                position: 4,
                type_label: "data_slice",
                size_bytes: None,
                sha256_encrypted: None,
            },
            FrontIndexFile {
                position: 5,
                type_label: "seal_marker",
                size_bytes: None,
                sha256_encrypted: None,
            },
        ];
        let hollow_bytes = layout::generate_front_index(&inputs.label, &hollow_files);

        let fi_entry = built
            .layout
            .entries
            .iter_mut()
            .find(|e| matches!(e.kind, ZoneKind::FrontIndex))
            .unwrap();
        let ContentSource::Materialized(path) = &fi_entry.source else {
            panic!("expected Materialized");
        };
        std::fs::write(path, hollow_bytes.as_bytes()).unwrap();
        fi_entry.size_bytes = Some(hollow_bytes.len() as u64);
        fi_entry.sha256 = Some(sha_hex(hollow_bytes.as_bytes()));

        let errs = built
            .validate(&ok_keys(&inputs), SliceCheck::Size)
            .unwrap_err();
        assert!(
            errs.iter()
                .any(|e| matches!(e, LayoutError::GeneratedZoneInconsistent { .. })),
            "expected the hollow data_slice entry (missing size/hash) to trip \
             format::validate_consistency; got {errs:?}"
        );

        // Issue #357: the violations reach the operator in words, joined —
        // not as Rust's `{:?}` of the Vec (`[MissingSize { position: 4 }, ..]`).
        let message = errs
            .iter()
            .find_map(|e| match e {
                LayoutError::GeneratedZoneInconsistent { message, .. } => Some(message.as_str()),
                _ => None,
            })
            .unwrap();
        assert!(
            message.contains("position 4 carries no size_bytes")
                && message.contains("position 4 carries no sha256_encrypted"),
            "both hollow-entry violations, in words: {message}"
        );
        assert!(
            !message.contains('[') && !message.contains('{') && !message.contains("MissingSize"),
            "no Debug rendering of the violation list: {message}"
        );
    }

    // ── PLAN.toml wiring (T8, format §8) ──

    /// Decrypt the (single) materialized envelope entry matching `kind_matches`
    /// with `secret_key`, returning the decrypted tar bytes.
    fn decrypt_envelope_tar(
        built: &BuiltLayout,
        kind_matches: impl Fn(&ZoneKind) -> bool,
        secret_key: &str,
    ) -> Vec<u8> {
        let entry = built
            .layout
            .entries
            .iter()
            .find(|e| kind_matches(&e.kind))
            .expect("no matching envelope entry");
        let ContentSource::Materialized(path) = &entry.source else {
            panic!("expected Materialized");
        };
        let ciphertext = std::fs::read(path).unwrap();
        let identity: age::x25519::Identity = secret_key.parse().expect("valid age identity");
        let decryptor = age::Decryptor::new(ciphertext.as_slice()).expect("valid age ciphertext");
        let mut reader = decryptor
            .decrypt(std::iter::once(&identity as &dyn age::Identity))
            .expect("this identity decrypts the envelope");
        let mut tar_bytes = Vec::new();
        std::io::Read::read_to_end(&mut reader, &mut tar_bytes).unwrap();
        tar_bytes
    }

    fn tar_member_names(tar_bytes: Vec<u8>) -> Vec<String> {
        let mut archive = tar::Archive::new(std::io::Cursor::new(tar_bytes));
        archive
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn operator_envelope_contains_plan_toml_but_tenant_envelope_does_not() {
        // volume-format-v2.md §1/§8: PLAN.toml is an operator-only tar
        // member (the planning header folded in, not a standalone tape
        // file) — tenant envelopes must never carry it. This needs a
        // self-contained fixture (both key halves retained) rather than
        // `two_tenant_inputs`/`fake_tenant_and_unit` above, which only keep
        // the public half on `TenantInfo` — decryption needs the secret.
        let src = tempfile::tempdir().unwrap();
        let tenant_kp = crate::crypto::keys::generate_keypair();
        let op_kp = crate::crypto::keys::generate_keypair();
        let slice = fake_slice(src.path(), 1, 1, b"slice bytes for the plan.toml test");
        let unit = BuildUnit {
            stage_set_id: 1,
            snapshot_id: 1,
            unit_name: "unit-alpha".to_string(),
            unit_uuid: Uuid::new_v4().to_string(),
            tenant_id: 1,
            dar_version: Some("2.7.20".to_string()),
            dar_command: Some("dar -c base -R /src".to_string()),
            catalog_path: None,
            snapshot_version: 1,
            slices: vec![slice],
            span: None,
        };
        let inputs = BuildInputs {
            label: "PLANTEST".to_string(),
            volume_uuid: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            media_type: "LTO-6".to_string(),
            tapectl_version: "0.1.0-test".to_string(),
            created_at: "2026-07-22T20:09:00Z".to_string(),
            block_size: BS,
            usable_bytes: 100 * BS,
            enospc_buffer: BS,
            nominal_capacity: 2_400_000_000_000,
            mam_capacity: 0,
            mam_manufacturer: String::new(),
            mam_serial: String::new(),
            cartridge_identity_source: None,
            mam_length: 0,
            mam_loads: 0,
            units: vec![unit],
            tenants: vec![TenantInfo {
                tenant_id: 1,
                tenant_name: "alpha".to_string(),
                public_keys: vec![tenant_kp.public_key.clone()],
            }],
            operator_public_keys: vec![op_kp.public_key.clone()],
            escrow_public_key: None,
            catalog_db_path: None,
            index: None,
        };
        let session = tempfile::tempdir().unwrap();
        let built = build(&inputs, session.path()).unwrap();

        let tenant_tar = decrypt_envelope_tar(
            &built,
            |k| matches!(k, ZoneKind::TenantEnvelope { .. }),
            &tenant_kp.secret_key,
        );
        let tenant_members = tar_member_names(tenant_tar);
        assert!(tenant_members.contains(&"MANIFEST.toml".to_string()));
        assert!(
            !tenant_members.contains(&"PLAN.toml".to_string()),
            "tenant envelope must not carry PLAN.toml: {tenant_members:?}"
        );

        let op_tar = decrypt_envelope_tar(
            &built,
            |k| matches!(k, ZoneKind::OperatorEnvelope),
            &op_kp.secret_key,
        );
        let op_members = tar_member_names(op_tar);
        assert!(
            op_members.contains(&"PLAN.toml".to_string()),
            "operator envelope must carry PLAN.toml: {op_members:?}"
        );
    }

    /// Mirror of `operator_envelope_contains_plan_toml_but_tenant_envelope_does_not`
    /// for `catalog.db` (issue #83). THIS IS THE TEST THAT MATTERS for tenant
    /// isolation: the filtered catalog subset covers every unit on the
    /// volume, across all tenants, so it may only ever land in the operator
    /// envelope — if it ever reached a tenant envelope, tenant A would learn
    /// tenant B's unit names, file names, and sizes (a direct breach of the
    /// strict-tenant-isolation invariant `tests/tenant_isolation.rs` exists
    /// to defend).
    #[test]
    fn operator_envelope_contains_catalog_db_but_tenant_envelope_does_not() {
        let src = tempfile::tempdir().unwrap();
        let tenant_kp = crate::crypto::keys::generate_keypair();
        let op_kp = crate::crypto::keys::generate_keypair();
        let slice = fake_slice(src.path(), 1, 1, b"slice bytes for the catalog.db test");
        let unit = BuildUnit {
            stage_set_id: 1,
            snapshot_id: 1,
            unit_name: "unit-alpha".to_string(),
            unit_uuid: Uuid::new_v4().to_string(),
            tenant_id: 1,
            dar_version: Some("2.7.20".to_string()),
            dar_command: Some("dar -c base -R /src".to_string()),
            catalog_path: None,
            snapshot_version: 1,
            slices: vec![slice],
            span: None,
        };

        let catalog_db_dir = tempfile::tempdir().unwrap();
        let catalog_db_path = catalog_db_dir.path().join("catalog.db");
        std::fs::write(
            &catalog_db_path,
            b"fake sqlite bytes for the catalog.db isolation test",
        )
        .unwrap();

        let inputs = BuildInputs {
            label: "CATDBTEST".to_string(),
            volume_uuid: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            media_type: "LTO-6".to_string(),
            tapectl_version: "0.1.0-test".to_string(),
            created_at: "2026-07-22T20:09:00Z".to_string(),
            block_size: BS,
            usable_bytes: 100 * BS,
            enospc_buffer: BS,
            nominal_capacity: 2_400_000_000_000,
            mam_capacity: 0,
            mam_manufacturer: String::new(),
            mam_serial: String::new(),
            cartridge_identity_source: None,
            mam_length: 0,
            mam_loads: 0,
            units: vec![unit],
            tenants: vec![TenantInfo {
                tenant_id: 1,
                tenant_name: "alpha".to_string(),
                public_keys: vec![tenant_kp.public_key.clone()],
            }],
            operator_public_keys: vec![op_kp.public_key.clone()],
            escrow_public_key: None,
            catalog_db_path: Some(catalog_db_path),
            index: None,
        };
        let session = tempfile::tempdir().unwrap();
        let built = build(&inputs, session.path()).unwrap();

        let tenant_tar = decrypt_envelope_tar(
            &built,
            |k| matches!(k, ZoneKind::TenantEnvelope { .. }),
            &tenant_kp.secret_key,
        );
        let tenant_members = tar_member_names(tenant_tar);
        assert!(tenant_members.contains(&"MANIFEST.toml".to_string()));
        assert!(
            !tenant_members.contains(&"catalog.db".to_string()),
            "tenant envelope must not carry catalog.db: {tenant_members:?}"
        );

        let op_tar = decrypt_envelope_tar(
            &built,
            |k| matches!(k, ZoneKind::OperatorEnvelope),
            &op_kp.secret_key,
        );
        let op_members = tar_member_names(op_tar);
        assert!(
            op_members.contains(&"catalog.db".to_string()),
            "operator envelope must carry catalog.db: {op_members:?}"
        );
    }

    // ── a span's MANIFEST block (ADR-0012 item 125, #478) ──

    /// A spannable stage set of four slices (ids 11-14, numbers 1-4) of
    /// which this tape plans slices 2 and 3 (staged files for ids 12 and 13
    /// only); 1 and 4 are on other tapes and have no staging path.
    fn span_unit(dir: &Path) -> BuildUnit {
        let planned = vec![
            fake_slice(dir, 12, 2, b"span slice two"),
            fake_slice(dir, 13, 3, b"span slice three"),
        ];
        let elsewhere = |slice_id: i64, slice_number: i64| SpanSlice {
            slice_id,
            slice_number,
            size_bytes: 1000 + slice_number,
            encrypted_bytes: 2000 + slice_number,
            sha256_plain: format!("{slice_number:02}").repeat(32),
            sha256_encrypted: format!("e{slice_number}").repeat(32),
        };
        let span = UnitSpan {
            slices: vec![
                elsewhere(11, 1),
                SpanSlice::from(&planned[0]),
                SpanSlice::from(&planned[1]),
                elsewhere(14, 4),
            ],
        };
        BuildUnit {
            stage_set_id: 10,
            snapshot_id: 10,
            unit_name: "unit-span".to_string(),
            unit_uuid: "cccccccc-1111-4222-8333-444444444444".to_string(),
            tenant_id: 1,
            dar_version: Some("2.7.20".to_string()),
            dar_command: Some("dar -c base -R /src".to_string()),
            catalog_path: None,
            snapshot_version: 3,
            slices: planned,
            span: Some(span),
        }
    }

    /// ADR-0012 item 125, through `build()` and both envelopes: a span
    /// unit's MANIFEST block states `may_span = true` and lists all four
    /// slices of its set with their hashes, a `tape_position` on the two
    /// this tape plans (the positions the Layout gave them) and none on the
    /// others. The whole unit beside it keeps its 1.6.0 block, and the tape
    /// itself (the Layout, so File 3) holds only the planned slices.
    #[test]
    fn a_span_units_manifest_lists_every_slice_with_positions_only_for_the_planned_ones() {
        use crate::volume::manifest::Manifest;
        let src = tempfile::tempdir().unwrap();
        let kp = crate::crypto::keys::generate_keypair();
        let op = crate::crypto::keys::generate_keypair();
        let span = span_unit(src.path());
        let whole = BuildUnit {
            stage_set_id: 20,
            snapshot_id: 20,
            unit_name: "unit-whole".to_string(),
            unit_uuid: "dddddddd-1111-4222-8333-444444444444".to_string(),
            snapshot_version: 1,
            slices: vec![fake_slice(src.path(), 21, 1, b"whole slice one")],
            span: None,
            ..span.clone()
        };
        let inputs = BuildInputs {
            operator_public_keys: vec![op.public_key.clone()],
            ..base_inputs(
                "550e8400-e29b-41d4-a716-4466554400b1",
                vec![TenantInfo {
                    tenant_id: 1,
                    tenant_name: "alpha".to_string(),
                    public_keys: vec![kp.public_key.clone()],
                }],
                vec![span.clone(), whole],
            )
        };
        let session = tempfile::tempdir().unwrap();
        let built = build(&inputs, session.path()).unwrap();

        let on_tape: Vec<i64> = built
            .layout
            .entries
            .iter()
            .filter_map(|e| match e.kind {
                ZoneKind::Slice { stage_slice_id } => Some(stage_slice_id),
                _ => None,
            })
            .collect();
        assert_eq!(
            on_tape,
            vec![12, 13, 21],
            "only the planned slices are on the tape"
        );
        let p2 = i64::from(slice_position(&built.layout, 12));
        let p3 = i64::from(slice_position(&built.layout, 13));
        let p_whole = i64::from(slice_position(&built.layout, 21));

        for (what, kind_is, key) in [
            (
                "tenant",
                (|k: &ZoneKind| matches!(k, ZoneKind::TenantEnvelope { .. }))
                    as fn(&ZoneKind) -> bool,
                &kp.secret_key,
            ),
            (
                "operator",
                (|k: &ZoneKind| matches!(k, ZoneKind::OperatorEnvelope)) as fn(&ZoneKind) -> bool,
                &op.secret_key,
            ),
        ] {
            let tar_bytes = decrypt_envelope_tar(&built, kind_is, key);
            let mut archive = tar::Archive::new(std::io::Cursor::new(tar_bytes));
            let mut text = String::new();
            for entry in archive.entries().unwrap() {
                let mut entry = entry.unwrap();
                if entry.path().unwrap().to_string_lossy() == "MANIFEST.toml" {
                    std::io::Read::read_to_string(&mut entry, &mut text).unwrap();
                }
            }
            let manifest = Manifest::from_toml(&text).unwrap();
            assert_eq!(manifest.units.len(), 2, "{what}: {text}");

            let range = &manifest.units[0];
            assert_eq!(range.name, "unit-span", "{what}");
            assert!(range.may_span, "{what}: {text}");
            assert_eq!(
                range
                    .slices
                    .iter()
                    .map(|s| (s.number, s.tape_position))
                    .collect::<Vec<_>>(),
                vec![(1, None), (2, Some(p2)), (3, Some(p3)), (4, None)],
                "{what}: every slice of the set, a position only where this tape plans one"
            );
            let span_slices = &span.span.as_ref().unwrap().slices;
            for (listed, fact) in range.slices.iter().zip(span_slices) {
                assert_eq!(listed.size_bytes, fact.size_bytes, "{what}");
                assert_eq!(listed.encrypted_bytes, fact.encrypted_bytes, "{what}");
                assert_eq!(listed.sha256_plain, fact.sha256_plain, "{what}");
                assert_eq!(listed.sha256_encrypted, fact.sha256_encrypted, "{what}");
            }

            let whole = &manifest.units[1];
            assert!(!whole.may_span, "{what}");
            assert_eq!(whole.slices.len(), 1, "{what}");
            assert_eq!(whole.slices[0].tape_position, Some(p_whole), "{what}");

            assert_eq!(
                text.matches("\nmay_span = true\n").count(),
                1,
                "{what}: {text}"
            );
            assert_eq!(
                text.matches("\ntape_position = ").count(),
                3,
                "{what}: {text}"
            );
        }
    }

    /// `build()` refuses a span that leaves out a slice this tape plans,
    /// states one differently, or is out of slice order, before it writes
    /// anything: the MANIFEST would otherwise omit a slice the tape holds or
    /// state hashes its bytes do not have.
    #[test]
    fn a_span_that_misstates_a_planned_slice_is_refused() {
        let src = tempfile::tempdir().unwrap();
        let kp = crate::crypto::keys::generate_keypair();
        let tenant = TenantInfo {
            tenant_id: 1,
            tenant_name: "alpha".to_string(),
            public_keys: vec![kp.public_key],
        };
        type Change<'a> = &'a dyn Fn(&mut Vec<SpanSlice>);
        let mutated = |change: Change| {
            let mut unit = span_unit(src.path());
            change(&mut unit.span.as_mut().unwrap().slices);
            base_inputs(
                "550e8400-e29b-41d4-a716-4466554400b2",
                vec![tenant.clone()],
                vec![unit],
            )
        };
        let cases: [(&str, Change); 3] = [
            ("does not list slice 3 (id 13)", &|s| {
                s.remove(2);
            }),
            ("states slice 2 (id 12) differently", &|s| {
                s[1].sha256_encrypted = "00".repeat(32);
            }),
            ("not in slice-number order", &|s| s.swap(0, 3)),
        ];
        for (why, change) in cases {
            let session = tempfile::tempdir().unwrap();
            let dir = session.path().join("never");
            let err = build(&mutated(change), &dir).unwrap_err().to_string();
            assert!(err.contains("unit unit-span (stage set 10)"), "{err}");
            assert!(err.contains(why), "{why}: {err}");
            assert!(!dir.exists(), "{why}: nothing is written");
        }
        // The fixture itself builds.
        let session = tempfile::tempdir().unwrap();
        build(&mutated(&|_| {}), session.path()).unwrap();
    }

    // ── File 0 and the seal placeholder per tape (ADR-0012 item 124) ──

    /// A materialized zone's text.
    fn zone_text(layout: &Layout, want: fn(&ZoneKind) -> bool) -> String {
        let e = layout.entries.iter().find(|e| want(&e.kind)).unwrap();
        let ContentSource::Materialized(path) = &e.source else {
            panic!("{:?} is materialized", e.kind);
        };
        std::fs::read_to_string(path).unwrap()
    }

    /// File 3's hash and the seal's embedded copy of a built Layout, as
    /// `build` assembles them: every entry with its size and hash, File 3's
    /// own included, and the seal's own entry bare.
    fn seal_copy(layout: &Layout) -> (String, Vec<FrontIndexFile>) {
        let fi_hash = layout
            .entries
            .iter()
            .find(|e| e.kind == ZoneKind::FrontIndex)
            .and_then(|e| e.sha256.clone())
            .unwrap();
        let files = layout
            .entries
            .iter()
            .map(|e| {
                let bare = e.kind == ZoneKind::SealMarker;
                FrontIndexFile {
                    position: e.position,
                    type_label: e.kind.type_label(),
                    size_bytes: if bare { None } else { e.size_bytes },
                    sha256_encrypted: if bare { None } else { e.sha256.clone() },
                }
            })
            .collect();
        (fi_hash, files)
    }

    /// Every line of a seal but its `sealed_at`, which is the clock's.
    fn without_sealed_at(seal: &str) -> Vec<&str> {
        seal.lines()
            .filter(|l| !l.starts_with("sealed_at = "))
            .collect()
    }

    /// The brief's byte-identity case: a tape with no unit that may span
    /// builds File 0 and the seal placeholder exactly as 1.6.0's generators
    /// make them (`requires = ["short-seal"]`), so a whole-unit tape's bytes
    /// do not move in 1.7.0.
    #[test]
    fn a_tape_with_no_span_builds_file_0_and_the_placeholder_as_1_6_0_did() {
        let (inputs, _src) = three_unit_inputs();
        assert!(inputs.units.iter().all(|u| u.span.is_none()));
        let session = tempfile::tempdir().unwrap();
        let layout = build(&inputs, session.path()).unwrap().layout;
        let total_files = layout.entries.len() as i32;

        let file0 = zone_text(&layout, |k| matches!(k, ZoneKind::IdThunk));
        let want = layout::generate_id_thunk_v2(&IdThunkV2Params {
            label: &inputs.label,
            uuid: &inputs.volume_uuid,
            media_type: &inputs.media_type,
            tapectl_version: &inputs.tapectl_version,
            nominal_capacity: inputs.nominal_capacity,
            mam_capacity: inputs.mam_capacity,
            total_files,
            mam_manufacturer: &inputs.mam_manufacturer,
            mam_serial: &inputs.mam_serial,
            mam_length: inputs.mam_length,
            mam_loads: inputs.mam_loads,
            created_at: &inputs.created_at,
            cartridge_identity_source: inputs.cartridge_identity_source.as_deref(),
        });
        assert_eq!(file0, want, "File 0 is 1.6.0's, byte for byte");
        assert_eq!(file0.matches("\nrequires = [\"short-seal\"]\n").count(), 1);

        let placeholder = zone_text(&layout, |k| matches!(k, ZoneKind::SealMarker));
        let (fi_hash, copy) = seal_copy(&layout);
        let want = layout::generate_seal_marker(&inputs.label, total_files, &fi_hash, &copy);
        assert_eq!(placeholder.len(), want.len());
        assert_eq!(without_sealed_at(&placeholder), without_sealed_at(&want));
        assert_eq!(
            placeholder
                .matches("\nrequires = [\"short-seal\"]\n")
                .count(),
            1
        );
    }

    /// `layout-session.md` "Spanning" and `volume-format-v2.md` §4.1
    /// "Length", through `build`: a span tape's placeholder states both
    /// features, is exactly the length of the full seal that states them, and
    /// is at least as long as every short seal that states them.
    #[test]
    fn a_span_tapes_placeholder_holds_every_short_seal_that_states_both_features() {
        let src = tempfile::tempdir().unwrap();
        let kp = crate::crypto::keys::generate_keypair();
        let span = span_unit(src.path());
        let whole = BuildUnit {
            stage_set_id: 20,
            snapshot_id: 20,
            unit_name: "unit-whole".to_string(),
            unit_uuid: "dddddddd-1111-4222-8333-444444444444".to_string(),
            snapshot_version: 1,
            slices: vec![fake_slice(src.path(), 21, 1, b"whole slice one")],
            span: None,
            ..span.clone()
        };
        let inputs = base_inputs(
            "550e8400-e29b-41d4-a716-4466554400b3",
            vec![TenantInfo {
                tenant_id: 1,
                tenant_name: "alpha".to_string(),
                public_keys: vec![kp.public_key],
            }],
            vec![span, whole],
        );
        let session = tempfile::tempdir().unwrap();
        let layout = build(&inputs, session.path()).unwrap().layout;
        let total_files = layout.entries.len() as i32;
        let both = "\nrequires = [\"short-seal\", \"spanning\"]\n";
        let placeholder = zone_text(&layout, |k| matches!(k, ZoneKind::SealMarker));
        assert_eq!(placeholder.matches(both).count(), 1, "{placeholder}");
        assert_eq!(
            zone_text(&layout, |k| matches!(k, ZoneKind::IdThunk))
                .matches(both)
                .count(),
            1
        );

        let (fi_hash, copy) = seal_copy(&layout);
        let full = layout::generate_seal_marker_requiring(
            &inputs.label,
            total_files,
            &fi_hash,
            &copy,
            layout::REQUIRES_SPAN,
        );
        assert_eq!(full.len(), placeholder.len());
        assert_eq!(without_sealed_at(&full), without_sealed_at(&placeholder));
        let slices: Vec<i32> = layout
            .entries
            .iter()
            .filter(|e| matches!(e.kind, ZoneKind::Slice { .. }))
            .map(|e| e.position)
            .collect();
        let seal_pos = total_files - 1;
        let shorts: Vec<i32> = slices.into_iter().filter(|&k| k <= seal_pos - 2).collect();
        assert_eq!(shorts.len(), 2, "two places a short seal can come");
        for k in shorts {
            let short = layout::generate_short_seal_marker_requiring(
                &inputs.label,
                k,
                &fi_hash,
                &copy[..=k as usize],
                layout::REQUIRES_SPAN,
            );
            assert_eq!(short.matches(both).count(), 1, "K={k}");
            assert!(short.len() <= placeholder.len(), "K={k}");
            format::parse_seal_marker(&short)
                .unwrap()
                .check_parse_rules()
                .unwrap();
        }
    }

    /// ADR-0012 item 123 through `build`: the last unit, when it may span, is
    /// frozen as the span's tail, and only it may pass the plan bound. Built
    /// in the plan order (the whole unit, then the span), a bound that the
    /// tail crosses validates; with the span first nothing is frozen and the
    /// same bound refuses the plan. Neither sidecar names a tail it lacks.
    #[test]
    fn build_freezes_the_last_units_span_as_the_tail_the_gate_lets_pass() {
        let src = tempfile::tempdir().unwrap();
        let kp = crate::crypto::keys::generate_keypair();
        let span = span_unit(src.path());
        let whole = BuildUnit {
            stage_set_id: 20,
            snapshot_id: 20,
            unit_name: "unit-whole".to_string(),
            unit_uuid: "dddddddd-1111-4222-8333-444444444444".to_string(),
            snapshot_version: 1,
            slices: vec![fake_slice(src.path(), 21, 1, b"whole slice one")],
            span: None,
            ..span.clone()
        };
        let inputs = |units: Vec<BuildUnit>| {
            base_inputs(
                "550e8400-e29b-41d4-a716-4466554400b5",
                vec![TenantInfo {
                    tenant_id: 1,
                    tenant_name: "alpha".to_string(),
                    public_keys: vec![kp.public_key.clone()],
                }],
                units,
            )
        };
        let in_order = inputs(vec![whole.clone(), span.clone()]);
        let probe = tempfile::tempdir().unwrap();
        let probe = build(&in_order, probe.path()).unwrap().layout;
        let total = probe.on_tape_bytes().unwrap();
        let first = slice_position(&probe, 12);
        assert_eq!(probe.budget.span_tail_from, Some(first));
        // Under a fill budget of the fixed files, the whole unit and the
        // reserve, a bound one block short of the plan's end: the span's
        // last slice passes it.
        let fixed = probe.fixed_on_tape_bytes();
        let mut in_order = in_order;
        in_order.usable_bytes = fixed + BS + in_order.enospc_buffer;
        let bound = total - BS;

        let session = tempfile::tempdir().unwrap();
        let built = build_with_plan_bound(&in_order, Some(bound), session.path()).unwrap();
        assert_eq!(built.layout.budget.span_tail_from, Some(first));
        built
            .validate(&ok_keys(&in_order), SliceCheck::Size)
            .unwrap();
        let sidecar = std::fs::read_to_string(session.path().join(LAYOUT_SIDECAR)).unwrap();
        assert!(
            sidecar.contains(&format!("\"span_tail_from\": {first}")),
            "{sidecar}"
        );
        let frozen: Layout = serde_json::from_str(&sidecar).unwrap();
        assert_eq!(frozen, built.layout, "a resume revalidates the same tail");

        // The span first: no tail is frozen, and the bound refuses the plan.
        let mut span_first = inputs(vec![span, whole]);
        span_first.usable_bytes = in_order.usable_bytes;
        let session = tempfile::tempdir().unwrap();
        let built = build_with_plan_bound(&span_first, Some(bound), session.path()).unwrap();
        assert_eq!(built.layout.budget.span_tail_from, None);
        let errs = built
            .validate(&ok_keys(&span_first), SliceCheck::Size)
            .unwrap_err();
        assert!(
            errs.iter()
                .any(|e| matches!(e, LayoutError::PlanBoundExceeded { .. })),
            "{errs:?}"
        );
        let sidecar = std::fs::read_to_string(session.path().join(LAYOUT_SIDECAR)).unwrap();
        assert!(!sidecar.contains("span_tail_from"), "{sidecar}");
    }

    /// [`BuiltLayout::place`] reads a built plan back in the plan order,
    /// with the classes the caller gives: here the caller built the span
    /// first, so the Layout's order is not the plan's.
    #[test]
    fn a_built_layouts_units_are_placed_by_the_callers_classes() {
        let src = tempfile::tempdir().unwrap();
        let kp = crate::crypto::keys::generate_keypair();
        let span = span_unit(src.path());
        let whole = BuildUnit {
            stage_set_id: 20,
            snapshot_id: 20,
            unit_name: "unit-whole".to_string(),
            unit_uuid: "dddddddd-1111-4222-8333-444444444444".to_string(),
            snapshot_version: 1,
            slices: vec![fake_slice(src.path(), 21, 1, b"whole slice one")],
            span: None,
            ..span.clone()
        };
        let inputs = base_inputs(
            "550e8400-e29b-41d4-a716-4466554400b4",
            vec![TenantInfo {
                tenant_id: 1,
                tenant_name: "alpha".to_string(),
                public_keys: vec![kp.public_key],
            }],
            vec![span, whole],
        );
        let session = tempfile::tempdir().unwrap();
        let built = build(&inputs, session.path()).unwrap();
        assert!(slice_position(&built.layout, 12) < slice_position(&built.layout, 21));
        for (span_is, order) in [
            (PlanClass::Spannable, vec![1, 0]),
            (PlanClass::Owed, vec![0, 1]),
        ] {
            let p = built.place(&inputs.units, &[span_is, PlanClass::Whole]);
            assert!(p.is_ok(), "{span_is:?}: {:?}", p.errors);
            assert_eq!(p.order, order, "{span_is:?}");
        }
    }

    /// #449 comment 2: RECOVERY.md names a unit's dar catalogue only when the
    /// envelope holds it. Built for real: a tenant with two units, one staged
    /// with a catalogue (files beside its `catalog_path` stem, as `staging`
    /// writes them) and one without. The tenant envelope's RECOVERY.md has the
    /// `dar -l` lines for the first and none for the second; the operator
    /// envelope's likewise; and every `catalogs/` member the text names is a
    /// member of the tar.
    #[test]
    fn recovery_md_names_only_the_catalogues_the_envelope_holds() {
        let src = tempfile::tempdir().unwrap();
        let kp = crate::crypto::keys::generate_keypair();
        let op = crate::crypto::keys::generate_keypair();
        let uuid_with = "aaaaaaaa-1111-4222-8333-444444444444";
        let uuid_without = "bbbbbbbb-1111-4222-8333-444444444444";
        let stem = src.path().join("aaaaaaaa_v1");
        std::fs::write(format!("{}.1.dar", stem.display()), b"catalogue bytes").unwrap();

        let unit = |id: i64, name: &str, uuid: &str, catalog_path: Option<String>| BuildUnit {
            stage_set_id: id,
            snapshot_id: id,
            unit_name: name.to_string(),
            unit_uuid: uuid.to_string(),
            tenant_id: 1,
            dar_version: None,
            dar_command: None,
            catalog_path,
            snapshot_version: 1,
            slices: vec![fake_slice(
                src.path(),
                id,
                1,
                format!("slice {id}").as_bytes(),
            )],
            span: None,
        };
        let units = vec![
            unit(
                1,
                "with-catalogue",
                uuid_with,
                Some(stem.to_string_lossy().into_owned()),
            ),
            unit(2, "no-catalogue", uuid_without, None),
        ];
        let inputs = BuildInputs {
            operator_public_keys: vec![op.public_key.clone()],
            ..base_inputs(
                "550e8400-e29b-41d4-a716-4466554400a9",
                vec![TenantInfo {
                    tenant_id: 1,
                    tenant_name: "alpha".to_string(),
                    public_keys: vec![kp.public_key.clone()],
                }],
                units,
            )
        };
        let session = tempfile::tempdir().unwrap();
        let built = build(&inputs, session.path()).unwrap();

        for (what, kind_is, key) in [
            (
                "tenant",
                (|k: &ZoneKind| matches!(k, ZoneKind::TenantEnvelope { .. }))
                    as fn(&ZoneKind) -> bool,
                &kp.secret_key,
            ),
            (
                "operator",
                (|k: &ZoneKind| matches!(k, ZoneKind::OperatorEnvelope)) as fn(&ZoneKind) -> bool,
                &op.secret_key,
            ),
        ] {
            let tar_bytes = decrypt_envelope_tar(&built, kind_is, key);
            let mut archive = tar::Archive::new(std::io::Cursor::new(tar_bytes));
            let mut members = Vec::new();
            let mut recovery = String::new();
            for entry in archive.entries().unwrap() {
                let mut entry = entry.unwrap();
                let name = entry.path().unwrap().to_string_lossy().into_owned();
                if name == "RECOVERY.md" {
                    std::io::Read::read_to_string(&mut entry, &mut recovery).unwrap();
                }
                members.push(name);
            }
            assert!(
                members.contains(&"catalogs/aaaaaaaa_v1.1.dar".to_string()),
                "{what}: {members:?}"
            );
            let sections: Vec<&str> = recovery.split("\n## ").collect();
            let with = sections
                .iter()
                .find(|s| s.starts_with("with-catalogue"))
                .unwrap();
            let without = sections
                .iter()
                .find(|s| s.starts_with("no-catalogue"))
                .unwrap();
            assert!(
                with.contains("dar -l catalogs/aaaaaaaa_v1 -N -Q"),
                "{what}:\n{with}"
            );
            assert!(!without.contains("dar -l"), "{what}:\n{without}");
            assert!(!without.contains("catalogs/"), "{what}:\n{without}");
            // Every catalogue the text names is in the tar.
            for line in recovery.lines().filter(|l| l.contains("`catalogs/")) {
                let named = line
                    .split('`')
                    .find(|p| p.starts_with("catalogs/"))
                    .unwrap();
                assert!(members.contains(&named.to_string()), "{what}: {named}");
            }
        }
    }

    // ── streaming envelope construction (issue #87) ──

    /// A catalogue substantially larger than `staging::STREAM_COPY_BUFFER`
    /// (128 KiB) round-trips through the streaming envelope path byte-for-
    /// byte, proving `materialize_envelope_streaming` doesn't truncate,
    /// corrupt, or otherwise mishandle a file-backed tar member that spans
    /// many copy-buffer refills. Deliberately non-uniform bytes (not all
    /// zero) so a bug that only zeroes/duplicates a chunk would show up as
    /// a mismatch rather than passing by accident.
    #[test]
    fn large_catalog_file_round_trips_through_streaming_envelope() {
        let src = tempfile::tempdir().unwrap();
        let tenant_kp = crate::crypto::keys::generate_keypair();
        let op_kp = crate::crypto::keys::generate_keypair();

        // ~1 MiB of non-uniform bytes, well over STREAM_COPY_BUFFER's 128 KiB.
        let big_catalog_bytes: Vec<u8> = (0..(1024 * 1024 + 12345))
            .map(|i| (i % 251) as u8)
            .collect();
        let catalog_stem = src.path().join("catalog_base");
        std::fs::write(
            format!("{}.1.dar", catalog_stem.display()),
            &big_catalog_bytes,
        )
        .unwrap();

        let slice = fake_slice(src.path(), 1, 1, b"slice bytes for the large-catalog test");
        let unit = BuildUnit {
            stage_set_id: 1,
            snapshot_id: 1,
            unit_name: "unit-alpha".to_string(),
            unit_uuid: Uuid::new_v4().to_string(),
            tenant_id: 1,
            dar_version: Some("2.7.20".to_string()),
            dar_command: Some("dar -c base -R /src".to_string()),
            catalog_path: Some(catalog_stem.to_string_lossy().into_owned()),
            snapshot_version: 1,
            slices: vec![slice],
            span: None,
        };
        let inputs = base_inputs(
            "550e8400-e29b-41d4-a716-446655440001",
            vec![TenantInfo {
                tenant_id: 1,
                tenant_name: "alpha".to_string(),
                public_keys: vec![tenant_kp.public_key.clone()],
            }],
            vec![unit],
        );
        // base_inputs generates its own operator keypair; override with ours
        // so the test can decrypt the operator envelope below.
        let inputs = BuildInputs {
            operator_public_keys: vec![op_kp.public_key.clone()],
            ..inputs
        };

        let session = tempfile::tempdir().unwrap();
        let built = build(&inputs, session.path()).unwrap();

        // Ciphertext (not plaintext) hash is what's recorded on the entry —
        // confirm it matches a fresh hash of the materialized file's bytes,
        // which is exactly what `Layout::validate`'s full-hash re-check does
        // against a real staging directory.
        let tenant_entry = built
            .layout
            .entries
            .iter()
            .find(|e| matches!(e.kind, ZoneKind::TenantEnvelope { .. }))
            .unwrap();
        let ContentSource::Materialized(tenant_path) = &tenant_entry.source else {
            panic!("expected Materialized");
        };
        let on_disk = std::fs::read(tenant_path).unwrap();
        assert_eq!(
            tenant_entry.sha256.as_deref(),
            Some(sha_hex(&on_disk).as_str())
        );
        assert_eq!(tenant_entry.size_bytes, Some(on_disk.len() as u64));

        let tenant_tar = decrypt_envelope_tar(
            &built,
            |k| matches!(k, ZoneKind::TenantEnvelope { .. }),
            &tenant_kp.secret_key,
        );
        let mut archive = tar::Archive::new(std::io::Cursor::new(tenant_tar));
        let mut seen = std::collections::HashMap::new();
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let name = entry.path().unwrap().to_string_lossy().into_owned();
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut bytes).unwrap();
            seen.insert(name, bytes);
        }
        assert!(seen.contains_key("MANIFEST.toml"));
        assert!(seen.contains_key("RECOVERY.md"));
        assert!(!seen.contains_key("PLAN.toml"));
        let recovered = seen
            .get("catalogs/catalog_base.1.dar")
            .expect("large catalog member present in tenant envelope tar");
        assert_eq!(
            recovered, &big_catalog_bytes,
            "large catalog file must round-trip byte-for-byte through the streaming envelope path"
        );

        // ── the invariant #87 most endangered ──
        //
        // `OperatorEnvelopeBackup` must be the SAME ciphertext as
        // `OperatorEnvelope`, not a second independent encryption. Under the
        // old buffered path this was free (both entries materialized the one
        // `op_encrypted` Vec). Streaming makes "just call the helper again"
        // the obvious-looking implementation, and it is wrong: `age::Encryptor`
        // draws a fresh ephemeral key per call and the tar layer stamps
        // `set_mtime(now)`, so a second pass yields unrelated bytes that merely
        // happen to decrypt to the same plaintext — a redundant copy that
        // shares no ciphertext with the thing it is meant to back up. Nothing
        // pinned this before; asserting on the raw bytes means a future
        // regression fails here instead of on a tape years from now.
        let op_path = match &built
            .layout
            .entries
            .iter()
            .find(|e| matches!(e.kind, ZoneKind::OperatorEnvelope))
            .unwrap()
            .source
        {
            ContentSource::Materialized(p) => p.clone(),
            _ => panic!("expected Materialized"),
        };
        let backup_entry = built
            .layout
            .entries
            .iter()
            .find(|e| matches!(e.kind, ZoneKind::OperatorEnvelopeBackup))
            .unwrap();
        let ContentSource::Materialized(backup_path) = &backup_entry.source else {
            panic!("expected Materialized");
        };
        let op_bytes = std::fs::read(&op_path).unwrap();
        let backup_bytes = std::fs::read(backup_path).unwrap();
        assert_eq!(
            op_bytes, backup_bytes,
            "operator envelope backup must be a byte-for-byte copy of the primary, \
             not a second independent encryption (issue #87)"
        );
        assert_eq!(
            backup_entry.sha256.as_deref(),
            Some(sha_hex(&op_bytes).as_str()),
            "backup entry must record the primary's ciphertext hash"
        );
        assert_eq!(backup_entry.size_bytes, Some(op_bytes.len() as u64));
    }

    /// The heir's side of `catalog.db` (#83; ADR-0012 2026-10-12 item 48):
    /// every `sqlite3 catalog.db …` line `RECOVERY.md` shows runs, as
    /// written, against what `db::ontape_catalog::write` makes, and says
    /// what the section claims — the entry counts, then one unit's entries
    /// with each kind by name, the hash in hex, the mtime as a date and a
    /// symlink's target. Run through rusqlite's bundled SQLite, the engine
    /// the `sqlite3` shell wraps.
    #[test]
    fn recovery_md_catalog_db_queries_run_on_what_write_makes() {
        use crate::db::files::{fixture, FileEntry, FileKind};
        let conn = crate::db::open_memory().unwrap();
        conn.execute_batch(
            "INSERT INTO tenants (id, name, is_operator, status) VALUES (1, 'alice', 0, 'active');
             INSERT INTO units (id, uuid, name, tenant_id, checksum_mode, encrypt, status)
                  VALUES (1, 'u-1', 'photos', 1, 'mtime_size', 1, 'active');
             INSERT INTO snapshots (id, unit_id, version, status, source_path, file_count,
                                    total_size)
                  VALUES (1, 1, 3, 'staged', '/src/photos', 3, 5);
             INSERT INTO stage_sets (id, snapshot_id, status, slice_size, num_slices)
                  VALUES (1, 1, 'staged', 1000, 1);",
        )
        .unwrap();
        let at = Some(1_788_264_000_000_000_000);
        for e in [
            FileEntry {
                path: "2019".into(),
                kind: FileKind::Dir,
                size_bytes: 0,
                mtime_ns: at,
                sha256: None,
                link_target: None,
            },
            FileEntry {
                path: "2019/a.jpg".into(),
                kind: FileKind::Regular,
                size_bytes: 5,
                mtime_ns: at,
                sha256: Some([0xAB; 32]),
                link_target: None,
            },
            FileEntry {
                path: "2019/latest".into(),
                kind: FileKind::Symlink,
                size_bytes: 5,
                mtime_ns: at,
                sha256: None,
                link_target: Some("a.jpg".into()),
            },
        ] {
            fixture::insert_entry(&conn, 1, e);
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog.db");
        crate::db::ontape_catalog::write(&conn, &[1], &path).unwrap();
        let ontape = rusqlite::Connection::open(&path).unwrap();

        let queries: Vec<String> = RECOVERY_CATALOG_DB
            .lines()
            .filter_map(|l| l.strip_prefix("sqlite3 catalog.db "))
            .map(|q| {
                let quote = q.chars().next().unwrap();
                assert!(quote == '\'' || quote == '"', "a quoted query: {q}");
                let inner = q.strip_prefix(quote).unwrap().strip_suffix(quote).unwrap();
                assert!(
                    !inner.contains(quote) && !inner.contains('$') && !inner.contains('\\'),
                    "the shell passes it to sqlite3 unchanged: {inner}"
                );
                inner.replace("UNIT", "photos")
            })
            .collect();
        assert_eq!(queries.len(), 2, "{RECOVERY_CATALOG_DB}");

        let rows = |sql: &str| -> Vec<Vec<rusqlite::types::Value>> {
            let mut stmt = ontape.prepare(sql).unwrap();
            let n = stmt.column_count();
            stmt.query_map([], |r| (0..n).map(|i| r.get(i)).collect())
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        use rusqlite::types::Value::{Integer, Null, Text};
        let s = |t: &str| Text(t.to_string());
        assert_eq!(
            rows(&queries[0]),
            vec![vec![s("photos"), Integer(3), Integer(3)]]
        );
        let when = s("2026-09-01 12:00:00");
        assert_eq!(
            rows(&queries[1]),
            vec![
                vec![s("2019"), s("dir"), Integer(0), s(""), when.clone(), Null],
                vec![
                    s("2019/a.jpg"),
                    s("regular"),
                    Integer(5),
                    s(&"ab".repeat(32)),
                    when.clone(),
                    Null
                ],
                vec![
                    s("2019/latest"),
                    s("symlink"),
                    Integer(5),
                    s(""),
                    when,
                    s("a.jpg")
                ],
            ]
        );
    }
}
