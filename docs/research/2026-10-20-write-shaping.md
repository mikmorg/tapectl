# Write shaping: keeping the HP LTO-6 streaming under host load

*Desk research for #488, 2026-10-20. Research only: no product code changed, and no tape
drive, SG node or tape device was opened. home2 was inspected read-only. One short CPU
benchmark ran there (a single-core `openssl speed`). The sha256 benchmarks ran on vm-desk1.
Author: research agent. §10 is the protocol for the real-drive phase, which comes later.*

Facts are tagged:
- **DOC**: a vendor or kernel document, cited by page.
- **CODE**: source in this repository at `37b007a`.
- **MEASURED**: run by me today, on vm-desk1 or (read-only) on home2.
- **RECORD**: an earlier run record or issue comment, named.
- **OBSERVED**: read today from a file, a fixture or sysfs, never a device node.
- **INFERRED**: reasoned from the above.
- **NEEDS DRIVE**: only the protocol in §10 can settle it.

Sources: HP-V2 and HP-V3 are volumes 2 and 3 of the *HP LTO Ultrium 6 Tape Drives Technical
Reference Manual* (October 2012, first edition). QS is the *HP LTO Ultrium Tape Drives
QuickSpecs*, DA-13572 version 14 (2013-09-13). Their page numbers equal the PDF page numbers.

## 1. Summary

1. **tapectl's hasher is not 19% faster than the drive. It is about as fast as the drive.**
   The issue's 191 MB/s is OpenSSL's rate on home2. tapectl hashes with the `sha2` crate,
   `asm` feature (CODE: `Cargo.toml`). On vm-desk1 that runs at 163.6 MB/s, against 216.7 for
   OpenSSL and 216 for `ring` (MEASURED). Scaled to home2's OpenSSL figure today (188.2 MB/s,
   MEASURED), tapectl's hash runs at about **142-157 MB/s** there (INFERRED). The quiet-host
   write ran at **146.5 MB/s** (RECORD, #426 E1 and E3). That write was hash-bound: tmpfs and md3
   staging gave the same time, and the drive takes 158 MB/s from a plain feeder (RECORD,
   2026-10-20 fill). So the producer never runs ahead of the drive. tapectl's 256 MiB queue is
   then about empty, and only the drive's own buffer smooths the feed (INFERRED; §9's trace
   shows it).
2. **A bigger queue alone changes nothing.** A queue absorbs a stall only if it was filled
   before, and it fills only while the producer outruns the drive. The fix is headroom: a
   faster hash (`ring` gives about +32%), or a pace below what the host sustains, or both.
   **Queue size and pace are one lever** (§5.1).
3. **The extra tape is gaps, not extra data.** In the 2026-09-23 runs, the bursty feed and the
   steady feed each wrote about 8,696 data sets for 20 GiB (8,698 and 8,695). The bursty run
   spread them over 3.66 MB of native tape each instead of 2.48 (OBSERVED from the raw
   captures). Every stop and restart leaves unused tape between data sets (INFERRED).
4. **The drive counts its own wear.** HP's Performance Data log page **0x34**, parameter 0, is
   "Repositions per 100 MB": tape direction changes per 100 MB written. HP adds, "The higher the
   figure the more wear occurs" (DOC: HP-V3 p91). The drive lists the page. sg_logs mis-decodes it
   as IBM's "Read forward errors", so tapectl must parse the raw bytes. tapectl's post-write
   sweep already journals those bytes for every production write (CODE: `log_pages::sweep`).
5. **On home2, three of today's levers do nothing:**
   - `ionice`: every staging disk uses mq-deadline, which on 4.19 ignores I/O priority.
   - `nice -10`: it orders tapectl only within its own cgroup. vm-desk1's vCPU threads sit in
     another (`cpu:/system.slice/libvirtd.service`), and docker's java in a third.
   - home2-prep.sh §9's `virsh schedinfo cpu_shares` and `blkiotune`: they write cgroups that
     hold none of vm-desk1's vCPU threads.

   The settings and the cgroup placement are MEASURED; their effect follows from them. CPU
   affinity does work: `virsh vcpupin` and `emulatorpin` place
   threads correctly through the cpuset controller, and `taskset` works for tapectl.
6. **The cause of #426's 1.505 is still open.** That run had about 8 busy cores out of 24. No
   lack of CPU explains a stall of 4 s or more. These do, and §10 tells them apart:
   - an SMT sibling sharing the hasher's core (with no headroom, any slowdown reaches the drive);
   - a staging read stalled behind dm-crypt;
   - scheduling across cgroups.
7. **Recommendation:** build the instrumentation first (§9). Then measure a faster hash, a
   pace with a multi-GiB queue, and a refill pause on the test cartridge, about 2 hours of
   drive time (§10). Reserving two physical cores during tape commands is the host lever
   that works. The new disk should be dedicated staging (§8). The decisions are in §12.

---

## 2. What happens mechanically

### 2.1 The drive

- **It is the half-height SAS model.** Page 0x3e parameter 4, "Product Number", reads
  `0x01090232` in the drive's own capture (OBSERVED: `tests/fixtures/sg_logs/hp_lto6_sg0_fuji_ew7vwmvkf6/page_0x3e.bin`).
  HP-V3 p94 maps that value to "LTO-6 half-height SAS standalone", the StoreEver Ultrium 6250.
- **Speed matching: 54 to 160 MB/s.** HP calls it Data Rate Matching. QS p30 gives "Data rate
  matching 54-160 MB/s" for the half-height LTO-6 (and p27 the same for full height). QS p9: it
  "combines sophisticated buffer management with HP's exclusive Variable Adaptive Tape Speed
  technology … dynamically and continuously", and "minimizes rewinding and repositioning of the
  tape, reducing wear". HP does not publish the number of speeds or the switching rule (not
  found).
- **The buffer: 512 MB.** QS p30 and HP-V3 p55 ("main buffer size of 512 MB"). Page 0x0c
  parameter 8, "Maximum native capacity in device object buffer", reads **418 MB** on this drive
  (OBSERVED, fixture and the 2026-09-23 captures). That is about 2.6 s of tape at 160 MB/s, and
  2.9 s at 146.5 MB/s (INFERRED).
- **Writes return when the data is in the buffer.** With buffered mode on, WRITE reports GOOD
  "once the data is successfully transferred to the data buffer (but not necessarily to tape)"
  (DOC: HP-V3 p255). So a `write(2)` that takes long means the drive buffer is full. A writer
  waiting for data means the buffer is draining.
- **An immediate filemark does not flush.** HP-V3 p261: with Immed set, "GOOD status may be
  reported and the marks left in the data buffer". Without it, the drive writes all buffered data
  first. HP-V2 p24 warns that a non-immediate filemark "will adversely affect performance and
  waste tape capacity". tapectl writes slices with `MTWEOFI`, the immediate form, and only the
  last filemark with `MTWEOF` (CODE: `tape/ioctl.rs` `write_stream`, `run_entries` passes
  `sync = false`). **The drive does not see a slice boundary as a flush.** A pause "between
  files" helps tapectl (its interrupt check and cursor row live there). It does not help the
  drive.
- **The drive flushes on its own after 30 s idle.** "Write Delay Time" defaults to 12Ch, 30 s:
  "the time … that the drive should wait with unwritten data in the buffer and no activity on
  the interface before forcing data to tape" (DOC: HP-V3 p106; HP-V2 p40). A deliberate pause
  shorter than 30 s therefore leaves the partial data set in the buffer, unpadded (INFERRED).
- **Never poll the drive during a long write.** HP-V2 p23: "Do not interleave write commands with
  other commands, such as READ POSITION and LOG SENSE." A LOG SELECT forces a flush (HP-V3 p255).
  So any in-write signal must come from the host, and the drive's counters are read at phase
  boundaries only.
- **20,000 passes.** HP-V2 p19 recommends at most 20,000 passes over any area of a tape. That
  figure depends on "the quality of the application … in being able to maintain streaming,
  thereby preventing repositioning over the same area of tape".

### 2.2 Why an uneven feed costs capacity

The measurements, all on this drive and the test cartridge EW7VWMVKF6:

| Run | Feed | Native per data byte | Source |
|---|---|---|---|
| 2026-09-23 run 1, `openssl enc` pipe (bursty) | 94.8 MB/s | 1.48 | RECORD |
| 2026-09-23 run 2, 2 GiB file from `/dev/shm` | 151 MB/s | 1.006 | RECORD |
| 2026-09-23 run 3, tapectl 1.0.x serial loop (steady) | 56 MB/s | 1.000 | RECORD |
| 2026-10-13, 1.1.0, java plus vm-desk1 busy (#426) | 105.4 MB/s | 1.505 | RECORD |
| 2026-10-13, 1.1.0, quiet host, tmpfs and md3 | 146.5 MB/s | 1.0000 | RECORD |
| 2026-10-20 profile fill, Python feeder | 158.2 MB/s | 1.0009 of nominal at early warning | RECORD |

A slow steady feed costs nothing (run 3). An uneven feed costs about half a tape.

The raw captures of runs 1 and 2 are still on vm-desk1 (`/scratch/tapectl-lto6/unit182-20260923-021914/`).
They show where the extra tape went:

| | run 1 (bursty) | run 2 (steady) |
|---|---|---|
| page 0x17 "Total data sets written", before → after | 13,205 → 21,903 | 21,906 → 30,601 |
| data sets for 21,474,836,480 bytes | **8,698** | **8,695** |
| bytes per data set | 2,468,940 | 2,469,791 |
| page 0x0c BOP→EOD (each run from BOT) | 31,874 MB | 21,607 MB |
| native tape per data set | **3.66 MB** | **2.48 MB** |

(OBSERVED. The 13,205 is from the fixture captured at 02:15, before run 1 at 02:19.)

The bursty run wrote the same data sets, each full. It spread them 1.47 times wider.
- **Padding is not the mechanism.** A padded flush would have added data sets.
- **Inferred mechanism:** each underrun makes the drive stop, reverse, and restart. The restart
  appends after a run-up, and leaves unused tape before the next data set.
- **Not settled:** whether the drive also writes fill patterns while it waits. If so, it costs
  the same tape without a direction change. Page 0x34's reposition count against the BOP→EOD
  excess will tell the two apart (§10, P0).
- **The cost of one stop is not measured.** §10 gets it as (BOP→EOD excess) ÷ (repositions).

### 2.3 How tapectl feeds the drive

- **Three threads per file** (CODE: `pipeline::write_verified`):
  - a reader (`tapectl-stage-read`) reads the staged slice through `DropBehind`
    (`POSIX_FADV_SEQUENTIAL`, drop-behind every 64 MiB);
  - a hasher (`tapectl-hash`);
  - the calling thread writes 512 KiB blocks to `st`.
- **The queue.** At most `QUEUE_BYTES` = 256 MiB of buffers are in flight, in one pool that both
  queues share: read but not yet hashed, and hashed but not yet written.
- **The hasher holds each file's last block** until the hash is done (tri-layer L2).
- **The pipeline is per file.** It is built, drained and joined for every entry of the Layout. At
  each file boundary the queue empties. The writer then waits for the next file's first block to
  be read and hashed, and the drive buffer covers that gap.
- **The gap is small on a quiet host** (MEASURED, home2 lifecycle session log
  `20261020T022533Z-volume-write-VOL-A-7049.log`):
  - the writer waited 10-12 ms for the first block of each 1 MiB slice;
  - between one file's end and the next file's start, about 1 ms passed.
- **Production slices are 1 GiB** (CODE: `default_slice_size`, "≈ 2,300 slices per LTO-6
  tape"). At 146.5 MB/s each takes 7.3 s, so the boundary happens every 7 s.
- **Between files, the writer thread does catalog work.** It updates the slice's
  `write_positions` row under `busy::retry`. "A busy catalog is waited out (minutes)" (CODE). A
  catalog held busy for longer than the drive buffer lasts would stop the drive (INFERRED). The production catalog lives on sda, a
  PERC H700 volume shared with `/`, `/var` and two VM disks (OBSERVED, `lsblk`).

### 2.4 The arithmetic that decides every lever

Let D be the drive's tape rate, P the producer's rate (the slowest of read and hash), Q the bytes
hashed and waiting, and B the drive's buffer (about 418 MB). The producer stalls for s seconds.
The tape keeps moving through the stall if Q + B ≥ D × s at its start. After it, the queue refills
at P - D. That holds only when P > D.

- **Today on a quiet host,** P ≈ 146.5 and D rises to meet it, so P - D ≈ 0. Q never fills.
  B fills only as far as the speed matching leaves it, an unknown level (NEEDS DRIVE: the queue
  trace in §9). A stall reaches the drive almost at once.
- **Pace the writer at R < P,** for example R = 110. The drive then settles at D = R, and Q fills
  at P - R = 36 MB/s. A 4 GiB queue then absorbs a 39 s stall and refills in 118 s.
- **A faster hash,** `ring` on home2 at about 188 (INFERRED), paced at R = 140: the queue
  absorbs 31 s and refills in 89 s.

The cost is time. A 2.5 TB tape takes 4.7 h at 146.5 MB/s, 5.0 h at 140, 6.3 h at 110 and 6.9 h
at 100 (arithmetic).

---

## 3. home2 today

| | Value | Tag |
|---|---|---|
| Platform | Dell PowerEdge R710, BIOS 6.6.0 | OBSERVED (DMI) |
| CPU | 2 × Xeon L5640 (Westmere-EP), 6 cores × 2 threads = 24 logical CPUs. CPU *n* and *n+12* are SMT siblings. Two NUMA nodes (even CPUs node 0, odd node 1) | OBSERVED (`lscpu`, sysfs) |
| ISA | SSSE3, SSE4.2, AES-NI, PCLMULQDQ. **No SHA-NI, no AVX** | OBSERVED (`/proc/cpuinfo`) |
| Frequency | No OS cpufreq driver loaded. `/proc/cpuinfo` showed 1,596 MHz on idle CPUs and 2,696 MHz on a busy one (APERF/MPERF), so the firmware manages P-states and turbo. The L5640's top turbo is 2.8 GHz (Intel spec); fewer turbo bins with more cores active | OBSERVED; turbo bins INFERRED |
| sha256, one core | OpenSSL 1.1.1n, 512 KiB blocks: **188.2 MB/s** (191 on 2026-10-13, RECORD #326) | MEASURED |
| tapectl's own sha256 | about **142-157 MB/s** (§1, §5.9) | INFERRED |
| RAM | 64,409 MB total; 38,534 MB available; swap 64 GiB with **4.5 GB in use** | MEASURED (`free -m`) |
| Kernel | Debian 4.19.0-27. **No PSI** (`/proc/pressure` absent). PTI on. `sched_autogroup_enabled` 0 | OBSERVED |
| Load signals that exist | `/proc/loadavg`; `/proc/stat` (iowait; steal is 0 on a host); `/proc/schedstat` version 15 with per-CPU run delay; **per-thread run delay** in `/proc/<pid>/task/<tid>/schedstat`, kept up although `kernel.sched_schedstats` is 0 (a busy vCPU thread's wait field advanced 2.6 ms in 2 s); `/proc/diskstats` for member disks (md devices report zero time fields) | MEASURED |
| cgroups | v1 hybrid, systemd 241. An operator shell sits in `cpu:/user.slice`. **All 64 libvirtd/qemu vCPU threads sit in `cpu:/system.slice/libvirtd.service`.** `cpu:/machine/qemu-3-vm-desk1…` and its `vcpuN` groups hold 0 tasks (the `emulator` group holds 2). Docker containers sit in `cpu:/docker/…`. Every top group has `cpu.shares` 1024 | MEASURED |
| cpuset | `cpuset:/machine/qemu-3-vm-desk1…/vcpu0` holds its thread, so vCPU pinning takes effect | MEASURED |
| VMs | vm-desk1: 16 vCPUs floating on 0-23, emulator 0-23, 10 GiB, `cpu_shares` 1024, blkio weight 500 (defaults). h1win10: 4 vCPUs pinned to 20-23, emulator and iothread on 0-3. vm-jump1 and vm-fs1: 1 vCPU each, floating | MEASURED (`virsh -r -c qemu:///system`) |
| Busy at inspection | load average 1.1-2.6; vm-desk1's qemu at 232% CPU; kcryptd kworkers at nice -20 among the top CPU users | MEASURED (`ps`) |
| Production staging | `/srv/acache/tapectl-staging` on md1: RAID0 of sdc (WDC WD20EZRX) and sdd (Hitachi HDS722020ALA330), 512 KiB chunks, LUKS, 2.2 TB free. Since boot, **sdd averages 16.8 ms per read, sdc 3.6 ms** | MEASURED (`/proc/diskstats`); profile CODE |
| Test staging (#426) | `/srv/google_copy` on md3: RAID0 of two WDC WD40EFRX, LUKS; a 100 GB LV with 67 GB free. Also holds `/srv/local_backup` and `/srv/worm1` | MEASURED |
| I/O schedulers | `mq-deadline` on sdc, sdd, sdh, sdi; read-ahead 2,048 KB on md and dm | MEASURED |
| Tape path | LSI SAS2008 at 07:00.0; staging disks behind another HBA at 06:00.0; `numa_node` -1 for both (one IOH) | OBSERVED (sysfs) |
| Catalog | `/srv/archive_meta` on sda (PERC H700, LUKS), shared with `/` and `/var` | OBSERVED |
| Tools present | `taskset`, `chrt`, `ionice`, `systemd-run`, `python3`, `openssl`, `perf` (but `perf_event_paranoid` = 3). **Absent:** stress-ng, fio, iostat, pidstat, sar | MEASURED |
| tapectl | `/usr/local/bin/tapectl` 1.5.0; `tapectl-op` raises tape commands to `nice -10` and `ionice -c2 -n0` | OBSERVED |

The issue says staging is md3. Production staging is md1 (`contrib/hosts/home2.profile`
`STAGING_DIR`); #426's measurements used md3. md3's staging LV (67 GB free) cannot hold a tape.

---

## 4. Where a contended write stalls (question 1)

Nothing measured yet splits #426's slowdown by stage. The per-file session-log lines ("tape
waited … for data, queue full …") of that run are gone with its scratch home. What is known:

- **Throughput fell 28%.** The write ran at 105.4 MB/s against 146.5 quiet (RECORD), with about
  8 of 24 CPUs busy (java about 6 cores, vm-desk1 about 2).
- **At this headroom, any slowdown of the hasher is a slowdown of the drive's feed** (§2.4).
  These candidates fit:
  1. **SMT sharing.** A hasher whose sibling thread runs a vCPU or java gets a fraction of the
     core (Westmere SMT: INFERRED, typically 55-65%). The scheduler prefers idle cores but does
     not guarantee one when 8 threads float over 12 cores.
  2. **Turbo.** With more cores active the turbo bins fall (2.8 GHz towards 2.27). That alone is
     up to 19% of the hash rate (INFERRED).
  3. **Cross-cgroup scheduling.** CFS shares a CPU between cgroups by group weight, and nice
     applies only within a group (DOC: kernel `sched-design-CFS`, group scheduling). tapectl at
     nice -10 in `user.slice` and a vCPU in `system.slice/libvirtd.service` on one CPU split it
     by the groups' per-CPU weights, not 9.3:1 (INFERRED).
  4. **Staging reads through dm-crypt.** Decryption runs in `kcryptd` kworkers at nice -20 on
     any CPU (MEASURED: `ps`). A delayed decrypt is a delayed read. The reader is not the
     bottleneck on a quiet host (tmpfs and md3 equal, RECORD), but a burst of stalls on md3
     under java's I/O is possible. #426 did not measure disk activity.
  5. **The writer thread's catalog update between files** (§2.3), if sda was busy.
- **Distinguishing them needs the per-thread trace** (§9): the hasher's run delay and CPU, the
  queue split, the writer's waits, `write(2)` latency, and member-disk latency. §10's load
  ladder applies one contender at a time.
- **Stop rule.** If no CPU load reproduces about 1.5 and the I/O load does, the cause is the
  staging path. If nothing does, the 1.505 was not reproducible, and the record says so.

---

## 5. The levers (question 3)

| Lever | Expected effect | Memory | Time | Complexity | Needs from the CTO |
|---|---|---|---|---|---|
| 5.1 Queue size, read-ahead | None alone today (P ≤ D). With headroom, absorbs stalls of Q/R seconds | Q (e.g. 4 GiB) | none | small: a constant, plus carrying the queue across files | the size; constant or key |
| 5.2 Fixed pace | Steady feed; Q fills at P - R | none | +14% at 125, +33% at 110 (vs 146.5) | small: a token bucket on the writer | the rate, or the rule that derives it |
| 5.3 Adaptive pace | As 5.2, but uses quiet time | none | between | medium | marks, step, floor |
| 5.4 Pause and refill | One deliberate stop instead of many when the host cannot sustain even the pace | Q | the pause | medium | marks; whether to also wait for a quiet host |
| 5.5 Gate during the write | Pauses for a loud host | none | the waits | medium | the signal and its thresholds |
| 5.6 Pinning | Removes SMT, cgroup and VM contention from the hasher | none | none | host-side: small | a host decision with homorg's founder |
| 5.7 ionice, nice, libvirt weights | **None on home2 as configured** | | | | whether to correct the docs and home2-prep.sh |
| 5.8 Staging: dedicated disk or tmpfs | Removes staging I/O contention | tmpfs: the set | none | none | the disk (§8) |
| 5.9 A faster hash | +32% producer (MEASURED on vm-desk1) | none | faster | a new dependency, one seam | the crate |
| 5.10 Real-time priority | Strong, but can starve kworkers | none | none | small | not recommended |

### 5.1 Queue size and read-ahead

- **Today a larger `QUEUE_BYTES` does nothing,** because the producer cannot run ahead
  (§2.4). Prove it once (§10, R6) and move on.
- **With headroom it is the cushion.** Size it as R × (longest stall to ride out). 30 s at 140
  MB/s is 4.2 GB.
- **It must survive file boundaries,** or each 1 GiB slice starts with an empty cushion. Two ways:
  - **Carry it across files:** one reader and one hasher for the whole session, with file
    boundaries as messages. The hasher still withholds each file's last block until its own
    verdict, which tri-layer L2 requires. Ctrl-C stays a between-files check on the writer side.
  - **Read the next file ahead** into a second pool while the current one drains. This is simpler
    to bolt on, but it doubles the memory.
- **"A whole slice or more"** is just a queue of at least 1 GiB. The verdict for a file then comes
  before its first block is written, not only before its last. That is stronger than L2 needs.
- **Memory.** home2 has 38 GB available, but 4.5 GB of swap is already in use. A multi-GiB pool
  that pages out becomes a new stall. Lock it (`mlock`, which needs `RLIMIT_MEMLOCK` raised by
  root, as `tapectl-op` could), or record the process's `VmSwap` in the trace and keep the pool
  modest.

### 5.2 Fixed pace

- **A token bucket on the writer thread,** at R MB/s with a burst of a few blocks. The drive sees
  a steady R and settles on one speed of its 54-160 MB/s range.
- **What R should be.** Below the producer's sustained rate under the load to be tolerated, and
  well above the 54 MB/s floor.
- **On home2 with `sha2`:** about 110-125 MB/s, a 15-25% slowdown. The 105 MB/s average of
  #426's contended run says the host could have sustained close to that.
- **With `ring`:** about 140-150.
- **Question 3's last point** ("is pacing below about 150 MB/s the right default on this CPU?"):
  with `sha2`, 150 is above what the host delivers, and the write is already hash-paced at about
  146 with no margin. With `ring`, 140-150 is right.

### 5.3 Adaptive pace

- Step R down when Q falls below a low mark, and up when it stays above a high mark, within
  [floor, drive max].
- It uses quiet time and backs off under load. Each step is a speed change for the drive, not a
  stop. HP says speed matching works "dynamically and continuously" (QS p9).
- **NEEDS DRIVE:** whether a change of feed rate itself costs a reposition (§10, R10).
- Keep steps small and infrequent (for example 10% at most every 30 s), so the drive is not
  chasing a moving rate as it did with the bursty pipe.

### 5.4 Pause and refill (hysteresis)

- **The rule.** When Q falls below a low mark (for example 10%) even at the floor pace, stop
  feeding until Q reaches a high mark (for example 90%). Optionally also wait until the host
  signals are quiet. Then resume at R.
- **One stop per episode** instead of one per underrun. Its cost is one reposition, which §10
  measures in tape and seconds.
- **Where to pause.** The file boundary is the natural place in tapectl's code, but not for the
  drive: immediate filemarks mean the drive sees no boundary (§2.1). Pausing mid-file is
  physically the same.
- **Keep it under 30 s,** or end it before the write-delay flush (§2.1). Otherwise the drive also
  flushes and pads a partial data set.
- **Prerequisite:** with today's per-file pipeline, the queue is empty at every boundary. A pause
  there must refill from the next file. That works: start the next file's pipeline and hold the
  writer until Q reaches the high mark. But it must be conditional (only after the producer lost
  ground), or every file becomes a stop.

### 5.5 A gate during the write

- **The trigger** should be the queue level: hashed bytes waiting, plus the writer's waits. That
  is the ground truth of "falling behind".
- **Host signals** explain and predict, but they misfire both ways: a high load average with
  idle cores, or a quiet load with a busy SMT sibling. Use them:
  - to delay a resume until the host has been quiet for N seconds (5.4's option);
  - to log why.
- **Available on 4.19 without PSI:**
  - the hasher thread's own run delay (`/proc/self/task/<tid>/schedstat`, field 2);
  - per-CPU run delay (`/proc/schedstat`);
  - `/proc/stat` iowait;
  - the staging member disks' utilisation and await (`/proc/diskstats`, members, not md);
  - `/proc/loadavg`;
  - the contender list.
- **Never** the drive's log pages during the write (HP-V2 p23).

### 5.6 CPU and vCPU pinning

What works on home2 (MEASURED placement):
- **The host side.** Reserve two physical cores, both threads of each: for example (6, 18) and
  (7, 19). Those are clear of h1win10's pins (20-23, siblings 8-11) and its emulator (0-3). Keep
  every guest off them during a tape command, with `virsh vcpupin <dom> <vcpu> <cpuset> --live`
  for each vCPU and `virsh emulatorpin <dom> <cpuset> --live`, and restore afterwards.
  vm-desk1 then runs its 16 vCPUs on 20 logical CPUs.
- **The tapectl side.** `taskset -c 6,7,18,19 tapectl …`, inherited by every thread. This needs
  no tapectl change; it can live in `tapectl-op`, which already runs tape commands as root.
- **Optional.** The unbound kcryptd workqueues can be kept off the reserved CPUs through
  `/sys/devices/virtual/workqueue/cpumask` (root; INFERRED that 4.19's dm-crypt queues honour it).
- **Pinning tapectl alone, without moving the guests, is worse than nothing.** A pinned hasher
  cannot leave a contended CPU.
- **Cost:** vm-desk1 loses 4 of 24 logical CPUs for the length of a write. This is a host
  decision for the CTO and homorg's founder, as home2-prep.sh already frames it.

### 5.7 ionice, nice and libvirt weights

All measured on home2 today:
- **ionice:** the staging members (sdc, sdd, sdh, sdi) use `mq-deadline`. Before Linux 5.14,
  mq-deadline did not act on I/O priority (DOC: `ionice(1)` needs a scheduler that supports
  priorities; the mq-deadline priority support landed in 5.14, INFERRED from kernel history,
  unverified). On 4.19, `ionice -c2 -n0` changes nothing.
- **nice:** see §4 item 3. `nice -10` beats other tasks in `user.slice` only.
- **`virsh schedinfo vm-desk1 --set cpu_shares=256`** (home2-prep.sh §9, printed, never applied)
  writes `cpu:/machine/qemu-3-vm-desk1…`, which holds no vCPU thread (0 tasks; the threads are in
  `system.slice/libvirtd.service`). As placed today it would change nothing.
- **`blkiotune`:** needs CFQ or BFQ, which home2-prep.sh itself says. vm-desk1's disks are on
  sdb (PERC), not the staging arrays, anyway.
- **Real-time priority** (§5.10): `chrt -r 1` on tapectl's threads would beat every CFS thread,
  and RT throttling keeps 5% of each CPU (`sched_rt_runtime_us`, default 950,000). But a
  CPU-bound RT hasher can starve per-CPU kworkers on its CPU for seconds. With pinning it is
  unnecessary. Not recommended.

### 5.8 Staging: a dedicated disk or tmpfs

- **tmpfs takes the disk out.** #426 E1 staged 16.6 GiB in `/dev/shm`. It removes disk and
  dm-crypt variance, not CPU contention: quiet, tmpfs and md3 gave the same time.
- **A 4 GiB hashed queue gives most of that benefit** at a fraction of the memory, and it works
  for any set size. tmpfs makes sense only for small sets on a host with I/O contention.
- **A dedicated disk** is §8.

### 5.9 A faster hash

MEASURED on vm-desk1, release builds, pinned to one vCPU, 512 KiB updates, best of 3 × 2 GiB, two
interleaved rounds, glibc target:

| Implementation | MB/s |
|---|---|
| `sha2` 0.10.9, default (soft) | 139.5-142.1 |
| `sha2` 0.10.9, `asm` (what tapectl ships) | **163.6** |
| `ring` 0.17 `digest` | **215.0-216.8** |
| `aws-lc-rs` 1 `digest` | 216.1-217.6 |
| OpenSSL 3.0.13 CLI (`openssl speed -evp sha256 -bytes 524288`) | 216.5-216.9 |
| coreutils 9.4 `sha256sum` (another moment, so not comparable) | 168.6 |
| static musl build (`x86_64-unknown-linux-musl`, the repo's 1.94.1 toolchain): `sha2` asm / `ring` | 162.4 / **216.7** |

- **The gap.** `ring` and aws-lc carry OpenSSL's SSSE3 SHA-256 code. `sha2`'s `asm` feature does
  not reach it (about 0.76 of OpenSSL here; 163/195 = 0.84 on 2026-09-23, RECORD #326).
- **On home2,** with OpenSSL at 188 today: `sha2` asm about 142-157, `ring` about 188.
- **Effect:** the producer goes about 32% faster, from at the drive's rate to about 18% above it.
  That gives §5.1 and §5.2 something to work with. It also speeds every other hashing path
  (verify ran at 140.3 MiB/s in production, RECORD #326, the same cap) and staging.
- **Cost:**
  - `ring` builds C and assembly with `cc`.
  - Production is a static musl build (`docs/handoff.md`). `ring` 0.17 builds static-pie for
    musl with the repo's toolchain and keeps its speed there (MEASURED, table above).
  - `pass_count::Sha256` wraps `sha2::Sha256` through the `Digest` traits, and `ring` has no
    `Digest` impl, so that seam changes shape.
  - The budget tests count bytes through it.
- **A single stream cannot be hashed in parallel.** Two hashers on alternate files need a whole
  slice read ahead (5.1), which is more complex than a faster single core.

---

## 6. A direct wear counter (question 2)

**Page 0x34, Performance Data** (DOC: HP-V3 p91), five 2-byte parameters, with parameter
control byte 60h (the drive gives parameter 4 control byte 40h in both captures, OBSERVED):

| Param | Name | HP's definition | Updated |
|---|---|---|---|
| 0 | Repositions per 100 MB | "Number of tape direction changes per 100 MB written. The higher the figure the more wear occurs." | after each 100 MB physically written |
| 1 | Data rate into buffer | after compression, 100 KB/s units; "the true 'tape' data rate" | after each data set logically written |
| 2 | Maximum data rate | what the data's compressibility would allow | the same |
| 3 | Current data rate | accepted before compression, 100 KB/s units | the same |
| 4 | Native data rate | 640h = 160 MB/s for LTO-6 | at load and unload |

- **How it behaves.** "All the parameters relate to the current tape. They are all cleared at
  the start of tape load, except Parameter 4 … All data will be reset when the log page is reset"
  (HP-V3 p91).
- **HP-V2 p20 disagrees with HP-V3.** It says parameter 4 reads 07D0h (200 MB/s). The drive reads
  `0x0640`, 160 MB/s, in both captures (OBSERVED), as HP-V3 says.
- **The drive lists 0x34 on page 0x00, and the bytes have HP's layout** (OBSERVED,
  `tests/fixtures/sg_logs/*/page_0x34.bin`).
  - With the test cartridge just loaded: parameters 0-3 all 0.
  - In the no-medium capture of 2026-09-23 01:11: parameter 0 = 5 repositions per 100 MB, and
    parameter 1 = 0x037f, 89.5 MB/s into the buffer. That is the state left by an earlier tape;
    its write is unknown. It proves the counter is live, not what any run did.
- **sg_logs mis-decodes it.** sg3-utils 1.81 decodes 0x34 as IBM's "Read forward errors
  (lto-5)" and then "Unable to decode" (OBSERVED: the fixtures' `page_0x00.decoded.txt` and
  `page_0x34.decoded.txt`). home2's 1.44 was not tried. tapectl must parse 0x34's raw bytes
  itself, keyed on vendor HP.
- **How to read it.** `sg_logs --page=0x34 --maxlen=65532 --raw`, which tapectl's sweep already
  does after every command that contacts the drive (CODE: `log_pages::sweep` reads every listed
  page). Read it before a write and after the write phase, never during.
- **How to reset it:**
  - A tape load clears it. The drive is standalone, so a reload needs the operator's hands.
  - LOG SELECT with PC = 01b and a parameter list of one 4-byte header for page 0x34 clears just
    that page (DOC: HP-V3 p59). LOG SELECT with PCR = 1 resets "all the logs on the drive that
    can be reset", which is too broad.
  - Any LOG SELECT forces a flush (HP-V3 p255), so only between runs.
  - Whether the per-page form works on this firmware is NEEDS DRIVE. The protocol avoids the
    question by using deltas.
- **Unknown: is "per 100 MB" an average since load, or the last 100 MB?** §10 P0 settles it
  first, because every later reading depends on it. A 2-byte integer per 100 MB also has a
  resolution floor: one reposition per GB reads 0 or 1.

Cross-checks and fallbacks, all monotonic or per-load and read at phase boundaries:
- **Page 0x14 parameter 0004h, "Lifetime meters of tape processed"** (DOC: HP-V3 p77). It reads
  534,951 m on this drive (OBSERVED, fixture). Each direction change adds overshoot and run-up
  metres; a streaming write adds about its share of a wrap. Metres per GB written across a run
  is a second wear measure.
- **Page 0x17 or 0x30 "Total data sets written"** (HP-V3 p82, p89) together with page 0x0c
  BOP→EOD gives native tape per data set (§2.2).
- **The feed ratio** (CODE: `tape::feed_ratio`) stays as the after-the-fact capacity figure.
- **st's own write timing** (`/sys/class/scsi_tape/nstN/stats/write_ns`, `write_cnt`) and a
  `write(2)` latency histogram in the trace show when the drive buffer was full: a long
  `write(2)`. They do not show an empty buffer; the writer's wait for data shows that.
- **Production data already exists.** Every production write's post-command sweep journalled
  0x34 and 0x14 raw in `log_page_journal` (CODE), and its feed ratio as a `write_feed_ratio`
  event. L6-0001's and L6-0002's repositions per 100 MB can therefore be decoded at zero drive
  time. I could not read the production home (permission denied, and out of bounds), so that is
  R0 for the operator.
- **Caveat: the post-command sweep runs after confirm,** which rewinds and spaces. If 0x34
  counts direction changes outside writes, confirm pollutes it. §9 adds a sweep at the end of
  the write phase for the research build.

---

## 7. A host profile (question 4)

The counterpart of `scripts/lto-profile.sh`: a script, `scripts/host-profile.sh`, that measures
the host and prints a proposed block. Like the drive profile, it changes nothing itself.

**What it measures, and why each figure is needed:**

| Step | Measures | How | Takes |
|---|---|---|---|
| H1 inventory | CPUs, SMT siblings, NUMA, turbo evidence, kernel, PSI, schedstat, cgroup version and where `tapectl-op` lands, the staging members' I/O schedulers, guests and their pins (if `virsh -r` answers) | `lscpu`, sysfs, `/proc`, `virsh -r -c qemu:///system` | seconds |
| H2 tapectl's own hash rate | one core alone; one core with its SMT sibling busy | **tapectl's** hash code, not OpenSSL's (the 191-vs-146 confusion is exactly this). Needs a hidden subcommand or a shipped probe | 2 × 10 s |
| H3 staging read rate | sustained sequential read of the staging filesystem through the same path as `DropBehind`, p50 and p99 per 512 KiB read; then the same under a concurrent `dd` writer on the same array | O_DIRECT and buffered reads of a 10 GiB file it writes and removes | about 3 min |
| H4 memory | `MemAvailable`, swap in use, `RLIMIT_MEMLOCK` | `/proc` | seconds |
| H5 the drive's floor and ceiling | 54 and 160 MB/s for LTO-6 | the drive profile or `src/media.rs`'s generation table (the floor would be a new column) | none |

It takes about 5 minutes, or 10 with the loaded variant of H3.

**What it emits.** Each key is a CTO decision under ADR-0012's unknown-key rule. None exists
today, and none is shown here as a config file.

| Proposed key | Derivation | Why needed | Example (home2, `ring`) |
|---|---|---|---|
| `[write_shaping] pace_mb_s` | 0.85 × min(H2 with busy sibling, H3 p10 under load, drive max); refuse below 1.25 × floor | the steady rate the drive settles on | about 140 |
| `[write_shaping] queue_bytes` | pace × the stall to ride out (30 s), within [1 GiB, MemAvailable / 4] | the cushion | "4G" |
| `[write_shaping] refill_low`, `refill_high` | fixed fractions (0.10, 0.90) unless measured otherwise | the hysteresis marks | 0.10, 0.90 |
| `[write_shaping] cpus` | the reserved cores, if the host reserves any | affinity inside tapectl instead of `taskset` in the wrapper | "6,7,18,19" |

**There is a zero-key alternative.** The write measures its own producer rate during its first
GiB, unpaced and with the queue filling, and paces at 0.85 of it. It needs no profile and no
key, but it is fooled by a host that is loud at the start. Keep `QUEUE_BYTES` a constant, as now,
and the marks too. The profile would then only check that the host can reach 1.25 × the floor.

---

## 8. The new disk (question 5)

- **Dedicated to staging,** written by nothing else, so a write's reads never meet another
  writer.
- **Size.** At least one tape of slices: a full plan at the 0.97 fill ceiling of 2.5 TB is 2.43
  TB of ciphertext, and the cartridge's native size is 2.62 TB (RECORD). 3 TB or more leaves
  room for a second set staging while one writes, and ADR-0012 items 94 and 115 need a whole
  cartridge staged for real.
- **Rate.** One HDD's inner zone reads at about half its outer rate (INFERRED, typical of
  HDDs). A large single disk may fall below 160 MB/s near its end. Speed matching handles a
  steady 110 cleanly, so that costs time, not tape, provided the pace is set from H3's worst
  case. Two disks in RAID0, or an SSD, avoid it.
- **LUKS.** Staging holds only age ciphertext (#370: no plaintext on the staging device). LUKS
  on it encrypts ciphertext again and puts the kcryptd workers in the read path (§4 item 4).
  Without LUKS that cost goes. This is a CTO decision: it changes home2's at-rest posture for
  that disk, though not for the data.
- **HBA.** The staging disks and the tape are already on separate HBAs (OBSERVED), as HP-V2 p23
  recommends.
- **Scheduler:** mq-deadline is fine once nothing else reads the disk.

---

## 9. Instrumentation to build before the drive phase

Everything off the tape thread. A sampler thread writes one line per 250 ms; the cost is one
`/proc` read per thread per interval.

1. **Pipeline trace,** `<home>/logs/<session>.pipeline.tsv`. Columns:
   - time and the current file;
   - bytes written;
   - **queue bytes read but not hashed, and hashed but not written**;
   - the writer's `write(2)` max and p99 latency in the interval;
   - writer waited, reader waited;
   - hasher busy;
   - for the reader, hasher and writer threads: `/proc/self/task/<tid>/schedstat` (run ns, run
     delay ns) and the CPU each last ran on (`stat` field 39).
2. **Host sampler,** in the same file:
   - `/proc/stat` deltas (user, system, iowait);
   - `/proc/schedstat` run delay summed and for the CPUs the threads used;
   - `/proc/diskstats` for the staging filesystem's member disks (util%, await), resolved
     through the dm and md slaves in sysfs;
   - `/proc/loadavg`;
   - the process's `VmSwap`.
3. **The inter-file gap** as its own session-log field: from the last block of file N to the
   first block of file N+1. Split it into where it went:
   - time inside the `write_positions` UPDATE, including any `busy::retry` waits;
   - the pipeline's join and restart, through the next file's first hashed block.

   The 1 ms gap measured so far (§2.3) is from a 12.7 MiB write of 1 MiB slices on a quiet
   host and a scratch catalog. It says nothing about a busy sda, which §4 item 5 names.
4. **A sweep at the end of the write phase,** before the seal, in the research build: pages 0x34,
   0x14, 0x0c and 0x17, journalled with a phase tag. This avoids confirm's rewinds polluting the
   reading (§6).
5. **An HP 0x34 decoder** (raw bytes to five named parameters), and the repositions figure added
   to the `write_feed_ratio` event's details. A small product change. It lets `report events` show
   wear beside capacity on every production write, including the ones already journalled.
6. **Experiment knobs,** read once at session start and announced loudly, in a research build
   only: environment variables, never config keys, following `TAPECTL_TEST_PAUSE_AFTER_PLAN`'s
   pattern:
   - `TAPECTL_EXP_QUEUE_BYTES`;
   - `TAPECTL_EXP_PACE_MBPS`;
   - `TAPECTL_EXP_PACE_ADAPTIVE` (low, high, step);
   - `TAPECTL_EXP_REFILL` (low, high);
   - `TAPECTL_EXP_HASH` (`sha2` or `ring`);
   - `TAPECTL_EXP_CROSSFILE` (carry the queue across files).
7. **A dry run on vm-desk1 with `MemStore`** (no tape) under the same loads, to check the trace
   and the knobs before any drive time.

### 9.1 As built (2026-10-20, the CTO's §12 decisions 1, 3 and 4)

**The hash (§12 item 4).** `pass_count::Sha256` hashes with `ring`'s SHA-256; its public
surface (`sha2`'s `Digest` traits, every byte counted once in `update`) did not change, and
`tests/pass_budget.rs` passes untouched. Production hashing on the write, verify and staging
paths goes through it; the only production `sha2::Sha256` left is `progress::session_id_for`'s
digest of a command line. Debug build, vm-desk1, 256 MiB in 512 KiB updates: `ring` 212.4
MB/s, `sha2` asm 156.9 (MEASURED, 1.35x; the release figures are §5.9's). The musl target
checks with it.

**Item 5, the 0x34 decoder.** `tape::log_pages::HpPerformance` decodes the five parameters from
the raw page, keyed on vendor `HP` (`drives.vendor`, exactly "HP" after trimming; HPE drives are
not assumed). `write_feed_ratio` events carry `repositions_per_100mb` in `details` from the
same contact's sweep, absent when the drive is not HP or no 0x34 was read. `report events`,
`report health` and `volume info` show it beside the ratio; for an event written before, the
figure is decoded from the 0x34 row the event's `details.contact_id` journalled (the ratio's
own 0x0c came from that contact's sweep, so it is the same moment). R0 is therefore
`tapectl report events --entity volume` on a copy of the production catalog.

**Items 1 and 2, the trace.** `TAPECTL_PIPELINE_TRACE=1`, in every build, off by default:
`<session>.pipeline.tsv` beside the session log, one row per 250 ms
(`tapectl::pipeline_trace::COLUMNS` documents each column). Its lifetime is the write pool's,
so one trace covers every file of an execute loop without a session hook. As built:
- `wr_n`, `wr_max_ms`, `wr_p99_ms` time the store between its successive reads of the queue
  (one block read, one `write(2)`, the next read): `write(2)` plus a block copy. The p99 is the
  upper edge of its quarter-octave bucket. The exact `write(2)` would need `tape/ioctl.rs`.
- Waits are counted while they last (a 4 s stall spreads over the rows it spans);
  `writer_wait_max_ms` is the longest single wait that ended in the row.
- Per-file stage threads add their last stretch of run and delay to the role when they exit,
  so nothing is lost between rows; `*_cpu` is empty between files.
- The disk columns need the staged file's path, which only the write session knows: it must call
  `BufferPool::trace_source(path)` before each `write_verified` (the hook below). Until it
  does, `file` reads `#N` and the disk columns are empty. A path on no block device (tmpfs)
  gives empty disk columns, never an error.
- A trace that cannot start says why once in the session log; the write goes on.

**Item 6, the knobs.** Behind the cargo feature `research` (off by default, in no production
build; `tapectl::research`). Read once per process, printed on stderr and into each session log
when a write's pool starts. A research build announces itself even with no knob set. Without
the feature each set variable draws one warning line and is ignored; the shaper type cannot
be constructed. Semantics as built (decimal MB/s throughout):
- `TAPECTL_EXP_QUEUE_BYTES=<size>` (`4G`, `512M`, bytes): the write pipeline's queue in place of
  `QUEUE_BYTES`; the read pipeline keeps 256 MiB. Without the cross-file carry the queue still
  empties at each file boundary, so it holds at most one file.
- `TAPECTL_EXP_PACE_MBPS=<R>`: §5.2's fixed pace, a token bucket on the writer's side of the
  queue with a four-block burst, carried across files; the trace's `paced_ms` and `pace_mbps`.
- `TAPECTL_EXP_PACE_ADAPTIVE=<floor>,<ceiling>,<step %>`: §5.3. **The ambiguity resolved:** the
  two numbers are the rate band (R10's "150/90" is ceiling 150, floor 90), not queue marks.
  The marks are fixed at 0.25 and 0.75. It starts at `TAPECTL_EXP_PACE_MBPS` if set, else the
  ceiling. Once per 30 s it steps down by `step` % if the store was starved mid-file or the
  window's lowest queue fill was under 0.25, up if it stayed at or over 0.75, and otherwise holds.
- `TAPECTL_EXP_REFILL=<low>,<high>`: §5.4. Mid-file, when the queue falls under `low` after
  having reached it in the file (or the store was starved), the writer holds until `high`, at
  most 25 s (under the 30 s write-delay flush); the trace's `paused_ms`.
- **Queue fill**, for both: hashed bytes waiting ÷ what the queue can still hold of this file
  (the pool, or the file's untaken remainder if smaller). A file's first fill-up is not judged.
- `TAPECTL_EXP_HASH=ring|sha2`: the hash behind `pass_count::Sha256`, for R7's `sha2` baseline;
  it applies to every hash in the process.
- `TAPECTL_EXP_CROSSFILE`: not built (below); set, it is reported as ignored.

**Item 7, the dry run.** `tests/pipeline_trace.rs`, ungated (about 3 s): four 20 MiB files from
a real disk through `write_verified` (reader through `DropBehind`, the hash, `MemStore` behind a
throttle), the trace on, and the TSV checked for its header, strictly monotonic times, bytes
that end at the total, a queue within the pool, timed blocks, followed threads, about 250 ms a
row, and (positive control) resolved disks. It drives the pipeline directly, calling
`trace_source` where the session would.

**Deferred, each needing `volume/session.rs`:**
- **The staging path for the trace:** in `run_entries`, before
  `pipeline::write_verified(&mut pool, file, ...)`, call `pool.trace_source(path)` (`path` is
  `entry_path(entry)?`, already in hand).
- **Item 3, the inter-file gap:** in `run_entries`, time the `busy::retry(... "a slice's write
  position" ...)` block, and the span from one `write_verified` return to the next one's first
  block (the pipeline can expose its file-start-to-first-block time). Log both on the per-file
  `wrote file` line.
- **Item 4, the write-end sweep:** after the last content entry and before the seal (in the
  research build only), run `log_pages::sweep` limited to 0x34, 0x14, 0x0c and 0x17, and journal
  it with a trigger such as `volume write (write-end)`. This needs the store's device closed or
  idle at that point, and a subset sweep in `log_pages` (none exists).
- **`TAPECTL_EXP_CROSSFILE`:** one reader and one hasher for the whole Layout, file boundaries
  as messages, the hasher still withholding each file's last block until its verdict.
  `run_entries`'s loop then feeds one long-lived pipeline instead of one `write_verified` per
  entry, and Ctrl-C stays a between-files check on the writer side.
- **The announcement at session start:** the knobs are announced when the first write pool
  starts. Calling `research::announce()` at the start of `volume write` would put the banner
  before the build and positioning phases.

---

## 10. Measurement protocol for the real-drive phase

**Ground rules.**
- Test cartridge **EW7VWMVKF6** only, with its chip serial checked before each run, on the
  operator's go-ahead. A scratch home.
- Stop the drive-poll timer, as for `lto-profile.sh`.
- Keep vm-desk1 quiet except where a run loads it on purpose: other sessions hold heavy work, as
  on 2026-10-20.
- No eject between runs. The drive is standalone, page 0x34 clears at load, and every figure is
  a delta.
- Never read a log page during a write.

**Setup, once** (no drive):
- Stage one set of about 20 GiB on md3 test staging (`/srv/google_copy`, 67 GB free).
  #426's unit, 16.2 GiB in 17 slices, would do; 20 GiB synthetic is better.
- Write each run with `--keep-staging`, so the set is staged once.
- Make a 20 GB load file on the same array for the I/O load.
- Build the research binary as a static musl **release** build, approved for measuring only,
  as #426's was. A debug build's hash and copy loops are not production's, and this protocol
  measures rates.

**Each run,** about 8 minutes of drive time: a short erase, init, a 20 GiB write (2.5 min at 146
MB/s, 3.6 at 100), the seal, and the quick confirm (about 3.2 min, RECORD #426):

```bash
tapectl --home "$H" cartridge erase --device /dev/tape/by-id/scsi-HUJ808A5L4-nst --confirm-label "$PREV"
tapectl --home "$H" volume init "$LABEL" --device /dev/tape/by-id/scsi-HUJ808A5L4-nst
tapectl --home "$H" host check
tapectl --home "$H" volume write "$LABEL" --device /dev/tape/by-id/scsi-HUJ808A5L4-nst --keep-staging --yes
```

**Recorded per run:**
- the load (exact commands, start and stop times) and the lever (knob values);
- the pipeline and host trace (§9);
- the session log;
- the write-end sweep (0x34, 0x14, 0x0c, 0x17), the post-command sweep, and the feed-ratio
  event;
- MAM wear before and after;
- wall times.

**Derived per run:**
- native per data byte;
- native MB per data set;
- repositions per 100 MB;
- metres processed per GB;
- the stall histogram (writer waits over 100 ms, 1 s, 2.6 s);
- the hasher's CPU share and run delay;
- disk await.

**The loads.** home2 has no stress-ng or fio, so they use what exists:

| Load | Exact commands | Where |
|---|---|---|
| L0 quiet | none; `tapectl host check` quiet; vm-desk1 idle | |
| L1 vm-desk1, 8 threads | `for i in $(seq 8); do timeout 900 sha256sum /dev/zero & done; wait` | inside vm-desk1 |
| L2 vm-desk1, 16 threads | the same with `seq 16` | inside vm-desk1 |
| L3 #426's condition | the java service started by the operator, plus L1 with `seq 2` | home2 and vm-desk1 |
| L4 SMT sibling | with tapectl pinned (`taskset -c 6,7`), `taskset -c 18 timeout 900 sha256sum /dev/zero` and the same on 19 | home2, as the operator |
| L5 staging I/O | `dd if=/srv/google_copy/<scratch>/load.bin of=/dev/null bs=1M iflag=direct`, then a run with a writer instead: `dd if=/dev/zero of=/srv/google_copy/<scratch>/w.bin bs=1M count=20000 oflag=direct` | home2, md3 |

**The runs.** Each lever run uses L\*, the first load that reproduces about 1.5.

| Run | Load | Lever | Answers |
|---|---|---|---|
| R0 | none | none; no drive: the operator decodes 0x34 and 0x14 from the production `log_page_journal` (a read-only copy of a `db backup`) for L6-0001 and L6-0002, beside their `write_feed_ratio` events | what production writes cost already |
| P0 | L0, L2, L0 | three ~7 GiB writes in one load; the write-end sweep after each | 0x34's semantics (cumulative since load vs recent window); repositions against BOP→EOD excess, which separates stops from fill |
| R1 | L0 | today's pipeline (`sha2`, 256 MiB) | the baseline; hasher CPU ≈ wall time confirms §1 item 1 |
| R2 | L1 | baseline | ladder step 1 |
| R3 | L2 | baseline | ladder step 2 |
| R4 | L3 | baseline | ladder step 3: #426 as it happened |
| R5 | L5 | baseline | the staging path |
| R6 | L\* | queue 4 GiB, unpaced | §2.4's prediction: no gain without headroom |
| R7 | L\* | `sha2`, pace 110, queue 4 GiB, cross-file | pacing alone |
| R8 | L\* | `ring`, unpaced, queue 4 GiB, cross-file | headroom alone |
| R9 | L\* | `ring`, pace 140, queue 4 GiB, cross-file | both |
| R10 | L\* | `ring`, adaptive pace (150/90, 10% steps) plus refill (0.10/0.90) | the adaptive pace and the pause; per-pause cost in tape and seconds |
| R11 | L\* | pinning (vcpupin plus `taskset`, a host decision), unpaced, `sha2` | isolation alone |
| R12 | L0 and L\* | the winner, repeated | repeatability; no quiet-host regression |

- **Stop rule for R2-R5:** stop at the first load with a ratio of 1.3 or more; that is L\*. If
  the CPU loads do not reproduce it and L5 does, the staging path is the cause; skip R11 and
  weight §8. If none does, record that, run R1, R8 and R9 under L2 for the throughput
  difference, and stop.
- **Pass criterion** (the issue's acceptance): under L\*, native per data byte ≤ 1.05
  (`FEED_RATIO_WARN_THRESHOLD`) and repositions per 100 MB within the quiet baseline's range.
- **Drive time:** P0 plus 13 runs at about 8 minutes is about **2 hours**. About 320 GiB is
  written, about 0.13 of the cartridge's capacity, in 16 short erases. Rewinds and confirms add
  passes over the first wraps, which is what the test cartridge is for.

---

## 11. Recommendation

1. **Build §9 first,** including the 0x34 decoder in production: it gives every write a wear
   figure at no cost.
2. **Run R0 and §10.** Do not choose levers before P0 and the ladder. Today's data cannot say
   which stall source #426 hit.
3. **Expected outcome, to be confirmed by R7-R10:**
   - a faster hash (`ring`), which gives the pipeline headroom it does not have today;
   - a hashed queue of about 4 GiB carried across files;
   - a fixed pace at about 0.85 of the measured producer rate, self-calibrated per write, or
     from the host profile if the CTO prefers explicit keys;
   - pause and refill as the backstop when even the pace cannot be met.

   The pace costs about 5% of throughput with `ring` (140 vs 146.5 today). It spends memory and
   a little time to save tape, which is the trade the CTO asked for.
4. **On home2,** reserve two physical cores for tape commands (vcpupin plus `taskset` in
   `tapectl-op`), if R11 shows it helps.
   - Correct home2-prep.sh §9: its `cpu_shares` and `blkiotune` lines do nothing as the cgroups
     stand.
   - Stop describing `ionice` as a lever on this kernel and scheduler.
5. **Make the new disk dedicated staging,** 3 TB or more, and decide LUKS on it explicitly.

## 12. Decisions for the CTO

1. Approve the instrumentation (§9), including the HP 0x34 decoder and the repositions field in
   the `write_feed_ratio` event, and a research build with environment-variable knobs.
2. Approve the drive phase: EW7VWMVKF6, about 2 hours of drive time, about 16 writes of 20 GiB,
   a release build of the research branch for measuring only, and the loads of §10 on home2
   and vm-desk1. Some of them, the java service (L3) and pinning
   (R11), need the host owner too.
3. Approve R0: the operator decodes page 0x34 and 0x14 rows from a copy of the production
   catalog.
4. The hash: replace `sha2` with `ring` (or `aws-lc-rs`) on the write, verify and staging paths.
   It is a new C/asm dependency. It builds for musl (MEASURED: 216.7 MB/s static on vm-desk1).
5. The shaping: a fixed pace, an adaptive pace, a pause and refill, or a combination; on by
   default or opt-in.
6. Config keys: the `[write_shaping]` keys of §7, or none, with the pace self-calibrated per
   write and the queue and marks as constants.
7. `QUEUE_BYTES`: raise it from 256 MiB to a few GiB, and carry the queue across files. Lock it
   in memory, or accept the swap risk.
8. home2's CPU reservation during tape commands, with homorg's founder: which cores, and whether
   `tapectl-op` applies and restores the pins.
9. Correct home2-prep.sh §9 and the operator guide's priority paragraph (ionice, nice and the
   libvirt weights as found).
10. The new disk: dedicated, its size, RAID0 or single, and LUKS or none.

## 13. What I could not establish, and sources

**Could not establish:**
- HP's speed-matching steps for LTO-6 and its switching rule. QS gives only the 54-160 MB/s
  range. The HPE community thread that may quote more answered 403, and the white paper *How to
  Optimize the Performance of HP Ultrium Tape Drives* (cited by HP-V2 p20) was not found.
- Page 0x34's averaging window, and whether page-specific LOG SELECT works on firmware 35GD
  (NEEDS DRIVE, P0).
- What one stop costs in tape and seconds on this drive (NEEDS DRIVE, P0 and R10).
- Whether the drive writes fill patterns instead of stopping (NEEDS DRIVE, P0).
- tapectl's `sha2` rate on home2 itself: one benchmark was allowed there, and it was OpenSSL. The
  142-157 MB/s range is scaled from vm-desk1.
- `aws-lc-rs` and the soft `sha2` were not built for musl; `ring` and `sha2` asm were (§5.9).
- Whether vm-desk1's vCPU threads landed in `system.slice/libvirtd.service` from a
  `daemon-reload` or by design. That is where they are; how is not established.
- The CPU frequency during the 1.505 run (`perf` is closed to non-root, no cpufreq driver).
- The mq-deadline I/O-priority history (5.14), from memory of kernel history; not verified
  against the kernel tree in this session.

**Sources:**
- HP-V2: https://docs.oracle.com/cd/E38452_01/en/LTO6_Vol2_E1_D8/LTO6_Vol2_E1_D8.pdf (pp 7, 19,
  20-24, 40)
- HP-V3: https://docs.oracle.com/cd/E38452_01/en/LTO6_Vol3_E1_D20/LTO6_Vol3_E1_D20.pdf (pp 55,
  59-65, 77-84, 89-94, 106, 255, 261, 266)
- QS: HP LTO Ultrium Tape Drives QuickSpecs DA-13572 v14, 2013-09-13, a reseller copy at
  https://raidshop.ru/files/2778/2205/HPLTOUltriumTapeDrives.PDF (pp 9, 27, 30)
- Records: `docs/runs/2026-09-23-lto6-capacity-measurement.md`,
  `docs/runs/2026-10-12-1.1.0-real-drive-proof.md`, `docs/runs/2026-10-20-lto6-profile-and-lbp.md`,
  #326 and #426 with comments
- Raw captures: `/scratch/tapectl-lto6/unit182-20260923-021914/` on vm-desk1;
  `tests/fixtures/sg_logs/hp_lto6_sg0_{nomedia,fuji_ew7vwmvkf6}/`
- home2 today: `lscpu`, `/proc`, sysfs, `virsh -r -c qemu:///system`, `~/.cache/tapectl/i426/results`
  and `measure/results`, a lifecycle session log, all read-only
- The benchmark crate and scripts: in the session scratchpad (not committed); the target dir
  `/scratch/shabench-target-488`
