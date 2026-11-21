#!/usr/bin/env bash
# record-recorded-tape.sh -- record the tape under tests/fixtures/recorded-2.0.0/ (#552, plan T6.4).
#
# WHAT IT MAKES. A small tape written by the tapectl binary it is given, through the real
# staging path (GNU tar, the cut at member starts, each slice compressed alone when asked and
# age-encrypted, then `volume write`), on an mhvtl drive, and dumps every tape file the way the
# heir reads it (File 1's recipe: `mt setblk 524288`, `dd bs=512k`, one call per tape file) into
# the fixture directory as file-000, file-001, ... It also writes the test keys, EXPECTED.toml
# (what the tape holds, made from the source trees alone, never from tapectl's own catalog) and
# RECORDED.md (what made the tape).
#
# THE TAPE. Two tenants and the operator key (and the escrow recipient every write needs):
#   rec-alpha:  alpha-main  (no compression; several slices at slice_size 64K; the unit's root has
#                            mode 0750; a nested folder, an empty file and an empty folder, a name
#                            with a non-ASCII letter, a hard link, a symbolic link)
#   rec-bravo:  bravo-zstd, bravo-gzip (one small slice each; each compressed alone; xz is left
#               out: each unit costs a tape file of 512 KiB in the fixture)
# A short-sealed second tape is NOT made. mhvtl gives no real early warning: scripts/
# short-seal-acceptance.sh --mhvtl reaches one with the TAPECTL_TEST_EARLY_WARNING_AT hook, after
# filling the cartridge with examples/short_seal_filler (a binary built beside tapectl) up to a
# warning set inside the filler. That is machinery and drive time this fixture does not carry,
# and its tape would be as large as the fill. T6.4 says to skip it then.
#
# THE RULE (also in the fixture's README.md): the fixture is never re-recorded to make a test
# pass. A later build that cannot read it has broken the format, which is the CTO's decision, as
# a byte pin's change is. Re-running this script makes a NEW tape: it is for the day the CTO
# rules a new recorded tape, not for a red test.
#
# SAFETY. The only device it opens is an mhvtl drive (a by-id name scsi-XYZZY_*-nst). It
# OVERWRITES the cartridge in that drive (erases it first, as the gate does). Every tapectl call
# is made with --home pointing at a scratch directory made here; never ~/.tapectl.
#
# usage: scripts/record-recorded-tape.sh --device /dev/tape/by-id/scsi-XYZZY_A2-nst \
#            --bin <tapectl> --work <empty or absent scratch dir> [--out <fixture dir>]
set -euo pipefail

SCRIPT_DIR="$(dirname "$(readlink -f "$0")")"
REPO="$(dirname "$SCRIPT_DIR")"

die() { echo "record-recorded-tape: $*" >&2; exit 2; }

ARGS="$*"
DEVICE=""; BIN=""; WORK=""; OUT="$REPO/tests/fixtures/recorded-2.0.0"
while [ $# -gt 0 ]; do
    case "$1" in
        --device) DEVICE="${2:-}"; shift 2 ;;
        --bin) BIN="${2:-}"; shift 2 ;;
        --work) WORK="${2:-}"; shift 2 ;;
        --out) OUT="${2:-}"; shift 2 ;;
        -h|--help) sed -n '2,36p' "$0"; exit 0 ;;
        *) die "unknown argument: $1" ;;
    esac
done
[ -n "$DEVICE" ] || die "--device is required (no default)"
[ -n "$BIN" ] || die "--bin is required"
[ -n "$WORK" ] || die "--work is required"
[ -x "$BIN" ] || die "--bin $BIN is not executable"

# Only an mhvtl drive, named by its by-id link: the real drive's serial is HUJ808A5L4.
case "$DEVICE" in
    /dev/tape/by-id/scsi-XYZZY_*-nst) ;;
    *) die "refused: $DEVICE is not an mhvtl by-id drive (/dev/tape/by-id/scsi-XYZZY_*-nst)" ;;
esac
TAPE_DEV="$(readlink -f "$DEVICE")"
case "$TAPE_DEV" in /dev/nst*) ;; *) die "refused: $DEVICE does not resolve to a /dev/nstN node" ;; esac
case "$WORK" in "$HOME/.tapectl"*|/etc/tapectl*) die "refused: --work is a production home" ;; esac
if [ -e "$WORK" ] && [ -n "$(ls -A "$WORK" 2>/dev/null)" ]; then die "--work $WORK is not empty"; fi
mkdir -p "$WORK"
WORK="$(readlink -f "$WORK")"
[ -e "$OUT" ] && [ -n "$(ls -A "$OUT" 2>/dev/null)" ] && die "--out $OUT is not empty (a recorded tape is not recorded over)"

for tool in mt dd tar python3 sha256sum; do command -v "$tool" >/dev/null || die "$tool is not on PATH"; done
TAR_BIN="$(command -v tar)"
HOME_DIR="$WORK/home"; CFG="$HOME_DIR/config.toml"
SRC="$WORK/src"; STAGING="$WORK/staging"; LOG="$WORK/record.log"
mkdir -p "$HOME_DIR" "$SRC" "$STAGING" "$OUT"
TCTL() { "$BIN" --home "$HOME_DIR" --config "$CFG" "$@"; }
step() { echo "== $*" | tee -a "$LOG" >&2; }
LABEL="REC-0001"
OPERATOR="rec-operator"; ALPHA="rec-alpha"; BRAVO="rec-bravo"

# ---- the home, the config, the keys ----
step "init"
TCTL init --operator "$OPERATOR" >"$WORK/init.out" 2>&1
ESCROW_SECRET="$(grep -o 'AGE-SECRET-KEY-1[A-Z0-9]*' "$WORK/init.out" | head -n 1)"
[ -n "$ESCROW_SECRET" ] || die "init printed no escrow secret"
python3 - "$CFG" "$STAGING" "$TAPE_DEV" "$TAR_BIN" <<'PY'
import re, sys
cfg, staging, tape, tar = sys.argv[1:5]
t = open(cfg).read()
t = re.sub(r'(?m)^binary *=.*$', f'binary = "{tar}"', t, count=1)
t = re.sub(r'(?m)^slice_size *=.*$', 'slice_size = "64K"', t, count=1)
t = re.sub(r'(?m)^min_copies *=.*$', 'min_copies = 1', t, count=1)
t = re.sub(r'(?m)^directory *=.*$', f'directory = "{staging}"', t, count=1)
t = re.sub(r'(?m)^device_tape *=.*$', f'device_tape = "{tape}"', t)
if not re.search(r"(?m)^\[\[backends\.lto\]\]", t):
    t = re.sub(r'(?m)^lto *= *\[\] *\n', '', t)
    t += f'''
[[backends.lto]]
name = "rec-mhvtl"
device_tape = "{tape}"
generation = "LTO-8"
capacity_override = "2748779069440"
fill_ceiling = 0.95
enospc_buffer = "2G"
'''
open(cfg, "w").write(t)
PY
grep -E '^slice_size = "64K"' "$CFG" >/dev/null || die "the config has no slice_size = 64K"
grep -E '^min_copies = 1' "$CFG" >/dev/null || die "the config has no min_copies = 1"
TCTL config check >>"$LOG" 2>&1

step "tenants, archive sets"
TCTL tenant add "$ALPHA" >>"$LOG" 2>&1
TCTL tenant add "$BRAVO" >>"$LOG" 2>&1
TCTL archive-set create rec-zstd --compression zstd >>"$LOG" 2>&1
TCTL archive-set create rec-gzip --compression gzip >>"$LOG" 2>&1

# ---- the source trees: fixed names and contents; EXPECTED.toml is made from these ----
step "source trees"
GEN="$WORK/gen.py"
cat >"$GEN" <<'PY'
import hashlib, os, sys

# make: build the source trees (before `unit init`); expect: write EXPECTED.toml from them
# (after `unit init`, which adds each unit's .tapectl-unit.toml, a member of the tape).
mode, src, expected, label, operator, alpha, bravo = sys.argv[1:8]


def noise(n, seed):
    x = (seed * 2654435761 ^ 0x9E3779B9) & 0xFFFFFFFF
    out = bytearray()
    for _ in range(n):
        x ^= (x << 13) & 0xFFFFFFFF
        x ^= x >> 17
        x ^= (x << 5) & 0xFFFFFFFF
        out.append((x >> 11) & 0xFF)
    return bytes(out)


def text(n, seed):
    words = ["tape", "slice", "member", "archive", "heir", "cartridge", "filemark", "seal"]
    x = (seed * 2654435761) & 0xFFFFFFFF | 1
    out = bytearray()
    while len(out) < n:
        x ^= (x << 13) & 0xFFFFFFFF
        x ^= x >> 17
        x ^= (x << 5) & 0xFFFFFFFF
        out += words[x % len(words)].encode() + b" "
    return bytes(out[:n])


def put(root, rel, data):
    p = os.path.join(root, rel)
    os.makedirs(os.path.dirname(p), exist_ok=True)
    with open(p, "wb") as f:
        f.write(data)
    os.chmod(p, 0o644)


UNITS = [
    ("alpha-main", alpha, None, 0o750),
    ("bravo-zstd", bravo, "rec-zstd", 0o755),
    ("bravo-gzip", bravo, "rec-gzip", 0o755),
]
if mode == "make":
    # alpha-main: several slices at 64K; the special entries.
    r = os.path.join(src, "alpha-main")
    os.makedirs(r)
    for i in range(4):
        put(r, f"data/big-{i + 1}.bin", noise(28_000, 100 + i))
    put(r, "docs/notes.txt", text(3_000, 7))
    put(r, "docs/deep/er/leaf.txt", b"the leaf of a nested folder\n")
    put(r, "empty.dat", b"")
    put(r, "café.txt", "crème brûlée\n".encode())
    put(r, "hard-a.txt", text(2_000, 11))
    os.link(os.path.join(r, "hard-a.txt"), os.path.join(r, "hard-b.txt"))
    os.symlink("docs/notes.txt", os.path.join(r, "link-to-notes"))
    os.makedirs(os.path.join(r, "docs/empty-dir"))
    for d, _, _ in os.walk(r):
        os.chmod(d, 0o755)
    os.chmod(r, 0o750)

    for name, aset, seed in (("bravo-zstd", "rec-zstd", 21), ("bravo-gzip", "rec-gzip", 31)):
        r = os.path.join(src, name)
        os.makedirs(r)
        put(r, "report.txt", text(20_000, seed))
        put(r, "sub/more.txt", text(5_000, seed + 1))
        put(r, "tiny.txt", f"{name}\n".encode())
        for d, _, _ in os.walk(r):
            os.chmod(d, 0o755)
    sys.exit(0)
units = [(n, t, a, m, os.path.join(src, n)) for n, t, a, m in UNITS]


def toml_str(s):
    return '"' + s.replace("\\", "\\\\").replace('"', '\\"') + '"'


lines = [
    "# What tests/fixtures/recorded-2.0.0 holds, made from the source trees by",
    "# scripts/record-recorded-tape.sh and never from tapectl's own catalog.",
    "# kind: file | dir | symlink.  A hard link's second name is a file of the same bytes",
    "# (`hardlink_of` names the first).  `sha256` is of a file's bytes; `target` is a symlink's, and",
    "# a symlink's size is its target's length, as lstat gives it.",
    "",
    "[tape]",
    f"label = {toml_str(label)}",
    f"operator = {toml_str(operator)}",
    f"tenants = [{toml_str(alpha)}, {toml_str(bravo)}]",
    "",
]
for name, tenant, aset, mode, root in sorted(units):
    comp = {None: "none", "rec-zstd": "zstd", "rec-gzip": "gzip"}[aset]
    lines += [
        "[[unit]]",
        f"name = {toml_str(name)}",
        f"tenant = {toml_str(tenant)}",
        "version = 1",
        f"compression = {toml_str(comp)}",
        f"root_mode = 0o{mode:o}",
        "",
    ]
    inodes = {}
    entries = []
    for d, dirs, files in os.walk(root):
        for n in dirs + files:
            full = os.path.join(d, n)
            rel = os.path.relpath(full, root)
            entries.append((rel, full))
    for rel, full in sorted(entries):
        st = os.lstat(full)
        if os.path.islink(full):
            kind, extra = "symlink", f'target = {toml_str(os.readlink(full))}'
            size, sha = len(os.readlink(full).encode()), None
        elif os.path.isdir(full):
            kind, extra, size, sha = "dir", None, 0, None
        else:
            kind, extra = "file", None
            data = open(full, "rb").read()
            size, sha = len(data), hashlib.sha256(data).hexdigest()
            key = (st.st_dev, st.st_ino)
            if st.st_nlink > 1:
                if key in inodes:
                    extra = f"hardlink_of = {toml_str(inodes[key])}"
                else:
                    inodes[key] = rel
        lines += ["[[unit.path]]", f"path = {toml_str(rel)}", f'kind = "{kind}"', f"size = {size}"]
        if sha:
            lines.append(f'sha256 = "{sha}"')
        if extra:
            lines.append(extra)
        lines.append("")
open(expected, "w").write("\n".join(lines))
PY
python3 "$GEN" make "$SRC" "$OUT/EXPECTED.toml" "$LABEL" "$OPERATOR" "$ALPHA" "$BRAVO"

step "units"
TCTL unit init "$SRC/alpha-main" --tenant "$ALPHA" --name alpha-main >>"$LOG" 2>&1
TCTL unit init "$SRC/bravo-zstd" --tenant "$BRAVO" --name bravo-zstd --archive-set rec-zstd >>"$LOG" 2>&1
TCTL unit init "$SRC/bravo-gzip" --tenant "$BRAVO" --name bravo-gzip --archive-set rec-gzip >>"$LOG" 2>&1
# A file written within two seconds of its hash is hashed twice (git's racy case): let the
# trees age so the staging is the ordinary one.
sleep 3
step "snapshot, stage (the real path: GNU tar, cut, pack, age)"
TCTL snapshot create alpha-main bravo-zstd bravo-gzip >>"$LOG" 2>&1
TCTL stage create alpha-main bravo-zstd bravo-gzip >>"$LOG" 2>&1
# EXPECTED.toml, from the source trees as they are now: the unit dotfiles are on the tape.
python3 "$GEN" expect "$SRC" "$OUT/EXPECTED.toml" "$LABEL" "$OPERATOR" "$ALPHA" "$BRAVO"

# ---- the tape: a freshly erased cartridge, one volume ----
step "erase the cartridge, volume init, volume write"
mt -f "$TAPE_DEV" rewind
mt -f "$TAPE_DEV" erase
TCTL volume init "$LABEL" --device "$TAPE_DEV" >>"$LOG" 2>&1
TCTL volume write "$LABEL" --device "$TAPE_DEV" --yes >>"$LOG" 2>&1
TCTL volume verify "$LABEL" --device "$TAPE_DEV" >>"$LOG" 2>&1

# ---- dump every tape file as the heir reads it ----
step "dump the tape (mt setblk 524288; dd bs=512k, one call per tape file)"
mt -f "$TAPE_DEV" setblk 524288
mt -f "$TAPE_DEV" rewind
n=0; empty=0
while [ "$n" -lt 64 ]; do
    f="$OUT/file-$(printf '%03d' "$n")"
    if ! dd if="$TAPE_DEV" bs=512k of="$f" conv=sparse 2>/dev/null; then
        # The one read that may fail is the one at end of data (a blank check): nothing
        # was read, and the drive says it is at EOD. Anything else is a failed dump.
        [ "$(stat -c %s "$f")" -eq 0 ] && mt -f "$TAPE_DEV" status 2>/dev/null | grep -w EOD >/dev/null \
            || die "reading tape file $n failed"
        rm -f "$f"
        break
    fi
    if [ "$(stat -c %s "$f")" -eq 0 ]; then
        rm -f "$f"
        empty=$((empty + 1))
        [ "$empty" -ge 2 ] && break
    else
        empty=0
        # The padding is zeros: keep it a hole on this disk (git compresses it either way).
        cp --sparse=always "$f" "$f.sparse" && mv "$f.sparse" "$f"
    fi
    n=$((n + 1))
done
[ "$n" -lt 64 ] || die "no end of data within 64 files"
mt -f "$TAPE_DEV" rewind
FILES="$(find "$OUT" -maxdepth 1 -name 'file-*' | wc -l)"
[ "$FILES" -ge 5 ] || die "the dump holds only $FILES files"

# ---- the keys: test keys, made for this fixture alone ----
step "keys"
mkdir -p "$OUT/keys"
for who in "$OPERATOR:operator" "$ALPHA:tenant-alpha" "$BRAVO:tenant-bravo"; do
    name="${who%%:*}"; as="${who##*:}"
    key="$(ls "$HOME_DIR"/keys/"$name"-primary.age.key 2>/dev/null)" || die "no primary key of $name"
    install -m 0600 "$key" "$OUT/keys/$as.key"
done
printf '# test key, made for tests/fixtures/recorded-2.0.0 alone: the escrow recipient\n%s\n' \
    "$ESCROW_SECRET" >"$OUT/keys/escrow.key"
chmod 0600 "$OUT/keys/escrow.key"

# ---- RECORDED.md ----
step "RECORDED.md"
DRIVE_LINE="$(lsscsi 2>/dev/null | grep -F "$(basename "$TAPE_DEV" | sed 's/^n//')" | head -n 1 || true)"
{
    echo "# How tests/fixtures/recorded-2.0.0 was recorded"
    echo
    echo "- tapectl: $("$BIN" --version)"
    echo "- recording script at commit (the repository HEAD when it ran; the binary's own commit is the one its --version line names): $(git -C "$REPO" rev-parse HEAD)"
    echo "- GNU tar that made the tape: $("$TAR_BIN" --version | head -n 1)"
    echo "- date (UTC): $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "- drive: $DEVICE ($TAPE_DEV), mhvtl: ${DRIVE_LINE:-not reported by lsscsi}"
    echo "- age: the tape is encrypted by the age crate inside tapectl; no age CLI was used to write it"
    echo "- volume label: $LABEL; tape files: $FILES (file-000 to file-$(printf '%03d' $((FILES - 1)))), each as the heir reads it"
    echo "- command line: \`scripts/record-recorded-tape.sh $ARGS\`"
    echo "- keys: test keys, generated by that run for this fixture alone (keys/)"
} >"$OUT/RECORDED.md"
echo "recorded $FILES tape files into $OUT" >&2
