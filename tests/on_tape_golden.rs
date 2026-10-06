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
//! the prerequisite loop — and the test proved nothing else moved: undoing
//! that one word reproduced the previous pin (`bb29026c…`).
//!
//! **RE-PINNED A THIRD TIME, 2026-09-30, under the CTO's ruling recorded in
//! ADR-0012's 2026-09-30 amendment**, made while the first production
//! collection was still staging and before its first tape was written. The
//! pre-production structural review found that `--restore` decrypted a whole
//! unit into `${TMPDIR:-/tmp}` before dar ran, with no space check: on a
//! machine whose /tmp is RAM an heir could restore almost none of that tape's
//! bytes, and running out of space ended the script with no message or was
//! reported as a wrong key. The ruling covers one batch, taken on one re-pin:
//! slices decrypted into a scratch directory inside `--to` (or `--scratch
//! DIR`); a space check before the first slice is read (`--no-space-check`
//! skips it); out-of-space failures named as such; `--verify` hashing each
//! tape file as it streams instead of copying it to /tmp; any layout_version
//! other than 2 refused with the way to the tape's own script (a missing one
//! warns); and `tar` in the two Requirements lines (#363). The test checks
//! each item is present. The previous pin was `299a34c0…`.
//!
//! **RE-PINNED A FOURTH TIME, 2026-10-06, for tapectl 1.1.0, under the CTO's
//! rulings recorded in ADR-0012's 2026-10-06 amendments** (the minor version
//! moves because generated on-tape bytes change). One batch, one re-pin,
//! reviewed by the CTO: forward-only tape navigation with a cursor checked
//! against `mt status` (#396); a non-empty `--to` refused unless
//! `--overwrite`, dar's skip line fatal, and the rung-3 zero-strip recipe
//! stripping only trailing padding (#405); streaming restore into dar through
//! FIFOs, `--all` and repeated `--unit`, `--find-envelope` listing every
//! envelope, damage told apart from a wrong key, `mt-st` detection, the seal
//! read last, and dar `-N` (#412), with the envelope's dar catalogue given to
//! a streaming dar (2.7.13 loses a file's tail without it) and a fall-back
//! to slices on disk, and a dar that needs `gpg` named up front; `--list`
//! and `--path` reading the dar catalogues the envelopes already carry
//! (#418); and `magic`/`requires`
//! checks with the seal found at end of data (ADR-0012 amendment item 15,
//! #384). The test checks each item is present. The previous pin was
//! `6b64da4b…`.
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
const RESTORE_SH_SHA256: &str = "e8ca5902f62730f926e310393989d02978ac68a1f19901de0d666793706be9c4";

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
    // The prerequisite check: tar (#349), mt or mt-st, and the FIFO tools
    // the streaming restore needs (1.1.0).
    assert_eq!(
        script
            .matches("for tool in dd age sha256sum dar head truncate tar mkfifo tee; do\n")
            .count(),
        1,
        "the prerequisite line"
    );
    // Every item of the 2026-09-30 ruling still standing, and of 1.1.0's, is
    // in the pinned bytes.
    for ruled in [
        // 2026-09-30
        "--scratch) scratch=$2 ;;",
        "--no-space-check)\n      SKIP_SPACE_CHECK=1",
        "OUT OF DISK SPACE in $where",
        "check_layout_version \"$(toml_val \"$WORK/thunk.toml\" layout_version)\"",
        "# Requirements: mt (mt-st), dd, age, dar, sha256sum, head, truncate, tar, mkfifo, tee\n",
        "echo \"Requirements: mt (mt-st), dd, age, dar, sha256sum, head, truncate, tar, mkfifo, tee\"",
        // #396: forward-only navigation, cursor checked against mt status
        "\"$MT\" -f \"$DEVICE\" fsf $((pos - from))",
        "elif [ \"$here\" != \"$from 0\" ]; then",
        // #405: an empty --to, dar's skip line, the trailing-only strip
        "check_destination \"$destdir\" \"$scratch_parent\"",
        "--overwrite)\n      OVERWRITE=1",
        "skipped=$(grep 'not restored (user choice)$' \"$1\" 2>/dev/null |",
        "head -c $((size - tail_len + keep)) candidate.raw > candidate.stripped",
        // #412: streaming, -N, --all, every envelope, damage, mt-st
        "local -a dar_opts=(-O -Q -N --sequential-read)",
        // dar 2.7.13 loses a file's tail streaming without the catalogue:
        // -A from the envelope, else stream only on 2.7.21+, else spool;
        // a dar that needs gpg is named before the tape is read
        "[ -z \"$cat\" ] || dar_opts+=(-A \"$cat\")",
        "if [ -n \"$cat\" ] && dar_at_least 2.7.9; then",
        "if dar_at_least 2.7.21; then",
        "spool_unit \"$list\" \"$destdir\"",
        "grep -i 'INITIALIZATION FAILED FOR GPGME' >/dev/null; then",
        "mkfifo \"$fifos/restore.$num.dar\"",
        "check_space \"$destdir\" \"$destdir\" \"$total\" 0",
        "--all)\n      RESTORE_ALL=1",
        "walk_envelopes found_envelope",
        "if grep 'no identity matched' \"$WORK/age.err\" >/dev/null 2>&1; then\n      info \"  key $keyfile did not open the envelope at file $pos\"",
        "MT=mt-st",
        // #418: the envelope's dar catalogues
        "--list)\n  shift",
        "--path) RESTORE_PATHS+=(\"$2\") ;;",
        // ADR-0012 item 15 (#384): requires, the seal at end of data
        "check_requires \"File 0\" \"$WORK/thunk.toml\"",
        "\"$MT\" -f \"$DEVICE\" bsfm 2 2>/dev/null || return 1",
    ] {
        assert_eq!(script.matches(ruled).count(), 1, "ruled item {ruled:?}");
    }
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
