"""Columns and Rows automatic layouts (frame harness).

Three panes in an automatic layout, cycled with `LayoutNext`:

  1. The cycle runs bsp -> master -> monocle -> grid -> columns -> rows -> bsp,
     read off the status bar after every step.
  2. At `columns`, every pane reports `stty size`: equal heights spanning the
     full pane area, widths equal within one cell, left to right in pane order.
     The rounded boxes say the same thing from the frame's side.
  3. At `rows`, the transpose: equal widths spanning the full area, heights
     equal within one cell, top to bottom.
  4. With a remembered Custom layout, `rows` (not `grid`) is where the cycle
     turns back to `custom`.
  5. With `[layouts] master = false, grid = false` in config.toml, the cycle is
     bsp -> monocle -> columns -> rows -> bsp.
  6. With `[layouts] bsp = false`, a new session and a new tab both start in
     master, the first enabled layout, instead of bsp.
  7. With `default_layout = "columns"`, a new session and a new tab both start
     in columns; with `default_layout = "grid"` and `grid = false`, both fall
     back to bsp, the first enabled layout.
  8. With `default_layout = "columns"`, a tab made by moving a pane to a NEW
     tab and one made by a `CliSpawn` NewTab also start in columns.
  9. With `grid = false`, a new View starts in bsp and its cycle skips grid.
 10. No panic in any server log.

Run: python3 tests/frame/layout_columns_rows.py
"""
import os
import re
import sys
from harness import Server, Client, name_of, only

RUNDIR = "/tmp/rmxlcr"
SOCK = f"{RUNDIR}/run/remux.sock"
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
    """Reconstruct the composited grid from FullRender/RenderDiff."""

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

    def text(self):
        return ["".join(r) for r in self.g]

    def status_row_has(self, text):
        return text in "".join(self.g[self.rows - 1])

    def layout_name(self):
        words = set(re.findall(r"[a-z]+", "".join(self.g[self.rows - 1])))
        hits = [c for c in LAYOUTS if c in words]
        return hits[0] if len(hits) == 1 else f"?{hits}"

    def boxes(self):
        """(x, y, w, h) of every complete rounded box, in reading order."""
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

    def find_all(self, pattern):
        """Every match in every row. `finditer`, because panes share rows."""
        rx = re.compile(pattern)
        return [m.groups() for row in self.text() for m in rx.finditer(row)]


def snapshot(cli):
    """A fresh full grid: re-`Resize` to the same size to force a `FullRender`."""
    g = Grid(COLS, ROWS)
    cli.send({"Resize": {"cols": COLS, "rows": ROWS}})
    for m in cli.drain(0.7):
        g.apply(m)
    return g


def cmd(cli, c):
    cli.send({"Command": c})
    cli.drain(0.4)


def type_line(cli, line):
    cli.send({"Input": {"data": list((line + "\n").encode())}})
    cli.drain(0.6)


def size_of(cli, tag):
    """Have the focused pane report `stty size` under `tag`: (rows, cols)."""
    type_line(cli, f"clear; printf '{tag}:%s\\n' \"$(stty size)\"")
    got = snapshot(cli).find_all(rf"{tag}:(\d+) (\d+)")
    return (int(got[0][0]), int(got[0][1])) if len(got) == 1 else None


def sizes_walking(cli, prefix, back):
    """Sizes of all three panes, starting at the focused last one and moving
    `back` twice, returned in pane order (first pane first)."""
    out = [size_of(cli, f"{prefix}2")]
    cmd(cli, back)
    out.append(size_of(cli, f"{prefix}1"))
    cmd(cli, back)
    out.append(size_of(cli, f"{prefix}0"))
    return out[::-1]


def within_one(xs):
    return max(xs) - min(xs) <= 1


def with_server(config, body):
    srv = Server(RUNDIR).start(config=config)
    cli = Client(srv.sock)
    try:
        cli.hello()
        cli.send({"CreateSession": {"name": "main", "folder": None}})
        cli.send({"Attach": {"session_name": "main"}})
        cli.send({"Resize": {"cols": COLS, "rows": ROWS}})
        cli.drain(0.8)
        body(cli)
        check("panicked" not in srv.log(), "no panic in server log")
    finally:
        cli.close()
        srv.kill()


def disabled_layouts_are_skipped():
    print("[layouts] master = false, grid = false")

    def body(cli):
        cmd(cli, "PaneNew")
        cmd(cli, "PaneNew")
        seen = [snapshot(cli).layout_name()]
        for _ in range(4):
            cmd(cli, "LayoutNext")
            seen.append(snapshot(cli).layout_name())
        print(f"  seen: {seen}")
        check(seen == ["bsp", "monocle", "columns", "rows", "bsp"],
              "the cycle skips master and grid")

    with_server("[layouts]\nmaster = false\ngrid = false\n", body)


def disabled_default_falls_back():
    print("[layouts] bsp = false")

    def body(cli):
        check(snapshot(cli).layout_name() == "master",
              f"a new session starts in master ({snapshot(cli).layout_name()})")
        cmd(cli, "TabNew")
        check(snapshot(cli).layout_name() == "master",
              f"a new tab starts in master ({snapshot(cli).layout_name()})")

    with_server("[layouts]\nbsp = false\n", body)


def starts_in(expected):
    def body(cli):
        got = snapshot(cli).layout_name()
        check(got == expected, f"a new session starts in {expected} ({got})")
        cmd(cli, "TabNew")
        got = snapshot(cli).layout_name()
        check(got == expected, f"a new tab starts in {expected} ({got})")
    return body


def default_layout_applies_to_new_tabs():
    print('[appearance] default_layout = "columns"')
    with_server('[appearance]\ndefault_layout = "columns"\n', starts_in("columns"))
    print('[appearance] default_layout = "grid", [layouts] grid = false')
    with_server('[appearance]\ndefault_layout = "grid"\n[layouts]\ngrid = false\n',
                starts_in("bsp"))


def default_layout_applies_to_moved_and_cli_tabs():
    print('[appearance] default_layout = "columns": moved and CLI tabs')

    def body(cli):
        cmd(cli, "PaneNew")
        cmd(cli, "PaneNew")
        cmd(cli, {"PaneMoveToTabTarget": {"tab_id": None}})
        g = snapshot(cli)
        check(g.status_row_has("Tab 2") and g.layout_name() == "columns",
              f"a pane moved to a new tab starts in columns ({g.layout_name()})")

        caller = Client(SOCK)
        caller.hello()
        caller.send({"CliSpawn": {"session": "main", "placement": "NewTab"}})
        reply = next((m for m in caller.drain(3.0)
                      if name_of(m) in ("CliSpawned", "Error")), None)
        caller.close()
        check(reply is not None and name_of(reply) == "CliSpawned",
              f"CliSpawn NewTab is answered ({reply})")
        g = snapshot(cli)
        check(g.status_row_has("Tab 3") and g.layout_name() == "columns",
              f"a CliSpawn new tab starts in columns ({g.layout_name()})")

    with_server('[appearance]\ndefault_layout = "columns"\n', body)


def view_layout(msgs, view_id):
    """The layout name of view `view_id` in the LAST ViewList among `msgs`."""
    views = None
    for m in msgs:
        if name_of(m) == "ViewList":
            views = only(m, "ViewList")["views"]
    for v in views or []:
        if v["id"] == view_id:
            layout = v["layout"]
            return (next(iter(layout)) if isinstance(layout, dict) else layout).lower()
    return None


def views_skip_disabled_layouts():
    print("[layouts] grid = false: Views")

    def body(cli):
        cli.send({"ViewCreate": {"name": "V"}})
        msgs = cli.drain(1.0)
        created = next((only(m, "ViewCreated")["id"] for m in msgs
                        if name_of(m) == "ViewCreated"), None)
        check(created is not None, "ViewCreate is answered with ViewCreated")
        first = view_layout(msgs, created)
        check(first == "bsp", f"a new View starts in bsp, not grid ({first})")
        seen = [first]
        for _ in range(6):
            cli.send({"ViewCycleLayout": {"id": created}})
            seen.append(view_layout(cli.drain(0.6), created))
        print(f"  seen: {seen}")
        check(seen == ["bsp", "master", "monocle", "columns", "rows", "bsp", "master"],
              "ViewCycleLayout skips grid")

    with_server("[layouts]\ngrid = false\n", body)


def main():
    os.environ["HOME"] = RUNDIR
    srv = Server(RUNDIR).start()
    cli = Client(srv.sock)
    try:
        cli.hello()
        cli.send({"CreateSession": {"name": "main", "folder": None}})
        cli.send({"Attach": {"session_name": "main"}})
        cli.send({"Resize": {"cols": COLS, "rows": ROWS}})
        cli.drain(0.8)
        cmd(cli, "PaneNew")
        cmd(cli, "PaneNew")

        print("cycle order")
        seen = [snapshot(cli).layout_name()]
        grids = {}
        for _ in range(6):
            cmd(cli, "LayoutNext")
            g = snapshot(cli)
            seen.append(g.layout_name())
            grids[g.layout_name()] = g
            if g.layout_name() == "columns":
                print("columns")
                boxes = grids["columns"].boxes()
                print(f"  boxes={boxes}")
                check(len(boxes) == 3, f"columns: 3 boxes ({len(boxes)})")
                if len(boxes) == 3:
                    check([b[1] for b in boxes] == [0, 0, 0]
                          and all(b[3] == ROWS - 1 for b in boxes),
                          "columns: every box spans the full height")
                    check(within_one([b[2] for b in boxes]),
                          f"columns: box widths equal within 1 ({[b[2] for b in boxes]})")
                sz = sizes_walking(cli, "C", "PaneFocusLeft")
                print(f"  stty sizes (rows, cols) left to right: {sz}")
                check(all(sz), "columns: every pane reported its size")
                if all(sz):
                    check({r for r, _ in sz} == {ROWS - 3},
                          "columns: every pane is full height")
                    check(within_one([c for _, c in sz]),
                          "columns: widths equal within 1")
                    check(sum(c + 2 for _, c in sz) == COLS,
                          "columns: widths plus borders fill the width")
                cmd(cli, "PaneFocusRight")
                cmd(cli, "PaneFocusRight")
            if g.layout_name() == "rows":
                print("rows")
                boxes = grids["rows"].boxes()
                print(f"  boxes={boxes}")
                check(len(boxes) == 3, f"rows: 3 boxes ({len(boxes)})")
                if len(boxes) == 3:
                    check([b[0] for b in boxes] == [0, 0, 0]
                          and all(b[2] == COLS for b in boxes),
                          "rows: every box spans the full width")
                    check(within_one([b[3] for b in boxes]),
                          f"rows: box heights equal within 1 ({[b[3] for b in boxes]})")
                sz = sizes_walking(cli, "R", "PaneFocusUp")
                print(f"  stty sizes (rows, cols) top to bottom: {sz}")
                check(all(sz), "rows: every pane reported its size")
                if all(sz):
                    check({c for _, c in sz} == {COLS - 2},
                          "rows: every pane is full width")
                    check(within_one([r for r, _ in sz]),
                          "rows: heights equal within 1")
                    check(sum(r + 2 for r, _ in sz) == ROWS - 1,
                          "rows: heights plus borders fill the pane area")
                cmd(cli, "PaneFocusDown")
                cmd(cli, "PaneFocusDown")
        print(f"  seen: {seen}")
        check(seen == ["bsp", "master", "monocle", "grid", "columns", "rows", "bsp"],
              "LayoutNext cycles bsp -> master -> monocle -> grid -> columns -> rows -> bsp")

        print("return to a remembered custom layout")
        cmd(cli, "PaneSplitVertical")
        check(snapshot(cli).layout_name() == "custom", "a manual split ejects to custom")
        after = []
        for _ in range(7):
            cmd(cli, "LayoutNext")
            after.append(snapshot(cli).layout_name())
        print(f"  seen: {after}")
        check(after == ["bsp", "master", "monocle", "grid", "columns", "rows", "custom"],
              "with a saved custom layout, rows (not grid) returns to custom")

        log = srv.log()
        check("panicked" not in log, "no panic in server log")
    finally:
        cli.close()
        srv.kill()

    disabled_layouts_are_skipped()
    disabled_default_falls_back()
    default_layout_applies_to_new_tabs()
    default_layout_applies_to_moved_and_cli_tabs()
    views_skip_disabled_layouts()

    print()
    if fails:
        print(f"FAILED ({len(fails)}):")
        for f in fails:
            print("  -", f)
        sys.exit(1)
    print("ALL PASS")


if __name__ == "__main__":
    main()
