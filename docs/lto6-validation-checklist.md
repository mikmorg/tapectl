# LTO-6 Hardware Validation Checklist

This is the procedure for closing the last open M7 item — real-hardware
validation on an LTO-6 drive — once the drive is physically available.
It is a **stub** while development is mhvtl-only; flesh out each step
when hardware arrives.

The goal of the real-hardware pass is to prove, on actual media, that
everything mhvtl has been simulating works: fixed-block I/O, MAM
queries, sg_logs error counters, ENOSPC behavior at end-of-tape, and
the full write → verify → restore round-trip.

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
- [ ] tapectl binary and config already validated against mhvtl
      (all mhvtl-gated tests pass with `TAPECTL_MHVTL=1`).
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

- [ ] Using **only** `mt`, `dd`, `age`, and `dar` — no `tapectl` —
      follow the system guide and `RESTORE.sh` on a freshly-loaded tape
      and recover one tenant's unit end-to-end. This is the design's
      strongest claim; if it fails on real hardware, M7 is not done.

## ENOSPC drill

- [ ] Configure a synthetic volume large enough to overshoot the tape
      (or use an LTO-6 cartridge already close to full). Write a stage
      set that exceeds remaining capacity; confirm tapectl performs the
      layered ENOSPC recovery described in the design doc (normal
      metadata write in early-warning zone → fallback overwrite →
      last-resort sacrifice of final slice) and that the resulting
      tape is still self-describing.

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
