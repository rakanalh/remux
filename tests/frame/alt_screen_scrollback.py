#!/usr/bin/env python3
"""Alt-screen panes have no scrollback of their own (frame harness).

The user-visible symptom: scrolling (wheel, `ScrollDelta`, visual mode) inside a
pane running a full-screen application that does NOT grab the mouse -- neovim
without `set mouse=a`, `less`, a TUI installer -- walked remux's viewport into
the PRIMARY screen's history. That history has nothing to do with what is on
screen, so the pane appeared to "scroll someone else's text".

Two things were wrong and both are asserted here:

  * an alt-screen application's own redraws kept PUSHING lines into the
    scrollback (`Screen::scroll_up_region` pushed whenever `scroll_top == 0`,
    with no alt-screen guard), so the buffer grew without bound while a
    full-screen app was up;
  * the primary screen's existing history stayed addressable while the alt
    screen was active, so any scroll gesture revealed it.

The fix makes the alternate screen historyless (tmux/zellij semantics): the
primary scrollback is set aside on entry and handed back untouched on exit.

Assertions 3 and 4 FAIL before the fix (`viewport_top` moves and `LINE_` text
from the primary screen appears while the alt screen is up).

Run from the repo root:  python3 tests/frame/alt_screen_scrollback.py
"""
import os
import re
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import Client, Server, name_of, only  # noqa: E402

RUNDIR = "/tmp/rmx_ass"
COLS, ROWS = 100, 30


class Grid:
    """Reconstruct the composited grid from Full/Diff/Scroll renders."""

    def __init__(self, cols, rows):
        self.cols, self.rows = cols, rows
        self.g = [[" "] * cols for _ in range(rows)]
        self.viewport_top = None

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
    msgs = c.drain(t)
    for m in msgs:
        grid.apply(m)
    return len(msgs)


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

        # Several screens of PRIMARY history.
        c.send({"Input": {"data": list(b"for i in $(seq 1 200); do echo LINE_$i; done\n")}})
        time.sleep(1.2)
        pump(c, grid, 0.8)
        live_top = grid.viewport_top
        live_lines = grid.marks("LINE")
        print("live viewport_top:", live_top, "showing LINE_", min(live_lines), "..", max(live_lines))

        # -- 1. Baseline: on the PRIMARY screen the wheel scrolls into history.
        c.send({"ScrollDelta": {"delta": 20}})
        time.sleep(0.3)
        pump(c, grid, 0.5)
        scrolled_top = grid.viewport_top
        scrolled_lines = grid.marks("LINE")
        results.append((
            "1. primary-screen scroll reaches history (baseline)",
            scrolled_top is not None and live_top is not None
            and scrolled_top < live_top and min(scrolled_lines) < min(live_lines),
            f"viewport_top {live_top} -> {scrolled_top}, top LINE "
            f"{min(live_lines)} -> {min(scrolled_lines)}",
        ))
        # Back to the live tail.
        c.send({"ScrollDelta": {"delta": -1000}})
        time.sleep(0.3)
        pump(c, grid, 0.5)

        # -- 2. Enter the alt screen and let the "application" redraw a lot.
        # None of that output may become scrollback.
        c.send({"Input": {"data": list(b"printf '\\033[?1049h'\n")}})
        time.sleep(0.5)
        pump(c, grid, 0.4)
        c.send({"Input": {"data": list(b"for i in $(seq 1 120); do echo ALT_$i; done\n")}})
        time.sleep(1.2)
        pump(c, grid, 0.8)
        alt_top = grid.viewport_top
        alt_text_before = grid.text()
        results.append((
            "2. the alt screen is showing the application's own output",
            bool(grid.marks("ALT")),
            f"ALT_ marks present: {bool(grid.marks('ALT'))}",
        ))

        # -- 3/4. Scroll gestures on the alt screen: nothing to scroll.
        # Deep enough to walk past everything the alt-screen app itself emitted
        # and into the primary screen's `LINE_` history -- which is exactly what
        # the user saw in neovim.
        c.send({"ScrollDelta": {"delta": 300}})
        time.sleep(0.4)
        pump(c, grid, 0.6)
        after_delta_top = grid.viewport_top
        leaked = grid.marks("LINE")
        results.append((
            "3. keyboard scroll does not move an alt-screen pane's viewport",
            after_delta_top == alt_top,
            f"viewport_top {alt_top} -> {after_delta_top}",
        ))
        results.append((
            "4. no primary-screen history is revealed on the alt screen",
            not leaked,
            f"LINE_ marks visible while on the alt screen: {sorted(leaked)[:8]}",
        ))
        results.append((
            "5. the alt screen's own content is untouched by the scroll",
            grid.text() == alt_text_before,
            "grid identical before/after the ScrollDelta",
        ))
        # The wheel is a separate policy (8a85ca9 forwards it as arrow keys to
        # the full-screen app, so the CONTENT may legitimately change) -- what
        # must not happen is remux scrolling its own viewport.
        c.send({"MouseScroll": {"x": 5, "y": 5, "up": True}})
        time.sleep(0.2)
        c.send({"MouseScroll": {"x": 5, "y": 5, "up": True}})
        time.sleep(0.3)
        pump(c, grid, 0.5)
        results.append((
            "6. the wheel does not move an alt-screen pane's viewport either",
            grid.viewport_top == alt_top and not grid.marks("LINE"),
            f"viewport_top {alt_top} -> {grid.viewport_top}, "
            f"LINE_ leaked: {sorted(grid.marks('LINE'))[:4]}",
        ))

        # -- 7/8. Leave the alt screen: the primary history is back, intact, and
        # scrollable again. Ctrl-C first: the wheel above was forwarded to the
        # shell as arrow keys, which land on its input line.
        c.send({"Input": {"data": [0x03]}})
        time.sleep(0.3)
        pump(c, grid, 0.3)
        c.send({"Input": {"data": list(b"printf '\\033[?1049l'\n")}})
        time.sleep(0.6)
        pump(c, grid, 0.6)
        back_top = grid.viewport_top
        back_lines = grid.marks("LINE")
        results.append((
            "7. leaving the alt screen restores the primary screen",
            bool(back_lines) and not grid.marks("ALT"),
            f"top LINE {min(back_lines) if back_lines else None}, "
            f"ALT_ marks gone: {not grid.marks('ALT')}",
        ))
        seen_line, seen_alt = set(), set()
        for _ in range(6):
            c.send({"ScrollDelta": {"delta": 20}})
            time.sleep(0.25)
            pump(c, grid, 0.4)
            seen_line |= grid.marks("LINE")
            seen_alt |= grid.marks("ALT")
        again_top = grid.viewport_top
        results.append((
            "8. scrolling works again and shows the ORIGINAL history",
            again_top is not None and back_top is not None and again_top < back_top
            and bool(seen_line) and min(seen_line) <= min(scrolled_lines),
            f"viewport_top {back_top} -> {again_top}, LINE_ range seen "
            f"{min(seen_line) if seen_line else None}.."
            f"{max(seen_line) if seen_line else None} "
            f"(pre-alt run reached {min(scrolled_lines)})",
        ))
        results.append((
            "9. the app's alt-screen output never entered the history",
            not seen_alt,
            f"ALT_ marks wedged into the restored history: {sorted(seen_alt)[:8]}",
        ))

        results.append(("10. no panic in the server log",
                        "panic" not in srv.log().lower(), ""))
    finally:
        srv.kill()

    ok = True
    for name, passed, detail in results:
        print(f"{'PASS' if passed else 'FAIL'}: {name}" + (f"  [{detail}]" if detail else ""))
        ok = ok and passed
    print("PASS: alt-screen scrollback guard" if ok else "FAIL: alt-screen scrollback guard")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
