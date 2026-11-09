//! `tapectl setup`: the guided first run for someone with one tape drive and
//! a Linux box (ADR-0012 items 227, 229, 231).
//!
//! It runs as the person's own account, never needs root and never runs
//! `sudo`: whatever needs root is printed as one block at the end. Each step
//! composes the command that does that job (`init`'s home and keys, `config
//! set`, `drive add`, `location add`, `tenant add`, `collection add`,
//! `collection sync`, `config check`), says what it is about to do and what
//! it did, and is skipped when its work is already done, so a second run
//! finishes a first one that stopped and a run on a finished home changes
//! nothing. `scripts/first-run.sh` stays the path of a host with a service
//! user (item 228).
//!
//! The steps, in order: the home; the staging directory; the drive (its
//! identity only, from the kernel's records: the tape is never opened); the
//! operator's keys and the escrow identity; two locations; the first
//! collection; the backup and audit user timers (files only); `config
//! check`; the lines that need root; the commands for the first copies.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::cli::collection::CollectionCommands;
use crate::cli::location::LocationCommands;
use crate::cli::setup::SetupArgs;
use crate::cli::ConfigCommands;
use crate::config::{Config, TapectlPaths};
use crate::error::{Result, TapectlError};
use crate::startup::Startup;

/// The file the minted escrow secret is written to, in the home: never in
/// `keys/`, which `db backup --include-keys` copies and where the escrow
/// secret is never kept (ADR-0005).
pub const ESCROW_SECRET_FILE: &str = "escrow-secret.age.key";

/// The unit files setup writes, in the user unit directory.
pub const UNIT_FILES: [&str; 4] = [
    "tapectl-backup.service",
    "tapectl-backup.timer",
    "tapectl-audit.service",
    "tapectl-audit.timer",
];

/// The tools tapectl and the tapes' RESTORE.sh run, each with its
/// Debian/Ubuntu package.
const TOOLS: &[(&str, &str)] = &[
    ("dar", "dar"),
    ("sg_logs", "sg3-utils"),
    ("age", "age"),
    ("mt", "mt-st"),
    ("tar", "tar"),
];

/// Where the Quick start page is read.
pub const QUICK_START_URL: &str =
    "https://github.com/mikmorg/tapectl/blob/master/docs/quick-start.md";

/// How setup takes its answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Ask each question on stdin, Enter taking the default.
    Ask,
    /// `--yes`: every default, refused where a question has none.
    Yes,
    /// `--dry-run`: say what each step would do, ask nothing, change nothing.
    DryRun,
}

impl Mode {
    /// The mode the global `--dry-run` and `--yes` choose.
    pub fn choose(dry_run: bool, assume_yes: bool) -> Mode {
        match (dry_run, assume_yes) {
            (true, _) => Mode::DryRun,
            (false, true) => Mode::Yes,
            (false, false) => Mode::Ask,
        }
    }
}

/// What setup reads of the machine and the account, gathered once, so a test
/// can give its own.
#[derive(Debug, Clone)]
pub struct Facts {
    /// The account's login name.
    pub user: String,
    /// The effective uid.
    pub euid: u32,
    /// The account's home directory (`$HOME`).
    pub user_home: PathBuf,
    /// The names of the groups the account is in. `TAPECTL_TEST_SETUP_GROUPS`
    /// (comma-separated) replaces them, for tests.
    pub groups: Vec<String>,
    /// `$PATH`, where the tools are looked for.
    pub path_var: std::ffi::OsString,
    /// Where the user units are written: `TAPECTL_SETUP_UNIT_DIR`, else
    /// `${XDG_CONFIG_HOME:-~/.config}/systemd/user`.
    pub unit_dir: PathBuf,
    /// Where the drives are listed: `/dev/tape/by-id`
    /// (`TAPECTL_TEST_SETUP_BY_ID_DIR` replaces it, for tests).
    pub by_id_dir: PathBuf,
    /// The tapectl binary the timers run.
    pub tapectl_exe: PathBuf,
    /// Whether stdin is a terminal.
    pub stdin_is_terminal: bool,
    /// Whether systemd keeps the account's user manager running while it is
    /// logged out (`loginctl enable-linger`).
    pub linger: bool,
}

impl Facts {
    /// This process's facts.
    pub fn from_env() -> Facts {
        use std::io::IsTerminal;
        let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
        let euid = nix::unistd::geteuid();
        let user = std::env::var("USER")
            .ok()
            .filter(|u| !u.is_empty())
            .or_else(|| {
                nix::unistd::User::from_uid(euid)
                    .ok()
                    .flatten()
                    .map(|u| u.name)
            })
            .unwrap_or_else(|| euid.as_raw().to_string());
        let user_home = var("HOME").map(PathBuf::from).unwrap_or_default();
        let groups = match std::env::var("TAPECTL_TEST_SETUP_GROUPS") {
            Ok(list) => list
                .split(',')
                .map(str::trim)
                .filter(|g| !g.is_empty())
                .map(str::to_string)
                .collect(),
            Err(_) => account_groups(),
        };
        let unit_dir = var("TAPECTL_SETUP_UNIT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                var("XDG_CONFIG_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| user_home.join(".config"))
                    .join("systemd/user")
            });
        let by_id_dir = var("TAPECTL_TEST_SETUP_BY_ID_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/dev/tape/by-id"));
        let tapectl_exe = std::env::current_exe()
            .ok()
            .and_then(|p| std::fs::canonicalize(&p).ok().or(Some(p)))
            .unwrap_or_else(|| PathBuf::from("tapectl"));
        Facts {
            linger: Path::new("/var/lib/systemd/linger").join(&user).exists(),
            user,
            euid: euid.as_raw(),
            user_home,
            groups,
            path_var: std::env::var_os("PATH").unwrap_or_default(),
            unit_dir,
            by_id_dir,
            tapectl_exe,
            stdin_is_terminal: std::io::stdin().is_terminal(),
        }
    }
}

/// The names of the groups this process is in: its supplementary groups and
/// its effective group.
fn account_groups() -> Vec<String> {
    let mut gids = nix::unistd::getgroups().unwrap_or_default();
    gids.push(nix::unistd::getegid());
    let mut names: Vec<String> = gids
        .into_iter()
        .filter_map(|g| nix::unistd::Group::from_gid(g).ok().flatten())
        .map(|g| g.name)
        .collect();
    names.sort();
    names.dedup();
    names
}

/// `tapectl init`'s skeleton, shared by `init` and `setup`: the home's
/// directories, `config.toml` (the defaults, the staging directory at
/// `<home>/staging`, then the commented drive and quiet-host examples) and
/// the catalog with its schema. `create_staging` makes the staging
/// directory too (`init`); setup asks about it in a step of its own first.
pub fn make_home(paths: &TapectlPaths, create_staging: bool) -> Result<(Config, Connection)> {
    let other = |what: &str, e: std::io::Error| TapectlError::Other(format!("{what}: {e}"));
    paths.ensure_dirs()?;

    // Issue #140: the staging default is `<tapectl home>/staging`, and the
    // home a fresh init is actually using may be `--home`/`TAPECTL_HOME`
    // rather than the default one the serde default can see. Record the real
    // one, and create it with the same 0700 discipline as the rest of the
    // home — a default path that does not exist is what made `config
    // check`'s staging warning fire on every stock machine.
    let mut cfg = Config::default();
    let staging_dir = paths.home.join("staging");
    if create_staging {
        std::fs::create_dir_all(&staging_dir)
            .map_err(|e| other("failed to create the staging directory", e))?;
        crate::config::secure_path(&staging_dir, 0o700);
    }
    cfg.staging.directory = staging_dir.to_string_lossy().into_owned();
    cfg.save(&paths.config_file)?;
    // The commented [[backends.lto]] example goes on after serialization: an
    // empty Vec serializes to nothing, and toml round-trips drop comments, so
    // the section cannot be carried in the struct — leaving a fresh config
    // with no hint that a tape drive must be declared at all (#124). Then the
    // quiet-host keys (ADR-0012, 2026-09-24 amendment, item 7), commented out
    // so the defaults stay the defaults.
    {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&paths.config_file)
            .map_err(|e| other("failed to reopen config to append the drive example", e))?;
        f.write_all(crate::config::LTO_BACKEND_EXAMPLE.as_bytes())
            .map_err(|e| other("failed to append the drive example", e))?;
        f.write_all(crate::config::HOST_CHECK_EXAMPLE.as_bytes())
            .map_err(|e| other("failed to append the host-check example", e))?;
    }
    let conn = crate::db::open(&paths.db_file)?;
    Ok((cfg, conn))
}

/// Run every step for `startup`'s home, reading answers from `input`.
pub fn run(
    startup: &Startup,
    args: &SetupArgs,
    json_output: bool,
    mode: Mode,
    facts: &Facts,
    input: &mut dyn BufRead,
) -> Result<()> {
    if json_output {
        return Err(TapectlError::Other(
            "`tapectl setup` asks questions and talks to a person; it has no --json form. \
             Drop --json (each command it runs has its own)."
                .to_string(),
        ));
    }
    if facts.euid == 0 {
        return Err(TapectlError::Other(
            "`tapectl setup` runs as the account that will use the drive, never as root: \
             run it again as that account, without sudo. What needs root it prints for you \
             to run."
                .to_string(),
        ));
    }
    if mode == Mode::Ask && !facts.stdin_is_terminal {
        return Err(TapectlError::Other(
            "`tapectl setup` asks questions, and stdin is not a terminal: run it in a \
             terminal, or pass --yes to take every default (with the flags for the answers \
             that have none, e.g. --collection-root DIR), or --dry-run to see what it would \
             do."
            .to_string(),
        ));
    }
    let mut setup = Setup {
        startup,
        paths: &startup.paths,
        args,
        mode,
        facts,
        ask: Ask { mode, input },
        conn: None,
        changed: false,
        check_failed: false,
        timers_written: false,
        escrow_secret: None,
    };
    if mode == Mode::DryRun {
        println!("tapectl setup (DRY RUN — no changes made)");
    } else {
        println!("tapectl setup: each step says what it is about to do, then what it did.");
        if mode == Mode::Ask {
            println!("Enter takes the answer in [brackets]. Ctrl-C between steps is safe: run setup again to finish.");
        }
    }
    for n in 1..=10 {
        crate::signal::check(|| {
            "setup stopped between steps; run `tapectl setup` again to finish".to_string()
        })?;
        match n {
            1 => setup.home(),
            2 => setup.staging(),
            3 => setup.drive(),
            4 => setup.keys(),
            5 => setup.locations(),
            6 => setup.collection(),
            7 => setup.timers(),
            8 => setup.check(),
            9 => setup.root_block(),
            _ => setup.next(),
        }?;
    }
    if setup.check_failed {
        return Err(TapectlError::Other(
            "setup finished its steps, but `tapectl config check` found the configuration \
             invalid (above): fix what it names, then run `tapectl setup` again"
                .to_string(),
        ));
    }
    Ok(())
}

/// The questions, answered from stdin, by default, or (dry run) not at all.
struct Ask<'a> {
    mode: Mode,
    input: &'a mut dyn BufRead,
}

impl Ask<'_> {
    /// One line of text. `None` only in a dry run with no default.
    fn text(
        &mut self,
        question: &str,
        default: Option<&str>,
        flag: &str,
    ) -> Result<Option<String>> {
        match self.mode {
            Mode::DryRun => Ok(default.map(str::to_string)),
            Mode::Yes => match default {
                Some(d) => Ok(Some(d.to_string())),
                None => Err(TapectlError::Other(format!(
                    "setup --yes: \"{question}\" has no default; pass {flag}"
                ))),
            },
            Mode::Ask => loop {
                match default {
                    Some(d) => print!("  {question} [{d}]: "),
                    None => print!("  {question}: "),
                }
                let line = self.read_line(question, flag)?;
                match (line.as_str(), default) {
                    ("", Some(d)) => return Ok(Some(d.to_string())),
                    ("", None) => println!("  (an answer is needed here, or pass {flag})"),
                    (answer, _) => return Ok(Some(answer.to_string())),
                }
            },
        }
    }

    /// Yes or no; Enter (and `--yes`, and a dry run) takes `default`.
    fn confirm(&mut self, question: &str, default: bool) -> Result<bool> {
        if self.mode != Mode::Ask {
            return Ok(default);
        }
        loop {
            print!("  {question} [{}]: ", if default { "Y/n" } else { "y/N" });
            let line = self.read_line(question, "--yes")?;
            match line.to_ascii_lowercase().as_str() {
                "" => return Ok(default),
                "y" | "yes" => return Ok(true),
                "n" | "no" => return Ok(false),
                _ => println!("  (y or n)"),
            }
        }
    }

    fn read_line(&mut self, question: &str, flag: &str) -> Result<String> {
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let n = self.input.read_line(&mut line)?;
        crate::signal::check(|| {
            "setup stopped at a question; run `tapectl setup` again to finish".to_string()
        })?;
        if n == 0 {
            return Err(TapectlError::Other(format!(
                "setup: stdin closed at \"{question}\"; run `tapectl setup` again, or pass \
                 {flag}"
            )));
        }
        Ok(line.trim().to_string())
    }
}

struct Setup<'a> {
    startup: &'a Startup,
    paths: &'a TapectlPaths,
    args: &'a SetupArgs,
    mode: Mode,
    facts: &'a Facts,
    ask: Ask<'a>,
    conn: Option<Connection>,
    /// Whether any step changed anything.
    changed: bool,
    check_failed: bool,
    timers_written: bool,
    /// The escrow secret file this run wrote.
    escrow_secret: Option<PathBuf>,
}

fn header(n: usize, title: &str) {
    println!("\n{n}. {title}");
}

/// `~/x` as the account's home, and a relative path refused.
fn absolute_dir(answer: &str, user_home: &Path, what: &str) -> Result<PathBuf> {
    let path = match answer.strip_prefix("~/") {
        Some(rest) => user_home.join(rest),
        None if answer == "~" => user_home.to_path_buf(),
        None => PathBuf::from(answer),
    };
    if !path.is_absolute() {
        return Err(TapectlError::Other(format!(
            "{what} \"{answer}\" is not an absolute path: give the whole path, from /"
        )));
    }
    Ok(path)
}

/// Free space at `dir`, binary units, or `None` when statvfs cannot say.
fn free_space(dir: &Path) -> Option<String> {
    let stat = nix::sys::statvfs::statvfs(dir).ok()?;
    let bytes = (stat.blocks_available() as u64).saturating_mul(stat.fragment_size() as u64);
    Some(crate::util::format_bytes_binary(
        i64::try_from(bytes).unwrap_or(i64::MAX),
    ))
}

/// A drive found under `/dev/tape/by-id`.
#[derive(Debug, Clone)]
pub struct FoundDrive {
    /// The by-id link.
    pub by_id: PathBuf,
    /// What it resolves to (`/dev/nstN`).
    pub node: Option<PathBuf>,
    /// Vendor and model, from sysfs.
    pub model: Option<String>,
    /// The generation the model names.
    pub generation: Option<crate::media::Generation>,
}

impl FoundDrive {
    fn describe(&self) -> String {
        let node = self
            .node
            .as_ref()
            .map(|n| format!(" → {}", n.display()))
            .unwrap_or_default();
        let model = self.model.as_deref().unwrap_or("model unknown");
        let generation = self
            .generation
            .map(|g| format!(", {g}"))
            .unwrap_or_default();
        format!("{}{node}: {model}{generation}", self.by_id.display())
    }
}

/// The non-rewinding drives listed in `dir` (`*-nst`), each with the identity
/// the kernel recorded for it. Nothing is opened: a readlink and sysfs.
pub fn list_drives(dir: &Path) -> Vec<FoundDrive> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut drives: Vec<FoundDrive> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with("-nst"))
        .map(|e| {
            let by_id = e.path();
            let identity = crate::tape::drive_identity::attached_identity(&by_id.to_string_lossy());
            let model = identity.as_ref().and_then(|i| i.model.clone());
            let vendor = identity.as_ref().and_then(|i| i.vendor.clone());
            let serial = identity.as_ref().and_then(|i| i.serial.clone());
            let drive = FoundDrive {
                node: std::fs::canonicalize(&by_id).ok(),
                generation: model
                    .as_deref()
                    .and_then(crate::tape::drive_identity::generation_from_model),
                model: match (vendor, model) {
                    (Some(v), Some(m)) => Some(format!("{v} {m}")),
                    (None, m) => m,
                    (v, None) => v,
                },
                by_id,
            };
            (drive, serial)
        })
        .fold(
            Vec::<(FoundDrive, Option<String>)>::new(),
            |mut acc, (d, serial)| {
                // udev links one drive under several names (its serial's, its
                // WWN's): one entry per node, by the name that carries the
                // serial when one does.
                let names_serial = |d: &FoundDrive| {
                    serial.as_deref().is_some_and(|s| {
                        d.by_id
                            .file_name()
                            .is_some_and(|n| n.to_string_lossy().contains(s))
                    })
                };
                match acc
                    .iter_mut()
                    .find(|(seen, _)| seen.node.is_some() && seen.node == d.node)
                {
                    Some((seen, _)) => {
                        if names_serial(&d) && !names_serial(seen) {
                            *seen = d;
                        }
                    }
                    None => acc.push((d, serial)),
                }
                acc
            },
        )
        .into_iter()
        .map(|(d, _)| d)
        .collect();
    drives.sort_by(|a, b| a.by_id.cmp(&b.by_id));
    drives
}

/// The drive's config name: `LTO-6` → `lto6`.
fn drive_name(generation: &str) -> String {
    generation
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

/// The label prefix of a generation's cartridges: `LTO-6` → `L6`.
fn label_prefix(generation: &str) -> String {
    let digits: String = generation
        .trim_start_matches(|c: char| !c.is_ascii_digit())
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    format!("L{}", if digits.is_empty() { "?" } else { &digits })
}

/// One line of the root block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootLine {
    pub command: String,
    pub why: String,
}

/// What needs root, only the lines that apply (ADR-0012 items 227, 229).
pub fn root_lines(
    user: &str,
    drive_group: &str,
    groups: &[String],
    path_var: &std::ffi::OsStr,
    timers: bool,
    linger: bool,
) -> Vec<RootLine> {
    let mut lines = Vec::new();
    if !groups.iter().any(|g| g == drive_group) {
        lines.push(RootLine {
            command: format!("sudo usermod -aG {drive_group} {user}"),
            why: format!("the drive's group; then log out and back in (`id` lists {drive_group})"),
        });
    }
    let mut missing: Vec<&str> = TOOLS
        .iter()
        .filter(|(tool, _)| crate::policy::depth_check::resolve_on_path(tool, path_var).is_none())
        .map(|(_, package)| *package)
        .collect();
    missing.dedup();
    if !missing.is_empty() {
        lines.push(RootLine {
            command: format!("sudo apt install {}", missing.join(" ")),
            why: "the tools tapectl and the tapes' RESTORE.sh run (Debian/Ubuntu package \
                  names; or your distro's)"
                .to_string(),
        });
    }
    if timers && !linger {
        lines.push(RootLine {
            command: format!("sudo loginctl enable-linger {user}"),
            why: "so the timers run while you are logged out".to_string(),
        });
    }
    lines
}

/// The group that owns `device`'s node, `tape` when it cannot be read.
fn drive_group(device: Option<&str>) -> String {
    use std::os::unix::fs::MetadataExt;
    device
        .and_then(|d| std::fs::metadata(d).ok())
        .and_then(|m| {
            nix::unistd::Group::from_gid(nix::unistd::Gid::from_raw(m.gid()))
                .ok()
                .flatten()
        })
        .map(|g| g.name)
        .unwrap_or_else(|| "tape".to_string())
}

/// `ExecStart=`'s program, quoted when it holds a space.
fn exec_word(path: &Path) -> String {
    let s = path.display().to_string();
    if s.chars().any(char::is_whitespace) {
        format!("\"{s}\"")
    } else {
        s
    }
}

/// The four user unit files: name and text.
pub fn unit_files(
    exe: &Path,
    tapectl_home: Option<&Path>,
    backup_dir: &Path,
) -> Vec<(&'static str, String)> {
    let env = tapectl_home
        .map(|h| format!("Environment=TAPECTL_HOME={}\n", h.display()))
        .unwrap_or_default();
    let exe = exec_word(exe);
    let backup = exec_word(backup_dir);
    vec![
        (
            UNIT_FILES[0],
            format!(
                "# Written by `tapectl setup`.\n\
                 [Unit]\n\
                 Description=tapectl catalog backup (db backup to {backup}, no tape)\n\
                 \n\
                 [Service]\n\
                 Type=oneshot\n\
                 {env}\
                 ExecStart={exe} db backup {backup}\n\
                 # 75: the catalog was busy; the next run backs it up.\n\
                 SuccessExitStatus=75\n"
            ),
        ),
        (
            UNIT_FILES[1],
            "# Written by `tapectl setup`.\n\
             [Unit]\n\
             Description=Weekly tapectl catalog backup\n\
             \n\
             [Timer]\n\
             # Weekly, Sunday night. After a write session, a backup at once:\n\
             # systemctl --user start tapectl-backup.service\n\
             OnCalendar=Sun *-*-* 03:00:00\n\
             # A machine that was off then backs up at its next start.\n\
             Persistent=true\n\
             RandomizedDelaySec=15m\n\
             Unit=tapectl-backup.service\n\
             \n\
             [Install]\n\
             WantedBy=timers.target\n"
                .to_string(),
        ),
        (
            UNIT_FILES[2],
            format!(
                "# Written by `tapectl setup`.\n\
                 [Unit]\n\
                 Description=tapectl advisory audit (read-only, no tape)\n\
                 \n\
                 [Service]\n\
                 Type=oneshot\n\
                 {env}\
                 ExecStart={exe} audit\n\
                 # 1: warnings only, advisory; 75: the catalog was busy. Only 2\n\
                 # (violations) fails the unit: `journalctl --user -u tapectl-audit`.\n\
                 SuccessExitStatus=1 75\n"
            ),
        ),
        (
            UNIT_FILES[3],
            "# Written by `tapectl setup`.\n\
             [Unit]\n\
             Description=Weekly tapectl advisory audit\n\
             \n\
             [Timer]\n\
             OnCalendar=Mon *-*-* 09:00:00\n\
             Persistent=true\n\
             RandomizedDelaySec=30m\n\
             Unit=tapectl-audit.service\n\
             \n\
             [Install]\n\
             WantedBy=timers.target\n"
                .to_string(),
        ),
    ]
}

impl Setup<'_> {
    fn dry(&self) -> bool {
        self.mode == Mode::DryRun
    }

    /// The catalog, opened once the home exists. `None` in a dry run on a
    /// home that does not exist yet.
    fn conn(&mut self) -> Result<Option<&Connection>> {
        if self.conn.is_none() && self.paths.is_initialized() {
            self.conn = Some(crate::db::open(&self.paths.db_file)?);
        }
        Ok(self.conn.as_ref())
    }

    fn config(&self) -> Result<Option<Config>> {
        if !self.paths.config_file.exists() {
            return Ok(None);
        }
        Ok(Some(Config::load(&self.paths.config_file)?))
    }

    fn operator(&mut self) -> Result<Option<String>> {
        Ok(match self.conn()? {
            Some(conn) => crate::db::queries::get_operator_tenant(conn)?.map(|t| t.name),
            None => None,
        })
    }

    fn location_names(&mut self) -> Result<Vec<String>> {
        let Some(conn) = self.conn()? else {
            return Ok(Vec::new());
        };
        let mut stmt = conn.prepare("SELECT name FROM locations ORDER BY id")?;
        let names = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(names)
    }

    // ── 1. Home ──────────────────────────────────────────────────────────

    fn home(&mut self) -> Result<()> {
        header(1, "Home: where the catalog, config and keys live");
        let home = self.paths.home.display().to_string();
        let why = self.startup.source.describe();
        if self.paths.is_initialized() {
            println!("  already done: the home is {home} ({why})");
            println!("  every step already done is skipped");
            return Ok(());
        }
        println!("  the home will be {home} ({why})");
        println!("  (to put it elsewhere, stop here and run `tapectl --home DIR setup`)");
        if self.dry() {
            println!("  would make it: config.toml, the catalog tapectl.db, keys/");
            return Ok(());
        }
        if !self.ask.confirm("Make the home there?", true)? {
            return Err(TapectlError::Other(
                "nothing made: run `tapectl --home DIR setup` to put the home in DIR".to_string(),
            ));
        }
        let (_, conn) = make_home(self.paths, false)?;
        self.conn = Some(conn);
        self.changed = true;
        println!("  made {home}: config.toml, the catalog tapectl.db, keys/");
        Ok(())
    }

    // ── 2. Staging ───────────────────────────────────────────────────────

    fn staging(&mut self) -> Result<()> {
        header(
            2,
            "Staging: where archives wait between staging and the tape",
        );
        let cfg = self.config()?;
        let current = cfg
            .as_ref()
            .map(|c| PathBuf::from(&c.staging.directory))
            .unwrap_or_else(|| self.paths.home.join("staging"));
        if self.args.staging.is_none() && cfg.is_some() && current.is_dir() {
            println!(
                "  already done: staged slices go in {} ({} free)",
                current.display(),
                free_space(&current).unwrap_or_else(|| "free space unknown".into())
            );
            return Ok(());
        }
        println!(
            "  staged slices are the encrypted archives waiting for the tape; give them a big \
             disk that is not the one your data is on"
        );
        let answer = self
            .ask
            .text(
                "Staging directory",
                Some(&current.display().to_string()),
                "--staging DIR",
            )?
            .expect("a default is given");
        let want = absolute_dir(&answer, &self.facts.user_home, "staging directory")?;
        if !want.exists() {
            if self.dry() {
                println!("  would create {}", want.display());
            } else if self.ask.confirm(
                &format!("{} does not exist. Create it?", want.display()),
                true,
            )? {
                std::fs::create_dir_all(&want).map_err(|e| {
                    TapectlError::Other(format!("could not create {}: {e}", want.display()))
                })?;
                crate::config::secure_path(&want, 0o700);
                self.changed = true;
                println!("  made {}", want.display());
            } else {
                return Err(TapectlError::Other(format!(
                    "staging: {} does not exist; create it, or give another with --staging DIR",
                    want.display()
                )));
            }
        } else if !want.is_dir() {
            return Err(TapectlError::Other(format!(
                "staging: {} is not a directory",
                want.display()
            )));
        }
        if want.exists() {
            if nix::unistd::access(&want, nix::unistd::AccessFlags::W_OK).is_err() {
                return Err(TapectlError::Other(format!(
                    "staging: {} is not writable by {}; give a directory you can write, with \
                     --staging DIR",
                    want.display(),
                    self.facts.user
                )));
            }
            println!(
                "  {} is writable, {} free",
                want.display(),
                free_space(&want).unwrap_or_else(|| "free space unknown".into())
            );
        }
        if want != current || cfg.is_none() {
            let value = want.display().to_string();
            if self.dry() {
                println!("  would set staging.directory = \"{value}\" in config.toml");
            } else {
                crate::cli::config::run_edit(
                    self.paths,
                    &ConfigCommands::Set {
                        key: "staging.directory".to_string(),
                        value: format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\"")),
                    },
                    false,
                    false,
                )?;
                self.changed = true;
            }
        }
        Ok(())
    }

    // ── 3. Drive ─────────────────────────────────────────────────────────

    fn drive(&mut self) -> Result<()> {
        header(3, "Drive: the tape drive tapectl writes with");
        if let Some(b) = self.config()?.and_then(|c| c.backends.lto.first().cloned()) {
            println!(
                "  already done: drive {} ({}) at {}",
                b.name, b.generation, b.device_tape
            );
            return Ok(());
        }
        let drives = list_drives(&self.facts.by_id_dir);
        let device = match &self.args.device {
            Some(d) => d.clone(),
            None => self.choose_drive(&drives)?,
        };
        // A /dev/nstN is written as the by-id link that names it: nstN
        // numbering is not stable across reboots.
        let device = match std::fs::canonicalize(&device) {
            Ok(node) if !device.starts_with(&*self.facts.by_id_dir.to_string_lossy()) => {
                match drives.iter().find(|d| d.node.as_ref() == Some(&node)) {
                    Some(d) => {
                        let by_id = d.by_id.display().to_string();
                        println!("  {device} is {by_id}: writing the by-id link, which survives a reboot");
                        by_id
                    }
                    None => device,
                }
            }
            _ => device,
        };
        // Identity only, from sysfs: no command goes to the drive.
        let attached = crate::tape::drive_identity::attached_identity(&device);
        let mut declared = self.args.generation.clone();
        let generation = loop {
            match crate::cli::drive::drive_generation(
                &device,
                declared.as_deref(),
                attached.as_ref(),
            ) {
                Ok(g) => break g,
                Err(e) if self.mode == Mode::Ask && declared.is_none() => {
                    println!("  {e}");
                    declared = self.ask.text(
                        "Generation the drive writes, e.g. LTO-6",
                        None,
                        "--generation GEN",
                    )?;
                }
                Err(e) if self.dry() && declared.is_none() => {
                    println!("  {e}");
                    println!(
                        "  would add the drive at {device} once --generation names its generation"
                    );
                    return Ok(());
                }
                Err(e) => return Err(e),
            }
        };
        if let Some(read) = &generation.read_from {
            println!("  {read}");
        }
        let canonical = crate::media::Generation::parse(&generation.generation)
            .map(|g| g.as_str().to_string())
            .unwrap_or_else(|| generation.generation.clone());
        let name = drive_name(&canonical);
        if self.dry() {
            println!("  would add drive \"{name}\" ({canonical}) at {device}, as `tapectl drive add` does");
            return Ok(());
        }
        println!("  adding drive \"{name}\" ({canonical}) at {device}");
        crate::cli::backend::add(
            self.paths,
            &name,
            &device,
            None,
            &generation.generation,
            None,
            None,
            false,
            false,
        )?;
        self.changed = true;
        Ok(())
    }

    fn choose_drive(&mut self, drives: &[FoundDrive]) -> Result<String> {
        match drives {
            [] => {
                println!(
                    "  no tape drive is listed under {}. Either it is not attached (check the cable \
                     and power, then `lsscsi`), or this account cannot see it (it must be in the \
                     drive's group, `tape` on most systems: see the root block at the end).",
                    self.facts.by_id_dir.display()
                );
                println!(
                    "  A drive not attached now can be added by its path, with --generation for \
                     the generation it writes."
                );
                Ok(self
                    .ask
                    .text(
                        "The drive's path, e.g. /dev/tape/by-id/<DRIVE>-nst",
                        None,
                        "--device PATH (and --generation GEN)",
                    )?
                    .unwrap_or_else(|| "<--device>".to_string()))
            }
            [one] => {
                println!("  found {}", one.describe());
                let path = one.by_id.display().to_string();
                if self.ask.confirm("Use this drive?", true)? {
                    Ok(path)
                } else {
                    Ok(self
                        .ask
                        .text("The drive's path", None, "--device PATH")?
                        .unwrap_or(path))
                }
            }
            several => {
                println!("  found {} drives:", several.len());
                for (i, d) in several.iter().enumerate() {
                    println!("    {}) {}", i + 1, d.describe());
                }
                let answer = self.ask.text(
                    "Which drive (its number or its path)",
                    None,
                    "--device PATH",
                )?;
                let Some(answer) = answer else {
                    return Ok("<--device>".to_string());
                };
                Ok(match answer.parse::<usize>() {
                    Ok(n) if (1..=several.len()).contains(&n) => {
                        several[n - 1].by_id.display().to_string()
                    }
                    _ => answer,
                })
            }
        }
    }

    // ── 4. Keys ──────────────────────────────────────────────────────────

    fn keys(&mut self) -> Result<()> {
        header(4, "Keys: the operator's keys and the escrow identity");
        let operator = match self.operator()? {
            Some(name) => {
                println!("  already done: the operator is \"{name}\", with its keys in keys/");
                name
            }
            None => {
                let default = crate::tenant::operator_name_for_init(
                    None,
                    Some(&self.facts.user),
                    self.facts.euid,
                    crate::tenant::system_uid_min(),
                )
                .ok();
                let name = match &self.args.operator {
                    Some(n) => n.clone(),
                    None => self
                        .ask
                        .text("The operator's name", default.as_deref(), "--operator NAME")?
                        .unwrap_or_else(|| "<operator>".to_string()),
                };
                if self.dry() {
                    println!(
                        "  would make the operator \"{name}\", with a primary and a backup key pair \
                         in keys/"
                    );
                    name
                } else {
                    crate::tenant::refuse_stray_key_files(
                        self.paths,
                        &name,
                        crate::tenant::MINTED_ALIASES,
                    )?;
                    let paths = self.paths;
                    let conn = self.conn()?.expect("the home exists");
                    crate::tenant::add_tenant(conn, paths, &name, Some("System operator"), true)?;
                    self.changed = true;
                    println!(
                        "  made the operator \"{name}\", with a primary and a backup key pair in \
                         {}",
                        paths.keys_dir.display()
                    );
                    name
                }
            }
        };
        let secret_path = self.paths.home.join(ESCROW_SECRET_FILE);
        let escrow = match self.conn()? {
            Some(conn) => crate::db::queries::escrow_key_exists(conn)?,
            None => false,
        };
        if escrow {
            println!("  already done: the escrow identity is registered");
            if secret_path.exists() && self.escrow_secret.is_none() {
                println!(
                    "  REMINDER: the escrow secret is still on this machine, at {}. Move it \
                     offline (print it, or a USB stick in a safe), then delete it: rm {}",
                    secret_path.display(),
                    secret_path.display()
                );
            }
            return Ok(());
        }
        let adopt = match &self.args.escrow_public_key {
            Some(k) => Some(k.clone()),
            None => {
                println!(
                    "  the escrow identity is the one key that reads every tape if everything \
                     else is lost; setup mints a new one unless you already have one to adopt"
                );
                if self.ask.confirm("Mint a new escrow identity?", true)? {
                    None
                } else {
                    self.ask.text(
                        "The escrow public key to adopt (age1…, or a .pub file)",
                        None,
                        "--escrow-public-key KEY_OR_FILE",
                    )?
                }
            }
        };
        if self.dry() {
            match adopt {
                Some(k) => println!("  would adopt the escrow public key {k}"),
                None => println!(
                    "  would mint the escrow identity for \"{operator}\" and write its secret to \
                     {}, to be moved offline and deleted",
                    secret_path.display()
                ),
            }
            return Ok(());
        }
        let paths = self.paths;
        let conn = self.conn()?.expect("the home exists");
        match adopt {
            Some(value) => {
                let pk = crate::cli::key::adopt_escrow_recipient(conn, paths, &value)?;
                self.changed = true;
                println!(
                    "  adopted the escrow public key {pk}; its secret stays wherever you keep it"
                );
            }
            None => {
                let public_key = write_and_register_escrow(conn, paths, &secret_path)?;
                self.changed = true;
                self.escrow_secret = Some(secret_path.clone());
                print_escrow_box(&public_key, &secret_path);
            }
        }
        Ok(())
    }

    // ── 5. Locations ─────────────────────────────────────────────────────

    fn locations(&mut self) -> Result<()> {
        header(5, "Locations: the places cartridges are kept");
        let existing = self.location_names()?;
        if self.args.locations.is_empty() && existing.len() >= 2 {
            println!("  already done: {}", existing.join(", "));
            return Ok(());
        }
        let wanted: Vec<String> = if self.args.locations.is_empty() {
            println!(
                "  two places, so one copy survives what happens to the other (a fire, a \
                 flood, a burglary)"
            );
            let first = self
                .ask
                .text("The first place", Some("home"), "--location NAME")?
                .expect("a default is given");
            let second = self
                .ask
                .text(
                    "The second place, away from the first",
                    Some("offsite"),
                    "--location NAME",
                )?
                .expect("a default is given");
            vec![first, second]
        } else {
            self.args.locations.clone()
        };
        for name in wanted {
            let name = name.trim().to_string();
            if existing.contains(&name) {
                println!("  already done: location \"{name}\"");
                continue;
            }
            if self.dry() {
                println!("  would add location \"{name}\", as `tapectl location add` does");
                continue;
            }
            let conn = self.conn()?.expect("the home exists");
            crate::cli::location::run(
                conn,
                &LocationCommands::Add {
                    name: name.clone(),
                    description: None,
                    kind: "shelf".to_string(),
                },
                false,
                false,
            )?;
            self.changed = true;
        }
        Ok(())
    }

    // ── 6. Collection ────────────────────────────────────────────────────

    fn collection(&mut self) -> Result<()> {
        header(
            6,
            "First collection: a folder whose subfolders are the units",
        );
        let cfg = self.config()?;
        if let Some(cfg) = cfg.as_ref().filter(|c| !c.collections.is_empty()) {
            let names: Vec<&str> = cfg.collections.iter().map(|c| c.name.as_str()).collect();
            let conn = self.conn()?.expect("a config means a home");
            let mut unregistered = 0;
            for lib in &cfg.collections {
                if let Ok(preview) =
                    crate::collection::sync::sync_collection_with_config(conn, cfg, lib, true)
                {
                    unregistered += preview.created;
                }
            }
            println!("  already done: collection {}", names.join(", "));
            if unregistered > 0 {
                println!(
                    "  {} not registered yet",
                    crate::render::plural(
                        unregistered as i64,
                        "unit folder is",
                        "unit folders are"
                    )
                );
                self.register_units()?;
            }
            return Ok(());
        }
        println!(
            "  a collection is a folder whose subfolders are the units, one folder, one unit: \
             /media/photos with 2019-italy/ and 2020-garden/ in it makes two units"
        );
        let Some(root) = (match &self.args.collection_root {
            Some(r) => Some(r.clone()),
            None => self.ask.text(
                "The collection's folder, e.g. /media/photos",
                None,
                "--collection-root DIR",
            )?,
        }) else {
            println!("  would ask for the collection's folder (--collection-root DIR)");
            return Ok(());
        };
        let root_path = absolute_dir(&root, &self.facts.user_home, "the collection's folder")?;
        let root = root_path.display().to_string();
        let default_name = root_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "collection".to_string());
        let name = match &self.args.collection {
            Some(n) => n.clone(),
            None => self
                .ask
                .text(
                    "The collection's name",
                    Some(&default_name),
                    "--collection NAME",
                )?
                .expect("a default is given"),
        };
        let operator = self.operator()?.or_else(|| self.args.operator.clone());
        let tenant = match (&self.args.tenant, &operator) {
            (Some(t), _) => t.clone(),
            (None, Some(op)) => self
                .ask
                .text("The tenant its units belong to", Some(op), "--tenant NAME")?
                .expect("a default is given"),
            (None, None) => self
                .ask
                .text("The tenant its units belong to", None, "--tenant NAME")?
                .unwrap_or_else(|| "<the operator>".to_string()),
        };
        let Some(cfg) = cfg else {
            println!(
                "  would add collection \"{name}\" (root {root}, tenant {tenant}), as `tapectl \
                 collection add` does, and show the units its sync would register"
            );
            return Ok(());
        };
        let tenant_exists = match self.conn()? {
            Some(conn) => crate::db::queries::get_tenant_by_name(conn, &tenant)?.is_some(),
            None => false,
        };
        if !tenant_exists {
            if self.dry() {
                println!("  would add tenant \"{tenant}\", as `tapectl tenant add` does");
            } else if self.ask.confirm(
                &format!("Tenant \"{tenant}\" does not exist. Add it?"),
                true,
            )? {
                let paths = self.paths;
                let conn = self.conn()?.expect("the home exists");
                crate::tenant::add_tenant(conn, paths, &tenant, None, false)?;
                self.changed = true;
                println!("  added tenant \"{tenant}\", with a primary and a backup key pair");
            } else {
                return Err(TapectlError::Other(format!(
                    "tenant \"{tenant}\" does not exist: name another with --tenant NAME"
                )));
            }
        }
        if self.dry() && !tenant_exists {
            println!(
                "  would add collection \"{name}\" (root {root}, tenant {tenant}), as `tapectl \
                 collection add` does"
            );
            return Ok(());
        }
        let (paths, dry) = (self.paths, self.dry());
        let conn = self.conn()?.expect("a config means a home");
        crate::cli::collection::run(
            conn,
            paths,
            &cfg,
            &CollectionCommands::Add {
                name: name.clone(),
                root,
                tenant,
                unit_depth: 1,
                exclude: Vec::new(),
                archive_set: None,
                no_dotfiles: false,
            },
            false,
            dry,
            false,
        )?;
        if !dry {
            self.changed = true;
            self.register_units()?;
        } else {
            println!("  would ask whether to register them (`tapectl collection sync`)");
        }
        Ok(())
    }

    /// `collection sync`, on yes.
    fn register_units(&mut self) -> Result<()> {
        if self.dry() {
            println!("  would ask whether to register them (`tapectl collection sync`)");
            return Ok(());
        }
        if !self.ask.confirm("Register these units now?", true)? {
            println!("  not registered: `tapectl collection sync` registers them");
            return Ok(());
        }
        let cfg = self.config()?.expect("the home exists");
        let paths = self.paths;
        let conn = self.conn()?.expect("the home exists");
        let code = crate::cli::collection::run(
            conn,
            paths,
            &cfg,
            &CollectionCommands::Sync { dry_run: false },
            false,
            false,
            false,
        )?;
        self.changed = true;
        if code != crate::error::EXIT_SUCCESS {
            println!(
                "  the sync had warnings (above); `tapectl collection status` shows them again"
            );
        }
        Ok(())
    }

    // ── 7. Backups and audit ─────────────────────────────────────────────

    fn timers(&mut self) -> Result<()> {
        header(7, "Backups and audit: user timers");
        let dir = self.facts.unit_dir.clone();
        if self.args.no_timers {
            println!(
                "  skipped (--no-timers): back the catalog up yourself after each write \
                 session, `tapectl db backup DIR`, and run `tapectl audit` now and then"
            );
            return Ok(());
        }
        if UNIT_FILES.iter().all(|f| dir.join(f).exists()) {
            println!(
                "  already done: {} and {} in {}",
                UNIT_FILES[1],
                UNIT_FILES[3],
                dir.display()
            );
            self.timers_written = true;
            return Ok(());
        }
        println!(
            "  a weekly copy of the catalog to another disk, and a weekly `tapectl audit`, as \
             systemd user timers that run as {}",
            self.facts.user
        );
        let default = self.facts.user_home.join("tapectl-backups");
        let answer = match &self.args.backup_dir {
            Some(d) => d.clone(),
            None => self
                .ask
                .text(
                    "Directory for the catalog backups, best on another disk",
                    Some(&default.display().to_string()),
                    "--backup-dir DIR",
                )?
                .expect("a default is given"),
        };
        let backup_dir = absolute_dir(&answer, &self.facts.user_home, "backup directory")?;
        if !backup_dir.exists() {
            if self.dry() {
                println!("  would create {}", backup_dir.display());
            } else if self.ask.confirm(
                &format!("{} does not exist. Create it?", backup_dir.display()),
                true,
            )? {
                std::fs::create_dir_all(&backup_dir).map_err(|e| {
                    TapectlError::Other(format!("could not create {}: {e}", backup_dir.display()))
                })?;
                crate::config::secure_path(&backup_dir, 0o700);
                self.changed = true;
                println!("  made {}", backup_dir.display());
            } else {
                return Err(TapectlError::Other(format!(
                    "backups: {} does not exist; create it, or give another with --backup-dir \
                     DIR (or --no-timers)",
                    backup_dir.display()
                )));
            }
        }
        if same_disk(&backup_dir, &self.paths.home) {
            println!(
                "  warning: {} is on the same disk as the home, {}: a backup on the disk that \
                 holds the original is lost with it. Point it at another disk when you can \
                 (run setup again with --backup-dir after deleting the two backup unit files).",
                backup_dir.display(),
                self.paths.home.display()
            );
        }
        let default_home = self.facts.user_home.join(".tapectl");
        let tapectl_home = (self.paths.home != default_home).then_some(self.paths.home.as_path());
        let files = unit_files(&self.facts.tapectl_exe, tapectl_home, &backup_dir);
        if self.dry() {
            for (name, _) in &files {
                println!("  would write {}", dir.join(name).display());
            }
            return Ok(());
        }
        std::fs::create_dir_all(&dir)
            .map_err(|e| TapectlError::Other(format!("could not create {}: {e}", dir.display())))?;
        for (name, text) in &files {
            let path = dir.join(name);
            std::fs::write(&path, text).map_err(|e| {
                TapectlError::Other(format!("could not write {}: {e}", path.display()))
            })?;
            println!("  wrote {}", path.display());
        }
        self.changed = true;
        self.timers_written = true;
        println!("  turn them on (setup writes the files only):");
        println!(
            "    systemctl --user daemon-reload && systemctl --user enable --now \
             tapectl-backup.timer tapectl-audit.timer"
        );
        Ok(())
    }

    // ── 8. Check ─────────────────────────────────────────────────────────

    fn check(&mut self) -> Result<()> {
        header(8, "Check: `tapectl config check`");
        if !self.paths.is_initialized() {
            println!("  would run `tapectl config check` on the new home");
            return Ok(());
        }
        let startup = self.startup;
        let conn = self.conn()?.expect("the home exists");
        let code = crate::cli::config::check(conn, startup, false)?;
        if code == crate::error::EXIT_SUCCESS {
            println!("  config check: {}", crate::render::verdict("ok"));
        } else {
            println!("  config check: {}", crate::render::verdict("FAIL"));
            self.check_failed = true;
        }
        Ok(())
    }

    // ── 9. Root ──────────────────────────────────────────────────────────

    fn root_block(&mut self) -> Result<()> {
        header(9, "What needs root");
        let device = self
            .config()?
            .and_then(|c| c.backends.lto.first().map(|b| b.device_tape.clone()))
            .or_else(|| self.args.device.clone());
        let group = drive_group(device.as_deref());
        let timers = !self.args.no_timers && (self.timers_written || self.dry());
        let lines = root_lines(
            &self.facts.user,
            &group,
            &self.facts.groups,
            &self.facts.path_var,
            timers,
            self.facts.linger,
        );
        if lines.is_empty() {
            println!("  nothing: this account is in the drive's group, the tools are installed");
            return Ok(());
        }
        println!("  setup never runs sudo; copy these and run them yourself:");
        println!();
        for line in &lines {
            println!("    {}", line.command);
        }
        println!();
        for line in &lines {
            let word = line.command.split_whitespace().nth(1).unwrap_or("");
            println!("  {word}: {}", line.why);
        }
        Ok(())
    }

    // ── 10. Next ─────────────────────────────────────────────────────────

    fn next(&mut self) -> Result<()> {
        header(10, "Next: the first two copies");
        if !self.changed && !self.dry() {
            println!("  setup: every step was already done; nothing changed");
        }
        let cfg = self.config()?;
        let generation = cfg
            .as_ref()
            .and_then(|c| c.backends.lto.first().map(|b| b.generation.clone()))
            .or_else(|| self.args.generation.clone())
            .unwrap_or_else(|| "LTO-6".to_string());
        let prefix = label_prefix(&generation);
        let collection = cfg
            .as_ref()
            .and_then(|c| c.collections.first().map(|l| l.name.clone()))
            .or_else(|| self.args.collection.clone())
            .unwrap_or_else(|| "<COLLECTION>".to_string());
        let mut places = self.location_names()?;
        if places.is_empty() {
            places = if self.args.locations.is_empty() {
                vec!["home".to_string(), "offsite".to_string()]
            } else {
                self.args.locations.clone()
            };
        }
        let first = places.first().cloned().unwrap_or_else(|| "home".into());
        let second = places.get(1).cloned().unwrap_or_else(|| first.clone());
        println!("  see how the units pack into tapes:");
        println!("    tapectl collection plan");
        println!("  copy 1: load a blank cartridge, then");
        println!(
            "    tapectl collection run {collection} --label {prefix}-0001 --init \
             --full-confirm --move-to {first}"
        );
        println!("  copy 2: load the next blank cartridge, then");
        println!("    tapectl volume write {prefix}-0002 --init --full-confirm --move-to {second}");
        println!("  where things stand, and the next command, at any time:");
        println!("    tapectl status");
        println!();
        println!(
            "  Optional, with a spare cartridge: scripts/lto-profile.sh in the tapectl \
             repository measures this drive and gives its profile. It ERASES that cartridge \
             and takes about 4.5 hours on LTO-6. Without it, each copy's full readback \
             (--full-confirm) is the proof, and planning stays at the fill ceiling."
        );
        println!("  The Quick start: {QUICK_START_URL}");
        if self.escrow_secret.is_some()
            || self.paths.home.join(ESCROW_SECRET_FILE).exists() && !self.dry()
        {
            println!(
                "  Before the first copy: move the escrow secret offline and delete it from \
                 this machine (step 4)."
            );
        }
        Ok(())
    }
}

/// Whether `a` and `b` are on one filesystem.
fn same_disk(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let dev = |p: &Path| {
        p.ancestors()
            .find_map(|q| std::fs::metadata(q).ok())
            .map(|m| m.dev())
    };
    matches!((dev(a), dev(b)), (Some(x), Some(y)) if x == y)
}

/// Mint the escrow identity, write its secret to `secret_path` (mode 0600,
/// never overwriting), and only then register its public half, so a write
/// that fails leaves no recipient whose secret is lost. Returns the public
/// key.
fn write_and_register_escrow(
    conn: &Connection,
    paths: &TapectlPaths,
    secret_path: &Path,
) -> Result<String> {
    use std::os::unix::fs::OpenOptionsExt;
    let kp = crate::crypto::keys::generate_keypair();
    let text = format!(
        "# tapectl escrow identity (ADR-0005): the one key that reads every tape if\n\
         # everything else is lost. Move it offline (print it, or a USB stick in a\n\
         # safe), then delete this file from this machine.\n\
         # created: {}\n\
         # public key: {}\n\
         {}\n",
        chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
        kp.public_key,
        kp.secret_key
    );
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(secret_path)
        .map_err(|e| {
            TapectlError::Other(format!(
                "could not write the escrow secret to {}: {e} (a file left there by an \
                 earlier run: move it offline if it is a secret you keep, delete it, and run \
                 setup again)",
                secret_path.display()
            ))
        })?;
    let written = f.write_all(text.as_bytes()).and_then(|_| f.sync_all());
    drop(f);
    if let Err(e) = written {
        let _ = std::fs::remove_file(secret_path);
        return Err(TapectlError::Other(format!(
            "could not write the escrow secret to {}: {e}",
            secret_path.display()
        )));
    }
    if let Err(e) = crate::cli::key::register_escrow_keypair(conn, paths, &kp, None) {
        let _ = std::fs::remove_file(secret_path);
        return Err(e);
    }
    Ok(kp.public_key)
}

/// The warning box for a minted escrow identity. The secret itself is never
/// printed: only where it is.
fn print_escrow_box(public_key: &str, secret_path: &Path) {
    let rule = "  ==========================================================================";
    let file = secret_path.display();
    println!();
    println!("{rule}");
    println!("  THE ESCROW SECRET: MOVE IT OFFLINE, THEN DELETE IT FROM THIS MACHINE");
    println!("{rule}");
    println!("  The escrow identity is the one key that reads every tape tapectl writes");
    println!("  if everything else is lost: this machine, the catalog, the operator's");
    println!("  keys. Every tape is encrypted to it as well as to its tenant.");
    println!();
    println!("  Its secret was written to:");
    println!("      {file}");
    println!();
    println!("  Move it offline now: print it, or copy it to a USB stick kept in a");
    println!("  safe, in two places if you can. Then delete it from this machine:");
    println!("      rm {file}");
    println!("  Whoever holds it can read every tape: keep it as you keep a will.");
    println!("  `tapectl key escrow-kit DIR` writes the Heir Kit to keep beside it.");
    println!();
    println!("  Its public key (safe to keep anywhere): {public_key}");
    println!("{rule}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_prefixes_follow_the_generation() {
        assert_eq!(drive_name("LTO-6"), "lto6");
        assert_eq!(drive_name("LTO-8"), "lto8");
        assert_eq!(label_prefix("LTO-6"), "L6");
        assert_eq!(label_prefix("LTO-10"), "L10");
    }

    /// The root block names only what applies: the group when the account
    /// is not in it, the packages whose tools are missing, linger only with
    /// timers.
    #[test]
    fn the_root_block_names_only_what_applies() {
        let empty = tempfile::TempDir::new().unwrap();
        let bin = tempfile::TempDir::new().unwrap();
        for (tool, _) in TOOLS {
            let p = bin.path().join(tool);
            std::fs::write(&p, "#!/bin/sh\n").unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let groups = vec!["alice".to_string(), "tape".to_string()];
        let all = root_lines("alice", "tape", &groups, bin.path().as_os_str(), true, true);
        assert!(all.is_empty(), "{all:?}");

        let lines = root_lines(
            "alice",
            "tape",
            &["alice".into()],
            empty.path().as_os_str(),
            true,
            false,
        );
        let commands: Vec<&str> = lines.iter().map(|l| l.command.as_str()).collect();
        assert_eq!(
            commands,
            [
                "sudo usermod -aG tape alice",
                "sudo apt install dar sg3-utils age mt-st tar",
                "sudo loginctl enable-linger alice",
            ]
        );
        let no_timers = root_lines(
            "alice",
            "tape",
            &groups,
            empty.path().as_os_str(),
            false,
            false,
        );
        assert!(no_timers.iter().all(|l| !l.command.contains("linger")));
    }

    /// The unit files run this tapectl, the backup into the directory given,
    /// with TAPECTL_HOME only for a home that is not the default.
    #[test]
    fn the_unit_files_run_tapectl_as_the_user() {
        let files = unit_files(
            Path::new("/usr/local/bin/tapectl"),
            None,
            Path::new("/mnt/b"),
        );
        let names: Vec<&str> = files.iter().map(|(n, _)| *n).collect();
        assert_eq!(names, UNIT_FILES);
        assert!(files[0]
            .1
            .contains("ExecStart=/usr/local/bin/tapectl db backup /mnt/b\n"));
        assert!(files[2]
            .1
            .contains("ExecStart=/usr/local/bin/tapectl audit\n"));
        assert!(files[2].1.contains("SuccessExitStatus=1 75\n"));
        for (_, text) in &files {
            assert!(!text.contains("User="), "{text}");
            assert!(!text.contains("TAPECTL_HOME"), "{text}");
        }
        let elsewhere = unit_files(
            Path::new("/opt/my tools/tapectl"),
            Some(Path::new("/srv/archive")),
            Path::new("/mnt/b"),
        );
        assert!(elsewhere[0]
            .1
            .contains("Environment=TAPECTL_HOME=/srv/archive\n"));
        assert!(elsewhere[0]
            .1
            .contains("ExecStart=\"/opt/my tools/tapectl\" db backup"));
    }

    #[test]
    fn a_relative_answer_is_refused_and_tilde_is_the_home() {
        let home = Path::new("/home/alice");
        assert_eq!(
            absolute_dir("~/staging", home, "x").unwrap(),
            Path::new("/home/alice/staging")
        );
        assert!(absolute_dir("staging", home, "x").is_err());
    }
}
