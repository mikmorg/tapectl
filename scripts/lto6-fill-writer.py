#!/usr/bin/env python3
"""lto6-fill-writer.py -- the one-process tape writer, reader and summary of the fill in
scripts/lto-profile.sh (issue #420, the short seal at the drive's early warning; issue #479 moved
the driver, and scripts/lto6-fill.sh is now a wrapper of it). The file keeps its historical name.

WHY IT IS ONE PROCESS. The Linux st driver admits one opener, and closing it
after a write makes the driver write a filemark. Everything this run measures
-- what st and the drive do AFTER the first write refusal -- must therefore
happen on one open file descriptor, exactly as tapectl's own writer holds the
drive from the first block to the seal. So `write` does the whole fill from BOT
(512 KiB write(2)s from the same AES-CTR keystream file the dd fill used), the
first refusal, the post-warning slice, the filemark / seal block / filemark
and, on request, the walk to the hard end, without ever closing the device in
between. Its status calls are MTIOCGET (no SCSI command) and, at three points
where a flush happens anyway, MTIOCPOS (READ POSITION). A failed MTIOCPOS is
recorded with its errno (result.json: setup.lba0_errno, read_position).

Subcommands (scripts/lto-profile.sh drives them; see its header):

  write    open DEV, fill from BOT to the first refusal, then the past-warning
           phase. Writes RUN/result.json, RUN/writer.log (every attempt of the
           slice) and, at the warning, RUN/at-warning.* (MAM + log pages).
  verify   read back, never the whole tape: the post-warning slice in full, the
           filemark after it, the seal block, the filemark after it, and blocks
           of a hard-end run. Two routes (--route, default auto):
             locate   LOCATE to the block addresses READ POSITION recorded
                      while the writer still held the drive;
             spacing  rewind, SPACE forward over the fill (MTFSR) to a few
                      blocks before the first refusal, and read on from there:
                      no READ POSITION, no LOCATE (#487: on home2's HP LTO-6
                      both failed through st).
           auto takes locate, and spacing when the writer recorded no position
           or the LOCATE to the slice fails. Writes RUN/readback.json, with the
           route it took.
  summary  the plain-lines summary the CTO sends back (RUN/summary.txt).
  selftest the state machine and the readback against an in-memory fake tape
           (no hardware): `scripts/lto6-fill-writer.py selftest`.

Python 3.7 and stdlib only (home2 is Debian 10).
"""
import argparse
import errno
import fcntl
import json
import os
import signal
import struct
import subprocess
import sys
import time
import traceback

BLOCK = 524288                    # tapectl's block size
SLICE_BLOCKS = 2048               # a 1 GiB slice (ADR-0012, 2026-10-06)
REFUSAL_RUN_LIMIT = 8             # see --refusal-run-limit in the usage text
HARD_END_CAP = 110000000000       # 110 GB past the warning (the reserve is ~99 GB)
PRIOR_FILL_BYTES = 2501995134976  # the 2026-09-24 fill on this cartridge

# ---------------------------------------------------------------- st ioctls --
MTGET_FMT = "lllllii"   # mt_type mt_resid mt_dsreg mt_gstat mt_erreg mt_fileno mt_blkno
MTOP_FMT = "hi"         # mt_op mt_count
MTPOS_FMT = "l"         # mt_blkno


def _ioc(direction, nr, size):
    return (direction << 30) | (size << 16) | (ord("m") << 8) | nr


MTIOCTOP = _ioc(1, 1, struct.calcsize(MTOP_FMT))
MTIOCGET = _ioc(2, 2, struct.calcsize(MTGET_FMT))
MTIOCPOS = _ioc(2, 3, struct.calcsize(MTPOS_FMT))
MTFSF, MTFSR, MTWEOF, MTREW, MTSETBLK, MTSEEK, MTCOMPRESSION, MTWEOFI = 1, 3, 5, 6, 20, 22, 32, 35
# SPACE(6)'s count is a 24-bit signed field (8,388,607 at most) and st passes MTFSR's count into it unchecked:
# the spacing route spaces in chunks no larger than this. An LTO-8 fill is more than 8.4 million 512 KiB blocks.
SPACE_CHUNK = 8000000
SPACE_MAX = 8388607
FILL_TAIL_BLOCKS = 4    # the spacing route reads this many fill blocks before the slice, to prove where it landed
ROUTES = ("auto", "locate", "spacing")

GMT_FLAGS = [
    (0x80000000, "EOF"), (0x40000000, "BOT"), (0x20000000, "EOT"),
    (0x10000000, "SM"), (0x08000000, "EOD"), (0x04000000, "WR_PROT"),
    (0x01000000, "ONLINE"), (0x00800000, "D_6250"), (0x00400000, "D_1600"),
    (0x00200000, "D_800"), (0x00040000, "DR_OPEN"), (0x00010000, "IM_REP_EN"),
    (0x00008000, "CLN"),
]


def errname(num):
    return errno.errorcode.get(num, "errno%s" % num)


def stamp(t=None):
    t = time.time() if t is None else t
    return time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime(t)) + ".%06dZ" % int((t % 1) * 1000000)


def fmt_mtget(d):
    """`mt status` as mt-st prints it, from MTIOCGET (mt cannot run while the writer holds the drive)."""
    if d is None or "error" in d:
        return "mtget unavailable (%s)" % (errname(d["error"]) if d else "no device")
    flags = " ".join(n for bit, n in GMT_FLAGS if d["gstat"] & bit) or "(none)"
    return ("File number=%d, block number=%d, partition=%d. Tape block size %d bytes. "
            "Density code 0x%x. Soft error count since last status=%d. "
            "General status bits on (%08x): %s"
            % (d["fileno"], d["blkno"], d["resid"], d["dsreg"] & 0xFFFFFF,
               (d["dsreg"] >> 24) & 0xFF, d["erreg"], d["gstat"] & 0xFFFFFFFF, flags))


# ------------------------------------------------------------------ devices --
class RealDev(object):
    """An open st node (or, in tests, any file: the ioctls then report ENOTTY)."""

    def __init__(self, path, flags):
        self.path = path
        self.fd = os.open(path, flags)
        self.pos_errno = None    # the errno of the last failed MTIOCPOS, None after one that answered

    def write(self, buf):
        return os.write(self.fd, buf)

    def read_into(self, buf):
        return os.readv(self.fd, [buf])

    def top(self, op, count):
        try:
            fcntl.ioctl(self.fd, MTIOCTOP, struct.pack(MTOP_FMT, op, count))
            return True, 0
        except OSError as e:
            return False, e.errno

    def mtget(self):
        try:
            raw = fcntl.ioctl(self.fd, MTIOCGET, bytes(struct.calcsize(MTGET_FMT)))
        except OSError as e:
            return {"error": e.errno}
        f = struct.unpack(MTGET_FMT, raw)
        return dict(zip(("type", "resid", "dsreg", "gstat", "erreg", "fileno", "blkno"), f))

    def mtpos(self):
        """READ POSITION through st: the block address, or None with the errno kept in self.pos_errno."""
        try:
            raw = fcntl.ioctl(self.fd, MTIOCPOS, bytes(struct.calcsize(MTPOS_FMT)))
        except OSError as e:
            self.pos_errno = e.errno
            return None
        self.pos_errno = None
        return struct.unpack(MTPOS_FMT, raw)[0]

    def close(self):
        try:
            os.close(self.fd)
            return True, 0
        except OSError as e:
            return False, e.errno


class Keystream(object):
    """The AES-CTR file; global block g of the stream is block (g mod nblk) of it."""

    def __init__(self, path, bs):
        self.fd = os.open(path, os.O_RDONLY)
        size = os.fstat(self.fd).st_size
        if size == 0 or size % bs:
            raise SystemExit("keystream %s is %d bytes: not a whole number of %d-byte blocks" % (path, size, bs))
        self.bs, self.nblk = bs, size // bs
        try:
            os.posix_fadvise(self.fd, 0, 0, os.POSIX_FADV_SEQUENTIAL)
        except (AttributeError, OSError):
            pass

    def read(self, g, buf):
        if os.preadv(self.fd, [buf], (g % self.nblk) * self.bs) != self.bs:
            raise SystemExit("short read of the keystream file")

    def block(self, g):
        b = bytearray(self.bs)
        self.read(g, b)
        return b


class Log(object):
    console = True

    def __init__(self, path):
        self.f = open(path, "a", 1) if path else None

    def file(self, msg):
        if self.f:
            self.f.write("[%s] %s\n" % (stamp(), msg))

    def say(self, msg):
        self.file(msg)
        if self.console:
            try:
                sys.stdout.write("[%s] %s\n" % (time.strftime("%H:%M:%S", time.gmtime()), msg))
                sys.stdout.flush()
            except OSError:      # the reader of our stdout went away (tee killed): the file log still has it
                self.console = False


def is_refusal(outcome):
    return outcome in ("ENOSPC", "zero")


def rle(seq):
    out = []
    for o in seq:
        if out and out[-1][0] == o:
            out[-1][1] += 1
        else:
            out.append([o, 1])
    return out


def rle_text(runs, limit=12, totals=None, nruns=None):
    """'ENOSPC x1 -> ok x2048'; a long pattern is cut to `limit` runs and followed by exact totals."""
    nruns = len(runs) if nruns is None else nruns
    parts = ["%s x%d" % (o, c) for o, c in runs[:limit]]
    if nruns <= limit:
        return " -> ".join(parts)
    if totals is None:
        totals = {}
        for o, c in runs:
            totals[o] = totals.get(o, 0) + c
    return (" -> ".join(parts) + " -> ... (%d runs in all; totals: %s)"
            % (nruns, ", ".join("%s x%d" % kv for kv in sorted(totals.items()))))


def signature(rec):
    """How long each outcome's write(2) took and which st flags followed it: a refusal that costs no I/O and flips
    the status bit is st's own bookkeeping after an EOM-flagged write, not the drive refusing a command."""
    t, f = rec.get("timing") or {}, rec.get("flags_after") or {}
    if not t:
        return ""
    return ("write(2) took (median, max): %s; st flags right after: %s"
            % ("; ".join("%s %.2f ms, %.2f ms (n=%d)" % (o, v["median_ms"], v["max_ms"], v["n"]) for o, v in sorted(t.items())),
               "; ".join("%s -> %s" % (o, "+".join(v)) for o, v in sorted(f.items())) or "n/a"))


def describe(rec):
    """The post-refusal outcome sequence of a write_blocks() record, in words."""
    runs, totals, nruns = rec.get("outcomes_rle") or [], rec.get("outcome_totals"), rec.get("outcomes_runs", 0)
    if nruns == 2 and is_refusal(runs[0][0]) and runs[1][0] == "ok":
        return "%d refusal%s (%s), then %d accepted" % (runs[0][1], "" if runs[0][1] == 1 else "s", runs[0][0], runs[1][1])
    if nruns >= 4 and all(c == 1 for _, c in runs) and all(
            is_refusal(runs[i][0]) != is_refusal(runs[i + 1][0]) for i in range(len(runs) - 1)):
        return ("ALTERNATING: every accepted write was followed by one refusal, then accepted again (%s)"
                % ", ".join("%s x%d" % kv for kv in sorted((totals or {}).items())))
    return rle_text(runs, 12, totals, nruns)


# ------------------------------------------------------------------- writer --
class Writer(object):
    def __init__(self, dev, ks, bs, run, o, log):
        self.dev, self.ks, self.bs, self.run, self.o, self.log = dev, ks, bs, run, o, log
        self.buf = bytearray(bs)
        self.buf_g = None
        self.stop_flag = False
        self.rc = 0
        self.res = {"version": 1, "block_size": bs, "src_blocks": ks.nblk,
                    "slice_target_blocks": o.slice_blocks, "refusal_run_limit": o.refusal_run_limit,
                    "stage": "start", "started": stamp()}
        self.save()

    def save(self):
        tmp = os.path.join(self.run, "result.json.tmp")
        with open(tmp, "w") as f:
            json.dump(self.res, f, indent=1, sort_keys=True)
        os.replace(tmp, os.path.join(self.run, "result.json"))

    def stage(self, name):
        self.res["stage"] = name
        self.save()

    # one write(2): returns (outcome, n, err, t, seconds the call took)
    def write_buf(self, buf):
        t = time.time()
        m0 = time.monotonic()
        try:
            n = self.dev.write(buf)
        except OSError as e:
            return errname(e.errno), None, e.errno, t, time.monotonic() - m0
        took = time.monotonic() - m0
        if n == self.bs:
            return "ok", n, None, t, took
        if n == 0:
            return "zero", 0, None, t, took
        return "short(%d)" % n, n, None, t, took

    def block_buf(self, g):
        if self.buf_g != g:
            self.ks.read(g, self.buf)
            self.buf_g = g
        return self.buf

    def mtpos(self):
        """READ POSITION. A failure is counted by errno in result.json's read_position, so a null position says why
        (#487: on home2's HP LTO-6 every call failed, and the record did not say with what)."""
        pos = self.dev.mtpos()
        rp = self.res.setdefault("read_position", {"calls": 0, "failed": 0, "errnos": {}})
        rp["calls"] += 1
        if pos is None:
            rp["failed"] += 1
            e = getattr(self.dev, "pos_errno", None)
            name = "unknown" if e is None else errname(e)
            rp["errnos"][name] = rp["errnos"].get(name, 0) + 1
        return pos

    # ----- step 0: block size, position
    def setup(self):
        ok, err = self.dev.top(MTSETBLK, self.bs)
        st = self.dev.mtget()
        tape = "error" not in st
        if tape and (st["dsreg"] & 0xFFFFFF) != self.bs:
            raise SystemExit("the drive is not in fixed %d-byte block mode (block size %d): refusing"
                             % (self.bs, st["dsreg"] & 0xFFFFFF))
        # tapectl's TapeStore::open turns the drive's hardware compression off before any write
        # (encrypted data is incompressible); so does this run, best effort, as it does.
        comp_ok, comp_err = self.dev.top(MTCOMPRESSION, 0) if tape else (None, None)
        lba0 = self.mtpos()
        lba0_errno = None if lba0 is not None else getattr(self.dev, "pos_errno", None)
        self.res["setup"] = {"tape": tape, "setblk_ok": ok, "setblk_errno": err, "mtget": st, "lba0": lba0,
                             "lba0_errno": lba0_errno,
                             "compression_off_ok": comp_ok, "compression_off_errno": comp_err}
        self.log.say("device open; %s; compression off: %s; position (READ POSITION) = %s"
                     % (fmt_mtget(st) if tape else "not a tape device (test mode)",
                        "n/a" if comp_ok is None else "ok" if comp_ok else errname(comp_err),
                        lba0 if lba0 is not None else "unavailable (%s)" % ("unknown" if lba0_errno is None else errname(lba0_errno))))

    # ----- step 1: the fill, one open, to the first refusal
    def fill(self):
        bs, dev, buf, ks = self.bs, self.dev, self.buf, self.ks
        cap = self.o.max_fill_bytes // bs
        g = 0
        t0 = time.time()
        mono0 = last = time.monotonic()
        last_g = 0
        self.log.say("fill: writing %d-byte blocks from BOT, one open, until the first refusal (cap %d blocks)" % (bs, cap))
        event = None
        end = None
        while True:
            if self.stop_flag:
                end = "interrupted"
                break
            if g >= cap:
                end = "cap"
                break
            if (g & 127) == 0:
                now = time.monotonic()
                if now - last >= self.o.progress_s:
                    self.log.say("fill: %.1f GB (%d blocks); %.1f MB/s average, %.1f MB/s over the last %.0f s"
                                 % (g * bs / 1e9, g, g * bs / (now - mono0) / 1e6,
                                    (g - last_g) * bs / (now - last) / 1e6, now - last))
                    last, last_g = now, g
            ks.read(g, buf)
            try:
                n = dev.write(buf)
            except OSError as e:
                event = (errname(e.errno), None, e.errno, time.time())
                end = "refusal" if e.errno == errno.ENOSPC else "error"
                break
            if n != bs:
                event = ("zero" if n == 0 else "short(%d)" % n, n, None, time.time())
                end = "refusal" if n == 0 else "error"
                break
            g += 1
        t1 = time.time()
        self.buf_g = None
        secs = max(t1 - t0, 1e-9)
        self.res["fill"] = {"blocks": g, "bytes": g * bs, "seconds": secs, "mb_per_s": g * bs / secs / 1e6,
                            "end": end, "start": stamp(t0), "stop": stamp(t1)}
        self.log.say("fill: stopped (%s) after %d blocks = %d bytes in %.0f s = %.1f MB/s%s"
                     % (end, g, g * bs, secs, g * bs / secs / 1e6, "; first event %s" % event[0] if event else ""))
        return end, g, event

    # ----- steps 2-4: past the warning
    def write_blocks(self, region, g0, nmax, consec0=0, refusals0=0, detail=True, first_outcomes=()):
        """Write nmax blocks of keystream block g0.. one write(2) each, retrying a refused block.
        Stops on a non-ENOSPC error, a short count, or o.refusal_run_limit consecutive refusals.
        detail=True logs every attempt (the slice); False logs refusals/errors and runs of 1000 accepts."""
        limit = self.o.refusal_run_limit
        outcomes = list(first_outcomes)
        accepted = 0
        refusals, consec, maxconsec = refusals0, consec0, consec0
        g = g0
        stop = "complete"
        seq = len(outcomes)
        prev_t = None
        trans = 0
        t_start = time.time()
        last_prog = time.monotonic()
        run = [0, 0]   # accepted-run length and its first block, for the condensed log
        took_by = {}   # outcome -> seconds each write(2) took (first 4096): a refusal that costs no I/O is st's own
        flags_after = {}   # outcome -> the st status flags seen right after it (EOT = st re-armed, EOD = it refused)

        def flush_run():
            if run[0]:
                self.log.file("%s: ok x%d (blocks %d..%d)" % (region, run[0], run[1], run[1] + run[0] - 1))
                run[0] = 0

        while accepted < nmax:
            if self.stop_flag:
                stop = "interrupted"
                break
            outcome, n, err, t, took = self.write_buf(self.block_buf(g))
            seq += 1
            outcomes.append(outcome)
            lst = took_by.setdefault(outcome, [])
            if len(lst) < 4096:
                lst.append(took)
            dt = 0.0 if prev_t is None else t - prev_t
            prev_t = t
            changed = len(outcomes) < 2 or outcomes[-2] != outcome
            if outcome == "ok":
                if detail:
                    self.log.file("%s attempt %d block %d (accepted %d): ok ret=%d took=%.6f dt=%.6f ts=%s"
                                  % (region, seq, g, accepted + 1, n, took, dt, stamp(t)))
                else:
                    if run[0] == 0:
                        run[1] = g
                    run[0] += 1
                    if run[0] >= 1000:
                        flush_run()
                accepted += 1
                g += 1
                consec = 0
            else:
                flush_run()
                what = (" errno=%d (%s)" % (err, os.strerror(err))) if err is not None else " ret=%s" % n
                self.log.file("%s attempt %d block %d: %s%s took=%.6f dt=%.6f ts=%s"
                              % (region, seq, g, outcome, what, took, dt, stamp(t)))
                if is_refusal(outcome):
                    refusals += 1
                    consec += 1
                    maxconsec = max(maxconsec, consec)
                    if consec >= limit:
                        stop = "refusal-run(%d)" % consec
                        break
                else:
                    stop = outcome if outcome.startswith("short") else "error:%s" % outcome
                    break
            if changed and trans < 40:
                trans += 1
                st = self.dev.mtget()
                self.log.file("%s st state after the outcome changed to %s: %s" % (region, outcome, fmt_mtget(st)))
                if "error" not in st:
                    names = set(n_ for bit, n_ in GMT_FLAGS if st["gstat"] & bit) & set(["BOT", "EOF", "EOT", "EOD", "SM"])
                    flags_after.setdefault(outcome, set()).update(names or ["-"])
            if not detail and time.monotonic() - last_prog >= 60:
                last_prog = time.monotonic()
                self.log.say("%s: %d blocks accepted so far (%.2f GB), %d refusals"
                             % (region, accepted, accepted * self.bs / 1e9, refusals))
        flush_run()
        runs = rle(outcomes)
        totals = {}
        for o, c in runs:
            totals[o] = totals.get(o, 0) + c
        timing = {}
        for o_, lst in took_by.items():
            lst = sorted(lst)
            timing[o_] = {"n": len(lst), "median_ms": lst[len(lst) // 2] * 1000.0, "max_ms": lst[-1] * 1000.0}
        return {"region": region, "first_block": g0, "accepted": accepted, "attempts": seq,
                "refusals": refusals, "max_consecutive_refusals": maxconsec, "stop": stop,
                "seconds": time.time() - t_start, "outcomes_rle": runs[:400], "outcomes_runs": len(runs),
                "outcome_totals": totals, "timing": timing,
                "flags_after": dict((k_, sorted(v_)) for k_, v_ in flags_after.items())}

    def filemark(self, label, op, opname):
        """tapectl ends a data file with an immediate filemark (MTWEOFI) and the seal marker's with a synchronous one (MTWEOF).
        A refused attempt is retried only while READ POSITION shows it left no mark behind."""
        lb = prev = self.mtpos()
        outs, positions = [], []
        ok = False
        for _ in range(self.o.refusal_run_limit):
            t = time.time()
            ok, err = self.dev.top(op, 1)
            outs.append("ok" if ok else errname(err))
            pos = self.mtpos()
            positions.append(pos)
            self.log.file("%s: %s -> %s position %s ts=%s" % (label, opname, outs[-1], pos, stamp(t)))
            if ok or err != errno.ENOSPC:
                break
            if prev is not None and pos is not None and pos != prev:
                self.log.say("%s: refused (%s) but READ POSITION moved %s -> %s: the mark is on the tape; not retrying"
                             % (label, outs[-1], prev, pos))
                break
            prev = pos if pos is not None else prev
        rec = {"ok": ok, "op": opname, "attempts": outs, "positions": positions, "lba_before": lb,
               "lba_after": self.mtpos(), "mtget": self.dev.mtget()}
        self.res[label] = rec
        self.log.say("%s (%s): %s (attempts: %s); position %s -> %s; %s"
                     % (label, opname, "accepted" if ok else "NOT accepted", ", ".join(outs), lb, rec["lba_after"],
                        fmt_mtget(rec["mtget"])))
        return ok

    def seal_buf(self, slice_rec):
        hdr = ("LTO6-FILL-SEAL-MARKER v1\nutc=%s\nfill_blocks=%d\nslice_blocks_accepted=%d\n"
               "slice_stop=%s\nfirst_slice_block=%d\nblock_size=%d\n"
               % (stamp(), self.res["fill"]["blocks"], slice_rec["accepted"], slice_rec["stop"],
                  slice_rec["first_block"], self.bs)).encode("ascii")
        b = bytearray(self.bs)
        b[:len(hdr)] = hdr
        return b

    def seal(self, slice_rec):
        buf = self.seal_buf(slice_rec)
        outs, positions = [], []
        ok = False
        lb = prev = self.mtpos()
        for _ in range(self.o.refusal_run_limit):
            outcome, n, err, t, took = self.write_buf(buf)
            outs.append(outcome)
            pos = self.mtpos()
            positions.append(pos)
            self.log.file("seal block: %s position %s ts=%s" % (outcome, pos, stamp(t)))
            if outcome == "ok":
                ok = True
                break
            if not is_refusal(outcome):
                break
            if prev is not None and pos is not None and pos != prev:
                self.log.say("seal block: refused (%s) but READ POSITION moved %s -> %s: the block is on the tape; not retrying"
                             % (outcome, prev, pos))
                break
            prev = pos if pos is not None else prev
        rec = {"ok": ok, "attempts": outs, "positions": positions, "lba_before": lb,
               "text": bytes(buf).split(b"\0", 1)[0].decode("ascii")}
        self.res["seal"] = rec
        self.log.say("seal block: %s (attempts: %s)" % ("accepted" if ok else "NOT accepted", ", ".join(outs)))
        return ok

    def snapshot(self, tag):
        """MAM and log pages 0x0c / 0x17 by sg, while this process holds the st node. Never page 0x2E."""
        sg = self.o.sg
        if not sg:
            return None
        t = time.time()
        r = self.run

        def sh(args, out):
            try:
                with open(out, "wb") as f:
                    return subprocess.run(args, stdout=f, stderr=subprocess.STDOUT, timeout=180).returncode
            except (OSError, subprocess.TimeoutExpired) as e:
                with open(out, "ab") as f:
                    f.write(("failed: %s\n" % e).encode())
                return -1

        sh(["sg_read_attr", sg], os.path.join(r, "%s.mam.txt" % tag))
        for page in ("0x0c", "0x17"):
            raw = os.path.join(r, "%s.page_%s.bin" % (tag, page))
            if sh(["sg_logs", "--page=%s" % page, "--maxlen=65532", "--raw", sg], raw) == 0:
                sh(["sg_logs", "--in=%s" % raw, "--raw", "--pdt=1"], os.path.join(r, "%s.page_%s.txt" % (tag, page)))
        secs = time.time() - t
        self.log.say("snapshot %s taken by sg (%.1f s with the stream paused)" % (tag, secs))
        return secs

    def past_warning(self, n_fill, event):
        o, res = self.o, self.res
        outcome, n, err, t = event
        st = self.dev.mtget()
        res["first_refusal"] = {"outcome": outcome, "errno": err, "ret": n, "time": stamp(t),
                                "fill_blocks": n_fill, "bytes_accepted": n_fill * self.bs,
                                "mtget": st, "mt_status": fmt_mtget(st),
                                "lba_expected": (res["setup"]["lba0"] or 0) + n_fill}
        self.log.say("FIRST REFUSAL: %s%s at block %d; %d bytes accepted before it; %s"
                     % (outcome, (" (errno %d: %s)" % (err, os.strerror(err))) if err else "", n_fill,
                        n_fill * self.bs, fmt_mtget(st)))
        self.log.file("slice attempt 1 block %d: %s (the refusal that ended the fill) ts=%s" % (n_fill, outcome, stamp(t)))
        if not is_refusal(outcome):
            self.log.say("not a refusal the past-warning phase can continue from; stopping here")
            return
        self.stage("first-refusal")
        secs = self.snapshot("at-warning")
        res["first_refusal"]["snapshot_seconds"] = secs
        self.stage("slice")
        sl = self.write_blocks("slice", n_fill, o.slice_blocks, consec0=1, refusals0=1, first_outcomes=[outcome])
        sl["lba_after"] = self.mtpos()
        sl["mtget"] = self.dev.mtget()
        res["slice"] = sl
        self.log.say("slice: %d of %d blocks accepted in %.1f s; %d refusals (longest run %d); stop: %s; outcomes: %s"
                     % (sl["accepted"], o.slice_blocks, sl["seconds"], sl["refusals"],
                        sl["max_consecutive_refusals"], sl["stop"], describe(sl)))
        self.log.say("st state after the slice: %s; position %s" % (fmt_mtget(sl["mtget"]), sl["lba_after"]))
        if sl["stop"] == "interrupted":
            return
        self.stage("filemark-seal-filemark")
        self.filemark("filemark1", MTWEOFI, "MTWEOFI")
        self.seal(sl)
        self.filemark("filemark2", MTWEOF, "MTWEOF")
        res["after_seal"] = {"mtget": self.dev.mtget(), "lba": self.mtpos()}
        self.log.say("after the seal: %s; position %s" % (fmt_mtget(res["after_seal"]["mtget"]), res["after_seal"]["lba"]))
        self.save()
        if o.to_hard_end:
            if sl["stop"] == "complete" and not self.stop_flag:
                self.hard_end(n_fill, sl)
            else:
                res["hard_end_skipped"] = "the slice did not complete (stop: %s)" % sl["stop"]
                self.log.say("hard end skipped: %s" % res["hard_end_skipped"])

    def hard_end(self, n_fill, sl):
        self.stage("hard-end")
        past = sl["accepted"] * self.bs
        cap_blocks = max(0, (self.o.hard_end_cap - past) // self.bs)
        lba0 = self.mtpos()
        self.log.say("hard end: writing on until a non-recoverable error or %d GB past the warning (%d more blocks)"
                     % (self.o.hard_end_cap // 10 ** 9, cap_blocks))
        he = self.write_blocks("hard", n_fill + sl["accepted"], cap_blocks, detail=False)
        he["lba_start"] = lba0
        he["lba_after"] = self.mtpos()
        he["mtget"] = self.dev.mtget()
        if he["stop"] == "complete":
            he["stop"] = "cap reached (no non-recoverable error before it)"
        he["bytes_past_first_refusal"] = (sl["accepted"] + he["accepted"]) * self.bs
        self.res["hard_end"] = he
        self.log.say("hard end: %d more blocks accepted; %.2f GB past the first refusal in all; stop: %s; %s"
                     % (he["accepted"], he["bytes_past_first_refusal"] / 1e9, he["stop"], fmt_mtget(he["mtget"])))

    def finish(self):
        st = self.dev.mtget()
        ok, err = self.dev.close()
        self.res["close"] = {"ok": ok, "errno": err, "mtget_before_close": st}
        self.log.say("device closed (%s); st state just before: %s" % ("ok" if ok else errname(err), fmt_mtget(st)))
        self.res["finished"] = stamp()
        self.save()

    def execute(self):
        res = self.res
        try:
            self.setup()
            self.stage("fill")
            end, n_fill, event = self.fill()
            if end == "refusal":
                if self.o.stop_at_warning:
                    st = self.dev.mtget()
                    res["first_refusal"] = {"outcome": event[0], "errno": event[2], "time": stamp(event[3]),
                                            "fill_blocks": n_fill, "bytes_accepted": n_fill * self.bs,
                                            "mtget": st, "mt_status": fmt_mtget(st)}
                    self.log.say("FIRST REFUSAL: %s; %d bytes accepted; %s (--stop-at-warning: stopping)"
                                 % (event[0], n_fill * self.bs, fmt_mtget(st)))
                    self.stage("stopped-at-warning")
                else:
                    self.past_warning(n_fill, event)
            elif end == "error":
                res["fill_error"] = {"outcome": event[0], "errno": event[2], "mtget": self.dev.mtget()}
                self.log.say("the fill stopped on %s, which is not a refusal: no past-warning phase" % event[0])
                self.rc = 5
            elif end == "cap":
                self.log.say("the fill reached its runaway cap (%d bytes) with NO refusal: the drive did not "
                             "report an early warning here (an emulator?)" % self.o.max_fill_bytes)
                self.rc = 4
            else:
                self.rc = 3
            if self.stop_flag:
                self.rc = 3
            if self.rc == 0 and res["stage"] not in ("stopped-at-warning",):
                self.stage("done")
        except Exception:
            # A bug that only real hardware reaches must not cost a 4.5 hour fill: keep what was measured,
            # close the device cleanly, and say so in the summary.
            tb = traceback.format_exc()
            res["exception"] = tb
            self.rc = 6
            self.log.say("UNEXPECTED ERROR in the writer; the device is closed and the partial results are kept:\n" + tb)
        finally:
            self.finish()
        return self.rc


# ----------------------------------------------------------------- readback --
def position_failure(res):
    """Why the positions in result.json are null: 'EIO on 14 of 14 calls, from the first, at BOT', or None."""
    rp = res.get("read_position") or {}
    if not rp.get("failed"):
        return None
    errs = rp.get("errnos") or {"unknown": rp["failed"]}
    what = " and ".join("%s x%d" % kv for kv in sorted(errs.items())) if len(errs) > 1 else list(errs)[0]
    lba0_errno = (res.get("setup") or {}).get("lba0_errno")
    at_bot = ""
    if lba0_errno is not None:
        at_bot = ", from the first, at BOT" if errname(lba0_errno) == what else ", the first at BOT with %s" % errname(lba0_errno)
    return "%s on %d of %d calls%s" % (what, rp["failed"], rp.get("calls", 0), at_bot)


class Reader(object):
    """Read back the post-warning slice, the filemark after it, the seal block and the filemark after that; never
    the whole tape. Two routes:
      locate   LOCATE (MTSEEK) to the block addresses READ POSITION recorded while the writer held the drive, and
               one filemark spacing (fsf1-from-last-slice-block);
      spacing  rewind, SPACE forward over the fill (MTFSR, in chunks SPACE(6)'s 24-bit count can take) to a few
               blocks before the first refusal, read those (fill-tail-blocks), and read on from there through the
               slice, the filemark, the seal block and the filemark: no READ POSITION, no LOCATE.
    auto takes locate, and spacing when the writer recorded no position (setup.lba0 null) or the LOCATE to the slice
    fails. A slice that LOCATE reached but that reads wrong is a finding, never a reason to change route."""

    def __init__(self, dev, ks, bs, log, route="auto", space_chunk=SPACE_CHUNK, tail=FILL_TAIL_BLOCKS):
        self.dev, self.ks, self.bs, self.log = dev, ks, bs, log
        self.checks = []
        self.buf = bytearray(bs)
        self.exp = bytearray(bs)
        if route not in ROUTES or not 0 < space_chunk <= SPACE_MAX:
            raise ValueError("route %r, space chunk %r" % (route, space_chunk))
        self.route_asked, self.space_chunk, self.tail = route, space_chunk, tail
        self.route, self.route_reason = None, None
        self.found_fm_first = False
        self.after_slice_as_written = False
        self.unavailable = "READ POSITION unavailable"

    def set_route(self, route, why):
        self.route, self.route_reason = route, why
        self.log.say("readback route: %s (%s)" % (route, why))

    def check(self, name, ok, detail):
        status = "PASS" if ok else ("SKIP" if ok is None else "FAIL")
        self.checks.append({"name": name, "status": status, "detail": detail})
        self.log.say("readback %-40s %s  %s" % (name, status, detail))

    def read(self):
        """('data'|'fm'|'short'|'err', errno or None)"""
        try:
            n = self.dev.read_into(self.buf)
        except OSError as e:
            return "err", e.errno
        if n == self.bs:
            return "data", None
        return ("fm", None) if n == 0 else ("short", None)

    @staticmethod
    def word(kind, err):
        return errname(err) if kind == "err" else kind

    def seek(self, lba):
        return self.dev.top(MTSEEK, lba)

    def matches(self, g):
        self.ks.read(g, self.exp)
        return self.buf == self.exp

    def run_checks(self, res):
        bs = self.bs
        self.dev.top(MTSETBLK, bs)
        st = self.dev.mtget()
        if "error" not in st and (st["dsreg"] & 0xFFFFFF) != bs:
            self.check("block-size", False, "drive block size %d != %d" % (st["dsreg"] & 0xFFFFFF, bs))
            return
        fill, sl = res.get("fill") or {}, res.get("slice")
        if not sl:
            self.check("post-warning-slice", None, "no past-warning slice was written (stage %s)" % res.get("stage"))
            return
        n, k = fill["blocks"], sl["accepted"]
        lba0 = (res.get("setup") or {}).get("lba0")
        start = (lba0 or 0) + n
        fm1, seal, fm2 = res.get("filemark1") or {}, res.get("seal") or {}, res.get("filemark2") or {}
        failure = position_failure(res)
        self.unavailable = "READ POSITION unavailable" + (": %s" % failure if failure else "")
        self.position_checks(start, k, sl, fm1, seal, fm2)
        if self.route_asked != "auto":
            self.set_route(self.route_asked, "--route %s" % self.route_asked)
        elif lba0 is None:
            self.set_route("spacing", "auto: the writer recorded no block address (setup.lba0 is null; %s)" % self.unavailable)
        else:
            self.set_route("locate", "auto: the writer recorded block addresses")
        if self.route == "locate":
            if k == 0:
                self.check("slice-blocks", None, "no slice block was accepted")
            else:
                ok, err = self.seek(start)
                if not ok and self.route_asked == "auto":
                    self.set_route("spacing", "auto: LOCATE to block address %d failed: %s" % (start, errname(err)))
                elif not ok:
                    self.check("slice-first-block", False, "LOCATE to block address %d failed: %s" % (start, errname(err)))
                    self.fsf_check(n, k, start, seal)
                else:
                    self.slice_checks(n, k, "block address %d" % start, fm1, seal, fm2, res, locate_start=start)
                    self.fsf_check(n, k, start, seal)
            if self.route == "locate":
                self.hard_end_checks(n + k, res.get("hard_end"))
        if self.route == "spacing":
            self.spacing_checks(n, k, fm1, seal, fm2, res)

    def spacing_checks(self, n, k, fm1, seal, fm2, res):
        """The spacing route: rewind, SPACE forward over the fill to T blocks before the first refusal, read those T
        blocks, then the slice and what follows it. Every position is a count from BOT; nothing asks the drive where
        it is and nothing LOCATEs."""
        t = min(self.tail, n)
        ok, err = self.dev.top(MTREW, 1)
        if not ok:
            self.check("fill-tail-blocks", False, "rewind failed: %s" % errname(err))
            return
        t0 = time.time()
        spaced, cmds, err = self.space_forward(n - t)
        if err is not None:
            self.check("fill-tail-blocks", False, "SPACE forward from file 0 block %d failed: %s; %s"
                       % (spaced, errname(err), fmt_mtget(self.dev.mtget())))
            return
        secs = time.time() - t0
        bad, stop = [], None
        for i in range(t):
            kind, err = self.read()
            if kind != "data":
                stop = (i, self.word(kind, err))
                break
            if not self.matches(n - t + i):
                bad.append(i)
        how = "rewind, then %d blocks spaced in %d SPACE command(s) of at most %d (%.1f s)" % (n - t, cmds, self.space_chunk, secs)
        if t == 0:
            self.check("fill-tail-blocks", None, "the fill wrote no block before the first refusal")
        elif stop:
            self.check("fill-tail-blocks", False, "%s; file 0 block %d: %s, not a fill block" % (how, n - t + stop[0], stop[1]))
            return
        else:
            self.check("fill-tail-blocks", not bad,
                       "%s; the next %d blocks, file 0 blocks %d..%d (the last before the first refusal), %s"
                       % (how, t, n - t, n - 1, "match keystream blocks %d..%d" % ((n - t) % self.ks.nblk, (n - 1) % self.ks.nblk)
                          if not bad else "DIFFER from the keystream at %s" % ", ".join("file 0 block %d" % (n - t + i) for i in bad)))
        if k == 0:
            self.check("slice-blocks", None, "no slice block was accepted")
        else:
            self.slice_checks(n, k, "file 0 block %d (spaced from BOT)" % n, fm1, seal, fm2, res)
        self.check("fsf1-from-last-slice-block", None,
                   "needs LOCATE; on the spacing route the filemark and the seal block were read in sequence after the "
                   "last slice block (objects-after-slice)")
        he = res.get("hard_end")
        if not he or not he.get("accepted"):
            return
        g0, last = n + k, he["accepted"] - 1
        if self.after_slice_as_written and fm2.get("ok"):
            kind, err = self.read()
            good = kind == "data" and self.matches(g0)
            self.check("hard-end-first-block", good, "file 2 block 0, read on after the second filemark: %s"
                       % self.compared(kind, err, good, g0))
            if kind != "data":
                self.check("hard-end-last-block", None, "file 2 block 0 did not read, so file 2 is not spaced through")
            elif last > 0:
                # the walk's blocks are keystream g0.. in order, so an extra or a missing block shows here too
                spaced, cmds, err = self.space_forward(last - 1)
                kind, err2 = self.read() if err is None else ("err", err)
                good = kind == "data" and self.matches(g0 + last)
                self.check("hard-end-last-block", good, "file 2 block %d, %d blocks spaced on from block 1 in %d SPACE "
                           "command(s): %s" % (last, spaced, cmds, self.compared(kind, err2, good, g0 + last)))
        else:
            self.check("hard-end-blocks", None, "the objects after the slice did not read as written, so where file 2 "
                       "starts is not known without READ POSITION")
        if None not in (he.get("lba_start"), he.get("lba_after")):
            self.hard_end_count_check(he)

    def space_forward(self, count):
        """MTFSR count blocks, in SPACE commands of at most self.space_chunk (SPACE(6)'s count is 24-bit signed and st
        passes it on unchecked). Returns (blocks spaced, commands, None) or, on a failure, the errno in place of None."""
        spaced, cmds = 0, 0
        while spaced < count:
            step = min(count - spaced, self.space_chunk)
            ok, err = self.dev.top(MTFSR, step)
            cmds += 1
            if not ok:
                return spaced, cmds, err
            spaced += step
        return spaced, cmds, None

    def compared(self, kind, err, good, g):
        if good:
            return "matches keystream block %d" % (g % self.ks.nblk)
        return "differs from keystream block %d" % (g % self.ks.nblk) if kind == "data" else self.word(kind, err)

    def position_checks(self, start, k, sl, fm1, seal, fm2):
        """The drive's own count (READ POSITION, taken while the writer held it) against what write(2) reported."""
        got = sl.get("lba_after")
        if got is None:
            self.check("physical-block-count", None, self.unavailable)
        else:
            self.check("physical-block-count", got - start == k,
                       "READ POSITION after the slice = %d; expected fill start %d + %d slice blocks accepted = %d"
                       % (got, start, k, start + k))
        if None not in (fm1.get("lba_before"), fm1.get("lba_after")):
            moved = fm1["lba_after"] - fm1["lba_before"]
            self.check("position-after-filemark1", moved == (1 if fm1.get("ok") else 0),
                       "%s reported %s; READ POSITION %s -> %s (moved %d)"
                       % (fm1.get("op", "MTWEOF"), "ok" if fm1.get("ok") else ",".join(fm1.get("attempts", [])),
                          fm1["lba_before"], fm1["lba_after"], moved))
        if None not in (fm1.get("lba_after"), fm2.get("lba_before"), fm2.get("lba_after")):
            m_seal, m_fm2 = fm2["lba_before"] - fm1["lba_after"], fm2["lba_after"] - fm2["lba_before"]
            self.check("position-after-seal-and-filemark2",
                       m_seal == (1 if seal.get("ok") else 0) and m_fm2 == (1 if fm2.get("ok") else 0),
                       "seal block reported %s, moved %d; %s reported %s, moved %d"
                       % ("ok" if seal.get("ok") else ",".join(seal.get("attempts", [])), m_seal, fm2.get("op", "MTWEOF"),
                          "ok" if fm2.get("ok") else ",".join(fm2.get("attempts", [])), m_fm2))

    def slice_checks(self, n, k, where, fm1, seal, fm2, res, locate_start=None):
        """Read the k slice blocks from where the tape stands, then what follows them, against what the writer was
        told it wrote. Positions nothing itself, except that on the LOCATE route (locate_start) a first block that
        differs is looked for at the neighbouring addresses; on the spacing route the first block gets the same
        keystream-shift diagnosis as the others."""
        t0 = time.time()
        bad, first_bad, stop_at, first_ok, last_ok, shift = 0, [], None, None, None, ""
        for i in range(k):
            kind, err = self.read()
            if kind != "data":
                stop_at = (i, self.word(kind, err))
                break
            same = self.matches(n + i)
            if i == 0:
                first_ok = same
            if i == k - 1:
                last_ok = same
            if not same:
                bad += 1
                if len(first_bad) < 5:
                    first_bad.append(i)
                if not shift and (i > 0 or locate_start is None):
                    got = bytes(self.buf)
                    for d, word in ((-1, "an extra block (a refused write kept its data?) sits before it"),
                                    (1, "a block is missing before it")):
                        if got == bytes(self.ks.block(n + i + d)):
                            shift = "slice block %d holds the keystream block %+d away: %s" % (i, d, word)
                            break
        secs = max(time.time() - t0, 1e-9)
        self.check("slice-first-block", first_ok,
                   "%s holds keystream block %d (global block %d)" % (where, n % self.ks.nblk, n)
                   if first_ok else "%s differs from keystream block %d%s"
                   % (where, n % self.ks.nblk, "" if (stop_at and stop_at[0] == 0) or locate_start is None
                      else self.neighbours(locate_start, n)))
        self.check("slice-last-block", last_ok,
                   "slice block %d == keystream" % (k - 1) if last_ok else
                   ("not read: %s at slice block %d" % (stop_at[1], stop_at[0]) if stop_at else "differs"))
        read_n = stop_at[0] if stop_at else k
        self.check("slice-all-blocks", bad == 0 and stop_at is None,
                   "%d of %d blocks read in %.1f s (%.0f MB/s), %d differ%s%s"
                   % (read_n, k, secs, read_n * self.bs / secs / 1e6, bad,
                      " (first at %s)" % first_bad if first_bad else "",
                      "; read stopped: %s" % stop_at[1] if stop_at else "") + ("; " + shift if shift else ""))
        if stop_at:
            return
        # what physically follows the slice, in order, against what the writer was told it wrote
        expect = (["FM"] if fm1.get("ok") else []) + (["SEAL"] if seal.get("ok") else []) + (["FM"] if fm2.get("ok") else [])
        found, seal_raw = [], None
        for _ in range(max(len(expect), 1)):
            kind, err = self.read()
            if kind == "fm":
                found.append("FM")
            elif kind == "data":
                raw = bytes(self.buf)
                head = raw.split(b"\0", 1)[0]
                if head.startswith(b"LTO6-FILL-SEAL-MARKER"):
                    found.append("SEAL")
                    seal_raw = (head, raw[len(head):])
                else:
                    found.append("DATA")
            else:
                found.append("EOD(%s)" % self.word(kind, err))
                break
        agree = found == expect if expect else not (set(found) & set(["FM", "SEAL"]))
        self.check("objects-after-slice", agree,
                   "reported written: %s; read back: %s" % (" ".join(expect) or "nothing", " ".join(found)))
        if seal.get("ok"):
            if seal_raw is None:
                self.check("seal-block-content", False, "no seal block was found to compare")
            else:
                head, pad = seal_raw
                self.check("seal-block-content", head.decode("ascii", "replace") == seal.get("text") and not pad.strip(b"\0"),
                           "header %d bytes as written, %d bytes of zero padding" % (len(head), len(pad)))
        if expect and found == expect and not res.get("hard_end"):
            kind, err = self.read()
            self.log.say("readback (information) the read after the final filemark returned %s; %s"
                         % (self.word(kind, err), fmt_mtget(self.dev.mtget())))
            self.checks.append({"name": "read-after-final-filemark", "status": "INFO", "detail": self.word(kind, err)})
        self.found_fm_first = bool(found) and found[0] == "FM"
        self.after_slice_as_written = bool(expect) and found == expect

    def fsf_check(self, n, k, start, seal):
        """LOCATE to the last slice block, then space ONE filemark: that must land on the seal block."""
        if not getattr(self, "found_fm_first", False) or not seal.get("ok"):
            self.check("fsf1-from-last-slice-block", None, "no filemark and seal block were read back after the slice")
            return
        ok, err = self.seek(start + k - 1)
        if not ok:
            self.check("fsf1-from-last-slice-block", False, "LOCATE to %d failed: %s" % (start + k - 1, errname(err)))
            return
        kind, err = self.read()
        last_same = kind == "data" and self.matches(n + k - 1)
        ok, err = self.dev.top(MTFSF, 1)
        kind2, err2 = self.read() if ok else ("err", err)
        head = bytes(self.buf).split(b"\0", 1)[0].decode("ascii", "replace") if kind2 == "data" else ""
        self.check("fsf1-from-last-slice-block", last_same and ok and head.startswith("LTO6-FILL-SEAL-MARKER"),
                   "LOCATE %d: last slice block %s; fsf 1: %s; the next read: %s"
                   % (start + k - 1, "matches" if last_same else "DIFFERS (%s)" % kind,
                      "ok" if ok else errname(err), "the seal block" if head.startswith("LTO6") else self.word(kind2, err2)))

    def neighbours(self, start, g):
        notes = []
        for d in (-2, -1, 1, 2):
            if self.seek(start + d)[0]:
                kind, _ = self.read()
                if kind == "data" and self.matches(g):
                    notes.append("the expected block is at address %+d" % d)
        return " (" + "; ".join(notes) + ")" if notes else ""

    def hard_end_checks(self, g0, he):
        if not he or not he.get("accepted"):
            return
        if he.get("lba_start") is None:
            self.check("hard-end-blocks", None, self.unavailable)
            return
        for label, idx in (("first", 0), ("last", he["accepted"] - 1)):
            ok, err = self.seek(he["lba_start"] + idx)
            kind, err2 = self.read() if ok else ("err", err)
            self.check("hard-end-%s-block" % label, kind == "data" and self.matches(g0 + idx),
                       "block address %d: %s" % (he["lba_start"] + idx, "matches keystream" if kind == "data" and self.matches(g0 + idx)
                                                 else self.word(kind, err2)))
        if he.get("lba_after") is not None:
            self.hard_end_count_check(he)

    def hard_end_count_check(self, he):
        """Record only: what READ POSITION said the walk moved, against the blocks write(2) accepted."""
        self.check("hard-end-physical-block-count", he["lba_after"] - he["lba_start"] == he["accepted"],
                   "READ POSITION moved %d for %d accepted blocks" % (he["lba_after"] - he["lba_start"], he["accepted"]))


# ------------------------------------------------------------------ summary --
def grab(path, key):
    """First integer after `key ... :` in a decoded sg page or MAM dump."""
    try:
        with open(path, errors="replace") as f:
            for line in f:
                if key in line and ":" in line:
                    digits = ""
                    for ch in line.split(":", 1)[1].strip():
                        if ch.isdigit():
                            digits += ch
                        elif digits:
                            break
                    return int(digits) if digits else None
    except OSError:
        pass
    return None


def commas(v):
    return "n/a" if v is None else "{:,}".format(v)


def native_tracks(used_mb, written_bytes):
    """Whether page 0x17's 'Total used native capacity' [MB] plausibly tracks the data: within a factor of 2 of the
    bytes written. On home2's HP LTO-6 it read 0 before and 103,077 MB after a 2.6 TB fill, and did not move across
    a 2 GiB write (docs/runs/2026-10-20-lto6-profile-and-lbp.md): there it is no measure of the tape used."""
    return used_mb is not None and bool(written_bytes) and 0.5 <= used_mb * 1e6 / written_bytes <= 2.0


def make_summary(run, prior, nominal=2.5e12):
    def load(name):
        try:
            with open(os.path.join(run, name)) as f:
                return json.load(f)
        except (OSError, ValueError):
            return {}

    res, rb = load("result.json"), load("readback.json")
    bs = res.get("block_size", BLOCK)
    fill, fr, sl = res.get("fill") or {}, res.get("first_refusal") or {}, res.get("slice")
    target = res.get("slice_target_blocks", SLICE_BLOCKS)
    out = []
    p = out.append
    p("== lto fill summary (issue #420: what st and the drive do after the first write refusal) ==")
    p("run directory: %s" % run)
    stage, end = res.get("stage"), fill.get("end")
    if end == "cap":
        word = "NO REFUSAL before the runaway cap: an emulator, or a drive that reported no early warning; nothing past the warning was measured"
    elif end == "error":
        word = "THE FILL STOPPED ON A WRITE ERROR THAT IS NOT A REFUSAL (%s): no past-warning phase" % (res.get("fill_error") or {}).get("outcome")
    elif end == "interrupted":
        word = "INTERRUPTED before any refusal"
    elif stage == "stopped-at-warning":
        word = "STOPPED AT THE FIRST REFUSAL (--stop-at-warning); nothing was written after it"
    elif sl and sl.get("stop") == "interrupted":
        word = "INTERRUPTED during the past-warning phase"
    elif sl and stage == "done":
        word = "MEASUREMENT COMPLETE (readback: %s)" % ("FAIL" if any(c["status"] == "FAIL" for c in rb.get("checks", []))
                                                       else "PASS" if rb.get("checks") else "not run")
    else:
        word = "INCOMPLETE (stage %s)" % stage
    if res.get("exception"):
        word = "THE WRITER HIT AN UNEXPECTED ERROR (rc 6; see below); " + word
    p("status: %s" % word)
    if res.get("exception"):
        p("exception: %s" % res["exception"].strip().splitlines()[-1])
    p("stage reached: %s (started %s, finished %s)" % (stage, res.get("started"), res.get("finished")))
    failure = position_failure(res)
    if failure:
        p("READ POSITION (MTIOCPOS) unavailable: %s; the null positions in result.json are null for that reason" % failure)
    p("")
    p("1. BEFORE THE FIRST REFUSAL")
    p("bytes accepted before the first refusal: %s (%s blocks of %d); fill ended: %s; host feed %.1f MB/s over %.2f h"
      % (commas(fill.get("bytes")), commas(fill.get("blocks")), bs, fill.get("end"), fill.get("mb_per_s", 0),
         fill.get("seconds", 0) / 3600.0))
    if fill.get("bytes") is not None and prior:
        p("the 2026-09-24 dd fill of this cartridge accepted %s bytes: difference %s bytes (%+.3f%%)"
          % (commas(prior), "{:+,}".format(fill["bytes"] - prior), (fill["bytes"] - prior) * 100.0 / prior))
    if fr:
        p("first refusal: %s%s at %s" % (fr.get("outcome"), " (errno %s)" % fr["errno"] if fr.get("errno") else "", fr.get("time")))
        p("st/drive state at the refusal (MTIOCGET, as `mt status` prints it): %s" % fr.get("mt_status"))
    b_max = grab(os.path.join(run, "before.mam.txt"), "Maximum capacity in partition")
    used = grab(os.path.join(run, "after-rewind.page_0x17.txt"), "Total used native capacity")
    written = (fill.get("bytes") or 0) + ((sl or {}).get("accepted", 0) + (res.get("hard_end") or {}).get("accepted", 0)
                                          + (1 if (res.get("seal") or {}).get("ok") else 0)) * bs
    if fill.get("bytes") and b_max:
        p("bytes accepted / MAM maximum capacity (%s MiB): %.4f; / the generation table's %s planning figure: %.4f"
          % (commas(b_max), fill["bytes"] / (b_max * 1048576.0), "{:,} B".format(int(nominal)), fill["bytes"] / float(nominal)))
    if native_tracks(used, written):
        p("native capacity used per data byte, all data blocks written (page 0x17 after rewind %s MB / %s bytes): %.4f"
          % (commas(used), commas(written), used * 1e6 / written))
    elif used is not None and written:
        p("page 0x17's 'Total used native capacity' does not track writes on this drive: %s MB after rewind against %s "
          "bytes written (%.4f per data byte, not within a factor of 2), so no native-capacity-per-byte figure is derived"
          % (commas(used), commas(written), used * 1e6 / written))
    p("")
    p("2. AFTER THE FIRST REFUSAL (what write(2) returned for each attempt, run-length)")
    if sl:
        p(describe(sl))
        if signature(sl):
            p(signature(sl))
        p("full slice of %d blocks accepted: %s (%d accepted; stop reason: %s; %d refusals, longest run %d; "
          "refusal-run limit %s)" % (target, "YES" if sl["accepted"] >= target else "NO", sl["accepted"], sl["stop"],
                                     sl["refusals"], sl["max_consecutive_refusals"], res.get("refusal_run_limit")))
        p("slice written in %.1f s = %.1f MB/s" % (sl["seconds"], sl["accepted"] * bs / max(sl["seconds"], 1e-9) / 1e6))
        for lab, key in (("filemark after the slice", "filemark1"), ("seal block", "seal"), ("filemark after the seal", "filemark2")):
            r = res.get(key)
            p("%s%s: %s (attempts: %s)" % (lab, " (%s)" % r["op"] if r and r.get("op") else "",
                                           "accepted" if r and r.get("ok") else "NOT accepted" if r else "not attempted",
                                           ", ".join(r.get("attempts", [])) if r else "-"))
        su = res.get("setup") or {}
        if su.get("compression_off_ok") is not None:
            p("hardware compression turned off before the fill, as tapectl does: %s (the 2026-09-24 fill ran in the drive's "
              "as-found state, which may differ)" % ("yes" if su["compression_off_ok"] else "NO, errno %s" % su.get("compression_off_errno")))
        f0 = (fr.get("mtget") or {}).get("fileno")
        fn = lambda d: (d or {}).get("fileno")   # noqa: E731
        p("file numbers (MTIOCGET): at the first refusal %s; after filemark 1 %s; after the seal block and filemark 2 %s; "
          "just before close %s. Expected %s / %s / %s / %s: the post-warning slice is the continuation of file %s "
          "(its blocks %s..%s), the seal block is file %s."
          % (f0, fn((res.get("filemark1") or {}).get("mtget")), fn((res.get("after_seal") or {}).get("mtget")),
             fn((res.get("close") or {}).get("mtget_before_close")),
             f0, None if f0 is None else f0 + 1, None if f0 is None else f0 + 2, None if f0 is None else f0 + 2,
             f0, commas(fill.get("blocks")), commas((fill.get("blocks") or 0) + sl["accepted"] - 1),
             None if f0 is None else f0 + 1))
        for lab, d in (("after the slice", sl.get("mtget")), ("after the seal", (res.get("after_seal") or {}).get("mtget"))):
            p("st state %s: %s" % (lab, fmt_mtget(d)))
        c = res.get("close")
        if c:
            p("close: %s" % ("ok" if c.get("ok") else errname(c.get("errno") or 0)))
        try:
            with open(os.path.join(run, "after-close.mt.txt"), errors="replace") as f:
                mtl = [l.strip() for l in f if "File number" in l or "General status" in l or l.strip().startswith(("BOT", "EOT", "EOD", "ONLINE"))]
            p("`mt status` after the writer closed (st wrote its own filemark on close if the last operation was a write): %s"
              % (" | ".join(mtl) or "unavailable"))
        except OSError:
            pass
    else:
        p("(no past-warning slice was written)")
    p("")
    p("3. HEADROOM PAST THE WARNING (the walk to the hard end)")
    he = res.get("hard_end")
    if he:
        p("accepted %s more blocks after the seal; %.3f GB past the first refusal in all (slice + walk); stop: %s"
          % (commas(he["accepted"]), he["bytes_past_first_refusal"] / 1e9, he["stop"]))
        p("refusals during the walk: %d (longest run %d); outcomes: %s"
          % (he["refusals"], he["max_consecutive_refusals"], describe(he)))
        if signature(he):
            p(signature(he))
        p("st state at the end: %s" % fmt_mtget(he.get("mtget")))
    elif res.get("hard_end_skipped"):
        p("requested but skipped: %s" % res["hard_end_skipped"])
    else:
        p("not run (--no-hard-end was given, or the run stopped at the warning)")
    p("")
    p("4. READBACK (the whole tape is never read)")
    if rb.get("route"):
        p("route: %s (%s); chosen by %s" % (rb["route"], {
            "locate": "LOCATE to the block addresses recorded while writing, and one filemark spacing",
            "spacing": "rewind, SPACE forward over the fill, read on; no READ POSITION, no LOCATE"}.get(rb["route"], "?"),
            rb.get("route_reason")))
    if rb.get("checks"):
        for c in rb["checks"]:
            p("%-40s %s  %s" % (c["name"], c["status"], c["detail"]))
        p("overall: %s" % ("FAIL" if any(c["status"] == "FAIL" for c in rb["checks"]) else "PASS"))
    else:
        p("no readback ran")
    p("")
    p("5. DRIVE COUNTERS")
    for key, label in (("Native capacity from BOP to EOD", "page 0x0c native BOP->EOD [MB]"),
                       ("Minimum native capacity from EW to EOP", "page 0x0c min native EW->EOP [MB]")):
        v = dict((t, grab(os.path.join(run, "%s.page_0x0c.txt" % t), key)) for t in ("before", "at-warning", "after"))
        extra = ""
        if v["at-warning"] is not None and v["after"] is not None:
            extra = "; added by the past-warning phase: %s MB" % commas(v["after"] - v["at-warning"])
        p("%s: before the fill %s, at the warning %s, after the past-warning phase %s%s"
          % (label, commas(v["before"]), commas(v["at-warning"]), commas(v["after"]), extra))
    mam = dict((t, grab(os.path.join(run, "%s.mam.txt" % t), "Remaining capacity in partition"))
               for t in ("before", "at-warning", "after-rewind"))
    p("MAM remaining [MiB]: before %s, at the warning %s, after rewind %s"
      % (commas(mam["before"]), commas(mam["at-warning"]), commas(mam["after-rewind"])))
    u = dict((t, grab(os.path.join(run, "%s.page_0x17.txt" % t), "Total used native capacity"))
             for t in ("before", "at-warning", "after-rewind"))
    p("page 0x17 total used native [MB]: before %s, at the warning %s (read mid-write: a partial counter), after rewind %s"
      % (commas(u["before"]), commas(u["at-warning"]), commas(u["after-rewind"])))
    if fr.get("snapshot_seconds") is not None:
        p("the stream was paused %.1f s at the warning for the at-warning reads (writer.log has the timestamps)"
          % fr["snapshot_seconds"])
    p("")
    p("detail in this directory: result.json, readback.json, writer.log (every attempt of the slice), fill.log,")
    p("dmesg-tail.txt (empty or 'not permitted' means: run `sudo dmesg -T | tail -400` and send that too), *.mt.txt, *.page_*.txt, *.mam.txt")
    return "\n".join(out) + "\n"


# ---------------------------------------------------------------- self-test --
class FakeTape(object):
    """An in-memory tape with a scripted drive. verdict(attempt_no, written_blocks) -> None (accept),
    'ENOSPC', 'EIO', ('short', n) or 'ENOSPC+data' (refuse but keep the data: the case the readback must catch)."""

    def __init__(self, bs, verdict, fm_verdict=None, no_position=False, no_locate=False):
        self.bs, self.verdict, self.fm_verdict = bs, verdict, fm_verdict or (lambda a: None)
        self.rec, self.pos, self.attempts, self.fm_attempts, self.closed = [], 0, 0, 0, False
        # home2's HP LTO-6 through st (#487): READ POSITION and LOCATE both fail with EIO
        self.no_position, self.no_locate, self.pos_errno = no_position, no_locate, None
        self.space_counts = []

    def _put(self, item):
        del self.rec[self.pos:]
        self.rec.append(item)
        self.pos += 1

    def write(self, buf):
        self.attempts += 1
        v = self.verdict(self.attempts, sum(1 for r in self.rec if r is not None))
        if v is None:
            self._put(bytes(buf))
            return len(buf)
        if v == "ENOSPC+data":
            self._put(bytes(buf))
            raise OSError(errno.ENOSPC, "No space left on device")
        if v == "DROP":                      # reported as written, never reaches the tape (mhvtl past its capacity)
            return len(buf)
        if isinstance(v, tuple):
            return v[1]
        raise OSError(getattr(errno, v), os.strerror(getattr(errno, v)))

    def top(self, op, count):
        if op in (MTWEOF, MTWEOFI):
            self.fm_attempts += 1
            v = self.fm_verdict(self.fm_attempts)
            if v:
                if v.endswith("+mark"):      # reported as failed, but the mark is on the tape
                    self._put(None)
                    v = v[:-5]
                return False, getattr(errno, v)
            self._put(None)
        elif op == MTSEEK:
            if self.no_locate:
                return False, errno.EIO
            self.pos = count
        elif op == MTFSF:
            while self.pos < len(self.rec) and self.rec[self.pos] is not None:
                self.pos += 1
            self.pos = min(self.pos + 1, len(self.rec))
        elif op == MTREW:
            self.pos = 0
        elif op == MTFSR:
            # SPACE(6) blocks: a 24-bit signed count; st stops after a filemark it meets and reports EIO, as at EOD
            assert 0 < count <= SPACE_MAX, "MTFSR count %d does not fit SPACE(6)" % count
            self.space_counts.append(count)
            for _ in range(count):
                if self.pos >= len(self.rec):
                    return False, errno.EIO
                self.pos += 1
                if self.rec[self.pos - 1] is None:
                    return False, errno.EIO
        return True, 0

    def read_into(self, buf):
        if self.pos >= len(self.rec):
            raise OSError(errno.EIO, "blank check")
        r = self.rec[self.pos]
        self.pos += 1
        if r is None:
            return 0
        buf[:] = r
        return len(r)

    def mtget(self):
        fm = sum(1 for r in self.rec[:self.pos] if r is None)
        return {"type": 1, "resid": 0, "dsreg": self.bs, "gstat": 0x01010000, "erreg": 0, "fileno": fm, "blkno": -1}

    def mtpos(self):
        if self.no_position:
            self.pos_errno = errno.EIO
            return None
        self.pos_errno = None
        return self.pos

    def close(self):
        self.closed = True
        return True, 0


def selftest(argv=None):
    import shutil
    import tempfile
    bs, nblk, slice_blocks, fill_n = 4096, 16, 32, 100
    root = tempfile.mkdtemp(prefix="lto6-fill-selftest-")
    failures = []

    def expect(cond, what):
        print("  %s %s" % ("ok  " if cond else "FAIL", what))
        if not cond:
            failures.append(what)

    Log.console = False
    src = os.path.join(root, "src.bin")
    with open(src, "wb") as f:
        f.write(os.urandom(bs * nblk))

    def scenario(name, verdict, fm_verdict=None, hard=False, cap_bytes=None, limit=8, stop_flag=False, slice_n=None, **kw):
        print("scenario: %s" % name)
        run = os.path.join(root, name.replace(" ", "_"))
        os.makedirs(run)
        tape = FakeTape(bs, verdict, fm_verdict, no_position=kw.get("no_position", False), no_locate=kw.get("no_locate", False))
        o = argparse.Namespace(slice_blocks=slice_n or slice_blocks, refusal_run_limit=limit, stop_at_warning=kw.get("stop_at_warning", False),
                               to_hard_end=hard, hard_end_cap=kw.get("hard_cap", 10 ** 9),
                               max_fill_bytes=cap_bytes or 10 ** 9, sg="", progress_s=1e9)
        log = Log(os.path.join(run, "writer.log"))
        w = Writer(tape, Keystream(src, bs), bs, run, o, log)
        w.stop_flag = stop_flag
        rc = w.execute()
        rd = Reader(tape, Keystream(src, bs), bs, log, route=kw.get("route", "auto"), space_chunk=kw.get("chunk", SPACE_CHUNK))
        with open(os.path.join(run, "result.json")) as f:
            res = json.load(f)
        rd.run_checks(res)
        with open(os.path.join(run, "readback.json"), "w") as f:
            json.dump({"route": rd.route, "route_reason": rd.route_reason, "checks": rd.checks}, f)
        if kw.get("used_mb") is not None:
            with open(os.path.join(run, "after-rewind.page_0x17.txt"), "w") as f:
                f.write("Volume statistics page\n  Total used native capacity [MB]: %d\n" % kw["used_mb"])
        text = make_summary(run, None)
        return rc, res, rd, text, tape

    def status(rd, name):
        return [c["status"] for c in rd.checks if c["name"] == name]

    # A: the textbook st pattern -- one ENOSPC, then everything accepted
    rc, res, rd, text, tape = scenario("refuse once then accept", lambda a, w: "ENOSPC" if a == fill_n + 1 else None)
    expect(rc == 0 and res["fill"]["blocks"] == fill_n, "fill stops at the first ENOSPC after %d blocks" % fill_n)
    expect(res["slice"]["accepted"] == slice_blocks and res["slice"]["refusals"] == 1 and res["slice"]["stop"] == "complete",
           "a full slice, one refusal")
    expect(res["slice"]["outcomes_rle"] == [["ENOSPC", 1], ["ok", slice_blocks]], "outcomes: ENOSPC x1 then ok x%d" % slice_blocks)
    expect(res["filemark1"]["ok"] and res["seal"]["ok"] and res["filemark2"]["ok"], "filemark, seal block, filemark accepted")
    expect(not any(c["status"] == "FAIL" for c in rd.checks) and len(rd.checks) >= 8, "readback: no FAIL in %d checks" % len(rd.checks))
    expect("1 refusal (ENOSPC), then %d accepted" % slice_blocks in text, "summary line 'N refusals, then M accepted'")
    expect([type(r) for r in tape.rec[fill_n:fill_n + slice_blocks]] == [bytes] * slice_blocks and tape.rec[-1] is None
           and tape.rec[-3] is None and len(tape.rec) == fill_n + slice_blocks + 3, "tape layout: fill+slice, FM, seal, FM")

    # B: a sticky refusal -- stops after the run limit, still tries the filemark and seal
    rc, res, rd, text, tape = scenario("sticky refusal", lambda a, w: "ENOSPC" if a > fill_n else None)
    expect(res["slice"]["accepted"] == 0 and res["slice"]["refusals"] == 8 and res["slice"]["stop"].startswith("refusal-run"),
           "8 consecutive refusals stop the slice")
    expect(res["filemark1"]["ok"] and not res["seal"]["ok"] and res["seal"]["attempts"] == ["ENOSPC"] * 8, "the seal block is refused 8 times and logged")
    expect(status(rd, "slice-blocks") == ["SKIP"], "readback skips a slice that was never written")

    # C: alternation -- refuse every second attempt after the first refusal: never 8 in a row
    rc, res, rd, text, tape = scenario("alternating", lambda a, w: "ENOSPC" if a > fill_n and (a - fill_n) % 2 == 1 else None)
    expect(res["slice"]["accepted"] == slice_blocks and res["slice"]["max_consecutive_refusals"] == 1
           and res["slice"]["refusals"] == slice_blocks, "alternating refusals do not trip the run limit")
    expect(not any(c["status"] == "FAIL" for c in rd.checks), "readback clean after alternation")
    expect("ALTERNATING" in text and "ENOSPC x%d, ok x%d" % (slice_blocks, slice_blocks) in text, "summary names the alternation and gives totals")

    # C2: an alternation longer than the 400 runs the result file keeps: the totals stay exact
    rc, res, rd, text, tape = scenario("long alternation", lambda a, w: "ENOSPC" if a > fill_n and (a - fill_n) % 2 == 1 else None, slice_n=300)
    expect(res["slice"]["outcomes_runs"] == 600 and len(res["slice"]["outcomes_rle"]) == 400
           and res["slice"]["outcome_totals"] == {"ENOSPC": 300, "ok": 300}, "600 runs: 400 kept, exact totals kept")
    expect("ENOSPC x300, ok x300" in text, "the summary prints the exact totals")

    # D: a hard error after a few accepted blocks
    rc, res, rd, text, tape = scenario("eio", lambda a, w: "ENOSPC" if a == fill_n + 1 else ("EIO" if a == fill_n + 6 else None))
    expect(res["slice"]["accepted"] == 4 and res["slice"]["stop"] == "error:EIO", "EIO stops the slice at 4 accepted blocks")
    expect(not any(c["status"] == "FAIL" for c in rd.checks), "readback of a short slice is clean")

    # E: a short count
    rc, res, rd, text, tape = scenario("short count", lambda a, w: "ENOSPC" if a == fill_n + 1 else (("short", 100) if a == fill_n + 3 else None))
    expect(res["slice"]["stop"] == "short(100)" and res["slice"]["accepted"] == 1, "a short count is its own outcome and stops")

    # F: the walk to the hard end
    rc, res, rd, text, tape = scenario("hard end", lambda a, w: "ENOSPC" if a == fill_n + 1 else ("EIO" if w >= fill_n + slice_blocks + 11 else None),
                                       hard=True)
    expect(res["hard_end"]["accepted"] == 10 and res["hard_end"]["stop"] == "error:EIO", "the walk stops on EIO after 10 blocks")
    expect(not any(c["status"] == "FAIL" for c in rd.checks) and status(rd, "hard-end-last-block") == ["PASS"], "hard-end blocks read back")

    # G: the walk reaches its cap without a refusal
    rc, res, rd, text, tape = scenario("hard cap", lambda a, w: "ENOSPC" if a == fill_n + 1 else None, hard=True,
                                       hard_cap=(slice_blocks + 7) * bs)
    expect(res["hard_end"]["accepted"] == 7 and res["hard_end"]["stop"].startswith("cap reached"), "the walk stops at its cap")

    # H: no refusal at all (an emulator): the runaway cap ends the fill, rc 4, no past-warning phase
    rc, res, rd, text, tape = scenario("no refusal", lambda a, w: None, cap_bytes=50 * bs)
    expect(rc == 4 and res["fill"]["end"] == "cap" and "slice" not in res, "no refusal: the runaway cap ends the fill (rc 4)")

    # I: a refused write that DID reach the tape -- the readback must say so
    rc, res, rd, text, tape = scenario("refusal keeps data", lambda a, w: "ENOSPC+data" if a == fill_n + 1 else None)
    expect("FAIL" in status(rd, "physical-block-count") and "FAIL" in status(rd, "slice-all-blocks"),
           "a refused-but-written block is caught by the block count and the data compare")
    expect("an extra block" in text, "and the diagnosis names the duplicate")

    # J: --stop-at-warning
    rc, res, rd, text, tape = scenario("stop at warning", lambda a, w: "ENOSPC" if a == fill_n + 1 else None, stop_at_warning=True)
    expect(res["stage"] == "stopped-at-warning" and "slice" not in res and len(tape.rec) == fill_n, "--stop-at-warning writes nothing after the refusal")

    # K: interrupt flag
    rc, res, rd, text, tape = scenario("interrupted", lambda a, w: None, stop_flag=True)
    expect(rc == 3, "an interrupt ends the fill (rc 3)")

    # L: a filemark refused once, then accepted
    rc, res, rd, text, tape = scenario("filemark retried", lambda a, w: "ENOSPC" if a == fill_n + 1 else None,
                                       fm_verdict=lambda a: "ENOSPC" if a == 1 else None)
    expect(res["filemark1"]["attempts"] == ["ENOSPC", "ok"] and res["filemark1"]["ok"], "a refused filemark is retried and logged")

    # P: a refused filemark that left a mark behind is not retried (marks would stack)
    rc, res, rd, text, tape = scenario("refused filemark left a mark", lambda a, w: "ENOSPC" if a == fill_n + 1 else None,
                                       fm_verdict=lambda a: "ENOSPC+mark" if a == 1 else None)
    expect(res["filemark1"]["attempts"] == ["ENOSPC"] and not res["filemark1"]["ok"],
           "a refused filemark whose READ POSITION moved is not retried")
    expect("FAIL" in status(rd, "objects-after-slice") or "PASS" in status(rd, "objects-after-slice"), "and the readback still ran")

    # Q: a refused seal block that reached the tape is not retried either
    rc, res, rd, text, tape = scenario("refused seal kept", lambda a, w: "ENOSPC" if a == fill_n + 1 else ("ENOSPC+data" if a == fill_n + slice_blocks + 2 else None))
    expect(res["seal"]["attempts"] == ["ENOSPC"] and not res["seal"]["ok"], "a refused seal block whose READ POSITION moved is not retried")

    # R: timing and flags are recorded per outcome
    rc, res, rd, text, tape = scenario("signature", lambda a, w: "ENOSPC" if a > fill_n and (a - fill_n) % 2 == 1 else None)
    expect(set(res["slice"]["timing"]) == set(["ENOSPC", "ok"]) and "write(2) took" in text, "the summary carries a per-outcome timing line")

    # O: a bug that only hardware reaches (an unexpected exception mid-slice): rc 6, device closed, results kept
    def boom(a, w):
        if a == fill_n + 5:
            raise ValueError("injected")
        return "ENOSPC" if a == fill_n + 1 else None
    rc, res, rd, text, tape = scenario("unexpected exception", boom)
    expect(rc == 6 and tape.closed and "injected" in res.get("exception", "") and "UNEXPECTED ERROR" in text,
           "an unexpected exception ends in rc 6 with the device closed and the summary saying so")

    # M: MTWEOF reports EIO but the mark is on the tape -- the readback must disagree with the report
    rc, res, rd, text, tape = scenario("filemark reported failed but written", lambda a, w: "ENOSPC" if a == fill_n + 1 else None,
                                       fm_verdict=lambda a: "EIO+mark" if a == 1 else None)
    expect(res["filemark1"]["ok"] is False and "FAIL" in status(rd, "objects-after-slice"),
           "a filemark reported failed but present is caught by the object-sequence check")

    # N: the seal block is reported accepted but dropped by the drive (mhvtl past its capacity)
    rc, res, rd, text, tape = scenario("seal dropped", lambda a, w: "ENOSPC" if a == fill_n + 1 else ("DROP" if a == fill_n + slice_blocks + 2 else None))
    expect(res["seal"]["ok"] and "FAIL" in status(rd, "objects-after-slice") and "FAIL" in status(rd, "position-after-seal-and-filemark2"),
           "a dropped seal block is caught by the object sequence and by READ POSITION")

    # The readback routes (#487). Every scenario above ran `auto` with positions recorded: the LOCATE route.
    once = lambda a, w: "ENOSPC" if a == fill_n + 1 else None   # noqa: E731
    rc, res, rd, text, tape = scenario("route locate by default", once)
    expect(rd.route == "locate" and not tape.space_counts and status(rd, "fill-tail-blocks") == []
           and "route: locate" in text, "auto with recorded positions reads by LOCATE and never spaces by blocks")

    # S1: the spacing route on the textbook layout, in SPACE chunks of 7 blocks: every check passes
    rc, res, rd, text, tape = scenario("route spacing", once, route="spacing", chunk=7)
    expect(rd.route == "spacing" and not any(c["status"] == "FAIL" for c in rd.checks)
           and status(rd, "fill-tail-blocks") == ["PASS"] and status(rd, "slice-all-blocks") == ["PASS"]
           and status(rd, "objects-after-slice") == ["PASS"] and status(rd, "seal-block-content") == ["PASS"],
           "spacing: fill tail, slice, filemark, seal block and filemark all read back")
    expect(tape.space_counts == [7] * 13 + [5] and sum(tape.space_counts) == fill_n - FILL_TAIL_BLOCKS,
           "spacing: %d blocks spaced in SPACE commands no larger than the chunk" % (fill_n - FILL_TAIL_BLOCKS))
    expect(status(rd, "fsf1-from-last-slice-block") == ["SKIP"] and "route: spacing" in text, "spacing: the summary names the route")

    # S2: home2's drive (#487): READ POSITION fails on every call and LOCATE fails; auto spaces, the errno is kept
    rc, res, rd, text, tape = scenario("home2 no position no locate", once, no_position=True, no_locate=True)
    rp = res.get("read_position") or {}
    expect(res["setup"]["lba0"] is None and res["setup"]["lba0_errno"] == errno.EIO and rp.get("failed") == rp.get("calls")
           and rp.get("calls", 0) > 3 and rp.get("errnos") == {"EIO": rp["calls"]}, "every failed READ POSITION is counted by errno")
    expect(rd.route == "spacing" and "setup.lba0 is null" in rd.route_reason and not any(c["status"] == "FAIL" for c in rd.checks)
           and status(rd, "slice-all-blocks") == ["PASS"], "auto with no recorded position takes the spacing route, and it passes")
    expect(text.count("READ POSITION (MTIOCPOS) unavailable: EIO on %d of %d calls, from the first, at BOT" % (rp["calls"], rp["calls"])) == 1,
           "the summary says once why every position is null")
    expect("READ POSITION unavailable: EIO" in [c for c in rd.checks if c["name"] == "physical-block-count"][0]["detail"],
           "and so does the skipped block-count check")

    # S3: positions recorded, but LOCATE fails: auto falls back to spacing and says why
    rc, res, rd, text, tape = scenario("locate fails", once, no_locate=True)
    expect(rd.route == "spacing" and "LOCATE to block address %d failed: EIO" % fill_n in rd.route_reason
           and not any(c["status"] == "FAIL" for c in rd.checks) and status(rd, "physical-block-count") == ["PASS"],
           "auto falls back to spacing when the LOCATE to the slice fails")

    # S4: --route locate is not rescued: a failed LOCATE is a FAIL, as before
    rc, res, rd, text, tape = scenario("locate asked and fails", once, no_locate=True, route="locate")
    expect(rd.route == "locate" and status(rd, "slice-first-block") == ["FAIL"] and not tape.space_counts,
           "--route locate reports a failed LOCATE and does not space")

    # S5: a refused write that reached the tape, on the spacing route: the duplicate is still named
    rc, res, rd, text, tape = scenario("refusal keeps data spacing", lambda a, w: "ENOSPC+data" if a == fill_n + 1 else None,
                                       route="spacing")
    expect(status(rd, "fill-tail-blocks") == ["PASS"] and "FAIL" in status(rd, "slice-all-blocks") and "an extra block" in text,
           "spacing: a refused-but-written block is caught and diagnosed")

    # S6: the walk to the hard end, on the spacing route: file 2's first block is the walk's first keystream block
    rc, res, rd, text, tape = scenario("hard end spacing", lambda a, w: "ENOSPC" if a == fill_n + 1 else ("EIO" if w >= fill_n + slice_blocks + 11 else None),
                                       hard=True, no_position=True, no_locate=True)
    expect(rd.route == "spacing" and status(rd, "hard-end-first-block") == ["PASS"] and status(rd, "hard-end-last-block") == ["PASS"]
           and not any(c["status"] == "FAIL" for c in rd.checks),
           "spacing: the walk's first block reads back after the second filemark, and its last block after spacing on")

    # S6b: a refused walk write that reached the tape: spacing through file 2 lands one block short of the last
    rc, res, rd, text, tape = scenario("hard end duplicate spacing", lambda a, w: "ENOSPC" if a == fill_n + 1 else
                                       ("ENOSPC+data" if a == fill_n + slice_blocks + 5 else None),
                                       hard=True, hard_cap=(slice_blocks + 7) * bs, route="spacing")
    expect(res["hard_end"]["accepted"] == 7 and status(rd, "hard-end-first-block") == ["PASS"]
           and status(rd, "hard-end-last-block") == ["FAIL"], "spacing: a duplicated walk block fails the walk's last-block check")

    # S7: a seal block dropped by the drive, on the spacing route
    rc, res, rd, text, tape = scenario("seal dropped spacing", lambda a, w: "ENOSPC" if a == fill_n + 1 else ("DROP" if a == fill_n + slice_blocks + 2 else None),
                                       route="spacing")
    expect("FAIL" in status(rd, "objects-after-slice"), "spacing: a dropped seal block is caught by the object sequence")

    # The native-capacity line only when page 0x17 tracks the data (within a factor of 2), else both figures
    rc, res, rd, text, tape = scenario("native tracks", once, used_mb=1)
    expect("native capacity used per data byte" in text and "does not track" not in text, "a plausible page 0x17 figure is used")
    rc, res, rd, text, tape = scenario("native does not track", once, used_mb=103077)
    expect("native capacity used per data byte" not in text and "does not track writes on this drive: 103,077 MB" in text,
           "a page 0x17 figure far from the bytes written is named, not used")

    Log.console = True
    shutil.rmtree(root)
    print("\n%s" % ("selftest: all passed" if not failures else "selftest: %d FAILED: %s" % (len(failures), failures)))
    return 1 if failures else 0


# --------------------------------------------------------------------- CLI ---
def install_signals(w):
    def handler(signum, frame):
        w.stop_flag = True
        w.log.say("signal %d: stopping after the current write" % signum)
    for sig in (signal.SIGINT, signal.SIGTERM):
        signal.signal(sig, handler)
    signal.signal(signal.SIGHUP, signal.SIG_IGN)     # a dropped terminal must not end a multi-hour fill
    if hasattr(signal, "SIGXFSZ"):
        signal.signal(signal.SIGXFSZ, signal.SIG_IGN)


def cmd_write(a):
    os.makedirs(a.run, exist_ok=True)
    log = Log(os.path.join(a.run, "writer.log"))
    ks = Keystream(a.src, a.block_size)
    dev = RealDev(a.device, os.O_WRONLY)
    w = Writer(dev, ks, a.block_size, a.run, a, log)
    install_signals(w)
    return w.execute()


def cmd_verify(a):
    log = Log(os.path.join(a.run, "writer.log"))
    try:
        with open(os.path.join(a.run, "result.json")) as f:
            res = json.load(f)
    except (OSError, ValueError) as e:
        print("verify: no usable result.json in %s: %s" % (a.run, e))
        return 2
    ks = Keystream(a.src, a.block_size)
    dev = RealDev(a.device, os.O_RDONLY)
    r = Reader(dev, ks, a.block_size, log, route=a.route, space_chunk=a.space_chunk)
    try:
        r.run_checks(res)
    except Exception:
        r.check("readback-exception", False, "the readback itself failed: " + traceback.format_exc().strip().splitlines()[-1])
        log.say(traceback.format_exc())
    finally:
        dev.close()
    with open(os.path.join(a.run, "readback.json"), "w") as f:
        json.dump({"route": r.route, "route_requested": a.route, "route_reason": r.route_reason, "checks": r.checks}, f, indent=1)
    return 1 if any(c["status"] == "FAIL" for c in r.checks) else 0


def cmd_summary(a):
    try:
        text = make_summary(a.run, a.reference_bytes, a.nominal_bytes)
    except Exception:
        text = "THE SUMMARY ITSELF FAILED; send the whole directory (result.json has the numbers):\n" + traceback.format_exc()
    with open(os.path.join(a.run, "summary.txt"), "w") as f:
        f.write(text)
    sys.stdout.write(text)
    return 0


def parser():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    sub = ap.add_subparsers(dest="cmd")
    for name in ("write", "verify"):
        p = sub.add_parser(name)
        p.add_argument("--device", required=True)
        p.add_argument("--src", required=True)
        p.add_argument("--run", required=True)
        p.add_argument("--block-size", type=int, default=BLOCK)
        if name == "verify":
            p.add_argument("--route", choices=ROUTES, default="auto",
                           help="locate: LOCATE to the recorded block addresses; spacing: rewind and SPACE forward, no "
                                "READ POSITION or LOCATE; auto (default): locate, else spacing when no position was "
                                "recorded or the LOCATE fails")
            p.add_argument("--space-chunk", type=int, default=SPACE_CHUNK,
                           help="blocks per SPACE command on the spacing route (default %%(default)s; SPACE(6) takes "
                                "at most %d)" % SPACE_MAX)
        if name == "write":
            p.add_argument("--slice-blocks", type=int, default=SLICE_BLOCKS)
            p.add_argument("--refusal-run-limit", type=int, default=REFUSAL_RUN_LIMIT)
            p.add_argument("--stop-at-warning", action="store_true")
            p.add_argument("--to-hard-end", action="store_true")
            p.add_argument("--hard-end-cap", type=int, default=HARD_END_CAP)
            p.add_argument("--max-fill-bytes", type=int, default=3000000000000)
            p.add_argument("--sg", default="")
            p.add_argument("--progress-s", type=float, default=300.0)
    p = sub.add_parser("summary")
    p.add_argument("--run", required=True)
    p.add_argument("--reference-bytes", type=int, default=PRIOR_FILL_BYTES,
                   help="a prior fill of this cartridge to compare with; 0 for none")
    p.add_argument("--nominal-bytes", type=int, default=2500000000000,
                   help="the generation table's marketed capacity of the medium (default: LTO-6's)")
    sub.add_parser("selftest")
    return ap


def main(argv):
    a = parser().parse_args(argv)
    if struct.calcsize(MTGET_FMT) != 48 or struct.calcsize(MTOP_FMT) != 8:
        sys.exit("lto6-fill-writer: unexpected struct sizes: this helper is written for LP64 Linux")
    if a.cmd == "write":
        return cmd_write(a)
    if a.cmd == "verify":
        if not 0 < a.space_chunk <= SPACE_MAX:
            sys.exit("lto6-fill-writer: --space-chunk must be 1..%d (SPACE(6)'s count is 24-bit signed)" % SPACE_MAX)
        return cmd_verify(a)
    if a.cmd == "summary":
        return cmd_summary(a)
    if a.cmd == "selftest":
        return selftest()
    parser().print_help()
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
