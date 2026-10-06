#!/usr/bin/env bash
# plaintext-scan.sh — does `stage create` leave plaintext on the staging device?
#
# The raw-image layer of the proof that staging writes no plaintext to the
# staging device (ADR-0012, 2026-10-06 amendment item 4; issue #370;
# docs/research/2026-10-06-plaintext-free-staging.md §8, layer 3).
#
#   1. Build an ext4 image mounted `loop,sync,nodiscard` as the staging
#      directory. `sync` forces every write to the image as it happens and
#      `nodiscard` keeps freed blocks' bytes, so a plaintext file that lived
#      for a moment and was deleted is still in the image. (On an async
#      mount a file deleted before writeback never reaches the disk, and a
#      clean scan would prove nothing.)
#   2. Positive control: write a canary carrying a per-run token onto the
#      image, sync, delete it, sync. The scan must find it, or this
#      filesystem configuration cannot show a deleted file and the run says
#      so instead of passing.
#   3. Stage a unit whose content, file and directory names, symlink target,
#      xattr value, unit name and tenant name all carry a second per-run
#      token, with `compression = "none"` so any leak is verbatim. Small,
#      multi-slice, sparse and hard-linked files.
#   4. Unmount and count both tokens in the raw image file.
#
# PASS: the canary is found and the product token is not. Exit 0 pass,
# 1 fail (or a canary the scan could not see), 2 setup error.
#
# Needs: sudo (mount/umount), mkfs.ext4, dar, setfattr (optional: the xattr
# leg is skipped without it). Touches no tape device. The tapectl home is a
# normal directory outside the image: this checks the staging device only.
#
# Usage: scripts/plaintext-scan.sh TAPECTL_BINARY [WORK_DIR]
#   WORK_DIR defaults to a fresh directory under /scratch (or $TMPDIR),
#   removed at the end unless KEEP=1. A WORK_DIR you name is never removed.
#
# One-time control against the old code: run it with a tapectl from before
# the change (1.0.7 or earlier). Its stage writes the plaintext archive to
# staging, so the product token is found (the research counted 215 hits).

set -euo pipefail

BIN=${1:?usage: plaintext-scan.sh TAPECTL_BINARY [WORK_DIR]}
BIN=$(readlink -f "$BIN")
[ -x "$BIN" ] || { echo "SETUP: $BIN is not an executable" >&2; exit 2; }
base=/scratch
[ -d "$base" ] && [ -w "$base" ] || base=${TMPDIR:-/tmp}
if [ -n "${2:-}" ]; then
    WORK=$2
    owned=0
else
    WORK=$(mktemp -d "$base/plaintext-scan.XXXXXX")
    owned=1
fi
mkdir -p "$WORK"
IMG=$WORK/staging.img
MNT=$WORK/staging
HOME_DIR=$WORK/home
SRC_PARENT=$WORK/src
mkdir -p "$MNT" "$HOME_DIR" "$SRC_PARENT"

mounted=0
# shellcheck disable=SC2317  # reached through the EXIT trap
cleanup() {
    if [ "$mounted" = 1 ]; then
        sudo umount "$MNT" 2>/dev/null || true
    fi
    if [ "$owned" = 1 ] && [ "${KEEP:-0}" != 1 ]; then
        rm -rf "$WORK"
    else
        echo "kept: $WORK"
    fi
}
trap cleanup EXIT

# Per-run tokens, so nothing but this run can match.
rand() { od -An -N12 -tx1 /dev/urandom | tr -d ' \n'; }
CANARY="canary$(rand)"
TOKEN="leak$(rand)"
echo "canary token:  $CANARY"
echo "product token: $TOKEN"

# 1. The staging filesystem.
truncate -s 512M "$IMG"
mkfs.ext4 -q -F -E nodiscard "$IMG"
sudo mount -o loop,sync,nodiscard "$IMG" "$MNT"
mounted=1
sudo chown "$(id -u):$(id -g)" "$MNT"
chmod 0700 "$MNT"

# 2. Positive control: a canary written, synced, deleted.
for _ in $(seq 1 64); do printf '%s\n' "$CANARY"; done > "$MNT/canary.txt"
sync
rm "$MNT/canary.txt"
sync

# 3. A marker tree. Every place a name or a byte of content can hide.
UNIT="unit-$TOKEN"
TENANT="tenant-$TOKEN"
SRC=$SRC_PARENT/$UNIT
mkdir -p "$SRC/dir-$TOKEN/deeper"
# Content in a small file, many times over.
for _ in $(seq 1 200); do printf 'small file holding %s\n' "$TOKEN"; done > "$SRC/small-$TOKEN.txt"
# A multi-slice file: random bytes with the token planted every 64 KiB.
python3 - "$SRC/dir-$TOKEN/big.bin" "$TOKEN" <<'PY'
import os, sys
path, token = sys.argv[1], sys.argv[2].encode()
with open(path, "wb") as f:
    for _ in range(12 * 16):  # 12 MiB in 64 KiB blocks
        block = bytearray(os.urandom(65536))
        block[100:100 + len(token)] = token
        f.write(block)
PY
# A sparse file with the token past a hole.
truncate -s 8M "$SRC/sparse-$TOKEN.img"
printf 'after the hole %s\n' "$TOKEN" | dd of="$SRC/sparse-$TOKEN.img" bs=1 seek=$((6 * 1024 * 1024)) conv=notrunc status=none
# A hard link and a symlink whose target carries the token.
ln "$SRC/small-$TOKEN.txt" "$SRC/dir-$TOKEN/hardlink-$TOKEN.txt"
ln -s "target-$TOKEN" "$SRC/dir-$TOKEN/deeper/link"
# Many small files, each named and filled with the token.
for i in $(seq 1 100); do printf '%s %d\n' "$TOKEN" "$i" > "$SRC/dir-$TOKEN/deeper/f$i-$TOKEN"; done
if command -v setfattr >/dev/null 2>&1 \
    && setfattr -n user.scan -v "xattr-$TOKEN" "$SRC/small-$TOKEN.txt" 2>/dev/null; then
    echo "xattr leg: on"
else
    echo "xattr leg: skipped (no setfattr, or the source filesystem refuses user xattrs)"
fi

# The tapectl home (not on the image) and its config.
T() { "$BIN" --home "$HOME_DIR" --config "$HOME_DIR/config.toml" "$@" </dev/null; }
if ! T init --operator "op-$TOKEN" >"$WORK/init.log" 2>&1; then
    echo "SETUP: tapectl init failed:" >&2; cat "$WORK/init.log" >&2; exit 2
fi
python3 - "$HOME_DIR/config.toml" "$MNT" <<'PY'
import re, sys
cfg, staging = sys.argv[1:3]
t = open(cfg).read()
t = re.sub(r'(?m)^binary *=.*$', 'binary = "dar"', t, count=1)
t = re.sub(r'(?m)^slice_size *=.*$', 'slice_size = "4M"', t, count=1)
t = re.sub(r'(?m)^compression *=.*$', 'compression = "none"', t, count=1)
t = re.sub(r'(?m)^directory *=.*$', f'directory = "{staging}"', t, count=1)
open(cfg, "w").write(t)
PY
for step in "tenant add $TENANT" "unit init $SRC --tenant $TENANT --name $UNIT" "snapshot create $UNIT"; do
    # shellcheck disable=SC2086
    if ! T $step >"$WORK/step.log" 2>&1; then
        echo "SETUP: tapectl $step failed:" >&2; cat "$WORK/step.log" >&2; exit 2
    fi
done
if ! T --yes stage create "$UNIT" >"$WORK/stage.log" 2>&1; then
    echo "SETUP: tapectl stage create failed:" >&2; cat "$WORK/stage.log" >&2; exit 2
fi
echo "staged; staging now holds:"
ls -la "$MNT"

# 4. Scan the raw image.
sync
sudo umount "$MNT"
mounted=0
count() { { grep -a -o -F "$1" "$IMG" || true; } | wc -l; }
canary_hits=$(count "$CANARY")
token_hits=$(count "$TOKEN")
echo "raw image: canary $canary_hits hit(s), product token $token_hits hit(s)"

if [ "$canary_hits" -eq 0 ]; then
    echo "FAIL: the deleted canary is not in the image, so this scan cannot see a deleted file; it proves nothing"
    exit 1
fi
if [ "$token_hits" -ne 0 ]; then
    echo "FAIL: plaintext from the staged unit is on the staging device ($token_hits hit(s))"
    exit 1
fi
echo "PASS: no plaintext on the staging device; the canary shows deleted blocks are visible"
exit 0
