# sg_logs page fixtures: every page one mhvtl drive supports

Captured 2026-09-22 from this VM's mhvtl drive `/dev/sg1` (`scsi-XYZZY_A1`,
IBM ULT3580-TD8, fw 2160, sg3-utils `sg_logs` 1.81 20200110), for issue #298.
All pages come from one drive at one moment.

| file | source command |
|---|---|
| `page_0x00.live.txt` | `sg_logs --page=0 /dev/sg1`: the drive's own list of supported pages, as the live decode prints it (with the INQUIRY identity header line) |
| `page_0xNN.bin` | `sg_logs --page=0xNN --raw /dev/sg1`: the page's response bytes exactly, one LOG SENSE per page |
| `page_0xNN.decoded.txt` | `sg_logs --in=page_0xNN.bin --raw --pdt=1`: the same bytes decoded OFFLINE, with no second read of the drive |

The pages are exactly those page 0x00 lists: 00 02 03 0c 0d 10 11 17 2e 30 31 32 37.

**Why raw bytes plus an offline decode.** ADR-0013's hazard: TapeAlert (0x2e)
may be cleared by reading it, so each page is read at most once per contact.
Reading `--raw` once and decoding from the stored bytes gives the verbatim
record AND the parser's input from one LOG SENSE. Verified at capture: the
offline decode of `page_0x17.bin` matches the live `sg_logs --page=0x17`
output line for line; the live output only adds the INQUIRY header.
`--pdt=1` (sequential-access) is REQUIRED. Without it `sg_logs --in` guesses
a disk and decodes 0x17 as "Non-volatile cache page".

**mhvtl is not an LTO-6.** A real HP LTO-6's page 0x00 has never been
captured; the only real-drive pages are `../hp_lto6_page_0x02.txt` and
`../hp_lto6_page_0x03.txt`, both text-only. Capturing the real drive's page
0x00 and full page set is a job for the real-drive rehearsal on home2.
