#!/usr/bin/env python3
"""Frame-level test: the server's agent list (PROTOCOL_VERSION 8).

Covers `SubscribeAgents` / `UnsubscribeAgents` / `AgentList`:

  1. subscribing yields an immediate `AgentList` (a panel is populated at once);
  2. a pane running a LISTED command appears, with its session and tab;
  3. a pane running an UNLISTED command never does;
  4. output flips an entry to `Working`;
  5. silence past the working window decays it to `Idle`;
  6. a configured pattern flips it to `NeedsInput` -- **in the FOREGROUND tab**,
     which is the §3 regression guard: `record_pane_activity` returns early for
     the tab being viewed, so anything piggybacking on tab activity would be
     blind to exactly the pane the user is looking at;
  7. **a `NeedsInput` entry is STILL `NeedsInput` after silence far beyond the
     working window.** This is the §11 bug the original spec would have shipped:
     a blocked agent produces no output precisely BECAUSE it is waiting, so a
     state that decayed on silence would vanish at the moment the user needs it;
  8. clearing the prompt off the screen lets it move on;
  9. an agent in a BACKGROUND tab is listed too, with its own tab index;
 10. `UnsubscribeAgents` stops the pushes.

Uses a stand-in `claude` script on `PATH`, so nothing here needs a real agent
installed.

Run: python3 tests/frame/agents_panel.py
"""
import os
import shutil
import stat
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import Client, Server, name_of  # noqa: E402

RUNDIR = "/tmp/rmx-agents"
# OUTSIDE the rundir on purpose: `Server.start` wipes the rundir, so a stand-in
# written inside it would be deleted before the server ever looked for it -- and
# the pane would silently fall back to whatever real `claude` the developer
# happens to have installed.
BINDIR = "/tmp/rmx-agents-bin"
FAILURES = []

# The stand-in agent. `comm` for a `#!`-script is the SCRIPT's basename, so
# `/proc/<pgid>/comm` reads `claude` exactly as it would for the real thing.
#
# It reads commands from its stdin, which is the pane's PTY: once it is in the
# foreground the shell is not reading, so `Input` reaches this script.
AGENT = """#!/bin/sh
printf 'agent ready\\n'
while read line; do
  case "$line" in
    block) printf 'Do you want to proceed?\\n> 1. Yes\\n  2. No\\n' ;;
    clear) printf '\\033[2J\\033[H'; printf 'back to work\\n' ;;
    *) printf 'ok\\n' ;;
  esac
done
"""

# Same program under a name the config does not list.
NOT_AGENT = AGENT

CONFIG = """
[agents]
commands = ["claude"]
working_ms = 400
scan_rows = 12

  [[agents.pattern]]
  name = "test-approval"
  command = "claude"
  regex = "Do you want to proceed"
"""


def check(cond, msg):
    if cond:
        print(f"  PASS  {msg}")
    else:
        print(f"  FAIL  {msg}")
        FAILURES.append(msg)


def write_bins():
    shutil.rmtree(BINDIR, ignore_errors=True)
    os.makedirs(BINDIR, exist_ok=True)
    for name, body in (("claude", AGENT), ("notanagent", NOT_AGENT)):
        p = f"{BINDIR}/{name}"
        with open(p, "w") as f:
            f.write(body)
        os.chmod(p, os.stat(p).st_mode | stat.S_IEXEC | stat.S_IXGRP | stat.S_IXOTH)
    os.environ["PATH"] = BINDIR + ":" + os.environ["PATH"]


def lists(msgs):
    return [m["AgentList"]["agents"] for m in msgs if name_of(m) == "AgentList"]


def latest(c, timeout=1.6):
    """The most recent `AgentList` seen within `timeout`, or None."""
    seen = None
    end = time.time() + timeout
    while time.time() < end:
        for got in lists(c.drain(0.15)):
            seen = got
    return seen


def wait_for(c, predicate, timeout=4.0):
    """The first `AgentList` satisfying `predicate`, else the last one seen."""
    last = None
    end = time.time() + timeout
    while time.time() < end:
        for got in lists(c.drain(0.15)):
            last = got
            if predicate(got):
                return got
    return last


def states(agents):
    return {(a["command"], a["state"]) for a in agents}


def main():
    write_bins()
    srv = Server(RUNDIR).start(config=CONFIG)
    try:
        try:
            run(srv)
        except Exception as e:  # noqa: BLE001 -- see below
            # Deliberately broad. A server that dies mid-run takes the rest of
            # the checks with it, and the most useful thing left to say is in
            # the LOG -- so the run must reach the panic check below rather than
            # ending in a traceback that never looks at it.
            check(False, f"the run completed without an exception ({e!r})")
    finally:
        log = srv.log()
        srv.kill()

    check("panicked at" not in log, "no panic in the server log")

    print()
    if FAILURES:
        print(f"FAILED ({len(FAILURES)}): " + "; ".join(FAILURES))
        return 1
    print("agents_panel: all checks passed")
    return 0


def run(srv):
    c = Client(srv.sock)
    c.hello()
    c.send({"CreateSession": {"name": "main", "folder": None}})
    c.send({"Attach": {"session_name": "main"}})
    c.send({"Resize": {"cols": 100, "rows": 30}})
    c.drain(0.5)

    # 1. Subscribing is answered at once.
    c.send("SubscribeAgents")
    first = latest(c, 1.2)
    check(first is not None, "1 subscribing yields an AgentList immediately")
    check(first == [], "1 with no agent running, the list is empty")

    # 3. An unlisted command first, so its absence is not merely "nothing has
    #    started yet" by the time the listed one is checked.
    c.send({"Input": {"data": list(b"notanagent\n")}})
    time.sleep(1.0)
    got = latest(c, 1.2)
    check(
        all(a["command"] != "notanagent" for a in got or []),
        "3 a pane running an unlisted command is not listed",
    )
    # Leave it running in its own pane so it stays a live negative for the rest
    # of the run: split, then start the agent in the NEW pane.
    c.send({"Command": "PaneSplitVertical"})
    time.sleep(0.4)
    c.drain(0.3)

    # 2 + 4. The agent appears, and its start-up output reads as Working.
    c.send({"Input": {"data": list(b"claude\n")}})
    got = wait_for(c, lambda ags: any(a["command"] == "claude" for a in ags))
    entry = next((a for a in got or [] if a["command"] == "claude"), None)
    check(entry is not None, "2 a pane running a listed command is listed")
    if entry is None:
        return
    check(entry["session"] == "main", "2 the entry names its session")
    check(entry["tab_index"] == 0, "2 the entry names its tab")
    check(
        all(a["command"] != "notanagent" for a in got or []),
        "3 and the unlisted one is still absent",
    )
    agent_pane = entry["pane_id"]

    got = wait_for(
        c,
        lambda ags: any(a["command"] == "claude" and a["state"] == "Working" for a in ags),
        timeout=2.0,
    )
    check(
        ("claude", "Working") in states(got or []),
        "4 output flips the entry to Working",
    )

    # 5. Silence decays it.
    time.sleep(1.2)
    got = latest(c, 1.2)
    check(
        ("claude", "Idle") in states(got or []),
        "5 silence past the working window decays Working to Idle",
    )

    # 6. A configured pattern, in the FOREGROUND tab (the §3 guard).
    c.send({"Input": {"data": list(b"block\n")}})
    got = wait_for(
        c,
        lambda ags: any(a["state"] == "NeedsInput" for a in ags),
        timeout=3.0,
    )
    check(
        ("claude", "NeedsInput") in states(got or []),
        "6 a matched pattern flips the entry to NeedsInput IN THE FOREGROUND TAB",
    )

    # 7. THE ONE THAT MATTERS: it must not decay.
    #
    #    Asked of a FRESH connection, whose `SubscribeAgents` is answered with a
    #    list collected then and there. That is the strongest form of the
    #    question: a server that only kept saying `NeedsInput` because it
    #    remembered saying it would fail this, and a classifier that re-derives
    #    the state from the screen with no memory at all passes it.
    #
    #    The silence first is 6x the 400ms working window.
    c.drain(0.5)
    quiet = lists(c.drain(2.5))
    check(
        quiet == [],
        f"7 holding NeedsInput costs no pushes at all (got {len(quiet)})",
    )
    b = Client(srv.sock)
    b.hello()
    b.send("SubscribeAgents")
    fresh = latest(b, 1.5)
    check(
        ("claude", "NeedsInput") in states(fresh or []),
        "7 NeedsInput SURVIVES silence far beyond the working window",
    )
    b.close()

    # 8. Clearing the prompt off the screen lets it move on again.
    c.send({"Input": {"data": list(b"clear\n")}})
    got = wait_for(
        c,
        lambda ags: any(a["command"] == "claude" and a["state"] != "NeedsInput" for a in ags),
        timeout=3.0,
    )
    check(
        any(a["command"] == "claude" and a["state"] != "NeedsInput" for a in got or []),
        "8 a prompt cleared off the screen releases NeedsInput",
    )

    # 9. An agent in a BACKGROUND tab is listed too.
    c.send({"Command": "TabNew"})
    time.sleep(0.5)
    c.drain(0.3)
    c.send({"Input": {"data": list(b"claude\n")}})
    wait_for(c, lambda ags: len([a for a in ags if a["command"] == "claude"]) == 2, timeout=3.0)
    # Tab 1 is the foreground now; go back so the FIRST agent is the one being
    # viewed and the second is in the background.
    c.send({"Command": "TabNext"})
    time.sleep(0.6)
    got = latest(c, 1.5) or []
    claudes = sorted(a["tab_index"] for a in got if a["command"] == "claude")
    check(claudes == [0, 1], f"9 an agent in a background tab is listed too (got {claudes})")
    check(
        any(a["pane_id"] == agent_pane for a in got),
        "9 and the first agent kept its identity across the refresh",
    )

    # 10. Unsubscribing stops the pushes, even while output is flowing.
    c.send("UnsubscribeAgents")
    c.drain(0.5)
    c.send({"Input": {"data": list(b"hello\n")}})
    time.sleep(1.0)
    check(lists(c.drain(0.8)) == [], "10 UnsubscribeAgents stops the pushes")

    c.close()


if __name__ == "__main__":
    sys.exit(main())
