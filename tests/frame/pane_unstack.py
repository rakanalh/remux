"""`PaneUnstack*` takes the focused pane out of its stack into its own slot.

The stacked starting state comes from Monocle, which is AUTOMATIC and puts
every pane in one stack. That is what lets the Custom check discriminate: a
stack built with `PaneStackAdd` is already Custom before the unstack runs.

Two panes, 1 (`ID:one`) and 2 (`ID:two`, focused), side by side under BSP.
Each reports `stty size` there first, as the reference geometry. Then:

  * Monocle, `PaneUnstackRight`: two boxes, Custom, pane 2 to the RIGHT of
    pane 1, and BOTH PTYs back at their BSP sizes -- read by `stty size` in
    each pane, since a BSP split and a manual split share the 0.5 ratio;
  * `PaneUnstackLeft` again: pane 2 is alone in its stack, nothing changes;
  * Monocle, `PaneUnstackDown`: pane 2 BELOW pane 1, full width, half height;
  * BSP with no stack at all: `PaneUnstackRight` is a no-op and must NOT eject
    the tab to Custom;
  * Monocle zoomed: the unstack releases the zoom and still happens;
  * Monocle with the popup open: refused, so closing the popup shows Monocle;
  * BSP, `PaneStackIntoLeft` then `PaneUnstackRight`: the round trip puts pane
    2's PTY back at its original size.

Every marker is ASSEMBLED by the shell (`printf 'R:%s' "$(stty size)"` prints
`R:27 48`), so the typed command line can never satisfy the search.

Run: python3 tests/frame/pane_unstack.py
"""
import os
import re
import sys
from harness import Server, Client, name_of, only

RUNDIR = "/tmp/rmxust"
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

    def where(self, pattern):
        """(y, x) of every match, for telling which side of the other a pane is."""
        rx = re.compile(pattern)
        return [(y, m.start()) for y, row in enumerate(self.text())
                for m in rx.finditer(row)]


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
    """Have the focused pane report its size under `tag`, and return it."""
    type_line(cli, f"printf '{tag}:%s\\n' \"$(stty size)\"")
    got = snapshot(cli).find_all(rf"{tag}:(\d+) (\d+)")
    return got[0] if len(got) == 1 else got


def open_two(cli, name):
    """A fresh BSP session: pane 1 left, pane 2 right and focused, each named."""
    cli.send({"CreateSession": {"name": name, "folder": None}})
    cli.send({"Attach": {"session_name": name}})
    cli.send({"Resize": {"cols": COLS, "rows": ROWS}})
    cli.drain(0.8)
    type_line(cli, "printf 'ID:%s\\n' one")
    cmd(cli, "PaneNew")
    type_line(cli, "printf 'ID:%s\\n' two")


def to_monocle(cli):
    """BSP -> Master -> Monocle: both panes in one automatic stack, 2 showing."""
    cmd(cli, "LayoutNext")
    cmd(cli, "LayoutNext")
    g = snapshot(cli)
    return g


def main():
    os.environ["HOME"] = RUNDIR
    srv = Server(RUNDIR).start()
    cli = Client(srv.sock)
    try:
        cli.hello()

        print("right: unstack out of Monocle")
        open_two(cli, "right")
        g = snapshot(cli)
        check(len(g.boxes()) == 2 and g.layout_name() == "bsp",
              f"setup is two BSP boxes ({len(g.boxes())}, {g.layout_name()})")
        bsp_right = size_of(cli, "BR")
        cmd(cli, "PaneFocusLeft")
        bsp_left = size_of(cli, "BL")
        cmd(cli, "PaneFocusRight")
        print(f"  BSP sizes: left={bsp_left} right={bsp_right}")
        check(bsp_left and bsp_right and bsp_left[1] != str(COLS - 2),
              "both BSP slots reported a half-width size")

        g = to_monocle(cli)
        check(len(g.boxes()) == 1 and g.layout_name() == "monocle",
              f"Monocle shows one box ({len(g.boxes())}, {g.layout_name()})")
        mono = size_of(cli, "MO")
        check(mono != bsp_right, f"the Monocle slot differs from the BSP slot ({mono})")

        cmd(cli, "PaneUnstackRight")
        mover = size_of(cli, "UR")
        g = snapshot(cli)
        print(f"  after: layout={g.layout_name()} boxes={g.boxes()} mover stty={mover}")
        check(len(g.boxes()) == 2, f"two visible slots after the unstack ({len(g.boxes())})")
        check(g.layout_name() == "custom", f"the unstack ejects to Custom ({g.layout_name()})")
        check(mover == bsp_right, f"the mover's PTY has the right half ({mover} vs {bsp_right})")
        one, two = g.where(r"ID:one"), g.where(r"UR:\d+")
        check(len(one) == 1 and len(two) == 1 and two[0][1] > one[0][1],
              f"the mover is to the right of the pane left behind ({one} {two})")
        cmd(cli, "PaneFocusLeft")
        stayed = size_of(cli, "UL")
        check(stayed == bsp_left,
              f"the remaining pane's PTY has the left half ({stayed} vs {bsp_left})")
        cmd(cli, "PaneFocusRight")

        boxes_before = snapshot(cli).boxes()
        cmd(cli, "PaneUnstackLeft")
        g = snapshot(cli)
        check(g.boxes() == boxes_before, "a pane alone in its stack does not unstack")

        print("down: unstack below")
        open_two(cli, "down")
        g = to_monocle(cli)
        check(g.layout_name() == "monocle", f"[down] Monocle ({g.layout_name()})")
        mono = size_of(cli, "MD")
        cmd(cli, "PaneUnstackDown")
        mover = size_of(cli, "DN")
        g = snapshot(cli)
        print(f"  [down] after: layout={g.layout_name()} boxes={g.boxes()} "
              f"monocle={mono} mover={mover}")
        check(len(g.boxes()) == 2 and g.layout_name() == "custom",
              f"[down] two boxes, Custom ({len(g.boxes())}, {g.layout_name()})")
        check(bool(mover) and mover[1] == mono[1] and int(mover[0]) < int(mono[0]) // 2 + 1,
              f"[down] full width, half height ({mover} vs {mono})")
        one, two = g.where(r"ID:one"), g.where(r"DN:\d+")
        check(len(one) == 1 and len(two) == 1 and two[0][0] > one[0][0],
              f"[down] the mover is below the pane left behind ({one} {two})")

        print("bsp: nothing to unstack")
        open_two(cli, "flat")
        g = snapshot(cli)
        boxes_before = g.boxes()
        cmd(cli, "PaneUnstackRight")
        g = snapshot(cli)
        check(g.boxes() == boxes_before, "[bsp] a lone pane changes nothing")
        check(g.layout_name() == "bsp", f"[bsp] the no-op keeps BSP ({g.layout_name()})")

        print("zoomed: the unstack releases the zoom")
        open_two(cli, "zoomed")
        to_monocle(cli)
        cmd(cli, "PaneToggleZoom")
        cmd(cli, "PaneUnstackRight")
        g = snapshot(cli)
        print(f"  [zoomed] after: layout={g.layout_name()} boxes={g.boxes()}")
        check(len(g.boxes()) == 2 and g.layout_name() == "custom",
              f"[zoomed] two boxes, Custom ({len(g.boxes())}, {g.layout_name()})")

        print("popup: refused while the popup is open")
        open_two(cli, "popup")
        g = to_monocle(cli)
        boxes_before = g.boxes()
        cmd(cli, "PopupToggle")
        cmd(cli, "PaneUnstackRight")
        cmd(cli, "PopupToggle")
        g = snapshot(cli)
        print(f"  [popup] after close: layout={g.layout_name()} boxes={g.boxes()}")
        check(g.boxes() == boxes_before, f"[popup] the layout is unchanged ({len(g.boxes())})")
        check(g.layout_name() == "monocle", f"[popup] still Monocle ({g.layout_name()})")

        print("round trip: stack into, then unstack")
        open_two(cli, "trip")
        original = size_of(cli, "T0")
        cmd(cli, "PaneStackIntoLeft")
        g = snapshot(cli)
        check(len(g.boxes()) == 1, f"[trip] stacked into one box ({len(g.boxes())})")
        cmd(cli, "PaneUnstackRight")
        back = size_of(cli, "T1")
        g = snapshot(cli)
        check(len(g.boxes()) == 2, f"[trip] two boxes again ({len(g.boxes())})")
        check(back == original, f"[trip] the PTY is back at its size ({back} vs {original})")
    finally:
        cli.close()
        srv.kill()

    log = srv.log()
    check("panicked at" not in log, "no panic in server.log")
    check("PaneUnstack direction=Right" in log, "the server log shows the command arrived")

    if fails:
        print(f"\nFAIL: {len(fails)} check(s) failed")
        sys.exit(1)
    print("\nOK")


if __name__ == "__main__":
    main()
