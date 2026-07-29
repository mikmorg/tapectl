#!/usr/bin/env bash
# mhvtl verify gate — tapectl's release-verify analog (renovation ticket #7).
#
# Four legs over a real mhvtl tape, driven through the tapectl BINARY:
#   1. tapectl round trip: init → tenants → units → snapshot → stage →
#      volume init/write/verify → restore → diff -r
#   2. Heir leg (no tapectl, no DB): dd RESTORE.sh off the tape and run
#      --info / --find-envelope / --restore with a tenant key
#   3. Negative leg: cross-tenant decrypt must fail; raw media must not
#      contain plaintext canaries
#   4. Evidence leg: verify must leave a verification_sessions row (ADR-0001)
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

TAPE_DEV="${TAPECTL_GATE_TAPE:-/dev/nst0}"
SCRATCH="${TAPECTL_GATE_SCRATCH:-/scratch/tapectl-gate}"
LABEL="MHVTLG"

die() { echo "GATE PRECONDITION FAILED: $*" >&2; exit 2; }

# ---------- preconditions (loud — this gate must never rot quietly) ----------
[ "${TAPECTL_MHVTL:-}" = "1" ] || die "TAPECTL_MHVTL=1 not set"
grep -q '^mhvtl ' /proc/modules \
    || die "mhvtl module not loaded for $(uname -r) — dkms status; see docs/operator-guide.md"
[ -e "$TAPE_DEV" ] || die "$TAPE_DEV missing — systemctl start mhvtl.target"
for bin in lsscsi mtx mt dar age sha256sum python3 cargo; do
    command -v "$bin" >/dev/null || die "required binary missing: $bin"
done

# Single-drive rule (#9): one tape user at a time, across processes.
exec 9>/tmp/tapectl-tape.lock
flock -n 9 || die "another process holds the tape lock (/tmp/tapectl-tape.lock)"

# ---------- device discovery (#67) ----------
ST_BASE="$(basename "$TAPE_DEV")"; ST_BASE="${ST_BASE#n}"   # nst0 -> st0
ROW="$(lsscsi -g | awk -v d="/dev/$ST_BASE" '$0 ~ d" " || $NF ~ d {print; exit}')"
[ -n "$ROW" ] || ROW="$(lsscsi -g | grep -F "/dev/$ST_BASE " | head -1)"
[ -n "$ROW" ] || die "cannot find $TAPE_DEV in lsscsi -g"
HCTL="$(echo "$ROW" | sed -n 's/^\[\([0-9:]*\)\].*/\1/p')"
DRIVE_MODEL="$(echo "$ROW" | awk '{print $4}')"
DRIVE_SG="$(echo "$ROW" | awk '{print $NF}')"
T_CHAN="$(echo "$HCTL" | cut -d: -f2)"; T_TGT="$(echo "$HCTL" | cut -d: -f3)"; T_LUN="$(echo "$HCTL" | cut -d: -f4)"

# device.conf: match Drive by CHANNEL/TARGET/LUN -> queue; owning Library = last Library seen above it
DRIVE_Q="" ; LIB_Q="" ; cur_lib=""
while read -r kind q rest; do
    case "$kind" in
        Library:) cur_lib="$q" ;;
        Drive:)
            c=$(echo "$rest" | sed -n 's/.*CHANNEL: *\([0-9][0-9]*\).*/\1/p')
            t=$(echo "$rest" | sed -n 's/.*TARGET: *\([0-9][0-9]*\).*/\1/p')
            l=$(echo "$rest" | sed -n 's/.*LUN: *\([0-9][0-9]*\).*/\1/p')
            if [ "$((10#${c:-99}))" -eq "$((10#$T_CHAN))" ] \
               && [ "$((10#${t:-99}))" -eq "$((10#$T_TGT))" ] \
               && [ "$((10#${l:-99}))" -eq "$((10#$T_LUN))" ]; then
                DRIVE_Q="$q"; LIB_Q="$cur_lib"
            fi ;;
    esac
done < <(grep -E '^(Library|Drive):' /etc/mhvtl/device.conf)
[ -n "$DRIVE_Q" ] || die "no device.conf Drive matches $TAPE_DEV at $HCTL"
DTE=$((DRIVE_Q - LIB_Q - 1))

# changer sg node: the mediumx row whose C:T:L matches the Library's device.conf entry
LIB_LINE="$(grep -E "^Library: $LIB_Q " /etc/mhvtl/device.conf)"
L_TGT=$(echo "$LIB_LINE" | sed -n 's/.*TARGET: *\([0-9][0-9]*\).*/\1/p')
T_HOST="$(echo "$HCTL" | cut -d: -f1)"
CHG_SG="$(lsscsi -g | grep mediumx \
    | grep -E "^\[$((10#$T_HOST)):$((10#$T_CHAN)):$((10#$L_TGT)):[0-9]+\]" \
    | awk '{print $NF; exit}')"
[ -n "$CHG_SG" ] || die "cannot locate changer sg node for library $LIB_Q"

# media generation from drive model (ULT3580-TDn -> Ln)
GEN="$(echo "$DRIVE_MODEL" | sed -n 's/.*TD\([0-9]\).*/L\1/p')"
[ -n "$GEN" ] || die "cannot derive media generation from drive model '$DRIVE_MODEL'"

STATUS="$(mtx -f "$CHG_SG" status)" || die "mtx status failed on $CHG_SG"
LOADED_TAG="$(echo "$STATUS" | sed -n "s/.*Data Transfer Element $DTE:Full.*VolumeTag *= *\([A-Z0-9]*\).*/\1/p")"
if [ -n "$LOADED_TAG" ] && [ "${LOADED_TAG: -2}" != "$GEN" ]; then
    ORIGIN="$(echo "$STATUS" | sed -n "s/.*Data Transfer Element $DTE:Full (Storage Element \([0-9]*\) Loaded).*/\1/p")"
    mtx -f "$CHG_SG" unload "${ORIGIN:-1}" "$DTE" || die "cannot unload wrong-generation tape"
    LOADED_TAG=""
fi
if [ -z "$LOADED_TAG" ]; then
    SLOT="$(echo "$STATUS" | grep -E "Storage Element [0-9]+:Full" | grep "VolumeTag=[EF][0-9]*$GEN" | head -1 \
            | sed -n 's/.*Storage Element \([0-9]*\):Full.*/\1/p')"
    [ -n "$SLOT" ] || die "no $GEN data cartridge in library $LIB_Q"
    mtx -f "$CHG_SG" load "$SLOT" "$DTE" || die "mtx load $SLOT $DTE failed"
    LOADED_TAG="$(mtx -f "$CHG_SG" status | sed -n "s/.*Data Transfer Element $DTE:Full.*VolumeTag *= *\([A-Z0-9]*\).*/\1/p")"
fi
echo "gate: drive=$TAPE_DEV ($DRIVE_MODEL, sg=$DRIVE_SG) changer=$CHG_SG dte=$DTE tape=$LOADED_TAG"

# ---------- workspace + build ----------
RUN="$SCRATCH/run-$(date +%Y%m%d-%H%M%S)"
mkdir -p "$RUN"
echo "gate: workspace $RUN"
cargo build --quiet || die "cargo build failed"
BIN="${CARGO_TARGET_DIR:-target}/debug/tapectl"
[ -x "$BIN" ] || die "built binary not found at $BIN"

HOME_DIR="$RUN/home"; mkdir -p "$HOME_DIR"
CFG="$HOME_DIR/config.toml"
TCTL() { "$BIN" --config "$CFG" "$@"; }

# ---------- check harness ----------
declare -A RESULT
CHECKS=()
check() { # check <name> <fn>
    local name="$1"; shift
    CHECKS+=("$name")
    if "$@" >"$RUN/log-$name.txt" 2>&1; then RESULT[$name]=PASS; else RESULT[$name]=FAIL; fi
    echo "  [$name] ${RESULT[$name]}"
}
# EMPTY as of 2026-07-29 — every check below must now PASS. Do not add an
# entry here to make a red gate green; a new failure is a regression to fix,
# and the array may only grow via a deliberate, ticketed decision.
#
# History of what used to be pinned here:
#   H1 fixed in #24: the mini-index is generated from the complete Layout, so
#     it now lists the envelopes and the no-tapectl heir path works end-to-end
#     (find-envelope + full restore, byte-identical).
#   H8 fixed in #34: list_slices parses dar's numeric slice index instead of
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
head -c 12000000 /dev/urandom > "$SRC/unitB/twelve-meg.bin"   # ~12 slices @1M
echo "target" > "$SRC/unitC/target.txt"
ln -s target.txt "$SRC/unitC/link-ok"
ln -s /nonexistent-gate-path "$SRC/unitC/link-broken"

# ---------- leg 1: tapectl round trip ----------
step_init() {
    TCTL init --operator gate-op
    python3 - "$CFG" "$RUN" "$TAPE_DEV" "$DRIVE_SG" <<'PY'
import sys, re
cfg, run, tape, sg = sys.argv[1:5]
t = open(cfg).read()
t = re.sub(r'(?m)^binary *=.*$', 'binary = "/usr/bin/dar"', t, count=1)
t = re.sub(r'(?m)^slice_size *=.*$', 'slice_size = "1M"', t, count=1)
t = re.sub(r'(?m)^directory *=.*$', f'directory = "{run}/staging"', t, count=1)
t = re.sub(r'(?m)^device_tape *=.*$', f'device_tape = "{tape}"', t)
t = re.sub(r'(?m)^device_sg *=.*$', f'device_sg = "{sg}"', t)
if '[[backends.lto]]' not in t:
    # `init` writes an empty backends.lto (audit shell-MED); the gate supplies one.
    t = re.sub(r'(?m)^lto *= *\[\] *\n', '', t)  # drop the inline empty array first
    t += f'''
[[backends.lto]]
name = "gate-mhvtl"
device_tape = "{tape}"
device_sg = "{sg}"
media_type = "LTO-6"
nominal_capacity = "2.5T"
usable_capacity_factor = 0.95
manifest_reserve = "1G"
enospc_buffer = "2G"
block_size = "512K"
hardware_compression = false
'''
open(cfg, 'w').write(t)
PY
    mkdir -p "$RUN/staging"
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
# Bulk-erase the scratch cartridge first — the gate reuses one cartridge across
# runs, so from the second run onward it carries a SEALED volume and contact
# discipline (#27) correctly refuses to overwrite it. `--force` cannot defeat
# AlreadySealed by design (ADR-0003), so the honest fix is a real erase, which
# mirrors the production reuse procedure (retire, bulk-erase, mark-erased) and
# is instant on mhvtl. Erasing lets the gate exercise the DEFAULT no-force
# path, which is the one an operator actually runs.
step_erase_scratch_tape() {
    mt -f "$TAPE_DEV" rewind && mt -f "$TAPE_DEV" erase
}
step_vol_init() { TCTL volume init "$LABEL" --device "$TAPE_DEV"; }
step_vol_write() { TCTL volume write "$LABEL" --device "$TAPE_DEV"; }
step_vol_verify() {
    TCTL volume verify "$LABEL" --device "$TAPE_DEV" --json | tee "$RUN/verify.json"
    python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); assert d.get("failed",1)==0 and d.get("passed",0)>0, d' "$RUN/verify.json"
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
check init            step_init
check tenants         step_tenants
check units           step_units
check snapshots       step_snapshots
check stage_main      step_stage_main
check stage_symlink_unit step_stage_symlinks
check erase_scratch   step_erase_scratch_tape
check volume_init     step_vol_init
check volume_write    step_vol_write
check volume_verify   step_vol_verify
check evidence_row    step_evidence
check restore_diff    step_restore_A
check restore_multislice_unit step_restore_B
check restore_symlink_unit    step_restore_C

# ---------- leg 3a: negative crypto + leak scan (before heir leg rewinds) ----------
step_crosskey() {
    # Slices are uuid-named on disk — resolve unitA's first slice via the catalog.
    local slice bobkey
    slice="$(python3 - "$HOME_DIR/tapectl.db" <<'PY'
import sqlite3, sys
row = sqlite3.connect(sys.argv[1]).execute(
    """SELECT sl.staging_path FROM stage_slices sl
       JOIN stage_sets ss ON ss.id = sl.stage_set_id
       JOIN snapshots s ON s.id = ss.snapshot_id
       JOIN units u ON u.id = s.unit_id
       WHERE u.name = 'unitA' AND sl.staging_path IS NOT NULL
       ORDER BY sl.slice_number LIMIT 1"""
).fetchone()
print(row[0] if row else "")
PY
)"
    [ -n "$slice" ] && [ -f "$slice" ] || { echo "no unitA slice found via catalog"; return 1; }
    bobkey="$HOME_DIR/keys/bob-primary.age.key"
    [ -f "$bobkey" ] || { echo "bob key missing"; return 1; }
    if age -d -i "$bobkey" "$slice" >/dev/null 2>&1; then
        echo "bob's key decrypted alice's slice — isolation broken"; return 1
    fi
    return 0
}
step_leakscan() {
    local media="/opt/mhvtl/$LOADED_TAG"
    [ -d "$media" ] || return 1
    if grep -a -rq "$CANARY" "$media"; then return 1; fi
    if grep -a -rq "unitA" "$media" ; then return 1; fi
    return 0
}
echo "gate: leg 3 — negative checks"
check crosskey_rejected step_crosskey
check no_plaintext_leak step_leakscan

# ---------- leg 2: heir leg (no tapectl, no DB) ----------
HEIR="$RUN/heir"; mkdir -p "$HEIR"
step_heir_extract() {
    mt -f "$TAPE_DEV" rewind && mt -f "$TAPE_DEV" fsf 2 \
    && dd if="$TAPE_DEV" bs=512k 2>/dev/null | tr -d '\0' > "$HEIR/RESTORE.sh" \
    && chmod +x "$HEIR/RESTORE.sh" && bash -n "$HEIR/RESTORE.sh"
}
step_heir_info() { (cd "$HEIR" && ./RESTORE.sh --info); }
step_heir_find() { (cd "$HEIR" && ./RESTORE.sh --find-envelope --key "$HOME_DIR/keys/alice-primary.age.key"); }
step_heir_restore() {
    # RESTORE.sh extracts the unit's contents directly into --to (dar restores
    # the unit's own tree), so compare that tree to the source directly — same
    # shape as the tapectl restore_diff leg.
    #
    # `--unit unitA` is REQUIRED, and its absence used to pass only by
    # accident: alice owns both unitA and unitC, but before #33 unitC could
    # never stage, so alice's envelope happened to hold exactly one unit and
    # RESTORE.sh had nothing to disambiguate. With #33 fixed, unitC reaches
    # the tape and RESTORE.sh correctly refuses to guess ("FATAL: multiple
    # units found"). Naming the unit restores the intended assertion — this
    # leg diffs against $SRC/unitA, so it must ask for unitA.
    (cd "$HEIR" && ./RESTORE.sh --restore --unit unitA --key "$HOME_DIR/keys/alice-primary.age.key" --to "$HEIR/recovered") \
    && diff -r "$SRC/unitA" "$HEIR/recovered"
}
# The heir path is the reason this project exists, so symlink survival is
# checked there too, not only through tapectl. See step_restore_C for why
# --no-dereference is mandatory here.
step_heir_restore_symlinks() {
    (cd "$HEIR" && ./RESTORE.sh --restore --unit unitC --key "$HOME_DIR/keys/alice-primary.age.key" --to "$HEIR/recovered-C") \
    && diff -r --no-dereference "$SRC/unitC" "$HEIR/recovered-C"
}
echo "gate: leg 2 — heir leg (RESTORE.sh, no tapectl)"
check heir_extract_script step_heir_extract
check heir_info           step_heir_info
check heir_find_envelope  step_heir_find
check heir_restore        step_heir_restore
check heir_restore_symlink_unit step_heir_restore_symlinks

# ---------- verdict: compare against the EXPECTED_FAIL manifest ----------
echo
echo "== gate verdict =="
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
echo
if [ $rc -eq 0 ]; then
    echo "GATE GREEN (against manifest: ${#EXPECTED_FAIL[@]} expected failures remain). Logs: $RUN"
else
    echo "GATE RED. Logs: $RUN"
fi
exit $rc
