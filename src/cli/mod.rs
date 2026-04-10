pub mod archive_set;
pub mod audit;
pub mod cartridge;
pub mod catalog;
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

    /// Path to config file
    #[arg(long, global = true)]
    pub config: Option<String>,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Initialize tapectl (DB, config, operator tenant, keys)
    Init {
        /// Operator name (defaults to system username)
        #[arg(long)]
        operator: Option<String>,
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
    QuickArchive {
        /// Path to directory
        path: String,
        /// Tenant name
        #[arg(long)]
        tenant: String,
        /// Volume label
        #[arg(long)]
        volume: String,
        /// Tags
        #[arg(long, short)]
        tag: Vec<String>,
        /// Tape device path
        #[arg(long, default_value = "/dev/nst0")]
        device: String,
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

/// Database operations.
#[derive(Subcommand, Debug)]
pub enum DbCommands {
    /// Backup database, keys, and catalogs
    Backup {
        /// Destination path
        #[arg(long)]
        to: String,
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
