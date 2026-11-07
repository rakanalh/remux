#!/usr/bin/env python3
"""From INSIDE a View, the agents panel still jumps to the agent's pane.

The report: clicking a row of the views panel enters a view, but once inside
it, clicking a row of the agents panel did nothing. A live view swallowed every
mouse event before the sidebar routing saw it, so a press on a panel never
reached the plugin.

The agent runs in a SECOND session the view does not alias, so landing on it
needs a real jump out of the view, not a focus change between its cells.

  1  mouse: click the agent's row while the view is on screen, click the views
     panel's row to go back in, and click the agent again
  2  keyboard: `Alt+h` from the view's left cell into the panel, then Enter
  3  two agents: the first click on the unselected row only selects it and the
     view stays, the second lands on THAT agent and not the first
  4  a click on the panel takes the keyboard, and a click back on a cell returns
     it: a line typed next runs in the cell's shell

The jumps assert the view is gone from the status bar AND that a line typed
next reaches the stand-in agent, which answers with a marker it ASSEMBLES
(`SAW-AGENT1<whereami>`), so the terminal's echo of the typed word cannot
satisfy the check. A view cell's shell would only echo the word back.

The rundirs are fixed: two concurrent runs corrupt each other, and the false
red looks like a real failure.

Run from the repo root:  python3 tests/pty/sidebar_agents_from_view.py
"""
import json
import os
import socket
import struct
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pty_harness import (  # noqa: E402
    PROTOCOL_VERSION,
    Tui,
    install_agent_standin,
    sm_compose_view,
)

COLS, ROWS = 120, 40
SIDEBAR_W = 30
DOTS = ("●", "○")

CFG = f"""
[[sidebar]]
edge = "left"
size = {SIDEBAR_W}
visible = true

  [[sidebar.panel]]
  plugin = "agents"
  weight = 1

  [[sidebar.panel]]
  plugin = "views"
  weight = 1
"""

AGENT = """#!/bin/sh
tag=$1
printf 'agent ready %s\\n' "$tag"
while read line; do
  printf 'SAW-%s<%s>\\n' "$tag" "$line"
done
"""



class Wire:
    def __init__(self, path):
        self.s = socket.socket(socket.AF_UNIX)
        self.s.connect(path)
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

    def close(self):
        self.s.close()


def sgr(col, row, release=False):
    """An SGR left-button report; coordinates are 0-based here, 1-based on the wire."""
    return f"\x1b[<0;{col + 1};{row + 1}{'m' if release else 'M'}".encode()


def agent_rows_y(t):
    """Screen rows of the panel's agent entries, top first."""
    return [
        y for y, row in enumerate(t.rows_text())
        if row[1:SIDEBAR_W - 1].strip().startswith(DOTS)
    ]


def click(t, y, x=5):
    t.send(sgr(x, y), 0.1)
    t.send(sgr(x, y, release=True), 0.3)


def seed_agent(rundir, session, tag):
    w = Wire(f"{rundir}/run/remux.sock")
    w.send({"CreateSession": {"name": session, "folder": None}})
    w.send({"Attach": {"session_name": session}})
    w.send({"Resize": {"cols": 80, "rows": 24}})
    time.sleep(0.8)
    w.send({"Input": {"data": list(f"claude {tag}\n".encode())}})
    time.sleep(0.5)
    w.close()


def in_view(t):
    return "View 1" in t.rows_text()[-1]


def setup(rundir, agents=(("agents", "AGENT1"),)):
    bindir = f"{rundir}-bin"
    install_agent_standin(bindir, "claude", AGENT)
    t = Tui(rundir, cols=COLS, rows=ROWS, config=CFG,
            extra_env={"PATH": f"{bindir}:" + os.environ.get("PATH", "")}).start()
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

    # The agents' sessions are created only now, so the session manager above
    # saw a single "Tab 1" to compose from.
    for session, tag in agents:
        seed_agent(rundir, session, tag)

    deadline = time.time() + 8
    while time.time() < deadline and len(agent_rows_y(t)) < len(agents):
        t.pump(0.4)
    if len(agent_rows_y(t)) < len(agents):
        t.dump("agent rows missing")
        raise SystemExit("ABORT: the agents panel never listed the stand-ins")
    if not in_view(t):
        raise SystemExit("ABORT: the view went away before the jump was tried")
    return t


def landed_on_agent(t, fails, tag="AGENT1", probe="whereami"):
    t.pump(1.0)
    if in_view(t):
        fails.append(f"still in the view: {t.rows_text()[-1].rstrip()!r}")
    answer = f"SAW-{tag}<{probe}>"
    t.send(f"{probe}\r", 1.2)
    if not any(answer in r[SIDEBAR_W:] for r in t.rows_text()):
        t.dump("after typing the probe")
        fails.append(f"the typed line did not reach the agent (no {answer!r} on screen)")


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


def test_click_agent_from_view():
    name = "test_click_agent_from_view"
    t = setup("/tmp/rmx-safv1")
    fails = []
    # The only row is already the selection, so one click activates it.
    click(t, agent_rows_y(t)[0])
    landed_on_agent(t, fails, probe="first")

    view_row = next(
        (y for y, r in enumerate(t.rows_text()[:-1]) if "View 1" in r[:SIDEBAR_W]), None)
    if view_row is None:
        t.dump("no view row")
        fails.append("the views panel does not list View 1")
        return finish(t, name, fails)
    click(t, view_row)
    t.pump(1.0)
    if not in_view(t):
        fails.append("the views panel's row did not re-enter the view")
    click(t, agent_rows_y(t)[0])
    landed_on_agent(t, fails, probe="second")
    return finish(t, name, fails)


def test_enter_agent_from_view():
    name = "test_enter_agent_from_view"
    t = setup("/tmp/rmx-safv2")
    fails = []
    t.send(b"\x1bh", 0.6)
    t.send(b"\r", 0.3)
    landed_on_agent(t, fails)
    return finish(t, name, fails)


def test_click_unselected_agent_from_view():
    name = "test_click_unselected_agent_from_view"
    t = setup("/tmp/rmx-safv3", agents=(("agents", "AGENT1"), ("other", "AGENT2")))
    fails = []
    y = agent_rows_y(t)[1]
    text = t.rows_text()[y][:SIDEBAR_W]
    tag = "AGENT2" if "other" in text else "AGENT1"
    click(t, y)
    t.pump(0.6)
    if not in_view(t):
        fails.append("a click that only moves the selection left the view")
    click(t, y)
    landed_on_agent(t, fails, tag=tag)
    return finish(t, name, fails)


def test_click_back_on_a_cell_returns_the_keyboard():
    name = "test_click_back_on_a_cell_returns_the_keyboard"
    t = setup("/tmp/rmx-safv4")
    fails = []
    # A blank row of the panel: it takes focus and activates nothing.
    blank = agent_rows_y(t)[0] + 4
    click(t, blank)
    # No Enter: the panel would activate its selected row.
    t.send("qzq", 1.0)
    if any("qzq" in r[SIDEBAR_W:] for r in t.rows_text()):
        fails.append("the press on the panel did not take the keyboard")

    click(t, 20, x=SIDEBAR_W + 10)
    t.send("printf 'CELL%s\\n' KEYS\r", 1.0)
    if not any("CELLKEYS" in r[SIDEBAR_W:] for r in t.rows_text()):
        t.dump("after typing in the cell")
        fails.append("a click on a cell did not return the keyboard from the panel")
    if not in_view(t):
        fails.append("the view went away")
    return finish(t, name, fails)


if __name__ == "__main__":
    results = [
        test_click_agent_from_view(),
        test_enter_agent_from_view(),
        test_click_unselected_agent_from_view(),
        test_click_back_on_a_cell_returns_the_keyboard(),
    ]
    sys.exit(0 if all(results) else 1)
