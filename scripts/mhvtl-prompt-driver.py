#!/usr/bin/env python3
"""mhvtl-prompt-driver.py -- the person at a cartridge prompt, for the mhvtl gate's span leg.

A restore of a unit held across tapes stops at each tape's end and asks for the next
cartridge (ADR-0012 items 89, 131 and 132; volume-format-v2.md 4.4): RESTORE.sh reads the
answer from its input, and tapectl asks only when its input is a terminal (it refuses
unattended). This runs COMMAND with its input on a pipe, or with --pty on a pseudo-terminal
(stdin only: that is all tapectl's terminal check reads), and its output and errors on one
pipe, copied to this script's output as they come. Each time PROMPT appears in that output
again, the next --swap command runs (`bash -c`, with PROMPT_N and PROMPT_LINE in its
environment: the prompt's number and the line it is on), and then ENTER is written to
COMMAND's input. A prompt with no --swap left, or whose swap fails, is answered with --stop
instead, so a run that asks more often than expected ends rather than hangs.

At each prompt it records the dar processes running under COMMAND, when the prompt came
and again just before ENTER is written: the mhvtl gate reads them to tell a dar run that
waited across the swap (--stream-tapes) from one that has not started yet (the default
mode). No process is looked up by its command line.

--events FILE gets one JSON object per line: one per prompt
  {"prompt", "line", "t_prompt", "dar_at_prompt", "swap_rc", "t_reply", "dar_at_reply",
   "reply"}
(times in seconds since COMMAND started, dar as [pid, ...]) and a last one
  {"exit", "prompts", "elapsed", "timed_out"}.
The exit status is COMMAND's; 124 when --timeout ended it.

usage: mhvtl-prompt-driver.py [--pty] --prompt TEXT --events FILE [--swap CMD]...
                              [--stop TEXT] [--timeout SECONDS] -- COMMAND [ARG]...
"""
import argparse
import json
import os
import pty
import select
import signal
import subprocess
import sys
import termios
import time


def descendants(root):
    """(pid, comm) of every process under `root`, read from /proc."""
    children = {}
    for name in os.listdir("/proc"):
        if not name.isdigit():
            continue
        try:
            with open(f"/proc/{name}/stat", "rb") as f:
                stat = f.read().decode("utf-8", "replace")
        except OSError:
            continue
        # comm is in parentheses and may hold spaces or parentheses itself.
        lpar, rpar = stat.find("("), stat.rfind(")")
        if lpar < 0 or rpar < 0:
            continue
        comm = stat[lpar + 1:rpar]
        fields = stat[rpar + 2:].split()
        if len(fields) < 2:
            continue
        children.setdefault(int(fields[1]), []).append((int(name), comm))
    found, todo = [], [root]
    while todo:
        for pid, comm in children.get(todo.pop(), []):
            found.append((pid, comm))
            todo.append(pid)
    return found


def dar_pids(root):
    return sorted(pid for pid, comm in descendants(root) if comm == "dar")


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--pty", action="store_true", help="put COMMAND's input on a pseudo-terminal")
    ap.add_argument("--prompt", required=True, help="the text that marks a prompt (a plain string)")
    ap.add_argument("--swap", action="append", default=[], help="the command run at the next prompt")
    ap.add_argument("--stop", default="quit", help="the answer to a prompt with no swap left")
    ap.add_argument("--events", required=True, help="where the JSON lines go")
    ap.add_argument("--timeout", type=float, default=3600.0)
    ap.add_argument("command", nargs=argparse.REMAINDER)
    args = ap.parse_args()
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command:
        ap.error("no COMMAND given")

    if args.pty:
        master, slave = pty.openpty()
        attrs = termios.tcgetattr(slave)
        attrs[3] &= ~termios.ECHO  # the answers are not echoed back into nothing
        termios.tcsetattr(slave, termios.TCSANOW, attrs)
        stdin = slave
    else:
        stdin = subprocess.PIPE
    out_r, out_w = os.pipe()
    start = time.monotonic()
    child = subprocess.Popen(
        command, stdin=stdin, stdout=out_w, stderr=out_w, start_new_session=True
    )
    os.close(out_w)
    if args.pty:
        os.close(slave)

        def answer(text):
            os.write(master, (text + "\n").encode())
    else:

        def answer(text):
            child.stdin.write((text + "\n").encode())
            child.stdin.flush()

    events = open(args.events, "w")
    since = lambda: round(time.monotonic() - start, 3)  # noqa: E731
    text, answered, timed_out = "", 0, False
    swaps = list(args.swap)
    while True:
        ready, _, _ = select.select([out_r], [], [], 0.2)
        if ready:
            chunk = os.read(out_r, 65536)
            if not chunk:
                break
            sys.stdout.buffer.write(chunk)
            sys.stdout.buffer.flush()
            text += chunk.decode("utf-8", "replace").replace("\r", "")
        while text.count(args.prompt) > answered:
            answered += 1
            at = -1
            for _ in range(answered):
                at = text.find(args.prompt, at + 1)
            line = text[text.rfind("\n", 0, at) + 1:].split("\n", 1)[0]
            event = {"prompt": answered, "line": line, "t_prompt": since(),
                     "dar_at_prompt": dar_pids(child.pid)}
            if swaps:
                env = dict(os.environ, PROMPT_N=str(answered), PROMPT_LINE=line)
                print(f"\n[prompt-driver] prompt {answered}: running the swap", flush=True)
                event["swap_rc"] = subprocess.call(["bash", "-c", swaps.pop(0)], env=env)
                reply = "" if event["swap_rc"] == 0 else args.stop
            else:
                print(f"\n[prompt-driver] prompt {answered}: no swap left, answering {args.stop!r}",
                      flush=True)
                event["swap_rc"] = None
                reply = args.stop
            event["dar_at_reply"] = dar_pids(child.pid)
            event["t_reply"] = since()
            event["reply"] = reply
            events.write(json.dumps(event) + "\n")
            events.flush()
            try:
                answer(reply)
            except OSError:
                pass
        if not ready and child.poll() is not None:
            # Drain what the pipe still holds, then stop.
            while select.select([out_r], [], [], 0)[0]:
                chunk = os.read(out_r, 65536)
                if not chunk:
                    break
                sys.stdout.buffer.write(chunk)
            break
        if time.monotonic() - start > args.timeout:
            timed_out = True
            os.killpg(child.pid, signal.SIGKILL)
            break
    rc = child.wait()
    if timed_out:
        rc = 124
    sys.stdout.buffer.flush()
    events.write(json.dumps({"exit": rc, "prompts": answered, "elapsed": since(),
                             "timed_out": timed_out}) + "\n")
    events.close()
    return rc


if __name__ == "__main__":
    sys.exit(main())
