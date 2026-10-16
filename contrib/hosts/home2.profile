# contrib/hosts/home2.profile — scripts/first-run.sh defaults for home2, the
# production host (Debian 10, kernel 4.19, the HP LTO-6 on the PERC's SAS
# port). Sourced by `scripts/first-run.sh --profile contrib/hosts/home2.profile`;
# any flag on the command line overrides a value here. Run
# contrib/hosts/home2-prep.sh --apply first: it creates the directories, the
# dar and the service-user home this file names.
#
# Decided with the operator on 2026-09-28; the reason for each is beside it.

# The drive, by serial (the sg node is derived from sysfs; do not pin /dev/sgN:
# a USB stick moved the numbering on this host).
DEVICE=/dev/tape/by-id/scsi-HUJ808A5L4-nst

OPERATOR=mikmorg

# The 1.1.0 fresh start keeps the escrow identity (ADR-0012, 2026-10-12, item 39).
# Its public key is kept OUT of git (item 50): put
#   ESCROW_PUBLIC_KEY=age1…        (the Heir Kit cover sheet's line)
# in ~/.config/tapectl/home2.local.profile (or the file TAPECTL_LOCAL_PROFILE
# names), or pass --escrow-public-key on the command line. A file outside the
# repo leaves `git describe --dirty`, and so the binary's version, clean.
ESCROW_PUBLIC_KEY=""

# #368, measured on home2 2026-10-13 (7 keepsake units, ~35 GiB): 1 job 13m08s,
# 2 jobs 8m02s, 4 jobs 7m29s. Two at once (ADR-0012, 2026-10-13, item 53).
STAGING_JOBS=2
# audit's full-readback clock, per unit and per volume (#453, #454): every tape is read back
# in full at least once a year. home2 has no archive sets, so this is where the interval lives.
VERIFY_INTERVAL_DAYS=365

# What `host check` names before a write (ADR-0012, 2026-10-13, item 55): the
# built-in list plus java — the minecraft server here, which on 2026-10-12 made
# a write use 1.5x its native tape (#426). The whole list: it replaces the default.
CONTENDER_PROCESSES=(cargo rustc docker Runner.Worker java)
# Tape commands run at raised CPU and I/O priority: vm-desk1 (homorg's work)
# shares this host's CPUs and array, and a busy host starves the drive's feed
# (#426, #432). Priority can only be raised by root. Step 13 raises first-run.sh's
# own shell once (sudo renice and ionice: one password prompt, root's sudo, not
# just the AS-tapectl rule of home2-prep.sh part 8) and every command it starts,
# the write, the resume and the verify among them, inherits it. Step 14 gives
# tapectl-op and the drive-poll unit the same values; they raise a tape command
# as root and drop to tapectl (ADR-0012, 2026-10-15, item 58). Empty TAPE_NICE =
# never raise. The host-side lever (lowering vm-desk1's weight) is printed by
# home2-prep.sh and is the CTO's and homorg's founder's call.
TAPE_NICE=-10
TAPE_IONICE="-c2 -n0"

_local_profile="${TAPECTL_LOCAL_PROFILE:-$HOME/.config/tapectl/home2.local.profile}"
# shellcheck source=/dev/null
[ -f "$_local_profile" ] && . "$_local_profile"
unset _local_profile

# ADOPT_KEYS_FROM is deliberately unset: the 1.1.0 fresh start mints new keys
# for the operator and every tenant (ADR-0012, 2026-10-12, item 41). Set it
# only to keep keys people already hold (docs/keys-and-recovery.md, runbook F).

# The service user's home — and so the tapectl home, catalog and keys — lives on
# /srv/archive_meta (sda, LUKS), not /var (95% full). home2-prep.sh moves it.
SVC_HOME_WANT=/srv/archive_meta/tapectl

# Staging holds one tape's encrypted slices: acache (md1, LUKS, ~2.3 TB free).
STAGING_DIR=/srv/acache/tapectl-staging

# The daily catalog backup goes to a different array from the home (md3, LUKS).
# It is the catalog only, never keys (install-systemd.sh), and encrypted at rest.
BACKUP_DIR=/srv/local_backup/tapectl

# Buster ships dar 2.6.2; the suite and the real-drive rehearsals ran on 2.7.x.
# home2-prep.sh builds 2.7.21 here, statically linked to its own libdar.
DAR_BIN=/usr/local/bin/dar

# No /scratch on this host: the build lock and the step-12 rehearsal's
# throwaway homes live under the operator's cache (/srv/home, ~80 GB free).
WORK_DIR="$HOME/.cache/tapectl"

KIT_OUT="$HOME/tapectl-heir-kit"

# The standing test cartridge (step 12 erases it). A default only: the
# rehearsal still makes you type it, and checks the loaded chip reports it.
TEST_BARCODE=EW7VWMVKF6
LABEL=L6-0001

# Tape 1 is recorded at the first; copy 2 goes to the second.
LOCATIONS=(
  "home2-rack|the shelf beside the LTO-6 in home2"
  "offsite|copy 2, away from home2"
)

# Rings of trust: the operator (mikmorg) reads everything; parents and family
# are separate keys. Who holds which key is the ring — a unit belongs to one
# tenant, and tapectl never encrypts one unit to two tenants.
TENANTS=(
  "parents|parents — the second ring"
  "family|family keepsakes — the outer ring"
)

# keepsake/original-data is one folder per thing: each becomes a unit
# (keepsake/<folder>), owned by family.
COLLECTIONS=(
  "keepsake|/srv/keepsake/original-data|family|1"
)

# Nothing on home2 touches acache or the drive (operator, 2026-09-28). The
# load/memory checks still run; memory and I/O pressure do not exist on 4.19.
CONTENDER_UNITS=()

# Issue #393: let mikmorg (and an agent helping) watch sessions without sudo —
# a plain `tapectl status` (the pointer /etc/tapectl/home, #437). The group reads
# logs/ and nothing else in the home. Ruled for home2 (ADR-0012, 2026-10-07
# amendment, item 18): on the 1.1.0 fresh start, step 7 creates the group, adds
# tapectl and mikmorg to it, writes [ops] group, grants the group traverse (x
# only) on each directory above the home that lacks it (/srv/archive_meta is
# root 0750), and checks `status` as mikmorg with the group.
OPS_GROUP=tapectl-ops
