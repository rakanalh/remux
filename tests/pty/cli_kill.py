#!/usr/bin/env python3
"""`remux kill <name>` must finish, tell the truth, and leave the server right.

`KillSession` has no reply of its own. A success only reaches clients attached
to the removed session, and only when it was the last one, so a command line
that never attaches hears nothing. The CLI therefore follows the kill with
`ListSessions` on the same connection: the server reads a connection's messages
in order, so the list is sent only once the kill has been handled, and an Error
for the kill arrives ahead of it.

Every CLI call has a timeout, so a hang is a failed check and not a hung
harness. Each case asserts the exit code, the text, and what the server reports
afterwards through a separate connection, because a CLI that prints "Killed"
proves nothing about the session.

The stand-in server covers what a real one cannot be made to do on demand: a
broadcast ahead of the answer, a connection closed before it, silence, and a
list that still holds the session.

Needs pyte, so run with an interpreter that has it, from the repo root. Naming
case functions runs only those:
    python3 tests/pty/cli_kill.py [case ...]
"""
import os
import re
import socket
import subprocess
import sys
import time
import traceback

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from fake_server import FakeServer, _frame, _read_frame  # noqa: E402
from pty_harness import BIN, PROTOCOL_VERSION, Tui  # noqa: E402
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "frame"))
from harness import Client, Server, remux_daemons  # noqa: E402

RUNDIR = "/tmp/rmx-clikill"
CLI_TIMEOUT = 6.0
# Past the CLI's own reply deadline, for the case that waits it out.
SILENT_TIMEOUT = 14.0
NO_SERVER = "remux: no_server: no remux server is running here"
RAW_CONTROL = re.compile("[\\x00-\\x1f\\x7f-\\x9f\\u202e]")

fails = []


def check(ok, what):
    print(f"  {'ok  ' if ok else 'FAIL'} {what}")
    if not ok:
        fails.append(what)


def cli(env, *args, timeout=CLI_TIMEOUT):
    """(exit code, stdout, stderr), with None as the code for a hang."""
    try:
        r = subprocess.run([BIN, *args], env=env, capture_output=True, text=True, timeout=timeout)
    except subprocess.TimeoutExpired:
        return None, "", ""
    return r.returncode, r.stdout, r.stderr


def expect(result, label, code, out, err):
    got, stdout, stderr = result
    check(got == code, f"{label}: exit code {code} (got {'a hang' if got is None else got})")
    check(stdout.strip() == out, f"{label}: stdout is {out!r} (got {stdout.strip()!r})")
    check(stderr.strip() == err, f"{label}: stderr is {err!r} (got {stderr.strip()!r})")


def sessions(c):
    """{name: clients attached}, as the server reports it."""
    c.drain(0.1)
    c.send("ListSessions")
    for msg in c.drain(1.0):
        if isinstance(msg, dict) and "SessionList" in msg:
            return {s["name"]: s["client_count"] for s in msg["SessionList"]["sessions"]}
    raise AssertionError("the server never answered ListSessions")


def create(c, name):
    c.send({"CreateSession": {"name": name, "folder": None}})
    time.sleep(0.3)


def no_panic(log, label):
    check(bool(log), f"{label}: the log has content")
    check("panicked at" not in log, f"{label}: no panic in the log")


def start_server(*names):
    s = Server(RUNDIR).start()
    c = Client(s.sock)
    c.hello()
    for name in names:
        create(c, name)
    c.drain(0.2)
    return s, c


def wait_for(t, predicate, timeout=4.0):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if predicate():
            return True
        t.pump(0.1)
    return predicate()


def start_tui():
    t = Tui(RUNDIR, cols=120, rows=40).start()
    c = Client(f"{t.rundir}/run/remux.sock")
    c.hello()
    c.drain(0.3)
    return t, c


def recovered(t, c, label, session, tag):
    """A stuck client still paints its last frame, so recovery is typed input.
    The marker is assembled by the shell, so the echoed line cannot satisfy it."""
    check(t.alive(), f"{label}: the client is alive")
    if not t.alive():
        return
    bar = rf"(^|\s){re.escape(session)} │"
    check(wait_for(t, lambda: re.search(bar, t.rows_text()[-1]) is not None),
          f"{label}: the status bar names {session!r}")
    t.send(f"printf 'RAN:%s:%s\\n' {tag} \"$REMUX_SESSION\"\r", 0.2)
    marker = f"RAN:{tag}:{session}"
    check(wait_for(t, lambda: t.has(marker)), f"{label}: typed input ran, {marker!r} is on screen")
    check(sessions(c).get(session) == 1, f"{label}: the server counts the client on {session!r}")


def existing_session():
    s, c = start_server("victim", "haven")
    try:
        expect(cli(s.env, "kill", "victim"), "kill", 0, "Killed session 'victim'.", "")
        check(sorted(sessions(c)) == ["haven"], "`victim` is gone and `haven` is not")
        ls = cli(s.env, "ls")
        check(ls[0] == 0 and "haven" in ls[1] and "victim" not in ls[1], f"`remux ls` agrees: {ls}")
        check('KillSession name="victim" panes_removed=1' in s.log(), "the server reaped its pane")
    finally:
        s.kill()
    no_panic(s.log(), "existing")


def unknown_session():
    s, c = start_server("haven")
    try:
        expect(cli(s.env, "kill", "nope"), "kill", 1, "",
               "remux: error: session 'nope' not found")
        check(sorted(sessions(c)) == ["haven"], "nothing was removed")
    finally:
        s.kill()
    no_panic(s.log(), "unknown")


def no_argument():
    s, c = start_server("haven")
    try:
        code, out, err = cli(s.env, "kill")
        check(code == 2, f"exit code 2 (got {code})")
        check(out == "" and "<NAME>" in err and "Usage:" in err, f"a usage error: {err!r}")
        check(sorted(sessions(c)) == ["haven"], "nothing was removed")
    finally:
        s.kill()


def last_session():
    s, c = start_server("only")
    try:
        expect(cli(s.env, "kill", "only"), "kill", 0, "Killed session 'only'.", "")
        check(sessions(c) == {}, "the server reports no sessions")
        check(s.proc.poll() is None, "the server is still running")
        expect(cli(s.env, "ls"), "ls", 0, "No sessions.", "")
        expect(cli(s.env, "kill", "only"), "second kill", 1, "",
               "remux: error: session 'only' not found")
    finally:
        s.kill()
    no_panic(s.log(), "last")


def hostile_name():
    """A name is whatever a client sent, so it can drive the terminal that prints
    it unless every path out escapes it."""
    name = "evil\x1b]52;c;AAAA\x07\u202e\nname"
    s, c = start_server(name, "haven")
    try:
        check(name in sessions(c), "the server accepted the name")
        code, out, err = cli(s.env, "kill", name)
        check(code == 0, f"exit code 0 (got {code})")
        check(RAW_CONTROL.search(out.rstrip("\n")) is None, f"stdout has no raw control: {out!r}")
        check(out.startswith("Killed session 'evil\\x1b]52;c;AAAA"), f"stdout shows it: {out!r}")
        check(err == "", f"stderr is empty: {err!r}")
        check(sorted(sessions(c)) == ["haven"], "the session is gone")
        code, out, err = cli(s.env, "kill", name)
        check(code == 1, f"second kill: exit code 1 (got {code})")
        check(out == "", f"second kill: stdout is empty: {out!r}")
        check(RAW_CONTROL.search(err.rstrip("\n")) is None, f"stderr has no raw control: {err!r}")
        check("evil\\x1b]52;c;AAAA" in err, f"stderr shows it: {err!r}")
    finally:
        s.kill()
    no_panic(s.log(), "hostile")


def attached_client_moves():
    t, c = start_tui()
    try:
        create(c, "haven")
        check(sessions(c).get("main") == 1, "the client starts on `main`")
        expect(cli(t.env, "kill", "main"), "kill", 0, "Killed session 'main'.", "")
        check(sorted(sessions(c)) == ["haven"], "`main` is gone")
        recovered(t, c, "attached", "haven", "alpha")
    finally:
        t.kill()
    no_panic(t.log("server"), "attached")


def attached_client_on_last_session_exits():
    t, c = start_tui()
    try:
        expect(cli(t.env, "kill", "main"), "kill", 0, "Killed session 'main'.", "")
        check(sessions(c) == {}, "the server reports no sessions")
        check(wait_for(t, lambda: not t.alive()), "the attached client exited")
    finally:
        t.kill()
    no_panic(t.log("server"), "attached last")


def from_inside_the_victim():
    """The kill closes the pane that runs it, so nothing may be left waiting."""
    t, c = start_tui()
    try:
        create(c, "haven")
        t.send(f"{BIN} kill main\r", 0.5)
        check(wait_for(t, lambda: "main" not in sessions(c)), "`main` is gone")
        recovered(t, c, "inside", "haven", "bravo")
        check(remux_daemons(RUNDIR, kinds=(b"kill",)) == [], "no `remux kill` process is left")
    finally:
        t.kill()
    no_panic(t.log("server"), "inside")


def no_server():
    s, _ = start_server("haven")
    s.stop()
    check(not os.path.exists(s.sock), "the server is gone")
    expect(cli(s.env, "kill", "haven"), "kill", 1, "", NO_SERVER)
    check(not os.path.exists(s.sock), "no socket appeared")
    check(remux_daemons(RUNDIR) == [], "no server was started")


def dead_server():
    s, _ = start_server("haven")
    s.crash()
    check(os.path.exists(s.sock), "the dead server left its socket")
    expect(cli(s.env, "kill", "haven"), "kill", 1, "", NO_SERVER)
    check(remux_daemons(RUNDIR) == [], "no server was started")


class ScriptedServer(FakeServer):
    """Answers each message with `script[variant]`: a list of frames, or None to
    close the connection. A variant it does not name gets nothing."""

    def __init__(self, path, script):
        super().__init__(path)
        self.script = script
        self.received = []

    def _handle(self, conn):
        try:
            _read_frame(conn)
            conn.sendall(_frame({"protocol_version": PROTOCOL_VERSION, "remux_version": "fake"}))
            while True:
                msg = _read_frame(conn)
                if msg is None:
                    return
                variant = msg if isinstance(msg, str) else next(iter(msg))
                self.received.append(variant)
                frames = self.script.get(variant, [])
                if frames is None:
                    conn.shutdown(socket.SHUT_RDWR)
                    return
                for frame in frames:
                    conn.sendall(_frame(frame))
        except OSError:
            return


def listing(*names):
    return {"SessionList": {"sessions": [
        {"name": n, "folder": None, "tab_count": 1, "client_count": 0} for n in names]}}


def scripted(label, script, code, out, err, timeout=CLI_TIMEOUT):
    s, _ = start_server()
    s.stop()
    fake = ScriptedServer(s.sock, script).start()
    try:
        expect(cli(s.env, "kill", "victim", timeout=timeout), label, code, out, err)
        check(fake.received[:1] == ["KillSession"], f"{label}: the kill was sent first")
    finally:
        fake.close()


def broadcast_ahead_of_the_answer():
    pushed = {"Event": {"SessionCreated": "other"}}
    scripted("broadcast", {"KillSession": [pushed], "ListSessions": [listing("other")]},
             0, "Killed session 'victim'.", "")


def broadcast_ahead_of_a_refusal():
    pushed = {"Event": {"SessionCreated": "other"}}
    refused = {"Error": {"message": "session 'victim' not found", "code": None, "req": None}}
    scripted("refusal", {"KillSession": [pushed, refused], "ListSessions": [listing("other")]},
             1, "", "remux: error: session 'victim' not found")


def connection_closed():
    # Closed on the second message, so that the CLI has finished sending. Closed
    # on the first, its second write can fail and report that instead.
    scripted("closed", {"ListSessions": None}, 1, "",
             "remux: transport: the server closed the connection")


def silent_server():
    scripted("silent", {}, 1, "", "remux: timeout: the server did not answer in time",
             timeout=SILENT_TIMEOUT)


def still_listed():
    scripted("still listed", {"ListSessions": [listing("victim")]}, 1, "",
             "remux: not_killed: the server still lists session 'victim'")


CASES = [
    ("an existing session", existing_session),
    ("a name that does not exist", unknown_session),
    ("no argument", no_argument),
    ("a name full of control characters", hostile_name),
    ("the last remaining session", last_session),
    ("a client attached to the victim moves to the survivor", attached_client_moves),
    ("a client attached to the last session exits", attached_client_on_last_session_exits),
    ("run from a pane of the victim", from_inside_the_victim),
    ("no server running", no_server),
    ("a server that died and left its socket", dead_server),
    ("a broadcast ahead of the answer is not the answer", broadcast_ahead_of_the_answer),
    ("a broadcast ahead of a refusal is not the answer", broadcast_ahead_of_a_refusal),
    ("a connection closed before the answer is a failure", connection_closed),
    ("a server that never answers is a failure", silent_server),
    ("a list that still holds the session is a failure", still_listed),
]


def main():
    print(f"binary: {BIN} (built {time.ctime(os.path.getmtime(BIN))})")
    wanted = sys.argv[1:]
    for label, case in CASES:
        if wanted and case.__name__ not in wanted:
            continue
        print(label)
        try:
            case()
        except Exception:
            traceback.print_exc(limit=2)
            fails.append(f"{label}: aborted")
    if fails:
        print(f"FAIL: {len(fails)} check(s)")
        for f in fails:
            print(f"  - {f}")
        sys.exit(1)
    print("PASS")


if __name__ == "__main__":
    main()
