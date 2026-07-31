"""`PaneNewInTab` targets a BACKGROUND tab, and leaves it structurally sound.

`create_pane_in_tab`'s `tab_index: Some(n)` branch is reached only by
`PaneNewInTab` (the session manager's "new pane in that tab"), and only it can
mutate a tab that nothing is currently rendering -- which is exactly where a
`pane_order`/tree divergence would go unnoticed. The debug build's
`debug_check_invariant` runs on that mutation, so a divergence panics the server
here rather than surfacing later as a restored pane with no PTY.

  1. Two tabs; go back to tab 0 so tab 1 is in the background.
  2. `PaneNewInTab { tab_index: 1 }` -- tab 0 must be untouched.
  3. Switch to tab 1: the new pane is there (2 boxes), and the server never
     panicked on the invariant.

Run: python3 tests/frame/pane_new_in_background_tab.py
"""
import sys
from harness import Server, Client, name_of, only

RUNDIR = "/tmp/rmxpnb"
COLS, ROWS = 100, 30

fails = []


def check(cond, msg):
    if cond:
        print(f"  PASS {msg}")
    else:
        print(f"  FAIL {msg}")
        fails.append(msg)


class Grid:
    def __init__(self, cols, rows):
        self.cols, self.rows = cols, rows
        self.g = [[" "] * cols for _ in range(rows)]

    def _put(self, y, x, cell):
        if 0 <= y < self.rows and 0 <= x < self.cols:
            self.g[y][x] = cell.get("c", " ") if isinstance(cell, dict) else " "

    def apply(self, msg):
        n = name_of(msg)
        body = only(msg, n)
        if n == "FullRender":
            for y, row in enumerate(body["cells"]):
                for x, cell in enumerate(row):
                    self._put(y, x, cell)
        elif n == "RenderDiff":
            for ch in body["changes"]:
                self._put(ch["y"], ch["x"], ch["cell"])

    def boxes(self):
        found = []
        for y in range(self.rows - 1):
            for x in range(self.cols):
                if self.g[y][x] != "╭":
                    continue
                x2 = next((c for c in range(x + 1, self.cols)
                           if self.g[y][c] == "╮"), None)
                if x2 is None:
                    continue
                y2 = next((r for r in range(y + 1, self.rows - 1)
                           if self.g[r][x] == "╰" and self.g[r][x2] == "╯"), None)
                if y2 is not None:
                    found.append((x, y, x2 - x + 1, y2 - y + 1))
        return found


def snapshot(cli):
    g = Grid(COLS, ROWS)
    cli.send({"Resize": {"cols": COLS, "rows": ROWS}})
    for m in cli.drain(0.7):
        g.apply(m)
    return g


def main():
    srv = Server(RUNDIR).start()
    cli = Client(srv.sock)
    try:
        cli.hello()
        cli.send({"CreateSession": {"name": "main", "folder": None}})
        cli.send({"Attach": {"session_name": "main"}})
        cli.send({"Resize": {"cols": COLS, "rows": ROWS}})
        cli.drain(0.8)

        # Tab 1, then back to tab 0 so tab 1 is in the background.
        cli.send({"Command": "TabNew"})
        cli.drain(0.8)
        cli.send({"Command": "TabPrev"})
        cli.drain(0.8)
        g = snapshot(cli)
        check(len(g.boxes()) == 1, f"tab 0 starts with one pane ({len(g.boxes())})")

        cli.send({"Command": {"PaneNewInTab": {"session": "main", "tab_index": 1}}})
        cli.drain(1.0)
        g = snapshot(cli)
        check(len(g.boxes()) == 1,
              f"the foreground tab is untouched ({len(g.boxes())} boxes)")

        cli.send({"Command": "TabNext"})
        cli.drain(1.0)
        g = snapshot(cli)
        n = len(g.boxes())
        print(f"  background tab after PaneNewInTab: boxes={n}")
        check(n == 2, f"the pane landed in tab 1: 2 boxes, got {n}")

        log = srv.log()
        check("panicked" not in log, "no panic in server log (invariant held)")
    finally:
        cli.close()
        srv.kill()

    print()
    if fails:
        print(f"FAILED ({len(fails)}):")
        for f in fails:
            print("  -", f)
        sys.exit(1)
    print("ALL PASS")


if __name__ == "__main__":
    main()
