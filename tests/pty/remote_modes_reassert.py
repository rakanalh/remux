#!/usr/bin/env python3
"""The client sends its input modes again when a remote connects or drops.

A login banner, a ProxyCommand or a terminal reset can switch off mouse
tracking (1000) and SGR mouse (1006), and after that no click reaches the
client. ssh's own stderr can no longer do it, because the client drains it
into client.log (`tests/pty/ssh_stderr.py`), but anything else that reaches the
outer terminal still can, so the client sends its input modes again when a
remote connects and when one drops.

The fake `ssh` shim writes `CSI ? 1000 l` and `CSI ? 1006 l` to stderr before it
starts the relay, and again after the relay exits. Each write carries a marker.
The harness checks that neither reaches the client's output stream, that both
reach client.log escaped (which is what proves the shim ran at each point), and
that `CSI ? 1000 h` and `CSI ? 1006 h` are sent after the connect and after the
drop.

The connect is made with a `Prefix c` -> `RemoteConnect` binding once the
client is running.
An `auto_connect` remote would not discriminate: it dials before
`setup_terminal`, which sends the modes anyway.

What it covers:

  1  after the connect, mouse tracking and SGR mouse are re-sent after the
     shim's connect-time leak
  2  after the relay is killed (a dropped ssh), they are re-sent after the
     shim's drop-time leak
  3  the client stays alive and neither log has a panic

Run from the repo root:
    python3 tests/pty/remote_modes_reassert.py
"""
import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
from pty_harness import BIN, Tui  # noqa: E402
from rundirs import kill_relays, relay_pids, run_dir, stop_runtime  # noqa: E402

ROOT = run_dir("rmx-rmodes")
REMOTE = f"{ROOT}/remote"
BINDIR = f"{ROOT}/bin"
CONNECT_LEAK = b"LEAKCONNECT\x1b[?1000l\x1b[?1006l"
DROP_LEAK = b"LEAKDROP\x1b[?1000l\x1b[?1006l"
MODES = (("mouse tracking", b"\x1b[?1000h"), ("SGR mouse", b"\x1b[?1006h"))

CONFIG = """
[remotes.mini]
ssh = "whatever"
remux_path = "remux"

[keybindings.command]
c = "RemoteConnect mini"
"""

FAILURES = []


def check(name, cond, detail=""):
    if cond:
        print(f"  PASS  {name}")
    else:
        print(f"  FAIL  {name}\n        {detail}")
        FAILURES.append(name)


def remote_env():
    return {
        **os.environ,
        "HOME": f"{REMOTE}/home",
        "XDG_RUNTIME_DIR": f"{REMOTE}/run",
        "XDG_STATE_HOME": f"{REMOTE}/state",
        "XDG_DATA_HOME": f"{REMOTE}/data",
        "XDG_CONFIG_HOME": f"{REMOTE}/config",
        "SHELL": "/bin/sh",
        "ENV": "/dev/null",
    }


def write_shim():
    """The relay runs as a foreground child, not through `exec`, so the shim is
    still there to write the drop-time leak once the relay is killed."""
    os.makedirs(BINDIR, exist_ok=True)
    shim = f"{BINDIR}/ssh"
    with open(shim, "w") as f:
        f.write(
            "#!/bin/sh\n"
            "printf 'LEAKCONNECT\\033[?1000l\\033[?1006l\\n' >&2\n"
            f"export XDG_RUNTIME_DIR={REMOTE}/run\n"
            f"export XDG_STATE_HOME={REMOTE}/state\n"
            f"export XDG_DATA_HOME={REMOTE}/data\n"
            f"sh -c 'echo $$ >> {relay_pids()}; exec {BIN} relay'\n"
            "printf 'LEAKDROP\\033[?1000l\\033[?1006l\\n' >&2\n"
        )
    os.chmod(shim, 0o755)


def start_remote_server():
    for s in ("run", "state", "data", "config", "home"):
        os.makedirs(f"{REMOTE}/{s}", exist_ok=True)
    os.chmod(f"{REMOTE}/run", 0o700)
    p = subprocess.Popen([BIN, "server"], env=remote_env(),
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    sock = f"{REMOTE}/run/remux.sock"
    for _ in range(200):
        if os.path.exists(sock):
            time.sleep(0.3)
            return p
        time.sleep(0.05)
    p.kill()
    raise SystemExit("the remote server's socket never appeared")


def wait_for(t, needle, timeout=8.0):
    end = time.time() + timeout
    while time.time() < end:
        if bytes(t.raw).find(needle) >= 0:
            return True
        t.pump(0.2)
    return False


def escaped(leak):
    return leak.replace(b"\x1b", b"\\x1b")


def wait_for_log(t, needle, timeout=8.0):
    end = time.time() + timeout
    while time.time() < end:
        if needle in t.log("client"):
            return True
        t.pump(0.2)
    return False


def check_reasserted(t, label, leak, at):
    raw = bytes(t.raw)
    check(f"{label}: the shim's leak never reached the terminal", leak not in raw,
          raw[raw.find(leak) - 40:][:200])
    after = raw[at:]
    for name, seq in MODES:
        check(f"{label}: {name} is re-sent after the {label}", seq in after, after[:200])


def main():
    write_shim()
    remote = start_remote_server()
    t = Tui(f"{ROOT}/local", cols=100, rows=30, config=CONFIG,
            extra_env={"PATH": f"{BINDIR}:" + os.environ.get("PATH", "")}).start()
    try:
        t.pump(1.0)
        check("setup: no remote dialled before the binding", CONNECT_LEAK not in bytes(t.raw))

        at = len(t.raw)
        t.prefix(b"c", 0.5)
        check("connect: the shim was run",
              wait_for_log(t, escaped(CONNECT_LEAK).decode()))
        check("connect: the session manager lists the remote", wait_for(t, b"mini"))
        t.pump(1.5)
        check_reasserted(t, "connect", CONNECT_LEAK, at)

        at = len(t.raw)
        kill_relays(within=ROOT)
        # Written after the handshake, so it is logged in the summary that
        # follows ssh's stderr closing, not on its own line.
        check("drop: the shim reported the dropped relay",
              wait_for_log(t, escaped(DROP_LEAK).decode()))
        t.pump(1.5)
        check_reasserted(t, "drop", DROP_LEAK, at)

        check("client alive", t.alive())
        for which in ("client", "server"):
            check(f"no panic in {which}.log", "panicked at" not in t.log(which))
    finally:
        t.kill()
        kill_relays(within=ROOT)
        stop_runtime(f"{REMOTE}/run")
        try:
            remote.wait(timeout=5)
        except Exception:
            remote.kill()

    if FAILURES:
        print(f"\nFAILED: {len(FAILURES)}")
        for f in FAILURES:
            print(f"  - {f}")
        sys.exit(1)
    print("\nALL PASS")


if __name__ == "__main__":
    main()
