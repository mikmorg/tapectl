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

pub mod access;
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

/// The command that configures a collection, as every "none is configured"
/// line names it (ADR-0012 item 206).
pub const ADD_ONE: &str = "add one with `tapectl collection add NAME ROOT --tenant TENANT`";

/// How bad one problem a `collection sync|status|plan` meets is (ADR-0012
/// item 212, the 1.9.0 coordinator's OQ6). The command exits with the worst
/// it saw, 0 when it saw none, and every collection is processed whatever
/// another's problems are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Exit 1: the command did its work around it — an entry outside every
    /// unit, a folder refused (an invalid name, among others), a unit
    /// marked missing.
    Warning,
    /// Exit 2: a root that is not absolute or does not exist, a tenant or
    /// archive set the catalog does not know, permission denied, a dotfile
    /// that could not be written.
    Error,
}

impl Severity {
    /// The exit code for the worst of `seen`: 0 for none.
    pub fn exit_code(seen: impl IntoIterator<Item = Severity>) -> i32 {
        match seen.into_iter().max() {
            None => crate::error::EXIT_SUCCESS,
            Some(Severity::Warning) => crate::error::EXIT_WARNING,
            Some(Severity::Error) => crate::error::EXIT_ERROR,
        }
    }
}

/// What makes a configured collection unusable as a whole (ADR-0012 item
/// 220): a root that is not absolute, or does not exist or cannot be
/// read; a tenant or an archive set the catalog does not know. Empty when
/// it is usable. Each is an error of that collection alone: `collection
/// sync|status|plan` report it under the collection's name and go on to
/// the next. Read-only; at most two statements.
pub fn collection_problems(
    conn: &rusqlite::Connection,
    lib: &CollectionConfig,
) -> Result<Vec<String>> {
    let mut problems = Vec::new();
    if !std::path::Path::new(&lib.root).is_absolute() {
        problems.push(format!(
            "root \"{}\" is not an absolute path; fix it with: tapectl config set \
             'collections[{}].root' /ABSOLUTE/PATH",
            lib.root, lib.name
        ));
    } else if let Err(e) = std::fs::canonicalize(&lib.root) {
        problems.push(format!(
            "root \"{}\" does not exist or cannot be read: {e}",
            lib.root
        ));
    }
    if crate::db::queries::get_tenant_by_name(conn, &lib.tenant)?.is_none() {
        problems.push(format!(
            "tenant \"{}\" does not exist (`tapectl tenant list` names the tenants); fix it \
             with: tapectl config set 'collections[{}].tenant' TENANT",
            lib.tenant, lib.name
        ));
    }
    if let Err(e) = crate::db::queries::resolve_archive_set(conn, lib.archive_set.as_deref()) {
        problems.push(e.to_string());
    }
    Ok(problems)
}

/// A collection name: one segment, under the unit-name rule, since it
/// begins every unit name the collection registers (`"{name}/{folder}"`).
pub fn validate_collection_name(name: &str) -> Result<()> {
    let refuse = |why: String| {
        Err(TapectlError::Other(format!(
            "invalid collection name \"{name}\": {why}"
        )))
    };
    if name.contains('/') {
        return refuse("it is one segment, with no /".to_string());
    }
    if !name.is_empty() && name.chars().all(|c| c.is_ascii_digit()) {
        return refuse(format!(
            "a name of digits alone reads as a position in `tapectl config set \
             'collections[{name}].KEY'`; give it a letter, e.g. photos-{name}"
        ));
    }
    if let Err(e) = crate::naming::validate_unit_name(name) {
        return refuse(format!(
            "it begins every unit name the collection registers, so it follows the unit-name \
             rule ({e})"
        ));
    }
    Ok(())
}

/// Two collection roots overlap when either is the other or lies inside it.
/// Each is compared canonical when it resolves, as given otherwise.
pub fn roots_overlap(a: &str, b: &str) -> bool {
    let canon = |p: &str| {
        std::fs::canonicalize(p)
            .map(|c| c.to_string_lossy().into_owned())
            .unwrap_or_else(|_| p.trim_end_matches('/').to_string())
    };
    let (a, b) = (canon(a), canon(b));
    is_under_root(&a, &b) || is_under_root(&b, &a)
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
        (None, []) => Err(TapectlError::Other(format!(
            "no collection is configured, so there is none to run; {ADD_ONE}"
        ))),
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

    /// A collection name is one unit-name segment, and not digits alone,
    /// which `config set 'collections[N].KEY'` reads as a position.
    #[test]
    fn a_collection_name_is_one_segment_and_not_digits_alone() {
        assert!(validate_collection_name("photos-2024").is_ok());
        for (bad, says) in [
            ("2024", "reads as a position"),
            ("a/b", "one segment"),
            ("my photos", "unit-name rule"),
        ] {
            let err = validate_collection_name(bad).unwrap_err().to_string();
            assert!(err.contains(says), "{bad}: {err}");
        }
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
