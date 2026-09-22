#!/usr/bin/env python3
"""Per-pane scrollback, through the REAL client.

`tests/frame/mouse_scroll_per_pane.py` proves the server half by speaking the
wire protocol. It cannot prove the user's scenario, because the user does not
send `MouseScroll`: they turn a wheel, and a terminal emits an SGR report that
the CLIENT has to decode, map to a screen coordinate and forward. Only a real
pseudo-terminal exercises that, and only pyte sees the composited result the
user actually looks at.

The two reports, driven the way they were reported:

  1. the wheel over a pane that does NOT have focus scrolled the focused one;
  2. scrolling a pane, moving focus away and back lost its position.

Both are asserted on CONTENT. The wire's `scroll_offset` is the FOCUSED pane's
offset by definition, so it stays 0 while the wheel moves a non-focused pane and
could never have seen report 1 at all -- and from here it is not even visible.

The two panes carry DIFFERENT markers, each assembled by the shell from a loop
variable (`printf` plus `$i`), so a shell echoing the command line cannot
produce one. Every row scan uses `finditer`: the panes sit side by side and
share every screen row, so a first-match scan reads the left pane's text and
reports it as the right pane's.

Run from the repo root:
    python3 tests/pty/mouse_scroll_per_pane.py [-v]
"""
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pty_harness import Tui  # noqa: E402

# Keyed on the pid: two concurrent runs would otherwise share a socket and a
# state dir, and the corruption that follows looks exactly like the bug under
# test. Kept short, because a Unix socket path must stay under 108 characters.
RUNDIR = f"/tmp/rmx-pp{os.getpid()}"
COLS, ROWS = 120, 40
HIST = 300
VERBOSE = "-v" in sys.argv

ALT_H, ALT_L = b"\x1bh", b"\x1bl"
# SGR wheel-up. Button 64, coordinates 1-based, exactly what a terminal in SGR
# mouse mode sends and what `view_mouse_scroll_select.py` already relies on.
WHEEL_UP = 64
NOTCHES = 20


def wheel_up(t, col, row, n=NOTCHES):
    for _ in range(n):
        t.send(f"\x1b[<{WHEEL_UP};{col};{row}M".encode(), 0.12)
    t.pump(0.6)


def marks(t, prefix):
    """Every `PREFIX:<n>` on the whole screen, as a set of ints."""
    pat = re.compile(prefix + r":(\d+)")
    return {int(m.group(1)) for r in t.rows_text() for m in pat.finditer(r)}


def mark_columns(t, prefix):
    """The screen columns every `PREFIX:` match starts at (0-based).

    This is how the wheel finds its target. Hunting the frame for a divider
    glyph would be reading the rendering to decide where to aim at the
    rendering; the markers are produced by the shell INSIDE the pane, so their
    columns are the pane's real position whatever the chrome around it does.
    """
    pat = re.compile(prefix + r":\d+")
    return sorted({m.start() for r in t.rows_text() for m in pat.finditer(r)})


def aim(t, prefix):
    """A 1-based (col, row) inside the pane holding `prefix`."""
    cols = mark_columns(t, prefix)
    if not cols:
        raise AssertionError(f"no {prefix}: markers on screen to aim at")
    col0 = cols[len(cols) // 2]
    pat = re.compile(prefix + r":\d+")
    rows = [y for y, r in enumerate(t.rows_text()) if pat.search(r)]
    row0 = rows[len(rows) // 2]
    return col0 + 2, row0 + 1


def fill(t, mark):
    """Put >1 screen of history into the pane that currently has focus."""
    t.send(f"for i in $(seq 1 {HIST}); do printf '{mark}:%s\\n' $i; done\r"
           .encode(), 2.2)
    t.pump(1.2)


def show(t, label):
    if VERBOSE:
        t.dump(label)


def build(t):
    """Two side-by-side panes, LEFT holds AAA history, RIGHT holds BBB.

    `PaneSplitVertical` focuses the pane it creates, which is the RIGHT one, so
    the right pane is filled first and `Alt+h` reaches the left one.
    """
    t.send(b"clear\r", 0.5)
    t.prefix(b"pv", 1.2)
    fill(t, "BBB")
    t.send(ALT_H, 0.8)
    t.send(b"clear\r", 0.5)
    fill(t, "AAA")
    # Focus to the RIGHT pane, so the wheel below goes over a pane that does
    # not own input. Without this the case tests the ordinary focused wheel,
    # which never broke.
    t.send(ALT_L, 0.8)

    a_cols, b_cols = mark_columns(t, "AAA"), mark_columns(t, "BBB")
    assert a_cols and b_cols, (
        f"the split did not produce two panes with content: "
        f"AAA cols {a_cols}, BBB cols {b_cols}")
    assert max(a_cols) < min(b_cols), (
        f"AAA is not entirely left of BBB, so the panes are not side by side: "
        f"AAA {a_cols[:3]}.. BBB {b_cols[:3]}..")


def check(t, fails, ok, msg):
    if not ok:
        fails.append(msg)
        print(f"  FAIL {msg}")
    return ok


def main():
    t = Tui(RUNDIR, cols=COLS, rows=ROWS).start()
    fails = []
    try:
        build(t)
        show(t, "built")

        a_live, b_live = marks(t, "AAA"), marks(t, "BBB")
        if not (a_live and b_live):
            print(f"ABORT: a pane has no history (AAA {len(a_live)}, "
                  f"BBB {len(b_live)}) -- every assertion would be vacuous")
            t.dump("no history")
            t.kill()
            return 1

        # --- report 1: the wheel scrolls the pane under the cursor -----------
        col, row = aim(t, "AAA")
        print(f"wheel at (col={col}, row={row}) over the LEFT pane; "
              f"focus is on the RIGHT one")
        wheel_up(t, col, row)
        show(t, "after wheel")

        a_scrolled, b_after = marks(t, "AAA"), marks(t, "BBB")
        check(t, fails,
              a_scrolled and min(a_scrolled) < min(a_live),
              f"the wheel over the NON-FOCUSED pane did not scroll it: "
              f"AAA min {min(a_live)} -> "
              f"{min(a_scrolled) if a_scrolled else None}")
        check(t, fails, b_after == b_live,
              f"the wheel over the LEFT pane moved the focused RIGHT pane: "
              f"BBB {sorted(b_live)[:3]}.. -> {sorted(b_after)[:3]}..")
        print(f"  AAA {min(a_live)}..{max(a_live)} -> "
              f"{min(a_scrolled)}..{max(a_scrolled)}" if a_scrolled else "")

        # --- report 2: the position survives a focus round trip --------------
        #
        # Scrolled from the FOCUSED pane, deliberately, so this half stands on
        # its own. Report 1's wheel is the thing report 1 is about; if it is
        # broken, every "the pane did not move" check here is satisfied by a
        # pane that never moved, and the whole section passes while testing
        # nothing. Focusing the pane first makes the scroll work on any build,
        # so a failure here can only be the focus change losing it.
        t.send(ALT_H, 0.8)
        col, row = aim(t, "AAA")
        wheel_up(t, col, row)
        a_focused_scroll = marks(t, "AAA")
        if not (a_focused_scroll and min(a_focused_scroll) < min(a_live)):
            fails.append(
                "PRECONDITION: scrolling the FOCUSED left pane did not move it "
                f"(AAA min {min(a_live)} -> "
                f"{min(a_focused_scroll) if a_focused_scroll else None}); the "
                "focus round trip below cannot be tested and is SKIPPED rather "
                "than passed")
            print(f"  FAIL {fails[-1]}")
        else:
            t.send(ALT_L, 0.8)
            check(t, fails, marks(t, "AAA") == a_focused_scroll,
                  "focusing AWAY from the scrolled pane moved it: "
                  f"{sorted(a_focused_scroll)[:3]}.. -> "
                  f"{sorted(marks(t, 'AAA'))[:3]}..")
            t.send(ALT_H, 0.8)
            a_back = marks(t, "AAA")
            check(t, fails, a_back == a_focused_scroll,
                  "the pane's scroll was lost across a focus round trip: "
                  f"{sorted(a_focused_scroll)[:3]}.. -> {sorted(a_back)[:3]}..")
            print(f"  round trip: AAA {min(a_focused_scroll)}.."
                  f"{max(a_focused_scroll)} -> {min(a_back)}..{max(a_back)}")
        show(t, "after focus round trip")

        # --- typing in the scrolled pane returns it to the tail --------------
        # Focus is on the LEFT pane. A bare Enter is enough: the snap is on
        # `Input`, not on what the input says.
        t.send(b"\r", 1.2)
        a_typed = marks(t, "AAA")
        check(t, fails,
              a_typed and max(a_typed) >= max(a_live),
              f"typing into the scrolled pane did not return it to the tail: "
              f"max AAA {max(a_typed) if a_typed else None} < {max(a_live)}")
        show(t, "after typing")

        check(t, fails, t.alive(), "the client exited during the run")
        for which in ("client", "server"):
            log = t.log(which)
            check(t, fails, "panicked at" not in log,
                  f"the {which} log contains a panic:\n{log[-800:]}")
    finally:
        t.kill()

    if fails:
        print(f"\n{len(fails)} failure(s)")
        return 1
    print("\nPASS: per-pane scrollback works through the real client")
    return 0


if __name__ == "__main__":
    sys.exit(main())
