use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Result, TapectlError};

/// Default tapectl home directory.
pub fn default_home() -> PathBuf {
    dirs_home().join(".tapectl")
}

fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/root"))
}

/// Root configuration — maps to ~/.tapectl/config.toml.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub dar: DarConfig,

    #[serde(default)]
    pub backends: BackendsConfig,

    #[serde(default)]
    pub archive_sets: Vec<ArchiveSetConfig>,

    #[serde(default)]
    pub defaults: DefaultsConfig,

    #[serde(default)]
    pub staging: StagingConfig,

    #[serde(default)]
    pub discovery: DiscoveryConfig,

    #[serde(default)]
    pub libraries: Vec<LibraryConfig>,

    #[serde(default)]
    pub packing: PackingConfig,

    #[serde(default)]
    pub compaction: CompactionConfig,

    #[serde(default)]
    pub labels: LabelsConfig,

    #[serde(default)]
    pub logging: LoggingConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DarConfig {
    #[serde(default = "default_dar_binary")]
    pub binary: String,
}

fn default_dar_binary() -> String {
    "/opt/dar/bin/dar".to_string()
}

impl Default for DarConfig {
    fn default() -> Self {
        Self {
            binary: default_dar_binary(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BackendsConfig {
    #[serde(default)]
    pub lto: Vec<LtoBackendConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LtoBackendConfig {
    pub name: String,
    pub device_tape: String,
    pub device_sg: String,
    pub media_type: String,
    pub nominal_capacity: String,
    #[serde(default = "default_usable_capacity_factor")]
    pub usable_capacity_factor: f64,
    #[serde(default = "default_enospc_buffer")]
    pub enospc_buffer: String,
    #[serde(default = "default_block_size")]
    pub block_size: String,
    #[serde(default)]
    pub hardware_compression: bool,
}

fn default_usable_capacity_factor() -> f64 {
    0.92
}
fn default_enospc_buffer() -> String {
    "50M".to_string()
}
fn default_block_size() -> String {
    "1M".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveSetConfig {
    pub name: String,
    pub min_copies: Option<i32>,
    pub required_locations: Option<Vec<String>>,
    pub encrypt: Option<bool>,
    pub compression: Option<String>,
    pub checksum_mode: Option<String>,
    pub verify_interval_days: Option<i32>,
    pub slice_size: Option<String>,
    pub preserve_xattrs: Option<bool>,
    pub preserve_acls: Option<bool>,
    pub preserve_fsa: Option<bool>,
    pub dirty_on_metadata_change: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DefaultsConfig {
    #[serde(default = "default_slice_size")]
    pub slice_size: String,
    #[serde(default = "default_compression")]
    pub compression: String,
    #[serde(default = "default_hash")]
    pub hash: String,
    #[serde(default = "default_checksum_mode")]
    pub checksum_mode: String,
    #[serde(default = "default_true")]
    pub encrypt: bool,
    #[serde(default = "default_true")]
    pub preserve_xattrs: bool,
    #[serde(default = "default_true")]
    pub preserve_acls: bool,
    #[serde(default = "default_true")]
    pub preserve_fsa: bool,
    #[serde(default)]
    pub dirty_on_metadata_change: bool,
    #[serde(default)]
    pub global_excludes: Vec<String>,
    #[serde(default = "default_large_file_warn")]
    pub large_file_warn_threshold: String,
    #[serde(default = "default_min_copies")]
    pub min_copies_for_tape_only: i32,
    #[serde(default = "default_min_locations")]
    pub min_locations_for_tape_only: i32,
}

fn default_slice_size() -> String {
    // Ratified 2026-07-22 (docs/design/v2-open-questions.md §1.3). dar -s is an
    // exact per-slice cut and a per-unit max; 10G ≈ 250 slices per LTO-6 tape,
    // ~1 min retry quantum, ≤5G expected loss per damage event, zero padding on
    // full slices (multiple of the 512 KB block). The old 2400G default was a
    // whole-tape slice: maximal blast radius and guaranteed OOM under the
    // pre-#35 buffering glue. Per-class overrides ride the policy chain (#35).
    "10G".to_string()
}
fn default_compression() -> String {
    "none".to_string()
}
fn default_hash() -> String {
    "sha256".to_string()
}
fn default_checksum_mode() -> String {
    "mtime_size".to_string()
}
fn default_true() -> bool {
    true
}
fn default_large_file_warn() -> String {
    "100G".to_string()
}
fn default_min_copies() -> i32 {
    2
}
fn default_min_locations() -> i32 {
    2
}

impl Default for DefaultsConfig {
    fn default() -> Self {
        Self {
            slice_size: default_slice_size(),
            compression: default_compression(),
            hash: default_hash(),
            checksum_mode: default_checksum_mode(),
            encrypt: true,
            preserve_xattrs: true,
            preserve_acls: true,
            preserve_fsa: true,
            dirty_on_metadata_change: false,
            global_excludes: vec![
                "*.nfo".into(),
                "Thumbs.db".into(),
                ".DS_Store".into(),
                "*.tmp".into(),
            ],
            large_file_warn_threshold: default_large_file_warn(),
            min_copies_for_tape_only: default_min_copies(),
            min_locations_for_tape_only: default_min_locations(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StagingConfig {
    #[serde(default = "default_staging_dir")]
    pub directory: String,
}

fn default_staging_dir() -> String {
    "/mnt/staging".to_string()
}

impl Default for StagingConfig {
    fn default() -> Self {
        Self {
            directory: default_staging_dir(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiscoveryConfig {
    #[serde(default)]
    pub watch_roots: Vec<String>,
}

/// One media-library root (`docs/design/v2-open-questions.md` §11): a
/// folder=unit factory over existing unit machinery, batch-synced and
/// batch-written instead of ceremonially `unit init`'d one at a time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryConfig {
    /// Library name — also the unit-name prefix library sync assigns
    /// (`"{name}/{relative_path}"`), so units stay unique across libraries.
    pub name: String,
    /// Root directory to walk.
    pub root: String,
    /// Tenant new units are registered under.
    pub tenant: String,
    /// Depth at which child folders become atomic units (1 = immediate
    /// children; 2 = grandchildren, e.g. show/season shapes).
    #[serde(default = "default_unit_depth")]
    pub unit_depth: usize,
    /// Walk-level excludes (glob, matched against the unit folder's own
    /// basename) — on top of `defaults.global_excludes`, which apply inside
    /// units at stage time. E.g. `"*.partial"` to skip an in-flight copy.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Policy binding (slice size, min_copies, …) for units this library
    /// registers. `None` falls through to system defaults, same as manual
    /// `unit init` without `--archive-set`.
    #[serde(default)]
    pub archive_set: Option<String>,
    /// `true` (default): register new units with a `.tapectl-unit.toml`
    /// (uuid identity, rename-proof). `false`: path-keyed identity for
    /// read-only sources that can't be written to — trades away rename
    /// robustness for zero on-disk footprint.
    #[serde(default = "default_true")]
    pub dotfiles: bool,
}

fn default_unit_depth() -> usize {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackingConfig {
    #[serde(default = "default_packing_strategy")]
    pub strategy: String,
    #[serde(default = "default_fill_threshold")]
    pub fill_threshold: f64,
    #[serde(default = "default_min_free_for_append")]
    pub min_free_for_append: String,
}

fn default_packing_strategy() -> String {
    "best_fit_decreasing".to_string()
}
fn default_fill_threshold() -> f64 {
    0.95
}
fn default_min_free_for_append() -> String {
    "50G".to_string()
}

impl Default for PackingConfig {
    fn default() -> Self {
        Self {
            strategy: default_packing_strategy(),
            fill_threshold: default_fill_threshold(),
            min_free_for_append: default_min_free_for_append(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionConfig {
    #[serde(default = "default_utilization_threshold")]
    pub utilization_threshold: f64,
    #[serde(default = "default_tape_only_safety")]
    pub tape_only_safety_multiplier: i32,
}

fn default_utilization_threshold() -> f64 {
    0.50
}
fn default_tape_only_safety() -> i32 {
    2
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            utilization_threshold: default_utilization_threshold(),
            tape_only_safety_multiplier: default_tape_only_safety(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LabelsConfig {
    #[serde(default = "default_label_format")]
    pub format: String,
}

fn default_label_format() -> String {
    "L{gen}-{seq:04}".to_string()
}

impl Default for LabelsConfig {
    fn default() -> Self {
        Self {
            format: default_label_format(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    #[serde(default = "default_log_level")]
    pub level: String,
    #[serde(default = "default_log_format")]
    pub format: String,
}

fn default_log_level() -> String {
    "info".to_string()
}
fn default_log_format() -> String {
    "json".to_string()
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
            format: default_log_format(),
        }
    }
}

impl Config {
    /// Load config from file, falling back to defaults.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Err(TapectlError::ConfigNotFound(path.display().to_string()));
        }
        let content = std::fs::read_to_string(path)?;
        toml::from_str(&content).map_err(|e| TapectlError::Config(e.to_string()))
    }

    /// Load config or use defaults if the file doesn't exist yet (for `init`).
    #[allow(dead_code)]
    pub fn load_or_default(path: &Path) -> Self {
        if path.exists() {
            Self::load(path).unwrap_or_default()
        } else {
            Self::default()
        }
    }

    /// Write config to file.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content =
            toml::to_string_pretty(self).map_err(|e| TapectlError::Config(e.to_string()))?;
        std::fs::write(path, content)?;
        Ok(())
    }
}

/// Resolved paths for the tapectl home directory.
#[derive(Debug, Clone)]
pub struct TapectlPaths {
    pub home: PathBuf,
    pub config_file: PathBuf,
    pub db_file: PathBuf,
    pub keys_dir: PathBuf,
    pub catalogs_dir: PathBuf,
    pub receipts_dir: PathBuf,
    pub logs_dir: PathBuf,
}

impl TapectlPaths {
    pub fn new(home: PathBuf) -> Self {
        Self {
            config_file: home.join("config.toml"),
            db_file: home.join("tapectl.db"),
            keys_dir: home.join("keys"),
            catalogs_dir: home.join("catalogs"),
            receipts_dir: home.join("receipts"),
            logs_dir: home.join("logs"),
            home,
        }
    }

    pub fn default_paths() -> Self {
        Self::new(default_home())
    }

    /// Create all directories if they don't exist.
    pub fn ensure_dirs(&self) -> Result<()> {
        for dir in [
            &self.home,
            &self.keys_dir,
            &self.catalogs_dir,
            &self.receipts_dir,
            &self.logs_dir,
        ] {
            std::fs::create_dir_all(dir)?;
        }
        Ok(())
    }

    /// Check if tapectl has been initialized (DB exists).
    pub fn is_initialized(&self) -> bool {
        self.db_file.exists()
    }
}
