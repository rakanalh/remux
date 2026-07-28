"""Issue #5 (PTY): Visual (copy) mode `space` then hold `k` must scroll into
scrollback history and grow the selection, not stop at the top.

Pane with >1 screen of numbered history; enter Visual mode, space to start the
selection, press k many times. Assert earlier line numbers scroll into view.
"""
import re, sys
from pty_harness import Tui

RUNDIR = "/tmp/rmxfix/i5"


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

    live = visible_line_nums(t)
    print("live min/max:", (min(live), max(live)) if live else None)

    # Enter Visual (copy) mode: prefix + v.
    t.prefix(b"v", 0.5)
    if not t.has("VISUAL"):
        print("FAIL: not in visual mode"); t.dump("state"); t.kill(); sys.exit(1)
    # Give RequestScrollbackInfo a moment to round-trip.
    t.pump(0.4)

    # Start selection, then hold k well past the top of the viewport. The first
    # ~visible_rows presses move the cursor to the top row; each one after that
    # scrolls one line into scrollback (deliberate per-key timing so each
    # ScrollDelta round-trips). This must keep going, not clamp at the top.
    t.send(" ", 0.3)  # space: start char selection
    for _ in range(90):
        t.send("k", 0.1)
    t.pump(0.6)

    after = visible_line_nums(t)
    print("after k*90 min/max:", (min(after), max(after)) if after else None)
    # ~90 presses - ~37 rows to reach the top => tens of lines of history.
    scrolled = bool(after) and (min(live) - min(after)) >= 20

    alive = t.alive()
    panic = "panic" in t.log("client").lower() or "panic" in t.log("server").lower()
    if not scrolled:
        t.dump("final")
    t.kill()
    print(f"scrolled_into_history={scrolled} alive={alive} panic={panic}")
    if scrolled and alive and not panic:
        print("PASS: visual-mode k scrolls into scrollback history")
        sys.exit(0)
    print("FAIL")
    sys.exit(1)


if __name__ == "__main__":
    main()
