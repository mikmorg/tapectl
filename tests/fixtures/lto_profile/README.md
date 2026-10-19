# lto-profile fixtures (issue #479)

`hp_lto6_run_a/` is a run directory in the format `scripts/lto-profile.sh` leaves (docs/lto-profiling.md,
"The run directory"): the raw captures, no `profile.json` (the tests build it with
`scripts/lto-profile-report.py build`). It is shaped on the HP LTO-6 and the FUJIFILM cartridge
EW7VWMVKF6 of the 2026-09-23/24 real-drive work, so the derivation is exercised on real magnitudes.

**Recorded** (from `docs/runs/2026-09-23-real-drive-rehearsal.md`, `docs/runs/2026-09-23-lto6-capacity-measurement.md`
and `tests/fixtures/sg_logs/hp_lto6_sg0_fuji_ew7vwmvkf6/`): the MAM attributes of the cartridge, the drive's
log pages 0x02 0x03 0x0c 0x17 (copied), the 20 GiB capacity write (MAM remaining 2,499,053 -> 2,478,446 MiB,
page 0x17 used 0 -> 21,607 MB), the fill's 2,501,995,134,976 bytes before the first ENOSPC, MAM remaining
101,850 MiB at the warning, page 0x0c's "Minimum native capacity from EW to EOP" 99,008 MB, the block-size
throughputs (114.0 / 114.4 MiB/s, `docs/lto6-session-journal-2026-09-10.md`).

**Synthetic** (nothing on the real drive has measured them yet; #420's past-warning fill and the walk to the
hard end are the operator's run on home2): `fill/result.json`'s post-warning slice, filemarks, seal block and
hard-end walk (187,500 blocks after the slice, stopped by 8 refusals in a row), the final snapshot's wear
counters, the end-of-run page 0x17 used figure, `inq.txt` (assembled from the drive's lsscsi row and
`docs/lto6-drive-passthrough.md`), and the hard-end result. Section E (end of data) and C (LBP) are absent:
not measured on the real drive, which also exercises the report's "not measured" path.

Do not read a number here as a measurement of the drive unless it is in the recorded list.
