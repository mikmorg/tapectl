//! `tapectl drive poll` — read the drive's health on a schedule (ADR-0012,
//! 2026-10-07 item 29; issue #309). The work is `tape::poll`; this module
//! resolves the drive, takes its lock BEFORE the catalog is opened, prints
//! the report and turns it into the exit code the timer's wrapper reads.
//!
//! `tapectl drive status` — what is in the drive right now (issue #428). It
//! is the opposite kind of command: it records nothing (no contact, no log
//! page, no journal row), takes no lock, and is the one drive command meant
//! to be run WHILE something else has the drive. See [`status`].
//!
//! `tapectl drive eject` — unload the cartridge, with the catalog told (issue
//! #429; ADR-0012 2026-10-15 item 61). The one drive command that moves a
//! cartridge, so the one that refuses first. See [`eject`].

use clap::Subcommand;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::path::Path;

use crate::config::{Config, LtoBackendConfig, TapectlPaths};
use crate::error::{Result, TapectlError, EXIT_SUCCESS};
use crate::staging::lock::{self, DriveLockState};
use crate::tape::contact::{ContactSite, Medium, Operation, OUTCOME_FAILED, OUTCOME_OK};
use crate::tape::drive_identity::{self, DriveIdentity};
use crate::tape::ioctl::{self, CartridgeState, TapeNodeStatus};
use crate::tape::mam::{self, MamInfo, MamRead};
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

    /// Show what is in the drive right now, without touching it or the tape
    ///
    /// Reads, and records nothing: the st driver's own status for the tape
    /// node (`MTIOCGET`, through a read-only `O_NONBLOCK` open, the open
    /// `mt status` makes) — empty or loaded, ready, the write-protect tab,
    /// the position st holds, a cleaning request. Whenever the node answers
    /// it also reads the drive's identity (sysfs; an INQUIRY through the
    /// SCSI generic node only if sysfs has no serial). Only when a cartridge
    /// is loaded and ready does it read the cartridge chip's MAM (READ
    /// ATTRIBUTE through the SCSI generic node), and then the catalog's
    /// record of that chip serial: the cartridge it names (barcode, status,
    /// location) and the volumes it holds, or "unknown to this catalog".
    /// Also names any tapectl command holding the drive or a volume right
    /// now, with its pid and command where they can be learned.
    ///
    /// It never moves the tape: no rewind, no read of File 0, no space, no
    /// load or eject, no MTSETBLK; a rewinding node (`/dev/stN`, whose close
    /// rewinds) is refused unopened. It takes no lock and writes nothing to
    /// the catalog — no contact, no log page, no journal row — so it is safe
    /// to run while a write is in progress: a drive held by another tapectl
    /// command, or whose tape node another process has open, is reported as
    /// such and gets no SCSI command at all. (Like `mt status`, its
    /// MTIOCGET clears the st driver's recovered-error count; the count it
    /// held is printed as `recovered_errors`.)
    ///
    /// Exit 0: the probe ran, whatever it found — an empty drive, a held
    /// drive and a cartridge the catalog does not know are all answers. Exit
    /// 2: the tape node could not be read (no such drive, no permission) or
    /// no drive is configured; with `--json` the report is still printed,
    /// with the reason in it. There is no exit 1: it gives no health verdict
    /// (`drive poll` does).
    ///
    /// With `--json` the whole of stdout is one JSON object; warnings go to
    /// stderr.
    Status {
        /// The drive to look at, by its tape node path as configured
        /// (`[[backends.lto]].device_tape`; a by-id link). Defaults to the
        /// only configured drive. A path no backend claims is looked at too,
        /// without the cartridge chip unless the kernel binds a SCSI generic
        /// node to it.
        #[arg(long)]
        device: Option<String>,
    },

    /// Unload the cartridge from the drive, and tell the catalog it left
    ///
    /// Reads the cartridge chip's serial (READ ATTRIBUTE through the SCSI
    /// generic node; no motion), then rewinds and unloads the cartridge
    /// (`MTOFFL`, what `mt offline` sends) — and records it: one contact
    /// (`drive eject`) naming the cartridge and the drive, and one
    /// `ejected` event on the cartridge (on the drive when the chip is not
    /// registered). It changes nothing else: the cartridge's volumes stay
    /// bound to it (an ejected cartridge is on a shelf, not displaced), and
    /// its place stays what `cartridge move` last said. When the cartridge
    /// is going somewhere else, the output names the command.
    ///
    /// Refuses while any tapectl command holds the drive or a volume (a
    /// write, a verify, a restore, a poll): exit 75, naming the holder's pid
    /// and command, with nothing sent to the drive and nothing recorded.
    /// There is no `--force`. Once it has the drive it holds the drive's lock
    /// until it exits, so no command can start a tape operation mid-eject.
    /// The tape node being open in a process that is not tapectl is refused
    /// the same way (exit 75).
    ///
    /// Strict, like every command that moves a cartridge: the drive must be
    /// a configured backend (`tapectl backend add`), whose SCSI generic node
    /// is checked to be the same drive as its tape node. On a machine with
    /// no backend, `mt -f <device> offline` still works; the catalog is not
    /// told.
    ///
    /// Exit 0: unloaded and recorded, or nothing to do because the drive was
    /// already empty (no contact, no event: no cartridge was in the drive).
    /// Exit 75: refused, a tapectl command or another process has the drive.
    /// Exit 2: not unloaded — the tape node could not be read, a cartridge is
    /// present and the drive is not ready (loading, or faulted), or the
    /// unload failed (the contact is recorded as failed; there is no event).
    /// A catalog that cannot record the event after a good unload is a
    /// warning, not a failure.
    ///
    /// `--dry-run` does everything up to the unload: it checks for a holder,
    /// reads the node and the chip, and reports what it would eject; it takes
    /// no lock, sends no unload and records nothing. With `--json` the whole
    /// of stdout is one JSON object.
    Eject {
        /// The drive to eject from, by its tape node path as configured
        /// (`[[backends.lto]].device_tape`; a by-id link). Defaults to the
        /// only configured drive.
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
        // Read-only, so `--dry-run` has nothing to preview and is accepted
        // (tests/dry_run_contract.rs: ReadOnly). The catalog is opened for
        // reading only in effect: `db::open` writes nothing when nothing is
        // pending, and this command takes neither the drive lock nor the
        // write lock.
        DriveCommands::Status { device } => {
            // Lenient, like every read path: a backend is optional, so a
            // rebuilt machine with keys and no `backend add` can still ask
            // what is in its drive.
            let (device, backend) = crate::config::resolve_device(config, device.as_deref())?;
            let conn = crate::db::open(&paths.db_file)?;
            let report = status(&conn, paths, &device, backend, StatusProbe::default());
            render_status(&report, json_output);
            let code = status_exit_code(&report);
            if code != EXIT_SUCCESS {
                if let Some(why) = &report.tape_node.error {
                    eprintln!("drive status: {}: {why}", report.device);
                }
            }
            Ok(code)
        }
        // Honours `--dry-run` (tests/dry_run_contract.rs): it checks and
        // reports, and sends and records nothing. STRICT, unlike `status`:
        // the command moves a cartridge, and the record it makes names the
        // cartridge by the chip read through `device_sg`, so a backend whose
        // sg node is another drive (#329) must refuse here, not record the
        // wrong cartridge as ejected.
        DriveCommands::Eject { device } => {
            let backend = crate::config::resolve_lto_backend(config, device.as_deref())?;
            let conn = crate::db::open(&paths.db_file)?;
            let report = eject(
                &conn,
                paths,
                config,
                backend,
                dry_run,
                EjectProbe::default(),
            )?;
            render_eject(&report, json_output);
            Ok(eject_exit_code(&report))
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

// ── `drive status` (issue #428) ─────────────────────────────────────────────

/// How the tape node answered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeProbe {
    /// Not looked at yet (the default only a test report starts from).
    #[default]
    NotProbed,
    /// Opened `O_NONBLOCK` and answered `MTIOCGET`.
    Read,
    /// A tapectl command holds the drive lock, so the node was not opened
    /// and no SCSI command was sent.
    SkippedDriveHeld,
    /// The node is open in another process (`EBUSY`: st allows one opener),
    /// so it was not read and no SCSI command was sent.
    InUse,
    /// No drive answers on this node (`ENOENT`, `ENODEV`, `ENXIO`).
    Absent,
    /// The node exists and would not answer (no permission, an I/O error).
    Unreadable,
}

/// What `drive status` learned about the drive itself.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct DriveFacts {
    pub serial: Option<String>,
    pub vendor: Option<String>,
    pub model: Option<String>,
    pub firmware: Option<String>,
    /// The generation the registered backend declares it natively writes.
    pub declared_generation: Option<String>,
    /// The SCSI generic node the identity and the chip were read through.
    pub device_sg: Option<String>,
}

/// The st driver's answer for the tape node.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct TapeNodeFacts {
    pub probe: NodeProbe,
    /// Why the node could not be read, when it could not.
    pub error: Option<String>,
    pub cartridge: Option<CartridgeState>,
    /// A cartridge is loaded and the drive is ready (`GMT_ONLINE`, no
    /// `GMT_DR_OPEN`).
    pub ready: Option<bool>,
    /// The cartridge's write-protect tab; `None` unless a ready cartridge
    /// gives the bit a meaning.
    pub write_protected: Option<bool>,
    pub cleaning_requested: Option<bool>,
    /// The st driver's own belief about the head: not read off the tape.
    pub position: Option<PositionFacts>,
    pub density_code: Option<u8>,
    pub block_size: Option<i64>,
    /// st's recovered-error count since the previous `MTIOCGET` by anyone;
    /// this reading cleared it.
    pub recovered_errors: Option<i64>,
    pub gstat: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PositionFacts {
    /// st's file number; `None` where st has lost track.
    pub file: Option<i32>,
    pub block: Option<i32>,
    pub at_bot: bool,
    pub at_eod: bool,
    pub at_eot: bool,
}

/// The cartridge chip (MAM), read through the SCSI generic node.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct MediumFacts {
    /// The chip answered. When it did not, `error` says what the tool said.
    pub read: bool,
    pub error: Option<String>,
    pub serial: Option<String>,
    pub load_count: Option<i64>,
    pub manufacturer: Option<String>,
    /// `LTO-6` and the like, from the first source that recognised a density
    /// code; `generation_source` names it (`mam_medium_density`,
    /// `mam_format_density`, `st_driver_density`).
    pub generation: Option<String>,
    pub generation_source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VolumeFacts {
    pub label: String,
    pub status: String,
}

/// The cartridge the catalog has under the chip serial.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CartridgeFacts {
    pub barcode: String,
    pub status: String,
    pub media_type: String,
    /// The location it is recorded at (ADR-0011); `None` when not placed.
    pub location: Option<String>,
    /// The volumes with an open mount on it.
    pub volumes: Vec<VolumeFacts>,
}

/// The catalog's own last record of a cartridge in this drive: a record,
/// not an observation — the loaded cartridge is the one the chip names.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LastContactFacts {
    pub at: String,
    pub operation: String,
    pub outcome: Option<String>,
    pub cartridge: Option<String>,
    pub volume: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct CatalogFacts {
    /// The chip was read and no cartridge in the catalog has its serial.
    /// `None` when no chip serial was read, which says nothing either way.
    pub unknown_cartridge: Option<bool>,
    pub cartridge: Option<CartridgeFacts>,
    pub last_contact_on_device: Option<LastContactFacts>,
}

/// A tapectl session holding the drive or a volume.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Holder {
    /// `drive` (the drive lock) or `volume` (a volume's session lock; not
    /// tied to this drive, which a volume lock cannot say).
    pub kind: String,
    /// The tape node, or the volume's label.
    pub name: String,
    /// From `/proc/locks`; `None` when the kernel does not say.
    pub pid: Option<u32>,
    /// The session's command and phase, from its session log.
    pub command: Option<String>,
    pub phase: Option<String>,
    /// For a volume: the cartridge the catalog has it on.
    pub cartridge: Option<String>,
}

/// What `tapectl drive status` found. Everything but `tape_node.probe` and
/// the lock fields is an `Option` or a list for the same reason: a field
/// that could not be learned is absent, never a default that reads as an
/// answer.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct StatusReport {
    /// The tape node as resolved.
    pub device: String,
    /// The registered backend's name, when one claims the node.
    pub backend: Option<String>,
    pub drive: DriveFacts,
    /// `free`, `held` (a tapectl command is in contact with the drive) or
    /// `unknown` (the lockfile could not be tested).
    pub drive_lock: String,
    pub holders: Vec<Holder>,
    pub tape_node: TapeNodeFacts,
    /// `None` when no chip was asked: nothing ready was loaded, or nothing
    /// may be sent to the drive right now.
    pub medium: Option<MediumFacts>,
    pub catalog: CatalogFacts,
    /// What a reader should know that no field says.
    pub notes: Vec<String>,
}

/// Where `drive status`'s hardware and process answers come from.
/// `StatusProbe::default()` — the production value — asks the machine; a test
/// supplies each answer, as [`Probe`] does for the poll.
#[derive(Default)]
pub struct StatusProbe {
    pub drive_lock: Option<DriveLockState>,
    pub node: Option<std::io::Result<TapeNodeStatus>>,
    pub mam: Option<MamRead>,
    pub identity: Option<DriveIdentity>,
    pub holders: Option<Vec<Holder>>,
}

/// Look at the drive `device` names. Never fails: what could not be learned
/// is absent from the report, with a note, and the caller turns the node's
/// state into an exit code ([`status_exit_code`]).
///
/// The order is the safety argument. The drive lock is tested first; a drive
/// a tapectl command holds is not opened and is sent nothing. The tape node
/// is opened `O_NONBLOCK` ([`ioctl::status_nonblocking`]); one that is
/// already open elsewhere (`EBUSY`) is likewise sent nothing. Only a node
/// that answered `MTIOCGET` with a ready cartridge is asked for the chip
/// (READ ATTRIBUTE) and the drive for its identity (sysfs, and INQUIRY only
/// if sysfs has no serial): both are commands the drive answers without
/// moving the medium, and the one a poll sends too.
///
/// Nothing is written: no contact, no journal row. A contact is "a time a
/// cartridge was in a drive" and exists to carry readings that must be
/// differenced or must not be lost — the log pages a poll reads (page 0x2E
/// may clear as it is read) and the MAM it journals. This reads no log page,
/// and an ad-hoc row per look would pollute the history `cartridge info`
/// reports as every time the cartridge was in a drive.
pub fn status(
    conn: &Connection,
    paths: &TapectlPaths,
    device: &str,
    backend: Option<&LtoBackendConfig>,
    probe: StatusProbe,
) -> StatusReport {
    let mut report = StatusReport {
        device: device.to_string(),
        backend: backend.map(|b| b.name.clone()),
        ..StatusReport::default()
    };
    report.drive.declared_generation = backend.map(|b| b.generation.clone());

    // The SCSI generic node: the backend's, or the one the kernel binds to
    // the tape node (sysfs only) for a node no backend claims.
    let sg: Option<String> = match backend {
        Some(b) => Some(b.device_sg.clone()).filter(|sg| !sg.is_empty()),
        None => drive_identity::derive_sg_for(device).map(|p| p.display().to_string()),
    };
    report.drive.device_sg = sg.clone();
    if backend.is_none() {
        report.notes.push(match &sg {
            Some(_) => "no backend is configured for this node; its SCSI generic node was \
                        taken from the kernel's binding"
                .into(),
            None => "no backend is configured for this node and the kernel names no SCSI \
                     generic node for it, so the drive's identity and the cartridge chip \
                     were not read"
                .into(),
        });
    }

    // 1. Who has the drive.
    let lock_state = probe
        .drive_lock
        .unwrap_or_else(|| lock::drive_lock_state(&paths.db_file, device));
    report.drive_lock = match &lock_state {
        DriveLockState::Free => "free",
        DriveLockState::Held => "held",
        DriveLockState::Unknown(why) => {
            report
                .notes
                .push(format!("the drive lock could not be tested ({why})"));
            "unknown"
        }
    }
    .into();
    report.holders = probe
        .holders
        .unwrap_or_else(|| find_holders(conn, paths, device, &lock_state));

    // 2. What the tape node says — unless a tapectl command has the drive.
    let mut chip_allowed = false;
    if lock_state == DriveLockState::Held {
        report.tape_node.probe = NodeProbe::SkippedDriveHeld;
        report.notes.push(
            "a tapectl command holds this drive: its tape node was not opened and nothing was \
             sent to it. The catalog's record, if it has one, is below."
                .into(),
        );
    } else {
        let node = probe
            .node
            .unwrap_or_else(|| ioctl::status_nonblocking(device));
        match node {
            Ok(st) => {
                fill_node(&mut report.tape_node, &st);
                chip_allowed = st.cartridge() == CartridgeState::Loaded;
                if st.cartridge() == CartridgeState::NotReady {
                    report.notes.push(
                        "a cartridge is present and the drive is not ready (loading, or \
                         faulted): the chip was not read"
                            .into(),
                    );
                }
            }
            Err(e) => {
                let node = &mut report.tape_node;
                node.error = Some(e.to_string());
                node.probe = node_probe_of(&e);
                if node.probe == NodeProbe::InUse {
                    report.notes.push(
                        "the tape node is open in another process (the st driver allows \
                         one opener): nothing was sent to the drive"
                            .into(),
                    );
                }
            }
        }
    }

    // 3. The drive's identity and the chip: only through a node that just
    //    answered, with a ready cartridge for the chip.
    let answered = report.tape_node.probe == NodeProbe::Read;
    if answered {
        let identity = probe.identity.unwrap_or_else(|| {
            let id_backend = backend.cloned().unwrap_or_else(|| LtoBackendConfig {
                name: String::new(),
                device_tape: device.to_string(),
                device_sg: sg.clone().unwrap_or_default(),
                generation: String::new(),
                capacity_override: None,
                fill_ceiling: 0.95,
                enospc_buffer: "1GiB".into(),
            });
            drive_identity::read_identity(&id_backend)
        });
        report.drive.serial = identity.serial;
        report.drive.vendor = identity.vendor;
        report.drive.model = identity.model;
        report.drive.firmware = identity.firmware_rev;
    }
    if chip_allowed {
        let read = match (probe.mam, &sg) {
            (Some(read), _) => Some(read),
            (None, Some(sg)) => Some(mam::read_mam(sg)),
            (None, None) => None,
        };
        if let Some(read) = read {
            report.medium = Some(medium_facts(&read, report.tape_node.density_code));
        }
    }

    // 4. What the catalog says about it.
    let chip_serial = report
        .medium
        .as_ref()
        .and_then(|m| m.serial.clone())
        .filter(|s| !s.is_empty());
    if let Some(serial) = &chip_serial {
        report.catalog.cartridge = cartridge_facts(conn, serial);
        report.catalog.unknown_cartridge = Some(report.catalog.cartridge.is_none());
    }
    report.catalog.last_contact_on_device =
        last_contact_on_device(conn, device, report.drive.serial.as_deref());
    report
}

/// What a failed open or `MTIOCGET` of the tape node means: `EBUSY` is
/// another opener (st allows one), `ENOENT`/`ENODEV`/`ENXIO` no drive, and
/// anything else a node that exists and would not answer. Shared by `drive
/// status` and `drive eject`, which must agree about what a node is.
fn node_probe_of(e: &std::io::Error) -> NodeProbe {
    match e.raw_os_error() {
        Some(nix::libc::EBUSY) => NodeProbe::InUse,
        Some(nix::libc::ENODEV) | Some(nix::libc::ENXIO) => NodeProbe::Absent,
        _ if e.kind() == std::io::ErrorKind::NotFound => NodeProbe::Absent,
        _ => NodeProbe::Unreadable,
    }
}

/// Fold the st driver's answer into the report's tape-node section.
fn fill_node(node: &mut TapeNodeFacts, st: &TapeNodeStatus) {
    let cartridge = st.cartridge();
    node.probe = NodeProbe::Read;
    node.cartridge = Some(cartridge);
    node.ready = Some(cartridge == CartridgeState::Loaded);
    node.write_protected = st.write_protected();
    node.cleaning_requested = Some(st.cleaning_requested());
    node.position = (cartridge == CartridgeState::Loaded).then(|| PositionFacts {
        file: st.file_number(),
        block: st.block_number(),
        at_bot: st.at_bot(),
        at_eod: st.at_eod(),
        at_eot: st.at_eot(),
    });
    node.density_code = st.density_code();
    node.block_size = Some(st.block_size());
    node.recovered_errors = Some(st.raw.soft_errors());
    node.gstat = st.raw.gstat_flags().into_iter().map(String::from).collect();
}

/// The chip's facts, and the generation its density codes name — the
/// priority `media_detect::detect` uses: the MAM medium density code, then
/// the MAM format density code, then the st driver's density register.
fn medium_facts(read: &MamRead, st_density: Option<u8>) -> MediumFacts {
    let (generation, source) = medium_generation(&read.info, st_density);
    MediumFacts {
        read: read.error.is_none() && read.capture.ok(),
        error: read.error.clone(),
        serial: read.info.serial.clone(),
        load_count: read.info.load_count,
        manufacturer: read.info.manufacturer.clone(),
        generation: generation.map(|g| g.as_str().to_string()),
        generation_source: source.map(String::from),
    }
}

fn medium_generation(
    mam: &MamInfo,
    st_density: Option<u8>,
) -> (Option<crate::media::Generation>, Option<&'static str>) {
    use crate::media::Generation;
    let candidates = [
        (mam.medium_density_code, "mam_medium_density"),
        (mam.format_density_code, "mam_format_density"),
        (st_density, "st_driver_density"),
    ];
    for (code, source) in candidates {
        if let Some(g) = code.and_then(Generation::from_density_code) {
            return (Some(g), Some(source));
        }
    }
    (None, None)
}

/// The cartridge the catalog has under chip serial `serial` (identity is the
/// chip serial, ADR-0012), with the volumes holding an open mount on it.
fn cartridge_facts(conn: &Connection, serial: &str) -> Option<CartridgeFacts> {
    let (id, barcode, status, media_type, location): (i64, String, String, String, Option<String>) =
        conn.query_row(
            "SELECT c.id, c.barcode, c.status, c.media_type, l.name
               FROM cartridges c LEFT JOIN locations l ON l.id = c.location_id
              WHERE c.serial_number = ?1",
            params![serial],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()
        .ok()
        .flatten()?;
    let volumes = conn
        .prepare(
            "SELECT v.label, v.status FROM cartridge_volumes cv
               JOIN volumes v ON v.id = cv.volume_id
              WHERE cv.cartridge_id = ?1 AND cv.unmounted_at IS NULL
              ORDER BY v.label",
        )
        .and_then(|mut stmt| {
            stmt.query_map(params![id], |r| {
                Ok(VolumeFacts {
                    label: r.get(0)?,
                    status: r.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
        })
        .unwrap_or_default();
    Some(CartridgeFacts {
        barcode,
        status,
        media_type,
        location,
        volumes,
    })
}

/// The newest `cartridge_contacts` row for this drive: the rows that name the
/// node as given or as it resolves, or the drive's serial once it is known.
fn last_contact_on_device(
    conn: &Connection,
    device: &str,
    drive_serial: Option<&str>,
) -> Option<LastContactFacts> {
    let canonical = std::fs::canonicalize(device)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| device.to_string());
    conn.query_row(
        "SELECT cc.opened_at, cc.operation, cc.outcome, c.barcode, v.label
           FROM cartridge_contacts cc
           LEFT JOIN cartridges c ON c.id = cc.cartridge_id
           LEFT JOIN volumes v ON v.id = cc.volume_id
          WHERE cc.device IN (?1, ?2)
             OR cc.drive_id = (SELECT id FROM drives WHERE serial = ?3)
          ORDER BY cc.opened_at DESC, cc.id DESC LIMIT 1",
        params![device, canonical, drive_serial],
        |r| {
            Ok(LastContactFacts {
                at: r.get(0)?,
                operation: r.get(1)?,
                outcome: r.get(2)?,
                cartridge: r.get(3)?,
                volume: r.get(4)?,
            })
        },
    )
    .optional()
    .ok()
    .flatten()
}

/// The tapectl sessions holding the drive lock or a volume's session lock
/// right now. A pid comes from `/proc/locks` (the kernel's owner of the
/// `flock`), and its command and phase from the running session whose log
/// names that pid ([`crate::cli::status::read_sessions`]) — both best
/// effort: a holder whose pid or log cannot be learned is still listed.
fn find_holders(
    conn: &Connection,
    paths: &TapectlPaths,
    device: &str,
    drive_lock: &DriveLockState,
) -> Vec<Holder> {
    // The locks first: the session logs are read only if something holds one.
    let mut held: Vec<(&str, String, Vec<u32>, Option<String>)> = Vec::new();
    if *drive_lock == DriveLockState::Held {
        let lockfile = lock::drive_lock_path(&paths.db_file, device);
        held.push((
            "drive",
            device.to_string(),
            lock::lock_holder_pids(&lockfile),
            None,
        ));
    }
    if let Some(db_file) = lock::db_file_of(conn) {
        for id in lock::live_volume_sessions(conn) {
            let label: Option<String> = conn
                .query_row("SELECT label FROM volumes WHERE id = ?1", [id], |r| {
                    r.get(0)
                })
                .optional()
                .ok()
                .flatten();
            let cartridge: Option<String> = conn
                .query_row(
                    "SELECT c.barcode FROM cartridge_volumes cv
                       JOIN cartridges c ON c.id = cv.cartridge_id
                      WHERE cv.volume_id = ?1
                      ORDER BY cv.unmounted_at IS NULL DESC, cv.mounted_at DESC LIMIT 1",
                    [id],
                    |r| r.get(0),
                )
                .optional()
                .ok()
                .flatten();
            held.push((
                "volume",
                label.unwrap_or_else(|| format!("(volume {id})")),
                lock::lock_holder_pids(&lock::volume_lock_path(&db_file, id)),
                cartridge,
            ));
        }
    }
    if held.is_empty() {
        return Vec::new();
    }
    let sessions = crate::cli::status::read_sessions(&paths.logs_dir, 0).unwrap_or_default();
    let mut out = Vec::new();
    for (kind, name, pids, cartridge) in held {
        // A hold whose pid the kernel does not give is still a hold.
        let pids: Vec<Option<u32>> = if pids.is_empty() {
            vec![None]
        } else {
            pids.into_iter().map(Some).collect()
        };
        for pid in pids {
            let session = pid.and_then(|p| sessions.iter().find(|s| s.pid == Some(p)));
            out.push(Holder {
                kind: kind.to_string(),
                name: name.clone(),
                pid,
                command: session.and_then(|s| s.command.clone()),
                phase: session.and_then(|s| s.phase.clone()),
                cartridge: cartridge.clone(),
            });
        }
    }
    out
}

/// The exit code for a status: 0 whenever the probe ran, 2 when the tape
/// node could not be read at all (no such drive, no permission). A held drive,
/// an open node, an empty drive and a cartridge the catalog does not know are
/// answers, not failures.
pub fn status_exit_code(report: &StatusReport) -> i32 {
    match report.tape_node.probe {
        NodeProbe::Absent | NodeProbe::Unreadable => crate::error::EXIT_ERROR,
        _ => EXIT_SUCCESS,
    }
}

/// One holder as a line: what it holds, its pid, its command and phase, and
/// for a volume the cartridge the catalog has it on. Shared by `drive
/// status` and the refusals of `drive eject`.
fn holder_line(h: &Holder) -> String {
    format!(
        "{} {}{}{}{}{}",
        h.kind,
        h.name,
        h.pid.map(|p| format!(", pid {p}")).unwrap_or_default(),
        h.command
            .as_deref()
            .map(|c| format!(", `{c}`"))
            .unwrap_or_default(),
        h.phase
            .as_deref()
            .map(|p| format!(", {p}"))
            .unwrap_or_default(),
        h.cartridge
            .as_deref()
            .map(|c| format!(", on cartridge {c}"))
            .unwrap_or_default(),
    )
}

fn render_status(report: &StatusReport, json_output: bool) {
    if json_output {
        println!("{}", serde_json::to_string(report).unwrap_or_default());
        return;
    }
    print!("{}", status_text(report));
}

/// The human form of a status.
pub fn status_text(report: &StatusReport) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "drive {}{}",
        report.device,
        report
            .backend
            .as_deref()
            .filter(|b| !b.is_empty())
            .map(|b| format!(" (backend {b})"))
            .unwrap_or_default()
    );
    let d = &report.drive;
    let identity = [d.vendor.as_deref(), d.model.as_deref()]
        .iter()
        .flatten()
        .copied()
        .collect::<Vec<_>>()
        .join(" ");
    if !identity.is_empty() || d.serial.is_some() {
        let _ = writeln!(
            out,
            "  drive:     {}{}{}{}",
            if identity.is_empty() {
                "(unnamed)".to_string()
            } else {
                identity
            },
            d.serial
                .as_deref()
                .map(|s| format!(", serial {s}"))
                .unwrap_or_default(),
            d.firmware
                .as_deref()
                .map(|f| format!(", firmware {f}"))
                .unwrap_or_default(),
            d.declared_generation
                .as_deref()
                .filter(|g| !g.is_empty())
                .map(|g| format!(" (backend declares {g})"))
                .unwrap_or_default(),
        );
    }
    // tapectl's own hold.
    match report.drive_lock.as_str() {
        "held" => {
            let _ = writeln!(out, "  tapectl:   HELD — a tapectl command has this drive");
        }
        "unknown" => {
            let _ = writeln!(
                out,
                "  tapectl:   unknown whether a tapectl command has this drive"
            );
        }
        _ => {
            let _ = writeln!(out, "  tapectl:   free — no tapectl command has this drive");
        }
    }
    for h in &report.holders {
        let _ = writeln!(out, "             {}", holder_line(h));
    }
    // The tape node.
    let n = &report.tape_node;
    match n.probe {
        NodeProbe::Read => {
            let cartridge = match n.cartridge {
                Some(CartridgeState::Empty) => "no cartridge loaded".to_string(),
                Some(CartridgeState::Loaded) => {
                    let mut s = String::from("cartridge loaded, ready");
                    match n.write_protected {
                        Some(true) => s.push_str(", WRITE-PROTECTED"),
                        Some(false) => s.push_str(", writable"),
                        None => {}
                    }
                    s
                }
                Some(CartridgeState::NotReady) => {
                    "cartridge present, drive NOT READY (loading, or faulted)".to_string()
                }
                None => "unknown".to_string(),
            };
            let _ = writeln!(out, "  tape node: {cartridge}");
            if let Some(p) = &n.position {
                let at = |v: Option<i32>| v.map(|v| v.to_string()).unwrap_or_else(|| "?".into());
                let mut marks = Vec::new();
                if p.at_bot {
                    marks.push("at BOT");
                }
                if p.at_eod {
                    marks.push("at EOD");
                }
                if p.at_eot {
                    marks.push("at EOT");
                }
                let _ = writeln!(
                    out,
                    "  position:  file {}, block {}{} (the st driver's count; the tape was not read)",
                    at(p.file),
                    at(p.block),
                    if marks.is_empty() {
                        String::new()
                    } else {
                        format!(", {}", marks.join(", "))
                    }
                );
            }
            if n.cleaning_requested == Some(true) {
                let _ = writeln!(out, "  cleaning:  the drive asks to be CLEANED");
            }
            if let Some(e) = n.recovered_errors.filter(|e| *e > 0) {
                let _ = writeln!(
                    out,
                    "  st recovered {e} error(s) since the previous status read (now cleared)"
                );
            }
        }
        NodeProbe::SkippedDriveHeld => {
            let _ = writeln!(
                out,
                "  tape node: not opened (a tapectl command has the drive)"
            );
        }
        NodeProbe::InUse => {
            let _ = writeln!(out, "  tape node: open in another process; not read");
        }
        NodeProbe::Absent | NodeProbe::Unreadable | NodeProbe::NotProbed => {
            let _ = writeln!(
                out,
                "  tape node: {} — {}",
                if n.probe == NodeProbe::Absent {
                    "no drive answers"
                } else {
                    "cannot be read"
                },
                n.error.as_deref().unwrap_or("not probed")
            );
        }
    }
    // The chip and what the catalog makes of it.
    if let Some(m) = &report.medium {
        if !m.read {
            let _ = writeln!(
                out,
                "  chip:      not read ({})",
                m.error.as_deref().unwrap_or("no answer")
            );
        } else {
            let _ = writeln!(
                out,
                "  chip:      serial {}{}{}{}",
                m.serial.as_deref().unwrap_or("(none reported)"),
                m.generation
                    .as_deref()
                    .map(|g| format!(", {g}"))
                    .unwrap_or_default(),
                m.load_count
                    .map(|l| format!(", {l} load(s)"))
                    .unwrap_or_default(),
                m.manufacturer
                    .as_deref()
                    .map(|m| format!(", {m}"))
                    .unwrap_or_default(),
            );
        }
    }
    let c = &report.catalog;
    match (&c.cartridge, c.unknown_cartridge) {
        (Some(cart), _) => {
            let _ = writeln!(
                out,
                "  catalog:   cartridge {} ({}, {}) at {}",
                cart.barcode,
                cart.media_type,
                cart.status,
                cart.location.as_deref().unwrap_or("(not placed)")
            );
            if cart.volumes.is_empty() {
                let _ = writeln!(out, "             no volume on it");
            }
            for v in &cart.volumes {
                let _ = writeln!(out, "             volume {} ({})", v.label, v.status);
            }
        }
        (None, Some(true)) => {
            let _ = writeln!(
                out,
                "  catalog:   this cartridge is unknown to this catalog"
            );
        }
        (None, _) => {}
    }
    if let Some(l) = &c.last_contact_on_device {
        let _ = writeln!(
            out,
            "  last contact on this drive (catalog record): {} — {}{}{}",
            l.at,
            l.operation,
            l.cartridge
                .as_deref()
                .map(|c| format!(", cartridge {c}"))
                .unwrap_or_default(),
            l.outcome
                .as_deref()
                .map(|o| format!(" ({o})"))
                .unwrap_or_default(),
        );
    }
    for note in &report.notes {
        let _ = writeln!(out, "  note: {note}");
    }
    out
}

// ── `drive eject` (issue #429) ──────────────────────────────────────────────

/// How an eject ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EjectOutcome {
    /// The cartridge was unloaded, and the contact and event recorded.
    Ejected,
    /// `--dry-run`: every check ran, nothing was sent or recorded.
    WouldEject,
    /// No cartridge in the drive: the end state already holds. Nothing was
    /// sent and nothing recorded — no cartridge was in the drive, so there
    /// was no contact.
    NoCartridge,
    /// A tapectl command holds the drive or a volume.
    RefusedHeld,
    /// The tape node is open in a process that is not holding tapectl's lock.
    RefusedNodeInUse,
    /// A cartridge is present and the drive is not ready: loading, or
    /// faulted. Its chip cannot be read, so the record could not name it.
    RefusedNotReady,
    /// The tape node could not be read (no such drive, no permission).
    NodeUnreadable,
    /// The unload failed; the contact is recorded as failed.
    UnloadFailed,
}

/// What `tapectl drive eject` did and found.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EjectReport {
    pub device: String,
    pub backend: String,
    pub dry_run: bool,
    pub outcome: EjectOutcome,
    /// The outcome in words: what was done, or why not and what to do.
    pub message: String,
    /// Who held the drive or a volume, for a refusal.
    pub holders: Vec<Holder>,
    /// The chip's serial, when the chip answered.
    pub chip_serial: Option<String>,
    /// The cartridge the catalog has under that serial.
    pub cartridge: Option<CartridgeFacts>,
    /// The `drive eject` contact, `None` unless one was opened.
    pub contact_id: Option<i64>,
    /// The `ejected` event, `None` unless one was written.
    pub event_id: Option<i64>,
    pub warnings: Vec<String>,
}

/// Where `drive eject`'s hardware and process answers come from.
/// `EjectProbe::default()` — the production value — asks the machine; a test
/// supplies each answer, and the unload itself.
#[derive(Default)]
pub struct EjectProbe<'a> {
    pub node: Option<std::io::Result<TapeNodeStatus>>,
    pub mam: Option<MamRead>,
    pub identity: Option<DriveIdentity>,
    pub holders: Option<Vec<Holder>>,
    /// The unload. `None`, the production value, is [`ioctl::offline`].
    pub unload: Option<&'a mut dyn FnMut() -> std::io::Result<()>>,
    pub sysfs_root: Option<&'a Path>,
}

/// Eject the cartridge from the drive `backend` names, and record that it
/// left.
///
/// THE ORDER IS THE SAFETY ARGUMENT. Nothing is sent to the drive until a
/// check has said no tapectl command has it:
///
/// 1. **The drive lock is TAKEN**, not tested ([`lock::try_hold_drive`], one
///    atomic step: a test followed by a take would leave a gap in which a
///    write's contact could start). A failure to take it names the holder
///    and refuses. A dry run only tests it ([`lock::drive_lock_state`]) and
///    keeps nothing. Once taken it is kept until the process exits, which is
///    what stops a command from starting a tape operation mid-eject: a
///    contact waits for it ([`lock::hold_drive_for_contact`]).
/// 2. Any live volume session refuses too. A session holds its volume's lock
///    from before its contact opens (a write's staging check, a verify's
///    open), and a volume lock cannot say which drive it will use.
/// 3. The tape node is read ([`ioctl::status_nonblocking`], `MTIOCGET`).
///    Open elsewhere (`EBUSY`) refuses; empty is nothing to do; present but
///    not ready refuses.
/// 4. The chip is read (READ ATTRIBUTE through the sg node; no motion), so the
///    contact can name the cartridge. A chip that does not answer does not
///    stop the eject; the contact records why it has no cartridge.
/// 5. The contact opens, the cartridge is unloaded, the contact closes
///    (`ok`, or `failed` with the error), and only a good unload writes the
///    event.
///
/// It never touches `cartridge_volumes` (ADR-0012 2026-10-15 item 61):
/// migration 020 reserves `unmounted_at` for displacement, and coverage reads
/// `unmounted_at IS NULL` as "bound" — a cartridge on a shelf is still bound.
///
/// Errors only for what makes the lock unobtainable (an unwritable `locks/`);
/// every other outcome is a report.
pub fn eject(
    conn: &Connection,
    paths: &TapectlPaths,
    config: &Config,
    backend: &LtoBackendConfig,
    dry_run: bool,
    mut probe: EjectProbe<'_>,
) -> Result<EjectReport> {
    let device = backend.device_tape.as_str();
    let mut report = EjectReport {
        device: device.to_string(),
        backend: backend.name.clone(),
        dry_run,
        outcome: EjectOutcome::NoCartridge,
        message: String::new(),
        holders: Vec::new(),
        chip_serial: None,
        cartridge: None,
        contact_id: None,
        event_id: None,
        warnings: Vec::new(),
    };

    // 1 and 2. Who has the drive, and who has a volume.
    let drive_held = if dry_run {
        match lock::drive_lock_state(&paths.db_file, device) {
            DriveLockState::Free => false,
            DriveLockState::Held => true,
            DriveLockState::Unknown(why) => {
                return Err(TapectlError::Other(format!(
                    "drive eject: the drive lock for {device} could not be tested ({why}); \
                     refusing — the lock is what keeps a command from starting a tape \
                     operation under an eject"
                )))
            }
        }
    } else {
        !lock::try_hold_drive(&paths.db_file, device)?
    };
    let lock_state = if drive_held {
        DriveLockState::Held
    } else {
        DriveLockState::Free
    };
    report.holders = probe
        .holders
        .take()
        .unwrap_or_else(|| find_holders(conn, paths, device, &lock_state));
    if drive_held || !report.holders.is_empty() {
        let who: Vec<String> = report.holders.iter().map(holder_line).collect();
        report.outcome = EjectOutcome::RefusedHeld;
        let held_by = if who.is_empty() {
            "a tapectl command holds the drive".to_string()
        } else {
            format!("held by {}", who.join("; "))
        };
        report.message = format!(
            "drive eject: refused — {held_by}. Nothing was sent to {device} and nothing was \
             recorded; `tapectl status` shows what is running. Run this again when it has \
             finished (there is no --force)."
        );
        return Ok(report);
    }

    // 3. What the tape node says.
    let status = match probe
        .node
        .take()
        .unwrap_or_else(|| ioctl::status_nonblocking(device))
    {
        Ok(status) => status,
        Err(e) => {
            match node_probe_of(&e) {
                NodeProbe::InUse => {
                    report.outcome = EjectOutcome::RefusedNodeInUse;
                    report.message = format!(
                        "drive eject: refused — {device} is open in another process (the st \
                         driver allows one opener) that is not holding tapectl's drive lock. \
                         Nothing was sent to the drive. Close whatever has it open and run \
                         this again."
                    );
                }
                NodeProbe::Absent => {
                    report.outcome = EjectOutcome::NodeUnreadable;
                    report.message = format!("drive eject: no drive answers at {device}: {e}");
                }
                _ => {
                    report.outcome = EjectOutcome::NodeUnreadable;
                    report.message = format!("drive eject: {device} cannot be read: {e}");
                }
            }
            return Ok(report);
        }
    };
    match status.cartridge() {
        CartridgeState::Empty => {
            report.outcome = EjectOutcome::NoCartridge;
            report.message = format!("no cartridge is loaded in {device}; nothing to eject");
            return Ok(report);
        }
        CartridgeState::NotReady => {
            report.outcome = EjectOutcome::RefusedNotReady;
            report.message = format!(
                "drive eject: a cartridge is present in {device} and the drive is not ready \
                 (loading, or faulted), so its chip cannot be read and the record could not \
                 name what leaves. Nothing was sent. Try again in a minute; if the drive is \
                 faulted, `mt -f {device} offline` unloads it without telling the catalog."
            );
            return Ok(report);
        }
        CartridgeState::Loaded => {}
    }

    // 4. Which cartridge: the chip's serial, never a guess from the catalog.
    let mam_read = probe
        .mam
        .take()
        .unwrap_or_else(|| mam::read_mam(&backend.device_sg));
    report.chip_serial = mam_read.info.serial.clone().filter(|s| !s.is_empty());
    report.cartridge = report
        .chip_serial
        .as_deref()
        .and_then(|serial| cartridge_facts(conn, serial));
    let what = match (&report.cartridge, &report.chip_serial) {
        (Some(c), Some(serial)) => format!("cartridge {} (chip {serial})", c.barcode),
        (None, Some(serial)) => {
            format!("a cartridge whose chip serial {serial} is not registered in this catalog")
        }
        (_, None) => "a cartridge whose chip serial could not be read".to_string(),
    };
    if dry_run {
        report.outcome = EjectOutcome::WouldEject;
        report.message = format!(
            "dry run: would unload {what} from {device} and record a `drive eject` contact \
             and an `ejected` event. Nothing was sent to the drive and nothing was recorded."
        );
        return Ok(report);
    }

    // 5. The contact, the unload, the event.
    let mut site = ContactSite::new(
        config,
        Operation::DriveEject,
        device,
        Medium::Observed {
            backend,
            mam: &mam_read.info,
        },
    );
    if let Some(identity) = probe.identity.as_ref() {
        site = site.with_drive_identity(identity);
    }
    if let Some(root) = probe.sysfs_root {
        site = site.with_sysfs_root(root);
    }
    let guard = site.open(conn, None);
    report.contact_id = guard.id();
    let unloaded = match probe.unload.take() {
        Some(unload) => unload(),
        None => ioctl::offline(device),
    };
    match unloaded {
        Ok(()) => {
            guard.finish(OUTCOME_OK, Some(&format!("unloaded {what}")));
            report.outcome = EjectOutcome::Ejected;
            report.message = format!("ejected {what} from {device}");
            match record_ejected(conn, &report, device) {
                Ok(id) => report.event_id = id,
                Err(e) => report.warnings.push(format!(
                    "the cartridge was unloaded, but the catalog could not record the \
                     `ejected` event: {e}"
                )),
            }
            if report.event_id.is_none() && report.warnings.is_empty() {
                report.warnings.push(
                    "the cartridge was unloaded; no `ejected` event was written because \
                     neither the cartridge nor the drive is identified in this catalog (the \
                     `drive eject` contact is the record)"
                        .into(),
                );
            }
        }
        Err(e) => {
            guard.finish(OUTCOME_FAILED, Some(&e.to_string()));
            report.outcome = EjectOutcome::UnloadFailed;
            report.message = format!(
                "drive eject: unloading {what} from {device} failed: {e}. The contact is \
                 recorded as failed and nothing else in the catalog changed; check the drive \
                 (`tapectl drive status`)."
            );
        }
    }
    Ok(report)
}

/// Write the `ejected` event for a good unload, and return its id. The
/// subject is the cartridge the contact was attributed to, else the drive
/// it was attributed to (an unregistered chip); with neither there is no
/// entity to hang an event on and none is written (`Ok(None)`).
fn record_ejected(conn: &Connection, report: &EjectReport, device: &str) -> Result<Option<i64>> {
    let Some(contact_id) = report.contact_id else {
        return Ok(None);
    };
    let (cartridge, barcode, drive, drive_serial): (
        Option<i64>,
        Option<String>,
        Option<i64>,
        Option<String>,
    ) = conn.query_row(
        "SELECT cc.cartridge_id, c.barcode, cc.drive_id, d.serial
           FROM cartridge_contacts cc
           LEFT JOIN cartridges c ON c.id = cc.cartridge_id
           LEFT JOIN drives d ON d.id = cc.drive_id
          WHERE cc.id = ?1",
        params![contact_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    let (entity, id, label) = match (cartridge, drive) {
        (Some(id), _) => ("cartridge", id, barcode),
        (None, Some(id)) => ("drive", id, drive_serial.clone()),
        (None, None) => return Ok(None),
    };
    let details = serde_json::json!({
        "contact_id": contact_id,
        "device": device,
        "chip_serial": report.chip_serial,
        "drive_serial": drive_serial,
    })
    .to_string();
    Ok(Some(crate::db::events::log_event(
        conn,
        entity,
        id,
        label.as_deref(),
        "ejected",
        Some("device"),
        None,
        Some(device),
        Some(&details),
        None,
    )?))
}

/// The exit code for an eject: 0 when the cartridge is out or never was in,
/// 75 when it was refused because the drive is in use (retry when it is
/// free), 2 when it could not be done.
pub fn eject_exit_code(report: &EjectReport) -> i32 {
    match report.outcome {
        EjectOutcome::Ejected | EjectOutcome::WouldEject | EjectOutcome::NoCartridge => {
            EXIT_SUCCESS
        }
        EjectOutcome::RefusedHeld | EjectOutcome::RefusedNodeInUse => EXIT_DRIVE_BUSY,
        EjectOutcome::RefusedNotReady
        | EjectOutcome::NodeUnreadable
        | EjectOutcome::UnloadFailed => crate::error::EXIT_ERROR,
    }
}

fn render_eject(report: &EjectReport, json_output: bool) {
    if json_output {
        println!("{}", serde_json::to_string(report).unwrap_or_default());
        return;
    }
    let text = eject_text(report);
    // A refusal or a failure is an error and goes to stderr, where a script
    // that checks the exit code looks for the reason.
    if eject_exit_code(report) == EXIT_SUCCESS {
        print!("{text}");
    } else {
        eprint!("{text}");
    }
}

/// The human form of an eject.
pub fn eject_text(report: &EjectReport) -> String {
    use std::fmt::Write;
    let mut out = format!("{}\n", report.message);
    if report.outcome == EjectOutcome::Ejected {
        if let Some(c) = &report.cartridge {
            let _ = writeln!(
                out,
                "  it is recorded at {}; its volumes stay bound to it. If it is going \
                 elsewhere: tapectl cartridge move {} --to <location>",
                c.location.as_deref().unwrap_or("(no place)"),
                c.barcode
            );
        }
        if let Some(cid) = report.contact_id {
            let _ = writeln!(out, "  recorded as contact {cid} (tapectl cartridge info)");
        }
    }
    for w in &report.warnings {
        let _ = writeln!(out, "  warning: {w}");
    }
    out
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
    // ── `drive status` (issue #428) ─────────────────────────────────────────

    use crate::tape::mtget_journal::MtStatus;
    use std::os::unix::process::ExitStatusExt;

    const GMT_BOT: i64 = 0x4000_0000;
    const GMT_WR_PROT: i64 = 0x0400_0000;
    const GMT_ONLINE: i64 = 0x0100_0000;
    const GMT_DR_OPEN: i64 = 0x0004_0000;
    const GMT_CLN: i64 = 0x0000_8000;

    /// A node that answered `MTIOCGET` with these bits.
    fn node(gstat: i64) -> std::io::Result<TapeNodeStatus> {
        Ok(TapeNodeStatus {
            raw: MtStatus {
                mt_gstat: gstat,
                mt_dsreg: 0x5a00_0000 | 524_288,
                ..MtStatus::default()
            },
        })
    }

    fn errno(e: i32) -> std::io::Result<TapeNodeStatus> {
        Err(std::io::Error::from_raw_os_error(e))
    }

    fn mam_read(stdout: Option<&str>) -> MamRead {
        let out = match stdout {
            Some(s) => std::process::Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout: s.as_bytes().to_vec(),
                stderr: Vec::new(),
            },
            None => std::process::Output {
                status: std::process::ExitStatus::from_raw(2 << 8),
                stdout: Vec::new(),
                stderr: b"Read attribute failed: Not ready".to_vec(),
            },
        };
        mam::capture_from_output(
            "/nonexistent/drive-status-test/sg7",
            "2026-10-15 09:00:00".into(),
            None,
            Ok(out),
        )
    }

    fn hp_drive() -> DriveIdentity {
        DriveIdentity {
            vendor: Some("HP".into()),
            model: Some("Ultrium 6-SCSI".into()),
            firmware_rev: Some("35GD".into()),
            serial: Some("HUJ808A5L4".into()),
        }
    }

    fn lto6_backend() -> LtoBackendConfig {
        LtoBackendConfig {
            name: "lto0".into(),
            device_tape: "/nonexistent/drive-status-test/nst7".into(),
            device_sg: "/nonexistent/drive-status-test/sg7".into(),
            generation: "LTO-6".into(),
            capacity_override: None,
            fill_ceiling: 0.95,
            enospc_buffer: "1GiB".into(),
        }
    }

    const DEVICE: &str = "/nonexistent/drive-status-test/nst7";

    /// A status of `node` with a free lock, the HP drive's identity and
    /// `mam`, against `conn`.
    fn look(
        conn: &Connection,
        node: std::io::Result<TapeNodeStatus>,
        mam: Option<MamRead>,
    ) -> StatusReport {
        let home = tempfile::TempDir::new().unwrap();
        let paths = TapectlPaths::new(home.path().to_path_buf());
        status(
            conn,
            &paths,
            DEVICE,
            Some(&lto6_backend()),
            StatusProbe {
                drive_lock: Some(DriveLockState::Free),
                node: Some(node),
                mam,
                identity: Some(hp_drive()),
                holders: Some(Vec::new()),
            },
        )
    }

    fn register_cartridge(conn: &Connection, barcode: &str, serial: &str) -> i64 {
        conn.execute(
            "INSERT INTO cartridges (barcode, media_type, serial_number, nominal_capacity, status)
             VALUES (?1, 'LTO-6', ?2, 2500000000000, 'in_use')",
            params![barcode, serial],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn put_volume_on(conn: &Connection, cartridge: i64, label: &str, status: &str) {
        conn.execute(
            "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                  capacity_bytes, status)
             VALUES (?1, 'lto', 'lto0', 'LTO-6', 2500000000000, ?2)",
            params![label, status],
        )
        .unwrap();
        let vol = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO cartridge_volumes (cartridge_id, volume_id) VALUES (?1, ?2)",
            params![cartridge, vol],
        )
        .unwrap();
    }

    /// Every table a contact, a poll or a reading would write to.
    fn rows_written(conn: &Connection) -> i64 {
        [
            "cartridge_contacts",
            "log_page_journal",
            "mam_journal",
            "mtget_journal",
            "st_stats_journal",
            "health_logs",
            "drives",
            "events",
        ]
        .iter()
        .map(|t| {
            conn.query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap_or_else(|e| panic!("{t}: {e}"))
        })
        .sum()
    }

    /// The empty drive: an answer, not a failure — nothing read from the
    /// chip, no position, no tab, exit 0.
    #[test]
    fn an_empty_drive_is_reported_as_empty_and_exits_0() {
        let conn = crate::db::open_memory().unwrap();
        let r = look(&conn, node(GMT_DR_OPEN | GMT_ONLINE), Some(mam_read(None)));
        assert_eq!(r.tape_node.probe, NodeProbe::Read);
        assert_eq!(r.tape_node.cartridge, Some(CartridgeState::Empty));
        assert_eq!(r.tape_node.ready, Some(false));
        assert_eq!(r.tape_node.write_protected, None, "no cartridge, no tab");
        assert_eq!(r.tape_node.position, None);
        assert!(r.medium.is_none(), "an empty drive is not asked for a chip");
        assert_eq!(r.catalog.unknown_cartridge, None, "no chip: says nothing");
        assert_eq!(r.drive.serial.as_deref(), Some("HUJ808A5L4"));
        assert_eq!(r.drive.declared_generation.as_deref(), Some("LTO-6"));
        assert_eq!(status_exit_code(&r), 0);
        assert!(
            status_text(&r).contains("no cartridge loaded"),
            "{}",
            status_text(&r)
        );
    }

    /// A loaded, registered cartridge: the catalog names it by the CHIP's
    /// serial, with its place and the volumes on it.
    #[test]
    fn a_known_cartridge_is_named_with_its_place_and_volumes() {
        let conn = crate::db::open_memory().unwrap();
        let cart = register_cartridge(&conn, "CART01L6", "EW7VWMVKF6");
        put_volume_on(&conn, cart, "L6-0001", "sealed");
        conn.execute(
            "INSERT INTO locations (name, kind) VALUES ('shelf', 'shelf')",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE cartridges SET location_id = last_insert_rowid() WHERE id = ?1",
            [cart],
        )
        .unwrap();
        let r = look(
            &conn,
            node(GMT_BOT | GMT_ONLINE),
            Some(mam_read(Some(mam::tests_support::LTO6_SAMPLE))),
        );
        assert_eq!(r.tape_node.cartridge, Some(CartridgeState::Loaded));
        assert_eq!(r.tape_node.write_protected, Some(false));
        let p = r
            .tape_node
            .position
            .as_ref()
            .expect("a loaded cartridge has a position");
        assert!(p.at_bot);
        assert_eq!((p.file, p.block), (Some(0), Some(0)));
        let m = r.medium.as_ref().expect("the chip was asked");
        assert!(m.read);
        assert_eq!(m.serial.as_deref(), Some("EW7VWMVKF6"));
        assert_eq!(m.generation.as_deref(), Some("LTO-6"));
        assert_eq!(m.generation_source.as_deref(), Some("mam_medium_density"));
        let c = r
            .catalog
            .cartridge
            .as_ref()
            .expect("the catalog knows this chip");
        assert_eq!(
            (c.barcode.as_str(), c.status.as_str(), c.location.as_deref()),
            ("CART01L6", "in_use", Some("shelf"))
        );
        assert_eq!(
            c.volumes,
            vec![VolumeFacts {
                label: "L6-0001".into(),
                status: "sealed".into()
            }]
        );
        assert_eq!(r.catalog.unknown_cartridge, Some(false));
        let text = status_text(&r);
        assert!(
            text.contains("CART01L6") && text.contains("L6-0001 (sealed)"),
            "{text}"
        );
    }

    /// A chip the catalog has never seen is "unknown to this catalog", not
    /// an error and not a guess from the last cartridge in the drive.
    #[test]
    fn an_unregistered_cartridge_is_unknown_to_the_catalog() {
        let conn = crate::db::open_memory().unwrap();
        let other = register_cartridge(&conn, "OTHER", "SOMEONE_ELSES_CHIP");
        conn.execute(
            "INSERT INTO cartridge_contacts (cartridge_id, operation, device) VALUES (?1, 'volume verify', ?2)",
            params![other, DEVICE],
        )
        .unwrap();
        let r = look(
            &conn,
            node(GMT_BOT | GMT_ONLINE),
            Some(mam_read(Some(mam::tests_support::LTO6_SAMPLE))),
        );
        assert_eq!(r.catalog.unknown_cartridge, Some(true));
        assert!(r.catalog.cartridge.is_none());
        // The catalog's last record of this drive is offered as a record,
        // under its own key, never as the loaded cartridge.
        let last = r.catalog.last_contact_on_device.as_ref().unwrap();
        assert_eq!(last.cartridge.as_deref(), Some("OTHER"));
        assert_eq!(last.operation, "volume verify");
        assert!(status_text(&r).contains("unknown to this catalog"));
    }

    #[test]
    fn a_write_protected_cartridge_and_a_cleaning_request_are_reported() {
        let conn = crate::db::open_memory().unwrap();
        let r = look(
            &conn,
            node(GMT_BOT | GMT_ONLINE | GMT_WR_PROT | GMT_CLN),
            Some(mam_read(None)),
        );
        assert_eq!(r.tape_node.write_protected, Some(true));
        assert_eq!(r.tape_node.cleaning_requested, Some(true));
        let text = status_text(&r);
        assert!(
            text.contains("WRITE-PROTECTED") && text.contains("CLEANED"),
            "{text}"
        );
        // A chip that would not answer is said so, and the catalog is not
        // asked to guess.
        let m = r.medium.as_ref().unwrap();
        assert!(!m.read && m.error.is_some());
        assert_eq!(r.catalog.unknown_cartridge, None);
        assert_eq!(status_exit_code(&r), 0);
        // The positive control for the two fields above: neither bit set.
        let clean = look(&conn, node(GMT_BOT | GMT_ONLINE), Some(mam_read(None)));
        assert_eq!(clean.tape_node.write_protected, Some(false));
        assert_eq!(clean.tape_node.cleaning_requested, Some(false));
    }

    /// A drive a tapectl command holds is not opened and is sent nothing:
    /// the node probe is SKIPPED (the same device, free, is probed and is
    /// absent), no identity, no chip.
    #[test]
    fn a_drive_held_by_a_session_is_not_touched() {
        let conn = crate::db::open_memory().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        let paths = TapectlPaths::new(home.path().to_path_buf());
        let holder = Holder {
            kind: "drive".into(),
            name: DEVICE.into(),
            pid: Some(4242),
            command: Some("volume write L6-0001".into()),
            phase: Some("writing".into()),
            cartridge: None,
        };
        let probe = |lock| StatusProbe {
            drive_lock: Some(lock),
            node: None, // the real open, of a path that does not exist
            mam: Some(mam_read(Some(mam::tests_support::LTO6_SAMPLE))),
            identity: Some(hp_drive()),
            holders: Some(vec![holder.clone()]),
        };
        let held = status(
            &conn,
            &paths,
            DEVICE,
            Some(&lto6_backend()),
            probe(DriveLockState::Held),
        );
        assert_eq!(held.drive_lock, "held");
        assert_eq!(held.tape_node.probe, NodeProbe::SkippedDriveHeld);
        assert_eq!(held.tape_node.error, None, "the node was not even tried");
        assert!(held.medium.is_none() && held.drive.serial.is_none());
        assert_eq!(held.holders, vec![holder.clone()]);
        assert_eq!(status_exit_code(&held), 0, "a held drive is an answer");
        let text = status_text(&held);
        assert!(
            text.contains("HELD")
                && text.contains("pid 4242")
                && text.contains("volume write L6-0001"),
            "{text}"
        );

        // Positive control: the same drive, free, is really probed.
        let free = status(
            &conn,
            &paths,
            DEVICE,
            Some(&lto6_backend()),
            probe(DriveLockState::Free),
        );
        assert_eq!(free.tape_node.probe, NodeProbe::Absent);
    }

    #[test]
    fn a_node_open_elsewhere_is_in_use_and_is_sent_nothing() {
        let conn = crate::db::open_memory().unwrap();
        let r = look(
            &conn,
            errno(nix::libc::EBUSY),
            Some(mam_read(Some(mam::tests_support::LTO6_SAMPLE))),
        );
        assert_eq!(r.tape_node.probe, NodeProbe::InUse);
        assert!(
            r.medium.is_none(),
            "no READ ATTRIBUTE to a drive someone has open"
        );
        assert!(r.drive.serial.is_none(), "and no identity read");
        assert_eq!(status_exit_code(&r), 0, "busy is an answer");
        assert!(!r.notes.is_empty());
    }

    #[test]
    fn a_node_that_cannot_be_read_exits_2_with_the_reason_in_the_report() {
        let conn = crate::db::open_memory().unwrap();
        for (e, probe) in [
            (nix::libc::ENOENT, NodeProbe::Absent),
            (nix::libc::ENODEV, NodeProbe::Absent),
            (nix::libc::ENXIO, NodeProbe::Absent),
            (nix::libc::EACCES, NodeProbe::Unreadable),
            (nix::libc::EIO, NodeProbe::Unreadable),
        ] {
            let r = look(&conn, errno(e), None);
            assert_eq!(r.tape_node.probe, probe, "errno {e}");
            assert!(r.tape_node.error.is_some(), "errno {e}");
            assert_eq!(status_exit_code(&r), crate::error::EXIT_ERROR, "errno {e}");
        }
        // Positive control: the answered node exits 0.
        assert_eq!(status_exit_code(&look(&conn, node(GMT_ONLINE), None)), 0);
    }

    /// The command writes nothing: no contact, no journal row, no reading,
    /// no drive row — the discriminating negative for "no
    /// `Operation::DriveStatus`".
    #[test]
    fn a_status_records_nothing_in_the_catalog() {
        let conn = crate::db::open_memory().unwrap();
        let cart = register_cartridge(&conn, "CART01L6", "EW7VWMVKF6");
        put_volume_on(&conn, cart, "L6-0001", "sealed");
        let before = rows_written(&conn);
        assert_eq!(
            before, 0,
            "positive control: the counter sees an empty catalog"
        );
        let r = look(
            &conn,
            node(GMT_BOT | GMT_ONLINE),
            Some(mam_read(Some(mam::tests_support::LTO6_SAMPLE))),
        );
        assert!(r.catalog.cartridge.is_some(), "the look really happened");
        assert_eq!(rows_written(&conn), 0);
        // And the counter does count a contact: the control for the control.
        conn.execute(
            "INSERT INTO cartridge_contacts (operation, device) VALUES ('drive poll', 'x')",
            [],
        )
        .unwrap();
        assert_eq!(rows_written(&conn), 1);
    }

    #[test]
    fn the_generation_comes_from_the_first_density_code_that_names_one() {
        use crate::media::Generation::{Lto5, Lto6, Lto8};
        let mam = |medium, format| MamInfo {
            medium_density_code: medium,
            format_density_code: format,
            ..MamInfo::default()
        };
        assert_eq!(
            medium_generation(&mam(Some(0x58), Some(0x5a)), Some(0x5e)),
            (Some(Lto5), Some("mam_medium_density")),
            "the medium's own code outranks the format code and the driver"
        );
        assert_eq!(
            medium_generation(&mam(None, Some(0x5a)), Some(0x5e)),
            (Some(Lto6), Some("mam_format_density"))
        );
        assert_eq!(
            medium_generation(&mam(Some(0xff), None), Some(0x5e)),
            (Some(Lto8), Some("st_driver_density")),
            "an unrecognised code falls through"
        );
        assert_eq!(medium_generation(&mam(None, None), None), (None, None));
    }

    /// The JSON is one object with the fields a script reads, by name.
    #[test]
    fn the_json_report_has_the_documented_shape() {
        let conn = crate::db::open_memory().unwrap();
        let r = look(&conn, node(GMT_BOT | GMT_ONLINE), Some(mam_read(None)));
        let v = serde_json::to_value(&r).unwrap();
        for key in [
            "device",
            "backend",
            "drive",
            "drive_lock",
            "holders",
            "tape_node",
            "medium",
            "catalog",
            "notes",
        ] {
            assert!(v.get(key).is_some(), "missing {key}: {v}");
        }
        assert_eq!(v["tape_node"]["probe"], "read");
        assert_eq!(v["tape_node"]["cartridge"], "loaded");
        assert_eq!(v["tape_node"]["ready"], true);
        assert_eq!(v["tape_node"]["position"]["at_bot"], true);
        assert_eq!(v["drive_lock"], "free");
        assert_eq!(v["drive"]["serial"], "HUJ808A5L4");
    }

    /// A node no backend claims is still looked at; the note says what was
    /// not read.
    #[test]
    fn a_device_no_backend_claims_is_looked_at_without_a_chip() {
        let conn = crate::db::open_memory().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        let paths = TapectlPaths::new(home.path().to_path_buf());
        let r = status(
            &conn,
            &paths,
            "/nonexistent/drive-status-test/nst9",
            None,
            StatusProbe {
                drive_lock: Some(DriveLockState::Free),
                node: Some(node(GMT_BOT | GMT_ONLINE)),
                mam: None,
                identity: Some(hp_drive()),
                holders: Some(Vec::new()),
            },
        );
        assert_eq!(r.backend, None);
        assert_eq!(r.tape_node.cartridge, Some(CartridgeState::Loaded));
        assert!(r.medium.is_none(), "no sg node to read the chip through");
        assert!(
            r.notes.iter().any(|n| n.contains("no backend")),
            "{:?}",
            r.notes
        );
    }

    // ── `drive eject` (issue #429) ──────────────────────────────────────────

    use std::cell::Cell;

    /// A catalog in a real file under a temp home (the drive lock and the
    /// volume locks are files beside it), an empty fake sysfs, and a config.
    struct EjectEnv {
        _home: tempfile::TempDir,
        sysfs: tempfile::TempDir,
        paths: TapectlPaths,
        conn: Connection,
        config: Config,
    }

    fn eject_env() -> EjectEnv {
        let home = tempfile::TempDir::new().unwrap();
        let paths = TapectlPaths::new(home.path().to_path_buf());
        let conn = crate::db::open(&paths.db_file).unwrap();
        EjectEnv {
            sysfs: tempfile::TempDir::new().unwrap(),
            paths,
            conn,
            config: Config::default(),
            _home: home,
        }
    }

    /// Run an eject on `env` with the node, chip and unload supplied. The
    /// unload is counted in `unloads` (and returns `unload_result`), and
    /// notes whether the drive lock was held while it ran.
    fn run_eject(
        env: &EjectEnv,
        dry_run: bool,
        node: Option<std::io::Result<TapeNodeStatus>>,
        mam: Option<MamRead>,
        unloads: &Cell<u32>,
        lock_held_during: &Cell<Option<bool>>,
        unload_result: std::io::Result<()>,
    ) -> EjectReport {
        let mut unload_result = Some(unload_result);
        let mut unload = || {
            unloads.set(unloads.get() + 1);
            lock_held_during.set(Some(
                lock::drive_lock_state(&env.paths.db_file, DEVICE) == DriveLockState::Held,
            ));
            unload_result.take().unwrap_or(Ok(()))
        };
        eject(
            &env.conn,
            &env.paths,
            &env.config,
            &lto6_backend(),
            dry_run,
            EjectProbe {
                node,
                mam,
                identity: Some(hp_drive()),
                holders: None,
                unload: Some(&mut unload),
                sysfs_root: Some(env.sysfs.path()),
            },
        )
        .expect("the lock is obtainable under a temp home")
    }

    fn loaded() -> Option<std::io::Result<TapeNodeStatus>> {
        Some(node(GMT_BOT | GMT_ONLINE))
    }

    fn chip() -> Option<MamRead> {
        Some(mam_read(Some(mam::tests_support::LTO6_SAMPLE)))
    }

    /// Every row an eject could write, plus the binding it must not touch.
    fn eject_rows(conn: &Connection) -> i64 {
        rows_written(conn)
    }

    fn bindings(conn: &Connection) -> Vec<(i64, i64, String, Option<String>)> {
        conn.prepare(
            "SELECT cartridge_id, volume_id, mounted_at, unmounted_at
               FROM cartridge_volumes ORDER BY id",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
    }

    /// A foreign holder: another open file description flocking `path`, as
    /// another process would (the in-process registry would hide it).
    fn foreign_flock(path: &std::path::Path) -> nix::fcntl::Flock<std::fs::File> {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let file = std::fs::File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .unwrap();
        nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusiveNonblock).unwrap()
    }

    /// THE test for the command's job: the cartridge is unloaded with the
    /// drive lock held, one contact and one event name it, and — the item 61
    /// negative — the binding is exactly what it was. `unmounted_at` is the
    /// displacement record; an ejected cartridge is on a shelf.
    #[test]
    fn an_eject_unloads_under_the_drive_lock_and_records_a_contact_and_an_event_only() {
        let env = eject_env();
        let cart = register_cartridge(&env.conn, "CART01L6", "EW7VWMVKF6");
        put_volume_on(&env.conn, cart, "L6-0001", "sealed");
        let bound = bindings(&env.conn);
        assert_eq!(bound.len(), 1, "positive control: the cartridge is bound");
        assert_eq!(bound[0].3, None, "positive control: the mount is open");
        let status_before: String = env
            .conn
            .query_row("SELECT status FROM cartridges WHERE id = ?1", [cart], |r| {
                r.get(0)
            })
            .unwrap();

        let (unloads, during) = (Cell::new(0), Cell::new(None));
        let r = run_eject(&env, false, loaded(), chip(), &unloads, &during, Ok(()));

        assert_eq!(r.outcome, EjectOutcome::Ejected, "{}", r.message);
        assert_eq!(eject_exit_code(&r), 0);
        assert_eq!(unloads.get(), 1, "one unload");
        assert_eq!(
            during.get(),
            Some(true),
            "the drive lock was held while the cartridge was being unloaded"
        );
        assert_eq!(
            lock::drive_lock_state(&env.paths.db_file, DEVICE),
            DriveLockState::Held,
            "and it is kept until the process exits"
        );
        assert_eq!(r.chip_serial.as_deref(), Some("EW7VWMVKF6"));
        assert_eq!(r.cartridge.as_ref().unwrap().barcode, "CART01L6");

        // The contact: one, closed ok, naming the cartridge and the drive.
        let contacts: Vec<(i64, String, Option<String>, Option<i64>, Option<i64>)> = env
            .conn
            .prepare(
                "SELECT id, operation, outcome, cartridge_id, drive_id FROM cartridge_contacts",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(contacts.len(), 1, "{contacts:?}");
        assert_eq!(contacts[0].1, "drive eject");
        assert_eq!(contacts[0].2.as_deref(), Some("ok"));
        assert_eq!(contacts[0].3, Some(cart));
        assert!(contacts[0].4.is_some(), "the drive is named");
        assert_eq!(r.contact_id, Some(contacts[0].0));

        // The event: one, on the cartridge, carrying the contact.
        let events: Vec<(String, i64, String, String)> = env
            .conn
            .prepare("SELECT entity_type, entity_id, action, details FROM events")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(
            (events[0].0.as_str(), events[0].1, events[0].2.as_str()),
            ("cartridge", cart, "ejected")
        );
        let details: serde_json::Value = serde_json::from_str(&events[0].3).unwrap();
        assert_eq!(details["contact_id"], contacts[0].0);
        assert_eq!(details["chip_serial"], "EW7VWMVKF6");
        assert_eq!(details["drive_serial"], "HUJ808A5L4");
        assert!(r.event_id.is_some());

        // Item 61: nothing else moved.
        assert_eq!(bindings(&env.conn), bound, "unmounted_at is not an eject's");
        let status_after: String = env
            .conn
            .query_row("SELECT status FROM cartridges WHERE id = ?1", [cart], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(status_after, status_before);
        let vol_status: String = env
            .conn
            .query_row(
                "SELECT status FROM volumes WHERE label = 'L6-0001'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(vol_status, "sealed");

        let text = eject_text(&r);
        assert!(
            text.contains("ejected cartridge CART01L6") && text.contains("cartridge move CART01L6"),
            "{text}"
        );
    }

    /// A failed unload is a failed contact and nothing else: no event, the
    /// binding as it was, and exit 2.
    #[test]
    fn a_failed_unload_is_a_failed_contact_with_no_event() {
        let env = eject_env();
        let cart = register_cartridge(&env.conn, "CART01L6", "EW7VWMVKF6");
        put_volume_on(&env.conn, cart, "L6-0001", "sealed");
        let bound = bindings(&env.conn);

        let (unloads, during) = (Cell::new(0), Cell::new(None));
        let r = run_eject(
            &env,
            false,
            loaded(),
            chip(),
            &unloads,
            &during,
            Err(std::io::Error::from_raw_os_error(nix::libc::EIO)),
        );

        assert_eq!(r.outcome, EjectOutcome::UnloadFailed);
        assert_eq!(eject_exit_code(&r), crate::error::EXIT_ERROR);
        assert_eq!(unloads.get(), 1, "positive control: it did try");
        let (outcome, detail): (Option<String>, Option<String>) = env
            .conn
            .query_row(
                "SELECT outcome, detail FROM cartridge_contacts WHERE operation = 'drive eject'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(outcome.as_deref(), Some("failed"));
        assert!(detail.unwrap().contains("Input/output error"));
        let events: i64 = env
            .conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(events, 0, "a failed eject writes no event");
        assert_eq!(r.event_id, None);
        assert_eq!(bindings(&env.conn), bound);
        assert!(r.message.contains("failed"), "{}", r.message);
    }

    /// Nothing in the drive is the end state already: exit 0, no unload, and
    /// no contact (no cartridge was in the drive) and no event.
    #[test]
    fn an_empty_drive_is_nothing_to_eject_and_records_nothing() {
        let env = eject_env();
        let (unloads, during) = (Cell::new(0), Cell::new(None));
        let r = run_eject(
            &env,
            false,
            Some(node(GMT_DR_OPEN | GMT_ONLINE)),
            chip(),
            &unloads,
            &during,
            Ok(()),
        );
        assert_eq!(r.outcome, EjectOutcome::NoCartridge);
        assert_eq!(eject_exit_code(&r), 0);
        assert_eq!(unloads.get(), 0, "no unload to an empty drive");
        assert_eq!(eject_rows(&env.conn), 0);
        assert!(r.message.contains("nothing to eject"), "{}", r.message);
    }

    /// A cartridge present in a drive that is not ready (loading, faulted) is
    /// refused: its chip cannot be read, so the record could not name what
    /// leaves. Nothing is sent and nothing recorded; exit 2.
    #[test]
    fn a_drive_that_is_not_ready_is_refused_untouched() {
        let env = eject_env();
        let (unloads, during) = (Cell::new(0), Cell::new(None));
        let r = run_eject(
            &env,
            false,
            Some(node(0)),
            chip(),
            &unloads,
            &during,
            Ok(()),
        );
        assert_eq!(r.outcome, EjectOutcome::RefusedNotReady);
        assert_eq!(eject_exit_code(&r), crate::error::EXIT_ERROR);
        assert_eq!(unloads.get(), 0);
        assert_eq!(eject_rows(&env.conn), 0);
    }

    /// A node open in another process (st allows one opener) is refused with
    /// 75, and a node that does not answer is 2; neither sends anything.
    #[test]
    fn a_node_that_is_busy_or_absent_is_refused_untouched() {
        for (e, outcome, code) in [
            (nix::libc::EBUSY, EjectOutcome::RefusedNodeInUse, 75),
            (nix::libc::ENOENT, EjectOutcome::NodeUnreadable, 2),
            (nix::libc::EACCES, EjectOutcome::NodeUnreadable, 2),
        ] {
            let env = eject_env();
            let (unloads, during) = (Cell::new(0), Cell::new(None));
            let r = run_eject(
                &env,
                false,
                Some(errno(e)),
                chip(),
                &unloads,
                &during,
                Ok(()),
            );
            assert_eq!(r.outcome, outcome, "errno {e}");
            assert_eq!(eject_exit_code(&r), code, "errno {e}");
            assert_eq!(unloads.get(), 0, "errno {e}");
            assert_eq!(eject_rows(&env.conn), 0, "errno {e}");
        }
    }

    /// A drive another process holds is refused with the holder named, and
    /// the tape node is not even read (the same eject with the drive free is
    /// the control: it reaches the node and finds none). Once the holder
    /// lets go, the same call goes through.
    #[test]
    fn a_held_drive_is_refused_with_its_holder_named_before_the_node_is_read() {
        let env = eject_env();
        let lockfile = lock::drive_lock_path(&env.paths.db_file, DEVICE);
        let holder = foreign_flock(&lockfile);
        let unloads = Cell::new(0);
        let mut unload = || {
            unloads.set(unloads.get() + 1);
            Ok(())
        };
        let r = eject(
            &env.conn,
            &env.paths,
            &env.config,
            &lto6_backend(),
            false,
            EjectProbe {
                // The real node read, of a path that does not exist: if the
                // refusal came after it, the outcome would be NodeUnreadable.
                node: None,
                mam: chip(),
                identity: Some(hp_drive()),
                holders: None,
                unload: Some(&mut unload),
                sysfs_root: Some(env.sysfs.path()),
            },
        )
        .unwrap();
        assert_eq!(r.outcome, EjectOutcome::RefusedHeld, "{}", r.message);
        assert_eq!(eject_exit_code(&r), EXIT_DRIVE_BUSY);
        assert_eq!(unloads.get(), 0);
        assert_eq!(r.holders.len(), 1, "{:?}", r.holders);
        assert_eq!(r.holders[0].kind, "drive");
        assert_eq!(
            r.holders[0].pid,
            Some(std::process::id()),
            "/proc/locks names the holder"
        );
        assert!(
            r.message.contains(&format!("pid {}", std::process::id())),
            "{}",
            r.message
        );
        assert_eq!(eject_rows(&env.conn), 0, "a refusal records nothing");

        // Positive control: the holder lets go and the same eject goes through.
        drop(holder);
        let (unloads, during) = (Cell::new(0), Cell::new(None));
        let r = run_eject(&env, false, loaded(), chip(), &unloads, &during, Ok(()));
        assert_eq!(r.outcome, EjectOutcome::Ejected, "{}", r.message);
        assert_eq!(unloads.get(), 1);
    }

    /// A live volume session refuses too, named by the volume — a write holds
    /// its volume's lock before its contact (and the drive lock) exists.
    #[test]
    fn a_live_volume_session_refuses_an_eject() {
        let env = eject_env();
        let cart = register_cartridge(&env.conn, "CART01L6", "EW7VWMVKF6");
        put_volume_on(&env.conn, cart, "L6-0042", "initialized");
        let vol: i64 = env
            .conn
            .query_row("SELECT id FROM volumes WHERE label = 'L6-0042'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let _session = foreign_flock(&lock::volume_lock_path(&env.paths.db_file, vol));

        let (unloads, during) = (Cell::new(0), Cell::new(None));
        let r = run_eject(&env, false, loaded(), chip(), &unloads, &during, Ok(()));
        assert_eq!(r.outcome, EjectOutcome::RefusedHeld, "{}", r.message);
        assert_eq!(eject_exit_code(&r), 75);
        assert_eq!(unloads.get(), 0);
        assert_eq!(r.holders.len(), 1, "{:?}", r.holders);
        assert_eq!(r.holders[0].kind, "volume");
        assert_eq!(r.holders[0].name, "L6-0042");
        assert!(r.message.contains("L6-0042"), "{}", r.message);
        assert_eq!(
            eject_rows(&env.conn),
            0,
            "a refusal records nothing (and the binding is untouched)"
        );
    }

    /// `--dry-run` does every check and sends and records nothing — and takes
    /// no lock: the drive reads Free afterwards. A held drive is still refused
    /// by it, because that is what the real run would say.
    #[test]
    fn a_dry_run_reports_what_it_would_eject_and_changes_nothing() {
        let env = eject_env();
        let cart = register_cartridge(&env.conn, "CART01L6", "EW7VWMVKF6");
        put_volume_on(&env.conn, cart, "L6-0001", "sealed");
        let bound = bindings(&env.conn);

        let (unloads, during) = (Cell::new(0), Cell::new(None));
        let r = run_eject(&env, true, loaded(), chip(), &unloads, &during, Ok(()));
        assert_eq!(r.outcome, EjectOutcome::WouldEject, "{}", r.message);
        assert_eq!(eject_exit_code(&r), 0);
        assert!(r.dry_run);
        assert_eq!(unloads.get(), 0, "a dry run unloads nothing");
        assert_eq!(eject_rows(&env.conn), 0, "and records nothing");
        assert_eq!(bindings(&env.conn), bound);
        assert_eq!(
            lock::drive_lock_state(&env.paths.db_file, DEVICE),
            DriveLockState::Free,
            "and keeps no lock"
        );
        assert_eq!(r.cartridge.as_ref().unwrap().barcode, "CART01L6");
        assert!(r.message.starts_with("dry run:"), "{}", r.message);

        let holder = foreign_flock(&lock::drive_lock_path(&env.paths.db_file, DEVICE));
        let r = run_eject(&env, true, loaded(), chip(), &unloads, &during, Ok(()));
        assert_eq!(r.outcome, EjectOutcome::RefusedHeld);
        drop(holder);
    }

    /// A chip that will not answer does not stop the eject: the contact says
    /// why it names no cartridge, and the event hangs on the drive.
    #[test]
    fn an_unreadable_or_unregistered_chip_still_ejects_and_the_event_hangs_on_the_drive() {
        for mam in [Some(mam_read(None)), chip()] {
            let env = eject_env();
            let (unloads, during) = (Cell::new(0), Cell::new(None));
            let r = run_eject(&env, false, loaded(), mam, &unloads, &during, Ok(()));
            assert_eq!(r.outcome, EjectOutcome::Ejected, "{}", r.message);
            assert!(r.cartridge.is_none());
            let (cartridge, reason): (Option<i64>, Option<String>) = env
                .conn
                .query_row(
                    "SELECT cartridge_id, identity_reason FROM cartridge_contacts",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(cartridge, None);
            assert!(
                reason
                    .as_deref()
                    .is_some_and(|r| r.contains("medium serial")),
                "{reason:?}"
            );
            let (entity, label): (String, Option<String>) = env
                .conn
                .query_row("SELECT entity_type, entity_label FROM events", [], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })
                .unwrap();
            assert_eq!(entity, "drive");
            assert_eq!(label.as_deref(), Some("HUJ808A5L4"));
        }
    }

    #[test]
    fn the_eject_json_report_has_the_documented_shape() {
        let env = eject_env();
        let (unloads, during) = (Cell::new(0), Cell::new(None));
        let r = run_eject(&env, false, loaded(), chip(), &unloads, &during, Ok(()));
        let v = serde_json::to_value(&r).unwrap();
        for key in [
            "device",
            "backend",
            "dry_run",
            "outcome",
            "message",
            "holders",
            "chip_serial",
            "cartridge",
            "contact_id",
            "event_id",
            "warnings",
        ] {
            assert!(v.get(key).is_some(), "missing {key}: {v}");
        }
        assert_eq!(v["outcome"], "ejected");
        assert_eq!(v["chip_serial"], "EW7VWMVKF6");
    }
}
