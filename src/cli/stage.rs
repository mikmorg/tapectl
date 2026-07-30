use clap::Subcommand;
use rusqlite::{params, Connection};
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

#[derive(Tabled)]
struct StageRow {
    #[tabled(rename = "ID")]
    id: i64,
    #[tabled(rename = "Unit")]
    unit: String,
    #[tabled(rename = "Ver")]
    version: i64,
    #[tabled(rename = "Status")]
    status: String,
    #[tabled(rename = "Slices")]
    slices: String,
    #[tabled(rename = "Encrypted")]
    encrypted_size: String,
    #[tabled(rename = "Staged At")]
    staged_at: String,
}

pub fn run(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    command: &StageCommands,
    json_output: bool,
) -> Result<()> {
    match command {
        StageCommands::List { status } => {
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
                        slices: row
                            .get::<_, Option<i64>>(4)?
                            .map(|n| n.to_string())
                            .unwrap_or_default(),
                        encrypted_size: enc_size
                            .map(|s| format!("{} MB", s / (1024 * 1024)))
                            .unwrap_or_default(),
                        staged_at: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;

            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!(rows
                        .iter()
                        .map(|r| serde_json::json!({
                            "id": r.id, "unit": r.unit, "version": r.version,
                            "status": r.status, "slices": r.slices,
                        }))
                        .collect::<Vec<_>>()))
                    .unwrap()
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
                println!("  dar size:  {} MB", dar_size.unwrap_or(0) / (1024 * 1024));
                println!("  encrypted: {} MB", enc_size.unwrap_or(0) / (1024 * 1024));
                if let Some(sa) = &staged_at {
                    println!("  Staged at: {sa}");
                }
                if !slices.is_empty() {
                    println!("  Slices:");
                    for (num, size, enc, sha) in &slices {
                        println!(
                            "    #{num}: {} MB dar, {} MB enc, sha256={}",
                            size / (1024 * 1024),
                            enc.unwrap_or(0) / (1024 * 1024),
                            sha.as_deref().unwrap_or("(none)"),
                        );
                    }
                }
            }
        }

        StageCommands::Create { name, version } => {
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
                            "no unstaged snapshot for unit \"{name}\" — run `tapectl snapshot create` first"
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
                    "staged: {} ({} slices, {} MB dar, {} MB encrypted)",
                    name,
                    num_slices.unwrap_or(0),
                    total_dar.unwrap_or(0) / (1024 * 1024),
                    total_enc.unwrap_or(0) / (1024 * 1024),
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
        )
        .unwrap();

        // Release the first stage set's slices (force, since there's no
        // completed write backing it in this test).
        crate::staging::clean::clean_staging(&conn, true).unwrap();
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
        )
        .unwrap();

        crate::staging::clean::clean_staging(&conn, true).unwrap();
        run(
            &conn,
            &paths,
            &config,
            &StageCommands::Create {
                name: "unit1".to_string(),
                version: Some(1),
            },
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
        )
        .unwrap();
    }
}
