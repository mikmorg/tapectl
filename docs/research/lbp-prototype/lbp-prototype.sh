#!/usr/bin/env bash
# #388's real-drive prototype (docs/research/2026-10-16-logical-block-protection.md, section 8):
# Logical Block Protection, Reed-Solomon CRC (method 01h), on a test cartridge it ERASES.
#
#   lbp-prototype.sh --device /dev/tape/by-id/scsi-<serial>-nst --i-will-lose-the-cartridge SERIAL
#                    [--out DIR] [--block N]   (N: data bytes per block, default 524288)
#
# Steps: 1 read the page (current, changeable, default, saved) and READ BLOCK LIMITS;
# 2 probe method 02h (CRC32C); 3 arm method 01h with LBP_W and LBP_R, block size N+4;
# 4 write 8 protected blocks and a 9th whose CRC has one bit flipped (expect the drive to refuse it);
# 5 the heir's view: LBP off, 512 KiB blocks, plain dd, byte-compare; variable mode: record length;
# 6 LBP_R read of the 8 blocks, each CRC checked on the host; then (N+4)-byte reads with LBP off;
# 7 the page after the device is closed and opened again;
# 8 always (trap): method 0, LBP_W and LBP_R 0, block size 524288, rewind, page read back.
# The page is never hardcoded: the drive's own page is read and only bytes 4-6 change (an HP LTO-6
# reports length 04h and 4Ah, mhvtl 1Ch and 0Ah).
set -uo pipefail
HERE="$(dirname "$(readlink -f "$0")")"; LBP="python3 -I $HERE/lbp.py"
DEV=""; SERIAL=""; OUT=~/.cache/tapectl/lbp; BLK=524288
while [ $# -gt 0 ]; do case "$1" in
  --device) DEV="$2"; shift 2 ;; --i-will-lose-the-cartridge) SERIAL="$2"; shift 2 ;;
  --out) OUT="$2"; shift 2 ;; --block) BLK="$2"; shift 2 ;; *) echo "unknown argument $1"; exit 2 ;; esac; done
[ -n "$DEV" ] && [ -n "$SERIAL" ] || { sed -n 2,8p "$0"; exit 2; }
NST=$(readlink -f "$DEV"); SG=/dev/$(ls "/sys/class/scsi_tape/${NST##*/}/device/scsi_generic/")
PBLK=$((BLK + 4)); R="$OUT/lbp-$(date -u +%Y%m%dT%H%M%SZ)-b$BLK"; mkdir -p "$R"; cd "$R" || exit 2
exec > >(tee "$R/lbp.log") 2>&1
say() { echo; echo "## $*  ($(date -u +%T)Z)"; }
run() { echo "\$ $*"; "$@"; local rc=$?; echo "  -> exit $rc"; return $rc; }

# ---------------------------------------------------------------- preflight (nothing written)
say "preflight: $DEV -> $NST, $SG; run directory $R"
if pgrep -x tapectl >/dev/null; then echo "ABORT: a tapectl process is running"; exit 2; fi
if systemctl cat tapectl-drive-poll.timer >/dev/null 2>&1 && \
   [ "$(systemctl is-active tapectl-drive-poll.timer)" != inactive ]; then
  echo "ABORT: tapectl-drive-poll.timer is active"; exit 2; fi
exec 9>/tmp/tapectl-tape.lock; flock -n 9 || { echo "ABORT: the harness tape lock is held"; exit 2; }
sg_read_attr "$SG" >/dev/null 2>&1   # a pending unit attention
GOT=$(sg_read_attr -f 0x401 "$SG" 2>/dev/null | awk -F': *' '/Medium serial number/ {print $2}' | tr -d '[:space:]')
echo "medium serial: ${GOT:-<none>}"
[ "$GOT" = "$SERIAL" ] || { echo "ABORT: the loaded cartridge is not $SERIAL"; exit 2; }
$LBP selftest || exit 2
sg_inq "$SG" | grep -E "Vendor|Product|Revision" | sed 's/^ */  /'

# page helpers: the current page as hex bytes, and a write of it with bytes 4-6 replaced
page_hex() {  # $1 = page control (0 current, 1 changeable, 2 default, 3 saved)
  sg_modes --page=0x0a,0xf0 --control="${1:-0}" --raw "$SG" 2>/dev/null | od -An -v -tx1 | tr -s ' \n' ' '; }
page_only() { # strip the MODE SENSE(10) header and block descriptors
  local -a b=($1); local bdl=$(( 16#${b[6]}${b[7]} )); local off=$((8 + bdl))
  local len=$(( 16#${b[off+2]}${b[off+3]} )); echo "${b[@]:off:len+4}"; }
show_page() { local p; p=$(page_only "$(page_hex 0)"); echo "  page now: $p"
  local -a b=($p); echo "  method=${b[4]} info_len=${b[5]} flags=${b[6]} (LBP_W 40, LBP_R 80, RBDP 20)"; }
set_lbp() {   # $1 method, $2 info length, $3 flags byte
  local -a b=($(page_only "$(page_hex 0)")); b[0]=$(printf '%02x' $(( 16#${b[0]} & 0x7f )))
  b[4]=$1; b[5]=$2; b[6]=$3; local c; c=$(IFS=,; echo "${b[*]}")
  run sg_wr_mode --page=0x0a,0xf0 --dbd --contents="$c" "$SG"; local rc=$?; show_page; return $rc; }
rbl() { echo "  READ BLOCK LIMITS: $(sg_raw -r 6 "$SG" 05 00 00 00 00 00 2>/dev/null | grep -E '^ *00 ' | head -1)"; }

end_state() {
  say "8. end state: LBP off, block size 524288, rewind"
  set_lbp 00 00 00 >/dev/null 2>&1; show_page
  mt -f "$NST" setblk 524288; mt -f "$NST" rewind
  echo "lbp-prototype: run directory $R"; }
trap end_state EXIT

# ---------------------------------------------------------------- 1
say "1. the page: current, changeable, default, saved"
for pc in 0 1 2 3; do echo "  control $pc: $(page_only "$(page_hex $pc)" 2>/dev/null || echo '<none>')"; done
sg_vpd --page=ei "$SG" 2>&1 | grep -iE "SPT|protection" | sed 's/^ */  /'
rbl

# ---------------------------------------------------------------- 2
say "2. probe method 02h (CRC32C), LBP_W and LBP_R"
set_lbp 02 04 c0; echo "  (a refusal is the expected answer on an LTO-6)"
set_lbp 00 00 00 >/dev/null

# ---------------------------------------------------------------- 3
say "3. arm method 01h (Reed-Solomon), LBP_W and LBP_R; block size $PBLK"
set_lbp 01 04 c0 || { echo "FINDING: method 01h refused"; exit 1; }
rbl
run mt -f "$NST" setblk $PBLK

# ---------------------------------------------------------------- 4
say "4. write 8 protected blocks and a 9th with a bad CRC, in one dd"
head -c $((9*BLK)) /dev/urandom > payload9.bin
head -c $((8*BLK)) payload9.bin > payload8.bin
$LBP protect payload9.bin nine.lbp --block $BLK --corrupt-last
run mt -f "$NST" rewind
dd if=nine.lbp of="$NST" bs=$PBLK count=9 2> dd-write.err; echo "  dd exit $?: $(tr '\n' ' ' < dd-write.err)"
mt -f "$NST" status 2>&1 | head -3 | sed 's/^/  /'
dmesg 2>/dev/null | tail -6 | sed 's/^/  dmesg: /' || true

# ---------------------------------------------------------------- 5
say "5. the heir's view: LBP off, $BLK-byte blocks, plain dd"
set_lbp 00 00 00
run mt -f "$NST" setblk $BLK; run mt -f "$NST" rewind
dd if="$NST" of=back8.bin bs=$BLK count=8 2> dd-read.err; echo "  dd exit $?: $(tr '\n' ' ' < dd-read.err)"
if cmp back8.bin payload8.bin; then echo "  HEIR VIEW: the 8 blocks read back byte-identical, with no CRC"; else echo "  FINDING: the plain read differs"; fi
run mt -f "$NST" setblk 0; run mt -f "$NST" rewind
dd if="$NST" of=var1.bin bs=$((2*BLK)) count=1 2>var1.err; echo "  variable mode ($(tr '\n' ' ' < var1.err)): one record read as $(stat -c %s var1.bin) bytes ($BLK = no CRC on the medium's logical block)"
run mt -f "$NST" setblk 524288

# ---------------------------------------------------------------- 6
say "6. LBP_R: read the 8 blocks with their CRC and check each on the host"
set_lbp 01 04 c0
run mt -f "$NST" setblk $PBLK; run mt -f "$NST" rewind
dd if="$NST" of=back8.lbp bs=$PBLK count=8 2> dd-lbpr.err; echo "  dd exit $?: $(tr '\n' ' ' < dd-lbpr.err)"
$LBP verify back8.lbp --block $BLK && echo "  LBP_R: every CRC the drive returned matches"
say "6b. LBP off, block size still $PBLK: how st reports the length mismatch"
set_lbp 00 00 00 >/dev/null
run mt -f "$NST" rewind
dd if="$NST" of=/dev/null bs=$PBLK count=1 2>&1 | sed 's/^/  /'

# ---------------------------------------------------------------- 7
say "7. persistence: arm, close and reopen the device, read the page"
set_lbp 01 04 c0 >/dev/null
mt -f "$NST" status >/dev/null 2>&1; mt -f "$NST" rewind
show_page; echo "  (a standalone drive cannot be offlined and reloaded unattended; not tested)"
exit 0
