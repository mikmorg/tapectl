use std::process;

use thiserror::Error;

/// Exit codes per design: 0=success, 1=warnings, 2=errors/violations.
///
/// Mirrors the convention `audit` already established (`src/cli/audit.rs`):
/// 0=clean, 1=warning, 2=violation. `db fsck` (src/main.rs) computes its exit
/// code against these same constants — see `fsck_exit_code` (issue #45/H10).
/// `volume verify` has its own three, below (issue #356).
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
         attached a rebuilt tape's contents to this row. `--force` does not apply. To write a \
         real blank cartridge, run `tapectl volume init <new-label>` on it; the File 0 check is \
         the consent point (ADR-0010)."
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

#[cfg(test)]
mod tests {
    use super::*;

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
