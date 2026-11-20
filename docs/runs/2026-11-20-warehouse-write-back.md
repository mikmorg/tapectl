# 2026-11-20: the warehouse write-back, rehearsed on mhvtl (T5.4)

The plan's Stage 5 introduction makes the coordinator rehearse the warehouse way back when
T5.4 lands, because T5.4 wrote it (`docs/operator-guide.md`, "Getting the bytes back") from
`volume-format-v3.md` alone. This is that rehearsal. Both ways back passed.

## What ran

- Binary: a debug build of `l3-t54` rebased onto `layout-3` 49695251 (tapectl 2.0.0 in build,
  Layout 3). GNU tar 1.35, age 1.1.1, mt-st, as uid 1000 on vm-desk1 (Ubuntu, kernel
  6.8.0-142).
- Drive: mhvtl A2 (`/dev/tape/by-id/scsi-XYZZY_A2-nst`, `/dev/nst1`, ULT3580-TD8), changer
  `/dev/sg3`, DTE 1. Cartridge E01001L8 (slot 1) for the volume, E01003L8 (slot 3) for the
  write-back; both erased first, the library put back after.
- Home: a throwaway `--home` under `/scratch/tapectl-wh-rehearsal`. One tenant (alice), an
  escrow key, one unit `unitA` of 44 files and 3.3 MiB: a 640-mode text file, a symlink, 40
  small files two levels down and one 3.5 MB random file. `slice_size = "1M"`, no compression.
- `stage create unitA`: 6 slices. `volume init WHR001` + `volume write WHR001`: sealed and
  confirmed (LBP fell back, as always on the TD8).

## Step 1 of the deposit procedure: the dump

`tapectl restore raw-volume --to dump --from WHR001 --device /dev/nst1`:
`dumped 15 files (4.9 MiB) … verified: 13  mismatched: 0  unverifiable: 2`. Files 0000 to
0014: id thunk, system guide, RESTORE.sh, front index (524,288 bytes), tenant envelope, operator
envelope, its backup, the tenant envelope's backup, six data slices (positions 8 to 13), seal
marker (524,288 bytes). Content files came out at their exact lengths.

## The manual way back (the guide's steps 2 to 6)

- Step 2: `tr -d '\0' < 0003_front_index.bin` read as plain text.
- Step 3: `age -d -i keys/alice-primary.age.key < 0004_tenant_envelope.bin | tar -x` gave
  `MANIFEST.toml`, `RECOVERY.md`, `index/` and `tables/c5cd276f_v1_s1.tsv` (the file table) and
  `tables/c5cd276f_v1_s1.ids.tsv` (the id map: `u 1000 mikmorg`, `g 1000 mikmorg`).
- Step 4: the six `[[units.slices]]` blocks mapped `number` 1 to 6 onto `tape_position` 8 to 13.
  For each, `sha256sum` of the dumped file equalled `sha256_encrypted`, and of `age -d`'s output
  equalled `sha256_plain`.
- Step 5: `cat slice.0001 … slice.0006 | tar -x -f - --numeric-owner --preserve-permissions
  --xattrs --acls` into an empty directory.
- Step 6: `diff -r --no-dereference` against the source: identical.

## The write-back

E01001L8 out to its slot, E01003L8 in; `mt rewind`, `mt erase`, `mt setblk 524288`, `mt rewind`,
then for each of the 15 files in position order `dd if=NNNN_type.bin of=/dev/nst1 bs=512k
conv=sync`, as the guide gives it. RESTORE.sh was the dump's `0002_restore_sh.bin`, made
executable, with `TAPE_DEVICE=/dev/nst1`:

- `RESTORE.sh --verify`: `PASS` for all 15 files, `VERIFY PASSED: 14 file(s) match the front
  index.`, exit 0.
- `RESTORE.sh --restore --key keys/alice-primary.age.key --to heir-dest --all`: `File check:
  all 43 regular file(s) match the sha256 in the unit's file table.`, `RESTORE COMPLETE: 1
  unit(s) restored`, exit 0. `diff -r --no-dereference` against the source: identical (mode 640
  and the symlink kept).

## What it changed

- The guide's two "not run" notes now say the rehearsal ran, and point here.
- The guide now says to `export TAPE_DEVICE="$TAPE"` before RESTORE.sh, since it reads
  `/dev/nst0` without it, as the guide's heir section already says.
- One unit restored with `--all` lands in `DIR` itself, not `DIR/unitA`: RESTORE.sh's header
  says so ("with more than one unit each goes to DIR/UNIT"), and the guide now says it too.
  RESTORE.sh's own hint after `--find-envelope` ("or --all for every unit, each in DIR/NAME",
  line 1128 of this build) leaves out the one-unit case. That is heir-script text, not changed
  here. It is noted for the reads of T6.x.

## Not covered

No cloud: rclone and aws were not run, as the guide says. No spanning (a unit on two
deposits), no compression, no second tenant. A spanned deposit goes through the same
RESTORE.sh, which asks for each cartridge in turn; the gate's `span_*` legs prove that.
