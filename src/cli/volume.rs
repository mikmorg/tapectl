use clap::Subcommand;
use rusqlite::Connection;
use serde::Serialize;
use tabled::{Table, Tabled};

use crate::cli::{read_device, write_device};
use crate::config::{Config, TapectlPaths};
use crate::error::{Result, TapectlError};
use crate::store::{TapeStore, Tier};
use crate::tape::contact::{ContactSite, Medium, Operation};
use crate::volume::write::{self, Initialized};

pub(crate) const DEFAULT_BLOCK_SIZE: usize = 512 * 1024; // 512 KB

#[derive(Subcommand, Debug)]
pub enum VolumeCommands {
    /// Initialize a new volume (write ID thunk to tape)
    ///
    /// After the volume line, prints one line naming the medium the
    /// cartridge was taken for: its generation, the capacity stored on the
    /// volume (decimal) and what decided each, e.g. `medium: LTO-5, 1.50 TB
    /// (the cartridge chip's medium density code 0x58)`. An LTO-5 cartridge
    /// in an LTO-6 drive is seen to be LTO-5 here. `--json` carries the
    /// same facts as a `medium` object.
    Init {
        /// Volume label (e.g., L6-0001)
        label: String,
        /// Tape device (by-id path). Defaults to the only configured drive;
        /// required when more than one is configured.
        #[arg(long)]
        device: Option<String>,
        /// Overwrite a cartridge whose File 0 already identifies a
        /// DIFFERENT volume (e.g. a mislabeled or stale tape), or is EMPTY
        /// (a filemark at the beginning of the tape). Refused by
        /// default — loading the wrong cartridge would
        /// otherwise silently overwrite it. An EMPTY or unreadable File 0
        /// needs no `--force` when the tape is PROVEN blank (ADR-0012
        /// 2026-10-15 item 57): the chip's lifetime totals read 0 MiB
        /// written, or this catalog's own `cartridge erase` verified it
        /// blank and nothing has been written since; init prints which
        /// proof it used. Never overrides a cartridge
        /// that is already SEALED (ADR-0003): retire its volume, erase the
        /// tape in the drive (`mt erase`, or `mt weof 1` at the start) and
        /// run `cartridge mark-erased` first for that case. Never degauss
        /// an LTO cartridge: that destroys its servo tracks. It also
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
    ///
    /// Once the volume is sealed and confirmed, staging is released if every
    /// unit written has reached its resolved `min_copies` (ADR-0012,
    /// 2026-10-07 item 2: after a quick confirm too); otherwise it is kept
    /// and the output names each unit that is still short. A release that
    /// fails is a warning and the exit status stays 0. `--keep-staging`
    /// never releases; `staging clean` does it later.
    ///
    /// A further copy in one command (ADR-0012 2026-10-15 item 59):
    /// `tapectl volume write L6-0003 --init --full-confirm --move-to
    /// offsite` initialises the loaded cartridge, writes the volume, reads
    /// every file back, and moves the sealed volume (and its cartridge) to
    /// `offsite`. Each step keeps its own checks and the command stops at
    /// the first one that fails: an init that refuses writes nothing, and
    /// nothing moves unless the write sealed and confirmed.
    ///
    /// Exit status, shared by every command that writes a tape: 0 = sealed
    /// and confirmed. 2 = an error with the medium untouched: nothing was
    /// written (or a usage error). 3 = the confirm was inconclusive: the
    /// tape is sealed, `volume resume` re-enters the confirm. 4 =
    /// interrupted part-way: `volume resume` continues. 5 = aborted (end of
    /// tape, a slice that changed): write the label again. 6 = the medium
    /// was proven bad and the volume quarantined. 75 = the catalog was busy.
    Write {
        /// Volume label. A write names its volume: the label ties the data
        /// to the volume `volume init` consented to (ADR-0003). Give it, or
        /// give `--loaded` instead
        #[arg(required_unless_present = "loaded")]
        label: Option<String>,
        /// Write the loaded tape without naming it: File 0 is read (a
        /// `volume identify` contact) and the tape is written only when it
        /// is one of this catalog's volumes, `initialized` and with nothing
        /// written to it; the volume chosen is printed. Any other state, or
        /// a tape this catalog does not know, is refused naming it. The
        /// write that follows takes its usual File 0 consent check on its
        /// own handle. Cannot be combined with a label or `--init`
        #[arg(long, conflicts_with = "label")]
        loaded: bool,
        /// Tape device (by-id path). Defaults to the only configured drive;
        /// required when more than one is configured.
        #[arg(long)]
        device: Option<String>,
        /// See `volume init --force` — same override, same limits. With
        /// `--init` it goes to the init only: the write that follows is
        /// never forced, so a cartridge swapped in between the two is
        /// refused at the write's own File 0 check.
        #[arg(long)]
        force: bool,
        /// Initialise the loaded cartridge as this volume first
        /// (`volume init <label>`: the same function, the same File 0
        /// consent check and refusals, ADR-0003 and ADR-0012 item 57), then
        /// write it with the tape still loaded. An init that refuses writes
        /// nothing and exits 2. Only `--force` is taken from `volume init`'s
        /// flags; when a cartridge needs `--generation` or `--cartridge`,
        /// run `volume init` yourself first. ADR-0012 2026-10-15 item 59.
        #[arg(long, requires = "label", conflicts_with = "loaded")]
        init: bool,
        /// Move the volume, with its cartridge, to this location once it is
        /// sealed and confirmed (`volume move <label> --to LOCATION`). The
        /// location is checked before anything is written. Nothing moves
        /// unless the write sealed and confirmed. A move that then fails is
        /// a warning naming the `volume move` command that finishes it; the
        /// copy stands and the exit status stays 0.
        #[arg(long, value_name = "LOCATION")]
        move_to: Option<String>,
        /// Seal slices that were staged before the escrow recipient existed
        /// (ADR-0005). Only reachable for tapes copied forward via
        /// `read-slices`/`compact-read`, since `stage create` refuses to
        /// stage without escrow; use it to migrate a dying pre-escrow
        /// cartridge, knowing the copy stays unrecoverable by the escrow key.
        #[arg(long)]
        allow_missing_escrow: bool,
        /// Full-hash every staged slice from disk before the tape moves
        /// (tri-layer L1). Off by default (ADR-0012, 2026-09-30): the write
        /// then checks each slice exists at its recorded size, and a slice
        /// that rotted in staging is caught while streaming (clean abort,
        /// tape left unsealed). This costs one extra full read of the batch.
        #[arg(long)]
        prewrite_hash: bool,
        /// Fill this volume to at most this fraction of its capacity, for
        /// this write only: `0.99` or `99%`. Overrides the drive's
        /// `fill_ceiling` (default 97%, ADR-0012). The pre-write check
        /// refuses a layout above the ceiling and names by how much.
        #[arg(long, value_parser = crate::config::parse_fill_ceiling)]
        fill_ceiling: Option<f64>,
        /// After sealing, read every file back and hash it against the
        /// front index (tri-layer L3, hours on a full cartridge). Off by
        /// default (ADR-0012, 2026-10-06): the confirm then reads the front
        /// index and the seal marker, and a passing one seals the volume
        /// with no full readback recorded — `audit` and `report
        /// verify-status` name it until `volume verify` runs one.
        #[arg(long)]
        full_confirm: bool,
        /// Keep the staged slices after the write seals and confirms, even
        /// when every unit written has reached its resolved `min_copies`.
        /// Without this flag the staging is released then; release it later
        /// with `tapectl staging clean`.
        #[arg(long)]
        keep_staging: bool,
    },

    /// Resume an interrupted write session. Reload the SAME
    /// cartridge first: the session continues from its frozen staging files
    /// rather than rebuilding them.
    ///
    /// An already-sealed tape is RE-CONFIRMED, not refused, when the tape
    /// itself proves it is this session's own: File 0's identity matches
    /// this volume, File 0's OWN recorded seal-marker pointer equals this
    /// session's layout, and a real seal marker parses at that position.
    /// The seal is never written a second time. This is the
    /// path `volume write` sends you down when it says "run `tapectl
    /// volume resume <label>` to retry the confirm readback".
    ///
    /// Everything else quarantines the volume: a File 0 whose identity does
    /// not match, one that is unreadable or unparseable, no recorded
    /// pointer, a pointer disagreeing with this session, or a position that
    /// does not parse as a seal marker. There is no --force: `volume write
    /// --force` overrides a wrong-cartridge finding before anything is
    /// written, which has no meaning for a tape this session has already
    /// partly written.
    ///
    /// An ABORTED session is adopted and re-confirmed the same way (never
    /// written, never re-sealed) only when its seal is recorded, its
    /// condition is ok, and a passing FULL verify is recorded after the
    /// abort (ADR-0012, 2026-09-23); otherwise resume names the first unmet
    /// condition and, where one exists, the command that resolves it.
    ///
    /// A write the CATALOG lost (it was restored from a backup taken after
    /// `volume init` and before the write) is adopted as sealed, reading
    /// only, once `catalog rebuild --from-volume` has attached it and a
    /// passing full verify is recorded after that rebuild — and only when
    /// File 0's uuid is this volume's, a seal marker binds the front index,
    /// and every slice hash in the front index is the one this catalog
    /// recorded when it staged that slice (ADR-0012, 2026-09-29 later).
    ///
    /// Refuses any volume whose CATALOG status is not `initialized` — a
    /// volume recorded sealed, retired or erased, or whose condition is
    /// quarantined, is not a write target (ADR-0012); no flag overrides it.
    /// That is a different question from whether the loaded TAPE carries a
    /// seal marker, which is what the re-confirm path above is about.
    ///
    /// Once the resumed volume is sealed and confirmed, staging is released
    /// or kept exactly as `volume write` does it (`--keep-staging` opts out).
    ///
    /// Exit status: `volume write`'s (0 sealed and confirmed, 2 nothing
    /// written, 3 confirm inconclusive, 4 interrupted, 5 aborted, 6
    /// quarantined, 75 catalog busy). A resume refused before it changed
    /// the session exits 2 and leaves the session as it was.
    Resume {
        /// Volume label. Give it, or give `--loaded` instead
        #[arg(required_unless_present = "loaded")]
        label: Option<String>,
        /// Resume the loaded tape without naming it: File 0 is read (a
        /// `volume identify` contact) and the tape is resumed only when it
        /// is one of this catalog's volumes with an interrupted write
        /// session, a re-confirmable aborted one, or a write the catalog
        /// lost; the volume chosen is printed. Any other state, or a tape
        /// this catalog does not know, is refused naming it. Cannot be
        /// combined with a label
        #[arg(long, conflicts_with = "label")]
        loaded: bool,
        /// Tape device (by-id path). Defaults to the only configured drive;
        /// required when more than one is configured.
        #[arg(long)]
        device: Option<String>,
        /// See `volume write --prewrite-hash`: the revalidation full-hashes
        /// every staged slice instead of size-checking it. The session's
        /// frozen generated files are re-hashed either way.
        #[arg(long)]
        prewrite_hash: bool,
        /// See `volume write --full-confirm`: the confirm this resume
        /// runs (or re-enters) reads every file back. A full readback that
        /// was interrupted CONTINUES: the files it already read back clean
        /// are not read again, as long as the front index on the tape is
        /// the one they were checked against. Without this flag the resume
        /// runs the quick confirm and the readback is left to `volume
        /// verify`.
        #[arg(long)]
        full_confirm: bool,
        /// See `volume write --keep-staging`: the staged slices stay even
        /// when this resume completes the last copy the policy asks for.
        #[arg(long)]
        keep_staging: bool,
    },

    /// Deliberately abandon a volume's unfinished write session:
    /// `docs/design/layout-session.md`'s Aborted row, first clause. Use this
    /// when a `volume resume` reports a revalidation failure you know to be
    /// permanent (the staged data is really gone), or to clear a `planned`
    /// session that was killed before anything was written. Nothing can tell
    /// a transient cause from a permanent one but you, which is why resume
    /// never decides this on its own.
    ///
    /// The tape is never contacted (hence no --device): the cartridge is left
    /// exactly as the session left it, and the staged files stay pinned
    /// until `staging clean --force` runs. The session becomes ABORTED:
    /// `volume resume` will not pick it up again unless its seal is recorded
    /// AND a clean full verify is recorded after the abort (ADR-0012,
    /// 2026-09-23), and a session aborted before its seal is never
    /// resumable. When the seal is recorded, keep staging if you intend to
    /// verify and resume: that re-confirm revalidates against the staged
    /// files, so releasing them forfeits it.
    Abort {
        /// Volume label
        label: String,
        // No local `yes` field: consent comes from the GLOBAL `--yes`/`-y`
        // (issue #240). It used to be redeclared here as `#[arg(long)]`,
        // which shares clap's arg id with the global but NOT its `short` --
        // and the local declaration wins for this subcommand's parser, so
        // `tapectl volume abort L -y` died with "unexpected argument '-y'"
        // while `tapectl -y volume abort L` and `... --yes` both worked.
        // One declaration cannot drift from itself.
    },

    /// Verify volume contents via the keyless chain walk (seal -> front index
    /// -> content). Default tier is `--full` (integrity: hashes every
    /// content file); `--quick` opts down to navigable (seal binding + front
    /// index self-consistency only, no per-file content hashing).
    ///
    /// A full verify that was stopped part-way CONTINUES when run again:
    /// files the volume's latest readback (this command's or a write's
    /// `--full-confirm`) already read back clean are not read again, as
    /// long as the front index on the tape is the one they were checked
    /// against.
    ///
    /// A full verify that passes also records the volume's write figures
    /// (bytes written, data files, write dates) when the write that sealed
    /// it could not, from the front index it read and the catalog's own
    /// rows; figures already recorded are left as they are.
    ///
    /// Exit status: 0 = every checked file matched. 2 = the verify PROVED
    /// THE MEDIUM BAD: the volume is quarantined and no longer counts as a
    /// copy — write its content to another cartridge. 3 = inconclusive: a
    /// drive or transport failure, or an error before any verdict (no
    /// cartridge loaded, the wrong tape, an unknown label, a mistyped
    /// command line); the volume is untouched — check the drive and verify
    /// again.
    Verify {
        /// Volume label. Without it the loaded tape names itself: File 0
        /// is read (the read a verify makes at contact anyway) and the
        /// volume is found in this catalog by its uuid; the one chosen is
        /// printed. A label that is given is checked against File 0 as
        /// ever. A tape this catalog does not know is refused, naming what
        /// File 0 says
        label: Option<String>,
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
    ///
    /// Reads the unit's slices off the loaded tape, checks each against its
    /// recorded hash, and records where it put them, so `volume write` can
    /// write them to another cartridge without the source directory. Every
    /// version of the unit on the volume is read except those marked
    /// reclaimable or purged; `--version` reads one version, whatever its
    /// state. The staging directory is checked for room before the first
    /// slice is read, and a read that does not finish removes what it wrote.
    ReadSlices {
        /// Source volume label
        #[arg(long)]
        from: String,
        /// Unit name to read
        #[arg(long)]
        unit: String,
        /// Read only this version of the unit (a reclaimable or purged
        /// version is read when named). Without it, every version on the
        /// volume except the reclaimable and purged ones is read.
        #[arg(long, value_name = "N")]
        version: Option<i64>,
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
    ///
    /// Exit status: `volume write`'s (0 sealed and confirmed, 2 nothing
    /// written, 3 confirm inconclusive, 4 interrupted, 5 aborted, 6
    /// quarantined, 75 catalog busy).
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
        /// See `volume write --prewrite-hash`.
        #[arg(long)]
        prewrite_hash: bool,
        /// See `volume write --fill-ceiling`: this write only.
        #[arg(long, value_parser = crate::config::parse_fill_ceiling)]
        fill_ceiling: Option<f64>,
        /// See `volume write --full-confirm`.
        #[arg(long)]
        full_confirm: bool,
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
        /// Estimate at this fill ceiling instead of the drive's
        /// `fill_ceiling` (default 97%): `0.99` or `99%`.
        #[arg(long, value_parser = crate::config::parse_fill_ceiling)]
        fill_ceiling: Option<f64>,
        /// List first the staged units a write would help: those whose
        /// audit finds too few copies or a required location with no copy,
        /// each with what it falls short of. `volume write` writes every
        /// staged set either way; this orders the listing only.
        #[arg(long)]
        policy_aware: bool,
    },

    /// Retire source volume after compaction (compaction step 3)
    CompactFinish {
        /// Source volume label to retire
        label: String,
        /// Waive the ADR-0008 Tier-2 prompt: proceed when the retirement
        /// leaves a live version below its policy but above zero. It
        /// defeats NEITHER Tier-3 refusal — a live slice with
        /// no copy on another volume, and the last eligible copy of a live
        /// version, each stop the retirement outright and no flag reaches
        /// them. See cli::consent.
        #[arg(long)]
        force: bool,
    },

    /// Interactive compaction: read + write + finish in one flow
    ///
    /// One drive (`--device`) serves both the read and the write, so the
    /// flow PAUSES after step 1 for you to unload the source and load the
    /// destination — always, with or without `--to`. It therefore needs a
    /// terminal, and without one it refuses before reading anything. To
    /// compact unattended, run the steps separately: `volume compact-read
    /// <SOURCE>`, swap cartridges, `volume compact-write --destination
    /// <DEST>`, then `volume compact-finish <SOURCE>`.
    ///
    /// Step 3 applies `compact-finish`'s ADR-0008 Tier-2 gate and may
    /// refuse non-interactively without `--force`. When it does, the
    /// destination tape is already written and sealed and nothing is lost:
    /// `volume compact-finish <SOURCE> --force` completes the flow without
    /// re-reading or re-writing anything.
    ///
    /// Exit status: `volume write`'s, for step 2's write (0 sealed and
    /// confirmed, 2 nothing written, 3 confirm inconclusive, 4 interrupted,
    /// 5 aborted, 6 quarantined, 75 catalog busy). A step 1 failure exits 2:
    /// step 1 writes nothing. Step 3 refused for consent (no terminal, no
    /// --yes) exits 0 with a warning naming `volume compact-finish --force`:
    /// the destination is sealed, confirmed and a full carry-forward. Step 3
    /// refused because content was not carried forward exits 2.
    Compact {
        /// Source volume label
        label: String,
        /// Destination volume label (already initialised). With it, the
        /// pause after step 1 only asks you to swap cartridges and press
        /// Enter; without it, that pause also asks for the label. It never
        /// skips the swap: the destination is written through the same
        /// drive the source was read from.
        #[arg(long)]
        to: Option<String>,
        /// Tape device (by-id path). Defaults to the only configured drive;
        /// required when more than one is configured.
        #[arg(long)]
        device: Option<String>,
        /// See `volume write --allow-missing-escrow`.
        #[arg(long)]
        allow_missing_escrow: bool,
        /// See `volume write --prewrite-hash` (step 2's write).
        #[arg(long)]
        prewrite_hash: bool,
        /// See `volume write --fill-ceiling` (step 2's write).
        #[arg(long, value_parser = crate::config::parse_fill_ceiling)]
        fill_ceiling: Option<f64>,
        /// See `volume write --full-confirm` (step 2's write).
        #[arg(long)]
        full_confirm: bool,
        /// See `volume compact-finish --force` — step 3's ADR-0008 Tier-2
        /// gate. With this (or the global `--yes`) step 3 asks nothing; the
        /// cartridge swap after step 1 still waits for you.
        #[arg(long)]
        force: bool,
    },

    /// Record and inspect WAREHOUSE DEPOSITS of sealed volumes (ADR-0006).
    ///
    /// tapectl does NOT move the bytes: an operator copies a sealed
    /// volume's bytes to cold cloud storage by the documented external
    /// procedure (rclone / aws-cli) and
    /// then RECORDS that copy here, so the catalog can reason about it.
    Deposit {
        #[command(subcommand)]
        command: DepositCommands,
    },

    /// List every volume, most recently written first.
    ///
    /// Catalog-only: never opens a drive. Every status is shown by default —
    /// ADR-0011: retired means unfit to WRITE, not unreadable ("a retired
    /// volume can still be restored from"), and the dangerous failure for an
    /// inventory is a tape you forgot you had. `--status` narrows; nothing is
    /// hidden without it.
    ///
    /// GENERATION and CAPACITY are what `volume init` decided for the volume
    /// (capacity decimal, as the box counts it); `--json` carries them as
    /// `media_type` and `capacity_bytes`.
    List {
        /// Only volumes in this status (initialized, active, retired,
        /// erased, sealed). Every status is shown when
        /// omitted. A medium's CONDITION (e.g. quarantined) is its own fact
        /// now (ADR-0012, the 2026-09-17 amendment) and is shown in its own
        /// column/line, not filterable here.
        #[arg(long)]
        status: Option<String>,
    },

    /// The dossier for one volume: capacity, media generation, cartridge
    /// binding, location, units carried, writes, verification
    /// history, warehouse deposits.
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
        /// a claim.
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

/// `volumes.status`'s CHECK constraint
/// (`src/db/migrations/032_drop_volume_status_full.sql`, which last rewrote
/// it and is the schema's current word on it: `full` left there, as `blank`
/// and `missing` left in 026 -- nothing ever wrote any of them; issue #362,
/// ADR-0012 amendment 2026-10-07 item 16). `quarantined` is deliberately
/// absent: ADR-0012's 2026-09-17 amendment ("the status column is the
/// operator's; a medium's condition is its own fact", issue #242) moved it to
/// `volumes.observed_condition` in 017 — it left this CHECK entirely, it was
/// not merely renamed within it. Pinned to the live schema by
/// `volume_statuses_equal_the_live_check`.
const VOLUME_STATUSES: &[&str] = &["initialized", "active", "retired", "erased", "sealed"];

/// `volume list --status` is a usage error when it names anything other
/// than one of `VOLUME_STATUSES` (issue #171, ADR-0012) — `volumes.status`
/// never had an `offsite` value (a volume's place has always been
/// `location_id`, since `007_warehouse_locations.sql`), so this needs no
/// ADR-0011 special case the way `cartridge list --status` does.
///
/// `quarantined` gets its own message rather than the bare "unknown value"
/// `validate_closed_set` would otherwise give it (issue #242): operators
/// have typed it here for a long time, and it is not simply unknown — it
/// moved to a different fact entirely.
fn validate_volume_status(value: &str) -> Result<()> {
    if value == "quarantined" {
        return Err(TapectlError::Other(
            "--status quarantined: \"quarantined\" is a medium condition now, not a status \
             (ADR-0012, the 2026-09-17 amendment) -- it moved to `volumes.observed_condition` \
             and cannot be used to filter `--status`. `volume list`/`volume info` show it in \
             their own CONDITION field/line alongside status."
                .to_string(),
        ));
    }
    crate::config::validate_closed_set("--status", value, VOLUME_STATUSES)
        .map_err(TapectlError::Other)
}

/// The three things a clean FULL verify's clearing of
/// `volumes.observed_condition` can honestly be told to the operator
/// (issue #280), chosen by `counts_as_copy` — the exact answer
/// `policy::coverage::counts_as_copy` gives for this volume, i.e. the same
/// predicate `audit`/`report copies`/every `min_copies` gate reads. Kept as
/// its own pure function (no `Connection`, no `VerifyReport`) so each of
/// the three outcomes can be pinned by its own test without a live
/// store/DB round trip through the whole `volume verify` command.
///
/// `clear_condition_on_clean_full_verify` (`src/volume/write.rs`) only ever
/// writes `observed_condition`; `volumes.status` never moves here or there
/// — a medium observation never moves the operator's column (ADR-0012's
/// 2026-09-23 amendment rejected exactly that). So "counts as a copy again"
/// is only ever true in the one case this message was originally written
/// for (`counts_as_copy` true, which after a clean clear means `status =
/// 'sealed'`).
///
/// The remedy for a sealed-but-unconfirmed volume is `volume resume`, which
/// re-enters `confirm` on the recorded seal (the 2026-09-21 amendment for an
/// `interrupted` session; the 2026-09-23 amendment for an `aborted` one this
/// very verify may just have made adoptable). It is named ONLY when
/// `resume_would_reconfirm` — `volume::session::resume_would_reconfirm`,
/// the same admission `volume resume` itself applies — is true, because
/// naming it otherwise hands the operator a command that refuses.
pub(crate) fn clean_clear_message(
    label: &str,
    previous_condition: &str,
    status: &str,
    sealed_at_set: bool,
    counts_as_copy: bool,
    resume_would_reconfirm: bool,
) -> String {
    if counts_as_copy {
        format!(
            "volume \"{label}\" RETURNED TO SERVICE: a full verify read every file back and \
             found no mismatch, so its condition moves from \"{previous_condition}\" to \"ok\" \
             and it counts as a copy again (ADR-0012, 2026-09-18). Its status was never touched."
        )
    } else if status == "retired" {
        format!(
            "volume \"{label}\": a full verify read every file back and found no mismatch, so \
             its condition moves from \"{previous_condition}\" to \"ok\" — but it remains \
             RETIRED (`volumes.status = \"retired\"`), so it still counts as a copy for \
             nothing. That status was set deliberately, and a clean medium observation does \
             not undo it."
        )
    } else {
        // A plain `&str`, not `format!` — it interpolates nothing, and
        // `clippy::useless_format` is an error under `-D warnings`.
        //
        // "confirm PASSED", not "confirm completed": in this state confirm
        // did run to completion. It took `SealedPending::confirm`'s
        // `proves_medium_bad` branch, which is what recorded the quarantine
        // this verify has just cleared. What never happened is confirm
        // passing — the one branch that moves `writes` to `completed` and
        // `volumes.status` to `sealed`. Saying "completed" would read as if
        // confirm had crashed or never run, which is a different fault with
        // a different remedy.
        let sealed_note = if sealed_at_set {
            " The tape IS physically sealed, though: the seal marker and every byte are on \
             it, and this verify just read them all back. What is missing is the catalog's \
             record that the write session's confirm PASSED, which is the only thing that \
             sets \"sealed\"."
        } else {
            ""
        };
        let remedy = if resume_would_reconfirm {
            format!(
                " With this same cartridge loaded, run `tapectl volume resume {label}`: it \
                 re-enters the write session's confirm on the recorded seal — it never writes \
                 or re-seals the tape — and a passing confirm is what makes the volume count \
                 as a copy."
            )
        } else {
            String::new()
        };
        format!(
            "volume \"{label}\": a full verify read every file back and found no mismatch, so \
             its condition moves from \"{previous_condition}\" to \"ok\" — but it still does \
             NOT count as a copy: its status is \"{status}\", not \"sealed\".{sealed_note}{remedy}"
        )
    }
}

/// The `--json` twin of [`clean_clear_message`]'s branch selection (issue
/// #280): `returned_to_service` must never read `true` when the volume
/// does not currently count as a copy, exactly as the human message must
/// never claim RETURNED TO SERVICE in that case. `cleared` is
/// `report.cleared.is_some()` — this verify actually cleared the
/// condition — and `counts_as_copy` is `policy::coverage::counts_as_copy`'s
/// answer for this volume. Pulled out, like `clean_clear_message`, so this
/// one bit of `--json` semantics can be pinned by a test without a live
/// DB/store round trip.
fn returned_to_service_json(cleared: bool, counts_as_copy: bool) -> bool {
    cleared && counts_as_copy
}

/// What `volume write` / `volume resume` did about staging once the copy
/// was sealed and confirmed (issue #440).
#[derive(Debug)]
enum StagingRelease {
    /// `--keep-staging`: nothing was looked at, nothing released.
    Kept,
    /// Every unit written now meets its resolved `min_copies`: the report of
    /// the release (which may count zero sets when none was eligible).
    Released(crate::staging::clean::CleanReport),
    /// At least one unit written is still short: staging was kept, and these
    /// are the units that keep it.
    Retained(Vec<crate::staging::release::CopyProgress>),
    /// The volume has no unit with a completed write: nothing to judge.
    NothingWritten,
    /// The release failed after the copy stood (ADR-0012 2026-10-07 item
    /// 33): the warning to print. The write's exit status is unchanged.
    Failed(String),
}

/// The release step of `volume write` / `volume resume`, run only after the
/// write returned `Ok` — sealed and confirmed, quick or full. The rule is
/// the one `collection run` applies, through the same function
/// ([`crate::staging::release::release_if_covered`]), over the units the
/// volume now holds a completed write of. Never fails the write: an error
/// becomes [`StagingRelease::Failed`], the warning naming the step and
/// `tapectl staging clean`, which finishes it.
fn release_staging_after_write(
    conn: &Connection,
    config: &Config,
    label: &str,
    keep_staging: bool,
) -> StagingRelease {
    if keep_staging {
        return StagingRelease::Kept;
    }
    let outcome = crate::staging::release::unit_ids_written_to(conn, label).and_then(|ids| {
        if ids.is_empty() {
            Ok(None)
        } else {
            crate::staging::release::release_if_covered(conn, config, &ids).map(Some)
        }
    });
    match outcome {
        Ok(None) => StagingRelease::NothingWritten,
        Ok(Some(o)) => match o.cleaned {
            Some(cleaned) => StagingRelease::Released(cleaned),
            None => StagingRelease::Retained(o.under_copied),
        },
        Err(e) => StagingRelease::Failed(crate::error::unfinished_after_seal(
            label,
            "releasing staging",
            &e,
            Some("tapectl staging clean"),
        )),
    }
}

/// `base` with the release's keys added (issue #440), the names
/// `collection run --json` already uses where they mean the same thing.
/// Always present so a scripted consumer never branches on whether a key
/// exists: `staging_released`, `stage_sets_released`, `bytes_freed`,
/// `under_copied` (empty unless a unit kept the staging), `keep_staging`
/// and `release_warning` (null unless the release failed).
fn staging_release_json(
    mut base: serde_json::Value,
    release: &StagingRelease,
) -> serde_json::Value {
    let (released, sets, bytes) = match release {
        StagingRelease::Released(c) => (true, c.sets_cleaned, c.bytes_freed),
        _ => (false, 0, 0),
    };
    let under: Vec<serde_json::Value> = match release {
        StagingRelease::Retained(units) => units
            .iter()
            .map(|p| {
                serde_json::json!({
                    "unit": p.unit_name,
                    "copies": p.copies,
                    "min_copies": p.min_copies,
                })
            })
            .collect(),
        _ => Vec::new(),
    };
    let warning = match release {
        StagingRelease::Failed(w) => Some(w.as_str()),
        _ => None,
    };
    if let Some(obj) = base.as_object_mut() {
        obj.insert("staging_released".into(), released.into());
        obj.insert("stage_sets_released".into(), sets.into());
        obj.insert("bytes_freed".into(), bytes.into());
        obj.insert("under_copied".into(), under.into());
        obj.insert(
            "keep_staging".into(),
            matches!(release, StagingRelease::Kept).into(),
        );
        obj.insert("release_warning".into(), warning.into());
    }
    base
}

/// The human lines for [`StagingRelease`], printed after the write's own
/// "write completed" line (which the gate greps and stays unchanged).
/// Problems — a failed release, a file that could not be unlinked — go to
/// stderr, as warnings.
fn print_staging_release(label: &str, release: &StagingRelease) {
    match release {
        StagingRelease::Kept => println!(
            "staging kept (--keep-staging); `tapectl staging clean` releases it when you want it gone"
        ),
        StagingRelease::NothingWritten => {}
        StagingRelease::Retained(units) => {
            println!(
                "staging kept: not every unit written has its policy's copies yet, and the next \
                 copy reuses these staged slices:"
            );
            for p in units {
                println!("  {}", p.describe());
            }
            println!(
                "  write another copy with `tapectl volume write <label>`; `tapectl staging \
                 clean` releases the units that are covered"
            );
        }
        StagingRelease::Released(r) => {
            if r.sets_cleaned == 0 {
                println!("staging: no stage set was eligible for release");
            } else {
                println!(
                    "staging released: {} stage set(s), {} freed (every unit written has its \
                     policy's copies)",
                    r.sets_cleaned,
                    crate::util::format_bytes_binary(r.bytes_freed),
                );
            }
            for (id, labels) in &r.held_by_live_session {
                println!(
                    "  stage set {id} kept: a live session is using it on volume(s) {}",
                    labels.join(", ")
                );
            }
            if !r.stranded.is_empty() {
                // `staging_path` is nulled before the unlink (issue #108), so
                // no later `staging clean` can find these: name them.
                eprintln!(
                    "warning: volume \"{label}\" is sealed and confirmed and counts as a copy, \
                     but {} staged file(s) could NOT be removed and no later `staging clean` \
                     can find them. Remove by hand:",
                    r.stranded.len()
                );
                for p in &r.stranded {
                    eprintln!("  {}", p.display());
                }
            }
        }
        StagingRelease::Failed(warning) => {
            println!("staging NOT released");
            eprintln!("{warning}");
        }
    }
}

/// `volume identify`'s drive work, shared with `--loaded`: the no-medium
/// check, the MAM read, the read-only store, and the corroborated File 0
/// read, recorded as the `volume identify` contact it is. One definition so
/// the drive steps (and the guards that count them) stay one.
fn identify_loaded_tape(
    conn: &Connection,
    config: &Config,
    device: &str,
) -> Result<write::Identified> {
    // Issue #166: refuse before the store is opened if this drive cannot
    // read the loaded medium. Proceeds silently with no configured backend
    // (ADR-0010's DR-path leniency).
    let reads = crate::tape::mam_journal::MamReads::new(conn, Operation::VolumeIdentify);
    reads.check_read_contact(config, device)?;
    // Before the store open: reading the MAM opens the device read-only and
    // drops the fd, and the st driver refuses a second concurrent open.
    let observed = crate::volume::binding::loaded_medium(config, device, &reads);
    let mut store = TapeStore::open_read(device, DEFAULT_BLOCK_SIZE)?;
    // Corroborated (ADR-0012, issue #193) where there is a catalog row to
    // compare against; the bare `volume_identify` stays the DB-less File 0
    // reader the heir path mirrors.
    write::volume_identify_corroborated(
        conn,
        &mut store,
        ContactSite::new(
            config,
            Operation::VolumeIdentify,
            device,
            Medium::from_read(observed.as_ref().map(|(b, m)| (*b, m))),
        )
        .with_mam_reads(&reads),
    )
}

/// What a `--loaded` write command works on.
#[derive(Clone, Copy)]
enum LoadedFor {
    /// `volume write --loaded`: an initialized volume nothing is written to.
    Write,
    /// `volume resume --loaded`: a volume `volume resume` would pick up.
    Resume,
}

/// `--loaded` (issue #435): which volume of this catalog the loaded tape is,
/// and whether it is in the state this command works on. A `volume
/// identify` contact reads File 0; the volume is found by uuid; a state the
/// command does not work on is refused naming it. The label comes back, and
/// the command goes on exactly as if it had been typed — including the
/// write's own File 0 consent check on its own handle, which a tape swapped
/// in since this read fails (ADR-0003).
///
/// This is the one place a write-family command reads the tape before its
/// own contact, and the reason it is a recorded `volume identify` contact
/// rather than a private peek: it is that command, run for this purpose. A
/// tape the catalog contradicts (`Identified::contradiction`) is not chosen.
fn resolve_loaded_for(
    conn: &Connection,
    config: &Config,
    device: &str,
    wants: LoadedFor,
) -> Result<String> {
    use crate::volume::loaded;
    let id = identify_loaded_tape(conn, config, device)?;
    if let Some(why) = id.contradiction {
        return Err(TapectlError::Other(format!(
            "--loaded does not choose a tape the catalog contradicts. {why}"
        )));
    }
    let file0 = crate::volume::binding::file0_facts_from_text(&id.text);
    let volume = loaded::resolve(conn, &file0)?;
    let doing = match wants {
        LoadedFor::Write => {
            loaded::require_writable(conn, &volume)?;
            "writing"
        }
        LoadedFor::Resume => {
            loaded::require_resumable(conn, &volume)?;
            "resuming"
        }
    };
    loaded::announce(doing, &volume);
    Ok(volume.label)
}

/// What `--move-to` did once a copy was sealed and confirmed (issue #438).
#[derive(Debug)]
enum MoveAfterWrite {
    /// `--move-to` was not given.
    NotAsked,
    /// The volume and its cartridge now sit at `to`.
    Moved {
        to: String,
        outcome: crate::cli::location::MoveOutcome,
    },
    /// The move failed after the copy stood (ADR-0012 2026-10-07 item 33):
    /// the warning to print, naming the `volume move` that finishes it. The
    /// write's exit status is unchanged.
    Failed { to: String, warning: String },
}

/// The move step of `volume write --move-to`, run only after the write
/// returned `Ok` — sealed and confirmed, quick or full. The one mover
/// `volume move` and `cartridge move` share
/// ([`crate::cli::location::move_volume`]), so the shelf and the catalog
/// cannot drift (ADR-0011). Never fails the write: an error becomes
/// [`MoveAfterWrite::Failed`].
fn move_after_write(conn: &Connection, label: &str, move_to: Option<&str>) -> MoveAfterWrite {
    let Some(to) = move_to else {
        return MoveAfterWrite::NotAsked;
    };
    match crate::cli::location::move_volume(conn, label, to, false) {
        Ok(outcome) => MoveAfterWrite::Moved {
            to: to.trim().to_string(),
            outcome,
        },
        Err(e) => MoveAfterWrite::Failed {
            to: to.to_string(),
            warning: crate::error::unfinished_after_seal(
                label,
                "moving the volume",
                &e,
                Some(&format!("tapectl volume move {label} --to {to}")),
            ),
        },
    }
}

/// Refuse a `--move-to` the real move would refuse for its DESTINATION,
/// before the tape is touched: a mistyped location found after hours of
/// writing is a warning about a copy that is already done, and one found
/// first costs nothing. Only the two facts about the destination are asked
/// (it exists; it is not a warehouse, which no physical cartridge can be
/// moved into); the mover itself, [`move_after_write`], still decides the
/// move and reports whatever else it finds as a warning.
fn check_move_destination(conn: &Connection, location: &str) -> Result<()> {
    use rusqlite::OptionalExtension;
    let name = location.trim();
    let kind: Option<String> = conn
        .query_row("SELECT kind FROM locations WHERE name = ?1", [name], |r| {
            r.get(0)
        })
        .optional()?;
    match kind.as_deref() {
        Some("warehouse") => Err(TapectlError::Other(format!(
            "--move-to \"{name}\" is a warehouse location, and a physical cartridge cannot be \
             moved into one. Nothing was written."
        ))),
        Some(_) => Ok(()),
        None => {
            let mut stmt = conn.prepare("SELECT name FROM locations ORDER BY name")?;
            let known: Vec<String> = stmt
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            let list = if known.is_empty() {
                "no locations are defined yet — add one with `tapectl location add <NAME>`"
                    .to_string()
            } else {
                format!("known locations: {}", known.join(", "))
            };
            Err(TapectlError::Other(format!(
                "--move-to location \"{name}\" not found ({list}). Nothing was written."
            )))
        }
    }
}

/// The human lines for [`MoveAfterWrite`], printed after the staging lines
/// so a warning — which carries the command that finishes the step — is the
/// last thing on the screen. The moved lines are `volume move`'s own.
fn print_move_after_write(label: &str, moved: &MoveAfterWrite) {
    match moved {
        MoveAfterWrite::NotAsked => {}
        MoveAfterWrite::Moved { to, outcome } => {
            println!("volume \"{label}\" moved to \"{to}\"");
            if let Some(barcode) = &outcome.cartridge {
                println!("  cartridge \"{barcode}\" moved with it");
                let others: Vec<&str> = outcome
                    .volumes
                    .iter()
                    .filter(|l| l.as_str() != label)
                    .map(|l| l.as_str())
                    .collect();
                if !others.is_empty() {
                    println!(
                        "  {} other volume(s) on that cartridge moved too: {}",
                        others.len(),
                        others.join(", ")
                    );
                }
            }
        }
        MoveAfterWrite::Failed { to, warning } => {
            println!("volume \"{label}\" NOT moved to \"{to}\"");
            eprintln!("{warning}");
        }
    }
}

/// The two library calls of a `volume write` that touch the drive, as a
/// seam: the arm's own logic — what runs after what, and what does not run
/// when a step stops — is then testable with no tape, as the write path's
/// own `ContactStore` seam does for the library. Production is
/// [`DriveWriteSteps`].
trait WriteSteps {
    /// What the write refuses from the catalog and the disk alone, asked
    /// before `--init` initialises anything (issue #445).
    fn preflight(&self, label: &str) -> Result<()>;
    /// `volume init <label>`; returns the new volume's id and the decision
    /// about the medium that went onto its row (issue #482).
    fn init(&self, label: &str, force: bool) -> Result<Initialized>;
    /// `volume write <label>`; `Ok` only when sealed and confirmed.
    fn write(&self, label: &str, force: bool) -> Result<()>;
}

/// [`WriteSteps`] on the real drive.
struct DriveWriteSteps<'a> {
    conn: &'a Connection,
    paths: &'a TapectlPaths,
    config: &'a Config,
    device: &'a str,
    allow_missing_escrow: bool,
    prewrite_hash: bool,
    full_confirm: bool,
    assume_yes: bool,
}

impl WriteSteps for DriveWriteSteps<'_> {
    fn preflight(&self, label: &str) -> Result<()> {
        write::write_catalog_preflight(self.conn, label, self.allow_missing_escrow)
    }

    fn init(&self, label: &str, force: bool) -> Result<Initialized> {
        write::volume_init(
            self.conn,
            self.config,
            label,
            self.device,
            DEFAULT_BLOCK_SIZE,
            force,
            None,
            None,
        )
    }

    fn write(&self, label: &str, force: bool) -> Result<()> {
        write::volume_write(
            self.conn,
            self.paths,
            self.config,
            label,
            self.device,
            DEFAULT_BLOCK_SIZE,
            force,
            self.allow_missing_escrow,
            self.prewrite_hash,
            self.full_confirm,
            self.assume_yes,
        )
    }
}

/// What the operator asked of one `volume write`.
struct WriteRequest<'a> {
    label: &'a str,
    /// The label came from the loaded tape (`--loaded`), not the command line.
    loaded: bool,
    init: bool,
    force: bool,
    move_to: Option<&'a str>,
    keep_staging: bool,
    /// `--json`: a write that fails after `--init` initialised the volume
    /// says so on stdout ([`left_initialized_json`]).
    json_output: bool,
}

/// What `volume init` prints (issue #482): the volume line it always
/// printed, then one line naming the medium it decided on and what decided
/// it, so an LTO-5 cartridge in an LTO-6 drive is seen to have been taken as
/// LTO-5 at init, not discovered later in `volume info`.
fn init_text(label: &str, init: &Initialized) -> String {
    format!(
        "volume \"{label}\" initialized (id={})\n{}",
        init.volume_id,
        init.medium.operator_line()
    )
}

/// `volume init --json`: the keys it always had, plus the additive `medium`
/// object (issue #482) -- the same facts as [`init_text`]'s second line, raw.
fn init_json(label: &str, init: &Initialized) -> serde_json::Value {
    serde_json::json!({
        "volume_id": init.volume_id,
        "label": label,
        "status": "initialized",
        "medium": init.medium.to_json(),
    })
}

/// What one `volume write` did, once the volume was sealed and confirmed.
#[derive(Debug)]
struct WriteDone {
    /// The volume was chosen from the loaded tape (`--loaded`).
    loaded: bool,
    /// The volume `--init` created and the medium it decided on, when it ran.
    initialized: Option<Initialized>,
    release: StagingRelease,
    moved: MoveAfterWrite,
}

/// `volume write`, with its optional `--init` before and `--move-to` after
/// (issue #438, ADR-0012 2026-10-15 item 59).
///
/// Strictly in order, each step behind the previous one's `?`: with `--init`
/// the write's catalog-only pre-flight (issue #445), the init, the write, and
/// only once the write returned `Ok` — sealed AND confirmed; every other
/// outcome left through that `?` — the two post-write steps. A pre-flight
/// or an init that refuses returns before the write is attempted, so nothing
/// is written and the error exits by the write family's table (an
/// unclassified error: 2, or 75 for a busy catalog).
///
/// The pre-flight is what `volume write` would refuse from the catalog and
/// the disk alone — nothing staged, a staged slice missing, a tenant with no
/// key — asked before the cartridge is initialised, so such a refusal leaves
/// the tape untouched and no volume row. What only the contact can raise (the
/// quiet-host question, the capacity gate, a File 0 that changed since the
/// init) still comes after the init, and a write that refuses there exits by
/// the write family's table as it always did — 2 for a refusal before the
/// tape moved — though the volume IS initialised. Exit 2 does not say so;
/// the stderr line and `--json`'s `initialized: true`
/// ([`left_initialized_json`]) do, and are the contract for a script.
///
/// `--force` goes to the init only. The write that follows is never forced:
/// the init has just written File 0, so a force there could only matter if
/// the cartridge had been swapped in between, and that is exactly the case
/// the write's own File 0 check exists to refuse.
///
/// The staging release runs before the move. Neither depends on the other —
/// the release counts copies (`policy::coverage::copy_count_expr` has no
/// location term) and the move changes only where the cartridge sits — so
/// the order is for the operator: the move's warning, which carries the
/// command that finishes it, is then the last thing printed.
fn write_and_follow(
    conn: &Connection,
    config: &Config,
    steps: &dyn WriteSteps,
    req: &WriteRequest<'_>,
) -> Result<WriteDone> {
    let initialized = if req.init {
        // Issue #445: before the cartridge is touched.
        steps.preflight(req.label)?;
        let init = steps.init(req.label, req.force)?;
        // stderr, so `--json` keeps stdout to the one document. The write
        // below may stop; the operator is told what stands and how to go on.
        eprintln!(
            "volume \"{0}\" initialized (id={1}); writing it now. If the write does not \
             complete the volume stays initialized: `tapectl volume write {0}` (without \
             --init) writes it, `tapectl volume resume {0}` continues an interrupted write.",
            req.label, init.volume_id
        );
        // What the init decided about the medium (issue #482), where the
        // operator can see it before the hours of writing it commits them to.
        eprintln!("{}", init.medium.operator_line());
        Some(init)
    } else {
        None
    };
    let written = steps.write(req.label, req.force && !req.init);
    // Issue #445: a write that fails leaves the volume this command
    // initialised initialised, and says so before the error is printed.
    if let (Err(e), Some(init)) = (&written, &initialized) {
        report_left_initialized(req, init, e);
    }
    written?;
    // Reached only when the write above returned `Ok` — sealed AND
    // confirmed. Issues #440 and #438.
    let release = release_staging_after_write(conn, config, req.label, req.keep_staging);
    let moved = move_after_write(conn, req.label, req.move_to);
    Ok(WriteDone {
        loaded: req.loaded,
        initialized,
        release,
        moved,
    })
}

/// What `volume write --init --json` prints on stdout when its write half
/// failed after the init half had initialised the volume (issue #445): the
/// exit code is the write's, and a refusal before the tape moved exits 2,
/// the code that elsewhere means "the medium untouched". `initialized` is
/// the key the success document already has; `volume_status` is what the
/// catalog holds now.
fn left_initialized_json(
    label: &str,
    init: &Initialized,
    error: &TapectlError,
) -> serde_json::Value {
    serde_json::json!({
        "label": label,
        "status": "failed",
        "initialized": true,
        "volume_id": init.volume_id,
        "volume_status": "initialized",
        "medium": init.medium.to_json(),
        "error": error.to_string(),
    })
}

/// A write that failed after `--init` initialised the volume: say so, on
/// stderr, and with `--json` on stdout (issue #445). The pre-write line told
/// the operator the volume would stay initialised; this one is the verdict,
/// printed after the write stopped and before the error itself, and names
/// the exact retry — `--init` again would be refused, the volume exists.
fn report_left_initialized(req: &WriteRequest<'_>, init: &Initialized, error: &TapectlError) {
    let volume_id = init.volume_id;
    eprintln!(
        "volume \"{0}\" was initialized by this command (id={volume_id}) and stays \
         initialized, whatever the write that followed did: its exit code describes the write, \
         not the init. `tapectl volume write {0}` (without --init, which would be refused: the \
         volume exists) writes it; `tapectl volume resume {0}` continues an interrupted write.",
        req.label
    );
    if req.json_output {
        println!("{}", left_initialized_json(req.label, init, error));
    }
}

/// `volume write --json`: the keys it always had, plus the staging release's
/// (issue #440) and — all additive — `initialized`, `moved_to`,
/// `move_warning`, `moved_cartridge` and `volumes_moved` (issue #438), and
/// `loaded` (issue #435). Always
/// present, so a scripted consumer never branches on whether a key exists:
/// `moved_to` and `move_warning` are null unless `--move-to` moved or failed.
fn write_done_json(label: &str, done: &WriteDone) -> serde_json::Value {
    let mut base = staging_release_json(
        serde_json::json!({
            "label": label,
            "status": "completed",
            "initialized": done.initialized.is_some(),
            // Null without `--init`, like `moved_to` without `--move-to`
            // (issue #482): what the init decided about the medium.
            "medium": done.initialized.as_ref().map(|i| i.medium.to_json()),
            "loaded": done.loaded,
        }),
        &done.release,
    );
    let (moved_to, warning, cartridge, volumes): (
        Option<&str>,
        Option<&str>,
        Option<&str>,
        Vec<&str>,
    ) = match &done.moved {
        MoveAfterWrite::NotAsked => (None, None, None, Vec::new()),
        MoveAfterWrite::Moved { to, outcome } => (
            Some(to.as_str()),
            None,
            outcome.cartridge.as_deref(),
            outcome.volumes.iter().map(String::as_str).collect(),
        ),
        MoveAfterWrite::Failed { warning, .. } => (None, Some(warning.as_str()), None, Vec::new()),
    };
    if let Some(obj) = base.as_object_mut() {
        obj.insert("moved_to".into(), moved_to.into());
        obj.insert("move_warning".into(), warning.into());
        obj.insert("moved_cartridge".into(), cartridge.into());
        obj.insert("volumes_moved".into(), volumes.into());
    }
    base
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
            // Issue #241: a meaningful preview would have to open the
            // drive and read the MAM/density register to detect the
            // medium (ADR-0010) — most of the write path with none of its
            // safety. Checked before `write_device` so the refusal is not
            // itself a side effect.
            if dry_run {
                return Err(crate::cli::refuse_dry_run(
                    "volume init",
                    "a real preview would have to open the drive and detect the loaded \
                     medium's generation (ADR-0010) to know what would be written.",
                ));
            }
            let device = write_device(config, device.as_deref())?;
            let init = write::volume_init(
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
                println!("{}", init_json(label, &init));
            } else {
                println!("{}", init_text(label, &init));
            }
        }

        VolumeCommands::Write {
            label,
            loaded,
            device,
            force,
            init,
            move_to,
            allow_missing_escrow,
            prewrite_hash,
            fill_ceiling,
            full_confirm,
            keep_staging,
        } => {
            // Issue #241: a real preview would have to open the drive and
            // re-derive the whole layout (session build/validate/plan) —
            // most of the write path with none of its safety. Checked
            // before `write_device` so the refusal is not itself a side
            // effect. `volume plan` gives an estimate with no drive needed.
            if dry_run {
                return Err(crate::cli::refuse_dry_run(
                    "volume write",
                    "a real preview would have to open the drive and re-derive the layout \
                     session (build/validate/plan) to know what would be written. \
                     `volume plan` estimates tapes needed with no drive required.",
                ));
            }
            // Issue #438: a mistyped `--move-to` is refused before the tape
            // is touched, not found after the copy is done.
            if let Some(to) = move_to {
                check_move_destination(conn, to)?;
            }
            let device = write_device(config, device.as_deref())?;
            // Issue #435: `--loaded` names the volume from the loaded tape's
            // File 0 — resolved here, before the write's own contact, and
            // refused unless the volume is one a write works on. `label` is
            // then the volume it chose, and the write goes on as typed.
            let from_loaded;
            let label: &str = match (label.as_deref(), *loaded) {
                (Some(label), false) => label,
                (None, true) => {
                    from_loaded = resolve_loaded_for(conn, config, &device, LoadedFor::Write)?;
                    &from_loaded
                }
                _ => {
                    return Err(TapectlError::Other(
                        "volume write takes a label or --loaded, not both and not neither".into(),
                    ))
                }
            };
            // Issue #391: `--fill-ceiling` for this write only.
            let config = &config.with_fill_ceiling(*fill_ceiling);
            let steps = DriveWriteSteps {
                conn,
                paths,
                config,
                device: &device,
                allow_missing_escrow: *allow_missing_escrow,
                prewrite_hash: *prewrite_hash,
                full_confirm: *full_confirm,
                assume_yes: yes,
            };
            let done = write_and_follow(
                conn,
                config,
                &steps,
                &WriteRequest {
                    label,
                    loaded: *loaded,
                    init: *init,
                    force: *force,
                    move_to: move_to.as_deref(),
                    keep_staging: *keep_staging,
                    json_output,
                },
            )?;
            if json_output {
                println!("{}", write_done_json(label, &done));
            } else {
                println!("volume \"{label}\" write completed");
                print_staging_release(label, &done.release);
                print_move_after_write(label, &done.moved);
            }
        }

        VolumeCommands::Resume {
            label,
            loaded,
            device,
            prewrite_hash,
            full_confirm,
            keep_staging,
        } => {
            // Issue #241: same reasoning as `volume write` — resuming
            // reopens the drive and revalidates the frozen staging files
            // against it before anything is known about what remains to
            // write.
            if dry_run {
                return Err(crate::cli::refuse_dry_run(
                    "volume resume",
                    "a real preview would have to open the drive and revalidate the \
                     interrupted session's staged slices against it.",
                ));
            }
            let device = write_device(config, device.as_deref())?;
            // Issue #435: as for `volume write`, `--loaded` takes the
            // volume's name from the loaded tape's File 0, refusing unless
            // the volume is one `volume resume` would pick up.
            let from_loaded;
            let label: &str = match (label.as_deref(), *loaded) {
                (Some(label), false) => label,
                (None, true) => {
                    from_loaded = resolve_loaded_for(conn, config, &device, LoadedFor::Resume)?;
                    &from_loaded
                }
                _ => {
                    return Err(TapectlError::Other(
                        "volume resume takes a label or --loaded, not both and not neither".into(),
                    ))
                }
            };
            write::volume_resume(
                conn,
                paths,
                config,
                label,
                &device,
                DEFAULT_BLOCK_SIZE,
                *prewrite_hash,
                *full_confirm,
            )?;
            // As for `volume write`: only an `Ok` resume (sealed and
            // confirmed, whichever of its three routes) gets here. Issue #440.
            let release = release_staging_after_write(conn, config, label, *keep_staging);
            if json_output {
                println!(
                    "{}",
                    staging_release_json(
                        serde_json::json!({
                            "label": label,
                            "status": "completed",
                            "resumed": true,
                            "loaded": loaded,
                        }),
                        &release
                    )
                );
            } else {
                println!("volume \"{label}\" write resumed and completed");
                print_staging_release(label, &release);
            }
        }

        // `yes` here is `run`'s GLOBAL parameter -- `Abort` no longer
        // declares a local one (issue #240).
        //
        // The removed field was not a bug in itself: it shared clap's arg id
        // with the global, and `global = true` unifies matches by id, so
        // `--yes` typed in either position set both and `*yes` alone was
        // correct (that was issue #237, closed as not-reproducible after
        // testing the binary rather than reading the source). What it could
        // not share was the global's `short`, so `-y` was accepted before the
        // subcommand and rejected after it.
        //
        // Do NOT reintroduce a local field "for clarity". The sibling arms
        // `CompactFinish`/`Compact` do `*force || yes`, and that OR is
        // load-bearing there only because `force` is a genuinely different
        // arg id -- a resemblance that has already produced one false issue.
        VolumeCommands::Abort { label } => {
            // Issue #241: `volume_abort` (src/volume/write.rs) is outside
            // this fix's file scope, and its own ADR-0008 Tier-2 consent
            // prompt already IS the preview an operator would ask a dry
            // run for — it names exactly what a real run would change
            // before doing so. Refusing here is honest rather than
            // reimplementing that prompt as a second code path.
            if dry_run {
                return Err(crate::cli::refuse_dry_run(
                    "volume abort",
                    "run it interactively (without --yes) to see the confirmation prompt, \
                     which already names every session row that would be marked aborted \
                     before anything changes.",
                ));
            }
            let seal = write::volume_abort(conn, label, yes)?;
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "label": label,
                        "status": "aborted",
                        "seal_recorded": seal.seal_recorded(),
                        "reconfirmable": seal == write::AbortSeal::SealedReconfirmable,
                    })
                );
            } else {
                println!("{}", write::abort_done_message(label, &seal));
            }
        }

        VolumeCommands::Verify {
            label,
            device,
            full: _,
            quick,
        } => {
            // Issue #241: not read-only the way it looks — a verify writes
            // a `verification_sessions` row and, on qualifying failures,
            // flips `observed_condition` to `quarantined` (ADR-0012,
            // 2026-09-17). The read IS the check; there is nothing left to
            // preview once the tape has not been read.
            if dry_run {
                return Err(crate::cli::refuse_dry_run(
                    "volume verify",
                    "the verify IS the read — it records a verification_sessions row and \
                     can quarantine the medium's observed_condition, so there is nothing \
                     meaningful to preview without actually reading the tape.",
                ));
            }
            let tier = if *quick {
                Tier::Navigable
            } else {
                Tier::Integrity
            };
            let tier_name = if *quick { "quick" } else { "full" };
            let device = read_device(config, device.as_deref())?;
            // Issue #435: no label verifies the loaded tape, found by what
            // its File 0 says; `loaded` is then true and `label` is the
            // volume it chose. With a label this is `volume verify` as it
            // always was.
            let (label, loaded, report) = match label {
                Some(label) => {
                    let report = write::volume_verify(
                        conn,
                        config,
                        label,
                        &device,
                        DEFAULT_BLOCK_SIZE,
                        tier,
                    )?;
                    (label.clone(), false, report)
                }
                None => {
                    let (found, report) = write::volume_verify_loaded(
                        conn,
                        config,
                        &device,
                        DEFAULT_BLOCK_SIZE,
                        tier,
                    )?;
                    (found.label, true, report)
                }
            };
            let label = &label;
            // Issue #280: read ONCE, before either output branch, so the
            // human message and its `--json` twin can never derive "does
            // this volume count as a copy" two different ways.
            // `counts_as_copy` routes through `policy::coverage::eligible`
            // (issue #96's sole owner of every `volumes.status` predicate)
            // — never hand-write `status = 'sealed'` at this call site.
            let current_volume_status: String = conn
                .query_row(
                    "SELECT status FROM volumes WHERE label = ?1",
                    [label.as_str()],
                    |r| r.get(0),
                )
                .unwrap_or_default();
            let volume_sealed_at_set: bool = conn
                .query_row(
                    "SELECT sealed_at IS NOT NULL FROM volumes WHERE label = ?1",
                    [label.as_str()],
                    |r| r.get(0),
                )
                .unwrap_or(false);
            let volume_counts_as_copy =
                crate::policy::coverage::counts_as_copy(conn, label).unwrap_or(false);
            // Issue #280, ADR-0012's 2026-09-23 amendment: whether `volume
            // resume` would pick this volume up and re-confirm it NOW — read
            // after this verify's own row is recorded, since that row may be
            // the very evidence that makes an aborted session adoptable. The
            // one derivation `volume resume` itself uses (#96).
            let volume_resume_would_reconfirm: bool = conn
                .query_row(
                    "SELECT id FROM volumes WHERE label = ?1",
                    [label.as_str()],
                    |r| r.get::<_, i64>(0),
                )
                .ok()
                .and_then(|id| crate::volume::session::resume_would_reconfirm(conn, id).ok())
                .unwrap_or(false);
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
                        // Issue #435, additive: whether the volume was found
                        // from the loaded tape's File 0 (no label given).
                        "loaded": loaded,
                        "tier": tier_name,
                        "checked": report.checked,
                        "passed": report.passed,
                        "failed": report.failed,
                        "mismatches": mismatches,
                        // Issue #234: `quarantined` is the headline answer;
                        // `quarantine` carries what the condition WAS,
                        // because a volume whose medium was already known
                        // bad is a different fact from one this verify took
                        // out of service. `null` on a clean verify and on a
                        // failed one that proved nothing about the medium.
                        "quarantined": report.quarantine.is_some(),
                        // Issue #268, additive: the inverse of `quarantined`.
                        // True only when this verify actually returned the
                        // volume to service, never on the ordinary clean
                        // verify of a healthy one. Issue #280 corrected the
                        // semantics without renaming the field: a clean
                        // clear on a `retired` or otherwise non-`sealed`
                        // volume no longer claims `true` here — that was
                        // the durability lie this field existed to avoid,
                        // just stated for the wrong half of the condition.
                        // `condition_changed` alone (which `cleared` below
                        // still carries unconditionally) is enough to tell
                        // a machine reader "the condition moved"; this
                        // field now also answers "and it counts".
                        "returned_to_service": returned_to_service_json(
                            report.cleared.is_some(),
                            volume_counts_as_copy,
                        ),
                        "cleared": report.cleared.as_ref().map(|c| serde_json::json!({
                            "previous_condition": c.previous_condition,
                            "condition_changed": true,
                            // Issue #280: the same derived answer the human
                            // message gives, plus the status it is derived
                            // from, so a machine reader is never left to
                            // infer "does this count" from `status` and
                            // `eligible` on its own.
                            "counts_as_copy": volume_counts_as_copy,
                            "status": current_volume_status,
                            // Issue #280, additive: the same answer that
                            // decides whether the human message names
                            // `tapectl volume resume <label>`.
                            "resume_would_reconfirm": volume_resume_would_reconfirm,
                        })),
                        "quarantine": report.quarantine.as_ref().map(|q| {
                            // Issue #242: a verify no longer touches
                            // `status` at all -- `previous_status`/
                            // `status_changed` are kept (additive-JSON
                            // rule) but now honestly report a status that
                            // never moves; `previous_condition`/
                            // `condition_changed` are the new, accurate
                            // pair the ADR's amendment is actually about.
                            let current_status: String = conn
                                .query_row(
                                    "SELECT status FROM volumes WHERE label = ?1",
                                    [label.as_str()],
                                    |r| r.get(0),
                                )
                                .unwrap_or_default();
                            serde_json::json!({
                                "previous_status": current_status,
                                "status_changed": false,
                                "previous_condition": q.previous_condition,
                                "condition_changed": q.condition_changed(),
                                "proof": q.proof.iter().map(|m| serde_json::json!({
                                    "position": m.position,
                                    "kind": m.kind.label(),
                                })).collect::<Vec<_>>(),
                            })
                        }),
                        "drive_health_note": report.drive_health_note,
                        // ADR-0012 2026-10-07 item 33 (#423), additive:
                        // `null` when this verify recorded nothing.
                        "figures": figures_json(report.figures.as_ref()),
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
                        if q.condition_changed() {
                            println!(
                                "volume \"{label}\" QUARANTINED: {} of {} failure(s) prove the \
                                 medium is bad. Its status is untouched (ADR-0012, 2026-09-17) — \
                                 only its condition changed, from \"{}\" to \"quarantined\" — but \
                                 it no longer counts as a copy, so `volume retire` will no longer \
                                 refuse it as the last one — salvage what still reads off it \
                                 first (`volume read-slices --from {label} --unit <UNIT>`).",
                                q.proof.len(),
                                report.failed,
                                q.previous_condition,
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
                    // Issue #268: the clean-pass arm is no longer silent when
                    // this verify RETURNED the volume to service. That is a
                    // real change to the catalog -- the volume starts counting
                    // as a copy again -- and an operator who cleaned the drive
                    // and re-verified needs to be told it worked, in the same
                    // place they were told it failed.
                    //
                    // Issue #280: "counts as a copy again" is only true while
                    // `status = 'sealed'` -- `clear_condition_on_clean_full_
                    // verify` only ever touches `observed_condition`, so a
                    // `retired` volume (salvaged after quarantine, then
                    // deliberately retired) or one stuck `initialized` after
                    // a confirm that never reached the sealing UPDATE
                    // (`session.rs`'s `proves_medium_bad` arm) clears clean
                    // and STILL counts for nothing. `clean_clear_message`
                    // (below) is the single place that decides which of the
                    // three true statements to make, driven by
                    // `counts_as_copy` -- the same predicate `audit`/`report
                    // copies` read -- so this text can never claim more than
                    // the catalog would count.
                    (None, _) => {
                        if let Some(c) = &report.cleared {
                            println!(
                                "{}",
                                clean_clear_message(
                                    label,
                                    &c.previous_condition,
                                    &current_volume_status,
                                    volume_sealed_at_set,
                                    volume_counts_as_copy,
                                    volume_resume_would_reconfirm,
                                )
                            );
                        }
                    }
                }
                // ADR-0012 2026-10-07 item 33 (#423): what this verify
                // recorded of the figures the write left unrecorded.
                if let Some(figures) = &report.figures {
                    println!("{}", figures_message(label, figures));
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
            let id = identify_loaded_tape(conn, config, &device)?;
            // The tape's own account FIRST, always — see `Identified`. A
            // contradiction is reported after it and through the exit code,
            // never by withholding the answer the operator asked for.
            println!("{}", id.text);
            // Issue #435: what the catalog makes of the tape — which volume
            // it is, or that the catalog does not know it (a fact, not an
            // error). On stderr, so stdout stays File 0 for anything parsing
            // it. The note and the exit status below come from ONE lookup
            // (`Identified`, issue #443), so they cannot disagree.
            if let Some(note) = &id.catalog_note {
                eprintln!("{note}");
            }
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
            //
            // Issue #461: a real move waits on no live session of the volume
            // it names. Refused without waiting (exit 75) when one holds the
            // volume's lock; a dry run changes nothing and takes no lock. A
            // label the catalog does not know is `move_volume`'s refusal.
            let _volume_lock = if dry_run {
                None
            } else {
                use rusqlite::OptionalExtension;
                let id: Option<i64> = conn
                    .query_row("SELECT id FROM volumes WHERE label = ?1", [label], |r| {
                        r.get(0)
                    })
                    .optional()?;
                match id {
                    Some(id) => Some(crate::staging::lock::acquire_volume_to_change(
                        conn,
                        id,
                        label,
                        &format!("move volume \"{label}\" to \"{to}\""),
                    )?),
                    None => None,
                }
            };
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

        VolumeCommands::ReadSlices {
            from,
            unit,
            version,
            device,
        } => {
            // Issue #241: same reasoning as `volume compact-read` — a real
            // preview would have to open the drive and chain-walk the
            // volume to know what would be staged, which is the entire
            // operation.
            if dry_run {
                return Err(crate::cli::refuse_dry_run(
                    "volume read-slices",
                    "a real preview would have to open the drive and chain-walk the source \
                     volume to know what would be read.",
                ));
            }
            let device = read_device(config, device.as_deref())?;
            // Issue #166: same fact check as `Identify`, before the store
            // is opened.
            let reads = crate::tape::mam_journal::MamReads::new(conn, Operation::VolumeReadSlices);
            reads.check_read_contact(config, &device)?;
            let observed = crate::volume::binding::loaded_medium(config, &device, &reads);
            let mut store = TapeStore::open_read(&device, DEFAULT_BLOCK_SIZE)?;
            let report = write::read_slices(
                conn,
                config,
                from,
                unit,
                *version,
                &mut store,
                ContactSite::new(
                    config,
                    Operation::VolumeReadSlices,
                    &device,
                    Medium::from_read(observed.as_ref().map(|(b, m)| (*b, m))),
                )
                .with_mam_reads(&reads),
            )?;
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "from": from, "unit": unit,
                        "slices_read": report.slices_read,
                        "bytes_read": report.bytes_read,
                        // Issue #474: additive.
                        "versions": report.versions,
                    })
                );
            } else {
                println!(
                    "read {} slices ({}) of \"{}\" {} from \"{}\" into staging",
                    report.slices_read,
                    crate::util::format_bytes_binary(report.bytes_read),
                    unit,
                    versions_phrase(&report.versions),
                    from,
                );
                // Issue #474: a blank cartridge must be `volume init`-ed
                // first, which `volume write --init` does with the tape
                // still loaded.
                println!(
                    "load the cartridge to copy to and run `tapectl volume write DEST --init \
                     --device {device}` (a cartridge already set up with `tapectl volume init \
                     DEST` takes `tapectl volume write DEST`)",
                );
            }
        }

        VolumeCommands::Plan {
            copies,
            generation,
            device,
            fill_ceiling,
            policy_aware,
        } => {
            let config = &config.with_fill_ceiling(*fill_ceiling);
            // Show what staged data would be written
            let plan_rows = staged_plan_rows(conn, config, *policy_aware)?;
            let rows: Vec<(String, i64, Option<i64>, Option<i64>)> = plan_rows
                .iter()
                .map(|r| (r.unit.clone(), r.version, r.slices, r.size))
                .collect();
            let finding = |i: usize| plan_rows[i].finding.as_deref();

            let total_bytes: i64 = rows.iter().map(|(_, _, _, s)| s.unwrap_or(0)).sum();
            let total_slices: i64 = rows.iter().map(|(_, _, n, _)| n.unwrap_or(0)).sum();
            // The cross-volume index (#418 R4, `volume-format-v2.md` §1.5)
            // every envelope of a tape written from this will carry.
            let index = if rows.is_empty() {
                None
            } else {
                Some(planned_index_bytes(
                    conn,
                    config,
                    generation.as_deref(),
                    device.as_deref(),
                )?)
            };

            // Issue #271: `--json` must be checked BEFORE the `is_empty`
            // branch, not after it -- the empty case used to fall into a
            // plain `println!("no staged data to plan")` regardless of
            // `json_output`, so `--json` on a fresh home emitted a bare
            // English sentence instead of JSON (issue #56's rule: every
            // `--json` mode's WHOLE stdout must parse). The document shape
            // below is the same one the populated case emits, just zeroed.
            if json_output {
                let units: Vec<serde_json::Value> = rows
                    .iter()
                    .enumerate()
                    .map(|(i, (name, ver, slices, size))| {
                        serde_json::json!({"unit": name, "version": ver, "slices": slices,
                                           "size": size, "finding": finding(i)})
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::json!({
                        "copies": copies, "total_slices": total_slices,
                        "total_bytes": total_bytes, "units": units,
                        "policy_aware": policy_aware,
                        // #418 R4: an estimate, see `planned_index_bytes`.
                        "index_bytes": index,
                        "index_bytes_on_tape": index.map(|n| 2 * n),
                    })
                );
            } else if rows.is_empty() {
                println!("no staged data to plan");
            } else {
                println!("volume write plan ({copies} copy/copies):");
                for (i, (name, ver, slices, size)) in rows.iter().enumerate() {
                    println!(
                        "  {name} v{ver}: {} slices, {}{}",
                        slices.unwrap_or(0),
                        crate::util::format_bytes_binary(size.unwrap_or(0)),
                        finding(i).map(|f| format!("  ({f})")).unwrap_or_default(),
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
                let factor = backend.fill_ceiling;
                let usable = backend.fill_budget_bytes(tape_cap as u64) as i64;
                let tapes_needed = ((total_bytes * copies) + usable - 1) / usable;
                println!(
                    "estimated tapes: {tapes_needed} (at the {}% fill ceiling)",
                    (factor * 100.0).round() as i64
                );
                if let Some(index) = index {
                    println!("{}", plan_index_line(index));
                }
            }
        }

        VolumeCommands::CompactRead { label, device } => {
            // Issue #241: a real preview would have to open the drive and
            // chain-walk the volume to know which slices are live — most
            // of this step's own work.
            if dry_run {
                return Err(crate::cli::refuse_dry_run(
                    "volume compact-read",
                    "a real preview would have to open the drive and chain-walk the volume \
                     to know which slices are still live.",
                ));
            }
            let device = read_device(config, device.as_deref())?;
            // Issue #166: same fact check as `Identify`, before the store
            // is opened.
            let reads = crate::tape::mam_journal::MamReads::new(conn, Operation::VolumeCompactRead);
            reads.check_read_contact(config, &device)?;
            let observed = crate::volume::binding::loaded_medium(config, &device, &reads);
            let mut store = TapeStore::open_read(&device, DEFAULT_BLOCK_SIZE)?;
            let report = write::compact_read(
                conn,
                config,
                label,
                &mut store,
                ContactSite::new(
                    config,
                    Operation::VolumeCompactRead,
                    &device,
                    Medium::from_read(observed.as_ref().map(|(b, m)| (*b, m))),
                )
                .with_mam_reads(&reads),
            )?;
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
            prewrite_hash,
            fill_ceiling,
            full_confirm,
        } => {
            // Issue #241: same reasoning as `volume write` — a real
            // preview would have to open the drive and re-derive the
            // write-session layout for the staged compaction slices.
            if dry_run {
                return Err(crate::cli::refuse_dry_run(
                    "volume compact-write",
                    "a real preview would have to open the drive and re-derive the write \
                     session layout for the staged compaction slices.",
                ));
            }
            let device = write_device(config, device.as_deref())?;
            // Issue #391: `--fill-ceiling` for this write only.
            let config = &config.with_fill_ceiling(*fill_ceiling);
            write::compact_write(
                conn,
                paths,
                config,
                destination,
                &device,
                DEFAULT_BLOCK_SIZE,
                *allow_missing_escrow,
                *prewrite_hash,
                *full_confirm,
                yes,
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
            // Issue #241: `compact_finish` (src/volume/write.rs) is
            // outside this fix's file scope, and it already shows its
            // ADR-0008 Tier-2 coverage facts and asks consent before
            // mutating anything (see its own doc comment on gate
            // ordering) — that IS the preview a dry run would offer.
            // Refusing here is honest rather than a second, easily
            // divergent implementation of the same gate.
            if dry_run {
                return Err(crate::cli::refuse_dry_run(
                    "volume compact-finish",
                    "when retiring the source would leave a unit below policy or at zero \
                     copies, this command already shows that impact and asks consent before \
                     mutating anything; run it interactively (without --force/--yes) to see it.",
                ));
            }
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
            prewrite_hash,
            fill_ceiling,
            full_confirm,
            force,
        } => {
            // Issue #241: the interactive 3-step flow opens the drive
            // twice (read, then write) and only its own step-3 prompt
            // (compact-finish) previews anything — refuse for the whole
            // command rather than half-honour it. `volume compact-read`
            // done for real, then `volume compact-finish --force`, is the
            // way to inspect each step without committing further.
            if dry_run {
                return Err(crate::cli::refuse_dry_run(
                    "volume compact",
                    "this runs all three compaction steps, two of which open the drive to \
                     read then write; there is no cheap preview of the whole flow.",
                ));
            }
            // Interactive: run all 3 steps. Strict resolution (ADR-0010):
            // step 2 writes, so this needs a real backend even though step 1
            // only reads.
            let device = write_device(config, device.as_deref())?;
            // Issue #358: ONE drive serves both steps, so someone must swap
            // cartridges between them. With no terminal nobody can, and the
            // refusal comes now — before step 1's read, not after it.
            let to = refuse_unattended_compact(
                crate::cli::consent::stdin_is_terminal(),
                label,
                to.as_deref(),
            )?;
            // Issue #166: step 1 only reads, so it gets the same fact check
            // as every other read path, before its store is opened.
            let reads = crate::tape::mam_journal::MamReads::new(conn, Operation::VolumeCompact);
            reads.check_read_contact(config, &device)?;
            println!("=== Step 1: Reading live slices from \"{label}\" ===");
            // Scoped so the read-only store (and its device fd) closes
            // before step 2 opens the same device for writing — the st
            // driver refuses a second concurrent open (EBUSY).
            let observed = crate::volume::binding::loaded_medium(config, &device, &reads);
            let report = {
                let mut store = TapeStore::open_read(&device, DEFAULT_BLOCK_SIZE)?;
                // `Operation::VolumeCompact`, not `VolumeCompactRead`: this
                // is step 1 of the interactive three-step command, and step 2
                // makes a SECOND, separate contact through `volume_write`
                // (the read-only store must close first — the st driver
                // refuses a second concurrent open), which is physically
                // what happens.
                write::compact_read(
                    conn,
                    config,
                    label,
                    &mut store,
                    ContactSite::new(
                        config,
                        Operation::VolumeCompact,
                        &device,
                        Medium::from_read(observed.as_ref().map(|(b, m)| (*b, m))),
                    )
                    .with_mam_reads(&reads),
                )?
            };
            println!(
                "  Read {} slices ({})",
                report.slices_read,
                crate::util::format_bytes_binary(report.bytes_read),
            );

            // Issue #358: the swap. Step 1's store has closed; the source is
            // still in the drive. `--to` names the destination — it never
            // skips this pause.
            let dest_label = swap_to_destination(to.as_deref(), label, &device)?;
            let dest_label = dest_label.as_str();

            println!("=== Step 2: Writing compaction slices to \"{dest_label}\" ===");
            write::compact_write(
                conn,
                paths,
                // Issue #391: `--fill-ceiling` for step 2's write only.
                &config.with_fill_ceiling(*fill_ceiling),
                dest_label,
                &device,
                DEFAULT_BLOCK_SIZE,
                *allow_missing_escrow,
                *prewrite_hash,
                *full_confirm,
                yes,
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
            //
            // ADR-0012 2026-10-07 item 33 (and the CTO's ruling on its
            // question 2): a consent refusal of step 3 leaves a destination
            // that is sealed, confirmed and a full carry-forward, so the
            // command warns, naming `volume compact-finish --force`, and
            // exits 0. A Tier-3 refusal means content was NOT carried
            // forward — the copy does not stand in item 33's sense — so it
            // keeps its error and its non-zero exit
            // (`compact_step3_outcome`).
            match write::compact_finish(conn, config, label, *force || yes) {
                Ok(report) => {
                    println!("  Volume \"{label}\" retired");
                    if json_output {
                        println!(
                            "{}",
                            serde_json::json!({
                                "source": label,
                                "destination": dest_label,
                                "status": "completed",
                                "source_retired": true,
                                "affected_units": compact_finish_evidence_json(&report),
                            })
                        );
                    } else {
                        print_compact_finish_evidence(&report);
                        println!("\ncompaction complete: {label} → {dest_label}");
                    }
                }
                Err(e) => {
                    let warning = compact_step3_outcome(label, dest_label, e)?;
                    eprintln!("{warning}");
                    if json_output {
                        println!(
                            "{}",
                            serde_json::json!({
                                "source": label,
                                "destination": dest_label,
                                "status": "source_not_retired",
                                "source_retired": false,
                                "warning": warning,
                            })
                        );
                    } else {
                        println!(
                            "\ncompaction written: {label} → {dest_label}; \"{label}\" is not retired"
                        );
                    }
                }
            }
        }

        VolumeCommands::Deposit { command } => run_deposit(conn, command, json_output, dry_run)?,

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

/// What `volume compact` does when step 3 — retiring `source` — fails
/// after step 2 sealed and confirmed `dest` (ADR-0012 2026-10-07 item 33,
/// and the CTO's ruling on its question 2):
///
/// - a **consent refusal** (no terminal and no `--yes`, or a "no" at the
///   prompt): `dest` is a full carry-forward and counts as a copy, so this
///   is `Ok(warning)` — printed, and the command exits 0. The warning names
///   `volume compact-finish <source> --force`, which finishes step 3
///   without re-reading or re-writing anything.
/// - anything else, notably `compact_finish`'s **Tier-3 refusals** (an
///   unprotected live slice; the last eligible copy of a live version):
///   after a successful compact-write either means content was NOT carried
///   forward, so the copy does not stand in item 33's sense. `Err(err)`,
///   unchanged: the same message and non-zero exit as before item 33.
///   Naming `--force` for an absolute floor is the confusion ADR-0008 warns
///   about, and neither refusal's text contains the consent substrings.
fn compact_step3_outcome(
    source: &str,
    dest: &str,
    err: crate::error::TapectlError,
) -> std::result::Result<String, crate::error::TapectlError> {
    let msg = err.to_string();
    let consent =
        msg.contains("refused: non-interactive session") || msg.contains("aborted, not confirmed");
    if !consent {
        return Err(err);
    }
    Ok(crate::error::unfinished_after_seal(
        dest,
        &format!("step 3, retiring the source \"{source}\","),
        &msg,
        Some(&format!("tapectl volume compact-finish {source} --force")),
    ))
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
/// 2. The volume must pass `coverage::eligible` (sealed AND
///    `observed_condition = 'ok'`). You cannot have deposited bytes that
///    were never sealed -- an unsealed volume's bytes are not final, so a
///    copy of them is a copy of nothing durable -- and a volume tapectl has
///    observed to be quarantined is not durable either, sealed or not
///    (issue #255): routed through `coverage::eligible` itself, not a
///    second hand-written `status`-only predicate (issue #96).
fn run_deposit(
    conn: &Connection,
    command: &DepositCommands,
    json_output: bool,
    dry_run: bool,
) -> Result<()> {
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
            // Issue #255: route the refusal through `coverage::eligible`
            // (both `status` AND `observed_condition`) rather than a
            // hand-written `status != "sealed"` check -- migration 017
            // moved quarantine off `status` onto `observed_condition`, so
            // a verify-quarantined volume still reads `status = 'sealed'`
            // and a status-only check no longer catches it (`coverage.rs`
            // is the declared sole owner of every `volumes.status`/
            // `observed_condition` predicate, issue #96). `status`/
            // `condition` are fetched too, but only to WORD the refusal
            // below -- `eligible_expr`, evaluated by SQLite itself, is
            // what actually decides.
            let eligible_expr = crate::policy::coverage::eligible("v");
            let (vol_id, status, condition, eligible): (i64, String, String, bool) = conn
                .query_row(
                    &format!(
                        "SELECT v.id, v.status, v.observed_condition, {eligible_expr} \
                         FROM volumes v WHERE v.label = ?1"
                    ),
                    params![label],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
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
            if !eligible {
                if status != "sealed" {
                    return Err(TapectlError::Other(format!(
                        "volume \"{label}\" is {status}, not sealed; only a sealed volume's \
                         bytes are final, so there is nothing durable to have deposited"
                    )));
                }
                // `status == "sealed"` but `eligible` is still false: the
                // only other thing `coverage::eligible` checks is
                // `observed_condition` (issue #255) -- a prior verify or
                // write-time contact check quarantined this medium, so its
                // bytes are not eligible for a durable warehouse claim even
                // though the write itself completed and sealed.
                return Err(TapectlError::Other(format!(
                    "volume \"{label}\" is sealed but its observed_condition is \
                     \"{condition}\" (ADR-0012, the 2026-09-17 amendment): a prior verify or \
                     write-time contact check found evidence this medium cannot be trusted, so \
                     there is nothing durable to have deposited. `--force` does not apply -- \
                     this is a fact the catalog recorded, not a risk judgement."
                )));
            }

            // Issue #241: both refusals above are facts about the request,
            // not the mutation, so they stay ahead of this return — a dry
            // run must still refuse what the real run would refuse.
            if dry_run {
                if json_output {
                    println!(
                        "{}",
                        serde_json::json!({"volume": label, "location": to,
                                           "receipt": receipt, "storage_class": storage_class,
                                           "dry_run": true})
                    );
                } else {
                    println!(
                        "would record a warehouse deposit of \"{label}\" at \"{to}\" \
                         (DRY RUN — no changes made)"
                    );
                }
                return Ok(());
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
            if dry_run {
                let exists: bool = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM volume_deposits
                      WHERE volume_id = (SELECT id FROM volumes WHERE label = ?1)
                        AND location_id = (SELECT id FROM locations WHERE name = ?2))",
                    params![label, from],
                    |row| row.get(0),
                )?;
                // A dry run must still refuse what the real run would
                // refuse (no recorded deposit to remove).
                if !exists {
                    return Err(TapectlError::Other(format!(
                        "no recorded deposit of \"{label}\" at \"{from}\""
                    )));
                }
                if json_output {
                    println!(
                        "{}",
                        serde_json::json!({"volume": label, "location": from, "dry_run": true})
                    );
                } else {
                    println!(
                        "would remove the recorded deposit of \"{label}\" at \"{from}\" \
                         (DRY RUN — no changes made)"
                    );
                }
                return Ok(());
            }
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

/// `volume compact`'s unattended refusal, taken BEFORE step 1 (issue #358),
/// and the `--to` check that belongs with it. Returns `--to`, trimmed.
///
/// `volume compact` has ONE `--device`: step 1 reads the source through it
/// and step 2 writes the destination through the same drive, so a person has
/// to unload one cartridge and load the other in between. With no terminal
/// there is nobody to do that or to say it is done, and ADR-0008's
/// non-hanging rule forbids waiting for them. Until #358 `--to` made the
/// command run straight through anyway — issue #146 had added it so
/// compaction could run from a script — and on its one drive step 2 then
/// wrote to the SOURCE tape still loaded, where File 0 refused it, after a
/// read that can take hours. Refusing here costs nothing: no drive has been
/// touched. The three separate steps are the unattended recipe, and the
/// refusal names them — `compact-read`, the swap, `compact-write`,
/// `compact-finish` — with the destination filled in when `--to` gave one.
///
/// An empty `--to` is refused here too, before step 1 rather than after it.
fn refuse_unattended_compact(
    is_tty: bool,
    source: &str,
    to: Option<&str>,
) -> Result<Option<String>> {
    let to = match to.map(str::trim) {
        Some("") => {
            return Err(TapectlError::Other(
                "--to was given an empty destination label".into(),
            ))
        }
        other => other.map(str::to_string),
    };
    if is_tty {
        return Ok(to);
    }
    let dest = to.as_deref().unwrap_or("<DEST>");
    Err(TapectlError::Other(format!(
        "volume compact refused: it reads \"{source}\" and writes the destination through \
         ONE drive, so someone has to swap cartridges between the two steps, and stdin is \
         not a terminal — there is nobody to do it or to say it is done. Nothing was read. \
         Run the three steps separately, which works unattended:\n    \
         tapectl volume compact-read {source}\n    \
         (unload \"{source}\", load the destination)\n    \
         tapectl volume compact-write --destination {dest}\n    \
         tapectl volume compact-finish {source}"
    )))
}

/// The pause between `volume compact`'s step 1 and step 2: the operator
/// swaps cartridges, and the destination label is settled (issue #358).
///
/// **Always a pause, `--to` or not.** One `--device` serves both steps, so
/// the source is still in the drive when step 1 ends. With `--to` the
/// operator only confirms the swap (Enter); without it the one prompt is
/// both the swap and the label, as it always was (issue #146). An end of
/// input (Ctrl-D) is not a confirmed swap: nothing is written, and the
/// message says how to finish from the slices step 1 staged.
///
/// Never reads a non-terminal (ADR-0008's non-hanging rule; issue #146's
/// crux). `run` refuses that case before step 1 via
/// [`refuse_unattended_compact`], so this arm is the belt to its braces.
fn swap_to_destination(to: Option<&str>, source: &str, device: &str) -> Result<String> {
    swap_to_destination_with(
        to,
        source,
        device,
        crate::cli::consent::stdin_is_terminal(),
        || {
            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;
            Ok(input)
        },
    )
}

/// [`swap_to_destination`] with the TTY answer and the read injected, so the
/// pause is provably taken and the non-interactive branch provably never
/// reads — the same testing discipline as `cli::consent::confirm_with`.
fn swap_to_destination_with(
    to: Option<&str>,
    source: &str,
    device: &str,
    is_tty: bool,
    read_answer: impl FnOnce() -> Result<String>,
) -> Result<String> {
    let finish_later = |dest: &str| {
        format!(
            "the live slices of \"{source}\" are staged; to finish, load the destination and \
             run `tapectl volume compact-write --destination {dest}`, then \
             `tapectl volume compact-finish {source}`"
        )
    };
    if !is_tty {
        return Err(TapectlError::Other(format!(
            "volume compact stopped before step 2: stdin is not a terminal, so nobody can swap \
             cartridges — {}",
            finish_later(to.unwrap_or("<DEST>"))
        )));
    }
    match to {
        Some(dest) => {
            println!(
                "\nUnload \"{source}\" from {device} and load the destination \"{dest}\", then \
                 press Enter to write it (Ctrl-D stops here):"
            );
            let answer = read_answer()?;
            if answer.is_empty() {
                return Err(TapectlError::Other(format!(
                    "volume compact stopped before step 2: no answer at the swap prompt — {}",
                    finish_later(dest)
                )));
            }
            Ok(dest.to_string())
        }
        None => {
            println!(
                "\nUnload \"{source}\" from {device}, load the destination tape, and enter its \
                 volume label:"
            );
            let input = read_answer()?;
            let label = input.trim();
            if label.is_empty() {
                return Err(TapectlError::Other(format!(
                    "no destination label provided — {}",
                    finish_later("<DEST>")
                )));
            }
            Ok(label.to_string())
        }
    }
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

/// How an ERROR from `volume <command>` — anything `run` returns as `Err` —
/// maps to an exit code (issue #356, CTO ruling 2026-09-28).
///
/// `volume verify`'s exit status is a three-way contract (see
/// [`verify_exit_code`]) in which 2 means "the medium is proven bad and the
/// volume is quarantined". An error is never that: every one a verify stops
/// on — an unknown label, an empty drive, the wrong tape, a drive that
/// cannot read this generation, a front index that will not read, a store
/// error, `--dry-run` — happened before a verdict or instead of one, and
/// none of them touches the volume's condition. So a verify's errors exit
/// [`crate::error::EXIT_VERIFY_INCONCLUSIVE`], and 2 stays reachable only
/// through a quarantine. Every other volume subcommand keeps the ordinary
/// error code.
///
/// Decided from the parsed command, before it runs, so that `main` maps
/// every error the invocation can return — including the ones raised
/// before this module is reached (the database, the config) — and not only
/// those from the verify itself. A verify command line that does not parse
/// never gets this far; `main`'s `parse_error_exit_code` gives it 3 too.
pub fn error_contract(command: &VolumeCommands) -> crate::error::ErrorContract {
    match command {
        VolumeCommands::Verify { .. } => crate::error::ErrorContract::Verify,
        // ADR-0012, 2026-10-07 item 20 (issue #408): the write family's
        // one exit table. `compact` writes in its step 2.
        VolumeCommands::Write { .. }
        | VolumeCommands::Resume { .. }
        | VolumeCommands::CompactWrite { .. }
        | VolumeCommands::Compact { .. } => crate::error::ErrorContract::Write,
        _ => crate::error::ErrorContract::Ordinary,
    }
}

/// Decide the process exit code for `volume verify` from its report —
/// issue #45/H10 made a failing verify non-zero; issue #356 (CTO ruling,
/// 2026-09-28) split the failure in two, because its two outcomes have
/// opposite remedies (ADR-0012, the 2026-09-17 amendment):
///
/// - [`crate::error::EXIT_SUCCESS`] (0): every checked file matched.
/// - [`crate::error::EXIT_VERIFY_MEDIUM_BAD`] (2): at least one mismatch
///   PROVES THE MEDIUM BAD, so the volume's condition is — or, re-verified,
///   still is — `quarantined` and it no longer counts as a copy. Remedy:
///   another cartridge.
/// - [`crate::error::EXIT_VERIFY_INCONCLUSIVE`] (3): mismatches, none of
///   them medium evidence — a read or transport failure, "we could not read
///   it today", which is not "the bytes are gone". The volume is untouched.
///   Remedy: the drive, then verify again. Every ERROR a verify returns
///   exits 3 as well ([`error_contract`]).
///
/// Keyed on `report.quarantine`, the same field that drives the printed
/// summary and `--json`'s `quarantined`, so the exit code, the message and
/// the JSON can never disagree about which outcome this was. Until #356
/// both failures exited 2 and only `--json` told them apart; issue #234 had
/// declined a third code as "a CLI contract nobody asked for", and the
/// systemd timers' health ping and `first-run.sh` step 13 turned out to be
/// the ones asking.
fn verify_exit_code(report: &write::VerifyReport) -> i32 {
    if report.quarantine.is_some() {
        crate::error::EXIT_VERIFY_MEDIUM_BAD
    } else if report.failed > 0 {
        crate::error::EXIT_VERIFY_INCONCLUSIVE
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
    /// `volumes.observed_condition` (ADR-0012's 2026-09-17 amendment, issue
    /// #242): "ok" or "quarantined". Since that amendment a quarantined
    /// volume can read `STATUS = "sealed"`, so this column carries the fact
    /// STATUS alone can no longer show.
    #[tabled(rename = "CONDITION")]
    condition: String,
    /// `volumes.media_type`: the generation `volume init` decided the
    /// medium is (ADR-0010, issue #482) -- the same key `volume info --json`
    /// uses for the column. Nullable in the schema (`None` for a row older
    /// than the column), rendered "—" in the table, `null` in `--json`.
    #[tabled(rename = "GENERATION", display_with = "display_generation")]
    media_type: Option<String>,
    /// `volumes.capacity_bytes`: the capacity `volume init` decided, once
    /// (ADR-0010 decision 3) -- raw bytes in `--json`, DECIMAL in the table
    /// (ADR-0012: cartridge capacities are decimal, data sizes binary).
    #[tabled(rename = "CAPACITY", display_with = "display_capacity")]
    capacity_bytes: i64,
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
    /// This volume's own most recent PASSED FULL `verification_sessions`
    /// row (raw timestamp; `None` = never verified). A quick confirm or
    /// quick verify reads none of the volume's bytes and never counts
    /// (ADR-0012 2026-10-06 items 1 and 24); `volume info` lists every
    /// session with its type. Deliberately a fresh
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

/// `(v2)` or `(v1, v2)`: the versions a `read-slices` run read (issue #474).
fn versions_phrase(versions: &[i64]) -> String {
    let named: Vec<String> = versions.iter().map(|v| format!("v{v}")).collect();
    format!("({})", named.join(", "))
}

fn display_generation(v: &Option<String>) -> String {
    v.clone().unwrap_or_else(|| "—".to_string())
}

fn display_capacity(bytes: &i64) -> String {
    crate::util::format_bytes_decimal(*bytes)
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
        "SELECT v.label, v.status, v.observed_condition, c.barcode, l.name,
                (SELECT MIN(per.copies) FROM (
                    SELECT DISTINCT cu.id, ({copy_expr}) AS copies
                    FROM writes cw2
                    JOIN stage_sets css2 ON css2.id = cw2.stage_set_id
                    JOIN snapshots cs2 ON cs2.id = css2.snapshot_id
                    JOIN units cu ON cu.id = cs2.unit_id
                    WHERE cw2.volume_id = v.id AND cw2.status = 'completed'
                 ) per) AS copies,
                (SELECT MAX({dated}) FROM verification_sessions vs
                  WHERE vs.volume_id = v.id AND vs.outcome = 'passed'
                    AND vs.verify_type = 'full') AS last_verified,
                v.media_type, v.capacity_bytes
         FROM volumes v
         LEFT JOIN cartridge_volumes cv ON cv.volume_id = v.id
         LEFT JOIN cartridges c ON c.id = cv.cartridge_id
         LEFT JOIN locations l ON l.id = v.location_id",
        dated = crate::policy::evidence::full_readback_date("vs"),
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
                condition: row.get(2)?,
                cartridge: row.get(3)?,
                location: row.get(4)?,
                copies: row.get(5)?,
                verified: row.get(6)?,
                media_type: row.get(7)?,
                capacity_bytes: row.get(8)?,
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

/// One `writes` row: a record that a specific stage set was written to
/// this volume, regardless of outcome — `volume info` shows the write
/// history, not just the successes. Not a "receipt": that word means the
/// recipient list a stage set was encrypted to (CONTEXT.md, issue #361).
#[derive(Debug, Clone, Serialize)]
struct WriteRow {
    unit: String,
    version: i64,
    status: String,
    started_at: Option<String>,
    completed_at: Option<String>,
    num_slices: Option<i64>,
    bytes: Option<i64>,
}

/// One `verification_sessions` row. Every outcome is shown here (unlike
/// `VolumeRow::verified`, which is deliberately the latest PASSED full
/// session only) — a dossier's verification history is exactly the place a failed
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
    /// `volumes.observed_condition` (ADR-0012's 2026-09-17 amendment, issue
    /// #242): "ok" or "quarantined" — see `VolumeRow::condition`'s doc for
    /// why `status` alone can no longer show this fact.
    condition: String,
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
    /// When this volume's own seal was written (`volumes.sealed_at`,
    /// migration 018): set once by the write session's `seal()` and never
    /// cleared, so it is set on a volume whose confirm did not finish, or
    /// whose session was then aborted — the fact `volume resume`'s
    /// re-confirmation of an aborted session turns on (ADR-0012
    /// 2026-09-23), and that `scripts/first-run.sh` step 13 reads.
    sealed_at: Option<String>,
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
    writes: Vec<WriteRow>,
    verifications: Vec<VerificationRow>,
    deposits: Vec<DepositRow>,
    /// Each recorded write's feed ratio, newest first (issue #439): native
    /// tape bytes per data byte sent, with the verdict it was judged by.
    /// Empty when the drive gave no reading, and an older write has none.
    feed_ratios: Vec<crate::cli::report::FeedRatioRow>,
    /// The phases of the most recent write, resume, verify or read session
    /// recorded against this volume (issue #386, migration 028). Human
    /// output only: `--json`'s shape is unchanged.
    #[serde(skip)]
    phase_timings: Vec<crate::db::phase_timings::PhaseRow>,
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
        condition,
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
            // `first_write` is when the first write BEGAN (issue #392),
            // read from the writes themselves: a volume written before that
            // fix stored its first write's completion there instead.
            "SELECT v.id, v.status, v.observed_condition, v.backend_type, v.backend_name,
                    v.media_type, v.capacity_bytes, v.bytes_written, v.created_at,
                    COALESCE((SELECT MIN(w.started_at) FROM writes w WHERE w.volume_id = v.id),
                             v.first_write),
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
                    row.get(14)?,
                ))
            },
        )
        .map_err(|_| TapectlError::VolumeNotFound(label.to_string()))?;

    let sealed_at: Option<String> = conn.query_row(
        "SELECT sealed_at FROM volumes WHERE id = ?1",
        rusqlite::params![vol_id],
        |r| r.get(0),
    )?;

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

    // Writes: every write of this volume, whatever its outcome —
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
    let writes: Vec<WriteRow> = writes_stmt
        .query_map(rusqlite::params![vol_id], |row| {
            Ok(WriteRow {
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
    // `VolumeRow::verified`, which is the latest PASSED full one only).
    // Newest recorded first, by id: a continued full readback is dated from
    // its oldest read (ADR-0012 2026-10-07 item 1), which can be older than
    // the interrupted sessions it continued, so `started_at` would sort the
    // one that concluded below them.
    let mut verify_stmt = conn.prepare(
        "SELECT started_at, completed_at, verify_type, outcome,
                slices_checked, slices_passed, slices_failed
         FROM verification_sessions
         WHERE volume_id = ?1
         ORDER BY id DESC",
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
        condition,
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
        sealed_at,
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
        feed_ratios: crate::cli::report::feed_ratio_rows(conn, Some(label))?,
        phase_timings: crate::db::phase_timings::latest_for_volume(conn, vol_id)?,
    })
}

/// `volume info`'s write-history section, one line per `writes` row under
/// a "Writes:" heading — "Write receipts:" until issue #361 ("receipt"
/// means the recipient list a stage set was encrypted to). Rendered to a
/// string so both the empty and the populated heading are testable.
fn render_write_history(writes: &[WriteRow]) -> String {
    if writes.is_empty() {
        return "Writes: none\n".to_string();
    }
    let mut out = String::from("Writes:\n");
    for w in writes {
        out.push_str(&format!(
            "    {} v{}: {} ({})\n",
            w.unit,
            w.version,
            w.status,
            w.completed_at.as_deref().unwrap_or("not completed"),
        ));
    }
    out
}

/// `volume verify`'s line about the write figures it recorded (ADR-0012
/// 2026-10-07 item 33, issue #423), or withheld.
fn figures_message(label: &str, figures: &crate::volume::figures::Figures) -> String {
    use crate::volume::figures::Figures;
    match figures {
        Figures::Recorded(changes) => format!(
            "volume \"{label}\": recorded the write figures it was missing or had wrong: {}",
            changes.join("; ")
        ),
        Figures::Withheld(why) => format!(
            "volume \"{label}\": its write figures are missing or wrong, and were NOT recorded: \
             {why}. The verify itself passed."
        ),
    }
}

/// `volume verify --json`'s `figures` key: `{"recorded": [...]}`,
/// `{"withheld": "<why>"}`, or `null`.
fn figures_json(figures: Option<&crate::volume::figures::Figures>) -> serde_json::Value {
    use crate::volume::figures::Figures;
    match figures {
        None => serde_json::Value::Null,
        Some(Figures::Recorded(changes)) => serde_json::json!({ "recorded": changes }),
        Some(Figures::Withheld(why)) => serde_json::json!({ "withheld": why }),
    }
}

/// `volume info`'s two write-date lines (issue #392): when the first write
/// began and when the last one completed, each line saying which.
fn render_write_span(first_write: Option<&str>, last_write: Option<&str>) -> String {
    let first = first_write
        .map(|t| format!("{t} (started)"))
        .unwrap_or_else(|| "(never written)".to_string());
    let last = last_write
        .map(|t| format!("{t} (completed)"))
        .unwrap_or_else(|| "(never written)".to_string());
    format!("  First write: {first}\n  Last write:  {last}\n")
}

/// `volume info`'s seal line (issue #305): `volumes.sealed_at`, which `--json`
/// carried and the human output never printed — on a rebuilt volume it is
/// the seal marker's own date, the one thing the tape says about when its
/// write finished. A volume that recorded none says so.
fn render_sealed_line(sealed_at: Option<&str>) -> String {
    format!(
        "  Sealed:      {}",
        sealed_at.unwrap_or("(no seal recorded)")
    )
}

/// `volume info`'s verification-history section, one line per session.
/// Both ends of each verify are printed and named (issue #392): this showed
/// the start alone while `report verify-status` showed the completion, so
/// one verify read as two different times.
fn render_verification_history(verifications: &[VerificationRow]) -> String {
    if verifications.is_empty() {
        return "Verification history: never verified\n".to_string();
    }
    let mut out = String::from("Verification history:\n");
    for v in verifications {
        out.push_str(&format!(
            "    [{}] {}: started {}, completed {} ({}/{} slices passed)\n",
            v.verify_type,
            v.outcome,
            v.started_at,
            v.completed_at.as_deref().unwrap_or("(not completed)"),
            v.slices_passed,
            v.slices_checked,
        ));
    }
    out
}

fn print_volume_info(info: &VolumeInfo) {
    println!("Volume: {}", info.label);
    println!("  Status:      {}", info.status);
    // Issue #242: since a quarantined volume can read Status "sealed", the
    // condition is a separate, always-shown line rather than folded into
    // Status's text.
    println!("  Condition:   {}", info.condition);
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
    print!(
        "{}",
        render_write_span(info.first_write.as_deref(), info.last_write.as_deref())
    );
    println!("{}", render_sealed_line(info.sealed_at.as_deref()));
    print!(
        "{}",
        crate::cli::report::render_feed_ratio_lines(&info.feed_ratios, info.last_write.is_some())
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
    print!("{}", render_write_history(&info.writes));

    println!();
    print!("{}", render_verification_history(&info.verifications));

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

    // Issue #386: where the last session's time went.
    if !info.phase_timings.is_empty() {
        println!();
        print!("{}", crate::db::phase_timings::render(&info.phase_timings));
    }
}

/// One `volume plan` row: a staged stage set, and — under `--policy-aware`
/// — what its unit falls short of, when it does.
#[derive(Debug)]
struct PlanRow {
    unit: String,
    version: i64,
    slices: Option<i64>,
    size: Option<i64>,
    finding: Option<String>,
}

/// `volume plan`'s rows: every `'staged'` stage set, largest first. With
/// `policy_aware` (ADR-0012 2026-10-07 item 26, issue #144), the rows whose
/// unit falls short of its policy on the audit's own terms
/// (`coverage::Shortfall`: too few copies, a required location with no
/// copy) come first, each carrying the shortfall, and the rest after, the
/// size order kept within each group; a unit whose policy cannot be
/// resolved is in the first group, the error its finding. It orders a
/// listing: `volume write` selects every staged set regardless
/// (`find_staged_data`), so here there is no selection for the flag to
/// change.
/// `volume plan`'s line about the cross-volume index (#418 R4): the index is
/// in every envelope, and every envelope is on the tape twice (§1.3), so about
/// twice its plaintext size, counted by the capacity gate with every other
/// file. Said to be an estimate, with why (see [`planned_index_bytes`]).
fn plan_index_line(index: i64) -> String {
    format!(
        "cross-volume index: about {} per tape, {} on it with the envelope backups \
         (an estimate: all of the above on one tape, as the catalog stands now)",
        crate::util::format_bytes_binary(index),
        crate::util::format_bytes_binary(2 * index),
    )
}

/// The size, in bytes, of the cross-volume index (`volume-format-v2.md`
/// §1.5) that a write of everything now staged would put in each copy of the
/// envelopes, before tar and age overhead: the real generator, run into a sink
/// ([`crate::volume::index::measure`]), so every listed version's paths are
/// read once but nothing is written to disk.
///
/// **An estimate**, three ways: there is no volume yet, so this tape's row is
/// written with a typical label, uuid and serial; every staged set is counted
/// as if it were on one tape (`volume write` would put them on as many as the
/// plan says); and the index is as of this moment, growing with the archive.
fn planned_index_bytes(
    conn: &Connection,
    config: &Config,
    generation: Option<&str>,
    device: Option<&str>,
) -> Result<i64> {
    let stage_sets: Vec<i64> = conn
        .prepare("SELECT id FROM stage_sets WHERE status = 'staged' ORDER BY id")?
        .query_map([], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    // The medium it would be written to names the generation File 0 says;
    // fall back to a typical figure when no drive is configured to ask.
    let generation = generation
        .map(str::to_string)
        .or_else(|| {
            crate::config::resolve_lto_backend(config, device)
                .ok()
                .map(|b| b.generation.clone())
        })
        .unwrap_or_else(|| "LTO-6".to_string());
    let measured = crate::volume::index::measure(
        conn,
        &crate::policy::coverage::IndexThisTape {
            label: "L6-0000",
            uuid: "00000000-0000-0000-0000-000000000000",
            cartridge_serial: "00000000",
            generation: &generation,
        },
        &stage_sets,
    )?;
    Ok(measured.total_bytes() as i64)
}

fn staged_plan_rows(
    conn: &Connection,
    config: &Config,
    policy_aware: bool,
) -> Result<Vec<PlanRow>> {
    let mut stmt = conn.prepare(
        "SELECT u.name, s.version, ss.num_slices, ss.total_encrypted_size
         FROM stage_sets ss
         JOIN snapshots s ON s.id = ss.snapshot_id
         JOIN units u ON u.id = s.unit_id
         WHERE ss.status = 'staged'
         ORDER BY ss.total_encrypted_size DESC",
    )?;
    let mut rows: Vec<PlanRow> = stmt
        .query_map([], |row| {
            Ok(PlanRow {
                unit: row.get(0)?,
                version: row.get(1)?,
                slices: row.get(2)?,
                size: row.get(3)?,
                finding: None,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if policy_aware {
        for row in &mut rows {
            if let Some(unit) = crate::db::queries::get_unit_by_name(conn, &row.unit)? {
                // One unit's unresolvable policy (a bad `[policy]` value,
                // say) is that row's finding, as `audit` reports it — not
                // an abort of the whole listing.
                match crate::policy::coverage::unit_shortfall(conn, config, &unit) {
                    Ok(short) => {
                        if short.any() {
                            row.finding = Some(short.describe());
                        }
                    }
                    Err(e @ crate::error::TapectlError::PolicyUnresolvable { .. }) => {
                        row.finding = Some(format!("policy unresolvable: {e}"));
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        // Stable: the size order holds within each group.
        rows.sort_by_key(|r| r.finding.is_none());
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #418 R4: `volume plan` names the index's size, twice it for the
    /// envelope backups, and calls it an estimate.
    #[test]
    fn the_plan_line_for_the_index_doubles_it_for_the_backups_and_says_estimate() {
        let line = plan_index_line(3 * 1024 * 1024);
        assert!(
            line.starts_with("cross-volume index: about 3.0 MiB per tape"),
            "{line}"
        );
        assert!(
            line.contains("6.0 MiB on it with the envelope backups"),
            "{line}"
        );
        assert!(line.contains("an estimate"), "{line}");
    }

    /// Issue #440: what `volume write` and `volume resume` do about staging
    /// once the copy is sealed and confirmed. The write itself needs a tape
    /// drive, so these drive the release step on a catalog that looks as it
    /// does right after one (the seam `collection::batch::release_if_covered`
    /// uses for the same reason), and pin the wiring around it by source.
    mod staging_release_after_write {
        use super::*;
        use crate::staging::release::fixtures::*;
        use crate::staging::release::CopyProgress;

        /// `alpha` v1, `copies` completed writes, staged on disk; the first
        /// copy's volume label is the one "just written".
        fn written(copies: i64) -> (Connection, tempfile::TempDir, Staged) {
            let conn = crate::db::open_memory().unwrap();
            let dir = tempfile::tempdir().unwrap();
            let alpha = unit(&conn, tenant(&conn), "testlib/alpha");
            let s = staged_version(&conn, alpha, 1, "current", dir.path(), copies);
            (conn, dir, s)
        }

        /// Released at the floor: the write that brought the unit to its
        /// second copy frees the staging, and says how much.
        #[test]
        fn released_when_the_write_brings_every_unit_to_min_copies() {
            let (conn, dir, s) = written(2);
            let config = config(dir.path(), 2);

            let release = release_staging_after_write(&conn, &config, &s.volumes[1], false);

            match &release {
                StagingRelease::Released(r) => {
                    assert_eq!(r.sets_cleaned, 1);
                    assert!(r.bytes_freed > 0);
                }
                other => panic!("expected a release at the floor: {other:?}"),
            }
            assert!(!s.file.exists());
            assert_eq!(stage_set_status(&conn, s.stage_set_id), "cleaned");
            let json = staging_release_json(serde_json::json!({"label": "x"}), &release);
            assert_eq!(json["staging_released"], true);
            assert_eq!(json["stage_sets_released"], 1);
            assert_eq!(json["under_copied"], serde_json::json!([]));
            assert_eq!(json["keep_staging"], false);
            assert!(json["release_warning"].is_null());
        }

        /// Kept below it, naming the unit and the shortfall in the words the
        /// operator reads; the same facts are in `--json`.
        #[test]
        fn kept_below_min_copies_with_the_reason() {
            let (conn, dir, s) = written(1);
            let config = config(dir.path(), 2);

            let release = release_staging_after_write(&conn, &config, &s.volumes[0], false);

            let StagingRelease::Retained(units) = &release else {
                panic!("one copy of two must keep the staging: {release:?}");
            };
            assert_eq!(
                units.iter().map(CopyProgress::describe).collect::<Vec<_>>(),
                ["unit testlib/alpha has 1 copy, needs 2"]
            );
            assert!(s.file.exists(), "the next copy reuses these bytes");
            assert_eq!(stage_set_status(&conn, s.stage_set_id), "staged");
            let json = staging_release_json(serde_json::json!({"label": "x"}), &release);
            assert_eq!(json["staging_released"], false);
            assert_eq!(
                json["under_copied"],
                serde_json::json!([{"unit": "testlib/alpha", "copies": 1, "min_copies": 2}])
            );
        }

        /// `--keep-staging`: covered, and kept anyway. The positive control
        /// is the same catalog released without the flag.
        #[test]
        fn keep_staging_opts_out_even_when_every_unit_is_covered() {
            let (conn, dir, s) = written(2);
            let config = config(dir.path(), 2);

            let release = release_staging_after_write(&conn, &config, &s.volumes[1], true);

            assert!(matches!(release, StagingRelease::Kept), "{release:?}");
            assert!(s.file.exists(), "--keep-staging releases nothing");
            assert_eq!(stage_set_status(&conn, s.stage_set_id), "staged");
            let json = staging_release_json(serde_json::json!({"label": "x"}), &release);
            assert_eq!(json["keep_staging"], true);
            assert_eq!(json["staging_released"], false);

            // Positive control: without the flag this catalog DOES release.
            let again = release_staging_after_write(&conn, &config, &s.volumes[1], false);
            assert!(matches!(again, StagingRelease::Released(_)), "{again:?}");
            assert!(!s.file.exists());
        }

        /// A release that errors after the copy stands is a warning: the
        /// step and the command that finishes it are named, nothing is
        /// released, and nothing here can change the write's exit status.
        #[test]
        fn a_failed_release_is_a_warning_naming_staging_clean() {
            let (conn, dir, s) = written(2);
            let config = config(dir.path(), 2);
            // The catalog fails the release's own reads, after the gate.
            conn.execute_batch("DROP TABLE stage_slices").unwrap();

            let release = release_staging_after_write(&conn, &config, &s.volumes[1], false);

            let StagingRelease::Failed(warning) = &release else {
                panic!("a catalog error must be reported, not raised: {release:?}");
            };
            assert!(
                warning.starts_with(
                    "warning: volume \"V-u1-v1-1\" is sealed and confirmed and counts as a \
                     copy, but releasing staging did not finish: "
                ),
                "{warning}"
            );
            assert!(
                warning.ends_with("To finish it: tapectl staging clean"),
                "{warning}"
            );
            let json = staging_release_json(serde_json::json!({"label": "x"}), &release);
            assert_eq!(json["staging_released"], false);
            assert_eq!(json["release_warning"], serde_json::json!(warning));
        }

        /// A failed or unconfirmed write never reaches the release: the
        /// command returns before it. Driven for real where the write can be
        /// refused without a drive (a sealed volume is no write target), with
        /// a catalog whose staging WOULD release — the control follows.
        #[test]
        fn a_write_that_does_not_complete_releases_nothing() {
            let (conn, dir, s) = written(2);
            let config = {
                let mut c = config(dir.path(), 2);
                c.backends.lto.push(crate::config::LtoBackendConfig {
                    name: "lto0".into(),
                    device_tape: "/nonexistent/tapectl-release-nst".into(),
                    device_sg: "/nonexistent/tapectl-release-sg".into(),
                    generation: "LTO-6".into(),
                    capacity_override: None,
                    fill_ceiling: 1.0,
                    enospc_buffer: "0".into(),
                });
                c
            };
            let tmp = tempfile::TempDir::new().unwrap();
            let paths = TapectlPaths::new(tmp.path().join("home"));
            let cmd = VolumeCommands::Write {
                label: Some(s.volumes[1].clone()),
                loaded: false,
                device: Some("/nonexistent/tapectl-release-nst".into()),
                force: false,
                init: false,
                move_to: None,
                allow_missing_escrow: false,
                prewrite_hash: false,
                fill_ceiling: None,
                full_confirm: false,
                keep_staging: false,
            };

            let err = run(&conn, &paths, &config, &cmd, false, true, false)
                .expect_err("a sealed volume is not a write target");
            assert!(
                matches!(err, TapectlError::VolumeNotWriteTarget { .. }),
                "the write itself was reached and refused: {err:?}"
            );
            assert!(s.file.exists(), "a refused write released nothing");
            assert_eq!(stage_set_status(&conn, s.stage_set_id), "staged");

            // Control: the catalog is releasable, so the file survived
            // because the write failed, not because nothing could be freed.
            let release = release_staging_after_write(&conn, &config, &s.volumes[1], false);
            assert!(
                matches!(release, StagingRelease::Released(_)),
                "{release:?}"
            );
        }

        /// `--dry-run` refuses `volume write` and `volume resume` outright
        /// (issue #241), before any release can be considered.
        #[test]
        fn dry_run_releases_nothing() {
            let (conn, dir, s) = written(2);
            let config = config(dir.path(), 2);
            let tmp = tempfile::TempDir::new().unwrap();
            let paths = TapectlPaths::new(tmp.path().join("home"));
            for cmd in [
                VolumeCommands::Write {
                    label: Some(s.volumes[1].clone()),
                    loaded: false,
                    device: None,
                    force: false,
                    init: false,
                    move_to: None,
                    allow_missing_escrow: false,
                    prewrite_hash: false,
                    fill_ceiling: None,
                    full_confirm: false,
                    keep_staging: false,
                },
                VolumeCommands::Resume {
                    label: Some(s.volumes[1].clone()),
                    loaded: false,
                    device: None,
                    prewrite_hash: false,
                    full_confirm: false,
                    keep_staging: false,
                },
            ] {
                let err = run(&conn, &paths, &config, &cmd, false, true, true)
                    .expect_err("--dry-run is refused");
                assert!(
                    err.to_string().contains("--dry-run is not supported"),
                    "{err}"
                );
            }
            assert!(s.file.exists());
            assert_eq!(stage_set_status(&conn, s.stage_set_id), "staged");
        }

        /// The wiring, by source (the house pattern for what cannot be
        /// driven without a drive): in both arms the release is the statement
        /// AFTER the write's own `?` — every failed, aborted, interrupted or
        /// inconclusive outcome returns at that `?` — and `--keep-staging`
        /// reaches it. For `volume write` that sequence lives in
        /// `write_and_follow` (issue #438): the init, then the write, and
        /// only behind the write's `?` the release and the `--move-to` move.
        #[test]
        fn both_arms_release_only_after_the_write_returns_ok() {
            const SRC: &str = include_str!("volume.rs");
            let run_body = &SRC[SRC.find("pub fn run(").unwrap()..];

            // `volume write`: the arm hands the whole sequence to
            // `write_and_follow`, which is where the order is pinned.
            let start = run_body.find("VolumeCommands::Write {\n").unwrap();
            let arm = &run_body[start
                ..start
                    + run_body[start..]
                        .find("VolumeCommands::Resume {\n")
                        .unwrap()];
            assert_eq!(
                arm.matches("write_and_follow(").count(),
                1,
                "the Write arm runs its steps through write_and_follow"
            );
            assert!(
                !arm.contains("release_staging_after_write(")
                    && !arm.contains(" move_after_write("),
                "the arm itself releases and moves nothing: write_and_follow does, after the write"
            );
            let seq_start = SRC.find("fn write_and_follow(").unwrap();
            let seq = &SRC[seq_start..seq_start + SRC[seq_start..].find("\n}\n").unwrap()];
            let init = seq.find("steps.init(").expect("write_and_follow inits");
            let call = seq.find("steps.write(").expect("write_and_follow writes");
            // The write's result is held while a failure after `--init` is
            // reported (issue #445), then returned through its own `?`.
            let question = call + seq[call..].find("written?;").expect("the write's own `?`");
            let release = seq
                .find("release_staging_after_write(conn, config, req.label, req.keep_staging)")
                .expect("the release honours --keep-staging");
            let moved = seq
                .find("move_after_write(conn, req.label, req.move_to)")
                .expect("the move is requested");
            assert!(init < call, "the init comes before the write");
            let preflight = seq
                .find("steps.preflight(req.label)?")
                .expect("write_and_follow asks the write's catalog-only refusals");
            assert!(
                preflight < init,
                "the catalog-only pre-flight comes before the init (issue #445)"
            );
            assert!(
                release > question && moved > question,
                "the release and the move must come after the write's `?`, not before it"
            );
            assert!(
                release < moved,
                "the staging release runs before the move, whose warning is printed last"
            );
            assert_eq!(seq.matches("release_staging_after_write(").count(), 1);
            assert_eq!(seq.matches("move_after_write(").count(), 1);
            assert!(
                seq.contains("req.force && !req.init"),
                "--force goes to the init only: the write after it is never forced"
            );
            let drive = SRC.find("impl WriteSteps for DriveWriteSteps").unwrap();
            assert!(
                SRC[drive..drive + 1500].contains("write::volume_write(")
                    && SRC[drive..drive + 1500].contains("write::volume_init("),
                "DriveWriteSteps is the real init and the real write"
            );

            // `volume resume`.
            let arm = "VolumeCommands::Resume {\n";
            let start = run_body.find(arm).unwrap();
            let body = &run_body
                [start..start + run_body[start..].find("VolumeCommands::Abort {").unwrap()];
            let call = body
                .find("write::volume_resume(")
                .unwrap_or_else(|| panic!("{arm}: no write"));
            let question = call + body[call..].find(")?;").expect("the write's own `?`");
            let release = body
                .find("release_staging_after_write(conn, config, label, *keep_staging)")
                .unwrap_or_else(|| panic!("{arm}: no release honouring --keep-staging"));
            assert!(
                release > question,
                "{arm}: the release must come after the write's `?`, not before it"
            );
            assert_eq!(
                body.matches("release_staging_after_write(").count(),
                1,
                "{arm}: one release call"
            );
            // And nowhere else in the library: `collection run`, `quick-archive`
            // and `volume compact*` reach `volume_write` directly and have
            // their own release step, so a release inside it would double up.
            let write_rs = include_str!("../volume/write.rs");
            assert!(
                !write_rs.contains("staging::release"),
                "volume::write must not release staging itself"
            );
        }
    }

    /// Issue #438 (ADR-0012 2026-10-15 item 59): `volume write --init
    /// --full-confirm --move-to <location>`. The two library calls that
    /// touch the drive are scripted through [`WriteSteps`]; everything else
    /// is the arm's real logic on a real catalog.
    mod write_one_command {
        use super::*;
        use crate::error::{ErrorContract, WriteOutcome};
        use crate::staging::release::fixtures::*;
        use std::cell::RefCell;

        const NEW: &str = "V-NEW";

        #[derive(Clone, Copy, PartialEq)]
        enum Fail {
            Nothing,
            /// `volume init` refuses (a File 0 that is not blank, say).
            Init,
            /// The write is interrupted part-way: exit 4, not sealed.
            Write,
            /// The write refuses for a reason only the contact can raise
            /// (the capacity gate, here): before the tape moved, so exit 2.
            Contact,
            /// The staged version was released (a first copy's staging, item
            /// 63): the catalog alone knows the write has nothing to write,
            /// and the pre-flight is the REAL library function asking it.
            Unstaged,
        }

        /// The drive, scripted: `init` leaves the volume row `volume init`
        /// leaves; `write` leaves the catalog as a sealed, confirmed write
        /// does — a completed `writes` row and the volume `sealed`.
        struct FakeSteps<'a> {
            conn: &'a Connection,
            staged: &'a Staged,
            fail: Fail,
            log: RefCell<Vec<String>>,
            /// The cartridge: `init` writes File 0 to it, as the real one does.
            tape: RefCell<crate::store::MemStore>,
        }

        impl FakeSteps<'_> {
            fn log(&self) -> Vec<String> {
                self.log.borrow().clone()
            }
        }

        /// What the scripted init decided about the cartridge: an LTO-5 the
        /// chip's medium density code named, in the LTO-6 drive the real
        /// run of issue #482 used.
        fn fake_medium() -> crate::tape::media_detect::MediumDecision {
            use crate::tape::media_detect::{DetectSource, MediaSource, MediumDecision};
            MediumDecision {
                generation: crate::media::Generation::Lto5,
                generation_source: MediaSource::Detected(DetectSource::MamMedium),
                density_code: Some(0x58),
                capacity_bytes: 1_500_000_000_000,
                capacity_source: crate::media::CapacitySource::GenerationTable,
            }
        }

        /// Issue #482: `volume init` keeps the volume line it always printed
        /// and adds one line naming the medium; `--json` keeps its keys and
        /// adds the same facts as a `medium` object.
        #[test]
        fn init_prints_the_medium_line_after_the_volume_line_and_json_carries_it() {
            let init = Initialized {
                volume_id: 3,
                medium: fake_medium(),
            };
            assert_eq!(
                init_text("L5-0001", &init),
                "volume \"L5-0001\" initialized (id=3)\n\
                 medium: LTO-5, 1.50 TB (the cartridge chip's medium density code 0x58)"
            );
            let json = init_json("L5-0001", &init);
            assert_eq!(json["volume_id"], 3);
            assert_eq!(json["label"], "L5-0001");
            assert_eq!(json["status"], "initialized");
            assert_eq!(json["medium"]["media_type"], "LTO-5");
            assert_eq!(json["medium"]["capacity_bytes"], 1_500_000_000_000u64);
            assert_eq!(json["medium"]["generation_source"], "mam_medium_density");
            assert_eq!(json["medium"]["density_code"], 0x58);
            assert_eq!(json["medium"]["capacity_source"], "generation_table");
        }

        impl WriteSteps for FakeSteps<'_> {
            fn preflight(&self, label: &str) -> Result<()> {
                self.log.borrow_mut().push("preflight".to_string());
                if self.fail == Fail::Unstaged {
                    return crate::volume::write::write_catalog_preflight(self.conn, label, false);
                }
                Ok(())
            }

            fn init(&self, label: &str, force: bool) -> Result<Initialized> {
                self.log
                    .borrow_mut()
                    .push(format!("init {label} force={force}"));
                if self.fail == Fail::Init {
                    return Err(TapectlError::Other(
                        "volume init refused: the loaded cartridge's File 0 identifies a \
                         DIFFERENT volume"
                            .into(),
                    ));
                }
                self.conn
                    .execute(
                        "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes, status)
                         VALUES (?1, 'lto', 'p', 10485760, 'initialized')",
                        [label],
                    )
                    .unwrap();
                // File 0, as the real init writes it.
                self.tape.borrow_mut().files.push(b"File 0".to_vec());
                Ok(Initialized {
                    volume_id: self.conn.last_insert_rowid(),
                    medium: fake_medium(),
                })
            }

            fn write(&self, label: &str, force: bool) -> Result<()> {
                self.log
                    .borrow_mut()
                    .push(format!("write {label} force={force}"));
                if self.fail == Fail::Write {
                    return Err(TapectlError::WriteInterrupted("the tape stopped".into()));
                }
                if self.fail == Fail::Contact {
                    return Err(TapectlError::Other(
                        "volume write refused: the batch does not fit the cartridge's capacity"
                            .into(),
                    ));
                }
                add_write(self.conn, self.staged, label, "initialized", "completed");
                self.conn
                    .execute(
                        "UPDATE volumes SET status = 'sealed' WHERE label = ?1",
                        [label],
                    )
                    .unwrap();
                Ok(())
            }
        }

        fn exit_code(err: TapectlError) -> i32 {
            ErrorContract::Write.exit_code(&anyhow::Error::from(err))
        }

        fn add_shelf(conn: &Connection, name: &str) -> i64 {
            conn.execute(
                "INSERT INTO locations (name, kind) VALUES (?1, 'shelf')",
                [name],
            )
            .unwrap();
            conn.last_insert_rowid()
        }

        fn location_of(conn: &Connection, label: &str) -> Option<i64> {
            conn.query_row(
                "SELECT location_id FROM volumes WHERE label = ?1",
                [label],
                |r| r.get(0),
            )
            .unwrap()
        }

        fn request<'a>(init: bool, force: bool, move_to: Option<&'a str>) -> WriteRequest<'a> {
            WriteRequest {
                label: NEW,
                loaded: false,
                init,
                force,
                move_to,
                keep_staging: false,
                json_output: false,
            }
        }

        /// The whole one-command form: init, write, then the release and the
        /// move, in that order, each only behind the one before it.
        #[test]
        fn init_write_release_and_move_run_in_order_and_succeed() {
            let conn = crate::db::open_memory().unwrap();
            let dir = tempfile::tempdir().unwrap();
            let alpha = unit(&conn, tenant(&conn), "testlib/alpha");
            let s = staged_version(&conn, alpha, 1, "current", dir.path(), 1);
            let config = config(dir.path(), 2);
            let offsite = add_shelf(&conn, "offsite");
            let steps = FakeSteps {
                conn: &conn,
                staged: &s,
                fail: Fail::Nothing,
                log: RefCell::default(),
                tape: RefCell::new(crate::store::MemStore::new(4096)),
            };

            let done = write_and_follow(
                &conn,
                &config,
                &steps,
                &request(true, true, Some("offsite")),
            )
            .unwrap();

            // `--force` went to the init only.
            assert_eq!(
                steps.log(),
                [
                    "preflight",
                    "init V-NEW force=true",
                    "write V-NEW force=false"
                ],
                "the pre-flight comes first, and `--force` goes to the init only"
            );
            assert!(done.initialized.is_some());
            assert!(
                matches!(done.release, StagingRelease::Released(_)),
                "the write brought the unit to its second copy: {:?}",
                done.release
            );
            assert!(!s.file.exists());
            let MoveAfterWrite::Moved { to, .. } = &done.moved else {
                panic!("the sealed volume moves: {:?}", done.moved);
            };
            assert_eq!(to, "offsite");
            assert_eq!(location_of(&conn, NEW), Some(offsite));

            let json = write_done_json(NEW, &done);
            // Keys that always existed keep their names and meaning.
            assert_eq!(json["label"], NEW);
            assert_eq!(json["status"], "completed");
            assert_eq!(json["staging_released"], true);
            // The additive keys.
            assert_eq!(json["initialized"], true);
            // Issue #482: what the init decided about the medium, raw.
            assert_eq!(json["medium"], fake_medium().to_json());
            assert_eq!(json["medium"]["media_type"], "LTO-5");
            assert_eq!(json["moved_to"], "offsite");
            assert!(json["move_warning"].is_null());
            assert_eq!(json["volumes_moved"], serde_json::json!([NEW]));
        }

        /// No `--init`, no `--move-to`: the command is exactly what it was.
        #[test]
        fn without_the_new_flags_only_the_write_runs() {
            let conn = crate::db::open_memory().unwrap();
            let dir = tempfile::tempdir().unwrap();
            let alpha = unit(&conn, tenant(&conn), "testlib/alpha");
            let s = staged_version(&conn, alpha, 1, "current", dir.path(), 1);
            let config = config(dir.path(), 2);
            // The volume already exists, as `volume init` left it.
            conn.execute(
                "INSERT INTO volumes (label, backend_type, backend_name, capacity_bytes, status)
                 VALUES (?1, 'lto', 'p', 10485760, 'initialized')",
                [NEW],
            )
            .unwrap();
            let steps = FakeSteps {
                conn: &conn,
                staged: &s,
                fail: Fail::Nothing,
                log: RefCell::default(),
                tape: RefCell::new(crate::store::MemStore::new(4096)),
            };

            let done =
                write_and_follow(&conn, &config, &steps, &request(false, true, None)).unwrap();

            // Positive control for the force test below: with no `--init`,
            // `--force` still reaches the write. And no pre-flight: the
            // write itself runs those checks (issue #445).
            assert_eq!(steps.log(), ["write V-NEW force=true"]);
            assert!(done.initialized.is_none());
            assert!(matches!(done.moved, MoveAfterWrite::NotAsked));
            let json = write_done_json(NEW, &done);
            assert_eq!(json["initialized"], false);
            assert!(json["medium"].is_null(), "no --init, no decision: {json}");
            assert!(json["moved_to"].is_null() && json["move_warning"].is_null());
            assert_eq!(json["volumes_moved"], serde_json::json!([]));
        }

        /// An init that refuses: the write is never attempted, nothing is
        /// recorded, and the error exits by the write family's table — 2,
        /// nothing written.
        #[test]
        fn an_init_that_refuses_writes_nothing_and_exits_2() {
            let conn = crate::db::open_memory().unwrap();
            let dir = tempfile::tempdir().unwrap();
            let alpha = unit(&conn, tenant(&conn), "testlib/alpha");
            let s = staged_version(&conn, alpha, 1, "current", dir.path(), 1);
            let config = config(dir.path(), 2);
            add_shelf(&conn, "offsite");
            let steps = FakeSteps {
                conn: &conn,
                staged: &s,
                fail: Fail::Init,
                log: RefCell::default(),
                tape: RefCell::new(crate::store::MemStore::new(4096)),
            };

            let err = write_and_follow(
                &conn,
                &config,
                &steps,
                &request(true, false, Some("offsite")),
            )
            .unwrap_err();

            assert!(err.to_string().contains("DIFFERENT volume"), "{err}");
            assert_eq!(
                steps.log(),
                ["preflight", "init V-NEW force=false"],
                "the write never ran"
            );
            let rows: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM volumes WHERE label = ?1",
                    [NEW],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(rows, 0, "no volume row: nothing was initialised");
            assert!(s.file.exists(), "nothing was released");
            assert_eq!(exit_code(err), crate::error::EXIT_ERROR);
        }

        /// A write that does not seal and confirm: nothing moves and nothing
        /// is released, and the exit is the write's own (4 interrupted). The
        /// volume stays initialised, which the operator was told after the
        /// init. The control is the same catalog with the write succeeding.
        ///
        /// Issue #445 rewrote what this pins. It used to be the stranded
        /// `initialized` row of ANY refusal after the init, the catalog-only
        /// ones included; those are now refused by the pre-flight before the
        /// init (below). What is left, deliberately, is a failure only the
        /// contact can raise: the tape stopping part-way (here), or a refusal
        /// at the contact (the next test).
        #[test]
        fn a_write_that_fails_does_not_move_or_release() {
            let conn = crate::db::open_memory().unwrap();
            let dir = tempfile::tempdir().unwrap();
            let alpha = unit(&conn, tenant(&conn), "testlib/alpha");
            let s = staged_version(&conn, alpha, 1, "current", dir.path(), 1);
            let config = config(dir.path(), 2);
            add_shelf(&conn, "offsite");
            let steps = FakeSteps {
                conn: &conn,
                staged: &s,
                fail: Fail::Write,
                log: RefCell::default(),
                tape: RefCell::new(crate::store::MemStore::new(4096)),
            };

            let err = write_and_follow(
                &conn,
                &config,
                &steps,
                &request(true, false, Some("offsite")),
            )
            .unwrap_err();

            assert_eq!(
                steps.log(),
                [
                    "preflight",
                    "init V-NEW force=false",
                    "write V-NEW force=false"
                ]
            );
            assert_eq!(
                location_of(&conn, NEW),
                None,
                "an unsealed volume is not moved"
            );
            assert!(s.file.exists(), "nothing was released");
            assert_eq!(
                WriteOutcome::of(&anyhow::Error::from(err)),
                Some(WriteOutcome::Interrupted)
            );

            // Control: the same request with a write that seals DOES move.
            let conn = crate::db::open_memory().unwrap();
            let alpha = unit(&conn, tenant(&conn), "testlib/alpha");
            let s = staged_version(&conn, alpha, 1, "current", dir.path(), 1);
            let offsite = add_shelf(&conn, "offsite");
            let steps = FakeSteps {
                conn: &conn,
                staged: &s,
                fail: Fail::Nothing,
                log: RefCell::default(),
                tape: RefCell::new(crate::store::MemStore::new(4096)),
            };
            write_and_follow(
                &conn,
                &config,
                &steps,
                &request(true, false, Some("offsite")),
            )
            .unwrap();
            assert_eq!(location_of(&conn, NEW), Some(offsite));
        }

        /// Issue #445: `--init` with nothing staged — a first copy released
        /// its staging, item 63 — refuses before the cartridge is touched:
        /// the pre-flight is the real library function over a real catalog
        /// whose only staged version was cleaned, `init` and `write` are
        /// never called, the cartridge holds nothing, no volume row exists
        /// and the exit is 2 (nothing written). It used to rewrite File 0,
        /// bind the cartridge and strand an `initialized` row first.
        #[test]
        fn an_init_with_nothing_staged_refuses_before_the_cartridge_is_touched() {
            let conn = crate::db::open_memory().unwrap();
            let dir = tempfile::tempdir().unwrap();
            let alpha = unit(&conn, tenant(&conn), "testlib/alpha");
            let s = staged_version(&conn, alpha, 1, "current", dir.path(), 1);
            // The staging was released after the first copy.
            conn.execute("UPDATE stage_sets SET status = 'cleaned'", [])
                .unwrap();
            let config = config(dir.path(), 2);
            add_shelf(&conn, "offsite");
            let steps = FakeSteps {
                conn: &conn,
                staged: &s,
                fail: Fail::Unstaged,
                log: RefCell::default(),
                tape: RefCell::new(crate::store::MemStore::new(4096)),
            };

            let err = write_and_follow(
                &conn,
                &config,
                &steps,
                &request(true, false, Some("offsite")),
            )
            .unwrap_err();

            assert!(
                err.to_string().contains("no staged data to write"),
                "the write's own refusal, from the pre-flight: {err}"
            );
            assert_eq!(steps.log(), ["preflight"], "neither init nor write ran");
            assert!(
                steps.tape.borrow().files.is_empty(),
                "the cartridge was not touched: no File 0"
            );
            let rows: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM volumes WHERE label = ?1",
                    [NEW],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(rows, 0, "no volume row");
            assert_eq!(exit_code(err), crate::error::EXIT_ERROR, "nothing written");
        }

        /// Issue #445: a refusal only the contact can raise (the capacity
        /// gate here) comes after the init, so the volume IS initialised
        /// while the exit is 2, the code that elsewhere means "the medium
        /// untouched". The contract that tells a script is the pair this
        /// pins: `--json` carries `initialized: true` and the volume left,
        /// and the stderr line names it. The control is the pre-flight
        /// refusing the same request when the catalog alone can.
        #[test]
        fn a_refusal_only_the_contact_can_raise_leaves_the_volume_initialized_and_says_so() {
            let conn = crate::db::open_memory().unwrap();
            let dir = tempfile::tempdir().unwrap();
            let alpha = unit(&conn, tenant(&conn), "testlib/alpha");
            let s = staged_version(&conn, alpha, 1, "current", dir.path(), 1);
            let config = config(dir.path(), 2);
            add_shelf(&conn, "offsite");
            let steps = FakeSteps {
                conn: &conn,
                staged: &s,
                fail: Fail::Contact,
                log: RefCell::default(),
                tape: RefCell::new(crate::store::MemStore::new(4096)),
            };

            let err = write_and_follow(
                &conn,
                &config,
                &steps,
                &request(true, false, Some("offsite")),
            )
            .unwrap_err();

            assert_eq!(
                steps.log(),
                [
                    "preflight",
                    "init V-NEW force=false",
                    "write V-NEW force=false"
                ]
            );
            assert_eq!(steps.tape.borrow().files.len(), 1, "File 0 was written");
            let status: String = conn
                .query_row("SELECT status FROM volumes WHERE label = ?1", [NEW], |r| {
                    r.get(0)
                })
                .expect("the volume row exists");
            assert_eq!(status, "initialized", "left initialized");
            assert_eq!(location_of(&conn, NEW), None, "nothing moved");
            assert!(s.file.exists(), "nothing was released");

            // The code is the write's: 2, a refusal before the tape moved.
            let json = left_initialized_json(
                NEW,
                &Initialized {
                    volume_id: 7,
                    medium: fake_medium(),
                },
                &err,
            );
            assert_eq!(exit_code(err), crate::error::EXIT_ERROR);
            assert_eq!(json["initialized"], true);
            assert_eq!(json["volume_status"], "initialized");
            assert_eq!(json["label"], NEW);
            assert_eq!(json["volume_id"], 7);
            assert_eq!(
                json["medium"],
                fake_medium().to_json(),
                "the init stood, and so did what it decided about the medium"
            );
            assert_eq!(json["status"], "failed");
            assert!(
                json["error"]
                    .as_str()
                    .unwrap()
                    .contains("does not fit the cartridge"),
                "{json}"
            );
        }

        /// A move that fails once the copy is sealed and confirmed is a
        /// warning, not a failed write: the copy stands, the result is `Ok`
        /// (so the arm exits 0), and the warning carries the `volume move`
        /// that finishes it. `--json` says so too.
        #[test]
        fn a_failed_move_is_a_warning_naming_volume_move_and_the_write_stands() {
            let conn = crate::db::open_memory().unwrap();
            let dir = tempfile::tempdir().unwrap();
            let alpha = unit(&conn, tenant(&conn), "testlib/alpha");
            let s = staged_version(&conn, alpha, 1, "current", dir.path(), 1);
            let config = config(dir.path(), 2);
            // No location named "gone": the destination vanished after the
            // arm's up-front check, say, or the check was bypassed.
            let steps = FakeSteps {
                conn: &conn,
                staged: &s,
                fail: Fail::Nothing,
                log: RefCell::default(),
                tape: RefCell::new(crate::store::MemStore::new(4096)),
            };

            let done =
                write_and_follow(&conn, &config, &steps, &request(true, false, Some("gone")))
                    .expect("a move that fails after the seal does not fail the write");

            let MoveAfterWrite::Failed { to, warning } = &done.moved else {
                panic!("the move failed: {:?}", done.moved);
            };
            assert_eq!(to, "gone");
            assert!(
                warning.starts_with(
                    "warning: volume \"V-NEW\" is sealed and confirmed and counts as a copy, \
                     but moving the volume did not finish: "
                ),
                "{warning}"
            );
            assert!(
                warning.ends_with("To finish it: tapectl volume move V-NEW --to gone"),
                "{warning}"
            );
            assert_eq!(location_of(&conn, NEW), None);
            let sealed: String = conn
                .query_row("SELECT status FROM volumes WHERE label = ?1", [NEW], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(sealed, "sealed", "the copy stands");
            assert!(
                matches!(done.release, StagingRelease::Released(_)),
                "a failed move leaves the release as it was"
            );
            let json = write_done_json(NEW, &done);
            assert_eq!(json["status"], "completed");
            assert!(json["moved_to"].is_null());
            assert_eq!(json["move_warning"], serde_json::json!(warning));
        }

        /// The destination is checked before anything is written: a typo, a
        /// location never added, or a warehouse (no cartridge goes there).
        #[test]
        fn the_move_destination_is_checked_before_the_tape_is_touched() {
            let conn = crate::db::open_memory().unwrap();
            add_shelf(&conn, "offsite");
            conn.execute(
                "INSERT INTO locations (name, kind) VALUES ('glacier', 'warehouse')",
                [],
            )
            .unwrap();

            check_move_destination(&conn, "offsite").unwrap();
            check_move_destination(&conn, " offsite ").unwrap();

            let err = check_move_destination(&conn, "ofsite")
                .unwrap_err()
                .to_string();
            assert!(err.contains("\"ofsite\" not found"), "{err}");
            assert!(err.contains("known locations: glacier, offsite"), "{err}");
            assert!(err.contains("Nothing was written"), "{err}");

            let err = check_move_destination(&conn, "glacier")
                .unwrap_err()
                .to_string();
            assert!(err.contains("warehouse location"), "{err}");
            assert!(err.contains("Nothing was written"), "{err}");
        }

        /// The flag surface: `--init` and `--move-to` parse beside the write's
        /// own flags and default off.
        #[test]
        fn the_new_flags_parse_beside_the_writes_own() {
            use clap::Parser;
            let parse = |args: &[&str]| {
                let mut argv = vec!["tapectl", "volume", "write"];
                argv.extend_from_slice(args);
                match crate::cli::Cli::try_parse_from(argv).unwrap().command {
                    crate::cli::Commands::Volume { command } => command,
                    other => panic!("not a volume command: {other:?}"),
                }
            };
            let VolumeCommands::Write {
                label,
                init,
                move_to,
                full_confirm,
                force,
                ..
            } = parse(&[
                "L6-0003",
                "--init",
                "--full-confirm",
                "--move-to",
                "offsite",
                "--force",
            ])
            else {
                panic!("a write");
            };
            assert_eq!(label.as_deref(), Some("L6-0003"));
            assert!(init && full_confirm && force);
            assert_eq!(move_to.as_deref(), Some("offsite"));

            let VolumeCommands::Write { init, move_to, .. } = parse(&["L6-0003"]) else {
                panic!("a write");
            };
            assert!(!init && move_to.is_none(), "both default off");
        }

        fn drive_config(dir: &std::path::Path) -> Config {
            let mut c = config(dir, 2);
            c.backends.lto.push(crate::config::LtoBackendConfig {
                name: "lto0".into(),
                device_tape: "/nonexistent/tapectl-438-nst".into(),
                device_sg: "/nonexistent/tapectl-438-sg".into(),
                generation: "LTO-6".into(),
                capacity_override: None,
                fill_ceiling: 1.0,
                enospc_buffer: "0".into(),
            });
            c
        }

        fn write_cmd(label: &str, init: bool, move_to: Option<&str>) -> VolumeCommands {
            VolumeCommands::Write {
                label: Some(label.into()),
                loaded: false,
                device: Some("/nonexistent/tapectl-438-nst".into()),
                force: false,
                init,
                move_to: move_to.map(str::to_string),
                allow_missing_escrow: false,
                prewrite_hash: false,
                fill_ceiling: None,
                full_confirm: false,
                keep_staging: false,
            }
        }

        /// Through the real arm and the real `volume init`: an init that is
        /// refused (there is no cartridge or drive here) leaves the catalog
        /// untouched — no volume row, no write row — and the error is the
        /// init's, not the write's "volume not found", so the write was
        /// never attempted. Exit 2.
        #[test]
        fn the_real_arm_stops_at_a_refused_init_with_nothing_written() {
            let conn = crate::db::open_memory().unwrap();
            let dir = tempfile::tempdir().unwrap();
            let config = drive_config(dir.path());
            let tmp = tempfile::TempDir::new().unwrap();
            let paths = TapectlPaths::new(tmp.path().join("home"));
            add_shelf(&conn, "offsite");

            let err = run(
                &conn,
                &paths,
                &config,
                &write_cmd(NEW, true, Some("offsite")),
                false,
                true,
                false,
            )
            .expect_err("no cartridge: the init is refused");

            let text = err.to_string();
            assert!(
                !text.contains("volume not found"),
                "the init failed, not the write: {text}"
            );
            for table in ["volumes", "writes"] {
                let n: i64 = conn
                    .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                    .unwrap();
                assert_eq!(n, 0, "{table}: nothing was recorded");
            }
            assert_eq!(exit_code(err), crate::error::EXIT_ERROR);
        }

        /// A `--move-to` that names no location is refused first — before
        /// the device is resolved (this config has none), before the init.
        #[test]
        fn the_real_arm_refuses_an_unknown_move_to_before_anything_else() {
            let conn = crate::db::open_memory().unwrap();
            let tmp = tempfile::TempDir::new().unwrap();
            let paths = TapectlPaths::new(tmp.path().join("home"));
            let no_drive = Config::default();

            let err = run(
                &conn,
                &paths,
                &no_drive,
                &write_cmd(NEW, true, Some("nowhere")),
                false,
                true,
                false,
            )
            .expect_err("unknown location");

            let text = err.to_string();
            assert!(
                text.contains("--move-to location \"nowhere\" not found"),
                "{text}"
            );
            assert_eq!(exit_code(err), crate::error::EXIT_ERROR);
        }

        /// Through the real arm: a write that is refused (a sealed volume is
        /// no write target) moves nothing, `--move-to` or not.
        #[test]
        fn the_real_arm_moves_nothing_when_the_write_is_refused() {
            let conn = crate::db::open_memory().unwrap();
            let dir = tempfile::tempdir().unwrap();
            let alpha = unit(&conn, tenant(&conn), "testlib/alpha");
            let s = staged_version(&conn, alpha, 1, "current", dir.path(), 2);
            let config = drive_config(dir.path());
            let tmp = tempfile::TempDir::new().unwrap();
            let paths = TapectlPaths::new(tmp.path().join("home"));
            add_shelf(&conn, "offsite");

            let err = run(
                &conn,
                &paths,
                &config,
                &write_cmd(&s.volumes[1], false, Some("offsite")),
                false,
                true,
                false,
            )
            .expect_err("a sealed volume is not a write target");

            assert!(
                matches!(err, TapectlError::VolumeNotWriteTarget { .. }),
                "{err:?}"
            );
            assert_eq!(location_of(&conn, &s.volumes[1]), None);
        }
    }

    /// Issue #435: the loaded tape as a volume reference — the flag surface
    /// of `volume verify`, `write` and `resume`, `volume identify`'s note on
    /// what the catalog makes of the tape, and the `--loaded` arm's first act.
    mod loaded_tape {
        use super::*;
        use clap::Parser;

        fn try_parse(args: &[&str]) -> std::result::Result<VolumeCommands, clap::Error> {
            let mut argv = vec!["tapectl", "volume"];
            argv.extend_from_slice(args);
            crate::cli::Cli::try_parse_from(argv).map(|cli| match cli.command {
                crate::cli::Commands::Volume { command } => command,
                other => panic!("not a volume command: {other:?}"),
            })
        }

        #[test]
        fn verify_takes_a_label_or_none() {
            let VolumeCommands::Verify { label, .. } = try_parse(&["verify", "L6-0002"]).unwrap()
            else {
                panic!("a verify");
            };
            assert_eq!(label.as_deref(), Some("L6-0002"));
            let VolumeCommands::Verify { label, quick, .. } =
                try_parse(&["verify", "--quick"]).unwrap()
            else {
                panic!("a verify");
            };
            assert!(label.is_none() && quick, "the loaded tape, quick");
        }

        /// A write or a resume names its volume, or says `--loaded`: one
        /// answer to the question, never two and never none.
        #[test]
        fn write_and_resume_take_a_label_or_loaded_never_both_or_neither() {
            let fields = |cmd: VolumeCommands| match cmd {
                VolumeCommands::Write { label, loaded, .. }
                | VolumeCommands::Resume { label, loaded, .. } => (label, loaded),
                other => panic!("not a write or a resume: {other:?}"),
            };
            for cmd in ["write", "resume"] {
                let (label, loaded) = fields(try_parse(&[cmd, "--loaded"]).unwrap());
                assert!(label.is_none() && loaded, "{cmd} --loaded");
                let (label, loaded) = fields(try_parse(&[cmd, "L6-0003"]).unwrap());
                assert_eq!((label.as_deref(), loaded), (Some("L6-0003"), false));

                let both = try_parse(&[cmd, "L6-0003", "--loaded"]).unwrap_err();
                assert_eq!(
                    both.kind(),
                    clap::error::ErrorKind::ArgumentConflict,
                    "{cmd}"
                );
                let neither = try_parse(&[cmd]).unwrap_err();
                assert_eq!(
                    neither.kind(),
                    clap::error::ErrorKind::MissingRequiredArgument,
                    "{cmd}"
                );
            }
        }

        /// `--init` initialises a volume it NAMES: with no label there is
        /// nothing to initialise as, and `--loaded` is the other answer.
        #[test]
        fn init_needs_the_label_and_excludes_loaded() {
            assert!(try_parse(&["write", "L6-0003", "--init"]).is_ok());
            let err = try_parse(&["write", "--loaded", "--init"]).unwrap_err();
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::ArgumentConflict,
                "{err}"
            );
            let err = try_parse(&["write", "--init"]).unwrap_err();
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::MissingRequiredArgument,
                "{err}"
            );
        }

        /// The `--loaded` arm's first act is to read the tape, through the
        /// drive `volume identify` uses: with none to read it stops there,
        /// before the write — an error from the read (not the "takes a label"
        /// refusal), and nothing recorded.
        #[test]
        fn loaded_write_and_resume_read_the_tape_before_anything_else() {
            let conn = crate::db::open_memory().unwrap();
            let dir = tempfile::tempdir().unwrap();
            let mut config = Config::default();
            config.backends.lto.push(crate::config::LtoBackendConfig {
                name: "lto0".into(),
                device_tape: "/nonexistent/tapectl-435-nst".into(),
                device_sg: "/nonexistent/tapectl-435-sg".into(),
                generation: "LTO-6".into(),
                capacity_override: None,
                fill_ceiling: 1.0,
                enospc_buffer: "0".into(),
            });
            let paths = TapectlPaths::new(dir.path().join("home"));
            let device = Some("/nonexistent/tapectl-435-nst".to_string());
            for cmd in [
                VolumeCommands::Write {
                    label: None,
                    loaded: true,
                    device: device.clone(),
                    force: false,
                    init: false,
                    move_to: None,
                    allow_missing_escrow: false,
                    prewrite_hash: false,
                    fill_ceiling: None,
                    full_confirm: false,
                    keep_staging: false,
                },
                VolumeCommands::Resume {
                    label: None,
                    loaded: true,
                    device: device.clone(),
                    prewrite_hash: false,
                    full_confirm: false,
                    keep_staging: false,
                },
            ] {
                let err = run(&conn, &paths, &config, &cmd, false, true, false)
                    .expect_err("no tape to read");
                let text = err.to_string();
                assert!(!text.contains("takes a label or --loaded"), "{text}");
                assert!(
                    !text.contains("volume not found"),
                    "the write was not reached: {text}"
                );
                assert_eq!(
                    crate::error::ErrorContract::Write.exit_code(&anyhow::Error::from(err)),
                    crate::error::EXIT_ERROR
                );
            }
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM writes", [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 0);
        }
    }

    /// ADR-0012 2026-10-07 item 33 and the ruling on its question 2:
    /// `volume compact`'s step 3 refused for CONSENT after step 2 sealed and
    /// confirmed the destination is a warning naming `volume compact-finish
    /// --force`, and the command exits 0. A Tier-3 refusal (content not
    /// carried forward) is not: it stays the error it was, non-zero.
    mod compact_step3_outcome_by_refusal {
        use super::*;

        #[test]
        fn a_consent_refusal_is_a_warning_naming_compact_finish_with_force() {
            for text in [
                "retire volume \"L6-SRC\" refused: non-interactive session",
                "retire volume \"L6-SRC\": aborted, not confirmed",
            ] {
                let w = compact_step3_outcome("L6-SRC", "L6-DST", TapectlError::Other(text.into()))
                    .unwrap_or_else(|e| panic!("{text}: a consent refusal warns, got {e}"));
                assert!(
                    w.starts_with(
                        "warning: volume \"L6-DST\" is sealed and confirmed and counts as a \
                         copy, but step 3, retiring the source \"L6-SRC\", did not finish: "
                    ),
                    "{w}"
                );
                assert!(
                    w.ends_with("To finish it: tapectl volume compact-finish L6-SRC --force"),
                    "{w}"
                );
            }
        }

        #[test]
        fn a_tier3_refusal_keeps_its_error_and_its_non_zero_exit() {
            let text = "refusing to retire \"L6-SRC\": it holds the last eligible copy";
            let err = compact_step3_outcome("L6-SRC", "L6-DST", TapectlError::Other(text.into()))
                .expect_err("content not carried forward does not stand: no exit 0");
            assert_eq!(err.to_string(), text, "the message is unchanged");
            assert_eq!(
                crate::error::ErrorContract::Write.exit_code(&anyhow::Error::from(err)),
                crate::error::EXIT_ERROR,
                "the exit it had before item 33"
            );
        }

        /// The Compact arm routes step 3's failure through
        /// `compact_step3_outcome`, propagating its `Err` with `?`.
        #[test]
        fn the_compact_arm_routes_step_3_through_the_outcome() {
            const SRC: &str = include_str!("volume.rs");
            let start = SRC.find("VolumeCommands::Compact {\n").unwrap();
            let arm = &SRC[start..start + SRC[start..].find("VolumeCommands::Deposit").unwrap()];
            assert!(
                arm.contains("match write::compact_finish(conn, config, label, *force || yes) {"),
                "{arm}"
            );
            assert!(
                arm.contains("compact_step3_outcome(label, dest_label, e)?"),
                "{arm}"
            );
        }
    }

    /// Issue #144 (ADR-0012 2026-10-07 item 26): `volume plan
    /// --policy-aware` lists first the staged units a write would help.
    /// `big` v2 is staged and larger, but `big` v1 already has the default
    /// two copies; `small` v1 is staged with none.
    #[test]
    fn plan_rows_put_the_units_with_findings_first_under_policy_aware() {
        let conn = crate::db::open_memory().unwrap();
        conn.execute_batch(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('t', 0, 'active');
             INSERT INTO units (uuid, name, tenant_id, status) VALUES
                 ('u-big', 'big', 1, 'active'), ('u-small', 'small', 1, 'active');
             INSERT INTO snapshots (id, unit_id, version, snapshot_type, status, source_path) VALUES
                 (1, 1, 1, 'full', 'current', '/b'), (2, 1, 2, 'full', 'staged', '/b'),
                 (3, 2, 1, 'full', 'staged', '/s');
             INSERT INTO stage_sets (id, snapshot_id, status, slice_size, total_encrypted_size) VALUES
                 (1, 1, 'cleaned', 524288, 50), (2, 2, 'staged', 524288, 100),
                 (3, 3, 'staged', 524288, 10);
             INSERT INTO volumes (id, label, backend_type, backend_name, capacity_bytes, status) VALUES
                 (1, 'S1', 'lto', 'p', 1, 'sealed'), (2, 'S2', 'lto', 'p', 1, 'sealed');
             INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status) VALUES
                 (1, 1, 1, 'completed'), (1, 1, 2, 'completed');",
        )
        .unwrap();
        let config = Config::default();
        let units = |rows: &[PlanRow]| rows.iter().map(|r| r.unit.clone()).collect::<Vec<_>>();

        let plain = staged_plan_rows(&conn, &config, false).unwrap();
        assert_eq!(units(&plain), ["big", "small"], "largest first");
        assert!(plain.iter().all(|r| r.finding.is_none()));

        let aware = staged_plan_rows(&conn, &config, true).unwrap();
        assert_eq!(units(&aware), ["small", "big"]);
        assert_eq!(aware[0].finding.as_deref(), Some("0 of 2 copies"));
        assert_eq!(aware[1].finding, None);
    }

    /// Issue #144 review: one staged unit whose dotfile `[policy]` cannot be
    /// resolved (edited after staging) must not abort the whole
    /// `volume plan --policy-aware` listing. Like `audit`, the row is shown
    /// with the error as its finding, in the first group.
    #[test]
    fn plan_rows_show_an_unresolvable_policy_as_a_finding() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".tapectl-unit.toml"),
            "[unit]\nuuid = \"u-bad\"\nname = \"bad\"\ncreated = \"2026-01-01T00:00:00Z\"\n\
             tenant = \"t\"\n\n[policy]\ncompression = \"bogus\"\n",
        )
        .unwrap();
        let conn = crate::db::open_memory().unwrap();
        conn.execute_batch(
            "INSERT INTO tenants (name, is_operator, status) VALUES ('t', 0, 'active');
             INSERT INTO units (uuid, name, tenant_id, status) VALUES ('u-big', 'big', 1, 'active');
             INSERT INTO snapshots (id, unit_id, version, snapshot_type, status, source_path) VALUES
                 (1, 1, 1, 'full', 'current', '/b'), (2, 1, 2, 'full', 'staged', '/b');
             INSERT INTO stage_sets (id, snapshot_id, status, slice_size, total_encrypted_size) VALUES
                 (1, 1, 'cleaned', 524288, 50), (2, 2, 'staged', 524288, 100);
             INSERT INTO volumes (id, label, backend_type, backend_name, capacity_bytes, status) VALUES
                 (1, 'S1', 'lto', 'p', 1, 'sealed'), (2, 'S2', 'lto', 'p', 1, 'sealed');
             INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status) VALUES
                 (1, 1, 1, 'completed'), (1, 1, 2, 'completed');",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO units (uuid, name, tenant_id, current_path, status)
             VALUES ('u-bad', 'bad', 1, ?1, 'active')",
            [dir.path().to_string_lossy().to_string()],
        )
        .unwrap();
        conn.execute_batch(
            "INSERT INTO snapshots (id, unit_id, version, snapshot_type, status, source_path) VALUES
                 (3, 2, 1, 'full', 'staged', '/s');
             INSERT INTO stage_sets (id, snapshot_id, status, slice_size, total_encrypted_size) VALUES
                 (3, 3, 'staged', 524288, 10);",
        )
        .unwrap();

        let rows = staged_plan_rows(&conn, &Config::default(), true)
            .expect("one unit's policy fault must not abort the listing");
        let units: Vec<&str> = rows.iter().map(|r| r.unit.as_str()).collect();
        assert_eq!(units, ["bad", "big"], "the unresolvable unit sorts first");
        let finding = rows[0].finding.as_deref().unwrap();
        assert!(
            finding.starts_with("policy unresolvable: ") && finding.contains("compression"),
            "the finding names the fault: {finding}"
        );
        assert_eq!(rows[1].finding, None);
    }

    /// Issue #146: `volume compact`'s destination-label prompt had no
    /// terminal check, so a non-interactive run blocked forever on a handle
    /// nobody would ever write to — the issue #33 failure shape ADR-0008
    /// names by name. Issue #358: `--to` then skipped the prompt — and with
    /// it the only pause in the flow — so on its one drive step 2 wrote to
    /// the source tape still loaded. Every test here injects both the TTY
    /// answer and the read; the real stdin is never touched, exactly as
    /// `cli::consent`'s own tests avoid the ambient terminal.
    mod compact_destination {
        use super::*;
        use std::cell::Cell;

        const SRC: &str = "L6-SRC";
        const DEV: &str = "/nonexistent/tapectl-compact-nst";

        /// Issue #358, the crux: `--to` names the destination, it does not
        /// skip the swap. One `--device` serves both steps, so the flow must
        /// stop for the operator to unload the source and load the
        /// destination — with `--to` exactly as without it.
        #[test]
        fn to_still_pauses_for_the_cartridge_swap() {
            let read = Cell::new(false);
            let label = swap_to_destination_with(Some("L6-DST"), SRC, DEV, true, || {
                read.set(true);
                Ok("\n".to_string())
            })
            .unwrap();
            assert_eq!(label, "L6-DST");
            assert!(
                read.get(),
                "the flow must wait for the operator to swap cartridges, --to or not"
            );
        }

        /// Ctrl-D at the swap prompt is not "I swapped": nothing is written,
        /// and the refusal says how to finish later from the staged slices.
        #[test]
        fn end_of_input_at_the_swap_prompt_stops_before_writing() {
            let err =
                swap_to_destination_with(Some("L6-DST"), SRC, DEV, true, || Ok(String::new()))
                    .expect_err("no answer is not a confirmed swap");
            let msg = err.to_string();
            assert!(
                msg.contains("volume compact-write --destination L6-DST"),
                "{msg}"
            );
        }

        /// Never reached through `run` (the up-front refusal comes first),
        /// but the rule is absolute: a non-terminal is never read.
        #[test]
        fn non_tty_never_reads_stdin_at_the_swap() {
            for to in [None, Some("L6-DST")] {
                let err = swap_to_destination_with(to, SRC, DEV, false, || {
                    panic!("must never attempt to read stdin when non-TTY")
                });
                assert!(err.is_err(), "{to:?}");
            }
        }

        /// Issue #358: with no terminal nobody can swap cartridges, so the
        /// whole command is refused BEFORE step 1 — not after a multi-hour
        /// read — naming the three separate steps that do work unattended.
        #[test]
        fn non_tty_refuses_before_step_1_naming_the_three_step_recipe() {
            for (to, dest) in [(Some("L6-DST"), "L6-DST"), (None, "<DEST>")] {
                let err = refuse_unattended_compact(false, SRC, to)
                    .expect_err("no terminal, nobody to swap cartridges");
                let msg = err.to_string();
                assert!(msg.contains("refused"), "{msg}");
                assert!(msg.contains("terminal"), "{msg}");
                for step in [
                    format!("tapectl volume compact-read {SRC}"),
                    format!("tapectl volume compact-write --destination {dest}"),
                    format!("tapectl volume compact-finish {SRC}"),
                ] {
                    assert!(msg.contains(&step), "missing `{step}`: {msg}");
                }
            }
        }

        #[test]
        fn a_tty_is_not_refused_up_front() {
            assert_eq!(
                refuse_unattended_compact(true, SRC, Some(" L6-DST\n")).unwrap(),
                Some("L6-DST".to_string()),
                "--to is trimmed"
            );
            assert_eq!(refuse_unattended_compact(true, SRC, None).unwrap(), None);
        }

        /// An empty `--to` is a mistake, not a request to prompt — and it is
        /// caught before step 1 now, not after the read.
        #[test]
        fn an_empty_to_is_rejected_before_step_1() {
            for is_tty in [true, false] {
                let err = refuse_unattended_compact(is_tty, SRC, Some("   "))
                    .expect_err("an empty --to is a mistake");
                assert!(err.to_string().contains("--to"), "{err}");
            }
        }

        /// Without `--to` the one prompt is still both the swap and the
        /// label, and it names the source to unload.
        #[test]
        fn a_tty_without_to_prompts_and_trims_the_answer() {
            let label =
                swap_to_destination_with(None, SRC, DEV, true, || Ok("  L6-DST \n".to_string()))
                    .unwrap();
            assert_eq!(label, "L6-DST");
        }

        /// A bare Enter is not a destination. It used to be caught after
        /// the read; it still is.
        #[test]
        fn an_empty_answer_on_a_tty_is_refused() {
            assert!(
                swap_to_destination_with(None, SRC, DEV, true, || Ok("\n".to_string())).is_err()
            );
        }

        /// Through the real command arm: `volume compact --to` with no
        /// terminal (every test build — `stdin_is_terminal` is false under
        /// `cfg(test)`) refuses before step 1 touches the drive: no MAM
        /// read is journalled and no contact is recorded. Until #358 this
        /// read the whole source volume and then failed at File 0 of the
        /// source tape still loaded.
        #[test]
        fn volume_compact_to_without_a_terminal_refuses_before_touching_the_drive() {
            let conn = crate::db::open_memory().unwrap();
            let tmp = tempfile::TempDir::new().unwrap();
            let paths = TapectlPaths::new(tmp.path().join("home"));
            let mut config = Config::default();
            config.backends.lto.push(crate::config::LtoBackendConfig {
                name: "lto0".into(),
                device_tape: DEV.into(),
                device_sg: "/nonexistent/tapectl-compact-sg".into(),
                generation: "LTO-6".into(),
                capacity_override: None,
                fill_ceiling: 1.0,
                enospc_buffer: "0".into(),
            });
            let cmd = VolumeCommands::Compact {
                label: SRC.into(),
                to: Some("L6-DST".into()),
                device: Some(DEV.into()),
                allow_missing_escrow: false,
                prewrite_hash: false,
                fill_ceiling: None,
                full_confirm: false,
                force: true,
            };
            let err = run(&conn, &paths, &config, &cmd, false, true, false)
                .expect_err("no terminal: nobody can swap the cartridges");
            let msg = err.to_string();
            assert!(
                msg.contains("tapectl volume compact-read L6-SRC"),
                "the refusal names the unattended recipe: {msg}"
            );
            let touched: i64 = conn
                .query_row(
                    "SELECT (SELECT COUNT(*) FROM mam_journal)
                          + (SELECT COUNT(*) FROM cartridge_contacts)",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(touched, 0, "refused before step 1 read anything");
        }

        /// The code of `src` with comment lines dropped and every string
        /// literal's contents blanked, so a word in a message or a doc is
        /// neither a call nor a use of `to`.
        fn code_only(src: &str) -> String {
            let mut out = String::new();
            for line in src.lines().filter(|l| !l.trim_start().starts_with("//")) {
                let (mut in_str, mut escaped) = (false, false);
                for c in line.chars() {
                    if in_str {
                        match (escaped, c) {
                            (true, _) => escaped = false,
                            (false, '\\') => escaped = true,
                            (false, '"') => {
                                in_str = false;
                                out.push('"');
                            }
                            _ => {}
                        }
                    } else {
                        in_str = c == '"';
                        out.push(c);
                    }
                }
                out.push('\n');
            }
            out
        }

        /// Where `run`'s Compact arm takes the swap, or why it does not take
        /// it unconditionally.
        fn swap_is_unconditional(arm: &str) -> std::result::Result<(), String> {
            let code = code_only(arm);
            const SWAP: &str = "swap_to_destination(";
            if code.matches(SWAP).count() != 1 {
                return Err(format!(
                    "{} swap calls, not one",
                    code.matches(SWAP).count()
                ));
            }
            // At the arm's own statement level (12 spaces), so it sits in no
            // `if`, `match` arm or closure — and fed `--to` itself.
            let stmt =
                "            let dest_label = swap_to_destination(to.as_deref(), label, &device)?;";
            if !code.lines().any(|l| l == stmt) {
                return Err("the swap is not an unconditional statement of the arm".into());
            }
            let (read, swap, write) = (
                code.find("write::compact_read(").ok_or("no step 1")?,
                code.find(SWAP).unwrap(),
                code.find("write::compact_write(").ok_or("no step 2")?,
            );
            if !(read < swap && swap < write) {
                return Err("the swap is not between step 1's read and step 2's write".into());
            }
            // `to` is bound, checked once before step 1, and handed to the
            // swap — any other use is a decision taken on it.
            let uses = code
                .match_indices("to")
                .filter(|(i, _)| {
                    let ident = |c: char| c.is_alphanumeric() || c == '_';
                    !code[..*i].ends_with(ident) && !code[i + 2..].starts_with(ident)
                })
                .count();
            if uses != 4 {
                return Err(format!(
                    "`to` appears {uses} times: the binding, `let to =`, the pre-step-1 \
                     check and the swap make 4"
                ));
            }
            Ok(())
        }

        /// Issue #358's review: the pause with `--to` is proved above only at
        /// function level (`swap_to_destination_with`), and a test through
        /// `run` cannot reach it — under `cfg(test)` stdin is never a
        /// terminal, so `run` refuses before step 1. So nothing pinned that
        /// `run` still routes `--to` THROUGH the swap: a short-circuit there
        /// (`match to { Some(t) => t, None => swap_to_destination(..)? }`),
        /// the exact regression #358 fixed, would have left every test green.
        /// Pinned by source, with the checker's own negative controls.
        #[test]
        fn run_routes_to_through_the_swap_unconditionally() {
            let src = include_str!("volume.rs");
            let prod = &src[..src.find("#[cfg(test)]\nmod tests").unwrap()];
            let start = prod
                .find("        VolumeCommands::Compact {")
                .expect("run's Compact arm");
            let end = start
                + prod[start..]
                    .find("\n        VolumeCommands::Deposit {")
                    .expect("the arm after it");
            let arm = &prod[start..end];
            assert!(
                arm.contains("refuse_unattended_compact("),
                "positive control: the slice is the real arm"
            );
            assert_eq!(swap_is_unconditional(arm), Ok(()));

            // The fixed arm, reduced to the lines the checker reads, and
            // three ways back to the bug.
            let good = "        VolumeCommands::Compact {\n            to,\n        } => {\n            \
                        let to = refuse_unattended_compact(tty, label, to.as_deref())?;\n            \
                        let report = write::compact_read(conn)?;\n            \
                        let dest_label = swap_to_destination(to.as_deref(), label, &device)?;\n            \
                        write::compact_write(conn, &dest_label)?;\n        }\n";
            assert_eq!(swap_is_unconditional(good), Ok(()), "calibration");
            for (shape, bad) in [
                (
                    "a match that skips the swap for --to",
                    good.replace(
                        "            let dest_label = swap_to_destination(to.as_deref(), label, &device)?;",
                        "            let dest_label = match to {\n                Some(t) => t,\n                \
                         None => swap_to_destination(to.as_deref(), label, &device)?,\n            };",
                    ),
                ),
                (
                    "an early return when --to is given",
                    good.replace(
                        "            let report",
                        "            if to.is_some() {\n                return Ok(());\n            }\n            let report",
                    ),
                ),
                (
                    "the swap after the write",
                    good.replace(
                        "            let dest_label = swap_to_destination(to.as_deref(), label, &device)?;\n",
                        "",
                    ) + "            let dest_label = swap_to_destination(to.as_deref(), label, &device)?;\n",
                ),
            ] {
                assert!(swap_is_unconditional(&bad).is_err(), "not rejected: {shape}");
            }
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
                false,
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
                false,
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
                false,
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
                false,
            )
            .unwrap();
        }
    }

    /// ADR-0012 2026-10-07 item 33 (#423): `volume verify` says what it
    /// recorded of the write figures, or why it withheld them, in both
    /// outputs; `--json` carries `null` when it recorded nothing.
    #[test]
    fn verify_reports_the_write_figures_it_recorded_or_withheld() {
        use crate::volume::figures::Figures;
        let recorded = Figures::Recorded(vec![
            "num_data_files: 0 -> 1".to_string(),
            "write_completed event: (none) -> recorded".to_string(),
        ]);
        assert_eq!(
            figures_message("L6-0002", &recorded),
            "volume \"L6-0002\": recorded the write figures it was missing or had wrong: \
             num_data_files: 0 -> 1; write_completed event: (none) -> recorded"
        );
        assert_eq!(
            figures_json(Some(&recorded)),
            serde_json::json!({"recorded": [
                "num_data_files: 0 -> 1",
                "write_completed event: (none) -> recorded",
            ]})
        );
        let withheld = Figures::Withheld("the catalog records two slices".to_string());
        let msg = figures_message("L6-0002", &withheld);
        assert!(
            msg.contains("NOT recorded: the catalog records two slices"),
            "{msg}"
        );
        assert!(msg.ends_with("The verify itself passed."), "{msg}");
        assert_eq!(
            figures_json(Some(&withheld)),
            serde_json::json!({"withheld": "the catalog records two slices"})
        );
        assert_eq!(figures_json(None), serde_json::Value::Null);
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

    /// Issue #356 (CTO ruling 2026-09-28): a failure that proved nothing
    /// about the medium — a read error, a short read, a transport fault —
    /// left the volume untouched, and exits 3, never the 2 that now means
    /// "the medium is bad". Still non-zero: issue #45/H10 (a scheduled
    /// verify that could read nothing must not report success) stands.
    #[test]
    fn verify_exit_code_a_failure_that_proved_nothing_is_inconclusive() {
        let report = write::VerifyReport {
            checked: 10,
            passed: 9,
            failed: 1,
            ..Default::default()
        };
        assert_eq!(
            verify_exit_code(&report),
            crate::error::EXIT_VERIFY_INCONCLUSIVE
        );
        assert_eq!(crate::error::EXIT_VERIFY_INCONCLUSIVE, 3, "the ruled value");
    }

    #[test]
    fn verify_exit_code_all_failed_without_medium_evidence_is_inconclusive() {
        let report = write::VerifyReport {
            checked: 3,
            passed: 0,
            failed: 3,
            ..Default::default()
        };
        assert_eq!(
            verify_exit_code(&report),
            crate::error::EXIT_VERIFY_INCONCLUSIVE
        );
    }

    fn a_proving_mismatch() -> crate::store::Mismatch {
        crate::store::Mismatch {
            position: 4,
            kind: crate::store::MismatchKind::ContentHashMismatch,
            expected: "aa".into(),
            actual: "bb".into(),
        }
    }

    /// Issue #356: 2 is the quarantine, and only the quarantine — the one
    /// verify outcome whose remedy is another cartridge.
    #[test]
    fn verify_exit_code_a_quarantine_is_medium_bad() {
        let report = write::VerifyReport {
            checked: 10,
            passed: 9,
            failed: 1,
            mismatches: vec![a_proving_mismatch()],
            quarantine: Some(write::QuarantineEffect {
                previous_condition: "ok".into(),
                proof: vec![a_proving_mismatch()],
            }),
            ..Default::default()
        };
        assert_eq!(
            verify_exit_code(&report),
            crate::error::EXIT_VERIFY_MEDIUM_BAD
        );
        assert_eq!(crate::error::EXIT_VERIFY_MEDIUM_BAD, 2, "the ruled value");
    }

    /// Re-verifying a volume already quarantined, and finding the medium
    /// still bad, is the same finding: 2, although the condition did not
    /// change this time.
    #[test]
    fn verify_exit_code_a_reconfirmed_quarantine_is_still_medium_bad() {
        let report = write::VerifyReport {
            checked: 10,
            passed: 9,
            failed: 1,
            mismatches: vec![a_proving_mismatch()],
            quarantine: Some(write::QuarantineEffect {
                previous_condition: "quarantined".into(),
                proof: vec![a_proving_mismatch()],
            }),
            ..Default::default()
        };
        assert_eq!(
            verify_exit_code(&report),
            crate::error::EXIT_VERIFY_MEDIUM_BAD
        );
    }

    #[test]
    fn verify_exit_code_empty_report_is_success() {
        // A volume with zero checkable slices (degenerate but not a
        // failure) must not be reported as a violation.
        let report = write::VerifyReport::default();
        assert_eq!(verify_exit_code(&report), crate::error::EXIT_SUCCESS);
    }

    /// Issue #356: every ERROR a `volume verify` invocation stops on — an
    /// unknown label, the wrong tape, an empty drive, a front index that
    /// will not read — reached no verdict about the medium and changed
    /// nothing, so it exits 3. Only a quarantine exits 2. Every other volume
    /// subcommand keeps the ordinary error code.
    #[test]
    fn a_verify_that_errors_exits_inconclusive_and_nothing_else_changes() {
        let verify = VolumeCommands::Verify {
            label: Some("L".into()),
            device: None,
            full: false,
            quick: false,
        };
        assert_eq!(error_contract(&verify), crate::error::ErrorContract::Verify);
        for other in [
            VolumeCommands::Identify { device: None },
            VolumeCommands::Retire { label: "L".into() },
            VolumeCommands::CompactRead {
                label: "L".into(),
                device: None,
            },
        ] {
            assert_eq!(
                error_contract(&other),
                crate::error::ErrorContract::Ordinary,
                "{other:?}"
            );
        }
    }

    /// ADR-0012, 2026-10-07 item 20 (issue #408): the four volume
    /// subcommands that write share the write family's exit table.
    #[test]
    fn the_volume_writes_take_the_write_family_contract() {
        for write in [
            VolumeCommands::Write {
                label: Some("L".into()),
                loaded: false,
                device: None,
                force: false,
                init: false,
                move_to: None,
                allow_missing_escrow: false,
                prewrite_hash: false,
                fill_ceiling: None,
                full_confirm: false,
                keep_staging: false,
            },
            VolumeCommands::Resume {
                label: Some("L".into()),
                loaded: false,
                device: None,
                prewrite_hash: false,
                full_confirm: false,
                keep_staging: false,
            },
            VolumeCommands::CompactWrite {
                destination: "L".into(),
                device: None,
                allow_missing_escrow: false,
                prewrite_hash: false,
                fill_ceiling: None,
                full_confirm: false,
            },
            VolumeCommands::Compact {
                label: "S".into(),
                to: None,
                device: None,
                allow_missing_escrow: false,
                prewrite_hash: false,
                fill_ceiling: None,
                full_confirm: false,
                force: false,
            },
        ] {
            assert_eq!(
                error_contract(&write),
                crate::error::ErrorContract::Write,
                "{write:?}"
            );
        }
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

        /// ADR-0012 2026-10-06 items 1 and 24: VERIFIED is the newest FULL
        /// readback. A quick verify or quick confirm newer than it (L6-0001)
        /// does not move it, and a volume with only quick ones (L6-0002) was
        /// never verified — none of its bytes were read.
        #[test]
        fn a_quick_readback_does_not_count_as_verified() {
            let conn = seed();
            let id = |label: &str| -> i64 {
                conn.query_row("SELECT id FROM volumes WHERE label = ?1", [label], |r| {
                    r.get(0)
                })
                .unwrap()
            };
            let before = volume_rows(&conn, None).unwrap();
            let full_l6_0001 = before
                .iter()
                .find(|r| r.label == "L6-0001")
                .unwrap()
                .verified
                .clone();
            assert!(
                full_l6_0001.is_some(),
                "positive control: seeded full verify"
            );
            for label in ["L6-0001", "L6-0002"] {
                conn.execute(
                    "INSERT INTO verification_sessions (volume_id, verify_type, outcome, completed_at)
                     VALUES (?1, 'quick', 'passed', datetime('now'))",
                    [id(label)],
                )
                .unwrap();
            }
            let rows = volume_rows(&conn, None).unwrap();
            let row = |label: &str| rows.iter().find(|r| r.label == label).unwrap();
            assert_eq!(row("L6-0001").verified, full_l6_0001);
            assert_eq!(row("L6-0002").verified, None);
        }

        /// ADR-0012 2026-10-07 item 30: VERIFIED dates a full readback by
        /// its START. A continued full verify of L6-0002 that completed
        /// today started, by its oldest checkpoint, 100 days ago, and the
        /// column says so: some of its files were last read then.
        #[test]
        fn a_continued_full_readback_shows_its_oldest_checkpoint() {
            let conn = seed();
            conn.execute(
                "INSERT INTO verification_sessions
                     (volume_id, verify_type, outcome, started_at, completed_at)
                 SELECT id, 'full', 'passed', datetime('now', '-100 days'), datetime('now')
                 FROM volumes WHERE label = 'L6-0002'",
                [],
            )
            .unwrap();
            let rows = volume_rows(&conn, None).unwrap();
            let row = rows.iter().find(|r| r.label == "L6-0002").unwrap();
            assert_eq!(
                crate::policy::evidence::compact_age(
                    row.verified.as_deref(),
                    chrono::Utc::now().naive_utc()
                ),
                "100d ago",
                "{:?}",
                row.verified
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
            // Issue #242: 'quarantined' left the accepted-status set
            // entirely -- an ordinary typo's error must not name it.
            assert!(!msg.contains("quarantined"), "{msg}");
        }

        /// Issue #242: 'quarantined' is not an ordinary unknown value -- it
        /// used to be a real `--status`, and an operator typing it deserves
        /// to be told where the fact went, not just "unknown value".
        #[test]
        fn validate_volume_status_names_where_quarantined_went() {
            let err = validate_volume_status("quarantined").unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("ADR-0012"), "{msg}");
            assert!(msg.contains("observed_condition"), "{msg}");
        }

        /// Issue #362: the `--status` closed set is the live CHECK, not a
        /// copy of a migration that a later one superseded (`blank` and
        /// `missing` sat here long after anything could have set them).
        #[test]
        fn volume_statuses_equal_the_live_check() {
            let mut ours: Vec<String> = VOLUME_STATUSES.iter().map(|s| s.to_string()).collect();
            ours.sort();
            assert_eq!(ours, crate::db::live_status_check("volumes"));
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
                    "media_type",
                    "capacity_bytes",
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
            // Issue #482: the stored columns, raw -- the keys `volume info
            // --json` already uses, a number and not "2.50 TB".
            assert_eq!(initialized["media_type"], "LTO-6");
            assert_eq!(initialized["capacity_bytes"], 2_500_000_000_000u64);
        }

        /// Issue #482: `volume list` shows the generation `volume init`
        /// decided and the capacity it stored, so an LTO-5 cartridge written
        /// in an LTO-6 drive is told apart from an LTO-6 without opening
        /// `volume info`. The capacity is DECIMAL (ADR-0012), and a row with
        /// no recorded generation renders the dash, not an empty cell.
        #[test]
        fn list_shows_the_generation_and_the_decimal_capacity_of_each_volume() {
            let conn = crate::db::open_memory().unwrap();
            conn.execute_batch(
                "INSERT INTO volumes (label, backend_type, backend_name, media_type, capacity_bytes,
                                      status, created_at) VALUES
                    ('L5-0001','lto','lto6','LTO-5',1500000000000,'sealed','2026-09-03 00:00:00'),
                    ('L6-0001','lto','lto6','LTO-6',2500000000000,'sealed','2026-09-02 00:00:00'),
                    ('OLD-0001','lto','lto6',NULL,0,'erased','2026-09-01 00:00:00');",
            )
            .unwrap();
            let rows = volume_rows(&conn, None).unwrap();

            let table = Table::new(&rows).to_string();
            let header = table.lines().nth(1).expect(&table);
            assert!(header.contains("GENERATION"), "{table}");
            assert!(header.contains("CAPACITY"), "{table}");
            let cells = |label: &str| -> Vec<String> {
                table
                    .lines()
                    .find(|l| l.contains(label))
                    .unwrap_or_else(|| panic!("no row for {label}:\n{table}"))
                    .split('|')
                    .map(|c| c.trim().to_string())
                    .collect()
            };
            let col = |name: &str| -> usize {
                header
                    .split('|')
                    .position(|c| c.trim() == name)
                    .unwrap_or_else(|| panic!("no column {name}:\n{table}"))
            };
            let (g, c) = (col("GENERATION"), col("CAPACITY"));
            assert_eq!(cells("L5-0001")[g], "LTO-5", "{table}");
            assert_eq!(cells("L5-0001")[c], "1.50 TB", "{table}");
            assert_eq!(cells("L6-0001")[g], "LTO-6", "{table}");
            assert_eq!(cells("L6-0001")[c], "2.50 TB", "{table}");
            assert_eq!(cells("OLD-0001")[g], "—", "{table}");
            assert_eq!(cells("OLD-0001")[c], "0 B", "{table}");

            let json = volume_rows_to_json(&rows);
            let l5 = json
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["label"] == "L5-0001")
                .unwrap();
            assert_eq!(l5["media_type"], "LTO-5");
            assert_eq!(l5["capacity_bytes"], 1_500_000_000_000u64);
            let old = json
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["label"] == "OLD-0001")
                .unwrap();
            assert!(old["media_type"].is_null(), "raw null, not the table's dash: {old}");
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
                condition: "ok".into(),
                cartridge: None,
                location: None,
                copies: None,
                verified: None,
                media_type: None,
                capacity_bytes: 0,
            };
            let has_data_never_verified = VolumeRow {
                label: "L6-DATA".into(),
                status: "sealed".into(),
                condition: "ok".into(),
                cartridge: None,
                location: None,
                copies: Some(1),
                verified: None,
                media_type: Some("LTO-6".into()),
                capacity_bytes: 2_500_000_000_000,
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

        /// Issue #439: the dossier carries this volume's recorded feed
        /// ratios (and only its own), in `--json` too; a volume with none
        /// carries an empty list.
        #[test]
        fn info_carries_the_volumes_feed_ratios() {
            use crate::tape::feed_ratio::{Assessment, FeedRatio, EVENT_ACTION, EVENT_FIELD};
            let conn = crate::db::open_memory().unwrap();
            for l in ["L6-FR", "L6-OTHER"] {
                conn.execute(
                    "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                          capacity_bytes, status)
                     VALUES (?1, 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed')",
                    [l],
                )
                .unwrap();
                let id = conn.last_insert_rowid();
                let a = Assessment {
                    ratio: FeedRatio::new(1_505, 1_000_000_000).unwrap(),
                    suppressed: None,
                };
                crate::db::events::log_event(
                    &conn,
                    "volume",
                    id,
                    Some(l),
                    EVENT_ACTION,
                    Some(EVENT_FIELD),
                    None,
                    Some("1.5050"),
                    Some(&a.details_json(1)),
                    None,
                )
                .unwrap();
            }
            conn.execute(
                "INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                      capacity_bytes, status)
                 VALUES ('L6-NONE', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed')",
                [],
            )
            .unwrap();

            let info = volume_info(&conn, "L6-FR", false).unwrap();
            assert_eq!(info.feed_ratios.len(), 1, "{:?}", info.feed_ratios);
            assert_eq!(info.feed_ratios[0].volume, "L6-FR");
            let v = serde_json::to_value(&info).unwrap();
            assert_eq!(v["feed_ratios"][0]["verdict"], "above_threshold");
            assert_eq!(v["feed_ratios"][0]["ratio"], 1.505);
            let none = volume_info(&conn, "L6-NONE", false).unwrap();
            assert_eq!(
                serde_json::to_value(&none).unwrap()["feed_ratios"],
                serde_json::json!([])
            );
        }

        /// Issue #305: the seal time is printed, and its absence named.
        #[test]
        fn info_prints_the_seal_time_or_says_none_was_recorded() {
            assert_eq!(
                render_sealed_line(Some("2026-09-12 03:04:05")),
                "  Sealed:      2026-09-12 03:04:05"
            );
            assert_eq!(
                render_sealed_line(None),
                "  Sealed:      (no seal recorded)"
            );
        }

        /// Issue #392: "First write" and "Last write" both showed when the
        /// write session FINISHED (the bookkeeping stamps both at the end),
        /// so a write that ran from 02:53 to 18:06 showed 18:06 twice. First
        /// write is when the first write began — from `writes.started_at`,
        /// which also corrects a row stamped before this was fixed — and
        /// each line says which end it is.
        #[test]
        fn info_first_write_is_when_the_first_write_began() {
            let conn = crate::db::open_memory().unwrap();
            conn.execute_batch(
                "INSERT INTO tenants (name, is_operator, status) VALUES ('t', 0, 'active');
                 INSERT INTO units (uuid, name, tenant_id, checksum_mode, encrypt, status)
                     VALUES ('u', 'unit', 1, 'mtime_size', 1, 'active');
                 INSERT INTO snapshots (unit_id, version, snapshot_type, status, source_path)
                     VALUES (1, 1, 'full', 'current', '/x');
                 INSERT INTO stage_sets (snapshot_id, status, slice_size) VALUES (1, 'staged', 1);
                 INSERT INTO volumes (label, backend_type, backend_name, media_type,
                                      capacity_bytes, status, first_write, last_write)
                     VALUES ('L6-T', 'lto', 'lto0', 'LTO-6', 2500000000000, 'sealed',
                             '2026-10-01 18:06:17', '2026-10-01 18:06:17');
                 INSERT INTO writes (stage_set_id, snapshot_id, volume_id, status,
                                     started_at, completed_at)
                     VALUES (1, 1, 1, 'completed', '2026-10-01 02:53:12',
                             '2026-10-01 18:06:17');",
            )
            .unwrap();
            let info = volume_info(&conn, "L6-T", false).unwrap();
            assert_eq!(info.first_write.as_deref(), Some("2026-10-01 02:53:12"));
            assert_eq!(info.last_write.as_deref(), Some("2026-10-01 18:06:17"));
            let text = render_write_span(info.first_write.as_deref(), info.last_write.as_deref());
            assert!(
                text.contains("First write: 2026-10-01 02:53:12 (started)")
                    && text.contains("Last write:  2026-10-01 18:06:17 (completed)"),
                "{text}"
            );
            assert!(render_write_span(None, None).contains("First write: (never written)"));
        }

        /// Issue #392: the verification history printed a verify's START,
        /// unlabelled, while `report verify-status` printed its completion.
        #[test]
        fn verification_history_names_the_start_and_the_completion() {
            let rows = [VerificationRow {
                started_at: "2026-10-01 08:14:57".into(),
                completed_at: Some("2026-10-01 18:06:17".into()),
                verify_type: "full".into(),
                outcome: "passed".into(),
                slices_checked: 27,
                slices_passed: 27,
                slices_failed: 0,
            }];
            let text = render_verification_history(&rows);
            assert!(
                text.contains("started 2026-10-01 08:14:57")
                    && text.contains("completed 2026-10-01 18:06:17"),
                "{text}"
            );
            assert_eq!(
                render_verification_history(&[]),
                "Verification history: never verified\n"
            );
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

    /// Issue #280: `volume verify`'s clean-clear message ("RETURNED TO
    /// SERVICE") asserted "it counts as a copy again" unconditionally, but
    /// `clear_condition_on_clean_full_verify` only ever touches
    /// `observed_condition` -- the claim is only true while `status =
    /// 'sealed'`. These tests drive `clean_clear_message` and
    /// `returned_to_service_json` directly (the smallest functions that
    /// decide the wording), for the three reachable (status, condition)
    /// shapes named in the issue.
    mod clean_clear_message_tests {
        use super::*;

        /// Test 1, the POSITIVE CONTROL: a `sealed` volume whose condition
        /// this verify just cleared is exactly the case the 2026-09-18
        /// ruling (ADR-0012) was written for, and it must not regress.
        /// Without this passing, the absence assertions in the other two
        /// tests below cannot distinguish "correctly refuses to claim" from
        /// "never claims anything" -- this is the case that proves the
        /// function CAN say RETURNED TO SERVICE at all.
        #[test]
        fn sealed_and_cleared_says_returned_to_service_and_counts_as_a_copy() {
            let msg = clean_clear_message("L6-0001", "quarantined", "sealed", true, true, false);
            assert!(
                msg.contains("RETURNED TO SERVICE"),
                "a sealed volume that now counts as a copy must say so: {msg}"
            );
            assert!(
                msg.contains("counts as a copy again"),
                "the durability claim itself must still be made here: {msg}"
            );
            assert!(
                returned_to_service_json(true, true),
                "the --json twin must agree with the human message"
            );
        }

        /// Test 2: state (A) from issue #280 -- a `retired` volume a clean
        /// verify just cleared the condition on. Must NOT say RETURNED TO
        /// SERVICE, must NOT claim it counts, and must name `status =
        /// "retired"` as the reason -- the operator retired it on purpose
        /// and a medium observation does not undo that.
        #[test]
        fn retired_and_cleared_does_not_claim_returned_to_service() {
            let msg = clean_clear_message("L6-0002", "quarantined", "retired", true, false, false);
            assert!(
                !msg.contains("RETURNED TO SERVICE"),
                "a retired volume must never be told it returned to service: {msg}"
            );
            assert!(
                !msg.contains("counts as a copy again"),
                "a retired volume does not count as a copy, and must not be told it does: {msg}"
            );
            assert!(
                msg.contains("retired") || msg.contains("RETIRED"),
                "the status itself must be named as the reason: {msg}"
            );
            assert!(
                !returned_to_service_json(true, false),
                "the --json twin must not carry a true returned_to_service for a retired \
                 volume"
            );
        }

        /// Test 3: state (B) from issue #280 -- a volume left `initialized`
        /// because `SealedPending::confirm`'s `proves_medium_bad` arm never
        /// reached the sealing UPDATE, with `sealed_at` set (the tape IS
        /// physically sealed) and every `writes` row `aborted`, now cleared
        /// by this verify. ADR-0012's 2026-09-23 amendment makes `volume
        /// resume` adopt exactly this state, so the message must NAME it --
        /// replacing the pre-ruling test
        /// `initialized_sealed_but_unconfirmed_does_not_claim_service_or_name_resume`,
        /// which pinned its absence. Paired with
        /// `volume::session::tests::resume_adopts_an_aborted_sealed_session_after_a_clean_full_verify_and_seals_it`,
        /// which builds this state for real, computes this message's
        /// `resume_would_reconfirm` input from it, and proves the named
        /// resume succeeds.
        #[test]
        fn initialized_sealed_and_resumable_names_volume_resume_without_claiming_service() {
            let msg =
                clean_clear_message("L6-0003", "quarantined", "initialized", true, false, true);
            assert!(
                !msg.contains("RETURNED TO SERVICE"),
                "an initialized, unconfirmed volume must never be told it returned to \
                 service: {msg}"
            );
            assert!(
                !msg.contains("counts as a copy again"),
                "it does not count as a copy, and must not be told it does: {msg}"
            );
            assert!(
                msg.contains("initialized"),
                "the actual status must be named: {msg}"
            );
            assert!(
                msg.contains("tapectl volume resume L6-0003"),
                "resume would adopt this volume, so the message must name it: {msg}"
            );
            assert!(
                !returned_to_service_json(true, false),
                "the --json twin must not carry a true returned_to_service for an \
                 initialized, unconfirmed volume"
            );
        }

        /// The other half of Test 3's derivation: when resume would NOT
        /// adopt the volume (e.g. no clean full verify after the abort, or
        /// a seal that was never recorded), naming it would hand the
        /// operator a command that refuses -- so it is not named.
        #[test]
        fn initialized_sealed_but_not_resumable_does_not_name_resume() {
            let msg =
                clean_clear_message("L6-0003", "quarantined", "initialized", true, false, false);
            assert!(
                !msg.to_lowercase().contains("resume"),
                "resume would refuse here, so it must not be named: {msg}"
            );
            assert!(!msg.contains("RETURNED TO SERVICE"), "{msg}");
        }

        /// The `sealed_at`-set note is materially true and worth saying
        /// (the bytes ARE on the tape) precisely in state (B) -- assert it
        /// is actually said, not just that nothing false is said.
        #[test]
        fn initialized_sealed_but_unconfirmed_says_the_tape_is_physically_sealed() {
            let msg =
                clean_clear_message("L6-0003", "quarantined", "initialized", true, false, false);
            assert!(
                msg.to_lowercase().contains("physically sealed"),
                "sealed_at is set -- the tape really does carry the seal marker and bytes, \
                 which is a materially different fact from a blank tape: {msg}"
            );
        }

        /// `returned_to_service_json` in isolation: the conjunction must
        /// never read true unless BOTH `cleared` and `counts_as_copy` are
        /// true.
        #[test]
        fn returned_to_service_json_requires_both_cleared_and_counts_as_copy() {
            assert!(returned_to_service_json(true, true));
            assert!(!returned_to_service_json(true, false));
            assert!(!returned_to_service_json(false, true));
            assert!(!returned_to_service_json(false, false));
        }
    }

    /// Issue #361: the populated write history is headed "Writes:" too —
    /// tests/cli_smoke.rs reaches only the empty case, since a volume with
    /// `writes` rows needs a tape. "Receipt" means the recipient list.
    #[test]
    fn write_history_is_headed_writes_when_there_are_writes() {
        let row = |unit: &str, completed_at: Option<&str>| WriteRow {
            unit: unit.to_string(),
            version: 3,
            status: "some-status".to_string(),
            started_at: None,
            completed_at: completed_at.map(str::to_string),
            num_slices: None,
            bytes: None,
        };
        let text = render_write_history(&[
            row("photos", Some("2026-09-01 10:00:00")),
            row("docs", None),
        ]);
        assert_eq!(
            text,
            "Writes:\n    photos v3: some-status (2026-09-01 10:00:00)\n    \
             docs v3: some-status (not completed)\n"
        );
        assert_eq!(render_write_history(&[]), "Writes: none\n");
    }
}
