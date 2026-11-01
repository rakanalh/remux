#!/usr/bin/env python3
"""A frame header asking for 4 GiB drops that connection and nothing else.

A frame is a 4-byte big-endian length and a JSON payload, and the payload
buffer is allocated before any of it arrives. A header of 0xFFFFFFFF (a
garbage or hostile peer) asked for 4 GiB, which could abort the whole server.

1. 0xFFFFFFFF as the very first frame, where the Hello is expected: the
   connection is dropped.
2. 0xFFFFFFFF after a completed handshake, in the reader loop: dropped too.
3. The server is still running, and a second client completes a full round
   trip (Hello/Welcome, then ListSessionTree answered).
4. The refusal is logged with its length, and nothing panicked.

Run: python3 tests/frame/frame_cap.py   (RMX_RUNDIR overrides the run dir)
"""
import os
import socket
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import Server, Client, name_of  # noqa: E402

RUNDIR = os.environ.get("RMX_RUNDIR", "/tmp/rmx-framecap")
HUGE = b"\xff\xff\xff\xff"

failures = []


def check(cond, label):
    print(("PASS  " if cond else "FAIL  ") + label)
    if not cond:
        failures.append(label)


def dropped(sock, timeout=3.0):
    """Whether the server closed `sock`: a read returns EOF (or a reset)."""
    sock.settimeout(timeout)
    try:
        while True:
            chunk = sock.recv(65536)
            if not chunk:
                return True
    except socket.timeout:
        return False
    except ConnectionError:
        return True


for var in ("REMUX_SESSION", "REMUX_PANE"):
    os.environ.pop(var, None)

srv = Server(RUNDIR)
try:
    srv.start()

    raw = socket.socket(socket.AF_UNIX)
    raw.connect(srv.sock)
    raw.sendall(HUGE)
    check(dropped(raw), "1 a 4 GiB header in place of the Hello drops the connection")
    raw.close()

    c = Client(srv.sock)
    c.hello()
    c.s.sendall(HUGE)
    check(dropped(c.s), "2 a 4 GiB header after the handshake drops the connection")
    c.close()

    time.sleep(0.3)
    check(srv.proc.poll() is None, "3 the server is still running")
    ok = False
    try:
        d = Client(srv.sock)
        d.hello()
        d.send("ListSessionTree")
        ok = any(name_of(m) == "SessionTree" for m in d.drain(1.5))
        d.close()
    except OSError as e:
        print(f"      second client failed: {e}")
    check(ok, "3 a second client completes a full round trip")

    log = srv.log()
    check(log.count("refusing a 4294967295-byte frame") == 2,
          "4 both refusals are logged with their length")
    check("panicked at" not in log, "4 no panic in the server log")
finally:
    srv.kill()

print()
if failures:
    print(f"{len(failures)} FAILED")
    sys.exit(1)
print("ALL PASS")
