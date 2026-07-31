#!/usr/bin/env python3
"""Mouse policy in a NORMAL SESSION pane (frame harness).

Covers the session half of "mouse-aware applications (claude code, neovim) don't
work under remux":

  * A drag over an ALT-SCREEN pane used to scroll remux's scrollback -- which on
    the alternate screen holds the PRIMARY screen's history, i.e. text that has
    nothing to do with what the app is showing -- and to ARM the 40ms
    drag-autoscroll ticker. Because an alt-screen app's own output keeps feeding
    that scrollback, `at_top && new_offset < max_scroll` never went false, so the
    ticker replayed a full composite + FullRender ~25x/s forever into the
    client's unbounded channel. That is the "remux stops responding" freeze.
    Assertions 1 and 2 fail (2 by flooding) without the fix.
  * A drag over a MOUSE-TRACKING pane selected remux text instead of reaching the
    application. Assertions 3-5 check the press/motion/release reports actually
    arrive, by running `cat -v` in the pane so it prints what it receives.
  * The plain-shell behaviour must be untouched: assertion 7.

Coordinates are checked by DIFFERENCE (two events a known distance apart) so the
test does not have to predict the pane's border inset.

Run from the repo root:  python3 tests/frame/mouse_tracking_session.py
"""
import os
import re
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import Client, Server, name_of, only  # noqa: E402

RUNDIR = "/tmp/rmx_mts"
COLS, ROWS = 100, 30
# `cat -v` renders ESC as "^[", so a forwarded SGR report reads like
# "^[[<64;10;5M" on screen.
REPORT = re.compile(r"\^\[\[<(\d+);(\d+);(\d+)([Mm])")


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
            self.fpr = body.get("focused_pane_rect") or self.fpr
        elif n == "RenderDiff":
            for ch in body["changes"]:
                y, x = ch["y"], ch["x"]
                if y < self.rows and x < self.cols:
                    self.g[y][x] = self._ch(ch["cell"])
            self.viewport_top = body.get("viewport_top", self.viewport_top)
            self.fpr = body.get("focused_pane_rect") or self.fpr
        elif n == "ScrollRender":
            px, py = body["pane_x"], body["pane_y"]
            pw, ph = body["pane_width"], body["pane_height"]
            delta, new_rows = body["delta"], body["new_rows"]
            if delta > 0:
                for r in range(py, py + ph - delta):
                    if r + delta < self.rows:
                        self.g[r][px:px + pw] = self.g[r + delta][px:px + pw]
                for i, row in enumerate(new_rows):
                    r = py + ph - delta + i
                    if 0 <= r < self.rows:
                        self.g[r][px:px + pw] = [self._ch(c) for c in row][:pw]
            elif delta < 0:
                d = -delta
                for r in range(py + ph - 1, py + d - 1, -1):
                    self.g[r][px:px + pw] = self.g[r - d][px:px + pw]
                for i, row in enumerate(new_rows):
                    r = py + i
                    if 0 <= r < self.rows:
                        self.g[r][px:px + pw] = [self._ch(c) for c in row][:pw]
            self.viewport_top = body.get("viewport_top", self.viewport_top)

    def text(self):
        return "\n".join("".join(r) for r in self.g)


def pump(c, grid, t=0.5):
    """Apply everything arriving within `t` seconds; return the message count."""
    msgs = c.drain(t)
    for m in msgs:
        grid.apply(m)
    return len(msgs)


def reports(grid):
    """Every mouse report `cat -v` has printed, oldest first.

    Newlines are dropped so a report that wrapped at the pane's right edge still
    reads as one sequence.
    """
    flat = grid.text().replace("\n", "")
    return [(int(b), int(x), int(y), f) for b, x, y, f in REPORT.findall(flat)]


def main():
    srv = Server(RUNDIR).start()
    results = []
    try:
        c = Client(srv.sock)
        c.hello()
        c.send({"CreateSession": {"name": "main", "folder": None}})
        c.send({"Attach": {"session_name": "main"}})
        c.send({"Resize": {"cols": COLS, "rows": ROWS}})
        time.sleep(0.4)
        grid = Grid(COLS, ROWS)
        pump(c, grid, 0.5)

        # More than a screen of PRIMARY-screen history for a stray autoscroll to
        # chase.
        c.send({"Input": {"data": list(b"for i in $(seq 1 200); do echo LINE_$i; done\n")}})
        time.sleep(1.0)
        pump(c, grid, 0.8)
        fpr = grid.fpr or {"x": 0, "y": 0, "width": COLS, "height": ROWS - 1}
        top_y, mid_y = fpr["y"], fpr["y"] + 8
        mid_x = fpr["x"] + 4
        live_top = grid.viewport_top
        print("live viewport_top:", live_top, "fpr:", fpr)

        # -- 1/2. ALT SCREEN, NO TRACKING (e.g. `less`): a drag on the top
        # content row must neither scroll into the primary screen's history nor
        # arm the repeat ticker.
        c.send({"Input": {"data": list(b"printf '\\033[?1049h'\n")}})
        time.sleep(0.6)
        pump(c, grid, 0.5)
        vt_before = grid.viewport_top
        c.send({"MouseClick": {"x": mid_x, "y": mid_y}})
        time.sleep(0.15)
        pump(c, grid, 0.3)
        c.send({"MouseDrag": {"start_x": mid_x, "start_y": mid_y,
                              "end_x": mid_x, "end_y": top_y, "is_final": False}})
        time.sleep(0.3)
        pump(c, grid, 0.4)
        vt_after_drag = grid.viewport_top
        # Nothing more is sent: anything arriving now is the ticker replaying.
        idle = pump(c, grid, 2.0)
        vt_idle = grid.viewport_top
        c.send({"MouseDrag": {"start_x": mid_x, "start_y": mid_y,
                              "end_x": mid_x, "end_y": top_y, "is_final": True}})
        time.sleep(0.2)
        pump(c, grid, 0.3)
        no_scroll = vt_after_drag == vt_before and vt_idle == vt_before
        results.append(("1. alt-screen drag does not scroll the primary scrollback",
                        no_scroll, f"viewport_top {vt_before} -> {vt_after_drag} -> {vt_idle}"))
        results.append(("2. alt-screen edge drag does not arm the autoscroll ticker",
                        idle == 0, f"{idle} unsolicited frames in 2s while resting on the top edge"))

        # -- 3/4/5. ALT SCREEN + MOUSE TRACKING: the application gets the events.
        c.send({"Input": {"data": list(b"printf '\\033[?1002h\\033[?1006h'; cat -v\n")}})
        time.sleep(0.8)
        pump(c, grid, 0.6)

        c.send({"MouseScroll": {"x": mid_x, "y": mid_y, "up": True}})
        time.sleep(0.2)
        c.send({"MouseScroll": {"x": mid_x + 5, "y": mid_y + 2, "up": False}})
        time.sleep(0.2)
        pump(c, grid, 0.5)
        rs = reports(grid)
        wheel_ok = (
            len(rs) >= 2
            and rs[0][0] == 64 and rs[1][0] == 65
            and (rs[1][1] - rs[0][1], rs[1][2] - rs[0][2]) == (5, 2)
            and rs[0][1] >= 1 and rs[0][2] >= 1
        )
        results.append(("3. wheel reaches a tracking app at the right position",
                        wheel_ok, f"{rs}"))

        before = len(reports(grid))
        c.send({"MouseClick": {"x": mid_x, "y": mid_y}})
        time.sleep(0.2)
        c.send({"MouseDrag": {"start_x": mid_x, "start_y": mid_y,
                              "end_x": mid_x + 3, "end_y": mid_y + 1, "is_final": False}})
        time.sleep(0.2)
        c.send({"MouseDrag": {"start_x": mid_x, "start_y": mid_y,
                              "end_x": mid_x + 3, "end_y": mid_y + 1, "is_final": True}})
        time.sleep(0.2)
        pump(c, grid, 0.5)
        gesture = reports(grid)[before:]
        drag_ok = (
            len(gesture) == 3
            and gesture[0][0] == 0 and gesture[0][3] == "M"      # press
            and gesture[1][0] == 32 and gesture[1][3] == "M"     # motion
            and gesture[2][0] == 0 and gesture[2][3] == "m"      # release
            and (gesture[1][1] - gesture[0][1], gesture[1][2] - gesture[0][2]) == (3, 1)
            and (gesture[2][1], gesture[2][2]) == (gesture[1][1], gesture[1][2])
        )
        results.append(("4. press/motion/release reach a tracking app",
                        drag_ok, f"{gesture}"))

        # A drag resting on the pane's TOP content row: forwarded once, and the
        # ticker must not replay it (which would spray motion reports at the app).
        before = len(reports(grid))
        c.send({"MouseClick": {"x": mid_x, "y": mid_y}})
        time.sleep(0.15)
        c.send({"MouseDrag": {"start_x": mid_x, "start_y": mid_y,
                              "end_x": mid_x, "end_y": top_y, "is_final": False}})
        time.sleep(0.3)
        pump(c, grid, 0.4)
        settled = len(reports(grid))
        idle = pump(c, grid, 2.0)
        after_idle = len(reports(grid))
        c.send({"MouseDrag": {"start_x": mid_x, "start_y": mid_y,
                              "end_x": mid_x, "end_y": top_y, "is_final": True}})
        time.sleep(0.2)
        pump(c, grid, 0.3)
        results.append(("5. an edge drag on a tracking pane is not replayed by the ticker",
                        idle == 0 and after_idle == settled,
                        f"{settled - before} reports for the gesture, "
                        f"{after_idle - settled} more + {idle} frames during 2s of rest"))

        # -- 5b. Visual mode is remux's copy-mode: the user asked for the mouse
        # explicitly, so it outranks the application's tracking (the client
        # already refuses to forward the wheel there; a drag must match).
        c.send({"ModeChanged": {"mode": "VISUAL"}})
        time.sleep(0.2)
        pump(c, grid, 0.3)
        before = len(reports(grid))
        c.send({"MouseClick": {"x": mid_x, "y": mid_y}})
        time.sleep(0.15)
        c.send({"MouseDrag": {"start_x": mid_x, "start_y": mid_y,
                              "end_x": mid_x + 4, "end_y": mid_y, "is_final": False}})
        time.sleep(0.2)
        c.send({"MouseScroll": {"x": mid_x, "y": mid_y, "up": True}})
        time.sleep(0.2)
        pump(c, grid, 0.4)
        results.append(("5b. copy-mode keeps the mouse away from a tracking app",
                        len(reports(grid)) == before,
                        f"{len(reports(grid)) - before} reports leaked to the app"))
        c.send({"MouseDrag": {"start_x": mid_x, "start_y": mid_y,
                              "end_x": mid_x + 4, "end_y": mid_y, "is_final": True}})
        time.sleep(0.2)
        c.send({"ModeChanged": {"mode": "NORMAL"}})
        time.sleep(0.2)
        pump(c, grid, 0.4)

        # -- 6/7. Back on the PRIMARY screen: remux's own wheel + selection are
        # exactly as before.
        # The forwarded reports left an unterminated line in `cat`'s input
        # buffer, so flush it before the EOF that ends `cat -v`.
        c.send({"Input": {"data": [10]}})
        time.sleep(0.3)
        c.send({"Input": {"data": [4]}})
        time.sleep(0.5)
        c.send({"Input": {"data": list(b"printf '\\033[?1049l'\n")}})
        time.sleep(0.6)
        pump(c, grid, 0.6)
        vt_live = grid.viewport_top
        c.send({"MouseScroll": {"x": mid_x, "y": mid_y, "up": True}})
        time.sleep(0.3)
        pump(c, grid, 0.4)
        results.append(("6. wheel still scrolls remux scrollback in a plain shell",
                        grid.viewport_top is not None and vt_live is not None
                        and grid.viewport_top < vt_live,
                        f"viewport_top {vt_live} -> {grid.viewport_top}"))

        c.send({"MouseClick": {"x": mid_x, "y": mid_y}})
        time.sleep(0.2)
        c.drain(0.3)
        c.send({"MouseDrag": {"start_x": mid_x, "start_y": mid_y,
                              "end_x": mid_x + 6, "end_y": mid_y, "is_final": True}})
        time.sleep(0.3)
        yanked = [m for m in c.drain(0.6) if name_of(m) == "CopyToClipboard"]
        results.append(("7. drag still selects and yanks in a plain shell",
                        bool(yanked), f"{yanked[:1]}"))

        log = srv.log()
        results.append(("8. no panic in the server log", "panic" not in log.lower(), ""))
    finally:
        srv.kill()

    ok = True
    for name, passed, detail in results:
        print(f"{'PASS' if passed else 'FAIL'}: {name}" + (f"  [{detail}]" if detail else ""))
        ok = ok and passed
    print("PASS: session mouse policy" if ok else "FAIL: session mouse policy")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
