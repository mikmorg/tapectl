//! `tapectl config` command bodies (issue #112).
//!
//! Moved out of `main.rs` for the same reason as [`crate::cli::db`]:
//! integration tests import `tapectl::` and cannot reach the binary, so
//! anything inlined in `main.rs` is untestable from there.
//!
//! `config set`/`add`/`remove` (issue #143) rewrite config.toml by design,
//! through [`crate::config_edit`], which checks the edited file before it
//! is written. Everything else here only reads.
//!
//! Every `config check` scan below (shadowing, subsumed, decorative keys,
//! unknown `[defaults]` keys, unsupported compression, and the
//! dar/staging/tape depth checks) is ADVISORY — it advises, never rewrites
//! operator-owned files, and never touches the exit code. That contract is load-bearing
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
//!
//! The one addition (issue #479): a config that LOADS but breaks the slice
//! cap (`slice_size` too large for `post_warning_bytes`) is reported as an
//! `error:` line and also exits non-zero, though every other command loads
//! and runs it. The check lives here, in `config check`'s own report path,
//! and in `config set` and `stage create`, never in `Config::load`: a
//! staging setting must not block a restore.

use rusqlite::Connection;

use crate::cli::ConfigCommands;
use crate::config::TapectlPaths;
use crate::error::{Result, EXIT_ERROR, EXIT_SUCCESS};

pub fn run(
    conn: &Connection,
    paths: &TapectlPaths,
    command: &ConfigCommands,
    json_output: bool,
    dry_run: bool,
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
        ConfigCommands::Set { .. } | ConfigCommands::Add { .. } | ConfigCommands::Remove { .. } => {
            // `main.rs` dispatches these to `run_edit` before the strict
            // load (#143); reaching here means a caller skipped that.
            run_edit(paths, command, json_output, dry_run)?;
            Ok(EXIT_SUCCESS)
        }
    }
}

/// `config set` / `add` / `remove` (issue #143): plan the edit with
/// [`crate::config_edit::plan`] — which refuses, naming every problem and
/// writing nothing, an edit whose result would not load — then replace the
/// file atomically. `--dry-run` plans (and so refuses) exactly as the real
/// run does, and writes nothing.
pub fn run_edit(
    paths: &TapectlPaths,
    command: &ConfigCommands,
    json_output: bool,
    dry_run: bool,
) -> Result<()> {
    use crate::config_edit::Edit;
    let edit = match command {
        ConfigCommands::Set { key, value } => Edit::Set {
            key: key.clone(),
            value: value.clone(),
        },
        ConfigCommands::Add { key, values } => Edit::Add {
            key: key.clone(),
            values: values.clone(),
        },
        ConfigCommands::Remove { key, values } => Edit::Remove {
            key: key.clone(),
            values: values.clone(),
        },
        ConfigCommands::Show | ConfigCommands::Check => {
            unreachable!("run_edit is only called for set/add/remove")
        }
    };
    let file = &paths.config_file;
    let original = std::fs::read_to_string(file)?;
    let planned = crate::config_edit::plan(&original, file, &edit)?;
    apply_planned(file, &original, &planned, json_output, dry_run)
}

/// Write (unless `dry_run`) and report a planned edit.
fn apply_planned(
    file: &std::path::Path,
    original: &str,
    planned: &crate::config_edit::Planned,
    json_output: bool,
    dry_run: bool,
) -> Result<()> {
    for w in &planned.warnings {
        eprintln!("{w}");
    }
    if planned.changed && !dry_run {
        crate::config_edit::replace_file(file, original, &planned.new_text)?;
    }
    if json_output {
        let mut obj = planned.json.clone();
        obj["file"] = serde_json::json!(file.display().to_string());
        obj["changed"] = serde_json::json!(planned.changed);
        if dry_run {
            obj["dry_run"] = serde_json::json!(true);
        }
        if !planned.remaining_problems.is_empty() {
            obj["remaining_problems"] = serde_json::json!(planned.remaining_problems);
        }
        println!("{obj}");
    } else if !planned.changed {
        println!(
            "no change: {} already says so ({})",
            file.display(),
            planned.summary
        );
    } else if dry_run {
        println!(
            "would {} in {} (DRY RUN — no changes made)",
            planned.summary,
            file.display()
        );
    } else {
        println!("{} in {}", planned.summary, file.display());
    }
    if !planned.remaining_problems.is_empty() {
        eprintln!(
            "warning: {} still does not load — `tapectl config check` lists it all:",
            file.display()
        );
        for p in &planned.remaining_problems {
            eprintln!("  - {p}");
        }
    }
    Ok(())
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
    //
    // Issue #431: a backend without `device_sg` is shown with the node the
    // kernel derives for it. A config that loads has been through that
    // already; the lenient parse of one that does not never was, so it is
    // done here (idempotent) and the checks below read the same thing every
    // other command would.
    let mut best_effort = report.best_effort.clone();
    if let Some(cfg) = best_effort.as_mut() {
        cfg.fill_derived_sg_nodes();
    }
    let loaded = best_effort.as_ref();
    let sg_derived = loaded
        .map(|cfg| derived_sg_nodes(&toml_str, cfg))
        .unwrap_or_default();

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
    // `[defaults]`-specific remediation text (e.g. the issue #348 renames,
    // "min_copies_for_tape_only was renamed to min_copies") which
    // `report.problems`'s generic message does not carry.
    let unknown_key_hits = crate::policy::unknown_keys::scan(&toml_str);

    // Advisory scan (issue #50/#92 precedent): a `preserve_acls` that
    // disagrees with its layer's `preserve_xattrs` cannot take effect (dar
    // has no independent ACL switch; ACLs ride EAs, issue #347).
    let subsumed_hits = match loaded {
        Some(cfg) if paths.db_file.exists() => crate::policy::subsumed::scan(cfg, conn),
        Some(cfg) => crate::policy::subsumed::scan(cfg, &rusqlite::Connection::open_in_memory()?),
        None => Vec::new(),
    };

    // Decorative-key advisory (issue #62, #92/#50 precedent; populated by
    // issue #347). Reads the `archive_sets` table too, like `subsumed`.
    let decorative_hits = match loaded {
        Some(cfg) if paths.db_file.exists() => crate::policy::decorative::scan(cfg, conn),
        Some(cfg) => crate::policy::decorative::scan(cfg, &rusqlite::Connection::open_in_memory()?),
        None => Vec::new(),
    };

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

    // ADR-0012, 2026-09-24 amendment, item 7: the quiet-host limits in
    // force — whether the file has a `[host_check]` table, and what the
    // check will actually use. Display only; validation is `report`'s.
    let host_check = loaded.map(|cfg| (cfg.host_check.is_some(), cfg.host_check()));

    // Issue #393: what an `[ops] group` lets another account see, and what
    // is not yet set up for it. Advisory, like the scans above.
    let ops_lines = describe_ops(paths, loaded.and_then(|cfg| cfg.ops.as_ref()));

    // Issue #453: `[defaults] verify_interval_days` as the config in force
    // has it: the file's value, else the code default of 365 (ADR-0012 item
    // 75); `null` only when the config did not parse well enough to say.
    // `config show` prints the file, which names the key only when it was set
    // or written by `init`; this is the one place `--json` states the value in
    // force outright, and the human report's last line states it too.
    let verify_interval_days = loaded.map(|cfg| cfg.defaults.verify_interval_days);
    let defaults_json = loaded.map(|cfg| {
        serde_json::json!({
            "verify_interval_days": cfg.defaults.verify_interval_days,
        })
    });

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
            &sg_derived,
            &unsupported_compression_hits,
            &capacity_override_hits,
            host_check.as_ref(),
            &ops_lines,
            defaults_json,
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
            &sg_derived,
            &unsupported_compression_hits,
            &capacity_override_hits,
            host_check.as_ref(),
            &ops_lines,
            verify_interval_days,
        );
    }

    Ok(if report.valid && report.slice_cap_problems.is_empty() {
        EXIT_SUCCESS
    } else {
        EXIT_ERROR
    })
}

/// Issue #431: the backends of `cfg` whose `device_sg` the file does not set
/// but which now carry one — `(backend name, derived node)` — so `config
/// check` can say it was derived rather than configured. `toml_str` is the
/// file as written (a loaded `Config` cannot tell the two apart); a file
/// that does not parse as TOML names none.
fn derived_sg_nodes(toml_str: &str, cfg: &crate::config::Config) -> Vec<(String, String)> {
    let Ok(table) = toml_str.parse::<toml::Table>() else {
        return Vec::new();
    };
    let written: std::collections::HashSet<String> = table
        .get("backends")
        .and_then(|b| b.get("lto"))
        .and_then(|l| l.as_array())
        .map(|entries| {
            entries
                .iter()
                .filter(|e| e.get("device_sg").is_some())
                .filter_map(|e| e.get("name")?.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    cfg.backends
        .lto
        .iter()
        .filter(|b| !b.device_sg.is_empty() && !written.contains(&b.name))
        .map(|b| (b.name.clone(), b.device_sg.clone()))
        .collect()
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
    sg_derived: &[(String, String)],
    unsupported_compression_hits: &[crate::policy::compression_capability::UnsupportedCompressionHit],
    capacity_override_hits: &[&str],
    host_check: Option<&(bool, crate::config::HostCheckConfig)>,
    ops_lines: &[String],
    defaults_json: Option<serde_json::Value>,
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
                "value": h.preserve_acls,
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
                "device_sg_derived": sg_derived.iter().any(|(name, _)| *name == c.backend_name),
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
            "slice_cap_problems": report.slice_cap_problems,
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
            "host_check": host_check.map(|(present, hc)| serde_json::json!({
                "table_present": present,
                "effective": hc,
                "warnings": crate::host_check::long_process_name_warnings(hc),
            })),
            "ops": ops_lines,
            "defaults": defaults_json,
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
    sg_derived: &[(String, String)],
    unsupported_compression_hits: &[crate::policy::compression_capability::UnsupportedCompressionHit],
    capacity_override_hits: &[&str],
    host_check: Option<&(bool, crate::config::HostCheckConfig)>,
    ops_lines: &[String],
    verify_interval_days: Option<i64>,
) {
    if report.valid {
        println!("config: valid");
    } else {
        println!("config: INVALID");
        for problem in &report.problems {
            println!("  - {problem}");
        }
    }
    // Issue #479: the file loads, so every other command runs, but a slice
    // size the measured post-warning room cannot hold is an error `config
    // check` reports and exits non-zero on.
    for problem in &report.slice_cap_problems {
        println!("error: {problem}");
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
            "  hint: an unrecognised key under [policy] is refused by name; \
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
        println!("{}", describe_device_check(check));
        if let Some((_, node)) = sg_derived
            .iter()
            .find(|(name, _)| *name == check.backend_name)
        {
            println!(
                "  backend \"{}\": device_sg is not set; derived {node} from device_tape \
                 (the kernel's binding, read from sysfs)",
                check.backend_name
            );
        }
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
    if let Some((present, hc)) = host_check {
        println!("{}", describe_host_check(*present, hc));
        for line in crate::host_check::long_process_name_warnings(hc) {
            println!("{line}");
        }
    }
    for line in ops_lines {
        println!("{line}");
    }
    if let Some(days) = verify_interval_days {
        println!(
            "defaults: verify_interval_days = {days} (`audit` warns when a unit has gone \
             longer without a passing full `volume verify`; an archive set that names its \
             own interval overrides this for its units)"
        );
    }
}

/// `config check`'s lines on the `[ops] group` (issue #393): what the group
/// can see, and every way the home is not yet as that needs. Advisory: an
/// ops group that is not set up only hides the logs from it, so nothing
/// here changes the exit code. Pure but for reading the filesystem and this
/// process's groups.
pub(crate) fn describe_ops(
    paths: &TapectlPaths,
    ops: Option<&crate::config::OpsConfig>,
) -> Vec<String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    // No table: the private default, nothing to report.
    let Some(ops) = ops else {
        return Vec::new();
    };
    let name = ops.group.as_str();
    let Some(group) = nix::unistd::Group::from_name(name).ok().flatten() else {
        return vec![format!(
            "warning: ops: [ops] group \"{name}\" does not exist — create it (groupadd \
             {name}) and add the service user and the operator's account to it"
        )];
    };
    let mut lines = vec![format!(
        "ops: group \"{name}\" may read the session logs (`tapectl --home {} status`), \
         and nothing else in the home",
        paths.home.display()
    )];
    let gid = group.gid;
    let member = nix::unistd::getegid() == gid
        || nix::unistd::getgroups()
            .map(|g| g.contains(&gid))
            .unwrap_or(false);
    if !member {
        lines.push(format!(
            "warning: ops: this user is not a member of \"{name}\", so it cannot give the home \
             and logs/ that group — usermod -aG {name} <service user>, then log in again"
        ));
    }
    for (dir, want) in [(&paths.home, 0o710u32), (&paths.logs_dir, 0o2750)] {
        match std::fs::metadata(dir) {
            Ok(m) => {
                let mode = m.permissions().mode() & 0o7777;
                if m.gid() != gid.as_raw() || mode != want {
                    lines.push(format!(
                        "warning: ops: {} is mode {mode:04o}, group {}; wants {want:04o}, group \
                         \"{name}\" (set on the next tapectl command run as the service user)",
                        dir.display(),
                        m.gid()
                    ));
                }
            }
            Err(e) => lines.push(format!("warning: ops: {}: {e}", dir.display())),
        }
    }
    // Logs written before the group was set stay 0600: name how many.
    if let Ok(entries) = std::fs::read_dir(&paths.logs_dir) {
        let private = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".log"))
            .filter_map(|e| e.metadata().ok())
            .filter(|m| m.gid() != gid.as_raw() || m.permissions().mode() & 0o040 == 0)
            .count();
        if private > 0 {
            lines.push(format!(
                "ops: {private} older session log(s) are not readable by \"{name}\" (written \
                 before the group was set); `tapectl status` names them unreadable"
            ));
        }
    }
    lines
}

/// `config check`'s line for one backend's device nodes: the library's
/// ([`crate::policy::depth_check::describe_tape_device`]), except for a drive
/// that is simply not attached (issue #447).
///
/// A backend with no `device_sg` whose tape node is not there cannot have
/// had one derived — [`crate::tape::drive_identity::sg_pairing_problem`]
/// says so, and rightly refuses a write through it — but that is the normal
/// state of a drive that is unplugged, and `describe_tape_device` words any
/// pairing problem as "warning: … volume write refuses this backend", which
/// is a sentence about a MISCONFIGURED drive. It is a note here, as it was
/// before 1.1.0 made the empty `device_sg` a pairing problem. The exit code
/// and the `--json` report (which still carries the problem) are unchanged;
/// with the node present and `device_sg` still underivable it stays the
/// warning it is.
pub(crate) fn describe_device_check(check: &crate::policy::depth_check::TapeDeviceCheck) -> String {
    match &check.pairing_problem {
        Some(problem) if check.device_sg.is_empty() && !check.device_tape_exists => {
            format!("note: {problem} — normal if the drive isn't attached")
        }
        _ => crate::policy::depth_check::describe_tape_device(check),
    }
}

/// `config check`'s one line on the quiet-host limits in force (ADR-0012,
/// 2026-09-24 amendment, item 7) — where they come from, then each value,
/// so the operator can see what `volume write`'s pre-flight will ask about
/// without reading the source for the defaults.
pub(crate) fn describe_host_check(present: bool, hc: &crate::config::HostCheckConfig) -> String {
    let list = |v: &[String]| {
        if v.is_empty() {
            "none".to_string()
        } else {
            v.join(", ")
        }
    };
    format!(
        "host check ({}): units {}; processes {}; max load {:.2}/CPU; min available {} MiB; \
         max memory pressure {:.2}%; max I/O pressure {:.2}% — `tapectl host check` runs it",
        if present {
            "[host_check]"
        } else {
            "defaults, no [host_check] table"
        },
        list(&hc.contender_units),
        list(&hc.contender_processes),
        hc.max_load_per_cpu,
        hc.min_available_mb,
        hc.max_memory_pressure_pct,
        hc.max_io_pressure_pct,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HostCheckConfig;

    /// Issue #431: `config check` says which backends' `device_sg` it
    /// derived — those the file does not set, and only those that now carry a
    /// node. Derivation is faked by a sysfs tree under a temp dir.
    #[test]
    fn config_check_names_the_backends_whose_device_sg_was_derived() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dev = |node: &str| tmp.path().join(node).display().to_string();
        // Tape nodes that exist (plain files stand in; nothing opens them),
        // except the third, which is not attached.
        for node in ["nst-fake-7", "nst-fake-8"] {
            std::fs::File::create(tmp.path().join(node)).unwrap();
        }
        let toml_str = format!(
            "[[backends.lto]]\nname = \"derived\"\ndevice_tape = \"{}\"\ngeneration = \"LTO-6\"\n\n\
             [[backends.lto]]\nname = \"written\"\ndevice_tape = \"{}\"\n\
             device_sg = \"/dev/sg-written\"\ngeneration = \"LTO-6\"\n\n\
             [[backends.lto]]\nname = \"underivable\"\ndevice_tape = \"{}\"\ngeneration = \"LTO-6\"\n",
            dev("nst-fake-7"),
            dev("nst-fake-8"),
            dev("nst-fake-9"),
        );
        let toml_str = toml_str.as_str();
        let mut cfg: crate::config::Config = toml::from_str(toml_str).unwrap();
        let sysfs = tmp.path().join("sys");
        for (node, sg) in [
            ("nst-fake-7", "sg13"),
            ("nst-fake-8", "sg14"),
            ("nst-fake-9", "sg15"),
        ] {
            let dir = sysfs.join(node).join("device").join("scsi_generic");
            std::fs::create_dir_all(dir.join(sg)).unwrap();
        }
        cfg.fill_derived_sg_nodes_under(&sysfs);

        assert_eq!(
            derived_sg_nodes(toml_str, &cfg),
            vec![("derived".to_string(), "/dev/sg13".to_string())],
            "only the backend the file leaves without device_sg, and that got one"
        );
        // A file that is not TOML names none rather than guessing.
        assert!(derived_sg_nodes("not [toml", &cfg).is_empty());
    }

    /// Issue #447: a drive that is simply not attached (no tape node, so no
    /// `device_sg` could be derived) is a note, not "warning: … volume write
    /// refuses this backend". Through the production scan, not a hand-built
    /// check; the node-present, still-underivable case is the control that
    /// keeps its warning, and a proven wrong pairing is untouched.
    #[test]
    fn config_check_calls_a_detached_drive_a_note_and_a_broken_one_a_warning() {
        let tmp = tempfile::TempDir::new().unwrap();
        let present = tmp.path().join("nst-present");
        std::fs::File::create(&present).unwrap();
        let config_for = |tape: &str| {
            toml::from_str::<crate::config::Config>(&format!(
                "[[backends.lto]]\nname = \"drive\"\ndevice_tape = \"{tape}\"\n\
                 generation = \"LTO-6\"\n"
            ))
            .unwrap()
        };
        let line_for = |cfg: &crate::config::Config| {
            let checks = crate::policy::depth_check::scan_tape_devices(cfg);
            assert_eq!(checks.len(), 1);
            assert!(
                checks[0].pairing_problem.is_some(),
                "the scan still reports the problem (the JSON and the write refusal keep it)"
            );
            describe_device_check(&checks[0])
        };

        let detached = line_for(&config_for(
            tmp.path().join("nst-not-attached").to_str().unwrap(),
        ));
        assert!(detached.starts_with("note: "), "{detached}");
        assert!(detached.contains("isn't attached"), "{detached}");
        assert!(
            detached.contains("device_sg is not set"),
            "it still says what is missing: {detached}"
        );
        assert!(!detached.contains("warning"), "{detached}");
        assert!(!detached.contains("volume write refuses"), "{detached}");

        // The node is there and the kernel cannot name its sg node: that is
        // a problem the operator has to fix, and it stays a warning.
        let broken = line_for(&config_for(present.to_str().unwrap()));
        assert!(broken.starts_with("warning: "), "{broken}");
        assert!(
            broken.contains("volume write refuses this backend"),
            "{broken}"
        );
    }

    #[test]
    fn config_check_names_the_host_check_limits_and_where_they_come_from() {
        let line = describe_host_check(false, &HostCheckConfig::default());
        assert!(line.contains("defaults, no [host_check] table"), "{line}");
        assert!(line.contains("units none"), "{line}");
        assert!(
            line.contains("cargo, rustc, docker, Runner.Worker"),
            "{line}"
        );
        assert!(line.contains("min available 2048 MiB"), "{line}");

        let hc = HostCheckConfig {
            contender_units: vec!["homorg-db-suite.timer".into()],
            ..HostCheckConfig::default()
        };
        let line = describe_host_check(true, &hc);
        assert!(line.starts_with("host check ([host_check])"), "{line}");
        assert!(line.contains("units homorg-db-suite.timer"), "{line}");
    }
}
