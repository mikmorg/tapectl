//! #543's low findings in `scripts/first-run.sh` and `docs/install.md`.
//!
//! The upgrade needs a release tag, an installed binary and a catalog, so most
//! of these read the script's text, as `first_run_refusal_phrases.rs` does; the
//! tape lock's one line is run for real. The rest is the coordinator's run of
//! `scripts/upgrade-mhvtl.sh` (the report of T5.5 names it).

use std::path::Path;
use std::process::Command;

fn read(rel: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(rel))
        .unwrap_or_else(|e| panic!("read {rel}: {e}"))
}

/// The text of upgrade step `n`: from its `uhdr n` line to the next `uhdr`.
fn upgrade_step(script: &str, n: u32) -> String {
    let start = script
        .find(&format!("\nuhdr {n} "))
        .unwrap_or_else(|| panic!("no upgrade step {n}"));
    let rest = &script[start + 1..];
    let end = rest[5..].find("\nuhdr ").map_or(rest.len(), |e| e + 5);
    rest[..end].to_string()
}

#[test]
fn steps_4_and_5_refuse_without_the_records_of_steps_2_and_3() {
    let s = read("scripts/first-run.sh");
    assert!(
        s.contains("\nneed_rollback() {"),
        "need_rollback is not defined"
    );
    for n in [4, 5] {
        assert!(
            upgrade_step(&s, n).contains("need_rollback"),
            "upgrade step {n} does not require step 2's backup and step 3's kept binary"
        );
    }
    // The helper checks both records.
    let f = &s[s.find("\nneed_rollback() {").unwrap()..];
    let f = &f[..f.find("\n}\n").unwrap()];
    assert!(
        f.contains("ustate backup") && f.contains("\"$KEEP\""),
        "{f}"
    );
}

#[test]
fn a_reused_step_2_backup_states_its_age_and_is_not_reused_past_a_bound() {
    let s = read("scripts/first-run.sh");
    let step = upgrade_step(&s, 2);
    assert!(step.contains("TAPECTL_UPGRADE_BACKUP_MAX_AGE"), "{step}");
    assert!(step.contains("stat -c %Y"), "the backup's age is not read");
    assert!(
        step.contains("$U_BACKUP_AGE_TEXT old"),
        "the age is not said"
    );
    assert!(
        step.contains("is not reused, a new one is taken"),
        "a stale backup is still reused"
    );
}

#[test]
fn step_11_says_what_the_backup_service_did() {
    let s = read("scripts/first-run.sh");
    let step = upgrade_step(&s, 11);
    assert!(
        !step.contains("ok \"tapectl-backup.service ran\""),
        "step 11 still reports success whatever the service did"
    );
    assert!(step.contains("-newermt"), "no check for a copy written");
    assert!(
        step.contains("exited 75") && step.contains("wrote NO copy"),
        "exit 75 is not named"
    );
}

#[test]
fn step_7_leaves_the_host_home_pointer_alone_under_home() {
    let s = read("scripts/first-run.sh");
    // home_pointer: under --home only the caller's TAPECTL_HOME_POINTER, never
    // the host's /etc/tapectl/home; without --home the host's is the default.
    let at = s.find("home_pointer() {").expect("home_pointer");
    let body = &s[at..at + s[at..].find("\n}\n").expect("home_pointer's end")];
    let (under_home, otherwise) = body
        .split_once("else")
        .expect("home_pointer has an --home branch and an else");
    assert!(
        under_home.contains("if [ -n \"$HOME_DIR\" ]")
            && under_home.contains("${TAPECTL_HOME_POINTER:-}")
            && !under_home.contains("/etc/tapectl/home"),
        "under --home the pointer falls back to the host's: {body}"
    );
    assert!(
        otherwise.contains("${TAPECTL_HOME_POINTER:-/etc/tapectl/home}"),
        "without --home the host's pointer is the default: {body}"
    );
    // Positive control: the host's path appears in the script at all, so the
    // count below searched something.
    assert!(s.contains("/etc/tapectl/home"));
    // Every place step 7 names the pointer goes through home_pointer.
    assert_eq!(
        s.matches("${TAPECTL_HOME_POINTER:-/etc/tapectl/home}")
            .count(),
        1,
        "a pointer default outside home_pointer"
    );
    // The ops-group block (it writes the pointer) runs only with one.
    let at = s.find("THE OPS GROUP.").expect("the ops-group block");
    let line = s[..at]
        .lines()
        .rev()
        .find(|l| l.starts_with("if [ -n \"$OPS_GROUP\""))
        .expect("the ops-group block's condition");
    assert!(
        line.contains("[ -n \"$(home_pointer)\" ]"),
        "the block that writes the pointer is not guarded by home_pointer: {line}"
    );
    // The stale-pointer removal before init removes only home_pointer's.
    assert!(
        s.contains("_ptr=\"$(home_pointer)\"\n  if [ -n \"$_ptr\" ]"),
        "the stale-pointer removal does not go through home_pointer"
    );
}

#[test]
fn the_rollback_puts_the_old_releases_wrappers_back_too() {
    let s = read("scripts/first-run.sh");
    let step = upgrade_step(&s, 13);
    assert!(
        step.contains("--from 14 --to 14") && step.contains("checkout v%s"),
        "step 13's rollback is the binary alone: {step}"
    );
    let doc = read("docs/install.md");
    let at = doc.find("**Rolling back.**").expect("Rolling back");
    let sec = &doc[at..at + 3500];
    assert!(sec.contains("--from 14 --to 14"), "{sec}");
}

#[test]
fn install_md_agrees_with_tapectl_op_on_volume_init() {
    let wrapper = read("contrib/systemd/tapectl-op.in");
    assert!(
        wrapper.contains("compact-read | init | identify)"),
        "tapectl-op.in no longer lists init as a tape command"
    );
    let doc = read("docs/install.md");
    assert!(doc.contains("`init` and\n`identify`"), "the list");
    assert!(
        !doc.contains("`volume init` is not in the list"),
        "install.md still says volume init is not in tapectl-op's list"
    );
}

/// The lock's fixed name in /tmp is not followed when it is a symbolic link.
#[test]
fn the_tape_lock_does_not_follow_a_symlink() {
    let s = read("scripts/first-run.sh");
    let line = s
        .lines()
        .find(|l| l.starts_with("tape_lock() {"))
        .expect("tape_lock");
    let dir = tempfile::tempdir().unwrap();
    let victim = dir.path().join("victim");
    std::fs::write(&victim, "precious").unwrap();
    let lock = dir.path().join("lock");
    std::os::unix::fs::symlink(&victim, &lock).unwrap();
    let prog = format!(
        "die() {{ echo \"$*\" >&2; exit 7; }}\nTAPE_LOCK={}\n{line}\ntape_lock\necho locked\n",
        lock.display()
    );
    let o = Command::new("bash").arg("-c").arg(&prog).output().unwrap();
    assert_eq!(o.status.code(), Some(7), "{o:?}");
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "precious");
    // Positive control: a plain file is taken.
    std::fs::remove_file(&lock).unwrap();
    let o = Command::new("bash").arg("-c").arg(&prog).output().unwrap();
    assert!(
        String::from_utf8_lossy(&o.stdout).contains("locked"),
        "{o:?}"
    );
}
