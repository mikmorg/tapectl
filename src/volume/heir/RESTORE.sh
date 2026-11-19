#!/usr/bin/env bash
# RESTORE.sh -- the heir's script for a tapectl volume of Layout 3.
# File 2 of the tape. The same bytes are on every tape: what differs between
# tapes (the label, the plan, the units) is read from the tape when this runs.
#
# It restores data from this tape WITHOUT tapectl: it needs the tape drive,
# mt, dd, age, tar and sha256sum (and gzip, xz or zstd for a unit that was
# compressed), and your age key file.
#
# Usage:
#   RESTORE.sh --info                          what this tape is (no key)
#   RESTORE.sh --verify                        read every file and check its
#                                              hash against the front index
#                                              (no key)
#   RESTORE.sh --find-envelope --key K         which envelope your key opens,
#                                              and its units
#   RESTORE.sh --list --key K [--unit U] [--version N]
#                                              a unit's files, from its file
#                                              table; no data slice is read
#   RESTORE.sh --restore --key K --to DIR --all
#   RESTORE.sh --restore --key K --to DIR --unit U [--unit U2 ...] [--version N]
#                                              whole units; with more than one
#                                              unit each goes to DIR/UNIT
#   Also: [--key K2 ...] repeats a key; --work DIR puts the work directory
#   there (the default is inside --to); --no-space-check; --overwrite (--to
#   need not be empty); --no-file-check.
#   The tape is TAPE_DEVICE in the environment, /dev/nst0 when it is not set.
#
# The tape is read forward from one rewind: every read leaves the head at the
# start of the next file, and the script rewinds only for a file behind it.
#
# A restore reads each slice, checks it against the front index and the
# manifest, decrypts it (and decompresses it, for a compressed unit) into the
# work directory, checks it again, and only then gives it to tar. One slice
# rests in the work directory at a time. A slice that is not on this tape (a
# short tape, or a unit continued on another cartridge), or fails a check, is
# left out; its files are listed and everything else is restored. Nothing is
# ever filled in with zeros. After the restore every regular file is hashed
# on disk against the unit's file table: that check, never tar's exit status,
# decides what was restored. The files that fail it, or that were left out,
# are written to .tapectl-damaged-files.txt in the restored directory.

set -euo pipefail
export LC_ALL=C
unset TAR_OPTIONS POSIXLY_CORRECT || true

DEVICE="${TAPE_DEVICE:-/dev/nst0}"
BLOCK=524288 # 512 KiB, the fixed block size of the tape

# ---- the tool table: the one list the tool checks are drawn from ----
# Each mode checks only the tools it runs, before the first read of the tape.
# mt is checked on its own (mt-st sets the block size; GNU cpio's mt cannot).
# The everyday text tools (awk, sed, grep, tr, cat, wc, ls, df, mktemp, sort)
# are not checked for.
TOOLS_READ=(dd head sha256sum)
TOOLS_ENVELOPE=(dd head sha256sum age tar)
TOOLS_LIST=(dd head sha256sum age tar)
TOOLS_RESTORE=(dd head sha256sum age tar)
# What a unit that states `compression` needs, and only such a unit:
# algorithm=tool, one per line.
DECOMPRESSORS="
gzip=gzip
xz=xz
zstd=zstd
"

MODE=""
KEYS=()
UNITS=()
ALL=0
VERSION=""
TO=""
WORK_ARG=""
NO_SPACE_CHECK=0
OVERWRITE=0
NO_FILE_CHECK=0

WORK=""
WORK_MADE_IN=""
umask 077 # decrypted plaintext is not world-readable

# ---- messages and the way out ----

die() {
  echo "FATAL: $*" >&2
  exit 1
}
warn() { echo "WARNING: $*" >&2; }
note() { echo "NOTE: $*" >&2; }
say() { printf '%s\n' "$*"; }

cleanup() {
  local rc=$?
  trap - EXIT
  if [ -n "$WORK" ] && [ -d "$WORK" ]; then
    chmod -R u+rwx "$WORK" 2>/dev/null || true
    if ! rm -rf "$WORK" 2>/dev/null; then
      # The restored root may have been given a mode that forbids it.
      chmod u+w "$WORK_MADE_IN" 2>/dev/null || true
      rm -rf "$WORK" 2>/dev/null || warn "could not remove the work directory $WORK"
    fi
  fi
  exit "$rc"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 129' HUP
trap 'exit 143' TERM

usage() {
  sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'
}

die_usage() {
  echo "FATAL: $*" >&2
  echo "       Run '$0 --help' for the modes." >&2
  exit 2
}

# Modes of the format that a later build of this script carries.
later_build() {
  die "$1 is not in this build of RESTORE.sh yet. This build restores whole units (--restore --all, --restore --unit U), lists a unit's files, and checks a tape (--info, --verify, --find-envelope)."
}

is_uint() { case ${1:-} in '' | *[!0-9]*) return 1 ;; *) return 0 ;; esac }

# Text that came off a tape, made safe to show.
safe_str() { printf '%s' "$1" | tr -c 'A-Za-z0-9 ._/:=,+-' '?' | head -c 120; }

# ---- arguments ----

need_arg() { [ "$#" -ge 2 ] || die_usage "$1 needs a value"; }
set_mode() {
  [ -z "$MODE" ] || [ "$MODE" = "$1" ] || die_usage "$1 and --$MODE are different modes: give one"
  MODE=$1
}

while [ $# -gt 0 ]; do
  case $1 in
  --info) set_mode info ;;
  --verify) set_mode verify ;;
  --find-envelope) set_mode find-envelope ;;
  --list) set_mode list ;;
  --restore) set_mode restore ;;
  --key)
    need_arg "$@"
    KEYS+=("$2")
    shift
    ;;
  --to)
    need_arg "$@"
    TO=$2
    shift
    ;;
  --unit)
    need_arg "$@"
    UNITS+=("$2")
    shift
    ;;
  --all) ALL=1 ;;
  --version)
    need_arg "$@"
    is_uint "$2" || die_usage "--version needs a whole number"
    VERSION=$((10#$2))
    shift
    ;;
  --work)
    need_arg "$@"
    WORK_ARG=$2
    shift
    ;;
  --no-space-check) NO_SPACE_CHECK=1 ;;
  --overwrite) OVERWRITE=1 ;;
  --no-file-check) NO_FILE_CHECK=1 ;;
  --path | --paths-from | --where | --find | --slices-from | --swap-tapes | --fill-from | --stream-tapes)
    later_build "$1"
    ;;
  --scratch) die_usage "--scratch is now --work DIR" ;;
  -h | --help)
    usage
    exit 0
    ;;
  *) die_usage "unknown option: $(safe_str "$1")" ;;
  esac
  shift
done

[ -n "$MODE" ] || die_usage "no mode given (--info, --verify, --find-envelope, --list or --restore)"
case $MODE in
find-envelope | list | restore) [ ${#KEYS[@]} -gt 0 ] || die_usage "--$MODE needs your age key file: --key FILE" ;;
esac
if [ "$MODE" = restore ]; then
  [ -n "$TO" ] || die_usage "--restore needs a destination: --to DIR"
fi
if [ "$ALL" = 1 ] && [ ${#UNITS[@]} -gt 0 ]; then die_usage "--all and --unit are two ways to choose: give one"; fi
case $MODE in
restore | list | find-envelope) : ;;
*) if [ ${#KEYS[@]} -gt 0 ] || [ ${#UNITS[@]} -gt 0 ] || [ -n "$TO" ]; then die_usage "--$MODE takes no --key, --unit or --to"; fi ;;
esac

# ---- the tool check, before the first tape read ----

tools_of_mode() {
  case $1 in
  info | verify) echo "${TOOLS_READ[*]}" ;;
  find-envelope) echo "${TOOLS_ENVELOPE[*]}" ;;
  list) echo "${TOOLS_LIST[*]}" ;;
  restore) echo "${TOOLS_RESTORE[*]}" ;;
  esac
}

MT=mt
require_tools() {
  local tool missing=""
  command -v mt >/dev/null 2>&1 || command -v mt-st >/dev/null 2>&1 || missing="mt (package mt-st)"
  for tool in $(tools_of_mode "$MODE"); do
    command -v "$tool" >/dev/null 2>&1 || missing="${missing:+$missing, }$tool"
  done
  [ -z "$missing" ] || die "missing required tool(s): $missing"
}
require_tools

# The decompressor of an algorithm, which a unit that states one needs.
decompressor_for() { # <algorithm>
  while IFS='=' read -r alg tool; do
    [ -n "$alg" ] || continue
    if [ "$alg" = "$1" ]; then
      echo "$tool"
      return 0
    fi
  done <<EOF
$DECOMPRESSORS
EOF
  return 1
}

require_key_files() {
  local k
  for k in "${KEYS[@]}"; do
    [ -f "$k" ] && [ -r "$k" ] || die "key file '$(safe_str "$k")' does not exist or cannot be read"
  done
}
AGE_ARGS=()
if [ ${#KEYS[@]} -gt 0 ]; then
  require_key_files
  for k in "${KEYS[@]}"; do AGE_ARGS+=(-i "$k"); done
fi

# ---- the work directory ----

die_io() { # <what failed> <text the tool printed> <directory written to>
  local what=$1 text=${2:-} dir=${3:-$WORK}
  echo "FATAL: $what: $(printf '%s' "$text" | head -n 1)" >&2
  echo "       The work directory $dir cannot take it: it is full, or not writable." >&2
  echo "       Nothing on the tape is wrong. Free space there, or give --work DIR on a larger disk" >&2
  echo "       (a directory on a file system that holds files and directories), and run the same command again." >&2
  : >"$WORK/fatal.flag" 2>/dev/null || true
  exit 1
}

is_full_text() {
  case $(printf '%s' "$1" | tr 'A-Z' 'a-z') in
  *"no space left"* | *"disk quota"* | *"read-only file system"* | *"permission denied"* | *"not permitted"* | *"cannot create"*) return 0 ;;
  esac
  return 1
}

free_kb() { # <directory>: free KiB, or nothing when df cannot say
  { df -Pk -- "$1" 2>/dev/null | awk 'NR == 2 { print $4 }'; } || true
}

fs_id() { { df -Pk -- "$1" 2>/dev/null | awk 'NR == 2 { print $1 }'; } || true; }

make_work() {
  local parent
  if [ -n "$WORK_ARG" ]; then
    parent=$WORK_ARG
  elif [ "$MODE" = restore ]; then
    parent=$TO
  else
    parent=${TMPDIR:-/tmp}
  fi
  mkdir -p -- "$parent" 2>/dev/null || die "cannot create the directory $(safe_str "$parent")"
  WORK_MADE_IN=$parent
  WORK="$(mktemp -d "$parent/.tapectl-work.XXXXXX" 2>/dev/null)" ||
    die_io "cannot make a work directory in $(safe_str "$parent")" "" "$parent"
}

# Is there room for $2 bytes in $1 (and $4 more in $3)? One message, naming
# the directory, the need, and the way out.
check_space() { # <what> <dir> <bytes> [<dir2> <bytes2>]
  [ "$NO_SPACE_CHECK" = 0 ] || return 0
  local what=$1 dir=$2 need=$3 dir2=${4:-} need2=${5:-0} free free2 f1 f2
  free=$(free_kb "$dir")
  if ! is_uint "$free"; then
    warn "cannot measure the free space of $dir (df failed); continuing without the check"
    return 0
  fi
  if [ -n "$dir2" ]; then
    f1=$(fs_id "$dir")
    f2=$(fs_id "$dir2")
    if [ -n "$f1" ] && [ "$f1" = "$f2" ]; then
      need=$((need + need2))
    else
      free2=$(free_kb "$dir2")
      if is_uint "$free2" && [ $((free2 * 1024)) -lt "$need2" ]; then
        die "$what: $dir2 has $((free2 * 1024)) bytes free and this needs about $need2. Nothing has been read from a slice yet. Free space there or choose --work DIR; --no-space-check skips this check."
      fi
    fi
  fi
  if [ $((free * 1024)) -lt "$need" ]; then
    die "$what: $dir has $((free * 1024)) bytes free and this needs about $need. Nothing has been read from a slice yet. Free space there, or choose a larger disk with --to or --work DIR; --no-space-check skips this check."
  fi
}

# ---- the tape ----

# mt-st can set the block size; GNU cpio's mt (plain `mt` on Debian without
# mt-st) cannot.
tape_init() {
  if command -v mt-st >/dev/null 2>&1; then MT=mt-st; fi
  local ver why
  ver=$("$MT" --version 2>&1 | head -n 1 || true)
  if "$MT" -f "$DEVICE" setblk "$BLOCK" 2>"$WORK/mt.err"; then
    return 0
  fi
  why=$(head -n 1 "$WORK/mt.err" 2>/dev/null || true)
  case "$ver $why" in
  *mt-st*)
    die "cannot set the block size on $DEVICE: ${why:-mt failed}
       mt-st itself refused, so the device is the problem: is $DEVICE a tape drive with a cartridge loaded?
       List the drives: ls -l /dev/tape/by-id/"
    ;;
  *"GNU cpio"* | *"invalid argument"* | *setblk*)
    warn "'$MT' is not mt-st (${ver:-unknown version}) and cannot set the block size."
    echo "         Reading $DEVICE in whatever mode the drive is in. In variable-block mode, the" >&2
    echo "         st driver's default, dd bs=512k reads each 512 KiB tape block whole and this" >&2
    echo "         works. If a read fails, install mt-st (apt install mt-st) and run again." >&2
    ;;
  *)
    die "cannot set the block size on $DEVICE with '$MT' (${ver:-unknown version}): ${why:-mt failed}
       Is $DEVICE a tape drive with a cartridge loaded? ls -l /dev/tape/by-id/"
    ;;
  esac
}

# CUR is the tape file the head sits at the START of, or empty when that is
# not known for certain. It is 0 after a rewind and pos+1 after a read that
# took file pos through its filemark; a failed mt or dd, or a read that
# stopped inside a file, empties it. seek_to moves only forward from a known
# CUR and rewinds whenever it is unknown or behind the target, so any doubt
# costs a rewind and never a read of the wrong file.
CUR=""
MT_STATUS_WARNED=0

mt_position() {
  "$MT" -f "$DEVICE" status 2>/dev/null | awk '
    { l = tolower($0) }
    l ~ /file number *=/  { s = l; sub(/.*file number *= */, "", s); sub(/[^-0-9].*/, "", s); f = s }
    l ~ /block number *=/ { s = l; sub(/.*block number *= */, "", s); sub(/[^-0-9].*/, "", s); b = s }
    END { if (f ~ /^-?[0-9][0-9]*$/ && b ~ /^-?[0-9][0-9]*$/) print f, b }
  ' || true
}

seek_to() { # <tape file>: the head at its start, or 1
  local pos=$1 from=$CUR here
  CUR=""
  if [ -n "$from" ]; then
    here=$(mt_position)
    if [ -z "$here" ]; then
      if [ "$MT_STATUS_WARNED" = 0 ]; then
        note "'$MT status' gave no file number this script can read; rewinding before every read (slower, still correct)."
        MT_STATUS_WARNED=1
      fi
      from=""
    elif [ "$here" != "$from 0" ]; then
      note "the drive reports file/block $here, not the start of file $from; rewinding to be sure."
      from=""
    fi
  fi
  if [ -n "$from" ] && [ "$pos" -eq "$from" ]; then
    :
  elif [ -n "$from" ] && [ "$pos" -gt "$from" ]; then
    "$MT" -f "$DEVICE" fsf $((pos - from)) 2>/dev/null || return 1
  else
    "$MT" -f "$DEVICE" rewind 2>/dev/null || return 1
    if [ "$pos" -gt 0 ]; then
      "$MT" -f "$DEVICE" fsf "$pos" 2>/dev/null || return 1
    fi
  fi
  CUR=$pos
}

dd_detail() {
  { grep -v -e 'records in$' -e 'records out$' -e 'bytes.*copied' "$WORK/dd.err" 2>/dev/null | tail -n 2; } || true
}

# Read tape file $1 into file $2, at most $3 bytes of it (all of it when $3 is
# empty). Returns 1 when the tape cannot give it. A work directory that
# cannot take it ends the run, saying that.
fetch() { # <tape file> <out> [<bytes>]
  local pos=$1 out=$2 size=${3:-} msg="" rc=0
  seek_to "$pos" || return 1
  CUR=""
  if [ -n "$size" ]; then
    msg=$({ dd if="$DEVICE" bs="$BLOCK" 2>"$WORK/dd.err" | {
      head -c "$size" >"$out"
      cat >/dev/null
    }; } 2>&1) || rc=$?
  else
    msg=$({ dd if="$DEVICE" bs="$BLOCK" 2>"$WORK/dd.err" | cat >"$out"; } 2>&1) || rc=$?
  fi
  # The copy is written by head, whose status the group's last command (cat)
  # hides: what head said is read from its words.
  if is_full_text "$msg $(cat "$WORK/dd.err" 2>/dev/null || true)"; then
    die_io "reading tape file $pos into the work directory failed" "$msg" "$WORK"
  fi
  if [ "$rc" -ne 0 ] || [ -n "$msg" ]; then
    note "reading tape file $pos failed: ${msg:-$(dd_detail)}"
    return 1
  fi
  [ -s "$out" ] || return 1
  CUR=$((pos + 1))
}

# sha256 of tape file $1, its first $2 bytes (all when empty), hashed as it
# streams off the tape. The hash is left in TAPE_SHA. Returns 1 when the file
# cannot be reached or yields no bytes.
EMPTY_SHA256=e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
TAPE_SHA=""
hash_tape_file() { # <tape file> [<bytes>]
  local pos=$1 size=${2:-} sum
  TAPE_SHA=""
  seek_to "$pos" || return 1
  CUR=""
  if [ -n "$size" ]; then
    sum=$(dd if="$DEVICE" bs="$BLOCK" 2>"$WORK/dd.err" | {
      head -c "$size" | sha256sum
      cat >/dev/null
    }) || {
      note "reading tape file $pos failed: $(dd_detail)"
      return 1
    }
  else
    sum=$(dd if="$DEVICE" bs="$BLOCK" 2>"$WORK/dd.err" | sha256sum) || {
      note "reading tape file $pos failed: $(dd_detail)"
      return 1
    }
  fi
  sum=${sum%% *}
  sum=${sum#\\}
  [ -n "$sum" ] && [ "$sum" != "$EMPTY_SHA256" ] || return 1
  CUR=$((pos + 1))
  TAPE_SHA=$sum
}

sha_of_file() { # <file>
  local s
  s=$(sha256sum <"$1") || return 1
  s=${s%% *}
  printf '%s' "${s#\\}"
}

# ---- text helpers ----

# The value of `key = value` in a TOML text, the first match, quotes taken off.
toml_val() { # <file> <key>
  awk -v k="$2" '
    ($1 "") == (k "") && $2 == "=" { v = $0; sub(/^[^=]*= */, "", v); sub(/^"/, "", v); sub(/"[ \t]*$/, "", v); print v; exit }
  ' "$1"
}

AWK_FILES='
/^\[\[files\]\]/ { if (p != "") print p "|" t "|" s "|" h; p = ""; t = ""; s = ""; h = "" }
/^position = /         { p = $3 }
/^type = /             { t = $3; gsub(/"/, "", t) }
/^size_bytes = /       { s = $3 }
/^sha256_encrypted = / { h = $3; gsub(/"/, "", h) }
END { if (p != "") print p "|" t "|" s "|" h }
'

# A front index (or the seal's copy of it) is a total, ordered map: positions
# 0, 1, 2 ... with no gap, one front_index entry at 3, one seal_marker entry
# and it last, and (when given) exactly `total` entries.
AWK_CONSISTENT='
BEGIN { FS = "|" }
{
  if ($1 !~ /^[0-9][0-9]*$/) bad = "a position is not a non-negative integer"
  else if (($1 + 0) != NR - 1) bad = "positions are not contiguous from 0"
  if (($2 "") == "front_index") { fi++; if (($1 + 0) != 3) bad = "the front_index entry is not at position 3" }
  if (($2 "") == "seal_marker") { sm++; smrow = NR }
}
END {
  if (NR == 0) bad = "the list is empty"
  else if (fi != 1) bad = "expected one front_index entry, found " fi + 0
  else if (sm != 1 || smrow != NR) bad = "the seal_marker entry is not the one last entry"
  else if (total != "" && NR != total + 0) bad = "it has " NR " entries and the plan has " total
  if (bad != "") { print bad > "/dev/stderr"; exit 1 }
}
'

file_field() { # <field: 2 type, 3 size, 4 hash> <position> <list file>
  awk -F'|' -v f="$1" -v p="$2" '($1 + 0) == (p + 0) && $1 != "" { print $f; exit }' "$3"
}
file_type_at() { file_field 2 "$1" "$2"; }
file_size_at() { file_field 3 "$1" "$2"; }
file_hash_at() { file_field 4 "$1" "$2"; }

# ---- File 0, the front index, the seal ----

LABEL="" UUID="" TOTAL_FILES="" FRONT_INDEX=3 SEAL_MARKER="" TAPECTL_VERSION="" MEDIA_TYPE="" CREATED_AT=""
FILES_TXT="" FILES_SOURCE=""
ENDED_AFTER="" SEAL_STATE="" SEAL_BINDS="" SEAL_FILE_COUNT="" SEAL_SEALED_AT=""
FRONT_INDEX_HASH=""

die_other_layout() { # <magic> <layout_version>
  die "this is a tapectl volume of Layout $(safe_str "${2:-unknown}") (magic $(safe_str "${1:-none}")), label $(safe_str "$LABEL"). This RESTORE.sh reads Layout 3 only.
       Read it with the tapectl that wrote it, or with the RESTORE.sh in its own File 2:
         mt -f $DEVICE rewind; mt -f $DEVICE fsf 2; dd if=$DEVICE bs=512k | tr -d '\\0' > RESTORE-OLD.sh"
}

# $1 is a TOML text with a `requires` list; the feature names it holds.
requires_of() {
  local v
  v=$(toml_val "$1" requires)
  printf '%s' "$v" | tr -d '[]",' | tr -s ' ' | sed 's/^ //; s/ $//'
}

# A feature this script does not know is refused by name; this script knows
# none (Layout 3's `requires` is empty on every tape).
check_requires() { # <what> <file>
  local feats
  feats=$(requires_of "$2")
  [ -z "$feats" ] ||
    die "$1 requires feature(s) this RESTORE.sh does not know: $(safe_str "$feats").
       Use a newer tapectl, the tapectl that wrote this tape, or the RESTORE.sh in this tape's own File 2."
}

bootstrap() {
  tape_init
  if ! fetch 0 "$WORK/file0.raw"; then
    die "cannot read File 0 from $DEVICE: is a cartridge loaded and is $DEVICE the right drive? ($(dd_detail))"
  fi
  tr -d '\0' <"$WORK/file0.raw" >"$WORK/file0.txt"
  local magic lv
  magic=$(toml_val "$WORK/file0.txt" magic)
  lv=$(toml_val "$WORK/file0.txt" layout_version)
  LABEL=$(toml_val "$WORK/file0.txt" label)
  if [ -n "$magic" ] && [ "$magic" != tapectl-volume-v3 ]; then die_other_layout "$magic" "$lv"; fi
  if [ -n "$lv" ] && [ "$lv" != 3 ]; then die_other_layout "$magic" "$lv"; fi
  if [ -z "$magic" ] || [ -z "$lv" ]; then
    warn "File 0 states no magic or no layout_version (a damaged File 0?). Going on as a Layout 3 tape; the seal marker's own claims decide."
  fi
  check_requires "File 0" "$WORK/file0.txt"
  UUID=$(toml_val "$WORK/file0.txt" uuid)
  TAPECTL_VERSION=$(toml_val "$WORK/file0.txt" tapectl_version)
  MEDIA_TYPE=$(toml_val "$WORK/file0.txt" media_type)
  CREATED_AT=$(toml_val "$WORK/file0.txt" created_at)
  FRONT_INDEX=$(toml_val "$WORK/file0.txt" front_index)
  SEAL_MARKER=$(toml_val "$WORK/file0.txt" seal_marker)
  TOTAL_FILES=$(toml_val "$WORK/file0.txt" total_files)
  is_uint "$FRONT_INDEX" || FRONT_INDEX=3
  is_uint "$SEAL_MARKER" || SEAL_MARKER=""
  is_uint "$TOTAL_FILES" || TOTAL_FILES=""
}

# Rung 1: File 3. Leaves the parsed map in $WORK/files_from_index.txt.
try_front_index() {
  fetch "$FRONT_INDEX" "$WORK/front_index.raw" || return 1
  tr -d '\0' <"$WORK/front_index.raw" >"$WORK/front_index.stripped"
  grep '^\[index\]' "$WORK/front_index.stripped" >/dev/null 2>&1 || return 1
  sed -n '/^\[index\]/,$p' "$WORK/front_index.stripped" >"$WORK/index.toml"
  awk "$AWK_FILES" "$WORK/index.toml" >"$WORK/files_from_index.txt"
  [ -s "$WORK/files_from_index.txt" ] || return 1
  awk -v total="$TOTAL_FILES" "$AWK_CONSISTENT" "$WORK/files_from_index.txt" 2>"$WORK/consistent.err" || {
    note "the front index is not consistent: $(head -n 1 "$WORK/consistent.err")"
    return 1
  }
  FRONT_INDEX_HASH=$(sha_of_file "$WORK/front_index.stripped")
}

# The seal marker: the last file before end of data (mt eod, then back over
# two filemarks; GNU mt spells end of data eom), else File 0's pointer. It is
# found by filemarks, never by READ POSITION or LOCATE. Sets SEAL_* and leaves
# the seal's copy of the map in $WORK/files_from_seal.txt on success.
read_seal() {
  local ok=0
  CUR=""
  if { "$MT" -f "$DEVICE" eod 2>/dev/null || "$MT" -f "$DEVICE" eom 2>/dev/null; } &&
    "$MT" -f "$DEVICE" bsfm 2 2>/dev/null; then
    if { dd if="$DEVICE" of="$WORK/seal.raw" bs="$BLOCK" 2>"$WORK/dd.err"; } && [ -s "$WORK/seal.raw" ]; then ok=1; fi
  fi
  if [ "$ok" = 0 ] && [ -n "$SEAL_MARKER" ]; then
    if fetch "$SEAL_MARKER" "$WORK/seal.raw"; then ok=1; fi
  fi
  CUR=""
  [ "$ok" = 1 ] || return 1
  tr -d '\0' <"$WORK/seal.raw" >"$WORK/seal.stripped"
  grep '^\[seal\]' "$WORK/seal.stripped" >/dev/null 2>&1 || return 1
  sed -n '/^\[seal\]/,$p' "$WORK/seal.stripped" >"$WORK/seal.toml"
  return 0
}

# Does the seal read as a seal, by its parse rules (S1, S2, S4, S5)? A seal
# that breaks one is no seal and the tape reads as unsealed.
seal_valid() {
  local ea fc
  ea=$(toml_val "$WORK/seal.toml" ended_after)
  fc=$(toml_val "$WORK/seal.toml" file_count)
  if ! is_uint "$ea" || ! is_uint "$fc" || [ "${#ea}" -gt 15 ] || [ "${#fc}" -gt 15 ]; then
    note "the seal marker states ended_after = $(safe_str "$ea") and file_count = $(safe_str "$fc"): not a valid seal (rule S2); the tape is read as unsealed."
    return 1
  fi
  if [ $((10#$ea + 2)) -ne $((10#$fc)) ]; then
    note "the seal marker's ended_after is not file_count - 2 (rule S1); it is not a valid seal and the tape is read as unsealed."
    return 1
  fi
  awk "$AWK_FILES" "$WORK/seal.toml" >"$WORK/files_from_seal.txt"
  if ! awk -v total="$((10#$fc))" "$AWK_CONSISTENT" "$WORK/files_from_seal.txt" 2>"$WORK/consistent.err"; then
    note "the seal marker's copy of the front index is not consistent ($(head -n 1 "$WORK/consistent.err")); the tape is read as unsealed."
    return 1
  fi
  if [ "$(file_type_at "$((10#$ea))" "$WORK/files_from_seal.txt")" != data_slice ]; then
    note "the seal marker says the tape ended after file $((10#$ea)), which is not a data slice (rule S5); the tape is read as unsealed."
    return 1
  fi
  SEAL_FILE_COUNT=$((10#$fc))
  ENDED_AFTER=$((10#$ea))
  SEAL_SEALED_AT=$(toml_val "$WORK/seal.toml" sealed_at)
}

learn_end() {
  ENDED_AFTER=""
  SEAL_STATE=unsealed
  SEAL_BINDS=""
  if ! read_seal; then
    warn "no seal marker found as the last file: the tape is read as unsealed. A planned file that is not on the tape fails when it is read."
    return 0
  fi
  check_requires "the seal marker" "$WORK/seal.toml"
  if ! seal_valid; then
    ENDED_AFTER=""
    warn "the last file is not a valid seal marker: the tape is read as unsealed."
    return 0
  fi
  local want
  want=$(toml_val "$WORK/seal.toml" front_index_sha256)
  if [ -n "$FRONT_INDEX_HASH" ]; then
    if [ "$want" = "$FRONT_INDEX_HASH" ]; then SEAL_BINDS=yes; else SEAL_BINDS=no; fi
  fi
  # A seal that does not bind File 3, or that counts more files than the plan,
  # is not trusted (volume-format-v3.md 7.2 step 3, R2): the tape reads as
  # unsealed, its ended_after is not used, and the two ends are said to disagree.
  if [ "$SEAL_BINDS" = no ]; then
    warn "the two ends of the tape disagree: the seal marker binds front_index_sha256 $want, and File 3's hash is $FRONT_INDEX_HASH. The tape is read as unsealed; the seal's ended_after is not used. Every file is still checked against File 3's hashes as it is read."
    SEAL_STATE=disagrees
    ENDED_AFTER=""
    return 0
  fi
  if [ -n "$TOTAL_FILES" ] && [ "$SEAL_FILE_COUNT" -gt "$TOTAL_FILES" ]; then
    warn "the two ends of the tape disagree: the seal marker counts $SEAL_FILE_COUNT files and the plan (File 0) has $TOTAL_FILES. The tape is read as unsealed; the seal's ended_after is not used."
    SEAL_STATE=disagrees
    SEAL_BINDS=no
    ENDED_AFTER=""
    return 0
  fi
  if [ -n "$TOTAL_FILES" ] && [ "$SEAL_FILE_COUNT" -lt "$TOTAL_FILES" ]; then
    SEAL_STATE=short
  else
    SEAL_STATE=full
  fi
}

# The files of the plan up to the end of the tape, the seal last (R3: no read
# is ever sent past it).
on_tape() { # <position>: is it on this tape?
  [ -z "$ENDED_AFTER" ] || [ "$1" -le "$ENDED_AFTER" ]
}

establish() {
  bootstrap
  if try_front_index; then
    FILES_TXT="$WORK/files_from_index.txt"
    FILES_SOURCE="front index (File $FRONT_INDEX)"
    learn_end
    return 0
  fi
  warn "the front index (file $FRONT_INDEX) is unreadable or inconsistent: falling back to the seal marker's own copy of it. The front of the tape may be damaged; treat this recovery as degraded."
  if read_seal && seal_valid; then
    check_requires "the seal marker" "$WORK/seal.toml"
    FILES_TXT="$WORK/files_from_seal.txt"
    FILES_SOURCE="the seal marker's copy of the front index"
    SEAL_STATE=$([ -n "$TOTAL_FILES" ] && [ "$SEAL_FILE_COUNT" -lt "$TOTAL_FILES" ] && echo short || echo full)
    SEAL_BINDS=""
    return 0
  fi
  die "neither the front index nor the seal marker can be read, so there is no map of the tape.
       The manual way, with no script: mt -f $DEVICE rewind; then for each file
       mt -f $DEVICE fsf N; dd if=$DEVICE bs=512k | tr -d '\\0' | less
       File 1 (mt fsf 1) is the system guide that says how to go on."
}

not_on_tape_line() {
  local first=$((ENDED_AFTER + 1)) last=$1
  printf 'NOT ON TAPE  files %d to %d  (planned; the tape ended after file %d, and its seal marker is file %d)\n' "$first" "$last" "$ENDED_AFTER" "$((ENDED_AFTER + 1))"
}

last_planned() { awk -F'|' 'END { print $1 }' "$FILES_TXT"; }

# ---- --info ----

do_info() {
  establish
  local pos type size hash last
  say "Tape: $(safe_str "$LABEL")   uuid $(safe_str "$UUID")"
  say "Layout 3, written by tapectl $(safe_str "$TAPECTL_VERSION") on $(safe_str "$MEDIA_TYPE") media, $(safe_str "$CREATED_AT")"
  say "The map is read from the $FILES_SOURCE"
  case $SEAL_STATE in
  full) say "Seal marker: present (file $((ENDED_AFTER + 1))), sealed $(safe_str "$SEAL_SEALED_AT"): the tape is complete, every planned file is on it." ;;
  short) say "Seal marker: present (file $((ENDED_AFTER + 1))), sealed $(safe_str "$SEAL_SEALED_AT"): this tape ended EARLY, after file $ENDED_AFTER. A unit with a slice after it is on this tape in part only." ;;
  disagrees) say "Seal marker: present but NOT TRUSTED: it does not agree with the front index (see the WARNING). The tape is read as unsealed; trailing files may be missing." ;;
  *) say "Seal marker: NOT FOUND. The tape is unsealed (an interrupted write) or damaged: trailing files may be missing." ;;
  esac
  case $SEAL_BINDS in
  yes) say "The seal binds the front index: its sha256 is the seal's front_index_sha256." ;;
  no) say "The seal does NOT bind the front index: the two disagree (see the WARNING)." ;;
  esac
  say "Files on the map (position, kind, bytes on tape):"
  last=$(last_planned)
  while IFS='|' read -r pos type size hash; do
    if [ -n "$ENDED_AFTER" ] && [ "$pos" -gt "$((ENDED_AFTER + 1))" ]; then break; fi
    if [ -n "$ENDED_AFTER" ] && [ "$pos" -eq "$((ENDED_AFTER + 1))" ]; then
      printf '  %4d  %-24s\n' "$pos" "seal_marker"
      break
    fi
    printf '  %4d  %-24s %s\n' "$pos" "$(safe_str "$type")" "${size:+$size bytes}"
  done <"$FILES_TXT"
  if [ -n "$ENDED_AFTER" ] && [ "$SEAL_STATE" = short ]; then
    printf '  %s\n' "$(not_on_tape_line "$last")"
  fi
  say "Next: --verify checks every file against the front index with no key; --find-envelope --key KEY finds your units."
}

# ---- --verify ----

do_verify() {
  establish
  local pos type size hash fails=0 passed=0 bound="" fi_hash last stop
  last=$(last_planned)
  stop=$last
  [ -z "$ENDED_AFTER" ] || stop=$ENDED_AFTER
  while IFS='|' read -r pos type size hash; do
    [ "$pos" -le "$stop" ] || break
    if [ "$type" = seal_marker ]; then break; fi
    if [ "$type" = front_index ]; then
      fi_hash=$FRONT_INDEX_HASH
      local want
      want=$(toml_val "$WORK/seal.toml" front_index_sha256 2>/dev/null || true)
      if [ -z "$fi_hash" ]; then
        printf 'NOTE  %4d  front_index          not read: the map above is the seal marker'"'"'s copy of it\n' "$pos"
      elif [ "$SEAL_BINDS" = no ]; then
        printf 'FAIL  %4d  front_index          the seal marker binds another hash\n' "$pos"
        fails=$((fails + 1))
      elif [ "$SEAL_BINDS" = yes ]; then
        printf 'PASS  %4d  front_index          bound by the seal marker\n' "$pos"
        passed=$((passed + 1))
      else
        printf 'PASS  %4d  front_index          (no seal to bind it; it read and is consistent)\n' "$pos"
        passed=$((passed + 1))
      fi
      continue
    fi
    if [ -z "$size" ] || [ -z "$hash" ]; then
      printf 'FAIL  %4d  %-18s the front index gives it no size or hash\n' "$pos" "$type"
      fails=$((fails + 1))
      continue
    fi
    if hash_tape_file "$pos" "$size" && [ "$TAPE_SHA" = "$hash" ]; then
      printf 'PASS  %4d  %-18s %s bytes\n' "$pos" "$type" "$size"
      passed=$((passed + 1))
    elif [ -n "$TAPE_SHA" ]; then
      printf 'FAIL  %4d  %-18s hash %s, the front index says %s\n' "$pos" "$type" "$TAPE_SHA" "$hash"
      fails=$((fails + 1))
    else
      printf 'FAIL  %4d  %-18s cannot be read from the tape\n' "$pos" "$type"
      fails=$((fails + 1))
      if [ -z "$ENDED_AFTER" ]; then
        printf 'NOTE  the tape has no seal, so its end is not known: nothing after file %d was tried.\n' "$pos"
        break
      fi
    fi
  done <"$FILES_TXT"
  if [ -n "$ENDED_AFTER" ]; then
    if [ "$SEAL_STATE" = short ]; then not_on_tape_line "$last"; fi
    if [ "$SEAL_BINDS" = no ]; then bound=" (but it does not bind File 3)"; fi
    printf 'PASS  %4d  seal_marker          %s seal%s\n' "$((ENDED_AFTER + 1))" "$SEAL_STATE" "$bound"
  else
    printf 'NOTE  there is no seal marker: this verification proves each file read, not that the tape is complete.\n'
  fi
  if [ "$fails" -eq 0 ] && [ -n "$ENDED_AFTER" ]; then
    say "VERIFY PASSED: $passed file(s) match the front index."
    return 0
  fi
  if [ "$fails" -eq 0 ]; then
    say "VERIFY INCOMPLETE: $passed file(s) match, and the tape is unsealed."
    return 1
  fi
  say "VERIFY FAILED: $fails file(s) differ from the front index or cannot be read; $passed match."
  return 1
}

# ---- envelopes ----

AWK_UNITS='
function val(s) { sub(/^[^=]*= */, "", s); sub(/^"/, "", s); sub(/"[ \t]*$/, "", s); return s }
function flush() { if (n > 0) print pos "|" n "|" name "|" ver "|" sset "|" uuid "|" comp "|" sb "|" ft "|" idm "|" ns }
/^\[\[units\]\]/ { flush(); n++; name = ver = sset = uuid = comp = sb = ft = idm = ""; ns = 0; mode = 1; next }
/^\[\[units\.slices\]\]/ { if (n > 0) { ns++; mode = 2 } ; next }
/^\[/ { mode = 0; next }
mode == 1 && /^name = /             { name = val($0) }
mode == 1 && /^uuid = /             { uuid = val($0) }
mode == 1 && /^snapshot_version = / { ver = val($0) }
mode == 1 && /^stage_set_id = /     { sset = val($0) }
mode == 1 && /^stream_bytes = /     { sb = val($0) }
mode == 1 && /^file_table = /       { ft = val($0) }
mode == 1 && /^id_map = /           { idm = val($0) }
mode == 1 && /^compression = /      { comp = val($0) }
END { flush() }
'

# The slices of the unit block numbered `want` in one manifest:
# number|position|size_bytes|encrypted_bytes|sha256_plain|sha256_encrypted|continues
AWK_SLICES='
function val(s) { sub(/^[^=]*= */, "", s); sub(/^"/, "", s); sub(/"[ \t]*$/, "", s); return s }
function emit() { if (have) print num "|" tp "|" sz "|" en "|" sp "|" se "|" co; have = 0 }
/^\[\[units\]\]/ { emit(); n++; mode = 0; next }
/^\[\[units\.slices\]\]/ { emit(); if (n == want + 0) { mode = 2; have = 1; num = tp = sz = en = sp = se = co = "" } else mode = 0; next }
/^\[/ { emit(); mode = 0; next }
mode == 2 && /^number = /           { num = val($0) }
mode == 2 && /^tape_position = /    { tp = val($0) }
mode == 2 && /^size_bytes = /       { sz = val($0) }
mode == 2 && /^encrypted_bytes = /  { en = val($0) }
mode == 2 && /^sha256_plain = /     { sp = val($0) }
mode == 2 && /^sha256_encrypted = / { se = val($0) }
mode == 2 && /^continues = /        { co = val($0) }
END { emit() }
'

envelope_list() {
  awk -F'|' '$2 == "tenant_envelope" || $2 == "operator_envelope" { print $1 "|" $2 }' "$FILES_TXT"
}

# The backup of the primary at $1: the *_backup entry with the same ciphertext
# hash (never found by position arithmetic).
backup_of() {
  local h
  h=$(file_hash_at "$1" "$FILES_TXT")
  [ -n "$h" ] || return 1
  awk -F'|' -v h="$h" '$2 ~ /_backup$/ && ($4 "") == (h "") { print $1; exit }' "$FILES_TXT"
}

# Try one envelope file. Echo-free: results in globals.
#   0  opened, extracted into $WORK/env.<primary>
#   2  not for the key given (age: no identity matched)
#   3  damaged or unreadable
try_envelope_at() { # <position of this copy> <primary position> <true bytes> <hash>
  local pos=$1 primary=$2 size=$3 hash=$4 dir="$WORK/env.$2" arc="" trc="" text
  rm -rf "$dir"
  mkdir -p "$dir" || die_io "cannot make $dir" "" "$WORK"
  if ! on_tape "$pos"; then
    note "the copy of envelope $primary at file $pos is after the end of this tape; not read."
    return 3
  fi
  if ! fetch "$pos" "$WORK/envelope.enc" "$size"; then
    note "envelope file $pos cannot be read from the tape."
    return 3
  fi
  if [ "$(sha_of_file "$WORK/envelope.enc")" != "$hash" ]; then
    note "envelope file $pos differs from the front index's hash: damaged."
    rm -f "$WORK/envelope.enc"
    return 3
  fi
  rm -f "$WORK/age.rc" "$WORK/tar.rc"
  {
    {
      rc=0
      age -d "${AGE_ARGS[@]}" <"$WORK/envelope.enc" 2>"$WORK/age.err" || rc=$?
      echo "$rc" >"$WORK/age.rc"
    } | {
      rc=0
      tar -x -f - -C "$dir" 2>"$WORK/tar.err" || rc=$?
      echo "$rc" >"$WORK/tar.rc"
    }
  } || true
  rm -f "$WORK/envelope.enc"
  arc=$(cat "$WORK/age.rc" 2>/dev/null || echo 1)
  trc=$(cat "$WORK/tar.rc" 2>/dev/null || echo 1)
  text="$(cat "$WORK/age.err" "$WORK/tar.err" 2>/dev/null || true)"
  if [ "$arc" != 0 ] && printf '%s' "$text" | grep -i 'no identity matched' >/dev/null 2>&1; then
    rm -rf "$dir"
    return 2
  fi
  if [ "$arc" = 0 ] && [ "$trc" = 0 ] && [ -f "$dir/MANIFEST.toml" ]; then return 0; fi
  if is_full_text "$text"; then
    die_io "unpacking the envelope at file $pos into the work directory failed" "$text" "$WORK"
  fi
  warn "a key matched the envelope at file $pos, but the envelope is DAMAGED: $(printf '%s' "$text" | head -n 2 | tr '\n' ' ')"
  rm -rf "$dir"
  return 3
}

DAMAGED_ENVELOPES=0
OPENED="" # positions of the primaries that opened, space separated

# Walk the primaries; open each the key opens. A copy that is damaged is
# replaced by its backup (§1.3); a backup whose primary opened is never read.
open_envelopes() {
  local list pos type size hash bpos rc
  list=$(envelope_list)
  [ -n "$list" ] || die "the front index lists no envelope: this tape has nothing a key can open."
  OPENED=""
  DAMAGED_ENVELOPES=0
  while IFS='|' read -r pos type; do
    size=$(file_size_at "$pos" "$FILES_TXT")
    hash=$(file_hash_at "$pos" "$FILES_TXT")
    rc=0
    try_envelope_at "$pos" "$pos" "$size" "$hash" || rc=$?
    if [ "$rc" = 3 ]; then
      bpos=$(backup_of "$pos" || true)
      if [ -n "$bpos" ]; then
        note "reading the backup copy of the envelope at file $pos: file $bpos."
        rc=0
        try_envelope_at "$bpos" "$pos" "$size" "$hash" || rc=$?
      fi
    fi
    case $rc in
    0) OPENED="$OPENED $pos" ;;
    3) DAMAGED_ENVELOPES=$((DAMAGED_ENVELOPES + 1)) ;;
    esac
  done <<EOF
$list
EOF
  if [ -z "$OPENED" ]; then
    if [ "$DAMAGED_ENVELOPES" -gt 0 ]; then
      die "no envelope opened with the key given, and $DAMAGED_ENVELOPES envelope(s) on this tape are DAMAGED (see the WARNING and NOTE lines above). Another copy of this volume, or the operator or escrow key, may still recover the data."
    fi
    die "no envelope on this tape ($(safe_str "$LABEL")) opens with the key given. Is it the right key, and the right tape?"
  fi
}

# Space for the envelopes, before any is unpacked.
envelope_space_check() {
  local total=0 biggest=0 pos size
  while IFS='|' read -r pos _; do
    size=$(file_size_at "$pos" "$FILES_TXT")
    is_uint "$size" || continue
    total=$((total + size))
    [ "$size" -le "$biggest" ] || biggest=$size
  done <<EOF
$(envelope_list)
EOF
  check_space "the envelopes are unpacked into the work directory $WORK, which needs room for them" "$WORK" $((total + biggest + BLOCK))
}

collect_units() {
  : >"$WORK/units.txt"
  local pos
  for pos in $OPENED; do
    awk -v pos="$pos" "$AWK_UNITS" "$WORK/env.$pos/MANIFEST.toml" >>"$WORK/units.txt"
  done
}

# ---- --find-envelope ----

do_find_envelope() {
  establish
  envelope_space_check
  open_envelopes
  local pos nunits
  for pos in $OPENED; do
    say "The key opens the envelope at file $pos ($(file_type_at "$pos" "$FILES_TXT"))."
  done
  collect_units
  say "Units in the envelope(s) your key opened (unit, version, slices, bytes of stream, compression):"
  awk -F'|' '{ printf "  %s  version %s  %s slice(s)  %s bytes%s\n", $3, $4, $11, $8, ($7 != "" ? "  compressed with " $7 : "") }' "$WORK/units.txt"
  nunits=$(awk -F'|' '$9 != "" { n[$3 ""] = 1 } END { c = 0; for (k in n) c++; print c }' "$WORK/units.txt")
  if [ "${nunits:-0}" -gt 1 ]; then
    say "To restore one, run: $0 --restore --key KEY --to DIR --unit NAME   (or --all for every unit, each in DIR/NAME)"
  else
    say "To restore, run: $0 --restore --key KEY --to DIR --all"
  fi
}

# ---- choosing units ----

# $WORK/chosen.txt: one units.txt row for each unit to restore or list.
choose_units() { # [listing]: with it, several units are not refused
  local u
  : >"$WORK/want.txt"
  for u in ${UNITS[@]+"${UNITS[@]}"}; do printf '%s\n' "$u" >>"$WORK/want.txt"; done
  awk -F'|' -v ver="$VERSION" -v nw="${#UNITS[@]}" '
    FILENAME == ARGV[1] { want[$0 ""] = 1; next }
    $9 == "" { next }
    {
      name = $3 ""
      v = $4 + 0; s = $5 + 0
      if (nw > 0 && !(name in want)) next
      if (ver != "" && v != ver + 0) next
      if (!(name in bv) || v > bv[name] || (v == bv[name] && s > bs[name])) { bv[name] = v; bs[name] = s; row[name] = $0 }
    }
    END { for (n in row) print row[n] }
  ' "$WORK/want.txt" "$WORK/units.txt" | sort -t'|' -k3,3 >"$WORK/chosen.txt"
  for u in ${UNITS[@]+"${UNITS[@]}"}; do
    if ! awk -F'|' -v n="$u" '($3 "") == (n "") { f = 1 } END { exit f ? 0 : 1 }' "$WORK/chosen.txt"; then
      echo "FATAL: unit '$(safe_str "$u")'${VERSION:+ version $VERSION} is not in the envelope(s) your key opened. Units there:" >&2
      awk -F'|' '$9 != "" { printf "         %s  version %s\n", $3, $4 }' "$WORK/units.txt" >&2
      exit 1
    fi
  done
  if [ ! -s "$WORK/chosen.txt" ]; then
    die "the envelope(s) your key opened hold no unit${VERSION:+ of version $VERSION}."
  fi
  if [ -z "${1:-}" ] && [ "$ALL" = 0 ] && [ ${#UNITS[@]} -eq 0 ] && [ "$(wc -l <"$WORK/chosen.txt")" -gt 1 ]; then
    echo "FATAL: multiple units found; specify one with --unit NAME, or --all. Units:" >&2
    awk -F'|' '{ printf "         %s  version %s\n", $3, $4 }' "$WORK/chosen.txt" >&2
    exit 1
  fi
}

# ---- --list ----

unit_table_path() { # <pos> <file_table value>
  printf '%s/env.%s/%s' "$WORK" "$1" "$2"
}

do_list() {
  establish
  envelope_space_check
  open_envelopes
  collect_units
  choose_units listing
  local pos idx name ver uuid comp sb ft idm ns table
  while IFS='|' read -r pos idx name ver _ uuid comp sb ft idm ns; do
    table=$(unit_table_path "$pos" "$ft")
    [ -f "$table" ] || die "the envelope has no file table for unit $(safe_str "$name") at $(safe_str "$ft")"
    say "Unit $(safe_str "$name")  version $ver  ($ns slice(s), $sb bytes of stream${comp:+, compressed with $comp})"
    say "  kind  slice  bytes  path"
    awk -F'\t' '/^#/ { next } { printf "  %s  %5s  %s  %s%s\n", $1, $2, $5, $7, ($1 == "h" ? "  -> " $8 : "") }' "$table"
  done <"$WORK/chosen.txt"
}

# ---- restoring whole units ----

TAR_NAME=""
TAR_FLAGS=()
TAR_LOSES=""
tar_init() {
  local v
  v=$(tar --version 2>&1 | head -n 1 || true)
  TAR_NAME=$v
  case $v in
  *"GNU tar"*)
    if tar --numeric-owner -p --xattrs --acls --version >/dev/null 2>&1; then
      TAR_FLAGS=(--numeric-owner -p --xattrs --acls)
      TAR_LOSES="owner and group only when this script runs as root; an ACL or extended attribute the destination file system will not take is reported by tar and not restored"
    elif tar --numeric-owner -p --version >/dev/null 2>&1; then
      TAR_FLAGS=(--numeric-owner -p)
      TAR_LOSES="this GNU tar has no --xattrs or --acls: no ACL and no extended attribute is restored; owner and group only as root"
    else
      TAR_LOSES="owner and group only as root; no ACL and no extended attribute"
    fi
    ;;
  *bsdtar* | *libarchive*)
    TAR_FLAGS=(-p)
    TAR_LOSES="owner and group only as root; an ACL or extended attribute the file system will not take is reported and not restored"
    ;;
  *)
    TAR_NAME="${v:-a tar that does not say its version (BusyBox?)}"
    TAR_LOSES="CONTENT ONLY: no owner or group, no ACL, no extended attribute and no sub-second time is restored, silently"
    ;;
  esac
}

# Check the destination before anything is made in it.
check_destination() {
  if [ -e "$TO" ] && [ ! -d "$TO" ]; then die "--to $(safe_str "$TO") exists and is not a directory"; fi
  if [ -d "$TO" ] && [ -n "$(ls -A -- "$TO" 2>/dev/null)" ] && [ "$OVERWRITE" = 0 ]; then
    die "--to $(safe_str "$TO") is not empty. Restore into an empty or new directory, or give --overwrite to replace files that are there (to finish a restore that stopped)."
  fi
}

# One slice per entry, 1-based: the manifest's claims for the unit chosen.
SL_POS=() SL_SIZE=() SL_ENC=() SL_SHAP=() SL_SHAE=() SL_CONT=()
NSL=0
COMP=""

load_slices() { # <pos> <idx>
  local num tp sz en sp se co
  SL_POS=() SL_SIZE=() SL_ENC=() SL_SHAP=() SL_SHAE=() SL_CONT=()
  NSL=0
  while IFS='|' read -r num tp sz en sp se co; do
    is_uint "$num" || continue
    NSL=$((NSL + 1))
    [ "$num" -eq "$NSL" ] || die "the manifest's slices are not numbered 1, 2, 3 ... in order (found $(safe_str "$num") where $NSL belongs)"
    SL_POS[$NSL]=$tp
    SL_SIZE[$NSL]=$sz
    SL_ENC[$NSL]=$en
    SL_SHAP[$NSL]=$sp
    SL_SHAE[$NSL]=$se
    SL_CONT[$NSL]=$co
  done <<EOF
$(awk -v want="$2" "$AWK_SLICES" "$WORK/env.$1/MANIFEST.toml")
EOF
}

mark_bad() { # <slice> <why>
  printf '%s|%s\n' "$1" "$2" >>"$WORK/bad.slices"
}

feeder_fatal() {
  echo "FATAL: $1" >&2
  : >"$WORK/fatal.flag" 2>/dev/null || true
  exit 1
}

# Check slice $1 from the tape and leave its plain cut in slice.plain, or mark
# it bad and return 1. A slice is checked against the front index (hash of
# the file as written), then against the manifest after decryption (length
# and hash of the cut, after decompression). Nothing the tape or age or a
# decompressor says about its own exit status is the verdict; the lengths and
# hashes are.
prepare_slice() { # <slice number>
  local k=$1 pos=${SL_POS[$1]} enc=${SL_ENC[$1]} arc="" got="" text="" why="" want_h
  rm -f "$WORK/slice.enc" "$WORK/slice.plain" "$WORK/age.rc"
  if [ -z "$pos" ]; then
    mark_bad "$k" "this tape's plan does not hold it (it is on another cartridge)"
    return 1
  fi
  if ! on_tape "$pos"; then
    mark_bad "$k" "file $pos is after the end of this tape (it ended after file $ENDED_AFTER)"
    return 1
  fi
  if [ "$(file_type_at "$pos" "$FILES_TXT")" != data_slice ] ||
    [ "$(file_size_at "$pos" "$FILES_TXT")" != "$enc" ] ||
    [ "$(file_hash_at "$pos" "$FILES_TXT")" != "${SL_SHAE[$k]}" ]; then
    mark_bad "$k" "the manifest and the front index disagree about file $pos"
    return 1
  fi
  if ! fetch "$pos" "$WORK/slice.enc" "$enc"; then
    mark_bad "$k" "the tape could not be read at file $pos"
    return 1
  fi
  want_h=${SL_SHAE[$k]}
  if [ "$(sha_of_file "$WORK/slice.enc")" != "$want_h" ]; then
    mark_bad "$k" "file $pos differs from the front index's hash"
    rm -f "$WORK/slice.enc"
    return 1
  fi
  if [ -z "$COMP" ]; then
    {
      rc=0
      age -d "${AGE_ARGS[@]}" <"$WORK/slice.enc" >"$WORK/slice.plain" 2>"$WORK/age.err" || rc=$?
      echo "$rc" >"$WORK/age.rc"
    } || true
  else
    {
      {
        rc=0
        age -d "${AGE_ARGS[@]}" <"$WORK/slice.enc" 2>"$WORK/age.err" || rc=$?
        echo "$rc" >"$WORK/age.rc"
      } | "$DECOMP" -dc >"$WORK/slice.plain" 2>"$WORK/decomp.err"
    } || true
  fi
  rm -f "$WORK/slice.enc"
  arc=$(cat "$WORK/age.rc" 2>/dev/null || echo 1)
  text="$(cat "$WORK/age.err" "$WORK/decomp.err" 2>/dev/null || true)"
  if is_full_text "$text"; then
    feeder_fatal "writing slice $k into the work directory failed: $(printf '%s' "$text" | head -n 1). The work directory $WORK is full or not writable. Nothing on the tape is wrong: free space there or give --work DIR on a larger disk."
  fi
  if [ "$arc" != 0 ]; then
    if printf '%s' "$text" | grep -i 'no identity matched' >/dev/null 2>&1; then
      why="no key given opens it (a slice can be sealed to an older key than the envelope: give each key with --key)"
    else
      why="age could not decrypt it: $(printf '%s' "$text" | head -n 1 | head -c 100)"
    fi
    mark_bad "$k" "$why"
    rm -f "$WORK/slice.plain"
    return 1
  fi
  got=$(wc -c <"$WORK/slice.plain" | tr -d ' ')
  if [ "$got" != "${SL_SIZE[$k]}" ]; then
    mark_bad "$k" "its decoded length is $got bytes and the manifest says ${SL_SIZE[$k]}${COMP:+ (decompressed with $COMP)}"
    rm -f "$WORK/slice.plain"
    return 1
  fi
  if [ "$(sha_of_file "$WORK/slice.plain")" != "${SL_SHAP[$k]}" ]; then
    mark_bad "$k" "its plain cut does not match the manifest's sha256"
    rm -f "$WORK/slice.plain"
    return 1
  fi
  return 0
}

# Give slice $1's plain cut to tar (our standard output). Returns 1 when tar
# stopped reading.
send_slice() {
  local rc=0
  cat "$WORK/slice.plain" || rc=$?
  rm -f "$WORK/slice.plain"
  return "$rc"
}

# Feed the slices from $1 on. Runs on the left of the pipe into tar. A slice
# is a run of its own, except that a long member's slices (the later ones
# state continues = true) are one run: they are fed piece by piece, each
# checked first. If a piece fails, nothing more is fed, so tar's input ends
# and that tar run ends; the run's partial file is removed, the member is
# listed, and the next tar run starts at the slice after the run.
feed_from() { # <slice number>
  local i=$1 a b k
  while [ "$i" -le "$NSL" ]; do
    a=$i
    b=$a
    while [ $((b + 1)) -le "$NSL" ] && [ "${SL_CONT[$((b + 1))]}" = true ]; do b=$((b + 1)); done
    k=$a
    while [ "$k" -le "$b" ]; do
      if prepare_slice "$k"; then
        if ! send_slice "$k"; then
          echo "gone $k" >"$WORK/feed.state"
          printf '%s\n' "$CUR" >"$WORK/feed.cur"
          return 0
        fi
      else
        local j
        if [ "$k" -eq "$a" ]; then
          # The run's first slice is bad: the member never begins.
          for ((j = a + 1; j <= b; j++)); do mark_bad "$j" "part of the long member whose first slice is bad"; done
          k=$b
        else
          for ((j = k + 1; j <= b; j++)); do mark_bad "$j" "part of a long member with a bad piece"; done
          echo "resume $((b + 1)) $a" >"$WORK/feed.state"
          printf '%s\n' "$CUR" >"$WORK/feed.cur"
          return 0
        fi
      fi
      k=$((k + 1))
    done
    i=$((b + 1))
  done
  echo "done" >"$WORK/feed.state"
  printf '%s\n' "$CUR" >"$WORK/feed.cur"
}

# Path of a table row's file as the stream names it: unescape the index's
# four escapes (backslash, tab, newline, carriage return).
unesc() { # <escaped text>; result in REPLY
  local s=$1 out=""
  while [[ $s == *\\* ]]; do
    out+=${s%%\\*}
    s=${s#*\\}
    case ${s:0:1} in
    t) out+=$'\t' ;;
    n) out+=$'\n' ;;
    r) out+=$'\r' ;;
    \\) out+='\' ;;
    *) out+='\'"${s:0:1}" ;;
    esac
    s=${s:1}
  done
  out+=$s
  REPLY=$out
}

# The paths (escaped) of the non-folder members of a table that begin in the
# slices listed in $2 (one number at the start of each line).
members_in_slices() { # <table> <bad.slices file: number|why>
  cut -d'|' -f1 "$2" >"$WORK/bad.numbers"
  awk -F'\t' '
    FILENAME == ARGV[1] { bad[$1 ""] = 1; next }
    /^#/ { next }
    ($1 != "d") && (($2 "") in bad) { print $7 }
  ' "$WORK/bad.numbers" "$1"
}

# Remove the partial file of a long member whose run broke (its row is the
# one that begins in slice $2).
remove_partial() { # <table> <first slice of the run> <dest>
  local esc
  while IFS= read -r esc; do
    unesc "$esc"
    rm -f -- "$3/$REPLY" 2>/dev/null || true
  done <<EOF
$(awk -F'\t' -v s="$2" '!/^#/ && ($2 + 0) == (s + 0) && $1 == "f" { print $7 }' "$1")
EOF
}

# Hash every regular file the table lists, on disk. Writes the escaped paths
# that are missing or differ to $3. Prints nothing; the counts are in
# CHECK_ROWS and CHECK_FAILED.
CHECK_ROWS=0
CHECK_FAILED=0
file_check() { # <table> <destination> <out: escaped paths that fail>
  local table=$1 dest=$2 out=$3 h esc path sum
  : >"$out"
  CHECK_ROWS=0
  CHECK_FAILED=0
  # Rows with no backslash in the path: one sha256sum -c over the list, read
  # with no quoting to undo. The rest are done one by one.
  awk -F'\t' '!/^#/ && ($1 == "f" || $1 == "h") && $6 ~ /^[0-9a-f]+$/ && index($7, "\\") == 0 { print $6 "  ./" $7 }' "$table" >"$WORK/check.fast"
  awk -F'\t' '!/^#/ && ($1 == "f" || $1 == "h") && $6 ~ /^[0-9a-f]+$/ && index($7, "\\") > 0 { print $6 "\t" $7 }' "$table" >"$WORK/check.slow"
  CHECK_ROWS=$(($(wc -l <"$WORK/check.fast") + $(wc -l <"$WORK/check.slow")))
  if [ -s "$WORK/check.fast" ]; then
    # A file counts as checked only when sha256sum SAID it matches: what the
    # check did not positively pass is damaged, so a sha256sum that printed
    # nothing, or too little, cannot make a restore look whole.
    (cd "$dest" && sha256sum -c "$WORK/check.fast" 2>/dev/null || true) |
      awk '/: OK$/ { s = $0; sub(/: OK$/, "", s); sub(/^\\/, "", s); sub(/^\.\//, "", s); print s }' >"$WORK/check.ok"
    awk -F'\t' '
      FILENAME == ARGV[1] { ok[$0 ""] = 1; next }
      { p = $0; sub(/^[0-9a-f]+  \.\//, "", p); if (!((p "") in ok)) print p }
    ' "$WORK/check.ok" "$WORK/check.fast" >>"$out"
  fi
  if [ -s "$WORK/check.slow" ]; then
    while IFS=$'\t' read -r h esc; do
      unesc "$esc"
      path=$REPLY
      sum=""
      if [ -f "$dest/$path" ]; then sum=$(sha_of_file "$dest/$path" 2>/dev/null || true); fi
      [ "$sum" = "$h" ] || printf '%s\n' "$esc" >>"$out"
    done <"$WORK/check.slow"
  fi
  CHECK_FAILED=$(wc -l <"$out" | tr -d ' ')
}

# Extract one tar run's stream from our standard input.
tar_extract() { # <destination>
  local rc=0
  tar -x -f - -C "$1" ${TAR_FLAGS[@]+"${TAR_FLAGS[@]}"} 2>"$WORK/tar.err" || rc=$?
  echo "$rc" >"$WORK/tar.rc"
}

DAMAGED_TOTAL=0
UNITS_DONE=0

restore_one() { # <chosen row> <destination>
  local pos idx name ver uuid comp sb ft idm ns dest=$2 table ids
  IFS='|' read -r pos idx name ver _ uuid comp sb ft idm ns <<<"$1"
  table=$(unit_table_path "$pos" "$ft")
  [ -f "$table" ] || die "the envelope has no file table for unit $(safe_str "$name") at $(safe_str "$ft")"
  COMP=$comp
  DECOMP=""
  if [ -n "$COMP" ]; then
    DECOMP=$(decompressor_for "$COMP" || true)
    [ -n "$DECOMP" ] || die "unit $(safe_str "$name") states compression '$(safe_str "$COMP")', which this RESTORE.sh does not know (it knows gzip, xz and zstd). Use a newer tapectl or RESTORE.sh."
    command -v "$DECOMP" >/dev/null 2>&1 ||
      die "unit $(safe_str "$name") was compressed with $COMP and needs the tool '$DECOMP', which is not installed (package: $DECOMP, or xz-utils for xz). Nothing has been read from a slice."
  fi
  load_slices "$pos" "$idx"
  [ "$NSL" -gt 0 ] || die "the manifest lists no slice for unit $(safe_str "$name")"
  mkdir -p -- "$dest" || die "cannot create $(safe_str "$dest")"
  # Space: the unit's files, and one slice twice (the cut, and its plain form).
  local files_bytes biggest=0 k
  files_bytes=$(awk -F'\t' '!/^#/ && $1 == "f" { s += $5 } END { printf "%.0f", s }' "$table")
  for ((k = 1; k <= NSL; k++)); do
    if is_uint "${SL_SIZE[$k]}" && [ "${SL_SIZE[$k]}" -gt "$biggest" ]; then biggest=${SL_SIZE[$k]}; fi
    if is_uint "${SL_ENC[$k]}" && [ "${SL_ENC[$k]}" -gt "$biggest" ]; then biggest=${SL_ENC[$k]}; fi
  done
  check_space "unit $(safe_str "$name") needs its files, and the work directory one slice twice" "$dest" "$files_bytes" "$WORK" $((2 * biggest + BLOCK))
  say ">>> Unit $(safe_str "$name") version $ver: $NSL slice(s), $sb bytes of stream, into $dest"
  : >"$WORK/bad.slices"
  rm -f "$WORK/fatal.flag"
  local resume=1 state="" tarrc partial_first
  while :; do
    rm -f "$WORK/feed.state" "$WORK/tar.rc" "$WORK/feed.cur"
    {
      { feed_from "$resume"; } || true
    } | tar_extract "$dest"
    if [ -e "$WORK/fatal.flag" ]; then exit 1; fi
    CUR=$(cat "$WORK/feed.cur" 2>/dev/null || true)
    state=$(cat "$WORK/feed.state" 2>/dev/null || echo "gone ?")
    tarrc=$(cat "$WORK/tar.rc" 2>/dev/null || echo "?")
    if [ "$tarrc" != 0 ]; then
      note "tar ended with status $tarrc: $(head -n 3 "$WORK/tar.err" 2>/dev/null | tr '\n' ' '). The per-file check below decides what was restored."
    fi
    case $state in
    done) break ;;
    resume*)
      # shellcheck disable=SC2086
      set -- $state
      resume=$2
      partial_first=$3
      remove_partial "$table" "$partial_first" "$dest"
      mark_bad "$partial_first" "a long file's run broke at one of its later slices"
      note "a long file's run broke at a bad slice; its partial file was removed and the file is listed. Reading on with slice $resume."
      ;;
    gone*)
      # shellcheck disable=SC2086
      set -- $state
      if [ "$tarrc" = 0 ] && [ "${2:-}" = "$NSL" ]; then break; fi
      warn "tar stopped reading at slice ${2:-?} (tar: $(head -n 2 "$WORK/tar.err" 2>/dev/null | tr '\n' ' ')); the slices from there on were not given to it."
      for ((k = ${2:-1}; k <= NSL; k++)); do mark_bad "$k" "tar stopped reading before it"; done
      break
      ;;
    *) break ;;
    esac
  done
  # What was left out, and why.
  local nbad
  nbad=$(wc -l <"$WORK/bad.slices" | tr -d ' ')
  if [ "$nbad" -gt 0 ]; then
    warn "$nbad slice(s) of unit $(safe_str "$name") were left out; their files are listed as damaged:"
    awk -F'|' '{ printf "         slice %s: %s\n", $1, $2 }' "$WORK/bad.slices" >&2
  fi
  members_in_slices "$table" "$WORK/bad.slices" >"$WORK/left.out" || true
  : >"$WORK/failed.paths"
  if [ "$NO_FILE_CHECK" = 0 ]; then
    file_check "$table" "$dest" "$WORK/failed.paths"
    if [ "$CHECK_ROWS" -eq 0 ]; then
      say "File check: the unit's table lists no regular file, so none was checked."
    elif [ "$CHECK_FAILED" -eq 0 ]; then
      say "File check: all $CHECK_ROWS regular file(s) match the sha256 in the unit's file table."
    else
      say "File check: $CHECK_FAILED of $CHECK_ROWS regular file(s) are missing or differ from the file table."
    fi
  else
    note "--no-file-check: the restored files were not checked against the file table."
  fi
  sort -u "$WORK/left.out" "$WORK/failed.paths" >"$WORK/damaged.paths"
  local ndam
  ndam=$(wc -l <"$WORK/damaged.paths" | tr -d ' ')
  local list="$dest/.tapectl-damaged-files.txt"
  if [ "$ndam" -gt 0 ]; then
    {
      printf '#tapectl-damaged-files\t1\tunit=%s\tuuid=%s\tversion=%s\n' "$name" "$uuid" "$ver"
      cat "$WORK/damaged.paths"
    } >"$list" || die_io "cannot write the damaged-file list" "" "$dest"
    say "DAMAGED: $ndam path(s) were left out or failed the check. They are listed in $list:"
    head -n 41 "$WORK/damaged.paths" | sed 's/^/  /'
    if [ "$ndam" -gt 40 ]; then say "  ... and $((ndam - 40)) more, in the list."; fi
    DAMAGED_TOTAL=$((DAMAGED_TOTAL + ndam))
  else
    rm -f -- "$list"
  fi
  ids="$WORK/env.$pos/$idm"
  if [ -n "$idm" ] && [ -f "$ids" ]; then
    say "Who the ids were (the stream carries owners by number; a restore never remaps them by name):"
    awk -F'\t' '/^#/ { next } { printf "  %s %s  %s\n", ($1 == "u" ? "user " : "group"), $2, ($3 == "" ? "(no name known)" : $3) }' "$ids"
  fi
  UNITS_DONE=$((UNITS_DONE + 1))
}

do_restore() {
  tar_init
  check_destination
  # The work directory is made once the destination is known to be usable.
  make_work_for_restore
  establish
  envelope_space_check
  open_envelopes
  collect_units
  choose_units
  local n row name dest
  n=$(wc -l <"$WORK/chosen.txt" | tr -d ' ')
  # fd 3, so that nothing a unit's restore runs can read the list from stdin.
  while IFS= read -r row <&3; do
    IFS='|' read -r _ _ name _ <<<"$row"
    if [ "$n" -eq 1 ]; then dest=$TO; else dest="$TO/$name"; fi
    restore_one "$row" "$dest"
  done 3<"$WORK/chosen.txt"
  say "The tar used: ${TAR_NAME:-unknown}. What it does not restore: $TAR_LOSES."
  if [ "$DAMAGED_TOTAL" -gt 0 ]; then
    say "RESTORE INCOMPLETE: $DAMAGED_TOTAL path(s) are damaged or left out; restore them from another copy."
    return 1
  fi
  say "RESTORE COMPLETE: $UNITS_DONE unit(s) restored into $TO."
}

make_work_for_restore() {
  mkdir -p -- "$TO" 2>/dev/null || die "cannot create --to $(safe_str "$TO")"
  make_work
}

# ---- main ----

case $MODE in
restore) do_restore ;;
*)
  make_work
  case $MODE in
  info) do_info ;;
  verify) do_verify ;;
  find-envelope) do_find_envelope ;;
  list) do_list ;;
  esac
  ;;
esac
