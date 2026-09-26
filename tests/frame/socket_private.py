#!/usr/bin/env python3
"""The server's socket is private to the user who runs it.

With no XDG runtime directory the socket used to go straight into `/tmp`,
shared with every other user, who could connect to it or create it first. It
now lives in `$TMPDIR/remux-<uid>/` (TMPDIR defaulting to /tmp), created 0700
and refused unless it is a real directory of ours closed to group and other.
The socket itself is 0600 wherever it is, and the server refuses a peer whose
uid is not its own (a unit test covers that seam: a second uid needs root).

  1  no XDG_RUNTIME_DIR: the directory is 0700, the socket 0600, and a client
     reaches the server through it
  2  XDG_RUNTIME_DIR set: the socket there is 0600 too
  3  a pre-existing fallback directory open to others is refused by the server
     and by a client, with a message naming it, and no socket is made
  4  a fallback directory that is a symlink is refused the same way
  5  no panic

Never touches the real /tmp/remux-<uid>: TMPDIR points under RUNDIR.

Run: python3 tests/frame/socket_private.py   (RMX_RUNDIR overrides the run dir)
"""
import os
import shutil
import socket
import stat
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import BIN  # noqa: E402

RUNDIR = os.environ.get("RMX_RUNDIR", "/tmp/rmx-sockperm")
UID = os.geteuid()

failures = []


def check(cond, label):
    print(("PASS  " if cond else "FAIL  ") + label)
    if not cond:
        failures.append(label)


def env_for(name, xdg_runtime=None):
    base = f"{RUNDIR}/{name}"
    for d in ("tmp", "state", "data", "config"):
        os.makedirs(f"{base}/{d}", exist_ok=True)
    env = {k: v for k, v in os.environ.items()
           if k not in ("XDG_RUNTIME_DIR", "REMUX_SESSION", "REMUX_PANE")}
    env.update(TMPDIR=f"{base}/tmp", XDG_STATE_HOME=f"{base}/state",
               XDG_DATA_HOME=f"{base}/data", XDG_CONFIG_HOME=f"{base}/config",
               SHELL="/bin/sh", ENV="/dev/null", TERM="xterm-256color")
    if xdg_runtime:
        os.makedirs(xdg_runtime, mode=0o700, exist_ok=True)
        env["XDG_RUNTIME_DIR"] = xdg_runtime
    return base, env


def mode(path):
    return stat.S_IMODE(os.lstat(path).st_mode)


def start(env, sock):
    p = subprocess.Popen([BIN, "server"], env=env, stdout=subprocess.DEVNULL,
                         stderr=subprocess.PIPE)
    for _ in range(100):
        if os.path.exists(sock) or p.poll() is not None:
            break
        time.sleep(0.05)
    time.sleep(0.2)
    return p


def stop(p):
    if p.poll() is None:
        p.kill()
    p.wait()


def read(path):
    return open(path, errors="ignore").read() if os.path.exists(path) else ""


shutil.rmtree(RUNDIR, ignore_errors=True)
logs = []

# -- 1 ---------------------------------------------------------------------------
base, env = env_for("fallback")
private = f"{base}/tmp/remux-{UID}"
sock = f"{private}/remux.sock"
p = start(env, sock)
try:
    check(os.path.exists(sock), f"1 with no XDG_RUNTIME_DIR the socket is in {private}")
    check(os.path.isdir(private) and mode(private) == 0o700,
          f"1 the directory is 0700 ({oct(mode(private)) if os.path.exists(private) else '-'})")
    check(os.path.exists(sock) and mode(sock) == 0o600,
          f"1 the socket is 0600 ({oct(mode(sock)) if os.path.exists(sock) else '-'})")
    r = subprocess.run([BIN, "ls"], env=env, capture_output=True, timeout=20)
    check(r.returncode == 0 and b"No sessions." in r.stdout,
          f"1 a client reaches the server through it (rc={r.returncode}, "
          f"err={r.stderr.decode(errors='replace').strip()!r})")
finally:
    stop(p)
    logs.append(read(f"{base}/state/remux/server.log"))

# -- 2 ---------------------------------------------------------------------------
base, env = env_for("xdg", xdg_runtime=f"{RUNDIR}/xdg/run")
sock = f"{RUNDIR}/xdg/run/remux.sock"
p = start(env, sock)
try:
    check(os.path.exists(sock) and mode(sock) == 0o600,
          f"2 with XDG_RUNTIME_DIR the socket there is 0600 "
          f"({oct(mode(sock)) if os.path.exists(sock) else '-'})")
finally:
    stop(p)
    logs.append(read(f"{base}/state/remux/server.log"))

# -- 3 ---------------------------------------------------------------------------
base, env = env_for("open")
private = f"{base}/tmp/remux-{UID}"
os.makedirs(private)
os.chmod(private, 0o755)
p = start(env, f"{private}/remux.sock")
try:
    err = p.stderr.read().decode(errors="replace") if p.poll() is not None else ""
    check(p.poll() not in (None, 0) and "refusing" in err and private in err,
          f"3 the server refuses a fallback directory open to others ({err.strip()!r})")
    check(not os.path.exists(f"{private}/remux.sock"), "3 and makes no socket in it")
    # Someone else's server in that directory: a client must not connect to it.
    # (A server that wrongly accepted the directory is stopped first, so this
    # case is judged on its own.)
    stop(p)
    if os.path.exists(f"{private}/remux.sock"):
        os.unlink(f"{private}/remux.sock")
    squatter = socket.socket(socket.AF_UNIX)
    squatter.bind(f"{private}/remux.sock")
    squatter.listen(4)
    squatter.settimeout(0.5)
    try:
        r = subprocess.run([BIN, "ls"], env=env, capture_output=True, timeout=5)
        rc, cerr = r.returncode, r.stderr.decode(errors="replace")
    except subprocess.TimeoutExpired:
        rc, cerr = None, "timed out waiting on the squatter"
    try:
        squatter.accept()
        reached = True
    except socket.timeout:
        reached = False
    squatter.close()
    check(rc not in (None, 0) and "refusing" in cerr and not reached,
          f"3 a client will not connect to a socket in it "
          f"(reached={reached}, err={cerr.strip()!r})")
finally:
    stop(p)
    logs.append(read(f"{base}/state/remux/server.log"))

# -- 4 ---------------------------------------------------------------------------
base, env = env_for("link")
target = f"{base}/elsewhere"
os.makedirs(target, mode=0o700)
os.symlink(target, f"{base}/tmp/remux-{UID}")
p = start(env, f"{base}/tmp/remux-{UID}/remux.sock")
try:
    err = p.stderr.read().decode(errors="replace") if p.poll() is not None else ""
    check(p.poll() not in (None, 0) and "not a directory" in err,
          f"4 a symlinked fallback directory is refused ({err.strip()!r})")
    check(not os.path.exists(f"{target}/remux.sock"), "4 and nothing is made behind the link")
finally:
    stop(p)
    logs.append(read(f"{base}/state/remux/server.log"))

check(all("panicked at" not in lg for lg in logs), "5 no panic in any server log")

print()
if failures:
    print(f"{len(failures)} FAILED:")
    for f in failures:
        print("  - " + f)
    sys.exit(1)
print("all checks passed")
