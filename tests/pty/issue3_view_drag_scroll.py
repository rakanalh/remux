"""Issue #3 (view branch) validation: normal-session drag-autoscroll already
works (see tests/frame/issue3_drag_autoscroll.py). The gap is that a VIEW cell
had no drag-autoscroll. Now a left-drag reaching a cell's TOP content edge
scrolls that cell's source pane into history.

Build a 1-cell view over a pane with >1 screen of numbered lines, press-drag to
the top edge repeatedly, and assert earlier line numbers scroll in.
"""
import re, sys
from pty_harness import Tui, sm_compose_view

RUNDIR = "/tmp/rmxfix/i3v"


def press(t, col, row):
    t.send(f"\x1b[<0;{col};{row}M".encode(), 0.2)


def drag(t, col, row, n=1):
    for _ in range(n):
        t.send(f"\x1b[<32;{col};{row}M".encode(), 0.15)


def release(t, col, row):
    t.send(f"\x1b[<0;{col};{row}m".encode(), 0.2)


def visible_line_nums(t):
    nums = set()
    for r in t.rows_text():
        for m in re.finditer(r"LINE_(\d+)", r):
            nums.add(int(m.group(1)))
    return nums


def main():
    t = Tui(RUNDIR, cols=120, rows=40).start()
    t.send("clear\r", 0.3)
    t.send("for i in $(seq 1 200); do echo LINE_$i; done\r", 1.2)
    t.pump(0.6)
    # Background tab so the pane is NOT "session-visible" (else its cell shows the
    # "● Active in session" placeholder instead of the live content dragged here).
    t.send(b"\x1bt", 0.6)   # Alt+t: new empty tab
    # 1-cell view over this pane.
    sm_compose_view(t, panes=(0,), settle=0.9)
    if not t.has("View 1"):
        print("FAIL: not in view"); t.dump("s"); t.kill(); sys.exit(1)

    live = visible_line_nums(t)
    print("live min/max:", min(live), max(live))

    # Press near the bottom, then drag to the TOP content edge (row 1, inside the
    # 1-cell border at row 0) and hold there with repeated drag events.
    press(t, 40, 36)
    drag(t, 40, 1, n=20)
    t.pump(0.4)
    after = visible_line_nums(t)
    print("after top-edge drag min/max:", (min(after), max(after)) if after else None)
    release(t, 40, 1)

    scrolled = bool(after) and min(after) < min(live)
    alive = t.alive()
    panic = "panic" in t.log("client").lower() or "panic" in t.log("server").lower()
    if not scrolled:
        t.dump("final")
    t.kill()
    print(f"scrolled_into_history={scrolled} alive={alive} panic={panic}")
    if scrolled and alive and not panic:
        print("PASS: view cell drag-autoscroll pulls history into view")
        sys.exit(0)
    print("FAIL")
    sys.exit(1)


if __name__ == "__main__":
    main()
