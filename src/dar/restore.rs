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

/// dar's marker, on stdout, for a file it declined to overwrite.
const SKIPPED_MARKER: &str = "not restored (user choice)";

/// Paths dar silently declined to restore because something already
/// existed at the destination.
///
/// EMPIRICAL BASIS (dar 2.7.13, issues #50/#51, ratified 2026-07-31).
/// Under `-Q` there is no terminal, so dar answers its own overwrite
/// prompt with "no" — it leaves the stale file, extracts everything else,
/// and **exits 0**. The operator is told the restore succeeded.
///
/// Two traps make this worth pinning in code:
///
/// 1. The obvious counter is the wrong one. The skip is tallied under
///    `inode(s) ignored (excluded by filters)`, while `inode(s) not
///    restored (overwriting policy decision)` — the line whose name
///    matches the situation exactly — stays **0**. A detector keyed off
///    that counter would never fire.
/// 2. The signal is on **stdout**, not stderr (verified by stream).
///
/// So we key off the per-file line, which names the path and is the only
/// place the specific casualty appears.
fn skipped_paths(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            line.strip_suffix(SKIPPED_MARKER)
                .map(|path| path.trim().to_string())
                .filter(|path| !path.is_empty())
        })
        .collect()
}

/// Turn a silent partial restore into a loud failure (issue #51).
fn fail_on_skipped(stdout: &[u8], dest: &Path) -> Result<()> {
    let skipped = skipped_paths(&String::from_utf8_lossy(stdout));
    if skipped.is_empty() {
        return Ok(());
    }
    Err(TapectlError::Other(format!(
        "restore into \"{}\" is INCOMPLETE: dar declined to overwrite {} file(s) that already \
         existed, and would otherwise have reported success. The stale copies are still in \
         place — the restored data is NOT what is on tape. Skipped: {}. Restore into an empty \
         directory, or remove those files first.",
        dest.display(),
        skipped.len(),
        skipped.join(", "),
    )))
}

/// Extract a dar archive to a destination directory.
///
/// Fails if dar skipped any file rather than overwriting it — see
/// [`skipped_paths`] for why that is not detectable the obvious way.
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
    fail_on_skipped(&output.stdout, dest)
}

/// Extract a single file from a dar archive.
///
/// Guarded the same way as [`extract`], and it is the sharper end of the
/// same hazard: the operator asked for exactly one file, so a silent skip
/// means the one thing they requested is the one thing they did not get.
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
    fail_on_skipped(&output.stdout, dest)
}

/// Test a dar archive integrity.
pub fn test(dar_binary: &str, archive_base: &Path) -> Result<()> {
    super::create::test_archive(dar_binary, archive_base)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verbatim capture from dar 2.7.13 (`dar -x arch -R dest -O -Q`, one
    /// colliding file present). Kept literal rather than hand-written:
    /// this parser's whole job is to survive dar's real wording, and the
    /// counter that looks authoritative here is the one that stays 0.
    const REAL_DAR_SKIP_OUTPUT: &str = "\
/tmp/dest/collide.txt not restored (user choice)


 --------------------------------------------
 1 inode(s) restored
    including 0 hard link(s)
 0 inode(s) not restored (not saved in archive)
 0 inode(s) not restored (overwriting policy decision)
 1 inode(s) ignored (excluded by filters)
 0 inode(s) failed to restore (filesystem error)
 0 inode(s) deleted
 --------------------------------------------
 Total number of inode(s) considered: 2
";

    #[test]
    fn detects_a_real_dar_skip_and_names_the_path() {
        let skipped = skipped_paths(REAL_DAR_SKIP_OUTPUT);
        assert_eq!(skipped, vec!["/tmp/dest/collide.txt".to_string()]);
    }

    /// The trap that makes this parser necessary: the summary block claims
    /// `0 inode(s) not restored (overwriting policy decision)` even though
    /// a file was demonstrably not restored. Any detector keyed off that
    /// counter reports a clean restore. Pin it so nobody "simplifies" the
    /// parser into reading the summary.
    #[test]
    fn the_overwriting_policy_counter_is_zero_and_must_not_be_trusted() {
        assert!(
            REAL_DAR_SKIP_OUTPUT.contains("0 inode(s) not restored (overwriting policy decision)")
        );
        assert!(
            !skipped_paths(REAL_DAR_SKIP_OUTPUT).is_empty(),
            "a file WAS skipped despite that counter reading 0"
        );
    }

    #[test]
    fn a_clean_restore_reports_nothing_skipped() {
        let clean = " --------------------------------------------\n \
                      2 inode(s) restored\n \
                      0 inode(s) not restored (overwriting policy decision)\n";
        assert!(skipped_paths(clean).is_empty());
    }

    #[test]
    fn several_skips_are_all_reported() {
        let out = "/d/a.txt not restored (user choice)\n\
                   /d/b.txt not restored (user choice)\n \
                   1 inode(s) restored\n";
        assert_eq!(skipped_paths(out), vec!["/d/a.txt", "/d/b.txt"]);
    }

    #[test]
    fn fail_on_skipped_errors_and_names_destination_and_casualties() {
        let err = fail_on_skipped(REAL_DAR_SKIP_OUTPUT.as_bytes(), Path::new("/tmp/dest"))
            .expect_err("a skipped file must fail the restore");
        let msg = err.to_string();
        assert!(
            msg.contains("/tmp/dest"),
            "must name the destination: {msg}"
        );
        assert!(msg.contains("collide.txt"), "must name the casualty: {msg}");
        assert!(
            msg.contains("INCOMPLETE"),
            "must not read as a warning: {msg}"
        );
    }

    #[test]
    fn fail_on_skipped_passes_a_clean_restore() {
        assert!(fail_on_skipped(b" 2 inode(s) restored\n", Path::new("/tmp/dest")).is_ok());
    }

    /// End-to-end against the real `dar`, because the tests above only
    /// exercise the parser — they would all still pass if `extract` never
    /// called `fail_on_skipped` at all. This one pins the WIRING, which is
    /// the part that actually protects a restore.
    ///
    /// The mhvtl gate does not cover this: its restore legs extract into
    /// fresh destinations, so they never produce a collision.
    ///
    /// Skips when `dar` is absent rather than failing — the rest of the
    /// ungated suite deliberately needs no external binaries, and this
    /// single test should not change that contract.
    #[test]
    fn real_dar_collision_is_reported_as_a_failed_restore() {
        use std::process::Stdio;
        let dar = "dar";
        let have_dar = Command::new(dar)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !have_dar {
            eprintln!("SKIP real_dar_collision_is_reported_as_a_failed_restore: no dar binary");
            return;
        }

        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dest = tmp.path().join("dest");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(src.join("collide.txt"), b"ARCHIVED").unwrap();
        std::fs::write(src.join("other.txt"), b"other").unwrap();

        let archive_base = tmp.path().join("arch");
        let created = Command::new(dar)
            .arg("-c")
            .arg(&archive_base)
            .arg("-R")
            .arg(&src)
            .arg("-Q")
            .output()
            .unwrap();
        assert!(created.status.success(), "dar -c failed in test setup");

        // Pre-place a STALE copy of one archived file.
        std::fs::write(dest.join("collide.txt"), b"STALE").unwrap();

        let err = extract(dar, &archive_base, &dest)
            .expect_err("a collision must fail the restore, not report success");
        let msg = err.to_string();
        assert!(
            msg.contains("collide.txt"),
            "must name the file that was not restored: {msg}"
        );

        // And prove the failure was warranted: the stale bytes really did
        // survive, which is exactly what dar reported success for before.
        assert_eq!(
            std::fs::read(dest.join("collide.txt")).unwrap(),
            b"STALE",
            "dar left the stale copy in place -- this is the silent data loss being caught"
        );
    }
}
