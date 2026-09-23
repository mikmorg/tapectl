#!/usr/bin/env python3
"""ADR-0013 forensic checks on a lifecycle run's catalog taken on a REAL drive.

    scripts/realdrive-forensics.py <run>/<scenario>/home/tapectl.db <DRIVE_SERIAL>

Asserts what the pass count cannot: every backend contact names that drive,
every closed write/verify/restore contact swept exactly the pages its own
page 0x00 listed (each ok, each ONE LOG SENSE via --maxlen), every health
reading has counters, every volume was sized from the detected LTO-6
generation, and the cartridge is the sanctioned one. Read-only; touches no
device. Written for the 2026-09-23 rehearsal (docs/runs/2026-09-23-real-drive-rehearsal.md).
"""
import sqlite3, sys
db, want_serial = sys.argv[1], sys.argv[2]
c = sqlite3.connect(db)
out, bad = [], []
contacts = c.execute("""SELECT c.id, c.operation, c.outcome, d.serial, c.closed_at, c.backend_name,
                               c.cartridge_id, c.identity_reason
                        FROM cartridge_contacts c LEFT JOIN drives d ON d.id = c.drive_id ORDER BY c.id""").fetchall()
assert contacts, "positive control: no contacts at all"
ops = sorted({r[1] for r in contacts})
out.append(f"{len(contacts)} contacts, operations {ops}")
# 1. every contact through the configured backend names the real drive
for cid, op, outcome, serial, closed, backend, cart, reason in contacts:
    if backend and serial != want_serial:
        bad.append(f"contact {cid} ({op}, {outcome}) names drive {serial!r}, not {want_serial}")
    if closed is None:
        bad.append(f"contact {cid} ({op}) never closed")
# 2. sweep completeness per closed backend contact
swept = 0
for cid, op, outcome, serial, closed, backend, cart, reason in contacts:
    if not backend or closed is None:
        continue
    if op == "volume init":
        # ADR-0013's 2026-09-23 amendment lists write/resume/verify and the read
        # paths; init is outside the ruling. Reported, not failed.
        n = c.execute("SELECT COUNT(*) FROM log_page_journal WHERE contact_id=?", (cid,)).fetchone()[0]
        out.append(f"note: volume init contact {cid} has {n} log-page rows (init is outside ADR-0013's sweep ruling)")
        continue
    rows = c.execute("SELECT page_code, ok, raw, tool_argv FROM log_page_journal WHERE contact_id=?", (cid,)).fetchall()
    if not rows:
        bad.append(f"contact {cid} ({op}): no log-page sweep at all"); continue
    z = [r for r in rows if r[0] == 0]
    if not z or z[0][1] != 1:
        bad.append(f"contact {cid} ({op}): page 0x00 missing or not ok"); continue
    raw = bytes(z[0][2]); listed = set(raw[4:4 + int.from_bytes(raw[2:4], "big")]) - {0}
    read = {r[0] for r in rows} - {0}
    if listed != read:
        bad.append(f"contact {cid} ({op}): listed {sorted(listed)} read {sorted(read)}")
    if any("--maxlen=" not in a for *_, a in rows):
        bad.append(f"contact {cid} ({op}): a read without --maxlen")
    notok = [hex(r[0]) for r in rows if r[1] != 1]
    if notok:
        bad.append(f"contact {cid} ({op}): pages not ok: {notok}")
    swept += 1
out.append(f"{swept} backend contacts each swept exactly the pages their own 0x00 listed (all ok, all --maxlen)")
# 3. health readings: counters present (NULL means not read)
h = c.execute("""SELECT operation, contact_id, total_corrected, total_uncorrected, tape_alerts FROM health_logs""").fetchall()
nulls = [r for r in h if r[3] is None or r[4] is None]
if not h: bad.append("no health_logs rows")
if nulls: bad.append(f"health rows with NULL counters: {nulls[:5]}")
alerts = [r for r in h if r[4]]
out.append(f"{len(h)} health readings ({sorted({r[0] for r in h})}); uncorrected total {sum(r[3] or 0 for r in h)}; TapeAlert flags raised in {len(alerts)}")
# 4. cartridge and capacity
for label, cap, mamcap, status, sealed in c.execute(
        "SELECT label, capacity_bytes, mam_capacity_bytes, status, sealed_at FROM volumes ORDER BY id"):
    out.append(f"volume {label}: status={status} sealed={'yes' if sealed else 'no'} capacity_bytes={cap} mam_capacity_bytes={mamcap}")
    if cap != 2_500_000_000_000:
        bad.append(f"volume {label}: capacity_bytes {cap} is not the LTO-6 generation-table 2.5e12 (ADR-0010: detected generation)")
for bc, serial, mt, st in c.execute("SELECT barcode, serial_number, media_type, status FROM cartridges"):
    out.append(f"cartridge {bc}: serial={serial} media_type={mt} status={st}")
    if serial != "EW7VWMVKF6" or mt != "LTO-6":
        bad.append(f"cartridge {bc}: serial {serial} / media {mt} is not EW7VWMVKF6 / LTO-6")
print("\n".join(out))
if bad:
    print("FORENSICS RED:\n  " + "\n  ".join(bad)); sys.exit(1)
print("FORENSICS GREEN")
