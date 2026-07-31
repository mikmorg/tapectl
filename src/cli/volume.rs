use clap::Subcommand;
use rusqlite::Connection;

use crate::config::{Config, TapectlPaths};
use crate::error::Result;
use crate::store::Tier;
use crate::volume::write;

const DEFAULT_BLOCK_SIZE: usize = 512 * 1024; // 512 KB

#[derive(Subcommand, Debug)]
pub enum VolumeCommands {
    /// Initialize a new volume (write ID thunk to tape)
    Init {
        /// Volume label (e.g., L6-0001)
        label: String,
        /// Tape device path
        #[arg(long, default_value = "/dev/nst0")]
        device: String,
        /// Overwrite a cartridge whose File 0 already identifies a
        /// DIFFERENT volume (e.g. a mislabeled or stale tape). Refused by
        /// default (issue #27) — loading the wrong cartridge would
        /// otherwise silently overwrite it. Never overrides a cartridge
        /// that is already SEALED (ADR-0003): bulk-erase the physical tape
        /// and run `cartridge mark-erased` first for that case.
        #[arg(long)]
        force: bool,
    },

    /// Write staged data to volume
    Write {
        /// Volume label
        label: String,
        /// Tape device path
        #[arg(long, default_value = "/dev/nst0")]
        device: String,
        /// See `volume init --force` — same override, same limits.
        #[arg(long)]
        force: bool,
    },

    /// Resume an interrupted write session (issue #25). Reload the SAME
    /// cartridge first: the session continues from its frozen staging files
    /// rather than rebuilding them, and it refuses (quarantining the volume)
    /// if the loaded tape's File 0 does not match, or if the tape is already
    /// sealed. There is no --force: `volume write --force` overrides a
    /// wrong-cartridge finding before anything is written, which has no
    /// meaning for a tape this session has already partly written.
    Resume {
        /// Volume label
        label: String,
        /// Tape device path
        #[arg(long, default_value = "/dev/nst0")]
        device: String,
    },

    /// Deliberately abandon a volume's unfinished write session (issue #94):
    /// `docs/design/layout-session.md`'s Aborted row, first clause. Use this
    /// when a `volume resume` reports a revalidation failure you know to be
    /// permanent (the staged data is really gone), or to clear a `planned`
    /// session that was killed before anything was written. Nothing can tell
    /// a transient cause from a permanent one but you, which is why resume
    /// never decides this on its own.
    ///
    /// The tape is never contacted (hence no --device): the cartridge is left
    /// unsealed and physically unharmed, and the staged files stay pinned
    /// until `staging clean` runs. The session, however, becomes unresumable
    /// for good.
    Abort {
        /// Volume label
        label: String,
        /// Skip the confirmation prompt (ADR-0008 Tier 2). Required in a
        /// non-interactive session, which otherwise refuses rather than
        /// assuming consent.
        #[arg(long)]
        yes: bool,
    },

    /// Verify volume contents via the keyless chain walk (seal -> front index
    /// -> content). Default tier is `--full` (integrity: hashes every
    /// content file); `--quick` opts down to navigable (seal binding + front
    /// index self-consistency only, no per-file content hashing).
    Verify {
        /// Volume label
        label: String,
        /// Tape device path
        #[arg(long, default_value = "/dev/nst0")]
        device: String,
        /// Full integrity chain walk (default): hash every content file
        /// against the front index's ciphertext hashes.
        #[arg(long, conflicts_with = "quick")]
        full: bool,
        /// Quick navigable-only chain walk: seal binding + front index
        /// self-consistency, no per-file content hashing.
        #[arg(long)]
        quick: bool,
    },

    /// Identify a tape (read ID thunk)
    Identify {
        /// Tape device path
        #[arg(long, default_value = "/dev/nst0")]
        device: String,
    },

    /// Move a volume to a location
    Move {
        /// Volume label
        label: String,
        /// Destination location name
        #[arg(long)]
        to: String,
    },

    /// Retire a volume (with impact analysis)
    Retire {
        /// Volume label
        label: String,
    },

    /// Read encrypted slices from a volume into staging (then use `volume write` to write them)
    ReadSlices {
        /// Source volume label
        #[arg(long)]
        from: String,
        /// Unit name to read
        #[arg(long)]
        unit: String,
        /// Tape device path
        #[arg(long, default_value = "/dev/nst0")]
        device: String,
    },

    /// Read live encrypted slices from a volume to staging (compaction step 1)
    CompactRead {
        /// Source volume label
        label: String,
        /// Tape device path
        #[arg(long, default_value = "/dev/nst0")]
        device: String,
    },

    /// Write compaction slices from staging to destination (compaction step 2)
    CompactWrite {
        /// Destination volume label
        #[arg(long)]
        destination: String,
        /// Tape device path
        #[arg(long, default_value = "/dev/nst0")]
        device: String,
    },

    /// Show bin-packing plan for pending staged data
    Plan {
        /// Number of copies to plan
        #[arg(long, default_value = "1")]
        copies: i64,
    },

    /// Retire source volume after compaction (compaction step 3)
    CompactFinish {
        /// Source volume label to retire
        label: String,
    },

    /// Interactive compaction: read + write + finish in one flow
    Compact {
        /// Source volume label
        label: String,
        /// Tape device path
        #[arg(long, default_value = "/dev/nst0")]
        device: String,
    },
}

/// Run a volume subcommand. Returns the process exit code (issue #45/H10),
/// mirroring the `audit` convention (`src/cli/audit.rs`): 0=clean,
/// 1=warning, 2=violation. Every arm but `Verify` has no exit-code
/// semantics of its own and returns `EXIT_SUCCESS`; the caller (`main.rs`)
/// decides whether to actually call `std::process::exit`, exactly as it
/// does for `audit`.
pub fn run(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    command: &VolumeCommands,
    json_output: bool,
    yes: bool,
    dry_run: bool,
) -> Result<i32> {
    let mut exit_code = crate::error::EXIT_SUCCESS;
    match command {
        VolumeCommands::Init {
            label,
            device,
            force,
        } => {
            let vol_id =
                write::volume_init(conn, config, label, device, DEFAULT_BLOCK_SIZE, *force)?;
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"volume_id": vol_id, "label": label, "status": "initialized"})
                );
            } else {
                println!("volume \"{label}\" initialized (id={vol_id})");
            }
        }

        VolumeCommands::Write {
            label,
            device,
            force,
        } => {
            write::volume_write(
                conn,
                paths,
                config,
                label,
                device,
                DEFAULT_BLOCK_SIZE,
                *force,
            )?;
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"label": label, "status": "completed"})
                );
            } else {
                println!("volume \"{label}\" write completed");
            }
        }

        VolumeCommands::Resume { label, device } => {
            write::volume_resume(conn, paths, config, label, device, DEFAULT_BLOCK_SIZE)?;
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"label": label, "status": "completed", "resumed": true})
                );
            } else {
                println!("volume \"{label}\" write resumed and completed");
            }
        }

        VolumeCommands::Abort { label, yes } => {
            write::volume_abort(conn, label, *yes)?;
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"label": label, "status": "aborted"})
                );
            } else {
                println!(
                    "volume \"{label}\" write session aborted — the session can no longer be \
                     resumed. The cartridge is unsealed and unharmed; the staged slices stay \
                     pinned until `tapectl staging clean`."
                );
            }
        }

        VolumeCommands::Verify {
            label,
            device,
            full: _,
            quick,
        } => {
            let tier = if *quick {
                Tier::Navigable
            } else {
                Tier::default()
            };
            let tier_name = if *quick { "quick" } else { "full" };
            let report =
                write::volume_verify(conn, config, label, device, DEFAULT_BLOCK_SIZE, tier)?;
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "label": label,
                        "tier": tier_name,
                        "checked": report.checked,
                        "passed": report.passed,
                        "failed": report.failed,
                    })
                );
            } else {
                println!(
                    "verify {label} ({tier_name} tier): {} checked, {} passed, {} failed",
                    report.checked, report.passed, report.failed,
                );
            }
            // issue #45/H10: a failing verify must not exit 0 — a
            // cron-scheduled integrity check that finds corruption but
            // reports success defeats the entire point of verifying.
            exit_code = verify_exit_code(&report);
        }

        VolumeCommands::Identify { device } => {
            let id = write::volume_identify(device, DEFAULT_BLOCK_SIZE)?;
            println!("{id}");
        }

        VolumeCommands::Move { label, to } => {
            crate::cli::location::move_volume(conn, label, to)?;
            if json_output {
                println!("{}", serde_json::json!({"label": label, "location": to}));
            } else {
                println!("volume \"{label}\" moved to \"{to}\"");
            }
        }

        VolumeCommands::Retire { label } => {
            crate::cli::operations::volume_retire(conn, label, yes, dry_run, json_output)?;
        }

        VolumeCommands::ReadSlices { from, unit, device } => {
            let report = write::read_slices(conn, config, from, unit, device, DEFAULT_BLOCK_SIZE)?;
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "from": from, "unit": unit,
                        "slices_read": report.slices_read,
                        "bytes_read": report.bytes_read,
                    })
                );
            } else {
                println!(
                    "read {} slices ({} MB) from \"{}\" into staging",
                    report.slices_read,
                    report.bytes_read / (1024 * 1024),
                    from,
                );
                println!(
                    "run `tapectl volume write DEST --device {}` to write to tape",
                    device
                );
            }
        }

        VolumeCommands::Plan { copies } => {
            // Show what staged data would be written
            let mut stmt = conn.prepare(
                "SELECT u.name, s.version, ss.num_slices, ss.total_encrypted_size
                 FROM stage_sets ss
                 JOIN snapshots s ON s.id = ss.snapshot_id
                 JOIN units u ON u.id = s.unit_id
                 WHERE ss.status = 'staged'
                 ORDER BY ss.total_encrypted_size DESC",
            )?;
            let rows: Vec<(String, i64, Option<i64>, Option<i64>)> = stmt
                .query_map([], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;

            if rows.is_empty() {
                println!("no staged data to plan");
            } else {
                let total_bytes: i64 = rows.iter().map(|(_, _, _, s)| s.unwrap_or(0)).sum();
                let total_slices: i64 = rows.iter().map(|(_, _, n, _)| n.unwrap_or(0)).sum();

                if json_output {
                    let units: Vec<serde_json::Value> = rows
                        .iter()
                        .map(|(name, ver, slices, size)| {
                            serde_json::json!({"unit": name, "version": ver, "slices": slices, "size": size})
                        })
                        .collect();
                    println!(
                        "{}",
                        serde_json::json!({
                            "copies": copies, "total_slices": total_slices,
                            "total_bytes": total_bytes, "units": units,
                        })
                    );
                } else {
                    println!("volume write plan ({copies} copy/copies):");
                    for (name, ver, slices, size) in &rows {
                        println!(
                            "  {name} v{ver}: {} slices, {} MB",
                            slices.unwrap_or(0),
                            size.unwrap_or(0) / (1024 * 1024),
                        );
                    }
                    println!(
                        "\ntotal: {total_slices} slices, {} MB x {copies} = {} MB",
                        total_bytes / (1024 * 1024),
                        total_bytes * copies / (1024 * 1024),
                    );
                    // Estimate tapes needed from configured LTO backend
                    let backend = config.backends.lto.first().ok_or_else(|| {
                        crate::error::TapectlError::Config("no LTO backend configured".into())
                    })?;
                    let tape_cap = crate::staging::parse_size_to_bytes(&backend.nominal_capacity)?;
                    let factor = backend.usable_capacity_factor;
                    let usable = (tape_cap as f64 * factor) as i64;
                    let tapes_needed = ((total_bytes * copies) + usable - 1) / usable;
                    println!(
                        "estimated tapes: {tapes_needed} (at {}% usable capacity)",
                        (factor * 100.0).round() as i64
                    );
                }
            }
        }

        VolumeCommands::CompactRead { label, device } => {
            let report = write::compact_read(conn, config, label, device, DEFAULT_BLOCK_SIZE)?;
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"label": label, "slices_read": report.slices_read, "bytes_read": report.bytes_read})
                );
            } else {
                println!(
                    "compact-read \"{label}\": {} live slices ({} MB) staged",
                    report.slices_read,
                    report.bytes_read / (1024 * 1024),
                );
            }
        }

        VolumeCommands::CompactWrite {
            destination,
            device,
        } => {
            write::compact_write(conn, paths, config, destination, device, DEFAULT_BLOCK_SIZE)?;
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"destination": destination, "status": "completed"})
                );
            } else {
                println!("compact-write to \"{destination}\" completed");
            }
        }

        VolumeCommands::CompactFinish { label } => {
            let report = write::compact_finish(conn, label)?;
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "label": label,
                        "status": "retired",
                        "affected_units": compact_finish_evidence_json(&report),
                    })
                );
            } else {
                println!("compact-finish \"{label}\": volume retired");
                print_compact_finish_evidence(&report);
            }
        }

        VolumeCommands::Compact { label, device } => {
            // Interactive: run all 3 steps
            println!("=== Step 1: Reading live slices from \"{label}\" ===");
            let report = write::compact_read(conn, config, label, device, DEFAULT_BLOCK_SIZE)?;
            println!(
                "  Read {} slices ({} MB)",
                report.slices_read,
                report.bytes_read / (1024 * 1024),
            );

            println!("\nInsert destination tape and enter volume label:");
            let mut dest_label = String::new();
            std::io::stdin().read_line(&mut dest_label).ok();
            let dest_label = dest_label.trim();

            if dest_label.is_empty() {
                return Err(crate::error::TapectlError::Other(
                    "no destination label provided".into(),
                ));
            }

            println!("=== Step 2: Writing compaction slices to \"{dest_label}\" ===");
            write::compact_write(conn, paths, config, dest_label, device, DEFAULT_BLOCK_SIZE)?;
            println!("  Write completed");

            println!("=== Step 3: Retiring source volume \"{label}\" ===");
            let report = write::compact_finish(conn, label)?;
            println!("  Volume \"{label}\" retired");
            if !json_output {
                print_compact_finish_evidence(&report);
            }

            if json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "source": label,
                        "destination": dest_label,
                        "status": "completed",
                        "affected_units": compact_finish_evidence_json(&report),
                    })
                );
            } else {
                println!("\ncompaction complete: {label} → {dest_label}");
            }
        }
    }
    Ok(exit_code)
}

/// ADR-0004 Tier 1: print the remaining-coverage evidence for every unit
/// `compact_finish` retired coverage for. Display-only, matching
/// `cli::operations::print_retire_impact`'s evidence lines -- compaction
/// retires the source volume exactly like `volume retire` does.
fn print_compact_finish_evidence(report: &[write::CompactFinishReport]) {
    let now = chrono::Utc::now().naive_utc();
    for unit in report {
        if let Some(line) = crate::policy::evidence::describe(&unit.unit_name, &unit.evidence, now)
        {
            println!("  {line}");
        }
    }
}

/// JSON shape for `compact_finish`'s per-unit evidence, mirroring
/// `cli::operations::retire_impacts_json`'s `evidence`/`evidence_summary`
/// fields.
fn compact_finish_evidence_json(report: &[write::CompactFinishReport]) -> Vec<serde_json::Value> {
    let now = chrono::Utc::now().naive_utc();
    report
        .iter()
        .map(|unit| {
            let evidence: Vec<serde_json::Value> = unit
                .evidence
                .iter()
                .map(|e| serde_json::json!({"volume": e.volume_label, "last_verified": e.last_verified}))
                .collect();
            let evidence_summary =
                crate::policy::evidence::describe(&unit.unit_name, &unit.evidence, now);
            serde_json::json!({
                "unit": unit.unit_name,
                "evidence": evidence,
                "evidence_summary": evidence_summary,
            })
        })
        .collect()
}

/// Decide the process exit code for `volume verify` from its report
/// (issue #45/H10). No warning tier of its own: a chain walk either
/// confirms every checked slice or it finds real corruption, so the
/// result is binary — clean or violation — unlike `fsck`, which can also
/// report a repaired-but-notable finding.
fn verify_exit_code(report: &write::VerifyReport) -> i32 {
    if report.failed > 0 {
        crate::error::EXIT_ERROR
    } else {
        crate::error::EXIT_SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_exit_code_clean_report_is_success() {
        let report = write::VerifyReport {
            checked: 10,
            passed: 10,
            failed: 0,
        };
        assert_eq!(verify_exit_code(&report), crate::error::EXIT_SUCCESS);
    }

    #[test]
    fn verify_exit_code_any_failure_is_violation() {
        let report = write::VerifyReport {
            checked: 10,
            passed: 9,
            failed: 1,
        };
        assert_eq!(verify_exit_code(&report), crate::error::EXIT_ERROR);
    }

    #[test]
    fn verify_exit_code_all_failed_is_violation() {
        let report = write::VerifyReport {
            checked: 3,
            passed: 0,
            failed: 3,
        };
        assert_eq!(verify_exit_code(&report), crate::error::EXIT_ERROR);
    }

    #[test]
    fn verify_exit_code_empty_report_is_success() {
        // A volume with zero checkable slices (degenerate but not a
        // failure) must not be reported as a violation.
        let report = write::VerifyReport::default();
        assert_eq!(verify_exit_code(&report), crate::error::EXIT_SUCCESS);
    }
}
