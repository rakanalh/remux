#!/usr/bin/env python3
"""A relative XDG_RUNTIME_DIR is ignored, as every other XDG variable is.

The XDG spec says a relative value is invalid, and `dirs` drops one on Linux,
but the socket resolver used to fall back to the RAW variable. So a relative
value was resolved against the process's working directory: the server made
the directory there and bound its socket in it, a client started elsewhere
looked somewhere else, and a `..` could climb to any directory, the user's
real runtime dir included. A relative or empty value must now behave exactly
as an unset one: the socket goes to `$TMPDIR/remux-<uid>/`.

  1  a server started with XDG_RUNTIME_DIR=run binds in $TMPDIR/remux-<uid>/
     and creates nothing under its working directory
  2  a client with the same relative value, from ANOTHER directory, reaches
     that server (it lists a session created on the fallback socket)
  3  `remux stop` with the same relative value, from another directory, stops
     that server
  4  a `..`-climbing value aimed at a throwaway sentinel directory creates and
     binds nothing there; the socket is in the fallback
  5  an empty value uses the fallback too
  6  no panic in any server log

Every relative value resolves inside RUNDIR, never toward /run/user/<uid> or
the real XDG_RUNTIME_DIR, and TMPDIR is a private directory under RUNDIR.
Servers are stopped by their own process handle; nothing is found by search.

Run: python3 tests/frame/relative_runtime_dir.py   (RMX_RUNDIR overrides the run dir)
"""
import json
import os
import shutil
import socket
import struct
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import BIN, PROTOCOL_VERSION  # noqa: E402
from rundirs import run_dir  # noqa: E402

RUNDIR = os.path.normpath(os.environ.get("RMX_RUNDIR") or run_dir("rmx-relrun"))
UID = os.geteuid()
if not os.path.isabs(RUNDIR) or RUNDIR.count("/") < 2:
    raise SystemExit(f"refusing RMX_RUNDIR={RUNDIR!r}: not a dedicated absolute dir")
for real in (os.environ.get("XDG_RUNTIME_DIR", ""), f"/run/user/{UID}"):
    real = os.path.normpath(real) if real else ""
    if real and (RUNDIR == real or RUNDIR.startswith(real + "/")
                 or real.startswith(RUNDIR + "/")):
        raise SystemExit(f"refusing RMX_RUNDIR={RUNDIR!r}: it overlaps {real}")

TMP = f"{RUNDIR}/tmp"
FALLBACK = f"{TMP}/remux-{UID}"
SOCK = f"{FALLBACK}/remux.sock"
PIDFILE = f"{FALLBACK}/remux.pid"

failures = []


def check(cond, label):
    print(("PASS  " if cond else "FAIL  ") + label)
    if not cond:
        failures.append(label)


def mkdir(path):
    os.makedirs(path, mode=0o700, exist_ok=True)
    return path


def env_with(runtime):
    env = {k: v for k, v in os.environ.items()
           if k not in ("XDG_RUNTIME_DIR", "TMPDIR", "REMUX_SESSION", "REMUX_PANE")}
    env.update(TMPDIR=TMP, XDG_STATE_HOME=f"{RUNDIR}/state", XDG_DATA_HOME=f"{RUNDIR}/data",
               XDG_CONFIG_HOME=f"{RUNDIR}/config", HOME=f"{RUNDIR}/home", SHELL="/bin/sh",
               ENV="/dev/null", TERM="xterm-256color", XDG_RUNTIME_DIR=runtime)
    return env


def contents(path):
    """Every path under `path`, relative to it, so a nested socket shows."""
    found = []
    for dirpath, dirnames, files in os.walk(path):
        for name in dirnames + files:
            found.append(os.path.relpath(os.path.join(dirpath, name), path))
    return sorted(found)


def start(label, env, cwd, wrong_sock):
    """`remux server` from `cwd`, waited on until it binds the fallback socket
    or the socket the bug would bind, or exits.

    A socket already in the fallback would let the "socket is in the fallback"
    checks pass on a file that a crashed earlier server left there."""
    check(not os.path.exists(SOCK), f"{label} no socket is in the fallback before the start")
    p = subprocess.Popen([BIN, "server"], env=env, cwd=cwd, stdout=subprocess.DEVNULL,
                         stderr=subprocess.PIPE)
    for _ in range(100):
        if os.path.exists(SOCK) or os.path.exists(wrong_sock) or p.poll() is not None:
            break
        time.sleep(0.05)
    time.sleep(0.2)
    return p


def owns_fallback(p):
    """Whether the fallback's pid file names `p`, waited for. The server
    writes it after it binds, so once it names `p` the server has made every
    directory it will make, and an empty cwd is evidence rather than timing."""
    for _ in range(100):
        try:
            if open(PIDFILE).read().strip() == str(p.pid):
                return True
        except OSError:
            pass
        if p.poll() is not None:
            return False
        time.sleep(0.05)
    return False


def stop(p):
    if p.poll() is None:
        p.terminate()
        try:
            p.wait(timeout=10)
        except subprocess.TimeoutExpired:
            p.kill()
    p.wait()


def send(s, obj):
    body = json.dumps(obj).encode()
    s.sendall(struct.pack(">I", len(body)) + body)


def recv(s, buf):
    while len(buf[0]) < 4:
        buf[0] += s.recv(65536)
    n = struct.unpack(">I", buf[0][:4])[0]
    while len(buf[0]) < 4 + n:
        buf[0] += s.recv(65536)
    body, buf[0] = buf[0][4:4 + n], buf[0][4 + n:]
    return json.loads(body)


def create_session(path, name):
    s = socket.socket(socket.AF_UNIX)
    s.settimeout(3.0)
    try:
        s.connect(path)
        buf = [b""]
        send(s, {"protocol_version": PROTOCOL_VERSION, "remux_version": "t"})
        recv(s, buf)
        send(s, {"CreateSession": {"name": name, "folder": None}})
        time.sleep(0.5)
        return True
    except OSError as e:
        print(f"      could not create a session on {path}: {e!r}")
        return False
    finally:
        s.close()


def log_text():
    path = f"{RUNDIR}/state/remux/server.log"
    return open(path, errors="ignore").read() if os.path.exists(path) else ""


shutil.rmtree(RUNDIR, ignore_errors=True)
for d in ("tmp", "state", "data", "config", "home"):
    mkdir(f"{RUNDIR}/{d}")

# -- 1-3: XDG_RUNTIME_DIR=run ---------------------------------------------------
start_cwd = mkdir(f"{RUNDIR}/cwd-start")
other_cwd = mkdir(f"{RUNDIR}/cwd-other")
env = env_with("run")
p = start("1", env, start_cwd, f"{start_cwd}/run/remux.sock")
try:
    check(os.path.exists(SOCK), f"1 the server binds in the fallback {FALLBACK}")
    check(contents(start_cwd) == [],
          f"1 nothing is created under the server's cwd ({contents(start_cwd)})")
    made = os.path.exists(SOCK) and create_session(SOCK, "relrun")
    r = subprocess.run([BIN, "ls"], env=env, cwd=other_cwd, capture_output=True, timeout=20)
    out = r.stdout.decode(errors="replace")
    check(made and r.returncode == 0 and "relrun" in out,
          f"2 a client from another cwd reaches that server (rc={r.returncode}, "
          f"out={out.strip()!r}, err={r.stderr.decode(errors='replace').strip()!r})")
    r = subprocess.run([BIN, "stop"], env=env, cwd=other_cwd, capture_output=True, timeout=20)
    try:
        p.wait(timeout=10)
    except subprocess.TimeoutExpired:
        pass
    stop_out = r.stdout.decode(errors="replace")
    check(p.poll() is not None and "Server stopped." in stop_out,
          f"3 `remux stop` from another cwd stops that server "
          f"(rc={r.returncode}, out={stop_out.strip()!r}, exited={p.poll() is not None})")
    check(contents(other_cwd) == [],
          f"2/3 nothing is created under the client's cwd ({contents(other_cwd)})")
finally:
    stop(p)

# -- 4: a value that climbs to a sentinel ----------------------------------------
sentinel = mkdir(f"{RUNDIR}/sentinel")
deep = mkdir(f"{RUNDIR}/deep/a")
climb = "../../sentinel"
assert os.path.normpath(os.path.join(deep, climb)) == sentinel
p = start("4", env_with(climb), deep, f"{sentinel}/remux.sock")
try:
    check(owns_fallback(p), f"4 the fallback's pid file names this server (pid {p.pid})")
    check(os.path.exists(SOCK), f"4 with XDG_RUNTIME_DIR={climb} the socket is in the fallback")
    check(contents(sentinel) == [],
          f"4 nothing is created or bound in the sentinel ({contents(sentinel)})")
    check(contents(deep) == [], f"4 nothing is created under the cwd ({contents(deep)})")
finally:
    stop(p)

# -- 5: an empty value -------------------------------------------------------------
empty_cwd = mkdir(f"{RUNDIR}/cwd-empty")
p = start("5", env_with(""), empty_cwd, f"{empty_cwd}/remux.sock")
try:
    check(owns_fallback(p), f"5 the fallback's pid file names this server (pid {p.pid})")
    err = p.stderr.read().decode(errors="replace") if p.poll() is not None else ""
    check(os.path.exists(SOCK) and p.poll() is None,
          f"5 with XDG_RUNTIME_DIR='' the server runs on the fallback ({err.strip()!r})")
    check(contents(empty_cwd) == [], f"5 nothing is created under the cwd ({contents(empty_cwd)})")
finally:
    stop(p)

check("panicked at" not in log_text(), "6 no panic in the server log")

print()
if failures:
    print(f"{len(failures)} FAILED:")
    for f in failures:
        print("  - " + f)
    sys.exit(1)
print("all checks passed")
