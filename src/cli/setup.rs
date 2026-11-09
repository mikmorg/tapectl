//! `tapectl setup` — the guided first run for one person, one drive, their own
//! account (ADR-0012 items 227, 229, 231). The steps are `crate::setup`; this
//! module is its command line.

use clap::Args;

use crate::error::Result;

/// The end of `tapectl setup --help`: examples and the exit codes.
pub const SETUP_AFTER_LONG_HELP: &str = "\
EXAMPLES:
  Answer the questions one at a time (Enter takes the default in brackets):
    tapectl setup

  Take every default, naming what has none:
    tapectl setup --yes --staging /srv/staging --device /dev/tape/by-id/<DRIVE>-nst \\
        --collection-root /media/photos

  See what each step would do, and change nothing:
    tapectl setup --dry-run

Exit status: 0 = every step is done (a second run on a finished home says so and
changes nothing); 2 = a step was refused or failed: what was done before it stays
done, and running setup again finishes the rest.";

/// `tapectl setup`'s answers: every question has its flag. The global
/// `--yes` takes every default, and the global `--dry-run` previews.
#[derive(Args, Debug, Clone, Default)]
pub struct SetupArgs {
    /// Directory for staged slices: ideally a big disk that is not the source
    ///
    /// Created when it does not exist (setup asks first). Default: the
    /// staging directory config.toml already names, `<home>/staging` in a new
    /// home.
    #[arg(long, value_name = "DIR")]
    pub staging: Option<String>,

    /// The tape drive's node, preferably its /dev/tape/by-id/…-nst link
    ///
    /// Default: the one drive under /dev/tape/by-id/; with several, setup asks
    /// which. A /dev/nstN is written as the by-id link that names it, when
    /// one does. Only the drive's identity is read (from the kernel's
    /// records); the tape is never opened.
    #[arg(long, value_name = "PATH")]
    pub device: Option<String>,

    /// The generation the drive writes, e.g. LTO-6 (default: its model's)
    ///
    /// As for `drive add`: needed when the drive is not attached now or its
    /// model is not one tapectl knows.
    #[arg(long, value_name = "GEN")]
    pub generation: Option<String>,

    /// The operator's name (default: this account's login name)
    #[arg(long, value_name = "NAME")]
    pub operator: Option<String>,

    /// Adopt this escrow public key instead of minting a new escrow identity
    ///
    /// An age1… literal, or a path to a .pub file, as for `init
    /// --escrow-public-key`. Left out, setup mints one and writes its secret
    /// to one file in the home, to be moved offline and deleted.
    #[arg(long, value_name = "KEY_OR_FILE")]
    pub escrow_public_key: Option<String>,

    /// A place cartridges are kept; give it twice (default: home, offsite)
    #[arg(long = "location", value_name = "NAME")]
    pub locations: Vec<String>,

    /// The folder whose subfolders are the units, e.g. /media/photos
    ///
    /// The first collection's root. It has no default: under --yes, setup
    /// refuses without it.
    #[arg(long, value_name = "DIR")]
    pub collection_root: Option<String>,

    /// The first collection's name (default: its folder's name)
    #[arg(long, value_name = "NAME")]
    pub collection: Option<String>,

    /// The tenant the collection's units belong to (default: the operator)
    ///
    /// Added when it does not exist.
    #[arg(long, value_name = "NAME")]
    pub tenant: Option<String>,

    /// Directory for the weekly catalog backup, best on another disk
    ///
    /// Default: ~/tapectl-backups (setup warns when it is on the home's disk).
    #[arg(long, value_name = "DIR")]
    pub backup_dir: Option<String>,

    /// Write no systemd user timers for the backup and the audit
    #[arg(long)]
    pub no_timers: bool,
}

/// Run `tapectl setup` for `startup`'s home.
pub fn run(
    startup: &crate::startup::Startup,
    args: &SetupArgs,
    json_output: bool,
    dry_run: bool,
    assume_yes: bool,
) -> Result<()> {
    crate::setup::run(
        startup,
        args,
        json_output,
        crate::setup::Mode::choose(dry_run, assume_yes),
        &crate::setup::Facts::from_env(),
        &mut std::io::stdin().lock(),
    )
}
