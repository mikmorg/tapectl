#!/usr/bin/env bash
# first-run.sh — a guided, resumable walk from a bare machine to the first
# sealed production tape.
#
# What it does, in order (each step detects whether it is already done and
# offers to skip; `--from N` starts at a step; nothing touches a tape until
# step 12, and every tape-touching command is confirmed by name):
#
#    1  Rust toolchain (rustup; the repo pins 1.94.1 in rust-toolchain.toml)
#    2  runtime tools: dar >= 2.6, mt, sg3-utils, acl, python3, lsscsi (age optional: heir path + rehearsal)
#    3  build tapectl (release) and install it system-wide
#    4  the ungated test suite (no tape needed)
#    5  the SERVICE USER: a nologin `tapectl` account owns the keys, catalog and staging;
#       every tapectl command from here on runs as it (sudo -u tapectl -H)
#    6  find the tape drive BY SERIAL and confirm the service user can open it
#    7  initialise the tapectl home — mints the escrow identity (paper ready)
#    8  register the drive as a backend, by-id path, generation from the
#       drive's INQUIRY product id (never from the loaded cartridge)
#    9  the Heir Kit — generate, print, seal, two failure domains
#   10  a shelf location for cartridges
#   11  tenants and units: who owns which paths; the service user is granted read by ACL
#   12  OPTIONAL rehearsal on a TEST cartridge (erases it — barcode required; runs as YOU)
#   13  the first production tape: snapshot → stage → init → write → verify
#   14  what to do next
#
# Steps 1-4 run as you (they need your toolchain and sudo). Steps 5-11 and 13
# run tapectl as the service user through one seam, `as_svc`; step 12 runs as
# you because the lifecycle suite builds its own binary. `--no-service-user`
# turns the seam off and runs everything as you, the pre-2026-09-13 behaviour.
#
# Every `tapectl` flag here was taken from the binary's own --help on
# 2026-09-12; if a flag drifts, the failing command prints the real help.
#
# Testing this script against mhvtl (never the real drive) is done with a binary
# the service user can execute (your home is not traversable by it):
#   install -m 0755 target/debug/tapectl /scratch/fr-bin/tapectl
#   scripts/first-run.sh --auto --home /tmp/fr-home --tapectl /scratch/fr-bin/tapectl \
#       --device /dev/tape/by-id/scsi-XYZZY_A1-nst --sg /dev/sg1 --generation LTO-8 --label L6-TEST \
#       --tenant alice --unit-path /tmp/fr-src/photos --skip-build --skip-tests
set -euo pipefail

# ---------------------------------------------------------------- arguments
HOME_DIR=""            # tapectl home; empty = the real default (~/.tapectl)
FROM=0
TO=99
TAPECTL=""             # binary; resolved in step 0 unless given
AUTO=0                 # accept defaults for non-destructive prompts
DEVICE=""; SG=""; LABEL=""; OPERATOR=""; TENANT=""; UNIT_PATH=""; LOCATION=""; KIT_OUT=""; TEST_BARCODE=""; DGEN=""
SKIP_BUILD=0; SKIP_TESTS=0
SVC_USER="tapectl"; SVC_MODE=1   # --no-service-user → run tapectl as yourself
usage() {
  sed -n '2,25p' "$0" | sed 's/^# \{0,1\}//'
  cat <<EOF

Options:
  --home DIR        tapectl home (default ~/.tapectl). Use a temp dir to rehearse.
  --from N          start at step N (1-14)
  --to N            stop after step N (e.g. --to 9: everything up to and including the printed kit)
  --tapectl PATH    the tapectl binary to use (default: PATH, then target/release, then target/debug)
  --user NAME       the service user that owns and runs tapectl (default: tapectl; created in step 5)
  --no-service-user run tapectl as yourself instead — no account, no ACLs, your ~/.tapectl
  --auto            take defaults for non-destructive prompts (for scripted rehearsal)
  --device PATH     tape device by-id path (skips the interactive pick in step 5)
  --sg PATH         matching /dev/sgN (derived from sysfs when omitted)
  --generation GEN  the generation this DRIVE natively writes (default asked; e.g. LTO-6)
  --label L         first volume label (default asked; e.g. L6-0001)
  --operator NAME   operator name for init (default: \$USER)
  --tenant NAME     first tenant (default asked)
  --unit-path DIR   first unit directory (default asked)
  --location NAME   shelf location name (default asked; e.g. home-rack)
  --kit-out DIR     heir kit output dir (default ~/heir-kit)
  --barcode S       TEST cartridge barcode for the step-12 rehearsal (required for it under --auto)
  --skip-build      do not build/install (use the resolved binary as-is)
  --skip-tests      skip the ungated test suite
  -h, --help
EOF
}
while [ $# -gt 0 ]; do
  case "$1" in
    --home) HOME_DIR="$2"; shift 2 ;;
    --from) FROM="$2"; shift 2 ;;
    --to) TO="$2"; shift 2 ;;
    --tapectl) TAPECTL="$2"; shift 2 ;;
    --auto) AUTO=1; shift ;;
    --device) DEVICE="$2"; shift 2 ;;
    --sg) SG="$2"; shift 2 ;;
    --generation) DGEN="$2"; shift 2 ;;
    --label) LABEL="$2"; shift 2 ;;
    --operator) OPERATOR="$2"; shift 2 ;;
    --tenant) TENANT="$2"; shift 2 ;;
    --unit-path) UNIT_PATH="$2"; shift 2 ;;
    --location) LOCATION="$2"; shift 2 ;;
    --kit-out) KIT_OUT="$2"; shift 2 ;;
    --barcode) TEST_BARCODE="$2"; shift 2 ;;
    --skip-build) SKIP_BUILD=1; shift ;;
    --skip-tests) SKIP_TESTS=1; shift ;;
    --user) SVC_USER="$2"; shift 2 ;;
    --no-service-user) SVC_MODE=0; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

# ---------------------------------------------------------------- plumbing
REPO="$(cd "$(dirname "$0")/.." && pwd)"
if [ -t 1 ]; then B=$'\e[1m'; D=$'\e[2m'; R=$'\e[0m'; Y=$'\e[33m'; G=$'\e[32m'; RD=$'\e[31m'; else B=""; D=""; R=""; Y=""; G=""; RD=""; fi
[ "$SVC_MODE" = 1 ] && [ "$SVC_USER" = "$USER" ] && SVC_MODE=0
# as_svc: THE privilege seam. Every tapectl invocation and every read/write of
# tapectl-owned state (the 0700 home, the 0600 config) goes through it; the
# rest of the script stays you, with sudo for root work.
as_svc()   { if [ "$SVC_MODE" = 1 ]; then sudo -u "$SVC_USER" -H "$@"; else "$@"; fi; }
svc_home() { if [ "$SVC_MODE" = 1 ]; then { getent passwd "$SVC_USER" 2>/dev/null || true; } | cut -d: -f6; else printf '%s' "$HOME"; fi; }
svc_in_group() { id -nG "$1" 2>/dev/null | tr ' ' '\n' | grep -qx "$2"; }
compute_home() { local h; h="$(svc_home)"; EFFECTIVE_HOME="${HOME_DIR:-${h:-/var/lib/$SVC_USER}/.tapectl}"; }
compute_home
# The log is yours, never inside the service user's home (which you cannot enter).
LOG="${XDG_STATE_HOME:-$HOME/.local/state}/tapectl/first-run.log"
STEP=0

hdr()     { STEP="$1"; [ "$1" -gt "$TO" ] && { printf '\n%s== stopping before step %s (--to %s) ==%s\n' "$B" "$1" "$TO" "$R"; exit 0; }; printf '\n%s== Step %s — %s ==%s\n' "$B" "$1" "$2" "$R"; }
explain() { printf '%s' "$D"; fold -s -w 78 | sed 's/^/   /'; printf '%s' "$R"; }
note()    { printf '   %s%s%s\n' "$Y" "$*" "$R"; }
ok()      { printf '   %s✓ %s%s\n' "$G" "$*" "$R"; }
die()     { printf '   %s✗ %s%s\n' "$RD" "$*" "$R" >&2; exit 1; }
log()     { mkdir -p "$(dirname "$LOG")"; printf '%s %s\n' "$(date -u +%FT%TZ)" "$*" >> "$LOG"; }
# ask VAR "prompt" "default"
ask() {
  local __v="$1" prompt="$2" def="${3:-}" ans
  if [ "$AUTO" = 1 ] && [ -n "$def" ]; then printf -v "$__v" '%s' "$def"; printf '   %s [auto: %s]\n' "$prompt" "$def"; return; fi
  read -r -p "   $prompt${def:+ [$def]}: " ans </dev/tty || true
  printf -v "$__v" '%s' "${ans:-$def}"
}
# confirm "question" → 0 yes / 1 no. Non-destructive: --auto says yes.
confirm() {
  local ans
  if [ "$AUTO" = 1 ]; then printf '   %s [auto: yes]\n' "$1"; return 0; fi
  read -r -p "   $1 [y/N] " ans </dev/tty || true
  [[ "$ans" =~ ^[Yy] ]]
}
# confirm_destructive "question" "word" → the user must type the word. --auto counts only if the word was supplied via flags.
confirm_destructive() {
  local ans
  if [ "$AUTO" = 1 ]; then printf '   %s [auto: typed "%s" via flags]\n' "$1" "$2"; return 0; fi
  read -r -p "   $1 — type $2 to proceed: " ans </dev/tty || true
  [ "$ans" = "$2" ]
}
# run: echo, log, execute (stdout+stderr go to the terminal AND the log)
run() {
  printf '   %s$ %s%s\n' "$B" "$*" "$R"; log "\$ $*"
  "$@" 2>&1 | tee -a "$LOG"
  return "${PIPESTATUS[0]}"
}
# run_capture FILE cmd...: `run`, plus a copy of the output in FILE for the
# script itself to read back (issue #179 — step 13 needs the bound cartridge's
# placeholder barcode, and needs to tell one refusal from another).
run_capture() {
  local f="$1"; shift
  printf '   %s$ %s%s\n' "$B" "$*" "$R"; log "\$ $*"
  mkdir -p "$(dirname "$f")"
  "$@" 2>&1 | tee "$f" | tee -a "$LOG"
  return "${PIPESTATUS[0]}"
}
# run_nolog: for the one command whose output must never be written to disk
run_nolog() { printf '   %s$ %s%s\n' "$B" "$*" "$R"; log "\$ $* (output NOT logged)"; "$@"; }
tc() { if [ -n "$HOME_DIR" ]; then as_svc "$TAPECTL" --home "$HOME_DIR" "$@"; else as_svc "$TAPECTL" "$@"; fi; }
# The DRIVE's own generation, parsed from its INQUIRY product id (issue #178).
# shellcheck source=lib/drive-generation.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib/drive-generation.sh"
# The heir kit is YOUR artifact to print, but escrow-kit chmods its out dir 0700
# as whoever runs it. Hand the dir to the service user for the write, take it back after.
kit_prepare() { mkdir -p "$1"; [ "$SVC_MODE" = 1 ] && sudo chown -R "$SVC_USER" "$1"; return 0; }
kit_finish()  { [ "$SVC_MODE" = 1 ] && sudo chown -R "$USER" "$1"; return 0; }
# grant_read PATH: make PATH readable (tree) and its top dir writable (the
# .tapectl-unit.toml dotfile) by the service user, via ACLs; ancestors get x.
grant_read() {
  [ "$SVC_MODE" = 1 ] || return 0
  if as_svc test -r "$1" && as_svc test -w "$1" && as_svc test -x "$1"; then ok "$SVC_USER can already read $1"; return 0; fi
  explain <<EOF
$SVC_USER cannot read $1 yet. snapshot and stage read every file in the tree, and unit init writes .tapectl-unit.toml into its top directory. The grant is a POSIX ACL — read+traverse on the tree, a default so files created later inherit it, write on the top directory only, and traverse-only (x) on each ancestor so the path can be reached — leaving the owner and mode of every file exactly as they are.
EOF
  confirm "Grant $SVC_USER read access to $1 (setfacl)?" || return 1
  local p; p="$(dirname "$1")"
  while [ "$p" != / ] && [ -n "$p" ]; do as_svc test -x "$p" || run sudo setfacl -m "u:$SVC_USER:x" "$p" || return 1; p="$(dirname "$p")"; done
  run sudo setfacl -R -m "u:$SVC_USER:rX" "$1" || { note "setfacl failed — a filesystem without ACL support? (mount option acl; ZFS: acltype=posixacl). Fall back to group ownership."; return 1; }
  run sudo setfacl -R -d -m "u:$SVC_USER:rX" "$1" || return 1
  run sudo setfacl -m "u:$SVC_USER:rwX" "$1" || return 1
  as_svc test -r "$1" && as_svc test -w "$1" && as_svc test -x "$1"
}
skip_if() { [ "$FROM" -gt "$STEP" ] && { note "skipped (--from $FROM)"; return 0; }; return 1; }
vercmp_ge() { [ "$(printf '%s\n%s\n' "$2" "$1" | sort -V | head -1)" = "$2" ]; }
# rustup's env is loaded here, not only in step 1, so --from 3 never falls
# through to a distro cargo that cannot parse edition 2021.
# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
# toolchain_check: cargo and rustc must be rustup's, at the pinned version.
toolchain_check() {
  local want have
  want="$(sed -n 's/^channel *= *"\(.*\)"/\1/p' "$REPO/rust-toolchain.toml")"
  have="$( { cd "$REPO" && rustc --version 2>/dev/null || true; } | awk '{print $2}')"
  [ "$have" = "$want" ] || die "rustc is ${have:-absent} at $(command -v rustc || echo '<none>'), the repo pins $want — run step 1 (scripts/first-run.sh --from 1 --to 1)"
  case "$(command -v cargo)" in "$HOME"/.cargo/bin/*|"${CARGO_HOME:-/nonexistent}"/bin/*) ;; *) die "cargo at $(command -v cargo) is not rustup's — a distro cargo cannot build this crate; run step 1" ;; esac
}

# ================================================================ step 0
printf '%stapectl first run%s — repo %s\n' "$B" "$R" "$REPO"
printf 'home: %s   log: %s\n' "$EFFECTIVE_HOME" "$LOG"
printf 'service user: %s\n' "$( [ "$SVC_MODE" = 1 ] && echo "$SVC_USER (every tapectl command runs as it)" || echo "none — running as $USER" )"
if [ "$SVC_MODE" = 0 ] || id "$SVC_USER" >/dev/null 2>&1; then
  if [ -z "$HOME_DIR" ] && as_svc test -e "$EFFECTIVE_HOME/tapectl.db" 2>/dev/null && [ "$FROM" -le 7 ]; then
    note "$EFFECTIVE_HOME is already initialised. This script will detect that in step 7 and not re-init."
  fi
fi
resolve_tapectl() {
  if [ -n "$TAPECTL" ]; then [ -x "$TAPECTL" ] || die "--tapectl $TAPECTL is not executable"; return; fi
  if command -v tapectl >/dev/null 2>&1; then TAPECTL="$(command -v tapectl)"
  elif [ -x "$REPO/target/release/tapectl" ]; then TAPECTL="$REPO/target/release/tapectl"
  elif [ -x "$REPO/target/debug/tapectl" ]; then TAPECTL="$REPO/target/debug/tapectl"
  elif [ -n "${CARGO_TARGET_DIR:-}" ] && [ -x "$CARGO_TARGET_DIR/release/tapectl" ]; then TAPECTL="$CARGO_TARGET_DIR/release/tapectl"
  elif [ -n "${CARGO_TARGET_DIR:-}" ] && [ -x "$CARGO_TARGET_DIR/debug/tapectl" ]; then TAPECTL="$CARGO_TARGET_DIR/debug/tapectl"
  else TAPECTL=""; fi
}

# ================================================================ step 1
hdr 1 "Rust toolchain"
skip_if || {
explain <<'EOF'
tapectl is built from source. The repository pins the exact toolchain in rust-toolchain.toml (1.94.1 with clippy and rustfmt), and rustup installs that version automatically the first time cargo runs here — so the only thing you need is rustup itself.
EOF
if ! command -v rustup >/dev/null 2>&1; then
  note "rustup is not installed."
  if confirm "Install rustup now (official installer, default profile)?"; then
    run bash -c 'curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path'
    # shellcheck disable=SC1091
    [ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
  else die "rustup is required. https://rustup.rs"; fi
fi
( cd "$REPO" && run rustup show active-toolchain ) || true
toolchain_check
ok "rustc $(rustc --version | awk '{print $2}'), cargo $(cargo --version | awk '{print $2}') at $(command -v cargo)"
}

# ================================================================ step 2
hdr 2 "Runtime tools"
skip_if || {
explain <<'EOF'
tapectl shells out to `dar` for every archive (a hard dependency, >= 2.6, 2.7.20+ recommended) and to `mt` and the sg3-utils for drive control, health pages and the cartridge's MAM. `age` is not used by the binary itself — it uses the rage crate — but the on-tape RESTORE.sh, the heir path, needs it, and so does the rehearsal in step 11. `setfacl` (package acl) grants the service user read on your data in step 11. `lsscsi` and `python3` are for step 6 and the lifecycle suite.
EOF
MISSING=()
for t in dar mt sg_read_attr sg_logs setfacl python3 lsscsi; do command -v "$t" >/dev/null 2>&1 || MISSING+=("$t"); done
if [ "${#MISSING[@]}" -gt 0 ]; then
  note "missing: ${MISSING[*]}"
  explain <<'EOF'
Debian/Ubuntu package names: dar, mt-st, sg3-utils, acl, python3, lsscsi.
EOF
  if confirm "Run: sudo apt install dar mt-st sg3-utils acl python3 lsscsi ?"; then run sudo apt install -y dar mt-st sg3-utils acl python3 lsscsi; else die "install the missing tools and re-run with --from 2"; fi
fi
if ! command -v age >/dev/null 2>&1; then
  explain <<'EOF'
`age` (the CLI) is NOT needed by tapectl itself — the binary uses the rage crate. It is needed by RESTORE.sh, the heir path written to every tape, and therefore by the step-12 rehearsal, which runs that script off the tape. Debian ships an `age` package only from bookworm (12) onward; older releases have none, which is why apt cannot find it. Install it from the upstream release (a single static Go binary), or with `go install filippo.io/age/cmd/...@latest`, or skip it for now — step 12 will refuse to run without it, and nothing else here needs it.
EOF
  if confirm "Install age v1.3.2 from the upstream GitHub release into /usr/local/bin now?"; then
    AGE_TAG="v1.3.2"
    AGE_URL="https://github.com/FiloSottile/age/releases/download/${AGE_TAG}/age-${AGE_TAG}-linux-amd64.tar.gz"
    TMPD="$(mktemp -d)"
    run bash -c "curl -fsSL '$AGE_URL' | tar -xz -C '$TMPD'" || die "download failed — check the tag at https://github.com/FiloSottile/age/releases"
    run sudo install -m 0755 "$TMPD/age/age" "$TMPD/age/age-keygen" /usr/local/bin/
    rm -rf "$TMPD"
    run age --version
  else note "continuing without age — the step-12 rehearsal will be unavailable until it is installed"; fi
fi
DARV="$(dar --version 2>&1 | sed -n 's/.*dar version \([0-9.]*\).*/\1/p' | head -1)"
[ -n "$DARV" ] || DARV="$(dar --version 2>&1 | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1)"
[ -n "$DARV" ] || die "could not read dar's version (dar --version)"
vercmp_ge "$DARV" "2.6.0" || die "dar $DARV is too old; tapectl needs >= 2.6"
vercmp_ge "$DARV" "2.7.20" || note "dar $DARV works; 2.7.20+ is recommended (bookworm ships 2.7.x)"
ok "dar $DARV, mt, sg3-utils, setfacl, python3, lsscsi present$(command -v age >/dev/null 2>&1 && echo ", age present" || echo "; age ABSENT (heir-path script and rehearsal only)")"
}

# ================================================================ step 3
hdr 3 "Build tapectl"
skip_if || {
if [ "$SKIP_BUILD" = 1 ]; then note "--skip-build"; else
explain <<'EOF'
Everything validated on mhvtl and on the real drive so far ran the debug binary — it is the proven artifact. A release build is the same source with optimisation on: markedly faster at the sha256 hashing and age encryption a multi-hundred-gigabyte write is made of. Build release for production, then run one real-drive rehearsal on it (step 12) before you trust it with data, because a different binary is a different artifact. Install it to /usr/local/bin: the service user cannot execute a binary under your home.
EOF
if confirm "Build the release binary now (cargo build --release; a few minutes)?"; then
  toolchain_check
  ( cd "$REPO" && run cargo build --release )
  TAPECTL="$REPO/target/release/tapectl"
  [ -x "$TAPECTL" ] || TAPECTL="${CARGO_TARGET_DIR:-$REPO/target}/release/tapectl"
  if confirm "Install it to /usr/local/bin/tapectl (sudo)?"; then run sudo install -m 0755 "$TAPECTL" /usr/local/bin/tapectl; TAPECTL=/usr/local/bin/tapectl; fi
fi
fi
}
resolve_tapectl
[ -n "$TAPECTL" ] || die "no tapectl binary found — build one (step 3) or pass --tapectl"
run "$TAPECTL" --version
ok "using $TAPECTL"

# ================================================================ step 4
hdr 4 "The ungated test suite"
skip_if || {
if [ "$SKIP_TESTS" = 1 ]; then note "--skip-tests"; else
explain <<'EOF'
`cargo test` runs ~900 tests that need no tape and no mhvtl — only dar. It proves this machine's dar, filesystem and toolchain behave the way the suite expects. Two to three minutes.
EOF
if confirm "Run cargo test now?"; then toolchain_check; ( cd "$REPO" && run cargo test ) || die "the suite is red on this machine — stop here and look"; ok "suite green"; fi
fi
}

# ================================================================ step 5
hdr 5 "The service user"
skip_if || {
if [ "$SVC_MODE" != 1 ]; then note "--no-service-user: tapectl runs as $USER, home $EFFECTIVE_HOME"; else
explain <<EOF
WHY A SERVICE USER. The tapectl home holds the operator private key, every tenant private key and the catalog, and tapectl resolves that home purely from \$HOME. Under a dedicated account those files share a home with nothing else — not your shell history, your ssh agent or the next script you run — and the systemd audit timer in contrib/ gets the fixed User= and HOME= it needs. The account is a nologin system user; you drive it with \`sudo -u $SVC_USER -H tapectl …\`, which is what this script does for every tapectl command from here on.

Its needs are exactly three, and each is granted where it arises: membership of the group that owns the drive nodes (step 6), read on the trees it archives (per unit, by ACL, step 11), and ownership of the staging directory (step 7). Restore needs no root — dar runs with -O — but a restore destination must be writable by $SVC_USER.
EOF
if id "$SVC_USER" >/dev/null 2>&1; then ok "user $SVC_USER exists: $(id "$SVC_USER")"
else
  confirm "Create system user $SVC_USER (home /var/lib/$SVC_USER, shell nologin)?" || die "no service user — re-run with --no-service-user to run tapectl as $USER"
  run sudo useradd --system --create-home --home-dir "/var/lib/$SVC_USER" --shell /usr/sbin/nologin --comment "tapectl archival service" "$SVC_USER"
fi
SVC_HOME="$(svc_home)"; [ -n "$SVC_HOME" ] || die "no home directory for $SVC_USER"
[ -d "$SVC_HOME" ] || { run sudo mkdir -p "$SVC_HOME"; run sudo chown "$SVC_USER" "$SVC_HOME"; }
as_svc id >/dev/null || die "sudo -u $SVC_USER failed — sudo is required for the service-user mode"
as_svc test -x "$TAPECTL" || die "$TAPECTL is not executable by $SVC_USER — install it system-wide (step 3 → /usr/local/bin) or pass --tapectl"
if [ -n "$HOME_DIR" ]; then run sudo mkdir -p "$HOME_DIR"; run sudo chown "$SVC_USER" "$HOME_DIR"; fi
compute_home
run as_svc "$TAPECTL" --version
ok "$SVC_USER ready — home $SVC_HOME, tapectl home $EFFECTIVE_HOME"
fi
}

# ================================================================ step 6
hdr 6 "Find the tape drive — by serial, never by number"
skip_if || {
explain <<'EOF'
/dev/nstN numbers move across reboots on any host with more than one SCSI device. The stable name is the serial under /dev/tape/by-id/. tapectl's config stores that path. Nothing in this step touches the tape.
EOF
run ls -l /dev/tape/by-id/ || note "(no /dev/tape/by-id — is a drive attached?)"
command -v lsscsi >/dev/null 2>&1 && run lsscsi -g || true
if [ -z "$DEVICE" ]; then
  DEF=""; for f in /dev/tape/by-id/*-nst; do case "$f" in *XYZZY*) ;; *) [ -e "$f" ] && { DEF="$(basename "$f")"; break; } ;; esac; done
  ask DEVICE "by-id path of the drive (…-nst, the non-rewinding node)" "${DEF:+/dev/tape/by-id/$DEF}"
fi
[ -e "$DEVICE" ] || die "$DEVICE does not exist"
NST="$(readlink -f "$DEVICE")"; NSTN="$(basename "$NST")"
if [ -z "$SG" ]; then
  SGN="$(ls "/sys/class/scsi_tape/$NSTN/device/scsi_generic/" 2>/dev/null | head -1)"
  [ -n "$SGN" ] && SG="/dev/$SGN"
  ask SG "matching SCSI-generic node (for health and MAM)" "$SG"
fi
[ -e "$SG" ] || die "$SG does not exist"
if [ "$SVC_MODE" = 1 ]; then
  for node in "$NST" "$SG"; do
    g="$(stat -c %G "$node")"
    if ! svc_in_group "$SVC_USER" "$g" && [ "$g" != root ]; then
      note "$node is group $g; $SVC_USER is not a member"
      confirm "sudo usermod -aG $g $SVC_USER ?" && run sudo usermod -aG "$g" "$SVC_USER"
    fi
  done
  as_svc test -r "$NST" && as_svc test -w "$NST" || die "$SVC_USER cannot open $NST ($(stat -c '%A %U:%G' "$NST")) — fix the group and re-run with --from 6"
  as_svc test -r "$SG" && as_svc test -w "$SG" || note "$SVC_USER cannot open $SG ($(stat -c '%A %U:%G' "$SG")) — health pages and MAM reads will fail until it can"
fi
run as_svc mt -f "$DEVICE" status || note "mt status failed — is a cartridge loaded, and is the device readable?"
ok "drive: $DEVICE ($NST), sg: $SG$( [ "$SVC_MODE" = 1 ] && echo " — openable by $SVC_USER" )"
}

# ================================================================ step 7
hdr 7 "Initialise the tapectl home"
skip_if || {
if as_svc test -e "$EFFECTIVE_HOME/tapectl.db"; then
  ok "$EFFECTIVE_HOME is already initialised — not re-running init"
  # An initialised home can still carry a config this version refuses to
  # load: #171 made unknown keys a hard error and #172 deleted four keys
  # that older `init` runs wrote. The db check above says nothing about
  # that, so without this every step below dies on the same load error with
  # no idea why (ADR-0012; the pre-production rulings ruled the breakage
  # acceptable BECAUSE first-run offers a way out — this is that way out).
  # Probe BEHAVIOUR, not a message: `config show` succeeds only if the
  # config actually loads, which is true whatever `config check` prints
  # (issue #173 changed that text, and pinning a substring of it here would
  # silently stop detecting the day it changes again).
  if ! tc config show >/dev/null 2>&1; then
    note "config.toml exists but this tapectl cannot LOAD it:"
    tc config check 2>&1 | head -20 | sed 's/^/     /'
    explain <<'EOF'
The message above names the offending key. If it names a specific table or key
to delete, that edit is the smallest fix and keeps your backends, collections
and staging path intact.

Regenerating starts from tapectl's own defaults instead. Your database, keys,
tenants and every tape are untouched — only config.toml is replaced — but the
config's [[backends.lto]] and [[collections]] tables are NOT carried over.
This script re-adds the drive in step 8; any collections must be re-added by
hand afterwards.
EOF
    if confirm "Back up config.toml and write a fresh default one?"; then
      CFG_BAK="$EFFECTIVE_HOME/config.toml.superseded-$(date +%Y%m%d-%H%M%S)"
      as_svc cp "$EFFECTIVE_HOME/config.toml" "$CFG_BAK" || die "could not back up config.toml"
      # Generated through tapectl's own writer in a throwaway home, never a
      # template kept in this script: a hand-written default here would drift
      # the moment a config field is added. --no-escrow so the throwaway home
      # mints no identity and prints no secret (ADR-0005).
      CFG_TMP="$(as_svc mktemp -d)" || die "could not make a temp dir"
      as_svc "$TAPECTL" --home "$CFG_TMP" --config "$CFG_TMP/config.toml" init --no-escrow >/dev/null 2>&1 \
        || { as_svc rm -rf "$CFG_TMP"; die "could not generate a fresh config; the original is untouched"; }
      as_svc cp "$CFG_TMP/config.toml" "$EFFECTIVE_HOME/config.toml" \
        || { as_svc rm -rf "$CFG_TMP"; die "could not install the fresh config; the original is at $CFG_BAK"; }
      as_svc rm -rf "$CFG_TMP"
      tc config show >/dev/null 2>&1 \
        || die "the freshly generated config still does not load — that is a bug in tapectl, not in your configuration; your original is at $CFG_BAK"
      ok "fresh config.toml written; previous kept at $CFG_BAK"
      note "staging.directory and the drive are re-asked below; re-add any [[collections]] by hand."
    else
      die "cannot continue with a config tapectl will not load — fix the key named above, then re-run with --from 7"
    fi
  fi
else
explain <<'EOF'
`tapectl init` creates the database, config and the operator tenant — and mints the permanent ESCROW IDENTITY (ADR-0005): the one key that is a recipient of every tape and is never rotated. Its SECRET half is printed ONCE, to your terminal, and stored nowhere on this machine. Have paper ready; write it down before you do anything else. It later goes on the Heir Kit's cover sheet (step 9), which is how an heir — or you, on a rebuilt machine — gets back in.

If you are REBUILDING a machine and already hold the original escrow key, do NOT let init mint a new one: answer with the original public key below and it is adopted instead (no command can replace a registered escrow identity later).

This step's output is deliberately NOT written to the log.
EOF
  ask OPERATOR "operator name" "${OPERATOR:-$USER}"
  ADOPT=""
  if [ "$AUTO" != 1 ]; then ask ADOPT "existing escrow PUBLIC key to adopt (age1…, or a .pub path) — leave empty to mint a new one" ""; fi
  note "PAPER READY? The escrow secret appears exactly once, next."
  confirm "Run init now? (type y — Enter means no, nothing has been created)" || die "stopped before init: nothing was minted or written. Re-run with --from $STEP and answer y when the paper is ready"
  if [ -n "$ADOPT" ]; then run_nolog tc init --operator "$OPERATOR" --escrow-public-key "$ADOPT"
  else run_nolog tc init --operator "$OPERATOR"; fi
  note "Written down? It will not be shown again."
fi
run tc config check || note "config check reported something — read it; advisory, exit code above"
run tc db fsck || true
explain <<'EOF'
STAGING SPACE. `stage create` writes every encrypted slice of a unit to the staging directory before anything goes to tape, so it needs room for the largest batch you will write in one session — up to a full cartridge (2.5 TB for LTO-6) if you fill tapes in one go. init writes a default path into config.toml that may not exist, or may not be the service user's to write to. Put staging on a filesystem with the space, owned by the user that runs tapectl.
EOF
CFG="$EFFECTIVE_HOME/config.toml"
SD="$(as_svc sed -n '/^\[staging\]/,/^\[/{s/^directory *= *"\(.*\)"/\1/p}' "$CFG" | head -1)"
SD_DEF="$SD"
if [ -z "$SD" ] || ! as_svc test -d "$SD" || ! as_svc test -w "$SD"; then
  note "staging.directory is '${SD:-<unset>}' — $( [ -z "$SD" ] && echo unset || { [ -d "$SD" ] && echo "not writable by $SVC_USER" || echo "does not exist"; } )"
  [ "$AUTO" = 1 ] && SD_DEF="$EFFECTIVE_HOME/staging"
fi
ask SD_NEW "staging directory (needs space for a full tape's slices)" "$SD_DEF"
if [ "$SD_NEW" != "$SD" ]; then
  as_svc sed -i "/^\[staging\]/,/^\[/{s|^directory *= *\".*\"|directory = \"$SD_NEW\"|}" "$CFG"
  as_svc grep -q "^directory = \"$SD_NEW\"" "$CFG" || die "could not rewrite staging.directory in $CFG — edit it by hand"
fi
[ -d "$SD_NEW" ] || run sudo mkdir -p "$SD_NEW"
if ! as_svc test -w "$SD_NEW"; then
  OWN_USER="$( [ "$SVC_MODE" = 1 ] && echo "$SVC_USER" || echo "$USER" )"
  note "$SD_NEW is not writable by $OWN_USER"; confirm "sudo chown $OWN_USER $SD_NEW ?" && run sudo chown "$OWN_USER" "$SD_NEW"
fi
as_svc test -w "$SD_NEW" || die "staging directory $SD_NEW is not writable by $( [ "$SVC_MODE" = 1 ] && echo "$SVC_USER" || echo "$USER" )"
run as_svc df -h "$SD_NEW"
ok "staging at $SD_NEW"
ok "home ready at $EFFECTIVE_HOME"
}

# ================================================================ step 8
hdr 8 "Register the drive as a backend"
skip_if || {
CFG="$EFFECTIVE_HOME/config.toml"
if as_svc grep -q '^\[\[backends.lto\]\]' "$CFG" 2>/dev/null; then ok "a [[backends.lto]] entry already exists in $CFG"; run as_svc grep -A5 '^\[\[backends.lto\]\]' "$CFG" || true
else
explain <<'EOF'
`backend add` appends a [[backends.lto]] table to config.toml with the by-id tape path and the sg node, so volume write knows where to write and the health checks know where to ask. You declare what the DRIVE is — its own generation — and nothing about the tapes you will feed it: each cartridge's generation is read from its density code when the volume is initialised, and that is what fixes the tape's capacity. So one LTO-6 drive handles LTO-5 and LTO-6 cartridges with nothing to change between them, and media the drive cannot write is refused before the tape is touched (ADR-0010).
EOF
  [ -n "$DEVICE" ] || die "no device chosen — run with --from 6"
  ask BNAME "backend name" "lto6"
  # ADR-0010 decision 1: a drive declares only the one generation it IS. What
  # cartridge happens to be loaded says NOTHING about that — this used to
  # default from the loaded medium's density, so an LTO-5 tape in an LTO-6
  # drive during setup wrote `generation = "LTO-5"` permanently, and every
  # later `volume init` refused with a message about physics instead of naming
  # the config key that was wrong (issue #178).
  #
  # The drive's identity comes from its own INQUIRY product id. sysfs first:
  # world-readable, opens no device node, needs no sg permission for the
  # service user, and identical for mhvtl and the real drive.
  DRIVE_MODEL="$(as_svc cat "/sys/class/scsi_tape/$NSTN/device/model" 2>/dev/null | sed 's/[[:space:]]*$//')"
  DRIVE_VENDOR="$(as_svc cat "/sys/class/scsi_tape/$NSTN/device/vendor" 2>/dev/null | sed 's/[[:space:]]*$//')"
  if [ -z "$DRIVE_MODEL" ] && command -v sg_inq >/dev/null 2>&1 && [ -n "$SG" ]; then
    DRIVE_MODEL="$(as_svc sg_inq "$SG" 2>/dev/null | sed -n 's/^ *Product identification: *//p' | head -1 | sed 's/[[:space:]]*$//')"
  fi
  DRIVE_GEN=""
  if [ -n "$DRIVE_MODEL" ]; then
    DRIVE_GEN="$(drive_generation_from_model "$DRIVE_MODEL")" || DRIVE_GEN=""
  fi
  if [ -n "$DRIVE_GEN" ]; then
    ok "drive identifies as '${DRIVE_VENDOR:+$DRIVE_VENDOR }$DRIVE_MODEL' -> $DRIVE_GEN"
  else
    note "cannot derive a generation from this drive's product id '${DRIVE_VENDOR:+$DRIVE_VENDOR }${DRIVE_MODEL:-<unreadable>}'"
  fi

  # Default chain: --generation, else the product id, else NOTHING. There is
  # deliberately no literal fallback: a wrong generation here is written into
  # config.toml permanently and there is no `backend edit`.
  if [ -n "${DGEN:-}" ]; then
    :
  elif [ -n "$DRIVE_GEN" ]; then
    DGEN="$DRIVE_GEN"
  elif [ "$AUTO" = 1 ]; then
    die "cannot derive this drive's generation from '${DRIVE_VENDOR:+$DRIVE_VENDOR }${DRIVE_MODEL:-<unreadable>}' — pass --generation LTO-n"
  fi
  ask DGEN "generation this DRIVE natively writes (LTO-5 … LTO-9)" "${DGEN:-}"
  [ -n "$DGEN" ] || die "no drive generation given, and none could be derived — pass --generation LTO-n"

  # The loaded medium's density is a CROSS-CHECK, evaluated after DGEN is
  # final. It never feeds the default (ADR-0010) and never changes DGEN.
  MEDIUM_GEN="$(as_svc mt -f "$DEVICE" status 2>/dev/null | sed -n 's/.*Density code 0x[0-9a-fA-F]* (\([^)]*\)).*/\1/p' | head -1)"
  if [ -z "$MEDIUM_GEN" ]; then
    note "no cartridge loaded or density unreadable — nothing to cross-check"
  else
    dnum="${DGEN##*-}"; mnum="$(printf '%s' "$MEDIUM_GEN" | sed -n 's/.*[Ll][Tt][Oo][- ]*\([0-9][0-9]*\).*/\1/p')"
    if [ -z "$mnum" ]; then
      note "loaded medium reports '$MEDIUM_GEN', which is not an LTO generation — nothing to cross-check"
    elif [ "$mnum" = "$dnum" ]; then
      ok "loaded medium agrees (LTO-$mnum)"
    elif [ "$mnum" -gt "$dnum" ] 2>/dev/null; then
      note "loaded medium reports LTO-$mnum but this drive identifies as $DGEN — an LTO drive cannot hold a newer generation, so the model string was misread or 'mt status' is not this drive; check 'lsscsi -g' and pass --generation if the drive is right"
    else
      note "loaded medium is LTO-$mnum — a fact about that cartridge, not this drive (ADR-0010); the drive stays $DGEN"
    fi
  fi
  run tc backend add --name "$BNAME" --device-tape "$DEVICE" --device-sg "$SG" --generation "$DGEN"
  run tc config check || true
fi
}

# ================================================================ step 9
hdr 9 "The Heir Kit"
skip_if || {
explain <<'EOF'
`key escrow-kit` writes three files: COVER.txt (the escrow key in retypable Bech32 plus instructions — the decades-scale artifact, readable with cat), escrow-kit.html (the same with a QR, for printing from a browser) and catalog.db.age (the whole catalog encrypted to the escrow key). Yours to do afterwards: PRINT COVER.txt, seal it in tamper-evident envelopes, and keep copies in at least two independent failure domains. Generate it now, before any data — `audit` will remind you (escrow_kit_stale, a warning) after every write, which is the cue to regenerate with the real catalog.
EOF
ask KIT_OUT "kit output directory" "${KIT_OUT:-$HOME/heir-kit}"
if [ -f "$KIT_OUT/COVER.txt" ] && ! confirm "A kit already exists in $KIT_OUT — regenerate?"; then ok "keeping the existing kit"; else
  kit_prepare "$KIT_OUT"
  run tc key escrow-kit --out "$KIT_OUT"; kit_finish "$KIT_OUT"
  [ -r "$KIT_OUT/COVER.txt" ] || die "$KIT_OUT/COVER.txt is not readable by $USER"
fi
note "Print: $KIT_OUT/COVER.txt   (and/or open $KIT_OUT/escrow-kit.html and print)"
note "Seal in tamper-evident envelopes; two failure domains; refresh after each write session."
}

# ================================================================ step 10
hdr 10 "A shelf location"
skip_if || {
explain <<'EOF'
Cartridges live somewhere. A `shelf` location is a physical place — a rack, a drawer, a relative's house — and `volume move` records which cartridge is where, so `catalog locate` can answer "which building do I drive to". Policy can require copies in more than one location; that is how fire-risk is derived.
EOF
EXISTING="$(tc location list --json 2>/dev/null | python3 -c 'import json,sys
try:
  d=json.load(sys.stdin); rows=d if isinstance(d,list) else d.get("locations",[]); print(" ".join(r.get("name","") for r in rows))
except Exception: pass' 2>/dev/null || true)"
ask LOCATION "location name" "${LOCATION:-${EXISTING%% *}}"; [ -n "$LOCATION" ] || ask LOCATION "location name" "home-rack"
case " $EXISTING " in
  *" $LOCATION "*) ok "location $LOCATION already exists" ;;
  *) ask LDESC "description" "the rack next to the drive"; run tc location add "$LOCATION" -d "$LDESC" ;;
esac
}

# ================================================================ step 11
hdr 11 "Tenants and units — who owns which paths"
skip_if || {
explain <<'EOF'
The model, in three words: TENANT, UNIT, COLLECTION.

A TENANT is a person or trust domain with their own age keys. Everything staged for a tenant is encrypted to that tenant's key (plus yours as operator, plus escrow), and NOTHING about it is on the tape in plaintext — not names, not filenames. A tenant restores their own data with only their key and RESTORE.sh; they cannot read another tenant's.

A UNIT is one directory archived as one entity: a photo year, a project, a show season. It gets a .tapectl-unit.toml (a uuid, so renames are survivable) and is what you snapshot, stage and restore. Keep units at the size you would want to restore in one go.

A COLLECTION is a folder-per-unit source root bound to ONE tenant: `collection sync` registers every child folder at a fixed depth as a unit — the right tool when a tenant's data is already "one folder per thing". Configure it as a [[collections]] table in config.toml.

Layout that works: one top directory per tenant (/data/alice, /data/bob), units or a collection root beneath. A path belongs to exactly one tenant.

Tenants are KEY domains, not Unix accounts: one service user reads every tenant's data and encrypts each to its own key. So each unit directory you register here is granted to the service user by ACL, read-only on the tree, before it is registered.
EOF
NT="$(tc tenant list --json 2>/dev/null | python3 -c 'import json,sys
try:
  d=json.load(sys.stdin); rows=d if isinstance(d,list) else d.get("tenants",[]); print(len([r for r in rows if not r.get("is_operator")]))
except Exception: print(0)' 2>/dev/null || echo 0)"
[ "$NT" != 0 ] && note "$NT non-operator tenant(s) exist already."
ADD_MORE=1
while [ "$ADD_MORE" = 1 ]; do
  if [ "$NT" != 0 ] && [ "$AUTO" = 1 ]; then break; fi
  if [ "$NT" != 0 ] && ! confirm "Add a tenant?"; then break; fi
  ask TENANT "tenant name (letters, digits, dot, underscore, dash)" "${TENANT:-alice}"
  ask TDESC "description" "$TENANT's data"
  run tc tenant add "$TENANT" -d "$TDESC" || note "(already exists?)"
  explain <<'EOF'
Register this tenant's data as units. Give a directory and it becomes one unit; repeat for each. (For a folder-per-unit tree, add a [[collections]] stanza instead — shown at the end of this step.)
EOF
  UP="$UNIT_PATH"
  while :; do
    ask UP "directory to register as a unit for $TENANT (empty to finish)" "${UP:-}"
    [ -z "$UP" ] && break
    [ -d "$UP" ] || { note "$UP is not a directory"; UP=""; [ "$AUTO" = 1 ] && break; continue; }
    grant_read "$UP" || { note "$SVC_USER cannot read $UP — not registering it"; UP=""; [ "$AUTO" = 1 ] && break; continue; }
    ask UNAME "unit name" "$TENANT/$(basename "$UP")"
    if ! run tc unit init --tenant "$TENANT" --name "$UNAME" "$UP"; then
      # A .tapectl-unit.toml already in the directory means this unit was
      # registered before — by an earlier run, or on the machine this one is
      # replacing. The dotfile carries the uuid, so the right move is to adopt
      # it, not to overwrite it: `unit discover` scans the configured
      # watch_roots and registers what it finds.
      if [ -f "$UP/.tapectl-unit.toml" ]; then
        note "$UP already carries a .tapectl-unit.toml — adopting it instead of re-creating"
        WR="$(as_svc sed -n 's/^watch_roots *= *//p' "$CFG" | head -1)"
        if [ -z "$WR" ] || [ "$WR" = "[]" ]; then
          as_svc sed -i "s|^watch_roots *= *\[\]|watch_roots = [\"$UP\"]|" "$CFG" \
            || note "could not add $UP to watch_roots — add it by hand and run: tapectl unit discover"
        fi
        run tc unit discover || note "(unit discover found nothing — check watch_roots in $CFG)"
      else
        note "(unit init failed — see above)"
      fi
    fi
    UP=""; [ "$AUTO" = 1 ] && break
  done
  NT=$((NT+1)); [ "$AUTO" = 1 ] && ADD_MORE=0
done
run tc tenant list || true
run tc unit list || true
explain <<'EOF'
Folder-per-unit alternative — add to config.toml, then `tapectl collection sync`:

  [[collections]]
  name       = "alice-photos"       # unit names become "alice-photos/<folder>"
  root       = "/data/alice/photos"
  tenant     = "alice"
  unit_depth = 1                    # 1 = each immediate child folder is a unit
  # exclude    = ["*.partial"]
  # archive_set = "critical"

`collection status` shows what is pending; `collection plan` fills tape-sized batches; `collection run` stages and writes them. Grant the service user the root the same way first:
  sudo setfacl -R -m u:tapectl:rX /data/alice/photos && sudo setfacl -R -d -m u:tapectl:rX /data/alice/photos
  sudo setfacl -m u:tapectl:rwX /data/alice/photos/*      # each unit folder gets the dotfile
EOF
}

# ================================================================ step 12
hdr 12 "OPTIONAL rehearsal on a TEST cartridge (erases it)"
skip_if || {
explain <<'EOF'
Before real data, the lifecycle suite can run a whole simulated first year — write, verify, every restore path including the heir script off the tape — on a cartridge you are willing to lose. It ERASES that cartridge. It needs its barcode typed exactly, cross-checked against the cartridge's MAM. This is the step that proves the drive, the host's st driver and the binary you built agree.

This step runs as YOU, not the service user: the suite builds its own debug binary with your toolchain and uses throwaway homes under /scratch. It only needs your login to be able to open the drive.
EOF
if ! command -v age >/dev/null 2>&1; then note "skipped: the rehearsal runs RESTORE.sh off the tape, which needs the age CLI (step 2 explains how to install it)"
elif [ "$AUTO" = 1 ] && [ -z "$TEST_BARCODE" ]; then note "skipped under --auto: no --barcode given (erasing a cartridge is never a default)"
elif confirm "Run the first-year rehearsal on a TEST cartridge now?"; then
  NST="$(readlink -f "$DEVICE")"; DEVGRP="$(stat -c %G "$NST")"; WRAP=()
  if ! { [ -r "$NST" ] && [ -w "$NST" ]; }; then
    note "$USER cannot open $NST ($(stat -c '%A %U:%G' "$NST"))"
    svc_in_group "$USER" "$DEVGRP" || { confirm "sudo usermod -aG $DEVGRP $USER (takes effect at your next login)?" && run sudo usermod -aG "$DEVGRP" "$USER"; }
    svc_in_group "$USER" "$DEVGRP" || die "$USER is not in group $DEVGRP — cannot run the rehearsal"
    WRAP=(sg "$DEVGRP" -c)   # this login predates the membership; sg opens a shell with it now
  fi
  if [ -n "$SG" ]; then run sudo sg_read_attr "$SG" | grep -iE "Medium serial|manufacturer" || true; fi
  ask TEST_BARCODE "barcode/serial of the TEST cartridge in the drive (it will be erased)" "$TEST_BARCODE"
  [ -n "$TEST_BARCODE" ] || die "no barcode given"
  if confirm_destructive "ERASE $TEST_BARCODE and run the rehearsal" "$TEST_BARCODE"; then
    CMD="cd '$REPO' && bash scripts/lifecycle-suite.sh --scenario first-year --device '$DEVICE' --erase short --single-cartridge --i-will-lose-the-cartridge '$TEST_BARCODE'"
    if [ "${#WRAP[@]}" -gt 0 ]; then run "${WRAP[@]}" "$CMD"; else run bash -c "$CMD"; fi || die "rehearsal RED — do not write real data until this is understood"
    ok "rehearsal green"
    note "Eject the test cartridge (mt -f $DEVICE offline) and load the production one before step 13."
  fi
else note "skipped"; fi
}

# ================================================================ step 13
hdr 13 "The first production tape"
skip_if || {
NV="$(tc catalog stats --json 2>/dev/null | python3 -c 'import json,sys
try: print(json.load(sys.stdin).get("volumes",0))
except Exception: print(0)' 2>/dev/null || echo 0)"
[ "$NV" != 0 ] && note "$NV volume(s) already in the catalog." && { confirm "Write another tape now?" || { ok "nothing to do"; FROM=14; }; }
if [ "$FROM" -le 13 ]; then
explain <<'EOF'
The pipeline is three phases: `snapshot create` walks the unit and records what exists; `stage create` runs dar, hashes, encrypts to every recipient and writes slices to staging; `volume write` plans the whole tape first — every file, position and size — then writes it in one session and reads the seal back. A sealed volume is immutable: there is no append. Then `volume verify --full` reads every byte back against the front index, which turns the tape's claims into checked evidence.

`volume init` also reads the loaded cartridge: its generation from the density code, which fixes this tape's capacity and is checked against what the drive can write, and its medium serial, which binds the volume to a cartridge in the catalog (ADR-0010). Nothing to set for a mixed LTO-5/LTO-6 shelf — each tape is planned against its own size.

Label convention: something you can write on the cartridge, e.g. L6-0001.
EOF
  [ -n "$DEVICE" ] || die "no device — run with --from 6"
  run as_svc mt -f "$DEVICE" status || true
  if as_svc mt -f "$DEVICE" status 2>/dev/null | grep -q DR_OPEN; then die "no cartridge loaded in $DEVICE"; fi
  ask LABEL "volume label" "${LABEL:-L6-0001}"
  explain <<'EOF'
THE CARTRIDGE. There is nothing to register and nothing to type. A cartridge is known by the serial its chip reports, and a barcode is a sticker (ADR-0012) — so `volume init` reads that serial, registers the cartridge itself, and wears the serial as a placeholder barcode until you replace it. Put the sticker on whenever you like, before or after this write, with `cartridge relabel`; the command is printed below once the cartridge is registered. Registering by hand FIRST is the one thing not to do: init matches on the serial, finds no row carrying it, and registers a second cartridge — two rows for one tape, with your label on the one the catalog is not using.
EOF
  UNITS="$(tc unit list --json 2>/dev/null | python3 -c 'import json,sys
try:
  d=json.load(sys.stdin); rows=d if isinstance(d,list) else d.get("units",[]); print("\n".join(r.get("name","") for r in rows))
except Exception: pass' 2>/dev/null || true)"
  [ -n "$UNITS" ] || die "no units registered — run with --from 11"
  printf '   units:\n%s\n' "$(printf '%s\n' "$UNITS" | sed 's/^/     /')"
  confirm "Snapshot and stage EVERY unit above?" || die "stopped"
  while IFS= read -r u; do
    [ -z "$u" ] && continue
    run tc snapshot create "$u" || die "snapshot failed for $u"
    run tc stage create "$u" || die "stage failed for $u"
  done <<< "$UNITS"
  run tc staging status || true
  confirm_destructive "WRITE volume $LABEL to the cartridge in $DEVICE (the cartridge's current contents are overwritten)" "$LABEL" || die "stopped before writing"
  # Capture init's output as well as logging it: two later steps read it back
  # -- the placeholder barcode it reports, and which refusal it gave.
  INIT_OUT="$(dirname "$LOG")/volume-init-$LABEL.out"
  if ! run_capture "$INIT_OUT" tc volume init "$LABEL" --device "$DEVICE"; then
    if grep -q "no medium serial" "$INIT_OUT"; then
      # ADR-0012: with no readable serial the operator must name the cartridge,
      # and that is their word -- there is no default to fall back on and no
      # flag that could supply one, so --auto stops here rather than guessing.
      explain <<'EOF'
This drive reports no medium serial, so tapectl cannot tell which physical cartridge is loaded and will not guess (ADR-0012). Name it: the barcode you give becomes the recorded identity, and the tape itself will say so — File 0 records `cartridge_identity_source = "operator"` rather than "mam", so anyone reading this tape later can tell a chip-verified serial from a label somebody typed.
EOF
      [ "$AUTO" = 1 ] && die "volume init could read no medium serial; naming a cartridge is your word, re-run scripts/first-run.sh --from 13 interactively"
      ask CARTRIDGE "no serial readable — name this cartridge (the barcode on its sticker)"
      [ -n "$CARTRIDGE" ] || die "no cartridge named"
      if ! tc cartridge list --json 2>/dev/null | grep -q "\"$CARTRIDGE\""; then
        CGEN="$(as_svc mt -f "$DEVICE" status 2>/dev/null | sed -n 's/.*Density code 0x[0-9a-fA-F]* (\([^)]*\)).*/\1/p' | head -1)"
        if [ -n "$CGEN" ]; then note "the loaded medium reports $CGEN"; else note "could not read the medium's density from the drive"; fi
        # DGEN is now the DRIVE's identity, so `${CGEN:-$DGEN}` is exactly
        # ADR-0010's last rung (detected -> --generation -> row -> drive) and
        # `volume init` refuses any contradiction. The trailing LTO-6 literal
        # is gone: guessing a generation is what issue #178 removed.
        ask CGEN "generation of THIS cartridge" "${CGEN:-${DGEN:-}}"
        [ -n "$CGEN" ] || die "no cartridge generation given — pass --generation, or load the cartridge so its density can be read"
        run tc cartridge register --barcode "$CARTRIDGE" --generation "$CGEN" || die "cartridge register failed"
      fi
      run_capture "$INIT_OUT" tc volume init "$LABEL" --device "$DEVICE" --cartridge "$CARTRIDGE" || die "volume init failed"
    elif grep -q "ADR-0003" "$INIT_OUT"; then
      # The SEALED case, split out from the mismatch case below (issue #223).
      #
      # This branch used to share the one below, whose text named a sealed
      # volume as "the usual reason" and then prescribed --force. For a genuinely
      # sealed tape that is impossible: `decide_fresh_write_contact` refuses
      # `AlreadySealed` outright, and its own message says "--force cannot
      # override this". The script then ran init --force anyway, failed
      # identically, and died -- walking the operator into the same wall twice
      # and naming none of the real remedy.
      #
      # tapectl's refusal already says what to do, so this does not repeat it;
      # it stops rather than offering a flag that cannot work.
      explain <<'EOF'
volume init was refused by an ADR-0003 rule — almost always because the loaded cartridge already carries a SEALED volume. This is not a consent question and --force does not reach it: every ADR-0003 refusal says so itself, because a sealed volume is immutable and there is no append. Read the refusal above; it names the remedy for the case you hit. For a sealed cartridge that is: retire the volume on it, bulk-erase the physical tape, then `tapectl cartridge mark-erased` before writing to it again. If you did not expect a sealed tape here, `tapectl volume identify --device <dev>` will say what it actually holds.
EOF
      die "volume init refused by ADR-0003 and no flag overrides it. Act on the remedy in the refusal above (for a sealed cartridge: retire, erase, cartridge mark-erased), or load a different cartridge, then re-run scripts/first-run.sh --from 13"
    else
      explain <<'EOF'
volume init refused. The cartridge's File 0 identifies a DIFFERENT volume that is NOT sealed — a stale or foreign tape. tapectl will not overwrite one by accident. If this cartridge is genuinely expendable (a retired volume, a test tape), re-run init with --force; if you are not sure, stop and check `tapectl volume identify --device <dev>` first. (A SEALED tape is a different case and --force would not help there; this is not that.)
EOF
      [ "$AUTO" = 1 ] && die "volume init refused under --auto; not forcing"
      confirm_destructive "OVERWRITE whatever is on this cartridge with $LABEL" "$LABEL" || die "stopped"
      run_capture "$INIT_OUT" tc volume init "$LABEL" --device "$DEVICE" --force || die "volume init failed"
    fi
  fi
  # The cartridge this volume is now bound to, from init's own report lines
  # (src/volume/write.rs `report_binding`). Best-effort: a missing line costs
  # the operator a printed hint, never the write.
  CART_BOUND="$(sed -n 's/^cartridge \(.*\) auto-registered from MAM.*/\1/p; s/^volume "[^"]*" bound to cartridge \(.*\)$/\1/p' "$INIT_OUT" | head -1)"
  if [ -n "$CART_BOUND" ]; then
    ok "cartridge $CART_BOUND registered from its chip; that serial is its placeholder barcode"
    note "When you put a sticker on it:  tapectl cartridge relabel $CART_BOUND <your-barcode>"
  fi
  run tc volume write "$LABEL" --device "$DEVICE" || die "write did not seal — read the output; the catalog knows exactly why"
  run tc volume verify "$LABEL" --device "$DEVICE" --full || die "verify FAILED — do not trust this tape"
  ok "$LABEL sealed and verified"
  explain <<'EOF'
`audit` compares the catalog against policy. With one tape and a policy of two copies it will report copy_count violations — that is correct and advisory (exit 2 means "violations", not "broken"). The second copy is the next cartridge: `volume read-slices` + `volume write`, or just stage again.
EOF
  run tc audit || true
  if [ -n "$LOCATION" ]; then run tc volume move "$LABEL" --to "$LOCATION" || true; fi
  ask KIT_OUT "regenerate the heir kit into" "${KIT_OUT:-$HOME/heir-kit}"
  kit_prepare "$KIT_OUT"
  run tc key escrow-kit --out "$KIT_OUT" && note "Reprint $KIT_OUT/COVER.txt — the catalog inside it now knows this tape."
  kit_finish "$KIT_OUT"
fi
}

# ================================================================ step 14
hdr 14 "Next"
SUDO_PREFIX="$( [ "$SVC_MODE" = 1 ] && printf 'sudo -u %s -H ' "$SVC_USER" )"
cat <<EOF
   • Every tapectl command from now on: ${SUDO_PREFIX}tapectl <command>   (alias it). Its home: $EFFECTIVE_HOME
   • A restore destination must be writable by ${SVC_USER}; new unit trees need the same ACL grant as step 11.
   • Eject with: ${SUDO_PREFIX}mt -f ${DEVICE:-<device>} offline. Write the label on the cartridge.
     Then tell the catalog what the sticker says (ADR-0012 — the serial stays its identity):
     ${SUDO_PREFIX}tapectl cartridge relabel ${CART_BOUND:-<medium-serial>} <the-barcode-you-wrote>
   • Second copy, other location: load a fresh cartridge, \`tapectl volume read-slices --from ${LABEL:-<label>} --unit <unit>\`
     for each unit (or stage again), then \`volume write\` a new label and \`volume move --to <other shelf>\`.
   • \`tapectl audit\` weekly — contrib/systemd/ has a timer; its User=/HOME= default to the service user.
   • \`tapectl report summary\`, \`report fire-risk\`, \`catalog locate <unit>\` answer the operator questions.
   • Disaster recovery, the whole procedure: docs/operator-guide.md, "Disaster Recovery".
   • This script is resumable: scripts/first-run.sh --from N. Log: $LOG
EOF
ok "done"
