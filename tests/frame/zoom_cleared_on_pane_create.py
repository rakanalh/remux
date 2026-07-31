"""Every pane-creating command clears the tab's zoom.

`zoomed_pane` is an `Option<PaneId>` that the render and PTY-sizing paths now
HONOUR (they paint the pane it names, not whatever is focused). That only stays
correct if nothing leaves a stale id behind, and a new pane means a new
arrangement -- the old full-area pane is no longer what the user asked for. Four
of the five creating commands cleared it; `PaneStackAdd` did not, which was
harmless only for as long as the payload was ignored.

Each command gets a fresh session: zoom, create a pane, and assert the status
bar's `Z` flag is gone. `PaneNewInTab` is driven by explicit target, so it is
checked against its own session by name.

Run: python3 tests/frame/zoom_cleared_on_pane_create.py
"""
import sys
from harness import Server, Client, name_of, only

RUNDIR = "/tmp/rmxzcl"
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

    def status_row(self):
        return "".join(self.g[self.rows - 1])


def snapshot(cli):
    g = Grid(COLS, ROWS)
    cli.send({"Resize": {"cols": COLS, "rows": ROWS}})
    for m in cli.drain(0.7):
        g.apply(m)
    return g


def zoomed_session(cli, name):
    cli.send({"CreateSession": {"name": name, "folder": None}})
    cli.send({"Attach": {"session_name": name}})
    cli.send({"Resize": {"cols": COLS, "rows": ROWS}})
    cli.drain(0.8)
    cli.send({"Command": "PaneToggleZoom"})
    cli.drain(0.6)
    g = snapshot(cli)
    check("Z" in g.status_row(), f"[{name}] zoom engaged (status bar shows Z)")


def main():
    srv = Server(RUNDIR).start()
    cli = Client(srv.sock)
    try:
        cli.hello()

        cases = [
            ("PaneNew", "PaneNew"),
            ("PaneSplitVertical", "PaneSplitVertical"),
            ("PaneSplitHorizontal", "PaneSplitHorizontal"),
            ("PaneStackAdd", "PaneStackAdd"),
        ]
        for i, (label, command) in enumerate(cases):
            name = f"s{i}"
            zoomed_session(cli, name)
            cli.send({"Command": command})
            cli.drain(0.8)
            g = snapshot(cli)
            check("Z" not in g.status_row(), f"[{name}] {label} clears the zoom")

        # PaneNewInTab targets a named session/tab rather than the focused one.
        zoomed_session(cli, "target")
        cli.send({"Command": {"PaneNewInTab": {"session": "target", "tab_index": 0}}})
        cli.drain(0.8)
        g = snapshot(cli)
        check("Z" not in g.status_row(), "[target] PaneNewInTab clears the zoom")

        log = srv.log()
        check("panicked" not in log, "no panic in server log")
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
