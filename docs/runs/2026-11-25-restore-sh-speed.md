# 2026-11-24 to 2026-11-25: RESTORE.sh's speed on home2's real drive

T6.6 (`docs/runs/2026-11-24-2.0.0-real-drive.md`) measured the heir's script at about 24 MB/s.
The CTO then ordered the fix into 2.0.0: ADR-0012 item 308 (the hashes fold into the passes
that already read the bytes), item 309 (a read-ahead of the next slice on the same cartridge)
and item 312 (the final pin). This record is the timing that record promised. It was run from
vm-desk1 over ssh, in a tmux session on home2.

- **Scripts:** three copies of RESTORE.sh, by sha256:
  - old: `1aa023166b1078567b3587d07e0eef71be07e6f4c557fb81cdb7a696f2a5288e`, the copy T6.6 ran;
  - hash-only: `6e15124939e8043bf0adcdb8e964e4ea465e55b4ee5c0cd48c176e80832b0f7f`, item 308;
  - final: `ddce81da85350835dd8b446f80833bd12a41d804496cda08baa6e9ddec2ea826`, item 312, the copy
    master `07144179` writes as File 2.
- **Host:** home2, two Xeon L5640 (24 threads, no SHA extensions), kernel 4.19. `age` v1.3.2 and
  GNU tar 1.35. The drive was the HP Ultrium 6-SCSI (`/dev/tape/by-id/scsi-HUJ808A5L4-nst`).
- **Cartridge:** EW7VWMVKF6, holding SPN-0002 from T6.6's Run 4. Its chip serial was read before
  each run. `spn-span` is one unit of 594 slices: 505,241,497,600 bytes of stream, 3,651 files,
  505,248,679,136 bytes of file data.
- **Key:** the run's primary tenant key, from Run 4's throwaway home.
- **Input:** each run read standard input from `/dev/null`, with `TMPDIR` on `/srv/acache`.
- **Logs:** `/srv/acache/t66/bench/` and `/srv/acache/t66/whole/` on home2.

| Run | What | Window (UTC) | Time | Result |
|---|---|---|---|---|
| Bench, old | 747 files of one folder of `spn-span`, 31,241,841,452 bytes, each named with `--path` | 2026-11-24 17:15 to 17:39 | 1,398 s, 22.3 MB/s | 747 files, 0 differ |
| Bench, hash-only | the same | 2026-11-24 17:41 to 18:00 | 1,109 s, 28.2 MB/s | 747 files, 0 differ |
| Bench, final | the same | 2026-11-25 04:56 to 05:10 | 853 s, 36.6 MB/s | 747 files, 0 differ |
| Whole unit, final | `--restore --unit spn-span` | 2026-11-25 16:23 to 19:23 | 10,829 s | `RESTORE COMPLETE`, exit 0; 3,651 files, 0 differ |

Each "differ" column is a sha256 of every restored file against Run 4's manifest of the source,
taken before staging. The whole unit's compare read all 505,248,679,136 bytes back and ended at
20:08Z.

## The whole unit, against Run 4

Run 4's step 5 restored the same unit from the same cartridge with the old script in 17,774 s.
The final script took 10,829 s, 39% less. Its time fell into two parts:

| Part | Time | What |
|---|---|---|
| The tape read | 6,894 s (16:23:08 to 18:18:02) | every slice read, checked, decrypted and given to tar: about 73 MB/s of stream |
| The final check | 3,935 s (18:18:03 to 19:23:37) | `sha256sum -c` of every restored file against the file table, from disk: about 128 MB/s |

The drive was idle from 18:18Z. In the final check, one `sha256sum` process reads the whole
restored tree. 128 MB/s is one core's `sha256sum` rate on this host
(`docs/runs/2026-09-23-lto6-capacity-measurement.md` measured about 127 MB/s). On this host, the check is now 36% of a whole-unit restore.

The bench's rate (36.6 MB/s) is half the whole unit's (73 MB/s). It names 747 paths of one
folder, so each slice it reads restores only part of its files, and every path is checked.

## What else ran

homorg's CI ran on vm-desk1, which shares home2's CPUs, from about 18:25Z. That was after the
tape read, during the final check. The CTO ruled the overlap acceptable. The final check's
3,935 s may include some of that load.

## Not covered here

- heir2-C2: what `mt-st` answers to `setblk` and `rewind` while a cartridge is still threading
  after a load. It needs the cartridge reloaded by hand.
- Returning EW7VWMVKF6 to the test pool, and restarting `tapectl-drive-poll.timer`.
- Stage 7: the tag, the fresh start on home2, and the drill (#548).
