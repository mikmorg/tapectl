//! Golden pins for two on-tape documents whose BYTES must not move while
//! their generators are restructured (architecture review 2026-09-11,
//! candidates C3 and C6): the envelope `MANIFEST.toml` and `RESTORE.sh`.
//!
//! Written by the coordinator BEFORE the refactors were dispatched, so a
//! worker cannot move bytes and re-pin in the same change. If one of these
//! fails, the on-tape format changed — that is a CTO decision (bytes on tape
//! are forever), never something to fix by updating the constant here.
//!
//! **RE-PINNED ONCE, 2026-09-22, under an explicit CTO ruling on issue #288.**
//! The rule above held: the worker that moved the bytes was fenced out of this
//! file and reported the new hash rather than writing it, and the constant was
//! updated by the coordinator after reading the generated script. The ruling
//! covers exactly three changes, batched deliberately onto one re-pin so the
//! habit is not trained by cosmetic ones: repeated `--key` so an heir holding
//! a keyring can restore a volume whose envelope and slices sit on different
//! key generations (#288); the three "no envelope opened" exits all carrying
//! the tape's identity and the rotation cause, with the quoting defect that
//! made two of them print `'\''VOL-A'\''` fixed (#291); and the carried
//! `MB` -> `MiB` that #218 ruled should ride the next substantive change.
//! The CTO's own note on the timing: this is the cheapest moment the change
//! will ever be available, because after the first production write an
//! improved heir script means tapes in the archive disagree with each other
//! about their own recovery instructions.
//!
//! This paragraph is the record, not a precedent. The next failure of this
//! test is a CTO decision again.
//!
//! **RE-PINNED A SECOND TIME, 2026-09-29, under the CTO's ruling on issue
//! #349**, made the day before the first production write, for the same
//! reason the first re-pin gave: this is the cheapest moment the change will
//! ever be available. RESTORE.sh runs `tar xf -` to unpack an envelope but did
//! not check for `tar` up front, so an heir on a minimal system met a late,
//! confusing failure. The ruling covers exactly one line — `tar` appended to
//! the prerequisite loop — and the test below proves nothing else moved:
//! undoing that one word reproduces the previous pin.
//!
//! `MANIFEST.toml` carries a `created_at` timestamp; that one line is
//! normalised before comparison and is the only thing allowed to vary.

use sha2::{Digest, Sha256};
use tapectl::volume::layout::{
    generate_manifest_toml, generate_restore_script_v2, ManifestSlice, ManifestUnit,
};

fn sha256_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    format!("{:x}", h.finalize())
}

/// The RESTORE.sh a volume labelled GOLD01 with 12 files gets. The script is
/// pure substitution, so its hash is stable across runs and machines.
const RESTORE_SH_SHA256: &str = "299a34c018c131fc04da2ca69a6973a0293a4092ea917596b0a7f10697ca5b3b";
/// The pin before issue #349 — kept so the test can prove the re-pin moved
/// only the ruled line.
const RESTORE_SH_SHA256_PRE_349: &str =
    "bb29026cdf82975b8fb89e894f70f1dbb335a3ffbcde9e98ac3747d240a97f9a";

#[test]
fn restore_sh_bytes_are_pinned() {
    let script = generate_restore_script_v2("GOLD01", 12);
    let actual = sha256_hex(&script);
    if RESTORE_SH_SHA256 == "__RESTORE_SHA__" {
        eprintln!("CAPTURE restore_sh sha256 = {actual}");
    }
    assert_eq!(
        actual, RESTORE_SH_SHA256,
        "RESTORE.sh bytes changed. That is an on-tape format change and a CTO decision; \
         do not re-pin without one."
    );
    // Issue #349 moved exactly one line: undo it and the old bytes return.
    let ruled = "for tool in mt dd age sha256sum dar head truncate tar; do\n";
    assert_eq!(
        script.matches(ruled).count(),
        1,
        "the ruled prerequisite line"
    );
    let pre_349 = script.replace(
        ruled,
        "for tool in mt dd age sha256sum dar head truncate; do\n",
    );
    assert_eq!(
        sha256_hex(&pre_349),
        RESTORE_SH_SHA256_PRE_349,
        "something besides the #349 prerequisite line moved in RESTORE.sh"
    );
    // The two placeholders must both have been substituted.
    assert!(!script.contains("__LABEL__") && !script.contains("__TOTAL_FILES__"));
    assert!(script.contains("LABEL=\"GOLD01\""));
    assert!(script.contains("# Total files on tape: 12"));
}

fn golden_units() -> Vec<ManifestUnit> {
    vec![
        ManifestUnit {
            name: "photos/2019".to_string(),
            uuid: "11111111-2222-3333-4444-555555555555".to_string(),
            snapshot_version: 7,
            stage_set_id: 42,
            dar_version: Some("2.7.20".to_string()),
            dar_command: Some(r#"dar -c "x" -s 10G \ path"#.to_string()),
            slices: vec![
                ManifestSlice {
                    number: 1,
                    tape_position: 9,
                    size_bytes: 100,
                    encrypted_bytes: 120,
                    sha256_plain: "aa".repeat(32),
                    sha256_encrypted: "bb".repeat(32),
                },
                ManifestSlice {
                    number: 2,
                    tape_position: 10,
                    size_bytes: 50,
                    encrypted_bytes: 70,
                    sha256_plain: "cc".repeat(32),
                    sha256_encrypted: "dd".repeat(32),
                },
            ],
        },
        ManifestUnit {
            name: "ledgers/fy24".to_string(),
            uuid: "66666666-7777-8888-9999-000000000000".to_string(),
            snapshot_version: 1,
            stage_set_id: 43,
            dar_version: None,
            dar_command: None,
            slices: vec![ManifestSlice {
                number: 1,
                tape_position: 11,
                size_bytes: 5,
                encrypted_bytes: 25,
                sha256_plain: "ee".repeat(32),
                sha256_encrypted: "ff".repeat(32),
            }],
        },
    ]
}

/// Replace the one volatile line so the rest of the document can be pinned
/// byte-for-byte.
fn normalise_created_at(manifest: &str) -> String {
    manifest
        .lines()
        .map(|l| {
            if l.starts_with("created_at = ") {
                "created_at = \"<normalised>\"".to_string()
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + if manifest.ends_with('\n') { "\n" } else { "" }
}

const MANIFEST_EXPECTED: &str = r##"[manifest]
volume = "GOLD01"
tenant = "alpha"
created_at = "<normalised>"

[[units]]
name = "photos/2019"
uuid = "11111111-2222-3333-4444-555555555555"
snapshot_version = 7
stage_set_id = 42
dar_version = "2.7.20"
dar_command = "dar -c \"x\" -s 10G \\ path"

[[units.slices]]
number = 1
tape_position = 9
size_bytes = 100
encrypted_bytes = 120
sha256_plain = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
sha256_encrypted = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"

[[units.slices]]
number = 2
tape_position = 10
size_bytes = 50
encrypted_bytes = 70
sha256_plain = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
sha256_encrypted = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"

[[units]]
name = "ledgers/fy24"
uuid = "66666666-7777-8888-9999-000000000000"
snapshot_version = 1
stage_set_id = 43

[[units.slices]]
number = 1
tape_position = 11
size_bytes = 5
encrypted_bytes = 25
sha256_plain = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
sha256_encrypted = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"

"##;

#[test]
fn manifest_toml_bytes_are_pinned_except_created_at() {
    let manifest = generate_manifest_toml("GOLD01", "alpha", &golden_units());
    let actual = normalise_created_at(&manifest);
    if MANIFEST_EXPECTED == "__MANIFEST__" {
        eprintln!("CAPTURE manifest =\n{actual}\nCAPTURE end");
    }
    // Exactly one created_at line, and it is RFC 3339.
    let stamps: Vec<&str> = manifest
        .lines()
        .filter(|l| l.starts_with("created_at = "))
        .collect();
    assert_eq!(stamps.len(), 1, "{stamps:?}");
    assert!(
        stamps[0].contains('T') && stamps[0].ends_with('"'),
        "{}",
        stamps[0]
    );
    assert_eq!(
        actual, MANIFEST_EXPECTED,
        "MANIFEST.toml bytes changed (created_at excluded). That is an on-tape format \
         change and a CTO decision; do not re-pin without one."
    );
}
