use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
use tracing::info;

use crate::config::{Config, TapectlPaths};
use crate::crypto::keys;
use crate::dar;
use crate::db::queries;
use crate::error::{Result, TapectlError};
use crate::tape::ioctl::TapeDevice;

/// Restore a unit from a volume to a destination directory.
pub fn restore_unit(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    unit_name: &str,
    volume_label: &str,
    dest_dir: &str,
    device: &str,
    block_size: usize,
    dry_run: bool,
) -> Result<RestoreReport> {
    let unit = queries::get_unit_by_name(conn, unit_name)?
        .ok_or_else(|| TapectlError::UnitNotFound(unit_name.to_string()))?;

    let tenant = queries::get_tenant_by_id(conn, unit.tenant_id)?
        .ok_or_else(|| TapectlError::Other("tenant not found".into()))?;

    // Find write positions for this unit on this volume
    let positions = get_write_positions(conn, unit.id, volume_label)?;
    if positions.is_empty() {
        return Err(TapectlError::Other(format!(
            "no data for unit \"{unit_name}\" on volume \"{volume_label}\""
        )));
    }

    if dry_run {
        return Ok(RestoreReport {
            unit_name: unit_name.to_string(),
            volume_label: volume_label.to_string(),
            slices: positions.len(),
            destination: dest_dir.to_string(),
            dry_run: true,
            success: true,
        });
    }

    // Create temp dir for decrypted slices
    let restore_tmp = Path::new(dest_dir).join(".tapectl-restore-tmp");
    fs::create_dir_all(&restore_tmp)?;

    // Find the tenant's secret key for decryption
    let key_path = paths
        .keys_dir
        .join(format!("{}-primary.age.key", tenant.name));
    let secret_key_str = keys::read_secret_key(&key_path)?;
    let identity: age::x25519::Identity = secret_key_str
        .parse()
        .map_err(|e| TapectlError::Encryption(format!("invalid key: {e}")))?;

    // Open tape and read each slice
    let mut tape = TapeDevice::open_read(device, block_size)?;

    let mut dar_slices: Vec<PathBuf> = Vec::new();

    for (i, wp) in positions.iter().enumerate() {
        let tape_pos: i32 = wp.position.parse().unwrap_or(0);

        info!(
            slice = i + 1,
            total = positions.len(),
            tape_pos = tape_pos,
            "reading slice from tape"
        );

        // Seek to position
        tape.rewind()?;
        if tape_pos > 0 {
            tape.forward_space_file(tape_pos)?;
        }

        // Read encrypted data
        let enc_data = tape.read_file()?;

        // Trim padding to original size
        let trimmed = if (wp.encrypted_bytes as usize) < enc_data.len() {
            &enc_data[..wp.encrypted_bytes as usize]
        } else {
            &enc_data
        };

        // Verify checksum
        let hash = sha256_hex(trimmed);
        if hash != wp.sha256_encrypted {
            return Err(TapectlError::Other(format!(
                "slice {} checksum mismatch on tape: expected {}..., got {}...",
                wp.slice_number,
                &wp.sha256_encrypted[..16],
                &hash[..16],
            )));
        }

        // Decrypt
        let decryptor = age::Decryptor::new(trimmed)
            .map_err(|e| TapectlError::Encryption(format!("decryptor: {e}")))?;

        let mut reader = decryptor
            .decrypt(std::iter::once(&identity as &dyn age::Identity))
            .map_err(|e| TapectlError::Encryption(format!("decrypt: {e}")))?;

        let mut plaintext = Vec::new();
        reader
            .read_to_end(&mut plaintext)
            .map_err(|e| TapectlError::Encryption(format!("read: {e}")))?;

        // Verify plaintext checksum
        let plain_hash = sha256_hex(&plaintext);
        if plain_hash != wp.sha256_plain {
            return Err(TapectlError::Other(format!(
                "slice {} decrypted checksum mismatch",
                wp.slice_number,
            )));
        }

        // Write decrypted dar slice to temp dir
        // dar expects: basename.N.dar
        let slice_name = format!("restore.{}.dar", wp.slice_number);
        let slice_path = restore_tmp.join(&slice_name);
        fs::write(&slice_path, &plaintext)?;
        dar_slices.push(slice_path);

        info!(
            slice = i + 1,
            mb = plaintext.len() / (1024 * 1024),
            "decrypted slice"
        );
    }

    // Run dar extract
    let archive_base = restore_tmp.join("restore");
    info!("extracting dar archive to {dest_dir}");
    dar::restore::extract(&config.dar.binary, &archive_base, Path::new(dest_dir))?;

    // Clean up temp files
    for path in &dar_slices {
        let _ = fs::remove_file(path);
    }
    // Remove hash files too
    if let Ok(entries) = fs::read_dir(&restore_tmp) {
        for entry in entries.flatten() {
            let _ = fs::remove_file(entry.path());
        }
    }
    let _ = fs::remove_dir(&restore_tmp);

    info!(unit = unit_name, volume = volume_label, "restore complete");

    Ok(RestoreReport {
        unit_name: unit_name.to_string(),
        volume_label: volume_label.to_string(),
        slices: positions.len(),
        destination: dest_dir.to_string(),
        dry_run: false,
        success: true,
    })
}

/// Restore a single file from a unit on a volume.
pub fn restore_file(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    unit_name: &str,
    file_path: &str,
    volume_label: &str,
    dest_dir: &str,
    device: &str,
    block_size: usize,
) -> Result<()> {
    // First do a full restore to a temp dir, then extract the single file
    let tmp = tempfile::tempdir().map_err(|e| TapectlError::Other(e.to_string()))?;
    let tmp_path = tmp.path().to_string_lossy().to_string();

    restore_unit(
        conn,
        paths,
        config,
        unit_name,
        volume_label,
        &tmp_path,
        device,
        block_size,
        false,
    )?;

    // Copy the requested file to dest_dir
    let source_file = tmp.path().join(file_path);
    if !source_file.exists() {
        return Err(TapectlError::Other(format!(
            "file \"{file_path}\" not found in restored unit"
        )));
    }

    let dest = Path::new(dest_dir).join(
        Path::new(file_path)
            .file_name()
            .unwrap_or(std::ffi::OsStr::new(file_path)),
    );
    fs::create_dir_all(Path::new(dest_dir))?;
    fs::copy(&source_file, &dest)?;

    info!(file = file_path, dest = %dest.display(), "file restored");
    Ok(())
}

#[derive(Debug)]
pub struct RestoreReport {
    pub unit_name: String,
    pub volume_label: String,
    pub slices: usize,
    pub destination: String,
    pub dry_run: bool,
    #[allow(dead_code)]
    pub success: bool,
}

struct WritePositionInfo {
    slice_number: i64,
    position: String,
    sha256_plain: String,
    sha256_encrypted: String,
    encrypted_bytes: i64,
}

fn get_write_positions(
    conn: &Connection,
    unit_id: i64,
    volume_label: &str,
) -> Result<Vec<WritePositionInfo>> {
    let mut stmt = conn.prepare(
        "SELECT sl.slice_number, wp.position, sl.sha256_plain, sl.sha256_encrypted, sl.encrypted_bytes
         FROM write_positions wp
         JOIN writes w ON w.id = wp.write_id
         JOIN stage_slices sl ON sl.id = wp.stage_slice_id
         JOIN stage_sets ss ON ss.id = sl.stage_set_id
         JOIN snapshots s ON s.id = ss.snapshot_id
         JOIN volumes v ON v.id = w.volume_id
         WHERE s.unit_id = ?1 AND v.label = ?2 AND w.status = 'completed' AND wp.status = 'written'
         ORDER BY sl.slice_number",
    )?;

    let rows = stmt
        .query_map(params![unit_id, volume_label], |row| {
            Ok(WritePositionInfo {
                slice_number: row.get(0)?,
                position: row.get(1)?,
                sha256_plain: row.get(2)?,
                sha256_encrypted: row.get(3)?,
                encrypted_bytes: row.get(4)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    Ok(rows)
}

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
