pub mod clean;
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

/// Create a snapshot: fast directory walk, manifest, files table.
pub fn snapshot_create(conn: &Connection, unit_name: &str) -> Result<i64> {
    let unit = queries::get_unit_by_name(conn, unit_name)?
        .ok_or_else(|| TapectlError::UnitNotFound(unit_name.to_string()))?;

    let source_path = unit
        .current_path
        .as_deref()
        .ok_or_else(|| TapectlError::Other(format!("unit \"{unit_name}\" has no path")))?;

    if !Path::new(source_path).is_dir() {
        return Err(TapectlError::UnitPathNotFound(source_path.to_string()));
    }

    // Determine next version number
    let next_version: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) + 1 FROM snapshots WHERE unit_id = ?1",
        params![unit.id],
        |row| row.get(0),
    )?;

    // Walk directory and build manifest
    let (total_size, file_count, manifest_entries) = walk_directory(source_path)?;

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
        "INSERT INTO manifest_entries (manifest_id, path, size_bytes, mtime, is_directory,
                                       mode, uid, gid, username, groupname, has_xattrs, has_acls,
                                       file_type, link_target)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
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
            0i32, // has_xattrs — populated on stage
            0i32, // has_acls
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

    Ok(snapshot_id)
}

/// Full stage pipeline: validate → dar → encrypt → checksums.
pub fn stage_create(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    snapshot_id: i64,
) -> Result<i64> {
    let snapshot = get_snapshot(conn, snapshot_id)?;
    let unit = get_unit_for_snapshot(conn, &snapshot)?;
    let tenant = queries::get_tenant_by_id(conn, unit.tenant_id)?
        .ok_or_else(|| TapectlError::Other("tenant not found".into()))?;

    let staging_dir = Path::new(&config.staging.directory);
    if !staging_dir.exists() {
        fs::create_dir_all(staging_dir)?;
    }

    // Check staging space (basic check)
    let source_size = snapshot.total_size.unwrap_or(0);
    check_staging_space(staging_dir, source_size)?;

    // Resolve slice size
    let slice_size = config.defaults.slice_size.clone();
    let compression = config.defaults.compression.clone();

    // Create stage_set record
    conn.execute(
        "INSERT INTO stage_sets (snapshot_id, slice_size, compression, encrypted)
         VALUES (?1, ?2, ?3, 1)",
        params![snapshot_id, parse_size_to_bytes(&slice_size), compression],
    )?;
    let stage_set_id = conn.last_insert_rowid();

    // Step 1: SHA256 source validation
    info!("validating source checksums");
    let checksums = validate::validate_source(conn, snapshot_id, &snapshot.source_path)?;

    conn.execute(
        "UPDATE stage_sets SET source_validated_at = datetime('now') WHERE id = ?1",
        params![stage_set_id],
    )?;

    // Step 2: Run dar
    let archive_base = staging_dir.join(format!(
        "{}_v{}",
        unit.uuid.replace('-', "").get(..12).unwrap_or(&unit.uuid),
        snapshot.version,
    ));

    let dar_result = dar::create::create_archive(&dar::create::DarCreateParams {
        dar_binary: &config.dar.binary,
        source_path: Path::new(&snapshot.source_path),
        archive_base: &archive_base,
        slice_size: &slice_size,
        compression: &compression,
        exclude_patterns: &config.defaults.global_excludes,
        exclude_paths: &[],
        preserve_xattrs: config.defaults.preserve_xattrs,
        preserve_acls: config.defaults.preserve_acls,
        preserve_fsa: config.defaults.preserve_fsa,
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

    let catalog_dir = paths.catalogs_dir.join(&unit.uuid[..8]);
    let catalog_base = catalog_dir.join(format!("{}_v{}", &unit.uuid[..8], snapshot.version));
    if existing_catalogs == 0 {
        info!("extracting dar catalog");
        dar::create::extract_catalog(&config.dar.binary, &archive_base, &catalog_base)?;
    }
    conn.execute(
        "UPDATE stage_sets SET catalog_path = ?1 WHERE id = ?2",
        params![catalog_base.to_string_lossy().to_string(), stage_set_id],
    )?;

    // Step 4: Encrypt slices
    info!("encrypting slices");
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
    // (no-op if none is registered yet, or if it's already present).
    let all_pubkeys = queries::recipient_list_with_escrow(conn, all_pubkeys)?;

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
        fs::remove_file(slice_path)?;

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

    // Also remove sha512 hash files that dar created
    if let Some(parent) = archive_base.parent() {
        if let Ok(entries) = fs::read_dir(parent) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".sha512")
                    && name.starts_with(
                        &archive_base
                            .file_name()
                            .unwrap()
                            .to_string_lossy()
                            .to_string(),
                    )
                {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
    }

    // Update stage_set
    conn.execute(
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
    let updated = conn.execute(
        "UPDATE snapshots SET status = 'staged' WHERE id = ?1 AND status = 'created'",
        params![snapshot_id],
    )?;
    if updated > 0 {
        events::log_field_change(
            conn,
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
        backfill_checksums(conn, snapshot_id, &checksums)?;
    }

    // Generate receipt
    let receipt = generate_receipt(conn, stage_set_id, &unit, &snapshot, &tenant)?;
    let receipt_path = paths.receipts_dir.join(format!(
        "{}_{}.txt",
        chrono::Utc::now().format("%Y%m%d"),
        stage_set_id
    ));
    fs::create_dir_all(&paths.receipts_dir)?;
    fs::write(&receipt_path, &receipt)?;

    events::log_created(
        conn,
        "stage_set",
        stage_set_id,
        &format!("{} v{}", unit.name, snapshot.version),
        Some(unit.tenant_id),
    )?;

    Ok(stage_set_id)
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

fn check_staging_space(staging_dir: &Path, source_size: i64) -> Result<()> {
    // Basic check: warn if available space is less than 3x source size
    // (dar slices + encrypted copies before cleanup)
    if let Ok(stat) = nix::sys::statvfs::statvfs(staging_dir) {
        let available = stat.blocks_available() as i64 * stat.block_size() as i64;
        let needed = source_size * 3;
        if available < needed {
            tracing::warn!(
                available_gb = available / (1024 * 1024 * 1024),
                needed_gb = needed / (1024 * 1024 * 1024),
                "staging space may be insufficient"
            );
        }
    }
    Ok(())
}

/// Build an `age::Encryptor` for the given recipient public keys — shared by
/// `encrypt_data` (small, buffered payloads: envelopes/manifests in
/// `src/volume/build.rs`) and `encrypt_file_streaming` (large, streamed
/// slices; H9 fix, issue #35), so the recipient parsing/boxing dance lives
/// in exactly one place. Pure extraction: same errors, same messages, same
/// order of operations as before — `age::Encryptor` doesn't retain any
/// reference into `pubkey_strings` or the intermediate boxed recipients
/// once constructed, so returning it by value is safe.
fn build_encryptor(pubkey_strings: &[String]) -> Result<age::Encryptor> {
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
    result
}

fn encrypt_file_streaming_inner(
    input_path: &Path,
    output_path: &Path,
    pubkey_strings: &[String],
) -> Result<EncryptedSliceInfo> {
    let encryptor = build_encryptor(pubkey_strings)?;

    let input = fs::File::open(input_path)?;
    let mut reader = HashingReader::new(input);

    let output = fs::File::create(output_path)?;
    let hashing_output = HashingWriter::new(output);
    let mut writer = encryptor
        .wrap_output(hashing_output)
        .map_err(|e| TapectlError::Encryption(format!("wrap_output failed: {e}")))?;

    let plain_size = stream_copy(&mut reader, &mut writer)?;
    let sha256_plain = reader.finalize_hex();

    // Mandatory: without this, the STREAM's final chunk (the one carrying
    // the "last chunk" flag) is never written, producing a file that
    // hashes fine but cannot be decrypted (age's own doc comment on
    // `StreamWriter::finish` says exactly this). Reached only once
    // `stream_copy` has streamed the *entire* plaintext without error — an
    // error above returns via `?` and never reaches this line, so a
    // partial stream is never finished into a falsely-valid file.
    let hashing_output = writer
        .finish()
        .map_err(|e| TapectlError::Encryption(format!("finish failed: {e}")))?;
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

pub fn parse_size_to_bytes(s: &str) -> i64 {
    let s = s.trim();
    let (num_str, suffix) = s
        .find(|c: char| c.is_alphabetic())
        .map(|i| (&s[..i], &s[i..]))
        .unwrap_or((s, ""));
    let num: f64 = num_str.parse().unwrap_or(0.0);
    let multiplier = match suffix.to_uppercase().as_str() {
        "K" | "KB" => 1024.0,
        "M" | "MB" => 1024.0 * 1024.0,
        "G" | "GB" => 1024.0 * 1024.0 * 1024.0,
        "T" | "TB" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => 1.0,
    };
    (num * multiplier) as i64
}

/// Walk a directory and collect manifest entries.
fn walk_directory(path: &str) -> Result<(i64, i64, Vec<ManifestEntry>)> {
    use std::os::unix::fs::MetadataExt;
    use walkdir::WalkDir;

    let base = Path::new(path);
    let mut entries = Vec::new();
    let mut total_size: i64 = 0;
    let mut file_count: i64 = 0;

    for entry in WalkDir::new(base).follow_links(false) {
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
            walk_directory(tmp.path().to_str().unwrap()).unwrap();

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

        let (_, _, entries) = walk_directory(tmp.path().to_str().unwrap()).unwrap();

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
            walk_directory(tmp.path().to_str().unwrap()).unwrap();

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
}
