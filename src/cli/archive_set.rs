use clap::Subcommand;
use rusqlite::{params, Connection};
use tabled::{Table, Tabled};

use crate::config::Config;
use crate::db::events;
use crate::error::{Result, TapectlError};

/// Compression values `dar` accepts via `-z`. An archive_set's `compression`
/// now reaches the dar invocation directly (issue #92 made the dotfile
/// policy layer stop unconditionally shadowing it), so a bogus value must be
/// rejected here rather than surfacing as a runtime dar failure later.
const VALID_COMPRESSION_VALUES: &[&str] =
    &["none", "gzip", "bzip2", "lzo", "xz", "lzma", "zstd", "lz4"];

fn validate_compression(value: &str) -> Result<()> {
    if VALID_COMPRESSION_VALUES.contains(&value) {
        Ok(())
    } else {
        Err(TapectlError::Other(format!(
            "invalid compression \"{value}\": accepted values are {}",
            VALID_COMPRESSION_VALUES.join(", ")
        )))
    }
}

#[derive(Subcommand, Debug)]
pub enum ArchiveSetCommands {
    /// Create a new archive set policy
    Create {
        /// Archive set name
        name: String,
        /// Minimum copy count
        #[arg(long)]
        min_copies: Option<i64>,
        /// Required locations (comma-separated)
        #[arg(long)]
        required_locations: Option<String>,
        /// Encryption enabled
        #[arg(long)]
        encrypt: Option<bool>,
        /// Compression mode
        #[arg(long)]
        compression: Option<String>,
        /// Checksum mode
        #[arg(long)]
        checksum_mode: Option<String>,
        /// Slice size (e.g., "2400G")
        #[arg(long)]
        slice_size: Option<String>,
        /// Verify interval in days
        #[arg(long)]
        verify_interval_days: Option<i64>,
        /// Description
        #[arg(long, short)]
        description: Option<String>,
    },

    /// Edit an existing archive set
    Edit {
        /// Archive set name
        name: String,
        /// Minimum copy count
        #[arg(long)]
        min_copies: Option<i64>,
        /// Required locations (comma-separated)
        #[arg(long)]
        required_locations: Option<String>,
        /// Encryption enabled
        #[arg(long)]
        encrypt: Option<bool>,
        /// Compression mode
        #[arg(long)]
        compression: Option<String>,
        /// Checksum mode
        #[arg(long)]
        checksum_mode: Option<String>,
        /// Slice size (e.g., "2400G")
        #[arg(long)]
        slice_size: Option<String>,
        /// Verify interval in days
        #[arg(long)]
        verify_interval_days: Option<i64>,
        /// Description
        #[arg(long, short)]
        description: Option<String>,
    },

    /// List archive sets
    List,

    /// Show archive set details
    Info {
        /// Archive set name
        name: String,
    },

    /// Sync archive sets from config.toml
    Sync,
}

#[derive(Tabled)]
struct ArchiveSetRow {
    #[tabled(rename = "Name")]
    name: String,
    #[tabled(rename = "Copies")]
    min_copies: String,
    #[tabled(rename = "Locations")]
    locations: String,
    #[tabled(rename = "Verify Days")]
    verify_days: String,
    #[tabled(rename = "Units")]
    unit_count: i64,
}

pub fn run(
    conn: &Connection,
    config: &Config,
    command: &ArchiveSetCommands,
    json_output: bool,
) -> Result<()> {
    match command {
        ArchiveSetCommands::Create {
            name,
            min_copies,
            required_locations,
            encrypt,
            compression,
            checksum_mode,
            slice_size,
            verify_interval_days,
            description,
        } => {
            if let Some(c) = compression {
                validate_compression(c)?;
            }
            let locations_json = required_locations.as_ref().map(|locs| {
                let arr: Vec<&str> = locs.split(',').map(|s| s.trim()).collect();
                serde_json::to_string(&arr).unwrap()
            });
            let slice_bytes = slice_size
                .as_ref()
                .map(|s| crate::staging::parse_size_to_bytes(s));

            conn.execute(
                "INSERT INTO archive_sets (name, description, min_copies, required_locations,
                 encrypt, compression, checksum_mode, slice_size, verify_interval_days)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    name,
                    description,
                    min_copies,
                    locations_json,
                    encrypt.map(|b| b as i64),
                    compression,
                    checksum_mode,
                    slice_bytes,
                    verify_interval_days,
                ],
            )?;
            let id = conn.last_insert_rowid();
            events::log_created(conn, "archive_set", id, name, None)?;

            if json_output {
                println!("{}", serde_json::json!({"id": id, "name": name}));
            } else {
                println!("archive set \"{name}\" created (id={id})");
            }
        }

        ArchiveSetCommands::Edit {
            name,
            min_copies,
            required_locations,
            encrypt,
            compression,
            checksum_mode,
            slice_size,
            verify_interval_days,
            description,
        } => {
            if let Some(c) = compression {
                validate_compression(c)?;
            }
            let id: i64 = conn
                .query_row(
                    "SELECT id FROM archive_sets WHERE name = ?1",
                    params![name],
                    |row| row.get(0),
                )
                .map_err(|_| TapectlError::Other(format!("archive set \"{name}\" not found")))?;

            // Snapshot old values BEFORE any UPDATE runs, so every per-field
            // event below records a real old value instead of `None`
            // (issue #48 item 5).
            #[allow(clippy::type_complexity)]
            let (
                old_min_copies,
                old_locations,
                old_encrypt,
                old_compression,
                old_checksum_mode,
                old_slice_size,
                old_verify_days,
                old_description,
            ): (
                Option<i64>,
                Option<String>,
                Option<i64>,
                Option<String>,
                Option<String>,
                Option<i64>,
                Option<i64>,
                Option<String>,
            ) = conn.query_row(
                "SELECT min_copies, required_locations, encrypt, compression, checksum_mode,
                        slice_size, verify_interval_days, description
                 FROM archive_sets WHERE id = ?1",
                params![id],
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
            )?;

            // Every field UPDATE plus its audit event runs in one
            // transaction: a failure partway through must not leave some
            // fields changed and others not (issue #48 item 5). Mirrors
            // the `conn.unchecked_transaction()` idiom already used by
            // `unit::rename_unit`'s sibling call sites (`volume/session.rs`,
            // `volume/write.rs`, `cli/operations.rs`, `cli/key.rs`).
            let tx = conn.unchecked_transaction()?;

            if let Some(v) = min_copies {
                tx.execute(
                    "UPDATE archive_sets SET min_copies = ?1, updated_at = datetime('now') WHERE id = ?2",
                    params![v, id],
                )?;
                events::log_field_change(
                    &tx,
                    "archive_set",
                    id,
                    name,
                    "edited",
                    "min_copies",
                    old_min_copies.map(|v| v.to_string()).as_deref(),
                    &v.to_string(),
                    None,
                )?;
            }
            if let Some(locs) = required_locations {
                let arr: Vec<&str> = locs.split(',').map(|s| s.trim()).collect();
                let json = serde_json::to_string(&arr).unwrap();
                tx.execute(
                    "UPDATE archive_sets SET required_locations = ?1, updated_at = datetime('now') WHERE id = ?2",
                    params![json, id],
                )?;
                events::log_field_change(
                    &tx,
                    "archive_set",
                    id,
                    name,
                    "edited",
                    "required_locations",
                    old_locations.as_deref(),
                    &json,
                    None,
                )?;
            }
            if let Some(v) = encrypt {
                tx.execute(
                    "UPDATE archive_sets SET encrypt = ?1, updated_at = datetime('now') WHERE id = ?2",
                    params![*v as i64, id],
                )?;
                events::log_field_change(
                    &tx,
                    "archive_set",
                    id,
                    name,
                    "edited",
                    "encrypt",
                    old_encrypt.map(|v| (v != 0).to_string()).as_deref(),
                    &v.to_string(),
                    None,
                )?;
            }
            if let Some(v) = compression {
                tx.execute(
                    "UPDATE archive_sets SET compression = ?1, updated_at = datetime('now') WHERE id = ?2",
                    params![v, id],
                )?;
                events::log_field_change(
                    &tx,
                    "archive_set",
                    id,
                    name,
                    "edited",
                    "compression",
                    old_compression.as_deref(),
                    v,
                    None,
                )?;
            }
            if let Some(v) = checksum_mode {
                tx.execute(
                    "UPDATE archive_sets SET checksum_mode = ?1, updated_at = datetime('now') WHERE id = ?2",
                    params![v, id],
                )?;
                events::log_field_change(
                    &tx,
                    "archive_set",
                    id,
                    name,
                    "edited",
                    "checksum_mode",
                    old_checksum_mode.as_deref(),
                    v,
                    None,
                )?;
            }
            if let Some(v) = slice_size {
                let bytes = crate::staging::parse_size_to_bytes(v);
                tx.execute(
                    "UPDATE archive_sets SET slice_size = ?1, updated_at = datetime('now') WHERE id = ?2",
                    params![bytes, id],
                )?;
                events::log_field_change(
                    &tx,
                    "archive_set",
                    id,
                    name,
                    "edited",
                    "slice_size",
                    old_slice_size.map(|v| v.to_string()).as_deref(),
                    &bytes.to_string(),
                    None,
                )?;
            }
            if let Some(v) = verify_interval_days {
                tx.execute(
                    "UPDATE archive_sets SET verify_interval_days = ?1, updated_at = datetime('now') WHERE id = ?2",
                    params![v, id],
                )?;
                events::log_field_change(
                    &tx,
                    "archive_set",
                    id,
                    name,
                    "edited",
                    "verify_interval_days",
                    old_verify_days.map(|v| v.to_string()).as_deref(),
                    &v.to_string(),
                    None,
                )?;
            }
            if let Some(v) = description {
                tx.execute(
                    "UPDATE archive_sets SET description = ?1, updated_at = datetime('now') WHERE id = ?2",
                    params![v, id],
                )?;
                events::log_field_change(
                    &tx,
                    "archive_set",
                    id,
                    name,
                    "edited",
                    "description",
                    old_description.as_deref(),
                    v,
                    None,
                )?;
            }

            tx.commit()?;

            if json_output {
                println!("{}", serde_json::json!({"name": name, "updated": true}));
            } else {
                println!("archive set \"{name}\" updated");
            }
        }

        ArchiveSetCommands::List => {
            let mut stmt = conn.prepare(
                "SELECT a.name, a.min_copies, a.required_locations, a.verify_interval_days,
                        (SELECT COUNT(*) FROM units u WHERE u.archive_set_id = a.id) as unit_count
                 FROM archive_sets a ORDER BY a.name",
            )?;
            let rows: Vec<ArchiveSetRow> = stmt
                .query_map([], |row| {
                    Ok(ArchiveSetRow {
                        name: row.get(0)?,
                        min_copies: row
                            .get::<_, Option<i64>>(1)?
                            .map(|n| n.to_string())
                            .unwrap_or("-".into()),
                        locations: row.get::<_, Option<String>>(2)?.unwrap_or("-".into()),
                        verify_days: row
                            .get::<_, Option<i64>>(3)?
                            .map(|n| n.to_string())
                            .unwrap_or("-".into()),
                        unit_count: row.get(4)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;

            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!(rows
                        .iter()
                        .map(|r| serde_json::json!({"name": r.name, "min_copies": r.min_copies, "units": r.unit_count}))
                        .collect::<Vec<_>>()))
                    .unwrap()
                );
            } else if rows.is_empty() {
                println!("no archive sets defined");
            } else {
                println!("{}", Table::new(rows));
            }
        }

        ArchiveSetCommands::Info { name } => {
            type Row = (
                i64,
                Option<String>,
                Option<i64>,
                Option<String>,
                Option<i64>,
                Option<String>,
                Option<String>,
                Option<i64>,
                Option<i64>,
                String,
                String,
            );
            let (
                id,
                desc,
                min_copies,
                locs,
                encrypt,
                compression,
                checksum_mode,
                slice_size,
                verify_days,
                created,
                updated,
            ): Row = conn
                .query_row(
                    "SELECT id, description, min_copies, required_locations, encrypt,
                            compression, checksum_mode, slice_size, verify_interval_days,
                            created_at, updated_at
                     FROM archive_sets WHERE name = ?1",
                    params![name],
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
                            row.get(8)?,
                            row.get(9)?,
                            row.get(10)?,
                        ))
                    },
                )
                .map_err(|_| TapectlError::Other(format!("archive set \"{name}\" not found")))?;

            let unit_count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM units WHERE archive_set_id = ?1",
                params![id],
                |row| row.get(0),
            )?;

            if json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "name": name, "description": desc, "min_copies": min_copies,
                        "required_locations": locs, "encrypt": encrypt,
                        "compression": compression, "checksum_mode": checksum_mode,
                        "slice_size": slice_size, "verify_interval_days": verify_days,
                        "units": unit_count,
                    })
                );
            } else {
                println!("Archive set: {name}");
                if let Some(d) = &desc {
                    println!("  Description:      {d}");
                }
                println!(
                    "  Min copies:       {}",
                    min_copies.map(|n| n.to_string()).unwrap_or("-".into())
                );
                println!("  Req. locations:   {}", locs.as_deref().unwrap_or("-"));
                println!(
                    "  Encrypt:          {}",
                    encrypt
                        .map(|n| if n != 0 { "yes" } else { "no" })
                        .unwrap_or("-")
                );
                println!(
                    "  Compression:      {}",
                    compression.as_deref().unwrap_or("-")
                );
                println!(
                    "  Checksum mode:    {}",
                    checksum_mode.as_deref().unwrap_or("-")
                );
                if let Some(sz) = slice_size {
                    println!("  Slice size:       {} GB", sz / (1024 * 1024 * 1024));
                }
                println!(
                    "  Verify interval:  {} days",
                    verify_days.map(|n| n.to_string()).unwrap_or("-".into())
                );
                println!("  Units using:      {unit_count}");
                println!("  Created:          {created}");
                println!("  Updated:          {updated}");
            }
        }

        ArchiveSetCommands::Sync => {
            let mut created = 0;
            let mut updated = 0;
            for as_cfg in &config.archive_sets {
                // Same guard as create/edit: `sync` writes archive_sets rows
                // straight from config.toml, so without this a bogus
                // `compression` in the config file walks past the CLI
                // validation and only fails later at `dar -z` (issue #92).
                if let Some(c) = &as_cfg.compression {
                    validate_compression(c)?;
                }
                let locations_json = as_cfg
                    .required_locations
                    .as_ref()
                    .map(|locs| serde_json::to_string(locs).unwrap());
                let slice_bytes = as_cfg
                    .slice_size
                    .as_ref()
                    .map(|s| crate::staging::parse_size_to_bytes(s));
                let encrypt_int = as_cfg.encrypt.map(|b| b as i64);

                let existing: Option<i64> = conn
                    .query_row(
                        "SELECT id FROM archive_sets WHERE name = ?1",
                        params![as_cfg.name],
                        |row| row.get(0),
                    )
                    .ok();

                if let Some(id) = existing {
                    conn.execute(
                        "UPDATE archive_sets SET min_copies = ?1, required_locations = ?2,
                         encrypt = ?3, compression = ?4, checksum_mode = ?5,
                         slice_size = ?6, verify_interval_days = ?7,
                         updated_at = datetime('now')
                         WHERE id = ?8",
                        params![
                            as_cfg.min_copies,
                            locations_json,
                            encrypt_int,
                            as_cfg.compression,
                            as_cfg.checksum_mode,
                            slice_bytes,
                            as_cfg.verify_interval_days,
                            id,
                        ],
                    )?;
                    // Issue #48 item 6: this branch used to update the row
                    // and log nothing, while its sibling three lines below
                    // (the "new row" branch) already calls log_created —
                    // an asymmetry in the audit trail with no justification.
                    events::log_event(
                        conn,
                        "archive_set",
                        id,
                        Some(&as_cfg.name),
                        "synced",
                        None,
                        None,
                        None,
                        None,
                        None,
                    )?;
                    updated += 1;
                } else {
                    conn.execute(
                        "INSERT INTO archive_sets (name, min_copies, required_locations,
                         encrypt, compression, checksum_mode, slice_size, verify_interval_days)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                        params![
                            as_cfg.name,
                            as_cfg.min_copies,
                            locations_json,
                            encrypt_int,
                            as_cfg.compression,
                            as_cfg.checksum_mode,
                            slice_bytes,
                            as_cfg.verify_interval_days,
                        ],
                    )?;
                    let id = conn.last_insert_rowid();
                    events::log_created(conn, "archive_set", id, &as_cfg.name, None)?;
                    created += 1;
                }
            }
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"created": created, "updated": updated})
                );
            } else {
                println!("sync: {created} created, {updated} updated from config.toml");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_conn() -> Connection {
        crate::db::open_memory().unwrap()
    }

    fn create_cmd(
        name: &str,
        min_copies: Option<i64>,
        compression: Option<&str>,
    ) -> ArchiveSetCommands {
        ArchiveSetCommands::Create {
            name: name.to_string(),
            min_copies,
            required_locations: None,
            encrypt: None,
            compression: compression.map(|s| s.to_string()),
            checksum_mode: None,
            slice_size: None,
            verify_interval_days: None,
            description: None,
        }
    }

    /// Issue #92: since a dotfile no longer unconditionally shadows the
    /// archive_set's `compression`, a bogus value now actually reaches
    /// `dar -z <value>` at write time — reject it here instead of letting
    /// it surface as a runtime dar failure.
    #[test]
    fn create_rejects_invalid_compression() {
        let conn = fresh_conn();
        let config = Config::default();
        let err = run(
            &conn,
            &config,
            &create_cmd("cold", None, Some("not-a-real-codec")),
            false,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("not-a-real-codec"),
            "error must name the invalid value, got: {msg}"
        );
        assert!(
            conn.query_row(
                "SELECT COUNT(*) FROM archive_sets WHERE name = 'cold'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap()
                == 0,
            "no archive_set row should be created when compression is invalid"
        );
    }

    /// Issue #48 item 4: the `unit_count` subqueries in `List`/`Info` were
    /// always correct SQL — they simply had nothing to count, since
    /// nothing wrote `units.archive_set_id`. This proves the count becomes
    /// non-zero once a unit is linked through the REAL writer path
    /// (`unit::init_unit`), not by asserting the SQL is right in the
    /// abstract — "verify, don't assume."
    #[test]
    fn list_and_info_report_nonzero_unit_count_once_a_unit_is_linked() {
        let conn = fresh_conn();
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let paths = crate::config::TapectlPaths::new(home);
        paths.ensure_dirs().unwrap();
        crate::tenant::add_tenant(&conn, &paths, "alice", None, false).unwrap();

        let config = Config::default();
        run(&conn, &config, &create_cmd("cold", Some(3), None), false).unwrap();

        // The identical shape of subquery `List`/`Info` run, against the
        // pre-link state — proven to read 0, the pre-#48 behavior this fix
        // escapes.
        let count_for_cold = |conn: &Connection| -> i64 {
            conn.query_row(
                "SELECT COUNT(*) FROM units u
                 JOIN archive_sets a ON a.id = u.archive_set_id
                 WHERE a.name = 'cold'",
                [],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(count_for_cold(&conn), 0);

        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        crate::unit::init_unit(
            &conn,
            &paths,
            src.to_str().unwrap(),
            "alice",
            Some("unit1"),
            &[],
            Some("cold"),
        )
        .unwrap();

        assert_eq!(
            count_for_cold(&conn),
            1,
            "unit_count must become non-zero once a unit is actually linked \
             through the real writer path, now that #48 gives archive_set_id one"
        );

        // The real CLI handlers must also run cleanly end-to-end against
        // this now-linked state (List and Info both run the identical
        // shape of subquery just proven above).
        run(&conn, &config, &ArchiveSetCommands::List, true).unwrap();
        run(
            &conn,
            &config,
            &ArchiveSetCommands::Info {
                name: "cold".to_string(),
            },
            true,
        )
        .unwrap();
    }

    /// Issue #48 item 5: `Edit` used to log one generic "edited" event
    /// with field/old/new all `None`. It must instead log one event PER
    /// changed field, with real old and new values.
    #[test]
    fn edit_emits_per_field_events_with_real_old_and_new_values() {
        let conn = fresh_conn();
        let config = Config::default();
        run(
            &conn,
            &config,
            &create_cmd("cold", Some(2), Some("none")),
            false,
        )
        .unwrap();

        run(
            &conn,
            &config,
            &ArchiveSetCommands::Edit {
                name: "cold".to_string(),
                min_copies: Some(5),
                required_locations: None,
                encrypt: None,
                compression: Some("lzma".to_string()),
                checksum_mode: None,
                slice_size: None,
                verify_interval_days: None,
                description: None,
            },
            false,
        )
        .unwrap();

        let mut stmt = conn
            .prepare(
                "SELECT field, old_value, new_value FROM events
                 WHERE entity_type = 'archive_set' AND action = 'edited'
                 ORDER BY field",
            )
            .unwrap();
        let rows: Vec<(String, Option<String>, String)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(
            rows.len(),
            2,
            "exactly the two edited fields must be logged, not one generic event: {rows:?}"
        );
        let min_copies_event = rows.iter().find(|(f, _, _)| f == "min_copies").unwrap();
        assert_eq!(min_copies_event.1.as_deref(), Some("2"));
        assert_eq!(min_copies_event.2, "5");

        let compression_event = rows.iter().find(|(f, _, _)| f == "compression").unwrap();
        assert_eq!(compression_event.1.as_deref(), Some("none"));
        assert_eq!(compression_event.2, "lzma");
    }

    /// Issue #48 item 5: the 8 field UPDATEs used to autocommit
    /// independently — a failure partway through could leave some fields
    /// changed and others not. No column in `archive_sets` has a CHECK
    /// constraint to violate naturally (confirmed against
    /// `db/migrations/001_initial.sql`), so this injects a deterministic
    /// failure via a test-local trigger — the same "injectable failure,
    /// not real fault injection" spirit as `staging::tests`' `FlakyReader`
    /// — that fires only on a sentinel value no real `Edit` invocation
    /// would ever send, scoped to this test's own in-memory connection.
    #[test]
    fn edit_rolls_back_every_field_when_one_update_fails_partway_through() {
        let conn = fresh_conn();
        let config = Config::default();
        run(&conn, &config, &create_cmd("cold", Some(2), None), false).unwrap();
        conn.execute(
            "UPDATE archive_sets SET checksum_mode = 'mtime_size' WHERE name = 'cold'",
            [],
        )
        .unwrap();

        conn.execute_batch(
            "CREATE TRIGGER reject_sentinel_checksum_mode
             BEFORE UPDATE OF checksum_mode ON archive_sets
             WHEN NEW.checksum_mode = 'REJECT_ME_TEST_SENTINEL'
             BEGIN SELECT RAISE(FAIL, 'test-injected failure'); END;",
        )
        .unwrap();

        // min_copies is updated first (field order in `Edit`) and would
        // succeed on its own; checksum_mode is updated later in the SAME
        // transaction and is the poisoned one. If the transaction is truly
        // atomic, min_copies must come back unchanged after Edit errors.
        let result = run(
            &conn,
            &config,
            &ArchiveSetCommands::Edit {
                name: "cold".to_string(),
                min_copies: Some(99),
                required_locations: None,
                encrypt: None,
                compression: None,
                checksum_mode: Some("REJECT_ME_TEST_SENTINEL".to_string()),
                slice_size: None,
                verify_interval_days: None,
                description: None,
            },
            false,
        );
        assert!(
            result.is_err(),
            "the poisoned update must surface as an error"
        );

        let (min_copies, checksum_mode): (i64, String) = conn
            .query_row(
                "SELECT min_copies, checksum_mode FROM archive_sets WHERE name = 'cold'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            min_copies, 2,
            "min_copies must be rolled back to its pre-edit value — it must NOT \
             be left at 99 even though its own UPDATE succeeded before the \
             later checksum_mode UPDATE failed in the same transaction"
        );
        assert_eq!(
            checksum_mode, "mtime_size",
            "checksum_mode must be unchanged"
        );

        let event_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE entity_type = 'archive_set' AND action = 'edited'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            event_count, 0,
            "no per-field event should survive a rolled-back edit"
        );
    }
}
