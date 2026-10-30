#!/usr/bin/env python3
"""A silent connection must not hold up anyone else's.

The server gives each connection 5s to send its `Hello`. If the accept loop
waited for that handshake itself, one same-uid peer that connects and says
nothing would stall every other client, the activity tick and SIGTERM handling
for the whole timeout.

1. With a silent connection open, a second client completes Hello/Welcome
   in well under the 5s handshake timeout.
2. It can then use the server normally: a session tree request is answered.
3. A second silent connection opened AFTER that client does not stall a third.
4. No panic in the server log.

Run: python3 tests/frame/accept_concurrent.py   (RMX_RUNDIR overrides the run dir)
"""
import os
import socket
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import Server, Client, name_of  # noqa: E402

RUNDIR = os.environ.get("RMX_RUNDIR", "/tmp/rmx-accept")
BUDGET = 1.5

failures = []


def check(cond, label):
    print(("PASS  " if cond else "FAIL  ") + label)
    if not cond:
        failures.append(label)


def silent():
    s = socket.socket(socket.AF_UNIX)
    s.connect(srv.sock)
    # Let the server accept it and start waiting for its Hello, so the next
    # client connects while the silent one is mid-handshake.
    time.sleep(0.2)
    return s


def timed_hello():
    start = time.time()
    c = Client(srv.sock)
    c.s.settimeout(8.0)
    welcome = c.hello()
    return c, welcome, time.time() - start


for var in ("REMUX_SESSION", "REMUX_PANE"):
    os.environ.pop(var, None)

srv = Server(RUNDIR)
try:
    srv.start()

    quiet1 = silent()
    c1, welcome, took = timed_hello()
    check("protocol_version" in welcome, f"1 the second client got a Welcome ({welcome})")
    check(took < BUDGET, f"1 its handshake took {took:.2f}s beside a silent peer (budget {BUDGET}s)")

    c1.send("ListSessionTree")
    got = [name_of(m) for m in c1.drain(1.0)]
    check("SessionTree" in got, f"2 the second client is served normally (got {got})")

    quiet2 = silent()
    c2, welcome, took = timed_hello()
    check(took < BUDGET, f"3 a third client's handshake took {took:.2f}s (budget {BUDGET}s)")

    for s in (quiet1, quiet2):
        s.close()
    c1.close()
    c2.close()
    time.sleep(0.3)
    check("panicked at" not in srv.log(), "4 no panic in the server log")
finally:
    srv.kill()

print()
if failures:
    print(f"{len(failures)} FAILED")
    sys.exit(1)
print("ALL PASS")
