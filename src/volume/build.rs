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

use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::error::{Result, TapectlError};
use crate::staging;

use super::format;
use super::layout::{self, FrontIndexFile, IdThunkV2Params};
use super::layout_model::{
    CapacityBudget, ContentSource, KeyAvailability, Layout, LayoutEntry, LayoutError, ZoneKind,
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
    /// In the order they will be written (dar's slice-number order).
    pub slices: Vec<BuildSlice>,
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
    pub mam_serial: String,
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
pub fn build(inputs: &BuildInputs, session_dir: &Path) -> Result<BuiltLayout> {
    fs::create_dir_all(session_dir)?;

    let volume_uuid = Uuid::parse_str(&inputs.volume_uuid).map_err(|e| {
        TapectlError::Other(format!(
            "build: volume_uuid {:?} is not a valid UUID: {e}",
            inputs.volume_uuid
        ))
    })?;

    // Positions are pure arithmetic over counts, fixed before any byte is
    // generated (format §1: 4 front files, permuted tenant envelopes,
    // operator + backup, unit-contiguous slices, seal marker last).
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
    let data_start = op_backup_pos + 1;

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
    };
    let id_thunk_bytes = layout::generate_id_thunk_v2(&id_thunk_params);
    let (id_thunk_path, id_thunk_size, id_thunk_hash) =
        materialize(session_dir, "0000_id_thunk", id_thunk_bytes.as_bytes())?;
    entries.push(LayoutEntry {
        position: 0,
        kind: ZoneKind::IdThunk,
        size_bytes: Some(id_thunk_size),
        sha256: Some(id_thunk_hash),
        source: ContentSource::Materialized(id_thunk_path),
    });

    let guide_bytes = layout::generate_system_guide_v2(&inputs.label, total_files);
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
        let recovery =
            layout::generate_recovery_md(&inputs.label, &tenant.tenant_name, &manifest_units);
        let catalogs = catalogs_for_tenant(inputs, tenant_id);
        // PLAN.toml is operator-only (`volume-format-v2.md` §1's middle-zone
        // table: "PLAN.toml (op only)") — tenant envelopes never carry it.
        let tar = build_envelope_tar(&manifest, &recovery, &catalogs, None)?;

        let mut recipients: Vec<String> = tenant.public_keys.clone();
        recipients.extend(inputs.operator_public_keys.iter().cloned());
        let recipients = with_escrow(recipients, &inputs.escrow_public_key);
        let encrypted = staging::encrypt_data(&tar, &recipients)?;

        let pos = first_envelope_pos + i as i32;
        let (path, size, hash) = materialize(
            session_dir,
            &format!("{pos:04}_tenant_envelope_{tenant_id}"),
            &encrypted,
        )?;
        entries.push(LayoutEntry {
            position: pos,
            kind: ZoneKind::TenantEnvelope { tenant_id },
            size_bytes: Some(size),
            sha256: Some(hash),
            source: ContentSource::Materialized(path),
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
    let op_recovery = layout::generate_recovery_md(&inputs.label, "operator", &all_manifest_units);
    let all_catalogs = catalogs_for_all(inputs);
    // PLAN.toml survives the standalone v1 planning-header zone as an
    // operator-envelope tar member (`volume-format-v2.md` §8's "What v2
    // removes": "the standalone zone... [is] removed at the write flip"; the
    // generator itself, `generate_planning_header`, is unchanged — only its
    // caller and packaging change here).
    let plan_toml = layout::generate_planning_header(&inputs.label, &plan_units_all(inputs));
    let op_tar = build_envelope_tar(&op_manifest, &op_recovery, &all_catalogs, Some(&plan_toml))?;

    let op_recipients = with_escrow(
        inputs.operator_public_keys.clone(),
        &inputs.escrow_public_key,
    );
    let op_encrypted = staging::encrypt_data(&op_tar, &op_recipients)?;

    let (op_path, op_size, op_hash) = materialize(
        session_dir,
        &format!("{op_pos:04}_operator_envelope"),
        &op_encrypted,
    )?;
    entries.push(LayoutEntry {
        position: op_pos,
        kind: ZoneKind::OperatorEnvelope,
        size_bytes: Some(op_size),
        sha256: Some(op_hash),
        source: ContentSource::Materialized(op_path),
    });

    let (op_backup_path, op_backup_size, op_backup_hash) = materialize(
        session_dir,
        &format!("{op_backup_pos:04}_operator_envelope_backup"),
        &op_encrypted,
    )?;
    entries.push(LayoutEntry {
        position: op_backup_pos,
        kind: ZoneKind::OperatorEnvelopeBackup,
        size_bytes: Some(op_backup_size),
        sha256: Some(op_backup_hash),
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
    let seal_bytes =
        layout::generate_seal_marker(&inputs.label, total_files, &fi_hash, &seal_files);
    let seal_bytes_recheck =
        layout::generate_seal_marker(&inputs.label, total_files, &fi_hash, &seal_files);
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

    let layout = Layout {
        label: inputs.label.clone(),
        volume_uuid: inputs.volume_uuid.clone(),
        media_type: inputs.media_type.clone(),
        block_size: inputs.block_size,
        budget: CapacityBudget {
            available_bytes: inputs.usable_bytes,
            reserve_bytes: inputs.enospc_buffer,
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
    /// staged-slice full-hash, materialized-zone size/hash, keys —
    /// `layout_model.rs`) plus the parts that need a file read and a
    /// subprocess, which don't belong in that dependency-light module
    /// (`layout-session.md` validation point 4 / plan T5b point 3): the
    /// front index parses and is internally consistent
    /// (`format::parse_front_index` + `format::validate_consistency`, must
    /// be violation-free), the seal marker parses
    /// (`format::parse_seal_marker`), and RESTORE.sh passes `bash -n`.
    /// Collects every failure — never stops at the first, matching
    /// `Layout::validate`'s pre-flight-report convention.
    pub fn validate(&self, keys: &KeyAvailability) -> std::result::Result<(), Vec<LayoutError>> {
        let mut errs = Vec::new();
        if let Err(mut layout_errs) = self.layout.validate(keys) {
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
                        message: format!("{violations:?}"),
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

fn to_manifest_unit(unit: &BuildUnit, positions: &HashMap<i64, i32>) -> layout::ManifestUnit {
    layout::ManifestUnit {
        name: unit.unit_name.clone(),
        uuid: unit.unit_uuid.clone(),
        snapshot_version: unit.snapshot_version,
        stage_set_id: unit.stage_set_id,
        dar_version: unit.dar_version.clone(),
        dar_command: unit.dar_command.clone(),
        slices: unit
            .slices
            .iter()
            .map(|sl| layout::ManifestSlice {
                number: sl.slice_number,
                tape_position: positions.get(&sl.slice_id).copied().unwrap_or(0),
                size_bytes: sl.size_bytes,
                encrypted_bytes: sl.encrypted_bytes,
                sha256_plain: sl.sha256_plain.clone(),
                sha256_encrypted: sl.sha256_encrypted.clone(),
            })
            .collect(),
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

fn catalogs_for_tenant(inputs: &BuildInputs, tenant_id: i64) -> Vec<(String, Vec<u8>)> {
    inputs
        .units
        .iter()
        .filter(|u| u.tenant_id == tenant_id)
        .filter_map(|u| u.catalog_path.as_deref())
        .flat_map(read_catalog_files)
        .collect()
}

fn catalogs_for_all(inputs: &BuildInputs) -> Vec<(String, Vec<u8>)> {
    inputs
        .units
        .iter()
        .filter_map(|u| u.catalog_path.as_deref())
        .flat_map(read_catalog_files)
        .collect()
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

/// Read a unit's isolated dar catalogue slice files (`catalog_base.N.dar`)
/// for inclusion in an envelope tar. Mirrors `write.rs::read_catalog_files`
/// (private there — replicated here per the T5b scope fence rather than
/// widening write.rs's visibility, which is out of scope for this task).
fn read_catalog_files(catalog_path: &str) -> Vec<(String, Vec<u8>)> {
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
                if let Ok(bytes) = fs::read(e.path()) {
                    out.push((fname, bytes));
                }
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Envelope tar builder: MANIFEST.toml + RECOVERY.md + `catalogs/*`, plus
/// PLAN.toml when `plan_toml` is `Some` (T8: the operator envelope only,
/// `volume-format-v2.md` §1/§8 — tenant envelope call sites pass `None`).
/// Mirrors `write.rs::build_envelope_tar` (private there, and now deleted —
/// the v1 write pipeline this replicated is gone per the T8 flip).
fn build_envelope_tar(
    manifest: &str,
    recovery: &str,
    catalogs: &[(String, Vec<u8>)],
    plan_toml: Option<&str>,
) -> Result<Vec<u8>> {
    let mut tar_buf = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_buf);
        append_tar_member(&mut builder, "MANIFEST.toml", manifest.as_bytes())?;
        append_tar_member(&mut builder, "RECOVERY.md", recovery.as_bytes())?;
        if let Some(plan) = plan_toml {
            append_tar_member(&mut builder, "PLAN.toml", plan.as_bytes())?;
        }
        for (name, bytes) in catalogs {
            append_tar_member(&mut builder, &format!("catalogs/{name}"), bytes)?;
        }
        builder
            .finish()
            .map_err(|e| TapectlError::Other(format!("envelope tar finish: {e}")))?;
    }
    Ok(tar_buf)
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
            mam_length: 0,
            mam_loads: 0,
            units,
            tenants,
            operator_public_keys: vec![op_key.public_key],
            escrow_public_key: None,
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
        // envelopes, 6 operator, 7 operator backup, 8-9 slices, 10 seal.
        assert_eq!(entries.len(), 11);
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
        assert_eq!(entries[8].kind, ZoneKind::Slice { stage_slice_id: 1 });
        assert_eq!(entries[9].kind, ZoneKind::Slice { stage_slice_id: 2 });
        assert_eq!(entries[10].kind, ZoneKind::SealMarker);

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
        assert!(built.validate(&ok_keys(&inputs)).is_ok());
    }

    #[test]
    fn validate_catches_over_capacity() {
        let (mut inputs, _src) = two_tenant_inputs("550e8400-e29b-41d4-a716-446655440000");
        inputs.usable_bytes = 1; // absurdly small — everything overflows it
        let session = tempfile::tempdir().unwrap();
        let built = build(&inputs, session.path()).unwrap();
        let errs = built.validate(&ok_keys(&inputs)).unwrap_err();
        assert!(errs
            .iter()
            .any(|e| matches!(e, LayoutError::CapacityExceeded { .. })));
    }

    #[test]
    fn validate_catches_a_missing_staged_slice() {
        let (inputs, src) = two_tenant_inputs("550e8400-e29b-41d4-a716-446655440000");
        let session = tempfile::tempdir().unwrap();
        let built = build(&inputs, session.path()).unwrap();

        // The source slice file vanishes between stage and build/validate
        // time.
        std::fs::remove_file(src.path().join("slice_1.age")).unwrap();

        let errs = built.validate(&ok_keys(&inputs)).unwrap_err();
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
        // its hash — sacred invariant 2: validate() must full-hash from
        // disk, never trust the recorded checksum alone.
        let path = src.path().join("slice_1.age");
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[0] ^= 0xFF;
        std::fs::write(&path, bytes).unwrap();

        let errs = built.validate(&ok_keys(&inputs)).unwrap_err();
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
        let errs = built.validate(&keys).unwrap_err();
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

        let errs = built.validate(&ok_keys(&inputs)).unwrap_err();
        assert!(
            errs.iter()
                .any(|e| matches!(e, LayoutError::GeneratedZoneInconsistent { .. })),
            "expected the hollow data_slice entry (missing size/hash) to trip \
             format::validate_consistency; got {errs:?}"
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
}
