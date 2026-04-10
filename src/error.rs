use std::process;

use thiserror::Error;

/// Exit codes per design: 0=success, 1=warnings, 2=errors/violations.
pub const EXIT_SUCCESS: i32 = 0;
pub const EXIT_WARNING: i32 = 1;
pub const EXIT_ERROR: i32 = 2;

#[derive(Error, Debug)]
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

    #[error("{0}")]
    Other(String),
}

/// Convenience type alias for library results.
pub type Result<T> = std::result::Result<T, TapectlError>;

/// Exit the process with the appropriate code for the given error.
pub fn exit_with_error(err: &anyhow::Error) -> ! {
    eprintln!("error: {err:#}");
    process::exit(EXIT_ERROR);
}
