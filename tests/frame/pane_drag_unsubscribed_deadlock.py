#!/usr/bin/env python3
"""A MouseDrag on a pane the client does not subscribe to must not wedge the daemon.

`handle_pane_mouse_drag` reads the client's scroll offset / gesture under a
`clients` guard, and the "not subscribed" arm of that same `match` used to call
`disarm_pane_autoscroll`, which locks `clients` again. `tokio::sync::Mutex` is
not reentrant, so the task parked forever *holding* `clients` -- and every other
task that needs the map blocked behind it. One client's stray drag hung the
whole daemon, so this test asserts a SECOND, unrelated client keeps being served
too; a single-client test would not show the headline symptom.

The probe is `ListSessionTree`, which locks `clients` itself, so a reply proves
the map is free. Both waits are bounded: a wedged daemon fails the test on a
socket timeout instead of hanging the harness.

Run: python3 tests/frame/pane_drag_unsubscribed_deadlock.py
"""
import os, socket, sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import Server, Client, name_of

RUNDIR = "/tmp/rmx-pdd"
# A pane id no client subscribes to. Any unsubscribed pane reaches the same
# arm -- the `subscribed_panes` guard is checked before the pane map is.
BOGUS_PANE = 999999

failures = []


def probe(c, who, timeout=4.0):
    """Ask for the session tree and wait, bounded, for the reply."""
    c.send("ListSessionTree")
    c.s.settimeout(timeout)
    try:
        for _ in range(200):
            if name_of(c.recv()) == "SessionTree":
                return True
        failures.append(f"{who}: no SessionTree among 200 messages")
        return False
    except socket.timeout:
        failures.append(f"{who}: no reply within {timeout}s -- daemon wedged")
        return False


srv = Server(RUNDIR)
srv.start()
try:
    # Both clients connect BEFORE the bad drag: a client connecting afterwards
    # would hang in the accept loop's insert into `clients`, which would test
    # connecting rather than serving.
    a = Client(srv.sock)
    b = Client(srv.sock)
    a.hello()
    b.hello()
    a.send({"CreateSession": {"name": "main", "folder": None}})
    a.send({"Attach": {"session_name": "main"}})
    a.send({"Resize": {"cols": 80, "rows": 24}})
    a.drain(0.6)
    b.drain(0.3)

    # Both are served before the drag, so a later timeout is the drag's doing.
    if not probe(a, "client A (before drag)"):
        raise SystemExit("FAIL: " + "; ".join(failures))
    if not probe(b, "client B (before drag)"):
        raise SystemExit("FAIL: " + "; ".join(failures))

    # The trigger: a drag naming a pane this client never subscribed to.
    a.send({"MouseDrag": {"start_x": 0, "start_y": 0, "end_x": 5, "end_y": 0,
                          "is_final": False, "pane_id": BOGUS_PANE}})

    # A: its own reader task is parked inside the handler if the guard leaked.
    probe(a, "client A (after drag)")
    # B: unrelated client, the headline symptom -- one client's mistake must not
    # take the daemon down for everyone.
    probe(b, "client B (after drag)")

    a.close()
    b.close()

    log = srv.log()
    if "panicked at" in log:
        failures.append("server panicked: " + log[log.index("panicked at"):][:200])
finally:
    srv.kill()

if failures:
    print("FAIL: " + "; ".join(failures))
    sys.exit(1)
print("PASS: an unsubscribed pane drag leaves the daemon serving both clients")
