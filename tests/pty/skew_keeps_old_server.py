#!/usr/bin/env python3
"""A server from another protocol version is LEFT RUNNING, never replaced.

`ensure_server_running` used to treat ANY failure of its probe connection as a
stale socket, and the probe includes the handshake. So after an upgrade that
bumps the protocol, the next `remux` or `remux relay` met the user's LIVE old
server, failed the handshake, unlinked its socket and spawned an empty server,
and the old one kept running with every session in it, unreachable.

Only "nothing is listening" (connection refused, no such file) is stale. A
server that answers is left alone, and the version mismatch is reported.

Against a v11 stand-in (`FakeServer`) that owns the socket:
  1  `remux` (the interactive entry point) fails with "incompatible remux" and
     says `remux restart`; the socket is still the fake's; nothing was spawned
  2  `remux relay` leaves the socket alone, spawns nothing, and pumps the
     handshake through: the far end reads the v11 Welcome itself, which is how
     the far client comes to report "incompatible remux"
  3  a socket file nobody listens on IS stale: `remux` replaces it with a
     server of its own (the path the fix must keep)

With `REMUX_OLD_BIN` set to a binary built at an older protocol, also against
a GENUINELY old server:
  4  `remux` refuses it the same way, and the old server keeps running
  5  `remux restart` stops it (by pid, through its save-and-exit path) and
     starts a current server that restores the old one's session

Run: python3 tests/pty/skew_keeps_old_server.py
     REMUX_OLD_BIN=/path/to/old/remux python3 tests/pty/skew_keeps_old_server.py
"""
import json
import os
import select
import shutil
import socket
import struct
import subprocess
import sys
import time

for _var in ("REMUX_SESSION", "REMUX_PANE", "REMUX"):
    os.environ.pop(_var, None)
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from fake_server import FakeServer  # noqa: E402
from pty_harness import BIN, PROTOCOL_VERSION, guard_rundir, reap_rundir  # noqa: E402

RUN = "/tmp/rmx-skew"
guard_rundir(RUN)
# The protocol the stand-in speaks: hard-coded on purpose, since this is the
# skew test and it must not be the current one. The check below keeps it
# honest if the real version ever reaches it.
SKEWED = 11
if PROTOCOL_VERSION == SKEWED:
    raise SystemExit(f"PROTOCOL_VERSION is {SKEWED}: pick another SKEWED for the stand-in")
OLD_BIN = os.environ.get("REMUX_OLD_BIN")

FAILURES = []


def check(cond, label):
    print(("PASS  " if cond else "FAIL  ") + label)
    if not cond:
        FAILURES.append(label)


def env_for(name):
    base = f"{RUN}/{name}"
    for d in ("run", "state", "data", "config", "home"):
        os.makedirs(f"{base}/{d}", exist_ok=True)
    return base, {
        **os.environ,
        "XDG_RUNTIME_DIR": f"{base}/run",
        "XDG_STATE_HOME": f"{base}/state",
        "XDG_DATA_HOME": f"{base}/data",
        "XDG_CONFIG_HOME": f"{base}/config",
        "HOME": f"{base}/home",
        "SHELL": "/bin/sh",
        "ENV": "/dev/null",
        "TERM": "xterm-256color",
    }


def read(path):
    return open(path, errors="ignore").read() if os.path.exists(path) else ""


def frame(obj):
    b = json.dumps(obj).encode()
    return struct.pack(">I", len(b)) + b


def read_frame(fd, timeout=5.0):
    end = time.time() + timeout
    buf = b""
    while time.time() < end:
        ready, _, _ = select.select([fd], [], [], max(0.0, end - time.time()))
        if not ready:
            break
        chunk = os.read(fd, 65536)
        if not chunk:
            break
        buf += chunk
        if len(buf) >= 4:
            n = struct.unpack(">I", buf[:4])[0]
            if len(buf) >= 4 + n:
                return json.loads(buf[4:4 + n])
    return None


def remux_pids(runtime_dir):
    """Every `remux server` process whose environment names `runtime_dir`."""
    pids = []
    for pid in os.listdir("/proc"):
        if not pid.isdigit():
            continue
        try:
            cmd = open(f"/proc/{pid}/cmdline", "rb").read().split(b"\0")
            envb = open(f"/proc/{pid}/environ", "rb").read()
        except OSError:
            continue
        if len(cmd) >= 2 and cmd[1] == b"server" \
                and f"XDG_RUNTIME_DIR={runtime_dir}".encode() in envb.split(b"\0"):
            pids.append(int(pid))
    return pids


def stop_all(runtime_dir):
    reap_rundir(runtime_dir)


shutil.rmtree(RUN, ignore_errors=True)

# -- 1 and 2: a v11 stand-in owns the socket --------------------------------
base, env = env_for("fake")
sock = f"{base}/run/remux.sock"
fake = FakeServer(sock, protocol_version=SKEWED).start()
try:
    ino = os.stat(sock).st_ino
    p = subprocess.run([BIN], env=env, capture_output=True, timeout=30,
                       stdin=subprocess.DEVNULL)
    err = p.stderr.decode(errors="replace")
    check(p.returncode != 0 and "incompatible remux" in err and "remux restart" in err,
          f"1 `remux` against a v11 server fails, names the skew and `remux restart` "
          f"(rc={p.returncode}, err={err.strip()!r})")
    check(os.path.exists(sock) and os.stat(sock).st_ino == ino,
          "1 the v11 server's socket is still there, the same file")
    clog = read(f"{base}/state/remux/client.log")
    check("spawning server" not in clog and not remux_pids(f"{base}/run"),
          "1 no server was spawned in its place")

    relay = subprocess.Popen([BIN, "relay"], env=env, stdin=subprocess.PIPE,
                             stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    relay.stdin.write(frame({"protocol_version": PROTOCOL_VERSION,
                             "remux_version": "far-client"}))
    relay.stdin.flush()
    welcome = read_frame(relay.stdout.fileno())
    relay.kill()
    relay.wait()
    check(welcome is not None and welcome.get("protocol_version") == SKEWED,
          f"2 `remux relay` pumps the far client's handshake to the v11 server ({welcome})")
    check(os.path.exists(sock) and os.stat(sock).st_ino == ino,
          "2 and leaves its socket alone")
    rlog = read(f"{base}/state/remux/relay.log")
    check("spawning server" not in rlog and not remux_pids(f"{base}/run"),
          "2 and spawns nothing")
finally:
    fake.close()
    stop_all(f"{base}/run")

# -- 3: a socket file nobody listens on is stale -------------------------------
base, env = env_for("stale")
sock = f"{base}/run/remux.sock"
s = socket.socket(socket.AF_UNIX)
s.bind(sock)
s.close()
try:
    p = subprocess.run([BIN, "relay"], env=env, capture_output=True, timeout=30,
                       stdin=subprocess.DEVNULL)
    rlog = read(f"{base}/state/remux/relay.log")
    check("spawning server" in rlog and remux_pids(f"{base}/run"),
          f"3 a socket nobody listens on is replaced by a server "
          f"(rc={p.returncode}, err={p.stderr.decode(errors='replace').strip()!r})")
finally:
    subprocess.run([BIN, "stop"], env=env, capture_output=True, timeout=20)
    stop_all(f"{base}/run")

# -- 4 and 5: a genuinely old server --------------------------------------------
if OLD_BIN:
    base, env = env_for("old")
    sock = f"{base}/run/remux.sock"
    old = subprocess.Popen([OLD_BIN, "server"], env=env, stdout=subprocess.DEVNULL,
                           stderr=subprocess.DEVNULL)
    try:
        for _ in range(200):
            if os.path.exists(sock):
                break
            time.sleep(0.05)
        time.sleep(0.3)
        c = socket.socket(socket.AF_UNIX)
        c.connect(sock)
        c.sendall(frame({"protocol_version": SKEWED, "remux_version": "t"}))
        time.sleep(0.2)
        c.sendall(frame({"CreateSession": {"name": "keepme", "folder": None}}))
        time.sleep(0.6)
        c.close()
        ino = os.stat(sock).st_ino

        p = subprocess.run([BIN], env=env, capture_output=True, timeout=30,
                           stdin=subprocess.DEVNULL)
        err = p.stderr.decode(errors="replace")
        check(p.returncode != 0 and "incompatible remux" in err,
              f"4 `remux` refuses the genuinely old server (err={err.strip()!r})")
        check(old.poll() is None and os.stat(sock).st_ino == ino
              and remux_pids(f"{base}/run") == [old.pid],
              "4 which keeps running on its own socket, alone")

        p = subprocess.run([BIN, "restart"], env=env, capture_output=True, timeout=30,
                           stdin=subprocess.DEVNULL)
        out = p.stdout.decode(errors="replace")
        old_rc = old.wait(timeout=10)
        pids = remux_pids(f"{base}/run")
        check(p.returncode == 0 and "Server restarted" in out and len(pids) == 1
              and pids[0] != old.pid,
              f"5 `remux restart` stops the old server (rc {old_rc}) and starts a "
              f"current one (out={out.strip()!r}, pids={pids})")
        p = subprocess.run([BIN, "ls"], env=env, capture_output=True, timeout=30,
                           stdin=subprocess.DEVNULL)
        check("keepme" in p.stdout.decode(errors="replace"),
              f"5 and the old server's session is restored "
              f"({p.stdout.decode(errors='replace').strip()!r})")
    finally:
        if old.poll() is None:
            old.kill()
        subprocess.run([BIN, "stop"], env=env, capture_output=True, timeout=20)
        stop_all(f"{base}/run")
else:
    print("NOTE  4-5 need REMUX_OLD_BIN (a binary built at an older protocol); not run")

for d in ("fake", "stale", "old"):
    for which in ("server", "client", "relay"):
        log = read(f"{RUN}/{d}/state/remux/{which}.log")
        check("panicked at" not in log, f"no panic in {d}/{which}.log")

print()
if FAILURES:
    print(f"{len(FAILURES)} FAILED:")
    for f in FAILURES:
        print("  - " + f)
    sys.exit(1)
print("all checks passed")
