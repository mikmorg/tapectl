//! RESTORE.sh, the heir's script, File 2 of every Layout 3 tape: part one, the
//! modes that read a tape and restore whole units (the plan's T4.10;
//! `docs/design/volume-format-v3.md` §7.2 to §7.4).
//!
//! The script under test is the one the tape itself holds in File 2, run whole,
//! mode by mode, the way an heir runs it, against a directory tape laid out
//! from a REAL staging and a REAL volume build (`tests/common/heir/mod.rs`).
//! `mt` and `dd` are stand-ins that drive the directory tape and log every
//! call; `age` is `examples/heir_age.rs`, built from the `age` crate; tar,
//! sha256sum, awk, gzip, xz and zstd are the host's own. Every run is under
//! `timeout`.
//!
//! The lint tests (`lint_*`) run `bash -n`, `shellcheck` and the script's awk
//! programs under gawk, mawk and BusyBox awk where each is installed; the CI
//! lint job sets `TAPECTL_REQUIRE_HEIR_TOOLS=1`, which turns a missing tool
//! into a failure (a lint that quietly did not run proves nothing).

#[path = "common/heir/mod.rs"]
mod heir;

use std::path::Path;
use std::sync::OnceLock;

use heir::{Heir, Shape, Tape, Who, LABEL, TENANT_A, TENANT_B, UNIT_A, UNIT_B};

// ---- the tapes, built once each ----

fn two() -> &'static Tape {
    static T: OnceLock<Tape> = OnceLock::new();
    T.get_or_init(|| Tape::build(&Shape::two()))
}

/// The same two units, ended early inside the second slice of the plan: the
/// small unit is whole, the big one has only its first slice.
fn short() -> &'static Tape {
    static T: OnceLock<Tape> = OnceLock::new();
    T.get_or_init(|| {
        let mut shape = Shape::two();
        shape.warning_in_slice = Some(1);
        Tape::build(&shape)
    })
}

fn compressed(alg: &'static str) -> &'static Tape {
    static GZIP: OnceLock<Tape> = OnceLock::new();
    static XZ: OnceLock<Tape> = OnceLock::new();
    static ZSTD: OnceLock<Tape> = OnceLock::new();
    let cell = match alg {
        "gzip" => &GZIP,
        "xz" => &XZ,
        "zstd" => &ZSTD,
        other => panic!("no such algorithm: {other}"),
    };
    cell.get_or_init(|| Tape::build(&Shape::one(alg)))
}

/// Units whose names are equal as numbers and differ as text (#525 A4).
fn numeric() -> &'static Tape {
    static T: OnceLock<Tape> = OnceLock::new();
    T.get_or_init(|| {
        let mut shape = Shape::two();
        shape.units = vec![
            ("7".into(), TENANT_A, false),
            ("007".into(), TENANT_A, false),
        ];
        Tape::build(&shape)
    })
}

fn optional_tools_required() -> bool {
    std::env::var("TAPECTL_REQUIRE_HEIR_TOOLS").is_ok_and(|v| v == "1")
}

/// A host tool a test needs beyond the base set: `None` (with a line saying
/// so) where it is absent, unless the lint job requires it.
fn optional_tool(name: &str) -> Option<std::path::PathBuf> {
    match heir::host_tool(name) {
        Some(p) => Some(p),
        None => {
            assert!(
                !optional_tools_required(),
                "TAPECTL_REQUIRE_HEIR_TOOLS=1 and `{name}` is not installed"
            );
            eprintln!("SKIPPED: `{name}` is not installed on this host");
            None
        }
    }
}

/// Every awk this host has: `(name, how to put it on the PATH as `awk`)`.
fn awks() -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    for name in ["gawk", "mawk"] {
        if let Some(p) = optional_tool(name) {
            out.push((name, format!("exec {} \"$@\"\n", p.display())));
        }
    }
    if let Some(p) = optional_tool("busybox") {
        out.push(("busybox awk", format!("exec {} awk \"$@\"\n", p.display())));
    }
    out
}

fn put_awk(h: &Heir, body: &str) {
    h.put_tool("awk", &format!("#!/bin/sh\n{body}"));
}

fn key(h: &Heir, who: Who) -> String {
    h.key(who).to_string_lossy().into_owned()
}

/// A path as the index writes it: a backslash is `\\\\`.
fn escaped(path: &str) -> String {
    path.replace('\\', "\\\\")
}

fn unit_files<'a>(t: &'a Tape, name: &str) -> &'a [(String, Vec<u8>)] {
    &t.units.iter().find(|u| u.name == name).unwrap().files
}

/// The files of unit `name` restored into `dir` are exactly those it was
/// made of.
fn assert_restored(t: &Tape, name: &str, dir: &Path) {
    let got = heir::tree(dir);
    for (path, bytes) in unit_files(t, name) {
        let have = got
            .get(path)
            .unwrap_or_else(|| panic!("{path} was not restored; got {:?}", got.keys()));
        assert_eq!(have, bytes, "{path}");
    }
}

// ---- the script itself ----

/// The script on the tape is the file in the tree, byte for byte, the same on
/// every tape (§7.4): nothing is substituted into it.
#[test]
fn file_2_is_the_file_in_the_tree_byte_for_byte() {
    let tree = tapectl::volume::heir::RESTORE_SH.as_bytes();
    assert_eq!(two().script, tree);
    assert_eq!(short().script, tree);
    assert_eq!(compressed("gzip").script, tree);
    assert!(
        !String::from_utf8_lossy(tree).contains("__"),
        "no placeholder is left in the script"
    );
}

#[test]
fn lint_bash_parses_the_script() {
    let h = Heir::of(two());
    let o = std::process::Command::new("bash")
        .arg("-n")
        .arg(h.root.path().join("RESTORE.sh"))
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
}

#[test]
fn lint_shellcheck_is_clean_at_warning_level() {
    let Some(shellcheck) = optional_tool("shellcheck") else {
        return;
    };
    let h = Heir::of(two());
    let o = std::process::Command::new(shellcheck)
        .args(["-S", "warning"])
        .arg(h.root.path().join("RESTORE.sh"))
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
}

/// The script's awk programs read the same under every awk this host has:
/// the whole modes run under each and give the same words (the awk
/// differences the review found: numeric-looking unit names).
#[test]
fn lint_every_awk_reads_the_modes_alike() {
    let all = awks();
    assert!(
        !all.is_empty(),
        "no awk at all on this host (gawk, mawk or busybox)"
    );
    let mut outputs: Vec<(String, String)> = Vec::new();
    for (name, body) in &all {
        let h = Heir::of(two());
        put_awk(&h, body);
        let ka = key(&h, Who::TenantA);
        let info = h.run(&["--info"]);
        assert_eq!(info.code, 0, "{name}: {}", info.all());
        let list = h.run(&["--list", "--key", &ka]);
        assert_eq!(list.code, 0, "{name}: {}", list.all());
        let dest = h.dest("out");
        let r = h.run(&["--restore", "--key", &ka, "--to", dest.to_str().unwrap()]);
        assert_eq!(r.code, 0, "{name}: {}", r.all());
        assert_restored(two(), UNIT_A, &dest);
        let v = h.run(&["--verify"]);
        assert_eq!(v.code, 0, "{name}: {}", v.all());
        let find = h.run(&["--find-envelope", "--key", &ka]);
        outputs.push((
            name.to_string(),
            format!("{}\n{}\n{}", info.stdout, list.stdout, find.stdout),
        ));
    }
    // The words each mode printed are the same under every awk, but for the
    // work directory's random name.
    let norm = |s: &str| {
        s.lines()
            .filter(|l| !l.contains("tapectl-work") && !l.contains("To restore, run:"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    for (name, out) in &outputs[1..] {
        let (a, b) = (norm(out), norm(&outputs[0].1));
        if let Some((x, y)) = a.lines().zip(b.lines()).find(|(x, y)| x != y) {
            panic!("{name} against {}: {x:?} against {y:?}", outputs[0].0);
        }
        assert_eq!(a, b, "{name} against {}", outputs[0].0);
    }
}

/// #525 A4: two units whose names are equal as numbers read as two units under
/// every awk: `--unit 7` lists, restores and checks unit 7 alone.
#[test]
fn lint_numeric_looking_unit_names_are_two_units_under_every_awk() {
    for (name, body) in awks() {
        let t = numeric();
        let h = Heir::of(t);
        put_awk(&h, &body);
        let ka = key(&h, Who::TenantA);
        let list = h.run(&["--list", "--key", &ka, "--unit", "7"]);
        assert_eq!(list.code, 0, "{name}: {}", list.all());
        assert!(list.stdout.contains("Unit 7 "), "{name}: {}", list.stdout);
        assert!(!list.stdout.contains("Unit 007"), "{name}: {}", list.stdout);
        let dest = h.dest("seven");
        let r = h.run(&[
            "--restore",
            "--key",
            &ka,
            "--to",
            dest.to_str().unwrap(),
            "--unit",
            "7",
        ]);
        assert_eq!(r.code, 0, "{name}: {}", r.all());
        assert!(r.stdout.contains("RESTORE COMPLETE"), "{name}: {}", r.all());
        assert_restored(t, "7", &dest);
        assert!(
            !dest.join(".tapectl-damaged-files.txt").exists(),
            "{name}: unit 007's files were expected under unit 7"
        );
    }
}

// ---- usage and the tool check ----

#[test]
fn an_unknown_option_a_missing_mode_and_a_missing_key_are_told_by_name() {
    let h = Heir::of(two());
    let o = h.run(&["--frobnicate"]);
    assert_eq!(o.code, 2, "{}", o.all());
    assert!(
        o.stderr.contains("unknown option: --frobnicate"),
        "{}",
        o.stderr
    );
    let o = h.run(&[]);
    assert_eq!(o.code, 2);
    assert!(o.stderr.contains("no mode given"), "{}", o.stderr);
    let o = h.run(&["--restore", "--to", "/nowhere"]);
    assert_eq!(o.code, 2);
    assert!(o.stderr.contains("--key"), "{}", o.stderr);
    let o = h.run(&["--find-envelope"]);
    assert_eq!(o.code, 2);
    assert!(o.stderr.contains("--key"), "{}", o.stderr);
    assert!(h.ops().is_empty(), "no usage error reads the tape");
}

/// The modes of later builds are refused by name, before the tape moves.
#[test]
fn the_modes_part_two_and_three_carry_are_refused_by_name() {
    let h = Heir::of(two());
    let ka = key(&h, Who::TenantA);
    for args in [
        vec![
            "--restore",
            "--key",
            &ka,
            "--to",
            "/x",
            "--unit",
            "u",
            "--path",
            "p",
        ],
        vec![
            "--restore",
            "--key",
            &ka,
            "--to",
            "/x",
            "--paths-from",
            "list",
        ],
        vec!["--where", "u", "--key", &ka],
        vec!["--find", "text", "--key", &ka],
        vec![
            "--restore",
            "--key",
            &ka,
            "--to",
            "/x",
            "--slices-from",
            "/y",
        ],
        vec!["--restore", "--key", &ka, "--to", "/x", "--swap-tapes"],
    ] {
        let o = h.run(&args);
        assert_eq!(o.code, 1, "{args:?}: {}", o.all());
        assert!(
            o.stderr.contains("is not in this build of RESTORE.sh yet"),
            "{args:?}: {}",
            o.stderr
        );
    }
    assert!(h.ops().is_empty(), "a refusal reads no tape");
    let o = h.run(&["--restore", "--key", &ka, "--to", "/x", "--scratch", "/y"]);
    assert_eq!(o.code, 2);
    assert!(o.stderr.contains("--work"), "{}", o.stderr);
}

/// #449: each mode checks the tools it runs, before the tape is read: a host
/// with no `age` still reads and verifies a tape.
#[test]
fn each_mode_checks_the_tools_it_runs_and_no_others() {
    let t = two();
    let h = Heir::of(t);
    h.remove_tool("age");
    let ka = key(&h, Who::TenantA);
    for args in [
        vec!["--find-envelope", "--key", &ka],
        vec!["--list", "--key", &ka],
        vec!["--restore", "--key", &ka, "--to", "/nowhere-at-all"],
    ] {
        let o = h.run(&args);
        assert_eq!(o.code, 1, "{args:?}: {}", o.all());
        assert!(
            o.stderr.contains("missing required tool(s): age"),
            "{args:?}: {}",
            o.stderr
        );
    }
    assert!(
        h.ops().is_empty(),
        "the tool check is before the first tape read"
    );
    assert_eq!(h.run(&["--info"]).code, 0);
    assert_eq!(h.run(&["--verify"]).code, 0);

    let h = Heir::of(t);
    h.remove_tool("mt");
    let o = h.run(&["--info"]);
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("mt (package mt-st)"), "{}", o.stderr);
}

/// The decompressor is in the tool table for a unit that states compression,
/// and only for such a unit: refused before any slice is read, naming the
/// tool; an uncompressed unit is never asked for one.
#[test]
fn a_decompressor_is_needed_only_by_a_unit_that_states_one() {
    let t = compressed("xz");
    let h = Heir::of(t);
    h.remove_tool("xz");
    let ka = key(&h, Who::TenantA);
    let dest = h.dest("out");
    let o = h.run(&["--restore", "--key", &ka, "--to", dest.to_str().unwrap()]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stderr.contains("needs the tool 'xz'") && o.stderr.contains("compressed with xz"),
        "{}",
        o.stderr
    );
    let slice_positions = t.slice_positions();
    assert!(
        h.dd_reads().iter().all(|p| !slice_positions.contains(p)),
        "no slice was read: {:?}",
        h.dd_reads()
    );
    // An uncompressed unit restores on a host with none of the three.
    let h = Heir::of(two());
    for tool in ["gzip", "xz", "zstd"] {
        h.remove_tool(tool);
    }
    let ka = key(&h, Who::TenantA);
    let dest = h.dest("out");
    let o = h.run(&["--restore", "--key", &ka, "--to", dest.to_str().unwrap()]);
    assert_eq!(o.code, 0, "{}", o.all());
}

// ---- File 0's claims ----

fn file_0_with(t: &Tape, edit: impl Fn(&str) -> String) -> Vec<u8> {
    let text = String::from_utf8_lossy(&t.files[0]).into_owned();
    let text = text.trim_end_matches('\0').to_string();
    edit(&text).into_bytes()
}

#[test]
fn another_layout_is_refused_by_name_with_its_label() {
    let t = two();
    let h = Heir::of(t);
    h.replace(
        0,
        &file_0_with(t, |s| {
            s.replace("tapectl-volume-v3", "tapectl-volume-v2")
                .replace("layout_version = 3", "layout_version = 2")
        }),
    );
    let o = h.run(&["--info"]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stderr.contains("a tapectl volume of Layout 2")
            && o.stderr.contains("magic tapectl-volume-v2")
            && o.stderr.contains(LABEL)
            && o.stderr.contains("Layout 3 only")
            && o.stderr.contains("RESTORE.sh in its own File 2"),
        "{}",
        o.stderr
    );
    // Nothing but File 0 was read.
    assert_eq!(h.dd_reads(), vec![0]);
}

#[test]
fn a_tape_requiring_a_feature_is_refused_by_name_and_a_missing_magic_is_only_warned() {
    let t = two();
    let h = Heir::of(t);
    h.replace(
        0,
        &file_0_with(t, |s| {
            s.replace(
                "requires = []",
                "requires = [\"future-thing\", \"another\"]",
            )
        }),
    );
    let o = h.run(&["--info"]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stderr.contains("File 0 requires feature(s)")
            && o.stderr.contains("future-thing another"),
        "{}",
        o.stderr
    );
    let h = Heir::of(t);
    h.replace(
        0,
        &file_0_with(t, |s| {
            s.replace("magic = \"tapectl-volume-v3\"\n", "")
                .replace("layout_version = 3\n", "")
        }),
    );
    let o = h.run(&["--info"]);
    assert_eq!(
        o.code,
        0,
        "a damaged File 0 is warned about, never a hard stop: {}",
        o.all()
    );
    assert!(
        o.stderr.contains("states no magic or no layout_version"),
        "{}",
        o.stderr
    );
}

// ---- --info ----

#[test]
fn info_names_the_tape_its_files_and_its_full_seal_and_rewinds_once() {
    let t = two();
    let h = Heir::of(t);
    let o = h.run(&["--info"]);
    assert_eq!(o.code, 0, "{}", o.all());
    let s = &o.stdout;
    assert!(s.contains(&format!("Tape: {LABEL}")), "{s}");
    assert!(s.contains("Layout 3"), "{s}");
    assert!(
        s.contains("the tape is complete, every planned file is on it"),
        "{s}"
    );
    assert!(s.contains("The seal binds the front index"), "{s}");
    for e in &t.entries {
        assert!(s.contains(&e.type_label), "{} in {s}", e.type_label);
    }
    // The tape is read forward from one rewind: File 0, File 3 (spaced to by
    // filemarks), then the seal at end of data, and nothing behind it.
    assert_eq!(h.count_ops("mt rewind"), 1, "{:?}", h.ops());
    assert_eq!(h.dd_reads(), vec![0, 3, t.files.len() - 1], "{:?}", h.ops());
}

#[test]
fn info_on_a_short_tape_says_where_it_ended_and_names_the_planned_files_not_on_it() {
    let t = short();
    assert!(matches!(
        t.ending,
        tapectl::volume::session::Ending::Short { .. }
    ));
    let h = Heir::of(t);
    let o = h.run(&["--info"]);
    assert_eq!(o.code, 0, "{}", o.all());
    let ended = t.ended_after();
    assert!(
        o.stdout
            .contains(&format!("ended EARLY, after file {ended}")),
        "{}",
        o.stdout
    );
    assert!(
        o.stdout
            .contains(&format!("NOT ON TAPE  files {} to ", ended + 1))
            && o.stdout.contains("(planned; the tape ended after file"),
        "{}",
        o.stdout
    );
    // The seal is the file after the last data slice, and nothing past it
    // was asked of the tape.
    assert!(
        h.dd_reads().iter().all(|p| *p <= ended + 1),
        "{:?}",
        h.dd_reads()
    );
}

#[test]
fn a_tape_with_no_seal_reads_as_unsealed() {
    let t = two();
    let h = Heir::of(t);
    h.cut_after(t.files.len() - 1);
    let o = h.run(&["--info"]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(o.stdout.contains("Seal marker: NOT FOUND"), "{}", o.stdout);
    assert!(o.stderr.contains("read as unsealed"), "{}", o.stderr);
    // --verify on it verifies what is there and says that is not completeness.
    let o = h.run(&["--verify"]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(o.stdout.contains("VERIFY INCOMPLETE"), "{}", o.stdout);
}

#[test]
fn the_seal_is_found_at_end_of_data_when_file_0_points_elsewhere_and_gnu_mt_is_named() {
    let t = two();
    let h = Heir::of(t);
    // GNU mt spells end of data `eom`, and cannot set the block size.
    let o = h.run_with(&["--info"], &[("FAKE_MT_GNU", "1")]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(
        o.stderr.contains("is not mt-st") && o.stderr.contains("cannot set the block size"),
        "{}",
        o.stderr
    );
    assert!(
        h.count_ops("mt eom") >= 1 || h.count_ops("mt eod") >= 1,
        "{:?}",
        h.ops()
    );
    let h = Heir::of(t);
    let o = h.run_with(&["--info"], &[("FAKE_MT_SETBLK_FAILS", "1")]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stderr
            .contains("mt-st itself refused, so the device is the problem"),
        "{}",
        o.stderr
    );
}

// ---- --verify ----

#[test]
fn verify_passes_a_full_tape_with_no_key_and_reads_it_forward_once() {
    let t = two();
    let h = Heir::of(t);
    let o = h.run(&["--verify"]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(o.stdout.contains("VERIFY PASSED"), "{}", o.stdout);
    assert_eq!(
        o.stdout.matches("PASS ").count(),
        t.files.len(),
        "every file of the tape has a PASS line: {}",
        o.stdout
    );
    let reads = h.dd_reads();
    // One rewind to start, and one back from the seal at end of data; the
    // verification between them reads every file forward, in order.
    assert_eq!(h.count_ops("mt rewind"), 2, "{:?}", h.ops());
    let walk = &reads[3..];
    assert!(walk.windows(2).all(|w| w[0] < w[1]), "{reads:?}");
    for p in 0..t.files.len() {
        assert!(reads.contains(&p), "file {p} was never read: {reads:?}");
    }
}

#[test]
fn verify_on_a_short_tape_hashes_what_is_on_it_and_names_the_rest_not_on_tape() {
    let t = short();
    let h = Heir::of(t);
    let o = h.run(&["--verify"]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(o.stdout.contains("VERIFY PASSED"), "{}", o.stdout);
    assert_eq!(o.stdout.matches("NOT ON TAPE").count(), 1, "{}", o.stdout);
    assert!(
        h.dd_reads().iter().all(|p| *p <= t.ended_after() + 1),
        "R3: nothing past the end is read: {:?}",
        h.dd_reads()
    );
}

#[test]
fn verify_names_the_file_that_differs_and_exits_nonzero() {
    let t = two();
    let h = Heir::of(t);
    let slice = t.slice_positions()[1];
    h.flip(slice, 100);
    let o = h.run(&["--verify"]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stdout.contains(&format!("FAIL  {slice:4}  data_slice")),
        "{}",
        o.stdout
    );
    assert!(
        o.stdout.contains("VERIFY FAILED: 1 file(s)"),
        "{}",
        o.stdout
    );
}

#[test]
fn a_front_index_the_seal_does_not_bind_is_named() {
    let t = two();
    let h = Heir::of(t);
    // A byte of File 3's header text: still a consistent map, another hash.
    h.flip(3, 10);
    let o = h.run(&["--verify"]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stdout.contains("the seal marker binds another hash"),
        "{}",
        o.stdout
    );
}

// ---- --find-envelope ----

#[test]
fn find_envelope_with_a_tenant_key_shows_that_tenants_units_only() {
    let t = two();
    let h = Heir::of(t);
    let o = h.run(&["--find-envelope", "--key", &key(&h, Who::TenantA)]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(o.stdout.contains(UNIT_A), "{}", o.stdout);
    assert!(!o.stdout.contains(UNIT_B), "{}", o.stdout);
    // One unit: the closing line restores it with --all.
    assert!(
        o.stdout.contains("--restore --key KEY --to DIR --all"),
        "{}",
        o.stdout
    );
}

/// #525 A5: with more than one unit the closing line names --unit or --all,
/// never a command that refuses.
#[test]
fn find_envelope_with_the_escrow_key_lists_every_unit_and_closes_with_a_command_that_works() {
    let t = two();
    let h = Heir::of(t);
    let o = h.run(&["--find-envelope", "--key", &key(&h, Who::Escrow)]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(
        o.stdout.contains(UNIT_A) && o.stdout.contains(UNIT_B),
        "{}",
        o.stdout
    );
    assert!(
        o.stdout.contains("--unit NAME") && o.stdout.contains("--all"),
        "{}",
        o.stdout
    );
    assert!(
        o.stdout.contains("The key opens the envelope at file"),
        "{}",
        o.stdout
    );
}

#[test]
fn a_key_that_is_not_a_recipient_opens_nothing_and_says_so() {
    let t = two();
    let h = Heir::of(t);
    let other = tempfile_key();
    let o = h.run(&["--find-envelope", "--key", other.to_str().unwrap()]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stderr.contains("no envelope on this tape")
            && o.stderr.contains("opens with the key given"),
        "{}",
        o.stderr
    );
    assert!(
        !o.stderr.contains("DAMAGED"),
        "a wrong key is not damage: {}",
        o.stderr
    );
    let h = Heir::of(t);
    let o = h.run(&["--find-envelope", "--key", "/no/such/key"]);
    assert_eq!(o.code, 1);
    assert!(
        o.stderr.contains("does not exist or cannot be read"),
        "{}",
        o.stderr
    );
    assert!(
        h.ops().is_empty(),
        "the missing key is told before the tape is read"
    );
}

fn tempfile_key() -> std::path::PathBuf {
    use age::secrecy::ExposeSecret;
    let id = age::x25519::Identity::generate();
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("other.key");
    std::fs::write(&p, format!("{}\n", id.to_string().expose_secret())).unwrap();
    std::mem::forget(dir);
    p
}

// ---- --list ----

#[test]
fn list_shows_a_units_files_from_its_table_and_reads_no_slice() {
    let t = two();
    let h = Heir::of(t);
    let o = h.run(&["--list", "--key", &key(&h, Who::TenantB), "--unit", UNIT_B]);
    assert_eq!(o.code, 0, "{}", o.all());
    for (path, _) in unit_files(t, UNIT_B) {
        // The table's paths are written as the index writes them: a
        // backslash is `\\\\`.
        let shown = path.replace('\\', "\\\\");
        assert!(
            o.stdout.contains(&shown),
            "{path} is not listed:\n{}",
            o.stdout
        );
    }
    assert!(o.stdout.contains("a-symlink"), "{}", o.stdout);
    assert!(
        o.stdout.contains("  h  "),
        "a hard link is listed as one: {}",
        o.stdout
    );
    let slices = t.slice_positions();
    assert!(
        h.dd_reads().iter().all(|p| !slices.contains(p)),
        "--list reads no data slice: {:?}",
        h.dd_reads()
    );
}

#[test]
fn list_with_the_escrow_key_lists_every_unit() {
    let t = two();
    let h = Heir::of(t);
    let o = h.run(&["--list", "--key", &key(&h, Who::Escrow)]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(
        o.stdout.contains(&format!("Unit {UNIT_A} ")),
        "{}",
        o.stdout
    );
    assert!(
        o.stdout.contains(&format!("Unit {UNIT_B} ")),
        "{}",
        o.stdout
    );
}

// ---- --restore of whole units ----

#[test]
fn a_lone_unit_restores_byte_for_byte_into_to_and_the_work_directory_is_gone() {
    let t = two();
    let h = Heir::of(t);
    let dest = h.dest("out");
    let o = h.run(&[
        "--restore",
        "--key",
        &key(&h, Who::TenantA),
        "--to",
        dest.to_str().unwrap(),
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(o.stdout.contains("RESTORE COMPLETE"), "{}", o.stdout);
    assert_restored(t, UNIT_A, &dest);
    assert!(
        o.stdout
            .contains("regular file(s) match the sha256 in the unit's file table"),
        "{}",
        o.stdout
    );
    // The work directory was inside --to, and is gone.
    let entries: Vec<_> = std::fs::read_dir(&dest)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        entries.iter().all(|n| !n.starts_with(".tapectl-work")),
        "{entries:?}"
    );
    assert!(!dest.join(".tapectl-damaged-files.txt").exists());
    // The symlink and the hard link are what they were.
    let link = std::fs::read_link(dest.join("a-symlink")).unwrap();
    assert_eq!(link.to_string_lossy(), format!("{UNIT_A}-small.txt"));
    use std::os::unix::fs::MetadataExt;
    assert_eq!(
        std::fs::metadata(dest.join(format!("{UNIT_A}-small.txt")))
            .unwrap()
            .ino(),
        std::fs::metadata(dest.join(format!("{UNIT_A}-hardlink.txt")))
            .unwrap()
            .ino(),
        "a hard link stays a link"
    );
}

#[test]
fn the_work_directory_is_made_inside_to_and_work_replaces_it() {
    let t = two();
    let h = Heir::of(t);
    let dest = h.dest("out");
    let elsewhere = h.dest("elsewhere");
    // A tar that lists where the work directory is, while it runs.
    let real = heir::host_tool("tar").unwrap();
    h.put_tool(
        "tar",
        &format!(
            "#!/bin/bash\nfor a; do case \"$a\" in --version) exec {real} \"$a\" ;; esac; done\nls -A \"{d}\" >>\"{log}\"\nls -A \"{e}\" >>\"{log}\"\nexec {real} \"$@\"\n",
            real = real.display(),
            d = dest.display(),
            e = elsewhere.display(),
            log = h.root.path().join("workdirs.log").display()
        ),
    );
    let o = h.run(&[
        "--restore",
        "--key",
        &key(&h, Who::TenantA),
        "--to",
        dest.to_str().unwrap(),
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    let log = std::fs::read_to_string(h.root.path().join("workdirs.log")).unwrap();
    assert!(log.contains(".tapectl-work."), "inside --to: {log}");
    std::fs::remove_file(h.root.path().join("workdirs.log")).unwrap();
    let dest2 = h.dest("out2");
    let o = h.run(&[
        "--restore",
        "--key",
        &key(&h, Who::TenantA),
        "--to",
        dest2.to_str().unwrap(),
        "--work",
        elsewhere.to_str().unwrap(),
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    let log = std::fs::read_to_string(h.root.path().join("workdirs.log")).unwrap();
    assert!(log.contains(".tapectl-work."), "inside --work: {log}");
    assert!(
        std::fs::read_dir(&dest2).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".tapectl-work")),
        "not inside --to when --work is given"
    );
}

#[test]
fn all_restores_every_unit_each_in_its_own_directory_and_reads_the_tape_forward() {
    let t = two();
    let h = Heir::of(t);
    let dest = h.dest("out");
    let o = h.run(&[
        "--restore",
        "--key",
        &key(&h, Who::Escrow),
        "--to",
        dest.to_str().unwrap(),
        "--all",
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(
        o.stdout.contains("RESTORE COMPLETE: 2 unit(s)"),
        "{}",
        o.stdout
    );
    assert_restored(t, UNIT_A, &dest.join(UNIT_A));
    assert_restored(t, UNIT_B, &dest.join(UNIT_B));
    // Each slice of each unit was read once, and the slices in tape order.
    let reads = h.dd_reads();
    for s in t.slice_positions() {
        assert_eq!(
            reads.iter().filter(|p| **p == s).count(),
            1,
            "slice {s}: {reads:?}"
        );
    }
    let order: Vec<usize> = reads
        .iter()
        .copied()
        .filter(|p| t.slice_positions().contains(p))
        .collect();
    assert!(order.windows(2).all(|w| w[0] < w[1]), "{order:?}");
}

#[test]
fn repeated_unit_restores_those_units_across_tenants_with_the_escrow_key() {
    let t = two();
    let h = Heir::of(t);
    let dest = h.dest("out");
    let o = h.run(&[
        "--restore",
        "--key",
        &key(&h, Who::Operator),
        "--to",
        dest.to_str().unwrap(),
        "--unit",
        UNIT_B,
        "--unit",
        UNIT_A,
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert_restored(t, UNIT_A, &dest.join(UNIT_A));
    assert_restored(t, UNIT_B, &dest.join(UNIT_B));
}

#[test]
fn several_units_without_a_choice_are_refused_listing_them_and_a_missing_unit_is_named() {
    let t = two();
    let h = Heir::of(t);
    let dest = h.dest("out");
    let o = h.run(&[
        "--restore",
        "--key",
        &key(&h, Who::Escrow),
        "--to",
        dest.to_str().unwrap(),
    ]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stderr
            .contains("multiple units found; specify one with --unit NAME, or --all")
            && o.stderr.contains(UNIT_A)
            && o.stderr.contains(UNIT_B),
        "{}",
        o.stderr
    );
    let slices = t.slice_positions();
    assert!(
        h.dd_reads().iter().all(|p| !slices.contains(p)),
        "no slice was read"
    );
    let o = h.run(&[
        "--restore",
        "--key",
        &key(&h, Who::TenantA),
        "--to",
        dest.to_str().unwrap(),
        "--unit",
        "nothing-by-this-name",
    ]);
    assert_eq!(o.code, 1);
    assert!(
        o.stderr
            .contains("is not in the envelope(s) your key opened")
            && o.stderr.contains(UNIT_A),
        "{}",
        o.stderr
    );
}

#[test]
fn a_non_empty_destination_is_refused_before_the_tape_is_read_and_overwrite_replaces() {
    let t = two();
    let h = Heir::of(t);
    let dest = h.dest("out");
    std::fs::write(dest.join("old.txt"), "x").unwrap();
    let ka = key(&h, Who::TenantA);
    let o = h.run(&["--restore", "--key", &ka, "--to", dest.to_str().unwrap()]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(o.stderr.contains("is not empty"), "{}", o.stderr);
    assert!(
        h.ops().is_empty(),
        "the destination is judged before the tape moves"
    );
    // A truncated file of an earlier run is replaced under --overwrite.
    std::fs::write(dest.join(format!("{UNIT_A}-small.txt")), "short").unwrap();
    let o = h.run(&[
        "--restore",
        "--key",
        &ka,
        "--to",
        dest.to_str().unwrap(),
        "--overwrite",
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert_restored(t, UNIT_A, &dest);
    assert!(
        dest.join("old.txt").exists(),
        "--overwrite removes nothing else"
    );
}

#[test]
fn a_version_is_chosen_by_number_and_a_wrong_one_is_named() {
    let t = two();
    let h = Heir::of(t);
    let dest = h.dest("out");
    let ka = key(&h, Who::TenantA);
    let o = h.run(&[
        "--restore",
        "--key",
        &ka,
        "--to",
        dest.to_str().unwrap(),
        "--version",
        "1",
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    let dest2 = h.dest("out2");
    let o = h.run(&[
        "--restore",
        "--key",
        &ka,
        "--to",
        dest2.to_str().unwrap(),
        "--version",
        "9",
    ]);
    assert_eq!(o.code, 1);
    assert!(
        o.stderr.contains("hold no unit of version 9"),
        "{}",
        o.stderr
    );
}

/// §7.2 step 5: the file check, never tar's exit status, decides. A tar that
/// drops a file and exits 0 (as BusyBox tar does) is caught.
#[test]
fn a_tar_that_drops_a_file_at_exit_0_is_caught_by_the_file_check() {
    let t = two();
    let h = Heir::of(t);
    let real = heir::host_tool("tar").unwrap();
    let victim = format!("inner/{UNIT_A}-deep.dat");
    h.put_tool("tar", &heir::dropping_tar(&real, &victim));
    let dest = h.dest("out");
    let o = h.run(&[
        "--restore",
        "--key",
        &key(&h, Who::TenantA),
        "--to",
        dest.to_str().unwrap(),
    ]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stdout.contains("RESTORE INCOMPLETE: 1 path(s)"),
        "{}",
        o.stdout
    );
    assert!(o.stdout.contains("File check: 1 of "), "{}", o.stdout);
    let list = std::fs::read_to_string(dest.join(".tapectl-damaged-files.txt")).unwrap();
    let mut lines = list.lines();
    let first = lines.next().unwrap();
    assert!(
        first.starts_with("#tapectl-damaged-files\t1\tunit=")
            && first.contains(&format!("unit={UNIT_A}\t"))
            && first.contains("\tuuid=")
            && first.ends_with("\tversion=1"),
        "{first}"
    );
    assert_eq!(lines.collect::<Vec<_>>(), vec![victim.as_str()]);
    // The same run with the file check off says it did not check.
    let dest2 = h.dest("out2");
    let o = h.run(&[
        "--restore",
        "--key",
        &key(&h, Who::TenantA),
        "--to",
        dest2.to_str().unwrap(),
        "--no-file-check",
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(
        o.stderr
            .contains("--no-file-check: the restored files were not checked"),
        "{}",
        o.stderr
    );
}

/// BusyBox tar restores content only; the script says which tar it found and
/// what it does not restore.
#[test]
fn busybox_tar_restores_the_content_and_the_script_says_what_it_does_not_restore() {
    let Some(bb) = optional_tool("busybox") else {
        return;
    };
    let t = two();
    let h = Heir::of(t);
    h.put_tool(
        "tar",
        &format!("#!/bin/sh\nexec {} tar \"$@\"\n", bb.display()),
    );
    let dest = h.dest("out");
    let o = h.run(&[
        "--restore",
        "--key",
        &key(&h, Who::TenantB),
        "--to",
        dest.to_str().unwrap(),
    ]);
    assert!(o.stdout.contains("CONTENT ONLY"), "{}", o.all());
    assert_restored(t, UNIT_B, &dest);
    assert_eq!(o.code, 0, "{}", o.all());
    // And GNU tar's own words are what the host's tar said.
    let h = Heir::of(t);
    let dest = h.dest("out");
    let o = h.run(&[
        "--restore",
        "--key",
        &key(&h, Who::TenantB),
        "--to",
        dest.to_str().unwrap(),
    ]);
    assert!(
        o.stdout.contains("The tar used: tar (GNU tar)"),
        "{}",
        o.stdout
    );
}

/// A TAR_OPTIONS in the heir's environment is cleared; LC_ALL is C.
#[test]
fn a_tar_options_in_the_heirs_environment_does_not_change_the_restore() {
    let t = two();
    let h = Heir::of(t);
    let dest = h.dest("out");
    let o = h.run_with(
        &[
            "--restore",
            "--key",
            &key(&h, Who::TenantA),
            "--to",
            dest.to_str().unwrap(),
        ],
        &[
            ("TAR_OPTIONS", "--exclude=*"),
            ("LANG", "de_DE.UTF-8"),
            ("POSIXLY_CORRECT", "1"),
        ],
    );
    assert_eq!(o.code, 0, "{}", o.all());
    assert_restored(t, UNIT_A, &dest);
}

// ---- envelopes: the backup copies ----

/// §1.3: a damaged primary is replaced by its backup (the entry with the same
/// ciphertext hash); a backup whose primary opened is never read.
#[test]
fn a_backup_is_read_only_when_its_primary_does_not_read() {
    let t = two();
    let tenant_a_primary = t
        .positions_of("tenant_envelope")
        .into_iter()
        .find(|p| {
            // The one tenant A's key opens.
            let h = Heir::of(t);
            let o = h.run(&["--find-envelope", "--key", &key(&h, Who::TenantA)]);
            o.stdout.contains(&format!("envelope at file {p} "))
        })
        .unwrap();
    let backups = t.positions_of("tenant_envelope_backup");
    // With the primary whole: no backup is read.
    let h = Heir::of(t);
    let o = h.run(&["--find-envelope", "--key", &key(&h, Who::TenantA)]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(
        h.dd_reads().iter().all(|p| !backups.contains(p)),
        "{:?}",
        h.dd_reads()
    );
    // A damaged primary: the restore goes on from the backup.
    let h = Heir::of(t);
    h.flip(tenant_a_primary, 200);
    let dest = h.dest("out");
    let o = h.run(&[
        "--restore",
        "--key",
        &key(&h, Who::TenantA),
        "--to",
        dest.to_str().unwrap(),
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(
        o.stderr.contains("reading the backup copy of the envelope"),
        "{}",
        o.stderr
    );
    assert_restored(t, UNIT_A, &dest);
    // An unreadable primary (a failed read): the same.
    let h = Heir::of(t);
    let dest = h.dest("out");
    let o = h.run_with(
        &[
            "--restore",
            "--key",
            &key(&h, Who::TenantA),
            "--to",
            dest.to_str().unwrap(),
        ],
        &[("FAKE_DD_FAIL", &tenant_a_primary.to_string())],
    );
    assert_eq!(o.code, 0, "{}", o.all());
    assert_restored(t, UNIT_A, &dest);
}

#[test]
fn an_envelope_that_cannot_be_read_anywhere_is_called_damaged_and_not_a_wrong_key() {
    let t = two();
    let h = Heir::of(t);
    for p in t
        .positions_of("tenant_envelope")
        .into_iter()
        .chain(t.positions_of("tenant_envelope_backup"))
        .chain(t.positions_of("operator_envelope"))
        .chain(t.positions_of("operator_envelope_backup"))
    {
        h.flip(p, 300);
    }
    let o = h.run(&["--find-envelope", "--key", &key(&h, Who::TenantA)]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stderr.contains("envelope(s) on this tape are DAMAGED"),
        "{}",
        o.stderr
    );
}

/// #520: a work directory that fills while the envelope is unpacked is said
/// to be that, and no envelope is counted damaged.
#[test]
fn a_full_work_directory_while_an_envelope_is_unpacked_is_not_called_damage() {
    let t = two();
    let h = Heir::of(t);
    let real = heir::host_tool("tar").unwrap();
    h.put_tool(
        "tar",
        &format!(
            "#!/bin/bash\nfor a; do case \"$a\" in --version) exec {real} \"$a\" ;; esac; done\ncat >/dev/null\necho 'tar: MANIFEST.toml: Cannot write: No space left on device' >&2\nexit 2\n",
            real = real.display()
        ),
    );
    let dest = h.dest("out");
    let o = h.run(&[
        "--restore",
        "--key",
        &key(&h, Who::TenantA),
        "--to",
        dest.to_str().unwrap(),
    ]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stderr
            .contains("cannot take it: it is full, or not writable")
            && o.stderr.contains("--work DIR"),
        "{}",
        o.stderr
    );
    assert!(!o.stderr.contains("DAMAGED"), "{}", o.stderr);
    assert!(!o.stderr.contains("matched the envelope"), "{}", o.stderr);
    // A tape read that fills the work directory is named too: the copy of a
    // file on its way off the tape is written by `head`.
    let h = Heir::of(t);
    let real_head = heir::host_tool("head").unwrap();
    h.put_tool(
        "head",
        &format!(
            "#!/bin/bash\ncase \"$*\" in\n  \"-c \"[0-9][0-9][0-9][0-9][0-9]*)\n    cat >/dev/null\n    echo \"head: error writing 'standard output': No space left on device\" >&2\n    exit 1 ;;\nesac\nexec {real} \"$@\"\n",
            real = real_head.display()
        ),
    );
    let dest = h.dest("out");
    let o = h.run(&[
        "--restore",
        "--key",
        &key(&h, Who::TenantA),
        "--to",
        dest.to_str().unwrap(),
    ]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stderr
            .contains("cannot take it: it is full, or not writable"),
        "{}",
        o.stderr
    );
}

// ---- damaged slices and the damaged-file list ----

/// A bad slice is left out and its files listed; every other file is restored
/// and the run ends INCOMPLETE; nothing is zero-filled.
#[test]
fn a_bad_slice_is_left_out_its_files_are_listed_and_the_rest_restores() {
    let t = two();
    let h = Heir::of(t);
    // The big unit's first slice.
    let slices = t.slice_positions();
    let bad = slices[1];
    h.flip(bad, 200);
    let dest = h.dest("out");
    let o = h.run(&[
        "--restore",
        "--key",
        &key(&h, Who::TenantB),
        "--to",
        dest.to_str().unwrap(),
    ]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(o.stdout.contains("RESTORE INCOMPLETE"), "{}", o.stdout);
    assert!(
        o.stderr.contains("slice(s) of unit")
            && o.stderr.contains("differs from the front index's hash"),
        "{}",
        o.stderr
    );
    let list = std::fs::read_to_string(dest.join(".tapectl-damaged-files.txt")).unwrap();
    let listed: Vec<&str> = list.lines().skip(1).collect();
    assert!(!listed.is_empty(), "{list}");
    let got = heir::tree(&dest);
    let mut good = 0;
    for (path, bytes) in unit_files(t, UNIT_B) {
        if !listed.contains(&escaped(path).as_str()) {
            assert_eq!(got.get(path), Some(bytes), "{path} was restored");
            good += 1;
        }
    }
    assert!(good > 0, "the files of the other slices came back");
    // Every file that came back is right; none is a zero-filled stand-in.
    for (path, have) in &got {
        if let Some((_, bytes)) = unit_files(t, UNIT_B).iter().find(|(p, _)| p == path) {
            if listed.contains(&escaped(path).as_str()) {
                assert_ne!(have, bytes, "{path} is listed and must not be whole");
            }
        }
    }
}

#[test]
fn a_clean_restore_writes_no_list_and_removes_a_stale_one() {
    let t = two();
    let h = Heir::of(t);
    let dest = h.dest("out");
    std::fs::write(dest.join(".tapectl-damaged-files.txt"), "#stale\n").unwrap();
    let o = h.run(&[
        "--restore",
        "--key",
        &key(&h, Who::TenantA),
        "--to",
        dest.to_str().unwrap(),
        "--overwrite",
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(!dest.join(".tapectl-damaged-files.txt").exists());
}

/// D22: a long member's run is fed piece by piece, each checked first. A bad
/// LATER piece ends that tar run; the member's partial file is removed and the
/// member listed; the next tar run starts after the run, and the files in the
/// slices after it still come back.
#[test]
fn a_long_members_bad_piece_ends_its_tar_run_removes_the_partial_file_and_reads_on() {
    let t = two();
    let h = Heir::of(t);
    let large = format!("{UNIT_B}-large.bin");
    // Which slices the long member's run holds: those after the first that
    // state `continues = true`, found through the unit's own manifest.
    let ka = key(&h, Who::TenantB);
    let run = long_run_positions(t, &h, &ka);
    assert!(
        run.len() >= 3,
        "a 150 kB member over 64 kB slices is a run of three: {run:?}"
    );
    let bad = run[1];
    h.flip(bad, 4000);
    let dest = h.dest("out");
    let o = h.run(&["--restore", "--key", &ka, "--to", dest.to_str().unwrap()]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stderr.contains("a long file's run broke at a bad slice"),
        "{}",
        o.stderr
    );
    assert!(!dest.join(&large).exists(), "the partial file was removed");
    let list = std::fs::read_to_string(dest.join(".tapectl-damaged-files.txt")).unwrap();
    assert!(list.lines().any(|l| l == large), "{list}");
    // The next run started after the run: the files after it are whole.
    let got = heir::tree(&dest);
    for name in ["notes.txt", "inner/mid-7.dat"] {
        let (_, bytes) = unit_files(t, UNIT_B)
            .iter()
            .find(|(p, _)| p == name)
            .unwrap();
        assert_eq!(got.get(name), Some(bytes), "{name} after the run");
    }
}

/// The tape positions of the slices of the long member's run in unit B: the
/// slices after the first whose manifest block states `continues = true`.
fn long_run_positions(t: &Tape, h: &Heir, key_file: &str) -> Vec<usize> {
    // Read the manifest the way a heir does: `--list` shows the slice column;
    // the member `-large.bin` begins in slice N and the run follows it. The
    // slice → position map is the plan's: slices lie in order from the first
    // data slice, one unit after the other.
    let o = h.run(&["--list", "--key", key_file, "--unit", UNIT_B]);
    let first_slice: usize = o
        .stdout
        .lines()
        .find(|l| l.contains(&format!("{UNIT_B}-large.bin")))
        .map(|l| l.split_whitespace().nth(1).unwrap().parse().unwrap())
        .unwrap();
    let positions = t.slice_positions();
    let unit_a_slices = t.units.iter().find(|u| u.name == UNIT_A).unwrap().slices;
    let base = positions[unit_a_slices];
    // The member is 150,000 bytes over 65,536-byte slices: three pieces.
    (0..3).map(|i| base + first_slice - 1 + i).collect()
}

// ---- a short tape ----

#[test]
fn a_whole_unit_restores_from_a_short_tape_and_a_partial_one_restores_what_it_holds() {
    let t = short();
    let h = Heir::of(t);
    let ended = t.ended_after();
    let dest = h.dest("a");
    let o = h.run(&[
        "--restore",
        "--key",
        &key(&h, Who::TenantA),
        "--to",
        dest.to_str().unwrap(),
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert_restored(t, UNIT_A, &dest);
    // The big unit has its first slice here and the rest after the end.
    let dest = h.dest("b");
    let o = h.run(&[
        "--restore",
        "--key",
        &key(&h, Who::TenantB),
        "--to",
        dest.to_str().unwrap(),
    ]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(o.stdout.contains("RESTORE INCOMPLETE"), "{}", o.stdout);
    assert!(
        o.stderr
            .contains("is after the end of this tape (it ended after file"),
        "{}",
        o.stderr
    );
    let list = std::fs::read_to_string(dest.join(".tapectl-damaged-files.txt")).unwrap();
    assert!(list.lines().count() > 2, "{list}");
    // R3: no read was sent past the end, whatever the directory held there.
    assert!(
        h.dd_reads().iter().all(|p| *p <= ended + 1),
        "{:?}",
        h.dd_reads()
    );
    // What the first slice held is whole.
    let got = heir::tree(&dest);
    for (path, bytes) in unit_files(t, UNIT_B) {
        if !list.lines().any(|l| l == escaped(path)) {
            assert_eq!(got.get(path), Some(bytes), "{path}");
        }
    }
}

#[test]
fn all_on_a_short_tape_restores_the_whole_unit_and_lists_the_partial_one() {
    let t = short();
    let h = Heir::of(t);
    let dest = h.dest("out");
    let o = h.run(&[
        "--restore",
        "--key",
        &key(&h, Who::Escrow),
        "--to",
        dest.to_str().unwrap(),
        "--all",
    ]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert_restored(t, UNIT_A, &dest.join(UNIT_A));
    assert!(!dest
        .join(UNIT_A)
        .join(".tapectl-damaged-files.txt")
        .exists());
    assert!(dest
        .join(UNIT_B)
        .join(".tapectl-damaged-files.txt")
        .exists());
}

#[test]
fn list_and_find_envelope_on_a_short_tape_read_no_slice() {
    let t = short();
    let h = Heir::of(t);
    let o = h.run(&["--list", "--key", &key(&h, Who::TenantB), "--unit", UNIT_B]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(
        o.stdout.contains(&format!("{UNIT_B}-large.bin")),
        "{}",
        o.stdout
    );
    let slices = t.slices_on_tape();
    assert!(
        h.dd_reads().iter().all(|p| !slices.contains(p)),
        "{:?}",
        h.dd_reads()
    );
}

// ---- compressed units ----

/// Whole-unit `--restore` of a compressed unit for each algorithm carried,
/// with GNU tar and with the real decompressor.
#[test]
fn a_compressed_unit_restores_whole_for_each_algorithm_carried() {
    for alg in ["gzip", "xz", "zstd"] {
        let t = compressed(alg);
        let h = Heir::of(t);
        let o = h.run(&["--find-envelope", "--key", &key(&h, Who::TenantA)]);
        assert!(
            o.stdout.contains(&format!("compressed with {alg}")),
            "{alg}: {}",
            o.stdout
        );
        let dest = h.dest("out");
        let o = h.run(&[
            "--restore",
            "--key",
            &key(&h, Who::TenantA),
            "--to",
            dest.to_str().unwrap(),
        ]);
        assert_eq!(o.code, 0, "{alg}: {}", o.all());
        assert!(o.stdout.contains("RESTORE COMPLETE"), "{alg}: {}", o.stdout);
        assert_restored(t, UNIT_A, &dest);
    }
}

#[test]
fn a_compressed_unit_that_loses_a_file_at_exit_0_is_caught_for_each_algorithm() {
    for alg in ["gzip", "xz", "zstd"] {
        let t = compressed(alg);
        let h = Heir::of(t);
        let real = heir::host_tool("tar").unwrap();
        let victim = "notes.txt";
        h.put_tool("tar", &heir::dropping_tar(&real, victim));
        let dest = h.dest("out");
        let o = h.run(&[
            "--restore",
            "--key",
            &key(&h, Who::TenantA),
            "--to",
            dest.to_str().unwrap(),
        ]);
        assert_eq!(o.code, 1, "{alg}: {}", o.all());
        let list = std::fs::read_to_string(dest.join(".tapectl-damaged-files.txt")).unwrap();
        assert!(list.lines().any(|l| l == victim), "{alg}: {list}");
    }
}

/// A decompressor's exit status is given no weight: one that exits 0 after
/// writing too little (BusyBox `unxz` does) is caught by the slice's length.
#[test]
fn a_decompressor_that_exits_0_after_short_output_is_caught_by_the_slice_length() {
    let t = compressed("xz");
    let h = Heir::of(t);
    h.put_tool("xz", "#!/bin/sh\nhead -c 100\nexit 0\n");
    let dest = h.dest("out");
    let o = h.run(&[
        "--restore",
        "--key",
        &key(&h, Who::TenantA),
        "--to",
        dest.to_str().unwrap(),
    ]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stderr
            .contains("its decoded length is 100 bytes and the manifest says")
            && o.stderr.contains("(decompressed with xz)"),
        "{}",
        o.stderr
    );
    assert!(o.stdout.contains("RESTORE INCOMPLETE"), "{}", o.stdout);
}

// ---- the low findings of #525 and the rest of the traps ----

/// A2: the script keeps no process id to kill at the end.
#[test]
fn the_script_never_kills_a_process_id_it_remembers() {
    let text = tapectl::volume::heir::RESTORE_SH;
    assert!(!text.contains("BG_PIDS"), "no list of process ids");
    assert!(!text.contains("kill -KILL"), "no SIGKILL at exit");
    assert!(
        !text.contains("mkfifo"),
        "no FIFO to read, or to fail on exFAT"
    );
}

/// A8: a `df` that exits non-zero ends no mode silently.
#[test]
fn a_failing_df_is_warned_about_and_the_restore_goes_on() {
    let t = two();
    let h = Heir::of(t);
    h.put_tool("df", "#!/bin/sh\nexit 1\n");
    let dest = h.dest("out");
    let o = h.run(&[
        "--restore",
        "--key",
        &key(&h, Who::TenantA),
        "--to",
        dest.to_str().unwrap(),
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(
        o.stderr.contains("cannot measure the free space"),
        "{}",
        o.stderr
    );
    assert_restored(t, UNIT_A, &dest);
}

#[test]
fn the_space_check_refuses_before_any_slice_is_read_and_no_space_check_skips_it() {
    let t = two();
    let h = Heir::of(t);
    h.put_tool(
        "df",
        "#!/bin/sh\necho 'Filesystem 1024-blocks Used Available Capacity Mounted on'\necho '/dev/x 100 0 7 0% /'\n",
    );
    let dest = h.dest("out");
    let ka = key(&h, Who::TenantA);
    let o = h.run(&["--restore", "--key", &ka, "--to", dest.to_str().unwrap()]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stderr.contains("bytes free and this needs about")
            && o.stderr.contains("--no-space-check skips this check"),
        "{}",
        o.stderr
    );
    let slices = t.slice_positions();
    assert!(
        h.dd_reads().iter().all(|p| !slices.contains(p)),
        "no slice was read"
    );
    let o = h.run(&[
        "--restore",
        "--key",
        &ka,
        "--to",
        dest.to_str().unwrap(),
        "--no-space-check",
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
}

/// The file check reads names a `sha256sum` quotes with a backslash (GNU's
/// `\name`), and a unit with a backslash in a file name restores and checks.
#[test]
fn a_sha256sum_that_quotes_names_and_a_name_with_a_backslash_check_out() {
    let t = two();
    let h = Heir::of(t);
    let real = heir::host_tool("sha256sum").unwrap();
    h.put_tool(
        "sha256sum",
        &format!(
            "#!/bin/bash\n{real} \"$@\" | while IFS= read -r l; do\n  case \"$l\" in\n    *'\\\\'*) printf '\\\\%s\\n' \"$l\" ;;\n    *) printf '%s\\n' \"$l\" ;;\n  esac\ndone\nexit ${{PIPESTATUS[0]}}\n",
            real = real.display()
        ),
    );
    let dest = h.dest("out");
    let o = h.run(&[
        "--restore",
        "--key",
        &key(&h, Who::TenantB),
        "--to",
        dest.to_str().unwrap(),
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(o.stdout.contains("RESTORE COMPLETE"), "{}", o.stdout);
    assert!(dest.join("odd\\back name.txt").exists());
    assert_restored(t, UNIT_B, &dest);
    // And the file check does see a damaged file with such a name.
    let h = Heir::of(t);
    let real_tar = heir::host_tool("tar").unwrap();
    h.put_tool("tar", &heir::dropping_tar(&real_tar, "odd\\back name.txt"));
    let dest = h.dest("out");
    let o = h.run(&[
        "--restore",
        "--key",
        &key(&h, Who::TenantB),
        "--to",
        dest.to_str().unwrap(),
    ]);
    assert_eq!(o.code, 1, "{}", o.all());
    let list = std::fs::read_to_string(dest.join(".tapectl-damaged-files.txt")).unwrap();
    assert!(
        list.lines().any(|l| l == "odd\\\\back name.txt"),
        "escaped as the index escapes: {list}"
    );
}

/// A failed read forces a rewind before the next one, so a read never lands
/// on the wrong file; a status that disagrees with the head is not trusted.
#[test]
fn a_failed_read_forces_a_rewind_and_a_status_that_lies_is_not_trusted() {
    let t = two();
    let h = Heir::of(t);
    let slices = t.slice_positions();
    let o = h.run_with(&["--verify"], &[("FAKE_DD_FAIL", &slices[0].to_string())]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stdout
            .contains(&format!("FAIL  {:4}  data_slice", slices[0]))
            && o.stdout
                .contains(&format!("PASS  {:4}  data_slice", slices[1])),
        "the file after a failed read is still the right file: {}",
        o.stdout
    );
    assert!(h.count_ops("mt rewind") >= 2, "{:?}", h.ops());
    let h = Heir::of(t);
    let o = h.run_with(&["--verify"], &[("FAKE_MT_STATUS_LIE", "1")]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(o.stderr.contains("rewinding to be sure"), "{}", o.stderr);
}

/// The label is shown as it is, with the characters a label may hold.
#[test]
fn the_labels_characters_come_through_the_messages() {
    let t = two();
    let h = Heir::of(t);
    let o = h.run(&["--info"]);
    assert!(
        o.stdout.contains(&format!(
            "Tape: {LABEL}   uuid a1b2c3d4-e5f6-4789-a123-456789abcdef"
        )),
        "{}",
        o.stdout
    );
    let _ = (TENANT_A, TENANT_B);
}

// ---- the id map, the list, the signals, and a check that checked nothing ----

/// §7.4: after a restore it prints the damaged-file list, the id map of each
/// unit restored (never acted on), and which tar it used.
#[test]
fn a_restore_prints_the_id_map_the_damaged_list_and_the_tar_it_used() {
    use std::os::unix::fs::MetadataExt;
    let t = two();
    let h = Heir::of(t);
    let uid = std::fs::metadata(".").unwrap().uid();
    let dest = h.dest("out");
    let o = h.run(&[
        "--restore",
        "--key",
        &key(&h, Who::TenantB),
        "--to",
        dest.to_str().unwrap(),
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(
        o.stdout.contains("Who the ids were") && o.stdout.contains("never remaps them by name"),
        "{}",
        o.stdout
    );
    assert!(
        o.stdout.contains(&format!("  user  {uid}  ")),
        "the id map names the owner of the made files ({uid}): {}",
        o.stdout
    );
    assert!(
        o.stdout.contains("The tar used: tar (GNU tar)")
            && o.stdout.contains("What it does not restore:"),
        "{}",
        o.stdout
    );
    // A restore with a damaged slice prints the list itself, not only where it is.
    let h = Heir::of(t);
    h.flip(t.slice_positions()[2], 500);
    let dest = h.dest("out");
    let o = h.run(&[
        "--restore",
        "--key",
        &key(&h, Who::TenantB),
        "--to",
        dest.to_str().unwrap(),
    ]);
    assert_eq!(o.code, 1, "{}", o.all());
    let list = std::fs::read_to_string(dest.join(".tapectl-damaged-files.txt")).unwrap();
    for path in list.lines().skip(1) {
        assert!(
            o.stdout.contains(&format!("  {path}")),
            "{path}: {}",
            o.stdout
        );
    }
}

/// #525 A9: a file check that checked nothing never says the files match. A
/// `sha256sum` that prints nothing and exits 0 leaves every file damaged.
#[test]
fn a_file_check_that_checked_nothing_does_not_say_the_files_match() {
    let t = two();
    let h = Heir::of(t);
    let real = heir::host_tool("sha256sum").unwrap();
    // Silent on `-c`, itself otherwise (the script hashes slices and envelopes
    // with it too).
    h.put_tool(
        "sha256sum",
        &format!(
            "#!/bin/bash\nfor a; do if [ \"$a\" = -c ]; then exit 0; fi; done\nexec {} \"$@\"\n",
            real.display()
        ),
    );
    let dest = h.dest("out");
    let o = h.run(&[
        "--restore",
        "--key",
        &key(&h, Who::TenantA),
        "--to",
        dest.to_str().unwrap(),
    ]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(!o.stdout.contains("match the sha256"), "{}", o.stdout);
    assert!(o.stdout.contains("RESTORE INCOMPLETE"), "{}", o.stdout);
    let list = std::fs::read_to_string(dest.join(".tapectl-damaged-files.txt")).unwrap();
    assert!(list.lines().count() > 3, "{list}");
}

/// Ctrl-C, a hangup or a kill leave no work directory behind.
#[test]
fn a_kill_leaves_no_work_directory_behind() {
    let t = two();
    let h = Heir::of(t);
    let real = heir::host_tool("tar").unwrap();
    // A tar that takes its time on the first envelope.
    h.put_tool(
        "tar",
        &format!(
            "#!/bin/bash\nfor a; do case \"$a\" in --version) exec {real} \"$a\" ;; esac; done\nsleep 4\nexec {real} \"$@\"\n",
            real = real.display()
        ),
    );
    let dest = h.dest("out");
    let ka = key(&h, Who::TenantA);
    let mut c = h.command(&["--restore", "--key", &ka, "--to", dest.to_str().unwrap()]);
    let mut child = c.spawn().unwrap();
    let work_seen = (0..100).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(100));
        std::fs::read_dir(&dest).unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".tapectl-work")
        })
    });
    assert!(work_seen, "the work directory is made inside --to");
    let killed = std::process::Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let status = child.wait().unwrap();
    assert_ne!(status.code(), Some(0));
    assert!(
        std::fs::read_dir(&dest).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".tapectl-work")),
        "no work directory is left in --to"
    );
}

/// With no seal the end of the tape is not known: a slice that is not there
/// is a bad slice (R2), never a hang, and what is there restores.
#[test]
fn a_restore_from_an_unsealed_tape_leaves_out_the_slices_that_are_not_there() {
    let t = two();
    let h = Heir::of(t);
    // The tape stops inside the big unit's slices, with no seal.
    let slices = t.slice_positions();
    h.cut_after(slices[3] + 1);
    let dest = h.dest("out");
    let o = h.run(&[
        "--restore",
        "--key",
        &key(&h, Who::TenantB),
        "--to",
        dest.to_str().unwrap(),
    ]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(o.stderr.contains("read as unsealed"), "{}", o.stderr);
    assert!(o.stdout.contains("RESTORE INCOMPLETE"), "{}", o.stdout);
    let list = std::fs::read_to_string(dest.join(".tapectl-damaged-files.txt")).unwrap();
    assert!(list.lines().count() > 2, "{list}");
}

/// A key may be given more than once: each is offered to age together, for
/// the envelope and for every slice; a key file that is missing is named
/// before the tape is read.
#[test]
fn a_key_may_be_repeated_and_each_is_offered_to_age() {
    let t = two();
    let h = Heir::of(t);
    let stranger = tempfile_key();
    let dest = h.dest("out");
    let o = h.run(&[
        "--restore",
        "--key",
        stranger.to_str().unwrap(),
        "--key",
        &key(&h, Who::TenantA),
        "--to",
        dest.to_str().unwrap(),
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert_restored(t, UNIT_A, &dest);
    let h = Heir::of(t);
    let o = h.run(&[
        "--restore",
        "--key",
        &key(&h, Who::TenantA),
        "--key",
        "/no/such/second/key",
        "--to",
        "/nowhere",
    ]);
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("/no/such/second/key"), "{}", o.stderr);
    assert!(h.ops().is_empty(), "named before the tape is read");
}

/// Rung 2 of the degradation ladder: a front index that is gone is replaced
/// by the seal marker's own copy of it, loudly, and the restore goes on from
/// that map (every file is still checked against its hash).
#[test]
fn a_damaged_front_index_falls_back_to_the_seals_copy_and_restores() {
    let t = two();
    let h = Heir::of(t);
    h.replace(3, b"this is not a front index");
    let o = h.run(&["--info"]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(
        o.stderr
            .contains("falling back to the seal marker's own copy of it"),
        "{}",
        o.stderr
    );
    assert!(
        o.stdout
            .contains("The map is read from the the seal marker's copy")
            || o.stdout
                .contains("the seal marker's copy of the front index"),
        "{}",
        o.stdout
    );
    let dest = h.dest("out");
    let o = h.run(&[
        "--restore",
        "--key",
        &key(&h, Who::TenantA),
        "--to",
        dest.to_str().unwrap(),
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert_restored(t, UNIT_A, &dest);
    // With the seal gone as well there is no map at all, and the script says
    // how to go on by hand.
    let h = Heir::of(t);
    h.replace(3, b"nothing");
    h.cut_after(t.files.len() - 1);
    let o = h.run(&["--info"]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stderr.contains("there is no map of the tape") && o.stderr.contains("fsf N"),
        "{}",
        o.stderr
    );
}

// ---- a seal that disagrees with the front index; the gaps of the ledger ----

/// §7.2 step 3 with R2: a seal that parses but does not bind File 3 reads as
/// UNSEALED. Its `ended_after` is not used, and the two ends are said to
/// disagree, naming both hashes. Here the full tape's seal is replaced by the
/// short tape's (a seal that parses, ending after slice 1, binding another
/// File 3); every slice is on the directory tape, past that `ended_after`
/// too, and the restore reads them.
#[test]
fn a_seal_that_does_not_bind_file_3_reads_as_unsealed_and_its_ended_after_is_not_used() {
    let t = two();
    let other = short();
    let h = Heir::of(t);
    let last = t.files.len() - 1;
    h.replace(last, other.files.last().unwrap());
    let ended_by_seal = other.ended_after();
    assert!(
        t.slice_positions().iter().any(|p| *p > ended_by_seal),
        "a slice lies past the seal's ended_after"
    );
    let dest = h.dest("out");
    let o = h.run(&[
        "--restore",
        "--key",
        &key(&h, Who::TenantB),
        "--to",
        dest.to_str().unwrap(),
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(o.stdout.contains("RESTORE COMPLETE"), "{}", o.all());
    assert_restored(t, UNIT_B, &dest);
    assert!(
        o.stderr
            .matches("the two ends of the tape disagree")
            .count()
            == 1
            && o.stderr.contains("read as unsealed")
            && o.stderr.contains("the seal's ended_after is not used"),
        "{}",
        o.stderr
    );
    // Both hashes are named: the seal's, and File 3's.
    let seal = String::from_utf8_lossy(other.files.last().unwrap()).into_owned();
    let bound = seal
        .lines()
        .find_map(|l| l.strip_prefix("front_index_sha256 = \""))
        .unwrap()
        .trim_end_matches('"')
        .to_string();
    assert!(o.stderr.contains(&bound), "{}", o.stderr);
    let o = h.run(&["--info"]);
    assert!(o.stdout.contains("present but NOT TRUSTED"), "{}", o.stdout);
}

fn awk_program(name: &str) -> String {
    let text = tapectl::volume::heir::RESTORE_SH;
    let start = text.find(&format!("\n{name}='")).unwrap() + name.len() + 3;
    let end = text[start..].find("\n'\n").unwrap() + start;
    text[start..end].to_string()
}

/// A key a unit's block does not own, written in another table (`[manifest]`,
/// a slice's table), never retargets the selector: each awk reads `name`,
/// `uuid`, `stage_set_id` and the rest only in the `[[units]]` head, and a
/// slice key only in the slice's own table.
#[test]
fn a_key_outside_the_units_own_table_does_not_retarget_the_manifest_selector() {
    let manifest = r#"[manifest]
volume = "V"
name = "decoy-manifest"
uuid = "decoy-uuid"

[[units]]
name = "alpha"
uuid = "uuid-alpha"
snapshot_version = 1
stage_set_id = 7
payload = "tar"
stream_bytes = 100
file_table = "tables/a.tsv"
id_map = "tables/a.ids.tsv"

[[units.slices]]
number = 1
name = "decoy-slice"
stage_set_id = 99
tape_position = 5
size_bytes = 60
encrypted_bytes = 80
sha256_plain = "pp1"
sha256_encrypted = "ee1"

[[units.slices]]
number = 2
tape_position = 6
size_bytes = 40
encrypted_bytes = 60
sha256_plain = "pp2"
sha256_encrypted = "ee2"
continues = true

[[units]]
name = "beta"
uuid = "uuid-beta"
snapshot_version = 2
stage_set_id = 8
payload = "tar"
stream_bytes = 10
file_table = "tables/b.tsv"
compression = "gzip"

[[units.slices]]
number = 1
size_bytes = 10
encrypted_bytes = 30
sha256_plain = "pb1"
sha256_encrypted = "eb1"
"#;
    let units_prog = awk_program("AWK_UNITS");
    let slices_prog = awk_program("AWK_SLICES");
    let dir = tempfile::tempdir().unwrap();
    let m = dir.path().join("MANIFEST.toml");
    std::fs::write(&m, manifest).unwrap();
    let mut ran = 0;
    for (name, body) in awks() {
        let wrapper = dir.path().join("awk-under-test");
        std::fs::write(&wrapper, format!("#!/bin/sh\n{body}")).unwrap();
        std::fs::set_permissions(
            &wrapper,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        let units = std::process::Command::new(&wrapper)
            .args(["-v", "pos=4", &units_prog])
            .arg(&m)
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&units.stdout),
            "4|1|alpha|1|7|uuid-alpha||100|tables/a.tsv|tables/a.ids.tsv|2\n\
             4|2|beta|2|8|uuid-beta|gzip|10|tables/b.tsv||1\n",
            "{name}"
        );
        let slices = std::process::Command::new(&wrapper)
            .args(["-v", "want=1", &slices_prog])
            .arg(&m)
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&slices.stdout),
            "1|5|60|80|pp1|ee1|\n2|6|40|60|pp2|ee2|true\n",
            "{name}"
        );
        let slices = std::process::Command::new(&wrapper)
            .args(["-v", "want=2", &slices_prog])
            .arg(&m)
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&slices.stdout),
            "1||10|30|pb1|eb1|\n",
            "{name}"
        );
        ran += 1;
    }
    assert!(ran > 0, "at least one awk ran");
}

/// A unit that two envelopes list (the escrow key opens the operator's and
/// the tenant's): `--find-envelope` shows it in both, and `--list` and
/// `--restore` pick ONE block: the one with the file table, the tenant
/// envelope's, version 1 (the only version staged).
#[test]
fn a_unit_listed_by_two_envelopes_is_picked_once_from_the_one_with_the_file_table() {
    let t = two();
    let h = Heir::of(t);
    let esc = key(&h, Who::Escrow);
    let o = h.run(&["--find-envelope", "--key", &esc]);
    assert_eq!(o.code, 0, "{}", o.all());
    let rows = o
        .stdout
        .lines()
        .filter(|l| l.contains(&format!("  {UNIT_A}  version 1")))
        .count();
    assert_eq!(
        rows, 2,
        "the operator's and the tenant's envelope: {}",
        o.stdout
    );
    let o = h.run(&["--list", "--key", &esc, "--unit", UNIT_A]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert_eq!(
        o.stdout
            .matches(&format!("Unit {UNIT_A}  version 1"))
            .count(),
        1,
        "{}",
        o.stdout
    );
    assert!(
        o.stdout.contains(&format!("{UNIT_A}-small.txt")),
        "{}",
        o.stdout
    );
    let dest = h.dest("out");
    let o = h.run(&[
        "--restore",
        "--key",
        &esc,
        "--to",
        dest.to_str().unwrap(),
        "--unit",
        UNIT_A,
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(o.stdout.contains("version 1: "), "{}", o.stdout);
    assert_restored(t, UNIT_A, &dest);
}

/// The space check splits its need across two file systems: the files are
/// counted against `--to`'s and one slice twice against `--work`'s; on one
/// file system the two add. `df` is a stand-in that names a device for each
/// path and gives each 800 KiB.
#[test]
fn the_space_need_is_split_across_two_file_systems_and_added_on_one() {
    let t = two();
    let df = |h: &Heir| {
        h.put_tool(
            "df",
            "#!/bin/sh\nfor a; do last=$a; done\ncase \"$last\" in\n  *workdir*) dev=/dev/work ;;\n  *) dev=/dev/dest ;;\nesac\necho 'Filesystem 1024-blocks Used Available Capacity Mounted on'\necho \"$dev 999999 0 800 0% /\"\n",
        );
    };
    // Two file systems: each need fits its own.
    let h = Heir::of(t);
    df(&h);
    let work = h.dest("workdir");
    let dest = h.dest("out");
    let o = h.run(&[
        "--restore",
        "--key",
        &key(&h, Who::TenantB),
        "--to",
        dest.to_str().unwrap(),
        "--work",
        work.to_str().unwrap(),
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    // One file system (the work directory is inside --to): the needs add up
    // past what it has.
    let h = Heir::of(t);
    df(&h);
    let dest = h.dest("out");
    let o = h.run(&[
        "--restore",
        "--key",
        &key(&h, Who::TenantB),
        "--to",
        dest.to_str().unwrap(),
    ]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stderr.contains("bytes free and this needs about"),
        "{}",
        o.stderr
    );
}
