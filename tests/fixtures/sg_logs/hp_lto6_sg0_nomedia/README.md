# sg_logs page fixtures: every page the REAL HP LTO-6 supports (no medium loaded)

Captured 2026-09-23 from the real drive passed through to this VM:
`/dev/sg0` (`scsi-HUJ808A5L4`, HP Ultrium 6-SCSI, fw 35GD, serial HUJ808A5L4),
sg3-utils `sg_logs` 1.81 20200110. **No cartridge was loaded** (TEST UNIT
READY: "Medium not present"). The drive answers every log page without one;
per-medium pages (0x17 volume statistics, 0x30/0x31 tape usage/capacity)
therefore describe no cartridge. Same layout as `../mhvtl_td8_sg1/`.

| file | source command |
|---|---|
| `page_0x00.bin` | `sg_logs --page=0 --raw /dev/sg0`: the drive's list of supported pages |
| `page_0xNN.bin` | `sg_logs --page=0xNN --raw /dev/sg0`: one LOG SENSE per listed page, read once |
| `page_0xNN.decoded.txt` | `sg_logs --in=page_0xNN.bin --raw --pdt=1`: decoded OFFLINE from the stored bytes |
| `page_0x00.live_header.txt` | line 1 of the live `sg_logs --page=0 /dev/sg0`: the INQUIRY identity header |
| `page_0x2e.second_read.bin` | a deliberate SECOND read of 0x2e, for the read-to-clear question below |

**Page 0x00 lists 22 pages** (mhvtl lists 13): 00 02 03 0c 0d 11 12 13 14 15
16 17 18 1b 2e 30 31 32 33 34 35 3e. Every one read ok. 0x2e IS listed, so
this drive never takes the "0x2e not supported" path (#317). Verified at
capture: the offline decode of page 0x00 matches the live output line for
line; the live output adds only the header line.

**Read-to-clear (ADR-0013's hazard) is NOT answered by this capture.** Every
TapeAlert flag was 0 on the first read, and the second read is byte-identical
— but zero-then-zero cannot distinguish "reading clears" from "nothing to
clear". Answering it needs a set flag (e.g. a cartridge that raises one) at
the real-drive rehearsal.
