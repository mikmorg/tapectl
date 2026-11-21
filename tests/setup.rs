//! `tapectl setup` (ADR-0012 items 227, 229, 231), run as the binary against
//! a throwaway home, a throwaway user unit directory and no drive: the drive
//! is declared by a path that does not exist and its generation, as `drive
//! add` takes a drive not attached now. Group membership comes through
//! `TAPECTL_TEST_SETUP_GROUPS` and the drive listing through
//! `TAPECTL_TEST_SETUP_BY_ID_DIR`, so nothing depends on the test machine.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

/// One throwaway world: the account's home, the tapectl home inside it, a
/// collection folder of two units, a backup directory, the unit directory
/// and an empty drive listing.
struct World {
    root: TempDir,
}

impl World {
    fn new() -> World {
        let root = TempDir::new().unwrap();
        for unit in ["2019-italy", "2020-garden"] {
            let dir = root.path().join("media/photos").join(unit);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("a.txt"), unit).unwrap();
        }
        std::fs::create_dir_all(root.path().join("user")).unwrap();
        std::fs::create_dir_all(root.path().join("by-id")).unwrap();
        std::fs::create_dir_all(root.path().join("backups")).unwrap();
        World { root }
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.root.path().join(rel)
    }

    fn home(&self) -> PathBuf {
        self.p("archive")
    }

    fn units(&self) -> PathBuf {
        self.p("units")
    }

    /// `tapectl --home <home> ARGS`, stdin closed (not a terminal).
    fn tapectl(&self, args: &[&str]) -> Output {
        self.tapectl_at(&self.home(), args)
    }

    /// The same, for another home in the same account.
    fn tapectl_at(&self, home: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_tapectl"))
            .arg("--home")
            .arg(home)
            .args(args)
            .env("HOME", self.p("user"))
            .env("USER", "friend")
            .env_remove("TAPECTL_HOME")
            .env_remove("XDG_CONFIG_HOME")
            .env("TAPECTL_SETUP_UNIT_DIR", self.units())
            .env("TAPECTL_TEST_SETUP_GROUPS", "friend,users")
            .env("TAPECTL_TEST_SETUP_BY_ID_DIR", self.p("by-id"))
            .stdin(Stdio::null())
            .output()
            .expect("spawn tapectl")
    }

    /// `setup --yes` with every answer that has no default.
    fn setup_args(&self) -> Vec<String> {
        vec![
            "setup".into(),
            "--yes".into(),
            "--staging".into(),
            self.p("staging").display().to_string(),
            "--device".into(),
            "/dev/nonexistent-nst".into(),
            "--generation".into(),
            "LTO-6".into(),
            "--collection-root".into(),
            self.p("media/photos").display().to_string(),
            "--backup-dir".into(),
            self.p("backups").display().to_string(),
        ]
    }

    fn setup(&self, extra: &[&str]) -> Output {
        let mut args = self.setup_args();
        args.extend(extra.iter().map(|s| s.to_string()));
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        self.tapectl(&args)
    }

    /// Every path under the world, with its bytes (directories empty).
    fn tree(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        fn walk(dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
            for e in std::fs::read_dir(dir).unwrap() {
                let path = e.unwrap().path();
                if path.is_dir() {
                    out.insert(path.clone(), Vec::new());
                    walk(&path, out);
                } else {
                    out.insert(path.clone(), std::fs::read(&path).unwrap());
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(self.root.path(), &mut out);
        out
    }
}

fn text(out: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn assert_ok(out: &Output, what: &str) -> (String, String) {
    let (stdout, stderr) = text(out);
    assert!(
        out.status.success(),
        "{what} failed ({:?})\nstdout:\n{stdout}\nstderr:\n{stderr}",
        out.status.code()
    );
    (stdout, stderr)
}

/// `--yes` with the answers that have no default finishes: a home `config
/// check` passes, two locations, the collection with its units registered,
/// the four unit files running this tapectl as the user, the root block
/// naming `usermod` for an account outside the drive's group, and the
/// escrow secret shown once, as `init` shows it, and stored nowhere.
#[test]
fn yes_with_the_needed_flags_finishes_a_working_home() {
    let w = World::new();
    let (stdout, stderr) = assert_ok(&w.setup(&[]), "setup --yes");

    // config check passes on the home it left.
    assert_ok(&w.tapectl(&["config", "check"]), "config check");
    assert!(stdout.contains("config check: ok"), "{stdout}");

    // Two locations, home and offsite.
    let (locations, _) = assert_ok(&w.tapectl(&["--json", "location", "list"]), "location list");
    let locations: serde_json::Value = serde_json::from_str(&locations).unwrap();
    let names: Vec<&str> = locations
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["name"].as_str().unwrap())
        .collect();
    assert_eq!(names.len(), 2, "{locations}");
    assert!(
        names.contains(&"home") && names.contains(&"offsite"),
        "{names:?}"
    );

    // The collection, named for its folder, and its two units registered.
    let config = std::fs::read_to_string(w.home().join("config.toml")).unwrap();
    assert!(config.contains("name = \"photos\""), "{config}");
    assert!(config.contains("[[backends.lto]]"), "{config}");
    assert!(
        config.contains(&format!("directory = \"{}\"", w.p("staging").display())),
        "{config}"
    );
    let (units, _) = assert_ok(&w.tapectl(&["--json", "unit", "list"]), "unit list");
    for unit in ["photos/2019-italy", "photos/2020-garden"] {
        assert!(units.contains(unit), "{unit} not registered: {units}");
    }

    // The unit files: this tapectl, as the user, into the backup directory.
    let exe = std::fs::canonicalize(env!("CARGO_BIN_EXE_tapectl")).unwrap();
    let backup = std::fs::read_to_string(w.units().join("tapectl-backup.service")).unwrap();
    assert!(
        backup.contains(&format!(
            "ExecStart={} db backup {}\n",
            exe.display(),
            w.p("backups").display()
        )),
        "{backup}"
    );
    assert!(
        backup.contains(&format!(
            "Environment=TAPECTL_HOME={}\n",
            w.home().display()
        )),
        "{backup}"
    );
    let audit = std::fs::read_to_string(w.units().join("tapectl-audit.service")).unwrap();
    assert!(
        audit.contains(&format!("ExecStart={} audit\n", exe.display())),
        "{audit}"
    );
    for timer in ["tapectl-backup.timer", "tapectl-audit.timer"] {
        let t = std::fs::read_to_string(w.units().join(timer)).unwrap();
        assert!(
            t.contains("OnCalendar=") && t.contains("Persistent=true"),
            "{t}"
        );
    }
    assert!(
        stdout.contains("systemctl --user daemon-reload && systemctl --user enable --now"),
        "{stdout}"
    );

    // The root block: the account is not in the drive's group.
    assert!(stdout.contains("sudo usermod -aG tape friend"), "{stdout}");

    // The escrow secret is shown once on stderr, as `init` shows it
    // (ADR-0005), and stored nowhere. Positive control: the box and exactly
    // one secret line are there to be found.
    assert!(
        stderr.contains("ESCROW IDENTITY GENERATED -- THIS SECRET IS SHOWN EXACTLY ONCE"),
        "{stderr}"
    );
    let secret_lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("AGE-SECRET-KEY-"))
        .collect();
    assert_eq!(secret_lines.len(), 1, "{stderr}");
    let escrow_secret = secret_lines[0];
    assert!(escrow_secret.len() > 20, "{escrow_secret}");
    assert!(
        !stdout.contains("AGE-SECRET-KEY-"),
        "a secret key is in stdout"
    );
    // No file under the home, staging, the backups or the unit directory
    // holds it. The only secrets on disk are the tenants' own key files in
    // keys/ (the operator's primary and backup), which `init` writes too.
    let mut tenant_keys = 0;
    for dir in [w.home(), w.p("staging"), w.p("backups"), w.units()] {
        for (path, bytes) in w.tree() {
            if !path.starts_with(&dir) {
                continue;
            }
            let text = String::from_utf8_lossy(&bytes);
            assert!(
                !text.contains(escrow_secret),
                "the escrow secret is in {}",
                path.display()
            );
            if text.contains("AGE-SECRET-KEY-") {
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                assert!(
                    path.parent() == Some(w.home().join("keys").as_path())
                        && (name.ends_with("-primary.age.key")
                            || name.ends_with("-backup.age.key")),
                    "a secret key outside the tenants' key files: {}",
                    path.display()
                );
                tenant_keys += 1;
            }
        }
    }
    assert_eq!(tenant_keys, 2, "the operator's primary and backup keys");

    // The next commands, in this release's spellings.
    assert!(
        stdout.contains(
            "tapectl collection run photos --label L6-0001 --init --full-confirm --move-to home"
        ),
        "{stdout}"
    );
    assert!(
        stdout.contains("tapectl volume write L6-0002 --init --full-confirm --move-to offsite"),
        "{stdout}"
    );
}

/// A second run on a finished home changes nothing and says so, and never
/// shows a secret again.
#[test]
fn a_second_run_is_a_no_op_that_says_done() {
    let w = World::new();
    assert_ok(&w.setup(&[]), "the first setup");
    let before = w.tree();
    let (stdout, stderr) = assert_ok(&w.setup(&[]), "the second setup");
    assert!(
        stdout.contains("every step was already done; nothing changed"),
        "{stdout}"
    );
    assert!(stdout.contains("already done: the escrow identity is registered"));
    assert!(!stderr.contains("AGE-SECRET-KEY-"), "{stderr}");
    let after = w.tree();
    let changed: Vec<&PathBuf> = before
        .keys()
        .chain(after.keys())
        .filter(|p| before.get(*p) != after.get(*p))
        // The catalog's own files move whenever it is opened.
        .filter(|p| !p.to_string_lossy().contains("tapectl.db"))
        .filter(|p| !p.starts_with(w.home().join("logs")))
        .collect();
    assert!(changed.is_empty(), "the second run changed {changed:?}");
}

/// `--dry-run` writes nothing: the whole tree is byte for byte the same.
#[test]
fn a_dry_run_writes_nothing() {
    let w = World::new();
    let before = w.tree();
    let mut args = w.setup_args();
    args.retain(|a| a != "--yes");
    args.push("--dry-run".into());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let (stdout, _) = assert_ok(&w.tapectl(&args), "setup --dry-run");
    assert!(stdout.contains("DRY RUN"), "{stdout}");
    assert!(stdout.contains("would make it"), "{stdout}");
    assert!(stdout.contains("would add drive \"lto6\""), "{stdout}");
    assert_eq!(before, w.tree(), "the dry run changed the tree");
    assert!(!w.home().exists());
}

/// Without a terminal and without `--yes`, setup refuses before anything,
/// naming --yes.
#[test]
fn no_terminal_and_no_yes_refuses_with_the_hint() {
    let w = World::new();
    let before = w.tree();
    let out = w.tapectl(&["setup"]);
    let (_, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("stdin is not a terminal"), "{stderr}");
    assert!(stderr.contains("--yes"), "{stderr}");
    assert_eq!(before, w.tree());
}

/// Under `--yes` a question with no default is refused, naming its flag; the
/// steps before it stay done and the next run finishes.
#[test]
fn yes_refuses_a_question_without_a_default_and_a_rerun_finishes() {
    let w = World::new();
    let mut args = w.setup_args();
    let at = args.iter().position(|a| a == "--collection-root").unwrap();
    args.drain(at..at + 2);
    let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = w.tapectl(&args_ref);
    let (_, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("--collection-root DIR"), "{stderr}");
    assert!(w.home().join("tapectl.db").exists());

    let (stdout, _) = assert_ok(&w.setup(&[]), "setup again");
    assert!(stdout.contains("already done: the operator"), "{stdout}");
    assert!(stdout.contains("added collection \"photos\""), "{stdout}");
}

/// Adopting an escrow public key prints no secret.
#[test]
fn an_adopted_escrow_key_prints_no_secret() {
    let w = World::new();
    let kp = tapectl::crypto::keys::generate_keypair();
    let (stdout, stderr) = assert_ok(
        &w.setup(&["--escrow-public-key", &kp.public_key]),
        "setup --escrow-public-key",
    );
    assert!(stdout.contains(&format!("adopted the escrow public key {}", kp.public_key)));
    assert!(!stderr.contains("ESCROW IDENTITY GENERATED"), "{stderr}");
    for out in [&stdout, &stderr] {
        assert!(!out.contains("AGE-SECRET-KEY-"), "{out}");
    }
}

/// A world whose home has a `config.toml` with a drive in it (made by
/// `init` and `drive add` in another home) and no catalog: what an operator
/// restores before setting up again.
fn world_with_a_kept_config() -> World {
    let w = World::new();
    let donor = w.p("donor");
    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_tapectl"))
            .arg("--home")
            .arg(&donor)
            .args(args)
            .env("HOME", w.p("user"))
            .env_remove("TAPECTL_HOME")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_ok(&out, &args.join(" "));
    };
    run(&["init", "--no-escrow", "--operator", "friend"]);
    run(&[
        "drive",
        "add",
        "--name",
        "kept-drive",
        "--device-tape",
        "/dev/nonexistent-kept-nst",
        "--generation",
        "LTO-6",
    ]);
    std::fs::create_dir_all(w.home()).unwrap();
    std::fs::copy(donor.join("config.toml"), w.home().join("config.toml")).unwrap();
    w
}

/// Issue #538: `setup` in a home that has a `config.toml` and no catalog
/// keeps the file (its drive is the one setup then finds) and says so. On
/// the parent step 1 wrote the defaults over it.
#[test]
fn setup_keeps_a_config_toml_it_finds() {
    let w = world_with_a_kept_config();
    let (stdout, _) = assert_ok(&w.setup(&[]), "setup --yes");
    let config = std::fs::read_to_string(w.home().join("config.toml")).unwrap();
    assert!(config.contains("kept-drive"), "{config}");
    assert!(
        stdout.contains("already done: drive kept-drive"),
        "the drive in the kept file is the one setup finds: {stdout}"
    );
    assert!(
        stdout.contains("kept the config.toml that was there"),
        "{stdout}"
    );
}

/// Issue #538: its dry run says the file would be kept.
#[test]
fn setup_dry_run_says_it_would_keep_a_config_toml_it_finds() {
    let w = world_with_a_kept_config();
    let mut args = w.setup_args();
    args.retain(|a| a != "--yes");
    args.push("--dry-run".into());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let (stdout, _) = assert_ok(&w.tapectl(&args), "setup --dry-run");
    assert!(
        stdout.contains("would keep the config.toml there"),
        "{stdout}"
    );
}

/// #599 9-1: a kept `config.toml` that names a collection, and no catalog,
/// is the same recovery; its dry run previews every step and exits 0. On the
/// parent step 6 panicked ("a config means a home", exit 101).
#[test]
fn setup_dry_run_on_a_kept_config_with_a_collection_does_not_panic() {
    let w = world_with_a_kept_config();
    let mut config = std::fs::read_to_string(w.home().join("config.toml")).unwrap();
    config.push_str(&format!(
        "\n[[collections]]\nname = \"photos\"\nroot = \"{}\"\ntenant = \"friend\"\n",
        w.p("media/photos").display()
    ));
    std::fs::write(w.home().join("config.toml"), config).unwrap();
    let mut args = w.setup_args();
    args.retain(|a| a != "--yes");
    args.push("--dry-run".into());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let (stdout, _) = assert_ok(&w.tapectl(&args), "setup --dry-run");
    assert!(stdout.contains("collection photos"), "{stdout}");
}

/// #599 9-2: one `--location` is one place; the printed copy 2 is not
/// shelved beside copy 1, and setup says a second place is missing.
#[test]
fn one_location_does_not_send_both_copies_to_it() {
    let w = World::new();
    let (stdout, _) = assert_ok(&w.setup(&["--location", "vault"]), "setup --location vault");
    assert!(
        !stdout.contains("L6-0002 --init --full-confirm --move-to vault"),
        "{stdout}"
    );
    assert!(stdout.contains("one place is not two"), "{stdout}");
    assert!(stdout.contains("<SECOND-PLACE>"), "{stdout}");
}

/// #599 9-5: timers written for one home are not "already done" for
/// another home in the same account.
#[test]
fn timers_written_for_another_home_are_not_already_done() {
    let w = World::new();
    assert_ok(&w.setup(&[]), "setup for home A");
    let other = w.p("other-home");
    let mut args = w.setup_args();
    args.retain(|a| a != "--yes");
    args.push("--dry-run".into());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let (stdout, _) = assert_ok(&w.tapectl_at(&other, &args), "setup --dry-run, home B");
    assert!(
        !stdout.contains("already done: tapectl-backup.timer"),
        "{stdout}"
    );
    assert!(stdout.contains("for another home"), "{stdout}");
    // The same home still finds its own timers done.
    let (again, _) = assert_ok(&w.setup(&[]), "setup again, home A");
    assert!(
        again.contains("already done: tapectl-backup.timer"),
        "{again}"
    );
}

/// #599 9-6: first-run step 12's eject hint is built from `OP_CMD`, which
/// names the home (`--home`) whenever TAPECTL_HOME_DIR is set (item 197), not
/// from literal text that does not.
#[test]
fn first_run_step_12s_eject_hint_names_the_home_through_op_cmd() {
    let script =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/first-run.sh"))
            .unwrap();
    let hint = script
        .lines()
        .find(|l| l.contains("Eject the test cartridge"))
        .expect("step 12's eject hint");
    assert!(hint.contains("$OP_CMD drive eject --device $DEVICE"), "{hint}");
}

// ── the writer's tar floor (T5.4; ADR-0012 items 229, 274) ───────────────

/// A directory holding an executable `tar` that is a shell script.
fn stand_in_tar(w: &World, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let dir = w.p(name);
    std::fs::create_dir_all(&dir).unwrap();
    let tar = dir.join("tar");
    std::fs::write(&tar, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&tar, std::fs::Permissions::from_mode(0o755)).unwrap();
    tar
}

/// `tapectl setup ...` with `first` at the front of `PATH`.
fn setup_with_path_front(w: &World, first: &Path, extra: &[&str]) -> Output {
    let mut args = w.setup_args();
    args.extend(extra.iter().map(|s| s.to_string()));
    let path = format!(
        "{}:{}",
        first.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    Command::new(env!("CARGO_BIN_EXE_tapectl"))
        .arg("--home")
        .arg(w.home())
        .args(&args)
        .env("HOME", w.p("user"))
        .env("USER", "friend")
        .env("PATH", path)
        .env_remove("TAPECTL_HOME")
        .env_remove("XDG_CONFIG_HOME")
        .env("TAPECTL_SETUP_UNIT_DIR", w.units())
        .env("TAPECTL_TEST_SETUP_GROUPS", "friend,users")
        .env("TAPECTL_TEST_SETUP_BY_ID_DIR", w.p("by-id"))
        .stdin(Stdio::null())
        .output()
        .expect("spawn tapectl")
}

const TAR_1_34: &str = "echo 'tar (GNU tar) 1.34'; exit 0";

fn assert_stopped_at_the_floor(out: &Output) {
    let (stdout, stderr) = text(out);
    assert!(
        !out.status.success(),
        "a tar below the floor must stop setup\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let all = format!("{stdout}{stderr}");
    assert!(all.contains("GNU tar 1.35"), "names the floor: {all}");
    assert!(all.contains("ACL support"), "names the floor: {all}");
    assert!(all.contains("1.34"), "names the tar it found: {all}");
    assert!(all.contains("quick-start.md"), "points at the way: {all}");
}

/// A host whose tar is GNU tar 1.34 (Debian 12, Ubuntu 22.04): setup names
/// the floor and the tar it found and stops before it makes a home, a key or
/// a config, in a dry run too. On the parent it went on and made all three.
#[test]
fn a_tar_below_the_floor_stops_setup_before_it_makes_a_home() {
    let w = World::new();
    let before = w.tree();
    let tar = stand_in_tar(&w, "old-tar", TAR_1_34);
    let dir = tar.parent().unwrap().to_path_buf();
    let before_with_tar = {
        let mut b = before;
        b.insert(dir.clone(), Vec::new());
        b.insert(tar.clone(), std::fs::read(&tar).unwrap());
        b
    };
    let out = setup_with_path_front(&w, &dir, &[]);
    assert_stopped_at_the_floor(&out);
    assert!(!w.home().exists(), "no home was made");
    assert!(!w.units().exists(), "no unit file was written");
    assert_eq!(before_with_tar, w.tree(), "setup left something behind");

    let out = setup_with_path_front(&w, &dir, &["--dry-run"]);
    assert_stopped_at_the_floor(&out);
    assert!(!w.home().exists());
}

/// The config's `[tar] binary` decides where one is in play: a bad one named
/// there stops setup although `tar` on `PATH` is fine.
#[test]
fn the_configs_tar_binary_is_the_one_checked() {
    let w = world_with_a_kept_config();
    let bad = stand_in_tar(&w, "named-tar", TAR_1_34);
    let config = std::fs::read_to_string(w.home().join("config.toml")).unwrap();
    assert!(config.contains("[tar]\nbinary = \"tar\""), "{config}");
    let config = config.replace(
        "[tar]\nbinary = \"tar\"",
        &format!("[tar]\nbinary = \"{}\"", bad.display()),
    );
    std::fs::write(w.home().join("config.toml"), &config).unwrap();
    let out = w.setup(&[]);
    assert_stopped_at_the_floor(&out);
    assert!(!w.home().join("tapectl.db").exists(), "no catalog was made");
    assert_eq!(
        std::fs::read_to_string(w.home().join("config.toml")).unwrap(),
        config,
        "the config is as it was"
    );
}

/// A tar that meets the floor, named by the config, gets past the step even
/// when the `tar` first on `PATH` is below it.
#[test]
fn a_config_naming_a_tar_at_the_floor_gets_past_it() {
    let w = world_with_a_kept_config();
    let good = stand_in_tar(&w, "good-tar", "exec /usr/bin/tar \"$@\"");
    let config = std::fs::read_to_string(w.home().join("config.toml")).unwrap();
    assert!(config.contains("[tar]\nbinary = \"tar\""), "{config}");
    let config = config.replace(
        "[tar]\nbinary = \"tar\"",
        &format!("[tar]\nbinary = \"{}\"", good.display()),
    );
    std::fs::write(w.home().join("config.toml"), &config).unwrap();
    let bad = stand_in_tar(&w, "old-tar", TAR_1_34);
    let out = setup_with_path_front(&w, bad.parent().unwrap(), &[]);
    assert_ok(&out, "setup with a good [tar] binary");
    assert!(w.home().join("tapectl.db").exists());
}
