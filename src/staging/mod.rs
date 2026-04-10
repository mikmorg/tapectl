pub mod clean;
pub mod validate;

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
use tracing::info;

use crate::config::{Config, TapectlPaths};
use crate::dar;
use crate::db::{events, models, queries};
use crate::error::{Result, TapectlError};

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
        "INSERT INTO files (snapshot_id, path, size_bytes, modified_at, is_directory)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    let mut manifest_insert = conn.prepare(
        "INSERT INTO manifest_entries (manifest_id, path, size_bytes, mtime, is_directory,
                                       mode, uid, gid, username, groupname, has_xattrs, has_acls)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
    )?;

    for entry in &manifest_entries {
        file_insert.execute(params![
            snapshot_id,
            entry.path,
            entry.size,
            entry.mtime,
            entry.is_dir,
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
    let operator = queries::get_operator_tenant(conn)?
        .ok_or_else(|| TapectlError::Other("no operator tenant".into()))?;
    let operator_keys = queries::get_active_keys_for_tenant(conn, operator.id)?;

    let all_pubkeys: Vec<String> = tenant_keys
        .iter()
        .chain(operator_keys.iter())
        .map(|k| k.public_key.clone())
        .collect();

    let key_fingerprints: Vec<String> = tenant_keys
        .iter()
        .chain(operator_keys.iter())
        .map(|k| k.fingerprint.clone())
        .collect();

    let mut total_dar_size: i64 = 0;
    let mut total_encrypted_size: i64 = 0;

    for (i, slice_path) in dar_result.slice_paths.iter().enumerate() {
        let slice_num = (i + 1) as i64;
        let plain_data = fs::read(slice_path)?;
        let plain_size = plain_data.len() as i64;
        let sha256_plain = sha256_hex(&plain_data);

        let encrypted_path = PathBuf::from(format!("{}.age", slice_path.display()));
        let encrypted_data = encrypt_data(&plain_data, &all_pubkeys)?;
        let encrypted_size = encrypted_data.len() as i64;
        let sha256_encrypted = sha256_hex(&encrypted_data);

        fs::write(&encrypted_path, &encrypted_data)?;

        // Remove unencrypted slice
        fs::remove_file(slice_path)?;

        conn.execute(
            "INSERT INTO stage_slices (stage_set_id, slice_number, size_bytes, encrypted_bytes,
                                       sha256_plain, sha256_encrypted, staging_path)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                stage_set_id,
                slice_num,
                plain_size,
                encrypted_size,
                sha256_plain,
                sha256_encrypted,
                encrypted_path.to_string_lossy().to_string(),
            ],
        )?;

        total_dar_size += plain_size;
        total_encrypted_size += encrypted_size;

        info!(
            slice = slice_num,
            plain_mb = plain_size / (1024 * 1024),
            encrypted_mb = encrypted_size / (1024 * 1024),
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
    conn.execute(
        "UPDATE snapshots SET status = 'staged' WHERE id = ?1 AND status = 'created'",
        params![snapshot_id],
    )?;

    // Backfill sha256 into files and manifest_entries (first stage only)
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

pub fn encrypt_data(data: &[u8], pubkey_strings: &[String]) -> Result<Vec<u8>> {
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

    let encryptor = age::Encryptor::with_recipients(
        recipient_refs
            .iter()
            .map(|r| r.as_ref() as &dyn age::Recipient),
    )
    .map_err(|e| TapectlError::Encryption(format!("failed to create encryptor: {e}")))?;

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

fn sha256_hex(data: &[u8]) -> String {
    let hash = Sha256::digest(data);
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

fn backfill_checksums(
    conn: &Connection,
    snapshot_id: i64,
    checksums: &[(String, String)],
) -> Result<()> {
    let mut file_update =
        conn.prepare("UPDATE files SET sha256 = ?1 WHERE snapshot_id = ?2 AND path = ?3")?;
    let mut manifest_update = conn.prepare(
        "UPDATE manifest_entries SET sha256 = ?1
         WHERE manifest_id = (SELECT id FROM manifests WHERE snapshot_id = ?2 LIMIT 1)
         AND path = ?3",
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
        let size = if is_dir { 0 } else { meta.len() as i64 };
        let mtime = chrono::DateTime::from_timestamp(meta.mtime(), 0)
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_default();

        if !is_dir {
            total_size += size;
            file_count += 1;
        }

        entries.push(ManifestEntry {
            path: rel_path,
            size,
            mtime,
            is_dir,
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
    mode: Option<i64>,
    uid: Option<i64>,
    gid: Option<i64>,
    username: Option<String>,
    groupname: Option<String>,
}
