# sg_logs page fixtures: the real HP LTO-6 WITH a cartridge loaded

Captured 2026-09-23 from `/dev/sg0` (`scsi-HUJ808A5L4`, HP Ultrium 6-SCSI, fw
35GD), sg3-utils `sg_logs` 1.81 20200110, with the CTO's test cartridge loaded:
FUJIFILM LTO-6, MAM medium serial `EW7VWMVKF6`, manufactured 2017-08-24, load
count 2, 31131 MiB written in medium life (sg_read_attr's label). Same layout
as `../hp_lto6_sg0_nomedia/`: `page_0xNN.bin` is one `sg_logs --page=0xNN --raw`
read, and `page_0xNN.decoded.txt` is its OFFLINE decode (`--in= --raw --pdt=1`).

Page 0x00 is byte-identical to the no-medium capture (22 pages); every page
read ok. With a cartridge loaded, the per-medium pages have content: 0x17
volume statistics (thread count 2, 13205 data sets written), 0x30/0x31 tape
usage/capacity. Every TapeAlert flag (0x2e) is 0, so read-to-clear is still
unanswered.

**2026-09-23 — captured with the two-fetch argv (issue #328).** These pages
were read with `sg_logs --page=0xNN --raw <sg>`, no `--maxlen`. Per `man
sg_logs` (`-m, --maxlen=LEN`) that is TWO LOG SENSE commands per page: a
4-byte probe for the page length, then the full page. tapectl now reads with
`sg_logs --page=0xNN --maxlen=65532 --raw <sg>` — one LOG SENSE per page
(`tape::log_pages::READ_MAXLEN`). sg_logs writes `page length + 4` bytes to
stdout either way, so these bytes are the shape the new argv produces, and
every `.bin` here is complete by its own header. They were NOT re-captured.
