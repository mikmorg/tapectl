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
