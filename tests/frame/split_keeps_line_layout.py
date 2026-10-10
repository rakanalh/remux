"""A split along the Columns/Rows axis keeps the layout (frame harness).

  1. In `columns`, `PaneSplitVertical` adds a column: the status bar still says
     columns, and the frame shows one more full-height box.
  2. In `columns`, a `CliSpawn` `SplitRight` (what `remux split --right` sends)
     does the same, because it reaches the same pane-creation path.
  3. In `rows`, `PaneSplitHorizontal` adds a row and the tab stays in rows.
  4. Negative controls: `PaneSplitHorizontal` in columns and `PaneSplitVertical`
     in rows still eject to custom.
  5. No panic in any server log.

Run: python3 tests/frame/split_keeps_line_layout.py
"""
import os
import re
import sys
import time
from harness import Server, Client, name_of, only

RUNDIR = "/tmp/rmxsplk"
COLS, ROWS = 100, 31
LAYOUTS = ("monocle", "master", "custom", "columns", "rows", "grid", "bsp")

fails = []


def check(cond, msg):
    if cond:
        print(f"  PASS {msg}")
    else:
        print(f"  FAIL {msg}")
        fails.append(msg)


class Grid:
    def __init__(self):
        self.g = [[" "] * COLS for _ in range(ROWS)]

    def _put(self, y, x, cell):
        if 0 <= y < ROWS and 0 <= x < COLS:
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

    def layout_name(self):
        words = set(re.findall(r"[a-z]+", "".join(self.g[ROWS - 1])))
        hits = [c for c in LAYOUTS if c in words]
        return hits[0] if len(hits) == 1 else f"?{hits}"

    def boxes(self):
        """(x, y, w, h) of every complete rounded box, in reading order."""
        found = []
        for y in range(ROWS - 1):
            for x in range(COLS):
                if self.g[y][x] != "╭":
                    continue
                x2 = next((c for c in range(x + 1, COLS) if self.g[y][c] == "╮"), None)
                if x2 is None:
                    continue
                y2 = next((r for r in range(y + 1, ROWS - 1)
                           if self.g[r][x] == "╰" and self.g[r][x2] == "╯"), None)
                if y2 is not None:
                    found.append((x, y, x2 - x + 1, y2 - y + 1))
        return found


def snapshot(cli):
    """Re-`Resize` to the same size to force a `FullRender`."""
    g = Grid()
    cli.send({"Resize": {"cols": COLS, "rows": ROWS}})
    for m in cli.drain(0.7):
        g.apply(m)
    return g


def cmd(cli, c):
    cli.send({"Command": c})
    cli.drain(0.5)


def full_height(boxes):
    return all(b[1] == 0 and b[3] == ROWS - 1 for b in boxes)


def full_width(boxes):
    return all(b[0] == 0 and b[2] == COLS for b in boxes)


def with_server(layout, body):
    srv = Server(RUNDIR).start(config=f'[appearance]\ndefault_layout = "{layout}"\n')
    cli = Client(srv.sock)
    try:
        cli.hello()
        cli.send({"CreateSession": {"name": "main", "folder": None}})
        cli.send({"Attach": {"session_name": "main"}})
        cli.send({"Resize": {"cols": COLS, "rows": ROWS}})
        cli.drain(0.8)
        cmd(cli, "PaneNew")
        g = snapshot(cli)
        check(g.layout_name() == layout and len(g.boxes()) == 2,
              f"setup: two panes in {layout} ({g.layout_name()}, {len(g.boxes())} boxes)")
        body(srv, cli)
        check("panicked" not in srv.log(), "no panic in server log")
    finally:
        cli.close()
        srv.kill()


def split_keeps(layout, command, shape, shape_name):
    print(f"{layout}: {command} keeps the layout")

    def body(srv, cli):
        cmd(cli, command)
        g = snapshot(cli)
        boxes = g.boxes()
        check(g.layout_name() == layout, f"still {layout} after {command} ({g.layout_name()})")
        check(len(boxes) == 3, f"{command} added a pane (3 boxes, got {len(boxes)})")
        check(shape(boxes), f"every pane is {shape_name} ({boxes})")

    with_server(layout, body)


def cli_split_right_keeps_columns():
    print("columns: CliSpawn SplitRight keeps the layout")

    def body(srv, cli):
        caller = Client(srv.sock)
        caller.hello()
        caller.send({"CliSpawn": {"session": "main", "placement": "SplitRight"}})
        reply = None
        end = time.time() + 3.0
        while reply is None and time.time() < end:
            reply = next((m for m in caller.drain(0.3)
                          if name_of(m) in ("CliSpawned", "Error")), None)
        caller.close()
        check(reply is not None and name_of(reply) == "CliSpawned",
              f"CliSpawn SplitRight is answered ({reply})")
        cli.drain(0.5)
        g = snapshot(cli)
        boxes = g.boxes()
        check(g.layout_name() == "columns", f"still columns ({g.layout_name()})")
        check(len(boxes) == 3, f"CliSpawn added a pane (3 boxes, got {len(boxes)})")
        check(full_height(boxes), f"every pane is a full-height column ({boxes})")

    with_server("columns", body)


def cross_split_ejects(layout, command):
    print(f"{layout}: {command} still ejects to custom")

    def body(srv, cli):
        cmd(cli, command)
        g = snapshot(cli)
        check(g.layout_name() == "custom", f"{command} in {layout} ejects ({g.layout_name()})")
        check(len(g.boxes()) == 3, f"{command} added a pane (got {len(g.boxes())})")

    with_server(layout, body)


def main():
    os.environ["HOME"] = RUNDIR
    split_keeps("columns", "PaneSplitVertical", full_height, "a full-height column")
    cli_split_right_keeps_columns()
    split_keeps("rows", "PaneSplitHorizontal", full_width, "a full-width row")
    cross_split_ejects("columns", "PaneSplitHorizontal")
    cross_split_ejects("rows", "PaneSplitVertical")

    print()
    if fails:
        print(f"FAILED ({len(fails)}):")
        for f in fails:
            print("  -", f)
        sys.exit(1)
    print("ALL PASS")


if __name__ == "__main__":
    main()
