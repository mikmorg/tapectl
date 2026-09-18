use clap::Subcommand;
use rusqlite::{params, Connection};
use serde::Serialize;
use tabled::{Table, Tabled};

use crate::config::{Config, TapectlPaths};
use crate::error::{Result, TapectlError};
use crate::staging;

#[derive(Subcommand, Debug)]
pub enum StageCommands {
    /// Create staged slices (validate → dar → encrypt → checksums)
    Create {
        /// Unit name
        name: String,

        /// Re-stage a specific snapshot version instead of the latest
        /// unstaged one — for when `staging clean` already released the
        /// first stage set's slices and another copy is wanted (issue #53).
        /// Refuses if a stage set for that version already has live
        /// slices; use `volume write` to consume them, or `staging clean`
        /// to release them first.
        #[arg(long)]
        version: Option<i64>,
    },

    /// List stage sets
    List {
        /// Filter by status (staging, staged, failed, cleaned)
        #[arg(long)]
        status: Option<String>,
    },

    /// Show details for a stage set
    Info {
        /// Unit name
        name: String,
        /// Snapshot version
        #[arg(long)]
        version: i64,
    },
}

#[derive(Tabled, Serialize)]
struct StageRow {
    #[tabled(rename = "ID")]
    id: i64,
    #[tabled(rename = "Unit")]
    unit: String,
    #[tabled(rename = "Ver")]
    version: i64,
    #[tabled(rename = "Status")]
    status: String,
    /// Raw `Option<i64>`, not a pre-rendered string (issue #205's audit).
    /// A slice COUNT is a quantity; `SnapshotRow.files` next door already
    /// carried the correct shape. `display_opt_i64` keeps the table's `-`
    /// for NULL unchanged.
    #[tabled(rename = "Slices", display_with = "display_opt_i64")]
    slices: Option<i64>,
    /// Table-only until CTO decision 2026-09-11 (architecture review C2
    /// follow-up, C2b). Raw bytes; renamed to `total_encrypted_size` to
    /// match `stage info --json`'s existing key for the same
    /// `ss.total_encrypted_size` column (see `StageCommands::Info` below).
    #[tabled(rename = "Encrypted", display_with = "display_encrypted_mb")]
    #[serde(rename = "total_encrypted_size")]
    encrypted_size: Option<i64>,
    /// Table-only until CTO decision 2026-09-11 (architecture review C2
    /// follow-up, C2b).
    #[tabled(rename = "Staged At", display_with = "display_opt_string")]
    staged_at: Option<String>,
}

fn display_encrypted_mb(v: &Option<i64>) -> String {
    v.map(crate::util::format_bytes_binary).unwrap_or_default()
}

fn display_opt_i64(v: &Option<i64>) -> String {
    v.map(|n| n.to_string()).unwrap_or_else(|| "-".to_string())
}

fn display_opt_string(v: &Option<String>) -> String {
    v.clone().unwrap_or_default()
}

/// `stage list --json` shape. `encrypted_size`/`staged_at` were table-only
/// until CTO decision 2026-09-11 (architecture review C2 follow-up, C2b).
fn stage_rows_to_json(rows: &[StageRow]) -> serde_json::Value {
    serde_json::to_value(rows).unwrap()
}

/// `stage_sets.status`'s CHECK constraint (`src/db/migrations/001_initial.sql`).
const STAGE_SET_STATUSES: &[&str] = &["staging", "staged", "failed", "cleaned"];

/// `stage list --status` is a usage error when it names anything other than
/// one of `STAGE_SET_STATUSES` (issue #171, ADR-0012).
fn validate_stage_status(value: &str) -> Result<()> {
    crate::config::validate_closed_set("--status", value, STAGE_SET_STATUSES)
        .map_err(TapectlError::Other)
}

pub fn run(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    command: &StageCommands,
    json_output: bool,
    dry_run: bool,
) -> Result<()> {
    match command {
        StageCommands::List { status } => {
            if let Some(st) = status {
                validate_stage_status(st)?;
            }
            let mut sql = String::from(
                "SELECT ss.id, u.name, s.version, ss.status, ss.num_slices,
                        ss.total_encrypted_size, ss.staged_at
                 FROM stage_sets ss
                 JOIN snapshots s ON s.id = ss.snapshot_id
                 JOIN units u ON u.id = s.unit_id
                 WHERE 1=1",
            );
            let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
            if let Some(st) = status {
                sql.push_str(" AND ss.status = ?");
                param_values.push(Box::new(st.clone()));
            }
            sql.push_str(" ORDER BY ss.created_at DESC");

            let params_ref: Vec<&dyn rusqlite::types::ToSql> =
                param_values.iter().map(|p| p.as_ref()).collect();
            let mut stmt = conn.prepare(&sql)?;
            let rows: Vec<StageRow> = stmt
                .query_map(params_ref.as_slice(), |row| {
                    let enc_size: Option<i64> = row.get(5)?;
                    Ok(StageRow {
                        id: row.get(0)?,
                        unit: row.get(1)?,
                        version: row.get(2)?,
                        status: row.get(3)?,
                        slices: row.get::<_, Option<i64>>(4)?,
                        encrypted_size: enc_size,
                        staged_at: row.get::<_, Option<String>>(6)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;

            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&stage_rows_to_json(&rows)).unwrap()
                );
            } else if rows.is_empty() {
                println!("no stage sets found");
            } else {
                println!("{}", Table::new(rows));
            }
        }

        StageCommands::Info { name, version } => {
            let unit = crate::db::queries::get_unit_by_name(conn, name)?
                .ok_or_else(|| TapectlError::UnitNotFound(name.clone()))?;

            type Row = (
                i64,
                String,
                Option<String>,
                Option<String>,
                Option<i64>,
                Option<i64>,
                Option<i64>,
                Option<String>,
            );
            let (ss_id, status, dar_ver, dar_cmd, num_slices, dar_size, enc_size, staged_at): Row =
                conn.query_row(
                    "SELECT ss.id, ss.status, ss.dar_version, ss.dar_command,
                            ss.num_slices, ss.total_dar_size, ss.total_encrypted_size, ss.staged_at
                     FROM stage_sets ss
                     JOIN snapshots s ON s.id = ss.snapshot_id
                     WHERE s.unit_id = ?1 AND s.version = ?2
                     ORDER BY ss.created_at DESC, ss.id DESC LIMIT 1",
                    params![unit.id, version],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                            row.get(6)?,
                            row.get(7)?,
                        ))
                    },
                )
                .map_err(|_| {
                    TapectlError::Other(format!("no stage set for \"{name}\" v{version}"))
                })?;

            // Once issue #53 lets a snapshot carry several stage sets (a
            // re-stage after `staging clean`), the query above still shows
            // only the newest — this count lets both output modes say so
            // without hiding the others' existence.
            let stage_set_count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM stage_sets ss
                 JOIN snapshots s ON s.id = ss.snapshot_id
                 WHERE s.unit_id = ?1 AND s.version = ?2",
                params![unit.id, version],
                |row| row.get(0),
            )?;

            // Get slices
            let mut stmt = conn.prepare(
                "SELECT slice_number, size_bytes, encrypted_bytes, sha256_encrypted
                 FROM stage_slices WHERE stage_set_id = ?1 ORDER BY slice_number",
            )?;
            let slices: Vec<(i64, i64, Option<i64>, Option<String>)> = stmt
                .query_map(params![ss_id], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;

            if json_output {
                let slice_json: Vec<serde_json::Value> = slices
                    .iter()
                    .map(|(num, size, enc, sha)| {
                        serde_json::json!({
                            "slice": num, "dar_bytes": size,
                            "encrypted_bytes": enc, "sha256": sha,
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::json!({
                        "unit": name, "version": version, "stage_set_id": ss_id,
                        "status": status, "dar_version": dar_ver,
                        "num_slices": num_slices, "total_dar_size": dar_size,
                        "total_encrypted_size": enc_size, "slices": slice_json,
                        "stage_set_count": stage_set_count,
                    })
                );
            } else {
                println!("Stage set for {name} v{version} (id={ss_id})");
                if stage_set_count > 1 {
                    println!(
                        "  ({stage_set_count} stage sets exist for this version; showing the newest)"
                    );
                }
                println!("  Status:    {status}");
                if let Some(dv) = &dar_ver {
                    println!("  dar:       {dv}");
                }
                if let Some(dc) = &dar_cmd {
                    println!("  command:   {dc}");
                }
                println!("  Slices:    {}", num_slices.unwrap_or(0));
                println!(
                    "  dar size:  {}",
                    crate::util::format_bytes_binary(dar_size.unwrap_or(0))
                );
                println!(
                    "  encrypted: {}",
                    crate::util::format_bytes_binary(enc_size.unwrap_or(0))
                );
                if let Some(sa) = &staged_at {
                    println!("  Staged at: {sa}");
                }
                if !slices.is_empty() {
                    println!("  Slices:");
                    for (num, size, enc, sha) in &slices {
                        println!(
                            "    #{num}: {} dar, {} enc, sha256={}",
                            crate::util::format_bytes_binary(*size),
                            crate::util::format_bytes_binary(enc.unwrap_or(0)),
                            sha.as_deref().unwrap_or("(none)"),
                        );
                    }
                }
            }
        }

        StageCommands::Create { name, version } => {
            // Issue #241: the dar archive + age encryption pipeline IS
            // the work (hours and a tape's worth of staging disk, per
            // `collection run`'s own dry-run comment) — most of the
            // command with none of the safety if run "dry".
            if dry_run {
                return Err(crate::cli::refuse_dry_run(
                    "stage create",
                    "the dar archive, sha256 validation and age encryption pipeline IS the \
                     work — there is no cheaper way to know what would be staged.",
                ));
            }
            let unit = crate::db::queries::get_unit_by_name(conn, name)?
                .ok_or_else(|| TapectlError::UnitNotFound(name.clone()))?;

            let snapshot_id: i64 = match version {
                None => {
                    // Unchanged: the latest 'created' (never-yet-staged)
                    // snapshot for this unit.
                    conn.query_row(
                        "SELECT id FROM snapshots WHERE unit_id = ?1 AND status = 'created'
                         ORDER BY version DESC LIMIT 1",
                        params![unit.id],
                        |row| row.get(0),
                    )
                    .map_err(|_| {
                        TapectlError::Other(format!(
                            "no unstaged snapshot for unit \"{name}\" — run `tapectl snapshot create {name}` first if the contents changed, or re-stage an existing version with `tapectl stage create {name} --version <N>` (`tapectl snapshot list --unit {name}` shows them)"
                        ))
                    })?
                }
                Some(v) => {
                    // Re-stage: select that unit's snapshot at version `v`
                    // regardless of its status, then gate on whether any
                    // existing stage set for it already has live slices.
                    let snapshot_id: i64 = conn
                        .query_row(
                            "SELECT id FROM snapshots WHERE unit_id = ?1 AND version = ?2",
                            params![unit.id, v],
                            |row| row.get(0),
                        )
                        .map_err(|_| {
                            TapectlError::Other(format!(
                                "unit \"{name}\" has no snapshot at version {v}"
                            ))
                        })?;

                    let statuses: Vec<String> = conn
                        .prepare("SELECT status FROM stage_sets WHERE snapshot_id = ?1")?
                        .query_map(params![snapshot_id], |row| row.get(0))?
                        .collect::<std::result::Result<Vec<_>, _>>()?;

                    if statuses
                        .iter()
                        .any(|s| staging::stage_set_has_live_slices(s))
                    {
                        return Err(TapectlError::Other(format!(
                            "unit \"{name}\" v{v} already has a stage set with live slices — \
                             use `tapectl volume write` to consume them, or \
                             `tapectl staging clean` to release them first"
                        )));
                    }

                    snapshot_id
                }
            };

            let stage_set_id = staging::stage_create(conn, paths, config, snapshot_id)?;

            // Fetch results for display
            let (num_slices, total_dar, total_enc): (Option<i64>, Option<i64>, Option<i64>) = conn
                .query_row(
                    "SELECT num_slices, total_dar_size, total_encrypted_size
                     FROM stage_sets WHERE id = ?1",
                    params![stage_set_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?;

            if json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "stage_set_id": stage_set_id,
                        "unit": name,
                        "num_slices": num_slices,
                        "total_dar_size": total_dar,
                        "total_encrypted_size": total_enc,
                    })
                );
            } else {
                println!(
                    "staged: {} ({} slices, {} dar, {} encrypted)",
                    name,
                    num_slices.unwrap_or(0),
                    crate::util::format_bytes_binary(total_dar.unwrap_or(0)),
                    crate::util::format_bytes_binary(total_enc.unwrap_or(0)),
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Tests for issue #53's `stage create --version` re-staging flow.
    //!
    //! Shared setup builds a real tenant + unit (via `unit::init_unit`, so
    //! a real dotfile + real tenant keys exist — `stage_create` refuses to
    //! encrypt without active tenant keys) and runs the real `dar` binary,
    //! matching the pattern already established in
    //! `staging::tests::stage_create_uses_archive_set_resolved_slice_size_not_global_default`.

    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Issue #171 / ADR-0012: `stage list --status` must be a usage error
    /// naming the accepted set for anything outside `stage_sets.status`'s
    /// CHECK constraint.
    #[test]
    fn validate_stage_status_rejects_a_typo_naming_accepted_values() {
        let err = validate_stage_status("stagng").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("stagng"), "{msg}");
        assert!(msg.contains("staging"), "{msg}");
        assert!(msg.contains("cleaned"), "{msg}");
    }

    #[test]
    fn validate_stage_status_accepts_every_real_status() {
        for s in STAGE_SET_STATUSES {
            assert!(validate_stage_status(s).is_ok(), "{s} should be accepted");
        }
    }

    /// `stage list --json` shape (issue: C2 row-listing drift).
    /// `encrypted_size`/`staged_at` are additive since CTO decision
    /// 2026-09-11 (architecture review C2 follow-up, C2b). The byte count
    /// (125_829_121) is deliberately not an even multiple of 1 MiB, proving
    /// the JSON carries the raw fact rather than a value recomputed from
    /// the table's "120 MiB" text.
    #[test]
    fn pin_stage_rows_json_shape() {
        let rows = vec![
            StageRow {
                id: 1,
                unit: "backups".to_string(),
                version: 2,
                status: "staged".to_string(),
                slices: Some(3),
                encrypted_size: Some(125_829_121),
                staged_at: Some("2026-07-01T00:00:00Z".to_string()),
            },
            StageRow {
                id: 2,
                unit: "photos".to_string(),
                version: 1,
                status: "staging".to_string(),
                slices: None,
                encrypted_size: None,
                staged_at: None,
            },
        ];
        let value = stage_rows_to_json(&rows);
        assert_eq!(
            serde_json::to_string(&value).unwrap(),
            r#"[{"id":1,"slices":3,"staged_at":"2026-07-01T00:00:00Z","status":"staged","total_encrypted_size":125829121,"unit":"backups","version":2},{"id":2,"slices":null,"staged_at":null,"status":"staging","total_encrypted_size":null,"unit":"photos","version":1}]"#
        );
    }

    /// (conn, paths, config) with a tenant "alice" and a unit "unit1"
    /// registered, source content already written. Caller drives
    /// `snapshot create`/`stage create` itself.
    fn setup() -> (Connection, TapectlPaths, Config, TempDir) {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let paths = TapectlPaths::new(home);
        paths.ensure_dirs().unwrap();
        let conn = crate::db::open(&paths.db_file).unwrap();

        let staging_dir = tmp.path().join("staging");
        fs::create_dir_all(&staging_dir).unwrap();
        let mut config = Config::default();
        config.staging.directory = staging_dir.to_string_lossy().to_string();
        config.dar.binary = "dar".to_string();

        crate::tenant::add_tenant(&conn, &paths, "op", None, true).unwrap();
        crate::tenant::add_tenant(&conn, &paths, "alice", None, false).unwrap();
        // Issue #115 / ADR-0005: `stage_create` refuses without a registered
        // escrow recipient. Public key only, exactly as production does.
        {
            conn.execute(
                "INSERT INTO tenants (name, is_operator, status) VALUES ('escrow-holder', 0, 'active')",
                [],
            )
            .unwrap();
            let holder_id = conn.last_insert_rowid();
            let kp = crate::crypto::keys::generate_keypair();
            crate::db::queries::insert_escrow_key(
                &conn,
                holder_id,
                "test-escrow",
                &kp.fingerprint,
                &kp.public_key,
                Some("test escrow recipient (ADR-0005)"),
            )
            .unwrap();
        }

        let src = tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("f.txt"), b"restage test content").unwrap();

        crate::unit::init_unit(
            &conn,
            &paths,
            src.to_str().unwrap(),
            "alice",
            Some("unit1"),
            &[],
            None,
        )
        .unwrap();

        (conn, paths, config, tmp)
    }

    #[test]
    fn create_without_version_behaves_exactly_as_before() {
        let (conn, paths, config, _tmp) = setup();

        // No snapshot yet at all: same error message as before this change.
        let err = run(
            &conn,
            &paths,
            &config,
            &StageCommands::Create {
                name: "unit1".to_string(),
                version: None,
            },
            false,
            false,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("no unstaged snapshot for unit"),
            "unexpected error message: {err}"
        );

        // With a 'created' snapshot present, no-version staging still
        // finds and stages it exactly as before.
        let snap_id = crate::staging::snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        run(
            &conn,
            &paths,
            &config,
            &StageCommands::Create {
                name: "unit1".to_string(),
                version: None,
            },
            false,
            false,
        )
        .unwrap();

        let status: String = conn
            .query_row(
                "SELECT status FROM snapshots WHERE id = ?1",
                params![snap_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "staged");
    }

    #[test]
    fn create_with_version_on_nonexistent_version_errors() {
        let (conn, paths, config, _tmp) = setup();
        crate::staging::snapshot_create(&conn, "unit1", &Config::default()).unwrap();

        let err = run(
            &conn,
            &paths,
            &config,
            &StageCommands::Create {
                name: "unit1".to_string(),
                version: Some(99),
            },
            false,
            false,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("no snapshot at version 99"),
            "unexpected error message: {err}"
        );
    }

    #[test]
    fn create_with_version_refuses_when_a_stage_set_is_staged() {
        let (conn, paths, config, _tmp) = setup();
        crate::staging::snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        run(
            &conn,
            &paths,
            &config,
            &StageCommands::Create {
                name: "unit1".to_string(),
                version: None,
            },
            false,
            false,
        )
        .unwrap();

        // The stage set from the first `create` is 'staged' (live slices)
        // — a re-stage of the same version must be refused.
        let err = run(
            &conn,
            &paths,
            &config,
            &StageCommands::Create {
                name: "unit1".to_string(),
                version: Some(1),
            },
            false,
            false,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("already has a stage set with live slices"),
            "unexpected error message: {msg}"
        );
        assert!(msg.contains("volume write"));
        assert!(msg.contains("staging clean"));
    }

    #[test]
    fn create_with_version_succeeds_once_the_only_stage_set_is_cleaned() {
        let (conn, paths, config, _tmp) = setup();
        crate::staging::snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        run(
            &conn,
            &paths,
            &config,
            &StageCommands::Create {
                name: "unit1".to_string(),
                version: None,
            },
            false,
            false,
        )
        .unwrap();

        // Release the first stage set's slices (force, since there's no
        // completed write backing it in this test).
        crate::staging::clean::clean_staging(&conn, &config, true).unwrap();
        let status: String = conn
            .query_row("SELECT status FROM stage_sets LIMIT 1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(status, "cleaned");

        // Re-stage must now succeed and produce a second stage set.
        run(
            &conn,
            &paths,
            &config,
            &StageCommands::Create {
                name: "unit1".to_string(),
                version: Some(1),
            },
            false,
            false,
        )
        .unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM stage_sets", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            count, 2,
            "re-staging must add a second stage set, not replace the first"
        );
    }

    /// Issue #53 change 4: once a snapshot can have several stage sets,
    /// `stage info` must not silently hide the siblings. It still shows
    /// only the newest (via `ORDER BY created_at DESC, id DESC`, so two
    /// stage sets created in the same wall-clock second still resolve
    /// deterministically to the just-created one), but must be able to
    /// report how many exist. `run()` only prints to stdout, so this
    /// exercises the same count query `Info`'s handler runs, against the
    /// same fixture, rather than scraping process stdout (which would
    /// race other tests' output under parallel `cargo test`).
    #[test]
    fn info_query_counts_sibling_stage_sets_after_a_restage() {
        let (conn, paths, config, _tmp) = setup();
        crate::staging::snapshot_create(&conn, "unit1", &Config::default()).unwrap();
        run(
            &conn,
            &paths,
            &config,
            &StageCommands::Create {
                name: "unit1".to_string(),
                version: None,
            },
            false,
            false,
        )
        .unwrap();

        // Only one stage set yet — count must be 1, and both output modes
        // of `Info` must succeed without printing a sibling note.
        let unit = crate::db::queries::get_unit_by_name(&conn, "unit1")
            .unwrap()
            .unwrap();
        let count_before: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM stage_sets ss
                 JOIN snapshots s ON s.id = ss.snapshot_id
                 WHERE s.unit_id = ?1 AND s.version = ?2",
                params![unit.id, 1i64],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count_before, 1);
        run(
            &conn,
            &paths,
            &config,
            &StageCommands::Info {
                name: "unit1".to_string(),
                version: 1,
            },
            false,
            false,
        )
        .unwrap();
        run(
            &conn,
            &paths,
            &config,
            &StageCommands::Info {
                name: "unit1".to_string(),
                version: 1,
            },
            true,
            false,
        )
        .unwrap();

        crate::staging::clean::clean_staging(&conn, &config, true).unwrap();
        run(
            &conn,
            &paths,
            &config,
            &StageCommands::Create {
                name: "unit1".to_string(),
                version: Some(1),
            },
            false,
            false,
        )
        .unwrap();

        let count_after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM stage_sets ss
                 JOIN snapshots s ON s.id = ss.snapshot_id
                 WHERE s.unit_id = ?1 AND s.version = ?2",
                params![unit.id, 1i64],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            count_after, 2,
            "the exact query `stage info` uses to build stage_set_count must \
             see both stage sets once a re-stage has happened"
        );

        // `Info` must still resolve to a single row (the newest) and not
        // error, in both output modes, now that two exist for this version.
        run(
            &conn,
            &paths,
            &config,
            &StageCommands::Info {
                name: "unit1".to_string(),
                version: 1,
            },
            false,
            false,
        )
        .unwrap();
        run(
            &conn,
            &paths,
            &config,
            &StageCommands::Info {
                name: "unit1".to_string(),
                version: 1,
            },
            true,
            false,
        )
        .unwrap();
    }
}
