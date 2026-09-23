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
