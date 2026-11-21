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

/// ADR-0012 items 303 and 306 (#594 C-1): the unit's root is a member, and
/// RESTORE.sh's own tar (`--keep-old-files`, as tapectl's) gives an empty
/// `--to` its recorded mode and mtime, measured with GNU tar 1.35. With the
/// real tar and a real staging, so it is the stream's `././` member that is
/// read. The per-file check and the damaged-file list know nothing of it.
#[test]
fn an_empty_to_gets_the_unit_roots_recorded_mode_and_mtime() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let t = two();
    let h = Heir::of(t);
    let dest = h.dest("root-metadata");
    let before = std::fs::metadata(&dest).unwrap();
    assert_ne!(
        before.permissions().mode() & 0o7777,
        heir::ROOT_MODE,
        "positive control: the empty --to is not already the recorded root"
    );
    let o = h.run(&[
        "--restore",
        "--key",
        &key(&h, Who::TenantA),
        "--to",
        dstr(&dest),
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(o.stdout.contains("RESTORE COMPLETE"), "{}", o.all());
    let after = std::fs::metadata(&dest).unwrap();
    assert_eq!(after.permissions().mode() & 0o7777, heir::ROOT_MODE);
    assert_eq!(after.mtime(), heir::ROOT_MTIME_SECS);
    assert!(!dest.join(".tapectl-damaged-files.txt").exists());
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
    // Nothing is checked, so nothing leaves the list: the run is incomplete.
    assert_eq!(o.code, 1, "{all}");
    assert_eq!(damaged_list(&dest), vec![victim.to_string()], "{all}");
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
    assert_eq!(o.code, 1, "{all}");
    assert_eq!(damaged_list(&dest), vec![victim.to_string()], "{all}");
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
    // Nothing is checked, so no path leaves the list.
    let after = damaged_list(&dest);
    assert!(
        before.iter().all(|p| after.contains(p)),
        "{before:?} -> {after:?}\n{all}"
    );
}

/// #574 (round 2): the reader's three runs. Run 1 lists the hard-linked name
/// whose first name is in slice 1; run 2 under `--no-file-check` must not
/// take it off the list (tar cannot make it); run 3, the repair WITH the
/// check, on the tape holding slice 1, makes it and is the only verdict.
#[test]
fn a_no_file_check_run_never_takes_a_path_off_the_list_and_the_repair_then_completes() {
    let t = span2();
    let h = Heir::of_series(&[&t[1], &t[0]]);
    let dest = h.dest("out");
    let ka = key(&h, Who::TenantA);
    let args = ["--restore", "--key", &ka, "--all", "--to", dstr(&dest)];
    let o = h.run(&args);
    assert_eq!(o.code, 1, "{}", o.all());
    let small = "alpha-unit-small.txt".to_string();
    assert!(damaged_list(&dest).contains(&small), "run 1");
    let mut again = args.to_vec();
    again.push("--no-file-check");
    let o = h.run(&again);
    let all = o.all();
    assert_eq!(o.code, 1, "{all}");
    assert!(damaged_list(&dest).contains(&small), "run 2: {all}");
    assert!(
        !all.contains("The per-file check below decides"),
        "no check runs under the flag: {all}"
    );
    // The positive control: tar did end non-zero, and the flag's line says so.
    assert!(all.contains("tar ended with status"), "{all}");
    assert!(all.contains("--no-file-check: no check runs"), "{all}");
    // Run 3: the repair, with the check, on the tape that holds slice 1.
    let h1 = Heir::of(&t[0]);
    let list = dest.join(".tapectl-damaged-files.txt");
    let o = h1.run(&[
        "--restore",
        "--key",
        &ka,
        "--to",
        dstr(&dest),
        "--paths-from",
        dstr(&list),
    ]);
    let all = o.all();
    assert_eq!(o.code, 0, "{all}");
    assert!(dest.join(&small).is_file(), "{all}");
    assert!(!list.exists(), "{all}");
}

/// #574 (round 2): a FRESH `--no-file-check` restore from a tape lacking
/// slice 1 lists the hard-linked name whose first name is in slice 1.
#[test]
fn a_fresh_no_file_check_restore_lists_a_hard_link_whose_first_name_was_not_read() {
    let t = span2();
    let h = Heir::of_series(&[&t[1], &t[0]]);
    let dest = h.dest("out");
    let ka = key(&h, Who::TenantA);
    let o = h.run(&[
        "--restore",
        "--key",
        &ka,
        "--all",
        "--to",
        dstr(&dest),
        "--no-file-check",
    ]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        damaged_list(&dest).contains(&"alpha-unit-small.txt".to_string()),
        "{}",
        o.all()
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
        assert_eq!(
            heir::tree(&dest),
            want_tree,
            "enter_first={enter_first}: {all}"
        );
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

// ---- #596 heir-C1 ----

/// #596 heir-C1: the refusal of a non-empty `--to` that holds no damaged-file
/// list walks the tree. Bash's `**` glob built the whole list of a tree in
/// memory (9 s on a tree of 1,000,000 entries on this host); `find` stops at
/// the first list and needs 0.4 s for none. A `find` that logs is put in the
/// PATH: the walk is its job, and the list at any depth is still found.
#[test]
fn the_walk_under_a_non_empty_to_is_find_and_still_finds_a_list_at_any_depth() {
    let t = two();
    let h = Heir::of(t);
    let real = heir::host_tool("find").expect("find");
    h.put_tool(
        "find",
        &format!(
            "#!/bin/bash\necho \"$*\" >>\"$FAKE_TAPE/find.log\"\nexec {} \"$@\"\n",
            real.display()
        ),
    );
    let ka = key(&h, Who::TenantA);
    let dest = h.dest("out");
    std::fs::create_dir_all(dest.join("some/deep/dir")).unwrap();
    std::fs::write(dest.join("some/deep/dir/file"), "x").unwrap();
    let o = h.run(&["--restore", "--key", &ka, "--to", dstr(&dest)]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stderr.contains("is not empty. Restore into"),
        "{}",
        o.all()
    );
    let log = std::fs::read_to_string(h.root.path().join("state/find.log"))
        .unwrap_or_else(|_| panic!("the walk was not find: {}", o.all()));
    assert!(log.contains(".tapectl-damaged-files.txt"), "{log}");
    // A list three levels down is found: the refusal of the destination is
    // past, and it is the unit's own directory that is judged (its words
    // differ).
    write_list(t, heir::UNIT_A, &dest.join("some/deep/dir"), &[]);
    let o = h.run(&["--restore", "--key", &ka, "--to", dstr(&dest)]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stderr.contains("holds no damaged-file list"),
        "{}",
        o.all()
    );
}

// ---- #578 ----

/// #578: `rage` linked as `age` says `No matching keys found` for an envelope
/// the key is not for. That is "not for this key", never damage.
#[test]
fn rages_words_for_a_foreign_envelope_are_not_damage() {
    let t = two();
    let h = Heir::of(t);
    let real = heir::heir_age_binary();
    h.put_tool(
        "age",
        &format!(
            "#!/bin/bash\nerr=$(mktemp)\n{real} \"$@\" 2>\"$err\"\nrc=$?\nsed 's/no identity matched any of the file.s recipients/Error: No matching keys found/' \"$err\" >&2\nrm -f \"$err\"\nexit $rc\n",
            real = real.display()
        ),
    );
    let dest = h.dest("out");
    let o = h.run(&[
        "--restore",
        "--key",
        &key(&h, Who::TenantA),
        "--unit",
        heir::UNIT_A,
        "--to",
        dstr(&dest),
    ]);
    let all = o.all();
    assert!(!all.contains("DAMAGED"), "{all}");
    assert_eq!(o.code, 0, "{all}");
    // The stand-in really speaks rage's words.
    let o = h.run(&["--list", "--key", &key(&h, Who::TenantA)]);
    assert!(!o.all().contains("DAMAGED"), "{}", o.all());
}

// ---- the prompt flow after "done" (#576, round 2) ----

/// A refused cartridge stays in the drive and the heir answers `done`: what
/// the start tape holds is said to be on a tape that is not in the drive,
/// never "this tape's plan does not hold it" and never sent to --where.
#[test]
fn done_with_the_refused_cartridge_still_in_the_drive_names_the_start_tape() {
    let t = span2();
    let h = Heir::of_series(&[&t[1], &t[0]]);
    h.shelve_blank("BLANK");
    let dest = h.dest("out");
    let ka = key(&h, Who::TenantA);
    let o = h.run_prompted(
        &["--restore", "--key", &ka, "--all", "--to", dstr(&dest)],
        |k, _| match k {
            1 => {
                h.load("BLANK");
                Some(String::new())
            }
            2 => Some("done".into()),
            _ => panic!("asked {k} times"),
        },
    );
    let all = o.all();
    assert_eq!(o.code, 1, "{all}");
    assert!(all.contains("NOT IN THE DRIVE: slices 2-8 of 8"), "{all}");
    assert!(all.contains("HEIR02"), "{all}");
    assert!(
        all.contains("load HEIR02") || all.contains("Load HEIR02"),
        "{all}"
    );
    assert!(
        all.contains("NOT ON THIS CARTRIDGE: slices 1 of 8"),
        "{all}"
    );
    for bad in [
        "slice 2: this tape's plan does not hold it",
        "No tape the index names holds slice(s) 2",
        "slices 1-8",
    ] {
        assert!(!all.contains(bad), "{bad}: {all}");
    }
}

/// The prompt tells the heir to eject the cartridge first; the heir loads
/// something else (or nothing) and types `done` without pressing ENTER. No
/// probe ran, yet the start tape's slices are not read from the drive and
/// called damaged or unreadable.
#[test]
fn done_after_the_start_tape_was_ejected_is_not_damage() {
    let t = span2();
    for empty in [false, true] {
        let h = Heir::of_series(&[&t[1], &t[0]]);
        h.shelve_blank("BLANK");
        let dest = h.dest("out");
        let ka = key(&h, Who::TenantA);
        let o = h.run_prompted(
            &["--restore", "--key", &ka, "--all", "--to", dstr(&dest)],
            |k, _| match k {
                1 => {
                    if empty {
                        h.empty_bay();
                    } else {
                        h.load("BLANK");
                    }
                    Some("done".into())
                }
                _ => panic!("asked {k} times"),
            },
        );
        let all = o.all();
        assert_eq!(o.code, 1, "empty={empty}: {all}");
        assert!(
            all.contains("NOT IN THE DRIVE: slices 2-8 of 8"),
            "empty={empty}: {all}"
        );
        for bad in [
            "differs from the front index",
            "could not be read at file",
            "manifest and the front index disagree",
        ] {
            assert!(!all.contains(bad), "empty={empty} {bad}: {all}");
        }
    }
}

// ---- #577 follow-up: the reason of a refused directory ----

/// A non-empty unit directory with no list is not "a list that is not this
/// unit's": it is said to hold none.
#[test]
fn a_complete_sibling_directory_is_refused_for_what_it_is() {
    let mut shape = Shape::two();
    shape.units = vec![
        ("tv/a/s01".into(), heir::TENANT_A, false),
        ("tv/a/s02".into(), heir::TENANT_A, false),
    ];
    shape.copies = 2;
    let t = Tape::build_series(&shape);
    let h1 = Heir::of(&t[0]);
    let ka = key(&h1, Who::TenantA);
    // Only the first unit's slice is bad; the second unit comes out whole.
    h1.flip(t[0].slice_positions()[0], 500);
    let dest = h1.dest("out");
    let o = h1.run(&["--restore", "--key", &ka, "--all", "--to", dstr(&dest)]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(!dest.join("tv/a/s02/.tapectl-damaged-files.txt").exists());
    let h2 = Heir::of(&t[1]);
    let o = h2.run(&["--restore", "--key", &ka, "--all", "--to", dstr(&dest)]);
    let all = o.all();
    assert_eq!(o.code, 1, "{all}");
    assert!(all.contains("holds no damaged-file list"), "{all}");
    assert!(!all.contains("not this unit's and version's"), "{all}");
}

// ---- round 3 ----

fn sibling_pair() -> Vec<Tape> {
    let mut shape = Shape::two();
    shape.units = vec![
        ("tv/a/s01".into(), heir::TENANT_A, false),
        ("tv/a/s02".into(), heir::TENANT_A, false),
    ];
    shape.copies = 2;
    Tape::build_series(&shape)
}

/// heir2-1: a start tape whose File 0 states no uuid is still the cartridge
/// in the drive after `done`: its slices are restored.
#[test]
fn done_on_a_start_tape_whose_file_0_has_no_uuid_still_restores_what_it_holds() {
    let t = span2();
    let base = Heir::of_series(&[&t[1], &t[0]]);
    let want_dest = base.dest("out");
    let ka = key(&base, Who::TenantA);
    base.run(&["--restore", "--key", &ka, "--all", "--to", dstr(&want_dest)]);
    let want_tree = heir::tree(&want_dest);
    assert!(!want_tree.is_empty());
    let h = Heir::of_series(&[&t[1], &t[0]]);
    let text = String::from_utf8_lossy(&t[1].files[0]).into_owned();
    let text = text.trim_end_matches('\0');
    assert!(text.lines().any(|l| l.trim_start().starts_with("uuid")));
    let edited: String = text
        .lines()
        .filter(|l| !l.trim_start().starts_with("uuid"))
        .map(|l| format!("{l}\n"))
        .collect();
    h.replace(0, edited.as_bytes());
    let dest = h.dest("out");
    let o = h.run(&["--restore", "--key", &ka, "--all", "--to", dstr(&dest)]);
    let all = o.all();
    assert!(!all.contains("NOT IN THE DRIVE"), "{all}");
    assert_eq!(heir::tree(&dest), want_tree, "{all}");
}

/// heir2-2: the remedy the refusal names works and ends in a continuation.
#[test]
fn the_refusal_of_a_sibling_names_a_command_that_continues_the_damaged_unit() {
    let t = sibling_pair();
    let h1 = Heir::of(&t[0]);
    let ka = key(&h1, Who::TenantA);
    h1.flip(t[0].slice_positions()[0], 500);
    let dest = h1.dest("out");
    let o = h1.run(&["--restore", "--key", &ka, "--all", "--to", dstr(&dest)]);
    assert_eq!(o.code, 1, "{}", o.all());
    let h2 = Heir::of(&t[1]);
    let o = h2.run(&["--restore", "--key", &ka, "--all", "--to", dstr(&dest)]);
    let all = o.all();
    let remedy = format!("--unit tv/a/s01 --to {}/tv/a/s01", dstr(&dest));
    assert!(all.contains(&remedy), "{remedy}: {all}");
    let unit_dir = dest.join("tv/a/s01");
    let o = h2.run(&[
        "--restore",
        "--key",
        &ka,
        "--unit",
        "tv/a/s01",
        "--to",
        dstr(&unit_dir),
    ]);
    let all = o.all();
    assert_eq!(o.code, 0, "{all}");
    assert!(all.contains("this run adds to it"), "{all}");
    assert!(!unit_dir.join(".tapectl-damaged-files.txt").exists());
}

/// heir2-3: once the start tape is not in the drive the next prompt names it.
#[test]
fn the_prompt_names_the_start_tape_once_another_cartridge_was_taken() {
    let t = span2();
    let h = Heir::of_series(&[&t[1], &t[0]]);
    let dest = h.dest("out");
    let ka = key(&h, Who::TenantA);
    let o = h.run_prompted(
        &["--restore", "--key", &ka, "--all", "--to", dstr(&dest)],
        |k, text| match k {
            1 => {
                h.load("HEIR01");
                Some(String::new())
            }
            2 => {
                let after = &text[text.rfind("needs slice 2").expect("slice 2 prompt")..];
                assert!(
                    after.contains("tape HEIR02 (cartridge serial")
                        && after.contains("the tape this restore started on"),
                    "{after}"
                );
                Some("done".into())
            }
            _ => panic!("asked {k} times"),
        },
    );
    let all = o.all();
    assert!(all.contains("NOT IN THE DRIVE: slices 2-8 of 8"), "{all}");
    assert!(
        !all.contains("No tape the index names holds slice(s) 2"),
        "{all}"
    );
    assert!(!all.contains("NOT ON THIS CARTRIDGE: slices 2"), "{all}");
}

/// heir2-4: quit stops; nothing after the prompt is fetched or fed, and
/// nothing is called damage.
#[test]
fn quit_stops_reading() {
    let t = span2();
    let h = Heir::of_series(&[&t[1], &t[0]]);
    h.shelve_blank("BLANK");
    let dest = h.dest("out");
    let ka = key(&h, Who::TenantA);
    let o = h.run_prompted(
        &["--restore", "--key", &ka, "--all", "--to", dstr(&dest)],
        |k, _| match k {
            1 => {
                h.load("BLANK");
                Some("quit".into())
            }
            _ => panic!("asked {k} times"),
        },
    );
    let all = o.all();
    assert_eq!(o.code, 1, "{all}");
    assert!(all.contains("RESTORE STOPPED"), "{all}");
    for bad in ["could not be read", "differs from the front index"] {
        assert!(!all.contains(bad), "{bad}: {all}");
    }
    assert!(
        h.reads_by_cartridge().last().unwrap().is_empty(),
        "{:?}",
        h.reads_by_cartridge()
    );
}

/// heir2-5: a checked continuation removes every listed path that passes,
/// fed or not, and keeps the one that still fails.
#[test]
fn a_checked_continuation_removes_every_listed_path_that_passes() {
    let mut shape = Shape::two();
    shape.copies = 2;
    let t = Tape::build_series(&shape);
    let h1 = Heir::of(&t[0]);
    let kb = key(&h1, Who::TenantB);
    let dest = h1.dest("out");
    let o = h1.run(&["--restore", "--key", &kb, "--to", dstr(&dest)]);
    assert_eq!(o.code, 0, "{}", o.all());
    std::fs::remove_file(dest.join("inner/mid-1.dat")).unwrap();
    write_list(
        &t[0],
        heir::UNIT_B,
        &dest,
        &["notes.txt", "inner/mid-1.dat"],
    );
    // The second copy is damaged in every slice, so the missing file cannot
    // come back; notes.txt is whole on disk.
    let h2 = Heir::of(&t[1]);
    for p in t[1].slice_positions() {
        h2.flip(p, 500);
    }
    let o = h2.run(&["--restore", "--key", &kb, "--to", dstr(&dest)]);
    let all = o.all();
    assert_eq!(o.code, 1, "{all}");
    assert_eq!(
        damaged_list(&dest),
        vec!["inner/mid-1.dat".to_string()],
        "{all}"
    );
}

// ---- item 301: a flagged run whose tar failed is incomplete (#596 heir2-C1) ----

/// The unit B tape with tar standing in as one that exits 2 after the real
/// tar restored into `dest`.
fn failing_tar_heir() -> (Heir, std::path::PathBuf) {
    let h = Heir::of(two());
    let dest = h.dest("out");
    let real = heir::host_tool("tar").unwrap();
    h.put_tool("tar", &heir::failing_tar(&real, &dest));
    (h, dest)
}

/// Item 301, a fresh run: the flag skips the check, not tar's report.
#[test]
fn a_flagged_fresh_restore_whose_tar_failed_is_incomplete() {
    let (h, dest) = failing_tar_heir();
    let kb = key(&h, Who::TenantB);
    let o = h.run(&[
        "--restore",
        "--key",
        &kb,
        "--to",
        dstr(&dest),
        "--no-file-check",
    ]);
    let all = o.all();
    assert!(all.contains("tar ended with status 2"), "{all}");
    assert!(all.contains("RESTORE INCOMPLETE"), "{all}");
    assert!(!all.contains("RESTORE COMPLETE"), "{all}");
    assert_eq!(o.code, 1, "{all}");
}

/// Item 301, a `--path` run.
#[test]
fn a_flagged_path_restore_whose_tar_failed_is_incomplete() {
    let (h, dest) = failing_tar_heir();
    let kb = key(&h, Who::TenantB);
    let o = h.run(&[
        "--restore",
        "--key",
        &kb,
        "--unit",
        heir::UNIT_B,
        "--path",
        "notes.txt",
        "--to",
        dstr(&dest),
        "--no-file-check",
    ]);
    let all = o.all();
    assert!(all.contains("tar ended with status 2"), "{all}");
    assert!(all.contains("RESTORE INCOMPLETE"), "{all}");
    assert!(!all.contains("RESTORE COMPLETE"), "{all}");
    assert_eq!(o.code, 1, "{all}");
}

/// Item 301, a repair (its list is in `--to`, as the documented form has it).
#[test]
fn a_flagged_repair_whose_tar_failed_is_incomplete() {
    let t = two();
    let (h, dest) = failing_tar_heir();
    let good = Heir::of(t);
    let kb = key(&good, Who::TenantB);
    let o = good.run(&["--restore", "--key", &kb, "--to", dstr(&dest)]);
    assert_eq!(o.code, 0, "{}", o.all());
    write_list(t, heir::UNIT_B, &dest, &["notes.txt"]);
    let list = dest.join(".tapectl-damaged-files.txt");
    let kb = key(&h, Who::TenantB);
    let o = h.run(&[
        "--restore",
        "--key",
        &kb,
        "--to",
        dstr(&dest),
        "--paths-from",
        dstr(&list),
        "--no-file-check",
    ]);
    let all = o.all();
    assert!(all.contains("tar ended with status 2"), "{all}");
    assert!(all.contains("RESTORE INCOMPLETE"), "{all}");
    assert_eq!(o.code, 1, "{all}");
    assert_eq!(damaged_list(&dest), vec!["notes.txt".to_string()], "{all}");
}

/// Item 301's positive control: a flagged whole restore whose tar exited 0
/// still ends complete, exit 0 (`scripts/mhvtl-verify-gate.sh` expects it).
#[test]
fn a_flagged_whole_restore_whose_tar_exited_zero_is_complete() {
    let h = Heir::of(two());
    let kb = key(&h, Who::TenantB);
    let dest = h.dest("out");
    let o = h.run(&[
        "--restore",
        "--key",
        &kb,
        "--to",
        dstr(&dest),
        "--no-file-check",
    ]);
    let all = o.all();
    assert!(all.contains("RESTORE COMPLETE"), "{all}");
    assert_eq!(o.code, 0, "{all}");
}

// ---- #596 heir2-6: what to do about slices not in the drive ----

/// `--path` has no continuation: "run the same command again" dies "already
/// in DEST". The remedy named is `--paths-from` with the damaged-file list.
#[test]
fn not_in_the_drive_for_a_path_run_names_paths_from_not_the_same_command() {
    let t = span2();
    let h = Heir::of_series(&[&t[1], &t[0]]);
    h.shelve_blank("BLANK");
    let dest = h.dest("out");
    let ka = key(&h, Who::TenantA);
    // Slice 1 (the first name) is asked for first; the blank cartridge is
    // loaded and refused, and `notes.txt`, in a slice of the start tape, is
    // then not read from the drive.
    let o = h.run_prompted(
        &[
            "--restore",
            "--key",
            &ka,
            "--unit",
            heir::UNIT_A,
            "--path",
            "alpha-unit-small.txt",
            "--path",
            "notes.txt",
            "--to",
            dstr(&dest),
        ],
        |k, _| match k {
            1 => {
                h.load("BLANK");
                Some("done".into())
            }
            _ => panic!("asked {k} times"),
        },
    );
    let all = o.all();
    assert!(all.contains("NOT IN THE DRIVE"), "{all}");
    assert!(all.contains("--paths-from"), "{all}");
    assert!(!all.contains("run the same command again"), "{all}");
    assert!(all.contains("Load HEIR02"), "{all}");
}

/// `--all` with a unit that came out whole: the same command is refused for
/// that unit's directory. The remedy names the units that did not.
#[test]
fn not_in_the_drive_for_all_names_only_the_units_that_did_not_come_out_whole() {
    let mut shape = Shape::two();
    shape.units[0].2 = true;
    shape.span = Some(heir::Span {
        min_copies: 1,
        warns: vec![Some(0), None],
    });
    let t = Tape::build_series(&shape);
    let h = Heir::of_series(&[&t[1], &t[0]]);
    h.shelve_blank("BLANK");
    let dest = h.dest("out");
    let ko = key(&h, Who::Operator);
    let o = h.run_prompted(
        &["--restore", "--key", &ko, "--all", "--to", dstr(&dest)],
        |_, _| {
            h.load("BLANK");
            Some("done".into())
        },
    );
    let all = o.all();
    assert!(all.contains("NOT IN THE DRIVE"), "{all}");
    assert!(all.contains("did not come out whole"), "{all}");
}

// ---- #596 heir2-7: a repair into a directory that holds no list ----

/// The list of unit A at a path of its own, for `--paths-from` into a `--to`
/// that holds none.
fn list_outside(h: &Heir, t: &Tape, paths: &[&str]) -> std::path::PathBuf {
    let d = h.dest("lists");
    std::fs::create_dir_all(&d).unwrap();
    write_list(t, heir::UNIT_A, &d, paths);
    d.join(".tapectl-damaged-files.txt")
}

/// A flagged repair whose `--to` holds no list checked nothing: it neither
/// says a list is removed (none was there) nor ends COMPLETE; what it asked
/// for goes on a list a checked run can judge.
#[test]
fn a_flagged_repair_into_a_directory_with_no_list_is_not_complete() {
    let t = span2();
    let h = Heir::of(&t[0]);
    let ka = key(&h, Who::TenantA);
    let list = list_outside(&h, &t[0], &["alpha-unit-small.txt"]);
    let dest = h.dest("out");
    let o = h.run(&[
        "--restore",
        "--key",
        &ka,
        "--to",
        dstr(&dest),
        "--paths-from",
        dstr(&list),
        "--no-file-check",
    ]);
    let all = o.all();
    assert!(
        !all.contains("is removed: nothing is left to repair"),
        "{all}"
    );
    assert!(!all.contains("RESTORE COMPLETE"), "{all}");
    assert_eq!(o.code, 1, "{all}");
    assert!(dest.join("alpha-unit-small.txt").is_file(), "{all}");
    assert!(
        damaged_list(&dest).contains(&"alpha-unit-small.txt".to_string()),
        "{all}"
    );
}

/// The same repair with the check on says nothing of a list that was never
/// there, and ends complete.
#[test]
fn a_checked_repair_into_a_directory_with_no_list_does_not_claim_to_remove_one() {
    let t = span2();
    let h = Heir::of(&t[0]);
    let ka = key(&h, Who::TenantA);
    let list = list_outside(&h, &t[0], &["alpha-unit-small.txt"]);
    let dest = h.dest("out");
    let o = h.run(&[
        "--restore",
        "--key",
        &ka,
        "--to",
        dstr(&dest),
        "--paths-from",
        dstr(&list),
    ]);
    let all = o.all();
    assert!(
        !all.contains("is removed: nothing is left to repair"),
        "{all}"
    );
    assert!(all.contains("RESTORE COMPLETE"), "{all}");
    assert_eq!(o.code, 0, "{all}");
}

/// A hard-linked name whose first name is on a cartridge not given is neither
/// made nor, before this, listed under the flag: it is listed.
#[test]
fn a_flagged_repair_lists_a_hard_link_whose_first_name_is_absent() {
    let t = span2();
    let h = Heir::of_series(&[&t[1], &t[0]]);
    let ka = key(&h, Who::TenantA);
    let list = list_outside(&h, &t[0], &["alpha-unit-small.txt"]);
    let dest = h.dest("out");
    let o = h.run_prompted(
        &[
            "--restore",
            "--key",
            &ka,
            "--to",
            dstr(&dest),
            "--paths-from",
            dstr(&list),
            "--no-file-check",
        ],
        |_, _| Some("done".into()),
    );
    let all = o.all();
    assert!(
        damaged_list(&dest).contains(&"alpha-unit-small.txt".to_string()),
        "{all}"
    );
    assert_eq!(o.code, 1, "{all}");
}

// ---- #596 heir2-8: what the probe found on `done` ----

fn span3() -> &'static [Tape] {
    static T: OnceLock<Vec<Tape>> = OnceLock::new();
    T.get_or_init(|| {
        let mut shape = Shape::one("none");
        shape.span = Some(heir::Span {
            min_copies: 1,
            warns: vec![Some(0), Some(0), None],
        });
        Tape::build_series(&shape)
    })
}

/// `done` typed with the cartridge that holds the asked slice in the drive
/// (after another cartridge was taken, HOME_CART 0): the reason says what the
/// probe found, not that the cartridge does not hold it.
#[test]
fn done_with_the_cartridge_that_holds_the_slice_in_the_drive_says_so() {
    let t = span3();
    let h = Heir::of_series(&[&t[2], &t[0], &t[1]]);
    let dest = h.dest("out");
    let ka = key(&h, Who::TenantA);
    let o = h.run_prompted(
        &["--restore", "--key", &ka, "--all", "--to", dstr(&dest)],
        |k, _| match k {
            1 => {
                h.load("HEIR01");
                Some(String::new())
            }
            2 => {
                h.load("HEIR02");
                Some("done".into())
            }
            _ => panic!("asked {k} times"),
        },
    );
    let all = o.all();
    assert!(
        !all.contains("the cartridge in the drive does not hold it"),
        "{all}"
    );
    assert!(
        all.contains("the cartridge in the drive does hold it"),
        "{all}"
    );
}

/// `done` with a refused cartridge in the drive: the reason keeps its suffix,
/// "no cartridge that holds it was given", which is true there.
#[test]
fn done_with_a_refused_cartridge_keeps_the_no_cartridge_was_given_suffix() {
    let t = span2();
    let h = Heir::of_series(&[&t[1], &t[0]]);
    h.shelve_blank("BLANK");
    let dest = h.dest("out");
    let ka = key(&h, Who::TenantA);
    let o = h.run_prompted(
        &["--restore", "--key", &ka, "--all", "--to", dstr(&dest)],
        |k, _| match k {
            1 => {
                h.load("BLANK");
                Some("done".into())
            }
            _ => panic!("asked {k} times"),
        },
    );
    let all = o.all();
    assert!(
        all.contains("slice 1: this tape's plan does not hold it (it is on another cartridge); no cartridge that holds it was given"),
        "{all}"
    );
}

/// #548 T6.2-3: `--info`'s rows for the front index and the seal marker had
/// a blank size (the index cannot hold its own); each now says its size.
#[test]
fn info_gives_the_front_index_and_seal_marker_rows_a_size() {
    let t = two();
    let h = Heir::of(t);
    let o = h.run(&["--info"]);
    assert_eq!(o.code, 0, "{}", o.all());
    for kind in ["front_index", "seal_marker"] {
        let row = o
            .stdout
            .lines()
            .find(|l| l.split_whitespace().nth(1) == Some(kind))
            .unwrap_or_else(|| panic!("no {kind} row: {}", o.stdout));
        let size: Vec<&str> = row.split_whitespace().skip(2).collect();
        assert!(
            size.len() >= 2 && size[0].parse::<u64>().is_ok_and(|n| n > 0) && size[1] == "bytes",
            "the {kind} row has no size: {row:?}"
        );
    }
}

// ---- T6.4-1 (#552): names uutils quotes ----

/// uutils' `sha256sum -c` quotes a name with a space, a quote, a `;`, a `*`,
/// a `$`, `,@%+=:` or a non-ASCII byte under `LC_ALL=C`. The per-file check
/// reads no name back, so a whole restore of such names is COMPLETE.
fn quoted_tape() -> &'static Tape {
    static T: OnceLock<Tape> = OnceLock::new();
    T.get_or_init(|| {
        let mut shape = Shape::one("none");
        shape.quoted_names = true;
        Tape::build(&shape)
    })
}

#[test]
fn a_whole_restore_of_names_a_tool_quotes_is_complete_with_no_damaged_file() {
    let t = quoted_tape();
    let h = Heir::of(t);
    let dest = h.dest("quoted");
    let o = h.run(&[
        "--restore",
        "--key",
        &key(&h, Who::TenantA),
        "--to",
        dstr(&dest),
    ]);
    let all = o.all();
    assert_eq!(o.code, 0, "{all}");
    assert!(o.stdout.contains("RESTORE COMPLETE"), "{all}");
    assert!(!dest.join(".tapectl-damaged-files.txt").exists(), "{all}");
    for n in heir::QUOTED_NAMES {
        assert!(dest.join(n).is_file(), "{n} is restored: {all}");
    }
}

#[test]
fn a_damaged_file_among_quoted_names_is_the_only_one_reported() {
    let t = quoted_tape();
    let h = Heir::of(t);
    let dest = h.dest("quoted-damaged");
    let real = heir::host_tool("tar").expect("tar");
    h.put_tool("tar", &heir::corrupting_tar(&real, "caf\u{e9}.txt"));
    let o = h.run(&[
        "--restore",
        "--key",
        &key(&h, Who::TenantA),
        "--to",
        dstr(&dest),
    ]);
    let all = o.all();
    assert_eq!(o.code, 1, "{all}");
    assert!(o.stdout.contains("RESTORE INCOMPLETE"), "{all}");
    assert_eq!(
        damaged_list(&dest),
        vec!["caf\u{e9}.txt".to_string()],
        "{all}"
    );
}
