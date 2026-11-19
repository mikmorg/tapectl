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
#   4. Unmount and count both tokens, and a plaintext tar's own marker (the
#      "ustar" magic of a header, at offset 257 of a 512-byte block), in the
#      raw image file. Layout 3 stages a tar stream, so a leaked stream shows
#      the planted names and contents verbatim and the magic in every header.
#   5. Positive control for step 4: the same count over a plaintext tar of the
#      planted tree, made here by GNU tar. It must find the token and the
#      magic, or the counting is broken and the run fails instead of passing.
#
# PASS: the canary is found and neither the product token nor the tar magic is. Exit 0 pass,
# 1 fail (or a canary the scan could not see), 2 setup error.
#
# Needs: sudo (mount/umount), mkfs.ext4, tar (the GNU tar tapectl stages with), setfattr (optional: the xattr
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
    echo "xattr leg: on"; XATTR_LEG=on
else
    echo "xattr leg: skipped (no setfattr, or the source filesystem refuses user xattrs)"; XATTR_LEG=skipped
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
t = re.sub(r'(?m)^binary *=.*$', 'binary = "tar"', t, count=1)  # [tar] binary
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
# Positive control (#471): the stage's slices must have landed on the image
# being scanned. A staging directory that pointed elsewhere would leave the
# image empty of the unit and the scan below trivially clean.
nslices=$(sudo find "$MNT" -name '*.age' -type f | wc -l)
if [ "$nslices" -eq 0 ]; then
    echo "FAIL: no .age slice landed on the scanned staging device; the scan would prove nothing"
    exit 1
fi
echo "positive control: $nslices encrypted slice(s) on the scanned device"

# 4. Scan the raw image.
sync
sudo umount "$MNT"
mounted=0
# count FILE NEEDLE: occurrences of the fixed string NEEDLE in FILE.
count() { { grep -a -o -F "$2" "$1" || true; } | wc -l; }
# The tar header magic: "ustar" followed by NUL and "00" (POSIX/pax) or by two
# spaces and NUL (GNU), 257 bytes into a header. Counted as the 5 letters plus
# the byte after them being NUL or a space, which the ciphertext and ext4
# metadata reach only by chance (about 2^-40 a position).
magic_count() {
    python3 -I - "$1" <<'PY'
import mmap, re, sys
with open(sys.argv[1], "rb") as f:
    with mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ) as m:
        print(sum(1 for _ in re.finditer(rb"ustar[\x00 ]", m)))
PY
}

# 5 (control first, so a broken count fails before it can bless a clean image).
# A plaintext tar of the planted tree: what a leaked stream would look like.
CONTROL=$WORK/control.tar
tar --format=pax -cf "$CONTROL" -C "$SRC_PARENT" "$UNIT"
control_token=$(count "$CONTROL" "$TOKEN")
control_magic=$(magic_count "$CONTROL")
echo "control (a plaintext tar of the planted tree): product token $control_token hit(s), tar magic $control_magic"
if [ "$control_token" -eq 0 ] || [ "$control_magic" -eq 0 ]; then
    echo "FAIL: the scan does not see a plaintext tar of the planted tree (token $control_token, magic $control_magic); a clean image would prove nothing"
    exit 1
fi

canary_hits=$(count "$IMG" "$CANARY")
token_hits=$(count "$IMG" "$TOKEN")
magic_hits=$(magic_count "$IMG")
echo "raw image: canary $canary_hits hit(s), product token $token_hits hit(s), tar magic $magic_hits hit(s)"

if [ "$canary_hits" -eq 0 ]; then
    echo "FAIL: the deleted canary is not in the image, so this scan cannot see a deleted file; it proves nothing"
    exit 1
fi
if [ "$token_hits" -ne 0 ] || [ "$magic_hits" -ne 0 ]; then
    echo "FAIL: plaintext from the staged unit is on the staging device (token $token_hits hit(s), tar magic $magic_hits hit(s))"
    exit 1
fi
if [ "$XATTR_LEG" = skipped ]; then
    echo "PASS (xattr leg SKIPPED: user xattrs were not exercised): no plaintext on the staging device; the canary shows deleted blocks are visible"
else
    echo "PASS: no plaintext on the staging device; the canary shows deleted blocks are visible"
fi
exit 0
