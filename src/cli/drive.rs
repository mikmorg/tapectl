//! `tapectl drive poll` — read the drive's health on a schedule (ADR-0012,
//! 2026-10-07 item 29; issue #309). The work is `tape::poll`; this module
//! resolves the drive, takes its lock BEFORE the catalog is opened, prints
//! the report and turns it into the exit code the timer's wrapper reads.

use clap::Subcommand;

use crate::config::{Config, TapectlPaths};
use crate::error::{Result, TapectlError, EXIT_SUCCESS};
use crate::tape::poll::{self, PollReport, Probe};

#[derive(Subcommand, Debug)]
pub enum DriveCommands {
    /// Read the drive's health pages (and the cartridge chip, when one is
    /// loaded) and record them — read-only, through the SCSI generic node
    /// only
    ///
    /// Sends LOG SENSE, READ ATTRIBUTE and INQUIRY to the backend's
    /// `device_sg`, and nothing else: it never opens the tape node, never
    /// moves the tape, never loads or ejects a cartridge. Every log page it
    /// reads is journalled verbatim as a contact of its own (`report
    /// health` shows the reading), so a TapeAlert page the drive clears on
    /// read is never lost. With no known cartridge loaded the reading is
    /// the drive's alone.
    ///
    /// Takes the drive lock without waiting: while a tapectl command has
    /// the drive, or another command is writing the catalog, it reads
    /// nothing and exits 75. It reads no page the catalog cannot record.
    /// Exit 0: recorded, the drive reported nothing. Exit 1: recorded, and
    /// the drive raised a TapeAlert or reported an unrecovered error
    /// (named). Exit 2: no reading — the sg node could not be read, the
    /// drive is not configured, or the catalog refuses writes — or a
    /// reading the catalog could not record (named, and printed in full).
    /// Run daily by `contrib/systemd/tapectl-drive-poll.timer`.
    Poll {
        /// The drive to poll, by its tape node path as configured
        /// (`[[backends.lto]].device_tape`; a by-id link). Defaults to the
        /// only configured drive. The tape node itself is never opened.
        #[arg(long)]
        device: Option<String>,
    },
}

/// Exit 1: the reading was recorded and the drive reported a problem.
pub const EXIT_DRIVE_REPORTED_PROBLEM: i32 = 1;

/// Exit 75 (sysexits' `EX_TEMPFAIL`, as a busy catalog): a tapectl command
/// holds the drive, so the poll read nothing.
pub const EXIT_DRIVE_BUSY: i32 = 75;

/// Run a `drive` subcommand; returns the exit code. Opens the catalog
/// itself, AFTER the drive lock is taken, so a poll that skips touches
/// nothing at all.
pub fn run(
    paths: &TapectlPaths,
    config: &Config,
    command: &DriveCommands,
    json_output: bool,
    dry_run: bool,
) -> Result<i32> {
    match command {
        DriveCommands::Poll { device } => {
            if dry_run {
                return Err(crate::cli::refuse_dry_run(
                    "drive poll",
                    "a poll is one read of the drive and a record of it, and a preview would \
                     have to read the drive too (`tapectl report health` shows what earlier \
                     polls recorded).",
                ));
            }
            // Strict: the poll reads through `device_sg`, so it needs a
            // configured backend — and refuses one whose sg node is PROVEN
            // to be another drive (#329), the fail-closed the issue asks for.
            let backend = crate::config::resolve_lto_backend(config, device.as_deref())?;
            if !crate::staging::lock::try_hold_drive(&paths.db_file, &backend.device_tape)? {
                let msg = format!(
                    "drive poll: {} is held by a tapectl command; nothing was read (exit \
                     {EXIT_DRIVE_BUSY}) — the next poll will read it",
                    backend.device_tape
                );
                if json_output {
                    println!("{}", serde_json::json!({ "skipped": true, "reason": msg }));
                } else {
                    eprintln!("{msg}");
                }
                return Ok(EXIT_DRIVE_BUSY);
            }
            let conn = crate::db::open(&paths.db_file)?;
            let report = poll::poll(&conn, config, backend, Probe::default())?;
            render(&report, json_output);
            exit_code(&report)
        }
    }
}

/// The exit code for a poll: an error when something it read could not be
/// recorded (never "busy": the reading is lost, so the wrapper must
/// `/fail`), an error when no log page could be read at all, 1 when the
/// drive reported a problem, 0 otherwise.
pub fn exit_code(report: &PollReport) -> Result<i32> {
    if !report.unrecorded.is_empty() {
        return Err(TapectlError::Other(format!(
            "drive poll read {} but could not record in the catalog: {}. Page 0x2E (TapeAlert) \
             may clear when it is read, so what the drive said is in this output only",
            report.device_sg,
            report.unrecorded.join("; ")
        )));
    }
    if report.pages_read == 0 {
        return Err(TapectlError::Other(format!(
            "drive poll read no log page from {} ({} attempt(s) failed, each journalled in the \
             catalog's log-page journal). Is the service user in the group that owns the sg \
             node, and is device_sg this drive's?",
            report.device_sg, report.pages_failed
        )));
    }
    Ok(if report.drive_reported_problem() {
        EXIT_DRIVE_REPORTED_PROBLEM
    } else {
        EXIT_SUCCESS
    })
}

fn render(report: &PollReport, json_output: bool) {
    if json_output {
        let mut v = serde_json::to_value(report).unwrap_or_default();
        if let Some(obj) = v.as_object_mut() {
            obj.insert(
                "drive_reported_problem".into(),
                report.drive_reported_problem().into(),
            );
            obj.insert("problems".into(), report.problems().into());
        }
        println!("{v}");
        return;
    }
    println!(
        "drive {} ({}): {} log page(s) read, {} failed",
        report.backend,
        report
            .drive_serial
            .as_deref()
            .map(|s| format!("serial {s}"))
            .unwrap_or_else(|| "no serial".into()),
        report.pages_read,
        report.pages_failed
    );
    match (&report.medium_serial, &report.cartridge) {
        (None, _) => println!("  no cartridge answered (an empty drive): a drive-only reading"),
        (Some(serial), None) => {
            println!("  cartridge {serial} is loaded and is not registered: a drive-only reading")
        }
        (Some(serial), Some(barcode)) => println!(
            "  cartridge {barcode} (chip {serial}) is loaded{}",
            if report.volumes.is_empty() {
                String::new()
            } else {
                format!("; the catalog has {} on it", report.volumes.join(", "))
            }
        ),
    }
    match report.tape_alerts {
        Some(0) => println!("  TapeAlert: none raised"),
        Some(_) => {}
        None => println!("  TapeAlert: page 0x2E was not read"),
    }
    let problems = report.problems();
    for p in &problems {
        println!("  ** {p} **");
    }
    for u in &report.unrecorded {
        println!("  !! NOT RECORDED: {u}");
    }
    if let Some(cid) = report.contact_id {
        println!("  recorded as contact {cid} (tapectl report health)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> PollReport {
        PollReport {
            pages_read: 13,
            tape_alerts: Some(0),
            uncorrected: Some(0),
            ..PollReport::default()
        }
    }

    /// The wrapper's contract, by value: clean is 0, a raised flag or an
    /// unrecovered error is 1, nothing read is an error (2) — never a
    /// quiet 0.
    #[test]
    fn the_exit_codes_say_what_the_drive_said() {
        assert_eq!(exit_code(&report()).unwrap(), 0, "positive control: clean");
        let mut raised = report();
        raised.raised_alerts = vec!["Cleaning required".into()];
        raised.tape_alerts = Some(1);
        assert_eq!(exit_code(&raised).unwrap(), EXIT_DRIVE_REPORTED_PROBLEM);
        let mut unrecovered = report();
        unrecovered.uncorrected = Some(2);
        assert_eq!(
            exit_code(&unrecovered).unwrap(),
            EXIT_DRIVE_REPORTED_PROBLEM
        );
        let mut nothing = report();
        nothing.pages_read = 0;
        nothing.pages_failed = 3;
        assert!(
            exit_code(&nothing).is_err(),
            "no reading is never a quiet 0"
        );
        // A reading that could not be recorded fails even when the drive
        // reported nothing — and is never "busy" (75 sends no ping).
        let mut lost = report();
        lost.unrecorded = vec!["page 0x2e's journal row (disk full)".into()];
        let err = exit_code(&lost).expect_err("a lost reading is never a quiet 0");
        assert!(!crate::db::busy::is_busy_error(&err), "{err}");
        assert!(err.to_string().contains("0x2e"), "{err}");
        assert_eq!(EXIT_DRIVE_BUSY, crate::error::EXIT_CATALOG_BUSY);
    }
}
