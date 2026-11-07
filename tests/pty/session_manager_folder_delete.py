#!/usr/bin/env python3
"""Deleting a folder from the session manager, and what `fd` does on a folder.

The default chord for `FolderDelete` is `fx`. Plain `d` also deletes the row.
`fd` is not a default chord, so `f` arms the prefix and `d` clears it with no
prompt and no message. `press_fd` records that behaviour. Replace its two calls
with `arm(t, "fd", ...)` if `fd` becomes a default chord.

The tree is read from a second connection, so "still there" and "gone" are the
server's answers. The screen supplies the prompt and the `[f-]` hint.

Run from the repo root:
    python3 tests/pty/session_manager_folder_delete.py
"""
import os
import sys
import time
import traceback

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pty_harness import BIN, Tui, sm_open, sm_goto_text, sm_selected  # noqa: E402
from rundirs import run_dir  # noqa: E402
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "frame"))
from harness import Client  # noqa: E402

RUNDIR = os.environ.get("REMUX_SMFD_RUN") or run_dir("rmx-smfd")

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


def on_row(t, name):
    row = sm_goto_text(t, name)
    assert sm_selected(t).y == row.y and name in row.text, f"not on {name!r}: {row!r}"


def prompt_for(folder):
    return f"Delete folder '{folder}'?"


def press_fd(t, c, folder, session=None):
    """`fd` on `folder` when `fd` is not bound: nothing arms and nothing changes."""
    on_row(t, folder)
    before = server_tree(c)
    t.send("f", 0.4)
    check(t.has("[f-]"), f"`f` on {folder!r} arms the chord prefix")
    t.send("d", 0.6)
    check(not t.has("[f-]"), f"`d` clears the prefix on {folder!r}")
    check(not t.has(prompt_for(folder)), f"`fd` on {folder!r} opens no prompt")
    check(t.has("Session Manager"), f"`fd` on {folder!r} leaves the manager open")
    after = server_tree(c)
    check(after == before, f"`fd` leaves the server's tree unchanged ({after})")
    if session is not None:
        check(session in after["folders"].get(folder, []),
              f"`fd` leaves session {session!r} in {folder!r}")


def arm(t, keys, folder):
    on_row(t, folder)
    for key in keys:
        t.send(key, 0.35)
    t.pump(0.3)
    check(t.has(prompt_for(folder)), f"{keys!r} on {folder!r} opens the prompt")


def seeded(rundir, config=None):
    t = Tui(rundir, cols=120, rows=40, config=config).start()
    c = Client(f"{t.rundir}/run/remux.sock")
    c.hello()
    c.drain(0.3)
    for name in ("vacant", "vacant2", "crowded"):
        c.send({"Command": {"FolderNew": name}})
    c.send({"CreateSession": {"name": "resident", "folder": "crowded"}})
    time.sleep(0.5)
    tree = server_tree(c)
    assert tree["folders"] == {"vacant": [], "vacant2": [], "crowded": ["resident"]}, tree
    assert tree["unfiled"] == ["main"], tree
    sm_open(t)
    return t, c


def default_chords():
    t, c = seeded(RUNDIR)
    try:
        print("the footer advertises the real chord")
        check(t.has("fx folder del"), "the footer lists `fx folder del`")
        check(not t.has("fd folder"), "the footer lists no `fd` chord")

        print("`fd` on an empty folder and on a folder with a session")
        press_fd(t, c, "vacant")
        press_fd(t, c, "crowded", session="resident")

        # `fx` runs on the row that `fd` just left alone. This is the positive
        # control: it proves that keys reach the overlay on that row, so the
        # inert result above comes from the chord and not from a dead harness.
        print("`fx` on an empty folder")
        arm(t, "fx", "vacant")
        t.send("n", 0.5)
        check(not t.has(prompt_for("vacant")), "`n` closes the prompt")
        check("vacant" in server_tree(c)["folders"], "`n` deletes nothing")
        arm(t, "fx", "vacant")
        t.send("y", 0.8)
        tree = server_tree(c)
        check("vacant" not in tree["folders"], "`fx`, `y` removes the empty folder")
        check("vacant2" in tree["folders"] and "crowded" in tree["folders"],
              "`fx`, `y` leaves the other folders")

        print("plain `d` on an empty folder")
        arm(t, "d", "vacant2")
        t.send("y", 0.8)
        check("vacant2" not in server_tree(c)["folders"], "`d`, `y` removes the folder")

        print("`fx` on a folder with a session cascades to the session")
        arm(t, "fx", "crowded")
        t.send("y", 1.0)
        tree = server_tree(c)
        check(tree == {"folders": {}, "unfiled": ["main"]},
              f"the folder and its session are gone, `main` stays ({tree})")
        check(t.alive(), "the client is alive after the deletes")
    finally:
        t.kill()
        logs(t)


def configured_fd():
    t, c = seeded(f"{RUNDIR}-cfg", config='[keybindings.session_manager]\nfd = "FolderDelete"\n')
    try:
        print("a configured `fd` deletes a folder")
        arm(t, "fd", "vacant")
        t.send("y", 0.8)
        check("vacant" not in server_tree(c)["folders"], "configured `fd`, `y` removes it")
        check(t.alive(), "the client is alive")
    finally:
        t.kill()
        logs(t)


def logs(t):
    for which in ("server", "client"):
        log = t.log(which)
        check(bool(log), f"{which}.log has content")
        check("panicked at" not in log, f"no panic in {which}.log")


def main():
    print(f"binary: {BIN} (built {time.ctime(os.path.getmtime(BIN))})")
    for case in (default_chords, configured_fd):
        try:
            case()
        except Exception:
            traceback.print_exc()
            fails.append(f"{case.__name__} aborted")
    if fails:
        print(f"FAIL: {len(fails)} check(s)")
        for f in fails:
            print(f"  - {f}")
        sys.exit(1)
    print("PASS")


if __name__ == "__main__":
    main()
