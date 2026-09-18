use clap::Subcommand;
use rusqlite::{params, Connection};
use serde::Serialize;
use tabled::{Table, Tabled};

use crate::config::{Config, TapectlPaths};
use crate::error::{Result, TapectlError};
use crate::staging;

#[derive(Subcommand, Debug)]
pub enum SnapshotCommands {
    /// Create a snapshot (fast directory walk + manifest)
    Create {
        /// Unit name
        name: String,
    },

    /// List snapshots
    List {
        /// Filter by unit name
        #[arg(long)]
        unit: Option<String>,
        /// Filter by status (created, staged, current, superseded,
        /// reclaimable, purged, failed)
        #[arg(long)]
        status: Option<String>,
    },

    /// Compare two snapshot versions of a unit
    Diff {
        /// Unit name
        name: String,
        /// First version number
        #[arg(long)]
        v1: i64,
        /// Second version number
        #[arg(long)]
        v2: i64,
    },

    /// Delete an unwritten snapshot
    Delete {
        /// Unit name
        name: String,
        /// Snapshot version to delete
        #[arg(long)]
        version: i64,
        /// Force delete even if staged (but not written)
        #[arg(long)]
        force: bool,
    },

    /// Purge a reclaimable snapshot (remove DB records)
    Purge {
        /// Unit name
        name: String,
        /// Snapshot version
        #[arg(long)]
        version: i64,
    },

    /// Mark a snapshot as reclaimable (with enforced preconditions)
    MarkReclaimable {
        /// Unit name
        name: String,
        /// Snapshot version
        #[arg(long)]
        version: i64,
        /// Override preconditions
        #[arg(long)]
        force: bool,
    },
}

#[derive(Tabled, Serialize)]
struct SnapshotRow {
    #[tabled(rename = "ID")]
    id: i64,
    #[tabled(rename = "Unit")]
    unit: String,
    #[tabled(rename = "Ver")]
    version: i64,
    #[tabled(rename = "Status")]
    status: String,
    /// Table-only until CTO decision 2026-09-11 (architecture review C2
    /// follow-up, C2b). Renamed to `file_count` to match `snapshot create
    /// --json`'s existing key for the same `file_count` column.
    #[tabled(rename = "Files", display_with = "display_opt_i64")]
    #[serde(rename = "file_count")]
    files: Option<i64>,
    /// Table-only until CTO decision 2026-09-11 (architecture review C2
    /// follow-up, C2b). Raw bytes; renamed to `total_size` to match
    /// `snapshot create --json`'s existing key for the same `total_size`
    /// column.
    #[tabled(rename = "Size", display_with = "display_size_mb")]
    #[serde(rename = "total_size")]
    size: Option<i64>,
    /// Table-only until CTO decision 2026-09-11 (architecture review C2
    /// follow-up, C2b). `created_at` is NOT NULL, so this stays a plain
    /// `String` (not `Option`) -- renamed to match the DB column name and
    /// the "Created" timestamp convention used elsewhere (e.g. `TenantRow`,
    /// `StagingRow.staged_at`).
    #[tabled(rename = "Created")]
    #[serde(rename = "created_at")]
    created: String,
}

fn display_opt_i64(v: &Option<i64>) -> String {
    v.map(|n| n.to_string()).unwrap_or_default()
}

fn display_size_mb(v: &Option<i64>) -> String {
    v.map(crate::util::format_bytes_binary).unwrap_or_default()
}

/// `snapshot list --json` shape. `files`/`size`/`created` were table-only
/// until CTO decision 2026-09-11 (architecture review C2 follow-up, C2b).
fn snapshot_rows_to_json(rows: &[SnapshotRow]) -> serde_json::Value {
    serde_json::to_value(rows).unwrap()
}

/// `snapshots.status`'s CHECK constraint (`src/db/migrations/001_initial.sql`).
const SNAPSHOT_STATUSES: &[&str] = &[
    "created",
    "staged",
    "current",
    "superseded",
    "reclaimable",
    "purged",
    "failed",
];

/// `snapshot list --status` is a usage error when it names anything other
/// than one of `SNAPSHOT_STATUSES` (issue #171, ADR-0012).
fn validate_snapshot_status(value: &str) -> Result<()> {
    crate::config::validate_closed_set("--status", value, SNAPSHOT_STATUSES)
        .map_err(TapectlError::Other)
}

pub fn run(
    conn: &Connection,
    _paths: &TapectlPaths,
    config: &Config,
    command: &SnapshotCommands,
    json_output: bool,
    dry_run: bool,
) -> Result<()> {
    match command {
        SnapshotCommands::Create { name } => {
            // Issue #241: the sha256 directory walk IS the work — the
            // only way to know whether content changed (and so whether a
            // new version would be minted) is to run it, so there is
            // nothing cheaper to offer as a preview.
            if dry_run {
                return Err(crate::cli::refuse_dry_run(
                    "snapshot create",
                    "the directory walk and hash comparison that decide whether a new \
                     version would be minted ARE the command's own work.",
                ));
            }
            // ADR-0012 / issue #159: `snapshot_create_detailed` may report
            // an existing version instead of minting one — `outcome.minted`
            // says which. Exit 0 either way; this is success, not a
            // refusal. `outcome.snapshot_id` names the row (new or reused)
            // to look up below regardless.
            let outcome = staging::snapshot_create_detailed(conn, name, config)?;

            let (total_size, file_count): (Option<i64>, Option<i64>) = conn.query_row(
                "SELECT total_size, file_count FROM snapshots WHERE id = ?1",
                params![outcome.snapshot_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;

            if json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "snapshot_id": outcome.snapshot_id,
                        "unit": name,
                        "version": outcome.version,
                        "total_size": total_size,
                        "file_count": file_count,
                        // Same shape whether minted or not (issue #159):
                        // one key, `true` on the created path, `false`
                        // when an existing version was reported instead —
                        // never two different JSON shapes for the same
                        // command.
                        "minted": outcome.minted,
                    })
                );
            } else if outcome.minted {
                println!(
                    "snapshot created: {} v{} ({} files, {})",
                    name,
                    outcome.version,
                    file_count.unwrap_or(0),
                    crate::util::format_bytes_binary(total_size.unwrap_or(0)),
                );
            } else {
                println!(
                    "unit \"{name}\" is unchanged since v{}; no snapshot created",
                    outcome.version,
                );
            }
        }

        SnapshotCommands::Diff { name, v1, v2 } => {
            crate::cli::operations::snapshot_diff(conn, name, *v1, *v2, json_output)?;
        }

        SnapshotCommands::Delete {
            name,
            version,
            force,
        } => {
            // Issue #241: reproduces `snapshot_delete`'s two refusals
            // (completed writes; staged data without --force) so a dry
            // run refuses exactly what the real delete would refuse,
            // without touching a row or a staged file.
            if dry_run {
                let unit = crate::db::queries::get_unit_by_name(conn, name)?
                    .ok_or_else(|| TapectlError::UnitNotFound(name.clone()))?;
                let (snap_id, _status): (i64, String) = conn
                    .query_row(
                        "SELECT id, status FROM snapshots WHERE unit_id = ?1 AND version = ?2",
                        params![unit.id, version],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .map_err(|_| {
                        TapectlError::Other(format!("snapshot v{version} not found for \"{name}\""))
                    })?;
                let write_count: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM writes w
                     JOIN stage_sets ss ON ss.id = w.stage_set_id
                     WHERE ss.snapshot_id = ?1 AND w.status = 'completed'",
                    params![snap_id],
                    |row| row.get(0),
                )?;
                if write_count > 0 {
                    return Err(TapectlError::Other(format!(
                        "snapshot v{version} has {write_count} completed write(s) — cannot \
                         delete"
                    )));
                }
                let staged_count: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM stage_sets WHERE snapshot_id = ?1 AND status = \
                     'staged'",
                    params![snap_id],
                    |row| row.get(0),
                )?;
                if staged_count > 0 && !force {
                    return Err(TapectlError::Other(format!(
                        "snapshot v{version} has staged data — use --force to delete anyway"
                    )));
                }
                if json_output {
                    println!(
                        "{}",
                        serde_json::json!({"unit": name, "version": version, "dry_run": true})
                    );
                } else {
                    println!("would delete snapshot {name} v{version} (DRY RUN — no changes made)");
                }
                return Ok(());
            }
            crate::cli::operations::snapshot_delete(conn, name, *version, *force, json_output)?;
        }

        SnapshotCommands::Purge { name, version } => {
            // Issue #241: reproduces `snapshot_purge`'s own precondition
            // (status must be 'reclaimable') so a dry run refuses exactly
            // what the real purge would refuse.
            if dry_run {
                let unit = crate::db::queries::get_unit_by_name(conn, name)?
                    .ok_or_else(|| TapectlError::UnitNotFound(name.clone()))?;
                let status: String = conn
                    .query_row(
                        "SELECT status FROM snapshots WHERE unit_id = ?1 AND version = ?2",
                        params![unit.id, version],
                        |row| row.get(0),
                    )
                    .map_err(|_| {
                        TapectlError::Other(format!("snapshot v{version} not found for \"{name}\""))
                    })?;
                if status != "reclaimable" {
                    return Err(TapectlError::Other(format!(
                        "snapshot v{version} status is \"{status}\", must be \"reclaimable\" \
                         to purge"
                    )));
                }
                if json_output {
                    println!(
                        "{}",
                        serde_json::json!({"unit": name, "version": version, "dry_run": true})
                    );
                } else {
                    println!("would purge snapshot {name} v{version} (DRY RUN — no changes made)");
                }
                return Ok(());
            }
            crate::cli::operations::snapshot_purge(conn, name, *version, json_output)?;
        }

        SnapshotCommands::MarkReclaimable {
            name,
            version,
            force,
        } => {
            // Issue #241: enforces policy preconditions including the
            // tape-only 2x copy multiplier (Milestone 6) — a gate a dry
            // run must reproduce exactly or it lies about what the real
            // run would refuse. Refuse rather than risk that divergence.
            if dry_run {
                return Err(crate::cli::refuse_dry_run(
                    "snapshot mark-reclaimable",
                    "it enforces the resolved policy's coverage preconditions (including the \
                     tape-only 2x copy multiplier), a gate a preview would have to reproduce \
                     exactly or risk being wrong.",
                ));
            }
            crate::cli::operations::snapshot_mark_reclaimable(
                conn,
                config,
                name,
                *version,
                *force,
                json_output,
            )?;
        }

        SnapshotCommands::List { unit, status } => {
            if let Some(st) = status {
                validate_snapshot_status(st)?;
            }
            let mut sql = String::from(
                "SELECT s.id, u.name, s.version, s.status, s.file_count,
                        s.total_size, s.created_at
                 FROM snapshots s JOIN units u ON u.id = s.unit_id WHERE 1=1",
            );
            let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

            if let Some(unit_name) = unit {
                sql.push_str(" AND u.name = ?");
                param_values.push(Box::new(unit_name.clone()));
            }
            if let Some(st) = status {
                sql.push_str(" AND s.status = ?");
                param_values.push(Box::new(st.clone()));
            }
            sql.push_str(" ORDER BY u.name, s.version DESC");

            let params: Vec<&dyn rusqlite::types::ToSql> =
                param_values.iter().map(|p| p.as_ref()).collect();
            let mut stmt = conn.prepare(&sql)?;
            let rows: Vec<SnapshotRow> = stmt
                .query_map(params.as_slice(), |row| {
                    let size: Option<i64> = row.get(5)?;
                    Ok(SnapshotRow {
                        id: row.get(0)?,
                        unit: row.get(1)?,
                        version: row.get(2)?,
                        status: row.get(3)?,
                        files: row.get::<_, Option<i64>>(4)?,
                        size,
                        created: row.get(6)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;

            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&snapshot_rows_to_json(&rows)).unwrap()
                );
            } else if rows.is_empty() {
                println!("no snapshots found");
            } else {
                println!("{}", Table::new(rows));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Issue #171 / ADR-0012: `snapshot list --status` must be a usage
    /// error naming the accepted set for anything outside
    /// `snapshots.status`'s CHECK constraint.
    #[test]
    fn validate_snapshot_status_rejects_a_typo_naming_accepted_values() {
        let err = validate_snapshot_status("curent").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("curent"), "{msg}");
        assert!(msg.contains("current"), "{msg}");
        assert!(msg.contains("reclaimable"), "{msg}");
    }

    #[test]
    fn validate_snapshot_status_accepts_every_real_status() {
        for s in SNAPSHOT_STATUSES {
            assert!(
                validate_snapshot_status(s).is_ok(),
                "{s} should be accepted"
            );
        }
    }

    /// `snapshot list --json` shape (issue: C2 row-listing drift).
    /// `files`/`size`/`created` are additive since CTO decision 2026-09-11
    /// (architecture review C2 follow-up, C2b). The byte count
    /// (125_829_121) is deliberately not an even multiple of 1 MiB, proving
    /// the JSON carries the raw fact rather than a value recomputed from
    /// the table's "120 MiB" text.
    #[test]
    fn pin_snapshot_rows_json_shape() {
        let rows = vec![
            SnapshotRow {
                id: 10,
                unit: "backups".to_string(),
                version: 3,
                status: "current".to_string(),
                files: Some(42),
                size: Some(125_829_121),
                created: "2026-07-01T00:00:00Z".to_string(),
            },
            SnapshotRow {
                id: 11,
                unit: "photos".to_string(),
                version: 1,
                status: "reclaimable".to_string(),
                files: None,
                size: None,
                created: String::new(),
            },
        ];
        let value = snapshot_rows_to_json(&rows);
        assert_eq!(
            serde_json::to_string(&value).unwrap(),
            r#"[{"created_at":"2026-07-01T00:00:00Z","file_count":42,"id":10,"status":"current","total_size":125829121,"unit":"backups","version":3},{"created_at":"","file_count":null,"id":11,"status":"reclaimable","total_size":null,"unit":"photos","version":1}]"#
        );
    }
}
