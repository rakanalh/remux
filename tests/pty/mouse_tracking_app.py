#!/usr/bin/env python3
"""Mouse-aware applications through the REAL client (PTY harness).

The frame harness proves the server routes a mouse event to the application; it
cannot prove the CLIENT sends one. This drives the actual binary with the SGR
sequences a terminal emits and asserts the application on the other end printed
them -- the whole chain, in both places a pane can be driven from:

  A. a normal SESSION pane  (the user's "drag-select in neovim" case)
  B. a VIEW CELL            (the user's "scroll/drag in claude code" case)

The pane runs `cat -v`, which prints control bytes visibly ("^[[<64;10;5M"), so
the assertion is on what the application actually received rather than on remux's
own state. Part A also covers the click that never moved: the client has to send
the button-UP separately there, or a tracking app latches the button down.

Run from the repo root:
  PYTHONPATH=tests/pty python3 tests/pty/mouse_tracking_app.py
"""
import re
import sys

from pty_harness import Tui

RUNDIR = "/tmp/rmx_mta"
COLS, ROWS = 120, 40
LEFT_COL, RIGHT_COL = 30, 90
REPORT = re.compile(r"\^\[\[<(\d+);(\d+);(\d+)([Mm])")
# The alt screen + button-event tracking + SGR encoding, then an app that prints
# whatever it is fed.
TRACKER = "printf '\\033[?1002h\\033[?1006h\\033[?1049h'; cat -v\r"


def wheel(t, up, col, row):
    t.send(f"\x1b[<{64 if up else 65};{col};{row}M".encode(), 0.35)


def press(t, col, row):
    t.send(f"\x1b[<0;{col};{row}M".encode(), 0.35)


def drag(t, col, row):
    t.send(f"\x1b[<32;{col};{row}M".encode(), 0.35)


def release(t, col, row):
    t.send(f"\x1b[<0;{col};{row}m".encode(), 0.45)


def reports(t, x0=0, x1=COLS):
    """Mouse reports visible in screen columns [x0, x1), oldest first.

    Rows are joined without a separator so a report that wrapped at the right
    edge still reads as one sequence.
    """
    flat = "".join(r[x0:x1] for r in t.rows_text())
    return [(int(b), int(x), int(y), f) for b, x, y, f in REPORT.findall(flat)]


def shape(rs):
    """Just the button code + final byte of each report."""
    return [(b, f) for b, _, _, f in rs]


def build_view(t):
    """A 2-cell view: cell 1 = the tracking app, cell 2 = numbered history."""
    t.send("clear\r", 0.4)
    t.send(TRACKER, 1.0)
    t.prefix(b"pv", 0.9)
    t.send("clear\r", 0.4)
    t.send("for i in $(seq 1 200); do echo BBB_$i; done\r", 1.4)
    t.pump(0.6)
    # Park both panes in a BACKGROUND tab: a pane visible in the attached
    # session's active tab renders the "Active in session" placeholder instead
    # of content, and every assertion below would pass for the wrong reason.
    t.send(b"\x1bt", 0.8)
    t.prefix(b"xm", 0.9)
    for k in (b"j", b"j", b"l"):
        t.send(k, 0.3)
    t.send(b"j", 0.2); t.send(b" ", 0.3)
    t.send(b"j", 0.2); t.send(b" ", 0.3)
    t.send(b"v", 0.3); t.send(b"a", 0.8)
    t.send(b"\r", 1.6)
    t.pump(1.0)


def main():
    t = Tui(RUNDIR, cols=COLS, rows=ROWS).start()
    fails = []
    try:
        # ---- A. a normal session pane ------------------------------------
        t.send("clear\r", 0.4)
        t.send(TRACKER, 1.2)

        wheel(t, True, 20, 10)
        wheel(t, False, 25, 12)
        rs = reports(t)
        ok = (
            len(rs) >= 2
            and shape(rs)[:2] == [(64, "M"), (65, "M")]
            and (rs[1][1] - rs[0][1], rs[1][2] - rs[0][2]) == (5, 2)
        )
        if not ok:
            fails.append(f"A1 session wheel did not reach the app: {rs}")

        before = len(reports(t))
        press(t, 20, 15)
        drag(t, 23, 16)
        release(t, 23, 16)
        gesture = shape(reports(t)[before:])
        if gesture != [(0, "M"), (32, "M"), (0, "m")]:
            fails.append(f"A2 session press/motion/release did not reach the app: {gesture}")

        # A click that never moved still has to deliver its button-UP.
        before = len(reports(t))
        press(t, 30, 18)
        release(t, 30, 18)
        gesture = shape(reports(t)[before:])
        if gesture != [(0, "M"), (0, "m")]:
            fails.append(f"A3 a click without motion lost its release: {gesture}")

        # ---- B. the same pane aliased by a view cell ----------------------
        build_view(t)
        status = t.rows_text()[-1]
        if "View 1" not in status:
            t.dump("not in a view")
            print(f"ABORT: not in a view, status bar is {status.rstrip()!r}")
            t.kill()
            sys.exit(1)

        before_left = len(reports(t, 0, COLS // 2))
        before_right = len(reports(t, COLS // 2, COLS))
        wheel(t, True, LEFT_COL, 20)
        press(t, LEFT_COL, 15)
        drag(t, LEFT_COL + 3, 16)
        release(t, LEFT_COL + 3, 16)
        left = shape(reports(t, 0, COLS // 2)[before_left:])
        right = reports(t, COLS // 2, COLS)[before_right:]
        if left != [(64, "M"), (0, "M"), (32, "M"), (0, "m")]:
            fails.append(f"B1 view cell did not forward wheel+drag to the app: {left}")
        if right:
            fails.append(f"B2 the other cell received mouse reports: {right}")

        # The plain cell still scrolls remux's own scrollback (no regression).
        def bbb():
            return {int(m) for r in t.rows_text() for m in re.findall(r"BBB_(\d+)", r)}

        live = bbb()
        for _ in range(6):
            wheel(t, True, RIGHT_COL, 20)
        scrolled = bbb()
        if not (live and scrolled and min(scrolled) < min(live)):
            fails.append(f"B3 wheel stopped scrolling a plain cell: {min(live) if live else None} "
                         f"-> {min(scrolled) if scrolled else None}")

        if not t.alive():
            fails.append("client exited")
        for which in ("client", "server"):
            if "panic" in t.log(which).lower():
                fails.append(f"panic in the {which} log")
    finally:
        alive = t.alive()
        t.kill()

    for f in fails:
        print(f"FAIL: {f}")
    print(f"client alive at the end: {alive}")
    if fails:
        sys.exit(1)
    print("PASS: mouse-aware apps get their events in a session pane AND a view cell")
    sys.exit(0)


if __name__ == "__main__":
    main()
