#!/usr/bin/env bash
# mhvtl verify gate — tapectl's release-verify analog (renovation ticket #7).
#
# Five legs over a real mhvtl tape, driven through the tapectl BINARY:
#   1. tapectl round trip: init → tenants → units → snapshot → stage →
#      volume init/write/verify → restore → diff -r
#   2. Heir leg (no tapectl, no DB): dd RESTORE.sh off the tape and run it with the real age,
#      tar, mt, dd and sha256sum. Layout 3's script has these modes (T4.16; tests/heir_restore.rs
#      is its own suite): --info, --verify, --find-envelope, --list, --restore (a whole unit,
#      --unit/--all), --restore --path, --restore --paths-from (the one repair, from a second
#      cartridge), --where, --find; and, for a unit that continues on another cartridge, one
#      prompt (ENTER: loaded; done or end of input: "I do not have it"; quit). A unit stored with
#      compression is read with the real gzip, xz or zstd. heir_restore_tmpdir (#561) restores
#      with TMPDIR a directory the script cannot write: the work directory is made in --to.
#   3. Negative leg: cross-tenant decrypt must fail; raw media must not
#      contain plaintext canaries or a plaintext tar stream
#   4. Evidence leg: verify must leave a verification_sessions row (ADR-0001)
#   5. Interrupt + resume (issue #93): interrupt a real write three ways and
#      prove `volume resume` finishes each one — the recovery command that
#      would otherwise never be rehearsed before it is needed. RUNS LAST: it
#      erases the tape legs 1-4 wrote.
#   Journals leg (issue #319): catalog-only checks that the forensics
#      journals (mam_journal, log_page_journal) captured every read, once,
#      verbatim, attributed to its contact. Runs after leg 5, reads no tape.
#      Includes tape_alert_surfaced (issue #340): `report health` shows a
#      seeded non-zero TapeAlert on a COPY of the catalog, and none on the
#      real one.
#   Span leg (1.7.0, #478; ADR-0012 item 136; Layout 3: T4.16): a unit across tapes, in its
#      own homes and on its own cartridges, swapped with mtx; RESTORE.sh's
#      prompts and tapectl's (on a pseudo-terminal) answered by
#      scripts/mhvtl-prompt-driver.py. TAPECTL_GATE_SWAP_WAIT (seconds,
#      default 300) is how long the drive stays empty at the swap of span_heir_stream_swap.
#   LBP legs (1.7.2, #388; ADR-0012 items 162-166, 174): the gate's TD8 drive speaks CRC32C
#      and mhvtl cannot carry tapectl's 524,292-byte wire block, so every open falls back
#      (item 165): lbp_fallback_recorded proves the write records it, warned once, and
#      lbp_page_clear_after_every_command proves the page reads method 0 after every step.
#      rust_lbp runs tests/mhvtl_lbp.rs on a TD6 drive (Reed-Solomon), named by
#      TAPECTL_GATE_TAPE_TD6, at a 262,144-byte payload mhvtl can carry.
#
# The `check` lines below are the list, and GATE_EXPECTED_CHECKS (just before the verdict) names
# every one: the verdict judges each by NAME against it, never by a total (#471), a name that did
# not run is a FAIL and so is a check nobody listed. Layout 3 (T4.16) dropped heir_big_catalogue,
# heir_late_damage and heir_slice_boundary_read (they proved dar's stream), renamed
# heir_slice_from_copy to heir_paths_from_copy and heir_restore_symlink_unit to
# heir_restore_symlinks, and added heir_verify, heir_list, heir_path, heir_find_path,
# heir_long_member, span_heir_start_later, span_heir_second_run, span_heir_not_have and
# heir_{gzip,xz,zstd}_{restore,path}: 78 checks. The open issues before 2.0.0's tag added
# heir_restore_tmpdir (#561): 79. #400 is not exercisable on mhvtl, see its section.
#
# The tools it names: tar (GNU tar 1.35, built with ACL support, is the writer's floor; the
# config it writes names [tar] binary, never [dar]), age, gzip, xz, zstd (the heir's real
# decompressors), mt, dd, sha256sum. The summary prints each one's version for the run record.
#
# EXPECTED_FAIL manifest: checks named there MUST fail (they pin known,
# ticketed defects). The gate exits non-zero on any unexpected failure OR any
# unexpected pass — the list may only shrink, and shrinking it is a deliberate
# edit in the same commit as the fix. lcsas skip-rot-floor analog.
#
# Devices are DISCOVERED, never hardcoded (issue #67): SCSI enumeration
# shuffles across mhvtl reloads. Media is chosen by generation suffix to match
# the drive (an L6 tape for a TD6 drive).
set -uo pipefail

# No default device (2026-09-23): the real HP LTO-6 is now attached at
# /dev/nst0, the old default. The drive under test is always named.
TAPE_DEV="${TAPECTL_GATE_TAPE:-}"
SCRATCH="${TAPECTL_GATE_SCRATCH:-/scratch/tapectl-gate}"
LABEL="MHVTLG"

die() { echo "GATE PRECONDITION FAILED: $*" >&2; exit 2; }

# ---------- preconditions (loud — this gate must never rot quietly) ----------
[ "${TAPECTL_MHVTL:-}" = "1" ] || die "TAPECTL_MHVTL=1 not set"
[ -n "$TAPE_DEV" ] || die "TAPECTL_GATE_TAPE not set — name the mhvtl drive (e.g. /dev/nst1); there is no default"
grep -q '^mhvtl ' /proc/modules \
    || die "mhvtl module not loaded for $(uname -r) — dkms status; see docs/operator-guide.md"
[ -e "$TAPE_DEV" ] || die "$TAPE_DEV missing — systemctl start mhvtl.target"
# tar is the writer (config's [tar] binary) and the heir's reader; gzip, xz and zstd are the
# heir's real decompressors for the heir_{gzip,xz,zstd}_* legs. dar is gone (Layout 3).
for bin in lsscsi mtx mt tar gzip xz zstd age age-keygen sha256sum python3 cargo sg_modes; do
    command -v "$bin" >/dev/null || die "required binary missing: $bin"
done
# The tar tapectl writes with: TAPECTL_GATE_TAR names one, else the first on PATH. tapectl
# refuses a tar below its floor itself (GNU tar 1.35 with ACL support); the gate does not judge it.
GATE_TAR="${TAPECTL_GATE_TAR:-$(command -v tar)}"
[ -x "$GATE_TAR" ] || die "TAPECTL_GATE_TAR=$GATE_TAR is not an executable"
GATE_REPO="$(cd "$(dirname "$0")/.." && pwd)"

# Single-drive rule (#9): one tape user at a time, across processes.
exec 9>/tmp/tapectl-tape.lock
flock -n 9 || die "another process holds the tape lock (/tmp/tapectl-tape.lock)"

# ---------- device discovery + generation-matched media (#67, #111) ----------
# Delegated to scripts/mhvtl-device.sh, which is now the ONE implementation of
# this chain (st node -> lsscsi -> device.conf -> DTE -> changer sg -> media
# generation). It used to live inline here while tests/mhvtl_e2e.rs carried a
# partial second copy with hardcoded device paths. Sets TAPE_DEV, DRIVE_MODEL,
# DRIVE_SG, CHG_SG, DTE, GEN, LOADED_TAG.
DISCOVERY="$("$(dirname "$0")/mhvtl-device.sh" --tape "$TAPE_DEV" --ensure-media)" \
    || die "device discovery failed (see the message above)"
eval "$DISCOVERY"

echo "gate: drive=$TAPE_DEV ($DRIVE_MODEL, sg=$DRIVE_SG) changer=$CHG_SG dte=$DTE tape=$LOADED_TAG"

# ---------- the TD6 drive for rust_lbp (#388) ----------
# Logical Block Protection is proven on an LTO-6 emulation (ULT3580-TD6): mhvtl's TD5 and TD6
# personalities speak Reed-Solomon, the method tapectl arms (ADR-0012 item 162), where the
# gate's TD8 drive speaks CRC32C and falls back on every open. Named like TAPECTL_GATE_TAPE,
# never defaulted; discovery refuses a device that is not mhvtl's, and this a model that is
# not a TD6. TAPECTL_GATE_SPAN_ONLY runs no rust_lbp, so it needs none.
TAPE_TD6="${TAPECTL_GATE_TAPE_TD6:-}"
if [ -z "${TAPECTL_GATE_SPAN_ONLY:-}" ]; then
    [ -n "$TAPE_TD6" ] || die "TAPECTL_GATE_TAPE_TD6 not set — name an mhvtl TD6 drive for rust_lbp: TAPECTL_GATE_TAPE_TD6=\"\$(readlink -f /dev/tape/by-id/scsi-XYZZY_A3-nst)\""
    [ -e "$TAPE_TD6" ] || die "$TAPE_TD6 missing — systemctl start mhvtl.target"
    [ "$(readlink -f "$TAPE_TD6")" != "$(readlink -f "$TAPE_DEV")" ] \
        || die "TAPECTL_GATE_TAPE_TD6 and TAPECTL_GATE_TAPE name the same drive"
    TD6_DISCOVERY="$("$(dirname "$0")/mhvtl-device.sh" --tape "$TAPE_TD6")" \
        || die "TD6 device discovery failed (see the message above)"
    TD6_MODEL="$(sed -n "s/^DRIVE_MODEL='\(.*\)'\$/\1/p" <<<"$TD6_DISCOVERY")"
    TD6_SG="$(sed -n "s/^DRIVE_SG='\(.*\)'\$/\1/p" <<<"$TD6_DISCOVERY")"
    case "$TD6_MODEL" in
        *TD6*) ;;
        *) die "TAPECTL_GATE_TAPE_TD6=$TAPE_TD6 is a ${TD6_MODEL:-drive of unknown model}, not a TD6 emulation: rust_lbp needs Reed-Solomon LBP" ;;
    esac
    # The rehearsals take a drive's own lock (/tmp/tapectl-tape.lock.<node>), not the global one
    # this gate holds: take the TD6 drive's too, so rust_lbp never meets a rehearsal on it.
    exec 8>"/tmp/tapectl-tape.lock.$(basename "$(readlink -f "$TAPE_TD6")")"
    flock -n 8 || die "another process holds $TAPE_TD6's tape lock (/tmp/tapectl-tape.lock.$(basename "$(readlink -f "$TAPE_TD6")"))"
    export TAPECTL_GATE_TAPE_TD6="$TAPE_TD6"
    echo "gate: LBP drive=$TAPE_TD6 ($TD6_MODEL, sg=$TD6_SG)"
fi

# ---------- workspace + build ----------
RUN="$SCRATCH/run-$(date +%Y%m%d-%H%M%S)"
mkdir -p "$RUN"
echo "gate: workspace $RUN"
# The build lock is shared with every other cargo invocation on this VM
# (worktree-agent.md, "Build lock"): the box is 9 GB, and two concurrent
# links OOM-kill each other. This script gets its own CARGO_TARGET_DIR
# above, which keeps cargo's own per-directory lock from serializing it
# against a worker — but that is exactly what makes the memory collision
# possible, so the flock is not optional here either.
#
# -w, not a bare wait: if a CALLER already wrapped this script in
# `flock /scratch/tapectl-build.lock`, this line would wait forever on a lock
# its own ancestor holds — flock locks are per-open-file-description, with no
# reentrancy for a child process. That happened on 2026-09-16 and hung for 13
# minutes looking exactly like a slow build. Fail with the cause named instead.
# A real worker link is minutes, not twenty, so the timeout cannot fire on
# honest contention.
# -E 99 gives lock-conflict its own exit code, so a timeout is never confused
# with a compile failure (cargo exits 101, and a bare `-w` would report 1,
# which cargo can also return).
flock -w 1200 -E 99 /scratch/tapectl-build.lock cargo build --quiet
build_rc=$?
if [ "$build_rc" -eq 99 ]; then
    die "timed out waiting for /scratch/tapectl-build.lock.
   If you ran this script inside an outer 'flock /scratch/tapectl-build.lock',
   that is the cause: run it bare — the script takes the lock itself."
elif [ "$build_rc" -ne 0 ]; then
    die "cargo build failed"
fi
BIN="${CARGO_TARGET_DIR:-target}/debug/tapectl"
[ -x "$BIN" ] || die "built binary not found at $BIN"

HOME_DIR="$RUN/home"; mkdir -p "$HOME_DIR"
CFG="$HOME_DIR/config.toml"
# --home, not --config (issue #109). `--config` alone still relocates the
# whole home to the config file's parent — which is how this gate used to get
# an isolated ~/.tapectl — but that is now the deprecated, warning path. The
# gate says what it means: HOME_DIR is the archive, CFG is the file in it.
TCTL() { "$BIN" --home "$HOME_DIR" --config "$CFG" "$@"; }

# ---------- check harness ----------
# TAPECTL_GATE_SPAN_ONLY=1 is a switch for iterating on the span leg (#478): only the span_*
# checks run, after the discovery and the build above; every other check is skipped and not
# judged, and the verdict says PARTIAL and exits non-zero, never GREEN. Unset, it changes nothing.
GATE_SPAN_ONLY="${TAPECTL_GATE_SPAN_ONLY:-}"
declare -A RESULT
CHECKS=()
check() { # check <name> <fn>
    local name="$1"; shift
    if [ -n "$GATE_SPAN_ONLY" ] && [[ "$name" != span_* ]]; then return 0; fi
    CHECKS+=("$name")
    if "$@" >"$RUN/log-$name.txt" 2>&1; then RESULT[$name]=PASS; else RESULT[$name]=FAIL; fi
    lbp_note_page "$name"
    echo "  [$name] ${RESULT[$name]}"
}

# ---------- the drive's Logical Block Protection page (#388) ----------
# tapectl arms the Control Data Protection page (0Ah/F0h) for its own open of the drive and
# clears it before it closes the drive (ADR-0012 items 163-164); on this gate's TD8 drive every
# open falls back (item 165) and puts the page back as found. After EVERY step the gate reads
# the page and appends it to $LBP_PAGES; lbp_page_clear_after_every_command judges the rows.
# A MODE SENSE through sg3-utils: read-only, never a motion, never the st node.
#
# lbp_page_parse: the page's method, information length and flags (LBP_W 0x80, LBP_R 0x40)
# from a MODE SENSE(10) answer on stdin, "method=01 info_len=04 flags=c0", found past the header
# and the block descriptors it declares, as tape::lbp parses it. An answer holding no Control
# Data Protection page is an error, never method 0. tests/mhvtl_lbp.rs runs this function
# ungated on recorded answers, against the library's parser.
LBP_PAGES="$RUN/lbp-page-after-step.tsv"
lbp_page_parse() {
    python3 -c '
import sys
r = sys.stdin.buffer.read()
if len(r) < 8:
    sys.exit("a MODE SENSE(10) answer of %d bytes" % len(r))
p = r[8 + int.from_bytes(r[6:8], "big"):]
if len(p) < 8 or p[0] & 0x3F != 0x0A or p[1] != 0xF0:
    sys.exit("no Control Data Protection page (0Ah/F0h) in %s" % r.hex())
print("method=%02x info_len=%02x flags=%02x" % (p[4], p[5], p[6] & 0xC0))
'
}
lbp_page_state() { # lbp_page_state <sg>: the page now, through lbp_page_parse
    local raw
    raw="$(mktemp)" || return 1
    sg_modes --page=0x0a,0xf0 --control=0 --raw "$1" >"$raw" || { rm -f "$raw"; return 1; }
    lbp_page_parse <"$raw"
    local rc=$?
    rm -f "$raw"
    return $rc
}
lbp_note_page() { # lbp_note_page <step>: one row, "<step>\t<state>", or why it could not be read
    local state
    state="$(lbp_page_state "$DRIVE_SG" 2>&1)" || state="unreadable: ${state//$'\n'/ }"
    printf '%s\t%s\n' "$1" "$state" >>"$LBP_PAGES"
}
# EMPTY as of 2026-07-29 — every check below must now PASS. Do not add an
# entry here to make a red gate green; a new failure is a regression to fix,
# and the array may only grow via a deliberate, ticketed decision.
#
# History of what used to be pinned here:
#   H1 fixed in #24: the mini-index is generated from the complete Layout, so
#     it now lists the envelopes and the no-tapectl heir path works end-to-end
#     (find-envelope + full restore, byte-identical).
#   H8 fixed in #34: list_slices parses the numeric slice index instead of
#     sorting filenames lexicographically, so slice_number no longer permutes
#     at >=10 slices. Verified on tape: unitB staged a clean 1..=12 run and
#     restore_multislice_unit's `diff -r` came back byte-identical.
#   H7 fixed in #33: the directory walk records each entry's file type, and
#     content validation (size + sha256) applies to regular files only, so a
#     symlink no longer false-DIRTYs (lstat target-string length vs the
#     followed target's size) and a FIFO can no longer block staging forever.
EXPECTED_FAIL=()

# ---------- fixtures ----------
CANARY="CANARY_tapectl_gate_$(date +%s)"
SRC="$RUN/src"; mkdir -p "$SRC/unitA/nested" "$SRC/unitB" "$SRC/unitC"
echo "alpha content" > "$SRC/unitA/plain.txt"
echo "$CANARY payload" > "$SRC/unitA/${CANARY}.txt"
: > "$SRC/unitA/empty.bin"
head -c 700000 /dev/urandom > "$SRC/unitA/big-block.bin"
echo "nested" > "$SRC/unitA/nested/déjà-vu.txt"
# A hard link: two names, one file (the second name is an h member of the tar; heir_restore checks
# the restored names are one file, heir_list that the table lists one f and one h).
echo "hard-linked" > "$SRC/unitA/hl-a.txt"
ln "$SRC/unitA/hl-a.txt" "$SRC/unitA/hl-b.txt"
# unitB (bob's): three files under a slice (700000 bytes each: a slice ends at a member's start, so
# each is the only file of its own slice, s1 with the dotfile) and one file larger than a slice
# (12000000 bytes at slice_size 1M: a run of 13 slices to itself). So a bad slice costs exactly one
# small file (heir_paths_from_copy), a bad piece of the run costs the long file (heir_long_member),
# and a path is read from one slice (heir_path). Slice numbers are asked of --list, never assumed.
mkdir -p "$SRC/unitB/set"
head -c 700000 /dev/urandom > "$SRC/unitB/set/s1.bin"
head -c 700000 /dev/urandom > "$SRC/unitB/set/s2.bin"
head -c 700000 /dev/urandom > "$SRC/unitB/set/s3.bin"
head -c 12000000 /dev/urandom > "$SRC/unitB/twelve-meg.bin"
echo "target" > "$SRC/unitC/target.txt"
ln -s target.txt "$SRC/unitC/link-ok"
ln -s /nonexistent-gate-path "$SRC/unitC/link-broken"

# Rows of an SQL query on a catalog, read-only, tab-separated (the span leg's sp_q is this).
gate_q() { # gate_q <db> <sql> [param...]
    python3 - "$@" <<'PY'
import sqlite3, sys
c = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
for row in c.execute(sys.argv[2], sys.argv[3:]):
    print("\t".join("" if v is None else str(v) for v in row))
PY
}
# Appends a [policy] table to a unit's dotfile (`unit init` writes none): one line each argument,
# e.g. 'compression = "xz"'. The dotfile layer of the 3-level resolver (dotfile > archive set >
# [defaults]), the one an operator sets.
gate_unit_policy() { # gate_unit_policy <unit dir> <"key = value">...
    local d="$1" line
    shift
    grep -E '^\[policy\]' "$d/.tapectl-unit.toml" >/dev/null && { echo "gate_unit_policy: $d already has a [policy] table"; return 1; }
    { printf '\n[policy]\n'; for line in "$@"; do printf '%s\n' "$line"; done; } >>"$d/.tapectl-unit.toml"
}
# The tape file a unit's slice <n> is at on <label>, from the catalog the write recorded: what a
# heir leg injects a fault at or expects a read of. Prints nothing when there is no such slice.
gate_slice_pos() { # gate_slice_pos <db> <label> <unit> <n>
    gate_q "$1" "SELECT CAST(wp.position AS INTEGER) FROM write_positions wp
        JOIN writes w ON w.id = wp.write_id JOIN volumes v ON v.id = w.volume_id
        JOIN stage_slices sl ON sl.id = wp.stage_slice_id JOIN stage_sets ss ON ss.id = sl.stage_set_id
        JOIN snapshots s ON s.id = ss.snapshot_id JOIN units u ON u.id = s.unit_id
        WHERE v.label = ? AND u.name = ? AND sl.slice_number = ? AND wp.status IN ('written', 'verified')" "$2" "$3" "$4"
}

# ---------- leg 1: tapectl round trip ----------
# Each command's status is checked (issue #415): this step's verdict used to
# be `mkdir`'s alone, so a failed `init` or config rewrite still read PASS.
step_init() {
    TCTL init --operator gate-op --no-escrow || return 1
    python3 - "$CFG" "$RUN" "$TAPE_DEV" "$DRIVE_SG" "$GATE_TAR" <<'PY' || return 1
import sys, re
cfg, run, tape, sg, tar = sys.argv[1:6]
t = open(cfg).read()
# [tar] binary: the first `binary =` line init writes is the [tar] table's (there is no [dar]).
t = re.sub(r'(?m)^binary *=.*$', f'binary = "{tar}"', t, count=1)
t = re.sub(r'(?m)^slice_size *=.*$', 'slice_size = "1M"', t, count=1)
t = re.sub(r'(?m)^directory *=.*$', f'directory = "{run}/staging"', t, count=1)
t = re.sub(r'(?m)^device_tape *=.*$', f'device_tape = "{tape}"', t)
# device_sg is deliberately NOT written (issue #431): the gate proves the sg node
# is derived from device_tape. `sg` stays on the argv for its callers.
_ = sg
# Must match an UNCOMMENTED table header: `tapectl init` writes a commented
# [[backends.lto]] example (#124b), and a substring test sees that and wrongly
# concludes a backend is already configured — leaving the gate with no drive.
if not re.search(r"(?m)^\[\[backends\.lto\]\]", t):
    # `init` writes an empty backends.lto (audit shell-MED); the gate supplies one.
    t = re.sub(r'(?m)^lto *= *\[\] *\n', '', t)  # drop the inline empty array first
    t += f'''
[[backends.lto]]
name = "gate-mhvtl"
device_tape = "{tape}"
generation = "LTO-8"
# 2 748 779 069 440 = 2.5 TiB exactly, written as a bare byte count on
# purpose (issue #200). This string used to be "2.5T", parsed BINARY by
# volume init and DECIMAL by config validation; #200 made init decimal
# too, which would have shrunk this microcosm by 9.95%. Fixing a parser
# drift and resizing the test microcosm are two different changes, and
# doing both at once means a red gate cannot be attributed to either.
# A bare integer is the one literal both parsers read identically.
capacity_override = "2748779069440"
fill_ceiling = 0.95
enospc_buffer = "2G"
'''
open(cfg, 'w').write(t)
PY
    mkdir -p "$RUN/staging" || return 1
    # The config names [tar], and no [dar]: the tar line is the first `binary =` in the file.
    grep -E '^\[tar\]$' "$CFG" >/dev/null || { echo "the config has no [tar] table"; return 1; }
    if grep -E '^\[dar\]' "$CFG" >/dev/null; then echo "the config has a [dar] table"; return 1; fi
    grep -F "binary = \"$GATE_TAR\"" "$CFG" >/dev/null || { echo "[tar] binary is not $GATE_TAR"; return 1; }
    TCTL config check
}
step_tenants() {
    TCTL tenant add alice && TCTL tenant add bob \
    && step_escrow
}

# ADR-0005: a permanent escrow recipient participates in every encryption, and
# pre-write validation REFUSES without one — so this is a precondition of any
# volume write, not optional setup. `key generate --escrow` prints the secret
# once for paper transcription; here it lands in the throwaway gate log, which
# is fine for a disposable test identity under /scratch.
step_escrow() { TCTL key generate --escrow; }
step_units() {
    TCTL unit init "$SRC/unitA" --tenant alice --name unitA \
    && TCTL unit init "$SRC/unitB" --tenant bob --name unitB \
    && TCTL unit init "$SRC/unitC" --tenant alice --name unitC
}
step_snapshots() { TCTL snapshot create unitA && TCTL snapshot create unitB && TCTL snapshot create unitC; }
step_stage_main() { TCTL stage create unitA && TCTL stage create unitB; }
step_stage_symlinks() { TCTL stage create unitC; }
# --force (issue #27's contact-discipline check): this gate reuses whatever
# generation-matching cartridge `mtx` finds already loaded/in the library
# (see the device-discovery block above) and never erases it between runs,
# so a rerun against the same physical tape finds File 0 already carrying
# the previous run's identity under "$LABEL" (same label, different uuid --
# `volume_init` always generates a fresh one).
#
# IMPORTANT: --force only rescues this on a run whose target cartridge was
# NEVER sealed (a fresh/blank tape, or a leftover from an aborted prior
# run). check_tape_contact (session.rs) deliberately makes AlreadySealed
# un-overridable (ADR-0003) by probing a foreign tape's own self-reported
# seal position on an identity mismatch -- so a SECOND gate run against a
# tape the FIRST run actually sealed will hit AlreadySealed, which --force
# cannot defeat, and this step will fail. That is correct, expected
# behavior, not a bug: the sanctioned path past a sealed cartridge is a
# real erase (e.g. `mt -f "$TAPE_DEV" erase`), not a wider override, and
# this script does not currently perform one. NOT exercised in the change
# that added this flag (guardrail: no tape device access) -- the
# coordinator should expect the gate to need a freshly-erased (or
# never-yet-sealed) cartridge on the first run after #27, and add an
# explicit erase step here if repeat runs against the same media are
# wanted.
# Long-erase the scratch cartridge first — the gate reuses one cartridge across
# runs, so from the second run onward it carries a SEALED volume and contact
# discipline (#27) correctly refuses to overwrite it. `--force` cannot defeat
# AlreadySealed by design (ADR-0003), so the honest fix is a real erase, which
# mirrors the production reuse procedure (retire, erase in the drive,
# mark-erased; never a degausser, #417) and
# is instant on mhvtl. Erasing lets the gate exercise the DEFAULT no-force
# path, which is the one an operator actually runs.
step_erase_scratch_tape() {
    mt -f "$TAPE_DEV" rewind && mt -f "$TAPE_DEV" erase
}
step_vol_init() { TCTL volume init "$LABEL" --device "$TAPE_DEV"; }
# Every `volume write` here passes --yes: it answers the quiet-host pre-flight
# (ADR-0012, 2026-09-24 amendment, item 7), which on this VM nearly always has
# a finding (another worker's cargo), and which a non-interactive run without
# --yes declines as an ADR-0008 Tier-2 question. The findings still print
# ("host check: ..." lines, never "warning: volume"). --yes reaches no other
# question on `volume write`, and no Tier-3 refusal.
step_vol_write() { TCTL volume write "$LABEL" --device "$TAPE_DEV" --yes; }
# `volume verify --json`, judged on its EXIT STATUS as well as its report
# (issue #415). Every verify step used to be `verify --json | tee` followed by
# a python assert on the JSON, and nothing read the verify's own status: a
# non-zero verify (2 = medium proven bad, 3 = inconclusive, which every error
# exits) passed whenever the JSON it printed still said failed == 0. The exit
# is checked first, then the report. PIPESTATUS is read by the very next
# statement -- any command in between, `local` included, replaces it. tee is
# kept so the JSON still lands in the step log.
verify_json() { # verify_json <label> <device> <json-out>
    local label="$1" dev="$2" json="$3"
    local -a st
    TCTL volume verify "$label" --device "$dev" --json | tee "$json"
    st=("${PIPESTATUS[@]}")
    [ "${st[0]}" -eq 0 ] || {
        echo "volume verify $label exited ${st[0]}, want 0 (2 = medium proven bad, 3 = inconclusive)"
        return 1
    }
    [ "${st[1]}" -eq 0 ] || { echo "tee into $json exited ${st[1]}"; return 1; }
    python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); assert d.get("failed",1)==0 and d.get("passed",0)>0, d' "$json"
}
step_vol_verify() { verify_json "$LABEL" "$TAPE_DEV" "$RUN/verify.json"; }
# Issue #277's recording half, which NOTHING else covers. The `sealed_at`
# UPDATE lives in `finish_session`, reachable only through
# `volume write`/`volume resume` against a real device -- so every unit test
# for #277 sets the column by raw SQL and none proves it is ever written.
# Delete that one `conn.execute` and the whole Rust suite stays green while
# `volume resume` silently returns to re-sealing already-sealed cartridges.
# That is the revert-silent shape (issue #284) applied to a data-loss guard,
# so the guard gets an on-media check rather than an argument.
#
# The row lookup is its own positive control: `fetchone()` returning None
# means the volume is not in the catalog at all, which fails here rather
# than passing vacuously the way `assert row[0] is not None` on a missing
# row would.
step_sealed_at() {
    LABEL="$LABEL" python3 - "$HOME_DIR/tapectl.db" <<'PY'
import os, sqlite3, sys
label = os.environ["LABEL"]
row = sqlite3.connect(sys.argv[1]).execute(
    "SELECT sealed_at FROM volumes WHERE label = ?", (label,)
).fetchone()
assert row is not None, f"no volumes row for {label} -- the check cannot see what it is asserting about"
assert row[0] is not None, (
    f"volumes.sealed_at is NULL for {label} after a completed sealed write -- "
    "finish_session did not record the seal (issue #277)"
)
print(f"{label}: sealed_at = {row[0]}")
PY
}
# #388, ADR-0012 item 165: a drive that cannot take Logical Block Protection is warned about
# once and written without it, and the catalog says so (migration 041: volumes.write_lbp,
# write_lbp_note; a write_lbp event). On this gate's TD8 drive every write falls back: mhvtl
# cannot carry the 524,292-byte wire block in one command (cause 8: st answers the open's probe
# read EBUSY; cause 7 if st refuses the block size outright), or its CRC32C personality refuses
# Reed-Solomon at the select (cause 5) or does not take it (cause 6). Read from the catalog, not `volume info --json`, so the leg judges
# what the session recorded. The warning is counted in the write's own log as its plain
# stderr line: once per process per drive (lbp::note_unavailable). R12: no other leg asserts
# an empty stderr, and step_feed_ratio_recorded's needle ("warning: volume") does not match it.
LBP_WARNING="warning: Logical Block Protection is not available on "
step_lbp_fallback_recorded() {
    LABEL="$LABEL" python3 - "$HOME_DIR/tapectl.db" <<'PY' || return 1
import os, sqlite3, sys
label = os.environ["LABEL"]
c = sqlite3.connect(sys.argv[1])
row = c.execute("SELECT write_lbp, write_lbp_note FROM volumes WHERE label = ?", (label,)).fetchone()
assert row is not None, f"no volumes row for {label} -- the check cannot see what it is asserting about"
lbp, note = row
assert lbp == "off", (
    f"volumes.write_lbp is {lbp!r} for {label}: on the gate's TD8 drive every open falls back, "
    "so the write must record 'off' (NULL: the session recorded nothing)")
causes = {
    "8 (st cannot carry the wire block)": "st cannot carry a 524292-byte block",
    "7 (st refused the wire block size)": "st refused a 524292-byte block size",
    "5 (Reed-Solomon refused)": "the drive refused Reed-Solomon LBP (method 01h)",
    "6 (the page did not take)": "the drive's page did not take",
}
named = [k for k, text in causes.items() if note and text in note]
assert named, f"write_lbp_note {note!r} names none of the fallback causes the gate's drive can meet"
events = c.execute(
    """SELECT e.new_value, e.details FROM events e JOIN volumes v ON v.id = e.entity_id
       WHERE e.entity_type = 'volume' AND e.action = 'write_lbp' AND v.label = ?""",
    (label,)).fetchall()
assert events == [("off", note)], (
    f"write_lbp events for {label}: {events}; want exactly one, 'off' with the note "
    "(record_run_protection writes one per change)")
print(f"{label}: write_lbp = 'off', cause {named[0]}: {note}")
print(f"{label}: one write_lbp event, 'off'")
PY
    local log="$RUN/log-volume_write.txt" n
    [ -s "$log" ] || { echo "positive control: $log is missing or empty"; return 1; }
    n="$(grep -c "^$LBP_WARNING" "$log")"
    [ "$n" = 1 ] || {
        echo "the write's log holds $n plain '$LBP_WARNING…' line(s), want exactly 1 (item 165: warn once per drive)"
        grep -n "Logical Block Protection" "$log"
        return 1
    }
    grep "^$LBP_WARNING" "$log"
}
step_evidence() {
    python3 - "$HOME_DIR/tapectl.db" <<'PY'
import sqlite3, sys
n = sqlite3.connect(sys.argv[1]).execute("SELECT COUNT(*) FROM verification_sessions").fetchone()[0]
assert n >= 1, f"no verification_sessions rows (got {n})"
PY
}
step_restore_A() {
    TCTL restore unit --unit unitA --from "$LABEL" --to "$RUN/restored-A" --device "$TAPE_DEV" \
    && diff -r "$SRC/unitA" "$RUN/restored-A"
}
step_restore_B() {
    TCTL restore unit --unit unitB --from "$LABEL" --to "$RUN/restored-B" --device "$TAPE_DEV" \
    && diff -r "$SRC/unitB" "$RUN/restored-B"
}
# unitC carries a good symlink and a deliberately broken one. Before #33 it
# could not stage at all, so nothing ever checked that a symlink SURVIVES a
# round trip -- only that staging didn't error.
#
# `--no-dereference` is load-bearing, not a style choice: plain `diff -r`
# FOLLOWS symlinks, so if a symlink were restored as a flattened regular copy
# of its target, plain `diff -r` exits 0 and the check silently cannot fail.
# Demonstrated on diffutils 3.10 before this leg was written. With
# --no-dereference, both flattening and a wrong target exit 1. It also lets
# the broken symlink compare as a symlink instead of erroring on its missing
# target.
step_restore_C() {
    TCTL restore unit --unit unitC --from "$LABEL" --to "$RUN/restored-C" --device "$TAPE_DEV" \
    && diff -r --no-dereference "$SRC/unitC" "$RUN/restored-C"
}

echo "gate: leg 1 — tapectl round trip"
# Issue #338: after every completed `volume write`, the feed ratio (native
# tape consumed per data byte, page 0x0c BOP->EOD over the Layout's on-tape
# bytes) is RECORDED as an `events` row whether or not it warns. On mhvtl
# page 0x0c is a static 500 MB figure, so the ratio is nonsense (~16x) and
# the warning is suppressed by the drive's `capacity_override` -- but the
# row must still be there, with `details.suppressed` saying why, or the
# wiring is proven by nothing the gate runs (a source pin only). Positive
# control first: at least one completed write contact exists.
step_feed_ratio_recorded() {
    # `|| return 1` (issue #415): this python's status was dropped, so the
    # step's verdict was the grep below alone and none of these asserts could
    # fail it.
    python3 - "$HOME_DIR/tapectl.db" <<'PYFEED' || return 1
import json, sqlite3, sys
c = sqlite3.connect(sys.argv[1])
writes = c.execute(
    """SELECT id FROM cartridge_contacts
       WHERE operation = 'volume write' AND outcome = 'ok' AND closed_at IS NOT NULL
       ORDER BY id""").fetchall()
assert writes, "positive control: no completed 'volume write' contact -- nothing to assert a ratio about"
rows = c.execute(
    """SELECT id, entity_id, entity_label, new_value, details FROM events
       WHERE action = 'write_feed_ratio' ORDER BY id""").fetchall()
assert rows, (f"no write_feed_ratio event at all, yet {len(writes)} write contact(s) completed -- "
              "the #338 assessment is not wired (or its journal read-back found no page 0x0c)")
bad = []
by_contact = {}
for rid, vid, label, val, det in rows:
    tag = f"event {rid} (volume {label})"
    try:
        d = json.loads(det or "")
    except Exception as e:
        bad.append(f"{tag}: details is not JSON ({e})"); continue
    for k in ("source_page", "native_bop_to_eod_mb", "data_bytes", "ratio", "threshold", "warned", "contact_id"):
        if k not in d:
            bad.append(f"{tag}: details lacks {k!r}")
    if d.get("source_page") != "0x0c":
        bad.append(f"{tag}: source_page {d.get('source_page')!r}, expected '0x0c'")
    if d.get("warned") is not False:
        bad.append(f"{tag}: warned={d.get('warned')!r} on mhvtl (capacity_override set) -- must be false")
    if d.get("suppressed") != "capacity_override":
        bad.append(f"{tag}: suppressed={d.get('suppressed')!r}, expected 'capacity_override' on the gate's drive")
    if d.get("data_bytes", 0) <= 0:
        bad.append(f"{tag}: data_bytes {d.get('data_bytes')!r} is not positive")
    by_contact.setdefault(d.get("contact_id"), []).append(rid)
for (cid,) in writes:
    n = len(by_contact.get(cid, []))
    if n != 1:
        bad.append(f"write contact {cid}: {n} write_feed_ratio event(s), expected exactly one")
assert not bad, "write_feed_ratio events wrong:\n  " + "\n  ".join(bad)
print(f"{len(rows)} write_feed_ratio event(s) for {len(writes)} completed write contact(s): "
      f"recorded, unwarned, suppressed=capacity_override")
PYFEED
    # No warning may have reached stderr on mhvtl: the suppression is the
    # point. The gate captures every step's output under $RUN/log-*.txt.
    #
    # Issue #415: this was `grep -l ... 2>/dev/null | grep -q .`, an absence
    # check with no positive control. A glob matching nothing made grep exit
    # 2 into /dev/null and read as "absent", and under pipefail a `grep -l`
    # SIGPIPEd by the early-exiting `grep -q` read the same way. So: a
    # positive control first, over the SAME glob with the SAME `grep -l` --
    # the write's own completion line must be found in the write's own log,
    # the stream a feed-ratio warning lands in -- then the negative, judged
    # on grep's own status (the leakscan's rule: 1 is clean, 2+ is
    # inconclusive, never clean).
    #
    # This step's own log is in the glob and is being written while it runs:
    # nothing echoed before the negative grep may contain its needle.
    local logs=("$RUN"/log-*.txt) hits rc
    hits="$(grep -lF "volume \"$LABEL\" write completed" "${logs[@]}")"
    rc=$?
    if [ "$rc" -ne 0 ] || ! grep -xF -- "$RUN/log-volume_write.txt" >/dev/null <<<"$hits"; then
        echo "positive control: grep -l over $RUN/log-*.txt (rc=$rc) did not find the write's completion line in log-volume_write.txt -- the search below could not have found a feed-ratio line either"
        return 1
    fi
    hits="$(grep -lF "warning: volume" "${logs[@]}")"
    rc=$?
    case "$rc" in
        0)
            echo "a 'warning: volume' line reached stderr on mhvtl despite capacity_override, in: $hits" >&2
            grep -H "warning: volume" "${logs[@]}" >&2
            return 1 ;;
        1) ;;
        *)
            echo "grep failed (rc=$rc) searching $RUN/log-*.txt for 'warning: volume' -- inconclusive, not clean" >&2
            return 1 ;;
    esac
    echo "no 'warning: volume' line in any step log (suppressed on the gate's drive); the same search found the write's completion line"
}

# Issue #301: every contact journals the st driver's per-device sysfs
# counters (/sys/class/scsi_tape/<node>/stats/*) verbatim at its open and
# its close, so the I/O one command did is a difference of two rows. mhvtl's
# drives go through the real st driver, so the gate's contacts carry genuine
# readings -- without this step the capture is proven only by unit tests on a
# fake sysfs. Positive control first: closed contacts exist.
step_st_stats_recorded() {
    python3 - "$HOME_DIR/tapectl.db" <<'PYST'
import json, sqlite3, sys
c = sqlite3.connect(sys.argv[1])
contacts = c.execute(
    """SELECT id, operation FROM cartridge_contacts
       WHERE closed_at IS NOT NULL ORDER BY id""").fetchall()
assert contacts, "positive control: no closed contact -- nothing to assert st readings about"
bad, moved = [], 0
for cid, op in contacts:
    rows = c.execute(
        "SELECT point, stats_json, errors_json FROM st_stats_journal WHERE contact_id = ? ORDER BY id",
        (cid,)).fetchall()
    points = [r[0] for r in rows]
    if points != ["open", "close"]:
        bad.append(f"contact {cid} ({op}): readings {points}, expected ['open', 'close']"); continue
    o, cl = (json.loads(r[1]) for r in rows)
    if not o or set(o) != set(cl):
        bad.append(f"contact {cid} ({op}): open/close file sets differ or are empty"); continue
    if any(r[2] for r in rows):
        bad.append(f"contact {cid} ({op}): errors_json set: {[r[2] for r in rows]}")
    for k in ("read_byte_cnt", "write_byte_cnt"):
        if k in o and int(cl[k].strip()) < int(o[k].strip()):
            bad.append(f"contact {cid} ({op}): {k} went backwards ({o[k].strip()} -> {cl[k].strip()})")
    if any(int(cl[k].strip()) > int(o[k].strip()) for k in ("read_byte_cnt", "write_byte_cnt") if k in o):
        moved += 1
assert not bad, "st_stats_journal wrong:\n  " + "\n  ".join(bad)
assert moved > 0, "positive control: no contact's byte counters moved between open and close -- the readings are not measuring I/O"
print(f"{len(contacts)} closed contacts each carry an open and a close st reading; {moved} show bytes moved across the contact")
PYST
}

check init            step_init
check tenants         step_tenants
check units           step_units
check snapshots       step_snapshots
check stage_main      step_stage_main
check stage_symlink_unit step_stage_symlinks
check erase_scratch   step_erase_scratch_tape
check volume_init     step_vol_init
check volume_write    step_vol_write
check sealed_at_recorded step_sealed_at
check lbp_fallback_recorded step_lbp_fallback_recorded
check volume_verify   step_vol_verify

# ---------- #355: an empty drive is refused at once ----------
# Before #355 only `volume init` probed for a cartridge; `volume verify` (and
# write, identify, restore) on an empty drive sat ~2 minutes in the kernel's
# open and then failed with a bare I/O error. The probe must refuse at once,
# naming the device, and verify's exit must be 3 (inconclusive: nothing about
# any medium was learned). The timeout turns the old stall into a failure.
step_empty_drive_refused() {
    local slot out rc
    slot="$(mtx -f "$CHG_SG" status | sed -n "s/.*Data Transfer Element $DTE:Full (Storage Element \([0-9]*\) Loaded).*/\1/p")"
    [ -n "$slot" ] || { echo "no cartridge in DTE $DTE to take out"; return 1; }
    mtx -f "$CHG_SG" unload "$slot" "$DTE" || { echo "unload of DTE $DTE to slot $slot failed"; return 1; }
    out="$(timeout 60 "$BIN" --home "$HOME_DIR" --config "$CFG" volume verify "$LABEL" --device "$TAPE_DEV" 2>&1)"
    rc=$?
    # Put the cartridge back BEFORE judging, so a failure here cannot strand
    # every later check on an empty drive; rewind blocks until it is ready.
    mtx -f "$CHG_SG" load "$slot" "$DTE" || { echo "reload of slot $slot into DTE $DTE failed"; return 1; }
    mt -f "$TAPE_DEV" rewind >/dev/null 2>&1 || true
    printf '%s\n' "$out"
    [ "$rc" = 3 ] || { echo "exit $rc, want 3 (124 = the open stalled: the probe did not run first)"; return 1; }
    grep -q "no cartridge loaded in" <<<"$out" || { echo "the refusal does not say 'no cartridge loaded in'"; return 1; }
}
check empty_drive_refused step_empty_drive_refused
check evidence_row    step_evidence
check restore_diff    step_restore_A
check restore_multislice_unit step_restore_B
check restore_symlink_unit    step_restore_C

# ---------- leg 3a: negative crypto + leak scan (before heir leg rewinds) ----------
# Slices are uuid-named on disk — resolve a unit's first staged slice via the
# catalog. Prints nothing when there is none.
first_staged_slice() { # first_staged_slice <unit>
    python3 - "$HOME_DIR/tapectl.db" "$1" <<'PY'
import sqlite3, sys
row = sqlite3.connect(sys.argv[1]).execute(
    """SELECT sl.staging_path FROM stage_slices sl
       JOIN stage_sets ss ON ss.id = sl.stage_set_id
       JOIN snapshots s ON s.id = ss.snapshot_id
       JOIN units u ON u.id = s.unit_id
       WHERE u.name = ? AND sl.staging_path IS NOT NULL
       ORDER BY sl.slice_number LIMIT 1""", (sys.argv[2],)
).fetchone()
print(row[0] if row else "")
PY
}
# A rejection proves isolation only if the same `age -d` CAN succeed (issue
# #415): a slice that is not an age file, or a key file age cannot use, is
# "rejected" just the same. So two positive controls before the negative --
# alice's own key opens this slice, and bob's key opens bob's own slice.
step_crosskey() {
    local slice bslice alicekey bobkey
    slice="$(first_staged_slice unitA)"
    [ -n "$slice" ] && [ -f "$slice" ] || { echo "no unitA slice found via catalog"; return 1; }
    bslice="$(first_staged_slice unitB)"
    [ -n "$bslice" ] && [ -f "$bslice" ] || { echo "no unitB slice found via catalog"; return 1; }
    alicekey="$HOME_DIR/keys/alice-primary.age.key"
    bobkey="$HOME_DIR/keys/bob-primary.age.key"
    [ -f "$alicekey" ] || { echo "alice key missing"; return 1; }
    [ -f "$bobkey" ] || { echo "bob key missing"; return 1; }
    age -d -i "$alicekey" "$slice" >/dev/null 2>&1 || {
        echo "positive control: alice's own key did not decrypt alice's slice $slice -- a rejection of bob's key would prove nothing"
        return 1
    }
    age -d -i "$bobkey" "$bslice" >/dev/null 2>&1 || {
        echo "positive control: bob's key did not decrypt bob's own slice $bslice -- the key, not isolation, would be what fails below"
        return 1
    }
    if age -d -i "$bobkey" "$slice" >/dev/null 2>&1; then
        echo "bob's key decrypted alice's slice — isolation broken"; return 1
    fi
    echo "alice's slice refused bob's key; alice's key opens it, and bob's key opens bob's own slice"
    return 0
}
# Read every file on the tape, BOT to EOD, concatenated into one file.
# A read returning 0 bytes is a filemark; the st driver then advances past
# it, so the next read starts the next file. Two consecutive empty reads is
# EOD. The 64-file ceiling is a runaway guard, not a layout assumption -- a
# v2 volume is ~12 files -- and hitting it is reported as a failure rather
# than silently truncating the scan.
dump_whole_tape() { # dump_whole_tape <outfile>
    local out="$1" tmp="$RUN/.tapefile" got empty=0 n=0
    mt -f "$TAPE_DEV" rewind || { echo "dump_whole_tape: rewind failed"; return 1; }
    : > "$out"
    while [ "$n" -lt 64 ]; do
        dd if="$TAPE_DEV" bs=512k of="$tmp" 2>/dev/null
        got="$(stat -c %s "$tmp" 2>/dev/null)" || return 1
        if [ "$got" -eq 0 ]; then
            empty=$((empty + 1))
            [ "$empty" -ge 2 ] && break
        else
            empty=0
            # A short dump is a scan of less tape, never a clean one (#415).
            cat "$tmp" >> "$out" || { echo "dump_whole_tape: appending file $n to $out failed"; return 1; }
        fi
        n=$((n + 1))
    done
    rm -f "$tmp"
    [ "$n" -lt 64 ] || {
        echo "dump_whole_tape: hit the 64-file ceiling without reaching EOD"
        return 1
    }
    mt -f "$TAPE_DEV" rewind || return 1
    echo "dump_whole_tape: $n file(s), $(stat -c %s "$out") bytes"
}

# The on-media plaintext scan. Reads the TAPE DEVICE, not mhvtl's backing
# directory (issue #275). That directory is mode 0750 mhvtl:mhvtl and this
# gate runs unprivileged with no sudo anywhere, so every `grep -rq` into it
# exited 2 -- a permission error, never a match -- and the old
# negative-only check fell through to `return 0` on every run. It reported
# PASS because it could not read, for 100+ commits.
#
# The structural fix is the POSITIVE CONTROL below, not the device read: a
# check that only asserts absence cannot distinguish "searched and found
# nothing" from "searched nothing", which is the same shape as the
# `csc_fingerprint` guard (issue #258) and the never-run `permute` restore
# matrix. `volume-format-v3.md` puts the volume label in the ID thunk
# (File 0) in plaintext by design, so if the label is NOT found then the
# scan itself is broken, and this check must fail as loudly as a real leak
# rather than report a clean tape.
#
# Reading the device rather than the directory also makes this check work
# on a real LTO-6, which has no media directory at all -- it is no longer
# mhvtl-only.
#
# Layout 3 (T4.16): the slices hold age-encrypted tar streams, not dar's, so the needles that
# say "a plaintext archive is on this tape" are a tar's. A pax tar (what tapectl writes:
# `tar --format=pax --numeric-owner`) has "ustar", a NUL and "00" at offset 257 of every header
# block, and a member's name is in the stream in the clear: the scan looks for that magic and for
# names the units hold. Each needle KIND has a positive control in the same check: a tar made now
# from a file with such a name, run through the SAME scan function, must be found, or the
# negative scan below proves nothing (a scan for dar's signatures would have passed on any tar
# tape; a regex grep that silently matches nothing would too).
#
# The whole tape is scanned, encrypted slices included, rather than only
# the plaintext positions the Rust `mhvtl_no_plaintext_tenant_metadata`
# test parses out of the layout. That is deliberate: it is strictly
# stricter (a leak anywhere fails) and keeps this check independent of the
# layout parser it exists to cross-check. The cost is a chance of a short
# needle appearing in ciphertext by coincidence -- for "unitA" in a
# gate-sized volume that is ~1e-5, and the canary is long and unique.
# One scan, one answer: 0 found, 1 not found, 2 the search itself failed (never read as clean).
leak_found_fixed() { # leak_found_fixed <text> <file>
    LC_ALL=C grep -a -q -F -- "$1" "$2" 2>/dev/null
}
leak_found_regex() { # leak_found_regex <perl regex> <file>
    LC_ALL=C grep -a -q -P -- "$1" "$2" 2>/dev/null
}
step_leakscan() {
    local dump="$RUN/leakscan-tape.bin" needle rc
    dump_whole_tape "$dump" || return 1

    # Positive control FIRST: prove the scan can find what must be there
    # before trusting it about what must not be.
    grep -a -q "label = \"$LABEL\"" "$dump" || {
        echo "leakscan: volume label $LABEL is NOT in the tape dump -- the scan is broken, not the tape clean"
        return 1
    }
    # And the scan must have reached the END of the tape (issue #415): the
    # label is in File 0, so it alone cannot tell a whole-tape dump from one
    # that stopped early. The seal marker is the last file of a sealed v2
    # volume, and its banner appears nowhere else on tape (one hit, at the
    # dump's tail, in the 2026-10-05 gate run).
    grep -a -q "TAPECTL SEAL MARKER" "$dump" || {
        echo "leakscan: the seal marker (the volume's last file) is NOT in the tape dump -- the scan stopped short of the end, not the tape clean"
        return 1
    }

    # The tar needles and their positive control, before any negative is read.
    local ctl="$RUN/leakscan-control" tarmagic='ustar\x0000' name
    local -a names=(twelve-meg.bin big-block.bin link-broken)
    rm -rf "$ctl"; mkdir -p "$ctl/src" || return 1
    echo "positive control" >"$ctl/src/${names[0]}"
    "$GATE_TAR" --create --file="$ctl/sample.tar" --format=pax --numeric-owner -C "$ctl/src" "${names[0]}" \
        || { echo "leakscan: could not make the control tar with $GATE_TAR"; return 1; }
    leak_found_regex "$tarmagic" "$ctl/sample.tar"
    rc=$?
    [ "$rc" -eq 0 ] || { echo "leakscan: positive control: the ustar-magic scan did not find a pax tar's header (rc=$rc) -- the scan is broken, not the tape clean"; return 1; }
    leak_found_fixed "${names[0]}" "$ctl/sample.tar"
    rc=$?
    [ "$rc" -eq 0 ] || { echo "leakscan: positive control: the name scan did not find a member's name in a tar stream (rc=$rc)"; return 1; }
    echo "leakscan: positive control: the scans found the ustar magic and the name '${names[0]}' in a plaintext tar made now"
    leak_found_regex "$tarmagic" "$dump"
    rc=$?
    case "$rc" in
        0) echo "leakscan: PLAINTEXT TAR LEAK -- a tar header (ustar magic) is on the tape"; return 1 ;;
        1) ;;
        *) echo "leakscan: the ustar-magic scan failed (rc=$rc) -- inconclusive, not clean"; return 1 ;;
    esac
    for name in "${names[@]}"; do
        leak_found_fixed "$name" "$dump"
        rc=$?
        case "$rc" in
            0) echo "leakscan: PLAINTEXT LEAK -- the member name \"$name\" appears on tape"; return 1 ;;
            1) ;;
            *) echo "leakscan: grep failed (rc=$rc) looking for \"$name\" -- inconclusive, not clean"; return 1 ;;
        esac
    done

    # Negative needles. grep's rc 1 (no match) is the real pass; rc >= 2 is
    # an error and must never be read as "clean" -- that read is exactly
    # what issue #275 was.
    # #450: the isolation invariant names key fingerprints and recipients too
    # (volume-format-v3.md). A key's fingerprint is its age1 recipient
    # string; every one of both tenants' keys is a needle. At least two must be
    # found, or the needle list is empty and the scan proves nothing about keys.
    local keys t
    keys="$(for t in alice bob; do TCTL key list --tenant "$t" --json 2>/dev/null; echo; done | python3 -c '
import json, sys
out, dec, text, i = set(), json.JSONDecoder(), sys.stdin.read(), 0
while i < len(text):
    if text[i].isspace():
        i += 1; continue
    try:
        d, i = dec.raw_decode(text, i)
    except ValueError:
        break
    for r in (d if isinstance(d, list) else [d]):
        if isinstance(r, dict):
            for k in ("fingerprint", "public_key"):
                v = r.get(k)
                if isinstance(v, str) and v.startswith("age1"):
                    out.add(v)
print("\n".join(sorted(out)))')"
    [ "$(grep -c "^age1" <<<"$keys")" -ge 2 ] || {
        echo "leakscan: could not read the tenants' age1 recipients from key list --json -- the key needles would prove nothing"
        return 1
    }
    local needles=("$CANARY" "unitA")
    while IFS= read -r needle; do [ -n "$needle" ] && needles+=("$needle"); done <<<"$keys"
    echo "leakscan: ${#needles[@]} needles (canary, unit name, $(( ${#needles[@]} - 2 )) key recipients)"
    for needle in "${needles[@]}"; do
        grep -a -q -F -- "$needle" "$dump"
        rc=$?
        case "$rc" in
            0) echo "leakscan: PLAINTEXT LEAK -- \"$needle\" appears on tape"; return 1 ;;
            1) ;;
            *) echo "leakscan: grep failed (rc=$rc) looking for \"$needle\" -- inconclusive, not clean"; return 1 ;;
        esac
    done
    return 0
}
echo "gate: leg 3 — negative checks"
check crosskey_rejected step_crosskey
check no_plaintext_leak step_leakscan

# ---------- leg 2: heir leg (no tapectl, no DB) ----------
# Layout 3's RESTORE.sh (T4.16; src/volume/heir/RESTORE.sh), read off the tape by File 1's own
# mt and dd lines and run with the REAL age, tar, mt, dd and sha256sum: one leg for each mode the
# script has. tests/heir_restore.rs is the script's own suite (with those tools faked); every text
# a check below greps for is one a test there asserts, and the comment beside it names the test.
# The unit, the version, the slice and the position a leg expects come from the CATALOG and the
# source tree, never from the script's own output, so a script that printed a fixed answer fails.
HEIR="$RUN/heir"; mkdir -p "$HEIR"
HEIR_KEY_A="$HOME_DIR/keys/alice-primary.age.key"
HEIR_KEY_B="$HOME_DIR/keys/bob-primary.age.key"
HEIR_DB="$HOME_DIR/tapectl.db"
# RESTORE.sh defaults to `${TAPE_DEVICE:-/dev/nst0}` — the tape an HEIR would
# reach for, NOT necessarily this gate's drive. Every invocation below therefore
# pins TAPE_DEVICE="$TAPE_DEV". Without it the heir leg silently reads /dev/nst0:
# harmless while nst0 happened to be the mhvtl drive, but on a host where nst0 is
# a DIFFERENT (e.g. real) drive it reads the wrong tape entirely — heir_info still
# "passes" against any valid volume there while heir_find/restore fail on keys.
heir_sh() { (cd "$HEIR" && TAPE_DEVICE="$TAPE_DEV" ./RESTORE.sh "$@"); }

# The tape files of one type in RESTORE.sh --info's file map, which prints
# "  12  data_slice  1234 bytes"; the --info text is on stdin.
heir_positions_of_type() { # heir_positions_of_type <type>
    awk -v t="$1" '$2 == t && $1 ~ /^[0-9]+$/ { print $1 }'
}
# How many files --info's map lists (the seal's line included): --verify has a PASS line for each.
heir_map_count() {
    awk '/^Files on the map/ { f = 1; next } /^Next:/ { f = 0 } f && $1 ~ /^[0-9]+$/ { n++ } END { print n + 0 }'
}
# The newest snapshot version the CATALOG holds for a unit.
heir_unit_version() { # heir_unit_version <unit>
    python3 - "$HEIR_DB" "$1" <<'PY'
import sqlite3, sys
row = sqlite3.connect(sys.argv[1]).execute(
    "SELECT MAX(s.version) FROM snapshots s JOIN units u ON u.id = s.unit_id WHERE u.name = ?",
    (sys.argv[2],)).fetchone()
assert row is not None and row[0] is not None, f"no snapshot of {sys.argv[2]} in the catalog"
print(row[0])
PY
}

# The stand-ins on PATH for the heir legs that need a fault or a count (a real tape cannot be
# edited in the middle, and the heir's reads are not otherwise observable). Everything else (mt,
# the drive, age, tar, sha256sum) is the real thing.
#  - dd: reads through the real dd (REAL_DD), asks the drive (REAL_MT) which tape file the head
#    is at when dd starts, appends that number to TRACE_LOG when it is set, and flips one byte of
#    the file at FAULT_POS as it is read ("bad when it is read"); every other read is untouched.
#    The byte is at FAULT_AT (100000 when unset, or the file's last). Each flip is logged to
#    FAULT_LOG, so a run can show the fault really happened. It handles dd writing to a file (of=)
#    and to a pipe, as RESTORE.sh uses it.
heir_write_injector() { # heir_write_injector <path of the dd to write>
    cat >"$1" <<'INJECT'
#!/bin/sh
pos=$("$REAL_MT" -f "$TAPE_DEVICE" status 2>/dev/null | sed -n 's/.*[Ff]ile number *= *\([0-9-]*\).*/\1/p' | head -n 1)
[ -z "${TRACE_LOG:-}" ] || echo "$pos" >>"$TRACE_LOG"
if [ "$pos" = "$FAULT_POS" ]; then
    of=
    for a in "$@"; do
        case "$a" in of=*) of=${a#of=} ;; esac
    done
    if [ -n "$of" ]; then
        "$REAL_DD" "$@" || exit $?
        python3 -c 'import sys
p = sys.argv[1]
b = bytearray(open(p, "rb").read())
b[min(len(b) - 1, int(sys.argv[2]))] ^= 0xff
open(p, "wb").write(b)' "$of" "${FAULT_AT:-100000}"
    else
        "$REAL_DD" "$@" | python3 -c 'import sys
b = bytearray(sys.stdin.buffer.read())
b[min(len(b) - 1, int(sys.argv[1]))] ^= 0xff
sys.stdout.buffer.write(b)' "${FAULT_AT:-100000}"
    fi
    echo "flipped one byte of tape file $pos" >>"$FAULT_LOG"
    exit 0
fi
exec "$REAL_DD" "$@"
INJECT
    chmod +x "$1"
}
#  - tar: the real tar (REAL_TAR), then deletes DROP_PATH from the directory it extracted into
#    (-C), at exit 0: a tar that drops a file silently, which only the per-file sha256 check catches.
heir_write_dropper() { # heir_write_dropper <path of the tar to write>
    cat >"$1" <<'DROP'
#!/bin/bash
dest=""
args=("$@")
for ((i = 0; i < ${#args[@]}; i++)); do
  if [ "${args[$i]}" = -C ]; then dest=${args[$((i + 1))]}; fi
done
"$REAL_TAR" "$@"
rc=$?
if [ -n "$dest" ] && [ -d "$dest" ]; then rm -f -- "$dest/$DROP_PATH"; fi
exit $rc
DROP
    chmod +x "$1"
}
HEIR_HOOK="$RUN/heir-hook"
heir_hook_ready() {
    [ -x "$HEIR_HOOK/dd" ] && [ -x "$HEIR_HOOK/tar-drop/tar" ] && return 0
    mkdir -p "$HEIR_HOOK/tar-drop" && heir_write_injector "$HEIR_HOOK/dd" && heir_write_dropper "$HEIR_HOOK/tar-drop/tar"
}
# RESTORE.sh with the dd stand-in first on PATH: <fault pos> is the tape file whose read has a byte
# flipped ("none" for no fault); the faults it made go to <fault log>, every read's tape file to
# <trace log>. Both logs are emptied first.
heir_sh_hooked() { # heir_sh_hooked <fault pos|none> <fault log> <trace log> RESTORE.sh-args...
    local pos="$1" flog="$2" tlog="$3"; shift 3
    heir_hook_ready || { echo "cannot write the stand-ins under $HEIR_HOOK"; return 1; }
    : >"$flog"; : >"$tlog"
    (cd "$HEIR" && env "PATH=$HEIR_HOOK:$PATH" "REAL_DD=$(command -v dd)" "REAL_MT=$(command -v mt-st || command -v mt)" \
        "FAULT_POS=$pos" "FAULT_LOG=$flog" "TRACE_LOG=$tlog" "TAPE_DEVICE=$TAPE_DEV" ./RESTORE.sh "$@")
}
# The tape files of <trace log> that are data slices (<info text> is --info's, which names them),
# in the order they were read, one a line.
heir_slice_reads() { # heir_slice_reads <trace log> <info text>
    local pos all
    all="$(heir_positions_of_type data_slice <<<"$2")"
    while IFS= read -r pos; do
        [ -n "$pos" ] || continue
        grep -Fx -- "$pos" <<<"$all" >/dev/null && echo "$pos"
    done <"$1"
    return 0
}
# <row kind> <slice> <bytes> of one path in a unit's --list (the path as printed), from the table.
heir_list_row() { # heir_list_row <key> <unit> <path as printed>
    local out
    out="$(heir_sh --list --key "$1" --unit "$2" 2>&1)" || { printf '%s\n' "$out" >&2; return 1; }
    awk -v want="$3" '{ k = $1; s = $2; b = $3; p = $0; sub(/^ *[a-z] +[0-9]+ +[0-9]+  /, "", p); sub(/  -> .*/, "", p)
        if (length(k) == 1 && s ~ /^[0-9]+$/ && b ~ /^[0-9]+$/ && p == want) { print k, s, b; found = 1; exit } }
        END { exit found ? 0 : 1 }' <<<"$out"
}

step_heir_extract() {
    mt -f "$TAPE_DEV" rewind && mt -f "$TAPE_DEV" fsf 2 \
    && dd if="$TAPE_DEV" bs=512k 2>/dev/null | tr -d '\0' > "$HEIR/RESTORE.sh" \
    && chmod +x "$HEIR/RESTORE.sh" && bash -n "$HEIR/RESTORE.sh" || return 1
    # Every heir leg runs THIS script. Layout 2's says "Layout 2" in its second line; the file in
    # the tree is the one tests/heir_restore.rs runs (file_2_is_the_file_in_the_tree_byte_for_byte),
    # and the one File 2 of this tape must be, byte for byte once File 2's zero padding is stripped.
    sed -n 2p "$HEIR/RESTORE.sh" | grep -F "for a tapectl volume of Layout 3" >/dev/null \
        || { echo "the script read off the tape does not call itself Layout 3's: $(sed -n 2p "$HEIR/RESTORE.sh")"; return 1; }
    cmp -s "$HEIR/RESTORE.sh" "$GATE_REPO/src/volume/heir/RESTORE.sh" \
        || { echo "the RESTORE.sh on the tape is not src/volume/heir/RESTORE.sh"; cmp "$HEIR/RESTORE.sh" "$GATE_REPO/src/volume/heir/RESTORE.sh"; return 1; }
}
# Assert the tape read is the tape this gate WROTE, not merely some valid
# volume. Without this, heir_info passes against any sealed tape in any drive —
# which is exactly how the wrong-device read above stayed hidden for three runs
# while three later checks failed on "no envelope matched the provided key".
step_heir_info() {
    local out rc
    out="$(heir_sh --info 2>&1)"; rc=$?
    printf '%s\n' "$out"
    [ "$rc" -eq 0 ] || { echo "heir_info: RESTORE.sh --info exited $rc, want 0"; return 1; }
    # tests/heir_restore.rs info_names_the_tape_its_files_and_its_full_seal_and_rewinds_once
    grep -E "^Tape: $LABEL +uuid " >/dev/null <<<"$out" || { echo "heir_info: tape in $TAPE_DEV is not volume $LABEL — wrong drive or wrong cartridge" >&2; return 1; }
    grep -F "Layout 3, written by tapectl" >/dev/null <<<"$out" || { echo "heir_info: the tape does not say it is Layout 3" >&2; return 1; }
    grep -F "the tape is complete, every planned file is on it" >/dev/null <<<"$out" || { echo "heir_info: volume $LABEL is not sealed whole" >&2; return 1; }
    grep -F "The seal binds the front index" >/dev/null <<<"$out" || { echo "heir_info: the seal does not bind the front index" >&2; return 1; }
    # The control: the map names the tape's data slices, so the map was read.
    [ -n "$(heir_positions_of_type data_slice <<<"$out")" ] || { echo "heir_info: the file map names no data_slice" >&2; return 1; }
}
# --find-envelope: the key opens the tenant's envelope and shows that tenant's units only.
step_heir_find() {
    local out rc needle stranger="$HEIR/stranger.key"
    out="$(heir_sh --find-envelope --key "$HEIR_KEY_A" 2>&1)"; rc=$?
    printf '%s\n' "$out"
    [ "$rc" -eq 0 ] || { echo "heir_find_envelope: exited $rc, want 0"; return 1; }
    # tests/heir_restore.rs find_envelope_with_a_tenant_key_shows_that_tenants_units_only and
    # find_envelope_with_the_escrow_key_lists_every_unit_and_closes_with_a_command_that_works
    for needle in "The key opens the envelope at file" "  unitA  version " "  unitC  version " \
        "--restore --key KEY --to DIR --unit NAME" "(or --all for every unit, each in DIR/NAME)"; do
        grep -F -- "$needle" >/dev/null <<<"$out" || { echo "heir_find_envelope: alice's output lacks \"$needle\""; return 1; }
    done
    if grep -F "unitB" >/dev/null <<<"$out"; then echo "heir_find_envelope: alice's key shows bob's unit"; return 1; fi
    # The control: bob's key opens another envelope and shows his unit and none of alice's.
    out="$(heir_sh --find-envelope --key "$HEIR_KEY_B" 2>&1)" || { printf '%s\n' "$out"; echo "heir_find_envelope: bob's key failed"; return 1; }
    grep -F "  unitB  version " >/dev/null <<<"$out" || { printf '%s\n' "$out"; echo "heir_find_envelope: bob's key does not show unitB"; return 1; }
    if grep -E "unitA|unitC" >/dev/null <<<"$out"; then echo "heir_find_envelope: bob's key shows alice's unit"; return 1; fi
    # A key that is not a recipient: exit 1, said as a wrong key, never as damage
    # (tests/heir_restore.rs a_key_that_is_not_a_recipient_opens_nothing_and_says_so).
    age-keygen -o "$stranger" >/dev/null 2>&1 || { echo "age-keygen failed"; return 1; }
    out="$(heir_sh --find-envelope --key "$stranger" 2>&1)"; rc=$?
    [ "$rc" -eq 1 ] || { printf '%s\n' "$out"; echo "heir_find_envelope: a key that opens nothing exited $rc, want 1"; return 1; }
    for needle in "no envelope on this tape" "opens with the key given"; do
        grep -F -- "$needle" >/dev/null <<<"$out" || { printf '%s\n' "$out"; echo "heir_find_envelope: the wrong-key refusal lacks \"$needle\""; return 1; }
    done
    if grep -F "DAMAGED" >/dev/null <<<"$out"; then echo "heir_find_envelope: a wrong key was called damage"; return 1; fi
}
# --verify: no key; every file of the tape against the front index. The control: the same verify
# with one data slice flipped as it is read must fail, name the file and exit non-zero.
step_heir_verify() {
    local out rc info files passes bad
    info="$(heir_sh --info 2>&1)" || { printf '%s\n' "$info"; return 1; }
    files="$(heir_map_count <<<"$info")"
    [ "$files" -gt 8 ] || { echo "heir_verify: the file map lists only $files files"; return 1; }
    out="$(heir_sh --verify 2>&1)"; rc=$?
    printf '%s\n' "$out"
    # tests/heir_restore.rs verify_passes_a_full_tape_with_no_key_and_reads_it_forward_once
    [ "$rc" -eq 0 ] || { echo "heir_verify: --verify exited $rc, want 0"; return 1; }
    grep -E "^VERIFY PASSED: [0-9]+ file\\(s\\) match the front index\\.\$" >/dev/null <<<"$out" || { echo "heir_verify: no VERIFY PASSED line"; return 1; }
    passes="$(grep -c '^PASS ' <<<"$out" || true)"
    [ "$passes" = "$files" ] || { echo "heir_verify: $passes PASS line(s) for $files file(s) on the map"; return 1; }
    if grep -E '^FAIL ' >/dev/null <<<"$out"; then echo "heir_verify: a FAIL line on an intact tape"; return 1; fi
    # The control (tests/heir_restore.rs verify_names_the_file_that_differs_and_exits_nonzero).
    bad="$(gate_slice_pos "$HEIR_DB" "$LABEL" unitB 2)"
    [ -n "$bad" ] || { echo "heir_verify: the catalog has no slice 2 of unitB on $LABEL"; return 1; }
    out="$(heir_sh_hooked "$bad" "$RUN/heir-verify.fault" "$RUN/heir-verify.trace" --verify 2>&1)"; rc=$?
    printf '%s\n' "$out" | tail -n 12
    [ -s "$RUN/heir-verify.fault" ] || { echo "control: the stand-in never flipped a byte, so the run proves nothing"; return 1; }
    [ "$rc" -ne 0 ] || { echo "control: --verify passed with tape file $bad corrupted"; return 1; }
    grep -E "^FAIL +$bad  data_slice " >/dev/null <<<"$out" || { echo "control: no FAIL line for tape file $bad"; return 1; }
    grep -E "^VERIFY FAILED: 1 file\\(s\\) differ" >/dev/null <<<"$out" || { echo "control: no VERIFY FAILED: 1 file(s) differ"; return 1; }
}
# --list: a unit's files from its file table, no key to a slice and no slice read. The rows are
# checked against the source tree (kind, size, path), the tape files read against --info.
heir_list_rows_match() { # heir_list_rows_match <source dir> <list output file>
    python3 - "$@" <<'PY'
import os, sys
src, path = sys.argv[1:3]
rows = {}
for line in open(path, encoding="utf-8", errors="surrogateescape"):
    p = line.rstrip("\n").split(None, 3)
    if len(p) == 4 and len(p[0]) == 1 and p[0] in "fdlhpcb" and p[1].isdigit() and p[2].isdigit():
        rows[p[3].split("  -> ")[0]] = (p[0], int(p[2]))
assert rows, f"no row in {path}"
want, links = {}, {}
want["."] = ("d", None)   # the unit's root is a member and the table's first row (ADR-0012 item 306)
for root, dirs, files in os.walk(src):
    for n in dirs + files:
        full = os.path.join(root, n)
        rel = os.path.relpath(full, src)
        assert "\\" not in rel and "\t" not in rel and "\n" not in rel, f"{rel}: the leg's source needs no escaping"
        st = os.lstat(full)
        if os.path.islink(full): want[rel] = ("l", None)
        elif os.path.isdir(full): want[rel] = ("d", None)
        else:
            want[rel] = ("f", None if rel == ".tapectl-unit.toml" else st.st_size)
            if st.st_nlink > 1: links.setdefault((st.st_dev, st.st_ino), []).append(rel)
bad = []
for rel, (kind, size) in sorted(want.items()):
    got = rows.get(rel)
    if got is None: bad.append(f"{rel}: not listed"); continue
    if kind == "f" and got[0] in "fh": pass            # the second name of a hard link is an h row
    elif got[0] != kind: bad.append(f"{rel}: kind {got[0]}, want {kind}")
    if size is not None and got[0] == "f" and got[1] != size: bad.append(f"{rel}: {got[1]} bytes, want {size}")
for names in links.values():
    kinds = sorted(rows[n][0] for n in names if n in rows)
    if kinds != ["f", "h"]: bad.append(f"hard link {names}: rows {kinds}, want one f and one h")
extra = sorted(set(rows) - set(want))
if extra: bad.append(f"listed but not in the source: {extra}")
assert not bad, "\n".join(bad)
print(f"{len(rows)} rows, every one in the source; {len(links)} hard-linked pair(s) f + h")
PY
}
step_heir_list() {
    local info out rc u listing="$RUN/heir-list.out" reads
    info="$(heir_sh --info 2>&1)" || { printf '%s\n' "$info"; return 1; }
    for u in unitA unitC; do
        out="$(heir_sh_hooked none "$RUN/heir-list.fault" "$RUN/heir-list.trace" --list --key "$HEIR_KEY_A" --unit "$u" 2>&1)"; rc=$?
        printf '%s\n' "$out" >"$listing"
        printf '%s\n' "$out" | head -n 30
        [ "$rc" -eq 0 ] || { echo "heir_list: --list --unit $u exited $rc, want 0"; return 1; }
        # tests/heir_restore.rs list_shows_a_units_files_from_its_table_and_reads_no_slice
        grep -E "^Unit $u  version [0-9]+  \\([0-9]+ slice\\(s\\), [0-9]+ bytes of stream" >/dev/null <<<"$out" || { echo "heir_list: no 'Unit $u  version N  (K slice(s), B bytes of stream' line"; return 1; }
        grep -F "  kind  slice  bytes  path" >/dev/null <<<"$out" || { echo "heir_list: no column header"; return 1; }
        heir_list_rows_match "$SRC/$u" "$listing" || return 1
        # No data slice is read, and the control: front matter was (the trace is not empty).
        [ -s "$RUN/heir-list.trace" ] || { echo "control: the stand-in saw no read at all"; return 1; }
        reads="$(heir_slice_reads "$RUN/heir-list.trace" "$info")"
        [ -z "$reads" ] || { echo "heir_list: --list read data slice file(s): $reads"; return 1; }
    done
    # The escaped form is the spelling --path and --paths-from take: a name with a non-ASCII letter
    # is printed as it is (only a backslash, a TAB, a newline, a CR or a non-UTF-8 byte is escaped).
    heir_sh --list --key "$HEIR_KEY_A" --unit unitA 2>&1 | grep -F "nested/déjà-vu.txt" >/dev/null \
        || { echo "heir_list: unitA's list lacks nested/déjà-vu.txt"; return 1; }
    # A key opens its own tenant's units only: bob's unit is not listed by alice's key.
    if heir_sh --list --key "$HEIR_KEY_A" 2>&1 | grep -F "unitB" >/dev/null; then echo "heir_list: alice's key lists bob's unit"; return 1; fi
}
step_heir_restore() {
    local out rc dest="$HEIR/recovered" a b
    # `--unit unitA` is REQUIRED: alice owns unitA and unitC, and RESTORE.sh refuses to guess
    # ("FATAL: multiple units found"). One unit chosen is restored directly into --to.
    out="$(heir_sh --restore --unit unitA --key "$HEIR_KEY_A" --to "$dest" 2>&1)"; rc=$?
    printf '%s\n' "$out" | tail -n 30
    [ "$rc" -eq 0 ] || { echo "heir_restore: exited $rc, want 0"; return 1; }
    # tests/heir_restore.rs a_lone_unit_restores_byte_for_byte_into_to_and_the_work_directory_is_gone
    grep -F "RESTORE COMPLETE" >/dev/null <<<"$out" || { echo "heir_restore: no RESTORE COMPLETE"; return 1; }
    diff -r "$SRC/unitA" "$dest" || return 1
    [ ! -e "$dest/.tapectl-damaged-files.txt" ] || { echo "heir_restore: a damaged-file list on a clean restore"; return 1; }
    if compgen -G "$dest/.tapectl-work*" >/dev/null; then echo "heir_restore: the work directory was left in $dest"; return 1; fi
    # A hard link stays a link (a_lone_unit_restores_...: "a hard link stays a link").
    a="$(stat -c %i "$dest/hl-a.txt")" && b="$(stat -c %i "$dest/hl-b.txt")" || return 1
    [ "$a" = "$b" ] || { echo "heir_restore: hl-a.txt and hl-b.txt are two files after the restore"; return 1; }
    # tests/heir_restore.rs a_restore_prints_the_id_map_the_damaged_list_and_the_tar_it_used
    for needle in "Who the ids were" "never remaps them by name" "  user  $(id -u)  " "The tar used: tar (GNU tar" "What it does not restore:"; do
        grep -F -- "$needle" >/dev/null <<<"$out" || { echo "heir_restore: the output lacks \"$needle\""; return 1; }
    done
}
# The heir path is the reason this project exists, so symlink survival is
# checked there too, not only through tapectl. See step_restore_C for why
# --no-dereference is mandatory here.
step_heir_restore_symlinks() {
    local dest="$HEIR/recovered-C" l
    heir_sh --restore --unit unitC --key "$HEIR_KEY_A" --to "$dest" || return 1
    diff -r --no-dereference "$SRC/unitC" "$dest" || return 1
    for l in link-ok link-broken; do
        [ -L "$dest/$l" ] || { echo "heir_restore_symlinks: $l is not a symlink after the restore"; return 1; }
        [ "$(readlink "$dest/$l")" = "$(readlink "$SRC/unitC/$l")" ] || { echo "heir_restore_symlinks: $l points elsewhere"; return 1; }
    done
}
#  - tar: the real tar (REAL_TAR), after it appends to WATCH_LOG its arguments ("ARGV ...") and
#    what WATCH_DEST and WATCH_TMP hold at that moment ("DEST <name>", "TMP <name>", one a line):
#    where the script's work directory is while it runs (the_work_directory_is_made_inside_to_and_
#    work_replaces_it in tests/heir_restore.rs reads it the same way).
heir_write_watcher() { # heir_write_watcher <path of the tar to write>
    cat >"$1" <<'WATCH'
#!/bin/bash
{
  printf 'ARGV'; printf ' %s' "$@"; printf '\n'
  [ ! -d "$WATCH_DEST" ] || ls -A -- "$WATCH_DEST" | sed 's/^/DEST /'
  ls -A -- "$WATCH_TMP" | sed 's/^/TMP /'
} >>"$WATCH_LOG" 2>&1
exec "$REAL_TAR" "$@"
WATCH
    chmod +x "$1"
}
# heir_restore_tmpdir (#561; ADR-0012 items 270-271): heir_restore's case, unitA whole, with TMPDIR
# set, for the RESTORE.sh process only, to a directory the script cannot write. The work directory
# is made inside --to (the tar stand-in above sees it there while tar runs, and the envelope's tar
# is given a -C inside it), TMPDIR decides nothing, and nothing is written under it. The restored
# root keeps the root member's recorded mtime (ADR-0012 item 303; tests/heir_restore_s6.rs
# an_empty_to_gets_the_unit_roots_recorded_mode_and_mtime) as heir_restore's tree does: the
# control for that check is heir_restore's own tree.
step_heir_restore_tmpdir() {
    local d="$HEIR/recovered-tmpdir" ro="$HEIR/tmpdir-unwritable" log="$RUN/heir-tmpdir.watch" out rc before after want
    heir_hook_ready && mkdir -p "$HEIR_HOOK/tar-watch" && heir_write_watcher "$HEIR_HOOK/tar-watch/tar" \
        || { echo "cannot write the tar stand-in under $HEIR_HOOK"; return 1; }
    rm -rf "$d" "$ro"
    mkdir -p "$ro" && chmod 555 "$ro" || return 1
    # The control: the directory really refuses a write (it would not for root).
    if touch "$ro/probe" 2>/dev/null; then
        rm -f "$ro/probe"; echo "control: $ro takes a write, so a TMPDIR there proves nothing"; return 1
    fi
    before="$(stat -c '%Y.%Z' "$ro")" || return 1
    : >"$log"
    out="$(cd "$HEIR" && env "PATH=$HEIR_HOOK/tar-watch:$PATH" "REAL_TAR=$(command -v tar)" "WATCH_DEST=$d" "WATCH_TMP=$ro" \
        "WATCH_LOG=$log" "TMPDIR=$ro" "TAPE_DEVICE=$TAPE_DEV" ./RESTORE.sh --restore --unit unitA --key "$HEIR_KEY_A" --to "$d" 2>&1)"; rc=$?
    printf '%s\n' "$out" | tail -n 20
    # tests/heir_restore.rs a_lone_unit_restores_byte_for_byte_into_to_and_the_work_directory_is_gone
    [ "$rc" -eq 0 ] || { echo "heir_restore_tmpdir: exited $rc with TMPDIR=$ro, want 0"; return 1; }
    grep -F "RESTORE COMPLETE" >/dev/null <<<"$out" || { echo "heir_restore_tmpdir: no RESTORE COMPLETE"; return 1; }
    grep -E "File check: all [1-9][0-9]* regular file\\(s\\) match the sha256 in the unit's file table\\." >/dev/null <<<"$out" \
        || { echo "heir_restore_tmpdir: no file-check match line"; return 1; }
    diff -r "$SRC/unitA" "$d" || { echo "heir_restore_tmpdir: the tree differs from the source"; return 1; }
    [ ! -e "$d/.tapectl-damaged-files.txt" ] || { echo "heir_restore_tmpdir: a damaged-file list on a clean restore"; return 1; }
    if compgen -G "$d/.tapectl-work*" >/dev/null; then echo "heir_restore_tmpdir: the work directory was left in $d"; return 1; fi
    # The work directory, inside --to while tar ran. The positive control: the stand-in ran.
    grep -E '^ARGV ' "$log" >/dev/null || { echo "control: the tar stand-in never ran, so it saw nothing"; return 1; }
    grep -E "^DEST \\.tapectl-work\\." "$log" >/dev/null \
        || { echo "heir_restore_tmpdir: no .tapectl-work.* in --to while tar ran"; cat "$log"; return 1; }
    grep -F -- " -C $d/.tapectl-work." "$log" >/dev/null \
        || { echo "heir_restore_tmpdir: no tar was given a directory inside --to's work directory"; cat "$log"; return 1; }
    # Nothing under TMPDIR, while tar ran or after.
    if grep -E '^TMP ' "$log"; then echo "heir_restore_tmpdir: TMPDIR held the entries above while tar ran"; return 1; fi
    [ -z "$(find "$ro" -mindepth 1 -print -quit)" ] || { echo "heir_restore_tmpdir: $ro is not empty:"; find "$ro" -mindepth 1; return 1; }
    after="$(stat -c '%Y.%Z' "$ro")" || return 1
    [ "$before" = "$after" ] || { echo "heir_restore_tmpdir: $ro changed (mtime.ctime $before, now $after)"; return 1; }
    # The root's recorded mtime. The control: heir_restore's tree, restored with the default TMPDIR.
    want="$(stat -c %Y "$SRC/unitA")" || return 1
    [ -d "$HEIR/recovered" ] && [ "$(stat -c %Y "$HEIR/recovered")" = "$want" ] \
        || { echo "control: heir_restore's tree $HEIR/recovered does not have the source root's mtime $want, so this check proves nothing"; return 1; }
    [ "$(stat -c %Y "$d")" = "$want" ] \
        || { echo "heir_restore_tmpdir: the restored root's mtime is $(stat -c %Y "$d"), want the source root's $want (heir_restore's tree has it)"; return 1; }
}
echo "gate: leg 2 — heir leg (RESTORE.sh of Layout 3, no tapectl)"
check heir_extract_script step_heir_extract
check heir_info           step_heir_info
check heir_find_envelope  step_heir_find
check heir_verify         step_heir_verify
check heir_list           step_heir_list
check heir_restore        step_heir_restore
check heir_restore_tmpdir step_heir_restore_tmpdir
check heir_restore_symlinks step_heir_restore_symlinks

# heir_file_check (ADR-0012 item 103; Layout 3: the file table's sha256): the RESTORE.sh on the tape
# checks every restored regular file against the sha256 the unit's file table records, with the
# real sha256sum, and it is the check, never tar's exit status, that decides. The leg wants the
# "all N regular file(s) match" line, the tree equal to the source, the skipped line with
# --no-file-check, and the control: a tar that drops a file at exit 0 is caught, listed, exit 1.
step_heir_file_check() {
    local out rc victim="nested/déjà-vu.txt" list first
    out=$(heir_sh --restore --unit unitA --key "$HEIR_KEY_A" --to "$HEIR/recovered-fc" 2>&1) \
        || { echo "$out" | tail -20; return 1; }
    # tests/heir_restore.rs a_lone_unit_restores_byte_for_byte_into_to_and_the_work_directory_is_gone
    if ! printf '%s\n' "$out" | grep -E "File check: all [1-9][0-9]* regular file\\(s\\) match the sha256 in the unit's file table\\." >/dev/null; then
        echo "no file-check match line; the File check lines were:"
        printf '%s\n' "$out" | grep "File check" || echo "(none)"
        return 1
    fi
    diff -r "$SRC/unitA" "$HEIR/recovered-fc" || return 1
    # tests/heir_restore.rs a_tar_that_drops_a_file_at_exit_0_is_caught_by_the_file_check
    out=$(heir_sh --restore --unit unitA --no-file-check --key "$HEIR_KEY_A" --to "$HEIR/recovered-nofc" 2>&1) \
        || { echo "$out" | tail -20; return 1; }
    printf '%s\n' "$out" | grep -F -- "--no-file-check: the restored files were not checked" >/dev/null \
        || { echo "no 'the restored files were not checked' line with --no-file-check"; return 1; }
    heir_hook_ready || return 1
    out="$(cd "$HEIR" && env "PATH=$HEIR_HOOK/tar-drop:$PATH" "REAL_TAR=$(command -v tar)" "DROP_PATH=$victim" "TAPE_DEVICE=$TAPE_DEV" \
        ./RESTORE.sh --restore --unit unitA --key "$HEIR_KEY_A" --to "$HEIR/recovered-drop" 2>&1)"; rc=$?
    printf '%s\n' "$out" | tail -n 12
    [ "$rc" -eq 1 ] || { echo "control: a tar that dropped $victim at exit 0 gave exit $rc, want 1"; return 1; }
    grep -F "RESTORE INCOMPLETE: 1 path(s)" >/dev/null <<<"$out" || { echo "control: no 'RESTORE INCOMPLETE: 1 path(s)'"; return 1; }
    grep -F "File check: 1 of " >/dev/null <<<"$out" || { echo "control: no 'File check: 1 of ' line"; return 1; }
    list="$HEIR/recovered-drop/.tapectl-damaged-files.txt"
    [ -f "$list" ] || { echo "control: no damaged-file list"; return 1; }
    first="$(head -n 1 "$list")"
    case "$first" in
        $'#tapectl-damaged-files\t1\tunit=unitA\t'*) ;;
        *) echo "control: the list's first line is '$first'"; return 1 ;;
    esac
    [ "$(tail -n +2 "$list")" = "$victim" ] || { echo "control: the list names '$(tail -n +2 "$list")', want exactly $victim"; return 1; }
}
check heir_file_check     step_heir_file_check

# ---------- heir_path, heir_long_member (Layout 3: one file by the file table) ----------
# --restore --path reads, by the unit's file table, only the slices that hold the file (#518 was a
# path restore that read the wrong slices and exited 0): unitB's three small files are one slice
# each, and twelve-meg.bin, larger than a slice, is a run of slices to itself. The slice a file is
# in comes from --list, the tape file of a slice from the catalog, and the tape files the script
# READ from the dd stand-in's trace, never from the script's say-so.
step_heir_path() {
    local info out rc d reads want s k b i
    info="$(heir_sh --info 2>&1)" || { printf '%s\n' "$info"; return 1; }
    # One path, in a middle slice of a unit of many.
    read -r k s b < <(heir_list_row "$HEIR_KEY_B" unitB set/s2.bin) || { echo "heir_path: unitB's --list has no set/s2.bin"; return 1; }
    [ "$k" = f ] && [ "$s" -gt 1 ] && [ "$b" = 700000 ] || { echo "heir_path: set/s2.bin is '$k $s $b', want a 700000-byte file past slice 1"; return 1; }
    want="$(gate_slice_pos "$HEIR_DB" "$LABEL" unitB "$s")"
    [ -n "$want" ] || { echo "heir_path: no tape file for slice $s of unitB"; return 1; }
    d="$HEIR/path-one"
    out="$(heir_sh_hooked none "$RUN/heir-path.fault" "$RUN/heir-path.trace" --restore --unit unitB --key "$HEIR_KEY_B" --to "$d" --path set/s2.bin 2>&1)"; rc=$?
    printf '%s\n' "$out" | tail -n 20
    [ "$rc" -eq 0 ] || { echo "heir_path: --path set/s2.bin exited $rc, want 0"; return 1; }
    # tests/heir_restore.rs a_path_restore_reads_only_the_slice_that_holds_the_file_and_the_file_is_right
    grep -F "RESTORE COMPLETE" >/dev/null <<<"$out" || { echo "heir_path: no RESTORE COMPLETE"; return 1; }
    grep -F "File check: all 1 regular file(s) named" >/dev/null <<<"$out" || { echo "heir_path: no 'File check: all 1 regular file(s) named'"; return 1; }
    grep -F "Who the ids were" >/dev/null <<<"$out" || { echo "heir_path: no id map"; return 1; }
    cmp "$SRC/unitB/set/s2.bin" "$d/set/s2.bin" || { echo "heir_path: set/s2.bin differs from the source"; return 1; }
    [ "$(find "$d" -type f | wc -l)" = 1 ] || { echo "heir_path: more than the one file was restored:"; find "$d" -type f; return 1; }
    reads="$(heir_slice_reads "$RUN/heir-path.trace" "$info")"
    [ "$reads" = "$want" ] || { echo "heir_path: the slice reads were '$(tr '\n' ' ' <<<"$reads")', want only tape file $want (slice $s)"; return 1; }
    # Two paths, in two slices: those two slices, in order, once each
    # (tests/heir_restore.rs paths_in_two_slices_read_those_two_slices_once_each_for_every_algorithm).
    read -r k s b < <(heir_list_row "$HEIR_KEY_B" unitB set/s1.bin) || return 1
    read -r k i b < <(heir_list_row "$HEIR_KEY_B" unitB set/s3.bin) || return 1
    want="$(gate_slice_pos "$HEIR_DB" "$LABEL" unitB "$s")
$(gate_slice_pos "$HEIR_DB" "$LABEL" unitB "$i")"
    d="$HEIR/path-two"
    out="$(heir_sh_hooked none "$RUN/heir-path.fault" "$RUN/heir-path.trace" --restore --unit unitB --key "$HEIR_KEY_B" --to "$d" \
        --path set/s1.bin --path set/s3.bin 2>&1)"; rc=$?
    printf '%s\n' "$out" | tail -n 8
    [ "$rc" -eq 0 ] || { echo "heir_path: two paths exited $rc, want 0"; return 1; }
    grep -F "File check: all 2 regular file(s) named" >/dev/null <<<"$out" || { echo "heir_path: no 'File check: all 2 regular file(s) named'"; return 1; }
    for b in s1 s3; do cmp "$SRC/unitB/set/$b.bin" "$d/set/$b.bin" || return 1; done
    reads="$(heir_slice_reads "$RUN/heir-path.trace" "$info")"
    [ "$reads" = "$want" ] || { echo "heir_path: two paths read '$(tr '\n' ' ' <<<"$reads")', want '$(tr '\n' ' ' <<<"$want")'"; return 1; }
    # A path the table lacks is refused before any slice is read (a_path_not_in_the_table_is_refused_before_any_slice_is_read).
    out="$(heir_sh_hooked none "$RUN/heir-path.fault" "$RUN/heir-path.trace" --restore --unit unitB --key "$HEIR_KEY_B" --to "$HEIR/path-none" \
        --path no/such/file --path set/s1.bin 2>&1)"; rc=$?
    [ "$rc" -eq 1 ] || { printf '%s\n' "$out"; echo "heir_path: a path not in the table exited $rc, want 1"; return 1; }
    grep -F "no/such/file" >/dev/null <<<"$out" && grep -F "not in the file table" >/dev/null <<<"$out" \
        || { printf '%s\n' "$out"; echo "heir_path: the refusal does not name the path and 'not in the file table'"; return 1; }
    [ -z "$(heir_slice_reads "$RUN/heir-path.trace" "$info")" ] || { echo "heir_path: a slice was read for a path the table lacks"; return 1; }
}
check heir_path           step_heir_path

# heir_long_member: twelve-meg.bin is larger than slice_size (1M): a run of slices to itself. Whole
# unit and --path restore it across the run (the run fed to tar piece by piece, each checked); the
# slices a path restore reads are the run's, in order, once each. The control: a bad LATER piece of
# the run ends tar's run, the partial file is removed and listed (tests/heir_restore.rs
# a_long_members_bad_piece_ends_its_tar_run_removes_the_partial_file_and_reads_on), and the small
# files before it are whole.
step_heir_long_member() {
    local info out rc d reads s k b first last bad list
    info="$(heir_sh --info 2>&1)" || { printf '%s\n' "$info"; return 1; }
    read -r k first b < <(heir_list_row "$HEIR_KEY_B" unitB twelve-meg.bin) || { echo "heir_long_member: unitB's --list has no twelve-meg.bin"; return 1; }
    [ "$k" = f ] && [ "$b" = 12000000 ] || { echo "heir_long_member: twelve-meg.bin is '$k $first $b'"; return 1; }
    [ "$b" -gt 1048576 ] || return 1                       # larger than slice_size: a run, not a slice
    # The slices of the run: those after its first that the catalog says continue it.
    last="$(gate_q "$HEIR_DB" "SELECT MAX(sl.slice_number) FROM stage_slices sl JOIN stage_sets ss ON ss.id = sl.stage_set_id
        JOIN snapshots s ON s.id = ss.snapshot_id JOIN units u ON u.id = s.unit_id
        WHERE u.name = 'unitB' AND sl.continues = 1")"
    [ -n "$last" ] && [ "$last" -ge $((first + 5)) ] || { echo "heir_long_member: the catalog's run is slices $first-$last, want a run of 6 or more"; return 1; }
    # Whole unit across the run.
    d="$HEIR/long-whole"
    out="$(heir_sh --restore --unit unitB --key "$HEIR_KEY_B" --to "$d" 2>&1)"; rc=$?
    printf '%s\n' "$out" | tail -n 8
    [ "$rc" -eq 0 ] || { echo "heir_long_member: whole unitB exited $rc, want 0"; return 1; }
    diff -r "$SRC/unitB" "$d" || { echo "heir_long_member: the whole-unit restore differs from the source"; return 1; }
    # The one path: the run's slices, once each, in order.
    d="$HEIR/long-path"
    out="$(heir_sh_hooked none "$RUN/heir-long.fault" "$RUN/heir-long.trace" --restore --unit unitB --key "$HEIR_KEY_B" --to "$d" --path twelve-meg.bin 2>&1)"; rc=$?
    printf '%s\n' "$out" | tail -n 8
    [ "$rc" -eq 0 ] || { echo "heir_long_member: --path twelve-meg.bin exited $rc, want 0"; return 1; }
    cmp "$SRC/unitB/twelve-meg.bin" "$d/twelve-meg.bin" || { echo "heir_long_member: --path twelve-meg.bin differs from the source"; return 1; }
    reads="$(heir_slice_reads "$RUN/heir-long.trace" "$info")"
    [ "$(sort -n <<<"$reads")" = "$reads" ] && [ "$(sort -nu <<<"$reads")" = "$reads" ] \
        || { echo "heir_long_member: the slices were not read once each in order: $(tr '\n' ' ' <<<"$reads")"; return 1; }
    for s in $(seq "$first" "$last"); do
        grep -Fx -- "$(gate_slice_pos "$HEIR_DB" "$LABEL" unitB "$s")" <<<"$reads" >/dev/null \
            || { echo "heir_long_member: slice $s of the run was not read"; return 1; }
    done
    for s in $(seq 1 $((first - 1))); do
        if grep -Fx -- "$(gate_slice_pos "$HEIR_DB" "$LABEL" unitB "$s")" <<<"$reads" >/dev/null; then
            echo "heir_long_member: slice $s, before the run, was read"; return 1
        fi
    done
    # The control: the run's second piece is bad.
    bad="$(gate_slice_pos "$HEIR_DB" "$LABEL" unitB $((first + 1)))"
    [ -n "$bad" ] || { echo "heir_long_member: no tape file for slice $((first + 1))"; return 1; }
    d="$HEIR/long-bad"
    out="$(heir_sh_hooked "$bad" "$RUN/heir-long.fault" "$RUN/heir-long.trace" --restore --unit unitB --key "$HEIR_KEY_B" --to "$d" 2>&1)"; rc=$?
    printf '%s\n' "$out" | tail -n 14
    [ -s "$RUN/heir-long.fault" ] || { echo "control: the stand-in never flipped a byte, so the run proves nothing"; return 1; }
    [ "$rc" -eq 1 ] || { echo "control: a bad piece of the run exited $rc, want 1"; return 1; }
    grep -F "a long file's run broke at a bad slice" >/dev/null <<<"$out" || { echo "control: no \"a long file's run broke at a bad slice\""; return 1; }
    [ ! -e "$d/twelve-meg.bin" ] || { echo "control: the partial twelve-meg.bin was left"; return 1; }
    list="$d/.tapectl-damaged-files.txt"
    [ -f "$list" ] && tail -n +2 "$list" | grep -Fx twelve-meg.bin >/dev/null || { echo "control: the damaged-file list does not name twelve-meg.bin"; return 1; }
    for b in s1 s2 s3; do cmp "$SRC/unitB/set/$b.bin" "$d/set/$b.bin" || { echo "control: set/$b.bin, before the run, is not whole"; return 1; }; done
}
check heir_long_member    step_heir_long_member

# ---------- heir_where, heir_find, heir_find_path, heir_paths_from_copy (ADR-0012 items 78-81 and 101; #418, #480) ----------
#
# The tape tapectl wrote carries the cross-volume index in its envelopes
# (volume-format-v3.md 1.5), and the RESTORE.sh on that tape reads it with the real
# age, with no tapectl and no database. The unit, the version and the path the
# legs expect come from the CATALOG and the source tree, never from the script's
# own output, so a script that printed a fixed answer would fail.
#
# heir_paths_from_copy is the one repair of Layout 3 (item 255) on real media: a slice of the
# first cartridge bad when it is read (the dd stand-in flips a byte of that one tape file, since a
# tape cannot be edited in the middle) leaves one file out and listed; the SAME staged unit written
# to a second cartridge (a real tapectl copy, whose slices are byte for byte the first tape's) is
# then put in the drive, and `--restore --paths-from LIST` makes the listed path whole from it.

step_heir_where() {
    local ver out rc needle key="$HEIR_KEY_A"
    ver="$(heir_unit_version unitA)" || return 1
    out="$(heir_sh --where unitA --key "$key" 2>&1)"; rc=$?
    printf '%s\n' "$out"
    [ "$rc" -eq 0 ] || { echo "heir_where: RESTORE.sh --where unitA exited $rc, want 0"; return 1; }
    # tests/heir_restore.rs where_prints_every_version_and_holder_of_a_unit_from_a_tenant_key
    for needle in "Index read from tape: $LABEL" "Unit unitA (tenant alice):" "  Version $ver: " "    tape $LABEL  (this tape)"; do
        grep -F -- "$needle" >/dev/null <<<"$out" || { echo "heir_where: the output lacks \"$needle\""; return 1; }
    done
    # The same index asked about a unit it does not hold: exit 1, and it names the
    # units it does hold. This is the control for the lines above, which prove the
    # search finds a unit; this proves it is a search and not a fixed answer
    # (tests/heir_restore.rs where_names_the_units_the_index_lists_when_it_finds_none).
    out="$(heir_sh --where no-such-unit --key "$key" 2>&1)"; rc=$?
    [ "$rc" -eq 1 ] || { printf '%s\n' "$out"; echo "heir_where: an unknown unit exited $rc, want 1"; return 1; }
    grep -F "No unit named 'no-such-unit' is in this index" >/dev/null <<<"$out" || { printf '%s\n' "$out"; echo "heir_where: no 'No unit named' line"; return 1; }
    grep -F -- "unitA" >/dev/null <<<"$out" || { printf '%s\n' "$out"; echo "heir_where: the unknown-unit answer does not list the units the index holds"; return 1; }
    # --where --all: every unit the index names (tests/heir_restore.rs a_miss_and_where_all_on_a_large_index_name_every_unit).
    out="$(heir_sh --where --all --key "$key" 2>&1)"; rc=$?
    [ "$rc" -eq 0 ] || { printf '%s\n' "$out"; echo "heir_where: --where --all exited $rc, want 0"; return 1; }
    grep -F -- "Unit unitA (tenant alice): newest version $ver, on this tape" >/dev/null <<<"$out" \
        || { printf '%s\n' "$out"; echo "heir_where: --where --all does not list unitA at its newest version on this tape"; return 1; }
    grep -E "^[0-9]+ unit\\(s\\) in this index; this tape holds the newest version of [0-9]+ of them whole\\.\$" >/dev/null <<<"$out" \
        || { printf '%s\n' "$out"; echo "heir_where: --where --all has no totals line"; return 1; }
}
step_heir_find_path() {
    local ver size out rc needle key="$HEIR_KEY_A"
    ver="$(heir_unit_version unitA)" || return 1
    size=$(( $(wc -c <"$SRC/unitA/plain.txt") ))
    out="$(heir_sh --find plain.txt --key "$key" 2>&1)"; rc=$?
    printf '%s\n' "$out"
    [ "$rc" -eq 0 ] || { echo "heir_find: RESTORE.sh --find plain.txt exited $rc, want 0"; return 1; }
    # tests/heir_restore.rs find_prints_matching_paths_with_their_holders
    for needle in "In the index of tenant alice:" "  unitA  v$ver  f  $size bytes  plain.txt" "  Tapes holding unitA version $ver:" \
        "    tape $LABEL  (this tape)" "--restore --key KEY --unit UNIT --path PATH"; do
        grep -F -- "$needle" >/dev/null <<<"$out" || { echo "heir_find: the output lacks \"$needle\""; return 1; }
    done
    out="$(heir_sh --find no-such-path-anywhere --key "$key" 2>&1)"; rc=$?
    [ "$rc" -eq 1 ] || { printf '%s\n' "$out"; echo "heir_find: a path nothing holds exited $rc, want 1"; return 1; }
    grep -F "No path in this index contains" >/dev/null <<<"$out" || { printf '%s\n' "$out"; echo "heir_find: no 'No path in this index contains'"; return 1; }
}
# heir_find_path: --find prints a path on one line, as --path takes it; the path as printed is
# restored by --restore --path and comes back byte for byte (a name with non-ASCII letters, in a
# folder, so the printed form is the form --path reads).
step_heir_find_to_path() {
    local out rc printed d key="$HEIR_KEY_A"
    out="$(heir_sh --find vu.txt --key "$key" 2>&1)"; rc=$?
    printf '%s\n' "$out"
    [ "$rc" -eq 0 ] || { echo "heir_find_path: --find vu.txt exited $rc, want 0"; return 1; }
    printed="$(sed -n 's/^  unitA  v[0-9]*  f  [0-9]* bytes  //p' <<<"$out")"
    [ "$printed" = "nested/déjà-vu.txt" ] || { echo "heir_find_path: the match was printed as '$printed', want nested/déjà-vu.txt"; return 1; }
    d="$HEIR/find-path"
    heir_sh --restore --unit unitA --key "$key" --to "$d" --path "$printed" || { echo "heir_find_path: --path '$printed' failed"; return 1; }
    cmp "$SRC/unitA/$printed" "$d/$printed" || { echo "heir_find_path: '$printed' differs from the source"; return 1; }
    [ "$(find "$d" -type f | wc -l)" = 1 ] || { echo "heir_find_path: more than that one file was restored"; return 1; }
}

# The first cartridge back in the drive: a no-op when it is there already.
heir_reload_first() { # heir_reload_first <the storage slot the first cartridge came from>
    local status loaded
    status="$(mtx -f "$CHG_SG" status)" || return 1
    if grep -F "Data Transfer Element $DTE:Full (Storage Element $1 Loaded)" >/dev/null <<<"$status"; then
        return 0
    fi
    loaded="$(sed -n "s/.*Data Transfer Element $DTE:Full (Storage Element \\([0-9]*\\) Loaded).*/\\1/p" <<<"$status")"
    if [ -n "$loaded" ]; then
        mtx -f "$CHG_SG" unload "$loaded" "$DTE" || return 1
    fi
    mtx -f "$CHG_SG" load "$1" "$DTE" || return 1
    mt -f "$TAPE_DEV" rewind >/dev/null 2>&1 || true
}
# A real second copy: another cartridge, the same staged sets (--keep-staging, or the write would
# release the staging that leg 4 writes from). It is left LOADED in the drive.
heir_second_copy() { # heir_second_copy <label> <slot of the second cartridge> <slot of the first>
    local label="$1" b_slot="$2" a_slot="$3" info
    mtx -f "$CHG_SG" unload "$a_slot" "$DTE" || { echo "cannot take the first cartridge out to slot $a_slot"; return 1; }
    mtx -f "$CHG_SG" load "$b_slot" "$DTE" || { echo "cannot load slot $b_slot"; return 1; }
    if ! { mt -f "$TAPE_DEV" rewind && mt -f "$TAPE_DEV" erase; }; then
        echo "cannot erase the second cartridge"; return 1
    fi
    TCTL volume init "$label" --device "$TAPE_DEV" || return 1
    TCTL volume write "$label" --device "$TAPE_DEV" --yes --keep-staging || return 1
    info="$(heir_sh --info 2>&1)" || { printf '%s\n' "$info"; return 1; }
    grep -F "the tape is complete, every planned file is on it" >/dev/null <<<"$info" || { printf '%s\n' "$info"; echo "the second copy is not sealed whole"; return 1; }
    grep -E "^Tape: $label +uuid " >/dev/null <<<"$info" || { echo "the cartridge in the drive is not $label"; return 1; }
}
step_heir_paths_from_copy() {
    local w="$RUN/paths-copy" clabel="MHVTLC2" status a_slot b_slot bad out rc list d rc2=0 cleanup_rc=0 got
    rm -rf "$w"; mkdir -p "$w"
    status="$(mtx -f "$CHG_SG" status)" || { echo "mtx status failed"; return 1; }
    a_slot="$(sed -n "s/.*Data Transfer Element $DTE:Full (Storage Element \\([0-9]*\\) Loaded).*/\\1/p" <<<"$status")"
    [ -n "$a_slot" ] || { echo "no cartridge in DTE $DTE"; return 1; }
    b_slot="$(grep -E "Storage Element [0-9]+:Full" <<<"$status" | grep "VolumeTag=[EF][0-9]*$GEN" | head -n 1 | sed -n 's/.*Storage Element \([0-9]*\):Full.*/\1/p')"
    [ -n "$b_slot" ] || { echo "no second $GEN cartridge in the library: this leg needs two"; return 1; }
    # unitB's slice 2 holds set/s2.bin and nothing else (a slice ends at a member's start): the one
    # slice that goes bad, and so the one file the repair must bring back.
    bad="$(gate_slice_pos "$HEIR_DB" "$LABEL" unitB 2)"
    [ -n "$bad" ] || { echo "no tape file for slice 2 of unitB on $LABEL"; return 1; }
    echo "first cartridge in slot $a_slot; second in slot $b_slot; tape file $bad is the slice that goes bad"
    # The control: the first cartridge alone. The restore leaves set/s2.bin out and lists it.
    d="$w/unit"
    out="$(heir_sh_hooked "$bad" "$w/fault.log" "$w/trace.log" --restore --unit unitB --key "$HEIR_KEY_B" --to "$d" 2>&1)"; rc=$?
    printf '%s\n' "$out" | tail -n 16
    [ -s "$w/fault.log" ] || { echo "control: the stand-in never flipped a byte, so the run proves nothing"; return 1; }
    # tests/heir_restore.rs a_bad_slice_is_left_out_its_files_are_listed_and_the_rest_restores
    [ "$rc" -eq 1 ] || { echo "control: the restore with tape file $bad corrupted exited $rc, want 1"; return 1; }
    grep -F "RESTORE INCOMPLETE" >/dev/null <<<"$out" || { echo "control: no RESTORE INCOMPLETE"; return 1; }
    grep -F "differs from the front index's hash" >/dev/null <<<"$out" || { echo "control: the output does not say the slice differs from the front index's hash"; return 1; }
    list="$d/.tapectl-damaged-files.txt"
    [ -f "$list" ] || { echo "control: no damaged-file list"; return 1; }
    [ "$(tail -n +2 "$list")" = "set/s2.bin" ] || { echo "control: the list names '$(tail -n +2 "$list" | tr '\n' ' ')', want exactly set/s2.bin"; return 1; }
    [ ! -e "$d/set/s2.bin" ] || { echo "control: set/s2.bin is on disk although its slice was bad"; return 1; }
    for got in set/s1.bin set/s3.bin twelve-meg.bin; do cmp "$SRC/unitB/$got" "$d/$got" || { echo "control: $got is not whole"; return 1; }; done

    # The second cartridge: a real copy, left in the drive. Whatever happens the first cartridge
    # goes back, and the copy stops being a live copy: leg 4 asserts that its volumes hold the
    # ONLY copy of unitA/unitB, which a second sealed volume would falsify.
    heir_second_copy "$clabel" "$b_slot" "$a_slot" || rc2=$?
    if [ "$rc2" -eq 0 ]; then
        # The repair, from the loaded copy: only the listed path, and the list is removed.
        out="$(heir_sh --restore --unit unitB --key "$HEIR_KEY_B" --to "$d" --paths-from "$list" 2>&1)"; rc=$?
        printf '%s\n' "$out" | tail -n 16
        # tests/heir_restore.rs two_copies_each_damaged_in_different_files_are_made_whole_in_two_runs
        if [ "$rc" -ne 0 ]; then echo "the repair from the second cartridge exited $rc, want 0"; rc2=1; fi
        grep -F "RESTORE COMPLETE" >/dev/null <<<"$out" || { echo "the repair did not say RESTORE COMPLETE"; rc2=1; }
        grep -F "is removed: nothing is left to repair" >/dev/null <<<"$out" || { echo "the repair did not remove the damaged-file list"; rc2=1; }
        [ ! -e "$list" ] || { echo "the damaged-file list is still there"; rc2=1; }
    fi
    heir_reload_first "$a_slot" || { echo "cannot put the first cartridge back in the drive"; return 1; }
    if TCTL volume retire "$clabel" --yes; then :; else cleanup_rc=$?; echo "volume retire $clabel exited $cleanup_rc"; fi
    [ "$rc2" -eq 0 ] || return 1
    # The restored names compared byte for byte with the source.
    diff -r "$SRC/unitB" "$d" || { echo "the repaired tree differs from the source"; return 1; }
    [ "$cleanup_rc" -eq 0 ] || return 1
}
check heir_where          step_heir_where
check heir_find           step_heir_find_path
check heir_find_path      step_heir_find_to_path
check heir_paths_from_copy step_heir_paths_from_copy

# ---------- leg 4: interrupt + resume (issue #93) ----------
#
# MUST BE LAST. It erases the tape written by legs 1-3, so every check that
# reads $LABEL has to have run already.
#
# Why this leg exists: `volume resume` is the one command that only ever runs
# after something has already gone wrong, so it is the one that will never
# have been rehearsed before it is needed. Its inner machinery is covered by
# tests/resume_session.rs over MemStore, but the orchestrator opens a real
# TapeStore and had zero end-to-end coverage.
#
# `src/main.rs` installs the SIGINT handler, so a SIGINT here is a CLEAN
# interrupt, not a kill: `session.rs`'s run_entries checks the flag BETWEEN
# entries and marks `writes.status = 'interrupted'` itself. That is what we
# assert. (A hard crash leaves rows `in_progress` until the next db::open()
# sweep converts them — a different arm, noted as uncovered at the end.)
RLABEL1="MHVTLR1"   # arm 1: interrupted before any content — resume from BOT
RLABEL2="MHVTLR2"   # arm 2: interrupted mid-run — resume repositions
RLABEL3="MHVTLR3"   # arm 3: hard-killed — startup sweep then resume
RLABEL4="MHVTLR4"   # arm 4: interrupted AFTER seal — the Tier-3 floor (issue #276)

# Start a write in the background and SIGINT it once the DB shows the state
# this arm needs. Polling beats a fixed sleep for two reasons found the hard
# way on the first run of this leg:
#
#   1. Bash sets SIGINT to *ignored* for background jobs in a non-interactive
#      shell, and the tapectl process inherits that until ctrlc::set_handler
#      overrides it. A SIGINT sent at t=0 is therefore silently dropped and
#      the write runs to completion.
#   2. `volume write` spends its first seconds in build/validate/plan
#      (validate full-hashes every staged slice) before any byte reaches tape,
#      and the front zone + envelopes are written before the first slice. A
#      3s sleep landed before ANY slice — the window where slices are in
#      flight is short and machine-dependent.
#
# So each arm names the condition it needs and we wait for it. The binary is
# invoked directly rather than through TCTL() so $! is the tapectl process
# itself and not a wrapping subshell that would swallow the signal.
# Issue #113: the beginning-of-tape arm cannot be reached by POLLING. Its
# condition — session planned, zero entries confirmed — is a moment, not a
# state: `writes.status='in_progress'` is already true at plan(), so by the
# time a poller sees it and delivers a signal, entry 0 may already be
# confirmed. That arm reddened ~1 run in 3.
#
# Instead of guessing at the timing, we make the writer PARK there:
# TAPECTL_TEST_PAUSE_AFTER_PLAN names a marker path, execute() stops at
# exactly that state and creates the file, and we wait for the file to exist
# — a fact, not a race — before signalling. No sleeps, no tuning.
interrupt_write_parked() { # interrupt_write_parked <label> [signal=INT]
    local label="$1" sig="${2:-INT}" pid start waited marker
    marker="$RUN/parked-$label"
    rm -f "$marker"
    start=$SECONDS
    TAPECTL_TEST_PAUSE_AFTER_PLAN="$marker" \
        "$BIN" --home "$HOME_DIR" --config "$CFG" volume write "$label" --device "$TAPE_DEV" --yes &
    pid=$!
    while kill -0 "$pid" 2>/dev/null; do
        [ -e "$marker" ] && break
        if [ $(( SECONDS - start )) -ge 120 ]; then
            echo "interrupt_write_parked: TIMEOUT waiting for the park marker"; break
        fi
        sleep 0.1
    done
    waited=$(( SECONDS - start ))
    kill -"$sig" "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    echo "interrupt_write_parked: label=$label sig=$sig waited=${waited}s (parked at BOT)"
}

interrupt_write() { # interrupt_write <label> <sql-ready> <what> [signal=INT]
    local label="$1" ready_sql="$2" what="$3" sig="${4:-INT}" pid start waited
    start=$SECONDS
    "$BIN" --home "$HOME_DIR" --config "$CFG" volume write "$label" --device "$TAPE_DEV" --yes &
    pid=$!
    # Wait for the condition, but never past the process exiting or 120s.
    while kill -0 "$pid" 2>/dev/null; do
        if [ "$(python3 -c "
import sqlite3,sys
c=sqlite3.connect('file:$HOME_DIR/tapectl.db?mode=ro',uri=True)
print(1 if c.execute(\"\"\"$ready_sql\"\"\").fetchone()[0] else 0)
" 2>/dev/null)" = "1" ]; then
            break
        fi
        if [ $(( SECONDS - start )) -ge 120 ]; then
            echo "interrupt_write: TIMEOUT waiting for: $what"; break
        fi
        sleep 0.1
    done
    waited=$(( SECONDS - start ))
    kill -"$sig" "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    # Logged so drift is visible BEFORE it becomes a flake.
    echo "interrupt_write: label=$label sig=$sig waited=${waited}s for: $what"
}

# Assert the interrupt actually happened, and report how far it got.
# Without this the leg would false-pass whenever the write simply finished
# before the SIGINT landed — the failure mode that makes a timing-based
# check worthless.
assert_interrupted() { # assert_interrupted <label> <min-written> <max-written>
    python3 - "$HOME_DIR/tapectl.db" "$1" "$2" "$3" <<'PY'
import sqlite3, sys
db, label, lo, hi = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4])
c = sqlite3.connect(db)
status = c.execute("SELECT status FROM volumes WHERE label=?", (label,)).fetchone()[0]
assert status != 'sealed', (
    f"{label}: volume is already 'sealed' — the write COMPLETED before the SIGINT "
    f"landed, so nothing was resumed and this leg proved nothing. "
    f"Remedy: for the BOT arm, the park hook (TAPECTL_TEST_PAUSE_AFTER_PLAN) did not "
    f"engage; for the others, wait on a later DB precondition or enlarge the fixture "
    f"payload. There is no sleep to raise (issue #113).")
rows = c.execute(
    """SELECT w.status, COUNT(*) FROM writes w
       JOIN volumes v ON v.id = w.volume_id WHERE v.label=? GROUP BY w.status""",
    (label,)).fetchall()
by = dict(rows)
assert by.get('interrupted', 0) > 0, (
    f"{label}: expected >=1 write row 'interrupted' (main.rs installs the SIGINT "
    f"handler, so the run_entries loop marks them itself); got {by}. "
    f"If these are 'in_progress' the signal killed the process instead of being "
    f"handled — check that install_handler() still runs before the write.")
written = c.execute(
    """SELECT COUNT(*) FROM write_positions wp
       JOIN writes w ON w.id = wp.write_id
       JOIN volumes v ON v.id = w.volume_id
       WHERE v.label=? AND wp.status='written'""", (label,)).fetchone()[0]
assert lo <= written <= hi, (
    f"{label}: expected between {lo} and {hi} confirmed-written positions for this "
    f"arm, got {written}. There is no sleep to retune (issue #113): the BOT arm parks "
    f"the writer via TAPECTL_TEST_PAUSE_AFTER_PLAN and the others wait on a DB "
    f"precondition. A BOT failure here means the park hook did not engage — check that "
    f"the env var reached the process and that run_entries still honours it. Do NOT "
    f"widen this bound to 0..1: that makes this arm a duplicate of resume_midwrite and "
    f"deletes the beginning-of-tape case.")
print(f"{label}: interrupted cleanly, writes={by}, confirmed-written positions={written}")
PY
}

assert_sealed() { # assert_sealed <label>
    python3 - "$HOME_DIR/tapectl.db" "$1" <<'PY'
import sqlite3, sys
db, label = sys.argv[1], sys.argv[2]
c = sqlite3.connect(db)
status = c.execute("SELECT status FROM volumes WHERE label=?", (label,)).fetchone()[0]
assert status == 'sealed', f"{label}: expected volume 'sealed' after resume, got {status!r}"
bad = c.execute(
    """SELECT w.status, COUNT(*) FROM writes w JOIN volumes v ON v.id=w.volume_id
       WHERE v.label=? AND w.status <> 'completed' GROUP BY w.status""", (label,)).fetchall()
assert not bad, f"{label}: writes not all 'completed' after resume: {bad}"
print(f"{label}: sealed, all writes completed")
PY
}

# --- arm 1: interrupted before ANY content entry (cursor at BOT) ---
# SIGINT at t~0 lands on the check that precedes entry 0, so nothing is
# confirmed written and resume must restart from the beginning of tape.
step_resume_bot() {
    mt -f "$TAPE_DEV" rewind && mt -f "$TAPE_DEV" erase \
    && TCTL volume init "$RLABEL1" --device "$TAPE_DEV" \
    && interrupt_write_parked "$RLABEL1" \
    && assert_interrupted "$RLABEL1" 0 0 \
    && TCTL volume resume "$RLABEL1" --device "$TAPE_DEV" \
    && assert_sealed "$RLABEL1"
}

# --- arm 2: interrupted mid-run (cursor mid-tape, resume repositions) ---
# The uninterrupted write takes ~9s on mhvtl, so 3s reliably lands with
# several entries confirmed and several still to go. assert_interrupted
# turns a mistimed run into a FAIL with a remedy, never a silent pass.
step_resume_midwrite() {
    mt -f "$TAPE_DEV" rewind && mt -f "$TAPE_DEV" erase \
    && TCTL volume init "$RLABEL2" --device "$TAPE_DEV" \
    && interrupt_write "$RLABEL2" \
        "SELECT COUNT(*) FROM write_positions wp
           JOIN writes w ON w.id=wp.write_id
           JOIN volumes v ON v.id=w.volume_id
         WHERE v.label='$RLABEL2' AND wp.status='written'" \
        "at least one slice confirmed written (reposition arm)" \
    && assert_interrupted "$RLABEL2" 1 100000 \
    && TCTL volume resume "$RLABEL2" --device "$TAPE_DEV" \
    && assert_sealed "$RLABEL2"
}

# --- arm 3: hard crash (SIGKILL), the power-loss case ---
# Distinct code from arms 1-2: SIGKILL gives the process no chance to mark
# anything, so the rows stay 'in_progress' and only become 'interrupted' when
# `recover_orphaned_sessions` sweeps them at the next db::open() — which is
# the `volume resume` invocation itself. Arguably the likeliest real-world
# interruption, and until now the sweep-to-resume handoff was never exercised
# end to end on real tape.
step_resume_after_crash() {
    mt -f "$TAPE_DEV" rewind && mt -f "$TAPE_DEV" erase \
    && TCTL volume init "$RLABEL3" --device "$TAPE_DEV" \
    && interrupt_write "$RLABEL3" \
        "SELECT COUNT(*) FROM writes w JOIN volumes v ON v.id=w.volume_id
         WHERE v.label='$RLABEL3' AND w.status='in_progress'" \
        "the session to start writing, then KILL it uncleanly" KILL \
    && assert_crashed "$RLABEL3" \
    && TCTL volume resume "$RLABEL3" --device "$TAPE_DEV" \
    && assert_sealed "$RLABEL3"
}

# The crash arm's precondition is the OPPOSITE of assert_interrupted's: rows
# must still be 'in_progress', proving nothing had a chance to mark them and
# that the startup sweep is what rescues the session.
assert_crashed() { # assert_crashed <label>
    python3 - "$HOME_DIR/tapectl.db" "$1" <<'PYX'
import sqlite3, sys
db, label = sys.argv[1], sys.argv[2]
c = sqlite3.connect(db)
status = c.execute("SELECT status FROM volumes WHERE label=?", (label,)).fetchone()[0]
assert status != 'sealed', f"{label}: write completed before the KILL landed — nothing to resume"
by = dict(c.execute(
    """SELECT w.status, COUNT(*) FROM writes w JOIN volumes v ON v.id=w.volume_id
       WHERE v.label=? GROUP BY w.status""", (label,)).fetchall())
assert by.get('in_progress', 0) > 0, (
    f"{label}: expected >=1 write row still 'in_progress' after SIGKILL (a hard kill "
    f"leaves the process no chance to mark them; the startup sweep converts them on "
    f"the next db::open()); got {by}")
print(f"{label}: crashed uncleanly, writes={by} — resume must rely on the startup sweep")
PYX
}

# A resumed tape must be indistinguishable from a straight-through one:
# verify passes and a real unit round-trips byte-for-byte.
step_resume_verify() { verify_json "$RLABEL2" "$TAPE_DEV" "$RUN/verify-resumed.json"; }
step_resume_restore() {
    TCTL restore unit --unit unitA --from "$RLABEL2" --to "$RUN/restored-resumed" --device "$TAPE_DEV" \
    && diff -r "$SRC/unitA" "$RUN/restored-resumed"
}


# --- arm 4: interrupted AFTER seal, before confirm (issue #276) ---
#
# The state migration 018 calls case (b): `seal()` succeeded, so the seal
# marker is physically on the tape and `volumes.sealed_at` is recorded, but
# `confirm` never landed an outcome — `volumes.status` is still
# 'initialized' and the writes rows are 'interrupted'. Arms 1–3 cannot
# reach it: all three interrupt during execute, so seal() never runs.
#
# Why this arm is the acceptance for #276 and not a unit test: the defect is
# that `volume retire`'s ADR-0008 Tier-3 floor could not SEE this volume, so
# the only copy of a unit could be discarded with no refusal. Proving the fix
# needs the state to have been produced by a real write to real media, not
# hand-written into the catalog with an UPDATE — a fabricated row proves the
# query, never the reachability. TAPECTL_TEST_PAUSE_AFTER_SEAL parks the
# writer at exactly that point so the interrupt is a fact, not a race, the
# same way TAPECTL_TEST_PAUSE_AFTER_PLAN does for the BOT arm (issue #113).
#
# `--yes` is passed to the retire ON PURPOSE. ADR-0008 says Tier 3 is a fact,
# not a risk to accept, and `--yes` must not reach it; passing the flag makes
# a silent downgrade to a Tier-2 prompt fail here instead of hanging on stdin.
interrupt_write_after_seal() { # interrupt_write_after_seal <label>
    local label="$1" pid start waited marker
    marker="$RUN/sealed-parked-$label"
    rm -f "$marker"
    start=$SECONDS
    TAPECTL_TEST_PAUSE_AFTER_SEAL="$marker" \
        "$BIN" --home "$HOME_DIR" --config "$CFG" volume write "$label" --device "$TAPE_DEV" --yes &
    pid=$!
    while kill -0 "$pid" 2>/dev/null; do
        [ -e "$marker" ] && break
        if [ $(( SECONDS - start )) -ge 120 ]; then
            echo "interrupt_write_after_seal: TIMEOUT waiting for the post-seal park marker"; break
        fi
        sleep 0.1
    done
    waited=$(( SECONDS - start ))
    kill -INT "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    echo "interrupt_write_after_seal: label=$label waited=${waited}s (parked after seal)"
}

# The reachability proof, and its own positive control. Asserting only that
# retire refuses would pass just as happily if the write had never run at
# all, so this pins the three facts that MAKE it case (b) first.
assert_sealed_but_unconfirmed() { # assert_sealed_but_unconfirmed <label>
    python3 - "$HOME_DIR/tapectl.db" "$1" <<'PYS'
import sqlite3, sys
db, label = sys.argv[1], sys.argv[2]
c = sqlite3.connect(db)
row = c.execute("SELECT status, sealed_at FROM volumes WHERE label=?", (label,)).fetchone()
assert row is not None, f"{label}: no volumes row at all — the write never ran"
status, sealed_at = row
assert sealed_at is not None, (
    f"{label}: sealed_at is NULL, so seal() never ran and this is migration 018's case "
    f"(a), not (b). The park hook fired too early — check that "
    f"TAPECTL_TEST_PAUSE_AFTER_SEAL parks AFTER the sealed_at UPDATE in finish_session.")
assert status != 'sealed', (
    f"{label}: volume is already {status!r} — confirm completed, so this is an ordinary "
    f"sealed tape and the arm is testing nothing. The interrupt landed too late.")
by = dict(c.execute(
    """SELECT w.status, COUNT(*) FROM writes w JOIN volumes v ON v.id=w.volume_id
       WHERE v.label=? GROUP BY w.status""", (label,)).fetchall())
assert by.get('interrupted', 0) > 0, (
    f"{label}: expected >=1 write row 'interrupted' after the post-seal interrupt; got {by}")
print(f"{label}: sealed_at={sealed_at}, volume status={status!r}, writes={by} "
      f"— migration 018 case (b), reached from a real write")
PYS
}

# The refusal itself. Every assertion names a substring of the Tier-3 message,
# because a bare non-zero exit is also what "volume not found" or a panic
# produces — the distinction this gate exists to make (issue #275: a check
# that only asserts a failure cannot tell WHICH failure it got).
assert_retire_refused() { # assert_retire_refused <label> <unit> <want_resume_line:yes|no>
    local label="$1" unit="$2" want_resume="$3" out rc needle has_resume errexit_was=0
    # Restore the CALLER's errexit state rather than forcing it on (issue
    # #415). This was `set +e ... set -e`, and this script never sets -e
    # (`set -uo pipefail`, line 33), so every gate run left errexit ON for
    # everything after leg 4.
    case $- in *e*) errexit_was=1 ;; esac
    set +e
    out="$(TCTL volume retire "$label" --yes 2>&1)"
    rc=$?
    if [ "$errexit_was" = 1 ]; then set -e; fi
    printf '%s\n' "$out" > "$RUN/retire-refusal-$label.txt"
    if [ $rc -eq 0 ]; then
        echo "assert_retire_refused: $label: retire SUCCEEDED (rc=0). This is issue #276 \
exactly: a sealed-but-unconfirmed volume holding the only copy of \"$unit\" was retired \
with no ADR-0008 Tier-3 refusal. Output:"
        printf '%s\n' "$out"
        return 1
    fi
    # grep reads a here-string, not `printf | grep -q` (issue #415): under
    # pipefail a grep -q that exits on its first match can SIGPIPE the
    # printf, and the pipeline then reads as "absent".
    for needle in "LAST eligible copy" "$unit"; do
        if ! grep -F -- "$needle" >/dev/null <<<"$out"; then
            echo "assert_retire_refused: $label: refused (rc=$rc) but the message does not \
contain \"$needle\" — a non-zero exit alone does not prove the Tier-3 floor fired rather \
than some unrelated error. Output:"
            printf '%s\n' "$out"
            return 1
        fi
    done
    # The `volume resume` advice must appear when it APPLIES and be absent when
    # it does not -- asserting only its presence would pass a build that printed
    # it unconditionally, which would be wrong advice for an ordinary sealed
    # tape. Checked in both directions for that reason.
    #
    # The absent direction's positive control is the loop above: the same
    # grep over the same $out has just found "LAST eligible copy" and the
    # unit. grep's own status decides (issue #415), and 2+ is never "absent".
    grep -F -- "volume resume" >/dev/null <<<"$out"
    has_resume=$?
    [ "$has_resume" -le 1 ] || {
        echo "assert_retire_refused: $label: grep failed (rc=$has_resume) looking for \`volume resume\` -- inconclusive"
        return 1
    }
    if [ "$has_resume" -eq 0 ]; then
        if [ "$want_resume" != yes ]; then
            echo "assert_retire_refused: $label: the refusal offers \`volume resume\` for a \
volume that is already sealed. is_sealed_but_unconfirmed should be false here; that advice \
is only correct while confirm is still owed. Output:"
            printf '%s\n' "$out"
            return 1
        fi
    elif [ "$want_resume" = yes ]; then
        echo "assert_retire_refused: $label: refused (rc=$rc) but never offers \`volume \
resume\`, which is the cheapest correct first act for a sealed-but-unconfirmed volume \
(issue #276). Output:"
        printf '%s\n' "$out"
        return 1
    fi
    echo "$label: retire refused at Tier 3 despite --yes, naming \"$unit\" (volume resume \
offered: $want_resume)"
}

# The arm. MUST run after arm 3: `volume init` displaces the cartridge's
# previous volume and marks it 'erased' (ADR-0012), so by the time RLABEL4 is
# initialised every earlier arm's volume is gone and RLABEL4 genuinely holds
# the ONLY copy of unitA/unitB — which is the precondition the floor gates on.
step_tier3_floor_unconfirmed() {
    mt -f "$TAPE_DEV" rewind && mt -f "$TAPE_DEV" erase \
    && TCTL volume init "$RLABEL4" --device "$TAPE_DEV" \
    && interrupt_write_after_seal "$RLABEL4" \
    && assert_sealed_but_unconfirmed "$RLABEL4" \
    && assert_retire_refused "$RLABEL4" unitA yes \
    && TCTL volume resume "$RLABEL4" --device "$TAPE_DEV" \
    && assert_sealed "$RLABEL4" \
    && assert_retire_refused "$RLABEL4" unitA no
}

echo "gate: leg 4 — interrupt + resume (volume resume, issue #93)"
check resume_bot        step_resume_bot
check resume_midwrite   step_resume_midwrite
# ORDER MATTERS, and it did not used to (issue #164/#193). resume_verify and
# resume_restore both target RLABEL2, but step_resume_after_crash ERASES the
# cartridge and writes RLABEL3 over it -- so when the crash arm ran first,
# these two were pointed at a volume whose bytes were no longer on the tape.
# They passed anyway, for a reason that flatters nobody: the gate stages once
# and writes the same staged slices to every arm, so RLABEL2 and RLABEL3 hold
# BYTE-IDENTICAL ciphertext at identical positions, and a verify that never
# checked which tape was loaded could not tell them apart. That is precisely
# the defect issue #164 describes -- "records a passed verification for the
# wrong volume" -- and the gate was depending on it. Corroboration at contact
# now refuses, correctly, so these run while RLABEL2 is still the loaded tape.
check resume_verify     step_resume_verify
check resume_restore    step_resume_restore
check resume_after_crash step_resume_after_crash
check tier3_floor_unconfirmed step_tier3_floor_unconfirmed

# cfg_names_device_tape <device>: 0 if an uncommented `device_tape = ...` line
# in $CFG names "<device>", 1 if none does, 2 if $CFG could not be read.
# Issue #415: both by-id steps below asserted the by-id spelling ABSENT with
# `grep ... "$CFG" | grep -qF ...`, which an unreadable $CFG (grep exit 2) or a
# first grep SIGPIPEd under pipefail satisfied as "absent". One routine, its
# own status, and each caller runs it first on the spelling the config DOES
# carry ($TAPE_DEV, written there by step_init) as the positive control.
cfg_names_device_tape() {
    local lines rc
    lines="$(grep -E '^[[:space:]]*device_tape[[:space:]]*=' "$CFG")"
    rc=$?
    [ "$rc" -le 1 ] || return 2
    grep -F -- "\"$1\"" >/dev/null <<<"$lines"
}
# The two by-id steps' shared config precondition: $CFG names $TAPE_DEV (the
# positive control) and does NOT name <by_id>.
by_id_absent_from_cfg() { # by_id_absent_from_cfg <by_id>
    local rc
    cfg_names_device_tape "$TAPE_DEV"
    rc=$?
    [ "$rc" -eq 0 ] || {
        echo "positive control: $CFG has no device_tape line naming $TAPE_DEV (rc=$rc) -- the absence check for $1 could not have matched either"
        return 1
    }
    cfg_names_device_tape "$1"
    rc=$?
    case "$rc" in
        0) echo "precondition: $CFG already names $1 as a device_tape -- string equality would carry the lookup"; return 1 ;;
        1) return 0 ;;
        *) echo "precondition: could not read $CFG (rc=$rc) -- inconclusive, not absent"; return 1 ;;
    esac
}

# ---------- by-id device spelling (issue #321, #313's acceptance) ----------
# CLAUDE.md tells the operator to name the drive by serial,
# /dev/tape/by-id/scsi-<serial>-nst, because /dev/nstN moves across reboots.
# Every other step here spells it $TAPE_DEV, which is also what the gate
# home's config says (device_tape = "$TAPE_DEV"), so string equality alone
# would carry every one of them and a canonicalising resolver was never
# exercised on tape. This step spells the SAME drive the other way and
# asserts the contact still gets its drive, its health row and its sweep.
#
# It verifies $RLABEL4, not $LABEL: after leg 4 the tape holds RLABEL4 (sealed
# by the resume in tier3_floor_unconfirmed, untouched by the refused retire)
# and $LABEL is erased. So this step depends on tier3_floor_unconfirmed. It
# runs BEFORE the journals leg, so the new contact is covered by
# log_page_sweep_complete / contacts_name_their_drive as well.
#
# Preconditions are failures, not skips: if the config already said the by-id
# path, string equality would carry the lookup and this would be green for
# the wrong reason. The "before" contact id makes sure the assertions are
# about THIS command's contact, never an earlier /dev/nstN verify's.
step_health_by_id_device() {
    local serial by_id before
    serial="$(tail -c +5 "/sys/class/scsi_tape/$(basename "$(readlink -f "$TAPE_DEV")")/device/vpd_pg80" 2>/dev/null | tr -d '\0' | sed 's/ *$//')"
    [ -n "$serial" ] || { echo "cannot read the gate drive's serial from sysfs"; return 1; }
    by_id="/dev/tape/by-id/scsi-${serial}-nst"
    [ -e "$by_id" ] || { echo "precondition: $by_id does not exist"; return 1; }
    [ "$(readlink -f "$by_id")" = "$(readlink -f "$TAPE_DEV")" ] || {
        echo "precondition: $by_id -> $(readlink -f "$by_id"), not $TAPE_DEV -> $(readlink -f "$TAPE_DEV")"
        return 1
    }
    [ "$by_id" != "$TAPE_DEV" ] || {
        echo "precondition: the gate was run with TAPECTL_GATE_TAPE=$by_id; this step needs the /dev/nstN spelling in config"
        return 1
    }
    by_id_absent_from_cfg "$by_id" || return 1
    before="$(python3 -c 'import sqlite3,sys; print(sqlite3.connect(sys.argv[1]).execute("SELECT COALESCE(MAX(id),0) FROM cartridge_contacts WHERE operation = '"'volume verify'"'").fetchone()[0])' "$HOME_DIR/tapectl.db")" \
        || { echo "could not read the last verify contact id"; return 1; }
    echo "by-id: $by_id -> $(readlink -f "$by_id") (config says $TAPE_DEV); last verify contact before: $before"
    verify_json "$RLABEL4" "$by_id" "$RUN/verify-by-id.json" || return 1
    python3 - "$HOME_DIR/tapectl.db" "$before" "$by_id" "$serial" <<'PYBYID'
import sqlite3, sys
db, before, by_id, want = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4]
c = sqlite3.connect(db)
cid = c.execute(
    "SELECT MAX(id) FROM cartridge_contacts WHERE operation = 'volume verify'").fetchone()[0]
assert cid is not None and cid > before, (
    f"positive control: no NEW 'volume verify' contact (max id {cid}, before {before}) -- "
    "the assertions below would be about an earlier contact")
device, closed_at, drive_id, serial = c.execute(
    """SELECT cc.device, cc.closed_at, cc.drive_id, d.serial
       FROM cartridge_contacts cc LEFT JOIN drives d ON d.id = cc.drive_id
       WHERE cc.id = ?""", (cid,)).fetchone()
bad = []
if device != by_id:
    bad.append(f"contact records device {device!r}, not the by-id spelling {by_id!r} -- "
               "the spelling never reached tapectl")
if closed_at is None:
    bad.append("contact did not close")
if drive_id is None:
    bad.append("drive_id is NULL -- the by-id spelling found no backend at contact open")
elif serial != want:
    bad.append(f"drive_id names serial {serial!r}, not the gate drive {want!r}")
n_health = c.execute("SELECT COUNT(*) FROM health_logs WHERE contact_id = ?", (cid,)).fetchone()[0]
if n_health < 1:
    bad.append("no health_logs row for this contact -- health collection did not find the "
               "backend for the by-id spelling (issue #313's shape)")
n_zero = c.execute(
    "SELECT COUNT(*) FROM log_page_journal WHERE contact_id = ? AND page_code = 0",
    (cid,)).fetchone()[0]
if n_zero < 1:
    bad.append("no log_page_journal page 0x00 row for this contact -- no sweep ran")
assert not bad, f"by-id verify contact {cid}:\n  " + "\n  ".join(bad)
print(f"contact {cid}: device {device}, drive {serial}, {n_health} health row(s), page 0x00 swept")
PYBYID
}
check health_by_id_device step_health_by_id_device

# ---------- by-id on the READ path (issue #320, #313's missing control) ----------
# Since #320 every read-path contact takes the post-command sweep, through
# the SAME backend lookup `volume write`/`volume resume` use
# (`health_backend`, #313's canonicalising match). `volume verify` resolves
# its backend a different way (`config::resolve_device`), so
# health_by_id_device above cannot see a regression in `health_backend` --
# before this step, reverting #313 to a raw string `==` left the whole gate
# green (#321's finding). This step is that control: a by-id `restore unit`
# gets no health row and no page-0x00 row the moment `health_backend` stops
# canonicalising.
#
# Same drive, same preconditions, same "before" discipline as
# health_by_id_device, and the same tape: RLABEL4 (sealed by the resume in
# tier3_floor_unconfirmed; the only copy of unitA). Runs before the journals
# leg, so its contact is also covered by log_page_sweep_complete.
step_health_by_id_restore() {
    local serial by_id before
    serial="$(tail -c +5 "/sys/class/scsi_tape/$(basename "$(readlink -f "$TAPE_DEV")")/device/vpd_pg80" 2>/dev/null | tr -d '\0' | sed 's/ *$//')"
    [ -n "$serial" ] || { echo "cannot read the gate drive's serial from sysfs"; return 1; }
    by_id="/dev/tape/by-id/scsi-${serial}-nst"
    [ -e "$by_id" ] || { echo "precondition: $by_id does not exist"; return 1; }
    [ "$(readlink -f "$by_id")" = "$(readlink -f "$TAPE_DEV")" ] || {
        echo "precondition: $by_id -> $(readlink -f "$by_id"), not $TAPE_DEV -> $(readlink -f "$TAPE_DEV")"
        return 1
    }
    [ "$by_id" != "$TAPE_DEV" ] || {
        echo "precondition: the gate was run with TAPECTL_GATE_TAPE=$by_id; this step needs the /dev/nstN spelling in config"
        return 1
    }
    by_id_absent_from_cfg "$by_id" || return 1
    before="$(python3 -c 'import sqlite3,sys; print(sqlite3.connect(sys.argv[1]).execute("SELECT COALESCE(MAX(id),0) FROM cartridge_contacts WHERE operation = '"'restore unit'"'").fetchone()[0])' "$HOME_DIR/tapectl.db")" \
        || { echo "could not read the last restore contact id"; return 1; }
    echo "by-id: $by_id -> $(readlink -f "$by_id") (config says $TAPE_DEV); last restore contact before: $before"
    TCTL restore unit --unit unitA --from "$RLABEL4" --to "$RUN/restored-by-id" --device "$by_id" \
        && diff -r "$SRC/unitA" "$RUN/restored-by-id" \
        || return 1
    python3 - "$HOME_DIR/tapectl.db" "$before" "$by_id" "$serial" <<'PYBYIDR'
import sqlite3, sys
db, before, by_id, want = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4]
c = sqlite3.connect(db)
cid = c.execute(
    "SELECT MAX(id) FROM cartridge_contacts WHERE operation = 'restore unit'").fetchone()[0]
assert cid is not None and cid > before, (
    f"positive control: no NEW 'restore unit' contact (max id {cid}, before {before}) -- "
    "the assertions below would be about an earlier contact")
device, closed_at, drive_id, serial = c.execute(
    """SELECT cc.device, cc.closed_at, cc.drive_id, d.serial
       FROM cartridge_contacts cc LEFT JOIN drives d ON d.id = cc.drive_id
       WHERE cc.id = ?""", (cid,)).fetchone()
bad = []
if device != by_id:
    bad.append(f"contact records device {device!r}, not the by-id spelling {by_id!r} -- "
               "the spelling never reached tapectl")
if closed_at is None:
    bad.append("contact did not close")
if drive_id is None:
    bad.append("drive_id is NULL -- the by-id spelling found no backend at contact open")
elif serial != want:
    bad.append(f"drive_id names serial {serial!r}, not the gate drive {want!r}")
health = c.execute(
    "SELECT operation FROM health_logs WHERE contact_id = ?", (cid,)).fetchall()
if len(health) != 1:
    bad.append(f"{len(health)} health_logs rows for this contact, want exactly 1 -- "
               + ("the read path's health lookup did not find the backend for the by-id "
                  "spelling (issue #313's shape, on the path #320 added)" if not health
                  else "a contact was read twice (ADR-0013's once-per-contact rule)"))
elif health[0][0] != "restore":
    bad.append(f"health_logs.operation is {health[0][0]!r}, not 'restore' (issue #320)")
n_zero = c.execute(
    "SELECT COUNT(*) FROM log_page_journal WHERE contact_id = ? AND page_code = 0",
    (cid,)).fetchone()[0]
if n_zero != 1:
    bad.append(f"{n_zero} log_page_journal page 0x00 rows for this contact, want exactly 1 -- "
               + ("no sweep ran" if n_zero == 0 else "more than one sweep ran"))
assert not bad, f"by-id restore contact {cid}:\n  " + "\n  ".join(bad)
print(f"contact {cid}: device {device}, drive {serial}, one 'restore' health row, page 0x00 swept once")
PYBYIDR
}
check health_by_id_restore step_health_by_id_restore

# ---------- journals leg: the forensics journals (issue #319) ----------
# Migrations 022 (`mam_journal`, #297) and 023 (`log_page_journal`, #298)
# journal every MAM read and every sg_logs page read verbatim, against the
# contact that took it (ADR-0013). The Rust tests exercise the capture over
# fixtures; only a real run proves the call sites still fire, still attribute
# the row to a contact, and still read each page exactly once. Before this
# leg those facts were verified once, by hand, and nothing re-checked them.
#
# Placed here because the gate home's catalog is FINAL after leg 4: every
# init/write/resume/verify/restore this gate performs has happened. rust_e2e
# below uses its own home and adds nothing to this DB.
#
# Every snippet takes the DB path as argv[1] and nothing else, so each can be
# replayed offline against a copy of a gate DB with a defect injected -- the
# negative control for this leg is run that way, not on tape. And every one
# asserts a NON-EMPTY input first (the leakscan lesson, issue #275): a check
# that only asserts absence cannot tell "found nothing" from "searched
# nothing".
#
# The sweep runs once per contact at the END of the command (health
# collection after the session), so a SIGKILLed write (resume_after_crash)
# never closes its contact and never sweeps. That is why the spine query
# below takes CLOSED contacts only -- and why it is driven from
# cartridge_contacts, not from the journal: a journal-driven loop cannot see
# a contact that has zero rows.
step_log_page_sweep_complete() {
    python3 - "$HOME_DIR/tapectl.db" <<'PYLP_SWEEP'
import sqlite3, sys
c = sqlite3.connect(sys.argv[1])
# "restore unit" since issue #320: every read-path contact takes the same
# post-command sweep. "volume init" since issue #339: ADR-0013's 2026-09-23
# evening amendment rules that EVERY contact sweeps, init included -- the
# rehearsal before it showed every init contact on the real drive with zero
# journal rows. The other read paths (raw-volume, rebuild, identify,
# read-slices, compact-read) are not exercised by this gate, and the positive
# control below requires at least one contact of EACH listed operation -- so
# listing one the gate never runs would fail for the wrong reason.
OPS = ("volume init", "volume write", "volume resume", "volume verify", "restore unit")
contacts = c.execute(
    f"""SELECT id, operation FROM cartridge_contacts
        WHERE operation IN ({",".join("?" * len(OPS))}) AND closed_at IS NOT NULL
        ORDER BY id""", OPS).fetchall()
# Positive control: the gate performs at least one of each, so an empty or
# partial set means the query (or the gate) is broken, not the journal clean.
seen = {op for _, op in contacts}
assert len(contacts) >= len(OPS) and seen == set(OPS), (
    f"positive control: expected >={len(OPS)} closed contacts covering every one of {OPS}; "
    f"got {len(contacts)} covering {sorted(seen)} -- the check cannot see what it asserts about")
bad = []
for cid, op in contacts:
    rows = c.execute(
        "SELECT page_code, ok, raw FROM log_page_journal WHERE contact_id = ?", (cid,)).fetchall()
    zero = [r for r in rows if r[0] == 0]
    if not zero:
        bad.append(f"contact {cid} ({op}): no page 0x00 row (journal rows for it: {len(rows)})")
        continue
    _, ok, raw = zero[0]
    raw = bytes(raw) if raw is not None else b""
    if ok != 1 or len(raw) < 4:
        bad.append(f"contact {cid} ({op}): page 0x00 row ok={ok}, raw {len(raw)} bytes -- no page list to compare")
        continue
    page, sub, length = raw[0] & 0x3F, raw[1], int.from_bytes(raw[2:4], "big")
    if page != 0 or sub != 0 or 4 + length != len(raw):
        bad.append(f"contact {cid} ({op}): page 0x00 raw did not parse (page=0x{page:02x} "
                   f"subpage=0x{sub:02x} length={length} raw={len(raw)} bytes: {raw.hex()})")
        continue
    listed = set(raw[4:]) - {0}
    read = {r[0] for r in rows} - {0}
    if listed != read:
        bad.append(f"contact {cid} ({op}): page 0x00 lists {sorted(f'{p:02x}' for p in listed)} "
                   f"but the journal read {sorted(f'{p:02x}' for p in read)}; "
                   f"missing {sorted(f'{p:02x}' for p in listed - read)}, "
                   f"unlisted {sorted(f'{p:02x}' for p in read - listed)}")
assert not bad, "log-page sweep incomplete:\n  " + "\n  ".join(bad)
print(f"{len(contacts)} closed init/write/resume/verify/restore-unit contacts, each swept exactly the pages its own 0x00 listed")
PYLP_SWEEP
}

# ADR-0013 "Two hazards": TapeAlert (0x2E) clears on read, so a second read
# inside one contact can return zeros and destroy the first read's evidence.
# `sweep` promises each page at most once per contact; this is that promise.
step_log_page_read_once() {
    python3 - "$HOME_DIR/tapectl.db" <<'PYLP_ONCE'
import sqlite3, sys
c = sqlite3.connect(sys.argv[1])
n = c.execute("SELECT COUNT(*) FROM log_page_journal").fetchone()[0]
assert n > 0, "positive control: log_page_journal is EMPTY -- 'no duplicates' would mean 'searched nothing'"
dups = c.execute(
    """SELECT contact_id, trigger, printf('0x%02x', page_code), subpage_code, COUNT(*)
       FROM log_page_journal GROUP BY contact_id, page_code, subpage_code
       HAVING COUNT(*) > 1 ORDER BY contact_id, page_code""").fetchall()
assert not dups, (
    "a log page was read more than once inside one contact (read-to-clear hazard, "
    "ADR-0013) -- (contact_id, trigger, page, subpage, reads):\n  "
    + "\n  ".join(map(str, dups)))
# One ROW is one sg_logs process; it is one LOG SENSE only if the argv pins
# the allocation length. Without --maxlen, sg_logs sends a 4-byte probe first
# (`man sg_logs`), a second command at a page that may clear when read (#328).
import json
multi = [(rid, cid, argv) for rid, cid, argv in c.execute(
    "SELECT id, contact_id, tool_argv FROM log_page_journal ORDER BY id")
    if not any(a.startswith("--maxlen=") for a in json.loads(argv))]
assert not multi, (
    "log page reads without --maxlen are two LOG SENSE commands each (#328) -- (row, contact, argv):\n  "
    + "\n  ".join(map(str, multi[:10])))
print(f"{n} log_page_journal rows, no (contact, page, subpage) read twice, every read one LOG SENSE")
PYLP_ONCE
}

# "Capture everything verbatim now, parse it later" (ADR-0013) is only true
# if the bytes are there. Also pins that each raw response IS the page it is
# filed under (byte 0 low six bits = page code), so a mis-filed capture fails.
# tapectl_version is NOT NULL in the schema; asserted anyway as the check
# the issue names, and so a schema change cannot quietly drop it.
step_log_page_raw_kept() {
    python3 - "$HOME_DIR/tapectl.db" <<'PYLP_RAW'
import sqlite3, sys
c = sqlite3.connect(sys.argv[1])
rows = c.execute(
    """SELECT id, contact_id, trigger, page_code, subpage_code, ok, raw, tapectl_version
       FROM log_page_journal ORDER BY id""").fetchall()
assert rows, "positive control: log_page_journal is EMPTY -- nothing to assert raw bytes about"
bad = []
for rid, cid, trig, page, sub, ok, raw, ver in rows:
    tag = f"row {rid} (contact {cid}, {trig}, page 0x{page:02x})"
    if cid is None:
        bad.append(f"{tag}: contact_id is NULL -- the read is attributed to no contact")
    if not ver:
        bad.append(f"{tag}: tapectl_version is {ver!r}")
    if ok == 1:
        if raw is None or len(raw) == 0:
            bad.append(f"{tag}: ok=1 but raw is {'NULL' if raw is None else 'empty'}")
        elif (bytes(raw)[0] & 0x3F) != page:
            bad.append(f"{tag}: raw's own page code is 0x{bytes(raw)[0] & 0x3F:02x}, filed as 0x{page:02x}")
ok_rows = sum(1 for r in rows if r[5] == 1)
assert ok_rows > 0, f"positive control: none of {len(rows)} log_page_journal rows is ok=1"
assert not bad, "log_page_journal rows missing what they must keep:\n  " + "\n  ".join(bad)
print(f"{len(rows)} log_page_journal rows ({ok_rows} ok=1): raw kept, attributed, versioned")
PYLP_RAW
}

# mam_journal: every read attributed to the contact that took it, and every
# successful read's stdout kept. Read-path rows are chosen by `hook`, not
# `trigger` -- hook is the migration's fixed call-site name, trigger is free
# text. The two read-path hooks (check_read_contact, loaded_medium_serial)
# run inside ONE already-open contact, so a NULL there is never the
# "refused before the contact opened" case migration 022 allows for.
# Each row must also point at a contact whose operation IS its trigger: a
# row attributed to the wrong contact is worse than an unattributed one.
step_mam_journal_attributed() {
    python3 - "$HOME_DIR/tapectl.db" <<'PYMAM'
import sqlite3, sys
c = sqlite3.connect(sys.argv[1])
READ_HOOKS = ("check_read_contact", "loaded_medium_serial")
rows = c.execute(
    """SELECT m.id, m.contact_id, m.trigger, m.hook, m.ok, m.raw, m.tapectl_version,
              cc.operation
       FROM mam_journal m LEFT JOIN cartridge_contacts cc ON cc.id = m.contact_id
       ORDER BY m.id""").fetchall()
assert rows, "positive control: mam_journal is EMPTY -- nothing to assert attribution about"
hooks = {r[3] for r in rows}
assert set(READ_HOOKS) <= hooks, (
    f"positive control: the gate runs verify and restore, so both read-path hooks "
    f"{READ_HOOKS} must appear; got hooks {sorted(hooks)}")
bad = []
for rid, cid, trig, hook, ok, raw, ver, op in rows:
    tag = f"row {rid} ({trig} / {hook}, contact {cid})"
    if hook in READ_HOOKS and cid is None:
        bad.append(f"{tag}: read-path MAM read has NULL contact_id")
    if cid is not None and op != trig:
        bad.append(f"{tag}: points at a contact whose operation is {op!r}, not {trig!r}")
    if ok == 1 and (raw is None or len(raw) == 0):
        bad.append(f"{tag}: ok=1 but raw is {'NULL' if raw is None else 'empty'}")
    if not ver:
        bad.append(f"{tag}: tapectl_version is {ver!r}")
assert not bad, "mam_journal rows not attributed / not kept:\n  " + "\n  ".join(bad)
n_read = sum(1 for r in rows if r[3] in READ_HOOKS)
print(f"{len(rows)} mam_journal rows ({n_read} read-path), all attributed and kept; hooks {sorted(hooks)}")
PYMAM
}

echo "gate: journals leg — forensics journals (issue #319)"
# Every contact names the drive it was made with (issue #314), and that drive
# is THIS gate's drive: its serial is read from sysfs for $TAPE_DEV and
# compared by value, so a contact attributed to the wrong drive fails too.
step_contacts_name_their_drive() {
    local want
    want="$(tail -c +5 "/sys/class/scsi_tape/$(basename "$(readlink -f "$TAPE_DEV")")/device/vpd_pg80" 2>/dev/null | tr -d '\0' | sed 's/ *$//')"
    python3 - "$HOME_DIR/tapectl.db" "$want" <<'PYDRIVE'
import sqlite3, sys
c = sqlite3.connect(sys.argv[1]); want = sys.argv[2]
assert want, "positive control: could not read the gate drive's own serial from sysfs"
rows = c.execute(
    """SELECT c.id, c.operation, c.outcome, d.serial
       FROM cartridge_contacts c LEFT JOIN drives d ON d.id = c.drive_id
       ORDER BY c.id""").fetchall()
ops = {r[1] for r in rows}
need = {"volume init", "volume write", "volume verify", "volume resume", "restore unit"}
assert need <= ops, f"positive control: expected contacts for every one of {sorted(need)}; got {sorted(ops)}"
bad = [r for r in rows if r[3] != want]
assert not bad, (
    f"contacts not attributed to the gate drive {want!r} -- (id, operation, outcome, drive serial):\n  "
    + "\n  ".join(map(str, bad)))
print(f"{len(rows)} contacts across {len(ops)} operations, every one names drive {want}")
PYDRIVE
}

check log_page_sweep_complete step_log_page_sweep_complete
check log_page_read_once      step_log_page_read_once
check log_page_raw_kept       step_log_page_raw_kept
check mam_journal_attributed  step_mam_journal_attributed
check contacts_name_their_drive step_contacts_name_their_drive

# ---------- the first non-zero TapeAlert must be SEEN (issue #340) ----------
# Every page 0x2E ever journalled -- every capture, every rehearsal, every
# gate run -- has been all-zero, so `report health`'s `!! TAPE ALERT` line
# has never fired and nothing would notice if it could not. This step makes
# it fire on purpose. A consistent COPY of the gate catalog (sqlite's backup
# API -- the DB is WAL mode, a plain cp can miss <db>-wal), one real closed
# contact's 0x2E decode with two flags flipped to 1 and its health row's
# tape_alerts set to 2, and `report health` run against a second home built
# around that copy. The gate catalog itself is never modified.
#
# Positive control first: the line names THAT contact, the gate drive's
# serial, the contact's cartridge and flags 20 and 36 by number and name.
# Only then the negative, on the unmodified catalog: no sighting line -- and
# it looked, proved by the listing carrying alerts=0 readings and the DB
# carrying decoded 0x2E rows. If the unmodified catalog already carries a
# raised flag, the seed refuses and says so: that is #340's first sighting,
# to be read, not a defect in this step.
step_tape_alert_surfaced() {
    local seeded="$RUN/home-tape-alert" want picked cid barcode
    want="$(tail -c +5 "/sys/class/scsi_tape/$(basename "$(readlink -f "$TAPE_DEV")")/device/vpd_pg80" 2>/dev/null | tr -d '\0' | sed 's/ *$//')"
    [ -n "$want" ] || { echo "cannot read the gate drive's serial from sysfs"; return 1; }
    mkdir -p "$seeded"
    cp "$CFG" "$seeded/config.toml"
    # Prints "<contact id> <barcode>" for the contact it seeded.
    picked="$(python3 - "$HOME_DIR/tapectl.db" "$seeded/tapectl.db" <<'PYSEED'
import sqlite3, sys
src, dst = sys.argv[1], sys.argv[2]
s = sqlite3.connect(src); d = sqlite3.connect(dst)
s.backup(d)   # WAL-safe: a plain cp can miss <db>-wal and hand back a stale copy
s.close()
rows = d.execute(
    """SELECT j.id, j.contact_id, j.decoded, c.barcode, j.raw
       FROM log_page_journal j
       JOIN cartridge_contacts cc ON cc.id = j.contact_id
       LEFT JOIN cartridges c ON c.id = cc.cartridge_id
       WHERE j.page_code = 0x2e AND j.ok = 1 AND j.decoded IS NOT NULL
         AND cc.closed_at IS NOT NULL
         AND EXISTS (SELECT 1 FROM health_logs h WHERE h.contact_id = j.contact_id)
       ORDER BY j.id DESC""").fetchall()
assert rows, "positive control: no closed contact with both a decoded 0x2E row and a health row -- nothing to seed"
# The premise: every 0x2E ever read is all-zero. If that stopped being true,
# say so -- it is issue #340's first sighting, not a defect in this step.
raised = [(jid, cid) for jid, cid, dec, _, _ in rows
          if any(l.rstrip().endswith(": 1") for l in dec.splitlines())]
nonzero = d.execute("SELECT id, contact_id, tape_alerts FROM health_logs WHERE tape_alerts > 0").fetchall()
assert not raised and not nonzero, (
    "the UNMODIFIED gate catalog already carries a non-zero TapeAlert -- issue #340's first "
    f"sighting, on mhvtl: journal rows {raised}, health rows {nonzero}. Read it before touching this step.")
jid, cid, dec, barcode, raw = rows[0]
flipped = dec.replace("  Cleaning required: 0", "  Cleaning required: 1", 1) \
             .replace("  Drive temperature: 0", "  Drive temperature: 1", 1)
assert flipped.count(": 1") == 2, "the decode did not carry both flag lines to flip"
# #303 reads the flags from the journalled page bytes when they parse, so the
# bytes must say the same thing: set the value bit of parameters 20 and 36.
if raw is not None:
    page = bytearray(raw)
    assert page[0] & 0x3f == 0x2e, "the journalled raw page is not 0x2E"
    end, at, hit = 4 + int.from_bytes(page[2:4], "big"), 4, []
    while at + 4 <= end:
        code, length = int.from_bytes(page[at:at + 2], "big"), page[at + 3]
        if code in (20, 36) and length:
            page[at + 4] |= 0x01
            hit.append(code)
        at += 4 + length
    assert hit == [20, 36], f"the raw page did not carry parameters 20 and 36 to flip: {hit}"
    raw = bytes(page)
d.execute("UPDATE log_page_journal SET decoded = ?, raw = ? WHERE id = ?", (flipped, raw, jid))
d.execute("UPDATE health_logs SET tape_alerts = 2 WHERE contact_id = ?", (cid,))
d.commit()
print(cid, barcode if barcode is not None else "-")
PYSEED
)" || { echo "seeding the copy failed"; return 1; }
    read -r cid barcode <<<"$picked"
    echo "seeded copy: contact $cid (cartridge $barcode) now shows TapeAlert flags 20 and 36"

    # Positive control: the seeded copy is reported, with THIS contact's
    # identifiers -- not merely some line carrying the prefix.
    "$BIN" --home "$seeded" --config "$seeded/config.toml" report health > "$RUN/report-health-seeded.txt" \
        || { echo "report health against the seeded copy failed"; return 1; }
    python3 - "$RUN/report-health-seeded.txt" "$cid" "$want" "$barcode" <<'PYPOS'
import sys
out = open(sys.argv[1]).read(); cid, want, barcode = sys.argv[2:5]
lines = [l for l in out.splitlines() if l.startswith("!! TAPE ALERT ")]
assert len(lines) == 1, f"want exactly one sighting line for the one seeded contact, got {len(lines)}:\n{out}"
l = lines[0]
bad = []
for needle, what in ((f"contact={cid} ", "the seeded contact id"),
                     (f"drive={want} ", "the gate drive's serial"),
                     (f"cartridge={barcode} ", "the contact's cartridge"),
                     ("flags=20,36 (Cleaning required; Drive temperature)", "both flipped flags, numbered and named"),
                     ("source=log_page_journal#", "the journal as the source")):
    if needle not in l:
        bad.append(f"missing {what!r}: {needle!r}")
if "DISAGREES" in l:
    bad.append("the seeded count (2) matches the two flags, so no disagreement may be reported")
assert not bad, "the sighting line is wrong:\n  " + "\n  ".join(bad) + f"\n  line: {l}"
assert any("read-to-clear" in x for x in out.splitlines()), "the read-to-clear note is missing"
print(f"positive control: {l}")
PYPOS
    [ $? -eq 0 ] || return 1

    # Negative assertion, only now: the unmodified catalog prints no sighting
    # -- and it LOOKED: alerts=0 readings in the listing, 0x2E decodes in the DB.
    TCTL report health > "$RUN/report-health-unmodified.txt" \
        || { echo "report health against the gate catalog failed"; return 1; }
    python3 - "$RUN/report-health-unmodified.txt" "$HOME_DIR/tapectl.db" <<'PYNEG'
import sqlite3, sys
out = open(sys.argv[1]).read(); c = sqlite3.connect(sys.argv[2])
n_2e = c.execute("SELECT COUNT(*) FROM log_page_journal WHERE page_code = 0x2e AND ok = 1 AND decoded IS NOT NULL").fetchone()[0]
assert n_2e > 0, "positive control: no decoded 0x2E row to have looked at"
clean = [l for l in out.splitlines() if " alerts=0" in l]
assert clean, f"positive control: no recorded-and-clean reading (alerts=0) in the listing:\n{out}"
hits = [l for l in out.splitlines() if l.startswith("!! TAPE ALERT ") or "read-to-clear" in l]
assert not hits, "the unmodified catalog reports a TapeAlert sighting:\n  " + "\n  ".join(hits)
print(f"negative: {n_2e} decoded 0x2E rows, {len(clean)} alerts=0 readings listed, no sighting line")
PYNEG
}
check tape_alert_surfaced step_tape_alert_surfaced
check feed_ratio_recorded step_feed_ratio_recorded
check st_stats_recorded step_st_stats_recorded

# ---------- File 0 unreadable is not consent (issue #400): NOT on mhvtl ----------
# #400's refusal (an unreadable File 0 is Blank only when the tape proves it)
# cannot be exercised here: mhvtl fabricates no File 0 read error. Measured
# 2026-10-07 on this gate's LTO-8 emulation: File 0 rewritten as one 64 KiB
# block (`mt setblk 65536` + dd) is answered in full by a fixed 512 KiB read
# -- no incorrect-length error -- and mhvtl injects no medium error. The
# refusal arms are proven by the ungated FakeTape tests in src/volume/
# (EIO with and without blank evidence, --force, a chip serial bound to a live
# volume); the real drive is the only medium that could show it end to end.

# ---------- #444: cartridge erase, then volume init with no --force ----------
# ADR-0012 2026-10-15 item 57 (#434): a short `cartridge erase` that this
# catalog verified proves the EMPTY File 0 it leaves blank, so `volume init`
# needs no --force; a catalog with no such record still refuses. first-run.sh
# step 13 depends on exactly this (f54964c removed its --force retry), and until
# #444 no repo harness ran it: the gate and the suites blank with `mt erase`.
# Three throwaway homes on the gate's cartridge: P writes and seals a volume;
# C, which knows nothing of it, erases it with typed consent; D, another
# stranger, must still be refused at init (run BEFORE C's init writes File 0,
# or D would meet C's identity instead of the empty File 0). The chip proof
# cannot run here: mhvtl's MAM has no lifetime attributes.
step_erase_then_init_no_force() {
    local w="$RUN/erase-proof" h out rc word
    rm -rf "$w"; mkdir -p "$w/src/u1"; echo "erase proof $CANARY" >"$w/src/u1/f.txt"
    mt -f "$TAPE_DEV" rewind && mt -f "$TAPE_DEV" erase || { echo "mt erase failed"; return 1; }
    for h in p c d; do
        mkdir -p "$w/$h/staging"
        # P stages and writes, which needs an escrow recipient (ADR-0005): a
        # throwaway one, its secret going only to this run's log.
        local escrow="--no-escrow"; [ "$h" = p ] && escrow=""
        # shellcheck disable=SC2086 # empty or one word, by construction
        "$BIN" --home "$w/$h" init --operator "gate-$h" $escrow >/dev/null 2>&1 \
            && "$BIN" --home "$w/$h" drive add --name mhvtl --device-tape "$TAPE_DEV" --generation LTO-8 >/dev/null 2>&1 \
            && "$BIN" --home "$w/$h" config set staging.directory "$w/$h/staging" >/dev/null 2>&1 \
            || { echo "home $h: init/drive add/config set failed"; return 1; }
    done
    "$BIN" --home "$w/p" volume init EP-0001 --device "$TAPE_DEV" >"$w/p-init.out" 2>&1 \
        && "$BIN" --home "$w/p" quick-archive "$w/src/u1" --tenant gate-p --volume EP-0001 --device "$TAPE_DEV" --yes >"$w/p-write.out" 2>&1 \
        || { echo "home P could not write and seal EP-0001:"; tail -n 20 "$w/p-init.out" "$w/p-write.out"; return 1; }
    # The dry run names what is on the tape; its JSON carries the consent word.
    out="$("$BIN" --home "$w/c" cartridge erase --device "$TAPE_DEV" --dry-run --json 2>"$w/c-dry.err")"; rc=$?
    [ "$rc" = 0 ] || { echo "dry run exit $rc"; cat "$w/c-dry.err"; return 1; }
    word="$(python3 -c 'import json,sys; print(json.load(sys.stdin).get("confirm_label") or "")' <<<"$out")"
    [ "$word" = EP-0001 ] || { echo "dry run's confirm_label is \"$word\", want EP-0001 (File 0's label)"; return 1; }
    "$BIN" --home "$w/c" cartridge erase --device "$TAPE_DEV" --confirm-label EP-0001 >"$w/c-erase.out" 2>&1 \
        || { echo "cartridge erase failed:"; tail -20 "$w/c-erase.out"; return 1; }
    # Negative control: a catalog that did not erase this cartridge has no proof.
    "$BIN" --home "$w/d" volume init EP-0005 --device "$TAPE_DEV" >"$w/d-init.out" 2>&1; rc=$?
    [ "$rc" = 2 ] || { echo "home D's init exit $rc, want 2 (EMPTY File 0, no proof):"; tail -10 "$w/d-init.out"; return 1; }
    grep -F -- "--force" "$w/d-init.out" >/dev/null || { echo "home D's refusal does not name --force"; return 1; }
    # The proof: C's own verified erase. No --force anywhere.
    "$BIN" --home "$w/c" volume init EP-0004 --device "$TAPE_DEV" >"$w/c-init.out" 2>&1; rc=$?
    [ "$rc" = 0 ] || { echo "home C's init exit $rc, want 0:"; tail -10 "$w/c-init.out"; return 1; }
    grep -F "blank proven by this catalog" "$w/c-init.out" >/dev/null \
        || { echo "home C's init did not name the verified-erase proof:"; cat "$w/c-init.out"; return 1; }
    "$BIN" --home "$w/c" volume identify --device "$TAPE_DEV" 2>&1 | grep -F "EP-0004" >/dev/null \
        || { echo "File 0 does not name EP-0004 after C's init"; return 1; }
    mt -f "$TAPE_DEV" rewind && mt -f "$TAPE_DEV" erase
}
check erase_then_init_no_force step_erase_then_init_no_force

# ---------- heir_short_*: a tape the early warning ended, read by the heir (1.6.0; #420) ----------
# ADR-0012 item 70: the gate is the heir path's only test with the real age, tar,
# mt and sha256sum, and a tape may end before its plan (volume-format-v3.md): the drive's early
# warning comes in data slice K, the write finishes it and seals short at K+1; the unit it
# interrupts is held as a range (its slices up to K) and a unit planned after K gets no row (item 288). mhvtl's own early warning comes only near its 500 MB end, where it then
# loses what is written (tests/mhvtl_e2e.rs proves that case), so the tape here is made
# with the test hook TAPECTL_TEST_EARLY_WARNING_AT: from 30 MiB written, the write store answers as
# st does past a warning (store::TapeStore::open), and everything written reads back. A throwaway
# home, alice's three units in plan order: sa-whole (whole), sb-cut (40 MiB in 4 MiB slices, the
# warning inside them: one file larger than a slice, so a run of slices; the unit the warning
# interrupts, held as a range) and sc-after (planned after it: in the tape's plan and so named by
# its MANIFEST, but with no `writes` row). Then the RESTORE.sh on that tape: --info and --verify
# say where it ended, a whole unit restores byte for byte, and the other two are NOT restored as
# whole units (T4.16, Layout 3: the script restores what a tape holds whole, leaves the rest out
# and lists it; it never fills a lost slice with zeros). The names sb-cut and sc-after are kept.
SS="$RUN/short-seal"
SS_LABEL="SS-0001"
SSC() { "$BIN" --home "$SS/home" "$@"; }
# RESTORE.sh of the short tape (read off it into $SS/heir), against this gate's drive.
ss_sh() { (cd "$SS/heir" && TAPE_DEVICE="$TAPE_DEV" ./RESTORE.sh "$@"); }
# volumes.ended_after of the short tape: K, the last file before its seal.
ss_ended_after() {
    python3 - "$SS/home/tapectl.db" "$SS_LABEL" <<'PY'
import sqlite3, sys
row = sqlite3.connect(sys.argv[1]).execute(
    "SELECT ended_after FROM volumes WHERE label = ?", (sys.argv[2],)).fetchone()
assert row is not None, f"no volumes row for {sys.argv[2]}"
assert row[0] is not None, f"{sys.argv[2]}: volumes.ended_after is NULL, the tape sealed full"
print(row[0])
PY
}
step_heir_short_seal() {
    local u k verify
    rm -rf "$SS"; mkdir -p "$SS/src/sa-whole/nested" "$SS/src/sb-cut" "$SS/src/sc-after" "$SS/home/staging" "$SS/heir"
    echo "short-seal whole unit $CANARY" >"$SS/src/sa-whole/whole.txt"
    head -c 700000 /dev/urandom >"$SS/src/sa-whole/nested/blob.bin"
    head -c 41943040 /dev/urandom >"$SS/src/sb-cut/forty-megs.bin"
    echo "planned after the cut" >"$SS/src/sc-after/after.txt"
    mt -f "$TAPE_DEV" rewind && mt -f "$TAPE_DEV" erase || { echo "mt erase failed"; return 1; }
    # init without --no-escrow registers a throwaway escrow recipient (ADR-0005),
    # its secret going only to this run's log.
    SSC init --operator gate-s \
        && SSC drive add --name mhvtl --device-tape "$TAPE_DEV" --generation LTO-8 \
        && SSC config set staging.directory "$SS/home/staging" \
        && SSC config set defaults.slice_size 4M \
        && SSC tenant add alice \
        || { echo "home: init/drive add/config set/tenant add failed"; return 1; }
    for u in sa-whole sb-cut sc-after; do
        SSC unit init "$SS/src/$u" --tenant alice --name "$u" && SSC snapshot create "$u" && SSC stage create "$u" \
            || { echo "unit $u: init/snapshot/stage failed"; return 1; }
    done
    SSC volume init "$SS_LABEL" --device "$TAPE_DEV" || return 1
    TAPECTL_TEST_EARLY_WARNING_AT=31457280 SSC volume write "$SS_LABEL" --device "$TAPE_DEV" --yes >"$SS/write.out" 2>&1 \
        || { tail -n 30 "$SS/write.out"; echo "volume write exited non-zero: a short seal is a success"; return 1; }
    cat "$SS/write.out"
    grep -F "TAPECTL_TEST_EARLY_WARNING_AT=31457280 is set" "$SS/write.out" >/dev/null \
        || { echo "the write did not announce the test hook"; return 1; }
    grep -F "volume \"$SS_LABEL\" sealed short:" "$SS/write.out" >/dev/null \
        || { echo "the write did not say it sealed short"; return 1; }
    grep -F "Held here as a range, the rest of each still staged: sb-cut (slice" "$SS/write.out" >/dev/null \
        || { echo "the write did not name sb-cut as held here as a range"; return 1; }
    grep -F "Planned and not written (still staged): sc-after." "$SS/write.out" >/dev/null \
        || { echo "the write did not name sc-after as planned and not written"; return 1; }
    if grep -F "Cut (not on this tape" "$SS/write.out" >/dev/null; then
        echo "the write still speaks of cut units (item 288)"; return 1
    fi
    k="$(ss_ended_after)" || return 1
    # The rows: sa-whole a copy, sb-cut a range, sc-after none (item 288), and K one of sb-cut's
    # slices before its last (else the hook's figure no longer lands where this leg says).
    python3 - "$SS/home/tapectl.db" "$SS_LABEL" "$k" <<'PY' || return 1
import sqlite3, sys
db, label, k = sys.argv[1], sys.argv[2], int(sys.argv[3])
c = sqlite3.connect(db)
rows = dict(c.execute(
    "SELECT u.name, w.status FROM writes w JOIN volumes v ON v.id = w.volume_id "
    "JOIN stage_sets ss ON ss.id = w.stage_set_id JOIN snapshots s ON s.id = ss.snapshot_id "
    "JOIN units u ON u.id = s.unit_id WHERE v.label = ?", (label,)).fetchall())
assert rows == {"sa-whole": "completed", "sb-cut": "range"}, rows
# sc-after: no writes row, and its stage set stays staged with sb-cut's (the next write takes them).
st = dict(c.execute(
    "SELECT u.name, ss.status FROM stage_sets ss JOIN snapshots s ON s.id = ss.snapshot_id "
    "JOIN units u ON u.id = s.unit_id").fetchall())
assert st["sc-after"] == "staged" and st["sb-cut"] == "staged", st
pos = [p for (p,) in c.execute(
    "SELECT CAST(wp.position AS INTEGER) FROM write_positions wp JOIN writes w ON w.id = wp.write_id "
    "JOIN volumes v ON v.id = w.volume_id JOIN stage_slices sl ON sl.id = wp.stage_slice_id "
    "JOIN stage_sets ss ON ss.id = w.stage_set_id JOIN snapshots s ON s.id = ss.snapshot_id "
    "JOIN units u ON u.id = s.unit_id WHERE v.label = ? AND u.name = 'sb-cut' ORDER BY sl.slice_number",
    (label,))]
assert pos and pos[0] <= k < pos[-1], f"K = {k} is not inside sb-cut's slices {pos}"
print(f"{label}: ended_after = {k}; sb-cut's slices at {pos}; rows {rows}")
PY
    # tapectl's own full readback of the short tape, before the heir reads it.
    verify="$(SSC volume verify "$SS_LABEL" --device "$TAPE_DEV" --json)" \
        || { printf '%s\n' "$verify"; echo "volume verify of the short tape failed"; return 1; }
    python3 -c 'import json,sys; d=json.loads(sys.argv[1]); assert d.get("failed",1)==0 and d.get("passed",0)>0, d' "$verify" \
        || return 1
    mt -f "$TAPE_DEV" rewind && mt -f "$TAPE_DEV" fsf 2 \
        && dd if="$TAPE_DEV" bs=512k 2>/dev/null | tr -d '\0' >"$SS/heir/RESTORE.sh" \
        && chmod +x "$SS/heir/RESTORE.sh" && bash -n "$SS/heir/RESTORE.sh" \
        || { echo "cannot read RESTORE.sh off the short tape"; return 1; }
    cmp -s "$SS/heir/RESTORE.sh" "$GATE_REPO/src/volume/heir/RESTORE.sh" \
        || { echo "the RESTORE.sh on the short tape is not src/volume/heir/RESTORE.sh"; return 1; }
}
step_heir_short_info() {
    local k out
    k="$(ss_ended_after)" || return 1
    out="$(ss_sh --info 2>&1)" || { printf '%s\n' "$out"; return 1; }
    printf '%s\n' "$out"
    # tests/heir_restore.rs info_on_a_short_tape_says_where_it_ended_and_names_the_planned_files_not_on_it
    grep -E "^Tape: $SS_LABEL +uuid " >/dev/null <<<"$out" || { echo "heir_short_info: not volume $SS_LABEL"; return 1; }
    grep -F "ended EARLY, after file $k" >/dev/null <<<"$out" \
        || { echo "heir_short_info: no line saying the tape ended EARLY, after file $k"; return 1; }
    # One NOT ON TAPE line for the planned files after the end (ADR-0012 item 140).
    grep -E "^  NOT ON TAPE  files $((k + 1)) to [0-9]+  \(planned; the tape ended after file $k, and its seal marker is file $((k + 1))\)\$" >/dev/null <<<"$out" \
        || { echo "heir_short_info: the file map has no NOT ON TAPE line for files $((k + 1)) on"; return 1; }
    # Lines that START with it, at any indent: the --verify verdict names it mid-sentence.
    [ "$(grep -cE '^ *NOT ON TAPE' <<<"$out" || true)" -eq 1 ] \
        || { echo "heir_short_info: want exactly one NOT ON TAPE line"; return 1; }
}
step_heir_short_verify() {
    local k out rc
    k="$(ss_ended_after)" || return 1
    out="$(ss_sh --verify 2>&1)"; rc=$?
    printf '%s\n' "$out"
    # tests/heir_restore.rs verify_on_a_short_tape_hashes_what_is_on_it_and_names_the_rest_not_on_tape
    [ "$rc" -eq 0 ] || { echo "heir_short_verify: RESTORE.sh --verify exited $rc, want 0"; return 1; }
    grep -E "^VERIFY PASSED: [0-9]+ file\(s\) match the front index\.\$" >/dev/null <<<"$out" \
        || { echo "heir_short_verify: no VERIFY PASSED line"; return 1; }
    grep -E "^NOT ON TAPE  files $((k + 1)) to [0-9]+  \(planned; the tape ended after file $k, and its seal marker is file $((k + 1))\)\$" >/dev/null <<<"$out" \
        || { echo "heir_short_verify: files $((k + 1)) on, planned, are not reported NOT ON TAPE"; return 1; }
    # Lines that START with it, at any indent: the verdict names it mid-sentence.
    [ "$(grep -cE '^ *NOT ON TAPE' <<<"$out" || true)" -eq 1 ] \
        || { echo "heir_short_verify: want exactly one NOT ON TAPE line"; return 1; }
    if grep -E "^FAIL " <<<"$out"; then echo "heir_short_verify: a FAIL line"; return 1; fi
    # The control: the last data slice on the tape, K, was hashed, none after it, and the seal
    # marker (file K+1) is the one file past it that has a line.
    grep -E "^PASS +$k +data_slice" >/dev/null <<<"$out" \
        || { echo "heir_short_verify: file $k, the last data slice on the tape, was not hashed"; return 1; }
    grep -E "^PASS +$((k + 1)) +seal_marker " >/dev/null <<<"$out" \
        || { echo "heir_short_verify: no PASS line for the seal marker, file $((k + 1))"; return 1; }
    if awk -v k="$k" '$1 == "PASS" && $2 + 0 > k && $3 == "data_slice" { found = 1 } END { exit found ? 0 : 1 }' <<<"$out"; then
        echo "heir_short_verify: a data slice after file $k was hashed"; return 1
    fi
}
step_heir_short_restore() {
    local d="$SS/heir/recovered-whole"
    ss_sh --restore --unit sa-whole --key "$SS/home/keys/alice-primary.age.key" --to "$d" \
        && diff -r "$SS/src/sa-whole" "$d"
}
# The two units the tape holds in part or not at all: sb-cut has slices on the tape up to K, and a file
# larger than a slice that begins in them and ends past K; sc-after has none. tests/heir_restore.rs
# a_whole_unit_restores_from_a_short_tape_and_a_partial_one_restores_what_it_holds and
# a_unit_with_no_slice_on_a_short_tape_is_said_so_by_every_mode_that_knows: what the tape holds
# whole is restored (and every file that comes back is whole), the rest is left out and LISTED,
# the run exits 1 and says RESTORE INCOMPLETE, no byte past the tape's end was asked of it, and
# nothing of a file tar had begun is left.
step_heir_short_cut_refused() {
    local out rc d list k key="$SS/home/keys/alice-primary.age.key" f rc2=0
    k="$(ss_ended_after)" || return 1
    d="$SS/heir/recovered-cut"
    out="$(ss_sh --restore --unit sb-cut --key "$key" --to "$d" 2>&1)"; rc=$?
    printf '%s\n' "$out" | tail -n 16
    [ "$rc" -eq 1 ] || { echo "heir_short_cut_refused: the cut unit exited $rc, want 1"; rc2=1; }
    grep -F "RESTORE INCOMPLETE" >/dev/null <<<"$out" || { echo "heir_short_cut_refused: no RESTORE INCOMPLETE"; rc2=1; }
    grep -F "is after the end of this tape (it ended after file $k)" >/dev/null <<<"$out" \
        || { echo "heir_short_cut_refused: the slices after the end are not said to be after the end of the tape"; rc2=1; }
    list="$d/.tapectl-damaged-files.txt"
    if [ -f "$list" ] && tail -n +2 "$list" | grep -Fx forty-megs.bin >/dev/null; then :; else
        echo "heir_short_cut_refused: the damaged-file list does not name forty-megs.bin"; rc2=1
    fi
    [ ! -e "$d/forty-megs.bin" ] || { echo "heir_short_cut_refused: a partial forty-megs.bin was left"; rc2=1; }
    # Whatever came back is whole: no zero-filled stand-in.
    if [ -d "$d" ]; then
        while IFS= read -r f; do
            f="${f#"$d"/}"
            [ "$f" = .tapectl-damaged-files.txt ] && continue
            cmp "$SS/src/sb-cut/$f" "$d/$f" || { echo "heir_short_cut_refused: $f is not whole"; rc2=1; }
        done < <(find "$d" -type f)
    fi
    # The unit planned after the end: no slice of it is on the tape, said by --list.
    out="$(ss_sh --list --key "$key" --unit sc-after 2>&1)"
    grep -F "PART: this tape holds none of its " >/dev/null <<<"$out" \
        || { printf '%s\n' "$out"; echo "heir_short_cut_refused: --list sc-after does not say the tape holds none of its slices"; rc2=1; }
    d="$SS/heir/recovered-after"
    out="$(ss_sh --restore --unit sc-after --key "$key" --to "$d" 2>&1)"; rc=$?
    printf '%s\n' "$out" | tail -n 8
    [ "$rc" -eq 1 ] || { echo "heir_short_cut_refused: sc-after exited $rc, want 1"; rc2=1; }
    [ ! -e "$d/after.txt" ] || { echo "heir_short_cut_refused: sc-after's file was restored from a tape that does not hold it"; rc2=1; }
    if [ -f "$d/.tapectl-damaged-files.txt" ] && tail -n +2 "$d/.tapectl-damaged-files.txt" | grep -Fx after.txt >/dev/null; then :; else
        echo "heir_short_cut_refused: the damaged-file list does not name after.txt"; rc2=1
    fi
    mt -f "$TAPE_DEV" rewind && mt -f "$TAPE_DEV" erase
    return "$rc2"
}
echo "gate: heir_short_* — a short tape, read by RESTORE.sh with the real tools (#420)"
check heir_short_seal      step_heir_short_seal
check heir_short_info      step_heir_short_info
check heir_short_verify    step_heir_short_verify
check heir_short_restore   step_heir_short_restore
check heir_short_cut_refused step_heir_short_cut_refused

# ---------- heir_{gzip,xz,zstd}_{restore,path}: a compressed unit, read with the real decompressor ----------
# Layout 3 compresses each slice alone (after the cut, before the encryption); the heir's script
# decompresses it with the host's gzip, xz or zstd and checks the plain cut's length and hash, never
# the decompressor's exit status (tests/heir_restore.rs a_compressed_unit_restores_whole_for_each_
# algorithm_carried, a_decompressor_that_exits_0_after_short_output_is_caught_by_the_slice_length).
# One tape, three units of alice's (zu-gzip, zu-xz, zu-zstd), each with `[policy] compression =
# "<alg>"` in its dotfile; each holds text that shrinks, noise that does not, a nested file, and a
# file larger than a slice (a run). The tape is made by the first of the six legs to run, and the
# others find it ready (a failure there fails every one of them, with the first's log named).
# Each *_restore leg restores the whole unit with the real decompressor on PATH, byte for byte,
# and the control: a decompressor stand-in that fails, first on PATH, must NOT complete the restore
# (the script runs the one on PATH, so the real one is the one that ran); each *_path leg restores
# two paths and then the long file by --path, byte for byte.
CZ="$RUN/compressed"
CZ_LABEL="CZ-0001"
CZC() { "$BIN" --home "$CZ/home" "$@"; }
cz_sh() { (cd "$CZ/heir" && TAPE_DEVICE="$TAPE_DEV" ./RESTORE.sh "$@"); }
cz_make() {
    local a u
    rm -rf "$CZ"; mkdir -p "$CZ/home/staging" "$CZ/heir" "$CZ/stub" || return 1
    for a in gzip xz zstd; do
        u="zu-$a"
        mkdir -p "$CZ/src/$u/deep"
        seq 1 300000 >"$CZ/src/$u/text.txt"                       # compressible
        head -c 1500000 /dev/urandom >"$CZ/src/$u/noise.bin"      # not
        echo "note of $a" >"$CZ/src/$u/deep/note.txt"
        head -c 3500000 /dev/urandom >"$CZ/src/$u/big.bin"        # larger than a slice (1M): a run
    done
    mt -f "$TAPE_DEV" rewind && mt -f "$TAPE_DEV" erase || { echo "mt erase failed"; return 1; }
    CZC init --operator gate-z \
        && CZC drive add --name mhvtl --device-tape "$TAPE_DEV" --generation LTO-8 \
        && CZC config set staging.directory "$CZ/home/staging" \
        && CZC config set defaults.slice_size 1M \
        && CZC tenant add alice \
        || { echo "home: init/drive add/config set/tenant add failed"; return 1; }
    for a in gzip xz zstd; do
        u="zu-$a"
        CZC unit init "$CZ/src/$u" --tenant alice --name "$u" || { echo "unit init $u failed"; return 1; }
        gate_unit_policy "$CZ/src/$u" "compression = \"$a\"" || return 1
        CZC snapshot create "$u" && CZC stage create "$u" || { echo "snapshot or stage of $u failed"; return 1; }
        # The staging was packed with $a: a leg that read an uncompressed unit would prove nothing.
        [ "$(gate_q "$CZ/home/tapectl.db" "SELECT ss.compression FROM stage_sets ss JOIN snapshots s ON s.id = ss.snapshot_id
            JOIN units u ON u.id = s.unit_id WHERE u.name = ?" "$u")" = "$a" ] \
            || { echo "the catalog does not say $u was staged with $a"; return 1; }
    done
    CZC volume init "$CZ_LABEL" --device "$TAPE_DEV" && CZC volume write "$CZ_LABEL" --device "$TAPE_DEV" --yes \
        || { echo "volume init/write of the compressed tape failed"; return 1; }
    mt -f "$TAPE_DEV" rewind && mt -f "$TAPE_DEV" fsf 2 \
        && dd if="$TAPE_DEV" bs=512k 2>/dev/null | tr -d '\0' >"$CZ/heir/RESTORE.sh" \
        && chmod +x "$CZ/heir/RESTORE.sh" && bash -n "$CZ/heir/RESTORE.sh" \
        || { echo "cannot read RESTORE.sh off the compressed tape"; return 1; }
    cmp -s "$CZ/heir/RESTORE.sh" "$GATE_REPO/src/volume/heir/RESTORE.sh" \
        || { echo "the RESTORE.sh on the compressed tape is not src/volume/heir/RESTORE.sh"; return 1; }
}
cz_prepare() {
    [ -f "$CZ/ready" ] && return 0
    if [ -f "$RUN/compressed.failed" ]; then
        echo "the compressed tape could not be made: see the log of the first heir_{gzip,xz,zstd}_* leg that ran"
        return 1
    fi
    if cz_make; then : >"$CZ/ready"; return 0; fi
    : >"$RUN/compressed.failed"
    return 1
}
cz_restore() { # cz_restore <algorithm>
    local a="$1" u key out rc d needle
    u="zu-$a"; key="$CZ/home/keys/alice-primary.age.key"
    cz_prepare || return 1
    # The envelope says the unit is compressed with $a (tests/heir_restore.rs a_decompressor_is_needed_only_by_a_unit_that_states_one).
    out="$(cz_sh --find-envelope --key "$key" 2>&1)" || { printf '%s\n' "$out"; return 1; }
    grep -E "^  $u  version 1  [0-9]+ slice\(s\)  [0-9]+ bytes  compressed with $a  \(envelopes? at files? [0-9]+(, [0-9]+)*\)\$" >/dev/null <<<"$out" \
        || { printf '%s\n' "$out"; echo "the envelope does not say $u is compressed with $a"; return 1; }
    # The control first: a decompressor that fails, first on PATH, must not give a complete restore.
    printf '#!/bin/sh\nexit 99\n' >"$CZ/stub/$a"; chmod +x "$CZ/stub/$a"
    d="$CZ/heir/stubbed-$a"
    out="$(cd "$CZ/heir" && env "PATH=$CZ/stub:$PATH" "TAPE_DEVICE=$TAPE_DEV" ./RESTORE.sh --restore --unit "$u" --key "$key" --to "$d" 2>&1)"; rc=$?
    rm -f "$CZ/stub/$a"
    if [ "$rc" -eq 0 ] || grep -F "RESTORE COMPLETE" >/dev/null <<<"$out"; then
        printf '%s\n' "$out" | tail -n 10
        echo "control: a failing $a on PATH still gave a complete restore: the script is not running the $a on PATH"; return 1
    fi
    # The real decompressor.
    d="$CZ/heir/restored-$a"
    out="$(cz_sh --restore --unit "$u" --key "$key" --to "$d" 2>&1)"; rc=$?
    printf '%s\n' "$out" | tail -n 12
    [ "$rc" -eq 0 ] || { echo "heir_${a}_restore: exited $rc, want 0"; return 1; }
    for needle in "RESTORE COMPLETE" "regular file(s) match the sha256 in the unit's file table"; do
        grep -F -- "$needle" >/dev/null <<<"$out" || { echo "heir_${a}_restore: the output lacks \"$needle\""; return 1; }
    done
    diff -r "$CZ/src/$u" "$d" || { echo "heir_${a}_restore: the restored tree differs from the source"; return 1; }
    [ ! -e "$d/.tapectl-damaged-files.txt" ] || { echo "heir_${a}_restore: a damaged-file list on a clean restore"; return 1; }
}
cz_path() { # cz_path <algorithm>
    local a="$1" u key out rc d
    u="zu-$a"; key="$CZ/home/keys/alice-primary.age.key"
    cz_prepare || return 1
    d="$CZ/heir/path-$a"
    out="$(cz_sh --restore --unit "$u" --key "$key" --to "$d" --path text.txt --path deep/note.txt 2>&1)"; rc=$?
    printf '%s\n' "$out" | tail -n 10
    [ "$rc" -eq 0 ] || { echo "heir_${a}_path: two paths exited $rc, want 0"; return 1; }
    grep -F "File check: all 2 regular file(s) named" >/dev/null <<<"$out" || { echo "heir_${a}_path: no 'File check: all 2 regular file(s) named'"; return 1; }
    cmp "$CZ/src/$u/text.txt" "$d/text.txt" && cmp "$CZ/src/$u/deep/note.txt" "$d/deep/note.txt" || { echo "heir_${a}_path: a path differs from the source"; return 1; }
    [ "$(find "$d" -type f | wc -l)" = 2 ] || { echo "heir_${a}_path: more than the two files were restored"; return 1; }
    # The long file, a run of compressed slices, by its own path.
    d="$CZ/heir/path-long-$a"
    out="$(cz_sh --restore --unit "$u" --key "$key" --to "$d" --path big.bin 2>&1)"; rc=$?
    printf '%s\n' "$out" | tail -n 6
    [ "$rc" -eq 0 ] || { echo "heir_${a}_path: --path big.bin exited $rc, want 0"; return 1; }
    cmp "$CZ/src/$u/big.bin" "$d/big.bin" || { echo "heir_${a}_path: big.bin differs from the source"; return 1; }
}
step_heir_gzip_restore() { cz_restore gzip; }
step_heir_gzip_path() { cz_path gzip; }
step_heir_xz_restore() { cz_restore xz; }
step_heir_xz_path() { cz_path xz; }
step_heir_zstd_restore() { cz_restore zstd; }
step_heir_zstd_path() { cz_path zstd; }
echo "gate: heir_{gzip,xz,zstd}_* — a compressed unit, read by RESTORE.sh with the real decompressor"
check heir_gzip_restore    step_heir_gzip_restore
check heir_gzip_path       step_heir_gzip_path
check heir_xz_restore      step_heir_xz_restore
check heir_xz_path         step_heir_xz_path
check heir_zstd_restore    step_heir_zstd_restore
check heir_zstd_path       step_heir_zstd_path

# ---------- span_*: a unit across tapes, written and read on mhvtl (1.7.0; #478; Layout 3: T4.16) ----------
# ADR-0012 item 136: the mhvtl proof of spanning is this one leg, judged by test name, with the
# store's early-warning test hook (TAPECTL_TEST_EARLY_WARNING_AT, as heir_short_* uses it) on
# the write of each tape that must end inside the unit, and on no other. Everything runs in
# throwaway homes under $SP, tenant alice, on cartridges of the gate's generation picked by tag
# from the library and erased before each volume init. A cartridge is changed as a person
# changes it: `mtx` takes the one in the drive back to its slot and loads the next (sp_swap).
# The prompts of a restore across tapes are answered by scripts/mhvtl-prompt-driver.py: on a
# pipe for RESTORE.sh, which reads its answer from its input (ENTER: loaded; "done" or the end of
# the input: "I do not have it"; "quit"), and on a pseudo-terminal for tapectl, which asks only
# on a terminal (coordinator ruling X30; there is no production hook for it).
# Layout 3 (T4.16): RESTORE.sh feeds ONE tar run across the cartridges and keeps no slice in its
# work directory at a prompt, so the proofs that were about dar and scratch slices are about that.
#
#   span_rows              proof 1, home A (copy target 2): two copies over three tapes laid
#                          out as item 93 says. Tape 1 holds sa-whole and sa-whole-2, then
#                          sb-span (40 MiB; it sorts last, and the queue is in unit-name order, item 288)
#                          until the warning: a range. Tape 2
#                          begins with the owed run, the whole set: completed. Tape 3 holds the
#                          owed run that is left: a range. Every slice is on two volumes.
#   span_heir_tapes_1_3    proof 2: RESTORE.sh's restore from tapes 1 and 3: one prompt, for the
#                          first slice tape 1 lacks; the one tar run is waiting at it and holds no
#                          slice in the work directory.
#   span_heir_tape_2       proof 2: from tape 2 alone, which holds sb-span whole: no prompt.
#   span_heir_stream_swap  proof 3: the same restore with the drive empty for
#                          TAPECTL_GATE_SWAP_WAIT seconds (default 300) at the swap. The one tar
#                          run is the same process when the prompt comes and when it is answered
#                          (item 267: it waits on its input across a cartridge change).
#   span_heir_start_later  proof 3b: a restore STARTED on tape 3, which holds only the later slices:
#                          answered "done" at the prompt for slice 1, it restores the files its
#                          tape holds whole and lists the rest with the tapes that hold them.
#   span_heir_second_run   proof 3c: a second run into the same --to, started on tape 1 and given
#                          tape 3 at its one prompt, completes the unit; what already passed is not
#                          rewritten.
#   span_heir_not_have     proof 3d: tape 1 alone, "done" (and the end of the input) at the prompt
#                          for slice K1+1: the run ends, exit 1, the file tar had begun is removed
#                          and listed, nothing is asked again; "quit" stops it by name.
#   span_restore_unit      proof 4: tapectl restore unit from tape 1: with no terminal it takes what
#                          its tape holds whole and lists the rest; on a terminal it asks for the
#                          whole copy, tape 2 (item 131).
#   span_retire_floors     proof 5: retiring tape 2 is Tier 2 (one copy left); then tapes 1 and 3
#                          each hold the last copy of slices and are refused at Tier 3.
#   span_floor_target_1    proof 6, home F (copy target 1): tape 1 of a span released its slices
#                          and the version is not current, yet retiring the tape is refused (item 128).
#   span_rebuild_tape_3    proof 7, home R: catalog rebuild of tape 3 alone records its range and
#                          names the missing slices and their tapes; tape 1 then completes it.
#   span_three_tapes       proof 8, home C (copy target 1, small cartridges, post_warning_bytes):
#                          a unit larger than one tape is planned in the queue's order (item 288):
#                          pa-whole first, then pb-big until the warning on tape 1; pc-small, queued
#                          after it, is not on tapes 1 and 2 and follows pb-big's owed tail on tape 3.
#                          It restores across both swaps.
SP="$RUN/span"
SP_WAIT="${TAPECTL_GATE_SWAP_WAIT:-300}"
SP_DRIVER="$(cd "$(dirname "$0")" && pwd)/mhvtl-prompt-driver.py"
# The hook for a 40 MiB unit of 4 MiB slices (4.5 MiB each on tape) after the eight fixed files
# (4 MiB) and two small whole units, heir_short_seal's figure: the warning comes in its sixth slice.
SP_HOOK_AT=31457280
SP_PROMPT_HEIR="Load the cartridge that holds slice "
SP_PROMPT_TAPECTL="and press Enter."
SP_HOOK_RE='TAPECTL_TEST_EARLY_WARNING_AT=[0-9]+ is set'
SP_PREP=""
SP_ORIG=""
SP_FREE=()
declare -A SP_SLOT=() SP_TAG=()
export SP_CHG="$CHG_SG" SP_DTE="$DTE" SP_DEV="$TAPE_DEV" SP_SLOTS="$SP/slots.tsv"
SPA() { "$BIN" --home "$SP/a/home" "$@"; }
SPF() { "$BIN" --home "$SP/f/home" "$@"; }
SPC() { "$BIN" --home "$SP/c/home" "$@"; }
SPR() { "$BIN" --home "$SP/r/home" "$@"; }

# The swap a person makes at a cartridge prompt: the cartridge in the drive back to the slot it
# came from, a wait with the drive empty, the cartridge in slot $1 into the drive, and a rewind,
# which returns once the drive is ready. $1 "asked" is the slot of the label the prompt names
# (tapectl's "load LABEL, and press Enter", in the driver's PROMPT_LINE), from $SP_SLOTS.
# Exported with its variables: the prompt driver runs it.
sp_swap() { # sp_swap <slot|asked> [wait seconds]
    local slot="$1" wait="${2:-0}" from label n
    if [ "$slot" = asked ]; then
        label="$(sed -n 's/.*, load \([^ ,]*\), and press Enter\..*/\1/p' <<<"${PROMPT_LINE:-}")"
        slot="$(awk -F'\t' -v l="$label" '$1 == l { print $2 }' "$SP_SLOTS")"
        [ -n "$slot" ] || { echo "sp_swap: the prompt names '${label:-nothing}', no cartridge of this leg: ${PROMPT_LINE:-}"; return 1; }
        echo "sp_swap: the prompt asks for $label, the cartridge in slot $slot"
    fi
    from="$(mtx -f "$SP_CHG" status | sed -n "s/.*Data Transfer Element $SP_DTE:Full (Storage Element \([0-9]*\) Loaded).*/\1/p")"
    if [ "$from" != "$slot" ]; then
        if [ -n "$from" ]; then
            mtx -f "$SP_CHG" unload "$from" "$SP_DTE" || { echo "sp_swap: unload to slot $from failed"; return 1; }
        fi
        if [ "$wait" -gt 0 ]; then echo "sp_swap: the drive is empty; waiting ${wait}s"; sleep "$wait"; fi
        mtx -f "$SP_CHG" load "$slot" "$SP_DTE" || { echo "sp_swap: load of slot $slot failed"; return 1; }
    fi
    for n in 1 2 3 4 5 6 7 8 9 10; do
        if mt -f "$SP_DEV" rewind 2>/dev/null; then
            echo "sp_swap: slot $slot in the drive (slot ${from:-none} out)"
            return 0
        fi
        sleep 1
    done
    echo "sp_swap: the drive is not ready after slot $slot was loaded"
    return 1
}
# What a restore's work directory inside <dest> (.tapectl-work*, Layout 3's) holds when a prompt
# comes: a first line WORKDIR with its count (the positive control: the directory is there), then
# one line for each entry. Item 267: no slice, ciphertext or plain, waits there between cartridges.
# Exported with sp_swap: the prompt driver runs it.
sp_work_note() { # sp_work_note <dest> <out>
    local d f n=0
    : >"$2" || return 1
    for d in "$1"/.tapectl-work*; do [ -d "$d" ] && n=$((n + 1)); done
    echo "WORKDIR $n" >"$2"
    for d in "$1"/.tapectl-work*; do
        [ -d "$d" ] || continue
        for f in "$d"/*; do [ -e "$f" ] && echo "ENTRY $(basename "$f")" >>"$2"; done
    done
    echo "sp_work_note: $n work director(ies) under $1, $(($(wc -l <"$2") - 1)) entr(ies)"
}
export -f sp_swap sp_work_note

sp_q() { gate_q "$@"; }
# "first last count" of a unit's slices written on a volume, and a unit's slice count.
sp_run_of() { # sp_run_of <db> <label> <unit>
    sp_q "$1" "SELECT MIN(sl.slice_number), MAX(sl.slice_number), COUNT(*) FROM write_positions wp
        JOIN writes w ON w.id = wp.write_id JOIN volumes v ON v.id = w.volume_id
        JOIN stage_slices sl ON sl.id = wp.stage_slice_id JOIN stage_sets ss ON ss.id = sl.stage_set_id
        JOIN snapshots s ON s.id = ss.snapshot_id JOIN units u ON u.id = s.unit_id
        WHERE v.label = ? AND u.name = ? AND wp.status = 'written'" "$2" "$3" | tr '\t' ' '
}
sp_slices() { # sp_slices <db> <unit>
    sp_q "$1" "SELECT COUNT(*) FROM stage_slices sl JOIN stage_sets ss ON ss.id = sl.stage_set_id
        JOIN snapshots s ON s.id = ss.snapshot_id JOIN units u ON u.id = s.unit_id WHERE u.name = ?" "$2"
}

# The cartridges this leg may use: every storage slot holding a data cartridge of the gate's
# generation (the one in the drive is the gate's own, put back when the leg ends).
sp_prepare() {
    local status line slot
    rm -rf "$SP" && mkdir -p "$SP" && : >"$SP_SLOTS" || { SP_PREP="cannot create $SP"; return 1; }
    case "$SP_WAIT" in
        '' | *[!0-9]*) SP_PREP="TAPECTL_GATE_SWAP_WAIT='$SP_WAIT' is not a whole number of seconds"; return 1 ;;
    esac
    [ -f "$SP_DRIVER" ] || { SP_PREP="the prompt driver $SP_DRIVER is missing"; return 1; }
    status="$(mtx -f "$CHG_SG" status)" || { SP_PREP="mtx status on $CHG_SG failed"; return 1; }
    SP_ORIG="$(sed -n "s/.*Data Transfer Element $DTE:Full (Storage Element \([0-9]*\) Loaded).*/\1/p" <<<"$status")"
    while IFS= read -r line; do
        slot="$(sed -n 's/.*Storage Element \([0-9]*\):Full.*/\1/p' <<<"$line")"
        [ -n "$slot" ] || continue
        SP_FREE+=("$slot")
        SP_TAG[$slot]="$(sed -n 's/.*VolumeTag *= *\([A-Z0-9]*\).*/\1/p' <<<"$line")"
    done < <(grep -E "Storage Element [0-9]+:Full" <<<"$status" | grep "VolumeTag=[EF][0-9]*$GEN")
    [ "${#SP_FREE[@]}" -ge 7 ] || { SP_PREP="the library has ${#SP_FREE[@]} free $GEN data cartridges; the leg needs 7"; return 1; }
}
sp_ready() {
    [ -z "$SP_PREP" ] || { echo "the span leg could not be prepared: $SP_PREP"; return 1; }
}
# The next free cartridge becomes <label>'s, recorded in $SP_SLOTS for sp_swap's "asked".
sp_assign() { # sp_assign <label>
    [ "${#SP_FREE[@]}" -gt 0 ] || { echo "no free cartridge left for $1"; return 1; }
    SP_SLOT[$1]="${SP_FREE[0]}"
    SP_FREE=("${SP_FREE[@]:1}")
    printf '%s\t%s\n' "$1" "${SP_SLOT[$1]}" >>"$SP_SLOTS"
    echo "$1: the cartridge in slot ${SP_SLOT[$1]} (${SP_TAG[${SP_SLOT[$1]}]:-no tag})"
}
sp_load() { # sp_load <label>: <label>'s cartridge in the drive
    [ -n "${SP_SLOT[$1]:-}" ] || { echo "no cartridge was given to $1"; return 1; }
    sp_swap "${SP_SLOT[$1]}"
}
# A throwaway home: an escrow recipient (ADR-0005; its secret only in escrow.out), the gate's
# drive, staging, 4 MiB slices, the copy target, and tenant alice.
sp_home() { # sp_home <ctl> <dir> <operator> <min_copies>
    local ctl="$1" d="$2" op="$3" copies="$4"
    mkdir -p "$d/home/staging" "$d/src" || return 1
    "$ctl" init --operator "$op" --no-escrow \
        && "$ctl" key generate --escrow >"$d/escrow.out" 2>&1 \
        && chmod 600 "$d/escrow.out" \
        && "$ctl" drive add --name mhvtl --device-tape "$TAPE_DEV" --generation LTO-8 \
        && "$ctl" config set staging.directory "$d/home/staging" \
        && "$ctl" config set defaults.slice_size 4M \
        && "$ctl" config set defaults.min_copies "$copies" \
        && "$ctl" tenant add alice \
        || { echo "home $d: init, escrow, drive add, config set or tenant add failed"; return 1; }
}
# unit init each source under <dir>/src for alice, then snapshot and stage each. No unit opts in
# to anything: every unit continues on the next cartridge (item 288).
sp_units() { # sp_units <ctl> <dir> <unit>...
    local ctl="$1" d="$2" u
    shift 2
    for u in "$@"; do
        "$ctl" unit init "$d/src/$u" --tenant alice --name "$u" || { echo "unit init $u failed"; return 1; }
    done
    for u in "$@"; do
        "$ctl" snapshot create "$u" && "$ctl" stage create "$u" || { echo "snapshot or stage of $u failed"; return 1; }
    done
}
# <label>'s cartridge in the drive, erased, and `volume init --json` into <dir>/<label>.init.json.
sp_new_volume() { # sp_new_volume <ctl> <dir> <label> [volume init arg...]
    local ctl="$1" d="$2" label="$3"
    shift 3
    sp_load "$label" && mt -f "$TAPE_DEV" erase || { echo "loading and erasing $label's cartridge failed"; return 1; }
    "$ctl" --json volume init "$label" --device "$TAPE_DEV" "$@" >"$d/$label.init.json" \
        || { cat "$d/$label.init.json"; echo "volume init $label failed"; return 1; }
    cat "$d/$label.init.json"
}
# volume write --json: the report into <out>.json, the errors (the hook's announcement, the
# notices) into <out>.err. The hook is set for this one write when [hook bytes] is given.
sp_write() { # sp_write <ctl> <label> <out> [hook bytes]
    local ctl="$1" label="$2" out="$3" hook="${4:-}" rc
    if [ -n "$hook" ]; then
        TAPECTL_TEST_EARLY_WARNING_AT="$hook" "$ctl" --json --yes volume write "$label" --device "$TAPE_DEV" >"$out.json" 2>"$out.err"
    else
        "$ctl" --json --yes volume write "$label" --device "$TAPE_DEV" >"$out.json" 2>"$out.err"
    fi
    rc=$?
    cat "$out.err" "$out.json"
    [ "$rc" -eq 0 ] || { echo "volume write $label exited $rc (a short seal is a success)"; return 1; }
}
# Whether a write's errors announce the test hook: 0 yes, 1 no, 2 unreadable (never "no").
sp_hook_said() { # sp_hook_said <err file>
    grep -E "$SP_HOOK_RE" "$1" >/dev/null
}
# The hook announced by the hooked writes (the positive control), and by none of the others.
sp_hook_only_on() { # sp_hook_only_on <hooked err>... -- <unhooked err>...
    local f rc hooked=1
    for f in "$@"; do
        if [ "$f" = -- ]; then hooked=0; continue; fi
        sp_hook_said "$f"
        rc=$?
        if [ "$hooked" = 1 ] && [ "$rc" -ne 0 ]; then
            echo "$f: the hooked write does not announce TAPECTL_TEST_EARLY_WARNING_AT (grep rc=$rc)"; return 1
        fi
        if [ "$hooked" = 0 ] && [ "$rc" -ne 1 ]; then
            echo "$f: a write run without the hook announces it, or could not be searched (grep rc=$rc)"; return 1
        fi
    done
    echo "the hook was announced by the hooked writes and by no other"
}
# File 0's requires line of the tape in the drive: Layout 3 states `requires = []` on every tape, a
# spanning one too (the short seal and a unit on another cartridge are part of the layout, item
# 288; volume-format-v3.md 1.2). A tape that fenced an older reader would say so here.
sp_requires_none() { # sp_requires_none <label>
    local line
    line="$(mt -f "$TAPE_DEV" rewind && dd if="$TAPE_DEV" bs=512k count=1 2>/dev/null | tr -d '\0' | grep -E '^requires = ')"
    [ "$line" = 'requires = []' ] \
        || { echo "$1: File 0 says '${line:-no requires line}', want requires = []"; return 1; }
}
# RESTORE.sh (file 2) of the tape in the drive, into <dir>.
sp_restore_sh() { # sp_restore_sh <dir>
    mkdir -p "$1" && mt -f "$TAPE_DEV" rewind && mt -f "$TAPE_DEV" fsf 2 \
        && dd if="$TAPE_DEV" bs=512k 2>/dev/null | tr -d '\0' >"$1/RESTORE.sh" \
        && chmod +x "$1/RESTORE.sh" && bash -n "$1/RESTORE.sh" \
        || { echo "cannot read RESTORE.sh off the tape into $1"; return 1; }
}
# The prompt driver; COMMAND's output (and the driver's own lines) into <out>.
sp_drive() { # sp_drive <out> <events> <prompt> <stop> [driver option...] -- COMMAND [arg...]
    local out="$1" ev="$2" prompt="$3" stop="$4"
    shift 4
    python3 "$SP_DRIVER" --prompt "$prompt" --stop "$stop" --events "$ev" --timeout $((SP_WAIT + 1800)) "$@" >"$out" 2>&1
}
# Every needle in a file, each missing one named.
sp_has() { # sp_has <file> <needle>...
    local f="$1" n
    shift
    for n in "$@"; do
        grep -F -- "$n" "$f" >/dev/null || { echo "$f lacks: $n"; return 1; }
    done
}
# A volume's status in home A, and the versions of sb-span a retire of it would leave with no
# copy, as the dry run's JSON says ("none", or the versions; "error" when it cannot be read).
sp_a_status() { # sp_a_status <label>
    sp_q "$SP/a/home/tapectl.db" "SELECT status FROM volumes WHERE label = ?" "$1"
}
sp_a_last_copy() { # sp_a_last_copy <label>
    SPA --json --dry-run volume retire "$1" >"$SP/a/retire/$1.dry.json" 2>&1 || { echo error; return 0; }
    python3 - "$SP/a/retire/$1.dry.json" <<'PY' || echo error
import json, sys
d = json.load(open(sys.argv[1]))
print(" ".join(str(v) for u in d["affected_units"] if u["unit"] == "sb-span" for v in u["last_copy_versions"]) or "none")
PY
}

echo "gate: span_* — a unit across tapes, written and read on mhvtl (#478)"
if sp_prepare; then
    echo "gate: span leg: the drive's cartridge came from slot ${SP_ORIG:-none}; free $GEN cartridges in slots ${SP_FREE[*]}"
else
    echo "gate: span leg NOT prepared: $SP_PREP"
fi

step_span_rows() {
    local db="$SP/a/home/tapectl.db" n k1 run t u
    sp_ready || return 1
    sp_home SPA "$SP/a" gate-span-a 2 || return 1
    mkdir -p "$SP/a/src/sa-whole" "$SP/a/src/sb-span/deep" "$SP/a/src/sa-whole-2"
    echo "whole, before the span by name $CANARY" >"$SP/a/src/sa-whole/a.txt"
    head -c 700000 /dev/urandom >"$SP/a/src/sa-whole/a.bin"
    head -c 41943040 /dev/urandom >"$SP/a/src/sb-span/deep/forty-megs.bin"
    echo "a unit that may span" >"$SP/a/src/sb-span/note.txt"
    echo "whole, before the span by name too" >"$SP/a/src/sa-whole-2/c.txt"
    sp_units SPA "$SP/a" sa-whole sb-span sa-whole-2 || return 1
    for t in SPA-0001 SPA-0002 SPA-0003; do sp_assign "$t" || return 1; done

    # Tape 1: the hook ends sb-span in a slice before its last: a range.
    sp_new_volume SPA "$SP/a" SPA-0001 && sp_write SPA SPA-0001 "$SP/a/w1" "$SP_HOOK_AT" \
        && sp_requires_none SPA-0001 || return 1
    python3 - "$db" "$SP/a/w1.json" <<'PY' || return 1
import json, sqlite3, sys
c = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
j = json.load(open(sys.argv[2]))
def q(sql, *a): return c.execute(sql, a).fetchall()
rows = dict(q("""SELECT u.name, w.status FROM writes w JOIN volumes v ON v.id = w.volume_id
    JOIN stage_sets ss ON ss.id = w.stage_set_id JOIN snapshots s ON s.id = ss.snapshot_id
    JOIN units u ON u.id = s.unit_id WHERE v.label = 'SPA-0001'"""))
assert rows == {"sa-whole": "completed", "sb-span": "range", "sa-whole-2": "completed"}, f"tape 1's rows: {rows}"
k = q("SELECT ended_after FROM volumes WHERE label = 'SPA-0001'")[0][0]
assert k is not None, "tape 1 sealed full: the hook did not end it"
def pos(unit, written_only=False):
    return [p for (p,) in q(f"""SELECT CAST(wp.position AS INTEGER) FROM write_positions wp
        JOIN writes w ON w.id = wp.write_id JOIN volumes v ON v.id = w.volume_id
        JOIN stage_slices sl ON sl.id = wp.stage_slice_id JOIN stage_sets ss ON ss.id = w.stage_set_id
        JOIN snapshots s ON s.id = ss.snapshot_id JOIN units u ON u.id = s.unit_id
        WHERE v.label = 'SPA-0001' AND u.name = ? {"AND wp.status = 'written'" if written_only else ""}
        ORDER BY sl.slice_number""", unit)]
b = pos("sb-span")
assert b and b[0] <= k < b[-1], f"K = {k} is not inside sb-span's slices {b} (the hook's figure no longer lands there)"
assert pos("sa-whole")[-1] < pos("sa-whole-2")[0] < b[0], "the queue is in unit-name order, sb-span last, so the warning falls in it (item 288)"
k1 = len(pos("sb-span", True))
assert j["ending"] == "short" and j["ended_after"] == k and "cut_units" not in j, j
assert j["range_units"] == [{"unit": "sb-span", "version": 1, "first_slice": 1, "last_slice": k1}], j["range_units"]
assert j["keep_together"] == [] and j["deferred_units"] == [], j
st = q("SELECT s.status FROM snapshots s JOIN units u ON u.id = s.unit_id WHERE u.name = 'sb-span'")[0][0]
assert st == "staged", f"sb-span is {st} after tape 1: a range alone is not a copy (item 128)"
rel = q("""SELECT COUNT(*) FROM stage_slices sl JOIN stage_sets ss ON ss.id = sl.stage_set_id
    JOIN snapshots s ON s.id = ss.snapshot_id JOIN units u ON u.id = s.unit_id
    WHERE u.name = 'sb-span' AND sl.released_at IS NOT NULL""")[0][0]
assert rel == 0, f"{rel} slice(s) of sb-span released at one copy of a target of 2"
print(f"tape 1: ended_after {k}; sb-span a range of slices 1-{k1} at {b[:k1]}, whole units first; staged")
PY

    # Tape 2: no hook. The owed run, all of sb-span, goes first: completed, and current.
    sp_new_volume SPA "$SP/a" SPA-0002 && sp_write SPA SPA-0002 "$SP/a/w2" \
        && sp_requires_none SPA-0002 || return 1
    python3 - "$db" "$SP/a/w2.json" <<'PY' || return 1
import json, sqlite3, sys
c = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
j = json.load(open(sys.argv[2]))
def q(sql, *a): return c.execute(sql, a).fetchall()
rows = dict(q("""SELECT u.name, w.status FROM writes w JOIN volumes v ON v.id = w.volume_id
    JOIN stage_sets ss ON ss.id = w.stage_set_id JOIN snapshots s ON s.id = ss.snapshot_id
    JOIN units u ON u.id = s.unit_id WHERE v.label = 'SPA-0002'"""))
assert rows == {"sa-whole": "completed", "sb-span": "completed", "sa-whole-2": "completed"}, f"tape 2's rows: {rows}"
assert q("SELECT ended_after FROM volumes WHERE label = 'SPA-0002'")[0][0] is None, "tape 2 sealed short"
first = dict(q("""SELECT u.name, MIN(CAST(wp.position AS INTEGER)) FROM write_positions wp
    JOIN writes w ON w.id = wp.write_id JOIN volumes v ON v.id = w.volume_id
    JOIN stage_sets ss ON ss.id = w.stage_set_id JOIN snapshots s ON s.id = ss.snapshot_id
    JOIN units u ON u.id = s.unit_id WHERE v.label = 'SPA-0002' GROUP BY u.name"""))
assert first["sb-span"] < min(first["sa-whole"], first["sa-whole-2"]), f"the owed run is not first (item 122): {first}"
st = q("SELECT s.status FROM snapshots s JOIN units u ON u.id = s.unit_id WHERE u.name = 'sb-span'")[0][0]
assert st == "current", f"sb-span is {st} with every slice on a sealed volume (item 128)"
k1 = q("""SELECT MAX(sl.slice_number) FROM write_positions wp JOIN writes w ON w.id = wp.write_id
    JOIN volumes v ON v.id = w.volume_id JOIN stage_slices sl ON sl.id = wp.stage_slice_id
    WHERE v.label = 'SPA-0001' AND w.status = 'range' AND wp.status = 'written'""")[0][0]
rel = [n for (n,) in q("""SELECT sl.slice_number FROM stage_slices sl JOIN stage_sets ss ON ss.id = sl.stage_set_id
    JOIN snapshots s ON s.id = ss.snapshot_id JOIN units u ON u.id = s.unit_id
    WHERE u.name = 'sb-span' AND sl.released_at IS NOT NULL ORDER BY sl.slice_number""")]
assert rel == list(range(1, k1 + 1)), f"released {rel}, want slices 1-{k1}, the ones on two volumes (item 129)"
assert [s["slices"] for s in j["slices_released"]] == [list(range(1, k1 + 1))], j["slices_released"]
assert j["ending"] == "full" and j["range_units"] == [] and j["keep_together"] == [], j
print(f"tape 2: every unit completed, sb-span's owed run first at {first['sb-span']}; current; slices 1-{k1} released")
PY

    # Tape 3: no hook. The owed run that is left: a range, sealed full.
    sp_new_volume SPA "$SP/a" SPA-0003 && sp_write SPA SPA-0003 "$SP/a/w3" \
        && sp_requires_none SPA-0003 || return 1
    sp_hook_only_on "$SP/a/w1.err" -- "$SP/a/w2.err" "$SP/a/w3.err" || return 1
    n="$(sp_slices "$db" sb-span)"
    run="$(sp_run_of "$db" SPA-0001 sb-span)"; k1="${run#* }"; k1="${k1%% *}"
    sp_has "$SP/a/w3.err" "volume \"SPA-0003\": keep these together: \"sb-span\" v1 is whole across SPA-0001 (slices 1-$k1) and SPA-0003 (slices $((k1 + 1))-$n)." \
        || return 1
    SPA --json report copies --unit sb-span >"$SP/a/copies.json" || { echo "report copies failed"; return 1; }
    python3 - "$db" "$SP/a/w3.json" "$SP/a/copies.json" <<'PY' || return 1
import json, sqlite3, sys
c = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
j = json.load(open(sys.argv[2]))
copies = json.load(open(sys.argv[3]))
def q(sql, *a): return c.execute(sql, a).fetchall()
rows = {(l, u): s for l, u, s in q("""SELECT v.label, u.name, w.status FROM writes w
    JOIN volumes v ON v.id = w.volume_id JOIN stage_sets ss ON ss.id = w.stage_set_id
    JOIN snapshots s ON s.id = ss.snapshot_id JOIN units u ON u.id = s.unit_id""")}
span = [rows.get((l, "sb-span")) for l in ("SPA-0001", "SPA-0002", "SPA-0003")]
assert span == ["range", "completed", "range"], f"sb-span's rows on tapes 1-3: {span} (item 93)"
assert {u for (l, u) in rows if l == "SPA-0003"} == {"sb-span"}, f"tape 3 holds more than the owed run: {rows}"
assert q("SELECT ended_after FROM volumes WHERE label = 'SPA-0003'")[0][0] is None, "tape 3 sealed short"
n = q("""SELECT COUNT(*) FROM stage_slices sl JOIN stage_sets ss ON ss.id = sl.stage_set_id
    JOIN snapshots s ON s.id = ss.snapshot_id JOIN units u ON u.id = s.unit_id WHERE u.name = 'sb-span'""")[0][0]
def run(label):
    return q("""SELECT MIN(sl.slice_number), MAX(sl.slice_number), COUNT(*) FROM write_positions wp
        JOIN writes w ON w.id = wp.write_id JOIN volumes v ON v.id = w.volume_id
        JOIN stage_slices sl ON sl.id = wp.stage_slice_id JOIN stage_sets ss ON ss.id = sl.stage_set_id
        JOIN snapshots s ON s.id = ss.snapshot_id JOIN units u ON u.id = s.unit_id
        WHERE v.label = ? AND u.name = 'sb-span' AND wp.status = 'written'""", label)[0]
r1, r2, r3 = run("SPA-0001"), run("SPA-0002"), run("SPA-0003")
k1 = r1[1]
assert r1 == (1, k1, k1) and r2 == (1, n, n) and r3 == (k1 + 1, n, n - k1), f"runs {r1} {r2} {r3} of {n}"
per_slice = [m for (_, m) in q("""SELECT sl.slice_number, COUNT(DISTINCT w.volume_id) FROM stage_slices sl
    JOIN stage_sets ss ON ss.id = sl.stage_set_id JOIN snapshots s ON s.id = ss.snapshot_id
    JOIN units u ON u.id = s.unit_id
    LEFT JOIN write_positions wp ON wp.stage_slice_id = sl.id AND wp.status = 'written'
    LEFT JOIN writes w ON w.id = wp.write_id AND w.status IN ('completed', 'range')
    WHERE u.name = 'sb-span' GROUP BY sl.slice_number ORDER BY sl.slice_number""")]
assert per_slice == [2] * n, f"volumes per slice: {per_slice}, want every slice on two (item 76)"
c2 = [r["copies"] for r in copies if r["unit"] == "sb-span"]
assert c2 == [2], f"report copies says {copies} for sb-span, want 2 (item 127)"
rel = q("""SELECT COUNT(*) FROM stage_slices sl JOIN stage_sets ss ON ss.id = sl.stage_set_id
    JOIN snapshots s ON s.id = ss.snapshot_id JOIN units u ON u.id = s.unit_id
    WHERE u.name = 'sb-span' AND sl.released_at IS NULL""")[0][0]
assert rel == 0, f"{rel} slice(s) of sb-span still staged at two copies each"
assert j["ending"] == "full", j
assert j["range_units"] == [{"unit": "sb-span", "version": 1, "first_slice": k1 + 1, "last_slice": n}], j["range_units"]
want = [{"unit": "sb-span", "version": 1, "tapes": [
    {"label": "SPA-0001", "first_slice": 1, "last_slice": k1},
    {"label": "SPA-0003", "first_slice": k1 + 1, "last_slice": n}]}]
assert j["keep_together"] == want, f"keep_together {j['keep_together']} (item 134)"
print(f"rows range, completed, range; {n} slices, each on two volumes; 2 copies; tapes 1 and 3 kept together")
PY
}
check span_rows step_span_rows

# The facts of home A the restores below compare with: K1 (tape 1's last slice) and N.
sp_a_figures() {
    local run
    SP_A_N="$(sp_slices "$SP/a/home/tapectl.db" sb-span)"
    run="$(sp_run_of "$SP/a/home/tapectl.db" SPA-0001 sb-span)"
    SP_A_K1="${run#* }"
    SP_A_K1="${SP_A_K1%% *}"
    case "$SP_A_N:$SP_A_K1" in
        *[!0-9:]* | :* | *:) echo "home A has no span to read (N='$SP_A_N', K1='$SP_A_K1'): span_rows did not write it"; return 1 ;;
    esac
    echo "sb-span: $SP_A_N slices; tape 1 holds 1-$SP_A_K1, tape 3 holds $((SP_A_K1 + 1))-$SP_A_N"
}

step_span_heir_tapes_1_3() {
    local h="$SP/a/heir" dest="$SP/a/heir/from-1-3" key="$SP/a/home/keys/alice-primary.age.key" rc swap
    sp_ready && sp_a_figures || return 1
    sp_load SPA-0001 && sp_restore_sh "$h" || return 1
    swap="$(printf 'sp_work_note %q %q && sp_swap %q' "$dest" "$h/prompt-work.txt" "${SP_SLOT[SPA-0003]}")"
    (cd "$h" && sp_drive "$h/from-1-3.out" "$h/from-1-3.events" "$SP_PROMPT_HEIR" quit --swap "$swap" -- \
        env TAPE_DEVICE="$TAPE_DEV" ./RESTORE.sh --restore --unit sb-span --key "$key" --to "$dest")
    rc=$?
    cat "$h/from-1-3.out"
    [ "$rc" -eq 0 ] || { echo "RESTORE.sh across tapes 1 and 3 exited $rc, want 0"; return 1; }
    # tests/heir_restore.rs a_unit_on_three_tapes_restores_across_two_swaps_as_one_run_for_each_algorithm
    # and the_prompt_names_the_tapes_the_index_names_and_says_where_to_look_when_it_names_none
    sp_has "$h/from-1-3.out" "RESTORE COMPLETE" \
        "needs slice $((SP_A_K1 + 1)) of $SP_A_N next" \
        "This cartridge identifies as: SPA-0003" || return 1
    diff -r "$SP/a/src/sb-span" "$dest" || { echo "the restore from tapes 1 and 3 differs from the source"; return 1; }
    python3 - "$h/from-1-3.events" "$h/prompt-work.txt" "$SP_A_K1" <<'PY' || return 1
import json, sys
ev = [json.loads(l) for l in open(sys.argv[1])]
prompts = [e for e in ev if "prompt" in e]
assert len(prompts) == 1, f"{len(prompts)} prompts, want one cartridge change: {prompts}"
p = prompts[0]
assert p["swap_rc"] == 0 and p["reply"] == "", f"the swap failed: {p}"
k1 = int(sys.argv[3])
assert f"holds slice {k1 + 1} into" in p["line"], f"the prompt is {p['line']!r}, want slice {k1 + 1}"
# One tar run, begun before the prompt and waiting on its input across the swap (item 267). The
# positive control for "a tar was running" is this very reading, which finds it: an absence would
# prove nothing without it.
assert len(p["tar_at_prompt"]) == 1, f"want the one tar run waiting when the prompt came: {p}"
assert p["tar_at_reply"] == p["tar_at_prompt"], f"tar {p['tar_at_prompt']} at the prompt, {p['tar_at_reply']} when it was answered"
# Item 267: nothing of a slice waits in the work directory at the prompt. The note's first line
# says how many work directories it looked in (the positive control), then each entry.
note = open(sys.argv[2]).read().splitlines()
assert note and note[0] == "WORKDIR 1", f"the work directory was not there at the prompt: {note}"
entries = [l.split(" ", 1)[1] for l in note[1:]]
assert entries, "positive control: the work directory held nothing at all, so 'no slice in it' proves nothing"
slices = [e for e in entries if e.startswith("slice.")]
assert not slices, f"slice file(s) wait in the work directory at the prompt: {slices}"
print(f"one prompt for slice {k1 + 1}; tar {p['tar_at_prompt'][0]} waited across the swap; the work directory held {len(entries)} entr(ies), no slice")
PY
    if compgen -G "$dest/.tapectl-work*" >/dev/null; then echo "the work directory was left in $dest"; return 1; fi
}
check span_heir_tapes_1_3 step_span_heir_tapes_1_3

step_span_heir_tape_2() {
    local h="$SP/a/heir2" dest="$SP/a/heir2/from-2" key="$SP/a/home/keys/alice-primary.age.key" rc needle
    sp_ready || return 1
    sp_load SPA-0002 && sp_restore_sh "$h" || return 1
    (cd "$h" && TAPE_DEVICE="$TAPE_DEV" ./RESTORE.sh --restore --unit sb-span --key "$key" --to "$dest" </dev/null >"$h/from-2.out" 2>&1)
    rc=$?
    cat "$h/from-2.out"
    [ "$rc" -eq 0 ] || { echo "RESTORE.sh from tape 2 alone exited $rc, want 0"; return 1; }
    sp_has "$h/from-2.out" "RESTORE COMPLETE" || return 1
    diff -r "$SP/a/src/sb-span" "$dest" || { echo "the restore from tape 2 differs from the source"; return 1; }
    # Tape 2 holds sb-span whole: none of a restore across tapes' lines. The positive control is
    # the restore from tapes 1 and 3, whose output holds each of them.
    for needle in "$SP_PROMPT_HEIR" " needs slice " "This cartridge identifies as"; do
        grep -F -- "$needle" "$SP/a/heir/from-1-3.out" >/dev/null \
            || { echo "positive control: the restore from tapes 1 and 3 lacks '$needle', so its absence here proves nothing"; return 1; }
        grep -F -- "$needle" "$h/from-2.out" >/dev/null
        case $? in
            0) echo "tape 2 alone printed '$needle': it holds sb-span whole"; return 1 ;;
            1) ;;
            *) echo "grep failed on $h/from-2.out"; return 1 ;;
        esac
    done
    echo "tape 2 alone restored sb-span whole: no prompt"
}
check span_heir_tape_2 step_span_heir_tape_2

step_span_heir_stream_swap() {
    local h="$SP/a/heir" dest="$SP/a/heir/stream" key="$SP/a/home/keys/alice-primary.age.key" rc swap
    sp_ready && sp_a_figures || return 1
    [ -x "$h/RESTORE.sh" ] || { echo "no RESTORE.sh from tape 1: span_heir_tapes_1_3 did not get that far"; return 1; }
    sp_load SPA-0001 || return 1
    swap="$(printf 'sp_work_note %q %q && sp_swap %q %q' "$dest" "$h/stream-work.txt" "${SP_SLOT[SPA-0003]}" "$SP_WAIT")"
    (cd "$h" && sp_drive "$h/stream.out" "$h/stream.events" "$SP_PROMPT_HEIR" quit --swap "$swap" -- \
        env TAPE_DEVICE="$TAPE_DEV" ./RESTORE.sh --restore --unit sb-span --key "$key" --to "$dest")
    rc=$?
    cat "$h/stream.out"
    [ "$rc" -eq 0 ] || { echo "RESTORE.sh across tapes 1 and 3, the drive empty at the swap, exited $rc, want 0"; return 1; }
    # "the drive is empty; waiting" is sp_swap's own line, run by the prompt driver: the proof the
    # drive really was empty while the prompt waited. The prompt "waits for as long as you take"
    # and holds no device (tests/heir_restore.rs the_prompt_waits_without_a_time_limit_and_holds_no_device).
    sp_has "$h/stream.out" "RESTORE COMPLETE" \
        "This waits for as long as you take." \
        "sp_swap: the drive is empty; waiting ${SP_WAIT}s" \
        "This cartridge identifies as: SPA-0003" || return 1
    diff -r "$SP/a/src/sb-span" "$dest" || { echo "the restore across the long swap differs from the source"; return 1; }
    python3 - "$h/stream.events" "$SP_WAIT" <<'PY' || return 1
import json, sys
ev = [json.loads(l) for l in open(sys.argv[1])]
wait = int(sys.argv[2])
prompts = [e for e in ev if "prompt" in e]
assert len(prompts) == 1, f"{len(prompts)} prompts, want one cartridge change: {prompts}"
p = prompts[0]
assert p["swap_rc"] == 0 and p["reply"] == "", f"the swap failed: {p}"
assert len(p["tar_at_prompt"]) == 1, f"want the one tar run waiting when the prompt came: {p}"
assert p["tar_at_reply"] == p["tar_at_prompt"], (
    f"tar {p['tar_at_prompt']} at the prompt, {p['tar_at_reply']} when it was answered: "
    "the tar run did not wait across the swap (item 267)")
waited = p["t_reply"] - p["t_prompt"]
assert waited >= wait, f"the swap took {waited:.0f}s, less than the {wait}s wait"
print(f"tar {p['tar_at_prompt'][0]} waited on its input across the swap, {waited:.0f}s with the drive empty for {wait}s of it")
PY
}
check span_heir_stream_swap step_span_heir_stream_swap

# Whatever a restore into <dest> left, judged against the source tree: every regular file that is
# there is the source's, byte for byte; every regular file of the source is there or on the damaged-
# file list (never both, never neither); and each <listed> is on the list. No work directory is left.
# tests/heir_restore.rs a_start_on_the_second_tape_alone_restores_what_it_holds_whole_and_lists_the_rest.
sp_partition() { # sp_partition <source dir> <dest> [listed path...]
    python3 - "$@" <<'PY'
import os, sys
src, dest, want = sys.argv[1], sys.argv[2], sys.argv[3:]
def files(root, skip=()):
    out = {}
    for r, ds, fs in os.walk(root):
        ds[:] = [d for d in ds if not d.startswith(".tapectl-work")]
        for f in fs:
            full = os.path.join(r, f)
            rel = os.path.relpath(full, root)
            if rel not in skip and not os.path.islink(full):
                out[rel] = full
    return out
srcf = files(src)
listf = os.path.join(dest, ".tapectl-damaged-files.txt")
listed = []
if os.path.exists(listf):
    listed = [l.rstrip("\n") for l in open(listf, encoding="utf-8", errors="surrogateescape")][1:]
there = files(dest, skip=(".tapectl-damaged-files.txt",))
bad = []
for rel, full in there.items():
    if rel not in srcf: bad.append(f"{rel}: restored but not in the source")
    elif open(full, "rb").read() != open(srcf[rel], "rb").read(): bad.append(f"{rel}: restored and differs from the source")
    if rel in listed: bad.append(f"{rel}: restored and also on the damaged-file list")
for rel in srcf:
    if rel not in there and rel not in listed: bad.append(f"{rel}: neither restored nor listed")
for rel in want:
    if rel not in listed: bad.append(f"{rel}: not on the damaged-file list")
    if rel in there: bad.append(f"{rel}: on the list yet on disk")
for d in os.listdir(dest):
    if d.startswith(".tapectl-work"): bad.append(f"{d}: the work directory was left")
assert not bad, "\n".join(bad)
print(f"{len(there)} file(s) restored whole, {len(listed)} listed ({', '.join(listed) or 'none'}), of {len(srcf)}")
PY
}

# span_heir_start_later: a restore STARTED on tape 3, which holds only the later slices of sb-span.
# The first slice it needs is not there: the prompt for it is answered "done" ("I do not have it")
# and the run goes on with what tape 3 holds whole, lists the rest, and says where the rest is.
step_span_heir_start_later() {
    local h="$SP/a/heir3" dest="$SP/a/heir3/from-3" key="$SP/a/home/keys/alice-primary.age.key" rc
    sp_ready && sp_a_figures || return 1
    sp_load SPA-0003 && sp_restore_sh "$h" || return 1
    (cd "$h" && sp_drive "$h/from-3.out" "$h/from-3.events" "$SP_PROMPT_HEIR" "done" -- \
        env TAPE_DEVICE="$TAPE_DEV" ./RESTORE.sh --restore --unit sb-span --key "$key" --to "$dest")
    rc=$?
    cat "$h/from-3.out"
    [ "$rc" -eq 1 ] || { echo "a restore started on tape 3 exited $rc, want 1 (what is not on it is listed)"; return 1; }
    # tests/heir_restore.rs a_start_on_the_second_tape_alone_restores_what_it_holds_whole_and_lists_the_rest
    sp_has "$h/from-3.out" "RESTORE INCOMPLETE" \
        "needs slice 1 of $SP_A_N next" \
        "NOT ON THIS CARTRIDGE: slices 1-$SP_A_K1 of $SP_A_N" \
        "tape SPA-0001 (cartridge serial" \
        "a folder's modification time is that of the last run that touched the folder" \
        "a hard-linked name whose first name was not read is not made until that name is" || return 1
    # The long file begins on tape 1 (the hook ended tape 1 inside its run): it is left out.
    sp_partition "$SP/a/src/sb-span" "$dest" deep/forty-megs.bin || return 1
    python3 - "$h/from-3.events" <<'PY' || return 1
import json, sys
ev = [json.loads(l) for l in open(sys.argv[1])]
prompts = [e for e in ev if "prompt" in e]
assert len(prompts) == 1, f"{len(prompts)} prompts, want one (\"done\" is not asked again): {prompts}"
assert prompts[0]["reply"] == "done", prompts[0]
PY
}
check span_heir_start_later step_span_heir_start_later

# span_heir_second_run: the list that run left is the input of the next. Tape 1 is loaded, the
# same --to is given, and the one prompt (for slice K1+1, which only tape 3 holds) is answered by
# loading tape 3: the unit comes back whole, the list is gone, and every file the first run had
# already restored is the very same file (a file that passes is not rewritten).
step_span_heir_second_run() {
    local h="$SP/a/heir3" dest="$SP/a/heir3/from-3" key="$SP/a/home/keys/alice-primary.age.key" rc swap before after
    sp_ready && sp_a_figures || return 1
    [ -f "$dest/.tapectl-damaged-files.txt" ] || { echo "span_heir_start_later did not leave the list this run continues"; return 1; }
    before="$(find "$dest" -type f ! -name .tapectl-damaged-files.txt -printf '%p %i %T@\n' | sort)"
    [ -n "$before" ] || { echo "the first run restored no file: the second run's 'already restored' control would be empty"; return 1; }
    sp_load SPA-0001 || return 1
    swap="$(printf 'sp_work_note %q %q && sp_swap %q' "$dest" "$h/second-work.txt" "${SP_SLOT[SPA-0003]}")"
    (cd "$h" && sp_drive "$h/second.out" "$h/second.events" "$SP_PROMPT_HEIR" quit --swap "$swap" -- \
        env TAPE_DEVICE="$TAPE_DEV" ./RESTORE.sh --restore --unit sb-span --key "$key" --to "$dest")
    rc=$?
    cat "$h/second.out"
    [ "$rc" -eq 0 ] || { echo "the second run exited $rc, want 0"; return 1; }
    sp_has "$h/second.out" "RESTORE COMPLETE" "needs slice $((SP_A_K1 + 1)) of $SP_A_N next" "This cartridge identifies as: SPA-0003" || return 1
    diff -r "$SP/a/src/sb-span" "$dest" || { echo "the unit after the second run differs from the source"; return 1; }
    [ ! -e "$dest/.tapectl-damaged-files.txt" ] || { echo "the damaged-file list is still there after a complete run"; return 1; }
    after="$(find "$dest" -type f -printf '%p %i %T@\n' | sort)"
    if [ -n "$(comm -23 <(printf '%s\n' "$before") <(printf '%s\n' "$after"))" ]; then
        echo "a file the first run restored was rewritten (path, inode, mtime before and after):"
        comm -23 <(printf '%s\n' "$before") <(printf '%s\n' "$after")
        return 1
    fi
    python3 - "$h/second.events" <<'PY' || return 1
import json, sys
ev = [json.loads(l) for l in open(sys.argv[1])]
prompts = [e for e in ev if "prompt" in e]
assert len(prompts) == 1 and prompts[0]["swap_rc"] == 0, f"want one prompt, answered by the swap: {prompts}"
PY
}
check span_heir_second_run step_span_heir_second_run

# span_heir_not_have: tape 1 alone. "I do not have it" at the prompt for slice K1+1 (the answer
# "done", and the end of the input, are the same answer) ends the run: exit 1, the long file tar
# had begun is removed and listed, no cartridge is asked for again. "quit" stops by name.
step_span_heir_not_have() {
    local h="$SP/a/heir4" key="$SP/a/home/keys/alice-primary.age.key" d rc prompts
    sp_ready && sp_a_figures || return 1
    sp_load SPA-0001 && sp_restore_sh "$h" || return 1
    # done, through the driver.
    d="$h/done"
    (cd "$h" && sp_drive "$h/done.out" "$h/done.events" "$SP_PROMPT_HEIR" "done" -- \
        env TAPE_DEVICE="$TAPE_DEV" ./RESTORE.sh --restore --unit sb-span --key "$key" --to "$d")
    rc=$?
    cat "$h/done.out"
    # tests/heir_restore.rs the_same_file_answered_i_do_not_have_it_is_listed_and_leaves_no_partial_file
    [ "$rc" -eq 1 ] || { echo "'done' at the prompt exited $rc, want 1"; return 1; }
    sp_has "$h/done.out" "RESTORE INCOMPLETE" "needs slice $((SP_A_K1 + 1)) of $SP_A_N next" \
        "NOT ON THIS CARTRIDGE: slices $((SP_A_K1 + 1))-$SP_A_N of $SP_A_N" || return 1
    sp_partition "$SP/a/src/sb-span" "$d" deep/forty-megs.bin || return 1
    python3 - "$h/done.events" <<'PY' || return 1
import json, sys
prompts = [e for e in (json.loads(l) for l in open(sys.argv[1])) if "prompt" in e]
assert len(prompts) == 1 and prompts[0]["reply"] == "done", f"want one prompt, answered done, not asked again: {prompts}"
PY
    # The end of the input: the same answer.
    d="$h/eof"
    (cd "$h" && TAPE_DEVICE="$TAPE_DEV" ./RESTORE.sh --restore --unit sb-span --key "$key" --to "$d" </dev/null >"$h/eof.out" 2>&1)
    rc=$?
    cat "$h/eof.out"
    [ "$rc" -eq 1 ] || { echo "the end of the input at the prompt exited $rc, want 1"; return 1; }
    sp_has "$h/eof.out" "RESTORE INCOMPLETE" "NOT ON THIS CARTRIDGE: slices $((SP_A_K1 + 1))-$SP_A_N of $SP_A_N" || return 1
    prompts="$(grep -cF -- "$SP_PROMPT_HEIR" "$h/eof.out" || true)"
    [ "$prompts" = 1 ] || { echo "the end of the input was asked $prompts time(s), want once"; return 1; }
    sp_partition "$SP/a/src/sb-span" "$d" deep/forty-megs.bin || return 1
    # quit: stops by name; what is checked stays, the file tar had begun goes.
    d="$h/quit"
    (cd "$h" && sp_drive "$h/quit.out" "$h/quit.events" "$SP_PROMPT_HEIR" quit -- \
        env TAPE_DEVICE="$TAPE_DEV" ./RESTORE.sh --restore --unit sb-span --key "$key" --to "$d")
    rc=$?
    cat "$h/quit.out"
    [ "$rc" -eq 1 ] || { echo "'quit' at the prompt exited $rc, want 1"; return 1; }
    sp_has "$h/quit.out" "RESTORE STOPPED at your request" || return 1
    sp_partition "$SP/a/src/sb-span" "$d" deep/forty-megs.bin || return 1
}
check span_heir_not_have step_span_heir_not_have

step_span_restore_unit() {
    local w="$SP/a/restore" rc
    sp_ready && sp_a_figures || return 1
    mkdir -p "$w"
    sp_load SPA-0001 || return 1
    # Without a terminal tapectl asks nobody and swaps no cartridge: it restores what its own tape
    # holds whole, and lists the rest (INCOMPLETE; src/volume/restore/span_restore_tests.rs
    # with_nobody_at_the_terminal_the_restore_takes_what_its_tape_holds_whole_and_lists_the_rest).
    SPA restore unit --unit sb-span --from SPA-0001 --to "$w/unattended" --device "$TAPE_DEV" </dev/null >"$w/unattended.out" 2>&1
    rc=$?
    cat "$w/unattended.out"
    [ "$rc" -ne 0 ] || { echo "restore unit across cartridges, with no terminal to ask at, said it was complete"; return 1; }
    sp_has "$w/unattended.out" "INCOMPLETE" "forty-megs.bin" || return 1
    [ ! -e "$w/unattended/deep/forty-megs.bin" ] || { echo "the unattended restore left a partial forty-megs.bin"; return 1; }
    if find "$w/unattended" -type f 2>/dev/null | grep -F forty-megs >/dev/null; then echo "the unattended restore left a forty-megs file"; return 1; fi
    # On a terminal: the same command asks for the next cartridge, the whole copy on tape 2
    # first (item 131), and sp_swap loads what it names.
    sp_drive "$w/attended.out" "$w/attended.events" "$SP_PROMPT_TAPECTL" stop --pty --swap "sp_swap asked" -- \
        "$BIN" --home "$SP/a/home" restore unit --unit sb-span --from SPA-0001 --to "$w/attended" --device "$TAPE_DEV"
    rc=$?
    cat "$w/attended.out"
    [ "$rc" -eq 0 ] || { echo "restore unit on a terminal exited $rc, want 0"; return 1; }
    diff -r "$SP/a/src/sb-span" "$w/attended" || { echo "tapectl's restore across cartridges differs from the source"; return 1; }
    python3 - "$w/attended.events" "$TAPE_DEV" <<'PY' || return 1
import json, sys
ev = [json.loads(l) for l in open(sys.argv[1])]
prompts = [e for e in ev if "prompt" in e]
assert len(prompts) == 1, f"{len(prompts)} prompts, want one: {prompts}"
p = prompts[0]
want = f"Unload the cartridge in {sys.argv[2]} (`tapectl drive eject`), load SPA-0002, and press Enter."
assert p["line"].startswith(want), f"the prompt is {p['line']!r}, want it to ask for SPA-0002, the whole copy (item 131)"
assert p["swap_rc"] == 0 and p["reply"] == "", f"the swap failed: {p}"
print("on a terminal it asked for SPA-0002 once and restored sb-span across the swap")
PY
}
check span_restore_unit step_span_restore_unit

step_span_retire_floors() {
    local w="$SP/a/retire" rc t got
    sp_ready && sp_a_figures || return 1
    mkdir -p "$w"
    for t in SPA-0001 SPA-0003; do
        got="$(sp_a_last_copy "$t")"
        [ "$got" = none ] || { echo "the dry run of retiring $t says '$got' of sb-span, want none: tape 2 holds it whole"; return 1; }
    done
    # Tier 2: tape 2 is one whole copy; without it every slice keeps one copy, below the target of 2.
    SPA volume retire SPA-0002 </dev/null >"$w/t2-unconsented.out" 2>&1
    rc=$?
    cat "$w/t2-unconsented.out"
    [ "$rc" -ne 0 ] || { echo "retiring tape 2 needed no consent"; return 1; }
    sp_has "$w/t2-unconsented.out" "refused: non-interactive session with no confirmation given" \
        "unit \"sb-span\" v1 would be left with 1 copy, below its policy of 2" || return 1
    [ "$(sp_a_status SPA-0002)" = sealed ] || { echo "the refused retire changed tape 2"; return 1; }
    SPA --yes volume retire SPA-0002 >"$w/t2.out" 2>&1 || { cat "$w/t2.out"; echo "retiring tape 2 with --yes failed"; return 1; }
    [ "$(sp_a_status SPA-0002)" = retired ] || { echo "tape 2 is not retired"; return 1; }
    # Tier 3: tape 3 alone holds slices K1+1..N, tape 1 alone 1..K1 (and the whole units).
    for t in SPA-0003 SPA-0001; do
        got="$(sp_a_last_copy "$t")"
        [ "$got" = 1 ] || { echo "the dry run of retiring $t says '$got' of sb-span, want v1 as a last copy"; return 1; }
        SPA --yes volume retire "$t" >"$w/$t.out" 2>&1
        rc=$?
        cat "$w/$t.out"
        [ "$rc" -ne 0 ] || { echo "$t was retired holding the last copy of slices of sb-span"; return 1; }
        sp_has "$w/$t.out" "cannot retire volume \"$t\": \"$t\" holds the LAST eligible copy" "sb-span" \
            "There is no --force for this, and --yes does not reach it" || return 1
        [ "$(sp_a_status "$t")" = sealed ] || { echo "the refused retire changed $t"; return 1; }
    done
    # Tape 3 holds no whole unit: its refusal names sb-span alone, while tape 1's (the positive
    # control) names the whole units it holds too.
    sp_has "$w/SPA-0001.out" "sa-whole" "sa-whole-2" || return 1
    if grep -E 'sa-whole|sa-whole-2' "$w/SPA-0003.out" >/dev/null; then echo "tape 3's refusal names a whole unit it does not hold"; return 1; fi
    echo "tape 2 retired at Tier 2 with --yes; tapes 1 and 3 refused at Tier 3, each for slices only it holds"
}
check span_retire_floors step_span_retire_floors

step_span_floor_target_1() {
    local db="$SP/f/home/tapectl.db" rc
    sp_ready || return 1
    sp_home SPF "$SP/f" gate-span-f 1 || return 1
    mkdir -p "$SP/f/src/sf-span"
    head -c 41943040 /dev/urandom >"$SP/f/src/sf-span/forty-megs.bin"
    sp_units SPF "$SP/f" sf-span && sp_assign SPF-0001 || return 1
    sp_new_volume SPF "$SP/f" SPF-0001 && sp_write SPF SPF-0001 "$SP/f/w1" "$SP_HOOK_AT" || return 1
    sp_hook_only_on "$SP/f/w1.err" -- || return 1
    # The state item 128 guards: at a copy target of 1 the range's slices were released from
    # staging, and the version is not current, so the 1.6.0 floor (current versions) sees nothing.
    python3 - "$db" "$SP/f/w1.json" <<'PY' || return 1
import json, sqlite3, sys, os
c = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
j = json.load(open(sys.argv[2]))
def q(sql, *a): return c.execute(sql, a).fetchall()
assert j["ending"] == "short" and len(j["range_units"]) == 1, j
k1 = j["range_units"][0]["last_slice"]
sl = q("""SELECT sl.slice_number, sl.released_at, sl.staging_path FROM stage_slices sl
    JOIN stage_sets ss ON ss.id = sl.stage_set_id ORDER BY sl.slice_number""")
released = [n for n, r, _ in sl if r is not None]
assert released == list(range(1, k1 + 1)), f"released {released}, want 1-{k1} (item 129 at a target of 1)"
kept = [p for n, r, p in sl if r is None]
assert kept and all(p and os.path.exists(p) for p in kept), "the slices past the range are not all still staged"
assert all(p is None for n, r, p in sl if r is not None), "a released slice still names a staging file"
st = q("SELECT status FROM snapshots")[0][0]
assert st == "staged", f"sf-span is {st}: a span in progress is not current (item 128)"
print(f"sf-span: slices 1-{k1} on SPF-0001 only, released from staging; {len(kept)} still staged; the version staged")
PY
    SPF --json --dry-run volume retire SPF-0001 >"$SP/f/retire-dry.json" \
        && python3 -c '
import json, sys
d = json.load(open(sys.argv[1]))
v = [u["last_copy_versions"] for u in d["affected_units"] if u["unit"] == "sf-span"]
assert v == [[1]], f"the dry run reads {d} -- want sf-span v1 as a last copy"' "$SP/f/retire-dry.json" || return 1
    SPF --yes volume retire SPF-0001 >"$SP/f/retire.out" 2>&1
    rc=$?
    cat "$SP/f/retire.out"
    [ "$rc" -ne 0 ] || { echo "SPF-0001 was retired holding the only copy of sf-span's slices 1-K"; return 1; }
    sp_has "$SP/f/retire.out" "cannot retire volume \"SPF-0001\": \"SPF-0001\" holds the LAST eligible copy" \
        "Finish the span first: unit \"sf-span\" v1 is a version still being written across cartridges" || return 1
    [ "$(sp_q "$db" "SELECT status FROM volumes WHERE label = 'SPF-0001'")" = sealed ] || { echo "the refused retire changed SPF-0001"; return 1; }
}
check span_floor_target_1 step_span_floor_target_1

step_span_rebuild_tape_3() {
    local db="$SP/r/home/tapectl.db" pub key="$SP/a/home/keys/gate-span-a-primary.age.key"
    sp_ready && sp_a_figures || return 1
    pub="$(grep -ohE 'age1[a-z0-9]+' "$SP/a/escrow.out" | head -n 1)"
    [ -n "$pub" ] && [ -f "$key" ] || { echo "home A's escrow recipient or operator key is missing"; return 1; }
    mkdir -p "$SP/r/home"
    # The DR recipe (#139): a new home adopting the original escrow recipient; the rebuild reads
    # the tape with the original operator key. Only tape 3 is given.
    SPR init --operator gate-span-a --escrow-public-key "$pub" || { echo "init of the rebuild home failed"; return 1; }
    [ "$(sp_q "$db" "SELECT COUNT(*) FROM units")" = 0 ] || { echo "the rebuild home is not empty before the rebuild"; return 1; }
    sp_load SPA-0003 || return 1
    SPR --json catalog rebuild --from-volume --device "$TAPE_DEV" --key "$key" --label SPA-0003 >"$SP/r/tape3.json" \
        || { cat "$SP/r/tape3.json"; echo "catalog rebuild of SPA-0003 failed"; return 1; }
    cat "$SP/r/tape3.json"
    python3 - "$db" "$SP/r/tape3.json" "$SP_A_K1" "$SP_A_N" <<'PY' || return 1
import json, sqlite3, sys
c = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
j = json.load(open(sys.argv[2]))
k1, n = int(sys.argv[3]), int(sys.argv[4])
def q(sql, *a): return c.execute(sql, a).fetchall()
assert j["range_units"] == [{"unit": "sb-span", "version": 1, "first_slice": k1 + 1, "last_slice": n, "num_slices": n}], j["range_units"]
assert j["versions_promoted"] == [], j["versions_promoted"]
sp = j["spans_in_progress"]
assert len(sp) == 1 and sp[0]["unit"] == "sb-span" and sp[0]["missing_slices"] == list(range(1, k1 + 1)), sp
holders = {h["label"]: (h["first_slice"], h["last_slice"]) for h in sp[0]["holders"]}
assert holders.get("SPA-0001") == (1, k1) and "SPA-0002" in holders, f"tape 3's index names {holders}"
rows = q("""SELECT v.label, v.status, w.status FROM writes w JOIN volumes v ON v.id = w.volume_id""")
assert rows == [("SPA-0003", "sealed", "range")], f"the rebuilt rows: {rows} (item 130)"
slices = q("""SELECT sl.slice_number, wp.status FROM stage_slices sl
    LEFT JOIN write_positions wp ON wp.stage_slice_id = sl.id ORDER BY sl.slice_number""")
assert [s for s, _ in slices] == list(range(1, n + 1)), f"slice rows {slices}: the manifest lists all {n}"
written = [s for s, st in slices if st == "written"]
assert written == list(range(k1 + 1, n + 1)), f"written {written}, want only tape 3's {k1 + 1}-{n}"
st = q("SELECT s.status FROM snapshots s JOIN units u ON u.id = s.unit_id WHERE u.name = 'sb-span'")[0][0]
assert st == "staged", f"sb-span is {st} from tape 3 alone, want staged (0 copies until its other tapes)"
print(f"tape 3 alone: a range {k1 + 1}-{n} of {n} slice rows; slices 1-{k1} named missing, on {sorted(holders)}")
PY
    # The control: tape 1 too, and the version is whole on sealed volumes (item 144).
    sp_load SPA-0001 || return 1
    SPR --json catalog rebuild --from-volume --device "$TAPE_DEV" --key "$key" --label SPA-0001 >"$SP/r/tape1.json" \
        || { cat "$SP/r/tape1.json"; echo "catalog rebuild of SPA-0001 failed"; return 1; }
    python3 - "$db" "$SP/r/tape1.json" <<'PY' || return 1
import json, sqlite3, sys
c = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
j = json.load(open(sys.argv[2]))
assert j["spans_in_progress"] == [] and any("sb-span" in v for v in j["versions_promoted"]), j
st = c.execute("SELECT s.status FROM snapshots s JOIN units u ON u.id = s.unit_id WHERE u.name = 'sb-span'").fetchone()[0]
assert st == "current", f"sb-span is {st} with tapes 1 and 3 rebuilt (item 144)"
print(f"with tape 1 rebuilt too: promoted {j['versions_promoted']}, sb-span current")
PY
}
check span_rebuild_tape_3 step_span_rebuild_tape_3

# Home C's figures (MiB on tape: a 4 MiB slice takes nine 512 KiB blocks, 4.5 MiB; files 0-7
# take one block each, 4 MiB). pb-big, 57 full slices and a short one, is 257 MiB, more than a
# 94 MiB cartridge (98566144 bytes, its room 90 MiB past the fixed files): item 94's case. The
# queue is in unit-name order (item 288): pa-whole (27.5 MiB) first on tape 1, then pb-big, which
# carries the plan past the cartridge's nominal capacity and so is planned to its end and is the
# last set of the plan: pc-small (7 MiB) is not on tape 1. The hook at 114 MiB, 20 MiB past
# the cartridge's end as a real drive's warning comes past nominal, ends tapes 1 and 2 inside
# pb-big, a range each (tape 1 after fewer slices, pa-whole being ahead of it); tape 2 is the owed
# run alone, and tape 3 holds the last of the slices, then pc-small, queued after the owed run.
# Each tape needs post_warning_bytes (item 123); 64M leaves a 4 MiB slice and the seal inside half
# of it (W8), with a 4 MiB reserve.
SP_C_CAPACITY=98566144
SP_C_HOOK_AT=119537664
step_span_three_tapes() {
    local db="$SP/c/home/tapectl.db" h="$SP/c/heir" dest="$SP/c/heir/restored" t swap1 swap2 rc
    local key="$SP/c/home/keys/alice-primary.age.key"
    sp_ready || return 1
    sp_home SPC "$SP/c" gate-span-c 1 \
        && SPC config set 'backends.lto[0].post_warning_bytes' '"64M"' \
        && SPC config set 'backends.lto[0].enospc_buffer' '"4M"' \
        && SPC config check || { echo "home C's drive keys were refused"; return 1; }
    mkdir -p "$SP/c/src/pa-whole" "$SP/c/src/pb-big" "$SP/c/src/pc-small"
    head -c 25165824 /dev/urandom >"$SP/c/src/pa-whole/whole.bin"
    head -c 239075328 /dev/urandom >"$SP/c/src/pb-big/big.bin"
    head -c 6291456 /dev/urandom >"$SP/c/src/pc-small/small.bin"
    sp_units SPC "$SP/c" pa-whole pc-small pb-big || return 1
    for t in SPC-0001 SPC-0002 SPC-0003; do
        sp_assign "$t" \
            && SPC cartridge register --barcode "${SP_TAG[${SP_SLOT[$t]}]}" --generation LTO-8 \
                --capacity "$SP_C_CAPACITY" --notes "gate span leg micro-tape" \
            && sp_new_volume SPC "$SP/c" "$t" --cartridge "${SP_TAG[${SP_SLOT[$t]}]}" || return 1
        python3 -c 'import json, sys; c = json.load(open(sys.argv[1]))["medium"]["capacity_bytes"]; assert c == int(sys.argv[2]), f"{sys.argv[3]} stored capacity {c}"' \
            "$SP/c/$t.init.json" "$SP_C_CAPACITY" "$t" || return 1
        case "$t" in
            SPC-0003) sp_write SPC "$t" "$SP/c/$t" || return 1 ;;
            *) sp_write SPC "$t" "$SP/c/$t" "$SP_C_HOOK_AT" || return 1 ;;
        esac
    done
    sp_hook_only_on "$SP/c/SPC-0001.err" "$SP/c/SPC-0002.err" -- "$SP/c/SPC-0003.err" || return 1
    python3 - "$db" "$SP/c" "$SP_C_CAPACITY" <<'PY' || return 1
import json, sqlite3, sys
c = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
d, cap = sys.argv[2], int(sys.argv[3])
J = {t: json.load(open(f"{d}/{t}.json")) for t in ("SPC-0001", "SPC-0002", "SPC-0003")}
def q(sql, *a): return c.execute(sql, a).fetchall()
B = 512 * 1024
sizes = dict(q("""SELECT u.name, SUM((sl.encrypted_bytes + ? - 1) / ? * ?) FROM stage_slices sl
    JOIN stage_sets ss ON ss.id = sl.stage_set_id JOIN snapshots s ON s.id = ss.snapshot_id
    JOIN units u ON u.id = s.unit_id GROUP BY u.name""", B, B, B))
print(f"on tape: pb-big {sizes['pb-big']}, pa-whole {sizes['pa-whole']}, pc-small {sizes['pc-small']} bytes; cartridges {cap} bytes")
assert sizes["pb-big"] > cap, "pb-big is not larger than one tape: this is not item 94's case"
n = q("""SELECT COUNT(*) FROM stage_slices sl JOIN stage_sets ss ON ss.id = sl.stage_set_id
    JOIN snapshots s ON s.id = ss.snapshot_id JOIN units u ON u.id = s.unit_id WHERE u.name = 'pb-big'""")[0][0]
def rows(label):
    return dict(q("""SELECT u.name, w.status FROM writes w JOIN volumes v ON v.id = w.volume_id
        JOIN stage_sets ss ON ss.id = w.stage_set_id JOIN snapshots s ON s.id = ss.snapshot_id
        JOIN units u ON u.id = s.unit_id WHERE v.label = ?""", label))
def written(label, unit):
    return q("""SELECT sl.slice_number, CAST(wp.position AS INTEGER) FROM write_positions wp
        JOIN writes w ON w.id = wp.write_id JOIN volumes v ON v.id = w.volume_id
        JOIN stage_slices sl ON sl.id = wp.stage_slice_id JOIN stage_sets ss ON ss.id = sl.stage_set_id
        JOIN snapshots s ON s.id = ss.snapshot_id JOIN units u ON u.id = s.unit_id
        WHERE v.label = ? AND u.name = ? AND wp.status = 'written' ORDER BY sl.slice_number""", label, unit)
runs, nxt = [], 1
for t, ending in (("SPC-0001", "short"), ("SPC-0002", "short"), ("SPC-0003", "full")):
    j = J[t]
    assert j["ending"] == ending, f"{t} sealed {j['ending']}, want {ending}"
    w = written(t, "pb-big")
    lo, hi = w[0][0], w[-1][0]
    assert [s for s, _ in w] == list(range(lo, hi + 1)) and lo == nxt, f"{t}: pb-big's run {lo}-{hi} does not follow {nxt - 1}"
    assert j["range_units"] == [{"unit": "pb-big", "version": 1, "first_slice": lo, "last_slice": hi}], (t, j["range_units"])
    runs.append((t, lo, hi))
    nxt = hi + 1
    if t != "SPC-0003":
        want_rows = {"pa-whole": "completed", "pb-big": "range"} if t == "SPC-0001" else {"pb-big": "range"}
        assert rows(t) == want_rows, f"{t}'s rows: {rows(t)}, want {want_rows}"
        assert j["deferred_units"] == ["pc-small"], f"{t}: pc-small was queued after pb-big, so it is not on this tape and stays staged: {j['deferred_units']}"
assert nxt == n + 1, f"the runs {runs} do not reach slice {n}"
assert max(p for _, p in written("SPC-0001", "pa-whole")) < written("SPC-0001", "pb-big")[0][1], "pa-whole does not come before pb-big on tape 1 (name order, item 288)"
assert rows("SPC-0003") == {"pb-big": "range", "pc-small": "completed"}, f"tape 3's rows: {rows('SPC-0003')}"
tail_end = written("SPC-0003", "pb-big")[-1][1]
small_at = [p for _, p in written("SPC-0003", "pc-small")]
assert small_at and min(small_at) > tail_end, f"pc-small at {small_at} is not after pb-big's tail (ends {tail_end})"
want = [{"unit": "pb-big", "version": 1, "tapes": [
    {"label": t, "first_slice": lo, "last_slice": hi} for t, lo, hi in runs]}]
assert J["SPC-0003"]["keep_together"] == want, J["SPC-0003"]["keep_together"]
st = dict(q("SELECT u.name, s.status FROM snapshots s JOIN units u ON u.id = s.unit_id"))
assert st == {"pb-big": "current", "pa-whole": "current", "pc-small": "current"}, f"snapshots {st}"
print(f"pb-big over {runs} of {n}, after pa-whole on tape 1; pc-small deferred twice, then after the owed tail on tape 3")
PY
    # The restore from tape 1, in the default mode, across both swaps.
    sp_load SPC-0001 && sp_restore_sh "$h" || return 1
    swap1="$(printf 'sp_work_note %q %q && sp_swap %q' "$dest" "$h/work-1.txt" "${SP_SLOT[SPC-0002]}")"
    swap2="$(printf 'sp_work_note %q %q && sp_swap %q' "$dest" "$h/work-2.txt" "${SP_SLOT[SPC-0003]}")"
    (cd "$h" && sp_drive "$h/restore.out" "$h/restore.events" "$SP_PROMPT_HEIR" quit --swap "$swap1" --swap "$swap2" -- \
        env TAPE_DEVICE="$TAPE_DEV" ./RESTORE.sh --restore --unit pb-big --key "$key" --to "$dest")
    rc=$?
    tail -n 60 "$h/restore.out"
    [ "$rc" -eq 0 ] || { echo "RESTORE.sh across three tapes exited $rc, want 0"; return 1; }
    sp_has "$h/restore.out" "RESTORE COMPLETE" "This cartridge identifies as: SPC-0002" "This cartridge identifies as: SPC-0003" || return 1
    diff -r "$SP/c/src/pb-big" "$dest" || { echo "the restore across three tapes differs from the source"; return 1; }
    python3 - "$db" "$h" <<'PY' || return 1
import json, sqlite3, sys
c = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
h = sys.argv[2]
ev = [json.loads(l) for l in open(f"{h}/restore.events")]
prompts = [e for e in ev if "prompt" in e]
assert len(prompts) == 2 and all(p["swap_rc"] == 0 for p in prompts), f"want two swaps: {prompts}"
# ONE tar run across both swaps (item 267): the same process at both prompts and both replies.
tars = {tuple(p["tar_at_prompt"]) for p in prompts} | {tuple(p["tar_at_reply"]) for p in prompts}
assert len(tars) == 1 and len(next(iter(tars))) == 1, f"tar processes at the two prompts and replies: {tars}"
ends = [r[0] for r in c.execute("""SELECT MAX(sl.slice_number) FROM write_positions wp
    JOIN writes w ON w.id = wp.write_id JOIN volumes v ON v.id = w.volume_id
    JOIN stage_slices sl ON sl.id = wp.stage_slice_id
    WHERE v.label IN ('SPC-0001', 'SPC-0002') AND w.status = 'range' AND wp.status = 'written'
    GROUP BY v.label ORDER BY v.label""")]
out = open(f"{h}/restore.out").read()
for k, end in enumerate(ends, 1):
    assert f"holds slice {end + 1} into" in prompts[k - 1]["line"], f"prompt {k} is {prompts[k - 1]['line']!r}, want slice {end + 1}"
    assert f"needs slice {end + 1} of " in out, f"no 'needs slice {end + 1}' line before prompt {k}"
    note = open(f"{h}/work-{k}.txt").read().splitlines()
    assert note and note[0] == "WORKDIR 1", f"work directory at prompt {k}: {note}"
    assert len(note) > 1, f"positive control: the work directory held nothing at prompt {k}"
    assert not [l for l in note[1:] if l.startswith("ENTRY slice.")], f"slice file(s) wait in the work directory at prompt {k}: {note}"
print(f"two swaps, at slices {[e + 1 for e in ends]}; one tar run across both; no slice in the work directory at either")
PY
}
check span_three_tapes step_span_three_tapes

# The gate's own cartridge back in the drive. When every span check passed, the leg's bulk
# (sources, staging, restored trees) is removed and the logs, JSON reports and catalogs stay;
# after a failure all of it is kept, so the failure can be read from what it left.
if [ -z "$SP_PREP" ]; then
    if [ -n "$SP_ORIG" ]; then
        sp_swap "$SP_ORIG" >/dev/null || echo "gate: span leg: could not put the gate's cartridge back from slot $SP_ORIG"
    fi
    sp_all_passed=1
    for sp_name in span_rows span_heir_tapes_1_3 span_heir_tape_2 span_heir_stream_swap span_heir_start_later \
        span_heir_second_run span_heir_not_have span_restore_unit \
        span_retire_floors span_floor_target_1 span_rebuild_tape_3 span_three_tapes; do
        [ "${RESULT[$sp_name]:-}" = PASS ] || sp_all_passed=0
    done
    if [ "$sp_all_passed" = 1 ]; then
        rm -rf "$SP"/{a,c,f}/src "$SP"/{a,c,f}/home/staging "$SP"/a/heir/from-1-3 "$SP"/a/heir/stream \
            "$SP"/a/heir2/from-2 "$SP"/a/heir3/from-3 "$SP"/a/heir4/{done,eof,quit} "$SP"/a/restore/attended \
            "$SP"/a/restore/unattended "$SP"/c/heir/restored
    else
        echo "gate: span leg: a span check failed; its sources, staging and restored trees are kept in $SP"
    fi
fi

# ---------- leg 6: the Rust on-media suite (issue #259) ----------
# This gate ran five legs of bash and never once invoked tests/mhvtl_e2e.rs --
# the only place in the tree that produces medium evidence on real media. So
# "GATE GREEN 26/26" was a true statement about the bash legs and said nothing
# whatever about the Rust suite, whose 12 tests had gone unrun by anything
# routine for 105 commits. Asserting a destructive catalog effect in a file
# nothing executes is not coverage.
#
# It runs LAST, after leg 5 has finished with the tape, because it manages its
# own cartridge state (mhvtl_load + its own volumes) and must not interleave
# with the bash legs' positioning. It costs ~38s.
#
# The build lock is taken here for the same reason line 79 takes it and for no
# other: `cargo test` compiles. It is NOT wrapped around the whole leg -- the
# tape work links nothing and must not hold the lock while a worker waits.
# #471: a cargo leg passes only when every test it is there for ran and said
# `ok`. cargo exits 0 when a name filter matches nothing, and an on-media test
# that skips returns early and also says `ok`; TAPECTL_GATE_REQUIRE_MEDIA turns
# such a skip into a panic, and ran_ok names each test that must have run.
ignored_tests() { # the #[ignore] test functions of a test file, one per line
    awk '/^#\[ignore/{f=1; next} f && /^fn [a-z_0-9]+\(/{sub(/^fn /,""); sub(/\(.*/,""); print; f=0}' "$1"
}
ran_ok() { # ran_ok <log> <name>...: each named test reported ok
    local log="$1" n; shift
    [ "$#" -gt 0 ] || { echo "no test names to look for: the leg would prove nothing"; return 1; }
    for n in "$@"; do
        grep -E "^test $n \.\.\. ok\$" "$log" >/dev/null || { echo "test $n did not run and pass (see $log)"; return 1; }
    done
    echo "ran and passed: $*"
}
cargo_leg() { # cargo_leg <log> <want-count> <test-file> cargo-args...: run, then check every ignored test of the file
    local log="$1" want="$2" file="$3" names rc; shift 3
    names="$(ignored_tests "$GATE_REPO/$file")"
    [ "$(wc -w <<<"$names")" -ge "$want" ] \
        || { echo "found $(wc -w <<<"$names") ignored test(s) in $file, expected at least $want: the name extraction is broken"; return 1; }
    flock -w 1200 -E 99 /scratch/tapectl-build.lock \
        env TAPECTL_GATE_TAPE="$TAPE_DEV" TAPECTL_MHVTL=1 TAPECTL_GATE_REQUIRE_MEDIA=1 \
        cargo test "$@" >"$log" 2>&1
    rc=$?
    cat "$log"
    [ "$rc" = 0 ] || return "$rc"
    # shellcheck disable=SC2086 # one name per word, by construction
    ran_ok "$log" $names
}
# 14 since #420 (the short seal at the test hook's warning, and the seal lost past
# the cartridge's end): ignored_tests names every one, and the floor catches an
# extraction that found too few.
step_rust_e2e() {
    cargo_leg "$RUN/rust-e2e.out" 14 tests/mhvtl_e2e.rs --test mhvtl_e2e -- --ignored --nocapture
}
check rust_e2e          step_rust_e2e

# Issue #416 item 12: TapeStore's own command count held to the kernel's
# other_cnt. It writes the loaded cartridge, so it runs after every leg that
# reads what the earlier legs wrote.
step_rust_motions() {
    cargo_leg "$RUN/rust-motions.out" 1 tests/mhvtl_motions.rs --test mhvtl_motions -- --ignored --nocapture
}
check rust_motions      step_rust_motions

# #388: Logical Block Protection through the library on the TD6 drive (tests/mhvtl_lbp.rs says
# what each test proves), at a 262,144-byte payload: mhvtl cannot carry tapectl's 524,292-byte
# wire block in one command. It writes the TD6 drive's cartridge, never this gate drive's, and
# leaves that drive at 524,288 bytes with its page at method 0, which
# lbp_page_clear_after_every_command reads again at the end. 6 since 388d's probe after a read
# off the data: ignored_tests names every one, and the floor catches an extraction that found too few.
step_rust_lbp() {
    TAPECTL_GATE_TAPE_TD6="$TAPE_TD6" \
        cargo_leg "$RUN/rust-lbp.out" 6 tests/mhvtl_lbp.rs --test mhvtl_lbp -- --ignored --nocapture
}
check rust_lbp          step_rust_lbp

# Issue #416 item 9: a 2 GiB write and full confirm through the real write
# session into a MemStore, timed against this machine's own sha256 (so a loaded
# box does not fail it; an order-of-magnitude slowdown does). It holds in a
# debug build (measured 2026-10-15: 1.09x of two hash passes, ceiling 3x, ~80 s,
# 2 GiB of memory), so the gate needs no release build. No tape.
step_perf_write_2g() {
    local log="$RUN/perf-write-2g.out" rc
    flock -w 1200 -E 99 /scratch/tapectl-build.lock \
        env TAPECTL_PERF_TESTS=1 \
        cargo test --test performance perf_a_2gib_write_and_full_confirm_through_a_store \
            -- --ignored --nocapture >"$log" 2>&1
    rc=$?
    cat "$log"
    [ "$rc" = 0 ] || return "$rc"
    # A renamed test would match no filter, run nothing and exit 0 (#471).
    ran_ok "$log" perf_a_2gib_write_and_full_confirm_through_a_store
}
check perf_write_2g     step_perf_write_2g

# #388, ADR-0012 item 164: tapectl clears the page on every exit, and RESTORE.sh, which never
# touches it, needs it clear. Judged LAST, over the row check() wrote after every step before
# this one:
#   - every row parsed, and reads method 0 with neither check on;
#   - rows exist for the steps that write, verify, resume, restore and run RESTORE.sh (a
#     positive control: the rows are the gate's own steps, not an empty file);
#   - no output in the run holds tapectl's leftover note (an open that found the page armed
#     prints it, so a command that left it armed shows up in the next command's output, even
#     though that open cleared it). rust_lbp's output is left out: its leftover test arms the
#     TD6 drive's page by hand on purpose. Positive control first: the same search finds the
#     fallback warning in the write's log;
#   - both drives' pages, read now: the gate drive's and rust_lbp's TD6.
LBP_LEFTOVER_NOTE="had its Logical Block Protection page armed when tapectl opened it"
step_lbp_page_clear_after_every_command() {
    [ -s "$LBP_PAGES" ] || { echo "positive control: $LBP_PAGES is missing or empty"; return 1; }
    python3 - "$LBP_PAGES" <<'PY' || return 1
import sys
rows = [l.rstrip("\n").split("\t", 1) for l in open(sys.argv[1]) if l.strip()]
steps = {r[0] for r in rows}
need = ["volume_write", "volume_verify", "restore_diff", "heir_restore", "resume_midwrite",
        "resume_after_crash", "rust_e2e", "rust_motions"]
missing = [s for s in need if s not in steps]
assert not missing, f"positive control: no page row after {missing} -- check() did not record them"
bad = [r for r in rows if len(r) != 2 or not r[1].startswith("method=00 ") or not r[1].endswith(" flags=00")]
assert not bad, "the page did not read method 0 with neither check on after:\n  " + "\n  ".join("\t".join(r) for r in bad)
print(f"{len(rows)} page readings, one after every step; every one method 0, neither check on")
PY
    local hits rc
    hits="$(grep -rlF --exclude='rust-lbp.out' --exclude='log-rust_lbp.txt' \
        --exclude='log-lbp_page_clear_after_every_command.txt' -- "$LBP_WARNING" "$RUN")"
    rc=$?
    if [ "$rc" -ne 0 ] || ! grep -xF -- "$RUN/log-volume_write.txt" >/dev/null <<<"$hits"; then
        echo "positive control: grep -rl over $RUN (rc=$rc) did not find the fallback warning in log-volume_write.txt -- the search below could not have found the leftover note either"
        return 1
    fi
    hits="$(grep -rlF --exclude='rust-lbp.out' --exclude='log-rust_lbp.txt' \
        --exclude='log-lbp_page_clear_after_every_command.txt' -- "$LBP_LEFTOVER_NOTE" "$RUN")"
    rc=$?
    case "$rc" in
        0) echo "a tapectl open found the gate drive's page armed (a command before it left it so), in:"
           printf '%s\n' "$hits"
           return 1 ;;
        1) ;;
        *) echo "grep failed (rc=$rc) searching $RUN for the leftover note -- inconclusive, not clean"
           return 1 ;;
    esac
    echo "no tapectl open found the page armed; the same search found the fallback warning"
    local now
    now="$(lbp_page_state "$DRIVE_SG")" || { echo "the gate drive's page cannot be read now ($DRIVE_SG)"; return 1; }
    case "$now" in
        "method=00 "*" flags=00") echo "the gate drive's page now: $now" ;;
        *) echo "the gate drive's page now: $now -- not method 0 with neither check on"; return 1 ;;
    esac
    if [ -n "${TD6_SG:-}" ]; then
        now="$(lbp_page_state "$TD6_SG")" || { echo "the TD6 drive's page cannot be read now ($TD6_SG)"; return 1; }
        case "$now" in
            "method=00 "*" flags=00") echo "the TD6 drive's page now: $now" ;;
            *) echo "the TD6 drive's page now: $now -- rust_lbp left it armed"; return 1 ;;
        esac
    fi
}
check lbp_page_clear_after_every_command step_lbp_page_clear_after_every_command

# ---------- the checks the gate must have run, by name ----------
# Every `check` line above, in the order they run (T4.16: 78; with the open issues' legs: 79). The verdict compares the run to this
# list by NAME, never by a total: a check named here that did not run is a FAIL (a leg that was
# edited out, or a run that stopped early, cannot read GREEN), and so is a check that ran and is
# not named here. TAPECTL_GATE_SPAN_ONLY judges the span_* names alone.
GATE_EXPECTED_CHECKS=(
    init tenants units snapshots stage_main stage_symlink_unit erase_scratch volume_init
    volume_write sealed_at_recorded lbp_fallback_recorded volume_verify empty_drive_refused
    evidence_row restore_diff restore_multislice_unit restore_symlink_unit crosskey_rejected
    no_plaintext_leak heir_extract_script heir_info heir_find_envelope heir_verify heir_list
    heir_restore heir_restore_tmpdir heir_restore_symlinks heir_file_check heir_path heir_long_member heir_where
    heir_find heir_find_path heir_paths_from_copy resume_bot resume_midwrite resume_verify
    resume_restore resume_after_crash tier3_floor_unconfirmed health_by_id_device
    health_by_id_restore log_page_sweep_complete log_page_read_once log_page_raw_kept
    mam_journal_attributed contacts_name_their_drive tape_alert_surfaced feed_ratio_recorded
    st_stats_recorded erase_then_init_no_force heir_short_seal heir_short_info heir_short_verify
    heir_short_restore heir_short_cut_refused heir_gzip_restore heir_gzip_path heir_xz_restore
    heir_xz_path heir_zstd_restore heir_zstd_path span_rows span_heir_tapes_1_3 span_heir_tape_2
    span_heir_stream_swap span_heir_start_later span_heir_second_run span_heir_not_have
    span_restore_unit span_retire_floors span_floor_target_1 span_rebuild_tape_3
    span_three_tapes rust_e2e rust_motions rust_lbp perf_write_2g
    lbp_page_clear_after_every_command
)

# ---------- verdict: compare against the EXPECTED_FAIL manifest ----------
echo
echo "== gate verdict =="
# The run record: the tools this run read and wrote with, as RESTORE.sh and tapectl found them.
echo "run record: $("$BIN" --version 2>&1 | head -n 1)"
echo "  age $(age --version 2>&1 | head -n 1)"
echo "  tar (the heir's, on PATH): $(tar --version 2>&1 | head -n 1)"
echo "  tar (tapectl's [tar] binary, $GATE_TAR): $("$GATE_TAR" --version 2>&1 | head -n 1)"
echo "  gzip $(gzip --version 2>&1 | head -n 1); $(xz --version 2>&1 | head -n 1); zstd $(zstd --version 2>&1 | head -n 1)"
rc=0
for name in "${CHECKS[@]}"; do
    want=PASS
    for x in "${EXPECTED_FAIL[@]}"; do [ "$x" = "$name" ] && want=FAIL; done
    got="${RESULT[$name]}"
    if [ "$got" = "$want" ]; then
        [ "$want" = "FAIL" ] && echo "  $name: FAIL (expected — ticketed)" || echo "  $name: PASS"
    else
        if [ "$got" = FAIL ]; then
            echo "  $name: FAIL  << UNEXPECTED — regression (log: $RUN/log-$name.txt)"
        else
            echo "  $name: PASS  << UNEXPECTED — shrink EXPECTED_FAIL in the fixing commit"
        fi
        rc=1
    fi
done
# By name: an expected check that did not run, and a check that ran without being expected.
for name in "${GATE_EXPECTED_CHECKS[@]}"; do
    if [ -n "$GATE_SPAN_ONLY" ] && [[ "$name" != span_* ]]; then continue; fi
    if [ -z "${RESULT[$name]:-}" ]; then
        echo "  $name: NOT RUN  << the gate expects it and it did not run"
        rc=1
    fi
done
for name in "${CHECKS[@]}"; do
    expected=0
    for x in "${GATE_EXPECTED_CHECKS[@]}"; do [ "$x" = "$name" ] && expected=1; done
    if [ "$expected" = 0 ]; then
        echo "  $name: UNLISTED  << it ran and GATE_EXPECTED_CHECKS does not name it"
        rc=1
    fi
done
echo
if [ -n "$GATE_SPAN_ONLY" ]; then
    # Never a gate verdict: every check but span_* was skipped. 3 when those all passed.
    echo "PARTIAL — not a gate verdict: TAPECTL_GATE_SPAN_ONLY ran the ${#CHECKS[@]} span_* check(s) alone$([ $rc -eq 0 ] && echo ", all passing"). Logs: $RUN"
    [ $rc -eq 0 ] && exit 3
    exit 1
fi
if [ $rc -eq 0 ]; then
    echo "GATE GREEN: ${#CHECKS[@]} of ${#GATE_EXPECTED_CHECKS[@]} expected checks, each judged by name (against manifest: ${#EXPECTED_FAIL[@]} expected failures remain). Logs: $RUN"
else
    echo "GATE RED. Logs: $RUN"
fi
exit $rc
