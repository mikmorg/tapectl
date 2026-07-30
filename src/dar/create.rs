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
    #[allow(dead_code)]
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
        // dar's `-z` takes an OPTIONAL argument, so getopt only sees it when
        // it is glued to the flag. Passing `-z gzip` as two argv tokens makes
        // dar read the algorithm as a user target and abort with
        // "Given user target(s) could not be found: gzip". Latent until
        // issue #92 made archive-set compression reachable at all.
        cmd.arg(format!("-z{}", params.compression));
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

/// Parses `name` as a dar slice filename for `stem`, matching exactly
/// `{stem}.<digits>.dar` — anchored on both ends via `strip_prefix`/
/// `strip_suffix` rather than a substring/prefix test, so lookalikes don't
/// slip through: `{stem}_old.dar` and `{stem}2.4.dar` (a longer, different
/// stem) fail at the mandatory separating dot; `{stem}.backup.dar` and
/// `{stem}.notanumber.dar` fail at the numeric parse. Returns `None` for
/// anything that isn't an exact match; `list_slices` treats `None` as "not
/// a slice of this archive," not an error.
fn parse_slice_number(name: &str, stem: &str) -> Option<u32> {
    let rest = name.strip_prefix(stem)?;
    let rest = rest.strip_prefix('.')?;
    let digits = rest.strip_suffix(".dar")?;
    digits.parse().ok()
}

/// List dar slice files for an archive base path.
///
/// Matches only dar's own exact naming, `{stem}.<N>.dar` — dar numbers
/// slices from 1 (confirmed against the dar man page:
/// docs/research/2026-07-21-ontape-format-and-write-design.md §F1) — and
/// returns them ordered by the *parsed* integer N, not lexicographic
/// filename order. Lexicographic order sorts "base.10.dar" before
/// "base.2.dar", desyncing from dar's real numbering once an archive
/// reaches 10 slices; every individual slice still passes its own
/// checksum, so the corruption this produces downstream is entirely
/// silent (issue #34/H8).
///
/// The returned Vec's *position*, not just its order, is load-bearing:
/// `staging::stage_create` derives each `stage_slices.slice_number` as
/// `index + 1` against this Vec (src/staging/mod.rs), rather than
/// re-parsing the filename itself. That means this function must return a
/// clean `1..=N` run — no gap, no duplicate, no stray zero (dar counts
/// from 1, so a `.0.` slice is already out of range) — or the index-based
/// slice_number the caller records silently stops matching dar's real
/// slice index. Any deviation errors loudly instead of guessing an order:
/// proceeding quietly is exactly the failure mode #34 exists to close. An
/// empty result (nothing matched at all) errors the same way rather than
/// reporting a silent zero-slice archive — this function is only ever
/// called right after a successful `dar -c`, which always emits >=1 slice,
/// so finding none means something upstream is already wrong.
pub fn list_slices(archive_base: &Path) -> Result<Vec<PathBuf>> {
    let dir = archive_base
        .parent()
        .ok_or_else(|| TapectlError::Dar("invalid archive base path".to_string()))?;
    let stem = archive_base
        .file_name()
        .ok_or_else(|| TapectlError::Dar("invalid archive base path".to_string()))?
        .to_string_lossy();

    let mut numbered: Vec<(u32, PathBuf)> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter_map(|p| {
            let name = p.file_name()?.to_string_lossy();
            let n = parse_slice_number(&name, &stem)?;
            Some((n, p))
        })
        .collect();

    numbered.sort_by_key(|(n, _)| *n);

    if numbered.is_empty() {
        return Err(TapectlError::Dar(format!(
            "no dar slices found for archive \"{stem}\" in {} — expected at \
             least \"{stem}.1.dar\"",
            dir.display()
        )));
    }

    for (i, (n, _)) in numbered.iter().enumerate() {
        let expected = (i + 1) as u32;
        if *n != expected {
            let found: Vec<u32> = numbered.iter().map(|(n, _)| *n).collect();
            return Err(TapectlError::Dar(format!(
                "dar slice numbering for archive \"{stem}\" is not a clean \
                 1..={total} run: expected slice {expected} at position \
                 {position} (1-based), found slice {n} instead — all parsed \
                 slice numbers: {found:?}. Refusing to guess an order: a \
                 gap, duplicate, or out-of-range slice number means dar did \
                 not produce what tapectl expected.",
                total = numbered.len(),
                position = i + 1,
            )));
        }
    }

    Ok(numbered.into_iter().map(|(_, p)| p).collect())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    /// Touches `dir/name` as an empty file. `list_slices` only inspects
    /// filenames, so fixture content is irrelevant — this lets these tests
    /// pin down slice ordering/matching at 11+ slices without invoking dar.
    fn touch(dir: &Path, name: &str) {
        File::create(dir.join(name)).unwrap();
    }

    fn names_of(slices: &[PathBuf]) -> Vec<String> {
        slices
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn parse_slice_number_matches_the_exact_pattern_only() {
        assert_eq!(parse_slice_number("base.1.dar", "base"), Some(1));
        assert_eq!(parse_slice_number("base.42.dar", "base"), Some(42));
        assert_eq!(parse_slice_number("base.007.dar", "base"), Some(7));
        assert_eq!(parse_slice_number("base_old.dar", "base"), None);
        assert_eq!(parse_slice_number("base.backup.dar", "base"), None);
        assert_eq!(parse_slice_number("base2.4.dar", "base"), None);
        assert_eq!(parse_slice_number("base.notanumber.dar", "base"), None);
        assert_eq!(parse_slice_number("base.dar", "base"), None);
        assert_eq!(parse_slice_number("base..dar", "base"), None);
    }

    // --- issue #34/H8: list_slices used to sort lexicographically and
    // match by prefix. Lexicographic sort desyncs from dar's real slice
    // index once an archive reaches 10 slices ("base.10.dar" < "base.2.dar"
    // as strings), and prefix matching lets foreign leftover files ride
    // along as if they were slices of this archive. Both defects are
    // checksum-invisible: every individual slice still hashes correctly,
    // so the corruption only surfaces when dar tries to reassemble slices
    // in an order it never produced.
    //
    // The tests below call only `list_slices` (not any private helper) so
    // they compile and run unmodified against the pre-fix body too.

    #[test]
    fn slice_number_matches_dars_numeric_index_at_eleven_slices() {
        // The regression test issue #34 names by name: staging::mod's
        // `stage_create` derives `stage_slices.slice_number` as
        // `vec_position + 1` (see src/staging/mod.rs), so position i in
        // this Vec MUST be dar's own slice (i+1) or the recorded
        // slice_number silently stops matching the real file.
        let tmp = tempfile::tempdir().unwrap();
        let stem = "base";
        for n in 1..=11 {
            touch(tmp.path(), &format!("{stem}.{n}.dar"));
        }

        let slices = list_slices(&tmp.path().join(stem)).unwrap();
        assert_eq!(slices.len(), 11);
        for (i, path) in slices.iter().enumerate() {
            let expected_name = format!("{stem}.{}.dar", i + 1);
            let actual_name = path.file_name().unwrap().to_str().unwrap();
            assert_eq!(
                actual_name,
                expected_name,
                "position {i} (would be recorded as slice_number {}) must be \
                 dar's slice {expected_name}, found {actual_name} instead",
                i + 1
            );
        }
    }

    #[test]
    fn ordering_is_numeric_not_lexicographic() {
        let tmp = tempfile::tempdir().unwrap();
        let stem = "base";
        for n in 1..=10 {
            touch(tmp.path(), &format!("{stem}.{n}.dar"));
        }

        let names = names_of(&list_slices(&tmp.path().join(stem)).unwrap());
        let pos_2 = names.iter().position(|n| n == "base.2.dar").unwrap();
        let pos_10 = names.iter().position(|n| n == "base.10.dar").unwrap();
        assert!(
            pos_2 < pos_10,
            "base.2.dar must precede base.10.dar numerically, got order {names:?}"
        );
    }

    #[test]
    fn foreign_lookalike_files_are_excluded() {
        let tmp = tempfile::tempdir().unwrap();
        let stem = "base";
        touch(tmp.path(), &format!("{stem}.1.dar"));
        touch(tmp.path(), &format!("{stem}.2.dar"));
        touch(tmp.path(), &format!("{stem}_old.dar")); // no separating dot
        touch(tmp.path(), &format!("{stem}.backup.dar")); // non-numeric middle
        touch(tmp.path(), &format!("{stem}2.4.dar")); // different stem entirely
        touch(tmp.path(), &format!("{stem}.notanumber.dar")); // non-numeric middle

        let names = names_of(&list_slices(&tmp.path().join(stem)).unwrap());
        assert_eq!(
            names,
            vec!["base.1.dar".to_string(), "base.2.dar".to_string()],
            "foreign leftovers must not be treated as slices of this archive"
        );
    }

    #[test]
    fn a_gap_in_slice_numbers_errors_loudly_naming_stem_and_found_numbers() {
        let tmp = tempfile::tempdir().unwrap();
        let stem = "base";
        touch(tmp.path(), &format!("{stem}.1.dar"));
        touch(tmp.path(), &format!("{stem}.2.dar"));
        // .3.dar deliberately missing
        touch(tmp.path(), &format!("{stem}.4.dar"));

        let err = list_slices(&tmp.path().join(stem)).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains(stem), "error must name the stem: {msg}");
        assert!(
            msg.contains("expected slice 3"),
            "error must name the missing/expected index: {msg}"
        );
        assert!(
            msg.contains('4'),
            "error must surface what was actually found: {msg}"
        );
    }

    #[test]
    fn a_duplicate_slice_number_errors_loudly() {
        let tmp = tempfile::tempdir().unwrap();
        let stem = "base";
        touch(tmp.path(), &format!("{stem}.1.dar"));
        touch(tmp.path(), &format!("{stem}.2.dar"));
        touch(tmp.path(), &format!("{stem}.02.dar")); // leading zero: also parses to 2
        touch(tmp.path(), &format!("{stem}.3.dar"));

        let err = list_slices(&tmp.path().join(stem)).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains(stem), "error must name the stem: {msg}");
    }

    #[test]
    fn a_zero_slice_number_errors_loudly() {
        // dar numbers slices from 1 (confirmed against the dar man page:
        // docs/research/2026-07-21-ontape-format-and-write-design.md §F1),
        // so a `.0.` slice is out of dar's range and must not be silently
        // accepted as if it were slice 1.
        let tmp = tempfile::tempdir().unwrap();
        let stem = "base";
        touch(tmp.path(), &format!("{stem}.0.dar"));
        touch(tmp.path(), &format!("{stem}.1.dar"));
        touch(tmp.path(), &format!("{stem}.2.dar"));

        let err = list_slices(&tmp.path().join(stem)).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains(stem), "error must name the stem: {msg}");
    }

    #[test]
    fn zero_matching_slices_errors_loudly_instead_of_reporting_an_empty_archive() {
        // list_slices is only ever called right after a successful `dar -c`
        // (see create_archive above), which always emits >=1 slice (dar
        // numbers from 1). Finding nothing that matches after a successful
        // dar run means something is fundamentally wrong (wrong directory,
        // stem mismatch) -- silently returning Ok(vec![]) would let
        // create_archive report num_slices=0 with no error at all, which is
        // exactly the silent-proceed failure class #34 exists to close.
        let tmp = tempfile::tempdir().unwrap();
        touch(tmp.path(), "unrelated.txt");

        let err = list_slices(&tmp.path().join("base")).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("base"), "error must name the stem: {msg}");
    }
}
