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

/// D1 (#561, ADR-0012 item 271): the restore keeps the unit root's recorded
/// mtime (item 303) without writing outside `--to`: with TMPDIR a directory it
/// cannot write, the root still gets its recorded mtime to the nanosecond, and
/// with a writable TMPDIR nothing at all is created there during the restore
/// (inotify sees every file made there, however briefly).
#[test]
fn the_roots_mtime_is_kept_and_nothing_is_written_to_tmpdir() {
    use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let t = two();
    let mode = |p: &std::path::Path, m: u32| {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap()
    };
    // A TMPDIR the script cannot write.
    let h = Heir::of(t);
    let tmp = h.root.path().join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    mode(&tmp, 0o555);
    assert!(
        std::fs::File::create(tmp.join("probe")).is_err(),
        "positive control: TMPDIR is not writable (not root)"
    );
    let dest = h.dest("ro-tmp");
    let o = h.run(&[
        "--restore",
        "--key",
        &key(&h, Who::TenantA),
        "--to",
        dstr(&dest),
    ]);
    mode(&tmp, 0o755);
    assert_eq!(o.code, 0, "{}", o.all());
    let m = std::fs::metadata(&dest).unwrap();
    assert_eq!(
        (m.mtime(), m.mtime_nsec()),
        (heir::ROOT_MTIME_SECS, 0),
        "{}",
        o.all()
    );
    // A writable TMPDIR: nothing is made in it.
    let h = Heir::of(t);
    let tmp = h.root.path().join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let ino = Inotify::init(InitFlags::IN_NONBLOCK).unwrap();
    ino.add_watch(&tmp, AddWatchFlags::IN_CREATE | AddWatchFlags::IN_MOVED_TO)
        .unwrap();
    let dest = h.dest("rw-tmp");
    let o = h.run(&[
        "--restore",
        "--key",
        &key(&h, Who::TenantA),
        "--to",
        dstr(&dest),
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    let m = std::fs::metadata(&dest).unwrap();
    assert_eq!((m.mtime(), m.mtime_nsec()), (heir::ROOT_MTIME_SECS, 0));
    let made: Vec<String> = match ino.read_events() {
        Ok(evs) => evs.iter().map(|e| format!("{:?}", e.name)).collect(),
        Err(nix::errno::Errno::EAGAIN) => Vec::new(),
        Err(e) => panic!("inotify: {e}"),
    };
    assert_eq!(
        made,
        Vec::<String>::new(),
        "made in TMPDIR during a restore"
    );
}

/// #611 D-6 (ADR-0012 item 313): where a file held open cannot be removed (an
/// NFS client renames it `.nfsXXXX` in its directory until it is closed), the
/// work directory is still removed: the reference file that keeps the root's
/// mtime is let go, the directory removed, and a NOTE says the time is not
/// kept. A stand-in `rm` acts as that client: while the script holds fd 8 (the
/// reference file, which `rm` inherits), removing the work directory leaves
/// the held file as `.nfs0001` and fails; once fd 8 is closed it is the real
/// `rm`. Without the stand-in the root keeps its time to the nanosecond.
#[test]
fn the_work_directory_is_removed_where_a_file_held_open_cannot_be() {
    use std::os::unix::fs::MetadataExt;
    let t = two();
    let h = Heir::of(t);
    let real = heir::host_tool("rm").expect("rm");
    h.put_tool(
        "rm",
        &format!(
            "#!/bin/bash\n\
             for a; do\n\
               case ${{a##*/}} in\n\
               .tapectl-work.*)\n\
                 if [ -d \"$a\" ] && [ -e /proc/self/fd/8 ]; then\n\
                   [ ! -e \"$a/.mtime.ref\" ] || mv \"$a/.mtime.ref\" \"$a/.nfs0001\"\n\
                   find \"$a\" -mindepth 1 -maxdepth 1 ! -name .nfs0001 -exec \"{real}\" -rf {{}} +\n\
                   echo \"rm: cannot remove '$a': Directory not empty\" >&2\n\
                   exit 1\n\
                 fi\n\
                 ;;\n\
               esac\n\
             done\n\
             exec \"{real}\" \"$@\"\n",
            real = real.display()
        ),
    );
    let dest = h.dest("nfs");
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
    let left: Vec<String> = std::fs::read_dir(&dest)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".tapectl-work"))
        .collect();
    assert!(left.is_empty(), "{left:?}: {all}");
    assert!(
        !all.contains("could not remove the work directory"),
        "{all}"
    );
    assert!(
        o.stderr.contains(&format!(
            "NOTE: the work directory's removal changed the modification time of {}, and it is not set back: ",
            dest.display()
        )),
        "{}",
        o.stderr
    );
    // The positive control: the stand-in did act (the time is not kept).
    assert_ne!(
        std::fs::metadata(&dest).unwrap().mtime(),
        heir::ROOT_MTIME_SECS,
        "{all}"
    );
    // Without it, the time is kept, to the nanosecond, and no NOTE says otherwise.
    let h = Heir::of(t);
    let dest = h.dest("local");
    let o = h.run(&[
        "--restore",
        "--key",
        &key(&h, Who::TenantA),
        "--to",
        dstr(&dest),
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    let m = std::fs::metadata(&dest).unwrap();
    assert_eq!((m.mtime(), m.mtime_nsec()), (heir::ROOT_MTIME_SECS, 0));
    assert!(!o.stderr.contains("modification time"), "{}", o.stderr);
}

/// #611 G4 (ADR-0012 item 313): where the work directory cannot be removed
/// at all (a stand-in `rm` that removes nothing of it, as an immutable file
/// or a file system gone read-only gives), the WARNING says so and no NOTE
/// says its removal changed the root's time: nothing was removed, and the
/// time is the recorded one.
#[test]
fn a_work_directory_that_cannot_be_removed_gives_the_warning_and_no_note_on_the_time() {
    use std::os::unix::fs::MetadataExt;
    let t = two();
    let h = Heir::of(t);
    let real = heir::host_tool("rm").expect("rm");
    h.put_tool(
        "rm",
        &format!(
            "#!/bin/bash\n\
             for a; do\n\
               case ${{a##*/}} in\n\
               .tapectl-work.*) echo \"rm: cannot remove '$a': Operation not permitted\" >&2; exit 1 ;;\n\
               esac\n\
             done\n\
             exec \"{}\" \"$@\"\n",
            real.display()
        ),
    );
    let dest = h.dest("stuck");
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
    assert!(
        o.stderr
            .contains("WARNING: could not remove the work directory"),
        "{}",
        o.stderr
    );
    assert!(!o.stderr.contains("modification time"), "{}", o.stderr);
    // The positive control: the stand-in removed nothing.
    let left: Vec<String> = std::fs::read_dir(&dest)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".tapectl-work"))
        .collect();
    assert_eq!(left.len(), 1, "{all}");
    let m = std::fs::metadata(&dest).unwrap();
    assert_eq!((m.mtime(), m.mtime_nsec()), (heir::ROOT_MTIME_SECS, 0));
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

// ---- #611, ADR-0012 item 313: a damaged tape ----

/// Make the read of tape file `n` of the cartridge in the drive fail with an
/// I/O error (the stand-in `dd`'s `.fail.N`).
fn fail_read(h: &Heir, n: usize) {
    std::fs::write(h.root.path().join("files").join(format!(".fail.{n}")), "").unwrap();
}

/// The text of tape file `n` of `t`, without its padding.
fn text_of(t: &Tape, n: usize) -> String {
    String::from_utf8_lossy(&t.files[n])
        .trim_end_matches('\0')
        .to_string()
}

const FILE0_WARNING: &str = "WARNING: File 0 cannot be read from /dev/nst0";

/// DI-1 (`volume-format-v3.md` 1.2 rule 2): a File 0 that gives a read error
/// is a damaged File 0, warned about and never a hard stop. Every mode runs
/// as it does on the sound tape, and nothing asks whether a cartridge is
/// loaded, since the rest of the tape reads.
#[test]
fn an_unreadable_file_0_is_warned_about_and_every_mode_runs_as_on_the_sound_tape() {
    let t = two();
    let good = Heir::of(t);
    let h = Heir::of(t);
    fail_read(&h, 0);
    let ka = key(&h, Who::TenantA);
    let ke = key(&h, Who::Escrow);
    // The label and the date in the index banner are File 0's; no other line
    // of these modes' output is. The script names itself by its path, which
    // is each harness's own.
    let body = |h: &Heir, s: &str| -> String {
        s.replace(h.root.path().to_str().unwrap(), "ROOT")
            .lines()
            .filter(|l| !l.starts_with("Index read from tape:"))
            .map(|l| format!("{l}\n"))
            .collect()
    };
    for args in [
        vec!["--find-envelope", "--key", &ka],
        vec!["--list", "--key", &ka],
        vec!["--where", heir::UNIT_A, "--key", &ka],
        vec!["--where", "--all", "--key", &ke],
        vec!["--find", "deep", "--key", &ke],
    ] {
        let want = good.run(&args);
        assert_eq!(want.code, 0, "{args:?} on the sound tape: {}", want.all());
        let o = h.run(&args);
        assert_eq!(o.code, want.code, "{args:?}: {}", o.all());
        assert!(o.stderr.contains(FILE0_WARNING), "{args:?}: {}", o.stderr);
        assert!(
            !o.all().contains("is a cartridge loaded"),
            "{args:?}: {}",
            o.all()
        );
        assert_eq!(body(&h, &o.stdout), body(&good, &want.stdout), "{args:?}");
    }

    // --info: the map and the seal, as on the sound tape; File 0's own facts
    // (the first two lines) are not known.
    let want = good.run(&["--info"]);
    let o = h.run(&["--info"]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(o.stderr.contains(FILE0_WARNING), "{}", o.stderr);
    let from_map = |s: &str| -> String { s.lines().skip(2).map(|l| format!("{l}\n")).collect() };
    assert!(
        from_map(&want.stdout).contains("the tape is complete, every planned file is on it"),
        "{}",
        want.stdout
    );
    assert_eq!(from_map(&o.stdout), from_map(&want.stdout));

    // --restore: every unit of the tape, each with its tenant's key, and every
    // file of it as the source had it.
    for (who, unit) in [(Who::TenantA, heir::UNIT_A), (Who::TenantB, heir::UNIT_B)] {
        let dest = h.dest(&format!("out-{unit}"));
        let o = h.run(&["--restore", "--key", &key(&h, who), "--to", dstr(&dest)]);
        let all = o.all();
        assert_eq!(o.code, 0, "{unit}: {all}");
        assert!(o.stderr.contains(FILE0_WARNING), "{unit}: {all}");
        assert!(o.stdout.contains("RESTORE COMPLETE"), "{unit}: {all}");
        let got = heir::tree(&dest);
        for (path, bytes) in unit_files(t, unit) {
            assert_eq!(got.get(path), Some(bytes), "{unit} {path}: {all}");
        }
        assert!(
            !dest.join(".tapectl-damaged-files.txt").exists(),
            "{unit}: {all}"
        );
    }
}

/// DI-1 and File 1's sentence on `--verify`: a file that will not read is a
/// FAIL, File 0 too, and the run does not end `VERIFY PASSED`. Every other
/// file is still judged.
#[test]
fn verify_fails_an_unreadable_file_0_and_judges_every_other_file() {
    let t = two();
    let h = Heir::of(t);
    fail_read(&h, 0);
    let o = h.run(&["--verify"]);
    let all = o.all();
    assert_eq!(o.code, 1, "{all}");
    assert!(o.stderr.contains(FILE0_WARNING), "{all}");
    let fail: Vec<&str> = o.stdout.lines().filter(|l| l.starts_with("FAIL")).collect();
    assert_eq!(fail.len(), 1, "{all}");
    assert!(
        fail[0].starts_with("FAIL     0  ") && fail[0].contains("cannot be read from the tape"),
        "{all}"
    );
    assert_eq!(
        o.stdout.matches("PASS ").count(),
        t.files.len() - 1,
        "every other file has a PASS line: {all}"
    );
    assert!(!o.stdout.contains("VERIFY PASSED"), "{all}");
    assert!(o.stdout.contains("VERIFY FAILED: 1 file(s)"), "{all}");
}

/// DI-1 keeps the fence (ADR-0012 item 313): with File 0 unreadable the seal
/// marker's `requires` decides, and a feature this script does not know is
/// refused by name.
#[test]
fn an_unreadable_file_0_leaves_the_seals_requires_to_refuse_an_unknown_feature() {
    let t = two();
    let h = Heir::of(t);
    fail_read(&h, 0);
    let last = t.files.len() - 1;
    let seal = text_of(t, last);
    assert!(seal.contains("requires = []"), "{seal}");
    h.replace(
        last,
        seal.replace("requires = []", "requires = [\"future-thing\"]")
            .as_bytes(),
    );
    let dest = h.dest("out");
    for args in [
        vec!["--info"],
        vec![
            "--restore",
            "--key",
            &key(&h, Who::TenantA),
            "--to",
            dstr(&dest),
        ],
    ] {
        let o = h.run(&args);
        assert_eq!(o.code, 1, "{args:?}: {}", o.all());
        assert!(o.stderr.contains(FILE0_WARNING), "{args:?}: {}", o.stderr);
        assert!(
            o.stderr
                .contains("the seal marker requires feature(s) this RESTORE.sh does not know")
                && o.stderr.contains("future-thing"),
            "{args:?}: {}",
            o.stderr
        );
    }
    assert!(heir::tree(&dest).is_empty(), "nothing was restored");
}

/// DI-1: File 0 and the seal marker both unreadable. The tape is read as an
/// unsealed one from its front index, and the script says so, exactly as it
/// does for a File 0 that reads as garbage on a tape with no readable seal.
#[test]
fn an_unreadable_file_0_and_seal_read_as_an_unsealed_tape_as_a_garbage_file_0_does() {
    let t = two();
    let last = t.files.len() - 1;
    let garbage = Heir::of(t);
    garbage.replace(0, &heir::noise(4096, 7));
    fail_read(&garbage, last);
    let h = Heir::of(t);
    fail_read(&h, 0);
    fail_read(&h, last);
    let kb = key(&h, Who::TenantB);

    let want = garbage.run(&["--info"]);
    let o = h.run(&["--info"]);
    assert_eq!(want.code, 0, "{}", want.all());
    assert!(
        want.stdout.contains("Seal marker: NOT FOUND"),
        "{}",
        want.all()
    );
    assert_eq!(o.code, want.code, "{}", o.all());
    assert_eq!(o.stdout, want.stdout);
    assert!(
        o.stderr.contains(FILE0_WARNING)
            && o.stderr.contains("no seal marker found as the last file")
            && !o.all().contains("is a cartridge loaded"),
        "{}",
        o.stderr
    );

    let (dw, dh) = (garbage.dest("out"), h.dest("out"));
    let want = garbage.run(&["--restore", "--key", &kb, "--to", dstr(&dw)]);
    let o = h.run(&["--restore", "--key", &kb, "--to", dstr(&dh)]);
    assert_eq!(want.code, 0, "{}", want.all());
    assert_eq!(o.code, want.code, "{}", o.all());
    assert!(o.stdout.contains("RESTORE COMPLETE"), "{}", o.all());
    assert_eq!(heir::tree(&dh), heir::tree(&dw));
    let got = heir::tree(&dh);
    for (path, bytes) in unit_files(t, heir::UNIT_B) {
        assert_eq!(got.get(path), Some(bytes), "{path}");
    }
}

/// DI-1: the question the old message asked ("is a cartridge loaded and is
/// DEVICE the right drive?") is still the right one when NOTHING reads: an
/// empty drive, or a tape whose File 0, front index and seal marker all fail.
/// It is decided by the later files, never by File 0 alone.
#[test]
fn a_drive_that_gives_nothing_is_asked_about_and_the_run_stops() {
    let t = two();
    let empty = Heir::of(t);
    empty.empty_bay();
    let dead = Heir::of(t);
    for n in [0, 3, t.files.len() - 1] {
        fail_read(&dead, n);
    }
    for (name, h) in [
        ("an empty drive", &empty),
        ("three unreadable files", &dead),
    ] {
        let o = h.run(&["--info"]);
        assert_eq!(o.code, 1, "{name}: {}", o.all());
        assert!(
            o.stderr
                .contains("FATAL: nothing can be read from /dev/nst0")
                && o.stderr
                    .contains("is a cartridge loaded and is /dev/nst0 the right drive?")
                && o.stderr.contains("there is no map of the tape")
                && o.stderr.contains("fsf N"),
            "{name}: {}",
            o.stderr
        );
    }
}

/// DI-3 and File 1's sentence on `--verify` ("a file that is missing or will
/// not read is a FAIL, and it ends `VERIFY PASSED` only for a sealed tape
/// every file of which passed"): File 3 is a file of the tape as any other.
/// One that gives a read error, or reads as bytes that are no front index,
/// is a FAIL. The other files are still judged, against the seal marker's
/// copy of the map, and the last line names that map.
#[test]
fn verify_fails_a_front_index_that_did_not_read_and_names_the_map_it_judged_by() {
    let t = two();
    let unreadable = Heir::of(t);
    fail_read(&unreadable, 3);
    let garbage = Heir::of(t);
    garbage.replace(3, &heir::noise(4096, 11));
    for (name, h) in [("a read error", &unreadable), ("garbage", &garbage)] {
        let o = h.run(&["--verify"]);
        let all = o.all();
        assert_eq!(o.code, 1, "{name}: {all}");
        assert!(!o.stdout.contains("VERIFY PASSED"), "{name}: {all}");
        let fail: Vec<&str> = o.stdout.lines().filter(|l| l.starts_with("FAIL")).collect();
        assert_eq!(fail.len(), 1, "{name}: {all}");
        assert!(
            fail[0].starts_with("FAIL     3  front_index ")
                && fail[0].contains("the seal marker's copy of it"),
            "{name}: {all}"
        );
        assert_eq!(
            o.stdout.matches("PASS ").count(),
            t.files.len() - 1,
            "{name}: every other file is still judged: {all}"
        );
        assert!(
            o.stdout.lines().last().unwrap().starts_with(
                "VERIFY FAILED: 1 file(s) differ from the seal marker's copy of the front index or cannot be read; "
            ),
            "{name}: {all}"
        );
        assert!(
            o.stderr
                .contains("falling back to the seal marker's own copy of it"),
            "{name}: {all}"
        );
    }
}

/// What rewrites one value of a map's line: given the value as written, it
/// returns the value to write.
type Edit = fn(&str) -> String;

/// A front index's text with one line of its entry at `pos` rewritten: `edit`
/// is given the value of `key` as written and returns the value to write.
/// Every other byte stays: one damaged line of the plain-text map.
fn entry_line_edited(text: &str, pos: usize, key: &str, edit: Edit) -> String {
    let entry = text
        .find(&format!("\nposition = {pos}\n"))
        .unwrap_or_else(|| panic!("the map has no entry {pos}"));
    let start = entry
        + text[entry..]
            .find(&format!("\n{key} = "))
            .unwrap_or_else(|| panic!("entry {pos} has no {key}"))
        + 1;
    let end = start + text[start..].find('\n').unwrap_or(text.len() - start);
    assert!(
        !text[entry + 1..start].contains("[[files]]"),
        "{key} of entry {pos}"
    );
    let value = text[start..end].split_once(" = ").unwrap().1;
    format!("{}{key} = {}{}", &text[..start], edit(value), &text[end..])
}

/// File 3 of `t` with one line of its entry at `pos` rewritten.
fn front_index_with(t: &Tape, pos: usize, key: &str, edit: Edit) -> Vec<u8> {
    entry_line_edited(&text_of(t, 3), pos, key, edit).into_bytes()
}

/// A quoted sha256 with its first digit changed.
fn another_hash(v: &str) -> String {
    let mut b = v.as_bytes().to_vec();
    b[1] = if b[1] == b'0' { b'1' } else { b'0' };
    String::from_utf8(b).unwrap()
}

/// A size one byte greater.
fn one_more(v: &str) -> String {
    (v.parse::<u64>().unwrap() + 1).to_string()
}

/// The tape files of tenant A's envelope on `t`, and of the other tenant's:
/// `(A's primary, its backup, the other tenant's primary)`. A's is the one its
/// key opens on the sound tape, and a backup is the `_backup` entry with its
/// primary's ciphertext hash.
fn tenant_envelopes(t: &Tape) -> (usize, usize, usize) {
    let h = Heir::of(t);
    let o = h.run(&["--find-envelope", "--key", &key(&h, Who::TenantA)]);
    let primaries = t.positions_of("tenant_envelope");
    let a = *primaries
        .iter()
        .find(|p| {
            o.stdout
                .contains(&format!("The key opens the envelope at file {p} "))
        })
        .unwrap_or_else(|| panic!("tenant A's key opens no envelope: {}", o.all()));
    let backup = t
        .entries
        .iter()
        .find(|e| {
            e.type_label == "tenant_envelope_backup"
                && e.sha256_encrypted == t.entries[a].sha256_encrypted
        })
        .unwrap()
        .position as usize;
    let other = *primaries.iter().find(|p| **p != a).unwrap();
    (a, backup, other)
}

fn restore_a(h: &Heir, dest: &std::path::Path) -> heir::Out {
    h.run(&[
        "--restore",
        "--key",
        &key(h, Who::TenantA),
        "--to",
        dstr(dest),
    ])
}

fn assert_unit_restored(t: &Tape, name: &str, dest: &std::path::Path, all: &str) {
    let got = heir::tree(dest);
    for (path, bytes) in unit_files(t, name) {
        assert_eq!(got.get(path), Some(bytes), "{path}: {all}");
    }
    assert!(!dest.join(".tapectl-damaged-files.txt").exists(), "{all}");
}

/// DI-4 (a), `volume-format-v3.md` 7.2 steps 3 and 4 as ADR-0012 item 313
/// amends them: one changed byte in File 3's line for an envelope (its hash,
/// its size or its type) makes the tape's two ends disagree, and costs a try.
/// The envelope is intact, the seal marker's own copy of the map says so, and
/// `age` authenticates what it opens: the tenant's envelope opens and the unit
/// restores whole, as it does when File 3 is lost altogether.
#[test]
fn one_changed_line_of_an_envelope_in_file_3_costs_a_try_and_not_the_envelope() {
    let t = two();
    let (a, _, _) = tenant_envelopes(t);
    let cases: [(&str, &str, Edit); 3] = [
        ("hash", "sha256_encrypted", another_hash),
        ("size", "size_bytes", one_more),
        ("type", "type", |_| "\"tenant_envelopf\"".to_string()),
    ];
    for (name, line, edit) in cases {
        let h = Heir::of(t);
        h.replace(3, &front_index_with(t, a, line, edit));
        let ka = key(&h, Who::TenantA);
        let o = h.run(&["--find-envelope", "--key", &ka]);
        let all = o.all();
        assert_eq!(o.code, 0, "{name}: {all}");
        assert!(
            o.stdout
                .contains(&format!("The key opens the envelope at file {a} ")),
            "{name}: {all}"
        );
        assert!(
            o.stderr.contains("the two ends of the tape disagree") && !all.contains("DAMAGED"),
            "{name}: {all}"
        );
        let o = h.run(&["--list", "--key", &ka]);
        assert_eq!(o.code, 0, "{name}: {}", o.all());
        let dest = h.dest("out");
        let o = restore_a(&h, &dest);
        let all = o.all();
        assert_eq!(o.code, 0, "{name}: {all}");
        assert!(o.stdout.contains("RESTORE COMPLETE"), "{name}: {all}");
        assert_unit_restored(t, heir::UNIT_A, &dest, &all);
    }
}

/// DI-4 (a): a backup is paired through either map. Here the primary IS
/// damaged and File 3's hash line for it is changed too, so File 3 pairs it
/// with no backup; the seal marker's copy does, and the backup opens. With the
/// backup damaged as well, the envelope matches neither map and is DAMAGED,
/// as before.
#[test]
fn a_damaged_envelope_whose_line_is_changed_too_is_replaced_by_its_backup_through_the_seals_copy() {
    let t = two();
    let (a, backup, _) = tenant_envelopes(t);
    let h = Heir::of(t);
    h.replace(3, &front_index_with(t, a, "sha256_encrypted", another_hash));
    h.flip(a, 200);
    let dest = h.dest("out");
    let o = restore_a(&h, &dest);
    let all = o.all();
    assert_eq!(o.code, 0, "{all}");
    assert!(
        o.stderr.contains(&format!(
            "reading the backup copy of the envelope at file {a}: file {backup}."
        )),
        "{all}"
    );
    assert_unit_restored(t, heir::UNIT_A, &dest, &all);

    h.flip(backup, 200);
    let o = h.run(&["--find-envelope", "--key", &key(&h, Who::TenantA)]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stderr.contains("envelope(s) on this tape are DAMAGED"),
        "{}",
        o.stderr
    );
}

/// What bash and its builtins say of a number they cannot read: never the
/// script's words.
const BASH_NUMBER_ERRORS: [&str; 4] = [
    "value too great for base",
    "invalid octal number",
    "invalid number",
    "syntax error",
];

fn assert_no_bash_error(all: &str, name: &str) {
    for words in BASH_NUMBER_ERRORS {
        assert!(!all.contains(words), "{name}: {words}: {all}");
    }
}

/// A number with its first digit made a zero.
fn leading_zero(v: &str) -> String {
    format!("0{}", &v[1..])
}

/// A number with its second character made a letter.
fn a_letter(v: &str) -> String {
    format!("{}x{}", &v[..1], &v[2..])
}

/// DI-4 (c): a size in File 3 with a leading zero or a letter is a damaged
/// line of the map. It never reaches bash's arithmetic unread as base 10 and
/// never reaches `head -c` as it stands, so the run goes on. Here the damaged
/// line is the OTHER tenant's envelope: tenant A's restore is whole.
#[test]
fn a_size_with_a_leading_zero_or_a_letter_in_another_tenants_line_does_not_end_the_run() {
    let t = two();
    let (_, _, b) = tenant_envelopes(t);
    let cases: [(&str, Edit); 2] = [("a leading zero", leading_zero), ("a letter", a_letter)];
    for (name, edit) in cases {
        let h = Heir::of(t);
        h.replace(3, &front_index_with(t, b, "size_bytes", edit));
        let dest = h.dest("out");
        let o = restore_a(&h, &dest);
        let all = o.all();
        assert_eq!(o.code, 0, "{name}: {all}");
        assert_no_bash_error(&all, name);
        assert!(o.stdout.contains("RESTORE COMPLETE"), "{name}: {all}");
        assert_unit_restored(t, heir::UNIT_A, &dest, &all);
        if name == "a letter" {
            assert!(
                o.stderr.contains(&format!(
                    "the line the front index has for envelope file {b} gives no size that is a number"
                )),
                "{all}"
            );
        }
    }
}

/// DI-4 (c): `--verify` names a slice whose size line holds a letter as a
/// damaged line of the map, a FAIL of that line alone: the files after it are
/// still judged, where the tape used to be called unreadable from there on.
#[test]
fn verify_fails_the_one_line_whose_size_is_not_a_number_and_judges_the_rest() {
    let t = two();
    let slices = t.slice_positions();
    let p = slices[1];
    let last = *slices.last().unwrap();
    assert!(last > p);
    let h = Heir::of(t);
    h.replace(3, &front_index_with(t, p, "size_bytes", a_letter));
    let o = h.run(&["--verify"]);
    let all = o.all();
    assert_eq!(o.code, 1, "{all}");
    assert_no_bash_error(&all, "--verify");
    let row = o
        .stdout
        .lines()
        .find(|l| l.starts_with(&format!("FAIL  {p:4}  data_slice")))
        .unwrap_or_else(|| panic!("no FAIL row for file {p}: {all}"));
    assert!(row.contains("gives no size that is a number"), "{all}");
    assert!(
        o.stdout.contains(&format!("PASS  {last:4}  data_slice"))
            && !o.stdout.contains("nothing after file"),
        "the later files are judged: {all}"
    );
}

/// DI-4 (c): a position is a plain decimal number. One with a leading zero
/// (which bash's arithmetic and printf read as octal, or refuse) makes File 3
/// inconsistent, and the map is the seal marker's copy. File 0's two
/// pointers are read as base 10 too.
#[test]
fn a_position_or_a_pointer_with_a_leading_zero_is_never_read_as_octal() {
    let t = two();
    let h = Heir::of(t);
    h.replace(3, &front_index_with(t, 8, "position", |v| format!("0{v}")));
    let o = h.run(&["--info"]);
    let all = o.all();
    assert_eq!(o.code, 0, "{all}");
    assert_no_bash_error(&all, "position = 08");
    assert!(
        o.stderr.contains("the front index is not consistent")
            && o.stderr
                .contains("falling back to the seal marker's own copy of it"),
        "{all}"
    );

    // File 0's front_index = 08: file 8 is no front index, and the map is the
    // seal marker's copy.
    let h = Heir::of(t);
    let file0 = text_of(t, 0);
    assert!(file0.contains("\nfront_index = 3\n"), "{file0}");
    h.replace(
        0,
        file0
            .replace("\nfront_index = 3\n", "\nfront_index = 08\n")
            .as_bytes(),
    );
    let o = h.run(&["--info"]);
    let all = o.all();
    assert_eq!(o.code, 0, "{all}");
    assert_no_bash_error(&all, "front_index = 08");
    assert!(
        o.stderr.contains("the front index (file 8) is unreadable"),
        "{all}"
    );

    // File 0's seal_marker with a leading zero, where mt cannot space to end
    // of data and the pointer is used: the seal is found at that file.
    let h = Heir::of(t);
    let last = t.files.len() - 1;
    let pointer = format!("\nseal_marker = {last}\n");
    assert!(file0.contains(&pointer), "{file0}");
    h.replace(
        0,
        file0
            .replace(&pointer, &format!("\nseal_marker = 0{last}\n"))
            .as_bytes(),
    );
    let o = h.run_with(&["--info"], &[("FAKE_MT_NO_EOD", "1")]);
    let all = o.all();
    assert_eq!(o.code, 0, "{all}");
    assert_no_bash_error(&all, "seal_marker with a leading zero");
    assert!(
        o.stdout
            .contains(&format!("Seal marker: present (file {last})")),
        "{all}"
    );
}

const SEAL_VOUCHES: &str =
    "the seal marker at the end of this tape states layout_version = 3 and requires no feature";

/// DI-9 (ADR-0012 item 313: the refusal stays, its words change). A File 0
/// whose `requires` names a feature this script does not know is refused, by
/// `volume-format-v3.md` 1.2 rule 1. When the seal marker reads and states
/// Layout 3 and no feature, the refusal says so (File 0 may be the damaged
/// one), points to File 1's steps by hand, and no longer names the RESTORE.sh
/// in this tape's File 2, the script already running, as the way on. When the
/// seal marker states the feature too, or does not read, the old way on is
/// kept: the tape may well be a later one.
#[test]
fn a_refusal_on_file_0s_requires_says_what_the_seal_marker_states() {
    let t = two();
    let last = t.files.len() - 1;
    let file0 = text_of(t, 0).replace("requires = []", "requires = [\"future-thing\"]");
    let old_way = "the RESTORE.sh in this tape's own File 2";

    let h = Heir::of(t);
    h.replace(0, file0.as_bytes());
    let o = h.run(&["--info"]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stderr
            .contains("File 0 requires feature(s) this RESTORE.sh does not know: future-thing.")
            && o.stderr.contains(SEAL_VOUCHES)
            && o.stderr.contains("File 0 may be damaged")
            && o.stderr.contains("by the steps of File 1")
            && o.stderr.contains("fsf 1")
            && !o.stderr.contains(old_way),
        "{}",
        o.stderr
    );
    assert!(o.stdout.is_empty(), "the refusal stays: {}", o.stdout);
    // File 0, then the seal marker, and nothing else of the tape.
    assert_eq!(h.dd_reads(), vec![0, last]);

    let both = Heir::of(t);
    both.replace(0, file0.as_bytes());
    both.replace(
        last,
        text_of(t, last)
            .replace("requires = []", "requires = [\"future-thing\"]")
            .as_bytes(),
    );
    let unread = Heir::of(t);
    unread.replace(0, file0.as_bytes());
    fail_read(&unread, last);
    for (name, h) in [
        ("the seal requires it too", &both),
        ("no seal reads", &unread),
    ] {
        let o = h.run(&["--info"]);
        assert_eq!(o.code, 1, "{name}: {}", o.all());
        assert!(
            o.stderr.contains("File 0 requires feature(s)")
                && o.stderr.contains(old_way)
                && !o.stderr.contains(SEAL_VOUCHES)
                && !o.stderr.contains("File 0 may be damaged"),
            "{name}: {}",
            o.stderr
        );
    }
}

/// DI-9: the same for a File 0 that states another magic or layout. With a
/// seal marker of another layout, or none, the tape is taken for the other
/// layout it says it is, and the way on is the one a reader of that layout
/// gives.
#[test]
fn a_refusal_on_file_0s_layout_keeps_its_old_way_on_when_the_seal_marker_does_not_vouch() {
    let t = two();
    let last = t.files.len() - 1;
    let file0 = text_of(t, 0)
        .replace("tapectl-volume-v3", "tapectl-volume-v2")
        .replace("layout_version = 3", "layout_version = 2");
    let other = Heir::of(t);
    other.replace(0, file0.as_bytes());
    other.replace(
        last,
        text_of(t, last)
            .replace("layout_version = 3", "layout_version = 2")
            .as_bytes(),
    );
    let unread = Heir::of(t);
    unread.replace(0, file0.as_bytes());
    fail_read(&unread, last);
    for (name, h) in [("a seal of Layout 2", &other), ("no seal reads", &unread)] {
        let o = h.run(&["--info"]);
        assert_eq!(o.code, 1, "{name}: {}", o.all());
        assert!(
            o.stderr.contains("a tapectl volume of Layout 2")
                && o.stderr.contains("Layout 3 only")
                && o.stderr.contains("RESTORE.sh in its own File 2")
                && o.stderr.contains("RESTORE-OLD.sh")
                && !o.stderr.contains(SEAL_VOUCHES)
                && !o.stderr.contains("File 0 may be damaged"),
            "{name}: {}",
            o.stderr
        );
    }
}

/// S-5: `--info`, `--verify` and `--find-envelope` print values of the
/// plain-text files (File 3's sizes, kinds and hashes, the seal marker's
/// `front_index_sha256`). None reaches the terminal as it stands: an escape
/// sequence in a damaged or foreign tape's map is shown as `?`.
#[test]
fn no_escape_sequence_of_a_plain_text_file_reaches_the_terminal() {
    let t = two();
    let (a, _, _) = tenant_envelopes(t);
    let slices = t.slice_positions();
    let last = t.files.len() - 1;
    let h = Heir::of(t);
    let mut map = text_of(t, 3);
    map = entry_line_edited(&map, slices[0], "size_bytes", |v| {
        format!("{v}\u{1b}]0;OWNED\u{7}\u{1b}[31m")
    });
    map = entry_line_edited(&map, slices[1], "type", |_| {
        "\"data_slice\u{1b}[2J\"".to_string()
    });
    map = entry_line_edited(&map, slices[2], "sha256_encrypted", |_| {
        "\"ab\u{1b}[2Jcd\"".to_string()
    });
    map = entry_line_edited(&map, a, "type", |_| {
        "\"tenant_envelope\u{1b}[2J\"".to_string()
    });
    h.replace(3, map.as_bytes());
    let seal = text_of(t, last);
    let bound = seal
        .lines()
        .find(|l| l.starts_with("front_index_sha256 = "))
        .unwrap();
    h.replace(
        last,
        seal.replace(
            bound,
            &bound.replace("sha256 = \"", "sha256 = \"\u{1b}[2J\u{7}"),
        )
        .as_bytes(),
    );
    let ka = key(&h, Who::TenantA);
    for args in [
        vec!["--info"],
        vec!["--verify"],
        vec!["--find-envelope", "--key", &ka],
    ] {
        let o = h.run(&args);
        let all = o.all();
        assert!(
            !all.contains('\u{1b}') && !all.contains('\u{7}'),
            "{args:?}: {all:?}"
        );
        // The run reached what it prints of the map.
        match args[0] {
            "--info" => assert!(all.contains("OWNED"), "{all:?}"),
            "--verify" => assert!(all.contains("the front index says ab??2Jcd"), "{all:?}"),
            _ => assert!(
                all.contains(&format!("The key opens the envelope at file {a} (")),
                "{all:?}"
            ),
        }
    }
}

// ---- ADR-0012 item 313: what an unreadable File 0 now reaches ----

/// `volume-format-v3.md` 1.2 rule 2: where File 0 states no magic or no
/// layout_version (it did not read, or is damaged), the seal marker's own
/// `layout_version` decides. One that is not 3 is another layout, refused by
/// name as File 0's would be. And by rule 1 (F9 of the read of #611's fixes),
/// a seal marker that keeps its parse rules and states another layout is
/// refused where File 0 states Layout 3 too, whether File 3 reads or not;
/// the words say the two ends disagree. A seal that breaks its parse rules
/// is no seal and states nothing: the tape reads as unsealed.
#[test]
fn the_seal_markers_layout_version_decides_when_file_0_states_none() {
    let t = two();
    let last = t.files.len() - 1;
    let seal = text_of(t, last);
    assert!(seal.contains("\nlayout_version = 3\n"), "{seal}");
    let seal4 = seal.replacen("\nlayout_version = 3\n", "\nlayout_version = 4\n", 1);
    let unread = Heir::of(t);
    unread.replace(last, seal4.as_bytes());
    fail_read(&unread, 0);
    let garbage = Heir::of(t);
    garbage.replace(last, seal4.as_bytes());
    garbage.replace(0, &heir::noise(4096, 7));
    for (name, h) in [
        ("File 0 does not read", &unread),
        ("File 0 is garbage", &garbage),
    ] {
        let dest = h.dest("out");
        for args in [
            vec!["--info"],
            vec![
                "--restore",
                "--key",
                &key(h, Who::TenantA),
                "--to",
                dstr(&dest),
            ],
        ] {
            let o = h.run(&args);
            assert_eq!(o.code, 1, "{name} {args:?}: {}", o.all());
            assert!(
                o.stderr.contains(
                    "FATAL: the seal marker at the end of this tape states layout_version 4"
                ) && o.stderr.contains("a tapectl volume of Layout 4")
                    && o.stderr.contains("This RESTORE.sh reads Layout 3 only")
                    && o.stderr.contains("the RESTORE.sh in its own File 2"),
                "{name} {args:?}: {}",
                o.stderr
            );
        }
        assert!(heir::tree(&dest).is_empty(), "{name}: nothing was restored");
    }

    // File 0 states Layout 3 (this arm pinned "the run goes on" before F9).
    let sound = Heir::of(t);
    sound.replace(last, seal4.as_bytes());
    let no_file_3 = Heir::of(t);
    no_file_3.replace(last, seal4.as_bytes());
    fail_read(&no_file_3, 3);
    for (name, h) in [
        ("File 3 reads", &sound),
        ("File 3 does not read", &no_file_3),
    ] {
        let dest = h.dest("out");
        for args in [
            vec!["--info"],
            vec![
                "--restore",
                "--key",
                &key(h, Who::TenantA),
                "--to",
                dstr(&dest),
            ],
        ] {
            let o = h.run(&args);
            assert_eq!(o.code, 1, "{name} {args:?}: {}", o.all());
            assert!(o.stdout.is_empty(), "{name} {args:?}: {}", o.all());
            assert!(
                o.stderr.contains(
                    "FATAL: the seal marker at the end of this tape states layout_version 4, and File 0 states Layout 3: the two ends of the tape disagree."
                ) && o.stderr.contains("If it is a tapectl volume of Layout 4, read it with the tapectl that wrote it.")
                    && o.stderr.contains("by the steps of File 1")
                    && !o.stderr.contains("RESTORE.sh in its own File 2"),
                "{name} {args:?}: {}",
                o.stderr
            );
        }
        assert!(heir::tree(&dest).is_empty(), "{name}: nothing was restored");
    }

    // A seal that breaks rule S1 is no seal: stating 4, it refuses nothing,
    // and the tape reads as it does with such a seal stating 3.
    let fc = format!("\nfile_count = {}\n", t.files.len());
    let broken = |s: &str| -> Vec<u8> {
        assert!(s.contains(&fc), "{s}");
        s.replacen(&fc, &format!("\nfile_count = {}\n", t.files.len() + 1), 1)
            .into_bytes()
    };
    let want = Heir::of(t);
    want.replace(last, &broken(&seal));
    let want = want.run(&["--info"]);
    let invalid = Heir::of(t);
    invalid.replace(last, &broken(&seal4));
    let o = invalid.run(&["--info"]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(
        o.stderr.contains("the tape is read as unsealed") && !o.all().contains("Layout 4"),
        "{}",
        o.all()
    );
    assert_eq!(o.stdout, want.stdout, "{}", o.all());
}

/// The two units, ended early inside the second slice of the plan: the tape
/// is sealed short.
fn short_tape() -> &'static Tape {
    static T: OnceLock<Tape> = OnceLock::new();
    T.get_or_init(|| {
        let mut shape = Shape::two();
        shape.warning_in_slice = Some(1);
        Tape::build(&shape)
    })
}

/// A short seal is classed short when File 0 does not give the plan's count.
/// The front index lists every file the write planned, so its own count is
/// the plan's where File 0's is absent (File 0 unread, or garbage). `--info`
/// and `--verify` then say of a short-sealed tape what they say with File 0
/// readable, apart from File 0's own lines and row: never "the tape is
/// complete, every planned file is on it".
#[test]
fn a_short_seal_is_classed_short_when_file_0_does_not_give_the_plan() {
    let t = short_tape();
    let good = Heir::of(t);
    let unread = Heir::of(t);
    fail_read(&unread, 0);
    let garbage = Heir::of(t);
    garbage.replace(0, &heir::noise(4096, 7));

    // --info: everything after File 0's own two lines.
    let from_map = |s: &str| -> String { s.lines().skip(2).map(|l| format!("{l}\n")).collect() };
    let want = good.run(&["--info"]);
    assert_eq!(want.code, 0, "{}", want.all());
    assert!(
        want.stdout.contains("this tape ended EARLY") && want.stdout.contains("NOT ON TAPE"),
        "{}",
        want.stdout
    );
    // --verify: every row but file 0's, and not the last line.
    let rows = |s: &str| -> String {
        let lines: Vec<&str> = s.lines().collect();
        lines[..lines.len() - 1]
            .iter()
            .filter(|l| l.split_whitespace().nth(1) != Some("0"))
            .map(|l| format!("{l}\n"))
            .collect()
    };
    let want_v = good.run(&["--verify"]);
    assert_eq!(want_v.code, 0, "{}", want_v.all());
    assert!(
        want_v.stdout.contains("NOT ON TAPE") && want_v.stdout.contains("short seal"),
        "{}",
        want_v.stdout
    );
    for (name, h) in [
        ("File 0 does not read", &unread),
        ("File 0 is garbage", &garbage),
    ] {
        let o = h.run(&["--info"]);
        assert_eq!(o.code, 0, "{name}: {}", o.all());
        assert_eq!(from_map(&o.stdout), from_map(&want.stdout), "{name}");
        assert!(!o.stdout.contains("the tape is complete"), "{name}");
        let o = h.run(&["--verify"]);
        assert_eq!(o.code, 1, "{name}: {}", o.all());
        assert_eq!(rows(&o.stdout), rows(&want_v.stdout), "{name}");
        assert!(
            o.stdout
                .lines()
                .last()
                .unwrap()
                .starts_with("VERIFY FAILED: 1 file(s) "),
            "{name}: {}",
            o.stdout
        );
    }
}

/// Rewrite the text of tape file `n` of the shelved cartridge `cart`, keeping
/// the file's padded length.
fn edit_cart_text(h: &Heir, cart: &str, n: usize, edit: impl Fn(&str) -> String) {
    let p = h.cart_file(cart, n);
    let raw = std::fs::read(&p).unwrap();
    let text = String::from_utf8_lossy(&raw)
        .trim_end_matches('\0')
        .to_string();
    let mut new = edit(&text).into_bytes();
    assert!(new.len() <= raw.len());
    new.resize(raw.len(), 0);
    std::fs::write(&p, new).unwrap();
}

/// The whole unit of the span, restored from its first tape with the
/// cartridges of `offers` loaded one at each prompt.
fn restore_span(h: &Heir, dest: &std::path::Path, offers: &[&str]) -> heir::Out {
    let ka = key(h, Who::TenantA);
    h.run_prompted(
        &["--restore", "--key", &ka, "--all", "--to", dstr(dest)],
        |k, _| {
            assert!(k <= offers.len(), "asked {k} times");
            h.load(offers[k - 1]);
            Some(String::new())
        },
    )
}

/// File 1's rule for taking a cartridge at the prompt: its front index lists
/// the next slice's sha256 at a position its own seal marker covers, and "a
/// label or an index never stands in for the hash". File 0 is no part of it.
/// A second cartridge whose File 0 gives a read error, or reads and states
/// nothing, is warned about and taken by that rule, and the unit completes.
#[test]
fn a_cartridge_whose_file_0_gives_nothing_is_taken_by_its_front_index_and_seal() {
    let t = span2();
    let second = t[1].label.as_str();
    let base = Heir::of_series(&[&t[0], &t[1]]);
    let want_dest = base.dest("out");
    let o = restore_span(&base, &want_dest, &[second]);
    assert_eq!(o.code, 0, "{}", o.all());
    let want_tree = heir::tree(&want_dest);
    assert!(!want_tree.is_empty());

    let unread = Heir::of_series(&[&t[0], &t[1]]);
    unread.fail_file(second, 0);
    let garbage = Heir::of_series(&[&t[0], &t[1]]);
    let mut noise = heir::noise(4096, 7);
    noise.resize(heir::BLOCK, 0);
    std::fs::write(garbage.cart_file(second, 0), noise).unwrap();
    for (name, h, warning) in [
        (
            "a read error",
            &unread,
            "WARNING: this cartridge: its first file cannot be read (",
        ),
        (
            "garbage",
            &garbage,
            "WARNING: this cartridge: its first file states no magic, layout_version, label or uuid",
        ),
    ] {
        let dest = h.dest("out");
        let o = restore_span(h, &dest, &[second]);
        let all = o.all();
        assert_eq!(o.code, 0, "{name}: {all}");
        assert!(o.stdout.contains("RESTORE COMPLETE"), "{name}: {all}");
        assert!(
            all.contains(warning)
                && all.contains("Its front index and its seal marker decide whether it is taken")
                && all.contains("its front index lists each one's sha256")
                && !all.contains("Nothing is taken from it"),
            "{name}: {all}"
        );
        assert_eq!(all.matches(heir::PROMPT).count(), 1, "{name}: {all}");
        assert_eq!(heir::tree(&dest), want_tree, "{name}: {all}");
        assert!(damaged_list(&dest).is_empty(), "{name}: {all}");
    }
}

/// The fences hold through the seal marker when the offered cartridge's File 0
/// does not read: one whose seal marker names a feature this script does not
/// know, or states another layout, is refused by name, the prompt asks again,
/// and the sound cartridge is then taken.
#[test]
fn a_cartridge_whose_file_0_does_not_read_is_still_fenced_by_its_seal_marker() {
    let t = span2();
    let second = t[1].label.as_str();
    let last = t[1].files.len() - 1;
    let base = Heir::of_series(&[&t[0], &t[1]]);
    let want_dest = base.dest("out");
    let o = restore_span(&base, &want_dest, &[second]);
    assert_eq!(o.code, 0, "{}", o.all());

    let h = Heir::of_series(&[&t[0], &t[1]]);
    for (cart, from, to) in [
        ("FEATURE", "requires = []", "requires = [\"future-thing\"]"),
        (
            "LAYOUT4",
            "\nlayout_version = 3\n",
            "\nlayout_version = 4\n",
        ),
    ] {
        h.copy_cart(second, cart);
        h.fail_file(cart, 0);
        edit_cart_text(&h, cart, last, |s| {
            assert!(s.contains(from), "{cart}: {s}");
            s.replacen(from, to, 1)
        });
    }
    let dest = h.dest("out");
    let o = restore_span(&h, &dest, &["FEATURE", "LAYOUT4", second]);
    let all = o.all();
    assert_eq!(o.code, 0, "{all}");
    assert_eq!(all.matches(heir::PROMPT).count(), 3, "{all}");
    assert!(
        all.contains(
            "NOTE: the seal marker requires feature(s) this RESTORE.sh does not know: future-thing."
        ) && all.contains("NOTE: the seal marker at the end of this tape states layout_version 4")
            && !all.contains("FATAL"),
        "{all}"
    );
    assert_eq!(heir::tree(&dest), heir::tree(&want_dest), "{all}");
}

/// The other half: the tape the run STARTED on has a File 0 that does not
/// read, and `done` is answered at the prompt for a slice it does not hold.
/// It is recognised as the cartridge in the drive (it gave no File 0 then and
/// gives none now), and what it holds is restored, as on a start tape whose
/// File 0 reads.
#[test]
fn done_on_a_start_tape_whose_file_0_does_not_read_still_restores_what_it_holds() {
    let t = span2();
    let base = Heir::of_series(&[&t[1], &t[0]]);
    let want_dest = base.dest("out");
    let ka = key(&base, Who::TenantA);
    base.run(&["--restore", "--key", &ka, "--all", "--to", dstr(&want_dest)]);
    let want_tree = heir::tree(&want_dest);
    assert!(!want_tree.is_empty());
    let h = Heir::of_series(&[&t[1], &t[0]]);
    fail_read(&h, 0);
    let dest = h.dest("out");
    let o = h.run(&["--restore", "--key", &ka, "--all", "--to", dstr(&dest)]);
    let all = o.all();
    assert!(all.contains(FILE0_WARNING), "{all}");
    assert!(!all.contains("NOT IN THE DRIVE"), "{all}");
    assert_eq!(heir::tree(&dest), want_tree, "{all}");
    assert_eq!(damaged_list(&dest), damaged_list(&want_dest), "{all}");
}

/// `--verify` on a tape with no seal stops at the first file that does not
/// read: that may be the end of the data. A File 0 that does not read is not
/// the end, since the front index after it was read: every later file is
/// judged, and the run ends VERIFY FAILED with file 0's row. The stop at the
/// true end stays.
#[test]
fn verify_on_an_unsealed_tape_does_not_stop_at_a_file_0_that_does_not_read() {
    let t = two();
    let last = t.files.len() - 1;
    let h = Heir::of(t);
    h.cut_after(last);
    fail_read(&h, 0);
    let o = h.run(&["--verify"]);
    let all = o.all();
    assert_eq!(o.code, 1, "{all}");
    let fail: Vec<&str> = o.stdout.lines().filter(|l| l.starts_with("FAIL")).collect();
    assert_eq!(fail.len(), 1, "{all}");
    assert!(
        fail[0].starts_with("FAIL     0  ") && fail[0].contains("cannot be read from the tape"),
        "{all}"
    );
    assert_eq!(
        o.stdout.matches("PASS ").count(),
        last - 1,
        "every file after file 0 and before the missing seal is judged: {all}"
    );
    assert!(!o.stdout.contains("nothing after file"), "{all}");
    assert!(o.stdout.contains("there is no seal marker"), "{all}");
    assert!(
        o.stdout.lines().last().unwrap().starts_with(
            "VERIFY FAILED: 1 file(s) differ from the front index or cannot be read; "
        ),
        "{all}"
    );

    // A slice that does not read on a tape with no seal still ends the walk.
    let p = t.slice_positions()[1];
    let h = Heir::of(t);
    h.cut_after(last);
    fail_read(&h, p);
    let o = h.run(&["--verify"]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stdout
            .contains(&format!("nothing after file {p} was tried")),
        "{}",
        o.stdout
    );
}

/// `--verify`'s row for a file whose hash differs names the map it was judged
/// by: "the front index says" when File 3 read (the words are unchanged), and
/// "the seal marker's copy of the front index says" when the map is that copy.
#[test]
fn verifys_mismatch_row_names_the_map_it_judged_by() {
    let t = two();
    let p = t.slice_positions()[1];
    let row_of = |h: &Heir| -> String {
        let o = h.run(&["--verify"]);
        assert_eq!(o.code, 1, "{}", o.all());
        o.stdout
            .lines()
            .find(|l| l.starts_with(&format!("FAIL  {p:4}  data_slice")))
            .unwrap_or_else(|| panic!("no FAIL row for file {p}: {}", o.all()))
            .to_string()
    };
    let h = Heir::of(t);
    h.flip(p, 100);
    let row = row_of(&h);
    assert!(row.contains(", the front index says "), "{row}");

    let h = Heir::of(t);
    fail_read(&h, 3);
    h.flip(p, 100);
    let row = row_of(&h);
    assert!(
        row.contains(", the seal marker's copy of the front index says ")
            && !row.contains(", the front index says "),
        "{row}"
    );
}

/// An envelope is called damaged only once no map is left to try it by. Where
/// the two ends of the tape disagree and the envelope differs from File 3's
/// line, the NOTE says that, and that the seal marker's copy of the map is
/// tried; the word "damaged" as a verdict comes only when it matches neither.
#[test]
fn an_envelope_is_not_called_damaged_before_the_seal_markers_copy_was_tried() {
    let t = two();
    let (a, backup, _) = tenant_envelopes(t);
    let ka_of = |h: &Heir| key(h, Who::TenantA);

    // File 3's line is the damaged one: the envelope opens, and is never
    // called damaged.
    let h = Heir::of(t);
    h.replace(3, &front_index_with(t, a, "sha256_encrypted", another_hash));
    let o = h.run(&["--find-envelope", "--key", &ka_of(&h)]);
    let all = o.all();
    assert_eq!(o.code, 0, "{all}");
    assert!(
        all.contains(&format!(
            "NOTE: envelope file {a} differs from the hash File 3's line gives it; the seal marker's copy of the map is tried before it is called damaged."
        )) && !all.contains(": damaged.")
            && !all.contains("DAMAGED"),
        "{all}"
    );

    // Both copies ARE damaged: each is called damaged once it matched neither
    // map, by the last map tried.
    h.flip(a, 200);
    h.flip(backup, 200);
    let o = h.run(&["--find-envelope", "--key", &ka_of(&h)]);
    let all = o.all();
    assert_eq!(o.code, 1, "{all}");
    for pos in [a, backup] {
        assert!(
            all.contains(&format!(
                "NOTE: envelope file {pos} differs from the seal marker's copy of the front index's hash: damaged."
            )),
            "file {pos}: {all}"
        );
    }
    assert!(
        all.contains("envelope(s) on this tape are DAMAGED"),
        "{all}"
    );

    // A tape whose two ends agree: the words are as they were.
    let h = Heir::of(t);
    h.flip(a, 200);
    let o = h.run(&["--find-envelope", "--key", &ka_of(&h)]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(
        o.stderr.contains(&format!(
            "NOTE: envelope file {a} differs from the front index's hash: damaged."
        )),
        "{}",
        o.stderr
    );
}

/// A cartridge is never taken for the one in the drive because its File 0
/// fails too. The run starts on a tape whose File 0 does not read, so that
/// tape is known by its front index, not by a File 0 that gave nothing. A
/// cartridge NOTHING of which reads, offered at the prompt, is named as
/// unreadable and the prompt asks again; with `done` answered while it is in
/// the drive, nothing is read from it and the start tape's slices are said to
/// be on a tape that is not in the drive.
#[test]
fn a_cartridge_nothing_of_which_reads_is_not_taken_for_a_start_tape_with_no_file_0() {
    let t = span2();
    let (first, start) = (t[0].label.as_str(), t[1].label.as_str());
    let base = Heir::of_series(&[&t[0], &t[1]]);
    let want_dest = base.dest("out");
    let o = restore_span(&base, &want_dest, &[start]);
    assert_eq!(o.code, 0, "{}", o.all());
    let dead_heir = || -> Heir {
        let h = Heir::of_series(&[&t[1], &t[0]]);
        fail_read(&h, 0);
        h.copy_cart(first, "DEAD");
        for n in [0, 3, t[0].files.len() - 1] {
            h.fail_file("DEAD", n);
        }
        h
    };

    // ENTER with the dead cartridge loaded, then the two sound ones in turn.
    let h = dead_heir();
    let dest = h.dest("out");
    let o = restore_span(&h, &dest, &["DEAD", first, start]);
    let all = o.all();
    assert_eq!(o.code, 0, "{all}");
    assert_eq!(all.matches(heir::PROMPT).count(), 3, "{all}");
    assert!(
        all.contains("NOTE: this cartridge is unreadable: its first file cannot be read (")
            && !all.contains("is the same cartridge"),
        "{all}"
    );
    assert_eq!(heir::tree(&dest), heir::tree(&want_dest), "{all}");

    // `done` with the dead cartridge in the drive.
    let h = dead_heir();
    let dest = h.dest("out");
    let ka = key(&h, Who::TenantA);
    let o = h.run_prompted(
        &["--restore", "--key", &ka, "--all", "--to", dstr(&dest)],
        |k, _| {
            assert_eq!(k, 1, "asked {k} times");
            h.load("DEAD");
            Some("done".into())
        },
    );
    let all = o.all();
    assert_eq!(o.code, 1, "{all}");
    assert!(all.contains("NOT IN THE DRIVE"), "{all}");
    for wrong in ["could not be read at file", "differs from the front index"] {
        assert!(!all.contains(wrong), "{wrong}: {all}");
    }
}

// ---- ADR-0012 item 313: what the independent read of the fixes found ----

const MIB: usize = 1024 * 1024;

/// The bytes the work directories under `dest` hold in all, and the
/// cartridge directories (`cart.N`) in them.
fn work_dirs(dest: &std::path::Path) -> (u64, Vec<String>) {
    fn walk(d: &std::path::Path, bytes: &mut u64, carts: &mut Vec<String>) {
        for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
            let Ok(m) = e.path().symlink_metadata() else {
                continue;
            };
            if m.is_dir() {
                let n = e.file_name().to_string_lossy().into_owned();
                if n.starts_with("cart.") {
                    carts.push(n);
                }
                walk(&e.path(), bytes, carts);
            } else {
                *bytes += m.len();
            }
        }
    }
    let (mut bytes, mut carts) = (0, Vec::new());
    for e in std::fs::read_dir(dest).unwrap().flatten() {
        if e.file_name().to_string_lossy().starts_with(".tapectl-work") {
            walk(&e.path(), &mut bytes, &mut carts);
        }
    }
    (bytes, carts)
}

/// The reads the stand-in `dd` was given a count for: (tape file, blocks).
fn counted_reads(h: &Heir) -> Vec<(usize, usize)> {
    std::fs::read_to_string(h.root.path().join("state/dd.counts"))
        .unwrap_or_default()
        .lines()
        .map(|l| {
            let (p, n) = l.split_once(' ').unwrap();
            (p.parse().unwrap(), n.parse().unwrap())
        })
        .collect()
}

/// Every read of tape file `n` stopped at 33 blocks (16 MiB and one block),
/// and there was at least one.
fn every_read_of_file_is_capped(h: &Heir, n: usize, what: &str) {
    let reads = h.ops().iter().filter(|l| **l == format!("dd {n}")).count();
    let capped: Vec<usize> = counted_reads(h)
        .into_iter()
        .filter(|(p, _)| *p == n)
        .map(|(_, c)| c)
        .collect();
    assert!(reads > 0, "{what}: file {n} was never read");
    assert_eq!(
        capped.len(),
        reads,
        "{what}: a read of file {n} had no count: {:?}",
        h.ops()
    );
    assert!(
        capped.iter().all(|c| *c == 33),
        "{what}: file {n} read with counts {capped:?}"
    );
}

/// F1 of the read of #611's fixes: File 0 and the front index are read no
/// further than 16 MiB (and one block, to know a longer one), as tapectl's
/// own reader holds them (`SMALL_FILE_CAP`): a longer file is neither. A
/// foreign cartridge offered at the prompt, whose file 3 is 64 MiB, is
/// refused with no more than that read of it; its directory in the work
/// directory is gone at the next prompt; and the sound cartridge offered
/// then is taken.
#[test]
fn a_foreign_cartridge_at_the_prompt_is_read_no_further_than_16_mib_and_leaves_nothing() {
    let t = span2();
    let second = t[1].label.as_str();
    let base = Heir::of_series(&[&t[0], &t[1]]);
    let want_dest = base.dest("out");
    let o = restore_span(&base, &want_dest, &[second]);
    assert_eq!(o.code, 0, "{}", o.all());

    let h = Heir::of_series(&[&t[0], &t[1]]);
    h.shelve(
        "FOREIGN",
        &[
            b"AMANDA: TAPESTART DATE 20261127 TAPE DailySet1-07\n".to_vec(),
            heir::text(2000, 1),
            heir::text(2000, 2),
            heir::noise(64 * MIB, 3),
            heir::text(2000, 4),
        ],
    );
    let dest = h.dest("out");
    let ka = key(&h, Who::TenantA);
    let mut at_prompt = Vec::new();
    let o = h.run_prompted(
        &["--restore", "--key", &ka, "--all", "--to", dstr(&dest)],
        |k, _| {
            assert!(k <= 2, "asked {k} times");
            at_prompt.push(work_dirs(&dest));
            h.load(["FOREIGN", second][k - 1]);
            Some(String::new())
        },
    );
    let all = o.all();
    assert_eq!(o.code, 0, "{all}");
    assert!(o.stdout.contains("RESTORE COMPLETE"), "{all}");
    assert_eq!(heir::tree(&dest), heir::tree(&want_dest), "{all}");
    assert!(
        all.contains("tape file 3 is longer than 16 MiB")
            && all.contains("NOTE: this cartridge is not a tapectl volume"),
        "{all}"
    );
    assert_eq!(at_prompt.len(), 2, "{all}");
    let ((before, _), (after, carts)) = (&at_prompt[0], &at_prompt[1]);
    assert!(
        *after <= before + 16 * MIB as u64,
        "the work directory grew from {before} to {after} bytes"
    );
    assert!(carts.is_empty(), "left behind: {carts:?}");
    every_read_of_file_is_capped(&h, 3, "the run");
}

/// F1: a start tape whose File 0 is 64 MiB of noise. File 0 is read no
/// further than 16 MiB and one block, and is said to be no tapectl File 0. On
/// a tape nothing else of which is a map, the run is refused; on one whose
/// front index and seal marker are sound, that File 0 is a damaged File 0 as
/// one that does not read is (`volume-format-v3.md` 1.2 rule 2), and the
/// tape reads.
#[test]
fn a_file_0_longer_than_16_mib_is_no_file_0_and_is_not_read_whole() {
    let t = two();
    let long = heir::noise(64 * MIB, 5);
    let foreign = Heir::with_files(
        t,
        vec![
            long.clone(),
            heir::text(2000, 1),
            heir::text(2000, 2),
            heir::text(2000, 3),
        ],
    );
    let o = foreign.run(&["--info"]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(o.stdout.is_empty(), "{}", o.stdout);
    assert!(
        o.stderr.contains(
            "WARNING: File 0 on /dev/nst0 is longer than 16 MiB, more than a tapectl File 0 can be"
        ) && o.stderr.contains("FATAL: "),
        "{}",
        o.stderr
    );
    every_read_of_file_is_capped(&foreign, 0, "a foreign tape");

    let good = Heir::of(t).run(&["--info"]);
    let h = Heir::of(t);
    h.replace(0, &long);
    let o = h.run(&["--info"]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(
        o.stderr.contains(
            "WARNING: File 0 on /dev/nst0 is longer than 16 MiB, more than a tapectl File 0 can be"
        ),
        "{}",
        o.stderr
    );
    let from_map = |s: &str| -> String { s.lines().skip(2).map(|l| format!("{l}\n")).collect() };
    assert_eq!(from_map(&o.stdout), from_map(&good.stdout), "{}", o.all());
    every_read_of_file_is_capped(&h, 0, "a sound tape");
}

/// The two edits of a seal marker's claims a Layout 3 reader refuses: a
/// feature it does not know, and another layout.
const SEAL_CLAIMS: [(&str, &str, &str); 2] = [
    ("FEATURE", "requires = []", "requires = [\"future-thing\"]"),
    (
        "LAYOUT4",
        "\nlayout_version = 3\n",
        "\nlayout_version = 4\n",
    ),
];

/// F3 of the read of #611's fixes: the seal marker's claims are the fence
/// where neither File 0 nor File 3 reads (`establish`'s fallback, the map
/// the seal's own copy). A seal marker that names a feature this script does
/// not know, or states another layout, is refused by name there too, and
/// nothing is restored.
#[test]
fn with_file_0_and_file_3_unread_the_seal_markers_claims_still_refuse() {
    let t = two();
    let last = t.files.len() - 1;
    for (name, from, to) in SEAL_CLAIMS {
        let h = Heir::of(t);
        fail_read(&h, 0);
        fail_read(&h, 3);
        let seal = text_of(t, last);
        assert!(seal.contains(from), "{name}: {seal}");
        h.replace(last, seal.replacen(from, to, 1).as_bytes());
        let dest = h.dest("out");
        for args in [
            vec!["--info"],
            vec![
                "--restore",
                "--key",
                &key(&h, Who::TenantA),
                "--to",
                dstr(&dest),
            ],
        ] {
            let o = h.run(&args);
            assert_eq!(o.code, 1, "{name} {args:?}: {}", o.all());
            assert!(o.stdout.is_empty(), "{name} {args:?}: {}", o.all());
            let refusal = if name == "FEATURE" {
                "FATAL: the seal marker requires feature(s) this RESTORE.sh does not know: future-thing."
            } else {
                "FATAL: the seal marker at the end of this tape states layout_version 4"
            };
            assert!(
                o.stderr.contains(FILE0_WARNING)
                    && o.stderr
                        .contains("falling back to the seal marker's own copy of it")
                    && o.stderr.contains(refusal),
                "{name} {args:?}: {}",
                o.stderr
            );
        }
        assert!(heir::tree(&dest).is_empty(), "{name}: nothing was restored");
    }
}

/// F3: the same fence at the prompt, for a cartridge whose File 0 and File 3
/// both fail, so that it is judged by its seal marker's copy of the map
/// alone. One whose seal marker names an unknown feature, or states another
/// layout, is refused by name, the prompt asks again, and the sound
/// cartridge is then taken.
#[test]
fn a_cartridge_judged_by_its_seal_marker_alone_is_fenced_by_its_claims() {
    let t = span2();
    let second = t[1].label.as_str();
    let last = t[1].files.len() - 1;
    let base = Heir::of_series(&[&t[0], &t[1]]);
    let want_dest = base.dest("out");
    let o = restore_span(&base, &want_dest, &[second]);
    assert_eq!(o.code, 0, "{}", o.all());

    let h = Heir::of_series(&[&t[0], &t[1]]);
    for (cart, from, to) in SEAL_CLAIMS {
        h.copy_cart(second, cart);
        h.fail_file(cart, 0);
        h.fail_file(cart, 3);
        edit_cart_text(&h, cart, last, |s| {
            assert!(s.contains(from), "{cart}: {s}");
            s.replacen(from, to, 1)
        });
    }
    let dest = h.dest("out");
    let o = restore_span(&h, &dest, &["FEATURE", "LAYOUT4", second]);
    let all = o.all();
    assert_eq!(o.code, 0, "{all}");
    assert_eq!(all.matches(heir::PROMPT).count(), 3, "{all}");
    assert_eq!(
        all.matches(
            "the front index of this cartridge is unreadable: using the seal marker's copy of it"
        )
        .count(),
        2,
        "each refused cartridge is judged by its seal marker's copy: {all}"
    );
    assert!(
        all.contains(
            "NOTE: the seal marker requires feature(s) this RESTORE.sh does not know: future-thing."
        ) && all.contains("NOTE: the seal marker at the end of this tape states layout_version 4")
            && !all.contains("FATAL"),
        "{all}"
    );
    assert_eq!(heir::tree(&dest), heir::tree(&want_dest), "{all}");
}

/// F9 at the prompt: a cartridge whose File 0 states Layout 3 and whose seal
/// marker, keeping its parse rules, states another layout is refused by
/// name, whether it is judged by its front index or, File 3 failing, by its
/// seal marker's copy alone. The prompt asks again and the sound cartridge
/// is then taken.
#[test]
fn a_cartridge_whose_seal_marker_states_another_layout_is_refused_whatever_file_0_states() {
    let t = span2();
    let second = t[1].label.as_str();
    let last = t[1].files.len() - 1;
    let base = Heir::of_series(&[&t[0], &t[1]]);
    let want_dest = base.dest("out");
    let o = restore_span(&base, &want_dest, &[second]);
    assert_eq!(o.code, 0, "{}", o.all());

    let h = Heir::of_series(&[&t[0], &t[1]]);
    for cart in ["LAYOUT4", "LAYOUT4-NO-FILE-3"] {
        h.copy_cart(second, cart);
        edit_cart_text(&h, cart, last, |s| {
            s.replacen("\nlayout_version = 3\n", "\nlayout_version = 4\n", 1)
        });
    }
    h.fail_file("LAYOUT4-NO-FILE-3", 3);
    let dest = h.dest("out");
    let o = restore_span(&h, &dest, &["LAYOUT4", "LAYOUT4-NO-FILE-3", second]);
    let all = o.all();
    assert_eq!(o.code, 0, "{all}");
    assert_eq!(all.matches(heir::PROMPT).count(), 3, "{all}");
    assert_eq!(
        all.matches("NOTE: the seal marker at the end of this tape states layout_version 4, and File 0 states Layout 3: the two ends of the tape disagree.")
            .count(),
        2,
        "{all}"
    );
    assert!(!all.contains("FATAL"), "{all}");
    assert_eq!(heir::tree(&dest), heir::tree(&want_dest), "{all}");
}

/// The first line of `--info`.
fn tape_line(o: &heir::Out) -> String {
    o.stdout.lines().next().unwrap_or_default().to_string()
}

/// F6 of the read of #611's fixes: File 1 tells the heir to read which tape
/// is in the drive off `--info`'s first line ("Tape: LABEL"). Where File 0
/// gives no label, the label is the front index's (`[index] volume`), else
/// a valid seal marker's (`[seal] volume`), and the line says File 0 gave
/// none. No file but File 0 states the uuid, and the line says that too. A
/// sound File 0's line is as it was.
#[test]
fn with_file_0_unread_the_tape_line_takes_the_label_from_the_front_index_or_the_seal() {
    let t = two();
    let last = t.files.len() - 1;
    let label = &t.label;
    let sound = Heir::of(t).run(&["--info"]);
    assert!(
        tape_line(&sound).starts_with(&format!("Tape: {label}   uuid "))
            && !tape_line(&sound).contains("not known"),
        "{}",
        sound.stdout
    );

    let index_text = text_of(t, 3);
    let line = format!("\nvolume = \"{label}\"\n");
    assert!(index_text.contains(&line), "{index_text}");
    let no_volume = index_text.replacen(&line, "\n", 1);

    let from_index = Heir::of(t);
    fail_read(&from_index, 0);
    let from_seal = Heir::of(t);
    fail_read(&from_seal, 0);
    fail_read(&from_seal, 3);
    let index_says_none = Heir::of(t);
    fail_read(&index_says_none, 0);
    index_says_none.replace(3, no_volume.as_bytes());
    let none = Heir::of(t);
    fail_read(&none, 0);
    none.replace(3, no_volume.as_bytes());
    fail_read(&none, last);
    let uuid = "   uuid not known (File 0 gives none)";
    for (name, h, want) in [
        (
            "File 0 unread",
            &from_index,
            format!("Tape: {label} (File 0 gives no label: this one is the front index's){uuid}"),
        ),
        (
            "File 0 and File 3 unread",
            &from_seal,
            format!("Tape: {label} (File 0 gives no label: this one is the seal marker's){uuid}"),
        ),
        (
            "File 3 states no label",
            &index_says_none,
            format!("Tape: {label} (File 0 gives no label: this one is the seal marker's){uuid}"),
        ),
        (
            "nothing states a label",
            &none,
            format!("Tape: not known (File 0 gives no label, and neither the front index nor a valid seal marker states one){uuid}"),
        ),
    ] {
        let o = h.run(&["--info"]);
        assert_eq!(o.code, 0, "{name}: {}", o.all());
        assert_eq!(tape_line(&o), want, "{name}: {}", o.all());
    }
}

/// F6 at the prompt and after it: a cartridge taken whose File 0 does not
/// read says which tape it is by its front index's label, and that label is
/// the one the run names it by afterwards. The start tape too: with its File
/// 0 unread, `done` with another cartridge in the drive names it by label,
/// not as "the run started on".
#[test]
fn a_tape_whose_file_0_does_not_read_is_named_by_its_front_indexs_label() {
    let t = span2();
    let second = t[1].label.as_str();
    let h = Heir::of_series(&[&t[0], &t[1]]);
    h.fail_file(second, 0);
    let dest = h.dest("out");
    let o = restore_span(&h, &dest, &[second]);
    let all = o.all();
    assert_eq!(o.code, 0, "{all}");
    assert!(
        all.contains(&format!(
            "This cartridge identifies as: {second} (its File 0 gives no label: this one is its front index's)"
        )),
        "{all}"
    );

    let h = Heir::of_series(&[&t[1], &t[0]]);
    fail_read(&h, 0);
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
    assert!(
        all.contains(&format!(
            "NOT IN THE DRIVE: slices 2-8 of 8 of unit {} (version 1) are on the tape {second}, which is not in the drive now",
            heir::UNIT_A
        )) && all.contains(&format!("Load {second} and")),
        "{all}"
    );
    assert!(!all.contains("the run started on"), "{all}");
}

/// F2 of the read of #611's fixes: DI-9's words at the prompt. A cartridge
/// whose File 0 states another layout, or a feature this script does not
/// know, is still refused (item 313, `volume-format-v3.md` 1.2 rule 1); when
/// its seal marker reads and states Layout 3 and no feature, the words say so
/// and point to File 1's steps by hand, as at the start of a run, and no
/// longer send the heir to the RESTORE.sh in its File 2. With the seal
/// marker stating the feature too, the old way on stays. The sound cartridge
/// offered last is taken.
#[test]
fn a_cartridge_refused_on_file_0s_word_at_the_prompt_says_what_its_seal_marker_states() {
    let t = span2();
    let second = t[1].label.as_str();
    let last = t[1].files.len() - 1;
    let base = Heir::of_series(&[&t[0], &t[1]]);
    let want_dest = base.dest("out");
    let o = restore_span(&base, &want_dest, &[second]);
    assert_eq!(o.code, 0, "{}", o.all());

    let h = Heir::of_series(&[&t[0], &t[1]]);
    let feature = |s: &str| s.replacen("requires = []", "requires = [\"future-thing\"]", 1);
    h.copy_cart(second, "LAYOUT4");
    edit_cart_text(&h, "LAYOUT4", 0, |s| {
        s.replacen("\nlayout_version = 3\n", "\nlayout_version = 4\n", 1)
    });
    h.copy_cart(second, "FEATURE");
    edit_cart_text(&h, "FEATURE", 0, feature);
    h.copy_cart(second, "BOTH");
    edit_cart_text(&h, "BOTH", 0, feature);
    edit_cart_text(&h, "BOTH", last, feature);
    let dest = h.dest("out");
    let o = restore_span(&h, &dest, &["LAYOUT4", "FEATURE", "BOTH", second]);
    let all = o.all();
    assert_eq!(o.code, 0, "{all}");
    assert_eq!(all.matches(heir::PROMPT).count(), 4, "{all}");
    let notes: Vec<&str> = all
        .split("\n>>> Load the cartridge")
        .skip(1)
        .take(3)
        .collect();
    assert_eq!(notes.len(), 3, "{all}");
    let (layout4, feat, both) = (notes[0], notes[1], notes[2]);
    assert!(
        layout4.contains(
            "NOTE: this cartridge is a tapectl volume of Layout 4 (magic tapectl-volume-v3)"
        ) && layout4.contains(SEAL_VOUCHES)
            && layout4.contains("by the steps of File 1")
            && !layout4.contains("RESTORE.sh in its own File 2"),
        "{layout4}"
    );
    assert!(
        feat.contains("NOTE: this cartridge requires feature(s) future-thing")
            && feat.contains(SEAL_VOUCHES)
            && feat.contains("by the steps of File 1")
            && !feat.contains("own File 2"),
        "{feat}"
    );
    assert!(
        both.contains("NOTE: this cartridge requires feature(s) future-thing")
            && both.contains("or the RESTORE.sh in this tape's own File 2.")
            && !both.contains(SEAL_VOUCHES),
        "{both}"
    );
    assert!(!all.contains("FATAL"), "{all}");
    assert_eq!(heir::tree(&dest), heir::tree(&want_dest), "{all}");
}

/// F4 of the read of #611's fixes: File 0's `total_files` is a number off a
/// plain-text file like the others, held to at most 18 digits (DI-4(c)). A
/// value of 19 or 20 digits is a damaged line, never bash's own words: the
/// plan's count is then the front index's, and the tape reads as the sound
/// one does.
#[test]
fn a_total_files_too_long_to_be_a_number_is_a_damaged_line() {
    let t = two();
    let good = Heir::of(t).run(&["--info"]);
    let file0 = text_of(t, 0);
    let line = format!("\ntotal_files = {}\n", t.files.len());
    assert!(file0.contains(&line), "{file0}");
    for digits in ["9999999999999999999", "99999999999999999999"] {
        let h = Heir::of(t);
        h.replace(
            0,
            file0
                .replacen(&line, &format!("\ntotal_files = {digits}\n"), 1)
                .as_bytes(),
        );
        let o = h.run(&["--info"]);
        let all = o.all();
        assert_eq!(o.code, 0, "{digits}: {all}");
        assert_no_bash_error(&all, digits);
        assert!(!all.contains("RESTORE.sh: line "), "{digits}: {all}");
        assert!(!all.contains("not consistent"), "{digits}: {all}");
        assert_eq!(o.stdout, good.stdout, "{digits}: {all}");
    }
}

/// F5 of the read of #611's fixes: where `mt` cannot space to end of data,
/// the seal marker is read at its planned position (`volume-format-v3.md`
/// 1.2 rule 6): File 0's pointer, or File 3's `seal_marker` entry when File 0
/// gives none. With File 0 unread a sealed tape is read as sealed, as with
/// File 0 readable, not called unsealed.
#[test]
fn with_file_0_unread_and_no_end_of_data_the_seal_is_found_by_the_front_index() {
    let t = two();
    let no_eod = [("FAKE_MT_NO_EOD", "1")];
    let good = Heir::of(t).run_with(&["--info"], &no_eod);
    assert!(
        good.stdout.contains("the tape is complete"),
        "{}",
        good.all()
    );
    let h = Heir::of(t);
    fail_read(&h, 0);
    let o = h.run_with(&["--info"], &no_eod);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(
        o.stdout
            .contains("the tape is complete, every planned file is on it")
            && !o.all().contains("no seal marker found"),
        "{}",
        o.all()
    );
    let o = h.run_with(&["--verify"], &no_eod);
    let all = o.all();
    assert_eq!(o.code, 1, "{all}");
    assert!(
        o.stdout.contains(&format!(
            "PASS  {:4}  seal_marker          full seal",
            t.files.len() - 1
        )) && !o.stdout.contains("there is no seal marker"),
        "{all}"
    );
    assert!(o.stdout.contains("VERIFY FAILED: 1 file(s)"), "{all}");
    // The read there is the seal's own capped read, by the map's position.
    assert!(
        counted_reads(&h).contains(&(t.files.len() - 1, 128)),
        "{all}"
    );
}

/// F10 of the read of #611's fixes: two messages say only what is true in
/// the state they are printed in. The warning about File 0 is printed before
/// the seal marker is read, so it does not say the seal decides, only that it
/// does if it reads. And where File 0 gives nothing but the front index and
/// the seal marker DO give bytes that are no map, the stop does not say
/// nothing can be read, nor ask whether a cartridge is loaded.
#[test]
fn the_words_for_a_tape_with_no_file_0_say_only_what_is_true() {
    let t = two();
    let last = t.files.len() - 1;
    let unread = Heir::of(t);
    fail_read(&unread, 0);
    fail_read(&unread, last);
    let garbage = Heir::of(t);
    garbage.replace(0, &heir::noise(4096, 7));
    fail_read(&garbage, last);
    for (name, h) in [("File 0 unread", &unread), ("File 0 garbage", &garbage)] {
        let o = h.run(&["--info"]);
        assert_eq!(o.code, 0, "{name}: {}", o.all());
        assert!(
            o.stderr.contains(
                "this is read as a Layout 3 tape, and if a seal marker reads, its own claims decide."
            ) || o.stderr.contains(
                "Going on as a Layout 3 tape; if a seal marker reads, its own claims decide."
            ),
            "{name}: {}",
            o.stderr
        );
        assert!(
            !o.stderr.contains("the seal marker's own claims decide"),
            "{name}: {}",
            o.stderr
        );
    }

    let no_map = Heir::of(t);
    fail_read(&no_map, 0);
    no_map.replace(3, &heir::noise(4096, 11));
    no_map.replace(last, &heir::noise(4096, 12));
    let o = no_map.run(&["--info"]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stderr.contains(
            "FATAL: File 0 gives nothing this script can use, and what the front index and the seal marker give is no map of the tape"
        ) && o.stderr.contains("or this is not a tapectl tape")
            && !o.stderr.contains("nothing can be read")
            && !o.stderr.contains("is a cartridge loaded"),
        "{}",
        o.stderr
    );
}

/// The cartridge `FOREIGN` on the shelf: a first file of 64 MiB, a third of
/// 64 MiB, the rest short text; nothing of tapectl's.
fn shelve_foreign_with_a_long_file_0(h: &Heir) {
    h.shelve(
        "FOREIGN",
        &[
            heir::noise(64 * MIB, 9),
            heir::text(2000, 1),
            heir::text(2000, 2),
            heir::noise(64 * MIB, 3),
            heir::text(2000, 4),
        ],
    );
}

/// F1 in the probe's own read of File 0: a cartridge offered at the prompt
/// whose first file is 64 MiB is read no further than 16 MiB and one block.
/// A foreign one is refused as no tapectl volume, its first file named as
/// longer than a File 0 can be; one whose front index and seal marker are
/// sound is taken by them, as one whose File 0 does not read is. And after
/// `done`, the probe of the cartridge in the drive reads no further either.
#[test]
fn a_cartridge_whose_first_file_is_64_mib_is_read_no_further_than_16_mib() {
    let t = span2();
    let second = t[1].label.as_str();
    let base = Heir::of_series(&[&t[0], &t[1]]);
    let want_dest = base.dest("out");
    let o = restore_span(&base, &want_dest, &[second]);
    assert_eq!(o.code, 0, "{}", o.all());

    let h = Heir::of_series(&[&t[0], &t[1]]);
    shelve_foreign_with_a_long_file_0(&h);
    h.copy_cart(second, "LONG0");
    std::fs::write(h.cart_file("LONG0", 0), heir::noise(64 * MIB, 9)).unwrap();
    let dest = h.dest("out");
    let o = restore_span(&h, &dest, &["FOREIGN", "LONG0"]);
    let all = o.all();
    assert_eq!(o.code, 0, "{all}");
    assert_eq!(all.matches(heir::PROMPT).count(), 2, "{all}");
    let long_words = "its first file is longer than 16 MiB, more than a tapectl File 0 can be";
    assert!(
        all.contains(&format!(
            "NOTE: this cartridge is not a tapectl volume: {long_words}"
        )) && all.contains(&format!("WARNING: this cartridge: {long_words}")),
        "{all}"
    );
    assert_eq!(heir::tree(&dest), heir::tree(&want_dest), "{all}");
    every_read_of_file_is_capped(&h, 0, "the prompt");
    every_read_of_file_is_capped(&h, 3, "the prompt");

    // `done` with the foreign cartridge in the drive.
    let h = Heir::of_series(&[&t[0], &t[1]]);
    shelve_foreign_with_a_long_file_0(&h);
    let dest = h.dest("out");
    let ka = key(&h, Who::TenantA);
    let o = h.run_prompted(
        &["--restore", "--key", &ka, "--all", "--to", dstr(&dest)],
        |k, _| {
            assert_eq!(k, 1, "asked {k} times");
            h.load("FOREIGN");
            Some("done".into())
        },
    );
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        h.reads_by_cartridge().last().unwrap().contains(&3),
        "the probe after done read the cartridge: {:?}",
        h.ops()
    );
    every_read_of_file_is_capped(&h, 0, "after done");
    every_read_of_file_is_capped(&h, 3, "after done");
}
