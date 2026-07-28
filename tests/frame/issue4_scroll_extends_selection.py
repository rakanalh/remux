"""Issue #4 (frame harness): wheel-scroll while a mouse selection is active must
EXTEND the selection to cover the scrolled-into text, so the highlighted region
always equals what would be yanked.

Establish a small selection near the top of the pane, wheel up a couple notches
(revealing earlier lines while the anchor stays visible), then:
  - reconstruct the highlighted region from the frame's bg plane, and
  - release to yank and capture CopyToClipboard.
Assert both include the newly-revealed line numbers AND highlight lines == yank
lines.
"""
import re, sys, time
from harness import Server, Client, name_of, only

RUNDIR = "/tmp/rmxfix/i4"
COLS, ROWS = 100, 30


class Grid:
    def __init__(self, cols, rows):
        self.cols, self.rows = cols, rows
        self.c = [[" "] * cols for _ in range(rows)]
        self.bg = [[None] * cols for _ in range(rows)]
        self.viewport_top = None
        self.fpr = None

    def _put(self, y, x, cell):
        if 0 <= y < self.rows and 0 <= x < self.cols and isinstance(cell, dict):
            self.c[y][x] = cell.get("c", " ")
            self.bg[y][x] = cell.get("bg")

    def apply(self, msg):
        n = name_of(msg)
        body = only(msg, n)
        if n == "FullRender":
            for y, row in enumerate(body["cells"]):
                for x, cell in enumerate(row):
                    self._put(y, x, cell)
            self.viewport_top = body.get("viewport_top")
            self.fpr = body.get("focused_pane_rect") or self.fpr
        elif n == "RenderDiff":
            for ch in body["changes"]:
                self._put(ch["y"], ch["x"], ch["cell"])
            self.viewport_top = body.get("viewport_top")
            self.fpr = body.get("focused_pane_rect") or self.fpr
        elif n == "ScrollRender":
            px, py = body["pane_x"], body["pane_y"]
            pw, ph = body["pane_width"], body["pane_height"]
            delta = body["delta"]
            new_rows = body["new_rows"]

            def shift(plane, blank):
                if delta > 0:  # content up, new rows at bottom
                    for r in range(py, py + ph - delta):
                        if r + delta < self.rows:
                            plane[r][px:px + pw] = plane[r + delta][px:px + pw]
                elif delta < 0:  # content down, new rows at top
                    d = -delta
                    for r in range(py + ph - 1, py + d - 1, -1):
                        if 0 <= r - d:
                            plane[r][px:px + pw] = plane[r - d][px:px + pw]

            shift(self.c, " ")
            shift(self.bg, None)
            for i, row in enumerate(new_rows):
                r = (py + ph - delta + i) if delta > 0 else (py + i)
                if 0 <= r < self.rows:
                    for x, cell in enumerate(row):
                        if px + x < self.cols and isinstance(cell, dict):
                            self.c[r][px + x] = cell.get("c", " ")
                            self.bg[r][px + x] = cell.get("bg")
            self.viewport_top = body.get("viewport_top", self.viewport_top)

    def top_left_highlight(self):
        """Screen (x, y) of the top-left highlighted cell, or None."""
        for y in range(self.rows):
            xs = [x for x in range(self.cols) if self.bg[y][x] == {"Indexed": 7}]
            if xs:
                return (min(xs), y)
        return None

    def highlighted_lines(self):
        """Trimmed text of each highlighted row (skipping blank rows)."""
        out = []
        for y in range(self.rows):
            row = ""
            for x in range(self.cols):
                if self.bg[y][x] == {"Indexed": 7}:
                    row += self.c[y][x]
            if row.strip():
                out.append(row.rstrip())
        return out

    def highlighted_text(self):
        """Concatenate cells whose bg is the selection grey (Indexed 7), row-major."""
        out = []
        for y in range(self.rows):
            row = ""
            for x in range(self.cols):
                if self.bg[y][x] == {"Indexed": 7}:
                    row += self.c[y][x]
            if row.strip():
                out.append(row)
        return "\n".join(out)

    def line_nums(self, text):
        return set(int(m.group(1)) for m in re.finditer(r"LINE_(\d+)", text))

    def visible_text(self):
        return "\n".join("".join(self.c[y]) for y in range(self.rows))


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
    c.send({"Input": {"data": list(b"clear\n")}})
    time.sleep(0.2)
    c.send({"Input": {"data": list(b"for i in $(seq 1 300); do echo LINE_$i; done\n")}})
    time.sleep(1.2)
    for m in c.drain(0.8):
        grid.apply(m)

    fpr = grid.fpr or {"x": 0, "y": 0, "width": COLS, "height": ROWS - 1}
    ctop = fpr["y"] + 1
    cbot = min(fpr["y"] + fpr["height"] - 2, ROWS - 2)
    # Anchor at the RIGHT content edge and drive the moving end to the LEFT edge,
    # so every selected line is whole (no partial-column trimming at either
    # boundary) -- then the highlighted LINE_N set and the yanked LINE_N set are
    # directly comparable.
    leftx = fpr["x"] + 1
    rightx = fpr["x"] + fpr["width"] - 2

    # Anchor a selection near the TOP content row so one wheel notch keeps the
    # anchor visible (highlight then equals the whole selection).
    anchor_y = ctop + 4
    anchor_line = None
    m = re.search(r"LINE_(\d+)", "".join(grid.c[anchor_y]))
    if m:
        anchor_line = int(m.group(1))
    print("anchor screen row line:", anchor_line)

    # Click (clears + focuses) at the right edge, then a non-final drag one row up
    # to establish the selection with the anchor at (rightx, anchor_y).
    c.send({"MouseClick": {"x": rightx, "y": anchor_y}})
    time.sleep(0.1)
    c.send({"MouseDrag": {"start_x": rightx, "start_y": anchor_y,
                          "end_x": rightx, "end_y": anchor_y - 1, "is_final": False}})
    time.sleep(0.15)
    for m in c.drain(0.3):
        grid.apply(m)
    pre = grid.line_nums(grid.highlighted_text())
    print("pre-wheel highlighted lines:", sorted(pre))

    print("pre-wheel viewport_top:", grid.viewport_top)
    # Wheel up one notch (delta 3) -- reveal earlier lines. End pins to the top
    # edge; the anchor (near top) stays visible. A single scroll keeps the frame
    # reconstruction exact (no ScrollRender accumulation).
    c.send({"MouseScroll": {"x": leftx, "y": ctop, "up": True}})
    time.sleep(0.2)
    for m in c.drain(0.4):
        grid.apply(m)
    print("post-wheel viewport_top:", grid.viewport_top)

    hl_lines = grid.highlighted_lines()
    hl = grid.line_nums("\n".join(hl_lines))
    print("post-wheel highlighted lines:", sorted(hl))

    revealed = hl - pre
    print("newly-selected (revealed) lines:", sorted(revealed))

    # The moving end pins to the top-left highlighted cell = pane content (0,0).
    # Release EXACTLY there so the yank's end maps to the same cell the highlight
    # shows (independent of border-offset), and `!is_final` no longer autoscrolls
    # -- so the yanked range equals the highlighted range, line for line.
    tl = grid.top_left_highlight()
    print("top-left highlight screen cell:", tl)
    end_x, end_y = tl if tl else (leftx, ctop)
    c.send({"MouseDrag": {"start_x": rightx, "start_y": anchor_y,
                          "end_x": end_x, "end_y": end_y, "is_final": True}})
    yank = None
    for m in c.drain(0.6):
        grid.apply(m)
        if name_of(m) == "CopyToClipboard":
            yank = only(m, "CopyToClipboard")["data"]
    yank_lines = [ln.rstrip() for ln in (yank or "").split("\n")]
    yank_set = grid.line_nums(yank or "")
    print("yanked lines:", sorted(yank_set))

    panic = "panic" in srv.log().lower()
    srv.kill()

    extended = bool(revealed)
    yank_has_revealed = revealed.issubset(yank_set)
    # Exact text equality: the highlighted region must equal the yanked text.
    hl_eq_yank = (hl_lines == yank_lines)
    print(f"extended={extended} yank_has_revealed={yank_has_revealed} "
          f"highlight_text==yank_text={hl_eq_yank} panic={panic}")
    if extended and yank_has_revealed and hl_eq_yank and not panic:
        print("PASS: wheel extended the selection; highlight text == yank text, includes revealed lines")
        sys.exit(0)
    print("FAIL")
    print("--- highlight lines ---")
    print("\n".join(hl_lines))
    print("--- yank lines ---")
    print("\n".join(yank_lines))
    sys.exit(1)


if __name__ == "__main__":
    main()
