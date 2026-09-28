#!/usr/bin/env python3
"""Agent messaging, server side: which pane a connecting process belongs to.

The server attributes every connection's PEER PID to a pane by process
ancestry and logs one `identity:` line per connection. The answer comes from
the process table, never from `$REMUX_PANE`, so these cases connect from a
real process running inside a real pane and read the server log:

1a. `remux ls` run by a pane's `sh -c` is a descendant of the pane's session
    leader, so it is attributed to that pane by its session id.
1b. The harness's own connection is not in any pane of THIS server, and says so.
1c. `remux ls` as the pane's own program is the session leader itself.
1d. `setsid -w remux ls` leaves the pane's session, so only the parent walk
    reaches the pane, one link up.
1e. `remux ls` under `script` runs on script's pty, not the pane's. The walk
    still reaches the pane and the differing tty is logged, never refused.
1f. A double fork that detaches fully (new session, parent gone) names no
    pane and is refused attribution.
1z. No panic in the server log.

Later tasks extend this file with the mailbox cases.

Run: python3 tests/frame/msg_mailbox.py   (RMX_RUNDIR overrides the run dir)
"""
import os
import re
import shutil
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import BIN, Server, Client, name_of  # noqa: E402

RUNDIR = os.environ.get("RMX_RUNDIR", "/tmp/rmx-msg")

failures = []


def check(cond, label):
    print(("PASS  " if cond else "FAIL  ") + label)
    if not cond:
        failures.append(label)


def rows_text(msg):
    pc = msg["PaneContent"]
    return ["".join(c["c"] for c in row).rstrip() for row in pc["cells"]]


def cli_spawn(placement, argv=None, session="main"):
    """One CliSpawn on a fresh unattached connection, as the real CLI sends it."""
    caller = Client(srv.sock)
    caller.hello()
    body = {"session": session, "placement": placement}
    if argv is not None:
        body["argv"] = argv
    caller.send({"CliSpawn": body})
    reply = None
    for msg in caller.drain(3.0):
        if name_of(msg) in ("CliSpawned", "Error"):
            reply = msg
            break
    caller.close()
    return reply


def pane_content(cli, pane_id, want, timeout=6.0):
    cli.send({"SubscribePane": {"pane_id": pane_id, "cols": 80, "rows": 20,
                                "size_demand": False}})
    end = time.time() + timeout
    seen = []
    while time.time() < end:
        for msg in cli.drain(0.3):
            if name_of(msg) != "PaneContent":
                continue
            if msg["PaneContent"]["pane_id"] != pane_id:
                continue
            seen = rows_text(msg)
            if any(want in r for r in seen):
                return True, seen
    return False, seen


def log_has(pattern, timeout=4.0):
    """Wait for a server.log line matching `pattern`, returning the match."""
    rx = re.compile(pattern)
    end = time.time() + timeout
    while True:
        for line in srv.log().splitlines():
            if rx.search(line):
                return line
        if time.time() >= end:
            return None
        time.sleep(0.2)


def identity_lines():
    return [ln for ln in srv.log().splitlines() if "identity:" in ln]


# A developer running this from inside remux would otherwise hand the throwaway
# server's panes a pane identity that belongs to their real server.
for var in ("REMUX_SESSION", "REMUX_PANE"):
    os.environ.pop(var, None)

srv = Server(RUNDIR)
try:
    srv.start()

    cli = Client(srv.sock)
    cli.hello()
    cli.send({"CreateSession": {"name": "main", "folder": None}})
    cli.send({"Attach": {"session_name": "main"}})
    cli.send({"Resize": {"cols": 120, "rows": 40}})
    cli.drain(0.8)

    # --- 1a: a descendant of the pane's session leader ---------------------
    reply = cli_spawn("SplitBelow", argv=[
        "/bin/sh", "-c", f"{BIN} ls >/dev/null; printf 'LS:%s\\n' done; exec /bin/sh"])
    check(name_of(reply) == "CliSpawned", f"1a pane A spawned (got {reply})")
    a = reply["CliSpawned"]["pane_id"] if name_of(reply) == "CliSpawned" else None
    ok, seen = pane_content(cli, a, "LS:done")
    check(ok, f"1a `remux ls` ran inside pane A to completion (saw {seen[-5:]})")
    line = log_has(rf"identity: client_id=\d+ peer=Some\(\d+\) -> pane %{a} by Sid$")
    check(line is not None,
          f"1a the connection from inside pane A is attributed to %{a} by Sid "
          f"(identity lines: {identity_lines()})")

    # --- 1b: the harness itself is in no pane of this server ---------------
    line = log_has(rf"identity: client_id=\d+ peer=Some\({os.getpid()}\) "
                   r"not a pane: not a descendant of any pane$")
    check(line is not None,
          f"1b the harness's own connection is not a pane "
          f"(identity lines: {identity_lines()})")

    # --- 1c: the pane's own program is the session leader ------------------
    reply = cli_spawn("SplitRight", argv=[BIN, "ls"])
    check(name_of(reply) == "CliSpawned", f"1c pane B spawned (got {reply})")
    b = reply["CliSpawned"]["pane_id"] if name_of(reply) == "CliSpawned" else None
    line = log_has(rf"identity: client_id=\d+ peer=Some\(\d+\) -> pane %{b} by Sid$")
    check(line is not None,
          f"1c the pane's own `remux ls` is attributed to %{b} by Sid "
          f"(identity lines: {identity_lines()})")

    # --- 1d: a sender that left the pane's session ------------------------
    reply = cli_spawn("SplitBelow", argv=["setsid", "-w", BIN, "ls"])
    check(name_of(reply) == "CliSpawned", f"1d pane D spawned (got {reply})")
    d = reply["CliSpawned"]["pane_id"] if name_of(reply) == "CliSpawned" else None
    line = log_has(rf"identity: client_id=\d+ peer=Some\(\d+\) -> pane %{d} "
                   r"by Walk \{ depth: 1 \}$")
    check(line is not None,
          f"1d `setsid -w remux ls` is attributed to %{d} by the walk at depth 1 "
          f"(identity lines: {identity_lines()})")

    # --- 1e: a sender on a nested pty --------------------------------------
    if shutil.which("script") is None:
        print("SKIP  1e `script` is not installed")
    else:
        reply = cli_spawn("SplitRight", argv=["script", "-qec", f"{BIN} ls", "/dev/null"])
        check(name_of(reply) == "CliSpawned", f"1e pane E spawned (got {reply})")
        e = reply["CliSpawned"]["pane_id"] if name_of(reply) == "CliSpawned" else None
        line = log_has(rf" INFO .*identity: client_id=\d+ peer=Some\(\d+\) -> pane %{e} "
                       r"by Walk \{ depth: \d+ \}; its tty differs \(nested pty\?\)$")
        check(line is not None,
              f"1e `remux ls` under `script` is attributed to %{e} with its tty "
              f"reported as differing (identity lines: {identity_lines()})")

    # --- 1f: a fully detached double fork ----------------------------------
    # The subshell exits at once, so the `setsid` child is reparented away
    # from the pane, and `setsid` gives it a session of its own.
    reply = cli_spawn("SplitBelow", argv=[
        "/bin/sh", "-c",
        f"( setsid /bin/sh -c 'sleep 0.5; exec {BIN} ls' & ); "
        "printf 'DF:%s\\n' forked; exec /bin/sh"])
    check(name_of(reply) == "CliSpawned", f"1f pane F spawned (got {reply})")
    rx = re.compile(r"identity: client_id=\d+ peer=Some\((\d+)\) "
                    r"not a pane: not a descendant of any pane$")
    detached = []
    end = time.time() + 5.0
    while not detached and time.time() < end:
        time.sleep(0.2)
        detached = [m.group(1) for ln in srv.log().splitlines()
                    for m in rx.finditer(ln) if m.group(1) != str(os.getpid())]
    check(len(detached) == 1,
          f"1f the detached `remux ls` is not attributed to any pane "
          f"(identity lines: {identity_lines()})")

    cli.close()
    time.sleep(0.3)
    check("panicked at" not in srv.log(), "1z no panic in the server log")
finally:
    srv.kill()

print()
if failures:
    print(f"{len(failures)} FAILED")
    sys.exit(1)
print("ALL PASS")
