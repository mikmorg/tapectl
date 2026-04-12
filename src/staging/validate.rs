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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup_conn_with_snapshot(files: &[(&str, i64)]) -> (Connection, i64) {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        let schema = include_str!("../db/migrations/001_initial.sql");
        conn.execute_batch(schema).unwrap();

        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('op', 1, 'active')",
            [],
        )
        .unwrap();
        let tid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
             VALUES ('u1', 'u', ?1, 'mtime_size', 1, 'active')",
            [tid],
        )
        .unwrap();
        let uid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
             VALUES (?1, 1, 'full', 'current', '/tmp')",
            [uid],
        )
        .unwrap();
        let sid = conn.last_insert_rowid();

        for (path, size) in files {
            conn.execute(
                "INSERT INTO files (snapshot_id, path, size_bytes, is_directory)
                 VALUES (?1, ?2, ?3, 0)",
                params![sid, path, size],
            )
            .unwrap();
        }
        (conn, sid)
    }

    #[test]
    fn validate_source_happy_path() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), b"hello").unwrap();
        std::fs::write(tmp.path().join("b.bin"), b"world!!").unwrap();

        let (conn, sid) = setup_conn_with_snapshot(&[("a.txt", 5), ("b.bin", 7)]);
        let result = validate_source(&conn, sid, tmp.path().to_str().unwrap()).unwrap();
        assert_eq!(result.len(), 2);
        // Sha256 of "hello" is 2cf24d...
        let hello = result
            .iter()
            .find(|(p, _)| p == "a.txt")
            .expect("a.txt in results");
        assert_eq!(
            hello.1,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn validate_source_missing_file_errors() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("present.txt"), b"ok").unwrap();

        let (conn, sid) = setup_conn_with_snapshot(&[("present.txt", 2), ("missing.txt", 10)]);
        let err = validate_source(&conn, sid, tmp.path().to_str().unwrap())
            .err()
            .unwrap();
        let msg = format!("{err}");
        assert!(
            msg.contains("missing.txt"),
            "expected error to mention missing file, got: {msg}"
        );
    }

    #[test]
    fn validate_source_size_mismatch_errors() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("growing.txt"), b"actually longer").unwrap();

        let (conn, sid) = setup_conn_with_snapshot(&[("growing.txt", 3)]);
        let err = validate_source(&conn, sid, tmp.path().to_str().unwrap())
            .err()
            .unwrap();
        let msg = format!("{err}");
        assert!(msg.contains("size changed"), "got: {msg}");
    }

    #[test]
    fn validate_source_skips_directories() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("subdir")).unwrap();
        std::fs::write(tmp.path().join("subdir/f.txt"), b"x").unwrap();

        // Insert a directory row alongside the file — validate_source
        // must filter it out and not try to read it as a file.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        let schema = include_str!("../db/migrations/001_initial.sql");
        conn.execute_batch(schema).unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('op', 1, 'active')",
            [],
        )
        .unwrap();
        let tid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
             VALUES ('u1', 'u', ?1, 'mtime_size', 1, 'active')",
            [tid],
        )
        .unwrap();
        let uid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
             VALUES (?1, 1, 'full', 'current', '/tmp')",
            [uid],
        )
        .unwrap();
        let sid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO files (snapshot_id, path, size_bytes, is_directory)
             VALUES (?1, 'subdir', 0, 1)",
            [sid],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO files (snapshot_id, path, size_bytes, is_directory)
             VALUES (?1, 'subdir/f.txt', 1, 0)",
            [sid],
        )
        .unwrap();

        let result = validate_source(&conn, sid, tmp.path().to_str().unwrap()).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "subdir/f.txt");
    }
}
