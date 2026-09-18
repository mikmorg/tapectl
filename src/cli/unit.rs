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
        /// Filter by status (active, tape_only, missing, retired)
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

    /// Check file integrity against staged checksums
    CheckIntegrity {
        /// Unit name
        name: String,
    },

    /// Mark unit as tape-only (local data can be deleted)
    MarkTapeOnly {
        /// Unit name
        name: String,
        /// Override copy/location requirements
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

/// `units.status`'s CHECK constraint (`src/db/migrations/001_initial.sql`).
const UNIT_STATUSES: &[&str] = &["active", "tape_only", "missing", "retired"];

/// `unit list --status` is a usage error when it names anything other than
/// one of `UNIT_STATUSES` (issue #171, ADR-0012) — an unrecognised value
/// used to answer with an empty (or unfiltered) list rather than refusing.
fn validate_unit_status(value: &str) -> Result<()> {
    crate::config::validate_closed_set("--status", value, UNIT_STATUSES)
        .map_err(TapectlError::Other)
}

pub fn run(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    command: &UnitCommands,
    json_output: bool,
    dry_run: bool,
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
            let unit_id = crate::unit::init_unit(
                conn,
                paths,
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
            let results = crate::unit::init_bulk(conn, paths, path, tenant, tag)?;
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
            for tag in add {
                queries::add_tag_to_unit(conn, unit.id, tag)?;
            }
            for tag in remove {
                queries::remove_tag_from_unit(conn, unit.id, tag)?;
            }
            let tags = queries::get_tags_for_unit(conn, unit.id)?;
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
            let report = crate::unit::discovery::discover(conn, &config.discovery.watch_roots)?;
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
            crate::cli::operations::unit_mark_tape_only(conn, config, name, *force, json_output)?;
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
