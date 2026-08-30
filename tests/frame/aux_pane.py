#!/usr/bin/env python3
"""The server half of the file-manager sidebar plugin: auxiliary panes.

An aux pane is a PTY in no layout tree, owned by the connection that asked for
it. This asserts the four properties the feature rests on:

1. `SpawnAuxPane` answers `AuxPaneSpawned`, and the pane's OUTPUT reaches the
   requester through the ordinary `SubscribePane`/`PaneContent` path -- the
   frame is not the assertion, the text inside it is.
2. It spawns in the requested `cwd`.
3. It is absent from `SessionTree`, so it can never be navigated to as if it
   were a real pane.
4. It is REAPED both by an explicit `KillAuxPane` and by an abrupt disconnect.
   The reap is checked against the operating system (the child process is gone),
   not against the server's own bookkeeping -- an orphaned PTY is the headline
   risk of the feature and the server's word for it is not evidence.

Run: python3 tests/frame/aux_pane.py
"""
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import Server, Client, name_of  # noqa: E402

RUNDIR = "/tmp/rmx-aux"
# A deterministic stand-in for the file manager: prints its cwd and a marker,
# then reads keys forever, echoing each as `KEY:<char>`. The test is about
# plumbing, so a real `yazi` would only add its own rendering to the noise.
# `MARKER` is unique enough to `pgrep -f` for, which is how the orphan check
# asks the OS rather than the server.
# Each instance appends its own PID to `PIDFILE`, so liveness is asked of the
# OPERATING SYSTEM (`kill -0`) rather than of the server's bookkeeping.
# `pgrep -f` was tried first and is unusable here: the harness's own shell
# carries the pattern in its command line and matches forever.
STANDIN = """#!/bin/sh
echo "$$" >> "PIDFILE_PLACEHOLDER"
echo "CWD:$(pwd)"
while IFS= read -r line; do
  echo "KEY:$line"
done
"""

failures = []


def check(cond, label):
    print(("PASS  " if cond else "FAIL  ") + label)
    if not cond:
        failures.append(label)


def rows_text(msg):
    """Flatten a PaneContent's cell grid into a list of row strings."""
    pc = msg["PaneContent"]
    return ["".join(c["c"] for c in row).rstrip() for row in pc["cells"]]


def standin_pids():
    """PIDs of stand-ins that have not been fully reaped.

    A zombie counts. `kill(pid, 0)` succeeds for one -- a `<defunct>` child keeps
    its pid and its name until somebody waits for it -- so "not in the process
    table under its old command name" is the WEAK check that passes on exactly
    the leak under test. The state field is read instead, and the two outcomes
    are reported separately so a failure says which one happened.
    """
    if not os.path.exists(PIDFILE):
        return []
    leaked = []
    with open(PIDFILE) as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            pid = int(line)
            try:
                stat = open(f"/proc/{pid}/stat").read()
            except OSError:
                continue  # gone: reaped, which is the outcome we want
            state = stat.split(") ", 1)[1][0]
            leaked.append(f"{pid}{'(zombie)' if state == 'Z' else ''}")
    return leaked


def wait_for(pred, timeout=3.0):
    end = time.time() + timeout
    while time.time() < end:
        if pred():
            return True
        time.sleep(0.05)
    return False


def spawn_aux(cli, cwd, cols=40, rows=10):
    cli.send({"SpawnAuxPane": {"cols": cols, "rows": rows,
                               "command": STANDIN_PATH, "cwd": cwd}})
    for msg in cli.drain(2.0):
        if name_of(msg) == "AuxPaneSpawned":
            return msg["AuxPaneSpawned"]["pane_id"]
    return None


def content_for(cli, pane_id, want, timeout=3.0):
    """Subscribe and wait until some row of pane_id's content contains `want`."""
    cli.send({"SubscribePane": {"pane_id": pane_id, "cols": 40, "rows": 10,
                                "size_demand": True}})
    end = time.time() + timeout
    seen = []
    while time.time() < end:
        for msg in cli.drain(0.3):
            if name_of(msg) == "PaneContent" and msg["PaneContent"]["pane_id"] == pane_id:
                seen = rows_text(msg)
                if any(want in r for r in seen):
                    return True, seen
    return False, seen


def pane_ids_in_tree(cli):
    cli.send("ListSessionTree")
    ids = []
    for msg in cli.drain(1.5):
        if name_of(msg) != "SessionTree":
            continue
        tree = msg["SessionTree"]
        groups = list(tree["unfiled"])
        for f in tree["folders"]:
            groups += f["sessions"]
        for sess in groups:
            for tab in sess["tabs"]:
                for pane in tab["panes"]:
                    ids.append(pane["id"])
    return ids


os.makedirs(RUNDIR, exist_ok=True)
STANDIN_PATH = f"{RUNDIR}/standin.sh"
PIDFILE = f"{RUNDIR}/standin.pids"
WORKDIR = f"{RUNDIR}/workdir"

srv = Server(RUNDIR)
try:
    srv.start()
    # `Server.start` wipes RUNDIR, so the fixtures are written after it.
    os.makedirs(WORKDIR, exist_ok=True)
    with open(STANDIN_PATH, "w") as fh:
        fh.write(STANDIN.replace("PIDFILE_PLACEHOLDER", PIDFILE))
    os.chmod(STANDIN_PATH, 0o755)

    check(not standin_pids(), "no stand-in is running before the test")

    cli = Client(srv.sock)
    cli.hello()
    cli.send({"CreateSession": {"name": "main", "folder": None}})
    cli.send({"Attach": {"session_name": "main"}})
    cli.send({"Resize": {"cols": 100, "rows": 30}})
    cli.drain(0.8)

    session_panes = pane_ids_in_tree(cli)
    check(len(session_panes) == 1, f"the session has one real pane (got {session_panes})")

    # --- 1 + 2: spawn, stream, cwd ---------------------------------------
    aux = spawn_aux(cli, WORKDIR)
    check(aux is not None, "SpawnAuxPane is answered with AuxPaneSpawned")
    check(aux not in session_panes, "the aux pane id is not a session pane id")

    ok, seen = content_for(cli, aux, "CWD:")
    check(ok, f"the aux pane's output reaches its subscriber (rows={seen})")
    check(any(f"CWD:{WORKDIR}" in r for r in seen),
          f"the aux pane started in the requested cwd (rows={seen})")

    # --- input routing ----------------------------------------------------
    cli.send({"InputToPane": {"pane_id": aux, "data": list(b"hello\n")}})
    ok_key, seen_key = False, []
    end = time.time() + 3
    while time.time() < end and not ok_key:
        for msg in cli.drain(0.3):
            if name_of(msg) == "PaneContent" and msg["PaneContent"]["pane_id"] == aux:
                seen_key = rows_text(msg)
                ok_key = any("KEY:hello" in r for r in seen_key)
    check(ok_key, f"a keystroke routed by InputToPane reaches the aux pane (rows={seen_key})")

    # --- 3: invisible to the session tree ---------------------------------
    check(aux not in pane_ids_in_tree(cli),
          "the aux pane never appears in the session tree")

    check(len(standin_pids()) == 1, "exactly one stand-in process is running")

    # --- 4a: explicit kill ------------------------------------------------
    cli.send({"KillAuxPane": {"pane_id": aux}})
    exited = False
    for msg in cli.drain(1.5):
        ev = msg.get("Event") if isinstance(msg, dict) else None
        if isinstance(ev, dict) and "PaneExited" in ev and ev["PaneExited"]["pane_id"] == aux:
            exited = True
    check(exited, "KillAuxPane reports PaneExited to the subscriber")
    check(wait_for(lambda: not standin_pids()),
          f"KillAuxPane leaves no stand-in process behind (left={standin_pids()})")

    # --- 4b: abrupt disconnect -------------------------------------------
    aux2 = spawn_aux(cli, WORKDIR)
    check(aux2 is not None, "a second aux pane spawns")
    ok2, _ = content_for(cli, aux2, "CWD:")
    check(ok2, "the second aux pane is live")
    check(len(standin_pids()) == 1, "the second stand-in is running")
    # Drop the socket with no KillAuxPane: this is the abrupt path.
    cli.close()
    check(wait_for(lambda: not standin_pids(), 5.0),
          f"an abrupt client disconnect reaps the aux pane "
          f"(no orphan process; left={standin_pids()})")

    log = srv.log()
    check("panicked at" not in log, "no panic in the server log")
finally:
    srv.kill()
    for entry in standin_pids():
        try:
            os.kill(int(entry.split("(")[0]), 9)
        except OSError:
            pass

print()
if failures:
    print(f"{len(failures)} FAILED:")
    for f in failures:
        print("  - " + f)
    sys.exit(1)
print("all checks passed")
