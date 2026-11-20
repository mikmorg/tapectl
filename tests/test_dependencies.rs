//! External binaries the ungated suite genuinely requires (issue #43).
//!
//! The ungated suite is not hermetic: staging runs the real GNU tar, and the
//! tests that read a slice back run the real decompressor of each algorithm
//! tapectl carries. For months this workspace shipped a CI job whose comment
//! said otherwise (issue #43; the archiver then was a different tool),
//! and CI was red on every commit as a result, long enough that the signal
//! stopped being read at all.
//!
//! This file exists so that failure mode cannot recur quietly. It asserts
//! each dependency **once, loudly, by name**, instead of letting it surface
//! as a dozen cryptic panics scattered across two suites.
//!
//! ## Why this fails rather than skips
//!
//! Skipping would be the obvious "honest skips" reading of #43, and it is
//! the wrong one here. The tests that need tar are the staging-pipeline
//! regression guards — `stage_create_uses_archive_set_resolved_slice_size_
//! not_global_default` (#48), `two_stage_sets_of_the_same_snapshot_do_not_
//! collide_on_disk` (#53), `dotfile_and_global_exclude_patterns_keep_files_
//! out_of_the_stream` (#49), the bitrot false-positive guards (#36).
//! Silently skipping them hands a contributor a green suite that verified
//! none of the staging pipeline: precisely the silent-pass class this
//! project keeps having to dig out. #43's title says *honest* skips,
//! meaning visible — and the most visible thing is a named failure.
//!
//! The tools are already hard runtime dependencies of the tool (`CLAUDE.md`,
//! README), so requiring them to test it costs a contributor nothing they
//! did not already need.

use std::process::Command;

/// Tests known to require GNU tar at the writer's floor (`stage create`
/// stages on tar since 2.0.0's T4.5; ADR-0012 items 253, 274).
const TAR_DEPENDENT_TESTS: &[&str] = &[
    "staging::tests::a_unit_with_no_excludes_configured_behaves_exactly_as_before",
    "staging::tests::dotfile_and_global_exclude_patterns_keep_files_out_of_the_stream",
    "staging::tests::excluded_junk_file_content_drift_at_stable_size_does_not_false_positive_bitrot",
    "staging::tests::global_default_excluded_file_content_drift_at_stable_size_does_not_false_positive_bitrot",
    "staging::tests::stage_create_uses_archive_set_resolved_slice_size_not_global_default",
    "staging::tests::two_stage_sets_of_the_same_snapshot_do_not_collide_on_disk",
    "staging::tar_stage_tests::tests::the_cut_through_stage_create_holds_whole_members_and_a_run",
    "staging::tar_stage_tests::tests::a_unit_of_every_kind_stages_and_its_table_restores_each_file_by_range",
    "cli::stage::tests::create_without_version_behaves_exactly_as_before",
    "cli::stage::tests::create_with_version_refuses_when_a_stage_set_is_staged",
    "cli::stage::tests::create_with_version_succeeds_once_the_only_stage_set_is_cleaned",
    "cli::stage::tests::info_query_counts_sibling_stage_sets_after_a_restage",
    "cli_smoke::crashed_stage_is_detected_and_then_cleanable",
    "cli_smoke::live_stage_is_not_disturbed_by_a_concurrent_read_only_command",
];

/// GNU tar on `PATH` meets the writer's floor, GNU tar 1.35 built with ACL
/// support (ADR-0012 item 274): `stage create` refuses any other, so every
/// staging test needs it. Probed as `stage create` probes it.
#[test]
fn gnu_tar_at_the_writers_floor_is_on_path_because_staging_runs_it() {
    let scratch = tempfile::tempdir().unwrap();
    let refusal = match tapectl::tar::probe("tar", scratch.path()) {
        Ok(check) => check.refusal(),
        Err(e) => Some(e.to_string()),
    };
    assert!(
        refusal.is_none(),
        "`tar` on PATH is not the writer's tar: {}\n\n\
         {} tests stage a unit with the real tar and will fail with a confusing \
         error instead of this message:\n  {}\n\n\
         Install GNU tar 1.35 or later built with ACL support (Ubuntu 24.04's `tar` \
         is; on Debian 10 build it from the upstream release into /usr/local, \
         docs/research/2026-11-17-t02-gnu-tar-built-on-debian-10.md).",
        refusal.unwrap_or_default(),
        TAR_DEPENDENT_TESTS.len(),
        TAR_DEPENDENT_TESTS.join("\n  "),
    );
}

/// Layout 3's tools (2.0.0, the plan's T4.0): `setfacl` and
/// `getfacl` from `acl`, for the stream's ACL checks (ADR-0012 items 270, 274),
/// and the decompressor of each algorithm Layout 3 carries (item 285), which
/// tests use to read library-made slices back with the real tool.
const LAYOUT3_TOOLS: &[(&str, &str, &str)] = &[
    ("setfacl", "--version", "acl"),
    ("getfacl", "--version", "acl"),
    ("gzip", "--version", "gzip"),
    ("xz", "--version", "xz-utils"),
    ("zstd", "--version", "zstd"),
];

fn tool_runs(name: &str, arg: &str) -> bool {
    Command::new(name)
        .arg(arg)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[test]
fn layout_3s_tools_are_on_path() {
    let missing: Vec<String> = LAYOUT3_TOOLS
        .iter()
        .filter(|(name, arg, _)| !tool_runs(name, arg))
        .map(|(name, _, package)| format!("{name} (Debian/Ubuntu package `{package}`)"))
        .collect();
    assert!(
        missing.is_empty(),
        "these tools are not on PATH, and Layout 3's tests run them for real:\n  {}\n\n\
         Install them (Debian/Ubuntu: `sudo apt install acl gzip xz-utils zstd`).",
        missing.join("\n  "),
    );
}
