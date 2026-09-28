# contrib/hosts/vm-desk1-mhvtl.profile — a rehearsal of scripts/first-run.sh on
# the dev VM against mhvtl, NEVER the real drive. It exercises every profile
# mechanism home2.profile uses (work dir, dar path, staging, several locations,
# tenants, a collection, an explicit contender list) with a throwaway home.
#
#   cargo build && sudo install -m 0755 "${CARGO_TARGET_DIR:-target}/debug/tapectl" /scratch/fr-bin/tapectl
#   scripts/mhvtl-device.sh --tape /dev/tape/by-id/scsi-XYZZY_A1-nst --ensure-media   # prints LOADED_TAG
#   mkdir -p /scratch/fr-src/keepsake/{a,b} && echo a >/scratch/fr-src/keepsake/a/f && echo b >/scratch/fr-src/keepsake/b/f
#   scripts/first-run.sh --profile contrib/hosts/vm-desk1-mhvtl.profile --auto --barcode <LOADED_TAG>
#
# Clean up: rm -rf /scratch/fr-{home,work,kit,backups,staging,src}; sudo rm -rf those
# owned by tapectl.

HOME_DIR=/scratch/fr-home
TAPECTL=/scratch/fr-bin/tapectl
SKIP_BUILD=1
SKIP_TESTS=1
DEVICE=/dev/tape/by-id/scsi-XYZZY_A1-nst
DGEN=LTO-8
LABEL=L8-TEST
OPERATOR=mikmorg
WORK_DIR=/scratch/fr-work
STAGING_DIR=/scratch/fr-staging
DAR_BIN=/usr/bin/dar
BACKUP_DIR=/scratch/fr-backups
KIT_OUT=/scratch/fr-kit
LOCATIONS=(
  "fr-rack|the rehearsal shelf"
  "fr-offsite|the rehearsal second place"
)
TENANTS=(
  "parents|rehearsal ring two"
  "family|rehearsal ring three"
)
COLLECTIONS=(
  "keepsake|/scratch/fr-src/keepsake|family|1"
)
CONTENDER_UNITS=()
