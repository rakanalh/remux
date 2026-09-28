"""`PaneMoveLeft`/`PaneMoveRight` inside a stack of several panes.

The pane moves one place along its stack; at the stack's end the same key takes
it out of the stack into its own slot on that side, exactly as `PaneUnstack*`
does. A pane alone in its stack keeps the old spatial swap.

A reorder does NOT change the visible pane (the mover stays active), so the pane
body is identical before and after. Only the top-border strip ORDER tells, and
the panes are renamed `aa`/`bb`/`cc` so the strip reads as a sequence.

  * Monocle (automatic): `H` on the middle pane reorders the strip and keeps
    Monocle; `H` again at the start ejects it LEFT into its own slot, Custom;
  * Monocle: `L` reorders, and `LayoutNext` into Grid lays the panes out in the
    NEW order, which proves the move reached `pane_order` and not just the tree;
  * Monocle: `J` on the MIDDLE pane ejects it DOWN at once, and `K` UP, since a
    stack has no vertical order to walk first;
  * Custom stack from `PaneStackAdd`: `L` at the end ejects RIGHT;
  * BSP, single-pane stacks: `H` is still the old swap with the left neighbour,
    and `K`/`J` the old swap with the pane above/below.

Every `ID:` marker is ASSEMBLED by the shell (`printf 'ID:%s' one`), so the
typed command line can never satisfy the search.

Run: python3 tests/frame/pane_stack_reorder.py
"""
import os
import re
import sys
from harness import Server, Client, name_of, only

RUNDIR = "/tmp/rmxsro"
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

    def strips(self):
        """The stack labels on each top-border row, left to right, per row.

        `finditer`, because two panes' borders can share a row."""
        out = []
        for row in self.text():
            labels = [m.group(1) for m in re.finditer(r"\b(aa|bb|cc)\b", row)]
            if labels and "ID:" not in row:
                out.append(labels)
        return out

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


def open_session(cli, name):
    cli.send({"CreateSession": {"name": name, "folder": None}})
    cli.send({"Attach": {"session_name": name}})
    cli.send({"Resize": {"cols": COLS, "rows": ROWS}})
    cli.drain(0.8)


def name_three_backwards(cli):
    """Focused on the LAST of a 3-pane stack: name it cc, then bb, then aa,
    stepping back through the stack, and finish focused on bb (the middle)."""
    cmd(cli, {"PaneRename": "cc"})
    cmd(cli, "PaneStackPrev")
    cmd(cli, {"PaneRename": "bb"})
    cmd(cli, "PaneStackPrev")
    cmd(cli, {"PaneRename": "aa"})
    cmd(cli, "PaneStackNext")


def mark(cli, word):
    """Keep `ID:<word>` on the focused pane's screen for good. A layout change
    resizes the pane and the shell's redraw can wipe a one-off line, so the
    marker is redrawn in a loop instead."""
    type_line(cli, f"while :; do clear; printf 'ID:%s\\n' {word}; sleep 0.3; done")


def open_monocle_three(cli, name):
    """Three BSP panes marked one/two/three, then Monocle with bb focused."""
    open_session(cli, name)
    mark(cli, "one")
    cmd(cli, "PaneNew")
    mark(cli, "two")
    cmd(cli, "PaneNew")
    mark(cli, "three")
    cmd(cli, "LayoutNext")
    cmd(cli, "LayoutNext")
    # Renamed only now: a Monocle rebuild starts every stack name afresh.
    name_three_backwards(cli)
    return snapshot(cli)


def main():
    os.environ["HOME"] = RUNDIR
    srv = Server(RUNDIR).start()
    cli = Client(srv.sock)
    try:
        cli.hello()

        print("monocle: H reorders, then ejects left")
        g = open_monocle_three(cli, "left")
        print(f"  setup: layout={g.layout_name()} strips={g.strips()}")
        check(g.layout_name() == "monocle" and len(g.boxes()) == 1,
              f"setup is one Monocle box ({g.layout_name()}, {len(g.boxes())})")
        check(g.strips() == [["aa", "bb", "cc"]], f"setup strip is aa bb cc ({g.strips()})")
        check(len(g.where(r"ID:two")) == 1, "pane two (bb) is the one showing")

        cmd(cli, "PaneMoveLeft")
        g = snapshot(cli)
        print(f"  after H: layout={g.layout_name()} strips={g.strips()}")
        check(g.strips() == [["bb", "aa", "cc"]], f"H moves bb to the front ({g.strips()})")
        check(g.layout_name() == "monocle", f"the reorder keeps Monocle ({g.layout_name()})")
        check(len(g.boxes()) == 1, f"still one box ({len(g.boxes())})")
        check(len(g.where(r"ID:two")) == 1, "the mover is still the pane showing")

        cmd(cli, "PaneMoveLeft")
        g = snapshot(cli)
        print(f"  after H again: layout={g.layout_name()} boxes={g.boxes()} "
              f"strips={g.strips()}")
        check(len(g.boxes()) == 2, f"H at the start splits the slot ({len(g.boxes())})")
        check(g.layout_name() == "custom", f"the eject goes to Custom ({g.layout_name()})")
        two = g.where(r"ID:two")
        rest = g.where(r"ID:(one|three)")
        check(len(two) == 1 and len(rest) == 1 and two[0][1] < rest[0][1],
              f"the mover is LEFT of the stack left behind ({two} {rest})")

        print("monocle: L reorders, and the order reaches pane_order")
        g = open_monocle_three(cli, "order")
        cmd(cli, "PaneMoveRight")
        g = snapshot(cli)
        print(f"  after L: layout={g.layout_name()} strips={g.strips()}")
        check(g.strips() == [["aa", "cc", "bb"]], f"L moves bb to the end ({g.strips()})")
        check(g.layout_name() == "monocle", f"the reorder keeps Monocle ({g.layout_name()})")
        cmd(cli, "LayoutNext")
        g = snapshot(cli)
        one, two, three = (g.where(rf"ID:{w}") for w in ("one", "two", "three"))
        print(f"  grid: layout={g.layout_name()} one={one} two={two} three={three}")
        check(g.layout_name() == "grid", f"LayoutNext reaches Grid ({g.layout_name()})")
        # Grid is row-major: order [1, 3, 2] puts one and three on the top row
        # and two alone below. The old order [1, 2, 3] puts three below.
        check(len(one) == 1 and len(two) == 1 and len(three) == 1
              and one[0][0] == three[0][0] < two[0][0]
              and one[0][1] < three[0][1],
              "Grid lays the panes out in the reordered sequence")

        for key, direction, below in (("J", "PaneMoveDown", True),
                                      ("K", "PaneMoveUp", False)):
            side = "BELOW" if below else "ABOVE"
            print(f"monocle: {key} ejects the middle pane {side.lower()}")
            open_monocle_three(cli, f"eject{key}")
            cmd(cli, direction)
            g = snapshot(cli)
            boxes = g.boxes()
            print(f"  [{key}] after: layout={g.layout_name()} boxes={boxes} "
                  f"strips={g.strips()}")
            check(len(boxes) == 2, f"[{key}] the slot splits at once ({len(boxes)})")
            check(len(boxes) == 2 and boxes[0][0] == boxes[1][0]
                  and boxes[0][2] == boxes[1][2] == COLS,
                  f"[{key}] the two slots are full width, one over the other ({boxes})")
            check(g.layout_name() == "custom", f"[{key}] the eject goes to Custom "
                  f"({g.layout_name()})")
            two = g.where(r"ID:two")
            rest = g.where(r"ID:(one|three)")
            check(len(two) == 1 and len(rest) == 1
                  and (two[0][0] > rest[0][0]) == below,
                  f"[{key}] the mover is {side} the stack left behind ({two} {rest})")
            check(["aa", "cc"] in g.strips() and ["bb"] in g.strips(),
                  f"[{key}] bb has its own slot and aa cc keep their stack ({g.strips()})")

        print("custom stack: L at the end ejects right")
        open_session(cli, "manual")
        type_line(cli, "printf 'ID:%s\\n' one")
        cmd(cli, "PaneStackAdd")
        type_line(cli, "printf 'ID:%s\\n' two")
        cmd(cli, "PaneStackAdd")
        type_line(cli, "printf 'ID:%s\\n' three")
        name_three_backwards(cli)
        g = snapshot(cli)
        check(g.strips() == [["aa", "bb", "cc"]] and g.layout_name() == "custom",
              f"[custom] setup strip aa bb cc, Custom ({g.strips()}, {g.layout_name()})")
        cmd(cli, "PaneMoveRight")
        g = snapshot(cli)
        check(g.strips() == [["aa", "cc", "bb"]] and len(g.boxes()) == 1,
              f"[custom] L reorders in place ({g.strips()}, {len(g.boxes())})")
        cmd(cli, "PaneMoveRight")
        g = snapshot(cli)
        print(f"  [custom] after L again: boxes={g.boxes()} strips={g.strips()}")
        check(len(g.boxes()) == 2, f"[custom] L at the end splits the slot ({len(g.boxes())})")
        two = g.where(r"ID:two")
        rest = g.where(r"ID:(one|three)")
        check(len(two) == 1 and len(rest) == 1 and two[0][1] > rest[0][1],
              f"[custom] the mover is RIGHT of the stack left behind ({two} {rest})")

        print("bsp: a lone pane still swaps")
        open_session(cli, "flat")
        type_line(cli, "printf 'ID:%s\\n' one")
        cmd(cli, "PaneNew")
        type_line(cli, "printf 'ID:%s\\n' two")
        g = snapshot(cli)
        one, two = g.where(r"ID:one"), g.where(r"ID:two")
        check(len(one) == 1 and len(two) == 1 and one[0][1] < two[0][1],
              f"[bsp] setup: one left of two ({one} {two})")
        cmd(cli, "PaneMoveLeft")
        g = snapshot(cli)
        one, two = g.where(r"ID:one"), g.where(r"ID:two")
        print(f"  [bsp] after H: layout={g.layout_name()} one={one} two={two}")
        check(len(g.boxes()) == 2, f"[bsp] still two boxes ({len(g.boxes())})")
        check(len(one) == 1 and len(two) == 1 and two[0][1] < one[0][1],
              f"[bsp] H swapped the panes ({one} {two})")

        print("bsp: a lone pane still swaps up and down")
        # BSP with three panes: one on the left, two over three on the right,
        # three focused.
        open_session(cli, "vflat")
        mark(cli, "one")
        cmd(cli, "PaneNew")
        mark(cli, "two")
        cmd(cli, "PaneNew")
        mark(cli, "three")
        g = snapshot(cli)
        two, three = g.where(r"ID:two"), g.where(r"ID:three")
        check(len(two) == 1 and len(three) == 1 and two[0][0] < three[0][0]
              and two[0][1] == three[0][1],
              f"[bsp] setup: two above three ({two} {three})")
        for key, direction, three_on_top in (("K", "PaneMoveUp", True),
                                             ("J", "PaneMoveDown", False)):
            cmd(cli, direction)
            g = snapshot(cli)
            two, three = g.where(r"ID:two"), g.where(r"ID:three")
            print(f"  [bsp] after {key}: layout={g.layout_name()} boxes={len(g.boxes())} "
                  f"two={two} three={three}")
            check(len(g.boxes()) == 3, f"[bsp] {key}: still three boxes ({len(g.boxes())})")
            check(len(two) == 1 and len(three) == 1
                  and (three[0][0] < two[0][0]) == three_on_top
                  and two[0][1] == three[0][1],
                  f"[bsp] {key} swapped three with the pane "
                  f"{'above' if three_on_top else 'below'} ({two} {three})")
    finally:
        cli.close()
        srv.kill()

    log = srv.log()
    check("panicked at" not in log, "no panic in server.log")
    check("layout: shift_in_stack" in log, "the server log shows the in-stack move ran")

    if fails:
        print(f"\nFAIL: {len(fails)} check(s) failed")
        sys.exit(1)
    print("\nOK")


if __name__ == "__main__":
    main()
