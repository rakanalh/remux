#!/usr/bin/env python3
"""After the terminal is resized, Prefix shows ONE which-key popup, not two.

Reported on a 32-inch macOS screen: `Ctrl-a` drew two identical popups, one
near the top left and one at the bottom, the second with garbled cells.

The keypress drew the popup at the terminal's CURRENT size. Every server frame
after that (`FullRender`, `RenderDiff`, `ScrollRender`, i.e. any pane output)
re-laid the popup through `relay_overlays` at the size the client STARTED at,
which the event loop held in a `let (cols, rows)` that a resize never updated.
So a client started small and then enlarged painted a second, smaller-terminal
popup up and to the left, and the pane output kept overwriting cells of the
first one, which nothing re-laid.

A pane runs a ticker, so frames keep arriving while the popup is open: without
one the second popup never appears, which is why a bare open-and-look passes.

Cases, each after a ticker is started in the pane:
  1  start 80x30, resize to 220x60, Prefix: one popup, anchored to the bottom
     row and centred on the NEW width
  2  start 100x30, resize to 256x90 (past a u8): one popup, same placement
  3  start 300x100 with no resize (control): one popup
  4  resize while the popup is open: one popup, at the new placement
  5  Esc closes it: no popup cells left; Prefix again: one popup
  6  with a right sidebar: one popup after a resize
  7  the client is alive and neither log holds a panic

Run from the repo root: python3 tests/pty/whichkey_after_resize.py
"""
import re
import sys

from pty_harness import Tui
from rundirs import run_dir  # noqa: E402

RUNDIR = run_dir("rmx-wkr")
HEADER = ": command palette"
TICKER = "while :; do printf 'TICK%s\\n' x; sleep 0.2; done\r"

SIDEBAR = """
[[sidebar]]
edge = "right"
size = 30
visible = true
  [[sidebar.panel]]
  plugin = "placeholder"
  weight = 1
"""

failures = []


def check(name, ok, detail=""):
    print(f"{'PASS' if ok else 'FAIL'}  {name}" + (f"  ({detail})" if detail and not ok else ""))
    if not ok:
        failures.append(name)


def headers(t):
    """(row, col) of every popup header on screen, by cell column."""
    out = []
    for y, row in enumerate(t.cell_rows()):
        for m in re.finditer(re.escape(HEADER), row):
            out.append((y, m.start()))
    return out


def popup_box(t, y, x):
    """The popup's left border column and bottom border row, walked from its header.

    The header is `: command palette` at `start_x + 2`, so the left border is two
    columns left of it; the bottom border is the first row below whose cell
    there is the bottom-left corner. The width is read off that bottom border,
    since the header row's last `│` may be a pane border.
    """
    bx = x - 2
    buf = t.screen.buffer
    for yy in range(y, t.rows):
        if buf[yy][bx].data == "╰":
            return bx, yy
    return bx, None


def check_placement(t, name):
    """Exactly one popup, anchored to the last row and centred on the live width."""
    hs = headers(t)
    check(f"{name}: exactly one popup", len(hs) == 1, f"headers at {hs}")
    if len(hs) != 1:
        t.dump(name)
        return
    y, x = hs[0]
    bx, bottom = popup_box(t, y, x)
    check(f"{name}: popup is anchored to the last row", bottom == t.rows - 1,
          f"bottom border at {bottom}, rows {t.rows}")
    if bottom is None:
        return
    right = t.cell_rows()[bottom].find("╯", bx)
    width = right - bx + 1
    expected_x = (t.cols - width) // 2
    check(f"{name}: popup is centred on the current width", bx == expected_x,
          f"left border at {bx}, width {width}, expected {expected_x} for {t.cols} cols")


def open_popup(t):
    t.send(b"\x01", t=2.0)


def start(cols, rows, config=None):
    t = Tui(RUNDIR, cols=cols, rows=rows, config=config).start()
    t.send(TICKER, t=1.0)
    return t


def logs_clean(t, name):
    for which in ("client", "server"):
        log = t.log(which)
        check(f"{name}: no panic in {which}.log", "panicked" not in log)


def resized_then_open(name, start_size, new_size, config=None):
    t = start(*start_size, config=config)
    try:
        t.resize(*new_size)
        open_popup(t)
        check_placement(t, name)
        check(f"{name}: client alive", t.alive())
    finally:
        t.kill()
    logs_clean(t, name)


def main():
    resized_then_open("1 80x30 -> 220x60", (80, 30), (220, 60))
    resized_then_open("2 100x30 -> 256x90", (100, 30), (256, 90))

    t = start(300, 100)
    try:
        open_popup(t)
        check_placement(t, "3 300x100, no resize")
    finally:
        t.kill()
    logs_clean(t, "3")

    t = start(80, 30)
    try:
        t.resize(120, 40)
        open_popup(t)
        t.resize(200, 60, t=2.0)
        check_placement(t, "4 resized while open")

        t.send(b"\x1b", t=1.5)
        check("5 Esc leaves no popup cells", headers(t) == [], f"headers at {headers(t)}")
        stale = [y for y, r in enumerate(t.cell_rows()) if "send prefix" in r or "Alt-t new tab" in r]
        check("5 Esc leaves no popup rows", stale == [], f"rows {stale}")
        open_popup(t)
        check_placement(t, "5 reopened")
        check("5 client alive", t.alive())
    finally:
        t.kill()
    logs_clean(t, "4/5")

    resized_then_open("6 sidebar 90x30 -> 230x70", (90, 30), (230, 70), config=SIDEBAR)

    print()
    if failures:
        print(f"{len(failures)} FAILED: {failures}")
        sys.exit(1)
    print("all passed")


if __name__ == "__main__":
    main()
