#!/usr/bin/env bash
# RESTORE.sh — Emergency restore script for tapectl volume GOLD01 (layout v2)
# This script restores data from this tape WITHOUT tapectl installed.
# It reads the tape's front index (File 3), the seal marker (last file),
# finds your encrypted envelope, decrypts each data slice, and extracts
# the dar archive to a directory.
#
# Usage:
#   ./RESTORE.sh --info                                       Show tape layout + seal verdict
#   ./RESTORE.sh --verify                                     Keyless integrity check (no key needed)
#   ./RESTORE.sh --find-envelope --key KEYFILE [--key K2 ...] Decrypt your envelope
#   ./RESTORE.sh --restore --key KEYFILE [--key K2 ...] --to DIR
#                [--unit U [--unit U2 ...] | --all] [--version N]
#                [--no-space-check] [--overwrite] [--no-file-check] [--scratch DIR]
#   ./RESTORE.sh --restore --key KEYFILE --to DIR --unit U --path P [--path P2 ...]
#                [--version N] [--scratch DIR]                One file or folder
#   ./RESTORE.sh --list --key KEYFILE [--unit U] [--version N]  A unit's files
#   ./RESTORE.sh --restore ... [--slices-from DIR ...] [--swap-tapes]   A bad slice from another copy
#   ./RESTORE.sh --restore --key KEYFILE --to DIR --unit U --fill-from LIST   Files a restore could not restore
#   ./RESTORE.sh --verify [--slices-from DIR ...] [--swap-tapes]
#   ./RESTORE.sh --where UNIT --key KEYFILE [--key K2 ...]       Which tapes hold UNIT
#   ./RESTORE.sh --where --all --key KEYFILE [--key K2 ...]      Every unit the index names, no limit
#   ./RESTORE.sh --find TEXT --key KEYFILE [--key K2 ...]        Which unit and tape hold a path
#
# --list and --path read the dar catalogue each envelope carries
# (catalogs/<uuid8>_v<version>.1.dar): --list reads no data slice at all, and
# --path reads only the slices that hold what it asks for, plus the last.
# --to must be empty or new: dar keeps a file that is already there and
# reports success, so a restore over an earlier one cannot be trusted.
# --overwrite replaces such files instead, to finish a restore that stopped.
# With more than one unit (--all, or --unit repeated) each unit goes to its
# own directory, --to/UNIT.
#
# --where and --find read the cross-volume index that a tape written since
# version 1.3.0 carries in its envelopes (index/units.tsv, and index/paths.tsv
# in each tenant envelope): --where lists every version of a unit and the tapes
# holding it, --where --all lists EVERY unit the index names (no limit) with its
# newest version, whether this tape holds it, and every version's holders,
# --find lists the paths that contain a piece of text and the tapes holding
# them. An index is as of the moment its own tape was written, so the newest
# tape you hold has the most complete one. They read this tape only, forward
# from one rewind, like every other mode.
#
# --key may be repeated. An envelope and the slices it describes can need
# different keys after a key rotation, so every key is tried independently.
#
# A tape written since version 1.6.0 may end early: when the drive reported
# the end of its medium in a data slice before the last, the write sealed the
# tape after that slice, with a short seal. Everything before the slices is
# the plan (File 0, the front index, the envelopes); the seal marker, the last
# file, says where the tape really ended (ended_after), and a planned file
# after that is not on the tape. A unit with a slice there is cut: not on this
# tape whole, named and never restored from it (another tape holds it). So on
# such a tape every mode that reads slices or says which units it holds reads
# the seal first, right after the front index: one wind to the end of the data
# and a rewind. --info says where the tape ended, and --verify prints NOT ON
# TAPE for each planned file after it.
#
# A slice that fails its hash, or will not read, can be taken from another copy
# of the unit (ADR-0012 item 101). The copy's slice has the same sha256 in its
# front index, and that is the only way one is matched: --slices-from DIR names
# a directory of slice files copied off another tape (any names), and
# --swap-tapes asks for another cartridge after this one has been read. Either
# one restores the way an older dar does, slice by slice onto disk (about twice
# the unit's size), because a stream cannot take a slice back from dar.
#
# A restore whose dar run names files it could not restore (a failed CRC, a
# damaged slice) writes their paths to .tapectl-damaged-files.txt in the unit's
# directory (ADR-0012 item 102) and ends RESTORE INCOMPLETE. Another tape that
# holds the same unit at the same version fills exactly those files:
#   ./RESTORE.sh --restore --key KEYFILE --unit U --fill-from LIST --to DIR
#
# dar checks each file it restores with a short checksum, not a hash, so a
# damaged file can come back with no error. A tape written since version 1.3.0
# records the sha256 of every regular file in its index, and after each restore
# and each fill this script reads the restored files back and checks them
# against it with sha256sum (ADR-0012 item 103). A file that does not match, or
# is missing, goes on the damaged-file list like one dar named, and the run ends
# RESTORE INCOMPLETE. That costs one read of the restored files from disk, not
# a tape read; --no-file-check skips it. A tape written before 1.3.0 has no
# index to check against, and the script says so.
#
# Disk space: --restore streams each slice from the tape through age into dar,
# so nothing decrypted is written to disk but the restored files themselves:
# --to needs about the unit's size free (counted from the slices' encrypted
# size on tape: a compressed unit may need more). The envelopes are the other
# thing on disk: they go to the work directory in ${TMPDIR:-/tmp}, decrypted,
# and TMPDIR moves it. dar is given the unit's catalogue from the envelope
# (dar 2.7.13 loses a file's tail streaming without one).
# Where streaming is not safe (no usable catalogue and a dar before 2.7.21,
# or a dar before 2.7.9) the slices are decrypted to disk first, as 1.0.x
# did, in a scratch directory inside --to or --scratch DIR. The script
# measures the space before it reads any slice.
#
# Requirements: mt (mt-st), dd, age, dar, sha256sum, head, truncate, tar, mkfifo, tee
# Each mode checks only the tools it runs (see the prerequisite check below).
# Total files on tape: 12
#
# Degradation ladder (see the system guide, File 1, "If All Else Fails"):
#   rung 1 — File 3, the front index (normal path)
#   rung 2 — the seal marker's embedded front-index copy (if File 3 fails)
#   rung 3 — manual zero-strip recovery (if both fail; this script cannot
#            automate it, but prints the procedure)

set -euo pipefail

# This script matches the words of dar and of other tools (dar's skip line,
# the GPG start-up failure, a full disk). dar and coreutils ship translations,
# and a translated message would not match: ask for English in any locale.
export LC_ALL=C

DEVICE="${TAPE_DEVICE:-/dev/nst0}"
LABEL="GOLD01"
BLOCK=524288 # 512 KB — tapectl fixed block size

# Every --key given on the command line, in order. A volume's envelope and the
# data slices it describes can be sealed to DIFFERENT key generations: the
# envelope is built when the volume is written, each slice when it was staged.
# A key rotation between the two leaves no single key that opens both, so each
# key here is tried independently for the envelope and for every slice.
KEYS=()

# WORK is the work directory, made in ${TMPDIR:-/tmp}: TMPDIR moves it. It
# holds the tape's text zones, the FIFOs a restore streams slices through, and
# the ENVELOPES: each is read whole into it, and each one a key opens is kept
# there decrypted (its MANIFEST.toml, the dar catalogues of its units and, in
# the operator's, catalog.db) until the run ends; --all keeps every one it
# opens. These grow with the number of files on the tape. A data slice never
# lands here — a slice is up to 10 GiB and /tmp is RAM on many systems.
# SCRATCH is for the one mode that keeps slices on disk (--path, which reads
# only a few), a directory created inside --to or --scratch once its size is
# known.
SCRATCH=""
SCRATCH_PARENT=""
SKIP_SPACE_CHECK=0
OVERWRITE=0
# --slices-from DIR (repeatable), --swap-tapes and --fill-from LIST (1.3.0).
# SALVAGE is 1 when either of the first two was given.
SLICES_FROM=()
SWAP_TAPES=0
SALVAGE=0
FILL_FROM=""

umask 077 # decrypted plaintext and temp files must not be world-readable
WORK="$(mktemp -d "${TMPDIR:-/tmp}/tapectl-restore.XXXXXX")" ||
  {
    echo "FATAL: cannot create temporary directory" >&2
    exit 1
  }
# Background jobs of a streaming restore (dar, and a slice's hash) are
# killed on any way out: a dar left waiting on a FIFO would wait forever.
BG_PIDS=()
cleanup() {
  local p
  for p in ${BG_PIDS[@]+"${BG_PIDS[@]}"}; do
    disown "$p" 2>/dev/null || true
    kill -KILL "$p" 2>/dev/null || true
  done
  rm -rf "$WORK"
  [ -z "$SCRATCH" ] || rm -rf "$SCRATCH"
}
trap cleanup EXIT

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
       command again, with --overwrite if --to now holds part of the unit.
       --restore writes only the restored files to --to (--path also keeps
       the few slices it reads in a scratch directory, inside --to unless
       --scratch names another disk); everything else goes to the work
       directory in ${TMPDIR:-/tmp}: the decrypted envelopes, which grow with
       the number of files on the tape. If that is what filled, set TMPDIR to
       a directory on a larger disk."
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
  die "not enough disk space in $1 to restore:
       it needs about $(size_str "$2"), and $(size_str "$3") is free.
       No slice has been read yet. A restore streams each slice from the tape
       through age into dar, so it needs room for the restored files only,
       about the size of the unit's slices. That figure is the slices' size
       on tape, encrypted: a unit that was stored compressed restores larger,
       and a compressed unit may need more than this. --path also keeps the
       slices it reads on disk until dar has taken the files from them
       (--scratch DIR puts those on another disk).
       Free space there, or choose a larger disk with --to.
       (--no-space-check skips this check, for a filesystem that holds more than
       df reports, such as a compressed or thin-provisioned one.)"
}

# What the figures above count. dar does not say how large a unit restores
# before it runs, so the check counts the slices as they sit on tape.
space_note() {
  info "            (counted from the slices' encrypted size on tape; a compressed unit may need more)"
}

# Refuse BEFORE the first slice is read when the restore cannot fit. Running
# out of space mid-restore used to kill the script hours in, silently or as a
# key error. A streamed restore asks only for the files (scratch 0); --path
# asks for the slices it keeps in scratch as well. With both on one
# filesystem (unless stat proves otherwise) the two needs add up.
check_space() { # <destination> <scratch dir> <bytes of files> <bytes of scratch>
  local dest=$1 scr=$2 need_dest=$3 need_scr=$4
  if [ "$SKIP_SPACE_CHECK" = 1 ]; then
    info "Disk space not checked (--no-space-check)"
    return 0
  fi
  local scr_free dest_free
  dest_free=$(free_bytes "$dest")
  if ! is_uint "$dest_free"; then
    echo "WARNING: cannot measure free disk space (df failed); continuing without the check." >&2
    return 0
  fi
  if [ "$need_scr" -eq 0 ]; then
    info "Disk space: needs about $(size_str "$need_dest") in $dest (the files); $(size_str "$dest_free") free"
    space_note
    [ "$dest_free" -ge "$need_dest" ] || die_space "$dest" "$need_dest" "$dest_free"
    return 0
  fi
  scr_free=$(free_bytes "$scr")
  if ! is_uint "$scr_free"; then
    echo "WARNING: cannot measure free disk space (df failed); continuing without the check." >&2
    return 0
  fi
  local scr_dev dest_dev
  scr_dev=$(stat -c %d -- "$scr" 2>/dev/null || true)
  dest_dev=$(stat -c %d -- "$dest" 2>/dev/null || true)
  if [ -z "$scr_dev" ] || [ "$scr_dev" = "$dest_dev" ]; then
    local need=$((need_scr + need_dest))
    info "Disk space: needs about $(size_str "$need") in $dest (slices read, then the files); $(size_str "$dest_free") free"
    space_note
    [ "$dest_free" -ge "$need" ] || die_space "$dest" "$need" "$dest_free"
  else
    info "Disk space: needs about $(size_str "$need_scr") in $scr (slices read); $(size_str "$scr_free") free"
    info "            and about $(size_str "$need_dest") in $dest (the files); $(size_str "$dest_free") free"
    space_note
    [ "$scr_free" -ge "$need_scr" ] || die_space "$scr" "$need_scr" "$scr_free"
    [ "$dest_free" -ge "$need_dest" ] || die_space "$dest" "$need_dest" "$dest_free"
  fi
}

# Refuse, BEFORE the first tape read, a destination this restore could not
# be trusted to fill (#405). dar under -Q answers "no" to every overwrite
# question and exits 0, so a re-run over a restore that stopped part way (a
# full disk, a power cut, Ctrl-C) used to report RESTORE COMPLETE over the
# truncated files it left. And a scratch directory left by a killed restore
# may hold decrypted data, so it is named and never silently reused.
check_stale_scratch() { # <directory>...
  local d stale
  for d in "$@"; do
    [ -d "$d" ] || continue
    for stale in "$d"/.tapectl-restore.*; do
      [ -e "$stale" ] || continue
      die "refusing to restore: $stale is the scratch directory of an earlier
       restore that was killed or lost power, and it may hold DECRYPTED archive
       data. Remove it (rm -rf \"$stale\") and run the same command again.
       Nothing has been read from tape."
    done
  done
}

check_destination() { # <destination> <scratch parent>
  local dest=$1 scr=$2
  if [ -e "$dest" ] && [ ! -d "$dest" ]; then
    die "--to $dest exists and is not a directory. Nothing has been read from tape."
  fi
  check_stale_scratch "$dest" "$scr"
  [ -d "$dest" ] || return 0
  [ -n "$(ls -A "$dest")" ] || return 0
  if [ "$OVERWRITE" = 1 ]; then
    info "--to $dest is not empty; --overwrite replaces every file this unit restores"
    return 0
  fi
  die "--to $dest is not empty.
       dar keeps a file that is already there and still reports success, so a
       restore into a directory that holds files cannot be trusted: after an
       earlier restore that stopped part way, the truncated files would stay.
       Restore into an empty or new directory, or, to finish an earlier restore
       of the same unit, run the same command again with --overwrite, which
       replaces every file the unit restores.
       Nothing has been read from tape."
}

# The backstop behind check_destination: a file that appeared in --to while
# the restore ran is still kept by dar, which says so only in this line on
# its stdout, and exits 0.
die_if_dar_skipped() { # <dar output> <destination>
  local skipped
  skipped=$(grep 'not restored (user choice)$' "$1" 2>/dev/null |
    sed 's/ not restored (user choice)$//' | head -n 20 || true)
  [ -z "$skipped" ] && return 0
  die "the restore into $2 is INCOMPLETE: dar declined to overwrite file(s)
       that already existed there, and reported success anyway. The old copies
       are still in place, so they are NOT what is on tape. Skipped:
$skipped
       Restore into an empty directory, or run again with --overwrite."
}

# The dar on this machine: its version, and whether it can start at all.
# A dar built with GPG support asks, at startup, whether to carry on when
# the gpg program is missing; under -Q it answers no and aborts. `dar -V`
# meets the same question, so asking it first names the problem before the
# tape is read rather than after every slice has been.
DAR_VERSION=""
dar_check() {
  local out
  out=$(dar -V -Q -N 2>&1 </dev/null || true)
  if printf '%s\n' "$out" | grep -i 'INITIALIZATION FAILED FOR GPGME' >/dev/null; then
    die "dar cannot start here: it was built with GPG support and needs the gpg
       program on PATH (package gnupg; on Debian/Ubuntu: apt install gnupg).
       dar's own words: $(printf '%s\n' "$out" | grep -i 'INITIALIZATION FAILED FOR GPGME' | head -n 1)
       Nothing has been read from tape."
  fi
  DAR_VERSION=$(printf '%s\n' "$out" | sed -n 's/.*dar version \([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\).*/\1/p' | head -n 1)
  if [ -z "$DAR_VERSION" ]; then
    echo "WARNING: cannot tell which dar this is (dar -V said nothing usable); restoring the" >&2
    echo "         safe way, decrypting each unit's slices to disk before dar reads them." >&2
  fi
}

# Is this dar at least version $1 (X.Y.Z)? False when the version is unknown.
dar_at_least() {
  [ -n "$DAR_VERSION" ] || return 1
  awk -v have="$DAR_VERSION" -v want="$1" 'BEGIN {
    split(have, a, "."); split(want, b, ".")
    for (i = 1; i <= 3; i++) {
      if (a[i] + 0 > b[i] + 0) exit 0
      if (a[i] + 0 < b[i] + 0) exit 1
    }
    exit 0
  }'
}

# ---- prerequisite check ----

# What each mode runs, besides mt (every mode that reads the tape). A mode
# checks its own tools when it starts, before the first tape read, so a host
# that can only read and hash a tape is not refused for tools a restore needs.
# --info, --verify:
TOOLS_READ=(dd sha256sum head)
# --find-envelope:
TOOLS_ENVELOPE=(dd age sha256sum head truncate tar)
# --list:
TOOLS_LIST=(dd age dar sha256sum head truncate tar)
# --restore --path:
TOOLS_PATH=(dd age dar sha256sum head truncate tar tee)
# --restore, whole units (every tool any mode may run):
TOOLS_RESTORE=(dd age dar sha256sum head truncate tar mkfifo tee)
# --where, --find (the envelopes' index, read as plain text):
TOOLS_INDEX=(dd age sha256sum head truncate tar)

require_tools() { # <tool>...
  local tool
  command -v mt >/dev/null 2>&1 || command -v mt-st >/dev/null 2>&1 ||
    die "missing required tool: mt (package mt-st)"
  for tool in "$@"; do
    command -v "$tool" >/dev/null 2>&1 || die "missing required tool: $tool"
  done
}

# ---- tape helpers ----

# The mt this script drives. mt-st is the one that can set the block size;
# stock Debian and Ubuntu install GNU cpio's mt as plain `mt` when mt-st is
# absent, and that one has no setblk. tape_init chooses.
MT=mt

tape_init() {
  if command -v mt-st >/dev/null 2>&1; then
    MT=mt-st
  fi
  local ver
  ver=$("$MT" --version 2>&1 | head -n 1 || true)
  if "$MT" -f "$DEVICE" setblk "$BLOCK" 2>"$WORK/mt.err"; then
    return 0
  fi
  local why
  why=$(head -n 1 "$WORK/mt.err" 2>/dev/null || true)
  case "$ver $why" in
  *mt-st*)
    die "cannot set the block size on $DEVICE: ${why:-mt failed}
       mt-st itself refused, so the device is the problem: is $DEVICE a tape
       drive, with a cartridge loaded? List the drives: ls -l /dev/tape/by-id/"
    ;;
  *"GNU cpio"* | *"invalid argument"* | *setblk*)
    echo "WARNING: '$MT' is not mt-st (${ver:-unknown version}) and cannot set the block size." >&2
    echo "         Reading $DEVICE in whatever mode the drive is in. In variable-block mode, the" >&2
    echo "         st driver's default, dd bs=512k reads each 512 KiB tape block whole and this" >&2
    echo "         works. If a read fails, install mt-st (apt install mt-st) and run again." >&2
    ;;
  *)
    die "cannot set the block size on $DEVICE with '$MT' (${ver:-unknown version}): ${why:-mt failed}
       This script needs mt-st (package mt-st). If $MT is mt-st, check that
       $DEVICE is a tape drive with a cartridge loaded: ls -l /dev/tape/by-id/"
    ;;
  esac
}

# CUR is the tape file the head sits at the START of, or empty when that is
# not known for certain (#396). It is 0 after a rewind and pos+1 after a read
# that took file pos through its filemark; a failed mt or dd, or a read that
# stopped inside a file, empties it. seek_to moves only forward from a known
# CUR and rewinds whenever it is unknown or behind the target, so any doubt
# costs a rewind and never a read of the wrong file. Before 1.1.0 every read
# rewound to the start of the tape, which on LTO costs about two minutes.
CUR=""
MT_STATUS_WARNED=0

# Where the drive says the head is, as "FILE BLOCK", or nothing when mt
# status cannot be read or parsed (mt-st and GNU mt word it differently).
mt_position() {
  "$MT" -f "$DEVICE" status 2>/dev/null | awk '
    { l = tolower($0) }
    l ~ /file number *=/ {
      s = l; sub(/.*file number *= */, "", s); sub(/[^-0-9].*/, "", s); f = s
    }
    l ~ /block number *=/ {
      s = l; sub(/.*block number *= */, "", s); sub(/[^-0-9].*/, "", s); b = s
    }
    END { if (f ~ /^-?[0-9]+$/ && b ~ /^-?[0-9]+$/) print f, b }
  ' || true
}

# Put the head at the start of tape file $1. Returns 1 if mt fails.
seek_to() {
  local pos=$1 from=$CUR here
  CUR=""
  if [ -n "$from" ]; then
    # Trust CUR only if the drive agrees, as tapectl checks MTIOCGET.
    here=$(mt_position)
    if [ -z "$here" ]; then
      if [ "$MT_STATUS_WARNED" = 0 ]; then
        echo "NOTE: '$MT status' gave no file number this script can read; rewinding before every read (slower, still correct)." >&2
        MT_STATUS_WARNED=1
      fi
      from=""
    elif [ "$here" != "$from 0" ]; then
      echo "NOTE: the drive reports file/block $here, not the start of file $from; rewinding to be sure." >&2
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

# dd's own complaint, for a NOTE: not its record counts.
dd_detail() {
  grep -v -e 'records in$' -e 'records out$' -e 'bytes.*copied' "$1" 2>/dev/null | tail -n 2 || true
}

# Read tape file at position $1 into file $2 (raw bytes, block-padded). Dies
# on failure, naming the cause — used once a source is already trusted
# (post-ladder). dd's stderr used to go to /dev/null, so a full disk here ended
# the script with no message at all.
read_tape_raw() {
  local pos=$1 out=$2
  seek_to "$pos" || die "cannot position the tape at file $pos ($MT failed)"
  if ! dd if="$DEVICE" of="$out" bs="$BLOCK" 2>"$WORK/dd.err"; then
    CUR=""
    die_io "reading tape file $pos failed" "$WORK/dd.err" "$(dirname "$out")"
  fi
  if [ -s "$out" ]; then CUR=$((pos + 1)); else CUR=""; fi
}

# Same, but never exits: returns 1 on any failure (missing position, I/O
# error, or an empty read). Used wherever "absent" must be distinguished from
# "present but wrong" — the ladder's rung selection relies on this instead of
# letting `set -e` kill the script. dd's complaint is kept as a NOTE.
try_read_tape_raw() {
  local pos=$1 out=$2
  seek_to "$pos" || return 1
  if ! dd if="$DEVICE" of="$out" bs="$BLOCK" 2>"$WORK/dd.err"; then
    CUR=""
    echo "NOTE: reading tape file $pos failed: $(dd_detail "$WORK/dd.err")" >&2
    return 1
  fi
  if [ -s "$out" ]; then
    CUR=$((pos + 1))
  else
    CUR=""
    return 1
  fi
}

# sha256 of tape file $1, first $2 bytes (all of it when $2 is empty), hashed
# as it streams off the tape: --verify used to copy each file into WORK first,
# twice, and a 10 GiB slice in a RAM-backed /tmp made a good tape FAIL as
# "unreadable". Returns 1 when the file cannot be reached or read, or yields no
# bytes at all. The hash is left in TAPE_SHA, not printed: called as
# $(hash_tape_file ...) it would run in a subshell, and the cursor it moves
# would be lost with it.
EMPTY_SHA256=e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
TAPE_SHA=""
hash_tape_file() {
  local pos=$1 size=$2 sum
  TAPE_SHA=""
  seek_to "$pos" || return 1
  CUR=""
  if [ -n "$size" ]; then
    # head stops at the exact size; cat drains the block padding after it,
    # so dd never takes a SIGPIPE that pipefail would read as a tape fault,
    # and the head ends past the filemark.
    sum=$(dd if="$DEVICE" bs="$BLOCK" 2>"$WORK/dd.err" | {
      head -c "$size" | sha256sum
      cat >/dev/null
    }) || return 1
  else
    sum=$(dd if="$DEVICE" bs="$BLOCK" 2>"$WORK/dd.err" | sha256sum) || return 1
  fi
  sum=${sum%% *}
  [ -n "$sum" ] && [ "$sum" != "$EMPTY_SHA256" ] || return 1
  CUR=$((pos + 1))
  TAPE_SHA=$sum
}

# Keep a copy of tape file $1 (at most $3 bytes of it) in file $2 while
# passing it: --verify reads Files 1 and 2 on its way to File 3 and checks
# them once File 3 says what they should be, instead of rewinding for them.
# Returns 1 if the file cannot be read.
stash_tape_file() {
  local pos=$1 out=$2 max=$3
  seek_to "$pos" || return 1
  CUR=""
  dd if="$DEVICE" bs="$BLOCK" 2>"$WORK/dd.err" | {
    head -c "$max" >"$out"
    cat >/dev/null
  } || return 1
  [ -s "$out" ] || return 1
  CUR=$((pos + 1))
}

# The most of a file before the front index that --verify keeps while passing
# it (Files 0-2 are a few KiB each; this only bounds a damaged tape).
STASH_MAX=$((64 * BLOCK))

# sha256 of the first $2 bytes (all, when $2 is empty) of tape file $1 from
# what --verify kept of it while passing; returns 1 when nothing was kept, or
# too little, so the caller reads the tape instead.
stashed_sha() {
  local f="$WORK/stash.$1" size=$2 have
  [ -f "$f" ] || return 1
  have=$(wc -c <"$f")
  if [ -n "$size" ]; then
    [ "$have" -ge "$size" ] || return 1
    head -c "$size" "$f" | sha256sum | awk '{print $1}'
  else
    [ "$have" -lt "$STASH_MAX" ] || return 1
    sha256sum "$f" | awk '{print $1}'
  fi
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
file_type_at() { awk -F'|' -v p="$1" '$1 == p { print $2; exit }' "$2"; }

# List envelope positions in on-tape order (the file list is already
# position-sorted, and the format's fixed zone order puts the tenant
# envelope(s), then the operator envelope and its backup, then each tenant
# envelope's backup: every primary before every backup).
envelope_positions() {
  awk -F'|' '$2=="tenant_envelope" || $2=="operator_envelope" || $2=="operator_envelope_backup" || $2=="tenant_envelope_backup" { print $1 }' "$1"
}

# ---- bootstrap (File 0 — identity + pointers only in layout v2) ----

bootstrap_thunk() {
  tape_init
  info "Reading ID thunk (file 0)..."
  # Kept raw as well: --verify checks File 0's bytes against the front index
  # without going back for them. The text is plaintext TOML with no NULs of
  # its own, so deleting every NUL removes exactly the block padding.
  read_tape_raw 0 "$WORK/stash.0"
  tr -d '\0' <"$WORK/stash.0" >"$WORK/id_thunk.txt"
  sed -n '/^\[volume\]/,$p' "$WORK/id_thunk.txt" >"$WORK/thunk.toml"

  check_layout_version "$(toml_val "$WORK/thunk.toml" layout_version)"
  check_magic "$(toml_val "$WORK/thunk.toml" magic)"
  check_requires "File 0" "$WORK/thunk.toml"
  # A tape that requires short-seal may end before the last file its plan
  # lists: its seal says where, and is read before any data slice (R1).
  TAPE_SHORT_SEAL=0
  if requires_feature "$WORK/thunk.toml" short-seal; then
    TAPE_SHORT_SEAL=1
  fi

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

# The features this script knows how to read (volume-format-v2.md, "How v2
# extends"). A tape whose File 0 or seal marker names anything else under
# `requires` is refused by name: a later tapectl that changes something a
# reader must understand says so there, and this script must stop rather than
# misread it. An absent `requires` (every tape before 1.1.0) requires nothing,
# and any key a table carries that this script does not know is ignored
# unless `requires` names it. short-seal (every tape written since version
# 1.6.0): the tape may end before the last file its plan lists, and its seal
# marker, the last file, says where (volume-format-v2.md 4.1, 4.2).
KNOWN_FEATURES="short-seal"

die_newer_needed() { # <what> <detail>
  die "$2
       This RESTORE.sh (layout v2, tapectl 1.6.0) cannot read it safely, and
       will not guess. Use the RESTORE.sh stored on THIS tape — every tape
       carries the script that reads it, at file 2:
         mt -f $DEVICE rewind && mt -f $DEVICE fsf 2
         dd if=$DEVICE bs=512k | tr -d '\\0' > RESTORE.sh
       or a newer tapectl ($1 names what it needs)."
}

check_magic() { # <magic as File 0 states it>
  local m=$1
  [ -z "$m" ] && return 0
  [ "$m" = "tapectl-volume-v2" ] && return 0
  die_newer_needed "File 0" "this tape's File 0 says magic = $(safe_str "$m"), not tapectl-volume-v2."
}

check_requires() { # <what> <toml file>
  local what=$1 file=$2 raw list f unknown=""
  raw=$(toml_val "$file" requires)
  [ -z "$raw" ] && return 0
  case "$raw" in
  \[*\]) ;;
  *) die_newer_needed "$what" "$what states requires = $(safe_str "$raw"), which this script cannot parse." ;;
  esac
  list=$(printf '%s' "$raw" | tr -d '[]",' | tr -s ' ' ' ')
  for f in $list; do
    case " $KNOWN_FEATURES " in
    *" $f "*) ;;
    *) unknown="$unknown $(safe_str "$f")" ;;
    esac
  done
  [ -z "$unknown" ] && return 0
  die_newer_needed "$what" "this tape requires feature(s)${unknown}, which this script does not know
       ($what states requires = $(safe_str "$raw")). It was written by a newer
       tapectl (File 0: tapectl_version $(safe_str "$(toml_val "$WORK/thunk.toml" tapectl_version)"))."
}

# Does the requires line of a TOML file name feature $2? Matched as a whole
# name in the list check_requires reads, never as a pattern.
requires_feature() { # <toml file> <feature>
  local list
  list=$(toml_val "$1" requires | tr -d '[]",' | tr -s ' ' ' ')
  case " $list " in
  *" $2 "*) return 0 ;;
  esac
  return 1
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
  # The second cartridge of --swap-tapes is a different tape on purpose.
  [ "${OTHER_TAPE:-0}" = 0 ] || return 0
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

# On success sets SEAL_FRONT_INDEX_SHA256, SEAL_FILE_COUNT, SEAL_SEALED_AT,
# SEAL_LAST (the last file before the seal) and leaves the parsed, validated
# embedded copy at $WORK/files_from_seal.txt. Returns 1 when there is no usable
# seal; exits only for a seal that requires a feature this script does not
# know, which must never read as unsealed.
try_seal_copy() {
  local at="" n
  if read_last_file "$WORK/seal.raw" && seal_parses "$WORK/seal.raw"; then
    at=$((SEAL_FILE_COUNT - 1))
    if [ "$SEAL_MARKER" != "$at" ] && ! short_ending "$at"; then
      echo "WARNING: File 0 says the seal marker is file $SEAL_MARKER, but the last file on the tape is" >&2
      echo "         a seal marker for $SEAL_FILE_COUNT files (file $at). Using the last file." >&2
    fi
  else
    # No end-of-data route (an mt without eod, or the last file is not a
    # seal): File 0's pointer, read with the same bound.
    seek_to "$SEAL_MARKER" || return 1
    read_bounded "$WORK/seal.raw" || return 1
    seal_parses "$WORK/seal.raw" || return 1
  fi
  check_requires "the seal marker" "$WORK/seal.toml"
  seal_keeps_rules || return 1
  parse_front_index_entries "$WORK/seal.toml" >"$WORK/files_from_seal.txt"
  [ -s "$WORK/files_from_seal.txt" ] || return 1
  check_file_list_consistency "$WORK/files_from_seal.txt" "$SEAL_FILE_COUNT" || return 1
  seal_ends_on_a_slice || return 1
  return 0
}

# A seal before File 0's pointer is the normal short ending of a tape that
# requires short-seal, when the seal requires it too (volume-format-v2.md 1.2
# rule 6): it draws no warning. Any other seal at another place does.
short_ending() { # <the seal's own position>
  [ "$TAPE_SHORT_SEAL" = 1 ] && [ "$SEAL_SHORT" = 1 ] && is_uint "$1" &&
    [ "${#1}" -le 15 ] && [ "$((10#$1))" -lt "$((10#$SEAL_MARKER))" ]
}

# The seal's parse rules (volume-format-v2.md 4.1) for a seal that states
# ended_after or requires short-seal, as every seal since version 1.6.0 does.
# S1: ended_after is file_count - 2, so the seal is the file after it. S2: a
# seal that requires short-seal states ended_after. A seal that states neither
# (every seal before 1.6.0) is a full seal that ended after file_count - 2
# (S3). S4 is check_file_list_consistency, for every seal, and S5 below. A seal
# that breaks one is not a seal (S6): the tape reads as unsealed. Sets
# SEAL_LAST.
SEAL_LAST=""
seal_keeps_rules() {
  SEAL_LAST=""
  if [ -z "$SEAL_ENDED_AFTER" ]; then
    if [ "$SEAL_SHORT" = 1 ]; then
      echo "NOTE: the seal marker requires short-seal but states no ended_after: it is not a valid seal" >&2
      echo "      (volume-format-v2.md 4.1, rule S2), and the tape is read as unsealed." >&2
      return 1
    fi
    [ "${#SEAL_FILE_COUNT}" -le 15 ] && SEAL_LAST=$((10#$SEAL_FILE_COUNT - 2))
    return 0
  fi
  if ! is_uint "$SEAL_ENDED_AFTER" || [ "${#SEAL_ENDED_AFTER}" -gt 15 ] ||
    [ "${#SEAL_FILE_COUNT}" -gt 15 ] ||
    [ "$((10#$SEAL_ENDED_AFTER + 2))" -ne "$((10#$SEAL_FILE_COUNT))" ]; then
    echo "NOTE: the seal marker states ended_after = $(safe_str "$SEAL_ENDED_AFTER") and file_count = $(safe_str "$SEAL_FILE_COUNT"):" >&2
    echo "      ended_after must be file_count - 2 (volume-format-v2.md 4.1, rule S1). It is not a valid" >&2
    echo "      seal, and the tape is read as unsealed." >&2
    return 1
  fi
  SEAL_LAST=$((10#$SEAL_ENDED_AFTER))
}

# S5: the last file before a seal that states ended_after is a data slice (a
# write never ends inside the metadata). Checked against the seal's own copy.
seal_ends_on_a_slice() {
  [ -n "$SEAL_ENDED_AFTER" ] || return 0
  local t
  t=$(file_type_at "$SEAL_LAST" "$WORK/files_from_seal.txt")
  [ "$t" = data_slice ] && return 0
  echo "NOTE: the seal marker says the tape ended after file $SEAL_LAST, which its copy of the front index" >&2
  echo "      lists as $(safe_str "${t:-nothing}"), not a data slice (volume-format-v2.md 4.1, rule S5). It is not" >&2
  echo "      a valid seal, and the tape is read as unsealed." >&2
  return 1
}

# Read at most STASH_MAX bytes of the file under the head into $1: enough
# for any seal marker, and it never copies a 10 GiB data slice into /tmp.
# The head is left unknown (it may stop inside the file). Returns 1 if
# nothing was read.
read_bounded() { # <out>
  CUR=""
  dd if="$DEVICE" of="$1" bs="$BLOCK" count=$((STASH_MAX / BLOCK)) 2>"$WORK/dd.err" || {
    echo "NOTE: reading the last file failed: $(dd_detail "$WORK/dd.err")" >&2
    return 1
  }
  [ -s "$1" ]
}

# The last file on the tape, found the way the seal marker is defined — the
# last file — rather than by File 0's pointer alone (#384): space to the end
# of data, then back over two filemarks to the start of the file before
# them. mt-st calls end of data `eod`, GNU mt `eom`. Returns 1 if mt cannot.
read_last_file() { # <out>
  CUR=""
  "$MT" -f "$DEVICE" eod 2>/dev/null || "$MT" -f "$DEVICE" eom 2>/dev/null || return 1
  "$MT" -f "$DEVICE" bsfm 2 2>/dev/null || return 1
  read_bounded "$1"
}

# Does $1 hold a seal marker this script can use? Sets SEAL_FRONT_INDEX_SHA256,
# SEAL_FILE_COUNT, SEAL_SEALED_AT, SEAL_ENDED_AFTER (as the seal states it;
# empty on a seal before 1.6.0), SEAL_SHORT (1 when the seal requires
# short-seal) and $WORK/seal.toml.
SEAL_ENDED_AFTER=""
SEAL_SHORT=0
seal_parses() { # <raw file>
  tr -d '\0' <"$1" >"$WORK/seal.stripped"
  grep '^\[seal\]' "$WORK/seal.stripped" >/dev/null 2>&1 || return 1
  sed -n '/^\[seal\]/,$p' "$WORK/seal.stripped" >"$WORK/seal.toml"
  SEAL_FRONT_INDEX_SHA256=$(toml_val "$WORK/seal.toml" front_index_sha256)
  SEAL_FILE_COUNT=$(toml_val "$WORK/seal.toml" file_count)
  SEAL_SEALED_AT=$(toml_val "$WORK/seal.toml" sealed_at)
  SEAL_ENDED_AFTER=$(toml_val "$WORK/seal.toml" ended_after)
  SEAL_SHORT=0
  if requires_feature "$WORK/seal.toml" short-seal; then
    SEAL_SHORT=1
  fi
  [ -n "$SEAL_FRONT_INDEX_SHA256" ] || return 1
  is_uint "$SEAL_FILE_COUNT" || return 1
  [ "$SEAL_FILE_COUNT" -gt 0 ]
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
     mt -f "$DEVICE" rewind && mt -f "$DEVICE" fsf 4
  2. Read one candidate file. Each read leaves the tape at the start of the
     next file, so repeat this step for each file in turn, moving forward:
       dd if="$DEVICE" bs=524288 of=candidate.raw
  3. Strip only the TRAILING zero bytes, which are block padding. Zero bytes
     inside the ciphertext are part of it: deleting every zero byte (as
     tr -d does) destroys the file. These four lines keep everything up to
     the last non-zero byte:
       size=$(stat -c %s candidate.raw)
       tail_len=$(tail -c 524288 candidate.raw | wc -c)
       keep=$(tail -c 524288 candidate.raw | od -An -v -tu1 | awk '{ for (i = 1; i <= NF; i++) { n++; if ($i != 0) last = n } } END { print last + 0 }')
       head -c $((size - tail_len + keep)) candidate.raw > candidate.stripped
  4. Try: age -d -i YOUR_KEY.age.key candidate.stripped > candidate.dec
     "no identity matched" means this file is not sealed to your key: go
     back to step 2 for the next file. Any other error: real ciphertext can
     end in a zero byte, so put one back and retry, up to four times:
       printf '\0' >> candidate.stripped
  5. Once a candidate decrypts, it is your tenant (or operator) envelope:
     untar it and read MANIFEST.toml for the exact tape position and byte
     size of every slice belonging to your unit(s); read each slice (steps
     1-2, with fsf to its position), trim it to that exact size with
     truncate -s SIZE instead of zero-stripping, decrypt it to restore.N.dar,
     then, in an empty directory: dar -x restore -R /destination -O -Q -N

See the system guide (File 1), "If All Else Fails", for the full narrative.
ZSEOF
}

# ---- where a tape that requires short-seal ended (volume-format-v2.md 4.1, 4.2) ----

# Everything a tape holds before its first data slice is the plan: File 0,
# the front index and the envelopes are written before the writer can know
# whether the drive will report the end of its medium. A tape written since
# version 1.6.0 requires short-seal: when that warning came in a data slice
# before the last, the write sealed the tape after it, with a short seal whose
# ended_after is the last file before it. Every planned file after that is not
# on the tape, and a unit with a slice there is cut: not on this tape whole.
#
# TAPE_SHORT_SEAL is 1 when File 0 requires short-seal. ENDED_AFTER is then
# the last file before the seal, once the seal has been read; it is empty on
# every other tape, and on one whose seal cannot be read (R2: that tape reads
# as unsealed, and the end of its data is the end of it).
TAPE_SHORT_SEAL=0
ENDED_AFTER=""

# Read the seal of a tape that requires short-seal, before any data slice
# (R1). It is the last file on the tape, found by spacing to the end of the
# data and back over two filemarks (volume-format-v2.md 1.2 rule 6), which
# costs one wind of the tape and a rewind after it. With 1, the seal was read
# already (the file map came from its copy) and is used as it is.
learn_end() { # [1: the seal has been read]
  ENDED_AFTER=""
  [ "$TAPE_SHORT_SEAL" = 1 ] || return 0
  if [ "${1:-0}" = 1 ] || try_seal_copy; then
    ENDED_AFTER=$SEAL_LAST
    return 0
  fi
  echo "WARNING: this tape requires short-seal, and no valid seal marker was found as its last file:" >&2
  echo "         it is read as an unsealed tape, and a planned file past the end of its data is not on it." >&2
}

# Did the tape end before the last data slice of its plan (File 0's
# seal_marker is the planned seal, so its last slice is the file before)?
tape_is_short() {
  [ -n "$ENDED_AFTER" ] && [ "$ENDED_AFTER" -lt "$((10#$SEAL_MARKER - 1))" ]
}

# On a short tape, the file map becomes the plan's files up to ENDED_AFTER
# and the seal marker after them, as the seal's own copy lists them: a planned
# file after the end is not in it, so no read can be sent there (R3).
trim_to_end() {
  tape_is_short || return 0
  awk -F'|' -v k="$ENDED_AFTER" '$1 + 0 <= k + 0' "$FILES_TXT" >"$WORK/files_on_tape.txt"
  echo "$((ENDED_AFTER + 1))|seal_marker||" >>"$WORK/files_on_tape.txt"
  FILES_TXT="$WORK/files_on_tape.txt"
  info "This tape ended early, after file $ENDED_AFTER: its seal marker is file $((ENDED_AFTER + 1)), and files"
  info "  $((ENDED_AFTER + 1)) to $((10#$SEAL_MARKER - 1)) of the plan are not on it. A unit with a slice among them is not on it whole."
}

# ---- establish the trusted file map (rung 1 -> rung 2 -> rung 3) ----

# Sets FILES_TXT and FILES_SOURCE on success; on total failure prints the
# zero-strip procedure and exits (used by modes that cannot proceed at all
# without a map: --find-envelope, --restore; --info and --verify apply the
# same ladder but handle a total failure themselves so they can still report
# a verdict). On a tape that requires short-seal it then reads the seal and
# keeps the map to the files on the tape, unless told --no-seal: --find-envelope
# reads only envelopes, which are always before the first data slice.
establish_files() { # [--no-seal]
  local seal=1
  [ "${1:-}" != --no-seal ] || seal=0
  bootstrap_thunk

  if try_front_index; then
    FILES_TXT="$WORK/files_from_index.txt"
    FILES_SOURCE="front_index"
    if [ "$seal" = 1 ]; then
      learn_end
      trim_to_end
    fi
    return 0
  fi

  echo "WARNING: front index (file $FRONT_INDEX) is unreadable or failed its self-consistency check — falling back to the seal marker's embedded copy (degradation ladder RUNG-2, volume-format-v2.md sec 4)." >&2

  if try_seal_copy; then
    FILES_TXT="$WORK/files_from_seal.txt"
    FILES_SOURCE="seal_embedded_copy(RUNG-2)"
    echo "WARNING: using the seal marker's embedded copy as the file map — the front of the tape may be damaged; treat this recovery as degraded." >&2
    if [ "$seal" = 1 ]; then
      learn_end 1
      trim_to_end
    fi
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
  # On a tape that requires short-seal the seal says where the tape ended;
  # a short seal is still SEALED (volume-format-v2.md 4.2).
  ENDED_AFTER=""
  if [ "$seal_ok" -eq 1 ] && [ "$TAPE_SHORT_SEAL" = 1 ]; then
    ENDED_AFTER=$SEAL_LAST
  fi

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
  if tape_is_short; then
    echo "  The tape ended early, after file $ENDED_AFTER: its seal marker, the last file, is"
    echo "  file $((ENDED_AFTER + 1)) (a short seal). Files $((ENDED_AFTER + 1)) to $((10#$SEAL_MARKER - 1)) of the plan are not on it,"
    echo "  and a unit with a slice among them is not on this tape whole (another tape holds it)."
  fi
  echo ""

  if [ -n "$table_file" ]; then
    echo "File map (source: $table_source):"
    local gone=0
    [ "$table_file" != "$WORK/files_from_index.txt" ] || ! tape_is_short || gone=1
    while IFS='|' read -r pos type size hash; do
      if [ "$gone" = 1 ] && [ "$pos" -gt "$ENDED_AFTER" ]; then
        printf "  %3d  %-24s  %8s bytes  %s  (planned; NOT ON TAPE)\n" "$pos" "$type" "${size:--}" "${hash:+${hash:0:16}...}"
        continue
      fi
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
  echo "To find the other tapes that hold a unit, or the tape that holds a file:"
  echo "  $0 --where UNIT --key YOUR_KEY.age.key"
  echo "  $0 --where --all --key YOUR_KEY.age.key     (every unit the index names)"
  echo "  $0 --find TEXT --key YOUR_KEY.age.key"
}

# ---- --verify ----

# A file that failed the walk. A data slice also says whether a copy of it is
# in a --slices-from directory (matched by its sha256 alone); one that is not
# is remembered for the second cartridge --swap-tapes asks for.
VERIFY_BAD=()
VERIFY_COVERED=0
VERIFY_OTHER=0
verify_failed() { # <position> <type> <bytes> <sha256> <what>
  local pos=$1 type=$2 size=$3 hash=$4 what=$5 extra=""
  if [ "$type" = data_slice ] && [ -n "$size" ]; then
    if dir_find "$pos" "$size" "$hash"; then
      extra="; a copy whose sha256 matches is $DIR_HIT"
      VERIFY_COVERED=$((VERIFY_COVERED + 1))
    else
      VERIFY_BAD+=("$pos|$size|$hash")
    fi
  else
    VERIFY_OTHER=$((VERIFY_OTHER + 1))
  fi
  printf "FAIL  file %3d  %-24s  (%s%s)\n" "$pos" "$type" "$what" "$extra"
}

verify_needed() { # job|number|bytes|sha256|label, for each failed slice with no copy yet
  local e pos size hash
  for e in ${VERIFY_BAD[@]+"${VERIFY_BAD[@]}"}; do
    IFS='|' read -r pos size hash <<<"$e"
    echo "v|$pos|$size|$hash|tape file $pos"
  done
}

do_verify() {
  bootstrap_thunk
  [ "$SALVAGE" = 0 ] || salvage_init

  # One pass, ascending, from a single rewind (#396). Files 1 and 2 lie
  # between File 0 and the front index, so their bytes are kept as the head
  # passes them and checked once the front index says what they should be.
  # The seal marker is the last file on the tape, so it is read last (#412),
  # after the walk, with the same verdict as before. Only when the front index
  # cannot be used is the seal read first, for its copy of the map. On a tape
  # that requires short-seal (since version 1.6.0) the seal is read right after
  # the front index, before any data slice (R1): it says where the tape ended,
  # and a planned file after that is NOT ON TAPE, which is not a failure.
  local p
  for p in 1 2; do
    stash_tape_file "$p" "$WORK/stash.$p" "$STASH_MAX" || rm -f "$WORK/stash.$p"
  done

  local fi_ok=0 seal_ok=0 seal_tried=0 files_file=""
  if try_front_index; then
    fi_ok=1
    files_file="$WORK/files_from_index.txt"
  elif try_seal_copy; then
    seal_ok=1
    seal_tried=1
    echo "WARNING: front index unreadable — verifying against the seal marker's embedded copy (degradation ladder RUNG-2)." >&2
    files_file="$WORK/files_from_seal.txt"
  else
    zero_strip_instructions >&2
    exit 1
  fi
  FILES_TXT=$files_file
  FILES_SOURCE=verify
  if [ "$TAPE_SHORT_SEAL" = 1 ]; then
    if [ "$seal_tried" = 0 ]; then
      seal_tried=1
      try_seal_copy && seal_ok=1
    fi
    [ "$seal_ok" -ne 1 ] || ENDED_AFTER=$SEAL_LAST
  fi

  echo "=== keyless integrity walk: $LABEL ==="
  echo ""

  local overall_ok=1 not_on_tape=0

  while IFS='|' read -r pos type size hash; do
    # A planned file after the end of a short tape is not on it (R3): it is
    # named and never read. Only the front index lists one.
    if [ "$fi_ok" -eq 1 ] && tape_is_short && is_uint "$pos" &&
      [ "$pos" -gt "$ENDED_AFTER" ]; then
      if [ "$type" = seal_marker ]; then
        printf "NOT ON TAPE  file %3d  %-24s  (planned; the seal marker is file %d)\n" "$pos" "$type" "$((ENDED_AFTER + 1))"
      else
        printf "NOT ON TAPE  file %3d  %-24s\n" "$pos" "$type"
      fi
      not_on_tape=$((not_on_tape + 1))
      continue
    fi
    # Entries with no hash are self-referential (front_index's own entry in
    # the front-index-sourced list, and the seal marker's own entry always)
    # — nothing on the tape hashes them, so there is nothing to compare.
    [ -z "$hash" ] && continue

    require_uint "position(@$type)" "$pos"
    [ -z "$size" ] || require_uint "size_bytes(@$pos)" "$size"

    local actual=""
    if ! actual=$(stashed_sha "$pos" "$size"); then
      if hash_tape_file "$pos" "$size"; then
        actual=$TAPE_SHA
      else
        verify_failed "$pos" "$type" "$size" "$hash" "unreadable"
        overall_ok=0
        continue
      fi
    fi

    if [ "$actual" = "$hash" ]; then
      printf "PASS  file %3d  %-24s\n" "$pos" "$type"
    else
      verify_failed "$pos" "$type" "$size" "$hash" "hash mismatch"
      overall_ok=0
    fi
  done <"$files_file"

  # The seal marker, last. File 3 vs the seal binding is the one entry the
  # per-file loop above cannot check when the list came from the front index
  # itself (its own entry carries no hash there — self-reference).
  if [ "$fi_ok" -eq 1 ] && [ "$seal_tried" = 0 ] && try_seal_copy; then
    seal_ok=1
  fi
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
    echo "WARNING: no valid seal marker — this tape is UNSEALED; completeness cannot be confirmed. Only the files above were checked." >&2
  fi

  echo ""
  if [ "$overall_ok" -eq 1 ] && [ "$not_on_tape" -gt 0 ]; then
    echo "VERIFY: PASS — every file on this tape matches the front index. The tape ended early, after"
    echo "        file $ENDED_AFTER (its seal marker is file $((ENDED_AFTER + 1))): the $not_on_tape planned file(s) marked NOT ON TAPE"
    echo "        are not on it, which is not a failure. A unit with a slice among them is on another tape."
  elif [ "$overall_ok" -eq 1 ]; then
    echo "VERIFY: PASS — every file matches the front index."
  else
    if [ "$SWAP_TAPES" = 1 ] && [ ${#VERIFY_BAD[@]} -gt 0 ]; then
      VERIFY_ONLY=1
      swap_loop
    fi
    echo "VERIFY: FAIL — one or more files do not match. See FAIL lines above." >&2
    if [ "$SALVAGE" = 1 ]; then
      if [ "$VERIFY_OTHER" -eq 0 ] && [ ${#VERIFY_BAD[@]} -eq 0 ]; then
        echo "        Every file that failed is a data slice, and a copy of each was found (in a --slices-from" >&2
        echo "        directory or on a cartridge you loaded): --restore with the same options takes them from there." >&2
      else
        echo "        $VERIFY_OTHER failed file(s) are not data slices, and ${#VERIFY_BAD[@]} failed data slice(s) have no copy yet." >&2
      fi
    fi
    exit 1
  fi
}

# ---- envelopes ----

# The three "no envelope opened" exits share one shape (issues #288, #291):
# which keys were tried, what tape is actually in the drive, and the rotation
# fact that makes a correct-looking key fail on a correct tape. They live here,
# in one function, because the --unit exit — the one an heir is most likely to
# hit — had already drifted into a bare one-liner with none of it. When an
# envelope a key DID match turned out damaged (#412), that is said instead of
# the rotation hint: it is not a key problem.
die_no_envelope() { # [unit]
  local headline="no envelope matched $(keys_phrase)"
  if [ -n "${1:-}" ]; then
    headline="no envelope for unit '$1' matched $(keys_phrase)"
  fi
  if [ "$MATCHED_DAMAGED" -gt 0 ]; then
    die "$headline that could be opened.
       $DAMAGED_ENVELOPES envelope(s) on this tape are DAMAGED (see the WARNING
       lines above): a key matched, so this is not a wrong key. Another copy of
       this volume, or the operator or escrow key (which open the operator
       envelope and its backup), can still recover the data.
       Keys tried: $(keys_list)"
  fi
  if [ "$DAMAGED_ENVELOPES" -gt 0 ]; then
    die "$headline that could be opened.
       $DAMAGED_ENVELOPES envelope(s) on this tape are DAMAGED (see the WARNING
       lines above), but no key opened any of them: they could not be read or
       failed their hash, so this is damage, a key that is not a recipient, or
       both. Another copy of this volume, or the operator or escrow key (which
       open the operator envelope and its backup), can still recover the data.
       Tape in $DEVICE identifies as '${TAPE_LABEL:-<unreadable>}'; this script is for '$LABEL'.
       If you hold other keys, try them: an envelope is sealed with the key
       that was active when THIS tape was written.
       Keys tried: $(keys_list)"
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

# Read the envelope at tape file $1 and try each key on it, on its own
# (#288). Returns 0 when a key opened it (ENV_KEY names the key; the files
# are in $WORK/env), 1 when no key matched it, 2 when it is DAMAGED: a key
# matched but the payload would not decrypt or unpack (#412). age names the
# difference — "no identity matched" is a key that is not a recipient — and
# the front index's keyless hash says so before any key is tried.
ENV_KEY=""
DAMAGED_ENVELOPES=0
# How many of those a key MATCHED (age got past the header, the payload would
# not decrypt). Only then is "not a wrong key" true; an envelope that would not
# read, or failed its hash with no key opening it, says nothing about the keys.
MATCHED_DAMAGED=0
try_envelope() { # <position>
  local pos=$1 esize ehash actual keyfile mismatch=0
  ENV_KEY=""
  # A file that will not read is damaged too, not the end of the run: its
  # backup further on holds the same bytes. A full disk here is not the
  # tape's fault, and no backup would fare better, so that still stops.
  : >"$WORK/dd.err"
  if ! try_read_tape_raw "$pos" "$WORK/envelope.enc"; then
    if grep -i 'no space left' "$WORK/dd.err" >/dev/null 2>&1; then
      die_io "reading tape file $pos failed" "$WORK/dd.err" "$WORK"
    fi
    DAMAGED_ENVELOPES=$((DAMAGED_ENVELOPES + 1))
    echo "WARNING: the envelope at file $pos could not be read from the tape: it is DAMAGED" >&2
    return 2
  fi
  esize=$(file_size_at "$pos" "$FILES_TXT")
  ehash=$(file_hash_at "$pos" "$FILES_TXT")
  if [ -n "$esize" ]; then
    require_uint "size_bytes(@$pos)" "$esize"
    [ "$esize" -gt 0 ] && truncate -s "$esize" "$WORK/envelope.enc"
  fi
  if [ -n "$ehash" ]; then
    actual=$(sha256sum "$WORK/envelope.enc" | awk '{print $1}')
    if [ "$actual" != "$ehash" ]; then
      mismatch=1
      echo "WARNING: the envelope at file $pos does not match the front index: it is DAMAGED" >&2
      echo "         (sha256 ${actual:0:16}..., the front index says ${ehash:0:16}...). Trying it anyway." >&2
    fi
  fi
  for keyfile in ${KEYS[@]+"${KEYS[@]}"}; do
    rm -rf "$WORK/env" && mkdir -p "$WORK/env"
    if age -d -i "$keyfile" <"$WORK/envelope.enc" 2>"$WORK/age.err" |
      tar xf - -C "$WORK/env/" 2>"$WORK/tar.err"; then
      ENV_KEY=$keyfile
      return 0
    fi
    if grep 'no identity matched' "$WORK/age.err" >/dev/null 2>&1; then
      info "  key $keyfile did not open the envelope at file $pos"
      continue
    fi
    # The key matched (age got past the header), so no other key can do
    # better: these bytes are damaged.
    DAMAGED_ENVELOPES=$((DAMAGED_ENVELOPES + 1))
    MATCHED_DAMAGED=$((MATCHED_DAMAGED + 1))
    echo "WARNING: key $keyfile matched the envelope at file $pos, but the envelope is DAMAGED:" >&2
    cat "$WORK/age.err" "$WORK/tar.err" 2>/dev/null | sed 's/^/         /' >&2 || true
    return 2
  done
  if [ "$mismatch" = 1 ]; then
    DAMAGED_ENVELOPES=$((DAMAGED_ENVELOPES + 1))
    return 2
  fi
  return 1
}

# The work directory's own need, as far as the front index tells it: every
# envelope is read whole into $WORK (block padded), and each one a key opens
# is kept there decrypted, about its size again, until the run ends. Which
# envelopes a key opens is not known before it tries, so this is an upper
# bound (the largest, read whole, plus every envelope kept) and only a
# WARNING; a full disk is named when it happens (die_io), and TMPDIR moves
# the work directory. The sizes come from a plaintext zone, so the sum is
# done in awk, where a crafted figure cannot overflow shell arithmetic.
check_work_space() {
  [ "$SKIP_SPACE_CHECK" = 0 ] || return 0
  local need free
  need=$(awk -F'|' -v blk="$BLOCK" '
    ($2 == "tenant_envelope" || $2 == "operator_envelope") && $3 ~ /^[0-9]+$/ {
      sum += $3
      if ($3 + 0 > max) max = $3 + 0
    }
    END { if (sum > 0) printf "%.0f\n", sum + max + blk }
  ' "$FILES_TXT")
  [ -n "$need" ] || return 0
  free=$(free_bytes "$WORK")
  is_uint "$free" || return 0
  if ! awk -v have="$free" -v want="$need" 'BEGIN { exit (have + 0 >= want + 0) ? 0 : 1 }'; then
    echo "WARNING: the work directory $WORK has $(size_str "$free") free, and reading the" >&2
    echo "         envelopes on this tape may need up to $(size_str "$need") there: each envelope is read whole," >&2
    echo "         and each one a key opens stays decrypted until the run ends. If it fills, the run" >&2
    echo "         stops and says so; set TMPDIR to a directory on a larger disk to move the work" >&2
    echo "         directory (a key that opens one small envelope needs far less than this)." >&2
  fi
}

# Walk the envelopes in tape order — forward only — trying the keys on each,
# and call "$1 <position>" for each that opens; it returns 1 to stop the
# walk. A file whose bytes equal one already tried (an envelope's backup, by
# its front-index hash) is skipped unless that one was damaged or would not
# read: then the backup is what is read instead. With a second argument the
# walk RESUMES after that position (a walk a callback stopped), remembering
# what the first one tried.
WALK_SEEN=" "
walk_envelopes() { # <on-open callback> [<resume after this position>]
  local cb=$1 after=${2:-} pos h rc ty
  if [ -z "$after" ]; then
    WALK_SEEN=" "
    check_work_space
  fi
  while IFS= read -r pos; do
    require_uint envelope_position "$pos"
    if [ -n "$after" ] && [ "$pos" -le "$after" ]; then
      continue
    fi
    h=$(file_hash_at "$pos" "$FILES_TXT")
    case "$WALK_SEEN" in
    *" $h "*)
      [ -z "$h" ] || {
        info "File $pos holds the same bytes as an envelope already tried (its backup): skipped"
        continue
      }
      ;;
    esac
    info "Trying envelope at file $pos..."
    rc=0
    try_envelope "$pos" || rc=$?
    [ "$rc" = 2 ] || [ -z "$h" ] || WALK_SEEN="$WALK_SEEN$h "
    if [ "$rc" = 0 ]; then
      ty=$(file_type_at "$pos" "$FILES_TXT")
      case "$ty" in
      *_backup) info "File $pos is a backup copy, read because the envelope it copies could not be used" ;;
      esac
      rm -rf "$WORK/opened.$pos"
      mv "$WORK/env" "$WORK/opened.$pos"
      "$cb" "$pos" || break
    fi
  done < <(envelope_positions "$FILES_TXT")
}

# One line per unit an envelope lists: unit, versions, slices, size.
units_table() { # <MANIFEST.toml>
  printf "    %-32s  %-10s  %6s  %10s\n" "Unit" "Versions" "Slices" "Size"
  awk '
    /^\[\[units\]\]/ { in_head = 1; cur = ""; next }
    /^\[\[units\.slices\]\]/ { in_head = 0; if (cur != "") n[cur]++; next }
    /^\[/ { in_head = 0 }
    in_head && /^name = / {
      v = $3; gsub(/"/, "", v); cur = v
      if (!(cur in seen)) { seen[cur] = 1; order[++k] = cur }
    }
    in_head && /^snapshot_version = / && cur != "" {
      vers[cur] = (vers[cur] == "") ? $3 : vers[cur] "," $3
    }
    /^encrypted_bytes = / && cur != "" { b[cur] += $3 }
    END {
      for (i = 1; i <= k; i++) {
        u = order[i]
        printf "%s|%s|%d|%.0f\n", u, vers[u], n[u], b[u]
      }
    }
  ' "$1" | while IFS='|' read -r name versions nslices bytes; do
    printf "    %-32s  %-10s  %6s  %10s\n" "$name" "$versions" "$nslices" "$(size_str "$bytes")"
  done
}

# ---- --find-envelope ----

FOUND_ENVELOPES=()
found_envelope() { # <position>
  local pos=$1 m="$WORK/opened.$1/MANIFEST.toml" who
  FOUND_ENVELOPES+=("$pos")
  who=$(safe_str "$(toml_val "$m" tenant 2>/dev/null)")
  echo ""
  info "Decrypted envelope at file $pos${who:+ (tenant: $who)}"
  info "  opened with key $ENV_KEY"
  if [ -f "$m" ]; then
    units_table "$m"
  fi
  return 0
}

do_find_envelope() {
  # Every envelope lies before the first data slice, so this mode never needs
  # to know where a tape that requires short-seal ended: it reads no seal.
  establish_files --no-seal

  # Every envelope, not just the first that opens (#412): an operator or
  # escrow key opens them all, and the operator envelope lists every unit.
  walk_envelopes found_envelope
  [ "${#FOUND_ENVELOPES[@]}" -gt 0 ] || die_no_envelope

  if [ "${#FOUND_ENVELOPES[@]}" -eq 1 ]; then
    local d="$WORK/opened.${FOUND_ENVELOPES[0]}"
    if [ -f "$d/MANIFEST.toml" ]; then
      echo ""
      echo "--- MANIFEST.toml ---"
      cat "$d/MANIFEST.toml"
    fi
    if [ -f "$d/RECOVERY.md" ]; then
      echo ""
      echo "--- RECOVERY.md ---"
      cat "$d/RECOVERY.md"
    fi
  else
    echo ""
    echo "${#FOUND_ENVELOPES[@]} envelopes opened. The operator envelope (an operator or escrow"
    echo "key opens it) lists every unit on this tape; --restore --unit NAME finds the"
    echo "envelope that holds NAME, and --all restores every unit of the largest one."
  fi
  echo ""
  echo "To restore, run:"
  echo "  $0 --restore $(keys_args) --to /your/destination"
}

# ---- --restore ----

# Does an envelope MANIFEST.toml list a given unit under [[units]]? Exit 0 if
# so. Used by --restore to pick the right envelope for a universal key (#127).
manifest_has_unit() { # <manifest_path> <unit_name>
  awk -v u="$2" '
    /^\[\[units\]\]/ { in_u = 1; next }
    in_u && /^name = / { gsub(/"/, "", $3); if ($3 == u) found = 1; in_u = 0 }
    /^\[/             { in_u = 0 }
    END { exit(found ? 0 : 1) }
  ' "$1"
}

# A failed slice must say whether the keys or the bytes are to blame (#288).
die_slice_keys() { # <slice number>
  die "cannot decrypt slice $1 — none of the ${#KEYS[@]} key(s) decrypted it
       Keys tried: $(keys_list)
       The key that opened the envelope need not be the key that opens the
       slices: the envelope is sealed when the volume is written, each slice
       when it was staged, so a key rotation between the two puts them on
       different key generations. Supply the older key as well — repeat
       --key once per key.
       If this is the right key, the slice itself may be damaged (a damaged
       header reads this way too): --verify checks it without a key, and
       --slices-from DIR or --swap-tapes take it from another copy."
}

# Every --key as age identities. A slice is decrypted ONCE, with all of them:
# age tries each against the slice's recipients itself. Streamed from tape,
# a slice cannot be re-read for a second key, so the per-key retry of 1.0.x
# (which re-read a decrypted-to-disk slice per key) is gone; age's own
# "no identity matched any of the recipients" still names a missing key.
AGE_IDS=()
set_age_ids() {
  local k
  AGE_IDS=()
  for k in ${KEYS[@]+"${KEYS[@]}"}; do
    AGE_IDS+=(-i "$k")
  done
}

# Decrypt one slice file to another (--path, which keeps the few slices it
# needs on disk). A full disk is named as one, never as a key problem. With a
# size, only the first that many bytes of the file are the slice (a copy made
# with dd carries the tape's block padding after them).
decrypt_slice() { # <ciphertext> <plaintext-out> <slice-number> [bytes]
  local in=$1 out=$2 num=$3 size=${4:-}
  if [ -z "$size" ]; then
    if age -d "${AGE_IDS[@]}" <"$in" >"$out" 2>"$WORK/age.err"; then
      return 0
    fi
  else
    if head -c "$size" -- "$in" | age -d "${AGE_IDS[@]}" >"$out" 2>"$WORK/age.err"; then
      return 0
    fi
  fi
  cat "$WORK/age.err" >&2 || true
  if grep -i 'no space left' "$WORK/age.err" >/dev/null 2>&1; then
    rm -f "$out"
    die_io "decrypting slice $num failed" "$WORK/age.err" "$(dirname "$out")"
  fi
  rm -f "$out"
  if grep 'no identity matched' "$WORK/age.err" >/dev/null 2>&1; then
    die_slice_keys "$num"
  fi
  die "slice $num did not decrypt: a key matched it, so its bytes are damaged."
}

# Wait for background job $1, polling; if dar ($DAR_PID) exits first, return
# 3, so the caller can stop feeding a dar that is gone.
wait_feed() { # <pid>
  while kill -0 "$1" 2>/dev/null; do
    if ! kill -0 "$DAR_PID" 2>/dev/null; then
      return 3
    fi
    sleep 0.2 2>/dev/null || sleep 1
  done
  return 0
}

# dar is gone: say why, with its own words. One exit is not an error: dar
# refusing the envelope's catalogue as not this archive's (a re-staged
# snapshot, #419), which it does before writing anything. That sets
# STREAM_REFUSED and returns, and the caller restores another way.
STREAM_REFUSED=0
die_dar() { # <destination>
  local rc=0
  wait "$DAR_PID" 2>/dev/null || rc=$?
  DAR_PID=""
  CUR=""
  if grep 'do not correspond to the same data' "$WORK/extract.log" >/dev/null 2>&1; then
    STREAM_REFUSED=1
    return 0
  fi
  cat "$WORK/extract.log" >&2 2>/dev/null || true
  if grep -i 'no space left' "$WORK/extract.log" >/dev/null 2>&1; then
    die_io "dar extraction failed" "$WORK/extract.log" "$1"
  fi
  die "dar extraction failed (exit $rc) — dar's own message is above."
}

# Stream one unit's slices off the tape, through age, into dar (#412 item 1).
# dar reads the slices from FIFOs with --sequential-read, so nothing decrypted
# is written to disk except the restored files: the 1.0.x restore decrypted
# every slice to disk first and needed about twice the unit's size free. Each
# slice's ciphertext is hashed as it passes and checked against the front
# index, and age authenticates every 64 KiB as it decrypts, so damaged bytes
# stop the restore rather than reach dar. Whatever fails first is named: dar
# (its own output), the tape read (dd's), or age (a missing key, or damage).
stream_unit() { # <slices file: num|pos|size|sha per line> <destination> [catalogue]
  local list=$1 destdir=$2 cat=${3:-} fifos="$WORK/stream" i n
  STREAM_REFUSED=0
  local -a nums=() poss=() sizes=() shas=()
  local num tpos size sha
  while IFS='|' read -r num tpos size sha; do
    nums+=("$num")
    poss+=("$tpos")
    sizes+=("$size")
    shas+=("$sha")
  done <"$list"
  n=${#nums[@]}
  rm -rf "$fifos" && mkdir -p "$fifos"
  for num in "${nums[@]}"; do
    mkfifo "$fifos/restore.$num.dar"
  done
  mkfifo "$fifos/cipher"

  local -a dar_opts=(-O -Q -N --sequential-read)
  [ "$OVERWRITE" = 0 ] || dar_opts+=(-w)
  # The isolated catalogue, when there is one: dar 2.7.13 (Debian's and
  # Ubuntu's) reading sliced FIFOs sequentially without it cuts short a file
  # that runs into the last slice, for most slice sizes; with it the restore
  # is exact (measured with #411; fixed in dar 2.7.21).
  [ -z "$cat" ] || dar_opts+=(-A "$cat")
  dar -x "$fifos/restore" -R "$(abs_dir "$destdir")" "${dar_opts[@]}" >"$WORK/extract.log" 2>&1 </dev/null &
  DAR_PID=$!
  BG_PIDS+=("$DAR_PID")

  local hpid wpid st got
  for ((i = 0; i < n; i++)); do
    num=${nums[$i]} tpos=${poss[$i]} size=${sizes[$i]} sha=${shas[$i]}
    info "Slice $((i + 1))/$n — tape file $tpos"
    if ! kill -0 "$DAR_PID" 2>/dev/null; then
      die_dar "$destdir"
      return 0
    fi
    seek_to "$tpos" || die "cannot position the tape at file $tpos ($MT failed)"
    CUR=""
    sha256sum <"$fifos/cipher" >"$WORK/slice.sha" 2>/dev/null &
    hpid=$!
    BG_PIDS+=("$hpid")
    : >"$WORK/feed.status"
    (
      # Not -e here: the pipeline's statuses are the result, written below.
      set +e
      dd if="$DEVICE" bs="$BLOCK" 2>"$WORK/dd.err" </dev/null |
        {
          head -c "$size"
          cat >/dev/null
        } | tee "$fifos/cipher" |
        age -d "${AGE_IDS[@]}" 2>"$WORK/age.err" >"$fifos/restore.$num.dar"
      echo "${PIPESTATUS[*]}" >"$WORK/feed.status"
    ) </dev/null &
    wpid=$!
    if ! wait_feed "$wpid"; then
      # dar is gone. A feed that is opening, or about to open, this slice's
      # FIFO waits for a reader that will never come. Open it from the
      # other end in the background and close it at once: the feed's open
      # completes, its next write breaks the pipe, and it ends.
      (: <"$fifos/restore.$num.dar") 2>/dev/null &
      local opener=$!
      BG_PIDS+=("$opener")
      wait "$wpid" 2>/dev/null || true
      wait "$hpid" 2>/dev/null || true
      kill -KILL "$opener" 2>/dev/null || true
      die_dar "$destdir"
      return 0
    fi
    wait "$wpid" 2>/dev/null || true
    wait "$hpid" 2>/dev/null || true
    read -r -a st <"$WORK/feed.status" || true
    # st: dd, the head/drain group, tee, age.
    if [ "${st[0]:-1}" != 0 ]; then
      echo "NOTE: a slice that will not read can come from another copy of the unit: see --slices-from and --swap-tapes in --help." >&2
      die_io "reading tape file $tpos failed" "$WORK/dd.err" "$WORK"
    fi
    if [ "${st[3]:-1}" != 0 ]; then
      # age's own complaint about the slice (a damaged chunk, a header no key
      # opens) is what the heir needs to read, even when dar, seeing the slice
      # cut short, has already died of it. Only an age that failed to WRITE
      # (dar gone first) hands the word to dar.
      if ! grep -E 'decrypt|authenticat|no identity matched|header|MAC' "$WORK/age.err" >/dev/null 2>&1 &&
        ! kill -0 "$DAR_PID" 2>/dev/null; then
        die_dar "$destdir"
        return 0
      fi
      cat "$WORK/age.err" >&2 || true
      if grep 'no identity matched' "$WORK/age.err" >/dev/null 2>&1; then
        die_slice_keys "$num"
      fi
      die "slice $num (tape file $tpos) did not decrypt: a key matched it, so its bytes are
       damaged. Files already restored to $destdir from this unit may be incomplete.
$(replace_hint)"
    fi
    if [ "${st[1]:-1}" != 0 ] || [ "${st[2]:-1}" != 0 ]; then
      if ! kill -0 "$DAR_PID" 2>/dev/null; then
        die_dar "$destdir"
        return 0
      fi
      die "slice $num (tape file $tpos) could not be passed to dar (status ${st[*]})"
    fi
    CUR=$((tpos + 1))
    got=$(awk '{print $1}' "$WORK/slice.sha")
    if [ "$got" != "$sha" ]; then
      die "slice $num checksum MISMATCH (expected ${sha:0:16}…, got ${got:0:16}…)
       The slice does not match the front index. dar has already taken what
       decrypted of it: do not trust the files restored to $destdir.
$(replace_hint)"
    fi
    info "  checksum verified against front index"
  done

  local rc=0
  wait "$DAR_PID" 2>/dev/null || rc=$?
  DAR_PID=""
  if [ "$rc" != 0 ] && grep 'do not correspond to the same data' "$WORK/extract.log" >/dev/null 2>&1; then
    STREAM_REFUSED=1
    return 0
  fi
  dar_result "$WORK/extract.log" "$rc" "$destdir"
}

# Read, check and decrypt the slices listed in $1 as restore.N.dar, one
# encrypted slice on disk at a time, into directory $2 (default SCRATCH). A
# slice that fails stops the run, unless a replacement was asked for
# (SALVAGE): then it is looked for, by its sha256, in the --slices-from
# directories, and one not found is written to the file $3 (number | tape file
# | bytes | sha256) for the second tape, and the rest are read on.
spool_slices() { # <slices file> [<directory> [<file for the slices not found>]]
  local list=$1 into=${2:-$SCRATCH} missing=${3:-} num tpos size sha actual i=0 n
  [ -z "$missing" ] || : >"$missing"
  n=$(wc -l <"$list")
  while IFS='|' read -r num tpos size sha; do
    i=$((i + 1))
    info "Slice $num ($i/$n) — tape file $tpos"
    if [ "$SALVAGE" = 1 ]; then
      if read_slice_checked "$tpos" "$size" "$sha" "$SCRATCH/slice.enc"; then
        info "  checksum verified against front index"
        decrypt_slice "$SCRATCH/slice.enc" "$into/restore.$num.dar" "$num"
        rm -f "$SCRATCH/slice.enc"
      else
        info "  slice $num (tape file $tpos) $SLICE_WHY"
        if dir_find "$num" "$size" "$sha"; then
          info "  taking slice $num from $DIR_HIT (its sha256 is the one the front index lists)"
          decrypt_slice "$DIR_HIT" "$into/restore.$num.dar" "$num" "$size"
        else
          echo "$num|$tpos|$size|$sha" >>"$missing"
          info "  no copy of slice $num in the --slices-from directories"
        fi
      fi
      continue
    fi
    read_tape_raw "$tpos" "$SCRATCH/slice.enc"
    truncate -s "$size" "$SCRATCH/slice.enc"
    actual=$(sha256sum "$SCRATCH/slice.enc" | awk '{print $1}')
    [ "$actual" = "$sha" ] ||
      die "slice $num checksum MISMATCH (expected ${sha:0:16}…, got ${actual:0:16}…)
$(replace_hint)"
    info "  checksum verified against front index"
    decrypt_slice "$SCRATCH/slice.enc" "$into/restore.$num.dar" "$num"
    rm -f "$SCRATCH/slice.enc"
  done <"$list"
}

# The way 1.0.x restored, for a dar that cannot stream a unit safely: every
# slice decrypted to disk first, then dar reads them directly (not
# sequentially), which needs no catalogue. Needs room for the slices as well
# as the files, checked before the first slice is read.
spool_unit() { # <slices file> <destination>
  local list=$1 destdir=$2 bytes=0 num tpos size sha
  while IFS='|' read -r num tpos size sha; do
    bytes=$((bytes + size))
  done <"$list"
  check_space "$destdir" "$SCRATCH_PARENT" "$bytes" "$((bytes + BLOCK))"
  SCRATCH="$(mktemp -d "$SCRATCH_PARENT/.tapectl-restore.XXXXXX")" ||
    die "cannot create a scratch directory in $SCRATCH_PARENT"
  spool_slices "$list"
  local -a dar_opts=(-O -Q -N)
  local rc=0
  [ "$OVERWRITE" = 0 ] || dar_opts+=(-w)
  dar -x "$SCRATCH/restore" -R "$(abs_dir "$destdir")" "${dar_opts[@]}" >"$WORK/extract.log" 2>&1 </dev/null || rc=$?
  rm -rf "$SCRATCH"
  SCRATCH=""
  dar_result "$WORK/extract.log" "$rc" "$destdir"
}

# Restore one unit the best way this dar allows. Streaming needs the unit's
# isolated catalogue from the envelope with dar 2.7.9 or later (sliced
# sequential reading with a catalogue works from there), or no catalogue with
# dar 2.7.21 or later (which fixed reading without one). A catalogue dar
# refuses (#419) is dropped, and anything else is restored from slices
# decrypted to disk first.
restore_unit_plain() { # <slices file> <destination> <unit> <version>
  local list=$1 destdir=$2 unit=$3 ver=$4 cat
  cat=$(catalogue_path "$CHOSEN_ENV" "$unit" "$ver")
  if [ -n "$cat" ] && dar_at_least 2.7.9; then
    stream_unit "$list" "$destdir" "$cat"
    [ "$STREAM_REFUSED" = 1 ] || return 0
    echo "NOTE: dar refused the envelope's catalogue for '$unit': it belongs to another" >&2
    echo "      staging run of the same snapshot (#419). Nothing was written; restoring" >&2
    echo "      '$unit' without it." >&2
  fi
  if dar_at_least 2.7.21; then
    stream_unit "$list" "$destdir" ""
    [ "$STREAM_REFUSED" = 0 ] || die "dar refused to read '$unit' — see its message above"
    return 0
  fi
  info "dar ${DAR_VERSION:-(unknown version)} cannot stream '$unit' safely without its catalogue;"
  info "  decrypting the slices to disk first (dar 2.7.21 or later would stream it)"
  spool_unit "$list" "$destdir"
}

# One unit, whichever way: streamed, from slices on disk, or (when a
# replacement slice was asked for) from slices on disk with the bad ones
# replaced. UNIT_OK says whether dar named files it could not restore.
restore_unit() { # <slices file> <destination> <unit> <version>
  local list=$1 destdir=$2 unit=$3 ver=$4
  UNIT_NAME=$unit
  UNIT_VER=$ver
  UNIT_OK=1
  UNIT_UUID=$(unit_uuid_of "$CHOSEN_ENV" "$unit" "$ver")
  if [ "$SALVAGE" = 1 ]; then
    salvage_unit "$list" "$destdir" "$unit" "$ver"
    return 0
  fi
  restore_unit_plain "$list" "$destdir" "$unit" "$ver"
  if [ "$UNIT_OK" = 1 ]; then
    DONE_LINES+=("Unit '$unit' restored to: $destdir")
  fi
}

# The envelope --restore uses (walk_envelopes calls this for each that
# opens): the first that lists every unit asked for (#127: an escrow key opens
# them all, and the first may be another tenant's), or with --all the one
# that lists the most units — for an operator or escrow key, the operator
# envelope, which lists every unit on the tape.
RESTORE_UNITS=()
RESTORE_ALL=0
CHOSEN_ENV=""
CHOSEN_COUNT=0
CHOSEN_KEY=""
CHOSEN_POS=""
pick_for_restore() { # <position>
  local pos=$1 m="$WORK/opened.$1/MANIFEST.toml" u count
  [ -f "$m" ] || return 0
  if [ "$RESTORE_ALL" = 1 ]; then
    count=$(awk '
    /^\[\[units\]\]/ { in_u = 1; next }
    # !seen[] — a volume can carry the SAME unit in several snapshot versions,
    # one [[units]] block each. Listing it once per block made a single unit
    # look like several and pushed the plain --restore (no --unit) form into
    # the "multiple units" branch, printing the same name twice (#133).
    in_u && /^name = / { gsub(/"/, "", $3); if (!seen[$3]++) print $3; in_u = 0 }
    /^\[/              { in_u = 0 }
  ' "$m" | wc -l)
    if [ -z "$CHOSEN_ENV" ] || [ "$count" -gt "$CHOSEN_COUNT" ]; then
      CHOSEN_ENV="$WORK/opened.$pos" CHOSEN_COUNT=$count CHOSEN_KEY=$ENV_KEY CHOSEN_POS=$pos
    fi
    return 0
  fi
  for u in ${RESTORE_UNITS[@]+"${RESTORE_UNITS[@]}"}; do
    if ! manifest_has_unit "$m" "$u"; then
      info "Envelope at file $pos decrypts but does not list '$u'; continuing..."
      return 0
    fi
  done
  CHOSEN_ENV="$WORK/opened.$pos" CHOSEN_KEY=$ENV_KEY CHOSEN_POS=$pos
  return 1
}

# A restore that names no unit and no --all restores the one unit the first
# envelope its key opens lists, when it lists exactly one (the default is
# deliberate). The same key may open more envelopes that list more units, and
# an heir who sees RESTORE COMPLETE would take the tape for that one unit. So,
# before a single slice is read, the envelopes after the one used are looked
# at, only to count what they list besides it. They all come before every
# slice, so this moves the tape forward only; each is dropped once counted.
LONE_UNIT=""
OTHER_ENVELOPES=0
OTHER_UNITS=0
OTHER_BIGGEST=0
count_other_envelope() { # <position>
  local pos=$1 m="$WORK/opened.$1/MANIFEST.toml" n
  if [ -f "$m" ]; then
    OTHER_ENVELOPES=$((OTHER_ENVELOPES + 1))
    awk '
    /^\[\[units\]\]/ { in_u = 1; next }
    # !seen[] — a volume can carry the SAME unit in several snapshot versions,
    # one [[units]] block each. Listing it once per block made a single unit
    # look like several and pushed the plain --restore (no --unit) form into
    # the "multiple units" branch, printing the same name twice (#133).
    in_u && /^name = / { gsub(/"/, "", $3); if (!seen[$3]++) print $3; in_u = 0 }
    /^\[/              { in_u = 0 }
  ' "$m" >"$WORK/listed.txt"
    cat "$WORK/listed.txt" >>"$WORK/other_units.txt"
    n=$(wc -l <"$WORK/listed.txt")
    [ "$n" -le "$OTHER_BIGGEST" ] || OTHER_BIGGEST=$n
  fi
  rm -rf "$WORK/opened.$pos"
  return 0
}

survey_other_envelopes() { # <the unit being restored>
  local unit=$1 need free
  # The survey reads envelopes the restore itself would not have read, so it
  # must not be what fills a small work directory: the largest one after the
  # envelope in use is read whole and, if a key opens it, decrypted. Without
  # room for that the survey is skipped, whatever --no-space-check says.
  need=$(awk -F'|' -v after="$CHOSEN_POS" -v blk="$BLOCK" '
    ($2 == "tenant_envelope" || $2 == "operator_envelope") && $1 + 0 > after + 0 && $3 ~ /^[0-9]+$/ {
      if ($3 + 0 > max) max = $3 + 0
    }
    END { if (max > 0) printf "%.0f\n", 2 * max + blk }
  ' "$FILES_TXT")
  [ -n "$need" ] || return 0
  free=$(free_bytes "$WORK")
  if is_uint "$free" && ! awk -v have="$free" -v want="$need" 'BEGIN { exit (have + 0 >= want + 0) ? 0 : 1 }'; then
    info "Not looking at the other envelopes: the work directory has $(size_str "$free") free and the"
    info "  largest needs about $(size_str "$need") (set TMPDIR to a larger disk to see what else this key opens)."
    return 0
  fi
  LONE_UNIT=$unit
  : >"$WORK/other_units.txt"
  info "Looking at the other envelopes this key opens, to tell you at the end what else is on this tape..."
  walk_envelopes count_other_envelope "$CHOSEN_POS"
  OTHER_UNITS=$(U="$unit" awk '$0 != ENVIRON["U"] && !seen[$0]++' "$WORK/other_units.txt" | wc -l)
}

# Said just before RESTORE COMPLETE, and only when the survey found units the
# restore did not cover.
note_other_units() {
  if [ -z "$LONE_UNIT" ] || [ "$OTHER_UNITS" -eq 0 ]; then
    return 0
  fi
  echo ""
  info "NOTE: only '$LONE_UNIT' was restored: no --unit or --all said what to restore, so the"
  info "      first envelope this key opened decided. This key opened $OTHER_ENVELOPES other envelope(s)"
  info "      on this tape, which list $OTHER_UNITS other unit(s) besides it."
  info "      See what each lists:   $0 --list $(keys_args)"
  info "      Restore every unit of the envelope that lists the most ($OTHER_BIGGEST units here), each into"
  info "      --to/UNIT (use another, empty directory):"
  info "        $0 --restore $(keys_args) --all --to /another/destination"
}

# Pick the slices of one unit (one version) from the manifest into $3, checked
# against the file map before any slice is read. Sets PICKED_VERSION and
# PICKED_BYTES. On a short tape a version with a slice after ENDED_AFTER is
# cut (volume-format-v2.md 4.2): PICKED_CUT is then 1, PICKED_LAST is its last
# slice's tape file, $3 is left empty, and the caller names it (R4).
PICKED_VERSION=""
PICKED_BYTES=0
PICKED_CUT=0
PICKED_LAST=""
pick_slices() { # <manifest> <unit> <out> [version]
  local manifest=$1 unit=$2 out=$3 want=${4:-} num tpos meb msha isize ihash
  # A volume can carry the SAME unit more than once — two snapshot versions,
  # two stage sets. Collecting every matching [[units]] block concatenated
  # both versions' slices and handed the mix to dar (issue #131). So: buffer
  # per block and emit exactly one version's slices.
  awk -v unit="$unit" -v want="$want" '
    function flush() {
      if (in_s && num != "") {
        n[blk]++
        slice[blk, n[blk]] = num "|" tpos "|" eb "|" sha
      }
      in_s = 0; num = ""; tpos = ""; eb = ""; sha = ""
    }
    # in_head tracks POSITION (are we in the key/value region directly under
    # [[units]]?); in_u tracks IDENTITY (is this block the unit we want?).
    # Separate on purpose: name and snapshot_version belong to the block head,
    # so honouring them anywhere else — a name key added to [[units.slices]],
    # or to some future table — would silently retarget the selector and drop
    # the remaining slices. A wrong answer with no error is the worst failure
    # this script can produce (#135).
    #
    # No apostrophes in these comments: the whole program is single-quoted in
    # the shell, so one would end it mid-awk and break the generated script.
    /^\[\[units\]\]/ { flush(); blk++; hit[blk] = 0; ver[blk] = -1; in_u = 0; in_head = 1; next }
    /^\[/ { in_head = 0 }
    in_head && /^name = / {
      gsub(/"/, "", $3)
      if ($3 == unit) { in_u = 1; hit[blk] = 1 } else { in_u = 0 }
    }
    in_u && in_head && /^snapshot_version = / { ver[blk] = $3 + 0 }
    in_u && /^\[\[units\.slices\]\]/ { flush(); in_s = 1; next }
    in_s && /^number = /           { num = $3 }
    in_s && /^tape_position = /    { tpos = $3 }
    in_s && /^encrypted_bytes = /  { eb = $3 }
    in_s && /^sha256_encrypted = / { gsub(/"/, "", $3); sha = $3 }
    END {
      flush()
      best = -1; pick = 0
      for (b = 1; b <= blk; b++) {
        if (!hit[b]) continue
        if (want != "") { if (ver[b] == want + 0) { pick = b; best = ver[b] } }
        else if (ver[b] >= best) { best = ver[b]; pick = b }
      }
      if (pick) {
        print best > "/dev/stderr"
        for (i = 1; i <= n[pick]; i++) print slice[pick, i]
      }
    }
  ' "$manifest" >"$WORK/picked.txt" 2>"$WORK/picked_version.txt"
  PICKED_VERSION=$(tr -d ' \n' <"$WORK/picked_version.txt" 2>/dev/null)
  if [ ! -s "$WORK/picked.txt" ]; then
    if [ -n "$want" ]; then
      die "unit '$unit' has no version $want on this volume
       --find-envelope --key KEYFILE lists the versions here; --info is
       keyless and cannot see them. Omit --version for the newest."
    fi
    die "no slices found for unit '$unit'"
  fi
  : >"$out"
  PICKED_BYTES=0
  PICKED_CUT=0
  PICKED_LAST=""
  if tape_is_short; then
    PICKED_LAST=$(awk -F'|' -v k="$ENDED_AFTER" '
      $2 + 0 > k + 0 { cut = 1 }
      $2 + 0 > last + 0 { last = $2 + 0 }
      END { if (cut) print last }
    ' "$WORK/picked.txt")
    if [ -n "$PICKED_LAST" ]; then
      PICKED_CUT=1
      return 0
    fi
  fi
  while IFS='|' read -r num tpos meb msha; do
    isize=$(file_size_at "$tpos" "$FILES_TXT")
    ihash=$(file_hash_at "$tpos" "$FILES_TXT")
    if [ -z "$isize" ] || [ -z "$ihash" ]; then
      die "slice $num of '$unit' (tape file $tpos) has no data_slice entry in the file map — refusing to trust the envelope manifest alone"
    fi
    require_uint "front_index_size(@$tpos)" "$isize"
    if [ -n "$meb" ] && [ "$meb" != "$isize" ]; then
      die "slice $num of '$unit' size mismatch: envelope manifest says $meb bytes, file map says $isize bytes — tape may be tampered or damaged"
    fi
    if [ -n "$msha" ] && [ "$msha" != "$ihash" ]; then
      die "slice $num of '$unit' hash mismatch: envelope manifest and file map disagree — tape may be tampered or damaged"
    fi
    echo "$num|$tpos|$isize|$ihash" >>"$out"
    PICKED_BYTES=$((PICKED_BYTES + isize))
  done <"$WORK/picked.txt"
}

# A unit cut by the early end of the tape: its slices run past the last file
# before the seal, so this tape does not hold it whole, and nothing of it is
# restored from here (R4), though the slices before the end are on the tape
# and hash right. Another tape holds it whole, and --where says which.
cut_text() { # <unit> <version> <its last slice's tape file>
  echo "unit '$1' (version ${2:-unknown}) is not on this tape whole: the tape ended early,
       after tape file $ENDED_AFTER (its seal marker, the last file, says so), and the
       slices of this version run to tape file $3. It was cut by the end of the
       tape and is restorable only from another tape that holds it whole. Find the
       tapes that do:
         $0 --where '$1' $(keys_args)"
}

die_cut() { # <unit> <version> <its last slice's tape file>
  die "$(cut_text "$@")
       No slice has been read."
}

# The units --all left out because they are cut, as unit|version|last file.
CUT_UNITS=()

# Said before RESTORE COMPLETE when --all left a unit out because it is cut.
note_cut_units() {
  [ ${#CUT_UNITS[@]} -gt 0 ] || return 0
  local line u v last
  echo ""
  info "NOT ON THIS TAPE WHOLE: the tape ended early, after file $ENDED_AFTER, before the last slice of"
  info "  these unit(s), so they were not restored from it. Each is restorable only from another tape:"
  for line in "${CUT_UNITS[@]}"; do
    IFS='|' read -r u v last <<<"$line"
    info "    '$u' (version $v), slices to tape file $last"
  done
  info "  The tapes that hold one: $0 --where UNIT $(keys_args)"
}

do_restore() {
  local destdir=$1 want_version=${2:-} scratch_parent=${3:-}

  [ -n "$scratch_parent" ] || scratch_parent=$destdir
  check_destination "$destdir" "$scratch_parent"
  dar_check
  mkdir -p "$destdir" "$scratch_parent"
  SCRATCH_PARENT=$scratch_parent
  RESTORE_TO=$destdir
  set_age_ids

  establish_files
  [ "$SALVAGE" = 0 ] || salvage_init

  # Step 1: the envelope (see pick_for_restore). Each key is tried on its own
  # at each position (#288).
  walk_envelopes pick_for_restore
  local wanted="${RESTORE_UNITS[*]-}"
  [ -n "$CHOSEN_ENV" ] || die_no_envelope "${wanted// /, }"
  info "Decrypted envelope at file $CHOSEN_POS"
  info "  opened with key $CHOSEN_KEY"
  local manifest="$CHOSEN_ENV/MANIFEST.toml"
  [ -f "$manifest" ] || die "envelope missing MANIFEST.toml"

  # Step 2: which units.
  local -a unit_names=() targets=()
  local uname
  while IFS= read -r uname; do
    unit_names+=("$uname")
  done < <(awk '
    /^\[\[units\]\]/ { in_u = 1; next }
    # !seen[] — a volume can carry the SAME unit in several snapshot versions,
    # one [[units]] block each. Listing it once per block made a single unit
    # look like several and pushed the plain --restore (no --unit) form into
    # the "multiple units" branch, printing the same name twice (#133).
    in_u && /^name = / { gsub(/"/, "", $3); if (!seen[$3]++) print $3; in_u = 0 }
    /^\[/              { in_u = 0 }
  ' "$manifest")
  [ ${#unit_names[@]} -gt 0 ] || die "no units in manifest"

  if [ "$RESTORE_ALL" = 1 ]; then
    targets=("${unit_names[@]}")
  elif [ ${#RESTORE_UNITS[@]} -gt 0 ]; then
    targets=("${RESTORE_UNITS[@]}")
  elif [ ${#unit_names[@]} -eq 1 ]; then
    targets=("${unit_names[0]}")
    survey_other_envelopes "${unit_names[0]}"
  else
    echo "Units in this envelope:"
    for uname in "${unit_names[@]}"; do echo "  - $uname"; done
    die "multiple units found — specify one with --unit NAME (repeat it for several), or --all"
  fi
  if [ -n "$want_version" ] && [ ${#targets[@]} -gt 1 ]; then
    die "--version applies to one unit; give a single --unit with it"
  fi
  if [ ${#RESTORE_PATHS[@]} -gt 0 ]; then
    [ ${#targets[@]} -eq 1 ] || die "--path needs exactly one unit: give one --unit with it"
    info "Parsing slices for unit: ${targets[0]}"
    pick_slices "$manifest" "${targets[0]}" "$WORK/slices.0" "$want_version"
    [ "$PICKED_CUT" = 0 ] || die_cut "${targets[0]}" "$PICKED_VERSION" "$PICKED_LAST"
    info "Restoring from '${targets[0]}' snapshot version ${PICKED_VERSION:-unknown}"
    restore_paths "$WORK/slices.0" "$CHOSEN_ENV" "${targets[0]}" "$PICKED_VERSION" "$destdir" "$scratch_parent"
    restore_end
    return 0
  fi

  # Step 3: each unit's slices, checked against the file map, before any is
  # read. Always say which version: a volume can hold several, and silently
  # picking one is how an heir restores the wrong data believing it current.
  local i total=0 first
  local -a order_lines=() versions=()
  for ((i = 0; i < ${#targets[@]}; i++)); do
    info "Parsing slices for unit: ${targets[$i]}"
    pick_slices "$manifest" "${targets[$i]}" "$WORK/slices.$i" "$want_version"
    if [ "$PICKED_CUT" = 1 ]; then
      # --all restores every unit the tape holds whole and names the cut
      # ones; a unit asked for by name is refused by name.
      [ "$RESTORE_ALL" = 1 ] || die_cut "${targets[$i]}" "$PICKED_VERSION" "$PICKED_LAST"
      info "  '${targets[$i]}' (version $PICKED_VERSION) is not on this tape whole: the tape ended before its last slice"
      CUT_UNITS+=("${targets[$i]}|$PICKED_VERSION|$PICKED_LAST")
      continue
    fi
    versions[$i]=$PICKED_VERSION
    info "Restoring '${targets[$i]}' snapshot version ${PICKED_VERSION:-unknown} ($(wc -l <"$WORK/slices.$i") slice(s))"
    total=$((total + PICKED_BYTES))
    first=$(head -n 1 "$WORK/slices.$i" | cut -d'|' -f2)
    order_lines+=("$first|$i")
  done

  if [ ${#order_lines[@]} -eq 0 ]; then
    note_cut_units
    die "no unit this envelope lists is on this tape whole: the tape ended early, after
       file $ENDED_AFTER, and every one is cut (see above). Nothing was restored."
  fi

  # Step 4: room for all of it, measured once, before any slice is read.
  check_space "$destdir" "$destdir" "$total" 0

  # Step 5: the units in tape order, one forward pass. One unit goes straight
  # into --to; several go to --to/UNIT each.
  local line dest_i
  while IFS= read -r line; do
    i=${line#*|}
    if [ ${#targets[@]} -eq 1 ]; then
      dest_i=$destdir
    else
      dest_i="$destdir/${targets[$i]}"
      mkdir -p "$dest_i"
    fi
    info "Extracting '${targets[$i]}' to $dest_i ..."
    restore_unit "$WORK/slices.$i" "$dest_i" "${targets[$i]}" "${versions[$i]}"
  done < <(printf '%s\n' "${order_lines[@]}" | sort -t'|' -k1,1n)

  finish_pending
  restore_end
}

# ---- the envelope's dar catalogues (--list, --path; #418) ----

# Every envelope carries, under catalogs/, the isolated dar catalogue of each
# unit it lists, named <first 8 of the unit uuid>_v<snapshot version>. With
# it dar lists a unit's files, and says which slice holds each, without
# reading the tape. Prints the catalogue's base path, or dies.
catalogue_path() { # <envelope dir> <unit> <version>: the base path, or nothing
  local env=$1 unit=$2 ver=$3 uuid
  uuid=$(awk -v unit="$unit" -v want="$ver" '
    function done_head() {
      if (hit && v == want + 0 && id != "") { print id; found = 1 }
      hit = 0; v = -1; id = ""
    }
    /^\[\[units\]\]/ { if (!found) done_head(); in_head = 1; next }
    /^\[/ { if (in_head && !found) done_head(); in_head = 0; next }
    in_head && /^name = / { x = $3; gsub(/"/, "", x); hit = (x == unit) }
    in_head && /^uuid = / { id = $3; gsub(/"/, "", id) }
    in_head && /^snapshot_version = / { v = $3 + 0 }
    END { if (in_head && !found) done_head() }
  ' "$env/MANIFEST.toml")
  [ -n "$uuid" ] || return 0
  [ -f "$env/catalogs/${uuid:0:8}_v$ver.1.dar" ] || return 0
  echo "$env/catalogs/${uuid:0:8}_v$ver"
}

catalogue_for() { # <envelope dir> <unit> <version>
  local env=$1 unit=$2 ver=$3 uuid base
  uuid=$(awk -v unit="$unit" -v want="$ver" '
    function done_head() {
      if (hit && v == want + 0 && id != "") { print id; found = 1 }
      hit = 0; v = -1; id = ""
    }
    /^\[\[units\]\]/ { if (!found) done_head(); in_head = 1; next }
    /^\[/ { if (in_head && !found) done_head(); in_head = 0; next }
    in_head && /^name = / { x = $3; gsub(/"/, "", x); hit = (x == unit) }
    in_head && /^uuid = / { id = $3; gsub(/"/, "", id) }
    in_head && /^snapshot_version = / { v = $3 + 0 }
    END { if (in_head && !found) done_head() }
  ' "$env/MANIFEST.toml")
  [ -n "$uuid" ] || die "the envelope lists no uuid for unit '$unit' version $ver"
  base="$env/catalogs/${uuid:0:8}_v$ver"
  [ -f "$base.1.dar" ] || die "the envelope carries no dar catalogue for '$unit' version $ver
       (expected catalogs/${uuid:0:8}_v$ver.1.dar). --restore without --path
       restores the whole unit, which needs no catalogue."
  echo "$base"
}

# The units --list and --path work on: those named, else the envelope's only
# one, else (for --list) every unit in it.
units_of_envelope() { # <manifest>
  awk '
    /^\[\[units\]\]/ { in_u = 1; next }
    # !seen[] — a volume can carry the SAME unit in several snapshot versions,
    # one [[units]] block each. Listing it once per block made a single unit
    # look like several and pushed the plain --restore (no --unit) form into
    # the "multiple units" branch, printing the same name twice (#133).
    in_u && /^name = / { gsub(/"/, "", $3); if (!seen[$3]++) print $3; in_u = 0 }
    /^\[/              { in_u = 0 }
  ' "$1"
}

# --list still lists a cut unit's files (its dar catalogue is in the envelope,
# and no slice is read), and says the tape does not hold it whole.
list_cut_note() { # <unit>
  echo "    NOT ON THIS TAPE WHOLE: the tape ended early, after file $ENDED_AFTER, and this version's"
  echo "    slices run to file $PICKED_LAST. Restore it from another tape that holds it:"
  echo "      $0 --where '$1' $(keys_args)"
}

do_list() {
  local want_version=${1:-} manifest u base
  set_age_ids
  dar_check
  establish_files
  # No unit named: the envelope listing the most units, as --all chooses
  # (with an operator or escrow key, the operator envelope: every unit).
  [ ${#RESTORE_UNITS[@]} -gt 0 ] || RESTORE_ALL=1
  walk_envelopes pick_for_restore
  local wanted="${RESTORE_UNITS[*]-}"
  [ -n "$CHOSEN_ENV" ] || die_no_envelope "${wanted// /, }"
  info "Decrypted envelope at file $CHOSEN_POS"
  info "  opened with key $CHOSEN_KEY"
  manifest="$CHOSEN_ENV/MANIFEST.toml"
  [ -f "$manifest" ] || die "envelope missing MANIFEST.toml"
  local -a targets=()
  if [ ${#RESTORE_UNITS[@]} -gt 0 ]; then
    targets=("${RESTORE_UNITS[@]}")
  else
    while IFS= read -r u; do targets+=("$u"); done < <(units_of_envelope "$manifest")
  fi
  for u in "${targets[@]}"; do
    pick_slices "$manifest" "$u" "$WORK/list.slices" "$want_version"
    base=$(catalogue_path "$CHOSEN_ENV" "$u" "$PICKED_VERSION")
    echo ""
    if [ -z "$base" ]; then
      echo "=== $u, snapshot version $PICKED_VERSION — no dar catalogue in this envelope ==="
      if [ "$PICKED_CUT" = 1 ]; then
        list_cut_note "$u"
      else
        echo "    (it can still be restored whole; --list and --path need the catalogue)"
      fi
      continue
    fi
    echo "=== $u, snapshot version $PICKED_VERSION — catalogs/${base##*/} ==="
    [ "$PICKED_CUT" = 0 ] || list_cut_note "$u"
    dar -l "$base" -N -Q || die "dar could not list the catalogue of '$u'"
  done
  echo ""
  echo "To restore one of these, give its path as listed:"
  echo "  $0 --restore $(keys_args) --unit UNIT --path PATH --to /your/destination"
}

# --restore --path: dar's catalogue says which slices hold each path and the
# directories above it (dar -l -T slice); those, and the last slice, which
# carries the archive's own catalogue, are read from the tape, checked,
# decrypted into scratch, and dar extracts just the paths from them.
RESTORE_PATHS=()
restore_paths() { # <slices file> <envelope dir> <unit> <version> <destination> <scratch parent>
  local list=$1 env=$2 unit=$3 ver=$4 destdir=$5 scr=$6 base p range lo hi last
  local -a g=()
  for p in "${RESTORE_PATHS[@]}"; do
    p=${p#/}
    p=${p#./}
    g+=(-g "$p")
  done
  base=$(catalogue_for "$env" "$unit" "$ver")
  dar -l "$base" -N -Q -Tslice "${g[@]}" >"$WORK/slicing.txt" 2>&1 ||
    die "dar could not read the catalogue of '$unit': $(tail -n 3 "$WORK/slicing.txt")"
  range=$(sed -n 's/.*slice range \[\([0-9-]*\)\].*/\1/p' "$WORK/slicing.txt" | head -n 1)
  if [ -z "$range" ]; then
    die "'$unit' version $ver holds nothing at: ${RESTORE_PATHS[*]}
       A path is relative to the unit's root, exactly as --list prints it:
         $0 --list $(keys_args) --unit '$unit'
       No slice has been read."
  fi
  lo=${range%-*}
  hi=${range#*-}
  require_uint slice_range "$lo"
  require_uint slice_range "$hi"
  last=$(tail -n 1 "$list" | cut -d'|' -f1)
  awk -F'|' -v lo="$lo" -v hi="$hi" -v last="$last" \
    '($1 + 0 >= lo + 0 && $1 + 0 <= hi + 0) || $1 == last' "$list" >"$WORK/path.slices"
  local total=0 n num tpos size sha
  n=$(wc -l <"$WORK/path.slices")
  while IFS='|' read -r num tpos size sha; do
    total=$((total + size))
  done <"$WORK/path.slices"
  info "'${RESTORE_PATHS[*]}' is in slice(s) $lo-$hi of $last; reading $n of them"
  check_space "$destdir" "$scr" "$total" "$total"
  UNIT_NAME=$unit
  UNIT_VER=$ver
  UNIT_OK=1
  UNIT_UUID=$(unit_uuid_of "$env" "$unit" "$ver")
  if [ "$SALVAGE" = 1 ]; then
    # The same slices, in a job of their own, with a bad one replaced.
    ensure_scratch
    local j=$JOB_N
    JOB_N=$((JOB_N + 1))
    JOB_KIND[$j]=unit
    JOB_DEST[$j]=$destdir
    JOB_UNIT[$j]=$unit
    JOB_VER[$j]=$ver
    JOB_UUID[$j]=$UNIT_UUID
    EXTRACT_G=("${g[@]}")
    mkdir -p "$SCRATCH/job.$j"
    spool_slices "$WORK/path.slices" "$SCRATCH/job.$j" "$WORK/job.$j.missing"
    if [ -s "$WORK/job.$j.missing" ] && [ "$SWAP_TAPES" = 1 ]; then
      PENDING+=("$j")
      finish_pending
    else
      job_run "$j"
    fi
    return 0
  fi
  SCRATCH="$(mktemp -d "$scr/.tapectl-restore.XXXXXX")" ||
    die "cannot create a scratch directory in $scr"
  spool_slices "$WORK/path.slices"
  local -a dar_opts=(-O -Q -N)
  local rc=0
  [ "$OVERWRITE" = 0 ] || dar_opts+=(-w)
  info "Extracting ${RESTORE_PATHS[*]} to $destdir ..."
  dar -x "$SCRATCH/restore" -R "$(abs_dir "$destdir")" "${g[@]}" "${dar_opts[@]}" >"$WORK/extract.log" 2>&1 </dev/null || rc=$?
  rm -rf "$SCRATCH"
  SCRATCH=""
  dar_result "$WORK/extract.log" "$rc" "$destdir"
  if [ "$UNIT_OK" = 1 ]; then
    DONE_LINES+=("${RESTORE_PATHS[*]} of '$unit' restored to: $destdir")
  fi
}

# ---- replacement slices and the damaged-file list (1.3.0; ADR-0012 items 101, 102) ----

# A slice that fails its ciphertext hash, or will not read, can be taken from
# another copy of the same unit. Copies are written from one staging, so the
# copy's slice has the same sha256_encrypted in its own front index, and that
# hash is the only thing a replacement is matched by: not its position, not its
# file name. --slices-from DIR offers slice files copied off another tape;
# --swap-tapes asks for another cartridge once this one has been read. Either
# one makes the restore keep each slice on disk in the scratch area, decrypted
# (as the older-dar fallback does), because a stream cannot take back a slice
# dar has already consumed.

# The absolute form of directory $1, with no trailing slash. dar prints the -R
# it was given word for word in its messages, so the damaged-file list is cut
# from an absolute root this script chose.
abs_dir() { # <directory>
  local d=$1
  case "$d" in
  /*) ;;
  *) d="$PWD/$d" ;;
  esac
  while [ "${#d}" -gt 1 ] && [ "${d%/}" != "$d" ]; do
    d=${d%/}
  done
  printf '%s' "$d"
}

# A text as sec 1.5 of the format escapes a path: a backslash is written as
# two, a TAB as \t, a newline as \n, a carriage return as \r, and nothing else
# changes. Character by character, so no escape is escaped twice.
escape_text() { # <text>
  local s=$1 out="" c
  while [ -n "$s" ]; do
    c=${s:0:1}
    s=${s:1}
    case "$c" in
    "\\") out="$out\\\\" ;;
    $'\t') out="$out\\t" ;;
    $'\n') out="$out\\n" ;;
    $'\r') out="$out\\r" ;;
    *) out="$out$c" ;;
    esac
  done
  printf '%s' "$out"
}

# The reverse, in one left-to-right scan (chained substitutions would turn the
# text of a backslash followed by t into a TAB). The result is in UNESC, not
# printed, because a path may end in a newline that $(...) would drop.
UNESC=""
unesc_line() { # <escaped text>
  local s=$1 out="" d
  while [[ $s == *\\* ]]; do
    out="$out${s%%\\*}"
    s=${s#*\\}
    d=${s:0:1}
    s=${s:1}
    case "$d" in
    t) out="$out"$'\t' ;;
    n) out="$out"$'\n' ;;
    r) out="$out"$'\r' ;;
    "\\") out="$out\\" ;;
    *) out="$out\\$d" ;;
    esac
  done
  UNESC="$out$s"
}

# The directories named by --slices-from must exist before any tape is read;
# asking for a replacement turns SALVAGE on.
check_slices_from() {
  local d
  for d in ${SLICES_FROM[@]+"${SLICES_FROM[@]}"}; do
    [ -d "$d" ] || die "--slices-from $d: not a directory. Nothing has been read from tape."
  done
  if [ ${#SLICES_FROM[@]} -gt 0 ] || [ "$SWAP_TAPES" = 1 ]; then
    SALVAGE=1
  fi
}

# What a failure of a slice says when no replacement was offered.
replace_hint() {
  cat <<'HINTEOF'
       Another copy of this unit holds the same slice, byte for byte: the front
       index of its tape lists the same sha256 for it. Run the same command
       again, with --overwrite if --to now holds part of the unit, and
         --slices-from DIR   a directory of slice files copied off another tape
                             (any names: a file is used only if its sha256 is the
                             one the front index here lists), or
         --swap-tapes        this script asks for another cartridge, once it has
                             read this one, and reads just the slices it needs.
       --help and File 1 say more.
HINTEOF
}

# The sha256 of the first $2 bytes of file $1, remembered for the run: the
# slices of one unit share a size, so every file in a directory is a candidate
# for each of them and is hashed once, not once per slice.
dir_sum() { # <file> <bytes>
  local key="$1|$2" sum
  sum=$(K="$key" awk -F'\t' '$1 == ENVIRON["K"] { print $2; exit }' "$WORK/dir.sums" 2>/dev/null || true)
  if [ -z "$sum" ]; then
    sum=$(head -c "$2" -- "$1" | sha256sum | awk '{print $1}')
    printf '%s\t%s\n' "$key" "$sum" >>"$WORK/dir.sums"
  fi
  printf '%s' "$sum"
}

# Is a slice with this sha256 in one of the --slices-from directories? A file
# is a candidate by its size alone (the slice's exact size, or that rounded up
# to the tape block as dd copies it off a tape) and is USED only if the sha256
# of its first <bytes> bytes is the one wanted. The file is left in DIR_HIT.
DIR_HIT=""
DIR_NOTED=""
dir_find() { # <slice number> <bytes> <sha256>
  local num=$1 size=$2 sha=$3 padded d f fsz
  DIR_HIT=""
  [ ${#SLICES_FROM[@]} -gt 0 ] || return 1
  padded=$(((size + BLOCK - 1) / BLOCK * BLOCK))
  for d in "${SLICES_FROM[@]}"; do
    for f in "$d"/* "$d"/.[!.]*; do
      [ -f "$f" ] || continue
      fsz=$(stat -c %s -- "$f" 2>/dev/null || true)
      [ "$fsz" = "$size" ] || [ "$fsz" = "$padded" ] || continue
      if [ "$(dir_sum "$f" "$size")" = "$sha" ]; then
        DIR_HIT=$f
        return 0
      fi
      case "$DIR_NOTED" in
      *"|$f|"*) ;;
      *)
        DIR_NOTED="$DIR_NOTED|$f|"
        echo "NOTE: $f has the size of slice $num but not its sha256: it is not used." >&2
        ;;
      esac
    done
  done
  return 1
}

# Read the slice at tape file $1 into $4, trimmed to its $2 bytes, and check it
# against the sha256 $3 the front index states. Never exits on a bad slice:
# returns 1 and says why in SLICE_WHY (a full disk still stops the run).
SLICE_WHY=""
read_slice_checked() { # <tape file> <bytes> <sha256> <out>
  local pos=$1 size=$2 sha=$3 out=$4 actual
  SLICE_WHY=""
  : >"$WORK/dd.err"
  if ! try_read_tape_raw "$pos" "$out"; then
    if grep -i 'no space left' "$WORK/dd.err" >/dev/null 2>&1; then
      die_io "reading tape file $pos failed" "$WORK/dd.err" "$(dirname "$out")"
    fi
    rm -f "$out"
    SLICE_WHY="would not read"
    return 1
  fi
  truncate -s "$size" "$out"
  actual=$(sha256sum "$out" | awk '{print $1}')
  if [ "$actual" != "$sha" ]; then
    rm -f "$out"
    SLICE_WHY="does not match the front index (sha256 ${actual:0:16}..., the index says ${sha:0:16}...)"
    return 1
  fi
}

# ---- the unit being restored, and what became of it ----

UNIT_NAME=""
UNIT_VER=""
UNIT_UUID=""
UNIT_OK=1
INCOMPLETE=()
DONE_LINES=()
RESTORE_TO=""
DAMAGED_NAME=.tapectl-damaged-files.txt

# The uuid of one version of a unit, from its envelope's manifest.
unit_uuid_of() { # <envelope dir> <unit> <version>
  awk -v unit="$2" -v want="$3" '
    function done_head() {
      if (hit && v == want + 0 && id != "") { print id; found = 1 }
      hit = 0; v = -1; id = ""
    }
    /^\[\[units\]\]/ { if (!found) done_head(); in_head = 1; next }
    /^\[/ { if (in_head && !found) done_head(); in_head = 0; next }
    in_head && /^name = / { x = $3; gsub(/"/, "", x); hit = (x == unit) }
    in_head && /^uuid = / { id = $3; gsub(/"/, "", id) }
    in_head && /^snapshot_version = / { v = $3 + 0 }
    END { if (in_head && !found) done_head() }
  ' "$1/MANIFEST.toml"
}

# The first line of a damaged-file list: its magic, its format (1), then the
# unit's name (escaped as a path is), its uuid and its version, TAB-separated.
# A fill run uses a tape only if it lists the same unit uuid AND version.
damaged_header() { # <unit> <uuid> <version>
  printf '%s\t%s\tunit=%s\tuuid=%s\tversion=%s\n' '#tapectl-damaged-files' 1 "$(escape_text "$1")" "$2" "$3"
}

# One unit's note for the end of a run that did not get everything: how many
# files, where the list is, and the command that fills them from another copy.
damage_note() { # <count> <list file> <slices not available, or empty>
  local n=$1 list=$2 gone=${3:-} to=${RESTORE_TO:-DIR}
  if [ "${CHECK_ADDED:-0}" -eq 0 ]; then
    echo "Unit '$UNIT_NAME' (version $UNIT_VER): $n file(s) were not restored."
  elif [ "$CHECK_ADDED" -eq "$n" ]; then
    echo "Unit '$UNIT_NAME' (version $UNIT_VER): $n file(s) do not match the sha256 the index records for them, or are missing."
    echo "  dar named none of them: it checks a file with a short checksum, and this script checks it with a hash."
  else
    echo "Unit '$UNIT_NAME' (version $UNIT_VER): $n file(s) were not restored intact."
    echo "  $CHECK_ADDED of them were written without a dar error but do not match the sha256 the index records"
    echo "  for them, or are missing: dar checks a file with a short checksum, and this script checks it with a hash."
  fi
  if [ -n "$gone" ]; then
    echo "  Slice(s) not available: $gone. The files in them are among those listed."
  fi
  echo "  They are listed, one path per line, in"
  echo "    $list"
  echo "  Another copy of this same unit and version holds them. Load that tape and run ITS"
  echo "  RESTORE.sh (File 2 of that tape), which restores only those files and overwrites only them:"
  echo "    ./RESTORE.sh --restore $(keys_args) --unit $(printf '%q' "$UNIT_NAME") --fill-from $(printf '%q' "$list") --to $(printf '%q' "$to")"
  echo "  A tape that does not hold this unit at this version is refused: no other version is used."
}

# Write the list of files dar could not restore into the unit's directory and
# note the unit as incomplete.
record_damage() { # <destination> <file of escaped paths> <slices not available, or empty>
  local destdir=$1 paths=$2 gone=${3:-} n list
  list="$destdir/$DAMAGED_NAME"
  n=$(wc -l <"$paths")
  {
    damaged_header "$UNIT_NAME" "$UNIT_UUID" "$UNIT_VER"
    cat "$paths"
  } >"$list"
  UNIT_OK=0
  INCOMPLETE+=("$(damage_note "$n" "$list" "$gone")")
}

# A list left by an earlier run of this very unit and version is out of date
# once the unit restores whole: drop it, so a clean directory has no list.
clear_stale_list() { # <destination>
  local list="$1/$DAMAGED_NAME" first=""
  [ -f "$list" ] || return 0
  IFS= read -r first <"$list" || true
  case "$first" in
  *$'\t'"uuid=$UNIT_UUID"$'\t'"version=$UNIT_VER")
    rm -f "$list"
    info "Removed $list: this restore of '$UNIT_NAME' left nothing damaged"
    ;;
  esac
}

# ---- the restored files against their recorded sha256 (1.3.0; ADR-0012 item 103) ----

# dar checks each file it restores with a short checksum, not a hash: two equal
# byte flips can cancel, and a damaged file comes back with no error. A tape
# written since 1.3.0 records the sha256 of every regular file in its tenant
# envelope's index/paths.tsv, and once a unit is extracted each file is read
# back from --to and checked against it with sha256sum. The envelope is on disk
# already and the files are the ones just restored: the tape is not touched. A
# file that does not match, or a listed file that is not there, goes on the
# damaged-file list, so --fill-from can take it from another copy. A file with
# no recorded hash is not checked, and a tape with no index (written before
# 1.3.0) is not checked at all; both are said. --no-file-check skips it.
FILE_CHECK=1
CHECK_NOTED=" "
CHECK_INDEX=""
CHECK_ANY_INDEX=0
CHECK_BAD="$WORK/check.bad"
CHECK_FAILED=0
CHECK_UNNAMED=0
CHECK_ADDED=0

# True when $1 was said before in this run; else remembers it and is false.
said_before() { # <key>
  case "$CHECK_NOTED" in
  *" $1 "*) return 0 ;;
  esac
  CHECK_NOTED="$CHECK_NOTED$1 "
  return 1
}

# A path as the lists print it: one line, with no control character a
# terminal would act on (the lists already hold a TAB, newline or carriage
# return as \t, \n and \r).
printable_path() { # <escaped path>
  printf '%s' "$1" | LC_ALL=C tr -d '\000-\010\013\014\016-\037\177'
}

# The paths.tsv, among the envelopes this run has open, that lists this
# version of this unit: the envelope in use first, then the others the walk
# opened (an escrow or operator key restores from the operator envelope, which
# has no paths.tsv; the tenant envelope that lists the unit was opened before
# it and is still on disk). Sets CHECK_INDEX ("" when there is none) and
# CHECK_ANY_INDEX (1 when any open envelope carries an index member at all).
index_for_unit() { # <unit> <version> <unit uuid>
  local d
  CHECK_INDEX=""
  CHECK_ANY_INDEX=0
  for d in "$CHOSEN_ENV" "$WORK"/opened.*; do
    [ -d "$d" ] || continue
    if [ -f "$d/index/units.tsv" ] || [ -f "$d/index/paths.tsv" ]; then
      CHECK_ANY_INDEX=1
    fi
    [ -z "$CHECK_INDEX" ] || continue
    [ -f "$d/index/paths.tsv" ] && [ -f "$d/index/units.tsv" ] || continue
    if CHK_UNIT="$1" CHK_VER="$2" CHK_UUID="$3" awk '
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
    function cut_here(row, names,    f, line) {
      if (!cut_read) {
        cut_read = 1
        f = ENVIRON["INDEX_CUT"]
        if (f != "") {
          while ((getline line < f) > 0) cutv[line] = 1
          close(f)
        }
      }
      if (val(row, names, "state") != "this-tape") return 0
      return ((val(row, names, "unit_uuid") "\t" val(row, names, "version")) in cutv)
    }
    function holder(row, names,    state, sealed, first, last, count, st, s) {
      state = show(val(row, names, "state"))
      sealed = show(val(row, names, "sealed_at"))
      first = show(val(row, names, "first_slice"))
      last = show(val(row, names, "last_slice"))
      count = show(val(row, names, "num_slices"))
      if (state == "this-tape" && cut_here(row, names)) st = "CUT on this tape: it ended early, before the last slice of this version, so it does not hold it whole"
      else if (state == "this-tape") st = "this tape"
      else if (state == "sealed") st = (sealed == "") ? "sealed" : "sealed " sealed
      else if (state == "quarantined") {
        st = "QUARANTINED: a verify proved this medium bad, it may still read in part"
        if (sealed != "") st = st "; sealed " sealed
      }
      else st = (state == "") ? "state not known" : "state " state
      s = "    tape " nk(show(val(row, names, "volume"))) "  (" st ")"
      if (first != "" && last != "") s = s "  slices " first "-" last (count == "" ? "" : " of " count)
      s = s "\n        volume uuid " nk(show(val(row, names, "volume_uuid")))
      s = s "; cartridge serial " nk(show(val(row, names, "cartridge_serial")))
      s = s "; generation " nk(show(val(row, names, "generation")))
      return s
    }
   
    BEGIN {
      FS = "\t"
      unit = ENVIRON["CHK_UNIT"]
      ver = ENVIRON["CHK_VER"] + 0
      uuid = ENVIRON["CHK_UUID"]
    }
    NR == 1 {
      header($0, h)
      if (!("unit" in h) || !("version" in h)) { bad = 1; exit 2 }
      next
    }
    {
      split($0, f, "\t")
      if (unesc(val(f, h, "unit")) != unit) next
      if (val(f, h, "version") + 0 != ver) next
      u = val(f, h, "unit_uuid")
      if (uuid != "" && u != "" && u != uuid) next
      found = 1
      exit 0
    }
    END {
      if (bad) exit 2
      exit(found ? 0 : 1)
    }
  ' "$d/index/units.tsv"; then
      CHECK_INDEX="$d/index/paths.tsv"
    fi
  done
}

# Check the regular files of UNIT_NAME, version UNIT_VER, that were restored
# into $1, against the index. With $2, a file of escaped paths, only those
# (a fill). With --path, only what was asked for. The files that fail are left
# in $CHECK_BAD, one per line and escaped as the index escapes a path; their
# number is CHECK_FAILED, and CHECK_UNNAMED counts failures that sha256sum
# reported and this script could not name.
file_check() { # <destination> [<file of escaped paths to check>]
  local destdir=$1 only=${2:-} p whole=0
  local rows=0 files=0 checked=0 nspecial=0 nohash=0 named=0 unmatched=0 stray=0
  : >"$CHECK_BAD"
  CHECK_FAILED=0
  CHECK_UNNAMED=0
  if [ "$FILE_CHECK" = 0 ]; then
    said_before off || {
      info "File check skipped (--no-file-check): the restored files are checked by dar's own checksum only,"
      info "  not against the sha256 recorded for them in the index."
    }
    return 0
  fi
  if [ -n "$only" ] && [ ! -s "$only" ]; then
    return 0
  fi
  index_for_unit "$UNIT_NAME" "$UNIT_VER" "$UNIT_UUID"
  if [ -z "$CHECK_INDEX" ]; then
    if [ "$CHECK_ANY_INDEX" = 0 ]; then
      said_before no-index || {
        info "File check: this tape carries no index (a tape written before version 1.3.0 has none), so the"
        info "  restored files are checked by dar's own checksum only, not against a recorded sha256."
      }
    else
      info "File check: no envelope this key opened lists '$(visible_text "$UNIT_NAME")' version $UNIT_VER with its files, so they are"
      info "  not checked against a recorded sha256 (the key of the tenant who owns the unit opens the envelope that does)."
    fi
    return 0
  fi
  # --path: dar restored these paths and, for a directory, what is under it.
  if [ -z "$only" ] && [ ${#RESTORE_PATHS[@]} -gt 0 ]; then
    : >"$WORK/check.only"
    for p in "${RESTORE_PATHS[@]}"; do
      p=${p#/}
      p=${p#./}
      while [ "${p%/}" != "$p" ]; do
        p=${p%/}
      done
      if [ -z "$p" ]; then
        whole=1
        break
      fi
      printf '%s\n' "$(escape_text "$p")" >>"$WORK/check.only"
    done
    [ "$whole" = 1 ] || only="$WORK/check.only"
  fi
  if ! CHK_UNIT="$UNIT_NAME" CHK_VER="$UNIT_VER" CHK_ONLY="$only" CHK_SUMS="$WORK/check.sums" \
    CHK_SPECIAL="$WORK/check.special" CHK_COUNTS="$WORK/check.counts" \
    awk '
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
    function cut_here(row, names,    f, line) {
      if (!cut_read) {
        cut_read = 1
        f = ENVIRON["INDEX_CUT"]
        if (f != "") {
          while ((getline line < f) > 0) cutv[line] = 1
          close(f)
        }
      }
      if (val(row, names, "state") != "this-tape") return 0
      return ((val(row, names, "unit_uuid") "\t" val(row, names, "version")) in cutv)
    }
    function holder(row, names,    state, sealed, first, last, count, st, s) {
      state = show(val(row, names, "state"))
      sealed = show(val(row, names, "sealed_at"))
      first = show(val(row, names, "first_slice"))
      last = show(val(row, names, "last_slice"))
      count = show(val(row, names, "num_slices"))
      if (state == "this-tape" && cut_here(row, names)) st = "CUT on this tape: it ended early, before the last slice of this version, so it does not hold it whole"
      else if (state == "this-tape") st = "this tape"
      else if (state == "sealed") st = (sealed == "") ? "sealed" : "sealed " sealed
      else if (state == "quarantined") {
        st = "QUARANTINED: a verify proved this medium bad, it may still read in part"
        if (sealed != "") st = st "; sealed " sealed
      }
      else st = (state == "") ? "state not known" : "state " state
      s = "    tape " nk(show(val(row, names, "volume"))) "  (" st ")"
      if (first != "" && last != "") s = s "  slices " first "-" last (count == "" ? "" : " of " count)
      s = s "\n        volume uuid " nk(show(val(row, names, "volume_uuid")))
      s = s "; cartridge serial " nk(show(val(row, names, "cartridge_serial")))
      s = s "; generation " nk(show(val(row, names, "generation")))
      return s
    }
   
    function selected(p,    rest, off, i) {
      if (p in want) return 1
      rest = p
      off = 0
      while ((i = index(rest, "/")) > 0) {
        off += i
        if (substr(p, 1, off - 1) in want) return 1
        rest = substr(rest, i + 1)
      }
      return 0
    }
    function leaves(p) {
      return (p ~ /^\// || p == ".." || p ~ /^\.\.\// || p ~ /\/\.\.\// || p ~ /\/\.\.$/)
    }
    BEGIN {
      FS = "\t"
      unit = ENVIRON["CHK_UNIT"]
      ver = ENVIRON["CHK_VER"] + 0
      sums = ENVIRON["CHK_SUMS"]
      special = ENVIRON["CHK_SPECIAL"]
      counts = ENVIRON["CHK_COUNTS"]
      only = ENVIRON["CHK_ONLY"]
      if (only != "") {
        while ((getline line < only) > 0) { want[unesc(line)] = 1; nonly++ }
        close(only)
      }
      printf "" > sums
      printf "" > special
    }
    NR == 1 {
      header($0, h)
      if (!("unit" in h) || !("version" in h) || !("path" in h)) { bad = 1; exit 2 }
      next
    }
    {
      split($0, f, "\t")
      if (unesc(val(f, h, "unit")) != unit) next
      if (val(f, h, "version") + 0 != ver) next
      rows++
      if (val(f, h, "kind") != "f") next
      raw = val(f, h, "path")
      p = unesc(raw)
      if (nonly > 0 && !selected(p)) next
      files++
      hash = val(f, h, "sha256")
      if (length(hash) != 64 || hash !~ /^[0-9a-f]+$/ || leaves(p)) { nohash++; next }
      checked++
      if (index(p, "\\") > 0 || index(p, "\n") > 0 || index(p, "\r") > 0) {
        nspecial++
        print hash "\t" raw > special
      } else {
        print hash "  " p > sums
      }
    }
    END {
      if (bad) exit 2
      print rows + 0, files + 0, checked + 0, nspecial + 0, nohash + 0 > counts
      close(counts)
      close(sums)
      close(special)
    }
  ' "$CHECK_INDEX"; then
    echo "WARNING: this script could not make the list of files to check from the index in the envelope (it has no" >&2
    echo "         unit, version or path column, or the work directory $WORK is full; TMPDIR moves it):" >&2
    echo "         the restored files of '$(visible_text "$UNIT_NAME")' are not checked against a recorded sha256." >&2
    return 0
  fi
  read -r rows files checked nspecial nohash <"$WORK/check.counts" || true
  if [ "$checked" -eq 0 ]; then
    if [ "$nohash" -gt 0 ]; then
      info "File check: the index records no sha256 for the $nohash file(s) of '$(visible_text "$UNIT_NAME")' version $UNIT_VER: not checked."
    fi
    return 0
  fi
  # Names that sha256sum -c takes as they stand: one pass, quiet, so only a
  # failure is printed (a mismatch, or a file it could not open).
  if [ "$checked" -gt "$nspecial" ]; then
    (cd "$destdir" && sha256sum -c --quiet) <"$WORK/check.sums" >"$WORK/check.out" 2>"$WORK/check.err" || true
    CHK_REPORT="$WORK/check.out" CHK_BAD="$WORK/check.named" CHK_COUNTS="$WORK/check.named.counts" \
      awk '
    function esc(s,    out, i, n, c) {
      out = ""
      n = length(s)
      for (i = 1; i <= n; i++) {
        c = substr(s, i, 1)
        if (c == "\\") out = out "\\\\"
        else if (c == "\t") out = out "\\t"
        else if (c == "\n") out = out "\\n"
        else if (c == "\r") out = out "\\r"
        else out = out c
      }
      return out
    }
    function ends(s, t) {
      return length(s) > length(t) && substr(s, length(s) - length(t) + 1) == t
    }
    BEGIN {
      report = ENVIRON["CHK_REPORT"]
      bad = ENVIRON["CHK_BAD"]
      counts = ENVIRON["CHK_COUNTS"]
      open_fail = ": FAILED open or read"
      fail = ": FAILED"
      while ((getline line < report) > 0) {
        if (ends(line, open_fail)) name = substr(line, 1, length(line) - length(open_fail))
        else if (ends(line, fail)) name = substr(line, 1, length(line) - length(fail))
        else { stray++; continue }
        if (!(name in failed)) { failed[name] = 1; nfail++ }
      }
      close(report)
      printf "" > bad
    }
    {
      name = substr($0, 67)
      if (name in failed) { print esc(name) > bad; named++; delete failed[name] }
    }
    END {
      print named + 0, nfail - named, stray + 0 > counts
      close(counts)
      close(bad)
    }
  ' "$WORK/check.sums"
    read -r named unmatched stray <"$WORK/check.named.counts" || true
    cat "$WORK/check.named" >>"$CHECK_BAD"
    if [ "$unmatched" -gt 0 ] || [ "$stray" -gt 0 ]; then
      CHECK_UNNAMED=$((unmatched + stray))
      echo "WARNING: sha256sum's report could not be matched to the files for $CHECK_UNNAMED line(s):" >&2
      head -n 5 "$WORK/check.out" "$WORK/check.err" 2>/dev/null | sed 's/^/         /' >&2 || true
    fi
  fi
  # A name with a backslash, a newline or a carriage return, one file at a
  # time (sha256sum prints and reads such a name in an escaped form that
  # differs between versions; hashing the file from standard input has no name
  # to escape).
  if [ "$nspecial" -gt 0 ]; then
    local want esc got
    while IFS=$'\t' read -r want esc; do
      unesc_line "$esc"
      got=$({ sha256sum <"$destdir/$UNESC"; } 2>/dev/null) || got=""
      got=${got%% *}
      [ "$got" = "$want" ] || printf '%s\n' "$esc" >>"$CHECK_BAD"
    done <"$WORK/check.special"
  fi
  CHECK_FAILED=$(wc -l <"$CHECK_BAD")
  local what="'$(visible_text "$UNIT_NAME")' version $UNIT_VER"
  if [ "$CHECK_FAILED" -eq 0 ] && [ "$CHECK_UNNAMED" -eq 0 ]; then
    if [ "$nohash" -gt 0 ]; then
      info "File check: $checked restored file(s) of $what match the sha256 recorded in the index ($nohash more not checked: no sha256 recorded)"
    else
      info "File check: $checked restored file(s) of $what match the sha256 recorded in the index"
    fi
    return 0
  fi
  info "File check: $CHECK_FAILED of $checked file(s) of $what do NOT match their recorded sha256, or are missing:"
  head -n 10 "$CHECK_BAD" | while IFS= read -r p; do
    info "    $(printable_path "$p")"
  done
  if [ "$CHECK_FAILED" -gt 10 ]; then
    info "    ... and $((CHECK_FAILED - 10)) more (all are in the damaged-file list)"
  fi
}

# What a finished dar extraction (log $1, exit status $2, into $3) means.
# Complete: status 0 and nothing skipped. Damage: status 5, which dar uses when
# a file could not be restored, and it named the files (a failed CRC, a slice
# cut short): they go in the unit's list and the run goes on to the next unit,
# ending INCOMPLETE. $4 is the slices that were not available: with a slice
# missing, a file dar skipped (user choice) is a file that was in it, not one
# that was already there. Anything else is fatal, as it has always been.
# Either way the restored files are then checked against the index
# (file_check): the files dar named ($WORK/damaged.new, possibly none) and the
# ones the check finds go in one list.
dar_result() { # <log> <status> <destination> [slices not available]
  local log=$1 rc=$2 destdir=$3 gone=${4:-} errs=0 skipped=0 unmapped=0 failed=0 skips=0 n
  UNIT_OK=1
  cat "$log"
  if [ "$rc" != 0 ] && grep -i 'no space left' "$log" >/dev/null 2>&1; then
    die_io "dar extraction failed" "$log" "$destdir"
  fi
  [ -z "$gone" ] || skips=1
  : >"$WORK/damaged.new"
  if [ "$rc" = 0 ] && [ "$skips" = 0 ]; then
    die_if_dar_skipped "$log" "$destdir"
    finish_damage "$destdir" ""
    return 0
  fi
  if [ "$rc" != 0 ] && [ "$rc" != 5 ]; then
    die "dar extraction failed (exit $rc) — dar's own message is above"
  fi
  # Status 5 is also what dar says when the disk, not the archive, refused a
  # file. Those are not damaged files: a fill from another tape would meet the
  # same refusal. They stop the run, as ever.
  if [ "$rc" = 5 ] && grep -E ' : (Permission denied|Operation not permitted|Read-only file system|No such file or directory|Not a directory|Is a directory|File name too long|Too many open files|Too many levels of symbolic links|Disk quota exceeded|File exists|Input/output error|Cannot allocate memory)$' "$log" >/dev/null 2>&1; then
    die "dar extraction failed (exit $rc): the file system refused a file — dar's own message is above.
       This is not a damaged archive, so no list of damaged files is written."
  fi
  DAMAGED_ROOT="$(abs_dir "$destdir")" DAMAGED_SKIPS="$skips" DAMAGED_COUNTS="$WORK/damaged.counts" \
    awk '
    function esc(s,    out, i, n, c) {
      out = ""
      n = length(s)
      for (i = 1; i <= n; i++) {
        c = substr(s, i, 1)
        if (c == "\\") out = out "\\\\"
        else if (c == "\t") out = out "\\t"
        else if (c == "\n") out = out "\\n"
        else if (c == "\r") out = out "\\r"
        else out = out c
      }
      return out
    }
    function lastsep(s,    p, q) {
      p = 0
      while ((q = index(substr(s, p + 1), " : ")) > 0) p = p + q
      return p
    }
    function emit(path,    e) {
      if (index(path, root) != 1 || length(path) <= rl) { unmapped++; return }
      e = esc(substr(path, rl + 1))
      if (!(e in seen)) { seen[e] = 1; print e }
    }
    BEGIN {
      root = ENVIRON["DAMAGED_ROOT"] "/"
      rl = length(root)
      skips = (ENVIRON["DAMAGED_SKIPS"] == "1")
      counts = ENVIRON["DAMAGED_COUNTS"]
      tail = " not restored (user choice)"
      tl = length(tail)
    }
    {
      if (acc != "" && (index($0, "Error while restoring ") == 1 || $0 ~ /^ -----/ || (kind == "S" && index($0, root) == 1))) {
        unmapped++; acc = ""; kind = ""
      }
      if (acc == "") {
        if (index($0, "Error while restoring ") == 1) { acc = substr($0, 23); kind = "E" }
        else if (skips && index($0, root) == 1) { acc = $0; kind = "S" }
        else {
          if ($0 ~ /^ [0-9]+ inode\(s\) failed to restore/) failed = $1 + 0
          next
        }
      } else {
        acc = acc "\n" $0
      }
      if (kind == "E") {
        p = lastsep(acc)
        if (p > 0) { emit(substr(acc, 1, p - 1)); errs++; acc = ""; kind = "" }
      } else {
        n = length(acc)
        if (n >= tl && substr(acc, n - tl + 1) == tail) { emit(substr(acc, 1, n - tl)); skipped++; acc = ""; kind = "" }
      }
    }
    END {
      if (acc != "") unmapped++
      print errs + 0, skipped + 0, unmapped + 0, failed + 0 > counts
      close(counts)
    }
  ' "$log" >"$WORK/damaged.new"
  read -r errs skipped unmapped failed <"$WORK/damaged.counts" || true
  n=$(wc -l <"$WORK/damaged.new")
  if [ "$n" -eq 0 ]; then
    if [ "$rc" != 0 ]; then
      die "dar extraction failed (exit $rc) — dar's own message is above"
    fi
    echo "NOTE: slice(s) $gone were not available, yet dar names no file it could not restore." >&2
    finish_damage "$destdir" ""
    return 0
  fi
  if [ "$unmapped" -gt 0 ] || { [ "$rc" = 5 ] && [ "$errs" -ne "$failed" ]; }; then
    echo "WARNING: dar reported $failed file(s) failed and $skipped skipped, and this script could name" >&2
    echo "         $((errs + skipped - unmapped)) of them; the list written below may be short. Read dar's messages above." >&2
  fi
  finish_damage "$destdir" "$gone"
}

# One unit's list, from the files dar named and the files the check finds: the
# list is written when it holds any file, and a list left by an earlier run is
# removed when it does not.
finish_damage() { # <destination> <slices not available, or empty>
  local destdir=$1 gone=${2:-} dar_n all_n
  file_check "$destdir"
  dar_n=$(wc -l <"$WORK/damaged.new")
  awk '!seen[$0]++' "$WORK/damaged.new" "$CHECK_BAD" >"$WORK/damaged.all"
  all_n=$(wc -l <"$WORK/damaged.all")
  CHECK_ADDED=$((all_n - dar_n))
  if [ "$all_n" -gt 0 ]; then
    record_damage "$destdir" "$WORK/damaged.all" "$gone"
  elif [ "$CHECK_UNNAMED" -gt 0 ]; then
    UNIT_OK=0
    INCOMPLETE+=("Unit '$UNIT_NAME' (version $UNIT_VER): $CHECK_UNNAMED file(s) failed the sha256 check and this script could not name them (see the WARNING above); no list is written.")
  else
    clear_stale_list "$destdir"
  fi
}

# ---- salvage mode: a unit's slices on disk, bad ones replaced ----

JOB_N=0
JOB_KIND=()
JOB_DEST=()
JOB_UNIT=()
JOB_VER=()
JOB_UUID=()
PENDING=()
EXTRACT_G=()
TAPE1_UUID=""
VERIFY_ONLY=0

ensure_scratch() {
  if [ -z "$SCRATCH" ]; then
    SCRATCH="$(mktemp -d "$SCRATCH_PARENT/.tapectl-restore.XXXXXX")" ||
      die "cannot create a scratch directory in $SCRATCH_PARENT"
  fi
}

# Called once the first tape's File 0 has been read: remember which cartridge
# it was, so a second tape that is the same one can be told apart.
salvage_init() {
  TAPE1_UUID=$(toml_val "$WORK/thunk.toml" uuid)
}

# The numbers of the slices job $1 still lacks, for a message.
missing_numbers() { # <job>
  awk -F'|' '{ printf "%s%s", sep, $1; sep = ", " }' "$WORK/job.$1.missing"
}

# A unit, in salvage mode. Its slices come off this tape one at a time into a
# directory of its own in the scratch area; a slice that fails is looked for in
# the --slices-from directories. With --swap-tapes a unit still short of slices
# waits for the second cartridge while the next units are read; without it dar
# is given what there is.
salvage_unit() { # <slices file> <destination> <unit> <version>
  local list=$1 destdir=$2 unit=$3 ver=$4 j bytes=0 num tpos size sha
  while IFS='|' read -r num tpos size sha; do
    bytes=$((bytes + size))
  done <"$list"
  check_space "$destdir" "$SCRATCH_PARENT" "$bytes" "$((bytes + BLOCK))"
  ensure_scratch
  j=$JOB_N
  JOB_N=$((JOB_N + 1))
  JOB_KIND[$j]=unit
  JOB_DEST[$j]=$destdir
  JOB_UNIT[$j]=$unit
  JOB_VER[$j]=$ver
  JOB_UUID[$j]=$UNIT_UUID
  mkdir -p "$SCRATCH/job.$j"
  spool_slices "$list" "$SCRATCH/job.$j" "$WORK/job.$j.missing"
  if [ -s "$WORK/job.$j.missing" ] && [ "$SWAP_TAPES" = 1 ]; then
    PENDING+=("$j")
    info "'$unit' is still short of slice(s) $(missing_numbers "$j"); it waits for the second cartridge"
    return 0
  fi
  job_run "$j"
}

job_run() { # <job>
  case "${JOB_KIND[$1]}" in
  fill) fill_extract "$1" ;;
  *) job_extract "$1" ;;
  esac
}

# dar reads a unit's decrypted slices from its directory in the scratch area.
job_extract() { # <job>
  local j=$1 destdir unit gone="" rc=0
  destdir=${JOB_DEST[$j]}
  unit=${JOB_UNIT[$j]}
  UNIT_NAME=$unit UNIT_VER=${JOB_VER[$j]} UNIT_UUID=${JOB_UUID[$j]}
  if [ -s "$WORK/job.$j.missing" ]; then
    gone=$(missing_numbers "$j")
    print_needs "$j"
    echo "NOTE: restoring '$unit' without slice(s) $gone: the files in them will be listed, not restored." >&2
  fi
  local -a dar_opts=(-O -Q -N)
  [ "$OVERWRITE" = 0 ] || dar_opts+=(-w)
  info "The slices are on disk; dar extracts '$unit' to $destdir ..."
  dar -x "$SCRATCH/job.$j/restore" -R "$(abs_dir "$destdir")" "${dar_opts[@]}" ${EXTRACT_G[@]+"${EXTRACT_G[@]}"} >"$WORK/extract.log" 2>&1 </dev/null || rc=$?
  rm -rf "$SCRATCH/job.$j"
  if [ -n "$gone" ] && [ "$rc" != 0 ] && [ "$rc" != 5 ]; then
    cat "$WORK/extract.log" >&2
    die "dar could not restore '$unit' without slice(s) $gone (exit $rc). The first and the last slice of a
       unit cannot be left out: they hold the archive's header and its catalogue."
  fi
  dar_result "$WORK/extract.log" "$rc" "$destdir" "$gone"
  if [ "$UNIT_OK" = 1 ]; then
    if [ ${#RESTORE_PATHS[@]} -gt 0 ]; then
      DONE_LINES+=("${RESTORE_PATHS[*]} of '$unit' restored to: $destdir")
    else
      DONE_LINES+=("Unit '$unit' restored to: $destdir")
    fi
  fi
}

# The slices a pending job is still short of: job|number|bytes|sha256|label.
needed_slices() {
  local j
  for j in ${PENDING[@]+"${PENDING[@]}"}; do
    U="${JOB_UNIT[$j]}" awk -F'|' -v j="$j" -v q="'" '{ print j "|" $1 "|" $3 "|" $4 "|unit " q ENVIRON["U"] q " slice " $1 }' "$WORK/job.$j.missing"
  done
}

# What is still needed, by sha256: the figure to look for in another tape's
# front index, and the number to find it by in this unit.
print_needs() { # <job: its missing file is read, or - to print $WORK/needed.txt>
  local job num size sha label
  echo ""
  info "Slices still needed. Another copy holds each one as the same bytes, listed under the same sha256"
  info "in its front index (the number is only this tape's numbering of the unit):"
  if [ "$1" = - ]; then
    while IFS='|' read -r job num size sha label; do
      info "  $label, $size bytes, sha256 $sha"
    done <"$WORK/needed.txt"
  else
    while IFS='|' read -r num _ size sha; do
      info "  unit '${JOB_UNIT[$1]}' slice $num, $size bytes, sha256 $sha"
    done <"$WORK/job.$1.missing"
  fi
}

# The cartridge now in the drive, as far as this script needs it: its label,
# and its file map in $WORK/other.files (position|type|size|sha256 per line).
# Everything the first tape established is put back afterwards. A cartridge
# that requires short-seal has its seal read first, before any of its slices,
# and nothing after its ended_after is taken from it (R5): its map keeps only
# the files on it.
OTHER_TAPE=0
OTHER_LABEL=""
other_tape_map() {
  local k_fi=$FRONT_INDEX k_seal=$SEAL_MARKER k_total=$TOTAL_FILES k_label=$TAPE_LABEL
  local k_created=${CREATED:-} k_files=$FILES_TXT k_src=${FILES_SOURCE:-} uuid ok=0
  local k_short=$TAPE_SHORT_SEAL k_end=$ENDED_AFTER
  cp "$FILES_TXT" "$WORK/tape1.files"
  OTHER_TAPE=1
  CUR=""
  bootstrap_thunk
  OTHER_TAPE=0
  OTHER_LABEL=$TAPE_LABEL
  uuid=$(toml_val "$WORK/thunk.toml" uuid)
  if [ -n "$uuid" ] && [ "$uuid" = "$TAPE1_UUID" ]; then
    info "That is the cartridge this run started with (volume uuid $(safe_str "$uuid")). It cannot hold a good copy of its own bad slices."
  elif try_front_index; then
    cp "$WORK/files_from_index.txt" "$WORK/other.files"
    ok=1
    learn_end
  elif try_seal_copy; then
    echo "WARNING: the front index of this cartridge is unreadable: using the seal marker's copy of it." >&2
    cp "$WORK/files_from_seal.txt" "$WORK/other.files"
    ok=1
    learn_end 1
  else
    info "Neither the front index of this cartridge nor the seal marker's copy of it can be read."
  fi
  if [ "$ok" = 1 ] && tape_is_short; then
    info "This cartridge ended early, after file $ENDED_AFTER (its seal marker says so): nothing after that is read from it."
    awk -F'|' -v k="$ENDED_AFTER" '$1 + 0 <= k + 0' "$WORK/other.files" >"$WORK/other.files.new"
    mv "$WORK/other.files.new" "$WORK/other.files"
  fi
  FRONT_INDEX=$k_fi SEAL_MARKER=$k_seal TOTAL_FILES=$k_total TAPE_LABEL=$k_label CREATED=$k_created
  FILES_SOURCE=$k_src TAPE_SHORT_SEAL=$k_short ENDED_AFTER=$k_end
  cp "$WORK/tape1.files" "$k_files"
  [ "$ok" = 1 ]
}

# Read the cartridge now in the drive for the slices still needed (the file
# $WORK/needed.txt), by sha256 only, in the order they lie on it: one rewind,
# then forward. In restore mode each is read into the scratch area, checked
# against the sha256 the first tape's front index gave, and decrypted into the
# slice directory of every job short of it; in verify mode it is only hashed.
swap_fetch() {
  local pos sha size job num rest
  if ! (tape_init) 2>"$WORK/swap.err"; then
    echo "NOTE: the drive is not ready: $(head -n 1 "$WORK/swap.err" 2>/dev/null)" >&2
    echo "      Wait until the cartridge is loaded, then press ENTER again." >&2
    return 1
  fi
  other_tape_map || return 1
  info "This cartridge identifies as: $(safe_str "${OTHER_LABEL:-<unreadable>}")"
  awk -F'|' '
    FILENAME == ARGV[1] { need[$4] = $3; next }
    $2 == "data_slice" && ($4 in need) && $3 == need[$4] && !($4 in seen) { seen[$4] = 1; print $1 "|" $4 "|" $3 }
  ' "$WORK/needed.txt" "$WORK/other.files" >"$WORK/fetch.plan"
  if [ ! -s "$WORK/fetch.plan" ]; then
    info "No file in the front index of this cartridge has the sha256 of a slice still needed."
    return 1
  fi
  : >"$WORK/fetched.txt"
  while IFS='|' read -r pos sha size; do
    require_uint position "$pos"
    require_uint size_bytes "$size"
    info "Tape file $pos of this cartridge lists sha256 ${sha:0:16}..., a slice still needed"
    if [ "$VERIFY_ONLY" = 1 ]; then
      if hash_tape_file "$pos" "$size" && [ "$TAPE_SHA" = "$sha" ]; then
        info "  checksum verified: a good copy"
        echo "$sha" >>"$WORK/fetched.txt"
      else
        echo "NOTE: tape file $pos of this cartridge does not hash to that sha256 either." >&2
      fi
      continue
    fi
    if read_slice_checked "$pos" "$size" "$sha" "$SCRATCH/fetch.enc"; then
      info "  checksum verified against the first tape's front index"
      while IFS='|' read -r job num _ rest; do
        [ "${rest%%|*}" = "$sha" ] || continue
        decrypt_slice "$SCRATCH/fetch.enc" "$SCRATCH/job.$job/restore.$num.dar" "$num"
      done <"$WORK/needed.txt"
      rm -f "$SCRATCH/fetch.enc"
      echo "$sha" >>"$WORK/fetched.txt"
    else
      echo "NOTE: tape file $pos of this cartridge $SLICE_WHY: it is not used." >&2
    fi
  done <"$WORK/fetch.plan"
  for job in ${PENDING[@]+"${PENDING[@]}"}; do
    awk -F'|' 'FILENAME == ARGV[1] { got[$1] = 1; next } !($4 in got)' "$WORK/fetched.txt" "$WORK/job.$job.missing" >"$WORK/job.$job.missing.new"
    mv "$WORK/job.$job.missing.new" "$WORK/job.$job.missing"
  done
  if [ "$VERIFY_ONLY" = 1 ]; then
    local e
    local -a keep=()
    for e in ${VERIFY_BAD[@]+"${VERIFY_BAD[@]}"}; do
      grep -qxF -- "${e##*|}" "$WORK/fetched.txt" || keep+=("$e")
    done
    VERIFY_BAD=(${keep[@]+"${keep[@]}"})
  fi
}

# Ask for cartridges until nothing pending is still short or the heir says to
# go on without. The first tape has been read to its end by now; nothing here
# moves it back.
swap_loop() {
  local reply
  while true; do
    if [ "$VERIFY_ONLY" = 1 ]; then
      verify_needed >"$WORK/needed.txt"
    else
      needed_slices >"$WORK/needed.txt"
    fi
    [ -s "$WORK/needed.txt" ] || return 0
    print_needs -
    info "Load a cartridge that holds copies of these slices into $DEVICE (any tape whose front index lists"
    info "the sha256 values above; it need not be a copy of the whole tape), wait until the drive is ready,"
    info "then press ENTER. Type done and ENTER (or end the input) to go on without them."
    reply=quit
    IFS= read -r reply || reply=quit
    case "$reply" in
    [dD]one | DONE | q | quit)
      info "Going on without them."
      return 0
      ;;
    esac
    swap_fetch || true
  done
}

# After the last unit has been read from the first tape: the second cartridge
# for the units still short, then dar for each of them.
finish_pending() {
  local j
  [ ${#PENDING[@]} -gt 0 ] || return 0
  swap_loop
  for j in "${PENDING[@]}"; do
    job_run "$j"
  done
  PENDING=()
}

# The end of a restore: COMPLETE only when no unit came back short. A unit
# --all left out because the tape ended before it is named, not counted short:
# it is not on this tape whole by design, and another tape holds it.
restore_end() {
  local line
  note_other_units
  note_cut_units
  echo ""
  if [ ${#INCOMPLETE[@]} -gt 0 ]; then
    info "RESTORE INCOMPLETE"
    for line in "${INCOMPLETE[@]}"; do
      printf '%s\n' "$line" | sed 's/^/>>> /'
    done
    for line in ${DONE_LINES[@]+"${DONE_LINES[@]}"}; do
      info "$line"
    done
    exit 1
  fi
  info "RESTORE COMPLETE"
  for line in ${DONE_LINES[@]+"${DONE_LINES[@]}"}; do
    info "$line"
  done
}

# ---- --restore --fill-from LIST: the files an earlier restore could not restore ----

# Restore, from the tape in the drive, exactly the files a damaged-file list
# names, over the damaged or missing copies in the directory the list sits in,
# and nothing else there. The tape is used only if its envelope lists the same
# unit with the same uuid at the same version as the list's header; dar's own
# checks confirm each file it restores. The list is rewritten with whatever is
# still damaged, and removed when nothing is.
FILL_ESC=()
FILL_PATH=()
do_fill() { # <destination> <version given, or empty> <scratch parent, or empty>
  local destdir=$1 want_version=${2:-} scr=${3:-} list=$FILL_FROM
  local first magic fmt f_unit f_uuid f_ver unit uuid ver line p root listdir todir sub
  [ -f "$list" ] || die "--fill-from: no such file: $list"
  [ -d "$destdir" ] || die "--to $destdir does not exist: a fill restores into the directory the earlier restore wrote"
  first=""
  IFS= read -r first <"$list" || true
  magic="" fmt="" f_unit="" f_uuid="" f_ver=""
  IFS=$'\t' read -r magic fmt f_unit f_uuid f_ver _ <<<"$first" || true
  [ "$magic" = '#tapectl-damaged-files' ] ||
    die "$list is not a list of damaged files written by this script: its first line is not the header"
  [ "$fmt" = 1 ] || die "$list is a list of format $(safe_str "$fmt"); this script reads format 1"
  case "$f_unit:$f_uuid:$f_ver" in
  unit=*:uuid=?*:version=?*) ;;
  *) die "$list: the header does not name a unit, its uuid and its version" ;;
  esac
  unesc_line "${f_unit#unit=}"
  unit=$UNESC
  uuid=${f_uuid#uuid=}
  ver=${f_ver#version=}
  require_uint version "$ver"
  case "$uuid" in
  *[!0-9a-fA-F-]*) die "$list: the unit uuid in the header is not a uuid" ;;
  esac
  if [ ${#RESTORE_UNITS[@]} -ne 1 ] || [ "${RESTORE_UNITS[0]}" != "$unit" ]; then
    die "this list is for unit '$(visible_text "$unit")', version $ver: give --unit '$(visible_text "$unit")' (and only that unit)"
  fi
  if [ -n "$want_version" ] && [ "$want_version" != "$ver" ]; then
    die "this list is for version $ver of '$(visible_text "$unit")', and --version says $want_version: a fill uses the list's version only"
  fi

  # The directory the list sits in is the unit's directory: --to itself for a
  # restore of one unit, --to/UNIT for several.
  listdir=$(cd "$(dirname -- "$list")" && pwd -P) || die "cannot enter the directory of $list"
  todir=$(cd "$destdir" && pwd -P) || die "cannot enter --to $destdir"
  root=""
  if [ "$listdir" = "$todir" ]; then
    root=$todir
  else
    sub=$(cd "$todir/$unit" 2>/dev/null && pwd -P) || sub=""
    if [ -n "$sub" ] && [ "$listdir" = "$sub" ]; then
      root=$sub
    fi
  fi
  [ -n "$root" ] ||
    die "$list is not in --to ($todir) or in --to/UNIT: keep the list where the restore wrote it,
       in the unit's own directory, and give the same --to"
  [ "$(basename -- "$list")" = "$DAMAGED_NAME" ] || echo "NOTE: the list is named $(basename -- "$list"), not $DAMAGED_NAME; it is rewritten under the name it has." >&2

  FILL_ESC=()
  FILL_PATH=()
  : >"$WORK/fill.wanted"
  while IFS= read -r line || [ -n "$line" ]; do
    [ -n "$line" ] || continue
    unesc_line "$line"
    p=$UNESC
    case "$p" in
    /*) die "$list names an absolute path ($(visible_text "$p")): a list names paths inside the unit" ;;
    esac
    case "/$p/" in
    */../*) die "$list names a path with .. in it ($(visible_text "$p")): a list names paths inside the unit" ;;
    esac
    if [ -d "$root/$p" ] && [ ! -L "$root/$p" ]; then
      die "$list names a directory ($(visible_text "$p")): a fill restores files, and restoring a directory
       would overwrite everything under it. Remove the line from the list."
    fi
    FILL_ESC+=("$line")
    FILL_PATH+=("$p")
    printf '%s\n' "$line" >>"$WORK/fill.wanted"
  done < <(tail -n +2 "$list")
  [ ${#FILL_PATH[@]} -gt 0 ] || die "$list names no file"
  info "Filling ${#FILL_PATH[@]} file(s) of unit '$(visible_text "$unit")', version $ver, in $root"

  SCRATCH_PARENT=${scr:-$root}
  mkdir -p "$SCRATCH_PARENT"
  check_stale_scratch "$root" "$SCRATCH_PARENT"
  dar_check
  set_age_ids
  establish_files
  [ "$SALVAGE" = 0 ] || salvage_init

  walk_envelopes pick_for_restore
  [ -n "$CHOSEN_ENV" ] || die_no_envelope "$(visible_text "$unit")"
  info "Decrypted envelope at file $CHOSEN_POS"
  info "  opened with key $CHOSEN_KEY"
  local manifest="$CHOSEN_ENV/MANIFEST.toml" here
  [ -f "$manifest" ] || die "envelope missing MANIFEST.toml"
  here=$(unit_uuid_of "$CHOSEN_ENV" "$unit" "$ver")
  if [ -z "$here" ]; then
    echo "This tape lists the unit '$(visible_text "$unit")' as:" >&2
    units_table "$manifest" >&2
    die "this tape holds no version $ver of unit '$(visible_text "$unit")'. A fill uses the same unit at the same version
       only (another version may differ in the very files that are damaged). Nothing was changed."
  fi
  if [ "$here" != "$uuid" ]; then
    die "this tape's version $ver of '$(visible_text "$unit")' has unit uuid $(safe_str "$here"), and the list says $(safe_str "$uuid"):
       it is not the same unit. Nothing was changed."
  fi
  local base
  base=$(catalogue_path "$CHOSEN_ENV" "$unit" "$ver")
  [ -n "$base" ] || die "this tape's envelope carries no dar catalogue for '$(visible_text "$unit")' version $ver, so a fill cannot
       tell which slices hold the files. Restore the whole unit into another directory instead
       (--restore --unit NAME --to NEWDIR) and copy the files from there."

  # Which of the files this version's catalogue holds, and in which slices.
  dar -l "$base" -N -Q -Tslice 2>"$WORK/fill.dar.err" |
    FILL_WANTED="$WORK/fill.wanted" awk '
    function esc(s,    out, i, n, c) {
      out = ""
      n = length(s)
      for (i = 1; i <= n; i++) {
        c = substr(s, i, 1)
        if (c == "\\") out = out "\\\\"
        else if (c == "\t") out = out "\\t"
        else if (c == "\n") out = out "\\n"
        else if (c == "\r") out = out "\\r"
        else out = out c
      }
      return out
    }
    function flush(    e, a, k, lo, hi, i) {
      if (!open) return
      open = 0
      e = esc(path)
      if ((e in want) && !(e in done)) {
        done[e] = 1
        print "P\t" e
        k = split(slices, a, "-")
        lo = a[1] + 0
        hi = (k > 1) ? a[2] + 0 : lo
        for (i = lo; i <= hi; i++) if (!(i in mark)) { mark[i] = 1; print "S\t" i }
      }
    }
    BEGIN {
      wf = ENVIRON["FILL_WANTED"]
      while ((getline line < wf) > 0) want[line] = 1
      close(wf)
      perm = " [^ ][-rwxsStT][-rwxsStT][-rwxsStT][-rwxsStT][-rwxsStT][-rwxsStT][-rwxsStT][-rwxsStT][-rwxsStT] "
    }
    $0 == "-----" { flush(); stopped = 1 }
    stopped { next }
    /^[0-9]+(-[0-9]+)?\t / {
      flush()
      t = index($0, "\t")
      rest = substr($0, t + 1)
      if (match(rest, perm)) {
        slices = substr($0, 1, t - 1)
        path = substr(rest, RSTART + RLENGTH)
        open = 1
      }
      next
    }
    open { path = path "\n" $0 }
    END { flush() }
  ' >"$WORK/fill.found" ||
    die "dar could not read the catalogue of '$(visible_text "$unit")': $(tail -n 3 "$WORK/fill.dar.err")"
  awk -F'\t' '$1 == "P" { print $2 }' "$WORK/fill.found" >"$WORK/fill.present"
  awk -F'\t' '$1 == "S" { print $2 }' "$WORK/fill.found" | sort -n >"$WORK/fill.slices"
  awk 'FILENAME == ARGV[1] { a[$0] = 1; next } !($0 in a)' "$WORK/fill.present" "$WORK/fill.wanted" >"$WORK/fill.absent"
  if [ -s "$WORK/fill.absent" ]; then
    echo "NOTE: $(wc -l <"$WORK/fill.absent") listed path(s) are not in this tape's catalogue of the version; they stay in the list:" >&2
    sed 's/^/        /' "$WORK/fill.absent" | head -n 20 >&2
  fi
  if [ ! -s "$WORK/fill.present" ]; then
    die "none of the listed files is in this tape's version $ver of '$(visible_text "$unit")'. The list is unchanged."
  fi

  pick_slices "$manifest" "$unit" "$WORK/slices.0" "$ver"
  # A fill takes nothing from a tape that does not hold the version whole (R4,
  # R5): the tape it reads is the other copy, and its own seal said where it
  # ended. The list is unchanged.
  [ "$PICKED_CUT" = 0 ] || die "$(cut_text "$unit" "$ver" "$PICKED_LAST")
       A fill uses a tape that holds the unit whole. Nothing was changed."
  local last
  last=$(tail -n 1 "$WORK/slices.0" | cut -d'|' -f1)
  awk -F'|' -v last="$last" 'FILENAME == ARGV[1] { need[$1] = 1; next } ($1 in need) || $1 == last' "$WORK/fill.slices" "$WORK/slices.0" >"$WORK/fill.slice.list"
  local total=0 n num tpos size sha
  n=$(wc -l <"$WORK/fill.slice.list")
  while IFS='|' read -r num tpos size sha; do
    total=$((total + size))
  done <"$WORK/fill.slice.list"
  info "The files lie in $((n - 1)) slice(s) of $last; reading those and the last"
  check_space "$root" "$SCRATCH_PARENT" "$total" "$total"
  ensure_scratch

  local j=$JOB_N
  JOB_N=$((JOB_N + 1))
  JOB_KIND[$j]=fill
  JOB_DEST[$j]=$root
  JOB_UNIT[$j]=$unit
  JOB_VER[$j]=$ver
  JOB_UUID[$j]=$uuid
  mkdir -p "$SCRATCH/job.$j"
  FILL_HEADER=$first
  spool_slices "$WORK/fill.slice.list" "$SCRATCH/job.$j" "$WORK/job.$j.missing"
  if [ -s "$WORK/job.$j.missing" ] && [ "$SWAP_TAPES" = 1 ]; then
    PENDING+=("$j")
    finish_pending
  else
    job_run "$j"
  fi
  fill_end "$list" "$root"
}

FILL_HEADER=""
FILL_BAD="$WORK/fill.bad"

# dar over the files of a fill, 500 paths to a run, always overwriting (these
# are the files to replace) and never anything else: -g names a path and
# takes no pattern. What dar reports it could not restore, and every path it
# was never asked for because it is not in the catalogue, is still damaged.
fill_extract() { # <job>
  local j=$1 root i k rc run=0 errs skipped unmapped failed
  root=$(abs_dir "${JOB_DEST[$j]}")
  UNIT_NAME=${JOB_UNIT[$j]} UNIT_VER=${JOB_VER[$j]} UNIT_UUID=${JOB_UUID[$j]}
  : >"$FILL_BAD"
  local gone=""
  if [ -s "$WORK/job.$j.missing" ]; then
    gone=$(missing_numbers "$j")
    print_needs "$j"
    echo "NOTE: filling without slice(s) $gone: the files in them stay in the list." >&2
  fi
  awk 'FILENAME == ARGV[1] { a[$0] = 1; next } ($0 in a) { print FNR }' "$WORK/fill.present" "$WORK/fill.wanted" >"$WORK/fill.rows"
  local -a g=()
  local row
  k=0
  while true; do
    g=()
    while [ ${#g[@]} -lt 1000 ] && IFS= read -r row <&3; do
      g+=(-g "${FILL_PATH[$((row - 1))]}")
    done
    [ ${#g[@]} -gt 0 ] || break
    run=$((run + 1))
    rc=0
    info "Restoring $((${#g[@]} / 2)) file(s) over their damaged copies ..."
    dar -x "$SCRATCH/job.$j/restore" -R "$root" -O -Q -N -w "${g[@]}" >"$WORK/fill.$run.log" 2>&1 </dev/null || rc=$?
    cat "$WORK/fill.$run.log"
    if [ "$rc" != 0 ] && grep -i 'no space left' "$WORK/fill.$run.log" >/dev/null 2>&1; then
      die_io "dar extraction failed" "$WORK/fill.$run.log" "$root"
    fi
    if [ "$rc" != 0 ] && [ "$rc" != 5 ]; then
      die "dar extraction failed (exit $rc) — dar's own message is above. The list is unchanged."
    fi
    DAMAGED_ROOT=$root DAMAGED_SKIPS=1 DAMAGED_COUNTS="$WORK/damaged.counts" \
      awk '
    function esc(s,    out, i, n, c) {
      out = ""
      n = length(s)
      for (i = 1; i <= n; i++) {
        c = substr(s, i, 1)
        if (c == "\\") out = out "\\\\"
        else if (c == "\t") out = out "\\t"
        else if (c == "\n") out = out "\\n"
        else if (c == "\r") out = out "\\r"
        else out = out c
      }
      return out
    }
    function lastsep(s,    p, q) {
      p = 0
      while ((q = index(substr(s, p + 1), " : ")) > 0) p = p + q
      return p
    }
    function emit(path,    e) {
      if (index(path, root) != 1 || length(path) <= rl) { unmapped++; return }
      e = esc(substr(path, rl + 1))
      if (!(e in seen)) { seen[e] = 1; print e }
    }
    BEGIN {
      root = ENVIRON["DAMAGED_ROOT"] "/"
      rl = length(root)
      skips = (ENVIRON["DAMAGED_SKIPS"] == "1")
      counts = ENVIRON["DAMAGED_COUNTS"]
      tail = " not restored (user choice)"
      tl = length(tail)
    }
    {
      if (acc != "" && (index($0, "Error while restoring ") == 1 || $0 ~ /^ -----/ || (kind == "S" && index($0, root) == 1))) {
        unmapped++; acc = ""; kind = ""
      }
      if (acc == "") {
        if (index($0, "Error while restoring ") == 1) { acc = substr($0, 23); kind = "E" }
        else if (skips && index($0, root) == 1) { acc = $0; kind = "S" }
        else {
          if ($0 ~ /^ [0-9]+ inode\(s\) failed to restore/) failed = $1 + 0
          next
        }
      } else {
        acc = acc "\n" $0
      }
      if (kind == "E") {
        p = lastsep(acc)
        if (p > 0) { emit(substr(acc, 1, p - 1)); errs++; acc = ""; kind = "" }
      } else {
        n = length(acc)
        if (n >= tl && substr(acc, n - tl + 1) == tail) { emit(substr(acc, 1, n - tl)); skipped++; acc = ""; kind = "" }
      }
    }
    END {
      if (acc != "") unmapped++
      print errs + 0, skipped + 0, unmapped + 0, failed + 0 > counts
      close(counts)
    }
  ' "$WORK/fill.$run.log" >>"$FILL_BAD"
    errs=0 skipped=0 unmapped=0 failed=0
    read -r errs skipped unmapped failed <"$WORK/damaged.counts" || true
    if [ "$unmapped" -gt 0 ] || [ "$failed" -ne "$errs" ]; then
      echo "WARNING: dar reported $failed file(s) failed and this script could name $errs; a file it could not" >&2
      echo "         name is missing from the rewritten list. Read dar's messages above." >&2
    fi
  done 3<"$WORK/fill.rows"
  rm -rf "$SCRATCH/job.$j"
}

# After the fill: the list now holds the files still damaged, or is gone.
fill_end() { # <list> <root>
  local list=$1 root=$2 n
  awk '
    FILENAME == ARGV[1] { bad[$0] = 1; ord[++k] = $0; next }
    FILENAME == ARGV[2] { if (!($0 in bad)) { bad[$0] = 1; ord[++k] = $0 } next }
    { w[$0] = 1; if (($0 in bad) && !($0 in out)) { out[$0] = 1; print } }
    END { for (i = 1; i <= k; i++) if (!(ord[i] in out)) { out[ord[i]] = 1; print ord[i] } }
  ' "$WORK/fill.absent" "$FILL_BAD" "$WORK/fill.wanted" >"$WORK/fill.still"
  # What dar restored without complaint is checked against the index too
  # (ADR-0012 item 103): a file that fails stays in the list.
  awk 'FILENAME == ARGV[1] { s[$0] = 1; next } !($0 in s)' "$WORK/fill.still" "$WORK/fill.wanted" >"$WORK/fill.restored"
  file_check "$root" "$WORK/fill.restored"
  if [ "$CHECK_UNNAMED" -gt 0 ] && [ "$CHECK_FAILED" -eq 0 ]; then
    cp "$WORK/fill.wanted" "$WORK/fill.still"
  elif [ "$CHECK_FAILED" -gt 0 ]; then
    awk '!seen[$0]++' "$WORK/fill.still" "$CHECK_BAD" >"$WORK/fill.still.new"
    mv "$WORK/fill.still.new" "$WORK/fill.still"
  fi
  n=$(wc -l <"$WORK/fill.still")
  echo ""
  if [ "$n" -eq 0 ]; then
    rm -f "$list"
    info "FILL COMPLETE: every listed file of '$(visible_text "$UNIT_NAME")' was restored from this tape into $root"
    info "  (dar checked each one). $list is removed."
    return 0
  fi
  {
    printf '%s\n' "$FILL_HEADER"
    cat "$WORK/fill.still"
  } >"$list"
  info "FILL INCOMPLETE: $n file(s) of '$(visible_text "$UNIT_NAME")' are still damaged or missing. The list now holds only those:"
  info "  $list"
  info "Fill them from another tape holding this unit at version $UNIT_VER, the same way."
  exit 1
}

# ---- the cross-volume index (--where, --find; volume-format-v2.md 1.5) ----

# A tape written since version 1.3.0 carries, in each tenant envelope,
# index/units.tsv (that tenant's units: every version and every tape holding it)
# and index/paths.tsv (every path of those versions); the operator envelope
# carries index/units.tsv for every tenant. Both are tab-separated text with a
# header line naming the columns, and both are read here with awk, so they need
# no more than the tools that open an envelope. The index is as of the moment
# its own tape was written: it never names a tape written after it, and a tape
# it names may since have been retired or erased.
INDEX_OPENED=0
INDEX_HAD=0

# On a short tape, the versions an opened envelope lists that this tape does
# not hold whole go to $WORK/cut.versions (uuid TAB version), which the index
# programs read through INDEX_CUT: a this-tape row of such a version is cut,
# never a holder, and the loaded tape does not count as holding it (R4).
note_cut_versions() { # <envelope dir>
  tape_is_short || return 0
  [ -f "$1/MANIFEST.toml" ] || return 0
  awk -v k="$ENDED_AFTER" '
    function done_block() {
      if (inblk && cut && id != "") printf "%s\t%s\n", id, v
      inblk = 0; cut = 0; id = ""; v = ""
    }
    /^\[\[units\]\]/ { done_block(); inblk = 1; in_head = 1; in_s = 0; next }
    /^\[\[units\.slices\]\]/ { in_head = 0; in_s = inblk; next }
    /^\[/ { in_head = 0; in_s = 0 }
    in_head && /^uuid = / { id = $3; gsub(/"/, "", id) }
    in_head && /^snapshot_version = / { v = $3 + 0 }
    in_s && /^tape_position = / { if ($3 + 0 > k + 0) cut = 1 }
    END { done_block() }
  ' "$1/MANIFEST.toml" >>"$WORK/cut.versions"
}

# The line after a listing whose holders include a version this tape cut.
note_cut_holders() {
  [ -s "$WORK/cut.versions" ] || return 0
  info "A holder marked CUT on this tape is the loaded tape, which ended early, after file $ENDED_AFTER (its"
  info "seal marker says so), before the last slice of that version: it does not hold it whole."
}

index_banner() {
  echo ""
  info "Index read from tape: ${TAPE_LABEL:-<unreadable>} (written $(safe_str "${CREATED:-unknown}"))"
  info "An index is as of the moment its own tape was written. It never names a tape written"
  info "after it, and a tape it names may since have been retired or erased. The newest tape"
  info "you hold has the most complete index: if this is not it, run this on that one."
}

# A tape with no index says so and points at what it can do. Exit 1: the answer
# is not on this tape.
die_no_index() {
  echo ""
  info "This tape carries no index. Tapes written before version 1.3.0 have none, and nothing"
  info "on this tape says where a unit's other versions or copies are."
  info "What this tape can tell you about itself:"
  info "  $0 --find-envelope $(keys_args)     the units, versions and slices it holds"
  info "  $0 --list $(keys_args)              the files of a unit (add --unit NAME)"
  info "For another tape, load it and run its own RESTORE.sh the same way; a tape written"
  info "since version 1.3.0 can answer for all the tapes written before it."
  exit 1
}

# --where: copy out the units.tsv of each envelope a key opens, then remove the
# envelope (an escrow key opens them all, and a paths.tsv can be large). An
# operator envelope's units.tsv lists every tenant's units, so the walk stops
# at it; the tenant envelopes come before it on the tape.
WHERE_FILES=()
WHERE_TENANTS=()
WHERE_OPERATOR=""
where_envelope() { # <position>
  local pos=$1 d="$WORK/opened.$1" ty who=""
  INDEX_OPENED=$((INDEX_OPENED + 1))
  note_cut_versions "$d"
  ty=$(file_type_at "$pos" "$FILES_TXT")
  if [ -f "$d/index/units.tsv" ]; then
    INDEX_HAD=$((INDEX_HAD + 1))
    mv "$d/index/units.tsv" "$WORK/where.$pos.tsv"
    case "$ty" in
    operator_envelope*)
      WHERE_OPERATOR="$WORK/where.$pos.tsv"
      rm -rf "$d"
      return 1
      ;;
    esac
    who=$(toml_val "$d/MANIFEST.toml" tenant 2>/dev/null || true)
    WHERE_FILES+=("$WORK/where.$pos.tsv")
    WHERE_TENANTS+=("$(safe_str "$who")")
  fi
  rm -rf "$d"
  return 0
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
      INDEX_CUT="$WORK/cut.versions" \
      awk '
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
    function cut_here(row, names,    f, line) {
      if (!cut_read) {
        cut_read = 1
        f = ENVIRON["INDEX_CUT"]
        if (f != "") {
          while ((getline line < f) > 0) cutv[line] = 1
          close(f)
        }
      }
      if (val(row, names, "state") != "this-tape") return 0
      return ((val(row, names, "unit_uuid") "\t" val(row, names, "version")) in cutv)
    }
    function holder(row, names,    state, sealed, first, last, count, st, s) {
      state = show(val(row, names, "state"))
      sealed = show(val(row, names, "sealed_at"))
      first = show(val(row, names, "first_slice"))
      last = show(val(row, names, "last_slice"))
      count = show(val(row, names, "num_slices"))
      if (state == "this-tape" && cut_here(row, names)) st = "CUT on this tape: it ended early, before the last slice of this version, so it does not hold it whole"
      else if (state == "this-tape") st = "this tape"
      else if (state == "sealed") st = (sealed == "") ? "sealed" : "sealed " sealed
      else if (state == "quarantined") {
        st = "QUARANTINED: a verify proved this medium bad, it may still read in part"
        if (sealed != "") st = st "; sealed " sealed
      }
      else st = (state == "") ? "state not known" : "state " state
      s = "    tape " nk(show(val(row, names, "volume"))) "  (" st ")"
      if (first != "" && last != "") s = s "  slices " first "-" last (count == "" ? "" : " of " count)
      s = s "\n        volume uuid " nk(show(val(row, names, "volume_uuid")))
      s = s "; cartridge serial " nk(show(val(row, names, "cartridge_serial")))
      s = s "; generation " nk(show(val(row, names, "generation")))
      return s
    }
   
    function holders_of(k,    j, r, s, lab, st) {
      s = ""
      for (j = 1; j <= rows[k]; j++) {
        split(row[k, j], r, "\t")
        lab = nk(show(val(r, h, "volume")))
        st = val(r, h, "state")
        if (st == "this-tape" && cut_here(r, h)) lab = lab " (CUT on this tape: not held whole)"
        else if (st == "this-tape") lab = lab " (this tape)"
        else if (st == "quarantined") lab = lab " (QUARANTINED)"
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
      if (val(f, h, "state") == "this-tape" && !cut_here(f, h)) here[k] = 1
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
        print "Unit " show(gunit[n]) who ": newest version " show(vtext[kn]) ", " (onhere ? "on this tape" : "NOT on this tape")
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
  ' "$f" >>"$WORK/all.out" || rc=$?
    case "$rc" in
    0 | 1)
      n="" a=""
      read -r n a <"$WORK/all.counts" || true
      if is_uint "$n" && is_uint "$a"; then
        total=$((total + n))
        here=$((here + a))
      fi
      ;;
    *) echo "WARNING: an index in this tape's envelopes has no unit or version column, so this script cannot read it." >&2 ;;
    esac
  done
  if [ "$total" -eq 0 ]; then
    info "This index names no unit."
    exit 1
  fi
  info "$total unit(s) in this index; this tape holds the newest version of $here of them."
  if [ -s "$WORK/all.absent" ]; then
    echo ""
    info "The newest version of these is NOT on this tape (unit, version, unit uuid, the tapes that hold it):"
    cat "$WORK/all.absent"
  fi
  echo ""
  info "Every unit the index names, with every version and every tape that holds it:"
  echo ""
  cat "$WORK/all.out"
  info "A holder marked 'this tape' is the tape you have loaded. For any other, find the cartridge"
  info "by its label or its chip serial, load it, and use the RESTORE.sh stored on that tape."
  note_cut_holders
}

do_where() { # <unit, or --all for every unit>
  local unit=$1 i rc found=0 f
  establish_files
  : >"$WORK/cut.versions"
  walk_envelopes where_envelope
  [ "$INDEX_OPENED" -gt 0 ] || die_no_envelope
  index_banner
  echo ""
  if [ -n "$WHERE_OPERATOR" ]; then
    WHERE_FILES=("$WHERE_OPERATOR")
    WHERE_TENANTS=("")
  fi
  [ "$INDEX_HAD" -gt 0 ] || die_no_index
  if [ "$unit" = --all ]; then
    where_every_unit
    return 0
  fi
  : >"$WORK/where.known"
  for ((i = 0; i < ${#WHERE_FILES[@]}; i++)); do
    f=${WHERE_FILES[$i]}
    rc=0
    WHERE_UNIT="$unit" WHERE_KNOWN="$WORK/where.known" WHERE_TENANT="${WHERE_TENANTS[$i]}" \
      INDEX_CUT="$WORK/cut.versions" \
      awk '
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
    function cut_here(row, names,    f, line) {
      if (!cut_read) {
        cut_read = 1
        f = ENVIRON["INDEX_CUT"]
        if (f != "") {
          while ((getline line < f) > 0) cutv[line] = 1
          close(f)
        }
      }
      if (val(row, names, "state") != "this-tape") return 0
      return ((val(row, names, "unit_uuid") "\t" val(row, names, "version")) in cutv)
    }
    function holder(row, names,    state, sealed, first, last, count, st, s) {
      state = show(val(row, names, "state"))
      sealed = show(val(row, names, "sealed_at"))
      first = show(val(row, names, "first_slice"))
      last = show(val(row, names, "last_slice"))
      count = show(val(row, names, "num_slices"))
      if (state == "this-tape" && cut_here(row, names)) st = "CUT on this tape: it ended early, before the last slice of this version, so it does not hold it whole"
      else if (state == "this-tape") st = "this tape"
      else if (state == "sealed") st = (sealed == "") ? "sealed" : "sealed " sealed
      else if (state == "quarantined") {
        st = "QUARANTINED: a verify proved this medium bad, it may still read in part"
        if (sealed != "") st = st "; sealed " sealed
      }
      else st = (state == "") ? "state not known" : "state " state
      s = "    tape " nk(show(val(row, names, "volume"))) "  (" st ")"
      if (first != "" && last != "") s = s "  slices " first "-" last (count == "" ? "" : " of " count)
      s = s "\n        volume uuid " nk(show(val(row, names, "volume_uuid")))
      s = s "; cartridge serial " nk(show(val(row, names, "cartridge_serial")))
      s = s "; generation " nk(show(val(row, names, "generation")))
      return s
    }
   
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
  ' "$f" || rc=$?
    case "$rc" in
    0) found=1 ;;
    1) ;;
    *) echo "WARNING: an index in this tape's envelopes has no unit or version column, so this script cannot read it." >&2 ;;
    esac
  done
  if [ "$found" = 0 ]; then
    echo ""
    info "No unit named '$unit' is in this index."
    if [ -s "$WORK/where.known" ]; then
      info "It names these units (all of them; each one's newest version and holders:"
      info "  $0 --where --all $(keys_args)):"
      sort -u "$WORK/where.known" | sed 's/^/    /'
    fi
    if [ -z "$WHERE_OPERATOR" ]; then
      info "A tenant's key opens that tenant's index only; the operator or escrow key opens the"
      info "index of every tenant."
    fi
    exit 1
  fi
  echo ""
  info "A holder marked 'this tape' is the tape you have loaded. For any other, find the cartridge"
  info "by its label or its chip serial, load it, and use the RESTORE.sh stored on that tape."
  note_cut_holders
}

# --find: the matches of each tenant envelope's paths.tsv, kept in a file so the
# banner can come first. One limit is shared by every envelope.
FIND_TEXT=""
FIND_LEFT=50
FIND_SHOWN=0
FIND_TOTAL=0
find_in_envelope() { # <position>
  local pos=$1 d="$WORK/opened.$1" u who shown total
  INDEX_OPENED=$((INDEX_OPENED + 1))
  note_cut_versions "$d"
  if [ -f "$d/index/units.tsv" ] || [ -f "$d/index/paths.tsv" ]; then
    INDEX_HAD=$((INDEX_HAD + 1))
  fi
  if [ -f "$d/index/paths.tsv" ]; then
    u="$d/index/units.tsv"
    [ -f "$u" ] || u=/dev/null
    who=$(toml_val "$d/MANIFEST.toml" tenant 2>/dev/null || true)
    : >"$WORK/find.counts"
    FIND_TEXT="$FIND_TEXT" FIND_LIMIT="$FIND_LEFT" FIND_UNITS="$u" FIND_COUNTS="$WORK/find.counts" \
      FIND_WHO="tenant $(safe_str "$who")" INDEX_CUT="$WORK/cut.versions" \
      awk '
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
    function cut_here(row, names,    f, line) {
      if (!cut_read) {
        cut_read = 1
        f = ENVIRON["INDEX_CUT"]
        if (f != "") {
          while ((getline line < f) > 0) cutv[line] = 1
          close(f)
        }
      }
      if (val(row, names, "state") != "this-tape") return 0
      return ((val(row, names, "unit_uuid") "\t" val(row, names, "version")) in cutv)
    }
    function holder(row, names,    state, sealed, first, last, count, st, s) {
      state = show(val(row, names, "state"))
      sealed = show(val(row, names, "sealed_at"))
      first = show(val(row, names, "first_slice"))
      last = show(val(row, names, "last_slice"))
      count = show(val(row, names, "num_slices"))
      if (state == "this-tape" && cut_here(row, names)) st = "CUT on this tape: it ended early, before the last slice of this version, so it does not hold it whole"
      else if (state == "this-tape") st = "this tape"
      else if (state == "sealed") st = (sealed == "") ? "sealed" : "sealed " sealed
      else if (state == "quarantined") {
        st = "QUARANTINED: a verify proved this medium bad, it may still read in part"
        if (sealed != "") st = st "; sealed " sealed
      }
      else st = (state == "") ? "state not known" : "state " state
      s = "    tape " nk(show(val(row, names, "volume"))) "  (" st ")"
      if (first != "" && last != "") s = s "  slices " first "-" last (count == "" ? "" : " of " count)
      s = s "\n        volume uuid " nk(show(val(row, names, "volume_uuid")))
      s = s "; cartridge serial " nk(show(val(row, names, "cartridge_serial")))
      s = s "; generation " nk(show(val(row, names, "generation")))
      return s
    }
   
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
  ' "$d/index/paths.tsv" >>"$WORK/find.out" || {
      echo "WARNING: the index in the envelope at file $pos has no unit, version or path column, so this script cannot read it." >&2
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
  rm -rf "$d"
  return 0
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

do_find() { # <text>
  FIND_TEXT=$1
  : >"$WORK/find.out"
  establish_files
  : >"$WORK/cut.versions"
  walk_envelopes find_in_envelope
  [ "$INDEX_OPENED" -gt 0 ] || die_no_envelope
  index_banner
  [ "$INDEX_HAD" -gt 0 ] || die_no_index
  if [ "$FIND_TOTAL" -eq 0 ]; then
    echo ""
    info "No path in this index contains '$(visible_text "$FIND_TEXT")'. (The text is matched as it stands, not as a"
    info "pattern, and capitals matter. A path is relative to its unit's root.)"
    exit 1
  fi
  echo ""
  info "Paths containing '$(visible_text "$FIND_TEXT")' (unit, version, kind, size, path; kind is d directory,"
  info "f file, l symlink, s special). A TAB, newline or carriage return in a path is shown as"
  info "\\t, \\n or \\r."
  echo ""
  cat "$WORK/find.out"
  echo ""
  if [ "$FIND_TOTAL" -gt "$FIND_SHOWN" ]; then
    info "$((FIND_TOTAL - FIND_SHOWN)) more match(es) are not shown: this prints the first $FIND_SHOWN. Give more of"
    info "the path to narrow it."
  fi
  note_cut_holders
  info "To restore one of these from the tape that holds it:"
  info "  $0 --restore $(keys_args) --unit UNIT --path PATH --to /your/destination"
}

# ---- main ----

case "${1:-}" in
--info)
  require_tools "${TOOLS_READ[@]}"
  do_info
  ;;
--verify)
  shift
  while [ $# -gt 0 ]; do
    case "$1" in
    --slices-from)
      [ $# -ge 2 ] || die "--slices-from needs a directory

       usage: $0 --verify [--slices-from DIR ...] [--swap-tapes]"
      SLICES_FROM+=("$2")
      shift 2
      ;;
    --swap-tapes)
      SWAP_TAPES=1
      shift
      ;;
    *) die "unknown option: $1

       usage: $0 --verify [--slices-from DIR ...] [--swap-tapes]" ;;
    esac
  done
  check_slices_from
  require_tools "${TOOLS_READ[@]}"
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
  require_tools "${TOOLS_ENVELOPE[@]}"
  require_key_files
  do_find_envelope
  ;;
--restore)
  shift
  KEYS=()
  dest="" want="" scratch=""
  # Every flag but the switches takes a value. `shift 2` on a TRAILING bare
  # flag fails because $# is 1, `set -e` fires, and the script exits 1 having
  # printed nothing at all — the usage check below is never reached. Check the
  # arity first and say which flag was short (#133).
  while [ $# -gt 0 ]; do
    case "$1" in
    --key | --to | --unit | --version | --scratch | --path | --slices-from | --fill-from)
      [ $# -ge 2 ] || die "$1 needs a value

       usage: $0 --restore --key KEYFILE --to DIR [--unit U | --all] [--version N]"
      case "$1" in
      --key) KEYS+=("$2") ;;
      --to) dest=$2 ;;
      --unit) RESTORE_UNITS+=("$2") ;;
      --version) want=$2 ;;
      --scratch) scratch=$2 ;;
      --path) RESTORE_PATHS+=("$2") ;;
      --slices-from) SLICES_FROM+=("$2") ;;
      --fill-from) FILL_FROM=$2 ;;
      esac
      shift 2
      ;;
    --all)
      RESTORE_ALL=1
      shift
      ;;
    --swap-tapes)
      SWAP_TAPES=1
      shift
      ;;
    --no-space-check)
      SKIP_SPACE_CHECK=1
      shift
      ;;
    --overwrite)
      OVERWRITE=1
      shift
      ;;
    --no-file-check)
      FILE_CHECK=0
      shift
      ;;
    *) die "unknown option: $1" ;;
    esac
  done
  [ ${#KEYS[@]} -gt 0 ] || die "usage: $0 --restore --key KEYFILE --to DIR [--unit U | --all] [--version N]"
  [ -n "$dest" ] || die "usage: $0 --restore --key KEYFILE --to DIR [--unit U | --all] [--version N]"
  if [ "$RESTORE_ALL" = 1 ] && [ ${#RESTORE_UNITS[@]} -gt 0 ]; then
    die "--all restores every unit; give it without --unit"
  fi
  check_slices_from
  if [ -n "$FILL_FROM" ]; then
    [ "$RESTORE_ALL" = 0 ] || die "--fill-from fills the files of one unit: give --unit U, not --all"
    [ ${#RESTORE_PATHS[@]} -eq 0 ] || die "--fill-from restores the files its list names: it takes no --path"
    [ ${#RESTORE_UNITS[@]} -eq 1 ] || die "--fill-from needs the unit its list is for: give --unit U (once)"
  fi
  if [ ${#RESTORE_PATHS[@]} -gt 0 ]; then
    require_tools "${TOOLS_PATH[@]}"
  else
    require_tools "${TOOLS_RESTORE[@]}"
  fi
  require_key_files
  [ -z "$want" ] || require_uint version "$want"
  if [ -n "$FILL_FROM" ]; then
    do_fill "$dest" "$want" "$scratch"
  else
    do_restore "$dest" "$want" "$scratch"
  fi
  ;;
--list)
  shift
  KEYS=()
  want=""
  while [ $# -gt 0 ]; do
    case "$1" in
    --key | --unit | --version)
      [ $# -ge 2 ] || die "$1 needs a value

       usage: $0 --list --key KEYFILE [--unit U] [--version N]"
      case "$1" in
      --key) KEYS+=("$2") ;;
      --unit) RESTORE_UNITS+=("$2") ;;
      --version) want=$2 ;;
      esac
      shift 2
      ;;
    *) die "unknown option: $1" ;;
    esac
  done
  [ ${#KEYS[@]} -gt 0 ] || die "usage: $0 --list --key KEYFILE [--unit U] [--version N]"
  require_tools "${TOOLS_LIST[@]}"
  require_key_files
  [ -z "$want" ] || require_uint version "$want"
  do_list "$want"
  ;;
--where)
  shift
  if [ $# -lt 1 ] || [ "$1" = --key ]; then
    die "--where needs a unit name, or --all for every unit the index names

       usage: $0 --where UNIT|--all --key KEYFILE [--key K2 ...]"
  fi
  where_unit=$1
  shift
  KEYS=()
  while [ $# -gt 0 ]; do
    case "$1" in
    --key)
      [ $# -ge 2 ] || die "--key needs a value

       usage: $0 --where UNIT|--all --key KEYFILE [--key K2 ...]"
      KEYS+=("$2")
      shift 2
      ;;
    *) die "unknown option: $1" ;;
    esac
  done
  [ ${#KEYS[@]} -gt 0 ] || die "usage: $0 --where UNIT|--all --key KEYFILE [--key K2 ...]"
  require_tools "${TOOLS_INDEX[@]}"
  require_key_files
  do_where "$where_unit"
  ;;
--find)
  shift
  if [ $# -lt 1 ] || [ "$1" = --key ]; then
    die "--find needs the text to look for

       usage: $0 --find TEXT --key KEYFILE [--key K2 ...]"
  fi
  [ -n "$1" ] || die "--find needs the text to look for: an empty text matches every path"
  find_text=$1
  shift
  KEYS=()
  while [ $# -gt 0 ]; do
    case "$1" in
    --key)
      [ $# -ge 2 ] || die "--key needs a value

       usage: $0 --find TEXT --key KEYFILE [--key K2 ...]"
      KEYS+=("$2")
      shift 2
      ;;
    *) die "unknown option: $1" ;;
    esac
  done
  [ ${#KEYS[@]} -gt 0 ] || die "usage: $0 --find TEXT --key KEYFILE [--key K2 ...]"
  require_tools "${TOOLS_INDEX[@]}"
  require_key_files
  do_find "$find_text"
  ;;
--help | -h)
  echo "RESTORE.sh — Emergency restore for tapectl volume $LABEL (layout v2)"
  echo ""
  echo "Usage:"
  echo "  $0 --info                                       Show tape layout + seal verdict"
  echo "  $0 --verify [--slices-from DIR ...] [--swap-tapes]   Keyless integrity check"
  echo "  $0 --find-envelope --key KEYFILE [--key K2 ...]  Decrypt your envelope"
  echo "  $0 --restore --key KEYFILE [--key K2 ...] --to DIR"
  echo "               [--unit U [--unit U2 ...] | --all] [--version N]"
  echo "               [--no-space-check] [--overwrite] [--no-file-check] [--slices-from DIR ...] [--swap-tapes]"
  echo "      Full restore. Without --version the NEWEST version of the unit on"
  echo "      this volume is restored. To see which versions this tape holds,"
  echo "      run --find-envelope --key KEYFILE: snapshot_version lives in the"
  echo "      encrypted envelope manifest, so --info cannot report it."
  echo "      --all restores every unit of the envelope (--unit repeated, the"
  echo "      ones named), in one pass along the tape, each into --to/UNIT."
  echo "  $0 --restore --key KEYFILE --to DIR --unit U --path P [--path P2 ...]"
  echo "               [--version N] [--scratch DIR]"
  echo "      Restore only P (a file or folder, as --list prints it), reading"
  echo "      just the slices that hold it and the last. Those wait in a scratch"
  echo "      directory inside --to, or in --scratch DIR."
  echo "  $0 --list --key KEYFILE [--unit U] [--version N]"
  echo "      A unit's files, from the dar catalogue in the envelope: no slice"
  echo "      is read."
  echo "  $0 --where UNIT --key KEYFILE [--key K2 ...]"
  echo "      Which tapes hold UNIT: every version of it in the cross-volume index"
  echo "      this tape carries, with its slices and bytes and each tape holding it"
  echo "      (label, volume uuid, cartridge serial, generation, state, when it"
  echo "      sealed). The operator or escrow key reads the operator envelope's"
  echo "      map of every tenant's units; a tenant's key reads that tenant's."
  echo "  $0 --where --all --key KEYFILE [--key K2 ...]"
  echo "      EVERY unit the index names, with no limit: for each unit its newest"
  echo "      version (the highest number, per unit uuid), whether this tape holds"
  echo "      it, and every version with each tape that holds it (label, volume"
  echo "      uuid, cartridge serial, generation, state). The units whose newest"
  echo "      version is not on this tape are named first. It reads the same"
  echo "      envelopes, with the same keys, as --where UNIT. Exit 1 when the tape"
  echo "      has no index."
  echo "  $0 --find TEXT --key KEYFILE [--key K2 ...]"
  echo "      Which unit, version and tape hold a file: every path in each tenant"
  echo "      envelope's index that contains TEXT (the characters as they stand,"
  echo "      not a pattern; capitals matter), with unit, version, kind, size, and"
  echo "      then the tapes holding that version. At most 50 matches are printed,"
  echo "      with a count of the rest. A TAB, newline or carriage return in a path"
  printf '%s\n' '      is shown as \t, \n or \r.'
  echo "      An index is as of the moment its own tape was written, so the newest"
  echo "      tape you hold has the most complete one. A tape written before"
  echo "      version 1.3.0 has none, and these modes say so. Both exit 1 when"
  echo "      nothing matched or the tape has no index."
  echo ""
  echo "  --key may be repeated, and each key is tried on its own for the"
  echo "      envelope and for every slice: a key rotation between staging a"
  echo "      unit and writing the volume seals the two to different key"
  echo "      generations, so no single key opens both."
  echo ""
  echo "  Disk space: each slice streams from the tape through age into dar,"
  echo "      so --to needs room for the restored files only, about the unit's"
  echo "      size (counted from the slices' encrypted size on tape: a compressed"
  echo "      unit may need more). The script checks before it reads any slice. Where this"
  echo "      dar cannot stream a unit safely (no catalogue in the envelope and"
  echo "      dar older than 2.7.21, or dar older than 2.7.9) its slices are"
  echo "      decrypted to disk first, in --scratch DIR (default: inside --to)."
  echo "  A dar built with GPG support needs gpg (package gnupg) to start."
  echo "  --slices-from DIR   (--restore, --verify; repeat it for more directories) A directory of"
  echo "      slice files copied off another tape that holds a copy of the unit, under any names."
  echo "      When a slice of this tape fails its sha256 or will not read, a file in DIR is used in"
  echo "      its place only if the sha256 of its first bytes is the one this tape's front index"
  echo "      lists for that slice: never by position or by name. A copy made with dd, still"
  echo "      padded to the tape block, will do. A file of the right size but another hash is"
  echo "      named and not used."
  echo "  --swap-tapes   (--restore, --verify) When slices are still missing after this tape has"
  echo "      been read, say which (their number and sha256), wait for ENTER with another cartridge"
  echo "      in the drive, and read just those slices from it: any tape whose front index lists the"
  echo "      same sha256 values. Each tape is read forward from one rewind. Type done and ENTER to"
  echo "      go on without them. Needs the input to be a terminal, or a line of input per swap."
  echo "      Either option restores the way an older dar does, a slice at a time onto disk, so --to"
  echo "      needs about twice the unit's size (--scratch DIR puts the slices on another disk)."
  echo "      Without them a slice that fails stops the restore, and says so."
  echo "  A restore in which dar names files it could not restore (a failed CRC, a slice it was"
  echo "      not given) writes their paths, one per line, to .tapectl-damaged-files.txt in the"
  echo "      unit's directory, restores the rest, and ends RESTORE INCOMPLETE with exit status 1."
  echo "  $0 --restore --key KEYFILE --to DIR --unit U --fill-from LIST"
  echo "      With another tape that holds the SAME unit AT THE SAME VERSION: restore only the"
  echo "      files LIST names, over their damaged copies, and nothing else in the directory. The"
  echo "      first line of the list records the unit, its uuid and its version, and a tape that"
  echo "      lists another is refused; no other version is ever used. dar checks each file, and so"
  echo "      does the sha256 check below. The list is rewritten with what is still damaged and"
  echo "      removed when nothing is. Give --to the same directory as the restore (the list is in"
  echo "      it, or in --to/UNIT)."
  echo "  --no-file-check   (--restore, with --fill-from too) dar checks each file it restores with a"
  echo "      short checksum, so a damaged file can come back with no error. A tape written since"
  echo "      version 1.3.0 records the sha256 of every regular file in its index, and after each"
  echo "      restore and fill this script reads the restored files back and checks them against it"
  echo "      (with --path, only the paths restored). A file that does not match, or is missing, goes"
  echo "      on the damaged-file list like one dar named, so --fill-from can take it from another"
  echo "      copy, and the run ends RESTORE INCOMPLETE. The cost is one read of the restored files"
  echo "      from disk; the tape is not read again. --no-file-check skips it. A file with no sha256"
  echo "      recorded is not checked, and a tape written before 1.3.0 has no index: both are said."
  echo "  --no-space-check   Skip that check (a compressed or thin-provisioned"
  echo "      filesystem can hold more than df reports)."
  echo "  --overwrite   --to must otherwise be empty or new: dar keeps a file"
  echo "      that is already there and reports success. --overwrite replaces"
  echo "      such files, to finish a restore of the same unit that stopped."
  echo "  A tape written since version 1.6.0 may end early, at the end of its medium: its seal"
  echo "      marker, the last file, says where (ended_after). --info says so, --verify prints"
  echo "      NOT ON TAPE for each planned file after it (not a failure), and a unit with a slice"
  echo "      after it is not on that tape whole: --restore refuses it by name (--all restores the"
  echo "      rest and names it), --list says so, and --where marks that tape CUT for it."
  echo ""
  echo "Environment:"
  echo "  TAPE_DEVICE   Tape device path (default: /dev/nst0)"
  echo "  TMPDIR        Where the work directory is made (default: /tmp). It holds the"
  echo "      decrypted envelopes (the operator envelope's catalog.db, the dar"
  echo "      catalogues), which grow with the number of files on the tape; --all"
  echo "      keeps every envelope it opens. No data slice goes there."
  echo ""
  echo "Requirements: mt (mt-st), dd, age, dar, sha256sum, head, truncate, tar, mkfifo, tee"
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
