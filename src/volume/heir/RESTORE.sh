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
#   RESTORE.sh --restore --key K --to DIR --unit U [--version N] --path P [--path P2 ...]
#                                              only those files of one unit, by
#                                              its file table: the slices that
#                                              hold them are read, no others.
#                                              P is written as --list prints it:
#                                              escaped, so a backslash in a name
#                                              is typed \\, a TAB \t, a newline
#                                              \n, a carriage return \r, and a
#                                              byte that is not UTF-8 \xHH (two
#                                              lowercase hex digits)
#   RESTORE.sh --restore --key K --to DIR --paths-from LIST
#                                              the one repair: LIST is the
#                                              .tapectl-damaged-files.txt of an
#                                              earlier restore, and this tape is
#                                              another copy of the same unit and
#                                              version; only the listed paths are
#                                              restored (and overwritten)
#   RESTORE.sh --where UNIT --key K            every version of a unit and the
#   RESTORE.sh --where --all --key K           tapes that hold it; every unit
#   RESTORE.sh --find TEXT --key K             paths containing TEXT, with their
#                                              unit, version and tapes
#                                              (all three read the index this
#                                              tape carries, as of its writing)
#   Also: [--key K2 ...] repeats a key; --work DIR puts the work directory
#   there (the default is inside --to); --no-space-check; --overwrite (--to
#   need not be empty); --no-file-check.
#   The tape is TAPE_DEVICE in the environment, /dev/nst0 when it is not set.
#
# A unit may continue on another cartridge. A restore reads its slices in
# order; when the next slice is not on the cartridge in the drive it asks for
# the cartridge that holds it (after telling you to eject the one in the
# drive), waits for as long as you take, and goes on in the same tar run.
# Press ENTER once the cartridge is loaded; type done and ENTER (or end the
# input) if you do not have it: the restore goes on with what it has and
# lists the rest with the tapes that hold it; type quit and ENTER to stop.
# A restore may start on any tape of a span and takes the files that tape
# holds whole. A later run into the same --to, from another tape of the same
# unit, adds that tape's files and leaves alone those that already pass.
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
TOOLS_INDEX=(dd head sha256sum age tar)
# What a path restore or the repair adds: members are cut from a slice with it.
TOOLS_PATHS=(tail)
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
PATHS=()
PATHS_FROM=""
WHERE_UNIT=""
FIND_TEXT=""
ALL=0
VERSION=""
TO=""
WORK_ARG=""
NO_SPACE_CHECK=0
OVERWRITE=0
NO_FILE_CHECK=0

WORK=""
WORK_MADE_IN=""
# The cartridge in the drive, as far as this run knows: it is the one the run
# started on until a cartridge is taken (HOME_CART 0). A cartridge offered and
# refused changes none of this (FOREIGN_LOADED, below).
HOME_CART=1
LOADED_UUID=""
LOADED_F0SHA="" # the hash of File 0 as read, for a cartridge whose File 0 states no uuid
START_LABEL=""  # the label of the tape the run started on
# 1 while the cartridge in the drive is one a probe refused: the facts above
# and below still describe the last cartridge taken (or the one the run
# started on), which is not in the drive now (#576).
FOREIGN_LOADED=0
GAVE_UP=0        # 1 once the heir said "I do not have it" for this unit
QUIT=0           # 1 once the heir typed quit
TO_HELD_LIST=0   # 1 when --to held an earlier run's damaged-file list
TAR_FAILED=0     # 1 once tar ended non-zero under --no-file-check: no check judges it, so the run is incomplete (item 301)
DRIVE_HOLDS=""   # the slice the last "done" probe found the cartridge in the drive does hold
ABSENT_HOW=fresh # which restore says what to do about slices not in the drive: fresh, continue, path or repair
ABSENT_DEST=""
RUN_UNITS=1      # how many units this run restores
TAR_OPEN=0       # 1 while tar's input (fd 4) is open
FEEDING=0        # the slice being prepared or fed
INFLIGHT_DEST="" # where, and from which table, a signal's cleanup looks for a partial member
INFLIGHT_TABLE=""
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
  # Order: tar's input is closed and tar ends, a member it had begun is
  # removed (the member being fed when a signal came), then the work directory.
  tar_close 300 || true
  scrub_inflight || true
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

# Spellings of Layout 2's script that Layout 3 does not have (item 255).
removed_option() {
  die_usage "$1 is not in this RESTORE.sh: Layout 3 has one repair, --paths-from LIST (LIST is the .tapectl-damaged-files.txt of an earlier restore, given on a tape that holds the same unit and version), and a unit across cartridges asks for the next cartridge at its own prompt."
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
  --where)
    need_arg "$@"
    set_mode where
    WHERE_UNIT=$2
    shift
    ;;
  --find)
    need_arg "$@"
    set_mode find
    FIND_TEXT=$2
    shift
    ;;
  --path)
    need_arg "$@"
    PATHS+=("$2")
    shift
    ;;
  --paths-from)
    need_arg "$@"
    PATHS_FROM=$2
    shift
    ;;
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
  --slices-from | --swap-tapes | --fill-from | --stream-tapes)
    removed_option "$1"
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

# --to and --work as absolute paths, once: the per-file check runs inside the
# destination (cd), where a path as typed (./out) would name something else.
# Only a path not already absolute is prefixed, with the directory the script
# was started in, so that no tool needs to resolve a directory that does not
# exist yet (#575).
absolute_path() { # <path>: the path, from the root
  case $1 in
  /*) REPLY=$1 ;;
  *) REPLY="$PWD/$1" ;;
  esac
}
if [ -n "$TO" ]; then
  absolute_path "$TO"
  TO=$REPLY
fi
if [ -n "$WORK_ARG" ]; then
  absolute_path "$WORK_ARG"
  WORK_ARG=$REPLY
fi

[ -n "$MODE" ] || die_usage "no mode given (--info, --verify, --find-envelope, --list, --restore, --where or --find)"
case $MODE in
find-envelope | list | restore | where | find) [ ${#KEYS[@]} -gt 0 ] || die_usage "--$MODE needs your age key file: --key FILE" ;;
esac
if [ "$MODE" = find ] && [ -z "$FIND_TEXT" ]; then die_usage "--find needs the text to look for"; fi
if [ "$MODE" = where ] && [ -z "$WHERE_UNIT" ]; then die_usage "--where needs a unit name, or --all"; fi
if [ "$MODE" != restore ] && { [ ${#PATHS[@]} -gt 0 ] || [ -n "$PATHS_FROM" ]; }; then
  die_usage "--path and --paths-from go with --restore"
fi
if [ "$MODE" = restore ] && [ ${#PATHS[@]} -gt 0 ] && [ -n "$PATHS_FROM" ]; then
  die_usage "--path and --paths-from are two ways to name paths: give one"
fi
if [ ${#PATHS[@]} -gt 0 ]; then
  [ "$ALL" = 0 ] && [ ${#UNITS[@]} -eq 1 ] || die_usage "--path names files of ONE unit: give exactly one --unit NAME (and not --all)"
fi
if [ -n "$PATHS_FROM" ]; then
  [ "$ALL" = 0 ] && [ ${#UNITS[@]} -le 1 ] || die_usage "--paths-from restores the paths of ONE unit, the one its list names: give no --all and at most that --unit"
fi
if [ "$MODE" = restore ]; then
  [ -n "$TO" ] || die_usage "--restore needs a destination: --to DIR"
fi
if [ "$ALL" = 1 ] && [ ${#UNITS[@]} -gt 0 ]; then die_usage "--all and --unit are two ways to choose: give one"; fi
case $MODE in
restore | list | find-envelope | where | find) : ;;
*) if [ ${#KEYS[@]} -gt 0 ] || [ ${#UNITS[@]} -gt 0 ] || [ -n "$TO" ]; then die_usage "--$MODE takes no --key, --unit or --to"; fi ;;
esac

# ---- the tool check, before the first tape read ----

tools_of_mode() {
  case $1 in
  info | verify) echo "${TOOLS_READ[*]}" ;;
  find-envelope) echo "${TOOLS_ENVELOPE[*]}" ;;
  where | find) echo "${TOOLS_INDEX[*]}" ;;
  list) echo "${TOOLS_LIST[*]}" ;;
  restore)
    echo "${TOOLS_RESTORE[*]}"
    if [ ${#PATHS[@]} -gt 0 ] || [ -n "$PATHS_FROM" ]; then echo "${TOOLS_PATHS[*]}"; fi
    ;;
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

# Does age's text say the file is not sealed to the key given? Go age says
# "no identity matched any of the file's recipients"; rage (the Rust age
# crate) says "No matching keys found". Anything else, after a key matched,
# is damage (#578).
not_a_recipient() { # <age's words>
  printf '%s' "$1" | grep -i -e 'no identity matched' -e 'no matching keys found' >/dev/null 2>&1
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
  file0_fields
}

# The facts File 0 states, from $WORK/file0.txt.
file0_fields() {
  LABEL=$(toml_val "$WORK/file0.txt" label)
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
# The most a seal read takes of the last tape file: 64 MiB, 128 blocks. A seal is
# a short header and one entry of about 170 bytes for each tape file, and
# tapectl holds it to 16 MiB (volume-format-v3.md 5.2 W8: a slice and the seal
# fit in half the room past the early warning; 98,000 entries); the cap is four
# times that. The last file of an unsealed tape is its last data slice, up to
# GiBs, and no more of it than this is read.
SEAL_MAX_BLOCKS=128
SEAL_MAX_BYTES=$((SEAL_MAX_BLOCKS * BLOCK))

read_seal() {
  local ok=0
  CUR=""
  rm -f "$WORK/seal.raw" "$WORK/seal.stripped"
  if { "$MT" -f "$DEVICE" eod 2>/dev/null || "$MT" -f "$DEVICE" eom 2>/dev/null; } &&
    "$MT" -f "$DEVICE" bsfm 2 2>/dev/null; then
    # What is read is stripped of its zero padding as it comes: one copy, of at
    # most the cap.
    if dd if="$DEVICE" bs="$BLOCK" count="$SEAL_MAX_BLOCKS" 2>"$WORK/dd.err" | tr -d '\0' >"$WORK/seal.stripped" &&
      [ -s "$WORK/seal.stripped" ]; then ok=1; fi
  fi
  if [ "$ok" = 0 ] && [ -n "$SEAL_MARKER" ]; then
    if fetch "$SEAL_MARKER" "$WORK/seal.raw" "$SEAL_MAX_BYTES"; then
      tr -d '\0' <"$WORK/seal.raw" >"$WORK/seal.stripped"
      rm -f "$WORK/seal.raw"
      ok=1
    fi
  fi
  CUR=""
  [ "$ok" = 1 ] || return 1
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
  [ -n "$LOADED_UUID" ] || LOADED_UUID=$UUID
  # A File 0 with no readable uuid cannot be recognised by it again: its bytes
  # as read now are what the same cartridge gives later.
  LOADED_F0SHA=$(sha_of_file "$WORK/file0.raw")
  START_LABEL=$LABEL
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
#   2  not for the key given (age: no identity matched; rage: No matching keys found)
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
  if [ "$arc" != 0 ] && not_a_recipient "$text"; then
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

# The versions an opened envelope lists that this tape holds only some of
# (a range), in $WORK/range.versions: uuid TAB version TAB first TAB last TAB
# slices (see AWK_RANGE_VERSIONS). The tape's own seal and manifest say which.
collect_ranges() {
  local pos
  : >"$WORK/range.versions"
  for pos in $OPENED; do note_range_versions "$WORK/env.$pos"; done
}

# What this tape holds of version $2 of the unit with uuid $1 when it is only
# part of it: "first last slices" (first and last empty when none), or nothing.
range_of() { # <unit uuid> <version>
  RANGE_UUID="$1" RANGE_VER="$2" awk -F'\t' '
    $1 == ENVIRON["RANGE_UUID"] && ($2 + 0) == (ENVIRON["RANGE_VER"] + 0) { print $3 "|" $4 "|" $5; exit }
  ' "$WORK/range.versions"
}

# The words for a range, from range_of's answer.
range_words() { # <first|last|slices>
  local first last n
  IFS='|' read -r first last n <<<"$1"
  if [ -z "$first" ]; then
    printf 'this tape holds none of its %s slices (it ended before them)' "$n"
  else
    printf 'this tape holds only slices %s-%s of its %s' "$first" "$last" "$n"
  fi
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
  collect_ranges
  say "Units in the envelope(s) your key opened (unit, version, slices, bytes of stream, compression):"
  awk -F'|' '{ printf "  %s  version %s  %s slice(s)  %s bytes%s\n", $3, $4, $11, $8, ($7 != "" ? "  compressed with " $7 : "") }' "$WORK/units.txt"
  local row r name ver uuid ft
  while IFS= read -r row <&3; do
    IFS='|' read -r _ _ name ver _ uuid _ _ ft _ <<<"$row"
    [ -n "$ft" ] || continue
    r=$(range_of "$uuid" "$ver")
    [ -z "$r" ] || say "  PART: unit $(safe_str "$name") version $ver: $(range_words "$r"); the rest is on another tape."
  done 3<"$WORK/units.txt"
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
  collect_ranges
  choose_units listing
  local pos name ver uuid comp sb ft idm ns table r
  while IFS='|' read -r pos _ name ver _ uuid comp sb ft idm ns; do
    table=$(unit_table_path "$pos" "$ft")
    [ -f "$table" ] || die "the envelope has no file table for unit $(safe_str "$name") at $(safe_str "$ft")"
    say "Unit $(safe_str "$name")  version $ver  ($ns slice(s), $sb bytes of stream${comp:+, compressed with $comp})"
    r=$(range_of "$uuid" "$ver")
    [ -z "$r" ] || say "  PART: $(range_words "$r"). A file in a slice this tape does not hold is not on it."
    say "  Paths are written escaped: a backslash in a name is \\\\, a TAB \\t, a newline \\n, a carriage"
    say "  return \\r, and a byte that is not UTF-8 \\xHH. Give --path and --paths-from a path as it is printed here."
    say "  kind  slice  bytes  path"
    awk -F'\t' '/^#/ { next } { printf "  %s  %5s  %s  %s%s\n", $1, $2, $5, $7, ($1 == "h" ? "  -> " $8 : "") }' "$table"
  done <"$WORK/chosen.txt"
}

# ---- the next cartridge of a unit (R5, R7, R8, R9; volume-format-v3.md 5.3, 7.4) ----

# The tapes the index in the envelope this unit came from names as holders of
# this version of this unit, other than the loaded tape, one a line in
# $WORK/holders.txt: volume, first slice, last slice, state, chip serial
# (TAB-separated). With $1, only those whose run holds that slice. Returns 1
# when the envelope has no index or names none.
# Is the tape the run started on the cartridge in the drive now? Once another
# cartridge was taken, or a refused one is in the drive, it is not.
start_tape_in_drive() {
  [ "$HOME_CART" = 1 ] && [ "$FOREIGN_LOADED" = 0 ]
}

# Does the index say the tape the run started on holds slice $1 of this unit?
start_holds() { # <slice>
  local f="$WORK/env.$U_POS/index/units.tsv"
  [ -f "$f" ] || return 1
  U_ID="$U_UUID" U_V="$U_VER" U_K="$1" awk -F'\t' '
    NR == 1 { for (i = 1; i <= NF; i++) col[$i] = i; ok = ("unit_uuid" in col) && ("version" in col) && ("state" in col) && ("first_slice" in col) && ("last_slice" in col); next }
    !ok { next }
    $(col["unit_uuid"]) == ENVIRON["U_ID"] && ($(col["version"]) + 0) == (ENVIRON["U_V"] + 0) && $(col["state"]) == "this-tape" && $(col["first_slice"]) ~ /^[0-9]+$/ && $(col["last_slice"]) ~ /^[0-9]+$/ && (ENVIRON["U_K"] + 0) >= ($(col["first_slice"]) + 0) && (ENVIRON["U_K"] + 0) <= ($(col["last_slice"]) + 0) { f = 1 }
    END { exit f ? 0 : 1 }
  ' "$f"
}

holders_of() { # [slice]
  local f="$WORK/env.$U_POS/index/units.tsv"
  : >"$WORK/holders.txt"
  [ -f "$f" ] || return 1
  U_START_IN=$(start_tape_in_drive && echo 1 || echo 0) U_ID="$U_UUID" U_V="$U_VER" U_K="${1:-}" awk -F'\t' '
    NR == 1 {
      for (i = 1; i <= NF; i++) col[$i] = i
      ok = ("unit_uuid" in col) && ("version" in col) && ("state" in col) && ("first_slice" in col) && ("last_slice" in col) && ("volume" in col)
      next
    }
    !ok { next }
    {
      if ($(col["unit_uuid"]) != ENVIRON["U_ID"]) next
      if (($(col["version"]) + 0) != (ENVIRON["U_V"] + 0)) next
      st = $(col["state"])
      if (st == "this-tape" && ENVIRON["U_START_IN"] == "1") next
      f = $(col["first_slice"]); l = $(col["last_slice"])
      if (f !~ /^[0-9]+$/ || l !~ /^[0-9]+$/) next
      k = ENVIRON["U_K"]
      if (k != "" && (k + 0 < f + 0 || k + 0 > l + 0)) next
      ser = ("cartridge_serial" in col) ? $(col["cartridge_serial"]) : ""
      printf "%s\t%s\t%s\t%s\t%s\n", $(col["volume"]), f, l, st, ser
    }
  ' "$f" >"$WORK/holders.txt"
  [ -s "$WORK/holders.txt" ]
}

# $WORK/holders.txt as lines of words, each starting with $1.
print_holders() { # <prefix>
  local lab f l st ser
  while IFS=$'\t' read -r lab f l st ser; do
    printf '%s  tape %s (cartridge serial %s): slices %s to %s%s%s\n' "$1" "$(safe_str "$lab")" "${ser:-not recorded}" "$f" "$l" "$([ "$st" = quarantined ] && echo '; QUARANTINED: a verify proved it bad, it may still read in part')" "$([ "$st" = this-tape ] && echo '; the tape this restore started on')"
  done <"$WORK/holders.txt"
}

eject_first() {
  echo ">>> First eject the cartridge now in the drive: its eject button, or mt -f $DEVICE offline in another" >&2
  echo ">>> terminal, which is safe while this waits (no tape device is held open)." >&2
}

# What the prompt for the next cartridge says: the slice needed, the tapes the
# index names for it, how to answer.
ask_cartridge() { # <slice>
  local k=$1 f="$WORK/env.$U_POS/index/units.tsv"
  {
    echo ""
    echo ">>> Unit $(safe_str "$U_NAME") (version $U_VER) needs slice $k of $NSL next: ${SL_ENC[$k]} bytes, sha256 ${SL_SHAE[$k]}"
    if holders_of "$k"; then
      echo ">>> The index in this tape's envelope names these tapes as holding slice $k:"
      print_holders ">>>"
    elif [ -f "$f" ]; then
      echo ">>> No tape the index in this tape's envelope names holds slice $k (an index never names a tape"
      echo ">>> written after its own tape): $0 --where $(safe_str "$U_NAME") --key KEY on the newest tape you hold names them."
    else
      echo ">>> This tape's envelope carries no index, so no tape is named: $0 --where $(safe_str "$U_NAME") --key KEY"
      echo ">>> on the newest tape you hold may name them."
    fi
    eject_first
    echo ">>> Load the cartridge that holds slice $k into $DEVICE, wait until the drive is ready, then press ENTER."
    echo ">>> A cartridge is taken only when its front index lists that sha256 at a position its seal marker covers:"
    echo ">>> its label is never taken for it. This waits for as long as you take."
    echo ">>> Type done and ENTER (or end the input) if you do not have it: the restore goes on with what it has and"
    echo ">>> lists the rest with the tapes that hold it. Type quit and ENTER to stop the restore."
  } >&2
}

# Read the cartridge now in the drive, in a subshell of its own that reports
# through files in $1, so that nothing it refuses ends the restore (#521).
# Exit 0: it gives slice $2 (its facts are in $1/vars); 3: it is the cartridge
# already in the drive; anything else: refused, and the words say why.
probe_cartridge() { # <dir> <slice>
  trap - EXIT
  local dir=$1 k=$2 rc=0 magic lv label uuid pos feats v h16
  WORK=$dir
  tape_init
  CUR=""
  seek_to 0 || {
    echo "NOTE: this cartridge is unreadable: the drive cannot reach its first file. Nothing is taken from it." >&2
    exit 2
  }
  dd if="$DEVICE" bs="$BLOCK" of="$WORK/file0.raw" 2>"$WORK/dd.err" || rc=$?
  if [ "$rc" -ne 0 ]; then
    echo "NOTE: this cartridge is unreadable: its first file cannot be read ($(dd_detail)). Nothing is taken from it." >&2
    exit 2
  fi
  if [ ! -s "$WORK/file0.raw" ]; then
    echo "NOTE: this cartridge is blank: nothing can be read at its first file. Nothing is taken from it." >&2
    exit 2
  fi
  CUR=1
  tr -d '\0' <"$WORK/file0.raw" >"$WORK/file0.txt"
  magic=$(toml_val "$WORK/file0.txt" magic)
  lv=$(toml_val "$WORK/file0.txt" layout_version)
  label=$(toml_val "$WORK/file0.txt" label)
  uuid=$(toml_val "$WORK/file0.txt" uuid)
  if [ -z "$magic$lv$label$uuid" ]; then
    echo "NOTE: this cartridge is not a tapectl volume: its first file states no magic, layout_version, label or uuid. Nothing is taken from it." >&2
    exit 2
  fi
  if { [ -n "$magic" ] && [ "$magic" != tapectl-volume-v3 ]; } || { [ -n "$lv" ] && [ "$lv" != 3 ]; }; then
    echo "NOTE: this cartridge is a tapectl volume of Layout $(safe_str "${lv:-unknown}") (magic $(safe_str "${magic:-none}")), label $(safe_str "$label"); this RESTORE.sh reads Layout 3 only, so nothing is taken from it. Read it with the tapectl that wrote it or with the RESTORE.sh in its own File 2." >&2
    exit 2
  fi
  feats=$(requires_of "$WORK/file0.txt")
  if [ -n "$feats" ]; then
    echo "NOTE: this cartridge requires feature(s) $(safe_str "$feats"), which this RESTORE.sh does not know, so nothing is taken from it." >&2
    exit 2
  fi
  F0SHA=$(sha_of_file "$WORK/file0.raw")
  if { [ -n "$uuid" ] && [ "$uuid" = "$LOADED_UUID" ]; } ||
    { [ -z "$LOADED_UUID" ] && [ -n "$LOADED_F0SHA" ] && [ "$F0SHA" = "$LOADED_F0SHA" ]; }; then
    # The cartridge the facts describe, back in the drive after a refusal.
    if [ "$FOREIGN_LOADED" = 1 ]; then exit 4; fi
    echo "NOTE: this is the same cartridge as the one already in the drive (label $(safe_str "$label")), which does not hold slice $k. Eject it and load the cartridge that does." >&2
    exit 3
  fi
  file0_fields
  if try_front_index; then
    FILES_TXT="$WORK/files_from_index.txt"
    FILES_SOURCE="front index (File $FRONT_INDEX)"
    learn_end
  elif read_seal && seal_valid; then
    echo "WARNING: the front index of this cartridge is unreadable: using the seal marker's copy of it." >&2
    check_requires "the seal marker" "$WORK/seal.toml"
    FILES_TXT="$WORK/files_from_seal.txt"
    FILES_SOURCE="the seal marker's copy of the front index"
    SEAL_STATE=$([ -n "$TOTAL_FILES" ] && [ "$SEAL_FILE_COUNT" -lt "$TOTAL_FILES" ] && echo short || echo full)
    SEAL_BINDS=""
  else
    echo "NOTE: neither the front index nor the seal marker of this cartridge can be read, so nothing is taken from it." >&2
    exit 2
  fi
  if [ -z "$ENDED_AFTER" ] || { [ "$SEAL_STATE" != full ] && [ "$SEAL_STATE" != short ]; }; then
    echo "NOTE: Its seal marker cannot be read (or the tape is not sealed), so nothing is taken from it: a cartridge gives a slice only at a position its seal marker says the tape holds (R8)." >&2
    exit 2
  fi
  h16=$(printf '%s' "$NEED_HASH" | head -c 16)
  pos=$(awk -F'|' -v h="$NEED_HASH" -v s="$NEED_SIZE" '$2 == "data_slice" && ($4 "") == (h "") && ($3 "") == (s "") { print $1; exit }' "$FILES_TXT")
  if [ -z "$pos" ]; then
    echo "NOTE: This cartridge (label $(safe_str "$label")) lists no data slice with the sha256 of slice $k ($h16...) in its front index, so it does not hold that slice. Nothing is taken from it." >&2
    exit 2
  fi
  if [ "$pos" -gt "$ENDED_AFTER" ]; then
    echo "NOTE: This cartridge (label $(safe_str "$label")) lists slice $k (sha256 $h16...) at tape file $pos, but the tape ended after file $ENDED_AFTER (its seal marker says so): that slice is not on it. Nothing is taken from it." >&2
    exit 2
  fi
  {
    for v in F0SHA LABEL UUID FILES_TXT FILES_SOURCE ENDED_AFTER SEAL_STATE SEAL_BINDS SEAL_FILE_COUNT TOTAL_FILES FRONT_INDEX SEAL_MARKER; do
      printf '%s=%q\n' "$v" "${!v-}"
    done
  } >"$dir/vars"
  exit 0
}

CART_SEQ=0
# Is the cartridge now in the drive one that gives slice $1 (R8)? On success
# it is the loaded cartridge: its facts are taken, and SL_HERE says which of
# the unit's slices it holds.
accept_cartridge() { # <slice>
  local k=$1 sub rc=0 a b j
  CART_SEQ=$((CART_SEQ + 1))
  sub="$WORK/cart.$CART_SEQ"
  mkdir -p "$sub" || die_io "cannot make a directory for the cartridge" "" "$WORK"
  CUR=""
  NEED_HASH=${SL_SHAE[$k]} NEED_SIZE=${SL_ENC[$k]}
  (probe_cartridge "$sub" "$k") 2>"$sub/err" || rc=$?
  CUR=""
  sed 's/^FATAL: /NOTE: /' "$sub/err" >&2 || true
  if [ "$rc" = 3 ]; then
    return 1
  fi
  if [ "$rc" = 4 ]; then
    # The cartridge the run's facts describe is back in the drive.
    FOREIGN_LOADED=0
    locate_here
    if [ -n "${SL_HERE[$k]:-}" ]; then
      echo "This is the cartridge the run had before ($(safe_str "$LABEL")): it gives slice $k." >&2
      return 0
    fi
    echo "NOTE: this is the cartridge the run had before, and it does not hold slice $k. Eject it and load the cartridge that does." >&2
    return 1
  fi
  if [ "$rc" != 0 ] || [ ! -f "$sub/vars" ]; then
    # Another cartridge is in the drive now, one this run cannot use. What the
    # run knows of the cartridge it took last is kept: that one may come back
    # (a refusal never costs the run its tape, #576).
    FOREIGN_LOADED=1
    SL_HERE=()
    return 1
  fi
  # shellcheck disable=SC1090
  . "$sub/vars"
  HOME_CART=0
  FOREIGN_LOADED=0
  LOADED_UUID=$UUID
  LOADED_F0SHA=$F0SHA
  locate_here
  [ -n "${SL_HERE[$k]:-}" ] || return 1
  a=$k
  b=$k
  for ((j = k + 1; j <= NSL; j++)); do
    [ -n "${SL_HERE[$j]:-}" ] || break
    b=$j
  done
  echo "This cartridge identifies as: $(safe_str "$LABEL")" >&2
  echo "It gives slice(s) $a to $b of $(safe_str "$U_NAME"): its front index lists each one's sha256, at positions its seal marker covers." >&2
  return 0
}

# Ask for the cartridge that gives slice $1, until one does or the heir
# answers. The answer is read from standard input with no time limit, and no
# tape device is open while it waits (every mt and dd opens it by itself).
# done, or the end of the input, is "I do not have it": the restore goes on
# with what it has. quit ends the restore.
# After "done", whatever came before: what is in the drive now? The cartridge
# the run's facts describe (exit 3 or 4: the heir may have reloaded it, or never
# changed it): what it holds is restored. Anything else, another cartridge, a
# blank one or an empty drive: nothing is read from it, and the slices the
# described cartridge holds are said to be on a cartridge that is not in the
# drive (#576).
recheck_drive() { # <slice asked for>
  local sub rc=0
  CART_SEQ=$((CART_SEQ + 1))
  sub="$WORK/cart.$CART_SEQ"
  mkdir -p "$sub" || return 0
  CUR=""
  NEED_HASH=${SL_SHAE[$1]} NEED_SIZE=${SL_ENC[$1]}
  (probe_cartridge "$sub" "$1") 2>/dev/null || rc=$?
  CUR=""
  DRIVE_HOLDS=""
  if [ "$rc" = 3 ] || [ "$rc" = 4 ]; then
    FOREIGN_LOADED=0
  else
    FOREIGN_LOADED=1
    # Another cartridge that holds the slice asked for is in the drive: the
    # heir typed done before it was taken.
    if [ "$rc" = 0 ]; then DRIVE_HOLDS=$1; fi
  fi
  locate_here
  return 0
}

swap_for() { # <slice>: 0 when the cartridge in the drive now holds it
  [ "$GAVE_UP" = 0 ] || return 1
  local reply
  while :; do
    ask_cartridge "$1"
    reply="done"
    IFS= read -r reply || reply="done"
    case $reply in
    done | Done | DONE)
      GAVE_UP=1
      recheck_drive "$1"
      echo "NOTE: going on without it: what the cartridges read so far hold is restored, and the rest is listed with the tapes that hold it." >&2
      return 1
      ;;
    quit | Quit | QUIT | q | Q)
      GAVE_UP=1
      QUIT=1
      echo "NOTE: stopping the restore at your request. The files restored and checked so far stay; a file tar had begun is removed." >&2
      return 1
      ;;
    esac
    if accept_cartridge "$1"; then return 0; fi
  done
}

# SL_HERE[k]: the tape file of slice k on the cartridge in the drive, or
# nothing. On the cartridge the run started on, the manifest's tape_position
# at or before the seal's ended_after; on any other, the file its front index
# lists with the slice's sha256 and size at or before ITS ended_after (R8).
SL_HERE=()
SL_DESC=() # the slices the cartridge the run's facts describe holds, in the drive or not
locate_here() {
  local k
  locate_described
  SL_DESC=()
  for k in "${!SL_HERE[@]}"; do SL_DESC[$k]=${SL_HERE[$k]}; done
  [ "$FOREIGN_LOADED" = 0 ] || SL_HERE=()
}
locate_described() {
  local k p num pos
  SL_HERE=()
  if [ "$HOME_CART" = 1 ]; then
    for ((k = 1; k <= NSL; k++)); do
      p=${SL_POS[$k]:-}
      if is_uint "$p" && on_tape "$((10#$p))"; then SL_HERE[$k]=$((10#$p)); fi
    done
    return 0
  fi
  [ -n "$FILES_TXT" ] && [ -f "$FILES_TXT" ] && is_uint "$ENDED_AFTER" || return 0
  for ((k = 1; k <= NSL; k++)); do printf '%s|%s|%s\n' "$k" "${SL_ENC[$k]}" "${SL_SHAE[$k]}"; done >"$WORK/slices.want"
  awk -F'|' -v end="$ENDED_AFTER" '
    FILENAME == ARGV[1] { if ($2 == "data_slice" && $1 ~ /^[0-9]+$/ && $1 + 0 <= end + 0 && !(($4 "|" $3) in at)) at[$4 "|" $3] = $1; next }
    { key = $3 "|" $2; if (key in at) print $1 "|" at[key] }
  ' "$FILES_TXT" "$WORK/slices.want" >"$WORK/slices.here"
  while IFS='|' read -r num pos; do SL_HERE[$num]=$pos; done <"$WORK/slices.here"
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
    # A destination that holds the damaged-file list of an earlier run is one a
    # later run from another tape adds to (whether it is the same unit and
    # version is judged when the unit is read).
    # The list lies in the directory of its unit, $TO/<unit name>, and a unit
    # name may hold slashes (tv/breaking-bad/s01): no one-level glob finds it
    # (#577). The units of this run are not known yet, so any depth is looked
    # at; each unit's own directory is judged when that unit is restored.
    if (
      shopt -s globstar nullglob
      set -- "$TO"/**/.tapectl-damaged-files.txt # ** also matches no directory at all: $TO itself
      [ $# -gt 0 ]
    ); then
      TO_HELD_LIST=1
      return 0
    fi
    die "--to $(safe_str "$TO") is not empty. Restore into an empty or new directory, or give --overwrite to replace files that are there (to finish a restore that stopped)."
  fi
}

# One slice per entry, 1-based: the manifest's claims for the unit chosen.
SL_POS=() SL_SIZE=() SL_ENC=() SL_SHAP=() SL_SHAE=() SL_CONT=() SL_OFF=()
NSL=0
COMP=""

# SL_OFF[k] is where slice k's cut begins in the unit's stream: the sum of the
# plain lengths before it (the slices joined are the stream, rule 13).
load_slices() { # <pos> <idx>
  local num tp sz en sp se co off=0
  SL_POS=() SL_SIZE=() SL_ENC=() SL_SHAP=() SL_SHAE=() SL_CONT=() SL_OFF=()
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
    SL_OFF[$NSL]=$off
    if is_uint "$sz"; then off=$((off + sz)); fi
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

# Slice $1 is not on the cartridge in the drive and none that holds it was
# given: it is left out, and its files are listed. Said as it is: not on this
# cartridge (never damage), by the manifest's position or by the hash.
absent_slice() { # <slice number>
  local k=$1 p why
  p=${SL_POS[$k]:-}
  if [ "$DRIVE_HOLDS" = "$k" ]; then
    why="the cartridge in the drive does hold it, but done was answered before it was taken: nothing was read from it (ask again and press ENTER to take it)"
  elif [ "$FOREIGN_LOADED" = 1 ] && [ -n "${SL_DESC[$k]:-}" ]; then
    why="it is on the tape $(safe_str "${LABEL:-the run started on}"), which is not in the drive now: load that tape again and its slices are restored"
    printf '%s\n' "$k" >>"$WORK/outdrive.slices"
  elif [ "$HOME_CART" = 0 ] && start_holds "$k"; then
    why="it is on the tape $(safe_str "${START_LABEL:-the run started on}"), which this restore started on and which is not in the drive now: load that tape again and its slices are restored"
    printf '%s\n' "$k" >>"$WORK/outstart.slices"
  elif [ "$HOME_CART" = 1 ] && is_uint "$p" && [ -n "$ENDED_AFTER" ] && [ "$((10#$p))" -gt "$ENDED_AFTER" ]; then
    why="file $((10#$p)) is after the end of this tape (it ended after file $ENDED_AFTER)"
  elif [ "$HOME_CART" = 1 ]; then
    why="this tape's plan does not hold it (it is on another cartridge)"
  elif [ "$FOREIGN_LOADED" = 1 ]; then
    why="it is on a cartridge that is not in the drive (nothing was read from the one that is)"
  else
    why="the cartridge in the drive does not hold it (it is on another cartridge)"
  fi
  if [ "$QUIT" = 1 ]; then
    why="$why; you stopped the restore"
  elif [ "$GAVE_UP" = 1 ] && [ "$DRIVE_HOLDS" != "$k" ]; then
    why="$why; no cartridge that holds it was given"
  fi
  printf '%s\n' "$k" >>"$WORK/absent.slices"
  mark_bad "$k" "$why"
}

# Slice $1 is absent because slice $2, the first of its long member's run, is.
absent_follows() { # <slice> <slice it follows>
  if [ -z "${SL_HERE[$1]:-}" ] && grep -x "$2" "$WORK/absent.slices" >/dev/null 2>&1; then
    printf '%s\n' "$1" >>"$WORK/absent.slices"
    if grep -x "$2" "$WORK/outdrive.slices" >/dev/null 2>&1 && [ -n "${SL_DESC[$1]:-}" ]; then
      printf '%s\n' "$1" >>"$WORK/outdrive.slices"
    elif grep -x "$2" "$WORK/outstart.slices" >/dev/null 2>&1 && start_holds "$1"; then
      printf '%s\n' "$1" >>"$WORK/outstart.slices"
    fi
  fi
}

# Check slice $1 from the tape and leave its plain cut in slice.plain, or mark
# it bad and return 1. A slice is checked against the front index (hash of
# the file as written), then against the manifest after decryption (length
# and hash of the cut, after decompression). Nothing the tape or age or a
# decompressor says about its own exit status is the verdict; the lengths and
# hashes are.
prepare_slice() { # <slice number>
  local k=$1 pos="${SL_HERE[$1]:-}" enc=${SL_ENC[$1]} arc="" got="" text="" why="" want_h rc=0
  rm -f "$WORK/slice.enc" "$WORK/slice.plain" "$WORK/age.rc"
  FEEDING=$k
  if [ -z "$pos" ] && [ "$QUIT" = 0 ]; then
    # The cartridge in the drive does not hold it: ask for the one that does.
    swap_for "$k" || true
    pos="${SL_HERE[$k]:-}"
  fi
  if [ "$QUIT" = 1 ] && [ -n "$pos" ]; then
    # quit stops: nothing more is fetched or fed, and what was not reached is
    # listed as such (the cartridge in the drive may not even be this one).
    mark_bad "$k" "not reached: you stopped the restore"
    return 1
  fi
  if [ -z "$pos" ]; then
    absent_slice "$k"
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
    if not_a_recipient "$text"; then
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
  cat "$WORK/slice.plain" >&4 || rc=$?
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
          for ((j = a + 1; j <= b; j++)); do
            mark_bad "$j" "part of the long member whose first slice is bad"
            absent_follows "$j" "$a"
          done
          k=$b
        else
          for ((j = k + 1; j <= b; j++)); do
            mark_bad "$j" "part of a long member with a bad piece"
            absent_follows "$j" "$k"
          done
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

# A path as the file table writes it (volume-format-v3.md 1.5) is escaped: a
# backslash is \\, a TAB \t, a newline \n, a carriage return \r, and each byte
# that is not part of a valid UTF-8 sequence \xHH (two lowercase hex digits). The
# name on disk is the bytes. This ONE awk program reads the escaped text from its
# standard input (never through -v, which would process the backslashes) and
# writes the bytes, each by its code: not printf %b, which reads other escapes
# (\c stops the output). Run under LC_ALL=C by every awk the script supports,
# it writes a byte as a byte. A backslash that is not one of these escapes is
# kept as it stands. A NUL byte is never written (a name has none).
AWK_UNESC='
BEGIN { hex = "0123456789abcdef" }
{
  s = $0
  while ((p = index(s, "\\")) > 0) {
    printf "%s", substr(s, 1, p - 1)
    d = substr(s, p + 1, 1)
    if (d == "t") { printf "\t"; s = substr(s, p + 2) }
    else if (d == "n") { printf "\n"; s = substr(s, p + 2) }
    else if (d == "r") { printf "\r"; s = substr(s, p + 2) }
    else if (d == "\\") { printf "\\"; s = substr(s, p + 2) }
    else if (d == "x" && substr(s, p + 2, 2) ~ /^[0-9a-f][0-9a-f]$/) {
      v = (index(hex, substr(s, p + 2, 1)) - 1) * 16 + (index(hex, substr(s, p + 3, 1)) - 1)
      if (v > 0) printf "%c", v
      s = substr(s, p + 4)
    }
    else { printf "\\"; s = substr(s, p + 1) }
  }
  printf "%s", s
}
'

# The name on disk of an escaped path: unesc <escaped text>; the result is in
# REPLY. A sentinel byte is written after the bytes and taken off again, so a
# name that ends in a newline survives the command substitution.
unesc() {
  REPLY=$(
    printf '%s' "$1" | awk "$AWK_UNESC"
    printf x
  )
  REPLY=${REPLY%x}
}

# What tar's non-zero status means to the heir: the per-file check decides what
# was restored, and under --no-file-check no check runs, so nothing does.
tar_status_note() { # <status>
  if [ "$NO_FILE_CHECK" = 1 ]; then
    TAR_FAILED=1
    note "tar ended with status $1: $(head -n 3 "$WORK/tar.err" 2>/dev/null | tr '\n' ' '). --no-file-check: no check runs, so nothing says what was restored, and the run is incomplete."
  else
    note "tar ended with status $1: $(head -n 3 "$WORK/tar.err" 2>/dev/null | tr '\n' ' '). The per-file check below decides what was restored."
  fi
}

# The paths (escaped) of every member of a table that begins in the slices
# listed in $2 (one number at the start of each line): a folder is a member
# like another and is listed too. A listed folder brings back its own entry
# (its mode, owner and times) and nothing under it: the repair reads members
# by their offsets and never extracts by name. A hard-linked name whose first
# name begins in one of those slices is listed too: tar cannot make it, and a
# run with no file check must never leave it silently missing.
members_in_slices() { # <table> <bad.slices file: number|why>
  cut -d'|' -f1 "$2" >"$WORK/bad.numbers"
  awk -F'\t' '
    FILENAME == ARGV[1] { bad[$1 ""] = 1; next }
    /^#/ { next }
    { n++; kind[n] = $1; sl[n] = $2 ""; path[n] = $7; tgt[n] = $8; at[$7] = $2 "" }
    END {
      for (i = 1; i <= n; i++) {
        if (sl[i] in bad) print path[i]
        else if (kind[i] == "h" && (tgt[i] in at) && (at[tgt[i]] in bad)) print path[i]
      }
    }
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

# Extract one tar run's stream from our standard input. It runs as a process
# substitution (tar_open), so a signal for it is answered with an rc file the
# waiting shell can read.
tar_extract() { # <destination>
  local rc=0
  trap 'echo 143 >"$WORK/tar.rc"; exit 143' TERM HUP INT
  tar -x -f - -C "$1" ${TAR_FLAGS[@]+"${TAR_FLAGS[@]}"} 2>"$WORK/tar.err" || rc=$?
  echo "$rc" >"$WORK/tar.rc"
}

# One tar run takes its input on fd 4, written by this shell: the prompt for a
# cartridge must be read here (a trap waits for a foreground pipeline, so a
# prompt read in the left side of one would not answer a signal), and what a
# run learns of the cartridge in the drive must stay here. No FIFO file is
# made, and no process id is remembered.
wait_tar() { # [tenths of a second to wait]
  local n=0 max=${1:-6000}
  until [ -e "$WORK/tar.rc" ]; do
    sleep 0.1 2>/dev/null || sleep 1
    n=$((n + 1))
    if [ "$n" -ge "$max" ]; then
      warn "tar did not report its end; going on."
      return 0
    fi
  done
}

tar_open() { # <destination>
  rm -f "$WORK/tar.rc" "$WORK/tar.err"
  exec 4> >(tar_extract "$1")
  TAR_OPEN=1
}

tar_close() { # [tenths of a second to wait for tar]
  [ "$TAR_OPEN" = 1 ] || return 0
  exec 4>&-
  TAR_OPEN=0
  wait_tar "${1:-6000}"
}

# A signal, or a failure, while a unit is being fed: the member tar was
# extracting may be partial. Every regular member whose slices include the
# slice being prepared or fed, or the one before it, is looked at, and one that
# is not whole (its length or its sha256 differs from the table) is removed; a
# file that is whole stays, so a file an earlier run restored and checked is
# never lost. The one before it too: slice k's cat has returned when slice k+1
# is being prepared, but tar may still hold the last 64 KiB (1 MiB under
# uutils) of k in its pipe, and a signal kills it inside a member of k.
scrub_inflight() {
  [ -n "$INFLIGHT_TABLE" ] && [ -f "$INFLIGHT_TABLE" ] && [ -n "$WORK" ] && [ -f "$WORK/slices.off" ] || return 0
  [ "$FEEDING" -gt 0 ] || return 0
  local sz sha esc f
  while IFS=$'\t' read -r sz sha esc; do
    [ -n "$esc" ] || continue
    unesc "$esc"
    f="$INFLIGHT_DEST/$REPLY"
    [ -f "$f" ] || continue
    if [ "$(wc -c <"$f" | tr -d ' ')" != "$sz" ] || [ "$(sha_of_file "$f" 2>/dev/null || true)" != "$sha" ]; then
      rm -f -- "$f" 2>/dev/null || true
    fi
  done <<EOF
$(awk -F'\t' -v at="$FEEDING" '
    FILENAME == ARGV[1] { n++; off[$1 + 0] = $2 + 0; sz[$1 + 0] = $3 + 0; next }
    /^#/ { next }
    $1 == "f" {
      b = $2 + 0
      if (b > at) next
      last = b
      while (last < n && off[last] + sz[last] < $4 + 0) last++
      if (last >= (at > 1 ? at - 1 : at)) print $5 "\t" $6 "\t" $7
    }
  ' "$WORK/slices.off" "$INFLIGHT_TABLE")
EOF
}

DAMAGED_TOTAL=0
UNITS_DONE=0

# The unit chosen, from its row of units.txt: its file table and its slices
# are read, and the decompressor its staging needs is found.
U_POS="" U_IDX="" U_NAME="" U_VER="" U_UUID="" U_COMP="" U_SB="" U_FT="" U_IDM="" U_NS="" U_TABLE=""
unit_open() { # <chosen row>
  IFS='|' read -r U_POS U_IDX U_NAME U_VER _ U_UUID U_COMP U_SB U_FT U_IDM U_NS <<<"$1"
  U_TABLE=$(unit_table_path "$U_POS" "$U_FT")
  [ -f "$U_TABLE" ] || die "the envelope has no file table for unit $(safe_str "$U_NAME") at $(safe_str "$U_FT")"
  COMP=$U_COMP
  DECOMP=""
  if [ -n "$COMP" ]; then
    DECOMP=$(decompressor_for "$COMP" || true)
    [ -n "$DECOMP" ] || die "unit $(safe_str "$U_NAME") states compression '$(safe_str "$COMP")', which this RESTORE.sh does not know (it knows gzip, xz and zstd). Use a newer tapectl or RESTORE.sh."
    command -v "$DECOMP" >/dev/null 2>&1 ||
      die "unit $(safe_str "$U_NAME") was compressed with $COMP and needs the tool '$DECOMP', which is not installed (package: $DECOMP, or xz-utils for xz). Nothing has been read from a slice."
  fi
  load_slices "$U_POS" "$U_IDX"
  [ "$NSL" -gt 0 ] || die "the manifest lists no slice for unit $(safe_str "$U_NAME")"
  # Where each slice's cut begins in the stream, for the cleanup of a member
  # a signal left partial.
  local k
  for ((k = 1; k <= NSL; k++)); do printf '%s\t%s\t%s\n' "$k" "${SL_OFF[$k]}" "${SL_SIZE[$k]}"; done >"$WORK/slices.off"
  : >"$WORK/absent.slices"
  : >"$WORK/outdrive.slices"
  : >"$WORK/outstart.slices"
  FEEDING=0
  GAVE_UP=0
  DRIVE_HOLDS=""
  locate_here
}

# The damaged-file list of a unit's restored directory (volume-format-v3.md
# 1.7): the header, then one path a line, escaped as the index escapes.
write_damaged_list() { # <destination> <name> <uuid> <version> <file of paths>
  local list="$1/.tapectl-damaged-files.txt"
  {
    printf '#tapectl-damaged-files\t1\tunit=%s\tuuid=%s\tversion=%s\n' "$2" "$3" "$4"
    cat "$5"
  } >"$list" || die_io "cannot write the damaged-file list" "" "$1"
}

print_damaged() { # <file of paths> <count> <list path>
  say "DAMAGED: $2 path(s) were left out or failed the check. They are listed in $3:"
  head -n 41 "$1" | sed 's/^/  /'
  if [ "$2" -gt 40 ]; then say "  ... and $(($2 - 40)) more, in the list."; fi
}

# Who the ids were: the id map of the staging, shown and never acted on.
print_id_map() { # <envelope position> <id map name>
  local ids="$WORK/env.$1/$2"
  if [ -n "$2" ] && [ -f "$ids" ]; then
    say "Who the ids were (the stream carries owners by number; a restore never remaps them by name):"
    awk -F'\t' '/^#/ { next } { printf "  %s %s  %s\n", ($1 == "u" ? "user " : "group"), $2, ($3 == "" ? "(no name known)" : $3) }' "$ids"
  fi
}

# The first line of the damaged-file list of the unit and version open.
list_header() {
  printf '#tapectl-damaged-files\t1\tunit=%s\tuuid=%s\tversion=%s' "$U_NAME" "$U_UUID" "$U_VER"
}

# What a unit read in more than one run, or with some of its slices on other
# cartridges, loses (measured, T0.7 and #567): said once when it happens.
say_run_limits() {
  note "A unit read in more than one run has two limits, both measured: a folder's modification time is that of the last run that touched the folder, and a hard-linked name whose first name was not read is not made until that name is (the repair makes it once the tape that holds the first name has been read)."
}

# The slices left out because they were on another cartridge: where the rest
# of the unit is, from this tape's MANIFEST (which gives those slices no
# position here) and the index in its envelope (which names the tapes), and the
# hard-linked names whose first name is among them.
ranges_of() { # <file of slice numbers>: 1-3, 5
  sort -n -u "$1" | awk '
    { n = $1 + 0; if (have && n == last + 1) { last = n; next } if (have) printf "%s%s", (c++ ? ", " : ""), (first == last ? first : first "-" last); first = last = n; have = 1 }
    END { if (have) printf "%s%s", (c++ ? ", " : ""), (first == last ? first : first "-" last) }'
}

# What to do once the tape holding slices that were not in the drive is loaded,
# by the mode of the restore that left them out: only a whole-unit restore and a
# repair have a continuation (the same command), and a restore of several units
# refuses the directory of one that came out whole (it holds no list).
absent_remedy() {
  local d
  d=$(safe_str "$ABSENT_DEST")
  case $ABSENT_HOW in
  path) printf 'give the damaged-file list to --paths-from (--restore --paths-from %s/.tapectl-damaged-files.txt --to %s): --path has no continuation, and the same command is refused, its files are already there. The files of those slices are on the damaged-file list.' "$d" "$d" ;;
  repair) printf 'run the same command again: the paths of those slices are still on the damaged-file list, and the repair restores them.' ;;
  *)
    if [ "$RUN_UNITS" -gt 1 ]; then
      printf 'run the command again for only the units that did not come out whole, each as --unit NAME --to %s/NAME (the same command is refused for a unit that did: its directory holds no damaged-file list). The files of those slices are on the damaged-file list, and the continuation restores them.' "$(safe_str "$TO")"
    else
      printf 'run the same command again: the files of those slices are on the damaged-file list, and the continuation restores them.'
    fi
    ;;
  esac
}

report_absent() { # <damaged paths file>
  [ -s "$WORK/absent.slices" ] || return 0
  local ranges nh
  # Slices the tape the run's facts describe holds, while another cartridge (or
  # none) is in the drive: they are not on a tape the heir lacks.
  if [ -s "$WORK/outdrive.slices" ]; then
    say "NOT IN THE DRIVE: slices $(ranges_of "$WORK/outdrive.slices") of $NSL of unit $(safe_str "$U_NAME") (version $U_VER) are on the tape $(safe_str "${LABEL:-the run started on}"), which is not in the drive now (another cartridge, or none, is). Nothing was read from what is in the drive. Load $(safe_str "${LABEL:-that tape}") and $(absent_remedy)"
  fi
  if [ -s "$WORK/outstart.slices" ]; then
    say "NOT IN THE DRIVE: slices $(ranges_of "$WORK/outstart.slices") of $NSL of unit $(safe_str "$U_NAME") (version $U_VER) are on the tape $(safe_str "${START_LABEL:-the run started on}"), the one this restore started on, which is not in the drive now. Nothing was read from what is in the drive. Load $(safe_str "${START_LABEL:-that tape}") and $(absent_remedy)"
  fi
  cat "$WORK/outdrive.slices" "$WORK/outstart.slices" >"$WORK/outall.slices"
  awk 'FILENAME == ARGV[1] { o[$1 + 0] = 1; next } !(($1 + 0) in o)' "$WORK/outall.slices" "$WORK/absent.slices" >"$WORK/absent.other"
  if [ -s "$WORK/absent.other" ]; then
    ranges=$(ranges_of "$WORK/absent.other")
    say "NOT ON THIS CARTRIDGE: slices $ranges of $NSL of unit $(safe_str "$U_NAME") (version $U_VER). Read from this tape's MANIFEST: it gives those slices no place on this tape, or a place after the end its seal marker states. Their files are on the damaged-file list."
    if holders_of; then
      say "The index in this tape's envelope names these other tapes as holding the unit, with the slices each holds:"
      print_holders "  "
      # The slices no named tape holds.
      local none
      none=$(sort -n -u "$WORK/absent.other" | U_H="$WORK/holders.txt" awk '
        BEGIN { while ((getline l < ENVIRON["U_H"]) > 0) { split(l, f, "\t"); for (i = f[2] + 0; i <= f[3] + 0; i++) held[i] = 1 } }
        { if (!(($1 + 0) in held)) printf "%s%s", (c++ ? ", " : ""), $1 }')
      if [ -n "$none" ]; then
        say "No tape the index names holds slice(s) $none (an index never names a tape written after its own tape): $0 --where $(safe_str "$U_NAME") --key KEY on the newest tape you hold names them."
      fi
    elif [ -f "$WORK/env.$U_POS/index/units.tsv" ]; then
      say "No tape the index in this tape's envelope names holds the slices (an index never names a tape written after its own tape): $0 --where $(safe_str "$U_NAME") --key KEY on the newest tape you hold names them."
    else
      say "This tape's envelope carries no index, so no tape is named: $0 --where $(safe_str "$U_NAME") --key KEY on the newest tape you hold may name them."
    fi
  fi
  # Hard-linked names whose first name lies in a slice not read.
  nh=$(awk -F'\t' '
    FILENAME == ARGV[1] { gone[$1 + 0] = 1; next }
    FILENAME == ARGV[2] { want[$0] = 1; next }
    /^#/ { next }
    { at[$7] = $2 + 0; if ($1 == "h") { hp[++n] = $7; ht[n] = $8 } }
    END { for (i = 1; i <= n; i++) if ((hp[i] in want) && (ht[i] in at) && (at[ht[i]] in gone)) print hp[i] "\t" ht[i] "\t" at[ht[i]] }
  ' "$WORK/absent.slices" "$1" "$U_TABLE" | head -n 20)
  if [ -n "$nh" ]; then
    say "Hard-linked names whose first name is in a slice that was not read (restored by the repair once that tape has been read):"
    printf '%s\n' "$nh" | while IFS=$'\t' read -r hp ht sl; do
      say "  $(visible_text "$hp") -> first name $(visible_text "$ht"), slice $sl"
    done
  fi
  say_run_limits
}

# The units whose directories under --to hold a damaged-file list, and the
# command that continues them: --unit NAME --to TO/NAME for one (its directory
# is --to for a single unit), --unit A --unit B --to TO for several.
units_with_lists() {
  local names n first
  names=$(
    shopt -s globstar nullglob
    for f in "$TO"/**/.tapectl-damaged-files.txt; do
      n=${f#"$TO"/}
      n=${n%/.tapectl-damaged-files.txt}
      [ "$n" != ".tapectl-damaged-files.txt" ] || continue
      printf '%s\n' "$n"
    done
  )
  [ -n "$names" ] || return 0
  first=$(printf '%s\n' "$names" | head -n 1)
  if [ "$(printf '%s\n' "$names" | wc -l | tr -d ' ')" -eq 1 ]; then
    printf 'The unit whose directory holds a list is %s: continue it with --unit %s --to %s/%s. ' "$(safe_str "$first")" "$(safe_str "$first")" "$(safe_str "$TO")" "$(safe_str "$first")"
  else
    printf 'The units whose directories hold a list: %s. Continue them with %s--to %s. ' "$(printf '%s\n' "$names" | while IFS= read -r n; do printf '%s, ' "$(safe_str "$n")"; done | sed 's/, $//')" "$(printf '%s\n' "$names" | while IFS= read -r n; do printf -- '--unit %s ' "$(safe_str "$n")"; done)" "$(safe_str "$TO")"
  fi
}

restore_one() { # <chosen row> <destination>
  local dest=$2 table pos name ver uuid idm ns sb
  unit_open "$1"
  table=$U_TABLE pos=$U_POS name=$U_NAME ver=$U_VER uuid=$U_UUID idm=$U_IDM ns=$U_NS sb=$U_SB
  mkdir -p -- "$dest" || die "cannot create $(safe_str "$dest")"
  local earlier="$dest/.tapectl-damaged-files.txt"
  if [ -f "$earlier" ] && [ "$(head -n 1 "$earlier" 2>/dev/null || true)" = "$(list_header)" ]; then
    continue_unit "$dest"
    return 0
  fi
  if [ "$OVERWRITE" = 0 ] && { { [ "$dest" != "$TO" ] && [ -n "$(ls -A -- "$dest" 2>/dev/null)" ]; } || { [ "$dest" = "$TO" ] && [ "$TO_HELD_LIST" = 1 ]; }; }; then
    if [ -f "$earlier" ]; then
      die "$(safe_str "$dest") is not empty, and the damaged-file list in it is not this unit's and version's. Restore into an empty or new directory, or give --overwrite to replace files that are there."
    fi
    die "$(safe_str "$dest") is not empty and holds no damaged-file list, so it is not an earlier run of this unit that stopped short. $(units_with_lists)Restore into an empty or new directory, or give --overwrite to replace files that are there."
  fi
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
  INFLIGHT_DEST=$dest
  INFLIGHT_TABLE=$table
  local resume=1 state="" tarrc partial_first
  while :; do
    rm -f "$WORK/feed.state" "$WORK/tar.rc" "$WORK/feed.cur"
    tar_open "$dest"
    feed_from "$resume" || true
    tar_close
    if [ -e "$WORK/fatal.flag" ]; then exit 1; fi
    CUR=$(cat "$WORK/feed.cur" 2>/dev/null || true)
    state=$(cat "$WORK/feed.state" 2>/dev/null || echo "gone ?")
    tarrc=$(cat "$WORK/tar.rc" 2>/dev/null || echo "?")
    if [ "$tarrc" != 0 ]; then
      tar_status_note "$tarrc"
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
  INFLIGHT_TABLE=""
  sort -u "$WORK/left.out" "$WORK/failed.paths" >"$WORK/damaged.paths"
  local ndam
  ndam=$(wc -l <"$WORK/damaged.paths" | tr -d ' ')
  local list="$dest/.tapectl-damaged-files.txt"
  if [ "$ndam" -gt 0 ]; then
    write_damaged_list "$dest" "$name" "$uuid" "$ver" "$WORK/damaged.paths"
    print_damaged "$WORK/damaged.paths" "$ndam" "$list"
    DAMAGED_TOTAL=$((DAMAGED_TOTAL + ndam))
    ABSENT_HOW=fresh ABSENT_DEST=$dest
    report_absent "$WORK/damaged.paths"
  else
    rm -f -- "$list"
  fi
  print_id_map "$pos" "$idm"
  UNITS_DONE=$((UNITS_DONE + 1))
}

# A destination that holds the damaged-file list of this unit and version is an
# earlier run's: this run adds to it. Only the rows this tape holds whole that
# do not already pass their sha256 are restored (the members are fed one by
# one, by their offsets, as a path restore feeds them), so a file that passes
# is never rewritten, and the list follows the merge rule of the repair.
continue_unit() { # <destination>
  local dest=$1 k
  if [ "$NO_FILE_CHECK" = 1 ]; then
    note "$(safe_str "$dest") holds the damaged-file list of an earlier run of unit $(safe_str "$U_NAME") version $U_VER: this run adds to it. With --no-file-check nothing says which files are whole, so every file the list names is restored again (the cartridge a file needs is asked for, as the repair asks)."
  else
    note "$(safe_str "$dest") holds the damaged-file list of an earlier run of unit $(safe_str "$U_NAME") version $U_VER: this run adds to it. Only the files that do not already pass the table's sha256 are restored (the cartridge a file needs is asked for, as the repair asks); a file that passes is left alone."
  fi
  : >"$WORK/failed.paths"
  if [ "$NO_FILE_CHECK" = 0 ]; then
    file_check "$U_TABLE" "$dest" "$WORK/failed.paths"
  else
    # Nothing can say which of the files pass, so none is taken to: every path
    # the list names is fed again, as the repair feeds them, and what this run
    # does not read stays on the list (#574).
    sed -n '2,$p' "$dest/.tapectl-damaged-files.txt" | grep -v '^$' >"$WORK/failed.paths" || true
  fi
  # A folder, a link or a special file is wanted when it is not there.
  awk -F'\t' '!/^#/ && $1 != "f" && $1 != "h" { print $7 }' "$U_TABLE" >"$WORK/other.rows"
  : >"$WORK/other.missing"
  local esc
  while IFS= read -r esc; do
    unesc "$esc"
    if [ ! -e "$dest/$REPLY" ] && [ ! -L "$dest/$REPLY" ]; then printf '%s\n' "$esc" >>"$WORK/other.missing"; fi
  done <"$WORK/other.rows"
  sort -u "$WORK/failed.paths" "$WORK/other.missing" >"$WORK/want.paths"
  # With the check on, every listed path that passes it (or is present, for a
  # folder or link) is whole whether or not this run fed it: it leaves the list.
  : >"$WORK/passes.paths"
  if [ "$NO_FILE_CHECK" = 0 ]; then
    sed -n '2,$p' "$dest/.tapectl-damaged-files.txt" | grep -v '^$' | sort -u >"$WORK/listed.paths" || true
    awk 'FILENAME == ARGV[1] { no[$0 ""] = 1; next } !(($0 "") in no)' "$WORK/want.paths" "$WORK/listed.paths" >"$WORK/passes.paths"
  fi
  if [ ! -s "$WORK/want.paths" ] && [ "$NO_FILE_CHECK" = 1 ]; then
    note "--no-file-check: the restored files were not checked against the file table, and the damaged-file list in $dest names no path to restore: it is kept."
    print_id_map "$U_POS" "$U_IDM"
    UNITS_DONE=$((UNITS_DONE + 1))
    return 0
  fi
  if [ ! -s "$WORK/want.paths" ]; then
    rm -f -- "$dest/.tapectl-damaged-files.txt"
    say "File check: every file the unit's table lists is here and passes; the damaged-file list in $dest is removed: nothing is left to restore."
    print_id_map "$U_POS" "$U_IDM"
    UNITS_DONE=$((UNITS_DONE + 1))
    return 0
  fi
  say_run_limits
  restore_paths "$dest" continue
}

# ---- named paths and the one repair (volume-format-v3.md 7.2 "One file", 1.7) ----

# The wanted members, in stream order: what to cut from which slices.
M_START=() M_END=() M_SLICE=() M_LAST=() M_PATH=()
NM=0

# Is slice $1 on this tape whole: planned here and at or before the seal's
# ended_after (a range holds some of a unit's slices, a short tape fewer)?
slice_here() { # <slice number>
  [ -n "${SL_HERE[$1]:-}" ]
}

# Load the rows of $1 (table rows, stream order) as the members to feed, each
# with the slice it begins in and the slice its last byte is in. A member whose
# slices are not all on this tape is not fed: its path goes to
# $WORK/absent.paths and it stays damaged.
load_members() { # <rows file>
  local slice start end path k last here
  M_START=() M_END=() M_SLICE=() M_LAST=() M_PATH=()
  NM=0
  : >"$WORK/absent.paths"
  while IFS=$'\t' read -r _ slice start end _ _ path _; do
    is_uint "$slice" && is_uint "$start" && is_uint "$end" || die "the file table of unit $(safe_str "$U_NAME") has a row with a number that is not one (path $(safe_str "$path"))"
    slice=$((10#$slice)) start=$((10#$start)) end=$((10#$end))
    if [ "$slice" -lt 1 ] || [ "$slice" -gt "$NSL" ] || [ "$start" -lt "${SL_OFF[$slice]}" ] ||
      [ "$start" -ge $((SL_OFF[$slice] + SL_SIZE[$slice])) ] || [ "$end" -le "$start" ]; then
      die "the file table and the manifest of unit $(safe_str "$U_NAME") disagree about $(safe_str "$path"): it begins at byte $start in slice $slice. The envelope is damaged; try another copy of this tape."
    fi
    last=$slice
    while [ $((SL_OFF[last] + SL_SIZE[last])) -lt "$end" ]; do
      [ "$last" -lt "$NSL" ] || die "the file table of unit $(safe_str "$U_NAME") puts $(safe_str "$path") past the last slice. The envelope is damaged; try another copy of this tape."
      last=$((last + 1))
    done
    # With no cartridge to ask for, a member whose slices are not all here is
    # listed now; otherwise it is fed, and the cartridge is asked for.
    here=1
    if [ "$GAVE_UP" = 1 ]; then
      for ((k = slice; k <= last; k++)); do slice_here "$k" || here=0; done
    fi
    if [ "$here" = 0 ]; then
      printf '%s\n' "$path" >>"$WORK/absent.paths"
      continue
    fi
    NM=$((NM + 1))
    M_START[$NM]=$start
    M_END[$NM]=$end
    M_SLICE[$NM]=$slice
    M_LAST[$NM]=$last
    M_PATH[$NM]=$path
  done <"$1"
}

# The slice a member's part is cut from, prepared and still in slice.plain.
HAVE_SLICE=""
HAVE_RC=1
need_slice() { # <slice number>: 0 when its plain cut is in slice.plain
  local k=$1
  if [ "$HAVE_SLICE" = "$k" ]; then return "$HAVE_RC"; fi
  rm -f "$WORK/slice.plain"
  HAVE_SLICE=$k
  HAVE_RC=1
  # A slice found bad by an earlier tar run is not read again.
  if awk -F'|' -v k="$k" '($1 + 0) == (k + 0) { f = 1 } END { exit f ? 0 : 1 }' "$WORK/bad.slices"; then
    return 1
  fi
  if prepare_slice "$k"; then HAVE_RC=0; fi
  return "$HAVE_RC"
}

# Give tar the part of member $1 that lies in slice $2: bytes [from, to) of
# the stream, cut from the slice's plain file by offset, as a pipeline of its
# own. Returns 1 when tar stopped reading.
emit_part() { # <member> <slice>
  local j=$1 k=$2 off from to rc=0
  off=${SL_OFF[$k]}
  from=${M_START[$j]}
  to=${M_END[$j]}
  [ "$from" -ge "$off" ] || from=$off
  [ "$to" -le $((off + SL_SIZE[k])) ] || to=$((off + SL_SIZE[k]))
  { tail -c +$((from - off + 1)) "$WORK/slice.plain" || true; } | head -c $((to - from)) >&4 || rc=$?
  return "$rc"
}

mark_left() { # <member>
  printf '%s\n' "${M_PATH[$1]}" >>"$WORK/left.out"
}

# Feed the wanted members from member $1 on, in stream order, to our standard
# output (the left of the pipe into tar). Each member is cut from its slice or
# slices by its offsets and given whole, never named to tar. A member in a long
# member's run is fed piece by piece, each piece checked first. When its first
# slice is bad the member never begins and the next one is fed; when a later
# piece is bad nothing more is fed, so tar's input ends and that run ends: the
# partial file is removed by the caller, the member is listed, and the next run
# starts at the next member.
feed_members() { # <member number>
  local j=$1 a b k
  HAVE_SLICE=""
  HAVE_RC=1
  while [ "$j" -le "$NM" ]; do
    a=${M_SLICE[$j]}
    b=${M_LAST[$j]}
    for ((k = a; k <= b; k++)); do
      if ! need_slice "$k"; then
        mark_left "$j"
        if [ "$k" -gt "$a" ]; then
          echo "resume $((j + 1)) $j" >"$WORK/feed.state"
          printf '%s\n' "$CUR" >"$WORK/feed.cur"
          return 0
        fi
        break
      fi
      if ! emit_part "$j" "$k"; then
        echo "gone $j" >"$WORK/feed.state"
        printf '%s\n' "$CUR" >"$WORK/feed.cur"
        return 0
      fi
    done
    j=$((j + 1))
  done
  echo "done" >"$WORK/feed.state"
  printf '%s\n' "$CUR" >"$WORK/feed.cur"
}

# $1 is the wanted paths (escaped, one a line, sorted) and $2 the unit's table.
# Writes $WORK/wanted.rows (the table rows of those paths, plus the first name
# of each hard link among them, in stream order), $WORK/brought.paths (the
# paths brought only as a hard link's first name) and $WORK/missing.paths (the
# wanted paths the table does not have).
resolve_paths() { # <wanted paths file> <table>
  awk -F'\t' -v rows="$WORK/wanted.rows" -v brought="$WORK/brought.paths" -v missing="$WORK/missing.paths" '
    FILENAME == ARGV[1] { want[$0 ""] = 1; next }
    /^#/ { next }
    { n++; line[n] = $0; path[n] = $7 ""; kind[n] = $1; target[n] = $8 ""; at[$7 ""] = n }
    END {
      for (i = 1; i <= n; i++) if (path[i] in want) { take[i] = 1; found[path[i]] = 1 }
      for (i = 1; i <= n; i++) if (take[i] == 1 && kind[i] == "h" && (target[i] in at) && !take[at[target[i]]]) take[at[target[i]]] = 2
      for (i = 1; i <= n; i++) if (take[i]) { print line[i] > rows; if (take[i] == 2) print path[i] > brought }
      for (w in want) if (!(w in found)) print w > missing
      close(rows); close(brought); close(missing)
    }
  ' "$1" "$2"
}

# Merge this run into the destination's damaged-file list. The list keeps what
# an earlier restore of THIS unit and version left damaged, less the paths this
# run was asked for (whole now, or still damaged: the latter are in
# damaged.paths), plus what is still damaged. Another unit's list is never
# touched (returns 2). The list is removed when nothing is left.
LIST_LEFT=0
LIST_EXISTED=0 # 1 when the destination held this unit's list before the merge
ADD_ASKED=0    # 1 for a repair under --no-file-check: what it asked for was not checked, so it goes on the list
merge_damaged_list() { # <destination> <requested and brought paths file>
  local list="$1/.tapectl-damaged-files.txt" first
  : >"$WORK/list.old"
  LIST_EXISTED=0
  if [ -f "$list" ]; then
    first=$(head -n 1 "$list" 2>/dev/null || true)
    case $first in
    "#tapectl-damaged-files"$'\t'1$'\t'"unit=$U_NAME"$'\t'"uuid=$U_UUID"$'\t'"version=$U_VER")
      sed -n '2,$p' "$list" | grep -v '^$' >"$WORK/list.old" || true
      LIST_EXISTED=1
      ;;
    *)
      note "$list is the damaged-file list of another unit or version and is left alone; this run's damaged paths are only printed."
      return 2
      ;;
    esac
  fi
  if [ "$NO_FILE_CHECK" = 1 ]; then
    # Nothing is checked, so no path is known to be whole: none leaves the
    # list. A run with the check (or the repair) removes each that passes.
    cp "$WORK/list.old" "$WORK/list.kept"
    if [ "$ADD_ASKED" = 1 ]; then cat "$2" >>"$WORK/list.kept"; fi
  else
    awk 'FILENAME == ARGV[1] { drop[$0 ""] = 1; next } !(($0 "") in drop)' "$2" "$WORK/list.old" >"$WORK/list.kept"
  fi
  sort -u "$WORK/list.kept" "$WORK/damaged.paths" >"$WORK/list.new"
  if [ -s "$WORK/list.new" ]; then
    write_damaged_list "$1" "$U_NAME" "$U_UUID" "$U_VER" "$WORK/list.new"
    LIST_LEFT=$(wc -l <"$WORK/list.new" | tr -d ' ')
  else
    rm -f -- "$list"
    LIST_LEFT=0
  fi
}

# A hard-linked name whose member was not read (its slices are on a cartridge
# not given) but whose first name is here: the name is made from the first
# name. A hard link has no data of its own, so nothing is lost by it; the
# per-file check that follows judges the result.
make_links() { # <destination>
  local dest=$1 kind p t pe te
  : >"$WORK/linked.paths"
  [ -s "$WORK/wanted.rows" ] || return 0
  while IFS=$'\t' read -r kind _ _ _ _ _ p t; do
    [ "$kind" = h ] || continue
    pe=$p te=$t
    unesc "$p"
    p=$REPLY
    unesc "$t"
    t=$REPLY
    if [ -e "$dest/$p" ] || [ -L "$dest/$p" ] || [ ! -f "$dest/$t" ]; then continue; fi
    mkdir -p -- "$(dirname -- "$dest/$p")" 2>/dev/null || continue
    if ln -- "$dest/$t" "$dest/$p" 2>/dev/null; then
      note "made the hard-linked name $(visible_text "$pe") from its first name $(visible_text "$te") (its own member was not read)."
      printf '%s\n' "$pe" >>"$WORK/linked.paths"
    fi
  done <"$WORK/wanted.rows"
}

# Restore the paths of $WORK/want.paths from the unit open (unit_open) into
# $1, by the file table; $2 is "repair" for --paths-from, "path" for --path.
restore_paths() { # <destination> <how>
  local dest=$1 how=$2 npaths nm k p files_bytes biggest=0 ndam
  mkdir -p -- "$dest" || die "cannot create $(safe_str "$dest")"
  sort -u "$WORK/want.paths" >"$WORK/want.sorted"
  npaths=$(wc -l <"$WORK/want.sorted" | tr -d ' ')
  : >"$WORK/wanted.rows"
  : >"$WORK/brought.paths"
  : >"$WORK/missing.paths"
  resolve_paths "$WORK/want.sorted" "$U_TABLE"
  if [ -s "$WORK/missing.paths" ]; then
    echo "FATAL: $(wc -l <"$WORK/missing.paths" | tr -d ' ') path(s) are not in the file table of unit $(safe_str "$U_NAME") version $U_VER on this tape (a path is written as '$0 --list' shows it, relative to the unit's root):" >&2
    sort "$WORK/missing.paths" | head -n 10 | while IFS= read -r p; do printf '         %s\n' "$(safe_str "$p")" >&2; done
    echo "       Nothing has been read from a slice and nothing was changed." >&2
    exit 1
  fi
  if [ "$how" = path ] && [ "$OVERWRITE" = 0 ]; then
    while IFS= read -r p; do
      unesc "$p"
      if [ -e "$dest/$REPLY" ] || [ -L "$dest/$REPLY" ]; then
        WANT_PATH="$p" awk -F'\t' '($7 "") == (ENVIRON["WANT_PATH"] "") && $1 == "d" { f = 1 } END { exit f ? 0 : 1 }' "$U_TABLE" ||
          die "$(visible_text "$p") is already in $(safe_str "$dest"). Give --overwrite to replace the named files (nothing has been read from a slice)."
      fi
    done <"$WORK/want.sorted"
  fi
  # A folder names its own entry only: its mode, owner and times, nothing under it.
  nm=$(awk -F'\t' 'FILENAME == ARGV[1] { want[$0 ""] = 1; next } /^#/ { next } $1 == "d" && (($7 "") in want) { n++ } END { print n + 0 }' "$WORK/want.sorted" "$U_TABLE")
  if [ "$nm" -gt 0 ]; then
    note "$nm of the paths is a folder: a folder names only its own entry (its mode, owner and times), never what lies under it. Name each file under it to restore those."
  fi
  if [ "$how" = continue ] && [ -s "$WORK/brought.paths" ]; then
    # A first name that is already here and passes is not fed again.
    : >"$WORK/skip.paths"
    while IFS= read -r p; do
      unesc "$p"
      if [ -f "$dest/$REPLY" ]; then printf '%s\n' "$p" >>"$WORK/skip.paths"; fi
    done <"$WORK/brought.paths"
    awk -F'\t' 'FILENAME == ARGV[1] { skip[$0] = 1; next } !(($7 "") in skip)' "$WORK/skip.paths" "$WORK/wanted.rows" >"$WORK/wanted.rows.new"
    mv "$WORK/wanted.rows.new" "$WORK/wanted.rows"
  fi
  load_members "$WORK/wanted.rows"
  files_bytes=$(awk -F'\t' '$1 == "f" { s += $5 } END { printf "%.0f", s }' "$WORK/wanted.rows")
  for ((k = 1; k <= NSL; k++)); do
    if is_uint "${SL_SIZE[$k]}" && [ "${SL_SIZE[$k]}" -gt "$biggest" ]; then biggest=${SL_SIZE[$k]}; fi
    if is_uint "${SL_ENC[$k]}" && [ "${SL_ENC[$k]}" -gt "$biggest" ]; then biggest=${SL_ENC[$k]}; fi
  done
  check_space "the named paths of unit $(safe_str "$U_NAME") need their files, and the work directory one slice twice" "$dest" "$files_bytes" "$WORK" $((2 * biggest + BLOCK))
  say ">>> Unit $(safe_str "$U_NAME") version $U_VER: $npaths path(s) by the file table, into $dest"
  if [ -s "$WORK/brought.paths" ]; then
    note "$(wc -l <"$WORK/brought.paths" | tr -d ' ') hard link(s) among them are made from the first name of the file, which is restored first and is checked too."
  fi
  if [ -s "$WORK/absent.paths" ]; then
    note "$(wc -l <"$WORK/absent.paths" | tr -d ' ') of the paths lie in slices this tape does not hold whole (a unit across cartridges, or a tape that ended early): they are not read here and stay on the list."
  fi
  : >"$WORK/bad.slices"
  : >"$WORK/left.out"
  rm -f "$WORK/fatal.flag"
  INFLIGHT_DEST=$dest
  INFLIGHT_TABLE=$U_TABLE
  local resume=1 state="" tarrc
  while [ "$resume" -le "$NM" ]; do
    rm -f "$WORK/feed.state" "$WORK/tar.rc" "$WORK/feed.cur"
    tar_open "$dest"
    feed_members "$resume" || true
    tar_close
    if [ -e "$WORK/fatal.flag" ]; then exit 1; fi
    CUR=$(cat "$WORK/feed.cur" 2>/dev/null || true)
    state=$(cat "$WORK/feed.state" 2>/dev/null || echo "gone ?")
    tarrc=$(cat "$WORK/tar.rc" 2>/dev/null || echo "?")
    if [ "$tarrc" != 0 ]; then
      tar_status_note "$tarrc"
    fi
    case $state in
    done) break ;;
    resume*)
      # shellcheck disable=SC2086
      set -- $state
      resume=$2
      unesc "${M_PATH[$3]}"
      rm -f -- "${dest:?}/${REPLY:?}" 2>/dev/null || true
      note "a long file's run broke at a bad slice; its partial file was removed and the file is listed. Reading on with the next path."
      ;;
    *)
      # shellcheck disable=SC2086
      set -- $state
      warn "tar stopped reading at path number ${2:-?} (tar: $(head -n 2 "$WORK/tar.err" 2>/dev/null | tr '\n' ' ')); the paths from there on were not given to it."
      for ((k = ${2:-1}; k <= NM; k++)); do mark_left "$k"; done
      break
      ;;
    esac
  done
  if [ -s "$WORK/absent.slices" ] && [ ! -s "$WORK/absent.paths" ]; then
    note "$(wc -l <"$WORK/left.out" | tr -d ' ') of the paths lie in slices this tape does not hold whole (a unit across cartridges, or a tape that ended early): they were not read and stay on the list."
  fi
  local nbad
  nbad=$(wc -l <"$WORK/bad.slices" | tr -d ' ')
  if [ "$nbad" -gt 0 ]; then
    warn "$nbad slice(s) of unit $(safe_str "$U_NAME") were left out; the paths in them are listed as damaged:"
    awk -F'|' '{ printf "         slice %s: %s\n", $1, $2 }' "$WORK/bad.slices" >&2
  fi
  make_links "$dest"
  : >"$WORK/failed.paths"
  if [ "$NO_FILE_CHECK" = 0 ]; then
    file_check "$WORK/wanted.rows" "$dest" "$WORK/failed.paths"
    if [ "$CHECK_ROWS" -eq 0 ]; then
      say "File check: the named paths hold no regular file, so none was checked."
    elif [ "$CHECK_FAILED" -eq 0 ]; then
      say "File check: all $CHECK_ROWS regular file(s) named match the sha256 in the unit's file table."
    else
      say "File check: $CHECK_FAILED of $CHECK_ROWS regular file(s) named are missing or differ from the file table."
    fi
  else
    note "--no-file-check: the restored files were not checked against the file table."
    # A hard-linked name whose first name is not here is neither made nor
    # checked: nothing else would list it.
    local hk hp
    while IFS=$'\t' read -r hk _ _ _ _ _ hp _; do
      [ "$hk" = h ] || continue
      unesc "$hp"
      if [ ! -e "$dest/$REPLY" ] && [ ! -L "$dest/$REPLY" ]; then printf '%s\n' "$hp" >>"$WORK/failed.paths"; fi
    done <"$WORK/wanted.rows"
  fi
  INFLIGHT_TABLE=""
  # A name made from its first name is whole if the check passed it (a name it
  # failed is in failed.paths and stays).
  sort -u "$WORK/left.out" "$WORK/absent.paths" >"$WORK/notread.paths"
  if [ -s "$WORK/linked.paths" ]; then
    awk 'FILENAME == ARGV[1] { made[$0] = 1; next } !(($0 "") in made)' "$WORK/linked.paths" "$WORK/notread.paths" >"$WORK/notread.new"
    mv "$WORK/notread.new" "$WORK/notread.paths"
  fi
  sort -u "$WORK/notread.paths" "$WORK/failed.paths" >"$WORK/damaged.paths"
  ndam=$(wc -l <"$WORK/damaged.paths" | tr -d ' ')
  sort -u "$WORK/want.sorted" "$WORK/brought.paths" >"$WORK/asked.paths"
  if [ "$how" = continue ]; then cat "$WORK/passes.paths" >>"$WORK/asked.paths"; fi
  local merged=0 counted=$ndam
  ADD_ASKED=0
  if [ "$NO_FILE_CHECK" = 1 ] && [ "$how" = repair ]; then ADD_ASKED=1; fi
  merge_damaged_list "$dest" "$WORK/asked.paths" || merged=$?
  if [ "$NO_FILE_CHECK" = 1 ] && [ "$how" != path ] && [ "$merged" = 0 ] && [ "$LIST_LEFT" -gt "$ndam" ]; then
    counted=$LIST_LEFT
  fi
  if [ "$ndam" -gt 0 ]; then
    if [ "$merged" = 0 ]; then
      print_damaged "$WORK/damaged.paths" "$ndam" "$dest/.tapectl-damaged-files.txt"
    else
      say "DAMAGED: $ndam path(s) were left out or failed the check:"
      head -n 41 "$WORK/damaged.paths" | sed 's/^/  /'
    fi
    ABSENT_HOW=$how ABSENT_DEST=$dest
    report_absent "$WORK/damaged.paths"
  fi
  DAMAGED_TOTAL=$((DAMAGED_TOTAL + counted))
  if [ "$NO_FILE_CHECK" = 1 ] && [ "$how" != path ] && [ "$merged" = 0 ] && [ "$LIST_LEFT" -gt 0 ]; then
    if [ "$LIST_EXISTED" = 1 ]; then
      say "The damaged-file list in $dest is kept with $LIST_LEFT path(s): --no-file-check judges nothing, so no path leaves it. The same run without --no-file-check (or the repair, --paths-from, on a tape that holds them) removes each path that passes."
    else
      say "The damaged-file list in $dest is written with $LIST_LEFT path(s): --no-file-check checked none of them, so none is known whole. The same run without --no-file-check removes each path that passes."
    fi
  elif [ "$ndam" -eq 0 ] && [ "$how" != path ] && [ "$merged" = 0 ]; then
    if [ "$LIST_LEFT" -eq 0 ]; then
      if [ "$LIST_EXISTED" = 1 ]; then say "The damaged-file list in $dest is removed: nothing is left to repair."; fi
    else
      say "The damaged-file list in $dest is kept with the $LIST_LEFT path(s) this run did not name."
    fi
  fi
  print_id_map "$U_POS" "$U_IDM"
  UNITS_DONE=$((UNITS_DONE + 1))
}

# --restore --unit U --path P [--path P2 ...]
# A path is given as --list prints it, and as the damaged-file list writes it:
# escaped (volume-format-v3.md 1.5), so a backslash in a name is typed \\, a TAB
# \t, a newline \n, a carriage return \r, and a byte that is not UTF-8 \xHH. It
# is matched against the file table's own text, with no conversion.
do_paths() {
  local p
  : >"$WORK/want.paths"
  for p in "${PATHS[@]}"; do
    case $p in
    *$'\t'* | *$'\n'* | *$'\r'*)
      die_usage "--path takes a path as --list prints it: a TAB, a newline or a carriage return in a name is written \\t, \\n or \\r, and a backslash \\\\"
      ;;
    esac
    p=${p#./}
    while [ "${#p}" -gt 1 ] && [ "${p%/}" != "$p" ]; do p=${p%/}; done
    [ -n "$p" ] || p=.
    printf '%s\n' "$p" >>"$WORK/want.paths"
  done
  unit_open "$(head -n 1 "$WORK/chosen.txt")"
  restore_paths "$TO" path
}

# The list --paths-from names: its header says which unit and version it is
# for; its other lines are the paths (escaped), written to $WORK/want.paths.
LIST_UNIT="" LIST_UUID="" LIST_VER=""
read_repair_list() {
  local first tag fmt f1 f2 f3
  [ -f "$PATHS_FROM" ] && [ -r "$PATHS_FROM" ] || die "--paths-from $(safe_str "$PATHS_FROM"): the file does not exist or cannot be read"
  first=$(head -n 1 "$PATHS_FROM" 2>/dev/null || true)
  IFS=$'\t' read -r tag fmt f1 f2 f3 <<<"$first" || true
  [ "${tag:-}" = "#tapectl-damaged-files" ] ||
    die "--paths-from $(safe_str "$PATHS_FROM") is not a damaged-file list: its first line is not '#tapectl-damaged-files' (the list a restore writes in the restored directory as .tapectl-damaged-files.txt). To restore named paths, give each with --path under --unit."
  [ "${fmt:-}" = 1 ] || die "the damaged-file list $(safe_str "$PATHS_FROM") is of format $(safe_str "${fmt:-?}"), which this RESTORE.sh does not know."
  LIST_UNIT=${f1:-}
  LIST_UUID=${f2:-}
  LIST_VER=${f3:-}
  case $LIST_UNIT in unit=?*) LIST_UNIT=${LIST_UNIT#unit=} ;; *) LIST_UNIT="" ;; esac
  case $LIST_UUID in uuid=?*) LIST_UUID=${LIST_UUID#uuid=} ;; *) LIST_UUID="" ;; esac
  case $LIST_VER in version=?*) LIST_VER=${LIST_VER#version=} ;; *) LIST_VER="" ;; esac
  { [ -n "$LIST_UNIT" ] && [ -n "$LIST_UUID" ] && is_uint "$LIST_VER"; } ||
    die "the damaged-file list $(safe_str "$PATHS_FROM") has a header this RESTORE.sh cannot read (it names the unit, its uuid and its version)."
  LIST_VER=$((10#$LIST_VER))
  sed -n '2,$p' "$PATHS_FROM" | grep -v '^$' >"$WORK/want.paths" || true
}

# Choose the unit the list is for: the row with its uuid and version, by what
# the envelope says. Another version or another unit is refused, naming it,
# and the list is left as it is.
choose_repair_unit() {
  LIST_ID="$LIST_UUID" awk -F'|' -v ver="$LIST_VER" '
    $9 == "" { next }
    ($6 "") == (ENVIRON["LIST_ID"] "") && ($4 + 0) == (ver + 0) { if (!n++ || ($5 + 0) > best) { best = $5 + 0; row = $0 } }
    END { if (n) print row }
  ' "$WORK/units.txt" >"$WORK/chosen.txt"
  [ -s "$WORK/chosen.txt" ] && return 0
  local others same
  others=$(LIST_ID="$LIST_UUID" awk -F'|' '$9 != "" && ($6 "") == (ENVIRON["LIST_ID"] "") { printf "%s ", $4 }' "$WORK/units.txt")
  same=$(LIST_NAME="$LIST_UNIT" awk -F'|' '$9 != "" && ($3 "") == (ENVIRON["LIST_NAME"] "") { n++ } END { print n + 0 }' "$WORK/units.txt")
  if [ -n "$others" ]; then
    die "this tape holds unit $(safe_str "$LIST_UNIT") only as version(s) ${others% }, and the list is for version $LIST_VER. Another version is not a copy of the files the list names: the list is left as it is. Load a tape that holds version $LIST_VER."
  elif [ "$same" -gt 0 ]; then
    die "this tape holds a unit named $(safe_str "$LIST_UNIT"), but it is another unit than the list's (its uuid is not $(safe_str "$LIST_UUID")): the list is left as it is. Load a tape that holds the same unit."
  fi
  die "unit $(safe_str "$LIST_UNIT") (uuid $(safe_str "$LIST_UUID")) is not in the envelope(s) your key opened on this tape: the list is left as it is. Is it the right key, and the right tape?"
}

# --restore --paths-from LIST: the one repair.
do_repair() {
  read_repair_list
  if [ ${#UNITS[@]} -eq 1 ] && [ "${UNITS[0]}" != "$LIST_UNIT" ]; then
    die_usage "--unit ${UNITS[0]} is not the list's unit ($(safe_str "$LIST_UNIT")): give no --unit, or the list's"
  fi
  if [ -n "$VERSION" ] && [ "$VERSION" != "$LIST_VER" ]; then
    die_usage "--version $VERSION is not the list's version ($LIST_VER): give no --version, or the list's"
  fi
  choose_repair_unit
  unit_open "$(head -n 1 "$WORK/chosen.txt")"
  if [ ! -s "$WORK/want.paths" ]; then
    say "The damaged-file list names no path: nothing to restore."
    return 0
  fi
  restore_paths "$TO" repair
}

do_restore() {
  tar_init
  if [ ${#PATHS[@]} -gt 0 ] || [ -n "$PATHS_FROM" ]; then
    # A named path or a repair goes into a directory a restore made.
    if [ -e "$TO" ] && [ ! -d "$TO" ]; then die "--to $(safe_str "$TO") exists and is not a directory"; fi
  else
    check_destination
  fi
  # The work directory is made once the destination is known to be usable.
  make_work_for_restore
  establish
  envelope_space_check
  open_envelopes
  collect_units
  local n row name dest
  if [ -n "$PATHS_FROM" ]; then
    do_repair
  elif [ ${#PATHS[@]} -gt 0 ]; then
    choose_units
    [ "$(wc -l <"$WORK/chosen.txt" | tr -d ' ')" -eq 1 ] || die "--path names files of one unit: more than one unit matched"
    do_paths
  else
    choose_units
    n=$(wc -l <"$WORK/chosen.txt" | tr -d ' ')
    RUN_UNITS=$n
    # A unit directory that is not empty and holds no list is refused before
    # any unit is touched, while the lists of its siblings are still there to
    # be named (a unit continued first would have removed its own).
    if [ "$n" -gt 1 ] && [ "$OVERWRITE" = 0 ]; then
      while IFS= read -r row; do
        IFS='|' read -r _ _ name _ <<<"$row"
        dest="$TO/$name"
        if [ ! -f "$dest/.tapectl-damaged-files.txt" ] && [ -n "$(ls -A -- "$dest" 2>/dev/null)" ]; then
          die "$(safe_str "$dest") is not empty and holds no damaged-file list, so it is not an earlier run of this unit that stopped short. $(units_with_lists)Restore into an empty or new directory, or give --overwrite to replace files that are there."
        fi
      done <"$WORK/chosen.txt"
    fi
    # fd 3, so that nothing a unit's restore runs can read the list from stdin.
    while IFS= read -r row <&3; do
      IFS='|' read -r _ _ name _ <<<"$row"
      if [ "$n" -eq 1 ]; then dest=$TO; else dest="$TO/$name"; fi
      [ "$QUIT" = 0 ] || break
      restore_one "$row" "$dest"
    done 3<"$WORK/chosen.txt"
  fi
  say "The tar used: ${TAR_NAME:-unknown}. What it does not restore: $TAR_LOSES."
  if [ "$QUIT" = 1 ]; then
    say "RESTORE STOPPED at your request: the files restored and checked so far stay, a file tar had begun was removed, and what was left out is on the damaged-file list."
    return 1
  fi
  if [ "$DAMAGED_TOTAL" -gt 0 ]; then
    say "RESTORE INCOMPLETE: $DAMAGED_TOTAL path(s) are damaged or left out; restore them from another copy (the list is the one repair: give it to --paths-from on a tape that holds the same version)."
    return 1
  fi
  if [ "$TAR_FAILED" = 1 ]; then
    say "RESTORE INCOMPLETE: tar ended with a status other than 0, and --no-file-check runs no check to say what it restored. Run again without --no-file-check to learn which files are whole."
    return 1
  fi
  if [ ${#PATHS[@]} -gt 0 ] || [ -n "$PATHS_FROM" ]; then
    say "RESTORE COMPLETE: the named path(s) of unit $(safe_str "$U_NAME") are restored into $TO."
  else
    say "RESTORE COMPLETE: $UNITS_DONE unit(s) restored into $TO."
  fi
}

make_work_for_restore() {
  mkdir -p -- "$TO" 2>/dev/null || die "cannot create --to $(safe_str "$TO")"
  make_work
}

# ---- the cross-volume index (--where, --find; volume-format-v3.md 1.5) ----

# Each envelope carries, as its last members, index/units.tsv (the tenant's
# units: every version and every tape holding it; the operator's: every
# tenant's) and, in a tenant envelope, index/paths.tsv (every path of those
# versions). Both are tab-separated text with a header naming the columns, and
# are read here with awk. The index is as of the moment its own tape was
# written: it never names a tape written after it, and a tape it names may
# since have been retired or erased. A column is found by its header name and a
# column this script does not know is ignored.

# The functions the index programs share: the escapes of 1.5 decoded by ONE
# left-to-right scan (never chained substitutions, which turn the escaped text
# of a backslash followed by t into a TAB), a printable form of a value (a path
# may hold a real newline, which would break one-row-per-line output and could
# forge a row: only what is printed is made visible; the match is on the
# decoded text), a human size, and the header-name column lookup.
AWK_INDEX_COMMON='
function unesc(s,    out, i, n, c, d) {
  if (index(s, "\\") == 0) return s
  out = ""
  n = length(s)
  for (i = 1; i <= n; i++) {
    c = substr(s, i, 1)
    if (c == "\\" && i < n) {
      d = substr(s, i + 1, 1)
      if (d == "t")  { out = out "\t"; i++; continue }
      if (d == "n")  { out = out "\n"; i++; continue }
      if (d == "r")  { out = out "\r"; i++; continue }
      if (d == "\\") { out = out "\\"; i++; continue }
    }
    out = out c
  }
  return out
}
function show(s,    out, i, n, c) {
  s = unesc(s)
  if (s !~ /[[:cntrl:]]/) return s
  out = ""
  n = length(s)
  for (i = 1; i <= n; i++) {
    c = substr(s, i, 1)
    if (c == "\t") out = out "\\t"
    else if (c == "\n") out = out "\\n"
    else if (c == "\r") out = out "\\r"
    else if (c ~ /[[:cntrl:]]/) out = out "?"
    else out = out c
  }
  return out
}
function human(b) {
  if (b !~ /^[0-9]+$/) return "size not known"
  if (b + 0 >= 1073741824) return sprintf("%.1f GiB", b / 1073741824)
  if (b + 0 >= 1048576) return sprintf("%.1f MiB", b / 1048576)
  return sprintf("%.1f KiB", b / 1024)
}
function header(line, names,    n, f, i) {
  n = split(line, f, "\t")
  for (i = 1; i <= n; i++) names[f[i]] = i
  return n
}
function val(f, names, key) {
  if (!(key in names)) return ""
  return f[names[key]]
}
function nk(s) {
  return (s == "") ? "not known" : s
}
function range_here(row, names,    f, line, q, key) {
  if (!range_read) {
    range_read = 1
    f = ENVIRON["INDEX_RANGE"]
    if (f != "") {
      while ((getline line < f) > 0) {
        split(line, q, "\t")
        rangev[q[1] "\t" q[2]] = q[3] "\t" q[4]
      }
      close(f)
    }
  }
  if (val(row, names, "state") != "this-tape") return ""
  key = val(row, names, "unit_uuid") "\t" val(row, names, "version")
  return (key in rangev) ? rangev[key] : ""
}
function partial(first, last, count) {
  if (first !~ /^[0-9]+$/ || last !~ /^[0-9]+$/ || count !~ /^[0-9]+$/) return 0
  return (first + 0 != 1 || last + 0 != count + 0)
}
function note_partial(    f) {
  f = ENVIRON["INDEX_PARTIAL"]
  if (f == "" || partial_noted) return
  partial_noted = 1
  print "1" >> f
  close(f)
}
function holder(row, names,    state, sealed, first, last, count, st, s, run, q, part) {
  state = show(val(row, names, "state"))
  sealed = show(val(row, names, "sealed_at"))
  first = show(val(row, names, "first_slice"))
  last = show(val(row, names, "last_slice"))
  count = show(val(row, names, "num_slices"))
  run = range_here(row, names)
  if (run != "") { split(run, q, "\t"); first = q[1]; last = q[2] }
  part = (run != "" || partial(first, last, count))
  if (state == "this-tape") st = "this tape"
  else if (state == "sealed") st = (sealed == "") ? "sealed" : "sealed " sealed
  else if (state == "quarantined") {
    st = "QUARANTINED: a verify proved this medium bad, it may still read in part"
    if (sealed != "") st = st "; sealed " sealed
  }
  else st = (state == "") ? "state not known" : "state " state
  if (part) { st = st ", a range: not the whole version"; note_partial() }
  s = "    tape " nk(show(val(row, names, "volume"))) "  (" st ")"
  if (first != "" && last != "") s = s "  slices " first "-" last (count == "" ? "" : " of " count)
  else if (run != "") s = s "  no slice of it is on this tape (it ended before them)"
  s = s "\n        volume uuid " nk(show(val(row, names, "volume_uuid")))
  s = s "; cartridge serial " nk(show(val(row, names, "cartridge_serial")))
  s = s "; generation " nk(show(val(row, names, "generation")))
  return s
}
'

# The versions an envelope MANIFEST.toml lists that this tape holds only some
# of (a range; every unit continues, so a version can end on this tape's seal),
# by which of each block's slices the tape holds: a slice with no
# tape_position is on another tape, one whose tape_position is after k (the
# seal's ended_after, or empty) is not on this one, the others are here. Each
# such version is appended to the file RANGE_OUT as: uuid TAB version TAB first
# TAB last TAB slices (the first and last slice number here, empty when none,
# and how many slices the block lists). uuid and snapshot_version are read in
# the [[units]] head only.
AWK_RANGE_VERSIONS='
function slice_done() {
  if (in_s && num != "") {
    n++
    if (tp == "") away++
    else if (k != "" && tp + 0 > k + 0) past++
    else { here++; if (first == "") first = num; last = num }
  }
  num = ""; tp = ""
}
function done_block() {
  slice_done()
  if (inblk && id != "" && away + past > 0 && rf != "") printf "%s\t%s\t%s\t%s\t%s\n", id, v, first, last, n >> rf
  inblk = 0; id = ""; v = ""; in_s = 0
  n = 0; here = 0; away = 0; past = 0; first = ""; last = ""
}
BEGIN { rf = ENVIRON["RANGE_OUT"] }
/^\[\[units\]\]/ { done_block(); inblk = 1; in_head = 1; next }
/^\[\[units\.slices\]\]/ { slice_done(); in_head = 0; in_s = inblk; next }
/^\[/ { slice_done(); in_head = 0; in_s = 0 }
in_head && /^uuid = / { id = $3; gsub(/"/, "", id) }
in_head && /^snapshot_version = / { v = $3 + 0 }
in_s && /^number = / { num = $3 }
in_s && /^tape_position = / { tp = $3 }
END { done_block(); if (rf != "") close(rf) }
'

# --where UNIT over one index/units.tsv: every row for the unit, grouped by
# version, with the slice range and count, the bytes, and each holding tape.
# The unit name comes in through WHERE_UNIT in the environment, never -v, which
# would process the backslashes of an escaped name; it is compared with the
# DECODED unit column. Every unit name seen is appended to the file WHERE_KNOWN,
# so a miss can list what the index does hold. WHERE_TENANT names the tenant of
# a tenant envelope, whose rows carry no tenant column. Exit status 0 when the
# unit has rows, 1 when it has none, 2 when the file lacks the unit or version
# column.
AWK_INDEX_WHERE='
BEGIN {
  FS = "\t"
  want = ENVIRON["WHERE_UNIT"]
  known = ENVIRON["WHERE_KNOWN"]
  tenant = ENVIRON["WHERE_TENANT"]
}
NR == 1 {
  header($0, h)
  if (!("unit" in h) || !("version" in h)) { bad = 1; exit 2 }
  next
}
{
  split($0, f, "\t")
  u = unesc(val(f, h, "unit"))
  if (!(u in seen)) { seen[u] = 1; print show(val(f, h, "unit")) >> known }
  if (u != want) next
  found = 1
  t = ("tenant" in h) ? val(f, h, "tenant") : tenant
  if (!started || t != lastt) {
    if (started) print ""
    print "Unit " show(val(f, h, "unit")) (t == "" ? "" : " (tenant " show(t) ")") ":"
    started = 1; lastt = t; lastv = ""; vseen = 0
  }
  v = val(f, h, "version")
  if (!vseen || v != lastv) {
    print ""
    print "  Version " show(v) ": " show(val(f, h, "num_slices")) " slice(s), " human(val(f, h, "version_bytes")) " (" show(val(f, h, "version_bytes")) " bytes); unit uuid " nk(show(val(f, h, "unit_uuid")))
    lastv = v; vseen = 1
  }
  print holder(f, h)
}
END {
  if (bad) exit 2
  exit(found ? 0 : 1)
}
'

# --where --all over one index/units.tsv: EVERY unit the index names, with no
# limit, so an heir can enumerate a box. The whole file is read before anything
# is printed, and no row order is relied on: a unit deleted and made again under
# one name is two units with two uuids whose rows interleave, so a unit is keyed
# by tenant, name AND unit uuid, in the order first seen, and its newest version
# is the highest by NUMBER (never by text, where 9 follows 10). For each unit it
# prints the newest version, whether this tape holds it whole, and every version
# with each tape that holds it. ALL_TENANT names the tenant of a tenant
# envelope. Two files are written for the caller, who prints a summary before the
# listing: ALL_COUNTS gets "UNITS ON_THIS_TAPE", and ALL_ABSENT gets, appended,
# one line for each unit whose newest version this tape does not hold whole,
# with the tapes that do. Exit status 0 when the file has units, 1 when it has
# none, 2 when it lacks the unit or version column.
AWK_INDEX_ALL='
function holders_of(k,    j, r, s, lab, st, run, q) {
  s = ""
  for (j = 1; j <= rows[k]; j++) {
    split(row[k, j], r, "\t")
    lab = nk(show(val(r, h, "volume")))
    st = val(r, h, "state")
    run = range_here(r, h)
    if (run != "") {
      split(run, q, "\t")
      lab = lab (q[1] == "" ? " (this tape: no slice of it)" : " (this tape: slices " q[1] "-" q[2] " only)")
    }
    else if (st == "this-tape") lab = lab " (this tape)"
    else if (st == "quarantined") lab = lab " (QUARANTINED)"
    if (st != "this-tape" && partial(val(r, h, "first_slice"), val(r, h, "last_slice"), val(r, h, "num_slices")))
      lab = lab " (slices " show(val(r, h, "first_slice")) "-" show(val(r, h, "last_slice")) " only)"
    s = s (s == "" ? "" : ", ") lab
  }
  return (s == "") ? "no tape the index lists" : s
}
BEGIN {
  FS = "\t"
  tenant = ENVIRON["ALL_TENANT"]
  counts = ENVIRON["ALL_COUNTS"]
  absent = ENVIRON["ALL_ABSENT"]
}
NR == 1 {
  header($0, h)
  if (!("unit" in h) || !("version" in h)) { bad = 1; exit 2 }
  next
}
$0 == "" { next }
{
  split($0, f, "\t")
  t = ("tenant" in h) ? val(f, h, "tenant") : tenant
  id = val(f, h, "unit_uuid")
  g = unesc(t) SUBSEP unesc(val(f, h, "unit")) SUBSEP id
  if (!(g in gnum)) { gnum[g] = ++ng; gten[ng] = t; gunit[ng] = val(f, h, "unit") }
  n = gnum[g]
  v = val(f, h, "version") + 0
  k = n SUBSEP v
  if (!(k in rows)) {
    rows[k] = 0
    nver[n]++
    ver[n, nver[n]] = v
    vtext[k] = val(f, h, "version")
    vslices[k] = val(f, h, "num_slices")
    vbytes[k] = val(f, h, "version_bytes")
    vuuid[k] = id
  }
  rows[k]++
  row[k, rows[k]] = $0
  if (val(f, h, "state") == "this-tape" && range_here(f, h) == "") here[k] = 1
}
END {
  if (bad) exit 2
  for (n = 1; n <= ng; n++) {
    c = nver[n]
    for (i = 1; i <= c; i++) vs[i] = ver[n, i]
    for (i = 2; i <= c; i++) {
      x = vs[i]
      for (j = i - 1; j >= 1 && vs[j] > x; j--) vs[j + 1] = vs[j]
      vs[j + 1] = x
    }
    newest = vs[c]
    kn = n SUBSEP newest
    who = (gten[n] == "") ? "" : " (tenant " show(gten[n]) ")"
    onhere = (kn in here)
    if (onhere) tothere++
    print "Unit " show(gunit[n]) who ": newest version " show(vtext[kn]) ", " (onhere ? "on this tape" : "NOT on this tape (whole)")
    if (!onhere) printf "    %s%s, version %s, unit uuid %s: %s\n", show(gunit[n]), who, show(vtext[kn]), nk(show(vuuid[kn])), holders_of(kn) >> absent
    for (i = 1; i <= c; i++) {
      k = n SUBSEP vs[i]
      if (i > 1) print ""
      print "  Version " show(vtext[k]) (vs[i] == newest ? " (newest)" : "") ": " show(vslices[k]) " slice(s), " human(vbytes[k]) " (" show(vbytes[k]) " bytes); unit uuid " nk(show(vuuid[k]))
      for (j = 1; j <= rows[k]; j++) {
        split(row[k, j], r, "\t")
        print holder(r, h)
      }
    }
    print ""
  }
  print ng + 0, tothere + 0 > counts
  close(counts)
  close(absent)
  exit(ng > 0 ? 0 : 1)
}
'

# --find TEXT over one tenant envelope: FIND_UNITS is its index/units.tsv (or
# /dev/null), the program input its index/paths.tsv. Prints the rows whose
# DECODED path contains the literal text in FIND_TEXT (index(), not a pattern),
# at most FIND_LIMIT of them, each as unit, version, kind, size and path, then
# the tapes holding each printed version. FIND_WHO names the envelope. The
# numbers printed and matched are written to the file FIND_COUNTS, so the
# caller can say how many matches it did not print and share one limit across
# envelopes. Exit status 2 when paths.tsv lacks a unit, version or path column.
AWK_INDEX_FIND='
BEGIN {
  FS = "\t"
  needle = ENVIRON["FIND_TEXT"]
  limit = ENVIRON["FIND_LIMIT"] + 0
  uf = ENVIRON["FIND_UNITS"]
  cf = ENVIRON["FIND_COUNTS"]
  who = ENVIRON["FIND_WHO"]
  if ((getline line < uf) > 0) {
    header(line, uh)
    while ((getline line < uf) > 0) {
      split(line, r, "\t")
      k = unesc(val(r, uh, "unit")) SUBSEP val(r, uh, "version")
      nh[k]++
      hold[k, nh[k]] = line
    }
  }
  close(uf)
}
NR == 1 {
  header($0, ph)
  if (!("unit" in ph) || !("version" in ph) || !("path" in ph)) { bad = 1; exit 2 }
  next
}
{
  p = $(ph["path"])
  if (index(unesc(p), needle) == 0) next
  total++
  if (shown >= limit) next
  shown++
  if (shown == 1) print "In the index of " who ":"
  us = $(ph["unit"])
  vs = $(ph["version"])
  printf "  %s  v%s  %s  %s bytes  %s\n", show(us), show(vs), (("kind" in ph) ? show($(ph["kind"])) : "?"), (("size_bytes" in ph) ? show($(ph["size_bytes"])) : "?"), show(p)
  k = unesc(us) SUBSEP vs
  if (!(k in want)) { want[k] = 1; order[++nord] = k; dunit[k] = show(us); dver[k] = show(vs) }
}
END {
  if (bad) exit 2
  for (i = 1; i <= nord; i++) {
    k = order[i]
    print ""
    print "  Tapes holding " dunit[k] " version " dver[k] ":"
    if (nh[k] + 0 == 0) print "    none is listed in this envelope"
    for (j = 1; j <= nh[k]; j++) {
      split(hold[k, j], r, "\t")
      print holder(r, uh)
    }
  }
  print shown + 0, total + 0 > cf
  close(cf)
}
'

INDEX_OPENED=0
INDEX_HAD=0

# The versions an opened envelope lists that this tape holds only some of, to
# $WORK/range.versions (see AWK_RANGE_VERSIONS). The index programs never print
# the loaded tape as a holder of the whole of such a version.
note_range_versions() { # <envelope dir>
  [ -f "$1/MANIFEST.toml" ] || return 0
  RANGE_OUT="$WORK/range.versions" awk -v k="$ENDED_AFTER" "$AWK_RANGE_VERSIONS" "$1/MANIFEST.toml"
}

# The line after a listing that printed a holder of only some slices of a
# version.
note_range_holders() {
  [ -s "$WORK/partial.flag" ] || return 0
  say "A holder marked 'a range' holds only the slices it names of that version, and is not a copy of it"
  say "on its own: a restore needs every slice, 1 to the last, in slice order, from the tapes that hold them."
}

index_banner() {
  say ""
  say "Index read from tape: $(safe_str "${LABEL:-<unreadable>}") (written $(safe_str "${CREATED_AT:-unknown}"))"
  say "An index is as of the moment its own tape was written. It never names a tape written"
  say "after it, and a tape it names may since have been retired or erased. The newest tape"
  say "you hold has the most complete index: if this is not it, run this on that one."
}

# A tape with no index says so and points at what it can do. Exit 1: the answer
# is not on this tape.
die_no_index() {
  say ""
  say "This tape carries no index, and nothing on this tape says where a unit's other versions"
  say "or copies are. What this tape can tell you about itself:"
  say "  $0 --find-envelope --key KEY     the units, versions and slices it holds"
  say "  $0 --list --key KEY              the files of a unit (add --unit NAME)"
  say "For another tape, load it and run its own RESTORE.sh the same way; a tape written later"
  say "can answer for all the tapes written before it."
  exit 1
}

# The envelopes the key opened: collect each one's units.tsv (the operator's
# lists every tenant's units, so it alone is read when it opened) and note the
# versions of the loaded tape that are only a range.
WHERE_FILES=()
WHERE_TENANTS=()
WHERE_OPERATOR=""
collect_index_files() {
  local pos d ty who
  WHERE_FILES=()
  WHERE_TENANTS=()
  WHERE_OPERATOR=""
  : >"$WORK/range.versions"
  rm -f "$WORK/partial.flag"
  for pos in $OPENED; do
    d="$WORK/env.$pos"
    INDEX_OPENED=$((INDEX_OPENED + 1))
    note_range_versions "$d"
    ty=$(file_type_at "$pos" "$FILES_TXT")
    if [ -f "$d/index/units.tsv" ]; then
      INDEX_HAD=$((INDEX_HAD + 1))
      case $ty in
      operator_envelope*) WHERE_OPERATOR="$d/index/units.tsv" ;;
      *)
        who=$(toml_val "$d/MANIFEST.toml" tenant 2>/dev/null || true)
        WHERE_FILES+=("$d/index/units.tsv")
        WHERE_TENANTS+=("$who")
        ;;
      esac
    fi
  done
  if [ -n "$WHERE_OPERATOR" ]; then
    WHERE_FILES=("$WHERE_OPERATOR")
    WHERE_TENANTS=("")
  fi
}

# --where --all: every unit the index names, with no limit. The awk reads each
# index file whole and writes the totals and the units whose newest version this
# tape does not hold to two files, so the summary can come before the listing.
where_every_unit() {
  local i f rc n a total=0 here=0
  : >"$WORK/all.absent"
  : >"$WORK/all.out"
  for ((i = 0; i < ${#WHERE_FILES[@]}; i++)); do
    f=${WHERE_FILES[$i]}
    : >"$WORK/all.counts"
    rc=0
    ALL_TENANT="${WHERE_TENANTS[$i]}" ALL_COUNTS="$WORK/all.counts" ALL_ABSENT="$WORK/all.absent" \
      INDEX_RANGE="$WORK/range.versions" INDEX_PARTIAL="$WORK/partial.flag" \
      awk "$AWK_INDEX_COMMON $AWK_INDEX_ALL" "$f" >>"$WORK/all.out" || rc=$?
    case "$rc" in
    0 | 1)
      n="" a=""
      read -r n a <"$WORK/all.counts" || true
      if is_uint "$n" && is_uint "$a"; then
        total=$((total + n))
        here=$((here + a))
      fi
      ;;
    *) warn "an index in this tape's envelopes has no unit or version column, so this script cannot read it." ;;
    esac
  done
  if [ "$total" -eq 0 ]; then
    say "This index names no unit."
    exit 1
  fi
  say "$total unit(s) in this index; this tape holds the newest version of $here of them whole."
  if [ -s "$WORK/all.absent" ]; then
    say ""
    say "The newest version of these is NOT on this tape whole (unit, version, unit uuid, the tapes that hold it):"
    cat "$WORK/all.absent"
  fi
  say ""
  say "Every unit the index names, with every version and every tape that holds it:"
  say ""
  cat "$WORK/all.out"
  say "A holder marked 'this tape' is the tape you have loaded. For any other, find the cartridge"
  say "by its label or its chip serial, load it, and use the RESTORE.sh stored on that tape."
  note_range_holders
}

do_where() {
  establish
  envelope_space_check
  open_envelopes
  collect_index_files
  local i rc f found=0
  index_banner
  say ""
  [ "$INDEX_HAD" -gt 0 ] || die_no_index
  if [ "$WHERE_UNIT" = --all ]; then
    where_every_unit
    return 0
  fi
  : >"$WORK/where.known"
  for ((i = 0; i < ${#WHERE_FILES[@]}; i++)); do
    f=${WHERE_FILES[$i]}
    rc=0
    WHERE_UNIT="$WHERE_UNIT" WHERE_KNOWN="$WORK/where.known" WHERE_TENANT="${WHERE_TENANTS[$i]}" \
      INDEX_RANGE="$WORK/range.versions" INDEX_PARTIAL="$WORK/partial.flag" \
      awk "$AWK_INDEX_COMMON $AWK_INDEX_WHERE" "$f" || rc=$?
    case "$rc" in
    0) found=1 ;;
    1) ;;
    *) warn "an index in this tape's envelopes has no unit or version column, so this script cannot read it." ;;
    esac
  done
  if [ "$found" = 0 ]; then
    say ""
    say "No unit named '$(safe_str "$WHERE_UNIT")' is in this index."
    if [ -s "$WORK/where.known" ]; then
      say "It names these units (all of them; each one's newest version and holders:"
      say "  $0 --where --all --key KEY):"
      sort -u "$WORK/where.known" | sed 's/^/    /'
    fi
    if [ -z "$WHERE_OPERATOR" ]; then
      say "A tenant's key opens that tenant's index only; the operator or escrow key opens the"
      say "index of every tenant."
    fi
    exit 1
  fi
  say ""
  say "A holder marked 'this tape' is the tape you have loaded. For any other, find the cartridge"
  say "by its label or its chip serial, load it, and use the RESTORE.sh stored on that tape."
  note_range_holders
}

# The text given, with a TAB, newline or carriage return written out, so a
# message about it stays on its own line.
visible_text() { # <text>
  local t=$1
  t=${t//$'\t'/\\t}
  t=${t//$'\n'/\\n}
  t=${t//$'\r'/\\r}
  printf '%s' "$t"
}

# --find: the matches of each tenant envelope's paths.tsv, kept in a file so
# the banner can come first. One limit is shared by every envelope.
FIND_LEFT=50
FIND_SHOWN=0
FIND_TOTAL=0
find_in_envelope() { # <position>
  local pos=$1 d="$WORK/env.$1" u who shown total
  INDEX_OPENED=$((INDEX_OPENED + 1))
  note_range_versions "$d"
  if [ -f "$d/index/units.tsv" ] || [ -f "$d/index/paths.tsv" ]; then
    INDEX_HAD=$((INDEX_HAD + 1))
  fi
  if [ -f "$d/index/paths.tsv" ]; then
    u="$d/index/units.tsv"
    [ -f "$u" ] || u=/dev/null
    who=$(toml_val "$d/MANIFEST.toml" tenant 2>/dev/null || true)
    : >"$WORK/find.counts"
    FIND_TEXT="$FIND_TEXT" FIND_LIMIT="$FIND_LEFT" FIND_UNITS="$u" FIND_COUNTS="$WORK/find.counts" \
      FIND_WHO="tenant $(safe_str "$who")" INDEX_RANGE="$WORK/range.versions" INDEX_PARTIAL="$WORK/partial.flag" \
      awk "$AWK_INDEX_COMMON $AWK_INDEX_FIND" "$d/index/paths.tsv" >>"$WORK/find.out" || {
      warn "the index in the envelope at file $pos has no unit, version or path column, so this script cannot read it."
      : >"$WORK/find.counts"
    }
    shown=0
    total=0
    read -r shown total <"$WORK/find.counts" || true
    if is_uint "$shown" && is_uint "$total"; then
      FIND_SHOWN=$((FIND_SHOWN + shown))
      FIND_TOTAL=$((FIND_TOTAL + total))
      FIND_LEFT=$((FIND_LEFT - shown))
    fi
  fi
}

do_find() {
  local pos
  : >"$WORK/find.out"
  : >"$WORK/range.versions"
  rm -f "$WORK/partial.flag"
  establish
  envelope_space_check
  open_envelopes
  for pos in $OPENED; do find_in_envelope "$pos"; done
  index_banner
  [ "$INDEX_HAD" -gt 0 ] || die_no_index
  if [ "$FIND_TOTAL" -eq 0 ]; then
    say ""
    say "No path in this index contains '$(visible_text "$FIND_TEXT")'. (The text is matched as it stands, not as a"
    say "pattern, and capitals matter. A path is relative to its unit's root.)"
    exit 1
  fi
  say ""
  say "Paths containing '$(visible_text "$FIND_TEXT")' (unit, version, kind, size, path; kind is d directory,"
  say "f file, l symlink, s special). A TAB, newline or carriage return in a path is shown as"
  say "\\t, \\n or \\r, and a byte that is not UTF-8 as \\xHH."
  say ""
  cat "$WORK/find.out"
  say ""
  if [ "$FIND_TOTAL" -gt "$FIND_SHOWN" ]; then
    say "$((FIND_TOTAL - FIND_SHOWN)) more match(es) are not shown: this prints the first $FIND_SHOWN. Give more of"
    say "the path to narrow it."
  fi
  note_range_holders
  say "To restore one of these from the tape that holds it:"
  say "  $0 --restore --key KEY --unit UNIT --path PATH --to /your/destination"
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
  where) do_where ;;
  find) do_find ;;
  esac
  ;;
esac
