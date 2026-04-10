use std::path::Path;

use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
use tracing::info;

use crate::error::{Result, TapectlError};

/// Validate source files by computing SHA256 for all files in the snapshot.
/// Returns Vec<(relative_path, sha256_hex)>.
pub fn validate_source(
    conn: &Connection,
    snapshot_id: i64,
    source_path: &str,
) -> Result<Vec<(String, String)>> {
    let base = Path::new(source_path);

    // Get all non-directory files from the manifest
    let mut stmt = conn.prepare(
        "SELECT path, size_bytes FROM files WHERE snapshot_id = ?1 AND is_directory = 0",
    )?;
    let files: Vec<(String, i64)> = stmt
        .query_map(params![snapshot_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let total_files = files.len();
    let total_bytes: i64 = files.iter().map(|(_, s)| s).sum();
    info!(
        files = total_files,
        total_mb = total_bytes / (1024 * 1024),
        "validating source checksums"
    );

    let mut checksums = Vec::new();
    let mut validated = 0;

    for (rel_path, expected_size) in &files {
        let full_path = base.join(rel_path);

        if !full_path.exists() {
            return Err(TapectlError::Other(format!(
                "source file missing: {rel_path}"
            )));
        }

        let data = std::fs::read(&full_path)?;
        if data.len() as i64 != *expected_size {
            return Err(TapectlError::Other(format!(
                "source file size changed: {rel_path} (expected {expected_size}, got {})",
                data.len()
            )));
        }

        let hash = Sha256::digest(&data);
        let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        checksums.push((rel_path.clone(), hex));

        validated += 1;
        if validated % 100 == 0 {
            info!(
                progress = format!("{validated}/{total_files}"),
                "validating"
            );
        }
    }

    info!(files = validated, "source validation complete");
    Ok(checksums)
}
