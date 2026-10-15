//! `contrib/systemd/tapectl-scheduled-backup.sh` and the units the installer
//! renders (issue #470). Runs the real wrapper with `TAPECTL_BIN` pointing at
//! a stub, and the real installer in `--render-dir` mode, which installs
//! nothing. Ungated: no device, no catalog, no root.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

fn stub(dir: &Path, name: &str, body: &str) {
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A `tapectl` whose `db backup --to X` writes a SQLite header to X and
/// exits `backup_rc` (no file at all when it is not 0), and whose `db fsck`
/// exits 0.
fn tapectl_stub(bin: &Path, backup_rc: i32) {
    stub(
        bin,
        "tapectl",
        &format!(
            r#"if [ "$1 $2" = "db backup" ]; then
  [ {backup_rc} = 0 ] || exit {backup_rc}
  printf 'SQLite format 3\000 stub' > "$4"; exit 0
fi
exit 0"#
        ),
    );
}

struct Run {
    code: i32,
    output: String,
    /// The `tapectl-*.db` files left in the backup directory, sorted.
    left: Vec<String>,
}

fn run(backup_rc: i32, keep: u32, existing: &[&str]) -> Run {
    let dir = TempDir::new().unwrap();
    let bin = dir.path().join("bin");
    let backups = dir.path().join("backups");
    std::fs::create_dir(&bin).unwrap();
    std::fs::create_dir(&backups).unwrap();
    tapectl_stub(&bin, backup_rc);
    for name in existing {
        std::fs::write(backups.join(name), b"SQLite format 3\0older").unwrap();
    }
    let out = Command::new("bash")
        .arg("contrib/systemd/tapectl-scheduled-backup.sh")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("TAPECTL_BIN", bin.join("tapectl"))
        .env("TAPECTL_BACKUP_DIR", &backups)
        .env("TAPECTL_BACKUP_KEEP", keep.to_string())
        .env_remove("TAPECTL_HEALTHCHECK_URL")
        .output()
        .expect("bash must be available");
    let mut left: Vec<String> = std::fs::read_dir(&backups)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("tapectl-") && n.ends_with(".db"))
        .collect();
    left.sort();
    Run {
        code: out.status.code().unwrap_or(-1),
        output: format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
        left,
    }
}

/// The name of the copy this run wrote, from its own report line.
fn written(r: &Run) -> String {
    let line = r
        .output
        .lines()
        .find(|l| l.starts_with("backup: /"))
        .unwrap_or_else(|| panic!("no 'backup: <path>' line:\n{}", r.output));
    let path = line["backup: ".len()..].split(' ').next().unwrap();
    Path::new(path)
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

/// #470: with every existing copy named in the future (a clock that went
/// backwards since they were written), the new copy sorts last. It must be
/// kept, and count toward KEEP.
#[test]
fn a_backwards_clock_never_prunes_the_copy_just_written() {
    let future: Vec<String> = (1..=5)
        .map(|d| format!("tapectl-299901{d:02}T000000Z.db"))
        .collect();
    let refs: Vec<&str> = future.iter().map(String::as_str).collect();
    let r = run(0, 3, &refs);
    assert_eq!(r.code, 0, "{}", r.output);
    let new = written(&r);
    assert!(
        r.left.contains(&new),
        "the copy just written was pruned: {:?}\n{}",
        r.left,
        r.output
    );
    assert_eq!(r.left.len(), 3, "KEEP=3 counts the new copy: {:?}", r.left);
    assert!(
        r.output.contains("named later than this run's copy"),
        "the future-named copies are not reported:\n{}",
        r.output
    );
}

/// The ordinary case is unchanged: the newest KEEP survive, the new one first.
#[test]
fn retention_keeps_the_newest_including_the_new_copy() {
    let past = [
        "tapectl-20200101T000000Z.db",
        "tapectl-20200102T000000Z.db",
        "tapectl-20200103T000000Z.db",
    ];
    let r = run(0, 2, &past);
    assert_eq!(r.code, 0, "{}", r.output);
    let new = written(&r);
    assert_eq!(
        r.left,
        vec!["tapectl-20200103T000000Z.db".to_string(), new],
        "{}",
        r.output
    );
    assert!(!r.output.contains("named later"), "{}", r.output);
}

/// A busy catalog is 75, with no file left behind and nothing pruned.
#[test]
fn a_busy_catalog_exits_75_and_prunes_nothing() {
    let past = ["tapectl-20200101T000000Z.db", "tapectl-20200102T000000Z.db"];
    let r = run(75, 1, &past);
    assert_eq!(r.code, 75, "{}", r.output);
    assert_eq!(
        r.left.len(),
        2,
        "a busy run pruned or left a file: {:?}",
        r.left
    );
}

fn render(backup_dir: &str) -> (i32, String, TempDir) {
    let dir = TempDir::new().unwrap();
    let out = Command::new("bash")
        .arg("scripts/install-systemd.sh")
        .args(["--render-dir"])
        .arg(dir.path().join("r"))
        .args(["--user", "tapectl-render-test-nobody", "--home"])
        .arg(dir.path().join("svc"))
        .args([
            "--backup-dir",
            backup_dir,
            "--tapectl",
            "/usr/local/bin/tapectl",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("bash must be available");
    (
        out.status.code().unwrap_or(-1),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
        dir,
    )
}

fn unit_lines(dir: &Path, unit: &str) -> Vec<String> {
    std::fs::read_to_string(dir.join("r").join(unit))
        .unwrap_or_else(|e| panic!("{unit} not rendered: {e}"))
        .lines()
        .map(str::to_string)
        .collect()
}

/// The rendered units: exit 75 is success for the backup and the audit, and
/// the backup unit may write its directory under ProtectSystem=strict.
#[test]
fn the_rendered_units_take_75_as_success_and_let_the_backup_write() {
    let (code, out, dir) = render("/srv/backups-render-test");
    assert_eq!(code, 0, "{out}");
    let backup = unit_lines(dir.path(), "tapectl-backup.service");
    assert!(
        backup.iter().any(|l| l == "SuccessExitStatus=75"),
        "{backup:?}"
    );
    let rw = backup
        .iter()
        .find(|l| l.starts_with("ReadWritePaths="))
        .expect("the backup unit has ReadWritePaths");
    assert!(
        rw.split(['=', ' '])
            .any(|p| p == "/srv/backups-render-test"),
        "the backup directory is not writable under ProtectSystem=strict: {rw}"
    );
    let audit = unit_lines(dir.path(), "tapectl-audit.service");
    assert!(
        audit.iter().any(|l| l == "SuccessExitStatus=1 75"),
        "{audit:?}"
    );
}

/// A path the sed rendering cannot carry is refused by name, not rendered
/// into a broken unit.
#[test]
fn a_backup_dir_with_an_ampersand_is_refused() {
    let (code, out, dir) = render("/srv/a&b");
    assert_ne!(code, 0, "{out}");
    assert!(out.contains("contains | & or"), "{out}");
    assert!(!dir.path().join("r").join("tapectl-backup.service").exists());
}
