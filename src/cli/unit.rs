use clap::Subcommand;
use rusqlite::Connection;
use serde::Serialize;
use tabled::{Table, Tabled};

use crate::config::{Config, TapectlPaths};
use crate::db::queries;
use crate::error::{Result, TapectlError};

#[derive(Subcommand, Debug)]
pub enum UnitCommands {
    /// Initialize a directory as an archival unit
    Init {
        /// Path to directory
        path: String,
        /// Tenant name
        #[arg(long)]
        tenant: String,
        /// Override auto-generated name
        #[arg(long)]
        name: Option<String>,
        /// Tags to apply
        #[arg(long, short)]
        tag: Vec<String>,
        /// Archive set name
        #[arg(long)]
        archive_set: Option<String>,
    },

    /// Bulk-initialize subdirectories as units
    InitBulk {
        /// Parent directory to scan
        path: String,
        /// Tenant name
        #[arg(long)]
        tenant: String,
        /// Tags to apply to all
        #[arg(long, short)]
        tag: Vec<String>,
    },

    /// List units
    List {
        /// Filter by tenant name
        #[arg(long)]
        tenant: Option<String>,
        /// Filter by status (active, tape_only, missing)
        #[arg(long)]
        status: Option<String>,
        /// Filter by tag
        #[arg(long, short)]
        tag: Option<String>,
    },

    /// Show unit status/details
    Status {
        /// Unit name or path
        name: String,
        /// Show dirty/clean/new status via an on-disk fingerprint scan
        /// (checksum_mode-aware — see `unit init`'s checksum_mode) instead
        /// of the usual detail view
        #[arg(long)]
        dirty: bool,
    },

    /// Add/remove tags
    Tag {
        /// Unit name
        name: String,
        /// Tags to add
        #[arg(long)]
        add: Vec<String>,
        /// Tags to remove
        #[arg(long)]
        remove: Vec<String>,
    },

    /// Rename a unit
    Rename {
        /// Current name
        current: String,
        /// New name
        new: String,
    },

    /// Scan watch_roots for .tapectl-unit.toml dotfiles
    Discover,

    /// Check file integrity against the newest staged version's checksums
    CheckIntegrity {
        /// Unit name
        name: String,
    },

    /// Mark unit as tape-only (local data can be deleted)
    MarkTapeOnly {
        /// Unit name
        name: String,
        /// Confirm in advance, as the global --yes does: mark the unit even
        /// though it is short of its policy's min_copies, the [defaults]
        /// min_locations floor or its required_locations, or is dirty
        /// (changed since its last snapshot). Without either flag a
        /// terminal asks, and a non-interactive run refuses, naming each
        /// shortfall. A unit that was never archived is refused whatever
        /// the flags (ADR-0008 Tier 3)
        #[arg(long)]
        force: bool,
    },
}

/// Table-only: `unit list --json` serializes `db::models::Unit` directly
/// (already `Serialize`), so there is no hand-rolled JSON derived from
/// `UnitRow` to keep in sync -- and the two aren't even shaped alike:
/// `UnitRow.tenant` is a resolved tenant NAME and `UnitRow.tags` is a
/// joined-in, comma-separated string, neither of which `Unit` carries
/// (it has `tenant_id` and no tags at all). The `Serialize` derive and its
/// pin below exist for structural parity with the other ten row structs;
/// nothing in `run()` calls it.
#[derive(Tabled, Serialize)]
struct UnitRow {
    #[tabled(rename = "Name")]
    name: String,
    #[tabled(rename = "Status")]
    status: String,
    #[tabled(rename = "Tenant")]
    tenant: String,
    #[tabled(rename = "Path")]
    path: String,
    #[tabled(rename = "Tags")]
    tags: String,
}

/// `units.status`'s CHECK constraint (`src/db/migrations/026_drop_unwritten_states.sql`,
/// which dropped `retired`: nothing ever wrote it, issue #362). Pinned to the
/// live schema by `unit_statuses_equal_the_live_check`.
const UNIT_STATUSES: &[&str] = &["active", "tape_only", "missing"];

/// `unit list --status` is a usage error when it names anything other than
/// one of `UNIT_STATUSES` (issue #171, ADR-0012) — an unrecognised value
/// used to answer with an empty (or unfiltered) list rather than refusing.
fn validate_unit_status(value: &str) -> Result<()> {
    crate::config::validate_closed_set("--status", value, UNIT_STATUSES)
        .map_err(TapectlError::Other)
}

pub fn run(
    conn: &Connection,
    _paths: &TapectlPaths,
    config: &Config,
    command: &UnitCommands,
    json_output: bool,
    dry_run: bool,
    assume_yes: bool,
) -> Result<()> {
    match command {
        UnitCommands::Init {
            path,
            tenant,
            name,
            tag,
            archive_set,
        } => {
            // Issue #241: `init_unit` writes `.tapectl-unit.toml` into the
            // operator's OWN source tree and resolves auto-generated-name
            // collisions as it goes; reproducing that safely without
            // writing would duplicate its logic in two places that could
            // drift.
            if dry_run {
                return Err(crate::cli::refuse_dry_run(
                    "unit init",
                    "it writes `.tapectl-unit.toml` into the source directory and resolves \
                     name collisions as it goes; a faithful preview would duplicate that logic.",
                ));
            }
            let unit_id = crate::unit::init_unit_with_config(
                conn,
                config,
                path,
                tenant,
                name.as_deref(),
                tag,
                archive_set.as_deref(),
            )?;
            if json_output {
                let unit =
                    queries::get_unit_by_name(conn, &resolve_unit_name(conn, unit_id)?)?.unwrap();
                println!("{}", serde_json::to_string_pretty(&unit).unwrap());
            } else {
                let unit_name = resolve_unit_name(conn, unit_id)?;
                println!("unit \"{unit_name}\" initialized (id={unit_id})");
            }
        }

        UnitCommands::InitBulk { path, tenant, tag } => {
            // Issue #241: same reasoning as `unit init`, multiplied over
            // every subdirectory.
            if dry_run {
                return Err(crate::cli::refuse_dry_run(
                    "unit init-bulk",
                    "it writes `.tapectl-unit.toml` into every subdirectory it registers; a \
                     faithful preview would duplicate `unit init`'s own logic.",
                ));
            }
            let results = crate::unit::init_bulk(conn, config, path, tenant, tag)?;
            let mut success = 0;
            let mut failed = 0;
            for (dir, result) in &results {
                match result {
                    Ok(id) => {
                        if !json_output {
                            println!("  ok: {dir} (id={id})");
                        }
                        success += 1;
                    }
                    Err(e) => {
                        if !json_output {
                            println!("  skip: {dir}: {e}");
                        }
                        failed += 1;
                    }
                }
            }
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"success": success, "failed": failed})
                );
            } else {
                println!("{success} units created, {failed} skipped");
            }
        }

        UnitCommands::List {
            tenant,
            status,
            tag,
        } => {
            if let Some(s) = status {
                validate_unit_status(s)?;
            }
            let tenant_id = if let Some(name) = tenant {
                Some(crate::tenant::require_tenant(conn, name)?.id)
            } else {
                None
            };
            let units = queries::list_units(conn, tenant_id, status.as_deref())?;

            // If filtering by tag, do it in memory (simpler than a join query for now)
            let units = if let Some(tag_filter) = tag {
                units
                    .into_iter()
                    .filter(|u| {
                        queries::get_tags_for_unit(conn, u.id)
                            .unwrap_or_default()
                            .contains(tag_filter)
                    })
                    .collect()
            } else {
                units
            };

            if json_output {
                println!("{}", serde_json::to_string_pretty(&units).unwrap());
            } else if units.is_empty() {
                println!("no units found");
            } else {
                let mut rows = Vec::new();
                for u in &units {
                    let tenant_name = queries::get_tenant_by_id(conn, u.tenant_id)?
                        .map(|t| t.name)
                        .unwrap_or_else(|| "?".to_string());
                    let tags = queries::get_tags_for_unit(conn, u.id)?.join(", ");
                    rows.push(UnitRow {
                        name: u.name.clone(),
                        status: u.status.clone(),
                        tenant: tenant_name,
                        path: u.current_path.clone().unwrap_or_default(),
                        tags,
                    });
                }
                println!("{}", Table::new(rows));
            }
        }

        UnitCommands::Status { name, dirty } if *dirty => {
            show_dirty_status(conn, name, json_output, &config.defaults.global_excludes)?;
        }

        UnitCommands::Status { name, .. } => {
            let unit = resolve_unit(conn, name)?;
            let tags = queries::get_tags_for_unit(conn, unit.id)?;
            let tenant = queries::get_tenant_by_id(conn, unit.tenant_id)?;
            // Issue #150: `unit_path_history` has been written on every
            // rename since 001_initial.sql and read by nothing. This is the
            // detail view, so this is where the trail belongs.
            let prior_paths = queries::unit_path_history(conn, unit.id)?;

            if json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "unit": unit,
                        "tags": tags,
                        "tenant": tenant,
                        "prior_paths": prior_paths,
                    })
                );
            } else {
                println!("Unit: {}", unit.name);
                println!("  UUID:          {}", unit.uuid);
                println!("  Status:        {}", unit.status);
                println!(
                    "  Tenant:        {}",
                    tenant.map(|t| t.name).unwrap_or_else(|| "?".into())
                );
                println!(
                    "  Path:          {}",
                    unit.current_path.as_deref().unwrap_or("(none)")
                );
                println!("  Checksum mode: {}", unit.checksum_mode);
                println!("  Encrypted:     {}", unit.encrypt);
                println!("  Created:       {}", unit.created_at);
                if !tags.is_empty() {
                    println!("  Tags:          {}", tags.join(", "));
                }
                if !prior_paths.is_empty() {
                    // "recorded at" and not "moved away", because
                    // `observed_at` is when the unit was seen AT that path;
                    // the table has no departure timestamp.
                    println!("  Prior paths:   (newest first, recorded at)");
                    for rec in &prior_paths {
                        println!("    {}  {}", rec.observed_at, rec.path);
                    }
                }
            }
        }

        UnitCommands::Tag { name, add, remove } => {
            let unit = resolve_unit(conn, name)?;
            // Issue #241: pure precheck-then-UPDATE with no policy gate —
            // compute the resulting set in memory instead of writing it.
            if dry_run {
                let mut tags = queries::get_tags_for_unit(conn, unit.id)?;
                for tag in add {
                    if !tags.contains(tag) {
                        tags.push(tag.clone());
                    }
                }
                tags.retain(|t| !remove.contains(t));
                if json_output {
                    let mut obj = serde_json::json!({"name": unit.name, "tags": tags});
                    obj["dry_run"] = serde_json::json!(true);
                    println!("{obj}");
                } else {
                    println!(
                        "unit \"{}\": tags would become [{}] (DRY RUN — no changes made)",
                        unit.name,
                        tags.join(", ")
                    );
                }
                return Ok(());
            }
            // Issue #359: the database AND the unit's dotfile.
            let tags = crate::unit::tag_unit(conn, &unit, add, remove)?;
            if json_output {
                println!("{}", serde_json::json!({"name": unit.name, "tags": tags}));
            } else {
                println!("unit \"{}\": tags = [{}]", unit.name, tags.join(", "));
            }
        }

        UnitCommands::Rename { current, new } => {
            // Issue #241: reproduces `rename_unit`'s own preconditions
            // (unit exists, new name valid and free) so a dry run refuses
            // exactly what the real rename would refuse.
            if dry_run {
                queries::get_unit_by_name(conn, current)?
                    .ok_or_else(|| TapectlError::UnitNotFound(current.clone()))?;
                crate::naming::validate_unit_name(new)?;
                if queries::get_unit_by_name(conn, new)?.is_some() {
                    return Err(TapectlError::UnitAlreadyExists(new.clone()));
                }
                if json_output {
                    println!(
                        "{}",
                        serde_json::json!({"old_name": current, "new_name": new, "dry_run": true})
                    );
                } else {
                    println!(
                        "unit \"{current}\" would be renamed to \"{new}\" (DRY RUN — no \
                         changes made)"
                    );
                }
                return Ok(());
            }
            crate::unit::rename_unit(conn, current, new)?;
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"old_name": current, "new_name": new})
                );
            } else {
                println!("unit \"{current}\" renamed to \"{new}\"");
            }
        }

        UnitCommands::Discover => {
            // Issue #241: a filesystem-to-DB reconciliation (like
            // `archive_set sync`/`collection sync`) — the scan itself
            // decides created/updated/unchanged as it walks, so a faithful
            // preview would duplicate `discovery::discover`'s own logic.
            if dry_run {
                return Err(crate::cli::refuse_dry_run(
                    "unit discover",
                    "the watch-root scan decides created/updated/unchanged as it walks; a \
                     faithful preview would duplicate that reconciliation logic.",
                ));
            }
            let report = crate::unit::discovery::discover(conn, config)?;
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "created": report.created,
                        "updated": report.updated,
                        "unchanged": report.unchanged,
                        "errors": report.errors,
                    })
                );
            } else {
                println!(
                    "discover: {} created, {} updated, {} unchanged",
                    report.created, report.updated, report.unchanged
                );
                if !report.skipped_roots.is_empty() {
                    println!("  skipped roots: {}", report.skipped_roots.join(", "));
                }
                for err in &report.errors {
                    println!("  error: {err}");
                }
            }
        }

        UnitCommands::CheckIntegrity { name } => {
            crate::cli::operations::unit_check_integrity(conn, name, json_output)?;
        }

        UnitCommands::MarkTapeOnly { name, force } => {
            // Issue #241: `unit_mark_tape_only` (cli::operations, outside
            // this fix's file scope) enforces min_copies/min_locations —
            // a gate a dry run must reproduce exactly or it lies about
            // what the real run would refuse. Refuse rather than risk
            // that divergence.
            if dry_run {
                return Err(crate::cli::refuse_dry_run(
                    "unit mark-tape-only",
                    "it enforces the resolved policy's min_copies/min_locations, a gate a \
                     preview would have to reproduce exactly or risk being wrong.",
                ));
            }
            // Issue #348: the global `--yes` is the same advance Tier-2
            // consent `--force` is here — `consent::confirm`'s
            // non-interactive refusal tells the operator to re-run with
            // `--yes`, which is only true if it arrives.
            crate::cli::operations::unit_mark_tape_only(
                conn,
                config,
                name,
                *force || assume_yes,
                json_output,
            )?;
        }
    }
    Ok(())
}

/// Resolve a unit by name or path.
fn resolve_unit(conn: &Connection, name_or_path: &str) -> Result<crate::db::models::Unit> {
    // Try by name first
    if let Some(u) = queries::get_unit_by_name(conn, name_or_path)? {
        return Ok(u);
    }
    // Try by path
    if let Ok(abs) = std::fs::canonicalize(name_or_path) {
        if let Some(u) = queries::get_unit_by_path(conn, &abs.to_string_lossy())? {
            return Ok(u);
        }
    }
    Err(TapectlError::UnitNotFound(name_or_path.to_string()))
}

fn resolve_unit_name(conn: &Connection, unit_id: i64) -> Result<String> {
    let unit = conn.query_row(
        "SELECT name FROM units WHERE id = ?1",
        rusqlite::params![unit_id],
        |row| row.get(0),
    )?;
    Ok(unit)
}

/// One unit's dirty-scan verdict — split out from `show_dirty_status`'s
/// printing so the result is directly assertable in tests without
/// capturing stdout.
struct DirtyStatus {
    state: &'static str, // "clean" | "new" | "dirty"
    changes: crate::collection::fingerprint::FingerprintDiff,
}

/// `unit status --dirty`'s scan: reuses `fingerprint::classify` — the same
/// scan the Collection layer (`collection sync|status|plan`) and `report
/// dirty` use — so this can never disagree with them about whether a
/// unit's disk matches its last snapshot (issue #36/H10). `global_excludes`
/// is `config.defaults.global_excludes` (issue #49), kept in lockstep with
/// those other callers.
fn dirty_status(
    conn: &Connection,
    unit: &crate::db::models::Unit,
    global_excludes: &[String],
) -> Result<DirtyStatus> {
    use crate::collection::fingerprint::{self, PendingReason};

    Ok(match fingerprint::classify(conn, unit, global_excludes)? {
        None => DirtyStatus {
            state: "clean",
            changes: fingerprint::FingerprintDiff::default(),
        },
        Some(p) if p.reason == PendingReason::New => DirtyStatus {
            state: "new",
            changes: fingerprint::FingerprintDiff::default(),
        },
        Some(p) => DirtyStatus {
            state: "dirty",
            changes: p.changes,
        },
    })
}

fn show_dirty_status(
    conn: &Connection,
    name: &str,
    json_output: bool,
    global_excludes: &[String],
) -> Result<()> {
    let unit = resolve_unit(conn, name)?;
    let status = dirty_status(conn, &unit, global_excludes)?;

    if json_output {
        println!(
            "{}",
            serde_json::json!({
                "unit": unit.name,
                "state": status.state,
                "added": status.changes.added,
                "removed": status.changes.removed,
                "modified": status.changes.modified,
            })
        );
    } else {
        match status.state {
            "clean" => println!("unit \"{}\": clean", unit.name),
            "new" => println!("unit \"{}\": new — never archived", unit.name),
            _ => {
                println!(
                    "unit \"{}\": dirty ({} added, {} removed, {} modified)",
                    unit.name,
                    status.changes.added.len(),
                    status.changes.removed.len(),
                    status.changes.modified.len(),
                );
                // The audit's own wording ("shows specific changes") is why
                // this exists at all — a bare "dirty" doesn't tell an
                // operator whether it's safe to delete local data.
                for p in &status.changes.added {
                    println!("  + {p}");
                }
                for p in &status.changes.removed {
                    println!("  - {p}");
                }
                for p in &status.changes.modified {
                    println!("  ~ {p}");
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use tempfile::TempDir;

    /// Issue #171 / ADR-0012: `unit list --status` must be a usage error
    /// naming the accepted set for anything outside `units.status`'s CHECK
    /// constraint, not a silently empty (or unfiltered) result.
    #[test]
    fn validate_unit_status_rejects_a_typo_naming_accepted_values() {
        let err = validate_unit_status("actve").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("actve"), "{msg}");
        assert!(msg.contains("active"), "{msg}");
        assert!(msg.contains("tape_only"), "{msg}");
    }

    /// Issue #362: the `--status` closed set is the live CHECK, not a copy
    /// of a migration that a later one superseded (`retired` sat here long
    /// after anything could have set it).
    #[test]
    fn unit_statuses_equal_the_live_check() {
        let mut ours: Vec<String> = UNIT_STATUSES.iter().map(|s| s.to_string()).collect();
        ours.sort();
        assert_eq!(ours, crate::db::live_status_check("units"));
    }

    #[test]
    fn validate_unit_status_accepts_every_real_status() {
        for s in UNIT_STATUSES {
            assert!(validate_unit_status(s).is_ok(), "{s} should be accepted");
        }
    }

    /// `UnitRow`'s own `Serialize` shape (issue: C2 row-listing drift). Not
    /// wired into `unit list --json`, which already serializes
    /// `db::models::Unit` directly -- see the doc comment on `UnitRow`.
    #[test]
    fn pin_unit_rows_json_shape() {
        let rows = vec![
            UnitRow {
                name: "photos".to_string(),
                status: "active".to_string(),
                tenant: "alice".to_string(),
                path: "/home/alice/photos".to_string(),
                tags: "family, vacation".to_string(),
            },
            UnitRow {
                name: "scratch".to_string(),
                status: "tape_only".to_string(),
                tenant: "?".to_string(),
                path: String::new(),
                tags: String::new(),
            },
        ];
        let value = serde_json::to_value(&rows).unwrap();
        assert_eq!(
            serde_json::to_string(&value).unwrap(),
            r#"[{"name":"photos","path":"/home/alice/photos","status":"active","tags":"family, vacation","tenant":"alice"},{"name":"scratch","path":"","status":"tape_only","tags":"","tenant":"?"}]"#
        );
    }

    fn setup_unit(current_path: &str, checksum_mode: &str) -> (Connection, i64) {
        // Full migration set (not just 001) — snapshot_create's real walk
        // writes files.file_type/link_target, added by migration 005.
        let conn = crate::db::open_memory().unwrap();
        conn.execute(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('t', 0, 'active')",
            [],
        )
        .unwrap();
        let tid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, current_path, checksum_mode, encrypt, status)
             VALUES ('u1', 'unit1', ?1, ?2, ?3, 1, 'active')",
            params![tid, current_path, checksum_mode],
        )
        .unwrap();
        let uid = conn.last_insert_rowid();
        (conn, uid)
    }

    #[test]
    fn a_clean_unit_reports_clean() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), b"hello").unwrap();
        let (conn, _uid) = setup_unit(tmp.path().to_str().unwrap(), "mtime_size");
        crate::staging::snapshot_create(&conn, "unit1", &Config::default()).unwrap();

        let unit = queries::get_unit_by_name(&conn, "unit1").unwrap().unwrap();
        let status = dirty_status(&conn, &unit, &[]).unwrap();
        assert_eq!(status.state, "clean");
        assert!(status.changes.is_empty());
    }

    #[test]
    fn a_never_archived_unit_reports_new() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), b"hello").unwrap();
        let (conn, _uid) = setup_unit(tmp.path().to_str().unwrap(), "mtime_size");
        // No snapshot_create call — never archived.

        let unit = queries::get_unit_by_name(&conn, "unit1").unwrap().unwrap();
        let status = dirty_status(&conn, &unit, &[]).unwrap();
        assert_eq!(status.state, "new");
    }

    #[test]
    fn a_modified_unit_reports_dirty_and_names_the_changed_file() {
        let tmp = TempDir::new().unwrap();
        let file_path = tmp.path().join("f.txt");
        std::fs::write(&file_path, b"hello").unwrap();
        let (conn, _uid) = setup_unit(tmp.path().to_str().unwrap(), "mtime_size");
        crate::staging::snapshot_create(&conn, "unit1", &Config::default()).unwrap();

        std::fs::write(&file_path, b"hello, world! now a different size").unwrap();

        let unit = queries::get_unit_by_name(&conn, "unit1").unwrap().unwrap();
        let status = dirty_status(&conn, &unit, &[]).unwrap();
        assert_eq!(status.state, "dirty");
        assert_eq!(status.changes.modified, vec!["f.txt".to_string()]);
    }

    #[test]
    fn show_dirty_status_runs_end_to_end_for_a_dirty_unit() {
        // Wiring smoke test: the CLI-facing function must run to completion
        // (JSON and plain) against a real dirty unit, not just the
        // underlying dirty_status() helper the tests above exercise
        // directly.
        let tmp = TempDir::new().unwrap();
        let file_path = tmp.path().join("f.txt");
        std::fs::write(&file_path, b"hello").unwrap();
        let (conn, _uid) = setup_unit(tmp.path().to_str().unwrap(), "mtime_size");
        crate::staging::snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        std::fs::write(&file_path, b"hello, world! now a different size").unwrap();

        show_dirty_status(&conn, "unit1", false, &[]).expect("plain output must succeed");
        show_dirty_status(&conn, "unit1", true, &[]).expect("json output must succeed");
    }
}

#[cfg(test)]
mod tag_tests {
    //! Issue #359(a): `unit tag` must leave the unit's dotfile agreeing
    //! with the database, exactly as `unit rename` does -- otherwise a
    //! later adoption (`unit discover`, `collection sync`) or rebuild from
    //! the dotfile restores the tags the operator removed.
    use super::*;
    use tempfile::TempDir;

    fn harness() -> (Connection, TempDir, TapectlPaths) {
        let conn = crate::db::open_memory().unwrap();
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let paths = TapectlPaths::new(home);
        paths.ensure_dirs().unwrap();
        crate::tenant::add_tenant(&conn, &paths, "alice", None, false).unwrap();
        (conn, tmp, paths)
    }

    fn run_cmd(conn: &Connection, paths: &TapectlPaths, cmd: UnitCommands) {
        run(conn, paths, &Config::default(), &cmd, false, false, false).unwrap();
    }

    #[test]
    fn unit_tag_writes_the_resulting_tags_into_the_dotfile() {
        let (conn, tmp, paths) = harness();
        let src = tmp.path().join("photos");
        std::fs::create_dir_all(&src).unwrap();
        run_cmd(
            &conn,
            &paths,
            UnitCommands::Init {
                path: src.to_string_lossy().to_string(),
                tenant: "alice".into(),
                name: Some("photos".into()),
                tag: vec!["old".into()],
                archive_set: None,
            },
        );
        let dotfile_path = src.join(".tapectl-unit.toml");
        // Hand-set a policy key, to prove the rewrite round-trips it.
        let mut raw = std::fs::read_to_string(&dotfile_path).unwrap();
        raw.push_str("\n[policy]\nslice_size = \"500M\"\n");
        std::fs::write(&dotfile_path, raw).unwrap();

        run_cmd(
            &conn,
            &paths,
            UnitCommands::Tag {
                name: "photos".into(),
                add: vec!["family".into(), "vacation".into()],
                remove: vec!["old".into()],
            },
        );

        let df = crate::unit::dotfile::read_dotfile(&dotfile_path).unwrap();
        assert_eq!(
            df.tags,
            vec!["family".to_string(), "vacation".to_string()],
            "the dotfile must carry the tag set the database now holds"
        );
        assert_eq!(
            df.slice_size.as_deref(),
            Some("500M"),
            "rewriting the tags must not drop a [policy] key"
        );
        let unit = queries::get_unit_by_name(&conn, "photos").unwrap().unwrap();
        assert_eq!(queries::get_tags_for_unit(&conn, unit.id).unwrap(), df.tags);
    }

    /// A dry run changes nothing -- neither the database nor the dotfile.
    #[test]
    fn unit_tag_dry_run_leaves_the_dotfile_alone() {
        let (conn, tmp, paths) = harness();
        let src = tmp.path().join("photos");
        std::fs::create_dir_all(&src).unwrap();
        run_cmd(
            &conn,
            &paths,
            UnitCommands::Init {
                path: src.to_string_lossy().to_string(),
                tenant: "alice".into(),
                name: Some("photos".into()),
                tag: vec!["old".into()],
                archive_set: None,
            },
        );
        let dotfile_path = src.join(".tapectl-unit.toml");
        let before = std::fs::read_to_string(&dotfile_path).unwrap();
        run(
            &conn,
            &paths,
            &Config::default(),
            &UnitCommands::Tag {
                name: "photos".into(),
                add: vec!["new".into()],
                remove: vec!["old".into()],
            },
            false,
            true,
            false,
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&dotfile_path).unwrap(), before);
    }

    /// A unit whose dotfile is corrupt still gets its tags changed in the
    /// database -- the same warn-not-fail rule as `unit rename` (issue
    /// #110): the database is authoritative and has already committed.
    #[test]
    fn unit_tag_with_an_unreadable_dotfile_still_tags_in_the_database() {
        let (conn, tmp, paths) = harness();
        let src = tmp.path().join("photos");
        std::fs::create_dir_all(&src).unwrap();
        run_cmd(
            &conn,
            &paths,
            UnitCommands::Init {
                path: src.to_string_lossy().to_string(),
                tenant: "alice".into(),
                name: Some("photos".into()),
                tag: vec![],
                archive_set: None,
            },
        );
        std::fs::write(src.join(".tapectl-unit.toml"), "not [ valid toml = =").unwrap();
        run_cmd(
            &conn,
            &paths,
            UnitCommands::Tag {
                name: "photos".into(),
                add: vec!["family".into()],
                remove: vec![],
            },
        );
        let unit = queries::get_unit_by_name(&conn, "photos").unwrap().unwrap();
        assert_eq!(
            queries::get_tags_for_unit(&conn, unit.id).unwrap(),
            vec!["family".to_string()]
        );
    }
}

#[cfg(test)]
mod checksum_mode_tests {
    //! Issue #347: a new unit's `units.checksum_mode` comes from the resolved
    //! policy (dotfile `[policy]` > archive set > `[defaults]`), not a
    //! hardcoded `mtime_size`. Driven through `cli::unit::run`, so these
    //! prove the operator's own config reaches the command.
    use super::*;
    use tempfile::TempDir;

    fn harness() -> (Connection, TempDir, TapectlPaths) {
        let conn = crate::db::open_memory().unwrap();
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let paths = TapectlPaths::new(home);
        paths.ensure_dirs().unwrap();
        crate::tenant::add_tenant(&conn, &paths, "alice", None, false).unwrap();
        (conn, tmp, paths)
    }

    fn stored_mode(conn: &Connection, name: &str) -> String {
        queries::get_unit_by_name(conn, name)
            .unwrap()
            .unwrap_or_else(|| panic!("unit {name} must exist"))
            .checksum_mode
    }

    fn init(
        conn: &Connection,
        paths: &TapectlPaths,
        config: &Config,
        dir: &std::path::Path,
        name: &str,
        archive_set: Option<&str>,
    ) {
        std::fs::create_dir_all(dir).unwrap();
        run(
            conn,
            paths,
            config,
            &UnitCommands::Init {
                path: dir.to_string_lossy().to_string(),
                tenant: "alice".into(),
                name: Some(name.into()),
                tag: vec![],
                archive_set: archive_set.map(str::to_string),
            },
            false,
            false,
            false,
        )
        .unwrap();
    }

    #[test]
    fn unit_init_takes_checksum_mode_from_defaults() {
        let (conn, tmp, paths) = harness();
        let mut config = Config::default();
        config.defaults.checksum_mode = "sha256".into();
        init(&conn, &paths, &config, &tmp.path().join("a"), "a", None);
        assert_eq!(stored_mode(&conn, "a"), "sha256");
    }

    #[test]
    fn unit_init_takes_checksum_mode_from_the_archive_set_over_defaults() {
        let (conn, tmp, paths) = harness();
        conn.execute(
            "INSERT INTO archive_sets (name, checksum_mode) VALUES ('cold', 'sha256_on_archive')",
            [],
        )
        .unwrap();
        let mut config = Config::default();
        config.defaults.checksum_mode = "sha256".into();
        init(
            &conn,
            &paths,
            &config,
            &tmp.path().join("a"),
            "a",
            Some("cold"),
        );
        assert_eq!(stored_mode(&conn, "a"), "sha256_on_archive");
    }

    /// The control: nothing configured anywhere still means `mtime_size`.
    #[test]
    fn unit_init_with_nothing_configured_stays_mtime_size() {
        let (conn, tmp, paths) = harness();
        init(
            &conn,
            &paths,
            &Config::default(),
            &tmp.path().join("a"),
            "a",
            None,
        );
        assert_eq!(stored_mode(&conn, "a"), "mtime_size");
    }

    #[test]
    fn unit_init_bulk_takes_checksum_mode_from_defaults() {
        let (conn, _tmp, paths) = harness();
        // init-bulk derives each unit's name from its path, and a unit name
        // may not have a dot-led segment -- so not under TempDir's `.tmpXXX`.
        let tmp = tempfile::Builder::new().prefix("bulk").tempdir().unwrap();
        let parent = tmp.path().join("parent");
        for d in ["x", "y"] {
            std::fs::create_dir_all(parent.join(d)).unwrap();
        }
        let mut config = Config::default();
        config.defaults.checksum_mode = "sha256".into();
        run(
            &conn,
            &paths,
            &config,
            &UnitCommands::InitBulk {
                path: parent.to_string_lossy().to_string(),
                tenant: "alice".into(),
                tag: vec![],
            },
            false,
            false,
            false,
        )
        .unwrap();
        let units = queries::list_units(&conn, None, None).unwrap();
        assert_eq!(units.len(), 2);
        for u in units {
            assert_eq!(u.checksum_mode, "sha256", "unit {}", u.name);
        }
    }

    /// Adoption by `unit discover`: a dotfile with no `[policy]
    /// checksum_mode` defers to the archive set, then `[defaults]`.
    #[test]
    fn unit_discover_adopts_with_the_resolved_checksum_mode() {
        let (conn, tmp, paths) = harness();
        let roots = tmp.path().join("roots");
        let dir = roots.join("found");
        std::fs::create_dir_all(&dir).unwrap();
        crate::unit::dotfile::write_dotfile(
            &dir.join(".tapectl-unit.toml"),
            &crate::unit::dotfile::UnitDotfile {
                uuid: uuid::Uuid::new_v4().to_string(),
                name: "found".into(),
                created: "2026-01-01T00:00:00Z".into(),
                tags: vec![],
                tenant: "alice".into(),
                archive_set: None,
                checksum_mode: None,
                compression: None,
                slice_size: None,
                warehouse_copies: None,
                exclude_patterns: vec![],
            },
        )
        .unwrap();
        let mut config = Config::default();
        config.defaults.checksum_mode = "sha256".into();
        config.discovery.watch_roots = vec![roots.to_string_lossy().to_string()];
        run(
            &conn,
            &paths,
            &config,
            &UnitCommands::Discover,
            false,
            false,
            false,
        )
        .unwrap();
        assert_eq!(stored_mode(&conn, "found"), "sha256");
    }

    /// A dotfile's own `[policy] checksum_mode` still wins at adoption.
    #[test]
    fn unit_discover_keeps_the_dotfiles_own_checksum_mode() {
        let (conn, tmp, paths) = harness();
        let roots = tmp.path().join("roots");
        let dir = roots.join("found");
        std::fs::create_dir_all(&dir).unwrap();
        crate::unit::dotfile::write_dotfile(
            &dir.join(".tapectl-unit.toml"),
            &crate::unit::dotfile::UnitDotfile {
                uuid: uuid::Uuid::new_v4().to_string(),
                name: "found".into(),
                created: "2026-01-01T00:00:00Z".into(),
                tags: vec![],
                tenant: "alice".into(),
                archive_set: None,
                checksum_mode: Some("mtime_size".into()),
                compression: None,
                slice_size: None,
                warehouse_copies: None,
                exclude_patterns: vec![],
            },
        )
        .unwrap();
        let mut config = Config::default();
        config.defaults.checksum_mode = "sha256".into();
        config.discovery.watch_roots = vec![roots.to_string_lossy().to_string()];
        run(
            &conn,
            &paths,
            &config,
            &UnitCommands::Discover,
            false,
            false,
            false,
        )
        .unwrap();
        assert_eq!(stored_mode(&conn, "found"), "mtime_size");
    }
}

#[cfg(test)]
mod mark_tape_only_consent_tests {
    //! Issue #348: `unit mark-tape-only` asks for ADR-0008 Tier-2 consent
    //! through `cli::consent::confirm`, whose non-interactive refusal says
    //! "re-run with --yes to proceed". That is true only if the global
    //! `--yes` reaches the handler, so these go through `cli::unit::run` —
    //! the dispatch `main` calls with `cli.yes` — not the handler directly.
    //! (`cfg(test)` makes stdin a non-terminal, so a refusal never prompts.)
    use super::*;
    use tempfile::TempDir;

    /// `photos`: 2 copies in 2 locations (`coverage::tests`' fixture),
    /// against a `[defaults] min_copies` of 3 — below policy, not dirty and
    /// not never-archived, so only the Tier-2 copy shortfall can stop it.
    fn below_policy() -> (Connection, TempDir, TapectlPaths, Config) {
        let (conn, _unit, _vol) = crate::policy::coverage::tests::setup_unit_with_deposit("active");
        let tmp = TempDir::new().unwrap();
        let paths = TapectlPaths::new(tmp.path().join("home"));
        let mut config = Config::default();
        config.defaults.min_copies = 3;
        (conn, tmp, paths, config)
    }

    fn mark(
        conn: &Connection,
        paths: &TapectlPaths,
        config: &Config,
        force: bool,
        assume_yes: bool,
    ) -> Result<()> {
        run(
            conn,
            paths,
            config,
            &UnitCommands::MarkTapeOnly {
                name: "photos".into(),
                force,
            },
            false,
            false,
            assume_yes,
        )
    }

    fn status(conn: &Connection) -> String {
        queries::get_unit_by_name(conn, "photos")
            .unwrap()
            .unwrap()
            .status
    }

    #[test]
    fn the_global_yes_confirms_a_below_policy_unit_in_advance() {
        let (conn, _tmp, paths, config) = below_policy();
        mark(&conn, &paths, &config, false, true)
            .expect("--yes is Tier-2 consent given in advance (ADR-0008)");
        assert_eq!(status(&conn), "tape_only");
    }

    /// The negative control: neither flag, no terminal — refused, with the
    /// facts and the flags that would confirm, and nothing changed.
    #[test]
    fn without_yes_or_force_a_non_interactive_run_refuses() {
        let (conn, _tmp, paths, config) = below_policy();
        let msg = mark(&conn, &paths, &config, false, false)
            .expect_err("no consent, no terminal: refuse")
            .to_string();
        assert!(msg.contains("insufficient copies: 2 < 3"), "{msg}");
        assert!(msg.contains("re-run with --yes to proceed"), "{msg}");
        assert!(
            msg.contains("`--force` or `--yes` confirms this in advance"),
            "the refusal names both flags that now reach the handler: {msg}"
        );
        assert_eq!(status(&conn), "active");
    }

    #[test]
    fn force_still_confirms_a_below_policy_unit_in_advance() {
        let (conn, _tmp, paths, config) = below_policy();
        mark(&conn, &paths, &config, true, false).expect("--force confirms, as before");
        assert_eq!(status(&conn), "tape_only");
    }
}
