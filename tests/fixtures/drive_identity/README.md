# Drive-identity fixtures

Captured 2026-09-22 from this VM's mhvtl drive at `/dev/nst1` (`/dev/sg1`,
by-id name `scsi-XYZZY_A1-nst`), for issue #295. All of these describe the
**same drive at the same moment**, which is the point: the acceptance wants a
test that the independent identity sources AGREE, and that is only meaningful
when they come from one drive.

| file | source command |
|---|---|
| `mhvtl_nst1_sysfs.txt` | `cat /sys/class/scsi_tape/nst1/device/{vendor,model,rev}` — three lines, in that order, with the SCSI space padding **preserved** |
| `mhvtl_nst1_vpd_pg80.hex` | `xxd -p /sys/class/scsi_tape/nst1/device/vpd_pg80` — the raw VPD page 0x80 as hex, one line |
| `mhvtl_nst1_sg_inq_page80.txt` | `sg_inq --page=0x80 /dev/sg1` — the fallback path's output |
| `../sg_logs/mhvtl_ibm_td8_page_0x02.txt` | `sg_logs --page=0x02 /dev/sg1` — line 2 is the identity header pre-#298 health collection concatenated into `health_logs.raw_log` (since #298 it is rendered from an INQUIRY; see `src/tape/log_pages.rs`) |

What they all say: vendor `IBM`, model `ULT3580-TD8`, firmware `2160`,
serial `XYZZY_A1`.

**The serial is also the by-id name.** `scsi-XYZZY_A1-nst` carries exactly the
`vpd_pg80` serial, which is why `CLAUDE.md` can tell an operator to resolve the
real LTO-6 as `scsi-HUJ808A5L4-nst` — `HUJ808A5L4` is that drive's serial.
Three independent routes to one identity, and their disagreement would itself
be a finding.

**Why the existing `hp_lto6_page_0x02.txt` could not serve.** It is a real
HP LTO-6 (`HP`/`Ultrium 6-SCSI`/`35GD`) recorded in a different session; this
VM's sysfs describes an IBM-emulating mhvtl node. Testing "the sysfs identity
agrees with the raw_log header" across those two asserts that two different
drives are the same drive, which is false. Keep using the HP capture for
parsing a real drive's header; use these for agreement.

**Unverified, and deliberately so:** whether the real HP LTO-6 publishes a
stable `vpd_pg80`. It is physically on home2 and not attached to this VM, so
only mhvtl nodes were readable. `sg_inq --page=0x80` is the fallback and the
by-id name is the last resort. Confirm at the real-drive rehearsal before
calling #295 done.
