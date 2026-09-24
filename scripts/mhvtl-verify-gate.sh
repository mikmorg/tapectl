#!/usr/bin/env bash
# mhvtl verify gate — tapectl's release-verify analog (renovation ticket #7).
#
# Five legs over a real mhvtl tape, driven through the tapectl BINARY:
#   1. tapectl round trip: init → tenants → units → snapshot → stage →
#      volume init/write/verify → restore → diff -r
#   2. Heir leg (no tapectl, no DB): dd RESTORE.sh off the tape and run
#      --info / --find-envelope / --restore with a tenant key
#   3. Negative leg: cross-tenant decrypt must fail; raw media must not
#      contain plaintext canaries
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
#
# 39 checks as of #301; the `check` lines below are the list.
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
for bin in lsscsi mtx mt dar age sha256sum python3 cargo; do
    command -v "$bin" >/dev/null || die "required binary missing: $bin"
done

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
    TCTL init --operator gate-op --no-escrow
    python3 - "$CFG" "$RUN" "$TAPE_DEV" "$DRIVE_SG" <<'PY'
import sys, re
cfg, run, tape, sg = sys.argv[1:5]
t = open(cfg).read()
t = re.sub(r'(?m)^binary *=.*$', 'binary = "/usr/bin/dar"', t, count=1)
t = re.sub(r'(?m)^slice_size *=.*$', 'slice_size = "1M"', t, count=1)
t = re.sub(r'(?m)^directory *=.*$', f'directory = "{run}/staging"', t, count=1)
t = re.sub(r'(?m)^device_tape *=.*$', f'device_tape = "{tape}"', t)
t = re.sub(r'(?m)^device_sg *=.*$', f'device_sg = "{sg}"', t)
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
device_sg = "{sg}"
generation = "LTO-8"
# 2 748 779 069 440 = 2.5 TiB exactly, written as a bare byte count on
# purpose (issue #200). This string used to be "2.5T", parsed BINARY by
# volume init and DECIMAL by config validation; #200 made init decimal
# too, which would have shrunk this microcosm by 9.95%. Fixing a parser
# drift and resizing the test microcosm are two different changes, and
# doing both at once means a red gate cannot be attributed to either.
# A bare integer is the one literal both parsers read identically.
capacity_override = "2748779069440"
usable_capacity_factor = 0.95
enospc_buffer = "2G"
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
    python3 - "$HOME_DIR/tapectl.db" <<'PYFEED'
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
    if grep -l "warning: volume" "$RUN"/log-*.txt 2>/dev/null | grep -q .; then
        echo "a 'warning: volume' line reached stderr on mhvtl despite capacity_override:" >&2
        grep -H "warning: volume" "$RUN"/log-*.txt >&2
        return 1
    fi
    echo "no 'warning: volume' line in any step log (suppressed on the gate's drive)"
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
            cat "$tmp" >> "$out"
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
# matrix. `volume-format-v2.md` puts the volume label in the ID thunk
# (File 0) in plaintext by design, so if the label is NOT found then the
# scan itself is broken, and this check must fail as loudly as a real leak
# rather than report a clean tape.
#
# Reading the device rather than the directory also makes this check work
# on a real LTO-6, which has no media directory at all -- it is no longer
# mhvtl-only.
#
# The whole tape is scanned, encrypted slices included, rather than only
# the plaintext positions the Rust `mhvtl_no_plaintext_tenant_metadata`
# test parses out of the layout. That is deliberate: it is strictly
# stricter (a leak anywhere fails) and keeps this check independent of the
# layout parser it exists to cross-check. The cost is a chance of a short
# needle appearing in ciphertext by coincidence -- for "unitA" in a
# gate-sized volume that is ~1e-5, and the canary is long and unique.
step_leakscan() {
    local dump="$RUN/leakscan-tape.bin" needle rc
    dump_whole_tape "$dump" || return 1

    # Positive control FIRST: prove the scan can find what must be there
    # before trusting it about what must not be.
    grep -a -q "label = \"$LABEL\"" "$dump" || {
        echo "leakscan: volume label $LABEL is NOT in the tape dump -- the scan is broken, not the tape clean"
        return 1
    }

    # Negative needles. grep's rc 1 (no match) is the real pass; rc >= 2 is
    # an error and must never be read as "clean" -- that read is exactly
    # what issue #275 was.
    for needle in "$CANARY" "unitA"; do
        grep -a -q "$needle" "$dump"
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
HEIR="$RUN/heir"; mkdir -p "$HEIR"
step_heir_extract() {
    mt -f "$TAPE_DEV" rewind && mt -f "$TAPE_DEV" fsf 2 \
    && dd if="$TAPE_DEV" bs=512k 2>/dev/null | tr -d '\0' > "$HEIR/RESTORE.sh" \
    && chmod +x "$HEIR/RESTORE.sh" && bash -n "$HEIR/RESTORE.sh"
}
# RESTORE.sh defaults to `${TAPE_DEVICE:-/dev/nst0}` — the tape an HEIR would
# reach for, NOT necessarily this gate's drive. Every invocation below therefore
# pins TAPE_DEVICE="$TAPE_DEV". Without it the heir leg silently reads /dev/nst0:
# harmless while nst0 happened to be the mhvtl drive, but on a host where nst0 is
# a DIFFERENT (e.g. real) drive it reads the wrong tape entirely — heir_info still
# "passes" against any valid volume there while heir_find/restore fail on keys.
# Assert the tape read is the tape this gate WROTE, not merely some valid
# volume. Without this, heir_info passes against any sealed tape in any drive —
# which is exactly how the wrong-device read above stayed hidden for three runs
# while three later checks failed on "no envelope matched the provided key".
step_heir_info() {
    local out
    out=$( (cd "$HEIR" && TAPE_DEVICE="$TAPE_DEV" ./RESTORE.sh --info) ) || return 1
    printf '%s\n' "$out"
    grep -q "Tape identifies as: $LABEL\$" <<<"$out" ||
        {
            echo "heir_info: tape in $TAPE_DEV is not volume $LABEL — wrong drive or wrong cartridge" >&2
            return 1
        }
    grep -q "Verdict: SEALED" <<<"$out" || {
        echo "heir_info: volume $LABEL is not SEALED" >&2
        return 1
    }
}
step_heir_find() { (cd "$HEIR" && TAPE_DEVICE="$TAPE_DEV" ./RESTORE.sh --find-envelope --key "$HOME_DIR/keys/alice-primary.age.key"); }
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
    (cd "$HEIR" && TAPE_DEVICE="$TAPE_DEV" ./RESTORE.sh --restore --unit unitA --key "$HOME_DIR/keys/alice-primary.age.key" --to "$HEIR/recovered") \
    && diff -r "$SRC/unitA" "$HEIR/recovered"
}
# The heir path is the reason this project exists, so symlink survival is
# checked there too, not only through tapectl. See step_restore_C for why
# --no-dereference is mandatory here.
step_heir_restore_symlinks() {
    (cd "$HEIR" && TAPE_DEVICE="$TAPE_DEV" ./RESTORE.sh --restore --unit unitC --key "$HOME_DIR/keys/alice-primary.age.key" --to "$HEIR/recovered-C") \
    && diff -r --no-dereference "$SRC/unitC" "$HEIR/recovered-C"
}
echo "gate: leg 2 — heir leg (RESTORE.sh, no tapectl)"
check heir_extract_script step_heir_extract
check heir_info           step_heir_info
check heir_find_envelope  step_heir_find
check heir_restore        step_heir_restore
check heir_restore_symlink_unit step_heir_restore_symlinks

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
        "$BIN" --home "$HOME_DIR" --config "$CFG" volume write "$label" --device "$TAPE_DEV" &
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
    "$BIN" --home "$HOME_DIR" --config "$CFG" volume write "$label" --device "$TAPE_DEV" &
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
step_resume_verify() {
    TCTL volume verify "$RLABEL2" --device "$TAPE_DEV" --json | tee "$RUN/verify-resumed.json"
    python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); assert d.get("failed",1)==0 and d.get("passed",0)>0, d' "$RUN/verify-resumed.json"
}
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
        "$BIN" --home "$HOME_DIR" --config "$CFG" volume write "$label" --device "$TAPE_DEV" &
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
    local label="$1" unit="$2" want_resume="$3" out rc
    set +e
    out="$(TCTL volume retire "$label" --yes 2>&1)"
    rc=$?
    set -e
    printf '%s\n' "$out" > "$RUN/retire-refusal-$label.txt"
    if [ $rc -eq 0 ]; then
        echo "assert_retire_refused: $label: retire SUCCEEDED (rc=0). This is issue #276 \
exactly: a sealed-but-unconfirmed volume holding the only copy of \"$unit\" was retired \
with no ADR-0008 Tier-3 refusal. Output:"
        printf '%s\n' "$out"
        return 1
    fi
    for needle in "LAST eligible copy" "$unit"; do
        if ! printf '%s' "$out" | grep -qF "$needle"; then
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
    if printf '%s' "$out" | grep -qF "volume resume"; then
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
    if grep -E '^[[:space:]]*device_tape[[:space:]]*=' "$CFG" | grep -qF "\"$by_id\""; then
        echo "precondition: $CFG already names $by_id as a device_tape -- string equality would carry the lookup"
        return 1
    fi
    before="$(python3 -c 'import sqlite3,sys; print(sqlite3.connect(sys.argv[1]).execute("SELECT COALESCE(MAX(id),0) FROM cartridge_contacts WHERE operation = '"'volume verify'"'").fetchone()[0])' "$HOME_DIR/tapectl.db")" \
        || { echo "could not read the last verify contact id"; return 1; }
    echo "by-id: $by_id -> $(readlink -f "$by_id") (config says $TAPE_DEV); last verify contact before: $before"
    TCTL volume verify "$RLABEL4" --device "$by_id" --json | tee "$RUN/verify-by-id.json"
    python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); assert d.get("failed",1)==0 and d.get("passed",0)>0, d' "$RUN/verify-by-id.json" \
        || return 1
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
    if grep -E '^[[:space:]]*device_tape[[:space:]]*=' "$CFG" | grep -qF "\"$by_id\""; then
        echo "precondition: $CFG already names $by_id as a device_tape -- string equality would carry the lookup"
        return 1
    fi
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
    """SELECT j.id, j.contact_id, j.decoded, c.barcode
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
raised = [(jid, cid) for jid, cid, dec, _ in rows
          if any(l.rstrip().endswith(": 1") for l in dec.splitlines())]
nonzero = d.execute("SELECT id, contact_id, tape_alerts FROM health_logs WHERE tape_alerts > 0").fetchall()
assert not raised and not nonzero, (
    "the UNMODIFIED gate catalog already carries a non-zero TapeAlert -- issue #340's first "
    f"sighting, on mhvtl: journal rows {raised}, health rows {nonzero}. Read it before touching this step.")
jid, cid, dec, barcode = rows[0]
flipped = dec.replace("  Cleaning required: 0", "  Cleaning required: 1", 1) \
             .replace("  Drive temperature: 0", "  Drive temperature: 1", 1)
assert flipped.count(": 1") == 2, "the decode did not carry both flag lines to flip"
d.execute("UPDATE log_page_journal SET decoded = ? WHERE id = ?", (flipped, jid))
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
step_rust_e2e() {
    flock -w 1200 -E 99 /scratch/tapectl-build.lock \
        env TAPECTL_GATE_TAPE="$TAPE_DEV" TAPECTL_MHVTL=1 \
        cargo test --test mhvtl_e2e -- --ignored --nocapture
}
check rust_e2e          step_rust_e2e

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
