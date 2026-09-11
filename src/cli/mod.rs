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

    /// Show what would be done without making changes
    #[arg(long, global = true)]
    pub dry_run: bool,

    /// Enable verbose output
    #[arg(long, short, global = true)]
    pub verbose: bool,

    /// Skip confirmation prompts
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
    /// permanent escrow recipient — ADR-0005; use --no-escrow to skip it)
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
        /// Media type
        #[arg(long, default_value = "LTO-6")]
        media_type: String,
        /// Capacity (e.g., "2500G")
        #[arg(long, default_value = "2500G")]
        capacity: String,
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
        /// Tape device path
        #[arg(long, default_value = "/dev/nst0")]
        device: String,
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
        /// Media type, e.g. LTO-6
        #[arg(long, default_value = "LTO-6")]
        media_type: String,
        /// Uncompressed nominal capacity, e.g. 2.5TB
        #[arg(long, default_value = "2.5TB")]
        capacity: String,
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
        /// Attempt to repair issues
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
