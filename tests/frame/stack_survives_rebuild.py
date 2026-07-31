"""Stacking a pane must eject to Custom, so later rebuilds cannot eat the stack.

`PaneStackAdd` is a *manual* tree mutation (`add_to_stack` splices a pane into
the focused pane's stack node) exactly like `PaneSplit*` is. The splits eject the
tab to `Custom` so nothing rebuilds the tree behind the user's back; stacking did
not, so while the tab was still in an automatic mode any later rebuild
(`PaneNew`, `LayoutNext`, `SetMaster`) silently flattened the stack -- and
`saved_custom_layout` never captured it either, so it was unrecoverable.

Three independent sessions, each: stack two panes, then trigger one rebuild path.

  a) `PaneNew`     -- the tree must be kept: Split(Stack{1,2}, Stack{3}) = 2 boxes
                      (a flattening rebuild would give 3 side-by-side boxes).
  b) `LayoutNext`  -- flattening here is what the user ASKED for, so survival
                      means recoverable: cycle the automatic modes back around to
                      Custom and the stack must come back (1 box).
  c) `SetMaster`   -- same as (b): it switches to the Master automatic mode, so
                      the custom arrangement must be snapshotted first and be
                      restorable by cycling back to Custom.

A 2-pane stack paints ONE zellij box (only the stack's active pane gets a rect),
so the box count is a direct read of "is the stack still a stack".

Run: python3 tests/frame/stack_survives_rebuild.py
"""
import sys
from harness import Server, Client, name_of, only

RUNDIR = "/tmp/rmxstk"
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

    def status_row(self):
        return "".join(self.g[self.rows - 1])

    def layout_name(self):
        bar = self.status_row()
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


def snapshot(cli):
    """A fresh full grid: re-`Resize` to the same size to force a `FullRender`."""
    g = Grid(COLS, ROWS)
    cli.send({"Resize": {"cols": COLS, "rows": ROWS}})
    for m in cli.drain(0.7):
        g.apply(m)
    return g


def open_stacked(cli, name):
    """Fresh session with two panes stacked into one stack node."""
    cli.send({"CreateSession": {"name": name, "folder": None}})
    cli.send({"Attach": {"session_name": name}})
    cli.send({"Resize": {"cols": COLS, "rows": ROWS}})
    cli.drain(0.8)
    g = snapshot(cli)
    check(g.layout_name() == "bsp", f"[{name}] starts in the default automatic layout (bsp)")
    cli.send({"Command": "PaneStackAdd"})
    cli.drain(0.5)
    g = snapshot(cli)
    print(f"  [{name}] after PaneStackAdd: layout={g.layout_name()} boxes={len(g.boxes())}")
    check(g.layout_name() == "custom",
          f"[{name}] PaneStackAdd ejects the tab to Custom")
    check(len(g.boxes()) == 1,
          f"[{name}] the two panes paint as ONE stack box ({len(g.boxes())})")
    return g


def cycle_back_to_custom(cli, name):
    """`LayoutNext` around the automatic cycle until Custom returns."""
    for _ in range(8):
        cli.send({"Command": "LayoutNext"})
        g = snapshot(cli)
        if g.layout_name() == "custom":
            return g
    return None


def main():
    srv = Server(RUNDIR).start()
    cli = Client(srv.sock)
    try:
        cli.hello()

        # -- (a) PaneNew must not rebuild the tree ---------------------------
        print("(a) PaneNew after stacking")
        open_stacked(cli, "a")
        cli.send({"Command": "PaneNew"})
        cli.drain(0.6)
        g = snapshot(cli)
        n = len(g.boxes())
        print(f"  [a] after PaneNew: layout={g.layout_name()} boxes={n}")
        check(n == 2, f"[a] stack survives PaneNew: 2 boxes (stack + new pane), got {n}")

        # -- (b) LayoutNext: flattened by request, but recoverable ----------
        print("(b) LayoutNext after stacking")
        open_stacked(cli, "b")
        cli.send({"Command": "LayoutNext"})
        cli.drain(0.6)
        g = snapshot(cli)
        print(f"  [b] after LayoutNext: layout={g.layout_name()} boxes={len(g.boxes())}")
        check(g.layout_name() != "custom", "[b] LayoutNext left Custom for an automatic mode")
        g = cycle_back_to_custom(cli, "b")
        check(g is not None, "[b] the cycle returns to Custom (the stack was snapshotted)")
        if g is not None:
            n = len(g.boxes())
            print(f"  [b] back in Custom: boxes={n}")
            check(n == 1, f"[b] the stack is restored: 1 box, got {n}")

        # -- (c) SetMaster: same, via the Master promotion path --------------
        print("(c) SetMaster after stacking")
        open_stacked(cli, "c")
        cli.send({"Command": "SetMaster"})
        cli.drain(0.6)
        g = snapshot(cli)
        print(f"  [c] after SetMaster: layout={g.layout_name()} boxes={len(g.boxes())}")
        check(g.layout_name() == "master", "[c] SetMaster switched to the Master layout")
        g = cycle_back_to_custom(cli, "c")
        check(g is not None, "[c] the cycle returns to Custom (the stack was snapshotted)")
        if g is not None:
            n = len(g.boxes())
            print(f"  [c] back in Custom: boxes={n}")
            check(n == 1, f"[c] the stack is restored: 1 box, got {n}")

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
