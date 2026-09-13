use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Result, TapectlError};

/// Best-effort tighten `path` to `mode`. Never fails the caller — a chmod
/// that can't be applied (non-Unix filesystem, path not owned by this
/// process, a backup destination on removable media, etc.) only logs and
/// moves on rather than aborting an otherwise-fine command. Used everywhere
/// under `~/.tapectl` — and on operator-chosen backup destinations — that
/// used to get whatever the process umask handed out (issue #41/#40).
pub fn secure_path(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)) {
        tracing::warn!(
            path = %path.display(),
            mode = format!("{mode:o}"),
            error = %e,
            "could not set restrictive permissions; leaving as-is"
        );
    }
}

/// Write `contents` to `path`, created with `mode` from the very first
/// `open()` call — no umask-derived default in between. Mirrors
/// `crypto::keys::save_secret_key`'s pattern. Unlike `secure_path`, a
/// failure here IS propagated: this is for brand-new files this process is
/// creating itself under its own home directory, where failure indicates a
/// real problem (e.g. disk full) worth surfacing, not a foreign-owned or
/// removable-media destination we should tolerate not being able to touch.
pub fn write_private_file(path: &Path, contents: &[u8], mode: u32) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(path)?;
    file.write_all(contents)?;
    // `.mode()` on `OpenOptions` only applies when `O_CREAT` actually
    // creates a new inode. If `path` already existed (e.g. a receipt
    // regenerated on a re-run), `open()` reuses the existing inode and its
    // existing permission bits survive untouched — so force the mode
    // explicitly too, otherwise a stale, looser mode from a prior run can
    // outlive a rewrite.
    file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

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
    pub collections: Vec<CollectionConfig>,

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
    // Bare and PATH-resolved (issue #124) -- portable across distros that
    // install dar to /usr/bin, /usr/local/bin, or elsewhere, and honored by
    // `config check` (issue #119) exactly like the runtime honors it via
    // `Command::new`.
    "dar".to_string()
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
    // Skipped when empty so a fresh `init` does not write a bare `lto = []`.
    // That stub is not harmless: TOML rejects a later `[[backends.lto]]` table
    // as a duplicate key, so `backend add` (#126) could not append to the very
    // file `init` had just written. Deserialization is unaffected — the
    // `default` covers an absent key, which is what "no drives yet" means.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lto: Vec<LtoBackendConfig>,
}

/// A configured LTO drive.
///
/// ADR-0010: a drive declares only what it can natively write
/// (`generation`) — the medium's actual generation is a fact about the
/// cartridge, detected at `volume init` (`tape::media_detect`), never
/// declared here. `media_type`/`nominal_capacity` (the pre-ADR-0010 fields)
/// are rejected by name via `#[serde(deny_unknown_fields)]` — see
/// [`Config::load`]'s stale-field pre-scan for the friendly error a stale
/// config gets instead of a raw serde message. `capacity_override` survives
/// as the sole legitimate way to lie about capacity, for virtual drives
/// (mhvtl) and the microcosm test harnesses only.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LtoBackendConfig {
    pub name: String,
    pub device_tape: String,
    pub device_sg: String,
    /// The generation this drive natively writes, e.g. `"LTO-6"`. Parsed via
    /// `crate::media::Generation::parse` — `config check` (and every
    /// `Config::load`, via `validate_sizes`) errors if it does not parse.
    pub generation: String,
    /// Capacity override for a drive that lies about its media (mhvtl) or a
    /// cartridge whose real capacity differs from its generation's marketed
    /// figure. Checked ahead of the bound cartridge row and the generation
    /// table in every capacity resolution (ADR-0010); `config check` warns
    /// when this is set, since a real drive should never need it.
    #[serde(default)]
    pub capacity_override: Option<String>,
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
    // Issue #121: the write path's block size is a format constant that
    // never scales (docs/design/v2-open-questions.md:442,
    // volume-format-v2.md §1/D7) -- this default was "1M", silently
    // contradicting it. The field is still inert today
    // (cli::volume::DEFAULT_BLOCK_SIZE is what the write path actually
    // uses), so this default is currently only misleading, not dangerous,
    // but it should read as what the format actually fixes.
    "512K".to_string()
}

impl LtoBackendConfig {
    /// This backend's declared native capacity in bytes — an alias for
    /// [`Self::planning_capacity_bytes`] with no `--media` declaration.
    ///
    /// **No write path may call this.** ADR-0010 decision 3: capacity is
    /// decided once at `volume init`, from the generation of the medium
    /// ACTUALLY LOADED, and stored on `volumes.capacity_bytes`; every later
    /// gate (`write`, `resume`, `verify`) reads that row and config is never
    /// consulted for capacity again. Reading a drive's figure after init is
    /// exactly how issue #141 planned an LTO-5 cartridge as 2.5 TB. It
    /// survives only for capacity PLANNING, before any cartridge is loaded —
    /// and `planning_capacity_bytes` says that in its name.
    ///
    /// Both `generation` and `capacity_override` are already validated at
    /// `Config::load` time (`Config::validate_sizes`); the error path here
    /// only matters for a `Config` built directly (e.g. in tests) rather
    /// than loaded from a file.
    pub fn capacity_bytes(&self) -> Result<u64> {
        self.planning_capacity_bytes(None)
    }

    /// This drive's own native generation, parsed.
    pub fn native_generation(&self) -> Result<crate::media::Generation> {
        crate::media::Generation::parse(&self.generation).ok_or_else(|| {
            TapectlError::Config(format!(
                "backends.lto[\"{}\"].generation = {:?} is not a recognised LTO generation",
                self.name, self.generation
            ))
        })
    }

    /// Capacity for PLANNING a tape that is not loaded — `volume plan` and
    /// `collection plan`, which size batches before any cartridge is in the
    /// drive (ADR-0010, "Consequences").
    ///
    /// `media` is the operator's `--media <GEN>`, defaulting to this drive's
    /// native generation. The cartridge row is deliberately absent from the
    /// precedence that [`crate::media::resolve_capacity`] applies here:
    /// nothing is bound yet, because nothing has been initialised. Once a
    /// volume exists, its own `volumes.capacity_bytes` is the authoritative
    /// figure and config is never consulted again.
    pub fn planning_capacity_bytes(&self, media: Option<&str>) -> Result<u64> {
        let generation = match media {
            Some(m) => crate::media::Generation::parse(m).ok_or_else(|| {
                TapectlError::Other(format!(
                    "--media {m:?} is not a recognised LTO generation \
                     (e.g. LTO-6, LTO-7, LTO-7-M8, LTO-8)"
                ))
            })?,
            None => self.native_generation()?,
        };
        let override_bytes = match &self.capacity_override {
            Some(cap) => Some(crate::staging::parse_size_to_bytes(cap)? as u64),
            None => None,
        };
        Ok(crate::media::resolve_capacity(override_bytes, None, generation).0)
    }
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
    /// ADR-0006: how many WAREHOUSE copies a unit should carry, as the
    /// bottom layer of the three-level policy chain. 0 means "tape only is
    /// fine" — LTO is the primary line and the irreplaceable core earns
    /// the extra leg, so this is opt-in, per class, not fleet-wide.
    #[serde(default)]
    pub warehouse_copies: i64,
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
            warehouse_copies: 0,
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

/// One media-collection root (`docs/design/v2-open-questions.md` §11): a
/// folder=unit factory over existing unit machinery, batch-synced and
/// batch-written instead of ceremonially `unit init`'d one at a time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectionConfig {
    /// Collection name — also the unit-name prefix collection sync assigns
    /// (`"{name}/{relative_path}"`), so units stay unique across collections.
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
    /// Policy binding (slice size, min_copies, …) for units this collection
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
        // ADR-0010 renamed `backends.lto[].media_type`/`.nominal_capacity` to
        // `.generation`/`.capacity_override`, and `LtoBackendConfig` now
        // rejects unknown fields — a stale config with either old key would
        // otherwise fail with serde's generic "unknown field" message. Catch
        // it here, before serde ever sees it, so the operator gets the exact
        // remediation instead.
        if let Some(msg) = stale_lto_fields_message(&content) {
            return Err(TapectlError::Config(msg));
        }
        let config: Config =
            toml::from_str(&content).map_err(|e| TapectlError::Config(e.to_string()))?;
        config.validate_sizes()?;
        Ok(config)
    }

    /// Reject unparseable size strings (issue #59) at config-load time,
    /// rather than letting a bad value silently become 0 bytes or the wrong
    /// magnitude downstream in capacity math / bin-packing. Every
    /// size-typed field is named in its own error so the operator can find
    /// it without grepping the TOML: `defaults.slice_size`,
    /// `defaults.large_file_warn_threshold`, and each configured LTO
    /// backend's `enospc_buffer`/`capacity_override`.
    ///
    /// Also rejects an unparseable `backends.lto[].generation` (ADR-0010):
    /// every capacity and compatibility decision downstream reads this via
    /// `crate::media::Generation::parse`, so a bad value should fail loudly
    /// here rather than downstream as a confusing `None`.
    ///
    /// `backends.lto[].block_size` and `packing.min_free_for_append` are
    /// size-typed strings too but are dead config — nothing in the write
    /// path reads either (see `collection/plan.rs`'s `BLOCK_SIZE` comment
    /// for `block_size`; `min_free_for_append` has no reader at all) — so
    /// they are deliberately NOT validated here to avoid rejecting a config
    /// file over a field tapectl never acts on.
    fn validate_sizes(&self) -> Result<()> {
        crate::staging::parse_size_to_bytes(&self.defaults.slice_size)
            .map_err(|e| TapectlError::Config(format!("defaults.slice_size = {e}")))?;
        crate::staging::parse_size_to_bytes(&self.defaults.large_file_warn_threshold).map_err(
            |e| TapectlError::Config(format!("defaults.large_file_warn_threshold = {e}")),
        )?;
        for (i, backend) in self.backends.lto.iter().enumerate() {
            if crate::media::Generation::parse(&backend.generation).is_none() {
                return Err(TapectlError::Config(format!(
                    "backends.lto[{i}] (\"{}\").generation = {:?} is not a recognised LTO \
                     generation (e.g. LTO-6, LTO-7, LTO-7-M8, LTO-8)",
                    backend.name, backend.generation
                )));
            }
            if let Some(cap) = &backend.capacity_override {
                crate::staging::parse_size_to_bytes(cap).map_err(|e| {
                    TapectlError::Config(format!(
                        "backends.lto[{i}] (\"{}\").capacity_override = {e}",
                        backend.name
                    ))
                })?;
            }
            crate::staging::parse_size_to_bytes(&backend.enospc_buffer).map_err(|e| {
                TapectlError::Config(format!(
                    "backends.lto[{i}] (\"{}\").enospc_buffer = {e}",
                    backend.name
                ))
            })?;
        }
        Ok(())
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

/// The `[[backends.lto]]` block a user must add before anything can touch tape,
/// as commented-out TOML.
///
/// Serde cannot emit this: an empty `Vec<LtoBackendConfig>` serializes to
/// nothing at all, so a fresh config.toml gives no hint that the section exists
/// or what it must contain. `init` appends this after serialization (TOML
/// round-trips drop comments, so it cannot live in the struct), and
/// [`no_lto_backend_error`] points at it.
pub const LTO_BACKEND_EXAMPLE: &str = r#"
# ---------------------------------------------------------------------------
# Tape drive. Uncomment and edit before `volume write` / `collection run`.
# Find your drive:  ls -l /dev/tape/by-id/   (and `lsscsi -g` for the sg node)
#
# Prefer the by-id paths: /dev/nstN numbering is not stable across reboots.
# ---------------------------------------------------------------------------
# [[backends.lto]]
# name = "lto6"
# device_tape = "/dev/tape/by-id/scsi-XXXXXXXX-nst"
# device_sg = "/dev/sg1"
# generation = "LTO-6"
# hardware_compression = false
"#;

/// Error for "tape was asked for, but no drive is configured".
///
/// Shared by every site that resolves a tape backend so they cannot drift.
/// The bare string this replaced ("no LTO backend configured") stated a fact
/// and left the user to guess the file, the section name and its fields —
/// which is most of `init`'s first-run cliff (#124).
/// `paths` is optional only because two call sites do not carry it; the
/// literal fallback is the documented default home.
pub fn no_lto_backend_error(paths: Option<&TapectlPaths>) -> TapectlError {
    let file = paths.map_or_else(
        // Honest about the uncertainty: home resolution also honours --home and
        // TAPECTL_HOME, which are not reachable from here.
        || "your tapectl config (by default ~/.tapectl/config.toml)".to_string(),
        |p| p.config_file.display().to_string(),
    );
    TapectlError::Config(format!(
        "no LTO backend configured — tapectl does not know what tape drive to use.\n\n\
         Add a [[backends.lto]] section to {file}:\n{}\n\n\
         `tapectl init` leaves a commented-out example there to uncomment.",
        LTO_BACKEND_EXAMPLE.trim_end(),
    ))
}

/// If `content` (raw, unparsed config TOML) declares a `[[backends.lto]]`
/// entry still carrying the pre-ADR-0010 `media_type` or `nominal_capacity`
/// keys, return the exact remediation message for it.
///
/// Parses `content` as generic `toml::Value` rather than scanning lines by
/// hand, so comments and quoting are handled the way TOML actually defines
/// them, not approximately. Returns `None` (letting the normal parse error
/// surface unchanged) when `content` is not even syntactically valid TOML —
/// this check only ever *sharpens* an error that was going to happen anyway,
/// never introduces a new failure mode of its own.
fn stale_lto_fields_message(content: &str) -> Option<String> {
    let value: toml::Value = toml::from_str(content).ok()?;
    let backends = value.get("backends")?.get("lto")?.as_array()?;
    for entry in backends {
        let table = entry.as_table()?;
        if table.contains_key("media_type") || table.contains_key("nominal_capacity") {
            let name = table.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            return Some(format!(
                "backends.lto[\"{name}\"]: \"media_type\" and \"nominal_capacity\" moved — \
                 declare the DRIVE's generation as generation = \"LTO-6\"; capacity now \
                 follows the cartridge's generation (ADR-0010); capacity_override is for \
                 virtual drives only"
            ));
        }
    }
    None
}

/// Match a configured `device_tape` against a requested device path.
///
/// String equality first (the common case, and the only comparison that
/// works for paths that don't exist — every microcosm test fixture uses
/// `/dev/null` or a nonexistent placeholder for both); falling back to
/// `std::fs::canonicalize` of both sides so a by-id symlink and the
/// `/dev/nstN` it resolves to are recognised as the same drive.
/// Canonicalize errors (either side missing) are treated as "no match", not
/// propagated — this is a best-effort convenience, not a filesystem check.
fn device_matches(configured: &str, requested: &str) -> bool {
    if configured == requested {
        return true;
    }
    match (
        std::fs::canonicalize(configured),
        std::fs::canonicalize(requested),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Resolve the configured LTO backend for a WRITE path (`volume init`,
/// `write`, `resume`, `compact-read`, `compact-write`) — STRICT (ADR-0010):
/// an explicit `device` that matches no configured `device_tape` is an
/// error, never a silent fallback to "whichever backend happened to be
/// first", which is the exact shape of issue #141.
///
/// - `device` given: the backend whose `device_tape` matches (string or
///   canonicalized path); no match is an error naming every configured
///   `device_tape` so the operator can see what does exist.
/// - `device` absent: the sole configured backend; zero is
///   [`no_lto_backend_error`]; more than one is an error naming every
///   backend and asking for `--device`.
pub fn resolve_lto_backend<'a>(
    config: &'a Config,
    device: Option<&str>,
) -> Result<&'a LtoBackendConfig> {
    if let Some(dev) = device {
        return config
            .backends
            .lto
            .iter()
            .find(|b| device_matches(&b.device_tape, dev))
            .ok_or_else(|| {
                let configured: Vec<&str> = config
                    .backends
                    .lto
                    .iter()
                    .map(|b| b.device_tape.as_str())
                    .collect();
                TapectlError::Config(format!(
                    "no [[backends.lto]] entry has device_tape = {dev} (configured: {})",
                    if configured.is_empty() {
                        "none".to_string()
                    } else {
                        configured.join(", ")
                    }
                ))
            });
    }
    match config.backends.lto.len() {
        0 => Err(no_lto_backend_error(None)),
        1 => Ok(&config.backends.lto[0]),
        _ => {
            let names: Vec<&str> = config
                .backends
                .lto
                .iter()
                .map(|b| b.name.as_str())
                .collect();
            Err(TapectlError::Config(format!(
                "multiple LTO backends configured ({}); pass --device to select one",
                names.join(", ")
            )))
        }
    }
}

/// Resolve a device path (and, if known, its backend) for a READ path
/// (`volume identify`, `verify`, `read-slices`, `catalog rebuild`,
/// `restore`) — LENIENT (ADR-0010): these must stay usable on a rebuilt
/// machine that has keys and no `backend add` yet (ADR-0005's DR path), so
/// an explicit `device` is used exactly as given and the backend is
/// whatever matches it, or `None` — this function never errors when
/// `device` is `Some`.
///
/// - `device` given: returned as-is, paired with whatever backend matches it
///   (or `None`).
/// - `device` absent: the sole configured backend's `device_tape`; zero
///   backends is an error (there is truly nothing to read from); more than
///   one is an error asking for `--device`, mirroring
///   [`resolve_lto_backend`].
pub fn resolve_device<'a>(
    config: &'a Config,
    device: Option<&str>,
) -> Result<(String, Option<&'a LtoBackendConfig>)> {
    if let Some(dev) = device {
        let backend = config
            .backends
            .lto
            .iter()
            .find(|b| device_matches(&b.device_tape, dev));
        return Ok((dev.to_string(), backend));
    }
    match config.backends.lto.len() {
        0 => Err(TapectlError::Config(
            "no drive configured and no --device given".to_string(),
        )),
        1 => {
            let b = &config.backends.lto[0];
            Ok((b.device_tape.clone(), Some(b)))
        }
        _ => {
            let names: Vec<&str> = config
                .backends
                .lto
                .iter()
                .map(|b| b.name.as_str())
                .collect();
            Err(TapectlError::Config(format!(
                "multiple LTO backends configured ({}); pass --device to select one",
                names.join(", ")
            )))
        }
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

    /// Create all directories if they don't exist, and tighten every one
    /// (freshly created or pre-existing) to 0700.
    ///
    /// Issue #41: `~/.tapectl` holds the plaintext content-metadata index
    /// (`tapectl.db`, receipts, dar catalogs) the on-tape format works hard
    /// to keep out of plaintext — leaving the directory tree at whatever
    /// the process umask hands out (0755 on a stock single-user box)
    /// contradicts that. Tightening runs every call, not just on first
    /// creation, so an already-initialized `~/.tapectl` gets the same
    /// treatment as a fresh `init` — this is idempotent and, via
    /// `secure_path`, never fails the caller: a directory this process
    /// does not own (e.g. a shared multi-user box) only logs a warning
    /// rather than aborting an otherwise-fine command.
    pub fn ensure_dirs(&self) -> Result<()> {
        for dir in [
            &self.home,
            &self.keys_dir,
            &self.catalogs_dir,
            &self.receipts_dir,
            &self.logs_dir,
        ] {
            std::fs::create_dir_all(dir)?;
            secure_path(dir, 0o700);
        }
        Ok(())
    }

    /// Check if tapectl has been initialized (DB exists).
    pub fn is_initialized(&self) -> bool {
        self.db_file.exists()
    }
}

#[cfg(test)]
mod tests {
    //! Issue #41: `~/.tapectl` was created with no explicit mode anywhere,
    //! so it ends up whatever the process umask hands out — on a stock
    //! single-user box (umask 022) that's 0755 dirs / 0644 files, which
    //! leaves the plaintext content-metadata index (tapectl.db, receipts,
    //! dar catalogs) world-readable. These tests assert actual mode bits,
    //! never "does it not error", since that's exactly the class of test
    //! that would pass whether or not the fix is present.
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path)
            .unwrap_or_else(|e| panic!("metadata({}) failed: {e}", path.display()))
            .permissions()
            .mode()
            & 0o777
    }

    fn all_dirs(paths: &TapectlPaths) -> Vec<(&'static str, &Path)> {
        vec![
            ("home", &paths.home),
            ("keys_dir", &paths.keys_dir),
            ("catalogs_dir", &paths.catalogs_dir),
            ("receipts_dir", &paths.receipts_dir),
            ("logs_dir", &paths.logs_dir),
        ]
    }

    #[test]
    fn ensure_dirs_creates_home_and_every_subdir_at_0700() {
        let tmp = TempDir::new().unwrap();
        // home itself is the "intermediate" relative to the four subdirs
        // (the "leaves") — this single assertion set covers both, per the
        // task's requirement to verify leaf AND intermediate.
        let home = tmp.path().join(".tapectl");
        let paths = TapectlPaths::new(home);
        paths.ensure_dirs().unwrap();

        for (name, dir) in all_dirs(&paths) {
            assert!(dir.is_dir(), "{name} should exist");
            assert_eq!(
                mode_of(dir),
                0o700,
                "{name} ({}) should be 0700, was {:o}",
                dir.display(),
                mode_of(dir)
            );
        }
    }

    #[test]
    fn ensure_dirs_is_idempotent_and_does_not_error() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join(".tapectl");
        let paths = TapectlPaths::new(home);

        paths.ensure_dirs().unwrap();
        // Second call must not error, and modes must remain 0700.
        paths.ensure_dirs().unwrap();

        for (name, dir) in all_dirs(&paths) {
            assert_eq!(mode_of(dir), 0o700, "{name} should stay 0700 on re-run");
        }
    }

    /// Umask-independent mutation detector (advisor guidance): this box's
    /// umask (0002) happens to make a *freshly created* dir land at 0755,
    /// which already differs from 0700 — so the "fresh creation" test above
    /// would also catch a reverted fix here. But on a box with umask 077, a
    /// freshly created dir would coincidentally already be 0700 even with
    /// no chmod call at all, and that test would falsely "pass" against
    /// reverted code. Seeding an explicitly-loose pre-existing directory and
    /// asserting it gets *tightened* is independent of umask entirely: with
    /// the fix reverted, a pre-existing 0755 dir is left completely
    /// untouched (create_dir_all no-ops on a dir that already exists), so
    /// this test fails identically no matter what the ambient umask is.
    #[test]
    fn ensure_dirs_tightens_a_pre_existing_loose_directory() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join(".tapectl");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(mode_of(&home), 0o755, "fixture must start loose");

        let paths = TapectlPaths::new(home.clone());
        paths.ensure_dirs().unwrap();

        assert_eq!(
            mode_of(&home),
            0o700,
            "a pre-existing 0755 home dir must be tightened to 0700"
        );
    }

    #[test]
    fn secure_path_sets_directory_mode() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("loose");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        secure_path(&dir, 0o700);

        assert_eq!(mode_of(&dir), 0o700);
    }

    #[test]
    fn secure_path_sets_file_mode() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("loose.txt");
        std::fs::write(&file, b"content").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();

        secure_path(&file, 0o600);

        assert_eq!(mode_of(&file), 0o600);
    }

    #[test]
    fn secure_path_on_missing_path_does_not_panic_or_propagate() {
        let tmp = TempDir::new().unwrap();
        let ghost = tmp.path().join("does-not-exist");
        // Best-effort: must not panic. There is nothing to assert on the
        // filesystem afterward — the guarantee under test is "doesn't
        // crash the caller", which a successful return from this call
        // (never a Result, so nothing to unwrap) demonstrates directly.
        secure_path(&ghost, 0o700);
        assert!(!ghost.exists());
    }

    #[test]
    fn write_private_file_creates_with_requested_mode() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("secret.txt");

        write_private_file(&path, b"top secret", 0o600).unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"top secret");
        assert_eq!(mode_of(&path), 0o600);
    }

    #[test]
    fn write_private_file_overwrites_existing_content_and_keeps_mode() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("secret.txt");
        std::fs::write(&path, b"stale, world-readable").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        write_private_file(&path, b"fresh", 0o600).unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"fresh");
        assert_eq!(mode_of(&path), 0o600);
    }

    /// The commented `[[backends.lto]]` example `init` writes is the first
    /// thing a user edits, and the error message reprints it verbatim. If a
    /// field is renamed, made required, or dropped, the example silently
    /// becomes instructions that produce a config the tool then rejects.
    ///
    /// So: uncomment it exactly as a user would and require that it parses into
    /// a real backend.
    #[test]
    fn the_commented_lto_example_is_a_valid_backend_once_uncommented() {
        let uncommented: String = LTO_BACKEND_EXAMPLE
            .lines()
            .filter(|l| !l.trim_start().starts_with("# ---") && !l.trim().is_empty())
            .map(|l| l.trim_start().trim_start_matches('#').trim_start())
            .filter(|l| l.starts_with("[[") || l.contains(" = "))
            .collect::<Vec<_>>()
            .join("\n");

        let cfg: Config = toml::from_str(&uncommented).unwrap_or_else(|e| {
            panic!("uncommented example is not valid config: {e}\n{uncommented}")
        });

        assert_eq!(
            cfg.backends.lto.len(),
            1,
            "example should define exactly one backend, got:\n{uncommented}"
        );
        let b = &cfg.backends.lto[0];
        assert!(!b.name.is_empty());
        assert!(b.device_tape.starts_with("/dev/"));
        assert!(b.device_sg.starts_with("/dev/"));
        // ADR-0010: the example declares a generation, not a capacity — it
        // must survive the same parser `config check`/`Config::load` uses.
        assert!(
            crate::media::Generation::parse(&b.generation).is_some(),
            "example generation {:?} must parse",
            b.generation
        );
    }

    /// As shipped (fully commented) the example must be inert: appending it to
    /// a freshly serialized config must still parse, and must NOT declare a
    /// backend — otherwise `init` would hand every new user a half-configured
    /// drive pointing at a placeholder device path.
    #[test]
    fn the_example_is_inert_until_the_user_uncomments_it() {
        let mut text = toml::to_string_pretty(&Config::default()).unwrap();
        text.push_str(LTO_BACKEND_EXAMPLE);
        let cfg: Config = toml::from_str(&text).expect("config + example must parse");
        assert!(
            cfg.backends.lto.is_empty(),
            "the shipped example must declare no backend"
        );
    }

    // ---- ADR-0010: stale-field pre-scan ----

    #[test]
    fn stale_media_type_is_named_with_the_exact_remediation() {
        let text = "[[backends.lto]]\nname = \"lto1\"\ndevice_tape = \"/dev/nst0\"\n\
                     device_sg = \"/dev/sg0\"\nmedia_type = \"LTO-6\"\n\
                     nominal_capacity = \"2.5TB\"\n";
        let msg = stale_lto_fields_message(text).expect("must be flagged");
        assert!(msg.contains("backends.lto[\"lto1\"]"), "{msg}");
        assert!(
            msg.contains("media_type") && msg.contains("nominal_capacity"),
            "{msg}"
        );
        assert!(msg.contains("ADR-0010"), "{msg}");
    }

    #[test]
    fn stale_nominal_capacity_alone_is_also_flagged() {
        let text = "[[backends.lto]]\nname = \"lto1\"\ndevice_tape = \"/dev/nst0\"\n\
                     device_sg = \"/dev/sg0\"\ngeneration = \"LTO-6\"\n\
                     nominal_capacity = \"2.5TB\"\n";
        assert!(stale_lto_fields_message(text).is_some());
    }

    #[test]
    fn a_clean_generation_only_backend_is_not_flagged() {
        let text = "[[backends.lto]]\nname = \"lto1\"\ndevice_tape = \"/dev/nst0\"\n\
                     device_sg = \"/dev/sg0\"\ngeneration = \"LTO-6\"\n";
        assert!(stale_lto_fields_message(text).is_none());
    }

    #[test]
    fn syntactically_invalid_toml_is_not_flagged_here() {
        // Not this function's job — the normal toml::from_str error surfaces
        // unchanged for a genuinely broken file.
        assert!(stale_lto_fields_message("this is not [ toml").is_none());
    }

    #[test]
    fn config_load_rejects_a_stale_backend_with_the_friendly_message() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "[[backends.lto]]\nname = \"lto1\"\ndevice_tape = \"/dev/nst0\"\n\
             device_sg = \"/dev/sg0\"\nmedia_type = \"LTO-6\"\n\
             nominal_capacity = \"2.5TB\"\n",
        )
        .unwrap();
        let err = Config::load(&path).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("moved"), "{msg}");
        assert!(msg.contains("ADR-0010"), "{msg}");
    }

    #[test]
    fn config_load_rejects_an_unparseable_generation() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "[[backends.lto]]\nname = \"lto1\"\ndevice_tape = \"/dev/nst0\"\n\
             device_sg = \"/dev/sg0\"\ngeneration = \"not-a-generation\"\n",
        )
        .unwrap();
        let err = Config::load(&path).unwrap_err();
        assert!(err.to_string().contains("generation"), "{err}");
    }

    // ---- ADR-0010: resolve_lto_backend (strict) ----

    fn backend_with(name: &str, device_tape: &str) -> LtoBackendConfig {
        LtoBackendConfig {
            name: name.to_string(),
            device_tape: device_tape.to_string(),
            device_sg: "/dev/sg0".to_string(),
            generation: "LTO-6".to_string(),
            capacity_override: None,
            usable_capacity_factor: default_usable_capacity_factor(),
            enospc_buffer: default_enospc_buffer(),
            block_size: default_block_size(),
            hardware_compression: false,
        }
    }

    #[test]
    fn resolve_lto_backend_with_device_matches_by_string() {
        let mut config = Config::default();
        config.backends.lto.push(backend_with("a", "/dev/null"));
        let b = resolve_lto_backend(&config, Some("/dev/null")).unwrap();
        assert_eq!(b.name, "a");
    }

    #[test]
    fn resolve_lto_backend_with_device_and_no_match_errors_naming_configured() {
        let mut config = Config::default();
        config.backends.lto.push(backend_with("a", "/dev/null"));
        let err = resolve_lto_backend(&config, Some("/dev/nonexistent-xyz")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("/dev/null"), "{msg}");
        assert!(msg.contains("/dev/nonexistent-xyz"), "{msg}");
    }

    #[test]
    fn resolve_lto_backend_no_device_sole_backend_is_used() {
        let mut config = Config::default();
        config.backends.lto.push(backend_with("a", "/dev/null"));
        let b = resolve_lto_backend(&config, None).unwrap();
        assert_eq!(b.name, "a");
    }

    #[test]
    fn resolve_lto_backend_no_device_zero_backends_is_no_lto_backend_error() {
        let config = Config::default();
        let err = resolve_lto_backend(&config, None).unwrap_err();
        assert!(err.to_string().contains("no LTO backend configured"));
    }

    #[test]
    fn resolve_lto_backend_no_device_multiple_backends_errors_naming_them() {
        let mut config = Config::default();
        config.backends.lto.push(backend_with("a", "/dev/null"));
        config.backends.lto.push(backend_with("b", "/dev/zero"));
        let err = resolve_lto_backend(&config, None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains('a') && msg.contains('b'), "{msg}");
        assert!(msg.contains("--device"), "{msg}");
    }

    // ---- ADR-0010: resolve_device (lenient) ----

    #[test]
    fn resolve_device_with_device_never_errors_even_with_zero_backends() {
        let config = Config::default();
        let (device, backend) = resolve_device(&config, Some("/dev/whatever")).unwrap();
        assert_eq!(device, "/dev/whatever");
        assert!(backend.is_none());
    }

    #[test]
    fn resolve_device_with_device_finds_a_matching_backend() {
        let mut config = Config::default();
        config.backends.lto.push(backend_with("a", "/dev/null"));
        let (device, backend) = resolve_device(&config, Some("/dev/null")).unwrap();
        assert_eq!(device, "/dev/null");
        assert_eq!(backend.unwrap().name, "a");
    }

    #[test]
    fn resolve_device_with_device_and_no_matching_backend_returns_none_not_error() {
        let mut config = Config::default();
        config.backends.lto.push(backend_with("a", "/dev/null"));
        let (device, backend) = resolve_device(&config, Some("/dev/nonexistent-xyz")).unwrap();
        assert_eq!(device, "/dev/nonexistent-xyz");
        assert!(backend.is_none());
    }

    #[test]
    fn resolve_device_no_device_sole_backend_is_used() {
        let mut config = Config::default();
        config.backends.lto.push(backend_with("a", "/dev/null"));
        let (device, backend) = resolve_device(&config, None).unwrap();
        assert_eq!(device, "/dev/null");
        assert_eq!(backend.unwrap().name, "a");
    }

    #[test]
    fn resolve_device_no_device_zero_backends_errors() {
        let config = Config::default();
        let err = resolve_device(&config, None).unwrap_err();
        assert!(err.to_string().contains("no drive configured"));
    }

    #[test]
    fn resolve_device_no_device_multiple_backends_errors_naming_them() {
        let mut config = Config::default();
        config.backends.lto.push(backend_with("a", "/dev/null"));
        config.backends.lto.push(backend_with("b", "/dev/zero"));
        let err = resolve_device(&config, None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains('a') && msg.contains('b'), "{msg}");
        assert!(msg.contains("--device"), "{msg}");
    }
}
