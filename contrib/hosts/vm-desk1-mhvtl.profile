# contrib/hosts/vm-desk1-mhvtl.profile — a rehearsal of scripts/first-run.sh on
# the dev VM against mhvtl, NEVER the real drive. It exercises the profile
# mechanisms home2.profile uses (work dir, dar path, staging, several locations,
# tenants, an explicit contender list) but POST_WARNING_BYTES (see below), and a
# collection, which home2.profile no longer sets, with a throwaway home.
#
#   cargo build && sudo install -m 0755 "${CARGO_TARGET_DIR:-target}/debug/tapectl" /scratch/fr-bin/tapectl
#   scripts/mhvtl-device.sh --tape /dev/tape/by-id/scsi-XYZZY_A1-nst --ensure-media   # prints LOADED_TAG
#   mkdir -p /scratch/fr-src/keepsake/{a,b} && echo a >/scratch/fr-src/keepsake/a/f && echo b >/scratch/fr-src/keepsake/b/f
#   scripts/first-run.sh --profile contrib/hosts/vm-desk1-mhvtl.profile --auto --barcode <LOADED_TAG>
#
# Clean up: rm -rf /scratch/fr-{home,work,kit,backups,staging,src}; sudo rm -rf those
# owned by tapectl.
#
# scripts/first-run-reentry-mhvtl.sh sources this profile with every path moved into
# its own workspace, and adds issue #414's interrupted-write re-entry on top.

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
# No POST_WARNING_BYTES, deliberately (ADR-0012 item 200), so the rehearsal runs
# step 8's other path: the notice that says what a drive without
# post_warning_bytes loses. Two reasons. It is a measurement of a real drive's
# room past its early warning (scripts/lto-profile.sh), and mhvtl has none to
# measure: its media report no real capacity, which is why the gate gives its
# drive capacity_override, and a made-up figure here would set the slice cap on
# nothing. And scripts/upgrade-mhvtl.sh's phase P runs the previous release's
# first-run.sh on a profile that sources this one: a release before 1.9.0 does
# not know the name and refuses the profile. tests/first_run_profile.rs proves
# the path that sets it.
# Tape commands at raised priority (#432), as home2 runs them (these are also
# first-run.sh's defaults). The re-entry scenario asserts them on the live write.
TAPE_NICE=-10
TAPE_IONICE="-c2 -n0"
