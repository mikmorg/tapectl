//! RESTORE.sh, the heir's script: the Stage 6 fixes of T6.1's reads (#574 to
//! #578), each a case that failed before its fix. The harness is the one
//! `tests/heir_restore.rs` uses (`tests/common/heir/mod.rs`).

#[path = "common/heir/mod.rs"]
mod heir;

use std::sync::OnceLock;

use heir::{Heir, Shape, Tape, Who};

fn two() -> &'static Tape {
    static T: OnceLock<Tape> = OnceLock::new();
    T.get_or_init(|| Tape::build(&Shape::two()))
}

fn key(h: &Heir, who: Who) -> String {
    h.key(who).to_string_lossy().into_owned()
}

/// #575: the Heir Kit's sheet prescribes `--to ./out`; a relative destination
/// must not make the per-file check fail every file.
#[test]
fn a_relative_to_and_work_restore_complete_with_an_empty_failure_list() {
    let t = two();
    let h = Heir::of(t);
    let cwd = h.dest("cwd");
    let mut c = h.command(&[
        "--restore",
        "--key",
        &key(&h, Who::TenantA),
        "--to",
        "./out",
        "--work",
        "./wk",
    ]);
    c.current_dir(&cwd);
    let o = c.output().unwrap();
    let (code, stdout, stderr) = (
        o.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&o.stdout).into_owned(),
        String::from_utf8_lossy(&o.stderr).into_owned(),
    );
    assert_eq!(code, 0, "{stdout}\n{stderr}");
    assert!(stdout.contains("RESTORE COMPLETE"), "{stdout}");
    assert!(!cwd.join("out/.tapectl-damaged-files.txt").exists());
    assert!(
        !cwd.join("out").read_dir().unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".tapectl-work")),
        "work directory left in --to"
    );
}

