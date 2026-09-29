pub mod clean;
pub mod exclude;
pub mod lock;
pub mod validate;

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};
use tracing::info;

use crate::config::{Config, TapectlPaths};
use crate::dar;
use crate::db::{events, models, queries};
use crate::error::{Result, TapectlError};
use crate::util::{HashingReader, HashingWriter};

/// Outcome of `snapshot_create_detailed` (issue #159 / ADR-0012): whether a
/// new version was actually minted, and which row — new or reused — the
/// caller should report or act on.
#[derive(Debug, Clone)]
pub struct SnapshotOutcome {
    pub snapshot_id: i64,
    pub version: i64,
    /// `false` when the walk matched an existing snapshot's recorded
    /// content and nothing new was created — `snapshot_id`/`version`/
    /// `status` then describe that existing row, not a fresh one.
    pub minted: bool,
    /// The snapshot row's status (`created`, `staged`, or `current`) —
    /// lets a caller like `collection::batch::execute_batch` tell "exists
    /// but never staged" from "already staged" from "already on tape"
    /// apart on the `minted: false` path, where none of that is obvious
    /// from `snapshot_id`/`version` alone.
    pub status: String,
}

/// Create a snapshot: fast directory walk, manifest, files table — unless
/// the walk already matches the unit's most recent snapshot (ADR-0012,
/// issue #159): "a version is minted only when content changed." A version
/// number names content, so two versions of a unit never hold identical
/// content; on a match this reports the existing version and creates
/// nothing (`minted: false`) rather than mint a byte-identical sibling.
///
/// The comparison reuses `unit::content_match::matches_snapshot` — the
/// exact predicate `unit status --dirty`/`report dirty`/`audit` already use
/// to decide *Dirty* (`collection::fingerprint::classify`) — against the
/// walk this function has *already* performed (`manifest_entries`, below).
/// It is never re-walked: a second `walk_directory`/`WalkDir` pass here
/// would silently double every snapshot's wall-clock time, which is the
/// exact regression issue #159 warns against.
///
/// `config.defaults.global_excludes` (issue #49 item 5) is passed through
/// to `walk_directory` so the recorded `files`/manifest rows never include
/// a file dar itself was never going to archive (the unit's own dotfile
/// excludes are read internally by `walk_directory`, keyed off
/// `source_path`). `config` also supplies `defaults.large_file_warn_threshold`
/// for the large-file warning (issue #52, design line 203).
pub fn snapshot_create_detailed(
    conn: &Connection,
    unit_name: &str,
    config: &Config,
) -> Result<SnapshotOutcome> {
    let global_excludes = &config.defaults.global_excludes;
    let unit = queries::get_unit_by_name(conn, unit_name)?
        .ok_or_else(|| TapectlError::UnitNotFound(unit_name.to_string()))?;

    let source_path = unit
        .current_path
        .as_deref()
        .ok_or_else(|| TapectlError::Other(format!("unit \"{unit_name}\" has no path")))?;

    if !Path::new(source_path).is_dir() {
        return Err(TapectlError::UnitPathNotFound(source_path.to_string()));
    }

    // Nested unit detection (design line 184): "unit init and snapshot
    // create check parent/child. Both errors." Excludes the unit's own
    // row (change 3, issue #52) so a snapshot of an already-registered
    // unit doesn't trip on itself. Runs before the directory walk to fail
    // fast, before doing expensive work.
    crate::unit::nesting::check_nesting_excluding(conn, source_path, Some(unit.id))?;

    // Walk directory and build manifest — the ONLY walk this function
    // performs. Both the content-match short-circuit immediately below and
    // the mint path that follows it reuse `manifest_entries` (issue #159).
    let (total_size, file_count, manifest_entries) = walk_directory(source_path, global_excludes)?;

    // ADR-0012 / issue #159: does this walk already match the unit's most
    // recent snapshot? `content_match::latest_snapshot` is the exact same
    // "latest" lookup `collection::fingerprint::classify` uses, so the two
    // can never pick different rows and disagree about what "latest" means
    // (the drift this predicate exists to prevent).
    if let Some((latest_id, latest_version, latest_status)) =
        crate::unit::content_match::latest_snapshot(conn, unit.id)?
    {
        let mut fresh_stamps: Vec<crate::unit::content_match::FileStamp> = manifest_entries
            .iter()
            .filter(|e| !e.is_dir)
            .map(|e| crate::unit::content_match::FileStamp {
                path: e.path.clone(),
                size_bytes: e.size,
                modified_at: e.mtime.clone(),
            })
            .collect();
        fresh_stamps.sort();

        let diff = crate::unit::content_match::matches_snapshot(
            conn,
            latest_id,
            &unit.checksum_mode,
            Path::new(source_path),
            &fresh_stamps,
            crate::staging::validate::hash_source_file,
        )?;

        if diff.is_empty() {
            match latest_status.as_str() {
                // Change 2: the live, on-tape version already IS this
                // content — report it, mint nothing.
                //
                // Change 3: an unwritten snapshot (never staged, or staged
                // but never written) already holds this exact content —
                // reuse that row rather than mint a byte-identical sibling
                // beside it. Either way the caller gets `minted: false`
                // and reads `status` to tell which case it was.
                "current" | "created" | "staged" => {
                    return Ok(SnapshotOutcome {
                        snapshot_id: latest_id,
                        version: latest_version,
                        minted: false,
                        status: latest_status,
                    });
                }
                // superseded/reclaimable/purged/failed: a dead row. Content
                // happening to match it is coincidence, not identity —
                // mint fresh rather than resurrect it.
                _ => {}
            }
        }
    }

    // Determine next version number
    let next_version: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) + 1 FROM snapshots WHERE unit_id = ?1",
        params![unit.id],
        |row| row.get(0),
    )?;

    // Empty units: warn but allow (design line 185). Gated on `file_count`,
    // not `total_size` — a unit full of zero-byte files is not empty.
    if file_count == 0 {
        tracing::warn!(unit = %unit_name, "unit has no files; snapshot will be empty");
    }

    // Large files: warn on any file exceeding `large_file_warn_threshold`
    // (design line 203). Threshold computed once, outside the loop.
    // `parse_size_to_bytes` (issue #59) now rejects a malformed threshold
    // rather than silently miscomputing it; `config.defaults.*` is also
    // validated at config load, so in practice this only fails if that
    // guard is ever bypassed.
    let large_file_threshold = parse_size_to_bytes(&config.defaults.large_file_warn_threshold)?;
    for entry in &manifest_entries {
        if !entry.is_dir && entry.size > large_file_threshold {
            tracing::warn!(
                path = %entry.path,
                size_bytes = entry.size,
                threshold_bytes = large_file_threshold,
                "file exceeds large_file_warn_threshold"
            );
        }
    }

    // Insert snapshot
    conn.execute(
        "INSERT INTO snapshots (unit_id, version, source_path, total_size, file_count)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![unit.id, next_version, source_path, total_size, file_count],
    )?;
    let snapshot_id = conn.last_insert_rowid();

    // Insert manifest
    conn.execute(
        "INSERT INTO manifests (snapshot_id) VALUES (?1)",
        params![snapshot_id],
    )?;
    let manifest_id = conn.last_insert_rowid();

    // Insert manifest entries and files
    let mut file_insert = conn.prepare(
        "INSERT INTO files (snapshot_id, path, size_bytes, modified_at, is_directory,
                            file_type, link_target)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?;
    let mut manifest_insert = conn.prepare(
        // No has_xattrs/has_acls (issue #149, migration 013): both were bound
        // as literal 0 here under a comment claiming they were "populated on
        // stage", nothing ever fulfilled it, and nothing ever read them. dar
        // owns xattr/ACL handling and records what it preserved in its own
        // archive catalog; a second copy here could only drift from it.
        "INSERT INTO manifest_entries (manifest_id, path, size_bytes, mtime, is_directory,
                                       mode, uid, gid, username, groupname,
                                       file_type, link_target)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
    )?;

    for entry in &manifest_entries {
        file_insert.execute(params![
            snapshot_id,
            entry.path,
            entry.size,
            entry.mtime,
            entry.is_dir,
            entry.file_type,
            entry.link_target,
        ])?;
        manifest_insert.execute(params![
            manifest_id,
            entry.path,
            entry.size,
            entry.mtime,
            entry.is_dir,
            entry.mode,
            entry.uid,
            entry.gid,
            entry.username,
            entry.groupname,
            entry.file_type,
            entry.link_target,
        ])?;
    }

    events::log_created(
        conn,
        "snapshot",
        snapshot_id,
        &format!("{unit_name} v{next_version}"),
        Some(unit.tenant_id),
    )?;

    Ok(SnapshotOutcome {
        snapshot_id,
        version: next_version,
        minted: true,
        status: "created".to_string(),
    })
}

/// Thin wrapper over `snapshot_create_detailed`, kept at its original
/// signature and return type for the ~40 existing call sites across the
/// tree (issue #159) that only ever wanted the new snapshot's id and must
/// keep compiling untouched. A caller that needs to tell "created" from
/// "unchanged, nothing minted" apart — the `snapshot` CLI command,
/// `collection::batch::execute_batch` — calls `snapshot_create_detailed`
/// directly instead.
pub fn snapshot_create(conn: &Connection, unit_name: &str, config: &Config) -> Result<i64> {
    Ok(snapshot_create_detailed(conn, unit_name, config)?.snapshot_id)
}

/// Stage set statuses that mean "live slices already exist for this
/// snapshot" — re-staging on top of one is pointless (the existing slices
/// should go to `volume write`) and would silently produce a second,
/// unrelated copy. Under migration 001's `CHECK(status IN ('staging',
/// 'staged','failed','cleaned'))`, this is exactly the complement of
/// `'cleaned'`/`'failed'`, but it's named after the blocking condition
/// (what `stage create --version`'s refusal message describes), not the
/// allowed one — the single place this status list is written (issue #96:
/// five inlined status lists is how that issue happened).
pub(crate) fn stage_set_has_live_slices(status: &str) -> bool {
    matches!(status, "staging" | "staged")
}

/// Full stage pipeline: validate → dar → encrypt → checksums.
///
/// Everything that can refuse without touching the source runs first,
/// before the `stage_sets` INSERT (issue #354): the escrow recipient, the
/// tenant's and operator's keys, and the staging directory (created, then
/// proved writable). A staging directory too small for the unit is refused
/// after the sha256 pass and before dar: that pass reads every byte, and
/// nothing short of reading the content bounds what dar stores (see
/// [`check_staging_space`]). One that MAY be too small — the need cannot be
/// pinned down — is asked about (ADR-0008 Tier 2): `assume_yes` (the global
/// `--yes`) is that consent given in advance, and without it a
/// non-interactive run refuses. What remains to fail after dar is the work
/// itself.
///
/// Thin wrapper around `stage_create_inner` mirroring
/// `encrypt_file_streaming`'s "wrapper does cleanup on `Err`" pattern
/// (issue #54): on failure, best-effort cleanup runs before the original
/// error is returned unchanged — cleanup never masks the real error.
pub fn stage_create(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    snapshot_id: i64,
    assume_yes: bool,
) -> Result<i64> {
    stage_create_reporting(
        conn,
        paths,
        config,
        snapshot_id,
        assume_yes,
        &mut std::io::stderr(),
    )
}

/// [`stage_create`] with its operator notices written to `notices` rather
/// than straight to stderr — the seam the tests read them through.
///
/// A notice is something the operator must see whatever `[logging] level`
/// says (issue #347: `encrypt = false` used to be reported only through
/// `tracing::warn!`, which `logging.level = "error"` silences). So notices
/// never go through `tracing`; `stage_create` hands this stderr, the way
/// `cli::key::print_escrow_secret_warning` bypasses the log level, and
/// stdout stays clean for `--json`.
pub(crate) fn stage_create_reporting(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    snapshot_id: i64,
    assume_yes: bool,
    notices: &mut dyn Write,
) -> Result<i64> {
    let stage_set_id_holder: std::cell::Cell<Option<i64>> = std::cell::Cell::new(None);
    // Holds the stage set's flock guard for the entire lifetime of this
    // function call, success or error (issue #98) — `lock::StageLock`
    // unlocks on drop, and this `Cell` lives until `stage_create` returns,
    // so the lock is released only once the caller gets control back
    // (after `cleanup_failed_stage_set` has already run on the error path
    // below).
    let lock_holder: std::cell::Cell<Option<lock::StageLock>> = std::cell::Cell::new(None);
    match stage_create_inner(
        conn,
        paths,
        config,
        snapshot_id,
        assume_yes,
        &stage_set_id_holder,
        &lock_holder,
        notices,
    ) {
        Ok(id) => Ok(id),
        Err(e) => {
            if let Some(stage_set_id) = stage_set_id_holder.get() {
                cleanup_failed_stage_set(conn, config, stage_set_id);
            }
            Err(e)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn stage_create_inner(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    snapshot_id: i64,
    assume_yes: bool,
    stage_set_id_holder: &std::cell::Cell<Option<i64>>,
    lock_holder: &std::cell::Cell<Option<lock::StageLock>>,
    notices: &mut dyn Write,
) -> Result<i64> {
    // ADR-0005 / issue #115. First thing, before the `stage_sets` INSERT and
    // long before dar: without a registered escrow recipient,
    // `recipient_list_with_escrow` below is a silent no-op and every slice
    // this run produces is encrypted to tenant + operator only. Those slices
    // are exactly what the escrow line exists to be able to open, and
    // `volume write`'s pre-flight now refuses them
    // (`LayoutError::StageSetLacksEscrow`) — so staging them at all would
    // spend hours of dar + age on material that cannot be written and cannot
    // be repaired in place. Fail here, cheaply, with the remedy.
    //
    // The check belongs to this caller, not to `recipient_list_with_escrow`:
    // that helper is also how `volume write` builds its ENVELOPE recipient
    // lists, and must keep working as a no-op for callers that are not
    // minting new ciphertext (issue #115's scope fence).
    if queries::escrow_public_key(conn)?.is_none() {
        return Err(TapectlError::Other(
            "no escrow recipient is registered — staged slices would be unrecoverable \
             with the escrow key and volume write would refuse them (ADR-0005). \
             Register one first: `tapectl key generate --escrow`, or adopt an existing \
             public key with `tapectl key import --escrow <age1...>`"
                .into(),
        ));
    }

    let snapshot = get_snapshot(conn, snapshot_id)?;
    let unit = get_unit_for_snapshot(conn, &snapshot)?;
    let tenant = queries::get_tenant_by_id(conn, unit.tenant_id)?
        .ok_or_else(|| TapectlError::Other("tenant not found".into()))?;

    // Issue #354 (c): the recipient list is a pure database read, so it is
    // built — and every refusal it can raise is raised — here, before the
    // `stage_sets` INSERT and before dar, beside the escrow check above. It
    // used to be built at the top of the encryption loop, after dar had
    // archived the whole unit, so a tenant with no active keys cost a full
    // dar run to discover.
    let tenant_keys = queries::get_active_keys_for_tenant(conn, unit.tenant_id)?;
    // Refuse rather than silently encrypt operator-only: a tenant with zero
    // active keys (e.g. an interrupted rotation, pre-H13 fix) would otherwise
    // produce slices the tenant can never decrypt themselves.
    if tenant_keys.is_empty() {
        return Err(TapectlError::Other(format!(
            "tenant for unit \"{}\" has no active keys — refusing to encrypt \
             (the tenant could not decrypt its own data); run `tapectl key rotate` \
             or restore the tenant's keys first",
            unit.name
        )));
    }
    let operator = queries::get_operator_tenant(conn)?
        .ok_or_else(|| TapectlError::Other("no operator tenant".into()))?;
    let operator_keys = queries::get_active_keys_for_tenant(conn, operator.id)?;

    let all_pubkeys: Vec<String> = tenant_keys
        .iter()
        .chain(operator_keys.iter())
        .map(|k| k.public_key.clone())
        .collect();
    // ADR-0005: every recipient list gets the escrow public key appended
    // (a no-op if it's already present; its absence was refused above).
    let all_pubkeys = queries::recipient_list_with_escrow(conn, all_pubkeys)?;
    // Parse every recipient now, for the same reason: a malformed public key
    // in the database would otherwise surface from the first slice's
    // `encrypt_file_streaming`, after dar. The encryptor itself is rebuilt
    // per slice (age draws a fresh file key each time); this one is only the
    // check.
    build_encryptor(&all_pubkeys)?;

    // Resolve policy (dotfile > archive_set > defaults) — issue #47/#48:
    // stage_create used to read config.defaults.* unconditionally, so an
    // archive_set or dotfile override of slice_size/compression/preserve_*
    // was silently discarded even where #48 gives archive_set_id a writer.
    let resolved = crate::policy::resolve(conn, config, &unit)?;

    // `ResolvedPolicy.slice_size` is bytes-only at every layer (even
    // `policy::resolve`'s own default layer runs `config.defaults.slice_size`
    // through `parse_size_to_bytes`), but dar's `-s` argument must keep
    // receiving a *string dar parses itself* — see `resolve_slice_size_string`
    // for why this can't simply be `resolved.slice_size.to_string()`
    // unconditionally (issue #59's known parser defects must never reach
    // real on-tape slicing — only the bookkeeping column below, which is
    // `resolved.slice_size` directly now, with no second parse needed).
    let slice_size = resolve_slice_size_string(conn, config, &unit, resolved.slice_size);
    let compression = resolved.compression.clone();

    // Issue #354: the staging directory is usable, or the stage is refused
    // here — before the INSERT, the sha256 pass and dar. Whether it is big
    // enough is known now only when it plainly is; otherwise the sha256
    // pass below measures what dar must store, and the refusal comes then,
    // still before dar.
    let staging_dir = Path::new(&config.staging.directory);
    prepare_staging_dir(staging_dir)?;
    let space = StagingSpaceInputs {
        staging_dir,
        unit_name: &unit.name,
        snapshot: &snapshot,
        slice_size: resolved.slice_size,
        compression: &compression,
        assume_yes,
    };
    let space_upper = staging_space_before_reading(&space, notices)?;

    // ADR-0005's escrow recipient participates in every write, and
    // pre-write validation refuses without one — encryption cannot be made
    // optional without contradicting that, and doing so would also breach
    // the sacred no-plaintext-tenant-identity-on-tape invariant. Coordinator
    // decision (issues #47/#48, 2026-07-29): never refuse the stage and
    // never silently ignore a `policy.encrypt = false`, but never honor it
    // either — warn loudly and encrypt regardless.
    //
    // Issue #347: "loudly" means a notice, not `tracing::warn!` — the log
    // level (`[logging] level`, default "warn") could silence it, and an
    // operator who set `encrypt = false` must never be left believing it
    // took effect.
    if !resolved.encrypt {
        let _ = writeln!(
            notices,
            "warning: unit \"{}\": policy sets encrypt = false, which tapectl never \
             honours — its slices are encrypted anyway, to the tenant, operator and escrow \
             recipients (ADR-0005). Remove `encrypt = false` from the unit's archive set \
             or from [defaults].",
            unit.name
        );
    }

    // Create stage_set record.
    //
    // Issue #98: the row must never be visible to another connection at
    // `status = 'staging'` without its flock already held, or a concurrent
    // `db::open()` sweep (`recover_orphaned_sessions`) could probe-lock it,
    // find it free, and mark a genuinely live stage_set `'failed'`. Under
    // WAL a row is invisible to other connections until commit, so this
    // whole sequence — INSERT, read back the new id, acquire the flock —
    // happens INSIDE one explicit transaction, and the flock is acquired
    // BEFORE that transaction commits. Acquiring a flock is microseconds,
    // so this does not reintroduce the long-running-transaction problem the
    // rest of this function's comments warn about (the dar run below stays
    // entirely outside any transaction, exactly as before).
    let insert_tx = conn.unchecked_transaction()?;
    insert_tx.execute(
        "INSERT INTO stage_sets (snapshot_id, slice_size, compression, encrypted)
         VALUES (?1, ?2, ?3, 1)",
        params![snapshot_id, resolved.slice_size, compression],
    )?;
    let stage_set_id = insert_tx.last_insert_rowid();
    stage_set_id_holder.set(Some(stage_set_id));

    let stage_lock = lock::acquire(&paths.db_file, stage_set_id)?;
    lock_holder.set(Some(stage_lock));

    insert_tx.commit()?;

    // Step 1: SHA256 source validation
    info!("validating source checksums");
    let validate::SourceValidation {
        checksums,
        nonzero_bytes,
    } = validate::validate_source(
        conn,
        snapshot_id,
        &snapshot.source_path,
        &config.defaults.global_excludes,
    )?;

    conn.execute(
        "UPDATE stage_sets SET source_validated_at = datetime('now') WHERE id = ?1",
        params![stage_set_id],
    )?;

    // Issue #354: the read just made is what bounds dar's archive from
    // below, so a staging directory too small for the unit is refused here,
    // after the one full read and before dar.
    if let Some(upper) = space_upper {
        check_staging_space(&space, upper, nonzero_bytes, notices)?;
    }

    // Step 2: Run dar
    //
    // `archive_base` is per-*stage-set* (issue #53): it carries
    // `stage_set_id` so two stage sets of the SAME snapshot (a re-stage
    // after `staging clean` released the first one) never write
    // identically-named `.age` files and silently overwrite each other.
    // `cleanup_failed_stage_set`'s prefix derivation below must move in
    // lockstep with this — both go through `archive_base_name`.
    let archive_base = staging_dir.join(archive_base_name(
        &unit.uuid,
        snapshot.version,
        stage_set_id,
    ));

    // Issue #49 items 2/5: dar's -X masks must see BOTH layers of
    // "effective excludes" — config.defaults.global_excludes (today's only
    // source) AND the unit's own dotfile `[excludes] patterns` (until this
    // fix, read/written but never consumed here). stage_create already has
    // both `config` and the snapshot's own `source_path` in scope, so this
    // merge is fully local — no threading through other callers needed
    // (contrast walk_directory/walk_fingerprint's dotfile-only interim
    // state; see exclude::dotfile_patterns's doc comment for why).
    let mut dar_exclude_patterns = config.defaults.global_excludes.clone();
    dar_exclude_patterns.extend(exclude::dotfile_patterns(Path::new(&snapshot.source_path))?);
    // Issue #359 (c): split into dar's two mask kinds, so what dar archives
    // is exactly what `walk_directory` recorded — a plain pattern is a `-X`
    // basename mask as before, and a directory pattern (`name/`) becomes the
    // `-P` prune masks `name` and `*/name`. Handed to `-X` raw, `name/`
    // matched nothing in dar and the subtree reached tape uncatalogued.
    let dar_masks = exclude::dar_masks(&dar_exclude_patterns);

    let dar_result = dar::create::create_archive(&dar::create::DarCreateParams {
        dar_binary: &config.dar.binary,
        source_path: Path::new(&snapshot.source_path),
        archive_base: &archive_base,
        slice_size: &slice_size,
        compression: &compression,
        exclude_patterns: &dar_masks.exclude,
        exclude_paths: &dar_masks.prune,
        preserve_xattrs: resolved.preserve_xattrs,
        preserve_fsa: resolved.preserve_fsa,
    })?;

    info!(slices = dar_result.num_slices, "dar archive created");

    conn.execute(
        "UPDATE stage_sets SET dar_version = ?1, dar_command = ?2 WHERE id = ?3",
        params![dar_result.dar_version, dar_result.dar_command, stage_set_id],
    )?;

    // Step 3: Extract dar catalog (per-snapshot, first stage only)
    let existing_catalogs: i64 = conn.query_row(
        "SELECT COUNT(*) FROM stage_sets WHERE snapshot_id = ?1 AND catalog_path IS NOT NULL",
        params![snapshot_id],
        |row| row.get(0),
    )?;

    // `catalog_base` is deliberately PER-SNAPSHOT ({uuid8}_v{version}), NOT
    // per-stage-set like `archive_base` above (issue #53) — the dar catalog
    // is a pure function of the snapshot's content, so the `existing_catalogs
    // == 0` guard below extracts it once per snapshot and every later stage
    // set of that snapshot reuses it. Do not change this to include
    // stage_set_id "for consistency" with archive_base.
    let catalog_dir = paths.catalogs_dir.join(&unit.uuid[..8]);
    let catalog_base = catalog_dir.join(format!("{}_v{}", &unit.uuid[..8], snapshot.version));
    if existing_catalogs == 0 {
        info!("extracting dar catalog");
        // Issue #41: `catalogs_dir` itself is secured by `ensure_dirs`, but
        // this per-unit subdirectory is a fresh, nested `create_dir_all`
        // that gets whatever the process umask hands out unless tightened
        // explicitly — a parent's mode does not propagate to children it
        // didn't create. Pre-create it 0700 so dar's own `create_dir_all`
        // (inside `extract_catalog`, a no-op once it already exists) never
        // gets a chance to leave it loose.
        fs::create_dir_all(&catalog_dir)?;
        crate::config::secure_path(&catalog_dir, 0o700);
        dar::create::extract_catalog(&config.dar.binary, &archive_base, &catalog_base)?;
        // dar wrote the catalog file(s) itself via subprocess, with no mode
        // of its own — tighten what it produced after the fact.
        secure_catalog_files(&catalog_dir);
    }
    conn.execute(
        "UPDATE stage_sets SET catalog_path = ?1 WHERE id = ?2",
        params![catalog_base.to_string_lossy().to_string(), stage_set_id],
    )?;

    // Step 4: Encrypt slices, to the recipient list built (and checked)
    // before dar ran.
    info!("encrypting slices");

    // fingerprint == public_key by construction for every key in this system
    // (see crypto::keys::generate_keypair and `key import`), so the recorded
    // fingerprints are exactly the (now escrow-augmented) recipient list —
    // keeping this audit record honest about who can actually decrypt the
    // slices it describes, rather than a second, silently-divergent list.
    let key_fingerprints = all_pubkeys.clone();

    let mut total_dar_size: i64 = 0;
    let mut total_encrypted_size: i64 = 0;

    for (i, slice_path) in dar_result.slice_paths.iter().enumerate() {
        let slice_num = (i + 1) as i64;

        // Streams the plaintext slice straight to its `.age` file, hashing
        // both sides as they flow — peak RAM is the copy buffer, never the
        // slice size (H9 fix, issue #35; see `encrypt_file_streaming`).
        let encrypted_path = PathBuf::from(format!("{}.age", slice_path.display()));
        let info = encrypt_file_streaming(slice_path, &encrypted_path, &all_pubkeys)?;

        // Remove unencrypted slice
        fs::remove_file(slice_path)
            .map_err(|e| staging_io_error("cannot remove plaintext slice", slice_path, e))?;

        conn.execute(
            "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes, encrypted_bytes,
                                       sha256_plain, sha256_encrypted, staging_path)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                stage_set_id,
                slice_num,
                info.plain_size,
                info.encrypted_size,
                info.sha256_plain,
                info.sha256_encrypted,
                encrypted_path.to_string_lossy().to_string(),
            ],
        )?;

        total_dar_size += info.plain_size;
        total_encrypted_size += info.encrypted_size;

        info!(
            slice = slice_num,
            plain_mb = info.plain_size / (1024 * 1024),
            encrypted_mb = info.encrypted_size / (1024 * 1024),
            "encrypted slice"
        );
    }

    // Also remove any leftover sha512 hash files. dar no longer creates
    // these (create.rs dropped `-3`/`--hash sha512`, issues #50/#51 — the
    // files were never read, pure wasted I/O), but a staging dir can still
    // hold `.sha512` files left behind by archives created before that
    // change, so this cleanup path must keep matching them.
    if let Some(parent) = archive_base.parent() {
        if let Ok(entries) = fs::read_dir(parent) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                // The trailing dot is load-bearing: dar names every slice
                // `{base}.{N}.dar`, so `{base}.` is the real prefix. Matching
                // on the bare base makes `..._v1` a prefix of `..._v10.1.dar`.
                // `archive_base` is per-stage-set (issue #53) so this is
                // already narrower than "per-unit-version", but the trailing
                // dot still matters against sibling stage-set ids (`_s1` vs
                // `_s10`). Same convention as `volume::build::catalog_file_paths`.
                let prefix = format!("{}.", archive_base.file_name().unwrap().to_string_lossy());
                if name.ends_with(".sha512") && name.starts_with(&prefix) {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
    }

    // Issue #54: finalization only, not the whole pipeline, runs inside a
    // transaction — matching the `conn.unchecked_transaction()` pattern
    // already established at `src/volume/session.rs:778`,
    // `src/volume/write.rs:1262`, `src/cli/operations.rs:38`
    // (`snapshot_purge`), and `src/cli/key.rs:224`.
    //
    // Deliberately NOT wrapping the whole of `stage_create`: `unchecked_transaction`
    // is DEFERRED, so it takes SQLite's single write lock at the first write
    // inside it and holds it until commit — around the dar run (which can
    // take hours) that would block every other tapectl invocation for the
    // duration. Worse, a crash mid-dar would roll back the `stage_sets`
    // INSERT itself, destroying the `status='staging'` row that
    // `recover_orphaned_sessions` (`src/db/mod.rs`) relies on to mark the
    // set `'failed'` on the next `db::open()` — the operator's only signal
    // that something went wrong. So the incremental progress writes above
    // (the initial INSERT, the `source_validated_at`/`dar_version`/
    // `catalog_path` UPDATEs, and the per-slice `stage_slices` INSERTs)
    // stay outside any transaction; only the finalization below — which
    // only ever runs once the pipeline has fully succeeded — is atomic.
    let tx = conn.unchecked_transaction()?;

    // Update stage_set
    tx.execute(
        "UPDATE stage_sets SET status = 'staged', num_slices = ?1, total_dar_size = ?2,
         total_encrypted_size = ?3, key_fingerprints = ?4, staged_at = datetime('now')
         WHERE id = ?5",
        params![
            dar_result.num_slices as i64,
            total_dar_size,
            total_encrypted_size,
            serde_json::to_string(&key_fingerprints).unwrap(),
            stage_set_id,
        ],
    )?;

    // Update snapshot status
    let updated = tx.execute(
        "UPDATE snapshots SET status = 'staged' WHERE id = ?1 AND status = 'created'",
        params![snapshot_id],
    )?;
    if updated > 0 {
        events::log_field_change(
            &tx,
            "snapshot",
            snapshot_id,
            &format!("{} v{}", unit.name, snapshot.version),
            "status_change",
            "status",
            Some("created"),
            "staged",
            Some(unit.tenant_id),
        )?;
    }

    // Backfill sha256 into files and manifest_entries — establishes the
    // baseline ONLY where one doesn't already exist (issue #32/H6).
    // `validate_source` now refuses to stage (BITROT) before we ever get
    // here if a hash disagrees with an existing baseline, but the thing
    // that actually makes "(first stage only)" true is
    // `backfill_checksums`'s own `sha256 IS NULL` guard on both UPDATEs —
    // not this `is_empty()` check, which only skips a no-op call.
    if !checksums.is_empty() {
        backfill_checksums(&tx, snapshot_id, &checksums)?;
    }

    tx.commit()?;

    // Receipt writing is filesystem work and must not sit inside a DB
    // transaction — done here, after commit, along with the creation event.
    let receipt = generate_receipt(conn, stage_set_id, &unit, &snapshot, &tenant)?;
    let _receipt_path = write_stage_receipt(paths, stage_set_id, &receipt)?;

    events::log_created(
        conn,
        "stage_set",
        stage_set_id,
        &format!("{} v{}", unit.name, snapshot.version),
        Some(unit.tenant_id),
    )?;

    Ok(stage_set_id)
}

/// The `archive_base` file-name stem for one stage set: `{uuid12}_v{version}_s{stage_set_id}`.
///
/// Per-*stage-set*, not per-snapshot (issue #53) — the single place this
/// shape is computed, so `stage_create_inner`'s dar run and
/// `cleanup_failed_stage_set`'s prefix scan can never drift apart again.
/// `catalog_base` is deliberately NOT built this way — it stays
/// per-snapshot on purpose (see `stage_create_inner`'s catalog step).
pub(crate) fn archive_base_name(unit_uuid: &str, version: i64, stage_set_id: i64) -> String {
    format!(
        "{}_v{}_s{}",
        unit_uuid.replace('-', "").get(..12).unwrap_or(unit_uuid),
        version,
        stage_set_id,
    )
}

/// `archive_base_name(..)` plus the load-bearing trailing dot: dar names
/// every slice `{base}.{N}.dar`, so the real filesystem prefix is
/// `{base}.`, not the bare base — without the dot, `_s1` would prefix-match
/// `_s10.1.dar`.
pub(crate) fn archive_base_prefix(unit_uuid: &str, version: i64, stage_set_id: i64) -> String {
    format!("{}.", archive_base_name(unit_uuid, version, stage_set_id))
}

/// Best-effort cleanup of a `stage_set` that `stage_create` failed to
/// finish, run from the `stage_create` wrapper's `Err` path (issue #54).
///
/// Two different discovery strategies are used deliberately, for two
/// different classes of leftover file:
///
/// - **Plaintext `.dar`/`.sha512` are found by filesystem prefix, NOT by DB
///   rows.** `dar -c` creates every slice up front; the encryption loop
///   only deletes a `.dar` (and inserts its `stage_slices` row) *after*
///   writing its `.age`. So a failure partway through the encryption loop
///   — or any failure before it even starts, e.g. the zero-active-keys
///   refusal — leaves plaintext `.dar` files with no `stage_slices` row at
///   all. Iterating `stage_slices` would find exactly the files that are
///   already safe (already encrypted, already deleted) and miss every one
///   that actually matters. `archive_base_name` (issue #53) carries
///   `stage_set_id`, so it is unique per stage set, not just per snapshot —
///   a prefix scan of `staging_dir` for `{archive_base_prefix}*.dar` /
///   `*.sha512` can never collide with a sibling stage set of the same
///   snapshot, which is what makes this safe.
///
/// - **`.age` files are found by DB row (`stage_slices.staging_path`), NOT
///   by prefix.** This is strictly more precise than a prefix scan and
///   doesn't depend on the prefix reasoning above at all — deleting only
///   the rows this specific `stage_set_id` owns keeps the blast radius
///   correct regardless of how `archive_base` is shaped. Left unchanged by
///   issue #53; do not "simplify" it into a prefix scan just because one
///   would now be safe.
///
/// The `stage_sets` row itself is deliberately left alone — not deleted,
/// not re-statused. Leaving it `status='staging'` is exactly what lets
/// `recover_orphaned_sessions` (`src/db/mod.rs`) mark it `'failed'` on the
/// next `db::open()`, which is the operator's only signal that this stage
/// attempt didn't complete.
fn cleanup_failed_stage_set(conn: &Connection, config: &Config, stage_set_id: i64) {
    let staging_dir = Path::new(&config.staging.directory);

    // Resolve this stage set's archive_base prefix via the SAME
    // `archive_base_name`/`archive_base_prefix` helpers `stage_create_inner`
    // used to build it — the lockstep issue #53 requires: this is not a
    // parallel re-derivation, it's the identical computation.
    let prefix = match conn
        .query_row(
            "SELECT u.uuid, sn.version
             FROM stage_sets ss
             JOIN snapshots sn ON sn.id = ss.snapshot_id
             JOIN units u ON u.id = sn.unit_id
             WHERE ss.id = ?1",
            params![stage_set_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .ok()
    {
        Some((uuid, version)) => archive_base_prefix(&uuid, version, stage_set_id),
        None => {
            tracing::warn!(
                stage_set_id,
                "cleanup: could not resolve stage set, skipping"
            );
            return;
        }
    };

    let mut removed = 0u64;

    // Plaintext .dar / dar's .sha512 — prefix-keyed (see doc comment above).
    if let Ok(entries) = fs::read_dir(staging_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(&prefix)
                && (name.ends_with(".dar") || name.ends_with(".sha512"))
                && fs::remove_file(entry.path()).is_ok()
            {
                removed += 1;
            }
        }
    }

    // Encrypted .age — DB-row-keyed to this stage_set_id only (see doc
    // comment above); never a prefix scan.
    let age_paths: Vec<String> = match conn
        .prepare("SELECT staging_path FROM stage_slices WHERE stage_set_id = ?1")
        .and_then(|mut stmt| {
            stmt.query_map(params![stage_set_id], |row| row.get(0))?
                .collect::<std::result::Result<Vec<_>, _>>()
        }) {
        Ok(paths) => paths,
        Err(e) => {
            tracing::warn!(stage_set_id, error = %e, "cleanup: could not list stage_slices");
            Vec::new()
        }
    };
    for p in &age_paths {
        if fs::remove_file(p).is_ok() {
            removed += 1;
        }
    }

    if let Err(e) = conn.execute(
        "DELETE FROM stage_slices WHERE stage_set_id = ?1",
        params![stage_set_id],
    ) {
        tracing::warn!(stage_set_id, error = %e, "cleanup: could not delete stage_slices rows");
    }

    tracing::warn!(
        stage_set_id,
        files_removed = removed,
        "stage_create failed — removed orphaned staging files for this stage set; \
         the stage_sets row itself was left as status='staging' so the next \
         `db::open()` sweep marks it 'failed'"
    );
}

/// Write a stage receipt to `paths.receipts_dir` (creating it if needed)
/// and return the path written.
///
/// Issue #41: receipts hold the same plaintext content-metadata index
/// `tapectl.db` does (unit/tenant names, paths, sizes, checksums) — they
/// get `write_private_file`'s 0600-from-creation treatment instead of a
/// plain `fs::write` at whatever mode the process umask hands out.
/// Factored out of `stage_create` so it's testable without a real dar
/// binary or a full stage pipeline.
fn write_stage_receipt(paths: &TapectlPaths, stage_set_id: i64, receipt: &str) -> Result<PathBuf> {
    let receipt_path = paths.receipts_dir.join(format!(
        "{}_{}.txt",
        chrono::Utc::now().format("%Y%m%d"),
        stage_set_id
    ));
    fs::create_dir_all(&paths.receipts_dir)?;
    crate::config::write_private_file(&receipt_path, receipt.as_bytes(), 0o600)?;
    Ok(receipt_path)
}

/// Best-effort tighten every regular file dar's `-C` catalog extraction
/// wrote inside `catalog_dir` to 0600.
///
/// dar writes these itself via subprocess, so unlike `write_private_file`
/// there's no `open()` call under our control to set the mode at creation
/// time — this tightens what dar produced after the fact instead. Same
/// content-metadata exposure as issue #41's `tapectl.db`/receipts, just
/// produced by an external process. Non-fatal by design (`secure_path`):
/// a directory listing failure here must not sink an otherwise-successful
/// stage.
fn secure_catalog_files(catalog_dir: &Path) {
    let Ok(entries) = fs::read_dir(catalog_dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            crate::config::secure_path(&entry.path(), 0o600);
        }
    }
}

/// The literal STRING to hand `dar -s` for this unit's stage, resolved
/// through the SAME dotfile > archive_set > default priority as
/// `policy::resolve` — but never by reformatting an already-parsed byte
/// count back into a suffixed string. `resolved_bytes` must be
/// `policy::resolve(..).slice_size` for the same `unit`, so the
/// archive_set fallback below is always consistent with whatever the
/// resolver already decided.
///
/// Why this isn't simply `resolved_bytes.to_string()` unconditionally:
/// `parse_size_to_bytes` (issue #59) silently maps any suffix outside
/// K/KB/M/MB/G/GB/T/TB to a multiplier of 1 — a real defect that today is
/// confined to the `stage_sets.slice_size` *bookkeeping* column, which
/// nothing downstream trusts for the real cut (dar re-parses its own `-s`
/// argument independently). Routing dar's actual argument through that
/// same parser — even indirectly, via a byte count computed from it —
/// would let that defect reach real on-tape slice boundaries for every
/// unit, including the overwhelmingly common case with no override at
/// all. So: whichever layer has a native operator-facing string (the
/// dotfile's raw TOML value, or the system default's config string) hands
/// that string to dar untouched, exactly as before issue #47. Only the
/// archive_set layer has no string to fall back on — `archive_sets.slice_size`
/// has been byte-typed in the schema since M6, so `resolved_bytes` is its
/// only representation — but handing dar that exact byte count with no
/// suffix is lossless and valid syntax: dar's own manual states a bare
/// `-s` number means exactly that many bytes ("'20M' means 20 megabytes,
/// by default, it is the same as giving 20971520 as argument").
fn resolve_slice_size_string(
    conn: &Connection,
    config: &Config,
    unit: &models::Unit,
    resolved_bytes: i64,
) -> String {
    // Layer 1 (highest priority): the unit dotfile's own [policy] slice_size.
    //
    // Still read as raw TOML here, but NOT for the reason this comment gave
    // until 2026-09-17 (issue #212 residual). It claimed `slice_size` "isn't
    // part of the structured `UnitDotfile`/`PolicySection` model" — it is,
    // since #212 added it so `unit rename`'s read/write round trip stopped
    // silently dropping it. The raw read is now merely redundant, not
    // required.
    //
    // It is also harmless: `policy::resolve` runs first (see the caller) and
    // its `?` means a dotfile with a bad `[policy]` never reaches here at all
    // (#211) — including, since issue #263, a misspelled TOP-LEVEL table
    // name (`[polcy]` instead of `[policy]`), which `policy::resolve` now
    // refuses by name too rather than letting it look like an absent
    // `[policy]` section. Left as-is rather than rewired, because
    // `slice_arg_for_dar`'s doc explains at length why this function hands
    // dar the operator's RAW string, and changing how the string is
    // obtained is a different, riskier change than correcting a comment: it
    // moves real on-tape slice boundaries for every unit.
    if let Some(ref path) = unit.current_path {
        let dotfile_path = Path::new(path).join(".tapectl-unit.toml");
        if let Ok(contents) = fs::read_to_string(&dotfile_path) {
            if let Ok(toml) = contents.parse::<toml::Table>() {
                if let Some(v) = toml
                    .get("policy")
                    .and_then(|p| p.as_table())
                    .and_then(|p| p.get("slice_size"))
                    .and_then(|v| v.as_str())
                {
                    return v.to_string();
                }
            }
        }
    }

    // Layer 2: archive_set. Byte-only column — the resolved byte count
    // (already computed by `policy::resolve`) is the only faithful string.
    if let Some(as_id) = unit.archive_set_id {
        let has_override: bool = conn
            .query_row(
                "SELECT slice_size IS NOT NULL FROM archive_sets WHERE id = ?1",
                params![as_id],
                |row| row.get(0),
            )
            .unwrap_or(false);
        if has_override {
            return resolved_bytes.to_string();
        }
    }

    // Layer 3: system default — unchanged from before issue #47: dar
    // receives the config string verbatim, never round-tripped through
    // `parse_size_to_bytes`.
    config.defaults.slice_size.clone()
}

fn get_snapshot(conn: &Connection, id: i64) -> Result<models::Snapshot> {
    conn.query_row(
        "SELECT id, unit_id, version, snapshot_type, base_snapshot_id, status,
                source_path, total_size, file_count, created_at, superseded_at, notes
         FROM snapshots WHERE id = ?1",
        params![id],
        |row| {
            Ok(models::Snapshot {
                id: row.get(0)?,
                unit_id: row.get(1)?,
                version: row.get(2)?,
                snapshot_type: row.get(3)?,
                base_snapshot_id: row.get(4)?,
                status: row.get(5)?,
                source_path: row.get(6)?,
                total_size: row.get(7)?,
                file_count: row.get(8)?,
                created_at: row.get(9)?,
                superseded_at: row.get(10)?,
                notes: row.get(11)?,
            })
        },
    )
    .map_err(|_| TapectlError::Other(format!("snapshot {id} not found")))
}

fn get_unit_for_snapshot(conn: &Connection, snapshot: &models::Snapshot) -> Result<models::Unit> {
    queries::get_unit_by_name(conn, &{
        let name: String = conn.query_row(
            "SELECT name FROM units WHERE id = ?1",
            params![snapshot.unit_id],
            |row| row.get(0),
        )?;
        name
    })?
    .ok_or_else(|| TapectlError::Other("unit not found".into()))
}

/// An io failure on a staging path, as an error that names the operation
/// and the path (issue #354).
///
/// A bare `?` on an `io::Error` becomes `TapectlError::Io`, whose `#[from]`
/// makes the io error its `source()` as well as its Display — and
/// `error::exit_with_error` prints the whole chain (`{:#}`), so the operator
/// saw `Permission denied (os error 13): Permission denied (os error 13)`:
/// the same text twice and no path. `Other` carries no source, so the io
/// text appears exactly once, after the path it happened to.
fn staging_io_error(operation: &str, path: &Path, e: std::io::Error) -> TapectlError {
    TapectlError::Other(format!("{operation} {}: {e}", path.display()))
}

/// Make sure `stage create` can use `staging_dir` at all: create it if it is
/// missing, then prove it is writable by creating (and at once removing) a
/// file in it (issue #354).
///
/// Writability is tested by doing the write, the way `config check`'s
/// `policy::depth_check::check_staging` does, because mode bits lie under
/// root, ACLs and read-only mounts. Without the probe an unwritable
/// directory passed every check here and failed inside dar — after the
/// whole sha256 validation pass over the source.
fn prepare_staging_dir(staging_dir: &Path) -> Result<()> {
    fs::create_dir_all(staging_dir)
        .map_err(|e| staging_io_error("cannot create staging directory", staging_dir, e))?;
    let probe = staging_dir.join(format!(".tapectl-stage-probe-{}", std::process::id()));
    fs::write(&probe, b"tapectl stage create probe")
        .map_err(|e| staging_io_error("cannot write to staging directory", staging_dir, e))?;
    let _ = fs::remove_file(&probe);
    Ok(())
}

/// dar's per-entry overhead in an archive: each entry's inline header plus
/// its record in the catalog dar appends at the end. Measured at ~290 bytes
/// per file on dar 2.7.13 (2,001 files, `compression = none`); rounded up,
/// because this only ever widens the "may not fit" band, never a refusal.
const DAR_ENTRY_OVERHEAD_BYTES: i64 = 1024;

/// What the staging-space check needs to know about the stage it guards.
struct StagingSpaceInputs<'a> {
    staging_dir: &'a Path,
    unit_name: &'a str,
    snapshot: &'a models::Snapshot,
    /// The resolved slice size, in bytes.
    slice_size: i64,
    /// The resolved dar compression (`"none"` or an algorithm).
    compression: &'a str,
    /// Tier-2 consent given in advance (the global `--yes`) for a stage
    /// that may not fit (issue #354).
    assume_yes: bool,
}

impl StagingSpaceInputs<'_> {
    /// Staging's peak for a dar archive of `archive` bytes. dar writes every
    /// slice first; then the encryption loop writes ONE slice's `.age`
    /// beside its plaintext and deletes the plaintext before the next. So
    /// staging peaks at the archive plus one encrypted slice — about 1x the
    /// unit, not the 3x this check used to warn at (which would refuse a
    /// staging directory sized, as `first-run.sh` tells the operator to size
    /// it, for one tape's worth).
    fn peak(&self, archive: i64) -> i64 {
        archive.saturating_add(self.slice_size.min(archive))
    }

    /// The most the stage is expected to need with `compression = none`,
    /// as near as it can be told without reading the files: the snapshot's
    /// recorded size — the sum of its regular files' apparent sizes — plus
    /// [`DAR_ENTRY_OVERHEAD_BYTES`] a file, at its peak.
    fn upper(&self) -> i64 {
        let files = self.snapshot.file_count.unwrap_or(0);
        self.peak(
            self.snapshot
                .total_size
                .unwrap_or(0)
                .saturating_add(files.saturating_mul(DAR_ENTRY_OVERHEAD_BYTES)),
        )
    }

    fn free(&self, notices: &mut dyn Write) -> Option<i64> {
        match staging_free_bytes(self.staging_dir) {
            Ok(free) => Some(i64::try_from(free).unwrap_or(i64::MAX)),
            Err(e) => {
                let _ = writeln!(
                    notices,
                    "warning: could not read the free space of staging directory {} ({e}); \
                     staging unit \"{}\" without a space check",
                    self.staging_dir.display(),
                    self.unit_name
                );
                None
            }
        }
    }
}

/// The half of the staging-space check that runs before the source is read
/// (issue #354). Returns the upper bound when the check must be finished by
/// [`check_staging_space`] once the read has measured the unit, and `None`
/// when there is nothing more to decide.
///
/// - free space covers the upper bound: nothing to say;
/// - any compression but `none`: dar's output has no lower bound (text can
///   compress to a sliver), so whether it fits cannot be known — the
///   operator is ASKED, with the uncompressed figure ([`ask_to_stage_anyway`]:
///   ADR-0008 Tier 2; `--yes` proceeds, a non-interactive run without it
///   refuses). Never a hard refusal: the need may well fit;
/// - `compression = none`: wait for the read.
fn staging_space_before_reading(
    inputs: &StagingSpaceInputs,
    notices: &mut dyn Write,
) -> Result<Option<i64>> {
    use crate::util::format_bytes_binary as fmt;

    let Some(free) = inputs.free(notices) else {
        return Ok(None);
    };
    let upper = inputs.upper();
    if free >= upper {
        return Ok(None);
    }
    if inputs.compression == "none" {
        return Ok(Some(upper));
    }
    ask_to_stage_anyway(
        inputs,
        format!(
            "staging directory {} may be too small for unit \"{}\": {} free, and staging it \
             needs up to {} if its data does not compress (compression = \"{}\") — the dar \
             archive plus one encrypted slice beside it; {STAGING_RUNS_OUT}",
            inputs.staging_dir.display(),
            inputs.unit_name,
            fmt(free),
            fmt(upper),
            inputs.compression,
        ),
        notices,
    )?;
    Ok(None)
}

/// Ask whether to stage a unit that may not fit (issue #354, criterion
/// (b): "refused, or asked about"). It is ADR-0008 Tier 2 — the need is
/// uncertain, not proven short, so the operator may knowingly go ahead —
/// and goes through the one consent gate, `cli::consent::confirm`: a
/// terminal is shown `fact` and asked; `--yes` proceeds; a non-interactive
/// run without `--yes` refuses, carrying `fact`. `--yes` does not hide the
/// figures: they are written to `notices` before the stage goes on.
fn ask_to_stage_anyway(
    inputs: &StagingSpaceInputs,
    fact: String,
    notices: &mut dyn Write,
) -> Result<()> {
    if inputs.assume_yes {
        let _ = writeln!(notices, "warning: {fact} — staging anyway (--yes given)");
    }
    crate::cli::consent::confirm(
        &format!("stage unit \"{}\"", inputs.unit_name),
        &[fact],
        inputs.assume_yes,
    )
}

/// What happens to a stage that proceeds and then runs out of staging.
const STAGING_RUNS_OUT: &str =
    "if it runs out, dar fails partway and the partial slices are removed";

/// Refuse a stage that cannot fit in the staging directory, before dar
/// starts (issue #354) — and say so, with the numbers. Runs after the
/// sha256 pass, with `compression = none` and free space below `upper`
/// ([`staging_space_before_reading`]).
///
/// **Why only after the read.** Only a true lower bound on what dar stores
/// may refuse, since there is no way past a refusal. The snapshot's
/// recorded size is an UPPER bound, and so is a file's allocation on disk:
/// dar stores a hard-linked file once and turns runs of zeros into holes by
/// default (`--sparse-file-min-size` 15), so a dense file of written zeros
/// — a preallocated disk image, a fallocate'd database, a zero-padded ISO —
/// is fully allocated yet costs dar a few hundred bytes (measured: 16 MiB of
/// zeros, a 752-byte archive). What dar cannot shrink without compression
/// is the non-zero bytes, and counting them means reading every byte: the
/// sha256 pass does that anyway ([`validate::SourceValidation::nonzero_bytes`]).
///
/// - free space (read again: the pass can take hours) covers `upper`:
///   nothing to say;
/// - below the lower bound — the non-zero bytes, plus one slice of them —
///   the stage cannot fit: refused, whatever the flags (a fact, not a
///   risk);
/// - between the two: it may or may not fit, so the operator is asked, with
///   both figures ([`ask_to_stage_anyway`]).
///
/// Every byte counted in the lower bound is one dar must write, so a refusal
/// is never wrong about the archive dar would make with tapectl's flags. (A
/// `~/.darrc` that adds compression to every `dar -c` is outside what this
/// can see: tapectl does not pass dar `-N`.)
fn check_staging_space(
    inputs: &StagingSpaceInputs,
    upper: i64,
    nonzero_bytes: i64,
    notices: &mut dyn Write,
) -> Result<()> {
    use crate::util::format_bytes_binary as fmt;

    let dir = inputs.staging_dir.display();
    let unit = inputs.unit_name;
    let Some(free) = inputs.free(notices) else {
        return Ok(());
    };
    if free >= upper {
        return Ok(());
    }
    let lower = inputs.peak(nonzero_bytes);
    if free < lower {
        return Err(TapectlError::Other(format!(
            "not enough space in staging directory {dir} for unit \"{unit}\": staging it needs \
             at least {} — its dar archive (at least {}, the unit's non-zero bytes, every one \
             of which dar stores) plus one encrypted slice ({}) written beside it — and {} is \
             free. Free space there, or point [staging] directory at a larger filesystem.",
            fmt(lower),
            fmt(nonzero_bytes),
            fmt(inputs.slice_size.min(nonzero_bytes)),
            fmt(free),
        )));
    }
    ask_to_stage_anyway(
        inputs,
        format!(
            "staging directory {dir} may be too small for unit \"{unit}\": {} free, and \
             staging it needs between {} and {} (the low end counts only non-zero bytes: dar \
             stores runs of zeros as holes and a hard-linked file once); {STAGING_RUNS_OUT}",
            fmt(free),
            fmt(lower),
            fmt(upper),
        ),
        notices,
    )
}

#[cfg(test)]
thread_local! {
    /// Test-only: the free space `staging_free_bytes` reports on this
    /// thread, so the space check's refusal can be driven end to end
    /// without filling a real filesystem. `None` = ask the filesystem.
    static STAGING_FREE_OVERRIDE: std::cell::Cell<Option<u64>> =
        const { std::cell::Cell::new(None) };
}

/// Test-only: pretend the staging filesystem has `bytes` free, on this
/// test's thread, until dropped. Crate-visible so that a caller's own tests
/// (`cli::stage`, `collection::batch`, `cli::operations`'s quick-archive)
/// can drive the staging-space consent gate through that caller, proving the
/// global `--yes` reaches it (issue #354) — `stage_create` runs on the
/// calling thread, so the override is seen.
#[cfg(test)]
pub(crate) struct FreeSpaceOverride;
#[cfg(test)]
impl FreeSpaceOverride {
    pub(crate) fn set(bytes: u64) -> Self {
        STAGING_FREE_OVERRIDE.with(|c| c.set(Some(bytes)));
        Self
    }
}
#[cfg(test)]
impl Drop for FreeSpaceOverride {
    fn drop(&mut self) {
        STAGING_FREE_OVERRIDE.with(|c| c.set(None));
    }
}

/// Bytes an unprivileged process can still write under `dir`:
/// `f_bavail` (not `f_bfree`, which counts root's reserve) in units of
/// `f_frsize` (POSIX; `f_bsize` is only the preferred I/O size) — the same
/// arithmetic as `config check`'s staging-space line.
fn staging_free_bytes(dir: &Path) -> std::result::Result<u64, nix::Error> {
    #[cfg(test)]
    if let Some(free) = STAGING_FREE_OVERRIDE.with(|c| c.get()) {
        return Ok(free);
    }
    let stat = nix::sys::statvfs::statvfs(dir)?;
    Ok((stat.blocks_available() as u64).saturating_mul(stat.fragment_size()))
}

/// Build an `age::Encryptor` for the given recipient public keys — shared by
/// `encrypt_data` (small, buffered payloads: envelopes/manifests in
/// `src/volume/build.rs`) and `encrypt_file_streaming` (large, streamed
/// slices; H9 fix, issue #35), so the recipient parsing/boxing dance lives
/// in exactly one place. Pure extraction: same errors, same messages, same
/// order of operations as before — `age::Encryptor` doesn't retain any
/// reference into `pubkey_strings` or the intermediate boxed recipients
/// once constructed, so returning it by value is safe.
pub(crate) fn build_encryptor(pubkey_strings: &[String]) -> Result<age::Encryptor> {
    let recipients: Vec<age::x25519::Recipient> = pubkey_strings
        .iter()
        .map(|k| {
            k.parse::<age::x25519::Recipient>()
                .map_err(|e| TapectlError::Encryption(format!("invalid public key: {e}")))
        })
        .collect::<Result<Vec<_>>>()?;

    let recipient_refs: Vec<Box<dyn age::Recipient + Send>> = recipients
        .into_iter()
        .map(|r| Box::new(r) as Box<dyn age::Recipient + Send>)
        .collect();

    age::Encryptor::with_recipients(
        recipient_refs
            .iter()
            .map(|r| r.as_ref() as &dyn age::Recipient),
    )
    .map_err(|e| TapectlError::Encryption(format!("failed to create encryptor: {e}")))
}

/// Whole-buffer age encryption: holds the full plaintext AND full ciphertext
/// in RAM at once. Superseded on all production paths by
/// `encrypt_file_streaming` (slices, H9/#35) and `volume::build`'s streaming
/// envelope path (H9 residual, #87). Retained as a small, easy-to-audit
/// reference implementation for tests that want a one-shot encrypt/decrypt
/// round trip without standing up a file-backed streaming pipeline —
/// including several integration-test crates (`tests/tenant_isolation.rs`,
/// `tests/format_v2.rs`, `tests/failure_modes.rs`, `tests/integration.rs`)
/// that call it as `tapectl::staging::encrypt_data`.
///
/// NOT `#[cfg(test)]`-gated: `cfg(test)` only applies when this crate is
/// compiled in test mode for its own unit tests (`cargo test --lib`) — it
/// does not propagate to the integration-test binaries above, which link
/// the normally-built library. Gating this behind `cfg(test)` would make it
/// disappear for exactly those external callers and break the build; see
/// the issue #87 final report for the full account. The actual negative
/// control for the H9 residual this retirement was meant to guard against
/// is structural instead: `volume::build`'s envelope call sites
/// (`materialize_envelope_streaming`) no longer call this function at all,
/// so a regression toward whole-object envelope buffering would have to
/// reintroduce a call here, which is visible in review/diff even without a
/// compiler gate.
pub fn encrypt_data(data: &[u8], pubkey_strings: &[String]) -> Result<Vec<u8>> {
    let encryptor = build_encryptor(pubkey_strings)?;

    let mut encrypted = Vec::new();
    let mut writer = encryptor
        .wrap_output(&mut encrypted)
        .map_err(|e| TapectlError::Encryption(format!("wrap_output failed: {e}")))?;
    writer
        .write_all(data)
        .map_err(|e| TapectlError::Encryption(format!("write failed: {e}")))?;
    writer
        .finish()
        .map_err(|e| TapectlError::Encryption(format!("finish failed: {e}")))?;

    Ok(encrypted)
}

/// Fixed-size copy buffer for streaming slice encryption (H9 fix, issue
/// #35) — matches `volume::layout_model::hash_file`'s existing 128 KiB
/// streaming-hash convention. Peak RAM for `encrypt_file_streaming` is this
/// buffer, plus age's own constant ~64 KiB STREAM chunk buffer
/// (`age::primitives::stream::CHUNK_SIZE`) on both the plaintext and
/// ciphertext side, plus O(recipient count) for the header — never the
/// size of the file being encrypted.
const STREAM_COPY_BUFFER: usize = 128 * 1024;

/// Sizes and hashes recorded for one slice encrypted by
/// `encrypt_file_streaming` — the same four values `stage_create` used to
/// get from a `fs::read` + `encrypt_data` + `fs::write` sequence, now
/// produced without ever holding the whole slice in RAM.
pub struct EncryptedSliceInfo {
    pub plain_size: i64,
    pub sha256_plain: String,
    pub encrypted_size: i64,
    pub sha256_encrypted: String,
}

/// Stream-encrypt `input_path` straight to `output_path` as an age file,
/// hashing plaintext and ciphertext as each streams through — peak RAM is
/// `STREAM_COPY_BUFFER` plus age's own constant-size STREAM chunk buffer,
/// never the size of `input_path` (H9 fix, issue #35: the buffered
/// predecessor held the whole plaintext *and* the whole ciphertext in RAM
/// at once, which OOMs at the ratified 10G slice default —
/// `docs/design/v2-open-questions.md` §1.3 — on any machine with less than
/// ~20 GB free).
///
/// Neither `sha256_encrypted` nor `encrypted_size` — nor the raw ciphertext
/// bytes — are reproducible across separate calls with identical inputs:
/// `age::Encryptor` draws a fresh ephemeral key and nonce every time (see
/// `src/volume/build.rs`'s envelope-backup comment for the same fact,
/// confirmed empirically there), *and* every non-passphrase header carries
/// an extra randomly-shaped "grease" recipient stanza
/// (`age_core::format::grease_the_joint`) whose length also varies from
/// call to call. Only `sha256_plain` and `plain_size` are pure functions of
/// the plaintext and thus deterministic.
///
/// On any error, best-effort removes `output_path` rather than leaving a
/// partial `.age` file in staging (the buffered predecessor could never
/// produce one, since it only ever wrote after the full ciphertext existed
/// in memory) — this is not an integrity concern either way, since
/// `Layout::validate`'s `check_staged_slices` re-hashes from disk before
/// ever trusting a staged slice, and a retried `stage_create` truncates via
/// `File::create` regardless.
pub fn encrypt_file_streaming(
    input_path: &Path,
    output_path: &Path,
    pubkey_strings: &[String],
) -> Result<EncryptedSliceInfo> {
    let result = encrypt_file_streaming_inner(input_path, output_path, pubkey_strings);
    if result.is_err() {
        let _ = fs::remove_file(output_path);
    }
    // Issue #354: an io failure here (ENOSPC writing the `.age` into
    // staging is the likely one) names both files and the operation, once.
    result.map_err(|e| match e {
        TapectlError::Io(io) => TapectlError::Other(format!(
            "cannot encrypt {} to {}: {io}",
            input_path.display(),
            output_path.display()
        )),
        other => other,
    })
}

fn encrypt_file_streaming_inner(
    input_path: &Path,
    output_path: &Path,
    pubkey_strings: &[String],
) -> Result<EncryptedSliceInfo> {
    let encryptor = build_encryptor(pubkey_strings)?;
    let input = fs::File::open(input_path)?;
    let output = fs::File::create(output_path)?;
    encrypt_stream(encryptor, input, output)
}

/// The body of [`encrypt_file_streaming`] over any reader and writer — the
/// seam a test drives a writer that fails partway through.
fn encrypt_stream(
    encryptor: age::Encryptor,
    input: impl Read,
    output: impl Write,
) -> Result<EncryptedSliceInfo> {
    let mut reader = HashingReader::new(input);
    let hashing_output = HashingWriter::new(output);
    // Issue #354: both `wrap_output` (which writes the age header) and
    // `finish` (which writes the last chunk) fail only by failing to write,
    // so their errors stay io errors — `encrypt_file_streaming` then names
    // the slice and its `.age`. As `Encryption` strings they used to pass
    // through it pathless: an ENOSPC in staging read "encryption error:
    // finish failed: No space left on device (os error 28)".
    let mut writer = encryptor.wrap_output(hashing_output)?;

    let plain_size = stream_copy(&mut reader, &mut writer)?;
    let sha256_plain = reader.finalize_hex();

    // Mandatory: without this, the STREAM's final chunk (the one carrying
    // the "last chunk" flag) is never written, producing a file that
    // hashes fine but cannot be decrypted (age's own doc comment on
    // `StreamWriter::finish` says exactly this). Reached only once
    // `stream_copy` has streamed the *entire* plaintext without error — an
    // error above returns via `?` and never reaches this line, so a
    // partial stream is never finished into a falsely-valid file.
    let hashing_output = writer.finish()?;
    let sha256_encrypted = hashing_output.finalize_hex();
    let encrypted_size = hashing_output.bytes_written() as i64;

    Ok(EncryptedSliceInfo {
        plain_size: plain_size as i64,
        sha256_plain,
        encrypted_size,
        sha256_encrypted,
    })
}

/// Copy every byte from `reader` to `writer` through a fixed-size buffer —
/// never allocates more than `STREAM_COPY_BUFFER`, regardless of how much
/// data flows through. Returns the total bytes copied.
fn stream_copy<R: Read, W: Write>(reader: &mut R, writer: &mut W) -> Result<u64> {
    let mut buf = [0u8; STREAM_COPY_BUFFER];
    let mut total = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        writer.write_all(&buf[..n])?;
        total += n as u64;
    }
    Ok(total)
}

/// Establish the sha256 baseline for every `(path, hash)` pair — but ONLY
/// where `files`/`manifest_entries` don't already have one (issue #32/H6).
///
/// The `sha256 IS NULL` guard on both UPDATEs is the actual enforcement:
/// it makes this function safe to call on every `stage_create` (including
/// a re-stage of an already-baselined snapshot) regardless of what
/// `checksums` contains, rather than relying on the caller to have
/// filtered it first. `validate_source` already refuses to stage (BITROT)
/// before this point if a hash disagrees with an existing baseline, so in
/// practice every row here either has no baseline yet (this call
/// establishes it) or already matches (a no-op rewrite of the identical
/// value) — but the guard holds even if that invariant is ever violated by
/// a future caller.
fn backfill_checksums(
    conn: &Connection,
    snapshot_id: i64,
    checksums: &[(String, String)],
) -> Result<()> {
    let mut file_update = conn.prepare(
        "UPDATE files SET sha256 = ?1 WHERE snapshot_id = ?2 AND path = ?3 AND sha256 IS NULL",
    )?;
    let mut manifest_update = conn.prepare(
        "UPDATE manifest_entries SET sha256 = ?1
         WHERE manifest_id = (SELECT id FROM manifests WHERE snapshot_id = ?2 LIMIT 1)
         AND path = ?3 AND sha256 IS NULL",
    )?;

    for (path, hash) in checksums {
        file_update.execute(params![hash, snapshot_id, path])?;
        manifest_update.execute(params![hash, snapshot_id, path])?;
    }
    Ok(())
}

fn generate_receipt(
    conn: &Connection,
    stage_set_id: i64,
    unit: &models::Unit,
    snapshot: &models::Snapshot,
    tenant: &models::Tenant,
) -> Result<String> {
    let slices: Vec<(i64, i64, i64, String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT slice_number, size_bytes, encrypted_bytes, sha256_plain, sha256_encrypted
             FROM stage_slices WHERE stage_set_id = ?1 ORDER BY slice_number",
        )?;
        let rows = stmt.query_map(params![stage_set_id], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };

    let mut receipt = String::new();
    receipt.push_str("tapectl staging receipt\n");
    receipt.push_str("======================\n\n");
    receipt.push_str(&format!("Unit:     {} ({})\n", unit.name, unit.uuid));
    receipt.push_str(&format!("Tenant:   {}\n", tenant.name));
    receipt.push_str(&format!("Snapshot: v{}\n", snapshot.version));
    receipt.push_str(&format!("Stage:    {stage_set_id}\n"));
    receipt.push_str(&format!(
        "Date:     {}\n\n",
        chrono::Utc::now().to_rfc3339()
    ));
    receipt.push_str("Slices:\n");

    for (num, plain, enc, hash_p, hash_e) in &slices {
        receipt.push_str(&format!(
            "  #{num}: {plain} bytes -> {enc} bytes\n    plain:     {hash_p}\n    encrypted: {hash_e}\n",
        ));
    }

    Ok(receipt)
}

/// Parse an operator-facing size string (e.g. `"10G"`, `"500M"`, or a bare
/// byte count like `"1024"`) into a byte count.
///
/// A bare number with no suffix is valid and means bytes. Anything else that
/// doesn't parse cleanly — an unparseable number, an unrecognized suffix, a
/// negative value, or a magnitude too large to represent — is an ERROR
/// (issue #59): this value feeds `stage_sets.slice_size`, capacity math, and
/// bin-packing, so a silent fallback (0 bytes for a bad number; multiplier-1
/// for a bad suffix, e.g. `"2500GG"` silently becoming 2500 bytes) is worse
/// than a loud rejection.
pub fn parse_size_to_bytes(s: &str) -> Result<i64> {
    let trimmed = s.trim();
    let invalid = || {
        TapectlError::Config(format!(
            "{trimmed:?} is not a valid size (expected e.g. 2500G, 500M, or a bare byte count)"
        ))
    };

    let (num_str, suffix) = trimmed
        .find(|c: char| c.is_alphabetic())
        .map(|i| (&trimmed[..i], &trimmed[i..]))
        .unwrap_or((trimmed, ""));

    let num: f64 = num_str.parse().map_err(|_| invalid())?;
    if num.is_nan() || num < 0.0 {
        return Err(invalid());
    }

    let multiplier: f64 = match suffix.to_uppercase().as_str() {
        "" => 1.0,
        "K" | "KB" => 1024.0,
        "M" | "MB" => 1024.0 * 1024.0,
        "G" | "GB" => 1024.0 * 1024.0 * 1024.0,
        "T" | "TB" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return Err(invalid()),
    };

    let bytes = num * multiplier;
    if !bytes.is_finite() || bytes > i64::MAX as f64 {
        return Err(invalid());
    }
    Ok(bytes as i64)
}

#[cfg(test)]
mod parse_size_to_bytes_tests {
    //! Issue #59: `parse_size_to_bytes` used to silently paper over both an
    //! unparseable number (`.unwrap_or(0.0)` -> 0 bytes) and an unknown
    //! suffix (falls through to multiplier 1.0, so `"2500GG"` became 2500
    //! bytes instead of erroring). Both are now `Err`.
    use super::parse_size_to_bytes;

    #[test]
    fn bare_number_means_bytes() {
        assert_eq!(parse_size_to_bytes("1024").unwrap(), 1024);
        assert_eq!(parse_size_to_bytes("0").unwrap(), 0);
    }

    #[test]
    fn every_known_suffix_case_insensitive() {
        assert_eq!(parse_size_to_bytes("2K").unwrap(), 2 * 1024);
        assert_eq!(parse_size_to_bytes("2k").unwrap(), 2 * 1024);
        assert_eq!(parse_size_to_bytes("2KB").unwrap(), 2 * 1024);
        assert_eq!(parse_size_to_bytes("2kb").unwrap(), 2 * 1024);
        assert_eq!(parse_size_to_bytes("2M").unwrap(), 2 * 1024 * 1024);
        assert_eq!(parse_size_to_bytes("2MB").unwrap(), 2 * 1024 * 1024);
        assert_eq!(parse_size_to_bytes("2G").unwrap(), 2 * 1024 * 1024 * 1024);
        assert_eq!(parse_size_to_bytes("2GB").unwrap(), 2 * 1024 * 1024 * 1024);
        assert_eq!(
            parse_size_to_bytes("2T").unwrap(),
            2 * 1024 * 1024 * 1024 * 1024
        );
        assert_eq!(
            parse_size_to_bytes("2TB").unwrap(),
            2 * 1024 * 1024 * 1024 * 1024
        );
    }

    #[test]
    fn unknown_suffix_is_an_error() {
        let err = parse_size_to_bytes("2500GG").unwrap_err().to_string();
        assert!(err.contains("2500GG"), "error must name the value: {err}");
        assert!(
            err.contains("2500G") || err.to_lowercase().contains("valid size"),
            "error must show accepted forms: {err}"
        );

        assert!(parse_size_to_bytes("5X").is_err());
    }

    #[test]
    fn unknown_suffix_does_not_silently_become_bytes() {
        // The historic defect: "2500GG" silently became 2500 bytes via
        // multiplier 1.0. It must now be an error, never 2500.
        assert!(parse_size_to_bytes("2500GG").is_err());
    }

    #[test]
    fn unparseable_number_is_an_error() {
        assert!(parse_size_to_bytes("").is_err());
        assert!(parse_size_to_bytes("abc").is_err());
        assert!(parse_size_to_bytes("1.2.3").is_err());
    }

    #[test]
    fn negative_value_is_an_error() {
        assert!(parse_size_to_bytes("-5G").is_err());
        assert!(parse_size_to_bytes("-1").is_err());
    }

    #[test]
    fn absurd_magnitude_errors_rather_than_producing_garbage() {
        // Historically `(num * multiplier) as i64` on an out-of-range f64
        // silently saturates/truncates. Must error deliberately instead.
        assert!(parse_size_to_bytes("99999999999999999999G").is_err());
    }

    #[test]
    fn whitespace_is_trimmed() {
        assert_eq!(
            parse_size_to_bytes("  10G  ").unwrap(),
            10 * 1024 * 1024 * 1024
        );
    }
}

/// Walk a directory and collect manifest entries. `global_excludes` is
/// `config.defaults.global_excludes` (issue #49 item 5) — combined with the
/// unit's own dotfile `[excludes] patterns` (read internally, keyed off
/// `path`, exactly as before) via `exclude::effective_compiled`, the single
/// place both halves are merged so this walk and
/// `collection::fingerprint::walk_fingerprint` can never independently
/// disagree about the effective set.
fn walk_directory(
    path: &str,
    global_excludes: &[String],
) -> Result<(i64, i64, Vec<ManifestEntry>)> {
    use std::os::unix::fs::MetadataExt;
    use walkdir::WalkDir;

    let base = Path::new(path);
    // Issue #49 items 3+5: global excludes + the unit's own dotfile exclude
    // patterns, compiled once per walk (not per entry). A plain pattern never
    // matches a directory (dar's own `-X` rule); a directory pattern
    // (`name/`, issue #359) prunes the subtree below `name`, which dar
    // receives as `-P` masks (`exclude::dar_masks`, in `stage_create`).
    let exclude_compiled = exclude::effective_compiled(base, global_excludes)?;
    let mut entries = Vec::new();
    let mut total_size: i64 = 0;
    let mut file_count: i64 = 0;

    // Issue #359 (c): a directory INSIDE a pruned subtree is neither
    // recorded nor descended — dar's `-P` + `-D` store the pruned directory
    // itself, empty, and nothing below it. The pruned directory's own entry
    // passes (`excludes_dir_entry` keeps it) and its non-directory children
    // are dropped by `is_excluded` below. `file_type()` here is the lstat
    // type (`follow_links(false)`), the same fact `is_dir` is below.
    let walker = WalkDir::new(base)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            !(e.file_type().is_dir() && exclude_compiled.excludes_dir_entry(e.path()))
        });
    for entry in walker {
        let entry = entry.map_err(|e| TapectlError::Other(e.to_string()))?;
        let rel_path = entry
            .path()
            .strip_prefix(base)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .to_string();

        if rel_path.is_empty() {
            continue; // skip root
        }

        let meta = entry
            .metadata()
            .map_err(|e| TapectlError::Other(e.to_string()))?;
        let is_dir = meta.is_dir();

        // Issue #49: a non-directory entry matching an exclude pattern is
        // dropped before any further work (symlink-target read, manifest
        // row) — dar will never archive it (`stage_create` hands dar the
        // same patterns, as `-X` and `-P` masks via `exclude::dar_masks`),
        // so the manifest/files table must not record it either. Checked
        // before the file_type classification below so an excluded entry
        // costs nothing beyond the pattern match.
        if !is_dir && exclude::is_excluded(entry.path(), &exclude_compiled) {
            continue;
        }
        // lstat's own size for the entry — for a symlink this is the length
        // of the *target path string*, not any real content size. Kept
        // as-is (not zeroed for a symlink/special below): it's what
        // `collection::fingerprint`'s independent walk also computes from
        // the same never-follow metadata, and zeroing it here would make
        // that fingerprint comparison disagree with what's recorded,
        // falsely flagging every symlink-containing unit as perpetually
        // dirty. Only `total_size` below excludes it (see that comment).
        let size = if is_dir { 0 } else { meta.len() as i64 };
        let mtime = chrono::DateTime::from_timestamp(meta.mtime(), 0)
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_default();

        // Classify by filesystem type (issue #33/H7) via entry.file_type(),
        // which — under WalkDir::follow_links(false), already set above —
        // reflects symlink_metadata (never follows), matching the size/mtime
        // computation above. This is the fact the validator
        // (staging/validate.rs) filters its content-validation set on: only
        // 'regular' files get a size check + sha256; symlinks and special
        // files (FIFO, socket, block/char device) are recorded here but
        // never opened or hashed.
        let ft = entry.file_type();
        let (file_type, link_target): (&'static str, Option<String>) = if is_dir {
            ("dir", None)
        } else if ft.is_symlink() {
            // Never followed: the target may not exist (a broken symlink
            // is still recorded, not an error) and reading the link
            // itself — unlike opening a FIFO — never risks blocking.
            let target = fs::read_link(entry.path()).map_err(|e| {
                TapectlError::Other(format!("cannot read symlink target: {rel_path} ({e})"))
            })?;
            ("symlink", Some(target.to_string_lossy().to_string()))
        } else if ft.is_file() {
            ("regular", None)
        } else {
            // FIFO, socket, block/char device: recorded (so restore and
            // `catalog ls` still see them) but never content-validated —
            // opening a FIFO with no writer via File::open blocks forever
            // with no timeout, and none of these has "content" to
            // checksum in the first place.
            tracing::warn!(
                path = %rel_path,
                "special file (FIFO/socket/device) recorded but not content-validated"
            );
            ("special", None)
        };

        if !is_dir {
            // Every non-directory entry counts toward file_count, same as
            // before this fix (dar will archive and catalog a symlink or
            // special file as a real entry even though it carries no
            // content payload).
            file_count += 1;
            // Only real content bytes count as archival payload — a
            // symlink's "size" (from lstat, above) is the length of its
            // target *string*, not payload, and a special file has no
            // payload at all (issue #33/H7).
            if file_type == "regular" {
                total_size += size;
            }
        }

        entries.push(ManifestEntry {
            path: rel_path,
            size,
            mtime,
            is_dir,
            file_type,
            link_target,
            mode: Some(meta.mode() as i64),
            uid: Some(meta.uid() as i64),
            gid: Some(meta.gid() as i64),
            username: None,
            groupname: None,
        });
    }

    Ok((total_size, file_count, entries))
}

/// Test-only cross-module seam (issue #49): the relative, non-directory
/// paths `walk_directory` enumerates for `path`, without exposing
/// `ManifestEntry`'s internal shape outside this module. Used by
/// `collection::fingerprint`'s anti-regression test proving `walk_directory`
/// and `walk_fingerprint` enumerate an identical relative-path set for the
/// same exclude configuration — the property that keeps the two
/// independent `WalkDir`-based walks from silently drifting apart again
/// (issues #33/#36/#48 each hit exactly this failure shape once already).
#[cfg(test)]
pub(crate) fn walk_directory_relative_paths_for_test(
    path: &str,
    global_excludes: &[String],
) -> Result<Vec<String>> {
    let (_, _, entries) = walk_directory(path, global_excludes)?;
    Ok(entries
        .into_iter()
        .filter(|e| !e.is_dir)
        .map(|e| e.path)
        .collect())
}

struct ManifestEntry {
    path: String,
    size: i64,
    mtime: String,
    is_dir: bool,
    file_type: &'static str,
    link_target: Option<String>,
    mode: Option<i64>,
    uid: Option<i64>,
    gid: Option<i64>,
    username: Option<String>,
    groupname: Option<String>,
}

#[cfg(test)]
mod tests {
    //! Tests for the H9 fix (issue #35): `encrypt_file_streaming` must
    //! behave equivalently to the old buffered `encrypt_data` +
    //! `fs::write` pair it replaces in `stage_create`'s slice loop, while
    //! never holding a whole slice's plaintext or ciphertext in RAM.
    //!
    //! Two nuances drive the assertions below, both about `age`, not about
    //! this crate's code:
    //!   - `age::Encryptor` draws a fresh ephemeral key and nonce on
    //!     *every* call (`protocol.rs`'s `Nonce::random()`/`new_file_key()`;
    //!     confirmed empirically too, and already assumed elsewhere in this
    //!     codebase — `src/volume/build.rs`'s operator-envelope backup
    //!     comment clones ciphertext bytes rather than re-encrypting,
    //!     precisely because re-encryption "would NOT reproduce the same
    //!     bytes").
    //!   - Less obviously: **ciphertext *length* is not deterministic
    //!     either.** Every non-passphrase `age` header gets an extra
    //!     "grease" recipient stanza with a randomly chosen tag, arg count,
    //!     and body length (`age-core::format::grease_the_joint`, "Keep the
    //!     joint well oiled!") — anti-fingerprinting padding, by design.
    //!     Measured empirically here: encrypting the same plaintext to the
    //!     same single recipient 8 times in a row produced ciphertexts
    //!     ranging 53739–53863 bytes, a ~124-byte spread. So neither
    //!     `sha256_encrypted` nor `encrypted_size` can be asserted equal
    //!     across the buffered and streaming paths (or across any two
    //!     calls at all) — only `sha256_plain` and `plain_size` are pure
    //!     functions of the plaintext and thus safe to compare cross-path.
    //!
    //! What must hold for the encrypted side instead is *self-consistency*:
    //! the recorded `sha256_encrypted`/`encrypted_size` equal an
    //! independent re-hash/re-measure of the bytes that actually landed on
    //! disk (exactly what `Layout::validate`'s `check_staged_slices` —
    //! sacred invariant 2 — recomputes via `hash_file` before ever trusting
    //! a slice), and the file actually decrypts back to the original
    //! plaintext.
    use super::*;
    use sha2::{Digest, Sha256};
    use tempfile::TempDir;

    fn direct_hash(data: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(data);
        format!("{:x}", h.finalize())
    }

    /// Register the permanent escrow recipient (ADR-0005) on a throwaway
    /// holder tenant. `stage_create` refuses without one (issue #115), so
    /// this is not fixture decoration — it is the state every real staging
    /// run requires. Public key only, exactly as production does: the secret
    /// half never touches the database. Mirrors `tests/mhvtl_e2e.rs`'s
    /// harness. Returns the escrow public key.
    fn register_test_escrow(conn: &Connection) -> String {
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('escrow-holder', 0, 'active')",
            [],
        )
        .unwrap();
        let holder_id = conn.last_insert_rowid();
        let kp = crate::crypto::keys::generate_keypair();
        queries::insert_escrow_key(
            conn,
            holder_id,
            "test-escrow",
            &kp.fingerprint,
            &kp.public_key,
            Some("test escrow recipient (ADR-0005)"),
        )
        .unwrap();
        kp.public_key
    }

    /// Real age decryption of a file on disk, mirroring the pattern used by
    /// `tests/failure_modes.rs`'s `decrypt_with` — this is what fails if a
    /// `StreamWriter` is ever left un-`finish()`ed (a truncated STREAM with
    /// no final chunk), which no hash-only check would catch.
    fn decrypt_file(path: &Path, secret_key: &str) -> Vec<u8> {
        let identity: age::x25519::Identity = secret_key.parse().unwrap();
        let ct_file = fs::File::open(path).unwrap();
        let decryptor = age::Decryptor::new(ct_file).unwrap();
        let mut reader = decryptor
            .decrypt(std::iter::once(&identity as &dyn age::Identity))
            .expect("decrypt must succeed — a missing .finish() truncates the STREAM");
        let mut out = Vec::new();
        reader.read_to_end(&mut out).unwrap();
        out
    }

    #[test]
    fn streaming_matches_buffered_on_plaintext_and_is_self_consistent_on_ciphertext() {
        let kp = crate::crypto::keys::generate_keypair();
        let pubkeys = vec![kp.public_key.clone()];

        let plaintext = b"some slice content, repeated to be a bit larger than one line \
                           so this isn't a degenerate single-byte case; "
            .repeat(500);

        // The old buffered primitive (still used elsewhere for small
        // envelope/manifest payloads) as a semantic reference: it must
        // decrypt to the same plaintext as the streaming path, even though
        // — see the module doc comment — its ciphertext bytes and even
        // length are never reproducible across calls, buffered or
        // streaming, so there is nothing byte-level to compare it against.
        let buffered_identity: age::x25519::Identity = kp.secret_key.parse().unwrap();
        let buffered_ct = encrypt_data(&plaintext, &pubkeys).unwrap();
        let buffered_decryptor = age::Decryptor::new(&buffered_ct[..]).unwrap();
        let mut buffered_reader = buffered_decryptor
            .decrypt(std::iter::once(&buffered_identity as &dyn age::Identity))
            .unwrap();
        let mut buffered_plaintext = Vec::new();
        buffered_reader
            .read_to_end(&mut buffered_plaintext)
            .unwrap();
        assert_eq!(buffered_plaintext, plaintext);

        let expected_sha_plain = direct_hash(&plaintext);

        let tmp = TempDir::new().unwrap();
        let input_path = tmp.path().join("slice.1.dar");
        let output_path = tmp.path().join("slice.1.dar.age");
        fs::write(&input_path, &plaintext).unwrap();

        let info = encrypt_file_streaming(&input_path, &output_path, &pubkeys).unwrap();

        // Deterministic — must match the buffered path exactly (pure
        // functions of the plaintext bytes, no randomness involved).
        assert_eq!(info.plain_size, plaintext.len() as i64);
        assert_eq!(info.sha256_plain, expected_sha_plain);

        // NOT comparable cross-path (age's per-call ephemeral key/nonce
        // plus its randomized "grease" stanza mean neither the ciphertext
        // bytes nor even its length are reproducible — see the module doc
        // comment). Self-consistency instead: the recorded hash/size must
        // match the bytes that actually landed on disk, which is what
        // `Layout::validate`'s `check_staged_slices` independently
        // recomputes via `hash_file` before ever trusting a slice.
        let on_disk = fs::read(&output_path).unwrap();
        assert_eq!(on_disk.len() as i64, info.encrypted_size);
        assert_eq!(info.sha256_encrypted, direct_hash(&on_disk));

        // And it must actually decrypt back to the original plaintext.
        assert_eq!(decrypt_file(&output_path, &kp.secret_key), plaintext);
    }

    #[test]
    fn streamed_age_file_round_trips_through_real_decryption() {
        let kp = crate::crypto::keys::generate_keypair();
        let pubkeys = vec![kp.public_key.clone()];
        // Several times age's own 64 KiB STREAM chunk size, so this
        // exercises more than one chunk boundary, not just a toy example.
        let plaintext = b"round-trip content, needs to exceed one age STREAM chunk to \
                           prove multi-chunk streaming, not just a single-write toy case. "
            .repeat(2000);

        let tmp = TempDir::new().unwrap();
        let input_path = tmp.path().join("slice.dar");
        let output_path = tmp.path().join("slice.dar.age");
        fs::write(&input_path, &plaintext).unwrap();

        let info = encrypt_file_streaming(&input_path, &output_path, &pubkeys).unwrap();
        assert_eq!(info.plain_size, plaintext.len() as i64);
        assert_eq!(info.sha256_plain, direct_hash(&plaintext));

        assert_eq!(decrypt_file(&output_path, &kp.secret_key), plaintext);
    }

    #[test]
    fn copy_buffer_is_a_small_fixed_constant_independent_of_input_length() {
        // Structural guarantee behind the constant-memory claim: the copy
        // loop's only per-iteration allocation is this stack buffer, sized
        // once, never resized — so peak RAM for `encrypt_file_streaming`
        // cannot scale with the size of the file being encrypted (H9,
        // issue #35). Matches `volume::layout_model::hash_file`'s existing
        // 128 KiB streaming-hash convention in this codebase. Pinning the
        // exact value here (rather than just an upper bound) means any
        // future change to it is a deliberate, visible edit to this test,
        // not a silent drift back toward whole-slice buffering.
        assert_eq!(STREAM_COPY_BUFFER, 128 * 1024);
    }

    #[test]
    fn encrypts_an_input_many_times_larger_than_the_copy_buffer() {
        // ~16 MiB: >125x STREAM_COPY_BUFFER and >250x age's own 64 KiB
        // STREAM chunk — enough to force many loop iterations and many
        // STREAM chunks without materializing anything close to a real
        // 10G slice (keeps the ungated suite fast; do not stage a 10G file
        // in a unit test).
        let kp = crate::crypto::keys::generate_keypair();
        let pubkeys = vec![kp.public_key.clone()];

        let tmp = TempDir::new().unwrap();
        let input_path = tmp.path().join("big.dar");
        let output_path = tmp.path().join("big.dar.age");

        // Build the input by streaming chunks to disk (not one big Vec)
        // and hash as we go, so even test setup doesn't allocate a
        // slice-sized buffer.
        let mut f = fs::File::create(&input_path).unwrap();
        let mut expected_hasher = Sha256::new();
        let mut total_len: u64 = 0;
        for i in 0..256u64 {
            // Vary content per block so this isn't just N copies of one
            // block — a stronger check that hashing/streaming sees the
            // *whole* input in order, not just its first buffer's worth.
            let mut block = [0xABu8; 64 * 1024];
            block[0] = (i % 256) as u8;
            block[1] = ((i / 256) % 256) as u8;
            f.write_all(&block).unwrap();
            expected_hasher.update(block);
            total_len += block.len() as u64;
        }
        drop(f);
        let expected_sha_plain = format!("{:x}", expected_hasher.finalize());

        let info = encrypt_file_streaming(&input_path, &output_path, &pubkeys).unwrap();
        assert_eq!(info.plain_size, total_len as i64);
        assert_eq!(info.sha256_plain, expected_sha_plain);

        // Round-trip the whole thing back, streaming the comparison too.
        let identity: age::x25519::Identity = kp.secret_key.parse().unwrap();
        let ct_file = fs::File::open(&output_path).unwrap();
        let decryptor = age::Decryptor::new(ct_file).unwrap();
        let mut reader = decryptor
            .decrypt(std::iter::once(&identity as &dyn age::Identity))
            .unwrap();
        let mut actual_hasher = Sha256::new();
        let mut buf = [0u8; 64 * 1024];
        let mut decrypted_len: u64 = 0;
        loop {
            let n = reader.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            actual_hasher.update(&buf[..n]);
            decrypted_len += n as u64;
        }
        assert_eq!(decrypted_len, total_len);
        assert_eq!(
            format!("{:x}", actual_hasher.finalize()),
            expected_sha_plain
        );
    }

    #[test]
    fn stream_copy_surfaces_a_mid_stream_reader_error_instead_of_finishing() {
        // The precondition `encrypt_file_streaming` relies on to keep
        // `.finish()` from ever running on a partial stream: `stream_copy`
        // must propagate a reader error via `?` rather than treating a
        // failed read as EOF. A fixture `Read` impl that fails after its
        // first chunk (the same "injectable failure, not real fault
        // injection" style `MemStore`'s simulated ENOSPC already uses in
        // `src/store.rs` to make the tape ENOSPC abort path unit-testable
        // with no hardware) stands in for any real mid-file I/O error.
        struct FlakyReader {
            served: bool,
        }
        impl Read for FlakyReader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if !self.served {
                    self.served = true;
                    let n = buf.len().min(4096);
                    for b in &mut buf[..n] {
                        *b = 0x42;
                    }
                    Ok(n)
                } else {
                    Err(std::io::Error::other("simulated mid-stream I/O failure"))
                }
            }
        }

        let kp = crate::crypto::keys::generate_keypair();
        let pubkeys = vec![kp.public_key.clone()];
        let tmp = TempDir::new().unwrap();
        let output_path = tmp.path().join("partial.dar.age");

        let encryptor = build_encryptor(&pubkeys).unwrap();
        let hashing_output = HashingWriter::new(fs::File::create(&output_path).unwrap());
        let mut writer = encryptor.wrap_output(hashing_output).unwrap();
        let result = stream_copy(&mut FlakyReader { served: false }, &mut writer);

        assert!(
            result.is_err(),
            "flaky reader's error must propagate, not be swallowed"
        );
        // `writer` (and its `.finish()`) is never reached here, by
        // construction — `result` came back via `?` inside `stream_copy`
        // before control could ever return to a `.finish()` call site.
        // `encrypt_file_streaming` is structured the same way: `?` on
        // `stream_copy`'s result runs before the single `.finish()` call
        // in the function, so an error here always skips finish rather
        // than finishing a partial STREAM into a falsely-valid file.
    }

    #[test]
    fn a_bad_recipient_key_errors_before_any_output_file_is_created() {
        // Recipient parsing happens before the input is even opened, so a
        // malformed pubkey must fail cleanly with nothing written to
        // `output_path` — mirrors `encrypt_data`'s existing
        // `encrypt_rejects_malformed_pubkey` behavior (tests/failure_modes.rs)
        // for the streaming path.
        let tmp = TempDir::new().unwrap();
        let input_path = tmp.path().join("slice.dar");
        let output_path = tmp.path().join("slice.dar.age");
        fs::write(&input_path, b"irrelevant content").unwrap();

        let result =
            encrypt_file_streaming(&input_path, &output_path, &["not-an-age-key".to_string()]);

        assert!(result.is_err(), "malformed pubkey must error");
        assert!(
            !output_path.exists(),
            "no output file should be created before recipients are validated"
        );
    }

    // --- walk_directory: symlinks and special files (issue #33/H7) --------
    //
    // `walk_directory` used to record every non-directory entry as an
    // undifferentiated "file": for a symlink, `entry.metadata()` (never
    // follows — `WalkDir::follow_links(false)`) reports `size = len(target
    // string)`, not any real content size, and `is_dir = false`. The
    // validator (`staging::validate::check_source_size`) then compared that
    // recorded size against `std::fs::metadata`'s *followed* size — a
    // symlink whose target-string length differs from its target's content
    // size produced a false DIRTY (the mhvtl gate's exact fixture:
    // `target.txt` is 7 bytes, `link-ok`'s target string "target.txt" is 10
    // characters). The fix classifies each entry by filesystem type so the
    // validator can filter on that recorded fact instead of re-deriving
    // (and potentially re-disagreeing on) type information of its own.

    #[test]
    fn walk_directory_records_symlink_file_type_target_and_excludes_it_from_total_size() {
        // Reproduces the mhvtl gate's exact fixture shape: target.txt holds
        // 7 bytes of real content; link-ok's target-string "target.txt" is
        // 10 characters — deliberately different, so a bug that conflates
        // "symlink size" with "content size" shows up immediately in
        // total_size.
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("target.txt"), b"target\n").unwrap();
        std::os::unix::fs::symlink("target.txt", tmp.path().join("link-ok")).unwrap();

        let (total_size, file_count, entries) =
            walk_directory(tmp.path().to_str().unwrap(), &[]).unwrap();

        let link_entry = entries
            .iter()
            .find(|e| e.path == "link-ok")
            .expect("link-ok must be recorded in the manifest, not dropped");
        assert_eq!(link_entry.file_type, "symlink");
        assert_eq!(link_entry.link_target.as_deref(), Some("target.txt"));

        // Only target.txt's 7 real content bytes count as archival payload —
        // link-ok's target-string length (10) must never be added, whatever
        // its own recorded `size` field holds (that field stays lstat's raw
        // size — see the commit message for why it isn't zeroed).
        assert_eq!(
            total_size, 7,
            "a symlink's target-string length must not count as payload"
        );

        // Both non-directory entries count toward file_count — dar will
        // archive and catalog the symlink as a real entry even though it
        // carries no content payload (see commit message for the
        // file_count-vs-total_size rationale: this preserves today's
        // `if !is_dir { file_count += 1 }` behavior verbatim; only
        // total_size's accounting changes).
        assert_eq!(file_count, 2);
    }

    #[test]
    fn walk_directory_records_broken_symlink_without_error() {
        // A broken symlink (target does not exist) must still be walked
        // and recorded, never treated as an error — `fs::read_link` reads
        // the symlink's own stored target string and never requires the
        // target to exist.
        let tmp = TempDir::new().unwrap();
        std::os::unix::fs::symlink("does-not-exist.txt", tmp.path().join("dangling")).unwrap();

        let (_, _, entries) = walk_directory(tmp.path().to_str().unwrap(), &[]).unwrap();

        let entry = entries
            .iter()
            .find(|e| e.path == "dangling")
            .expect("a broken symlink must still be walked and recorded, not error out");
        assert_eq!(entry.file_type, "symlink");
        assert_eq!(entry.link_target.as_deref(), Some("does-not-exist.txt"));
    }

    #[test]
    fn walk_directory_classifies_a_fifo_as_special_and_excludes_it_from_total_size() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("regular.txt"), b"real content").unwrap();
        let fifo_path = tmp.path().join("a.fifo");
        nix::unistd::mkfifo(
            &fifo_path,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();

        let (total_size, file_count, entries) =
            walk_directory(tmp.path().to_str().unwrap(), &[]).unwrap();

        let fifo_entry = entries
            .iter()
            .find(|e| e.path == "a.fifo")
            .expect("the FIFO must still be recorded in the manifest, not dropped");
        assert_eq!(fifo_entry.file_type, "special");
        assert_eq!(fifo_entry.link_target, None);

        assert_eq!(
            total_size,
            "real content".len() as i64,
            "the FIFO must not contribute to total_size"
        );
        assert_eq!(file_count, 2, "the FIFO still counts toward file_count");
    }

    // ── issue #53: archive_base is per-stage-set, not per-snapshot ──

    #[test]
    fn archive_base_name_differs_for_two_stage_sets_of_the_same_snapshot() {
        // The core anti-collision claim: same unit uuid, same version, two
        // different stage_set_ids must never produce the same file-name
        // stem — otherwise a re-stage of the same snapshot would silently
        // overwrite the first stage set's `.age` slices.
        let a = archive_base_name("abcdef0123456789", 3, 7);
        let b = archive_base_name("abcdef0123456789", 3, 8);
        assert_ne!(a, b);
        assert_eq!(a, "abcdef012345_v3_s7");
        assert_eq!(b, "abcdef012345_v3_s8");
    }

    #[test]
    fn archive_base_prefix_is_the_name_plus_a_trailing_dot_and_stays_lockstep_with_cleanup() {
        // `cleanup_failed_stage_set` derives its plaintext-scan prefix via
        // this exact helper (not a parallel re-derivation) — this test
        // proves the shape stays what dar's `{base}.{N}.dar` naming needs,
        // and that the dot prevents `_s1` from prefix-matching `_s10.1.dar`.
        let p1 = archive_base_prefix("abcdef0123456789", 3, 1);
        let p10 = archive_base_prefix("abcdef0123456789", 3, 10);
        assert_eq!(p1, "abcdef012345_v3_s1.");
        assert_eq!(p10, "abcdef012345_v3_s10.");
        assert!(
            !"abcdef012345_v3_s10.1.dar".starts_with(&p1),
            "trailing dot must stop _s1 from prefix-matching _s10's files"
        );
    }

    // ── issue #47: stage_create must resolve policy, not read config.defaults directly ──

    /// Negative control / regression guard for issue #263's headline
    /// defect: `[excludes] pattern = [...]` (singular — the real key is
    /// `patterns`) parses cleanly today because `ExcludesSection` carries
    /// no `#[serde(deny_unknown_fields)]`, so `read_dotfile` silently
    /// returns `exclude_patterns: []`. `stage_create` then hands dar zero
    /// `-X` masks, and the material the operator meant to exclude is
    /// archived, encrypted, and written to write-once media — inside a
    /// valid sha256, so nothing downstream ever notices. This must now
    /// fail LOUDLY instead: the whole point is that silently including the
    /// material is unacceptable, so refusing the stage (not quietly
    /// excluding it after the fact) is the only safe outcome once the
    /// dotfile cannot be trusted.
    #[test]
    fn stage_create_refuses_a_dotfile_with_a_misspelled_excludes_key() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let paths = TapectlPaths::new(home);
        paths.ensure_dirs().unwrap();

        let conn = crate::db::open(&paths.db_file).unwrap();

        let staging_dir = tmp.path().join("staging");
        fs::create_dir_all(&staging_dir).unwrap();

        let mut config = Config::default();
        config.dar.binary = "dar".to_string();
        config.staging.directory = staging_dir.to_string_lossy().into_owned();

        crate::tenant::add_tenant(&conn, &paths, "op", None, true).unwrap();
        crate::tenant::add_tenant(&conn, &paths, "alice", None, false).unwrap();
        register_test_escrow(&conn);

        let src = tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("keep.txt"), b"keep me").unwrap();
        fs::write(src.join("secret.env"), b"do not archive me").unwrap();

        crate::unit::init_unit(
            &conn,
            &paths,
            src.to_str().unwrap(),
            "alice",
            Some("unit1"),
            &[],
            None,
        )
        .unwrap();

        // Hand-edit the dotfile the way an operator actually would — there
        // is no CLI for `[excludes] patterns` — replacing the correctly
        // spelled empty array `init_unit` wrote with the singular typo,
        // while leaving `[unit]` (uuid/created/tenant) untouched.
        let dotfile_path = src.join(".tapectl-unit.toml");
        let content = std::fs::read_to_string(&dotfile_path).unwrap();
        assert!(
            content.contains("patterns = []"),
            "test assumption: init_unit writes an empty patterns array, got: {content}"
        );
        let content = content.replace("patterns = []", "pattern = [\"*.env\"]");
        std::fs::write(&dotfile_path, content).unwrap();

        // `snapshot_create` walks the source directory too (`walk_directory`
        // -> `exclude::effective_compiled` -> `dotfile_patterns`, the same
        // shared helper `stage_create`'s dar `-X` masks use), so the fix
        // actually closes this gap even earlier than `stage_create` — at
        // manifest-build time. Either stage failing loudly satisfies this
        // test; the one thing that must NOT happen is the whole pipeline
        // reaching a successful stage_set with secret.env silently included.
        let result = snapshot_create(&conn, "unit1", &Config::default())
            .and_then(|snap_id| stage_create(&conn, &paths, &config, snap_id, false));

        assert!(
            result.is_err(),
            "a dotfile with a misspelled [excludes] key must be refused, not silently \
             yield zero excludes and archive secret.env onto write-once media (issue #263)"
        );
    }

    /// The core claim of issue #47: `stage_create` must resolve
    /// `slice_size` through `policy::resolve` (dotfile > archive_set >
    /// default), not read `config.defaults.slice_size` unconditionally.
    /// Exercises the REAL pipeline end to end (real `dar`, real tenant
    /// keys via `tenant::add_tenant`) so the assertion is against what
    /// actually lands in `stage_sets`, not a mocked shortcut.
    ///
    /// Also asserts on `compression`: prior to issue #92's fix,
    /// `unit::init_unit` always wrote a dotfile whose `[policy]` section
    /// carried a concrete `compression` value (see the design doc's own
    /// §2.2 example), and `policy::resolve`'s dotfile layer unconditionally
    /// outranks archive_set whenever a dotfile is present — so for every
    /// real, dotfile-backed unit, `compression` resolved to the dotfile's
    /// hardcoded value regardless of any archive_set override, making the
    /// archive set's `compression` structurally unreachable. Per the CTO's
    /// ratified fix ("Recast of v4.0 §2.2" in docs/design-errata.md,
    /// issue #92), dotfile policy fields are now `Option` and are omitted
    /// from newly-written dotfiles unless the operator sets them
    /// explicitly — so `init_unit`'s dotfile no longer shadows this
    /// archive_set field, and this test proves that end to end. Because the
    /// archive set names a real compressor, it also runs dar's `-z` codepath,
    /// which #92 made reachable for the first time.
    #[test]
    fn stage_create_uses_archive_set_resolved_slice_size_not_global_default() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let paths = TapectlPaths::new(home);
        paths.ensure_dirs().unwrap();

        let conn = crate::db::open(&paths.db_file).unwrap();

        let staging_dir = tmp.path().join("staging");
        fs::create_dir_all(&staging_dir).unwrap();

        let mut config = Config::default();
        config.dar.binary = "dar".to_string(); // PATH, not a hardcoded distro path (issue #43)
        config.staging.directory = staging_dir.to_string_lossy().into_owned();
        config.defaults.slice_size = "100M".to_string();
        // Leave the default at "none" and have the archive set override it to
        // a real compressor. That direction is the one that actually proves
        // the fix: pre-#92, `init_unit`'s dotfile hardcoded `compression =
        // "none"`, so an assertion expecting "none" would have passed for the
        // wrong reason. Expecting "gzip" can only succeed if the dotfile has
        // stopped shadowing. It also drives dar's real `-z` codepath.
        config.defaults.compression = "none".to_string();

        crate::tenant::add_tenant(&conn, &paths, "op", None, true).unwrap();
        crate::tenant::add_tenant(&conn, &paths, "alice", None, false).unwrap();
        // Issue #115: `stage_create` refuses without a registered escrow.
        register_test_escrow(&conn);

        // Archive set overriding both slice_size and compression away from
        // config.defaults' "100M"/"none" above.
        conn.execute(
            "INSERT INTO archive_sets (name, slice_size, compression) VALUES ('cold', ?1, ?2)",
            params![50i64 * 1024 * 1024, "gzip"],
        )
        .unwrap();

        let src = tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("f.txt"),
            b"hello world, this is stage_create resolver test content",
        )
        .unwrap();

        crate::unit::init_unit(
            &conn,
            &paths,
            src.to_str().unwrap(),
            "alice",
            Some("unit1"),
            &[],
            Some("cold"),
        )
        .unwrap();

        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        let stage_set_id = stage_create(&conn, &paths, &config, snap_id, false).unwrap();

        let slice_size: i64 = conn
            .query_row(
                "SELECT slice_size FROM stage_sets WHERE id = ?1",
                params![stage_set_id],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(
            slice_size,
            50 * 1024 * 1024,
            "stage_sets.slice_size must reflect the archive_set's override (50M), \
             not config.defaults.slice_size (100M) — proves stage_create resolves \
             policy instead of reading config.defaults directly"
        );

        let compression: String = conn
            .query_row(
                "SELECT compression FROM stage_sets WHERE id = ?1",
                params![stage_set_id],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(
            compression, "gzip",
            "stage_sets.compression must reflect the archive_set's override (gzip), \
             not config.defaults.compression (none), and must not be shadowed by \
             a concrete value baked into the unit's dotfile — proves the \
             archive_set's compression is actually reachable (issue #92)"
        );
    }

    /// End-to-end proof (real dar, real encryption) that two stage sets of
    /// the SAME snapshot never collide: their `.age` slice paths differ,
    /// and both sets of ciphertext survive on disk simultaneously —
    /// something the old per-snapshot `archive_base` could not guarantee.
    #[test]
    fn two_stage_sets_of_the_same_snapshot_do_not_collide_on_disk() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let paths = TapectlPaths::new(home);
        paths.ensure_dirs().unwrap();

        let conn = crate::db::open(&paths.db_file).unwrap();

        let staging_dir = tmp.path().join("staging");
        fs::create_dir_all(&staging_dir).unwrap();

        let mut config = Config::default();
        config.staging.directory = staging_dir.to_string_lossy().to_string();
        config.dar.binary = "dar".to_string();

        crate::tenant::add_tenant(&conn, &paths, "op", None, true).unwrap();
        crate::tenant::add_tenant(&conn, &paths, "alice", None, false).unwrap();
        // Issue #115: `stage_create` refuses without a registered escrow.
        register_test_escrow(&conn);

        let src = tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("f.txt"), b"collision-test content").unwrap();

        crate::unit::init_unit(
            &conn,
            &paths,
            src.to_str().unwrap(),
            "alice",
            Some("unit1"),
            &[],
            None,
        )
        .unwrap();

        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();

        // Two stage sets of the SAME snapshot — the flow issue #53 makes
        // reachable via `stage create --version`. Calling `stage_create`
        // directly here (bypassing the CLI gate) is deliberate: this test
        // is about `archive_base` collision, not about the gate.
        let stage_set_1 = stage_create(&conn, &paths, &config, snap_id, false).unwrap();
        let stage_set_2 = stage_create(&conn, &paths, &config, snap_id, false).unwrap();
        assert_ne!(stage_set_1, stage_set_2);

        let paths_for = |stage_set_id: i64| -> Vec<String> {
            conn.prepare("SELECT staging_path FROM stage_slices WHERE stage_set_id = ?1")
                .unwrap()
                .query_map(params![stage_set_id], |row| row.get(0))
                .unwrap()
                .collect::<std::result::Result<Vec<String>, _>>()
                .unwrap()
        };
        let slices_1 = paths_for(stage_set_1);
        let slices_2 = paths_for(stage_set_2);
        assert!(!slices_1.is_empty());
        assert!(!slices_2.is_empty());

        // Distinct paths, and both must still exist on disk — proof the
        // second stage set never overwrote the first's ciphertext.
        for p in slices_1.iter().chain(slices_2.iter()) {
            assert!(Path::new(p).exists(), "{p} must exist on disk");
        }
        let set1: std::collections::HashSet<_> = slices_1.iter().collect();
        let set2: std::collections::HashSet<_> = slices_2.iter().collect();
        assert!(
            set1.is_disjoint(&set2),
            "the two stage sets' slice paths must never overlap: {slices_1:?} vs {slices_2:?}"
        );
    }

    // ── issue #54: stage failure hygiene ──

    /// Proves the leak: a failure *after* `dar -c` has written every
    /// plaintext `.dar` slice but *before* the encryption loop starts — the
    /// worst case, where every slice is orphaned as plaintext. Before the
    /// change-2 fix, those `.dar` files are never cleaned up.
    ///
    /// The failure is injected at catalog extraction: `catalogs_dir` is a
    /// regular FILE, so creating the per-unit catalog directory under it
    /// fails (ENOTDIR — no permission bits involved, so it holds as root
    /// too). This test used to inject it with a tenant that had no active
    /// keys, until issue #354 moved that refusal before dar — where it can
    /// no longer orphan anything, which is the point of moving it.
    #[test]
    fn stage_create_failure_orphans_plaintext_dar_slices() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let mut paths = TapectlPaths::new(home);
        paths.ensure_dirs().unwrap();

        let conn = crate::db::open(&paths.db_file).unwrap();

        let staging_dir = tmp.path().join("staging");
        fs::create_dir_all(&staging_dir).unwrap();

        let mut config = Config::default();
        config.dar.binary = "dar".to_string(); // PATH, not a hardcoded distro path (issue #43)
        config.staging.directory = staging_dir.to_string_lossy().into_owned();

        crate::tenant::add_tenant(&conn, &paths, "op", None, true).unwrap();
        crate::tenant::add_tenant(&conn, &paths, "alice", None, false).unwrap();
        // Issue #115: `stage_create` refuses without a registered escrow.
        register_test_escrow(&conn);

        // The injected post-dar failure (see the doc comment).
        let catalogs_file = tmp.path().join("catalogs-is-a-file");
        fs::write(&catalogs_file, b"not a directory").unwrap();
        paths.catalogs_dir = catalogs_file;

        let src = tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("f.txt"),
            b"content that will be orphaned as plaintext",
        )
        .unwrap();

        crate::unit::init_unit(
            &conn,
            &paths,
            src.to_str().unwrap(),
            "alice",
            Some("unit1"),
            &[],
            None,
        )
        .unwrap();

        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        let result = stage_create(&conn, &paths, &config, snap_id, false);

        assert!(
            result.is_err(),
            "expected stage_create to fail creating the catalog directory"
        );

        // Positive control: the failure must have come AFTER dar, or there
        // were never any plaintext slices to leak and the assertion below
        // would pass vacuously. `dar_command` is recorded right after a
        // successful `dar -c`.
        let dar_ran: bool = conn
            .query_row(
                "SELECT dar_command IS NOT NULL FROM stage_sets WHERE snapshot_id = ?1",
                params![snap_id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            dar_ran,
            "the injected failure must land after dar -c, got: {result:?}"
        );

        let leaked: Vec<_> = fs::read_dir(&staging_dir)
            .unwrap()
            .flatten()
            .filter(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                name.ends_with(".dar") || name.ends_with(".sha512")
            })
            .collect();

        assert!(
            leaked.is_empty(),
            "expected the change-2 cleanup fix to have removed every orphaned \
             plaintext .dar/.sha512 file — found: {:?}",
            leaked.iter().map(|e| e.file_name()).collect::<Vec<_>>()
        );

        // The stage_sets row must survive the failure — that 'staging'
        // status is what recover_orphaned_sessions (src/db/mod.rs) sweeps
        // to 'failed' on the next db::open(), the operator's actual signal.
        let row_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM stage_sets WHERE snapshot_id = ?1",
                params![snap_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            row_count, 1,
            "stage_sets row must survive stage_create's failure so the startup \
             sweep can still mark it 'failed' — cleanup must never delete or \
             re-status it"
        );
    }

    /// Cleanup must not reach across snapshot versions of the same unit.
    ///
    /// `archive_base` is `{uuid12}_v{version}` and dar names slices
    /// `{base}.{N}.dar`, so the scan prefix must carry a trailing dot.
    /// Without it `..._v1` is a prefix of `..._v10.1.dar`, and a failed
    /// stage of version 1 deletes the in-flight plaintext of a concurrent
    /// stage of version 10 — silent cross-stage data destruction, the exact
    /// hazard the `.age` cleanup is deliberately DB-row-keyed to avoid.
    ///
    /// Drives `cleanup_failed_stage_set` directly against hand-placed decoy
    /// files, because provoking two concurrent real dar runs at versions 1
    /// and 10 is not worth the fixture cost to prove a string-prefix rule.
    #[test]
    fn cleanup_does_not_delete_a_higher_version_stage_of_the_same_unit() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let paths = TapectlPaths::new(home);
        paths.ensure_dirs().unwrap();
        let conn = crate::db::open(&paths.db_file).unwrap();

        let staging_dir = tmp.path().join("staging");
        fs::create_dir_all(&staging_dir).unwrap();
        let mut config = Config::default();
        config.staging.directory = staging_dir.to_string_lossy().into_owned();

        crate::tenant::add_tenant(&conn, &paths, "op", None, true).unwrap();
        let tid = crate::tenant::add_tenant(&conn, &paths, "alice", None, false).unwrap();

        // A unit whose uuid12 prefix we control, with snapshots at v1 and v10.
        let uuid = "aaaaaaaabbbbccccddddeeeeeeeeeeee";
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
             VALUES (?1, 'u', ?2, 'mtime_size', 1, 'active')",
            params![uuid, tid],
        )
        .unwrap();
        let unit_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, source_path, status)
             VALUES (?1, 1, '/src', 'created')",
            params![unit_id],
        )
        .unwrap();
        let snap_v1 = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, slice_size, compression, encrypted)
             VALUES (?1, 1024, 'none', 1)",
            params![snap_v1],
        )
        .unwrap();
        let failed_set = conn.last_insert_rowid();

        // A second stage set of the SAME snapshot (issue #53: two stage
        // sets can now coexist for one snapshot) whose files must survive
        // `failed_set`'s cleanup untouched.
        conn.execute(
            "INSERT INTO stage_sets (snapshot_id, slice_size, compression, encrypted)
             VALUES (?1, 1024, 'none', 1)",
            params![snap_v1],
        )
        .unwrap();
        let sibling_set = conn.last_insert_rowid();

        let base12 = &uuid[..12];
        let failed_slice = staging_dir.join(format!("{base12}_v1_s{failed_set}.1.dar"));
        let sibling_slice = staging_dir.join(format!("{base12}_v1_s{sibling_set}.1.dar"));
        let sibling_hash = staging_dir.join(format!("{base12}_v1_s{sibling_set}.1.dar.sha512"));
        for f in [&failed_slice, &sibling_slice, &sibling_hash] {
            fs::write(f, b"x").unwrap();
        }

        cleanup_failed_stage_set(&conn, &config, failed_set);

        assert!(
            !failed_slice.exists(),
            "the failed stage set's own orphaned plaintext slice must be removed"
        );
        assert!(
            sibling_slice.exists(),
            "a sibling stage set's in-flight plaintext slice must survive this \
             cleanup — a prefix without the stage_set_id/trailing-dot discipline \
             would have deleted it"
        );
        assert!(
            sibling_hash.exists(),
            "the sibling's hash file must survive this cleanup for the same reason"
        );
    }

    // ── issue #49: exclusions end-to-end (dotfile+global -> dar, walk, validation) ──

    /// Shared setup for the issue #49 tests below: a real tenant + unit
    /// (via `unit::init_unit`, so a real dotfile + real tenant keys exist —
    /// `stage_create` refuses to encrypt without active tenant keys), with
    /// the unit's dotfile `[excludes] patterns` overwritten to
    /// `exclude_patterns` (empty = the "no excludes configured" case,
    /// `init_unit` itself always writes an empty list). Returns
    /// `(conn, paths, config, src_dir)`; the caller writes fixture files
    /// into `src_dir` and drives `snapshot_create`/`stage_create` itself,
    /// since each test needs different file content/timing.
    fn setup_unit_with_excludes(
        tmp: &TempDir,
        exclude_patterns: Vec<String>,
    ) -> (Connection, TapectlPaths, Config, PathBuf) {
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let paths = TapectlPaths::new(home);
        paths.ensure_dirs().unwrap();
        let conn = crate::db::open(&paths.db_file).unwrap();

        let staging_dir = tmp.path().join("staging");
        fs::create_dir_all(&staging_dir).unwrap();

        let mut config = Config::default();
        config.dar.binary = "dar".to_string(); // PATH, not a hardcoded distro path (issue #43)
        config.staging.directory = staging_dir.to_string_lossy().into_owned();

        crate::tenant::add_tenant(&conn, &paths, "op", None, true).unwrap();
        crate::tenant::add_tenant(&conn, &paths, "alice", None, false).unwrap();

        let src = tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();

        crate::unit::init_unit(
            &conn,
            &paths,
            src.to_str().unwrap(),
            "alice",
            Some("unit1"),
            &[],
            None,
        )
        .unwrap();

        if !exclude_patterns.is_empty() {
            let dotfile_path = src.join(".tapectl-unit.toml");
            let mut df = crate::unit::dotfile::read_dotfile(&dotfile_path).unwrap();
            df.exclude_patterns = exclude_patterns;
            crate::unit::dotfile::write_dotfile(&dotfile_path, &df).unwrap();
        }

        // Issue #115: `stage_create` refuses without one.
        let _escrow_pk = register_test_escrow(&conn);

        (conn, paths, config, src)
    }

    /// Issue #115 / ADR-0005. Staging without a registered escrow recipient
    /// silently produces slices the escrow key can never open — the whole
    /// point of the escrow line — and `volume write` now refuses to put them
    /// on tape. So the refusal belongs here, before dar and age burn hours
    /// on material that cannot be written, not at the drive afterwards.
    ///
    /// The two halves that matter are both about *when*: no `stage_sets` row
    /// may be left behind, and dar must never start. `dar.binary` is pointed
    /// at a path that cannot exist, so reaching dar at all would produce a
    /// visibly different error.
    #[test]
    fn stage_create_refuses_when_no_escrow_recipient_is_registered() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
        // The shared fixture registers an escrow, because that is now the
        // precondition every other staging test needs. This is the one test
        // that must run without one.
        conn.execute("DELETE FROM encryption_keys WHERE is_escrow = 1", [])
            .unwrap();
        config.dar.binary = "/nonexistent/dar-must-never-run".to_string();

        fs::write(src.join("f.txt"), b"content that must never reach dar").unwrap();
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();

        let before: i64 = conn
            .query_row("SELECT COUNT(*) FROM stage_sets", [], |r| r.get(0))
            .unwrap();

        let err = stage_create(&conn, &paths, &config, snap_id, false).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("no escrow recipient is registered"),
            "expected the escrow refusal, got: {msg}"
        );
        assert!(
            msg.contains("key generate --escrow") && msg.contains("key import --escrow"),
            "the refusal must name both ways to register one: {msg}"
        );
        assert!(
            !msg.contains("dar-must-never-run"),
            "the refusal must precede the dar run, not follow it: {msg}"
        );

        let after: i64 = conn
            .query_row("SELECT COUNT(*) FROM stage_sets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            after, before,
            "the refusal must precede the stage_sets INSERT — no orphan row \
             for the startup sweep to find and mark 'failed'"
        );

        let left_in_staging: Vec<String> = fs::read_dir(&config.staging.directory)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            left_in_staging.is_empty(),
            "nothing may reach the staging directory before the refusal: {left_in_staging:?}"
        );
    }

    /// Issue #354 (c): a tenant with no active keys is refused BEFORE dar,
    /// the same way the escrow refusal above is. It used to be checked at
    /// the top of the encryption loop — after `dar -c` had archived the
    /// whole unit and the catalog had been extracted — so a large unit
    /// spent its entire dar run to reach a refusal that needed nothing but
    /// a database read.
    ///
    /// Same two "when" halves as the escrow test: dar never starts (its
    /// binary is a path that cannot exist, so reaching it would change the
    /// error), and no `stage_sets` row is left for the startup sweep.
    #[test]
    fn stage_create_refuses_a_tenant_with_no_active_keys_before_dar_runs() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
        conn.execute(
            "UPDATE encryption_keys SET is_active = 0
             WHERE tenant_id = (SELECT id FROM tenants WHERE name = 'alice')",
            [],
        )
        .unwrap();
        config.dar.binary = "/nonexistent/dar-must-never-run".to_string();

        fs::write(src.join("f.txt"), b"content that must never reach dar").unwrap();
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();

        let before: i64 = conn
            .query_row("SELECT COUNT(*) FROM stage_sets", [], |r| r.get(0))
            .unwrap();

        let err = stage_create(&conn, &paths, &config, snap_id, false).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("has no active keys"),
            "expected the no-active-keys refusal, got: {msg}"
        );
        assert!(
            !msg.contains("dar-must-never-run"),
            "the refusal must precede the dar run, not follow it: {msg}"
        );

        let after: i64 = conn
            .query_row("SELECT COUNT(*) FROM stage_sets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            after, before,
            "the refusal must precede the stage_sets INSERT — no orphan row \
             for the startup sweep to find and mark 'failed'"
        );

        let left_in_staging: Vec<String> = fs::read_dir(&config.staging.directory)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            left_in_staging.is_empty(),
            "nothing may reach the staging directory before the refusal: {left_in_staging:?}"
        );
    }

    /// Render an error exactly as `error::exit_with_error` does for the
    /// operator: `{:#}` on the anyhow error, which walks the `source()`
    /// chain. Asserting on `err.to_string()` alone would never see issue
    /// #354's doubled io text — that is the outer Display only.
    fn as_operator_sees_it(err: TapectlError) -> String {
        format!("{:#}", anyhow::Error::from(err))
    }

    /// How many times an io error's `(os error N)` text appears — once is
    /// right; twice is the `TapectlError::Io` chain-doubling of issue #354.
    fn os_error_mentions(msg: &str) -> usize {
        msg.matches("(os error").count()
    }

    /// Issue #354 (a): a staging directory that cannot be created is named,
    /// with the operation, and the io text is printed once. It used to reach
    /// the operator as `error: Not a directory (os error 20): Not a directory
    /// (os error 20)` — twice, and with no path.
    ///
    /// The parent is a regular file (ENOTDIR), so no permission bits are
    /// involved and this holds under root too.
    #[test]
    fn an_uncreatable_staging_directory_is_named_with_the_operation() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
        let not_a_dir = tmp.path().join("a-regular-file");
        fs::write(&not_a_dir, b"x").unwrap();
        let staging = not_a_dir.join("staging");
        config.staging.directory = staging.to_string_lossy().into_owned();
        config.dar.binary = "/nonexistent/dar-must-never-run".to_string();

        fs::write(src.join("f.txt"), b"content").unwrap();
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();

        let err = stage_create(&conn, &paths, &config, snap_id, false).unwrap_err();
        let msg = as_operator_sees_it(err);
        assert!(
            msg.contains(&*staging.to_string_lossy()),
            "the error must name the staging directory: {msg}"
        );
        assert!(
            msg.contains("cannot create staging directory"),
            "the error must name the operation: {msg}"
        );
        assert_eq!(
            os_error_mentions(&msg),
            1,
            "the io error must be printed once, not doubled: {msg}"
        );
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM stage_sets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0, "refused before the stage_sets INSERT");
    }

    /// Issue #354 (a): a staging directory that exists but cannot be written
    /// is refused before anything else touches it — named, with the
    /// operation. It used to pass every check, spend the whole sha256
    /// validation pass over the source, and only then fail inside dar.
    ///
    /// Permission bits do not bind root, so the test skips there (the
    /// ENOTDIR test above is the root-safe half of this behaviour).
    #[test]
    fn an_unwritable_staging_directory_is_refused_before_dar_naming_it() {
        use std::os::unix::fs::PermissionsExt;
        if nix::unistd::geteuid().is_root() {
            eprintln!("skipping: permission bits do not bind root");
            return;
        }
        let tmp = TempDir::new().unwrap();
        let (conn, paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
        let staging = PathBuf::from(&config.staging.directory);
        fs::set_permissions(&staging, fs::Permissions::from_mode(0o500)).unwrap();
        config.dar.binary = "/nonexistent/dar-must-never-run".to_string();

        fs::write(src.join("f.txt"), b"content").unwrap();
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();

        let result = stage_create(&conn, &paths, &config, snap_id, false);
        fs::set_permissions(&staging, fs::Permissions::from_mode(0o700)).unwrap();
        let msg = as_operator_sees_it(result.unwrap_err());
        assert!(
            msg.contains("cannot write to staging directory")
                && msg.contains(&*staging.to_string_lossy()),
            "the error must name the operation and the directory: {msg}"
        );
        assert!(
            !msg.contains("dar-must-never-run"),
            "the refusal must precede the dar run: {msg}"
        );
        assert_eq!(os_error_mentions(&msg), 1, "printed once: {msg}");
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM stage_sets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0, "refused before the stage_sets INSERT");
    }

    /// Issue #354 (a), the encryption half: the `.age` file is written into
    /// the staging directory, and a failure there (ENOSPC is the likely one
    /// in practice) must name both slice paths and the operation, once.
    #[test]
    fn a_slice_encryption_io_failure_names_the_paths_once() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("slice.1.dar");
        fs::write(&input, b"plaintext slice").unwrap();
        let output = tmp.path().join("no-such-dir").join("slice.1.dar.age");
        let kp = crate::crypto::keys::generate_keypair();

        let Err(err) = encrypt_file_streaming(&input, &output, &[kp.public_key]) else {
            panic!("encrypting into a directory that does not exist must fail");
        };
        let msg = as_operator_sees_it(err);
        assert!(
            msg.contains(&*output.to_string_lossy()) && msg.contains(&*input.to_string_lossy()),
            "the error must name the slice and its .age: {msg}"
        );
        assert!(msg.contains("cannot encrypt"), "names the operation: {msg}");
        assert_eq!(os_error_mentions(&msg), 1, "printed once: {msg}");
    }

    /// Issue #354 (a): the staging filesystem filling up WHILE the `.age` is
    /// written — not at `File::create` — must name the paths too. The age
    /// header is written by `wrap_output`, and the last (for a slice under
    /// 64 KiB, the only) chunk by `finish`; both used to surface as
    /// `Encryption` strings naming neither file — `encryption error:
    /// wrap_output failed: failed to write header: ...` and `encryption
    /// error: finish failed: No space left on device (os error 28)`.
    ///
    /// The output is a symlink to `/dev/full`, where every write is ENOSPC.
    /// Never the device itself: on failure `encrypt_file_streaming` removes
    /// its output path, which would unlink `/dev/full` under root.
    #[test]
    fn a_full_staging_filesystem_while_encrypting_names_the_paths_once() {
        if !Path::new("/dev/full").exists() {
            eprintln!("skipping: no /dev/full");
            return;
        }
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("slice.1.dar");
        fs::write(&input, b"plaintext slice").unwrap();
        let output = tmp.path().join("slice.1.dar.age");
        std::os::unix::fs::symlink("/dev/full", &output).unwrap();
        let kp = crate::crypto::keys::generate_keypair();

        let Err(err) = encrypt_file_streaming(&input, &output, &[kp.public_key]) else {
            panic!("every write to /dev/full fails");
        };
        let msg = as_operator_sees_it(err);
        assert!(
            msg.contains(&*output.to_string_lossy()) && msg.contains(&*input.to_string_lossy()),
            "the error must name the slice and its .age: {msg}"
        );
        assert!(
            msg.contains("cannot encrypt") && !msg.contains("wrap_output failed"),
            "names the operation, in the operator's terms: {msg}"
        );
        // age writes the header itself and flattens the io error into its
        // own text ("failed to write header: IoError(Os { .. })"), so the
        // cause is there in age's words, not as "(os error 28)" — once.
        assert_eq!(
            msg.matches("No space left on device").count(),
            1,
            "the cause, printed once: {msg}"
        );
        assert!(
            Path::new("/dev/full").exists(),
            "the device itself is untouched"
        );
    }

    /// A writer that accepts `budget` bytes in all, then fails with ENOSPC —
    /// a staging filesystem filling up partway through one `.age`.
    struct FillsUpAfter {
        budget: usize,
        accepted: usize,
    }
    impl Write for FillsUpAfter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.accepted + buf.len() > self.budget {
                return Err(std::io::Error::from_raw_os_error(
                    nix::errno::Errno::ENOSPC as i32,
                ));
            }
            self.accepted += buf.len();
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The `finish` half of the test above: the header fits, then the final
    /// chunk does not. The failure must come out as an io error, which
    /// `encrypt_file_streaming` rewrites to name both paths — not as an
    /// `Encryption` string it passes through without them.
    #[test]
    fn running_out_of_space_at_the_final_chunk_is_an_io_error() {
        let kp = crate::crypto::keys::generate_keypair();
        let plaintext = vec![7u8; 8 * 1024]; // under one 64 KiB STREAM chunk
        let mut out = FillsUpAfter {
            budget: 4096,
            accepted: 0,
        };

        let err = encrypt_stream(
            build_encryptor(&[kp.public_key]).unwrap(),
            &plaintext[..],
            &mut out,
        )
        .err()
        .expect("8 KiB of ciphertext cannot fit a 4 KiB budget");
        assert!(
            out.accepted > 0,
            "fixture: the header must have been written, so the failure is finish's"
        );
        assert!(
            matches!(err, TapectlError::Io(_)),
            "an ENOSPC in finish must reach the path-naming wrapper as io: {err:?}"
        );
    }

    /// The complement of the refusal above, and the ordering story issue
    /// #115 turns on. Two claims:
    ///
    /// 1. `policy.encrypt = false` is still never honored and never fatal —
    ///    it warns and encrypts anyway (the coordinator decision recorded in
    ///    `stage_create_inner`). The new escrow precondition must not have
    ///    quietly turned that warning into a refusal.
    /// 2. The recipient list `stage_create` RECORDS on the stage set
    ///    contains the escrow public key. That column is the sole evidence
    ///    `volume write`'s pre-flight has (an age X25519 stanza names no
    ///    recipient), so if it were ever wrong, the check built on it would
    ///    be too.
    #[test]
    fn encrypt_false_still_warns_encrypts_and_records_the_escrow_recipient() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
        let escrow_pk: String = conn
            .query_row(
                "SELECT public_key FROM encryption_keys WHERE is_escrow = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        config.defaults.encrypt = false;

        fs::write(
            src.join("f.txt"),
            b"content staged under policy.encrypt = false",
        )
        .unwrap();
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        let mut notices = Vec::new();
        let stage_set_id =
            stage_create_reporting(&conn, &paths, &config, snap_id, false, &mut notices)
                .expect("encrypt=false must warn, not refuse (ADR-0005 makes it un-honorable)");

        // Issue #347: the warning is a notice — written to what
        // `stage_create` makes stderr — not a `tracing::warn!` that
        // `logging.level = "error"` silences.
        let notices = String::from_utf8(notices).unwrap();
        assert!(
            notices.contains("encrypt = false") && notices.contains("\"unit1\""),
            "encrypt = false must produce a notice naming the unit: {notices:?}"
        );
        assert!(
            notices.contains("encrypted anyway") && notices.contains("ADR-0005"),
            "the notice must say what actually happens, and why: {notices:?}"
        );

        let (encrypted, fingerprints): (i64, String) = conn
            .query_row(
                "SELECT encrypted, key_fingerprints FROM stage_sets WHERE id = ?1",
                params![stage_set_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(encrypted, 1, "encryption cannot be disabled (ADR-0005)");

        let recipients: Vec<String> = serde_json::from_str(&fingerprints).unwrap();
        assert!(
            recipients.contains(&escrow_pk),
            "the recorded recipient list must contain the escrow public key — it is \
             what `volume write` checks the slices against (issue #115): {recipients:?}"
        );

        // And the bytes on disk really are age ciphertext, not plaintext dar.
        let slice_path: String = conn
            .query_row(
                "SELECT staging_path FROM stage_slices WHERE stage_set_id = ?1",
                params![stage_set_id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            slice_path.ends_with(".age"),
            "slice must be the encrypted artifact: {slice_path}"
        );
        let head = fs::read(&slice_path).unwrap();
        assert!(
            head.starts_with(b"age-encryption.org/v1"),
            "slice must carry a real age header despite policy.encrypt = false"
        );
    }

    /// The negative half of the notice above, with its positive control in
    /// that test: an ordinary stage — `encrypt` left at its default, and a
    /// staging directory with plenty of room — writes no notice at all, so
    /// the one that does appear is never noise an operator learns to skip.
    #[test]
    fn an_ordinary_stage_writes_no_notices() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, config, src) = setup_unit_with_excludes(&tmp, vec![]);
        assert!(config.defaults.encrypt, "fixture: the default policy");
        fs::write(src.join("f.txt"), b"ordinary content").unwrap();
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();

        let mut notices = Vec::new();
        stage_create_reporting(&conn, &paths, &config, snap_id, false, &mut notices).unwrap();
        assert_eq!(
            String::from_utf8(notices).unwrap(),
            "",
            "an ordinary stage must be quiet"
        );
    }

    // ── issue #354 (b): staging space, refused before dar ──

    /// `len` bytes with no zero byte anywhere — nothing dar could store as a
    /// hole — written and synced, so its allocation is on record.
    fn write_dense_file(path: &Path, len: usize) {
        let data: Vec<u8> = (0..len as u32)
            .map(|i| ((i.wrapping_mul(2_654_435_761) >> 13) as u8) | 1)
            .collect();
        let mut f = fs::File::create(path).unwrap();
        f.write_all(&data).unwrap();
        f.sync_all().unwrap();
    }

    /// A file's allocation on disk, capped at its length — what a bound read
    /// off `st_blocks` would count for it.
    fn allocated(path: &Path) -> i64 {
        use std::os::unix::fs::MetadataExt;
        let m = fs::metadata(path).unwrap();
        m.len().min(m.blocks() * 512) as i64
    }

    /// The file's non-zero bytes: what the staging-space lower bound counts
    /// for it (`validate::SourceValidation::nonzero_bytes`).
    fn nonzero(path: &Path) -> i64 {
        fs::read(path).unwrap().iter().filter(|&&b| b != 0).count() as i64
    }

    /// Issue #354 (b): a unit that cannot fit is refused before dar starts,
    /// with the numbers — what it needs and what is free. It used to be a
    /// `tracing::warn!` at 3x the source, and the real failure came from dar,
    /// partway through the archive.
    ///
    /// The refusal comes after the sha256 pass, which is what measures the
    /// lower bound (see `check_staging_space`), so unlike the key and escrow
    /// refusals a `stage_sets` row exists — validated, never given to dar —
    /// for the next `db::open()` sweep to mark failed, as a BITROT refusal
    /// leaves one. "Before dar" is the dar binary being a path that cannot
    /// exist, and no `dar_command` on the row.
    #[test]
    fn insufficient_staging_space_is_refused_before_dar_with_the_numbers() {
        use crate::util::format_bytes_binary as fmt;
        let tmp = TempDir::new().unwrap();
        let (conn, paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
        config.dar.binary = "/nonexistent/dar-must-never-run".to_string();
        write_dense_file(&src.join("big.bin"), 256 * 1024);
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();

        // What dar must store: every non-zero byte of big.bin and of the
        // dotfile `unit init` wrote (a regular file dar archives like any
        // other). One slice holds it all, so the peak is the archive plus
        // its `.age`: 2x.
        let stored = nonzero(&src.join("big.bin")) + nonzero(&src.join(".tapectl-unit.toml"));
        assert_eq!(
            nonzero(&src.join("big.bin")),
            256 * 1024,
            "fixture: a dense file with no zero byte"
        );
        let free: u64 = 100 * 1024;
        let _free = FreeSpaceOverride::set(free);

        let err = stage_create(&conn, &paths, &config, snap_id, false).unwrap_err();
        let msg = as_operator_sees_it(err);
        assert!(
            msg.contains("not enough space in staging directory")
                && msg.contains(&config.staging.directory),
            "the refusal must say what and where: {msg}"
        );
        assert!(
            msg.contains(&format!("needs at least {}", fmt(2 * stored)))
                && msg.contains(&format!("at least {}", fmt(stored)))
                && msg.contains(&format!("{} is free", fmt(free as i64))),
            "the refusal must carry the numbers: needed {} (archive {}), free {}: {msg}",
            fmt(2 * stored),
            fmt(stored),
            fmt(free as i64)
        );
        assert!(!msg.contains("dar-must-never-run"), "before dar: {msg}");
        let (validated, dar_command): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT source_validated_at, dar_command FROM stage_sets",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(validated.is_some(), "refused after the sha256 pass");
        assert!(
            dar_command.is_none(),
            "refused before dar ran: {dar_command:?}"
        );
        let left: Vec<_> = fs::read_dir(&config.staging.directory)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert!(left.is_empty(), "nothing left in staging: {left:?}");
    }

    /// The hard refusal must not block a unit that fits. A hard-linked file
    /// is one inode, which dar stores once, while the snapshot's recorded
    /// size counts every link — so refusing on the recorded size would turn
    /// away a unit whose real need is well inside the free space, with no
    /// way past it. Here free space sits between the lower bound (the link
    /// counted once) and the recorded-size figure: it is asked about, not
    /// refused, and with `--yes` the stage goes ahead with a notice carrying
    /// both numbers (the refusal without consent is
    /// `free_space_between_the_bounds_is_refused_without_consent`).
    #[test]
    fn a_hard_linked_unit_that_fits_is_staged_with_yes_not_refused() {
        use crate::util::format_bytes_binary as fmt;
        let tmp = TempDir::new().unwrap();
        let (conn, paths, config, src) = setup_unit_with_excludes(&tmp, vec![]);
        write_dense_file(&src.join("a.bin"), 64 * 1024);
        fs::hard_link(src.join("a.bin"), src.join("b.bin")).unwrap();
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        let snapshot = get_snapshot(&conn, snap_id).unwrap();

        let stored = nonzero(&src.join("a.bin")) + nonzero(&src.join(".tapectl-unit.toml"));
        let lower = 2 * stored;
        let upper = 2
            * (snapshot.total_size.unwrap()
                + snapshot.file_count.unwrap() * DAR_ENTRY_OVERHEAD_BYTES);
        let free = (lower + upper) / 2;
        // Fixture check: the recorded size alone WOULD have refused.
        assert!(
            free < 2 * snapshot.total_size.unwrap(),
            "fixture: free space must sit below the recorded-size peak"
        );
        let _free = FreeSpaceOverride::set(free as u64);

        let mut notices = Vec::new();
        stage_create_reporting(&conn, &paths, &config, snap_id, true, &mut notices)
            .expect("a unit whose lower bound fits must be staged with --yes, not refused");
        let notices = String::from_utf8(notices).unwrap();
        assert!(
            notices.contains("may be too small for unit \"unit1\"")
                && notices.contains(&format!("between {} and {}", fmt(lower), fmt(upper)))
                && notices.contains("staging anyway (--yes given)"),
            "the notice must carry both bounds ({} and {}): {notices:?}",
            fmt(lower),
            fmt(upper)
        );
    }

    /// The adversarial review's probe (issue #354): a DENSE file of zeros —
    /// written, not seeked, so every block is allocated — costs dar almost
    /// nothing, because dar stores zero runs as holes by default
    /// (`--sparse-file-min-size` 15; measured: 16 MiB of zeros made a
    /// 752-byte archive). A bound read off the file's allocation counted all
    /// 8 MiB and refused a stage that needs a few KiB, with no way past it —
    /// preallocated disk images, fallocate'd databases and zero-padded ISOs
    /// all look like this. Only what dar cannot turn into a hole, its
    /// non-zero bytes, may refuse; here that is the dotfile, so the stage
    /// is asked about rather than refused, and with `--yes` it runs, with a
    /// notice carrying both figures.
    #[test]
    fn a_dense_file_of_zeros_that_fits_is_staged_not_refused() {
        use crate::util::format_bytes_binary as fmt;
        let tmp = TempDir::new().unwrap();
        let (conn, paths, config, src) = setup_unit_with_excludes(&tmp, vec![]);
        let zeros = src.join("disk.img");
        {
            let mut f = fs::File::create(&zeros).unwrap();
            f.write_all(&vec![0u8; 8 * 1024 * 1024]).unwrap();
            f.sync_all().unwrap();
        }
        assert!(
            allocated(&zeros) >= 8 * 1024 * 1024,
            "fixture: the zeros must be allocated on disk (dense, not sparse), \
             or this test cannot tell an allocation bound from a content one"
        );
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        let free: u64 = 4 * 1024 * 1024;
        let _free = FreeSpaceOverride::set(free);

        let mut notices = Vec::new();
        let stage_set_id =
            stage_create_reporting(&conn, &paths, &config, snap_id, true, &mut notices).expect(
                "8 MiB of zeros needs a few KiB of staging: it must be staged, not refused",
            );

        // It really did fit: dar's archive is a sliver of the 4 MiB "free".
        let dar_size: i64 = conn
            .query_row(
                "SELECT total_dar_size FROM stage_sets WHERE id = ?1",
                params![stage_set_id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            dar_size < 64 * 1024,
            "dar stored the zeros as holes: {dar_size} bytes"
        );

        let dotfile = fs::metadata(src.join(".tapectl-unit.toml")).unwrap().len() as i64;
        let notices = String::from_utf8(notices).unwrap();
        assert!(
            notices.contains("may be too small for unit \"unit1\"")
                && notices.contains(&format!("between {}", fmt(2 * dotfile))),
            "the notice must carry the content-proven low end ({}): {notices:?}",
            fmt(2 * dotfile)
        );
    }

    /// With compression on, dar's archive has no useful lower bound (text
    /// can compress to a sliver), so a short staging directory is never a
    /// hard refusal: it is asked about, and with `--yes` the stage goes
    /// ahead with a notice carrying the uncompressed figure (the refusal
    /// without consent is
    /// `with_compression_short_space_is_asked_about_and_refused_without_consent`).
    #[test]
    fn with_compression_short_space_is_staged_with_yes_never_hard_refused() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
        config.defaults.compression = "gzip".to_string();
        write_dense_file(&src.join("big.bin"), 64 * 1024);
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        let _free = FreeSpaceOverride::set(1024);

        let mut notices = Vec::new();
        stage_create_reporting(&conn, &paths, &config, snap_id, true, &mut notices)
            .expect("compression makes the need unknowable: --yes proceeds");
        let notices = String::from_utf8(notices).unwrap();
        assert!(
            notices.contains("if its data does not compress")
                && notices.contains("compression = \"gzip\"")
                && notices.contains("staging anyway (--yes given)"),
            "the notice must say the figure assumes no compression: {notices:?}"
        );
    }

    /// Issue #354 (b), the compressed case: whether the unit fits cannot be
    /// known before dar runs, so a short staging directory is ASKED about
    /// (ADR-0008 Tier 2) — and a non-interactive run without `--yes`
    /// refuses, with the figures, rather than printing a notice and
    /// carrying on. Asked before the source is read: no `stage_sets` row,
    /// nothing in staging.
    #[test]
    fn with_compression_short_space_is_asked_about_and_refused_without_consent() {
        use crate::util::format_bytes_binary as fmt;
        let tmp = TempDir::new().unwrap();
        let (conn, paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
        config.defaults.compression = "gzip".to_string();
        config.dar.binary = "/nonexistent/dar-must-never-run".to_string();
        write_dense_file(&src.join("big.bin"), 64 * 1024);
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        let _free = FreeSpaceOverride::set(1024);

        let mut notices = Vec::new();
        let err = stage_create_reporting(&conn, &paths, &config, snap_id, false, &mut notices)
            .expect_err("no terminal and no --yes: a stage that may not fit is refused");
        let msg = as_operator_sees_it(err);
        assert!(
            msg.contains("stage unit \"unit1\" refused: non-interactive session")
                && msg.contains("re-run with --yes"),
            "through the consent gate: {msg}"
        );
        assert!(
            msg.contains("may be too small for unit \"unit1\"")
                && msg.contains(&format!("{} free", fmt(1024)))
                && msg.contains("if its data does not compress")
                && msg.contains("compression = \"gzip\""),
            "the refusal carries the figures it would have asked about: {msg}"
        );
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM stage_sets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0, "asked before the stage_sets INSERT");
    }

    /// Issue #354 (b), `compression = none`: free space between the
    /// content-proven low end and the apparent-size figure may or may not
    /// be enough — asked about, and refused without consent in a
    /// non-interactive run. Asked after the sha256 pass (which measures the
    /// low end), before dar: the row is validated, dar never ran, staging
    /// is left empty.
    #[test]
    fn free_space_between_the_bounds_is_refused_without_consent() {
        use crate::util::format_bytes_binary as fmt;
        let tmp = TempDir::new().unwrap();
        let (conn, paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
        config.dar.binary = "/nonexistent/dar-must-never-run".to_string();
        write_dense_file(&src.join("a.bin"), 64 * 1024);
        fs::hard_link(src.join("a.bin"), src.join("b.bin")).unwrap();
        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        let snapshot = get_snapshot(&conn, snap_id).unwrap();
        let stored = nonzero(&src.join("a.bin")) + nonzero(&src.join(".tapectl-unit.toml"));
        let lower = 2 * stored;
        let upper = 2
            * (snapshot.total_size.unwrap()
                + snapshot.file_count.unwrap() * DAR_ENTRY_OVERHEAD_BYTES);
        let _free = FreeSpaceOverride::set(((lower + upper) / 2) as u64);

        let mut notices = Vec::new();
        let err = stage_create_reporting(&conn, &paths, &config, snap_id, false, &mut notices)
            .expect_err("no terminal and no --yes: refused");
        let msg = as_operator_sees_it(err);
        assert!(
            msg.contains("stage unit \"unit1\" refused: non-interactive session"),
            "through the consent gate: {msg}"
        );
        assert!(
            msg.contains(&format!("between {} and {}", fmt(lower), fmt(upper))),
            "the refusal carries both bounds: {msg}"
        );
        assert!(!msg.contains("dar-must-never-run"), "before dar: {msg}");
        let (validated, dar_command): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT source_validated_at, dar_command FROM stage_sets",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(validated.is_some(), "asked after the sha256 pass");
        assert!(dar_command.is_none(), "asked before dar: {dar_command:?}");
        let left: Vec<_> = fs::read_dir(&config.staging.directory)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert!(left.is_empty(), "nothing left in staging: {left:?}");

        // The way past the refusal it names: the version is still unstaged
        // (`created`, which `stage create <unit>` selects), and the same
        // stage with `--yes` goes ahead beside the refused row.
        assert_eq!(get_snapshot(&conn, snap_id).unwrap().status, "created");
        config.dar.binary = "dar".to_string();
        let mut notices = Vec::new();
        stage_create_reporting(&conn, &paths, &config, snap_id, true, &mut notices)
            .expect("re-run with --yes, the refused stage proceeds");
        assert_eq!(get_snapshot(&conn, snap_id).unwrap().status, "staged");
    }

    /// Issue #52 change 2 — the self-match trap. `snapshot_create` for an
    /// ordinary, already-registered unit must succeed: the nesting check
    /// must exclude the unit's own row, or `check_path.starts_with(existing)`
    /// is trivially true for a unit against itself (`check_path == existing`)
    /// and every snapshot would fail with "X is inside existing unit X".
    /// Written and run BEFORE the naive (non-excluding) nesting-check wire-up
    /// existed, to prove the trap: with a bare
    /// `nesting::check_nesting(conn, source_path)` call (no exclusion), this
    /// test failed with:
    ///
    /// ```text
    /// called `Result::unwrap()` on an `Err` value: NestedUnit(
    ///     "/tmp/.../src is inside existing unit \"unit1\" at /tmp/.../src",
    /// )
    /// ```
    ///
    /// After wiring `check_nesting_excluding(conn, source_path, Some(unit.id))`
    /// instead, it passes.
    #[test]
    fn snapshot_create_does_not_trip_nesting_check_against_its_own_unit() {
        let tmp = TempDir::new().unwrap();
        let (conn, _paths, _config, src) = setup_unit_with_excludes(&tmp, vec![]);
        fs::write(src.join("f.txt"), b"ordinary file").unwrap();

        let result = snapshot_create(&conn, "unit1", &Config::default());
        assert!(
            result.is_ok(),
            "snapshot_create must not match a unit against its own row: {result:?}"
        );
    }

    /// Issue #52 change 3, design line 184: "unit init and snapshot create
    /// check parent/child. Both errors." Built with raw `INSERT INTO units`
    /// rows (not `init_unit`, which would itself refuse to create the
    /// nested unit) so both a parent and a genuinely nested child unit
    /// exist, and `snapshot_create` on the child must error.
    #[test]
    fn snapshot_create_errors_on_genuinely_nested_unit() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let paths = TapectlPaths::new(home);
        paths.ensure_dirs().unwrap();
        let conn = crate::db::open(&paths.db_file).unwrap();

        crate::tenant::add_tenant(&conn, &paths, "alice", None, false).unwrap();
        let tenant_id: i64 = conn
            .query_row("SELECT id FROM tenants WHERE name = 'alice'", [], |r| {
                r.get(0)
            })
            .unwrap();

        let parent = tmp.path().join("parent");
        let child = parent.join("child");
        fs::create_dir_all(&child).unwrap();

        conn.execute(
            "INSERT INTO units (uuid, tenant_id, name, current_path, status)
             VALUES (?1, ?2, 'parent-unit', ?3, 'active')",
            params![
                uuid::Uuid::new_v4().to_string(),
                tenant_id,
                parent.to_string_lossy().to_string()
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO units (uuid, tenant_id, name, current_path, status)
             VALUES (?1, ?2, 'child-unit', ?3, 'active')",
            params![
                uuid::Uuid::new_v4().to_string(),
                tenant_id,
                child.to_string_lossy().to_string()
            ],
        )
        .unwrap();

        let result = snapshot_create(&conn, "child-unit", &Config::default());
        assert!(
            matches!(result, Err(TapectlError::NestedUnit(_))),
            "nested unit must error per design line 184, got: {result:?}"
        );
    }

    /// Design line 185: "Empty units: warn but allow." An empty unit still
    /// produces a snapshot row with `file_count = 0` — proving "allow", not
    /// "refuse" — since asserting on `tracing::warn!` output directly is
    /// awkward in this crate's existing test style.
    #[test]
    fn snapshot_create_allows_empty_unit_with_warning() {
        // Built with a raw `INSERT INTO units` row rather than
        // `setup_unit_with_excludes`/`init_unit` — `init_unit` always
        // writes a `.tapectl-unit.toml` dotfile into the unit's directory,
        // and that dotfile is itself a real file `walk_directory` (rightly)
        // counts, so a unit created that way is never genuinely empty.
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let paths = TapectlPaths::new(home);
        paths.ensure_dirs().unwrap();
        let conn = crate::db::open(&paths.db_file).unwrap();

        crate::tenant::add_tenant(&conn, &paths, "alice", None, false).unwrap();
        let tenant_id: i64 = conn
            .query_row("SELECT id FROM tenants WHERE name = 'alice'", [], |r| {
                r.get(0)
            })
            .unwrap();

        let src = tmp.path().join("empty-src");
        fs::create_dir_all(&src).unwrap();

        conn.execute(
            "INSERT INTO units (uuid, tenant_id, name, current_path, status)
             VALUES (?1, ?2, 'unit1', ?3, 'active')",
            params![
                uuid::Uuid::new_v4().to_string(),
                tenant_id,
                src.to_string_lossy().to_string()
            ],
        )
        .unwrap();

        let snap_id = snapshot_create(&conn, "unit1", &Config::default())
            .expect("empty unit must be allowed, only warned about");

        let file_count: i64 = conn
            .query_row(
                "SELECT file_count FROM snapshots WHERE id = ?1",
                params![snap_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            file_count, 0,
            "empty unit's snapshot must record file_count = 0"
        );
    }

    /// Design line 203: "Warns on files > large_file_warn_threshold." A
    /// file over threshold is only warned about, never blocking — the
    /// snapshot succeeds and the manifest still records the file (proving
    /// "warn", not "refuse" or "skip").
    #[test]
    fn snapshot_create_allows_large_file_with_warning() {
        let tmp = TempDir::new().unwrap();
        let (conn, _paths, mut config, src) = setup_unit_with_excludes(&tmp, vec![]);
        // Issue #59: "10B" was never a documented valid suffix (only
        // K/KB/M/MB/G/GB/T/TB, or a bare number meaning bytes) — it only
        // "worked" before because an unrecognized suffix silently fell
        // back to multiplier 1.0. A bare "10" is the correct spelling for
        // ten bytes now that an unknown suffix is a hard error.
        config.defaults.large_file_warn_threshold = "10".to_string();
        fs::write(src.join("big.bin"), vec![0u8; 1024]).unwrap();

        let snap_id = snapshot_create(&conn, "unit1", &config)
            .expect("a file over the large-file threshold must only warn, not fail");

        let recorded_size: i64 = conn
            .query_row(
                "SELECT size_bytes FROM files WHERE snapshot_id = ?1 AND path = 'big.bin'",
                params![snap_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            recorded_size, 1024,
            "the large file must still be recorded in the manifest, not skipped"
        );
    }

    /// THE core claim of issue #49 (its own re-triage escalation): a unit
    /// must not be permanently blocked from staging by content drift in a
    /// file dar was never going to archive. Write this FIRST — it must
    /// fail against the pre-#49 code (see the PR report for the captured
    /// pre-fix failure): `walk_directory` used to record every file
    /// unfiltered, so `backfill_checksums` established a sha256 baseline
    /// for the excluded junk file too, and this re-stage's same-size
    /// content drift then tripped `validate_source`'s BITROT refusal for
    /// content dar was never going to touch.
    #[test]
    fn excluded_junk_file_content_drift_at_stable_size_does_not_false_positive_bitrot() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, config, src) = setup_unit_with_excludes(&tmp, vec!["*.tmp".to_string()]);

        fs::write(src.join("keep.txt"), b"real archival content, kept").unwrap();
        fs::write(src.join("junk.tmp"), b"AAAA").unwrap();

        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        stage_create(&conn, &paths, &config, snap_id, false).expect("first stage must succeed");

        // The excluded junk file's content drifts at an UNCHANGED size —
        // exactly the false-BITROT scenario the issue describes
        // (Thumbs.db/*.tmp regenerating at a stable size).
        fs::write(src.join("junk.tmp"), b"BBBB").unwrap();

        // Re-staging the SAME snapshot (a real "stage create" retry) must
        // succeed cleanly — never raise BITROT over content dar was never
        // going to archive.
        let result = stage_create(&conn, &paths, &config, snap_id, false);
        assert!(
            result.is_ok(),
            "re-staging must succeed — an excluded file's content drift must \
             never raise BITROT: {result:?}"
        );
    }

    #[test]
    fn excluded_files_do_not_appear_in_manifest_or_files_table() {
        let tmp = TempDir::new().unwrap();
        let (conn, _paths, _config, src) =
            setup_unit_with_excludes(&tmp, vec!["*.tmp".to_string()]);

        fs::write(src.join("keep.txt"), b"kept content").unwrap();
        fs::write(src.join("junk.tmp"), b"excluded junk").unwrap();

        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();

        let files_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM files WHERE snapshot_id = ?1 AND path = 'junk.tmp'",
                params![snap_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(files_count, 0, "excluded file must not appear in `files`");

        let manifest_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM manifest_entries me
                 JOIN manifests m ON m.id = me.manifest_id
                 WHERE m.snapshot_id = ?1 AND me.path = 'junk.tmp'",
                params![snap_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            manifest_count, 0,
            "excluded file must not appear in `manifest_entries`"
        );

        let kept_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM files WHERE snapshot_id = ?1 AND path = 'keep.txt'",
                params![snap_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(kept_count, 1, "non-excluded file must still be recorded");
    }

    #[test]
    fn excluded_files_never_receive_a_sha256_baseline() {
        // "Once (3) lands, backfill_checksums naturally stops seeing them
        // — verify that is true rather than assuming" (issue #49). Drives
        // the REAL stage_create pipeline (not just snapshot_create) so
        // backfill_checksums actually runs.
        let tmp = TempDir::new().unwrap();
        let (conn, paths, config, src) = setup_unit_with_excludes(&tmp, vec!["*.tmp".to_string()]);

        fs::write(src.join("keep.txt"), b"kept content, baselined").unwrap();
        fs::write(src.join("junk.tmp"), b"excluded junk").unwrap();

        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        stage_create(&conn, &paths, &config, snap_id, false).unwrap();

        let junk_row_exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM files WHERE snapshot_id = ?1 AND path = 'junk.tmp'",
                params![snap_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            junk_row_exists, 0,
            "excluded file must have no `files` row at all, so there is \
             nothing for backfill_checksums to baseline"
        );

        let kept_sha: Option<String> = conn
            .query_row(
                "SELECT sha256 FROM files WHERE snapshot_id = ?1 AND path = 'keep.txt'",
                params![snap_id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            kept_sha.is_some(),
            "the non-excluded file must still get its baseline established"
        );
    }

    #[test]
    fn dotfile_exclude_patterns_reach_dars_constructed_arguments() {
        // "Dotfile patterns reach dar (assert on the constructed dar
        // arguments)" — stage_sets.dar_command records the exact
        // Command::Debug-formatted string create_archive ran.
        let tmp = TempDir::new().unwrap();
        let (conn, paths, config, src) =
            setup_unit_with_excludes(&tmp, vec!["*.unusual-dotfile-pattern".to_string()]);

        fs::write(src.join("keep.txt"), b"kept content").unwrap();

        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        let stage_set_id = stage_create(&conn, &paths, &config, snap_id, false).unwrap();

        let dar_command: String = conn
            .query_row(
                "SELECT dar_command FROM stage_sets WHERE id = ?1",
                params![stage_set_id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            dar_command.contains("*.unusual-dotfile-pattern"),
            "the dotfile's own exclude pattern must reach dar's constructed \
             -X arguments, got: {dar_command}"
        );
        // The pre-existing global excludes must still be present too (this
        // fix merges, not replaces).
        assert!(
            dar_command.contains("Thumbs.db"),
            "config.defaults.global_excludes must still reach dar, got: {dar_command}"
        );
    }

    /// Issue #359 (c): a directory pattern (`name/`) keeps the subtree out
    /// of the DAR ARCHIVE, not only out of the manifest, the `files` table
    /// and the dirty scan. Before, `stage_create` handed dar every pattern
    /// as a raw `-X` basename mask and no `-P` prune, so `.cache/` matched
    /// nothing in dar and the cache's bytes were archived, encrypted and
    /// written to tape while the catalog said they were not there.
    ///
    /// The archive is read back through the isolated catalog dar extracted
    /// from it (`stage_sets.catalog_path` — the plaintext slices are gone
    /// once encrypted): `dar -l` lists every entry the archive holds. The
    /// pruned directory itself stays, empty (`-D`), in the archive and in
    /// the manifest alike; a directory INSIDE it is in neither.
    #[test]
    fn a_directory_exclude_keeps_the_subtree_out_of_the_dar_archive() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, config, src) =
            setup_unit_with_excludes(&tmp, vec![".cache/".to_string()]);
        fs::write(src.join("keep.txt"), b"kept content").unwrap();
        fs::create_dir_all(src.join(".cache/deepdir")).unwrap();
        fs::write(src.join(".cache/cached-top.bin"), b"cache bytes").unwrap();
        fs::write(src.join(".cache/deepdir/cached-deep.bin"), b"deeper").unwrap();
        fs::create_dir_all(src.join("sub/.cache")).unwrap();
        fs::write(src.join("sub/.cache/cached-nested.bin"), b"nested").unwrap();
        fs::write(src.join("sub/kept-nested.txt"), b"kept too").unwrap();

        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        let stage_set_id = stage_create(&conn, &paths, &config, snap_id, false).unwrap();

        let catalog: String = conn
            .query_row(
                "SELECT catalog_path FROM stage_sets WHERE id = ?1",
                params![stage_set_id],
                |r| r.get(0),
            )
            .unwrap();
        let out = std::process::Command::new("dar")
            .arg("-l")
            .arg(&catalog)
            .arg("-Q")
            .output()
            .expect("dar must be on PATH (tests/test_dependencies.rs)");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let listing = String::from_utf8_lossy(&out.stdout).into_owned();

        // Positive control: the listing is real and holds what was kept.
        for kept in ["keep.txt", "kept-nested.txt", ".cache"] {
            assert!(listing.contains(kept), "{kept} must be archived: {listing}");
        }
        for pruned in [
            "cached-top.bin",
            "cached-deep.bin",
            "cached-nested.bin",
            "deepdir",
        ] {
            assert!(
                !listing.contains(pruned),
                "`.cache/` must keep {pruned} out of the archive: {listing}"
            );
        }

        // The manifest agrees with the archive, directories included.
        let recorded = |path: &str| -> i64 {
            conn.query_row(
                "SELECT COUNT(*) FROM files WHERE snapshot_id = ?1 AND path = ?2",
                params![snap_id, path],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(recorded(".cache"), 1, "the pruned directory itself is kept");
        assert_eq!(recorded("sub/.cache"), 1, "at any depth");
        assert_eq!(
            recorded(".cache/deepdir"),
            0,
            "a directory inside a pruned subtree is not recorded, as dar -D does not store it"
        );
        assert_eq!(recorded(".cache/cached-top.bin"), 0);
    }

    #[test]
    fn a_unit_with_no_excludes_configured_behaves_exactly_as_before() {
        // Issue #49 trap: "do NOT break units with no excludes configured
        // — the empty-pattern case must behave exactly as today, and is
        // the common case." No dotfile override (init_unit's own default)
        // AND an empty `global_excludes` slice passed explicitly — the true
        // "nothing configured anywhere" case, which must record everything.
        // (The case where `global_excludes` is non-empty but no dotfile
        // exists — the ticket's own headline scenario — is covered
        // separately below, in the "second half" test block; this test's
        // fixture is deliberately a neutral filename, not one that
        // resembles a real default global-exclude pattern, so it stays a
        // clean proof of the empty/empty case rather than depending on
        // `Config::default()`'s specific pattern list.)
        let tmp = TempDir::new().unwrap();
        let (conn, paths, config, src) = setup_unit_with_excludes(&tmp, vec![]);

        fs::write(src.join("keep.txt"), b"kept content").unwrap();
        fs::write(src.join("media_file.dat"), b"ordinary archival content").unwrap();

        let snap_id = snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        let file_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM files WHERE snapshot_id = ?1",
                params![snap_id],
                |row| row.get(0),
            )
            .unwrap();
        // 2 files recorded (keep.txt, media_file.dat) + the dotfile itself
        // (.tapectl-unit.toml, swept up like any other regular file —
        // pre-existing, unrelated behavior this fix does not change).
        assert_eq!(
            file_count, 3,
            "with no dotfile exclude_patterns and no global_excludes, nothing \
             is filtered from the walk — matches pre-#49 behavior exactly"
        );

        stage_create(&conn, &paths, &config, snap_id, false)
            .expect("staging a unit with no excludes at all must succeed exactly as before");
    }

    // ── issue #49 (second half): global excludes must reach both walks ──
    //
    // The ticket's own headline example: `config.defaults.global_excludes`
    // (Thumbs.db/.DS_Store/*.nfo/*.tmp by default) reached dar only —
    // neither walk saw it. For a unit with NO dotfile override (the common
    // case), `walk_directory` recorded the globally-excluded file into
    // `files`, `backfill_checksums` gave it a sha256 baseline, and a
    // same-size content regeneration then tripped `validate_source`'s
    // BITROT refusal for content dar was never going to archive — see the
    // PR report for the captured pre-fix failure. These tests use
    // `setup_unit_with_excludes(&tmp, vec![])` (no dotfile override) and
    // pass the REAL `config.defaults.global_excludes` returned by that
    // helper, so `Thumbs.db` here is the actual default pattern, not a
    // stand-in.

    /// Write this FIRST — it must fail against the pre-fix code (see the PR
    /// report for the captured pre-fix failure output).
    #[test]
    fn global_default_excluded_file_content_drift_at_stable_size_does_not_false_positive_bitrot() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, config, src) = setup_unit_with_excludes(&tmp, vec![]);

        fs::write(src.join("keep.txt"), b"real archival content, kept").unwrap();
        fs::write(src.join("Thumbs.db"), b"AAAA").unwrap();

        let snap_id = snapshot_create(&conn, "unit1", &config).unwrap();
        stage_create(&conn, &paths, &config, snap_id, false).expect("first stage must succeed");

        // Thumbs.db regenerates at an UNCHANGED size — exactly the
        // false-BITROT scenario the issue describes.
        fs::write(src.join("Thumbs.db"), b"BBBB").unwrap();

        let result = stage_create(&conn, &paths, &config, snap_id, false);
        assert!(
            result.is_ok(),
            "re-staging must succeed — a globally-excluded file's content drift \
             must never raise BITROT, even with no dotfile override: {result:?}"
        );
    }

    #[test]
    fn global_default_excluded_files_do_not_appear_in_manifest_or_files_table() {
        let tmp = TempDir::new().unwrap();
        let (conn, _paths, config, src) = setup_unit_with_excludes(&tmp, vec![]);

        fs::write(src.join("keep.txt"), b"kept content").unwrap();
        fs::write(src.join("Thumbs.db"), b"thumbnail cache junk").unwrap();

        let snap_id = snapshot_create(&conn, "unit1", &config).unwrap();

        let files_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM files WHERE snapshot_id = ?1 AND path = 'Thumbs.db'",
                params![snap_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            files_count, 0,
            "a globally-excluded file must not appear in `files`, even with no \
             dotfile override"
        );

        let manifest_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM manifest_entries me
                 JOIN manifests m ON m.id = me.manifest_id
                 WHERE m.snapshot_id = ?1 AND me.path = 'Thumbs.db'",
                params![snap_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            manifest_count, 0,
            "a globally-excluded file must not appear in `manifest_entries` either"
        );

        let kept_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM files WHERE snapshot_id = ?1 AND path = 'keep.txt'",
                params![snap_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(kept_count, 1, "non-excluded file must still be recorded");
    }

    #[test]
    fn global_default_excluded_files_never_receive_a_sha256_baseline() {
        let tmp = TempDir::new().unwrap();
        let (conn, paths, config, src) = setup_unit_with_excludes(&tmp, vec![]);

        fs::write(src.join("keep.txt"), b"kept content, baselined").unwrap();
        fs::write(src.join("Thumbs.db"), b"thumbnail cache junk").unwrap();

        let snap_id = snapshot_create(&conn, "unit1", &config).unwrap();
        stage_create(&conn, &paths, &config, snap_id, false).unwrap();

        let junk_row_exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM files WHERE snapshot_id = ?1 AND path = 'Thumbs.db'",
                params![snap_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            junk_row_exists, 0,
            "a globally-excluded file must have no `files` row at all, so there \
             is nothing for backfill_checksums to baseline"
        );

        let kept_sha: Option<String> = conn
            .query_row(
                "SELECT sha256 FROM files WHERE snapshot_id = ?1 AND path = 'keep.txt'",
                params![snap_id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            kept_sha.is_some(),
            "the non-excluded file must still get its baseline established"
        );
    }

    // ── issue #159 / ADR-0012: a version is minted only when content changed ──

    /// Bare-bones tenant + active unit for the acceptance tests below — no
    /// escrow key, no staging directory, no dotfile: only `snapshot_create`/
    /// `snapshot_create_detailed` is under test here, and neither needs any
    /// of that (only `stage_create` requires a registered escrow
    /// recipient). Each call gets its own unique unit name so a test that
    /// seeds two units in one `conn` never collides.
    fn seed_snapshot_test_unit(conn: &Connection, path: &Path, checksum_mode: &str) -> String {
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('t159', 0, 'active')",
            [],
        )
        .ok(); // may already exist across two seeds in one test; ignore
        let tenant_id: i64 = conn
            .query_row("SELECT id FROM tenants WHERE name = 't159'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let name = format!("unit-{}", uuid::Uuid::new_v4());
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, current_path, checksum_mode, status)
             VALUES (?1, ?2, ?3, ?4, ?5, 'active')",
            params![
                uuid::Uuid::new_v4().to_string(),
                name,
                tenant_id,
                path.to_string_lossy().to_string(),
                checksum_mode,
            ],
        )
        .unwrap();
        name
    }

    /// Sets a file's mtime back to `mtime` after its content has already
    /// been rewritten — constructs the one case `mtime_size` cannot see:
    /// identical size, identical mtime, different bytes. Mirrors
    /// `collection::fingerprint`'s test helper of the same shape.
    fn restore_mtime_for_snapshot_test(path: &Path, mtime: std::time::SystemTime) {
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
    }

    #[test]
    fn snapshot_create_detailed_reports_the_existing_version_when_content_is_unchanged() {
        let conn = crate::db::open_memory().unwrap();
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), b"hello").unwrap();
        let unit_name = seed_snapshot_test_unit(&conn, tmp.path(), "mtime_size");

        let first = snapshot_create_detailed(&conn, &unit_name, &Config::default()).unwrap();
        assert!(first.minted);
        assert_eq!(first.version, 1);

        let second = snapshot_create_detailed(&conn, &unit_name, &Config::default()).unwrap();
        assert!(
            !second.minted,
            "unchanged content must not mint a new version"
        );
        assert_eq!(second.snapshot_id, first.snapshot_id);
        assert_eq!(second.version, 1);

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM snapshots WHERE unit_id = \
                 (SELECT id FROM units WHERE name = ?1)",
                params![unit_name],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "no second snapshot row must be created");
    }

    /// Issue #206 / the 2026-09-17 pre-production review: this pins what the
    /// minting rule actually guarantees, which is NARROWER than ADR-0012's
    /// concluding sentence claims.
    ///
    /// ADR-0012's rule is scoped to "its latest CURRENT snapshot", and that is
    /// exactly what `snapshot_create_detailed` implements. Its next sentence
    /// then generalises to "two versions of a unit never hold identical
    /// content" — which does not follow. Revert a unit's bytes to those of an
    /// older, superseded version and a fresh version is minted holding content
    /// byte-identical to that old one.
    ///
    /// Recorded as a PIN, not a bug report: the consequence is an under-count
    /// (the new version genuinely has no slices of its own, and
    /// `copy_count_expr`'s minimum is over CURRENT snapshots, so the dead
    /// sibling is correctly not credited). ADR-0012 itself calls an under-count
    /// "the safe direction, but still wrong". Whether the rule should widen to
    /// compare against every live version is a design question, not something
    /// to decide by changing this test.
    #[test]
    fn reverting_to_a_superseded_versions_content_mints_an_identical_sibling() {
        let conn = crate::db::open_memory().unwrap();
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("f.txt");
        std::fs::write(&file, b"content-A").unwrap();
        let unit_name = seed_snapshot_test_unit(&conn, tmp.path(), "mtime_size");

        let v1 = snapshot_create_detailed(&conn, &unit_name, &Config::default()).unwrap();
        assert!(v1.minted);

        std::fs::write(&file, b"content-B-which-differs-in-size").unwrap();
        let v2 = snapshot_create_detailed(&conn, &unit_name, &Config::default()).unwrap();
        assert!(v2.minted);
        assert_eq!(v2.version, 2);

        // v1 is now superseded; put the source back to exactly v1's bytes.
        std::fs::write(&file, b"content-A").unwrap();
        let v3 = snapshot_create_detailed(&conn, &unit_name, &Config::default()).unwrap();

        assert!(
            v3.minted,
            "a fresh version IS minted -- the short-circuit only ever compares \
             against the LATEST snapshot, and that is v2, which differs"
        );
        assert_eq!(v3.version, 3);
        assert_ne!(
            v3.snapshot_id, v1.snapshot_id,
            "v1 is not resurrected; v3 is a distinct row holding identical content"
        );

        // The two rows really do record the same content.
        let stamps = |sid: i64| -> Vec<(String, i64)> {
            let mut st = conn
                .prepare(
                    "SELECT path, size_bytes FROM files
                     WHERE snapshot_id = ?1 AND is_directory = 0 ORDER BY path",
                )
                .unwrap();
            st.query_map(params![sid], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap()
        };
        assert_eq!(
            stamps(v1.snapshot_id),
            stamps(v3.snapshot_id),
            "v1 and v3 hold byte-identical content, which ADR-0012's concluding \
             sentence says cannot happen"
        );
    }

    #[test]
    fn snapshot_create_detailed_mints_a_new_version_when_a_file_changes() {
        let conn = crate::db::open_memory().unwrap();
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("f.txt");
        std::fs::write(&file, b"hello").unwrap();
        let unit_name = seed_snapshot_test_unit(&conn, tmp.path(), "mtime_size");

        let first = snapshot_create_detailed(&conn, &unit_name, &Config::default()).unwrap();
        assert!(first.minted);

        std::fs::write(&file, b"hello, world! now a different size").unwrap();
        let second = snapshot_create_detailed(&conn, &unit_name, &Config::default()).unwrap();
        assert!(second.minted, "changed content must mint a new version");
        assert_eq!(second.version, 2);
        assert_ne!(second.snapshot_id, first.snapshot_id);
    }

    #[test]
    fn snapshot_create_detailed_mints_a_new_version_when_a_file_is_removed() {
        // ADR-0012: removal counts as change, not just add/modify.
        let conn = crate::db::open_memory().unwrap();
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("f.txt");
        std::fs::write(&file, b"hello").unwrap();
        std::fs::write(tmp.path().join("keep.txt"), b"keep").unwrap();
        let unit_name = seed_snapshot_test_unit(&conn, tmp.path(), "mtime_size");

        let first = snapshot_create_detailed(&conn, &unit_name, &Config::default()).unwrap();
        assert!(first.minted);

        std::fs::remove_file(&file).unwrap();
        let second = snapshot_create_detailed(&conn, &unit_name, &Config::default()).unwrap();
        assert!(
            second.minted,
            "a removed file is a content change and must mint a new version"
        );
        assert_eq!(second.version, 2);
    }

    #[test]
    fn snapshot_create_detailed_reuses_an_unwritten_matching_snapshot_instead_of_minting_beside_it()
    {
        // Change 3: the latest snapshot is 'created' (never staged), not
        // 'current' — a match must reuse that row, not mint a
        // byte-identical sibling beside it.
        let conn = crate::db::open_memory().unwrap();
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), b"hello").unwrap();
        let unit_name = seed_snapshot_test_unit(&conn, tmp.path(), "mtime_size");

        let first = snapshot_create_detailed(&conn, &unit_name, &Config::default()).unwrap();
        assert!(first.minted);
        assert_eq!(first.status, "created");
        let status: String = conn
            .query_row(
                "SELECT status FROM snapshots WHERE id = ?1",
                params![first.snapshot_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "created", "never staged — status stays 'created'");

        let second = snapshot_create_detailed(&conn, &unit_name, &Config::default()).unwrap();
        assert!(!second.minted);
        assert_eq!(second.snapshot_id, first.snapshot_id);
        assert_eq!(second.status, "created");

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM snapshots WHERE unit_id = \
                 (SELECT id FROM units WHERE name = ?1)",
                params![unit_name],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 1,
            "reuse must not leave a second row beside the unwritten one"
        );
    }

    #[test]
    fn snapshot_create_detailed_honors_checksum_mode_on_a_same_size_same_mtime_content_change() {
        // The one edit `mtime_size` cannot catch (issue #36, reused here
        // for #159): same path, same size, same mtime, different bytes.
        // `mtime_size` and `sha256` must disagree about it — each unit
        // follows its own `checksum_mode`, not a global rule.
        let conn = crate::db::open_memory().unwrap();

        let mtime_tmp = TempDir::new().unwrap();
        let mtime_file = mtime_tmp.path().join("f.txt");
        std::fs::write(&mtime_file, b"original content!").unwrap();
        let mtime_unit = seed_snapshot_test_unit(&conn, mtime_tmp.path(), "mtime_size");

        let sha_tmp = TempDir::new().unwrap();
        let sha_file = sha_tmp.path().join("f.txt");
        std::fs::write(&sha_file, b"original content!").unwrap();
        let sha_unit = seed_snapshot_test_unit(&conn, sha_tmp.path(), "sha256");

        let m1 = snapshot_create_detailed(&conn, &mtime_unit, &Config::default()).unwrap();
        let s1 = snapshot_create_detailed(&conn, &sha_unit, &Config::default()).unwrap();
        assert!(m1.minted && s1.minted);

        // Establish the sha256 baseline for the sha-mode unit — what a
        // real `stage_create` would have backfilled via the same
        // `hash_source_file` this scan reuses.
        let (hash, _) = crate::staging::validate::hash_source_file(&sha_file, "f.txt").unwrap();
        conn.execute(
            "UPDATE files SET sha256 = ?1 WHERE snapshot_id = ?2 AND path = 'f.txt'",
            params![hash, s1.snapshot_id],
        )
        .unwrap();

        // Same-size, same-mtime content swap on both units.
        let mtime_before = std::fs::metadata(&mtime_file).unwrap().modified().unwrap();
        std::fs::write(&mtime_file, b"REPLACED content!").unwrap();
        restore_mtime_for_snapshot_test(&mtime_file, mtime_before);

        let sha_before = std::fs::metadata(&sha_file).unwrap().modified().unwrap();
        std::fs::write(&sha_file, b"REPLACED content!").unwrap();
        restore_mtime_for_snapshot_test(&sha_file, sha_before);

        let m2 = snapshot_create_detailed(&conn, &mtime_unit, &Config::default()).unwrap();
        let s2 = snapshot_create_detailed(&conn, &sha_unit, &Config::default()).unwrap();

        assert!(
            !m2.minted,
            "mtime_size mode must stay blind to a same-size-same-mtime content \
             change — that tradeoff is documented, not a bug"
        );
        assert!(
            s2.minted,
            "sha256 mode must catch a content change mtime_size cannot see"
        );
    }

    /// Issue #41: `write_stage_receipt` and `secure_catalog_files` tested
    /// directly and hermetically — no dar binary, no full `stage_create`
    /// pipeline — per the same reasoning `crypto::keys`'s tests already
    /// apply to secret keys: a permission bug belongs to the function that
    /// sets (or fails to set) the mode, not to everything that happens to
    /// call it three layers up.
    mod file_custody {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        fn mode_of(path: &Path) -> u32 {
            fs::metadata(path).unwrap().permissions().mode() & 0o777
        }

        fn test_paths(tmp: &TempDir) -> TapectlPaths {
            let paths = TapectlPaths::new(tmp.path().join(".tapectl"));
            paths.ensure_dirs().unwrap();
            paths
        }

        #[test]
        fn write_stage_receipt_creates_file_at_0600() {
            let tmp = TempDir::new().unwrap();
            let paths = test_paths(&tmp);

            let path = write_stage_receipt(&paths, 42, "receipt body\n").unwrap();

            assert!(path.exists());
            assert_eq!(fs::read_to_string(&path).unwrap(), "receipt body\n");
            assert_eq!(mode_of(&path), 0o600, "receipt should be 0600");
        }

        #[test]
        fn write_stage_receipt_creates_receipts_dir_if_missing() {
            let tmp = TempDir::new().unwrap();
            // Deliberately do NOT call ensure_dirs — this exercises
            // write_stage_receipt's own fs::create_dir_all.
            let paths = TapectlPaths::new(tmp.path().join(".tapectl"));
            assert!(!paths.receipts_dir.exists());

            let path = write_stage_receipt(&paths, 7, "body").unwrap();

            assert!(path.exists());
            assert_eq!(mode_of(&path), 0o600);
        }

        #[test]
        fn secure_catalog_files_tightens_a_loose_file_dar_wrote() {
            let tmp = TempDir::new().unwrap();
            let catalog_dir = tmp.path().join("catalogs").join("abcd1234");
            fs::create_dir_all(&catalog_dir).unwrap();
            // Simulate what dar's `-C` extraction leaves behind: a file
            // written by an external subprocess, at whatever mode the
            // process umask handed out — not tapectl's own `OpenOptions`.
            let catalog_file = catalog_dir.join("abcd1234_v1.1.dar");
            fs::write(&catalog_file, b"fake dar catalog bytes").unwrap();
            fs::set_permissions(&catalog_file, fs::Permissions::from_mode(0o644)).unwrap();
            assert_eq!(mode_of(&catalog_file), 0o644, "fixture must start loose");

            secure_catalog_files(&catalog_dir);

            assert_eq!(
                mode_of(&catalog_file),
                0o600,
                "a catalog file dar wrote should be tightened to 0600"
            );
        }

        #[test]
        fn secure_catalog_files_tightens_every_file_present() {
            let tmp = TempDir::new().unwrap();
            let catalog_dir = tmp.path().join("catalogs").join("multi");
            fs::create_dir_all(&catalog_dir).unwrap();
            for name in ["a.1.dar", "a.2.dar", "a.3.dar"] {
                let p = catalog_dir.join(name);
                fs::write(&p, b"slice").unwrap();
                fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
            }

            secure_catalog_files(&catalog_dir);

            for name in ["a.1.dar", "a.2.dar", "a.3.dar"] {
                assert_eq!(
                    mode_of(&catalog_dir.join(name)),
                    0o600,
                    "{name} should be 0600"
                );
            }
        }

        #[test]
        fn secure_catalog_files_on_missing_dir_does_not_panic() {
            let tmp = TempDir::new().unwrap();
            let ghost = tmp.path().join("does-not-exist");
            // Best-effort: must not panic even if the directory somehow
            // isn't there (e.g. dar failed before this is ever reached in
            // the real call site, though that path already returns `?`
            // earlier and never gets here).
            secure_catalog_files(&ghost);
        }
    }
}
