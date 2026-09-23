//! `tapectl config` command bodies (issue #112).
//!
//! Moved out of `main.rs` for the same reason as [`crate::cli::db`]:
//! integration tests import `tapectl::` and cannot reach the binary, so
//! anything inlined in `main.rs` is untestable from there.
//!
//! Every scan below (shadowing, subsumed, decorative keys, unknown
//! `[defaults]` keys, unsupported compression, and the dar/staging/tape
//! depth checks) is ADVISORY — it advises, never rewrites operator-owned
//! files, and never touches the exit code. That contract is load-bearing
//! (ADR-0004 and the #50/#92 "surface, do not delete" precedent); preserve
//! it.
//!
//! `Check`'s own exit code is a SEPARATE thing from that contract (issue
//! #173): it mirrors `Config::load`'s verdict exactly — 0 when the config
//! loads, [`crate::error::EXIT_ERROR`] with the full problem list otherwise
//! — because "does this config load at all" is not a policy opinion the way
//! the scans above are, it is the same fact every other command's startup
//! already treats as fatal. `Check` is the one command allowed to survive
//! that fact long enough to report it in full; see
//! [`crate::policy::lenient_config`] for how.

use rusqlite::Connection;

use crate::cli::ConfigCommands;
use crate::config::TapectlPaths;
use crate::error::{Result, EXIT_ERROR, EXIT_SUCCESS};

pub fn run(
    conn: &Connection,
    paths: &TapectlPaths,
    command: &ConfigCommands,
    json_output: bool,
) -> Result<i32> {
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
            Ok(EXIT_SUCCESS)
        }
        ConfigCommands::Check => run_check(conn, paths, json_output),
    }
}

/// `config check`'s body, split out of [`run`] only for readability —
/// `Check` is by far the larger of the two arms.
///
/// Issue #173: this used to call `Config::load(...)?` up front, which meant
/// a config that could not strictly load never reached any of the code
/// below at all — `main.rs`'s own common dispatch path already loads the
/// config strictly before any subcommand runs, so the one command whose job
/// is diagnosing a broken config never saw one. `main.rs` now special-cases
/// `Commands::Config { command: ConfigCommands::Check }` to dispatch here
/// BEFORE that strict load (see its own comment; `config show` is left on
/// the normal path), and this function gets its own view of the config via
/// [`crate::policy::lenient_config::check`] instead of the strict loader.
fn run_check(conn: &Connection, paths: &TapectlPaths, json_output: bool) -> Result<i32> {
    let toml_str = std::fs::read_to_string(&paths.config_file)?;
    let report = crate::policy::lenient_config::check(&toml_str, &paths.config_file);
    // A best-effort `Config`, present whenever the lenient parse could
    // build one at all (always, when `report.valid`; sometimes, when it
    // isn't — e.g. an unknown key stripped cleanly but a bad `compression`
    // value remains). Every advisory scan below that needs a `Config`
    // simply has nothing to run against when this is `None`; `report.problems`
    // already says why.
    let loaded = report.best_effort.as_ref();

    // Advisory scan for pre-existing dotfiles that still shadow an
    // archive_set's policy fields (Recast of v4.0 §2.2,
    // docs/design-errata.md, issue #92). Needs only the database, not a
    // loaded Config, so it always runs.
    let shadowing_hits = if paths.db_file.exists() {
        crate::policy::shadowing::scan(conn)
    } else {
        Vec::new()
    };

    // Dotfiles that cannot be READ (issue #211's residual). Since #211 an
    // unrecognised `[policy]` key is a hard `PolicyUnresolvable` for `audit`,
    // `stage create` and `unit status` — so `config check`, the command run
    // precisely to find bad configuration, must not be the one place that
    // stays silent about it. Advisory: reported, never an exit-code change.
    let unreadable_dotfiles = if paths.db_file.exists() {
        crate::policy::shadowing::scan_unreadable(conn)
    } else {
        Vec::new()
    };

    // Unknown-key advisory (issue #129), scoped to `[defaults]`. Reads the
    // raw text directly, so it too always runs regardless of whether the
    // rest of the file loads. Issue #173: with every section now denying
    // unknown fields (#171), this can only ever fire on a config that ALSO
    // fails to strictly load — at which point `report.problems` already
    // names the same key via its own "unknown key: …" entries. This scan is
    // no longer reachable on a config that loads cleanly; kept for its
    // `[defaults]`-specific remediation text (e.g. `min_copies`'s "the real
    // knob is …") which `report.problems`'s generic message does not carry.
    let unknown_key_hits = crate::policy::unknown_keys::scan(&toml_str);

    // Advisory scan (issue #50/#92 precedent): `preserve_acls = false`
    // cannot take effect (dar has no independent ACL switch; ACLs ride EAs).
    let subsumed_hits = match loaded {
        Some(cfg) if paths.db_file.exists() => crate::policy::subsumed::scan(cfg, conn),
        Some(cfg) => crate::policy::subsumed::scan(cfg, &rusqlite::Connection::open_in_memory()?),
        None => Vec::new(),
    };

    // Decorative-key advisory (issue #62, #92/#50 precedent).
    let decorative_hits = loaded
        .map(crate::policy::decorative::scan)
        .unwrap_or_default();

    // Advisory scan (issue #97): a pre-existing archive_sets row whose
    // compression the local dar cannot perform.
    let unsupported_compression_hits = match loaded {
        Some(cfg) if paths.db_file.exists() => {
            crate::policy::compression_capability::scan(conn, &cfg.dar.binary)
        }
        _ => Vec::new(),
    };

    // Depth checks (issue #62): does the config actually work, not just
    // parse? Warnings only — never touch the exit code, never open a tape
    // device.
    let dar_check = loaded.map(|cfg| crate::policy::depth_check::check_dar(&cfg.dar.binary));
    let staging_check =
        loaded.map(|cfg| crate::policy::depth_check::check_staging(&cfg.staging.directory));
    // Issue #140: the third staging check — is it big enough for one
    // cartridge?
    let staging_space_check = loaded.map(crate::policy::depth_check::check_staging_space);
    let tape_device_checks = loaded
        .map(crate::policy::depth_check::scan_tape_devices)
        .unwrap_or_default();

    // ADR-0010: `capacity_override` exists for virtual drives (mhvtl) and
    // test harnesses only.
    let capacity_override_hits: Vec<&str> = loaded
        .map(|cfg| {
            cfg.backends
                .lto
                .iter()
                .filter(|b| b.capacity_override.is_some())
                .map(|b| b.name.as_str())
                .collect()
        })
        .unwrap_or_default();

    if json_output {
        print_json(
            &report,
            &shadowing_hits,
            &subsumed_hits,
            &decorative_hits,
            &unknown_key_hits,
            dar_check.as_ref(),
            staging_check.as_ref(),
            staging_space_check.as_ref(),
            &tape_device_checks,
            &unsupported_compression_hits,
            &capacity_override_hits,
        );
    } else {
        print_human(
            &report,
            &shadowing_hits,
            &unreadable_dotfiles,
            &subsumed_hits,
            &decorative_hits,
            &unknown_key_hits,
            dar_check.as_ref(),
            staging_check.as_ref(),
            staging_space_check.as_ref(),
            &tape_device_checks,
            &unsupported_compression_hits,
            &capacity_override_hits,
        );
    }

    Ok(if report.valid {
        EXIT_SUCCESS
    } else {
        EXIT_ERROR
    })
}

#[allow(clippy::too_many_arguments)]
fn print_json(
    report: &crate::policy::lenient_config::LenientReport,
    shadowing_hits: &[crate::policy::shadowing::ShadowingDotfile],
    subsumed_hits: &[crate::policy::subsumed::SubsumedAcls],
    decorative_hits: &[crate::policy::decorative::DecorativeHit],
    unknown_key_hits: &[crate::policy::unknown_keys::UnknownKeyHit],
    dar_check: Option<&crate::policy::depth_check::DarCheck>,
    staging_check: Option<&crate::policy::depth_check::StagingCheck>,
    staging_space_check: Option<&crate::policy::depth_check::StagingSpaceCheck>,
    tape_device_checks: &[crate::policy::depth_check::TapeDeviceCheck],
    unsupported_compression_hits: &[crate::policy::compression_capability::UnsupportedCompressionHit],
    capacity_override_hits: &[&str],
) {
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
    let dar_json = match dar_check {
        None => serde_json::Value::Null,
        Some(crate::policy::depth_check::DarCheck::Missing { path }) => {
            serde_json::json!({"status": "missing", "path": path})
        }
        Some(crate::policy::depth_check::DarCheck::NotExecutable { path }) => {
            serde_json::json!({"status": "not_executable", "path": path})
        }
        Some(crate::policy::depth_check::DarCheck::Unreadable { path, detail }) => {
            serde_json::json!({"status": "unreadable", "path": path, "detail": detail})
        }
        Some(crate::policy::depth_check::DarCheck::TooOld {
            path,
            found,
            minimum,
        }) => {
            serde_json::json!({"status": "too_old", "path": path, "found": found, "minimum": minimum})
        }
        Some(crate::policy::depth_check::DarCheck::Ok { path, version }) => {
            serde_json::json!({"status": "ok", "path": path, "version": version})
        }
    };
    let staging_json = match staging_check {
        None => serde_json::Value::Null,
        Some(crate::policy::depth_check::StagingCheck::Missing { path }) => {
            serde_json::json!({"status": "missing", "path": path})
        }
        Some(crate::policy::depth_check::StagingCheck::NotWritable { path, detail }) => {
            serde_json::json!({"status": "not_writable", "path": path, "detail": detail})
        }
        Some(crate::policy::depth_check::StagingCheck::Writable { path }) => {
            serde_json::json!({"status": "writable", "path": path})
        }
    };
    let staging_space_json = match staging_space_check {
        None | Some(crate::policy::depth_check::StagingSpaceCheck::Unknown) => {
            serde_json::Value::Null
        }
        Some(crate::policy::depth_check::StagingSpaceCheck::Sufficient {
            path,
            free_bytes,
            tape_bytes,
        }) => serde_json::json!({
            "status": "sufficient", "path": path,
            "free_bytes": free_bytes, "tape_bytes": tape_bytes,
        }),
        Some(crate::policy::depth_check::StagingSpaceCheck::Tight {
            path,
            free_bytes,
            tape_bytes,
            generation,
        }) => serde_json::json!({
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
                "device_sg_pairing_problem": c.pairing_problem,
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
            "valid": report.valid,
            "problems": report.problems,
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
}

#[allow(clippy::too_many_arguments)]
fn print_human(
    report: &crate::policy::lenient_config::LenientReport,
    shadowing_hits: &[crate::policy::shadowing::ShadowingDotfile],
    unreadable_dotfiles: &[crate::policy::shadowing::UnreadableDotfile],
    subsumed_hits: &[crate::policy::subsumed::SubsumedAcls],
    decorative_hits: &[crate::policy::decorative::DecorativeHit],
    unknown_key_hits: &[crate::policy::unknown_keys::UnknownKeyHit],
    dar_check: Option<&crate::policy::depth_check::DarCheck>,
    staging_check: Option<&crate::policy::depth_check::StagingCheck>,
    staging_space_check: Option<&crate::policy::depth_check::StagingSpaceCheck>,
    tape_device_checks: &[crate::policy::depth_check::TapeDeviceCheck],
    unsupported_compression_hits: &[crate::policy::compression_capability::UnsupportedCompressionHit],
    capacity_override_hits: &[&str],
) {
    if report.valid {
        println!("config: valid");
    } else {
        println!("config: INVALID");
        for problem in &report.problems {
            println!("  - {problem}");
        }
    }
    for hit in shadowing_hits {
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
    for hit in unreadable_dotfiles {
        println!(
            "warning: unit '{}' dotfile cannot be read, so every command that resolves \
             policy for this unit will refuse: {} ({})",
            hit.unit_name,
            hit.reason,
            hit.dotfile_path.display()
        );
    }
    if !unreadable_dotfiles.is_empty() {
        println!(
            "  hint: an unrecognised key under [policy] is refused by name (issue #211); \
             an ABSENT key is always fine and defers to the archive set or defaults"
        );
    }
    for hit in subsumed_hits {
        println!("{}", crate::policy::subsumed::describe(hit));
    }
    match dar_check {
        Some(check) => println!("{}", crate::policy::depth_check::describe_dar(check)),
        None => println!("note: dar could not be checked — the config did not parse well enough"),
    }
    match staging_check {
        Some(check) => println!("{}", crate::policy::depth_check::describe_staging(check)),
        None => {
            println!("note: staging could not be checked — the config did not parse well enough")
        }
    }
    if let Some(check) = staging_space_check {
        if let Some(line) = crate::policy::depth_check::describe_staging_space(check) {
            println!("{line}");
        }
    }
    for check in tape_device_checks {
        println!(
            "{}",
            crate::policy::depth_check::describe_tape_device(check)
        );
    }
    for hit in unknown_key_hits {
        println!("{}", crate::policy::unknown_keys::describe(hit));
    }
    for hit in decorative_hits {
        println!("{}", crate::policy::decorative::describe(hit));
    }
    for hit in unsupported_compression_hits {
        println!("{}", crate::policy::compression_capability::describe(hit));
    }
    for name in capacity_override_hits {
        println!(
            "warning: backend \"{name}\" sets capacity_override — intended \
             for virtual drives (mhvtl) only; a real drive's capacity should \
             come from the loaded cartridge's detected generation (ADR-0010)."
        );
    }
}
