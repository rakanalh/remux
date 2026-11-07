#!/usr/bin/env python3
"""From INSIDE a View, a mouse click on a row of every list panel.

`sidebar_agents_from_view.py` covers the agents panel. This covers the other
three: `views`, `sessions` and `files`. A click selects an unselected row and
activates a selected one, so each case counts its clicks.

  views     another view: the first click only selects, the second switches.
            Its cells are on screen, the old ones are gone and unsubscribed, and
            a line typed next runs in a cell of the new view
  views     the view already on screen: only the keyboard moves to its cells,
            with no second Detach or subscribe
  sessions  a session, a tab and a pane row each leave the view and land on the
            pane the row names, and so does the session the view's cells alias
  files     a directory descends and the view stays
  files     a file: a click and Enter both leave the view, re-attach the session
            and open the split there, and the view can be entered again. The
            control opens the same file with no view up
  files     with the panel on a remote, the file opens on the remote, in the
            remote session, and never on the local server
  files     with no live session to return to, the view stays and the client
            logs why

A landing is proven by the shell that receives the next line: `printf` assembles
`AT<session/pane>` from the pane's own `REMUX_SESSION`/`REMUX_PANE`, so neither
the echoed command nor a stale frame can satisfy the check.

The rundirs are fixed: two concurrent runs corrupt each other, and the false
red looks like a real failure.

Run from the repo root:  python3 tests/pty/sidebar_click_from_view.py [test ...]
"""
import atexit
import json
import os
import re
import shutil
import socket
import struct
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pty_harness import BIN, PROTOCOL_VERSION, Tui, sm_compose_view  # noqa: E402

COLS, ROWS = 120, 40
SIDEBAR_W = 30
WHEREAMI = "printf 'AT<%s/%s>\\n' \"$REMUX_SESSION\" \"$REMUX_PANE\"\r"
AT = re.compile(r"AT<([^/>]+)/%?(\d+)>")


def cfg(*plugins):
    panels = "".join(f'\n  [[sidebar.panel]]\n  plugin = "{p}"\n  weight = 1\n' for p in plugins)
    return f'[[sidebar]]\nedge = "left"\nsize = {SIDEBAR_W}\nvisible = true\n{panels}'


class Wire:
    def __init__(self, rundir):
        self.s = socket.socket(socket.AF_UNIX)
        self.s.connect(f"{rundir}/run/remux.sock")
        self.s.settimeout(3.0)
        self.buf = b""
        self.send({"protocol_version": PROTOCOL_VERSION, "remux_version": "harness"})
        self.recv()

    def send(self, obj):
        b = json.dumps(obj).encode()
        self.s.sendall(struct.pack(">I", len(b)) + b)

    def recv(self):
        while len(self.buf) < 4:
            self.buf += self.s.recv(65536)
        n = struct.unpack(">I", self.buf[:4])[0]
        while len(self.buf) < 4 + n:
            self.buf += self.s.recv(65536)
        body, self.buf = self.buf[4:4 + n], self.buf[4 + n:]
        return json.loads(body)

    def await_msg(self, kind):
        deadline = time.time() + 5
        while time.time() < deadline:
            m = self.recv()
            if isinstance(m, dict) and kind in m:
                return m[kind]
        raise SystemExit(f"ABORT: the server never sent {kind}")

    def session(self, name):
        self.send("ListSessionTree")
        tree = self.await_msg("SessionTree")
        sessions = tree["unfiled"] + [s for f in tree["folders"] for s in f["sessions"]]
        return next(s for s in sessions if s["name"] == name)

    def close(self):
        self.s.close()


def sgr(col, row, release=False):
    return f"\x1b[<0;{col + 1};{row + 1}{'m' if release else 'M'}".encode()


def drag(col, row):
    """An SGR left-button motion report, 0-based as in `sgr`."""
    return f"\x1b[<32;{col + 1};{row + 1}M".encode()


def click(t, y, x=5, settle=0.5):
    t.send(sgr(x, y), 0.1)
    t.send(sgr(x, y, release=True), settle)


def name_x(t, y, name):
    """The column of a row's name: a click on a session's marker folds it."""
    return t.cell_rows()[y].index(name)


def status(t):
    return t.cell_rows()[-1]


def panel(t):
    return [r[1:SIDEBAR_W - 1] for r in t.cell_rows()[:-1]]


def content(t):
    return "\n".join(r[SIDEBAR_W:] for r in t.cell_rows()[:-1])


def row_of(t, pred, what):
    for y, text in enumerate(panel(t)):
        if pred(text.strip()):
            return y
    t.dump(f"no panel row for {what}")
    raise SystemExit(f"ABORT: no panel row for {what}; every check after it would be vacuous")


def view_row(t, name):
    return row_of(t, lambda s: s in (name, f"* {name}"), name)


def where(t):
    """The (session, pane id) the keyboard reaches, as that pane's shell says."""
    before = len(AT.findall(content(t)))
    t.send(WHEREAMI, 1.2)
    hits = AT.findall(content(t))
    return (hits[-1][0], int(hits[-1][1])) if len(hits) > before else None


def keys_reach_panel(t):
    """Keys typed now stay out of every cell. No Enter: a panel would activate."""
    t.send("qzq", 0.8)
    return "qzq" not in content(t)


def seed_other_session(rundir):
    """`other`: Tab 1 with panes p1, p2 (p2 focused), Tab 2 with p3, active.

    Each row's target therefore differs from where a plain attach would land.
    """
    w = Wire(rundir)
    w.send({"CreateSession": {"name": "other", "folder": None}})
    w.send({"Attach": {"session_name": "other"}})
    w.send({"Resize": {"cols": 80, "rows": 24}})
    time.sleep(0.6)
    w.send({"Command": "PaneSplitVertical"})
    time.sleep(0.6)
    w.send({"Command": "TabNew"})
    time.sleep(0.8)
    tabs = w.session("other")["tabs"]
    p1, p2 = (p["id"] for p in tabs[0]["panes"])
    p3 = tabs[1]["panes"][0]["id"]
    for pane, name in ((p1, "PANEONE"), (p2, "PANETWO"), (p3, "PANETHREE")):
        w.send({"InputToPane": {"pane_id": pane, "data": list(f"printf 'CELL%s\\n' {name}\n".encode())}})
    time.sleep(0.4)
    w.close()
    return p1, p2, p3


def in_view(t, name="View 1"):
    return name in status(t)


def enter_first_view(t, cwd=None):
    if cwd:
        t.send(f"cd {cwd}\r", 0.4)
    t.send("clear\r", 0.4)
    t.send("printf 'CELL%s\\n' A\r", 0.5)
    t.prefix(b"pv", 0.8)
    t.send("clear\r", 0.4)
    t.send("printf 'CELL%s\\n' B\r", 0.6)
    sm_compose_view(t, panes=(0, 1), settle=2.0)
    t.pump(1.0)
    if not in_view(t):
        t.dump("not in a view")
        raise SystemExit("ABORT: the view never came up; every check would be vacuous")


def setup(rundir, *plugins, extra_env=None, cwd=None):
    t = Tui(rundir, cols=COLS, rows=ROWS, config=cfg(*plugins), extra_env=extra_env).start()
    enter_first_view(t, cwd)
    return t


def add_second_view(t, rundir, panes):
    w = Wire(rundir)
    w.send({"ViewCreate": {"name": "View 2"}})
    vid = w.await_msg("ViewCreated")["id"]
    w.send({"ViewAddCells": {"id": vid, "cells": [["Local", p] for p in panes]}})
    time.sleep(0.4)
    w.close()
    t.pump(1.0)


def finish(t, name, fails):
    if not t.alive():
        fails.append("the client died")
    logs = t.log("client") + t.log("server")
    if "panicked at" in logs:
        fails.append("a panic in the logs")
    t.kill()
    if fails:
        print(f"FAIL {name}")
        for f in fails:
            print(f"  - {f}")
        return False
    print(f"PASS {name}")
    return True


def subscribed(t, verb, pane):
    return len(re.findall(rf"\b{verb}\b[^\n]*\b{pane}\b", t.log("server")))


def test_click_another_view():
    name = "test_click_another_view"
    rundir = "/tmp/rmx-scfv1"
    t = setup(rundir, "views")
    fails = []
    w = Wire(rundir)
    old_panes = [p["id"] for p in w.session("main")["tabs"][0]["panes"]]
    w.close()
    p1, p2, _ = seed_other_session(rundir)
    add_second_view(t, rundir, (p1, p2))

    y = view_row(t, "View 2")
    click(t, y)
    if not in_view(t):
        fails.append("the first click on an unselected row left View 1")
    click(t, y, settle=1.5)
    if not in_view(t, "View 2"):
        fails.append(f"the second click did not switch: {status(t).rstrip()!r}")
    text = content(t)
    for marker in ("CELLPANEONE", "CELLPANETWO"):
        if marker not in text:
            fails.append(f"{marker} of the new view is not on screen")
    for marker in ("CELLA", "CELLB"):
        if marker in text:
            fails.append(f"{marker} of the old view is still on screen")
    for pane in old_panes:
        if subscribed(t, "UnsubscribePane", pane) == 0:
            fails.append(f"pane {pane} of the old view was never unsubscribed")
    at = where(t)
    if at is None or at[0] != "other" or at[1] not in (p1, p2):
        t.dump("after typing in the new view")
        fails.append(f"a typed line did not run in the new view: {at}")

    # And back, proving the switch is not one-way.
    y = view_row(t, "View 1")
    click(t, y)
    click(t, y, settle=1.5)
    if not in_view(t):
        fails.append(f"the click back did not switch: {status(t).rstrip()!r}")
    text = content(t)
    if "CELLA" not in text or "CELLPANEONE" in text:
        fails.append("the cells on screen are not View 1's after switching back")
    at = where(t)
    if at is None or at[1] not in old_panes:
        fails.append(f"a typed line did not run in View 1 after switching back: {at}")
    return finish(t, name, fails)


def test_click_the_view_on_screen():
    name = "test_click_the_view_on_screen"
    rundir = "/tmp/rmx-scfv2"
    t = setup(rundir, "views")
    fails = []
    w = Wire(rundir)
    panes = [p["id"] for p in w.session("main")["tabs"][0]["panes"]]
    w.close()
    p1, p2, _ = seed_other_session(rundir)
    add_second_view(t, rundir, (p1, p2))
    detaches = t.log("server").count("Detach")
    subs = t.log("server").count("SubscribePane")

    # View 1's row starts selected, so this one click activates it.
    click(t, view_row(t, "View 1"))
    at = where(t)
    if at is None or at[1] not in panes:
        fails.append(f"the click on the selected row left the keyboard off the cells: {at}")

    # Selection moved away: the row now takes two clicks, and the first leaves
    # the keyboard on the panel.
    click(t, view_row(t, "View 2"))
    y = view_row(t, "View 1")
    click(t, y)
    if not keys_reach_panel(t):
        fails.append("the selecting click did not leave the keyboard on the panel")
    click(t, y)
    at = where(t)
    if at is None or at[1] not in panes:
        fails.append(f"the activating click left the keyboard off the cells: {at}")

    if not in_view(t):
        fails.append("the view went away")
    if t.log("server").count("Detach") != detaches:
        fails.append("clicking the view on screen sent another Detach")
    if t.log("server").count("SubscribePane") != subs:
        fails.append("clicking the view on screen subscribed again")
    return finish(t, name, fails)


def drags_at_server(t):
    return t.log("server").count("msg=MouseDrag")


def test_a_lost_panel_release_does_not_take_a_cell_drag():
    name = "test_a_lost_panel_release_does_not_take_a_cell_drag"
    t = setup("/tmp/rmx-scfv12", "views")
    fails = []
    x = SIDEBAR_W + 10
    # A blank panel row claims the grab and activates nothing.
    t.send(sgr(5, 20), 0.4)
    before = drags_at_server(t)
    t.send(sgr(x, 5), 0.3)
    for y in (6, 7, 8):
        t.send(drag(x, y), 0.3)
    t.send(sgr(x, 8, release=True), 0.6)
    if drags_at_server(t) <= before:
        fails.append("a drag on a cell did not reach the server after a lost panel release")
    return finish(t, name, fails)


def test_a_panel_press_disarms_a_cell_drag():
    name = "test_a_panel_press_disarms_a_cell_drag"
    t = setup("/tmp/rmx-scfv13", "views")
    fails = []
    x = SIDEBAR_W + 10
    # The cell press's release is lost, which leaves its drag anchor armed.
    t.send(sgr(x, 5), 0.3)
    click(t, 20)
    before = drags_at_server(t)
    for y in (6, 7, 8):
        t.send(drag(x, y), 0.3)
    t.pump(0.5)
    if drags_at_server(t) != before:
        fails.append("a drag with no press extended the anchor of a cell press before the panel's")
    return finish(t, name, fails)


def session_rows(t):
    """Screen rows of `other`, then of its `Tab 1` and `Tab 2`."""
    other = row_of(t, lambda s: s.endswith(" other"), "session other")
    below = panel(t)[other + 1:]
    tab = lambda label: other + 1 + next(i for i, r in enumerate(below) if label in r)  # noqa: E731
    return other, tab("Tab 1"), tab("Tab 2")


def sessions_setup(rundir):
    t = setup(rundir, "sessions")
    panes = seed_other_session(rundir)
    t.pump(1.5)
    return t, panes


def landed(t, fails, pane):
    t.pump(1.0)
    if in_view(t):
        fails.append(f"still in the view: {status(t).rstrip()!r}")
    at = where(t)
    if at != ("other", pane):
        t.dump("after the jump")
        fails.append(f"expected to land on other/{pane}, the shell said {at}")


def test_click_session_row():
    name = "test_click_session_row"
    t, (_, _, p3) = sessions_setup("/tmp/rmx-scfv3")
    fails = []
    y = session_rows(t)[0]
    x = name_x(t, y, "other")
    click(t, y, x)
    if not in_view(t):
        fails.append("the first click on an unselected row left the view")
    click(t, y, x)
    landed(t, fails, p3)
    return finish(t, name, fails)


def test_click_tab_row():
    name = "test_click_tab_row"
    t, (_, p2, _) = sessions_setup("/tmp/rmx-scfv4")
    fails = []
    y = session_rows(t)[1]
    click(t, y)
    if not in_view(t):
        fails.append("the first click on an unselected row left the view")
    click(t, y)
    landed(t, fails, p2)
    return finish(t, name, fails)


def test_click_pane_row():
    name = "test_click_pane_row"
    t, (p1, _, _) = sessions_setup("/tmp/rmx-scfv5")
    fails = []
    tab = session_rows(t)[1]
    click(t, tab)
    # The click left the keyboard on the panel; a tab row expands with `l`.
    t.send("l", 0.6)
    y = tab + 1
    click(t, y)
    if not in_view(t):
        fails.append("the first click on an unselected row left the view")
    click(t, y)
    landed(t, fails, p1)
    return finish(t, name, fails)


def test_click_the_session_the_view_aliases():
    name = "test_click_the_session_the_view_aliases"
    rundir = "/tmp/rmx-scfv9"
    t, _ = sessions_setup(rundir)
    fails = []
    w = Wire(rundir)
    panes = [p["id"] for p in w.session("main")["tabs"][0]["panes"]]
    w.close()
    y = row_of(t, lambda s: s.endswith(" main"), "session main")
    x = name_x(t, y, "main")
    click(t, y, x)
    if in_view(t):
        click(t, y, x)
    t.pump(1.0)
    if in_view(t):
        fails.append(f"still in the view: {status(t).rstrip()!r}")
    at = where(t)
    if at is None or at[0] != "main" or at[1] not in panes:
        t.dump("after the jump")
        fails.append(f"expected to land in main, the shell said {at}")
    return finish(t, name, fails)


EDITOR = """#!/bin/sh
printf '%s\\n' "$1" >> {stamp}
printf 'EDIT%s\\n' ING
while read line; do printf 'GOT[%s]\\n' "$line"; done
"""


def files_env(rundir):
    """A fixture directory and a stand-in `$EDITOR` that stamps what it opens."""
    fix, stamp = f"{rundir}-fix", f"{rundir}-fix.stamp"
    shutil.rmtree(fix, ignore_errors=True)
    shutil.rmtree(f"{rundir}-bin", ignore_errors=True)
    if os.path.exists(stamp):
        os.remove(stamp)
    os.makedirs(f"{fix}/aaa")
    os.makedirs(f"{fix}/sub")
    os.makedirs(f"{rundir}-bin")
    for path in (f"{fix}/top.txt", f"{fix}/sub/first.txt", f"{fix}/sub/note.txt"):
        open(path, "w").close()
    editor = f"{rundir}-bin/ed"
    with open(editor, "w") as f:
        f.write(EDITOR.format(stamp=stamp))
    os.chmod(editor, 0o755)
    return fix, stamp, {"EDITOR": editor}


def wait_for_row(t, text):
    deadline = time.time() + 6
    while time.time() < deadline:
        if any(text in r for r in panel(t)):
            return True
        t.pump(0.3)
    return False


def click_into_sub_and_open_note(t, fails):
    y = row_of(t, lambda s: s == "sub/", "sub/")
    click(t, y)
    if any("note.txt" in r for r in panel(t)):
        fails.append("the first click on an unselected directory descended")
    click(t, y)
    if not wait_for_row(t, "note.txt"):
        t.dump("after the directory click")
        fails.append("the second click on a directory did not descend")
        return
    y = row_of(t, lambda s: s == "note.txt", "note.txt")
    click(t, y)
    click(t, y, settle=1.5)


def pane_count(rundir):
    w = Wire(rundir)
    n = sum(len(tab["panes"]) for tab in w.session("main")["tabs"])
    w.close()
    return n


def opened(stamp):
    return open(stamp).read().split() if os.path.exists(stamp) else []


def test_file_control_with_no_view():
    """The same clicks with no view up, so the two below cannot pass on a
    stand-in editor that never runs."""
    name = "test_file_control_with_no_view"
    rundir = "/tmp/rmx-scfv6"
    fix, stamp, env = files_env(rundir)
    t = Tui(rundir, cols=COLS, rows=ROWS, config=cfg("files"), extra_env=env).start()
    t.send(f"cd {fix}\r", 0.4)
    fails = []
    if not wait_for_row(t, "sub/"):
        t.dump("no listing")
        raise SystemExit("ABORT: the panel never listed the fixture")
    before = pane_count(rundir)
    click_into_sub_and_open_note(t, fails)
    if opened(stamp) != [f"{fix}/sub/note.txt"]:
        fails.append(f"the editor did not open the file: {opened(stamp)}")
    if pane_count(rundir) != before + 1:
        fails.append("no split appeared")
    return finish(t, name, fails)


def attach_precedes_open(log, session):
    """Whether the server took the view's Detach, then an Attach of `session`,
    then the OpenInSplit, and opened the split in that session.

    The Attach at startup also matches the text, so only one after the Detach
    counts.
    """
    opening = log.find("server: OpenInSplit client_id=")
    if opening < 0:
        return False
    detach = log.rfind("msg=Detach", 0, opening)
    attach = log.rfind(f'msg=Attach {{ session_name: "{session}" }}', 0, opening)
    return 0 <= detach < attach and f'session="{session}"' in log[opening:].split("\n", 1)[0]


def file_opened_outside_the_view(t, stamp, fix, session, fails):
    t.pump(1.0)
    if in_view(t):
        fails.append(f"still in the view: {status(t).rstrip()!r}")
    if session not in status(t):
        fails.append(f"the status bar does not show {session}: {status(t).rstrip()!r}")
    if opened(stamp) != [f"{fix}/sub/note.txt"]:
        fails.append(f"the editor did not open the file: {opened(stamp)}")
    t.send("hello\r", 1.0)
    if "GOT[hello]" not in content(t):
        t.dump("after typing into the split")
        fails.append("a typed line did not reach the editor in the new split")


def view_comes_back(t, fails, panes):
    y = view_row(t, "View 1")
    click(t, y, settle=1.0)
    if not in_view(t):
        click(t, y, settle=1.5)
    if not in_view(t):
        fails.append(f"the view did not come back: {status(t).rstrip()!r}")
    if "CELLA" not in content(t):
        fails.append("the view's cells are not on screen after coming back")
    at = where(t)
    if at is None or at[1] not in panes:
        fails.append(f"a line typed in the view did not run in a cell: {at}")


def file_view_setup(rundir):
    fix, stamp, env = files_env(rundir)
    t = Tui(rundir, cols=COLS, rows=ROWS, config=cfg("files", "views"), extra_env=env).start()
    t.send(f"cd {fix}\r", 0.4)
    if not wait_for_row(t, "sub/"):
        t.dump("no listing")
        raise SystemExit("ABORT: the panel never listed the fixture")
    enter_first_view(t)
    return t, fix, stamp


def main_panes(rundir):
    w = Wire(rundir)
    panes = [p["id"] for p in w.session("main")["tabs"][0]["panes"]]
    w.close()
    return panes


def test_file_click_in_view():
    name = "test_file_click_in_view"
    rundir = "/tmp/rmx-scfv7"
    t, fix, stamp = file_view_setup(rundir)
    fails = []
    before = pane_count(rundir)
    panes = main_panes(rundir)
    click_into_sub_and_open_note(t, fails)
    file_opened_outside_the_view(t, stamp, fix, "main", fails)
    if pane_count(rundir) != before + 1:
        fails.append("no split appeared")
    if not attach_precedes_open(t.log("server"), "main"):
        fails.append("the server did not take the Attach before the OpenInSplit")
    view_comes_back(t, fails, panes)
    return finish(t, name, fails)


def test_file_enter_in_view():
    name = "test_file_enter_in_view"
    rundir = "/tmp/rmx-scfv8"
    t, fix, stamp = file_view_setup(rundir)
    fails = []
    before = pane_count(rundir)
    panes = main_panes(rundir)
    # A cell click pins focus on the left cell, so one `Alt+h` enters the panel.
    click(t, 20, x=SIDEBAR_W + 10)
    t.send(b"\x1bh", 0.6)
    t.send("jl", 0.8)
    if not wait_for_row(t, "note.txt"):
        fails.append("the keyboard did not descend")
    t.send("j", 0.4)
    t.send("\r", 1.5)
    file_opened_outside_the_view(t, stamp, fix, "main", fails)
    if pane_count(rundir) != before + 1:
        fails.append("no split appeared")
    if not attach_precedes_open(t.log("server"), "main"):
        fails.append("the server did not take the Attach before the OpenInSplit")
    view_comes_back(t, fails, panes)
    return finish(t, name, fails)


def start_fake_remote(rundir, editor):
    """A second server and an `ssh` shim that relays to it."""
    remote = f"{rundir}-r"
    shutil.rmtree(remote, ignore_errors=True)
    for d in ("run", "state", "data", "config", "home", "bin"):
        os.makedirs(f"{remote}/{d}")
    xdg = {
        "XDG_RUNTIME_DIR": f"{remote}/run",
        "XDG_STATE_HOME": f"{remote}/state",
        "XDG_DATA_HOME": f"{remote}/data",
        "XDG_CONFIG_HOME": f"{remote}/config",
    }
    shim = f"{remote}/bin/ssh"
    with open(shim, "w") as f:
        f.write("#!/bin/sh\n" + "".join(f"export {k}={v}\n" for k, v in xdg.items())
                + f"exec {BIN} relay\n")
    os.chmod(shim, 0o755)
    env = {**os.environ, **xdg, "HOME": f"{remote}/home", "SHELL": "/bin/sh", "ENV": "/dev/null",
           "TERM": "xterm-256color", "PS1": "$ ", "EDITOR": editor}
    server = subprocess.Popen([BIN, "server"], env=env, stdout=subprocess.DEVNULL,
                              stderr=subprocess.DEVNULL)
    atexit.register(server.kill)
    deadline = time.time() + 8
    while not os.path.exists(f"{remote}/run/remux.sock"):
        if time.time() > deadline:
            server.kill()
            raise SystemExit("ABORT: the fake remote never bound its socket")
        time.sleep(0.05)
    time.sleep(0.3)
    return remote, server


def test_file_in_view_opens_on_the_panels_remote():
    name = "test_file_in_view_opens_on_the_panels_remote"
    rundir = "/tmp/rmx-scfv10"
    fix, local_stamp, env = files_env(rundir)
    remote_stamp = f"{fix}.remote"
    if os.path.exists(remote_stamp):
        os.remove(remote_stamp)
    remote_editor = f"{rundir}-bin/ed-remote"
    with open(remote_editor, "w") as f:
        f.write(EDITOR.format(stamp=remote_stamp))
    os.chmod(remote_editor, 0o755)
    remote, server = start_fake_remote(rundir, remote_editor)
    fails = []

    w = Wire(remote)
    w.send({"CreateSession": {"name": "far", "folder": None}})
    w.send({"Attach": {"session_name": "far"}})
    w.send({"Resize": {"cols": 80, "rows": 24}})
    time.sleep(0.6)
    w.send({"Input": {"data": list(f"cd {fix}\n".encode())}})
    time.sleep(0.6)
    w.close()

    config = cfg("sessions", "views", "files") + (
        '\n[remotes.mini]\nssh = "whatever"\nremux_path = "remux"\nauto_connect = true\n')
    env["PATH"] = f"{remote}/bin:" + os.environ.get("PATH", "")
    t = Tui(rundir, cols=COLS, rows=ROWS, config=config, extra_env=env).start()
    t.pump(1.5)
    t.send("printf 'CELL%s\\n' A\r", 0.5)
    panes = main_panes(rundir)
    w = Wire(rundir)
    w.send({"ViewCreate": {"name": "View 1"}})
    vid = w.await_msg("ViewCreated")["id"]
    w.send({"ViewAddCells": {"id": vid, "cells": [["Local", p] for p in panes]}})
    time.sleep(0.4)
    w.close()

    if not wait_for_row(t, "far"):
        t.dump("no remote session row")
        raise SystemExit("ABORT: the panel never listed the remote session")
    y = row_of(t, lambda s: s.endswith(" far"), "session far")
    x = name_x(t, y, "far")
    click(t, y, x)
    if "far" not in status(t):
        click(t, y, x, settle=1.5)
    if "far" not in status(t) or not wait_for_row(t, "sub/"):
        t.dump("not on the remote")
        raise SystemExit("ABORT: never reached the remote session and its listing")

    y = view_row(t, "View 1")
    click(t, y, settle=1.0)
    if not in_view(t):
        click(t, y, settle=1.5)
    if not in_view(t):
        t.dump("no view")
        raise SystemExit("ABORT: the view never came up over the remote foreground")

    click_into_sub_and_open_note(t, fails)
    file_opened_outside_the_view(t, remote_stamp, fix, "far", fails)
    if opened(local_stamp):
        fails.append(f"the LOCAL editor ran: {opened(local_stamp)}")
    remote_log = open(f"{remote}/state/remux/server.log").read()
    if not attach_precedes_open(remote_log, "far"):
        fails.append("the remote did not take the Attach before the OpenInSplit")
    if "msg=OpenInSplit" in t.log("server"):
        fails.append("the local server received the OpenInSplit")
    if "panicked at" in remote_log:
        fails.append("a panic in the remote's log")
    result = finish(t, name, fails)
    server.kill()
    server.wait()
    return result


def test_file_in_view_with_no_session_to_return_to():
    name = "test_file_in_view_with_no_session_to_return_to"
    rundir = "/tmp/rmx-scfv11"
    fix, stamp, env = files_env(rundir)
    t = Tui(rundir, cols=COLS, rows=ROWS, config=cfg("files", "views"), extra_env=env).start()
    t.send(f"cd {fix}\r", 0.4)
    if not wait_for_row(t, "sub/"):
        t.dump("no listing")
        raise SystemExit("ABORT: the panel never listed the fixture")
    fails = []
    p1, p2, _ = seed_other_session(rundir)
    add_second_view(t, rundir, (p1, p2))
    y = view_row(t, "View 2")
    click(t, y, settle=1.0)
    if not in_view(t, "View 2"):
        click(t, y, settle=1.5)
    if not in_view(t, "View 2"):
        t.dump("no view")
        raise SystemExit("ABORT: the view never came up")

    w = Wire(rundir)
    w.send({"KillSession": {"name": "main"}})
    time.sleep(0.5)
    w.close()
    t.pump(1.5)

    click_into_sub_and_open_note(t, fails)
    t.pump(1.0)
    if not in_view(t, "View 2"):
        t.dump("after the open")
        fails.append(f"the view went away: {status(t).rstrip()!r}")
    if opened(stamp):
        fails.append(f"the editor ran: {opened(stamp)}")
    if "no live session to return to" not in t.log("client"):
        fails.append("the client did not log why the file stayed closed")
    if "msg=OpenInSplit" in t.log("server"):
        fails.append("the server received an OpenInSplit")
    at = where(t)
    if at is None or at[1] not in (p1, p2):
        fails.append(f"the keyboard did not return to a cell: {at}")
    return finish(t, name, fails)


TESTS = [
    test_click_another_view,
    test_click_the_view_on_screen,
    test_a_lost_panel_release_does_not_take_a_cell_drag,
    test_a_panel_press_disarms_a_cell_drag,
    test_click_session_row,
    test_click_tab_row,
    test_click_pane_row,
    test_click_the_session_the_view_aliases,
    test_file_control_with_no_view,
    test_file_click_in_view,
    test_file_enter_in_view,
    test_file_in_view_opens_on_the_panels_remote,
    test_file_in_view_with_no_session_to_return_to,
]

if __name__ == "__main__":
    wanted = sys.argv[1:]
    unknown = set(wanted) - {f.__name__ for f in TESTS}
    if unknown:
        raise SystemExit(f"ABORT: no such test: {sorted(unknown)}")
    chosen = [f for f in TESTS if not wanted or f.__name__ in wanted]
    sys.exit(0 if all([f() for f in chosen]) else 1)
