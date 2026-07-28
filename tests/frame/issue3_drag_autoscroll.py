"""Issue #3 reproduction (frame harness, NORMAL session): drag-select edge
autoscroll. Fill a pane with >1 screen of numbered lines, click near the bottom,
then repeatedly drag with the end row pinned to the pane's TOP content row.

Assert: the viewport scrolls UP into scrollback (viewport_top decreases) and the
text on the anchored screen row stays stable as we scroll.
"""
import re, sys, time
from harness import Server, Client, name_of, only

RUNDIR = "/tmp/rmxfix/i3"
COLS, ROWS = 100, 30


class Grid:
    """Reconstruct the composited grid from Full/Diff/Scroll renders."""
    def __init__(self, cols, rows):
        self.cols, self.rows = cols, rows
        self.g = [[" "] * cols for _ in range(rows)]
        self.viewport_top = None
        self.fpr = None

    def _ch(self, cell):
        return cell.get("c", " ") if isinstance(cell, dict) else " "

    def apply(self, msg):
        n = name_of(msg)
        body = only(msg, n)
        if n == "FullRender":
            for y, row in enumerate(body["cells"]):
                for x, cell in enumerate(row):
                    if y < self.rows and x < self.cols:
                        self.g[y][x] = self._ch(cell)
            self.viewport_top = body.get("viewport_top")
            self.fpr = body.get("focused_pane_rect")
        elif n == "RenderDiff":
            for ch in body["changes"]:
                y, x = ch["y"], ch["x"]
                if y < self.rows and x < self.cols:
                    self.g[y][x] = self._ch(ch["cell"])
            self.viewport_top = body.get("viewport_top")
            self.fpr = body.get("focused_pane_rect") or self.fpr
        elif n == "ScrollRender":
            px, py = body["pane_x"], body["pane_y"]
            pw, ph = body["pane_width"], body["pane_height"]
            delta = body["delta"]
            new_rows = body["new_rows"]
            if delta > 0:  # content up, new rows at bottom
                for r in range(py, py + ph - delta):
                    if r + delta < self.rows:
                        self.g[r][px:px + pw] = self.g[r + delta][px:px + pw]
                for i, row in enumerate(new_rows):
                    r = py + ph - delta + i
                    if 0 <= r < self.rows:
                        self.g[r][px:px + pw] = [self._ch(c) for c in row][:pw]
            elif delta < 0:  # content down, new rows at top
                d = -delta
                for r in range(py + ph - 1, py + d - 1, -1):
                    if 0 <= r - d:
                        self.g[r][px:px + pw] = self.g[r - d][px:px + pw]
                for i, row in enumerate(new_rows):
                    r = py + i
                    if 0 <= r < self.rows:
                        self.g[r][px:px + pw] = [self._ch(c) for c in row][:pw]
            self.viewport_top = body.get("viewport_top", self.viewport_top)

    def row_text(self, y):
        return "".join(self.g[y]).rstrip()


def main():
    srv = Server(RUNDIR).start()
    c = Client(srv.sock)
    c.hello()
    c.send({"CreateSession": {"name": "main", "folder": None}})
    c.send({"Attach": {"session_name": "main"}})
    c.send({"Resize": {"cols": COLS, "rows": ROWS}})
    time.sleep(0.3)
    grid = Grid(COLS, ROWS)
    for m in c.drain(0.5):
        grid.apply(m)

    # >1 screen of numbered history.
    c.send({"Input": {"data": list(b"for i in $(seq 1 200); do echo LINE_$i; done\n")}})
    time.sleep(1.0)
    for m in c.drain(0.8):
        grid.apply(m)

    fpr = grid.fpr or {"x": 0, "y": 0, "width": COLS, "height": ROWS - 1}
    # Content rows: allow for a possible 1-cell border.
    top_y = fpr["y"]
    bot_y = fpr["y"] + fpr["height"] - 1
    # A drag/click just inside the content, away from the status bar (last row).
    bot_y = min(bot_y, ROWS - 2)
    midx = fpr["x"] + 2
    v_live = grid.viewport_top
    print("live viewport_top:", v_live, "fpr:", fpr)

    # Click near the bottom to anchor the selection.
    c.send({"MouseClick": {"x": midx, "y": bot_y}})
    time.sleep(0.15)
    for m in c.drain(0.3):
        grid.apply(m)

    # Anchor row text (the row we clicked on) -- should stay stable while we
    # scroll UP (the anchor is pinned in absolute coords).
    anchor_row_text = grid.row_text(bot_y)
    print("anchor row text at click:", repr(anchor_row_text))

    # Repeatedly drag with the end row pinned to the TOP content row.
    min_vt = v_live
    for _ in range(25):
        c.send({"MouseDrag": {"start_x": midx, "start_y": bot_y,
                              "end_x": midx, "end_y": top_y, "is_final": False}})
        time.sleep(0.06)
        for m in c.drain(0.12):
            grid.apply(m)
        if grid.viewport_top is not None:
            min_vt = min(min_vt, grid.viewport_top)

    print("min viewport_top after top-edge drags:", min_vt)
    scrolled_up = (v_live is not None and min_vt < v_live)

    # Now drag with the end row pinned to the BOTTOM to scroll back down.
    max_vt = min_vt
    for _ in range(30):
        c.send({"MouseDrag": {"start_x": midx, "start_y": bot_y,
                              "end_x": midx, "end_y": bot_y, "is_final": False}})
        time.sleep(0.06)
        for m in c.drain(0.12):
            grid.apply(m)
        if grid.viewport_top is not None:
            max_vt = max(max_vt, grid.viewport_top)
    print("max viewport_top after bottom-edge drags:", max_vt)
    scrolled_down = max_vt >= (v_live if v_live is not None else 0)

    # Release.
    c.send({"MouseDrag": {"start_x": midx, "start_y": bot_y,
                          "end_x": midx, "end_y": bot_y, "is_final": True}})
    time.sleep(0.15)

    panic = "panic" in srv.log().lower()
    srv.kill()
    print(f"scrolled_up={scrolled_up} scrolled_down={scrolled_down} panic={panic}")
    if scrolled_up and scrolled_down and not panic:
        print("PASS: drag-select edge autoscroll works in a NORMAL session")
        sys.exit(0)
    print("RESULT: drag autoscroll did NOT scroll as expected (possible regression)")
    sys.exit(2)


if __name__ == "__main__":
    main()
