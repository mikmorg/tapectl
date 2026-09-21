pub mod archive_set;
pub mod audit;
pub mod backend;
pub mod cartridge;
pub mod catalog;
pub mod collection;
pub mod config;
pub mod consent;
pub mod db;
pub mod key;
pub mod location;
pub mod operations;
pub mod report;
pub mod restore;
pub mod snapshot;
pub mod stage;
pub mod staging;
pub mod tenant;
pub mod unit;
pub mod volume;

use clap::{Parser, Subcommand};

/// tapectl — Multi-Tenant Archival Storage Management System
#[derive(Parser, Debug)]
#[command(name = "tapectl", version, about)]
pub struct Cli {
    /// Output in JSON format
    #[arg(long, global = true)]
    pub json: bool,

    /// Show what would be done without making changes. Commands that
    /// cannot preview refuse the flag rather than ignore it (issue #241).
    #[arg(long, global = true)]
    pub dry_run: bool,

    /// Enable verbose output
    #[arg(long, short, global = true)]
    pub verbose: bool,

    /// Skip ADR-0008 Tier-2 confirmation prompts. It never reaches a
    /// Tier-3 refusal — those are facts, not risks to accept (issue #147)
    #[arg(long, short, global = true)]
    pub yes: bool,

    /// Path to config file.
    ///
    /// NOTE: on its own this ALSO relocates the whole tapectl home to the
    /// config file's parent directory — database, keys, staging, receipts.
    /// That is how every test harness gets an isolated home, so it still
    /// works, but it is surprising enough that it now warns. Use --home
    /// when you mean "operate on a different archive", and --config only
    /// to point at a config file inside that home (issue #109).
    #[arg(long, global = true)]
    pub config: Option<String>,

    /// tapectl home directory: database, keys, catalogs, receipts, logs.
    ///
    /// Defaults to ~/.tapectl. The config file is taken from
    /// <home>/config.toml unless --config overrides it. Also settable as
    /// TAPECTL_HOME (issue #109).
    #[arg(long, global = true)]
    pub home: Option<String>,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Initialize tapectl (DB, config, operator tenant, keys, and the
    /// permanent escrow recipient — ADR-0005; use --no-escrow to skip it, or
    /// --escrow-public-key to adopt an existing one instead of minting a new
    /// identity, #139)
    Init {
        /// Operator name (defaults to system username)
        #[arg(long)]
        operator: Option<String>,
        /// Do NOT create the permanent escrow recipient (ADR-0005) at init.
        /// By default `init` generates it and prints its secret once. Use this
        /// only when you will adopt an existing escrow identity with
        /// `key import --escrow` instead, or in tests/tooling that register
        /// escrow separately.
        #[arg(long)]
        no_escrow: bool,
        /// Register THIS existing escrow public key (an age1… literal, or a
        /// path to a .pub file) instead of minting a new identity — the
        /// disaster-recovery form: a rebuilt machine adopts the original
        /// recipient from the heir kit's cover sheet so every tape's
        /// receipts keep matching (#139).
        #[arg(long, value_name = "KEY_OR_FILE", conflicts_with = "no_escrow")]
        escrow_public_key: Option<String>,
    },

    /// Manage tenants
    Tenant {
        #[command(subcommand)]
        command: tenant::TenantCommands,
    },

    /// Manage encryption keys
    Key {
        #[command(subcommand)]
        command: key::KeyCommands,
    },

    /// Manage archival units
    Unit {
        #[command(subcommand)]
        command: unit::UnitCommands,
    },

    /// Manage media collections (folder=unit factory + batch tape driver)
    Collection {
        #[command(subcommand)]
        command: collection::CollectionCommands,
    },

    /// Manage snapshots
    Snapshot {
        #[command(subcommand)]
        command: snapshot::SnapshotCommands,
    },

    /// Stage snapshots for writing
    Stage {
        #[command(subcommand)]
        command: stage::StageCommands,
    },

    /// Manage staging area
    Staging {
        #[command(subcommand)]
        command: staging::StagingCommands,
    },

    /// Manage volumes and tape operations
    Volume {
        #[command(subcommand)]
        command: volume::VolumeCommands,
    },

    /// Manage physical cartridges
    Cartridge {
        #[command(subcommand)]
        command: cartridge::CartridgeCommands,
    },

    /// Manage archive set policies
    ArchiveSet {
        #[command(subcommand)]
        command: archive_set::ArchiveSetCommands,
    },

    /// Policy compliance audit
    Audit {
        /// Show remediation commands
        #[arg(long)]
        action_plan: bool,
        /// Filter to a specific unit
        #[arg(long)]
        unit: Option<String>,
    },

    /// Browse and search file catalog
    Catalog {
        #[command(subcommand)]
        command: catalog::CatalogCommands,
    },

    /// Manage storage locations
    Location {
        #[command(subcommand)]
        command: location::LocationCommands,
    },

    /// Generate reports
    Report {
        #[command(subcommand)]
        command: report::ReportCommands,
    },

    /// Restore data from volumes
    Restore {
        #[command(subcommand)]
        command: restore::RestoreCommands,
    },

    /// Export encrypted slices to directory
    Export {
        /// Unit name
        #[arg(long)]
        unit: String,
        /// Destination directory
        #[arg(long)]
        to: String,
    },

    /// Import a pre-existing volume into the database
    Import {
        /// Volume label
        #[arg(long)]
        label: String,
        /// Backend type
        #[arg(long, default_value = "lto")]
        backend: String,
        /// Media generation (e.g., LTO-6, LTO-7, LTO-7-M8, LTO-8)
        #[arg(long)]
        generation: String,
        /// Capacity (e.g., "2500G"). Decimal, as printed on the cartridge
        /// (K=10^3 ... T=10^12; ADR-0012) — not the binary unit
        /// `slice_size`/`enospc_buffer` use.
        ///
        /// Defaults to the generation table's native capacity for
        /// `--generation` when omitted (ADR-0010, decision 3).
        #[arg(long)]
        capacity: Option<String>,
        /// Which configured drive this volume belongs to, by its device path
        /// (ADR-0010). Only needed when more than one `[[backends.lto]]` is
        /// configured — with one drive, or none, the behaviour is unchanged.
        /// Resolved leniently: this command only writes a catalog row and
        /// never touches the device, so a path no backend claims is not an
        /// error (issue #151).
        #[arg(long)]
        device: Option<String>,
        /// Notes
        #[arg(long)]
        notes: Option<String>,
    },

    /// Quick archive: create + stage + write in one flow
    ///
    /// The volume must already exist — run `tapectl volume init LABEL
    /// --device /dev/nstN` first. This command creates the unit, snapshot and
    /// stage set, but not the volume.
    QuickArchive {
        /// Path to directory
        path: String,
        /// Tenant name
        #[arg(long)]
        tenant: String,
        /// Label of an ALREADY-INITIALIZED volume to write to. Create it with
        /// `tapectl volume init LABEL --device ...`; quick-archive does not.
        #[arg(long)]
        volume: String,
        /// Tags
        #[arg(long, short)]
        tag: Vec<String>,
        /// Tape device (by-id path). Defaults to the only configured drive;
        /// required when more than one is configured.
        #[arg(long)]
        device: Option<String>,
    },

    /// Tape drive backends
    Backend {
        #[command(subcommand)]
        command: BackendCommands,
    },

    /// Database operations
    Db {
        #[command(subcommand)]
        command: DbCommands,
    },

    /// Configuration management
    Config {
        #[command(subcommand)]
        command: ConfigCommands,
    },

    /// Generate shell completions
    Completions {
        /// Shell to generate completions for
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
}

/// Tape drive backend configuration (#126).
#[derive(Subcommand, Debug)]
pub enum BackendCommands {
    /// Add an LTO tape drive to the config
    ///
    /// Writes a validated `[[backends.lto]]` block, appended so existing
    /// comments survive. Find your drive with `ls -l /dev/tape/by-id/`, and
    /// `lsscsi -g` for the sg node. Prefer the by-id paths: /dev/nstN
    /// numbering is not stable across reboots.
    Add {
        /// Name for this drive, used to select it later. Letters, digits,
        /// dot, underscore and dash.
        #[arg(long)]
        name: String,
        /// Tape device node, e.g. /dev/tape/by-id/scsi-XXXX-nst
        #[arg(long)]
        device_tape: String,
        /// SCSI generic node used for health/MAM queries, e.g. /dev/sg1
        #[arg(long)]
        device_sg: String,
        /// Generation this drive natively writes, e.g. LTO-6 (ADR-0010: a
        /// drive declares only what it can write; the medium's actual
        /// generation is detected at `volume init`, not declared here).
        #[arg(long)]
        generation: String,
        /// Capacity override — for virtual drives (mhvtl) and test
        /// harnesses only. A real drive's capacity follows the loaded
        /// cartridge's detected generation (ADR-0010); leave this unset.
        /// Decimal, as printed on the cartridge (K=10^3 ... T=10^12;
        /// ADR-0012) — not the binary unit `enospc_buffer` uses.
        #[arg(long)]
        capacity_override: Option<String>,
        /// Headroom reserved before end-of-tape, e.g. 50M
        #[arg(long)]
        enospc_buffer: Option<String>,
    },
}

/// Database operations.
#[derive(Subcommand, Debug)]
pub enum DbCommands {
    /// Backup database, and optionally keys, and catalogs
    Backup {
        /// Destination path
        #[arg(long)]
        to: String,
        /// Also copy the private key directory to `<dest>.keys`. Off by
        /// default — the database alone is the common backup case.
        /// Private key material copied this way must be treated as secret
        /// wherever the destination ends up (USB stick, network share,
        /// cloud-synced folder, ...) — issue #40.
        #[arg(long)]
        include_keys: bool,
    },
    /// Check database integrity
    Fsck {
        /// Delete rows whose foreign-key parent is missing, closing the
        /// graph in one transaction (children first, in effect); logged
        #[arg(long)]
        repair: bool,
    },
    /// Export database as JSON
    Export,
    /// Import database from backup
    Import {
        /// Path to backup file
        path: String,
    },
    /// Show database statistics
    Stats,
}

/// Configuration management commands.
#[derive(Subcommand, Debug)]
pub enum ConfigCommands {
    /// Show current configuration
    Show,
    /// Check configuration validity
    Check,
}

/// Resolve a `--device` for a WRITE path — STRICT (ADR-0010, "Backends
/// resolve by device").
///
/// A write needs the drive's own factor, ENOSPC buffer and sg node, so the
/// device must belong to a configured backend: the one whose `device_tape`
/// matches (canonicalised), else the sole configured backend, else an error
/// naming the candidates. The backend's own `device_tape` is returned rather
/// than the operator's spelling of it, so a by-id link and its `/dev/nstN`
/// target reach the tape layer as one path.
///
/// The counterpart for read paths is [`read_device`]; the split is the whole
/// point, so every call site should say which one it is.
pub(crate) fn write_device(
    config: &crate::config::Config,
    device: Option<&str>,
) -> crate::error::Result<String> {
    Ok(crate::config::resolve_lto_backend(config, device)?
        .device_tape
        .clone())
}

/// Resolve a `--device` for a READ path — LENIENT (ADR-0010, "Read paths
/// stay usable without a configured drive").
///
/// `identify`, `verify`, `read-slices`, `restore` and `catalog rebuild` must
/// work on the rebuilt machine that has keys and no `backend add` yet
/// (ADR-0005's DR path), so an explicit `--device` is taken exactly as given
/// and no backend need exist at all. Only the no-`--device` case can fail,
/// and then because there is genuinely nothing to read from.
pub(crate) fn read_device(
    config: &crate::config::Config,
    device: Option<&str>,
) -> crate::error::Result<String> {
    Ok(crate::config::resolve_device(config, device)?.0)
}

/// The one place every "`--dry-run` is not supported here" refusal is
/// worded (issue #241, #230's rule: "where a real dry-run is too large a
/// change, refuse the flag rather than ignore it — a refusal is honest;
/// silently writing a tape is not").
///
/// `command` is the full invocation as an operator would type it (e.g.
/// `"volume write"`), `why` is the one-clause, actionable reason a real
/// preview isn't offered here — what the command would otherwise have to
/// DO to produce one, and, where there is one, the alternative that gets
/// the operator most of the way there (e.g. `volume plan`). Called as the
/// FIRST statement of the arm, before any resource is touched, so the
/// refusal is never itself a side effect.
pub(crate) fn refuse_dry_run(command: &str, why: &str) -> crate::error::TapectlError {
    crate::error::TapectlError::Other(format!(
        "--dry-run is not supported by `tapectl {command}`: {why} Drop the flag to run it \
         for real."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Issue #169: `import --generation` lost its `"LTO-6"` default and is
    /// now a plain required `clap` field, so omitting it is a usage error
    /// clap itself enforces — never a value tapectl silently makes up. This
    /// is asserted at the `clap` layer (`Cli::try_parse_from`) rather than
    /// via `tests/cli_smoke.rs`'s process harness, since that file is
    /// outside this change's scope fence.
    #[test]
    fn import_without_generation_is_a_usage_error() {
        let result = Cli::try_parse_from(["tapectl", "import", "--label", "L1"]);
        let err = result.expect_err("missing --generation must be a clap usage error");
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::MissingRequiredArgument,
            "unexpected error kind: {err}"
        );
        assert!(
            err.to_string().contains("generation"),
            "usage error should name the missing --generation flag: {err}"
        );
    }

    /// `--capacity` has no default any more either (it resolves from the
    /// generation table instead), but unlike `--generation` it is optional,
    /// not required — this is the negative case proving the two flags
    /// diverged correctly.
    #[test]
    fn import_without_capacity_still_parses() {
        let result = Cli::try_parse_from([
            "tapectl",
            "import",
            "--label",
            "L1",
            "--generation",
            "LTO-6",
        ]);
        result.expect("omitting --capacity must still parse");
    }
}
