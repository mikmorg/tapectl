use std::path::Path;
use std::process::Command;

use crate::error::{Result, TapectlError};

/// Extract a dar archive to a destination directory.
pub fn extract(dar_binary: &str, archive_base: &Path, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::create_dir_all(dest)?;

    let output = Command::new(dar_binary)
        .arg("-x")
        .arg(archive_base)
        .arg("-R")
        .arg(dest)
        .arg("-O") // overwrite
        .arg("-Q")
        .output()
        .map_err(|e| TapectlError::Dar(e.to_string()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(TapectlError::Dar(format!("dar -x failed: {stderr}")));
    }
    Ok(())
}

/// Extract a single file from a dar archive.
pub fn extract_file(
    dar_binary: &str,
    archive_base: &Path,
    file_path: &str,
    dest: &Path,
) -> Result<()> {
    std::fs::create_dir_all(dest)?;

    let output = Command::new(dar_binary)
        .arg("-x")
        .arg(archive_base)
        .arg("-R")
        .arg(dest)
        .arg("-g")
        .arg(file_path)
        .arg("-O")
        .arg("-Q")
        .output()
        .map_err(|e| TapectlError::Dar(e.to_string()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(TapectlError::Dar(format!("dar -x -g failed: {stderr}")));
    }
    Ok(())
}

/// Test a dar archive integrity.
pub fn test(dar_binary: &str, archive_base: &Path) -> Result<()> {
    super::create::test_archive(dar_binary, archive_base)
}
