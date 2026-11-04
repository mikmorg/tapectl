pub mod content_match;
pub mod discovery;
pub mod dotfile;
pub mod identity;
pub mod nesting;

use std::path::Path;

use rusqlite::Connection;
use tracing::warn;
use uuid::Uuid;

use crate::config::{Config, TapectlPaths};
use crate::db::{events, queries};
use crate::error::{Result, TapectlError};

/// The checksum mode a unit being REGISTERED starts with (issue #347): the
/// resolved policy's, by the one resolver (`policy::resolve` -- dotfile
/// `[policy]` > archive set > `[defaults]`), rather than a hardcoded
/// `mtime_size`. Resolved before the row exists, against an in-memory stand-in
/// carrying the only three fields the resolver reads (`name`,
/// `archive_set_id`, `current_path`); the stand-in's other fields are never
/// consulted.
///
/// Called at creation ONLY. `units.checksum_mode` is a stored fact about a
/// registered unit, and dirty detection and version minting read it; a later
/// `[defaults]` or archive-set change is deliberately not applied to units
/// that already exist.
///
/// Refuses (a `PolicyUnresolvable` naming the file) when a dotfile already at
/// `dir_path` has an invalid `[policy]` -- a unit whose policy cannot be
/// resolved is not registered.
pub(crate) fn creation_checksum_mode(
    conn: &Connection,
    config: &Config,
    name: &str,
    archive_set_id: Option<i64>,
    dir_path: &str,
) -> Result<String> {
    let stand_in = crate::db::models::Unit {
        id: 0,
        uuid: String::new(),
        name: name.to_string(),
        tenant_id: 0,
        archive_set_id,
        current_path: Some(dir_path.to_string()),
        checksum_mode: String::new(),
        encrypt: true,
        status: "active".to_string(),
        created_at: String::new(),
        last_scanned: None,
        notes: None,
        copies_at_frozen: None,
    };
    Ok(crate::policy::resolve(conn, config, &stand_in)?.checksum_mode)
}

/// Initialize a single unit at the given directory path, resolving its
/// checksum mode against BUILT-IN defaults (`Config::default()`).
///
/// Kept for callers that hold no operator config (tests, and
/// `quick-archive` until it moves over). A command that has the operator's
/// `Config` must call `init_unit_with_config`, or `[defaults] checksum_mode`
/// is silently ignored for the unit it creates (issue #347).
pub fn init_unit(
    conn: &Connection,
    _paths: &TapectlPaths,
    dir_path: &str,
    tenant_name: &str,
    name_override: Option<&str>,
    tags: &[String],
    archive_set: Option<&str>,
) -> Result<i64> {
    init_unit_with_config(
        conn,
        &Config::default(),
        dir_path,
        tenant_name,
        name_override,
        tags,
        archive_set,
    )
}

/// Initialize a single unit at the given directory path. Its
/// `units.checksum_mode` is the resolved policy's under `config`
/// (`creation_checksum_mode`, issue #347).
///
/// The row and the dotfile go together or not at all (ADR-0012 item 220):
/// see [`register_unit`], which this is, less which of its two outcomes it
/// was.
pub fn init_unit_with_config(
    conn: &Connection,
    config: &Config,
    dir_path: &str,
    tenant_name: &str,
    name_override: Option<&str>,
    tags: &[String],
    archive_set: Option<&str>,
) -> Result<i64> {
    Ok(register_unit(
        conn,
        config,
        dir_path,
        tenant_name,
        name_override,
        tags,
        archive_set,
    )?
    .id())
}

/// What [`register_unit`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Registration {
    /// A new unit: its row and its dotfile, written together.
    Created(i64),
    /// A unit registered before 1.9.0 whose dotfile was never written (the
    /// half-registered unit, ADR-0012 item 220): its dotfile is written now,
    /// with the row's own uuid, and its row is unchanged.
    Completed(i64),
}

impl Registration {
    pub fn id(self) -> i64 {
        match self {
            Registration::Created(id) | Registration::Completed(id) => id,
        }
    }
}

/// Register the directory at `dir_path` as a unit: [`plan_new_unit`]'s
/// checks, then [`write_new_unit`], which commits the row only once the
/// dotfile is on disk.
///
/// A directory the catalog already holds at exactly this path with no
/// dotfile is the half-registered unit a dotfile write left before 1.9.0:
/// it is completed ([`complete_registration`]), never reported as nested in
/// itself. Its identity is the row's, so it is completed only for the
/// tenant that owns it, and under its own name when `--name` is given.
pub fn register_unit(
    conn: &Connection,
    config: &Config,
    dir_path: &str,
    tenant_name: &str,
    name_override: Option<&str>,
    tags: &[String],
    archive_set: Option<&str>,
) -> Result<Registration> {
    if let Some(stuck) = half_registered_at(conn, dir_path)? {
        // A collection with `dotfiles = false` registers its units by path,
        // with no dotfile by design: such a unit is whole, not stuck.
        let path = stuck.current_path.as_deref().unwrap_or(dir_path);
        if let Some(lib) = config.collections.iter().find(|lib| {
            !lib.dotfiles
                && crate::collection::canonical_root(lib)
                    .is_ok_and(|root| crate::collection::is_under_root(path, &root))
        }) {
            return Err(TapectlError::Other(format!(
                "{path} is already registered as unit \"{}\" by collection \"{}\", which keeps \
                 no {} in its folders (dotfiles = false)",
                stuck.name,
                lib.name,
                dotfile::UNIT_DOTFILE
            )));
        }
        let owner = tenant_name_of(conn, stuck.tenant_id)?;
        if owner != tenant_name || name_override.is_some_and(|n| n != stuck.name) {
            return Err(TapectlError::Other(format!(
                "{} is already registered as unit \"{}\" (tenant \"{owner}\"), but its {} was \
                 never written; run this again as `--tenant {owner}`{} to write it with that \
                 unit's own identity",
                stuck.current_path.as_deref().unwrap_or(dir_path),
                stuck.name,
                dotfile::UNIT_DOTFILE,
                if name_override.is_some() {
                    format!(" and `--name {}` or no --name", stuck.name)
                } else {
                    String::new()
                },
            )));
        }
        complete_registration(conn, &stuck).map_err(|e| TapectlError::Other(e.describe()))?;
        return Ok(Registration::Completed(stuck.id));
    }
    let new = plan_new_unit(
        conn,
        config,
        dir_path,
        tenant_name,
        name_override,
        tags,
        archive_set,
    )?;
    match write_new_unit(conn, &new) {
        Ok(id) => Ok(Registration::Created(id)),
        Err(RegisterError::Catalog(e)) => Err(e),
        Err(RegisterError::Dotfile(e)) => Err(TapectlError::Other(e.describe())),
    }
}

/// A unit about to be registered: every check registration makes, made,
/// and nothing written. `collection sync --dry-run` stops here, so it
/// refuses what the real run refuses (ADR-0012 item 220).
#[derive(Debug, Clone)]
pub struct NewUnit {
    /// The directory, canonical.
    pub path: String,
    pub name: String,
    pub tenant_id: i64,
    pub tenant_name: String,
    pub archive_set: Option<String>,
    pub archive_set_id: Option<i64>,
    pub checksum_mode: String,
    pub tags: Vec<String>,
}

/// Every check [`register_unit`] makes before it writes, in its order.
pub fn plan_new_unit(
    conn: &Connection,
    config: &Config,
    dir_path: &str,
    tenant_name: &str,
    name_override: Option<&str>,
    tags: &[String],
    archive_set: Option<&str>,
) -> Result<NewUnit> {
    let abs_path = std::fs::canonicalize(dir_path)
        .map_err(|_| TapectlError::UnitPathNotFound(dir_path.to_string()))?;
    let abs_str = abs_path.to_string_lossy().to_string();

    // Check the directory exists
    if !abs_path.is_dir() {
        return Err(TapectlError::UnitPathNotFound(abs_str));
    }

    // Check for nesting conflicts
    nesting::check_nesting(conn, &abs_str)?;

    // Check for existing dotfile
    let dotfile_path = abs_path.join(dotfile::UNIT_DOTFILE);
    if dotfile_path.exists() {
        return Err(TapectlError::UnitAlreadyExists(abs_str));
    }

    // Resolve tenant
    let tenant = crate::tenant::require_tenant(conn, tenant_name)?;

    // Resolve --archive-set to an id, validating it exists (issue #48): a
    // typo'd or nonexistent name must never be silently accepted and then
    // sit permanently inert as a NULL `archive_set_id` — the exact bug this
    // issue diagnosed. `None` (no --archive-set given) always resolves
    // cleanly to `None`, the common case.
    let archive_set_id = queries::resolve_archive_set(conn, archive_set)?;

    // Auto-name from path or use override
    let unit_name = if let Some(name) = name_override {
        name.to_string()
    } else {
        auto_name_from_path(&abs_str)
    };

    // Creation-time name validation (issue #103). Covers the derived name
    // too, not just `--name`: `auto_name_from_path` takes its segments from
    // a real directory path, which can legitimately contain characters that
    // have no business in an identifier this codebase interpolates into
    // staging paths.
    crate::naming::validate_unit_name(&unit_name)?;

    // Check name uniqueness
    if queries::get_unit_by_name(conn, &unit_name)?.is_some() {
        return Err(TapectlError::UnitAlreadyExists(unit_name));
    }

    // Issue #347. The dotfile is written below, with no `[policy]`, so the
    // resolver's dotfile layer is absent now and stays absent: archive set
    // > [defaults].
    let checksum_mode = creation_checksum_mode(conn, config, &unit_name, archive_set_id, &abs_str)?;

    Ok(NewUnit {
        path: abs_str,
        name: unit_name,
        tenant_id: tenant.id,
        tenant_name: tenant_name.to_string(),
        archive_set: archive_set.map(str::to_string),
        archive_set_id,
        checksum_mode,
        tags: tags.to_vec(),
    })
}

/// Why [`write_new_unit`] wrote nothing.
#[derive(Debug)]
pub enum RegisterError {
    /// The catalog refused the row (a name taken since the plan, a busy
    /// catalog). No dotfile was written.
    Catalog(TapectlError),
    /// The dotfile could not be written, so the row was rolled back.
    Dotfile(DotfileUnwritable),
}

/// A unit's `.tapectl-unit.toml` could not be written into its folder.
#[derive(Debug)]
pub struct DotfileUnwritable {
    /// The folder, canonical.
    pub dir: String,
    pub cause: TapectlError,
}

impl DotfileUnwritable {
    /// Permission was the reason: an ACL grant fixes it.
    pub fn permission_denied(&self) -> bool {
        matches!(&self.cause, TapectlError::Io(e)
            if e.kind() == std::io::ErrorKind::PermissionDenied)
    }

    /// The operator's line: what failed, as whom, and (for a permission
    /// failure) the grant that fixes it.
    pub fn describe(&self) -> String {
        let user = effective_user_name();
        let mut line = format!(
            "cannot write {} in {} as user {user}: {}",
            dotfile::UNIT_DOTFILE,
            self.dir,
            self.cause
        );
        if self.permission_denied() {
            line.push_str(&format!(
                "; grant it: sudo setfacl -m u:{user}:rwX {}",
                shell_word(&self.dir)
            ));
        }
        line
    }
}

/// Write `new`'s row and its dotfile as one change (ADR-0012 item 220): the
/// row, its creation event, its path history and its tags are written in a
/// transaction that commits only once the dotfile is on disk, so a dotfile
/// that cannot be written leaves no row. Before 1.9.0 the row was committed
/// first, and a folder the user could not write was left registered with no
/// dotfile, which every later sync called "nested" in itself.
///
/// The dotfile is written before the commit: a commit that then fails
/// leaves a dotfile with no row, which the next `collection sync` or `unit
/// discover` adopts under the same uuid, so the unit's identity holds
/// either way. This removes a dotfile it wrote when the commit fails all
/// the same, so `unit init` can simply be run again.
pub fn write_new_unit(conn: &Connection, new: &NewUnit) -> std::result::Result<i64, RegisterError> {
    let uuid = Uuid::new_v4().to_string();
    let dotfile_path = Path::new(&new.path).join(dotfile::UNIT_DOTFILE);
    let tx = Atomic::begin(conn).map_err(RegisterError::Catalog)?;
    let unit_id = (|| -> Result<i64> {
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
        // Record initial path history
        queries::update_unit_path(conn, unit_id, &new.path)?;
        for tag in &new.tags {
            queries::add_tag_to_unit(conn, unit_id, tag)?;
        }
        Ok(unit_id)
    })()
    .map_err(RegisterError::Catalog)?;

    let dotfile_data = dotfile::UnitDotfile {
        uuid,
        name: new.name.clone(),
        created: chrono::Utc::now().to_rfc3339(),
        tags: new.tags.clone(),
        tenant: new.tenant_name.clone(),
        archive_set: new.archive_set.clone(),
        checksum_mode: None,
        compression: None,
        slice_size: None,
        warehouse_copies: None,
        allow_spanning: None,
        copies_at: None,
        exclude_patterns: Vec::new(),
    };
    if let Err(cause) = dotfile::write_dotfile(&dotfile_path, &dotfile_data) {
        // A write that failed part-way (a full disk) may have left a
        // fragment; it was not there before (`plan_new_unit` checked).
        let _ = std::fs::remove_file(&dotfile_path);
        return Err(RegisterError::Dotfile(DotfileUnwritable {
            dir: new.path.clone(),
            cause,
        }));
    }
    if let Err(e) = tx.commit() {
        let _ = std::fs::remove_file(&dotfile_path);
        return Err(RegisterError::Catalog(e));
    }
    Ok(unit_id)
}

/// The unit the catalog holds at exactly `dir_path` when that folder has no
/// dotfile: the half-registered unit (ADR-0012 item 220). `None` for a
/// folder with a dotfile, a folder the catalog does not hold, or a path
/// that does not resolve.
pub fn half_registered_at(
    conn: &Connection,
    dir_path: &str,
) -> Result<Option<crate::db::models::Unit>> {
    let Ok(abs) = std::fs::canonicalize(dir_path) else {
        return Ok(None);
    };
    if !abs.is_dir() || abs.join(dotfile::UNIT_DOTFILE).exists() {
        return Ok(None);
    }
    queries::get_unit_by_path(conn, &abs.to_string_lossy())
}

/// Write the dotfile of a unit whose row exists and whose dotfile was never
/// written (ADR-0012 item 220): the row's own uuid, name, tenant, archive
/// set and tags, so the folder takes the identity the catalog already
/// gave it. The row itself is unchanged; the completion is recorded as an
/// event. Refuses (writing nothing) when a dotfile is already there.
pub fn complete_registration(
    conn: &Connection,
    unit: &crate::db::models::Unit,
) -> std::result::Result<(), DotfileUnwritable> {
    let dir = unit.current_path.clone().unwrap_or_default();
    let fail = |cause: TapectlError| DotfileUnwritable {
        dir: dir.clone(),
        cause,
    };
    let dotfile_path = Path::new(&dir).join(dotfile::UNIT_DOTFILE);
    if dotfile_path.exists() {
        return Err(fail(TapectlError::UnitAlreadyExists(
            dotfile_path.display().to_string(),
        )));
    }
    let data = completed_dotfile(conn, unit).map_err(&fail)?;
    if let Err(cause) = dotfile::write_dotfile(&dotfile_path, &data) {
        let _ = std::fs::remove_file(&dotfile_path);
        return Err(fail(cause));
    }
    events::log_field_change(
        conn,
        "unit",
        unit.id,
        &unit.name,
        "dotfile_completed",
        "dotfile",
        None,
        &dotfile_path.display().to_string(),
        Some(unit.tenant_id),
    )
    .map_err(&fail)?;
    Ok(())
}

/// The dotfile [`complete_registration`] writes for `unit`.
fn completed_dotfile(
    conn: &Connection,
    unit: &crate::db::models::Unit,
) -> Result<dotfile::UnitDotfile> {
    let archive_set: Option<String> = match unit.archive_set_id {
        Some(id) => Some(conn.query_row(
            "SELECT name FROM archive_sets WHERE id = ?1",
            rusqlite::params![id],
            |r| r.get(0),
        )?),
        None => None,
    };
    // `units.created_at` is `datetime('now')`, UTC without a zone; the
    // dotfile's `created` is RFC 3339, as `write_new_unit` writes it.
    let created = chrono::NaiveDateTime::parse_from_str(&unit.created_at, "%Y-%m-%d %H:%M:%S")
        .map(|t| t.and_utc().to_rfc3339())
        .unwrap_or_else(|_| unit.created_at.clone());
    Ok(dotfile::UnitDotfile {
        uuid: unit.uuid.clone(),
        name: unit.name.clone(),
        created,
        tags: queries::get_tags_for_unit(conn, unit.id)?,
        tenant: tenant_name_of(conn, unit.tenant_id)?,
        archive_set,
        checksum_mode: None,
        compression: None,
        slice_size: None,
        warehouse_copies: None,
        allow_spanning: None,
        copies_at: None,
        exclude_patterns: Vec::new(),
    })
}

fn tenant_name_of(conn: &Connection, tenant_id: i64) -> Result<String> {
    Ok(conn.query_row(
        "SELECT name FROM tenants WHERE id = ?1",
        rusqlite::params![tenant_id],
        |r| r.get(0),
    )?)
}

/// The name of the user this process runs as (its effective uid), for the
/// `setfacl` lines a refusal prints; the uid itself when it has no name.
pub fn effective_user_name() -> String {
    let uid = nix::unistd::geteuid();
    match nix::unistd::User::from_uid(uid) {
        Ok(Some(user)) => user.name,
        _ => uid.as_raw().to_string(),
    }
}

/// `s` as one shell word: bare when it holds only characters a shell
/// leaves alone, single-quoted otherwise.
pub fn shell_word(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+:@,=%".contains(c));
    if plain {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// Run `f`'s writes on `conn` as one change: all of them or none
/// (see [`Atomic`]).
pub(crate) fn atomically<T>(conn: &Connection, f: impl FnOnce() -> Result<T>) -> Result<T> {
    let tx = Atomic::begin(conn)?;
    let value = f()?;
    tx.commit()?;
    Ok(value)
}

/// One atomic change on `conn`: an IMMEDIATE transaction (the busy policy's
/// rule 2, `db::busy`) when none is open, a savepoint inside the caller's
/// when one is. Rolled back on drop unless committed.
struct Atomic<'c> {
    conn: &'c Connection,
    nested: bool,
    done: bool,
}

const SAVEPOINT: &str = "tapectl_unit_registration";

impl<'c> Atomic<'c> {
    fn begin(conn: &'c Connection) -> Result<Self> {
        let nested = !conn.is_autocommit();
        if nested {
            conn.execute_batch(&format!("SAVEPOINT {SAVEPOINT}"))?;
        } else {
            conn.execute_batch("BEGIN IMMEDIATE")?;
        }
        Ok(Self {
            conn,
            nested,
            done: false,
        })
    }

    fn commit(mut self) -> Result<()> {
        self.done = true;
        if self.nested {
            self.conn.execute_batch(&format!("RELEASE {SAVEPOINT}"))?;
        } else if let Err(e) = self.conn.execute_batch("COMMIT") {
            let _ = self.conn.execute_batch("ROLLBACK");
            return Err(e.into());
        }
        Ok(())
    }
}

impl Drop for Atomic<'_> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let sql = if self.nested {
            format!("ROLLBACK TO {SAVEPOINT}; RELEASE {SAVEPOINT}")
        } else {
            "ROLLBACK".to_string()
        };
        let _ = self.conn.execute_batch(&sql);
    }
}

/// Bulk-initialize units from a list of directories under a parent, each
/// as `init_unit_with_config` would under `config`.
pub fn init_bulk(
    conn: &Connection,
    config: &Config,
    parent_dir: &str,
    tenant_name: &str,
    tags: &[String],
) -> Result<Vec<(String, std::result::Result<i64, TapectlError>)>> {
    let parent = std::fs::canonicalize(parent_dir)
        .map_err(|_| TapectlError::UnitPathNotFound(parent_dir.to_string()))?;

    let mut results = Vec::new();

    // Walk immediate subdirectories (depth 1 by default)
    let entries = std::fs::read_dir(&parent)?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        // Skip hidden directories
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }

        let path_str = path.to_string_lossy().to_string();
        let result = init_unit_with_config(conn, config, &path_str, tenant_name, None, tags, None);
        results.push((path_str, result));
    }

    Ok(results)
}

/// Rename a unit.
pub fn rename_unit(conn: &Connection, current_name: &str, new_name: &str) -> Result<()> {
    let unit = queries::get_unit_by_name(conn, current_name)?
        .ok_or_else(|| TapectlError::UnitNotFound(current_name.to_string()))?;

    // A rename CHOOSES a name, so it validates (issue #103). Note only the
    // NEW name is checked — `current_name` is looked up, never validated, so
    // a unit registered before this rule existed can still be renamed *out*
    // of a name this rule would now reject. That asymmetry is the point.
    crate::naming::validate_unit_name(new_name)?;

    // Check new name doesn't conflict
    if queries::get_unit_by_name(conn, new_name)?.is_some() {
        return Err(TapectlError::UnitAlreadyExists(new_name.to_string()));
    }

    queries::update_unit_name(conn, unit.id, new_name)?;

    events::log_field_change(
        conn,
        "unit",
        unit.id,
        new_name,
        "renamed",
        "name",
        Some(current_name),
        new_name,
        Some(unit.tenant_id),
    )?;

    rewrite_dotfile(
        unit.current_path.as_deref(),
        &DotfileChange {
            done: "renamed",
            stale_when_unwritten: "the old name",
            stale_when_unreadable: "the name on disk is now stale",
        },
        |df| df.name = new_name.to_string(),
    );

    Ok(())
}

/// Add and remove tags on `unit`, then mirror the resulting set into its
/// `.tapectl-unit.toml` (issue #359). Returns the tag set the database now
/// holds.
///
/// The dotfile's `tags` are what `unit discover` and `collection sync`
/// register when they adopt a unit the database does not know yet, so a
/// dotfile left behind by `unit tag` would quietly bring the old tags back
/// on a rebuilt catalog. The whole resulting set is written (not just the
/// delta), so a dotfile that had drifted from the database is corrected too.
/// The dotfile rewrite follows `rename_unit`'s rule: warned, never fatal.
pub fn tag_unit(
    conn: &Connection,
    unit: &crate::db::models::Unit,
    add: &[String],
    remove: &[String],
) -> Result<Vec<String>> {
    for tag in add {
        queries::add_tag_to_unit(conn, unit.id, tag)?;
    }
    for tag in remove {
        queries::remove_tag_from_unit(conn, unit.id, tag)?;
    }
    let tags = queries::get_tags_for_unit(conn, unit.id)?;

    rewrite_dotfile(
        unit.current_path.as_deref(),
        &DotfileChange {
            done: "re-tagged",
            stale_when_unwritten: "the old tags",
            stale_when_unreadable: "the tags on disk are now stale",
        },
        |df| df.tags = tags.clone(),
    );
    Ok(tags)
}

/// Mirror a `tenant reassign` into the unit's `.tapectl-unit.toml` (#383),
/// so the dotfile names the tenant the catalog now says owns the unit.
/// Since #378 the dotfile is not content, so this mints no version. Follows
/// `rename_unit`'s rule: warned, never fatal.
pub fn retenant_dotfile(current_path: Option<&str>, tenant: &str) {
    rewrite_dotfile(
        current_path,
        &DotfileChange {
            done: "reassigned",
            stale_when_unwritten: "the old tenant",
            stale_when_unreadable: "the tenant on disk is now stale",
        },
        |df| df.tenant = tenant.to_string(),
    );
}

/// How to word the warning when a database change could not be mirrored
/// into the unit's dotfile. See `rewrite_dotfile`.
struct DotfileChange {
    /// Past participle for "unit ___ in the database" ("renamed").
    done: &'static str,
    /// What the unwritten dotfile still carries ("the old name").
    stale_when_unwritten: &'static str,
    /// The consequence of an unreadable dotfile ("the name on disk is now
    /// stale").
    stale_when_unreadable: &'static str,
}

/// Read -> mutate -> write the dotfile under `current_path`, after a change
/// the database has ALREADY committed. Absent path or absent dotfile: nothing
/// to do.
///
/// Failures are WARNED, not swallowed and not returned (issue #110). The
/// database is authoritative, and failing the command after the change has
/// been recorded would be worse than a stale dotfile -- but silence was
/// wrong; the operator has no other signal. The two failure modes are named
/// separately because they need different fixes (an unreadable or corrupt
/// file vs an unwritable one). The round trip goes through
/// `dotfile::read_dotfile`/`write_dotfile`, which model every `[policy]` key,
/// so nothing the mutation does not touch is lost (issue #212).
fn rewrite_dotfile(
    current_path: Option<&str>,
    change: &DotfileChange,
    mutate: impl FnOnce(&mut dotfile::UnitDotfile),
) {
    let Some(path) = current_path else {
        return;
    };
    let dotfile_path = Path::new(path).join(".tapectl-unit.toml");
    if !dotfile_path.exists() {
        return;
    }
    match round_trip_dotfile(&dotfile_path, |df| {
        mutate(df);
        true
    }) {
        Ok(_) => {}
        Err(DotfileRoundTrip::Write(e)) => warn!(
            path = %dotfile_path.display(),
            error = %e,
            "unit {} in the database, but its .tapectl-unit.toml could not be \
             rewritten — the dotfile still carries {} and must be corrected by hand",
            change.done,
            change.stale_when_unwritten,
        ),
        Err(DotfileRoundTrip::Read(e)) => warn!(
            path = %dotfile_path.display(),
            error = %e,
            "unit {} in the database, but its .tapectl-unit.toml could not be read, so {}",
            change.done,
            change.stale_when_unreadable,
        ),
    }
}

/// Why a dotfile round trip ([`round_trip_dotfile`]) stopped.
enum DotfileRoundTrip {
    /// The file could not be read or parsed; nothing was written.
    Read(TapectlError),
    /// It was read and changed, but could not be written back.
    Write(TapectlError),
}

/// The one read -> mutate -> write of a unit's dotfile, through
/// `dotfile::read_dotfile`/`write_dotfile` (which model every `[policy]` key,
/// so nothing the mutation does not touch is lost, issue #212). `mutate`
/// says whether it changed anything; when it did not, nothing is written.
/// Returns whether the file was written.
fn round_trip_dotfile(
    dotfile_path: &Path,
    mutate: impl FnOnce(&mut dotfile::UnitDotfile) -> bool,
) -> std::result::Result<bool, DotfileRoundTrip> {
    let mut df = dotfile::read_dotfile(dotfile_path).map_err(DotfileRoundTrip::Read)?;
    if !mutate(&mut df) {
        return Ok(false);
    }
    dotfile::write_dotfile(dotfile_path, &df).map_err(DotfileRoundTrip::Write)?;
    Ok(true)
}

/// What [`rename_location_in_dotfile`] did with one unit's dotfile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DotfileLocationRename {
    /// No dotfile, or its `[policy.copies_at]` does not name the location.
    Unchanged,
    /// The key was renamed and the file written.
    Rewritten,
    /// The file names the location, or may (it could not be parsed), and
    /// was not rewritten; the reason, for the operator.
    Failed(String),
}

/// Rename location `old` to `new` in the `[policy.copies_at]` of the
/// dotfile under `current_path` (#493, ADR-0012 item 155: `location rename`
/// rewrites every present dotfile, as it rewrites archive sets), through
/// the same round trip `rename_unit` uses. The count moves with the name; a
/// stale entry already under `new` (which no registered location could
/// have been, or the rename would have been refused) is replaced by it.
///
/// Unlike the unit-scoped rewrites above, a failure is returned, not
/// warned: `location rename` names every file it could not rewrite. A file
/// whose text does not contain `old` at all is not parsed, so an unrelated
/// malformed dotfile is not reported.
pub fn rename_location_in_dotfile(
    current_path: &str,
    old: &str,
    new: &str,
) -> DotfileLocationRename {
    let dotfile_path = Path::new(current_path).join(dotfile::UNIT_DOTFILE);
    match std::fs::read_to_string(&dotfile_path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return DotfileLocationRename::Unchanged;
        }
        Err(e) => {
            return DotfileLocationRename::Failed(format!("could not be read ({e})"));
        }
        Ok(text) if !text.contains(old) => return DotfileLocationRename::Unchanged,
        Ok(_) => {}
    }
    let renamed = round_trip_dotfile(&dotfile_path, |df| {
        let Some(map) = df.copies_at.as_mut() else {
            return false;
        };
        let Some(count) = map.remove(old) else {
            return false;
        };
        map.insert(new.to_string(), count);
        true
    });
    match renamed {
        Ok(true) => DotfileLocationRename::Rewritten,
        Ok(false) => DotfileLocationRename::Unchanged,
        Err(DotfileRoundTrip::Read(e)) => DotfileLocationRename::Failed(format!(
            "could not be parsed, so whether it names the location is unknown ({e})"
        )),
        Err(DotfileRoundTrip::Write(e)) => {
            DotfileLocationRename::Failed(format!("could not be rewritten ({e})"))
        }
    }
}

/// Generate an auto-name from a filesystem path.
/// Strips common prefixes like /media/, /mnt/ and joins remaining components with "/".
fn auto_name_from_path(path: &str) -> String {
    let path = Path::new(path);
    let components: Vec<&str> = path
        .components()
        .filter_map(|c| {
            if let std::path::Component::Normal(s) = c {
                Some(s.to_str().unwrap_or(""))
            } else {
                None
            }
        })
        .collect();

    // Strip common mount prefixes
    let skip = if components.first() == Some(&"media") || components.first() == Some(&"mnt") {
        1
    } else {
        0
    };

    components[skip..].join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auto_name_media() {
        assert_eq!(auto_name_from_path("/media/tv/bb/s01"), "tv/bb/s01");
    }

    #[test]
    fn test_auto_name_mnt() {
        assert_eq!(auto_name_from_path("/mnt/archive/photos"), "archive/photos");
    }

    #[test]
    fn test_auto_name_other() {
        assert_eq!(auto_name_from_path("/home/user/data"), "home/user/data");
    }

    // ── archive_set_id wiring (issue #48) ──

    fn harness() -> (Connection, tempfile::TempDir, TapectlPaths) {
        let conn = crate::db::open_memory().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let paths = TapectlPaths::new(home);
        paths.ensure_dirs().unwrap();
        crate::tenant::add_tenant(&conn, &paths, "alice", None, false).unwrap();
        (conn, tmp, paths)
    }

    /// The end-to-end proof issue #48 asks for: a unit created with
    /// `--archive-set X` must have `archive_set_id` actually persisted (not
    /// just written to the dotfile, which is all that happened before this
    /// fix), and `policy::resolve` must then return X's values instead of
    /// silently falling back to the global defaults it would use forever
    /// if `archive_set_id` stayed NULL.
    ///
    /// Asserts on `min_copies`/`required_locations` (never written into the
    /// dotfile, so they always resolve through archive_set) as well as
    /// `compression`/`checksum_mode`: as of issue #92's fix, `init_unit`'s
    /// dotfile now omits the `[policy]` table entirely when the operator
    /// hasn't set those fields explicitly, so `policy::resolve`'s dotfile
    /// layer has nothing to shadow with and archive_set's values reach
    /// through for a real, dotfile-backed unit too (Recast of v4.0 §2.2,
    /// docs/design-errata.md).
    #[test]
    fn init_unit_with_archive_set_persists_id_and_unlocks_policy_layer_2() {
        let (conn, tmp, paths) = harness();
        conn.execute(
            "INSERT INTO archive_sets (name, min_copies, required_locations, compression) \
             VALUES ('cold', 5, '[\"vault\"]', 'gzip')",
            [],
        )
        .unwrap();

        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();

        let unit_id = init_unit(
            &conn,
            &paths,
            src.to_str().unwrap(),
            "alice",
            Some("unit1"),
            &[],
            Some("cold"),
        )
        .unwrap();

        let unit = queries::get_unit_by_name(&conn, "unit1").unwrap().unwrap();
        assert_eq!(unit.id, unit_id);
        assert!(
            unit.archive_set_id.is_some(),
            "archive_set_id must be persisted to the DB, not left NULL"
        );

        let config = crate::config::Config::default();
        let resolved = crate::policy::resolve(&conn, &config, &unit).unwrap();
        assert_eq!(
            resolved.min_copies, 5,
            "must resolve the archive_set's min_copies, not the global default (2)"
        );
        assert_eq!(
            resolved.required_locations,
            vec!["vault".to_string()],
            "must resolve the archive_set's required_locations, not the global default (empty)"
        );
        assert_eq!(
            resolved.compression, "gzip",
            "must resolve the archive_set's compression, not be shadowed by a \
             concrete value baked into the unit's dotfile (issue #92)"
        );
    }

    /// Issue #48 item 2: a typo'd or nonexistent `--archive-set` name must
    /// never be silently accepted and left permanently inert — it must
    /// fail loudly, naming the unknown set, and the unit must not be
    /// created at all (no partial/incorrect state).
    #[test]
    fn init_unit_rejects_unknown_archive_set() {
        let (conn, tmp, paths) = harness();

        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();

        let err = init_unit(
            &conn,
            &paths,
            src.to_str().unwrap(),
            "alice",
            Some("unit1"),
            &[],
            Some("nonexistent"),
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(
            msg.contains("nonexistent"),
            "error must name the unknown archive set, got: {msg}"
        );
        assert!(
            queries::get_unit_by_name(&conn, "unit1").unwrap().is_none(),
            "no unit row should be created when --archive-set is invalid"
        );
    }

    /// The common case, and issue #48's own explicit "do NOT break this"
    /// requirement: no `--archive-set` at all must keep resolving cleanly
    /// to system defaults, completely unaffected by this fix.
    #[test]
    fn init_unit_without_archive_set_still_resolves_defaults() {
        let (conn, tmp, paths) = harness();

        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();

        let unit_id = init_unit(
            &conn,
            &paths,
            src.to_str().unwrap(),
            "alice",
            Some("unit1"),
            &[],
            None,
        )
        .unwrap();

        let unit = queries::get_unit_by_name(&conn, "unit1").unwrap().unwrap();
        assert_eq!(unit.id, unit_id);
        assert!(unit.archive_set_id.is_none());

        let config = crate::config::Config::default();
        let resolved = crate::policy::resolve(&conn, &config, &unit).unwrap();
        assert_eq!(resolved.min_copies, 2);
        assert_eq!(resolved.compression, "none");
    }

    // --- issue #110 item 5: rename must not swallow dotfile failures ---

    /// A rename whose dotfile cannot be rewritten still SUCCEEDS — the
    /// database is authoritative for the name, and failing the rename after
    /// it has already been committed would be worse than a stale dotfile.
    /// What changed is that it no longer does so silently (`let _ = ...`);
    /// it warns naming the path. The warning itself is a `tracing` event, so
    /// what is pinned here is the contract around it: the DB rename lands,
    /// and the operator is not left with a *failed* command.
    #[test]
    fn rename_with_an_unreadable_dotfile_still_renames_in_the_database() {
        let (conn, tmp, paths) = harness();
        let unit_dir = tmp.path().join("photos");
        std::fs::create_dir_all(&unit_dir).unwrap();
        init_unit(
            &conn,
            &paths,
            unit_dir.to_str().unwrap(),
            "alice",
            Some("photos"),
            &[],
            None,
        )
        .unwrap();

        // Corrupt the dotfile so `read_dotfile` fails.
        std::fs::write(unit_dir.join(".tapectl-unit.toml"), "not [ valid toml = =").unwrap();

        rename_unit(&conn, "photos", "pictures")
            .expect("a bad dotfile must not fail a rename already committed to the DB");

        let name: String = conn
            .query_row("SELECT name FROM units WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(name, "pictures", "the database rename must have landed");
    }

    /// Issue #212: `rename_unit` is a read -> mutate `name` -> write round
    /// trip over the dotfile (`dotfile::read_dotfile` / `write_dotfile`),
    /// and must preserve every `[policy]` key it never intended to touch --
    /// not just `slice_size`, all of them. Written against the dotfile's
    /// raw TOML text (never the `UnitDotfile` struct directly) so it proves
    /// the on-disk behavior regardless of which fields that struct happens
    /// to model.
    #[test]
    fn rename_preserves_every_policy_key_including_slice_size() {
        let (conn, tmp, paths) = harness();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();

        init_unit(
            &conn,
            &paths,
            src.to_str().unwrap(),
            "alice",
            Some("original"),
            &[],
            None,
        )
        .unwrap();

        let dotfile_path = src.join(".tapectl-unit.toml");
        // Hand-edit the dotfile the way an operator would, setting every
        // documented `[policy]` key at once.
        let mut contents = std::fs::read_to_string(&dotfile_path).unwrap();
        contents.push_str(
            "\n[policy]\nchecksum_mode = \"sha256\"\ncompression = \"gzip\"\n\
             slice_size = \"500M\"\nwarehouse_copies = 2\nallow_spanning = true\n\n\
             [policy.copies_at]\nhome = 1\noffsite = 2\n",
        );
        std::fs::write(&dotfile_path, &contents).unwrap();

        rename_unit(&conn, "original", "renamed").unwrap();

        let after = std::fs::read_to_string(&dotfile_path).unwrap();
        assert!(
            after.contains("checksum_mode"),
            "checksum_mode must survive rename, got: {after}"
        );
        assert!(
            after.contains("compression"),
            "compression must survive rename, got: {after}"
        );
        assert!(
            after.contains("slice_size"),
            "slice_size must survive rename (issue #212) -- it must not be silently \
             deleted, converting the operator's deliberate choice into silence; got: {after}"
        );
        assert!(
            after.contains("500M"),
            "slice_size's VALUE, not just some key, must survive; got: {after}"
        );
        assert!(
            after.contains("warehouse_copies"),
            "warehouse_copies must survive rename, got: {after}"
        );
        // #478 (ADR-0012 item 82).
        assert!(
            after.contains("allow_spanning = true"),
            "allow_spanning must survive rename, got: {after}"
        );
        // #493 (ADR-0012 item 155).
        assert!(
            after.contains("[policy.copies_at]\nhome = 1\noffsite = 2\n"),
            "copies_at must survive rename, got: {after}"
        );
    }
}
