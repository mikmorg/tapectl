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
#    7  initialise the tapectl home — mints the escrow identity (paper ready;
#       under --auto only with --mint-escrow), or adopts ESCROW_PUBLIC_KEY;
#       with ADOPT_KEYS_FROM, the operator adopts its existing key instead of minting
#    8  register the drive as a backend, by-id path, generation from the
#       drive's INQUIRY product id (never from the loaded cartridge)
#    9  the Heir Kit — generate, print, seal, two failure domains
#   10  a shelf location for cartridges
#   11  tenants and units: who owns which paths; the service user is granted read by ACL;
#       with ADOPT_KEYS_FROM, each profile tenant adopts its existing key
#   12  rehearsal on a TEST cartridge with the INSTALLED binary (erases it —
#       serial required; runs as YOU). Ruled required before the first write
#       (ADR-0012, 2026-09-23): a different binary is a different artifact.
#       It stops tapectl-drive-poll.timer for the rehearsal and starts it again.
#   13  the first production tape: snapshot → stage → init → write → verify
#   14  the systemd timers (weekly audit, daily catalog backup to a second
#       disk) and the /usr/local/bin/tapectl-op wrapper, via
#       scripts/install-systemd.sh — then what to do next
#
#
# --upgrade upgrades an installed host in place instead (ADR-0012 item 192):
# preflight, a catalog backup with the old binary, the old binary kept, the new
# one built and installed, then steps 14, 12 and 9 again and a tidy. Its own
# steps are listed in the upgrade section below and in --help.
#
# Steps 1-4 run as you (they need your toolchain and sudo). Steps 5-11 and 13
# run tapectl as the service user through one seam, `as_svc`; step 12 runs as
# you (the lifecycle suite needs your login to open the drive) and runs the
# installed binary through TAPECTL_BIN, so what it proves is what step 13
# uses. `--no-service-user` turns the seam off and runs everything as you.
#
# Every `tapectl` flag here was checked against the binary's own --help and
# the source on 2026-09-24; if a flag drifts, the failing command prints the
# real help.
#
# docs/install.md is the runbook for this script: what each step creates on
# the host, how to resume, re-run, move and uninstall.
#
# Testing this script against mhvtl (never the real drive) is done with a binary
# the service user can execute (your home is not traversable by it). Under
# --home, step 14 only PRINTS the installer's plan (--dry-run): a throwaway
# home never installs host-wide timers. The recipe is in the profile that
# drives it: contrib/hosts/vm-desk1-mhvtl.profile (a production host has its
# own, e.g. contrib/hosts/home2.profile).
set -euo pipefail

# ---------------------------------------------------------------- arguments
# The variables below are the PROFILE VARIABLES: a --profile file may set them,
# and flags given on the command line still win. The state only a flag sets
# (--auto, --from, --to, --no-service-user, --mint-escrow, and which values came
# from a flag) is declared AFTER the profile is sourced, so a profile that sets
# one is refused by name like any other unknown variable (see PROFILE_VARS).
HOME_DIR=""            # tapectl home; empty = the real default (the service user's ~/.tapectl)
TAPECTL=""             # binary; resolved in step 0 unless given
DEVICE=""; SG=""; LABEL=""; OPERATOR=""; TENANT=""; UNIT_PATH=""; LOCATION=""; KIT_OUT=""; TEST_BARCODE=""; DGEN=""
SKIP_BUILD=0; SKIP_TESTS=0
SVC_USER="tapectl"
# Step 14: where the daily `db backup` copies go. Empty = not chosen: step 14 then
# passes no --backup-dir, and install-systemd.sh takes its default on a first
# install but refuses to move a directory an installed unit already names (#502).
BACKUP_DIR=""
# Host-specific defaults. A --profile file (contrib/hosts/*.profile) sets these;
# flags given on the command line still win. Empty = the built-in behaviour.
WORK_DIR=""            # build lock + step-12 rehearsal workspace; default /scratch if present, else ~/.cache/tapectl
STAGING_DIR=""         # step 7: staging.directory default
STAGING_JOBS=""        # step 7: [staging] jobs, units staged at once (#368); empty = leave the config's value
VERIFY_INTERVAL_DAYS="" # step 7: [defaults] verify_interval_days, the full-readback clock audit keeps per unit
                       # and per volume (#453, #454; ADR-0012 items 66-67); empty = leave the config's value
# CONTENDER_PROCESSES=(…) step 7: [host_check] contender_processes, the WHOLE list (the built-in one is
#                        cargo rustc docker Runner.Worker; repeat it when adding). Unset = leave the config's value.
DAR_BIN=""             # step 2/4/7: the dar to check, test with and write into [dar] binary
SVC_HOME_WANT=""       # step 5: home for a NEW service user (default /var/lib/<user>)
LOCATIONS=()           # step 10: "name|description" — the first is the shelf step 13 moves tape 1 to
TENANTS=()             # step 11: "name|description" — non-operator tenants to create
COLLECTIONS=()         # step 11: "name|root|tenant|unit_depth" — [[collections]] to configure and sync
OPS_GROUP=""           # step 7: a group that may read the session logs (`tapectl status`), e.g. tapectl-ops (issue #393); empty = none
TAPE_NICE=-10          # step 13: the nice value this run raises ITSELF to, once, with sudo renice, so the tape commands
                       # it starts inherit it (issue #432; see tape_raise_setup). Step 14 bakes the same value into
                       # tapectl-op and the drive-poll unit, which raise a tape command as root and then drop to the
                       # service user (ADR-0012 2026-10-15 item 58). Empty = never raise.
TAPE_IONICE="-c2 -n0"  # the ionice arguments that go with it (best-effort, level 0). Empty = no I/O priority change.
ESCROW_PUBLIC_KEY=""   # step 7: the ORIGINAL escrow public key (age1…, or a .pub path) for init to adopt instead of
                       # minting one — a rebuilt or fresh home that keeps the Heir Kit (ADR-0005, #139). Empty = ask
                       # (under --auto: refuse, unless --mint-escrow is on the command line; ADR-0012 item 188).
ERASE_LABEL=""         # step 13: set by --erase-label ONLY, never by a profile (#469): the File 0 label (or, for a
                       # tape tapectl did not write, the chip serial) of a SEALED tape step 13 may `cartridge erase`
                       # under --auto when `volume init` refuses it. Empty = ask interactively, and never erase
                       # under --auto (a default is never consent to erase).
ADOPT_KEYS_FROM=""     # steps 7 and 11: a directory of <tenant>.key age identity files (the SECRETS, copied out of an
                       # old home: docs/keys-and-recovery.md) that the operator and the profile's tenants ADOPT instead
                       # of minting new keys (issue #424). Every one of them must have its file. Empty = mint, as usual.
# CONTENDER_UNITS is left UNSET here on purpose: unset means "this VM's homorg
# timers" (the historical default, step 13); a profile that declares it — even
# as () — replaces that list.
# The documented set: every variable a profile may set. The script reads each of
# them; a profile that sets any other name (a typo such as STAGING_JOB) is refused
# by name before anything runs (#506). ERASE_LABEL is not in it, and is not
# refused either: a profile's value is only reported as not used (#469, step 13).
PROFILE_VARS=(
  HOME_DIR TAPECTL SKIP_BUILD SKIP_TESTS SVC_USER SVC_HOME_WANT WORK_DIR DAR_BIN
  DEVICE SG DGEN OPERATOR ESCROW_PUBLIC_KEY ADOPT_KEYS_FROM STAGING_DIR STAGING_JOBS
  VERIFY_INTERVAL_DAYS CONTENDER_PROCESSES OPS_GROUP KIT_OUT LOCATIONS LOCATION
  TENANTS TENANT UNIT_PATH COLLECTIONS TEST_BARCODE LABEL CONTENDER_UNITS
  TAPE_NICE TAPE_IONICE BACKUP_DIR
)
# The profile is sourced BEFORE the flags are parsed, so every flag overrides it.
# --profile none runs with no profile at all (ADR-0012 item 187).
PROFILE=""; PROFILE_GIVEN=0; _help=0
for ((i = 1; i <= $#; i++)); do
  case "${!i}" in
    --profile) j=$((i + 1)); PROFILE="${!j:-}"; PROFILE_GIVEN=1 ;;
    -h | --help) _help=1 ;;
  esac
done
[ "$PROFILE_GIVEN" = 0 ] || [ -n "$PROFILE" ] || { echo "--profile needs a file (or the word none)" >&2; exit 2; }
if [ "$PROFILE_GIVEN" = 0 ]; then _logged_profile="unset"
elif [ "$PROFILE" = none ]; then _logged_profile="none"
elif [ -r "$PROFILE" ]; then PROFILE="$(realpath "$PROFILE")"; _logged_profile="$PROFILE"
else _logged_profile="$PROFILE"; fi
# Item 187: every run's first line in first-run.log says how it was invoked (the
# arguments carry no secret: keys are passed as paths or PUBLIC keys), with
# which profile, from which commit. Before the profile is read, so a run it
# refuses is recorded too.
if [ "$_help" = 0 ]; then
  _state="${XDG_STATE_HOME:-$HOME/.local/state}/tapectl"
  mkdir -p "$_state" || { echo "cannot create $_state (first-run.log's directory)" >&2; exit 2; }
  _args=""; [ "$#" -eq 0 ] || _args="$(printf ' %q' "$@")"
  printf '%s first-run.sh%s profile=%s git=%s\n' "$(date -u +%FT%TZ)" "$_args" "$_logged_profile" \
    "$(git -C "$(dirname "$0")/.." describe --always --dirty 2>/dev/null || echo unknown)" >>"$_state/first-run.log"
fi
if [ "$PROFILE_GIVEN" = 1 ] && [ "$PROFILE" != none ]; then
  [ -r "$PROFILE" ] || { echo "--profile $PROFILE is not readable" >&2; exit 2; }
  _pre_vars="$(compgen -v | LC_ALL=C sort)"
  # shellcheck disable=SC1090
  . "$PROFILE"
  # Every name the profile brought in must be a profile variable. Names that
  # existed before it (the environment, the defaults above) are not its to
  # check; a _-prefixed name is a profile's own helper (home2.profile's
  # _local_profile), and BASH_* are the shell's.
  _bad=()
  while IFS= read -r _n; do
    case "$_n" in _* | BASH_*) continue ;; esac
    case " ${PROFILE_VARS[*]} " in *" $_n "*) continue ;; esac
    _bad+=("$_n")
  done < <(LC_ALL=C comm -13 <(printf '%s\n' "$_pre_vars") <(compgen -v | LC_ALL=C sort))
  if [ "${#_bad[@]}" -gt 0 ]; then
    for _n in "${_bad[@]}"; do
      case "$_n" in
        MINT_ESCROW) echo "--profile $PROFILE sets MINT_ESCROW: minting an escrow identity is consent, given on the command line only (--mint-escrow), never by a profile (ADR-0012 item 188)" >&2 ;;
        *) echo "--profile $PROFILE sets $_n, which is not a profile variable: a typo, or a setting this script does not read" >&2 ;;
      esac
    done
    echo "the profile variables are: ${PROFILE_VARS[*]}" >&2
    exit 2
  fi
  unset _pre_vars _bad _n
fi
# What only a flag sets, declared after the profile so that a profile cannot.
FROM=0
TO=99
UPGRADE=0              # --upgrade: the in-place upgrade (ADR-0012 item 192), its own steps 1-13
AUTO=0                 # accept defaults for non-destructive prompts
SVC_MODE=1             # --no-service-user → run tapectl as yourself
LABEL_FROM_FLAG=0; BARCODE_FROM_FLAG=0
MINT_ESCROW=0          # --mint-escrow: under --auto, step 7 may mint a new escrow identity (ADR-0012 item 188)
BACKUP_DIR_DEFAULT="/var/backups/tapectl"   # install-systemd.sh's, for the text below
# ERASE_LABEL is consent to erase a cartridge holding a SEALED volume, so it is
# taken from --erase-label only (#469): a word left in a profile is standing
# consent, and in a run whose catalog is gone (a rebuild, a DR install) it would
# erase that cartridge the moment it is loaded. A profile's value is kept only
# to say why it is not used.
PROFILE_ERASE_LABEL="${ERASE_LABEL:-}"; ERASE_LABEL=""
# --upgrade runs three of the steps below as this script (--from N --to N, see
# upgrade_child). Those runs get every argument this one got except the ones the
# upgrade sets for them: the profile (passed by its resolved path), the step
# range, the binary and the kit directory.
FWD_ARGS=(); _drop=0
for _a in "$@"; do
  if [ "$_drop" = 1 ]; then _drop=0; continue; fi
  case "$_a" in
    --upgrade | --skip-build | --skip-tests) ;;
    --profile | --from | --to | --tapectl | --kit-out) _drop=1 ;;
    *) FWD_ARGS+=("$_a") ;;
  esac
done
unset _a _drop
usage() {
  # The step list ends at the "then what to do next" line; keep the range on it.
  sed -n '2,/then what to do next/p' "$0" | sed 's/^# \{0,1\}//'
  cat <<EOF

Options:
  --home DIR        tapectl home (default: the service user's ~/.tapectl, /var/lib/tapectl/.tapectl
                    for a new account; with --no-service-user, your own ~/.tapectl). Use a temp dir
                    to rehearse.
  --from N          start at step N (0-14)
  --to N            stop after step N (0-14; e.g. --to 9: everything up to and including the printed kit)
  --upgrade         upgrade this installed host in place to the checkout's release tag (ADR-0012
                    item 192; docs/install.md §10), in steps of its own:
                       1 preflight, always run: no tapectl running, HEAD an annotated vX.Y.Z tag
                         on a clean tree, newer than the installed tapectl
                       2 back up the catalog with the OLD binary, and check the copy
                       3 keep the old binary beside the new one, as tapectl-<old>
                       4 build the release binary and install it (with --skip-build, install the
                         --tapectl PATH instead, which must be a build of the tag)
                       5 config check: the new binary's first open applies the migrations
                       6 step 14 again (units, wrapper, timers)
                       7 pause at each newer release's "Upgrading" notes (docs/releases/)
                       8 look: status, audit, report failed-stages
                       9 step 12's rehearsal (a cartridge the catalog knows is refused)
                      10 step 9: the Heir Kit, its copy of tapectl checked against the install
                      11 start tapectl-backup.service once
                      12 tidy: list what earlier runs left; remove a group only on a typed word
                      13 print the rollback
                    Run it again to resume: a step already done is checked, not done twice.
                    Under --upgrade, --from N and --to N take these numbers (1-13)
  --tapectl PATH    the tapectl binary to use (default: PATH, then target/release, then target/debug)
  --user NAME       the service user that owns and runs tapectl (default: tapectl; created in step 5)
  --no-service-user run tapectl as yourself instead — no account, no ACLs, your ~/.tapectl
  --auto            take defaults for non-destructive prompts (for scripted rehearsal)
  --device PATH     tape device by-id path (skips the interactive pick in step 6)
  --sg PATH         matching /dev/sgN (derived from sysfs when omitted)
  --generation GEN  the generation this DRIVE natively writes (default asked; e.g. LTO-6)
  --label L         first volume label (default asked; e.g. L6-0001)
  --operator NAME   operator name for init (default: \$USER)
  --tenant NAME     first tenant (default asked)
  --unit-path DIR   first unit directory (default asked)
  --location NAME   shelf location name (default asked; e.g. home-rack)
  --kit-out DIR     heir kit output dir (default ~/heir-kit)
  --barcode S       the step-12 TEST cartridge's MEDIUM SERIAL (what its chip reports; sg_read_attr
                    prints it) — required for the rehearsal under --auto
  --backup-dir DIR  where step 14's daily catalog backup writes (default $BACKUP_DIR_DEFAULT;
                    put it on a SECOND disk). Over installed timers that back up elsewhere,
                    step 14 refuses unless this or the profile names the directory
  --profile FILE    host defaults (contrib/hosts/<host>.profile), shell assignments that flags
                    override. FILE may set only these variables (any other name is refused):
$(printf '%s\n' "${PROFILE_VARS[*]}" | fold -s -w 72 | sed 's/ *$//; s/^/                      /')
                    The host records FILE (\${XDG_STATE_HOME:-~/.local/state}/tapectl/profile), and a
                    later run without --profile refuses and names it (ADR-0012 item 187)
  --profile none    run with no profile: the built-in defaults, and the recorded profile is not
                    used (it stays recorded)
  --work-dir DIR    build lock and step-12 rehearsal workspace (default /scratch if it exists,
                    else ~/.cache/tapectl)
  --staging DIR     default for staging.directory in step 7
  --tape-nice N     nice value step 13 raises this run to (one sudo renice; the tape commands inherit it) and
                    step 14 gives tapectl-op and the drive-poll unit (default -10; empty = never raise)
  --tape-ionice ARGS  ionice arguments for them (default '-c2 -n0'; empty = no I/O priority change)
  --adopt-keys-from DIR
                    steps 7 and 11 adopt DIR/<name>.key (an age identity file) as the operator's and
                    each profile tenant's key instead of minting one (docs/keys-and-recovery.md)
  --erase-label L   step 13: when \`volume init\` refuses a tape holding a SEALED volume, \`cartridge erase\`
                    it under --auto only if L is the label its File 0 carries (a flag only: a profile's
                    ERASE_LABEL is ignored, because a word in a file would be standing consent)
  --escrow-public-key KEY
                    step 7 adopts KEY (age1… or a .pub path) as the escrow identity instead of
                    minting one, so an existing Heir Kit stays valid (ESCROW_PUBLIC_KEY in a profile)
  --mint-escrow     step 7 under --auto, on a fresh home with no ESCROW_PUBLIC_KEY: mint a NEW escrow
                    identity. Without it --auto refuses there: no command replaces an escrow identity
                    (ADR-0005). A flag only, never a profile: it is consent (ADR-0012 item 188)
  --dar PATH       the dar binary: checked in step 2, first on PATH for the step-4 suite,
                    written to [dar] binary in step 7 (default: dar on PATH)
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
    --upgrade) UPGRADE=1; shift ;;
    --tapectl) TAPECTL="$2"; shift 2 ;;
    --auto) AUTO=1; shift ;;
    --device) DEVICE="$2"; shift 2 ;;
    --sg) SG="$2"; shift 2 ;;
    --generation) DGEN="$2"; shift 2 ;;
    --label) LABEL="$2"; LABEL_FROM_FLAG=1; shift 2 ;;
    --operator) OPERATOR="$2"; shift 2 ;;
    --tenant) TENANT="$2"; shift 2 ;;
    --unit-path) UNIT_PATH="$2"; shift 2 ;;
    --location) LOCATION="$2"; shift 2 ;;
    --kit-out) KIT_OUT="$2"; shift 2 ;;
    --barcode) TEST_BARCODE="$2"; BARCODE_FROM_FLAG=1; shift 2 ;;
    --backup-dir) BACKUP_DIR="$2"; shift 2 ;;
    --skip-build) SKIP_BUILD=1; shift ;;
    --skip-tests) SKIP_TESTS=1; shift ;;
    --user) SVC_USER="$2"; shift 2 ;;
    --no-service-user) SVC_MODE=0; shift ;;
    --profile) shift 2 ;;   # already sourced above
    --work-dir) WORK_DIR="$2"; shift 2 ;;
    --staging) STAGING_DIR="$2"; shift 2 ;;
    --tape-nice) TAPE_NICE="$2"; shift 2 ;;
    --tape-ionice) TAPE_IONICE="$2"; shift 2 ;;
    --dar) DAR_BIN="$2"; shift 2 ;;
    --adopt-keys-from) ADOPT_KEYS_FROM="$2"; shift 2 ;;
    --escrow-public-key) ESCROW_PUBLIC_KEY="$2"; shift 2 ;;
    --erase-label) ERASE_LABEL="$2"; shift 2 ;;
    --mint-escrow) MINT_ESCROW=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

case "$TAPE_NICE" in
  '' | [0-9] | 1[0-9] | -[0-9] | -1[0-9] | -20) ;;
  *) echo "TAPE_NICE / --tape-nice must be an integer from -20 to 19, or empty to never raise; got '$TAPE_NICE'" >&2; exit 2 ;;
esac
case "$TAPE_IONICE" in
  *[!-0-9\ cn]*) echo "TAPE_IONICE / --tape-ionice takes only ionice's -c CLASS and -n LEVEL (e.g. '-c2 -n0'); got '$TAPE_IONICE'" >&2; exit 2 ;;
esac
# The steps are 0-14 (#506), and --upgrade's are 1-13 (its step 1 always runs,
# so FROM's default, 0, is its 1). TO's own default, 99, means "to the end".
for _flag in from to; do
  if [ "$_flag" = from ]; then _v="$FROM"; else _v="$TO"; fi
  [ "$_flag" = to ] && [ "$_v" = 99 ] && continue
  [ "$_flag" = from ] && [ "$UPGRADE" = 1 ] && [ "$_v" = 0 ] && continue
  if [ "$UPGRADE" = 1 ]; then
    case "$_v" in
      [1-9] | 1[0-3]) ;;
      *) echo "under --upgrade, --$_flag takes an upgrade step from 1 to 13 (--help lists them), got '$_v'" >&2; exit 2 ;;
    esac
  else
    case "$_v" in
      [0-9] | 1[0-4]) ;;
      *) echo "--$_flag must be a step number from 0 to 14, got '$_v'" >&2; exit 2 ;;
    esac
  fi
done
[ "$UPGRADE" = 0 ] || [ "$FROM" != 0 ] || FROM=1
[ "$FROM" -le "$TO" ] || { echo "--from $FROM comes after --to $TO: no step would run" >&2; exit 2; }

# Item 187: the profile this host was installed with, recorded beside
# first-run.log. A run that names none while one is recorded would fall back to
# the built-in defaults without a word (2026-10-20: the Heir Kit went to
# ~/heir-kit and the catalog backup to the catalog's own disk, #502), so it is
# refused, naming the record and both ways on. Never loaded silently.
PROFILE_RECORD="${XDG_STATE_HOME:-$HOME/.local/state}/tapectl/profile"
RECORDED_PROFILE=""
[ ! -e "$PROFILE_RECORD" ] || RECORDED_PROFILE="$(head -n 1 "$PROFILE_RECORD" 2>/dev/null || true)"
if [ "$PROFILE_GIVEN" = 0 ] && [ -n "$RECORDED_PROFILE" ]; then
  echo "this host was installed with the profile $RECORDED_PROFILE (recorded in $PROFILE_RECORD), and this run names none: without it, the script's built-in defaults would replace the host's (the Heir Kit and backup directories, the ops group, the locations). Re-run with --profile $RECORDED_PROFILE, or with --profile none to run without it" >&2
  exit 2
fi

# ---------------------------------------------------------------- plumbing
REPO="$(cd "$(dirname "$0")/.." && pwd)"
# WORK_DIR: /scratch keeps the lock path every harness on the dev VM shares
# (/scratch/tapectl-build.lock); a host without /scratch gets a private one
# instead of a flock that cannot open its file (home2, 2026-09-26).
if [ -z "$WORK_DIR" ]; then
  if [ -d /scratch ] && [ -w /scratch ]; then WORK_DIR=/scratch; else WORK_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/tapectl"; fi
fi
mkdir -p "$WORK_DIR" || { echo "cannot create work dir $WORK_DIR" >&2; exit 2; }
BUILD_LOCK="$WORK_DIR/tapectl-build.lock"
# Where step 3 (and --upgrade's step 4) installs the binary, and where --upgrade
# keeps the old one beside it. TAPECTL_INSTALL_BIN is a test seam, as
# install-systemd.sh's TAPECTL_UNIT_DIR is: the upgrade harness
# (scripts/upgrade-mhvtl.sh) points it into its workspace.
INSTALL_BIN="${TAPECTL_INSTALL_BIN:-/usr/local/bin/tapectl}"
DAR="${DAR_BIN:-dar}"
if [ -t 1 ]; then B=$'\e[1m'; D=$'\e[2m'; R=$'\e[0m'; Y=$'\e[33m'; G=$'\e[32m'; RD=$'\e[31m'; else B=""; D=""; R=""; Y=""; G=""; RD=""; fi
[ "$SVC_MODE" = 1 ] && [ "$SVC_USER" = "$USER" ] && SVC_MODE=0
# The command an operator types for a catalog command, named in every remedy
# this script prints (#469): in service-user mode the catalog is the service
# user's (0600), so a bare `tapectl` from your own account is refused. Step 14
# switches its summary to tapectl-op once the wrapper is installed.
# Not `$( [ ... ] && printf ... )`: under `set -e` that substitution's
# failure (no service user) would end the script with no word said.
SUDO_PREFIX=""
if [ "$SVC_MODE" = 1 ]; then SUDO_PREFIX="sudo -u $SVC_USER -H "; fi
OP_CMD="${SUDO_PREFIX}tapectl"
# as_svc: THE privilege seam. Every tapectl invocation and every read/write of
# tapectl-owned state (the 0700 home, the 0600 config) goes through it; the
# rest of the script stays you, with sudo for root work.
as_svc()   { if [ "$SVC_MODE" = 1 ]; then sudo -u "$SVC_USER" -H "$@"; else "$@"; fi; }
svc_home() { if [ "$SVC_MODE" = 1 ]; then { getent passwd "$SVC_USER" 2>/dev/null || true; } | cut -d: -f6; else printf '%s' "$HOME"; fi; }
svc_in_group() { id -nG "$1" 2>/dev/null | tr ' ' '\n' | grep -x "$2" >/dev/null; }
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
# Item 187: record the profile (its absolute path), or say why none is used.
if [ "$PROFILE_GIVEN" = 1 ] && [ "$PROFILE" = none ]; then
  if [ -n "$RECORDED_PROFILE" ]; then
    note "--profile none: this run uses no profile; the host's recorded profile $RECORDED_PROFILE is not used (it stays recorded)"
  else
    note "--profile none: this run uses no profile, only the built-in defaults"
  fi
  PROFILE=""
elif [ "$PROFILE_GIVEN" = 1 ]; then
  if [ "$RECORDED_PROFILE" != "$PROFILE" ]; then
    if ! { mkdir -p "$(dirname "$PROFILE_RECORD")" && printf '%s\n' "$PROFILE" >"$PROFILE_RECORD"; }; then
      die "could not record the profile in $PROFILE_RECORD"
    fi
    [ -z "$RECORDED_PROFILE" ] || note "this host's recorded profile was $RECORDED_PROFILE; it is now $PROFILE ($PROFILE_RECORD)"
  fi
fi
# tty_read VAR "prompt": one line from the terminal. -e gives readline, so an
# arrow key or Home edits the line instead of landing in the answer as an
# escape sequence; whatever control bytes still arrive (a bracketed paste, a
# stray ESC) are stripped, then the answer is trimmed. Found on home2,
# 2026-09-28: step 6 read an answer that printed as nothing and died on it.
tty_read() {
  local __v="$1" __a=""
  read -e -r -p "$2" __a </dev/tty || true
  __a="$(printf '%s' "$__a" | sed -e 's/\x1b\[[0-9;?]*[ -\/]*[@-~]//g' | tr -d '[:cntrl:]')"
  __a="${__a#"${__a%%[![:space:]]*}"}"; __a="${__a%"${__a##*[![:space:]]}"}"
  printf -v "$__v" '%s' "$__a"
}
# ask VAR "prompt" "default"
ask() {
  local __v="$1" prompt="$2" def="${3:-}" ans
  if [ "$AUTO" = 1 ] && [ -n "$def" ]; then printf -v "$__v" '%s' "$def"; printf '   %s [auto: %s]\n' "$prompt" "$def"; return; fi
  tty_read ans "   $prompt${def:+ [$def]}: "
  printf -v "$__v" '%s' "${ans:-$def}"
}
# confirm "question" → 0 yes / 1 no. Non-destructive: --auto says yes.
confirm() {
  local ans
  if [ "$AUTO" = 1 ]; then printf '   %s [auto: yes]\n' "$1"; return 0; fi
  tty_read ans "   $1 [y/N] "
  [[ "$ans" =~ ^[Yy] ]]
}
# confirm_destructive "question" "word" [from_flag] → the user must type the
# word. Under --auto it counts ONLY when the caller says the word came from a
# command-line flag (from_flag=1): a default is never consent to erase.
confirm_destructive() {
  local ans
  if [ "$AUTO" = 1 ]; then
    if [ "${3:-0}" = 1 ]; then printf '   %s [auto: "%s" supplied via a flag]\n' "$1" "$2"; return 0; fi
    printf '   %s [auto: "%s" was NOT supplied via a flag — refusing under --auto]\n' "$1" "$2"; return 1
  fi
  tty_read ans "   $1 — type $2 to proceed: "
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
# run_capture_json FILE cmd...: like `run_capture`, but keeps stdout CLEAN.
# `run_capture` merges stderr with `2>&1` so its callers can grep one stream;
# that is fatal for a `--json` capture, because tapectl deliberately writes
# progress and warnings to stderr precisely so `--json` stdout stays parseable
# (src/volume/write.rs `announce_staged_selection`). Merging them yields a file
# that is not JSON and a parse that silently falls back. Stderr still reaches
# the terminal and the log, just not the JSON file.
run_capture_json() {
  local f="$1"; shift
  printf '   %s$ %s%s\n' "$B" "$*" "$R"; log "\$ $*"
  mkdir -p "$(dirname "$f")"
  "$@" >"$f" 2> >(tee -a "$LOG" >&2)
  return $?
}
# run_capture_json_err JSONFILE ERRFILE cmd...: `run_capture_json`, and its
# stderr kept in ERRFILE as well, for a caller that must read why it failed.
# Stderr still streams to the terminal and the log as it comes (a full verify
# runs for hours); the `wait` lets the tee finish ERRFILE before it is read.
run_capture_json_err() {
  local f="$1" e="$2"; shift 2
  printf '   %s$ %s%s\n' "$B" "$*" "$R"; log "\$ $*"
  mkdir -p "$(dirname "$f")"
  local rc=0
  "$@" >"$f" 2> >(tee "$e" | tee -a "$LOG" >&2) || rc=$?
  wait $! 2>/dev/null || true
  return "$rc"
}
# run_nolog: for the one command whose output must never be written to disk
run_nolog() { printf '   %s$ %s%s\n' "$B" "$*" "$R"; log "\$ $* (output NOT logged)"; "$@"; }
tc() { if [ -n "$HOME_DIR" ]; then as_svc "$TAPECTL" --home "$HOME_DIR" "$@"; else as_svc "$TAPECTL" "$@"; fi; }
# tape_raise_setup (step 13): raise THIS shell's CPU and I/O priority, once.
# The drive wants an even feed and a busy host starves it (#426: a write used
# 1.505 bytes of tape per data byte), so the tape commands of step 13 (volume
# write, resume, verify, cartridge erase) should outrank other work on the host.
# A process cannot raise its own priority without root, but nice and ionice
# are inherited across fork, exec and `sudo -u` (checked on home2, 2026-10-14):
# so the one root step is renicing the script's own shell, here, while a person
# is present, and every `tc ...` after it inherits the priority without root
# being asked again. No sudo ticket is kept alive, and none is needed later.
#   - It covers the resume branch and the post-write verify, because they are
#     children of this shell.
#   - It also covers the non-tape commands of step 13 (snapshot, stage, audit,
#     the kit) and step 14, and it cannot be undone: an unprivileged process
#     cannot lower its priority back. That is accepted.
#   - Step 12's rehearsal runs before this and stays at normal priority, and
#     nothing earlier is raised.
#   - tapectl-op and the drive-poll unit do not use this; they raise only the
#     tape commands, as root and then dropped to the service user
#     (contrib/systemd/tapectl-op.in, ADR-0012 2026-10-15 item 58).
# It asks for root's sudo (one password prompt through the normal `sudo`; under
# --auto, `sudo -n`, which asks nobody). If sudo refuses, it says so once and the
# run continues at normal priority: the priority aids the feed, it is never a
# reason to fail a write.
tape_priority() { # print "<nice> <ionice>" of this shell
  printf '%s %s' "$(ps -o ni= -p $$ | tr -d ' ')" "$(ionice -p $$ 2>/dev/null | tr -d '\n')"
}
tape_raise_setup() {
  local sudo_opt=() io=() before failed=0
  if [ -z "$TAPE_NICE" ]; then note "TAPE_NICE is empty: the tape commands run at normal priority"; return 0; fi
  [ "$AUTO" != 1 ] || sudo_opt=(-n)
  [ -z "$TAPE_IONICE" ] || read -r -a io <<<"$TAPE_IONICE"
  before="$(tape_priority)"
  [ "$AUTO" = 1 ] || note "raising the tape commands' priority needs root's sudo: it may ask for YOUR password, once"
  sudo ${sudo_opt[@]+"${sudo_opt[@]}"} renice -n "$TAPE_NICE" -p $$ >/dev/null 2>&1 || failed=1
  if [ "$failed" = 0 ] && [ "${#io[@]}" -gt 0 ]; then
    sudo ${sudo_opt[@]+"${sudo_opt[@]}"} ionice "${io[@]}" -p $$ >/dev/null 2>&1 || failed=1
  fi
  if [ "$failed" = 0 ]; then
    ok "this run, and so every tape command in it, is now at nice $TAPE_NICE${TAPE_IONICE:+, ionice $TAPE_IONICE} (was: $before; now: $(tape_priority))"
  else
    note "root's sudo refused to raise this run's priority$( [ "$AUTO" = 1 ] && printf ' (--auto has no one to ask for a password)' ); the tape commands run at: $(tape_priority) (was: $before)"
  fi
}
# derive_nodes: NST/NSTN (the canonical st node) and SG (the sg node the
# kernel binds to it) from $DEVICE. Step 6 computes these; every later step
# that needs them recomputes them here, so `--from 8/12/13 --device X`
# does not run with empty values (a `backend add --device-sg ""` was possible).
derive_nodes() {
  [ -n "$DEVICE" ] || die "no device chosen — pass --device, or run with --from 6"
  [ -e "$DEVICE" ] || die "$DEVICE does not exist"
  NST="$(readlink -f "$DEVICE")"; NSTN="$(basename "$NST")"
  if [ -z "$SG" ]; then
    # `|| true` and the `if`: with no sg node bound (the sg module not loaded),
    # a failed ls under pipefail, or a false `[ ]` as the function's last word,
    # ended the script under `set -e` without a word (#506).
    SGN="$(ls "/sys/class/scsi_tape/$NSTN/device/scsi_generic/" 2>/dev/null | head -1 || true)"
    if [ -n "$SGN" ]; then SG="/dev/$SGN"; fi
  fi
  return 0
}
# tape_lock: the one-tape-user rule every harness honours (the gate, the
# lifecycle suite, lto6-measure, lto6-fill). Step 13 takes it for its writes;
# step 12 must NOT (the suite takes it itself, and a parent holding it would
# deadlock the child -- flock is per open file description).
tape_lock() { exec 9>/tmp/tapectl-tape.lock; flock -n 9 || die "another process holds the tape lock (/tmp/tapectl-tape.lock) — a gate, a rehearsal or a fill is using the drive; wait for it"; }
# Item 190: step 12 and the drive poll. A poll during the rehearsal records the
# test cartridge in the PRODUCTION catalog: the rehearsal's homes are throwaway
# ones, so the production drive lock never sees it. drive_poll_pause refuses
# while a poll runs and stops an active timer; drive_poll_resume starts it again,
# at the end of the step or, through the EXIT trap, on any exit before that. A
# host without systemd, or without the units (the dev VM), is left alone.
POLL_TIMER_STOPPED=0
poll_running() { # the poll service's state when it is running now, else nothing
  local s; s="$(systemctl is-active tapectl-drive-poll.service 2>/dev/null || true)"
  # A oneshot unit is "activating" for as long as it runs: is-active --quiet says no then.
  case "$s" in active | activating | deactivating | reloading) printf '%s' "$s" ;; esac
}
drive_poll_pause() {
  command -v systemctl >/dev/null 2>&1 || return 0
  local s; s="$(poll_running)"
  [ -z "$s" ] || die "tapectl-drive-poll.service is running now ($s: a drive health poll); wait for it to finish (systemctl status tapectl-drive-poll.service), then re-run scripts/first-run.sh --from 12"
  systemctl is-active --quiet tapectl-drive-poll.timer 2>/dev/null || return 0
  explain <<'EOF'
THE DRIVE POLL. tapectl-drive-poll.timer is active on this host: once a day it reads the drive's health pages and chip as the service user and records them in the PRODUCTION catalog. A poll during the rehearsal would record the test cartridge there, so the timer is stopped for the rehearsal and started again when this step ends, whether it succeeds or fails (ADR-0012 item 190).
EOF
  confirm "Stop tapectl-drive-poll.timer for the rehearsal (sudo; it is started again when this step ends)?" \
    || die "the rehearsal needs tapectl-drive-poll.timer stopped (a poll would record the test cartridge in the production catalog); nothing was run. Re-run scripts/first-run.sh --from 12 and answer y"
  run sudo systemctl stop tapectl-drive-poll.timer || die "could not stop tapectl-drive-poll.timer (above); nothing was run"
  POLL_TIMER_STOPPED=1
  trap drive_poll_on_exit EXIT
  # The timer may have fired between the check above and the stop.
  s="$(poll_running)"
  [ -z "$s" ] || die "tapectl-drive-poll.service started just now ($s); wait for it to finish, then re-run scripts/first-run.sh --from 12"
}
# drive_poll_resume never fails: it also runs from the EXIT trap, where under
# `set -e` a failing command would end the trap and replace the exit status.
drive_poll_resume() {
  [ "$POLL_TIMER_STOPPED" = 1 ] || return 0
  POLL_TIMER_STOPPED=0
  trap - EXIT
  if run sudo systemctl start tapectl-drive-poll.timer; then ok "tapectl-drive-poll.timer started again"
  else note "could not start tapectl-drive-poll.timer again: run sudo systemctl start tapectl-drive-poll.timer"; fi
  return 0
}
drive_poll_on_exit() { local rc=$?; drive_poll_resume; return "$rc"; }
# The DRIVE's own generation, parsed from its INQUIRY product id (issue #178).
# shellcheck source=lib/drive-generation.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib/drive-generation.sh"
# shellcheck source=lib/volume-state.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib/volume-state.sh"
# The heir kit is YOUR artifact to print, but escrow-kit chmods its out dir 0700
# as whoever runs it. Hand the dir to the service user for the write, take it back after.
kit_prepare() {
  mkdir -p "$1"
  [ "$SVC_MODE" = 1 ] || return 0
  # The kit dir may sit under your home, which the service user cannot
  # traverse (your home is 0750): give every ancestor traverse-only (x), the
  # same shape grant_read uses, then hand the dir itself over for the write.
  local p; p="$(dirname "$1")"
  while [ "$p" != / ] && [ -n "$p" ]; do as_svc test -x "$p" || sudo setfacl -m "u:$SVC_USER:x" "$p" || return 1; p="$(dirname "$p")"; done
  sudo chown -R "$SVC_USER" "$1"
}
kit_finish()  { [ "$SVC_MODE" = 1 ] && sudo chown -R "$USER" "$1"; return 0; }
# adopt_key_for NAME: with ADOPT_KEYS_FROM set, the identity file NAME adopts
# ($ADOPT_KEYS_FROM/NAME.key) in ADOPT_FILE, after checking that the user
# tapectl runs as can read it; dies naming the file otherwise — a missing
# file must never quietly become a freshly minted key (issue #424). Only the
# PATH is ever passed on or logged; tapectl reads the secret itself.
adopt_key_for() {
  ADOPT_FILE="$ADOPT_KEYS_FROM/$1.key"
  if ! as_svc test -r "$ADOPT_FILE"; then
    if [ -e "$ADOPT_FILE" ] || sudo -n test -e "$ADOPT_FILE" 2>/dev/null; then
      die "$( [ "$SVC_MODE" = 1 ] && echo "$SVC_USER" || echo "$USER" ) cannot read $ADOPT_FILE — hand the directory over first: sudo chown -R $( [ "$SVC_MODE" = 1 ] && echo "$SVC_USER" || echo "$USER" ) $ADOPT_KEYS_FROM && sudo chmod 0700 $ADOPT_KEYS_FROM && sudo chmod 0600 $ADOPT_KEYS_FROM/*.key (its parent directories must let that user through)"
    fi
    die "ADOPT_KEYS_FROM is set, but there is no $ADOPT_FILE to adopt for \"$1\" — copy it out of the old home (docs/keys-and-recovery.md), or unset ADOPT_KEYS_FROM to mint new keys"
  fi
}
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
# grant_collection ROOT DEPTH: grant_read's shape for a folder-per-unit root —
# read on the whole tree (and by default on what is added later), traverse on
# the ancestors, and write on each unit folder (DEPTH below ROOT) only, which
# is where `collection sync` puts each unit's .tapectl-unit.toml.
grant_collection() {
  [ "$SVC_MODE" = 1 ] || return 0
  local root="$1" depth="$2" p
  case "$root" in /*) ;; *) note "collection root $root is not an absolute path"; return 1 ;; esac
  explain <<EOF
$SVC_USER needs read on $root (every file is snapshotted and staged) and write on each unit folder $depth level(s) below it (the .tapectl-unit.toml dotfile). The grant is POSIX ACLs only — owners, groups and modes stay exactly as they are. A large tree takes a while to walk.
EOF
  confirm "Grant $SVC_USER read on $root and write on its unit folders (setfacl)?" || return 1
  p="$(dirname "$root")"
  while [ "$p" != / ] && [ -n "$p" ]; do as_svc test -x "$p" || run sudo setfacl -m "u:$SVC_USER:x" "$p" || return 1; p="$(dirname "$p")"; done
  run sudo setfacl -R -m "u:$SVC_USER:rX" "$root" || { note "setfacl failed — a filesystem without ACL support?"; return 1; }
  run sudo setfacl -R -d -m "u:$SVC_USER:rX" "$root" || return 1
  run sudo find "$root" -mindepth "$depth" -maxdepth "$depth" -type d -exec setfacl -m "u:$SVC_USER:rwX" {} + || return 1
  as_svc test -r "$root" && as_svc test -x "$root"
}
skip_if() { [ "$FROM" -gt "$STEP" ] && { note "skipped (--from $FROM)"; return 0; }; return 1; }
vercmp_ge() { [ "$(printf '%s\n%s\n' "$2" "$1" | sort -V | head -1)" = "$2" ]; }
# dar_path_prefix: "DIR:" when --dar names a dar outside PATH's first hit, so the
# step-4 suite (whose fixtures run plain `dar`) tests the dar production uses.
dar_path_prefix() { [ -n "$DAR_BIN" ] && printf '%s:' "$(dirname "$DAR_BIN")"; return 0; }
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
# build_release: step 3's release build, which --upgrade's step 4 runs too. BUILT
# is the binary it made.
build_release() {
  toolchain_check
  ( cd "$REPO" && run flock -w 1200 -E 99 "$BUILD_LOCK" cargo build --release ) || die "release build failed (exit 99 = timed out waiting for $BUILD_LOCK: another build is linking)"
  BUILT="$REPO/target/release/tapectl"
  [ -x "$BUILT" ] || BUILT="${CARGO_TARGET_DIR:-$REPO/target}/release/tapectl"
}
# known_cartridge NAME: the barcode of the catalog's cartridge whose barcode,
# chip serial or claimed serial is NAME, or nothing when none is. It fails when
# the catalog cannot be read, so the caller refuses rather than guesses
# (step 12, ADR-0012 item 192). `cartridge list --json` carries no serial, so
# each row is asked with `cartridge info --json`; a catalog holds a few.
known_cartridge() {
  local list barcodes b info rc
  list="$(tc cartridge list --json 2>/dev/null)" || return 1
  # Parsed into a variable first: a parse that fails inside `< <(…)` would only
  # end the loop, and an unread list must not read as an empty one.
  barcodes="$(printf '%s' "$list" | python3 -c 'import json, sys
d = json.load(sys.stdin)
for r in (d if isinstance(d, list) else d["cartridges"]):
    print(r["barcode"])')" || return 1
  while IFS= read -r b; do
    [ -n "$b" ] || continue
    info="$(tc cartridge info "$b" --json 2>/dev/null)" || return 1
    rc=0
    printf '%s' "$info" | python3 -c 'import json, sys
d = json.load(sys.stdin)
sys.exit(3 if sys.argv[1] in (d.get("barcode"), d.get("serial_number"), d.get("operator_serial")) else 0)' "$1" || rc=$?
    case "$rc" in
      0) ;;
      3) printf '%s' "$b"; return 0 ;;
      *) return 1 ;;
    esac
  done <<<"$barcodes"
  return 0
}

# ================================================================ step 0
printf '%stapectl first run%s — repo %s\n' "$B" "$R" "$REPO"
printf 'home: %s   log: %s\n' "$EFFECTIVE_HOME" "$LOG"
if [ -n "$PROFILE" ]; then printf 'profile: %s\n' "$PROFILE"; elif [ "$PROFILE_GIVEN" = 1 ]; then printf 'profile: none\n'; fi
printf 'work dir: %s   dar: %s\n' "$WORK_DIR" "$DAR"
printf 'service user: %s\n' "$( [ "$SVC_MODE" = 1 ] && echo "$SVC_USER (every tapectl command runs as it)" || echo "none — running as $USER" )"
if [ "$SVC_MODE" = 0 ] || id "$SVC_USER" >/dev/null 2>&1; then
  if [ -z "$HOME_DIR" ] && as_svc test -e "$EFFECTIVE_HOME/tapectl.db" 2>/dev/null && [ "$FROM" -le 7 ]; then
    note "$EFFECTIVE_HOME is already initialised. This script will detect that in step 7 and not re-init."
  fi
  # Keys renamed since a home may have been initialised (ADR-0012, 2026-09-28
  # amendment; issue #348): [defaults] min_copies_for_tape_only → min_copies and
  # min_locations_for_tape_only → min_locations, same meaning. The new binary
  # refuses the old names, and a re-entry at --from 12/13 never reaches step 7's
  # config probe — so the fixup lives here, where every run passes.
  CFG0="$EFFECTIVE_HOME/config.toml"
  if as_svc test -f "$CFG0" 2>/dev/null \
     && as_svc grep -qE '^[[:space:]]*min_(copies|locations)_for_tape_only[[:space:]]*=' "$CFG0" 2>/dev/null; then
    note "$CFG0 uses key names this tapectl renamed (same meaning):"
    note "  min_copies_for_tape_only → min_copies,  min_locations_for_tape_only → min_locations"
    if confirm "Rename them in place now (the old file is kept beside it)?"; then
      run as_svc cp "$CFG0" "$CFG0.pre-rename-$(date +%Y%m%d-%H%M%S)"
      run as_svc sed -i -E \
        -e 's/^([[:space:]]*)min_copies_for_tape_only([[:space:]]*=)/\1min_copies\2/' \
        -e 's/^([[:space:]]*)min_locations_for_tape_only([[:space:]]*=)/\1min_locations\2/' "$CFG0"
      as_svc grep -qE '^[[:space:]]*min_(copies|locations)_for_tape_only[[:space:]]*=' "$CFG0" \
        && die "the rename did not take — edit [defaults] in $CFG0 by hand"
      ok "config keys renamed"
    else
      note "left as is — every tapectl command will refuse this config until the two keys are renamed"
    fi
  fi
  # Issue #391 (ADR-0012, 2026-10-06 items 9 and 16): [[backends.lto]]
  # usable_capacity_factor → fill_ceiling, same meaning (the share of a
  # cartridge a write may fill). Renaming keeps the value the file has; deleting
  # the line instead takes the ruled default, 0.97.
  if as_svc test -f "$CFG0" 2>/dev/null \
     && as_svc grep -qE '^[[:space:]]*usable_capacity_factor[[:space:]]*=' "$CFG0" 2>/dev/null; then
    note "$CFG0 uses a key this tapectl renamed (same meaning): usable_capacity_factor → fill_ceiling"
    note "  renaming keeps your value; to take the new default (0.97) delete the line instead"
    if confirm "Rename it in place now, keeping its value (the old file is kept beside it)?"; then
      run as_svc cp "$CFG0" "$CFG0.pre-rename-$(date +%Y%m%d-%H%M%S)"
      run as_svc sed -i -E \
        -e 's/^([[:space:]]*)usable_capacity_factor([[:space:]]*=)/\1fill_ceiling\2/' "$CFG0"
      as_svc grep -qE '^[[:space:]]*usable_capacity_factor[[:space:]]*=' "$CFG0" \
        && die "the rename did not take — edit [[backends.lto]] in $CFG0 by hand"
      ok "config key renamed"
    else
      note "left as is — every tapectl command will refuse this config until the key is renamed or deleted"
    fi
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

# ================================================================ --upgrade
# ADR-0012 item 192 (#505): the in-place upgrade of a host this script
# installed, built from handoff (h)'s run (home2, 1.5.0 → 1.8.0, 2026-10-29).
# Its thirteen steps are listed in --help. Three of them are this script's own
# steps run again as children (`upgrade_child`: 14, 12, 9), so each is the code
# a first install runs. What a step has done is recorded in <state>/upgrade-<new>
# and checked when the upgrade runs again, never done twice: a run that stopped
# is resumed by running --upgrade again. What only one release needs (a config
# key, a tool, a skip rule) stays in its notes; step 7 stops at them.
if [ "$UPGRADE" = 1 ]; then
U_STEP=0
uhdr() {
  U_STEP="$1"
  [ "$1" -gt "$TO" ] && { printf '\n%s== stopping before upgrade step %s (--to %s) ==%s\n' "$B" "$1" "$TO" "$R"; exit 0; }
  printf '\n%s== Upgrade step %s — %s ==%s\n' "$B" "$1" "$2" "$R"; log "upgrade step $1: $2"
}
uskip() { [ "$FROM" -gt "$U_STEP" ] && { note "skipped (--from $FROM)"; return 0; }; return 1; }
# The upgrade's record: KEY=VALUE lines, appended; the last one wins.
ustate() { [ -f "$U_STATE" ] && sed -n "s/^$1=//p" "$U_STATE" | tail -n 1; return 0; }
ustate_set() { mkdir -p "$(dirname "$U_STATE")" && printf '%s=%s\n' "$1" "$2" >>"$U_STATE"; }
# db_facts FILE [version]: "<user_version> <ok|damaged>" (or the version alone),
# FILE opened READ-ONLY by python3's sqlite3 as the user tapectl runs as. Never
# by tapectl: its first open of a catalog applies the migrations.
db_facts() {
  as_svc python3 - "$1" "${2:-integrity}" <<'PY'
import sqlite3, sys, urllib.parse
c = sqlite3.connect("file:" + urllib.parse.quote(sys.argv[1]) + "?mode=ro", uri=True, timeout=30)
v = c.execute("PRAGMA user_version").fetchone()[0]
if sys.argv[2] == "version":
    print(v)
else:
    rows = [r[0] for r in c.execute("PRAGMA integrity_check")]
    print(v, "ok" if rows == ["ok"] else "damaged")
PY
}
# installed_is_new: the installed binary is a build of the tag.
installed_is_new() { case "$("$INSTALL_BIN" --version 2>/dev/null)" in "tapectl $NEW_VER ($TAG, "*) return 0 ;; *) return 1 ;; esac; }
need_new() { installed_is_new || die "the installed $INSTALL_BIN is not $TAG ($("$INSTALL_BIN" --version 2>/dev/null || echo unreadable)): run --upgrade --from 4"; }
# upgrade_child N [ARG…]: this script's own step N, with the installed binary and
# the arguments this run was given (FWD_ARGS). Not through `run`: the child logs
# its own lines, and its prompts read the terminal.
upgrade_child() {
  local n="$1" rc=0 pa=()
  shift
  [ "$PROFILE_GIVEN" = 0 ] || pa=(--profile "${PROFILE:-none}")
  local cmd=(bash "$REPO/scripts/first-run.sh" ${pa[@]+"${pa[@]}"} ${FWD_ARGS[@]+"${FWD_ARGS[@]}"}
    --tapectl "$INSTALL_BIN" --skip-build --skip-tests --from "$n" --to "$n" "$@")
  printf '   %s$ %s%s\n' "$B" "${cmd[*]}" "$R"; log "\$ ${cmd[*]}"
  "${cmd[@]}" || rc=$?
  [ "$rc" = 0 ] || die "first-run.sh's step $n stopped (exit $rc, above); once that is dealt with, run --upgrade again (it resumes at upgrade step $U_STEP)"
}
# look CMD…: a tapectl command whose exit status is shown, never acted on.
look() { local rc=0; run tc "$@" || rc=$?; note "(exit $rc)"; }

# ---------------------------------------------------------------- upgrade 1
uhdr 1 "Preflight"
explain <<'EOF'
An upgrade replaces the binary and keeps the home, the keys, the catalog and the tapes; the new binary's first open moves the catalog forward, and there is no downgrade past a migration. So before anything changes: nothing of tapectl may be running, the checkout must be exactly a release tag, and the release must be newer than what is installed. This step always runs, whatever --from says.
EOF
U_RUN_USER="$( [ "$SVC_MODE" = 1 ] && printf '%s' "$SVC_USER" || printf '%s' "$USER" )"
U_RUNNING="$(pgrep -a -x -u "$U_RUN_USER" tapectl || true)"
if [ -n "$U_RUNNING" ]; then
  printf '%s\n' "$U_RUNNING" | sed 's/^/   /'
  die "a tapectl command is running as $U_RUN_USER (above); wait for it to finish, then run --upgrade again"
fi
if command -v systemctl >/dev/null 2>&1; then
  for _u in tapectl-audit.service tapectl-backup.service tapectl-drive-poll.service; do
    _s="$(systemctl is-active "$_u" 2>/dev/null || true)"
    case "$_s" in active | activating | deactivating | reloading) die "$_u is running now ($_s); wait for it to finish (systemctl status $_u), then run --upgrade again" ;; esac
  done
fi
TAG="$(git -C "$REPO" describe --exact-match HEAD 2>/dev/null || true)"
[ -n "$TAG" ] || die "the checkout $REPO is at $(git -C "$REPO" describe --tags --always 2>/dev/null || echo '?'), not exactly an annotated release tag: check out the release first (git fetch --tags && git checkout vX.Y.Z)"
[[ "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "HEAD's tag $TAG is not a release tag of the form vX.Y.Z"
[ "$(git -C "$REPO" cat-file -t "$TAG" 2>/dev/null)" = tag ] || die "$TAG is not an annotated tag (git cat-file -t $TAG)"
# Untracked files do not count, as they do not for the binary's own version
# (build.rs: git describe --dirty): nothing untracked reaches the build.
if [ -n "$(git -C "$REPO" status --porcelain --untracked-files=no)" ]; then
  git -C "$REPO" status --short --untracked-files=no | sed 's/^/   /'
  die "the checkout has changes (above): the binary is built from the tag as it is, so leave the tree clean first"
fi
NEW_VER="${TAG#v}"
_cargo_ver="$(sed -n 's/^version *= *"\(.*\)"/\1/p' "$REPO/Cargo.toml" | head -n 1)"
[ "$_cargo_ver" = "$NEW_VER" ] || die "$TAG's Cargo.toml says version $_cargo_ver, not $NEW_VER"
U_NOTES="$REPO/docs/releases/$NEW_VER.md"
[ -f "$U_NOTES" ] || die "$TAG has no release notes ($U_NOTES), and step 7 stops at them"
[ -x "$INSTALL_BIN" ] || die "nothing is installed at $INSTALL_BIN, so there is nothing to upgrade: a first install is scripts/first-run.sh without --upgrade"
INST_LINE="$("$INSTALL_BIN" --version 2>/dev/null || true)"
INST_VER="$(printf '%s' "$INST_LINE" | awk '$1 == "tapectl" { print $2; exit }')"
[ -n "$INST_VER" ] || die "$INSTALL_BIN --version printed '${INST_LINE}', not a tapectl version"
U_STATE="$(dirname "$LOG")/upgrade-$NEW_VER"
OLD_VER="$(ustate old)"
UNIT_FILE="${TAPECTL_UNIT_DIR:-/etc/systemd/system}/tapectl-backup.service"
UNIT_BDIR=""
if [ -r "$UNIT_FILE" ]; then UNIT_BDIR="$(sed -n '/^Environment=TAPECTL_BACKUP_DIR=/{s///p;q;}' "$UNIT_FILE")"; fi
if [ -z "$OLD_VER" ]; then
  [ "$INST_VER" != "$NEW_VER" ] || die "the installed tapectl is already $NEW_VER ($INST_LINE), and no upgrade to it is recorded here ($U_STATE): there is nothing to upgrade. Every build installed on a production host has a version of its own (ADR-0012, 2026-09-30)"
  vercmp_ge "$NEW_VER" "$INST_VER" || die "$TAG is older than the installed $INST_VER: there is no downgrade past a migration (docs/install.md §10, \"Rolling back\")"
  OLD_VER="$INST_VER"
  ustate_set old "$OLD_VER"
  ustate_set started "$(date -u +%FT%TZ)"
  # Where the daily backup wrote before this upgrade, for step 12's tidy.
  ustate_set unit_backup_dir "$UNIT_BDIR"
else
  case "$INST_VER" in
    "$OLD_VER" | "$NEW_VER") note "resuming the upgrade from $OLD_VER to $NEW_VER recorded in $U_STATE" ;;
    *) die "the installed tapectl is $INST_VER, but $U_STATE records an upgrade from $OLD_VER to $NEW_VER: stop and look" ;;
  esac
fi
# The binary step 4 installs: built from the tag, or the --tapectl PATH that
# --skip-build names. A profile's TAPECTL alone is not a build to install.
NEW_BIN=""
if [ "$SKIP_BUILD" = 1 ]; then
  [ -n "$TAPECTL" ] || die "--skip-build under --upgrade installs the binary --tapectl names, and none is named"
  [ -x "$TAPECTL" ] || die "--tapectl $TAPECTL is not executable"
  NEW_BIN="$TAPECTL"
fi
# From here on every tapectl command is the installed one: the old binary until
# step 4 replaces it, then the new.
TAPECTL="$INSTALL_BIN"
CATALOG="$EFFECTIVE_HOME/tapectl.db"
as_svc test -r "$CATALOG" || die "there is no catalog at $CATALOG for $U_RUN_USER to read: is this the host's home (--home, --user)?"
# The backup goes where the daily backup does (#502: never the default unasked).
U_BDIR="${BACKUP_DIR:-$UNIT_BDIR}"
[ -n "$U_BDIR" ] || die "no backup directory: neither --backup-dir nor the profile names one, and no installed tapectl-backup.service does ($UNIT_FILE). Pass --backup-dir DIR, the second disk the daily backup uses (docs/install.md §7)"
KEEP="$(dirname "$INSTALL_BIN")/tapectl-$OLD_VER"
MIG_TOP="$(find "$REPO/src/db/migrations" -maxdepth 1 -name '[0-9]*.sql' -printf '%f\n' | sed 's/_.*//' | sort -n | tail -n 1)"
MIG_TOP=$((10#${MIG_TOP:-0}))
if [ -x /usr/local/bin/tapectl-op ] && [ -z "$HOME_DIR" ]; then U_OP=tapectl-op; else U_OP="${SUDO_PREFIX}$INSTALL_BIN${HOME_DIR:+ --home $HOME_DIR}"; fi
ok "installed: $INST_LINE ($INSTALL_BIN)"
ok "new: $TAG, the checkout $REPO (clean); its migrations end at $MIG_TOP"
ok "catalog: $CATALOG; backups: $U_BDIR"
log "upgrade $OLD_VER -> $NEW_VER ($U_STATE)"

# ---------------------------------------------------------------- upgrade 2
uhdr 2 "Back up the catalog with the old binary ($OLD_VER)"
uskip || {
U_BACKUP="$(ustate backup)"
if [ -n "$U_BACKUP" ]; then
  _facts="$(db_facts "$U_BACKUP")" || die "cannot read the backup $U_BACKUP that $U_STATE records"
  [ "${_facts#* }" = ok ] || die "the backup $U_BACKUP fails SQLite's integrity check"
  ok "taken already: $U_BACKUP (schema ${_facts%% *}, integrity ok)"
else
  [ "$INST_VER" = "$OLD_VER" ] || die "the installed tapectl is $INST_VER, and $U_STATE records no backup taken with $OLD_VER: the catalog may have moved on already. Do not go on without a backup: stop and look"
  { as_svc test -d "$U_BDIR" && as_svc test -w "$U_BDIR"; } || die "$U_RUN_USER cannot write $U_BDIR, the backup directory"
  U_BACKUP="$U_BDIR/tapectl-pre-$NEW_VER.db"
  if as_svc test -e "$U_BACKUP"; then
    U_BACKUP="$U_BDIR/tapectl-pre-$NEW_VER-$(date -u +%Y%m%dT%H%M%SZ).db"
    note "tapectl-pre-$NEW_VER.db is there already; this backup goes to $U_BACKUP"
  fi
  run tc db backup --to "$U_BACKUP" || die "db backup with $OLD_VER failed (above); nothing has changed"
  _facts="$(db_facts "$U_BACKUP")" || die "cannot read the backup $U_BACKUP just written"
  [ "${_facts#* }" = ok ] || die "the backup $U_BACKUP fails SQLite's integrity check: do not go on"
  ustate_set backup "$U_BACKUP"
  ustate_set schema_before "${_facts%% *}"
  ok "backed up with $OLD_VER: $U_BACKUP (schema ${_facts%% *}, integrity ok)"
fi
}

# ---------------------------------------------------------------- upgrade 3
uhdr 3 "Keep the old binary as $KEEP"
uskip || {
if [ -e "$KEEP" ]; then
  _kv="$("$KEEP" --version 2>/dev/null | awk '{ print $2; exit }')"
  [ "$_kv" = "$OLD_VER" ] || die "$KEEP is tapectl ${_kv:-<unreadable>}, not $OLD_VER: stop and look"
  ok "kept already: $KEEP ($("$KEEP" --version))"
else
  [ "$INST_VER" = "$OLD_VER" ] || die "the installed tapectl is already $INST_VER, and no copy of $OLD_VER is at $KEEP: a rollback would have no binary. Stop and look"
  run sudo cp -p "$INSTALL_BIN" "$KEEP" || die "could not copy $INSTALL_BIN to $KEEP"
  ok "kept: $KEEP ($("$KEEP" --version))"
fi
}

# ---------------------------------------------------------------- upgrade 4
uhdr 4 "Build and install $TAG"
uskip || {
if installed_is_new; then ok "installed already: $("$INSTALL_BIN" --version)"
else
  if [ -n "$NEW_BIN" ]; then
    note "--skip-build: installing $NEW_BIN"
  else
    explain <<'EOF'
The release binary, built from the checkout under the build lock: a few minutes. It is a new artifact whatever an earlier build proved, which is why step 9 rehearses it on a test cartridge.
EOF
    confirm "Build the release binary from $TAG now (cargo build --release)?" || die "stopped before the build; nothing has changed since step 3"
    build_release
    NEW_BIN="$BUILT"
  fi
  _nv="$("$NEW_BIN" --version 2>/dev/null || true)"
  case "$_nv" in
    "tapectl $NEW_VER ($TAG, "*) ;;
    *) die "$NEW_BIN says '${_nv}', not tapectl $NEW_VER built from $TAG: only a build of the tag is installed" ;;
  esac
  confirm "Install it to $INSTALL_BIN (sudo)?" || die "stopped before the install; nothing has changed since step 3"
  run sudo install -m 0755 "$NEW_BIN" "$INSTALL_BIN" || die "could not install $NEW_BIN to $INSTALL_BIN"
  need_new
  ok "installed: $("$INSTALL_BIN" --version)"
fi
}

# ---------------------------------------------------------------- upgrade 5
uhdr 5 "The new binary's first open: the migrations"
uskip || {
need_new
_before="$(ustate schema_before)"
[ -n "$_before" ] || { _before="$(db_facts "$CATALOG" version)" || die "cannot read $CATALOG's schema"; }
run tc config check || die "config check refused (above). A key a release renamed or removed is named there; the release notes say what to do ($U_NOTES, \"Upgrading\"). Fix it, then run --upgrade again"
_after="$(db_facts "$CATALOG" version)" || die "cannot read $CATALOG's schema"
ok "the catalog's schema: $_before → $_after"
[ "$_after" = "$MIG_TOP" ] || die "the catalog is at schema $_after, but $TAG's migrations end at $MIG_TOP"
[ "$_before" != "$_after" ] || note "no migration applied: $OLD_VER and $NEW_VER have the same schema"
}

# ---------------------------------------------------------------- upgrade 6
uhdr 6 "Step 14 again: the units, the wrapper and the timers"
uskip || { need_new; upgrade_child 14; }

# ---------------------------------------------------------------- upgrade 7
uhdr 7 "The release's own steps"
uskip || {
explain <<'EOF'
What only one release asks of you is in its notes, under "Upgrading": a config key to set, a tool to install, which audit check to read. The notes of every release after the one you had, up to this one, follow; a release contains the ones before it, so each one's steps apply.
EOF
_found=0
while IFS= read -r _v; do
  [ -n "$_v" ] || continue
  _found=1
  printf '\n   %sdocs/releases/%s.md, "Upgrading":%s\n' "$B" "$_v" "$R"
  _sec="$(awk '/^## Upgrading/ { f = 1; next } /^## / { f = 0 } f' "$REPO/docs/releases/$_v.md")"
  if [ -n "$(printf '%s' "$_sec" | tr -d '[:space:]')" ]; then printf '%s\n' "$_sec" | sed 's/^/     /'
  else note "(no \"Upgrading\" section: read the notes whole)"; fi
done < <(for _f in "$REPO"/docs/releases/[0-9]*.md; do
           _v="$(basename "$_f" .md)"
           if [ "$_v" != "$OLD_VER" ] && vercmp_ge "$_v" "$OLD_VER" && vercmp_ge "$NEW_VER" "$_v"; then printf '%s\n' "$_v"; fi
         done | sort -V)
[ "$_found" = 1 ] || die "no release notes between $OLD_VER and $NEW_VER under $REPO/docs/releases"
if [ "$AUTO" = 1 ]; then
  note "--auto does not stop here: the steps above are yours to do"
else
  _a=""
  tty_read _a "   Press Enter once they are done (or type q to stop here; --upgrade --from 8 goes on later): "
  [ "$_a" != q ] || die "stopped at upgrade step 7; once the release's steps are done, run --upgrade --from 8"
fi
}

# ---------------------------------------------------------------- upgrade 8
uhdr 8 "Look"
uskip || {
need_new
explain <<'EOF'
What the catalog says with the new binary. audit is advisory (exit 1 warnings, 2 violations): a check a release added may report something that was always true, and the notes above say what to expect.
EOF
look status
look audit
look report failed-stages
}

# ---------------------------------------------------------------- upgrade 9
uhdr 9 "Step 12 again: rehearse the installed binary on a test cartridge"
uskip || {
need_new
U_MARKER="$(dirname "$LOG")/rehearsal-ok-$(sha256sum "$INSTALL_BIN" | cut -c1-16)"
if [ -f "$U_MARKER" ]; then
  ok "a green rehearsal of the installed binary is recorded: $(cat "$U_MARKER")"
else
  upgrade_child 12
  [ -f "$U_MARKER" ] || die "no green rehearsal of the installed binary is recorded ($U_MARKER): step 12 above was skipped or refused it. The upgrade is not done without one: load a test cartridge and run --upgrade again"
  ok "rehearsal green: $(cat "$U_MARKER")"
fi
}

# ---------------------------------------------------------------- upgrade 10
uhdr 10 "Step 9 again: the Heir Kit"
uskip || {
need_new
# Step 9 asks for the directory, with this one as its default: the kit is
# checked here, so a different answer there is reported, not looked for.
KIT_OUT="${KIT_OUT:-$HOME/heir-kit}"
upgrade_child 9 --kit-out "$KIT_OUT"
_kb="$KIT_OUT/tapectl-$NEW_VER"
[ -f "$_kb" ] || die "no $_kb: the kit in $KIT_OUT was not made by $NEW_VER (above). If step 9 was given another directory, check the kit there"
_a="$(sha256sum "$_kb" | cut -d' ' -f1)"; _b="$(sha256sum "$INSTALL_BIN" | cut -d' ' -f1)"
[ "$_a" = "$_b" ] || die "the kit's tapectl-$NEW_VER ($_a) is not the installed binary ($_b)"
grep -F "$_a" "$KIT_OUT/COVER.txt" >/dev/null || die "$KIT_OUT/COVER.txt does not name tapectl-$NEW_VER's sha256 $_a"
ok "the kit's tapectl-$NEW_VER is the installed binary, and COVER.txt names its sha256"
note "Yours now: print $KIT_OUT/COVER.txt, put the new catalog.db.age and tapectl-$NEW_VER on the media in each envelope, and destroy the sheets they replace."
}

# ---------------------------------------------------------------- upgrade 11
uhdr 11 "A catalog backup made by $NEW_VER"
uskip || {
if [ -n "$HOME_DIR" ]; then
  note "--home $HOME_DIR is not the host's installed home: no backup unit is started for it"
elif ! command -v systemctl >/dev/null 2>&1 || ! systemctl cat tapectl-backup.service >/dev/null 2>&1; then
  note "tapectl-backup.service is not installed on this host (step 14 installs it): no copy taken"
elif confirm "Start tapectl-backup.service once now (sudo), so a copy made by $NEW_VER lands in its directory?"; then
  run sudo systemctl start tapectl-backup.service || die "tapectl-backup.service failed: journalctl -u tapectl-backup.service -n 30"
  ok "tapectl-backup.service ran"
else
  note "skipped: sudo systemctl start tapectl-backup.service takes one later"
fi
}

# ---------------------------------------------------------------- upgrade 12
uhdr 12 "Tidy what earlier runs left"
uskip || {
explain <<EOF
What earlier installs, rehearsals and upgrades leave behind, by group, with its size. Nothing is removed unless you type the word for its group (never under --auto). Never listed: the home ($EFFECTIVE_HOME), the kit directory (${KIT_OUT:-none}), the backup directory ($U_BDIR), the checkout, the installed binary and the newest kept one.
EOF
# tidy_kept PATH: PATH is, holds, or (for a directory the upgrade keeps) lies
# inside something this step never touches.
tidy_kept() {
  local p q
  p="$(realpath -m "$1")"
  for q in "$EFFECTIVE_HOME" "$KIT_OUT" "$U_BDIR" "$REPO"; do
    [ -n "$q" ] || continue
    q="$(realpath -m "$q")"
    case "$p/" in "$q/"*) return 0 ;; esac
  done
  for q in "$INSTALL_BIN" "$KEEP" "$HOME"; do
    q="$(realpath -m "$q")"
    case "$q/" in "$p/"*) return 0 ;; esac
  done
  return 1
}
# tidy_group TITLE OWNER PATH…: list the paths with their sizes; remove them only
# on the typed word. OWNER says who removes: me, svc (the service user) or root.
tidy_group() {
  local title="$1" owner="$2" p sz
  shift 2
  [ "$#" -gt 0 ] || return 0
  printf '\n   %s%s%s\n' "$B" "$title" "$R"
  for p in "$@"; do
    case "$owner" in
      svc) sz="$(as_svc du -sh "$p" 2>/dev/null | cut -f1)" ;;
      *) sz="$(du -sh "$p" 2>/dev/null | cut -f1)" ;;
    esac
    printf '     %6s  %s\n' "${sz:-?}" "$p"
  done
  log "tidy candidates ($title): $*"
  if [ "$AUTO" = 1 ]; then note "--auto lists only: nothing removed"; return 0; fi
  if confirm_destructive "Remove these $# ($title)" remove; then
    for p in "$@"; do
      case "$owner" in
        svc) run as_svc rm -rf -- "$p" || note "could not remove $p" ;;
        root) run sudo rm -rf -- "$p" || note "could not remove $p" ;;
        *) run rm -rf -- "$p" || note "could not remove $p" ;;
      esac
    done
  else note "kept"; fi
}
_t=()
# Candidate build directories copied to the host for an acceptance run
# (~/tapectl-<version>-<commit>): superseded once this release is installed.
while IFS= read -r _p; do
  [ -n "$_p" ] || continue
  _v="$(basename "$_p" | sed -E 's/^tapectl-([0-9]+\.[0-9]+\.[0-9]+)-.*/\1/')"
  vercmp_ge "$NEW_VER" "$_v" || continue
  tidy_kept "$_p" || _t+=("$_p")
done < <(find "$HOME" -mindepth 1 -maxdepth 1 -type d -regextype posix-extended -regex '.*/tapectl-[0-9]+\.[0-9]+\.[0-9]+-[0-9a-f]{7,40}' 2>/dev/null | sort)
tidy_group "candidate build directories under $HOME" me ${_t[@]+"${_t[@]}"}
# Step 12's throwaway homes, all but the newest (this upgrade's rehearsal).
_t=()
while IFS= read -r _p; do
  [ -n "$_p" ] || continue
  tidy_kept "$_p" || _t+=("$_p")
done < <(find "$WORK_DIR/tapectl-lifecycle" -mindepth 1 -maxdepth 1 -type d -name 'run-*' 2>/dev/null | sort | head -n -1)
tidy_group "rehearsal homes under $WORK_DIR/tapectl-lifecycle (the newest is kept)" me ${_t[@]+"${_t[@]}"}
# Another Heir Kit under your home: its tapectl may not read newer tapes.
_t=()
while IFS= read -r _p; do
  [ -n "$_p" ] || continue
  _p="$(dirname "$_p")"
  tidy_kept "$_p" || _t+=("$_p")
done < <(find "$HOME" -mindepth 2 -maxdepth 2 -name COVER.txt 2>/dev/null | sort)
tidy_group "other Heir Kit directories (print and seal the one in ${KIT_OUT:-the kit directory} first)" me ${_t[@]+"${_t[@]}"}
# Catalog copies in a backup directory the daily backup no longer writes to.
_t=()
for _d in "$(ustate unit_backup_dir)" "$BACKUP_DIR_DEFAULT"; do
  [ -n "$_d" ] || continue
  [ "$(realpath -m "$_d")" != "$(realpath -m "$U_BDIR")" ] || continue
  as_svc test -d "$_d" 2>/dev/null || continue
  while IFS= read -r _p; do
    [ -n "$_p" ] || continue
    tidy_kept "$_p" || _t+=("$_p")
  done < <(as_svc find "$_d" -mindepth 1 -maxdepth 1 -type f -name 'tapectl-*.db' 2>/dev/null | sort)
done
tidy_group "catalog copies in a backup directory the daily backup no longer uses" svc ${_t[@]+"${_t[@]}"}
# Rollback binaries older than the newest one kept.
_t=()
_newest="$(find "$(dirname "$INSTALL_BIN")" -maxdepth 1 -type f -regextype posix-extended -regex '.*/tapectl-[0-9]+\.[0-9]+\.[0-9]+' -printf '%f\n' 2>/dev/null | sort -V | tail -n 1)"
while IFS= read -r _p; do
  [ -n "$_p" ] || continue
  [ "$(basename "$_p")" != "$_newest" ] || continue
  tidy_kept "$_p" || _t+=("$_p")
done < <(find "$(dirname "$INSTALL_BIN")" -maxdepth 1 -type f -regextype posix-extended -regex '.*/tapectl-[0-9]+\.[0-9]+\.[0-9]+' 2>/dev/null | sort -V)
tidy_group "rollback binaries older than ${_newest:-the newest}" root ${_t[@]+"${_t[@]}"}
ok "tidy done"
}

# ---------------------------------------------------------------- upgrade 13
uhdr 13 "The rollback"
_rb="$(ustate backup)"
explain <<EOF
Should $NEW_VER have to come out: with the timers stopped, import the backup step 2 took with $OLD_VER while $NEW_VER is still installed, then put the kept binary back and check the catalog with it. In that order: every tapectl command opens the catalog first, and $OLD_VER cannot open one $NEW_VER has migrated, so $OLD_VER's own import would be refused. Whatever the catalog recorded after that backup is lost, and $OLD_VER may refuse a tape $NEW_VER wrote in between.
EOF
_timers="tapectl-audit.timer tapectl-backup.timer tapectl-drive-poll.timer"
[ -n "$HOME_DIR" ] || printf '     sudo systemctl stop %s\n' "$_timers"
printf '     %s db import %s\n' "$U_OP" "${_rb:-<the backup from step 2: none is recorded in $U_STATE>}"
printf '     sudo cp -p %s %s\n' "$KEEP" "$INSTALL_BIN"
printf '     %s db fsck\n' "$U_OP"
[ -n "$HOME_DIR" ] || printf '     sudo systemctl start %s\n' "$_timers"
ustate_set finished "$(date -u +%FT%TZ)"
ok "upgraded to $TAG: $("$INSTALL_BIN" --version). Log: $LOG; record: $U_STATE"
exit 0
fi

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
tapectl shells out to `dar` for every archive (a hard dependency, >= 2.6, 2.7.20+ recommended) and to `mt` and the sg3-utils for drive control, health pages, the cartridge's MAM and, from 1.7.2, the drive's Logical Block Protection page (`sg_modes`, `sg_wr_mode`). `age` is not used by the binary itself — it uses the rage crate — but the on-tape RESTORE.sh, the heir path, needs it, and so does the rehearsal in step 12. `setfacl` (package acl) grants the service user read on your data in step 11. `lsscsi` and `python3` are for step 6 and the lifecycle suite.
EOF
MISSING=()
for t in mt sg_read_attr sg_logs sg_modes sg_wr_mode setfacl python3 lsscsi; do command -v "$t" >/dev/null 2>&1 || MISSING+=("$t"); done
if [ -n "$DAR_BIN" ]; then [ -x "$DAR_BIN" ] || die "--dar $DAR_BIN is not executable (on home2, contrib/hosts/home2-prep.sh builds it)"
else command -v dar >/dev/null 2>&1 || MISSING+=(dar); fi
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
    # sha256 of that tarball (#506), as home2-prep.sh pins its dar: its age and
    # age-keygen are byte for byte the ones installed on home2. A new AGE_TAG needs a new pin.
    AGE_SHA256="cbe24006683f8eb669266162894b9a522a1af52f2665fbc63a4bb032ed26ac10"
    TMPD="$(mktemp -d)"
    run curl -fsSL -o "$TMPD/age.tar.gz" "$AGE_URL" || { rm -rf "$TMPD"; die "download failed — check the tag at https://github.com/FiloSottile/age/releases"; }
    echo "$AGE_SHA256  $TMPD/age.tar.gz" | sha256sum -c - \
      || { rm -rf "$TMPD"; die "the downloaded age tarball does not match the pinned sha256 ($AGE_SHA256) — not installing it"; }
    if ! { mkdir "$TMPD/x" && tar -xzf "$TMPD/age.tar.gz" -C "$TMPD/x"; }; then rm -rf "$TMPD"; die "could not unpack the age tarball"; fi
    run sudo install -m 0755 "$TMPD/x/age/age" "$TMPD/x/age/age-keygen" /usr/local/bin/
    rm -rf "$TMPD"
    run age --version
  else note "continuing without age — the step-12 rehearsal will be unavailable until it is installed"; fi
fi
DARV="$("$DAR" --version </dev/null 2>&1 | sed -n 's/.*dar version \([0-9.]*\).*/\1/p' | head -1)"
[ -n "$DARV" ] || DARV="$("$DAR" --version </dev/null 2>&1 | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1)"
[ -n "$DARV" ] || die "could not read dar's version (dar --version)"
vercmp_ge "$DARV" "2.6.0" || die "dar $DARV is too old; tapectl needs >= 2.6"
vercmp_ge "$DARV" "2.7.20" || note "dar $DARV works; 2.7.20+ is recommended (bookworm ships 2.7.x)"
ok "dar $DARV ($DAR), mt, sg3-utils, setfacl, python3, lsscsi present$(command -v age >/dev/null 2>&1 && echo ", age present" || echo "; age ABSENT (heir-path script and rehearsal only)")"
}

# ================================================================ step 3
hdr 3 "Build tapectl"
skip_if || {
if [ "$SKIP_BUILD" = 1 ]; then note "--skip-build"; else
explain <<'EOF'
Production runs the RELEASE binary: the same source with optimisation on, and it matters here — staging hashes and encrypts every byte, and a debug build does that roughly ten times slower than release (a 20 GiB stage took 19 minutes on the release binary on this host, 2026-09-23; the debug attempt was abandoned). Whatever an earlier build proved, the binary you build now is a new artifact, which is why step 12 rehearses it on a test cartridge before step 13 trusts it with data. Install it to /usr/local/bin: the service user cannot execute a binary under your home.
EOF
if confirm "Build the release binary now (cargo build --release; a few minutes)?"; then
  build_release
  TAPECTL="$BUILT"
  if confirm "Install it to $INSTALL_BIN (sudo)?"; then run sudo install -m 0755 "$TAPECTL" "$INSTALL_BIN"; TAPECTL="$INSTALL_BIN"; fi
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
`cargo test` runs the ungated suite (about 4,600 tests), which needs no tape and no mhvtl — only dar. It proves this machine's dar, filesystem and toolchain behave the way the suite expects. Two to three minutes.
EOF
if confirm "Run cargo test now?"; then toolchain_check; ( cd "$REPO" && PATH="$(dar_path_prefix)$PATH" run flock -w 1200 -E 99 "$BUILD_LOCK" cargo test </dev/null ) || die "the suite is red on this machine (exit 99 = timed out waiting for the build lock) — stop here and look"; ok "suite green"; fi
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
  if [ -n "$SVC_HOME_WANT" ] && [ "$(svc_home)" != "$SVC_HOME_WANT" ]; then
    die "$SVC_USER's home is $(svc_home) but the profile wants $SVC_HOME_WANT — move it first (on home2: contrib/hosts/home2-prep.sh), then re-run with --from 5"
  fi
else
  SVC_HOME_NEW="${SVC_HOME_WANT:-/var/lib/$SVC_USER}"
  confirm "Create system user $SVC_USER (home $SVC_HOME_NEW, shell nologin)?" || die "no service user — re-run with --no-service-user to run tapectl as $USER"
  run sudo useradd --system --create-home --home-dir "$SVC_HOME_NEW" --shell /usr/sbin/nologin --comment "tapectl archival service" "$SVC_USER"
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
/dev/nstN numbers move across reboots on any host with more than one SCSI device. The stable name is the serial under /dev/tape/by-id/. tapectl's config stores that path. The drive's second node, /dev/sgN, is where health pages and the cartridge chip (MAM) are read; it moves too, so it is taken from the kernel's own binding of the tape node (sysfs), and tapectl re-checks that pairing every time it resolves the drive — `config check` warns and `volume write` refuses if the two nodes ever name different drives. Nothing in this step touches the tape.
EOF
run ls -l /dev/tape/by-id/ || note "(no /dev/tape/by-id — is a drive attached?)"
command -v lsscsi >/dev/null 2>&1 && run lsscsi -g || true
if [ -z "$DEVICE" ]; then
  # The default must be a REAL drive. mhvtl drives carry a `scsi-XYZZY_*`
  # link but also a WWN alias (`scsi-3502233...`) that sorts first, so
  # excluding the XYZZY spelling alone offered an mhvtl drive as the default
  # (found 2026-09-24). Exclude every link whose target an mhvtl link also
  # points at, and offer a default only when exactly one real drive remains.
  declare -A VIRT=(); for f in /dev/tape/by-id/*XYZZY*; do [ -e "$f" ] && VIRT["$(readlink -f "$f")"]=1; done
  REAL=(); for f in /dev/tape/by-id/*-nst; do
    [ -e "$f" ] || continue; t="$(readlink -f "$f")"; [ -n "${VIRT[$t]:-}" ] && continue
    case "$f" in *XYZZY*) continue ;; esac; REAL+=("$f")
  done
  DEF=""; if [ "${#REAL[@]}" -ge 1 ]; then
    # several links (serial, WWN) may name ONE drive; collapse by target
    declare -A SEEN=(); TGTS=0; for f in "${REAL[@]}"; do t="$(readlink -f "$f")"; [ -n "${SEEN[$t]:-}" ] || { SEEN[$t]=1; TGTS=$((TGTS+1)); }; done
    if [ "$TGTS" -eq 1 ]; then for f in "${REAL[@]}"; do case "$f" in *scsi-3*) ;; *) DEF="$f"; break ;; esac; done; DEF="${DEF:-${REAL[0]}}"
    else note "${TGTS} real drives found — no default; name the one you mean"; fi
  else note "no real (non-mhvtl) drive found under /dev/tape/by-id"; fi
  ask DEVICE "by-id path of the drive (…-nst, the non-rewinding node; the serial spelling, not the WWN)" "$DEF"
fi
[ -e "$DEVICE" ] || die "$DEVICE does not exist"
NST="$(readlink -f "$DEVICE")"; NSTN="$(basename "$NST")"
if [ -z "$SG" ]; then
  SGN="$(ls "/sys/class/scsi_tape/$NSTN/device/scsi_generic/" 2>/dev/null | head -1 || true)"
  [ -n "$SGN" ] && SG="/dev/$SGN"
  ask SG "matching SCSI-generic node (for health and MAM)" "$SG"
  SG_DEF="${SGN:+/dev/$SGN}"
  # A wrong answer is asked again, not fatal: %q shows what was actually read.
  while [ ! -e "$SG" ] && [ "$AUTO" != 1 ]; do
    note "$(printf '%q' "$SG") does not exist — press Enter for ${SG_DEF:-<none found>}, or type the node"
    SG=""; ask SG "matching SCSI-generic node (for health and MAM)" "$SG_DEF"
    [ -n "$SG" ] || break
  done
fi
[ -e "$SG" ] || die "$(printf '%q' "$SG") does not exist"
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
  [ -n "$ADOPT_KEYS_FROM" ] && note "the operator's keys are left as they are (ADOPT_KEYS_FROM applies to a fresh init only)"
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
    # `|| true`: config check exits non-zero on this config, and under pipefail
    # that ended the script here, before the question below (#506).
    { tc config check 2>&1 || true; } | head -20 | sed 's/^/     /'
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
    # Item 189: `confirm` says yes under --auto, and regenerating drops tables.
    [ "$AUTO" = 1 ] && die "config.toml cannot be loaded (above), and regenerating it drops its [[backends.lto]] and [[collections]] tables, so --auto never does it (ADR-0012 item 189): fix the key named above, or run step 7 interactively (scripts/first-run.sh --from 7, without --auto) to regenerate it"
    if confirm "Back up config.toml and write a fresh default one?"; then
      CFG_BAK="$EFFECTIVE_HOME/config.toml.superseded-$(date +%Y%m%d-%H%M%S)"
      as_svc cp "$EFFECTIVE_HOME/config.toml" "$CFG_BAK" || die "could not back up config.toml"
      # Generated through tapectl's own writer in a throwaway home, never a
      # template kept in this script: a hand-written default here would drift
      # the moment a config field is added. --no-escrow so the throwaway home
      # mints no identity and prints no secret (ADR-0005).
      CFG_TMP="$(as_svc mktemp -d)" || die "could not make a temp dir"
      as_svc "$TAPECTL" --home "$CFG_TMP" --config "$CFG_TMP/config.toml" init --no-escrow --operator config-template >/dev/null 2>&1 \
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
`tapectl init` creates the database, config and the operator tenant — and mints the permanent ESCROW IDENTITY (ADR-0005): the one key that is a recipient of every tape and is never rotated. Its SECRET half is printed ONCE, to your terminal, and stored nowhere on this machine. Have paper ready; write it down before you do anything else. The Heir Kit (step 9) prints only the PUBLIC half — keep the handwritten secret in the same sealed envelope as the printed cover sheet; together they are how an heir — or you, on a rebuilt machine — gets back in.

If you are REBUILDING a machine and already hold the original escrow key, do NOT let init mint a new one: answer with the original public key below and it is adopted instead (no command can replace a registered escrow identity later).

This step's output is deliberately NOT written to the log.
EOF
  ask OPERATOR "operator name" "${OPERATOR:-$USER}"
  ADOPT="$ESCROW_PUBLIC_KEY"
  # Item 188: an escrow identity can never be replaced (ADR-0005), so --auto
  # mints one only when the command line says so; a profile cannot (MINT_ESCROW
  # is not a profile variable).
  if [ -n "$ADOPT" ] && [ "$MINT_ESCROW" = 1 ]; then
    die "--mint-escrow and an escrow public key to adopt ($ADOPT, from ESCROW_PUBLIC_KEY or --escrow-public-key) contradict each other: pass one of them. Nothing was created"
  fi
  if [ -n "$ADOPT" ]; then note "adopting the escrow public key from ESCROW_PUBLIC_KEY: $ADOPT (no new escrow identity is minted)"
  elif [ "$AUTO" != 1 ]; then ask ADOPT "existing escrow PUBLIC key to adopt (age1…, or a .pub path) — leave empty to mint a new one" ""
  elif [ "$MINT_ESCROW" = 1 ]; then note "--mint-escrow: init mints a NEW escrow identity (none is given to adopt)"
  else
    die "this home is new and no escrow public key is given, so init would mint a NEW escrow identity, which --auto never does unless told to: no command replaces an escrow identity once it is registered (ADR-0005), so a minted one cannot be taken back. To keep an existing Heir Kit, set ESCROW_PUBLIC_KEY (the profile, or --escrow-public-key), or run step 7 without --auto and answer its question; to mint one, re-run with --mint-escrow (ADR-0012 item 188). Nothing was created"
  fi
  INIT_ARGS=(init --operator "$OPERATOR")
  [ -n "$ADOPT" ] && INIT_ARGS+=(--escrow-public-key "$ADOPT")
  if [ -n "$ADOPT_KEYS_FROM" ]; then
    # Issue #424: the operator keeps a key its holder already has. The dry
    # run validates the file and names the public key it will register,
    # creating nothing; the real init below repeats every check.
    explain <<EOF
ADOPTING THE OPERATOR KEY. ADOPT_KEYS_FROM is $ADOPT_KEYS_FROM, so init registers $OPERATOR.key from it as the operator's key ("$OPERATOR-primary") instead of minting a pair. It is copied byte for byte into the home's keys/, mode 0600. Check that the public key printed next is the one you expect (age-keygen -y on the original prints it).
EOF
    adopt_key_for "$OPERATOR"
    INIT_ARGS+=(--operator-key "$ADOPT_FILE")
    run tc --dry-run "${INIT_ARGS[@]}" || die "init refuses $ADOPT_FILE (read the reason above) — nothing was created"
  fi
  # Issue #437: a home pointer left from a deleted home names a directory that
  # no longer exists, and init would refuse on it. It is rewritten in step 7.
  _ptr="${TAPECTL_HOME_POINTER:-/etc/tapectl/home}"
  if [ -f "$_ptr" ] && ! sudo test -d "$(cat "$_ptr")"; then run sudo rm -f "$_ptr"; fi
  note "PAPER READY? The escrow secret appears exactly once, next."
  confirm "Run init now? (type y — Enter means no, nothing has been created)" || die "stopped before init: nothing was minted or written. Re-run with --from $STEP and answer y when the paper is ready"
  run_nolog tc "${INIT_ARGS[@]}"
  note "Written down? It will not be shown again."
fi
explain <<'EOF'
Two read-only checks close the step. `config check` parses config.toml the way every command will, names any key this version does not know (unknown keys are errors, never ignored), checks the drive's two device nodes exist and belong to one drive, and warns about staging space. `db fsck` runs SQLite's own integrity check (which covers every CHECK and NOT NULL constraint) plus a foreign-key check, and is the same check to run after any restore of the database from backup.
EOF
run tc config check || note "config check reported something — read it; advisory, exit code above"
run tc db fsck || true
explain <<'EOF'
STAGING SPACE. `stage create` writes every encrypted slice of a unit to the staging directory before anything goes to tape, so it needs room for everything one tape will hold: a sealed volume is never appended to, so every tape is written in one session, and the staging space caps what each tape can contain (2.5 TB for a full LTO-6, 2.3 TB after the planning margin; the free space printed below caps each tape, so plan smaller tapes or add storage). init writes a default path into config.toml that may not exist, or may not be the service user's to write to. Put staging on a filesystem with the space, owned by the user that runs tapectl.
EOF
CFG="$EFFECTIVE_HOME/config.toml"
SD="$(as_svc sed -n '/^\[staging\]/,/^\[/{s/^directory *= *"\(.*\)"/\1/p}' "$CFG" | head -1)"
SD_DEF="$SD"
# A profile's staging directory is the default even when the config's current
# one works: the profile is the decision about this host.
[ -n "$STAGING_DIR" ] && [ "$SD" != "$STAGING_DIR" ] && SD_DEF="$STAGING_DIR" && note "profile staging directory: $STAGING_DIR (config has '${SD:-<unset>}')"
if [ -z "$SD" ] || ! as_svc test -d "$SD" || ! as_svc test -w "$SD"; then
  note "staging.directory is '${SD:-<unset>}' — $( [ -z "$SD" ] && echo unset || { [ -d "$SD" ] && echo "not writable by $SVC_USER" || echo "does not exist"; } )"
  # The home is usually on the root filesystem (small on this VM: / is
  # 22 GB); a big writable filesystem is the right default when one exists.
  if [ -n "$STAGING_DIR" ]; then SD_DEF="$STAGING_DIR"
  elif [ -d /scratch ] && [ -w /scratch ] || sudo test -w /scratch 2>/dev/null; then SD_DEF="/scratch/tapectl-staging"; else SD_DEF="$EFFECTIVE_HOME/staging"; fi
  for cand in "$SD" "$SD_DEF"; do [ -n "$cand" ] && { d="$cand"; while [ ! -d "$d" ]; do d="$(dirname "$d")"; done; note "$cand: $(df -h "$d" | awk 'NR==2{print $4" free on "$6}')"; }; done
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
if [ -n "$STAGING_JOBS" ]; then
  # #368: units staged at once, measured per host (home2: 2, ADR-0012 2026-10-13).
  run tc config set staging.jobs "$STAGING_JOBS" || die "could not set staging.jobs = $STAGING_JOBS"
  ok "staging.jobs = $STAGING_JOBS (units staged at once)"
fi
if [ -n "$VERIFY_INTERVAL_DAYS" ]; then
  run tc config set defaults.verify_interval_days "$VERIFY_INTERVAL_DAYS" \
    || die "could not set defaults.verify_interval_days = $VERIFY_INTERVAL_DAYS"
  ok "defaults.verify_interval_days = $VERIFY_INTERVAL_DAYS (audit warns when a unit or a volume has not been read back in full for longer)"
fi
if declare -p CONTENDER_PROCESSES >/dev/null 2>&1 && [ "${#CONTENDER_PROCESSES[@]}" -gt 0 ]; then
  # The processes `host check` names before a write (ADR-0012 2026-10-13 item 55).
  CP_TOML="[$(printf '"%s",' "${CONTENDER_PROCESSES[@]}" | sed 's/,$//')]"
  run tc config set host_check.contender_processes "$CP_TOML" || die "could not set host_check.contender_processes = $CP_TOML"
  ok "host check names these processes before a write: ${CONTENDER_PROCESSES[*]}"
fi
if [ -n "$DAR_BIN" ]; then
  # [dar] binary: the dar every tapectl command runs, whatever PATH the caller
  # (sudo's secure_path, a systemd unit) happens to have.
  CUR_DAR="$(as_svc sed -n '/^\[dar\]/,/^\[/{s/^binary *= *"\(.*\)"/\1/p}' "$CFG" | head -1)"
  if [ "$CUR_DAR" != "$DAR_BIN" ]; then
    as_svc sed -i "/^\[dar\]/,/^\[/{s|^binary *= *\".*\"|binary = \"$DAR_BIN\"|}" "$CFG"
    as_svc grep -q "^binary = \"$DAR_BIN\"" "$CFG" || die "could not set [dar] binary in $CFG — edit it by hand"
  fi
  run tc config check || true
  ok "dar: $DAR_BIN"
fi
# Issue #393: the profile's OPS_GROUP lets the operator's own account (and an
# agent helping them) watch sessions with `tapectl status` without sudo. The
# group reads logs/ and nothing else in the home; the service user must be a
# member too, so tapectl can give the home and logs/ the group itself.
if [ -n "$OPS_GROUP" ] && [ "$SVC_MODE" = 1 ]; then
  explain <<EOF
THE OPS GROUP. Live progress prints only on the terminal that started a command, and the session logs under $EFFECTIVE_HOME/logs/ belong to $SVC_USER (0700), so your own account cannot see whether a write is running, which phase it is in, or how the last one ended without sudo. With [ops] group = "$OPS_GROUP", every tapectl command run as $SVC_USER keeps the home at 0710 and logs/ at 2750 owned by $OPS_GROUP, and writes each session log 0640: members can run a plain \`tapectl status\` (the pointer /etc/tapectl/home names the home) and read the logs, and open nothing else — every other entry of the home (the catalog, the keys, config.toml, …) is kept closed to the group. Group membership takes effect at the member's next login.
EOF
  if ! getent group "$OPS_GROUP" >/dev/null; then
    confirm "Create group $OPS_GROUP?" && run sudo groupadd --system "$OPS_GROUP"
  fi
  if getent group "$OPS_GROUP" >/dev/null; then
    for m in "$SVC_USER" "$USER"; do
      svc_in_group "$m" "$OPS_GROUP" || { confirm "sudo usermod -aG $OPS_GROUP $m ?" && run sudo usermod -aG "$OPS_GROUP" "$m"; }
    done
    if as_svc grep -qE '^\[ops\]' "$CFG"; then ok "[ops] is already in config.toml"
    else
      printf '\n[ops]\ngroup = "%s"\n' "$OPS_GROUP" | as_svc tee -a "$CFG" >/dev/null
      tc config show >/dev/null 2>&1 || die "config.toml does not load after adding [ops] — read: tapectl config check"
    fi
    # Any tapectl command run as the service user applies the modes; this one
    # also reports them.
    run tc config check || true
    # Issue #437 / ADR-0012 2026-10-15 item 62: a pointer so members need no
    # --home. tapectl ignores it as root and when the account has its own ~/.tapectl.
    POINTER="${TAPECTL_HOME_POINTER:-/etc/tapectl/home}"
    run sudo install -d -m 0755 "$(dirname "$POINTER")"
    printf '%s\n' "$EFFECTIVE_HOME" | sudo tee "$POINTER" >/dev/null
    run sudo chmod 0644 "$POINTER"
    ok "home pointer $POINTER -> $EFFECTIVE_HOME"
    # The home's 0710 is not enough on its own: every directory above it must
    # let the group through too. On home2 /srv/archive_meta is root 0750 with
    # traverse granted to the service user alone, so members got "permission
    # denied" while this step said it worked. Grant traverse (x only) where the
    # group lacks it, then check as the operator with the group, not by claim.
    p="$(dirname "$EFFECTIVE_HOME")"
    while [ "$p" != / ] && [ -n "$p" ]; do
      read -r pmode pgroup <<< "$(sudo stat -c '%A %G' "$p")"
      if [ "${pmode:9:1}" = x ] || [ "${pmode:9:1}" = t ] \
         || { [ "$pgroup" = "$OPS_GROUP" ] && { [ "${pmode:6:1}" = x ] || [ "${pmode:6:1}" = s ]; }; } \
         || sudo getfacl -p "$p" 2>/dev/null | grep -E "^group:$OPS_GROUP:..x" >/dev/null; then :
      else
        run sudo setfacl -m "g:$OPS_GROUP:x" "$p" || die "could not let $OPS_GROUP traverse $p (setfacl)"
      fi
      p="$(dirname "$p")"
    done
    # Checked without --home, as members will run it; sudo may leave HOME=/root,
    # so HOME is pinned to the operator's own. If that account has its own
    # ~/.tapectl, this exercises that home, not the pointer.
    OP_HOME="$(getent passwd "$USER" | cut -d: -f6)"
    if sudo setpriv --reuid="$(id -u "$USER")" --regid="$(getent group "$OPS_GROUP" | cut -d: -f3)" --clear-groups \
         env -u TAPECTL_HOME HOME="$OP_HOME" TAPECTL_HOME_POINTER="$POINTER" "$TAPECTL" status >/dev/null 2>&1; then
      ok "ops group $OPS_GROUP: \`tapectl status\` works for its members (checked as $USER with the group)"
    else
      note "could not run \`tapectl status\` as $USER with group $OPS_GROUP from here — after your next login, run it once; a \"permission denied\" names the directory that still blocks the group"
    fi
    # The check above gives itself the group (setpriv), so it passes in a shell
    # that does not have it. Say when this one does not (#506, handoff (h)).
    if svc_in_group "$USER" "$OPS_GROUP" && ! { id -nG 2>/dev/null || true; } | tr ' ' '\n' | grep -x "$OPS_GROUP" >/dev/null; then
      note "$USER is in $OPS_GROUP, but this shell is not: it started before the membership (tmux keeps its server's groups). Log in again, or restart tmux, for a plain \`tapectl status\`; until then: sg $OPS_GROUP -c 'tapectl status'"
    fi
  else
    note "no group $OPS_GROUP — the session logs stay readable by $SVC_USER only"
  fi
fi
ok "home ready at $EFFECTIVE_HOME"
}

# ================================================================ step 8
hdr 8 "Register the drive as a backend"
skip_if || {
CFG="$EFFECTIVE_HOME/config.toml"
if as_svc grep -q '^\[\[backends.lto\]\]' "$CFG" 2>/dev/null; then
  ok "a [[backends.lto]] entry already exists in $CFG"; run as_svc grep -A5 '^\[\[backends.lto\]\]' "$CFG" || true
  derive_nodes
  CFG_TAPE="$(as_svc sed -n 's/^device_tape *= *"\(.*\)"/\1/p' "$CFG" | head -1)"
  if [ -n "$CFG_TAPE" ] && [ "$(readlink -f "$CFG_TAPE" 2>/dev/null)" != "$NST" ]; then
    note "config's device_tape ($CFG_TAPE) is not the drive you chose ($DEVICE -> $NST); fix it with \`$OP_CMD config set 'backends.lto[NAME].device_tape' $DEVICE\` (NAME is the block's name), or re-run step 6 with the configured drive"
  fi
  run tc config check || note "config check reported something — read it (an sg/tape pairing warning here becomes a refusal at the write)"
else
  derive_nodes
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
  # config.toml and refuses every write until `config set` corrects it.
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
  # Issue #431: no device_sg line -- tapectl derives the sg node from device_tape
  # through sysfs every time the config loads, so an /dev/sgN renumbering never matters.
  run tc backend add --name "$BNAME" --device-tape "$DEVICE" --generation "$DGEN"
  run tc config check || true
fi
}

# ================================================================ step 9
hdr 9 "The Heir Kit"
skip_if || {
explain <<'EOF'
`key escrow-kit` writes four files: COVER.txt (the escrow key in retypable Bech32 plus instructions — the decades-scale artifact, readable with cat), escrow-kit.html (the same with a QR, for printing from a browser) and catalog.db.age (the whole catalog encrypted to the escrow key), and from 1.5.0 a fourth, tapectl-<version> (a copy of the installed binary, whose size and sha256 COVER.txt prints). Yours to do afterwards: PRINT COVER.txt, put the handwritten escrow SECRET from step 7 in the same envelope (the kit prints only the public key; without the secret it opens nothing), seal it tamper-evident, and keep copies in at least two independent failure domains. Generate it now, before any data — `audit` will remind you (escrow_kit_stale, a warning) after every write, which is the cue to regenerate with the real catalog.
EOF
ask KIT_OUT "kit output directory" "${KIT_OUT:-$HOME/heir-kit}"
if [ -f "$KIT_OUT/COVER.txt" ] && ! confirm "A kit already exists in $KIT_OUT — regenerate?"; then ok "keeping the existing kit"; else
  kit_prepare "$KIT_OUT"
  # kit_finish runs whatever the kit command did, so a failure never leaves
  # the kit directory owned by the service user (#469).
  kerc=0; run tc key escrow-kit --out "$KIT_OUT" || kerc=$?; kit_finish "$KIT_OUT"
  [ "$kerc" = 0 ] || die "key escrow-kit exited $kerc (above); fix that and re-run scripts/first-run.sh --from 9 --to 9"
  [ -r "$KIT_OUT/COVER.txt" ] || die "$KIT_OUT/COVER.txt is not readable by $USER"
fi
note "Print: $KIT_OUT/COVER.txt   (and/or open $KIT_OUT/escrow-kit.html and print)"
note "Seal in tamper-evident envelopes; two failure domains; refresh after each write session."
# Another kit left under your home (2026-10-20: a profile-less run wrote one to
# ~/heir-kit) carries an older tapectl, which may not read newer tapes. A note only.
KIT_REAL="$(realpath -m "$KIT_OUT" 2>/dev/null || printf '%s' "$KIT_OUT")"
while IFS= read -r other; do
  [ -n "$other" ] || continue
  od="$(dirname "$other")"
  [ "$(realpath -m "$od" 2>/dev/null || printf '%s' "$od")" = "$KIT_REAL" ] && continue
  note "another Heir Kit is in $od ($other): its tapectl may not read newer tapes (1.5.0's refuses every tape written from 1.6.0 on). Once the kit in $KIT_OUT is printed and in the envelopes, remove that one."
done < <(find "$HOME" -maxdepth 2 -name COVER.txt 2>/dev/null || true)
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
if [ "${#LOCATIONS[@]}" -gt 0 ]; then
  # The profile names every place; the first is the shelf beside the drive,
  # where step 13 records tape 1. Copy 2 goes to the second.
  for spec in "${LOCATIONS[@]}"; do
    LNAME="${spec%%|*}"; LDESC="${spec#*|}"; [ "$LDESC" = "$spec" ] && LDESC=""
    case " $EXISTING " in
      *" $LNAME "*) ok "location $LNAME already exists" ;;
      *) run tc location add "$LNAME" ${LDESC:+-d "$LDESC"} ;;
    esac
  done
  [ -n "$LOCATION" ] || LOCATION="${LOCATIONS[0]%%|*}"
  ok "tape 1 goes to $LOCATION"
else
ask LOCATION "location name" "${LOCATION:-${EXISTING%% *}}"; [ -n "$LOCATION" ] || ask LOCATION "location name" "home-rack"
case " $EXISTING " in
  *" $LOCATION "*) ok "location $LOCATION already exists" ;;
  *) ask LDESC "description" "the rack next to the drive"; run tc location add "$LOCATION" -d "$LDESC" ;;
esac
fi
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
if [ "${#TENANTS[@]}" -gt 0 ] || [ "${#COLLECTIONS[@]}" -gt 0 ]; then
  # The profile's tenants and collections. Idempotent: an existing tenant or
  # an already-configured collection is left alone, so --from 11 is safe.
  EXIST_T=" $(tc tenant list --json 2>/dev/null | python3 -c 'import json,sys
try:
  d=json.load(sys.stdin); rows=d if isinstance(d,list) else d.get("tenants",[]); print(" ".join(r.get("name","") for r in rows))
except Exception: pass' 2>/dev/null || true) "
  # Issue #424: with ADOPT_KEYS_FROM, every tenant still to be created needs
  # its file, checked for all of them before the first is added.
  if [ -n "$ADOPT_KEYS_FROM" ]; then
    for spec in "${TENANTS[@]}"; do
      TNAME="${spec%%|*}"
      case "$EXIST_T" in *" $TNAME "*) ;; *) adopt_key_for "$TNAME" ;; esac
    done
  fi
  for spec in "${TENANTS[@]}"; do
    TNAME="${spec%%|*}"; TDESC="${spec#*|}"; [ "$TDESC" = "$spec" ] && TDESC="$TNAME's data"
    case "$EXIST_T" in
      *" $TNAME "*) ok "tenant $TNAME already exists$( [ -n "$ADOPT_KEYS_FROM" ] && echo " — its keys are left as they are" )" ;;
      *)
        if [ -n "$ADOPT_KEYS_FROM" ]; then
          adopt_key_for "$TNAME"
          run tc --dry-run tenant add "$TNAME" -d "$TDESC" --adopt-key "$ADOPT_FILE" || die "tenant add $TNAME refuses $ADOPT_FILE (read the reason above) — nothing was created for it"
          run tc tenant add "$TNAME" -d "$TDESC" --adopt-key "$ADOPT_FILE" || die "tenant add $TNAME --adopt-key failed"
        else
          run tc tenant add "$TNAME" -d "$TDESC" || die "tenant add $TNAME failed"
        fi ;;
    esac
  done
  CFG="$EFFECTIVE_HOME/config.toml"
  for spec in "${COLLECTIONS[@]}"; do
    IFS='|' read -r CNAME CROOT CTEN CDEPTH <<< "$spec"
    CDEPTH="${CDEPTH:-1}"
    { [ -n "$CNAME" ] && [ -n "$CROOT" ] && [ -n "$CTEN" ]; } || die "COLLECTIONS entry '$spec' is not name|root|tenant|unit_depth"
    [ -d "$CROOT" ] || sudo test -d "$CROOT" || die "collection $CNAME: $CROOT is not a directory"
    grant_collection "$CROOT" "$CDEPTH" || die "$SVC_USER cannot read $CROOT — not configuring collection $CNAME"
    if as_svc grep -qE "^name *= *\"$CNAME\"" "$CFG"; then
      ok "collection $CNAME is already in config.toml"
    else
      # init no longer writes `collections = []` (issue #345), but a config
      # written by an older init still carries it, and a [[collections]] table
      # beside it is a duplicate key — so an empty one goes first if present.
      as_svc sed -i '/^collections *= *\[\] *$/d' "$CFG"
      printf '\n[[collections]]\nname       = "%s"\nroot       = "%s"\ntenant     = "%s"\nunit_depth = %s\n' \
        "$CNAME" "$CROOT" "$CTEN" "$CDEPTH" | as_svc tee -a "$CFG" >/dev/null
      tc config show >/dev/null 2>&1 || die "config.toml does not load after adding collection $CNAME — read: tapectl config check"
      ok "collection $CNAME: $CROOT (tenant $CTEN, a unit per folder at depth $CDEPTH)"
    fi
  done
  if [ "${#COLLECTIONS[@]}" -gt 0 ]; then
    run tc collection sync --dry-run || die "collection sync --dry-run failed"
    if confirm "Register those units now (collection sync writes a .tapectl-unit.toml into each folder)?"; then
      run tc collection sync --yes || die "collection sync failed"
    fi
  fi
  NT=$((NT + ${#TENANTS[@]}))
  if [ "$AUTO" = 1 ] || ! confirm "Add more tenants or units by hand?"; then ADD_MORE=0; fi
fi
while [ "$ADD_MORE" = 1 ]; do
  if [ "$NT" != 0 ] && [ "$AUTO" = 1 ]; then break; fi
  if [ "$NT" != 0 ] && ! confirm "Add a tenant?"; then break; fi
  ask TENANT "tenant name (letters, digits, dot, underscore, dash)" "${TENANT:-alice}"
  ask TDESC "description" "$TENANT's data"
  if [ -n "$ADOPT_KEYS_FROM" ] && as_svc test -r "$ADOPT_KEYS_FROM/$TENANT.key"; then
    run tc tenant add "$TENANT" -d "$TDESC" --adopt-key "$ADOPT_KEYS_FROM/$TENANT.key" || note "(already exists, or the key was refused — read above)"
  else
    [ -n "$ADOPT_KEYS_FROM" ] && note "no $ADOPT_KEYS_FROM/$TENANT.key — minting new keys for $TENANT"
    run tc tenant add "$TENANT" -d "$TDESC" || note "(already exists?)"
  fi
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
        elif ! printf '%s' "$WR" | grep -F "\"$UP\"" >/dev/null; then
          # a second adopted unit: append inside the existing list
          as_svc sed -i "s|^\(watch_roots *= *\[.*\)\]|\1, \"$UP\"]|" "$CFG" \
            || note "could not append $UP to watch_roots — add it by hand and run: tapectl unit discover"
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
Folder-per-unit alternative. Add a `[[collections]]` table to config.toml (a config written by an older init may carry `collections = []` near the top — delete that line first, or the file stops loading with a duplicate key; `config check` says so), then run `tapectl collection sync`:

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
hdr 12 "Rehearsal on a TEST cartridge with the installed binary (erases it)"
skip_if || {
explain <<'EOF'
Before real data, the lifecycle suite runs a whole simulated first year — write, verify, every restore path including the heir script off the tape — on a cartridge you are willing to lose. It ERASES that cartridge. It needs the cartridge's medium serial (the one its chip reports; `sg_read_attr` prints it below) typed exactly, and refuses if the loaded cartridge reports a different one. This is the step that proves the drive, the host's st driver and the binary step 13 will use agree — it runs the INSTALLED binary (TAPECTL_BIN), not a build of its own — and it is required before the first production write (ADR-0012, ruled 2026-09-23).

It runs as YOU, not the service user, with throwaway homes under the work directory (--work-dir; /scratch on the dev VM); it only needs your login to be able to open the drive. It takes about three minutes on an LTO-6.
EOF
derive_nodes
if ! command -v age >/dev/null 2>&1; then note "skipped: the rehearsal runs RESTORE.sh off the tape, which needs the age CLI (step 2 explains how to install it) — step 13 will ask you to confirm writing without it"
elif [ "$AUTO" = 1 ] && [ -z "$TEST_BARCODE" ]; then note "skipped under --auto: no --barcode given (erasing a cartridge is never a default)"
elif confirm "Run the first-year rehearsal on a TEST cartridge now?"; then
  drive_poll_pause
  DEVGRP="$(stat -c %G "$NST")"; WRAP=()
  if ! { [ -r "$NST" ] && [ -w "$NST" ] && [ -r "$SG" ] && [ -w "$SG" ]; }; then
    note "$USER cannot open $NST and $SG ($(stat -c '%A %U:%G' "$NST"); $(stat -c '%A %U:%G' "$SG")) — the suite needs both (tape I/O, and MAM/health through sg)"
    svc_in_group "$USER" "$DEVGRP" || { confirm "sudo usermod -aG $DEVGRP $USER (takes effect at your next login)?" && run sudo usermod -aG "$DEVGRP" "$USER"; }
    svc_in_group "$USER" "$DEVGRP" || die "$USER is not in group $DEVGRP — cannot run the rehearsal"
    WRAP=(sg "$DEVGRP" -c)   # this login predates the membership; sg opens a shell with it now
  fi
  # Step 12 runs as you: read the chip directly when your login can open the
  # SG node (the tape group can on home2), sudo only when it cannot. On a host
  # whose sudo asks for a password, an unattended run used to hang here.
  if [ -n "$SG" ]; then
    if [ -r "$SG" ] && [ -w "$SG" ]; then run sg_read_attr "$SG" | grep -iE "Medium serial|manufacturer" || true
    else run sudo sg_read_attr "$SG" | grep -iE "Medium serial|manufacturer" || true; fi
  fi
  ask TEST_BARCODE "medium serial of the TEST cartridge in the drive, as printed above (it will be erased)" "$TEST_BARCODE"
  [ -n "$TEST_BARCODE" ] || die "no barcode given"
  if confirm_destructive "ERASE $TEST_BARCODE and run the rehearsal" "$TEST_BARCODE" "$BARCODE_FROM_FLAG"; then
    # ADR-0012 item 192 (#505): never a cartridge this catalog knows. The suite
    # erases the cartridge it is given, and checks only that the serial typed is
    # the one loaded; a production copy typed by mistake would pass that. The
    # check is fail-closed: a catalog that cannot be read refuses the rehearsal.
    KNOWN_CART=""
    KNOWN_CART="$(known_cartridge "$TEST_BARCODE")" \
      || die "cannot read this catalog's cartridges ($OP_CMD cartridge list), so nothing proves $TEST_BARCODE is not one of them; the rehearsal was not run"
    if [ -n "$KNOWN_CART" ]; then
      log "step 12 refused $TEST_BARCODE: catalog cartridge $KNOWN_CART"
      die "$TEST_BARCODE is a cartridge this catalog knows ($OP_CMD cartridge info $KNOWN_CART): the rehearsal erases the cartridge it is given, so it never takes one the catalog has recorded. Nothing was erased. Load a test cartridge this catalog has never seen, then re-run step 12"
    fi
    ok "$TEST_BARCODE is not a cartridge this catalog knows"
    # /usr/sbin:/sbin: Debian installs mtx there and leaves both off a normal
    # user's PATH; the suite's precondition check wants mtx even on a real
    # drive (home2, 2026-09-28).
    CMD="cd '$REPO' && PATH='$(dar_path_prefix)$PATH:/usr/sbin:/sbin' TAPECTL_LIFECYCLE_OUT='$WORK_DIR/tapectl-lifecycle' TAPECTL_BIN='$TAPECTL' bash scripts/lifecycle-suite.sh --scenario first-year --device '$DEVICE' --erase short --single-cartridge --i-will-lose-the-cartridge '$TEST_BARCODE'"
    if [ "${#WRAP[@]}" -gt 0 ]; then run "${WRAP[@]}" "$CMD"; else run bash -c "$CMD"; fi || die "rehearsal RED — do not write real data until this is understood"
    ok "rehearsal green"
    # B14: step 13 checks for this marker (per binary) before the first write.
    mkdir -p "$(dirname "$LOG")"; printf '%s %s %s\n' "$(date -u +%FT%TZ)" "$TEST_BARCODE" "$("$TAPECTL" --version 2>/dev/null)" > "$(dirname "$LOG")/rehearsal-ok-$(sha256sum "$TAPECTL" | cut -c1-16)"
    note "Eject the test cartridge ($( [ "$SVC_MODE" = 1 ] && printf 'sudo -u %s -H ' "$SVC_USER" )tapectl drive eject --device $DEVICE) and load the production one before step 13."
  fi
else note "skipped"; fi
# The step's end, whatever happened in it: the poll timer runs again (item 190).
drive_poll_resume
}

# ================================================================ step 13
hdr 13 "The first production tape"
skip_if || {
NV="$(tc catalog stats --json 2>/dev/null | python3 -c 'import json,sys
try: print(json.load(sys.stdin).get("volumes",0))
except Exception: print(0)' 2>/dev/null || echo 0)"
[ "$NV" != 0 ] && note "$NV volume(s) already in the catalog." && { confirm "Write another tape now, or finish one an interrupted run left (same label)?" || { ok "nothing to do"; FROM=14; }; }
if [ "$FROM" -le 13 ]; then
explain <<'EOF'
The pipeline is three phases: `snapshot create` walks the unit and records what exists; `stage create` hashes every source file, runs dar, and encrypts the archive once (age: one random file key per slice, wrapped for each recipient in a small header) into slices in staging; `volume write` plans the whole tape first — every file, position and size — then writes it in one session and, before it seals, reads EVERY byte back and checks it against the plan (confirm). A sealed volume is immutable: there is no append. `volume verify --full` afterwards is a second, independent full read — the first entry in this tape's verification history, which `report verify-status` and the audit build on; on a full tape it takes as long as the write did.

`volume init` also reads the loaded cartridge: its generation from the density code, which fixes this tape's capacity and is checked against what the drive can write, and its medium serial, which binds the volume to a cartridge in the catalog (ADR-0010). Nothing to set for a mixed LTO-5/LTO-6 shelf — each tape is planned against its own size.

Label convention: something you can write on the cartridge, e.g. L6-0001.
EOF
  derive_nodes
  # B14: the rehearsal on THIS binary is required before the first production
  # write (ADR-0012, 2026-09-23). A marker per binary hash; interactive
  # override only, never under --auto.
  if [ ! -e "$(dirname "$LOG")/rehearsal-ok-$(sha256sum "$TAPECTL" | cut -c1-16)" ]; then
    note "no green step-12 rehearsal is recorded for this binary ($TAPECTL)"
    [ "$AUTO" = 1 ] && die "the rehearsal is required before the first production write — run step 12 (or re-run with --from 12)"
    confirm "Write to a production cartridge WITHOUT a rehearsal on this binary?" || die "run step 12 first: scripts/first-run.sh --from 12 --to 12"
  fi
  # Staging, the write and its confirm read run for hours to days (issue #326: ~56 MB/s
  # write, ~16 MB/s confirm). A dropped SSH session kills it: a clean abort to
  # an unsealed tape, but the hours are gone.
  if [ -z "${TMUX:-}" ] && [ -z "${STY:-}" ] && [ -n "${SSH_CONNECTION:-}" ]; then
    note "this shell is an SSH session outside tmux/screen — if the connection drops, the write dies with it"
    [ "$AUTO" = 1 ] || confirm "Continue anyway? (safer: Ctrl-C, start tmux, re-run with --from 13)" || die "stopped: re-run inside tmux with --from 13"
  fi
  # The profile's first location is the shelf beside the drive; the catalog's
  # own list is sorted by name, so its first row can be the offsite one.
  [ -z "$LOCATION" ] && [ "${#LOCATIONS[@]}" -gt 0 ] && LOCATION="${LOCATIONS[0]%%|*}"
  if [ -z "$LOCATION" ]; then
    LOCATION="$(tc location list --json 2>/dev/null | python3 -c 'import json,sys
try:
  d=json.load(sys.stdin); rows=d if isinstance(d,list) else d.get("locations",[]); print(rows[0].get("name","") if rows else "")
except Exception: pass' 2>/dev/null || true)"
  fi
  tape_lock
  explain <<'EOF'
A QUIET HOST WHILE THE TAPE RUNS. The drive streams at up to 160 MB/s and stops and restarts (wasting tape and time) whenever the host feeds it slower than about 54 MB/s. The write and the verify below each read every byte through this machine, for as long as the data takes. Before you confirm: stop or pause anything on this host that competes for CPU, memory or the staging disk — CI runners and their timers, container builds, other backups. The host's known contenders are the units listed below (CONTENDER_UNITS in its profile); on the dev VM they were the homorg runner's timers, and the end-of-tape fill on 2026-09-24 only ran clean because they were paused. Nothing else should touch the drive: every tapectl harness takes /tmp/tapectl-tape.lock, and so does this step. To help, this step raises its own CPU and I/O priority once (sudo renice and ionice: one password prompt; TAPE_NICE and TAPE_IONICE in the profile; ADR-0012 2026-10-15 item 58), so the tape commands it starts run at that priority; that outranks other work on the host, it does not replace the quiet.

`tapectl host check` measures this (load, free memory, memory and I/O pressure, contender processes and units) and runs just before the WRITE confirmation below; you can run it yourself any time.
EOF
  # Issue #428: tapectl's own read-only look at the drive (one O_NONBLOCK MTIOCGET,
  # the chip and the catalog's record of that cartridge; never moves the tape).
  run tc drive status --device "$DEVICE" || true
  # `|| true`: drive status exits 2 for an absent or unreadable tape node, and
  # under pipefail that would end this script before the note below (#469).
  CART_STATE="$(tc drive status --device "$DEVICE" --json 2>/dev/null | python3 -c 'import json,sys
try: print(json.load(sys.stdin)["tape_node"]["cartridge"] or "")
except Exception: pass')" || true
  [ "$CART_STATE" = "empty" ] && die "no cartridge loaded in $DEVICE"
  [ "$CART_STATE" = "loaded" ] || note "drive status could not confirm a loaded cartridge (${CART_STATE:-no answer}); volume init refuses an empty drive on its own"
  ask LABEL "volume label" "${LABEL:-L6-0001}"
  # Issue #414: re-entering this step after an interrupted run must pick up
  # where that run stopped, not die. What $LABEL already is in the catalog
  # decides what is left: nothing (the whole path below), an initialised but
  # unwritten volume (the write), an interrupted write (`volume resume`), or a
  # sealed volume (only the post-write block). `volume info`'s own "not
  # found" refusal is the only thing read as "absent", so a busy or broken
  # catalog stops here rather than starting a second write.
  VSTATE=absent
  VINFO="$(dirname "$LOG")/volume-info-$LABEL.json"
  VINFO_ERR="$(dirname "$LOG")/volume-info-$LABEL.err"
  if tc volume info "$LABEL" --json >"$VINFO" 2>"$VINFO_ERR"; then
    VSTATE="$(volume_step13_state "$VINFO")"
  elif ! grep "volume not found" "$VINFO_ERR" >/dev/null; then
    cat "$VINFO_ERR" >&2
    die "could not read volume $LABEL from the catalog (above) — fix that, then re-run scripts/first-run.sh --from 13"
  fi
  log "step 13: volume $LABEL is $VSTATE"
  tape_raise_setup
  # The quiet-host check (ADR-0012, 2026-09-24 amendment, item 7), run right
  # before the WRITE or RESUME confirmation: staging's own dar load is over by
  # then, and what matters is the host the tape I/O will run on. `host check`
  # reads /proc and systemd only and exits 1 when anything tripped; it never
  # refuses anything. The two homorg timers are passed with --unit because
  # unit names are host-specific and tapectl checks none by default; on a host
  # without them systemd reports them not-found, which counts as quiet.
  quiet_host_check() {
    HC_OUT="$(dirname "$LOG")/host-check.out"
    # Unset = the dev VM's homorg timers (the historical default); a profile
    # that declares CONTENDER_UNITS, even empty, replaces the list.
    # `declare -p`, not ${CONTENDER_UNITS+set}: the latter is empty for a
    # declared-but-empty array, so a profile's `CONTENDER_UNITS=()` read as unset
    # and brought the dev VM's timers back (found by the 2026-09-28 mhvtl run).
    if ! declare -p CONTENDER_UNITS >/dev/null 2>&1; then CONTENDER_UNITS=(homorg-db-suite.timer homorg-prune-target.timer); fi
    HC_ARGS=(); for u in "${CONTENDER_UNITS[@]}"; do HC_ARGS+=(--unit "$u"); done
    while ! run_capture "$HC_OUT" tc host check "${HC_ARGS[@]}"; do
      explain <<'EOF'
THE HOST IS NOT QUIET. Each "host check:" line above names what tripped, what it measured and the limit it crossed. A busy host does not stop the write — it costs tape (the drive stops and restarts below ~54 MB/s of feed; a bursty feed used 48% more tape on this drive) and risks the session (a process killed for memory mid-write is a clean abort, but the hours are gone). Pause what is named, then check again. The limits are [host_check] in config.toml (docs/operator-guide.md, "A quiet host while the tape runs").
EOF
      TRIPPED=(); for u in "${CONTENDER_UNITS[@]}"; do grep -qF "unit $u" "$HC_OUT" && TRIPPED+=("$u"); done
      if [ "${#TRIPPED[@]}" -gt 0 ]; then
        note "pause them for the write:               sudo systemctl stop ${TRIPPED[*]}"
        note "and start them again once it is sealed:  sudo systemctl start ${TRIPPED[*]}"
      fi
      if [ "$AUTO" = 1 ]; then note "--auto: proceeding with the host NOT quiet — the findings are above and in $LOG"; break; fi
      ask HC_ANS "press Enter to check again once the host is quiet, or type 'write' to proceed as it is (Ctrl-C stops)" ""
      [ "$HC_ANS" = write ] && { log "host check: operator chose to write on a host that was not quiet"; break; }
    done
  }
  case "$VSTATE" in
    absent|fresh) ;;
    resume)
      explain <<'EOF'
THIS VOLUME HAS AN INTERRUPTED WRITE. A run of this step (or a `volume write` by hand) started writing it and stopped before it was sealed and confirmed — a Ctrl-C, a dropped session, a crash. `volume write` refuses it; `volume resume` continues that same session from its frozen staging files, after checking that the loaded tape is the one it was writing (File 0's identity) and that the tape is not already sealed. A tape whose seal was written but whose confirm did not finish is re-confirmed, never written again. The SAME cartridge must be in the drive. Nothing is staged again: the snapshot and staging phases below are skipped.
EOF
      quiet_host_check
      confirm "Resume the interrupted write of $LABEL on the cartridge in $DEVICE?" || die "stopped before resuming — re-run scripts/first-run.sh --from 13 --label $LABEL when ready"
      run tc volume resume "$LABEL" --device "$DEVICE" --keep-staging || die "resume did not seal and confirm $LABEL — read the output; the catalog knows exactly why. Re-run scripts/first-run.sh --from 13 --label $LABEL once it is fixed"
      ;;
    live)
      die "volume $LABEL has a write session in progress RIGHT NOW (another process is writing it — tapectl turns a crashed session into an interrupted one whenever it opens the catalog). Find that process and let it finish; do not start a second writer"
      ;;
    planned)
      die "volume $LABEL has a write session that was planned and never reached the tape. Clear it with \`$OP_CMD volume abort $LABEL\`, then re-run under a NEW label: scripts/first-run.sh --from 13 --label <new>. This label cannot be written again once its session is aborted. The cartridge's File 0 still names $LABEL, so the new label's init asks before overwriting it, and marks $LABEL erased"
      ;;
    sealed)
      ok "volume $LABEL is already sealed — going on to the verify, audit, shelf and Heir Kit steps it is still owed"
      ;;
    *)
      VSTAT="$(python3 -c 'import json,sys
try:
  d=json.load(open(sys.argv[1])); print("%s, condition %s" % (d.get("status","?"), d.get("condition","?")))
except Exception: print("unknown")' "$VINFO" 2>/dev/null || echo unknown)"
      if [ "$(volume_aborted_seal_recorded "$VINFO")" = yes ]; then
        # ADR-0012, 2026-09-23: a session aborted after its seal was written
        # is re-confirmed, not abandoned. A new label would leave a sealed
        # cartridge that counts for nothing.
        explain <<'EOF'
THIS VOLUME'S WRITE SESSION WAS ABORTED AFTER ITS SEAL WAS WRITTEN. The cartridge is sealed, so it is never written again (ADR-0003) — and it does not need to be: `volume resume` re-confirms an aborted session whose seal is recorded, once a clean FULL verify has been recorded after the abort, and the volume then counts as a copy like any other. Keep its staged slices in place: the resume checks the tape against them. Load the SAME cartridge, run the two commands below, then re-run this step with the same label; it finds the volume sealed and carries on with the steps after the write.
EOF
        die "volume $LABEL ($VSTAT): run \`$OP_CMD volume verify $LABEL --device $DEVICE --full\`, then \`$OP_CMD volume resume $LABEL --device $DEVICE\`, then scripts/first-run.sh --from 13 --label $LABEL"
      fi
      die "volume $LABEL already exists ($VSTAT) and is neither unwritten, interrupted nor sealed — \`$OP_CMD volume info $LABEL\` shows its history. Choose a new label: scripts/first-run.sh --from 13 --label <new>"
      ;;
  esac
if [ "$VSTATE" = absent ] || [ "$VSTATE" = fresh ]; then
  explain <<'EOF'
THE CARTRIDGE. There is nothing to register and nothing to type. A cartridge is known by the serial its chip reports, and a barcode is a sticker (ADR-0012) — so `volume init` reads that serial, registers the cartridge itself, and wears the serial as a placeholder barcode until you replace it. Put the sticker on whenever you like, before or after this write, with `cartridge relabel`; the command is printed below once the cartridge is registered. Do not register it by hand first WITHOUT its serial: init matches on the serial, finds no row carrying it, and registers a second cartridge — two rows for one tape, with your label on the one the catalog is not using. (`cartridge register --serial <medium serial>` is matched; a bare barcode is not.)
EOF
  UNITS="$(tc unit list --json 2>/dev/null | python3 -c 'import json,sys
try:
  d=json.load(sys.stdin); rows=d if isinstance(d,list) else d.get("units",[]); print("\n".join(r.get("name","") for r in rows))
except Exception: pass' 2>/dev/null || true)"
  [ -n "$UNITS" ] || die "no units registered — run with --from 11"
  printf '   units:\n%s\n' "$(printf '%s\n' "$UNITS" | sed 's/^/     /')"
  # Issue #269: this used to be ONE confirm covering both phases, and then a
  # single loop doing snapshot+stage per unit. Two problems on production day.
  #
  # The operator agreed to the EXPENSIVE phase (hashing, dar and one encryption
  # pass over the data, hours and a lot of staging disk) before anything had told
  # them how much data that was -- and the authoritative capacity check is the
  # pre-flight gate inside `volume write`, which runs AFTER all of it. Too much
  # source for the cartridge meant discovering it at the end.
  #
  # And there was no way back out: with nothing yet written, `staging clean`'s
  # non-force branch requires a completed `writes` row, so the staged bytes are
  # not even cleanable without `--force`.
  #
  # So the phases are split. `snapshot create` is the cheap metadata walk and
  # prints each unit's size as it goes; the operator sees those numbers and
  # THEN decides about staging.
  confirm "Snapshot EVERY unit above? (metadata only -- no archiving yet)" || die "stopped"
  while IFS= read -r u; do
    [ -z "$u" ] && continue
    run tc snapshot create "$u" || die "snapshot failed for $u"
  done <<< "$UNITS"
  explain <<'EOF'
Those are the sizes `snapshot create` recorded, before compression and encryption. Staging is the expensive phase — it reads and hashes every file, runs dar over every unit, and encrypts each archive once (the recipients only add a few hundred bytes of header each, not another pass), writing the slices to your staging directory. That is several full passes over the data. If that total looks wrong for the cartridge you have loaded, stop here: nothing has been archived yet, and stopping now costs you only the metadata walk. To do a subset instead, answer no and run `tapectl stage create <unit>` for the ones you want, then re-run this step with --from 13. `[staging] jobs` in config.toml (1 unless you set it) is how many units are staged at once; the largest start first.
EOF
  confirm "Stage them now? (runs dar + encryption, writes to staging)" || die "stopped before staging"
  # Issue #283: this loop MUST tolerate a unit that is already staged. The
  # explain block directly above tells the operator they may stage a subset
  # by hand and re-enter with `--from 13` -- and `stage create` refuses a unit
  # whose latest snapshot is no longer `'created'`, so following that advice
  # used to abort the script right here. There was then NO path through step
  # 13 that reached the write: answering "yes" died on the first
  # already-staged unit, and answering "no" died at the confirm above. A
  # re-entrant step has to tolerate the state its own documented recovery
  # path creates.
  #
  # The skip is deliberately narrow: ONLY when `stage create`'s refusal is
  # the no-unstaged-snapshot one AND that unit already has a `'staged'` set.
  # Any other failure still aborts, because "already done" and "broken" must
  # not look the same on production day.
  staged_already() { # staged_already <unit>
    local f; f="$(dirname "$LOG")/stage-list-staged.json"
    tc stage list --status staged --json >"$f" 2>/dev/null || return 1
    # `stage list --json` emits a list of StageRow; the unit's name is the
    # "unit" key (verified against `stage_rows_to_json`, not assumed -- the
    # lifecycle suite lost a whole check once to a guessed --json key).
    python3 - "$f" "$1" <<'PY'
import json, sys
try:
    rows = json.load(open(sys.argv[1]))
except Exception:
    sys.exit(1)
sys.exit(0 if any(r.get("unit") == sys.argv[2] for r in rows) else 1)
PY
  }
  has_unstaged() { # has_unstaged <unit>: a `created` snapshot waits to be staged
    local f; f="$(dirname "$LOG")/snapshot-list-created.json"
    tc snapshot list --unit "$1" --status created --json >"$f" 2>/dev/null || return 1
    python3 - "$f" <<'PY'
import json, sys
try:
    rows = json.load(open(sys.argv[1]))
except Exception:
    sys.exit(1)
sys.exit(0 if rows else 1)
PY
  }
  # Issue #368: every unit still to stage goes to ONE `stage create`, so
  # `[staging] jobs` in config.toml can stage several at once (largest
  # first). The #283 skip is as narrow as it was: a unit is left out only
  # when it has no unstaged snapshot AND already has a staged set. Any other
  # unit is passed on, and `stage create` refuses a unit it cannot stage by
  # name, before it stages anything.
  TO_STAGE=()
  while IFS= read -r u; do
    [ -z "$u" ] && continue
    if ! has_unstaged "$u" && staged_already "$u"; then
      note "$u is already staged -- skipping it (staged by hand, or by an earlier run of this step)"
      continue
    fi
    TO_STAGE+=("$u")
  done <<< "$UNITS"
  if [ "${#TO_STAGE[@]}" -gt 0 ]; then
    so="$(dirname "$LOG")/stage-create.out"
    run_capture "$so" tc stage create "${TO_STAGE[@]}" \
      || die "staging failed (see $so) -- the units it staged stay staged, and re-running this step skips them"
  fi
  explain <<'EOF'
`staging status` lists what is staged and what it weighs; `volume plan` totals it against the drive's generation and estimates how many tapes it needs. It is an ESTIMATE (it says so): the authoritative figure is the capacity `volume init` records once it has detected the loaded cartridge, and the gate that refuses an over-full plan is inside `volume write`, before any byte is written. If the estimate already says more than one tape, stage fewer units now — nothing has been written yet.
EOF
  run tc staging status || true
  # `volume plan` reads the staged stage_sets and totals their encrypted size
  # against the drive -- the authoritative estimate, and the script never ran
  # it. This is the last point before the destructive confirm at which the
  # operator can see what is about to be written and how much of the tape it
  # uses (issue #269).
  run tc volume plan || true
  # ADR-0012, 2026-09-24 amendment, item 7: the quiet-host rule is CHECKED
  # here, not only stated in the explain block above (`quiet_host_check`).
  quiet_host_check
  confirm_destructive "WRITE volume $LABEL to the cartridge in $DEVICE (the cartridge's current contents are overwritten)" "$LABEL" "$LABEL_FROM_FLAG" || die "stopped before writing"
  # Capture init's output as well as logging it: two later steps read it back
  # -- the placeholder barcode it reports, and which refusal it gave.
  INIT_OUT="$(dirname "$LOG")/volume-init-$LABEL.out"
  if [ "$VSTATE" = fresh ]; then
    # Issue #414: init already ran (an earlier run of this step stopped
    # between it and the write). Nothing to re-run; the write's own contact
    # check is what proves the loaded tape is this volume's cartridge.
    note "volume $LABEL is already initialised and unwritten — continuing to the write"
    : >"$INIT_OUT"
  elif ! run_capture "$INIT_OUT" tc volume init "$LABEL" --device "$DEVICE"; then
    if grep -q "already exists" "$INIT_OUT"; then
      # Re-entering after an interrupted run: the volume row exists. Only an
      # `initialized` one (init done, nothing written) may go straight to the
      # write; anything else needs a new label.
      VSTAT="$(tc volume list --json 2>/dev/null | python3 -c 'import json,sys
try:
  print(next((v.get("status","") for v in json.load(sys.stdin) if v.get("label")==sys.argv[1]), ""))
except Exception: print("")' "$LABEL" 2>/dev/null || true)"
      if [ "$VSTAT" = "initialized" ]; then note "volume $LABEL is already initialised and unwritten — continuing to the write"
      else die "volume $LABEL already exists (status: ${VSTAT:-unknown}) — choose a new label: scripts/first-run.sh --from 13 --label <new>"; fi
    elif grep -q "cannot tell which physical cartridge" "$INIT_OUT"; then
      # ADR-0012: with no readable serial the operator must name the cartridge,
      # and that is their word -- there is no default to fall back on and no
      # flag that could supply one, so --auto stops here rather than guessing.
      explain <<'EOF'
This drive reports no medium serial, so tapectl cannot tell which physical cartridge is loaded and will not guess (ADR-0012). Name it: the barcode you give becomes the recorded identity, and the tape itself will say so — File 0 records `cartridge_identity_source = "operator"` rather than "mam", so anyone reading this tape later can tell a chip-verified serial from a label somebody typed.
EOF
      [ "$AUTO" = 1 ] && die "volume init could read no medium serial; naming a cartridge is your word, re-run scripts/first-run.sh --from 13 interactively"
      ask CARTRIDGE "no serial readable — name this cartridge (the barcode on its sticker)"
      [ -n "$CARTRIDGE" ] || die "no cartridge named"
      if ! tc cartridge list --json 2>/dev/null | grep "\"$CARTRIDGE\"" >/dev/null; then
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
    elif grep -q "is not the drive device_tape" "$INIT_OUT"; then
      # Issue #329: the config's device_sg is not the sg node the kernel
      # binds to device_tape (numbering moved since step 8). A config fact,
      # not a consent question -- --force would fail identically.
      explain <<'EOF'
volume init refused before touching the tape: the drive's two device nodes in config.toml no longer name the same drive (/dev/sgN numbering moved, usually after a reboot). The refusal above names the sg node the kernel binds to the tape node. Remove the device_sg line from [[backends.lto]] in config.toml and tapectl derives it from device_tape (issue #431), or set device_sg to that node (`tapectl config show` prints it; `tapectl config check` confirms the fix), then re-run with --from 13.
EOF
      die "volume init refused: device_sg does not belong to device_tape — fix the [[backends.lto]] entry, then re-run scripts/first-run.sh --from 13"
    elif grep -q "device_sg is not set" "$INIT_OUT"; then
      # Issue #431: no device_sg line and none derivable from device_tape (the
      # drive is not attached, or device_tape is not a /dev/nstN tape node).
      die "volume init refused: no sg node can be derived from device_tape $DEVICE — attach the drive (lsscsi -g), or set device_sg in [[backends.lto]], then re-run scripts/first-run.sh --from 13"
    elif grep -F "already carries a SEALED volume" "$INIT_OUT" >/dev/null; then
      # The SEALED case (issue #223): --force does not reach it, so it is never
      # offered. What does is `cartridge erase` (ADR-0012 2026-10-12 items
      # 51-52): it identifies the tape read-only, refuses a volume THIS catalog
      # still holds (naming `volume retire`), takes the label File 0 carries
      # as consent, erases, reads the tape back and records it. Offered here,
      # after a dry run that shows what the tape holds; then init runs again.
      explain <<'EOF'
volume init was refused by an ADR-0003 rule — the loaded cartridge already carries a SEALED volume. A sealed volume is immutable, there is no append, and --force does not reach it. The likeliest case right after step 12 is that the TEST cartridge is still in the drive: then do NOT erase anything here — unload it, load the production cartridge, and re-run with --from 13. If this IS the cartridge to reuse (a tape from an old catalog, as after a fresh start), `tapectl cartridge erase` erases it: it reads the tape first and shows what it holds (below), refuses a volume this catalog still counts (retire that first), asks for the label it read from File 0, erases (a filemark at the start of the tape, seconds), reads the tape back to prove it, and records the cartridge. Never degauss or bulk-erase an LTO cartridge: that destroys its factory servo tracks.
EOF
      EOUT="$(dirname "$LOG")/cartridge-erase-dry-run.out"
      if ! run_capture "$EOUT" tc cartridge erase --device "$DEVICE" --dry-run; then
        die "cartridge erase refused this tape (above) — act on its remedy, or load a different cartridge, then re-run scripts/first-run.sh --from 13"
      fi
      # The consent word from the report's own field (#469), never from the
      # human line: `confirm_label` is null when the chip serial could not be read.
      EJSON="$(dirname "$LOG")/cartridge-erase-dry-run.json"
      run_capture_json "$EJSON" tc cartridge erase --device "$DEVICE" --dry-run --json \
        || die "cartridge erase --dry-run --json refused this tape (above); re-run scripts/first-run.sh --from 13"
      ERASE_WORD="$(python3 -c 'import json,sys
try: print(json.load(open(sys.argv[1])).get("confirm_label") or "")
except Exception: pass' "$EJSON")"
      if [ -z "$ERASE_WORD" ]; then
        die "cartridge erase cannot erase this tape: the chip serial could not be read (its note above), and without it nothing proves which cartridge was erased; nothing was erased. Check the drive's device_sg with \`tapectl config check\` and what the chip reports with \`tapectl volume identify --device $DEVICE\`, then re-run scripts/first-run.sh --from 13"
      fi
      if [ "$AUTO" = 1 ]; then
        # A default is never consent to erase: under --auto the label must be
        # named by --erase-label (never a profile, #469), and equal what
        # the tape says.
        if [ -z "$ERASE_LABEL" ]; then
          [ -n "$PROFILE_ERASE_LABEL" ] && note "the profile sets ERASE_LABEL=$PROFILE_ERASE_LABEL, which is not consent: pass --erase-label (#469)"
          die "the tape holds $ERASE_WORD (above). Under --auto, cartridge erase runs only when --erase-label names the label to overwrite; nothing was erased"
        fi
        [ "$ERASE_LABEL" = "$ERASE_WORD" ] || die "--erase-label $ERASE_LABEL does not match the tape in the drive, which holds $ERASE_WORD — nothing was erased"
        printf '   ERASE the tape holding %s [auto: named by --erase-label]\n' "$ERASE_WORD"
      else
        confirm_destructive "ERASE the tape in $DEVICE, which holds $ERASE_WORD (shown above)" "$ERASE_WORD" || die "stopped before erasing; nothing was erased"
      fi
      erc=0
      run tc cartridge erase --device "$DEVICE" --confirm-label "$ERASE_WORD" || erc=$?
      [ "$erc" = 0 ] || die "cartridge erase exited $erc (2 refused, nothing written; 3 unverified; 4 failed part-way; 6 the read-back contradicts the erase): read its message above, then re-run scripts/first-run.sh --from 13"
      ok "the tape is erased, read back and recorded"
      # Continue: init again. The short erase leaves File 0 EMPTY, and this catalog
      # just recorded the verified erase of this very cartridge with no write since:
      # volume init takes that as proof of blank (ADR-0012 2026-10-15 item 57, #434),
      # so no --force. If it still refuses, the proof did not hold -- say so, don't force.
      run_capture "$INIT_OUT" tc volume init "$LABEL" --device "$DEVICE" \
        || die "volume init still refused after the erase — read the refusal above, then re-run scripts/first-run.sh --from 13"
    elif grep -E "identifies a DIFFERENT volume|File 0 is EMPTY|is not provably blank" "$INIT_OUT" >/dev/null; then
      # The refusals --force can override (write.rs decide_fresh_write_contact_on:
      # IdentityMismatch, EmptyFileZero, and an unreadable File 0 on a tape that is
      # not provably blank and not bound to a live volume). Routed by phrase, never
      # by "ADR-0003", which the unreadable-File-0 refusals also cite (#446); the
      # phrases are pinned by tests/first_run_refusal_phrases.rs.
      explain <<'EOF'
volume init refused. The cartridge's File 0 identifies a DIFFERENT volume that is NOT sealed (a stale or foreign tape), or is EMPTY (a filemark at the beginning of the tape, e.g. after `mt weof`), or could not be read on a tape that is not provably blank (a drive that needs cleaning, a medium error, or a tape written in another block size: check the drive and the cartridge first); tapectl's refusal above says which. tapectl will not overwrite either by accident. If this cartridge is genuinely expendable (a retired volume, a test tape), re-run init with --force; if you are not sure, stop and check `tapectl volume identify --device <dev>` first. (A SEALED tape is a different case and --force would not help there; this is not that.)
EOF
      [ "$AUTO" = 1 ] && die "volume init refused under --auto; not forcing"
      confirm_destructive "OVERWRITE whatever is on this cartridge with $LABEL" "$LABEL" "$LABEL_FROM_FLAG" || die "stopped"
      run_capture "$INIT_OUT" tc volume init "$LABEL" --device "$DEVICE" --force || die "volume init failed"
    else
      # Everything else (no cartridge loaded, a generation the drive cannot
      # write, a retired or displaced cartridge, a barcode collision, ...) is
      # a fact --force does not change; the refusal names its own remedy.
      die "volume init refused for a reason no flag overrides — read the refusal above, act on it, then re-run scripts/first-run.sh --from 13"
    fi
  fi
  # The cartridge this volume is now bound to, from init's own report lines
  # (src/volume/write.rs `report_binding`). Best-effort: a missing line costs
  # the operator a printed hint, never the write.
  CART_NEW="$(sed -n 's/^cartridge \(.*\) auto-registered from MAM.*/\1/p' "$INIT_OUT" | head -1)"
  CART_BOUND="${CART_NEW:-$(sed -n 's/^volume "[^"]*" bound to cartridge \(.*\)$/\1/p' "$INIT_OUT" | head -1)}"
  if [ -n "$CART_NEW" ]; then
    ok "cartridge $CART_NEW registered from its chip; that serial is its placeholder barcode"
    note "When you put a sticker on it:  $OP_CMD cartridge relabel $CART_NEW <your-barcode>"
  elif [ -n "$CART_BOUND" ]; then
    ok "volume $LABEL bound to the already-registered cartridge $CART_BOUND"
  fi
  # --yes answers `volume write`'s own quiet-host pre-flight (ADR-0012,
  # 2026-09-24 item 7): the host check above asked that question moments ago,
  # before the WRITE confirmation, and a second prompt for the same finding --
  # or, under --auto with no terminal, a Tier-2 decline -- would add nothing.
  # The write still PRINTS any finding it sees. --yes reaches nothing else
  # here: volume write has no other Tier-2 question and no Tier-3 refusal.
  # A write that stops (a signal, a dropped session, an error mid-tape) is
  # picked up by re-running this step: it finds the interrupted session and
  # offers `volume resume` (issue #414).
  run tc volume write "$LABEL" --device "$DEVICE" --yes --keep-staging || die "write did not seal — read the output; the catalog knows exactly why. If it was interrupted, reload the same cartridge and re-run scripts/first-run.sh --from 13 --label $LABEL: it resumes the session"
fi
  # Issue #265: a failed verify has TWO outcomes and they need opposite
  # responses. ADR-0012's 2026-09-17 amendment: only a mismatch that PROVES the
  # medium bad takes the volume out of service; a drive or transport error
  # proves nothing about the tape and leaves it untouched. Collapsing both into
  # "do not trust this tape" told the operator the opposite of what the tool had
  # just decided in one of the two cases, and never mentioned that the other had
  # silently changed the catalog.
  VERIFY_JSON="$(dirname "$LOG")/volume-verify-$LABEL.json"
  VERIFY_ERR="$(dirname "$LOG")/volume-verify-$LABEL.err"
  if run_capture_json_err "$VERIFY_JSON" "$VERIFY_ERR" tc volume verify "$LABEL" --device "$DEVICE" --full --json; then
    ok "$LABEL sealed and verified"
    # #440: the write above kept staging (--keep-staging) so this full verify ran
    # while the staged slices could still rewrite a bad copy. Now release what the
    # policy's copies cover; under-copied units stay staged for the next copy.
    run tc staging clean || note "staging clean did not finish: run tapectl staging clean when you want the staged slices released"
  else
    # The loaded tape is not this volume: verify corroborates the cartridge
    # before reading anything (src/volume/binding.rs `corroborate_volume`)
    # and refuses "wrong tape:" / "wrong cartridge:". Nothing was read or
    # recorded, and neither the drive nor the medium is in question.
    if grep -E "wrong (tape|cartridge):" "$VERIFY_ERR" >/dev/null 2>&1; then
      note "The cartridge in $DEVICE is not $LABEL's: the verify refused before reading it (above)."
      note "This is a different cartridge loaded, not a drive or a medium problem; nothing was recorded."
      die "load $LABEL's cartridge (\`$OP_CMD volume info $LABEL\` names it), then re-run scripts/first-run.sh --from 13 --label $LABEL"
    fi
    # `quarantined` is the discriminator (src/cli/volume.rs, `volume verify
    # --json`): true = the medium was proved bad and the volume is now out of
    # service; false = the verify failed without proving anything about the
    # medium. "unknown" if the JSON did not parse, in which case say so rather
    # than guess which.
    VERIFY_QUAR="$(python3 -c 'import json,sys
try:
    d = json.load(open(sys.argv[1]))
except Exception:
    print("unknown"); raise SystemExit(0)
print("yes" if d.get("quarantined") else "no")' "$VERIFY_JSON" 2>/dev/null || echo unknown)"
    case "$VERIFY_QUAR" in
      yes)
        note "The catalog has taken $LABEL out of service: the verify proved the MEDIUM bad, so"
        note "its units' copy counts have just dropped. That is a real finding about this cartridge."
        explain <<'EOF'
Do not trust this tape. The data you staged is still on disk — nothing has been deleted — so the recovery is to write a fresh cartridge: put a new tape in, `tapectl volume init <new-label>`, `tapectl volume write <new-label>`. Keep this one out of the rotation until you know why it failed; `tapectl volume info` and the drive's own health page (`sg_logs`) are where to look.
EOF
        die "verify proved the medium bad — $LABEL is out of service, heir kit NOT generated"
        ;;
      no)
        note "The verify did not complete, but it proved NOTHING about the medium: this reads as a"
        note "DRIVE or transport problem, so $LABEL is untouched and still in service."
        explain <<'EOF'
Do not retire this cartridge. Clean the drive, check the cable and the SCSI path, then run the verify again — `tapectl volume verify <label> --device <device> --full`. A verify that then passes clears the volume's condition and finishes this step. The tape has not been altered by a failed read.
EOF
        die "verify did not complete (drive/transport, not the medium) — re-run it, heir kit NOT generated"
        ;;
      *)
        note "The verify failed and its --json could not be parsed, so this script cannot tell you"
        note "whether the MEDIUM was proved bad or the DRIVE could not read it. Read $VERIFY_JSON"
        note "and the output above before deciding; the two cases have opposite remedies."
        die "verify FAILED and its outcome could not be classified — heir kit NOT generated"
        ;;
    esac
  fi
  explain <<'EOF'
`audit` compares the catalog against policy. With one tape and a policy of two copies it will report copy_count violations — that is correct and advisory (exit 2 means "violations", not "broken"). The second copy is the next cartridge, and the staged slices are still on disk (staging is released only once every unit has its min_copies): load a fresh cartridge and run `volume init <label-2>` then `volume write <label-2>`. Only if staging was already cleaned do you need `volume read-slices --from <label-1>` (with the FIRST tape loaded) to bring the slices back before writing the second.
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
hdr 14 "Timers, the operator wrapper, and what comes next"
explain <<EOF
Three timers and a wrapper finish the installation (ADR-0012, 2026-09-24 and 2026-10-07): a WEEKLY advisory audit (Monday 09:00, read-only, no tape; exit 1 = warnings is not a failure), a DAILY drive health poll (06:30: the drive's log pages and chip through /dev/sg* only — never the tape node, never a motion; a raised TapeAlert is a failure) and a DAILY catalog backup at 03:00 — \`tapectl db backup\` to ${BACKUP_DIR:-$BACKUP_DIR_DEFAULT}, newest 14 kept, the copy checked for a SQLite header and the live catalog fsck'd — so the catalog outlives this disk. Put that directory on a SECOND disk (--backup-dir); the installer warns when it shares a filesystem or a disk with the home, and refuses to move the directory installed timers already use unless --backup-dir (or the profile) names the one to use. After a write session, \`sudo systemctl start tapectl-backup.service\` takes a copy at once. The wrapper /usr/local/bin/tapectl-op runs tapectl as $SVC_USER, so you need not type the prefix; a tape command (volume write, resume, verify, rewrite, compact, read-slices, init and identify; restore; cartridge erase; collection run; quick-archive; catalog rebuild; drive poll and eject) goes through \`sudo nice -n $TAPE_NICE ionice $TAPE_IONICE runuser -u $SVC_USER -- tapectl\` at raised priority (one sudo prompt where root sudo asks for one), and falls back to \`sudo -u $SVC_USER -H tapectl\` at normal priority with a notice if that is refused. scripts/install-systemd.sh does all of it, idempotently — re-run it to change anything, --uninstall reverses it — and nothing in it moves a tape. docs/install.md is the runbook.
EOF
INSTALL_ARGS=(--tapectl "$TAPECTL" --tape-nice "$TAPE_NICE" --tape-ionice "$TAPE_IONICE")
# Only a directory a profile or --backup-dir chose: passing the default as if
# chosen is how 2026-10-20's run moved home2's backups to the catalog's disk (#502).
if [ -n "$BACKUP_DIR" ]; then INSTALL_ARGS+=(--backup-dir "$BACKUP_DIR"); fi
if [ "$SVC_MODE" = 1 ]; then INSTALL_ARGS+=(--user "$SVC_USER"); else INSTALL_ARGS+=(--user "$USER"); fi
WRAPPER_INSTALLED=0
if [ -n "$HOME_DIR" ]; then
  # A rehearsal against a throwaway home must not leave host-wide timers
  # pointing at it. Show the plan; the real run (no --home) installs.
  note "rehearsal home ($HOME_DIR): host-wide timers are NOT installed for a throwaway home — this is the plan only"
  run bash "$REPO/scripts/install-systemd.sh" --dry-run --tapectl-home "$HOME_DIR" "${INSTALL_ARGS[@]}" || note "(install-systemd.sh --dry-run failed — read the output)"
elif confirm "Install the audit and backup timers and /usr/local/bin/tapectl-op now (sudo)?"; then
  if run bash "$REPO/scripts/install-systemd.sh" "${INSTALL_ARGS[@]}"; then
    [ -x /usr/local/bin/tapectl-op ] && WRAPPER_INSTALLED=1
    ok "timers installed — systemctl list-timers 'tapectl-*' shows them"
  else note "install-systemd.sh failed — fix what it printed and re-run it by hand: scripts/install-systemd.sh $(printf '%q ' "${INSTALL_ARGS[@]}")"; fi
else note "skipped — later: scripts/install-systemd.sh $(printf '%q ' "${INSTALL_ARGS[@]}")"; fi
[ "$WRAPPER_INSTALLED" = 1 ] && OP_CMD="tapectl-op"
cat <<EOF
   • Every tapectl command from now on: ${OP_CMD} <command>$( [ "$WRAPPER_INSTALLED" = 1 ] || printf '   (alias it)' ). Its home: $EFFECTIVE_HOME
   • A restore destination must be writable by ${SVC_USER}; new unit trees need the same ACL grant as step 11.
   • Eject with: ${OP_CMD} drive eject (it tells the catalog; a write or verify in progress refuses it). Write the label on the cartridge.
     Then tell the catalog what the sticker says (ADR-0012 — the serial stays its identity):
     ${OP_CMD} cartridge relabel ${CART_BOUND:-<medium-serial>} <the-barcode-you-wrote>
   • Second copy, other location: the staged slices are still on disk, so load a fresh cartridge,
     \`${OP_CMD} volume init <label-2>\`, \`${OP_CMD} volume write <label-2> --keep-staging\`, then \`volume move <label-2> --to <other shelf>\`
     (or both in one: \`${OP_CMD} volume write <label-2> --init --full-confirm --move-to <other shelf>\`).
     (Only if staging was cleaned: load tape 1 first and \`volume read-slices --from ${LABEL:-<label>} --unit <unit>\` per unit.)
   • \`tapectl audit\` runs weekly and the catalog is copied daily to ${BACKUP_DIR:-$BACKUP_DIR_DEFAULT} (systemctl list-timers 'tapectl-*');
     after each session: sudo systemctl start tapectl-backup.service, and regenerate the heir kit (step 9).
   • \`${OP_CMD} report summary\`, \`report fire-risk\`, \`catalog locate <unit>\` answer the operator questions;
     \`report health\` shows the drive's error counters from every tape contact (ADR-0013).
   • Disaster recovery, the whole procedure: docs/operator-guide.md, "Disaster Recovery".
   • Reinstall, move to another host, uninstall: docs/install.md.
   • This script is resumable: scripts/first-run.sh --from N. Log: $LOG
EOF
ok "done"
