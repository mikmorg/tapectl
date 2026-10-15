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

/// ADR-0012 2026-10-15 item 62, through the real euid: as root the pointer
/// is not read, so root gets its own `~/.tapectl` and the service home is
/// left alone. Only meaningful as root, so it skips otherwise (CI and the
/// dev VM run unprivileged; the injected-euid unit test covers every run).
#[test]
fn as_root_the_pointer_is_ignored() {
    if !is_root() {
        eprintln!(
            "skipped: needs euid 0 (unit test `root_ignores_the_pointer_entirely` covers the rule)"
        );
        return;
    }
    let host = Host::new();
    let out = host.run(&host.pointer, &["init", "--operator", "x"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(host.own_home().join("tapectl.db").exists());
}
