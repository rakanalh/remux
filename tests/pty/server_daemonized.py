"""The auto-spawned server is DETACHED and survives its terminal closing.

The reported bug: "I accidentally closed alacritty and therefore, remux somehow
closed. When I opened another terminal, we somehow did NOT resume the existing
server instance."

`ensure_server_running` used to spawn the server with null stdio and nothing
else, which LOOKS like daemonizing and is not -- the child inherited the
client's process group, session and controlling terminal and sat in the
foreground group, so closing the terminal SIGHUPed the whole group and took the
server with it. The fix is `setsid()` in the child via `pre_exec`.

No unit test can see any of this: it is a property of the spawned process as
the OS sees it, so every assertion here reads `/proc` or signals for real.

Three cases:

  1. The shape that makes it survivable -- the server's session id differs from
     the client's, its process group differs, and it has NO controlling
     terminal (`tty_nr == 0`).
  2. It stays terminal-free once panes exist. `setsid` alone does not promise
     this: a session leader is exactly the process that CAN acquire a ctty by
     opening a tty without `O_NOCTTY`. This case is only meaningful because the
     server under test IS a session leader -- the same assertion against a
     hand-started non-leader server passes while testing nothing.
  3. The decisive one: SIGHUP the client's whole process group, as closing a
     terminal emulator does, and prove the server is still alive AND still
     serving AND still holding the session. Case 3 asserts the CLIENT DIED
     first -- without that, "the server survived" is equally consistent with
     the signal never having been delivered, which is the false-green this
     whole test exists to avoid.

Run: python3 tests/pty/server_daemonized.py
"""
import json
import os
import signal
import socket
import struct
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from pty_harness import PROTOCOL_VERSION, Tui  # noqa: E402

RUNDIR = "/tmp/rmx_daemonized"
SOCK = f"{RUNDIR}/run/remux.sock"
PIDFILE = f"{RUNDIR}/run/remux.pid"

failures = []


def check(label, ok, detail=""):
    print(f"{'PASS' if ok else 'FAIL'}  {label}" + (f"  -- {detail}" if detail else ""))
    if not ok:
        failures.append(label)


# --------------------------------------------------------------------------
# /proc primitives
# --------------------------------------------------------------------------

def proc_stat(pid):
    """The session/pgrp/tty facts for a pid, or None if it is gone.

    `comm` is parenthesised and may itself contain spaces and parens, so the
    fields are split from after the LAST ')' -- splitting on whitespace from
    the left mis-parses any process whose name has a space in it.
    """
    try:
        with open(f"/proc/{pid}/stat") as f:
            raw = f.read()
    except (FileNotFoundError, ProcessLookupError):
        return None
    rest = raw[raw.rindex(")") + 2:].split()
    return {
        "state": rest[0],
        "ppid": int(rest[1]),
        "pgrp": int(rest[2]),
        "session": int(rest[3]),
        "tty_nr": int(rest[4]),
    }


def alive(pid):
    """Live and not a zombie.

    A reaped-but-not-waited child still has a `/proc/<pid>` entry, so an
    existence check alone would call a dead server alive.
    """
    st = proc_stat(pid)
    return st is not None and st["state"] != "Z"


def server_pid():
    for _ in range(50):
        try:
            with open(PIDFILE) as f:
                return int(f.read().strip())
        except (FileNotFoundError, ValueError):
            time.sleep(0.1)
    raise SystemExit(f"server pid file never appeared at {PIDFILE}")


# --------------------------------------------------------------------------
# A direct socket probe: does the server still SERVE, not merely exist?
# --------------------------------------------------------------------------

def talk(messages, expect):
    """Hello/Welcome, then send `messages` and return the first `expect` reply.

    Returns None if the socket cannot be reached or the exchange fails, so a
    dead server is a clean False rather than an exception.
    """
    try:
        s = socket.socket(socket.AF_UNIX)
        s.settimeout(3.0)
        s.connect(SOCK)
    except OSError:
        return None
    buf = bytearray()

    def send(obj):
        b = json.dumps(obj).encode()
        s.sendall(struct.pack(">I", len(b)) + b)

    def recv():
        while len(buf) < 4:
            chunk = s.recv(65536)
            if not chunk:
                raise OSError("closed")
            buf.extend(chunk)
        n = struct.unpack(">I", bytes(buf[:4]))[0]
        while len(buf) < 4 + n:
            chunk = s.recv(65536)
            if not chunk:
                raise OSError("closed")
            buf.extend(chunk)
        body = bytes(buf[4:4 + n])
        del buf[:4 + n]
        return json.loads(body)

    try:
        # PROTOCOL_VERSION is read out of src/protocol.rs by the harness, never
        # restated here: the server only LOGS a skew and proceeds, so a stale
        # hard-coded copy would keep passing against a protocol that no longer
        # exists.
        send({"protocol_version": PROTOCOL_VERSION, "remux_version": "daemonized-test"})
        # `Welcome` is a bare struct, not a `ServerMessage` variant, so it
        # arrives UNWRAPPED -- `{"protocol_version": N, ...}`, not
        # `{"Welcome": {...}}`. Checking for the wrapper instead is a silent
        # way to make every probe here return None.
        welcome = recv()
        if not isinstance(welcome, dict) or "protocol_version" not in welcome:
            return None
        for m in messages:
            send(m)
        deadline = time.time() + 3.0
        while time.time() < deadline:
            msg = recv()
            if isinstance(msg, dict) and expect in msg:
                return msg[expect]
        return None
    except (OSError, ValueError):
        return None
    finally:
        s.close()


def session_names():
    """Every live session name the server reports, folders included."""
    tree = talk(["ListSessionTree"], "SessionTree")
    if tree is None:
        return None
    names = [e["name"] for e in tree.get("unfiled", [])]
    for folder in tree.get("folders", []):
        names += [e["name"] for e in folder.get("sessions", [])]
    return names


def stop_server():
    """Never leave a detached server behind.

    This is the flip side of the fix: the harnesses used to be cleaned up BY
    the bug -- pexpect tearing down its PTY HUPed the shared-group server. A
    detached server outlives the harness, so teardown has to be explicit, and
    has to run on the red path too.
    """
    try:
        pid = int(open(PIDFILE).read().strip())
    except (FileNotFoundError, ValueError):
        return
    try:
        os.kill(pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    for _ in range(50):
        if not alive(pid):
            return
        time.sleep(0.1)
    try:
        os.kill(pid, signal.SIGKILL)
    except ProcessLookupError:
        pass


# --------------------------------------------------------------------------

def main():
    t = Tui(RUNDIR, cols=100, rows=30)
    try:
        t.start()
        cli = t.child.pid
        srv = server_pid()
        # The server has to be up and answering before any of this means
        # anything -- a pid file written by a process that then died would
        # sail through the /proc checks below.
        time.sleep(0.5)
        check("server is serving before the HUP", session_names() is not None)

        cs, ss = proc_stat(cli), proc_stat(srv)
        if cs is None or ss is None:
            check("client and server are both running", False, f"client={cs} server={ss}")
            return
        print(f"      client {cli}: {cs}")
        print(f"      server {srv}: {ss}")

        # --- Case 1: the detached shape ---------------------------------
        check("server is in its OWN session (not the client's)",
              ss["session"] != cs["session"],
              f"server sid={ss['session']} client sid={cs['session']}")
        check("server is a session LEADER",
              ss["session"] == srv,
              f"server sid={ss['session']} pid={srv}")
        check("server is in its OWN process group",
              ss["pgrp"] != cs["pgrp"],
              f"server pgrp={ss['pgrp']} client pgrp={cs['pgrp']}")
        check("server has NO controlling terminal",
              ss["tty_nr"] == 0, f"tty_nr={ss['tty_nr']}")

        # --- Case 2: still terminal-free once a pane exists --------------
        # The client's own attach already made a pane; drive a command
        # through it so the pane is demonstrably live rather than merely
        # allocated, then re-read. Meaningful ONLY because the server is a
        # session leader (asserted above) -- a non-leader cannot acquire a
        # ctty whatever it opens.
        t.send(b"printf 'PANE:%s\\n' live\r", t=1.0)
        saw_pane = any("PANE:live" in r for r in t.rows_text())
        check("a pane really ran a command (case 2 has something to test)",
              saw_pane)
        ss2 = proc_stat(srv)
        check("server still has NO controlling terminal after a pane exists",
              ss2 is not None and ss2["tty_nr"] == 0,
              f"tty_nr={ss2 and ss2['tty_nr']}")

        # A session whose survival we can name afterwards.
        created = talk([{"CreateSession": {"name": "survivor", "folder": None}},
                        "ListSessionTree"], "SessionTree")
        check("named session 'survivor' exists before the HUP",
              created is not None and "survivor" in (session_names() or []))

        # --- Case 3: the decisive one -----------------------------------
        # pexpect's child is a session leader, so its pgid == its pid. This
        # is the signal a terminal emulator sends its foreground group when
        # its window closes.
        pgid = os.getpgid(cli)
        print(f"      SIGHUP -> process group {pgid}")
        os.killpg(pgid, signal.SIGHUP)
        time.sleep(1.5)

        # THE guard against a meaningless pass: if the client is still alive
        # the signal never landed, and "the server survived" would prove
        # nothing at all.
        # If this one fails, every check below it is worthless rather than
        # merely wrong: an undelivered signal is entirely consistent with the
        # server "surviving".
        check("the SIGHUP was actually delivered (client died)",
              not alive(cli), f"client stat={proc_stat(cli)}")

        check("server SURVIVED the terminal's SIGHUP", alive(srv),
              f"stat={proc_stat(srv)}")
        names = session_names()
        check("server still ANSWERS its socket after the HUP",
              names is not None)
        check("the session survived too (this is what 'resume' means)",
              names is not None and "survivor" in names,
              f"sessions={names}")

        log = t.log("server")
        check("no panic in server.log", "panicked at" not in log)
    finally:
        t.kill()
        stop_server()

    print()
    if failures:
        print(f"FAILED ({len(failures)}): " + "; ".join(failures))
        raise SystemExit(1)
    print("all checks passed")


if __name__ == "__main__":
    main()
