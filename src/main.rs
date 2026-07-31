use tapectl::{cli, config, db, error, policy, signal, staging, tenant, unit, volume};

use anyhow::{bail, Context};
use clap::Parser;

use cli::{Cli, Commands};
use config::{Config, TapectlPaths};

fn main() {
    let cli = Cli::parse();

    init_tracing(cli.verbose);
    signal::install_handler();

    if let Err(err) = run(cli) {
        error::exit_with_error(&err);
    }
}

/// Install the global tracing subscriber (issue #45/H10 — closes the "no
/// sink" gap: `grep -rn tracing_subscriber` returned zero hits before this,
/// so every `tracing::warn!`/`info!` call in the codebase, including #32's
/// and #36's, went nowhere).
///
/// Writes to **stderr**, never stdout: every command's `--json` mode prints
/// machine-readable output to stdout, and `scripts/mhvtl-verify-gate.sh`
/// pipes `volume verify --json` straight into `tee` + a JSON parser under
/// `pipefail` — a log line on stdout would corrupt that stream for every
/// consumer.
///
/// Default level is WARN, so the existing warn!-only fixes surface without
/// burying a command's own output; `--verbose` raises it to DEBUG (closes
/// #3's "`--verbose` parsed but ignored").
///
/// Non-fatal by design: `try_init` (not `init`) so a failed or repeated
/// install — e.g. this binary embedded in a future test harness that
/// already set a subscriber — is silently ignored rather than panicking a
/// command that would otherwise work fine.
fn init_tracing(verbose: bool) {
    let level = if verbose {
        tracing::Level::DEBUG
    } else {
        tracing::Level::WARN
    };
    let _ = tracing_subscriber::fmt()
        .with_max_level(level)
        .with_writer(std::io::stderr)
        .try_init();
}

/// Flush stdout, then exit with `code` if it is non-zero (issue #45/H10).
/// A code of 0 is a no-op — the healthy/clean path returns normally rather
/// than calling `process::exit(0)`. The explicit flush guards against
/// `println!`'s buffered output being dropped when stdout is a pipe (exactly
/// how `scripts/mhvtl-verify-gate.sh` invokes this binary, under
/// `pipefail`, so a truncated line is a real failure mode).
fn exit_if_nonzero(code: i32) {
    if code > 0 {
        use std::io::Write;
        let _ = std::io::stdout().flush();
        std::process::exit(code);
    }
}

/// Decide the process exit code for `db fsck` from its report (issue
/// #45/H10). Mirrors the `audit` convention: 0=clean, 1=warning,
/// 2=violation.
///
/// - The integrity check itself failing is a violation, full stop — no
///   amount of orphan-row repair changes that.
/// - Any other issue (e.g. orphaned rows) is a warning regardless of
///   whether `--repair` fixed it: an unrepaired finding (fsck run without
///   `--repair`) is still a finding nobody should see reported as a clean
///   0 — that is exactly the "reports success while finding problems" bug
///   this ticket exists to kill. It just isn't tape/DB corruption, so it's
///   1, not 2.
/// - No issues at all is genuinely clean.
fn fsck_exit_code(report: &cli::operations::FsckReport) -> i32 {
    if !report.integrity_ok {
        error::EXIT_ERROR
    } else if !report.issues.is_empty() {
        error::EXIT_WARNING
    } else {
        error::EXIT_SUCCESS
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    // Resolve paths
    let paths = if let Some(ref config_path) = cli.config {
        // If --config is given, derive home from the config file's parent
        let config_file = std::path::Path::new(config_path);
        let home = config_file
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .to_path_buf();
        TapectlPaths::new(home)
    } else {
        TapectlPaths::default_paths()
    };

    // Init is special — it creates everything from scratch
    if let Commands::Init { ref operator } = cli.command {
        return cmd_init(&paths, operator.as_deref(), cli.json);
    }

    // Completions don't need DB
    if let Commands::Completions { shell } = cli.command {
        let mut cmd = <Cli as clap::CommandFactory>::command();
        clap_complete::generate(shell, &mut cmd, "tapectl", &mut std::io::stdout());
        return Ok(());
    }

    // Everything else requires initialization
    if !paths.is_initialized() {
        bail!("tapectl is not initialized — run `tapectl init` first");
    }

    // Issue #41: `ensure_dirs` tightens `~/.tapectl`'s directory tree to
    // 0700 (idempotent, warn-not-fail internally — see its doc comment).
    // `cmd_init` below already calls it for a brand-new home, but that
    // alone would only ever protect installs created *after* this fix.
    // Calling it here too is what makes the tightening reach an
    // already-initialized `~/.tapectl` on ordinary use, not just a fresh
    // `tapectl init`.
    paths
        .ensure_dirs()
        .context("failed to secure tapectl home directories")?;

    let cfg = Config::load(&paths.config_file).context("failed to load config")?;
    let conn = db::open(&paths.db_file).context("failed to open database")?;

    match cli.command {
        Commands::Tenant { ref command } => {
            cli::tenant::run(&conn, &paths, command, cli.json)?;
        }
        Commands::Key { ref command } => {
            cli::key::run(&conn, &paths, command, cli.json)?;
        }
        Commands::Unit { ref command } => {
            cli::unit::run(&conn, &paths, &cfg, command, cli.json)?;
        }
        Commands::Collection { ref command } => {
            cli::collection::run(&conn, &paths, &cfg, command, cli.json)?;
        }
        Commands::Snapshot { ref command } => {
            cli::snapshot::run(&conn, &paths, &cfg, command, cli.json)?;
        }
        Commands::Stage { ref command } => {
            cli::stage::run(&conn, &paths, &cfg, command, cli.json)?;
        }
        Commands::Staging { ref command } => {
            cli::staging::run(&conn, &cfg, command, cli.json)?;
        }
        Commands::Volume { ref command } => {
            // issue #45/H10: `volume::run` now returns a process exit code
            // (0=clean, 1=warning, 2=violation) for `Verify`; every other
            // subcommand returns EXIT_SUCCESS. Mirrors the Audit arm below.
            let exit_code =
                cli::volume::run(&conn, &paths, &cfg, command, cli.json, cli.yes, cli.dry_run)?;
            exit_if_nonzero(exit_code);
        }
        Commands::Restore { ref command } => {
            cli::restore::run(&conn, &paths, &cfg, command, cli.json)?;
        }
        Commands::Catalog { ref command } => {
            cli::catalog::run(&conn, command, cli.json)?;
        }
        Commands::Location { ref command } => {
            cli::location::run(&conn, command, cli.json)?;
        }
        Commands::Cartridge { ref command } => {
            cli::cartridge::run(&conn, command, cli.json, cli.yes, cli.dry_run)?;
        }
        Commands::ArchiveSet { ref command } => {
            cli::archive_set::run(&conn, &cfg, command, cli.json)?;
        }
        Commands::Audit {
            action_plan,
            ref unit,
        } => {
            let exit_code = cli::audit::run(&conn, &cfg, unit.as_deref(), action_plan, cli.json)?;
            if exit_code > 0 {
                std::process::exit(exit_code);
            }
        }
        Commands::Report { ref command } => {
            cli::report::run(&conn, &cfg, command, cli.json)?;
        }
        Commands::Export { ref unit, ref to } => {
            cli::operations::export_unit(&conn, unit, to, cli.json)?;
        }
        Commands::Import {
            ref label,
            ref backend,
            ref media_type,
            ref capacity,
            ref notes,
        } => {
            let cap_bytes = crate::staging::parse_size_to_bytes(capacity)?;
            // Resolve backend_name from configured backend of this type, else fall back
            // to the type string so the row remains self-consistent.
            let backend_name = match backend.as_str() {
                "lto" => cfg
                    .backends
                    .lto
                    .first()
                    .map(|b| b.name.clone())
                    .unwrap_or_else(|| backend.clone()),
                _ => backend.clone(),
            };
            conn.execute(
                "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes, status, notes)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'active', ?6)",
                rusqlite::params![label, backend, backend_name, media_type, cap_bytes, notes],
            )?;
            let vol_id = conn.last_insert_rowid();
            crate::db::events::log_created(&conn, "volume", vol_id, label, None)?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({"id": vol_id, "label": label, "status": "imported"})
                );
            } else {
                println!("volume \"{label}\" imported (id={vol_id}, {media_type}, {capacity})");
            }
        }
        Commands::QuickArchive {
            ref path,
            ref tenant,
            ref volume,
            ref tag,
            ref device,
        } => {
            // Step 1: init unit
            let unit_id = crate::unit::init_unit(&conn, &paths, path, tenant, None, tag, None)?;
            let unit_name: String = conn.query_row(
                "SELECT name FROM units WHERE id = ?1",
                rusqlite::params![unit_id],
                |row| row.get(0),
            )?;
            println!("unit \"{unit_name}\" initialized");
            // Step 2: snapshot
            let snap_id = crate::staging::snapshot_create(&conn, &unit_name, &cfg)?;
            println!("snapshot created (id={snap_id})");
            // Step 3: stage
            let ss_id = crate::staging::stage_create(&conn, &paths, &cfg, snap_id)?;
            println!("staged (stage_set={ss_id})");
            // Step 4: write
            // force=false: quick-archive writes to a caller-provided volume
            // label with no override surface of its own (issue #27 scopes
            // --force to `volume init`/`volume write` only).
            crate::volume::write::volume_write(
                &conn,
                &paths,
                &cfg,
                volume,
                device,
                512 * 1024,
                false,
            )?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({"unit": unit_name, "volume": volume, "status": "completed"})
                );
            } else {
                println!("quick-archive complete: \"{unit_name}\" written to \"{volume}\"");
            }
        }
        Commands::Db { ref command } => match command {
            cli::DbCommands::Backup { to, include_keys } => {
                cli::operations::db_backup(&paths, to, *include_keys)?;
                if cli.json {
                    println!(
                        "{}",
                        serde_json::json!({"backup": to, "keys_included": include_keys})
                    );
                } else if *include_keys {
                    println!("database and keys backed up to {to}");
                } else {
                    println!(
                        "database backed up to {to} (private keys not included — pass --include-keys to copy them)"
                    );
                }
            }
            cli::DbCommands::Fsck { repair } => {
                let report = cli::operations::db_fsck(&conn, *repair)?;
                if cli.json {
                    println!(
                        "{}",
                        serde_json::json!({"integrity_ok": report.integrity_ok, "issues": report.issues, "repaired": report.repaired})
                    );
                } else {
                    println!(
                        "fsck: integrity={}, issues={}, repaired={}",
                        if report.integrity_ok { "ok" } else { "FAIL" },
                        report.issues.len(),
                        report.repaired,
                    );
                    for issue in &report.issues {
                        println!("  {issue}");
                    }
                }
                // issue #45/H10: fsck must not exit 0 when it found real
                // problems — see `fsck_exit_code` for the exact rule.
                exit_if_nonzero(fsck_exit_code(&report));
            }
            cli::DbCommands::Export => {
                // Full streaming JSON dump of the database (issue #61):
                // schema_version + every user table, enumerated from
                // sqlite_master at runtime. `--json` is a no-op here since
                // the output is unconditionally JSON. Nothing but the JSON
                // document itself may go to stdout on this path.
                let stdout = std::io::stdout();
                let mut writer = std::io::BufWriter::new(stdout.lock());
                db::export::export_json(&conn, &mut writer)?;
            }
            cli::DbCommands::Import { path: import_path } => {
                cli::operations::db_import(&paths, import_path, cli.yes, cli.dry_run, cli.json)?;
            }
            cli::DbCommands::Stats => {
                let page_count: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
                let page_size: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
                let db_size = page_count * page_size;
                let table_count: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table'",
                    [],
                    |r| r.get(0),
                )?;
                if cli.json {
                    println!(
                        "{}",
                        serde_json::json!({"size_bytes": db_size, "tables": table_count, "pages": page_count})
                    );
                } else {
                    println!(
                        "database: {} KB, {table_count} tables, {page_count} pages",
                        db_size / 1024
                    );
                }
            }
        },
        Commands::Config { ref command } => match command {
            cli::ConfigCommands::Show => {
                let toml_str = std::fs::read_to_string(&paths.config_file)?;
                if cli.json {
                    let val: toml::Value = toml_str
                        .parse()
                        .unwrap_or(toml::Value::String(toml_str.clone()));
                    println!("{}", serde_json::to_string_pretty(&val).unwrap());
                } else {
                    print!("{toml_str}");
                }
            }
            cli::ConfigCommands::Check => {
                let toml_str = std::fs::read_to_string(&paths.config_file)?;

                // Advisory scan for pre-existing dotfiles that still shadow
                // an archive_set's policy fields (Recast of v4.0 §2.2,
                // docs/design-errata.md, issue #92). Never affects the
                // exit code and never rewrites anything — the operator
                // owns these files.
                let shadowing_hits = if paths.db_file.exists() {
                    policy::shadowing::scan(&conn)
                } else {
                    Vec::new()
                };

                match toml_str.parse::<toml::Value>() {
                    Ok(_) => {
                        let loaded = config::Config::load(&paths.config_file)?;

                        // Advisory scan for `preserve_acls = false`, which
                        // cannot take effect: dar has no independent ACL
                        // switch, so ACLs ride EAs (CTO decision
                        // 2026-07-31, issue #50; docs/design-errata.md).
                        // Same contract as the shadowing scan above —
                        // advises, rewrites nothing, never changes the
                        // exit code.
                        let subsumed_hits = if paths.db_file.exists() {
                            policy::subsumed::scan(&loaded, &conn)
                        } else {
                            policy::subsumed::scan(
                                &loaded,
                                &rusqlite::Connection::open_in_memory()?,
                            )
                        };
                        // Decorative-key advisory (issue #62, #92/#50
                        // precedent): keys that are parsed but have no
                        // reader yet get surfaced, not deleted.
                        let decorative_hits = policy::decorative::scan(&loaded);

                        // Depth checks (issue #62): does the config
                        // actually work, not just parse? Warnings only —
                        // never touches the exit code. Never opens a tape
                        // device — existence checks only.
                        let dar_check = policy::depth_check::check_dar(&loaded.dar.binary);
                        let staging_check =
                            policy::depth_check::check_staging(&loaded.staging.directory);
                        let tape_device_checks = policy::depth_check::scan_tape_devices(&loaded);

                        if cli.json {
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
                                        "note": policy::subsumed::describe(h),
                                    })
                                })
                                .collect();
                            let decorative_json: Vec<_> = decorative_hits
                                .iter()
                                .map(|h| {
                                    serde_json::json!({
                                        "key": h.key,
                                        "note": policy::decorative::describe(h),
                                    })
                                })
                                .collect();
                            let dar_json = match &dar_check {
                                policy::depth_check::DarCheck::Missing { path } => {
                                    serde_json::json!({"status": "missing", "path": path})
                                }
                                policy::depth_check::DarCheck::NotExecutable { path } => {
                                    serde_json::json!({"status": "not_executable", "path": path})
                                }
                                policy::depth_check::DarCheck::Unreadable { path, detail } => {
                                    serde_json::json!({"status": "unreadable", "path": path, "detail": detail})
                                }
                                policy::depth_check::DarCheck::TooOld {
                                    path,
                                    found,
                                    minimum,
                                } => {
                                    serde_json::json!({"status": "too_old", "path": path, "found": found, "minimum": minimum})
                                }
                                policy::depth_check::DarCheck::Ok { path, version } => {
                                    serde_json::json!({"status": "ok", "path": path, "version": version})
                                }
                            };
                            let staging_json = match &staging_check {
                                policy::depth_check::StagingCheck::Missing { path } => {
                                    serde_json::json!({"status": "missing", "path": path})
                                }
                                policy::depth_check::StagingCheck::NotWritable { path, detail } => {
                                    serde_json::json!({"status": "not_writable", "path": path, "detail": detail})
                                }
                                policy::depth_check::StagingCheck::Writable { path } => {
                                    serde_json::json!({"status": "writable", "path": path})
                                }
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
                            println!(
                                "{}",
                                serde_json::json!({
                                    "valid": true,
                                    "shadowing_dotfiles": shadowing_json,
                                    "subsumed_policy_fields": subsumed_json,
                                    "decorative_keys": decorative_json,
                                    "dar": dar_json,
                                    "staging": staging_json,
                                    "tape_devices": tape_devices_json,
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
                                println!("{}", policy::subsumed::describe(hit));
                            }
                            println!("{}", policy::depth_check::describe_dar(&dar_check));
                            println!("{}", policy::depth_check::describe_staging(&staging_check));
                            for check in &tape_device_checks {
                                println!("{}", policy::depth_check::describe_tape_device(check));
                            }
                            for hit in &decorative_hits {
                                println!("{}", policy::decorative::describe(hit));
                            }
                        }
                    }
                    Err(e) => {
                        if cli.json {
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
        },
        Commands::Init { .. } | Commands::Completions { .. } => {
            unreachable!()
        }
    }

    Ok(())
}

/// `tapectl init` — bootstrap everything.
fn cmd_init(
    paths: &TapectlPaths,
    operator_name: Option<&str>,
    json_output: bool,
) -> anyhow::Result<()> {
    if paths.is_initialized() {
        bail!("tapectl is already initialized at {}", paths.home.display());
    }

    // Create directory structure
    paths.ensure_dirs()?;

    // Write default config
    let cfg = Config::default();
    cfg.save(&paths.config_file)?;

    // Create database with schema
    let conn = db::open(&paths.db_file).context("failed to create database")?;

    // Determine operator name
    let op_name = operator_name
        .map(String::from)
        .unwrap_or_else(|| std::env::var("USER").unwrap_or_else(|_| "operator".to_string()));

    // Create operator tenant with keypairs
    let tenant_id = tenant::add_tenant(&conn, paths, &op_name, Some("System operator"), true)?;

    // Validate dar availability (non-fatal warning)
    let dar_path = &cfg.dar.binary;
    let dar_ok = check_dar(dar_path);

    if json_output {
        println!(
            "{}",
            serde_json::json!({
                "home": paths.home.display().to_string(),
                "operator": op_name,
                "operator_id": tenant_id,
                "dar_available": dar_ok,
            })
        );
    } else {
        println!("tapectl initialized at {}", paths.home.display());
        println!("  operator: {op_name}");
        println!("  database: {}", paths.db_file.display());
        println!("  config:   {}", paths.config_file.display());
        if dar_ok {
            println!("  dar:      {dar_path} (ok)");
        } else {
            println!("  dar:      {dar_path} (NOT FOUND — install before staging)");
        }
    }

    Ok(())
}

/// Check if dar is available at the configured path.
fn check_dar(dar_path: &str) -> bool {
    std::process::Command::new(dar_path)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cli::operations::FsckReport;

    #[test]
    fn fsck_exit_code_clean_is_success() {
        let report = FsckReport {
            integrity_ok: true,
            issues: vec![],
            repaired: 0,
        };
        assert_eq!(fsck_exit_code(&report), error::EXIT_SUCCESS);
    }

    #[test]
    fn fsck_exit_code_broken_integrity_is_violation() {
        let report = FsckReport {
            integrity_ok: false,
            issues: vec!["integrity_check: corrupted".to_string()],
            repaired: 0,
        };
        assert_eq!(fsck_exit_code(&report), error::EXIT_ERROR);
    }

    #[test]
    fn fsck_exit_code_broken_integrity_is_violation_even_if_other_things_repaired() {
        // Integrity failure outranks repair count — repairing orphan rows
        // does not paper over a corrupted database.
        let report = FsckReport {
            integrity_ok: false,
            issues: vec!["integrity_check: corrupted".to_string(), "1 orphan".into()],
            repaired: 1,
        };
        assert_eq!(fsck_exit_code(&report), error::EXIT_ERROR);
    }

    #[test]
    fn fsck_exit_code_issues_found_and_repaired_is_warning() {
        let report = FsckReport {
            integrity_ok: true,
            issues: vec!["3 orphaned write records".to_string()],
            repaired: 1,
        };
        assert_eq!(fsck_exit_code(&report), error::EXIT_WARNING);
    }

    #[test]
    fn fsck_exit_code_issues_found_but_not_repaired_is_still_warning() {
        // Ran without --repair: nothing got fixed (repaired == 0), but the
        // finding is real and must not be swallowed as a clean 0.
        let report = FsckReport {
            integrity_ok: true,
            issues: vec!["3 orphaned write records".to_string()],
            repaired: 0,
        };
        assert_eq!(fsck_exit_code(&report), error::EXIT_WARNING);
    }
}
