"""Normal-tab border parity across automatic layouts (frame harness).

This is the "other reading" of the reported bug: *"the Grid layout borders do not
match the other layouts in zellij style -- not rounded, different colors."*

Grid builds an ordinary `LayoutNode` tree (`src/server/layout.rs`), so it should
render through the very same `draw_zellij_panes` as Bsp / Master. This proves it
empirically on a NORMAL tab (no view involved):

  1. With 4 panes, cycle Bsp -> Master -> Grid and, in each layout, collect every
     border cell's glyph + fg color.
  2. Assert the glyph SET is identical across layouts, that it is the rounded set
     (`╭ ╮ ╰ ╯ ─ │`) and contains no square corner (`┌ ┐ └ ┘`).
  3. Assert the fg color SET is identical across layouts, and is exactly
     {frame_active_fg, frame_fg} (one focused pane, the rest inactive).
  4. Assert every layout draws the same NUMBER of boxes (4).
  5. Do the same in tmux style: no boxes at all, dividers only, one divider color.

Run: python3 tests/frame/grid_border_parity.py
"""
import sys
from harness import Server, Client, name_of, only

RUNDIR = "/tmp/rmxgbp"
COLS, ROWS = 100, 30

ROUNDED = {"╭", "╮", "╰", "╯"}   # ╭ ╮ ╰ ╯
SQUARE = {"┌", "┐", "└", "┘"}    # ┌ ┐ └ ┘
EDGES = {"─", "│"}                        # ─ │

fails = []


def check(cond, msg):
    if cond:
        print(f"  PASS {msg}")
    else:
        print(f"  FAIL {msg}")
        fails.append(msg)


class Grid:
    """Reconstruct the composited grid (glyph + fg) from FullRender/RenderDiff."""

    def __init__(self, cols, rows):
        self.cols, self.rows = cols, rows
        self.g = [[" "] * cols for _ in range(rows)]
        self.fg = [[None] * cols for _ in range(rows)]

    def _put(self, y, x, cell):
        if 0 <= y < self.rows and 0 <= x < self.cols:
            self.g[y][x] = cell.get("c", " ") if isinstance(cell, dict) else " "
            self.fg[y][x] = repr(cell.get("fg")) if isinstance(cell, dict) else None

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

    def frame_cells(self):
        """(glyph, fg) for every box-drawing cell above the status row."""
        out = []
        for y in range(self.rows - 1):
            for x in range(self.cols):
                if self.g[y][x] in ROUNDED | SQUARE | EDGES:
                    out.append((self.g[y][x], self.fg[y][x]))
        return out

    def corners(self):
        """Top-left corners of complete rounded boxes."""
        found = []
        for y in range(self.rows - 1):
            for x in range(self.cols):
                if self.g[y][x] != "╭":
                    continue
                # walk right to the matching ╮, then down to ╰ / ╯
                x2 = next((c for c in range(x + 1, self.cols)
                           if self.g[y][c] == "╮"), None)
                if x2 is None:
                    continue
                y2 = next((r for r in range(y + 1, self.rows - 1)
                           if self.g[r][x] == "╰" and self.g[r][x2] == "╯"), None)
                if y2 is not None:
                    found.append((x, y, x2 - x + 1, y2 - y + 1))
        return found


def collect(cli, grid):
    for m in cli.drain(0.7):
        grid.apply(m)
    return grid


def snapshot(cli):
    """A fresh full grid: re-`Resize` to the same size to force a `FullRender`."""
    g = Grid(COLS, ROWS)
    cli.send({"Resize": {"cols": COLS, "rows": ROWS}})
    collect(cli, g)
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
        # 4 panes.
        for _ in range(3):
            cli.send({"Command": "PaneSplitVertical"})
            cli.drain(0.5)

        # `PaneSplit*` ejects to Custom; cycle back to an automatic layout and
        # walk every automatic mode, keyed by the status-bar layout name.
        seen = {}
        for _ in range(8):
            cli.send({"Command": "LayoutNext"})
            g = snapshot(cli)
            bar = g.status_row()
            name = None
            for cand in ("monocle", "master", "grid", "bsp"):
                if cand in bar:
                    name = cand
                    break
            if name and name not in seen:
                seen[name] = g
            if {"bsp", "master", "grid"} <= set(seen):
                break

        print("zellij style, layouts seen:", sorted(seen))
        check({"bsp", "master", "grid"} <= set(seen),
              "bsp / master / grid all reachable")

        ref_glyphs, ref_colors, ref_boxes = None, None, None
        for name in ("bsp", "master", "grid"):
            g = seen.get(name)
            if g is None:
                continue
            cells = g.frame_cells()
            glyphs = set(c[0] for c in cells)
            colors = set(c[1] for c in cells)
            boxes = g.corners()
            print(f"  {name}: glyphs={sorted(glyphs)} colors={sorted(colors)} "
                  f"boxes={len(boxes)}")
            check(not (glyphs & SQUARE), f"{name}: no square corners")
            check(ROUNDED <= glyphs, f"{name}: all four rounded corners present")
            check(len(boxes) == 4, f"{name}: 4 complete boxes ({len(boxes)})")
            check(len(colors) == 2, f"{name}: exactly 2 frame colors (active + inactive)")
            if ref_glyphs is None:
                ref_glyphs, ref_colors, ref_boxes = glyphs, colors, len(boxes)
            else:
                check(glyphs == ref_glyphs, f"{name}: glyph set identical to bsp")
                check(colors == ref_colors, f"{name}: color set identical to bsp")
                check(len(boxes) == ref_boxes, f"{name}: box count identical to bsp")

        # -- tmux style: no boxes anywhere, dividers only -------------------
        cli.send({"Command": "ToggleStyle"})
        tg = snapshot(cli)
        tcells = tg.frame_cells()
        tglyphs = set(c[0] for c in tcells)
        tcolors = set(c[1] for c in tcells)
        print("  tmux: glyphs=", sorted(tglyphs), "colors=", sorted(tcolors))
        check(not (tglyphs & (ROUNDED | SQUARE)), "tmux: no box corners at all")
        check(tglyphs <= EDGES and tglyphs, "tmux: dividers only")
        check(len(tcolors) == 1, "tmux: a single divider color")

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
