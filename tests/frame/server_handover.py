#!/usr/bin/env python3
"""One server handing the socket to the next never loses either of them.

  1  a server's shutdown leaves a socket at its path that it did not bind (the
     next server's) and a pid file that names another process; its own socket
     and pid file are removed as before
  2  a server that refuses a live socket ("already listening") spawns no pane:
     it refuses before restoring the sessions it would otherwise bring back
  3  `remux restart` whose stop times out fails, and does not say "Server
     restarted.": the old server still owns the socket and its shutdown is
     still to come
  4  `remux stop` with a stale pid file in front of a live server says so,
     and leaves the socket that server still listens on
  5  `remux stop` while the server is still restoring is a graceful save and
     exit once the restore is done: the pid file and the signal handlers
     come before the restore
  6  no panic

Run: python3 tests/frame/server_handover.py
"""
import os
import signal
import socket
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import BIN, Client, Server, name_of, remux_daemons  # noqa: E402

RUN = "/tmp/rmx-handover"

failures = []


def check(cond, label):
    print(("PASS  " if cond else "FAIL  ") + label)
    if not cond:
        failures.append(label)


def wait_for(cond, timeout=10.0):
    end = time.time() + timeout
    while time.time() < end and not cond():
        time.sleep(0.05)
    return cond()


def read(path):
    return open(path).read() if os.path.exists(path) else ""


servers = []
try:
    # --- 1 -----------------------------------------------------------------
    own = Server(f"{RUN}/own").start()
    servers.append(own)
    pid_file = f"{own.rundir}/run/remux.pid"
    own.proc.send_signal(signal.SIGTERM)
    own.proc.wait(timeout=15)
    check(not os.path.exists(own.sock) and not os.path.exists(pid_file),
          "1 a server's own socket and pid file are removed at shutdown")

    late = Server(f"{RUN}/late").start()
    servers.append(late)
    pid_file = f"{late.rundir}/run/remux.pid"
    # The next server, as far as the late one can tell: a new socket at the
    # same path, and a pid file naming another process.
    os.unlink(late.sock)
    successor = socket.socket(socket.AF_UNIX)
    successor.bind(late.sock)
    successor.listen(1)
    successor_ino = os.stat(late.sock).st_ino
    with open(pid_file, "w") as f:
        f.write(str(os.getpid()))
    late.proc.send_signal(signal.SIGTERM)
    late.proc.wait(timeout=15)
    check(os.path.exists(late.sock) and os.stat(late.sock).st_ino == successor_ino,
          "1 a late shutdown leaves the socket the next server bound")
    check(read(pid_file) == str(os.getpid()),
          f"1 ...and the pid file that names the next server ({read(pid_file)!r})")
    successor.close()

    # --- 2 -----------------------------------------------------------------
    rundir = f"{RUN}/restore"
    marker = f"{RUN}/restore-shells"
    shell = f"{RUN}/marking-shell"
    os.makedirs(RUN, exist_ok=True)
    if os.path.exists(marker):
        os.remove(marker)
    with open(shell, "w") as f:
        f.write(f"#!/bin/sh\necho spawned >> '{marker}'\nexec /bin/sh \"$@\"\n")
    os.chmod(shell, 0o755)
    os.environ["REMUX_TEST_SHELL"] = shell
    first = Server(rundir).start()
    servers.append(first)
    c = Client(first.sock)
    c.hello()
    c.send({"CreateSession": {"name": "keep", "folder": None}})
    c.drain(0.8)
    c.close()
    spawned = lambda: read(marker).count("spawned")  # noqa: E731
    wait_for(lambda: spawned() >= 1)
    first.stop()
    first.start_again()
    restored = wait_for(lambda: spawned() >= 2)
    check(restored, f"2 a restarted server restores the session and spawns its shell ({spawned()} shells)")
    # Counted in the server log, which both servers append to: a server that
    # spawns a pane and then exits at once can take the pane's shell down
    # before it runs a line, so the marker alone would miss the spawn.
    spawn_line = "pty: child process spawned"
    log = f"{rundir}/state/remux/server.log"
    spawns_before = read(log).count(spawn_line)
    second = subprocess.run([BIN, "server"], env=first.env, capture_output=True, text=True, timeout=30)
    time.sleep(1.0)
    check(second.returncode != 0 and "already listening" in second.stderr,
          f"2 a second server refuses the live socket (rc {second.returncode})")
    spawns = read(log).count(spawn_line) - spawns_before
    check(spawns == 0 and spawned() == 2,
          f"2 ...and spawned no pane before refusing ({spawns} spawned, {spawned()} shells ran, expected 0 and 2)")
    del os.environ["REMUX_TEST_SHELL"]

    # --- 3 -----------------------------------------------------------------
    stuck = Server(f"{RUN}/stuck").start()
    servers.append(stuck)
    stuck.proc.send_signal(signal.SIGSTOP)
    restart = subprocess.run([BIN, "restart"], env=stuck.env, capture_output=True, text=True, timeout=60)
    check(restart.returncode != 0 and "Server restarted." not in restart.stdout
          and "did not exit" in restart.stderr,
          f"3 a restart whose stop timed out fails (rc {restart.returncode}, out {restart.stdout!r}, "
          f"err {restart.stderr.strip()[-160:]!r})")
    others = [p for p in remux_daemons(stuck.rundir, kinds=(b"server",)) if p != stuck.proc.pid]
    check(not others, f"3 ...and starts no server beside the stopped one ({others})")
    stuck.proc.send_signal(signal.SIGCONT)
    stuck.proc.wait(timeout=15)

    # --- 4 -----------------------------------------------------------------
    live = Server(f"{RUN}/stalepid").start()
    servers.append(live)
    gone = subprocess.Popen(["true"])
    gone.wait()
    with open(f"{live.rundir}/run/remux.pid", "w") as f:
        f.write(str(gone.pid))
    ino = os.stat(live.sock).st_ino
    stop = subprocess.run([BIN, "stop"], env=live.env, capture_output=True, text=True, timeout=30)
    check(stop.returncode != 0 and "pid file is stale" in stop.stderr,
          f"4 `remux stop` with a stale pid file in front of a live server says so (rc {stop.returncode}, "
          f"out {stop.stdout!r}, err {stop.stderr.strip()[-160:]!r})")
    answered = False
    if os.path.exists(live.sock) and os.stat(live.sock).st_ino == ino:
        c = Client(live.sock)
        c.hello()
        c.send("ListSessionTree")
        answered = "SessionTree" in [name_of(m) for m in c.drain(1.0)]
        c.close()
    check(live.proc.poll() is None and answered,
          "4 ...and leaves the live server's socket, which still answers")

    # --- 5 -----------------------------------------------------------------
    # state.json as a FIFO holds the server inside its restore (load_state
    # blocks on the read) for as long as this harness likes; the save at
    # shutdown renames a new file over it, so it cannot block there.
    win = Server(f"{RUN}/restorewin").start()
    servers.append(win)
    c = Client(win.sock)
    c.hello()
    c.send({"CreateSession": {"name": "window", "folder": None}})
    c.drain(0.8)
    c.close()
    win.stop()
    state = f"{win.rundir}/data/remux/state.json"
    saved = open(state, "rb").read()
    os.remove(state)
    os.mkfifo(state)
    win.start_again()
    wait_for(lambda: os.path.exists(f"{win.rundir}/run/remux.pid"), 3.0)
    stopper = subprocess.Popen([BIN, "stop"], env=win.env, stdout=subprocess.PIPE,
                               stderr=subprocess.PIPE, text=True)
    time.sleep(0.5)
    fed = False
    end = time.time() + 5
    while time.time() < end and not fed:
        try:
            fd = os.open(state, os.O_WRONLY | os.O_NONBLOCK)
        except OSError:
            time.sleep(0.05)
            continue
        os.set_blocking(fd, True)
        os.write(fd, saved)
        os.close(fd)
        fed = True
    out, err = stopper.communicate(timeout=30)
    exited = wait_for(lambda: win.proc.poll() is not None, 10.0)
    after = open(state).read() if os.path.isfile(state) else ""
    check(fed and stopper.returncode == 0 and "Server stopped." in out and exited and '"window"' in after,
          f"5 `remux stop` during the restore is a graceful save once the restore is done "
          f"(fed {fed}, rc {stopper.returncode}, out {out.strip()!r}, exited {exited}, saved {'window' in after})")
except Exception as e:  # noqa: BLE001
    check(False, f"the run completed without an exception ({e!r})")
finally:
    for s in servers:
        try:
            s.proc.send_signal(signal.SIGCONT)
        except Exception:  # noqa: BLE001
            pass
        s.kill()

logs = [read(f"{s.rundir}/state/remux/server.log") for s in servers]
check(all("panicked at" not in lg for lg in logs), "6 no panic in the server logs")

if failures:
    print(f"\n{len(failures)} FAILED:")
    for f in failures:
        print(f"  - {f}")
    sys.exit(1)
print("\nserver_handover: all checks passed")
