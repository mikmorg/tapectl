//! Issue #394: the st / SG boundary (`docs/design/threat-model.md` §2).
//!
//! The st driver does every data transfer and every movement of the medium;
//! SCSI generic is used to ASK — INQUIRY (`sg_inq`), LOG SENSE (`sg_logs`)
//! and READ ATTRIBUTE (`sg_read_attr`) — and, since #388 (ADR-0012 item
//! 163), to set exactly one page: the Control Data Protection page (0Ah/F0h)
//! that arms Logical Block Protection, read with `sg_modes` (MODE SENSE) and
//! written with `sg_wr_mode` (MODE SENSE, then MODE SELECT), never saved;
//! and, since 1.8.0 (ADR-0012 item 185), to ask the drive's position with one
//! raw command, READ POSITION's short form through `sg_raw`, because st's
//! `MTIOCPOS` sends a form the HP LTO-6 refuses. No SG command moves the
//! medium, so the st driver's own position accounting can never drift under a
//! command it did not see. This pins the set of sg3-utils tools the binary's
//! source names, the absence of the `sg_logs` flags that turn a LOG SENSE into
//! a LOG SELECT (which clears the drive's counters), the page argv, the one
//! READ POSITION argv, and that the binary never runs `mt`.
//!
//! A source scan, deliberately: the tools are spawned from half a dozen
//! modules and several only on real hardware, so no ungated run reaches them
//! all. Adding a tool here is a decision to record in the threat model first.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The sg3-utils tools tapectl may run, and the SCSI command each sends.
/// `sg_wr_mode` sends a MODE SENSE before its MODE SELECT, of the Control
/// Data Protection page only (`lbp_page_argv_is_pinned_and_never_saves`).
const ALLOWED: &[(&str, &str)] = &[
    ("sg_inq", "INQUIRY"),
    ("sg_logs", "LOG SENSE"),
    ("sg_read_attr", "READ ATTRIBUTE"),
    ("sg_modes", "MODE SENSE"),
    ("sg_wr_mode", "MODE SELECT"),
    // ADR-0012 item 185: READ POSITION's short form and no other CDB
    // (`read_position_argv_is_pinned_to_one_command`).
    ("sg_raw", "READ POSITION"),
];

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Every source file of the library and the binary, with its text.
fn sources() -> Vec<(PathBuf, String)> {
    let mut files = Vec::new();
    rust_files(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    assert!(
        files.len() > 50,
        "positive control: the scan must see the whole source tree, saw {}",
        files.len()
    );
    files
        .into_iter()
        .map(|p| {
            let text = std::fs::read_to_string(&p).unwrap();
            (p, text)
        })
        .collect()
}

/// Every `"sg_<name>"` string literal in `text`.
fn sg_literals(text: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while let Some(off) = text[i..].find("\"sg_") {
        let start = i + off + 1;
        let mut end = start;
        while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
            end += 1;
        }
        if end < bytes.len() && bytes[end] == b'"' {
            found.insert(text[start..end].to_string());
        }
        i = start;
    }
    found
}

#[test]
fn the_scan_finds_a_tool_name() {
    // Positive control for the literal scan itself.
    assert_eq!(
        sg_literals(r#"Command::new("sg_raw").arg("x"); let s = "sg_logs";"#),
        BTreeSet::from(["sg_raw".to_string(), "sg_logs".to_string()])
    );
    assert!(sg_literals(r#""sg_device" is a field, sg_x is not quoted"#).contains("sg_device"));
}

#[test]
fn only_the_asking_tools_and_the_page_tools_are_named() {
    let allowed: BTreeSet<&str> = ALLOWED.iter().map(|(t, _)| *t).collect();
    let mut tools: BTreeSet<String> = BTreeSet::new();
    for (path, text) in sources() {
        for lit in sg_literals(&text) {
            assert!(
                allowed.contains(lit.as_str()),
                "{} names \"{lit}\": a new SCSI-generic tool crosses the st/SG boundary — \
                 record it in docs/design/threat-model.md §2 (and here) first",
                path.display()
            );
            tools.insert(lit);
        }
    }
    // Every allowed tool is really in use, so this list cannot rot into a
    // superset that would wave a removed tool's replacement through.
    assert_eq!(
        tools,
        allowed
            .iter()
            .map(|t| t.to_string())
            .collect::<BTreeSet<_>>(),
        "the tools the source names"
    );
}

#[test]
fn sg_logs_only_ever_reads() {
    // LOG SELECT (`sg_logs --reset`/`-R`, `--select`/`-S`, a `--pcb` with
    // them) clears the drive's counters: the health history `report health`
    // is built from. The argv every page read uses, exactly — a flag added
    // to it fails here, including a harmless one, which is the point: the
    // boundary is reviewed, not assumed.
    use tapectl::tape::log_pages::SgLogs;
    assert_eq!(
        SgLogs::read_argv("/dev/sg9", 0x2e),
        [
            "sg_logs",
            "--page=0x2e",
            "--maxlen=65532",
            "--raw",
            "/dev/sg9"
        ]
    );
    // The offline decode reads the page's bytes from a file (a temporary one in
    // production) and touches no device.
    assert_eq!(
        SgLogs::decode_argv("/tmp/page.bin"),
        ["sg_logs", "--in=/tmp/page.bin", "--raw", "--pdt=1"]
    );
}

/// ADR-0012 item 163: the one page tapectl sets, exactly. The MODE SENSE
/// reads the current values; the MODE SELECT names page 0Ah/F0h, sends no
/// block descriptor (st's block length is st's), clears PS and is never
/// saved — a saved page would survive the power cycle that is the last way
/// to clear it (item 164). Any other page is refused before a program could
/// start. A flag added to either argv fails here, as for `sg_logs`.
#[test]
fn read_position_argv_is_pinned_to_one_command() {
    // ADR-0012 item 185: the one raw CDB tapectl sends. READ POSITION (34h),
    // short form (service action 00h, never st's vendor-specific 01h, which
    // the HP LTO-6 refuses), 20 bytes back in binary, the node opened
    // read-write (the sg layer refuses the command on `-R`). Any other CDB
    // byte, flag or data-out option fails here: `sg_raw` can send anything, so
    // the boundary is this argv.
    use tapectl::tape::lbp;
    let argv = lbp::read_position_argv("/dev/sg9");
    assert_eq!(
        argv,
        [
            "sg_raw", "-b", "-r", "20", "-t", "300", "/dev/sg9", "34", "00", "00", "00", "00",
            "00", "00", "00", "00", "00"
        ]
    );
    for word in &argv {
        for banned in [
            "-s",
            "--send",
            "-i",
            "--infile",
            "-c",
            "--cmdfile",
            "-R",
            "--readonly",
        ] {
            assert_ne!(word, banned, "{argv:?}");
            assert!(!word.starts_with(&format!("{banned}=")), "{argv:?}");
        }
    }
}

#[test]
fn lbp_page_argv_is_pinned_and_never_saves() {
    use tapectl::tape::lbp::{self, ProtectionPage, LBP_R, LBP_W, METHOD_RS, RS_INFO_LEN};
    assert_eq!(
        lbp::sense_argv("/dev/sg9"),
        [
            "sg_modes",
            "--page=0x0a,0xf0",
            "--control=0",
            "--raw",
            "/dev/sg9"
        ]
    );
    let hp = ProtectionPage::from_page_bytes(&[0x4a, 0xf0, 0x00, 0x04, 0, 0, 0, 0]).unwrap();
    let arming = hp.with_protection(METHOD_RS, RS_INFO_LEN, LBP_W | LBP_R);
    let select = lbp::select_argv("/dev/sg9", &arming).unwrap();
    assert_eq!(
        select,
        [
            "sg_wr_mode",
            "--page=0x0a,0xf0",
            "--dbd",
            "--contents=4a,f0,00,04,01,04,c0,00",
            "/dev/sg9"
        ]
    );
    let clearing = lbp::select_argv("/dev/sg9", &arming.cleared()).unwrap();
    assert_eq!(clearing[3], "--contents=4a,f0,00,04,00,00,00,00");
    // PS set on the page the drive reported comes out clear.
    let savable = ProtectionPage::from_page_bytes(&[0xca, 0xf0, 0x00, 0x04, 0, 0, 0, 0]).unwrap();
    let from_savable = lbp::select_argv("/dev/sg9", &savable).unwrap();
    assert!(
        from_savable[3].starts_with("--contents=4a,"),
        "{from_savable:?}"
    );
    for argv in [lbp::sense_argv("/dev/sg9"), select, clearing, from_savable] {
        for word in &argv {
            for banned in [
                "--save", "-s", "--force", "-f", "--mask", "-m", "-6", "--six",
            ] {
                assert_ne!(word, banned, "{argv:?}");
                assert!(!word.starts_with(&format!("{banned}=")), "{argv:?}");
            }
        }
    }
    // Another page never gets as far as an argv.
    assert!(ProtectionPage::from_page_bytes(&[0x4f, 0xf0, 0x00, 0x04, 0, 0, 0, 0]).is_err());
}

/// The program each `Command::new(…)` in `text` starts, as written (a
/// literal with its quotes, or the expression), outside the file's test
/// module and outside comments: what the binary itself can run.
fn spawned_programs(text: &str) -> Vec<String> {
    // The crate keeps a file's tests in a trailing `#[cfg(test)] mod tests`.
    let shipped = text
        .find("#[cfg(test)]\nmod tests")
        .or_else(|| text.find("#[cfg(test)]\npub(crate) mod tests"))
        .map_or(text, |i| &text[..i]);
    let mut found = Vec::new();
    for line in shipped.lines() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        let mut rest = line;
        while let Some(i) = rest.find("Command::new(") {
            let after = &rest[i + "Command::new(".len()..];
            let end = after.find(')').unwrap_or(after.len());
            found.push(after[..end].trim().to_string());
            rest = &after[end..];
        }
    }
    found
}

/// Every program the binary may start, by the file that starts it and the
/// expression it names. `"mt"`, `"sg_write_attr"` and the like are absent on
/// purpose: a movement of the medium belongs to the st driver through
/// `tape::ioctl`, so st's position accounting sees it, and SCSI generic only
/// asks. `sg_raw` runs through `lbp.rs`'s `&argv[0]` for one pinned READ
/// POSITION (ADR-0012 item 185).
const SPAWNS: &[(&str, &str)] = &[
    ("src/main.rs", "dar_path"),                  // the dar version probe
    ("src/dar/mod.rs", "binary"),                 // dar itself (create, restore, isolate)
    ("src/volume/build.rs", "\"bash\""),          // `bash -n` over the generated RESTORE.sh
    ("src/host_check.rs", "\"systemctl\""),       // whether contending units are active
    ("src/tape/drive_identity.rs", "\"sg_inq\""), // INQUIRY: the drive's identity
    ("src/tape/log_pages.rs", "\"sg_inq\""),      // INQUIRY: a health record's header
    ("src/tape/log_pages.rs", "LOG_TOOL"),        // `sg_logs -V`
    ("src/tape/log_pages.rs", "&argv[0]"),        // SgLogs::read_argv / decode_argv, pinned above
    ("src/tape/mam.rs", "MAM_TOOL"),              // sg_read_attr: READ ATTRIBUTE
    // sg_modes / sg_wr_mode: the Control Data Protection page (#388, ADR-0012
    // item 163), lbp::sense_argv / select_argv, pinned below; sg_raw: READ
    // POSITION's short form (item 185), lbp::read_position_argv, pinned below.
    ("src/tape/lbp.rs", "&argv[0]"),
    // `sh -c` over the operator's own `snapshot_command` / `release_command` of a
    // `[[collections]]` table (ADR-0012 2026-10-16 item 75): the operator's command
    // line, run as the user tapectl runs as, given the collection's name and root
    // and the snapshot's path and no device. tapectl does not choose what it runs.
    ("src/collection/hook.rs", "\"sh\""),
];

#[test]
fn the_spawn_scan_finds_a_program() {
    // Positive control for the scan itself: a spawn is found, one in a
    // comment or in the test module is not.
    assert_eq!(
        spawned_programs(
            "Command::new(\"mt\").arg(x); Command::new(MAM_TOOL);\n\
             /// `Command::new(binary)` in a doc comment\n\
             #[cfg(test)]\nmod tests { Command::new(\"awk\") }"
        ),
        ["\"mt\"", "MAM_TOOL"]
    );
}

#[test]
fn the_binary_starts_no_program_but_the_known_ones() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    for (path, text) in sources() {
        let file = path
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        for program in spawned_programs(&text) {
            assert!(
                SPAWNS.contains(&(file.as_str(), program.as_str())),
                "{file} starts {program}: not a program the binary is known to run. If it \
                 moves the medium or speaks SCSI, it crosses the st/SG boundary — record it \
                 in docs/design/threat-model.md §2 (and here) first"
            );
            seen.insert((file.clone(), program));
        }
    }
    // Every entry is really in use, so the list cannot rot into a superset.
    assert_eq!(
        seen,
        SPAWNS
            .iter()
            .map(|(f, p)| (f.to_string(), p.to_string()))
            .collect::<BTreeSet<_>>(),
        "the programs the source starts"
    );
}
