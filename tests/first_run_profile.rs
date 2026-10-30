//! `scripts/first-run.sh` and the host it runs on (#506, ADR-0012 items 187
//! and 190): the profile a host was installed with is recorded, and a later
//! run that names none refuses; a profile may set only the documented
//! variables; the step numbers are checked; and step 12 stops an active
//! `tapectl-drive-poll.timer` for the rehearsal and starts it again however
//! the step ends.
//!
//! Every run is `--no-service-user` with `HOME` and `XDG_STATE_HOME` in a
//! temp dir, so no account, no sudo and no record of the developer's own is
//! involved. Most runs stop with `--to 0`, before step 1. The step-12 runs put
//! stubs for `sudo`, `systemctl`, `age` and `sg_read_attr` first on `PATH`: the
//! `sudo` stub only records what it was asked, so nothing reaches the host,
//! and no run gets as far as the rehearsal itself (`/dev/null` is the drive).

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/first-run.sh")
}

struct Rig {
    tmp: TempDir,
}

impl Rig {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        for d in ["h", "state", "work", "bin"] {
            std::fs::create_dir_all(tmp.path().join(d)).unwrap();
        }
        Rig { tmp }
    }
    fn p(&self, rel: &str) -> PathBuf {
        self.tmp.path().join(rel)
    }
    fn record(&self) -> PathBuf {
        self.p("state/tapectl/profile")
    }
    fn log(&self) -> String {
        std::fs::read_to_string(self.p("state/tapectl/first-run.log")).unwrap_or_default()
    }
    fn profile(&self, name: &str, body: &str) -> PathBuf {
        let path = self.p(name);
        std::fs::write(&path, body).unwrap();
        path
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new("bash");
        c.arg(script())
            .args(["--no-service-user", "--work-dir"])
            .arg(self.p("work"))
            .args(args)
            .env("HOME", self.p("h"))
            .env("USER", "fr-tester")
            .env("XDG_STATE_HOME", self.p("state"))
            .env("TAPECTL_HOME_POINTER", self.p("home-pointer"))
            .env_remove("TAPECTL_HOME")
            .env_remove("TAPECTL_LOCAL_PROFILE")
            .env_remove("XDG_CACHE_HOME")
            .stdin(std::process::Stdio::null());
        c
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("spawn first-run.sh")
    }
}

fn text(o: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&o.stdout).into_owned(),
        String::from_utf8_lossy(&o.stderr).into_owned(),
    )
}

fn ok(o: &Output) -> String {
    let (out, err) = text(o);
    assert!(
        o.status.success(),
        "first-run.sh failed ({:?})\nstdout:\n{out}\nstderr:\n{err}",
        o.status
    );
    out
}

/// Item 187. The positive control comes first: with nothing recorded, a run
/// with no `--profile` goes ahead, so the refusal after it is the record's.
#[test]
fn a_recorded_profile_is_required_by_name_until_none_is_said() {
    let rig = Rig::new();
    ok(&rig.run(&["--to", "0"]));
    assert!(!rig.record().exists(), "a run with no profile records none");

    let first = rig.profile("first.profile", "OPERATOR=opr\n");
    let first_abs = first.canonicalize().unwrap();
    let first_s = first_abs.display().to_string();
    let out = ok(&rig.run(&["--profile", first.to_str().unwrap(), "--to", "0"]));
    assert!(out.contains(&format!("profile: {first_s}")), "{out}");
    assert_eq!(
        std::fs::read_to_string(rig.record()).unwrap(),
        format!("{first_s}\n"),
        "the record holds the profile's absolute path"
    );

    // No --profile while one is recorded: refused before step 0, naming it.
    let o = rig.run(&["--to", "0"]);
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(2), "{out}\n{err}");
    assert!(
        err.contains(&format!(
            "this host was installed with the profile {first_s}"
        )) && err.contains(&format!("Re-run with --profile {first_s}"))
            && err.contains("or with --profile none to run without it"),
        "{err}"
    );
    assert!(!out.contains("tapectl first run"), "nothing ran: {out}");

    // --help still works with a record present.
    let out = ok(&rig.run(&["--help"]));
    assert!(
        out.contains("--profile none") && out.contains("--mint-escrow"),
        "{out}"
    );

    // --profile none: no profile, a note, and the record left as it is.
    let out = ok(&rig.run(&["--profile", "none", "--to", "0"]));
    assert!(
        out.contains(&format!(
            "the host's recorded profile {first_s} is not used (it stays recorded)"
        )),
        "{out}"
    );
    assert!(out.contains("profile: none"), "{out}");
    assert_eq!(
        std::fs::read_to_string(rig.record()).unwrap(),
        format!("{first_s}\n")
    );

    // Another profile replaces the record, and says so.
    let second = rig.profile("second.profile", "OPERATOR=opr2\n");
    let second_s = second.canonicalize().unwrap().display().to_string();
    let out = ok(&rig.run(&["--profile", second.to_str().unwrap(), "--to", "0"]));
    assert!(
        out.contains(&format!(
            "this host's recorded profile was {first_s}; it is now {second_s}"
        )),
        "{out}"
    );
    assert_eq!(
        std::fs::read_to_string(rig.record()).unwrap(),
        format!("{second_s}\n")
    );

    // One log line per run, --help aside: the arguments, the profile, the commit.
    let log = rig.log();
    let lines: Vec<&str> = log
        .lines()
        .filter(|l| l.contains(" first-run.sh "))
        .collect();
    assert_eq!(lines.len(), 5, "{log}");
    for (line, profile) in lines.iter().zip([
        "unset",
        first_s.as_str(),
        "unset",
        "none",
        second_s.as_str(),
    ]) {
        assert!(line.contains(&format!(" profile={profile} git=")), "{line}");
        assert!(line.contains(" --to 0 "), "{line}");
    }
    assert!(lines[1].contains(&format!("--profile {}", first.display())));
}

/// A profile that sets a name outside the documented set is refused by name,
/// before anything runs or is recorded. `MINT_ESCROW` is consent (item 188),
/// so a profile cannot give it either. The positive control is the spelling
/// the script reads, `STAGING_JOBS`.
#[test]
fn a_profile_variable_outside_the_documented_set_is_refused_by_name() {
    let rig = Rig::new();
    let typo = rig.profile("typo.profile", "OPERATOR=opr\nSTAGING_JOB=2\n");
    let o = rig.run(&["--profile", typo.to_str().unwrap(), "--to", "0"]);
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(2), "{out}\n{err}");
    assert!(
        err.contains("sets STAGING_JOB, which is not a profile variable"),
        "{err}"
    );
    assert!(err.contains("the profile variables are: "), "{err}");
    assert!(!err.contains("sets OPERATOR"), "{err}");
    assert!(!out.contains("tapectl first run"), "{out}");
    assert!(!rig.record().exists(), "a refused profile is not recorded");

    let mint = rig.profile("mint.profile", "MINT_ESCROW=1\n");
    let o = rig.run(&["--profile", mint.to_str().unwrap(), "--to", "0"]);
    let (_, err) = text(&o);
    assert_eq!(o.status.code(), Some(2), "{err}");
    assert!(
        err.contains("sets MINT_ESCROW: minting an escrow identity is consent")
            && err.contains("(--mint-escrow), never by a profile"),
        "{err}"
    );

    let good = rig.profile("good.profile", "OPERATOR=opr\nSTAGING_JOBS=2\n_scratch=1\n");
    ok(&rig.run(&["--profile", good.to_str().unwrap(), "--to", "0"]));
}

/// The positive control for the refusal above, on the profiles in the repo:
/// home2's, with and without the local profile it sources (which on home2
/// sets `ESCROW_PUBLIC_KEY`, and may set `ADOPT_KEYS_FROM`), and the dev VM's.
#[test]
fn both_checked_in_profiles_pass_the_allowlist() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    let home2 = repo.join("contrib/hosts/home2.profile");
    let vm = repo.join("contrib/hosts/vm-desk1-mhvtl.profile");

    let rig = Rig::new();
    let out = ok(&rig.run(&["--profile", home2.to_str().unwrap(), "--to", "0"]));
    assert!(out.contains("stopping before step 1"), "{out}");

    let rig = Rig::new();
    let local = rig.p("h/.config/tapectl/home2.local.profile");
    std::fs::create_dir_all(local.parent().unwrap()).unwrap();
    std::fs::write(
        &local,
        "ESCROW_PUBLIC_KEY=age1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq\n\
         ADOPT_KEYS_FROM=/nonexistent/keys\n",
    )
    .unwrap();
    let out = ok(&rig.run(&["--profile", home2.to_str().unwrap(), "--to", "0"]));
    assert!(out.contains("stopping before step 1"), "{out}");

    let rig = Rig::new();
    let home = rig.p("home");
    let out = ok(&rig.run(&[
        "--profile",
        vm.to_str().unwrap(),
        "--home",
        home.to_str().unwrap(),
        "--to",
        "0",
    ]));
    assert!(out.contains("stopping before step 1"), "{out}");
}

/// `--from` and `--to` name steps 0-14; anything else is refused before a
/// step runs (it used to reach `hdr` and die on bash's integer comparison).
#[test]
fn the_steps_are_numbered_0_to_14() {
    let rig = Rig::new();
    for (args, want) in [
        (
            &["--from", "x"][..],
            "--from must be a step number from 0 to 14, got 'x'",
        ),
        (
            &["--from", "15"][..],
            "--from must be a step number from 0 to 14, got '15'",
        ),
        (
            &["--to", "1.5"][..],
            "--to must be a step number from 0 to 14, got '1.5'",
        ),
        (
            &["--from", "9", "--to", "3"][..],
            "--from 9 comes after --to 3: no step would run",
        ),
    ] {
        let o = rig.run(args);
        let (out, err) = text(&o);
        assert_eq!(o.status.code(), Some(2), "{args:?}: {out}\n{err}");
        assert!(err.contains(want), "{args:?}: {err}");
    }
    ok(&rig.run(&["--from", "0", "--to", "0"]));
}

// ---------------------------------------------------------------- step 12

fn stub(dir: &Path, name: &str, body: &str) {
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Step 12 alone (`--from 12 --to 12 --auto`) with `/dev/null` as the drive and
/// `sg` as its sg node, the poll service and timer in the given states.
/// Returns the output and the calls the stubs recorded.
fn step12(service: &str, timer: &str, sg: &str) -> (Output, String) {
    let rig = Rig::new();
    let bin = rig.p("bin");
    let calls = rig.p("calls");
    let calls_s = calls.display();
    stub(
        &bin,
        "sudo",
        &format!("echo \"sudo $*\" >> {calls_s}; exit 0"),
    );
    stub(
        &bin,
        "systemctl",
        &format!(
            r#"echo "systemctl $*" >> {calls_s}
if [ "$1" = is-active ]; then
  for a; do u="$a"; done
  case "$u" in *.service) s="{service}" ;; *) s="{timer}" ;; esac
  [ "$2" = --quiet ] || echo "$s"
  [ "$s" = active ] && exit 0
  exit 3
fi
exit 0"#
        ),
    );
    stub(&bin, "age", "exit 0");
    stub(&bin, "sg_read_attr", "exit 0");
    // A TEST_BARCODE from the profile, not from --barcode: the rehearsal is
    // offered, and under --auto its erase is refused (a default is never consent).
    let profile = rig.profile("p.profile", "TEST_BARCODE=TESTSERIAL\n");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = rig
        .command(&[
            "--profile",
            profile.to_str().unwrap(),
            "--auto",
            "--from",
            "12",
            "--to",
            "12",
            "--tapectl",
            env!("CARGO_BIN_EXE_tapectl"),
            "--device",
            "/dev/null",
            "--sg",
            sg,
        ])
        .env("PATH", path)
        .output()
        .expect("spawn first-run.sh");
    (out, std::fs::read_to_string(&calls).unwrap_or_default())
}

fn stop_then_start(calls: &str) {
    let stop = calls
        .find("sudo systemctl stop tapectl-drive-poll.timer")
        .unwrap_or_else(|| panic!("the timer was not stopped:\n{calls}"));
    let start = calls
        .find("sudo systemctl start tapectl-drive-poll.timer")
        .unwrap_or_else(|| panic!("the timer was not started again:\n{calls}"));
    assert!(stop < start, "{calls}");
    assert_eq!(calls.matches("systemctl start").count(), 1, "{calls}");
}

/// Item 190, the step ending without a rehearsal (the erase refused under
/// --auto): the active timer was stopped and is started again at the step's
/// end, and the run goes on (exit 0 at `--to 12`).
#[test]
fn step_12_stops_an_active_poll_timer_and_starts_it_again_at_its_end() {
    let (o, calls) = step12("inactive", "active", "/dev/null");
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(0), "{out}\n{err}\n{calls}");
    assert!(out.contains("THE DRIVE POLL"), "{out}");
    assert!(
        out.contains("was NOT supplied via a flag — refusing under --auto"),
        "{out}"
    );
    // At the step's end, not at the script's exit: before step 13 begins.
    let started = out
        .find("tapectl-drive-poll.timer started again")
        .unwrap_or_else(|| panic!("{out}"));
    let next = out
        .find("stopping before step 13")
        .unwrap_or_else(|| panic!("{out}"));
    assert!(started < next, "{out}");
    stop_then_start(&calls);
}

/// Item 190, the step dying after the stop (the user cannot open the sg node
/// and is not in the drive's group): the EXIT trap starts the timer again, and
/// the script's exit status is still the die's.
#[test]
fn step_12_starts_the_poll_timer_again_when_it_dies() {
    let (o, calls) = step12("inactive", "active", "/nonexistent/sg-506");
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(1), "{out}\n{err}\n{calls}");
    assert!(err.contains("is not in group"), "{err}");
    assert!(
        out.contains("tapectl-drive-poll.timer started again"),
        "{out}"
    );
    stop_then_start(&calls);
}

/// Item 190: a poll running now (a oneshot unit is "activating" while it
/// runs) refuses the step, and the timer is not touched. The other control:
/// with the timer inactive, nothing is stopped or started, and the step is
/// otherwise the same as the first test's.
#[test]
fn step_12_waits_for_a_running_poll_and_leaves_an_inactive_timer_alone() {
    let (o, calls) = step12("activating", "active", "/dev/null");
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(1), "{out}\n{err}\n{calls}");
    assert!(
        err.contains("tapectl-drive-poll.service is running now (activating"),
        "{err}"
    );
    assert!(!calls.contains("sudo systemctl"), "{calls}");

    let (o, calls) = step12("inactive", "inactive", "/dev/null");
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(0), "{out}\n{err}\n{calls}");
    assert!(
        calls.contains("systemctl is-active --quiet tapectl-drive-poll.timer"),
        "the timer was asked about: {calls}"
    );
    assert!(!calls.contains("sudo systemctl"), "{calls}");
    assert!(!out.contains("THE DRIVE POLL"), "{out}");
}
