# LTO-6 Hardware Validation Checklist

This is the procedure for the real-hardware validation session. Per the
#16 verdict an LTO-6 drive is owned but development stays mhvtl-first; this
session fires when phases 1–2 land and `scripts/mhvtl-verify-gate.sh` is
fully green (empty EXPECTED_FAIL). It was dry-run against mhvtl on
2026-07-20 (ticket #8); the annotations below record what that dry-run
established and what only real hardware can settle.

The goal of the real-hardware pass is to prove, on actual media, that
everything mhvtl has been simulating works: fixed-block I/O, MAM
queries, sg_logs error counters, ENOSPC behavior at end-of-tape, and
the full write → verify → restore round-trip.

## Dry-run findings (mhvtl, 2026-07-20) — read before the hardware session

Baseline recordings for later diffing are in
`docs/mhvtl-baseline-recordings.txt` (mhvtl's `sg_read_attr`, sg_logs
pages 0x02/0x0c, and the EOT-drill result).

- **The ENOSPC error path CANNOT be validated on mhvtl as configured.**
  EOT drill (CAPACITY=500 MB tape, ~591 MB of encrypted slices): mhvtl
  accepted every write **without returning ENOSPC**, sitting exactly at
  its 500 MB early-warning point, and silently produced 2 unreadable
  slices out of 10. `volume verify` caught it (8 passed / 2 failed), but
  `volume write` reported success and marked the snapshot `current`. So
  the ENOSPC drill below is a **real-hardware-only** check — mhvtl gives a
  false pass by not signalling. This is the fidelity gap flagged on #26.
- **Pre-flight device discovery is mandatory** (SCSI enumeration shuffles;
  see #67 and `scripts/mhvtl-verify-gate.sh`): find the changer
  (`lsscsi -g | grep mediumx`), the drive's sg node, and load a
  generation-matched cartridge — do not assume `/dev/sg0` or slot 1.
- **Block-size / compression pre-flight** were missing from the original
  stub; added below.
- The MAM/sg_logs commands work against mhvtl (recorded), so the *plumbing*
  is validated; only the values differ on real media.

## Pre-flight

- [ ] Drive visible: `lsscsi -g | grep -i lto` shows both `/dev/nst*`
      and `/dev/sg*` nodes.
- [ ] Drive responds: `mt -f /dev/nst0 status` succeeds; `sg_inq
      /dev/sg1` reports vendor/model.
- [ ] MAM query works: `sg_read_attr -r /dev/sg1` returns real capacity
      and serial.
- [ ] sg_logs populated: `sg_logs --page=0x02 /dev/sg1` has non-zero
      reads of the drive error counters.
- [ ] dar version ≥ 2.6: `dar --version`.
- [ ] Block size accepted: `mt -f /dev/nst0 setblk 524288 && mt -f
      /dev/nst0 status` reports `Tape block size 524288 bytes`. (tapectl
      writes fixed 512 KB; a drive that rejects it fails EINVAL on open.)
- [ ] Hardware compression state recorded: check the drive's compression
      mode page (`sg_logs` / mode select). Encrypted data is incompressible;
      the design says compression MUST be off (#28 issues MTCOMPRESSION 0).
      Record the as-found state for the write-throughput baseline.
- [ ] tapectl binary and config already validated against mhvtl
      (`scripts/mhvtl-verify-gate.sh` green with an empty EXPECTED_FAIL).
- [ ] A known-good blank tape is loaded; label prefix set aside (e.g.
      `LTO6-`) to distinguish from any mhvtl test labels in the DB.

## Round-trip on real media

- [ ] `tapectl volume init LTO6-0001 --device /dev/nst0`
- [ ] Stage at least two tenants' units so the volume exercises the
      multi-tenant envelope path.
- [ ] `tapectl volume write LTO6-0001 --device /dev/nst0` — note any
      warnings about block-size mismatch or compression.
- [ ] `tapectl volume verify LTO6-0001 --device /dev/nst0` — per-slice
      sha256 must all pass; failed count must be zero.
- [ ] `tapectl restore unit <name> LTO6-0001 --device /dev/nst0` for
      each tenant. `diff -r` against the source must be clean.
- [ ] `tapectl report health` — drive error counters from sg_logs
      should show write_ok >> write_corrected; no unrecovered errors.

## Raw-recovery drill (the killer feature)

Using **only** `mt`, `dd`, `age`, `dar`, and `sha256sum` — no `tapectl` —
recover one tenant's unit end-to-end from the freshly-written tape above.
This is the design's strongest claim; if it fails on real hardware, it is
not done. (This is exactly the heir leg of `scripts/mhvtl-verify-gate.sh`;
the drill here is the hardware confirmation of a leg already green on
mhvtl. `fsf 2` assumes the layout order ID-thunk(0)/guide(1)/RESTORE.sh(2)
— confirm with `./RESTORE.sh --info` if the count ever changes.)

- [ ] Extract RESTORE.sh from tape:
      ```
      mt -f /dev/nst0 rewind && mt -f /dev/nst0 fsf 2
      dd if=/dev/nst0 bs=512k | tr -d '\0' > RESTORE.sh
      chmod +x RESTORE.sh
      ```
- [ ] `./RESTORE.sh --info` — layout matches what `tapectl volume verify`
      reported (correct number of data slices, envelope count, etc.).
- [ ] `./RESTORE.sh --find-envelope --key <tenant-key>.age.key` — decrypts
      the correct tenant envelope and displays MANIFEST.toml with accurate
      slice positions and checksums.
- [ ] `./RESTORE.sh --restore --key <tenant-key>.age.key --to /tmp/recovered`
      — full restore succeeds: all slice checksums pass, age decryption works,
      dar extraction completes.
- [ ] `diff -r <original-source-dir> /tmp/recovered` — byte-identical.

## ENOSPC drill — REAL HARDWARE ONLY (mhvtl gives a false pass)

The 2026-07-20 dry-run established that mhvtl (CAPACITY=500) does **not**
return ENOSPC past capacity — it accepts the writes and silently corrupts
the overflow slices, so this drill cannot be validated virtually (see the
dry-run findings above). On real hardware:

- [ ] Two-part expectation, post-phase-1: with the Layout/WriteSession
      model (#21/#26) in place, a stage set whose plan exceeds available
      capacity is **refused by Layout validation before the first byte**
      (capacity vs plan + reserve). Confirm that refusal first — it is the
      primary defense and the thing the dry-run proved is missing today
      (the write silently succeeded with 2 dead slices and a `current`
      snapshot).
- [ ] Then the genuine-overflow case (a cartridge that fills mid-session
      from real write growth, not a mis-planned volume): write until the
      drive signals early-warning, and confirm tapectl performs the layered
      EOT recovery as a Layout transition (stop slices → regenerate
      metadata from the truncated Layout → seal), recording
      `writes.eot_recovery` / `sacrificed_slice_id`, and that the resulting
      tape is still self-describing (`RESTORE.sh --info` + a real restore of
      the surviving units). Capture the drive's exact ENOSPC sense data for
      the record — it is the input #26 could not get from mhvtl.

## After a successful pass

- [ ] Update `tapectl-design-v4_0.md` M7 checklist to mark
      "Real LTO-6 hardware validation" done.
- [ ] Add a dated entry to `docs/perf-baselines.md` with real-hardware
      throughput numbers for the three scenarios in `tests/performance.rs`
      (bump `TAPECTL_PERF_LARGE_MB` significantly — the design doc
      targets 2+ TB units).
- [ ] Note any deviations from mhvtl behavior in `CLAUDE.md`'s
      "Current State" section so future work has context.

## If something fails

Don't paper over it. Capture the full drive state — `sg_logs
--page=0x02,0x03,0x0c /dev/sg1`, `mt -f /dev/nst0 status`,
dmesg since the tape was loaded — and file it alongside the tapectl
command that triggered the failure. The point of this pass is to
surface differences between mhvtl and real hardware; silent workarounds
defeat the purpose.
