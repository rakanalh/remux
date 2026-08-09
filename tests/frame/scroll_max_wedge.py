"""Scrolled to the very TOP of history: does the session go dead?

User report: a remote session where every wheel event logged
`ScrollDelta ... new_offset=15 max_scrollable=15` -- the maximum scroll -- while
"visual mode scroll up does not work beyond the top of the pane's first line"
and, earlier, "typing does nothing". The server log showed the application alive
and answering every keystroke (`input 1 bytes -> pane_id=..` followed by two or
three `broadcast_full_render`) with the viewport pinned in history the whole
time, so the output landed below the visible area and the screen never changed.

That maximum is the trap. `viewport_top` -- until now the only scroll signal the
render messages carried -- is the ABSOLUTE index of the first displayed line,
i.e. `total - scroll_offset - pane_h`; at `scroll_offset == max_scroll_offset()`
(which is `total - pane_h`) it collapses to exactly **0**, byte-identical to the
live-tail case. The client read it as its scroll offset
(`scroll_offset = so; is_scrolled = so > 0`), so at max scroll the client
believed it was NOT scrolled, and every path that would return the viewport to
the live tail is gated on that belief:

  * typing (`if is_scrolled { ... ScrollReset }`)
  * returning to Normal mode / Escape
  * leaving Visual mode, cancelling Search

None of them fired. Zoom made it easy to hit because a full-height pane shrinks
`max_scroll_offset` to a handful of lines -- a couple of wheel notches and you
are at the maximum.

The fix has two halves. The wire now carries a real `scroll_offset` beside
`viewport_top` (`#[serde(default)]`, so no PROTOCOL_VERSION bump), which is what
lets a client tell max scroll from the live tail; and the SERVER returns a
client to the live tail when it types, so the recovery does not depend on the
client believing anything. This file tests the server half and the new field --
the client half (that the real TUI now notices and sends `ScrollReset` itself)
needs a PTY, and lives in `tests/pty/scroll_max_wedge_client.py`.

Asserted here:
  (1) scrolling up eventually pins at max (the offset stops changing);
  (2) at that point ONE frame reports `viewport_top == 0` -- indistinguishable
      from the live tail -- while `scroll_offset` reports the true maximum. That
      pair is the whole point of the new field: the blindness is real and the
      client can now see through it;
  (3) THE WEDGE: typing into the pane must bring the viewport back to the live
      tail, so the typed marker's output becomes visible;
  (4) and the frames say so: the reported `scroll_offset` is 0 again;
  (5) a SECOND client scrolled back in the same session is NOT moved by the
      first one typing (the snap is per-client);
  (6) copy mode is untouched: a client that reports Visual keeps its offset even
      if input arrives, so an explicit scrollback session is never yanked to the
      bottom mid-selection.

Assertions (2) [the `scroll_offset` half], (3) and (4) FAIL against a baseline
binary built from e0ab432 and pass after the fix. Note the baseline lacks the
`ScrollDelta` instrumentation too, but `last_offset`'s regex is unanchored and
matches both the plain and the instrumented log line, so (1) is comparable.

Run from the repo root:
    python3 tests/frame/scroll_max_wedge.py [-v]
    REMUX_BIN=/path/to/baseline/remux python3 tests/frame/scroll_max_wedge.py
"""
import os
import re
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import Client, Server, name_of, only  # noqa: E402

RUNDIR = "/tmp/rmx_smw"
COLS, ROWS = 100, 30
VERBOSE = "-v" in sys.argv
MARK = "WEDGEMARK_4242"


class Grid:
    """Reconstruct the composited grid from Full/Diff/Scroll renders.

    `viewport_top` and `scroll_offset` are captured TOGETHER from each frame, so
    the pair can be asserted on as one observation rather than two sightings
    that might come from different renders.
    """

    def __init__(self, cols, rows):
        self.cols, self.rows = cols, rows
        self.g = [[" "] * cols for _ in range(rows)]
        self.viewport_top = None
        # None until a frame carries the field at all: a pre-fix server omits it
        # (it does not exist there), which is exactly the state check (2) fails on.
        self.scroll_offset = None
        self.frames = 0

    def _ch(self, cell):
        return cell.get("c", " ") if isinstance(cell, dict) else " "

    def _scroll_fields(self, body):
        self.viewport_top = body.get("viewport_top", self.viewport_top)
        if "scroll_offset" in body:
            self.scroll_offset = body["scroll_offset"]
        self.frames += 1

    def apply(self, msg):
        n = name_of(msg)
        body = only(msg, n)
        if n == "FullRender":
            for y, row in enumerate(body["cells"]):
                for x, cell in enumerate(row):
                    if y < self.rows and x < self.cols:
                        self.g[y][x] = self._ch(cell)
            self._scroll_fields(body)
        elif n == "RenderDiff":
            for ch in body["changes"]:
                y, x = ch["y"], ch["x"]
                if y < self.rows and x < self.cols:
                    self.g[y][x] = self._ch(ch["cell"])
            self._scroll_fields(body)
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
            self._scroll_fields(body)

    def text(self):
        return "\n".join("".join(r) for r in self.g)


def pump(c, grid, t=0.5):
    for m in c.drain(t):
        grid.apply(m)


def last_offset(srv):
    """The most recent (offset, max) the server clamped this client to.

    Unanchored on purpose: the current server appends pane detail after
    `max_scrollable=N`, the baseline does not, and this must read both.
    """
    hits = re.findall(r"ScrollDelta client_id=\d+ delta=-?\d+ "
                      r"new_offset=(\d+) max_scrollable=(\d+)", srv.log())
    return (int(hits[-1][0]), int(hits[-1][1])) if hits else (0, 0)


def scroll_to_max(c, grid, srv):
    """Wheel up until the clamped offset stops moving. Returns (offset, max)."""
    pinned = 0
    for _ in range(60):
        c.send({"ScrollDelta": {"delta": 3}})
        time.sleep(0.08)
        pump(c, grid, 0.15)
        off, mx = last_offset(srv)
        if off == mx and mx > 0:
            pinned += 1
            if pinned >= 3:
                break
        else:
            pinned = 0
    return last_offset(srv)


def main():
    srv = Server(RUNDIR).start()
    results = []

    def check(label, ok, detail=""):
        results.append((label, ok, detail))
        print(f"{'PASS' if ok else 'FAIL'}  {label}" + (f"  -- {detail}" if detail else ""))

    try:
        c = Client(srv.sock)
        c.hello()
        c.send({"CreateSession": {"name": "main", "folder": None}})
        c.send({"Attach": {"session_name": "main"}})
        c.send({"Resize": {"cols": COLS, "rows": ROWS}})
        time.sleep(0.4)
        grid = Grid(COLS, ROWS)
        pump(c, grid, 0.5)

        # A couple of screens of primary-screen history, so there IS a maximum
        # scroll to reach (and the pane is provably not on the alt screen).
        c.send({"Input": {"data": list(b"for i in $(seq 1 60); do echo LINE_$i; done\n")}})
        time.sleep(1.2)
        pump(c, grid, 0.8)
        live_top, live_so = grid.viewport_top, grid.scroll_offset
        if VERBOSE:
            print(f"live viewport_top={live_top} scroll_offset={live_so}")

        # -- (1) scroll up until the offset stops moving: we are at the max. ---
        off, mx = scroll_to_max(c, grid, srv)
        check("scroll pins at maximum offset",
              off == mx and mx > 0, f"offset={off} max_scrollable={mx}")

        # -- (2) the lie, and the field that sees through it -------------------
        # At max scroll the frame's viewport_top is 0, exactly what the live tail
        # reports -- so `is_scrolled = viewport_top > 0` is false at precisely the
        # moment the client is MOST scrolled. The frame's own scroll_offset must
        # report the true maximum on that same frame.
        pump(c, grid, 0.4)
        blind = grid.viewport_top == 0
        sees = grid.scroll_offset == mx and mx > 0
        check("at max scroll one frame reports viewport_top == 0 AND scroll_offset == max",
              blind and sees,
              f"viewport_top={grid.viewport_top} scroll_offset={grid.scroll_offset} "
              f"server offset={off} max={mx}")

        # -- (5) a second client, scrolled back, must not be dragged along. ----
        c2 = Client(srv.sock)
        c2.hello()
        c2.send({"Attach": {"session_name": "main"}})
        c2.send({"Resize": {"cols": COLS, "rows": ROWS}})
        time.sleep(0.4)
        grid2 = Grid(COLS, ROWS)
        pump(c2, grid2, 0.5)
        for _ in range(3):
            c2.send({"ScrollDelta": {"delta": 2}})
            time.sleep(0.1)
            pump(c2, grid2, 0.2)
        pump(c2, grid2, 0.3)
        c2_before = grid2.scroll_offset
        if VERBOSE:
            print(f"second client scroll_offset before={c2_before}")

        # -- (3) THE WEDGE --------------------------------------------------
        # Type into the pane. The application answers (it echoes and prints the
        # marker), but if the viewport stays pinned in history the marker is
        # rendered below the visible area and the user sees a dead session.
        before = grid.text()
        c.send({"Input": {"data": list(f"echo {MARK}\n".encode())}})
        time.sleep(1.0)
        pump(c, grid, 1.0)
        after = grid.text()
        check("typing returns the viewport to the live tail (marker visible)",
              MARK in after,
              f"marker {'seen' if MARK in after else 'NOT seen'}; "
              f"grid {'changed' if after != before else 'FROZEN (unchanged)'}; "
              f"viewport_top={grid.viewport_top}")

        # -- (4) and the frames say so, in the field the client acts on. ------
        check("the frame after typing reports scroll_offset == 0 (live tail)",
              grid.scroll_offset == 0,
              f"reported scroll_offset={grid.scroll_offset}")

        # -- (5, cont.) the other client stayed where it was. ------------------
        pump(c2, grid2, 0.6)
        check("a second scrolled-back client is not snapped by the first typing",
              c2_before is not None and c2_before > 0 and grid2.scroll_offset == c2_before,
              f"before={c2_before} after={grid2.scroll_offset}")
        c2.close()

        # -- (6) copy mode keeps its scrollback session. -----------------------
        # Visual is remux's copy mode: an explicit trip into history that the
        # user drives with keys. Input must not yank it to the bottom.
        c.send({"ModeChanged": {"mode": "VISUAL"}})
        time.sleep(0.2)
        pump(c, grid, 0.3)
        off_v, mx_v = scroll_to_max(c, grid, srv)
        pump(c, grid, 0.4)
        visual_before = grid.scroll_offset
        c.send({"Input": {"data": list(b"\n")}})
        time.sleep(0.8)
        pump(c, grid, 0.8)
        check("copy mode (Visual) is not snapped to the live tail by input",
              visual_before is not None and visual_before > 0
              and grid.scroll_offset == visual_before,
              f"visual offset before={visual_before} after={grid.scroll_offset} "
              f"(server clamped to {off_v}/{mx_v})")
        c.send({"ModeChanged": {"mode": "NORMAL"}})
        time.sleep(0.2)

        if VERBOSE:
            print("---- grid after typing ----")
            print(after)

        c.close()
    finally:
        srv.kill()

    print()
    bad = [r for r in results if not r[1]]
    print(f"{len(results) - len(bad)}/{len(results)} checks passed")
    if "panicked" in srv.log():
        print("FAIL  server log contains a panic")
        bad.append(("panic", False, ""))
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
