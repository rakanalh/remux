#!/usr/bin/env python3
"""`PaneClearScrollback`, `ClearPaneScrollback` and `PaneClear` (frame harness).

Cmd+K is `PaneClearScrollback`: the server blanks the pane that takes input,
drops its scrollback and writes `^L`, so the shell redraws its prompt. A View
cell sends `ClearPaneScrollback { pane_id }` for the same thing. Cmd+L is
`PaneClear`: only the `^L`, and the scrollback stays.

The history is `HIST_<n>` lines printed by `seq`, and the prompt is `P42> `,
which bash builds from `$((6*7))`. Neither string is in a typed command, so a
command echoed on screen cannot satisfy a check. The prompt after a clear can
only come from the shell redrawing it on the `^L`: the server leaves the grid
blank.

What it covers:

  1  baseline: the history is reachable by scrolling up
  2  `PaneClearScrollback`: no `HIST_` is left on the live screen, scrolling up
     reveals none, the viewport is at line 0, and the shell redrew `P42> ` on
     the top row
  3  `ClearPaneScrollback` by pane id, from a second client that is attached
     to nothing (a View's position), clears the same way
  4  `PaneClear`: the shell redrew `P42> ` on the top row and the history is
     still reachable by scrolling up
  5  no panic in the server log

Run from the repo root:  python3 tests/frame/pane_clear_scrollback.py
"""
import os
import re
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import Client, Server, name_of, only  # noqa: E402
from rundirs import run_dir  # noqa: E402

RUNDIR = run_dir("rmx_pcs")
COLS, ROWS = 100, 30
PROMPT = "P42> "
FAILURES = []


def check(name, cond, detail=""):
    print(f"  {'PASS' if cond else 'FAIL'}  {name}" + ("" if cond else f"\n        {detail}"))
    if not cond:
        FAILURES.append(name)


class Grid:
    """The composited grid, rebuilt from FullRender and RenderDiff. A
    ScrollRender is never applied: it only arrives for an incremental scroll,
    and every scroll here waits for the FullRender a jump of 20 lines sends."""

    def __init__(self):
        self.g = [[" "] * COLS for _ in range(ROWS)]
        self.viewport_top = None
        self.scrolls = 0

    @staticmethod
    def _ch(cell):
        return cell.get("c", " ") if isinstance(cell, dict) else " "

    def apply(self, msg):
        n = name_of(msg)
        body = only(msg, n)
        if n == "FullRender":
            for y, row in enumerate(body["cells"][:ROWS]):
                for x, cell in enumerate(row[:COLS]):
                    self.g[y][x] = self._ch(cell)
        elif n == "RenderDiff":
            for ch in body["changes"]:
                if ch["y"] < ROWS and ch["x"] < COLS:
                    self.g[ch["y"]][ch["x"]] = self._ch(ch["cell"])
        elif n == "ScrollRender":
            self.scrolls += 1
        else:
            return
        self.viewport_top = body.get("viewport_top", self.viewport_top)

    def text(self):
        return "\n".join("".join(r) for r in self.g)

    def rows(self):
        return ["".join(r) for r in self.g]

    def pane_top(self):
        """The pane's first content row: inside the top border and the left one."""
        return "".join(self.g[1][1:])

    def hist(self):
        return sorted({int(m.group(1)) for m in re.finditer(r"HIST_(\d+)", self.text())})


def pump(c, grid, t=0.6):
    for m in c.drain(t):
        grid.apply(m)


def pane_ids(c):
    c.send("ListSessionTree")
    tree = None
    for m in c.drain(0.8):
        if name_of(m) == "SessionTree":
            tree = m["SessionTree"]
    assert tree is not None, "no SessionTree reply"
    sessions = list(tree.get("unfiled", []))
    for f in tree.get("folders", []):
        sessions += f.get("sessions", [])
    return [p["id"] for s in sessions for t in s["tabs"] for p in t["panes"]]


def fill(c, grid):
    c.send({"Input": {"data": list(b"seq -f 'HIST_%g' 1 200\n")}})
    time.sleep(1.2)
    pump(c, grid, 0.8)


def scrolled_hist(c, grid):
    """The `HIST_` numbers on screen 20 lines up, then back at the tail."""
    c.send({"ScrollDelta": {"delta": 20}})
    time.sleep(0.3)
    pump(c, grid, 0.5)
    seen, top = grid.hist(), grid.viewport_top
    c.send({"ScrollDelta": {"delta": -1000}})
    time.sleep(0.3)
    pump(c, grid, 0.5)
    return seen, top


def check_cleared(label, c, grid):
    live = grid.hist()
    check(f"{label}: no HIST_ line is left on the live screen", not live, live[:8])
    check(f"{label}: the shell redrew the prompt on the top row",
          grid.pane_top().startswith(PROMPT), repr(grid.pane_top()[:40]))
    seen, top = scrolled_hist(c, grid)
    check(f"{label}: scrolling up reveals no history", not seen, seen[:8])
    check(f"{label}: the viewport stays at line 0", top == 0, top)


def main():
    os.environ["PS1"] = "P$((6*7))> "
    srv = Server(RUNDIR).start()
    try:
        c = Client(srv.sock)
        c.hello()
        c.send({"CreateSession": {"name": "main", "folder": None}})
        c.send({"Attach": {"session_name": "main"}})
        c.send({"Resize": {"cols": COLS, "rows": ROWS}})
        time.sleep(0.5)
        grid = Grid()
        pump(c, grid)
        check("the prompt is assembled by the shell",
              grid.pane_top().startswith(PROMPT), grid.rows()[:3])

        fill(c, grid)
        seen, top = scrolled_hist(c, grid)
        check("1 baseline: scrolling up reaches the history", bool(seen) and top > 0,
              (seen[:4], top))

        c.send({"Command": "PaneClearScrollback"})
        time.sleep(0.8)
        pump(c, grid)
        check_cleared("2 PaneClearScrollback", c, grid)

        fill(c, grid)
        (pane,) = pane_ids(c)
        other = Client(srv.sock)
        other.hello()
        other.send({"ClearPaneScrollback": {"pane_id": pane}})
        time.sleep(0.8)
        pump(c, grid)
        check_cleared("3 ClearPaneScrollback by pane id", c, grid)
        other.close()

        fill(c, grid)
        c.send({"Command": "PaneClear"})
        time.sleep(0.8)
        pump(c, grid)
        check("4 PaneClear: the shell redrew the prompt on the top row",
              grid.pane_top().startswith(PROMPT), repr(grid.pane_top()[:40]))
        seen, top = scrolled_hist(c, grid)
        check("4 PaneClear: the history is still reachable", bool(seen) and top > 0,
              (seen[:4], top))
        c.close()
    except (ConnectionError, OSError, AssertionError) as e:
        check("the connection survives the run", False, repr(e))
    finally:
        srv.kill()
    log = srv.log()
    check("5 no panic in the server log", "panicked at" not in log)
    if FAILURES:
        print(f"\n{len(FAILURES)} FAILED: {FAILURES}\nlog: {RUNDIR}/state/remux/server.log")
        sys.exit(1)
    print("\nall passed")


if __name__ == "__main__":
    main()
