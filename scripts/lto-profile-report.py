#!/usr/bin/env python3
"""lto-profile-report.py -- the report builder and the block emitter of
scripts/lto-profile.sh (issue #479; ADR-0012 2026-10-17 items 96-98 and 100).

scripts/lto-profile.sh drives the tape and leaves RAW CAPTURES in a run
directory (sg_inq, sg_read_attr, sg_logs, sg_modes, mt output, the fill's
result.json ...). This helper turns them into the two files an operator and a
later fitting exercise read:

  build RUN_DIR   -> RUN_DIR/profile.json (the machine-readable report) and
                     RUN_DIR/PROFILE.txt (the same figures as plain lines).
                     Every figure carries its source: the log page, the MAM
                     attribute, the errno or the timing it came from.
  emit  RUN_DIR... -> a [[backends.lto]] block on stdout, from one or more
                     profile.json files of one drive model and one medium
                     generation. Every derived value names the runs it came
                     from. The rules are in the comments of the block.
  identify RUN_DIR -> identity.json and KEY=VALUE lines for the driver (the
                     drive's and the medium's generation, nominal capacity).
  meta RUN_DIR --set k=v ... -> merge keys into RUN_DIR/meta.json.
  media-table     -> the generation table as JSON (tests pin it to
                     src/media.rs, which stays the one authority).
  selftest        -> the parsers and the derivation rules against fixtures.

The run directory format (format "tapectl-lto-profile", version 1) is
documented in docs/lto-profiling.md.

Python 3.7 and stdlib only (home2 is Debian 10).
"""
import argparse
import json
import math
import os
import re
import socket
import sys
import textwrap
import time

sys.dont_write_bytecode = True   # loading lto6-fill-writer.py must not leave a __pycache__ in scripts/

FORMAT = "tapectl-lto-profile"
FORMAT_VERSION = 1

# --------------------------------------------------------- the media tables --
# Mirrors src/media.rs (Generation::density_code, native_capacity_bytes and
# can_write). src/media.rs stays the authority: tests/lto_profile.rs compares
# `media-table` with it, so a change there that is not made here fails a test.
MEDIA = [
    # (generation, st/MAM density code, marketed native capacity in bytes)
    ("LTO-5", 0x58, 1500000000000),
    ("LTO-6", 0x5A, 2500000000000),
    ("LTO-7", 0x5C, 6000000000000),
    ("LTO-7-M8", 0x5D, 9000000000000),
    ("LTO-8", 0x5E, 12000000000000),
    ("LTO-9", 0x60, 18000000000000),
]
DRIVE_GENERATIONS = ["LTO-5", "LTO-6", "LTO-7", "LTO-8", "LTO-9"]
# drive generation -> media generations it writes. LTO-5 also writes LTO-4,
# which the toolkit does not profile (the tables here start at LTO-5).
CAN_WRITE = {
    "LTO-5": ["LTO-5"],
    "LTO-6": ["LTO-5", "LTO-6"],
    "LTO-7": ["LTO-6", "LTO-7"],
    "LTO-8": ["LTO-7", "LTO-7-M8", "LTO-8"],
    "LTO-9": ["LTO-8", "LTO-9"],
}
DEFAULT_FILL_CEILING = 0.97   # src/config.rs DEFAULT_FILL_CEILING (ADR-0012 2026-10-06 item 16)
DEFAULT_MARGIN = 0.02         # the --emit margin below the measured early-warning fraction


def media_by_generation(gen):
    for g, code, cap in MEDIA:
        if g == gen:
            return {"generation": g, "density_code": code, "nominal_bytes": cap}
    return None


def media_by_density(code):
    for g, c, cap in MEDIA:
        if c == code:
            return {"generation": g, "density_code": c, "nominal_bytes": cap}
    return None


def drive_generation_from_model(model):
    """The DRIVE's own generation from its INQUIRY product identification
    (the python twin of scripts/lib/drive-generation.sh): IBM ULT3580-TDn /
    -HHn and ULTRIUM-TDn / -HHn, HP/HPE "Ultrium n-SCSI". None when unknown;
    never a guess."""
    model = model or ""
    if re.search(r"M8|-M[0-9]", model):
        return None
    m = re.search(r"[TH][DH]([0-9]+)", model)
    if m:
        return "LTO-%s" % m.group(1)
    m = re.search(r"[Uu][Ll][Tt][Rr][Ii][Uu][Mm] ([0-9]+)-SCSI", model)
    if m:
        return "LTO-%s" % m.group(1)
    return None


# ------------------------------------------------------------------ helpers --
def read_text(path):
    try:
        with open(path, errors="replace") as f:
            return f.read()
    except OSError:
        return None


def read_json(path):
    try:
        with open(path) as f:
            return json.load(f)
    except (OSError, ValueError):
        return None


def write_json(path, obj):
    tmp = path + ".tmp"
    with open(tmp, "w") as f:
        json.dump(obj, f, indent=1, sort_keys=True)
        f.write("\n")
    os.replace(tmp, path)


def utc(t=None):
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(time.time() if t is None else t))


def first_int(text):
    """The first run of digits in `text`, as an int (a leading 0x is hex)."""
    if text is None:
        return None
    m = re.match(r"\s*(0[xX][0-9a-fA-F]+)", text)
    if m:
        return int(m.group(1), 16)
    m = re.search(r"[0-9]+", text)
    return int(m.group(0)) if m else None


def fig(value, unit=None, source=None, **extra):
    d = {"value": value}
    if unit is not None:
        d["unit"] = unit
    if source is not None:
        d["source"] = source
    d.update(extra)
    return d


# ------------------------------------------------------------------ parsers --
def parse_sg_inq(text):
    out = {}
    for key, pat in (("vendor", r"Vendor identification:\s*(.*)"),
                     ("product", r"Product identification:\s*(.*)"),
                     ("firmware", r"Product revision level:\s*(.*)"),
                     ("serial", r"Unit serial number:\s*(.*)")):
        m = re.search(pat, text or "")
        if m:
            out[key] = m.group(1).strip()
    return out


def parse_mam(text):
    """sg_read_attr's `Label [unit]: value` lines -> {label: {"value", "unit"}}.
    The FIRST line of a label wins (`Assigning organization` appears twice)."""
    out = {}
    for line in (text or "").splitlines():
        m = re.match(r"^\s+([^:\[\]]+?)(?:\s*\[([^\]]+)\])?:\s*(.*?)\s*$", line)
        if not m:
            continue
        label = m.group(1).strip()
        if label not in out:
            out[label] = {"value": m.group(3).strip(), "unit": m.group(2)}
    return out


def mam_value(mam, label):
    e = mam.get(label)
    return e["value"] if e else None


def mam_int(mam, label):
    e = mam.get(label)
    if not e:
        return None
    return first_int(e["value"])


def parse_counters(text):
    """`Label [unit]: N unit` and `Label = N` lines of a decoded sg_logs page ->
    {label: {"value": int, "unit": str|None}}. The first line of a label wins."""
    out = {}
    for line in (text or "").splitlines():
        m = re.match(r"^\s+([^:=\[\]]+?)(?:\s*\[([^\]]+)\])?\s*[:=]\s*(-?[0-9]+)\s*([A-Za-z]*)\s*$", line)
        if not m:
            continue
        label = m.group(1).strip()
        if label not in out:
            out[label] = {"value": int(m.group(3)), "unit": m.group(2) or (m.group(4) or None)}
    return out


def counter(counters, prefix):
    """The value of the first counter whose label starts with `prefix`."""
    for label, e in counters.items():
        if label.startswith(prefix):
            return e["value"], e["unit"], label
    return None, None, None


def hex_page_bytes(text, heading):
    """The bytes of the hex dump that follows a `>> heading` line of sg_modes
    output (lines like ` 00     0f 0e 40 80 ...  ....`), or None."""
    lines = (text or "").splitlines()
    for i, line in enumerate(lines):
        if line.startswith(">>") and heading.lower() in line.lower():
            data = []
            for l in lines[i + 1:]:
                m = re.match(r"^\s*[0-9a-fA-F]{2,}\s{2,}((?:[0-9a-fA-F]{2}\s)+)", l)
                if not m:
                    if l.strip() == "" or l.startswith(">"):
                        break
                    continue
                data.extend(int(b, 16) for b in m.group(1).split())
            return data or None
    return None


def parse_compression(text):
    """Mode page 0x0f: DCE (compression enabled) and DCC (compression supported)."""
    if text is None:
        return None
    m = re.search(r"DCE\s*=\s*([01])", text)
    if m:
        dce = int(m.group(1))
        mc = re.search(r"DCC\s*=\s*([01])", text)
        return {"dce": dce, "dcc": int(mc.group(1)) if mc else None, "via": "decoded DCE bit"}
    data = hex_page_bytes(text, "Data Compression")
    if data and len(data) >= 3 and (data[0] & 0x3F) == 0x0F:
        return {"dce": (data[2] >> 7) & 1, "dcc": (data[2] >> 6) & 1, "via": "page bytes 0f %02x %02x" % (data[1], data[2])}
    return None


def parse_lbp(text):
    """Control Data Protection mode subpage (0x0a/0xf0): LBPM, LBP_W, LBP_R."""
    if text is None:
        return None
    m = re.search(r"LBPM\s*[=:]\s*(0x[0-9a-fA-F]+|[0-9]+)", text)
    if m:
        out = {"lbpm": first_int(m.group(1)), "via": "decoded LBPM field"}
        for key, pat in (("lbp_w", r"LBP_W\s*[=:]\s*([01])"), ("lbp_r", r"LBP_R\s*[=:]\s*([01])")):
            mm = re.search(pat, text)
            out[key] = int(mm.group(1)) if mm else None
        return out
    data = hex_page_bytes(text, "Control")
    if data and len(data) >= 7 and (data[0] & 0x3F) == 0x0A and data[1] == 0xF0:
        return {"lbpm": data[4], "lbp_info_length": data[5], "lbp_w": (data[6] >> 7) & 1,
                "lbp_r": (data[6] >> 6) & 1, "via": "subpage bytes %s" % " ".join("%02x" % b for b in data[:8])}
    return None


def parse_block_limits(text):
    mn = re.search(r"Minimum block size:\s*([0-9]+)", text or "")
    mx = re.search(r"Maximum block size:\s*([0-9]+)", text or "")
    return (int(mn.group(1)) if mn else None, int(mx.group(1)) if mx else None)


def read_tsv(path):
    rows = []
    text = read_text(path)
    for line in (text or "").splitlines():
        if line.strip():
            rows.append(line.split("\t"))
    return rows


def read_facts(path):
    out = {}
    for row in read_tsv(path):
        if len(row) >= 2:
            out[row[0]] = row[1]
    return out


def sibling_counters(run, tag, page):
    return parse_counters(read_text(os.path.join(run, "%s.page_%s.txt" % (tag, page))))


# --------------------------------------------------------------- identity ----
def identify(run):
    """Who and what this run measured: the drive key and the medium."""
    meta = read_json(os.path.join(run, "meta.json")) or {}
    inq = parse_sg_inq(read_text(os.path.join(run, "inq.txt")))
    mam = parse_mam(read_text(os.path.join(run, "mam-preflight.txt")))
    mt = read_text(os.path.join(run, "mt-status.txt")) or ""

    vendor, product, firmware = inq.get("vendor"), inq.get("product"), inq.get("firmware")
    serial = meta.get("drive_serial") or inq.get("serial")
    drive_gen = drive_generation_from_model(product)
    drive = {
        "vendor": vendor, "product": product, "firmware": firmware, "serial": serial,
        "generation": drive_gen,
        "generation_source": "sg_inq product identification %r" % product if drive_gen else None,
        "key": "|".join(str(x) for x in (vendor, product, firmware, drive_gen)),
        "source": "sg_inq (standard INQUIRY: vendor, product, revision); serial from sysfs vpd_pg80",
    }

    dens, dens_src = None, None
    for label in ("Medium density code", "Format density code"):
        v = mam_int(mam, label)
        if v is not None:
            dens, dens_src = v, "MAM attribute %s" % label
            break
    if dens is None:
        m = re.search(r"Density code (0x[0-9a-fA-F]+)", mt)
        if m and int(m.group(1), 16) != 0:
            dens, dens_src = int(m.group(1), 16), "the st driver's density register (mt status)"
    med = media_by_density(dens) if dens is not None else None
    medium = {
        "generation": med["generation"] if med else None,
        "generation_source": ("%s = 0x%02x" % (dens_src, dens)) if dens is not None else None,
        "density_code": dens,
        "nominal_capacity_bytes": med["nominal_bytes"] if med else None,
        "nominal_capacity_source": "generation table (src/media.rs native_capacity_bytes, marketed decimal bytes)" if med else None,
        "serial": mam_value(mam, "Medium serial number"),
        "maker": mam_value(mam, "Medium manufacturer"),
        "maker_source": "MAM attribute Medium manufacturer (0x0400)",
        "barcode": mam_value(mam, "Barcode") or None,
        "length_m": mam_int(mam, "Medium length"),
        "manufacture_date": mam_value(mam, "Medium manufacture date"),
    }

    evidence = []
    if (serial or "").upper().startswith("XYZZY"):
        evidence.append("drive serial %r is an mhvtl serial" % serial)
    if (medium["maker"] or "").lower() == "linuxvtl":
        evidence.append("the cartridge's Medium manufacturer is 'linuxVTL' (mhvtl)")
    emulated = {"is_emulated": bool(evidence), "evidence": evidence}

    problems = []
    if drive_gen is None:
        problems.append("the drive's generation is not known from its INQUIRY product %r" % product)
    if med is None:
        problems.append("the medium's generation is not known (density code %s is not in the LTO-5..LTO-9 tables)" % dens)
    if drive_gen and med and med["generation"] not in CAN_WRITE.get(drive_gen, []):
        problems.append("a %s drive cannot write %s media (src/media.rs can_write)" % (drive_gen, med["generation"]))
    return {"drive": drive, "medium": medium, "emulated": emulated, "problems": problems}


def cmd_identify(a):
    ident = identify(a.run)
    write_json(os.path.join(a.run, "identity.json"), ident)
    d, m = ident["drive"], ident["medium"]
    print("DRIVE_VENDOR=%s" % (d["vendor"] or ""))
    print("DRIVE_PRODUCT=%s" % (d["product"] or ""))
    print("DRIVE_FIRMWARE=%s" % (d["firmware"] or ""))
    print("DRIVE_GENERATION=%s" % (d["generation"] or ""))
    print("MEDIUM_GENERATION=%s" % (m["generation"] or ""))
    print("MEDIUM_NOMINAL_BYTES=%s" % (m["nominal_capacity_bytes"] or ""))
    print("MEDIUM_MAKER=%s" % (m["maker"] or ""))
    print("EMULATED=%d" % (1 if ident["emulated"]["is_emulated"] else 0))
    for p in ident["problems"]:
        print("PROBLEM=%s" % p)
    return 0


def cmd_meta(a):
    path = os.path.join(a.run, "meta.json")
    meta = read_json(path) or {}
    for kv in a.set or []:
        k, _, v = kv.partition("=")
        meta[k] = v
    for kv in a.set_int or []:
        k, _, v = kv.partition("=")
        meta[k] = int(v)
    for kv in a.set_bool or []:
        k, _, v = kv.partition("=")
        meta[k] = v in ("1", "true", "yes")
    write_json(path, meta)
    return 0


# ------------------------------------------------------------------- build ---
def humanize(n):
    return "n/a" if n is None else "{:,}".format(n)


def outcome_text(rec):
    """What write(2) returned after the first refusal, in words: the fill writer's own description
    ('1 refusal (ENOSPC), then 2048 accepted', 'ALTERNATING: ...') when it can be loaded, else the
    run-length list."""
    try:
        import importlib.util
        spec = importlib.util.spec_from_file_location(
            "lto6_fill_writer", os.path.join(os.path.dirname(os.path.abspath(__file__)), "lto6-fill-writer.py"))
        mod = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(mod)
        return mod.describe(rec)
    except Exception:
        runs = (rec or {}).get("outcomes_rle") or []
        parts = ["%s x%d" % (o, c) for o, c in runs[:8]]
        more = " -> ... (%d runs in all)" % rec["outcomes_runs"] if rec and rec.get("outcomes_runs", 0) > 8 else ""
        return " -> ".join(parts) + more


def build_blocksize(run, facts, figures, notes):
    rows = read_tsv(os.path.join(run, "blocksize.tsv"))
    limits = read_text(os.path.join(run, "block-limits.txt"))
    if limits is not None:
        mn, mx = parse_block_limits(limits)
        figures["block_limits"] = fig({"min": mn, "max": mx}, "bytes", "sg_read_block_limits (READ BLOCK LIMITS) -> block-limits.txt")
    results = []
    for r in rows:
        r = r + [""] * (6 - len(r))
        results.append({"label": r[0], "block_bytes": int(r[1]) if r[1].isdigit() else None, "status": r[2],
                        "mib_per_s": float(r[3]) if re.match(r"^[0-9.]+$", r[3]) else None,
                        "seconds": float(r[4]) if re.match(r"^[0-9.]+$", r[4]) else None, "note": r[5]})
    if results:
        figures["block_size"] = fig(results, None, "mt setblk + dd of incompressible data per block size -> blocksize.tsv, setblk-*.txt, dd-*.txt",
                                    payload_mib=int(facts["payload_mib"]) if facts.get("payload_mib", "").isdigit() else None,
                                    payload_location=facts.get("payload_location"),
                                    source_read_mib_s=float(facts["source_read_mib_s"]) if re.match(r"^[0-9.]+$", facts.get("source_read_mib_s", "")) else None)
    else:
        notes.append("block size: not measured (no blocksize.tsv)")


def build_compression_lbp(run, figures, notes):
    c = parse_compression(read_text(os.path.join(run, "modes-0f.txt")))
    if c:
        figures["hardware_compression_as_found"] = fig(
            "on" if c["dce"] else "off", None, "sg_modes --page=0x0f (Data Compression mode page, DCE bit) -> modes-0f.txt (%s)" % c["via"],
            dce=c["dce"], dcc=c["dcc"], note="tapectl turns it off per write (MTCOMPRESSION 0); as-found is recorded")
    else:
        notes.append("hardware compression: mode page 0x0f was not readable or not decodable (modes-0f.txt)")
    l = parse_lbp(read_text(os.path.join(run, "modes-0a-f0.txt")))
    if l:
        method = {0: "none (LBP off or unsupported)", 1: "Reed-Solomon CRC", 2: "CRC32C"}.get(l["lbpm"], "vendor method 0x%x" % l["lbpm"])
        figures["lbp"] = fig(method, None, "sg_modes --page=0x0a,0xf0 (Control Data Protection subpage, MODE SENSE only) -> modes-0a-f0.txt (%s)" % l["via"],
                             lbpm=l["lbpm"], lbp_w=l.get("lbp_w"), lbp_r=l.get("lbp_r"),
                             note="MODE SELECT is not attempted: enabling LBP changes the block format of every later command")
    else:
        notes.append("LBP: the Control Data Protection page (0x0a/0xf0) was not readable or not decodable (modes-0a-f0.txt); treat as unavailable")


def build_mam_capacity(run, facts, ident, figures, notes):
    pre = parse_mam(read_text(os.path.join(run, "snap-dbefore.mam.txt")))
    post = parse_mam(read_text(os.path.join(run, "snap-dafter.mam.txt")))
    if not pre or not post:
        notes.append("MAM capacity unit and over-report: not measured (snap-dbefore.mam.txt / snap-dafter.mam.txt)")
        return
    rb, ra = mam_int(pre, "Remaining capacity in partition"), mam_int(post, "Remaining capacity in partition")
    unit_label = (pre.get("Remaining capacity in partition") or {}).get("unit")
    max_cap = mam_int(pre, "Maximum capacity in partition")
    payload = int(facts["mam_payload_bytes"]) if facts.get("mam_payload_bytes", "").isdigit() else None
    wrote = facts.get("mam_write_ok") == "1"
    p17b = sibling_counters(run, "snap-dbefore", "0x17")
    p17a = sibling_counters(run, "snap-dafter", "0x17")
    nb, _, nlabel = counter(p17b, "Total used native capacity")
    na, _, _ = counter(p17a, "Total used native capacity")
    if not wrote or payload is None or rb is None or ra is None:
        notes.append("MAM capacity unit and over-report: NOT MEASURED -- the capacity write failed or MAM did not report remaining capacity (snap-dbefore.mam.txt, snap-dafter.mam.txt, dd-mam.txt)")
        return
    d_mam = rb - ra
    entry = {"remaining_before": rb, "remaining_after": ra, "decrease": d_mam, "label_unit": unit_label,
             "payload_bytes": payload, "max_capacity_attribute": max_cap}
    src = "sg_read_attr before/after a %d-byte write -> snap-dbefore.mam.txt, snap-dafter.mam.txt" % payload
    if nb is not None and na is not None and na > nb and d_mam > 0:
        native_delta_bytes = (na - nb) * 1000000
        unit_bytes = native_delta_bytes / float(d_mam)
        cls, near = "unclassified", None
        for name, ref in (("MiB (2^20 bytes)", 1048576.0), ("MB (10^6 bytes)", 1000000.0)):
            if abs(unit_bytes - ref) / ref < 0.03:
                cls, near = name, ref
        entry.update({"unit_bytes_per_count": round(unit_bytes, 1), "classified_as": cls,
                      "page_0x17_native_delta_mb": na - nb,
                      "resolution_warning": "the decrease is only %d counts: the unit is a rounding guess" % d_mam if d_mam < 200 else None})
        figures["mam_capacity_unit"] = fig(cls, None,
                                           "ratio of page 0x17 'Total used native capacity' [MB] growth (%s -> %s) to the MAM remaining-capacity decrease (%d) = %.1f bytes per count" % (humanize(nb), humanize(na), d_mam, unit_bytes),
                                           bytes_per_count=round(unit_bytes, 1), label_unit=unit_label)
        used = unit_bytes if near is None else near
        over = max(0, payload - int(d_mam * used))
        over_native = max(0, native_delta_bytes - int(d_mam * used))
        entry["native_used_per_data_byte"] = round(native_delta_bytes / float(payload), 4)
        figures["mam_over_report"] = fig(over, "bytes",
                                         "data bytes written minus MAM's remaining-capacity decrease (decrease x %.0f bytes per count)" % used,
                                         vs_native_used_bytes=over_native, native_used_per_data_byte=entry["native_used_per_data_byte"],
                                         note="MAM's remaining figure at the first refusal is the early-warning reserve, not writable space; see early_warning")
        if max_cap is not None and ident["medium"]["nominal_capacity_bytes"]:
            entry["max_capacity_bytes_if_that_unit"] = int(max_cap * used)
            entry["max_capacity_over_nominal"] = round(max_cap * used / float(ident["medium"]["nominal_capacity_bytes"]), 4)
    else:
        figures["mam_capacity_unit"] = fig(None, None, src,
                                           note="page 0x17 'Total used native capacity' did not grow across the write (an emulator, or the page is unreadable), so the unit cannot be derived; the label says %r" % unit_label)
        figures["mam_over_report"] = fig(None, "bytes", src, note="needs the unit")
    figures["mam_remaining_capacity"] = fig(entry, None, src)


def build_eod(run, facts, figures, notes):
    if "eod_verdict" not in facts:
        notes.append("EOD semantics: not measured")
        return
    v = facts["eod_verdict"]
    text = {"errored": "PASS: a read past end of data returned an error, as the chain walk assumes",
            "no-data": "PASS: a read past end of data returned no data",
            "stale-data": "FAIL: a read past end of data returned DATA -- a truncated session could read as complete",
            "not-measured": "NOT MEASURED: the three probe writes did not all succeed"}.get(v, v)
    figures["eod_semantics"] = fig(v, None, "mt fsf 3 then dd bs=512K count=1 past the third filemark -> eod-read.txt",
                                   verdict=text, read_exit=facts.get("eod_read_exit"), read_bytes=facts.get("eod_read_bytes"),
                                   error_text=facts.get("eod_error_text"), probe_writes_ok=facts.get("eod_writes_ok"))


def snapshot_figures(run, tag):
    """The wear-relevant numbers of one snapshot: MAM lifetime attributes and the counters of log pages 0x02, 0x03, 0x17."""
    mam = parse_mam(read_text(os.path.join(run, "%s.mam.txt" % tag)))
    out = {"tag": tag, "taken": None}
    if mam:
        out["mam"] = {
            "load_count": mam_int(mam, "Load count"),
            "initialization_count": mam_int(mam, "Initialization count"),
            "life_written_mib": mam_int(mam, "Total MiB written in medium life"),
            "life_read_mib": mam_int(mam, "Total MiB read in medium life"),
            "load_written_mib": mam_int(mam, "Total MiB written in current/last load"),
            "load_read_mib": mam_int(mam, "Total MiB read in current/last load"),
            "tape_alert_flags": mam_value(mam, "TapeAlert flags"),
            "remaining_capacity": mam_int(mam, "Remaining capacity in partition"),
            "maximum_capacity": mam_int(mam, "Maximum capacity in partition"),
            "manufacture_date": mam_value(mam, "Medium manufacture date"),
            "drive_ring_last_load": mam_value(mam, "Density vendor/serial number at last load"),
        }
    pages = {}
    for page in ("0x02", "0x03", "0x0c", "0x17"):
        c = sibling_counters(run, tag, page)
        if c:
            pages[page] = dict((k, e["value"]) for k, e in c.items())
    if pages:
        out["log_pages"] = pages
    return out


def build_wear(run, early, ident):
    wear = {"note": "recorded, not fitted (ADR-0012 item 98): collect these across runs; the fit waits for data",
            "mam_attributes_source": "sg_read_attr", "log_pages_source": "sg_logs 0x02/0x03/0x0c/0x17 (decoded offline from raw reads)"}
    snaps = []
    for tag in ("snap-asfound", "snap-final"):
        s = snapshot_figures(run, tag)
        if "mam" in s or "log_pages" in s:
            snaps.append(s)
    wear["snapshots"] = snaps
    # the drive's rewrite counters, as health.rs reads them
    for s in snaps:
        pages = s.get("log_pages", {})
        s["drive_rewrite_counters"] = {
            "write_total_rewrites": (pages.get("0x02") or {}).get("Total rewrites or rereads"),
            "read_total_rereads": (pages.get("0x03") or {}).get("Total rewrites or rereads"),
            "write_uncorrected_errors": (pages.get("0x02") or {}).get("Total uncorrected errors"),
            "total_write_retries": (pages.get("0x17") or {}).get("Total write retries"),
            "total_unrecovered_write_errors": (pages.get("0x17") or {}).get("Total unrecovered write errors"),
        }
    wear["end_of_tape"] = early
    return wear


def build_fill(run, ident, facts, figures, notes, options):
    fdir = os.path.join(run, "fill")
    res = read_json(os.path.join(fdir, "result.json"))
    rb = read_json(os.path.join(fdir, "readback.json")) or {}
    nominal = ident["medium"]["nominal_capacity_bytes"]
    early = {"observed": False, "nominal_bytes": nominal}
    post = {"observed": False}
    if res is None:
        notes.append("early warning and the room past it: not measured (no fill/result.json; --quick, or the fill did not run)")
        return early, post
    fill = res.get("fill") or {}
    fr = res.get("first_refusal") or {}
    sl = res.get("slice")
    he = res.get("hard_end")
    mam_before = parse_mam(read_text(os.path.join(fdir, "before.mam.txt")))
    mam_max = mam_int(mam_before, "Maximum capacity in partition")
    atw_mam = parse_mam(read_text(os.path.join(fdir, "at-warning.mam.txt")))
    observed = fill.get("end") == "refusal" and bool(fr)
    early.update({
        "observed": observed,
        "fill_ended": fill.get("end"),
        "source": "scripts/lto6-fill-writer.py write: one open of the device, 512 KiB write(2)s from BOT -> fill/result.json",
        "bytes_before_first_refusal": fill.get("bytes") if observed else None,
        "bytes_written_when_fill_ended": fill.get("bytes"),
        "first_refusal_errno": fr.get("outcome"),
        "first_refusal_time": fr.get("time"),
        "host_feed_mb_per_s": round(fill.get("mb_per_s", 0.0), 1) if fill else None,
        "fill_seconds": round(fill.get("seconds", 0.0), 1) if fill else None,
        "mam_maximum_capacity_attribute": mam_max,
        "mam_remaining_at_warning": mam_int(atw_mam, "Remaining capacity in partition"),
    })
    if observed and nominal:
        early["fraction_of_nominal"] = round(fill["bytes"] / float(nominal), 6)
    after_rewind = sibling_counters(fdir, "after-rewind", "0x17")
    used, _, _ = counter(after_rewind, "Total used native capacity")
    written = (fill.get("bytes") or 0) + ((sl or {}).get("accepted", 0) + (he or {}).get("accepted", 0) + (1 if (res.get("seal") or {}).get("ok") else 0)) * res.get("block_size", 524288)
    if used and written:
        early["native_used_per_data_byte"] = round(used * 1e6 / written, 4)
    if observed:
        figures["early_warning"] = fig(early["bytes_before_first_refusal"], "bytes",
                                       "first write(2) refusal (%s) of the fill from BOT -> fill/result.json first_refusal" % fr.get("outcome"),
                                       fraction_of_nominal=early.get("fraction_of_nominal"),
                                       nominal_bytes=nominal, host_feed_mb_per_s=early["host_feed_mb_per_s"])
    else:
        end = fill.get("end")
        figures["early_warning"] = fig(None, "bytes", "fill/result.json fill.end = %r" % end,
                                       note={"cap": "no refusal before the runaway cap: the drive reported no early warning (an emulator?)",
                                             "error": "the fill stopped on a write error that is not a refusal",
                                             "interrupted": "interrupted before any refusal"}.get(end, "the fill did not reach a refusal"))
    # the room past the warning
    reported = None
    for tag in ("at-warning", "after", "before"):
        c = sibling_counters(fdir, tag, "0x0c")
        v, _, label = counter(c, "Minimum native capacity from EW to EOP")
        if v is not None:
            reported = {"mb": v, "bytes": v * 1000000, "tag": tag, "label": label}
            break
    post.update({"observed": observed and sl is not None, "reported_ew_to_eop": reported})
    if reported:
        figures["post_warning_room_reported"] = fig(reported["bytes"], "bytes",
                                                    "sg_logs page 0x0c %r = %s MB (decimal), read in the %s snapshot -> fill/%s.page_0x0c.txt" % (reported["label"], humanize(reported["mb"]), reported["tag"], reported["tag"]))
    measured, measured_basis = None, None
    if sl:
        post["slice"] = {"target_blocks": res.get("slice_target_blocks"), "accepted_blocks": sl.get("accepted"),
                         "stop": sl.get("stop"), "refusals": sl.get("refusals"),
                         "outcomes": outcome_text(sl), "full_slice_accepted": sl.get("accepted", 0) >= res.get("slice_target_blocks", 1)}
        post["filemark_after_slice"] = bool((res.get("filemark1") or {}).get("ok"))
        post["seal_block"] = bool((res.get("seal") or {}).get("ok"))
        post["filemark_after_seal"] = bool((res.get("filemark2") or {}).get("ok"))
        post["readback"] = ("FAIL" if any(c.get("status") == "FAIL" for c in rb.get("checks", [])) else "PASS" if rb.get("checks") else "not run")
        past = (sl.get("accepted") or 0) * res.get("block_size", 524288)
        if he:
            past = he.get("bytes_past_first_refusal", past)
            hard = he.get("stop", "")
            reached = not str(hard).startswith("cap reached") and not str(hard).startswith("interrupted") and he.get("stop") != "complete"
            post["hard_end"] = {"walked": True, "accepted_blocks": he.get("accepted"), "stop": hard, "reached_hard_end": reached,
                                "bytes_past_first_refusal": past,
                                "cap_bytes": (options.get("hard_end_cap_gb") or 0) * 10 ** 9 or None}
            measured, measured_basis = past, ("the walk from the first refusal to the hard end (%s)" % hard) if reached else \
                ("a LOWER BOUND: the walk stopped at its cap (%s)" % hard)
        else:
            post["hard_end"] = {"walked": False, "reason": res.get("hard_end_skipped") or "not requested (--no-hard-end, or --stop-at-warning)"}
            if sl.get("stop") == "complete":
                measured, measured_basis = None, None
    if measured is not None:
        figures["post_warning_room_measured"] = fig(measured, "bytes", "fill/result.json hard_end.bytes_past_first_refusal: %s" % measured_basis,
                                                    lower_bound=not post["hard_end"].get("reached_hard_end", False))
    candidates = []
    if measured is not None and post.get("readback") == "FAIL":
        post["measured_ignored"] = ("the readback FAILED: bytes the drive accepted past the warning are not proven readable, "
                                    "so the walk does not count towards the room")
    elif measured is not None:
        candidates.append((measured, "measured walk"))
    if reported:
        candidates.append((reported["bytes"], "drive-reported page 0x0c"))
    if candidates and observed:
        room, basis = min(candidates)
        post["room_bytes"] = room
        post["room_basis"] = "the lower of the measured walk and the drive-reported figure: %s" % " vs ".join("%s %s" % (b, humanize(v)) for v, b in candidates) \
            if len(candidates) == 2 else "only the %s was available: %s" % (basis, humanize(room))
        post["room_is_lower_bound"] = bool(measured is not None and not post["hard_end"].get("reached_hard_end", False) and room == measured)
    return early, post


def build_profile(run):
    run = os.path.abspath(run)
    meta = read_json(os.path.join(run, "meta.json")) or {}
    facts = read_facts(os.path.join(run, "facts.tsv"))
    ident = identify(run)
    write_json(os.path.join(run, "identity.json"), ident)
    figures, notes = {}, []
    figures["drive_inventory"] = fig(
        {"vendor": ident["drive"]["vendor"], "product": ident["drive"]["product"], "firmware": ident["drive"]["firmware"], "serial": ident["drive"]["serial"]},
        None, ident["drive"]["source"])
    pages = read_text(os.path.join(run, "logs-00.txt"))
    if pages:
        figures["supported_log_pages"] = fig(re.findall(r"^\s+(0x[0-9a-fA-F]{2})\s", pages, re.M), None, "sg_logs --page=0 -> logs-00.txt")
    build_compression_lbp(run, figures, notes)
    build_blocksize(run, facts, figures, notes)
    build_mam_capacity(run, facts, ident, figures, notes)
    build_eod(run, facts, figures, notes)
    early, post = build_fill(run, ident, facts, figures, notes, (meta.get("options") or {}))
    wear = build_wear(run, early, ident)
    emulated = ident["emulated"]
    if emulated["is_emulated"]:
        emulated["fiction"] = ["mam_capacity_unit", "mam_over_report", "mam_remaining_capacity", "early_warning", "post_warning_room_measured",
                               "post_warning_room_reported", "wear", "block_size throughput (disk speed)", "the log pages and MAM capacity attributes (static)"]
        emulated["note"] = ("This run measured an EMULATOR. It exercised the toolkit's mechanics; the capacity, early-warning, "
                            "log-page and wear figures are the emulator's fiction, and `--emit` ignores the run unless told "
                            "otherwise. Block-size acceptance and end-of-data behaviour describe the emulator, not a drive.")
    profile = {
        "format": FORMAT, "format_version": FORMAT_VERSION,
        "tool": {"script": "scripts/lto-profile.sh", "report_builder": "scripts/lto-profile-report.py",
                 "tapectl_commit": meta.get("tapectl_commit")},
        "run": {"id": os.path.basename(run), "directory": run, "mode": meta.get("mode"), "started": meta.get("started"),
                "finished": meta.get("finished") or utc(), "host": meta.get("host") or socket.gethostname(),
                "device": meta.get("device"), "note": meta.get("note"), "options": meta.get("options") or {},
                "exit": {"fill_writer": first_int(facts.get("fill_writer_rc")), "fill_readback": first_int(facts.get("fill_verify_rc"))}},
        "emulated": emulated,
        "drive": ident["drive"],
        "medium": ident["medium"],
        "problems": ident["problems"],
        "figures": figures,
        "early_warning": early,
        "post_warning": post,
        "wear": wear,
        "not_measured": notes,
    }
    write_json(os.path.join(run, "profile.json"), profile)
    with open(os.path.join(run, "PROFILE.txt"), "w") as f:
        f.write(render_text(profile))
    return profile


def render_text(p):
    out = []
    w = out.append
    d, m, e = p["drive"], p["medium"], p["emulated"]
    w("== LTO profile %s ==" % p["run"]["id"])
    if e["is_emulated"]:
        w("EMULATED DEVICE (%s)" % "; ".join(e["evidence"]))
        w(e["note"])
    w("run: %s, mode %s, started %s, finished %s, host %s" % (p["run"]["id"], p["run"].get("mode"), p["run"].get("started"), p["run"].get("finished"), p["run"].get("host")))
    w("drive key: %s" % d["key"])
    w("  vendor %r, product %r, firmware %r, serial %r, drive generation %s -- %s" % (d["vendor"], d["product"], d["firmware"], d["serial"], d["generation"], d["source"]))
    w("medium: generation %s (%s), serial %s, maker %s (%s), length %s m, made %s" % (
        m["generation"], m["generation_source"], m["serial"], m["maker"], m["maker_source"], m["length_m"], m["manufacture_date"]))
    w("  nominal capacity %s bytes (%s)" % (humanize(m["nominal_capacity_bytes"]), m["nominal_capacity_source"]))
    for pr in p["problems"]:
        w("PROBLEM: %s" % pr)
    w("")
    w("-- figures (value; source) --")
    for name, f in sorted(p["figures"].items()):
        v = f["value"]
        if isinstance(v, (dict, list)):
            v = json.dumps(v, sort_keys=True)
        extra = "; ".join("%s=%s" % (k, json.dumps(x, sort_keys=True) if isinstance(x, (dict, list)) else x)
                          for k, x in sorted(f.items()) if k not in ("value", "unit", "source") and x is not None)
        w("%s: %s%s" % (name, "not derived" if v is None else v, (" " + f["unit"]) if f.get("unit") and v is not None else ""))
        w("    source: %s" % f.get("source"))
        if extra:
            w("    %s" % extra)
    w("")
    ew, pw = p["early_warning"], p["post_warning"]
    w("-- end of tape (item 98) --")
    if ew.get("observed"):
        w("first refusal after %s bytes = %.4f of the nominal %s; errno %s; host feed %s MB/s over %s s" % (
            humanize(ew["bytes_before_first_refusal"]), ew.get("fraction_of_nominal") or 0, humanize(ew["nominal_bytes"]),
            ew.get("first_refusal_errno"), ew.get("host_feed_mb_per_s"), ew.get("fill_seconds")))
        if ew.get("native_used_per_data_byte") is not None:
            w("native capacity used per data byte (page 0x17 after rewind): %s" % ew["native_used_per_data_byte"])
        if pw.get("slice"):
            s = pw["slice"]
            w("past the warning: slice %s of %s blocks (%s); filemark %s, seal block %s, filemark %s; readback %s" % (
                s["accepted_blocks"], s["target_blocks"], s["outcomes"], pw.get("filemark_after_slice"), pw.get("seal_block"),
                pw.get("filemark_after_seal"), pw.get("readback")))
        he = pw.get("hard_end") or {}
        if he.get("walked"):
            w("hard end: %s bytes past the first refusal; stop: %s; reached the hard end: %s" % (humanize(he["bytes_past_first_refusal"]), he["stop"], he["reached_hard_end"]))
        elif he:
            w("hard end: not walked (%s)" % he.get("reason"))
        if pw.get("room_bytes") is not None:
            w("post-warning room: %s bytes -- %s%s" % (humanize(pw["room_bytes"]), pw["room_basis"], " (a lower bound)" if pw.get("room_is_lower_bound") else ""))
    else:
        w("no early warning was observed in this run (%s)" % ((ew.get("fill_ended") or "no fill ran")))
    w("")
    w("-- wear, recorded beside it --")
    for s in p["wear"].get("snapshots", []):
        w("%s: MAM %s" % (s["tag"], json.dumps(s.get("mam"), sort_keys=True)))
        w("%s: drive rewrite counters %s" % (s["tag"], json.dumps(s.get("drive_rewrite_counters"), sort_keys=True)))
        pages = s.get("log_pages", {}).get("0x17")
        if pages:
            w("%s: volume statistics (page 0x17) %s" % (s["tag"], json.dumps(pages, sort_keys=True)))
    if p["not_measured"]:
        w("")
        w("-- not measured --")
        for n in p["not_measured"]:
            w(n)
    return "\n".join(out) + "\n"


def cmd_build(a):
    p = build_profile(a.run)
    sys.stdout.write(render_text(p))
    return 0


# -------------------------------------------------------------------- emit ---
def load_profile(path):
    if os.path.isdir(path):
        pj = os.path.join(path, "profile.json")
        if not os.path.exists(pj):
            if os.path.exists(os.path.join(path, "meta.json")):
                return build_profile(path)
            raise SystemExit("emit: %s has no profile.json (and no raw captures to build one from)" % path)
        path = pj
    p = read_json(path)
    if not p:
        raise SystemExit("emit: %s is not readable JSON" % path)
    if p.get("format") != FORMAT:
        raise SystemExit("emit: %s is not a %s report (format %r)" % (path, FORMAT, p.get("format")))
    if p.get("format_version") != FORMAT_VERSION:
        raise SystemExit("emit: %s is format version %r; this emitter reads version %d" % (path, p.get("format_version"), FORMAT_VERSION))
    return p


def run_tag(p):
    m = p["medium"]
    return "%s (cartridge %s, %s, %s)" % (p["run"]["id"], m.get("serial"), m.get("maker"), (p["run"].get("started") or p["run"].get("finished") or "?")[:10])


def wrap_comment(text):
    return textwrap.fill(text, width=110, initial_indent="# ", subsequent_indent="#   ", break_long_words=False, break_on_hyphens=False)


def percent_floor(x):
    return math.floor(x * 100 + 1e-9) / 100.0


def derive_block(profiles, margin, trust_emulated, name=None, device_tape=None, allow_mixed_firmware=False):
    """The block text and the warnings. Pure function of the profiles."""
    if not profiles:
        raise SystemExit("emit: no run directories given")
    first = profiles[0]
    for p in profiles[1:]:
        for what, get in (("vendor", lambda q: q["drive"]["vendor"]), ("product (drive model)", lambda q: q["drive"]["product"]),
                          ("drive generation", lambda q: q["drive"]["generation"]), ("medium generation", lambda q: q["medium"]["generation"])):
            if get(p) != get(first):
                raise SystemExit("emit: refusing to combine runs of different %s: %s has %r, %s has %r" % (
                    what, first["run"]["id"], get(first), p["run"]["id"], get(p)))
    fw = sorted(set(str(p["drive"]["firmware"]) for p in profiles))
    if len(fw) > 1 and not allow_mixed_firmware:
        raise SystemExit("emit: the runs were made on different drive firmware (%s); the profile is keyed by firmware (ADR-0012 item 97). "
                         "Profile each firmware separately, or pass --allow-mixed-firmware to take the most conservative figure across them." % ", ".join(fw))
    for p in profiles:
        if p.get("problems"):
            raise SystemExit("emit: %s recorded problems (%s); fix the run first" % (p["run"]["id"], "; ".join(p["problems"])))
    d, m = first["drive"], first["medium"]
    warnings = []
    usable, skipped = [], []
    for p in profiles:
        if p["emulated"]["is_emulated"] and not trust_emulated:
            skipped.append(p)
        else:
            usable.append(p)
    for p in skipped:
        warnings.append("%s measured an emulator: ignored for every derived value (--trust-emulated includes it)" % p["run"]["id"])

    nominal = m["nominal_capacity_bytes"]
    # ---- fill_ceiling
    points = [(p["early_warning"]["fraction_of_nominal"], p) for p in usable
              if p["early_warning"].get("observed") and p["early_warning"].get("fraction_of_nominal")]
    if points:
        lowest = min(f for f, _ in points)
        candidate = percent_floor(min(1.0, lowest - margin))
        low_run = [p for f, p in points if f == lowest][0]
        if candidate <= 0:
            raise SystemExit("emit: the measured early-warning fraction %.4f leaves nothing after the %.2f margin" % (lowest, margin))
        fill_ceiling = candidate
        fc_note = ("from %s: the lowest of %d run(s) -- first write refusal at %.4f of the %s-byte marketed capacity (%s bytes), "
                   "less the %.2f margin, rounded down to a whole percent" % (
                       run_tag(low_run), len(points), lowest, humanize(nominal), humanize(low_run["early_warning"]["bytes_before_first_refusal"]), margin))
        if fill_ceiling < DEFAULT_FILL_CEILING:
            warnings.append("the measured early warning (%.4f of nominal) supports only fill_ceiling = %.2f, BELOW the %.2f default" % (lowest, fill_ceiling, DEFAULT_FILL_CEILING))
            fc_note += ". LOWER than the %.2f default: this drive and cartridge stop short of it" % DEFAULT_FILL_CEILING
        elif fill_ceiling == DEFAULT_FILL_CEILING:
            fc_note += ". The data supports no more than the %.2f default" % DEFAULT_FILL_CEILING
        runs_for_fc = [p for _, p in points]
    else:
        fill_ceiling = DEFAULT_FILL_CEILING
        runs_for_fc = usable
        if not usable:
            fc_note = "kept at the %.2f default: every run measured an emulator" % DEFAULT_FILL_CEILING
        else:
            fc_note = ("kept at the %.2f default: no run measured an early-warning point (a --quick run, or a fill that "
                       "reached no refusal) -- runs: %s" % (DEFAULT_FILL_CEILING, "; ".join(run_tag(p) for p in usable)))
    # ---- post_warning_bytes
    rooms = [(p["post_warning"]["room_bytes"], p) for p in usable
             if p["early_warning"].get("observed") and p["post_warning"].get("room_bytes")]
    if rooms:
        smallest = min(r for r, _ in rooms)
        room_run = [p for r, p in rooms if r == smallest][0]
        post_bytes = smallest
        lb = " (a lower bound: the walk stopped at its cap)" if room_run["post_warning"].get("room_is_lower_bound") else ""
        pw_note = "from %s: the smallest of %d run(s) -- %s%s" % (run_tag(room_run), len(rooms), room_run["post_warning"]["room_basis"], lb)
    else:
        post_bytes = None
        pw_note = None

    # ---- the text
    L = []
    a = L.append
    a("# tapectl LTO profile block, emitted by scripts/lto-profile.sh --emit on %s" % utc())
    a("# drive:   %s %s, firmware %s (%s drive); medium generation %s" % (d["vendor"], d["product"], ", ".join(fw), d["generation"], m["generation"]))
    a(textwrap.fill("runs:    " + "; ".join(run_tag(p) for p in profiles), width=110, initial_indent="# ",
                    subsequent_indent="#          ", break_long_words=False, break_on_hyphens=False))
    a("# media makers seen (recorded, not a key, ADR-0012 item 97): %s" % (", ".join(sorted(set(str(p['medium'].get('maker')) for p in profiles)))))
    a("# rules:   fill_ceiling = the LOWEST over runs of (bytes written before the first write refusal / the marketed")
    a("#          capacity), less a %.2f margin, rounded DOWN to a whole percent, at most 1.0; %.2f when no run measured" % (margin, DEFAULT_FILL_CEILING))
    a("#          an early warning. post_warning_bytes = the SMALLEST over runs of the lower of the measured walk from the")
    a("#          first refusal to the hard end and the drive's page 0x0c 'minimum native capacity from EW to EOP'.")
    a("#          Only drive-level keys are set; capacity stays a property of the cartridge (ADR-0010).")
    for w in warnings:
        a("# WARNING: %s" % w)
    a("# Set name and device_tape for your drive, then paste this block into config.toml (`tapectl config check`).")
    a("[[backends.lto]]")
    serials = sorted(set(str(p["drive"].get("serial")) for p in profiles if p["drive"].get("serial")))
    a('name = "%s"' % (name or re.sub(r"[^a-z0-9]+", "-", ("%s-%s" % (d["vendor"], d["product"])).lower()).strip("-")))
    if device_tape:
        a('device_tape = "%s"' % device_tape)
    elif len(serials) == 1:
        a('device_tape = "/dev/tape/by-id/scsi-%s-nst"' % serials[0])
    else:
        a('device_tape = "/dev/tape/by-id/scsi-SERIAL-OF-YOUR-DRIVE-nst"')
    a('generation = "%s"' % d["generation"])
    a(wrap_comment("fill_ceiling " + fc_note))
    a("fill_ceiling = %.2f" % fill_ceiling)
    if post_bytes is not None:
        a(wrap_comment("post_warning_bytes " + pw_note))
        a('post_warning_bytes = "%s"' % (("%dM" % (post_bytes // 1000000)) if post_bytes % 1000000 == 0 else str(post_bytes)))
        a("# (a slice plus the seal must fit in half of post_warning_bytes: config check and stage create refuse a larger slice_size)")
    else:
        a("# post_warning_bytes: not set -- no usable run walked past the early warning and none reported page 0x0c's figure;")
        a("#   absent means nothing is checked. Run the profile without --quick on a test cartridge to measure it.")
    return "\n".join(L) + "\n", warnings


def cmd_emit(a):
    profiles = [load_profile(p) for p in a.runs]
    text, warnings = derive_block(profiles, a.margin, a.trust_emulated, a.name, a.device_tape, a.allow_mixed_firmware)
    for w in warnings:
        sys.stderr.write("lto-profile --emit: warning: %s\n" % w)
    sys.stdout.write(text)
    return 0


def cmd_media_table(a):
    print(json.dumps({"generations": [{"generation": g, "density_code": c, "native_capacity_bytes": cap} for g, c, cap in MEDIA],
                      "drive_generations": DRIVE_GENERATIONS, "can_write": CAN_WRITE,
                      "default_fill_ceiling": DEFAULT_FILL_CEILING}, indent=1, sort_keys=True))
    return 0


# ---------------------------------------------------------------- selftest ---
def selftest():
    failures = []

    def check(name, cond):
        print("  %s %s" % ("ok  " if cond else "FAIL", name))
        if not cond:
            failures.append(name)

    for model, gen in (("ULT3580-TD6", "LTO-6"), ("ULT3580-HH8", "LTO-8"), ("ULTRIUM-TD5", "LTO-5"), ("Ultrium 6-SCSI", "LTO-6"),
                       ("Ultrium 9-SCSI", "LTO-9"), ("ULT3580-TD7-M8", None), ("DAT160", None), ("", None)):
        check("drive generation of %r is %s" % (model, gen), drive_generation_from_model(model) == gen)
    mam = parse_mam("Attribute values:\n  Remaining capacity in partition [MiB]: 2499053\n  Medium serial number: EW7VWMVKF6                      \n"
                    "  Assigning organization: LTO-CVE \n  Assigning organization: other\n  Medium density code: 0x5a\n")
    check("MAM: integer with a unit", mam_int(mam, "Remaining capacity in partition") == 2499053 and mam["Remaining capacity in partition"]["unit"] == "MiB")
    check("MAM: padded serial is trimmed", mam_value(mam, "Medium serial number") == "EW7VWMVKF6")
    check("MAM: the first of a repeated label wins", mam_value(mam, "Assigning organization") == "LTO-CVE")
    check("MAM: hex density code", mam_int(mam, "Medium density code") == 0x5A)
    c = parse_counters("Sequential access device page (ssc-3)\n  Native capacity from BOP to EOD: 23 MB\n  Minimum native capacity from EW to EOP of current partition: 99008 MB\n"
                       "  Total used native capacity [MB]: 23\n  Total rewrites or rereads = 7\n")
    check("counters: EW to EOP", counter(c, "Minimum native capacity from EW to EOP")[0] == 99008)
    check("counters: bracketed unit label", counter(c, "Total used native capacity")[0] == 23)
    check("counters: '=' form", counter(c, "Total rewrites or rereads")[0] == 7)
    check("compression: hex page, DCE=0 DCC=1", parse_compression(">> Data Compression, page_control: current\n 00     0f 0e 40 80 ff ff ff ff  ff ff ff ff 00 00 00 00\n")["dce"] == 0)
    check("compression: hex page, DCE=1", parse_compression(">> Data Compression, page_control: current\n 00     0f 0e c0 80 ff ff ff ff  ff ff ff ff 00 00 00 00\n")["dce"] == 1)
    check("compression: decoded", parse_compression("  DCE=1  DCC=1  DDE=1  RED=0\n")["dce"] == 1)
    lbp = parse_lbp(">> Control, page_control: current\n 00     4a f0 00 1c 02 04 c0 00  00 00 00 00 00 00 00 00\n")
    check("LBP: subpage bytes", lbp and lbp["lbpm"] == 2 and lbp["lbp_w"] == 1 and lbp["lbp_r"] == 1)
    check("LBP: mhvtl's page, method 0", parse_lbp(">> Control, page_control: current\n 00     0a f0 00 1c 00 04 00 00  00 00 00 00 00 00 00 00\n")["lbpm"] == 0)
    check("block limits", parse_block_limits("\tMinimum block size: 1 byte(s)\n\tMaximum block size: 2097152 byte(s), 2048 KB, 2 MB\n") == (1, 2097152))
    check("percent_floor never rounds up", percent_floor(0.9808) == 0.98 and percent_floor(0.98) == 0.98 and percent_floor(0.9799) == 0.97)
    check("media table matches its own lookups", all(media_by_density(c)["generation"] == g for g, c, _ in MEDIA))
    check("every drive generation writes its own media", all(g in CAN_WRITE[g] for g in DRIVE_GENERATIONS))
    print("\n%s" % ("selftest: all passed" if not failures else "selftest: %d FAILED: %s" % (len(failures), failures)))
    return 1 if failures else 0


def parser():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    sub = ap.add_subparsers(dest="cmd")
    for name in ("build", "identify"):
        p = sub.add_parser(name)
        p.add_argument("run")
    p = sub.add_parser("meta")
    p.add_argument("run")
    p.add_argument("--set", action="append")
    p.add_argument("--set-int", action="append")
    p.add_argument("--set-bool", action="append")
    p = sub.add_parser("emit")
    p.add_argument("runs", nargs="+")
    p.add_argument("--margin", type=float, default=DEFAULT_MARGIN,
                   help="subtracted from the lowest measured early-warning fraction (default %(default)s)")
    p.add_argument("--name", help="the backend's name (default: derived from the drive model)")
    p.add_argument("--device-tape", help="the backend's device_tape (default: the by-id path of the profiled drive)")
    p.add_argument("--allow-mixed-firmware", action="store_true")
    p.add_argument("--trust-emulated", action="store_true",
                   help="derive values from runs on an emulator (mhvtl), whose figures are fiction: for rehearsals only")
    sub.add_parser("media-table")
    p = sub.add_parser("drive-generation", help="the drive generation of an INQUIRY product string (for tests)")
    p.add_argument("model")
    sub.add_parser("selftest")
    return ap


def main(argv):
    a = parser().parse_args(argv)
    if a.cmd == "build":
        return cmd_build(a)
    if a.cmd == "identify":
        return cmd_identify(a)
    if a.cmd == "meta":
        return cmd_meta(a)
    if a.cmd == "emit":
        return cmd_emit(a)
    if a.cmd == "media-table":
        return cmd_media_table(a)
    if a.cmd == "drive-generation":
        g = drive_generation_from_model(a.model)
        if g:
            print(g)
        return 0 if g else 1
    if a.cmd == "selftest":
        return selftest()
    parser().print_help()
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
