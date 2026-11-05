pub mod archive_set;
pub mod audit;
pub mod backend;
pub mod cartridge;
pub mod catalog;
pub mod collection;
pub mod config;
pub mod consent;
pub mod db;
pub mod drive;
pub mod host;
pub mod key;
pub mod location;
pub mod operations;
pub mod renamed;
pub mod report;
pub mod restore;
pub mod snapshot;
pub mod stage;
pub mod staging;
pub mod status;
pub mod tenant;
pub mod unit;
pub mod volume;

use clap::{Parser, Subcommand};

// `--version` prints the build identity (`build_info::VERSION`: package
// version, commit, build day), not the bare package version — ADR-0012,
// 2026-09-24 amendment, item 1. `examples/gen_man.rs` overrides it back to
// the package version so the committed man pages do not churn per commit.
// (A `///` comment here would become clap's long_about and land in the man
// page's DESCRIPTION — keep this a plain comment.)
/// tapectl — Multi-Tenant Archival Storage Management System
#[derive(Parser, Debug)]
#[command(
    name = "tapectl",
    version = crate::build_info::VERSION,
    about,
    after_long_help = ROOT_AFTER_LONG_HELP
)]
pub struct Cli {
    /// Output in JSON format, errors included
    ///
    /// Under it an error is one JSON document on stdout, `{"error": …,
    /// "exit_code": N}`, and the exit code is the command's own.
    #[arg(long, global = true, help_heading = GLOBAL_OPTIONS)]
    pub json: bool,

    /// Show what would be done without making changes
    ///
    /// Commands that cannot preview refuse the flag rather than ignore it.
    #[arg(long, short = 'n', global = true, help_heading = GLOBAL_OPTIONS)]
    pub dry_run: bool,

    /// Debug tracing on stderr
    ///
    /// Raises the log level to DEBUG (never lowers a `[logging] level` set
    /// higher); it never changes what a command does or prints on stdout.
    #[arg(long, short, global = true, help_heading = GLOBAL_OPTIONS)]
    pub verbose: bool,

    /// No live progress on stderr
    ///
    /// Long operations (stage create, volume write/resume/verify/read-slices,
    /// restore) still write their session log under <home>/logs/.
    #[arg(long, short, global = true, help_heading = GLOBAL_OPTIONS)]
    pub quiet: bool,

    /// Answer yes to the confirmation prompts of risks you may accept
    ///
    /// It never reaches a refusal: what is refused is a fact, not a risk to
    /// accept (ADR-0008, Tier 3), and a command's own --force names what it
    /// overrides.
    #[arg(long, short, global = true, help_heading = GLOBAL_OPTIONS)]
    pub yes: bool,

    /// Path to config file
    ///
    /// NOTE: on its own this ALSO relocates the whole tapectl home to the
    /// config file's parent directory — database, keys, staging, stage
    /// reports.
    /// That is how every test harness gets an isolated home, so it still
    /// works, but it is surprising enough that it now warns. Use --home
    /// when you mean "operate on a different archive", and --config only
    /// to point at a config file inside that home.
    #[arg(long, global = true, help_heading = GLOBAL_OPTIONS)]
    pub config: Option<String>,

    /// tapectl home directory: database, keys, catalogs, stage-reports, logs
    ///
    /// The home is the first of: --home; TAPECTL_HOME; the directory of
    /// --config; this account's ~/.tapectl, when it exists; the home named by
    /// the home pointer /etc/tapectl/home (TAPECTL_HOME_POINTER names another
    /// file), when the account has no ~/.tapectl of its own; else
    /// ~/.tapectl, where `tapectl init` makes a new one. The config file is
    /// <home>/config.toml unless --config names another. `tapectl status`
    /// and `tapectl config check` name the home in use and the rule that
    /// chose it.
    ///
    /// As root the home pointer is never read, and on a host that has one
    /// tapectl refuses unless --home or TAPECTL_HOME names the home: the
    /// service user's commands run through `tapectl-op`.
    #[arg(long, global = true, help_heading = GLOBAL_OPTIONS)]
    pub home: Option<String>,

    #[command(subcommand)]
    pub command: Commands,
}

/// The heading the global options are listed under, after a command's own
/// (ADR-0012 item 211).
pub const GLOBAL_OPTIONS: &str = "Global options";

/// The end of `tapectl --help`: the exit codes every command shares, and
/// where a command's own are (ADR-0012 item 212).
const ROOT_AFTER_LONG_HELP: &str = "\
Exit status: 0 = done; 2 = an error, a usage error included; 75 = the catalog was busy, run it \
again. A command with codes of its own (audit, host check, config check, db fsck, volume verify, \
cartridge erase, the commands that write a tape, the drive commands) names them in its own \
--help, and the CLI reference has every code in one table.";

/// The top-level help's groups: by job, in workflow order (ADR-0012 item
/// 211). Every visible top-level command is in exactly one group and no
/// hidden one is in any (`tests/help_text.rs` holds it to the tree); the help
/// lists them in this order.
pub const HELP_GROUPS: &[(&str, &[&str])] = &[
    (
        "Set up",
        &[
            "init",
            "tenant",
            "key",
            "location",
            "archive-set",
            "config",
            "host",
        ],
    ),
    ("Sources", &["collection", "unit", "snapshot"]),
    ("Stage and write", &["stage", "volume", "quick-archive"]),
    ("Check", &["status", "audit", "report"]),
    ("Restore", &["restore"]),
    ("Drive and cartridges", &["drive", "cartridge"]),
    (
        "Catalog and maintenance",
        &["catalog", "db", "completions", "help"],
    ),
];

/// The command line tapectl parses: the derived [`Cli`] with its top-level
/// help grouped by [`HELP_GROUPS`]. The binary parses with this, so
/// `tapectl --help`, `tapectl -h` and `tapectl help` show the groups.
pub fn command() -> clap::Command {
    use clap::CommandFactory;
    let mut cmd = Cli::command();
    // Built first, so clap's own `help` command is there to be listed.
    cmd.build();
    let template = grouped_help_template(&cmd);
    cmd.help_template(template)
}

/// Parse `args` (argv, the program name first) as the binary does, with
/// [`command`]'s grouped help.
pub fn try_parse_from(args: &[std::ffi::OsString]) -> Result<Cli, clap::Error> {
    use clap::FromArgMatches;
    let mut cmd = command();
    let mut matches = cmd.try_get_matches_from_mut(args)?;
    Cli::from_arg_matches_mut(&mut matches).map_err(|e| e.format(&mut cmd))
}

/// Whether `args` (argv, the program name first) asks for `--json`, read
/// from the line itself: for the errors raised before clap has parsed it, a
/// removed spelling's refusal and clap's own usage error, which under
/// `--json` are JSON documents too (ADR-0012 item 212, OQ7). Nothing after
/// `--` is read; once the line parses, `Cli::json` decides.
pub fn json_requested(args: &[std::ffi::OsString]) -> bool {
    args.iter()
        .skip(1)
        .take_while(|a| a.as_os_str() != "--")
        .any(|a| a.as_os_str() == "--json")
}

/// The text of clap's usage error `err` for its JSON document: clap's own
/// message without its `error: ` prefix, the usage and the hint after it
/// kept. A command that needs a subcommand says so first (clap's text for
/// it is the command's whole help).
pub fn usage_error_text(err: &clap::Error) -> String {
    let rendered = err.render().to_string();
    let text = rendered.trim();
    let text = text.strip_prefix("error: ").unwrap_or(text);
    if err.kind() == clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand {
        let usage = text
            .lines()
            .find(|l| l.starts_with("Usage:"))
            .unwrap_or("Usage:");
        return format!("a subcommand is required ({usage}); --help lists them");
    }
    text.to_string()
}

/// clap's default help template with the command list replaced by
/// [`HELP_GROUPS`], each command with its one-line about, and every option
/// (all of them global at the top level, `-h` and `-V` with them) under
/// [`GLOBAL_OPTIONS`].
fn grouped_help_template(cmd: &clap::Command) -> String {
    let styles = cmd.get_styles();
    let header = styles.get_header();
    let literal = styles.get_literal();
    let width = HELP_GROUPS
        .iter()
        .flat_map(|(_, names)| names.iter())
        .map(|n| n.len())
        .max()
        .unwrap_or(0);
    let mut groups = String::new();
    for (heading, names) in HELP_GROUPS {
        groups.push_str(&format!("{header}{heading}:{header:#}\n"));
        for name in *names {
            let about = cmd
                .find_subcommand(name)
                .and_then(|sub| sub.get_about())
                .map(|about| about.to_string())
                .unwrap_or_default();
            let pad = width - name.len();
            groups.push_str(&format!(
                "  {literal}{name}{literal:#}{:pad$}  {about}\n",
                ""
            ));
        }
        groups.push('\n');
    }
    format!(
        "{{before-help}}{{about-with-newline}}\n{{usage-heading}} {{usage}}\n\n{groups}\
         {header}{GLOBAL_OPTIONS}:{header:#}\n{{options}}{{after-help}}"
    )
}

/// The command tree shell completions are generated from: [`command`]
/// without the hidden stubs of the spellings 1.9.0 removed (`backend`,
/// `staging`, `export`, `import`, `report pending`, …), which completion would
/// otherwise offer (clap_complete lists hidden commands too). Rebuilt from
/// the visible commands, keeping what completion reads: names, abouts, the
/// arguments and their groups.
pub fn completion_command() -> clap::Command {
    use clap::CommandFactory;
    // clap takes a name as `&'static str` (without its `string` feature):
    // leaked, once per name, by a command that prints a script and exits.
    fn leak(s: &str) -> &'static str {
        Box::leak(s.to_owned().into_boxed_str())
    }
    fn visible(cmd: &clap::Command) -> clap::Command {
        let mut out = clap::Command::new(leak(cmd.get_name()))
            .args(cmd.get_arguments().cloned())
            .groups(cmd.get_groups().cloned())
            .subcommands(
                cmd.get_subcommands()
                    .filter(|sub| !sub.is_hide_set())
                    .map(visible),
            );
        if let Some(about) = cmd.get_about() {
            out = out.about(about.clone());
        }
        if let Some(about) = cmd.get_long_about() {
            out = out.long_about(about.clone());
        }
        if let Some(version) = cmd.get_version() {
            out = out.version(leak(version));
        }
        if cmd.is_disable_help_subcommand_set() {
            out = out.disable_help_subcommand(true);
        }
        if cmd.is_disable_help_flag_set() {
            out = out.disable_help_flag(true);
        }
        out
    }
    visible(&Cli::command())
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Make a new archive: catalog, config, operator tenant, keys, escrow
    ///
    /// Creates the database, config.toml, the operator tenant with its keys,
    /// and the permanent escrow recipient (ADR-0005), whose secret it prints
    /// once. --no-escrow skips the escrow recipient; --escrow-public-key
    /// adopts an existing one instead of minting a new identity. It ends with
    /// the next steps.
    Init {
        /// Name of the operator tenant (default: the login name)
        ///
        /// Defaults to the login name ($USER), except under a system account
        /// (a uid below UID_MIN in /etc/login.defs, such as a `tapectl`
        /// service user), where it is required: `init` refuses rather than
        /// name the operator after the account.
        #[arg(long)]
        operator: Option<String>,
        /// Do not create the permanent escrow recipient now
        ///
        /// By default `init` generates it (ADR-0005) and prints its secret
        /// once. Use this only when you will adopt an existing escrow
        /// identity with `key import --escrow` instead, or in tests and
        /// tooling that register escrow separately.
        #[arg(long)]
        no_escrow: bool,
        /// Adopt this escrow public key instead of minting a new identity
        ///
        /// An age1… literal, or a path to a .pub file. The disaster-recovery
        /// form: a rebuilt machine adopts the original recipient from the heir
        /// kit's cover sheet so every tape's receipts keep matching.
        #[arg(long, value_name = "KEY_OR_FILE", conflicts_with = "no_escrow")]
        escrow_public_key: Option<String>,
        /// Adopt this age identity file as the operator's primary key
        ///
        /// The SECRET, used instead of minting a pair, as `tenant add
        /// --adopt-key` does for a tenant: stored byte for byte as
        /// keys/<operator>-primary.age.key, mode 0600, and nothing is minted.
        /// A path only, never a key on the command line. Refused before
        /// anything is created when it is not one X25519 identity, or when it
        /// is the secret of --escrow-public-key.
        #[arg(long, value_name = "PATH")]
        operator_key: Option<std::path::PathBuf>,
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

    /// Manage collections: folders of units, written in batches
    ///
    /// A collection is a source root whose folders are units (one folder, one
    /// unit); `collection run` stages and writes them a batch at a time.
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

    /// Removed in 1.9.0: `stage clean` and `stage list` (ADR-0012 item 209)
    #[command(hide = true, disable_help_flag = true, disable_help_subcommand = true)]
    Staging {
        #[command(subcommand)]
        command: StagingStub,
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
    ///
    /// Exit status: 0 = clean. 1 = warnings only. 2 = at least one
    /// violation — and only that. 70 = the audit stopped on an error and
    /// reached no verdict (a database or config error, an unknown --unit, a
    /// mistyped command line). 75 = the catalog was busy; run it again.
    Audit {
        /// Show remediation commands
        #[arg(long)]
        action_plan: bool,
        /// Filter to a specific unit
        #[arg(long)]
        unit: Option<String>,
    },

    /// Browse and search the file catalog
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

    /// Removed in 1.9.0: `unit export` (ADR-0012 item 209)
    #[command(hide = true, disable_help_flag = true)]
    Export(renamed::Removed),

    /// Removed in 1.9.0: `volume import` (ADR-0012 item 209)
    #[command(hide = true, disable_help_flag = true)]
    Import(renamed::Removed),

    /// Quick archive: create + stage + write in one flow
    ///
    /// The volume must already exist — run `tapectl volume init LABEL
    /// --device /dev/tape/by-id/<drive>-nst` first. This command creates the
    /// unit, snapshot and stage set, but not the volume.
    ///
    /// Exit status: `volume write`'s (0 sealed and confirmed, 2 nothing
    /// written, 3 confirm inconclusive, 4 interrupted, 5 aborted, 6
    /// quarantined, 75 catalog busy).
    QuickArchive {
        /// Path to directory
        path: String,
        /// Tenant name
        #[arg(long)]
        tenant: String,
        /// Label of an already initialized volume to write to
        ///
        /// Create it with `tapectl volume init LABEL --device ...`;
        /// quick-archive does not.
        #[arg(long)]
        volume: String,
        /// Tags
        #[arg(long, short)]
        tag: Vec<String>,
        /// Tape device (by-id path; default: the only configured drive)
        ///
        /// Required when more than one drive is configured.
        #[arg(long)]
        device: Option<String>,
        /// See `volume write --prewrite-hash`.
        #[arg(long)]
        prewrite_hash: bool,
        /// See `volume write --fill-ceiling`: this command's write only.
        #[arg(long, value_parser = crate::config::parse_fill_ceiling)]
        fill_ceiling: Option<f64>,
        /// See `volume write --full-confirm`.
        #[arg(long)]
        full_confirm: bool,
    },

    /// Removed in 1.9.0: `drive add` (ADR-0012 item 209)
    #[command(hide = true, disable_help_flag = true, disable_help_subcommand = true)]
    Backend {
        #[command(subcommand)]
        command: BackendStub,
    },

    /// The tape drive: add it, see what is loaded, eject, health, cleanings
    ///
    /// Declare one (`drive add`), what is in it now (`drive status`), eject
    /// it (`drive eject`), its health on a schedule (`drive poll`), its
    /// cleanings (`drive cleaned`).
    Drive {
        #[command(subcommand)]
        command: drive::DriveCommands,
    },

    /// Back up, check, export and inspect the catalog database
    Db {
        #[command(subcommand)]
        command: DbCommands,
    },

    /// Show, check, read and edit config.toml
    Config {
        #[command(subcommand)]
        command: ConfigCommands,
    },

    /// This host's fitness to feed a tape drive (the quiet-host check)
    Host {
        #[command(subcommand)]
        command: host::HostCommands,
    },

    /// The home screen: the home, what is running, units by state, next steps
    ///
    /// The home and why it was chosen, what is running, the units by state,
    /// the volumes due a verify, and the next commands. The home is named with the rule that chose it (see --home). What is
    /// running and how the last sessions ended come from the session logs:
    /// the live session's phase, progress, rate and ETA, and each recent
    /// session's outcome and phase timings. Then, from the catalog, the units
    /// by state (pending: never archived or dirty; staged, awaiting a write;
    /// under-copied, below their copy floor), the volumes due a verify (no
    /// full readback, or past the verify_interval_days of a unit on them),
    /// and at most three next commands: an interrupted write to resume,
    /// pending units, staged or under-copied units, a volume to verify.
    ///
    /// An account in the home's `[ops] group` reads the session logs but not
    /// the catalog: it is shown the home and the sessions, and is told that
    /// the overview needs the service user (`tapectl-op status`).
    ///
    /// Exit status: 0 = shown (the catalog read, or an `[ops] group` account
    /// shown its part); 2 = the catalog could not be read (not initialized,
    /// damaged or busy), said on stderr after the home and the sessions.
    #[command(after_long_help = "\
Examples:
  # Where things stand, and the next command
  tapectl status
  # The same as one JSON document; `next` holds the next commands
  tapectl --json status
  # From an operator account, the service user's whole view
  tapectl-op status")]
    Status {
        /// How many finished sessions to show, newest first
        ///
        /// Running sessions are always shown.
        #[arg(long, default_value_t = 5)]
        last: usize,
    },

    /// Print a shell completion script
    ///
    /// `tapectl completions bash` prints the script bash-completion loads;
    /// scripts/install-systemd.sh installs it for `tapectl` and `tapectl-op`.
    Completions {
        /// Shell to generate completions for
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
}

/// The `backend` group 1.9.0 removed: its one command is `drive add`.
#[derive(Subcommand, Debug)]
pub enum BackendStub {
    /// Removed in 1.9.0: `drive add`
    #[command(hide = true, disable_help_flag = true)]
    Add(renamed::Removed),
}

/// The `staging` group 1.9.0 removed: `stage clean` and `stage list`.
#[derive(Subcommand, Debug)]
pub enum StagingStub {
    /// Removed in 1.9.0: `stage clean`
    #[command(hide = true, disable_help_flag = true)]
    Clean(renamed::Removed),
    /// Removed in 1.9.0: `stage list`
    #[command(hide = true, disable_help_flag = true)]
    Status(renamed::Removed),
}

/// Database operations.
#[derive(Subcommand, Debug)]
pub enum DbCommands {
    /// Back up the database, and optionally the private key directory
    ///
    /// Prints the size of the copy it wrote, and of the key files when it
    /// copied them (`size_bytes` and `keys_bytes` in `--json`): the catalog
    /// only grows, and the time a backup takes grows with it.
    #[command(after_long_help = "\
Examples:
  # A dated file in a directory on another disk
  tapectl db backup /mnt/backup/
  # A file of your naming, and the private keys beside it (secret)
  tapectl db backup /mnt/usb/tapectl.db --include-keys")]
    Backup {
        /// The backup file to write, or a directory for a dated one
        ///
        /// A directory gets `tapectl-YYYYmmddTHHMMSSZ.db` (the scheduled
        /// backup's name) and must already exist. An existing file is
        /// refused, never overwritten.
        #[arg(value_name = "PATH")]
        path: String,
        /// Also copy the private key directory, beside the backup
        ///
        /// Into a directory beside the backup named for it with its extension
        /// replaced by `.keys`
        /// (`db backup /mnt/usb/tapectl.db` copies the keys to
        /// `/mnt/usb/tapectl.keys/`). Off by default — the database alone
        /// is the common backup case. Private key material copied this way
        /// must be treated as secret wherever the destination ends up (USB
        /// stick, network share, cloud-synced folder, ...).
        #[arg(long)]
        include_keys: bool,
    },
    /// Check database integrity
    ///
    /// Exit status: 0 = clean; 1 = problems found, repaired or not; 2 = the
    /// database's integrity is broken (or an error).
    Fsck {
        /// Delete rows whose foreign-key parent is missing
        ///
        /// Closes the graph in one transaction (children first, in effect);
        /// logged.
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
    /// Show database statistics, and what each table weighs
    ///
    /// The database's size, then every table's row count and size, largest
    /// first. The journals (`health_logs`, `verification_results`, `events`,
    /// the forensic journals) are never pruned, so this is how their growth
    /// is seen. Sizes are measured with SQLite's dbstat table when the build
    /// has it (the bundled build does) and estimated otherwise; the output
    /// says which (`size_method` in `--json`).
    ///
    /// Then each journal's growth over the trailing 90 days, fastest first:
    /// the rows dated in the window, the bytes they take, and that pace over
    /// a year (`journal_growth` in `--json`). The row count is exact; the
    /// bytes are derived, the bytes in use on the table's pages shared among
    /// its rows by stored length. A catalog younger than 90 days is paced
    /// over its own age, from its oldest journal row.
    Stats,
}

/// Configuration management commands.
#[derive(Subcommand, Debug)]
pub enum ConfigCommands {
    /// Show current configuration
    Show,
    /// Check config.toml: whether every command can load it, and advice
    ///
    /// Says whether every other command can load the file (`config: valid`
    /// or `config: INVALID`), names the home it checked and why that one,
    /// then gives advice that never changes the exit code.
    ///
    /// Exit status: 0 = the configuration is valid; 2 = it is invalid (or an
    /// error).
    Check,
    /// Print one key's value in force, and where it comes from
    ///
    /// The source is config.toml, a default (the key is not in the file), or
    /// derived (a drive's device_sg, from its device_tape). KEY is spelled as
    /// for `config set`, and a bare key resolves to the one key of that name:
    /// `post_warning_bytes` is `backends.lto[lto6].post_warning_bytes` when
    /// one drive is configured, and with one drive `backends.lto.KEY` names
    /// its key too. A key that could be several is refused naming each, and a
    /// misspelt one names the key meant. `--json` gives `key`, `value` (null
    /// when nothing sets it) and `source` (`config.toml`, `default` or
    /// `derived`).
    #[command(after_long_help = "\
Examples:
  tapectl config get defaults.slice_size
  # One drive configured: its key by the bare name
  tapectl config get post_warning_bytes
  # The same key, spelled in full, for a script
  tapectl --json config get 'backends.lto[lto6].post_warning_bytes'")]
    Get {
        /// The key, e.g. defaults.slice_size or post_warning_bytes
        key: String,
    },
    /// Set one key in config.toml, keeping its comments and layout
    ///
    /// KEY is a dotted path as `config check` prints it:
    /// `defaults.slice_size`, `host_check.max_load_per_cpu`. An entry of a
    /// list of tables is selected by its name or its position from 0:
    /// `collections[movies].unit_depth`, `backends.lto[0].enospc_buffer`.
    /// A missing table is created. A bare key resolves to the one key of
    /// that name (`post_warning_bytes` with one drive), and a list of tables
    /// with one entry needs no selector (`backends.lto.enospc_buffer`).
    ///
    /// VALUE is tried as a TOML number, boolean or array first, and as a
    /// string when that does not load; quote it as TOML (`'"3"'`) to force a
    /// string. The edited file is checked as every command loads it before
    /// it is written: an unknown key or a bad value is refused by name and
    /// the file is left exactly as it was. The file is replaced atomically.
    #[command(after_long_help = "\
Examples:
  tapectl config set defaults.slice_size 2G
  tapectl config set defaults.verify_interval_days 365
  # One drive configured: its key by the bare name
  tapectl config set enospc_buffer 50M
  # An entry of a list of tables, by its name
  tapectl config set 'collections[movies].unit_depth' 2")]
    Set {
        /// The key, e.g. defaults.slice_size
        key: String,
        /// The value, e.g. 2G
        #[arg(allow_hyphen_values = true)]
        value: String,
    },
    /// Add a table to a list of tables, or values to a list
    ///
    /// For a list of tables (`archive_sets`, `backends.lto`) each VALUE is
    /// one `field=value` of the new table:
    /// `config add backends.lto name=lto6 device_tape=/dev/tape/by-id/X
    /// generation=LTO-6`. Its name must not already be taken. A drive added
    /// as `backends.lto` gets the checks `drive add` makes. A collection is
    /// added with `collection add`, which checks its root, tenant and access
    /// first; `config add collections` is refused. For a list value
    /// (`defaults.global_excludes`, `collections[movies].exclude`), each
    /// VALUE is appended. Checked and written as `config set` is.
    Add {
        /// The list, e.g. defaults.global_excludes
        key: String,
        /// field=value pairs for a table, or the values to append
        ///
        /// After `--` when one begins with `-`.
        #[arg(required = true)]
        values: Vec<String>,
    },
    /// Remove a key, an entry of a list of tables, or values from a list
    ///
    /// `config remove defaults.slice_size` removes the key, so its default
    /// applies; `config remove 'backends.lto[lto6]'` removes that table;
    /// `config remove defaults.global_excludes '*.bak'` removes that value.
    /// A whole list of tables is removed one entry at a time; a collection
    /// is removed with `collection remove`, which keeps its units. Checked and
    /// written as `config set` is: removing a required key is refused. On a
    /// file that already fails to load, an edit that adds no problem goes
    /// through and names the problems left, so a broken file can be repaired
    /// one key at a time.
    Remove {
        /// The key, table or list
        key: String,
        /// Values to remove from a list (none: remove KEY itself)
        ///
        /// After `--` when one begins with `-`.
        values: Vec<String>,
    },
}

/// How the errors of `command` map to an exit code, decided before it runs
/// so the errors raised before the command's own module is reached (the
/// database, the config) map the same way. `main` asks the contract for the
/// code of whatever error the invocation returned.
///
/// - `volume verify`: [`ErrorContract::Verify`] (issue #356).
/// - `audit`: [`ErrorContract::Audit`] (issue #408).
/// - The write family — `volume write`, `volume resume`, `volume
///   compact-write`, `volume compact`, `collection run`, `quick-archive`:
///   [`ErrorContract::Write`] (ADR-0012, 2026-10-07 item 20).
/// - `cartridge erase`: [`ErrorContract::Erase`] (ADR-0012 2026-10-12).
/// - Everything else: [`ErrorContract::Ordinary`].
///
/// [`ErrorContract::Erase`]: crate::error::ErrorContract::Erase
/// [`ErrorContract::Verify`]: crate::error::ErrorContract::Verify
/// [`ErrorContract::Audit`]: crate::error::ErrorContract::Audit
/// [`ErrorContract::Write`]: crate::error::ErrorContract::Write
/// [`ErrorContract::Ordinary`]: crate::error::ErrorContract::Ordinary
pub fn error_contract(command: &Commands) -> crate::error::ErrorContract {
    use crate::error::ErrorContract;
    match command {
        Commands::Volume { command } => volume::error_contract(command),
        Commands::Audit { .. } => ErrorContract::Audit,
        Commands::Collection {
            command: collection::CollectionCommands::Run { .. },
        }
        | Commands::QuickArchive { .. } => ErrorContract::Write,
        Commands::Cartridge {
            command: cartridge::CartridgeCommands::Erase { .. },
        } => ErrorContract::Erase,
        _ => ErrorContract::Ordinary,
    }
}

/// The error contract of the command `args` (argv, program name first)
/// names, whether or not the rest of it parses — for a command line that
/// did not parse, which leaves no [`Commands`] to ask. The same arguments
/// are parsed again leniently (`ignore_errors`), which keeps the subcommand
/// chain clap had matched before it hit the error. Scanning argv by hand
/// instead would have to know which global flags take a value (`--home X
/// volume verify`); the lenient parse knows because it IS the definition.
/// An invocation it cannot place (`volume verfy`, `audt`) is
/// [`crate::error::ErrorContract::Ordinary`].
pub fn invocation_contract(args: &[std::ffi::OsString]) -> crate::error::ErrorContract {
    use crate::error::ErrorContract;
    use clap::CommandFactory;
    let Ok(matches) = Cli::command()
        .ignore_errors(true)
        .try_get_matches_from(args)
    else {
        return ErrorContract::Ordinary;
    };
    match matches.subcommand() {
        Some(("volume", volume)) => match volume.subcommand_name() {
            Some("verify") => ErrorContract::Verify,
            Some("write" | "resume" | "compact-write" | "compact" | "rewrite") => {
                ErrorContract::Write
            }
            _ => ErrorContract::Ordinary,
        },
        Some(("collection", c)) if c.subcommand_name() == Some("run") => ErrorContract::Write,
        Some(("quick-archive", _)) => ErrorContract::Write,
        Some(("cartridge", c)) if c.subcommand_name() == Some("erase") => ErrorContract::Erase,
        Some(("audit", _)) => ErrorContract::Audit,
        _ => ErrorContract::Ordinary,
    }
}

/// The name of the progress session `command` opens (issue #386), or `None`
/// for a command short enough to need none. It names the session log
/// (`<home>/logs/<UTC>-<name>-<pid>.log`) and heads its first line.
pub fn progress_session_name(command: &Commands) -> Option<String> {
    use collection::CollectionCommands as C;
    use restore::RestoreCommands as R;
    use stage::StageCommands as S;
    use volume::VolumeCommands as V;
    Some(match command {
        // Issue #427: one unit by name, several as the first and a count.
        // Joining every name made a 27-unit stage's log name too long for
        // the filesystem, and the run got no session log at all.
        Commands::Stage {
            command: S::Create { name, .. },
        } => match name.as_slice() {
            [] => "stage create".to_string(),
            [one] => format!("stage create {one}"),
            [first, rest @ ..] => format!("stage create {first} +{} more", rest.len()),
        },
        Commands::QuickArchive { volume, .. } => format!("quick-archive {volume}"),
        Commands::Collection {
            command: C::Run { collection, .. },
        } => match collection {
            Some(name) => format!("collection run {name}"),
            None => "collection run".to_string(),
        },
        Commands::Volume { command } => match command {
            V::Init { label, .. } => format!("volume init {label}"),
            V::Write { label, .. } => {
                format!("volume write {}", label.as_deref().unwrap_or("loaded"))
            }
            V::Resume { label, .. } => {
                format!("volume resume {}", label.as_deref().unwrap_or("loaded"))
            }
            V::Verify { label, .. } => {
                format!("volume verify {}", label.as_deref().unwrap_or("loaded"))
            }
            V::ReadSlices { from, .. } => format!("volume read-slices {from}"),
            V::CompactRead { label, .. } => format!("volume compact-read {label}"),
            V::CompactWrite { destination, .. } => {
                format!("volume compact-write {destination}")
            }
            V::Compact { label, .. } => format!("volume compact {label}"),
            V::Rewrite {
                source,
                label,
                resume,
                ..
            } => match (resume, source, label) {
                (Some(new), _, _) => format!("volume rewrite --resume {new}"),
                (None, Some(source), _) => format!("volume rewrite {source}"),
                _ => "volume rewrite".to_string(),
            },
            _ => return None,
        },
        Commands::Restore { command } => match command {
            // A dry run reads nothing and opens no session.
            R::Unit { dry_run: true, .. } => return None,
            R::Unit { unit, .. } => format!("restore unit {}", unit.name()),
            R::Volume { dry_run: true, .. } => return None,
            R::Volume { label, .. } => {
                format!("restore volume {}", label.as_deref().unwrap_or("loaded"))
            }
            R::File { unit, .. } => format!("restore file {}", unit.name()),
            R::RawVolume { .. } => "restore raw-volume".to_string(),
        },
        // A long erase runs for hours on LTO (ADR-0012 2026-10-12 item 52).
        Commands::Cartridge {
            command: cartridge::CartridgeCommands::Erase { .. },
        } => "cartridge erase".to_string(),
        _ => return None,
    })
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
/// work on the rebuilt machine that has keys and no `drive add` yet
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

    /// `tapectl completions SHELL`'s script, as `main` generates it.
    fn completion_script(shell: clap_complete::Shell) -> String {
        let mut script = Vec::new();
        clap_complete::generate(shell, &mut completion_command(), "tapectl", &mut script);
        String::from_utf8(script).unwrap()
    }

    /// Every hidden command of the tree, as its words (`["report", "pending"]`).
    fn hidden_paths() -> Vec<Vec<String>> {
        fn walk(cmd: &clap::Command, path: &[String], out: &mut Vec<Vec<String>>) {
            for sub in cmd.get_subcommands() {
                let mut p = path.to_vec();
                p.push(sub.get_name().to_string());
                if sub.is_hide_set() {
                    out.push(p);
                } else {
                    walk(sub, &p, out);
                }
            }
        }
        let mut out = Vec::new();
        use clap::CommandFactory;
        walk(&Cli::command(), &[], &mut out);
        out
    }

    /// Whether `script`, `shell`'s completion script, offers the command
    /// `path` (its words) after its parent: bash's case arm for it, zsh's
    /// entry in its parent's list of commands, fish's candidate after its
    /// parent.
    fn offers(shell: clap_complete::Shell, script: &str, path: &[&str]) -> bool {
        let (parent, name) = path.split_at(path.len() - 1);
        let name = name[0];
        match shell {
            clap_complete::Shell::Bash => {
                let parent: Vec<String> = std::iter::once("tapectl".to_string())
                    .chain(parent.iter().map(|w| w.replace('-', "__")))
                    .collect();
                script.contains(&format!("{},{name})", parent.join("__")))
            }
            clap_complete::Shell::Zsh => {
                let list: String = parent.iter().map(|w| format!("__{w}")).collect();
                let head = format!("_tapectl{list}_commands() {{");
                script.split(&head).nth(1).is_some_and(|body| {
                    let body = &body[..body.find("\n}").unwrap_or(body.len())];
                    body.contains(&format!("\n'{name}:"))
                })
            }
            clap_complete::Shell::Fish => {
                let after = match parent {
                    [] => "__fish_tapectl_needs_command\"".to_string(),
                    [first, ..] => format!("__fish_tapectl_using_subcommand {first};"),
                };
                let mut lines = script.lines().filter(|l| l.contains(&after));
                if let [_, second] = parent {
                    lines.any(|l| {
                        l.contains(&format!("__fish_seen_subcommand_from {second};"))
                            && l.contains(&format!("-a \"{name}\""))
                    })
                } else {
                    lines.any(|l| l.contains(&format!("-a \"{name}\"")))
                }
            }
            _ => unreachable!("only bash, zsh and fish are checked"),
        }
    }

    /// ADR-0012 item 203's residual: completion offers no hidden stub of a
    /// spelling 1.9.0 removed, in any shell, while it still offers the live
    /// commands, some of the same names among them (the positive control:
    /// the same search finds a command that must be there).
    #[test]
    fn completions_offer_no_hidden_stub() {
        let hidden = hidden_paths();
        for stub in [&["backend"][..], &["staging"], &["export"], &["import"]] {
            assert!(
                hidden.iter().any(|h| h == stub),
                "{stub:?} is no longer a hidden stub: update this test"
            );
        }
        assert!(hidden.iter().any(|h| h == &["report", "pending"]));
        let live: &[&[&str]] = &[
            &["drive"],
            &["status"],
            &["drive", "add"],
            &["drive", "status"],
            &["stage", "list"],
            &["unit", "export"],
            &["volume", "import"],
            &["drive", "cleaning-cartridge", "add"],
        ];
        for shell in [
            clap_complete::Shell::Bash,
            clap_complete::Shell::Zsh,
            clap_complete::Shell::Fish,
        ] {
            let script = completion_script(shell);
            // The negative's positive control: the same search finds the
            // stubs in a script generated from the whole tree.
            let mut whole = Vec::new();
            {
                use clap::CommandFactory;
                clap_complete::generate(shell, &mut Cli::command(), "tapectl", &mut whole);
            }
            let whole = String::from_utf8(whole).unwrap();
            for stub in [&["backend"][..], &["report", "pending"], &["staging", "clean"]] {
                assert!(offers(shell, &whole, stub), "{shell}: the search misses {stub:?}");
            }
            for path in live {
                assert!(offers(shell, &script, path), "{shell} lacks {path:?}");
            }
            for path in &hidden {
                let path: Vec<&str> = path.iter().map(String::as_str).collect();
                assert!(
                    !offers(shell, &script, &path),
                    "{shell} offers the hidden {path:?}"
                );
            }
        }
    }

    /// OQ7: `--json` is read from the line before clap parses it, wherever
    /// it stands, and never after `--`.
    #[test]
    fn json_is_read_from_the_line_before_clap() {
        let line = |a: &[&str]| -> Vec<std::ffi::OsString> {
            std::iter::once("tapectl")
                .chain(a.iter().copied())
                .map(Into::into)
                .collect()
        };
        assert!(json_requested(&line(&["--json", "volume", "info"])));
        assert!(json_requested(&line(&["volume", "info", "L", "--json"])));
        assert!(!json_requested(&line(&["volume", "info", "L"])));
        assert!(!json_requested(&line(&[
            "catalog", "search", "--", "--json"
        ])));
        assert!(!json_requested(&line(&["--jsonx"])));
    }

    /// Issue #427: a several-unit `stage create` names its session by the
    /// first unit and a count, never by every unit.
    #[test]
    fn a_several_unit_stage_names_its_session_by_first_unit_and_count() {
        use clap::Parser;
        let name = |args: &[&str]| {
            let cli = Cli::try_parse_from(std::iter::once("tapectl").chain(args.iter().copied()))
                .unwrap();
            progress_session_name(&cli.command)
        };
        assert_eq!(
            name(&["stage", "create", "keepsake/a"]).as_deref(),
            Some("stage create keepsake/a")
        );
        assert_eq!(
            name(&["stage", "create", "keepsake/a", "keepsake/b", "keepsake/c"]).as_deref(),
            Some("stage create keepsake/a +2 more")
        );
        // Issue #483: a rewrite is hours of tape; it has a session log, named
        // for the volume it rewrites or the one it resumes.
        assert_eq!(
            name(&["volume", "rewrite", "L6-0001", "--label", "L6-0009"]).as_deref(),
            Some("volume rewrite L6-0001")
        );
        assert_eq!(
            name(&["volume", "rewrite", "--resume", "L6-0009"]).as_deref(),
            Some("volume rewrite --resume L6-0009")
        );
    }

    /// ADR-0012, 2026-10-07 items 19 and 20 (issue #408): which commands'
    /// errors take which exit contract — from a parsed command line, and
    /// from one that did not parse (the lenient reparse).
    #[test]
    fn each_command_takes_its_error_contract() {
        use crate::error::ErrorContract::{self, *};
        let cases: &[(&[&str], ErrorContract)] = &[
            (&["volume", "write", "L"], Write),
            (&["volume", "resume", "L"], Write),
            (&["volume", "compact-write", "--to", "L"], Write),
            (&["volume", "compact", "S", "--to", "L"], Write),
            (&["volume", "rewrite", "S", "--label", "L"], Write),
            (&["volume", "rewrite", "--resume", "L"], Write),
            (&["collection", "run", "c", "--label", "L"], Write),
            (&["collection", "run", "--label", "L"], Write),
            (
                &["quick-archive", "/src", "--tenant", "t", "--volume", "L"],
                Write,
            ),
            (&["volume", "verify", "L"], Verify),
            (&["audit"], Audit),
            (&["cartridge", "erase"], Erase),
            (
                &["cartridge", "erase", "--long", "--confirm-label", "L"],
                Erase,
            ),
            (&["cartridge", "mark-erased", "B"], Ordinary),
            (&["volume", "init", "L"], Ordinary),
            (&["volume", "compact-read", "L"], Ordinary),
            (&["volume", "compact-finish", "L"], Ordinary),
            (&["collection", "plan"], Ordinary),
            (&["report", "reclaimable"], Ordinary),
            (&["status"], Ordinary),
        ];
        for (args, want) in cases {
            let argv: Vec<&str> = std::iter::once("tapectl")
                .chain(args.iter().copied())
                .collect();
            let cli = Cli::try_parse_from(&argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
            assert_eq!(error_contract(&cli.command), *want, "{argv:?}");
            let os: Vec<std::ffi::OsString> = argv.iter().map(Into::into).collect();
            assert_eq!(invocation_contract(&os), *want, "{argv:?} reparsed");
        }
        // A line that does not parse past the subcommand still names it.
        let os = |a: &[&str]| -> Vec<std::ffi::OsString> { a.iter().map(Into::into).collect() };
        assert_eq!(
            invocation_contract(&os(&["argv0", "volume", "write", "--bogus"])),
            Write
        );
        assert_eq!(
            invocation_contract(&os(&["argv0", "volume", "wirte", "L"])),
            Ordinary
        );
    }

    /// Issue #169: `volume import --generation` lost its `"LTO-6"` default and is
    /// now a plain required `clap` field, so omitting it is a usage error
    /// clap itself enforces — never a value tapectl silently makes up. This
    /// is asserted at the `clap` layer (`Cli::try_parse_from`) rather than
    /// via `tests/cli_smoke.rs`'s process harness, since that file is
    /// outside this change's scope fence.
    #[test]
    fn import_without_generation_is_a_usage_error() {
        let result = Cli::try_parse_from(["tapectl", "volume", "import", "L1"]);
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
        let result =
            Cli::try_parse_from(["tapectl", "volume", "import", "L1", "--generation", "LTO-6"]);
        result.expect("omitting --capacity must still parse");
    }

    /// ADR-0012 2026-10-07 item 31: `collection run --full-confirm-first`
    /// reads the first copy back in full and the others quickly, and
    /// `--full-confirm` reads every copy; asking for both is a usage error,
    /// not a silent pick of one.
    #[test]
    fn collection_run_full_confirm_first_parses_and_conflicts_with_full_confirm() {
        let run = |extra: &[&str]| {
            let mut argv = vec!["tapectl", "collection", "run", "lib", "--label", "L1"];
            argv.extend_from_slice(extra);
            Cli::try_parse_from(argv)
        };
        let confirm_of = |cli: Cli| match cli.command {
            Commands::Collection {
                command: crate::cli::collection::CollectionCommands::Run { confirm, .. },
            } => confirm,
            other => panic!("not collection run: {other:?}"),
        };
        use crate::collection::batch::RunConfirm;
        assert_eq!(confirm_of(run(&[]).unwrap()).confirm(), RunConfirm::Quick);
        assert_eq!(
            confirm_of(run(&["--full-confirm"]).unwrap()).confirm(),
            RunConfirm::FullEvery
        );
        assert_eq!(
            confirm_of(run(&["--full-confirm-first"]).unwrap()).confirm(),
            RunConfirm::FullFirst
        );
        let err =
            run(&["--full-confirm", "--full-confirm-first"]).expect_err("the two flags conflict");
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::ArgumentConflict,
            "{err}"
        );
    }

    /// ADR-0012 item 210: `collection run [NAME]` takes one `--label`. The
    /// collection is optional (the only one configured, else named); a second
    /// `--label` is a usage error from clap that names the one-copy-per-run
    /// rule and how to write the further copies, and none is clap's own.
    #[test]
    fn collection_run_takes_one_label_and_an_optional_collection() {
        let parse = |args: &[&str]| {
            Cli::try_parse_from(
                ["tapectl", "collection", "run"]
                    .iter()
                    .chain(args.iter())
                    .copied(),
            )
        };
        let run_of = |cli: Cli| match cli.command {
            Commands::Collection {
                command:
                    crate::cli::collection::CollectionCommands::Run {
                        collection, label, ..
                    },
            } => (collection, label.label),
            other => panic!("not collection run: {other:?}"),
        };
        assert_eq!(
            run_of(parse(&["--label", "L1"]).unwrap()),
            (None, "L1".to_string())
        );
        assert_eq!(
            run_of(parse(&["keepsake", "--label", "L1"]).unwrap()),
            (Some("keepsake".to_string()), "L1".to_string())
        );

        for twice in [
            &["--label", "L1", "--label", "L2"][..],
            &["keepsake", "--label", "L1", "--label", "L1"][..],
        ] {
            let err = parse(twice).expect_err("a second --label is a usage error");
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::ArgumentConflict,
                "{err}"
            );
            let text = err.to_string();
            for says in [
                "collection run takes one --label",
                "more than one destination label",
                "One copy per run",
                "swap in the next cartridge",
                "tapectl volume write <NEW-LABEL> --init --full-confirm",
            ] {
                assert!(text.contains(says), "{says:?} in {text}");
            }
        }

        let err = parse(&["keepsake"]).expect_err("--label is required");
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::MissingRequiredArgument,
            "{err}"
        );
    }
}
