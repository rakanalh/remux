#!/usr/bin/env python3
"""`state.json` across builds: a file this build writes loads in the build
before agent session restore, and a file that build writes loads here.

Builds nothing. `$REMUX_OLD_BIN` names the older binary, made once from a
scratch worktree, never from a checkout someone is working in:

    git worktree add --detach <scratch> 71b60fb
    cargo build --manifest-path <scratch>/Cargo.toml --target-dir <scratch>/target
    REMUX_OLD_BIN=<scratch>/target/debug/remux python3 tests/frame/state_compat.py

Without it the harness prints SKIP and exits 0, which is visible and never a
silent pass.

new_to_old  this build stores an agent session record and saves on stop; the
            old server starts on the same dirs and restores the session.
old_to_new  the old server creates a session and saves on stop; this build
            starts on the same dirs, restores it, and holds no record.

Both servers log "restored N sessions" only after `load_state` parsed the
file; a file it cannot parse is logged as a failure and starts fresh, so the
count is the evidence. `STATE_COMPAT_PROBE=corrupt` truncates the file between
the two servers, which must turn both directions red.

Run: REMUX_OLD_BIN=... python3 tests/frame/state_compat.py
"""
import json
import os
import re
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import BIN, Client, Server, name_of  # noqa: E402
from rundirs import run_dir  # noqa: E402

RUNDIR = os.environ.get("RMX_RUNDIR") or run_dir("rmx-statecompat")
OLD = os.environ.get("REMUX_OLD_BIN")
RECORD_ID = "abcd0000-1111-4222-8333-944445555001"
CONFIG = """
[general]
save_sessions = true
automatic_restore = true
"""

failures = []


def check(cond, label):
    print(("PASS  " if cond else "FAIL  ") + label)
    if not cond:
        failures.append(label)


def launch(srv, binary):
    """Start `binary` as the server on `srv`'s dirs, keeping what is there."""
    srv.proc = subprocess.Popen([binary, "server"], env=srv.env,
                                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(200):
        if os.path.exists(srv.sock):
            time.sleep(0.3)
            return
        time.sleep(0.05)
    raise SystemExit(f"no socket from {binary}")


def stop(srv):
    """SIGTERM, which saves on the way out, as `remux stop` does. Sent
    directly so that this build's CLI never has to speak to the old server."""
    srv.proc.terminate()
    srv.proc.wait(timeout=10)
    for _ in range(100):
        if not os.path.exists(srv.sock):
            break
        time.sleep(0.05)


def create_session(srv, name):
    c = Client(srv.sock)
    c.hello()
    c.send({"CreateSession": {"name": name, "folder": None}})
    c.send({"Attach": {"session_name": name}})
    c.send({"Resize": {"cols": 100, "rows": 30}})
    c.drain(0.8)
    c.send("ListSessionTree")
    pane = None
    for m in c.drain(0.8):
        if name_of(m) == "SessionTree":
            for sess in m["SessionTree"]["unfiled"]:
                if sess["name"] == name:
                    pane = sess["tabs"][0]["panes"][0]["id"]
    c.close()
    return pane


def report(srv, pane):
    c = Client(srv.sock)
    c.hello()
    c.send({"AgentSession": {"agent": "claude", "reference": {"Id": RECORD_ID}, "pane": pane}})
    try:
        for m in c.drain(1.0):
            if name_of(m) in ("AgentSessionStored", "Error"):
                return m
    finally:
        c.close()
    return None


def state_file(srv):
    return f"{srv.rundir}/data/remux/state.json"


def between(srv):
    if os.environ.get("STATE_COMPAT_PROBE") == "corrupt":
        with open(state_file(srv), "r+") as f:
            f.truncate(20)
        print("probe: state.json truncated")


def restored_since(srv, offset):
    m = re.findall(r"restored (\d+) sessions", srv.log()[offset:])
    return int(m[-1]) if m else None


def new_to_old():
    srv = Server(f"{RUNDIR}-n2o").start(config=CONFIG)
    try:
        pane = create_session(srv, "compat")
        r = report(srv, pane)
        check(name_of(r) == "AgentSessionStored", f"new_to_old: this build stored a record (got {r})")
        stop(srv)
        with open(state_file(srv)) as f:
            saved = json.load(f)
        check(str(pane) in saved.get("agent_sessions", {}),
              "new_to_old: the file this build wrote holds agent_sessions")
        between(srv)
        offset = len(srv.log())
        launch(srv, OLD)
        n = restored_since(srv, offset)
        check(n is not None and n >= 1, f"new_to_old: the old server restored the session (restored {n})")
        check("panicked at" not in srv.log()[offset:], "new_to_old: no panic in the old server's log")
    finally:
        srv.kill()


def old_to_new():
    srv = Server(f"{RUNDIR}-o2n").start(config=CONFIG)
    srv.kill()
    try:
        launch(srv, OLD)
        create_session(srv, "compat")
        stop(srv)
        with open(state_file(srv)) as f:
            saved = json.load(f)
        check("agent_sessions" not in saved, "old_to_new: the old server's file has no agent_sessions")
        between(srv)
        offset = len(srv.log())
        launch(srv, BIN)
        n = restored_since(srv, offset)
        check(n is not None and n >= 1, f"old_to_new: this build restored the session (restored {n})")
        log = srv.log()[offset:]
        check("typed resume" not in log, "old_to_new: and typed nothing")
        check("panicked at" not in log, "old_to_new: no panic in this build's log")
    finally:
        srv.kill()


def main():
    if not OLD:
        print("SKIP  state_compat: set REMUX_OLD_BIN to a build from before agent session restore")
        return
    if not os.access(OLD, os.X_OK):
        raise SystemExit(f"REMUX_OLD_BIN={OLD} is not an executable")
    new_to_old()
    old_to_new()
    if failures:
        print(f"\n{len(failures)} FAILED")
        sys.exit(1)
    print("\nALL PASS")


if __name__ == "__main__":
    main()
