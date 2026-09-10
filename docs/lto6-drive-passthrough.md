# LTO-6 Drive Passthrough (home2 → vm-desk1)

How the real HP LTO-6 drive reaches the development VM, why it is wired
this way, and what to watch out for. Applied 2026-09-10.

Read this before `docs/lto6-validation-checklist.md` — the checklist assumes
the drive is already visible in the guest, and this is how it gets there.

## The hardware

| | |
|---|---|
| Drive | HP Ultrium 6-SCSI (LTO-6), firmware `35GD`, serial `HUJ808A5L4` |
| Host | `home2` (Debian 10, kernel 4.19, libvirt 5.0.0, QEMU 3.1.0) |
| SCSI address | `4:0:4:0` → `/dev/st0`, `/dev/nst0`, `/dev/sg16` **on home2** |
| HBA | LSI SAS2008 (`mpt3sas`) at PCI `07:00.0` |
| Guest | `vm-desk1` (Ubuntu, kernel 6.8), `pc-i440fx-3.1`, OVMF/UEFI |

## Why not PCI passthrough of the HBA

The SAS2008 at `07:00.0` is not dedicated to the tape drive. It also carries:

```
[4:0:0:0] SEAGATE ST3146755SS      /dev/sdj
[4:0:1:0] SEAGATE ST3146755SS      /dev/sdk
[4:0:2:0] HGST HUS726040ALS211     /dev/sdl ─┐
[4:0:3:0] HGST HUS726040ALS211     /dev/sdm ─┴─ md4 (RAID1)
[4:0:4:0] HP Ultrium 6-SCSI        /dev/st0     └─ /srv/backups
                                                   /srv/youtube_videos
                                                   /srv/mdisc_cache
```

Binding `07:00.0` to `vfio-pci` would pull a mounted RAID1 out from under a
running home2. **Rejected.** Pass the single LUN instead.

## The configuration

Two device fragments, kept on home2 at `~/tapectl-passthrough/`:

```xml
<!-- ctrl.xml — vm-desk1 had no SCSI controller at all (both disks are virtio-blk) -->
<controller type="scsi" index="0" model="virtio-scsi"/>
```

```xml
<!-- lto6.xml -->
<hostdev mode="subsystem" type="scsi" rawio="yes">
  <source>
    <adapter name="scsi_host4"/>
    <address bus="0" target="4" unit="0"/>
  </source>
</hostdev>
```

`scsi_host4` + `bus=0 target=4 unit=0` is the host:channel:id:lun of `4:0:4:0`.

Applied with (no sudo needed — membership in the `libvirt` group is enough;
`virsh` on home2 must be told `-c qemu:///system`):

```bash
virsh -c qemu:///system attach-device vm-desk1 ~/tapectl-passthrough/ctrl.xml --live --config
virsh -c qemu:///system attach-device vm-desk1 ~/tapectl-passthrough/lto6.xml --live --config
```

Controller first. `--live --config` hot-plugs *and* persists, so no guest reboot
is required — i440fx ACPI hotplug works under OVMF. libvirt assigned the
controller PCI slot `0x09` and the hostdev drive address `controller=0 bus=0
target=0 unit=0`.

To reverse, `detach-device` with the same two files (hostdev first).

### `rawio='yes'`, not `sgio='unfiltered'`

`sgio='unfiltered'` is in the libvirt 5.0.0 schema but **cannot be used here**:
it requires the `unpriv_sgio` sysfs knob, which is a RHEL-only kernel patch and
is absent on home2's Debian 4.19 kernel. libvirt refuses the attach.

`rawio='yes'` grants the domain's qemu process `CAP_SYS_RAWIO`, which bypasses
the SG_IO command filter. Without it the tape-specific CDBs tapectl depends on
— LOG SENSE (`sg_logs` drive health), READ ATTRIBUTE (MAM), REPORT DENSITY
SUPPORT — return EPERM. This is a real privilege grant to that qemu process;
it is the price of the passthrough.

## Verified working

In the guest after attach:

```
[4:0:0:0]  tape  HP  Ultrium 6-SCSI  35GD  /dev/st3  /dev/sg5
/dev/tape/by-id/scsi-HUJ808A5L4-nst -> ../../nst3
```

```
virtio_scsi virtio4: 1/0/0 default/read/poll queues
scsi host4: Virtio SCSI HBA
scsi 4:0:0:0: Sequential-Access HP Ultrium 6-SCSI 35GD PQ: 0 ANSI: 6
st 4:0:0:0: Attached scsi tape st3
st 4:0:0:0: st3: try direct i/o: yes (alignment 4 B)
```

- `sg_inq` → correct vendor/product/serial (INQUIRY passes through untouched,
  which is why the guest's `by-id` name matches the host's).
- `sg_logs --list` → full page list **including the LTO-6-specific pages
  `0x30` (Tape usage) and `0x31` (Tape capacity)**. This is the `rawio` payoff.
- `sg_read_attr` → full MAM read (see below). Before a cartridge was loaded this
  returned `Device not ready` — that was the *drive* reporting `DR_OPEN`, not a
  permission failure.

## Measured on real media, 2026-09-10

A cartridge was loaded mid-session, so the plumbing above was confirmed against
real media. **No data was written** — the cartridge remains pristine
(load count 1, 0 MiB written in medium life).

Media: FUJIFILM LTO-6, medium serial `EW7VWMVKF6`, 846 m, density `0x5a`.

| Probe | Result |
|---|---|
| `mt status` | `BOT ONLINE IM_REP_EN`, density `0x5a (LTO-6)` |
| MAM capacity | 2,499,053 MiB max / remaining (~2.44 TiB native) |
| MAM load count | 1 |
| MAM life written/read | 0 MiB / 0 MiB |
| READ BLOCK LIMITS | min 1 byte, **max 16,777,215 bytes (16 MB)** |
| `setblk` accepted | 512 K, 1 M, 2 M, 4 M — all accepted |

Two things worth carrying into the validation session:

- The **real drive advertises a 16 MB maximum block size**, not the 2 MiB the
  mhvtl virtual drive advertised in the 2026-08-02 dry-run. Any §5 reasoning
  based on the mhvtl figure is about mhvtl, not this hardware.
- **`setblk` acceptance is not write acceptance.** `mt setblk` only sets the
  `st` driver's block size; the dry-run's 1 MiB `EBUSY` was a buffer-allocation
  failure raised on the *write*. So the table above shows 4 M "accepted" while
  the largest size that actually writes through this path is still unknown. The
  question stays open until a real write is attempted at each size.

## ⚠ Device-name collision with mhvtl

mhvtl is loaded in vm-desk1 and owns the low tape nodes:

```
[3:0:3:0] IBM ULT3580-TD6  /dev/st0   ← mhvtl (virtual)
[3:0:1:0] IBM ULT3580-TD8  /dev/st1   ← mhvtl (virtual)
[3:0:4:0] IBM ULT3580-TD6  /dev/st2   ← mhvtl (virtual)
[4:0:0:0] HP  Ultrium 6    /dev/st3   ← REAL DRIVE
```

The real drive landing at `st3` is **incidental to mhvtl loading first**. If
mhvtl ever fails to load, the real LTO-6 becomes `/dev/nst0` — which is the
hardcoded default in `tests/mhvtl_e2e.rs`, `scripts/mhvtl-verify-gate.sh`
and `scripts/mhvtl-device.sh`. A routine `TAPECTL_MHVTL=1 cargo test` would
then write a synthetic test volume onto a real cartridge.

### THIS HAPPENED (2026-09-10, after the VM reboot)

The enumeration flipped — not because mhvtl failed to load, but simply because
a reboot re-raced the two SCSI hosts. mhvtl came up fine; the real drive just
won `nst0`:

```
/dev/tape/by-id/scsi-HUJ808A5L4-nst -> ../../nst0   ← REAL DRIVE, now nst0
/dev/tape/by-id/scsi-XYZZY_A1-nst   -> ../../nst1   ← mhvtl
```

So "if mhvtl fails to load" was too narrow a trigger. **Assume every reboot
reshuffles these names.** Only the `by-id` serial names are stable.

What it cost: three RED `mhvtl-verify-gate.sh` runs. The write path was never
at risk (see below), but the gate's heir leg invoked `RESTORE.sh` without
`TAPE_DEVICE`, and RESTORE.sh's own default is `/dev/nst0` — so it read the
real drive while the gate wrote mhvtl. Reads only (`setblk`, `rewind`, `fsf`,
`dd if=`); the real cartridge was repositioned, never written. Fixed in
`0dbfda7`; the confusing symptom it produced is fixed in `3b5d287`.

**Always address the real drive by its stable serial name:**

```bash
export TAPECTL_GATE_TAPE=/dev/tape/by-id/scsi-HUJ808A5L4-nst   # real LTO-6
```

Both the test suite and the gate scripts honour `TAPECTL_GATE_TAPE`.

### Write-path guard: already closed (verified)

The follow-up suggested here — INQUIRY-check the vendor before writing — turns
out to be unnecessary for the write path, because `scripts/mhvtl-device.sh` is
the single entry point for every writing harness (issue #111) and it resolves
the drive through `/etc/mhvtl/device.conf`. A device that is not an mhvtl drive
matches no Drive stanza, so it fails closed. Verified directly:

```console
$ bash scripts/mhvtl-device.sh --tape /dev/nst0
mhvtl-device.sh: no device.conf Drive matches /dev/nst0 at 0:0:0:0
$ echo $?
2
```

The gate treats that exit as fatal. So `TAPECTL_MHVTL=1` with an unset
`TAPECTL_GATE_TAPE` now *aborts* rather than eating the real cartridge.

The gap was never the write path — it was the **read** path, where RESTORE.sh
is deliberately standalone and cannot consult device.conf. That is addressed at
the level an heir actually experiences it: RESTORE.sh now announces its device,
reads the loaded tape's own label from the ID thunk, and warns loudly when it
does not match the volume the script was written for (`3b5d287`).

## Open / unmeasured

- **Max writable block size through the passthrough is still unknown.**
  `setblk` accepts up to 4 M and the drive advertises 16 MB (above), but no
  write has been attempted — and the write is where the dry-run's `EBUSY`
  appeared. `max_sectors` is not exposed for `host4` on home2's 4.19 kernel;
  `sg_tablesize=128` with `scatter_elem_sz=32768` *implies* ~4 MB of headroom,
  which is inference, not measurement.
  This is exactly the open question in `docs/design/v2-open-questions.md` §5
  (512 K vs 1 M block size), and it must be measured **through the passthrough
  path**, not from host-side numbers. `scripts/lto6-measure.sh` is the
  instrument.

  Note this now has **two** candidate ceilings stacked on each other. The
  2026-08-02 mhvtl dry-run (recorded in `lto6-validation-checklist.md`) already
  found 1 MiB blocks refused with `EBUSY` by the guest's own `st` driver, even
  though the drive advertised a 2 MiB maximum — a host-side buffer limit
  invisible to tapectl. The passthrough adds a second possible limit at
  virtio-scsi/`scsi-generic`. If a large block fails now, the measurement has
  to distinguish which layer refused it: compare the guest `st` limit against
  the same write attempted on home2 directly against `/dev/nst0`.
- **No host-side exclusion.** Nothing stops a process on home2 opening
  `/dev/st0` while the guest holds the drive. There is no lock; coordinate by
  hand.
- **`/dev/sgN` numbering is unstable** across reboots on both sides. The
  libvirt fragment addresses the drive by SCSI address (`scsi_host4` +
  `4:0:4:0`), which is stable only as long as HBA enumeration order holds on
  home2. Re-check after a host reboot.
- **Media state.** A FUJIFILM LTO-6 (`EW7VWMVKF6`) is loaded as of 2026-09-10
  and is pristine — load count 1, 0 MiB written. The block-size question above
  is one `scripts/lto6-measure.sh --erase-cartridge <BARCODE>` run away, but
  that harness erases the cartridge, so it ends the pristine state.
