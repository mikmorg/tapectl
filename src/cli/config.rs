//! `tapectl config` command bodies (issue #112).
//!
//! Moved out of `main.rs` for the same reason as [`crate::cli::db`]:
//! integration tests import `tapectl::` and cannot reach the binary, so
//! anything inlined in `main.rs` is untestable from there.
//!
//! Every scan below is ADVISORY — it advises, never rewrites operator-owned
//! files, and never touches the exit code. That contract is load-bearing
//! (ADR-0004 and the #50/#92 "surface, do not delete" precedent); preserve it.

use rusqlite::Connection;

use crate::cli::ConfigCommands;
use crate::config::TapectlPaths;
use crate::error::Result;

pub fn run(
    conn: &Connection,
    paths: &TapectlPaths,
    command: &ConfigCommands,
    json_output: bool,
) -> Result<()> {
    match command {
        ConfigCommands::Show => {
            let toml_str = std::fs::read_to_string(&paths.config_file)?;
            if json_output {
                let val: toml::Value = toml_str
                    .parse()
                    .unwrap_or(toml::Value::String(toml_str.clone()));
                println!("{}", serde_json::to_string_pretty(&val).unwrap());
            } else {
                print!("{toml_str}");
            }
        }
        ConfigCommands::Check => {
            let toml_str = std::fs::read_to_string(&paths.config_file)?;

            // Advisory scan for pre-existing dotfiles that still shadow
            // an archive_set's policy fields (Recast of v4.0 §2.2,
            // docs/design-errata.md, issue #92). Never affects the
            // exit code and never rewrites anything — the operator
            // owns these files.
            let shadowing_hits = if paths.db_file.exists() {
                crate::policy::shadowing::scan(conn)
            } else {
                Vec::new()
            };

            match toml_str.parse::<toml::Value>() {
                Ok(_) => {
                    let loaded = crate::config::Config::load(&paths.config_file)?;

                    // Advisory scan for `preserve_acls = false`, which
                    // cannot take effect: dar has no independent ACL
                    // switch, so ACLs ride EAs (CTO decision
                    // 2026-07-31, issue #50; docs/design-errata.md).
                    // Same contract as the shadowing scan above —
                    // advises, rewrites nothing, never changes the
                    // exit code.
                    let subsumed_hits = if paths.db_file.exists() {
                        crate::policy::subsumed::scan(&loaded, conn)
                    } else {
                        crate::policy::subsumed::scan(
                            &loaded,
                            &rusqlite::Connection::open_in_memory()?,
                        )
                    };
                    // Decorative-key advisory (issue #62, #92/#50
                    // precedent). Originally: keys that are parsed but have
                    // no reader yet get surfaced, not deleted. Spec W4 and
                    // issue #172 both found that stance does not survive
                    // contact with a config that has never been used in
                    // production — see `policy::decorative`'s module doc for
                    // why every key it was built for is now deleted from
                    // `Config` instead.
                    let decorative_hits = crate::policy::decorative::scan(&loaded);

                    // Unknown-key advisory (issue #129). Serde silently drops
                    // fields no struct declares, so a plausible setting can sit
                    // in a config for years doing nothing while `config check`
                    // says "valid". Needs the raw text — after parsing, the
                    // evidence is gone.
                    let unknown_key_hits = std::fs::read_to_string(&paths.config_file)
                        .map(|t| crate::policy::unknown_keys::scan(&t))
                        .unwrap_or_default();

                    // Advisory scan (issue #97): a pre-existing
                    // archive_sets row whose compression the local dar
                    // cannot perform — validation only runs at write
                    // time, so a row written before that guard (or
                    // against a different dar build) can still be
                    // sitting there. Never rewrites, never touches the
                    // exit code.
                    let unsupported_compression_hits = if paths.db_file.exists() {
                        crate::policy::compression_capability::scan(conn, &loaded.dar.binary)
                    } else {
                        Vec::new()
                    };

                    // Depth checks (issue #62): does the config
                    // actually work, not just parse? Warnings only —
                    // never touches the exit code. Never opens a tape
                    // device — existence checks only.
                    let dar_check = crate::policy::depth_check::check_dar(&loaded.dar.binary);
                    let staging_check =
                        crate::policy::depth_check::check_staging(&loaded.staging.directory);
                    // Issue #140: the third staging check — is it big enough
                    // for one cartridge? Advisory like the other two, and
                    // silent when no drive is configured.
                    let staging_space_check =
                        crate::policy::depth_check::check_staging_space(&loaded);
                    let tape_device_checks = crate::policy::depth_check::scan_tape_devices(&loaded);

                    // ADR-0010: `capacity_override` exists for virtual
                    // drives (mhvtl) and test harnesses only — a real drive
                    // should let capacity follow the loaded cartridge's
                    // detected generation. Advisory only, like every other
                    // scan here: never touches the exit code.
                    let capacity_override_hits: Vec<&str> = loaded
                        .backends
                        .lto
                        .iter()
                        .filter(|b| b.capacity_override.is_some())
                        .map(|b| b.name.as_str())
                        .collect();

                    if json_output {
                        let shadowing_json: Vec<_> = shadowing_hits
                            .iter()
                            .map(|h| {
                                serde_json::json!({
                                    "unit": h.unit_name,
                                    "dotfile_path": h.dotfile_path.display().to_string(),
                                    "checksum_mode_set": h.checksum_mode_set,
                                    "compression_set": h.compression_set,
                                })
                            })
                            .collect();
                        let subsumed_json: Vec<_> = subsumed_hits
                            .iter()
                            .map(|h| {
                                serde_json::json!({
                                    "source": h.source,
                                    "field": "preserve_acls",
                                    "note": crate::policy::subsumed::describe(h),
                                })
                            })
                            .collect();
                        let unknown_key_json: Vec<_> = unknown_key_hits
                            .iter()
                            .map(|h| {
                                serde_json::json!({
                                    "key": h.key,
                                    "note": crate::policy::unknown_keys::describe(h),
                                })
                            })
                            .collect();
                        let decorative_json: Vec<_> = decorative_hits
                            .iter()
                            .map(|h| {
                                serde_json::json!({
                                    "key": h.key,
                                    "note": crate::policy::decorative::describe(h),
                                })
                            })
                            .collect();
                        let dar_json = match &dar_check {
                            crate::policy::depth_check::DarCheck::Missing { path } => {
                                serde_json::json!({"status": "missing", "path": path})
                            }
                            crate::policy::depth_check::DarCheck::NotExecutable { path } => {
                                serde_json::json!({"status": "not_executable", "path": path})
                            }
                            crate::policy::depth_check::DarCheck::Unreadable { path, detail } => {
                                serde_json::json!({"status": "unreadable", "path": path, "detail": detail})
                            }
                            crate::policy::depth_check::DarCheck::TooOld {
                                path,
                                found,
                                minimum,
                            } => {
                                serde_json::json!({"status": "too_old", "path": path, "found": found, "minimum": minimum})
                            }
                            crate::policy::depth_check::DarCheck::Ok { path, version } => {
                                serde_json::json!({"status": "ok", "path": path, "version": version})
                            }
                        };
                        let staging_json = match &staging_check {
                            crate::policy::depth_check::StagingCheck::Missing { path } => {
                                serde_json::json!({"status": "missing", "path": path})
                            }
                            crate::policy::depth_check::StagingCheck::NotWritable {
                                path,
                                detail,
                            } => {
                                serde_json::json!({"status": "not_writable", "path": path, "detail": detail})
                            }
                            crate::policy::depth_check::StagingCheck::Writable { path } => {
                                serde_json::json!({"status": "writable", "path": path})
                            }
                        };
                        let staging_space_json = match &staging_space_check {
                            crate::policy::depth_check::StagingSpaceCheck::Unknown => {
                                serde_json::Value::Null
                            }
                            crate::policy::depth_check::StagingSpaceCheck::Sufficient {
                                path,
                                free_bytes,
                                tape_bytes,
                            } => serde_json::json!({
                                "status": "sufficient", "path": path,
                                "free_bytes": free_bytes, "tape_bytes": tape_bytes,
                            }),
                            crate::policy::depth_check::StagingSpaceCheck::Tight {
                                path,
                                free_bytes,
                                tape_bytes,
                                generation,
                            } => serde_json::json!({
                                "status": "tight", "path": path,
                                "free_bytes": free_bytes, "tape_bytes": tape_bytes,
                                "generation": generation,
                            }),
                        };
                        let tape_devices_json: Vec<_> = tape_device_checks
                            .iter()
                            .map(|c| {
                                serde_json::json!({
                                    "backend": c.backend_name,
                                    "device_tape": c.device_tape,
                                    "device_tape_exists": c.device_tape_exists,
                                    "device_sg": c.device_sg,
                                    "device_sg_exists": c.device_sg_exists,
                                })
                            })
                            .collect();
                        let unsupported_compression_json: Vec<_> = unsupported_compression_hits
                            .iter()
                            .map(|h| {
                                serde_json::json!({
                                    "archive_set": h.archive_set_name,
                                    "compression": h.compression,
                                    "note": crate::policy::compression_capability::describe(h),
                                })
                            })
                            .collect();
                        println!(
                            "{}",
                            serde_json::json!({
                                "valid": true,
                                "shadowing_dotfiles": shadowing_json,
                                "subsumed_policy_fields": subsumed_json,
                                "decorative_keys": decorative_json,
                                "unknown_keys": unknown_key_json,
                                "dar": dar_json,
                                "staging": staging_json,
                                "staging_space": staging_space_json,
                                "tape_devices": tape_devices_json,
                                "unsupported_compression": unsupported_compression_json,
                                "capacity_override_backends": capacity_override_hits,
                            })
                        );
                    } else {
                        println!("config: valid");
                        for hit in &shadowing_hits {
                            let mut fields = Vec::new();
                            if hit.checksum_mode_set {
                                fields.push("checksum_mode");
                            }
                            if hit.compression_set {
                                fields.push("compression");
                            }
                            println!(
                                "warning: unit '{}' dotfile sets [policy] {} — this overrides its archive set ({})",
                                hit.unit_name,
                                fields.join(", "),
                                hit.dotfile_path.display()
                            );
                        }
                        if !shadowing_hits.is_empty() {
                            println!(
                                "  hint: remove the shadowing key(s) from each dotfile's [policy] table to defer to the archive set"
                            );
                        }
                        for hit in &subsumed_hits {
                            println!("{}", crate::policy::subsumed::describe(hit));
                        }
                        println!("{}", crate::policy::depth_check::describe_dar(&dar_check));
                        println!(
                            "{}",
                            crate::policy::depth_check::describe_staging(&staging_check)
                        );
                        if let Some(line) =
                            crate::policy::depth_check::describe_staging_space(&staging_space_check)
                        {
                            println!("{line}");
                        }
                        for check in &tape_device_checks {
                            println!(
                                "{}",
                                crate::policy::depth_check::describe_tape_device(check)
                            );
                        }
                        for hit in &unknown_key_hits {
                            println!("{}", crate::policy::unknown_keys::describe(hit));
                        }
                        for hit in &decorative_hits {
                            println!("{}", crate::policy::decorative::describe(hit));
                        }
                        for hit in &unsupported_compression_hits {
                            println!("{}", crate::policy::compression_capability::describe(hit));
                        }
                        for name in &capacity_override_hits {
                            println!(
                                "warning: backend \"{name}\" sets capacity_override — intended \
                                 for virtual drives (mhvtl) only; a real drive's capacity should \
                                 come from the loaded cartridge's detected generation (ADR-0010)."
                            );
                        }
                    }
                }
                Err(e) => {
                    if json_output {
                        println!(
                            "{}",
                            serde_json::json!({"valid": false, "error": e.to_string()})
                        );
                    } else {
                        println!("config: INVALID — {e}");
                    }
                }
            }
        }
    }
    Ok(())
}
