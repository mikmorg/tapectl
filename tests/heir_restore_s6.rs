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

// ---- helpers ----

fn unit_files<'a>(t: &'a Tape, name: &str) -> &'a [(String, Vec<u8>)] {
    &t.units.iter().find(|u| u.name == name).unwrap().files
}

fn damaged_list(dest: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(dest.join(".tapectl-damaged-files.txt"))
        .unwrap_or_default()
        .lines()
        .skip(1)
        .map(str::to_string)
        .collect()
}

fn dstr(p: &std::path::Path) -> &str {
    p.to_str().unwrap()
}

/// The damaged-file list of unit `name` of `t`, naming `paths`.
fn write_list(t: &Tape, name: &str, dest: &std::path::Path, paths: &[&str]) {
    let mut s = format!(
        "#tapectl-damaged-files\t1\tunit={name}\tuuid={}\tversion=1\n",
        t.unit_uuids[name]
    );
    for p in paths {
        s.push_str(p);
        s.push('\n');
    }
    std::fs::write(dest.join(".tapectl-damaged-files.txt"), s).unwrap();
}

// ---- #574 ----

/// #574, first branch: a later run with `--no-file-check` into a destination
/// that holds a damaged-file list, every folder present. It feeds the files
/// the list names, never says that every file passes, and says it checked
/// nothing.
#[test]
fn a_continuation_with_no_file_check_feeds_the_listed_files_and_claims_no_pass() {
    let t = two();
    let h = Heir::of(t);
    let dest = h.dest("out");
    let kb = key(&h, Who::TenantB);
    let o = h.run(&["--restore", "--key", &kb, "--to", dstr(&dest)]);
    assert_eq!(o.code, 0, "{}", o.all());
    let victim = "notes.txt";
    let want = &unit_files(t, heir::UNIT_B)
        .iter()
        .find(|(p, _)| p == victim)
        .unwrap()
        .1;
    std::fs::remove_file(dest.join(victim)).unwrap();
    write_list(t, heir::UNIT_B, &dest, &[victim]);
    let o = h.run(&[
        "--restore",
        "--key",
        &kb,
        "--to",
        dstr(&dest),
        "--no-file-check",
    ]);
    let all = o.all();
    assert!(!all.contains("is here and passes"), "{all}");
    assert!(
        o.stderr.contains("--no-file-check: the restored files"),
        "{all}"
    );
    assert_eq!(
        std::fs::read(dest.join(victim)).ok().as_ref(),
        Some(want),
        "the listed file is fed again: {all}"
    );
}

/// #574, second branch: a folder is missing too, so the run restores
/// something already; the listed file must still be fed.
#[test]
fn a_continuation_with_no_file_check_feeds_the_listed_files_when_a_folder_is_missing() {
    let t = two();
    let h = Heir::of(t);
    let dest = h.dest("out");
    let kb = key(&h, Who::TenantB);
    let o = h.run(&["--restore", "--key", &kb, "--to", dstr(&dest)]);
    assert_eq!(o.code, 0, "{}", o.all());
    let victim = "notes.txt";
    let want = &unit_files(t, heir::UNIT_B)
        .iter()
        .find(|(p, _)| p == victim)
        .unwrap()
        .1;
    std::fs::remove_file(dest.join(victim)).unwrap();
    std::fs::remove_dir_all(dest.join("inner")).unwrap();
    write_list(t, heir::UNIT_B, &dest, &[victim]);
    let o = h.run(&[
        "--restore",
        "--key",
        &kb,
        "--to",
        dstr(&dest),
        "--no-file-check",
    ]);
    let all = o.all();
    assert!(!all.contains("is here and passes"), "{all}");
    assert!(dest.join("inner").is_dir(), "the folder is restored: {all}");
    assert_eq!(
        std::fs::read(dest.join(victim)).ok().as_ref(),
        Some(want),
        "the listed file is fed again: {all}"
    );
}

/// #574: a continuation from a tape that does not hold the listed files keeps
/// them on the list and does not say COMPLETE.
#[test]
fn a_continuation_with_no_file_check_keeps_what_it_could_not_restore() {
    let t = span2();
    let h = Heir::of_series(&[&t[1], &t[0]]);
    let dest = h.dest("out");
    let ka = key(&h, Who::TenantA);
    let args = ["--restore", "--key", &ka, "--all", "--to", dstr(&dest)];
    let o = h.run(&args);
    assert_eq!(o.code, 1, "{}", o.all());
    let before = damaged_list(&dest);
    assert!(!before.is_empty());
    let mut again = args.to_vec();
    again.push("--no-file-check");
    let o = h.run(&again);
    let all = o.all();
    assert_eq!(o.code, 1, "{all}");
    assert!(!all.contains("RESTORE COMPLETE"), "{all}");
    assert!(!all.contains("is here and passes"), "{all}");
    // What slice 1 held stays listed; a path this tape holds whole may leave.
    let after = damaged_list(&dest);
    assert!(after.contains(&"a-symlink".to_string()), "{after:?}\n{all}");
    assert!(
        after.iter().all(|p| before.contains(p)),
        "{before:?} -> {after:?}\n{all}"
    );
}

fn span2() -> &'static [Tape] {
    static T: OnceLock<Vec<Tape>> = OnceLock::new();
    T.get_or_init(|| {
        let mut shape = Shape::one("none");
        shape.span = Some(heir::Span {
            min_copies: 1,
            warns: vec![Some(0), None],
        });
        Tape::build_series(&shape)
    })
}

// ---- #576 ----

/// #576: a refused cartridge at the prompt does not cost the run the tape it
/// started on. Start on the tape of slices 2 to 8, refuse at slice 1's prompt
/// (a blank cartridge), reload the start tape, and answer `done` (or ENTER
/// first): what that tape holds is restored.
#[test]
fn a_refused_cartridge_does_not_cost_the_run_its_home_tape() {
    let t = span2();
    // What the start tape alone gives, with `done` at once.
    let base = Heir::of_series(&[&t[1], &t[0]]);
    let want_dest = base.dest("out");
    let ka = key(&base, Who::TenantA);
    let o = base.run(&["--restore", "--key", &ka, "--all", "--to", dstr(&want_dest)]);
    assert_eq!(o.code, 1, "{}", o.all());
    let want_tree = heir::tree(&want_dest);
    let want_list = damaged_list(&want_dest);
    assert!(!want_tree.is_empty() && !want_list.is_empty());
    for enter_first in [false, true] {
        let h = Heir::of_series(&[&t[1], &t[0]]);
        h.shelve_blank("BLANK");
        let dest = h.dest("out");
        let o = h.run_prompted(
            &["--restore", "--key", &ka, "--all", "--to", dstr(&dest)],
            |k, _| match (k, enter_first) {
                (1, _) => {
                    h.load("BLANK");
                    Some(String::new())
                }
                (2, false) => {
                    h.load("HEIR02");
                    Some("done".into())
                }
                (2, true) => {
                    h.load("HEIR02");
                    Some(String::new())
                }
                (3, true) => Some("done".into()),
                _ => panic!("asked {k} times"),
            },
        );
        let all = o.all();
        assert_eq!(o.code, 1, "enter_first={enter_first}: {all}");
        assert_eq!(heir::tree(&dest), want_tree, "enter_first={enter_first}: {all}");
        assert_eq!(damaged_list(&dest), want_list, "enter_first={enter_first}");
        assert!(
            !all.contains("NOT ON THIS CARTRIDGE: slices 2"),
            "enter_first={enter_first}: {all}"
        );
    }
}

// ---- #577 ----

/// #577: a later run into the same `--to` from another copy, units whose
/// names hold a `/`: the damaged-file list under `TO/tv/a/s01` is found.
#[test]
fn a_later_run_finds_the_list_of_a_unit_with_a_hierarchical_name() {
    let mut shape = Shape::two();
    shape.units = vec![
        ("tv/a/s01".into(), heir::TENANT_A, false),
        ("tv/a/s02".into(), heir::TENANT_A, false),
    ];
    shape.copies = 2;
    let t = Tape::build_series(&shape);
    let h1 = Heir::of(&t[0]);
    let ka = key(&h1, Who::TenantA);
    // Slice 1 of each unit is bad on the first copy.
    let all_slices = t[0].slice_positions();
    let second = t[0].units[0].slices;
    h1.flip(all_slices[0], 500);
    h1.flip(all_slices[second], 500);
    let dest = h1.dest("out");
    let o = h1.run(&["--restore", "--key", &ka, "--all", "--to", dstr(&dest)]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(dest.join("tv/a/s01/.tapectl-damaged-files.txt").is_file());
    let h2 = Heir::of(&t[1]);
    let o = h2.run(&["--restore", "--key", &ka, "--all", "--to", dstr(&dest)]);
    let all = o.all();
    assert!(!all.contains("is not empty"), "{all}");
    assert_eq!(o.code, 0, "{all}");
    assert!(all.contains("RESTORE COMPLETE"), "{all}");
    for name in ["tv/a/s01", "tv/a/s02"] {
        for (p, bytes) in unit_files(&t[0], name) {
            assert_eq!(
                &std::fs::read(dest.join(name).join(p)).unwrap(),
                bytes,
                "{name}/{p}"
            );
        }
    }
}

