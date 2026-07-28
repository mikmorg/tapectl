//! The Library concept (`docs/design/v2-open-questions.md` §11, finishing
//! the §7 media-library workload sketch): a factory + batch driver over
//! existing unit machinery for append-mostly media roots (thousands of
//! folder-units), so the operator configures one `[[libraries]]` block per
//! root instead of `unit init`-ing each folder by hand.
//!
//! Units remain first-class underneath — this module only automates
//! registration (`sync`), reports readiness (`status`), and batches pending
//! work into tape-sized groups (`plan`) before driving the existing
//! stage/write pipeline once per batch (`batch`).
//!
//! Deliberately out of scope (§11): filesystem watching/daemons (#13
//! verdict), scheduled sync, any cross-library dedup (#12 — full-only
//! stands), and best-fit-decreasing packing (§7 — alphabetical first-fit
//! preserves the name-ordered "tape spine").

pub mod fingerprint;
pub mod selector;
pub mod sync;

use crate::config::LibraryConfig;
use crate::db::models::Unit;
use crate::error::{Result, TapectlError};

/// All units currently registered under a library's root, any status —
/// membership is determined by path prefix against the (canonicalized)
/// root, since units carry no `library_id` (no schema change in T10; see
/// the T10 report). Both `sync`'s vanished-detection and `status`/`plan`'s
/// pending-scan build on this.
pub fn units_under_root(conn: &rusqlite::Connection, root_canonical: &str) -> Result<Vec<Unit>> {
    let all = crate::db::queries::list_units(conn, None, None)?;
    Ok(all
        .into_iter()
        .filter(|u| match &u.current_path {
            Some(p) => is_under_root(p, root_canonical),
            None => false,
        })
        .collect())
}

/// `path` is `root` itself or a descendant of it. Both are expected already
/// canonical (absolute, symlink-resolved) — string comparison only, no
/// filesystem access, so it still works for a unit whose directory has
/// since vanished.
pub fn is_under_root(path: &str, root_canonical: &str) -> bool {
    path == root_canonical || path.starts_with(&format!("{root_canonical}/"))
}

/// Canonicalize a library's configured root. Returns the plain
/// `TapectlError::Other` the callers (`sync`/`status`/`plan`) already
/// surface per-library rather than aborting a multi-library run.
pub fn canonical_root(lib: &LibraryConfig) -> Result<String> {
    std::fs::canonicalize(&lib.root)
        .map(|p| p.to_string_lossy().to_string())
        .map_err(|e| {
            TapectlError::Other(format!(
                "library \"{}\": root \"{}\" does not exist or is not readable: {e}",
                lib.name, lib.root
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_under_root_matches_root_itself_and_descendants() {
        assert!(is_under_root("/media/movies", "/media/movies"));
        assert!(is_under_root("/media/movies/foo", "/media/movies"));
        assert!(is_under_root("/media/movies/foo/bar", "/media/movies"));
    }

    #[test]
    fn is_under_root_rejects_sibling_with_shared_prefix() {
        // The classic string-prefix trap: "/media/movies2" must NOT match
        // root "/media/movies".
        assert!(!is_under_root("/media/movies2", "/media/movies"));
        assert!(!is_under_root("/media/movies2/foo", "/media/movies"));
        assert!(!is_under_root("/media/mov", "/media/movies"));
    }

    #[test]
    fn is_under_root_rejects_unrelated_path() {
        assert!(!is_under_root("/data/tv", "/media/movies"));
    }
}
