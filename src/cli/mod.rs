pub mod catalog;
pub mod key;
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

    // ── Future milestone stubs ──
    /// Manage physical cartridges
    Cartridge {
        #[command(subcommand)]
        command: StubCommand,
    },

    /// Manage archive set policies
    ArchiveSet {
        #[command(subcommand)]
        command: StubCommand,
    },

    /// Policy compliance audit
    Audit {
        #[command(subcommand)]
        command: StubCommand,
    },

    /// Browse and search file catalog
    Catalog {
        #[command(subcommand)]
        command: catalog::CatalogCommands,
    },

    /// Manage storage locations
    Location {
        #[command(subcommand)]
        command: StubCommand,
    },

    /// Generate reports
    Report {
        #[command(subcommand)]
        command: StubCommand,
    },

    /// Restore data from volumes
    Restore {
        #[command(subcommand)]
        command: restore::RestoreCommands,
    },

    /// Export encrypted slices to directory
    Export {
        #[command(subcommand)]
        command: StubCommand,
    },

    /// Quick archive: create + stage + write in one flow
    QuickArchive {
        #[command(subcommand)]
        command: StubCommand,
    },

    /// Database operations
    Db {
        #[command(subcommand)]
        command: StubCommand,
    },

    /// Configuration management
    Config {
        #[command(subcommand)]
        command: StubCommand,
    },

    /// Generate shell completions
    Completions {
        /// Shell to generate completions for
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
}

/// Placeholder for subcommands not yet implemented.
#[derive(Subcommand, Debug)]
pub enum StubCommand {
    /// Not yet implemented
    #[command(name = "_stub", hide = true)]
    Stub,
}
