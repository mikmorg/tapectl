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

/// dar's report of one `-x` invocation, VERBATIM (issue #306; ADR-0013's
/// "capture everything verbatim now, parse it later", on the content side).
///
/// Both streams byte for byte — dar prints file names, and a non-UTF-8 name
/// lossily rewritten is not the report — plus the argv and the exit status.
/// Handed back on EVERY path the process ran: a clean exit, a non-zero exit,
/// and the exit-0-but-INCOMPLETE case [`skipped_paths`] exists for, where
/// the "not restored (user choice)" lines in `stdout` are the only evidence.
/// Until this existed, success kept nothing and failure kept an excerpt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DarReport {
    /// The command line, program first. Lossily stringified for JSON —
    /// provenance, not evidence; the evidence is the two streams.
    pub argv: Vec<String>,
    /// `None` when dar was killed by a signal.
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl DarReport {
    /// `argv` as a JSON array, the spelling `mam_journal.tool_argv` uses.
    pub fn argv_json(&self) -> String {
        serde_json::Value::from(self.argv.clone()).to_string()
    }

    /// dar's own `N inode(s) restored` summary count, or `None` when the
    /// summary block is not in `stdout` (dar killed, or output not UTF-8).
    ///
    /// A convenience parsed from the verbatim text, never a substitute for
    /// it — and NOT the `inode(s) not restored (overwriting policy
    /// decision)` counter, which [`skipped_paths`] proves cannot be trusted.
    pub fn inodes_restored(&self) -> Option<i64> {
        inodes_restored(std::str::from_utf8(&self.stdout).ok()?)
    }
}

/// The `N inode(s) restored` line of a dar summary block, as a count.
fn inodes_restored(stdout: &str) -> Option<i64> {
    stdout.lines().find_map(|line| {
        line.trim()
            .strip_suffix("inode(s) restored")
            .and_then(|n| n.trim().parse().ok())
    })
}

/// Run `dar -x` with `args` and hand back BOTH its verbatim report and the
/// verdict. The report is `None` only when dar could not be spawned.
///
/// The verdict fails on a non-zero exit, and on the silent-skip case
/// (`fail_on_skipped`) even though dar exited 0.
fn run_extract(
    dar_binary: &str,
    what: &str,
    args: &[std::ffi::OsString],
    dest: &Path,
) -> (Option<DarReport>, Result<()>) {
    let argv = std::iter::once(dar_binary.to_string())
        .chain(args.iter().map(|a| a.to_string_lossy().into_owned()))
        .collect();
    let output = match Command::new(dar_binary).args(args).output() {
        Ok(o) => o,
        Err(e) => return (None, Err(TapectlError::Dar(e.to_string()))),
    };
    let report = DarReport {
        argv,
        exit_code: output.status.code(),
        stdout: output.stdout,
        stderr: output.stderr,
    };
    let verdict = if !output.status.success() {
        let stderr = String::from_utf8_lossy(&report.stderr);
        Err(TapectlError::Dar(format!("{what} failed: {stderr}")))
    } else {
        fail_on_skipped(&report.stdout, dest)
    };
    (Some(report), verdict)
}

/// Extract a dar archive to a destination directory.
///
/// Fails if dar skipped any file rather than overwriting it — see
/// [`skipped_paths`] for why that is not detectable the obvious way.
pub fn extract(dar_binary: &str, archive_base: &Path, dest: &Path) -> Result<()> {
    extract_reported(dar_binary, archive_base, dest).1
}

/// [`extract`], also handing back dar's [`DarReport`] whenever dar ran —
/// on success and on failure alike — for the `restores` row (issue #306).
pub fn extract_reported(
    dar_binary: &str,
    archive_base: &Path,
    dest: &Path,
) -> (Option<DarReport>, Result<()>) {
    if let Some(parent) = dest.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return (None, Err(e.into()));
        }
    }
    if let Err(e) = std::fs::create_dir_all(dest) {
        return (None, Err(e.into()));
    }
    warn_if_non_root();

    let args: Vec<std::ffi::OsString> = vec![
        "-x".into(),
        archive_base.into(),
        "-R".into(),
        dest.into(),
        // -O is `--comparison-field` (ignore-owner), NOT "overwrite" -- it
        // tells dar not to consider stored user/group, which is what lets a
        // non-root restore succeed instead of failing to chown to the
        // archived owner. The old `// overwrite` comment here was simply
        // wrong (issue #51).
        "-O".into(),
        "-Q".into(),
    ];
    run_extract(dar_binary, "dar -x", &args, dest)
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

    let args: Vec<std::ffi::OsString> = vec![
        "-x".into(),
        archive_base.into(),
        "-R".into(),
        dest.into(),
        "-g".into(),
        file_path.into(),
        // -O is `--comparison-field` (ignore-owner): see `extract`.
        "-O".into(),
        "-Q".into(),
    ];
    run_extract(dar_binary, "dar -x -g", &args, dest).1
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

    // ── the verbatim report (issue #306) ──

    /// The count comes from the `inode(s) restored` line and ONLY that line:
    /// the real sample has three other `inode(s) ... restored` counters on
    /// the lines around it, and 1 is the one that is right.
    #[test]
    fn inodes_restored_reads_the_one_summary_line() {
        assert_eq!(inodes_restored(REAL_DAR_SKIP_OUTPUT), Some(1));
        assert_eq!(inodes_restored(" 12 inode(s) restored\n"), Some(12));
        assert_eq!(
            inodes_restored(" 0 inode(s) not restored (overwriting policy decision)\n"),
            None,
            "the counter that cannot be trusted is not read as the count"
        );
        assert_eq!(inodes_restored(""), None);
        let killed = DarReport {
            argv: vec!["dar".into()],
            exit_code: None,
            stdout: b"\xff\xfe".to_vec(),
            stderr: Vec::new(),
        };
        assert_eq!(killed.inodes_restored(), None, "not UTF-8: no count, no panic");
    }

    #[test]
    fn argv_json_is_a_json_array_program_first() {
        let r = DarReport {
            argv: vec!["dar".into(), "-x".into(), "/d/a b".into()],
            exit_code: Some(0),
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        assert_eq!(r.argv_json(), r#"["dar","-x","/d/a b"]"#);
    }

    /// POSITIVE CONTROL for the `restores` row's report columns: a clean
    /// extract by the real `dar` hands back a report whose stdout is
    /// non-empty and carries the summary, so a row with an empty report is
    /// a capture failure and not "dar says nothing on success". And the
    /// verdict is `Ok` — the report is handed back on the success path,
    /// which is the path that kept nothing before.
    ///
    /// Requires `dar`, like `real_dar_collision_is_reported_as_a_failed_restore`
    /// below (issue #43).
    #[test]
    fn real_dar_clean_extract_hands_back_a_non_empty_report() {
        let dar = "dar";
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dest = tmp.path().join("dest");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.txt"), b"A").unwrap();
        std::fs::write(src.join("b.txt"), b"B").unwrap();
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

        let (report, verdict) = extract_reported(dar, &archive_base, &dest);
        verdict.expect("a clean extract");
        let report = report.expect("dar ran, so there is a report");
        assert_eq!(report.exit_code, Some(0));
        assert_eq!(report.argv[0], "dar");
        assert_eq!(report.argv[1], "-x");
        assert!(!report.stdout.is_empty(), "dar's summary is on stdout");
        assert_eq!(report.inodes_restored(), Some(2), "{:?}", String::from_utf8_lossy(&report.stdout));
        assert_eq!(std::fs::read(dest.join("a.txt")).unwrap(), b"A");
    }

    /// The failure path keeps the report too — the whole point (issue #306:
    /// "kept five lines of stderr on failure and nothing on success").
    #[test]
    fn real_dar_collision_still_hands_back_the_report() {
        let dar = "dar";
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dest = tmp.path().join("dest");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(src.join("collide.txt"), b"ARCHIVED").unwrap();
        let archive_base = tmp.path().join("arch");
        assert!(Command::new(dar)
            .arg("-c")
            .arg(&archive_base)
            .arg("-R")
            .arg(&src)
            .arg("-Q")
            .output()
            .unwrap()
            .status
            .success());
        std::fs::write(dest.join("collide.txt"), b"STALE").unwrap();

        let (report, verdict) = extract_reported(dar, &archive_base, &dest);
        assert!(verdict.is_err(), "a collision is a failed restore");
        let report = report.expect("dar ran");
        assert_eq!(report.exit_code, Some(0), "dar itself exited clean — that is the trap");
        assert!(
            String::from_utf8_lossy(&report.stdout).contains(SKIPPED_MARKER),
            "the evidence is in the verbatim stdout"
        );
    }

    /// End-to-end against the real `dar`, because the tests above only
    /// exercise the parser — they would all still pass if `extract` never
    /// called `fail_on_skipped` at all. This one pins the WIRING, which is
    /// the part that actually protects a restore.
    ///
    /// The mhvtl gate does not cover this: its restore legs extract into
    /// fresh destinations, so they never produce a collision.
    ///
    /// Requires `dar`, like the ten staging-pipeline tests it now sits
    /// alongside (issue #43). This test used to carry its own skip guard,
    /// justified as "keeping the ungated suite free of external-binary
    /// requirements" — a premise the #43 audit disproved: the suite already
    /// required dar in ten other places, and CI had been red for months
    /// because of it. `tests/test_dependencies.rs` now asserts the
    /// dependency once, by name; a bespoke skip here would be the only one
    /// of thirteen dar-dependent tests that quietly passes without dar,
    /// which is worse than either consistent policy.
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
        assert!(
            have_dar,
            "`dar` not on PATH — see tests/test_dependencies.rs, which reports \
             this dependency once with instructions"
        );

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
