#!/usr/bin/env python3
"""Float zoom (real-PTY harness): `PaneToggleZoomFloat` through the real client.

With a 34-column left sidebar and two panes:

  1. A float draws a framed box over the layout and sizes the floated pane's
     PTY to the box interior (`stty size` in the pane).
  2. The other pane keeps its frame and its size.
  3. Toggling again snaps the pane back to its original size.
  4. A click on the other pane, on a sidebar row, on the sidebar's frame and
     on the bar's tail each snap the float back. The first one also moves
     focus to the pane clicked.
  5. Alt-h snaps the float back and moves focus left.

The interior size is derived from what the pane reports before the split, so
the script does not restate the sidebar width: the area the layout fills is
the single pane's size plus its 1-cell frame.

Run from the repo root: python3 tests/pty/float_zoom.py
"""
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pty_harness import Tui  # noqa: E402

COLS, ROWS = 160, 48
ALT_U = b"\x1bu"
CONFIG = """
[keybindings.command]
"Alt-u" = "PaneToggleZoomFloat"

[[sidebar]]
edge = "left"
size = 34
visible = true
  [[sidebar.panel]]
  plugin = "agents"
  [[sidebar.panel]]
  plugin = "sessions"
"""

failures = []


def check(ok, msg):
    print(("  PASS  " if ok else "  FAIL  ") + msg)
    if not ok:
        failures.append(msg)


def sgr_click(col, row):
    """A left press and release at 0-based `(col, row)`; SGR is 1-based."""
    return (f"\x1b[<0;{col + 1};{row + 1}M".encode()
            + f"\x1b[<0;{col + 1};{row + 1}m".encode())


def size_in_pane(t, tag):
    """`rows x cols` as the focused pane's own tty reports it."""
    t.send(f"echo {tag}=$(stty size | tr ' ' x)\r", 0.9)
    for row in t.rows_text():
        m = re.search(tag + r"=(\d+x\d+)", row)
        if m and "echo" not in row:
            return m.group(1)
    return None


def box_corners(t):
    """Columns of every top-left corner on the rows that hold one, by row."""
    return {y: [x for x, ch in enumerate(r) if ch == "╭"]
            for y, r in enumerate(t.rows_text()) if "╭" in r}


def main():
    t = Tui("/tmp/rmxfloat/pty", cols=COLS, rows=ROWS, config=CONFIG)
    try:
        t.start()
        t.pump(1.6)
        alone = size_in_pane(t, "A0")
        rows0, cols0 = (int(n) for n in alone.split("x"))
        area_h, area_w = rows0 + 2, cols0 + 2
        want_h, want_w = int(area_h * 0.9) - 2, int(area_w * 0.9) - 2
        print(f"single pane {alone}: area {area_w}x{area_h}, float interior {want_h}x{want_w}")

        t.prefix(b"pv", 1.8)
        right_before = size_in_pane(t, "R0")
        t.dump("two panes")
        corners_before = box_corners(t)

        t.send(ALT_U, 1.5)
        t.dump("float up")
        floated = size_in_pane(t, "F1")
        check(floated == f"{want_h}x{want_w}", f"the floated pane is {want_h}x{want_w} (got {floated})")
        check(" Z" in t.rows_text()[-1], "the status bar shows Z")
        corners = box_corners(t)
        check(any(len(xs) >= 2 for xs in corners.values())
              or sum(len(xs) for xs in corners.values()) > sum(len(xs) for xs in corners_before.values()),
              "a framed box is drawn over the layout")

        t.send(ALT_U, 1.5)
        check(" Z" not in t.rows_text()[-1], "the second toggle ends the float")
        right_after = size_in_pane(t, "R1")
        check(right_after == right_before, f"the pane is back at {right_before} (got {right_after})")

        def float_up():
            t.send(ALT_U, 1.5)
            return " Z" in t.rows_text()[-1]

        # A click on the pane to the left, outside the box.
        check(float_up(), "the float comes up again")
        t.send(sgr_click(36, 2), 1.5)
        check(" Z" not in t.rows_text()[-1], "a click outside the box snaps the float back")
        t.send("echo LEFT-CLICKED\r", 0.9)
        left_rows = [r[:80] for r in t.rows_text()]
        check(any("LEFT-CLICKED" in r and "echo" not in r for r in left_rows),
              "and the click focused the left pane")

        # Presses the client answers itself: a sidebar row, the sidebar frame,
        # the bar's tail.
        for label, (cx, cy) in [("a sidebar row", (5, 10)),
                                ("the sidebar's frame", (33, 2)),
                                ("the bar's tail", (COLS - 1, ROWS - 1))]:
            t.send(sgr_click(130, 2), 0.8)  # take the keyboard back to the content
            check(float_up(), f"the float is up before a press on {label}")
            t.send(sgr_click(cx, cy), 1.5)
            check(" Z" not in t.rows_text()[-1], f"a press on {label} snaps the float back")

        # Alt-h snaps back and moves focus left.
        t.send(sgr_click(130, 2), 0.8)
        t.send(b"\x1bl", 0.8)
        check(float_up(), "the float is up before Alt-h")
        t.send(b"\x1bh", 1.5)
        check(" Z" not in t.rows_text()[-1], "Alt-h snaps the float back")
        t.send("echo ALTH-LEFT\r", 0.9)
        check(any("ALTH-LEFT" in r[:80] and "echo" not in r for r in t.rows_text()),
              "and moves focus to the left pane")

        check(t.alive(), "the client is alive")
        for which in ("server", "client"):
            check("panicked" not in t.log(which), f"no panic in {which}.log")
    finally:
        t.kill()
    print("ALL PASS" if not failures else f"FAILED: {len(failures)}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
