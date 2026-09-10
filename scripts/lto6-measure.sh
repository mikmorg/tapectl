#!/usr/bin/env bash
# LTO-6 hardware measurement harness (docs/design/v2-open-questions.md §5).
#
# Six questions have been deferred to real hardware since the v2 regear,
# because mhvtl cannot answer them — and on the most important one it gives a
# FALSE PASS: past capacity it accepts every write without returning ENOSPC
# and silently produces unreadable slices (dry-run, 2026-07-20). This script
# turns the measurable subset of that session from an exploration into
# "run this, read the output".
#
#   A. block size 512 K vs 1 M — acceptance and throughput
#   B. hardware compression as-found state (must be OFF for encrypted data)
#   C. LBP (logical block protection) MODE SENSE/SELECT acceptance + st readback
#   D. MAM remaining-capacity over-report bound — this sizes the ENOSPC buffer
#   E. EOD semantics — forward ops past EOD must ERROR, not return stale data
#      (§3.2's physics assumption, which nothing has ever tested)
#   F. as-found drive inventory + error counters, for the record
#
# NOT here, deliberately:
#   - the ENOSPC drill. Filling a real LTO-6 cartridge takes hours and the
#     checklist wants the operator watching it. It stays a manual step in
#     docs/lto6-validation-checklist.md.
#   - the raw-recovery drill. That is the heir leg of the mhvtl gate; on
#     hardware it is a confirmation, not a measurement.
#
# THIS SCRIPT ERASES THE LOADED CARTRIDGE. It refuses to start unless you
# name that cartridge on the command line, and it cross-checks the name
# against the barcode in MAM. That is ADR-0008's consent shape — naming the
# thing you are destroying — not a y/n prompt.
#
# Devices are DISCOVERED, never hardcoded (issue #67): SCSI enumeration
# shuffles between reloads and reboots.
set -uo pipefail

TAPE_DEV="${TAPECTL_MEASURE_TAPE:-/dev/nst0}"
OUT_DIR="${TAPECTL_MEASURE_OUT:-/scratch/tapectl-lto6}"
# Payload per throughput run. 2 GiB is enough to swamp buffer effects on an
# LTO-6 (~160 MB/s native) at ~13 s a run, and small enough not to make the
# session tedious. Raise it if the numbers look buffer-dominated.
PAYLOAD_MB="${TAPECTL_MEASURE_MB:-2048}"
ERASE_LABEL=""
ALLOW_UNVERIFIED_BARCODE=0

usage() {
    cat <<'USAGE'
usage: lto6-measure.sh --erase-cartridge <BARCODE> [options]

  --erase-cartridge <BARCODE>   Required. The cartridge you are willing to
                                destroy. Cross-checked against MAM.
  --allow-unverified-barcode    Proceed when MAM will not report a barcode.
  --payload-mb <N>              Per-throughput-run payload (default 2048).
  --device <path>               Tape device (default /dev/nst0).
  --out <dir>                   Recording directory (default /scratch/tapectl-lto6).

Everything written goes to <out>/run-<timestamp>/. Nothing is read from or
written to ~/.tapectl; this script does not use tapectl at all.
USAGE
}

while [ $# -gt 0 ]; do
    case "$1" in
        --erase-cartridge) ERASE_LABEL="${2:-}"; shift 2 ;;
        --allow-unverified-barcode) ALLOW_UNVERIFIED_BARCODE=1; shift ;;
        --payload-mb) PAYLOAD_MB="${2:-}"; shift 2 ;;
        --device) TAPE_DEV="${2:-}"; shift 2 ;;
        --out) OUT_DIR="${2:-}"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
done

die() { echo "MEASURE PRECONDITION FAILED: $*" >&2; exit 2; }

[ -n "$ERASE_LABEL" ] || {
    usage >&2
    die "--erase-cartridge is required. This script erases the loaded tape; name it."
}

for bin in mt dd lsscsi python3 sg_inq; do
    command -v "$bin" >/dev/null || die "required binary missing: $bin"
done
[ -e "$TAPE_DEV" ] || die "$TAPE_DEV does not exist"

# Single-drive rule (#9): the same lock the mhvtl gate takes, so a measurement
# run and a gate run can never both be driving the drive.
exec 9>/tmp/tapectl-tape.lock
flock -n 9 || die "another process holds the tape lock (/tmp/tapectl-tape.lock)"

# ---------- device discovery (#67) ----------
ST_BASE="$(basename "$TAPE_DEV")"; ST_BASE="${ST_BASE#n}"
ROW="$(lsscsi -g | grep -F "/dev/$ST_BASE " | head -1)"
[ -n "$ROW" ] || die "cannot find $TAPE_DEV in lsscsi -g"
DRIVE_SG="$(echo "$ROW" | awk '{print $NF}')"
DRIVE_MODEL="$(echo "$ROW" | awk '{print $3" "$4}')"
[ -e "$DRIVE_SG" ] || die "discovered sg node $DRIVE_SG does not exist"

STAMP="$(date +%Y%m%d-%H%M%S)"
RUN="$OUT_DIR/run-$STAMP"
mkdir -p "$RUN" || die "cannot create $RUN"
REPORT="$RUN/REPORT.md"

# Every raw command output is kept. The checklist's closing instruction is
# "don't paper over it" — a summary without the raw capture cannot be
# re-examined once the tape is unloaded.
#
# Every `raw ... || true` call site in this script (mam-preflight,
# mt-status-$label, mam-before, mam-after) is deliberate: the caller never
# branches on raw()'s own return value, it inspects the OUTPUT FILE
# afterward instead (grepping it for a barcode, a block-size confirmation, a
# MAM attribute). `|| true` there only stops a probe that is allowed to come
# back empty from tripping `set -uo pipefail`; it does not hide a failure
# that matters, because the file it wrote is always there to go check.
raw() { # raw <name> <command...>
    local name="$1"; shift
    echo "\$ $*" > "$RUN/$name.txt"
    "$@" >> "$RUN/$name.txt" 2>&1
    return $?
}

say() { echo "$*" | tee -a "$REPORT"; }
note() { echo "$*" >> "$REPORT"; }

say "# LTO-6 hardware measurements — $STAMP"
say ""
say "- device: \`$TAPE_DEV\` (sg: \`$DRIVE_SG\`)"
say "- drive: $DRIVE_MODEL"
say "- cartridge named for erasure: \`$ERASE_LABEL\`"
say "- payload per throughput run: ${PAYLOAD_MB} MiB"
say ""
note "Raw command output for every step is in \`$RUN\`."
note ""

# ---------- consent: the named cartridge must be the loaded one ----------
BARCODE=""
if command -v sg_read_attr >/dev/null; then
    raw mam-preflight sg_read_attr "$DRIVE_SG" || true
    # MAM attribute 0x0806 is the barcode; 0x0401 is the medium serial.
    #
    # The match is ANCHORED and label-specific on purpose. A loose
    # `grep -i 'barcode|serial'` matches "Density vendor/serial number at last
    # load" FIRST — verified against a real sg_read_attr dump — and would
    # compare the operator's cartridge name against the string "IBM XYZZY_A1",
    # refusing every legitimate run. A consent check that misfires is a
    # consent check that gets bypassed.
    BARCODE="$(grep -iE '^[[:space:]]*(barcode|medium serial number)[[:space:]]*:' \
        "$RUN/mam-preflight.txt" 2>/dev/null | head -1 | sed 's/^[^:]*: *//' | tr -d ' \r')"
fi

if [ -n "$BARCODE" ]; then
    if [ "$BARCODE" != "$ERASE_LABEL" ]; then
        die "loaded cartridge reports barcode '$BARCODE' but you named '$ERASE_LABEL'.
     Refusing to erase a cartridge you did not name."
    fi
    say "Barcode verified against MAM: \`$BARCODE\`."
elif [ "$ALLOW_UNVERIFIED_BARCODE" = "1" ]; then
    say "**MAM reported no barcode**; proceeding on \`--allow-unverified-barcode\`."
else
    die "MAM did not report a barcode, so the named cartridge cannot be verified.
     Re-run with --allow-unverified-barcode if you are certain the right tape is loaded."
fi
say ""

# ---------- F. as-found inventory ----------
say "## F. As-found inventory"
raw inq         sg_inq "$DRIVE_SG"          || note "- \`sg_inq\` failed (see inq.txt)"
raw mt-status   mt -f "$TAPE_DEV" status    || note "- \`mt status\` failed"
command -v sg_logs >/dev/null && {
    raw logs-02 sg_logs --page=0x02 "$DRIVE_SG" || note "- sg_logs page 0x02 unavailable"
    raw logs-0c sg_logs --page=0x0c "$DRIVE_SG" || note "- sg_logs page 0x0c unavailable"
    raw logs-03 sg_logs --page=0x03 "$DRIVE_SG" || note "- sg_logs page 0x03 unavailable"
}
say "Captured: \`sg_inq\`, \`mt status\`, sg_logs pages 0x02/0x03/0x0c."
say ""

# ---------- B. compression as-found ----------
say "## B. Hardware compression (must be OFF for encrypted data)"
if command -v sg_modes >/dev/null; then
    if raw modes-0f sg_modes --page=0x0f "$DRIVE_SG"; then
        # DCE (Data Compression Enable) is bit 7 of the first byte after the
        # page header. Reported rather than parsed to a verdict: mode page
        # rendering differs by vendor, and a wrong verdict here is worse than
        # no verdict — #97's fail-open rule applied to a hardware probe.
        say "Mode page 0x0f captured to \`modes-0f.txt\`. Read the DCE bit there;"
        say "the design requires compression OFF (#28 issues MTCOMPRESSION 0)."
    else
        say "Mode page 0x0f unavailable on this drive/emulator."
    fi
else
    say "\`sg_modes\` not installed — skipped."
fi
say ""

# ---------- A. block size acceptance + throughput ----------
say "## A. Block size: acceptance and throughput"
say ""
# READ BLOCK LIMITS is the authoritative statement of what the DRIVE accepts,
# and it is worth having before the write attempts: without it, a failed write
# is ambiguous between "the drive refuses this size" and "this host's st
# driver could not buffer it". Those have completely different remedies, and
# the second is invisible from the tapectl side.
if command -v sg_read_block_limits >/dev/null; then
    if raw block-limits sg_read_block_limits "$DRIVE_SG"; then
        BL_MAX="$(grep -i 'maximum block size' "$RUN/block-limits.txt" | grep -oE '[0-9]+' | head -1)"
        BL_MIN="$(grep -i 'minimum block size' "$RUN/block-limits.txt" | grep -oE '[0-9]+' | head -1)"
        say "Drive READ BLOCK LIMITS: min ${BL_MIN:-?} B, **max ${BL_MAX:-?} B**."
        say ""
    fi
fi
say "| block size | setblk | throughput |"
say "|---|---|---|"

# Payload lives in tmpfs when there's room, so every throughput run below
# reads from RAM rather than the source disk (issue #117): the original
# harness generated one payload file on $OUT_DIR (a disk) and read it once
# per block size, so the first run read it cold and later runs read it warm
# from page cache — it measured the source disk, not the tape. Falls back to
# $RUN (still on disk) when /dev/shm is missing or too small; the source
# read rate measured below makes a disk-bound fallback self-evident either way.
NEED_MB=$((PAYLOAD_MB + 64))
SHM_FREE_MB=""
if [ -d /dev/shm ]; then
    SHM_FREE_MB="$(df -m /dev/shm 2>/dev/null | awk 'NR==2{print $4}')"
fi
if [ -n "$SHM_FREE_MB" ] && [ "$SHM_FREE_MB" -ge "$NEED_MB" ] 2>/dev/null; then
    PAYLOAD="/dev/shm/tapectl-lto6-payload-$STAMP.bin"
    say "Payload location: \`/dev/shm\` (tmpfs) — needs ${NEED_MB} MiB, ${SHM_FREE_MB} MiB free."
else
    PAYLOAD="$RUN/payload.bin"
    say "Payload location: \`$RUN\` (disk) — /dev/shm is missing or has less than"
    say "${NEED_MB} MiB free (has: ${SHM_FREE_MB:-n/a} MiB). Throughput numbers"
    say "below may be source-disk-bound rather than tape-bound; the source read"
    say "rate line below is how to tell."
fi
say ""

# Incompressible on purpose: tapectl writes age ciphertext, so a compressible
# payload would measure the drive's compressor rather than the medium and
# report a throughput the real write path can never reach.
dd if=/dev/urandom of="$PAYLOAD" bs=1M count="$PAYLOAD_MB" status=none 2>/dev/null \
    || die "could not generate a ${PAYLOAD_MB} MiB payload at $PAYLOAD"

# Source read rate, reported so a source-bound measurement below is
# self-evident rather than mistaken for a tape/block-size effect (issue #117).
SRC_T0=$(date +%s.%N)
if dd if="$PAYLOAD" of=/dev/null bs=1M status=none 2>"$RUN/dd-source-read.txt"; then
    SRC_T1=$(date +%s.%N)
    SRC_SECS=$(python3 -c "print(max(1e-6, $SRC_T1 - $SRC_T0))")
    SRC_RATE=$(python3 -c "print(f'{$PAYLOAD_MB / $SRC_SECS:.1f}')")
    say "Source read rate: ${SRC_RATE} MiB/s (\`$PAYLOAD\` -> /dev/null, ${PAYLOAD_MB} MiB)."
else
    say "Source read rate: NOT MEASURED — the timing read failed (see \`dd-source-read.txt\`)."
fi
say ""

measure_blocksize() { # measure_blocksize <bytes> <label>
    local bs="$1" label="$2" t0 t1 secs rate
    mt -f "$TAPE_DEV" rewind >/dev/null 2>&1
    if ! mt -f "$TAPE_DEV" setblk "$bs" >"$RUN/setblk-$label.txt" 2>&1; then
        say "| $label | REJECTED | — |"
        return
    fi
    if ! mt -f "$TAPE_DEV" status 2>/dev/null | grep -q "block size $bs\|blocksize $bs\|Tape block size $bs"; then
        note ""
        note "> \`setblk $bs\` returned success but \`mt status\` does not report it."
        note "> Recorded as accepted-unconfirmed; check \`mt-status-$label.txt\`."
        raw "mt-status-$label" mt -f "$TAPE_DEV" status || true
    fi
    mt -f "$TAPE_DEV" rewind >/dev/null 2>&1
    t0=$(date +%s.%N)
    if ! dd if="$PAYLOAD" of="$TAPE_DEV" bs="$bs" status=none 2>"$RUN/dd-$label.txt"; then
        # Distinguish the two very different reasons a write at this size can
        # fail. If the drive's own READ BLOCK LIMITS says the size is within
        # range, the refusal came from THIS HOST's st driver (EBUSY on write
        # is the usual shape) — a host-side buffer limit, not a property of
        # the drive or the medium, and invisible from tapectl.
        local why="WRITE FAILED (see dd-$label.txt)"
        if grep -qi 'busy' "$RUN/dd-$label.txt" 2>/dev/null; then
            if [ -n "${BL_MAX:-}" ] && [ "$bs" -le "${BL_MAX:-0}" ] 2>/dev/null; then
                why="host \`st\` driver refused (EBUSY) — the DRIVE advertises ${BL_MAX} B, so this is a host limit, not a drive limit"
            else
                why="EBUSY — and the drive's max block size is ${BL_MAX:-unknown} B"
            fi
        fi
        say "| $label | setblk ok | $why |"
        return
    fi
    t1=$(date +%s.%N)
    secs=$(python3 -c "print(max(1e-6, $t1 - $t0))")
    rate=$(python3 -c "print(f'{$PAYLOAD_MB / $secs:.1f}')")
    say "| $label | accepted | ${rate} MiB/s (${PAYLOAD_MB} MiB in $(printf '%.1f' "$secs")s) |"
    echo "$label $rate" >> "$RUN/throughput.txt"
}

# Two passes in alternating order (issue #117): a real block-size effect
# looks the same regardless of order; a page-cache/warm-up artifact shows up
# as a trend across the passes instead of disorder. This is what exposed the
# original "1 M is 3x faster" artifact — it was the source disk warming up,
# not the tape.
measure_blocksize 524288 "512K (pass 1)"
measure_blocksize 1048576 "1M (pass 1)"
measure_blocksize 1048576 "1M (pass 2)"
measure_blocksize 524288 "512K (pass 2)"
say ""
say "The v2 write path uses fixed 512 K blocks. A 1 M advantage large enough to"
say "matter, consistent across both pass orders, is the input to reopening that"
say "choice; a wash — or a number that only shows up in one order — means leave"
say "it alone."
say ""

# ---------- D. MAM over-report bound ----------
say "## D. MAM remaining-capacity over-report"
if command -v sg_read_attr >/dev/null; then
    # weof at BOT truncates EOD back to BOT, which is what makes mam-before a
    # clean baseline (docs/lto6-session-journal-2026-09-10.md, Phase 6). Do
    # not simplify this sequence away.
    mt -f "$TAPE_DEV" rewind >/dev/null 2>&1
    mt -f "$TAPE_DEV" weof 1 >/dev/null 2>&1
    raw mam-before sg_read_attr "$DRIVE_SG" || true
    # Section A's last measure_blocksize call may have left the st driver in
    # 1 M fixed-block mode. This probe writes fixed 512 K blocks and must not
    # inherit that state independently of section A (issue #116): writing
    # 512 K blocks to a driver set for 1 M is EINVAL, and the old code threw
    # both the dd exit status and stderr away, sampling mam-after after a
    # zero-byte write and reporting a confident, wrong "no change". Kept
    # independent of measure_blocksize's own setblk on purpose — a future
    # reordering of the sections must not silently reintroduce this.
    if ! mt -f "$TAPE_DEV" setblk 524288 >"$RUN/setblk-mam.txt" 2>&1; then
        say "**NOT MEASURED — could not set block size** (see \`setblk-mam.txt\`)."
    elif dd if="$PAYLOAD" of="$TAPE_DEV" bs=524288 status=none 2>"$RUN/dd-mam.txt"; then
        mt -f "$TAPE_DEV" weof 1 >/dev/null 2>&1
        raw mam-after sg_read_attr "$DRIVE_SG" || true
        say "Captured MAM before and after writing ${PAYLOAD_MB} MiB"
        say "(\`mam-before.txt\` / \`mam-after.txt\`)."
        say ""
        BEFORE_LINE="$(grep -F 'Remaining capacity in partition' "$RUN/mam-before.txt" 2>/dev/null | head -1)"
        AFTER_LINE="$(grep -F 'Remaining capacity in partition' "$RUN/mam-after.txt" 2>/dev/null | head -1)"
        say "- before: \`${BEFORE_LINE:-not found in mam-before.txt}\`"
        say "- after:  \`${AFTER_LINE:-not found in mam-after.txt}\`"
        say ""
        say "**To size the ENOSPC buffer:** diff the remaining-capacity attribute"
        say "across those two files. Remaining should fall by ~${PAYLOAD_MB} MiB. The"
        say "shortfall — how much MORE the drive claims it still has than it really"
        say "does — is the over-report, and the buffer must exceed it. This is"
        say "reported rather than computed because the attribute's name and units"
        say "differ by vendor, and a mis-parsed capacity would silently produce a"
        say "buffer that is too small."
    else
        say "**NOT MEASURED — the capacity write FAILED (see dd-mam.txt); the"
        say "before/after MAM pair below is meaningless.**"
    fi
else
    say "\`sg_read_attr\` not installed — skipped."
fi
say ""

# ---------- E. EOD semantics ----------
say "## E. EOD semantics (§3.2's untested physics assumption)"
# The v2 chain walk assumes a forward read past end-of-data ERRORS. If a drive
# instead returns stale data from a previous write, a truncated session could
# read as complete — which is the one failure mode the front index and seal
# marker cannot detect, because both would look internally consistent.
mt -f "$TAPE_DEV" rewind >/dev/null 2>&1
if ! mt -f "$TAPE_DEV" setblk 524288 >"$RUN/setblk-eod.txt" 2>&1; then
    say "**NOT MEASURED — could not set block size for the EOD probe**"
    say "(see \`setblk-eod.txt\`); the writes below are expected to fail too."
fi
# These three writes are load-bearing: the EOD probe below is meaningless if
# fewer than three real files ended up on the tape, so a failure here is
# reported loudly instead of silently producing a probe result that looks
# like a real PASS or FAIL but isn't (issue #116's bug class).
for i in 1 2 3; do
    if ! head -c 524288 /dev/urandom | dd of="$TAPE_DEV" bs=524288 status=none 2>"$RUN/dd-eod-$i.txt"; then
        say "**NOT MEASURED reliably — EOD probe write $i FAILED**"
        say "(see \`dd-eod-$i.txt\`); the EOD read below may not reflect three"
        say "real files on the tape."
    fi
    mt -f "$TAPE_DEV" weof 1 >/dev/null 2>&1
done
mt -f "$TAPE_DEV" rewind >/dev/null 2>&1
# Skip past all three files, then past EOD. Left silent deliberately: making
# this positioning step report its own failure would change what a FAIL below
# means (a mispositioned fsf could make the probe read real file 1 data and
# misreport it as "stale data past EOD"), and that is EOD-probe interpretation
# logic, not a write-failure report — out of scope for this change (issues
# #116/#117 are about writes and the block-size table, not this probe's
# positioning). Flagged as a residual, not fixed here.
mt -f "$TAPE_DEV" fsf 3 >/dev/null 2>&1
EOD_OUT="$RUN/eod-read.txt"
if dd if="$TAPE_DEV" of=/dev/null bs=524288 count=1 >"$EOD_OUT" 2>&1; then
    READ_BYTES="$(grep -oE '[0-9]+ bytes' "$EOD_OUT" | head -1 | awk '{print $1}')"
    if [ "${READ_BYTES:-0}" -gt 0 ] 2>/dev/null; then
        say "**FAIL — this is the dangerous outcome.** A read past EOD returned"
        say "${READ_BYTES} bytes instead of erroring. §3.2 assumes it errors; a"
        say "drive that returns stale data can make a truncated session read as"
        say "complete. File this against the chain-walk design before writing"
        say "production data on this drive."
    else
        say "PASS — read past EOD returned no data (see \`eod-read.txt\`)."
    fi
else
    say "PASS — read past EOD errored, as §3.2 assumes (see \`eod-read.txt\`)."
fi
say ""

# ---------- C. LBP ----------
say "## C. Logical block protection (LBP)"
if command -v sg_modes >/dev/null; then
    if raw modes-0a-f0 sg_modes --page=0x0a,0xf0 "$DRIVE_SG"; then
        say "Control Data Protection mode page captured to \`modes-0a-f0.txt\`."
        say "Read LBP_METHOD there: 0 = LBP unsupported/off, non-zero = a"
        say "protection method the drive will accept."
    else
        say "The Control Data Protection page (0x0a subpage 0xf0) is not"
        say "readable on this drive — treat LBP as unavailable."
    fi
    say ""
    say "MODE SELECT is deliberately NOT attempted here. Enabling LBP changes"
    say "the block format the drive expects on every subsequent command, and a"
    say "half-applied change on the cartridge you are about to write real data"
    say "to is a worse outcome than not knowing. Decide from the page above,"
    say "then enable it as a considered change with its own ticket."
else
    say "\`sg_modes\` not installed — skipped."
fi
say ""

# ---------- close ----------
# Leave the drive at the block size tapectl actually uses. This run may have
# set 1 M partway through, and block size is DRIVER state that outlives the
# process — leaving it there would make the next mhvtl gate run fail in a way
# that looks like a code regression. Restored explicitly rather than relying
# on the EOD probe having happened to set it back.
mt -f "$TAPE_DEV" rewind >/dev/null 2>&1
mt -f "$TAPE_DEV" setblk 524288 >/dev/null 2>&1 \
    || note "> WARNING: could not restore the 512 K block size; run \`mt -f $TAPE_DEV setblk 524288\` before the next gate run."
rm -f "$PAYLOAD"

say "## Still manual — and still required"
say ""
say "- **The ENOSPC drill.** mhvtl gives a false pass (it accepts writes past"
say "  capacity and silently corrupts them), so this is real-hardware-only and"
say "  it is the single most important check. Procedure:"
say "  \`docs/lto6-validation-checklist.md\`."
say "- **The raw-recovery drill** — \`RESTORE.sh\` off the tape with no tapectl."
say "- Recording real throughput in \`docs/perf-baselines.md\` and marking the"
say "  M7 checklist item done."
say ""
say "Recording directory: \`$RUN\`"

echo
echo "measurements complete — report: $REPORT"
