//! File 1 and RECOVERY.md of Layout 3, every command run as written (the
//! plan's T4.13; `docs/design/volume-format-v3.md` §7.5; #522, #523, #526,
//! #563, #477, #481).
//!
//! The model is the old suite's "File 1's own commands, run as written": the
//! text is the one the tape itself holds (File 1 from the tape's own bytes,
//! RECOVERY.md out of the envelope the tape's own key opens), its code blocks
//! are taken out of it, the placeholders it names (`UNIT`, `POSITION`, `SIZE`
//! and the like) are filled in, and each command is run by `bash` against a
//! directory tape written by the heir harness from REAL stagings
//! (`tests/common/heir/mod.rs`, which this file uses and never edits). `mt`
//! and `dd` are the harness's stand-ins, `age` is `examples/heir_age`, and
//! tar, gzip, xz, zstd, awk and sha256sum are the host's own.
//!
//! A test runs every code line of the sections it owns, and says so by
//! recording each line it ran: [`Sh::run`] keeps them, and
//! [`every_line_ran`] fails for a code line of a section that a test owns and
//! did not run. [`every_code_block_is_owned`] fails for a code block in a
//! section no test owns, so a command added to either text without a test
//! fails here. A command a test cannot run is not in the text: the one
//! exception is named in [`NOT_RUN`].
//!
//! File 1's code lines are the lines indented four spaces (and nothing else
//! is indented that far); RECOVERY.md's are its fenced blocks.

#[path = "common/heir/mod.rs"]
mod heir;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use heir::{Heir, Shape, Tape, Who, TENANT_A, TENANT_B, UNIT_A, UNIT_B};

// ---- the tapes, built once each ----

fn two() -> &'static Tape {
    static T: OnceLock<Tape> = OnceLock::new();
    T.get_or_init(|| Tape::build(&Shape::two()))
}

/// Two tapes written from the same stagings, one of them to be damaged.
fn copies() -> &'static [Tape] {
    static T: OnceLock<Vec<Tape>> = OnceLock::new();
    T.get_or_init(|| {
        let mut shape = Shape::two();
        shape.copies = 2;
        Tape::build_series(&shape)
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

/// HEIR01 holds slices 1-3 of the one big unit, HEIR02 4-5, HEIR03 6-8.
fn span3() -> &'static [Tape] {
    static T: OnceLock<Vec<Tape>> = OnceLock::new();
    T.get_or_init(|| {
        let mut shape = Shape::one("none");
        shape.span = Some(heir::Span {
            min_copies: 1,
            warns: vec![Some(2), Some(1), None],
        });
        let t = Tape::build_series(&shape);
        let held: Vec<(i64, i64)> = t.iter().map(|x| (x.runs[0].3, x.runs[0].4)).collect();
        assert_eq!(held, [(1, 3), (4, 5), (6, 8)]);
        t
    })
}

/// One tenant, three units: a unit of several slices first, then two of one
/// slice each (#522's trace: 12, 1 and 1 slices).
fn three_units() -> &'static Tape {
    static T: OnceLock<Tape> = OnceLock::new();
    T.get_or_init(|| {
        let mut shape = Shape::two();
        shape.units = vec![
            ("big-one".into(), TENANT_A, true),
            ("small-one".into(), TENANT_A, false),
            ("small-two".into(), TENANT_A, false),
        ];
        Tape::build(&shape)
    })
}

// ---- the texts ----

/// Bytes off a tape with its block padding taken out, as text.
fn plain(bytes: &[u8]) -> String {
    let kept: Vec<u8> = bytes.iter().copied().filter(|b| *b != 0).collect();
    String::from_utf8_lossy(&kept).into_owned()
}

/// File 1, from the tape's own bytes.
fn guide(t: &Tape) -> String {
    plain(&t.files[1])
}

/// The text of the section `## title`, to the next `## ` heading.
fn section(text: &str, title: &str) -> String {
    let head = format!("## {title}");
    let from = text
        .find(&format!("\n{head}\n"))
        .or_else(|| text.starts_with(&head).then_some(0))
        .unwrap_or_else(|| panic!("no section {head:?} in:\n{text}"));
    let rest = &text[from + head.len() + 1..];
    rest[..rest.find("\n## ").unwrap_or(rest.len())].to_string()
}

/// Every `## ` title of `text`.
fn titles(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| l.strip_prefix("## "))
        .map(str::to_string)
        .collect()
}

/// File 1's code groups: runs of lines indented four spaces, each line with
/// its first four spaces taken off.
fn code_groups(section: &str) -> Vec<Vec<String>> {
    let mut groups: Vec<Vec<String>> = vec![Vec::new()];
    for line in section.lines() {
        if let Some(code) = line.strip_prefix("    ").filter(|c| !c.trim().is_empty()) {
            groups.last_mut().unwrap().push(code.to_string());
        } else if !groups.last().unwrap().is_empty() {
            groups.push(Vec::new());
        }
    }
    groups.retain(|g| !g.is_empty());
    groups
}

/// The group of `section` with a line that contains `needle`.
fn block(section: &str, needle: &str) -> Vec<String> {
    code_groups(section)
        .into_iter()
        .find(|g| g.iter().any(|l| l.contains(needle)))
        .unwrap_or_else(|| panic!("no code block with {needle:?} in:\n{section}"))
}

/// A RECOVERY.md section's fenced blocks, each as its lines.
fn fenced(section: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut cur: Option<Vec<String>> = None;
    for line in section.lines() {
        if line.starts_with("```") {
            match cur.take() {
                Some(b) => out.push(b),
                None => cur = Some(Vec::new()),
            }
        } else if let Some(b) = cur.as_mut() {
            b.push(line.to_string());
        }
    }
    out
}

/// The sections of File 1 whose code lines no test of this file runs, and
/// who does. The Heir Kit's catalogue's commands (`age -o`, `sqlite3`) are
/// run by the library's own `reread4_item1_the_heir_kits_catalogue_opens_and_the_queries_run`
/// against a catalogue made as the kit's is; the zero-strip procedure's by
/// `the_zero_strip_recipe_keeps_interior_zero_bytes_and_decrypts`.
const OWNED_ELSEWHERE: &[(&str, &str)] = &[
    (
        "The Heir Kit's catalogue (catalog.db.age)",
        "layout.rs: reread4_item1_the_heir_kits_catalogue_opens_and_the_queries_run",
    ),
    (
        "tapectl itself",
        "tests/frozen_spellings.rs: file_1s_three_commands_parse_and_are_live (each tapectl command of File 1 parses against the binary)",
    ),
    (
        "If All Else Fails",
        "layout.rs: the_zero_strip_recipe_keeps_interior_zero_bytes_and_decrypts",
    ),
];

/// The code lines no test can run: finding the drive on a host that has one.
/// Everything else in a code block is run.
const NOT_RUN: &[&str] = &["ls -l /dev/tape/by-id/"];

// ---- running a block as written ----

/// A shell on the directory tape: commands run by `bash -e` with the
/// harness's PATH, in `work`, and the template lines each run recorded.
struct Sh {
    root: PathBuf,
    bin: PathBuf,
    work: PathBuf,
    ran: BTreeSet<String>,
}

impl Sh {
    fn new(h: &Heir, work: PathBuf) -> Sh {
        // The manual path and the zero-strip use two coreutils the script
        // itself does not: link the host's, as a use of the harness.
        for tool in ["truncate", "od", "stat"] {
            if let Some(real) = heir::host_tool(tool) {
                if !h.bin().join(tool).exists() {
                    h.link_tool(tool, &real);
                }
            }
        }
        std::fs::create_dir_all(&work).unwrap();
        Sh {
            root: h.root.path().to_path_buf(),
            bin: h.bin(),
            work,
            ran: BTreeSet::new(),
        }
    }

    fn command(&self, script: &str) -> Command {
        let mut c = Command::new("bash");
        c.args(["-e", "-c", script])
            .current_dir(&self.work)
            .env_clear()
            .env("PATH", &self.bin)
            .env("HOME", &self.root)
            .env("TMPDIR", self.root.join("tmp"))
            .env("FAKE_TAPE", self.root.join("state"))
            .env("FAKE_FILES", self.root.join("files"))
            .env("TAPE_DEVICE", "/dev/nst0")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        std::fs::create_dir_all(self.root.join("tmp")).unwrap();
        c
    }

    /// Run `template` (lines as the text gives them) after the placeholders
    /// `subs` are filled in, longest first. `Ok(output)` when it ended 0.
    fn try_run(&mut self, template: &[String], subs: &[(&str, String)]) -> Result<String, String> {
        for l in template.iter().flat_map(|l| l.split('\n')) {
            self.ran.insert(l.trim().to_string());
        }
        let mut script = template.join("\n");
        let mut subs: Vec<&(&str, String)> = subs.iter().collect();
        subs.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
        for (from, to) in subs {
            script = script.replace(from, to);
        }
        let o = self.command(&script).output().expect("spawn bash");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
        if o.status.success() {
            Ok(text)
        } else {
            Err(format!(
                "{script}\n--- ended {:?}:\n{text}",
                o.status.code()
            ))
        }
    }

    fn run(&mut self, template: &[String], subs: &[(&str, String)]) -> String {
        self.try_run(template, subs)
            .unwrap_or_else(|e| panic!("{e}"))
    }
}

/// Every code line of `section` was run by `sh` (or is in [`NOT_RUN`]).
fn every_line_ran(sh: &Sh, title: &str, section: &str) {
    for g in code_groups(section) {
        for l in g {
            let l = l.trim().to_string();
            if l.starts_with('#') {
                continue;
            }
            assert!(
                sh.ran.contains(&l) || NOT_RUN.contains(&l.as_str()),
                "a code line of \"{title}\" no test ran: {l}"
            );
        }
    }
}

fn key_of(h: &Heir, who: Who) -> String {
    h.key(who).to_string_lossy().into_owned()
}

fn unit_truth<'a>(t: &'a Tape, name: &str) -> &'a heir::UnitTruth {
    t.units.iter().find(|u| u.name == name).unwrap()
}

fn assert_restored(t: &Tape, name: &str, dir: &Path) {
    let got = heir::tree(dir);
    for (path, bytes) in &unit_truth(t, name).files {
        let have = got
            .get(path)
            .unwrap_or_else(|| panic!("{path} was not restored; got {:?}", got.keys()));
        assert!(have == bytes, "{path} differs");
    }
}

// ---- the manual path (File 1, "Manual Recovery Steps") ----

/// File 1's manual path, command by command, as an heir with `mt`, `dd`,
/// `age`, `tar`, `sha256sum` and the standard tools would: File 0, the front
/// index, the seal marker, an envelope the key opens after one it does not,
/// the manifest, each slice, the join into one tar, the per-file check, the
/// id map and one file cut out by its offsets. The unit that comes out is the
/// unit that went in. With `alg` the unit is compressed and the slice line is
/// the one of "Compressed units".
fn manual_restore(t: &Tape, who: Who, unit: &str, alg: Option<&str>) {
    let h = Heir::of(t);
    let g = guide(t);
    let manual = section(&g, "Manual Recovery Steps");
    let comp_section = section(&g, "Compressed units");
    let work = h.root.path().join("manual");
    let mut sh = Sh::new(&h, work.clone());
    let key = key_of(&h, who);
    let truth = unit_truth(t, unit);

    // Steps 1 to 4: block size, File 0, File 3 with its hash, the seal.
    sh.run(&block(&manual, "setblk 524288"), &[]);
    sh.run(&block(&manual, "> file0.txt"), &[]);
    let file0 = std::fs::read_to_string(work.join("file0.txt")).unwrap();
    assert!(
        file0.contains(&format!("label = \"{}\"", t.label)),
        "{file0}"
    );
    let out = sh.run(&block(&manual, "> front_index.toml"), &[]);
    let fi_hash = out.split_whitespace().next().unwrap().to_string();
    sh.run(&block(&manual, "> seal.toml"), &[]);
    let seal = std::fs::read_to_string(work.join("seal.toml")).unwrap();
    assert!(
        seal.contains(&format!("front_index_sha256 = \"{fi_hash}\"")),
        "the seal binds the front index the manual path read:\n{seal}"
    );

    // Step 5: the envelopes, in order; the key that is not theirs says so.
    let envelope_block = block(&manual, "> envelope.tar");
    let mut opened = false;
    for e in &t.entries {
        if e.type_label != "tenant_envelope" && e.type_label != "operator_envelope" {
            continue;
        }
        let subs = [
            ("POSITION", e.position.to_string()),
            ("SIZE", e.size_bytes.unwrap().to_string()),
            ("YOUR_KEY.age.key", key.clone()),
        ];
        match sh.try_run(&envelope_block, &subs) {
            Ok(out) => {
                assert!(
                    out.contains(e.sha256_encrypted.as_ref().unwrap()),
                    "the sha256sum printed equals the front index's: {out}"
                );
                opened = true;
                break;
            }
            Err(e) => assert!(
                e.contains("no identity matched"),
                "a key that is not an envelope's says so: {e}"
            ),
        }
    }
    assert!(opened, "the key opens one envelope");
    let out = sh.run(&block(&manual, "tar xf envelope.tar"), &[]);
    assert!(
        out.contains("MANIFEST.toml") && out.contains("tables"),
        "{out}"
    );

    // Step 6: the manifest.
    let env = work.join("env");
    let manifest = tapectl::volume::manifest::Manifest::from_toml(
        &std::fs::read_to_string(env.join("MANIFEST.toml")).unwrap(),
    )
    .unwrap();
    let mu = manifest
        .units
        .iter()
        .find(|u| u.name == unit)
        .unwrap_or_else(|| panic!("{unit} is not in the envelope the key opened"));
    assert_eq!(mu.compression.as_deref(), alg);
    let out = sh.run(&block(&manual, "^(name|uuid"), &[]);
    assert!(out.contains(unit) && out.contains("file_table"), "{out}");
    let table = mu
        .file_table
        .clone()
        .expect("a tenant envelope names the table");
    let table_stem = table
        .trim_start_matches("tables/")
        .trim_end_matches(".tsv")
        .to_string();

    // Step 7: each slice, in order, as the text names the commands.
    sh.run(
        &block(&manual, "FIRST_TAPE_POSITION"),
        &[(
            "FIRST_TAPE_POSITION",
            mu.slices[0].tape_position.unwrap().to_string(),
        )],
    );
    let slice_block = match alg {
        None => block(&manual, "> slice.NUMBER.tar"),
        Some(_) => block(&comp_section, "DECOMPRESS"),
    };
    let decompress = match alg {
        Some("gzip") => "gzip -dc",
        Some("xz") => "xz -dc",
        Some("zstd") => "zstd -dc",
        _ => "",
    };
    for s in &mu.slices {
        let subs = [
            ("SHA256_ENCRYPTED", s.sha256_encrypted.clone()),
            ("SHA256_PLAIN", s.sha256_plain.clone()),
            ("ENCRYPTED_BYTES", s.encrypted_bytes.to_string()),
            ("SIZE_BYTES", s.size_bytes.to_string()),
            ("NUMBER", s.number.to_string()),
            ("DECOMPRESS", decompress.to_string()),
            ("YOUR_KEY.age.key", key.clone()),
        ];
        let out = sh.run(&slice_block, &subs);
        assert!(!out.contains("FAILED"), "slice {}: {out}", s.number);
        assert!(work.join(format!("slice.{}.tar", s.number)).exists());
        assert!(
            !work.join(format!("slice.{}.age", s.number)).exists(),
            "the encrypted copy is removed once decrypted"
        );
    }
    if alg.is_none() {
        // The slice line stops at a failed hash: typed one line at a time
        // with no `-e` to save them, a wrong hash keeps the encrypted copy
        // and decrypts nothing.
        let s = &mu.slices[0];
        let first = section(&g, "Manual Recovery Steps");
        let positioning = block(&first, "FIRST_TAPE_POSITION");
        sh.run(
            &positioning,
            &[("FIRST_TAPE_POSITION", s.tape_position.unwrap().to_string())],
        );
        std::fs::remove_file(work.join(format!("slice.{}.tar", s.number))).unwrap();
        let subs = [
            ("SHA256_ENCRYPTED", "0".repeat(64)),
            ("SHA256_PLAIN", s.sha256_plain.clone()),
            ("ENCRYPTED_BYTES", s.encrypted_bytes.to_string()),
            ("SIZE_BYTES", s.size_bytes.to_string()),
            ("NUMBER", s.number.to_string()),
            ("YOUR_KEY.age.key", key.clone()),
        ];
        let mut script = slice_block.join("\n");
        for (from, to) in &subs {
            script = script.replace(from, to);
        }
        let o = sh
            .command(&script.replace("\n", "; true\n"))
            .output()
            .unwrap();
        let said = format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
        assert!(said.contains("FAILED"), "the hash check failed: {said}");
        assert!(
            work.join(format!("slice.{}.age", s.number)).exists(),
            "the encrypted copy is kept when its hash fails: {said}"
        );
        assert!(
            !work.join(format!("slice.{}.tar", s.number)).exists(),
            "a slice whose hash failed is never decrypted: {said}"
        );
        // Put it right again.
        std::fs::remove_file(work.join(format!("slice.{}.age", s.number))).unwrap();
        sh.run(
            &positioning,
            &[("FIRST_TAPE_POSITION", s.tape_position.unwrap().to_string())],
        );
        sh.run(
            &slice_block,
            &[
                ("SHA256_ENCRYPTED", s.sha256_encrypted.clone()),
                ("SHA256_PLAIN", s.sha256_plain.clone()),
                ("ENCRYPTED_BYTES", s.encrypted_bytes.to_string()),
                ("SIZE_BYTES", s.size_bytes.to_string()),
                ("NUMBER", s.number.to_string()),
                ("YOUR_KEY.age.key", key.clone()),
            ],
        );
    }

    // A gap between two slices: the forward space the text shows.
    sh.run(&block(&manual, "fsf COUNT"), &[("COUNT", "1".to_string())]);

    // Step 8: the slices joined, one tar run, into an empty directory.
    let dest = h.dest("manual-restored");
    let out = sh.run(
        &block(&manual, "--xattrs --acls"),
        &[("/destination", dest.to_string_lossy().into_owned())],
    );
    assert!(!out.contains("Unexpected EOF"), "{out}");
    assert_restored(t, unit, &dest);

    // Step 9: the per-file check against the file table, and the id map.
    let out = sh.run(
        &block(&manual, "sha256sum -c --quiet"),
        &[
            ("/destination", dest.to_string_lossy().into_owned()),
            ("TABLE", table_stem.clone()),
        ],
    );
    assert!(!out.contains("FAILED"), "{out}");
    let out = sh.run(
        &block(&manual, ".ids.tsv"),
        &[("TABLE", table_stem.clone())],
    );
    assert!(out.starts_with("#tapectl-id-map"), "{out}");

    // One file, by its offsets: a file that begins in a slice and one that
    // is a long file's run. The row, the slice's first member, and the cut.
    let table_path = env.join("tables").join(format!("{table_stem}.tsv"));
    let rows: Vec<Vec<String>> = std::fs::read_to_string(&table_path)
        .unwrap()
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| l.split('\t').map(str::to_string).collect())
        .collect();
    let row_of =
        |path: &str| -> Vec<String> { rows.iter().find(|r| r[6] == path).unwrap().clone() };
    let one_file = |sh: &mut Sh, path: &str, dest2: &Path| {
        let row = row_of(path);
        let (slice, start, end) = (&row[1], &row[2], &row[3]);
        let subs = [
            ("TABLE", table_stem.clone()),
            ("SOME_PATH", path.to_string()),
        ];
        let out = sh.run(&block(&manual, "$7 == "), &subs);
        assert!(out.contains(&row[5]), "the row of {path}: {out}");
        let out = sh.run(
            &block(&manual, "SLICE_START"),
            &[("TABLE", table_stem.clone()), ("SLICE", slice.clone())],
        );
        let slice_start = out.trim().to_string();
        // The slices the member lies in: from its first to the one holding
        // its last byte.
        let first: usize = slice.parse().unwrap();
        let mut last = first;
        let ends: Vec<u64> = {
            let mut acc = 0u64;
            mu.slices
                .iter()
                .map(|s| {
                    acc += s.size_bytes as u64;
                    acc
                })
                .collect()
        };
        let end_n: u64 = end.parse().unwrap();
        while ends[last - 1] < end_n {
            last += 1;
        }
        let joined: Vec<String> = (first..=last).map(|n| format!("slice.{n}.tar")).collect();
        let subs = [
            ("SLICE_START", slice_start),
            ("START", start.clone()),
            ("END", end.clone()),
            ("FIRST_SLICE_TO_LAST", joined.join(" ")),
            ("SLICE", slice.clone()),
            ("/destination", dest2.to_string_lossy().into_owned()),
        ];
        let which = if last == first {
            "slice.SLICE.tar | head -c"
        } else {
            "FIRST_SLICE_TO_LAST"
        };
        sh.run(&block(&manual, which), &subs);
    };
    let small = format!("inner/{unit}-deep.dat");
    let d1 = h.dest("one-file");
    one_file(&mut sh, &small, &d1);
    let got = std::fs::read(d1.join(&small)).unwrap();
    let want = &truth.files.iter().find(|(p, _)| *p == small).unwrap().1;
    assert_eq!(&got, want, "one file cut out by its offsets");
    let large = format!("{unit}-large.bin");
    if truth.files.iter().any(|(p, _)| *p == large) {
        let d2 = h.dest("one-run");
        one_file(&mut sh, &large, &d2);
        let got = std::fs::read(d2.join(&large)).unwrap();
        let want = &truth.files.iter().find(|(p, _)| *p == large).unwrap().1;
        assert_eq!(&got, want, "a long file, cut out of its run of slices");
    }

    match alg {
        None if truth.files.iter().any(|(p, _)| *p == large) => {
            every_line_ran(&sh, "Manual Recovery Steps", &manual)
        }
        None => {}
        Some(_) => every_line_ran(&sh, "Compressed units", &comp_section),
    }
}

#[test]
fn file_1_manual_path_restores_a_unit_as_written() {
    // The big unit of tenant B: several slices and a file that is a run.
    manual_restore(two(), Who::TenantB, UNIT_B, None);
}

#[test]
fn file_1_manual_path_restores_the_small_unit_as_written() {
    manual_restore(two(), Who::TenantA, UNIT_A, None);
}

#[test]
fn file_1_manual_path_for_a_gzip_unit_as_written() {
    manual_restore(compressed("gzip"), Who::TenantA, UNIT_A, Some("gzip"));
}

#[test]
fn file_1_manual_path_for_an_xz_unit_as_written() {
    manual_restore(compressed("xz"), Who::TenantA, UNIT_A, Some("xz"));
}

#[test]
fn file_1_manual_path_for_a_zstd_unit_as_written() {
    manual_restore(compressed("zstd"), Who::TenantA, UNIT_A, Some("zstd"));
}

// ---- RESTORE.sh's own modes, as File 1 gives them ----

/// The values the placeholders of a RESTORE.sh command line stand for.
fn script_subs(h: &Heir, who: Who, unit: &str, dest: &Path) -> Vec<(&'static str, String)> {
    vec![
        ("YOUR_KEY.age.key", key_of(h, who)),
        ("/destination", dest.to_string_lossy().into_owned()),
        ("SOME_PATH", format!("inner/{unit}-deep.dat")),
        ("TEXT", "notes".to_string()),
        ("UNIT", unit.to_string()),
    ]
}

/// The logical commands of `section` that run RESTORE.sh, one each (a line
/// continued with a backslash is one).
fn script_commands(section: &str) -> Vec<String> {
    let mut out = Vec::new();
    for g in code_groups(section) {
        let mut cur = String::new();
        for l in g {
            cur.push_str(&l);
            if l.ends_with('\\') {
                cur.push('\n');
                continue;
            }
            if cur.contains("./RESTORE.sh") {
                out.push(cur.clone());
            }
            cur.clear();
        }
    }
    out
}

/// The repair, as a text gives it (`cmd`, one `./RESTORE.sh --restore
/// --paths-from LIST --to DIR` line with `/destination/UNIT` standing for the
/// unit's directory): a restore from a damaged copy of the big unit leaves
/// `.tapectl-damaged-files.txt`; the command, run on the other copy, restores
/// the listed files and removes the list. Returns what `sh` ran.
fn run_repair(cmd: &str) -> BTreeSet<String> {
    let t = &copies()[0];
    let damaged = Heir::of(t);
    damaged.flip(t.slice_positions()[3], 1000);
    let ddest = damaged.dest("damaged");
    let o = damaged.run(&[
        "--restore",
        "--key",
        &key_of(&damaged, Who::TenantB),
        "--unit",
        UNIT_B,
        "--to",
        ddest.to_str().unwrap(),
    ]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(ddest.join(".tapectl-damaged-files.txt").is_file());
    let good = Heir::of(&copies()[1]);
    let mut gsh = Sh::new(&good, good.root.path().join("heir"));
    let script = gsh.work.join("RESTORE.sh");
    std::fs::write(&script, &t.script).unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let out = gsh.run(
        &[cmd.to_string()],
        &[
            ("YOUR_KEY.age.key", key_of(&good, Who::TenantB)),
            ("/destination/UNIT", ddest.to_string_lossy().into_owned()),
        ],
    );
    assert!(out.contains("RESTORE COMPLETE"), "{out}");
    assert_restored(t, UNIT_B, &ddest);
    assert!(
        !ddest.join(".tapectl-damaged-files.txt").exists(),
        "the list is removed when nothing is left"
    );
    gsh.ran
}

#[test]
fn file_1_restore_sh_commands_run_as_written() {
    let t = &copies()[0];
    let h = Heir::of(t);
    let g = guide(t);
    let auto = section(&g, "Automated Recovery (recommended)");
    let device = section(&g, "Which device?");
    let damaged = section(&g, "If a slice or a file is damaged");
    let mut sh = Sh::new(&h, h.root.path().join("heir"));
    // The script is fetched off the tape with the commands the guide gives:
    // File 2 comes back byte for byte.
    sh.run(&block(&auto, "> RESTORE.sh"), &[]);
    assert_eq!(
        std::fs::read(sh.work.join("RESTORE.sh")).unwrap(),
        t.script,
        "the script read off the tape is the tape's File 2"
    );
    // The line that says which tape is in the drive.
    let out = sh.run(&block(&auto, "grep -E"), &[]);
    assert!(out.contains(&format!("label = \"{}\"", t.label)), "{out}");

    let mut ran = 0;
    for (title, text) in [
        ("Automated Recovery (recommended)", auto.clone()),
        ("Which device?", device.clone()),
    ] {
        for cmd in script_commands(&text) {
            let dest = h.dest(&format!("out{ran}"));
            let subs = script_subs(&h, Who::Operator, UNIT_B, &dest);
            let out = sh
                .try_run(std::slice::from_ref(&cmd), &subs)
                .unwrap_or_else(|e| panic!("{title}: {e}"));
            ran += 1;
            if cmd.contains("--verify") {
                assert!(out.contains("VERIFY PASSED"), "{out}");
            }
            if cmd.contains("--restore") && cmd.contains("--all") {
                assert!(out.contains("RESTORE COMPLETE"), "{out}");
                assert_restored(t, UNIT_A, &dest.join(UNIT_A));
                assert_restored(t, UNIT_B, &dest.join(UNIT_B));
            }
            if cmd.contains("--path ") {
                assert!(out.contains("RESTORE COMPLETE"), "{out}");
            }
        }
    }
    assert!(ran >= 12, "the guide gives the modes: {ran}");

    // The repair line of "If a slice or a file is damaged".
    let repair = script_commands(&damaged)
        .into_iter()
        .find(|c| c.contains("--paths-from"))
        .expect("File 1 gives the repair");
    sh.ran.extend(run_repair(&repair));
    every_line_ran(&sh, "Automated Recovery (recommended)", &auto);
    every_line_ran(&sh, "Which device?", &device);
    every_line_ran(&sh, "If a slice or a file is damaged", &damaged);
}

// ---- a unit on more than one cartridge, as File 1 gives it ----

#[test]
fn file_1_a_unit_across_cartridges_runs_as_written_with_each_answer() {
    let t = span3();
    let g = guide(&t[0]);
    let across = section(&g, "A unit on more than one cartridge");
    let cmds = script_commands(&across);
    let restore = cmds
        .iter()
        .find(|c| c.contains("--restore") && c.contains("--unit UNIT"))
        .expect("File 1 gives the restore of a unit across cartridges")
        .clone();
    // The section's one code line is the restore (it may be continued).
    let code: Vec<String> = code_groups(&across).concat();
    assert_eq!(code.join("\n").trim(), restore.trim(), "{code:?}");
    let words = across.replace('\n', " ");
    for answer in ["ENTER", "done", "quit", "end of the input"] {
        assert!(words.contains(answer), "the answers: {answer}");
    }
    let run = |h: &Heir, dest: &Path, on_prompt: &mut dyn FnMut(usize, &str) -> Option<String>| {
        let cmd = restore
            .replace("YOUR_KEY.age.key", &key_of(h, Who::TenantA))
            .replace("/destination/UNIT", &dest.to_string_lossy())
            .replace("UNIT", UNIT_A);
        let args: Vec<String> = cmd
            .replace("./RESTORE.sh ", "")
            .replace("\\\n", " ")
            .split_whitespace()
            .map(str::to_string)
            .collect();
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        h.run_prompted(&args, |k, text| on_prompt(k, text))
    };
    let large = format!("{UNIT_A}-large.bin");

    // ENTER after loading the cartridge: the unit comes back whole.
    {
        let h = Heir::of_series(&[&t[0], &t[1], &t[2]]);
        let dest = h.dest("enter").join("u");
        let o = run(&h, &dest, &mut |k, _| {
            h.load(["HEIR02", "HEIR03"][k - 1]);
            Some(String::new())
        });
        assert_eq!(o.code, 0, "{}", o.all());
        assert_restored(&t[0], UNIT_A, &dest);
        assert!(dest.join(&large).is_file());
    }
    // "I do not have it", typed: the restore goes on with what it has and
    // lists the rest.
    {
        let h = Heir::of_series(&[&t[0], &t[1], &t[2]]);
        let dest = h.dest("done").join("u");
        let o = run(&h, &dest, &mut |_, _| Some("done".to_string()));
        assert_eq!(o.code, 1, "{}", o.all());
        assert!(dest.join(".tapectl-damaged-files.txt").is_file());
    }
    // "I do not have it", as the end of the input.
    {
        let h = Heir::of_series(&[&t[0], &t[1], &t[2]]);
        let dest = h.dest("eof").join("u");
        let o = run(&h, &dest, &mut |_, _| None);
        assert_eq!(o.code, 1, "{}", o.all());
        assert!(dest.join(".tapectl-damaged-files.txt").is_file());
    }
    // quit: stopped, what was restored and checked stays.
    {
        let h = Heir::of_series(&[&t[0], &t[1], &t[2]]);
        let dest = h.dest("quit").join("u");
        let o = run(&h, &dest, &mut |_, _| Some("quit".to_string()));
        assert_eq!(o.code, 1, "{}", o.all());
        assert!(o.stdout.contains("RESTORE STOPPED"), "{}", o.stdout);
    }
    // A start on any tape: the second cartridge alone, with the same
    // command, gives the files it holds whole; a later run into the same
    // destination from another tape of the unit adds that tape's files.
    {
        let h = Heir::of_series(&[&t[1], &t[0], &t[2]]);
        let dest = h.dest("any").join("u");
        let o = run(&h, &dest, &mut |_, _| None);
        assert_eq!(o.code, 1, "start on the second tape: {}", o.all());
        assert!(dest.join(".tapectl-damaged-files.txt").is_file());
        h.load("HEIR03");
        let o = run(&h, &dest, &mut |_, _| None);
        assert!(
            o.all().contains("this run adds to it"),
            "a later run into the same destination: {}",
            o.all()
        );
    }
}

// ---- the seal, by hand ----

#[test]
fn file_1_seal_commands_run_as_written() {
    let t = two();
    let h = Heir::of(t);
    let g = guide(t);
    let ended = section(&g, "Where the tape ended");
    let mut sh = Sh::new(&h, h.root.path().join("seal"));
    let out = sh.run(&block(&ended, "ended_after"), &[]);
    assert!(
        out.contains(&format!("ended_after = {}", t.ended_after())),
        "{out}"
    );
    assert!(
        out.contains(&format!("file_count = {}", t.files.len())),
        "{out}"
    );
    every_line_ran(&sh, "Where the tape ended", &ended);
}

// ---- RECOVERY.md (#522, #526) ----

/// The envelope of `who`'s tenant, opened with the key and unpacked into
/// `<work>/env`; its RECOVERY.md.
fn recovery_of(t: &Tape, h: &Heir, who: Who, work: &Path) -> String {
    let key = key_of(h, who);
    let age = heir::heir_age_binary();
    for e in &t.entries {
        if e.type_label != "tenant_envelope" {
            continue;
        }
        let raw = &t.files[e.position as usize][..e.size_bytes.unwrap() as usize];
        std::fs::create_dir_all(work).unwrap();
        std::fs::write(work.join("envelope.raw"), raw).unwrap();
        let o = Command::new(&age)
            .args(["-d", "-i", &key])
            .arg(work.join("envelope.raw"))
            .output()
            .unwrap();
        if !o.status.success() {
            continue;
        }
        std::fs::write(work.join("envelope.tar"), &o.stdout).unwrap();
        let env = work.join("env");
        std::fs::create_dir_all(&env).unwrap();
        let s = Command::new("tar")
            .args(["xf"])
            .arg(work.join("envelope.tar"))
            .arg("-C")
            .arg(&env)
            .status()
            .unwrap();
        assert!(s.success());
        return std::fs::read_to_string(env.join("RECOVERY.md")).unwrap();
    }
    panic!("the key opens no tenant envelope");
}

/// #522: the blocks of a three-unit envelope (a unit of several slices, then
/// two of one slice), run literally, in order, in ONE directory, restore
/// every unit's files with the source's bytes: no block reads another's
/// slices.
#[test]
fn recovery_md_blocks_of_three_units_run_literally_in_one_directory() {
    let t = three_units();
    let h = Heir::of(t);
    let work = h.root.path().join("rec");
    let md = recovery_of(t, &h, Who::TenantA, &work);
    let mut sh = Sh::new(&h, work.clone());
    let dest = h.dest("recovered");
    let key = key_of(&h, Who::TenantA);
    for name in ["big-one", "small-one", "small-two"] {
        let sec = section(&md, name);
        let blocks = fenced(&sec);
        assert!(!blocks.is_empty(), "{name}: {sec}");
        for b in blocks {
            sh.run(
                &b,
                &[
                    ("YOUR_KEY.age.key", key.clone()),
                    ("/destination", dest.to_string_lossy().into_owned()),
                ],
            );
        }
    }
    // Each unit has a directory of its own, and its files are the source's.
    let dirs: Vec<_> = std::fs::read_dir(&dest)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(dirs.len(), 3, "a directory for each unit: {dirs:?}");
    for name in ["big-one", "small-one", "small-two"] {
        let truth = unit_truth(t, name);
        let found = dirs.iter().any(|d| {
            let got = heir::tree(d);
            truth.files.iter().all(|(p, b)| got.get(p) == Some(b))
        });
        assert!(found, "{name} was restored in one of {dirs:?}");
    }
}

/// RECOVERY.md's other code: where the tape ended, which key.
#[test]
fn recovery_md_seal_and_key_commands_run_as_written() {
    let t = two();
    let h = Heir::of(t);
    let work = h.root.path().join("rec");
    let md = recovery_of(t, &h, Who::TenantB, &work);
    let mut sh = Sh::new(&h, work.clone());
    let key = key_of(&h, Who::TenantB);
    let ended = section(&md, "Where this tape ended");
    for b in fenced(&ended) {
        let out = sh.run(&b, &[]);
        assert!(out.contains("ended_after"), "{out}");
    }
    // The "Which key" line: several keys, one that opens it.
    let which = section(&md, "Which key");
    let slice = t
        .entries
        .iter()
        .find(|e| e.type_label == "data_slice")
        .unwrap();
    let _ = slice;
    for b in fenced(&which) {
        // The example names `OLD.age.key` and `NEW.age.key`: two files, one
        // of them the real key.
        std::fs::copy(&key, work.join("NEW.age.key")).unwrap();
        std::fs::copy(&key, work.join("OLD.age.key")).unwrap();
        // Run on a tenant slice of tenant B: the first slice of its unit.
        let pos = t
            .entries
            .iter()
            .filter(|e| e.type_label == "data_slice")
            .nth(t.units[0].slices)
            .unwrap();
        let raw = &t.files[pos.position as usize][..pos.size_bytes.unwrap() as usize];
        std::fs::write(work.join("slice.1.age"), raw).unwrap();
        let out = sh.run(&b, &[]);
        assert!(!out.contains("no identity"), "{out}");
    }
    // The repair in "If a slice or a file is damaged".
    let dmg = section(&md, "If a slice or a file is damaged");
    let blocks = fenced(&dmg);
    assert_eq!(blocks.len(), 1, "{dmg}");
    run_repair(&blocks[0].join("\n"));
}

/// Every fenced block of RECOVERY.md is in a section a test above runs, or
/// the library's own test of the index's `awk` lines.
#[test]
fn recovery_md_every_fenced_block_is_owned() {
    let t = three_units();
    let h = Heir::of(t);
    let md = recovery_of(t, &h, Who::TenantA, &h.root.path().join("rec"));
    let owned = [
        "Which key",
        "Where this tape ended",
        "If a slice or a file is damaged",
        "big-one",
        "small-one",
        "small-two",
        // layout.rs: recovery_md_describes_the_index_and_its_awk_lines_work
        "The index of other tapes",
    ];
    for title in titles(&md) {
        let sec = section(&md, &title);
        if fenced(&sec).is_empty() {
            continue;
        }
        assert!(
            owned.contains(&title.as_str()),
            "a fenced block in \"{title}\", which no test runs:\n{sec}"
        );
    }
}

// ---- the inventory and the absences ----

/// Every code block of File 1 is in a section a test owns above (or the two
/// whose owners are named in [`OWNED_ELSEWHERE`]).
#[test]
fn every_code_block_is_owned() {
    let g = guide(two());
    let owned_here = [
        "Manual Recovery Steps",
        "Compressed units",
        "Automated Recovery (recommended)",
        "Which device?",
        "If a slice or a file is damaged",
        "A unit on more than one cartridge",
        "Where the tape ended",
    ];
    for title in titles(&g) {
        let sec = section(&g, &title);
        if code_groups(&sec).is_empty() {
            continue;
        }
        let ok = owned_here.contains(&title.as_str())
            || OWNED_ELSEWHERE.iter().any(|(t, _)| *t == title);
        assert!(
            ok,
            "a code block in \"{title}\", which no test owns:\n{sec}"
        );
    }
    for (title, _) in OWNED_ELSEWHERE {
        assert!(titles(&g).iter().any(|t| t == title), "{title}");
    }
}

/// Neither text names what Layout 3 removed, a literal version of the current
/// tapectl, or a unit, tenant or key.
#[test]
fn neither_text_names_what_is_gone_or_a_name() {
    let t = three_units();
    let h = Heir::of(t);
    let g = guide(t);
    let md = recovery_of(t, &h, Who::TenantA, &h.root.path().join("rec"));
    for (what, text) in [("File 1", &g), ("RECOVERY.md", &md)] {
        let words: BTreeSet<&str> = text
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '.')
            .collect();
        assert!(!words.contains("dar"), "{what} names dar");
        for gone in [
            "--slices-from",
            "--swap-tapes",
            "--spool",
            "--scratch",
            "--fill-from",
            "--stream-tapes",
            "catalogs/",
            concat!("may", "_span"),
            "short-seal",
        ] {
            assert!(!text.contains(gone), "{what} still says {gone}");
        }
        for word in text.split_whitespace() {
            let w = word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '.' && c != '-');
            if let Some(v) = w.strip_prefix("tapectl-") {
                assert!(
                    v == "X.Y.Z" || !v.chars().next().unwrap_or('x').is_ascii_digit(),
                    "{what}: a literal version {w}"
                );
            }
        }
        assert!(!text.contains("tapectl 1."), "{what}");
    }
    // Invariant 3: File 1 is plaintext on tape and names no unit, tenant, key.
    for name in ["big-one", "small-one", "small-two", TENANT_A, TENANT_B] {
        assert!(!g.contains(name), "File 1 names {name}");
    }
    for k in t.keys.values() {
        assert!(!g.contains(k.as_str()), "File 1 holds a key");
    }
    assert!(!g.contains(&t.volume_uuid), "File 1 states the volume uuid");
    for uuid in t.unit_uuids.values() {
        assert!(!g.contains(uuid.as_str()), "File 1 states a unit uuid");
    }
}
