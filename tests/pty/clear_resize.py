"""After a clear, zooming and unzooming a pane keeps the prompt at its top.

The harness drives the real client. The left pane of a vertical split prints
a hundred numbered lines, clears its screen with ED 2 (`CSI H CSI 2J`, which
leaves the scrollback in place, unlike `clear`'s ED 3), and draws its prompt
on the top row. A zoom then widens the pane and an unzoom narrows it again,
and both reflow its screen.

What it covers:

  1  before any resize, the prompt is on the pane's top row and none of the
     numbered lines is on screen
  2  after the zoom, and again after the unzoom, the prompt is still on the
     pane's top row and no numbered line has come back into view above it
  3  the client stays alive and neither log holds a panic

The numbered lines are printed as `N:<n>` by printf, so the typed command line
never matches what the check looks for.

Run from the repo root: python3 tests/pty/clear_resize.py [-v]
"""
import re
import sys

from pty_harness import Tui
from rundirs import run_dir  # noqa: E402

COLS, ROWS = 120, 30
VERBOSE = "-v" in sys.argv
NUMBERED = re.compile(r"N:\d+")

fails = []


def check(cond, msg):
    if VERBOSE or not cond:
        print(("  PASS  " if cond else "  FAIL  ") + msg)
    if not cond:
        fails.append(msg)


def left_pane(t):
    """The left pane's interior rows, between its borders."""
    rows = t.cell_rows()
    right = rows[0].find("╮")
    return [r[1:right] for r in rows[1:-2]]


def verify(t, label):
    pane = left_pane(t)
    check(pane[0].startswith("$"), f"{label}: the prompt is on the top row ({pane[0].rstrip()!r})")
    shown = [r.strip() for r in pane if NUMBERED.search(r)]
    check(not shown, f"{label}: no numbered line is on screen ({shown[:3]})")


def main():
    rundir = run_dir("rmxclrz")
    t = Tui(rundir, cols=COLS, rows=ROWS, extra_env={"XDG_CACHE_HOME": f"{rundir}/cache"})
    t.start()
    try:
        t.prefix(b"pv", 1.0)
        t.send(b"\x1bh", 0.6)
        t.send("i=0; while [ $i -lt 100 ]; do printf 'N:%s\\n' $i; i=$((i+1)); done\r", 1.0)
        t.send("printf '\\033[H\\033[2J'\r", 1.0)
        verify(t, "1 after the clear")
        t.prefix(b"z", 1.2)
        verify(t, "2 zoomed")
        t.prefix(b"z", 1.2)
        verify(t, "2 unzoomed")
        if fails:
            t.dump("at failure")
        check(t.alive(), "3 the client is alive")
    finally:
        t.kill()
    for which in ("client", "server"):
        check("panicked at" not in t.log(which), f"3 no panic in {which}.log")
    print(f"{'FAIL' if fails else 'PASS'}  clear_resize: {len(fails)} failure(s)")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
