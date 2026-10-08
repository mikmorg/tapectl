use std::process;

use thiserror::Error;

/// Exit codes per design: 0=success, 1=warnings, 2=errors/violations.
///
/// Mirrors the convention `audit` already established (`src/cli/audit.rs`):
/// 0=clean, 1=warning, 2=violation. `db fsck` (src/main.rs) computes its exit
/// code against these same constants — see `fsck_exit_code` (issue #45/H10).
/// `volume verify` has its own three, below (issue #356), and `audit`'s
/// errors exit [`EXIT_AUDIT_ERROR`] (issue #408).
pub const EXIT_SUCCESS: i32 = 0;
pub const EXIT_WARNING: i32 = 1;
pub const EXIT_ERROR: i32 = 2;

/// `volume verify`: the verify FAILED and PROVED THE MEDIUM BAD — at least
/// one mismatch is medium evidence, so the volume's condition is (or already
/// was) `quarantined` and it no longer counts as a copy (ADR-0012, the
/// 2026-09-17 amendment). The remedy is another cartridge.
///
/// CTO ruling, 2026-09-28 (issue #356): exit codes are an interface, and a
/// failed verify has two outcomes with opposite remedies. Until then both
/// exited 2, and a script — the systemd timers' health ping, `first-run.sh`
/// step 13 — could tell them apart only by parsing `--json`. Now 2 is
/// reachable from `volume verify` ONLY through a quarantine; every other
/// failure is [`EXIT_VERIFY_INCONCLUSIVE`] — every error the command
/// returns, and a command line that does not parse, whose usage error clap
/// would otherwise exit 2 with (`main`'s `parse_error_exit_code`).
pub const EXIT_VERIFY_MEDIUM_BAD: i32 = 2;

/// `volume verify`: INCONCLUSIVE — the verify reached no verdict about the
/// medium and the volume is untouched. A drive or transport failure (a read
/// error, a short read, an unreadable front index), a refusal before the
/// tape was read (no cartridge loaded, the wrong tape, a drive that cannot
/// read this generation, an unknown label, `--dry-run`), a command line
/// that does not parse (a missing label, a mistyped flag), or any other
/// error the command stopped on. The remedy is the drive, the cartridge in
/// it, or the command line — then verify again. Issue #356.
pub const EXIT_VERIFY_INCONCLUSIVE: i32 = 3;

/// The catalog was busy: another tapectl process held SQLite's write lock
/// for longer than this command would wait (issue #377). Not a verdict and
/// not a failure of the thing the command checks — the remedy is to run it
/// again later. 75 is sysexits' `EX_TEMPFAIL`, the conventional "temporary
/// failure, retry", chosen so it can never collide with a verdict code.
/// `volume verify` keeps its own contract instead (every error is
/// [`EXIT_VERIFY_INCONCLUSIVE`], which already means "try again").
pub const EXIT_CATALOG_BUSY: i32 = 75;

/// `audit`: the audit stopped on an ERROR and reached no verdict (ADR-0012,
/// the 2026-10-07 amendment, item 19; issue #408). 70 is sysexits'
/// `EX_SOFTWARE`. Until then every error exited 2, the code that means "at
/// least one violation", so a scheduled wrapper paged violations for an
/// audit that never ran. Now 2 is reachable from `audit` only through a
/// violation — as #356 made `volume verify`'s 2 reachable only through a
/// quarantine. A busy catalog keeps [`EXIT_CATALOG_BUSY`].
pub const EXIT_AUDIT_ERROR: i32 = 70;

/// The write family's exit table (ADR-0012, the 2026-10-07 amendment, item
/// 20; issue #408): `volume write`, `volume resume`, `collection run`,
/// `quick-archive`, `volume compact-write` and `volume compact` all exit
///
/// - 0 sealed and confirmed;
/// - 2 an error with the medium untouched — refused before anything was
///   written, or a usage error ([`EXIT_ERROR`]);
/// - 3 confirm inconclusive, as `volume verify`'s 3
///   ([`EXIT_WRITE_CONFIRM_INCONCLUSIVE`]);
/// - 4 interrupted, resumable with `volume resume`
///   ([`EXIT_WRITE_INTERRUPTED`]);
/// - 5 aborted, the session cannot resume ([`EXIT_WRITE_ABORTED`]);
/// - 6 the medium proven bad and quarantined ([`EXIT_WRITE_MEDIUM_BAD`]);
/// - 75 the catalog busy ([`EXIT_CATALOG_BUSY`]).
///
/// Before it, every one of these failures exited 2, and a script driving a
/// write could not tell "load the right tape" from "resume" from "replace
/// the cartridge" without parsing the message.
pub const EXIT_WRITE_CONFIRM_INCONCLUSIVE: i32 = 3;
/// See [`EXIT_WRITE_CONFIRM_INCONCLUSIVE`]: interrupted, resumable.
pub const EXIT_WRITE_INTERRUPTED: i32 = 4;
/// See [`EXIT_WRITE_CONFIRM_INCONCLUSIVE`]: aborted, not resumable.
pub const EXIT_WRITE_ABORTED: i32 = 5;
/// See [`EXIT_WRITE_CONFIRM_INCONCLUSIVE`]: the medium proven bad.
pub const EXIT_WRITE_MEDIUM_BAD: i32 = 6;

/// How a write-family command ended, ordered from best to worst — the
/// order `collection run` reports "the worst outcome among its copies" by
/// (6 > 5 > 4 > 3 > 2 > 0, ADR-0012 2026-10-07 item 20). A busy catalog
/// is not an outcome here: it is decided only when no outcome was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WriteOutcome {
    /// 0: sealed and confirmed.
    Sealed,
    /// 2: an error with the medium untouched.
    Untouched,
    /// 3: the seal is on the tape, the confirm reached no verdict.
    ConfirmInconclusive,
    /// 4: interrupted; `volume resume` continues the session.
    Interrupted,
    /// 5: aborted; the session cannot resume.
    Aborted,
    /// 6: the medium proven bad; the volume is quarantined.
    Quarantined,
}

impl WriteOutcome {
    /// This outcome's exit code.
    pub const fn exit_code(self) -> i32 {
        match self {
            WriteOutcome::Sealed => EXIT_SUCCESS,
            WriteOutcome::Untouched => EXIT_ERROR,
            WriteOutcome::ConfirmInconclusive => EXIT_WRITE_CONFIRM_INCONCLUSIVE,
            WriteOutcome::Interrupted => EXIT_WRITE_INTERRUPTED,
            WriteOutcome::Aborted => EXIT_WRITE_ABORTED,
            WriteOutcome::Quarantined => EXIT_WRITE_MEDIUM_BAD,
        }
    }

    /// The worst of `outcomes` ([`WriteOutcome::Sealed`] for none).
    pub fn worst(outcomes: impl IntoIterator<Item = WriteOutcome>) -> WriteOutcome {
        outcomes.into_iter().max().unwrap_or(WriteOutcome::Sealed)
    }

    /// The outcome an error out of a write-family command stands for: the
    /// worst one any error in its chain is classified as, or `None` when
    /// none is — an error before anything was written.
    pub fn of(err: &anyhow::Error) -> Option<WriteOutcome> {
        err.chain()
            .filter_map(|cause| cause.downcast_ref::<TapectlError>())
            .filter_map(TapectlError::write_outcome)
            .max()
    }
}

/// How a command's ERRORS map to an exit code — decided from the parsed
/// command before it runs, so the errors raised before the command's own
/// module is reached (the database, the config, an uninitialised home) map
/// the same way as its own. `main` asks [`ErrorContract::exit_code`] once,
/// with whatever error the invocation returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorContract {
    /// Every command without a contract of its own: [`EXIT_ERROR`], or
    /// [`EXIT_CATALOG_BUSY`] when the catalog was busy.
    Ordinary,
    /// `volume verify` (issue #356): every error is
    /// [`EXIT_VERIFY_INCONCLUSIVE`], a busy catalog included — "no verdict,
    /// try again" is already what 3 means.
    Verify,
    /// `audit` (issue #408): every error is [`EXIT_AUDIT_ERROR`], except a
    /// busy catalog, which keeps [`EXIT_CATALOG_BUSY`] so the scheduled
    /// wrapper's "retry later, no fail ping" arm still sees it.
    Audit,
    /// The write family (issue #408, [`WriteOutcome`]): an error the write
    /// path classified exits its outcome's code; an unclassified one was
    /// raised before anything was written, so it exits [`EXIT_ERROR`], or
    /// [`EXIT_CATALOG_BUSY`] when the catalog was busy. A usage error keeps
    /// clap's 2, which the table gives it.
    Write,
}

impl ErrorContract {
    /// The exit code for `err`, an error this contract's command returned.
    pub fn exit_code(self, err: &anyhow::Error) -> i32 {
        let busy = || crate::db::busy::is_catalog_busy(err);
        match self {
            ErrorContract::Verify => EXIT_VERIFY_INCONCLUSIVE,
            ErrorContract::Audit if busy() => EXIT_CATALOG_BUSY,
            ErrorContract::Audit => EXIT_AUDIT_ERROR,
            ErrorContract::Write => match WriteOutcome::of(err) {
                Some(outcome) => outcome.exit_code(),
                None if busy() => EXIT_CATALOG_BUSY,
                None => EXIT_ERROR,
            },
            ErrorContract::Ordinary if busy() => EXIT_CATALOG_BUSY,
            ErrorContract::Ordinary => EXIT_ERROR,
        }
    }

    /// The exit code for a command line under this contract that did not
    /// PARSE (clap's usage error), or `None` to keep clap's own (2). A
    /// verify or an audit whose command line is wrong has read nothing and
    /// reached no verdict, so it must not exit with the code its verdict
    /// table gives 2 (issues #356, #408).
    pub fn usage_error_code(self) -> Option<i32> {
        match self {
            ErrorContract::Verify => Some(EXIT_VERIFY_INCONCLUSIVE),
            ErrorContract::Audit => Some(EXIT_AUDIT_ERROR),
            ErrorContract::Ordinary | ErrorContract::Write => None,
        }
    }
}

#[derive(Error, Debug)]
#[allow(dead_code)]
pub enum TapectlError {
    // Database
    /// The SQLite text once, after "database error:". Not `#[from]`: that
    /// would also make the error this variant's `source()`, and `{:#}`
    /// (`exit_with_error`) printed the text twice — the shape `Io` had until
    /// #354 (issue #363). The `From` impl below keeps `?` working.
    #[error("database error: {0}")]
    Database(rusqlite::Error),

    #[error("migration error: {0}")]
    Migration(String),

    /// Issue #377: another tapectl process held the catalog's write lock for
    /// longer than a long writer's retry budget (`db::busy`). Exits
    /// [`EXIT_CATALOG_BUSY`]. The text names what was being recorded, so
    /// the operator knows what did and did not land.
    #[error(
        "catalog busy: {0} — another tapectl command held the database's write lock for too \
         long. Nothing is wrong with the catalog; run the command again once the other one \
         has finished."
    )]
    CatalogBusy(String),

    /// Issue #233: `db::migrate` catches specifically
    /// `rusqlite_migration::Error::ForeignKeyCheck` — never any other
    /// migration failure, and never a match on `e.to_string()` — and
    /// surfaces this instead of the generic [`TapectlError::Migration`]
    /// above. `.foreign_key_check()` (migrations 003/012/013/017) runs
    /// `PRAGMA foreign_key_check` with no table argument, i.e.
    /// whole-database, so a pre-existing orphan ANYWHERE — hand-edited,
    /// partially restored, recovered from a damaged file, or written by an
    /// older tapectl — trips it the instant any pending migration carrying
    /// the check runs. Left as the generic `Migration` variant, this made
    /// `db::open` fail with no path forward: `db fsck --repair`, the one
    /// tool that deletes orphans, could not open the very database that
    /// needed it (chicken and egg). `db::open_for_repair` is the way in —
    /// it never calls `migrate()`, so it is unaffected by this check.
    #[error(
        "database has foreign-key violations that block migration: {0}\n\
         run `tapectl db fsck --repair` to remove the orphaned rows, then retry the command \
         that failed."
    )]
    DatabaseNeedsRepair(String),

    // Configuration
    #[error("configuration error: {0}")]
    Config(String),

    #[error("configuration file not found: {0}")]
    ConfigNotFound(String),

    // Tenant
    #[error("tenant not found: {0}")]
    TenantNotFound(String),

    #[error("tenant already exists: {0}")]
    TenantAlreadyExists(String),

    #[error("cannot delete tenant with active units")]
    TenantHasActiveUnits,

    // Key management
    #[error("key not found: {0}")]
    KeyNotFound(String),

    #[error("key already exists: {0}")]
    KeyAlreadyExists(String),

    #[error("encryption error: {0}")]
    Encryption(String),

    // Unit
    #[error("unit not found: {0}")]
    UnitNotFound(String),

    #[error("unit already exists: {0}")]
    UnitAlreadyExists(String),

    #[error("nested unit detected: {0}")]
    NestedUnit(String),

    #[error("unit path does not exist: {0}")]
    UnitPathNotFound(String),

    // dar
    #[error("dar error: {0}")]
    Dar(String),

    #[error("dar not found at configured path: {0}")]
    DarNotFound(String),

    #[error("dar version {found} below minimum {minimum}")]
    DarVersionTooOld { found: String, minimum: String },

    // Volume / Tape
    #[error("volume not found: {0}")]
    VolumeNotFound(String),

    /// ADR-0012: `volume write`/`volume resume` refuse any volume whose
    /// catalog status is not `initialized` (`policy::coverage::is_write_target`).
    /// Not a Tier-2 risk judgement — `--force` never consults this, because
    /// there is nothing to override: it is a fact about the catalog row, and
    /// a sealed/retired/erased/quarantined volume is never a write target
    /// regardless of what tape happens to be loaded.
    ///
    /// This is the STATUS half of the write-target check only. The other
    /// half — a volume whose status still reads `initialized` but which
    /// already has a completed write recorded (`policy::coverage::
    /// has_completed_write`, ADR-0012's 2026-09-16 amendment, issue #199)
    /// — is deliberately a DIFFERENT variant ([`TapectlError::
    /// VolumeHasRecordedWrite`]): this variant's message asserts the
    /// status itself is the problem, which would be actively misleading
    /// for that case (the status genuinely IS `initialized`).
    #[error(
        "volume \"{label}\" is {status} and is not a write target (ADR-0012): only a volume \
         that `volume init` left `initialized` can be written, and a sealed volume is never \
         written again (ADR-0003). `--force` does not apply — this is a fact about the catalog \
         row, not a risk judgement. To write this cartridge again, run `tapectl volume init \
         <new-label>` on it; the File 0 check is the consent point (ADR-0010)."
    )]
    VolumeNotWriteTarget { label: String, status: String },

    /// ADR-0012, amendment 2026-09-16 (issue #199): the sibling of
    /// [`TapectlError::VolumeNotWriteTarget`] for a volume whose
    /// `volumes.status` is `initialized` but which already has a
    /// *completed* write recorded (`policy::coverage::
    /// has_completed_write`) — most often `catalog rebuild --from-volume`
    /// attaching a rebuilt tape's contents to a stale `initialized` row
    /// without ever touching its status (#158 deliberately leaves an
    /// existing row's status alone). Also not a Tier-2 judgement and not
    /// `--force`-overridable, for the same reason as its sibling: this is
    /// a fact about the catalog row, not a risk call.
    #[error(
        "volume \"{label}\" already has a completed write recorded and is not a write target \
         (ADR-0012): its catalog row still reads `initialized`, but the `writes` table shows \
         bytes were already written to it — most likely `catalog rebuild --from-volume` \
         attached a rebuilt tape's contents to this row. `--force` does not apply. If that \
         tape is this volume, sealed by a write this catalog lost (it was restored from a \
         backup taken before the write), a full `tapectl volume verify {label}` and then \
         `tapectl volume resume {label}` adopt it as sealed (ADR-0012, 2026-09-29 later). To \
         write a real blank cartridge, run `tapectl volume init <new-label>` on it; the File 0 \
         check is the consent point (ADR-0010)."
    )]
    VolumeHasRecordedWrite { label: String },

    /// ADR-0012's 2026-09-17 amendment ("the status column is the
    /// operator's; a medium's condition is its own fact", issue #242): the
    /// THIRD half of the write-target check, alongside
    /// [`TapectlError::VolumeNotWriteTarget`]'s status test and
    /// [`TapectlError::VolumeHasRecordedWrite`]'s attached-write test.
    ///
    /// A separate variant rather than folding this into
    /// `VolumeNotWriteTarget` for the same reason `VolumeHasRecordedWrite`
    /// is its own variant: that variant's message asserts the STATUS is the
    /// problem ("volume X is {status} and is not a write target"), which
    /// would be actively misleading here — a volume a write-time contact
    /// check quarantined mid-session still reads `status = 'initialized'`
    /// (it never sealed), so the status genuinely IS the one value that
    /// admits a write. `observed_condition` is what refuses it. Not a
    /// Tier-2 risk judgement and not `--force`-overridable, for the same
    /// reason as its siblings: this is a fact the catalog recorded, not a
    /// risk call.
    #[error(
        "volume \"{label}\" is quarantined (ADR-0012, the 2026-09-17 amendment) and is not a \
         write target: a prior contact check found evidence this medium cannot be trusted, and \
         recorded it in `observed_condition` rather than `status`. `--force` does not apply — \
         this is a fact the catalog recorded, not a risk judgement. To write this cartridge \
         again, run `tapectl volume init <new-label>` on it; the File 0 check is the consent \
         point (ADR-0010)."
    )]
    VolumeQuarantined { label: String },

    /// Issue #376: another process holds this volume's session lock
    /// (`staging::lock::acquire_volume`) — a `volume init`, `write`,
    /// `resume` (through its confirm) or `verify` is running on it right
    /// now. A fact the kernel reports, not a risk judgement: ADR-0008 Tier
    /// 3, and neither `--force` nor `--yes` crosses it.
    #[error(
        "volume \"{label}\" has a live session: another tapectl process is running `volume \
         init`, `write`, `resume` or `verify` on it right now (it holds the volume's lock). \
         Refusing — acting on a live session would cut it out from under its writer. \
         `--force` does not apply. Wait for that command to finish; if it was killed, the \
         kernel has already released its lock and this refusal will not recur."
    )]
    VolumeSessionLive { label: String },

    #[error("tape I/O error: {0}")]
    TapeIo(String),

    /// Issue #408: the medium is full — the drive refused a write with
    /// ENOSPC (its early-warning point). The one store failure a write
    /// session ABORTS on (ADR-0007: no salvage, a clean abort to an
    /// unsealed tape); every other failure leaves it resumable.
    #[error("tape full: {0}")]
    MediumFull(String),

    /// Issue #408: reading a STAGED file (the source of a tape write)
    /// failed — the disk, not the tape. Named apart from [`Self::TapeIo`],
    /// which it used to be reported as, so the operator looks at the right
    /// device.
    #[error("staged source read error: {0}")]
    SourceIo(String),

    /// The drive reported no cartridge in place (`GMT_DR_OPEN`), answered by
    /// the non-blocking probe every tape-touching command runs before its
    /// first blocking open (issue #152 for `volume init`, issue #355 for the
    /// rest). Its own variant, not a `TapeIo`: nothing was read or written,
    /// the fix is to load a cartridge, and `volume verify` maps it — like
    /// every verify that reached no verdict — to its "inconclusive" exit
    /// code rather than to the one that means the medium is bad (issue #356).
    ///
    /// The text is the one `volume init` has printed since #152; keep it.
    #[error("no cartridge loaded in {device}")]
    NoCartridgeLoaded { device: String },

    // The write family's classified failures (ADR-0012, 2026-10-07 item 20;
    // issue #408). Each is raised only once the write path KNOWS which
    // outcome it is, and carries the full operator text; [`WriteOutcome`]
    // and [`ErrorContract::Write`] read the variant for the exit code. Any
    // error a write path raises without one of these was raised before
    // anything was written.
    /// Exit 4: the session stopped — a signal, or a drive, staging or
    /// catalog error mid-session — and `volume resume` continues it.
    #[error("{0}")]
    WriteInterrupted(String),

    /// Exit 5: the session aborted and cannot resume (the medium ran out,
    /// a staged slice changed under the write).
    #[error("{0}")]
    WriteAborted(String),

    /// Exit 6: the medium was proven bad and the volume quarantined.
    #[error("{0}")]
    WriteQuarantined(String),

    /// Exit 3: the seal is on the tape and the confirm reached no verdict;
    /// `volume resume` re-enters it.
    #[error("{0}")]
    ConfirmInconclusive(String),

    // General
    #[error("not initialized — run `tapectl init` first")]
    NotInitialized,

    #[error("already initialized at {0}")]
    AlreadyInitialized(String),

    /// A signal (SIGINT, SIGTERM or SIGHUP) asked the run to stop, and it
    /// stopped at a point it can be resumed or re-run from (issue #404).
    /// The text says where it stopped and what to run next
    /// (`crate::signal::check`).
    #[error("stopped by a signal: {0}")]
    Interrupted(String),

    /// `transparent`, not `"{0}"`: with `#[from]`, a `"{0}"` Display made the
    /// io error BOTH this variant's text and its `source()`, so the
    /// operator's `{:#}` rendering (`exit_with_error`) printed it twice —
    /// `Permission denied (os error 13): Permission denied (os error 13)`
    /// (issue #354). `transparent` forwards Display and `source()` to the io
    /// error itself: its text once, then only a cause it genuinely carries.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// A layer of the 3-level policy chain could not be resolved (issue
    /// #114). Carries WHICH layer, because the remedy differs completely —
    /// editing a dotfile does not fix a dangling `archive_set_id` — and the
    /// error text alone cannot be branched on.
    #[error("{detail}")]
    PolicyUnresolvable { layer: PolicyLayer, detail: String },

    #[error("{0}")]
    Other(String),
}

impl TapectlError {
    /// The write-family outcome this error was classified as, if any
    /// (issue #408).
    pub fn write_outcome(&self) -> Option<WriteOutcome> {
        match self {
            TapectlError::WriteInterrupted(_) => Some(WriteOutcome::Interrupted),
            TapectlError::WriteAborted(_) => Some(WriteOutcome::Aborted),
            TapectlError::WriteQuarantined(_) => Some(WriteOutcome::Quarantined),
            TapectlError::ConfirmInconclusive(_) => Some(WriteOutcome::ConfirmInconclusive),
            _ => None,
        }
    }

    /// An error raised once volume `label`'s write session holds state the
    /// tape and the catalog share — its `writes` rows are planned, or it is
    /// writing — and that nothing classified more precisely (issue #408).
    /// The session is resumable: rows left `in_progress` are swept to
    /// `interrupted` on the next open. So it is [`Self::WriteInterrupted`],
    /// with its text kept and the resume named; an error already classified
    /// is returned unchanged.
    pub fn in_write_session(self, label: &str) -> TapectlError {
        if self.write_outcome().is_some() {
            return self;
        }
        TapectlError::WriteInterrupted(format!(
            "{self} — volume \"{label}\"'s write session stopped part-way and is left \
             resumable: fix the cause, reload the same cartridge and run `tapectl volume \
             resume {label}`"
        ))
    }
}

impl From<rusqlite::Error> for TapectlError {
    fn from(e: rusqlite::Error) -> Self {
        TapectlError::Database(e)
    }
}

/// Which layer of `policy::resolve`'s dotfile > archive_set > defaults chain
/// failed (issue #114).
///
/// Exists so a caller can give the operator an action that is actually
/// correct. `audit` previously told them to "fix the [policy] section in
/// <unit>/.tapectl-unit.toml" no matter which layer broke — advice that
/// sends someone to edit a file that is not the problem when the real fault
/// is in the catalog or in `config.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyLayer {
    /// `config.toml`'s `[defaults]` — a bad value there affects every unit.
    Defaults,
    /// The unit's `archive_sets` row: missing, or holding unparseable data.
    ArchiveSet,
    /// The unit's own `.tapectl-unit.toml`.
    Dotfile,
}

impl PolicyLayer {
    /// The concrete thing an operator should go and fix.
    ///
    /// Takes the unit path because only the dotfile remedy is unit-scoped;
    /// the other two point at shared state, which is itself worth conveying —
    /// a `[defaults]` fault is not one unit's problem.
    pub fn remedy(&self, unit_path: Option<&str>) -> String {
        match self {
            PolicyLayer::Defaults => {
                "fix the [defaults] section in ~/.tapectl/config.toml (it affects every unit)"
                    .to_string()
            }
            PolicyLayer::ArchiveSet => {
                "fix the unit's archive set — `tapectl archive-set list` to find it, \
                 `tapectl archive-set edit <name>` to correct it, or re-point the unit \
                 with `tapectl unit init --archive-set <name>`"
                    .to_string()
            }
            PolicyLayer::Dotfile => format!(
                "fix {}/.tapectl-unit.toml",
                unit_path.unwrap_or("<unit path>")
            ),
        }
    }
}

/// Convenience type alias for collection results.
pub type Result<T> = std::result::Result<T, TapectlError>;

/// Exit the process with the appropriate code for the given error.
pub fn exit_with_error(err: &anyhow::Error) -> ! {
    exit_with_error_code(err, EXIT_ERROR)
}

/// [`exit_with_error`] with the code chosen by the caller — `volume verify`
/// exits [`EXIT_VERIFY_INCONCLUSIVE`] on every error, because under its exit
/// contract 2 means "the medium is proven bad" (issue #356). The message is
/// printed exactly as [`exit_with_error`] prints it.
pub fn exit_with_error_code(err: &anyhow::Error, code: i32) -> ! {
    eprintln!("error: {err:#}");
    process::exit(code);
}

/// The warning a write-family command prints, and then exits 0 on, when a
/// step AFTER a copy was sealed and confirmed fails (ADR-0012 2026-10-07
/// item 33): the copy stands, so the failure is not a write outcome and the
/// command does not exit 2. It names the copy, the `step` that did not
/// finish and why, and `finish` — the command that finishes that step on
/// its own — or says that none does. One wording for every such step:
/// `volume compact`'s retirement of the source, `collection run`'s release
/// of staging, and recording what followed a sealed copy.
pub fn unfinished_after_seal(
    volume: &str,
    step: &str,
    err: &dyn std::fmt::Display,
    finish: Option<&str>,
) -> String {
    let finish = match finish {
        Some(cmd) => format!("To finish it: {cmd}"),
        None => "No tapectl command finishes it afterwards.".to_string(),
    };
    format!(
        "warning: volume \"{volume}\" is sealed and confirmed and counts as a copy, but {step} \
         did not finish: {err}. The tape needs nothing. {finish}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-0012 2026-10-07 item 33: the warning names the copy that stands,
    /// the step and why, and the command that finishes it.
    #[test]
    fn the_post_seal_warning_names_the_step_and_the_command_that_finishes_it() {
        let w = unfinished_after_seal(
            "L6-0002",
            "releasing the batch's staging",
            &"database is locked",
            Some("tapectl staging clean"),
        );
        assert_eq!(
            w,
            "warning: volume \"L6-0002\" is sealed and confirmed and counts as a copy, but \
             releasing the batch's staging did not finish: database is locked. The tape needs \
             nothing. To finish it: tapectl staging clean"
        );
        let w = unfinished_after_seal("L6-0002", "recording", &"x", None);
        assert!(
            w.ends_with("No tapectl command finishes it afterwards."),
            "{w}"
        );
    }

    /// Render an error exactly as [`exit_with_error`] prints it: `{:#}` on
    /// the anyhow error, which walks the `source()` chain. `to_string()`
    /// alone is the outer Display only and never shows a doubled chain.
    fn as_operator_sees_it(err: TapectlError) -> String {
        format!("{:#}", anyhow::Error::from(err))
    }

    /// Issue #354 (a), systemic: every `std::io::Error` that reached the
    /// operator through `?` printed twice — `Permission denied (os error
    /// 13): Permission denied (os error 13)` — because the variant's Display
    /// IS the io error's text and `#[from]` also made that io error its
    /// `source()`, so `{:#}` printed it again as the cause.
    #[test]
    fn an_io_error_reaches_the_operator_once() {
        let err: TapectlError = std::io::Error::from_raw_os_error(13).into();
        assert_eq!(as_operator_sees_it(err), "Permission denied (os error 13)");
    }

    /// The same for an io error carrying its own message (what
    /// `io::Error::other` and friends build), not only a raw errno.
    #[test]
    fn a_custom_io_error_reaches_the_operator_once() {
        let err: TapectlError = std::io::Error::other("staging disk went away").into();
        assert_eq!(as_operator_sees_it(err), "staging disk went away");
    }

    /// Issue #408 (ADR-0012, 2026-10-07 item 19): each contract's errors,
    /// busy and not. `audit`'s errors exit 70 so 2 is only ever a
    /// violation, but a busy catalog keeps 75 there (the scheduled wrapper's
    /// "retry later" arm); `volume verify` folds both into its 3.
    #[test]
    fn each_error_contract_maps_an_error_and_a_busy_catalog() {
        let plain = anyhow::Error::from(TapectlError::UnitNotFound("u".into()));
        let busy = anyhow::Error::from(TapectlError::CatalogBusy("the seal".into()));
        let cases = [
            (ErrorContract::Ordinary, EXIT_ERROR, EXIT_CATALOG_BUSY),
            (ErrorContract::Audit, EXIT_AUDIT_ERROR, EXIT_CATALOG_BUSY),
            (
                ErrorContract::Verify,
                EXIT_VERIFY_INCONCLUSIVE,
                EXIT_VERIFY_INCONCLUSIVE,
            ),
        ];
        for (contract, on_error, on_busy) in cases {
            assert_eq!(contract.exit_code(&plain), on_error, "{contract:?}");
            assert_eq!(contract.exit_code(&busy), on_busy, "{contract:?} busy");
        }
        assert_eq!(EXIT_AUDIT_ERROR, 70, "sysexits' EX_SOFTWARE");
    }

    /// ADR-0012, 2026-10-07 item 20 (issue #408): the write family's table,
    /// code by code, through the contract `main` asks — including through
    /// an `anyhow` context, which is how a caller's wrapping arrives.
    #[test]
    fn the_write_contract_maps_each_classified_outcome_to_its_code() {
        let code = |e: TapectlError| ErrorContract::Write.exit_code(&anyhow::Error::from(e));
        assert_eq!(code(TapectlError::VolumeNotFound("L".into())), 2);
        assert_eq!(code(TapectlError::Interrupted("before the tape".into())), 2);
        assert_eq!(code(TapectlError::ConfirmInconclusive("c".into())), 3);
        assert_eq!(code(TapectlError::WriteInterrupted("i".into())), 4);
        assert_eq!(code(TapectlError::WriteAborted("a".into())), 5);
        assert_eq!(code(TapectlError::WriteQuarantined("q".into())), 6);
        assert_eq!(code(TapectlError::CatalogBusy("plan".into())), 75);

        let wrapped =
            anyhow::Error::from(TapectlError::WriteAborted("a".into())).context("quick-archive");
        assert_eq!(ErrorContract::Write.exit_code(&wrapped), 5);
        assert_eq!(ErrorContract::Write.usage_error_code(), None, "clap's 2");
        assert_eq!(
            ErrorContract::Ordinary
                .exit_code(&anyhow::Error::from(TapectlError::WriteAborted("a".into()))),
            EXIT_ERROR,
            "only the write family reads the outcome"
        );
    }

    /// `collection run` reports the worst outcome among its copies:
    /// 6 > 5 > 4 > 3 > 2 > 0.
    #[test]
    fn the_worst_write_outcome_follows_the_ruled_order() {
        use WriteOutcome::*;
        let order = [
            Sealed,
            Untouched,
            ConfirmInconclusive,
            Interrupted,
            Aborted,
            Quarantined,
        ];
        assert_eq!(
            order.map(WriteOutcome::exit_code),
            [0, 2, 3, 4, 5, 6],
            "each outcome's code"
        );
        for (i, &better) in order.iter().enumerate() {
            for &worse in &order[i..] {
                assert_eq!(WriteOutcome::worst([better, worse]), worse);
                assert_eq!(WriteOutcome::worst([worse, better]), worse);
            }
        }
        assert_eq!(WriteOutcome::worst([]), Sealed);
    }

    /// An error raised once the session holds shared state is resumable
    /// unless it was classified already; the text is kept and the resume
    /// named.
    #[test]
    fn an_unclassified_error_in_a_write_session_is_resumable() {
        let e = TapectlError::TapeIo("seal write failed".into()).in_write_session("L6-1");
        assert!(matches!(e, TapectlError::WriteInterrupted(_)), "{e:?}");
        let text = e.to_string();
        assert!(
            text.starts_with("tape I/O error: seal write failed"),
            "{text}"
        );
        assert!(text.contains("tapectl volume resume L6-1"), "{text}");

        for kept in [
            TapectlError::WriteAborted("a".into()),
            TapectlError::WriteQuarantined("q".into()),
            TapectlError::ConfirmInconclusive("c".into()),
            TapectlError::WriteInterrupted("i".into()),
        ] {
            let before = kept.write_outcome();
            let after = kept.in_write_session("L6-1");
            assert_eq!(after.write_outcome(), before);
            assert_eq!(after.to_string().len(), 1, "text unchanged");
        }
    }

    /// A command line that does not parse: the two verdict commands take
    /// their "no verdict" code, everything else keeps clap's.
    #[test]
    fn usage_errors_keep_clap_s_code_except_for_the_verdict_commands() {
        assert_eq!(
            ErrorContract::Verify.usage_error_code(),
            Some(EXIT_VERIFY_INCONCLUSIVE)
        );
        assert_eq!(
            ErrorContract::Audit.usage_error_code(),
            Some(EXIT_AUDIT_ERROR)
        );
        assert_eq!(ErrorContract::Ordinary.usage_error_code(), None);
    }

    /// Issue #363: `Database` had the shape `Io` had before #354 — the
    /// SQLite text in its own Display AND as its `source()` — so `{:#}`
    /// printed it twice. Once, with the "database error:" prefix kept.
    #[test]
    fn a_database_error_reaches_the_operator_once() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let sqlite = conn
            .execute("INSERT INTO no_such_table VALUES (1)", [])
            .unwrap_err();
        let text = sqlite.to_string();
        assert!(text.contains("no_such_table"), "{text}");
        let err: TapectlError = sqlite.into();
        let shown = as_operator_sees_it(err);
        assert_eq!(shown, format!("database error: {text}"));
        assert_eq!(shown.matches("no_such_table").count(), 1, "{shown}");
    }
}
