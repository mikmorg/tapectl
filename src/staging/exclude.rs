//! Shared exclude-pattern matcher (issue #49): the single place that
//! decides whether a path is "excluded" from archival content. Used
//! identically by `staging::walk_directory` (feeds the snapshot manifest
//! and `files` table) and `collection::fingerprint::walk_fingerprint` (the
//! dirty/pending scan), so the two walks can never independently disagree
//! about the same fact — exactly the failure shape issues #33 (walk vs
//! validator on symlinks), #36 (a near-duplicate dirty scanner), and #48
//! (two registration paths reading one dotfile field differently) each
//! hit once already. `collection::sync` matches a collection's own
//! `exclude` here too.
//!
//! It is the only matcher (D14; ADR-0012 item 252): tar is given the
//! snapshot's exact list (`volume-format-v3.md` §3.1), so what the walk
//! records is what is archived, and the two matchers that once disagreed
//! (#542) are one.
//!
//! ## The one exclude rule (issue #359)
//!
//! Every exclude pattern — `[defaults] global_excludes`, a unit dotfile's
//! `[excludes] patterns`, and a collection's `exclude` — follows the same
//! rule:
//!
//!  - **Case never matters.** A collection's `exclude` was the one
//!    case-sensitive matcher until #359; making it case-insensitive only
//!    ever excludes MORE, so no directory an existing config excluded is
//!    newly registered.
//!  - **A plain pattern** (`*.tmp`, `Thumbs.db`) matches an entry's
//!    **basename** only, never its path, and never a directory. A file
//!    nested inside a directory is excluded on its own basename's merits.
//!    (In a collection's `exclude`, where every candidate IS a directory, a
//!    plain pattern matches the candidate directory's own name, as it always
//!    has.)
//!  - **A directory pattern** (`name/`, e.g. `.cache/`, `node_modules/`)
//!    excludes that directory's **whole subtree**, at any depth below the
//!    walk root: an entry is excluded when any component of its path below
//!    the root matches `name`. The pruned directory itself is kept, as an
//!    empty directory, so `excludes_dir_entry` keeps that directory's own
//!    entry and drops only what is inside it. The rule does not distinguish
//!    a file from a directory: a FILE named `.cache` is excluded by
//!    `.cache/` too. Components ABOVE the walk root are never consulted (a
//!    unit living under `~/.cache/` is not excluded wholesale). Before #359 a
//!    `name/` pattern matched nothing at all (a basename never contains
//!    `/`), so honouring it only ever excludes more.
//!  - **Matches nothing:** a pattern that is not a valid glob (refused by
//!    name where a list is read, #542; a walk never crashes on one), a plain
//!    pattern containing `/`, and a directory pattern whose name contains
//!    `/` or is empty (`a/b/`, `/`).

use std::borrow::Cow;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

use glob::{MatchOptions, Pattern};

use crate::error::Result;

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

/// A name as a pattern reads it: itself when it is UTF-8, else its escaped
/// form (item 297).
fn match_text(name: &OsStr) -> Cow<'_, str> {
    match name.to_str() {
        Some(text) => Cow::Borrowed(text),
        None => crate::volume::index::escape_os(name),
    }
}

/// One pattern, classified. `None` = matches nothing (see the module doc).
enum Classified<'a> {
    /// No trailing `/`: a basename glob.
    Plain(&'a str),
    /// Trailing `/` (any number of them): the directory's name.
    Directory(&'a str),
}

fn classify(pattern: &str) -> Option<Classified<'_>> {
    match pattern.strip_suffix('/') {
        None => Some(Classified::Plain(pattern)),
        Some(_) => {
            let name = pattern.trim_end_matches('/');
            if name.is_empty() || name.contains('/') {
                None
            } else {
                Some(Classified::Directory(name))
            }
        }
    }
}

/// What is wrong with `pattern`, if the matcher rejects it (#542). Such a
/// pattern would be dropped by [`Excludes::new`] and leave out nothing, so
/// every place a list is read refuses it by name instead, with `origin`
/// saying where it was set. A pattern that is valid but matches nothing
/// (module doc comment) is not a rejection.
pub fn pattern_problem(pattern: &str, origin: &str) -> Option<String> {
    let glob = match classify(pattern)? {
        Classified::Plain(g) | Classified::Directory(g) => g,
    };
    Pattern::new(glob)
        .err()
        .map(|e| format!("{origin}: the exclusion pattern \"{pattern}\" is not a valid glob ({e})"))
}

/// The first rejected pattern of `patterns`, as a refusal naming it and
/// `origin`.
pub fn check_patterns(patterns: &[String], origin: &str) -> Result<()> {
    match patterns.iter().find_map(|p| pattern_problem(p, origin)) {
        Some(msg) => Err(crate::error::TapectlError::Other(msg)),
        None => Ok(()),
    }
}

/// A compiled exclude set for one walk root (a unit's directory, or a
/// collection's root). Build it with `effective_compiled` (a unit: globals +
/// dotfile) or `Excludes::new` (any explicit pattern list).
#[derive(Debug, Clone)]
pub struct Excludes {
    root: PathBuf,
    /// Plain patterns: matched against a basename. Each is kept with the
    /// text it was written as, which a snapshot reports (#542).
    names: Vec<(String, Pattern)>,
    /// Directory patterns, without their `/`: matched against every path
    /// component below `root`. Each is kept with the text it was written as
    /// (`.cache/`).
    dirs: Vec<(String, Pattern)>,
}

impl Excludes {
    /// Compile `patterns` for a walk rooted at `root`, silently dropping any
    /// that match nothing (module doc comment).
    pub fn new(root: &Path, patterns: &[String]) -> Self {
        let mut names = Vec::new();
        let mut dirs = Vec::new();
        for p in patterns {
            match classify(p) {
                Some(Classified::Plain(g)) => {
                    names.extend(Pattern::new(g).ok().map(|c| (p.clone(), c)))
                }
                Some(Classified::Directory(g)) => {
                    dirs.extend(Pattern::new(g).ok().map(|c| (p.clone(), c)))
                }
                None => {}
            }
        }
        Excludes {
            root: root.to_path_buf(),
            names,
            dirs,
        }
    }

    fn is_empty(&self) -> bool {
        self.names.is_empty() && self.dirs.is_empty()
    }

    /// The components of `path` below the root, as text a pattern is matched
    /// against: a UTF-8 name as it is, a name that is not UTF-8 in its
    /// escaped form (ADR-0012 item 297; `volume::index::escape_os`), so no
    /// name is lost to a lossy conversion and a pattern such as `*.tmp` or
    /// `caf*` still reads it. A path outside the root is reduced to its
    /// leaf alone, so nothing above the root is ever consulted.
    fn components_below_root<'p>(&self, path: &'p Path) -> Vec<Cow<'p, str>> {
        let rel = match path.strip_prefix(&self.root) {
            Ok(rel) => rel,
            Err(_) => return path.file_name().map(match_text).into_iter().collect(),
        };
        rel.components()
            .filter_map(|c| match c {
                Component::Normal(s) => Some(match_text(s)),
                _ => None,
            })
            .collect()
    }

    /// The directory pattern, as written, that `component` matches.
    fn dir_pattern_of(&self, component: &str) -> Option<&str> {
        let opts = match_options();
        self.dirs
            .iter()
            .find(|(_, p)| p.matches_with(component, opts))
            .map(|(text, _)| text.as_str())
    }

    /// The plain pattern, as written, that `name` matches.
    fn name_pattern_of(&self, name: &str) -> Option<&str> {
        let opts = match_options();
        self.names
            .iter()
            .find(|(_, p)| p.matches_with(name, opts))
            .map(|(text, _)| text.as_str())
    }

    fn matches_dir_pattern(&self, component: &str) -> bool {
        self.dir_pattern_of(component).is_some()
    }

    fn matches_name_pattern(&self, name: &str) -> bool {
        self.name_pattern_of(name).is_some()
    }

    /// A NON-directory entry of a walk: excluded when its basename matches a
    /// plain pattern, or any component of its path below the root (its own
    /// name included) matches a directory pattern.
    pub fn excludes_file(&self, path: &Path) -> bool {
        self.file_pattern(path).is_some()
    }

    /// The pattern, as written, that leaves the NON-directory entry `path`
    /// out: the one [`Excludes::excludes_file`] answers by, so a snapshot can
    /// name it (#542). A directory pattern is looked for first.
    pub fn file_pattern(&self, path: &Path) -> Option<&str> {
        if self.is_empty() {
            return None;
        }
        let parts = self.components_below_root(path);
        parts
            .iter()
            .find_map(|c| self.dir_pattern_of(c))
            .or_else(|| parts.last().and_then(|leaf| self.name_pattern_of(leaf)))
    }

    /// The directory pattern, as written, that matches the DIRECTORY entry
    /// `path`'s own name: the folder a snapshot keeps as an empty entry and
    /// reports as left out with what was under it (#542). A plain pattern
    /// never matches a directory. `None` for an entry that merely lies
    /// inside a pruned folder: the walk never reaches it.
    pub fn dir_self_pattern(&self, path: &Path) -> Option<&str> {
        let parts = self.components_below_root(path);
        parts.last().and_then(|leaf| self.dir_pattern_of(leaf))
    }

    /// A DIRECTORY entry of a walk: excluded when it lies INSIDE a pruned
    /// subtree (a component strictly above it matches a directory pattern).
    /// The pruned directory itself is kept, as an empty directory, and a
    /// plain pattern never matches a directory.
    pub fn excludes_dir_entry(&self, path: &Path) -> bool {
        let parts = self.components_below_root(path);
        match parts.split_last() {
            Some((_, above)) => above.iter().any(|c| self.matches_dir_pattern(c)),
            None => false,
        }
    }

    /// A collection's candidate unit directory: excluded when its own name
    /// matches any pattern (plain or directory), or a directory between the
    /// collection root and it matches a directory pattern.
    pub fn excludes_unit_dir(&self, path: &Path) -> bool {
        let parts = self.components_below_root(path);
        let Some((leaf, above)) = parts.split_last() else {
            return false;
        };
        self.matches_name_pattern(leaf)
            || self.matches_dir_pattern(leaf)
            || above.iter().any(|c| self.matches_dir_pattern(c))
    }
}

/// True if the NON-directory entry `path` is excluded by `set` — see
/// `Excludes::excludes_file` and the module doc comment. Both walks
/// (`staging::walk_directory`, `collection::fingerprint::walk_fingerprint`)
/// call this for every non-directory entry. A directory entry is
/// `Excludes::excludes_dir_entry`'s question, which `walk_directory` asks
/// to prune the walk; `walk_fingerprint` skips directories altogether.
pub fn is_excluded(path: &Path, set: &Excludes) -> bool {
    set.excludes_file(path) || is_socket(path)
}

/// True if `path` is a socket (never followed: a symlink to one is a
/// symlink). Sockets are left out of every unit (ADR-0012 item 270): tar
/// skips one at exit 0, so a listed socket would have no member. The
/// snapshot walk asks the same question from the file type it already
/// holds; the dirty scan, which has only the path, asks it here, so the two
/// walks still agree.
pub fn is_socket(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_socket())
}

/// The unit dotfile's own `[excludes] patterns` for the unit rooted at
/// `dir_path` — issue #49 item 2's per-unit half of "effective excludes =
/// globals + dotfile". Reads `dir_path/.tapectl-unit.toml` directly (the
/// same file `policy::resolve` reads ad hoc, and `dotfile::read_dotfile`
/// reads structurally elsewhere), so
/// both `staging::walk_directory` and
/// `collection::fingerprint::walk_fingerprint` can call this with just the
/// directory they are already walking — neither needs a new parameter.
///
/// Returns an empty Vec if the dotfile is ABSENT: a directory with no unit
/// dotfile yet (or one that predates `unit init`) must behave exactly as
/// before this fix (issue #92's absent-vs-present split — `exists()` is
/// the whole test, matching `policy::resolve`'s identical treatment).
///
/// Issue #263: a dotfile that IS present but cannot be read or parsed used
/// to collapse to the same empty Vec via `unwrap_or_default()` — silently
/// indistinguishable from "no excludes configured." That is exactly how a
/// misspelled `[excludes] pattern` (singular) key let material an operator
/// meant to exclude reach the archive as zero exclusions, get archived
/// and encrypted, and land on write-once media anyway. A dotfile present
/// but unparseable is now a loud, named `Err` that reaches the caller —
/// including the TOCTOU case where it is removed between the `exists()`
/// check and the read, which is a real failure, not "never existed."
pub fn dotfile_patterns(dir_path: &Path) -> Result<Vec<String>> {
    let dotfile_path = dir_path.join(".tapectl-unit.toml");
    if !dotfile_path.exists() {
        return Ok(Vec::new());
    }
    let patterns = crate::unit::dotfile::read_dotfile(&dotfile_path).map(|d| d.exclude_patterns)?;
    check_patterns(
        &patterns,
        &format!("{} [excludes] patterns", dotfile_path.display()),
    )?;
    Ok(patterns)
}

/// The full, compiled "effective excludes" set for `dir_path`: the caller's
/// `global_excludes` (`config.defaults.global_excludes` — issue #49 item
/// 5's other half, previously reaching the archiver only, never either walk) plus
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
pub fn effective_compiled(dir_path: &Path, global_excludes: &[String]) -> Result<Excludes> {
    check_patterns(global_excludes, "[defaults] global_excludes")?;
    let mut patterns = global_excludes.to_vec();
    patterns.extend(dotfile_patterns(dir_path)?);
    Ok(Excludes::new(dir_path, &patterns))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// A set rooted at the empty path, for the relative paths below.
    fn compile(patterns: &[String]) -> Excludes {
        Excludes::new(Path::new(""), patterns)
    }

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
    fn matching_is_case_insensitive() {
        let compiled = compile(&["thumbs.db".to_string()]);
        assert!(is_excluded(Path::new("Thumbs.db"), &compiled));
        assert!(is_excluded(Path::new("THUMBS.DB"), &compiled));
    }

    /// #542: a pattern the matcher rejects is refused by name, never
    /// dropped. A walk that dropped it would archive what the operator meant
    /// to leave out, and say nothing.
    #[test]
    fn an_invalid_pattern_is_refused_by_name_where_it_was_set() {
        let tmp = TempDir::new().unwrap();

        let err = effective_compiled(tmp.path(), &["[".to_string(), "*.tmp".to_string()])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("\"[\"") && err.contains("global_excludes"),
            "a global pattern is named with where it was set: {err}"
        );
        // A directory pattern's name is checked too.
        assert!(effective_compiled(tmp.path(), &["[a/".to_string()]).is_err());

        let dotfile = tmp.path().join(".tapectl-unit.toml");
        crate::unit::dotfile::write_dotfile(
            &dotfile,
            &crate::unit::dotfile::UnitDotfile {
                uuid: "u".into(),
                name: "n".into(),
                created: "2026-01-01T00:00:00Z".into(),
                tags: vec![],
                tenant: "t".into(),
                archive_set: None,
                checksum_mode: Some("mtime_size".into()),
                compression: Some("none".into()),
                compression_level: None,
                slice_size: None,
                warehouse_copies: None,
                allow_spanning: None,
                copies_at: None,
                exclude_patterns: vec!["*.ok".into(), "[".into()],
            },
        )
        .unwrap();
        let err = effective_compiled(tmp.path(), &[]).unwrap_err().to_string();
        assert!(
            err.contains("\"[\"") && err.contains(&dotfile.display().to_string()),
            "a dotfile pattern names the pattern and the dotfile: {err}"
        );
    }

    #[test]
    fn dotfile_patterns_is_empty_when_no_dotfile_exists() {
        let tmp = TempDir::new().unwrap();
        assert!(dotfile_patterns(tmp.path()).unwrap().is_empty());
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
                checksum_mode: Some("mtime_size".into()),
                compression: Some("none".into()),
                compression_level: None,
                slice_size: None,
                warehouse_copies: None,
                allow_spanning: None,
                copies_at: None,
                exclude_patterns: vec!["*.secret".into()],
            },
        )
        .unwrap();
        assert_eq!(
            dotfile_patterns(tmp.path()).unwrap(),
            vec!["*.secret".to_string()]
        );
    }

    /// Was `a_malformed_dotfile_yields_no_patterns_rather_than_erroring`,
    /// asserting `dotfile_patterns` swallowed a parse failure into an empty
    /// Vec. That was pinning issue #263's own defect: a PRESENT-but-
    /// unparseable dotfile is indistinguishable from "no excludes
    /// configured," which is exactly how a misspelled `[excludes] pattern`
    /// key let material reach the archive as zero exclusions and get
    /// archived onto write-once media. Inverted to assert the loud `Err`
    /// this function must now return instead — absent still stays silent
    /// (`dotfile_patterns_is_empty_when_no_dotfile_exists`, unchanged
    /// above), but present-and-broken no longer does.
    #[test]
    fn a_malformed_dotfile_is_refused_rather_than_silently_dropped() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".tapectl-unit.toml"), b"not valid toml [[[").unwrap();
        assert!(
            dotfile_patterns(tmp.path()).is_err(),
            "a dotfile present but not valid TOML must be a loud error, not a silent \
             empty Vec indistinguishable from \"no excludes configured\" (issue #263)"
        );
    }

    #[test]
    fn effective_compiled_is_empty_with_no_globals_and_no_dotfile() {
        // Issue #49 trap: the no-excludes case must behave exactly as
        // today — empty globals, no dotfile, nothing excluded.
        let tmp = TempDir::new().unwrap();
        let compiled = effective_compiled(tmp.path(), &[]).unwrap();
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
                checksum_mode: Some("mtime_size".into()),
                compression: Some("none".into()),
                compression_level: None,
                slice_size: None,
                warehouse_copies: None,
                allow_spanning: None,
                copies_at: None,
                exclude_patterns: vec!["*.secret".into()],
            },
        )
        .unwrap();

        let global_excludes = vec!["Thumbs.db".to_string()];
        let compiled = effective_compiled(tmp.path(), &global_excludes).unwrap();

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
        let compiled = effective_compiled(tmp.path(), &global_excludes).unwrap();
        assert!(is_excluded(Path::new("Thumbs.db"), &compiled));
        assert!(is_excluded(Path::new("junk.tmp"), &compiled));
        assert!(!is_excluded(Path::new("keep.txt"), &compiled));
    }

    // ── issue #359: directory patterns (`name/`) prune the whole subtree ──

    /// A fixture unit root with a `.cache` subtree at several depths and
    /// spellings, plus look-alikes that must survive.
    fn cache_fixture(root: &Path) {
        for d in [".cache/deep", "sub/.Cache/x", "sub/keep", "a/b/.cache"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        for (f, body) in [
            ("top.txt", "a"),
            (".cache/c1", "b"),
            (".cache/deep/c2", "c"),
            ("sub/.Cache/x/c3", "d"),
            ("sub/keep/k", "e"),
            ("sub/.cachefile", "f"),
            ("a/b/.cache/deepfile", "g"),
            ("a/b/ok", "h"),
        ] {
            std::fs::write(root.join(f), body).unwrap();
        }
    }

    #[test]
    fn a_directory_pattern_excludes_its_whole_subtree_at_any_depth_ignoring_case() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        cache_fixture(root);
        let set = effective_compiled(root, &[".cache/".to_string()]).unwrap();
        for excluded in [
            ".cache/c1",
            ".cache/deep/c2",
            "sub/.Cache/x/c3",
            "a/b/.cache/deepfile",
        ] {
            assert!(
                is_excluded(&root.join(excluded), &set),
                "{excluded} lies under a .cache/ directory and must be excluded"
            );
        }
        for kept in ["top.txt", "sub/keep/k", "sub/.cachefile", "a/b/ok"] {
            assert!(
                !is_excluded(&root.join(kept), &set),
                "{kept} is not under a .cache/ directory and must be kept"
            );
        }
    }

    /// The same rule whichever layer the pattern comes from: the unit's
    /// dotfile `[excludes] patterns` as well as `global_excludes`.
    #[test]
    fn a_directory_pattern_in_the_dotfile_prunes_the_subtree_too() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        cache_fixture(root);
        crate::unit::dotfile::write_dotfile(
            &root.join(".tapectl-unit.toml"),
            &crate::unit::dotfile::UnitDotfile {
                uuid: "u".into(),
                name: "n".into(),
                created: "2026-01-01T00:00:00Z".into(),
                tags: vec![],
                tenant: "t".into(),
                archive_set: None,
                checksum_mode: None,
                compression: None,
                compression_level: None,
                slice_size: None,
                warehouse_copies: None,
                allow_spanning: None,
                copies_at: None,
                exclude_patterns: vec![".cache/".into()],
            },
        )
        .unwrap();
        let set = effective_compiled(root, &[]).unwrap();
        assert!(is_excluded(&root.join(".cache/deep/c2"), &set));
        assert!(!is_excluded(&root.join("top.txt"), &set));
    }

    /// Only components BELOW the walk root are consulted: a unit that itself
    /// lives under a directory called `.cache` is not excluded wholesale.
    #[test]
    fn a_directory_pattern_never_consults_components_above_the_root() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join(".cache").join("unit");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.txt"), b"a").unwrap();
        let set = effective_compiled(&root, &[".cache/".to_string()]).unwrap();
        assert!(!is_excluded(&root.join("a.txt"), &set));
    }

    /// A directory pattern leaves out a FILE of that name as well as a
    /// directory.
    #[test]
    fn a_directory_pattern_also_excludes_a_file_of_that_name() {
        let tmp = TempDir::new().unwrap();
        let set = effective_compiled(tmp.path(), &[".cache/".to_string()]).unwrap();
        assert!(is_excluded(&tmp.path().join("sub/.cache"), &set));
    }

    /// A directory ENTRY inside a pruned subtree is excluded; the pruned
    /// directory itself is kept (an empty directory).
    #[test]
    fn excludes_dir_entry_keeps_the_pruned_directory_but_not_its_descendants() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let set = effective_compiled(root, &[".cache/".to_string(), "*.tmp".to_string()]).unwrap();
        assert!(!set.excludes_dir_entry(&root.join(".cache")));
        assert!(!set.excludes_dir_entry(&root.join("a/b/.Cache")));
        assert!(set.excludes_dir_entry(&root.join(".cache/deep")));
        assert!(set.excludes_dir_entry(&root.join("a/.cache/deep/deeper")));
        assert!(!set.excludes_dir_entry(&root.join("a/b")));
        // A plain pattern never matches a directory.
        assert!(!set.excludes_dir_entry(&root.join("x.tmp/inner")));
    }
}
