#!/usr/bin/env python3
"""`remux server` never takes the socket of a server that is still listening.

Startup used to unlink whatever sat at the socket path and bind its own. Run
while another server was live, it left that server running, with every session
in it, where no client could reach it. Now only a socket nobody listens on is
treated as stale.

  1  a second `remux server` against a live one exits non-zero and says so
  2  the first server's socket is the same file, and it still answers
  3  a socket file nobody listens on is still replaced: a server starts on it
  4  no panic

Run: python3 tests/frame/server_start_keeps_live_socket.py
"""
import os
import socket
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import BIN, Client, Server, name_of  # noqa: E402

RUNDIR = "/tmp/rmx-startlive"
RUNDIR_STALE = "/tmp/rmx-startstale"

failures = []


def check(cond, label):
    print(("PASS  " if cond else "FAIL  ") + label)
    if not cond:
        failures.append(label)


def run_live(srv):
    ino = os.stat(srv.sock).st_ino
    second = subprocess.Popen([BIN, "server"], env=srv.env, stdout=subprocess.DEVNULL,
                              stderr=subprocess.PIPE)
    try:
        rc = second.wait(timeout=3.0)
        err = second.stderr.read().decode(errors="replace")
    except subprocess.TimeoutExpired:
        # Still running: it took the socket over and is serving on it.
        second.kill()
        second.wait()
        rc, err = None, ""
    check(rc not in (None, 0) and "already listening" in err,
          f"1 a second server refuses a live socket (rc={rc}, err={err.strip()!r})")
    check(os.path.exists(srv.sock) and os.stat(srv.sock).st_ino == ino,
          "2 the first server's socket is the same file")
    c = Client(srv.sock)
    c.hello()
    c.send("ListSessionTree")
    got = [name_of(m) for m in c.drain(1.0)]
    c.close()
    check("SessionTree" in got and srv.proc.poll() is None,
          f"2 and the first server still answers ({got})")


def run_stale(stale):
    for d in ("run", "state", "data", "config"):
        os.makedirs(f"{RUNDIR_STALE}/{d}", exist_ok=True)
    sock = f"{RUNDIR_STALE}/run/remux.sock"
    s = socket.socket(socket.AF_UNIX)
    s.bind(sock)
    s.close()
    env = {**stale.env} if stale.env else None
    p = subprocess.Popen([BIN, "server"], env=env, stdout=subprocess.DEVNULL,
                         stderr=subprocess.DEVNULL)
    stale.proc = p
    ok = False
    for _ in range(100):
        try:
            c = Client(sock)
            c.hello()
            c.close()
            ok = True
            break
        except OSError:
            time.sleep(0.05)
    check(ok and p.poll() is None, "3 a socket nobody listens on is replaced by a new server")


def main():
    srv = Server(RUNDIR)
    stale = Server(RUNDIR_STALE)
    logs = []
    try:
        srv.start()
        run_live(srv)
        # Server.start's env without launching, so the stale socket is ours to lay.
        stale.start()
        stale.kill()
        for f in os.listdir(f"{RUNDIR_STALE}/run"):
            os.unlink(f"{RUNDIR_STALE}/run/{f}")
        run_stale(stale)
    except Exception as e:  # noqa: BLE001 -- report, then reach the log check
        check(False, f"the run completed without an exception ({e!r})")
    finally:
        logs = [srv.log(), stale.log()]
        srv.kill()
        stale.kill()
    check(all("panicked at" not in lg for lg in logs), "4 no panic in the server logs")

    print()
    if failures:
        print(f"{len(failures)} FAILED:")
        for f in failures:
            print("  - " + f)
        return 1
    print("server_start_keeps_live_socket: all checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
