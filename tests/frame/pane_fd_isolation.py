#!/usr/bin/env python3
"""No pane's shell holds another pane's terminal.

A pane's master is the pane: whatever holds it can type into the pane and
read what it prints, outside every messaging guard, and while it is held the
pane's own shell never sees its hangup. A pane created while another is
being created must not inherit the other's master (/dev/ptmx) or slave
(/dev/pts/N), so this creates many panes at once, from many connections,
then has each pane's shell list where its own descriptors point.

  1. every pane reports its descriptors
  2. no pane's shell holds /dev/ptmx (another pane's master)
  3. no pane's shell holds a /dev/pts/N other than its own terminal
  4. no panic

Run: python3 tests/frame/pane_fd_isolation.py   (RMX_RUNDIR overrides the run dir)
"""
import os
import re
import sys
import threading
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import Client, Server, name_of  # noqa: E402

RUNDIR = os.environ.get("RMX_RUNDIR", "/tmp/rmx-fdiso")
CONNECTIONS = 12
PER_CONNECTION = 4

FAILURES = []


def check(cond, label):
    print(("PASS  " if cond else "FAIL  ") + label)
    if not cond:
        FAILURES.append(label)


def spawner(sock, gate, out):
    """Create PER_CONNECTION panes on one connection, each in a tab of its own,
    as soon as the gate opens, without waiting for one before the next."""
    c = Client(sock)
    c.hello()
    gate.wait()
    for _ in range(PER_CONNECTION):
        c.send({"CliSpawn": {"session": "main", "placement": "NewTab"}})
    end = time.time() + 15
    got = []
    while time.time() < end and len(got) < PER_CONNECTION:
        for m in c.drain(0.3):
            if name_of(m) == "CliSpawned":
                got.append(m["CliSpawned"]["pane_id"])
    c.close()
    out.extend(got)


srv = Server(RUNDIR)
try:
    srv.start()
    cli = Client(srv.sock)
    cli.hello()
    cli.send({"CreateSession": {"name": "main", "folder": None}})
    cli.send({"Attach": {"session_name": "main"}})
    cli.send({"Resize": {"cols": 120, "rows": 40}})
    cli.drain(0.8)

    gate = threading.Barrier(CONNECTIONS)
    panes = []
    threads = [threading.Thread(target=spawner, args=(srv.sock, gate, panes)) for _ in range(CONNECTIONS)]
    for t in threads:
        t.start()
    for t in threads:
        t.join(30)
    time.sleep(1.5)

    # Each shell lists ITS OWN descriptors ($$), not those of the commands
    # the loop runs, and names its terminal by fd 0.
    def ask_for_fds(targets):
        for pane in targets:
            line = (f"for f in /proc/$$/fd/*; do printf '%s %s\\n' \"${{f##*/}}\" \"$(readlink \"$f\")\"; "
                    f"done > {RUNDIR}/fd-{pane}.tmp; mv {RUNDIR}/fd-{pane}.tmp {RUNDIR}/fd-{pane}\n")
            cli.send({"InputToPane": {"pane_id": pane, "data": list(line.encode())}})

    def unreported():
        return [p for p in panes if not os.path.exists(f"{RUNDIR}/fd-{p}")]

    # 48 shells start at once, and a slow one can miss the line typed into
    # it; it is asked once more.
    ask_for_fds(panes)
    for wait in (8, 15):
        end = time.time() + wait
        while time.time() < end and unreported():
            time.sleep(0.2)
        if wait == 8 and unreported():
            ask_for_fds(unreported())

    reported, masters, others = 0, [], []
    for pane in panes:
        try:
            links = dict(ln.split(" ", 1) for ln in open(f"{RUNDIR}/fd-{pane}").read().splitlines() if " " in ln)
        except OSError:
            continue
        reported += 1
        own = links.get("0", "")
        for fd, target in links.items():
            if target == "/dev/ptmx" or target.endswith("/ptmx"):
                masters.append((pane, fd, target))
            elif re.fullmatch(r"/dev/pts/\d+", target) and target != own:
                others.append((pane, fd, target, own))
    check(len(panes) == CONNECTIONS * PER_CONNECTION and reported == len(panes),
          f"1 all {len(panes)} panes created at once report their descriptors ({reported} did; "
          f"missing {unreported()})")
    check(not masters, f"2 no pane's shell holds another pane's master (found {masters[:6]})")
    check(not others, f"3 no pane's shell holds another pane's terminal (found {others[:6]})")
    check("panicked at" not in srv.log(), "4 no panic")
finally:
    srv.kill()

print()
if FAILURES:
    print(f"{len(FAILURES)} FAILED")
    sys.exit(1)
print("ALL PASS")
