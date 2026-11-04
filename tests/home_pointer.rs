//! Issue #437: the system home pointer (`/etc/tapectl/home`), through the real
//! binary. `startup`'s unit tests prove the ranking on `resolve_from`; this
//! proves the only thing they cannot — that `resolve()` reads the pointer's
//! location from `TAPECTL_HOME_POINTER` and the euid from the process, and
//! that the whole binary then operates on the home the pointer names.
//!
//! No test touches `/etc`: every invocation names its own pointer file in a
//! temp dir through the child's environment (the parent's is never changed).
//! The probe for "which home did this resolve to" is `tapectl init`, which
//! refuses an initialized home by name and, when it does not, creates the
//! home it resolved — so one command reveals the resolution either way.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

fn is_root() -> bool {
    nix::unistd::geteuid().is_root()
}

/// A service home (initialized), an operator's `HOME` with no `~/.tapectl`,
/// and a pointer file naming the service home.
struct Host {
    _tmp: TempDir,
    svc: PathBuf,
    op_home: PathBuf,
    pointer: PathBuf,
}

impl Host {
    fn new() -> Host {
        let tmp = TempDir::new().unwrap();
        let svc = tmp.path().join("svc/.tapectl");
        let op_home = tmp.path().join("op");
        std::fs::create_dir_all(&op_home).unwrap();
        let host = Host {
            svc,
            op_home,
            pointer: tmp.path().join("etc-tapectl-home"),
            _tmp: tmp,
        };
        // The service home, made the way every test makes one: --home.
        let init = Command::new(env!("CARGO_BIN_EXE_tapectl"))
            .arg("--home")
            .arg(&host.svc)
            .args(["init", "--operator", "svc"])
            .env_remove("TAPECTL_HOME")
            .output()
            .expect("spawn tapectl");
        assert!(
            init.status.success(),
            "{}",
            String::from_utf8_lossy(&init.stderr)
        );
        std::fs::write(&host.pointer, format!("{}\n", host.svc.display())).unwrap();
        host
    }

    /// Run as the operator: `HOME` is theirs, no `TAPECTL_HOME`, and the
    /// pointer is whatever `pointer` says.
    fn run(&self, pointer: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_tapectl"))
            .args(args)
            .env("HOME", &self.op_home)
            .env_remove("TAPECTL_HOME")
            .env("TAPECTL_HOME_POINTER", pointer)
            .output()
            .expect("spawn tapectl")
    }

    fn own_home(&self) -> PathBuf {
        self.op_home.join(".tapectl")
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The whole point: a plain command, no `--home`, reaches the service home.
/// `init` is the probe (it names the home it found initialized); `status`
/// is the read the issue is about.
#[test]
fn a_plain_command_reaches_the_home_the_pointer_names() {
    if is_root() {
        eprintln!("skipped: as root the pointer is ignored (see the root test)");
        return;
    }
    let host = Host::new();

    let init = host.run(&host.pointer, &["init", "--operator", "x"]);
    assert!(!init.status.success());
    assert!(
        stderr(&init).contains(&format!("already initialized at {}", host.svc.display())),
        "{}",
        stderr(&init)
    );

    let status = host.run(&host.pointer, &["status", "--json"]);
    assert!(status.status.success(), "{}", stderr(&status));
    assert!(!host.own_home().exists(), "the operator got no ~/.tapectl");
}

/// Positive control for the above: with no pointer file the same command
/// makes the operator's own fresh `~/.tapectl` — today's behaviour, and the
/// reason the probe is meaningful.
#[test]
fn without_a_pointer_file_nothing_changes() {
    let host = Host::new();
    let absent = host.pointer.with_file_name("no-such-pointer");
    let init = host.run(&absent, &["init", "--operator", "x"]);
    assert!(init.status.success(), "{}", stderr(&init));
    assert!(host.own_home().join("tapectl.db").exists());
}

/// An existing `~/.tapectl` outranks the pointer (the gate's isolation, a
/// developer's own home), and so does `--home`.
#[test]
fn an_own_home_and_the_home_flag_outrank_the_pointer() {
    let host = Host::new();
    // Make the operator's own home, with no pointer in play.
    let absent = host.pointer.with_file_name("no-such-pointer");
    let made = host.run(&absent, &["init", "--operator", "x"]);
    assert!(made.status.success(), "{}", stderr(&made));

    // Now the pointer exists, but ~/.tapectl wins.
    let again = host.run(&host.pointer, &["init", "--operator", "x"]);
    assert!(
        stderr(&again).contains(&format!(
            "already initialized at {}",
            host.own_home().display()
        )),
        "{}",
        stderr(&again)
    );

    // And --home wins over both.
    let flagged = host.run(
        &host.pointer,
        &[
            "--home",
            host.svc.to_str().unwrap(),
            "init",
            "--operator",
            "x",
        ],
    );
    assert!(
        stderr(&flagged).contains(&format!("already initialized at {}", host.svc.display())),
        "{}",
        stderr(&flagged)
    );
}

/// A pointer that is present but wrong stops the command, names the file,
/// and creates nothing — it never falls through to a fresh `~/.tapectl`.
#[test]
fn a_wrong_pointer_is_a_named_error_not_a_fresh_home() {
    if is_root() {
        eprintln!("skipped: as root the pointer is never read");
        return;
    }
    let host = Host::new();
    let relative = host.pointer.with_file_name("relative-pointer");
    std::fs::write(&relative, "svc/.tapectl\n").unwrap();
    let gone = host.pointer.with_file_name("gone-pointer");
    std::fs::write(
        &gone,
        format!("{}\n", host.svc.with_file_name("gone").display()),
    )
    .unwrap();
    for (what, pointer, why) in [
        ("relative", &relative, "not an absolute path"),
        ("missing target", &gone, "does not exist"),
    ] {
        let out = host.run(pointer, &["status"]);
        assert!(!out.status.success(), "{what}");
        let said = stderr(&out);
        assert!(
            said.contains(&pointer.display().to_string()),
            "{what}: {said}"
        );
        assert!(said.contains(why), "{what}: {said}");
        assert!(!host.own_home().exists(), "{what}: created a home");
    }
}

/// ADR-0012 item 213 (OQ10), through the real euid: as root, on a host with a
/// home pointer, a command that does not name its home is refused and names
/// `tapectl-op`, rather than opening /root/.tapectl; `--home` and
/// `TAPECTL_HOME` still work, and `host check`, `completions`, `--version`
/// and `--help` are exempt. Only meaningful as root, so it skips otherwise
/// (CI and the dev VM run unprivileged; `startup`'s unit test
/// `root_is_refused_beside_a_pointer_unless_the_home_is_named` covers the
/// rule).
#[test]
fn as_root_beside_a_pointer_only_a_named_home_runs() {
    if !is_root() {
        eprintln!(
            "skipped: needs euid 0 (unit test `root_is_refused_beside_a_pointer_unless_the_home_is_named` covers the rule)"
        );
        return;
    }
    let host = Host::new();
    for args in [
        &["status"][..],
        &["init", "--operator", "x"],
        &["unit", "list"],
    ] {
        let out = host.run(&host.pointer, args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", stderr(&out));
        assert!(
            stderr(&out).contains("tapectl-op"),
            "{args:?}: {}",
            stderr(&out)
        );
        assert!(!host.own_home().exists(), "{args:?}: made /root's home");
    }
    let named = host.run(
        &host.pointer,
        &["--home", host.svc.to_str().unwrap(), "unit", "list"],
    );
    assert!(named.status.success(), "{}", stderr(&named));
    let env = Command::new(env!("CARGO_BIN_EXE_tapectl"))
        .args(["unit", "list"])
        .env("HOME", &host.op_home)
        .env("TAPECTL_HOME", &host.svc)
        .env("TAPECTL_HOME_POINTER", &host.pointer)
        .output()
        .unwrap();
    assert!(env.status.success(), "{}", stderr(&env));
    for args in [
        &["host", "check"][..],
        &["completions", "bash"],
        &["--version"],
        &["--help"],
    ] {
        let out = host.run(&host.pointer, args);
        assert!(
            !stderr(&out).contains("refusing to run as root"),
            "{args:?} is exempt: {}",
            stderr(&out)
        );
    }
}

/// ADR-0012 item 213: `status` and `config check` name the home and the rule
/// that chose it, for every rule an operator meets.
#[test]
fn status_and_config_check_name_the_home_and_why() {
    if is_root() {
        eprintln!("skipped: as root beside a pointer tapectl refuses (see the root test)");
        return;
    }
    let host = Host::new();
    let first_line = |out: &Output| {
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .unwrap_or_default()
            .to_string()
    };

    // The pointer.
    let out = host.run(&host.pointer, &["status"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        first_line(&out),
        format!(
            "home: {} (named by the home pointer {})",
            host.svc.display(),
            host.pointer.display()
        )
    );
    let json: serde_json::Value =
        serde_json::from_slice(&host.run(&host.pointer, &["status", "--json"]).stdout).unwrap();
    assert_eq!(json["home_source"], "pointer");
    assert_eq!(json["home_pointer"], host.pointer.display().to_string());
    assert_eq!(json["home"], host.svc.display().to_string());
    assert!(json["units"]["active"].is_number(), "{json}");
    assert!(json["next"].is_array(), "{json}");

    // `config check`: second, after the verdict scripts read first.
    let check = host.run(&host.pointer, &["config", "check"]);
    let text = String::from_utf8_lossy(&check.stdout);
    let lines: Vec<&str> = text.lines().take(2).collect();
    assert_eq!(lines[0], "config: valid", "{text}");
    assert_eq!(
        lines[1],
        format!(
            "home: {} (named by the home pointer {})",
            host.svc.display(),
            host.pointer.display()
        )
    );

    // --home.
    let out = host.run(
        &host.pointer,
        &["--home", host.svc.to_str().unwrap(), "status"],
    );
    assert_eq!(
        first_line(&out),
        format!("home: {} (named by --home)", host.svc.display())
    );

    // TAPECTL_HOME.
    let out = Command::new(env!("CARGO_BIN_EXE_tapectl"))
        .arg("status")
        .env("HOME", &host.op_home)
        .env("TAPECTL_HOME", &host.svc)
        .env("TAPECTL_HOME_POINTER", &host.pointer)
        .output()
        .unwrap();
    assert_eq!(
        first_line(&out),
        format!("home: {} (named by TAPECTL_HOME)", host.svc.display())
    );

    // --config alone.
    let cfg = host.svc.join("config.toml");
    let out = host.run(
        &host.pointer,
        &["--config", cfg.to_str().unwrap(), "status"],
    );
    assert_eq!(
        first_line(&out),
        format!("home: {} (the directory of --config)", host.svc.display())
    );

    // No pointer and no ~/.tapectl: the default, not initialized, exit 2.
    let absent = host.pointer.with_file_name("no-such-pointer");
    let out = host.run(&absent, &["status"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert_eq!(
        first_line(&out),
        format!(
            "home: {} (~/.tapectl, the default: nothing named another home)",
            host.own_home().display()
        )
    );
    assert!(!host.own_home().exists(), "status made no home");

    // This account's own ~/.tapectl.
    let made = host.run(&absent, &["init", "--operator", "x"]);
    assert!(made.status.success(), "{}", stderr(&made));
    let out = host.run(&host.pointer, &["status"]);
    assert_eq!(
        first_line(&out),
        format!(
            "home: {} (this account's ~/.tapectl)",
            host.own_home().display()
        )
    );
}

/// ADR-0012 item 213: "not initialized" names the path and the rule, and
/// offers `tapectl init` only for a new archive, never on a host with a
/// home pointer.
#[test]
fn not_initialized_names_the_path_and_offers_init_only_without_a_pointer() {
    if is_root() {
        eprintln!("skipped: as root beside a pointer tapectl refuses (see the root test)");
        return;
    }
    let host = Host::new();
    let absent = host.pointer.with_file_name("no-such-pointer");
    let out = host.run(&absent, &["unit", "list"]);
    assert_eq!(out.status.code(), Some(2));
    let said = stderr(&out);
    assert!(
        said.contains(&format!(
            "tapectl is not initialized at {}",
            host.own_home().display()
        )),
        "{said}"
    );
    assert!(said.contains("nothing named a home"), "{said}");
    assert!(said.contains("`tapectl init`"), "{said}");

    // A pointer naming a directory with no catalog: no init offered.
    let empty = host.svc.with_file_name("empty-home");
    std::fs::create_dir_all(&empty).unwrap();
    let to_empty = host.pointer.with_file_name("pointer-to-empty");
    std::fs::write(&to_empty, format!("{}\n", empty.display())).unwrap();
    let out = host.run(&to_empty, &["unit", "list"]);
    assert_eq!(out.status.code(), Some(2));
    let said = stderr(&out);
    assert!(
        said.contains(&format!(
            "tapectl is not initialized at {}",
            empty.display()
        )),
        "{said}"
    );
    assert!(
        said.contains(&format!("the home pointer {} named it", to_empty.display())),
        "{said}"
    );
    assert!(said.contains("tapectl-op"), "{said}");
    assert!(!said.contains("init`"), "{said}");
    assert!(!empty.join("tapectl.db").exists(), "nothing was made");
}

/// ADR-0012 item 213: `init` ends with the next steps, in order, and names
/// `tapectl status` for every step after.
#[test]
fn init_ends_with_the_next_steps() {
    let tmp = TempDir::new().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_tapectl"))
        .arg("--home")
        .arg(tmp.path().join("h"))
        .args(["init", "--operator", "op", "--no-escrow"])
        .env_remove("TAPECTL_HOME")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let text = String::from_utf8_lossy(&out.stdout);
    let steps = text
        .split("next steps:\n")
        .nth(1)
        .unwrap_or_else(|| panic!("no next steps: {text}"));
    let order = [
        "tapectl tenant add",
        "tapectl drive add",
        "tapectl collection add",
        "tapectl unit init",
        "tapectl location add",
        "tapectl volume write <NEW-LABEL> --init --full-confirm --move-to <PLACE>",
        "`tapectl status`",
    ];
    let mut at = 0;
    for want in order {
        let found = steps[at..]
            .find(want)
            .unwrap_or_else(|| panic!("{want:?} after byte {at} in:\n{steps}"));
        at += found + want.len();
    }
    assert!(!steps.contains("/dev/nst"), "{steps}");

    let json = Command::new(env!("CARGO_BIN_EXE_tapectl"))
        .arg("--home")
        .arg(tmp.path().join("j"))
        .args(["--json", "init", "--operator", "op", "--no-escrow"])
        .env_remove("TAPECTL_HOME")
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(v["next"][0], "tapectl tenant add <TENANT>", "{v}");
}

/// `.cargo/config.toml` switches the system home pointer off for every test binary,
/// so a HOME-isolated test can never resolve to a real service home on a host that has
/// `/etc/tapectl/home` (issue #437). If this fails, the suite is no longer isolated.
#[test]
fn the_suite_runs_with_the_system_pointer_switched_off() {
    let p = std::env::var_os("TAPECTL_HOME_POINTER")
        .expect("TAPECTL_HOME_POINTER is unset: .cargo/config.toml [env] is not in force");
    assert!(
        !std::path::Path::new(&p).exists(),
        "TAPECTL_HOME_POINTER names {p:?}, which exists: the suite could read a real home"
    );
}
