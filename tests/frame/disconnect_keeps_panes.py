#!/usr/bin/env python3
"""An abrupt client disconnect must leave the session's panes RUNNING.

This is the surviving half of what `tests/frame/aux_pane.py` used to assert, and
it is here because that file was deleted.

`aux_pane.py` checked four properties of auxiliary panes, three of which died
with the feature (the spawn/answer handshake, the `cwd`, and their absence from
the session tree). The fourth was that a disconnect REAPED them, asked of the
operating system rather than of the server's bookkeeping — because an orphaned
PTY was the headline risk of that feature.

Aux panes are gone and every pane now belongs to a session's layout, so the
property inverts: a disconnect must reap **nothing**. That is not a formality.
`handle_client_disconnect` used to take the departing connection's aux panes out
and call `reap_panes` on them; removing that left a rewritten function on the
path every detach takes, and getting it wrong in the other direction — reaping
what is left, rather than what was owned — would kill a user's work every time
they closed a terminal. Detaching and finding your session intact is the single
thing a multiplexer is for, and nothing else in the suite asserts it.

Liveness is asked of the OS (`kill -0` on a pid the pane's own shell wrote to a
file), never of the server: "the server still lists it" is exactly the evidence
that a bookkeeping-only reap would leave intact.

The disconnect is ABRUPT — the socket is closed with no detach, as a killed
terminal emulator does. That is the path with no `KillAuxPane`-shaped warning
before it, and the one `handle_client_disconnect` exists to cover.

Run: python3 tests/frame/disconnect_keeps_panes.py
"""
import os
import shutil
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import Server, Client  # noqa: E402

RUNDIR = "/tmp/rmx-disc"
PIDFILE = f"{RUNDIR}/pane.pid"

failures = []


def check(cond, label, detail=""):
    print(("PASS  " if cond else "FAIL  ") + label)
    if not cond:
        failures.append(label)
        if detail:
            print(f"        {detail}")


def alive(pid):
    """Ask the OPERATING SYSTEM, not the server."""
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        # It exists and belongs to someone else, which cannot happen here but
        # is still "alive" rather than "gone".
        return True


def pane_ids(client):
    """Every pane id the session tree reports, across every session and tab."""
    client.send("ListSessionTree")
    end = time.time() + 4.0
    while time.time() < end:
        msg = client.recv()
        if not (isinstance(msg, dict) and "SessionTree" in msg):
            continue
        body = msg["SessionTree"]
        sessions = list(body["unfiled"]) + [
            s for f in body["folders"] for s in f["sessions"]
        ]
        return [p["id"] for s in sessions for t in s["tabs"] for p in t["panes"]]
    raise SystemExit("no SessionTree came back")


def main():
    if not os.path.exists(os.environ.get("REMUX_BIN", "target/debug/remux")):
        raise SystemExit("target/debug/remux not found; run `cargo build` first")
    shutil.rmtree(RUNDIR, ignore_errors=True)
    srv = Server(RUNDIR).start()
    try:
        a = Client(srv.sock)
        a.hello()
        a.send({"CreateSession": {"name": "main", "folder": None}})
        a.send({"Attach": {"session_name": "main"}})
        a.send({"Resize": {"cols": 80, "rows": 24}})
        a.drain(0.8)

        # A second pane, so the check covers a session with more than the one
        # the attach created.
        a.send({"Command": "PaneSplitVertical"})
        a.drain(0.8)

        before = pane_ids(a)
        check(len(before) == 2, "the session has two panes to lose", before)

        # Each pane's own shell writes its pid. TYPED text cannot fake this:
        # the file is written by the process under test, and `$$` is expanded
        # by that shell, so the number in the file is one the harness never
        # knew. A shell echoing the command line puts `$$` on screen, not a pid.
        a.send({"Input": {"data": list(b"echo $$ >> %s\n" % PIDFILE.encode())}})
        time.sleep(0.6)
        a.send({"Command": "PaneFocusLeft"})
        a.send({"Input": {"data": list(b"echo $$ >> %s\n" % PIDFILE.encode())}})
        time.sleep(0.8)
        a.drain(0.5)

        pids = []
        if os.path.exists(PIDFILE):
            pids = [int(l) for l in open(PIDFILE).read().split() if l.strip().isdigit()]
        check(len(pids) == 2, "both panes' shells reported their pids", pids)
        check(all(alive(p) for p in pids), "and both are running before the drop", pids)
        if failures:
            # Everything below is meaningless without live pids to watch.
            raise SystemExit(1)

        # ABRUPT: close the socket with no detach, as a killed terminal does.
        a.close()
        time.sleep(1.0)

        for p in pids:
            check(alive(p), f"pane process {p} survived the client's disconnect")

        # And the server still knows about them. Checked SECOND and treated as
        # the weaker of the two: a reap that only forgot the panes would leave
        # this failing while the processes above stayed alive, and a reap that
        # only killed them would leave this passing. Both are asked.
        b = Client(srv.sock)
        b.hello()
        after = pane_ids(b)
        check(sorted(after) == sorted(before),
              "the session tree still lists exactly the same panes",
              (before, after))

        # Reattaching gets a frame rather than an error -- the session is usable,
        # not merely present.
        b.send({"Attach": {"session_name": "main"}})
        b.send({"Resize": {"cols": 80, "rows": 24}})
        msgs = b.drain(1.2)
        check(any(isinstance(m, dict) and "FullRender" in m for m in msgs),
              "a fresh client can reattach and gets a frame",
              [k for m in msgs for k in (m if isinstance(m, dict) else {})])
        b.close()
    finally:
        log = srv.log()
        check("panicked at" not in log, "no panic in the server log", log[-1200:])
        srv.kill()

    if failures:
        print(f"\nFAILED: {len(failures)}")
        for f in failures:
            print(f"  - {f}")
        sys.exit(1)
    print("\nOK")


if __name__ == "__main__":
    main()
