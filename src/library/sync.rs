//! `library sync` (`docs/design/v2-open-questions.md` §11): walk a
//! library's root at `unit_depth`, registering new unit directories,
//! resolving moved/renamed ones by dotfile uuid (exactly as
//! `unit::discovery` already does), and marking vanished ones `missing` —
//! never deleting or retiring; those stay operator acts.

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};
use tracing::warn;
use walkdir::WalkDir;

use crate::config::{LibraryConfig, TapectlPaths};
use crate::db::{events, queries};
use crate::error::{Result, TapectlError};
use crate::unit::dotfile;

/// Outcome of one `library sync` run.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SyncReport {
    /// Newly registered units (fresh directories, or orphaned dotfiles the
    /// DB didn't know about yet).
    pub created: usize,
    /// Existing units whose recorded path was updated (moved/renamed,
    /// resolved by dotfile uuid).
    pub moved: usize,
    /// Existing `missing` units found again (their directory reappeared).
    pub reactivated: usize,
    /// Existing `active` units whose directory is now gone.
    pub missing: usize,
    /// Units needing archival work: no snapshot at all yet.
    pub pending: usize,
    /// Units needing archival work: a snapshot exists but is stale.
    pub dirty: usize,
}

/// Sync one library. `dry_run` computes and reports every count above
/// without mutating anything (no DB writes, no dotfile writes, no `unit
/// init` calls) — the same detection logic runs either way; only the
/// mutation step is gated per finding.
///
/// Errors from individual directories (e.g. a dotfile naming an unknown
/// tenant) are collected rather than aborting the whole sync, same spirit
/// as `unit::discovery::discover`.
pub fn sync_library(
    conn: &Connection,
    paths: &TapectlPaths,
    lib: &LibraryConfig,
    dry_run: bool,
) -> Result<(SyncReport, Vec<String>)> {
    let mut report = SyncReport::default();
    let mut errors = Vec::new();

    let root = match super::canonical_root(lib) {
        Ok(r) => r,
        Err(e) => {
            errors.push(e.to_string());
            return Ok((report, errors));
        }
    };
    let root_path = PathBuf::from(&root);

    // Step 1: walk root at unit_depth, registering/resolving each currently
    // existing directory. This MUST run before step 2 (vanished-detection)
    // — a moved unit's new location is found and its `current_path` updated
    // here, so step 2 (which re-reads `current_path` fresh from the DB)
    // sees the new, existing path and never flags it as vanished.
    for dir in candidate_unit_dirs(&root_path, lib) {
        if let Err(e) = sync_one_directory(conn, paths, lib, &root, &dir, dry_run, &mut report) {
            errors.push(format!("{}: {e}", dir.display()));
        }
    }

    // Step 2: vanished-detection. Only `active` units under this root whose
    // recorded directory no longer exists flip to `missing` — `tape_only`
    // and `retired` are deliberate operator states this never touches.
    let tracked = super::units_under_root(conn, &root)?;
    for unit in tracked.iter().filter(|u| u.status == "active") {
        let Some(path) = unit.current_path.as_deref() else {
            continue;
        };
        if !Path::new(path).is_dir() {
            report.missing += 1;
            if !dry_run {
                conn.execute(
                    "UPDATE units SET status = 'missing' WHERE id = ?1",
                    params![unit.id],
                )?;
                events::log_field_change(
                    conn,
                    "unit",
                    unit.id,
                    &unit.name,
                    "library_sync_vanished",
                    "status",
                    Some("active"),
                    "missing",
                    Some(unit.tenant_id),
                )?;
                warn!(unit = %unit.name, path, "library sync: directory vanished, marked missing");
            }
        }
    }

    // Step 3: pending-work detection, over the current DB state (in
    // dry-run mode this is the PRE-sync state, since nothing above was
    // actually written — newly-would-be-created units correctly don't
    // appear here yet, they're already counted via `report.created`).
    for p in super::fingerprint::pending_units_for_library(conn, lib)? {
        match p.reason {
            super::fingerprint::PendingReason::New => report.pending += 1,
            super::fingerprint::PendingReason::Dirty => report.dirty += 1,
        }
    }

    Ok((report, errors))
}

/// Directories at exactly `unit_depth` below `root`, excluding any whose
/// basename matches one of `lib.exclude`'s glob patterns (e.g. `*.partial`,
/// so an in-flight copy isn't registered as a unit mid-transfer).
fn candidate_unit_dirs(root: &Path, lib: &LibraryConfig) -> Vec<PathBuf> {
    let depth = lib.unit_depth.max(1);
    let patterns: Vec<glob::Pattern> = lib
        .exclude
        .iter()
        .filter_map(|p| glob::Pattern::new(p).ok())
        .collect();

    WalkDir::new(root)
        .follow_links(false)
        .min_depth(depth)
        .max_depth(depth)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_dir())
        .filter(|e| {
            let name = e.file_name().to_string_lossy();
            !patterns.iter().any(|p| p.matches(&name))
        })
        .map(|e| e.path().to_path_buf())
        .collect()
}

/// The unit name library sync assigns: `"{library}/{path relative to
/// root}"`, unique across libraries and stable across `unit_depth`.
fn library_unit_name(lib: &LibraryConfig, root: &str, abs_str: &str) -> String {
    let rel = Path::new(abs_str)
        .strip_prefix(root)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| abs_str.to_string());
    format!("{}/{}", lib.name, rel)
}

fn sync_one_directory(
    conn: &Connection,
    paths: &TapectlPaths,
    lib: &LibraryConfig,
    root: &str,
    dir: &Path,
    dry_run: bool,
    report: &mut SyncReport,
) -> Result<()> {
    let abs = std::fs::canonicalize(dir).map_err(|e| TapectlError::Other(e.to_string()))?;
    let abs_str = abs.to_string_lossy().to_string();

    if lib.dotfiles {
        let dotfile_path = abs.join(".tapectl-unit.toml");
        if dotfile_path.exists() {
            let df = dotfile::read_dotfile(&dotfile_path)?;
            match queries::get_unit_by_uuid(conn, &df.uuid)? {
                Some(existing) => resolve_existing(conn, &existing, &abs_str, dry_run, report),
                None => {
                    // Dotfile on disk, DB doesn't know it yet — adopt it
                    // verbatim (mirrors `unit::discovery`'s own "not found"
                    // branch) rather than minting a second uuid.
                    if !dry_run {
                        adopt_dotfile(conn, &df, &abs_str)?;
                    }
                    report.created += 1;
                    Ok(())
                }
            }
        } else {
            // No unit row can exist for a path with no dotfile under
            // dotfiles=true (`init_unit` always writes one) — a fresh
            // directory.
            if !dry_run {
                let name = library_unit_name(lib, root, &abs_str);
                crate::unit::init_unit(
                    conn,
                    paths,
                    &abs_str,
                    &lib.tenant,
                    Some(&name),
                    &[],
                    lib.archive_set.as_deref(),
                )?;
            }
            report.created += 1;
            Ok(())
        }
    } else {
        // Path-keyed identity: no dotfile, ever — read-only sources trade
        // away rename robustness for zero on-disk footprint (§11).
        match queries::get_unit_by_path(conn, &abs_str)? {
            Some(existing) => resolve_existing(conn, &existing, &abs_str, dry_run, report),
            None => {
                if !dry_run {
                    insert_path_keyed_unit(conn, lib, root, &abs_str)?;
                }
                report.created += 1;
                Ok(())
            }
        }
    }
}

/// A directory resolved to an already-known unit (by uuid or by path):
/// update its recorded path if it moved, and reactivate it if it was
/// previously `missing` and has now reappeared.
fn resolve_existing(
    conn: &Connection,
    existing: &crate::db::models::Unit,
    abs_str: &str,
    dry_run: bool,
    report: &mut SyncReport,
) -> Result<()> {
    if existing.current_path.as_deref() != Some(abs_str) {
        report.moved += 1;
        if !dry_run {
            queries::update_unit_path(conn, existing.id, abs_str)?;
            events::log_field_change(
                conn,
                "unit",
                existing.id,
                &existing.name,
                "library_sync_path_update",
                "current_path",
                existing.current_path.as_deref(),
                abs_str,
                Some(existing.tenant_id),
            )?;
        }
    }

    if existing.status == "missing" {
        report.reactivated += 1;
        if !dry_run {
            conn.execute(
                "UPDATE units SET status = 'active' WHERE id = ?1",
                params![existing.id],
            )?;
            events::log_field_change(
                conn,
                "unit",
                existing.id,
                &existing.name,
                "library_sync_reactivated",
                "status",
                Some("missing"),
                "active",
                Some(existing.tenant_id),
            )?;
        }
    }
    Ok(())
}

/// Register a unit whose dotfile already existed on disk but wasn't in the
/// DB yet — mirrors `unit::discovery::sync_discovered_unit`'s "not found"
/// branch: trust the dotfile's own recorded identity verbatim.
fn adopt_dotfile(conn: &Connection, df: &dotfile::UnitDotfile, dir_path: &str) -> Result<()> {
    let tenant = queries::get_tenant_by_name(conn, &df.tenant)?
        .ok_or_else(|| TapectlError::TenantNotFound(df.tenant.clone()))?;
    let unit_id = queries::insert_unit(
        conn,
        &df.uuid,
        &df.name,
        tenant.id,
        dir_path,
        &df.checksum_mode,
        true,
    )?;
    events::log_created(conn, "unit", unit_id, &df.name, Some(tenant.id))?;
    for tag in &df.tags {
        queries::add_tag_to_unit(conn, unit_id, tag)?;
    }
    Ok(())
}

/// Register a unit with no dotfile at all (`dotfiles = false`): path-keyed
/// identity. Skips with a clear error on a name collision rather than a raw
/// `UNIQUE` violation, mirroring `unit::init_bulk`'s per-directory
/// skip-and-report.
fn insert_path_keyed_unit(
    conn: &Connection,
    lib: &LibraryConfig,
    root: &str,
    abs_str: &str,
) -> Result<()> {
    let tenant = crate::tenant::require_tenant(conn, &lib.tenant)?;
    let name = library_unit_name(lib, root, abs_str);
    if queries::get_unit_by_name(conn, &name)?.is_some() {
        return Err(TapectlError::UnitAlreadyExists(name));
    }
    let uuid = uuid::Uuid::new_v4().to_string();
    let unit_id = queries::insert_unit(conn, &uuid, &name, tenant.id, abs_str, "mtime_size", true)?;
    events::log_created(conn, "unit", unit_id, &name, Some(tenant.id))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TapectlPaths;
    use crate::db;

    fn seed_tenant(conn: &Connection, name: &str) -> i64 {
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES (?1, 0, 'active')",
            params![name],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn test_lib(root: &Path, tenant: &str) -> LibraryConfig {
        LibraryConfig {
            name: "testlib".to_string(),
            root: root.to_string_lossy().to_string(),
            tenant: tenant.to_string(),
            unit_depth: 1,
            exclude: vec!["*.partial".to_string()],
            archive_set: None,
            dotfiles: true,
        }
    }

    fn paths_in(tmp: &Path) -> TapectlPaths {
        TapectlPaths::new(tmp.to_path_buf())
    }

    #[test]
    fn a_new_directory_becomes_a_unit() {
        let conn = db::open_memory().unwrap();
        seed_tenant(&conn, "media");
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("alpha")).unwrap();

        let lib = test_lib(root.path(), "media");
        let (report, errors) = sync_library(&conn, &paths_in(home.path()), &lib, false).unwrap();

        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        assert_eq!(report.created, 1);
        let unit = queries::get_unit_by_name(&conn, "testlib/alpha")
            .unwrap()
            .expect("unit must be registered");
        assert_eq!(unit.status, "active");
        assert!(root.path().join("alpha/.tapectl-unit.toml").exists());
    }

    #[test]
    fn dry_run_detects_but_never_mutates() {
        let conn = db::open_memory().unwrap();
        seed_tenant(&conn, "media");
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("alpha")).unwrap();
        std::fs::create_dir_all(root.path().join("beta")).unwrap();

        let lib = test_lib(root.path(), "media");
        let (report, errors) = sync_library(&conn, &paths_in(home.path()), &lib, true).unwrap();

        assert!(errors.is_empty());
        assert_eq!(report.created, 2, "must detect both new directories");
        assert!(
            queries::get_unit_by_name(&conn, "testlib/alpha")
                .unwrap()
                .is_none(),
            "dry-run must not insert any unit row"
        );
        assert!(
            !root.path().join("alpha/.tapectl-unit.toml").exists(),
            "dry-run must not write any dotfile"
        );
    }

    #[test]
    fn a_vanished_directory_is_marked_missing_not_deleted() {
        let conn = db::open_memory().unwrap();
        seed_tenant(&conn, "media");
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("alpha")).unwrap();

        let lib = test_lib(root.path(), "media");
        sync_library(&conn, &paths_in(home.path()), &lib, false).unwrap();
        let unit_before = queries::get_unit_by_name(&conn, "testlib/alpha")
            .unwrap()
            .unwrap();
        assert_eq!(unit_before.status, "active");

        std::fs::remove_dir_all(root.path().join("alpha")).unwrap();
        let (report, errors) = sync_library(&conn, &paths_in(home.path()), &lib, false).unwrap();
        assert!(errors.is_empty());
        assert_eq!(report.missing, 1);
        assert_eq!(report.created, 0, "must not re-create the vanished unit");

        // Still present in the DB — never deleted.
        let unit_after = queries::get_unit_by_name(&conn, "testlib/alpha")
            .unwrap()
            .expect("unit row must survive — sync never deletes");
        assert_eq!(unit_after.status, "missing");
        assert_eq!(unit_after.id, unit_before.id, "same unit, not a new row");
    }

    #[test]
    fn a_renamed_directory_resolves_to_the_same_unit_by_uuid() {
        let conn = db::open_memory().unwrap();
        seed_tenant(&conn, "media");
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("alpha")).unwrap();

        let lib = test_lib(root.path(), "media");
        sync_library(&conn, &paths_in(home.path()), &lib, false).unwrap();
        let unit_before = queries::get_unit_by_name(&conn, "testlib/alpha")
            .unwrap()
            .unwrap();

        // Rename on disk (dotfile — and its uuid — travels with the
        // directory, exactly like `unit::discovery`'s rename-proofing).
        std::fs::rename(root.path().join("alpha"), root.path().join("alpha-renamed")).unwrap();

        let (report, errors) = sync_library(&conn, &paths_in(home.path()), &lib, false).unwrap();
        assert!(errors.is_empty());
        assert_eq!(report.moved, 1);
        assert_eq!(
            report.created, 0,
            "a renamed unit must resolve by uuid, not register as new"
        );

        // Still the SAME unit row (same id, same name/uuid) — only the path
        // changed. The name stays "testlib/alpha" (names aren't
        // re-derived from path on every sync — only `unit rename` changes a
        // name), but current_path must reflect the new location.
        let unit_after = queries::get_unit_by_name(&conn, &unit_before.name)
            .unwrap()
            .expect("same unit must still resolve by its original name");
        assert_eq!(unit_after.id, unit_before.id);
        assert_eq!(unit_after.uuid, unit_before.uuid);
        assert!(unit_after
            .current_path
            .as_deref()
            .unwrap()
            .ends_with("alpha-renamed"));
    }

    #[test]
    fn a_missing_unit_reactivates_when_its_directory_reappears() {
        let conn = db::open_memory().unwrap();
        seed_tenant(&conn, "media");
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("alpha")).unwrap();
        let lib = test_lib(root.path(), "media");
        sync_library(&conn, &paths_in(home.path()), &lib, false).unwrap();

        // Simulate an external drive going away: directory removed, then a
        // sync runs and marks it missing.
        std::fs::remove_dir_all(root.path().join("alpha")).unwrap();
        sync_library(&conn, &paths_in(home.path()), &lib, false).unwrap();
        assert_eq!(
            queries::get_unit_by_name(&conn, "testlib/alpha")
                .unwrap()
                .unwrap()
                .status,
            "missing"
        );

        // The drive comes back with the exact same directory (same dotfile
        // uuid survives, since it was never deleted from disk in this
        // scenario — only removed as a filesystem entry and now restored).
        std::fs::create_dir_all(root.path().join("alpha")).unwrap();
        // The dotfile went with the directory removal in this test's
        // simulation (remove_dir_all deletes everything under it) — restore
        // it with the SAME uuid to model "the external drive was
        // unmounted, not erased."
        let original_uuid = queries::get_unit_by_name(&conn, "testlib/alpha")
            .unwrap()
            .unwrap()
            .uuid;
        dotfile::write_dotfile(
            &root.path().join("alpha/.tapectl-unit.toml"),
            &dotfile::UnitDotfile {
                uuid: original_uuid,
                name: "testlib/alpha".to_string(),
                created: chrono::Utc::now().to_rfc3339(),
                tags: vec![],
                tenant: "media".to_string(),
                archive_set: None,
                checksum_mode: "mtime_size".to_string(),
                compression: "none".to_string(),
                exclude_patterns: vec![],
            },
        )
        .unwrap();

        let (report, errors) = sync_library(&conn, &paths_in(home.path()), &lib, false).unwrap();
        assert!(errors.is_empty());
        assert_eq!(report.reactivated, 1);
        assert_eq!(
            queries::get_unit_by_name(&conn, "testlib/alpha")
                .unwrap()
                .unwrap()
                .status,
            "active"
        );
    }

    #[test]
    fn excluded_directory_names_are_never_registered() {
        let conn = db::open_memory().unwrap();
        seed_tenant(&conn, "media");
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("alpha")).unwrap();
        std::fs::create_dir_all(root.path().join("beta.partial")).unwrap();

        let lib = test_lib(root.path(), "media");
        let (report, _errors) = sync_library(&conn, &paths_in(home.path()), &lib, false).unwrap();

        assert_eq!(report.created, 1, "only the non-excluded directory");
        assert!(queries::get_unit_by_name(&conn, "testlib/alpha")
            .unwrap()
            .is_some());
        assert!(queries::get_unit_by_name(&conn, "testlib/beta.partial")
            .unwrap()
            .is_none());
    }

    #[test]
    fn dotfiles_false_uses_path_keyed_identity_with_no_dotfile_written() {
        let conn = db::open_memory().unwrap();
        seed_tenant(&conn, "media");
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("alpha")).unwrap();

        let mut lib = test_lib(root.path(), "media");
        lib.dotfiles = false;

        let (report, errors) = sync_library(&conn, &paths_in(home.path()), &lib, false).unwrap();
        assert!(errors.is_empty());
        assert_eq!(report.created, 1);
        assert!(
            !root.path().join("alpha/.tapectl-unit.toml").exists(),
            "dotfiles=false must never write a dotfile"
        );
        assert!(queries::get_unit_by_name(&conn, "testlib/alpha")
            .unwrap()
            .is_some());
    }
}
