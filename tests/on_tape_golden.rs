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
//! Re-pinned 2026-10-12 under ADR-0012 2026-10-12 item 44 (within the
//! 2026-10-07 on-tape authorization): every tenant envelope has a backup
//! copy on tape (#418 item 2, `volume-format-v2.md` §1.3), which RESTORE.sh
//! walks, reading a backup only when its primary is damaged or does not
//! read. The previous pin was `e8ca5902…`.
//!
//! **RE-PINNED 2026-10-16 for tapectl 1.2.0, under the CTO's "Approve re-pin"
//! (ADR-0012 2026-10-16 items 71-74, #449 and #448's File 1 half),** bounded by
//! item 72: this pin only, ending when v1.2.0 is installed. Five changes to
//! RESTORE.sh's bytes (File 1 and RECOVERY.md also change that day, but they
//! are not pinned here): a lone-unit restore (no `--unit`, no `--all`) says,
//! before RESTORE COMPLETE, how many other envelopes its key opened and how
//! many other units they list, and names `--list` and `--all` (it reads the
//! remaining envelopes first, forward only, and skips that where the work
//! directory could not hold the largest); each mode checks the tools it runs
//! (`TOOLS_READ`, `TOOLS_ENVELOPE`, `TOOLS_LIST`, `TOOLS_PATH`,
//! `TOOLS_RESTORE`, drawn from one table in `layout.rs`) instead of every tool
//! in every mode; the DAMAGED message says "a key matched" only when one did;
//! the work directory is described as it is (the decrypted envelopes, in
//! `${TMPDIR:-/tmp}`), with a pre-read estimate and a note that the space
//! check counts the slices' encrypted size; and `export LC_ALL=C`. The
//! Requirements lines and every other ruled item below are byte-identical.
//! Two things did not move (item 73) and are asserted below: "tapectl 1.1.0" in
//! `die_newer_needed`, and `requires` (empty, `KNOWN_FEATURES=""`). The
//! MANIFEST.toml pin did not move. The previous pin was `2a09fe98…`.
//!
//! **RE-PINNED 2026-10-17 for tapectl 1.3.0, under the CTO's rulings in
//! ADR-0012's 2026-10-16 amendment (items 78-81, #418 R4),** bounded by item 81:
//! this pin only, ending when v1.3.0 is installed. RESTORE.sh gains two modes
//! and nothing it already did changes (the diff against the previous script is
//! additions alone): `--where UNIT` prints the rows of the cross-volume index's
//! `index/units.tsv` for a unit (every version, its slices and bytes, and each
//! tape holding it), from the operator envelope with an operator or escrow key
//! and the tenant's with a tenant key; `--find TEXT` prints the paths of each
//! tenant envelope's `index/paths.tsv` that contain a literal piece of text
//! (matched on the decoded path, at most 50 printed, then the holders of each
//! version printed); a tape with no index says so and exits 1. Both read
//! forward from one rewind, take their tools from one new row of the per-mode
//! table (`TOOLS_INDEX`, the `--find-envelope` set), use three named awk
//! fragments (`AWK_INDEX_COMMON`, `AWK_INDEX_WHERE`, `AWK_INDEX_FIND`) that read
//! the same under gawk, mawk, nawk and busybox awk, and are in `--help`, the
//! header comment and `--info`'s closing hints. The `Requirements:` lines, the
//! other four `TOOLS_*` sets and every ruled item below are byte-identical. The
//! two fixed points of item 81 did not move and are asserted below: "tapectl
//! 1.1.0" in `die_newer_needed` (the only "tapectl 1." in the script), and
//! `requires` (empty, `KNOWN_FEATURES=""`). The MANIFEST.toml pin did not move.
//! The previous pin was `6c7bb948…`.
//!
//! **The 1.3.0 pin WIDENED, 2026-10-17, under the CTO's rulings in ADR-0012's
//! 2026-10-17 amendment (items 101 and 102, #480),** within the same bound:
//! this pin only, ending when v1.3.0 is installed (item 81 says the re-pin
//! widens to carry RESTORE.sh's half of both). The first 1.3.0 pin
//! (`36c970aa…`, `--where` and `--find`) was ADDITIONS ALONE; this one is `594da829…`. This widening is
//! not, and says so. ADDED: `--slices-from DIR` (repeatable) and `--swap-tapes`
//! on `--restore` and `--verify`, which take a slice that fails its ciphertext
//! hash or will not read from another copy of the unit, matched by its
//! `sha256_encrypted` from the front index and by nothing else (a `dd` copy,
//! block padding and all, is trimmed to the size the index states), from a
//! directory or from a second cartridge read forward from one rewind after the
//! first (`other_tape_map`, `swap_fetch`, `swap_loop`); a slice still missing
//! is left out and listed, never reported complete; `--fill-from LIST` on
//! `--restore`, which restores only the files a `.tapectl-damaged-files.txt`
//! names, from a tape whose envelope lists the same unit uuid at the same
//! version, overwriting only those files (`do_fill`); `dar_result`, which reads
//! the files dar says it could not restore (`AWK_DAR_DAMAGED`, a new named
//! awk fragment, as is `AWK_FILL_LISTING`) into that list; and the `--help`,
//! header-comment and hint text for all of it. CHANGED, and the reason a
//! reader should look: (1) the end of every restore is now `restore_end`,
//! which prints `RESTORE COMPLETE` only when no unit came back short and
//! otherwise `RESTORE INCOMPLETE` with exit status 1 (it was printed
//! unconditionally after the last unit); (2) the dar tail of `stream_unit`,
//! `spool_unit` and `restore_paths` goes through `dar_result`: status 5 with
//! the files named is a list and INCOMPLETE, a file-system refusal (permission
//! denied, read-only, disk quota) and every other status stay fatal as before;
//! (3) every dar gets an absolute `-R`, because dar prints it word for word in
//! its messages; (4) `check_destination` is split, the stale-scratch refusal
//! becoming `check_stale_scratch` so a fill, whose directory is full by
//! definition, bypasses only the emptiness refusal; (5) `decrypt_slice` takes an
//! optional byte count and `spool_slices` a target directory and a file for
//! the slices not found; (6) `--verify` has an option parser and names, for a
//! failed data slice, whether a copy is in a `--slices-from` directory or on a
//! second cartridge; (7) `stream_unit` reports an age failure that names a
//! decrypt, an authentication or a header as the slice's before asking whether
//! dar is gone (a damaged chunk shortens the slice and dar can have died of it
//! first, which hid the slice behind dar's exit); (8) a bad slice's failure
//! names `--slices-from` and `--swap-tapes`, and the rejected-key message says a
//! damaged slice reads that way too. A restore with neither option is
//! unchanged except for those messages and the dar tail above. The two fixed
//! points did not move and are asserted below: "tapectl 1.1.0" in
//! `die_newer_needed`, and `requires` (empty, `KNOWN_FEATURES=""`). The
//! MANIFEST.toml pin did not move. The previous pin was `36c970aa…`.
//!
//! **The 1.3.0 pin WIDENED AGAIN, 2026-10-19, under ADR-0012 item 103 (the
//! heir checks every restored file against its recorded sha256, #418),**
//! within the same bound: this pin only, ending when v1.3.0 is installed. The
//! previous pin was `594da829…`; this one is `072b0c6e…`. dar's per-file check
//! is a short checksum where equal byte flips cancel, so a damaged file could
//! come back with no error; the tenant envelope's `index/paths.tsv` now carries
//! each regular file's recorded sha256 (a `sha256` column between `size_bytes`
//! and `path`, found by its header name; a reader that does not know the column
//! ignores it, so it needs no `requires` entry), and RESTORE.sh checks the
//! files it restored against it. ADDED: `--no-file-check` on `--restore`
//! (`--fill-from` too); `file_check`, which reads the restored files back from
//! `--to` and checks them with one `sha256sum -c --quiet` pass over the unit's
//! hashed regular files (a name holding a backslash, a newline or a carriage
//! return is hashed alone, from standard input, because `sha256sum` escapes
//! such a name differently between versions); `index_for_unit`, which finds the
//! `paths.tsv` that lists the unit and version among the envelopes the run has
//! open (the one in use first; an escrow or operator key restores from the
//! operator envelope, which has none, and the tenant envelope that lists the
//! unit was opened before it); `finish_damage`; three named awk fragments,
//! `AWK_FILE_CHECK_LISTED`, `AWK_FILE_CHECK_LIST` and `AWK_FILE_CHECK_FAILED`;
//! and the header-comment, `--help` and message text for all of it. CHANGED:
//! `dar_result` now always ends in `finish_damage`, which runs the check and
//! unions the files dar named with the ones the check finds into one
//! damaged-file list, so `record_damage` and `clear_stale_list` are reached
//! through it only; `damage_note` has three wordings, by how many files the
//! check added; `fill_end` checks the files the fill restored and keeps a
//! failing one in the list. No tool was added: `sha256sum` was in every mode's
//! set already, and the `TOOLS_*` lines did not move. A tape with no index
//! (written before 1.3.0), or a file with no sha256 recorded, is not checked,
//! and the script says so. The tape is not touched by the check (it reads the
//! restored files and the envelope already on disk). The fixed points did not
//! move and are asserted below: "tapectl 1.1.0" in `die_newer_needed`,
//! `requires` (empty) and `KNOWN_FEATURES=""`. The MANIFEST.toml pin did not
//! move.
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
const RESTORE_SH_SHA256: &str = "072b0c6ef7d7639274d194ee8fea3f382ad6ed4e320c80ec515fc3e1d99d08f4";

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
    // the streaming restore needs (1.1.0); per mode since 1.2.0 (#449): each
    // mode checks only what it runs, and --restore's set is the union.
    assert!(
        !script.contains("for tool in dd age"),
        "no mode checks every tool any more"
    );
    for line in [
        "# --info, --verify:\nTOOLS_READ=(dd sha256sum head)\n",
        "# --find-envelope:\nTOOLS_ENVELOPE=(dd age sha256sum head truncate tar)\n",
        "# --list:\nTOOLS_LIST=(dd age dar sha256sum head truncate tar)\n",
        "# --restore --path:\nTOOLS_PATH=(dd age dar sha256sum head truncate tar tee)\n",
        "# --restore, whole units (every tool any mode may run):\nTOOLS_RESTORE=(dd age dar sha256sum head truncate tar mkfifo tee)\n",
        "# --where, --find (the envelopes' index, read as plain text):\nTOOLS_INDEX=(dd age sha256sum head truncate tar)\n",
        "require_tools() { # <tool>...\n",
    ] {
        assert_eq!(script.matches(line).count(), 1, "the prerequisite set {line:?}");
    }
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
        // 1.2.0 (ADR-0012 2026-10-16 item 71, #449): locale, per-mode tools,
        // the survey behind the lone-unit note, "a key matched" only when one
        // did, the work directory and its estimate
        "\nexport LC_ALL=C\n",
        "require_tools \"${TOOLS_RESTORE[@]}\"",
        "require_tools \"${TOOLS_PATH[@]}\"",
        "walk_envelopes count_other_envelope \"$CHOSEN_POS\"",
        "survey_other_envelopes \"${unit_names[0]}\"",
        "restore_unit() { # <slices file> <destination> <unit> <version>\n",
        "  if [ \"$MATCHED_DAMAGED\" -gt 0 ]; then\n    die \"$headline that could be opened.",
        "    MATCHED_DAMAGED=$((MATCHED_DAMAGED + 1))\n",
        "# WORK is the work directory, made in ${TMPDIR:-/tmp}: TMPDIR moves it.",
        "check_work_space() {",
        "  info \"            (counted from the slices' encrypted size on tape; a compressed unit may need more)\"",
        "  echo \"  TMPDIR        Where the work directory is made (default: /tmp). It holds the\"",
        // 1.3.0 (ADR-0012 2026-10-16 items 78-81, #418 R4): the cross-volume
        // index, read by --where and --find
        "--where)\n  shift",
        "--find)\n  shift",
        "  require_tools \"${TOOLS_INDEX[@]}\"\n  require_key_files\n  do_where \"$where_unit\"",
        "  require_tools \"${TOOLS_INDEX[@]}\"\n  require_key_files\n  do_find \"$find_text\"",
        "  walk_envelopes where_envelope\n",
        "  walk_envelopes find_in_envelope\n",
        "index_banner() {\n",
        "die_no_index() {\n",
        // 1.3.0, widened (ADR-0012 2026-10-17 items 101, 102, #480): a slice
        // from another copy by its sha256, the damaged-file list, the fill
        "--slices-from) SLICES_FROM+=(\"$2\") ;;",
        "--fill-from) FILL_FROM=$2 ;;",
        "    --all)\n      RESTORE_ALL=1\n      shift\n      ;;\n    --swap-tapes)\n      SWAP_TAPES=1",
        "    --swap-tapes)\n      SWAP_TAPES=1\n      shift\n      ;;\n    *) die \"unknown option: $1\n\n       usage: $0 --verify",
        "    --slices-from)\n      [ $# -ge 2 ] || die \"--slices-from needs a directory",
        "check_stale_scratch() { # <directory>...\n",
        "decrypt_slice() { # <ciphertext> <plaintext-out> <slice-number> [bytes]\n",
        "spool_slices() { # <slices file> [<directory> [<file for the slices not found>]]\n",
        "      if [ \"$(dir_sum \"$f\" \"$size\")\" = \"$sha\" ]; then\n",
        "other_tape_map() {\n",
        "swap_fetch() {\n",
        "swap_loop() {\n",
        "    [dD]one | DONE | q | quit)\n",
        "DAMAGED_NAME=.tapectl-damaged-files.txt\n",
        "dar_result() { # <log> <status> <destination> [slices not available]\n",
        "  dar_result \"$WORK/extract.log\" \"$rc\" \"$destdir\"\n}\n\n# Read, check and decrypt the slices listed in $1",
        "  SCRATCH=\"\"\n  dar_result \"$WORK/extract.log\" \"$rc\" \"$destdir\"\n}\n",
        "  dar_result \"$WORK/extract.log\" \"$rc\" \"$destdir\"\n  if [ \"$UNIT_OK\" = 1 ]; then",
        "do_fill() { # <destination> <version given, or empty> <scratch parent, or empty>\n",
        "restore_end() {\n",
        "    info \"RESTORE INCOMPLETE\"\n",
        "  info \"RESTORE COMPLETE\"\n  for line in ${DONE_LINES[@]+\"${DONE_LINES[@]}\"}; do",
        "  finish_pending\n  restore_end\n",
        "dar -x \"$fifos/restore\" -R \"$(abs_dir \"$destdir\")\" \"${dar_opts[@]}\"",
        "dar -x \"$SCRATCH/job.$j/restore\" -R \"$(abs_dir \"$destdir\")\"",
        "    do_fill \"$dest\" \"$want\" \"$scratch\"\n",
        "  echo \"  $0 --restore --key KEYFILE --to DIR --unit U --fill-from LIST\"",
        // 1.3.0, widened again (ADR-0012 item 103): the restored files against
        // the sha256 their index records
        "    --no-file-check)\n      FILE_CHECK=0\n      shift\n      ;;\n",
        "index_for_unit() { # <unit> <version> <unit uuid>\n",
        "file_check() { # <destination> [<file of escaped paths to check>]\n",
        "finish_damage() { # <destination> <slices not available, or empty>\n",
        "  file_check \"$root\" \"$WORK/fill.restored\"\n",
        "  (cd \"$destdir\" && sha256sum -c --quiet) <\"$WORK/check.sums\"",
    ] {
        assert_eq!(script.matches(ruled).count(), 1, "ruled item {ruled:?}");
    }
    // The two placeholders must both have been substituted.
    assert!(!script.contains("__LABEL__") && !script.contains("__TOTAL_FILES__"));
    assert!(!script.contains("__AWK_") && !script.contains("__TOOLS_"));
    // ADR-0012 2026-10-16 item 73: the two things the re-pin does not move.
    assert_eq!(
        script
            .matches("This RESTORE.sh (layout v2, tapectl 1.1.0) cannot read it safely")
            .count(),
        1,
        "die_newer_needed still says tapectl 1.1.0"
    );
    assert_eq!(
        script.matches("tapectl 1.").count(),
        1,
        "no other version named"
    );
    assert_eq!(
        script.matches("\nKNOWN_FEATURES=\"\"\n").count(),
        1,
        "RESTORE.sh still knows no feature a tape could require"
    );
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
