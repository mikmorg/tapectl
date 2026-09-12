#!/usr/bin/env bash
# first-run.sh — a guided, resumable walk from a bare machine to the first
# sealed production tape.
#
# What it does, in order (each step detects whether it is already done and
# offers to skip; `--from N` starts at a step; nothing touches a tape until
# step 12, and every tape-touching command is confirmed by name):
#
#    1  Rust toolchain (rustup; the repo pins 1.94.1 in rust-toolchain.toml)
#    2  runtime tools: dar >= 2.6, mt, sg3-utils, python3, lsscsi (age optional: heir path + rehearsal)
#    3  build tapectl (release) and optionally install it
#    4  the ungated test suite (no tape needed)
#    5  find the tape drive BY SERIAL and confirm it
#    6  initialise the tapectl home — mints the escrow identity (paper ready)
#    7  register the drive as a backend, by-id path
#    8  the Heir Kit — generate, print, seal, two failure domains
#    9  a shelf location for cartridges
#   10  tenants and units: who owns which paths, and how to register them
#   11  OPTIONAL rehearsal on a TEST cartridge (erases it — barcode required)
#   12  the first production tape: snapshot → stage → init → write → verify
#   13  what to do next
#
# Every `tapectl` flag here was taken from the binary's own --help on
# 2026-09-12; if a flag drifts, the failing command prints the real help.
#
# Testing this script against mhvtl (never the real drive) is done with:
#   scripts/first-run.sh --auto --home /tmp/fr-home --tapectl target/debug/tapectl \
#       --device /dev/tape/by-id/scsi-XYZZY_A1-nst --sg /dev/sg1 --label L6-TEST \
#       --tenant alice --unit-path /tmp/fr-src/photos --skip-build --skip-tests
set -euo pipefail

# ---------------------------------------------------------------- arguments
HOME_DIR=""            # tapectl home; empty = the real default (~/.tapectl)
FROM=0
TO=99
TAPECTL=""             # binary; resolved in step 0 unless given
AUTO=0                 # accept defaults for non-destructive prompts
DEVICE=""; SG=""; LABEL=""; OPERATOR=""; TENANT=""; UNIT_PATH=""; LOCATION=""; KIT_OUT=""; BARCODE=""
SKIP_BUILD=0; SKIP_TESTS=0
usage() {
  sed -n '2,25p' "$0" | sed 's/^# \{0,1\}//'
  cat <<EOF

Options:
  --home DIR        tapectl home (default ~/.tapectl). Use a temp dir to rehearse.
  --from N          start at step N (1-13)
  --to N            stop after step N (e.g. --to 8: everything up to and including the printed kit)
  --tapectl PATH    the tapectl binary to use (default: PATH, then target/release, then target/debug)
  --auto            take defaults for non-destructive prompts (for scripted rehearsal)
  --device PATH     tape device by-id path (skips the interactive pick in step 5)
  --sg PATH         matching /dev/sgN (derived from sysfs when omitted)
  --label L         first volume label (default asked; e.g. L6-0001)
  --operator NAME   operator name for init (default: \$USER)
  --tenant NAME     first tenant (default asked)
  --unit-path DIR   first unit directory (default asked)
  --location NAME   shelf location name (default asked; e.g. home-rack)
  --kit-out DIR     heir kit output dir (default ~/heir-kit)
  --barcode S       TEST cartridge barcode for the step-11 rehearsal (required for it under --auto)
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
    --label) LABEL="$2"; shift 2 ;;
    --operator) OPERATOR="$2"; shift 2 ;;
    --tenant) TENANT="$2"; shift 2 ;;
    --unit-path) UNIT_PATH="$2"; shift 2 ;;
    --location) LOCATION="$2"; shift 2 ;;
    --kit-out) KIT_OUT="$2"; shift 2 ;;
    --barcode) BARCODE="$2"; shift 2 ;;
    --skip-build) SKIP_BUILD=1; shift ;;
    --skip-tests) SKIP_TESTS=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

# ---------------------------------------------------------------- plumbing
REPO="$(cd "$(dirname "$0")/.." && pwd)"
if [ -t 1 ]; then B=$'\e[1m'; D=$'\e[2m'; R=$'\e[0m'; Y=$'\e[33m'; G=$'\e[32m'; RD=$'\e[31m'; else B=""; D=""; R=""; Y=""; G=""; RD=""; fi
EFFECTIVE_HOME="${HOME_DIR:-$HOME/.tapectl}"
LOG="$EFFECTIVE_HOME/first-run.log"
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
# run_nolog: for the one command whose output must never be written to disk
run_nolog() { printf '   %s$ %s%s\n' "$B" "$*" "$R"; log "\$ $* (output NOT logged)"; "$@"; }
tc() { if [ -n "$HOME_DIR" ]; then "$TAPECTL" --home "$HOME_DIR" "$@"; else "$TAPECTL" "$@"; fi; }
skip_if() { [ "$FROM" -gt "$STEP" ] && { note "skipped (--from $FROM)"; return 0; }; return 1; }
vercmp_ge() { [ "$(printf '%s\n%s\n' "$2" "$1" | sort -V | head -1)" = "$2" ]; }

# ================================================================ step 0
printf '%stapectl first run%s — repo %s\n' "$B" "$R" "$REPO"
printf 'home: %s   log: %s\n' "$EFFECTIVE_HOME" "$LOG"
if [ -z "$HOME_DIR" ] && [ -e "$HOME/.tapectl/tapectl.db" ] && [ "$FROM" -le 6 ]; then
  note "$HOME/.tapectl is already initialised. This script will detect that in step 6 and not re-init."
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
# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
WANT="$(sed -n 's/^channel *= *"\(.*\)"/\1/p' "$REPO/rust-toolchain.toml")"
( cd "$REPO" && run rustup show active-toolchain ) || true
HAVE="$(cd "$REPO" && rustc --version 2>/dev/null | awk '{print $2}')"
[ "$HAVE" = "$WANT" ] || die "rustc is $HAVE, the repo pins $WANT — run: cd $REPO && rustup show"
ok "rustc $HAVE, cargo $(cargo --version | awk '{print $2}')"
}

# ================================================================ step 2
hdr 2 "Runtime tools"
skip_if || {
explain <<'EOF'
tapectl shells out to `dar` for every archive (a hard dependency, >= 2.6, 2.7.20+ recommended) and to `mt` and the sg3-utils for drive control, health pages and the cartridge's MAM. `age` is not used by the binary itself — it uses the rage crate — but the on-tape RESTORE.sh, the heir path, needs it, and so does the rehearsal in step 11. `lsscsi` and `python3` are for step 5 and the lifecycle suite.
EOF
MISSING=()
for t in dar mt sg_read_attr sg_logs python3 lsscsi; do command -v "$t" >/dev/null 2>&1 || MISSING+=("$t"); done
if [ "${#MISSING[@]}" -gt 0 ]; then
  note "missing: ${MISSING[*]}"
  explain <<'EOF'
Debian/Ubuntu package names: dar, mt-st, sg3-utils, python3, lsscsi.
EOF
  if confirm "Run: sudo apt install dar mt-st sg3-utils python3 lsscsi ?"; then run sudo apt install -y dar mt-st sg3-utils python3 lsscsi; else die "install the missing tools and re-run with --from 2"; fi
fi
if ! command -v age >/dev/null 2>&1; then
  explain <<'EOF'
`age` (the CLI) is NOT needed by tapectl itself — the binary uses the rage crate. It is needed by RESTORE.sh, the heir path written to every tape, and therefore by the step-11 rehearsal, which runs that script off the tape. Debian ships an `age` package only from bookworm (12) onward; older releases have none, which is why apt cannot find it. Install it from the upstream release (a single static Go binary), or with `go install filippo.io/age/cmd/...@latest`, or skip it for now — step 11 will refuse to run without it, and nothing else here needs it.
EOF
  if confirm "Install age v1.3.2 from the upstream GitHub release into /usr/local/bin now?"; then
    AGE_TAG="v1.3.2"
    AGE_URL="https://github.com/FiloSottile/age/releases/download/${AGE_TAG}/age-${AGE_TAG}-linux-amd64.tar.gz"
    TMPD="$(mktemp -d)"
    run bash -c "curl -fsSL '$AGE_URL' | tar -xz -C '$TMPD'" || die "download failed — check the tag at https://github.com/FiloSottile/age/releases"
    run sudo install -m 0755 "$TMPD/age/age" "$TMPD/age/age-keygen" /usr/local/bin/
    rm -rf "$TMPD"
    run age --version
  else note "continuing without age — the step-11 rehearsal will be unavailable until it is installed"; fi
fi
DARV="$(dar --version 2>&1 | sed -n 's/.*dar version \([0-9.]*\).*/\1/p' | head -1)"
[ -n "$DARV" ] || DARV="$(dar --version 2>&1 | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1)"
[ -n "$DARV" ] || die "could not read dar's version (dar --version)"
vercmp_ge "$DARV" "2.6.0" || die "dar $DARV is too old; tapectl needs >= 2.6"
vercmp_ge "$DARV" "2.7.20" || note "dar $DARV works; 2.7.20+ is recommended (bookworm ships 2.7.x)"
ok "dar $DARV, mt, sg3-utils, python3, lsscsi present$(command -v age >/dev/null 2>&1 && echo ", age present" || echo "; age ABSENT (heir-path script and rehearsal only)")"
}

# ================================================================ step 3
hdr 3 "Build tapectl"
skip_if || {
if [ "$SKIP_BUILD" = 1 ]; then note "--skip-build"; else
explain <<'EOF'
Everything validated on mhvtl and on the real drive so far ran the debug binary — it is the proven artifact. A release build is the same source with optimisation on: markedly faster at the sha256 hashing and age encryption a multi-hundred-gigabyte write is made of. Build release for production, then run one real-drive rehearsal on it (step 11) before you trust it with data, because a different binary is a different artifact.
EOF
if confirm "Build the release binary now (cargo build --release; a few minutes)?"; then
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
if confirm "Run cargo test now?"; then ( cd "$REPO" && run cargo test ) || die "the suite is red on this machine — stop here and look"; ok "suite green"; fi
fi
}

# ================================================================ step 5
hdr 5 "Find the tape drive — by serial, never by number"
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
run mt -f "$DEVICE" status || note "mt status failed — is a cartridge loaded, and is the device readable?"
ok "drive: $DEVICE ($NST), sg: $SG"
}

# ================================================================ step 6
hdr 6 "Initialise the tapectl home"
skip_if || {
if [ -e "$EFFECTIVE_HOME/tapectl.db" ]; then
  ok "$EFFECTIVE_HOME is already initialised — not re-running init"
else
explain <<'EOF'
`tapectl init` creates the database, config and the operator tenant — and mints the permanent ESCROW IDENTITY (ADR-0005): the one key that is a recipient of every tape and is never rotated. Its SECRET half is printed ONCE, to your terminal, and stored nowhere on this machine. Have paper ready; write it down before you do anything else. It later goes on the Heir Kit's cover sheet (step 8), which is how an heir — or you, on a rebuilt machine — gets back in.

If you are REBUILDING a machine and already hold the original escrow key, do NOT let init mint a new one: answer with the original public key below and it is adopted instead (no command can replace a registered escrow identity later).

This step's output is deliberately NOT written to the log.
EOF
  ask OPERATOR "operator name" "${OPERATOR:-$USER}"
  ADOPT=""
  if [ "$AUTO" != 1 ]; then ask ADOPT "existing escrow PUBLIC key to adopt (age1…, or a .pub path) — leave empty to mint a new one" ""; fi
  note "PAPER READY? The escrow secret appears exactly once, next."
  confirm "Run init now?" || die "stopped before init"
  if [ -n "$ADOPT" ]; then run_nolog tc init --operator "$OPERATOR" --escrow-public-key "$ADOPT"
  else run_nolog tc init --operator "$OPERATOR"; fi
  note "Written down? It will not be shown again."
fi
run tc config check || note "config check reported something — read it; advisory, exit code above"
run tc db fsck || true
explain <<'EOF'
STAGING SPACE. `stage create` writes every encrypted slice of a unit to the staging directory before anything goes to tape, so it needs room for the largest batch you will write in one session — up to a full cartridge (2.5 TB for LTO-6) if you fill tapes in one go. init writes a default path into config.toml that may not exist, or may not be yours to write to. Put staging on a filesystem with the space, owned by the user that runs tapectl.
EOF
CFG="$EFFECTIVE_HOME/config.toml"
SD="$(sed -n '/^\[staging\]/,/^\[/{s/^directory *= *"\(.*\)"/\1/p}' "$CFG" | head -1)"
SD_DEF="$SD"
if [ -z "$SD" ] || [ ! -d "$SD" ] || [ ! -w "$SD" ]; then
  note "staging.directory is '${SD:-<unset>}' — $( [ -z "$SD" ] && echo unset || { [ -d "$SD" ] && echo "not writable by $USER" || echo "does not exist"; } )"
  [ "$AUTO" = 1 ] && SD_DEF="$EFFECTIVE_HOME/staging"
fi
ask SD_NEW "staging directory (needs space for a full tape's slices)" "$SD_DEF"
if [ "$SD_NEW" != "$SD" ]; then
  sed -i "/^\[staging\]/,/^\[/{s|^directory *= *\".*\"|directory = \"$SD_NEW\"|}" "$CFG"
  grep -q "^directory = \"$SD_NEW\"" "$CFG" || die "could not rewrite staging.directory in $CFG — edit it by hand"
fi
mkdir -p "$SD_NEW" 2>/dev/null || sudo mkdir -p "$SD_NEW"
[ -w "$SD_NEW" ] || { note "$SD_NEW is not writable by $USER"; confirm "sudo chown $USER $SD_NEW ?" && run sudo chown "$USER" "$SD_NEW"; }
[ -w "$SD_NEW" ] || die "staging directory $SD_NEW is not writable"
run df -h "$SD_NEW"
ok "staging at $SD_NEW"
ok "home ready at $EFFECTIVE_HOME"
}

# ================================================================ step 7
hdr 7 "Register the drive as a backend"
skip_if || {
CFG="$EFFECTIVE_HOME/config.toml"
if grep -q '^\[\[backends.lto\]\]' "$CFG" 2>/dev/null; then ok "a [[backends.lto]] entry already exists in $CFG"; run grep -A5 '^\[\[backends.lto\]\]' "$CFG" || true
else
explain <<'EOF'
`backend add` appends a [[backends.lto]] table to config.toml with the by-id tape path and the sg node, so volume write knows where to write and the health checks know where to ask. Media type and nominal capacity default to LTO-6 / 2.5TB; change them for another generation.
EOF
  [ -n "$DEVICE" ] || die "no device chosen — run with --from 5"
  ask BNAME "backend name" "lto6"
  ask MTYPE "media type" "LTO-6"
  run tc backend add --name "$BNAME" --device-tape "$DEVICE" --device-sg "$SG" --media-type "$MTYPE"
  run tc config check || true
fi
}

# ================================================================ step 8
hdr 8 "The Heir Kit"
skip_if || {
explain <<'EOF'
`key escrow-kit` writes three files: COVER.txt (the escrow key in retypable Bech32 plus instructions — the decades-scale artifact, readable with cat), escrow-kit.html (the same with a QR, for printing from a browser) and catalog.db.age (the whole catalog encrypted to the escrow key). Yours to do afterwards: PRINT COVER.txt, seal it in tamper-evident envelopes, and keep copies in at least two independent failure domains. Generate it now, before any data — `audit` will remind you (escrow_kit_stale, a warning) after every write, which is the cue to regenerate with the real catalog.
EOF
ask KIT_OUT "kit output directory" "${KIT_OUT:-$HOME/heir-kit}"
if [ -f "$KIT_OUT/COVER.txt" ] && ! confirm "A kit already exists in $KIT_OUT — regenerate?"; then ok "keeping the existing kit"; else
  run tc key escrow-kit --out "$KIT_OUT"
fi
note "Print: $KIT_OUT/COVER.txt   (and/or open $KIT_OUT/escrow-kit.html and print)"
note "Seal in tamper-evident envelopes; two failure domains; refresh after each write session."
}

# ================================================================ step 9
hdr 9 "A shelf location"
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

# ================================================================ step 10
hdr 10 "Tenants and units — who owns which paths"
skip_if || {
explain <<'EOF'
The model, in three words: TENANT, UNIT, COLLECTION.

A TENANT is a person or trust domain with their own age keys. Everything staged for a tenant is encrypted to that tenant's key (plus yours as operator, plus escrow), and NOTHING about it is on the tape in plaintext — not names, not filenames. A tenant restores their own data with only their key and RESTORE.sh; they cannot read another tenant's.

A UNIT is one directory archived as one entity: a photo year, a project, a show season. It gets a .tapectl-unit.toml (a uuid, so renames are survivable) and is what you snapshot, stage and restore. Keep units at the size you would want to restore in one go.

A COLLECTION is a folder-per-unit source root bound to ONE tenant: `collection sync` registers every child folder at a fixed depth as a unit — the right tool when a tenant's data is already "one folder per thing". Configure it as a [[collections]] table in config.toml.

Layout that works: one top directory per tenant (/data/alice, /data/bob), units or a collection root beneath. A path belongs to exactly one tenant.
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
    ask UNAME "unit name" "$TENANT/$(basename "$UP")"
    run tc unit init --tenant "$TENANT" --name "$UNAME" "$UP" || note "(unit init failed — see above)"
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

`collection status` shows what is pending; `collection plan` fills tape-sized batches; `collection run` stages and writes them.
EOF
}

# ================================================================ step 11
hdr 11 "OPTIONAL rehearsal on a TEST cartridge (erases it)"
skip_if || {
explain <<'EOF'
Before real data, the lifecycle suite can run a whole simulated first year — write, verify, every restore path including the heir script off the tape — on a cartridge you are willing to lose. It ERASES that cartridge. It needs its barcode typed exactly, cross-checked against the cartridge's MAM. This is the step that proves the drive, the host's st driver and the binary you built agree.
EOF
if ! command -v age >/dev/null 2>&1; then note "skipped: the rehearsal runs RESTORE.sh off the tape, which needs the age CLI (step 2 explains how to install it)"
elif [ "$AUTO" = 1 ] && [ -z "$BARCODE" ]; then note "skipped under --auto: no --barcode given (erasing a cartridge is never a default)"
elif confirm "Run the first-year rehearsal on a TEST cartridge now?"; then
  if [ -n "$SG" ]; then run sudo sg_read_attr "$SG" | grep -iE "Medium serial|manufacturer" || true; fi
  ask BARCODE "barcode/serial of the TEST cartridge in the drive (it will be erased)" "$BARCODE"
  [ -n "$BARCODE" ] || die "no barcode given"
  if confirm_destructive "ERASE $BARCODE and run the rehearsal" "$BARCODE"; then
    ( cd "$REPO" && run bash scripts/lifecycle-suite.sh --scenario first-year --device "$DEVICE" --erase short --single-cartridge --i-will-lose-the-cartridge "$BARCODE" ) || die "rehearsal RED — do not write real data until this is understood"
    ok "rehearsal green"
    note "Eject the test cartridge (mt -f $DEVICE offline) and load the production one before step 12."
  fi
else note "skipped"; fi
}

# ================================================================ step 12
hdr 12 "The first production tape"
skip_if || {
NV="$(tc catalog stats --json 2>/dev/null | python3 -c 'import json,sys
try: print(json.load(sys.stdin).get("volumes",0))
except Exception: print(0)' 2>/dev/null || echo 0)"
[ "$NV" != 0 ] && note "$NV volume(s) already in the catalog." && { confirm "Write another tape now?" || { ok "nothing to do"; FROM=13; }; }
if [ "$FROM" -le 12 ]; then
explain <<'EOF'
The pipeline is three phases: `snapshot create` walks the unit and records what exists; `stage create` runs dar, hashes, encrypts to every recipient and writes slices to staging; `volume write` plans the whole tape first — every file, position and size — then writes it in one session and reads the seal back. A sealed volume is immutable: there is no append. Then `volume verify --full` reads every byte back against the front index, which turns the tape's claims into checked evidence.

Label convention: something you can write on the cartridge, e.g. L6-0001.
EOF
  [ -n "$DEVICE" ] || die "no device — run with --from 5"
  run mt -f "$DEVICE" status || true
  if mt -f "$DEVICE" status 2>/dev/null | grep -q DR_OPEN; then die "no cartridge loaded in $DEVICE"; fi
  ask LABEL "volume label" "${LABEL:-L6-0001}"
  if [ -n "$SG" ] && confirm "Register the cartridge's barcode in the catalog (reads MAM)?"; then
    MSER="$(sudo sg_read_attr "$SG" 2>/dev/null | awk -F: '/Medium serial number/{gsub(/ /,"",$2); print $2}')"
    ask BARCODE "barcode" "${MSER:-}"
    if [ -n "$BARCODE" ]; then
      if tc cartridge list --json 2>/dev/null | grep -q "\"$BARCODE\""; then ok "cartridge $BARCODE already registered"
      else run tc cartridge register --barcode "$BARCODE" --media-type "${MTYPE:-LTO-6}"; fi
    fi
  fi
  UNITS="$(tc unit list --json 2>/dev/null | python3 -c 'import json,sys
try:
  d=json.load(sys.stdin); rows=d if isinstance(d,list) else d.get("units",[]); print("\n".join(r.get("name","") for r in rows))
except Exception: pass' 2>/dev/null || true)"
  [ -n "$UNITS" ] || die "no units registered — run with --from 10"
  printf '   units:\n%s\n' "$(printf '%s\n' "$UNITS" | sed 's/^/     /')"
  confirm "Snapshot and stage EVERY unit above?" || die "stopped"
  while IFS= read -r u; do
    [ -z "$u" ] && continue
    run tc snapshot create "$u" || die "snapshot failed for $u"
    run tc stage create "$u" || die "stage failed for $u"
  done <<< "$UNITS"
  run tc staging status || true
  confirm_destructive "WRITE volume $LABEL to the cartridge in $DEVICE (the cartridge's current contents are overwritten)" "$LABEL" || die "stopped before writing"
  if ! run tc volume init "$LABEL" --device "$DEVICE"; then
    explain <<'EOF'
volume init refused. The usual reason: the cartridge's File 0 already identifies a DIFFERENT sealed volume, and sealed volumes are immutable (ADR-0003) — tapectl will not overwrite one by accident. If this cartridge is genuinely expendable (a retired volume, a test tape), re-run init with --force; if you are not sure, stop and check `tapectl volume identify --device <dev>` first.
EOF
    [ "$AUTO" = 1 ] && die "volume init refused under --auto; not forcing"
    confirm_destructive "OVERWRITE whatever is on this cartridge with $LABEL" "$LABEL" || die "stopped"
    run tc volume init "$LABEL" --device "$DEVICE" --force || die "volume init failed"
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
  run tc key escrow-kit --out "$KIT_OUT" && note "Reprint $KIT_OUT/COVER.txt — the catalog inside it now knows this tape."
fi
}

# ================================================================ step 13
hdr 13 "Next"
cat <<EOF
   • Eject with: mt -f ${DEVICE:-<device>} offline. Write the label on the cartridge.
   • Second copy, other location: load a fresh cartridge, \`tapectl volume read-slices --from ${LABEL:-<label>} --unit <unit>\`
     for each unit (or stage again), then \`volume write\` a new label and \`volume move --to <other shelf>\`.
   • \`tapectl audit\` weekly — contrib/systemd/ has a timer; set User= and Environment=HOME= explicitly.
   • \`tapectl report summary\`, \`report fire-risk\`, \`catalog locate <unit>\` answer the operator questions.
   • Disaster recovery, the whole procedure: docs/operator-guide.md, "Disaster Recovery".
   • This script is resumable: scripts/first-run.sh --from N. Log: $LOG
EOF
ok "done"
