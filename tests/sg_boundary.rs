//! Issue #394: the st / SG boundary (`docs/design/threat-model.md` §2).
//!
//! The st driver does every data transfer and every movement of the medium;
//! SCSI generic is used only to ASK — INQUIRY (`sg_inq`), LOG SENSE
//! (`sg_logs`) and READ ATTRIBUTE (`sg_read_attr`) — so the st driver's own
//! position accounting can never drift under a command it did not see. This
//! pins the set of sg3-utils tools the binary's source names, the absence of
//! the `sg_logs` flags that turn a LOG SENSE into a LOG SELECT (which clears
//! the drive's counters), and that the binary never runs `mt` or `sg_raw`.
//!
//! A source scan, deliberately: the tools are spawned from half a dozen
//! modules and several only on real hardware, so no ungated run reaches them
//! all. Adding a tool here is a decision to record in the threat model first.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The sg3-utils tools tapectl may run, and the one SCSI command each sends.
const ALLOWED: &[(&str, &str)] = &[
    ("sg_inq", "INQUIRY"),
    ("sg_logs", "LOG SENSE"),
    ("sg_read_attr", "READ ATTRIBUTE"),
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
fn only_the_three_asking_tools_are_named() {
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
    // The offline decode reads bytes from stdin and touches no device.
    assert_eq!(
        SgLogs::decode_argv(),
        ["sg_logs", "--in=-", "--raw", "--pdt=1"]
    );
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
/// expression it names. `"mt"`, `"sg_raw"`, `"sg_write_attr"` and the like
/// are absent on purpose: a movement of the medium belongs to the st driver
/// through `tape::ioctl`, so st's position accounting sees it, and SCSI
/// generic only asks.
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
