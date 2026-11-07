#!/usr/bin/env python3
"""Deleting the session a client is on must move that client to a live session.

The contract is `handle_session_removed`: while another session exists, every
client on the removed session is moved onto one, and `SessionDeleted` means that
no session is left. The client exits on that event when its manager is closed.

Recovery is proved by typed input, because a stuck client still paints its last
frame. The typed line assembles `RAN:<tag>:$REMUX_SESSION`, so the marker cannot
be the echo of the line, and the session in it comes from the shell that ran it.
The session the server says the client is on is the `*` row of the client's own
manager, which the server marks per client.

Each case leaves one session alive, so the landing session is known. The server
picks from a `HashMap`, so with several survivors the choice is arbitrary.

Run from the repo root:
    python3 tests/pty/session_delete_current.py
"""
import os
import re
import sys
import time
import traceback

import pexpect
import pyte

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pty_harness import BIN, Tui, sm_open, sm_goto_text, sm_tree  # noqa: E402
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "frame"))
from harness import Client, Server  # noqa: E402

RUNDIR = os.environ.get("REMUX_SDC_RUN", "/tmp/rmx-sdc")
WINCH_RUNDIR = f"{RUNDIR}w"

# A foreground loop and not a trap at the prompt, so that the trap runs within a
# `sleep` of the signal whatever the shell does with traps while it reads a line.
WINCH_SCRIPT = """trap 'stty size >> {log}' WINCH
stty size > {log}
while :; do sleep 0.1; done
"""

fails = []


def check(ok, what):
    print(f"  {'ok  ' if ok else 'FAIL'} {what}")
    if not ok:
        fails.append(what)


def server_tree(c):
    """`{"folders": {name: [session, ...]}, "unfiled": [session, ...]}`."""
    for _ in range(5):
        c.drain(0.1)
        c.send("ListSessionTree")
        for msg in c.drain(0.5):
            if isinstance(msg, dict) and "SessionTree" in msg:
                tree = msg["SessionTree"]
                return {
                    "folders": {
                        f["name"]: [s["name"] for s in f["sessions"]]
                        for f in tree["folders"]
                    },
                    "unfiled": [s["name"] for s in tree["unfiled"]],
                }
    raise AssertionError("the server never answered ListSessionTree")


def status_names(t, session):
    return re.search(rf"(^|\s){re.escape(session)} │", t.rows_text()[-1]) is not None


def wait_for(t, predicate, timeout=4.0):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if predicate():
            return True
        t.pump(0.1)
    return predicate()


def close_manager(t):
    if t.has("Session Manager"):
        t.send("\x1b", 0.6)


def current_row(t):
    """The session the `*` row of this client's own manager names, or None."""
    try:
        tree = sm_open(t)
    except AssertionError:
        return None
    hits = [m.group(1) for r in tree for m in [re.search(r"\* (\S+) \(", r.text)] if m]
    close_manager(t)
    return hits[0] if len(hits) == 1 else None


def recovered(t, label, session, tag):
    check(t.alive(), f"{label}: the client is alive")
    if not t.alive():
        return
    close_manager(t)
    check(wait_for(t, lambda: status_names(t, session)),
          f"{label}: the status bar names {session!r} ({t.rows_text()[-1].strip()[:50]!r})")
    t.send(f"printf 'RAN:%s:%s\\n' {tag} \"$REMUX_SESSION\"\r", 0.2)
    marker = f"RAN:{tag}:{session}"
    check(wait_for(t, lambda: t.has(marker)), f"{label}: typed input ran, {marker!r} is on screen")
    check(current_row(t) == session, f"{label}: the server marks {session!r} as this client's")


def confirm_delete(t, keys, row, prompt):
    sm_goto_text(t, row)
    for key in keys:
        t.send(key, 0.35)
    assert wait_for(t, lambda: t.has(prompt), 2.0), f"no prompt {prompt!r} after {keys!r}"
    t.send("y", 1.0)


def start():
    t = Tui(RUNDIR, cols=120, rows=40).start()
    c = Client(f"{t.rundir}/run/remux.sock")
    c.hello()
    c.drain(0.3)
    return t, c


def second_client(t):
    """A second real terminal on `t`'s server. `kill()` on it stops only the client."""
    t2 = Tui(t.rundir, cols=t.cols, rows=t.rows)
    t2.screen = pyte.Screen(t2.cols, t2.rows)
    t2.stream = pyte.ByteStream(t2.screen)
    t2.raw = bytearray()
    t2.child = pexpect.spawn(BIN, [], env=t.env, dimensions=(t2.rows, t2.cols), encoding=None)
    t2.pump(1.2)
    assert status_names(t2, "main"), f"second client not on main: {t2.rows_text()[-1]!r}"
    return t2


def create(c, name, folder=None):
    c.send({"CreateSession": {"name": name, "folder": folder}})
    time.sleep(0.3)


def in_folder_with_sibling(c):
    c.send({"Command": {"FolderNew": "home"}})
    create(c, "kin", "home")
    c.send({"Command": {"FolderMoveSession": {"session": "main", "folder": "home"}}})
    create(c, "haven")
    tree = server_tree(c)
    assert tree["unfiled"] == ["haven"], tree
    assert sorted(tree["folders"]["home"]) == ["kin", "main"], tree


def logs(t):
    for which in ("server", "client"):
        log = t.log(which)
        check(bool(log), f"{which}.log has content")
        check("panicked at" not in log, f"no panic in {which}.log")


def sx_on_current():
    t, c = start()
    try:
        create(c, "haven")
        sm_open(t)
        confirm_delete(t, "sx", "main", "Delete session 'main'?")
        check(server_tree(c) == {"folders": {}, "unfiled": ["haven"]}, "the server deleted `main`")
        recovered(t, "sx", "haven", "alpha")
    finally:
        t.kill()
        logs(t)


def fx_on_folder_of_current():
    t, c = start()
    try:
        in_folder_with_sibling(c)
        sm_open(t)
        confirm_delete(t, "fx", "home", "Delete folder 'home'?")
        check(server_tree(c) == {"folders": {}, "unfiled": ["haven"]},
              "the server deleted the folder, `main` and `kin`")
        recovered(t, "fx", "haven", "bravo")
    finally:
        t.kill()
        logs(t)


def second_client_recovers(keys, row, prompt, seed, tags):
    t, c = start()
    t2 = None
    try:
        t2 = second_client(t)
        seed(c)
        sm_open(t)
        confirm_delete(t, keys, row, prompt)
        recovered(t, f"{keys} requester", "haven", tags[0])
        recovered(t2, f"{keys} second client", "haven", tags[1])
    finally:
        if t2 is not None:
            t2.kill()
        t.kill()
        logs(t)


def second_client_manager_open(keys, row, prompt, seed, tag):
    t, c = start()
    t2 = None
    try:
        t2 = second_client(t)
        seed(c)
        sm_open(t2)
        sm_open(t)
        confirm_delete(t, keys, row, prompt)

        def gone():
            try:
                return not any(row in r.text for r in sm_tree(t2))
            except AssertionError:
                return False

        check(wait_for(t2, gone), f"{keys}: an open manager on the second client drops {row!r}")
        recovered(t2, f"{keys} second client, manager open", "haven", tag)
    finally:
        if t2 is not None:
            t2.kill()
        t.kill()
        logs(t)


def killed_from_outside():
    t, c = start()
    try:
        create(c, "haven")
        c.send({"KillSession": {"name": "main"}})
        t.pump(1.0)
        check(server_tree(c) == {"folders": {}, "unfiled": ["haven"]}, "the server deleted `main`")
        recovered(t, "killed from outside", "haven", "golf")
    finally:
        t.kill()
        logs(t)


def not_current_does_not_move():
    t, c = start()
    try:
        create(c, "haven")
        sm_open(t)
        confirm_delete(t, "sx", "haven", "Delete session 'haven'?")
        check(server_tree(c) == {"folders": {}, "unfiled": ["main"]}, "the server deleted `haven`")
        check(t.has("Session Manager"), "the manager stays open")
        recovered(t, "not current", "main", "hotel")
    finally:
        t.kill()
        logs(t)


def only_session_still_exits(keys, row, prompt):
    t, c = start()
    try:
        if keys == "fx":
            c.send({"Command": {"FolderNew": "home"}})
            c.send({"Command": {"FolderMoveSession": {"session": "main", "folder": "home"}}})
            time.sleep(0.4)
        sm_open(t)
        confirm_delete(t, keys, row, prompt)
        check(wait_for(t, lambda: not t.alive()), f"{keys} on the only session exits the client")
    finally:
        t.kill()
        logs(t)


def shell_exit_lands_on_survivor():
    t, c = start()
    try:
        create(c, "haven")
        t.send("exit\r", 1.5)
        check(server_tree(c) == {"folders": {}, "unfiled": ["haven"]},
              "the shell's exit removed `main`")
        recovered(t, "shell exit", "haven", "india")
    finally:
        t.kill()
        logs(t)


def winch_lines(path):
    if not os.path.exists(path):
        return []
    with open(path) as f:
        return f.read().split("\n")[:-1]


def wait_for_winch(path, seen, timeout=4.0):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if len(winch_lines(path)) > seen:
            return True
        time.sleep(0.05)
    return False


def attach_sized(c, session, cols, rows):
    c.send({"Attach": {"session_name": session}})
    c.send({"Resize": {"cols": cols, "rows": rows}})
    c.drain(0.4)


def detached_session_keeps_its_size(how):
    """`bystander` is the only survivor, so the server must pick it as `next`."""
    s = Server(WINCH_RUNDIR).start()
    log = f"{s.rundir}/winch.log"
    script = f"{s.rundir}/winch.sh"
    with open(script, "w") as f:
        f.write(WINCH_SCRIPT.format(log=log))
    try:
        c = Client(s.sock)
        c.hello()
        c.drain(0.2)
        c.send({"CreateSession": {"name": "bystander", "folder": None}})
        attach_sized(c, "bystander", 100, 30)
        c.send({"Input": {"data": list(f"sh {script}\n".encode())}})
        assert wait_for_winch(log, 0), "the WINCH script never started"

        seen = len(winch_lines(log))
        c.send({"Resize": {"cols": 110, "rows": 32}})
        check(wait_for_winch(log, seen), f"{how}: a real resize fires the trap")
        c.close()
        time.sleep(1.0)

        other = Client(s.sock)
        other.hello()
        other.drain(0.2)
        if how == "fx":
            other.send({"Command": {"FolderNew": "home"}})
            other.send({"CreateSession": {"name": "doomed", "folder": "home"}})
        else:
            other.send({"CreateSession": {"name": "doomed", "folder": None}})
        other.drain(0.6)
        before = winch_lines(log)

        if how == "fx":
            other.send({"Command": {"FolderDelete": "home"}})
        else:
            other.send({"KillSession": {"name": "doomed"}})
        other.drain(1.5)
        tree = server_tree(other)
        check(tree == {"folders": {}, "unfiled": ["bystander"]},
              f"{how}: only `bystander` is left ({tree})")
        after = winch_lines(log)
        check(after == before, f"{how}: no resize reached the detached shell "
                               f"(size {before[-1]!r}, log now ends {after[-1]!r})")

        # The silence above means nothing if the kill took the shell or its trap
        # with it, so a client that attaches at a new size must still fire it.
        attach_sized(other, "bystander", 90, 28)
        check(wait_for_winch(log, len(after)), f"{how}: the trap still fires after the delete")
        other.close()
    finally:
        s.stop()
        check("panicked at" not in s.log(), f"{how}: no panic in server.log")


def unfiled_haven(c):
    create(c, "haven")


CASES = [
    ("`sx` on the current session", sx_on_current, ()),
    ("`fx` on the folder of the current session", fx_on_folder_of_current, ()),
    ("`sx`, a second client on the session", second_client_recovers,
     ("sx", "main", "Delete session 'main'?", unfiled_haven, ("charlie", "delta"))),
    ("`fx`, a second client on the session", second_client_recovers,
     ("fx", "home", "Delete folder 'home'?", in_folder_with_sibling, ("echo1", "echo2"))),
    ("`sx`, the second client has its manager open", second_client_manager_open,
     ("sx", "main", "Delete session 'main'?", unfiled_haven, "foxtrot")),
    ("`fx`, the second client has its manager open", second_client_manager_open,
     ("fx", "home", "Delete folder 'home'?", in_folder_with_sibling, "fox2")),
    ("`KillSession` from another connection", killed_from_outside, ()),
    ("a session that is not current", not_current_does_not_move, ()),
    ("a shell exit with a never-attached survivor", shell_exit_lands_on_survivor, ()),
    ("`KillSession` of an unattached session, a detached survivor",
     detached_session_keeps_its_size, ("kill",)),
    ("`FolderDelete` of an unattached session, a detached survivor",
     detached_session_keeps_its_size, ("fx",)),
    ("`sx` on the only session", only_session_still_exits,
     ("sx", "main", "Delete session 'main'?")),
    ("`fx` on the folder of the only session", only_session_still_exits,
     ("fx", "home", "Delete folder 'home'?")),
]


def main():
    print(f"binary: {BIN} (built {time.ctime(os.path.getmtime(BIN))})")
    for label, case, args in CASES:
        print(label)
        try:
            case(*args)
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
