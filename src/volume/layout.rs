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
    } = *params;
    let seal_marker = total_files - 1;
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

To read the next file (the full recovery guide):

    mt -f /dev/nst0 setblk 524288
    mt -f /dev/nst0 fsf 1
    dd if=/dev/nst0 bs=512k | tr -d '\\0' > GUIDE.md
    less GUIDE.md

If you just read this file and the tape is already positioned
past it, read the next file directly:

    mt -f /dev/nst0 setblk 524288
    dd if=/dev/nst0 bs=512k | tr -d '\\0' > GUIDE.md

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
tape_length_meters = {mam_length}
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
head, and truncate.

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

    # Full restore to a directory
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
   against File 3's `sha256_encrypted`, decrypt with age
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
/// - `--find-envelope --key K`: trial-decrypt envelope positions (found by
///   type in the file map), as v1.
/// - `--restore --key K --to DIR [--unit U]`: slice positions/sizes come from
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
/// Tools used: mt, dd, age, dar, sha256sum, head, truncate, plus standard
/// coreutils (awk/sed/grep/tr) — no TOML collection, per the grammar contract.
pub fn generate_restore_script_v2(label: &str, total_files: i32) -> String {
    r#"#!/usr/bin/env bash
# RESTORE.sh — Emergency restore script for tapectl volume __LABEL__ (layout v2)
# This script restores data from this tape WITHOUT tapectl installed.
# It reads the tape's front index (File 3), the seal marker (last file),
# finds your encrypted envelope, decrypts each data slice, and extracts
# the dar archive to a directory.
#
# Usage:
#   ./RESTORE.sh --info                                       Show tape layout + seal verdict
#   ./RESTORE.sh --verify                                     Keyless integrity check (no key needed)
#   ./RESTORE.sh --find-envelope --key KEYFILE                Decrypt your envelope
#   ./RESTORE.sh --restore --key KEYFILE --to DIR [--unit U]  Full restore
#
# Requirements: mt, dd, age, dar, sha256sum, head, truncate
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

umask 077 # decrypted plaintext and temp files must not be world-readable
WORK="$(mktemp -d "${TMPDIR:-/tmp}/tapectl-restore.XXXXXX")" ||
  {
    echo "FATAL: cannot create temporary directory" >&2
    exit 1
  }
trap 'rm -rf "$WORK"' EXIT

die() {
  echo "FATAL: $*" >&2
  exit 1
}
info() { echo ">>> $*"; }

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

# ---- prerequisite check ----

for tool in mt dd age sha256sum dar head truncate; do
  command -v "$tool" >/dev/null 2>&1 || die "missing required tool: $tool"
done

# ---- tape helpers ----

tape_init() {
  mt -f "$DEVICE" setblk "$BLOCK" 2>/dev/null ||
    die "cannot set block size — is $DEVICE a tape device?"
}

# Read tape file at position $1 into file $2 (raw bytes, block-padded). Dies
# on failure — used once a source is already trusted (post-ladder).
read_tape_raw() {
  local pos=$1 out=$2
  mt -f "$DEVICE" rewind
  [ "$pos" -gt 0 ] && mt -f "$DEVICE" fsf "$pos"
  dd if="$DEVICE" of="$out" bs="$BLOCK" 2>/dev/null
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
  awk '
    /^\[\[files\]\]/ {
      if (p != "") print p "|" t "|" s "|" h
      p = ""; t = ""; s = ""; h = ""
    }
    /^position = /         { p = $3 }
    /^type = /             { t = $3; gsub(/"/, "", t) }
    /^size_bytes = /       { s = $3 }
    /^sha256_encrypted = / { h = $3; gsub(/"/, "", h) }
    END { if (p != "") print p "|" t "|" s "|" h }
  ' "$file"
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
  awk -F'|' -v expected="$expected_total" '
    {
      row = NR - 1
      if ($1 !~ /^[0-9]+$/)   { badpos = 1 }
      else if ($1 + 0 != row) { badcontig = 1 }
      if ($2 == "front_index") {
        fi_count++
        if ($1 + 0 != 3) { badfipos = 1 }
      }
      if ($2 == "seal_marker") { seal_count++; seal_row = NR }
      last_row = NR
    }
    END {
      if (NR == 0)              { print "empty file list" > "/dev/stderr"; exit 1 }
      if (badpos)               { print "a position value is not a non-negative integer" > "/dev/stderr"; exit 1 }
      if (badcontig)            { print "positions are not contiguous from 0" > "/dev/stderr"; exit 1 }
      if (fi_count + 0 != 1)    { print "expected exactly one front_index entry, found " fi_count+0 > "/dev/stderr"; exit 1 }
      if (badfipos)             { print "front_index entry not at position 3" > "/dev/stderr"; exit 1 }
      if (seal_count + 0 != 1)  { print "expected exactly one seal_marker entry, found " seal_count+0 > "/dev/stderr"; exit 1 }
      if (seal_row != last_row) { print "seal_marker entry is not the last file" > "/dev/stderr"; exit 1 }
      if (expected != "" && NR != expected + 0) {
        print "entry count " NR " does not match expected total " expected > "/dev/stderr"; exit 1
      }
    }
  ' "$list"
}

# Look up size_bytes / sha256_encrypted for a given tape file position.
file_size_at() { awk -F'|' -v p="$1" '$1 == p { print $3; exit }' "$2"; }
file_hash_at() { awk -F'|' -v p="$1" '$1 == p { print $4; exit }' "$2"; }

# List tenant/operator envelope positions in on-tape order (the file list is
# already position-sorted, and the format's fixed zone order already puts
# tenant envelope(s) before the operator envelope before its backup).
envelope_positions() {
  awk -F'|' '$2=="tenant_envelope" || $2=="operator_envelope" || $2=="operator_envelope_backup" { print $1 }' "$1"
}

# ---- bootstrap (File 0 — identity + pointers only in layout v2) ----

bootstrap_thunk() {
  tape_init
  info "Reading ID thunk (file 0)..."
  read_tape_text 0 "$WORK/id_thunk.txt"
  sed -n '/^\[volume\]/,$p' "$WORK/id_thunk.txt" >"$WORK/thunk.toml"

  FRONT_INDEX=$(toml_val "$WORK/thunk.toml" front_index)
  SEAL_MARKER=$(toml_val "$WORK/thunk.toml" seal_marker)
  TOTAL_FILES=$(toml_val "$WORK/thunk.toml" total_files)
  CREATED=$(toml_val "$WORK/thunk.toml" created_at)

  require_uint front_index "$FRONT_INDEX"
  require_uint seal_marker "$SEAL_MARKER"
  require_uint total_files "$TOTAL_FILES"
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
  echo "=== tapectl volume: $LABEL ==="
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

    if ! try_read_tape_raw "$pos" "$WORK/chk.raw"; then
      printf "FAIL  file %3d  %-24s  (unreadable)\n" "$pos" "$type"
      overall_ok=0
      continue
    fi

    local trimmed="$WORK/chk.raw"
    if [ -n "$size" ]; then
      require_uint "size_bytes(@$pos)" "$size"
      head -c "$size" "$WORK/chk.raw" >"$WORK/chk.trim"
      trimmed="$WORK/chk.trim"
    fi

    local actual
    actual=$(sha256sum "$trimmed" | awk '{print $1}')
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

do_find_envelope() {
  local keyfile=$1

  establish_files

  local found=0 pos
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

    rm -rf "$WORK/env" && mkdir -p "$WORK/env"
    if age -d -i "$keyfile" <"$WORK/envelope.enc" 2>/dev/null |
      tar xf - -C "$WORK/env/" 2>/dev/null; then
      found=1
      echo ""
      info "Decrypted envelope at file $pos"
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
  done < <(envelope_positions "$FILES_TXT")

  [ "$found" -eq 1 ] || die "no envelope matched the provided key"
  echo ""
  echo "To restore, run:"
  echo "  $0 --restore --key $keyfile --to /your/destination"
}

# ---- --restore ----

do_restore() {
  local keyfile=$1 destdir=$2 target_unit=$3

  mkdir -p "$destdir"

  establish_files

  # Step 1: find and decrypt envelope (same candidates as --find-envelope)
  local found=0 pos
  while IFS= read -r pos; do
    require_uint envelope_position "$pos"
    read_tape_raw "$pos" "$WORK/envelope.enc"
    local esize
    esize=$(file_size_at "$pos" "$FILES_TXT")
    if [ -n "$esize" ]; then
      require_uint "size_bytes(@$pos)" "$esize"
      [ "$esize" -gt 0 ] && truncate -s "$esize" "$WORK/envelope.enc"
    fi
    rm -rf "$WORK/env" && mkdir -p "$WORK/env"
    if age -d -i "$keyfile" <"$WORK/envelope.enc" 2>/dev/null |
      tar xf - -C "$WORK/env/" 2>/dev/null; then
      found=1
      info "Decrypted envelope at file $pos"
      break
    fi
  done < <(envelope_positions "$FILES_TXT")
  [ "$found" -eq 1 ] || die "no envelope matched the provided key"
  [ -f "$WORK/env/MANIFEST.toml" ] || die "envelope missing MANIFEST.toml"

  local manifest="$WORK/env/MANIFEST.toml"

  # Step 2: identify units in manifest
  local -a unit_names
  while IFS= read -r uname; do
    unit_names+=("$uname")
  done < <(awk '
    /^\[\[units\]\]/ { in_u = 1; next }
    in_u && /^name = / { gsub(/"/, "", $3); print $3; in_u = 0 }
    /^\[/              { in_u = 0 }
  ' "$manifest")

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
  awk -v unit="$target_unit" '
    function flush() {
      if (in_s && num != "") print num "|" tpos "|" eb "|" sha
      in_s = 0; num = ""; tpos = ""; eb = ""; sha = ""
    }
    /^\[\[units\]\]/               { in_u = 0; flush() }
    /^name = /                     { gsub(/"/, "", $3); if ($3 == unit) in_u = 1 }
    in_u && /^\[\[units\.slices\]\]/ { flush(); in_s = 1; next }
    in_s && /^number = /           { num = $3 }
    in_s && /^tape_position = /    { tpos = $3 }
    in_s && /^encrypted_bytes = /  { eb = $3 }
    in_s && /^sha256_encrypted = / { gsub(/"/, "", $3); sha = $3 }
    END { flush() }
  ' "$manifest" >"$WORK/slices.txt"

  local nslices
  nslices=$(wc -l <"$WORK/slices.txt")
  [ "$nslices" -gt 0 ] || die "no slices found for unit '$target_unit'"
  info "$nslices slice(s) to read"

  # Step 4: cross-check each slice against the front index, then verify+decrypt
  local dar_dir="$WORK/dar"
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

    read_tape_raw "$tpos" "$WORK/slice.enc"
    truncate -s "$idx_size" "$WORK/slice.enc"

    local actual
    actual=$(sha256sum "$WORK/slice.enc" | awk '{print $1}')
    if [ "$actual" != "$idx_hash" ]; then
      die "slice $num checksum MISMATCH (expected ${idx_hash:0:16}…, got ${actual:0:16}…)"
    fi
    info "  checksum verified against front index"

    age -d -i "$keyfile" <"$WORK/slice.enc" >"$dar_dir/restore.$num.dar" ||
      die "cannot decrypt slice $num — wrong key?"

    local bytes
    bytes=$(wc -c <"$dar_dir/restore.$num.dar")
    info "  decrypted ($((bytes / 1048576)) MB)"
    rm -f "$WORK/slice.enc"

  done <"$WORK/slices.txt"

  # Step 5: extract with dar
  info "Extracting archive to $destdir ..."
  dar -x "$dar_dir/restore" -R "$destdir" -O -Q ||
    die "dar extraction failed"

  rm -rf "$dar_dir"
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
  [ "${1:-}" = "--key" ] && [ -n "${2:-}" ] ||
    die "usage: $0 --find-envelope --key KEYFILE"
  [ -f "$2" ] || die "key file not found: $2"
  do_find_envelope "$2"
  ;;
--restore)
  shift
  key="" dest="" unit=""
  while [ $# -gt 0 ]; do
    case "$1" in
    --key)
      key="${2:-}"
      shift 2
      ;;
    --to)
      dest="${2:-}"
      shift 2
      ;;
    --unit)
      unit="${2:-}"
      shift 2
      ;;
    *) die "unknown option: $1" ;;
    esac
  done
  [ -n "$key" ] || die "usage: $0 --restore --key KEYFILE --to DIR [--unit U]"
  [ -n "$dest" ] || die "usage: $0 --restore --key KEYFILE --to DIR [--unit U]"
  [ -f "$key" ] || die "key file not found: $key"
  do_restore "$key" "$dest" "$unit"
  ;;
--help | -h)
  echo "RESTORE.sh — Emergency restore for tapectl volume $LABEL (layout v2)"
  echo ""
  echo "Usage:"
  echo "  $0 --info                                       Show tape layout + seal verdict"
  echo "  $0 --verify                                     Keyless integrity check"
  echo "  $0 --find-envelope --key KEYFILE                Decrypt your envelope"
  echo "  $0 --restore --key KEYFILE --to DIR [--unit U]  Full restore"
  echo ""
  echo "Environment:"
  echo "  TAPE_DEVICE   Tape device path (default: /dev/nst0)"
  echo ""
  echo "Requirements: mt, dd, age, dar, sha256sum, head, truncate"
  ;;
*)
  echo "RESTORE.sh for tapectl volume $LABEL"
  echo "Run '$0 --help' for usage."
  ;;
esac
"#
    .replace("__LABEL__", label)
    .replace("__TOTAL_FILES__", &total_files.to_string())
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
pub fn generate_manifest_toml(label: &str, tenant_name: &str, units: &[ManifestUnit]) -> String {
    let now = chrono::Utc::now().to_rfc3339();
    let mut s = format!(
        r#"[manifest]
volume = "{label}"
tenant = "{tenant_name}"
created_at = "{now}"
layout_version = 1

"#
    );

    for unit in units {
        s.push_str(&format!(
            "[[units]]\nname = \"{}\"\nuuid = \"{}\"\nsnapshot_version = {}\nstage_set_id = {}\n",
            unit.name, unit.uuid, unit.snapshot_version, unit.stage_set_id,
        ));
        if let Some(ref dar_ver) = unit.dar_version {
            s.push_str(&format!("dar_version = \"{dar_ver}\"\n"));
        }
        if let Some(ref cmd) = unit.dar_command {
            // TOML basic-string escape for the command line.
            let esc = cmd.replace('\\', "\\\\").replace('"', "\\\"");
            s.push_str(&format!("dar_command = \"{esc}\"\n"));
        }
        s.push('\n');
        for slice in &unit.slices {
            s.push_str(&format!(
                "[[units.slices]]\nnumber = {}\ntape_position = {}\nsize_bytes = {}\nencrypted_bytes = {}\nsha256_plain = \"{}\"\nsha256_encrypted = \"{}\"\n\n",
                slice.number, slice.tape_position, slice.size_bytes,
                slice.encrypted_bytes, slice.sha256_plain, slice.sha256_encrypted,
            ));
        }
    }

    s
}

/// Generate RECOVERY.md for a tenant envelope.
pub fn generate_recovery_md(label: &str, tenant_name: &str, units: &[ManifestUnit]) -> String {
    let now = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let mut s = format!(
        "# Recovery Guide for {tenant_name}\n\n\
         Volume: {label}\n\
         Date: {now}\n\n\
         This tape holds age-encrypted `dar` archives. With your age key and the\n\
         standard tools (`mt`, `dd`, `truncate`, `age`, `dar`, `sha256sum`) you can\n\
         recover your data by hand — no tapectl required. The automated `RESTORE.sh`\n\
         (tape file 2) does exactly these steps for you; use it if you can.\n\n\
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
             decrypt each slice:\n\n\
             ```bash\n\
             mt -f /dev/nst0 setblk 524288\n\n",
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
         - **wrong key** — `age` decryption silently fails with a foreign key; use the\n\
           key issued for this tenant (or the operator key, which can read every unit).\n",
    );

    s
}

pub struct ManifestUnit {
    pub name: String,
    pub uuid: String,
    pub snapshot_version: i64,
    pub stage_set_id: i64,
    pub dar_version: Option<String>,
    pub dar_command: Option<String>,
    pub slices: Vec<ManifestSlice>,
}

pub struct ManifestSlice {
    pub number: i64,
    pub tape_position: i32,
    pub size_bytes: i64,
    pub encrypted_bytes: i64,
    pub sha256_plain: String,
    pub sha256_encrypted: String,
}

#[cfg(test)]
mod tests {
    use super::*;

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
        };
        let a = generate_id_thunk_v2(&params);
        let b = generate_id_thunk_v2(&params);
        assert_eq!(
            a, b,
            "id thunk must be byte-identical across two calls with the same created_at"
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
}
