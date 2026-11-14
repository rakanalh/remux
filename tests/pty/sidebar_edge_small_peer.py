#!/usr/bin/env python3
"""Alt+<dir> from an edge pane enters the sidebar on that edge while a SMALLER
client shares the session.

A second, smaller client on the session makes the server composite every frame
at the smallest attached size. This client is then sent frames narrower and
shorter than its own content rect, and the focused pane rect it gets with them
is in that smaller world. The edge test must be measured against the frame the
rect came with: measured against this client's own pane area, the rightmost or
bottom-most pane never touches the edge, and the key leaks to the server as a
`PaneFocus*` that goes nowhere.

  1  right sidebar, no peer: Alt+l from the right pane enters it (control)
  2  right sidebar, smaller peer: Alt+l from the right pane enters it
  3  right sidebar, smaller peer: Alt+l from the LEFT pane moves to the right
     pane and does not enter the sidebar
  4  bottom sidebar, smaller peer: Alt+j from the only pane enters it
  5  left sidebar, smaller peer: Alt+h from the left pane enters it (control:
     the left edge is at column 0 of every frame)
  6  right sidebar, smaller 70-column peer: a left click on the client-drawn
     status bar at column 80, past the server's 70-column bar, sends no
     `MouseClick`; a click on the first tab's label sends one, and it hits
     that tab

The placeholder panel draws `focused` while it holds the keyboard. A marker
typed after the key, assembled by the shell (`printf '%s:%s' K n`), must then
appear in no pane; a marker in a pane names the pane that kept the keyboard.
The server log must hold no `PaneFocus*` for a key the sidebar took.

Run from the repo root:
    python3 tests/pty/sidebar_edge_small_peer.py [-v]
"""
import json
import os
import re
import socket
import struct
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pty_harness import PROTOCOL_VERSION, Tui  # noqa: E402
from rundirs import run_dir  # noqa: E402

VERBOSE = "-v" in sys.argv
ALT = {"h": b"\x1bh", "j": b"\x1bj", "l": b"\x1bl"}
COLS, ROWS = 120, 36
PEER = (70, 24)

failures = []


def check(cond, msg, detail=""):
    print(f"  {'PASS' if cond else 'FAIL'} {msg}")
    if not cond:
        failures.append(msg)
        if detail and VERBOSE:
            print(f"       {detail}")


def config(edge, size):
    return f"""
[[sidebar]]
edge = "{edge}"
size = {size}
visible = true

  [[sidebar.panel]]
  plugin = "placeholder"
  weight = 1
"""


class Peer:
    """A wire client attached to `main` at a fixed size, and nothing else."""

    def __init__(self, sock, cols, rows):
        self.s = socket.socket(socket.AF_UNIX)
        self.s.connect(sock)
        self.s.settimeout(3.0)
        self.send({"protocol_version": PROTOCOL_VERSION, "remux_version": "peer"})
        self.s.recv(65536)
        self.send({"Attach": {"session_name": "main"}})
        self.send({"Resize": {"cols": cols, "rows": rows}})

    def send(self, obj):
        b = json.dumps(obj).encode()
        self.s.sendall(struct.pack(">I", len(b)) + b)

    def close(self):
        self.s.close()


def screen(t):
    return "\n".join(t.rows_text())


def sidebar_focused(t):
    return "focused" in screen(t)


def columns(text, needle):
    """Every column `needle` starts at, on any row. Two panes share each row,
    so a scan that stops at a row's first match would read the wrong pane."""
    return sorted({m.start() for row in text.splitlines()
                   for m in re.finditer(re.escape(needle), row)})


def presses(t):
    return [line for line in t.log("server").splitlines()
            if "server: MouseClick" in line and "release=false" in line]


def sgr_click(t, col, row):
    t.send(f"\x1b[<0;{col + 1};{row + 1}M".encode(), 0.2)
    t.send(f"\x1b[<0;{col + 1};{row + 1}m".encode(), 0.8)


def focus_commands(t):
    return t.log("server").count("PaneFocus")


def press(t, key, word, label, want_sidebar):
    """Send Alt+`key`, then type marker K:`word`, and check who took them."""
    before = focus_commands(t)
    t.send(ALT[key], 1.0)
    focused = sidebar_focused(t)
    t.send(f"printf '%s:%s\\n' K {word}\r".encode(), 1.0)
    s = screen(t)
    leaked = focus_commands(t) - before
    if want_sidebar:
        check(focused, f"{label}: the sidebar is focused", s)
        check(f"K:{word}" not in s, f"{label}: the typed marker reaches no pane", s)
        check(leaked == 0, f"{label}: no PaneFocus reached the server ({leaked} log lines)")
    else:
        check(not focused, f"{label}: the sidebar is not focused", s)
        check(leaked > 0, f"{label}: the key went to the server as a PaneFocus ({leaked} log lines)")
    return s


def run(name, edge, size, peer_size, body):
    print(f"--- {name}")
    t = Tui(run_dir("rmx-sesp"), cols=COLS, rows=ROWS, config=config(edge, size))
    peer = None
    try:
        t.start()
        t.pump(1.0)
        if peer_size:
            peer = Peer(f"{t.rundir}/run/remux.sock", *peer_size)
            t.pump(1.0)
        body(t)
        if VERBOSE:
            t.dump(name)
        check(t.alive(), f"{name}: the client is alive")
        for which in ("server", "client"):
            check("panicked" not in t.log(which), f"{name}: no panic in {which}.log")
    finally:
        if peer:
            peer.close()
        t.kill()


def two_panes(t):
    t.send(b"printf '%s:%s\\n' P one\r", 0.6)
    t.prefix(b"pn", 1.0)
    t.send(b"printf '%s:%s\\n' P two\r", 0.6)


def from_right_pane(t):
    two_panes(t)
    press(t, "l", "right", "Alt+l from the right pane", True)


def from_left_pane(t):
    two_panes(t)
    t.send(ALT["h"], 1.0)
    s = press(t, "l", "moved", "Alt+l from the left pane", False)
    moved = columns(s, "K:moved")
    two = columns(s, "P:two")
    check(len(moved) == 1 and len(two) == 1 and moved == two,
          f"Alt+l from the left pane: the marker lands in the right pane ({moved} vs {two})", s)


def from_only_pane_down(t):
    press(t, "j", "down", "Alt+j from the only pane", True)


def from_left_pane_left(t):
    two_panes(t)
    t.send(ALT["h"], 1.0)
    press(t, "h", "left", "Alt+h from the left pane", True)


def bar_clicks(t):
    """The client draws the bar across the terminal; the server's bar is only
    the 70 columns of the frame the peer's size gives it."""
    t.prefix(b"tn", 1.0)
    bar = t.rows_text()[-1]
    if VERBOSE:
        print(f"       bar: {bar!r}")
    first = bar.find("Tab 1")
    check(0 <= first < PEER[0], f"the first tab label starts inside column {PEER[0]} ({first})", bar)
    before = len(presses(t))
    sgr_click(t, PEER[0] + 10, ROWS - 1)
    outside = presses(t)[before:]
    check(not outside, f"a click on the client bar at column {PEER[0] + 10} sends no MouseClick", outside)
    before = len(presses(t))
    sgr_click(t, first + 1, ROWS - 1)
    inside = presses(t)[before:]
    check(len(inside) == 1, f"a click on the first tab label sends one MouseClick ({len(inside)})", inside)
    check(len(inside) == 1 and "target=Tab(0)" in inside[0], "that click hits the first tab", inside)


def main():
    run("1 right sidebar, no peer", "right", 24, None, from_right_pane)
    run("2 right sidebar, smaller peer", "right", 24, (70, 24), from_right_pane)
    run("3 right sidebar, smaller peer, from the left pane", "right", 24, (70, 24),
        from_left_pane)
    run("4 bottom sidebar, smaller peer", "bottom", 8, (70, 20), from_only_pane_down)
    run("5 left sidebar, smaller peer", "left", 24, (70, 24), from_left_pane_left)
    run("6 client bar past a narrower frame", "right", 24, PEER, bar_clicks)
    print(f"\n{'FAILED' if failures else 'OK'}: {len(failures)} failure(s)")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
