use clap::Subcommand;
use rusqlite::Connection;
use serde::Serialize;
use tabled::{Table, Tabled};

use crate::cli::{read_device, write_device};
use crate::config::{Config, TapectlPaths};
use crate::error::{Result, TapectlError};
use crate::store::{TapeStore, Tier};
use crate::volume::write;

pub(crate) const DEFAULT_BLOCK_SIZE: usize = 512 * 1024; // 512 KB

#[derive(Subcommand, Debug)]
pub enum VolumeCommands {
    /// Initialize a new volume (write ID thunk to tape)
    Init {
        /// Volume label (e.g., L6-0001)
        label: String,
        /// Tape device (by-id path). Defaults to the only configured drive;
        /// required when more than one is configured.
        #[arg(long)]
        device: Option<String>,
        /// Overwrite a cartridge whose File 0 already identifies a
        /// DIFFERENT volume (e.g. a mislabeled or stale tape). Refused by
        /// default (issue #27) — loading the wrong cartridge would
        /// otherwise silently overwrite it. Never overrides a cartridge
        /// that is already SEALED (ADR-0003): bulk-erase the physical tape
        /// and run `cartridge mark-erased` first for that case. It also
        /// never overrides the drive/media compatibility refusal, which is
        /// a physical fact rather than a risk judgement (ADR-0010).
        #[arg(long)]
        force: bool,
        /// Declare the loaded medium's generation (e.g. LTO-6, LTO-7-M8).
        ///
        /// Normally unnecessary and normally ignored: ADR-0010 DETECTS the
        /// generation from the drive (MAM medium density code, else MAM
        /// format density code, else the st driver's density register), and
        /// a detected code is a fact about the tape that this flag cannot
        /// override — a `--generation` contradicting one is an error, not a
        /// hint. It is consulted only when no source reports a recognised code.
        #[arg(long)]
        generation: Option<String>,
        /// Bind this volume to an already-registered cartridge by barcode.
        ///
        /// Normally unnecessary: `volume init` matches the loaded medium's
        /// MAM serial to `cartridges.serial_number`, and auto-registers a
        /// cartridge (barcode = that serial) when nothing matches. Use this
        /// to attach the volume to a cartridge YOU labelled — the barcode on
        /// the physical sticker — instead. A serial match wins over this
        /// flag, since the serial was read off the medium.
        #[arg(long)]
        cartridge: Option<String>,
    },

    /// Write staged data to volume
    ///
    /// Refuses any volume not left `initialized` by `volume init` — a
    /// sealed, retired, erased or quarantined volume is not a write target
    /// (ADR-0012); no flag overrides it.
    Write {
        /// Volume label
        label: String,
        /// Tape device (by-id path). Defaults to the only configured drive;
        /// required when more than one is configured.
        #[arg(long)]
        device: Option<String>,
        /// See `volume init --force` — same override, same limits.
        #[arg(long)]
        force: bool,
        /// Seal slices that were staged before the escrow recipient existed
        /// (ADR-0005). Only reachable for tapes copied forward via
        /// `read-slices`/`compact-read`, since `stage create` refuses to
        /// stage without escrow; use it to migrate a dying pre-escrow
        /// cartridge, knowing the copy stays unrecoverable by the escrow key.
        #[arg(long)]
        allow_missing_escrow: bool,
    },

    /// Resume an interrupted write session (issue #25). Reload the SAME
    /// cartridge first: the session continues from its frozen staging files
    /// rather than rebuilding them, and it refuses (quarantining the volume)
    /// if the loaded tape's File 0 does not match, or if the tape is already
    /// sealed. There is no --force: `volume write --force` overrides a
    /// wrong-cartridge finding before anything is written, which has no
    /// meaning for a tape this session has already partly written.
    ///
    /// Refuses any volume not left `initialized` by `volume init` — a
    /// sealed, retired, erased or quarantined volume is not a write target
    /// (ADR-0012); no flag overrides it.
    Resume {
        /// Volume label
        label: String,
        /// Tape device (by-id path). Defaults to the only configured drive;
        /// required when more than one is configured.
        #[arg(long)]
        device: Option<String>,
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
        /// Tape device (by-id path). Defaults to the only configured drive;
        /// required when more than one is configured.
        #[arg(long)]
        device: Option<String>,
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
        /// Tape device (by-id path). Defaults to the only configured drive;
        /// required when more than one is configured.
        #[arg(long)]
        device: Option<String>,
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
        /// Tape device (by-id path). Defaults to the only configured drive;
        /// required when more than one is configured.
        #[arg(long)]
        device: Option<String>,
    },

    /// Read live encrypted slices from a volume to staging (compaction step 1)
    CompactRead {
        /// Source volume label
        label: String,
        /// Tape device (by-id path). Defaults to the only configured drive;
        /// required when more than one is configured.
        #[arg(long)]
        device: Option<String>,
    },

    /// Write compaction slices from staging to destination (compaction step 2)
    CompactWrite {
        /// Destination volume label
        #[arg(long)]
        destination: String,
        /// Tape device (by-id path). Defaults to the only configured drive;
        /// required when more than one is configured.
        #[arg(long)]
        device: Option<String>,
        /// See `volume write --allow-missing-escrow`. A compaction whose
        /// source volume predates the escrow recipient needs this to proceed.
        #[arg(long)]
        allow_missing_escrow: bool,
    },

    /// Show bin-packing plan for pending staged data
    Plan {
        /// Number of copies to plan
        #[arg(long, default_value = "1")]
        copies: i64,
        /// Estimate against this media generation rather than the drive's
        /// own (ADR-0010) — e.g. counting LTO-5 cartridges for an LTO-6
        /// drive. No cartridge need be loaded; this is an estimate, and the
        /// authoritative figure is each volume's own `capacity_bytes` once
        /// `volume init` has detected the medium it is actually on.
        #[arg(long)]
        generation: Option<String>,
        /// Which configured drive to plan against, by its device path. Only
        /// needed when more than one `[[backends.lto]]` is configured —
        /// without it, planning errored outright on a multi-drive config
        /// rather than asking.
        #[arg(long)]
        device: Option<String>,
    },

    /// Retire source volume after compaction (compaction step 3)
    CompactFinish {
        /// Source volume label to retire
        label: String,
        /// Waive the ADR-0008 Tier-2 prompt: proceed when the retirement
        /// leaves a live version below its policy but above zero. It
        /// defeats NEITHER Tier-3 refusal (issue #147) — a live slice with
        /// no copy on another volume, and the last eligible copy of a live
        /// version, each stop the retirement outright and no flag reaches
        /// them. See cli::consent.
        #[arg(long)]
        force: bool,
    },

    /// Interactive compaction: read + write + finish in one flow
    ///
    /// Step 3 applies `compact-finish`'s ADR-0008 Tier-2 gate and may
    /// refuse non-interactively without `--force`. When it does, the
    /// destination tape is already written and sealed and nothing is lost:
    /// `volume compact-finish <SOURCE> --force` completes the flow without
    /// re-reading or re-writing anything.
    Compact {
        /// Source volume label
        label: String,
        /// Destination volume label. Without it the flow PROMPTS for one,
        /// which needs a terminal — pass this to run compaction
        /// non-interactively (issue #146, ADR-0008's non-hanging rule).
        #[arg(long)]
        to: Option<String>,
        /// Tape device (by-id path). Defaults to the only configured drive;
        /// required when more than one is configured.
        #[arg(long)]
        device: Option<String>,
        /// See `volume write --allow-missing-escrow`.
        #[arg(long)]
        allow_missing_escrow: bool,
        /// See `volume compact-finish --force` — step 3's ADR-0008 Tier-2
        /// gate. With `--to` and this (or the global `--yes`), the whole
        /// three-step flow runs non-interactively.
        #[arg(long)]
        force: bool,
    },

    /// Record and inspect WAREHOUSE DEPOSITS of sealed volumes (ADR-0006).
    ///
    /// tapectl does NOT move the bytes. Issue #72 was rescoped by CTO
    /// decision: an operator copies a sealed volume's bytes to cold cloud
    /// storage by the documented external procedure (rclone / aws-cli) and
    /// then RECORDS that copy here, so the catalog can reason about it.
    Deposit {
        #[command(subcommand)]
        command: DepositCommands,
    },

    /// List every volume, most recently written first (issue #195).
    ///
    /// Catalog-only: never opens a drive. Every status is shown by default —
    /// ADR-0011: retired means unfit to WRITE, not unreadable ("a retired
    /// volume can still be restored from"), and the dangerous failure for an
    /// inventory is a tape you forgot you had. `--status` narrows; nothing is
    /// hidden without it.
    List {
        /// Only volumes in this status (blank, initialized, active, full,
        /// retired, missing, erased, sealed, quarantined). Every status is
        /// shown when omitted.
        #[arg(long)]
        status: Option<String>,
    },

    /// The dossier for one volume: capacity, media generation, cartridge
    /// binding, location, units carried, write receipts, verification
    /// history, warehouse deposits (issue #195).
    ///
    /// Catalog-only: never opens a drive. Summarises units carried by
    /// default — the design probes ~280 units per cartridge
    /// (docs/design/v2-open-questions.md:434) — pass `--units` to list every
    /// one instead of the largest few.
    Info {
        /// Volume label
        label: String,
        /// List every unit carried by this volume instead of the summary
        #[arg(long)]
        units: bool,
    },
}

/// `volume deposit` -- record and list warehouse copies (ADR-0006).
#[derive(Subcommand, Debug)]
pub enum DepositCommands {
    /// Record that a sealed volume's bytes now also exist at a warehouse.
    Add {
        /// Volume label whose bytes were deposited
        label: String,
        /// Warehouse location name (must be a location of kind `warehouse`)
        #[arg(long)]
        to: String,
        /// The provider's receipt / object-version identifier, if it gave
        /// one. There is deliberately no checksum field: tapectl did not
        /// perform the copy, so a typed-in checksum would be a claim about
        /// a claim (issue #73).
        #[arg(long)]
        receipt: Option<String>,
        /// Storage class the bytes were placed in (e.g. DEEP_ARCHIVE)
        #[arg(long)]
        storage_class: Option<String>,
        /// Free-text note
        #[arg(long)]
        notes: Option<String>,
    },
    /// List recorded deposits
    List {
        /// Only deposits of this volume
        #[arg(long)]
        volume: Option<String>,
    },
    /// Un-record a deposit whose bytes are gone from the warehouse.
    ///
    /// ADR-0006 names the reason this must exist: "a warehouse copy dies
    /// weeks after payment stops". A lapsed bill or a provider lifecycle
    /// rule deletes the object without telling tapectl, and a deposit row
    /// that outlives its bytes keeps counting as a copy at exactly the
    /// gates that decide whether local data may be deleted.
    Remove {
        /// Volume label whose deposit is gone
        label: String,
        /// Warehouse location the deposit was recorded at
        #[arg(long)]
        from: String,
    },
}

/// `volumes.status`'s CHECK constraint (`src/db/migrations/003_v2_lifecycle.sql`,
/// which extended 001's original set and is the schema's current word on it).
const VOLUME_STATUSES: &[&str] = &[
    "blank",
    "initialized",
    "active",
    "full",
    "retired",
    "missing",
    "erased",
    "sealed",
    "quarantined",
];

/// `volume list --status` is a usage error when it names anything other
/// than one of `VOLUME_STATUSES` (issue #171, ADR-0012) — `volumes.status`
/// never had an `offsite` value (a volume's place has always been
/// `location_id`, since `007_warehouse_locations.sql`), so this needs no
/// ADR-0011 special case the way `cartridge list --status` does.
fn validate_volume_status(value: &str) -> Result<()> {
    crate::config::validate_closed_set("--status", value, VOLUME_STATUSES)
        .map_err(TapectlError::Other)
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
            generation,
            cartridge,
        } => {
            let device = write_device(config, device.as_deref())?;
            let vol_id = write::volume_init(
                conn,
                config,
                label,
                &device,
                DEFAULT_BLOCK_SIZE,
                *force,
                generation.as_deref(),
                cartridge.as_deref(),
            )?;
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
            allow_missing_escrow,
        } => {
            let device = write_device(config, device.as_deref())?;
            write::volume_write(
                conn,
                paths,
                config,
                label,
                &device,
                DEFAULT_BLOCK_SIZE,
                *force,
                *allow_missing_escrow,
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
            let device = write_device(config, device.as_deref())?;
            write::volume_resume(conn, paths, config, label, &device, DEFAULT_BLOCK_SIZE)?;
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"label": label, "status": "completed", "resumed": true})
                );
            } else {
                println!("volume \"{label}\" write resumed and completed");
            }
        }

        // `*yes` alone, NOT `*yes || yes` — and that is correct, verified
        // empirically rather than inferred (issue #237, closed as
        // not-reproducible). This arm's local field and the global
        // `Cli::yes` share clap's default arg id (the field name `yes`), and
        // `global = true` unifies matches by id: `--yes` typed ANYWHERE sets
        // both fields, omitted leaves both false. An `||` here would OR two
        // values that are always equal — a no-op dressed as a fix.
        //
        // Written down because the SHAPE looks like the defect its siblings
        // really had: `CompactFinish`/`Compact` do `*force || yes`, and that
        // OR is load-bearing there only because `force` is a genuinely
        // different arg id. #237 was filed off that resemblance, by reading
        // source without testing behaviour. The three `volume_abort_*` tests
        // in `tests/cli_smoke.rs` pin the real end-to-end behaviour, so a
        // future rename that decoupled the ids goes red rather than silently
        // reintroducing the bug this comment says does not exist.
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
            let device = read_device(config, device.as_deref())?;
            let report =
                write::volume_verify(conn, config, label, &device, DEFAULT_BLOCK_SIZE, tier)?;
            if json_output {
                // Issue #142: `failed: 3` without naming the three is the
                // difference between an operator who knows what to re-copy
                // and one who re-copies a whole tape. This array is the
                // FULL evidence, including the metadata-position mismatches
                // that `verification_results` cannot store a row for.
                let mismatches: Vec<serde_json::Value> = report
                    .mismatches
                    .iter()
                    .map(|m| {
                        serde_json::json!({
                            "position": m.position,
                            "kind": m.kind.label(),
                            "expected": m.expected,
                            "actual": m.actual,
                            // Issue #234: per mismatch, which side of
                            // ADR-0012's line it falls on, so a script can
                            // tell "this tape is bad" from "this drive
                            // could not read it" without a kind allow-list
                            // of its own that would drift.
                            "proves_medium_bad": m.kind.proves_medium_bad(),
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::json!({
                        "label": label,
                        "tier": tier_name,
                        "checked": report.checked,
                        "passed": report.passed,
                        "failed": report.failed,
                        "mismatches": mismatches,
                        // Issue #234: `quarantined` is the headline answer;
                        // `quarantine` carries what the status WAS, because
                        // a volume that was already quarantined is a
                        // different fact from one this verify took out of
                        // service. `null` on a clean verify and on a failed
                        // one that proved nothing about the medium.
                        "quarantined": report.quarantine.is_some(),
                        "quarantine": report.quarantine.as_ref().map(|q| serde_json::json!({
                            "previous_status": q.previous_status,
                            "status_changed": q.status_changed(),
                            "proof": q.proof.iter().map(|m| serde_json::json!({
                                "position": m.position,
                                "kind": m.kind.label(),
                            })).collect::<Vec<_>>(),
                        })),
                        "drive_health_note": report.drive_health_note,
                    })
                );
            } else {
                println!(
                    "verify {label} ({tier_name} tier): {} checked, {} passed, {} failed",
                    report.checked, report.passed, report.failed,
                );
                for m in &report.mismatches {
                    // Issue #234: the classification, per line. Without it
                    // the operator has to know which of six kind names is
                    // quarantine-grade to read the summary below.
                    let verdict = if m.kind.proves_medium_bad() {
                        "proves the medium is bad"
                    } else {
                        "not medium evidence"
                    };
                    println!(
                        "    position {}: {} ({verdict}) — expected {}, found {}",
                        m.position,
                        m.kind.label(),
                        m.expected,
                        m.actual
                    );
                }
                // Issue #234 / ADR-0012's 2026-09-17 amendment: the
                // distinction must be visible. A clean verify says nothing
                // new; a failure always says which of the two it was.
                match (&report.quarantine, report.failed) {
                    (Some(q), _) => {
                        if q.status_changed() {
                            println!(
                                "volume \"{label}\" QUARANTINED (was {}): {} of {} failure(s) \
                                 prove the medium is bad. It no longer counts as a copy, so \
                                 `volume retire` will no longer refuse it as the last one — \
                                 salvage what still reads off it first \
                                 (`volume read-slices --from {label} --unit <UNIT>`).",
                                q.previous_status,
                                q.proof.len(),
                                report.failed,
                            );
                        } else {
                            println!(
                                "volume \"{label}\" was ALREADY quarantined; this verify \
                                 confirms it — {} of {} failure(s) prove the medium is bad.",
                                q.proof.len(),
                                report.failed,
                            );
                        }
                    }
                    (None, failed) if failed > 0 => {
                        println!(
                            "volume \"{label}\" NOT quarantined: no failure here proves the \
                             medium is bad — these are read or transport failures, and \"we \
                             could not read it today\" is not \"the bytes are gone\". The \
                             volume's status is unchanged. Check the drive (cleaning, block \
                             size, cabling, the right tape loaded) and verify again."
                        );
                    }
                    (None, _) => {}
                }
                // Issue #187: said out loud, not silently omitted.
                if let Some(note) = &report.drive_health_note {
                    println!("note: {note}");
                }
            }
            // issue #45/H10: a failing verify must not exit 0 — a
            // cron-scheduled integrity check that finds corruption but
            // reports success defeats the entire point of verifying.
            exit_code = verify_exit_code(&report);
        }

        VolumeCommands::Identify { device } => {
            let device = read_device(config, device.as_deref())?;
            // Issue #166: refuse before the store is opened if this drive
            // cannot read the loaded medium. Proceeds silently with no
            // configured backend (ADR-0010's DR-path leniency).
            crate::tape::media_detect::check_read_contact(config, &device)?;
            // Before the store open: reading the MAM opens the device
            // read-only and drops the fd, and the st driver refuses a second
            // concurrent open.
            let medium_serial = crate::volume::binding::loaded_medium_serial(config, &device);
            let mut store = TapeStore::open_read(&device, DEFAULT_BLOCK_SIZE)?;
            // Corroborated (ADR-0012, issue #193) where there is a catalog
            // row to compare against; the bare `volume_identify` stays the
            // DB-less File 0 reader the heir path mirrors.
            let id =
                write::volume_identify_corroborated(conn, &mut store, medium_serial.as_deref())?;
            // The tape's own account FIRST, always — see `Identified`. A
            // contradiction is reported after it and through the exit code,
            // never by withholding the answer the operator asked for.
            println!("{}", id.text);
            if let Some(why) = id.contradiction {
                eprintln!("\nwarning: this tape contradicts the catalog.\n{why}");
                return Err(TapectlError::Other(
                    "the loaded tape and the catalog disagree (above); the tape's own \
                     identity is printed unchanged"
                        .to_string(),
                ));
            }
        }

        VolumeCommands::Move { label, to } => {
            // Issue #230: the global `--dry-run` reached this function and
            // this arm never read it, so a dry run committed the move.
            // `move_volume` honours it inside the one shared mover
            // (`cli::location::move_together`), which is also why
            // `cartridge move` needed no second gate of its own. The dry
            // branch prints the outcome that WOULD have been written; the
            // real branch below is byte-for-byte what it always was.
            let outcome = crate::cli::location::move_volume(conn, label, to, dry_run)?;
            let others: Vec<&str> = outcome
                .volumes
                .iter()
                .filter(|l| l.as_str() != label)
                .map(|l| l.as_str())
                .collect();
            if json_output {
                // `cartridge`/`volumes_moved` are ADDITIVE (ADR-0011): the
                // move now carries the cartridge and any other volume on it,
                // and a consumer that only reads `label`/`location` sees
                // exactly what it saw before.
                let mut obj = serde_json::json!({
                    "label": label,
                    "location": to,
                    "cartridge": outcome.cartridge,
                    "volumes_moved": outcome.volumes,
                });
                if dry_run {
                    obj["dry_run"] = serde_json::json!(true);
                }
                println!("{obj}");
            } else if dry_run {
                println!(
                    "volume \"{label}\" would be moved to \"{to}\" (DRY RUN — no changes made)"
                );
                if let Some(barcode) = &outcome.cartridge {
                    println!("  cartridge \"{barcode}\" would move with it");
                    if !others.is_empty() {
                        println!(
                            "  {} other volume(s) on that cartridge would move too: {}",
                            others.len(),
                            others.join(", ")
                        );
                    }
                }
            } else {
                println!("volume \"{label}\" moved to \"{to}\"");
                if let Some(barcode) = &outcome.cartridge {
                    println!("  cartridge \"{barcode}\" moved with it");
                    if !others.is_empty() {
                        println!(
                            "  {} other volume(s) on that cartridge moved too: {}",
                            others.len(),
                            others.join(", ")
                        );
                    }
                }
            }
        }

        VolumeCommands::Retire { label } => {
            crate::cli::operations::volume_retire(conn, config, label, yes, dry_run, json_output)?;
        }

        VolumeCommands::ReadSlices { from, unit, device } => {
            let device = read_device(config, device.as_deref())?;
            // Issue #166: same fact check as `Identify`, before the store
            // is opened.
            crate::tape::media_detect::check_read_contact(config, &device)?;
            let medium_serial = crate::volume::binding::loaded_medium_serial(config, &device);
            let mut store = TapeStore::open_read(&device, DEFAULT_BLOCK_SIZE)?;
            let report = write::read_slices(
                conn,
                config,
                from,
                unit,
                &mut store,
                medium_serial.as_deref(),
            )?;
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
                    "read {} slices ({}) from \"{}\" into staging",
                    report.slices_read,
                    crate::util::format_bytes_binary(report.bytes_read),
                    from,
                );
                println!(
                    "run `tapectl volume write DEST --device {}` to write to tape",
                    device
                );
            }
        }

        VolumeCommands::Plan {
            copies,
            generation,
            device,
        } => {
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
                            "  {name} v{ver}: {} slices, {}",
                            slices.unwrap_or(0),
                            crate::util::format_bytes_binary(size.unwrap_or(0)),
                        );
                    }
                    println!(
                        "\ntotal: {total_slices} slices, {} x {copies} = {}",
                        crate::util::format_bytes_binary(total_bytes),
                        crate::util::format_bytes_binary(total_bytes * copies),
                    );
                    // Estimate tapes needed from the configured LTO backend.
                    // ADR-0010: the figure follows the GENERATION being
                    // planned for (`--generation`, else the drive's own), not a
                    // capacity declared on the drive.
                    let backend = crate::config::resolve_lto_backend(config, device.as_deref())?;
                    let tape_cap = backend.planning_capacity_bytes(generation.as_deref())? as i64;
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
            let device = read_device(config, device.as_deref())?;
            // Issue #166: same fact check as `Identify`, before the store
            // is opened.
            crate::tape::media_detect::check_read_contact(config, &device)?;
            let medium_serial = crate::volume::binding::loaded_medium_serial(config, &device);
            let mut store = TapeStore::open_read(&device, DEFAULT_BLOCK_SIZE)?;
            let report =
                write::compact_read(conn, config, label, &mut store, medium_serial.as_deref())?;
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"label": label, "slices_read": report.slices_read, "bytes_read": report.bytes_read})
                );
            } else {
                println!(
                    "compact-read \"{label}\": {} live slices ({}) staged",
                    report.slices_read,
                    crate::util::format_bytes_binary(report.bytes_read),
                );
            }
        }

        VolumeCommands::CompactWrite {
            destination,
            device,
            allow_missing_escrow,
        } => {
            let device = write_device(config, device.as_deref())?;
            write::compact_write(
                conn,
                paths,
                config,
                destination,
                &device,
                DEFAULT_BLOCK_SIZE,
                *allow_missing_escrow,
            )?;
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"destination": destination, "status": "completed"})
                );
            } else {
                println!("compact-write to \"{destination}\" completed");
            }
        }

        VolumeCommands::CompactFinish { label, force } => {
            let report = write::compact_finish(conn, config, label, *force || yes)?;
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

        VolumeCommands::Compact {
            label,
            to,
            device,
            allow_missing_escrow,
            force,
        } => {
            // Interactive: run all 3 steps. Strict resolution (ADR-0010):
            // step 2 writes, so this needs a real backend even though step 1
            // only reads.
            let device = write_device(config, device.as_deref())?;
            // Issue #166: step 1 only reads, so it gets the same fact check
            // as every other read path, before its store is opened.
            crate::tape::media_detect::check_read_contact(config, &device)?;
            println!("=== Step 1: Reading live slices from \"{label}\" ===");
            // Scoped so the read-only store (and its device fd) closes
            // before step 2 opens the same device for writing — the st
            // driver refuses a second concurrent open (EBUSY).
            let medium_serial = crate::volume::binding::loaded_medium_serial(config, &device);
            let report = {
                let mut store = TapeStore::open_read(&device, DEFAULT_BLOCK_SIZE)?;
                write::compact_read(conn, config, label, &mut store, medium_serial.as_deref())?
            };
            println!(
                "  Read {} slices ({})",
                report.slices_read,
                crate::util::format_bytes_binary(report.bytes_read),
            );

            let dest_label = resolve_compact_destination(to.as_deref())?;
            let dest_label = dest_label.as_str();

            println!("=== Step 2: Writing compaction slices to \"{dest_label}\" ===");
            write::compact_write(
                conn,
                paths,
                config,
                dest_label,
                &device,
                DEFAULT_BLOCK_SIZE,
                *allow_missing_escrow,
            )?;
            println!("  Write completed");

            println!("=== Step 3: Retiring source volume \"{label}\" ===");
            // Step 2 has already written and sealed the destination by now,
            // so a step-3 consent refusal must not read as "the compaction
            // failed" — it read as that in every earlier draft, and the
            // operator's rational response would have been to redo a
            // multi-hour write that had already succeeded.
            //
            // A PRE-FLIGHT gate was considered and rejected: before step 2
            // the destination holds nothing, so `retire_impacts` reports
            // ZERO other copies for every LIVE unit on the source (its only
            // copy IS the source, which is what compaction is about to fix).
            // Gating there would demand `--force` for ordinary compaction
            // and invert the gate's meaning. Only after step 2 does the
            // at-risk set narrow to units whose content was not carried
            // forward — exactly the issue #147 case that should gate.
            let report =
                write::compact_finish(conn, config, label, *force || yes).inspect_err(|e| {
                    // ONLY the consent refusal. `compact_finish`'s other two
                    // failures are its Tier-3 refusals (an unprotected live
                    // slice; the last eligible copy of a live version), and
                    // after a successful compact-write either means content was
                    // not carried forward — a bug, not a `--force` situation.
                    // Neither refusal's text contains these substrings, so
                    // neither can reach this hint; naming `--force` as the
                    // recovery for an absolute floor is precisely the confusion
                    // ADR-0008 warns about.
                    let msg = e.to_string();
                    if msg.contains("refused: non-interactive session")
                        || msg.contains("aborted, not confirmed")
                    {
                        eprintln!(
                            "\nNothing was lost: destination \"{dest_label}\" is written and \
                         sealed, and source \"{label}\" is simply not retired yet.\n\
                         To complete step 3 without re-reading or re-writing anything:\n    \
                         tapectl volume compact-finish {label} --force"
                        );
                    }
                })?;
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

        VolumeCommands::Deposit { command } => run_deposit(conn, command, json_output)?,

        VolumeCommands::List { status } => {
            if let Some(s) = status {
                validate_volume_status(s)?;
            }
            let rows = volume_rows(conn, status.as_deref())?;
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&volume_rows_to_json(&rows)).unwrap()
                );
            } else if rows.is_empty() {
                println!("no volumes");
            } else {
                println!("{}", Table::new(rows));
            }
        }

        VolumeCommands::Info { label, units } => {
            let info = volume_info(conn, label, *units)?;
            if json_output {
                println!("{}", serde_json::to_string_pretty(&info).unwrap());
            } else {
                print_volume_info(&info);
            }
        }
    }
    Ok(exit_code)
}

/// `volume deposit` (ADR-0006, issue #73).
///
/// Two refusals, and only two. Both are validation of the recorded FACT,
/// not policy gates -- ADR-0004 keeps every coverage judgement advisory,
/// so nothing here warns, blocks, or grows a `--force`:
///
/// 1. The target location must be a `warehouse`. Recording a deposit at a
///    shelf would claim a copy exists in a place nothing was copied to,
///    and it would then be counted as one by every derivation.
/// 2. The volume must pass `coverage::eligible` (sealed). You cannot have
///    deposited bytes that were never sealed -- an unsealed volume's bytes
///    are not final, so a copy of them is a copy of nothing durable.
fn run_deposit(conn: &Connection, command: &DepositCommands, json_output: bool) -> Result<()> {
    use crate::error::TapectlError;
    use rusqlite::params;

    match command {
        DepositCommands::Add {
            label,
            to,
            receipt,
            storage_class,
            notes,
        } => {
            let (vol_id, status): (i64, String) = conn
                .query_row(
                    "SELECT id, status FROM volumes WHERE label = ?1",
                    params![label],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(|_| TapectlError::VolumeNotFound(label.clone()))?;

            let (loc_id, kind): (i64, String) = conn
                .query_row(
                    "SELECT id, kind FROM locations WHERE name = ?1",
                    params![to],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(|_| TapectlError::Other(format!("location \"{to}\" not found")))?;

            if kind != "warehouse" {
                return Err(TapectlError::Other(format!(
                    "location \"{to}\" is a {kind}, not a warehouse; a deposit records bytes \
                     copied to cold cloud storage. Create one with: \
                     tapectl location add <NAME> --kind warehouse"
                )));
            }
            if status != "sealed" {
                return Err(TapectlError::Other(format!(
                    "volume \"{label}\" is {status}, not sealed; only a sealed volume's bytes \
                     are final, so there is nothing durable to have deposited"
                )));
            }

            conn.execute(
                "INSERT INTO volume_deposits (volume_id, location_id, receipt, storage_class, notes)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![vol_id, loc_id, receipt, storage_class, notes],
            )?;
            let id = conn.last_insert_rowid();
            crate::db::events::log_created(conn, "volume_deposit", id, label, None)?;

            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"id": id, "volume": label, "location": to,
                                       "receipt": receipt, "storage_class": storage_class})
                );
            } else {
                println!(
                    "recorded warehouse deposit of \"{label}\" at \"{to}\" (id={id}) — \
                     never re-verified, and warehouse copies do not refresh"
                );
            }
        }

        DepositCommands::List { volume } => {
            let mut sql = String::from(
                "SELECT v.label, l.name, d.deposited_at, d.receipt, d.storage_class, d.notes
                 FROM volume_deposits d
                 JOIN volumes v ON v.id = d.volume_id
                 JOIN locations l ON l.id = d.location_id",
            );
            let mut binds: Vec<String> = Vec::new();
            if let Some(v) = volume {
                sql.push_str(" WHERE v.label = ?1");
                binds.push(v.clone());
            }
            sql.push_str(" ORDER BY v.label, l.name");
            let mut stmt = conn.prepare(&sql)?;
            let bind_refs: Vec<&dyn rusqlite::types::ToSql> = binds
                .iter()
                .map(|b| b as &dyn rusqlite::types::ToSql)
                .collect();
            let rows: Vec<serde_json::Value> = stmt
                .query_map(bind_refs.as_slice(), |row| {
                    Ok(serde_json::json!({
                        "volume": row.get::<_, String>(0)?,
                        "location": row.get::<_, String>(1)?,
                        "deposited_at": row.get::<_, String>(2)?,
                        "receipt": row.get::<_, Option<String>>(3)?,
                        "storage_class": row.get::<_, Option<String>>(4)?,
                        "notes": row.get::<_, Option<String>>(5)?,
                    }))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;

            if json_output {
                println!("{}", serde_json::to_string_pretty(&rows).unwrap());
            } else if rows.is_empty() {
                println!("no warehouse deposits recorded");
            } else {
                for r in &rows {
                    println!(
                        "  {} at {} — deposited {}{}",
                        r["volume"].as_str().unwrap_or("?"),
                        r["location"].as_str().unwrap_or("?"),
                        r["deposited_at"].as_str().unwrap_or("?"),
                        r["receipt"]
                            .as_str()
                            .map(|x| format!(", receipt {x}"))
                            .unwrap_or_default(),
                    );
                }
            }
        }

        DepositCommands::Remove { label, from } => {
            let deleted = conn.execute(
                "DELETE FROM volume_deposits
                  WHERE volume_id = (SELECT id FROM volumes WHERE label = ?1)
                    AND location_id = (SELECT id FROM locations WHERE name = ?2)",
                params![label, from],
            )?;
            if deleted == 0 {
                return Err(TapectlError::Other(format!(
                    "no recorded deposit of \"{label}\" at \"{from}\""
                )));
            }
            crate::db::events::log_event(
                conn,
                "volume_deposit",
                0,
                Some(label),
                "deleted",
                None,
                None,
                None,
                Some(&format!("deposit at {from} un-recorded")),
                None,
            )?;

            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"volume": label, "location": from, "removed": deleted})
                );
            } else {
                println!(
                    "removed the recorded deposit of \"{label}\" at \"{from}\"; \
                     it no longer counts as a copy"
                );
            }
        }
    }
    Ok(())
}

/// ADR-0004 Tier 1: print the remaining-coverage evidence for every unit
/// `compact_finish` retired coverage for. Display-only, matching
/// `cli::operations::print_retire_impact`'s evidence lines -- compaction
/// retires the source volume exactly like `volume retire` does.
/// The compaction destination label, resolved WITHOUT ever blocking on a
/// prompt nobody can answer (issue #146).
///
/// `volume compact`'s step-2 destination used to be a bare
/// `stdin().read_line()` with no terminal check, and the global `--yes` did
/// not skip it — so a cron job, the mhvtl verify gate, or any redirected
/// run hung here forever. That is precisely the failure shape ADR-0008
/// names ("blocking forever on a handle that will never produce input",
/// the issue #33 class), and its rule is absolute: refuse with a non-zero
/// exit rather than wait.
///
/// This is NOT routed through `cli::consent::confirm`. That is the Tier-2
/// y/N gate; this is a VALUE the operator has to supply, and there is no
/// safe default to assume — `--yes` cannot invent a label. So the terminal
/// check is the same (`std::io::IsTerminal`, no new dependency) and the
/// override is `--to`.
fn resolve_compact_destination(to: Option<&str>) -> Result<String> {
    use std::io::IsTerminal;
    resolve_compact_destination_with(to, std::io::stdin().is_terminal(), || {
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        Ok(input)
    })
}

/// [`resolve_compact_destination`] with the TTY answer and the read
/// injected, so the non-interactive branch can be proven never to attempt
/// the read that would hang — the same testing discipline as
/// `cli::consent::confirm_with`.
fn resolve_compact_destination_with(
    to: Option<&str>,
    is_tty: bool,
    read_answer: impl FnOnce() -> Result<String>,
) -> Result<String> {
    if let Some(label) = to {
        let label = label.trim();
        if label.is_empty() {
            return Err(crate::error::TapectlError::Other(
                "--to was given an empty destination label".into(),
            ));
        }
        return Ok(label.to_string());
    }

    if !is_tty {
        return Err(crate::error::TapectlError::Other(
            "volume compact refused: a destination label is required and stdin is not a \
             terminal, so there is nobody to prompt — re-run with `--to <LABEL>` (the \
             destination tape must already be initialised)"
                .into(),
        ));
    }

    println!("\nInsert destination tape and enter volume label:");
    let input = read_answer()?;
    let label = input.trim();
    if label.is_empty() {
        return Err(crate::error::TapectlError::Other(
            "no destination label provided".into(),
        ));
    }
    Ok(label.to_string())
}

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
                .map(crate::cli::operations::evidence_json)
                .collect();
            let evidence_summary =
                crate::policy::evidence::describe(&unit.unit_name, &unit.evidence, now);
            // `status`/`remaining_copies` are ADDITIVE (issue #147), and
            // named exactly as `operations::retire_impacts_json` names the
            // same two facts — the shapes come from one derivation now, so
            // they should read alike.
            serde_json::json!({
                "unit": unit.unit_name,
                "status": unit.unit_status,
                "remaining_copies": unit.other_copies,
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
///
/// **Issue #234 deliberately does NOT change this.** A verify now has two
/// distinguishable failure outcomes — one that quarantined the volume and
/// one that did not — and neither changes the answer to the question this
/// function asks. A read or transport failure is still a failure, so
/// dropping it to `EXIT_SUCCESS` would resurrect issue #45/H10 exactly (a
/// cron-scheduled integrity check that finds nothing readable and reports
/// success); and giving a quarantine a third code would invent a CLI
/// contract nobody asked for, when the distinction is already carried where
/// the amendment requires it — in the printed summary and in `--json`'s
/// `quarantined` / `quarantine` fields.
fn verify_exit_code(report: &write::VerifyReport) -> i32 {
    if report.failed > 0 {
        crate::error::EXIT_ERROR
    } else {
        crate::error::EXIT_SUCCESS
    }
}

// ─────────────────────── `volume list` (issue #195) ───────────────────────

/// One row of `volume list`. `#[derive(Tabled, Serialize)]` per the C2b
/// discipline (`docs/design-errata.md`): the same struct backs the table and
/// `--json`, every table column is a JSON key, and the stored value is the
/// raw fact — `display_with` renders it for the table only.
#[derive(Tabled, Serialize)]
struct VolumeRow {
    #[tabled(rename = "LABEL")]
    label: String,
    #[tabled(rename = "STATUS")]
    status: String,
    /// The cartridge this volume is bound to, by barcode
    /// (`cartridge_volumes`, ADR-0010). `None` for a volume `volume init`
    /// left unbound (no readable medium serial) — rendered "(unbound)",
    /// matching the vocabulary `volume/binding.rs` already uses for this
    /// state.
    #[tabled(rename = "CARTRIDGE", display_with = "display_cartridge")]
    cartridge: Option<String>,
    /// `volumes.location_id` is nullable and every imported volume has none
    /// (rule #3) — rendered "(not placed)", matching `cartridge.rs:315`'s
    /// wording for the same fact about a cartridge.
    #[tabled(rename = "LOCATION", display_with = "display_not_placed")]
    location: Option<String>,
    /// The worst (ADR-0012 per-version MIN) current coverage among the
    /// units this volume carries, via `policy::coverage::copy_count_expr` —
    /// never re-derived (rule #4). `None` when the volume carries no unit
    /// yet (e.g. freshly `initialized`), rendered "—": that is a different
    /// fact from a unit really having zero copies, which renders `0`.
    ///
    /// The header is "MIN COPIES", not "COPIES", deliberately. This is a
    /// fact about the least-covered UNIT on this tape, not about the tape:
    /// a volume is one physical object and "copies of a volume" is not a
    /// meaningful quantity. Under a bare "COPIES" header the cell reads as
    /// a property of the row it sits in — the exact failure #91 records,
    /// where a coverage string that was true beside its context asserted
    /// something false when read alone. The question this answers is "if I
    /// lose this tape, how thin does anything on it get".
    #[tabled(rename = "MIN COPIES", display_with = "display_copies")]
    copies: Option<i64>,
    /// This volume's own most recent PASSED `verification_sessions` row
    /// (raw timestamp; `None` = never verified). Deliberately a fresh
    /// per-volume query rather than `policy::evidence` (see
    /// `remaining_coverage_evidence`'s doc): that module's queries are
    /// scoped to a UNIT's coverage across many volumes and would require a
    /// unit to correlate against, where this is a single volume's own
    /// verification history — a different question at a different
    /// granularity, so it is not "copying `audit.rs`'s trap," it is simply
    /// out of that module's scope.
    ///
    /// Rendered via `Self::display_verified` (not a plain field function)
    /// because the table cell also needs `copies`: a volume that has never
    /// carried any data renders "—" (verification is not yet a meaningful
    /// question), which must read as visibly different from "never" — a
    /// volume WITH data nobody has checked (rule #6).
    #[tabled(rename = "VERIFIED", display_with("Self::display_verified", self))]
    verified: Option<String>,
}

impl VolumeRow {
    fn display_verified(&self) -> String {
        if self.copies.is_none() {
            "—".to_string()
        } else {
            crate::policy::evidence::compact_age(
                self.verified.as_deref(),
                chrono::Utc::now().naive_utc(),
            )
        }
    }
}

fn display_cartridge(v: &Option<String>) -> String {
    v.clone().unwrap_or_else(|| "(unbound)".to_string())
}

fn display_not_placed(v: &Option<String>) -> String {
    v.clone().unwrap_or_else(|| "(not placed)".to_string())
}

fn display_copies(v: &Option<i64>) -> String {
    v.map(|n| n.to_string()).unwrap_or_else(|| "—".to_string())
}

/// The pure half of [`display_verified`], taking `now` as a parameter so the
/// never-vs-aged rendering is deterministically testable (same split as
/// `policy::evidence::describe`). An unparseable stamp renders raw, matching
/// that module's honesty rule rather than silently reading as "never".
/// `volume list --json` shape (rule #7: raw values, the render functions are
/// table-only).
fn volume_rows_to_json(rows: &[VolumeRow]) -> serde_json::Value {
    serde_json::to_value(rows).unwrap()
}

/// The query behind `volume list`, split out from the printing so the shape
/// is directly assertable in tests without capturing stdout (same pattern
/// as `cartridge_rows`/`report::copies_rows`).
///
/// Every status is returned by default (rule #2/#96) — the caller supplies
/// `status` only to narrow, and it is bound, never interpolated (issue
/// #110's precedent). `LEFT JOIN`s throughout: `cartridge_volumes`,
/// `cartridges` and `locations` are all optional facts about a volume (rules
/// #1/#3), so an unbound or unplaced volume must still appear rather than
/// vanish behind an inner join.
fn volume_rows(conn: &Connection, status: Option<&str>) -> Result<Vec<VolumeRow>> {
    let scope = crate::policy::coverage::CoverageQuery::current_unit("cu.id");
    let copy_expr = crate::policy::coverage::copy_count_expr(&scope);
    // "Copies" here is the WORST current coverage (ADR-0012: a unit is as
    // covered as its least-covered live version) among every unit this
    // volume carries ANY completed write for — not just its current
    // snapshot's writes, so a volume holding only a superseded version
    // still shows a real number rather than "—", which must mean "no data
    // at all" (a blank/initialized tape), a genuinely different fact.
    // `copy_count_expr` already folds the per-version MIN in per unit; the
    // outer `MIN` here takes the worst across the (possibly several) units
    // bin-packed onto this one volume.
    let mut sql = format!(
        "SELECT v.label, v.status, c.barcode, l.name,
                (SELECT MIN(per.copies) FROM (
                    SELECT DISTINCT cu.id, ({copy_expr}) AS copies
                    FROM writes cw2
                    JOIN stage_sets css2 ON css2.id = cw2.stage_set_id
                    JOIN snapshots cs2 ON cs2.id = css2.snapshot_id
                    JOIN units cu ON cu.id = cs2.unit_id
                    WHERE cw2.volume_id = v.id AND cw2.status = 'completed'
                 ) per) AS copies,
                (SELECT MAX(vs.completed_at) FROM verification_sessions vs
                  WHERE vs.volume_id = v.id AND vs.outcome = 'passed') AS last_verified
         FROM volumes v
         LEFT JOIN cartridge_volumes cv ON cv.volume_id = v.id
         LEFT JOIN cartridges c ON c.id = cv.cartridge_id
         LEFT JOIN locations l ON l.id = v.location_id"
    );
    if status.is_some() {
        sql.push_str(" WHERE v.status = ?1");
    }
    sql.push_str(" ORDER BY COALESCE(v.first_write, v.created_at) DESC, v.id DESC");

    let mut stmt = conn.prepare(&sql)?;
    let bound: Vec<&dyn rusqlite::types::ToSql> = match &status {
        Some(s) => vec![s],
        None => vec![],
    };
    let rows = stmt
        .query_map(bound.as_slice(), |row| {
            Ok(VolumeRow {
                label: row.get(0)?,
                status: row.get(1)?,
                cartridge: row.get(2)?,
                location: row.get(3)?,
                copies: row.get(4)?,
                verified: row.get(5)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

// ─────────────────────── `volume info` (issue #195) ───────────────────────

/// One unit this volume carries, aggregated across every completed write of
/// it that landed on this volume (a unit can be written to the same volume
/// more than once, e.g. successive snapshots or a compaction destination).
#[derive(Debug, Clone, Serialize)]
struct UnitOnVolume {
    unit: String,
    tenant: String,
    /// The highest version of this unit carried by this volume.
    version: i64,
    bytes: i64,
}

/// One `writes` row: a receipt that a specific stage set was written to
/// this volume, regardless of outcome — `volume info` shows the write
/// history, not just the successes.
#[derive(Debug, Clone, Serialize)]
struct WriteReceipt {
    unit: String,
    version: i64,
    status: String,
    started_at: Option<String>,
    completed_at: Option<String>,
    num_slices: Option<i64>,
    bytes: Option<i64>,
}

/// One `verification_sessions` row. Every outcome is shown here (unlike
/// `VolumeRow::verified`, which is deliberately the latest PASSED session
/// only) — a dossier's verification history is exactly the place a failed
/// or aborted attempt belongs.
#[derive(Debug, Clone, Serialize)]
struct VerificationRow {
    started_at: String,
    completed_at: Option<String>,
    verify_type: String,
    outcome: String,
    slices_checked: i64,
    slices_passed: i64,
    slices_failed: i64,
}

/// One recorded warehouse deposit (ADR-0006) of this volume's bytes — its
/// own evidence class, never folded into a copy count (rule #5).
#[derive(Debug, Clone, Serialize)]
struct DepositRow {
    location: String,
    deposited_at: String,
    receipt: Option<String>,
    storage_class: Option<String>,
    notes: Option<String>,
}

/// The dossier behind `volume info`. One struct backs both the plain-text
/// print and `--json` (rule #7): nothing is computed twice, so the two
/// renderings cannot drift apart.
///
/// Summarised by default (rule #8 / issue #195: the design probe models
/// ~280 units per cartridge) — `unit_count`/`unit_total_bytes`/`tenants`/
/// `largest_units` are always populated; `units` is `Some` only when
/// `--units` asked for the full list.
#[derive(Debug, Serialize)]
struct VolumeInfo {
    label: String,
    status: String,
    backend_type: String,
    backend_name: String,
    media_type: Option<String>,
    capacity_bytes: i64,
    bytes_written: i64,
    location: Option<String>,
    cartridge_barcode: Option<String>,
    cartridge_serial: Option<String>,
    created_at: String,
    first_write: Option<String>,
    last_write: Option<String>,
    notes: Option<String>,
    unit_count: i64,
    unit_total_bytes: i64,
    tenants: Vec<String>,
    units_first_seen: Option<String>,
    units_last_seen: Option<String>,
    /// The largest few units by bytes carried (top 5), shown regardless of
    /// `--units` so the summary is never empty just because the full list
    /// was not requested.
    largest_units: Vec<UnitOnVolume>,
    /// Every unit carried, only when `--units` was passed.
    units: Option<Vec<UnitOnVolume>>,
    writes: Vec<WriteReceipt>,
    verifications: Vec<VerificationRow>,
    deposits: Vec<DepositRow>,
}

/// How many of a volume's largest units are named in the summary before
/// `--units` is needed to see the rest.
const VOLUME_INFO_SUMMARY_UNITS: usize = 5;

/// Gather the `volume info` dossier for `label`. Catalog-only (rule #1): no
/// tape device, no `Store`, no `media_detect` — every field comes from the
/// database as it was last recorded (rule #8 in the issue: "last known,
/// never live").
fn volume_info(conn: &Connection, label: &str, include_units: bool) -> Result<VolumeInfo> {
    use crate::error::TapectlError;

    #[allow(clippy::type_complexity)]
    let (
        vol_id,
        status,
        backend_type,
        backend_name,
        media_type,
        capacity_bytes,
        bytes_written,
        created_at,
        first_write,
        last_write,
        notes,
        location,
        cartridge_barcode,
        cartridge_serial,
    ): (
        i64,
        String,
        String,
        String,
        Option<String>,
        i64,
        i64,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = conn
        .query_row(
            // LEFT JOINs throughout: a freshly initialized volume has no
            // cartridge_volumes/location row yet, and it must still be
            // inspectable (mirrors `cartridge info`'s LEFT JOIN for the
            // same reason).
            "SELECT v.id, v.status, v.backend_type, v.backend_name, v.media_type,
                    v.capacity_bytes, v.bytes_written, v.created_at, v.first_write,
                    v.last_write, v.notes, l.name, c.barcode, c.serial_number
             FROM volumes v
             LEFT JOIN locations l ON l.id = v.location_id
             LEFT JOIN cartridge_volumes cv ON cv.volume_id = v.id
             LEFT JOIN cartridges c ON c.id = cv.cartridge_id
             WHERE v.label = ?1",
            rusqlite::params![label],
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
                    row.get(11)?,
                    row.get(12)?,
                    row.get(13)?,
                ))
            },
        )
        .map_err(|_| TapectlError::VolumeNotFound(label.to_string()))?;

    // Units carried: one row per unit, aggregated across every completed
    // write of it that landed on THIS volume. Sorted largest-first so the
    // summary's "top few" is just this list's head.
    let mut units_stmt = conn.prepare(
        "SELECT u.name, t.name, MAX(s.version),
                COALESCE(SUM(ss.total_encrypted_size), 0),
                MIN(s.created_at), MAX(s.created_at)
         FROM writes w
         JOIN stage_sets ss ON ss.id = w.stage_set_id
         JOIN snapshots s ON s.id = ss.snapshot_id
         JOIN units u ON u.id = s.unit_id
         JOIN tenants t ON t.id = u.tenant_id
         WHERE w.volume_id = ?1 AND w.status = 'completed'
         GROUP BY u.id
         ORDER BY 4 DESC, u.name",
    )?;
    #[allow(clippy::type_complexity)]
    let unit_rows: Vec<(String, String, i64, i64, Option<String>, Option<String>)> = units_stmt
        .query_map(rusqlite::params![vol_id], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let unit_count = unit_rows.len() as i64;
    let unit_total_bytes = unit_rows.iter().map(|(_, _, _, bytes, _, _)| bytes).sum();
    let mut tenants: Vec<String> = unit_rows
        .iter()
        .map(|(_, tenant, ..)| tenant.clone())
        .collect();
    tenants.sort();
    tenants.dedup();
    let units_first_seen = unit_rows
        .iter()
        .filter_map(|(_, _, _, _, first, _)| first.clone())
        .min();
    let units_last_seen = unit_rows
        .iter()
        .filter_map(|(_, _, _, _, _, last)| last.clone())
        .max();
    let all_units: Vec<UnitOnVolume> = unit_rows
        .into_iter()
        .map(|(unit, tenant, version, bytes, _, _)| UnitOnVolume {
            unit,
            tenant,
            version,
            bytes,
        })
        .collect();
    let largest_units: Vec<UnitOnVolume> = all_units
        .iter()
        .take(VOLUME_INFO_SUMMARY_UNITS)
        .cloned()
        .collect();
    let units = if include_units { Some(all_units) } else { None };

    // Write receipts: every write of this volume, whatever its outcome —
    // this is the history, not the coverage derivation.
    let mut writes_stmt = conn.prepare(
        "SELECT u.name, s.version, w.status, w.started_at, w.completed_at,
                ss.num_slices, ss.total_encrypted_size
         FROM writes w
         JOIN stage_sets ss ON ss.id = w.stage_set_id
         JOIN snapshots s ON s.id = ss.snapshot_id
         JOIN units u ON u.id = s.unit_id
         WHERE w.volume_id = ?1
         ORDER BY w.completed_at DESC, w.id DESC",
    )?;
    let writes: Vec<WriteReceipt> = writes_stmt
        .query_map(rusqlite::params![vol_id], |row| {
            Ok(WriteReceipt {
                unit: row.get(0)?,
                version: row.get(1)?,
                status: row.get(2)?,
                started_at: row.get(3)?,
                completed_at: row.get(4)?,
                num_slices: row.get(5)?,
                bytes: row.get(6)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    // Verification history: every session, every outcome (unlike
    // `VolumeRow::verified`, which is the latest PASSED one only).
    let mut verify_stmt = conn.prepare(
        "SELECT started_at, completed_at, verify_type, outcome,
                slices_checked, slices_passed, slices_failed
         FROM verification_sessions
         WHERE volume_id = ?1
         ORDER BY started_at DESC, id DESC",
    )?;
    let verifications: Vec<VerificationRow> = verify_stmt
        .query_map(rusqlite::params![vol_id], |row| {
            Ok(VerificationRow {
                started_at: row.get(0)?,
                completed_at: row.get(1)?,
                verify_type: row.get(2)?,
                outcome: row.get(3)?,
                slices_checked: row.get(4)?,
                slices_passed: row.get(5)?,
                slices_failed: row.get(6)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    // Warehouse deposits (ADR-0006): their own evidence class, listed by
    // name, never folded into a copy count.
    let mut deposit_stmt = conn.prepare(
        "SELECT l.name, d.deposited_at, d.receipt, d.storage_class, d.notes
         FROM volume_deposits d
         JOIN locations l ON l.id = d.location_id
         WHERE d.volume_id = ?1
         ORDER BY d.deposited_at",
    )?;
    let deposits: Vec<DepositRow> = deposit_stmt
        .query_map(rusqlite::params![vol_id], |row| {
            Ok(DepositRow {
                location: row.get(0)?,
                deposited_at: row.get(1)?,
                receipt: row.get(2)?,
                storage_class: row.get(3)?,
                notes: row.get(4)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    Ok(VolumeInfo {
        label: label.to_string(),
        status,
        backend_type,
        backend_name,
        media_type,
        capacity_bytes,
        bytes_written,
        location,
        cartridge_barcode,
        cartridge_serial,
        created_at,
        first_write,
        last_write,
        notes,
        unit_count,
        unit_total_bytes,
        tenants,
        units_first_seen,
        units_last_seen,
        largest_units,
        units,
        writes,
        verifications,
        deposits,
    })
}

fn print_volume_info(info: &VolumeInfo) {
    println!("Volume: {}", info.label);
    println!("  Status:      {}", info.status);
    println!(
        "  Backend:     {} ({})",
        info.backend_type, info.backend_name
    );
    println!(
        "  Media:       {}",
        info.media_type.as_deref().unwrap_or("(unknown)")
    );
    // Issue #204: `capacity_bytes` is decimal by ADR-0012 ruling and stored
    // decimal (the generation table's LTO-6 = 2_500_000_000_000) — dividing
    // it by 1024^3 and calling the result GB, as this line used to, was the
    // same class of wrong number `cartridge info` had (an ~7% understate),
    // not just a mislabeling. `format_capacity_progress` renders the
    // written side binary (a measured data size) and the capacity side
    // decimal (a marketed figure) from the shared, tested helper, so the
    // two cannot silently converge on one wrong unit again.
    println!(
        "  Capacity:    {}",
        crate::util::format_capacity_progress(info.bytes_written, info.capacity_bytes)
    );
    println!(
        "  Cartridge:   {}",
        info.cartridge_barcode.as_deref().unwrap_or("(unbound)")
    );
    if let Some(serial) = &info.cartridge_serial {
        println!("               serial {serial}");
    }
    println!(
        "  Location:    {}",
        info.location.as_deref().unwrap_or("(not placed)")
    );
    println!("  Created:     {}", info.created_at);
    println!(
        "  First write: {}",
        info.first_write.as_deref().unwrap_or("(never written)")
    );
    println!(
        "  Last write:  {}",
        info.last_write.as_deref().unwrap_or("(never written)")
    );
    if let Some(notes) = &info.notes {
        println!("  Notes:       {notes}");
    }

    println!();
    if info.unit_count == 0 {
        println!("Units carried: none");
    } else {
        println!(
            "Units carried: {} ({} across {} tenant(s){})",
            info.unit_count,
            crate::util::format_bytes_binary(info.unit_total_bytes),
            info.tenants.len(),
            match (&info.units_first_seen, &info.units_last_seen) {
                (Some(a), Some(b)) if a != b => format!(", {a} .. {b}"),
                (Some(a), _) => format!(", {a}"),
                _ => String::new(),
            },
        );
        let shown = info.units.as_deref().unwrap_or(&info.largest_units);
        for u in shown {
            println!(
                "    {} v{} ({}) — {}",
                u.unit,
                u.version,
                u.tenant,
                crate::util::format_bytes_binary(u.bytes),
            );
        }
        if info.units.is_none() && info.unit_count as usize > shown.len() {
            println!(
                "    ... and {} more (use --units to list all of them)",
                info.unit_count as usize - shown.len()
            );
        }
    }

    println!();
    if info.writes.is_empty() {
        println!("Write receipts: none");
    } else {
        println!("Write receipts:");
        for w in &info.writes {
            println!(
                "    {} v{}: {} ({})",
                w.unit,
                w.version,
                w.status,
                w.completed_at.as_deref().unwrap_or("not completed"),
            );
        }
    }

    println!();
    if info.verifications.is_empty() {
        println!("Verification history: never verified");
    } else {
        println!("Verification history:");
        for v in &info.verifications {
            println!(
                "    {} [{}]: {} ({}/{} slices passed)",
                v.started_at, v.verify_type, v.outcome, v.slices_passed, v.slices_checked,
            );
        }
    }

    println!();
    if info.deposits.is_empty() {
        println!("Warehouse deposits: none");
    } else {
        println!("Warehouse deposits:");
        for d in &info.deposits {
            println!(
                "    {} ({}){}",
                d.location,
                d.deposited_at,
                d.receipt
                    .as_deref()
                    .map(|r| format!(", receipt {r}"))
                    .unwrap_or_default(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Issue #146: `volume compact`'s destination-label prompt had no
    /// terminal check, so a non-interactive run blocked forever on a handle
    /// nobody would ever write to — the issue #33 failure shape ADR-0008
    /// names by name. Every test here drives `resolve_compact_destination_with`
    /// so both the TTY answer and the read are injected; the real
    /// `resolve_compact_destination` is never called, exactly as
    /// `cli::consent`'s own tests avoid the ambient terminal.
    mod compact_destination {
        use super::*;

        /// The crux: the refusal path must not merely return an error, it
        /// must never ATTEMPT the read that would hang.
        #[test]
        fn non_tty_without_to_refuses_and_never_reads_stdin() {
            let err = resolve_compact_destination_with(None, false, || {
                panic!("must never attempt to read stdin when non-TTY and --to was not given")
            })
            .expect_err("a non-interactive compaction with no --to must refuse");
            let msg = err.to_string();
            assert!(msg.contains("refused"), "got: {msg}");
            assert!(
                msg.contains("--to"),
                "the refusal must name the override; got: {msg}"
            );
        }

        /// `--to` short-circuits before any stdin interaction, on a TTY or
        /// not — that is what makes compaction scriptable.
        #[test]
        fn to_short_circuits_before_any_read() {
            for is_tty in [true, false] {
                let label = resolve_compact_destination_with(Some("L6-DST"), is_tty, || {
                    panic!("--to must short-circuit before any stdin read")
                })
                .unwrap();
                assert_eq!(label, "L6-DST");
            }
        }

        #[test]
        fn to_is_trimmed_and_an_empty_one_is_rejected() {
            assert_eq!(
                resolve_compact_destination_with(Some("  L6-DST\n"), false, || unreachable!())
                    .unwrap(),
                "L6-DST"
            );
            assert!(
                resolve_compact_destination_with(Some("   "), false, || unreachable!()).is_err(),
                "an empty --to is a mistake, not a request to prompt"
            );
        }

        /// The interactive path still works — the fix is a terminal check,
        /// not the removal of the prompt.
        #[test]
        fn a_tty_still_prompts_and_trims_the_answer() {
            let label =
                resolve_compact_destination_with(None, true, || Ok("  L6-DST \n".to_string()))
                    .unwrap();
            assert_eq!(label, "L6-DST");
        }

        /// A bare Enter is not a destination. It used to be caught after
        /// the read; it still is.
        #[test]
        fn an_empty_answer_on_a_tty_is_refused() {
            assert!(resolve_compact_destination_with(None, true, || Ok("\n".to_string())).is_err());
        }
    }

    /// `volume deposit` validation (issue #73 / ADR-0006). These are the
    /// only two refusals the whole feature adds; everything else about a
    /// deposit is advisory.
    mod deposits {
        use super::*;
        use rusqlite::params;

        fn fixture() -> Connection {
            let (conn, _unit, _vol) =
                crate::policy::coverage::tests::setup_unit_with_deposit("active");
            conn.execute("DELETE FROM volume_deposits", []).unwrap();
            conn
        }

        fn add(conn: &Connection, label: &str, to: &str) -> Result<()> {
            run_deposit(
                conn,
                &DepositCommands::Add {
                    label: label.to_string(),
                    to: to.to_string(),
                    receipt: Some("rcpt-9".into()),
                    storage_class: Some("DEEP_ARCHIVE".into()),
                    notes: None,
                },
                true,
            )
        }

        #[test]
        fn records_a_deposit_of_a_sealed_volume_at_a_warehouse() {
            let conn = fixture();
            add(&conn, "L6-0003", "glacier").expect("sealed volume + warehouse must be accepted");
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM volume_deposits", [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 1);
            let receipt: Option<String> = conn
                .query_row("SELECT receipt FROM volume_deposits", [], |r| r.get(0))
                .unwrap();
            assert_eq!(receipt.as_deref(), Some("rcpt-9"));
        }

        #[test]
        fn refuses_a_shelf_location() {
            let conn = fixture();
            let err = add(&conn, "L6-0003", "home").expect_err(
                "a shelf is not a warehouse; recording a deposit there would \
                             claim a copy in a place nothing was copied to",
            );
            let msg = err.to_string();
            assert!(msg.contains("is a shelf, not a warehouse"), "{msg}");
        }

        #[test]
        fn refuses_a_volume_that_is_not_sealed() {
            let conn = fixture();
            conn.execute("UPDATE volumes SET status = 'active'", [])
                .unwrap();
            let err = add(&conn, "L6-0003", "glacier")
                .expect_err("unsealed bytes are not final, so nothing durable was deposited");
            let msg = err.to_string();
            assert!(msg.contains("is active, not sealed"), "{msg}");
        }

        /// ADR-0006: "a warehouse copy dies weeks after payment stops."
        /// When it does, the row must be removable -- a deposit that
        /// outlives its bytes keeps counting as a copy at `unit
        /// mark-tape-only` and `snapshot mark-reclaimable`, the two gates
        /// that decide whether local data may be deleted.
        #[test]
        fn a_deposit_can_be_un_recorded_and_stops_counting_as_a_copy() {
            let conn = fixture();
            add(&conn, "L6-0003", "glacier").unwrap();
            let before: i64 = conn
                .query_row("SELECT COUNT(*) FROM volume_deposits", [], |r| r.get(0))
                .unwrap();
            assert_eq!(before, 1);

            run_deposit(
                &conn,
                &DepositCommands::Remove {
                    label: "L6-0003".into(),
                    from: "glacier".into(),
                },
                true,
            )
            .expect("a recorded deposit must be removable");

            let after: i64 = conn
                .query_row("SELECT COUNT(*) FROM volume_deposits", [], |r| r.get(0))
                .unwrap();
            assert_eq!(after, 0, "the row must be gone, not merely flagged");
        }

        /// Removing something that was never recorded is an error, not a
        /// silent success -- a quiet no-op would let an operator believe
        /// they had corrected the catalog when they had typoed a label.
        #[test]
        fn removing_a_deposit_that_was_never_recorded_errors() {
            let conn = fixture();
            let err = run_deposit(
                &conn,
                &DepositCommands::Remove {
                    label: "L6-0003".into(),
                    from: "glacier".into(),
                },
                true,
            )
            .expect_err("nothing was recorded there");
            assert!(err.to_string().contains("no recorded deposit"), "{err}");
        }

        #[test]
        fn refuses_an_unknown_location_and_an_unknown_volume() {
            let conn = fixture();
            assert!(add(&conn, "L6-0003", "nowhere").is_err());
            assert!(add(&conn, "NO-SUCH-VOL", "glacier").is_err());
        }

        /// The UNIQUE(volume_id, location_id) constraint: the same volume
        /// cannot be deposited twice at one warehouse, which would
        /// double-count it as two copies.
        #[test]
        fn refuses_a_duplicate_deposit_of_the_same_volume_at_the_same_warehouse() {
            let conn = fixture();
            add(&conn, "L6-0003", "glacier").unwrap();
            assert!(add(&conn, "L6-0003", "glacier").is_err());
        }

        #[test]
        fn list_filters_by_volume_and_json_stdout_stays_parseable() {
            let conn = fixture();
            conn.execute(
                "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                      capacity_bytes, status)
                 VALUES ('L6-0004', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed')",
                [],
            )
            .unwrap();
            add(&conn, "L6-0003", "glacier").unwrap();
            add(&conn, "L6-0004", "glacier").unwrap();

            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM volume_deposits d JOIN volumes v ON v.id = d.volume_id
                     WHERE v.label = ?1",
                    params!["L6-0003"],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1);

            run_deposit(
                &conn,
                &DepositCommands::List {
                    volume: Some("L6-0003".into()),
                },
                true,
            )
            .unwrap();
        }
    }

    #[test]
    fn verify_exit_code_clean_report_is_success() {
        let report = write::VerifyReport {
            checked: 10,
            passed: 10,
            failed: 0,
            ..Default::default()
        };
        assert_eq!(verify_exit_code(&report), crate::error::EXIT_SUCCESS);
    }

    #[test]
    fn verify_exit_code_any_failure_is_violation() {
        let report = write::VerifyReport {
            checked: 10,
            passed: 9,
            failed: 1,
            ..Default::default()
        };
        assert_eq!(verify_exit_code(&report), crate::error::EXIT_ERROR);
    }

    #[test]
    fn verify_exit_code_all_failed_is_violation() {
        let report = write::VerifyReport {
            checked: 3,
            passed: 0,
            failed: 3,
            ..Default::default()
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

    /// `volume list` / `volume info` (issue #195).
    mod list_and_info {
        use super::*;
        use rusqlite::params;

        /// Fixture matching the ratified `volume list` design's own example
        /// table: five volumes across five statuses (initialized, sealed
        /// x2, retired, erased). `docs` is bin-packed onto BOTH the retired
        /// `L6-0000` and the sealed `L6-0002`, so `L6-0000`'s row proves
        /// `copies` is the unit's INCLUSIVE current coverage, not "other
        /// copies excluding this volume" — a lone eligible volume elsewhere
        /// must read as the real total, not zero.
        fn seed() -> Connection {
            let conn = crate::db::open_memory().unwrap();
            conn.execute_batch(
                "INSERT INTO locations (name, kind) VALUES ('home-rack','shelf'), ('bank','shelf');

                 INSERT INTO cartridges (barcode, media_type, nominal_capacity, serial_number, status) VALUES
                    ('E01001L8_17757944','LTO-6',2500000000000,'E01001L8_17757944','available'),
                    ('HUJ808A5L4','LTO-6',2500000000000,'HUJ808A5L4','in_use'),
                    ('E01001L8_17757943','LTO-6',2500000000000,'E01001L8_17757943','in_use'),
                    ('E01001L8_17757941','LTO-6',2500000000000,'E01001L8_17757941','retired_permanent');

                 INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes,
                                       bytes_written, status, location_id, created_at, first_write) VALUES
                    ('L6-0003','lto','lto0','LTO-6',2500000000000,0,'initialized',
                        (SELECT id FROM locations WHERE name='home-rack'),'2026-09-10 00:00:00',NULL),
                    ('L6-0002','lto','lto0','LTO-6',2500000000000,100000000000,'sealed',
                        (SELECT id FROM locations WHERE name='bank'),'2026-09-05 00:00:00','2026-09-06 00:00:00'),
                    ('L6-0001','lto','lto0','LTO-6',2500000000000,200000000000,'sealed',
                        (SELECT id FROM locations WHERE name='home-rack'),'2026-09-01 00:00:00','2026-09-01 12:00:00'),
                    ('L6-0000','lto','lto0','LTO-6',2500000000000,150000000000,'retired',
                        NULL,'2026-08-01 00:00:00','2026-08-01 12:00:00'),
                    ('L6-0004','lto','lto0','LTO-6',2500000000000,0,'erased',
                        NULL,'2026-07-01 00:00:00',NULL);

                 INSERT INTO cartridge_volumes (cartridge_id, volume_id) VALUES
                    ((SELECT id FROM cartridges WHERE barcode='E01001L8_17757944'),
                     (SELECT id FROM volumes WHERE label='L6-0003')),
                    ((SELECT id FROM cartridges WHERE barcode='HUJ808A5L4'),
                     (SELECT id FROM volumes WHERE label='L6-0002')),
                    ((SELECT id FROM cartridges WHERE barcode='E01001L8_17757943'),
                     (SELECT id FROM volumes WHERE label='L6-0001')),
                    ((SELECT id FROM cartridges WHERE barcode='E01001L8_17757941'),
                     (SELECT id FROM volumes WHERE label='L6-0000'));

                 INSERT INTO tenants (name, is_operator, status) VALUES ('t1', 0, 'active');

                 INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status) VALUES
                    ('u-photos','photos',(SELECT id FROM tenants WHERE name='t1'),'mtime_size',1,'active'),
                    ('u-docs','docs',(SELECT id FROM tenants WHERE name='t1'),'mtime_size',1,'active');

                 INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path) VALUES
                    ((SELECT id FROM units WHERE name='photos'), 1, 'full', 'current', '/data/photos'),
                    ((SELECT id FROM units WHERE name='docs'), 1, 'full', 'current', '/data/docs');

                 INSERT INTO stage_sets (snapshot_id, status, slice_size, num_slices, total_encrypted_size) VALUES
                    ((SELECT id FROM snapshots WHERE unit_id=(SELECT id FROM units WHERE name='photos')),
                     'staged', 104857600, 2, 209715200),
                    ((SELECT id FROM snapshots WHERE unit_id=(SELECT id FROM units WHERE name='docs')),
                     'staged', 104857600, 1, 52428800),
                    ((SELECT id FROM snapshots WHERE unit_id=(SELECT id FROM units WHERE name='docs')),
                     'staged', 104857600, 1, 52428800);",
            )
            .unwrap();

            let photos_ss: i64 = conn
                .query_row(
                    "SELECT ss.id FROM stage_sets ss
                     JOIN snapshots s ON s.id = ss.snapshot_id
                     JOIN units u ON u.id = s.unit_id
                     WHERE u.name = 'photos'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            let docs_ss: Vec<i64> = {
                let mut docs_ss_stmt = conn
                    .prepare(
                        "SELECT ss.id FROM stage_sets ss
                         JOIN snapshots s ON s.id = ss.snapshot_id
                         JOIN units u ON u.id = s.unit_id
                         WHERE u.name = 'docs' ORDER BY ss.id",
                    )
                    .unwrap();
                docs_ss_stmt
                    .query_map([], |r| r.get(0))
                    .unwrap()
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .unwrap()
            };
            let photos_snap: i64 = conn
                .query_row(
                    "SELECT id FROM snapshots WHERE unit_id = (SELECT id FROM units WHERE name='photos')",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            let docs_snap: i64 = conn
                .query_row(
                    "SELECT id FROM snapshots WHERE unit_id = (SELECT id FROM units WHERE name='docs')",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            let vol = |label: &str| -> i64 {
                conn.query_row(
                    "SELECT id FROM volumes WHERE label = ?1",
                    params![label],
                    |r| r.get(0),
                )
                .unwrap()
            };
            let (l6_0001, l6_0002, l6_0000) = (vol("L6-0001"), vol("L6-0002"), vol("L6-0000"));

            conn.execute(
                "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status, completed_at)
                 VALUES (?1, ?2, ?3, 'completed', '2026-09-01 13:00:00')",
                params![photos_ss, photos_snap, l6_0001],
            )
            .unwrap();
            // `docs`'s first copy: the volume that is later retired.
            conn.execute(
                "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status, completed_at)
                 VALUES (?1, ?2, ?3, 'completed', '2026-08-01 13:00:00')",
                params![docs_ss[0], docs_snap, l6_0000],
            )
            .unwrap();
            // `docs`'s second copy: sealed and eligible, so the unit's
            // current coverage is 1 even though the volume it was FIRST
            // written to is now retired.
            conn.execute(
                "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status, completed_at)
                 VALUES (?1, ?2, ?3, 'completed', '2026-09-05 13:00:00')",
                params![docs_ss[1], docs_snap, l6_0002],
            )
            .unwrap();

            let now = chrono::Utc::now().naive_utc();
            let d14 = (now - chrono::Duration::days(14))
                .format("%Y-%m-%d %H:%M:%S")
                .to_string();
            let d92 = (now - chrono::Duration::days(92))
                .format("%Y-%m-%d %H:%M:%S")
                .to_string();
            conn.execute(
                "INSERT INTO verification_sessions
                    (volume_id, started_at, completed_at, verify_type, outcome,
                     slices_checked, slices_passed, slices_failed)
                 VALUES (?1, ?2, ?2, 'full', 'passed', 2, 2, 0)",
                params![l6_0001, d14],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO verification_sessions
                    (volume_id, started_at, completed_at, verify_type, outcome,
                     slices_checked, slices_passed, slices_failed)
                 VALUES (?1, ?2, ?2, 'full', 'passed', 1, 1, 0)",
                params![l6_0000, d92],
            )
            .unwrap();

            conn
        }

        /// Acceptance: a retired, an erased and an unplaced volume all
        /// appear in the same `volume list` — nothing hidden by default
        /// (rule #2), and ordering is most-recently-written first.
        #[test]
        fn list_shows_every_status_most_recently_written_first() {
            let conn = seed();
            let rows = volume_rows(&conn, None).unwrap();
            let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
            assert_eq!(
                labels,
                vec!["L6-0003", "L6-0002", "L6-0001", "L6-0000", "L6-0004"],
                "COALESCE(first_write, created_at) DESC, id DESC"
            );

            let retired = rows.iter().find(|r| r.label == "L6-0000").unwrap();
            assert_eq!(retired.status, "retired");
            assert_eq!(
                retired.location, None,
                "an unplaced volume must still appear, not vanish behind an inner join"
            );

            let erased = rows.iter().find(|r| r.label == "L6-0004").unwrap();
            assert_eq!(erased.status, "erased");
            assert_eq!(
                erased.cartridge, None,
                "an unbound volume must still appear"
            );
        }

        /// A DISPLACED volume keeps its cartridge column.
        ///
        /// ADR-0010's re-initialisation path closes the displaced volume's
        /// mount (`cartridge_volumes.unmounted_at`) and marks it `erased`,
        /// leaving the row in place. The cartridge join is deliberately NOT
        /// filtered on `unmounted_at IS NULL`, so the volume still reports
        /// which physical tape it lived on — the question an operator asks
        /// of an erased volume is exactly "which cartridge got reused".
        ///
        /// `cartridge_volumes` is `UNIQUE(volume_id)` (001_initial.sql:222),
        /// so an unfiltered join can never duplicate a row here. Pinned
        /// because the filter looks like an omission: adding
        /// `unmounted_at IS NULL` would silently blank this column for every
        /// displaced volume, and the status column already says `erased`, so
        /// nothing here implies the bytes are still there.
        #[test]
        fn a_displaced_volume_still_names_the_cartridge_it_lived_on() {
            let conn = seed();
            let cart_id: i64 = conn
                .query_row(
                    "SELECT id FROM cartridges WHERE barcode = 'E01001L8_17757943'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            let vol_id: i64 = conn
                .query_row("SELECT id FROM volumes WHERE label = 'L6-0004'", [], |r| {
                    r.get(0)
                })
                .unwrap();
            // The shape re-initialisation leaves behind: a CLOSED mount.
            conn.execute(
                "INSERT INTO cartridge_volumes (cartridge_id, volume_id, mounted_at, unmounted_at)
                 VALUES (?1, ?2, datetime('now','-30 days'), datetime('now','-1 day'))",
                rusqlite::params![cart_id, vol_id],
            )
            .unwrap();

            let rows = volume_rows(&conn, None).unwrap();
            assert_eq!(
                rows.len(),
                5,
                "UNIQUE(volume_id) means a closed mount cannot duplicate the row"
            );
            let displaced = rows.iter().find(|r| r.label == "L6-0004").unwrap();
            assert_eq!(displaced.status, "erased");
            assert_eq!(
                displaced.cartridge.as_deref(),
                Some("E01001L8_17757943"),
                "a displaced volume must still name the cartridge it lived on"
            );
        }

        /// Rule: `--status` narrows but is never the default filter, and it
        /// is bound rather than interpolated (issue #110's precedent).
        #[test]
        fn status_filter_narrows_and_is_bound_not_interpolated() {
            let conn = seed();
            let rows = volume_rows(&conn, Some("sealed")).unwrap();
            assert_eq!(rows.len(), 2);
            assert!(rows.iter().all(|r| r.status == "sealed"));

            let rows = volume_rows(&conn, Some("sealed' OR '1'='1")).unwrap();
            assert!(
                rows.is_empty(),
                "a quoted payload must match no rows, not inject"
            );
        }

        /// Issue #171 / ADR-0012: `volume list --status` must be a usage
        /// error naming the accepted set for anything outside
        /// `volumes.status`'s CHECK constraint -- `volume_rows` itself (the
        /// function above) still just filters to nothing for a raw string;
        /// the usage-error guard lives in `run()`, one layer up.
        #[test]
        fn validate_volume_status_rejects_a_typo_naming_accepted_values() {
            let err = validate_volume_status("seald").unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("seald"), "{msg}");
            assert!(msg.contains("sealed"), "{msg}");
            assert!(msg.contains("quarantined"), "{msg}");
        }

        #[test]
        fn validate_volume_status_accepts_every_real_status() {
            for s in VOLUME_STATUSES {
                assert!(validate_volume_status(s).is_ok(), "{s} should be accepted");
            }
        }

        /// Rule #7 (C2b): every table column is a JSON key, and the JSON
        /// value is the RAW fact, not the table's rendered "—"/"(unbound)"
        /// text.
        #[test]
        fn json_shape_has_every_table_column_as_a_raw_valued_key() {
            let conn = seed();
            let rows = volume_rows(&conn, None).unwrap();
            let value = volume_rows_to_json(&rows);
            let full = serde_json::to_string(&value).unwrap();
            let reparsed: serde_json::Value =
                serde_json::from_str(&full).expect("the whole of stdout must parse as JSON");
            let arr = reparsed.as_array().unwrap();
            assert_eq!(arr.len(), rows.len());

            for row in arr {
                let obj = row.as_object().unwrap();
                for key in [
                    "label",
                    "status",
                    "cartridge",
                    "location",
                    "copies",
                    "verified",
                ] {
                    assert!(obj.contains_key(key), "missing key {key}: {row}");
                }
            }

            let initialized = arr.iter().find(|r| r["label"] == "L6-0003").unwrap();
            assert_eq!(
                initialized["copies"],
                serde_json::Value::Null,
                "raw value, not the table's \"—\""
            );
            assert_eq!(initialized["cartridge"], "E01001L8_17757944");
            assert_eq!(initialized["location"], "home-rack");
        }

        /// Acceptance: copies must agree with `report copies` on the same
        /// fixture, since both route through `policy::coverage` — a
        /// divergence is a bug. `L6-0000` (retired) is the interesting
        /// case: its own write no longer counts, so its row's `copies`
        /// must equal `docs`'s coverage from the OTHER sealed volume,
        /// proving the column is inclusive rather than "other copies".
        #[test]
        fn copies_agree_with_report_copies_on_the_same_fixture() {
            let conn = seed();
            let vol_rows = volume_rows(&conn, None).unwrap();
            let unit_rows = crate::cli::report::copies_rows(&conn, None).unwrap();
            let docs_copies = unit_rows
                .iter()
                .find(|(name, ..)| name.as_str() == "docs")
                .unwrap()
                .1;
            let photos_copies = unit_rows
                .iter()
                .find(|(name, ..)| name.as_str() == "photos")
                .unwrap()
                .1;
            assert_eq!(docs_copies, 1);
            assert_eq!(photos_copies, 1);

            let by_label =
                |label: &str| -> &VolumeRow { vol_rows.iter().find(|r| r.label == label).unwrap() };
            assert_eq!(by_label("L6-0002").copies, Some(docs_copies));
            assert_eq!(by_label("L6-0001").copies, Some(photos_copies));
            assert_eq!(
                by_label("L6-0000").copies,
                Some(docs_copies),
                "a retired volume's row must show the unit's real current \
                 coverage (from elsewhere), not 0 and not \"other copies\""
            );
            assert_eq!(
                by_label("L6-0003").copies,
                None,
                "an initialized volume with no writes carries no unit at all"
            );
        }

        /// Rule #6: never-verified must render distinctly from a
        /// long-ago verification, and distinctly again from a volume that
        /// has never carried any data at all (the pure half, matching
        /// `policy::evidence::describe`'s deterministic-`now` split).
        #[test]
        fn never_verified_renders_distinctly_from_an_aged_verification() {
            let now =
                chrono::NaiveDateTime::parse_from_str("2026-09-15 00:00:00", "%Y-%m-%d %H:%M:%S")
                    .unwrap();
            use crate::policy::evidence::compact_age;
            assert_eq!(compact_age(None, now), "never");
            assert_eq!(compact_age(Some("2026-09-01 00:00:00"), now), "14d ago");
            assert_ne!(
                compact_age(None, now),
                compact_age(Some("2026-09-01 00:00:00"), now)
            );
            // An unparseable stamp says so (issue #217). This used to assert
            // the RAW string, with a comment claiming that "matches
            // `policy::evidence`'s honesty rule" -- it did the opposite:
            // `compact_age` has always returned "unparseable", so `volume
            // list` would print a garbage timestamp verbatim in a column
            // where `catalog locate` printed "unparseable" for the same row.
            // The test pinned the divergence AND mis-cited the module it
            // diverged from.
            assert_eq!(compact_age(Some("not-a-timestamp"), now), "unparseable");
        }

        /// The table cell goes a step further than the pure formatter: a
        /// volume that has never carried any data (`copies: None`) must
        /// render "—", visibly different again from "never" — a volume
        /// WITH data nobody has checked.
        #[test]
        fn no_data_volume_renders_dash_never_verified_volume_renders_never() {
            let no_data = VolumeRow {
                label: "L6-BLANK".into(),
                status: "initialized".into(),
                cartridge: None,
                location: None,
                copies: None,
                verified: None,
            };
            let has_data_never_verified = VolumeRow {
                label: "L6-DATA".into(),
                status: "sealed".into(),
                cartridge: None,
                location: None,
                copies: Some(1),
                verified: None,
            };
            assert_eq!(no_data.display_verified(), "—");
            assert_eq!(has_data_never_verified.display_verified(), "never");
            assert_ne!(
                no_data.display_verified(),
                has_data_never_verified.display_verified()
            );
        }

        /// Rule #5 / ADR-0006: a warehouse deposit is its own evidence
        /// class in `volume info`'s dossier, never folded into a copy
        /// count.
        #[test]
        fn info_shows_deposits_as_their_own_field() {
            let (conn, _unit_id, _vol_id) =
                crate::policy::coverage::tests::setup_unit_with_deposit("active");
            let info = volume_info(&conn, "L6-0003", false).unwrap();
            assert_eq!(info.deposits.len(), 1);
            assert_eq!(info.deposits[0].location, "glacier");
        }

        /// Rule #8: `volume info` summarises by default (the design probes
        /// ~280 units per cartridge) and `--units` expands to the full
        /// list.
        #[test]
        fn info_summarises_by_default_and_units_flag_expands() {
            let conn = crate::db::open_memory().unwrap();
            conn.execute_batch(
                "INSERT INTO tenants (name, is_operator, status) VALUES ('t', 0, 'active');
                 INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                       capacity_bytes, status)
                     VALUES ('L6-BIG', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed');",
            )
            .unwrap();
            let vol_id: i64 = conn
                .query_row("SELECT id FROM volumes WHERE label = 'L6-BIG'", [], |r| {
                    r.get(0)
                })
                .unwrap();
            let tenant_id: i64 = conn
                .query_row("SELECT id FROM tenants WHERE name = 't'", [], |r| r.get(0))
                .unwrap();
            for i in 0..7 {
                conn.execute(
                    "INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
                     VALUES (?1, ?2, ?3, 'mtime_size', 1, 'active')",
                    params![format!("uuid-{i}"), format!("unit{i}"), tenant_id],
                )
                .unwrap();
                let unit_id = conn.last_insert_rowid();
                conn.execute(
                    "INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
                     VALUES (?1, 1, 'full', 'current', '/x')",
                    params![unit_id],
                )
                .unwrap();
                let snap_id = conn.last_insert_rowid();
                conn.execute(
                    "INSERT INTO stage_sets (snapshot_id, status, slice_size, total_encrypted_size)
                     VALUES (?1, 'staged', 524288, ?2)",
                    params![snap_id, (i + 1) * 1000],
                )
                .unwrap();
                let ss_id = conn.last_insert_rowid();
                conn.execute(
                    "INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status, completed_at)
                     VALUES (?1, ?2, ?3, 'completed', '2026-01-01 00:00:00')",
                    params![ss_id, snap_id, vol_id],
                )
                .unwrap();
            }

            let summary = volume_info(&conn, "L6-BIG", false).unwrap();
            assert_eq!(summary.unit_count, 7);
            assert_eq!(
                summary.largest_units.len(),
                VOLUME_INFO_SUMMARY_UNITS,
                "summarised to the top few, not all 7"
            );
            assert!(summary.units.is_none());

            let full = volume_info(&conn, "L6-BIG", true).unwrap();
            assert_eq!(
                full.units.as_ref().unwrap().len(),
                7,
                "--units lists every unit carried"
            );
        }

        /// Acceptance: `volume info` on a nonexistent label errors by name.
        #[test]
        fn info_on_a_nonexistent_label_errors_by_name() {
            let conn = crate::db::open_memory().unwrap();
            let err = volume_info(&conn, "NOPE", false).unwrap_err();
            assert!(err.to_string().contains("NOPE"), "{err}");
        }
    }
}
