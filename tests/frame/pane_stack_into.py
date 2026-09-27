"""`PaneStackInto*` folds an EXISTING pane into its neighbour's stack.

Three panes: 1 on the left, 2 top-right, 3 bottom-right (focused). Sending
`PaneStackIntoLeft` from pane 3 must:

  * put pane 3 into pane 1's stack as the visible pane, so the tab paints TWO
    boxes instead of three and pane 1's content is hidden behind it;
  * resize pane 3's PTY to pane 1's slot -- read from `stty size` INSIDE pane 3,
    compared with what pane 1 reported for itself before the move;
  * eject the tab to Custom, so a later `PaneNew` cannot flatten the stack;
  * leave a real stack: `PaneStackNext` brings pane 1 back and hides pane 3.

Then `PaneStackIntoLeft` again from the left edge has no neighbour, so it must
change nothing.

Two more sessions with the same shape:

  * zoomed on pane 3, the move releases the zoom and still happens;
  * with the popup open, the move is refused like every other layout command,
    so closing the popup shows the untouched three-box BSP layout.

Every marker is ASSEMBLED by the shell (`printf 'L:%s' "$(stty size)"` prints
`L:27 48`), so the typed command line can never satisfy the search.

Run: python3 tests/frame/pane_stack_into.py
"""
import os
import re
import sys
from harness import Server, Client, name_of, only

RUNDIR = "/tmp/rmxsti"
COLS, ROWS = 100, 30

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

    def layout_name(self):
        bar = "".join(self.g[self.rows - 1])
        for cand in ("monocle", "master", "custom", "grid", "bsp"):
            if cand in bar:
                return cand
        return "?"

    def boxes(self):
        """Top-left corners of complete rounded boxes."""
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
        """Every match in every row. `finditer`, because two panes share a row."""
        rx = re.compile(pattern)
        return [m.groups() or m.group(0) for row in self.text() for m in rx.finditer(row)]


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


def open_three(cli, name):
    """A fresh session with the 1 | 2/3 BSP shape, pane 3 focused."""
    cli.send({"CreateSession": {"name": name, "folder": None}})
    cli.send({"Attach": {"session_name": name}})
    cli.send({"Resize": {"cols": COLS, "rows": ROWS}})
    cli.drain(0.8)
    cmd(cli, "PaneNew")
    cmd(cli, "PaneNew")


def main():
    os.environ["HOME"] = RUNDIR
    srv = Server(RUNDIR).start()
    cli = Client(srv.sock)
    try:
        cli.hello()
        # `PaneNew` keeps the tab in BSP, which builds exactly this shape, so
        # the Custom check below measures the move and not the setup.
        open_three(cli, "main")
        g = snapshot(cli)
        print(f"setup: layout={g.layout_name()} boxes={g.boxes()}")
        check(len(g.boxes()) == 3, f"setup paints three boxes ({len(g.boxes())})")
        check(g.layout_name() == "bsp", f"setup is still automatic ({g.layout_name()})")

        # Pane 3 (bottom-right, focused) names itself and reports its size.
        type_line(cli, "printf 'ID:%s\\n' mover; printf 'B:%s\\n' \"$(stty size)\"")
        cmd(cli, "PaneFocusLeft")
        type_line(cli, "printf 'ID:%s\\n' left; printf 'L:%s\\n' \"$(stty size)\"")
        cmd(cli, "PaneFocusRight")
        cmd(cli, "PaneFocusDown")

        g = snapshot(cli)
        left = g.find_all(r"L:(\d+) (\d+)")
        before = g.find_all(r"B:(\d+) (\d+)")
        print(f"before: left pane stty={left} mover stty={before}")
        check(len(left) == 1 and len(before) == 1, "both panes reported a size")
        check(left != before, "the left slot and the mover's slot differ in size")
        check(g.find_all(r"ID:left") and g.find_all(r"ID:mover"),
              "both identity markers are visible before the move")

        cmd(cli, "PaneStackIntoLeft")
        type_line(cli, "printf 'A:%s\\n' \"$(stty size)\"")
        g = snapshot(cli)
        after = g.find_all(r"A:(\d+) (\d+)")
        print(f"after: layout={g.layout_name()} boxes={g.boxes()} mover stty={after}")
        check(len(g.boxes()) == 2, f"two visible slots after the move ({len(g.boxes())})")
        check(g.layout_name() == "custom", f"the move ejects to Custom ({g.layout_name()})")
        check(after == left, f"the mover's PTY now has the left slot's size ({after} vs {left})")
        check(g.find_all(r"ID:mover") and not g.find_all(r"ID:left"),
              "the mover is the visible pane of the left stack")

        cmd(cli, "PaneStackNext")
        g = snapshot(cli)
        check(g.find_all(r"ID:left") and not g.find_all(r"ID:mover"),
              "PaneStackNext shows the original left pane")
        cmd(cli, "PaneStackNext")
        g = snapshot(cli)
        check(g.find_all(r"ID:mover") and not g.find_all(r"ID:left"),
              "PaneStackNext again cycles back to the mover")

        boxes_before = g.boxes()
        cmd(cli, "PaneStackIntoLeft")
        g = snapshot(cli)
        check(g.boxes() == boxes_before,
              "PaneStackIntoLeft at the left edge changes nothing")
        check(g.find_all(r"ID:mover") and not g.find_all(r"ID:left"),
              "the no-op keeps the same pane visible")

        # An automatic layout would rebuild from `pane_order` here and flatten
        # the stack into four boxes. Custom keeps it: stack + pane 2 + new.
        cmd(cli, "PaneNew")
        g = snapshot(cli)
        check(len(g.boxes()) == 3,
              f"the stack survives a later PaneNew ({len(g.boxes())} boxes)")

        print("zoomed: the move releases the zoom and still happens")
        open_three(cli, "zoomed")
        cmd(cli, "PaneToggleZoom")
        g = snapshot(cli)
        check(len(g.boxes()) == 1, f"[zoomed] the zoom paints one box ({len(g.boxes())})")
        cmd(cli, "PaneStackIntoLeft")
        type_line(cli, "printf 'ZM:%s\\n' \"$(stty size)\"")
        g = snapshot(cli)
        moved = g.find_all(r"ZM:(\d+) (\d+)")
        print(f"  [zoomed] after: layout={g.layout_name()} boxes={g.boxes()} mover stty={moved}")
        check(len(g.boxes()) == 2,
              f"[zoomed] the zoom is released: two slots ({len(g.boxes())})")
        check(g.layout_name() == "custom", f"[zoomed] the move happened ({g.layout_name()})")
        check(moved == left, f"[zoomed] the mover has the left slot's size ({moved} vs {left})")

        print("popup: the move is refused while the popup is open")
        open_three(cli, "popup")
        g = snapshot(cli)
        boxes_before = g.boxes()
        cmd(cli, "PopupToggle")
        cmd(cli, "PaneStackIntoLeft")
        cmd(cli, "PopupToggle")
        g = snapshot(cli)
        print(f"  [popup] after close: layout={g.layout_name()} boxes={g.boxes()}")
        check(g.boxes() == boxes_before,
              f"[popup] the layout is unchanged ({len(g.boxes())} boxes)")
        check(g.layout_name() == "bsp", f"[popup] the tab stays automatic ({g.layout_name()})")
    finally:
        cli.close()
        srv.kill()

    log = srv.log()
    check("panicked at" not in log, "no panic in server.log")
    check("PaneStackInto direction=Left" in log, "the server log shows the command arrived")

    if fails:
        print(f"\nFAIL: {len(fails)} check(s) failed")
        sys.exit(1)
    print("\nOK")


if __name__ == "__main__":
    main()
