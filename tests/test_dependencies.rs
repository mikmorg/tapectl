//! External binaries the ungated suite genuinely requires (issue #43).
//!
//! For months this workspace shipped a CI job whose own comment read *"the
//! ungated suite is hermetic: no external binaries"* while ten library
//! tests plus two `cli_smoke` tests shelled out to a real `dar`. CI was red
//! on every commit as a result, and stayed red long enough that the signal
//! stopped being read at all (fixed 2026-07-31 in `8485f73`, which installs
//! dar in CI).
//!
//! This file exists so that failure mode cannot recur quietly. It asserts
//! the dependency **once, loudly, by name**, instead of letting it surface
//! as a dozen cryptic `dar -c failed` panics scattered across two suites.
//!
//! ## Why this fails rather than skips
//!
//! Skipping would be the obvious "honest skips" reading of #43, and it is
//! the wrong one here. The tests that need dar are the staging-pipeline
//! regression guards — `stage_create_uses_archive_set_resolved_slice_size_
//! not_global_default` (#48), `two_stage_sets_of_the_same_snapshot_do_not_
//! collide_on_disk` (#53), `dotfile_exclude_patterns_reach_dars_
//! constructed_arguments` (#49), the bitrot false-positive guards (#36).
//! Silently skipping them hands a contributor a green suite that verified
//! none of the staging pipeline: precisely the silent-pass class this
//! project keeps having to dig out. #43's title says *honest* skips,
//! meaning visible — and the most visible thing is a named failure.
//!
//! dar is already a hard runtime dependency of the tool (`CLAUDE.md`,
//! README), so requiring it to test the tool costs a contributor nothing
//! they did not already need.

use std::process::Command;

/// Every test that shells out to `dar` resolves the binary through `PATH`
/// as plain `"dar"`. Fixtures must NOT hardcode an absolute path —
/// `/usr/bin/dar` is correct on Debian/Ubuntu and wrong on a distro that
/// installs to `/usr/local/bin`, which turns a portability problem into a
/// mystery test failure.
const DAR: &str = "dar";

/// Tests known to require a working `dar`. Kept as a list rather than a
/// bare count so the failure message tells a contributor exactly what they
/// lose, and so adding a dar-dependent test without noticing is harder.
const DAR_DEPENDENT_TESTS: &[&str] = &[
    "staging::tests::a_unit_with_no_excludes_configured_behaves_exactly_as_before",
    "staging::tests::dotfile_exclude_patterns_reach_dars_constructed_arguments",
    "staging::tests::excluded_junk_file_content_drift_at_stable_size_does_not_false_positive_bitrot",
    "staging::tests::global_default_excluded_file_content_drift_at_stable_size_does_not_false_positive_bitrot",
    "staging::tests::stage_create_uses_archive_set_resolved_slice_size_not_global_default",
    "staging::tests::two_stage_sets_of_the_same_snapshot_do_not_collide_on_disk",
    "cli::stage::tests::create_without_version_behaves_exactly_as_before",
    "cli::stage::tests::create_with_version_refuses_when_a_stage_set_is_staged",
    "cli::stage::tests::create_with_version_succeeds_once_the_only_stage_set_is_cleaned",
    "cli::stage::tests::info_query_counts_sibling_stage_sets_after_a_restage",
    "dar::restore::tests::real_dar_collision_is_reported_as_a_failed_restore",
    "cli_smoke::crashed_stage_is_detected_and_then_cleanable",
    "cli_smoke::live_stage_is_not_disturbed_by_a_concurrent_read_only_command",
];

/// True when a usable `dar` is on `PATH`. Shared with `cli_smoke.rs`'s
/// fail-fast check via duplication rather than a helper crate — integration
/// test binaries cannot share `#[cfg(test)]` items with the library, and a
/// three-line `Command::status()` is not worth a dev-dependency.
fn dar_on_path() -> bool {
    Command::new(DAR)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[test]
fn dar_is_available_because_the_ungated_suite_is_not_hermetic() {
    assert!(
        dar_on_path(),
        "`dar` was not found on PATH.\n\n\
         The ungated test suite is NOT hermetic: {} tests shell out to a real dar \
         to build and extract archives, and every one of them will fail with a \
         confusing error instead of this message if you ignore this.\n\n\
         They are the staging-pipeline regression guards — losing them silently \
         means a green suite that verified none of the staging pipeline:\n  {}\n\n\
         Install dar >= 2.6 (Debian/Ubuntu: `sudo apt install dar`). dar is \
         already a hard runtime dependency of tapectl itself — see CLAUDE.md's \
         Testing section and the README.",
        DAR_DEPENDENT_TESTS.len(),
        DAR_DEPENDENT_TESTS.join("\n  "),
    );
}

/// dar's own minimum is enforced at runtime by `src/dar/version.rs`; this
/// pins that the *installed* dar clears it, so a too-old dar fails here by
/// name rather than midway through an archive test.
#[test]
fn installed_dar_meets_the_documented_minimum() {
    if !dar_on_path() {
        // The test above already reports this, loudly and with instructions.
        // Repeating the whole message here would just double the noise.
        return;
    }
    let out = Command::new(DAR)
        .arg("--version")
        .output()
        .expect("dar --version");
    let text = String::from_utf8_lossy(&out.stdout);
    let version_line = text
        .lines()
        .find(|l| l.contains("dar version"))
        .unwrap_or_default();
    assert!(
        !version_line.is_empty(),
        "could not parse a version out of `dar --version`:\n{text}"
    );

    // Parse "dar version 2.7.13, Copyright ..." -> (2, 7)
    let nums: Vec<u32> = version_line
        .split_whitespace()
        .find(|t| t.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .map(|v| {
            v.trim_end_matches(',')
                .split('.')
                .filter_map(|p| p.parse().ok())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        nums.len() >= 2,
        "unexpected dar version format: {version_line:?}"
    );
    let (major, minor) = (nums[0], nums[1]);
    assert!(
        major > 2 || (major == 2 && minor >= 6),
        "dar {major}.{minor} is below the documented minimum 2.6 \
         (enforced at runtime by src/dar/version.rs): {version_line:?}"
    );
}
