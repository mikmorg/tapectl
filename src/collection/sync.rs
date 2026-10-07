//! `collection sync` (`docs/design/v2-open-questions.md` §11): walk a
//! collection's root at `unit_depth`, registering new unit directories,
//! resolving moved/renamed ones by dotfile uuid (exactly as
//! `unit::discovery` already does), and marking vanished ones `missing` —
//! never deleting or retiring; those stay operator acts.

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};
use tracing::warn;
use walkdir::WalkDir;

use crate::config::{CollectionConfig, Config, TapectlPaths};
use crate::db::{events, queries};
use crate::error::{Result, TapectlError};
use crate::unit::dotfile;

/// Outcome of one `collection sync` run.
#[derive(Debug, Default, Clone)]
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
    /// Units refused during step 3's pending/dirty detection because their
    /// own dotfile could not be parsed (ADR-0012's 2026-09-22 amendment,
    /// issue #285) — distinct from `errors` (step 1's per-directory
    /// registration failures, an existing and unrelated mechanism): never
    /// counted in `pending`/`dirty`, and never archived.
    /// `cli::collection::cmd_sync` must report these and exit non-zero when
    /// non-empty.
    pub refused: Vec<super::fingerprint::RefusedUnit>,
    /// Entries under the root that belong to no unit (issue #382): loose
    /// files and symlinks down to `unit_depth`, and a symlinked directory
    /// where a unit folder would be. Never archived;
    /// `cli::collection::cmd_sync` names them and exits non-zero.
    pub outside: Vec<super::outside::OutsideEntry>,
    /// Known units under the root owned by another tenant than the
    /// collection's configured `tenant` (#383): `(unit name, its tenant)`.
    /// A warning, never a refusal and never a change: the catalog keeps
    /// the owner it has (a collection's `tenant` names who new units go to).
    /// `cli::collection::cmd_sync` prints them.
    pub tenant_differs: Vec<(String, String)>,
}

/// Sync one collection with BUILT-IN defaults (`Config::default()`) plus the
/// given `global_excludes` -- see `sync_collection_with_config`, which this
/// wraps. Kept for callers that hold no operator config (tests); a command
/// that has the operator's `Config` must call `sync_collection_with_config`,
/// or `[defaults] checksum_mode` is silently ignored for every unit this
/// registers (issue #347).
pub fn sync_collection(
    conn: &Connection,
    _paths: &TapectlPaths,
    lib: &CollectionConfig,
    dry_run: bool,
    global_excludes: &[String],
) -> Result<(SyncReport, Vec<String>)> {
    let mut config = Config::default();
    config.defaults.global_excludes = global_excludes.to_vec();
    sync_collection_with_config(conn, &config, lib, dry_run)
}

/// Sync one collection. `dry_run` computes and reports every count above
/// without mutating anything (no DB writes, no dotfile writes, no `unit
/// init` calls) — the same detection logic runs either way; only the
/// mutation step is gated per finding.
///
/// Errors from individual directories (e.g. a dotfile naming an unknown
/// tenant) are collected rather than aborting the whole sync, same spirit
/// as `unit::discovery::discover`.
///
/// `config` supplies two things. `config.defaults.global_excludes` (issue
/// #49) reaches step 3's `pending_units_for_collection` call so this sync's
/// pending/dirty counts agree with `unit status --dirty`/`report
/// dirty`/`collection status`. And every unit this registers -- a fresh
/// folder, a path-keyed one, or an adopted dotfile -- takes its checksum mode
/// from the resolved policy under `config` (issue #347,
/// `unit::creation_checksum_mode`); a unit already registered keeps the mode
/// it has.
pub fn sync_collection_with_config(
    conn: &Connection,
    config: &Config,
    lib: &CollectionConfig,
    dry_run: bool,
) -> Result<(SyncReport, Vec<String>)> {
    let global_excludes = &config.defaults.global_excludes;
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
    let dirs = candidate_unit_dirs(&root_path, lib);
    // ADR-0012, 2026-10-07 item 24 (#378): two directories in this walk
    // carrying one uuid, neither of them the unit's recorded directory, are
    // each refused before either is resolved (`identity::refused_groups`). A
    // dotfile that cannot be read is left to `sync_one_directory`, which
    // names it.
    let duplicated = if lib.dotfiles {
        let found: Vec<(String, PathBuf)> = dirs
            .iter()
            .filter_map(|d| {
                let df = dotfile::read_dotfile(&d.join(dotfile::UNIT_DOTFILE)).ok()?;
                Some((df.uuid, d.clone()))
            })
            .collect();
        crate::unit::identity::refused_groups(conn, &found)?
    } else {
        Default::default()
    };
    for dir in dirs {
        if let Some((uuid, group)) = duplicated.iter().find(|(_, g)| g.contains(&dir)) {
            errors.push(format!(
                "{}: {}",
                dir.display(),
                crate::unit::identity::duplicate_uuid_refusal(uuid, group)
            ));
            continue;
        }
        if let Err(e) = sync_one_directory(conn, config, lib, &root, &dir, dry_run, &mut report) {
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
                    "collection_sync_vanished",
                    "status",
                    Some("active"),
                    "missing",
                    Some(unit.tenant_id),
                )?;
                warn!(unit = %unit.name, path, "collection sync: directory vanished, marked missing");
            }
        }
    }

    // Step 3: pending-work detection, over the current DB state (in
    // dry-run mode this is the PRE-sync state, since nothing above was
    // actually written — newly-would-be-created units correctly don't
    // appear here yet, they're already counted via `report.created`).
    let scan = super::fingerprint::pending_units_for_collection(conn, lib, global_excludes)?;
    for p in &scan.pending {
        match p.reason {
            super::fingerprint::PendingReason::New => report.pending += 1,
            super::fingerprint::PendingReason::Dirty => report.dirty += 1,
        }
    }
    report.refused = scan.refused;

    // Step 4 (issue #382): what step 1's walk could never register — loose
    // files and symlinks down to `unit_depth`, and symlinked directories at
    // it. Read-only, so a dry run reports it the same.
    report.outside = super::outside::entries_outside_units(&root_path, lib);

    Ok((report, errors))
}

/// Directories at exactly `unit_depth` below `root`, excluding any that
/// `lib.exclude` excludes (e.g. `*.partial`, so an in-flight copy isn't
/// registered as a unit mid-transfer). Matched by the shared
/// `staging::exclude` rule (issue #359): case-insensitive like every other
/// exclude, a plain pattern against the candidate's own name, and a
/// directory pattern (`name/`) against the candidate and every directory
/// between it and the root, so the whole subtree is excluded.
fn candidate_unit_dirs(root: &Path, lib: &CollectionConfig) -> Vec<PathBuf> {
    let depth = lib.unit_depth.max(1);
    let excludes = crate::staging::exclude::Excludes::new(root, &lib.exclude);

    WalkDir::new(root)
        .follow_links(false)
        .min_depth(depth)
        .max_depth(depth)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_dir())
        .filter(|e| !excludes.excludes_unit_dir(e.path()))
        .map(|e| e.path().to_path_buf())
        .collect()
}

/// The unit name collection sync assigns: `"{collection}/{path relative to
/// root}"`, unique across collections and stable across `unit_depth`.
fn collection_unit_name(lib: &CollectionConfig, root: &str, abs_str: &str) -> String {
    let rel = Path::new(abs_str)
        .strip_prefix(root)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| abs_str.to_string());
    format!("{}/{}", lib.name, rel)
}

fn sync_one_directory(
    conn: &Connection,
    config: &Config,
    lib: &CollectionConfig,
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
                Some(existing) => {
                    // ADR-0012, 2026-10-07 item 24 (#378): a copy, not a move.
                    if let Some(refusal) = crate::unit::identity::copy_refusal(&existing, &abs_str)
                    {
                        return Err(TapectlError::Other(refusal));
                    }
                    resolve_existing(conn, lib, &existing, &abs_str, dry_run, report)
                }
                None => {
                    // Dotfile on disk, DB doesn't know it yet — adopt it
                    // verbatim (mirrors `unit::discovery`'s own "not found"
                    // branch) rather than minting a second uuid.
                    if !dry_run {
                        adopt_dotfile(conn, config, &df, &abs_str)?;
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
                let name = collection_unit_name(lib, root, &abs_str);
                crate::unit::init_unit_with_config(
                    conn,
                    config,
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
            Some(existing) => resolve_existing(conn, lib, &existing, &abs_str, dry_run, report),
            None => {
                if !dry_run {
                    insert_path_keyed_unit(conn, config, lib, root, &abs_str)?;
                }
                report.created += 1;
                Ok(())
            }
        }
    }
}

/// A directory resolved to an already-known unit (by uuid or by path):
/// update its recorded path if it moved, and reactivate it if it was
/// previously `missing` and has now reappeared. A unit owned by another
/// tenant than the collection's is named in `tenant_differs` (#383), and
/// keeps its owner.
fn resolve_existing(
    conn: &Connection,
    lib: &CollectionConfig,
    existing: &crate::db::models::Unit,
    abs_str: &str,
    dry_run: bool,
    report: &mut SyncReport,
) -> Result<()> {
    let owner: String = conn.query_row(
        "SELECT name FROM tenants WHERE id = ?1",
        params![existing.tenant_id],
        |r| r.get(0),
    )?;
    if owner != lib.tenant {
        warn!(
            unit = %existing.name,
            tenant = %owner,
            collection_tenant = %lib.tenant,
            "collection sync: a known unit is owned by another tenant than the collection's"
        );
        report.tenant_differs.push((existing.name.clone(), owner));
    }

    if existing.current_path.as_deref() != Some(abs_str) {
        report.moved += 1;
        if !dry_run {
            queries::update_unit_path(conn, existing.id, abs_str)?;
            events::log_field_change(
                conn,
                "unit",
                existing.id,
                &existing.name,
                "collection_sync_path_update",
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
                "collection_sync_reactivated",
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
fn adopt_dotfile(
    conn: &Connection,
    config: &Config,
    df: &dotfile::UnitDotfile,
    dir_path: &str,
) -> Result<()> {
    let tenant = queries::get_tenant_by_name(conn, &df.tenant)?
        .ok_or_else(|| TapectlError::TenantNotFound(df.tenant.clone()))?;
    // Resolve `df.archive_set` exactly as `unit::discovery::sync_discovered_unit`
    // does (issue #48 item 3). Both functions adopt a pre-existing dotfile for a
    // unit not yet in the DB, from the same field — so if only one of them read
    // it, `unit discover` and `collection sync` would silently disagree about
    // the same unit's policy depending on which command happened to register it.
    // A dotfile naming a deleted or hand-edited archive set surfaces as `Err`
    // here, which the caller treats as a per-unit failure, not a fatal scan.
    let archive_set_id = queries::resolve_archive_set(conn, df.archive_set.as_deref())?;
    // Issue #347: the resolved policy's mode, exactly as `unit discover`'s
    // adoption resolves it -- the dotfile's own when it sets one.
    let checksum_mode =
        crate::unit::creation_checksum_mode(conn, config, &df.name, archive_set_id, dir_path)?;

    let unit_id = queries::insert_unit(
        conn,
        &df.uuid,
        &df.name,
        tenant.id,
        archive_set_id,
        dir_path,
        &checksum_mode,
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
    config: &Config,
    lib: &CollectionConfig,
    root: &str,
    abs_str: &str,
) -> Result<()> {
    let tenant = crate::tenant::require_tenant(conn, &lib.tenant)?;
    let name = collection_unit_name(lib, root, abs_str);
    if queries::get_unit_by_name(conn, &name)?.is_some() {
        return Err(TapectlError::UnitAlreadyExists(name));
    }
    let uuid = uuid::Uuid::new_v4().to_string();
    // Path-keyed units have no dotfile to read an archive set from, but the
    // `CollectionConfig` itself carries one — and `sync_one_directory`'s
    // dotfile-backed path already passes `lib.archive_set` through to
    // `init_unit`. Resolving it here too keeps both branches of the same
    // `collection sync` agreeing about the collection's own configured
    // policy, rather than silently depending on `dotfiles = true/false`.
    let archive_set_id = queries::resolve_archive_set(conn, lib.archive_set.as_deref())?;
    // Issue #347: the same resolved mode `init_unit_with_config` gives the
    // dotfile-backed branch.
    let checksum_mode =
        crate::unit::creation_checksum_mode(conn, config, &name, archive_set_id, abs_str)?;

    let unit_id = queries::insert_unit(
        conn,
        &uuid,
        &name,
        tenant.id,
        archive_set_id,
        abs_str,
        &checksum_mode,
        true,
    )?;
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

    fn test_lib(root: &Path, tenant: &str) -> CollectionConfig {
        CollectionConfig {
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
        let (report, errors) =
            sync_collection(&conn, &paths_in(home.path()), &lib, false, &[]).unwrap();

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
        let (report, errors) =
            sync_collection(&conn, &paths_in(home.path()), &lib, true, &[]).unwrap();

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
        sync_collection(&conn, &paths_in(home.path()), &lib, false, &[]).unwrap();
        let unit_before = queries::get_unit_by_name(&conn, "testlib/alpha")
            .unwrap()
            .unwrap();
        assert_eq!(unit_before.status, "active");

        std::fs::remove_dir_all(root.path().join("alpha")).unwrap();
        let (report, errors) =
            sync_collection(&conn, &paths_in(home.path()), &lib, false, &[]).unwrap();
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
        sync_collection(&conn, &paths_in(home.path()), &lib, false, &[]).unwrap();
        let unit_before = queries::get_unit_by_name(&conn, "testlib/alpha")
            .unwrap()
            .unwrap();

        // Rename on disk (dotfile — and its uuid — travels with the
        // directory, exactly like `unit::discovery`'s rename-proofing).
        std::fs::rename(root.path().join("alpha"), root.path().join("alpha-renamed")).unwrap();

        let (report, errors) =
            sync_collection(&conn, &paths_in(home.path()), &lib, false, &[]).unwrap();
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

    /// ADR-0012, 2026-10-07 item 24 (#378): a directory carrying a known
    /// unit's uuid while the unit's recorded directory still exists is a
    /// copy (a restore into the collection root, say), not a move. Sync
    /// refuses it, naming both, and leaves the unit where it was.
    #[test]
    fn a_copy_of_a_known_unit_is_refused_not_taken_for_a_move() {
        let conn = db::open_memory().unwrap();
        seed_tenant(&conn, "media");
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("alpha")).unwrap();
        let lib = test_lib(root.path(), "media");
        sync_collection(&conn, &paths_in(home.path()), &lib, false, &[]).unwrap();
        let before = queries::get_unit_by_name(&conn, "testlib/alpha")
            .unwrap()
            .unwrap();

        std::fs::create_dir_all(root.path().join("alpha-restored")).unwrap();
        std::fs::copy(
            root.path().join("alpha/.tapectl-unit.toml"),
            root.path().join("alpha-restored/.tapectl-unit.toml"),
        )
        .unwrap();

        for dry_run in [true, false] {
            let (report, errors) =
                sync_collection(&conn, &paths_in(home.path()), &lib, dry_run, &[]).unwrap();
            assert_eq!(errors.len(), 1, "dry_run={dry_run}: {errors:?}");
            assert!(
                errors[0].contains("alpha-restored")
                    && errors[0].contains(before.current_path.as_deref().unwrap())
                    && errors[0].contains("copy"),
                "the refusal names both directories: {}",
                errors[0]
            );
            assert_eq!((report.moved, report.created), (0, 0));
        }
        let after = queries::get_unit_by_name(&conn, "testlib/alpha")
            .unwrap()
            .unwrap();
        assert_eq!(after.current_path, before.current_path, "never repointed");
    }

    /// #383: a collection whose configured `tenant` changed keeps the old
    /// owner for the units it already knows; sync names each one, changes
    /// nothing, and does not fail.
    #[test]
    fn a_known_unit_owned_by_another_tenant_is_named_not_changed() {
        let conn = db::open_memory().unwrap();
        seed_tenant(&conn, "media");
        seed_tenant(&conn, "family");
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("alpha")).unwrap();
        sync_collection(
            &conn,
            &paths_in(home.path()),
            &test_lib(root.path(), "media"),
            false,
            &[],
        )
        .unwrap();

        let lib = test_lib(root.path(), "family");
        let (report, errors) =
            sync_collection(&conn, &paths_in(home.path()), &lib, false, &[]).unwrap();
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(
            report.tenant_differs,
            vec![("testlib/alpha".to_string(), "media".to_string())]
        );
        let unit = queries::get_unit_by_name(&conn, "testlib/alpha")
            .unwrap()
            .unwrap();
        let owner: String = conn
            .query_row(
                "SELECT name FROM tenants WHERE id = ?1",
                params![unit.tenant_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(owner, "media", "the owner is not changed by sync");

        let (report, _) = sync_collection(
            &conn,
            &paths_in(home.path()),
            &test_lib(root.path(), "media"),
            false,
            &[],
        )
        .unwrap();
        assert!(report.tenant_differs.is_empty());
    }

    /// The other half of item 24: two directories in one walk carrying one
    /// uuid are both refused, and no unit is registered for either.
    #[test]
    fn two_directories_carrying_one_uuid_are_both_refused() {
        let conn = db::open_memory().unwrap();
        seed_tenant(&conn, "media");
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let df = dotfile::UnitDotfile {
            uuid: uuid::Uuid::new_v4().to_string(),
            name: "testlib/one".to_string(),
            created: "2026-01-01T00:00:00Z".to_string(),
            tags: vec![],
            tenant: "media".to_string(),
            archive_set: None,
            checksum_mode: None,
            compression: None,
            slice_size: None,
            warehouse_copies: None,
            exclude_patterns: vec![],
        };
        for d in ["one", "two"] {
            std::fs::create_dir_all(root.path().join(d)).unwrap();
            dotfile::write_dotfile(&root.path().join(d).join(".tapectl-unit.toml"), &df).unwrap();
        }
        let lib = test_lib(root.path(), "media");
        let (report, errors) =
            sync_collection(&conn, &paths_in(home.path()), &lib, false, &[]).unwrap();
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(errors.iter().all(|e| e.contains(&df.uuid)), "{errors:?}");
        assert_eq!(report.created, 0);
        assert!(queries::get_unit_by_uuid(&conn, &df.uuid)
            .unwrap()
            .is_none());
    }

    #[test]
    fn a_missing_unit_reactivates_when_its_directory_reappears() {
        let conn = db::open_memory().unwrap();
        seed_tenant(&conn, "media");
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("alpha")).unwrap();
        let lib = test_lib(root.path(), "media");
        sync_collection(&conn, &paths_in(home.path()), &lib, false, &[]).unwrap();

        // Simulate an external drive going away: directory removed, then a
        // sync runs and marks it missing.
        std::fs::remove_dir_all(root.path().join("alpha")).unwrap();
        sync_collection(&conn, &paths_in(home.path()), &lib, false, &[]).unwrap();
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
                checksum_mode: Some("mtime_size".to_string()),
                compression: Some("none".to_string()),
                slice_size: None,
                warehouse_copies: None,
                exclude_patterns: vec![],
            },
        )
        .unwrap();

        let (report, errors) =
            sync_collection(&conn, &paths_in(home.path()), &lib, false, &[]).unwrap();
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
        let (report, _errors) =
            sync_collection(&conn, &paths_in(home.path()), &lib, false, &[]).unwrap();

        assert_eq!(report.created, 1, "only the non-excluded directory");
        assert!(queries::get_unit_by_name(&conn, "testlib/alpha")
            .unwrap()
            .is_some());
        assert!(queries::get_unit_by_name(&conn, "testlib/beta.partial")
            .unwrap()
            .is_none());
    }

    /// Issue #359: a collection's `exclude` follows the one exclude rule --
    /// case-insensitive, like `global_excludes` and a dotfile's `[excludes]`.
    #[test]
    fn collection_excludes_ignore_case() {
        let conn = db::open_memory().unwrap();
        seed_tenant(&conn, "media");
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("alpha")).unwrap();
        std::fs::create_dir_all(root.path().join("Beta.PARTIAL")).unwrap();

        let lib = test_lib(root.path(), "media"); // exclude = ["*.partial"]
        let (report, errors) =
            sync_collection(&conn, &paths_in(home.path()), &lib, false, &[]).unwrap();
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(report.created, 1, "only alpha; Beta.PARTIAL is excluded");
        assert!(queries::get_unit_by_name(&conn, "testlib/Beta.PARTIAL")
            .unwrap()
            .is_none());
    }

    /// Issue #359: a directory pattern (`name/`) in a collection's `exclude`
    /// excludes that directory's whole subtree -- the directory itself when
    /// it is a candidate, and every candidate below it at a deeper
    /// `unit_depth`.
    #[test]
    fn a_collection_directory_pattern_excludes_the_whole_subtree() {
        let conn = db::open_memory().unwrap();
        seed_tenant(&conn, "media");
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        for d in ["shows/alpha", "shows/@eaDir", "Trash/old", "Trash/older"] {
            std::fs::create_dir_all(root.path().join(d)).unwrap();
        }

        let mut lib = test_lib(root.path(), "media");
        lib.unit_depth = 2;
        lib.exclude = vec!["trash/".to_string(), "@eadir/".to_string()];
        let (report, errors) =
            sync_collection(&conn, &paths_in(home.path()), &lib, false, &[]).unwrap();
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(report.created, 1, "only shows/alpha survives");
        assert!(queries::get_unit_by_name(&conn, "testlib/shows/alpha")
            .unwrap()
            .is_some());
    }

    /// Issue #347: adoption resolves the unit's policy to find its checksum
    /// mode, so a dotfile whose `[policy]` cannot be resolved is refused --
    /// named, per directory, and the rest of the collection still syncs --
    /// instead of being registered and failing later at `stage create`.
    #[test]
    fn adopting_a_dotfile_with_an_invalid_policy_value_is_refused_by_name() {
        let conn = db::open_memory().unwrap();
        seed_tenant(&conn, "media");
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("alpha")).unwrap();
        let bad = root.path().join("beta");
        std::fs::create_dir_all(&bad).unwrap();
        dotfile::write_dotfile(
            &bad.join(".tapectl-unit.toml"),
            &dotfile::UnitDotfile {
                uuid: uuid::Uuid::new_v4().to_string(),
                name: "testlib/beta".to_string(),
                created: "2026-01-01T00:00:00Z".to_string(),
                tags: vec![],
                tenant: "media".to_string(),
                archive_set: None,
                checksum_mode: None,
                compression: Some("zstd-ish".to_string()),
                slice_size: None,
                warehouse_copies: None,
                exclude_patterns: vec![],
            },
        )
        .unwrap();

        let lib = test_lib(root.path(), "media");
        let (report, errors) =
            sync_collection(&conn, &paths_in(home.path()), &lib, false, &[]).unwrap();
        assert_eq!(report.created, 1, "alpha still registers");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].contains(".tapectl-unit.toml") && errors[0].contains("compression"),
            "the refusal must name the dotfile and the bad key: {}",
            errors[0]
        );
        assert!(queries::get_unit_by_name(&conn, "testlib/beta")
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

        let (report, errors) =
            sync_collection(&conn, &paths_in(home.path()), &lib, false, &[]).unwrap();
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

    /// Seed an archive set and return its id.
    fn seed_archive_set(conn: &Connection, name: &str) -> i64 {
        conn.execute(
            "INSERT INTO archive_sets (name, min_copies) VALUES (?1, 3)",
            params![name],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn path_keyed_units_inherit_the_collection_s_archive_set() {
        // A path-keyed unit has no dotfile to read an archive set from, but
        // the CollectionConfig carries one — and the dotfile-backed branch of
        // the same `collection sync` already passes it through. If only one
        // branch resolved it, a collection's configured policy would silently
        // depend on `dotfiles = true/false`. Issue #48.
        let conn = db::open_memory().unwrap();
        seed_tenant(&conn, "media");
        let as_id = seed_archive_set(&conn, "bulk-media");
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("alpha")).unwrap();

        let mut lib = test_lib(root.path(), "media");
        lib.dotfiles = false;
        lib.archive_set = Some("bulk-media".to_string());

        let (_report, errors) =
            sync_collection(&conn, &paths_in(home.path()), &lib, false, &[]).unwrap();
        assert!(errors.is_empty(), "sync errors: {errors:?}");

        let unit = queries::get_unit_by_name(&conn, "testlib/alpha")
            .unwrap()
            .expect("unit must be registered");
        assert_eq!(
            unit.archive_set_id,
            Some(as_id),
            "a path-keyed unit must inherit the collection's configured archive set, \
             or resolver layer 2 stays inert for it"
        );
    }

    #[test]
    fn adopting_an_existing_dotfile_picks_up_its_archive_set() {
        // `collection sync`'s adopt path and `unit discover`'s not-found
        // branch read the SAME dotfile field in the SAME scenario. If only
        // one resolved it, the two commands would silently disagree about a
        // unit's policy depending on which happened to register it — the
        // walk-vs-validator failure shape that #33 and #36 both fixed.
        let conn = db::open_memory().unwrap();
        let tenant_id = seed_tenant(&conn, "media");
        let as_id = seed_archive_set(&conn, "bulk-media");
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("alpha");
        std::fs::create_dir_all(&dir).unwrap();

        let df = dotfile::UnitDotfile {
            uuid: uuid::Uuid::new_v4().to_string(),
            name: "testlib/alpha".to_string(),
            created: "2026-07-29T00:00:00Z".to_string(),
            tags: Vec::new(),
            tenant: "media".to_string(),
            archive_set: Some("bulk-media".to_string()),
            checksum_mode: Some("mtime_size".to_string()),
            compression: Some("none".to_string()),
            slice_size: None,
            warehouse_copies: None,
            exclude_patterns: Vec::new(),
        };

        adopt_dotfile(&conn, &Config::default(), &df, dir.to_str().unwrap()).unwrap();

        let unit = queries::get_unit_by_name(&conn, "testlib/alpha")
            .unwrap()
            .expect("unit must be registered");
        assert_eq!(unit.tenant_id, tenant_id);
        assert_eq!(
            unit.archive_set_id,
            Some(as_id),
            "adopt_dotfile must resolve the dotfile's archive_set, exactly as \
             unit::discovery::sync_discovered_unit does"
        );
    }
}
