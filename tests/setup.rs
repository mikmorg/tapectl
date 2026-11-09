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
        Command::new(env!("CARGO_BIN_EXE_tapectl"))
            .arg("--home")
            .arg(self.home())
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
/// escrow warning naming the secret's file without printing the secret.
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

    // The escrow warning names the file; the secret never appears.
    let secret_file = w.home().join("escrow-secret.age.key");
    let secret = std::fs::read_to_string(&secret_file).unwrap();
    let key_line = secret
        .lines()
        .find(|l| l.starts_with("AGE-SECRET-KEY-"))
        .expect("the secret file holds the identity");
    assert!(key_line.len() > 20, "{key_line}");
    assert!(stdout.contains("THE ESCROW SECRET"), "{stdout}");
    assert!(
        stdout.contains(&secret_file.display().to_string()),
        "{stdout}"
    );
    for (name, out) in [("stdout", &stdout), ("stderr", &stderr)] {
        assert!(
            !out.lines().any(|l| l.contains(key_line)),
            "the escrow secret is in {name}"
        );
        assert!(
            !out.contains("AGE-SECRET-KEY-"),
            "a secret key is in {name}"
        );
    }
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&secret_file)
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    // It is not in keys/, which `db backup --include-keys` copies.
    assert!(!w.home().join("keys/escrow-secret.age.key").exists());

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

/// A second run on a finished home changes nothing and says so; the escrow
/// secret still on the machine is named again.
#[test]
fn a_second_run_is_a_no_op_that_says_done() {
    let w = World::new();
    assert_ok(&w.setup(&[]), "the first setup");
    let before = w.tree();
    let (stdout, _) = assert_ok(&w.setup(&[]), "the second setup");
    assert!(
        stdout.contains("every step was already done; nothing changed"),
        "{stdout}"
    );
    assert!(stdout.contains("REMINDER: the escrow secret is still on this machine"));
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

/// Adopting an escrow public key writes no secret file.
#[test]
fn an_adopted_escrow_key_writes_no_secret() {
    let w = World::new();
    let kp = tapectl::crypto::keys::generate_keypair();
    let (stdout, _) = assert_ok(
        &w.setup(&["--escrow-public-key", &kp.public_key]),
        "setup --escrow-public-key",
    );
    assert!(stdout.contains(&format!("adopted the escrow public key {}", kp.public_key)));
    assert!(!w.home().join("escrow-secret.age.key").exists());
    assert!(!stdout.contains("THE ESCROW SECRET"), "{stdout}");
}
