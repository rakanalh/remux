#!/usr/bin/env python3
"""A pane's history must survive a height change (frame harness).

The user-visible symptom: "visual mode scroll up does not work beyond the top of
the pane's first line, though there is definitely something above -- and mouse
scroll doesn't work either". Their server log showed the wheel pinned:

    server: ScrollDelta client_id=1 delta=3 new_offset=15 max_scrollable=15

`max_scrollable` is `Screen::max_scroll_offset()`, which is
`total_lines() - rows`; since `grid.len() == rows` in every code path, that is
*exactly* `scrollback.len()`. So the number is never a clamp or an off-by-N in
the reporting chain -- the pane genuinely held only 15 lines of history while
hundreds of lines of real work had scrolled past.

Three different things produce exactly that shape, and this file covers the one
that is a genuine defect: a resize that changes only the HEIGHT. (The other two
are a pane wrongly believed to still be on the alt screen, and an application
that left a `DECSTBM` scroll region whose top is not row 0 -- both suppress
accumulation without corrupting the display, and the `ScrollDelta` log line now
prints `alt=` and `region=` so a report can tell them apart.)

`Screen::resize` used to route height-only resizes to `resize_clamp` ("nothing
can rewrap when the width is unchanged"), and `clamp_grid` keeps the **top**
`rows` of the grid. Shrinking the
height therefore *deleted the bottom rows outright* -- the cursor, the prompt and
the newest output -- and deleted them without them ever reaching scrollback. Two
very ordinary gestures change the height alone: zooming a pane that already spans
the full width (`Prefix+f` on a stacked split), and the terminal window getting
shorter. Each one ate the lines that no longer fit and left `max_scroll_offset()`
frozen, so the wheel and Visual mode both kept stopping at the same stale line no
matter how much had since been printed.

The same defect sat one indirection out, in `resize_saved_screen`: the parked
PRIMARY snapshot was clamped the same way, so resizing a pane while a full-screen
application was up silently ate the bottom of the screen the user would get back
when the application exited.

Asserted here, all quantitative:
  1. 500 lines emitted to the primary screen are all reachable by scrolling.
  2. The server's clamp and the client's `ScrollbackInfo` bound agree.
  3. A pure height change (the zoom analogue) loses nothing: every one of the
     500 marks is still reachable, and the reported total grew by exactly the
     number of new lines.
  4. An alt-screen round trip WITH a resize in the middle loses nothing.
  5. An application that never leaves the alt screen does not take the history
     with it, and its own output never enters the primary history.

Assertions 3 and 4 FAIL before the fix.

Run from the repo root:  python3 tests/frame/resize_scrollback_loss.py
"""
import os
import re
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import Client, Server, name_of, only  # noqa: E402

RUNDIR = "/tmp/rmx_rsl"
COLS, ROWS = 100, 30
TALL = 46
NLINES = 500


class Grid:
    """Reconstruct the composited grid from Full/Diff/Scroll renders."""

    def __init__(self, cols, rows):
        self.cols, self.rows = cols, rows
        self.g = [[" "] * cols for _ in range(rows)]
        self.viewport_top = None
        self.pane_h = None

    def resize(self, cols, rows):
        self.cols, self.rows = cols, rows
        self.g = [[" "] * cols for _ in range(rows)]

    def _ch(self, cell):
        return cell.get("c", " ") if isinstance(cell, dict) else " "

    def apply(self, msg):
        n = name_of(msg)
        body = only(msg, n)
        if n in ("FullRender", "RenderDiff", "ScrollRender"):
            fpr = body.get("focused_pane_rect")
            if fpr:
                self.pane_h = fpr["height"]
        if n == "FullRender":
            for y, row in enumerate(body["cells"]):
                for x, cell in enumerate(row):
                    if y < self.rows and x < self.cols:
                        self.g[y][x] = self._ch(cell)
            self.viewport_top = body.get("viewport_top", self.viewport_top)
        elif n == "RenderDiff":
            for ch in body["changes"]:
                y, x = ch["y"], ch["x"]
                if y < self.rows and x < self.cols:
                    self.g[y][x] = self._ch(ch["cell"])
            self.viewport_top = body.get("viewport_top", self.viewport_top)
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

    def marks(self, prefix):
        return {int(m) for m in re.findall(rf"{prefix}_(\d+)", self.text())}


def pump(c, grid, t=0.5):
    for m in c.drain(t):
        grid.apply(m)


def scrollback_total(c, grid):
    """`ScrollbackInfo.total_lines` -- the bound the client's Visual mode uses."""
    c.send("RequestScrollbackInfo")
    end = time.time() + 1.5
    total = None
    while time.time() < end:
        for m in c.drain(0.25):
            grid.apply(m)
            if name_of(m) == "ScrollbackInfo":
                total = only(m, "ScrollbackInfo")["total_lines"]
        if total is not None:
            return total
    return total


def sweep_up(c, grid, prefix, steps=60, delta=15):
    """Scroll to the very top, collecting every mark seen on the way."""
    seen = set(grid.marks(prefix))
    last_top = None
    for _ in range(steps):
        c.send({"ScrollDelta": {"delta": delta}})
        time.sleep(0.12)
        pump(c, grid, 0.2)
        seen |= grid.marks(prefix)
        if grid.viewport_top == last_top:
            break
        last_top = grid.viewport_top
    return seen, grid.viewport_top


def to_live_tail(c, grid):
    c.send({"ScrollDelta": {"delta": -100000}})
    time.sleep(0.3)
    pump(c, grid, 0.5)


def emit(c, grid, prefix, first, last, settle=1.6):
    c.send({"Input": {"data": list(
        f"for i in $(seq {first} {last}); do echo {prefix}_$i; done\n".encode())}})
    time.sleep(settle)
    pump(c, grid, 1.0)


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

        # -- 1. A known number of lines on the PRIMARY screen, all reachable.
        emit(c, grid, "MARK", 1, NLINES, settle=2.5)
        total0 = scrollback_total(c, grid)
        seen, top0 = sweep_up(c, grid, "MARK")
        results.append((
            "1. all 500 primary-screen lines are reachable by scrolling",
            1 in seen and NLINES in seen and top0 == 0,
            f"marks {min(seen) if seen else None}..{max(seen) if seen else None}, "
            f"{len(seen)} distinct, viewport_top bottomed out at {top0}",
        ))

        # -- 2. The server's clamp and the client's bound describe the SAME
        # history. `max_scrollable` is total - pane_h and the pane is the whole
        # screen bar the status row, so the two must land within a row of each
        # other; what matters is that neither reports a history the other cannot
        # reach.
        log = srv.log()
        clamps = [int(m) for m in re.findall(r"max_scrollable=(\d+)", log)]
        server_max = max(clamps) if clamps else None
        pane_h = grid.pane_h
        results.append((
            "2. server clamp and client ScrollbackInfo agree on the history",
            server_max is not None and total0 is not None and pane_h is not None
            and total0 - server_max == pane_h,
            f"ScrollbackInfo.total_lines={total0}, server max_scrollable={server_max}, "
            f"pane height={pane_h}: total - max = "
            f"{total0 - server_max if (total0 and server_max) else None} "
            f"(must equal the pane height, so the last scroll step lands on line 0)",
        ))

        to_live_tail(c, grid)

        # -- 3. THE ZOOM ANALOGUE: a resize that changes only the height.
        # Taller, print, back. Nothing may be lost and the reported total must
        # grow by exactly what was printed.
        c.send({"Resize": {"cols": COLS, "rows": TALL}})
        grid.resize(COLS, TALL)
        time.sleep(0.5)
        pump(c, grid, 0.6)
        before = scrollback_total(c, grid)
        emit(c, grid, "ZOOM", 1, 30)
        c.send({"Resize": {"cols": COLS, "rows": ROWS}})
        grid.resize(COLS, ROWS)
        time.sleep(0.5)
        pump(c, grid, 0.6)
        after = scrollback_total(c, grid)
        # The height went back to what it was, so the same content occupies the
        # same total -- plus exactly the 30 lines printed in between (plus the
        # shell's own echoed command line, hence the small allowance).
        grew = (after - before) if (after is not None and before is not None) else None
        results.append((
            "3a. a height round trip accounts for every line printed during it",
            grew is not None and 30 <= grew <= 34,
            f"ScrollbackInfo.total_lines {before} -> {after} (grew {grew}, expected 30..34)",
        ))
        to_live_tail(c, grid)
        seen, top = sweep_up(c, grid, "MARK")
        zoom_seen = grid.marks("ZOOM")
        results.append((
            "3b. no line was deleted by the height change",
            1 in seen and NLINES in seen and top == 0,
            f"marks {min(seen) if seen else None}..{max(seen) if seen else None}, "
            f"{len(seen)} distinct, viewport_top bottomed out at {top}",
        ))
        to_live_tail(c, grid)
        zoom_seen |= sweep_up(c, grid, "ZOOM")[0]
        missing = sorted(set(range(1, 31)) - zoom_seen)
        results.append((
            "3c. every line printed while tall is still reachable",
            not missing,
            f"{len(zoom_seen & set(range(1, 31)))}/30 ZOOM_ marks reachable, "
            f"deleted by the unzoom: {missing[:12]}",
        ))
        to_live_tail(c, grid)

        # -- 4. Alt-screen round trip WITH a resize in the middle.
        c.send({"Input": {"data": list(b"printf '\\033[?1049h'\n")}})
        time.sleep(0.6)
        pump(c, grid, 0.5)
        emit(c, grid, "ALT", 1, 60)
        c.send({"Resize": {"cols": COLS, "rows": TALL}})
        grid.resize(COLS, TALL)
        time.sleep(0.5)
        pump(c, grid, 0.6)
        alt_total = scrollback_total(c, grid)
        results.append((
            "4a. the alt screen reports no history of its own",
            alt_total is not None and alt_total <= TALL,
            f"ScrollbackInfo.total_lines while alt-active = {alt_total} (<= {TALL})",
        ))
        c.send({"Resize": {"cols": COLS, "rows": ROWS}})
        grid.resize(COLS, ROWS)
        time.sleep(0.5)
        pump(c, grid, 0.6)
        c.send({"Input": {"data": [0x03]}})
        time.sleep(0.3)
        c.send({"Input": {"data": list(b"printf '\\033[?1049l'\n")}})
        time.sleep(0.8)
        pump(c, grid, 0.8)
        to_live_tail(c, grid)
        seen, top = sweep_up(c, grid, "MARK")
        alt_leak = grid.marks("ALT")
        results.append((
            "4b. all 500 lines survive an alt-screen round trip with a resize",
            1 in seen and NLINES in seen and top == 0,
            f"marks {min(seen) if seen else None}..{max(seen) if seen else None}, "
            f"{len(seen)} distinct, viewport_top bottomed out at {top}",
        ))
        results.append((
            "4c. the alt screen's own output never entered the primary history",
            not alt_leak,
            f"ALT_ marks wedged into the restored history: {sorted(alt_leak)[:8]}",
        ))
        to_live_tail(c, grid)

        # -- 5. An application that never leaves the alt screen (killed, crashed,
        # dropped connection) must not take the history with it: the next clean
        # leave hands it back whole.
        c.send({"Input": {"data": list(b"printf '\\033[?1049h'\n")}})
        time.sleep(0.6)
        pump(c, grid, 0.5)
        emit(c, grid, "DEAD", 1, 40)
        stuck_total = scrollback_total(c, grid)
        c.send({"Input": {"data": [0x03]}})
        time.sleep(0.3)
        c.send({"Input": {"data": list(b"printf '\\033[?1049l'\n")}})
        time.sleep(0.8)
        pump(c, grid, 0.8)
        to_live_tail(c, grid)
        seen, top = sweep_up(c, grid, "MARK")
        results.append((
            "5. history parked by a never-exited application comes back whole",
            1 in seen and NLINES in seen and top == 0
            and stuck_total is not None and stuck_total <= TALL,
            f"while stuck total={stuck_total}; after the leave marks "
            f"{min(seen) if seen else None}..{max(seen) if seen else None}, "
            f"{len(seen)} distinct, viewport_top {top}",
        ))

        results.append(("6. no panic in the server log",
                        "panic" not in srv.log().lower(), ""))
    finally:
        srv.kill()

    ok = True
    for name, passed, detail in results:
        print(f"{'PASS' if passed else 'FAIL'}: {name}" + (f"  [{detail}]" if detail else ""))
        ok = ok and passed
    print("PASS: resize scrollback loss" if ok else "FAIL: resize scrollback loss")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
