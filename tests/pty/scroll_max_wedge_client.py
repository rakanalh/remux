"""The CLIENT half of the max-scroll wedge (PTY): can the real TUI tell that it
is scrolled when it is scrolled all the way to the top of history?

Companion to `tests/frame/scroll_max_wedge.py`, which tests the SERVER half.
Both halves are needed and neither substitutes for the other: the frame harness
speaks the socket itself, so it is blind to what the real client believes, and a
PTY test would still pass with the client half reverted because the server's
`snap_client_to_live_tail` would quietly rescue it.

The discriminator is therefore WHO ends the scroll, read out of the server log:

  * `server: ScrollReset client_id=..`  -- the CLIENT noticed it was scrolled and
    asked. This is the client half working: it can only happen if `is_scrolled`
    is true at maximum scroll, which is only true if the client reads the render
    frames' new `scroll_offset` instead of `viewport_top` (which is 0 there).
  * `server: input returns client_id=..` -- the server had to snap it, because
    the client did not ask.

At maximum scroll the first must appear and the second must not: the client
sends `ScrollReset` before the `Input` for the same keystroke, so by the time
the input arrives the offset is already 0 and the server's safety net is a no-op.

Asserted here:
  (1) the client is told the truth: one render frame in the client log reports
      `viewport_top=0` together with a NON-ZERO `scroll_offset`;
  (2) typing at maximum scroll makes the CLIENT send `ScrollReset` (and the
      server never has to snap it);
  (3) the marker the user typed is actually on screen afterwards, and the client
      is still alive.

All three FAIL against a baseline binary built from e0ab432.

Run from the repo root:
    python3 tests/pty/scroll_max_wedge_client.py [-v]
    REMUX_BIN=/path/to/baseline/remux python3 tests/pty/scroll_max_wedge_client.py
"""
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pty_harness import Tui  # noqa: E402

RUNDIR = "/tmp/rmx_smwc"
VERBOSE = "-v" in sys.argv
MARK = "PTYWEDGE_7171"


def wheel_up(t, col, row, n=1):
    # SGR mouse wheel: button 64=up; press 'M'; 1-based coords.
    seq = f"\x1b[<64;{col};{row}M".encode()
    for _ in range(n):
        t.send(seq, 0.08)


def server_log(t):
    return t.log("server")


def at_max(t):
    """The last (offset, max) the server clamped this client to.

    Unanchored: the current server appends pane detail after `max_scrollable=N`
    and the baseline does not, so this reads both.
    """
    hits = re.findall(r"ScrollDelta client_id=\d+ delta=-?\d+ "
                      r"new_offset=(\d+) max_scrollable=(\d+)", server_log(t))
    return (int(hits[-1][0]), int(hits[-1][1])) if hits else (0, 0)


def main():
    results = []

    def check(label, ok, detail=""):
        results.append((label, ok, detail))
        print(f"{'PASS' if ok else 'FAIL'}  {label}" + (f"  -- {detail}" if detail else ""))

    t = Tui(RUNDIR, cols=100, rows=30).start()
    try:
        # Primary-screen history, so there is a maximum scroll to reach.
        t.send("for i in $(seq 1 60); do echo LINE_$i; done\r", 1.5)
        t.pump(0.6)

        # Wheel up until the server stops moving the offset: maximum scroll.
        off = mx = 0
        for _ in range(20):
            wheel_up(t, 50, 15, 5)
            off, mx = at_max(t)
            if off == mx and mx > 0:
                break
        if VERBOSE:
            print(f"clamped at offset={off} max_scrollable={mx}")

        # -- (1) the client is told the truth at the maximum. -----------------
        # Every render frame the client logs carries both numbers. At the maximum
        # `viewport_top` is 0 -- the same value the live tail reports -- so a
        # frame that pairs it with a non-zero `scroll_offset` is the client
        # seeing through the blindness.
        pairs = [(int(a), int(b)) for a, b in
                 re.findall(r"viewport_top=(\d+) scroll_offset=(\d+)", t.log("client"))]
        blind_but_seeing = [(vt, so) for vt, so in pairs if vt == 0 and so > 0]
        if VERBOSE:
            print(f"client frames with viewport_top=0: "
                  f"{[p for p in pairs if p[0] == 0][:6]}")
        check("a render frame reports viewport_top=0 with a non-zero scroll_offset",
              bool(blind_but_seeing) and off == mx and mx > 0,
              f"server clamped to {off}/{mx}; matching frames={blind_but_seeing[:3]}")

        # -- (2) typing: the CLIENT ends the scroll, not the server's net. -----
        before = len(server_log(t))
        t.send(f"echo {MARK}\r", 1.5)
        t.pump(0.8)
        tail = server_log(t)[before:]
        asked = "server: ScrollReset client_id=" in tail
        rescued = "to the live tail from offset=" in tail
        check("typing at max scroll makes the CLIENT send ScrollReset",
              asked and not rescued,
              f"client asked={asked} server had to snap={rescued}")

        # -- (3) and the user sees the result. --------------------------------
        seen = t.has(MARK)
        alive = t.alive()
        if VERBOSE and not seen:
            t.dump("after typing")
        check("the typed marker is visible and the client is alive",
              seen and alive, f"marker seen={seen} alive={alive}")

        panicked = "panicked" in t.log("client") or "panicked" in server_log(t)
        check("no panic in the client or server log", not panicked)
    finally:
        t.kill()

    print()
    bad = [r for r in results if not r[1]]
    print(f"{len(results) - len(bad)}/{len(results)} checks passed")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
