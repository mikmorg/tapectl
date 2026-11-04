//! The Collection concept (`docs/design/v2-open-questions.md` §11, finishing
//! the §7 media-collection workload sketch): a factory + batch driver over
//! existing unit machinery for append-mostly media roots (thousands of
//! folder-units), so the operator configures one `[[collections]]` block per
//! root instead of `unit init`-ing each folder by hand.
//!
//! Units remain first-class underneath — this module only automates
//! registration (`sync`), reports readiness (`status`), and batches pending
//! work into tape-sized groups (`plan`) before driving the existing
//! stage/write pipeline once per batch (`batch`).
//!
//! Deliberately out of scope (§11): filesystem watching/daemons (#13
//! verdict), scheduled sync, any cross-collection dedup (#12 — full-only
//! stands), and best-fit-decreasing packing (§7 — alphabetical next-fit
//! preserves the name-ordered "tape spine"; issue #456).

pub mod batch;
pub mod fingerprint;
pub mod hook;
pub mod outside;
pub mod plan;
pub mod precheck;
pub mod selector;
pub mod status;
pub mod sync;

use crate::config::{CollectionConfig, Config};
use crate::db::models::Unit;
use crate::error::{Result, TapectlError};

/// All units currently registered under a collection's root, any status —
/// membership is determined by path prefix against the (canonicalized)
/// root, since units carry no `collection_id` (no schema change in T10; see
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

/// Canonicalize a collection's configured root. Returns the plain
/// `TapectlError::Other` the callers (`sync`/`status`/`plan`) already
/// surface per-collection rather than aborting a multi-collection run.
pub fn canonical_root(lib: &CollectionConfig) -> Result<String> {
    std::fs::canonicalize(&lib.root)
        .map(|p| p.to_string_lossy().to_string())
        .map_err(|e| {
            TapectlError::Other(format!(
                "collection \"{}\": root \"{}\" does not exist or is not readable: {e}",
                lib.name, lib.root
            ))
        })
}

/// Look up a configured collection by name — the one CLI surface (`collection
/// run`) that isn't spec'd in §11 as operating over every configured
/// collection at once, since executing a batch inherently targets one collection
/// (see the T10 report's "Deviations" section).
pub fn find_collection<'a>(config: &'a Config, name: &str) -> Result<&'a CollectionConfig> {
    config
        .collections
        .iter()
        .find(|l| l.name == name)
        .ok_or_else(|| TapectlError::Other(format!("no collection named \"{name}\" is configured")))
}

/// The collection `collection run [NAME]` runs (ADR-0012 item 210): the one
/// named, or, with no name, the only one configured. With none named and
/// several configured it is refused, naming them; nothing is guessed.
pub fn collection_to_run<'a>(
    config: &'a Config,
    name: Option<&str>,
) -> Result<&'a CollectionConfig> {
    match (name, config.collections.as_slice()) {
        (Some(name), _) => find_collection(config, name),
        (None, [only]) => Ok(only),
        (None, []) => Err(TapectlError::Other(
            "no collection is configured, so there is none to run ([[collections]] in \
             config.toml)"
                .to_string(),
        )),
        (None, several) => Err(TapectlError::Other(format!(
            "{} collections are configured: name the one to run, `tapectl collection run \
             NAME`, NAME one of {}",
            several.len(),
            several
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_collections(names: &[&str]) -> Config {
        let mut config = Config::default();
        for name in names {
            config.collections.push(CollectionConfig {
                name: name.to_string(),
                root: format!("/srv/{name}"),
                tenant: "t".to_string(),
                unit_depth: 1,
                exclude: Vec::new(),
                archive_set: None,
                dotfiles: true,
                snapshot_command: None,
                release_command: None,
            });
        }
        config
    }

    #[test]
    fn collection_run_without_a_name_runs_the_only_collection() {
        let one = with_collections(&["keepsake"]);
        assert_eq!(collection_to_run(&one, None).unwrap().name, "keepsake");
        assert_eq!(
            collection_to_run(&one, Some("keepsake")).unwrap().name,
            "keepsake"
        );
        let err = collection_to_run(&one, Some("movies"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no collection named \"movies\""), "{err}");
    }

    #[test]
    fn collection_run_without_a_name_refuses_naming_the_choices() {
        let two = with_collections(&["keepsake", "movies"]);
        let err = collection_to_run(&two, None).unwrap_err().to_string();
        assert!(err.contains("keepsake, movies"), "{err}");
        assert!(err.contains("collection run"), "{err}");
        assert_eq!(
            collection_to_run(&two, Some("movies")).unwrap().name,
            "movies"
        );
        let none = with_collections(&[]);
        let err = collection_to_run(&none, None).unwrap_err().to_string();
        assert!(err.contains("no collection is configured"), "{err}");
    }

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
