use std::path::Path;
use std::process::Command;

use tracing::warn;

use crate::error::{Result, TapectlError};

/// `-O` (`--comparison-field`, ignore-owner) exists precisely because a
/// non-root restore cannot set stored ownership. Warn the operator, so a
/// restore that silently lands everything under the invoking user rather
/// than the archived owners does not go unnoticed (issue #51).
fn warn_if_non_root() {
    if !nix::unistd::geteuid().is_root() {
        warn!(
            "restoring as a non-root user: restored files will be owned by \
             the invoking user, not their archived owners"
        );
    }
}

/// Extract a dar archive to a destination directory.
///
/// KNOWN GAP (issues #50/#51, pending a decision): under `-Q` (no terminal)
/// with these flags, dar SILENTLY SKIPS a file that already exists at the
/// destination and still exits 0. Measured against dar 2.7.13: the stale
/// file is left in place, other files are extracted, stdout carries
/// `<path> not restored (user choice)`, and the skip is tallied under
/// `inode(s) ignored (excluded by filters)` — NOT under `not restored
/// (overwriting policy decision)`, which reads 0. Any future detector must
/// key off the right counter; the obvious one is the wrong one.
pub fn extract(dar_binary: &str, archive_base: &Path, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::create_dir_all(dest)?;
    warn_if_non_root();

    let output = Command::new(dar_binary)
        .arg("-x")
        .arg(archive_base)
        .arg("-R")
        .arg(dest)
        // -O is `--comparison-field` (ignore-owner), NOT "overwrite" -- it
        // tells dar not to consider stored user/group, which is what lets a
        // non-root restore succeed instead of failing to chown to the
        // archived owner. The old `// overwrite` comment here was simply
        // wrong (issue #51).
        .arg("-O")
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
///
/// Shares [`extract`]'s KNOWN GAP, and is the sharper end of it: if the
/// requested file already exists at the destination, dar leaves the stale
/// copy and exits 0, so the operator is told the single file they asked for
/// was restored when it was not (issues #50/#51, pending a decision).
pub fn extract_file(
    dar_binary: &str,
    archive_base: &Path,
    file_path: &str,
    dest: &Path,
) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    warn_if_non_root();

    let output = Command::new(dar_binary)
        .arg("-x")
        .arg(archive_base)
        .arg("-R")
        .arg(dest)
        .arg("-g")
        .arg(file_path)
        // -O is `--comparison-field` (ignore-owner): see `extract`.
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
