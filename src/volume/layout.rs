/// Parameters for the v2 ID thunk (File 0). Layout v2 collapses v1's 18
/// positional arguments to this struct and drops every per-file position
/// field (sheet §2.3): File 0 now carries identity plus two pointers only —
/// `front_index` (always File 3) and `seal_marker` (always the last file).
/// Every other position/type/size fact lives solely in the front index
/// itself, as `[[files]]` entries (see `generate_front_index`).
#[derive(Debug, Clone, Copy)]
pub struct IdThunkV2Params<'a> {
    pub label: &'a str,
    pub uuid: &'a str,
    pub media_type: &'a str,
    /// The writer's PACKAGE version (`CARGO_PKG_VERSION`), never the build
    /// identity `build_info::VERSION` — File 0 is an on-tape byte and
    /// ADR-0012 (2026-09-24 amendment, item 1) adds no writer-commit field
    /// to it. `build_info::tests::on_tape_writer_string_is_the_package_version`
    /// pins the `volume::write` call site.
    pub tapectl_version: &'a str,
    pub nominal_capacity: i64,
    pub mam_capacity: i64,
    pub total_files: i32,
    pub mam_manufacturer: &'a str,
    pub mam_serial: &'a str,
    pub mam_length: i64,
    pub mam_loads: i64,
    /// RFC 3339 generation timestamp, rendered by the caller (`build()`) and
    /// injected rather than read from the clock in here (v2-implementation-plan
    /// T6 review finding #5). Without this, only `system_guide`/`restore_sh`
    /// were testable for build-twice byte-identity — the ID thunk's `created_at`
    /// varied on every call and could never be held constant across two builds
    /// in a test, leaving `layout-session.md`'s "same inputs + same generation
    /// timestamp ⇒ reproducible Layout" clause unverified for this zone. This
    /// also matters for resume, which depends on frozen (not regenerated)
    /// bytes — see `ContentSource::Materialized`'s doc comment.
    pub created_at: &'a str,
    /// How `cartridge_serial` was established — `"mam"` (the cartridge's own
    /// chip reported it) or `"operator"` (no serial was readable when this
    /// volume was bound, so the operator named the cartridge with
    /// `volume init --cartridge <barcode>` and `cartridge_serial` carries
    /// THAT barcode). `docs/design/volume-format-v2.md` §1.1, ADR-0012.
    ///
    /// `None` OMITS the line entirely, and that is load-bearing: absent means
    /// UNKNOWN and must never read as `"mam"`. Every tape written before this
    /// field existed omits it, and those tapes stay readable forever — a
    /// reader that defaulted the absent case to `"mam"` would make all of
    /// them falsely attest a chip-verified serial.
    ///
    /// Injected, like `created_at`, rather than derived from a live MAM read
    /// in here: the value records how the identity was established AT THE
    /// BINDING (read back from `cartridge_volumes.identity_source`), not what
    /// the drive happens to report at this contact. Deriving it from the
    /// contact would make the same physical tape attest different provenance
    /// depending on which drive wrote it.
    pub cartridge_identity_source: Option<&'a str>,
}

/// Generate the v2 ID thunk (File 0) content. Per sheet §2.3 and
/// `docs/design/volume-format-v2.md` §1: `[volume]` carries identity
/// (unchanged in spirit from v1, plus a new `uuid` field) and `[layout]`
/// carries ONLY `front_index = 3`, `seal_marker` (the last file), and
/// `total_files` — the v1 `data_start`/`data_end`/`first_envelope`/
/// `num_envelopes`/`mini_index`/operator-position fields are gone: they are
/// superseded by File 3's `[[files]]` entries, which carry position, type,
/// size, and ciphertext hash for every file. The human-readable header keeps
/// v1's structure, but its pointer text now says the map is File 3, not "the
/// next file."
///
/// `volume_init` writes this as a **provisional** identity stamp (positions
/// unknown at init time); the write session rewrites File 0 from BOT with
/// the real `total_files`/`seal_marker` once the layout is built (sheet
/// §2.3 — resume must not try to preserve init's File 0).
pub fn generate_id_thunk_v2(params: &IdThunkV2Params) -> String {
    let IdThunkV2Params {
        label,
        uuid,
        media_type,
        tapectl_version,
        nominal_capacity,
        mam_capacity,
        total_files,
        mam_manufacturer,
        mam_serial,
        mam_length,
        mam_loads,
        created_at: now,
        cartridge_identity_source,
    } = *params;
    let seal_marker = total_files - 1;
    // Absent means unknown (§1.1): an unknown provenance emits NO line at
    // all, so a pre-#192 File 0 and a post-#192 one with nothing to say are
    // byte-identical. Rendered as a whole line here (rather than as a value
    // spliced into the template) precisely so `None` can contribute zero
    // bytes — a template with an empty value would still leave the key.
    let identity_source_line = match cartridge_identity_source {
        Some(src) => format!("cartridge_identity_source = \"{src}\"\n"),
        None => String::new(),
    };
    format!(
        r#"================================================================
                     TAPECTL ARCHIVAL VOLUME
================================================================

Label:   {label}
Media:   {media_type}
Created: {now}

This tape contains encrypted archival data managed by tapectl,
an open-source archival storage tool.

>>> COMPLETE INSTRUCTIONS ARE IN THE NEXT FILE ON THIS TAPE. <<<
>>> THE FULL MAP OF THIS TAPE IS FILE 3 (the front index).   <<<

Your drive may not be /dev/nst0. That is an example, not a fact
about this tape. List the tape drives on the machine you are
using and substitute the right one everywhere below:

    ls -l /dev/tape/by-id/

A wrong device reads a DIFFERENT tape, or fails outright.

To read the next file (the full recovery guide):

    mt -f /dev/nst0 setblk 524288
    mt -f /dev/nst0 fsf 1
    dd if=/dev/nst0 bs=512k | tr -d '\0' > GUIDE.md
    less GUIDE.md

If you just read this file and the tape is already positioned
past it, read the next file directly:

    mt -f /dev/nst0 setblk 524288
    dd if=/dev/nst0 bs=512k | tr -d '\0' > GUIDE.md

The guide explains everything: what tools you need, how to find
your encryption key, and how to recover your data step by step.
It is written so that an AI assistant can follow it to help you.

================================================================
              MACHINE-READABLE METADATA (TOML)
================================================================

[volume]
magic = "tapectl-volume-v2"
label = "{label}"
uuid = "{uuid}"
layout_version = 2
tapectl_version = "{tapectl_version}"
media_type = "{media_type}"
nominal_capacity_bytes = {nominal_capacity}
mam_capacity_bytes = {mam_capacity}
created_at = "{now}"

[layout]
front_index = 3
seal_marker = {seal_marker}
total_files = {total_files}

[media]
cartridge_manufacturer = "{mam_manufacturer}"
cartridge_serial = "{mam_serial}"
{identity_source_line}tape_length_meters = {mam_length}
load_count_at_write = {mam_loads}
"#
    )
}

/// Generate the v2 system guide (File 1) — the heir manual for layout v2.
/// Rewrites v1's Quick Reference to the v2 zone order (front index at File 3,
/// tenant envelopes from File 4, seal marker last with an embedded copy of
/// the front index); replaces every "mini-index" reference with File 3 (the
/// v1 mid-tape mini-index no longer exists — `volume-format-v2.md` §8); adds
/// the accepted size-disclosure statement (§2 "Accepted disclosure") and the
/// three-rung degradation ladder with the zero-strip procedure (sheet §3.3,
/// §3.4).
pub fn generate_system_guide_v2(label: &str, total_files: i32) -> String {
    format!(
        r#"# tapectl Archival Volume Recovery Guide

## Volume: {label}

This document describes how to recover data from this tape without
tapectl or its database. All you need is: mt, dd, age, dar, sha256sum,
head, truncate, and tar.

## Quick Reference

This volume uses layout v2. Tape files are laid out in this fixed order:

- File 0: ID thunk (this tape's identity; says the map is File 3)
- File 1: This guide
- File 2: RESTORE.sh (automated restore + verify script)
- File 3: FRONT INDEX — every file's position, type, on-tape size, and
  ciphertext hash. Read this first for a full map of the tape.
- File 4 onward: Tenant envelope(s) — one per tenant sharing this tape
- Then: Operator envelope, then Operator envelope backup
- Then: Data slices (age-encrypted dar archives) — one unit's slices
  are stored contiguously
- LAST file: SEAL MARKER — asserts the tape is complete, and carries a
  full embedded copy of the front index (used if File 3 is damaged)

## Tools Required

- `mt` (mt-st package) — tape positioning
- `dd` — reading raw data from tape
- `age` (age-encryption.org) — decryption
- `dar` (dar.linux.free.fr) — archive extraction
- `sha256sum` (coreutils) — integrity verification
- `head`, `truncate` (coreutils) — trimming block padding to exact sizes
- `tar` — unpacking your decrypted envelope (it is a tar archive)

## Which device?

Every command in this guide says `/dev/nst0`. That is an example, not a
fact about this tape — drive numbering belongs to the machine you are
sitting at, and it changes when hardware is added or the machine reboots.
List the tape drives and substitute the right one throughout:

    ls -l /dev/tape/by-id/

A wrong device does not always fail loudly: if another tape is loaded in
it, the commands below will succeed and recover the WRONG volume. File 0
names the volume this tape carries; RESTORE.sh checks that for you and
warns (it takes `TAPE_DEVICE=/dev/nstN` to point it at another drive).

## Automated Recovery (recommended)

The easiest way to recover is the RESTORE.sh script (File 2):

    mt -f /dev/nst0 rewind && mt -f /dev/nst0 fsf 2
    dd if=/dev/nst0 bs=512k | tr -d '\0' > RESTORE.sh
    chmod +x RESTORE.sh

    # See what's on the tape and its seal verdict
    # (SEALED / UNSEALED / DAMAGED (ends disagree))
    ./RESTORE.sh --info

    # Keyless integrity check of every file against the front index —
    # no key needed, works even without your envelope
    ./RESTORE.sh --verify

    # Find your encrypted envelope
    ./RESTORE.sh --find-envelope --key your-key.age.key

    # Full restore to a directory. Every slice of the unit is decrypted
    # to disk before dar extracts it, so /destination needs about twice
    # the unit's size free, plus one slice (--scratch DIR puts the
    # decrypted slices on another disk). The script checks first.
    ./RESTORE.sh --restore --key your-key.age.key --to /destination

## Manual Recovery Steps

If RESTORE.sh is not available, follow these steps:

1. Set tape to fixed 512KB block mode: `mt -f /dev/nst0 setblk 524288`
2. Read the ID thunk (File 0) — it confirms File 3 is the front index
   and tells you which file is the seal marker (the last file on tape)
3. Read File 3, the front index, for exact byte sizes and ciphertext
   hashes for every file on the tape
4. Read the seal marker (the last file) and check that its
   `front_index_sha256` matches the sha256 of File 3's bytes (trailing
   zero padding stripped). If the seal marker is missing, or the two
   hashes disagree, treat the tape as unsealed/damaged: trailing data
   slices may be incomplete, but everything the front index describes
   is still readable and independently checkable
5. Read and trial-decrypt tenant envelopes (File 3 lists their
   positions as type `tenant_envelope`) with your key
6. Parse the MANIFEST.toml in your envelope for slice positions
7. For each slice: read from tape, trim to the exact size given in
   File 3 (block padding breaks age decryption), verify its sha256
   against File 3's `sha256_encrypted`, decrypt with age. Work on a
   disk with room for all of the unit's decrypted slices at once
   (about the unit's size), not in /tmp, which is RAM on many systems
8. Reassemble dar slices: `dar -x restore -R /destination -O -Q`

## Important: Block Padding

This tape uses 512KB (524288 byte) fixed block mode. Every file is
padded with zeros to the next block boundary. Encrypted files (data
slices, envelopes) MUST be trimmed to their exact byte size before
decryption — the padding zeros will cause age to reject the ciphertext.
Exact sizes are in File 3, the front index (`size_bytes` field).

## What the unencrypted parts of this tape reveal

Files 0-3 and the seal marker (the last file on the tape) are
plaintext; so is the tape's block structure. Anyone holding the
physical tape can therefore learn a limited set of *structural* facts
— but no content:

- the volume label, creation date, and tapectl version;
- how many tenants share this tape (the number of tenant envelopes);
- how many data slices there are, and the exact on-tape byte size and
  ciphertext hash of every file (needed so an heir can navigate and
  trim block padding without a key).

They CANNOT learn filenames, unit or tenant names, plaintext-content
checksums, ownership, or any file content — all of that lives only
inside the age-encrypted envelopes and slices.

**Accepted size disclosure.** Encryption overhead is deterministic and
this tape uses no compression, so an on-tape size approximates the
plaintext content size it encloses. If your units are archived one
folder per slice — the common case — the size of each data slice
effectively discloses that unit's approximate content size and
reveals unit boundaries: someone holding this tape could in principle
correlate slice sizes against publicly known media sizes. This is a
known, accepted trade-off: sizes and ciphertext hashes are structural
facts needed for keyless navigation and integrity checking, and
neither is tied to any tenant or unit name in plaintext.

## If All Else Fails

Normal recovery reads the front index (File 3) to know exactly where
everything is and how big it is. There are two more tries before
giving up — a three-rung degradation ladder:

1. **Front index (File 3)** — the normal path: read File 3 directly
   (`mt fsf 3`); it lists every file's position, type, on-tape size,
   and ciphertext hash.
2. **Seal-marker embedded copy** — if File 3 itself is damaged or will
   not parse, the LAST file on the tape (the seal marker) carries a
   full embedded copy of the same index, in the same format. Read the
   last file and parse its `[[files]]` entries exactly as you would
   File 3's.
3. **Filemark walk + zero-strip** — if BOTH the front index and the
   seal marker are lost or unreadable, recovery is still possible; see
   below.

### Zero-strip procedure (rung 3)

Envelopes are among the first files after the front index. Block
padding can be defeated without knowing the exact size:

1. Space forward file by file from the start of the tape
   (`mt -f /dev/nst0 fsf 1`, repeated) and read each candidate file
   with `dd`.
2. Strip ALL trailing zero bytes from the file you read (true
   ciphertext essentially never ends in a run of zero bytes; block
   padding always does).
3. Try `age -d -i YOUR_KEY.age.key` on the stripped file. If it fails,
   re-append a single zero byte and retry. A handful of retries
   suffices — the padding tail is at most one 512KB block.
4. Once your envelope decrypts, its MANIFEST.toml gives the exact tape
   position and byte size for every slice belonging to your unit(s).
   Apply the same read/trim/decrypt procedure to each slice using
   those exact sizes, then reassemble with dar as above.

## Total files on this tape: {total_files}
"#
    )
}

/// Generate RESTORE.sh v2 (File 2) — self-contained emergency restore +
/// verify script for layout v2. Four modes per sheet §10:
/// - `--info`: read File 0 + File 3 (front index) + the last file (seal
///   marker); print the layout table and the exact §2.5 verdict (SEALED /
///   UNSEALED / DAMAGED (ends disagree)) — never trusts the seal marker's
///   mere presence (§2.6): the verdict always re-derives from the hash chain.
/// - `--verify`: keyless integrity walk — every file's on-tape bytes (after
///   trimming to File 3's `size_bytes`) hashed and compared to File 3's
///   `sha256_encrypted`; File 3 itself checked against the seal binding.
///   Per-file PASS/FAIL lines, nonzero exit on any FAIL.
/// - `--find-envelope --key K [--key K2 ...]`: trial-decrypt envelope
///   positions (found by type in the file map), as v1.
/// - `--restore --key K [--key K2 ...] --to DIR [--unit U] [--version N]`: slice positions/sizes come from
///   the decrypted MANIFEST, cross-checked against the file map's
///   `size_bytes`/`sha256_encrypted` before each slice is trusted/decrypted.
///
/// Every mode that needs a file map applies the degradation ladder (sheet
/// §3.4): rung 1 is File 3 itself; rung 2 (if File 3 fails to parse or fails
/// its self-consistency check, §2.5) falls back to the seal marker's
/// embedded `[[files]]` copy, loudly warned; rung 3 (both gone) prints the
/// manual zero-strip procedure (§3.3) and exits — there is nothing left to
/// automate. Parsing stays line-oriented (`grep`/`awk`/`sed` over
/// `key = value` lines, the §3.1 grammar contract); every value read from a
/// plaintext tape metadata zone goes through `require_uint` before it
/// reaches arithmetic, `fsf`, or `seq` (S2 hardening, carried over from v1).
/// Tools used: mt, dd, age, dar, sha256sum, head, truncate, tar, plus standard
/// coreutils (awk/sed/grep/tr) — no TOML collection, per the grammar contract.
/// **`--key` is repeatable (issue #288, CTO ruling 2026-09-22).** A volume's
/// tenant envelope is sealed at WRITE time to the tenant's then-current public
/// keys (`build.rs`), while its data slices were sealed at STAGE time
/// (`staging/mod.rs`). A `key rotate` landing between the two puts envelope
/// and slices on different key generations, and then NO single key opens both
/// — measured on a real tape. tapectl itself is unaffected because it
/// trial-decrypts with every tenant and operator key, independently for the
/// envelope and for the slices; RESTORE.sh took one `--key`, so the heir path
/// was the only one that failed. It now mirrors tapectl: repeated `--key`,
/// each tried on its own for the envelope and for each slice (remembering the
/// key that opened the previous slice, since a wrong key costs a re-read of
/// tape-sized ciphertext). Every key path is checked for existence BEFORE the
/// first tape read — `age -d -i good -i missing` fails outright rather than
/// skipping the bad one.
///
/// **CARRIED FIX — issue #218, CTO ruling 2026-09-17: done, 2026-09-22.** The
/// decrypted-slice progress line read `MB` for a 1048576-divided figure, which
/// ADR-0012 forbids (capacities decimal, data sizes binary, the two named
/// apart). #204 fixed the class across ~40 CLI sites and deliberately left
/// this one, because RESTORE.sh is on-tape content pinned by
/// `RESTORE_SH_SHA256` in `tests/on_tape_golden.rs` and the ruling was to
/// batch it onto the next substantive change rather than spend a re-pin on a
/// label. #288 is that change, so the line now reads `MiB`.
///
/// **A quoting defect went with it (issue #291).** Two of the three
/// "no envelope opened" exits quoted the tape label as `'\''VOL-A'\''` — the
/// idiom for embedding a quote inside a SINGLE-quoted string, in a string
/// that is double-quoted, where a bare `'` is already literal. Bash printed
/// the backslashes verbatim, so the one line telling a reader in a disaster
/// whether they hold the wrong cartridge rendered as garbage. The third exit
/// (the `--unit` one, which is the ordinary way to reach it) carried no
/// context at all. All three now route through one `die_no_envelope` helper
/// so they cannot drift apart again.
///
/// This note lives on the FUNCTION, not inside the template: a comment added
/// within the string becomes shell comment lines in the generated script and
/// changes its hash. I did exactly that first, and the golden test caught it —
/// which is the test working as designed.
pub fn generate_restore_script_v2(label: &str, total_files: i32) -> String {
    use crate::volume::restore_script::{
        AWK_CHECK_FILE_LIST, AWK_FIND_ENVELOPE, AWK_MANIFEST_HAS_UNIT, AWK_PARSE_FILE_LIST,
        AWK_SELECT_VERSION, AWK_UNIT_LIST,
    };

    let script = r#"#!/usr/bin/env bash
# RESTORE.sh — Emergency restore script for tapectl volume __LABEL__ (layout v2)
# This script restores data from this tape WITHOUT tapectl installed.
# It reads the tape's front index (File 3), the seal marker (last file),
# finds your encrypted envelope, decrypts each data slice, and extracts
# the dar archive to a directory.
#
# Usage:
#   ./RESTORE.sh --info                                       Show tape layout + seal verdict
#   ./RESTORE.sh --verify                                     Keyless integrity check (no key needed)
#   ./RESTORE.sh --find-envelope --key KEYFILE [--key K2 ...] Decrypt your envelope
#   ./RESTORE.sh --restore --key KEYFILE [--key K2 ...] --to DIR [--unit U] [--version N]
#                [--scratch DIR] [--no-space-check]
#
# --key may be repeated. An envelope and the slices it describes can need
# different keys after a key rotation, so every key is tried independently.
#
# Disk space: --restore decrypts a unit's slices to disk before dar extracts
# them, into a scratch directory inside --to unless --scratch names another.
# With both on one disk a unit needs about twice its size free there, plus one
# slice. The script measures this before it reads any slice.
#
# Requirements: mt, dd, age, dar, sha256sum, head, truncate, tar
# Total files on tape: __TOTAL_FILES__
#
# Degradation ladder (see the system guide, File 1, "If All Else Fails"):
#   rung 1 — File 3, the front index (normal path)
#   rung 2 — the seal marker's embedded front-index copy (if File 3 fails)
#   rung 3 — manual zero-strip recovery (if both fail; this script cannot
#            automate it, but prints the procedure)

set -euo pipefail

DEVICE="${TAPE_DEVICE:-/dev/nst0}"
LABEL="__LABEL__"
BLOCK=524288 # 512 KB — tapectl fixed block size

# Every --key given on the command line, in order. A volume's envelope and the
# data slices it describes can be sealed to DIFFERENT key generations: the
# envelope is built when the volume is written, each slice when it was staged.
# A key rotation between the two leaves no single key that opens both, so each
# key here is tried independently for the envelope and for every slice.
KEYS=()

# WORK holds only small files: the tape's text zones and one envelope. A unit's
# slices never go here — a data slice is up to 10 GiB and /tmp is RAM on many
# systems. --restore decrypts them into SCRATCH, a directory it creates inside
# --to (or --scratch) once it knows how much room they need.
SCRATCH=""
SKIP_SPACE_CHECK=0

umask 077 # decrypted plaintext and temp files must not be world-readable
WORK="$(mktemp -d "${TMPDIR:-/tmp}/tapectl-restore.XXXXXX")" ||
  {
    echo "FATAL: cannot create temporary directory" >&2
    exit 1
  }
trap 'rm -rf "$WORK"; [ -z "$SCRATCH" ] || rm -rf "$SCRATCH"' EXIT

die() {
  echo "FATAL: $*" >&2
  exit 1
}
info() { echo ">>> $*"; }

# The provided keys as one line, so a failure names every key that was tried.
keys_list() {
  local k out=""
  for k in ${KEYS[@]+"${KEYS[@]}"}; do
    out="$out $k"
  done
  echo "${out# }"
}

# "the key provided" / "any of the 3 keys provided" — the count is what tells a
# reader whether they forgot to pass one.
keys_phrase() {
  if [ "${#KEYS[@]}" -eq 1 ]; then
    echo "the key provided"
  else
    echo "any of the ${#KEYS[@]} keys provided"
  fi
}

# The keys back as command-line arguments, for the hints this script prints.
keys_args() {
  local k out=""
  for k in ${KEYS[@]+"${KEYS[@]}"}; do
    out="$out --key $k"
  done
  echo "${out# }"
}

# Check every key path BEFORE the first tape read. `age -d -i good -i missing`
# fails outright rather than skipping the bad one, so a mistyped path must be
# caught here and not discovered minutes into a restore.
require_key_files() {
  local k
  for k in ${KEYS[@]+"${KEYS[@]}"}; do
    [ -n "$k" ] || die "--key needs a value"
    [ -f "$k" ] || die "key file not found: $k"
  done
}

# Reject a value read from a plaintext tape metadata zone (ID thunk, front
# index, seal marker) that is not a plain non-negative integer, BEFORE it is
# used in `$(( ))` arithmetic, `fsf`, or `seq`. Those zones are unauthenticated
# — anyone holding the tape can rewrite them without any key — so a crafted
# value such as 'a[$(cmd)]' would otherwise execute inside arithmetic
# expansion. Called in the current shell (not a subshell) so `die` halts the
# whole script. (MANIFEST.toml values are NOT re-checked here: they come from
# an age-authenticated envelope, a different trust tier — tampering there
# breaks decryption itself.)
require_uint() {
  local name=$1 val=$2
  case "$val" in
  "" | *[!0-9]*)
    die "tape layout value '$name' is not a non-negative integer: '$val' — the tape may be damaged or tampered"
    ;;
  esac
}

# Make a string read from an unauthenticated plaintext tape zone safe to ECHO.
# Same trust tier as require_uint's callers: anyone holding the tape can write
# these bytes. This one is never used in arithmetic — the danger is terminal
# control sequences (a crafted label could hide or forge output). Keep only
# printable ASCII, cap the length, and mark it if anything was dropped.
safe_str() {
  local raw=$1 clean
  clean=$(printf '%s' "$raw" | LC_ALL=C tr -cd '[:alnum:] ._:@/+-' | cut -c1-64)
  [ "$clean" = "$raw" ] || clean="$clean (sanitized)"
  printf '%s' "$clean"
}

# Non-fatal cousin of require_uint: true/false, never exits. Used in soft
# parse/consistency checks where a bad value should degrade the reader to the
# next rung of the ladder (or to an UNSEALED/DAMAGED verdict) instead of
# crashing the whole script.
is_uint() {
  case "$1" in
  "" | *[!0-9]*) return 1 ;;
  *) return 0 ;;
  esac
}

# A byte count for messages: GiB, MiB or KiB with one decimal, so a small unit
# does not read as "needs 0.0 GiB, 0.0 GiB free".
size_str() {
  awk -v b="$1" 'BEGIN {
    if (b >= 1073741824) printf "%.1f GiB", b / 1073741824
    else if (b >= 1048576) printf "%.1f MiB", b / 1048576
    else printf "%.1f KiB", b / 1024
  }'
}

# A failed write must say why. Out of disk space is the likely cause on a
# restore, and it must never read as a tape fault or a wrong key: the loop that
# tries each key used to report a full disk as "key did not decrypt".
die_io() { # <what failed> <file holding the tool's stderr> <directory written to>
  local what=$1 errf=$2 where=$3 detail
  detail=$(grep -v -e 'records in$' -e 'records out$' -e 'bytes.*copied' "$errf" 2>/dev/null | tail -n 3 || true)
  if grep -qi 'no space left' "$errf" 2>/dev/null; then
    die "$what: OUT OF DISK SPACE in $where
       ${detail}
       This is not a tape or key problem. Free space there and run the same
       command again. --restore keeps a unit's decrypted slices in its scratch
       directory (inside --to unless --scratch names another disk); everything
       else this script reads is small and goes to ${TMPDIR:-/tmp}."
  fi
  die "$what
       ${detail:-(no error text)}"
}

# Free space in bytes on the filesystem holding directory $1; empty if df
# cannot say.
free_bytes() {
  df -Pk -- "$1" 2>/dev/null | awk 'NR == 2 && $4 ~ /^[0-9]+$/ { printf "%.0f\n", $4 * 1024 }'
}

die_space() { # <directory> <bytes needed> <bytes free>
  die "not enough disk space in $1 to restore this unit:
       it needs about $(size_str "$2"), and $(size_str "$3") is free.
       No slice has been read yet. A restore decrypts every slice of the unit to
       disk before dar extracts them, so with the scratch space and --to on one
       disk it needs the unit's size about twice over, plus one slice.
       Free space there, choose a larger disk with --to, or put the decrypted
       slices on another disk with --scratch DIR.
       (--no-space-check skips this check, for a filesystem that holds more than
       df reports, such as a compressed or thin-provisioned one.)"
}

# Refuse BEFORE the first slice is read when the unit cannot fit. Running out of
# space mid-restore used to kill the script hours in, silently or as a key error.
# Scratch holds every decrypted slice (each no larger than its ciphertext) plus
# the one being read; --to receives the extracted files, about the archive's
# size again.
check_space() { # <destination> <scratch parent> <archive bytes> <largest slice bytes>
  local dest=$1 scr=$2 total=$3 largest=$4
  if [ "$SKIP_SPACE_CHECK" = 1 ]; then
    info "Disk space not checked (--no-space-check)"
    return 0
  fi
  local need_scr=$((total + largest + BLOCK)) need_dest=$total
  local scr_free dest_free
  scr_free=$(free_bytes "$scr")
  dest_free=$(free_bytes "$dest")
  if ! is_uint "$scr_free" || ! is_uint "$dest_free"; then
    echo "WARNING: cannot measure free disk space (df failed); continuing without the check." >&2
    return 0
  fi
  # One filesystem unless stat proves otherwise, so a failed stat over-asks
  # rather than under-asks.
  local scr_dev dest_dev
  scr_dev=$(stat -c %d -- "$scr" 2>/dev/null || true)
  dest_dev=$(stat -c %d -- "$dest" 2>/dev/null || true)
  if [ -z "$scr_dev" ] || [ "$scr_dev" = "$dest_dev" ]; then
    local need=$((need_scr + need_dest))
    info "Disk space: needs about $(size_str "$need") in $dest (decrypted slices, then the files); $(size_str "$dest_free") free"
    [ "$dest_free" -ge "$need" ] || die_space "$dest" "$need" "$dest_free"
  else
    info "Disk space: needs about $(size_str "$need_scr") in $scr (decrypted slices); $(size_str "$scr_free") free"
    info "            and about $(size_str "$need_dest") in $dest (the files); $(size_str "$dest_free") free"
    [ "$scr_free" -ge "$need_scr" ] || die_space "$scr" "$need_scr" "$scr_free"
    [ "$dest_free" -ge "$need_dest" ] || die_space "$dest" "$need_dest" "$dest_free"
  fi
}

# ---- prerequisite check ----

for tool in mt dd age sha256sum dar head truncate tar; do
  command -v "$tool" >/dev/null 2>&1 || die "missing required tool: $tool"
done

# ---- tape helpers ----

tape_init() {
  mt -f "$DEVICE" setblk "$BLOCK" 2>/dev/null ||
    die "cannot set block size — is $DEVICE a tape device?"
}

# Read tape file at position $1 into file $2 (raw bytes, block-padded). Dies
# on failure, naming the cause — used once a source is already trusted
# (post-ladder). dd's stderr used to go to /dev/null, so a full disk here ended
# the script with no message at all.
read_tape_raw() {
  local pos=$1 out=$2
  mt -f "$DEVICE" rewind
  [ "$pos" -gt 0 ] && mt -f "$DEVICE" fsf "$pos"
  dd if="$DEVICE" of="$out" bs="$BLOCK" 2>"$WORK/dd.err" ||
    die_io "reading tape file $pos failed" "$WORK/dd.err" "$(dirname "$out")"
}

# Same, but never exits: returns 1 on any failure (missing position, I/O
# error, or an empty read). Used wherever "absent" must be distinguished from
# "present but wrong" — the ladder's rung selection and --verify's per-file
# walk both rely on this instead of letting `set -e` kill the script.
try_read_tape_raw() {
  local pos=$1 out=$2
  mt -f "$DEVICE" rewind 2>/dev/null || return 1
  if [ "$pos" -gt 0 ]; then
    mt -f "$DEVICE" fsf "$pos" 2>/dev/null || return 1
  fi
  dd if="$DEVICE" of="$out" bs="$BLOCK" 2>/dev/null
  [ -s "$out" ] || return 1
}

# sha256 of tape file $1, first $2 bytes (all of it when $2 is empty), hashed
# as it streams off the tape: --verify used to copy each file into WORK first,
# twice, and a 10 GiB slice in a RAM-backed /tmp made a good tape FAIL as
# "unreadable". Returns 1 when the file cannot be reached or read, or yields no
# bytes at all.
EMPTY_SHA256=e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
hash_tape_file() {
  local pos=$1 size=$2 sum
  mt -f "$DEVICE" rewind 2>/dev/null || return 1
  if [ "$pos" -gt 0 ]; then
    mt -f "$DEVICE" fsf "$pos" 2>/dev/null || return 1
  fi
  if [ -n "$size" ]; then
    # head stops at the exact size; cat drains the block padding after it,
    # so dd never takes a SIGPIPE that pipefail would read as a tape fault.
    sum=$(dd if="$DEVICE" bs="$BLOCK" 2>/dev/null | {
      head -c "$size" | sha256sum
      cat >/dev/null
    }) || return 1
  else
    sum=$(dd if="$DEVICE" bs="$BLOCK" 2>/dev/null | sha256sum) || return 1
  fi
  sum=${sum%% *}
  [ -n "$sum" ] && [ "$sum" != "$EMPTY_SHA256" ] || return 1
  echo "$sum"
}

# Read tape file at position $1 into file $2, stripping null padding. Use for
# plaintext text files (ID thunk, front index, seal marker) where padding
# zeros are harmless to strip but would confuse text-processing tools — the
# content is TOML text and should never legitimately contain embedded NULs, so
# stripping every NUL is equivalent to stripping only the trailing padding.
read_tape_text() {
  local pos=$1 out=$2
  mt -f "$DEVICE" rewind
  [ "$pos" -gt 0 ] && mt -f "$DEVICE" fsf "$pos"
  dd if="$DEVICE" bs="$BLOCK" 2>/dev/null | tr -d '\0' >"$out"
}

# ---- TOML helpers (flat key = value parsing; sec 3.1 grammar contract) ----

# Print the value for a TOML key on a "key = value" line. Strips quotes.
toml_val() {
  local file=$1 key=$2
  awk -v k="$key" '
    $1 == k && $2 == "=" {
      v = $3
      for (i = 4; i <= NF; i++) v = v " " $i
      gsub(/^"/, "", v); gsub(/"$/, "", v)
      print v; exit
    }
  ' "$file"
}

# Parse [[files]] blocks (front index or the seal marker's embedded copy)
# into lines: position|type|size_bytes|sha256_encrypted. Either field may be
# empty (front index's own entry has neither; the seal marker's own entry
# has neither; everything else has both).
parse_front_index_entries() {
  local file=$1
  awk '__AWK_PARSE_FILE_LIST__' "$file"
}

# Front-index self-consistency check (sec 2.5): positions strictly
# increasing from 0 (a total index has no gaps), exactly one front_index
# entry and it is at position 3, exactly one seal_marker entry and it is
# last. Optional $2 = the expected total file count; when given, the entry
# count must match it too. A subtly wrong map fails loudly (nonzero exit)
# rather than silently misleading recovery. No `exit` appears in the main
# per-record block (awk runs END even after a mid-block `exit`) — flags are
# accumulated and only checked in END, where `exit` really does terminate.
check_file_list_consistency() {
  local list=$1 expected_total=${2:-}
  awk -F'|' -v expected="$expected_total" '__AWK_CHECK_FILE_LIST__' "$list"
}

# Look up size_bytes / sha256_encrypted for a given tape file position.
file_size_at() { awk -F'|' -v p="$1" '$1 == p { print $3; exit }' "$2"; }
file_hash_at() { awk -F'|' -v p="$1" '$1 == p { print $4; exit }' "$2"; }

# List tenant/operator envelope positions in on-tape order (the file list is
# already position-sorted, and the format's fixed zone order already puts
# tenant envelope(s) before the operator envelope before its backup).
envelope_positions() {
  awk -F'|' '__AWK_FIND_ENVELOPE__' "$1"
}

# ---- bootstrap (File 0 — identity + pointers only in layout v2) ----

bootstrap_thunk() {
  tape_init
  info "Reading ID thunk (file 0)..."
  read_tape_text 0 "$WORK/id_thunk.txt"
  sed -n '/^\[volume\]/,$p' "$WORK/id_thunk.txt" >"$WORK/thunk.toml"

  check_layout_version "$(toml_val "$WORK/thunk.toml" layout_version)"

  FRONT_INDEX=$(toml_val "$WORK/thunk.toml" front_index)
  SEAL_MARKER=$(toml_val "$WORK/thunk.toml" seal_marker)
  TOTAL_FILES=$(toml_val "$WORK/thunk.toml" total_files)
  CREATED=$(toml_val "$WORK/thunk.toml" created_at)

  require_uint front_index "$FRONT_INDEX"
  require_uint seal_marker "$SEAL_MARKER"
  require_uint total_files "$TOTAL_FILES"

  # Which volume is ACTUALLY in the drive. $LABEL is baked into this script at
  # write time; it says which tape the script was made for, and is printed even
  # when a different tape is loaded. Only the thunk says what is really there.
  TAPE_LABEL=$(safe_str "$(toml_val "$WORK/thunk.toml" label)")
  check_tape_identity
}

# This script reads layout v2 only. Every tape carries the script that reads
# it at file 2, so a tape of another layout is refused with the way to that
# script, before anything of it is interpreted. A missing value (a damaged
# thunk) warns and reads on as v2: a damaged thunk, the case where an heir
# needs this script most, must never become a hard stop.
check_layout_version() { # <layout_version as File 0 states it>
  local v=$1
  if [ -z "$v" ]; then
    echo "WARNING: File 0 states no layout_version (damaged?); reading this tape as layout v2." >&2
    return 0
  fi
  [ "$v" != 2 ] || return 0
  die "this tape is layout_version $(safe_str "$v"); this script reads layout v2 only.
       Use the RESTORE.sh stored on THIS tape — every tape carries the script
       that reads it, at file 2:
         mt -f $DEVICE rewind && mt -f $DEVICE fsf 2
         dd if=$DEVICE bs=512k | tr -d '\\0' > RESTORE.sh"
}

# Always say which device was read and what was found there. A silent default
# device is how a wrong-tape read disguises itself as a key or data problem:
# with the wrong cartridge loaded, --info happily describes it while
# --find-envelope reports that no envelope matched any key given, which reads
# as "your key is wrong" or "your archive is gone" when it means neither.
#
# This WARNS and continues rather than exiting. Everything here except $LABEL is
# generic, so running this script against a sibling tape is legitimate recovery
# — and a damaged thunk (the case where an heir needs this script most) must
# never be turned into a hard stop.
check_tape_identity() {
  info "Tape device:      $DEVICE${TAPE_DEVICE:+ (from TAPE_DEVICE)}"
  info "Tape identifies as: ${TAPE_LABEL:-<unreadable>}"
  if [ -n "$TAPE_LABEL" ] && [ "$TAPE_LABEL" != "$LABEL" ]; then
    echo "" >&2
    echo "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!" >&2
    echo "  WRONG TAPE? This script was written for volume '$LABEL'," >&2
    echo "  but the tape in $DEVICE identifies itself as '$TAPE_LABEL'." >&2
    echo "" >&2
    echo "  Reading continues, but keys and units from '$LABEL' will NOT" >&2
    echo "  be found on this tape. If that is not what you intended:" >&2
    echo "    - load the '$LABEL' cartridge, or" >&2
    echo "    - point this script at the right drive:" >&2
    echo "        TAPE_DEVICE=/dev/nstN $0 ..." >&2
    echo "  Drives on this machine: ls -l /dev/tape/by-id/" >&2
    echo "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!" >&2
    echo "" >&2
  fi
}

# ---- rung 1: the front index (File 3) ----

# On success sets FRONT_INDEX_HASH and leaves the parsed, validated entry
# list at $WORK/files_from_index.txt. Never exits — callers test it with `if`
# or `&&`.
try_front_index() {
  try_read_tape_raw "$FRONT_INDEX" "$WORK/front_index.raw" || return 1
  tr -d '\0' <"$WORK/front_index.raw" >"$WORK/front_index.stripped"
  grep -q '^\[index\]' "$WORK/front_index.stripped" || return 1
  sed -n '/^\[index\]/,$p' "$WORK/front_index.stripped" >"$WORK/index.toml"
  parse_front_index_entries "$WORK/index.toml" >"$WORK/files_from_index.txt"
  [ -s "$WORK/files_from_index.txt" ] || return 1
  check_file_list_consistency "$WORK/files_from_index.txt" "$TOTAL_FILES" || return 1
  FRONT_INDEX_HASH=$(sha256sum "$WORK/front_index.stripped" | awk '{print $1}')
  return 0
}

# ---- rung 2: the seal marker's embedded front-index copy ----

# On success sets SEAL_FRONT_INDEX_SHA256, SEAL_FILE_COUNT, SEAL_SEALED_AT and
# leaves the parsed, validated embedded copy at $WORK/files_from_seal.txt.
# Never exits — callers test it with `if` or `&&`.
try_seal_copy() {
  try_read_tape_raw "$SEAL_MARKER" "$WORK/seal.raw" || return 1
  tr -d '\0' <"$WORK/seal.raw" >"$WORK/seal.stripped"
  grep -q '^\[seal\]' "$WORK/seal.stripped" || return 1
  sed -n '/^\[seal\]/,$p' "$WORK/seal.stripped" >"$WORK/seal.toml"
  SEAL_FRONT_INDEX_SHA256=$(toml_val "$WORK/seal.toml" front_index_sha256)
  SEAL_FILE_COUNT=$(toml_val "$WORK/seal.toml" file_count)
  SEAL_SEALED_AT=$(toml_val "$WORK/seal.toml" sealed_at)
  [ -n "$SEAL_FRONT_INDEX_SHA256" ] || return 1
  is_uint "$SEAL_FILE_COUNT" || return 1
  parse_front_index_entries "$WORK/seal.toml" >"$WORK/files_from_seal.txt"
  [ -s "$WORK/files_from_seal.txt" ] || return 1
  check_file_list_consistency "$WORK/files_from_seal.txt" "$SEAL_FILE_COUNT" || return 1
  return 0
}

# ---- rung 3: nothing left to parse ----

zero_strip_instructions() {
  cat <<'ZSEOF'
FATAL: both the front index (File 3) and the seal marker's embedded copy
are unreadable or fail their self-consistency check. The navigation map is
lost at both ends of the tape (degradation ladder rung 3).

Manual "zero-strip" recovery is still possible: envelopes are among the
first files after the front index, and block padding can be defeated
without knowing exact sizes:

  1. mt -f "$DEVICE" setblk 524288
  2. Starting a few files past the front index, for each candidate position N:
       mt -f "$DEVICE" rewind && mt -f "$DEVICE" fsf N
       dd if="$DEVICE" bs=524288 of=candidate.raw
  3. Strip ALL trailing zero bytes (true ciphertext essentially never ends
     in a run of zero bytes; block padding always does):
       tr -d '\0' < candidate.raw > candidate.stripped
  4. Try: age -d -i YOUR_KEY.age.key < candidate.stripped > candidate.dec
     If it fails, re-append one zero byte to candidate.stripped and retry.
     A handful of retries suffices (padding is at most one 524288-byte block).
  5. Once a candidate decrypts, it is your tenant (or operator) envelope:
     untar it and read MANIFEST.toml for the exact tape position and byte
     size of every slice belonging to your unit(s); repeat steps 2-4
     (trimming to that exact size instead of zero-stripping) for each slice,
     then `dar -x restore -R /destination -O -Q`.

See the system guide (File 1), "If All Else Fails", for the full narrative.
ZSEOF
}

# ---- establish the trusted file map (rung 1 -> rung 2 -> rung 3) ----

# Sets FILES_TXT and FILES_SOURCE on success; on total failure prints the
# zero-strip procedure and exits (used by modes that cannot proceed at all
# without a map: --find-envelope, --restore; --info and --verify apply the
# same ladder but handle a total failure themselves so they can still report
# a verdict).
establish_files() {
  bootstrap_thunk

  if try_front_index; then
    FILES_TXT="$WORK/files_from_index.txt"
    FILES_SOURCE="front_index"
    return 0
  fi

  echo "WARNING: front index (file $FRONT_INDEX) is unreadable or failed its self-consistency check — falling back to the seal marker's embedded copy (degradation ladder RUNG-2, volume-format-v2.md sec 4)." >&2

  if try_seal_copy; then
    FILES_TXT="$WORK/files_from_seal.txt"
    FILES_SOURCE="seal_embedded_copy(RUNG-2)"
    echo "WARNING: using the seal marker's embedded copy as the file map — the front of the tape may be damaged; treat this recovery as degraded." >&2
    return 0
  fi

  zero_strip_instructions >&2
  exit 1
}

# ---- --info ----

do_info() {
  bootstrap_thunk

  local fi_ok=0 seal_ok=0
  try_front_index && fi_ok=1
  try_seal_copy && seal_ok=1

  local verdict
  if [ "$seal_ok" -ne 1 ]; then
    verdict="UNSEALED"
  elif [ "$fi_ok" -ne 1 ] || [ "$FRONT_INDEX_HASH" != "$SEAL_FRONT_INDEX_SHA256" ]; then
    verdict="DAMAGED (ends disagree)"
  else
    verdict="SEALED"
  fi

  local table_file="" table_source=""
  if [ "$fi_ok" -eq 1 ]; then
    table_file="$WORK/files_from_index.txt"
    table_source="front index (file $FRONT_INDEX)"
  elif [ "$seal_ok" -eq 1 ]; then
    table_file="$WORK/files_from_seal.txt"
    table_source="seal marker's embedded copy (RUNG-2 — front index unreadable)"
  fi

  echo ""
  # The tape's own label, not this script's — the map below describes what is
  # actually loaded. Identical in the normal case; honest in the wrong-tape one.
  echo "=== tapectl volume: ${TAPE_LABEL:-$LABEL} ==="
  echo ""
  echo "Verdict: $verdict"
  case "$verdict" in
  SEALED)
    echo "  Front index (file $FRONT_INDEX) hash matches the seal marker's binding."
    ;;
  UNSEALED)
    echo "  No valid seal marker was found — this tape was never sealed, or the"
    echo "  write was interrupted or aborted. Trailing data slices may be missing."
    ;;
  "DAMAGED (ends disagree)")
    echo "  The seal marker is present but the front index does not match it (or"
    echo "  is unreadable). Never trust the seal marker's presence alone — see"
    echo "  the system guide."
    ;;
  esac
  echo ""

  if [ -n "$table_file" ]; then
    echo "File map (source: $table_source):"
    while IFS='|' read -r pos type size hash; do
      printf "  %3d  %-24s  %8s bytes  %s\n" "$pos" "$type" "${size:--}" "${hash:+${hash:0:16}...}"
    done <"$table_file"
  else
    echo "No usable file map: both the front index and the seal marker's"
    echo "embedded copy are unreadable."
    echo ""
    zero_strip_instructions
  fi

  echo ""
  echo "Sealed at: ${SEAL_SEALED_AT:-unknown (tape not sealed)}"
  echo ""
  echo "To decrypt your envelope:"
  echo "  $0 --find-envelope --key YOUR_KEY.age.key"
  echo "To check tape integrity without any key:"
  echo "  $0 --verify"
}

# ---- --verify ----

do_verify() {
  bootstrap_thunk

  local fi_ok=0 seal_ok=0
  try_front_index && fi_ok=1
  try_seal_copy && seal_ok=1

  local files_file=""
  if [ "$fi_ok" -eq 1 ]; then
    files_file="$WORK/files_from_index.txt"
  elif [ "$seal_ok" -eq 1 ]; then
    echo "WARNING: front index unreadable — verifying against the seal marker's embedded copy (degradation ladder RUNG-2)." >&2
    files_file="$WORK/files_from_seal.txt"
  else
    zero_strip_instructions >&2
    exit 1
  fi

  echo "=== keyless integrity walk: $LABEL ==="
  echo ""

  local overall_ok=1

  # File 3 vs the seal binding: the one entry the generic per-file loop below
  # cannot check when sourced from the front index itself (its own entry
  # carries no hash there — self-reference).
  if [ "$seal_ok" -eq 1 ] && [ "$fi_ok" -eq 1 ]; then
    if [ "$FRONT_INDEX_HASH" = "$SEAL_FRONT_INDEX_SHA256" ]; then
      printf "PASS  file %3d  %-24s  (matches seal binding)\n" "$FRONT_INDEX" "front_index"
    else
      printf "FAIL  file %3d  %-24s  (seal binding mismatch)\n" "$FRONT_INDEX" "front_index"
      overall_ok=0
    fi
  elif [ "$seal_ok" -eq 1 ]; then
    printf "FAIL  file %3d  %-24s  (unreadable/unparseable)\n" "$FRONT_INDEX" "front_index"
    overall_ok=0
  else
    echo "WARNING: no valid seal marker — this tape is UNSEALED; completeness cannot be confirmed. Only the files below were checked." >&2
  fi

  while IFS='|' read -r pos type size hash; do
    # Entries with no hash are self-referential (front_index's own entry in
    # the front-index-sourced list, and the seal marker's own entry always)
    # — nothing on the tape hashes them, so there is nothing to compare.
    [ -z "$hash" ] && continue

    require_uint "position(@$type)" "$pos"
    [ -z "$size" ] || require_uint "size_bytes(@$pos)" "$size"

    local actual
    if ! actual=$(hash_tape_file "$pos" "$size"); then
      printf "FAIL  file %3d  %-24s  (unreadable)\n" "$pos" "$type"
      overall_ok=0
      continue
    fi

    if [ "$actual" = "$hash" ]; then
      printf "PASS  file %3d  %-24s\n" "$pos" "$type"
    else
      printf "FAIL  file %3d  %-24s  (hash mismatch)\n" "$pos" "$type"
      overall_ok=0
    fi
  done <"$files_file"

  echo ""
  if [ "$overall_ok" -eq 1 ]; then
    echo "VERIFY: PASS — every file matches the front index."
  else
    echo "VERIFY: FAIL — one or more files do not match. See FAIL lines above." >&2
    exit 1
  fi
}

# ---- --find-envelope ----

# The three "no envelope opened" exits share one shape (issues #288, #291):
# which keys were tried, what tape is actually in the drive, and the rotation
# fact that makes a correct-looking key fail on a correct tape. They live here,
# in one function, because the --unit exit — the one an heir is most likely to
# hit — had already drifted into a bare one-liner with none of it.
die_no_envelope() { # [unit]
  local headline="no envelope matched $(keys_phrase)"
  if [ -n "${1:-}" ]; then
    headline="no envelope for unit '$1' matched $(keys_phrase)"
  fi
  die "$headline
       Tape in $DEVICE identifies as '${TAPE_LABEL:-<unreadable>}'; this script is for '$LABEL'.
       If those differ, you have the wrong cartridge or the wrong drive
       (set TAPE_DEVICE=/dev/nstN) — not necessarily the wrong key.
       If they match, try your OTHER keys: an envelope is sealed with the key
       that was active when THIS tape was written, so a key issued after it
       will not open it. --find-envelope reports which keys do.
       Keys tried: $(keys_list)"
}

do_find_envelope() {
  establish_files

  local found=0 pos keyfile
  while IFS= read -r pos; do
    require_uint envelope_position "$pos"
    info "Trying envelope at file $pos..."
    read_tape_raw "$pos" "$WORK/envelope.enc"

    local esize
    esize=$(file_size_at "$pos" "$FILES_TXT")
    if [ -n "$esize" ]; then
      require_uint "size_bytes(@$pos)" "$esize"
      [ "$esize" -gt 0 ] && truncate -s "$esize" "$WORK/envelope.enc"
    fi

    for keyfile in ${KEYS[@]+"${KEYS[@]}"}; do
      rm -rf "$WORK/env" && mkdir -p "$WORK/env"
      if age -d -i "$keyfile" <"$WORK/envelope.enc" 2>/dev/null |
        tar xf - -C "$WORK/env/" 2>/dev/null; then
        found=1
        echo ""
        info "Decrypted envelope at file $pos"
        info "  opened with key $keyfile"
        if [ -f "$WORK/env/MANIFEST.toml" ]; then
          echo ""
          echo "--- MANIFEST.toml ---"
          cat "$WORK/env/MANIFEST.toml"
        fi
        if [ -f "$WORK/env/RECOVERY.md" ]; then
          echo ""
          echo "--- RECOVERY.md ---"
          cat "$WORK/env/RECOVERY.md"
        fi
        break
      fi
      info "  key $keyfile did not open the envelope at file $pos"
    done
    if [ "$found" -eq 1 ]; then
      break
    fi
  done < <(envelope_positions "$FILES_TXT")

  [ "$found" -eq 1 ] || die_no_envelope
  echo ""
  echo "To restore, run:"
  echo "  $0 --restore $(keys_args) --to /your/destination"
}

# ---- --restore ----

# Does an envelope MANIFEST.toml list a given unit under [[units]]? Exit 0 if
# so. Used by --restore to pick the right envelope for a universal key (#127).
manifest_has_unit() { # <manifest_path> <unit_name>
  awk -v u="$2" '__AWK_MANIFEST_HAS_UNIT__' "$1"
}

# Try every provided key against one slice, starting with whichever key opened
# the PREVIOUS slice: the slices of one volume are normally sealed to a single
# key, and a wrong key costs a full re-read of tape-sized ciphertext. It is
# deliberately NOT seeded from the key that opened the envelope — that the two
# can differ is the whole of issue #288. age's own stderr is left visible: its
# "no identity matched any of the recipients" line is the diagnostic that made
# that root cause findable.
SLICE_KEY=""
decrypt_slice() { # <ciphertext> <plaintext-out> <slice-number>
  local in=$1 out=$2 num=$3 k
  local -a order=()
  if [ -n "$SLICE_KEY" ]; then
    order+=("$SLICE_KEY")
  fi
  for k in ${KEYS[@]+"${KEYS[@]}"}; do
    if [ "$k" != "$SLICE_KEY" ]; then
      order+=("$k")
    fi
  done
  for k in ${order[@]+"${order[@]}"}; do
    if age -d -i "$k" <"$in" >"$out" 2>"$WORK/age.err"; then
      cat "$WORK/age.err" >&2 || true
      SLICE_KEY="$k"
      info "  key $k decrypted slice $num"
      return 0
    fi
    cat "$WORK/age.err" >&2 || true
    # A full disk makes age fail on EVERY key, which read as a key rotation.
    if grep -qi 'no space left' "$WORK/age.err" 2>/dev/null; then
      rm -f "$out"
      die_io "decrypting slice $num failed" "$WORK/age.err" "$(dirname "$out")"
    fi
    info "  key $k did not decrypt slice $num"
  done
  die "cannot decrypt slice $num — none of the ${#KEYS[@]} key(s) decrypted it
       Keys tried: $(keys_list)
       The key that opened the envelope need not be the key that opens the
       slices: the envelope is sealed when the volume is written, each slice
       when it was staged, so a key rotation between the two puts them on
       different key generations. Supply the older key as well — repeat
       --key once per key."
}

do_restore() {
  local destdir=$1 target_unit=$2 want_version=${3:-} scratch_parent=${4:-}

  mkdir -p "$destdir"
  [ -n "$scratch_parent" ] || scratch_parent=$destdir
  mkdir -p "$scratch_parent"

  establish_files

  # Step 1: find and decrypt the envelope that holds the target unit.
  #
  # A per-tenant key opens exactly one envelope, so the first decryptable one
  # is the right one. But the escrow recipient (ADR-0005) is on EVERY envelope,
  # so a universal key decrypts all of them and the first on tape may belong to
  # a different tenant than --unit. When a unit was named, keep searching until
  # an envelope whose manifest actually lists it is found (the operator
  # envelope always does); otherwise the first decryptable envelope wins, as
  # before. (issue #127)
  #
  # Each key is tried independently at each position (#288). Once ONE key has
  # opened an envelope that does not list --unit, the next key is pointless —
  # it is the same ciphertext with the same contents — so the search moves to
  # the next POSITION, preserving #127's behaviour.
  local found=0 pos keyfile env_key=""
  while IFS= read -r pos; do
    require_uint envelope_position "$pos"
    read_tape_raw "$pos" "$WORK/envelope.enc"
    local esize
    esize=$(file_size_at "$pos" "$FILES_TXT")
    if [ -n "$esize" ]; then
      require_uint "size_bytes(@$pos)" "$esize"
      [ "$esize" -gt 0 ] && truncate -s "$esize" "$WORK/envelope.enc"
    fi
    local opened=0
    for keyfile in ${KEYS[@]+"${KEYS[@]}"}; do
      rm -rf "$WORK/env" && mkdir -p "$WORK/env"
      if age -d -i "$keyfile" <"$WORK/envelope.enc" 2>/dev/null |
        tar xf - -C "$WORK/env/" 2>/dev/null; then
        opened=1
        env_key=$keyfile
        break
      fi
      info "Key $keyfile did not open the envelope at file $pos"
    done
    if [ "$opened" -eq 1 ]; then
      if [ -n "$target_unit" ] && [ -f "$WORK/env/MANIFEST.toml" ] &&
        ! manifest_has_unit "$WORK/env/MANIFEST.toml" "$target_unit"; then
        info "Envelope at file $pos decrypts but does not list '$target_unit'; continuing..."
        continue
      fi
      found=1
      info "Decrypted envelope at file $pos"
      info "  opened with key $env_key"
      break
    fi
  done < <(envelope_positions "$FILES_TXT")
  if [ "$found" -ne 1 ]; then
    die_no_envelope "$target_unit"
  fi
  [ -f "$WORK/env/MANIFEST.toml" ] || die "envelope missing MANIFEST.toml"

  local manifest="$WORK/env/MANIFEST.toml"

  # Step 2: identify units in manifest
  local -a unit_names
  while IFS= read -r uname; do
    unit_names+=("$uname")
  done < <(awk '__AWK_UNIT_LIST__' "$manifest")

  [ ${#unit_names[@]} -gt 0 ] || die "no units in manifest"

  if [ -z "$target_unit" ]; then
    if [ ${#unit_names[@]} -eq 1 ]; then
      target_unit="${unit_names[0]}"
    else
      echo "Units in this envelope:"
      for u in "${unit_names[@]}"; do echo "  - $u"; done
      die "multiple units found — specify one with --unit NAME"
    fi
  fi

  # Step 3: parse slices for target unit from MANIFEST.toml
  info "Parsing slices for unit: $target_unit"
  # A volume can carry the SAME unit more than once — two snapshot versions, two
  # stage sets. Collecting every matching [[units]] block concatenated both
  # versions' slices and handed the mix to dar (issue #131). Worse, when the
  # versions have different recipients (a `tenant reassign` between them), the
  # first slice is one this key cannot open and the failure reads as "wrong
  # key". So: buffer per block and emit exactly one version's slices.
  awk -v unit="$target_unit" -v want="${want_version:-}" '__AWK_SELECT_VERSION__' "$manifest" >"$WORK/slices.txt" 2>"$WORK/picked_version.txt"

  local nslices picked
  nslices=$(wc -l <"$WORK/slices.txt")
  picked=$(tr -d ' \n' <"$WORK/picked_version.txt" 2>/dev/null)
  if [ "$nslices" -eq 0 ]; then
    if [ -n "${want_version:-}" ]; then
      die "unit '$target_unit' has no version $want_version on this volume
       --find-envelope --key KEYFILE lists the versions here; --info is
       keyless and cannot see them. Omit --version for the newest."
    fi
    die "no slices found for unit '$target_unit'"
  fi
  # Always say which version is being restored: a volume can hold several, and
  # silently picking one is how an heir restores the wrong data believing it is
  # current.
  info "Restoring '$target_unit' snapshot version ${picked:-unknown} ($nslices slice(s))"

  # Step 4: room for it. Sizes come from the file map; a slice missing from it
  # is refused with its own message in step 5.
  local total=0 largest=0 spos ssize
  while IFS='|' read -r _ spos _ _; do
    ssize=$(file_size_at "$spos" "$FILES_TXT")
    [ -n "$ssize" ] || continue
    require_uint "front_index_size(@$spos)" "$ssize"
    total=$((total + ssize))
    [ "$ssize" -le "$largest" ] || largest=$ssize
  done <"$WORK/slices.txt"
  check_space "$destdir" "$scratch_parent" "$total" "$largest"
  SCRATCH="$(mktemp -d "$scratch_parent/.tapectl-restore.XXXXXX")" ||
    die "cannot create a scratch directory in $scratch_parent"

  # Step 5: cross-check each slice against the front index, then verify+decrypt
  local dar_dir="$SCRATCH/dar"
  mkdir -p "$dar_dir"
  local count=0

  while IFS='|' read -r num tpos manifest_eb manifest_sha; do
    count=$((count + 1))
    info "Slice $count/$nslices — tape file $tpos"

    local idx_size idx_hash
    idx_size=$(file_size_at "$tpos" "$FILES_TXT")
    idx_hash=$(file_hash_at "$tpos" "$FILES_TXT")
    if [ -z "$idx_size" ] || [ -z "$idx_hash" ]; then
      die "slice $num (tape file $tpos) has no data_slice entry in the file map — refusing to trust the envelope manifest alone"
    fi
    require_uint "front_index_size(@$tpos)" "$idx_size"

    if [ -n "$manifest_eb" ] && [ "$manifest_eb" != "$idx_size" ]; then
      die "slice $num size mismatch: envelope manifest says $manifest_eb bytes, file map says $idx_size bytes — tape may be tampered or damaged"
    fi
    if [ -n "$manifest_sha" ] && [ "$manifest_sha" != "$idx_hash" ]; then
      die "slice $num hash mismatch: envelope manifest and file map disagree — tape may be tampered or damaged"
    fi

    read_tape_raw "$tpos" "$SCRATCH/slice.enc"
    truncate -s "$idx_size" "$SCRATCH/slice.enc"

    local actual
    actual=$(sha256sum "$SCRATCH/slice.enc" | awk '{print $1}')
    if [ "$actual" != "$idx_hash" ]; then
      die "slice $num checksum MISMATCH (expected ${idx_hash:0:16}…, got ${actual:0:16}…)"
    fi
    info "  checksum verified against front index"

    decrypt_slice "$SCRATCH/slice.enc" "$dar_dir/restore.$num.dar" "$num"

    local bytes
    bytes=$(wc -c <"$dar_dir/restore.$num.dar")
    info "  decrypted ($((bytes / 1048576)) MiB)"
    rm -f "$SCRATCH/slice.enc"

  done <"$WORK/slices.txt"

  # Step 6: extract with dar
  info "Extracting archive to $destdir ..."
  dar -x "$dar_dir/restore" -R "$destdir" -O -Q ||
    die "dar extraction failed — dar's own message is above (No space left on device means $destdir is full)"

  rm -rf "$SCRATCH"
  SCRATCH=""
  echo ""
  info "RESTORE COMPLETE"
  info "Unit '$target_unit' restored to: $destdir"
}

# ---- main ----

case "${1:-}" in
--info)
  do_info
  ;;
--verify)
  do_verify
  ;;
--find-envelope)
  shift
  # --key may be repeated; every key is tried against every envelope (#288).
  KEYS=()
  while [ $# -gt 0 ]; do
    case "$1" in
    --key)
      [ -n "${2:-}" ] || die "usage: $0 --find-envelope --key KEYFILE"
      KEYS+=("$2")
      shift 2
      ;;
    *) die "usage: $0 --find-envelope --key KEYFILE" ;;
    esac
  done
  [ ${#KEYS[@]} -gt 0 ] || die "usage: $0 --find-envelope --key KEYFILE"
  require_key_files
  do_find_envelope
  ;;
--restore)
  shift
  KEYS=()
  dest="" unit="" want="" scratch=""
  # Every flag but --no-space-check takes a value. `shift 2` on a TRAILING bare
  # flag fails because $# is 1, `set -e` fires, and the script exits 1 having
  # printed nothing at all — the usage check below is never reached. Check the
  # arity first and say which flag was short (#133).
  while [ $# -gt 0 ]; do
    case "$1" in
    --key | --to | --unit | --version | --scratch)
      [ $# -ge 2 ] || die "$1 needs a value

       usage: $0 --restore --key KEYFILE --to DIR [--unit U] [--version N] [--scratch DIR]"
      case "$1" in
      --key) KEYS+=("$2") ;;
      --to) dest=$2 ;;
      --unit) unit=$2 ;;
      --version) want=$2 ;;
      --scratch) scratch=$2 ;;
      esac
      shift 2
      ;;
    --no-space-check)
      SKIP_SPACE_CHECK=1
      shift
      ;;
    *) die "unknown option: $1" ;;
    esac
  done
  [ ${#KEYS[@]} -gt 0 ] || die "usage: $0 --restore --key KEYFILE --to DIR [--unit U] [--version N] [--scratch DIR]"
  [ -n "$dest" ] || die "usage: $0 --restore --key KEYFILE --to DIR [--unit U] [--version N] [--scratch DIR]"
  require_key_files
  [ -z "$want" ] || require_uint version "$want"
  do_restore "$dest" "$unit" "$want" "$scratch"
  ;;
--help | -h)
  echo "RESTORE.sh — Emergency restore for tapectl volume $LABEL (layout v2)"
  echo ""
  echo "Usage:"
  echo "  $0 --info                                       Show tape layout + seal verdict"
  echo "  $0 --verify                                     Keyless integrity check"
  echo "  $0 --find-envelope --key KEYFILE [--key K2 ...]  Decrypt your envelope"
  echo "  $0 --restore --key KEYFILE [--key K2 ...] --to DIR [--unit U] [--version N]"
  echo "               [--scratch DIR] [--no-space-check]"
  echo "      Full restore. Without --version the NEWEST version of the unit on"
  echo "      this volume is restored. To see which versions this tape holds,"
  echo "      run --find-envelope --key KEYFILE: snapshot_version lives in the"
  echo "      encrypted envelope manifest, so --info cannot report it."
  echo ""
  echo "  --key may be repeated, and each key is tried on its own for the"
  echo "      envelope and for every slice: a key rotation between staging a"
  echo "      unit and writing the volume seals the two to different key"
  echo "      generations, so no single key opens both."
  echo ""
  echo "  --scratch DIR   Where the decrypted slices wait for dar (default: a"
  echo "      directory inside --to, removed afterwards). Every slice of the"
  echo "      unit is decrypted to disk before dar extracts them, so with both"
  echo "      on one disk a restore needs about twice the unit's size free,"
  echo "      plus one slice. The script checks before it reads any slice."
  echo "  --no-space-check   Skip that check (a compressed or thin-provisioned"
  echo "      filesystem can hold more than df reports)."
  echo ""
  echo "Environment:"
  echo "  TAPE_DEVICE   Tape device path (default: /dev/nst0)"
  echo ""
  echo "Requirements: mt, dd, age, dar, sha256sum, head, truncate, tar"
  ;;
"")
  echo "RESTORE.sh for tapectl volume $LABEL"
  echo "Run '$0 --help' for usage."
  ;;
*)
  # Exit 2, not 0. A typo'd mode used to print this and report success, so a
  # wrapper or cron job around the heir path read "restore worked" (#133).
  echo "RESTORE.sh for tapectl volume $LABEL" >&2
  echo "Unknown mode: $1" >&2
  echo "Run '$0 --help' for usage." >&2
  exit 2
  ;;
esac
"#;

    let result = script
        .replace("__AWK_PARSE_FILE_LIST__", AWK_PARSE_FILE_LIST)
        .replace("__AWK_CHECK_FILE_LIST__", AWK_CHECK_FILE_LIST)
        .replace("__AWK_FIND_ENVELOPE__", AWK_FIND_ENVELOPE)
        .replace("__AWK_MANIFEST_HAS_UNIT__", AWK_MANIFEST_HAS_UNIT)
        .replace("__AWK_UNIT_LIST__", AWK_UNIT_LIST)
        .replace("__AWK_SELECT_VERSION__", AWK_SELECT_VERSION)
        .replace("__LABEL__", label)
        .replace("__TOTAL_FILES__", &total_files.to_string());

    // No named-fragment placeholder may survive assembly — a stray one would
    // mean a placeholder in the template does not match any const's name (or
    // vice versa), and would ship an inert `__AWK_...__` token straight onto
    // tape. Debug-only: `no_awk_placeholder_survives_assembly` (mod tests)
    // covers the release-mode case unconditionally.
    debug_assert!(
        !result.contains("__AWK_"),
        "an __AWK_*__ placeholder was not substituted in generate_restore_script_v2"
    );

    result
}

/// Generate the planning header content — pre-v2 this was written as a
/// standalone tape file (File 3, encrypted to operator); at the v2 flip its
/// content becomes the `PLAN.toml` member of the operator envelope tar (same
/// recipients: operator + escrow) instead of a separate tape file
/// (`docs/design/volume-format-v2.md` §8 "What v2 removes" — the standalone
/// zone and `ZoneKind::PlanningHeader` are removed by the write flip, not by
/// this function). The generator itself is unchanged; only its caller and
/// packaging change in T8.
pub fn generate_planning_header(
    label: &str,
    units: &[(String, String, i64, i64)], // (unit_name, uuid, num_slices, total_bytes)
) -> String {
    let now = chrono::Utc::now().to_rfc3339();
    let mut s = format!(
        r#"[planning]
status = "planned"
volume = "{label}"
planned_at = "{now}"
"#
    );

    // One `[[units]]` header per unit — emitting it once before the loop put
    // every unit's keys into the same table, producing duplicate-key invalid
    // TOML for any multi-unit volume (T14).
    for (name, uuid, slices, bytes) in units {
        s.push_str(&format!(
            r#"
[[units]]
name = "{name}"
uuid = "{uuid}"
num_slices = {slices}
total_bytes = {bytes}
"#
        ));
    }
    s
}

/// One entry in the plaintext **front index** (File 3, layout v2). Navigation is
/// total — every file has `position` and `type` — but `size_bytes` is `None` for
/// the front index's own entry (its length is self-referential), and
/// `sha256_encrypted` is `None` for both the front index itself (self-reference)
/// and the seal marker (not yet written when File 3 is generated). See
/// `docs/design/volume-format-v2.md` §3-4 and ADR-0007.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontIndexFile {
    pub position: i32,
    pub type_label: &'static str,
    pub size_bytes: Option<u64>,
    /// sha256 of the file's on-tape bytes (ciphertext for encrypted zones, the
    /// plaintext bytes for Files 0-2). Hex; `None` where excluded (above).
    pub sha256_encrypted: Option<String>,
}

/// Generate the plaintext **front index** (File 3, layout v2). It maps every
/// tape position to its type and on-tape byte size, and carries the sha256 of
/// every file's on-tape (encrypted) bytes except its own and the seal marker's —
/// giving an heir keyless navigation and keyless byte-integrity with only `dd`
/// and `sha256sum` (ADR-0007). It carries NO content metadata: no filenames, no
/// plaintext-content hashes, no tenant/unit names — the isolation invariant of
/// `volume-format-v2.md` §2. The ciphertext hashes are safe in plaintext because
/// they hash pseudorandom age output and are non-attributable (D4).
pub fn generate_front_index(label: &str, files: &[FrontIndexFile]) -> String {
    let mut s = format!(
        r#"================================================================
                    TAPECTL FRONT INDEX
================================================================

Volume: {label}
This file maps every tape position to its type, on-tape byte size, and
the sha256 of its on-tape (encrypted) bytes. It contains NO content
metadata: no filenames, no plaintext checksums, no tenant or unit names.
The trailing seal marker binds this index by hash; see the system guide.

================================================================
              MACHINE-READABLE DATA (TOML)
================================================================

[index]
volume = "{label}"
layout_version = 2
"#
    );

    append_files_entries(&mut s, files);

    s
}

/// Emit `[[files]]` entries in the line-oriented grammar shared by the front
/// index and the seal marker's embedded copy (one `key = value` per line — the
/// shell-parseable contract RESTORE.sh depends on). One emitter so the two
/// cannot drift.
fn append_files_entries(s: &mut String, files: &[FrontIndexFile]) {
    for f in files {
        s.push_str("\n[[files]]\n");
        s.push_str(&format!("position = {}\n", f.position));
        s.push_str(&format!("type = \"{}\"\n", f.type_label));
        if let Some(sz) = f.size_bytes {
            s.push_str(&format!("size_bytes = {sz}\n"));
        }
        if let Some(h) = &f.sha256_encrypted {
            s.push_str(&format!("sha256_encrypted = \"{h}\"\n"));
        }
    }
}

/// Generate the plaintext **seal marker** (the last file, layout v2). Its
/// presence is the completeness assertion — "every file before me is present" —
/// and its `front_index_sha256` binds the front index, making the seal marker
/// the unhashed root of the keyless integrity chain (seal marker → front index →
/// every content file; ADR-0007, `volume-format-v2.md` §4). Its absence means
/// the tape is legitimately unsealed (interrupted or EOT-aborted).
///
/// `files` is the **embedded full copy of the front index** (ratified
/// 2026-07-22): two-ended redundancy — front-of-tape damage recovers the map
/// from the tail; tail damage reads as unsealed but stays navigable from the
/// front. By seal time File 3's bytes are known, so the caller fills in File 3's
/// own `size_bytes` + `sha256_encrypted` (more complete than File 3 itself);
/// only the seal marker's own entry stays hash-less (self-reference). The copy
/// is not hash-protected by anything on the tape — readers validate its per-file
/// claims by hashing the files they describe (`volume-format-v2.md` §4).
pub fn generate_seal_marker(
    label: &str,
    file_count: i32,
    front_index_sha256: &str,
    files: &[FrontIndexFile],
) -> String {
    // FIXED-WIDTH timestamp, deliberately: the build step sizes the seal
    // marker with a placeholder and the seal step regenerates it with the
    // real `sealed_at`, which is only sound if both render at identical
    // byte length (`v2-open-questions.md` §9). `to_rfc3339()` does NOT
    // guarantee that — it is `SecondsFormat::AutoSi`, which drops
    // trailing-zero fractional digits, so it emits 25/29/32/35-byte strings
    // depending on the nanosecond value (measured on this VM: ~99.89% at
    // 35 bytes, ~0.11% at 32, ~0.0002% at 29). Under that format a real
    // reseal would land on a different width roughly once per ~900 writes
    // and fail its own length-identity check. `SecondsFormat::Secs` with
    // `use_z = true` renders exactly 20 bytes ("2026-07-22T20:09:00Z"),
    // always — second precision is ample for an audit timestamp, and
    // fixed width is what the sizing trick actually requires.
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let mut s = format!(
        r#"================================================================
                    TAPECTL SEAL MARKER
================================================================

Volume: {label}
This file seals the tape: its presence means every file before it is
present. Its absence means the tape is unsealed (interrupted or aborted).
It binds the front index by the sha256 below. The [[files]] entries are
a full copy of the front index (File 3), usable if File 3 is damaged —
verify any entry by hashing the file it describes.

================================================================
              MACHINE-READABLE DATA (TOML)
================================================================

[seal]
volume = "{label}"
layout_version = 2
file_count = {file_count}
sealed_at = "{now}"
front_index_sha256 = "{front_index_sha256}"
"#
    );
    append_files_entries(&mut s, files);
    s
}

/// Generate MANIFEST.toml for a tenant envelope.
///
/// A thin wrapper over [`crate::volume::manifest::Manifest::to_toml`] — the
/// single writer/reader type for the on-tape envelope manifest (see
/// `src/volume/manifest.rs`). Kept here, under its original name and
/// signature, because every call site addresses it as `layout::
/// generate_manifest_toml`.
pub fn generate_manifest_toml(label: &str, tenant_name: &str, units: &[ManifestUnit]) -> String {
    let now = chrono::Utc::now().to_rfc3339();
    let manifest = crate::volume::manifest::Manifest {
        volume: label.to_string(),
        tenant: tenant_name.to_string(),
        created_at: now,
        units: units.to_vec(),
    };
    manifest.to_toml()
}

/// Generate RECOVERY.md for a tenant envelope.
pub fn generate_recovery_md(label: &str, tenant_name: &str, units: &[ManifestUnit]) -> String {
    let now = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let mut s = format!(
        "# Recovery Guide for {tenant_name}\n\n\
         Volume: {label}\n\
         Date: {now}\n\n\
         This tape holds age-encrypted `dar` archives. With your age key(s) and the\n\
         standard tools (`mt`, `dd`, `truncate`, `age`, `dar`, `sha256sum`) you can\n\
         recover your data by hand — no tapectl required. The automated `RESTORE.sh`\n\
         (tape file 2) does exactly these steps for you; use it if you can.\n\n\
         ## Which key\n\n\
         The key that opened this envelope is\n\
         not necessarily the key that opens the slices below.\n\
         The envelope was sealed when the tape was written; each slice was sealed\n\
         earlier, when it was staged. If the keys were rotated in between, the\n\
         slices need an older key. `age` accepts several keys at once and uses\n\
         whichever one matches, so pass every key you hold:\n\n\
         ```bash\n\
         age -d -i OLD.age.key -i NEW.age.key ...\n\
         ```\n\n\
         Name only key files that exist:\n\
         one unreadable `-i` path fails the whole command.\n\
         If every key fails with `no identity matched any of the recipients`, the\n\
         slice was sealed to a key you have not supplied — the data is not corrupt.\n\n\
         ## Units in this envelope\n\n\
         | Unit | Snapshot | Slices | Tape files |\n\
         |------|----------|--------|------------|\n"
    );
    for unit in units {
        let (first, last) = match (unit.slices.first(), unit.slices.last()) {
            (Some(f), Some(l)) => (f.tape_position, l.tape_position),
            _ => (0, 0),
        };
        s.push_str(&format!(
            "| {} | v{} | {} | {}..{} |\n",
            unit.name,
            unit.snapshot_version,
            unit.slices.len(),
            first,
            last,
        ));
    }
    s.push('\n');

    for unit in units {
        s.push_str(&format!(
            "## {}\n\n\
             UUID: `{}`  ·  snapshot v{}\n\n\
             Put the drive in fixed 512KB block mode, then read, trim, verify and\n\
             decrypt each slice. `/dev/nst0` below is an example — run\n\
             `ls -l /dev/tape/by-id/` and substitute your own drive, or you may\n\
             read a different tape:\n\n\
             ```bash\n\
             mt -f /dev/nst0 setblk 524288\n\n\
             # On each `age -d` line, add `-i <file>` for every other key you hold\n\
             # (see \"Which key\" above).\n\n",
            unit.name, unit.uuid, unit.snapshot_version,
        ));
        for slice in &unit.slices {
            // The number in `restore.N.dar` MUST be dar's slice number, and the
            // slices MUST share the base name `restore` — this is dar's required
            // `base.N.dar` convention. `truncate` trims the 512KB block padding
            // that would otherwise make age reject the ciphertext.
            s.push_str(&format!(
                "# Slice {n} — tape file {pos}, {eb} bytes\n\
                 mt -f /dev/nst0 rewind && mt -f /dev/nst0 fsf {pos}\n\
                 dd if=/dev/nst0 bs=512k of=restore.{n}.dar.age\n\
                 truncate -s {eb} restore.{n}.dar.age\n\
                 echo \"{sha}  restore.{n}.dar.age\" | sha256sum -c -\n\
                 age -d -i YOUR_KEY.age.key restore.{n}.dar.age > restore.{n}.dar\n\n",
                n = slice.number,
                pos = slice.tape_position,
                eb = slice.encrypted_bytes,
                sha = slice.sha256_encrypted,
            ));
        }
        s.push_str(
            "# Reassemble and extract all slices (they share the base name `restore`):\n\
             dar -x restore -R /destination -O -Q\n\
             ```\n\n\
             `-O` ignores stored ownership, needed when restoring as a non-root user.\n\n",
        );
    }

    s.push_str(
        "## Troubleshooting\n\n\
         - **age: \"unexpected data\" / decryption fails** — the slice still has 512KB\n\
           block padding. Re-run `truncate -s <bytes>` to the exact size shown above.\n\
         - **dar: cannot open the archive** — the decrypted slices must be named\n\
           `restore.1.dar`, `restore.2.dar`, … with no gaps, and extracted with\n\
           `dar -x restore` (base name `restore`, no `.N.dar` suffix in the command).\n\
         - **sha256 mismatch** — re-read the slice from tape; a short read or the wrong\n\
           block mode (must be 512KB fixed) is the usual cause.\n\
         - **age: \"no identity matched any of the recipients\"** — none of the keys\n\
           you passed is the one this slice was sealed to. It may be an older key from\n\
           before a rotation (see \"Which key\"), or the operator key, which can read\n\
           every unit. Pass them all with repeated `-i`.\n",
    );

    s
}

/// Re-exported so every existing `layout::ManifestUnit` / `layout::
/// ManifestSlice` path keeps compiling. The canonical definitions live in
/// [`crate::volume::manifest`], which both this module's writer and
/// `envelope`'s reader now share. `ManifestSlice::tape_position` is `i64`
/// here (previously `i32` in this module) — the reader's original, wider
/// type; the writer's `i32` was the narrower mistake.
pub use crate::volume::manifest::{ManifestSlice, ManifestUnit};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::volume::restore_script::{
        AWK_CHECK_FILE_LIST, AWK_FIND_ENVELOPE, AWK_MANIFEST_HAS_UNIT, AWK_PARSE_FILE_LIST,
        AWK_SELECT_VERSION, AWK_UNIT_LIST,
    };

    #[test]
    fn front_index_lists_every_file_with_hashes() {
        let files = vec![
            FrontIndexFile {
                position: 0,
                type_label: "id_thunk",
                size_bytes: Some(500),
                sha256_encrypted: Some("aa00".into()),
            },
            FrontIndexFile {
                position: 3,
                type_label: "front_index",
                size_bytes: None,       // self: length is self-referential
                sha256_encrypted: None, // self: cannot hash itself
            },
            FrontIndexFile {
                position: 4,
                type_label: "data_slice",
                size_bytes: Some(524288),
                sha256_encrypted: Some("bb11".into()),
            },
            FrontIndexFile {
                position: 5,
                type_label: "seal_marker",
                size_bytes: None,
                sha256_encrypted: None, // not yet written when File 3 is built
            },
        ];
        let s = generate_front_index("TEST01", &files);
        let body = &s[s.find("[index]").expect("has [index]")..];
        let parsed: toml::Value = body.parse().expect("TOML parses");
        let idx = parsed.get("index").unwrap();
        assert_eq!(idx.get("volume").unwrap().as_str(), Some("TEST01"));
        assert_eq!(idx.get("layout_version").unwrap().as_integer(), Some(2));

        let arr = parsed.get("files").unwrap().as_array().unwrap();
        assert_eq!(arr.len(), 4);
        // File 0 carries size + hash.
        assert_eq!(arr[0].get("position").unwrap().as_integer(), Some(0));
        assert_eq!(arr[0].get("size_bytes").unwrap().as_integer(), Some(500));
        assert_eq!(
            arr[0].get("sha256_encrypted").unwrap().as_str(),
            Some("aa00")
        );
        // The slice carries size + hash.
        assert_eq!(arr[2].get("size_bytes").unwrap().as_integer(), Some(524288));
        assert_eq!(
            arr[2].get("sha256_encrypted").unwrap().as_str(),
            Some("bb11")
        );
    }

    #[test]
    fn front_index_omits_self_and_seal_marker_hashes() {
        // The hash-chain rule (ADR-0007): the front index carries no hash of
        // itself (self-reference) or the seal marker (not yet written).
        let files = vec![
            FrontIndexFile {
                position: 3,
                type_label: "front_index",
                size_bytes: None,
                sha256_encrypted: None,
            },
            FrontIndexFile {
                position: 5,
                type_label: "seal_marker",
                size_bytes: None,
                sha256_encrypted: None,
            },
        ];
        let s = generate_front_index("TEST01", &files);
        let body = &s[s.find("[index]").unwrap()..];
        let parsed: toml::Value = body.parse().expect("TOML parses");
        let arr = parsed.get("files").unwrap().as_array().unwrap();
        // Both entries are navigable (position + type) but carry neither a
        // size nor a hash.
        for e in arr {
            assert!(e.get("position").is_some());
            assert!(e.get("type").is_some());
            assert!(e.get("size_bytes").is_none());
            assert!(e.get("sha256_encrypted").is_none());
        }
    }

    #[test]
    fn seal_marker_binds_front_index() {
        let s = generate_seal_marker("TEST01", 7, "deadbeefcafe", &[]);
        let body = &s[s.find("[seal]").expect("has [seal]")..];
        let parsed: toml::Value = body.parse().expect("TOML parses");
        let seal = parsed.get("seal").unwrap();
        assert_eq!(seal.get("volume").unwrap().as_str(), Some("TEST01"));
        assert_eq!(seal.get("layout_version").unwrap().as_integer(), Some(2));
        assert_eq!(seal.get("file_count").unwrap().as_integer(), Some(7));
        assert_eq!(
            seal.get("front_index_sha256").unwrap().as_str(),
            Some("deadbeefcafe")
        );
        // sealed_at is present and RFC3339-parseable.
        let sealed_at = seal.get("sealed_at").unwrap().as_str().unwrap();
        assert!(chrono::DateTime::parse_from_rfc3339(sealed_at).is_ok());
    }

    #[test]
    fn seal_marker_timestamp_is_fixed_width() {
        // Load-bearing for the build/seal placeholder-sizing trick
        // (v2-open-questions.md §9): build sizes the seal with a placeholder
        // timestamp, seal() regenerates with the real sealed_at, and the two
        // MUST be byte-length identical. chrono's plain to_rfc3339() is
        // AutoSi (drops trailing-zero fractional digits -> 25/29/32/35-byte
        // outputs), which would break that ~1 write in 900. SecondsFormat::Secs
        // + use_z renders exactly 20 bytes, always.
        let files = vec![FrontIndexFile {
            position: 0,
            type_label: "id_thunk",
            size_bytes: Some(10),
            sha256_encrypted: Some("aa".into()),
        }];
        let first = generate_seal_marker("TEST01", 2, "fi", &files);
        for _ in 0..64 {
            let again = generate_seal_marker("TEST01", 2, "fi", &files);
            assert_eq!(
                first.len(),
                again.len(),
                "seal marker length varies between generations — the placeholder \
                 sizing trick (and therefore seal()) is broken"
            );
        }
        // Pin the exact rendering too, so a future edit to the format is a
        // deliberate act rather than an accident.
        let body = &first[first.find("[seal]").unwrap()..];
        let parsed: toml::Value = body.parse().expect("TOML parses");
        let sealed_at = parsed
            .get("seal")
            .unwrap()
            .get("sealed_at")
            .unwrap()
            .as_str()
            .unwrap();
        assert_eq!(sealed_at.len(), 20, "sealed_at must be exactly 20 bytes");
        assert!(sealed_at.ends_with('Z'), "sealed_at must be Z-suffixed UTC");
        assert!(chrono::DateTime::parse_from_rfc3339(sealed_at).is_ok());
    }

    #[test]
    fn seal_marker_embeds_front_index_copy() {
        // The embedded copy (ratified 2026-07-22) is MORE complete than File 3:
        // by seal time File 3's own size + hash are known, so its entry is
        // filled in; only the seal marker's own entry stays hash-less.
        let files = vec![
            FrontIndexFile {
                position: 0,
                type_label: "id_thunk",
                size_bytes: Some(500),
                sha256_encrypted: Some("aa00".into()),
            },
            FrontIndexFile {
                position: 3,
                type_label: "front_index",
                size_bytes: Some(2048),                // known at seal time
                sha256_encrypted: Some("fi99".into()), // known at seal time
            },
            FrontIndexFile {
                position: 4,
                type_label: "seal_marker",
                size_bytes: None,
                sha256_encrypted: None, // self-reference: never hashable
            },
        ];
        let s = generate_seal_marker("TEST01", 5, "fi99", &files);
        let body = &s[s.find("[seal]").unwrap()..];
        let parsed: toml::Value = body.parse().expect("TOML parses");
        let arr = parsed.get("files").unwrap().as_array().unwrap();
        assert_eq!(arr.len(), 3);
        // File 3's entry in the COPY carries size + hash (unlike in File 3).
        assert_eq!(arr[1].get("type").unwrap().as_str(), Some("front_index"));
        assert_eq!(arr[1].get("size_bytes").unwrap().as_integer(), Some(2048));
        assert_eq!(
            arr[1].get("sha256_encrypted").unwrap().as_str(),
            Some("fi99")
        );
        // The seal marker's own entry stays hash-less.
        assert_eq!(arr[2].get("type").unwrap().as_str(), Some("seal_marker"));
        assert!(arr[2].get("sha256_encrypted").is_none());
        // The embedded copy's grammar matches the front index's byte-for-byte
        // (same emitter): the [[files]] tail of both documents is identical.
        let fi = generate_front_index("TEST01", &files);
        let fi_tail = &fi[fi.find("\n[[files]]").unwrap()..];
        let seal_tail = &s[s.find("\n[[files]]").unwrap()..];
        assert_eq!(fi_tail, seal_tail);
    }

    #[test]
    fn planning_header_embeds_unit_rows() {
        let units = vec![
            ("alpha".to_string(), "uuid-a".to_string(), 3, 10_000),
            ("beta".to_string(), "uuid-b".to_string(), 1, 500),
        ];
        let s = generate_planning_header("LAB01", &units);
        // Must be valid TOML with TWO distinct [[units]] entries — the old
        // single-[[units]] form produced duplicate keys and failed to parse
        // for any multi-unit volume (T14).
        let parsed: toml::Value = s.parse().expect("planning header must be valid TOML");
        assert_eq!(parsed["planning"]["volume"].as_str(), Some("LAB01"));
        let units_arr = parsed["units"].as_array().expect("units array");
        assert_eq!(units_arr.len(), 2);
        assert_eq!(units_arr[0]["name"].as_str(), Some("alpha"));
        assert_eq!(units_arr[0]["num_slices"].as_integer(), Some(3));
        assert_eq!(units_arr[0]["total_bytes"].as_integer(), Some(10_000));
        assert_eq!(units_arr[1]["name"].as_str(), Some("beta"));
        assert_eq!(units_arr[1]["uuid"].as_str(), Some("uuid-b"));
    }

    #[test]
    fn tenant_recovery_md_has_working_dar_recipe() {
        // The generated manual recipe must match what the (gate-verified)
        // RESTORE.sh does: truncate to encrypted_bytes, restore.N.dar naming,
        // `dar -x restore`. The old recipe (H2) had none of these.
        let units = vec![ManifestUnit {
            name: "alpha".into(),
            uuid: "uuid-a".into(),
            snapshot_version: 2,
            stage_set_id: 7,
            dar_version: Some("2.7.20".into()),
            dar_command: Some("dar -c base -R /src".into()),
            slices: vec![ManifestSlice {
                number: 1,
                tape_position: 4,
                size_bytes: 1_048_576,
                encrypted_bytes: 1_049_000,
                sha256_plain: "abc".into(),
                sha256_encrypted: "def456".into(),
            }],
        }];
        let s = generate_recovery_md("LAB01", "alice", &units);
        // Correct commands present:
        assert!(s.contains("mt -f /dev/nst0 setblk 524288"));
        assert!(s.contains("dd if=/dev/nst0 bs=512k of=restore.1.dar.age"));
        assert!(s.contains("truncate -s 1049000 restore.1.dar.age"));
        assert!(s.contains("def456  restore.1.dar.age")); // sha256sum -c line
        assert!(s.contains("> restore.1.dar"));
        assert!(s.contains("dar -x restore -R /destination -O -Q"));
        // Broken forms from H2 must be gone:
        assert!(!s.contains("slice_1.dar"), "old slice_N naming leaked");
        assert!(!s.contains("ARCHIVE_BASE"), "placeholder leaked");
        assert!(!s.contains("bs=64k"));
    }

    /// #312: the envelope is sealed at WRITE time and each slice at STAGE
    /// time (`build.rs` vs `staging/mod.rs`), so a `key rotate` between the
    /// two leaves no single key that opens both (#288, on a real tape).
    /// RECOVERY.md is read by someone who has already opened the envelope —
    /// the manual path must not let them conclude that key is the only one
    /// the slices can need. Each claim below was measured against the real
    /// `age` CLI: several `-i` are accepted at once, one unreadable `-i`
    /// path fails the whole command, and a missing key reports "no identity
    /// matched any of the recipients" (it does not fail silently).
    #[test]
    fn recovery_md_says_the_slices_may_need_a_different_key() {
        let units = vec![ManifestUnit {
            name: "alpha".into(),
            uuid: "uuid-a".into(),
            snapshot_version: 1,
            stage_set_id: 1,
            dar_version: None,
            dar_command: None,
            slices: vec![ManifestSlice {
                number: 1,
                tape_position: 4,
                size_bytes: 1,
                encrypted_bytes: 2,
                sha256_plain: "abc".into(),
                sha256_encrypted: "def".into(),
            }],
        }];
        let s = generate_recovery_md("LAB01", "alice", &units);
        assert!(
            s.contains("not necessarily the key that opens the slices"),
            "must say the envelope's key may not open the slices"
        );
        assert!(
            s.contains("-i OLD.age.key -i NEW.age.key"),
            "must show age taking several identities at once"
        );
        assert!(
            s.contains("one unreadable `-i` path fails the whole command"),
            "must warn that naming a missing key file fails age outright"
        );
        assert!(
            s.contains("no identity matched any of the recipients"),
            "must name the error the reader will actually see"
        );
        // The old troubleshooting claim was false: age does not fail silently.
        assert!(
            !s.contains("silently fails"),
            "stale 'silently fails' claim"
        );
    }

    /// #130: every heir-facing document hands out literal `mt -f /dev/nst0`
    /// commands, and `/dev/nst0` is a guess about the heir's machine rather
    /// than a fact about the tape. It was wrong on this very dev VM after a
    /// routine reboot, where a real LTO-6 and mhvtl swapped node numbers —
    /// and a wrong device is not reliably loud: with another tape loaded, the
    /// commands succeed and recover the WRONG volume.
    ///
    /// These are frozen plaintext zones that cannot check anything at read
    /// time the way RESTORE.sh now does, so the caveat is all they get. The
    /// issue named two documents; RECOVERY.md is a third with the same defect.
    #[test]
    fn every_heir_document_says_the_device_may_not_be_nst0() {
        let params = IdThunkV2Params {
            label: "DEV130",
            uuid: "11111111-2222-3333-4444-555555555555",
            media_type: "LTO-6",
            tapectl_version: "0.2.0",
            nominal_capacity: 2_500_000_000_000,
            mam_capacity: 2_400_000_000_000,
            total_files: 27,
            mam_manufacturer: "IBM",
            mam_serial: "SERIAL1",
            mam_length: 846,
            mam_loads: 5,
            created_at: "2026-07-22T20:09:00Z",
            cartridge_identity_source: None,
        };
        let units = vec![ManifestUnit {
            name: "alpha".into(),
            uuid: "uuid-a".into(),
            snapshot_version: 2,
            stage_set_id: 7,
            dar_version: None,
            dar_command: None,
            slices: vec![ManifestSlice {
                number: 1,
                tape_position: 4,
                size_bytes: 1_048_576,
                encrypted_bytes: 1_049_000,
                sha256_plain: "abc".into(),
                sha256_encrypted: "def456".into(),
            }],
        }];

        for (what, doc) in [
            ("ID thunk (File 0)", generate_id_thunk_v2(&params)),
            (
                "system guide (File 1)",
                generate_system_guide_v2("DEV130", 27),
            ),
            (
                "RECOVERY.md",
                generate_recovery_md("DEV130", "alice", &units),
            ),
        ] {
            // The runnable example stays — an heir needs something to type.
            assert!(
                doc.contains("/dev/nst0"),
                "{what} should keep a concrete example device"
            );
            // But it must say the example may be wrong, and how to find out.
            assert!(
                doc.contains("ls -l /dev/tape/by-id/"),
                "{what} must tell the heir how to list the real drives"
            );
            assert!(
                doc.contains("example"),
                "{what} must mark /dev/nst0 as an example, not a fact"
            );
        }
    }

    /// #134: the envelope manifest used to carry `layout_version = 1` on a v2
    /// tape. It was a v1-era constant the format flip never touched — nothing
    /// has ever read it, and it contradicted the ID thunk, the front index and
    /// the seal marker, all of which say 2. An heir comparing the two would
    /// conclude the tape was inconsistent when it is not.
    ///
    /// Deleted rather than bumped: a field no reader consults cannot be kept
    /// honest, which is the same trap as `meta.schema_version` (#61). This
    /// test exists so it cannot drift back in.
    #[test]
    fn the_envelope_manifest_carries_no_layout_version() {
        let s = generate_manifest_toml("LAB01", "alice", &[]);
        assert!(
            !s.contains("layout_version"),
            "the envelope manifest must not claim a layout version — the tape's \
             own zones are authoritative:\n{s}"
        );
        let parsed: toml::Value = s.parse().expect("manifest must still be valid TOML");
        let m = parsed.get("manifest").expect("[manifest] table survives");
        assert_eq!(m.get("volume").unwrap().as_str(), Some("LAB01"));
        assert_eq!(m.get("tenant").unwrap().as_str(), Some("alice"));
    }

    #[test]
    fn manifest_toml_round_trips_slices() {
        let units = vec![ManifestUnit {
            name: "alpha".into(),
            uuid: "uuid-a".into(),
            snapshot_version: 1,
            stage_set_id: 7,
            dar_version: Some("2.7.20".into()),
            dar_command: Some("dar -c base -R /src".into()),
            slices: vec![ManifestSlice {
                number: 1,
                tape_position: 4,
                size_bytes: 1_048_576,
                encrypted_bytes: 1_049_000,
                sha256_plain: "abc".into(),
                sha256_encrypted: "def".into(),
            }],
        }];
        let s = generate_manifest_toml("LAB01", "alice", &units);
        let parsed: toml::Value = s.parse().expect("manifest parses as TOML");
        let m = parsed.get("manifest").unwrap();
        assert_eq!(m.get("volume").unwrap().as_str(), Some("LAB01"));
        assert_eq!(m.get("tenant").unwrap().as_str(), Some("alice"));
        let u = &parsed.get("units").unwrap().as_array().unwrap()[0];
        assert_eq!(u.get("name").unwrap().as_str(), Some("alpha"));
        assert_eq!(u.get("dar_version").unwrap().as_str(), Some("2.7.20"));
        // #39: provenance fields for selective restore.
        assert_eq!(u.get("stage_set_id").unwrap().as_integer(), Some(7));
        assert_eq!(
            u.get("dar_command").unwrap().as_str(),
            Some("dar -c base -R /src")
        );
        let slice = &u.get("slices").unwrap().as_array().unwrap()[0];
        assert_eq!(slice.get("number").unwrap().as_integer(), Some(1));
        assert_eq!(slice.get("tape_position").unwrap().as_integer(), Some(4));
        assert_eq!(slice.get("sha256_plain").unwrap().as_str(), Some("abc"));
    }

    #[test]
    fn id_thunk_v2_parses_with_only_v2_layout_fields() {
        let params = IdThunkV2Params {
            label: "TEST01",
            uuid: "11111111-2222-3333-4444-555555555555",
            media_type: "LTO-6",
            tapectl_version: "0.2.0",
            nominal_capacity: 2_500_000_000_000,
            mam_capacity: 2_400_000_000_000,
            total_files: 27,
            mam_manufacturer: "IBM",
            mam_serial: "SERIAL1",
            mam_length: 846,
            mam_loads: 5,
            created_at: "2026-07-22T20:09:00Z",
            cartridge_identity_source: None,
        };
        let s = generate_id_thunk_v2(&params);
        let toml_start = s.find("[volume]").expect("has [volume] section");
        let body = &s[toml_start..];
        let parsed: toml::Value = body.parse().expect("TOML parses");

        let volume = parsed.get("volume").unwrap();
        assert_eq!(
            volume.get("magic").unwrap().as_str(),
            Some("tapectl-volume-v2")
        );
        assert_eq!(volume.get("label").unwrap().as_str(), Some("TEST01"));
        assert_eq!(
            volume.get("uuid").unwrap().as_str(),
            Some("11111111-2222-3333-4444-555555555555")
        );
        assert_eq!(volume.get("layout_version").unwrap().as_integer(), Some(2));

        // [layout] carries ONLY front_index, seal_marker, total_files (sheet
        // §2.3) — every v1 position field is gone entirely, not just unset.
        let layout = parsed.get("layout").unwrap();
        let layout_table = layout.as_table().expect("[layout] is a table");
        assert_eq!(
            layout_table.len(),
            3,
            "[layout] must carry exactly 3 keys, found: {:?}",
            layout_table.keys().collect::<Vec<_>>()
        );
        assert_eq!(layout.get("front_index").unwrap().as_integer(), Some(3));
        assert_eq!(layout.get("seal_marker").unwrap().as_integer(), Some(26)); // total_files - 1
        assert_eq!(layout.get("total_files").unwrap().as_integer(), Some(27));
        for v1_key in [
            "data_start",
            "data_end",
            "mini_index",
            "first_envelope",
            "num_envelopes",
            "operator_envelope",
            "operator_envelope_backup",
        ] {
            assert!(
                layout.get(v1_key).is_none(),
                "v1 position field '{v1_key}' must be absent from the v2 [layout] table"
            );
        }

        let media = parsed.get("media").unwrap();
        assert_eq!(
            media.get("cartridge_serial").unwrap().as_str(),
            Some("SERIAL1")
        );
    }

    /// File 0 is the first thing a finder reads, and its "read the guide"
    /// command must strip the NUL block padding and nothing else. It is in a
    /// raw string, where `'\\0'` stays two backslashes: `tr -d '\\0'` then
    /// deleted every backslash and every digit 0 and kept the NULs ("2026"
    /// became "226"). Found 2026-09-28 by the docs pass. This runs the
    /// thunk's own `tr` words through a shell, so it tests what the reader
    /// types, not how the source spells it.
    #[test]
    fn id_thunk_guide_command_strips_nuls_and_keeps_zeros() {
        let params = IdThunkV2Params {
            label: "TEST01",
            uuid: "11111111-2222-3333-4444-555555555555",
            media_type: "LTO-6",
            tapectl_version: "0.2.0",
            nominal_capacity: 2_500_000_000_000,
            mam_capacity: 2_400_000_000_000,
            total_files: 27,
            mam_manufacturer: "IBM",
            mam_serial: "SERIAL1",
            mam_length: 846,
            mam_loads: 5,
            created_at: "2026-07-22T20:09:00Z",
            cartridge_identity_source: None,
        };
        let s = generate_id_thunk_v2(&params);
        let tr_cmds: Vec<&str> = s
            .lines()
            .filter(|l| l.contains("GUIDE.md") && l.contains("tr -d"))
            .map(|l| {
                let from = l.find("tr -d").unwrap();
                let to = l[from..].find(" >").map(|i| from + i).unwrap_or(l.len());
                &l[from..to]
            })
            .collect();
        assert_eq!(tr_cmds.len(), 2, "both guide-read commands found: {s}");
        for tr in tr_cmds {
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("printf 'Guide 2026 v0.1\\n\\0\\0\\0' | {tr}"))
                .output()
                .expect("sh runs");
            assert_eq!(
                out.stdout, b"Guide 2026 v0.1\n",
                "`{tr}` must remove the NUL padding and keep every 0"
            );
        }
    }

    #[test]
    fn id_thunk_v2_is_byte_identical_across_two_calls_given_the_same_created_at() {
        // T6 review finding #5: before `created_at` was injectable, the ID
        // thunk read the clock internally, so two `generate_id_thunk_v2`
        // calls could never be compared for byte-identity in a test — only
        // `system_guide`/`restore_sh` (2 of ~9 zone kinds) were checkable,
        // leaving layout-session.md's "same inputs + same generation
        // timestamp ⇒ reproducible Layout" clause unverified for this zone.
        // With the timestamp injected, this now holds directly.
        let params = IdThunkV2Params {
            label: "DETERM1",
            uuid: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
            media_type: "LTO-6",
            tapectl_version: "0.2.0",
            nominal_capacity: 2_500_000_000_000,
            mam_capacity: 2_400_000_000_000,
            total_files: 12,
            mam_manufacturer: "IBM",
            mam_serial: "SERIAL9",
            mam_length: 846,
            mam_loads: 1,
            created_at: "2026-07-22T20:09:00Z",
            cartridge_identity_source: None,
        };
        let a = generate_id_thunk_v2(&params);
        let b = generate_id_thunk_v2(&params);
        assert_eq!(
            a, b,
            "id thunk must be byte-identical across two calls with the same created_at"
        );
    }

    /// THE COMPATIBILITY GUARANTEE for every tape already written (issue
    /// #192). `cartridge_identity_source` is emitted only when it is known;
    /// with `None` the ID thunk must be byte-for-byte what it was before the
    /// field existed — not "equivalent TOML", not "the same minus a blank
    /// line". A stray newline here would be invisible to every TOML parser
    /// and would still change File 0's size and hash in the front index, so
    /// the pin is on the exact bytes.
    ///
    /// The constant below was taken from the generator BEFORE the field was
    /// added. It is not a golden-file re-pin of the on-tape format (File 0 is
    /// deliberately unpinned, `tests/on_tape_golden.rs`) — it pins only that
    /// the ABSENT case is unchanged. If a later change to the ID thunk makes
    /// this fail, that later change is what must be justified.
    #[test]
    fn id_thunk_with_no_identity_source_is_byte_identical_to_the_pre_field_output() {
        use sha2::{Digest, Sha256};

        let params = IdThunkV2Params {
            label: "COMPAT1",
            uuid: "11111111-2222-3333-4444-555555555555",
            media_type: "LTO-6",
            tapectl_version: "0.2.0",
            nominal_capacity: 2_500_000_000_000,
            mam_capacity: 2_400_000_000_000,
            total_files: 27,
            mam_manufacturer: "IBM",
            mam_serial: "SERIAL1",
            mam_length: 846,
            mam_loads: 5,
            created_at: "2026-07-22T20:09:00Z",
            cartridge_identity_source: None,
        };
        let rendered = generate_id_thunk_v2(&params);
        // 2026-09-28: the two guide-read commands were corrected from
        // `tr -d '\\0'` (deleted every 0 digit, kept the NULs) to
        // `tr -d '\0'` — see `id_thunk_guide_command_strips_nuls_and_keeps_zeros`.
        // The pin keeps its meaning: undo exactly that correction and the
        // bytes must still be the pre-field output, so nothing else moved.
        let good = "tr -d '\\0' > GUIDE.md";
        assert_eq!(rendered.matches(good).count(), 2, "{rendered}");
        let pre_fix = rendered.replace(good, "tr -d '\\\\0' > GUIDE.md");
        let mut h = Sha256::new();
        h.update(pre_fix.as_bytes());
        let digest = format!("{:x}", h.finalize());
        assert_eq!(
            digest, "1fb03cbe041201afb2f47c77f60c1504eb94d8ea14df2d53b9ffae8f45fcd373",
            "the absent case must render exactly as it did before \
             cartridge_identity_source existed (apart from the tr correction)"
        );

        // ...and the present case differs by exactly one inserted line, in
        // exactly one place: immediately after `cartridge_serial`, inside
        // `[media]` (`docs/design/volume-format-v2.md` §1.1).
        let with_source = generate_id_thunk_v2(&IdThunkV2Params {
            cartridge_identity_source: Some("mam"),
            ..params
        });
        assert_eq!(
            with_source,
            rendered.replace(
                "cartridge_serial = \"SERIAL1\"\n",
                "cartridge_serial = \"SERIAL1\"\ncartridge_identity_source = \"mam\"\n",
            ),
            "the field must be one line, directly after cartridge_serial, and change \
             nothing else"
        );
    }

    #[test]
    fn system_guide_v2_covers_front_index_seal_disclosure_and_zero_strip() {
        let s = generate_system_guide_v2("LAB01", 42);
        assert!(s.contains("Volume: LAB01"));
        assert!(s.contains("File 3"));
        assert!(s.contains("seal marker"));
        // §2 "Accepted disclosure": the size-inference line must be stated
        // plainly, not hedged away.
        assert!(s.contains("Accepted size disclosure"));
        assert!(s.contains("reveals unit boundaries"));
        // §3.3/§3.4: the zero-strip procedure and the degradation ladder.
        assert!(s.contains("## If All Else Fails"));
        assert!(s.contains("zero-strip"));
        assert!(s.contains("Total files on this tape: 42"));
    }

    #[test]
    fn restore_script_v2_is_valid_bash() {
        // T1 floor, v2: the generated emergency script must at least parse.
        let s = generate_restore_script_v2("SYN01", 20);
        let mut f = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut f, s.as_bytes()).unwrap();
        let out = std::process::Command::new("bash")
            .arg("-n")
            .arg(f.path())
            .output()
            .expect("run bash -n");
        assert!(
            out.status.success(),
            "RESTORE.sh v2 failed bash -n: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn restore_script_v2_has_all_modes_and_rung2_fallback() {
        let s = generate_restore_script_v2("VOL01", 27);
        // Block size matches tapectl's 512KB fixed block mode
        assert!(s.contains("BLOCK=524288"));
        // All four command modes (v2 adds --verify to v1's three)
        assert!(s.contains("--info)"));
        assert!(s.contains("--verify)"));
        assert!(s.contains("--find-envelope)"));
        assert!(s.contains("--restore)"));
        // Key operations
        assert!(s.contains("mt -f \"$DEVICE\" setblk"));
        assert!(s.contains("age -d -i"));
        assert!(s.contains("sha256sum"));
        assert!(s.contains("dar -x"));
        assert!(s.contains("truncate -s"));
        assert!(s.contains("head -c"));
        // Envelope is tar archive
        assert!(s.contains("tar xf"));
        assert!(s.contains("MANIFEST.toml"));
        // Exact §2.5 verdict tokens, tied to the actual assignment sites (not
        // just an incidental substring of one another).
        assert!(s.contains("verdict=\"SEALED\""));
        assert!(s.contains("verdict=\"UNSEALED\""));
        assert!(s.contains("verdict=\"DAMAGED (ends disagree)\""));
        // Rung-2 fallback: File 3 unreadable/inconsistent falls back to the
        // seal marker's embedded copy, loudly warned (sheet §3.4).
        assert!(s.contains("RUNG-2"));
        assert!(s.contains("degradation ladder"));
    }

    #[test]
    fn restore_script_v2_is_hardened() {
        let s = generate_restore_script_v2("HARD2", 20);
        // S2: every plaintext-tape value from the ID thunk is
        // integer-validated before use — v2's field set (front_index /
        // seal_marker / total_files) replaces v1's data_start/mini_index/etc.
        assert!(s.contains("require_uint()"));
        for key in ["front_index", "seal_marker", "total_files"] {
            assert!(
                s.contains(&format!("require_uint {key} ")),
                "missing require_uint for {key}"
            );
        }
        // S7: no predictable temp path; mktemp + restrictive umask instead.
        assert!(s.contains("mktemp -d"));
        assert!(s.contains("umask 077"));
        assert!(
            !s.contains("tapectl-restore-$$"),
            "predictable /tmp path leaked"
        );
    }

    #[test]
    fn v2_generators_never_mention_mini_index() {
        // The v1 mid-tape mini-index is gone in v2 (volume-format-v2.md §8);
        // its facts moved into the front index (File 3). Neither v2 generator
        // may reference it under either spelling, in any case.
        let guide = generate_system_guide_v2("MINI1", 12).to_lowercase();
        let script = generate_restore_script_v2("MINI1", 12).to_lowercase();
        for needle in ["mini-index", "mini_index"] {
            assert!(
                !guide.contains(needle),
                "v2 guide must not mention '{needle}'"
            );
            assert!(
                !script.contains(needle),
                "v2 restore script must not mention '{needle}'"
            );
        }
    }

    #[test]
    fn on_tape_recovery_text_uses_the_write_paths_block_size() {
        // Issue #121: the recovery text an heir reads off the tape must stay
        // in lockstep with the write path's actual block size
        // (`cli::volume::DEFAULT_BLOCK_SIZE`), not a hand-typed literal that
        // could silently drift if the write path's block size ever changes
        // without re-templating these generators. This doesn't replace the
        // literal-asserting tests above (`restore_script_v2_has_all_modes_
        // and_rung2_fallback`'s `BLOCK=524288`, `tenant_recovery_md_has_
        // working_dar_recipe`'s `setblk 524288`) -- it adds the tripwire: if
        // DEFAULT_BLOCK_SIZE ever changes without this text changing too,
        // this test is what catches it.
        let guide = generate_system_guide_v2("LOCK01", 20);
        let script = generate_restore_script_v2("LOCK01", 20);
        assert!(guide.contains(&format!(
            "setblk {}",
            crate::cli::volume::DEFAULT_BLOCK_SIZE
        )));
        assert!(script.contains(&format!("BLOCK={}", crate::cli::volume::DEFAULT_BLOCK_SIZE)));
    }

    /// RESTORE.sh sanitizes the tape's label before echoing it (the ID thunk is
    /// unauthenticated), and then compares it against the label baked into the
    /// script. Those two things are only safe together while `safe_str`'s
    /// allowlist is a SUPERSET of what `validate_volume_label` accepts: if a
    /// legitimate label were altered by sanitizing, it could never equal
    /// `$LABEL`, and every correct tape would raise a permanent "WRONG TAPE"
    /// warning — and now also fail the gate's heir_info assertion.
    ///
    /// This runs the real `safe_str` out of the real generated script, so
    /// widening `validate_segment` without widening the allowlist fails here.
    #[test]
    fn safe_str_never_alters_a_label_that_tapectl_would_accept() {
        use std::io::Write;
        use std::process::{Command, Stdio};

        // The widest label validate_volume_label accepts: every permitted
        // character class, at exactly the maximum length, and not starting with
        // a dash or dot.
        let mut label = String::from("Az9");
        while label.len() < 64 {
            label.push_str("._-Az9");
        }
        label.truncate(64);
        crate::naming::validate_volume_label(&label)
            .expect("test fixture must itself be a valid volume label");

        let script = generate_restore_script_v2("PROBE1", 20);
        let body = script
            .split_once("safe_str() {")
            .and_then(|(_, rest)| rest.split_once("\n}"))
            .map(|(b, _)| b)
            .expect("generated RESTORE.sh must define safe_str()");

        let mut child = Command::new("bash")
            .args(["-s", "--", &label])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn bash");
        write!(
            child.stdin.as_mut().unwrap(),
            "safe_str() {{{body}\n}}\nsafe_str \"$1\"\n"
        )
        .unwrap();
        drop(child.stdin.take());
        let out = child.wait_with_output().expect("bash ran");
        let got = String::from_utf8_lossy(&out.stdout).to_string();
        assert_eq!(
            got,
            label,
            "safe_str altered a legitimate {}-char label — every correct tape \
             would now warn WRONG TAPE. Widen the tr allowlist in RESTORE.sh to \
             cover validate_segment, or narrow validate_segment.",
            label.len()
        );
    }

    /// Issue #131: a volume can carry the SAME unit twice — two snapshot
    /// versions, two stage sets. RESTORE.sh used to collect every matching
    /// [[units]] block, concatenating both versions' slices and handing the mix
    /// to dar. When the versions had different recipients (a `tenant reassign`
    /// between them) the first slice was one the key could not open, and the
    /// failure read as "wrong key" — on the heir path, where there is no DB to
    /// fall back on.
    ///
    /// Drives the real awk out of the real generated script, so the selection
    /// rule cannot rot independently of the script that ships.
    #[test]
    fn restore_sh_picks_one_version_of_a_unit_present_twice() {
        use std::process::{Command, Stdio};

        let program = AWK_SELECT_VERSION;

        // Built through the real writer, not hand-typed: a hand-typed sample
        // would keep parsing after the writer's shape changed underneath it.
        let unit =
            |name: &str, version: i64, tape_position: i64, eb: i64, sha: &str| ManifestUnit {
                name: name.to_string(),
                uuid: format!("uuid-{name}-{version}"),
                snapshot_version: version,
                stage_set_id: version,
                dar_version: None,
                dar_command: None,
                slices: vec![ManifestSlice {
                    number: 1,
                    tape_position,
                    size_bytes: 1,
                    encrypted_bytes: eb,
                    sha256_plain: "x".repeat(64),
                    sha256_encrypted: sha.to_string(),
                }],
            };
        let manifest = generate_manifest_toml(
            "MULTI1",
            "alice",
            &[
                unit("photos", 1, 20, 111, "aaa"),
                unit("docs", 1, 21, 222, "bbb"),
                unit("photos", 2, 30, 333, "ccc"),
            ],
        );
        let dir = std::env::temp_dir().join(format!("tapectl-awk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let man = dir.join("MANIFEST.toml");
        std::fs::write(&man, manifest).unwrap();
        let prog = dir.join("sel.awk");
        std::fs::write(&prog, program).unwrap();

        let run = |unit: &str, want: &str| -> String {
            let mut c = Command::new("awk")
                .args([
                    "-v",
                    &format!("unit={unit}"),
                    "-v",
                    &format!("want={want}"),
                    "-f",
                    prog.to_str().unwrap(),
                    man.to_str().unwrap(),
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn awk");
            let _ = c.stdin.take();
            let o = c.wait_with_output().unwrap();
            String::from_utf8_lossy(&o.stdout).to_string()
        };

        // Newest version by default — NOT both versions concatenated.
        let newest = run("photos", "");
        assert_eq!(
            newest.lines().count(),
            1,
            "expected exactly one version's slices, got:\n{newest}"
        );
        assert!(newest.contains("|30|"), "expected v2's slice:\n{newest}");
        assert!(
            !newest.contains("|20|"),
            "v1's slice must not be mixed in:\n{newest}"
        );

        // An explicit older version is selectable.
        let v1 = run("photos", "1");
        assert!(v1.contains("|20|"), "expected v1's slice:\n{v1}");
        assert!(!v1.contains("|30|"), "v2 must not leak in:\n{v1}");

        // A unit present once is unaffected, and a missing version yields
        // nothing (the caller turns that into a clear error).
        assert!(run("docs", "").contains("|21|"));
        assert!(run("photos", "9").trim().is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #133: the sibling the #131 fix did not cover. `--restore` without
    /// `--unit` collects the unit names from the manifest and auto-selects
    /// when there is exactly one. A unit stored in two snapshot versions has
    /// two `[[units]]` blocks, so it was listed twice and took the "multiple
    /// units" branch — telling the heir to disambiguate between a name and
    /// itself. `do_find_envelope` suggests exactly that no-`--unit` form, so
    /// the script's own advice walked into it.
    ///
    /// The #131 test runs the version-selecting awk standalone with an
    /// explicit `unit=`, which is precisely why it never saw this.
    #[test]
    fn restore_sh_lists_a_unit_stored_twice_only_once() {
        use std::process::{Command, Stdio};

        let program = AWK_UNIT_LIST;

        // Built through the real writer, not hand-typed (see the sibling
        // #131 test above for why).
        let unit = |name: &str, version: i64| ManifestUnit {
            name: name.to_string(),
            uuid: format!("uuid-{name}-{version}"),
            snapshot_version: version,
            stage_set_id: version,
            dar_version: None,
            dar_command: None,
            slices: vec![],
        };
        let manifest = generate_manifest_toml(
            "MULTI2",
            "alice",
            &[unit("photos", 1), unit("photos", 2), unit("docs", 1)],
        );
        let dir = std::env::temp_dir().join(format!("tapectl-names-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let man = dir.join("MANIFEST.toml");
        std::fs::write(&man, manifest).unwrap();
        let prog = dir.join("names.awk");
        std::fs::write(&prog, program).unwrap();

        let out = Command::new("awk")
            .args(["-f", prog.to_str().unwrap(), man.to_str().unwrap()])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .expect("spawn awk");
        let names: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect();

        assert_eq!(
            names,
            vec!["photos".to_string(), "docs".to_string()],
            "a unit stored in two versions must be named once, in first-seen order"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #135: a `name` key outside the `[[units]]` head must not retarget the
    /// version selector. Nothing emits one today — `naming.rs` also bans the
    /// characters that would break the field split — so this guards the
    /// direction of travel, not a live defect. The failure it prevents is the
    /// worst kind: the selector silently switches units mid-block and the
    /// restore succeeds with part of the data.
    #[test]
    fn a_name_key_outside_the_units_head_does_not_retarget_the_selector() {
        use std::process::{Command, Stdio};

        let program = AWK_SELECT_VERSION;

        // A slice table carrying its own `name`, as some future field might.
        let manifest = "\
[[units]]
name = \"photos\"
snapshot_version = 1

[[units.slices]]
number = 1
name = \"decoy\"
tape_position = 20
encrypted_bytes = 111
sha256_encrypted = \"aaa\"

[[units.slices]]
number = 2
tape_position = 21
encrypted_bytes = 222
sha256_encrypted = \"bbb\"
";
        let dir = std::env::temp_dir().join(format!("tapectl-guard-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let man = dir.join("MANIFEST.toml");
        std::fs::write(&man, manifest).unwrap();
        let prog = dir.join("sel.awk");
        std::fs::write(&prog, program).unwrap();

        let out = Command::new("awk")
            .args([
                "-v",
                "unit=photos",
                "-v",
                "want=",
                "-f",
                prog.to_str().unwrap(),
                man.to_str().unwrap(),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .expect("spawn awk");
        let got = String::from_utf8_lossy(&out.stdout).to_string();

        assert_eq!(
            got.lines().count(),
            2,
            "both slices must survive a decoy name key:\n{got}"
        );
        assert!(got.contains("|20|") && got.contains("|21|"), "{got}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The generated script must be syntactically valid bash. Cheap, and it
    /// catches the trap this file keeps setting: the awk programs are
    /// single-quoted in the shell, so one apostrophe anywhere inside one —
    /// including in a comment — ends the program early and breaks the script
    /// that gets frozen onto tape. `bash -n` parses without executing.
    #[test]
    fn the_generated_script_parses_as_bash() {
        use std::io::Write;
        use std::process::{Command, Stdio};

        let script = generate_restore_script_v2("SYNTAX1", 20);
        let mut child = Command::new("bash")
            .arg("-n")
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn bash -n");
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(script.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "generated RESTORE.sh is not valid bash:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Architecture review 2026-09-11 (candidate C6): every named `awk`
    /// fragment must reassemble into the generated script byte-for-byte —
    /// if a placeholder's boundaries or a const's content were ever off by
    /// one character, this fails here instead of only as an opaque
    /// `on_tape_golden` hash mismatch.
    #[test]
    fn every_named_awk_fragment_is_present_verbatim_in_the_assembled_script() {
        let script = generate_restore_script_v2("FRAG01", 20);
        for (name, fragment) in [
            ("AWK_PARSE_FILE_LIST", AWK_PARSE_FILE_LIST),
            ("AWK_CHECK_FILE_LIST", AWK_CHECK_FILE_LIST),
            ("AWK_FIND_ENVELOPE", AWK_FIND_ENVELOPE),
            ("AWK_MANIFEST_HAS_UNIT", AWK_MANIFEST_HAS_UNIT),
            ("AWK_UNIT_LIST", AWK_UNIT_LIST),
            ("AWK_SELECT_VERSION", AWK_SELECT_VERSION),
        ] {
            assert!(
                script.contains(fragment),
                "{name} is not a verbatim substring of the assembled script"
            );
        }
    }

    /// The template substitutes each `__AWK_*__` placeholder for its named
    /// const before `__LABEL__`/`__TOTAL_FILES__`; a stray one surviving
    /// would mean a placeholder and a const's name drifted apart, and would
    /// ship an inert token straight onto tape.
    #[test]
    fn no_awk_placeholder_survives_assembly() {
        let script = generate_restore_script_v2("FRAG02", 20);
        assert!(
            !script.contains("__AWK_"),
            "an __AWK_*__ placeholder was not substituted"
        );
    }

    /// #135's near-miss: every named fragment is single-quoted in the
    /// generated shell script, so a `'` anywhere inside one — including
    /// inside an awk comment — would end the quoting early and corrupt
    /// every RESTORE.sh built from it.
    #[test]
    fn no_named_fragment_contains_an_apostrophe() {
        for (name, fragment) in [
            ("AWK_PARSE_FILE_LIST", AWK_PARSE_FILE_LIST),
            ("AWK_CHECK_FILE_LIST", AWK_CHECK_FILE_LIST),
            ("AWK_FIND_ENVELOPE", AWK_FIND_ENVELOPE),
            ("AWK_MANIFEST_HAS_UNIT", AWK_MANIFEST_HAS_UNIT),
            ("AWK_UNIT_LIST", AWK_UNIT_LIST),
            ("AWK_SELECT_VERSION", AWK_SELECT_VERSION),
        ] {
            assert!(!fragment.contains('\''), "{name} contains an apostrophe");
        }
    }

    /// #133 defects 2 and 4, at the process boundary: how the script answers
    /// argv. Both are silent-success failures, which no assertion on the
    /// script's *text* would catch, so this runs the real thing.
    ///
    /// Every path here returns before any tape I/O — the mode dispatch and the
    /// argument loop both sit ahead of it — so this needs no device.
    #[test]
    fn restore_sh_reports_bad_invocations_instead_of_exiting_quietly() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;

        let dir = std::env::temp_dir().join(format!("tapectl-argv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sh = dir.join("RESTORE.sh");
        std::fs::write(&sh, generate_restore_script_v2("ARGV1", 20)).unwrap();

        // The script's prerequisite loop refuses to run without mt/age/dar, and
        // CI has none of them — it died with "missing required tool: age"
        // before reaching the dispatch under test. Stub every required tool on
        // PATH instead of installing tape software into CI: argv handling has
        // nothing to do with those binaries, and none of the paths exercised
        // here ever runs one. `command -v` only checks for an executable file,
        // so the stubs are never executed either.
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        for tool in [
            "mt",
            "dd",
            "age",
            "dar",
            "sha256sum",
            "head",
            "truncate",
            "tar",
        ] {
            let stub = bin.join(tool);
            std::fs::write(&stub, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );

        // Invoked as `bash RESTORE.sh`, not exec'd. A script written moments
        // earlier and then exec'd hits ETXTBSY whenever a sibling test thread
        // forks while this file's write fd is still open in the parent — the
        // classic fork/exec race, and nothing to do with what is being tested.
        let run = |args: &[&str]| -> (i32, String) {
            let o = Command::new("bash")
                .arg(&sh)
                .args(args)
                .env("PATH", &path)
                .output()
                .expect("spawn script");
            let mut text = String::from_utf8_lossy(&o.stdout).to_string();
            text.push_str(&String::from_utf8_lossy(&o.stderr));
            (o.status.code().unwrap_or(-1), text)
        };

        // Bare invocation is a friendly no-op and stays successful.
        let (code, text) = run(&[]);
        assert_eq!(code, 0, "bare invocation should succeed:\n{text}");
        assert!(text.contains("--help"), "should point at --help:\n{text}");

        // A typo'd mode must NOT look like a completed restore to a wrapper.
        let (code, text) = run(&["--resore"]);
        assert_eq!(code, 2, "an unknown mode must exit 2:\n{text}");
        assert!(text.contains("Unknown mode"), "should say so:\n{text}");

        // A trailing bare flag used to fail `shift 2` under `set -e` and exit
        // 1 having printed NOTHING. The message is the whole point.
        let (code, text) = run(&["--restore", "--key"]);
        assert_ne!(code, 0, "a flag with no value must fail:\n{text}");
        assert!(
            text.contains("--key needs a value"),
            "a short flag must name itself, not exit silently:\n{text:?}"
        );

        // --info is keyless and cannot see snapshot_version; --help must send
        // the heir to the command that can (#133 defect 3).
        let (_, help) = run(&["--help"]);
        assert!(
            help.contains("run --find-envelope --key KEYFILE"),
            "--help must point version discovery at --find-envelope:\n{help}"
        );

        // #288: --help must say --key repeats, and why. An heir who does not
        // know that a rotation splits envelope and slices across two key
        // generations has no reason to try passing a second key.
        assert!(
            help.contains("--key may be repeated"),
            "--help must document repeated --key:\n{help}"
        );
        assert!(
            help.contains("key rotation"),
            "--help must say WHY --key repeats:\n{help}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Lay out a directory holding the generated script plus stub versions of
    /// every tool its prerequisite loop demands, and return the script path
    /// and a `PATH` that finds the stubs.
    ///
    /// `mt` and `dd` are not inert stubs: they touch `$TAPECTL_TEST_SENTINEL`.
    /// That is how a test can tell "refused before any tape read" from
    /// "refused after reading tape", which is the whole point of validating
    /// key paths up front.
    #[cfg(test)]
    fn stubbed_script_dir(tag: &str, label: &str) -> (std::path::PathBuf, String) {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("tapectl-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sh = dir.join("RESTORE.sh");
        std::fs::write(&sh, generate_restore_script_v2(label, 20)).unwrap();

        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        for tool in [
            "mt",
            "dd",
            "age",
            "dar",
            "sha256sum",
            "head",
            "truncate",
            "tar",
        ] {
            let body = if tool == "mt" || tool == "dd" {
                "#!/bin/sh\nif [ -n \"${TAPECTL_TEST_SENTINEL:-}\" ]; then \
                 : >\"$TAPECTL_TEST_SENTINEL\"; fi\nexit 0\n"
            } else {
                "#!/bin/sh\nexit 0\n"
            };
            let stub = bin.join(tool);
            std::fs::write(&stub, body).unwrap();
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        (dir, path)
    }

    /// Issue #288, CTO ruling 2026-09-22: `--key` repeats, and every key path
    /// is validated BEFORE the first tape read.
    ///
    /// The discriminating cases are the ORDERING ones. Old single-key code
    /// accepted `--key a --key b` too — `--find-envelope` ignored the trailing
    /// pair and `--restore` simply overwrote — so "two keys are accepted"
    /// proves nothing on its own. What the old code could not do is notice a
    /// bad path that is not the last one: it checked exactly one file.
    ///
    /// `age -d -i good.key -i missing.key` fails outright rather than skipping
    /// the bad identity, so a mistyped path has to be caught here and not
    /// minutes into a restore — hence the sentinel, and hence the positive
    /// control that proves the sentinel can fire at all.
    #[test]
    fn restore_sh_takes_repeated_keys_and_names_a_missing_one_before_reading_tape() {
        use std::process::Command;

        let (dir, path) = stubbed_script_dir("multikey", "MULTI1");
        let sh = dir.join("RESTORE.sh");
        let sentinel = dir.join("tape-was-read");

        let good = dir.join("good.age.key");
        let good2 = dir.join("good2.age.key");
        std::fs::write(&good, "AGE-SECRET-KEY-1PLACEHOLDER\n").unwrap();
        std::fs::write(&good2, "AGE-SECRET-KEY-1PLACEHOLDER2\n").unwrap();
        let missing = dir.join("typo.age.key");

        let run = |args: &[&str]| -> (i32, String, bool) {
            let _ = std::fs::remove_file(&sentinel);
            let o = Command::new("bash")
                .arg(&sh)
                .args(args)
                .env("PATH", &path)
                .env("TAPECTL_TEST_SENTINEL", &sentinel)
                .output()
                .expect("spawn script");
            let mut text = String::from_utf8_lossy(&o.stdout).to_string();
            text.push_str(&String::from_utf8_lossy(&o.stderr));
            (o.status.code().unwrap_or(-1), text, sentinel.exists())
        };

        let missing_s = missing.display().to_string();
        let good_s = good.display().to_string();
        let good2_s = good2.display().to_string();

        // --find-envelope: the bad path is the SECOND one. Old code never
        // looked at it.
        let (code, text, read) = run(&["--find-envelope", "--key", &good_s, "--key", &missing_s]);
        assert_ne!(code, 0, "a missing key file must fail:\n{text}");
        assert!(
            text.contains(&format!("key file not found: {missing_s}")),
            "the missing key must be named:\n{text}"
        );
        assert!(
            !read,
            "no tape may be read before the key paths are checked"
        );

        // --restore: the bad path is the FIRST one. Old code checked only the
        // last `--key` it saw and would have proceeded.
        let dest = dir.join("dest");
        let dest_s = dest.display().to_string();
        let (code, text, read) = run(&[
            "--restore",
            "--key",
            &missing_s,
            "--key",
            &good_s,
            "--to",
            &dest_s,
        ]);
        assert_ne!(code, 0, "a missing key file must fail:\n{text}");
        assert!(
            text.contains(&format!("key file not found: {missing_s}")),
            "the missing key must be named:\n{text}"
        );
        assert!(
            !read,
            "no tape may be read before the key paths are checked"
        );

        // Positive control: with every key present, argument parsing accepts
        // the repetition and the script goes on to touch the tape. Without
        // this, "sentinel absent" could just mean the stub never runs.
        let (_, text, read) = run(&["--find-envelope", "--key", &good_s, "--key", &good2_s]);
        assert!(
            !text.contains("key file not found"),
            "two valid keys must both pass validation:\n{text}"
        );
        assert!(
            !text.contains("usage:"),
            "repeated --key must be accepted, not rejected as usage:\n{text}"
        );
        assert!(
            read,
            "control: with valid keys the script must reach the tape:\n{text}"
        );

        let (_, text, read) = run(&[
            "--restore",
            "--key",
            &good_s,
            "--key",
            &good2_s,
            "--to",
            &dest_s,
        ]);
        assert!(
            !text.contains("key file not found") && !text.contains("usage:"),
            "--restore must accept repeated --key:\n{text}"
        );
        assert!(
            read,
            "control: with valid keys --restore must reach the tape:\n{text}"
        );

        // Zero keys is still a usage error, in both modes.
        let (code, text, _) = run(&["--find-envelope"]);
        assert_ne!(code, 0, "no --key at all must fail:\n{text}");
        assert!(
            text.contains("usage: "),
            "no --key must print usage:\n{text}"
        );
        let (code, text, _) = run(&["--restore", "--to", &dest_s]);
        assert_ne!(code, 0, "no --key at all must fail:\n{text}");
        assert!(
            text.contains("usage: "),
            "no --key must print usage:\n{text}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Issue #291 + the quoting defect: all three "no envelope opened" exits
    /// must carry the unit (where there is one), the device/label sentence and
    /// the key-rotation sentence — and must RENDER, which the template text
    /// alone cannot prove.
    ///
    /// The exits sit behind `establish_files`, which reads tape, so argv alone
    /// cannot reach them. Sourcing the script defines its functions without
    /// running a mode (with no positional args the trailing `case` takes the
    /// `""` branch, which does not exit), and `die_no_envelope` can then be
    /// called directly with the globals set the way a real failure would leave
    /// them.
    #[test]
    fn the_three_no_envelope_exits_render_unit_label_and_the_rotation_hint() {
        use std::process::Command;

        let (dir, path) = stubbed_script_dir("noenv", "GOLD01");
        let sh = dir.join("RESTORE.sh");

        let call = |snippet: &str| -> (i32, String) {
            let prog = format!(
                "source \"{}\"\nDEVICE=/dev/nst1\nTAPE_LABEL=VOL-A\nKEYS=(k1 k2 k3)\n{snippet}\n",
                sh.display()
            );
            let o = Command::new("bash")
                .arg("-c")
                .arg(&prog)
                .env("PATH", &path)
                .output()
                .expect("spawn bash -c");
            let mut text = String::from_utf8_lossy(&o.stdout).to_string();
            text.push_str(&String::from_utf8_lossy(&o.stderr));
            (o.status.code().unwrap_or(-1), text)
        };

        // The shape every one of the three must have.
        let shared = |text: &str, what: &str| {
            assert!(
                text.contains("Tape in /dev/nst1 identifies as 'VOL-A'"),
                "{what}: the device/label sentence must render with plain \
                 quotes:\n{text}"
            );
            assert!(
                text.contains("this script is for 'GOLD01'"),
                "{what}: the script's own label must render:\n{text}"
            );
            assert!(
                !text.contains("'\\''"),
                "{what}: the single-quote-inside-single-quotes idiom is \
                 literal inside a double-quoted string and must not appear \
                 in the output:\n{text}"
            );
            assert!(
                text.contains("wrong cartridge or the wrong drive"),
                "{what}: must say it may be the wrong cartridge:\n{text}"
            );
            assert!(
                text.contains("sealed with the key"),
                "{what}: must name the rotation cause (#288):\n{text}"
            );
            assert!(
                text.contains("Keys tried: k1 k2 k3"),
                "{what}: must name every key tried:\n{text}"
            );
        };

        // Exit 1 — do_find_envelope. Exit 2 — do_restore with no --unit.
        // Both are the no-unit form.
        let (code, text) = call("die_no_envelope");
        assert_ne!(code, 0, "die_no_envelope must exit non-zero:\n{text}");
        assert!(
            text.contains("no envelope matched any of the 3 keys provided"),
            "the headline must count the keys:\n{text}"
        );
        shared(&text, "no-unit form");

        // Exit 3 — do_restore WITH --unit. This is the one an heir actually
        // hits, and the one that used to be a bare one-liner (#291).
        let (code, text) = call("die_no_envelope photos");
        assert_ne!(code, 0, "die_no_envelope must exit non-zero:\n{text}");
        assert!(
            text.contains("no envelope for unit 'photos' matched any of the 3 keys provided"),
            "the --unit form must still name the unit:\n{text}"
        );
        shared(&text, "unit form");

        // A single key reads as a singular, not "any of the 1 keys".
        let prog = format!(
            "source \"{}\"\nDEVICE=/dev/nst1\nTAPE_LABEL=VOL-A\nKEYS=(only.key)\ndie_no_envelope\n",
            sh.display()
        );
        let o = Command::new("bash")
            .arg("-c")
            .arg(&prog)
            .env("PATH", &path)
            .output()
            .expect("spawn bash -c");
        let text = String::from_utf8_lossy(&o.stderr).to_string();
        assert!(
            text.contains("no envelope matched the key provided"),
            "one key must read as a singular:\n{text}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Issue #288 at the slice, executed rather than asserted on text: each
    /// key is tried in turn, the key that worked last time goes first, age's
    /// own stderr stays visible, and "no key works" still fails.
    ///
    /// The real `age` CLI is not a test dependency here (the crate uses the
    /// rage library; the surrounding tests stub `age` for exactly that
    /// reason), so the stub decides by key NAME. That is enough: what is
    /// under test is the loop, not the cryptography.
    #[test]
    fn a_slice_is_tried_against_every_key_starting_with_the_last_one_that_worked() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;

        let (dir, path) = stubbed_script_dir("slicekeys", "SLICEK");
        let sh = dir.join("RESTORE.sh");

        // An `age` that succeeds only for the key named in $AGE_GOOD, records
        // every key it was handed in $AGE_TRIED, and otherwise fails the way
        // the real one does.
        let age_stub = dir.join("bin").join("age");
        std::fs::write(
            &age_stub,
            "#!/bin/sh\nkey=\"\"\nwhile [ $# -gt 0 ]; do\n  case \"$1\" in\n  \
             -i) key=$2; shift 2 ;;\n  *) shift ;;\n  esac\ndone\n\
             echo \"$key\" >>\"$AGE_TRIED\"\nif [ \"$key\" = \"$AGE_GOOD\" ]; then\n  \
             cat\n  exit 0\nfi\n\
             echo \"age: error: no identity matched any of the recipients\" >&2\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&age_stub, std::fs::Permissions::from_mode(0o755)).unwrap();

        let d = dir.display();
        let run = |prog: String| -> (i32, String) {
            let o = Command::new("bash")
                .arg("-c")
                .arg(&prog)
                .env("PATH", &path)
                .output()
                .expect("spawn bash -c");
            let mut text = String::from_utf8_lossy(&o.stdout).to_string();
            text.push_str(&String::from_utf8_lossy(&o.stderr));
            (o.status.code().unwrap_or(-1), text)
        };

        // kB is the only key that opens anything. kA must be tried and fail
        // first; kC must never be reached; slice 2 must start at kB.
        let (code, text) = run(format!(
            "source \"{sh}\"\nKEYS=(kA kB kC)\nexport AGE_GOOD=kB\n\
             export AGE_TRIED={d}/tried\n: >\"$AGE_TRIED\"\n\
             printf payload >{d}/in\n\
             decrypt_slice {d}/in {d}/out1 1\n\
             echo TRIED1: $(cat \"$AGE_TRIED\")\n\
             : >\"$AGE_TRIED\"\n\
             decrypt_slice {d}/in {d}/out2 2\n\
             echo TRIED2: $(cat \"$AGE_TRIED\")\n\
             echo \"OUT2: $(cat {d}/out2)\"\n",
            sh = sh.display()
        ));
        assert_eq!(code, 0, "a key that works must succeed:\n{text}");
        assert!(
            text.contains("TRIED1: kA kB\n"),
            "slice 1 must try kA, then kB, and stop there:\n{text}"
        );
        assert!(
            text.contains("TRIED2: kB\n"),
            "slice 2 must start with the key that opened slice 1:\n{text}"
        );
        assert!(
            text.contains("OUT2: payload"),
            "the decrypted bytes must land in the output file:\n{text}"
        );
        assert!(
            text.contains("key kA did not decrypt slice 1"),
            "every key tried must be named, which is the heir's only \
             debugging aid:\n{text}"
        );

        // No key works: this must still fail, loudly, naming the keys and the
        // envelope-vs-slice generation fact — and age's own stderr must reach
        // the reader, since "no identity matched any of the recipients" is
        // what made #288 diagnosable in the first place.
        let (code, text) = run(format!(
            "source \"{sh}\"\nKEYS=(kA kB kC)\nexport AGE_GOOD=none\n\
             export AGE_TRIED={d}/tried2\n: >\"$AGE_TRIED\"\n\
             printf payload >{d}/in\n\
             decrypt_slice {d}/in {d}/out3 7\n",
            sh = sh.display()
        ));
        assert_ne!(code, 0, "no working key must still fail:\n{text}");
        assert!(
            text.contains("cannot decrypt slice 7"),
            "the failure must name the slice:\n{text}"
        );
        assert!(
            text.contains("Keys tried: kA kB kC"),
            "the failure must name every key tried:\n{text}"
        );
        assert!(
            text.contains("key rotation between the two"),
            "the failure must state the envelope-vs-slice fact:\n{text}"
        );
        assert!(
            text.contains("no identity matched any of the recipients"),
            "age's own stderr must not be suppressed:\n{text}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The three exits must keep going through the one helper. A future edit
    /// that inlines a message again would pass the rendering test above (which
    /// calls the helper directly) while re-opening #291, so pin the call sites
    /// and pin the absence of the old single-key wording.
    #[test]
    fn every_no_envelope_exit_routes_through_the_shared_helper() {
        let s = generate_restore_script_v2("ROUTE1", 20);
        assert!(
            s.contains("die_no_envelope() {"),
            "the shared helper must exist"
        );
        // do_find_envelope's exit, and do_restore's (one call, covering both
        // its --unit and no---unit branches via the argument).
        assert!(
            s.contains("|| die_no_envelope\n"),
            "do_find_envelope must use the helper"
        );
        assert!(
            s.contains("die_no_envelope \"$target_unit\""),
            "do_restore must use the helper and pass the unit"
        );
        assert!(
            !s.contains("matched the provided key"),
            "the old single-key wording must be gone from every exit"
        );
        // The rendering defect must not survive anywhere in the script.
        assert!(
            !s.contains("'\\''"),
            "the '\\'' idiom is literal inside a double-quoted string"
        );
    }

    /// Issue #288's other half: a slice is tried against EVERY key, not only
    /// the key that opened the envelope, and the key that worked last time is
    /// tried first so a wrong key does not cost a tape-sized re-read per
    /// slice. Plus #218's carried `MB` -> `MiB`.
    #[test]
    fn slice_decryption_tries_every_key_and_remembers_the_last_one() {
        let s = generate_restore_script_v2("SLICE1", 20);
        assert!(
            s.contains("decrypt_slice \"$SCRATCH/slice.enc\""),
            "the slice loop must go through the multi-key helper"
        );
        assert!(
            s.contains("SLICE_KEY=\"$k\""),
            "the helper must remember the key that worked"
        );
        assert!(
            !s.contains("cannot decrypt slice $num — wrong key?"),
            "the single-key slice failure must be gone"
        );
        assert!(
            s.contains("key rotation between the two puts them on"),
            "the slice failure must state the envelope-vs-slice fact"
        );
        assert!(
            s.contains("Keys tried: $(keys_list)"),
            "the slice failure must name the keys tried"
        );
        // #218, CTO ruling 2026-09-17, carried onto this re-pin: bytes/1048576
        // is binary and must be labelled binary (ADR-0012).
        assert!(
            s.contains("$((bytes / 1048576)) MiB"),
            "the decrypted-slice size must be labelled MiB"
        );
        assert!(
            !s.contains("$((bytes / 1048576)) MB"),
            "the decimal label must be gone"
        );
        // The --find-envelope hint must carry every key forward, not the last
        // one parsed.
        assert!(
            s.contains("--restore $(keys_args) --to /your/destination"),
            "the restore hint must echo all the keys"
        );
    }

    /// Find `name` on the test process's own PATH, for building a PATH that
    /// holds exactly the tools a test chooses and nothing else.
    fn host_tool(name: &str) -> std::path::PathBuf {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|d| d.join(name))
            .find(|p| p.is_file())
            .unwrap_or_else(|| panic!("test host has no `{name}` on PATH"))
    }

    /// Issue #349, CTO ruling 2026-09-28: RESTORE.sh unpacks the decrypted
    /// envelope with `tar xf -`, so an heir without `tar` must be told so up
    /// front by the prerequisite check, not meet a confusing failure after
    /// the tape has already been read and decrypted.
    ///
    /// Executed, not grepped: the script runs on a PATH holding every other
    /// tool the check demands (inert stubs, plus the real `mktemp`/`rm` the
    /// preamble uses) and no `tar`. The positive control adds a `tar` stub to
    /// the same PATH and must get past the check — without it, a failure
    /// here could just mean the stripped PATH broke something else.
    #[test]
    fn restore_sh_refuses_up_front_when_tar_is_missing() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;

        let dir = std::env::temp_dir().join(format!("tapectl-notar-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let sh = dir.join("RESTORE.sh");
        std::fs::write(&sh, generate_restore_script_v2("NOTAR1", 20)).unwrap();
        for tool in ["mt", "dd", "age", "dar", "sha256sum", "head", "truncate"] {
            let stub = bin.join(tool);
            std::fs::write(&stub, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        for tool in ["mktemp", "rm"] {
            std::os::unix::fs::symlink(host_tool(tool), bin.join(tool)).unwrap();
        }
        let bash = host_tool("bash");
        let run = || -> (i32, String) {
            let o = Command::new(&bash)
                .arg(&sh)
                .env("PATH", &bin)
                .output()
                .expect("spawn script");
            let mut text = String::from_utf8_lossy(&o.stdout).to_string();
            text.push_str(&String::from_utf8_lossy(&o.stderr));
            (o.status.code().unwrap_or(-1), text)
        };

        let (code, text) = run();
        assert_eq!(code, 1, "no tar on PATH must be a fatal exit:\n{text}");
        assert!(
            text.contains("missing required tool: tar"),
            "the prerequisite check must name tar:\n{text}"
        );

        // Positive control: the same PATH plus tar gets past the check (a
        // bare invocation is a friendly no-op that exits 0).
        let stub = bin.join("tar");
        std::fs::write(&stub, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (code, text) = run();
        assert_eq!(
            code, 0,
            "control: with tar present the check passes:\n{text}"
        );
        assert!(!text.contains("missing required tool"), "{text}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Issue #349: File 1 is where an heir learns what to install before
    /// touching the tape, so it must name `tar` wherever it lists the tools.
    #[test]
    fn the_guide_names_tar_among_the_tools() {
        let guide = generate_system_guide_v2("TOOLS1", 20);
        let intro = guide
            .split_once("All you need is:")
            .and_then(|(_, rest)| rest.split_once("\n\n"))
            .map(|(list, _)| list)
            .expect("the guide states its tool list up front");
        assert!(intro.contains("tar"), "intro tool list omits tar: {intro}");
        let section = guide
            .split_once("## Tools Required")
            .and_then(|(_, rest)| rest.split_once("\n## "))
            .map(|(body, _)| body)
            .expect("the guide has a Tools Required section");
        assert!(
            section.lines().any(|l| l.starts_with("- `tar`")),
            "Tools Required must list tar:\n{section}"
        );
    }

    // ---- the 2026-09-30 heir-path batch (ADR-0012 2026-09-30 amendment) ----

    /// A directory holding the generated script and a `bin/` of stubs that
    /// shadow the host's tools, one `(name, body)` per stub. Everything not
    /// stubbed (sha256sum, head, df, stat, awk, mktemp, ...) is the host's
    /// real tool, which these tests need: they execute the script's own
    /// helpers rather than grep for them.
    #[cfg(test)]
    fn heir_harness(tag: &str, stubs: &[(&str, &str)]) -> (std::path::PathBuf, String) {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("tapectl-heir-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(
            dir.join("RESTORE.sh"),
            generate_restore_script_v2("HEIR01", 20),
        )
        .unwrap();
        // The prerequisite loop demands these; inert unless a test says more.
        let mut all: Vec<(&str, &str)> = vec![
            ("mt", "#!/bin/sh\nexit 0\n"),
            ("dd", "#!/bin/sh\nexit 0\n"),
            ("age", "#!/bin/sh\nexit 0\n"),
            ("dar", "#!/bin/sh\nexit 0\n"),
        ];
        all.retain(|(n, _)| !stubs.iter().any(|(m, _)| m == n));
        all.extend_from_slice(stubs);
        for (name, body) in all {
            let stub = bin.join(name);
            std::fs::write(&stub, body).unwrap();
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        (dir, path)
    }

    /// Source the harness's script (defining its functions without running a
    /// mode) and run `snippet` in the same shell.
    #[cfg(test)]
    fn heir_run(dir: &std::path::Path, path: &str, snippet: &str) -> (i32, String) {
        let prog = format!(
            "source \"{}\" >/dev/null\n{snippet}\n",
            dir.join("RESTORE.sh").display()
        );
        let o = std::process::Command::new("bash")
            .arg("-c")
            .arg(&prog)
            .env("PATH", path)
            .output()
            .expect("spawn bash -c");
        let mut text = String::from_utf8_lossy(&o.stdout).to_string();
        text.push_str(&String::from_utf8_lossy(&o.stderr));
        (o.status.code().unwrap_or(-1), text)
    }

    /// A `df -Pk` stub reporting `kib` KiB available on every path.
    #[cfg(test)]
    fn df_stub(kib: u64) -> String {
        format!(
            "#!/bin/sh\necho 'Filesystem 1024-blocks Used Available Capacity Mounted on'\n\
             echo \"/dev/x 999999999999 0 {kib} 0% /\"\n"
        )
    }

    /// The space check refuses a unit that cannot fit, names the fix, and says
    /// no slice has been read; with room it passes and says how much it needs.
    /// A 470 GiB unit restored on a machine whose /tmp is RAM is the case that
    /// prompted it: the script used to die mid-slice with no message.
    #[test]
    fn restore_space_check_refuses_a_unit_that_cannot_fit() {
        let gib: u64 = 1 << 30;
        let total = 20 * gib; // two 10 GiB slices
        let largest = 10 * gib;

        // 8 GiB free, one filesystem: refused.
        let small = df_stub(8 * (1 << 20));
        let (dir, path) = heir_harness("space-small", &[("df", &small)]);
        let dest = dir.join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let call = format!(
            "check_space \"{0}\" \"{0}\" {total} {largest}",
            dest.display()
        );
        let (code, text) = heir_run(&dir, &path, &call);
        assert_ne!(code, 0, "a unit that cannot fit must be refused:\n{text}");
        assert!(text.contains("not enough disk space in"), "{text}");
        assert!(text.contains("No slice has been read yet"), "{text}");
        assert!(
            text.contains("--scratch DIR"),
            "the fix must be named:\n{text}"
        );
        assert!(
            text.contains("--no-space-check"),
            "the escape must be named:\n{text}"
        );
        // Both halves on one disk: slices + one slice in flight + the files.
        assert!(
            text.contains("it needs about 50.0 GiB, and 8.0 GiB is free"),
            "{text}"
        );

        // Positive control: the same call with room passes and reports.
        let big = df_stub(100 * (1 << 20));
        let (dir2, path2) = heir_harness("space-big", &[("df", &big)]);
        let dest2 = dir2.join("dest");
        std::fs::create_dir_all(&dest2).unwrap();
        let call = format!(
            "check_space \"{0}\" \"{0}\" {total} {largest} && echo PASSED",
            dest2.display()
        );
        let (code, text) = heir_run(&dir2, &path2, &call);
        assert_eq!(code, 0, "{text}");
        assert!(text.contains("PASSED"), "{text}");
        assert!(
            text.contains("Disk space: needs about 50.0 GiB in"),
            "{text}"
        );

        // --no-space-check skips it, even when it would refuse.
        let call = format!(
            "SKIP_SPACE_CHECK=1\ncheck_space \"{0}\" \"{0}\" {total} {largest} && echo PASSED",
            dest.display()
        );
        let (code, text) = heir_run(&dir, &path, &call);
        assert_eq!(code, 0, "{text}");
        assert!(
            text.contains("not checked (--no-space-check)") && text.contains("PASSED"),
            "{text}"
        );

        // A df that cannot answer warns and carries on: the check must never be
        // what stops an heir.
        let (dir3, path3) = heir_harness("space-nodf", &[("df", "#!/bin/sh\nexit 1\n")]);
        let call = format!("check_space /x /x {total} {largest} && echo PASSED");
        let (code, text) = heir_run(&dir3, &path3, &call);
        assert_eq!(code, 0, "{text}");
        assert!(
            text.contains("cannot measure free disk space") && text.contains("PASSED"),
            "{text}"
        );

        for d in [dir, dir2, dir3] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    /// Sizes in the space messages pick their unit: a small unit must not read
    /// as "needs about 0.0 GiB, and 0.0 GiB is free" (seen on a real tape).
    #[test]
    fn restore_space_messages_name_small_sizes_in_small_units() {
        let (dir, path) = heir_harness("sizes", &[]);
        let (code, text) = heir_run(
            &dir,
            &path,
            "size_str 5000; echo; size_str 3145728; echo; size_str 53687091200",
        );
        assert_eq!(code, 0, "{text}");
        assert_eq!(text, "4.9 KiB\n3.0 MiB\n50.0 GiB", "{text}");

        // And in the refusal itself: 64 KiB free against a one-slice 3 MiB
        // unit (slices, the slice in flight plus one block, then the files).
        let df = df_stub(64);
        let (dir2, path2) = heir_harness("sizes-refuse", &[("df", &df)]);
        let (code, text) = heir_run(&dir2, &path2, "check_space /d /d 3145728 3145728");
        assert_ne!(code, 0, "{text}");
        assert!(
            text.contains("it needs about 9.5 MiB, and 64.0 KiB is free"),
            "{text}"
        );
        for d in [dir, dir2] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    /// With --scratch on another filesystem each disk is asked only for its
    /// own half: 31 GiB free on each passes a 20 GiB unit (30 GiB of slices in
    /// flight, 20 GiB of files) that one 31 GiB disk holding both could not.
    #[test]
    fn restore_space_check_splits_the_need_across_two_filesystems() {
        let gib: u64 = 1 << 30;
        let df = df_stub(31 * (1 << 20));
        // stat reports a different device per directory name.
        let stat = "#!/bin/sh\ncase \"$*\" in *scr*) echo 11 ;; *) echo 22 ;; esac\n";
        let (dir, path) = heir_harness("space-split", &[("df", &df), ("stat", stat)]);
        let call = format!(
            "check_space /d/dest /d/scr {} {} && echo PASSED",
            20 * gib,
            10 * gib
        );
        let (code, text) = heir_run(&dir, &path, &call);
        assert_eq!(code, 0, "{text}");
        assert!(text.contains("PASSED"), "{text}");
        assert!(text.contains("in /d/scr (decrypted slices)"), "{text}");
        assert!(text.contains("in /d/dest (the files)"), "{text}");

        // Control: the same disks as ONE filesystem must refuse.
        let same = "#!/bin/sh\necho 11\n";
        let (dir2, path2) = heir_harness("space-same", &[("df", &df), ("stat", same)]);
        let (code, text) = heir_run(&dir2, &path2, &call);
        assert_ne!(code, 0, "one 31 GiB disk cannot hold both halves:\n{text}");
        for d in [dir, dir2] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    /// The check runs after the version is picked and BEFORE the first slice
    /// is read, and the slices go to SCRATCH, never to WORK (which is /tmp).
    #[test]
    fn restore_measures_space_before_the_first_slice_and_keeps_slices_out_of_tmp() {
        let s = generate_restore_script_v2("ORDER1", 20);
        let check = s
            .find("check_space \"$destdir\"")
            .expect("do_restore calls check_space");
        let picked = s.find("snapshot version ${picked:-unknown}").unwrap();
        let first_read = s.find("read_tape_raw \"$tpos\"").expect("slice read");
        assert!(
            picked < check && check < first_read,
            "order: version, space, slices"
        );
        assert!(s.contains("read_tape_raw \"$tpos\" \"$SCRATCH/slice.enc\""));
        assert!(s.contains("local dar_dir=\"$SCRATCH/dar\""));
        assert!(!s.contains("$WORK/slice.enc") && !s.contains("$WORK/dar"));
        // --verify hashes as it streams; it copies nothing to WORK.
        assert!(
            !s.contains("$WORK/chk."),
            "--verify must not copy tape files to WORK"
        );
        // The trap removes the scratch directory on every exit.
        assert!(
            s.contains("trap 'rm -rf \"$WORK\"; [ -z \"$SCRATCH\" ] || rm -rf \"$SCRATCH\"' EXIT")
        );
    }

    /// A full disk while age writes a slice is reported as a full disk, on
    /// the first key, instead of every key failing in turn and the script
    /// blaming a key rotation.
    #[test]
    fn a_full_disk_while_decrypting_is_not_reported_as_a_wrong_key() {
        let age_full = "#!/bin/sh\necho 'age: error: failed to write output: write /x: no space left on device' >&2\nexit 1\n";
        let (dir, path) = heir_harness("age-full", &[("age", age_full)]);
        let snippet = format!(
            "KEYS=(k1 k2)\n: >\"{0}/in\"\ndecrypt_slice \"{0}/in\" \"{0}/out\" 3",
            dir.display()
        );
        let (code, text) = heir_run(&dir, &path, &snippet);
        assert_ne!(code, 0, "{text}");
        assert!(
            text.contains("decrypting slice 3 failed: OUT OF DISK SPACE"),
            "{text}"
        );
        assert!(text.contains("This is not a tape or key problem"), "{text}");
        assert!(
            !text.contains("none of the"),
            "must not blame the keys:\n{text}"
        );
        assert!(
            !text.contains("key k2"),
            "a full disk must stop at the first key:\n{text}"
        );
        assert!(
            !dir.join("out").exists(),
            "the partial plaintext is removed"
        );

        // Control: a genuinely wrong key still walks every key and ends on the
        // rotation hint.
        let age_wrong =
            "#!/bin/sh\necho 'age: error: no identity matched any of the recipients' >&2\nexit 1\n";
        let (dir2, path2) = heir_harness("age-wrong", &[("age", age_wrong)]);
        let snippet = format!(
            "KEYS=(k1 k2)\n: >\"{0}/in\"\ndecrypt_slice \"{0}/in\" \"{0}/out\" 3",
            dir2.display()
        );
        let (code, text) = heir_run(&dir2, &path2, &snippet);
        assert_ne!(code, 0, "{text}");
        assert!(text.contains("key k2 did not decrypt slice 3"), "{text}");
        assert!(text.contains("none of the 2 key(s) decrypted it"), "{text}");
        assert!(
            text.contains("no identity matched"),
            "age's own stderr stays visible:\n{text}"
        );
        for d in [dir, dir2] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    /// A full disk while dd copies a slice off the tape names the cause; it
    /// used to end the script with no message at all (dd's stderr went to
    /// /dev/null under `set -e`).
    #[test]
    fn a_full_disk_while_reading_a_slice_names_the_cause() {
        let dd_full = "#!/bin/sh\necho \"dd: error writing 'x': No space left on device\" >&2\n\
                       echo '3+0 records in' >&2\necho '2+0 records out' >&2\nexit 1\n";
        let (dir, path) = heir_harness("dd-full", &[("dd", dd_full)]);
        let snippet = format!("read_tape_raw 7 \"{}/slice.enc\"", dir.display());
        let (code, text) = heir_run(&dir, &path, &snippet);
        assert_ne!(code, 0, "{text}");
        assert!(
            text.contains("reading tape file 7 failed: OUT OF DISK SPACE in"),
            "{text}"
        );
        assert!(
            text.contains("No space left on device"),
            "dd's own line is kept:\n{text}"
        );
        assert!(
            !text.contains("records in"),
            "dd's record counts are noise:\n{text}"
        );

        // Control: a dd that succeeds returns quietly.
        let (dir2, path2) = heir_harness("dd-ok", &[]);
        let snippet = format!(
            "read_tape_raw 7 \"{}/slice.enc\" && echo READ",
            dir2.display()
        );
        let (code, text) = heir_run(&dir2, &path2, &snippet);
        assert_eq!(code, 0, "{text}");
        assert!(text.contains("READ"), "{text}");
        for d in [dir, dir2] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    /// --verify's hash of a tape file: exactly the first `size` bytes, taken
    /// as the file streams off the tape, with the block padding behind it
    /// drained so dd never dies of SIGPIPE (which pipefail would report as an
    /// unreadable file). The padding here is far larger than a pipe buffer, so
    /// a missing drain fails this test.
    #[test]
    fn verify_hashes_a_tape_file_as_it_streams() {
        use sha2::{Digest, Sha256};

        let dir0 = std::env::temp_dir().join(format!("tapectl-heir-tape-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir0);
        std::fs::create_dir_all(&dir0).unwrap();
        let payload: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let mut on_tape = payload.clone();
        on_tape.resize(payload.len() + 4 * 1024 * 1024, 0); // block padding
        let tape = dir0.join("tapefile");
        std::fs::write(&tape, &on_tape).unwrap();
        let want = format!("{:x}", Sha256::digest(&payload));
        let want_padded = format!("{:x}", Sha256::digest(&on_tape));

        let dd = format!("#!/bin/sh\ncat \"{}\"\n", tape.display());
        let (dir, path) = heir_harness("stream", &[("dd", &dd)]);
        let (code, text) = heir_run(&dir, &path, &format!("hash_tape_file 5 {}", payload.len()));
        assert_eq!(code, 0, "{text}");
        assert!(
            text.contains(&want),
            "the hash of exactly size_bytes:\n{text}"
        );
        // No size: the whole file, padding included (as before).
        let (code, text) = heir_run(&dir, &path, "hash_tape_file 5 ''");
        assert_eq!(code, 0, "{text}");
        assert!(text.contains(&want_padded), "{text}");

        // Nothing off the tape, or a dd that fails, is unreadable (return 1).
        let (dir2, path2) = heir_harness("stream-empty", &[("dd", "#!/bin/sh\nexit 0\n")]);
        let (code, text) = heir_run(&dir2, &path2, "hash_tape_file 5 100 || echo UNREADABLE");
        assert_eq!(code, 0, "{text}");
        assert!(text.contains("UNREADABLE"), "{text}");
        let (dir3, path3) = heir_harness("stream-fail", &[("dd", "#!/bin/sh\nexit 1\n")]);
        let (code, text) = heir_run(&dir3, &path3, "hash_tape_file 5 100 || echo UNREADABLE");
        assert_eq!(code, 0, "{text}");
        assert!(text.contains("UNREADABLE"), "{text}");
        for d in [dir0, dir, dir2, dir3] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    /// The script reads layout v2 only: another layout is refused, before
    /// anything of it is interpreted, with the way to the tape's own script; a
    /// missing value (a damaged thunk) warns and carries on; a crafted value is
    /// sanitised before it is echoed.
    #[test]
    fn restore_sh_refuses_another_layout_and_warns_on_a_missing_one() {
        let (dir, path) = heir_harness("layout", &[]);
        let (code, text) = heir_run(&dir, &path, "check_layout_version 3");
        assert_ne!(code, 0, "{text}");
        assert!(
            text.contains("this tape is layout_version 3; this script reads layout v2 only"),
            "{text}"
        );
        assert!(
            text.contains("fsf 2") && text.contains("tr -d '\\0' > RESTORE.sh"),
            "{text}"
        );

        let (code, text) = heir_run(&dir, &path, "check_layout_version '' && echo GO");
        assert_eq!(code, 0, "{text}");
        assert!(
            text.contains("states no layout_version") && text.contains("GO"),
            "{text}"
        );

        let (code, text) = heir_run(&dir, &path, "check_layout_version 2 && echo GO");
        assert_eq!(code, 0, "{text}");
        assert_eq!(text.trim(), "GO", "a v2 tape reads silently:\n{text}");

        let (code, text) = heir_run(&dir, &path, "check_layout_version $'3\\e[2J'");
        assert_ne!(code, 0, "{text}");
        assert!(
            text.contains("(sanitized)") && !text.contains('\u{1b}'),
            "{text}"
        );

        // bootstrap_thunk runs it before the pointers it would misread.
        let s = generate_restore_script_v2("ORDER2", 20);
        let guard = s.find("check_layout_version \"$(toml_val").unwrap();
        let pointers = s.find("FRONT_INDEX=$(toml_val").unwrap();
        assert!(guard < pointers);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// --restore accepts --scratch DIR and --no-space-check, and the help and
    /// header say what the space is for.
    #[test]
    fn restore_sh_documents_scratch_and_the_space_it_needs() {
        let s = generate_restore_script_v2("DOC01", 20);
        assert!(s.contains("--scratch) scratch=$2 ;;"));
        assert!(s.contains("--no-space-check)\n      SKIP_SPACE_CHECK=1"));
        assert!(s.contains("do_restore \"$dest\" \"$unit\" \"$want\" \"$scratch\""));
        assert!(s.contains("# Requirements: mt, dd, age, dar, sha256sum, head, truncate, tar\n"));
        assert!(
            s.contains("echo \"Requirements: mt, dd, age, dar, sha256sum, head, truncate, tar\"")
        );
        assert!(s.contains("# Disk space: --restore decrypts a unit's slices to disk"));
        let guide = generate_system_guide_v2("DOC01", 20);
        assert!(guide.contains("--scratch DIR"), "the guide names --scratch");
        assert!(
            guide.contains("not in /tmp"),
            "the manual steps warn off /tmp"
        );
    }
}
