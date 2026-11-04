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
//! Step 12 also refuses a test cartridge its catalog knows (ADR-0012 item
//! 192): those runs are given a home with cartridges registered in it.

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

// ---------------------------------------------------------------- step 7

fn is_root() -> bool {
    nix::unistd::geteuid().is_root()
}

/// `PATH` with the rig's stubs first: a `sudo` that records what it was asked
/// in `calls` and does nothing, so no run reaches the host.
fn sudo_stub_path(rig: &Rig) -> String {
    let calls = rig.p("calls");
    stub(
        &rig.p("bin"),
        "sudo",
        &format!("echo \"sudo $*\" >> {}; exit 0", calls.display()),
    );
    format!(
        "{}:{}",
        rig.p("bin").display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

/// #511's third defect (ADR-0012 item 199): on 2026-11-03 home2's
/// `/srv/acache`, rebuilt root:root 0710 without `u:tapectl:x`, kept the service
/// user out of the staging directory below it, and step 7 blamed the staging
/// directory and offered a `chown`. Now the parent the account cannot traverse
/// is named, with the traverse-only `setfacl` that fixes it, nothing is created
/// or given away, and no `chown` is offered. Here the account is the test's
/// own (`--no-service-user`) and the parent is its own directory at mode 0600.
/// The positive control is the same step with traverse given back.
#[test]
fn step_7_names_the_staging_parent_the_account_cannot_traverse() {
    if is_root() {
        eprintln!("skipped: running as root, whom no mode keeps out");
        return;
    }
    let rig = Rig::new();
    let home = rig.p("tapectl-home");
    tapectl_at(
        &rig,
        &home,
        &["init", "--operator", "fr-tester", "--no-escrow"],
    );
    let acache = rig.p("srv/acache");
    let staging = acache.join("tapectl-staging");
    std::fs::create_dir_all(&staging).unwrap();
    let profile = rig.profile("p.profile", &format!("STAGING_DIR={}\n", staging.display()));
    let path = sudo_stub_path(&rig);
    let step7 = || {
        rig.command(&[
            "--profile",
            profile.to_str().unwrap(),
            "--auto",
            "--from",
            "7",
            "--to",
            "7",
            "--tapectl",
            env!("CARGO_BIN_EXE_tapectl"),
            "--home",
            home.to_str().unwrap(),
        ])
        .env("PATH", &path)
        .output()
        .expect("spawn first-run.sh")
    };

    std::fs::set_permissions(&acache, std::fs::Permissions::from_mode(0o600)).unwrap();
    let o = step7();
    // Given back before asserting, so the temp dir can be removed either way.
    std::fs::set_permissions(&acache, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (out, err) = text(&o);
    let calls = std::fs::read_to_string(rig.p("calls")).unwrap_or_default();
    assert_eq!(o.status.code(), Some(1), "{out}\n{err}\n{calls}");
    let (a, s) = (acache.display(), staging.display());
    assert!(
        err.contains(&format!(
            "fr-tester cannot reach the staging directory {s}: {a} ("
        )) && err.contains(&format!("sudo setfacl -m u:fr-tester:x {a}")),
        "{err}"
    );
    assert!(
        !out.contains("chown") && !err.contains("chown") && !calls.contains("chown"),
        "a chown was offered:\n{out}\n{err}\n{calls}"
    );
    assert!(
        !err.contains("cannot write the staging directory") && !err.contains("is not writable"),
        "the staging directory was blamed: {err}"
    );
    assert!(
        !calls.contains("mkdir") && !calls.contains("install"),
        "something was created: {calls}"
    );

    let o = step7();
    let out = ok(&o);
    assert!(out.contains(&format!("staging at {s}")), "{out}");
}

// ---------------------------------------------------------------- step 8

/// Step 8 alone on `home`, with `/dev/null` as the drive: `drive add` writes a
/// device it cannot find a sg node for (a warning, never a refusal), and the
/// drive's facts (sysfs, `mt status`) are unreadable, which the step says.
fn step8(rig: &Rig, profile: &Path, home: &Path) -> Output {
    let path = sudo_stub_path(rig);
    rig.command(&[
        "--profile",
        profile.to_str().unwrap(),
        "--auto",
        "--from",
        "8",
        "--to",
        "8",
        "--tapectl",
        env!("CARGO_BIN_EXE_tapectl"),
        "--home",
        home.to_str().unwrap(),
        "--device",
        "/dev/null",
        "--generation",
        "LTO-6",
    ])
    .env("PATH", path)
    .output()
    .expect("spawn first-run.sh")
}

fn initialised_home(rig: &Rig) -> PathBuf {
    let home = rig.p("tapectl-home");
    tapectl_at(
        rig,
        &home,
        &["init", "--operator", "fr-tester", "--no-escrow"],
    );
    home
}

/// The uncommented `post_warning_bytes` lines of a config.
fn post_warning_lines(cfg: &str) -> Vec<&str> {
    cfg.lines()
        .filter(|l| l.trim_start().starts_with("post_warning_bytes"))
        .collect()
}

/// ADR-0012 item 200: the profile's `POST_WARNING_BYTES` becomes the drive's
/// `post_warning_bytes` right after `drive add`. A re-run finds the drive
/// already configured and the value already there, says so and changes no
/// byte; a profile that says otherwise is refused, naming both values, and the
/// file is left as it was. Without the variable, a drive that has the key is
/// only reported.
#[test]
fn step_8_sets_post_warning_bytes_from_the_profile_and_never_replaces_it() {
    let rig = Rig::new();
    let home = initialised_home(&rig);
    let cfg = home.join("config.toml");
    let profile = rig.profile("p.profile", "POST_WARNING_BYTES=99008M\n");

    let out = ok(&step8(&rig, &profile, &home));
    assert!(out.contains("$ tc drive add --name lto6"), "{out}");
    let set = out
        .find("$ tc config set backends.lto[lto6].post_warning_bytes 99008M")
        .unwrap_or_else(|| panic!("{out}"));
    assert!(
        out.find("$ tc drive add").unwrap() < set,
        "set after the drive is added: {out}"
    );
    assert!(
        out.contains("drive lto6: post_warning_bytes = 99008M (the profile's POST_WARNING_BYTES)"),
        "{out}"
    );
    let written = std::fs::read_to_string(&cfg).unwrap();
    assert_eq!(
        post_warning_lines(&written),
        vec!["post_warning_bytes = \"99008M\""],
        "{written}"
    );

    // The re-run: the drive exists, the value is the profile's.
    let out = ok(&step8(&rig, &profile, &home));
    assert!(
        out.contains("a [[backends.lto]] entry already exists"),
        "{out}"
    );
    assert!(
        out.contains("drive lto6: post_warning_bytes = 99008M already, as the profile says"),
        "{out}"
    );
    assert!(!out.contains("config set"), "{out}");
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), written);

    // A profile that disagrees with the drive: refused, nothing written.
    let other = rig.profile("other.profile", "POST_WARNING_BYTES=50000M\n");
    let o = step8(&rig, &other, &home);
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(1), "{out}\n{err}");
    assert!(
        err.contains(
            "drive lto6's post_warning_bytes is 99008M, and the profile's POST_WARNING_BYTES is 50000M"
        ) && err.contains("never replaces")
            && err.contains("config set 'backends.lto[lto6].post_warning_bytes' 50000M"),
        "{err}"
    );
    assert!(!out.contains("config set"), "{out}");
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), written);

    // No POST_WARNING_BYTES: the drive's value is reported, not the notice.
    let none = rig.profile("none.profile", "OPERATOR=fr-tester\n");
    let out = ok(&step8(&rig, &none, &home));
    assert!(
        out.contains(
            "drive lto6: post_warning_bytes = 99008M (already set; the profile sets no POST_WARNING_BYTES)"
        ),
        "{out}"
    );
    assert!(!out.contains("has no post_warning_bytes"), "{out}");
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), written);
}

/// ADR-0012 item 200: without `POST_WARNING_BYTES`, step 8 says once what a
/// drive without `post_warning_bytes` loses (planning past the fill ceiling,
/// item 113; the slice cap, item 100) and where to read about measuring it,
/// and sets nothing. A re-run says it again, once, and still sets nothing.
#[test]
fn step_8_without_post_warning_bytes_says_once_what_is_lost() {
    let rig = Rig::new();
    let home = initialised_home(&rig);
    let cfg = home.join("config.toml");
    let profile = rig.profile("p.profile", "OPERATOR=fr-tester\n");
    for run in ["first", "re-run"] {
        let out = ok(&step8(&rig, &profile, &home));
        // What is lost is said once, by the binary's `config check` note
        // (1.9.0, item 214); the script adds only the profile's part, once.
        assert_eq!(
            out.matches("has no post_warning_bytes").count(),
            1,
            "{run}: {out}"
        );
        assert_eq!(
            out.matches(
                "the profile sets no POST_WARNING_BYTES, so drive lto6 keeps no post_warning_bytes"
            )
            .count(),
            1,
            "{run}: {out}"
        );
        assert!(
            out.contains("fill ceiling")
                && out.contains("item 113")
                && out.contains("slice cap")
                && out.contains("item 100")
                && out.contains("docs/lto-profiling.md"),
            "{run}: {out}"
        );
        // The script runs no `config set` (a note may quote one).
        assert!(
            !out.lines()
                .any(|l| l.trim_start().starts_with("$ ") && l.contains("config set")),
            "{run}: {out}"
        );
        let written = std::fs::read_to_string(&cfg).unwrap();
        assert!(post_warning_lines(&written).is_empty(), "{run}: {written}");
    }
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
    step12_with(&Rig::new(), service, timer, sg, &[])
}

/// `step12` on a given rig, with more arguments after the usual ones.
fn step12_with(
    rig: &Rig,
    service: &str,
    timer: &str,
    sg: &str,
    extra: &[&str],
) -> (Output, String) {
    step12_sg(rig, service, timer, sg, extra, "exit 0")
}

/// `step12_with`, with `sg_read_attr`'s stub body given (`sh`).
fn step12_sg(
    rig: &Rig,
    service: &str,
    timer: &str,
    sg: &str,
    extra: &[&str],
    sg_read_attr: &str,
) -> (Output, String) {
    let bin = rig.p("bin");
    let calls = rig.p("calls");
    // One run's calls: a rig may run the step more than once.
    let _ = std::fs::remove_file(&calls);
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
    stub(&bin, "sg_read_attr", sg_read_attr);
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
        .args(extra)
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

// ------------------------------------------- step 12: the chip's preview (#511)

/// The question step 12 asks for the test cartridge's serial.
const SERIAL_QUESTION: &str = "medium serial of the TEST cartridge in the drive";

/// #511's first defect: on 2026-11-02 the drive was empty, the preview printed
/// nothing, and step 12 asked for the serial "as printed above". Now Not Ready
/// refuses with sg_read_attr's own words, before anything is asked, without a
/// second read, and the poll timer the step stopped is started again.
#[test]
fn step_12_refuses_a_drive_that_is_not_ready_before_asking_for_the_serial() {
    let rig = Rig::new();
    let calls = rig.p("calls");
    let (o, calls) = step12_sg(
        &rig,
        "inactive",
        "active",
        "/dev/null",
        &[],
        &format!(
            "echo \"sg_read_attr $*\" >> {}\n\
             echo 'Read attribute failed: Not ready, medium not present' >&2\nexit 2",
            calls.display()
        ),
    );
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(1), "{out}\n{err}\n{calls}");
    assert!(
        err.contains(
            "the drive is not ready: sg_read_attr /dev/null exited 2: \
             Read attribute failed: Not ready, medium not present"
        ) && err.contains("nothing was asked and nothing was erased"),
        "{err}"
    );
    assert!(
        !out.contains(SERIAL_QUESTION),
        "the serial was asked: {out}"
    );
    assert!(!out.contains("supplied via a flag"), "{out}");
    assert_eq!(
        calls.matches("sg_read_attr /dev/null").count(),
        1,
        "Not Ready is not read again: {calls}"
    );
    stop_then_start(&calls);
}

/// #511's first defect, the other two answers. A unit attention (the first
/// command after the cartridge loaded, 2026-11-02 23:32) is read again, once,
/// and the chip's serial is shown before the question. Any other failure says
/// what failed, with sg_read_attr's words and exit status, and the step goes on
/// to the question (the suite checks the serial against the chip itself).
#[test]
fn step_12_reads_the_chip_again_on_a_unit_attention_and_says_what_else_failed() {
    let rig = Rig::new();
    let calls = rig.p("calls");
    let count = rig.p("sg-count");
    let (o, calls_seen) = step12_sg(
        &rig,
        "inactive",
        "inactive",
        "/dev/null",
        &[],
        &format!(
            "echo \"sg_read_attr $*\" >> {calls}\n\
             n=$(cat {count} 2>/dev/null || echo 0); echo $((n + 1)) > {count}\n\
             if [ \"$n\" = 0 ]; then echo 'Read attribute failed: Unit attention' >&2; exit 6; fi\n\
             echo '  Medium serial number: TESTSERIAL'",
            calls = calls.display(),
            count = count.display()
        ),
    );
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(0), "{out}\n{err}\n{calls_seen}");
    assert!(
        out.contains("a unit attention (exit 6)") && out.contains("reading the chip again"),
        "{out}"
    );
    assert_eq!(
        calls_seen.matches("sg_read_attr /dev/null").count(),
        2,
        "{calls_seen}"
    );
    let serial = out
        .find("Medium serial number: TESTSERIAL")
        .unwrap_or_else(|| panic!("the chip's serial was not shown: {out}"));
    let question = out.find(SERIAL_QUESTION).unwrap_or_else(|| panic!("{out}"));
    assert!(serial < question, "{out}");

    let (o, calls_seen) = step12_sg(
        &rig,
        "inactive",
        "inactive",
        "/dev/null",
        &[],
        &format!(
            "echo \"sg_read_attr $*\" >> {}\n\
             echo 'sg_read_attr: open error: Permission denied' >&2\nexit 15",
            calls.display()
        ),
    );
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(0), "{out}\n{err}\n{calls_seen}");
    let failed = out
        .find(
            "sg_read_attr /dev/null failed (exit 15): sg_read_attr: open error: Permission denied",
        )
        .unwrap_or_else(|| panic!("{out}"));
    let question = out.find(SERIAL_QUESTION).unwrap_or_else(|| panic!("{out}"));
    assert!(failed < question, "{out}");
    assert_eq!(
        calls_seen.matches("sg_read_attr /dev/null").count(),
        1,
        "{calls_seen}"
    );
}

// ------------------------------------------- step 12: a cartridge the catalog knows

/// A home initialised by the binary itself, holding two cartridges: `B1`, whose
/// serial is only claimed (`operator_serial` S1, as `cartridge register --serial`
/// writes it), and `B2`, whose serial S2 the chip reported (`serial_number`, which
/// only a MAM read writes, so the test writes it into the catalog directly).
fn home_with_cartridges(rig: &Rig) -> PathBuf {
    let home = rig.p("tapectl-home");
    let tc = |args: &[&str]| {
        let o = Command::new(env!("CARGO_BIN_EXE_tapectl"))
            .arg("--home")
            .arg(&home)
            .args(args)
            .env("HOME", rig.p("h"))
            .env("TAPECTL_HOME_POINTER", rig.p("home-pointer"))
            .env_remove("TAPECTL_HOME")
            .output()
            .expect("spawn tapectl");
        let (out, err) = text(&o);
        assert!(o.status.success(), "tapectl {args:?}: {out}\n{err}");
    };
    tc(&["init", "--operator", "fr-tester", "--no-escrow"]);
    tc(&[
        "cartridge",
        "register",
        "--barcode",
        "B1",
        "--generation",
        "LTO-6",
        "--serial",
        "S1",
    ]);
    tc(&[
        "cartridge",
        "register",
        "--barcode",
        "B2",
        "--generation",
        "LTO-6",
    ]);
    let conn = rusqlite::Connection::open(home.join("tapectl.db")).unwrap();
    let n = conn
        .execute(
            "UPDATE cartridges SET serial_number = 'S2' WHERE barcode = 'B2'",
            [],
        )
        .unwrap();
    assert_eq!(n, 1);
    home
}

/// Item 192: the rehearsal erases the cartridge it is given, so a serial or
/// barcode the catalog has recorded is refused by name, after the consent and
/// before the suite runs, whichever of the three it matches; and the poll timer
/// the step stopped is started again. The suite is never started: its command
/// line is not in the output.
#[test]
fn step_12_refuses_a_cartridge_its_catalog_knows() {
    let rig = Rig::new();
    let home = home_with_cartridges(&rig);
    let home_s = home.to_str().unwrap();
    for (given, cartridge) in [("S1", "B1"), ("S2", "B2"), ("B1", "B1")] {
        let (o, calls) = step12_with(
            &rig,
            "inactive",
            "active",
            "/dev/null",
            &["--home", home_s, "--barcode", given],
        );
        let (out, err) = text(&o);
        assert_eq!(o.status.code(), Some(1), "{given}: {out}\n{err}\n{calls}");
        assert!(
            err.contains(&format!("{given} is a cartridge this catalog knows"))
                && err.contains(&format!("cartridge info {cartridge})")),
            "{given}: {err}"
        );
        assert!(err.contains("Nothing was erased"), "{err}");
        assert!(
            out.contains("supplied via a flag"),
            "the refusal came after the consent: {out}"
        );
        assert!(!out.contains("lifecycle-suite.sh"), "the suite ran: {out}");
        stop_then_start(&calls);
    }
}

/// Item 192, fail-closed: with no catalog to ask (a home that does not exist),
/// nothing proves the cartridge is not one the catalog knows, so the rehearsal
/// is refused too, and the suite never runs.
#[test]
fn step_12_refuses_when_the_catalog_cannot_say() {
    let rig = Rig::new();
    let missing = rig.p("no-such-home");
    let (o, calls) = step12_with(
        &rig,
        "inactive",
        "inactive",
        "/dev/null",
        &["--home", missing.to_str().unwrap(), "--barcode", "S9"],
    );
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(1), "{out}\n{err}\n{calls}");
    assert!(
        err.contains("cannot read this catalog's cartridges"),
        "{err}"
    );
    assert!(!out.contains("lifecycle-suite.sh"), "the suite ran: {out}");
}

// ------------------------------------------- the installed home outside ~/.tapectl

/// `tapectl --home HOME ...` as the test account, asserting success; its stdout.
fn tapectl_at(rig: &Rig, home: &Path, args: &[&str]) -> String {
    let o = Command::new(env!("CARGO_BIN_EXE_tapectl"))
        .arg("--home")
        .arg(home)
        .args(args)
        .env("HOME", rig.p("h"))
        .env("TAPECTL_HOME_POINTER", rig.p("home-pointer"))
        .env_remove("TAPECTL_HOME")
        .output()
        .expect("spawn tapectl");
    let (out, err) = text(&o);
    assert!(o.status.success(), "tapectl {args:?}: {out}\n{err}");
    out
}

/// ADR-0012 item 197: a profile's `TAPECTL_HOME_DIR` is the home every command
/// opens. Step 10 adds the profile's location to that home's catalog, and no
/// `~/.tapectl` is made: before the item, `tc` passed no `--home` for it, so the
/// step reached `$HOME/.tapectl`, which holds no catalog. A `--home` on the
/// command line still names a throwaway home over it.
#[test]
fn tapectl_home_dir_is_the_home_every_command_opens() {
    let rig = Rig::new();
    let home = rig.p("srv/tapectl/home");
    tapectl_at(
        &rig,
        &home,
        &["init", "--operator", "fr-tester", "--no-escrow"],
    );
    let profile = rig.profile(
        "p.profile",
        &format!(
            "TAPECTL_HOME_DIR={}\nLOCATIONS=(\n  \"t-rack|the test rack\"\n)\n",
            home.display()
        ),
    );
    let o = rig.run(&[
        "--profile",
        profile.to_str().unwrap(),
        "--auto",
        "--from",
        "10",
        "--to",
        "10",
        "--tapectl",
        env!("CARGO_BIN_EXE_tapectl"),
    ]);
    let out = ok(&o);
    assert!(out.contains(&format!("home: {}", home.display())), "{out}");
    let locations = tapectl_at(&rig, &home, &["location", "list"]);
    assert!(locations.contains("t-rack"), "{locations}");
    assert!(
        !rig.p("h/.tapectl").exists(),
        "a ~/.tapectl was made beside the home"
    );

    let other = rig.p("rehearsal-home");
    let o = rig.run(&[
        "--profile",
        profile.to_str().unwrap(),
        "--to",
        "0",
        "--home",
        other.to_str().unwrap(),
    ]);
    let out = ok(&o);
    assert!(out.contains(&format!("home: {}", other.display())), "{out}");
}

/// Item 197: tapectl ranks an account's own `~/.tapectl` above the pointer, so
/// one left beside a `TAPECTL_HOME_DIR` would be what a plain
/// `sudo -u tapectl -H tapectl` opens. The run refuses before step 1, naming
/// it; with it moved aside, the same run goes ahead. A relative path is refused.
#[test]
fn a_dot_tapectl_beside_tapectl_home_dir_is_refused() {
    let rig = Rig::new();
    let home = rig.p("srv/tapectl/home");
    let profile = rig.profile(
        "p.profile",
        &format!("TAPECTL_HOME_DIR={}\n", home.display()),
    );
    let p = profile.to_str().unwrap();
    std::fs::create_dir_all(rig.p("h/.tapectl")).unwrap();
    let o = rig.run(&["--profile", p, "--to", "0"]);
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(1), "{out}\n{err}");
    assert!(
        err.contains(&format!(
            "{} exists, and the home is {} (TAPECTL_HOME_DIR)",
            rig.p("h/.tapectl").display(),
            home.display()
        )),
        "{err}"
    );

    std::fs::rename(rig.p("h/.tapectl"), rig.p("h/.tapectl-old")).unwrap();
    ok(&rig.run(&["--profile", p, "--to", "0"]));

    let rel = rig.profile("rel.profile", "TAPECTL_HOME_DIR=srv/home\n");
    let o = rig.run(&["--profile", rel.to_str().unwrap(), "--to", "0"]);
    let (out, err) = text(&o);
    assert_eq!(o.status.code(), Some(2), "{out}\n{err}");
    assert!(err.contains("must be an absolute path"), "{err}");
}

/// Item 197: step 14 installs the timers for a `TAPECTL_HOME_DIR` with
/// `--tapectl-home` (the units, tapectl-op and the pointer name it), and a
/// `--home` stays a throwaway home whose install is only planned. The stubs
/// record `sudo` and do nothing, and the installer refuses the test account
/// (not a user), so nothing reaches the host either way.
#[test]
fn step_14_installs_for_tapectl_home_dir_and_only_plans_for_a_home() {
    let rig = Rig::new();
    let bin = rig.p("bin");
    let calls = rig.p("calls");
    let calls_s = calls.display();
    stub(
        &bin,
        "sudo",
        &format!("echo \"sudo $*\" >> {calls_s}; exit 97"),
    );
    stub(&bin, "systemctl", "exit 0");
    std::fs::create_dir_all(rig.p("units")).unwrap();
    let home = rig.p("srv/tapectl/home");
    let profile = rig.profile(
        "p.profile",
        &format!("TAPECTL_HOME_DIR={}\n", home.display()),
    );
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let step14 = |extra: &[&str]| {
        rig.command(&[
            "--profile",
            profile.to_str().unwrap(),
            "--auto",
            "--from",
            "14",
            "--to",
            "14",
            "--tapectl",
            env!("CARGO_BIN_EXE_tapectl"),
        ])
        .args(extra)
        .env("PATH", &path)
        .env("TAPECTL_UNIT_DIR", rig.p("units"))
        .output()
        .expect("spawn first-run.sh")
    };
    let installer = |out: &str| -> String {
        out.lines()
            .find(|l| l.contains("$ bash ") && l.contains("scripts/install-systemd.sh"))
            .unwrap_or_else(|| panic!("no installer command line:\n{out}"))
            .to_string()
    };

    let o = step14(&[]);
    let out = ok(&o);
    let line = installer(&out);
    assert!(
        line.contains(&format!("--tapectl-home {}", home.display())) && !line.contains("--dry-run"),
        "{line}"
    );
    assert!(
        out.contains(&format!("Its home: {}", home.display())),
        "{out}"
    );

    let other = rig.p("rehearsal-home");
    let o = step14(&["--home", other.to_str().unwrap()]);
    let out = ok(&o);
    let line = installer(&out);
    assert!(
        line.contains("--dry-run") && line.contains(&format!("--tapectl-home {}", other.display())),
        "{line}"
    );
    assert!(!out.contains(&home.display().to_string()), "{out}");
    let recorded = std::fs::read_to_string(&calls).unwrap_or_default();
    assert!(!recorded.contains("systemctl enable"), "{recorded}");
}
