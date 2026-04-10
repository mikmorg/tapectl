use std::path::{Path, PathBuf};
use std::process::Command;

use tracing::info;

use crate::error::{Result, TapectlError};

/// Parameters for a dar archive creation.
pub struct DarCreateParams<'a> {
    pub dar_binary: &'a str,
    pub source_path: &'a Path,
    pub archive_base: &'a Path,
    pub slice_size: &'a str,
    pub compression: &'a str,
    pub exclude_patterns: &'a [String],
    pub exclude_paths: &'a [String],
    pub preserve_xattrs: bool,
    pub preserve_acls: bool,
    pub preserve_fsa: bool,
}

/// Result of a dar archive creation.
pub struct DarCreateResult {
    pub dar_version: String,
    pub dar_command: String,
    pub slice_paths: Vec<PathBuf>,
    pub num_slices: usize,
}

/// Create a dar archive.
pub fn create_archive(params: &DarCreateParams) -> Result<DarCreateResult> {
    let ver = super::version::check(params.dar_binary)?;

    let mut cmd = Command::new(params.dar_binary);
    cmd.arg("-c").arg(params.archive_base);
    cmd.arg("-R").arg(params.source_path);
    cmd.arg("-s").arg(params.slice_size);

    if params.compression != "none" {
        cmd.arg("-z").arg(params.compression);
    }

    cmd.arg("-an"); // case-insensitive masks
    cmd.arg("-D"); // store excluded dirs as empty
    cmd.arg("-3").arg("sha512"); // slice hashing
    cmd.arg("-Q"); // quiet (no tty prompt)

    if params.preserve_xattrs {
        cmd.arg("-am");
    }
    // ACLs are preserved via -am (xattrs include POSIX ACLs in dar 2.7.x)
    if params.preserve_fsa {
        cmd.arg("--fsa-scope").arg("extX");
    }

    for pattern in params.exclude_patterns {
        cmd.arg("-X").arg(pattern);
    }
    for path in params.exclude_paths {
        cmd.arg("-P").arg(path);
    }

    let command_str = format!("{cmd:?}");
    info!(command = %command_str, "running dar");

    let output = cmd.output().map_err(|e| TapectlError::Dar(e.to_string()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(TapectlError::Dar(format!(
            "dar -c failed (exit {}): {}",
            output.status,
            stderr.lines().take(5).collect::<Vec<_>>().join("\n")
        )));
    }

    let slices = list_slices(params.archive_base)?;
    let num_slices = slices.len();

    Ok(DarCreateResult {
        dar_version: ver.full_string,
        dar_command: command_str,
        slice_paths: slices,
        num_slices,
    })
}

/// List dar slice files for an archive base path.
pub fn list_slices(archive_base: &Path) -> Result<Vec<PathBuf>> {
    let dir = archive_base
        .parent()
        .ok_or_else(|| TapectlError::Dar("invalid archive base path".to_string()))?;
    let stem = archive_base
        .file_name()
        .ok_or_else(|| TapectlError::Dar("invalid archive base path".to_string()))?
        .to_string_lossy();

    let mut slices: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().unwrap_or_default().to_string_lossy();
            name.starts_with(stem.as_ref()) && name.ends_with(".dar")
        })
        .collect();

    slices.sort();
    Ok(slices)
}

/// Run dar -t (test archive integrity).
pub fn test_archive(dar_binary: &str, archive_base: &Path) -> Result<()> {
    let output = Command::new(dar_binary)
        .arg("-t")
        .arg(archive_base)
        .arg("-Q")
        .output()
        .map_err(|e| TapectlError::Dar(e.to_string()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(TapectlError::Dar(format!("dar -t failed: {stderr}")));
    }
    Ok(())
}

/// Extract isolated catalog from archive.
pub fn extract_catalog(dar_binary: &str, archive_base: &Path, catalog_base: &Path) -> Result<()> {
    if let Some(parent) = catalog_base.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let output = Command::new(dar_binary)
        .arg("-C")
        .arg(catalog_base)
        .arg("-A")
        .arg(archive_base)
        .arg("-Q")
        .output()
        .map_err(|e| TapectlError::Dar(e.to_string()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(TapectlError::Dar(format!("dar -C failed: {stderr}")));
    }
    Ok(())
}
