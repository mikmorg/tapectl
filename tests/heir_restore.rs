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

/// A path as the index writes it (§1.5): a backslash is `\\\\`, a TAB `\\t`,
/// a newline `\\n`, a carriage return `\\r`.
fn escaped(path: &str) -> String {
    path.replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
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
        // The named paths, the index modes: the same words under every awk.
        let one = h.dest("one");
        let deep = format!("inner/{UNIT_A}-deep.dat");
        let p = h.run(&[
            "--restore",
            "--key",
            &ka,
            "--to",
            one.to_str().unwrap(),
            "--unit",
            UNIT_A,
            "--path",
            &deep,
        ]);
        assert_eq!(p.code, 0, "{name}: {}", p.all());
        assert!(one.join(&deep).is_file(), "{name}");
        let hh = Heir::of(with_holders());
        put_awk(&hh, body);
        let kh = key(&hh, Who::TenantA);
        let ke = key(&hh, Who::Escrow);
        let mut index = String::new();
        for args in [
            vec!["--where", UNIT_A, "--key", &kh],
            vec!["--where", "--all", "--key", &ke],
            vec!["--find", "deep", "--key", &ke],
            vec!["--where", "nothing", "--key", &kh],
        ] {
            let o = hh.run(&args);
            assert!(o.code <= 1, "{name} {args:?}: {}", o.all());
            index.push_str(&o.stdout);
        }
        outputs.push((
            name.to_string(),
            format!(
                "{}\n{}\n{}\n{}\n{}",
                info.stdout, list.stdout, find.stdout, p.stdout, index
            ),
        ));
    }
    // The words each mode printed are the same under every awk, but for the
    // work directory's random name.
    let norm = |s: &str| {
        s.lines()
            .filter(|l| {
                !l.contains("tapectl-work")
                    && !l.contains("To restore, run:")
                    && !l.contains("into /")
                    && !l.contains("/RESTORE.sh --")
                    && !l.contains("written 20")
            })
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

/// Layout 2's spellings for a second cartridge are gone (item 255, §9): each
/// is refused by name, before the tape moves, with the Layout 3 way
/// (`--paths-from`, and the prompt a unit across cartridges asks at);
/// `--scratch` is spelled `--work`.
#[test]
fn the_removed_spellings_are_refused_by_name_before_the_tape_moves() {
    let h = Heir::of(two());
    let ka = key(&h, Who::TenantA);
    for args in [
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
        vec!["--restore", "--key", &ka, "--to", "/x", "--stream-tapes"],
        vec!["--restore", "--key", &ka, "--to", "/x", "--fill-from", "/y"],
    ] {
        let o = h.run(&args);
        assert_eq!(o.code, 2, "{args:?}: {}", o.all());
        assert!(
            o.stderr.contains("is not in this RESTORE.sh")
                && o.stderr.contains("--paths-from")
                && o.stderr.contains("prompt"),
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
            && o.stderr.contains("Layout 3 only"),
        "{}",
        o.stderr
    );
    // This is a Layout 3 tape whose File 0 alone was changed, so its seal
    // marker still states Layout 3 and no feature. The refusal stays, and
    // says so: File 0 may be the damaged one, and the RESTORE.sh in this
    // tape's File 2, the script that is running, is not the way on (ADR-0012
    // item 313, DI-9). A tape whose seal marker does not vouch keeps the old
    // way on: `tests/heir_restore_s6.rs`.
    assert!(
        o.stderr.contains(
            "the seal marker at the end of this tape states layout_version = 3 and requires no feature"
        ) && o.stderr.contains("File 0 may be damaged")
            && o.stderr.contains("by the steps of File 1")
            && !o.stderr.contains("RESTORE.sh in its own File 2"),
        "{}",
        o.stderr
    );
    // Nothing but File 0 and the seal marker was read.
    assert_eq!(h.dd_reads(), vec![0, t.files.len() - 1]);
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

/// #596 C-2: an unsealed tape's last file is a data slice, up to GiBs. The
/// seal read of it is capped at 64 MiB, four times what the seal is allowed
/// (SEAL_ALLOWANCE_BYTES, 16 MiB; volume-format-v3.md 5.2 W8): `dd` is asked
/// for no more blocks than that, and the stripped copy is made from that read
/// alone.
#[test]
fn the_seal_read_of_an_unsealed_tape_is_capped() {
    let t = two();
    let h = Heir::of(t);
    let last = t.files.len() - 1;
    h.cut_after(last);
    // The last file is now a slice of 20 MiB (a real one is GiBs).
    h.replace(last - 1, &vec![b'A'; 20 * 1024 * 1024]);
    let o = h.run(&["--info"]);
    assert!(o.stderr.contains("read as unsealed"), "{}", o.all());
    let counts = std::fs::read_to_string(h.root.path().join("state/dd.counts"))
        .unwrap_or_else(|_| panic!("the seal read was not capped: {}", o.all()));
    let asked: Vec<usize> = counts
        .lines()
        .filter_map(|l| l.strip_prefix(&format!("{} ", last - 1)))
        .map(|n| n.parse().unwrap())
        .collect();
    assert!(
        !asked.is_empty(),
        "no capped read of the last file: {counts}"
    );
    for n in asked {
        assert!(
            n * 524_288 <= 64 * 1024 * 1024,
            "{n} blocks is more than a seal can be"
        );
    }
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
        let shown = escaped(path);
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

// ---- each verdict on a slice, by its words ----
//
// A slice is judged by the front index's hash of the file as read, then by
// the manifest's length and hash of its plain cut; a work directory that
// cannot take it ends the run and is never called damage. Each verdict is
// held here by its own words, whatever reads and hashes the bytes.

/// An `age` that is the stand-in for every file but a slice (its input is
/// `slice.enc`), where it runs `body`, with the stand-in as `$REAL`.
fn age_on_slices(h: &Heir, body: &str) {
    h.put_tool(
        "age",
        &format!(
            "#!/bin/bash\nREAL={real}\ncase \"$(ls -l /proc/$$/fd/0 2>/dev/null)\" in\n  *slice.enc) ;;\n  *) exec \"$REAL\" \"$@\" ;;\nesac\n{body}\n",
            real = heir::heir_age_binary().display()
        ),
    );
}

fn restore_unit_b(h: &Heir) -> (heir::Out, std::path::PathBuf) {
    let dest = h.dest("out");
    let o = h.run(&[
        "--restore",
        "--key",
        &key(h, Who::TenantB),
        "--to",
        dest.to_str().unwrap(),
    ]);
    (o, dest)
}

/// A slice the drive cannot read is left out as that, not as damage.
#[test]
fn a_slice_the_tape_cannot_give_is_left_out_as_unreadable() {
    let t = two();
    let h = Heir::of(t);
    let bad = t.slice_positions()[1];
    let dest = h.dest("out");
    let o = h.run_with(
        &[
            "--restore",
            "--key",
            &key(&h, Who::TenantB),
            "--to",
            dest.to_str().unwrap(),
        ],
        &[("FAKE_DD_FAIL", &bad.to_string())],
    );
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(o.stdout.contains("RESTORE INCOMPLETE"), "{}", o.stdout);
    assert!(
        o.stderr
            .contains(&format!("the tape could not be read at file {bad}")),
        "{}",
        o.stderr
    );
    assert!(
        !o.stderr.contains("differs from the front index"),
        "{}",
        o.stderr
    );
}

/// A tape file shorter than the front index says is judged by its hash: the
/// bytes read are not the bytes written.
#[test]
fn a_slice_shorter_on_the_tape_than_written_differs_from_the_front_indexs_hash() {
    let t = two();
    let h = Heir::of(t);
    let bad = t.slice_positions()[1];
    let p = h.file(bad);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&p)
        .unwrap()
        .set_len((t.size_of(bad) - 1000) as u64)
        .unwrap();
    let (o, _) = restore_unit_b(&h);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(o.stdout.contains("RESTORE INCOMPLETE"), "{}", o.stdout);
    assert!(
        o.stderr
            .contains(&format!("file {bad} differs from the front index's hash")),
        "{}",
        o.stderr
    );
}

/// A plain cut of the right length and the wrong bytes is caught by the
/// manifest's hash: from age itself, and from a decompressor.
#[test]
fn a_plain_cut_of_the_right_length_and_other_bytes_is_caught_by_its_hash() {
    let t = two();
    let h = Heir::of(t);
    age_on_slices(
        &h,
        "\"$REAL\" \"$@\" | tr '\\000' '\\001'\nexit ${PIPESTATUS[0]}",
    );
    let (o, _) = restore_unit_b(&h);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(o.stdout.contains("RESTORE INCOMPLETE"), "{}", o.stdout);
    assert!(
        o.stderr
            .contains("its plain cut does not match the manifest's sha256"),
        "{}",
        o.stderr
    );
    assert!(!o.stderr.contains("decoded length"), "{}", o.stderr);

    let t = compressed("xz");
    let h = Heir::of(t);
    let real = heir::host_tool("xz").unwrap();
    h.put_tool(
        "xz",
        &format!(
            "#!/bin/bash\n{real} \"$@\" | tr '\\000' '\\001'\nexit ${{PIPESTATUS[0]}}\n",
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
            .contains("its plain cut does not match the manifest's sha256"),
        "{}",
        o.stderr
    );
}

/// age's own failure on a slice is named with its words; a key that opens
/// the envelope and not the slice is told so.
#[test]
fn age_failing_on_a_slice_is_named_and_a_key_that_opens_no_slice_is_told_so() {
    let t = two();
    let h = Heir::of(t);
    age_on_slices(
        &h,
        "cat >/dev/null\necho 'age: error: failed to decrypt: chunk 2 is not authentic' >&2\nexit 1",
    );
    let (o, _) = restore_unit_b(&h);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(o.stdout.contains("RESTORE INCOMPLETE"), "{}", o.stdout);
    assert!(
        o.stderr.contains(
            "age could not decrypt it: age: error: failed to decrypt: chunk 2 is not authentic"
        ),
        "{}",
        o.stderr
    );

    let h = Heir::of(t);
    age_on_slices(
        &h,
        "cat >/dev/null\necho \"age: error: no identity matched any of the file's recipients\" >&2\nexit 1",
    );
    let (o, _) = restore_unit_b(&h);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stderr
            .contains("no key given opens it (a slice can be sealed to an older key"),
        "{}",
        o.stderr
    );
}

/// A work directory that fills while a slice is decrypted (or decompressed)
/// ends the run, saying so; the slice is not called damaged.
#[test]
fn a_full_work_directory_while_a_slice_is_decoded_ends_the_run_and_is_not_damage() {
    let t = two();
    let h = Heir::of(t);
    age_on_slices(
        &h,
        "cat >/dev/null\necho 'age: error: failed to write output: No space left on device' >&2\nexit 1",
    );
    let (o, _) = restore_unit_b(&h);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stderr.contains("FATAL: writing slice")
            && o.stderr.contains("is full or not writable")
            && o.stderr.contains("--work DIR"),
        "{}",
        o.stderr
    );
    assert!(!o.stderr.contains("could not decrypt"), "{}", o.stderr);
    assert!(!o.stdout.contains("RESTORE COMPLETE"), "{}", o.stdout);

    let t = compressed("xz");
    let h = Heir::of(t);
    h.put_tool(
        "xz",
        "#!/bin/sh\ncat >/dev/null\necho 'xz: (stdout): Write error: No space left on device' >&2\nexit 1\n",
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
        o.stderr.contains("FATAL: writing slice") && o.stderr.contains("is full or not writable"),
        "{}",
        o.stderr
    );
    assert!(!o.stderr.contains("decoded length"), "{}", o.stderr);
}

/// A work directory that fills while a slice comes off the tape ends the
/// run, saying so; the slice is neither unreadable nor damaged.
#[test]
fn a_full_work_directory_while_a_slice_is_read_ends_the_run_and_is_not_damage() {
    let t = two();
    let h = Heir::of(t);
    let bad = t.slice_positions()[1];
    let real_head = heir::host_tool("head").unwrap();
    h.put_tool(
        "head",
        &format!(
            "#!/bin/bash\ncase \"$*\" in\n  \"-c {size}\")\n    cat >/dev/null\n    echo \"head: error writing 'standard output': No space left on device\" >&2\n    exit 1 ;;\nesac\nexec {real} \"$@\"\n",
            size = t.size_of(bad),
            real = real_head.display()
        ),
    );
    let (o, _) = restore_unit_b(&h);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stderr.contains(&format!(
            "reading tape file {bad} into the work directory failed"
        )) && o
            .stderr
            .contains("cannot take it: it is full, or not writable"),
        "{}",
        o.stderr
    );
    for bad_words in ["could not be read at file", "differs from the front index"] {
        assert!(!o.stderr.contains(bad_words), "{bad_words}: {}", o.stderr);
    }
}

/// A `tee` that is the host's for every file but one whose name ends in
/// `name`: that copy it cuts to its first 1000 bytes while it passes every
/// byte on, as a disk that fills mid-file leaves GNU tee doing. `loud`, it
/// says so as a full disk does and exits 1; silent, it exits 0. It leaves
/// `tee.cut` in the state directory (`$FAKE_TAPE`) when it cut a copy: the
/// script's TMPDIR is its work directory.
fn short_tee(h: &Heir, name: &str, loud: bool) {
    let real = heir::host_tool("tee").unwrap();
    let end = if loud {
        "echo \"tee: $1: No space left on device\" >&2\nexit 1"
    } else {
        "exit 0"
    };
    h.put_tool(
        "tee",
        &format!(
            "#!/bin/bash\ncase \"$1\" in\n  *{name}) ;;\n  *) exec {real} \"$@\" ;;\nesac\n: >\"$FAKE_TAPE/tee.cut\"\ncat >\"$FAKE_TAPE/tee.all\"\nhead -c 1000 \"$FAKE_TAPE/tee.all\" >\"$1\"\ncat \"$FAKE_TAPE/tee.all\"\nrm -f \"$FAKE_TAPE/tee.all\"\n{end}\n",
            real = real.display()
        ),
    );
}

/// The encrypted slice is hashed as it streams off the tape into the work
/// directory, so a copy cut short while every byte streamed whole would pass
/// the hash: it is caught by its length, or by tee's words, and said to be
/// the work directory's, never the tape's.
#[test]
fn a_copy_of_a_slice_cut_short_while_its_bytes_stream_whole_is_the_work_directorys() {
    let t = two();
    let bad = t.slice_positions()[1];
    for loud in [true, false] {
        let h = Heir::of(t);
        short_tee(&h, "slice.enc", loud);
        let (o, _) = restore_unit_b(&h);
        assert!(
            h.root.path().join("state/tee.cut").exists(),
            "loud={loud}: the copy went through tee: {}",
            o.all()
        );
        assert_eq!(o.code, 1, "loud={loud}: {}", o.all());
        assert!(
            o.stderr
                .contains("cannot take it: it is full, or not writable"),
            "loud={loud}: {}",
            o.stderr
        );
        if loud {
            assert!(
                o.stderr.contains(&format!(
                    "reading tape file {bad} into the work directory failed"
                )),
                "{}",
                o.stderr
            );
        } else {
            assert!(
                o.stderr
                    .contains("into the work directory failed: the copy holds 1000 of the")
                    && o.stderr
                        .contains(&format!("bytes read from tape file {bad}")),
                "{}",
                o.stderr
            );
        }
        for bad_words in [
            "could not be read at file",
            "differs from the front index",
            "could not decrypt",
            "RESTORE COMPLETE",
        ] {
            assert!(
                !o.all().contains(bad_words),
                "loud={loud} {bad_words}: {}",
                o.all()
            );
        }
    }
}

/// The plain cut is hashed as it streams into the work directory too: a copy
/// cut short while every byte streamed whole is caught by tee's words (a
/// full work directory, which ends the run) or by its length (the decoded
/// length the manifest states), for a plain unit and a compressed one. It
/// never reaches tar as a slice that passed.
#[test]
fn a_plain_cut_cut_short_while_its_bytes_stream_whole_is_caught() {
    for alg in [None, Some("xz")] {
        for loud in [true, false] {
            let (t, who) = match alg {
                None => (two(), Who::TenantB),
                Some(a) => (compressed(a), Who::TenantA),
            };
            let h = Heir::of(t);
            short_tee(&h, "slice.plain", loud);
            let dest = h.dest("out");
            let o = h.run(&[
                "--restore",
                "--key",
                &key(&h, who),
                "--to",
                dest.to_str().unwrap(),
            ]);
            let all = o.all();
            assert!(
                h.root.path().join("state/tee.cut").exists(),
                "{alg:?} loud={loud}: the cut went through tee: {all}"
            );
            assert_eq!(o.code, 1, "{alg:?} loud={loud}: {all}");
            assert!(
                !all.contains("RESTORE COMPLETE"),
                "{alg:?} loud={loud}: {all}"
            );
            if loud {
                assert!(
                    o.stderr.contains("FATAL: writing slice")
                        && o.stderr.contains("No space left on device")
                        && o.stderr.contains("is full or not writable"),
                    "{alg:?}: {}",
                    o.stderr
                );
                assert!(
                    !o.stderr.contains("decoded length"),
                    "{alg:?}: {}",
                    o.stderr
                );
            } else {
                assert!(
                    o.stderr
                        .contains("its decoded length is 1000 bytes and the manifest says"),
                    "{alg:?}: {}",
                    o.stderr
                );
                assert!(
                    o.stdout.contains("RESTORE INCOMPLETE"),
                    "{alg:?}: {}",
                    o.stdout
                );
            }
            assert!(
                !all.contains("could not decrypt"),
                "{alg:?} loud={loud}: {all}"
            );
        }
    }
}

// ---- the low findings of #525 and the rest of the traps ----

/// A2: the script keeps no list of process ids to kill at the end. The one
/// it kills is the read ahead's (`ahead_kill`): a job this shell has not yet
/// waited for, so its id cannot have passed to another process, and its
/// process group.
#[test]
fn the_script_never_kills_a_process_id_it_remembers() {
    let text = tapectl::volume::heir::RESTORE_SH;
    assert!(!text.contains("BG_PIDS"), "no list of process ids");
    assert!(!text.contains("kill -KILL"), "no SIGKILL at exit");
    let body = |name: &str| {
        text.split(&format!("\n{name}() {{\n"))
            .nth(1)
            .and_then(|r| r.split("\n}\n").next())
            .unwrap_or_else(|| panic!("{name}"))
    };
    let (kill, running) = (body("ahead_kill"), body("ahead_running"));
    assert!(kill.contains("kill -TERM"), "the positive control: {kill}");
    assert!(
        running.contains("kill -0") && !running.contains("kill -TERM"),
        "ahead_running only asks: {running}"
    );
    let outside = text.replace(kill, "").replace(running, "");
    for line in outside.lines() {
        let code = line.split('#').next().unwrap_or("");
        assert!(
            !code.contains("kill -") && !code.contains("kill \""),
            "only ahead_kill kills: {line}"
        );
    }
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

/// #525 A9, as T6.4-1 (#552) changed it: a `sha256sum -c` that checked nothing
/// (it prints nothing and exits 0) is never taken for a pass, and no name is
/// read back from it. Every file is then hashed alone, so the restore is whole
/// only because each file's own hash matched; a file the disk damaged is still
/// the one listed. The old assertion (every file damaged) held only because
/// the check read names back from `-c`'s output.
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
    let tar = heir::host_tool("tar").unwrap();
    h.put_tool("tar", &heir::corrupting_tar(&tar, "notes.txt"));
    let dest = h.dest("out");
    let o = h.run(&[
        "--restore",
        "--key",
        &key(&h, Who::TenantB),
        "--to",
        dest.to_str().unwrap(),
    ]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(!o.stdout.contains("match the sha256"), "{}", o.stdout);
    assert!(o.stdout.contains("RESTORE INCOMPLETE"), "{}", o.stdout);
    let list = std::fs::read_to_string(dest.join(".tapectl-damaged-files.txt")).unwrap();
    assert_eq!(
        list.lines().skip(1).collect::<Vec<_>>(),
        ["notes.txt"],
        "{list}"
    );
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
/// the tenant's): `--find-envelope` shows it ONCE, naming both envelopes
/// (#548 T6.2-2), and `--list` and `--restore` pick ONE block: the one with
/// the file table, the tenant envelope's, version 1 (the only version staged).
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
    assert_eq!(rows, 1, "one line per unit and version: {}", o.stdout);
    let line = o
        .stdout
        .lines()
        .find(|l| l.contains(&format!("  {UNIT_A}  version 1")))
        .unwrap();
    assert!(
        line.contains("envelopes at files ") && line.contains(", "),
        "the line names both envelopes: {line}"
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

// ---- named paths, the one repair, the index (the plan's T4.11) ----

/// Two tapes written from the same stagings: the same units, versions and
/// keys, two labels (`HEIR01`, `HEIR02`), each of which a test may damage.
fn copies() -> &'static [Tape] {
    static T: OnceLock<Vec<Tape>> = OnceLock::new();
    T.get_or_init(|| {
        let mut shape = Shape::two();
        shape.copies = 2;
        Tape::build_series(&shape)
    })
}

/// The first tape of one unit and, written from its second staging after
/// `unit tag` rewrote the dotfile, a second tape of the same Version (#528).
fn restaged_dotfile() -> &'static [Tape] {
    static T: OnceLock<Vec<Tape>> = OnceLock::new();
    T.get_or_init(|| {
        let mut shape = Shape::two();
        shape.restage = heir::Restage::Dotfile;
        Tape::build_series(&shape)
    })
}

/// The first tape (Version 1) and a second tape of Version 2 of the same unit.
fn restaged_content() -> &'static [Tape] {
    static T: OnceLock<Vec<Tape>> = OnceLock::new();
    T.get_or_init(|| {
        let mut shape = Shape::two();
        shape.restage = heir::Restage::Content;
        Tape::build_series(&shape)
    })
}

/// A whole tape and a second one that ended early, inside the second slice of
/// the plan: the big unit has only its first slice on it, a range.
fn whole_and_range() -> &'static [Tape] {
    static T: OnceLock<Vec<Tape>> = OnceLock::new();
    T.get_or_init(|| {
        let mut shape = Shape::two();
        shape.copies = 2;
        shape.warning_in_slice = Some(1);
        shape.warn_tape = 1;
        Tape::build_series(&shape)
    })
}

/// The tape position of slice `n` (1-based) of `unit` on `t`, whose units lie
/// on it in staging order.
fn slice_pos(t: &Tape, unit: &str, n: usize) -> usize {
    let mut base = 0;
    for u in &t.units {
        if u.name == unit {
            break;
        }
        base += u.slices;
    }
    t.slice_positions()[base + n - 1]
}

/// The data slices a run read from the tape, in order.
fn data_reads(t: &Tape, h: &Heir) -> Vec<usize> {
    let slices = t.slice_positions();
    let end = t.ended_after();
    h.dd_reads()
        .into_iter()
        .filter(|p| slices.contains(p) && *p <= end)
        .collect()
}

/// The slices the members named by `listed` (escaped paths) are in, and the
/// slice of the first name of a hard link among them (it is brought too).
fn slices_needed(
    t: &Tape,
    rows: &[Row],
    unit: &str,
    listed: &[String],
) -> std::collections::BTreeSet<usize> {
    let mut out = std::collections::BTreeSet::new();
    for p in listed {
        let r = row_esc(rows, p);
        out.insert(slice_pos(t, unit, r.1));
        if r.0 == "h" {
            // The table's sort puts `<unit>-hardlink.txt` before its link.
            out.insert(slice_pos(
                t,
                unit,
                row(rows, &format!("{unit}-hardlink.txt")).1,
            ));
        }
    }
    out
}

/// One row of `--list`: kind, slice, bytes and path (the path as printed).
type Row = (String, usize, u64, String);

fn list_rows(h: &Heir, key_file: &str, unit: &str) -> Vec<Row> {
    let o = h.run(&["--list", "--key", key_file, "--unit", unit]);
    assert_eq!(o.code, 0, "{}", o.all());
    fn tok(s: &str) -> (&str, &str) {
        let s = s.trim_start();
        match s.find(char::is_whitespace) {
            Some(i) => (&s[..i], s[i..].trim_start()),
            None => (s, ""),
        }
    }
    o.stdout
        .lines()
        .filter_map(|l| {
            let (kind, rest) = tok(l);
            if kind.len() != 1 || !"fdlhpcb".contains(kind) {
                return None;
            }
            let (slice, rest) = tok(rest);
            let (bytes, path) = tok(rest);
            Some((
                kind.to_string(),
                slice.parse().ok()?,
                bytes.parse().ok()?,
                path.split("  -> ").next().unwrap().to_string(),
            ))
        })
        .collect()
}

/// The row of `path`; `--list` prints a path as the table writes it, escaped.
fn row<'a>(rows: &'a [Row], path: &str) -> &'a Row {
    row_esc(rows, &escaped(path))
}

fn row_esc<'a>(rows: &'a [Row], escaped_path: &str) -> &'a Row {
    rows.iter()
        .find(|r| r.3 == escaped_path)
        .unwrap_or_else(|| panic!("no row for {escaped_path}: {rows:?}"))
}

fn unit_slices(t: &Tape, unit: &str) -> usize {
    t.units.iter().find(|u| u.name == unit).unwrap().slices
}

fn damaged_list(dest: &Path) -> Vec<String> {
    std::fs::read_to_string(dest.join(".tapectl-damaged-files.txt"))
        .unwrap_or_default()
        .lines()
        .skip(1)
        .map(str::to_string)
        .collect()
}

fn dest_str(p: &Path) -> &str {
    p.to_str().unwrap()
}

/// #518's defect was a path restore that read the wrong slices and exited 0.
/// One file of a unit of many slices: the stand-in `dd`'s log says WHICH tape
/// files were read (the front matter, then the one slice that holds the
/// file's member and no other), and the file is right.
#[test]
fn a_path_restore_reads_only_the_slice_that_holds_the_file_and_the_file_is_right() {
    let t = two();
    let h = Heir::of(t);
    let kb = key(&h, Who::TenantB);
    let rows = list_rows(&h, &kb, UNIT_B);
    let n = unit_slices(t, UNIT_B);
    assert!(n >= 4, "a unit of many slices: {n}");
    let (_, slice, _, _) = row(&rows, "inner/mid-3.dat");
    assert!(
        *slice > 1 && *slice < n,
        "a file in the middle of the unit, not in its first or last slice: {slice} of {n}"
    );
    let dest = h.dest("one");
    h.reset_ops();
    let o = h.run(&[
        "--restore",
        "--key",
        &kb,
        "--to",
        dest_str(&dest),
        "--unit",
        UNIT_B,
        "--path",
        "inner/mid-3.dat",
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert_eq!(
        data_reads(t, &h),
        vec![slice_pos(t, UNIT_B, *slice)],
        "only the slice that holds the member was read: {:?}",
        h.ops()
    );
    let got = heir::tree(&dest);
    let (_, want) = unit_files(t, UNIT_B)
        .iter()
        .find(|(p, _)| p == "inner/mid-3.dat")
        .unwrap();
    assert_eq!(got.len(), 1, "{:?}", got.keys());
    assert_eq!(got.get("inner/mid-3.dat"), Some(want));
    assert!(o.stdout.contains("RESTORE COMPLETE"), "{}", o.stdout);
    assert!(
        o.stdout.contains("File check: all 1 regular file(s) named"),
        "{}",
        o.stdout
    );
    assert!(
        o.stdout.contains("Who the ids were"),
        "a path restore prints the id map too: {}",
        o.stdout
    );
    assert!(!dest.join(".tapectl-damaged-files.txt").exists());
}

/// Two paths in two slices read those two slices, in order, once each; a
/// compressed unit is cut by the offsets of the stream, after decompression.
#[test]
fn paths_in_two_slices_read_those_two_slices_once_each_for_every_algorithm() {
    for (label, t) in [
        ("none", two()),
        ("gzip", compressed("gzip")),
        ("xz", compressed("xz")),
        ("zstd", compressed("zstd")),
    ] {
        let unit = if label == "none" { UNIT_B } else { UNIT_A };
        let who = if label == "none" {
            Who::TenantB
        } else {
            Who::TenantA
        };
        let h = Heir::of(t);
        let k = key(&h, who);
        let rows = list_rows(&h, &k, unit);
        let first = format!("inner/{unit}-deep.dat");
        let (_, s1, _, _) = row(&rows, &first);
        let (_, s2, _, _) = row(&rows, "notes.txt");
        let dest = h.dest("two");
        h.reset_ops();
        let o = h.run(&[
            "--restore",
            "--key",
            &k,
            "--to",
            dest_str(&dest),
            "--unit",
            unit,
            "--path",
            "notes.txt",
            "--path",
            &first,
        ]);
        assert_eq!(o.code, 0, "{label}: {}", o.all());
        let mut want = vec![slice_pos(t, unit, *s1), slice_pos(t, unit, *s2)];
        want.sort_unstable();
        want.dedup();
        assert_eq!(data_reads(t, &h), want, "{label}: {:?}", h.ops());
        let got = heir::tree(&dest);
        assert_eq!(got.len(), 2, "{label}: {:?}", got.keys());
        for (path, bytes) in unit_files(t, unit) {
            if let Some(have) = got.get(path) {
                assert_eq!(have, bytes, "{label}: {path}");
            }
        }
    }
}

/// A long member's run is read piece by piece, and the member is whole; its
/// first slice is read before its last.
#[test]
fn a_path_in_a_long_members_run_is_fed_piece_by_piece_and_comes_back_whole() {
    let t = two();
    let h = Heir::of(t);
    let kb = key(&h, Who::TenantB);
    let large = format!("{UNIT_B}-large.bin");
    let rows = list_rows(&h, &kb, UNIT_B);
    let (_, first, size, _) = row(&rows, &large);
    assert!(*size > 65_536 * 2, "a run of three slices");
    let dest = h.dest("long");
    h.reset_ops();
    let o = h.run(&[
        "--restore",
        "--key",
        &kb,
        "--to",
        dest_str(&dest),
        "--unit",
        UNIT_B,
        "--path",
        &large,
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    let want: Vec<usize> = (0..3).map(|i| slice_pos(t, UNIT_B, first + i)).collect();
    assert_eq!(data_reads(t, &h), want, "{:?}", h.ops());
    let got = heir::tree(&dest);
    let (_, bytes) = unit_files(t, UNIT_B)
        .iter()
        .find(|(p, _)| *p == large)
        .unwrap();
    assert_eq!(got.get(&large), Some(bytes));
}

/// A bad LATER piece of a long member ends the tar run: the partial file is
/// removed and the member listed; a path after it still comes back.
#[test]
fn a_path_whose_long_run_breaks_is_removed_and_listed_and_the_next_path_comes_back() {
    let t = two();
    let h = Heir::of(t);
    let kb = key(&h, Who::TenantB);
    let large = format!("{UNIT_B}-large.bin");
    let rows = list_rows(&h, &kb, UNIT_B);
    let (_, first, _, _) = row(&rows, &large);
    let bad = slice_pos(t, UNIT_B, first + 1);
    h.flip(bad, 4000);
    let dest = h.dest("broken");
    let o = h.run(&[
        "--restore",
        "--key",
        &kb,
        "--to",
        dest_str(&dest),
        "--unit",
        UNIT_B,
        "--path",
        &large,
        "--path",
        "notes.txt",
    ]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stderr.contains("a long file's run broke at a bad slice"),
        "{}",
        o.stderr
    );
    assert!(!dest.join(&large).exists(), "the partial file was removed");
    assert_eq!(damaged_list(&dest), vec![large.clone()]);
    let got = heir::tree(&dest);
    let (_, bytes) = unit_files(t, UNIT_B)
        .iter()
        .find(|(p, _)| p == "notes.txt")
        .unwrap();
    assert_eq!(got.get("notes.txt"), Some(bytes), "the next path is whole");
}

/// A hard link brings its first name's member first; the two are one file.
#[test]
fn a_hard_link_is_restored_with_its_first_name_and_the_two_share_one_file() {
    use std::os::unix::fs::MetadataExt;
    let t = two();
    let h = Heir::of(t);
    let ka = key(&h, Who::TenantA);
    // The table's sort puts `hardlink.txt` first: `small.txt` is the link.
    let link = format!("{UNIT_A}-small.txt");
    let dest = h.dest("hl");
    let o = h.run(&[
        "--restore",
        "--key",
        &ka,
        "--to",
        dest_str(&dest),
        "--unit",
        UNIT_A,
        "--path",
        &link,
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(o.stderr.contains("hard link(s) among them"), "{}", o.stderr);
    let first = dest.join(format!("{UNIT_A}-hardlink.txt"));
    let a = std::fs::metadata(&first).unwrap();
    let b = std::fs::metadata(dest.join(&link)).unwrap();
    assert_eq!(
        (a.ino(), a.dev()),
        (b.ino(), b.dev()),
        "one file, two names"
    );
    assert!(a.nlink() >= 2);
    assert_eq!(
        std::fs::read(dest.join(&link)).unwrap(),
        unit_files(t, UNIT_A)[0].1
    );
}

/// A folder names its own entry only: nothing under it is restored, and the
/// script says so.
#[test]
fn a_folder_path_restores_the_folder_and_nothing_under_it() {
    let t = two();
    let h = Heir::of(t);
    let ka = key(&h, Who::TenantA);
    let dest = h.dest("dir");
    let o = h.run(&[
        "--restore",
        "--key",
        &ka,
        "--to",
        dest_str(&dest),
        "--unit",
        UNIT_A,
        "--path",
        "inner",
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(dest.join("inner").is_dir());
    assert_eq!(std::fs::read_dir(dest.join("inner")).unwrap().count(), 0);
    assert!(o.stderr.contains("a folder names only its own entry"));
}

/// A path the table does not have is refused before any slice is read, naming
/// it; nothing in the destination changes.
#[test]
fn a_path_not_in_the_table_is_refused_before_any_slice_is_read() {
    let t = two();
    let h = Heir::of(t);
    let ka = key(&h, Who::TenantA);
    let dest = h.dest("none");
    h.reset_ops();
    let o = h.run(&[
        "--restore",
        "--key",
        &ka,
        "--to",
        dest_str(&dest),
        "--unit",
        UNIT_A,
        "--path",
        "no/such/file",
        "--path",
        "notes.txt",
    ]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stderr.contains("no/such/file") && o.stderr.contains("not in the file table"),
        "{}",
        o.stderr
    );
    assert!(data_reads(t, &h).is_empty(), "no slice read: {:?}", h.ops());
    assert!(heir::tree(&dest).is_empty());
}

/// `--path` names a unit: exactly one, and never `--all`; `--path` and
/// `--paths-from` go with `--restore` only.
#[test]
fn path_modes_have_usage_errors_and_are_in_help() {
    let h = Heir::of(two());
    let ka = key(&h, Who::TenantA);
    for (args, word) in [
        (
            vec!["--restore", "--key", &ka, "--to", "/x", "--path", "p"],
            "ONE unit",
        ),
        (
            vec![
                "--restore",
                "--key",
                &ka,
                "--to",
                "/x",
                "--all",
                "--path",
                "p",
            ],
            "ONE unit",
        ),
        (
            vec![
                "--restore",
                "--key",
                &ka,
                "--to",
                "/x",
                "--unit",
                "a",
                "--path",
                "p",
                "--paths-from",
                "l",
            ],
            "give one",
        ),
        (
            vec!["--list", "--key", &ka, "--path", "p"],
            "go with --restore",
        ),
        (
            vec![
                "--restore",
                "--key",
                &ka,
                "--to",
                "/x",
                "--all",
                "--paths-from",
                "l",
            ],
            "ONE unit",
        ),
        (vec!["--key", &ka, "--where"], "needs a value"),
        (vec!["--key", &ka, "--find"], "needs a value"),
        (vec!["--where", "u"], "needs your age key file"),
    ] {
        let o = h.run(&args);
        assert_eq!(o.code, 2, "{args:?}: {}", o.all());
        assert!(o.stderr.contains(word), "{args:?}: {}", o.stderr);
    }
    assert!(h.ops().is_empty(), "a usage error reads no tape");
    let o = h.run(&["--help"]);
    for word in [
        "--path P",
        "--paths-from LIST",
        "--where UNIT",
        "--where --all",
        "--find TEXT",
    ] {
        assert!(o.stdout.contains(word), "{word} is in --help: {}", o.stdout);
    }
}

/// The check of a path restore covers the paths restored and no others: a
/// unit whose other files are not in the destination is no damage.
/// Also with a `sha256sum` that quotes names, and with a name holding a
/// backslash.
#[test]
fn a_path_restore_checks_only_the_paths_it_restored_even_with_a_quoting_sha256sum() {
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
    let kb = key(&h, Who::TenantB);
    let dest = h.dest("quoted");
    let o = h.run(&[
        "--restore",
        "--key",
        &kb,
        "--to",
        dest_str(&dest),
        "--unit",
        UNIT_B,
        "--path",
        &escaped("odd\\back name.txt"),
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(
        o.stdout.contains("File check: all 1 regular file(s) named"),
        "{}",
        o.stdout
    );
    assert_eq!(heir::tree(&dest).len(), 1);
}

/// A tar that drops the file at exit 0 (BusyBox does) leaves the path
/// damaged: the check, not tar's status, decides.
#[test]
fn a_path_a_tar_dropped_at_exit_0_is_caught_and_listed() {
    let t = two();
    let h = Heir::of(t);
    let real = heir::host_tool("tar").unwrap();
    h.put_tool("tar", &heir::dropping_tar(&real, "notes.txt"));
    let kb = key(&h, Who::TenantB);
    let dest = h.dest("dropped");
    let o = h.run(&[
        "--restore",
        "--key",
        &kb,
        "--to",
        dest_str(&dest),
        "--unit",
        UNIT_B,
        "--path",
        "notes.txt",
        "--path",
        "inner/mid-1.dat",
    ]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stdout
            .contains("File check: 1 of 2 regular file(s) named"),
        "{}",
        o.stdout
    );
    assert_eq!(damaged_list(&dest), vec!["notes.txt".to_string()]);
}

/// An existing file the path names is refused without `--overwrite`, before
/// any slice is read; with it the file is replaced.
#[test]
fn an_existing_file_is_refused_without_overwrite_and_replaced_with_it() {
    let t = two();
    let h = Heir::of(t);
    let ka = key(&h, Who::TenantA);
    let dest = h.dest("exists");
    std::fs::write(dest.join("notes.txt"), b"mine").unwrap();
    h.reset_ops();
    let name = format!("{UNIT_A}-small.txt");
    std::fs::write(dest.join(&name), b"mine").unwrap();
    let go = |extra: &[&str]| {
        let mut a = vec![
            "--restore",
            "--key",
            ka.as_str(),
            "--to",
            dest_str(&dest),
            "--unit",
            UNIT_A,
            "--path",
            name.as_str(),
        ];
        a.extend_from_slice(extra);
        h.run(&a)
    };
    let o = go(&[]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(o.stderr.contains("--overwrite"), "{}", o.stderr);
    assert!(data_reads(t, &h).is_empty(), "{:?}", h.ops());
    assert_eq!(std::fs::read(dest.join(&name)).unwrap(), b"mine");
    let o = go(&["--overwrite"]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert_eq!(
        std::fs::read(dest.join(&name)).unwrap(),
        unit_files(t, UNIT_A)[0].1
    );
    // The other file in the destination was not touched.
    assert_eq!(std::fs::read(dest.join("notes.txt")).unwrap(), b"mine");
}

/// A path with a TAB, a newline and a backslash is named as the heir types it
/// and written to the list in the index's escaping, and read back from it.
#[test]
fn a_tab_a_newline_and_a_backslash_in_a_path_round_trip_through_the_list() {
    let t = &copies()[0];
    let t2 = &copies()[1];
    let tab_nl = "tab\tand\nnewline.txt";
    let back = "odd\\back name.txt";
    // Both are named to --path as --list prints them: escaped (item 297).
    let h = Heir::of(t);
    let kb = key(&h, Who::TenantB);
    let dest = h.dest("raw");
    let o = h.run(&[
        "--restore",
        "--key",
        &kb,
        "--to",
        dest_str(&dest),
        "--unit",
        UNIT_B,
        "--path",
        &escaped(tab_nl),
        "--path",
        &escaped(back),
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    let got = heir::tree(&dest);
    for p in [tab_nl, back] {
        let (_, bytes) = unit_files(t, UNIT_B).iter().find(|(q, _)| q == p).unwrap();
        assert_eq!(got.get(p), Some(bytes), "{p:?}");
    }
    // The list a restore writes names them in the index's escaping.
    let h1 = Heir::of(t);
    let rows = list_rows(&h1, &kb, UNIT_B);
    let (_, slice, _, _) = row(&rows, back);
    h1.flip(slice_pos(t, UNIT_B, *slice), 300);
    let dest1 = h1.dest("first");
    let o = h1.run(&["--restore", "--key", &kb, "--to", dest_str(&dest1)]);
    assert_eq!(o.code, 1, "{}", o.all());
    let list = damaged_list(&dest1);
    assert!(
        list.iter().any(|l| l == "odd\\\\back name.txt"),
        "a backslash is two: {list:?}"
    );
    // The same slice also holds the TAB-and-newline file or not; whichever
    // were listed come back from the other copy, exactly.
    let h2 = Heir::of(t2);
    let o = h2.run(&[
        "--restore",
        "--key",
        &kb,
        "--to",
        dest_str(&dest1),
        "--paths-from",
        dest1.join(".tapectl-damaged-files.txt").to_str().unwrap(),
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert_restored(t, UNIT_B, &dest1);
    assert!(!dest1.join(".tapectl-damaged-files.txt").exists());
    // A list naming the TAB-and-newline file, escaped, is read as that file.
    let h3 = Heir::of(t2);
    let dest3 = h3.dest("esc");
    let uuid = &t2.unit_uuids[UNIT_B];
    let list_file = h3.root.path().join("my-list.txt");
    std::fs::write(
        &list_file,
        format!(
            "#tapectl-damaged-files\t1\tunit={UNIT_B}\tuuid={uuid}\tversion=1\ntab\\tand\\nnewline.txt\nodd\\\\back name.txt\n"
        ),
    )
    .unwrap();
    let o = h3.run(&[
        "--restore",
        "--key",
        &kb,
        "--to",
        dest_str(&dest3),
        "--paths-from",
        list_file.to_str().unwrap(),
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    let got = heir::tree(&dest3);
    assert_eq!(got.len(), 2, "{:?}", got.keys());
    assert!(got.contains_key(tab_nl) && got.contains_key(back));
}

/// The one repair, and the Done-when of T4.11: two copies of one unit and
/// version, each damaged in different files, made whole in two runs. The
/// second run reads only the slices that hold the listed files, the list is
/// removed, and nothing else in the destination is touched.
#[test]
fn two_copies_each_damaged_in_different_files_are_made_whole_in_two_runs() {
    let (t1, t2) = (&copies()[0], &copies()[1]);
    let kb = |h: &Heir| key(h, Who::TenantB);
    let h1 = Heir::of(t1);
    let rows = list_rows(&h1, &kb(&h1), UNIT_B);
    let n = unit_slices(t1, UNIT_B);
    assert!(n >= 5, "{n}");
    // Copy 1 loses the slice of mid-1; copy 2 the slice of notes.txt.
    let (_, s1, _, _) = row(&rows, "inner/mid-1.dat");
    let (_, s2, _, _) = row(&rows, "notes.txt");
    assert_ne!(s1, s2, "different slices, so different files");
    h1.flip(slice_pos(t1, UNIT_B, *s1), 500);
    let dest = h1.dest("unit");
    let o = h1.run(&["--restore", "--key", &kb(&h1), "--to", dest_str(&dest)]);
    assert_eq!(o.code, 1, "{}", o.all());
    let list = damaged_list(&dest);
    assert!(list.contains(&"inner/mid-1.dat".to_string()), "{list:?}");
    assert!(!list.contains(&"notes.txt".to_string()), "{list:?}");
    let mut before = heir::tree(&dest);
    before.remove(".tapectl-damaged-files.txt");
    // Copy 2, damaged in notes.txt's slice, repairs the list: the slice it
    // needs is intact, the one it does not is never read.
    let h2 = Heir::of(t2);
    h2.flip(slice_pos(t2, UNIT_B, *s2), 500);
    h2.reset_ops();
    let o = h2.run(&[
        "--restore",
        "--key",
        &kb(&h2),
        "--to",
        dest_str(&dest),
        "--paths-from",
        dest.join(".tapectl-damaged-files.txt").to_str().unwrap(),
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(o.stdout.contains("RESTORE COMPLETE"), "{}", o.stdout);
    assert!(
        o.stdout.contains("is removed: nothing is left to repair"),
        "{}",
        o.stdout
    );
    assert!(!dest.join(".tapectl-damaged-files.txt").exists());
    assert!(
        !data_reads(t2, &h2).contains(&slice_pos(t2, UNIT_B, *s2)),
        "the damaged slice of copy 2 was never needed: {:?}",
        h2.ops()
    );
    assert_restored(t1, UNIT_B, &dest);
    // Nothing beyond the listed paths was written: every file that was whole
    // before is the very same bytes (and the run read only the slices of the
    // listed files).
    for (p, b) in &before {
        assert_eq!(heir::tree(&dest).get(p), Some(b), "{p}");
    }
    let needed = slices_needed(t2, &rows, UNIT_B, &list);
    let read: std::collections::BTreeSet<usize> = data_reads(t2, &h2).into_iter().collect();
    assert_eq!(read, needed, "{:?}", h2.ops());
}

/// A listed file that the second copy also lost stays on the list; the list
/// is rewritten with the paths still damaged and the exit is 1.
#[test]
fn a_repair_whose_files_still_fail_leaves_a_list_with_just_those() {
    let (t1, t2) = (&copies()[0], &copies()[1]);
    let h1 = Heir::of(t1);
    let kb = key(&h1, Who::TenantB);
    let rows = list_rows(&h1, &kb, UNIT_B);
    let (_, s1, _, _) = row(&rows, "inner/mid-1.dat");
    let (_, s2, _, _) = row(&rows, "notes.txt");
    assert_ne!(s1, s2);
    // Copy 1 loses two slices; copy 2 loses one of the same.
    h1.flip(slice_pos(t1, UNIT_B, *s1), 500);
    h1.flip(slice_pos(t1, UNIT_B, *s2), 500);
    let dest = h1.dest("unit");
    let o = h1.run(&["--restore", "--key", &kb, "--to", dest_str(&dest)]);
    assert_eq!(o.code, 1, "{}", o.all());
    let list = damaged_list(&dest);
    assert!(list.contains(&"notes.txt".to_string()) && list.contains(&"inner/mid-1.dat".into()));
    let h2 = Heir::of(t2);
    h2.flip(slice_pos(t2, UNIT_B, *s2), 500);
    let list_path = dest.join(".tapectl-damaged-files.txt");
    let o = h2.run(&[
        "--restore",
        "--key",
        &kb,
        "--to",
        dest_str(&dest),
        "--paths-from",
        list_path.to_str().unwrap(),
    ]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(o.stdout.contains("RESTORE INCOMPLETE"), "{}", o.stdout);
    let left = damaged_list(&dest);
    assert!(left.contains(&"notes.txt".to_string()), "{left:?}");
    assert!(
        !left.contains(&"inner/mid-1.dat".to_string()),
        "the file the second copy had is whole and off the list: {left:?}"
    );
    let got = heir::tree(&dest);
    let (_, want) = unit_files(t1, UNIT_B)
        .iter()
        .find(|(p, _)| p == "inner/mid-1.dat")
        .unwrap();
    assert_eq!(got.get("inner/mid-1.dat"), Some(want));
    // The header of the rewritten list still names this unit and version.
    let first = std::fs::read_to_string(&list_path).unwrap();
    assert!(
        first.starts_with(&format!(
            "#tapectl-damaged-files\t1\tunit={UNIT_B}\tuuid={}\tversion=1\n",
            t1.unit_uuids[UNIT_B]
        )),
        "{first}"
    );
}

/// With a `sha256sum -c` that checks nothing (T6.4-1, #552: no name is read
/// back from it) the repair counts a file as whole only when the file's own
/// hash, taken alone, matched: the repaired file leaves the list, and the run
/// is whole because the hash said so.
#[test]
fn a_repair_with_a_sha256sum_that_checks_nothing_still_hashes_each_file_alone() {
    let (t1, t2) = (&copies()[0], &copies()[1]);
    let h1 = Heir::of(t1);
    let kb = key(&h1, Who::TenantB);
    let rows = list_rows(&h1, &kb, UNIT_B);
    let (_, s1, _, _) = row(&rows, "inner/mid-1.dat");
    h1.flip(slice_pos(t1, UNIT_B, *s1), 500);
    let dest = h1.dest("unit");
    h1.run(&["--restore", "--key", &kb, "--to", dest_str(&dest)]);
    let before = damaged_list(&dest);
    assert!(!before.is_empty());
    let h2 = Heir::of(t2);
    // A sha256sum that prints nothing and exits 0 when it is asked to check.
    let real = heir::host_tool("sha256sum").unwrap();
    h2.put_tool(
        "sha256sum",
        &format!(
            "#!/bin/bash\ncase \"$*\" in *-c*) exit 0 ;; esac\nexec {} \"$@\"\n",
            real.display()
        ),
    );
    let o = h2.run(&[
        "--restore",
        "--key",
        &kb,
        "--to",
        dest_str(&dest),
        "--paths-from",
        dest.join(".tapectl-damaged-files.txt").to_str().unwrap(),
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(damaged_list(&dest).is_empty(), "{}", o.all());
}

/// Another version, another unit (same name, other uuid) and a list that is
/// not a damaged-file list are each refused by name, with nothing read from a
/// slice and the list left as it was.
#[test]
fn a_repair_is_refused_for_another_version_another_unit_or_a_file_that_is_no_list() {
    let first = |h: &Heir, who| key(h, who);
    // A damaged restore of version 2 (the second tape of the series).
    let series = restaged_content();
    let (t_v1, t_v2) = (&series[0], &series[1]);
    let h = Heir::of(t_v2);
    let ka = first(&h, Who::TenantA);
    let rows = list_rows(&h, &ka, UNIT_A);
    let (_, slice, _, _) = row(&rows, &format!("{UNIT_A}-small.txt"));
    h.flip(slice_pos(t_v2, UNIT_A, *slice), 400);
    let dest = h.dest("v2");
    let o = h.run(&["--restore", "--key", &ka, "--to", dest_str(&dest)]);
    assert_eq!(o.code, 1, "{}", o.all());
    let list_path = dest.join(".tapectl-damaged-files.txt");
    let list_before = std::fs::read(&list_path).unwrap();
    let tree_before = heir::tree(&dest);
    assert!(
        String::from_utf8_lossy(&list_before).contains("\tversion=2\n"),
        "version 2: {}",
        String::from_utf8_lossy(&list_before)
    );
    // The tape of version 1 of the same unit: another version.
    let h1 = Heir::of(t_v1);
    h1.reset_ops();
    let o = h1.run(&[
        "--restore",
        "--key",
        &ka,
        "--to",
        dest_str(&dest),
        "--paths-from",
        list_path.to_str().unwrap(),
    ]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stderr.contains("only as version(s) 1") && o.stderr.contains("list is for version 2"),
        "{}",
        o.stderr
    );
    assert!(data_reads(t_v1, &h1).is_empty(), "{:?}", h1.ops());
    // A tape of another home: the same unit NAME, another uuid, another unit.
    let other = Tape::build(&Shape::two());
    let ho = Heir::of(&other);
    let o = ho.run(&[
        "--restore",
        "--key",
        &key(&ho, Who::TenantA),
        "--to",
        dest_str(&dest),
        "--paths-from",
        list_path.to_str().unwrap(),
    ]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stderr.contains("another unit than the list's"),
        "{}",
        o.stderr
    );
    // A file that is no list.
    let junk = h.root.path().join("junk.txt");
    std::fs::write(&junk, "a\nb\n").unwrap();
    let o = h.run(&[
        "--restore",
        "--key",
        &ka,
        "--to",
        dest_str(&dest),
        "--paths-from",
        junk.to_str().unwrap(),
    ]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stderr.contains("is not a damaged-file list"),
        "{}",
        o.stderr
    );
    // A list of a format this script does not know.
    std::fs::write(
        &junk,
        "#tapectl-damaged-files\t2\tunit=a\tuuid=b\tversion=1\n",
    )
    .unwrap();
    let o = h.run(&[
        "--restore",
        "--key",
        &ka,
        "--to",
        dest_str(&dest),
        "--paths-from",
        junk.to_str().unwrap(),
    ]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(o.stderr.contains("format 2"), "{}", o.stderr);
    // Through all of that the list and the destination are as they were.
    assert_eq!(std::fs::read(&list_path).unwrap(), list_before);
    assert_eq!(heir::tree(&dest), tree_before);
    // The right tape (this one's own copy is the tape itself, intact) works.
    let h_ok = Heir::of(t_v2);
    let o = h_ok.run(&[
        "--restore",
        "--key",
        &ka,
        "--to",
        dest_str(&dest),
        "--paths-from",
        list_path.to_str().unwrap(),
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(!list_path.exists());
}

/// A listed path the second tape's table does not have is refused before any
/// slice is read, and `--unit` or `--version` that disagree with the list are
/// usage errors.
#[test]
fn a_repair_with_a_path_the_table_lacks_or_a_unit_that_disagrees_is_refused() {
    let t = &copies()[1];
    let h = Heir::of(t);
    let kb = key(&h, Who::TenantB);
    let uuid = &t.unit_uuids[UNIT_B];
    let list = h.root.path().join("list.txt");
    std::fs::write(
        &list,
        format!("#tapectl-damaged-files\t1\tunit={UNIT_B}\tuuid={uuid}\tversion=1\nnotes.txt\nnot/a/file\n"),
    )
    .unwrap();
    let dest = h.dest("d");
    h.reset_ops();
    let o = h.run(&[
        "--restore",
        "--key",
        &kb,
        "--to",
        dest_str(&dest),
        "--paths-from",
        list.to_str().unwrap(),
    ]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(o.stderr.contains("not/a/file"), "{}", o.stderr);
    assert!(data_reads(t, &h).is_empty());
    for extra in [vec!["--unit", "other"], vec!["--version", "9"]] {
        let mut args = vec![
            "--restore",
            "--key",
            kb.as_str(),
            "--to",
            dest_str(&dest),
            "--paths-from",
            list.to_str().unwrap(),
        ];
        args.extend(extra.iter().copied());
        let o = h.run(&args);
        assert_eq!(o.code, 2, "{extra:?}: {}", o.all());
    }
}

/// A lost FOLDER goes on the damaged-file list, as every lost member does; the
/// repair brings its own entry and nothing under it (it reads by offsets and
/// never extracts by name).
#[test]
fn a_lost_folder_is_listed_and_the_repair_brings_its_own_entry_and_nothing_under_it() {
    let (t1, t2) = (&copies()[0], &copies()[1]);
    let h1 = Heir::of(t1);
    let kb = key(&h1, Who::TenantB);
    let rows = list_rows(&h1, &kb, UNIT_B);
    let (kind, dir_slice, _, _) = row(&rows, "inner");
    assert_eq!(kind, "d");
    h1.flip(slice_pos(t1, UNIT_B, *dir_slice), 400);
    let dest = h1.dest("unit");
    let o = h1.run(&["--restore", "--key", &kb, "--to", dest_str(&dest)]);
    assert_eq!(o.code, 1, "{}", o.all());
    let list = damaged_list(&dest);
    assert!(
        list.contains(&"inner".to_string()),
        "the folder is on the list: {list:?}"
    );
    // A file under the folder that came back whole from another slice: take
    // it away, to see the repair leave it away.
    let under: Vec<&Row> = rows
        .iter()
        .filter(|r| r.3.starts_with("inner/") && r.1 != *dir_slice)
        .collect();
    let victim = &under
        .first()
        .expect("a file under inner in another slice")
        .3;
    assert!(!list.contains(victim));
    std::fs::remove_file(dest.join(victim)).unwrap();
    // The folder's own entry is gone from the damaged copy: make it so.
    let h2 = Heir::of(t2);
    let o = h2.run(&[
        "--restore",
        "--key",
        &kb,
        "--to",
        dest_str(&dest),
        "--paths-from",
        dest.join(".tapectl-damaged-files.txt").to_str().unwrap(),
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(dest.join("inner").is_dir());
    assert!(
        !dest.join(victim).exists(),
        "the repair did not extract what lies under the folder"
    );
    // Only the listed members were fed: the same slices the list's members
    // begin in, and no others.
    let needed = slices_needed(t2, &rows, UNIT_B, &list);
    let read: std::collections::BTreeSet<usize> = data_reads(t2, &h2).into_iter().collect();
    assert_eq!(read, needed, "{:?}", h2.ops());
}

/// #528: a tape written from a re-stage after `unit tag` rewrote the dotfile
/// states the NEW dotfile's hash in its own table, so the whole restore is
/// complete, the dotfile is the new one, and a path restore of it checks out.
#[test]
fn a_tape_written_from_a_restage_after_a_dotfile_edit_restores_complete() {
    let series = restaged_dotfile();
    let (first, second) = (&series[0], &series[1]);
    assert_eq!(first.unit_uuids[UNIT_A], second.unit_uuids[UNIT_A]);
    let dotfile_of = |t: &Tape| {
        let h = Heir::of(t);
        let ka = key(&h, Who::TenantA);
        let dest = h.dest("whole");
        let o = h.run(&["--restore", "--key", &ka, "--to", dest_str(&dest)]);
        assert_eq!(o.code, 0, "{}", o.all());
        assert!(o.stdout.contains("RESTORE COMPLETE"), "{}", o.stdout);
        assert!(!dest.join(".tapectl-damaged-files.txt").exists());
        let dot = std::fs::read_to_string(dest.join(".tapectl-unit.toml")).unwrap();
        // And by path.
        let one = h.dest("one");
        let o = h.run(&[
            "--restore",
            "--key",
            &ka,
            "--to",
            dest_str(&one),
            "--unit",
            UNIT_A,
            "--path",
            ".tapectl-unit.toml",
        ]);
        assert_eq!(o.code, 0, "{}", o.all());
        assert!(
            o.stdout.contains("File check: all 1 regular file(s) named"),
            "the dotfile is checked against this tape's own table: {}",
            o.stdout
        );
        assert_eq!(
            std::fs::read_to_string(one.join(".tapectl-unit.toml")).unwrap(),
            dot
        );
        dot
    };
    let old = dotfile_of(first);
    let new = dotfile_of(second);
    assert!(!old.contains("re-staged"), "{old}");
    assert!(
        new.contains("re-staged"),
        "the tape of the re-stage holds the new dotfile: {new}"
    );
}

/// A range is a source for the files it holds whole: a copy that ended early
/// restores the listed files whose slices it holds, and the others stay on
/// the list.
#[test]
fn a_range_tape_restores_the_listed_files_it_holds_and_the_rest_stay_listed() {
    let (whole, range) = (&whole_and_range()[0], &whole_and_range()[1]);
    let h1 = Heir::of(whole);
    let kb = key(&h1, Who::TenantB);
    let rows = list_rows(&h1, &kb, UNIT_B);
    // Lose two slices on the whole tape: one that the range tape also holds
    // (its first slice) and one it does not.
    let first_slice = rows.iter().map(|r| r.1).min().unwrap();
    let last_slice = rows.iter().map(|r| r.1).max().unwrap();
    h1.flip(slice_pos(whole, UNIT_B, first_slice), 300);
    h1.flip(slice_pos(whole, UNIT_B, last_slice), 300);
    let dest = h1.dest("unit");
    let o = h1.run(&["--restore", "--key", &kb, "--to", dest_str(&dest)]);
    assert_eq!(o.code, 1, "{}", o.all());
    let listed = damaged_list(&dest);
    let in_first: Vec<&Row> = rows
        .iter()
        .filter(|r| r.1 == first_slice && r.0 == "f")
        .collect();
    let in_last: Vec<&Row> = rows
        .iter()
        .filter(|r| r.1 == last_slice && r.0 == "f")
        .collect();
    assert!(!in_first.is_empty() && !in_last.is_empty());
    let h2 = Heir::of(range);
    h2.reset_ops();
    let o = h2.run(&[
        "--restore",
        "--key",
        &kb,
        "--to",
        dest_str(&dest),
        "--paths-from",
        dest.join(".tapectl-damaged-files.txt").to_str().unwrap(),
    ]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stderr.contains("do not lie in slices this tape holds")
            || o.stderr.contains("lie in slices this tape does not hold"),
        "{}",
        o.stderr
    );
    let left = damaged_list(&dest);
    for r in &in_first {
        let p = r.3.clone();
        assert!(listed.contains(&p), "{p}: {listed:?}");
        assert!(!left.contains(&p), "{p} came from the range tape: {left:?}");
    }
    for r in &in_last {
        let p = r.3.clone();
        assert!(left.contains(&p), "{p} is still listed: {left:?}");
    }
    // Only the slice the range tape holds was read, and no read was sent
    // past the end of the tape, whatever the directory held there (R3).
    assert_eq!(
        data_reads(range, &h2),
        vec![slice_pos(range, UNIT_B, first_slice)],
        "{:?}",
        h2.ops()
    );
    assert!(
        h2.dd_reads().iter().all(|p| *p <= range.ended_after() + 1),
        "{:?}",
        h2.dd_reads()
    );
}

// ---- the index: --where and --find (volume-format-v3.md 1.5) ----

fn indexed(edit: fn(&tapectl::volume::index::IndexFiles)) -> Tape {
    let mut shape = Shape::two();
    shape.index_edit = Some(edit);
    Tape::build(&shape)
}

/// Two earlier tapes hold both units: one sealed, one proven bad.
fn with_holders() -> &'static Tape {
    static T: OnceLock<Tape> = OnceLock::new();
    T.get_or_init(|| indexed(heir::index_holders))
}

fn permuted() -> &'static Tape {
    static T: OnceLock<Tape> = OnceLock::new();
    T.get_or_init(|| indexed(heir::index_permuted))
}

fn bulk() -> &'static Tape {
    static T: OnceLock<Tape> = OnceLock::new();
    T.get_or_init(|| indexed(heir::index_bulk))
}

fn no_index() -> &'static Tape {
    static T: OnceLock<Tape> = OnceLock::new();
    T.get_or_init(|| {
        let mut shape = Shape::two();
        shape.no_index = true;
        Tape::build(&shape)
    })
}

#[test]
fn where_prints_every_version_and_holder_of_a_unit_from_a_tenant_key() {
    let t = with_holders();
    let h = Heir::of(t);
    let o = h.run(&["--where", UNIT_A, "--key", &key(&h, Who::TenantA)]);
    assert_eq!(o.code, 0, "{}", o.all());
    let out = &o.stdout;
    for want in [
        "Index read from tape: HEIR01",
        &format!("Unit {UNIT_A} (tenant {TENANT_A}):"),
        "  Version 1: ",
        &format!("unit uuid {}", t.unit_uuids[UNIT_A]),
        "    tape HEIR01  (this tape)  slices 1-",
        "    tape OLD01  (sealed 2026-10-01T00:00:00Z)",
        "    tape OLD02  (QUARANTINED: a verify proved this medium bad",
        "volume uuid ",
        "cartridge serial SERIALOLD01",
        "generation LTO-6",
        "A holder marked 'this tape' is the tape you have loaded",
    ] {
        assert!(out.contains(want), "{want}: {out}");
    }
    assert!(
        !out.contains(UNIT_B),
        "another tenant's unit is not shown: {out}"
    );
    assert!(!out.contains("a range"), "a whole tape is no range: {out}");
}

/// Columns are found by their header names: reversed, with one this script
/// does not know, every mode reads the same.
#[test]
fn columns_are_found_by_header_name_and_an_unknown_column_is_ignored() {
    let (a, b) = (permuted(), two());
    let (h1, h2) = (Heir::of(a), Heir::of(b));
    for (mode, arg) in [("--where", UNIT_A), ("--find", "deep")] {
        let o1 = h1.run(&[mode, arg, "--key", &key(&h1, Who::TenantA)]);
        let o2 = h2.run(&[mode, arg, "--key", &key(&h2, Who::TenantA)]);
        assert_eq!(o1.code, 0, "{mode}: {}", o1.all());
        assert_eq!(o2.code, 0, "{mode}: {}", o2.all());
        // The tapes differ in their uuids, and the written date is the same.
        let norm = |t: &Tape, s: &str| {
            s.replace(&t.unit_uuids[UNIT_A], "UUID")
                .replace(&t.volume_uuid, "VOL")
        };
        // (The ciphertext sizes of two stagings differ: the byte count of the
        // Version line is not compared, its words are.)
        let drop_bytes = |s: String| {
            s.lines()
                .map(|l| match l.find("slice(s), ") {
                    Some(i) if l.starts_with("  Version") => l[..i].to_string(),
                    _ if l.contains("/RESTORE.sh ") => "RESTORE.sh ...".to_string(),
                    _ => l.to_string(),
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert_eq!(
            drop_bytes(norm(a, &o1.stdout)),
            drop_bytes(norm(b, &o2.stdout)),
            "{mode}"
        );
    }
}

#[test]
fn where_with_the_escrow_key_reads_the_operator_map_of_every_tenant() {
    let t = with_holders();
    let h = Heir::of(t);
    let ke = key(&h, Who::Escrow);
    let o = h.run(&["--where", UNIT_B, "--key", &ke]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(
        o.stdout
            .contains(&format!("Unit {UNIT_B} (tenant {TENANT_B}):")),
        "{}",
        o.stdout
    );
    assert!(o.stdout.contains("tape OLD01"), "{}", o.stdout);
    let o = h.run(&["--where", UNIT_A, "--key", &ke]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(
        o.stdout
            .contains(&format!("Unit {UNIT_A} (tenant {TENANT_A}):")),
        "{}",
        o.stdout
    );
}

#[test]
fn where_names_the_units_the_index_lists_when_it_finds_none() {
    let t = with_holders();
    let h = Heir::of(t);
    let o = h.run(&["--where", "no-such-unit", "--key", &key(&h, Who::TenantA)]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stdout
            .contains("No unit named 'no-such-unit' is in this index"),
        "{}",
        o.stdout
    );
    assert!(o.stdout.contains(&format!("    {UNIT_A}")), "{}", o.stdout);
    assert!(!o.stdout.contains(UNIT_B), "{}", o.stdout);
    assert!(
        o.stdout
            .contains("A tenant's key opens that tenant's index only"),
        "{}",
        o.stdout
    );
    let o = h.run(&["--where", "no-such-unit", "--key", &key(&h, Who::Escrow)]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(o.stdout.contains(UNIT_A) && o.stdout.contains(UNIT_B));
    assert!(!o.stdout.contains("A tenant's key opens"), "{}", o.stdout);
}

/// A miss on a large index names every unit (no cap), and `--where --all`
/// lists every unit the index names past forty, in the order the rows give,
/// with each unit's newest version by number, never by text.
#[test]
fn a_miss_and_where_all_on_a_large_index_name_every_unit() {
    let t = bulk();
    let h = Heir::of(t);
    let ka = key(&h, Who::TenantA);
    let o = h.run(&["--where", "nothing", "--key", &ka]);
    assert_eq!(o.code, 1, "{}", o.all());
    for n in 0..60 {
        assert!(
            o.stdout.contains(&format!("bulk-1-{n:02}")),
            "{n}: {}",
            o.stdout
        );
    }
    let o = h.run(&["--where", "--all", "--key", &ka]);
    assert_eq!(o.code, 0, "{}", o.all());
    // Tenant A's index: its own unit and its sixty.
    assert!(
        o.stdout.contains(
            "61 unit(s) in this index; this tape holds the newest version of 1 of them whole."
        ),
        "{}",
        o.stdout
    );
    for n in 0..60 {
        assert!(o.stdout.contains(&format!(
            "Unit bulk-1-{n:02} (tenant {TENANT_A}): newest version"
        )));
    }
    // Version 10 is newer than version 2 by number.
    assert!(
        o.stdout.contains(
            "Unit bulk-1-00 (tenant loquat-alpha): newest version 10, NOT on this tape (whole)"
        ),
        "{}",
        o.stdout
    );
    assert!(o.stdout.contains("Version 10 (newest): "), "{}", o.stdout);
    assert!(
        o.stdout
            .contains("The newest version of these is NOT on this tape whole"),
        "{}",
        o.stdout
    );
    // The escrow key opens the operator's map: every tenant's units.
    let o = h.run(&["--where", "--all", "--key", &key(&h, Who::Escrow)]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(
        o.stdout.contains("62 unit(s) in this index"),
        "{}",
        o.stdout
    );
    assert!(o
        .stdout
        .contains(&format!("Unit {UNIT_B} (tenant {TENANT_B})")));
}

#[test]
fn a_tape_without_an_index_says_so_for_where_and_find() {
    let t = no_index();
    let h = Heir::of(t);
    let ka = key(&h, Who::TenantA);
    for args in [
        vec!["--where", UNIT_A, "--key", &ka],
        vec!["--where", "--all", "--key", &ka],
        vec!["--find", "deep", "--key", &ka],
    ] {
        let o = h.run(&args);
        assert_eq!(o.code, 1, "{args:?}: {}", o.all());
        assert!(
            o.stdout.contains("This tape carries no index"),
            "{args:?}: {}",
            o.stdout
        );
        assert!(o.stdout.contains("--find-envelope"), "{}", o.stdout);
    }
}

#[test]
fn find_prints_matching_paths_with_their_holders() {
    let t = with_holders();
    let h = Heir::of(t);
    let kb = key(&h, Who::TenantB);
    let o = h.run(&["--find", "mid-3", "--key", &kb]);
    assert_eq!(o.code, 0, "{}", o.all());
    for want in [
        &format!("In the index of tenant {TENANT_B}:"),
        &format!("  {UNIT_B}  v1  f  12000 bytes  inner/mid-3.dat"),
        &format!("  Tapes holding {UNIT_B} version 1:"),
        "    tape HEIR01  (this tape)",
        "    tape OLD01  (sealed 2026-10-01T00:00:00Z)",
        "--restore --key KEY --unit UNIT --path PATH",
    ] {
        assert!(o.stdout.contains(want), "{want}: {}", o.stdout);
    }
    assert!(!o.stdout.contains("mid-4"), "{}", o.stdout);
    let o = h.run(&["--find", "nowhere-at-all", "--key", &kb]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stdout.contains("No path in this index contains"),
        "{}",
        o.stdout
    );
}

/// The match is on the decoded path and a path is printed on one line, as
/// --list writes it (escaped, and so the form --path takes).
#[test]
fn find_matches_the_decoded_path_and_prints_it_on_one_line() {
    let t = two();
    let h = Heir::of(t);
    let kb = key(&h, Who::TenantB);
    let o = h.run(&["--find", "tab\tand\nnew", "--key", &kb]);
    assert_eq!(o.code, 0, "{}", o.all());
    let line = o
        .stdout
        .lines()
        .find(|l| l.contains("12") || l.contains("newline.txt"))
        .unwrap();
    assert!(line.ends_with("tab\\tand\\nnewline.txt"), "{line:?}");
    assert!(
        o.stdout.contains("Paths containing 'tab\\tand\\nnew'"),
        "{}",
        o.stdout
    );
    let o = h.run(&["--find", "odd\\back", "--key", &kb]);
    assert_eq!(o.code, 0, "{}", o.all());
    // The path is printed as --list prints it and as --path takes it: a backslash
    // is \\ (volume-format-v3.md 1.5; #596 6-6).
    assert!(o.stdout.contains("odd\\\\back name.txt"), "{}", o.stdout);
    assert!(!o.stdout.contains("odd\\back name.txt"), "{}", o.stdout);
}

#[test]
fn find_caps_its_matches_and_counts_the_rest_under_one_limit_for_every_tenant() {
    let t = bulk();
    let h = Heir::of(t);
    let o = h.run(&["--find", "bulk/", "--key", &key(&h, Who::TenantA)]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert_eq!(
        o.stdout
            .matches("  bulk  v2  f  10 bytes  bulk/file-")
            .count(),
        50,
        "{}",
        o.stdout
    );
    assert!(
        o.stdout.contains("10 more match(es) are not shown"),
        "{}",
        o.stdout
    );
    // One limit across the tenants the escrow key opens.
    let o = h.run(&["--find", "bulk/", "--key", &key(&h, Who::Escrow)]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert_eq!(
        o.stdout
            .matches("  bulk  v2  f  10 bytes  bulk/file-")
            .count(),
        50,
        "{}",
        o.stdout
    );
    assert!(
        o.stdout.contains("70 more match(es) are not shown"),
        "{}",
        o.stdout
    );
}

/// The index modes read forward from one rewind, as `--find-envelope` does,
/// and need exactly the tools that open an envelope.
#[test]
fn the_index_modes_read_forward_from_one_rewind_and_check_the_tools_they_run() {
    let t = with_holders();
    let h = Heir::of(t);
    let ka = key(&h, Who::TenantA);
    let o = h.run(&["--find-envelope", "--key", &ka]);
    assert_eq!(o.code, 0, "{}", o.all());
    let want = h.count_ops("mt rewind");
    for args in [
        vec!["--where", UNIT_A, "--key", &ka],
        vec!["--where", "--all", "--key", &ka],
        vec!["--find", "deep", "--key", &ka],
    ] {
        h.reset_ops();
        let o = h.run(&args);
        assert_eq!(o.code, 0, "{args:?}: {}", o.all());
        assert_eq!(h.count_ops("mt rewind"), want, "{args:?}: {:?}", h.ops());
        assert!(
            h.dd_reads()
                .iter()
                .all(|p| !t.slice_positions().contains(p)),
            "the index modes read no data slice: {:?}",
            h.ops()
        );
    }
    // A host with no `age` is refused before the tape is read; `--info` needs none.
    let h = Heir::of(t);
    let ka = key(&h, Who::TenantA);
    h.remove_tool("age");
    let o = h.run(&["--where", UNIT_A, "--key", &ka]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stderr.contains("missing required tool(s): age"),
        "{}",
        o.stderr
    );
    assert!(h.ops().is_empty(), "{:?}", h.ops());
}

/// On a tape that ended early, the version it holds only some of is marked a
/// range (the seal and the manifest say which slices are here), never a copy
/// of the whole version, by `--where`, `--where --all` and `--find`.
#[test]
fn where_and_find_on_a_range_tape_mark_this_tape_a_range_and_not_a_whole_copy() {
    let range = &whole_and_range()[1];
    let h = Heir::of(range);
    let kb = key(&h, Who::TenantB);
    let o = h.run(&["--where", UNIT_B, "--key", &kb]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(
        o.stdout
            .contains("tape HEIR02  (this tape, a range: not the whole version)  slices 1-1 of "),
        "{}",
        o.stdout
    );
    assert!(
        o.stdout.contains("holds only the slices it names"),
        "{}",
        o.stdout
    );
    let o = h.run(&["--where", "--all", "--key", &kb]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(
        o.stdout.contains(&format!(
            "Unit {UNIT_B} (tenant {TENANT_B}): newest version 1, NOT on this tape (whole)"
        )),
        "{}",
        o.stdout
    );
    assert!(
        o.stdout.contains("(this tape: slices 1-1 only)"),
        "{}",
        o.stdout
    );
    let o = h.run(&["--find", "notes", "--key", &kb]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(
        o.stdout
            .contains("this tape, a range: not the whole version"),
        "{}",
        o.stdout
    );
    // The whole copy of the same stagings is no range.
    let h = Heir::of(&whole_and_range()[0]);
    let o = h.run(&["--where", UNIT_B, "--key", &kb]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(!o.stdout.contains("a range"), "{}", o.stdout);
}

/// #596 (the "every slice, 1 to the last" concern): a tape that holds part of a
/// unit gives the files it holds whole and lists the rest (D17, ADR-0012 item
/// 266). The line after a range holder says that of the tape, and says the
/// whole VERSION is what needs every slice, in the words of Layout 3 and not
/// Layout 2's "a restore needs every slice".
#[test]
fn the_line_after_a_range_holder_does_not_state_layout_2s_rule() {
    let range = &whole_and_range()[1];
    let h = Heir::of(range);
    let kb = key(&h, Who::TenantB);
    let o = h.run(&["--where", UNIT_B, "--key", &kb]);
    assert_eq!(o.code, 0, "{}", o.all());
    let flat = o.stdout.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        flat.contains("a restore of the whole version needs every slice, 1 to the last"),
        "{}",
        o.stdout
    );
    assert!(
        flat.contains(
            "That tape alone restores the files whose slices it holds whole and lists the rest"
        ),
        "{}",
        o.stdout
    );
    assert!(
        !flat.contains("a restore needs every slice"),
        "{}",
        o.stdout
    );
}

/// BusyBox tar takes a member given whole, as the heir's path restore gives
/// it, and does not take a named folder with all that lies under it: a file,
/// a hard link with its first name, a folder and a long member's run all come
/// back, the repair included.
#[test]
fn busybox_tar_restores_named_paths_and_the_repair_from_members_given_whole() {
    let Some(bb) = optional_tool("busybox") else {
        return;
    };
    let busybox_tar = format!("#!/bin/sh\nexec {} tar \"$@\"\n", bb.display());
    let (t1, t2) = (&copies()[0], &copies()[1]);
    let h = Heir::of(t1);
    h.put_tool("tar", &busybox_tar);
    let kb = key(&h, Who::TenantB);
    let large = format!("{UNIT_B}-large.bin");
    let link = format!("{UNIT_B}-small.txt");
    let dest = h.dest("named");
    let o = h.run(&[
        "--restore",
        "--key",
        &kb,
        "--to",
        dest_str(&dest),
        "--unit",
        UNIT_B,
        "--path",
        &large,
        "--path",
        &link,
        "--path",
        "inner",
        "--path",
        "inner/mid-5.dat",
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(o.stdout.contains("CONTENT ONLY"), "{}", o.stdout);
    let got = heir::tree(&dest);
    for p in [large.as_str(), link.as_str(), "inner/mid-5.dat"] {
        let (_, bytes) = unit_files(t1, UNIT_B).iter().find(|(q, _)| q == p).unwrap();
        assert_eq!(got.get(p), Some(bytes), "{p}");
    }
    assert!(
        !got.contains_key("inner/mid-4.dat") && !got.contains_key("notes.txt"),
        "nothing under the named folder was extracted: {:?}",
        got.keys()
    );
    // The repair under BusyBox tar, from a copy with the slice of mid-1 lost.
    let h1 = Heir::of(t1);
    let rows = list_rows(&h1, &kb, UNIT_B);
    let (_, s, _, _) = row(&rows, "inner/mid-1.dat");
    h1.flip(slice_pos(t1, UNIT_B, *s), 500);
    let whole = h1.dest("whole");
    let o = h1.run(&["--restore", "--key", &kb, "--to", dest_str(&whole)]);
    assert_eq!(o.code, 1, "{}", o.all());
    let h2 = Heir::of(t2);
    h2.put_tool("tar", &busybox_tar);
    let o = h2.run(&[
        "--restore",
        "--key",
        &kb,
        "--to",
        dest_str(&whole),
        "--paths-from",
        whole.join(".tapectl-damaged-files.txt").to_str().unwrap(),
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert_restored(t1, UNIT_B, &whole);
}

// ---- what the review of this task added ----

/// A tape that holds a run of a version's slices, and not the whole of it,
/// is a range in the index, from the tape's own table.
#[test]
fn where_marks_an_earlier_tape_that_holds_a_run_of_slices_only_as_a_range() {
    let t = with_holders();
    let h = Heir::of(t);
    let ke = key(&h, Who::Escrow);
    let o = h.run(&["--where", UNIT_B, "--key", &ke]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(
        o.stdout.contains(
            "    tape OLD03  (sealed 2026-10-02T00:00:00Z, a range: not the whole version)  slices 1-1 of "
        ),
        "{}",
        o.stdout
    );
    assert!(
        o.stdout
            .contains("tape OLD01  (sealed 2026-10-01T00:00:00Z)"),
        "a whole holder is no range: {}",
        o.stdout
    );
    assert!(
        o.stdout.contains("holds only the slices it names"),
        "{}",
        o.stdout
    );
    let o = h.run(&["--where", "--all", "--key", &ke]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(o.stdout.contains("OLD03"), "{}", o.stdout);
    // A unit of one slice has no partial holder.
    let o = h.run(&["--where", UNIT_A, "--key", &ke]);
    assert!(!o.stdout.contains("OLD03"), "{}", o.stdout);
    let o = h.run(&["--find", "mid-3", "--key", &key(&h, Who::TenantB)]);
    assert!(o.stdout.contains("tape OLD03"), "{}", o.stdout);
    assert!(
        o.stdout.contains("holds only the slices it names"),
        "{}",
        o.stdout
    );
}

/// Three units, the tape ended early inside the big one's first slice: the
/// third unit has no slice on it.
fn three_short() -> &'static Tape {
    static T: OnceLock<Tape> = OnceLock::new();
    T.get_or_init(|| {
        let mut shape = Shape::two();
        shape.units.push(("charlie-unit".into(), TENANT_A, true));
        shape.warning_in_slice = Some(1);
        Tape::build(&shape)
    })
}

#[test]
fn a_unit_with_no_slice_on_a_short_tape_is_said_so_by_every_mode_that_knows() {
    let t = three_short();
    let h = Heir::of(t);
    let ka = key(&h, Who::TenantA);
    let o = h.run(&["--where", "charlie-unit", "--key", &ka]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(
        o.stdout
            .contains("no slice of it is on this tape (it ended before them)"),
        "{}",
        o.stdout
    );
    assert!(
        o.stdout.contains("a range: not the whole version"),
        "{}",
        o.stdout
    );
    let o = h.run(&["--where", "--all", "--key", &ka]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(
        o.stdout.contains("(this tape: no slice of it)"),
        "{}",
        o.stdout
    );
    let o = h.run(&["--find-envelope", "--key", &ka]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(
        o.stdout
            .contains("PART: unit charlie-unit version 1: this tape holds none of its "),
        "{}",
        o.stdout
    );
    let o = h.run(&["--list", "--key", &ka, "--unit", "charlie-unit"]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(
        o.stdout.contains("PART: this tape holds none of its "),
        "{}",
        o.stdout
    );
    // A path of it is not read: the tape does not hold it, and it is listed.
    let h = Heir::of(t);
    let dest = h.dest("none");
    let o = h.run(&[
        "--restore",
        "--key",
        &ka,
        "--to",
        dest_str(&dest),
        "--unit",
        "charlie-unit",
        "--path",
        "notes.txt",
    ]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stderr
            .contains("lie in slices this tape does not hold whole"),
        "{}",
        o.stderr
    );
    assert_eq!(damaged_list(&dest), vec!["notes.txt".to_string()]);
    assert!(data_reads(t, &h).is_empty(), "{:?}", h.ops());
}

/// `--find-envelope` and `--list` mark a unit the tape holds only part of.
#[test]
fn find_envelope_and_list_mark_a_unit_a_range_tape_holds_only_part_of() {
    let range = &whole_and_range()[1];
    let h = Heir::of(range);
    let kb = key(&h, Who::TenantB);
    let o = h.run(&["--find-envelope", "--key", &kb]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(
        o.stdout.contains(&format!(
            "PART: unit {UNIT_B} version 1: this tape holds only slices 1-1 of its "
        )),
        "{}",
        o.stdout
    );
    let o = h.run(&["--list", "--key", &kb, "--unit", UNIT_B]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(
        o.stdout.contains("PART: this tape holds only slices 1-1"),
        "{}",
        o.stdout
    );
    // The whole tape and the unit that is whole on the range tape say nothing.
    let o = h.run(&["--find-envelope", "--key", &key(&h, Who::TenantA)]);
    assert!(!o.stdout.contains("PART:"), "{}", o.stdout);
    let w = Heir::of(&whole_and_range()[0]);
    let o = w.run(&["--find-envelope", "--key", &key(&w, Who::TenantB)]);
    assert!(!o.stdout.contains("PART:"), "{}", o.stdout);
}

/// A refusal by the destination file system (tar's words, its exit status 2)
/// is not damage: the restored files check out, so the restore is complete
/// and no list is written. The per-file check decides, whole unit or path.
#[test]
fn a_destination_refusal_in_tars_words_is_no_damage_when_the_files_check_out() {
    let t = two();
    let real = heir::host_tool("tar").unwrap();
    let refusing = format!(
        "#!/bin/bash\n\"{}\" \"$@\"\nrc=$?\ncase \" $* \" in\n  *\" -C \"*\" --numeric-owner \"*)\n    echo 'tar: ./x: Cannot change ownership to uid 0: Operation not permitted' >&2\n    exit 2 ;;\nesac\nexit $rc\n",
        real.display()
    );
    let h = Heir::of(t);
    h.put_tool("tar", &refusing);
    let ka = key(&h, Who::TenantA);
    let dest = h.dest("whole");
    let o = h.run(&["--restore", "--key", &ka, "--to", dest_str(&dest)]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(o.stdout.contains("RESTORE COMPLETE"), "{}", o.stdout);
    assert!(o.stderr.contains("tar ended with status 2"), "{}", o.stderr);
    assert!(!dest.join(".tapectl-damaged-files.txt").exists());
    assert_restored(t, UNIT_A, &dest);
    let one = h.dest("one");
    let o = h.run(&[
        "--restore",
        "--key",
        &ka,
        "--to",
        dest_str(&one),
        "--unit",
        UNIT_A,
        "--path",
        "notes.txt",
        "--path",
        &format!("inner/{UNIT_A}-deep.dat"),
    ]);
    // (unit A has no notes.txt: refused before any read)
    assert_eq!(o.code, 1, "{}", o.all());
    let o = h.run(&[
        "--restore",
        "--key",
        &ka,
        "--to",
        dest_str(&one),
        "--unit",
        UNIT_A,
        "--path",
        &format!("inner/{UNIT_A}-deep.dat"),
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(!one.join(".tapectl-damaged-files.txt").exists());
}

/// `tail` cuts each member from its slice: the path modes name it in the tool
/// check, before the tape is read; a whole-unit restore does not need it.
#[test]
fn the_path_modes_check_for_tail_and_a_whole_restore_does_not_need_it() {
    let t = two();
    let h = Heir::of(t);
    h.remove_tool("tail");
    let ka = key(&h, Who::TenantA);
    let dest = h.dest("p");
    let o = h.run(&[
        "--restore",
        "--key",
        &ka,
        "--to",
        dest_str(&dest),
        "--unit",
        UNIT_A,
        "--path",
        "empty",
    ]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stderr.contains("missing required tool(s): tail"),
        "{}",
        o.stderr
    );
    assert!(h.ops().is_empty(), "{:?}", h.ops());
    let o = h.run(&["--restore", "--key", &ka, "--to", dest_str(&h.dest("w"))]);
    assert_eq!(o.code, 0, "{}", o.all());
}

/// A folder whose name holds a backslash is a folder, not a file the
/// destination already holds: the text off the tape reaches awk as it is.
#[test]
fn an_existing_folder_named_with_a_backslash_is_not_refused_as_an_existing_file() {
    let t = two();
    let h = Heir::of(t);
    let kb = key(&h, Who::TenantB);
    let dest = h.dest("exists");
    std::fs::create_dir_all(dest.join("dir\\one")).unwrap();
    let o = h.run(&[
        "--restore",
        "--key",
        &kb,
        "--to",
        dest_str(&dest),
        "--unit",
        UNIT_B,
        "--path",
        &escaped("dir\\one"),
        "--path",
        &escaped("dir\\one/leaf.txt"),
    ]);
    assert_eq!(o.code, 0, "an existing folder is no refusal: {}", o.all());
    assert!(dest.join("dir\\one/leaf.txt").is_file());
}

// ---- a unit across cartridges (the plan's T4.12) ----
//
// The tapes are written by tapectl's own pieces with the planner's selection
// (`staged_build_units_spanning`): the big unit has eight slices of 64 KiB,
// and its 150,000-byte file is a run of slices 2 to 4. `Span` gives each tape
// of a series the slice the drive's early warning comes in.

fn span_series(
    alg: &'static str,
    min_copies: i32,
    warns: Vec<Option<usize>>,
    no_index: bool,
) -> Vec<Tape> {
    let mut shape = Shape::one(alg);
    shape.no_index = no_index;
    shape.span = Some(heir::Span { min_copies, warns });
    Tape::build_series(&shape)
}

/// What each tape holds of the unit, `(first, last)` slice, as the seal covers it.
fn held(tapes: &[Tape]) -> Vec<(i64, i64)> {
    tapes
        .iter()
        .map(|t| {
            let r = &t.runs[0];
            (r.3, r.4)
        })
        .collect()
}

/// HEIR01 holds slices 1-3, HEIR02 4-5, HEIR03 6-8: the file of slices 2 to 4
/// lies across the first two cartridges.
fn span3() -> &'static [Tape] {
    static T: OnceLock<Vec<Tape>> = OnceLock::new();
    T.get_or_init(|| {
        let t = span_series("none", 1, vec![Some(2), Some(1), None], false);
        assert_eq!(held(&t), [(1, 3), (4, 5), (6, 8)]);
        t
    })
}

fn span3_alg(alg: &'static str) -> Vec<Tape> {
    let t = span_series(alg, 1, vec![Some(2), Some(1), None], false);
    assert_eq!(held(&t), [(1, 3), (4, 5), (6, 8)], "{alg}");
    t
}

/// HEIR01 holds slice 1 (the folder's dotfile, a symlink and the first name of
/// a hard link), HEIR02 slices 2-8, with the hard link's other name.
fn span2() -> &'static [Tape] {
    static T: OnceLock<Vec<Tape>> = OnceLock::new();
    T.get_or_init(|| {
        let t = span_series("none", 1, vec![Some(0), None], false);
        assert_eq!(held(&t), [(1, 1), (2, 8)]);
        t
    })
}

/// Item 93's two copies: copy 1 is HEIR01 (1-3) and HEIR03 (4-8); copy 2 is
/// HEIR02 (1-6) and HEIR04 (7-8).
fn copies2() -> &'static [Tape] {
    static T: OnceLock<Vec<Tape>> = OnceLock::new();
    T.get_or_init(|| {
        let t = span_series("none", 2, vec![Some(2), Some(5), None, None], false);
        assert_eq!(held(&t), [(1, 3), (1, 6), (4, 8), (7, 8)]);
        t
    })
}

/// The data slices of `t` the log of a run reads, in order.
fn slice_reads(t: &Tape, reads: &[usize]) -> Vec<usize> {
    // The seal of a tape that ended early lies at the place the plan gave a
    // slice after the last one it holds: it is no slice read.
    let all = t.slice_positions();
    let end = t.ended_after();
    reads
        .iter()
        .copied()
        .filter(|p| all.contains(p) && *p <= end)
        .collect()
}

/// The tape positions of slices `first..=last` of the unit on `t`.
fn positions(t: &Tape, first: usize, last: usize) -> Vec<usize> {
    let all = t.slice_positions();
    let base = t.runs[0].1 as usize;
    (first..=last).map(|n| all[n - base]).collect()
}

fn no_work_dir_in(dest: &Path) -> bool {
    std::fs::read_dir(dest).unwrap().all(|e| {
        !e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".tapectl-work")
    })
}

/// A `tar` that logs each run's arguments, then is the host's.
fn log_tar(h: &Heir) {
    let real = heir::host_tool("tar").unwrap();
    h.put_tool(
        "tar",
        &format!(
            "#!/bin/bash\nfor a; do case \"$a\" in --version) exec {real} \"$a\" ;; esac; done\necho \"$*\" >>\"$FAKE_TAPE/tar.log\"\nexec {real} \"$@\"\n",
            real = real.display()
        ),
    );
}

/// The runs of tar that extracted into `dest`.
fn tar_runs(h: &Heir, dest: &Path) -> usize {
    std::fs::read_to_string(h.root.path().join("state/tar.log"))
        .unwrap_or_default()
        .lines()
        .filter(|l| l.contains(&format!("-C {} ", dest.display())))
        .count()
}

fn restore_all<'a>(ka: &'a str, dest: &'a Path) -> Vec<&'a str> {
    vec!["--restore", "--key", ka, "--all", "--to", dest_str(dest)]
}

const LARGE: &str = "alpha-unit-large.bin";

/// Three tapes, two swaps, ONE tar run (item 267): the prompt asks for the
/// next cartridge after the eject line, each cartridge is read by the hash of
/// the slice needed, nothing past a seal is read, and the unit comes back
/// byte for byte, whatever the compression.
#[test]
fn a_unit_on_three_tapes_restores_across_two_swaps_as_one_run_for_each_algorithm() {
    for alg in ["none", "gzip", "xz", "zstd"] {
        let built;
        let tapes: &[Tape] = if alg == "none" {
            span3()
        } else {
            built = span3_alg(alg);
            &built
        };
        let h = Heir::of_series(&[&tapes[0], &tapes[1], &tapes[2]]);
        log_tar(&h);
        let dest = h.dest("out");
        let ka = key(&h, Who::TenantA);
        let o = h.run_prompted(&restore_all(&ka, &dest), |k, text| {
            assert!(k <= 2, "two cartridge changes: {text}");
            let eject = text
                .rfind("First eject the cartridge now in the drive")
                .unwrap();
            assert!(
                eject < text.rfind(heir::PROMPT).unwrap(),
                "eject first: {text}"
            );
            h.load(["HEIR02", "HEIR03"][k - 1]);
            Some(String::new())
        });
        assert_eq!(o.code, 0, "{alg}: {}", o.all());
        assert!(o.stdout.contains("RESTORE COMPLETE"), "{alg}: {}", o.stdout);
        assert_restored(&tapes[0], UNIT_A, &dest);
        assert_eq!(
            tar_runs(&h, &dest),
            1,
            "{alg}: one tar run across both swaps"
        );
        assert!(no_work_dir_in(&dest), "{alg}");
        assert!(!dest.join(".tapectl-damaged-files.txt").exists(), "{alg}");
        // Each cartridge gave the slices it holds and nothing past its seal.
        let by = h.reads_by_cartridge();
        assert_eq!(by.len(), 3, "{alg}: {:?}", h.ops());
        for (i, (a, z)) in [(1, 3), (4, 5), (6, 8)].into_iter().enumerate() {
            assert_eq!(
                slice_reads(&tapes[i], &by[i]),
                positions(&tapes[i], a, z),
                "{alg}: cartridge {i}"
            );
        }
        assert!(
            o.stdout.contains("This cartridge identifies as: HEIR02"),
            "{alg}"
        );
    }
}

/// Item 93's two copies, each across cartridges, and a mix of the two: the
/// order of the tapes is the copy's, and any tape that lists the slice's
/// sha256 inside its seal gives it.
#[test]
fn either_copy_restores_across_its_own_tapes_and_the_two_mixed() {
    let t = copies2();
    for (name, start, offers) in [
        ("copy 1", "HEIR01", vec!["HEIR03"]),
        ("copy 2", "HEIR02", vec!["HEIR04"]),
        ("mixed", "HEIR01", vec!["HEIR02", "HEIR04"]),
    ] {
        let mut series: Vec<&Tape> = t.iter().filter(|x| x.label == start).collect();
        series.extend(t.iter().filter(|x| x.label != start));
        let h = Heir::of_series(&series);
        let dest = h.dest("out");
        let ka = key(&h, Who::TenantA);
        let o = h.run_prompted(&restore_all(&ka, &dest), |k, text| {
            assert!(k <= offers.len(), "{name}: asked too often: {text}");
            h.load(offers[k - 1]);
            Some(String::new())
        });
        assert_eq!(o.code, 0, "{name}: {}", o.all());
        assert_restored(&t[0], UNIT_A, &dest);
        assert_eq!(h.reads_by_cartridge().len(), offers.len() + 1, "{name}");
    }
}

fn truth<'a>(t: &'a Tape, path: &str) -> &'a [u8] {
    &unit_files(t, UNIT_A)
        .iter()
        .find(|(p, _)| p == path)
        .unwrap_or_else(|| panic!("no file {path}"))
        .1
}

fn list_of(dest: &Path) -> Vec<String> {
    damaged_list(dest)
}

/// The file of slices 2 to 4 lies across the first two cartridges. The first
/// is read to its end, the prompt asks for the second, and the file is
/// restored whole through it; the third cartridge is not had, and the restore
/// goes on without it, listing what is in its slices.
#[test]
fn a_file_larger_than_a_slice_across_two_cartridges_restores_through_the_prompt() {
    let t = span3();
    let h = Heir::of_series(&[&t[0], &t[1], &t[2]]);
    let dest = h.dest("out");
    let ka = key(&h, Who::TenantA);
    let o = h.run_prompted(&restore_all(&ka, &dest), |k, _| match k {
        1 => {
            h.load("HEIR02");
            Some(String::new())
        }
        _ => Some("done".to_string()),
    });
    assert_eq!(o.code, 1, "{}", o.all());
    assert_eq!(
        std::fs::read(dest.join(LARGE)).unwrap(),
        truth(&t[0], LARGE)
    );
    let list = list_of(&dest);
    assert!(!list.iter().any(|p| p == LARGE), "{list:?}");
    for left in ["inner/mid-2.dat", "notes.txt"] {
        assert!(list.iter().any(|p| p == left), "{left}: {list:?}");
        assert!(!dest.join(left).exists(), "{left}");
    }
    // Slices 1 to 5 are all here.
    assert!(dest.join("alpha-unit-hardlink.txt").exists());
    assert!(dest.join("inner/mid-1.dat").exists());
    assert_eq!(o.stdout.matches(heir::PROMPT).count(), 2, "{}", o.stdout);
    assert!(no_work_dir_in(&dest));
}

/// The same file, with the second cartridge not had: "I do not have it" is an
/// answer, the restore goes on with what it has, and the file tar had begun
/// is removed and listed. The end of the input is the same answer.
#[test]
fn the_same_file_answered_i_do_not_have_it_is_listed_and_leaves_no_partial_file() {
    let t = span3();
    for answer in [Some("done"), None] {
        let h = Heir::of_series(&[&t[0], &t[1], &t[2]]);
        let dest = h.dest("out");
        let ka = key(&h, Who::TenantA);
        let mut asked = Vec::new();
        let o = h.run_prompted(&restore_all(&ka, &dest), |k, _| {
            asked.push(k);
            answer.map(str::to_string)
        });
        assert_eq!(o.code, 1, "{answer:?}: {}", o.all());
        assert_eq!(
            asked,
            vec![1],
            "no second prompt after the answer: {}",
            o.stdout
        );
        assert!(!dest.join(LARGE).exists(), "no partial file");
        let list = list_of(&dest);
        assert!(list.iter().any(|p| p == LARGE), "{list:?}");
        // What slice 1 holds is restored and checked.
        assert_eq!(
            std::fs::read(dest.join("alpha-unit-hardlink.txt")).unwrap(),
            truth(&t[0], "alpha-unit-hardlink.txt")
        );
        assert!(
            o.stdout.contains("NOT ON THIS CARTRIDGE: slices 4-8 of 8"),
            "{}",
            o.stdout
        );
        assert!(no_work_dir_in(&dest));
    }
}

/// A start on the second tape alone restores what it holds whole and lists
/// the rest, with the tape the index names for it; both measured limits are
/// printed. A second run on the first tape then completes the unit, and
/// leaves alone every file the first run restored.
#[test]
fn a_start_on_the_second_tape_alone_restores_what_it_holds_whole_and_lists_the_rest() {
    use std::os::unix::fs::MetadataExt;
    let t = span2();
    let h = Heir::of_series(&[&t[1], &t[0]]);
    let dest = h.dest("out");
    let ka = key(&h, Who::TenantA);
    let o = h.run(&restore_all(&ka, &dest));
    assert_eq!(o.code, 1, "{}", o.all());
    let all = o.all();
    for f in [
        ".tapectl-unit.toml",
        "a-symlink",
        "alpha-unit-hardlink.txt",
        "alpha-unit-small.txt",
    ] {
        assert!(
            list_of(&dest).iter().any(|p| p == f),
            "{f}: {:?}",
            list_of(&dest)
        );
        assert!(!dest.join(f).exists(), "{f} is not restored");
    }
    // Every other file came back whole.
    for (p, bytes) in unit_files(&t[0], UNIT_A) {
        if p == "alpha-unit-hardlink.txt" || p == "alpha-unit-small.txt" {
            continue;
        }
        assert_eq!(&std::fs::read(dest.join(p)).unwrap(), bytes, "{p}");
    }
    // Where the rest is: the index names the first tape and its run; and the
    // two limits, and the hard-linked name with the tape of its first name.
    for needle in [
        "NOT ON THIS CARTRIDGE: slices 1 of 8",
        "tape HEIR01 (cartridge serial not recorded): slices 1 to 1",
        "a folder's modification time is that of the last run that touched the folder",
        "a hard-linked name whose first name was not read is not made until that name is",
        "alpha-unit-small.txt -> first name alpha-unit-hardlink.txt, slice 1",
    ] {
        assert!(all.contains(needle), "{needle:?}:\n{all}");
    }
    let before: Vec<_> = ["notes.txt", LARGE]
        .iter()
        .map(|f| {
            let m = std::fs::metadata(dest.join(f)).unwrap();
            (m.ino(), m.mtime(), m.mtime_nsec())
        })
        .collect();

    // The second run, on the first tape, into the same --to.
    h.load("HEIR01");
    h.reset_ops();
    let o = h.run(&restore_all(&ka, &dest));
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(o.stdout.contains("RESTORE COMPLETE"), "{}", o.all());
    assert_restored(&t[0], UNIT_A, &dest);
    assert!(!dest.join(".tapectl-damaged-files.txt").exists());
    assert!(no_work_dir_in(&dest));
    let after: Vec<_> = ["notes.txt", LARGE]
        .iter()
        .map(|f| {
            let m = std::fs::metadata(dest.join(f)).unwrap();
            (m.ino(), m.mtime(), m.mtime_nsec())
        })
        .collect();
    assert_eq!(before, after, "a file that passes is not rewritten");
    // Only the first tape's own slice was read, and the hard link is one file.
    assert_eq!(slice_reads(&t[0], &h.dd_reads()), positions(&t[0], 1, 1));
    let a = std::fs::metadata(dest.join("alpha-unit-hardlink.txt")).unwrap();
    let b = std::fs::metadata(dest.join("alpha-unit-small.txt")).unwrap();
    assert_eq!(a.ino(), b.ino(), "the two names are one file");
}

/// HEIR01 holds slices 1-3, HEIR02 slices 4-8: the file of slices 2 to 4 lies
/// across the two cartridges, and its tail is in slice 4.
fn span_long() -> &'static [Tape] {
    static T: OnceLock<Vec<Tape>> = OnceLock::new();
    T.get_or_init(|| {
        let t = span_series("none", 1, vec![Some(2), None], false);
        assert_eq!(held(&t), [(1, 3), (4, 8)]);
        t
    })
}

/// A start on the later tape, answered "I do not have it" for slice 1: the
/// closing line names only the slices this tape's MANIFEST gives no place
/// (1-3). Slice 4 is here, and is left out for its own reason (the long
/// member's first slice is bad), kept in the warning list and not in that line.
#[test]
fn a_start_on_the_later_tape_names_as_not_on_this_cartridge_only_the_slices_it_does_not_hold() {
    let t = span_long();
    let h = Heir::of_series(&[&t[1], &t[0]]);
    let dest = h.dest("out");
    let ka = key(&h, Who::TenantA);
    let o = h.run_prompted(&restore_all(&ka, &dest), |_, _| Some("done".to_string()));
    assert_eq!(o.code, 1, "{}", o.all());
    let all = o.all();
    assert!(
        o.stdout
            .contains("NOT ON THIS CARTRIDGE: slices 1-3 of 8 of unit"),
        "{all}"
    );
    assert!(!all.contains("slices 1-4 of 8"), "{all}");
    assert!(
        all.contains("slice 4: part of the long member whose first slice is bad"),
        "{all}"
    );
    assert!(list_of(&dest).iter().any(|p| p == LARGE));
    assert!(!dest.join(LARGE).exists());
}

/// The second run, on the first tape, into the same --to: the file whose tail
/// is on the other cartridge is asked for through the one prompt, and the run
/// completes the unit without rewriting a file that passes.
#[test]
fn a_second_run_asks_for_the_cartridge_a_file_across_the_cartridges_needs() {
    use std::os::unix::fs::MetadataExt;
    let t = span_long();
    let h = Heir::of_series(&[&t[1], &t[0]]);
    let dest = h.dest("out");
    let ka = key(&h, Who::TenantA);
    let o = h.run_prompted(&restore_all(&ka, &dest), |_, _| Some("done".to_string()));
    assert_eq!(o.code, 1, "{}", o.all());
    let before = std::fs::metadata(dest.join("notes.txt")).unwrap();
    let before = (before.ino(), before.mtime(), before.mtime_nsec());
    h.load("HEIR01");
    let mut asked = Vec::new();
    let o = h.run_prompted(&restore_all(&ka, &dest), |k, _| {
        asked.push(k);
        h.load("HEIR02");
        Some(String::new())
    });
    assert_eq!(o.code, 0, "{}", o.all());
    assert_eq!(asked, vec![1], "{}", o.all());
    assert!(o.stdout.contains("RESTORE COMPLETE"), "{}", o.all());
    assert_restored(&t[0], UNIT_A, &dest);
    assert!(!dest.join(".tapectl-damaged-files.txt").exists());
    assert!(no_work_dir_in(&dest));
    let after = std::fs::metadata(dest.join("notes.txt")).unwrap();
    assert_eq!(
        before,
        (after.ino(), after.mtime(), after.mtime_nsec()),
        "a file that passes is not rewritten"
    );
}

/// The same second run, answered "I do not have it": the file stays listed,
/// nothing partial is left, and the files the first run restored stay.
#[test]
fn a_second_run_answered_i_do_not_have_it_leaves_the_file_listed_and_no_partial() {
    let t = span_long();
    let h = Heir::of_series(&[&t[1], &t[0]]);
    let dest = h.dest("out");
    let ka = key(&h, Who::TenantA);
    let o = h.run_prompted(&restore_all(&ka, &dest), |_, _| Some("done".to_string()));
    assert_eq!(o.code, 1, "{}", o.all());
    h.load("HEIR01");
    let mut asked = 0;
    let o = h.run_prompted(&restore_all(&ka, &dest), |_, _| {
        asked += 1;
        Some("done".to_string())
    });
    assert_eq!(o.code, 1, "{}", o.all());
    assert_eq!(asked, 1, "{}", o.all());
    assert!(!dest.join(LARGE).exists(), "no partial file");
    let list = list_of(&dest);
    assert_eq!(list, vec![LARGE.to_string()], "{list:?}");
    assert!(dest.join("notes.txt").exists());
    assert!(no_work_dir_in(&dest));
}

/// The hard-linked name the first run listed (its first name is on the other
/// tape) is restored by the repair once that tape is read: the list the first
/// run wrote is the repair's input.
#[test]
fn a_hard_linked_name_listed_by_the_first_run_is_restored_by_the_repair() {
    use std::os::unix::fs::MetadataExt;
    let t = span2();
    let h = Heir::of_series(&[&t[1], &t[0]]);
    let dest = h.dest("out");
    let ka = key(&h, Who::TenantA);
    let o = h.run(&restore_all(&ka, &dest));
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(list_of(&dest).iter().any(|p| p == "alpha-unit-small.txt"));
    assert!(!dest.join("alpha-unit-small.txt").exists());
    h.load("HEIR01");
    h.reset_ops();
    let list = dest.join(".tapectl-damaged-files.txt");
    let o = h.run(&[
        "--restore",
        "--key",
        &ka,
        "--to",
        dest_str(&dest),
        "--paths-from",
        dest_str(&list),
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(o.stdout.contains("RESTORE COMPLETE"), "{}", o.all());
    assert_restored(&t[0], UNIT_A, &dest);
    assert!(!list.exists(), "nothing is left to repair");
    let a = std::fs::metadata(dest.join("alpha-unit-hardlink.txt")).unwrap();
    let b = std::fs::metadata(dest.join("alpha-unit-small.txt")).unwrap();
    assert_eq!(a.ino(), b.ino(), "the two names are one file");
}

/// The prompt reads from the input with no time limit and holds no device: no
/// `mt` or `dd` starts while it waits, and an answer that comes late is taken.
#[test]
fn the_prompt_waits_without_a_time_limit_and_holds_no_device() {
    let t = span3();
    let h = Heir::of_series(&[&t[0], &t[1], &t[2]]);
    let dest = h.dest("out");
    let ka = key(&h, Who::TenantA);
    let o = h.run_prompted(&restore_all(&ka, &dest), |k, _| {
        if k == 1 {
            let before = h.ops();
            std::thread::sleep(std::time::Duration::from_secs(3));
            assert_eq!(h.ops(), before, "no mt or dd while the prompt waits");
            h.load("HEIR02");
            Some(String::new())
        } else {
            h.load("HEIR03");
            Some(String::new())
        }
    });
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(
        o.stdout.contains("This waits for as long as you take."),
        "{}",
        o.stdout
    );
}

/// Nothing is kept in the work directory between cartridges (item 267): at
/// the prompt no slice, ciphertext or plain, waits there.
#[test]
fn nothing_of_a_slice_waits_in_the_work_directory_at_the_prompt() {
    let t = span3();
    let h = Heir::of_series(&[&t[0], &t[1], &t[2]]);
    let dest = h.dest("out");
    let ka = key(&h, Who::TenantA);
    let o = h.run_prompted(&restore_all(&ka, &dest), |k, _| {
        let work = std::fs::read_dir(&dest)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".tapectl-work")
            })
            .expect("the work directory");
        for e in std::fs::read_dir(&work).unwrap() {
            let e = e.unwrap();
            let name = e.file_name().to_string_lossy().into_owned();
            assert!(
                !name.starts_with("slice."),
                "{name} waits in the work directory at prompt {k}"
            );
        }
        h.load(["HEIR02", "HEIR03"][k - 1]);
        Some(String::new())
    });
    assert_eq!(o.code, 0, "{}", o.all());
}

/// `quit` ends the restore: files restored and checked stay, the file tar
/// had begun is removed, the work directory is gone, and the exit is 1.
#[test]
fn quitting_at_the_prompt_keeps_what_is_checked_and_removes_what_tar_had_begun() {
    let t = span3();
    let h = Heir::of_series(&[&t[0], &t[1], &t[2]]);
    let dest = h.dest("out");
    let ka = key(&h, Who::TenantA);
    let o = h.run_prompted(&restore_all(&ka, &dest), |_, _| Some("quit".to_string()));
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stdout.contains("RESTORE STOPPED at your request"),
        "{}",
        o.stdout
    );
    assert_eq!(o.stdout.matches(heir::PROMPT).count(), 1, "{}", o.stdout);
    assert!(
        !dest.join(LARGE).exists(),
        "the file tar had begun is removed"
    );
    assert_eq!(
        std::fs::read(dest.join("alpha-unit-hardlink.txt")).unwrap(),
        truth(&t[0], "alpha-unit-hardlink.txt")
    );
    assert!(list_of(&dest).iter().any(|p| p == LARGE));
    assert!(no_work_dir_in(&dest));
}

/// Ctrl-C (INT), a hangup (HUP) and a kill (TERM), at the prompt and in the
/// middle of reading a slice, leave no work directory and no partial file.
/// The signal goes to the script alone.
#[test]
fn a_signal_at_the_prompt_or_mid_slice_leaves_no_work_directory_and_no_partial_file() {
    let t = span3();
    for (sig, code) in [("INT", 130), ("HUP", 129), ("TERM", 143)] {
        for mid in [false, true] {
            let h = Heir::of_series(&[&t[0], &t[1], &t[2]]);
            let dest = h.dest("out");
            let ka = key(&h, Who::TenantA);
            // Mid-slice: the read of slice 3, the second piece of the long
            // file, takes a few seconds (the signal is taken when it ends).
            let slow = format!("{}:3", positions(&t[0], 3, 3)[0]);
            let envs: Vec<(&str, &str)> = if mid {
                vec![("FAKE_DD_SLOW", slow.as_str())]
            } else {
                vec![]
            };
            let args = restore_all(&ka, &dest);
            let mut r = h.spawn(&args, &envs);
            if mid {
                let until = std::time::Instant::now() + std::time::Duration::from_secs(60);
                while !h.root.path().join("state/dd.slow").exists() {
                    assert!(std::time::Instant::now() < until, "{}", r.text());
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            } else {
                r.wait_for(heir::PROMPT, 1, 60);
            }
            // Positive control: tar has begun the long file.
            let until = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while !dest.join(LARGE).exists() {
                assert!(
                    std::time::Instant::now() < until,
                    "tar began nothing: {}",
                    r.text()
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            r.signal(sig);
            let o = r.finish();
            let what = format!("SIG{sig} mid-slice {mid}: {}", o.all());
            assert_eq!(o.code, code, "{what}");
            assert!(no_work_dir_in(&dest), "{what}");
            assert!(!dest.join(LARGE).exists(), "no partial file: {what}");
        }
    }
}

/// #596 C-1: `prepare_slice` sets the slice being fed to k+1 as soon as slice
/// k's `cat` has returned, while tar may still be reading the last bytes of k.
/// A signal then must not leave the last member of slice k on disk short.
/// The tar stand-in lags a block behind, so that member is cut short when the
/// slow read of slice k+1 is interrupted; after it, every file that is there
/// is whole (volume-format-v3.md 7.4: Ctrl-C leaves no partial file).
#[test]
fn a_signal_just_after_a_slice_was_fed_leaves_no_partial_file_from_that_slice() {
    let t = two();
    let python = heir::host_tool("python3").expect("python3 is needed (first-run.sh needs it too)");
    let real = heir::host_tool("tar").unwrap();
    let truth: std::collections::BTreeMap<&str, &[u8]> = t
        .units
        .iter()
        .find(|u| u.name == UNIT_B)
        .unwrap()
        .files
        .iter()
        .map(|(p, b)| (p.as_str(), b.as_slice()))
        .collect();
    // The slices a plain run of the unit reads, in order.
    let h = Heir::of(t);
    let kb = key(&h, Who::TenantB);
    let plain = h.dest("plain");
    let o = h.run(&[
        "--restore",
        "--key",
        &kb,
        "--to",
        plain.to_str().unwrap(),
        "--unit",
        UNIT_B,
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    let all = t.slice_positions();
    let read: Vec<usize> = h
        .dd_reads()
        .into_iter()
        .filter(|p| all.contains(p))
        .collect();
    assert!(read.len() > 2, "{read:?}");
    for slow in &read[1..] {
        let h = Heir::of(t);
        let dest = h.dest("out");
        h.put_tool("tar", &heir::lagging_tar(&real, &python, &dest));
        let kb = key(&h, Who::TenantB);
        let spec = format!("{slow}:2");
        let args = [
            "--restore",
            "--key",
            &kb,
            "--to",
            dest.to_str().unwrap(),
            "--unit",
            UNIT_B,
        ];
        let mut r = h.spawn(&args, &[("FAKE_DD_SLOW", spec.as_str())]);
        let until = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !h.root.path().join("state/dd.slow").exists() {
            assert!(std::time::Instant::now() < until, "{}", r.text());
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        r.signal("INT");
        let o = r.finish();
        let what = format!("SIGINT in the read of tape file {slow}: {}", o.all());
        assert_eq!(o.code, 130, "{what}");
        assert!(no_work_dir_in(&dest), "{what}");
        // A file the unit's truth does not list (its dotfile) is not judged.
        for (path, bytes) in heir::tree(&dest) {
            if let Some(whole) = truth.get(path.as_str()) {
                assert_eq!(*whole, bytes.as_slice(), "partial {path:?}: {what}");
            }
        }
    }
}

/// R8: a cartridge is taken only by the sha256 of the slice needed, listed in
/// its front index at a position its own seal covers. Each wrong cartridge is
/// named, the prompt asks again, and the right one is taken at the end: the
/// same cartridge again, a blank one, another layout's, one that is not
/// tapectl's, one the drive cannot read, one with no readable seal, one that
/// lists the slice past its seal, and one that lists no such slice. A
/// cartridge whose front index is unreadable is read by the seal's copy.
///
/// File 0 is no part of that rule (ADR-0012 item 313): a cartridge whose File
/// 0 alone does not read, or states nothing, is taken or refused by its front
/// index and seal marker (`tests/heir_restore_s6.rs`). So the cartridge that
/// is not tapectl's is one NO file of which is tapectl's, and the one the
/// drive cannot read gives neither its File 0, its front index nor its seal.
#[test]
fn a_cartridge_is_taken_only_by_the_next_slices_hash_at_a_position_its_seal_covers() {
    let t = span3();
    let h = Heir::of_series(&[&t[0], &t[1], &t[2]]);
    h.shelve_blank("BLANK");
    let mut f = t[2].files.clone();
    f[0] = file_0_with(&t[2], |s| {
        s.replace("tapectl-volume-v3", "tapectl-volume-v2")
            .replace("layout_version = 3", "layout_version = 2")
    });
    h.shelve("LAYOUT2", &f);
    let mut f: Vec<Vec<u8>> = t[2]
        .files
        .iter()
        .map(|_| b"a file of some other tape".to_vec())
        .collect();
    f[0] = b"the first file of some other tape".to_vec();
    h.shelve("NOTTAPE", &f);
    h.copy_cart("HEIR02", "UNREAD");
    for n in [0, 3, t[1].files.len() - 1] {
        h.fail_file("UNREAD", n);
    }
    let mut f = t[1].files.clone();
    let last = f.len() - 1;
    f[last] = b"junk where the seal marker was".to_vec();
    h.shelve("NOSEAL", &f);
    // The first tape's own plan lists slice 4 past its seal; this copy is a
    // different cartridge (another uuid).
    let mut f = t[0].files.clone();
    f[0] = file_0_with(&t[0], |s| {
        s.replace(&t[0].volume_uuid, "00000000-0000-4000-8000-00000000f0f0")
    });
    h.shelve("PAST", &f);
    let mut f = t[1].files.clone();
    f[3] = b"the front index is gone".to_vec();
    h.shelve("RUNG2", &f);
    let offers = [
        "HEIR01", "BLANK", "LAYOUT2", "NOTTAPE", "UNREAD", "NOSEAL", "PAST", "HEIR03", "RUNG2",
        "HEIR03",
    ];
    let dest = h.dest("out");
    let ka = key(&h, Who::TenantA);
    let o = h.run_prompted(&restore_all(&ka, &dest), |k, _| {
        assert!(k <= offers.len(), "asked too often");
        h.load(offers[k - 1]);
        Some(String::new())
    });
    assert_eq!(o.code, 0, "{}", o.all());
    assert_restored(&t[0], UNIT_A, &dest);
    let all = o.all();
    assert_eq!(all.matches(heir::PROMPT).count(), offers.len(), "{all}");
    for (needle, n) in [
        ("is the same cartridge as the one already in the drive", 1),
        ("this cartridge is blank", 1),
        ("a tapectl volume of Layout 2", 1),
        ("this cartridge is not a tapectl volume", 1),
        ("this cartridge is unreadable: its first file", 1),
        ("Its seal marker cannot be read", 1),
        ("but the tape ended after file", 1),
        ("lists no data slice with the sha256 of slice 4", 1),
        (
            "the front index of this cartridge is unreadable: using the seal marker's copy",
            1,
        ),
    ] {
        assert_eq!(all.matches(needle).count(), n, "{needle:?}:\n{all}");
    }
    assert!(!all.contains("FATAL"), "{all}");
    // Nothing is read from a cartridge not taken but File 0, the front index
    // and the seal (R5): never one of its slices.
    let by = h.reads_by_cartridge();
    // File 0, File 3 and the last file (the seal, where there is one).
    let ends: Vec<usize> = t.iter().map(|x| x.files.len() - 1).collect();
    for (i, reads) in by.iter().enumerate().skip(1).take(7) {
        assert!(
            reads.iter().all(|p| *p == 0 || *p == 3 || ends.contains(p)),
            "refused cartridge {i} was read past its seal: {reads:?}"
        );
    }
}

/// Where the rest is, by the index of the tape the restore started on. From
/// the last tape: the index names the earlier ones, each with its run. From
/// the first: a tape never names one written after it, so the prompt says
/// so and sends to `--where` on the newest tape. With no index: that is said.
#[test]
fn the_prompt_names_the_tapes_the_index_names_and_says_where_to_look_when_it_names_none() {
    let t = span3();
    // From the last tape.
    let h = Heir::of_series(&[&t[2], &t[0], &t[1]]);
    let dest = h.dest("out");
    let ka = key(&h, Who::TenantA);
    let o = h.run(&restore_all(&ka, &dest));
    assert_eq!(o.code, 1, "{}", o.all());
    let all = o.all();
    for needle in [
        "needs slice 1 of 8 next",
        "tape HEIR01 (cartridge serial not recorded): slices 1 to 3",
        "NOT ON THIS CARTRIDGE: slices 1-5 of 8",
        "tape HEIR02 (cartridge serial not recorded): slices 4 to 5",
    ] {
        assert!(all.contains(needle), "{needle:?}:\n{all}");
    }
    // The last tape's own files came back whole, and nothing else.
    assert_eq!(
        std::fs::read(dest.join("notes.txt")).unwrap(),
        truth(&t[0], "notes.txt")
    );
    assert!(!dest.join(LARGE).exists());
    // From the first.
    let h = Heir::of_series(&[&t[0]]);
    let dest = h.dest("out");
    let o = h.run(&restore_all(&key(&h, Who::TenantA), &dest));
    let all = o.all();
    assert!(
        all.contains("No tape the index in this tape's envelope names holds slice 4")
            && all.contains("--where alpha-unit --key KEY on the newest tape you hold names them"),
        "{all}"
    );
    // With no index.
    let no_index = span_series("none", 1, vec![Some(2), Some(1), None], true);
    let h = Heir::of_series(&[&no_index[0]]);
    let dest = h.dest("out");
    let o = h.run(&restore_all(&key(&h, Who::TenantA), &dest));
    let all = o.all();
    assert!(
        all.contains("This tape's envelope carries no index, so no tape is named"),
        "{all}"
    );
    assert!(
        all.contains("carries no index, so no tape is named: "),
        "{all}"
    );
}

/// A start on the middle tape: the file that began on the first tape is not
/// restored (its first slice is not here), the files of its own slices are.
#[test]
fn a_start_on_the_middle_tape_skips_the_file_begun_before_it_and_restores_the_rest() {
    let t = span3();
    let h = Heir::of_series(&[&t[1], &t[0], &t[2]]);
    let dest = h.dest("out");
    let o = h.run(&restore_all(&key(&h, Who::TenantA), &dest));
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(o.all().contains("needs slice 1 of 8 next"), "{}", o.all());
    assert!(!dest.join(LARGE).exists(), "no piece of the long file");
    assert!(list_of(&dest).iter().any(|p| p == LARGE));
    // Slice 5 holds the folders, the link and the first mid files.
    assert_eq!(
        std::fs::read(dest.join("inner/mid-1.dat")).unwrap(),
        truth(&t[0], "inner/mid-1.dat")
    );
    assert!(no_work_dir_in(&dest));
}

/// `--path` across cartridges: the file lies in slices 2 to 4. Started on the
/// first tape it reads slices 2 and 3 there and asks for the cartridge that
/// holds slice 4 and no other; started on the second it asks for the one that
/// holds slice 2, then for the second again.
#[test]
fn a_path_across_cartridges_asks_for_the_cartridge_that_holds_each_slice() {
    let t = span3();
    for (start, offers) in [
        ("HEIR01", vec!["HEIR02"]),
        ("HEIR02", vec!["HEIR01", "HEIR02"]),
    ] {
        let mut series: Vec<&Tape> = t.iter().filter(|x| x.label == start).collect();
        series.extend(t.iter().filter(|x| x.label != start));
        let h = Heir::of_series(&series);
        let dest = h.dest("out");
        let ka = key(&h, Who::TenantA);
        let o = h.run_prompted(
            &[
                "--restore",
                "--key",
                &ka,
                "--unit",
                UNIT_A,
                "--path",
                LARGE,
                "--to",
                dest_str(&dest),
            ],
            |k, text| {
                assert!(k <= offers.len(), "{start}: asked too often: {text}");
                h.load(offers[k - 1]);
                Some(String::new())
            },
        );
        assert_eq!(o.code, 0, "{start}: {}", o.all());
        assert_eq!(
            std::fs::read(dest.join(LARGE)).unwrap(),
            truth(&t[0], LARGE)
        );
        let others: Vec<String> = std::fs::read_dir(&dest)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n != LARGE)
            .collect();
        assert!(
            others.is_empty(),
            "{start}: only the named file: {others:?}"
        );
        let by = h.reads_by_cartridge();
        assert_eq!(by.len(), offers.len() + 1, "{start}: {:?}", h.ops());
        if start == "HEIR01" {
            // The second cartridge gives its slice 4 and nothing else.
            assert_eq!(slice_reads(&t[1], &by[1]), positions(&t[1], 4, 4));
        }
    }
}

/// The repair asks for a cartridge too: a file that lies across two
/// cartridges is on the list of a run that had only one of them, and the
/// repair, given both in turn, restores it whole.
#[test]
fn the_repair_asks_for_the_cartridge_a_listed_file_needs() {
    let t = span3();
    let h = Heir::of_series(&[&t[1], &t[0], &t[2]]);
    let dest = h.dest("out");
    let ka = key(&h, Who::TenantA);
    let o = h.run(&restore_all(&ka, &dest));
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(list_of(&dest).iter().any(|p| p == LARGE));
    h.load("HEIR01");
    let list = dest.join(".tapectl-damaged-files.txt");
    let o = h.run_prompted(
        &[
            "--restore",
            "--key",
            &ka,
            "--to",
            dest_str(&dest),
            "--paths-from",
            dest_str(&list),
        ],
        |k, text| {
            assert!(k <= 3, "{text}");
            // The listed files lie in slices 1-3 and 6-8: slice 4 (the last
            // piece of the long file) comes from HEIR02, slice 6 on from
            // HEIR03.
            h.load(["HEIR02", "HEIR03", "HEIR03"][k - 1]);
            Some(String::new())
        },
    );
    assert_eq!(o.code, 0, "{}", o.all());
    assert_restored(&t[0], UNIT_A, &dest);
    assert!(!list.exists());
}

/// BusyBox tar across a swap: one tar run of three cartridges' slices.
#[test]
fn busybox_tar_restores_across_the_swaps() {
    let Some(bb) = optional_tool("busybox") else {
        return;
    };
    let t = span3();
    let h = Heir::of_series(&[&t[0], &t[1], &t[2]]);
    h.put_tool(
        "tar",
        &format!("#!/bin/sh\nexec {} tar \"$@\"\n", bb.display()),
    );
    let dest = h.dest("out");
    let ka = key(&h, Who::TenantA);
    let o = h.run_prompted(&restore_all(&ka, &dest), |k, _| {
        h.load(["HEIR02", "HEIR03"][k - 1]);
        Some(String::new())
    });
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(o.stdout.contains("CONTENT ONLY"), "{}", o.stdout);
    assert_restored(&t[0], UNIT_A, &dest);
}

// ---- a name that is not UTF-8 (ADR-0012 item 297, #569; the plan's T4.17) ----
//
// The big unit of a `raw_names` tape holds a Latin-1 file, a folder so named with
// a file in it, a hard link and two symbolic links (one whose target is such a
// name, one whose target is not UTF-8 and is nowhere), and a UTF-8 file whose
// name holds the TEXT `\xe9`. Every name is a real one on this host's filesystem.

fn raw_shape() -> Shape {
    let mut shape = Shape::two();
    shape.raw_names = true;
    shape
}

fn raw() -> &'static Tape {
    static T: OnceLock<Tape> = OnceLock::new();
    T.get_or_init(|| Tape::build(&raw_shape()))
}

fn raw_copies() -> &'static [Tape] {
    static T: OnceLock<Vec<Tape>> = OnceLock::new();
    T.get_or_init(|| {
        let mut shape = raw_shape();
        shape.copies = 2;
        Tape::build_series(&shape)
    })
}

fn os(bytes: &[u8]) -> &std::ffi::OsStr {
    std::os::unix::ffi::OsStrExt::from_bytes(bytes)
}

/// A name's bytes as `--list` and the damaged-file list write them.
fn escaped_bytes(name: &[u8]) -> String {
    tapectl::volume::index::escape_bytes(name).into_owned()
}

/// Every raw-named regular file of unit B came back, byte for byte.
fn assert_raw_restored(t: &Tape, dest: &Path, except: &[&[u8]]) {
    let got = heir::tree_bytes(dest);
    let truth = &t.units.iter().find(|u| u.name == UNIT_B).unwrap().raw_files;
    assert!(
        !truth.is_empty(),
        "positive control: the tape has raw names"
    );
    for (name, bytes) in truth {
        if except.contains(&name.as_slice()) {
            continue;
        }
        assert_eq!(
            got.get(name),
            Some(bytes),
            "{} came back byte for byte",
            escaped_bytes(name)
        );
    }
}

/// `--list` prints the names escaped; a whole restore gives them back as bytes,
/// the hard link is still one file and the symbolic links' targets are right;
/// the per-file check, which unescapes the table's path to name the file, passes.
#[test]
fn a_unit_of_names_that_are_not_utf8_is_listed_escaped_and_restores_byte_for_byte() {
    let t = raw();
    let h = Heir::of(t);
    let kb = key(&h, Who::TenantB);
    let rows = list_rows(&h, &kb, UNIT_B);
    for name in [
        &b"caf\xe9.txt"[..],
        b"dir\xe9",
        b"dir\xe9/in\xff.bin",
        b"hard\xe9-link",
        b"lit\\xe9.txt",
        b"sym\xe9-link",
    ] {
        let esc = escaped_bytes(name);
        assert!(rows.iter().any(|r| r.3 == esc), "{esc:?} in {rows:?}");
    }
    // The two spellings are two names: the byte and the text.
    assert_eq!(escaped_bytes(b"caf\xe9.txt"), "caf\\xe9.txt");
    assert_eq!(escaped_bytes(b"lit\\xe9.txt"), "lit\\\\xe9.txt");
    let o = h.run(&["--list", "--key", &kb, "--unit", UNIT_B]);
    assert!(
        o.stdout.contains("a backslash in a name is \\\\"),
        "--list says how a path is typed: {}",
        o.stdout
    );
    let dest = h.dest("whole");
    let o = h.run(&[
        "--restore",
        "--key",
        &kb,
        "--to",
        dest_str(&dest),
        "--unit",
        UNIT_B,
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(!dest.join(".tapectl-damaged-files.txt").exists());
    assert_raw_restored(t, &dest, &[]);
    use std::os::unix::fs::MetadataExt;
    let ino = |n: &[u8]| std::fs::metadata(dest.join(os(n))).unwrap().ino();
    assert_eq!(
        ino(b"caf\xe9.txt"),
        ino(b"hard\xe9-link"),
        "one file, two names"
    );
    assert_eq!(
        std::fs::read_link(dest.join(os(b"sym\xe9-link"))).unwrap(),
        std::path::PathBuf::from(os(b"caf\xe9.txt"))
    );
    assert_eq!(
        std::fs::read_link(dest.join(os(b"to-\xfe\xfd-target"))).unwrap(),
        std::path::PathBuf::from(os(b"\xff\xfegone"))
    );
    assert!(
        o.stdout.contains("File check: all"),
        "the per-file check ran over the names: {}",
        o.stdout
    );
}

/// `--path` takes the escaped form `--list` prints: the byte name, the folder,
/// the name with the TEXT `\xe9`, and a UTF-8 name with a backslash (`\\`). The
/// raw form of a backslash name is not a name in the table.
#[test]
fn path_takes_the_escaped_form_and_tells_the_byte_from_the_text() {
    let t = raw();
    let h = Heir::of(t);
    let kb = key(&h, Who::TenantB);
    let dest = h.dest("paths");
    let o = h.run(&[
        "--restore",
        "--key",
        &kb,
        "--to",
        dest_str(&dest),
        "--unit",
        UNIT_B,
        "--path",
        "caf\\xe9.txt",
        "--path",
        "dir\\xe9/in\\xff.bin",
        "--path",
        "lit\\\\xe9.txt",
        "--path",
        &escaped("odd\\back name.txt"),
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    let got = heir::tree_bytes(&dest);
    let names: Vec<&[u8]> = got.keys().map(|k| k.as_slice()).collect();
    assert_eq!(
        names,
        [
            &b"caf\xe9.txt"[..],
            b"dir\xe9/in\xff.bin",
            b"lit\\xe9.txt",
            b"odd\\back name.txt"
        ],
        "only the named files, by their bytes"
    );
    assert_raw_restored(
        t,
        &dest,
        &[
            b"hard\xe9-link",
            b"dir\xe9/in\xff.bin",
            b"caf\xe9.txt",
            b"lit\\xe9.txt",
        ],
    );
    // The text `\xe9` is not the byte 0xe9: naming the byte does not bring the
    // file of that text, and a raw backslash is not the escaped one.
    let none = h.dest("none");
    let o = h.run(&[
        "--restore",
        "--key",
        &kb,
        "--to",
        dest_str(&none),
        "--unit",
        UNIT_B,
        "--path",
        "lit\\xe9.txt",
    ]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stderr.contains("are not in the file table"),
        "{}",
        o.stderr
    );
    let o = h.run(&[
        "--restore",
        "--key",
        &kb,
        "--to",
        dest_str(&none),
        "--unit",
        UNIT_B,
        "--path",
        "odd\\back name.txt",
    ]);
    assert_eq!(o.code, 1, "{}", o.all());
}

/// A damaged slice with a raw-named file in it: the per-file check names it
/// escaped in the damaged-file list (one line, valid UTF-8), the list given to
/// `--paths-from` on another copy restores it byte for byte, and a TAB, an LF
/// or a raw name typed to `--path` is refused with the way to type it.
#[test]
fn the_damaged_file_list_round_trips_a_name_that_is_not_utf8() {
    let t = &raw_copies()[0];
    let t2 = &raw_copies()[1];
    let h1 = Heir::of(t);
    let kb = key(&h1, Who::TenantB);
    let rows = list_rows(&h1, &kb, UNIT_B);
    let (_, slice, _, _) = row_esc(&rows, "caf\\xe9.txt");
    h1.flip(slice_pos(t, UNIT_B, *slice), 300);
    let dest = h1.dest("first");
    let o = h1.run(&[
        "--restore",
        "--key",
        &kb,
        "--to",
        dest_str(&dest),
        "--unit",
        UNIT_B,
    ]);
    assert_eq!(o.code, 1, "{}", o.all());
    let listed = damaged_list(&dest);
    assert!(
        listed.iter().any(|l| l == "caf\\xe9.txt"),
        "the name is in the list, escaped: {listed:?}"
    );
    assert!(
        std::fs::read_to_string(dest.join(".tapectl-damaged-files.txt")).is_ok(),
        "the list is UTF-8 text"
    );
    let h2 = Heir::of(t2);
    let o = h2.run(&[
        "--restore",
        "--key",
        &kb,
        "--to",
        dest_str(&dest),
        "--paths-from",
        dest.join(".tapectl-damaged-files.txt").to_str().unwrap(),
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(!dest.join(".tapectl-damaged-files.txt").exists());
    assert_raw_restored(t, &dest, &[]);
    assert_restored(t, UNIT_B, &dest);
    // A real TAB in --path is refused with the way to write it.
    let o = h2.run(&[
        "--restore",
        "--key",
        &kb,
        "--to",
        dest_str(&h2.dest("tab")),
        "--unit",
        UNIT_B,
        "--path",
        "a\tb",
    ]);
    assert_eq!(o.code, 2, "{}", o.all());
    assert!(o.stderr.contains("written \\t, \\n or \\r"), "{}", o.stderr);
}

/// The one unescape program, by itself, under every awk the host has: each case
/// of the escape comes out as the bytes it stands for, a name that ends in a
/// newline survives, and `printf %b`'s other escapes are NOT read.
#[test]
fn lint_the_unescape_awk_program_writes_the_same_bytes_under_every_awk() {
    let script = String::from_utf8(tapectl::volume::heir::RESTORE_SH.as_bytes().to_vec()).unwrap();
    let start = script.find("AWK_UNESC='").expect("the program") + "AWK_UNESC='".len();
    let program = &script[start..start + script[start..].find("\n'\n").unwrap()];
    assert!(program.contains("%c"), "the bytes are written by code");
    let cases: [(&str, &[u8]); 10] = [
        ("caf\\xe9.txt", b"caf\xe9.txt"),
        ("a\\\\b\\\\xe9", b"a\\b\\xe9"),
        ("tab\\tx\\nq\\r", b"tab\tx\nq\r"),
        ("plain-\u{e9}", "plain-\u{e9}".as_bytes()),
        ("\\xff\\xfe", b"\xff\xfe"),
        ("bad\\q", b"bad\\q"),
        ("end\\", b"end\\"),
        ("\\x4", b"\\x4"),
        ("\\c stops\\x41", b"\\c stopsA"),
        ("", b""),
    ];
    for (name, body) in awks() {
        let dir = tempfile::tempdir().unwrap();
        let prog = dir.path().join("unesc.awk");
        std::fs::write(&prog, program).unwrap();
        let mut ran = 0;
        for (text, want) in &cases {
            use std::io::Write;
            let tool = body.replace("\"$@\"", &format!("-f {} \"$@\"", prog.display()));
            let sh = dir.path().join("run.sh");
            std::fs::write(&sh, format!("#!/bin/sh\n{tool}")).unwrap();
            let mut child = std::process::Command::new("sh")
                .arg(&sh)
                .env("LC_ALL", "C")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(text.as_bytes())
                .unwrap();
            let out = child.wait_with_output().unwrap();
            assert_eq!(out.stdout, *want, "{name}: {text:?}");
            ran += 1;
        }
        assert_eq!(ran, cases.len(), "{name}");
    }
}

/// The per-file check names a damaged file whose name is not UTF-8 by its
/// escaped form (the list's own, `--list`'s), and the run fails as it does for
/// any damaged file. A tar stand-in overwrites the bytes of `dir\xe9/in\xff.bin`
/// after the real tar ran (exit 0); the same run undamaged passes.
#[test]
fn the_file_check_names_a_damaged_non_utf8_file_by_its_escaped_name() {
    let t = raw();
    let h = Heir::of(t);
    let kb = key(&h, Who::TenantB);
    // Positive control: undamaged, the same run passes and writes no list.
    let clean = h.dest("clean");
    let o = h.run(&[
        "--restore",
        "--key",
        &kb,
        "--to",
        dest_str(&clean),
        "--unit",
        UNIT_B,
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(o.stdout.contains("File check: all"), "{}", o.stdout);
    assert!(!clean.join(".tapectl-damaged-files.txt").exists());

    let real = heir::host_tool("tar").unwrap();
    h.put_tool(
        "tar",
        &format!(
            r#"#!/bin/bash
dest=""
args=("$@")
for ((i = 0; i < ${{#args[@]}}; i++)); do
  if [ "${{args[$i]}}" = -C ]; then dest=${{args[$((i + 1))]}}; fi
done
"{real}" "$@"
rc=$?
if [ -n "$dest" ] && [ -f "$dest/"$'dir\xe9/in\xff.bin' ]; then
  printf 'wrong bytes' > "$dest/"$'dir\xe9/in\xff.bin'
fi
exit $rc
"#,
            real = real.display()
        ),
    );
    let dest = h.dest("damaged");
    let o = h.run(&[
        "--restore",
        "--key",
        &kb,
        "--to",
        dest_str(&dest),
        "--unit",
        UNIT_B,
    ]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stdout.contains("RESTORE INCOMPLETE: 1 path(s)"),
        "{}",
        o.stdout
    );
    assert!(o.stdout.contains("File check: 1 of "), "{}", o.stdout);
    // The stand-in did damage the file: the bytes on disk are the wrong ones.
    assert_eq!(
        std::fs::read(dest.join(os(b"dir\xe9/in\xff.bin"))).unwrap(),
        b"wrong bytes"
    );
    assert_eq!(
        damaged_list(&dest),
        vec![escaped_bytes(b"dir\xe9/in\xff.bin")],
        "named by its escaped form, never as raw bytes"
    );
    assert_eq!(escaped_bytes(b"dir\xe9/in\xff.bin"), "dir\\xe9/in\\xff.bin");
}

// ---- the next slice is read while one is decrypted (CTO 2026-11-24) ----

/// What `log_reads_and_decrypts` saw, in the order it happened.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Event {
    /// A `dd` began on this tape file, in the process with this id.
    Dd(usize, u32),
    AgeStart,
    AgeEnd,
}

/// A `dd` and an `age` that append to `state/events`, each line whole and in
/// the order it happens: `dd <tape file> <time> <pid>` as each read begins (the
/// stand-in `dd` then runs in that same process), and `age-start <time>` /
/// `age-end <time>` around each slice's decryption (never an envelope's).
/// `age` sleeps `nap` seconds on a slice, so a read begun while it decrypts
/// lands between its two lines.
fn log_reads_and_decrypts(h: &Heir, nap: &str) {
    let bin = h.bin();
    std::fs::rename(bin.join("dd"), bin.join("dd.fake")).unwrap();
    h.put_tool(
        "dd",
        "#!/bin/sh\necho \"dd $(cat \"$FAKE_TAPE/pos\") $(date +%s.%N) $$\" >>\"$FAKE_TAPE/events\"\nexec \"$(dirname \"$0\")/dd.fake\" \"$@\"\n",
    );
    h.put_tool(
        "age",
        &format!(
            "#!/bin/bash\ncase \"$(ls -l /proc/$$/fd/0 2>/dev/null)\" in\n  *slice.enc) ;;\n  *) exec {real} \"$@\" ;;\nesac\necho \"age-start $(date +%s.%N)\" >>\"$FAKE_TAPE/events\"\nsleep {nap}\nrc=0\n{real} \"$@\" || rc=$?\necho \"age-end $(date +%s.%N)\" >>\"$FAKE_TAPE/events\"\nexit $rc\n",
            real = heir::heir_age_binary().display()
        ),
    );
}

fn events(h: &Heir) -> Vec<Event> {
    std::fs::read_to_string(h.root.path().join("state/events"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let w: Vec<&str> = l.split_whitespace().collect();
            match w.first().copied() {
                Some("dd") => Some(Event::Dd(w[1].parse().ok()?, w[3].parse().ok()?)),
                Some("age-start") => Some(Event::AgeStart),
                Some("age-end") => Some(Event::AgeEnd),
                _ => None,
            }
        })
        .collect()
}

/// For each slice decryption in turn (the i-th `age-end`, from 1), how many
/// reads of the slice positions `slices` had begun by its end. Without a read
/// ahead it is i: slice i+1 is read after slice i is decrypted. With it, i+1
/// where slice i+1's read began while slice i decrypted.
fn reads_begun_at_each_decrypt_end(evs: &[Event], slices: &[usize]) -> Vec<usize> {
    let mut begun = 0;
    let mut out = Vec::new();
    for e in evs {
        match e {
            Event::Dd(p, _) if slices.contains(p) => begun += 1,
            Event::AgeEnd => out.push(begun),
            _ => {}
        }
    }
    out
}

/// Read-ahead, depth one: while slice k is decrypted, the read of slice k+1
/// (on the cartridge in the drive) has already begun, at least once in a unit
/// of several slices, and never more than one slice ahead. Each slice is read
/// once, and the unit comes back byte for byte.
#[test]
fn the_next_slice_is_read_from_the_tape_while_one_is_decrypted() {
    let t = two();
    let h = Heir::of(t);
    log_reads_and_decrypts(&h, "0.3");
    let (o, dest) = restore_unit_b(&h);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(o.stdout.contains("RESTORE COMPLETE"), "{}", o.stdout);
    assert_restored(t, UNIT_B, &dest);
    let all = t.slice_positions();
    let reads: Vec<usize> = h
        .dd_reads()
        .into_iter()
        .filter(|p| all.contains(p))
        .collect();
    assert!(
        reads.len() > 2,
        "unit B is read in several slices: {reads:?}"
    );
    let mut once = reads.clone();
    once.sort_unstable();
    once.dedup();
    assert_eq!(
        once.len(),
        reads.len(),
        "each slice is read once: {reads:?}"
    );
    let evs = events(&h);
    let at_end = reads_begun_at_each_decrypt_end(&evs, &reads);
    assert_eq!(at_end.len(), reads.len(), "one decryption a slice: {evs:?}");
    for (i, n) in at_end.iter().enumerate() {
        assert!(
            *n <= i + 2,
            "never more than one slice ahead: decryption {} ended with {n} reads begun: {evs:?}",
            i + 1
        );
    }
    assert!(
        at_end.iter().enumerate().any(|(i, n)| *n == i + 2),
        "slice k+1's read begins before slice k's decryption ends, at least once: {at_end:?} {evs:?}"
    );
}

/// The data slices unit B's whole restore reads, in order, on a clean run.
fn unit_b_reads(t: &Tape) -> Vec<usize> {
    let h = Heir::of(t);
    let (o, _) = restore_unit_b(&h);
    assert_eq!(o.code, 0, "{}", o.all());
    data_reads(t, &h)
}

/// The positive control of a read ahead: the read of tape file `pos` began
/// before the `nth` slice decryption (from 1) ended.
fn read_began_during_decrypt(evs: &[Event], pos: usize, nth: usize) -> bool {
    let at = evs
        .iter()
        .position(|e| matches!(e, Event::Dd(p, _) if *p == pos));
    let end = evs
        .iter()
        .enumerate()
        .filter(|(_, e)| **e == Event::AgeEnd)
        .nth(nth - 1)
        .map(|(i, _)| i);
    matches!((at, end), (Some(a), Some(e)) if a < e)
}

/// A slice read ahead that fails is judged in the words of a read made when
/// it is asked for, and the verdict is that slice's: the tape cannot give it,
/// its bytes are not the front index's, the work directory fills while it is
/// read ahead, or its copy is cut short while every byte streams whole.
#[test]
fn a_slice_read_ahead_that_fails_has_the_verdict_of_a_read_made_then() {
    let t = two();
    let reads = unit_b_reads(t);
    let p = reads[1];
    let enc = t.size_of(p);
    // The tape cannot give it.
    let h = Heir::of(t);
    log_reads_and_decrypts(&h, "0.3");
    std::fs::write(h.root.path().join("files").join(format!(".fail.{p}")), "").unwrap();
    let (o, _) = restore_unit_b(&h);
    assert!(
        read_began_during_decrypt(&events(&h), p, 1),
        "{:?}",
        events(&h)
    );
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stderr.contains(&format!(
            "NOTE: reading tape file {p} failed: dd: error reading"
        )) && o
            .stderr
            .contains(&format!("slice 2: the tape could not be read at file {p}")),
        "{}",
        o.stderr
    );
    assert!(
        !o.stderr.contains("differs from the front index"),
        "{}",
        o.stderr
    );
    // Its bytes are not the ones the front index lists.
    let h = Heir::of(t);
    log_reads_and_decrypts(&h, "0.3");
    h.flip(p, 100);
    let (o, _) = restore_unit_b(&h);
    assert!(
        read_began_during_decrypt(&events(&h), p, 1),
        "{:?}",
        events(&h)
    );
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stderr.contains(&format!(
            "slice 2: file {p} differs from the front index's hash"
        )),
        "{}",
        o.stderr
    );
    assert!(
        !o.stderr.contains("could not be read at file"),
        "{}",
        o.stderr
    );
    // The work directory fills while it is read ahead: the run ends, and the
    // tape is not blamed.
    let h = Heir::of(t);
    log_reads_and_decrypts(&h, "0.3");
    let real = heir::host_tool("tee").unwrap();
    h.put_tool(
        "tee",
        &format!(
            "#!/bin/bash\ncase \"$1\" in\n  *slice.enc.next) ;;\n  *) exec {real} \"$@\" ;;\nesac\ncat >/dev/null\necho \"tee: $1: No space left on device\" >&2\nexit 1\n",
            real = real.display()
        ),
    );
    let (o, _) = restore_unit_b(&h);
    assert!(
        read_began_during_decrypt(&events(&h), p, 1),
        "{:?}",
        events(&h)
    );
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(
        o.stderr.contains(&format!(
            "FATAL: reading tape file {p} into the work directory failed: tee: "
        )) && o
            .stderr
            .contains("cannot take it: it is full, or not writable"),
        "{}",
        o.stderr
    );
    for bad_words in [
        "could not be read at file",
        "differs from the front index",
        "RESTORE COMPLETE",
    ] {
        assert!(!o.all().contains(bad_words), "{bad_words}: {}", o.all());
    }
    // Its copy is cut short while every byte streams whole, loudly or not.
    for loud in [true, false] {
        let h = Heir::of(t);
        log_reads_and_decrypts(&h, "0.3");
        short_tee(&h, "slice.enc.next", loud);
        let (o, _) = restore_unit_b(&h);
        assert!(
            h.root.path().join("state/tee.cut").exists(),
            "loud={loud}: {}",
            o.all()
        );
        assert!(
            read_began_during_decrypt(&events(&h), p, 1),
            "{:?}",
            events(&h)
        );
        assert_eq!(o.code, 1, "loud={loud}: {}", o.all());
        let words = if loud {
            format!("FATAL: reading tape file {p} into the work directory failed: tee: ")
        } else {
            format!(
                "FATAL: writing slice 2 into the work directory failed: the copy holds 1000 of the {enc} bytes read from tape file {p}"
            )
        };
        assert!(o.stderr.contains(&words), "loud={loud}: {}", o.stderr);
        assert!(
            o.stderr
                .contains("cannot take it: it is full, or not writable"),
            "loud={loud}: {}",
            o.stderr
        );
        for bad_words in [
            "could not be read at file",
            "differs from the front index",
            "could not decrypt",
            "RESTORE COMPLETE",
        ] {
            assert!(
                !o.all().contains(bad_words),
                "loud={loud} {bad_words}: {}",
                o.all()
            );
        }
    }
}

/// Is process `pid` running: there, and not a zombie? Where nothing reaps
/// orphans (docker's PID 1 without `--init`), a killed tool stays a zombie,
/// which holds no file open, the tape included.
fn running(pid: u32) -> bool {
    let Ok(s) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let state = s
        .rfind(") ")
        .and_then(|i| s[i + 2..].split_whitespace().next());
    !matches!(state, None | Some("Z") | Some("X"))
}

/// The processes of a run still alive: each one whose environment names this
/// run's directory tape and whose name is one of the tools a read runs.
fn live_tools(h: &Heir) -> Vec<(u32, String)> {
    let mark = format!("FAKE_TAPE={}", h.root.path().join("state").display());
    let mut out = Vec::new();
    for e in std::fs::read_dir("/proc").unwrap() {
        let name = e.unwrap().file_name().to_string_lossy().into_owned();
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        let Ok(env) = std::fs::read(format!("/proc/{pid}/environ")) else {
            continue;
        };
        if !env.split(|b| *b == 0).any(|v| v == mark.as_bytes()) {
            continue;
        }
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
        let comm = comm.trim().to_string();
        if [
            "dd",
            "dd.fake",
            "sleep",
            "head",
            "tee",
            "sha256sum",
            "cat",
            "mt",
        ]
        .contains(&comm.as_str())
        {
            out.push((pid, comm));
        }
    }
    out
}

/// TERM while a slice is being read ahead: the run ends as a kill does, the
/// work directory is gone, and nothing of the read is left running (the read
/// is a process group of its own, stopped and waited for on the way out).
#[test]
fn a_kill_while_a_slice_is_read_ahead_leaves_no_process_of_it_behind() {
    let t = two();
    let reads = unit_b_reads(t);
    let p = reads[1];
    let h = Heir::of(t);
    log_reads_and_decrypts(&h, "0.3");
    let dest = h.dest("out");
    let kb = key(&h, Who::TenantB);
    let slow = format!("{p}:20");
    let args = ["--restore", "--key", &kb, "--to", dest_str(&dest)];
    let mut r = h.spawn(&args, &[("FAKE_DD_SLOW", slow.as_str())]);
    let until = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !h.root.path().join("state/dd.slow").exists() {
        assert!(std::time::Instant::now() < until, "{}", r.text());
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let evs = events(&h);
    let dd_pid = evs
        .iter()
        .find_map(|e| match e {
            Event::Dd(q, pid) if *q == p => Some(*pid),
            _ => None,
        })
        .expect("the read of slice 2 began");
    // Positive control: the read ahead runs, and the scan sees it.
    assert!(running(dd_pid), "the read of tape file {p} is running");
    assert!(
        live_tools(&h).iter().any(|(pid, _)| *pid == dd_pid),
        "{:?}",
        live_tools(&h)
    );
    r.signal("TERM");
    let o = r.finish();
    assert_eq!(o.code, 143, "{}", o.all());
    assert!(
        !running(dd_pid),
        "the read ahead's dd ({dd_pid}) is gone when the run has ended: {}",
        o.all()
    );
    assert_eq!(live_tools(&h), vec![], "{}", o.all());
    assert!(no_work_dir_in(&dest), "{}", o.all());
    assert!(!o.all().contains("Terminated"), "{}", o.all());
}

/// A read ahead still running when the next slice asked for is not on the
/// cartridge in the drive: slice 2 (the first of the long file's run, 2 to 4)
/// fails to decrypt while slice 3 is read ahead, slowly, so the restore goes
/// on to slice 5, which is on the next tape. At that prompt the read ahead
/// has been waited for: no slice file waits in the work directory (item 267),
/// no read of it is running, and nothing touches the tape while it waits.
#[test]
fn a_read_ahead_in_flight_before_a_swap_ends_before_the_prompt() {
    let t = span3();
    let h = Heir::of_series(&[&t[0], &t[1], &t[2]]);
    log_reads_and_decrypts(&h, "0.3");
    h.put_tool(
        "age",
        &format!(
            "#!/bin/bash\ncase \"$(ls -l /proc/$$/fd/0 2>/dev/null)\" in\n  *slice.enc) ;;\n  *) exec {real} \"$@\" ;;\nesac\nn=$(cat \"$FAKE_TAPE/age.n\" 2>/dev/null || echo 0)\nn=$((n + 1))\necho \"$n\" >\"$FAKE_TAPE/age.n\"\necho \"age-start $(date +%s.%N)\" >>\"$FAKE_TAPE/events\"\nif [ \"$n\" = 2 ]; then\n  cat >/dev/null\n  echo 'age: error: the slice broke' >&2\n  echo \"age-end $(date +%s.%N)\" >>\"$FAKE_TAPE/events\"\n  exit 1\nfi\nrc=0\n{real} \"$@\" || rc=$?\necho \"age-end $(date +%s.%N)\" >>\"$FAKE_TAPE/events\"\nexit $rc\n",
            real = heir::heir_age_binary().display()
        ),
    );
    let p3 = positions(&t[0], 3, 3)[0];
    let slow = format!("{p3}:2");
    let dest = h.dest("out");
    let ka = key(&h, Who::TenantA);
    let args = restore_all(&ka, &dest);
    let mut r = h.spawn(&args, &[("FAKE_DD_SLOW", slow.as_str())]);
    let mut asked = 0;
    let until = std::time::Instant::now() + std::time::Duration::from_secs(200);
    let code = loop {
        let text = r.text();
        let n = text.matches(heir::PROMPT).count();
        if n > asked {
            asked = n;
            if asked == 1 {
                // Positive control: slice 3 was read ahead (its slow read began
                // while slice 2 decrypted).
                let evs = events(&h);
                assert!(read_began_during_decrypt(&evs, p3, 2), "{evs:?}");
                let dd_pid = evs
                    .iter()
                    .find_map(|e| match e {
                        Event::Dd(q, pid) if *q == p3 => Some(*pid),
                        _ => None,
                    })
                    .unwrap();
                assert!(
                    !running(dd_pid),
                    "the read ahead of slice 3 has ended at the prompt"
                );
                assert_eq!(live_tools(&h), vec![], "{text}");
                let work = std::fs::read_dir(&dest)
                    .unwrap()
                    .map(|e| e.unwrap().path())
                    .find(|p| {
                        p.file_name()
                            .unwrap()
                            .to_string_lossy()
                            .starts_with(".tapectl-work")
                    })
                    .expect("the work directory");
                for e in std::fs::read_dir(&work).unwrap() {
                    let name = e.unwrap().file_name().to_string_lossy().into_owned();
                    assert!(!name.starts_with("slice."), "{name} waits at the prompt");
                }
                let before = h.ops();
                std::thread::sleep(std::time::Duration::from_secs(1));
                assert_eq!(h.ops(), before, "no mt or dd while the prompt waits");
            }
            h.load(["HEIR02", "HEIR03"][asked - 1]);
            r.type_line("");
        }
        if let Some(code) = r.try_wait() {
            break code;
        }
        assert!(std::time::Instant::now() < until, "{}", r.text());
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let all = r.text();
    assert_eq!(asked, 2, "{all}");
    assert_eq!(code, 1, "{all}");
    assert!(all.contains("slice 2: age could not decrypt it"), "{all}");
    assert!(
        all.contains("slice 3: part of the long member whose first slice is bad"),
        "{all}"
    );
    assert!(list_of(&dest).iter().any(|p| p == LARGE), "{all}");
    assert!(no_work_dir_in(&dest), "{all}");
}

/// Named paths read ahead too: the next slice a wanted member needs is read
/// while the one before it decrypts, each slice once, and the files are right.
#[test]
fn named_paths_read_the_next_slice_they_need_while_one_is_decrypted() {
    let t = two();
    let h = Heir::of(t);
    let k = key(&h, Who::TenantB);
    let rows = list_rows(&h, &k, UNIT_B);
    let first = format!("inner/{UNIT_B}-deep.dat");
    let (_, s1, _, _) = row(&rows, &first);
    let (_, s2, _, _) = row(&rows, "notes.txt");
    assert_ne!(s1, s2, "two slices");
    let mut want = vec![slice_pos(t, UNIT_B, *s1), slice_pos(t, UNIT_B, *s2)];
    want.sort_unstable();
    log_reads_and_decrypts(&h, "0.3");
    h.reset_ops();
    let dest = h.dest("two");
    let o = h.run(&[
        "--restore",
        "--key",
        &k,
        "--to",
        dest_str(&dest),
        "--unit",
        UNIT_B,
        "--path",
        "notes.txt",
        "--path",
        &first,
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert_eq!(data_reads(t, &h), want, "each slice once: {:?}", h.ops());
    assert!(
        read_began_during_decrypt(&events(&h), want[1], 1),
        "{:?}",
        events(&h)
    );
    let got = heir::tree(&dest);
    assert_eq!(got.len(), 2, "{:?}", got.keys());
    for (path, bytes) in unit_files(t, UNIT_B) {
        if let Some(have) = got.get(path) {
            assert_eq!(have, bytes, "{path}");
        }
    }
}

/// A disk of `phys` bytes, as `df` and `tee` see it. Its use is what the
/// restore has put there: the slice files (`slice.*`) in the work directory
/// and, with `work` unset (the work directory inside `--to`), the files
/// restored under `dest`; nothing else counts, and until the envelopes are
/// unpacked it has room to spare (their room is not what this tests). `df`
/// reports `phys` and one tape block more, the block the script keeps free as
/// its own margin, so a read ahead it starts on that report fits in `phys`.
/// `tee` holds the disk to `phys`: a slice file that leaves more than that in
/// use fails as a full disk does. With `work` set, the work directory under it
/// is that disk, and every other path has room to spare.
fn slice_disk(h: &Heir, dest: &Path, work: Option<&Path>, phys: u64) {
    let root = work.unwrap_or(dest).display().to_string();
    let files = if work.is_some() {
        String::new()
    } else {
        format!(
            "  f=$(find \"{d}\" -path '*/.tapectl-work*' -prune -o -type f -printf '%s\\n' 2>/dev/null | awk '{{ s += $1 }} END {{ printf \"%d\", s }}')\n  s=$((s + f))\n",
            d = dest.display()
        )
    };
    let used = format!(
        "used() {{\n  s=$(find \"{root}\" -path '*/.tapectl-work*/slice.*' -type f -printf '%s\\n' 2>/dev/null | awk '{{ s += $1 }} END {{ printf \"%d\", s }}')\n{files}  echo \"$s\"\n}}\n"
    );
    h.put_tool(
        "df",
        &format!(
            "#!/bin/bash\n{used}for a; do last=$a; done\navail=999999999\ndev=/dev/other\ncase \"$last\" in\n  {root}*)\n    dev=/dev/slices\n    if [ -n \"$(find \"{root}\" -path '*/.tapectl-work*/env.*' -print -quit 2>/dev/null)\" ]; then\n      avail=$(( ({phys} + 524288 - $(used)) / 1024 ))\n    fi ;;\nesac\necho 'Filesystem 1024-blocks Used Available Capacity Mounted on'\necho \"$dev 999999999 0 $avail 0% /\"\n"
        ),
    );
    let real = heir::host_tool("tee").unwrap();
    h.put_tool(
        "tee",
        &format!(
            "#!/bin/bash\n{used}case \"$1\" in\n  *slice.*) ;;\n  *) exec {real} \"$@\" ;;\nesac\nrc=0\n{real} \"$@\" || rc=$?\n[ \"$rc\" = 0 ] || exit \"$rc\"\nu=$(used)\nif [ \"$u\" -gt {phys} ]; then\n  echo \"$1 $u\" >>\"$FAKE_TAPE/overfull\"\n  echo \"tee: $1: No space left on device\" >&2\n  exit 1\nfi\n",
            real = real.display()
        ),
    );
}

/// The read ahead is taken only where there is room for it: a work directory
/// with room for two of the unit's largest slices (the space check's need)
/// and not three restores the unit whole, each slice read once, and never
/// holds more than it has room for. On a disk of its own, the slices that
/// would not fit are not read ahead (a few small ones still may be); on the
/// disk of `--to`, where tar's pipe may still hold up to a megabyte that is
/// not yet on the disk, none is.
#[test]
fn a_work_directory_with_room_for_two_slices_restores_whole_without_a_read_ahead() {
    let t = two();
    let reads = unit_b_reads(t);
    let big = reads.iter().map(|p| t.size_of(*p) as u64).max().unwrap();
    // The work directory on a disk of its own.
    let h = Heir::of(t);
    let kb = key(&h, Who::TenantB);
    log_reads_and_decrypts(&h, "0.3");
    let work = h.dest("workdir");
    let dest = h.dest("out");
    slice_disk(&h, &dest, Some(&work), 2 * big + 1024);
    let o = h.run(&[
        "--restore",
        "--key",
        &kb,
        "--to",
        dest_str(&dest),
        "--work",
        dest_str(&work),
    ]);
    assert!(
        !h.root.path().join("state/overfull").exists(),
        "the work directory held more than it has room for: {}",
        o.all()
    );
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(o.stdout.contains("RESTORE COMPLETE"), "{}", o.stdout);
    assert_restored(t, UNIT_B, &dest);
    assert_eq!(data_reads(t, &h), reads, "each slice once");
    let at_end = reads_begun_at_each_decrypt_end(&events(&h), &reads);
    assert!(
        at_end[..at_end.len() - 1]
            .iter()
            .enumerate()
            .any(|(i, n)| *n == i + 1),
        "a slice that would not fit is not read ahead: {at_end:?} {:?}",
        events(&h)
    );
    // The work directory inside --to: room for the unit's files and two slices.
    let h = Heir::of(t);
    let kb = key(&h, Who::TenantB);
    let files: u64 = list_rows(&h, &kb, UNIT_B)
        .iter()
        .filter(|r| r.0 == "f")
        .map(|r| r.2)
        .sum();
    log_reads_and_decrypts(&h, "0.3");
    let dest = h.dest("out");
    slice_disk(&h, &dest, None, files + 2 * big + 1024);
    let o = h.run(&["--restore", "--key", &kb, "--to", dest_str(&dest)]);
    assert!(
        !h.root.path().join("state/overfull").exists(),
        "the disk held more than it has room for: {}",
        o.all()
    );
    assert_eq!(o.code, 0, "{}", o.all());
    assert_restored(t, UNIT_B, &dest);
    assert_eq!(data_reads(t, &h), reads, "each slice once");
    let at_end = reads_begun_at_each_decrypt_end(&events(&h), &reads);
    assert_eq!(
        at_end,
        (1..=reads.len()).collect::<Vec<_>>(),
        "no slice read ahead: {:?}",
        events(&h)
    );
}

// ---- #525: text compared as text, and no silent exit on a full pipe ----

/// Rows for units named `7`, `007`, `1.1` and `1.10` in every `units.tsv` of
/// the index (names equal as numbers, different as text), and in the
/// operator's a unit `twin` held under a tenant `7` and then a tenant `007`.
fn index_numeric_names(idx: &tapectl::volume::index::IndexFiles) {
    let mut files = vec![idx.operator_units.clone()];
    files.extend(idx.tenants.values().map(|t| t.units.clone()));
    for (i, p) in files.iter().enumerate() {
        heir::edit_tsv(p, |h, rows| {
            let c = |n: &str| h.iter().position(|x| x == n).unwrap();
            let tenant = h.iter().position(|x| x == "tenant");
            let mut row = |unit: &str, n: usize, ten: &str| {
                let mut r = vec![String::new(); h.len()];
                r[c("unit")] = unit.into();
                r[c("unit_uuid")] = format!("cccccccc-0000-4000-8000-{i:04}{n:08}");
                r[c("version")] = "1".into();
                r[c("num_slices")] = "1".into();
                r[c("first_slice")] = "1".into();
                r[c("last_slice")] = "1".into();
                r[c("version_bytes")] = "1000".into();
                r[c("volume")] = format!("OLD{n:02}");
                r[c("volume_uuid")] = format!("00000000-0000-4000-8000-0000000001{n:02}");
                r[c("state")] = "sealed".into();
                r[c("sealed_at")] = "2026-10-01T00:00:00Z".into();
                if let Some(t) = tenant {
                    r[t] = ten.into();
                }
                rows.push(r);
            };
            for (n, unit) in ["7", "007", "1.1", "1.10"].iter().enumerate() {
                row(unit, n, TENANT_A);
            }
            if i == 0 {
                row("twin", 10, "7");
                row("twin", 11, "007");
            }
        });
    }
}

fn numeric_index() -> &'static Tape {
    static T: OnceLock<Tape> = OnceLock::new();
    T.get_or_init(|| indexed(index_numeric_names))
}

/// The `Unit ...` heads `--where` printed: the name, and the tenant when one
/// is named (`Unit NAME (tenant T):`).
fn where_heads(out: &str) -> Vec<String> {
    out.lines()
        .filter_map(|l| {
            l.strip_prefix("Unit ")?
                .strip_suffix(':')
                .map(str::to_string)
        })
        .collect()
}

/// #525: `--where 7` names unit 7 alone, never `007` (and `1.1` never
/// `1.10`), and one unit held under the tenants `7` and `007` is told under
/// two heads, under every awk: names are compared as text, never as numbers.
#[test]
fn where_compares_unit_and_tenant_names_as_text_under_every_awk() {
    let all = awks();
    assert!(!all.is_empty(), "no awk at all on this host");
    for (name, body) in all {
        let h = Heir::of(numeric_index());
        put_awk(&h, &body);
        let ka = key(&h, Who::TenantA);
        for unit in ["7", "007", "1.1", "1.10"] {
            let o = h.run(&["--where", unit, "--key", &ka]);
            assert_eq!(o.code, 0, "{name} {unit}: {}", o.all());
            assert_eq!(
                where_heads(&o.stdout),
                vec![format!("{unit} (tenant {TENANT_A})")],
                "{name} {unit}: {}",
                o.stdout
            );
        }
        let ke = key(&h, Who::Escrow);
        let o = h.run(&["--where", "twin", "--key", &ke]);
        assert_eq!(o.code, 0, "{name} twin: {}", o.all());
        assert_eq!(
            where_heads(&o.stdout),
            vec![
                "twin (tenant 7)".to_string(),
                "twin (tenant 007)".to_string()
            ],
            "{name} twin: {}",
            o.stdout
        );
    }
}

/// Two units on a tape that ends early: `links-unit`, with 240 hard links of
/// long names to one file, all past the tape's end, and `zulu-unit`, which
/// `--all` restores after it.
fn many_links() -> &'static Tape {
    static T: OnceLock<Tape> = OnceLock::new();
    T.get_or_init(|| {
        let mut shape = Shape::two();
        shape.units = vec![
            ("zulu-unit".into(), TENANT_A, false),
            ("links-unit".into(), TENANT_A, true),
        ];
        shape.warning_in_slice = Some(1);
        shape.links = 240;
        Tape::build(&shape)
    })
}

/// #525: more hard-linked names whose first name lies in a slice not read
/// than a pipe holds the lines of. The restore says so (the first 20, and how
/// many more), prints the unit's id map and RESTORE INCOMPLETE, exits 1, and
/// `--all` goes on to the next unit: never a silent exit 141.
#[test]
fn many_hard_links_past_the_end_are_listed_and_the_restore_goes_on() {
    let t = many_links();
    let h = Heir::of(t);
    let ka = key(&h, Who::TenantA);
    let dest = h.dest("out");
    let o = h.run(&restore_all(&ka, &dest));
    let all = o.all();
    assert_eq!(o.code, 1, "{all}");
    assert!(o.stdout.contains("RESTORE INCOMPLETE"), "{all}");
    assert!(
        o.stdout.contains(
            "Hard-linked names whose first name is in a slice that was not read (restored by the repair once that tape has been read):"
        ),
        "{all}"
    );
    // links-unit's own hard link and the 240: the first 20, and the count of
    // the rest.
    let listed = o
        .stdout
        .lines()
        .filter(|l| l.contains(" -> first name ") && !l.contains("zulu-unit"))
        .count();
    assert_eq!(listed, 20, "{all}");
    assert!(
        o.stdout
            .lines()
            .any(|l| l == "  and 221 more, each on the damaged-file list"),
        "{all}"
    );
    assert_eq!(
        o.stdout
            .matches("Who the ids were (the stream carries owners by number")
            .count(),
        2,
        "the id map of each unit: {all}"
    );
    assert!(o.stdout.contains(">>> Unit zulu-unit "), "{all}");
    assert!(
        o.stdout
            .contains("zulu-unit-small.txt -> first name zulu-unit-hardlink.txt"),
        "zulu-unit was reached: {all}"
    );
}

// ---- #606 (the #551 read, R-3 and R-4): one escape for --find, no raw control byte printed ----

/// The paths `--find` printed for `unit`, as printed.
fn found_paths(out: &str, unit: &str) -> Vec<String> {
    out.lines()
        .filter(|l| l.starts_with(&format!("  {unit}  v")))
        .filter_map(|l| l.split_once(" bytes  ").map(|(_, p)| p.to_string()))
        .collect()
}

/// R-3: `--find` takes TEXT in the form `--list` prints a path (1.5's one
/// escape) and matches it against each path decoded the same way, under
/// every awk: `\xe9` is the byte, found in `caf\xe9.txt` and never in the
/// name that is the text `lit\\xe9.txt`; `\\xe9` is that text; `\t` is a TAB.
#[test]
fn find_takes_the_printed_form_of_a_path_under_every_awk() {
    let all = awks();
    assert!(!all.is_empty(), "no awk at all on this host");
    for (name, body) in all {
        let h = Heir::of(raw());
        put_awk(&h, &body);
        let kb = key(&h, Who::TenantB);
        let o = h.run(&["--find", "caf\\xe9", "--key", &kb]);
        assert_eq!(o.code, 0, "{name}: {}", o.all());
        assert_eq!(
            found_paths(&o.stdout, UNIT_B),
            vec!["caf\\xe9.txt"],
            "{name}: {}",
            o.stdout
        );
        let o = h.run(&["--find", "\\xe9", "--key", &kb]);
        assert_eq!(o.code, 0, "{name}: {}", o.all());
        let got = found_paths(&o.stdout, UNIT_B);
        assert!(got.contains(&"caf\\xe9.txt".to_string()), "{name}: {got:?}");
        assert!(
            !got.contains(&"lit\\\\xe9.txt".to_string()),
            "{name}: {got:?}"
        );
        let o = h.run(&["--find", "\\\\xe9", "--key", &kb]);
        assert_eq!(o.code, 0, "{name}: {}", o.all());
        assert_eq!(
            found_paths(&o.stdout, UNIT_B),
            vec!["lit\\\\xe9.txt"],
            "{name}: {}",
            o.stdout
        );
        let o = h.run(&["--find", "lit\\xe9", "--key", &kb]);
        assert_eq!(o.code, 1, "{name}: the byte is not the text: {}", o.all());
        let o = h.run(&["--find", "tab\\tand", "--key", &kb]);
        assert_eq!(o.code, 0, "{name}: {}", o.all());
        assert_eq!(
            found_paths(&o.stdout, UNIT_B),
            vec!["tab\\tand\\nnewline.txt"],
            "{name}: {}",
            o.stdout
        );
    }
}

fn control() -> &'static Tape {
    static T: OnceLock<Tape> = OnceLock::new();
    T.get_or_init(|| {
        let mut shape = Shape::two();
        shape.control_names = true;
        Tape::build(&shape)
    })
}

/// R-4: a name holding ESC and BEL never reaches the terminal raw: `--list`
/// and `--find` print each such byte as `\xHH`, a message names it the same
/// way, and `--path` takes the listed form back and restores the file by its
/// bytes, under every awk.
#[test]
fn a_control_byte_in_a_name_is_printed_escaped_and_the_listed_form_restores_it() {
    let printed = "bad\\x1b]0;owned\\x07name.txt";
    let all = awks();
    assert!(!all.is_empty(), "no awk at all on this host");
    for (name, body) in all {
        let t = control();
        let h = Heir::of(t);
        put_awk(&h, &body);
        let kb = key(&h, Who::TenantB);
        let list = h.run(&["--list", "--key", &kb, "--unit", UNIT_B]);
        assert_eq!(list.code, 0, "{name}: {}", list.all());
        assert!(
            !list.all().contains(['\u{1b}', '\u{7}']),
            "{name}: a raw control byte"
        );
        assert!(
            list.stdout
                .lines()
                .any(|l| l.ends_with(&format!("  {printed}"))),
            "{name}: {}",
            list.stdout
        );
        let find = h.run(&["--find", "owned", "--key", &kb]);
        assert_eq!(find.code, 0, "{name}: {}", find.all());
        assert!(
            !find.all().contains(['\u{1b}', '\u{7}']),
            "{name}: a raw control byte"
        );
        assert_eq!(found_paths(&find.stdout, UNIT_B), vec![printed], "{name}");
        let dest = h.dest("one");
        let args = [
            "--restore",
            "--key",
            &kb,
            "--to",
            dest_str(&dest),
            "--unit",
            UNIT_B,
            "--path",
            printed,
        ];
        let o = h.run(&args);
        assert_eq!(o.code, 0, "{name}: {}", o.all());
        assert_eq!(
            std::fs::read(dest.join(heir::CONTROL_NAME)).unwrap(),
            unit_files(t, UNIT_B)
                .iter()
                .find(|(p, _)| p == heir::CONTROL_NAME)
                .unwrap()
                .1,
            "{name}"
        );
        // Again, without --overwrite: the refusal names it escaped.
        let o = h.run(&args);
        assert_eq!(o.code, 1, "{name}: {}", o.all());
        assert!(
            o.stderr.contains(&format!("{printed} is already in")),
            "{name}: {}",
            o.stderr
        );
        assert!(
            !o.all().contains(['\u{1b}', '\u{7}']),
            "{name}: a raw control byte"
        );
    }
}

// ---- #606 C-2: a file-table path that leaves --to is refused before anything is written ----

/// Add to unit A's file table a copy of its first row of `kind` with the path
/// (or, for `h`, the hard link's target) `bad`.
fn craft_row(table: &Path, kind: &str, bad: &str) {
    let text = std::fs::read_to_string(table).unwrap();
    if !text.contains(&format!("{UNIT_A}-small.txt")) {
        return;
    }
    let row = text
        .lines()
        .find(|l| l.starts_with(&format!("{kind}\t")))
        .unwrap_or_else(|| panic!("no {kind} row"));
    let mut f: Vec<String> = row.split('\t').map(str::to_string).collect();
    if kind == "h" {
        f[7] = bad.to_string();
    } else {
        f[6] = bad.to_string();
    }
    let mut out = text.clone();
    out.push_str(&f.join("\t"));
    out.push('\n');
    std::fs::write(table, out).unwrap();
}

/// An edit of a staging's file table (`Shape::table_edit`).
type TableEdit = fn(&Path);

fn t_abs(p: &Path) {
    craft_row(p, "f", "/abs-sentinel.txt")
}
fn t_dotdot(p: &Path) {
    craft_row(p, "f", "../../sentinel.txt")
}
fn t_dotdot_x(p: &Path) {
    craft_row(p, "f", "\\x2e\\x2e/\\x2e\\x2e/sentinel.txt")
}
fn t_slash_x(p: &Path) {
    craft_row(p, "f", "inner\\x2fslash.txt")
}
fn t_round(p: &Path) {
    craft_row(p, "f", "caf\\x41.txt")
}
fn t_link(p: &Path) {
    craft_row(p, "h", "../../sentinel.txt")
}

/// #606 C-2: a file-table row whose path, or hard link's target, begins with
/// `/`, holds a `..` component (plainly or as `\x2e\x2e`), holds a `/` written
/// `\x2f`, or is not written as 1.5 writes a path, makes the unit's table
/// refused by name before anything is removed, made or linked: nothing of
/// that unit is written under `--to`, nothing beside `--to` or above it is
/// made or removed, and `--all` goes on to the next unit.
#[test]
fn a_table_path_that_leaves_to_is_refused_and_nothing_outside_to_is_touched() {
    let cases: [(TableEdit, &str); 6] = [
        (t_abs, "it begins with /"),
        (t_dotdot, "it has a .. component"),
        (t_dotdot_x, "it has a .. component"),
        (t_slash_x, "a / in it is written \\x2f"),
        (t_round, "it is not written as a path is written"),
        (t_link, "its hard link's target: it has a .. component"),
    ];
    for (edit, why) in cases {
        let mut shape = Shape::two();
        shape.table_edit = Some(edit);
        let t = Tape::build(&shape);
        let h = Heir::of(&t);
        let ke = key(&h, Who::Escrow);
        let up = h.dest("up");
        let boxed = up.join("box");
        std::fs::create_dir_all(&boxed).unwrap();
        std::fs::write(up.join("sentinel.txt"), b"keep up").unwrap();
        std::fs::write(boxed.join("sentinel.txt"), b"keep box").unwrap();
        let to = boxed.join("to");
        let listing = |d: &Path| {
            let mut v: Vec<String> = std::fs::read_dir(d)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            v.sort();
            v
        };
        let o = h.run(&restore_all(&ke, &to));
        let all = o.all();
        assert_eq!(o.code, 1, "{why}: {all}");
        assert!(
            o.stderr.contains(&format!(
                "the file table of unit {UNIT_A} holds a path this script will not write"
            )) && o.stderr.contains(why),
            "{why}: {}",
            o.stderr
        );
        assert!(
            o.stdout.contains(&format!(
                "RESTORE INCOMPLETE: the file table of unit(s) {UNIT_A} holds a path"
            )),
            "{why}: {all}"
        );
        assert!(
            !to.join(UNIT_A).exists(),
            "{why}: nothing of unit A under --to"
        );
        assert_restored(&t, UNIT_B, &to.join(UNIT_B));
        assert_eq!(listing(&boxed), ["sentinel.txt", "to"], "{why}");
        assert_eq!(listing(&up), ["box", "sentinel.txt"], "{why}");
        assert_eq!(
            std::fs::read(boxed.join("sentinel.txt")).unwrap(),
            b"keep box"
        );
        assert_eq!(std::fs::read(up.join("sentinel.txt")).unwrap(), b"keep up");
        assert!(!Path::new("/abs-sentinel.txt").exists());
    }
}
