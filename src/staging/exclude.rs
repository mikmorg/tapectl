//! Shared exclude-pattern matcher (issue #49): the single place that
//! decides whether a path is "excluded" from archival content. Used
//! identically by `staging::walk_directory` (feeds the snapshot manifest
//! and `files` table) and `collection::fingerprint::walk_fingerprint` (the
//! dirty/pending scan), so the two walks can never independently disagree
//! about the same fact — exactly the failure shape issues #33 (walk vs
//! validator on symlinks), #36 (a near-duplicate dirty scanner), and #48
//! (two registration paths reading one dotfile field differently) each
//! hit once already. `staging::stage_create`'s dar `-X` arguments are
//! built from the same effective pattern list (globals + dotfile), so all
//! three consumers agree.
//!
//! ## Matching semantics
//!
//! Deliberately mirrors dar's own real, **empirically verified** `-X`/
//! `-an` behavior (see the commit message for the actual `dar -c`/`dar -l`
//! run this is based on — not recollection, which turned out to be wrong
//! on the directory point below):
//!
//!  - Patterns match the entry's **basename** (leaf component) only, never
//!    the full relative path. This is dar's own documented `-X` behavior:
//!    "the mask ... is applied to filenames which are not directories"
//!    (`man dar`, `-X`/`--exclude`).
//!  - Patterns are matched **case-insensitively**. `dar::create::create_archive`
//!    unconditionally passes `-an` (`--alter=no-case`) before its `-X`
//!    loop, so every mask dar receives is already case-insensitive — a
//!    case-sensitive matcher here would silently disagree with dar for any
//!    pattern/filename pair differing only by case.
//!  - Patterns **never match directories** — confirmed empirically (`dar
//!    -X 'a_dir_name'` did NOT exclude a same-named directory or its
//!    contents) and confirmed in `man dar`: "-I and -X only use the name
//!    of files and do not apply to directories". Directory-subtree pruning
//!    is a *different* dar mechanism (`-P`/`--prune`, matched against
//!    path+filename, which this fix does not wire up — `stage_create`
//!    still always passes `exclude_paths: &[]`). So there is no "prune the
//!    whole subtree vs exclude just the directory entry" ambiguity to
//!    resolve here: a directory is never tested against these patterns at
//!    all, by either walk, and dar will archive its full contents
//!    regardless of what `exclude_patterns`/`global_excludes` contain. A
//!    file *nested inside* a directory is still excluded on its own merits
//!    if its own basename matches — parent directory names are never
//!    consulted.

use std::path::Path;

use glob::{MatchOptions, Pattern};

/// The one `MatchOptions` value every match in this module uses, so a
/// future tweak can't accidentally diverge between call sites. See the
/// module doc comment for why `case_sensitive: false`.
fn match_options() -> MatchOptions {
    MatchOptions {
        case_sensitive: false,
        require_literal_separator: false,
        require_literal_leading_dot: false,
    }
}

/// Compile `patterns` into `glob::Pattern`s, silently dropping any that
/// fail to parse — mirrors `collection::sync::candidate_unit_dirs`'s
/// existing precedent: a malformed pattern in config or a dotfile must
/// never crash a walk, it just never matches anything.
pub fn compile(patterns: &[String]) -> Vec<Pattern> {
    patterns
        .iter()
        .filter_map(|p| Pattern::new(p).ok())
        .collect()
}

/// True if `path`'s basename (see the module doc comment — leaf component
/// only, case-insensitive) matches at least one of `compiled`. A path with
/// no valid UTF-8 basename is never excluded — fails safe toward archiving
/// unrecognized content rather than silently dropping it.
///
/// Callers must never call this for a directory entry (both `walk_directory`
/// and `walk_fingerprint` only reach this for non-directory entries) — see
/// the module doc comment for why dar's own `-X` can't exclude directories
/// either, so there is nothing for this function to decide there.
pub fn is_excluded(path: &Path, compiled: &[Pattern]) -> bool {
    if compiled.is_empty() {
        return false;
    }
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let opts = match_options();
    compiled.iter().any(|p| p.matches_with(name, opts))
}

/// The unit dotfile's own `[excludes] patterns` for the unit rooted at
/// `dir_path` — issue #49 item 2's per-unit half of "effective excludes =
/// globals + dotfile". Reads `dir_path/.tapectl-unit.toml` directly (the
/// same file `staging::stage_create`'s `resolve_slice_size_string` reads
/// ad hoc, and `dotfile::read_dotfile` reads structurally elsewhere), so
/// both `staging::walk_directory` and
/// `collection::fingerprint::walk_fingerprint` can call this with just the
/// directory they are already walking — neither needs a new parameter.
///
/// Returns an empty Vec — never an error — if the dotfile is absent,
/// unreadable, or fails to parse: a directory with no unit dotfile yet (or
/// one that predates `unit init`) must behave exactly as before this fix,
/// and one malformed dotfile must never abort a walk.
pub fn dotfile_patterns(dir_path: &Path) -> Vec<String> {
    let dotfile_path = dir_path.join(".tapectl-unit.toml");
    crate::unit::dotfile::read_dotfile(&dotfile_path)
        .map(|d| d.exclude_patterns)
        .unwrap_or_default()
}

/// The full, compiled "effective excludes" set for `dir_path`: the caller's
/// `global_excludes` (`config.defaults.global_excludes` — issue #49 item
/// 5's other half, previously reaching dar only, never either walk) plus
/// this directory's own dotfile `[excludes] patterns` (`dotfile_patterns`,
/// item 2, already wired).
///
/// This is the ONE place the two layers are combined. Both
/// `staging::walk_directory` and `collection::fingerprint::walk_fingerprint`
/// call this instead of each independently concatenating the two pattern
/// lists — so the combination step itself cannot drift between the two
/// walks, on top of `dotfile_patterns` already guaranteeing that for the
/// per-unit half alone. That's the exact failure shape issues #33/#36/#48
/// each hit once: two independent scanners quietly disagreeing about one
/// fact. `global_excludes` is passed in (never read from a config file
/// here) for the same reason `dotfile_patterns` reads its dotfile directly
/// rather than requiring one: hidden I/O inside a walk is untestable and
/// lets production and test paths diverge — the caller already has
/// `Config` (or the test already has whatever list it wants to assert on)
/// and passes the slice in explicitly.
pub fn effective_compiled(dir_path: &Path, global_excludes: &[String]) -> Vec<Pattern> {
    let mut patterns = global_excludes.to_vec();
    patterns.extend(dotfile_patterns(dir_path));
    compile(&patterns)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn empty_patterns_never_exclude_anything() {
        let compiled = compile(&[]);
        assert!(!is_excluded(Path::new("Thumbs.db"), &compiled));
    }

    #[test]
    fn basename_glob_matches_regardless_of_parent_directories() {
        // Confirms basename-only matching (module doc comment): a pattern
        // with no slash matches purely on the leaf component, ignoring
        // whatever directories precede it.
        let compiled = compile(&["*.tmp".to_string()]);
        assert!(is_excluded(Path::new("a/b/c/junk.tmp"), &compiled));
        assert!(!is_excluded(Path::new("a/b/c/keep.txt"), &compiled));
    }

    #[test]
    fn matching_is_case_insensitive_like_dars_an_flag() {
        // dar's create_archive unconditionally passes -an before its -X
        // loop (src/dar/create.rs) — a case-sensitive matcher here would
        // silently disagree with what dar actually excludes.
        let compiled = compile(&["thumbs.db".to_string()]);
        assert!(is_excluded(Path::new("Thumbs.db"), &compiled));
        assert!(is_excluded(Path::new("THUMBS.DB"), &compiled));
    }

    #[test]
    fn an_invalid_pattern_is_silently_dropped_not_an_error() {
        // Mirrors collection::sync::candidate_unit_dirs's existing
        // precedent for a malformed glob pattern.
        let compiled = compile(&["[".to_string(), "*.tmp".to_string()]);
        assert_eq!(
            compiled.len(),
            1,
            "the malformed pattern must be dropped, not error"
        );
        assert!(is_excluded(Path::new("j.tmp"), &compiled));
    }

    #[test]
    fn dotfile_patterns_is_empty_when_no_dotfile_exists() {
        let tmp = TempDir::new().unwrap();
        assert!(dotfile_patterns(tmp.path()).is_empty());
    }

    #[test]
    fn dotfile_patterns_reads_the_units_own_exclude_patterns() {
        let tmp = TempDir::new().unwrap();
        crate::unit::dotfile::write_dotfile(
            &tmp.path().join(".tapectl-unit.toml"),
            &crate::unit::dotfile::UnitDotfile {
                uuid: "u".into(),
                name: "n".into(),
                created: "2026-01-01T00:00:00Z".into(),
                tags: vec![],
                tenant: "t".into(),
                archive_set: None,
                checksum_mode: "mtime_size".into(),
                compression: "none".into(),
                exclude_patterns: vec!["*.secret".into()],
            },
        )
        .unwrap();
        assert_eq!(dotfile_patterns(tmp.path()), vec!["*.secret".to_string()]);
    }

    #[test]
    fn a_malformed_dotfile_yields_no_patterns_rather_than_erroring() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".tapectl-unit.toml"), b"not valid toml [[[").unwrap();
        assert!(dotfile_patterns(tmp.path()).is_empty());
    }

    #[test]
    fn effective_compiled_is_empty_with_no_globals_and_no_dotfile() {
        // Issue #49 trap: the no-excludes case must behave exactly as
        // today — empty globals, no dotfile, nothing excluded.
        let tmp = TempDir::new().unwrap();
        let compiled = effective_compiled(tmp.path(), &[]);
        assert!(!is_excluded(Path::new("Thumbs.db"), &compiled));
        assert!(!is_excluded(Path::new("anything.tmp"), &compiled));
    }

    #[test]
    fn effective_compiled_merges_globals_and_dotfile_patterns() {
        let tmp = TempDir::new().unwrap();
        crate::unit::dotfile::write_dotfile(
            &tmp.path().join(".tapectl-unit.toml"),
            &crate::unit::dotfile::UnitDotfile {
                uuid: "u".into(),
                name: "n".into(),
                created: "2026-01-01T00:00:00Z".into(),
                tags: vec![],
                tenant: "t".into(),
                archive_set: None,
                checksum_mode: "mtime_size".into(),
                compression: "none".into(),
                exclude_patterns: vec!["*.secret".into()],
            },
        )
        .unwrap();

        let global_excludes = vec!["Thumbs.db".to_string()];
        let compiled = effective_compiled(tmp.path(), &global_excludes);

        assert!(
            is_excluded(Path::new("Thumbs.db"), &compiled),
            "the global pattern must be included"
        );
        assert!(
            is_excluded(Path::new("x.secret"), &compiled),
            "the dotfile pattern must also be included"
        );
        assert!(!is_excluded(Path::new("keep.txt"), &compiled));
    }

    #[test]
    fn effective_compiled_applies_globals_even_with_no_dotfile_at_all() {
        // The ticket's own headline gap: a unit with NO dotfile override
        // must still have config.defaults.global_excludes applied.
        let tmp = TempDir::new().unwrap();
        let global_excludes = vec!["Thumbs.db".to_string(), "*.tmp".to_string()];
        let compiled = effective_compiled(tmp.path(), &global_excludes);
        assert!(is_excluded(Path::new("Thumbs.db"), &compiled));
        assert!(is_excluded(Path::new("junk.tmp"), &compiled));
        assert!(!is_excluded(Path::new("keep.txt"), &compiled));
    }
}
