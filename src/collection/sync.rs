//! `collection sync` (`docs/design/v2-open-questions.md` §11): walk a
//! collection's root at `unit_depth`, registering new unit directories,
//! resolving moved/renamed ones by dotfile uuid (exactly as
//! `unit::discovery` already does), and marking vanished ones `missing` —
//! never deleting or retiring; those stay operator acts.
//!
//! A dry run makes every check the real run makes and stops short of each
//! write (ADR-0012 item 220), so the two report the same thing: what would
//! be created, what would be refused, and what is pending after.

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};
use tracing::debug;
use walkdir::WalkDir;

use crate::config::{CollectionConfig, Config, TapectlPaths};
use crate::db::{events, queries};
use crate::error::{Result, TapectlError};
use crate::unit::dotfile;

use super::fingerprint::PathOverlay;
use super::Severity;

/// One thing `collection sync` could not do: a folder it did not register
/// or resolve, or a problem with the collection as a whole, which stops its
/// sync before any folder is read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncProblem {
    pub severity: Severity,
    /// The folder, for a folder's problem; `None` for the collection's.
    pub folder: Option<String>,
    /// What went wrong and what to do; a folder's begins with its path.
    pub message: String,
}

/// Outcome of one `collection sync` run.
#[derive(Debug, Default, Clone)]
pub struct SyncReport {
    /// Newly registered units (fresh directories, or orphaned dotfiles the
    /// DB didn't know about yet): rows actually inserted, or in a dry run
    /// the ones the real run would insert.
    pub created: usize,
    /// Units registered before 1.9.0 whose dotfile was never written,
    /// completed now: the dotfile is written with the row's own uuid
    /// (ADR-0012 item 220). Never counted in `created`.
    pub completed: usize,
    /// Existing units whose recorded path was updated (moved/renamed,
    /// resolved by dotfile uuid).
    pub moved: usize,
    /// Existing `missing` units found again (their directory reappeared).
    pub reactivated: usize,
    /// Existing `active` units whose directory is now gone.
    pub missing: usize,
    /// The units `missing` counts: `(unit name, the folder that is gone)`.
    pub missing_units: Vec<(String, String)>,
    /// Pending in its first form (CONTEXT.md, Never Archived): no snapshot
    /// at all. A dry run counts here every unit it would create, as the
    /// real run, which registers them first, does.
    pub never_archived: usize,
    /// Pending in its second form: a snapshot exists but is stale.
    pub dirty: usize,
    /// Units refused during step 3's pending/dirty detection because their
    /// own dotfile could not be parsed (ADR-0012's 2026-09-22 amendment,
    /// issue #285) — distinct from `problems` (step 1's per-directory
    /// registration failures): never counted in `never_archived`/`dirty`,
    /// and never archived. `cli::collection::cmd_sync` must report these
    /// and exit non-zero when non-empty.
    pub refused: Vec<super::fingerprint::RefusedUnit>,
    /// Entries under the root that belong to no unit (issue #382): loose
    /// files and symlinks down to `unit_depth`, and a symlinked directory
    /// where a unit folder would be. Never archived;
    /// `cli::collection::cmd_sync` names them and exits non-zero.
    pub outside: Vec<super::outside::OutsideEntry>,
    /// Known units under the root owned by another tenant than the
    /// collection's configured `tenant` (#383): `(unit name, its tenant)`.
    /// A warning line, never a refusal and never a change: the catalog
    /// keeps the owner it has (a collection's `tenant` names who new units
    /// go to). It leaves the exit status as it is.
    pub tenant_differs: Vec<(String, String)>,
    /// Folders not registered or resolved, and the collection's own
    /// problems, each with its severity.
    pub problems: Vec<SyncProblem>,
}

impl SyncReport {
    /// Pending as CONTEXT.md means it: never archived or dirty.
    pub fn pending(&self) -> usize {
        self.never_archived + self.dirty
    }

    /// The folders this sync refused: the `M` of `would create N; M
    /// refused`.
    pub fn refused_folders(&self) -> usize {
        self.problems.iter().filter(|p| p.folder.is_some()).count()
    }

    /// Every severity this sync saw: its problems', and a warning each for
    /// a unit refused, an entry outside every unit and a unit marked
    /// missing (ADR-0012 item 212; the coordinator's OQ6).
    pub fn severities(&self) -> Vec<Severity> {
        let mut seen: Vec<Severity> = self.problems.iter().map(|p| p.severity).collect();
        if !self.refused.is_empty() || !self.outside.is_empty() || self.missing > 0 {
            seen.push(Severity::Warning);
        }
        seen
    }
}

/// Sync one collection with BUILT-IN defaults (`Config::default()`) plus the
/// given `global_excludes` -- see `sync_collection_with_config`, which this
/// wraps, beside every problem's message. Kept for callers that hold no
/// operator config (tests); a command that has the operator's `Config` must
/// call `sync_collection_with_config`, or `[defaults] checksum_mode` is
/// silently ignored for every unit this registers (issue #347).
pub fn sync_collection(
    conn: &Connection,
    _paths: &TapectlPaths,
    lib: &CollectionConfig,
    dry_run: bool,
    global_excludes: &[String],
) -> Result<(SyncReport, Vec<String>)> {
    let mut config = Config::default();
    config.defaults.global_excludes = global_excludes.to_vec();
    let report = sync_collection_with_config(conn, &config, lib, dry_run)?;
    let messages = report.problems.iter().map(|p| p.message.clone()).collect();
    Ok((report, messages))
}

/// Sync one collection. `dry_run` computes and reports every count above
/// without mutating anything (no DB writes, no dotfile writes) — the same
/// checks run either way, the registration checks included, and only the
/// writes are gated, so a dry run refuses what the real run refuses
/// (ADR-0012 item 220). A dry run cannot know what the real run's write
/// would meet, so it asks the kernel whether this user may write each
/// folder it would write a dotfile into.
///
/// A collection whose root, tenant or archive set is wrong
/// (`collection::collection_problems`) is not walked at all: its problems
/// are returned as errors and every count stays 0. A problem with one
/// folder is that folder's ([`SyncProblem`]), and the rest still sync.
/// An `Err` is left for what says nothing about any one collection (the
/// catalog failing).
///
/// `config` supplies two things. `config.defaults.global_excludes` (issue
/// #49) reaches step 3's `pending_units_for_collection` call so this sync's
/// pending/dirty counts agree with `unit info --dirty`/`report
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
) -> Result<SyncReport> {
    let global_excludes = &config.defaults.global_excludes;
    let mut report = SyncReport::default();

    let problems = super::collection_problems(conn, lib)?;
    let root = match problems.is_empty() {
        true => super::canonical_root(lib).map_err(|e| vec![e.to_string()]),
        false => Err(problems),
    };
    let root = match root {
        Ok(root) => root,
        Err(problems) => {
            report.problems = problems
                .into_iter()
                .map(|message| SyncProblem {
                    severity: Severity::Error,
                    folder: None,
                    message,
                })
                .collect();
            return Ok(report);
        }
    };
    let root_path = PathBuf::from(&root);

    // Step 1: walk root at unit_depth, registering/resolving each currently
    // existing directory. This MUST run before step 2 (vanished-detection)
    // — a moved unit's new location is found and its `current_path` updated
    // here, so step 2 (which re-reads `current_path` fresh from the DB)
    // sees the new, existing path and never flags it as vanished. A dry run
    // records each move and reactivation in `overlay` instead, so steps 2
    // and 3 see what the real run would.
    let (dirs, unreadable) = candidate_unit_dirs(&root_path, lib);
    report.problems.extend(unreadable);
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
    let mut overlay = PathOverlay::default();
    for dir in dirs {
        if let Some((uuid, group)) = duplicated.iter().find(|(_, g)| g.contains(&dir)) {
            report.problems.push(folder_problem(
                &dir,
                Severity::Warning,
                crate::unit::identity::duplicate_uuid_refusal(uuid, group),
            ));
            continue;
        }
        let mut sync = DirSync {
            conn,
            config,
            lib,
            root: &root,
            dry_run,
            report: &mut report,
            overlay: &mut overlay,
        };
        if let Err(problem) = sync.one_directory(&dir) {
            report.problems.push(problem);
        }
    }

    // Step 2: vanished-detection. Only `active` units under this root whose
    // recorded directory no longer exists flip to `missing` — `tape_only`
    // and `retired` are deliberate operator states this never touches. A
    // unit step 1 found moved is not gone, though a dry run has not
    // repointed its row.
    let tracked = super::units_under_root(conn, &root)?;
    for unit in tracked.iter().filter(|u| u.status == "active") {
        if overlay.moved.contains_key(&unit.id) {
            continue;
        }
        let Some(path) = unit.current_path.as_deref() else {
            continue;
        };
        if !Path::new(path).is_dir() {
            report.missing += 1;
            report
                .missing_units
                .push((unit.name.clone(), path.to_string()));
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
                // `cli::collection` prints the operator's line for it.
                debug!(unit = %unit.name, path, "collection sync: directory vanished, marked missing");
            }
        }
    }

    // Step 3: pending-work detection, over the catalog as the real run
    // leaves it: a dry run reads it through `overlay` (its moves and
    // reactivations), and counts every unit it would create as never
    // archived, as the real run registers them before this step.
    let scan =
        super::fingerprint::pending_units_with_overlay(conn, lib, global_excludes, &overlay)?;
    for p in &scan.pending {
        match p.reason {
            super::fingerprint::PendingReason::New => report.never_archived += 1,
            super::fingerprint::PendingReason::Dirty => report.dirty += 1,
        }
    }
    if dry_run {
        report.never_archived += report.created;
    }
    report.refused = scan.refused;

    // Step 4 (issue #382): what step 1's walk could never register — loose
    // files and symlinks down to `unit_depth`, and symlinked directories at
    // it. Read-only, so a dry run reports it the same.
    report.outside = super::outside::entries_outside_units(&root_path, lib);

    Ok(report)
}

/// A folder's problem, its message led by the folder's path.
fn folder_problem(dir: &Path, severity: Severity, why: impl std::fmt::Display) -> SyncProblem {
    SyncProblem {
        severity,
        folder: Some(dir.display().to_string()),
        message: format!("{}: {why}", dir.display()),
    }
}

/// How bad a failure registering one folder is (OQ6): an error when the
/// fix is outside the folder's own name and contents (permission, a tenant
/// or archive set the catalog does not know, the catalog itself), a warning
/// for the rest (a name, a nesting, a dotfile that does not parse, a copy).
fn severity_of(e: &TapectlError) -> Severity {
    match e {
        TapectlError::Io(io) if io.kind() == std::io::ErrorKind::PermissionDenied => {
            Severity::Error
        }
        TapectlError::TenantNotFound(_)
        | TapectlError::Database(_)
        | TapectlError::CatalogBusy(_) => Severity::Error,
        _ => Severity::Warning,
    }
}

/// Whether this process (its effective ids) may `mode` the path.
fn may(path: &Path, mode: nix::unistd::AccessFlags) -> std::result::Result<(), std::io::Error> {
    nix::unistd::faccessat(None, path, mode, nix::fcntl::AtFlags::AT_EACCESS)
        .map_err(|errno| std::io::Error::from_raw_os_error(errno as i32))
}

/// The refusal for a folder this user cannot read, with the grant.
fn unreadable(dir: &Path, cause: &std::io::Error) -> SyncProblem {
    let user = crate::unit::effective_user_name();
    folder_problem(
        dir,
        Severity::Error,
        format!(
            "cannot read it as user {user}: {cause}; grant it: sudo setfacl -R -m u:{user}:rX {}",
            crate::unit::shell_word(&dir.display().to_string())
        ),
    )
}

/// Directories at exactly `unit_depth` below `root`, excluding any that
/// `lib.exclude` excludes (e.g. `*.partial`, so an in-flight copy isn't
/// registered as a unit mid-transfer). Matched by the shared
/// `staging::exclude` rule (issue #359): case-insensitive like every other
/// exclude, a plain pattern against the candidate's own name, and a
/// directory pattern (`name/`) against the candidate and every directory
/// between it and the root, so the whole subtree is excluded.
///
/// Beside them, each directory down to that depth this user may not read,
/// an error naming the grant (ADR-0012 item 220): a folder the walk cannot
/// enter hides every unit under it.
fn candidate_unit_dirs(root: &Path, lib: &CollectionConfig) -> (Vec<PathBuf>, Vec<SyncProblem>) {
    let depth = lib.unit_depth.max(1);
    let excludes = crate::staging::exclude::Excludes::new(root, &lib.exclude);

    let mut dirs = Vec::new();
    let mut unreadable_dirs = Vec::new();
    for entry in WalkDir::new(root)
        .follow_links(false)
        .min_depth(depth)
        .max_depth(depth)
    {
        match entry {
            Ok(e) => {
                if e.file_type().is_dir() && !excludes.excludes_unit_dir(e.path()) {
                    dirs.push(e.path().to_path_buf());
                }
            }
            Err(err) => {
                let Some(path) = err.path().map(Path::to_path_buf) else {
                    continue;
                };
                // A directory removed mid-walk is not this run's to report.
                if let Some(io) = err.io_error() {
                    if io.kind() == std::io::ErrorKind::PermissionDenied
                        && !excludes.excludes_unit_dir(&path)
                    {
                        unreadable_dirs.push(unreadable(&path, io));
                    }
                }
            }
        }
    }
    (dirs, unreadable_dirs)
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

/// One step-1 pass over a folder, and what it shares with the walk.
struct DirSync<'a> {
    conn: &'a Connection,
    config: &'a Config,
    lib: &'a CollectionConfig,
    root: &'a str,
    dry_run: bool,
    report: &'a mut SyncReport,
    overlay: &'a mut PathOverlay,
}

impl DirSync<'_> {
    fn one_directory(&mut self, dir: &Path) -> std::result::Result<(), SyncProblem> {
        let fail = |e: TapectlError| folder_problem(dir, severity_of(&e), e);
        let abs = std::fs::canonicalize(dir).map_err(|e| fail(TapectlError::Io(e)))?;
        let abs_str = abs.to_string_lossy().to_string();
        // Every unit is read whole at its snapshot; a folder this user cannot
        // enter is refused now, with the grant, not registered to fail there.
        if let Err(cause) = may(
            &abs,
            nix::unistd::AccessFlags::R_OK | nix::unistd::AccessFlags::X_OK,
        ) {
            return Err(unreadable(dir, &cause));
        }

        if !self.lib.dotfiles {
            // Path-keyed identity: no dotfile, ever — read-only sources trade
            // away rename robustness for zero on-disk footprint (§11).
            return match queries::get_unit_by_path(self.conn, &abs_str).map_err(fail)? {
                Some(existing) => self.resolve_existing(&existing, &abs_str).map_err(fail),
                None => {
                    let name = self.checked_name(dir, &abs_str)?;
                    let new =
                        plan_path_keyed_unit(self.conn, self.config, self.lib, &name, &abs_str)
                            .map_err(fail)?;
                    if !self.dry_run {
                        write_path_keyed_unit(self.conn, &new).map_err(fail)?;
                    }
                    self.report.created += 1;
                    Ok(())
                }
            };
        }

        let dotfile_path = abs.join(dotfile::UNIT_DOTFILE);
        if dotfile_path.exists() {
            let df = dotfile::read_dotfile(&dotfile_path).map_err(fail)?;
            return match queries::get_unit_by_uuid(self.conn, &df.uuid).map_err(fail)? {
                Some(existing) => {
                    // ADR-0012, 2026-10-07 item 24 (#378): a copy, not a move.
                    if let Some(refusal) = crate::unit::identity::copy_refusal(&existing, &abs_str)
                    {
                        return Err(folder_problem(dir, Severity::Warning, refusal));
                    }
                    self.resolve_existing(&existing, &abs_str).map_err(fail)
                }
                None => {
                    // Dotfile on disk, DB doesn't know it yet — adopt it
                    // verbatim (mirrors `unit::discovery`'s own "not found"
                    // branch) rather than minting a second uuid.
                    let adoption = plan_adoption(self.conn, self.config, &df, &abs_str)
                        .map_err(|(severity, why)| folder_problem(dir, severity, why))?;
                    if !self.dry_run {
                        write_adoption(self.conn, &df, &abs_str, &adoption).map_err(fail)?;
                    }
                    self.report.created += 1;
                    Ok(())
                }
            };
        }

        // ADR-0012 item 220: a row at exactly this folder with no dotfile is
        // a unit a dotfile write left half-registered before 1.9.0. It is
        // completed with the row's own identity, never called nested in
        // itself.
        if let Some(stuck) = queries::get_unit_by_path(self.conn, &abs_str).map_err(fail)? {
            if self.dry_run {
                self.writable(dir, &abs_str)?;
            } else {
                crate::unit::complete_registration(self.conn, &stuck)
                    .map_err(|e| self.unwritable(dir, &e))?;
            }
            self.report.completed += 1;
            return self.resolve_existing(&stuck, &abs_str).map_err(fail);
        }

        // A fresh directory.
        let name = self.checked_name(dir, &abs_str)?;
        let new = crate::unit::plan_new_unit(
            self.conn,
            self.config,
            &abs_str,
            &self.lib.tenant,
            Some(&name),
            &[],
            self.lib.archive_set.as_deref(),
        )
        .map_err(fail)?;
        if self.dry_run {
            self.writable(dir, &abs_str)?;
        } else {
            match crate::unit::write_new_unit(self.conn, &new) {
                Ok(_) => {}
                Err(crate::unit::RegisterError::Catalog(e)) => return Err(fail(e)),
                Err(crate::unit::RegisterError::Dotfile(e)) => return Err(self.unwritable(dir, &e)),
            }
        }
        self.report.created += 1;
        Ok(())
    }

    /// The name this folder would be registered under, refused (a warning,
    /// in a dry run as in the real one) when it is not a valid unit name,
    /// with the two ways out.
    fn checked_name(&self, dir: &Path, abs_str: &str) -> std::result::Result<String, SyncProblem> {
        let name = collection_unit_name(self.lib, self.root, abs_str);
        crate::naming::validate_unit_name(&name).map_err(|e| {
            let folder = dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            folder_problem(
                dir,
                Severity::Warning,
                format!(
                    "{e} — rename the folder, or exclude it: tapectl config add \
                     'collections[{}].exclude' {}",
                    self.lib.name,
                    crate::unit::shell_word(&folder)
                ),
            )
        })?;
        Ok(name)
    }

    /// A dry run's stand-in for writing the dotfile: whether this user may.
    fn writable(&self, dir: &Path, abs_str: &str) -> std::result::Result<(), SyncProblem> {
        may(
            Path::new(abs_str),
            nix::unistd::AccessFlags::W_OK | nix::unistd::AccessFlags::X_OK,
        )
        .map_err(|cause| {
            self.unwritable(
                dir,
                &crate::unit::DotfileUnwritable {
                    dir: abs_str.to_string(),
                    cause: TapectlError::Io(cause),
                },
            )
        })
    }

    /// A dotfile that could not be written: the grant, and the collection's
    /// other way out (ADR-0012 item 220).
    fn unwritable(&self, dir: &Path, e: &crate::unit::DotfileUnwritable) -> SyncProblem {
        folder_problem(
            dir,
            Severity::Error,
            format!(
                "{}; or, for a source tapectl must not write, set the collection's dotfiles = \
                 false: tapectl config set 'collections[{}].dotfiles' false",
                e.describe(),
                self.lib.name
            ),
        )
    }

    /// A directory resolved to an already-known unit (by uuid or by path):
    /// update its recorded path if it moved, and reactivate it if it was
    /// previously `missing` and has now reappeared. A unit owned by another
    /// tenant than the collection's is named in `tenant_differs` (#383), and
    /// keeps its owner. Each move and reactivation is also put in the
    /// overlay, which is how a dry run's later steps see it.
    fn resolve_existing(
        &mut self,
        existing: &crate::db::models::Unit,
        abs_str: &str,
    ) -> Result<()> {
        let conn = self.conn;
        let owner: String = conn.query_row(
            "SELECT name FROM tenants WHERE id = ?1",
            params![existing.tenant_id],
            |r| r.get(0),
        )?;
        if owner != self.lib.tenant {
            // `cli::collection` prints the operator's line for it.
            debug!(
                unit = %existing.name,
                tenant = %owner,
                collection_tenant = %self.lib.tenant,
                "collection sync: a known unit is owned by another tenant than the collection's"
            );
            self.report
                .tenant_differs
                .push((existing.name.clone(), owner));
        }

        if existing.current_path.as_deref() != Some(abs_str) {
            self.report.moved += 1;
            self.overlay.moved.insert(existing.id, abs_str.to_string());
            if !self.dry_run {
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
            self.report.reactivated += 1;
            self.overlay.reactivated.insert(existing.id);
            if !self.dry_run {
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
}

/// What adopting a dotfile the catalog does not know resolves to, before
/// anything is written.
struct Adoption {
    tenant_id: i64,
    archive_set_id: Option<i64>,
    checksum_mode: String,
}

/// Every check adopting `df` makes (a dry run's as well as the real run's):
/// its tenant and archive set must exist (errors), its name must be free
/// and its policy resolvable (warnings).
fn plan_adoption(
    conn: &Connection,
    config: &Config,
    df: &dotfile::UnitDotfile,
    dir_path: &str,
) -> std::result::Result<Adoption, (Severity, String)> {
    let catalog = |e: TapectlError| (severity_of(&e), e.to_string());
    let tenant = queries::get_tenant_by_name(conn, &df.tenant)
        .map_err(catalog)?
        .ok_or_else(|| {
            (
                Severity::Error,
                format!(
                    "its {} names tenant \"{}\", which does not exist",
                    dotfile::UNIT_DOTFILE,
                    df.tenant
                ),
            )
        })?;
    // Resolve `df.archive_set` exactly as `unit::discovery::sync_discovered_unit`
    // does (issue #48 item 3). Both functions adopt a pre-existing dotfile for a
    // unit not yet in the DB, from the same field — so if only one of them read
    // it, `unit discover` and `collection sync` would silently disagree about
    // the same unit's policy depending on which command happened to register it.
    // A dotfile naming a deleted or hand-edited archive set is refused here, as
    // a per-unit failure, not a fatal scan.
    let archive_set_id = queries::resolve_archive_set(conn, df.archive_set.as_deref())
        .map_err(|e| (Severity::Error, e.to_string()))?;
    if queries::get_unit_by_name(conn, &df.name)
        .map_err(catalog)?
        .is_some()
    {
        return Err(catalog(TapectlError::UnitAlreadyExists(df.name.clone())));
    }
    // Issue #347: the resolved policy's mode, exactly as `unit discover`'s
    // adoption resolves it -- the dotfile's own when it sets one.
    let checksum_mode =
        crate::unit::creation_checksum_mode(conn, config, &df.name, archive_set_id, dir_path)
            .map_err(catalog)?;
    Ok(Adoption {
        tenant_id: tenant.id,
        archive_set_id,
        checksum_mode,
    })
}

/// Register a unit whose dotfile already existed on disk but wasn't in the
/// DB yet — mirrors `unit::discovery::sync_discovered_unit`'s "not found"
/// branch: trust the dotfile's own recorded identity verbatim. One change.
fn write_adoption(
    conn: &Connection,
    df: &dotfile::UnitDotfile,
    dir_path: &str,
    adoption: &Adoption,
) -> Result<()> {
    crate::unit::atomically(conn, || {
        let unit_id = queries::insert_unit(
            conn,
            &df.uuid,
            &df.name,
            adoption.tenant_id,
            adoption.archive_set_id,
            dir_path,
            &adoption.checksum_mode,
            true,
        )?;
        events::log_created(conn, "unit", unit_id, &df.name, Some(adoption.tenant_id))?;
        for tag in &df.tags {
            queries::add_tag_to_unit(conn, unit_id, tag)?;
        }
        // #493 (ADR-0012 item 148): registration with the source present
        // freezes the adopted dotfile's own `[policy.copies_at]`, from the
        // copy of it already read, so a unit adopted and then lost keeps its
        // targets.
        crate::policy::store_frozen_copies_at(
            conn,
            unit_id,
            &df.name,
            adoption.tenant_id,
            None,
            df.copies_at.as_ref(),
        )?;
        Ok(())
    })
}

/// [`plan_adoption`] then [`write_adoption`], for a caller that holds a
/// dotfile and wants it registered.
#[cfg(test)]
fn adopt_dotfile(
    conn: &Connection,
    config: &Config,
    df: &dotfile::UnitDotfile,
    dir_path: &str,
) -> Result<()> {
    let adoption =
        plan_adoption(conn, config, df, dir_path).map_err(|(_, why)| TapectlError::Other(why))?;
    write_adoption(conn, df, dir_path, &adoption)
}

/// A path-keyed unit about to be registered (`dotfiles = false`).
struct PathKeyedUnit {
    name: String,
    path: String,
    tenant_id: i64,
    archive_set_id: Option<i64>,
    checksum_mode: String,
}

/// Every check registering a unit with no dotfile at all (`dotfiles =
/// false`, path-keyed identity) makes. Skips with a clear error on a name
/// collision rather than a raw `UNIQUE` violation, mirroring
/// `unit::init_bulk`'s per-directory skip-and-report. `name` is already
/// validated (`DirSync::checked_name`), as the dotfile branch's is.
fn plan_path_keyed_unit(
    conn: &Connection,
    config: &Config,
    lib: &CollectionConfig,
    name: &str,
    abs_str: &str,
) -> Result<PathKeyedUnit> {
    let tenant = crate::tenant::require_tenant(conn, &lib.tenant)?;
    if queries::get_unit_by_name(conn, name)?.is_some() {
        return Err(TapectlError::UnitAlreadyExists(name.to_string()));
    }
    // Path-keyed units have no dotfile to read an archive set from, but the
    // `CollectionConfig` itself carries one — and the dotfile-backed branch
    // of the same `collection sync` already passes `lib.archive_set` through
    // to `init_unit`. Resolving it here too keeps both branches of the same
    // `collection sync` agreeing about the collection's own configured
    // policy, rather than silently depending on `dotfiles = true/false`.
    let archive_set_id = queries::resolve_archive_set(conn, lib.archive_set.as_deref())?;
    // Issue #347: the same resolved mode `init_unit_with_config` gives the
    // dotfile-backed branch.
    let checksum_mode =
        crate::unit::creation_checksum_mode(conn, config, name, archive_set_id, abs_str)?;
    Ok(PathKeyedUnit {
        name: name.to_string(),
        path: abs_str.to_string(),
        tenant_id: tenant.id,
        archive_set_id,
        checksum_mode,
    })
}

fn write_path_keyed_unit(conn: &Connection, new: &PathKeyedUnit) -> Result<()> {
    let uuid = uuid::Uuid::new_v4().to_string();
    crate::unit::atomically(conn, || {
        let unit_id = queries::insert_unit(
            conn,
            &uuid,
            &new.name,
            new.tenant_id,
            new.archive_set_id,
            &new.path,
            &new.checksum_mode,
            true,
        )?;
        events::log_created(conn, "unit", unit_id, &new.name, Some(new.tenant_id))?;
        Ok(())
    })
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
            snapshot_command: None,
            release_command: None,
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
            allow_spanning: None,
            copies_at: None,
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
                allow_spanning: None,
                copies_at: None,
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
                allow_spanning: None,
                copies_at: None,
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
            allow_spanning: None,
            copies_at: None,
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

    /// #478 (ADR-0012 item 82): `collection sync` adopts a dotfile carrying
    /// `[policy] allow_spanning`, keeps the file as it was, and the unit's
    /// resolved policy then reads it, over its archive set's silence.
    #[test]
    fn adopting_a_dotfile_with_allow_spanning_keeps_and_resolves_it() {
        let conn = db::open_memory().unwrap();
        seed_tenant(&conn, "media");
        seed_archive_set(&conn, "bulk-media");
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let dir = root.path().join("alpha");
        std::fs::create_dir_all(&dir).unwrap();
        let df = dotfile::UnitDotfile {
            uuid: uuid::Uuid::new_v4().to_string(),
            name: "testlib/alpha".to_string(),
            created: "2026-07-29T00:00:00Z".to_string(),
            tags: Vec::new(),
            tenant: "media".to_string(),
            archive_set: Some("bulk-media".to_string()),
            checksum_mode: None,
            compression: None,
            slice_size: None,
            warehouse_copies: None,
            allow_spanning: Some(true),
            copies_at: None,
            exclude_patterns: Vec::new(),
        };
        let path = dir.join(dotfile::UNIT_DOTFILE);
        dotfile::write_dotfile(&path, &df).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();

        let (_report, errors) = sync_collection(
            &conn,
            &paths_in(home.path()),
            &test_lib(root.path(), "media"),
            false,
            &[],
        )
        .unwrap();
        assert!(errors.is_empty(), "sync errors: {errors:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), written);

        let unit = queries::get_unit_by_name(&conn, "testlib/alpha")
            .unwrap()
            .expect("unit must be adopted");
        let policy = crate::policy::resolve(&conn, &Config::default(), &unit).unwrap();
        assert!(policy.allow_spanning);
    }

    /// #493 (ADR-0012 item 148): adopting a dotfile is a freeze point. The
    /// unit's row carries the dotfile's own `[policy.copies_at]` from the
    /// moment it is registered, so when the source later goes (here its
    /// folder is removed) the unit still resolves to those targets rather
    /// than to its archive set's. A dotfile that sets none freezes nothing.
    #[test]
    fn adopting_a_dotfile_freezes_its_copies_at_for_when_the_source_is_gone() {
        let conn = db::open_memory().unwrap();
        seed_tenant(&conn, "media");
        seed_archive_set(&conn, "bulk-media");
        conn.execute_batch(
            "INSERT INTO locations (name) VALUES ('home');
             INSERT INTO locations (name) VALUES ('offsite');
             UPDATE archive_sets SET copies_at = '{\"home\":3}' WHERE name = 'bulk-media';",
        )
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let df = |name: &str, copies_at: Option<&[(&str, i64)]>| dotfile::UnitDotfile {
            uuid: uuid::Uuid::new_v4().to_string(),
            name: format!("testlib/{name}"),
            created: "2026-10-24T00:00:00Z".to_string(),
            tags: Vec::new(),
            tenant: "media".to_string(),
            archive_set: Some("bulk-media".to_string()),
            checksum_mode: None,
            compression: None,
            slice_size: None,
            warehouse_copies: None,
            allow_spanning: None,
            copies_at: copies_at.map(|m| m.iter().map(|(n, c)| (n.to_string(), *c)).collect()),
            exclude_patterns: Vec::new(),
        };
        for (name, copies_at) in [
            ("alpha", Some(&[("home", 1), ("offsite", 1)][..])),
            ("bravo", None),
        ] {
            let dir = root.path().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            dotfile::write_dotfile(&dir.join(dotfile::UNIT_DOTFILE), &df(name, copies_at)).unwrap();
        }

        let (_report, errors) = sync_collection(
            &conn,
            &paths_in(home.path()),
            &test_lib(root.path(), "media"),
            false,
            &[],
        )
        .unwrap();
        assert!(errors.is_empty(), "sync errors: {errors:?}");

        let unit = |name: &str| {
            queries::get_unit_by_name(&conn, &format!("testlib/{name}"))
                .unwrap()
                .expect("adopted")
        };
        assert_eq!(
            unit("alpha").copies_at_frozen.as_deref(),
            Some(r#"{"home":1,"offsite":1}"#)
        );
        assert_eq!(
            unit("bravo").copies_at_frozen,
            None,
            "no key, nothing frozen"
        );

        std::fs::remove_dir_all(root.path().join("alpha")).unwrap();
        let resolved = crate::policy::resolve(&conn, &Config::default(), &unit("alpha"))
            .unwrap()
            .copies_at;
        assert_eq!(
            resolved,
            [("home".to_string(), 1), ("offsite".to_string(), 1)]
                .into_iter()
                .collect(),
            "the source is gone: the frozen dotfile layer, not the set's home = 3"
        );
    }

    // ── ADR-0012 item 220 ──

    fn chmod(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// Whether a read-only directory stops this user writing in it: mode
    /// bits do not bind root, nor some filesystems. Probed, not guessed.
    fn modes_bind(dir: &Path) -> bool {
        let probe = dir.join(".probe");
        std::fs::create_dir(&probe).unwrap();
        chmod(&probe, 0o500);
        let binds = std::fs::write(probe.join("x"), b"x").is_err();
        chmod(&probe, 0o700);
        std::fs::remove_dir_all(&probe).unwrap();
        binds
    }

    fn unit_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM units", [], |r| r.get(0))
            .unwrap()
    }

    /// The half-registered unit: a folder this user cannot write gets no
    /// row (the dry run predicts the same refusal), the error names the
    /// grant and `dotfiles = false`, and once the folder is writable the
    /// next sync registers it.
    #[test]
    fn a_dotfile_that_cannot_be_written_leaves_no_row_and_a_later_sync_registers_it() {
        let conn = db::open_memory().unwrap();
        seed_tenant(&conn, "media");
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        if !modes_bind(root.path()) {
            eprintln!("SKIPPED: a read-only directory does not stop this user (root?)");
            return;
        }
        let alpha = root.path().join("alpha");
        std::fs::create_dir_all(&alpha).unwrap();
        chmod(&alpha, 0o555);
        let lib = test_lib(root.path(), "media");

        for dry_run in [true, false] {
            let (report, errors) =
                sync_collection(&conn, &paths_in(home.path()), &lib, dry_run, &[]).unwrap();
            assert_eq!(report.created, 0, "dry_run={dry_run}");
            assert_eq!(report.refused_folders(), 1, "dry_run={dry_run}");
            assert_eq!(report.problems[0].severity, Severity::Error);
            let user = crate::unit::effective_user_name();
            assert!(
                errors[0].contains(&format!("setfacl -m u:{user}:rwX"))
                    && errors[0].contains("dotfiles = false")
                    && errors[0]
                        .contains("tapectl config set 'collections[testlib].dotfiles' false"),
                "dry_run={dry_run}: {}",
                errors[0]
            );
            assert_eq!(unit_count(&conn), 0, "dry_run={dry_run}: no row is left");
        }
        assert!(!alpha.join(dotfile::UNIT_DOTFILE).exists());

        chmod(&alpha, 0o755);
        let (report, errors) =
            sync_collection(&conn, &paths_in(home.path()), &lib, false, &[]).unwrap();
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!((report.created, report.completed), (1, 0));
        let unit = queries::get_unit_by_name(&conn, "testlib/alpha")
            .unwrap()
            .unwrap();
        let df = dotfile::read_dotfile(&alpha.join(dotfile::UNIT_DOTFILE)).unwrap();
        assert_eq!(df.uuid, unit.uuid);
    }

    /// A row stuck at its exact folder with no dotfile (what a failed write
    /// left before 1.9.0) is completed with its own uuid, not reported as
    /// nested in itself; the dry run says so and writes nothing.
    #[test]
    fn a_row_stuck_without_its_dotfile_is_completed_with_its_own_uuid() {
        let conn = db::open_memory().unwrap();
        let tenant_id = seed_tenant(&conn, "media");
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let alpha = root.path().canonicalize().unwrap().join("alpha");
        std::fs::create_dir_all(&alpha).unwrap();
        let id = queries::insert_unit(
            &conn,
            "u-stuck",
            "testlib/alpha",
            tenant_id,
            None,
            &alpha.to_string_lossy(),
            "mtime_size",
            true,
        )
        .unwrap();
        queries::add_tag_to_unit(&conn, id, "kept").unwrap();
        let lib = test_lib(root.path(), "media");

        let (report, errors) =
            sync_collection(&conn, &paths_in(home.path()), &lib, true, &[]).unwrap();
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!((report.created, report.completed), (0, 1));
        assert!(!alpha.join(dotfile::UNIT_DOTFILE).exists(), "a dry run");

        let (report, errors) =
            sync_collection(&conn, &paths_in(home.path()), &lib, false, &[]).unwrap();
        assert!(errors.is_empty(), "never called nested: {errors:?}");
        assert_eq!((report.created, report.completed), (0, 1));
        let df = dotfile::read_dotfile(&alpha.join(dotfile::UNIT_DOTFILE)).unwrap();
        assert_eq!(
            (df.uuid.as_str(), df.name.as_str(), df.tenant.as_str()),
            ("u-stuck", "testlib/alpha", "media")
        );
        assert_eq!(df.tags, vec!["kept".to_string()]);
        assert_eq!(unit_count(&conn), 1, "the row is the unit; none is added");

        let (report, errors) =
            sync_collection(&conn, &paths_in(home.path()), &lib, false, &[]).unwrap();
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!((report.created, report.completed), (0, 0));
    }

    /// Sync's dry run validates names as the real run does (ADR-0012 item
    /// 220): both refuse the folder with a space in it, with its two ways
    /// out, and agree on every count.
    #[test]
    fn the_dry_run_and_the_real_run_agree_on_an_invalid_name() {
        let conn = db::open_memory().unwrap();
        seed_tenant(&conn, "media");
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        for d in ["alpha", "2022 trip"] {
            std::fs::create_dir_all(root.path().join(d)).unwrap();
        }
        let lib = test_lib(root.path(), "media");
        let counts = |r: &SyncReport| (r.created, r.refused_folders(), r.never_archived);

        let (dry, dry_errors) =
            sync_collection(&conn, &paths_in(home.path()), &lib, true, &[]).unwrap();
        assert_eq!(counts(&dry), (1, 1, 1));
        assert_eq!(dry.problems[0].severity, Severity::Warning);
        assert!(
            dry_errors[0].contains("invalid unit name \"testlib/2022 trip\"")
                && dry_errors[0].contains("rename the folder")
                && dry_errors[0]
                    .contains("tapectl config add 'collections[testlib].exclude' '2022 trip'"),
            "{}",
            dry_errors[0]
        );
        assert_eq!(unit_count(&conn), 0);

        let (real, real_errors) =
            sync_collection(&conn, &paths_in(home.path()), &lib, false, &[]).unwrap();
        assert_eq!(counts(&real), counts(&dry));
        assert_eq!(real_errors, dry_errors);
        assert_eq!(unit_count(&conn), 1);
    }

    /// A dry run reads its own moves and reactivations: a renamed unit is
    /// moved, not also missing, and is pending at its new folder, as the
    /// real run reports it.
    #[test]
    fn a_dry_run_counts_a_move_as_the_real_run_does() {
        let conn = db::open_memory().unwrap();
        seed_tenant(&conn, "media");
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("alpha")).unwrap();
        let lib = test_lib(root.path(), "media");
        sync_collection(&conn, &paths_in(home.path()), &lib, false, &[]).unwrap();
        std::fs::rename(root.path().join("alpha"), root.path().join("alpha2")).unwrap();

        let counts = |r: &SyncReport| (r.moved, r.missing, r.never_archived, r.created);
        let (dry, _) = sync_collection(&conn, &paths_in(home.path()), &lib, true, &[]).unwrap();
        assert_eq!(counts(&dry), (1, 0, 1, 0));
        let (real, _) = sync_collection(&conn, &paths_in(home.path()), &lib, false, &[]).unwrap();
        assert_eq!(counts(&real), counts(&dry));
    }

    /// A collection whose tenant does not exist is that collection's error:
    /// nothing is walked or registered, and the problem names the fix.
    #[test]
    fn an_unknown_tenant_stops_the_collection_before_any_folder() {
        let conn = db::open_memory().unwrap();
        seed_tenant(&conn, "media");
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("alpha")).unwrap();
        let lib = test_lib(root.path(), "medai");
        let (report, errors) =
            sync_collection(&conn, &paths_in(home.path()), &lib, false, &[]).unwrap();
        assert_eq!(errors.len(), 1, "one line, not one a folder: {errors:?}");
        assert!(
            errors[0].contains("tenant \"medai\" does not exist"),
            "{errors:?}"
        );
        assert_eq!(report.problems[0].folder, None);
        assert_eq!(report.severities(), vec![Severity::Error]);
        assert_eq!(unit_count(&conn), 0);
    }
}
