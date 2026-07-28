"""Issue #2 validation (PTY): the mouse wheel scrolls the FOCUSED view cell's
source pane through its scrollback, not the masked foreground session.

Build a 1-cell view over a pane holding >1 screen of numbered history. Wheel up
=> the cell shows earlier line numbers; wheel down => returns to the live tail.
"""
import re, sys
from pty_harness import Tui

RUNDIR = "/tmp/rmxfix/i2"


def wheel(t, up, col, row, n=1):
    # SGR mouse wheel: button 64=up, 65=down; press 'M'; 1-based coords.
    b = 64 if up else 65
    seq = f"\x1b[<{b};{col};{row}M".encode()
    for _ in range(n):
        t.send(seq, 0.25)


def visible_line_nums(t):
    nums = set()
    for r in t.rows_text():
        for m in re.finditer(r"LINE_(\d+)", r):
            nums.add(int(m.group(1)))
    return nums


def main():
    t = Tui(RUNDIR, cols=120, rows=40).start()
    t.send("clear\r", 0.3)
    # >1 screen of numbered history in this single pane.
    t.send("for i in $(seq 1 200); do echo LINE_$i; done\r", 1.2)
    t.pump(0.6)

    # Move this pane into a BACKGROUND tab so it is NOT "session-visible" (a
    # session-visible pane renders the "● Active in session" placeholder in its
    # cell instead of the live, cell-sized content this harness scrolls/asserts).
    t.send(b"\x1bt", 0.6)   # Alt+t: new empty tab

    # Build a 1-cell view over this pane.
    t.prefix(b"xm", 0.7)
    t.send("j", 0.2); t.send("j", 0.2); t.send("l", 0.4)  # expand Tab 1
    t.send("j", 0.2); t.send(" ", 0.3)                    # mark the pane
    t.send("v", 0.2); t.send("a", 0.5)                    # AddToView
    t.send("\r", 0.9)                                     # create + enter view

    if not t.has("View 1"):
        print("FAIL: not in view"); t.dump("state"); t.kill(); sys.exit(1)

    live = visible_line_nums(t)
    print("live visible max:", max(live) if live else None,
          "min:", min(live) if live else None)

    # Wheel up over the cell interior (center).
    wheel(t, up=True, col=40, row=20, n=8)
    t.pump(0.4)
    up_nums = visible_line_nums(t)
    print("after wheel-up min:", min(up_nums) if up_nums else None,
          "max:", max(up_nums) if up_nums else None)

    scrolled_back = bool(up_nums) and (min(up_nums) < min(live))
    print("scrolled into earlier history:", scrolled_back)

    # Wheel down back to the live tail.
    wheel(t, up=False, col=40, row=20, n=12)
    t.pump(0.4)
    down_nums = visible_line_nums(t)
    print("after wheel-down max:", max(down_nums) if down_nums else None)
    returned = bool(down_nums) and (max(down_nums) >= max(live))

    alive = t.alive()
    panic = "panic" in t.log("client").lower() or "panic" in t.log("server").lower()
    if not scrolled_back or not returned:
        t.dump("final")
    t.kill()

    print(f"scrolled_back={scrolled_back} returned={returned} alive={alive} panic={panic}")
    if scrolled_back and returned and alive and not panic:
        print("PASS")
        sys.exit(0)
    print("FAIL")
    sys.exit(1)


if __name__ == "__main__":
    main()
