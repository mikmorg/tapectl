#!/usr/bin/env python3
"""check-docs.py — every `tapectl …` example in the user-facing docs must be a
command the binary accepts.

For each fenced code block in the checked Markdown files, every line that
invokes tapectl (bare, `tapectl-op`, or `sudo -u tapectl -H tapectl`) is
tokenised; the subcommand path is walked with `tapectl <path> --help`, and every
long flag (`--name`) must appear in the help of the command it is given to (or
in the global options). It also flags `--device /dev/nstN` (numbering is not
stable; examples use the by-id path) and fenced config blocks the binary would
refuse (`config check` on a throwaway home).

Usage:
    scripts/check-docs.py [--tapectl PATH] [FILE.md ...]
    scripts/check-docs.py --self-test        # the positive control

Exit 0 = clean, 1 = findings (each printed as file:line: message).

Dated records (docs/runs, docs/audits, docs/research, docs/adr, docs/design,
session journals, the handoff) are history, not instructions, and are not
checked.
"""
import glob
import os
import re
import shlex
import subprocess
import sys
import tempfile

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DEFAULT_FILES = ["README.md"] + sorted(
    f for f in glob.glob(os.path.join(REPO, "docs", "*.md"))
    if not re.search(r"(journal|handoff|decisions-pending|design-errata|perf-baselines)", f)
) + sorted(glob.glob(os.path.join(REPO, "docs", "cli", "*.md")))

FENCE = re.compile(r"^(\s*)(```|~~~)\s*([\w+-]*)")


def find_bin(explicit):
    if explicit:
        return explicit
    for c in [os.environ.get("CARGO_TARGET_DIR", ""), os.path.join(REPO, "target")]:
        for prof in ("debug", "release"):
            p = os.path.join(c, prof, "tapectl") if c else ""
            if p and os.access(p, os.X_OK):
                return p
    sys.exit("check-docs: no tapectl binary found — cargo build, or pass --tapectl")


class Help:
    def __init__(self, binary):
        self.bin = binary
        self.cache = {}

    def get(self, path):
        key = tuple(path)
        if key not in self.cache:
            r = subprocess.run([self.bin, *path, "--help"], capture_output=True, text=True)
            self.cache[key] = (r.returncode, r.stdout + r.stderr)
        return self.cache[key]

    def subcommands(self, path):
        rc, out = self.get(path)
        if rc != 0:
            return None
        subs, on = set(), False
        for line in out.splitlines():
            if line.startswith("Commands:"):
                on = True
                continue
            if on:
                m = re.match(r"^  (\S+)", line)
                if m:
                    subs.add(m.group(1))
                elif line.strip() == "" or not line.startswith(" "):
                    if subs:
                        on = False
        return subs

    def flags(self, path):
        rc, out = self.get(path)
        return set(re.findall(r"(--[a-z0-9][a-z0-9-]*)", out)) if rc == 0 else set()


def code_blocks(text):
    """Yield (first_line_no, lang, [lines]) for each fenced block."""
    lines = text.splitlines()
    i = 0
    while i < len(lines):
        m = FENCE.match(lines[i])
        if not m:
            i += 1
            continue
        fence, lang, start = m.group(2), m.group(3).lower(), i + 1
        body = []
        i += 1
        while i < len(lines) and not lines[i].lstrip().startswith(fence):
            body.append(lines[i])
            i += 1
        yield start + 1, lang, body
        i += 1


def logical_lines(first, body):
    """Join backslash continuations; strip prompts and trailing comments."""
    buf, start = "", None
    for n, raw in enumerate(body):
        line = raw.rstrip()
        if start is None:
            start = first + n
        if line.endswith("\\"):
            buf += line[:-1] + " "
            continue
        buf += line
        yield start, buf
        buf, start = "", None
    if buf:
        yield start, buf


def invocations(code):
    """Token lists (after the program name) of every tapectl invocation in a
    shell line: tapectl must be in COMMAND position — first word of a pipeline
    segment, after env assignments, `sudo -u U -H`, `time` or `$(` — so that
    `sudo chown tapectl DIR` or a table of subcommands is not a call."""
    out = []
    for seg in re.split(r"\|\||&&|;|\||\$\(|`", code):
        seg = re.split(r"\s(?:>|2>|<|>>)\s*|\)", seg, maxsplit=1)[0]
        try:
            toks = shlex.split(seg)
        except ValueError:
            toks = seg.split()
        while toks and (re.match(r"^[A-Z_][A-Z0-9_]*=", toks[0]) or toks[0] in ("time", "exec", "env")):
            toks = toks[1:]
        if len(toks) >= 4 and toks[0] == "sudo" and toks[1] == "-u" and toks[3] == "-H":
            toks = toks[4:]
        elif toks and toks[0] == "sudo":
            continue
        if toks and (toks[0] in ("tapectl", "tapectl-op") or toks[0].endswith("/tapectl")):
            out.append(toks[1:])
    return out


def check_command(helpdb, tokens):
    """tokens start just after `tapectl`. Returns a list of problems."""
    probs = []
    path = []
    i = 0
    # global options may precede the subcommand
    while i < len(tokens) and tokens[i].startswith("-"):
        if tokens[i] in ("--home", "--config"):
            i += 1
        i += 1
    while i < len(tokens):
        t = tokens[i]
        if t.startswith(("$", "<", "[", "…")) or t == "...":
            break  # a placeholder or a pass-through ("$@"), not a subcommand
        subs = helpdb.subcommands(path)
        if subs and t in subs:
            path.append(t)
            i += 1
            continue
        if subs and not t.startswith("-") and not path:
            probs.append(f"unknown command `tapectl {t}`")
            return probs
        if subs and not t.startswith("-") and helpdb.get(path)[1].count("Usage:") and "<COMMAND>" in helpdb.get(path)[1]:
            probs.append(f"unknown subcommand `tapectl {' '.join(path)} {t}`")
            return probs
        break
    if not path:
        return probs
    rc, _ = helpdb.get(path)
    if rc != 0:
        probs.append(f"`tapectl {' '.join(path)} --help` fails")
        return probs
    known = helpdb.flags(path) | helpdb.flags([])
    for t in tokens[i:]:
        if t.startswith("--"):
            f = t.split("=", 1)[0]
            if f not in known:
                probs.append(f"`tapectl {' '.join(path)}` has no flag {f}")
    return probs


def check_file(helpdb, path):
    findings = []
    text = open(path, encoding="utf-8").read()
    rel = os.path.relpath(path, REPO)
    for first, lang, body in code_blocks(text):
        if lang not in ("", "bash", "sh", "shell", "zsh", "console"):
            continue  # output (text), config (toml, checked below), diagrams…
        for lineno, line in logical_lines(first, body):
            code = line.split(" #", 1)[0] if not line.lstrip().startswith("#") else ""
            code = re.sub(r"^\s*\$\s+", "", code)
            for tokens in invocations(code):
                for p in check_command(helpdb, tokens):
                    findings.append(f"{rel}:{lineno}: {p}")
            if re.search(r"--device\s+/dev/nst\d", code):
                findings.append(f"{rel}:{lineno}: `--device /dev/nstN` — numbering is not stable; use /dev/tape/by-id/…-nst")
    return findings


def check_toml_blocks(binary, path):
    """A ```toml block that looks like a whole config.toml must pass config check."""
    findings = []
    text = open(path, encoding="utf-8").read()
    rel = os.path.relpath(path, REPO)
    for first, lang, body in code_blocks(text):
        if lang != "toml":
            continue
        src = "\n".join(body)
        if "check-docs: skip" in src or not re.search(r"^\[(dar|staging|defaults)\]", src, re.M):
            continue
        with tempfile.TemporaryDirectory() as home:
            subprocess.run([binary, "--home", home, "init", "--no-escrow"], capture_output=True)
            cfg = os.path.join(home, "config.toml")
            open(cfg, "w").write(src + "\n")
            r = subprocess.run([binary, "--home", home, "config", "check"], capture_output=True, text=True)
            if "config: valid" not in r.stdout + r.stderr:
                msg = (r.stdout + r.stderr).strip().splitlines()
                findings.append(f"{rel}:{first}: config block does not load: {msg[1] if len(msg) > 1 else msg}")
    return findings


def self_test(binary):
    """Positive control: known-bad lines must be reported, known-good must not."""
    helpdb = Help(binary)
    bad = {
        "tapectl volume frobnicate L6-0001": "unknown subcommand",
        "tapectl volume write L6-0001 --no-such-flag": "no flag --no-such-flag",
        "tapectl frobnicate": "unknown command",
    }
    good = ["tapectl volume write L6-0001 --device /dev/tape/by-id/x-nst --yes",
            "tapectl --home /tmp/h audit --json", "tapectl catalog search '*.jpg'"]
    ok = True
    for line, want in bad.items():
        p = check_command(helpdb, shlex.split(line)[1:])
        if not any(want in x for x in p):
            print(f"SELF-TEST FAIL: '{line}' not reported ({p})")
            ok = False
    for line in good:
        p = check_command(helpdb, shlex.split(line)[1:])
        if p:
            print(f"SELF-TEST FAIL: '{line}' wrongly reported: {p}")
            ok = False
    print("self-test: " + ("ok" if ok else "FAILED"))
    return 0 if ok else 1


def main():
    args = sys.argv[1:]
    binary = None
    if "--tapectl" in args:
        k = args.index("--tapectl")
        binary = args[k + 1]
        del args[k:k + 2]
    binary = find_bin(binary)
    if "--self-test" in args:
        sys.exit(self_test(binary))
    files = [os.path.abspath(a) for a in args] or DEFAULT_FILES
    helpdb = Help(binary)
    findings = []
    for f in files:
        findings += check_file(helpdb, f)
        findings += check_toml_blocks(binary, f)
    for x in findings:
        print(x)
    print(f"check-docs: {len(files)} file(s), {len(findings)} finding(s)", file=sys.stderr)
    sys.exit(1 if findings else 0)


if __name__ == "__main__":
    main()
