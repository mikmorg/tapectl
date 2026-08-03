use std::process;

use thiserror::Error;

/// Exit codes per design: 0=success, 1=warnings, 2=errors/violations.
///
/// Mirrors the convention `audit` already established (`src/cli/audit.rs`):
/// 0=clean, 1=warning, 2=violation. `volume verify` (src/cli/volume.rs) and
/// `db fsck` (src/main.rs) both compute their exit code against these same
/// constants — see `verify_exit_code`/`fsck_exit_code` (issue #45/H10).
pub const EXIT_SUCCESS: i32 = 0;
pub const EXIT_WARNING: i32 = 1;
pub const EXIT_ERROR: i32 = 2;

#[derive(Error, Debug)]
#[allow(dead_code)]
pub enum TapectlError {
    // Database
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("migration error: {0}")]
    Migration(String),

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

    #[error("tape I/O error: {0}")]
    TapeIo(String),

    // General
    #[error("not initialized — run `tapectl init` first")]
    NotInitialized,

    #[error("already initialized at {0}")]
    AlreadyInitialized(String),

    #[error("operation interrupted")]
    Interrupted,

    #[error("{0}")]
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
    eprintln!("error: {err:#}");
    process::exit(EXIT_ERROR);
}
