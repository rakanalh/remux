#!/usr/bin/env python3
"""Arrow keys as an alternative to h/j/k/l, through the real client.

The keys are decoded by the CLIENT (crossterm), looked up in its shortcut map
or its prefix tree, and only then sent to the server as commands, so only a
real PTY proves them. Every check sends the bytes an xterm-style terminal
sends, and reads the result from the shell inside a pane: a probe prints the
pane's own name and its `stty size`, and the probe's column and row on screen
say where that pane is. The printed marker is assembled by the shell
(`printf 'W%s7:%s'` typed, `WL7:` searched), so the typed line cannot fake it.

Each check first puts the client in a known state through the h/j/k/l keys,
and verifies that state with a probe, so a failed arrow cannot make the next
check pass.

  1  the Pane which-key page shows `h/<-` on one row, and the root page shows
     `Alt-h/<-` in its Alt section
  2  Alt+Left / Alt+Right (`CSI 1;3D` / `CSI 1;3C`) move focus between two
     side-by-side panes; Alt+Up / Alt+Down (`CSI 1;3A` / `CSI 1;3B`) between
     two stacked ones
  3  Prefix p Left / Right / Up / Down (`CSI D`, and the SS3 form `ESC O C`)
     move focus
  4  Prefix p R, then Right Right in the sticky Resize menu, changes the
     focused pane's width exactly as `l l` does; Left Left as `h h` does;
     Down Down and Up Up change a stacked pane's height as `j j` / `k k` do
  5  Alt+Shift+Right (`CSI 1;4C`) and Prefix p Shift+Right (`CSI 1;2C`) move
     the pane to the right half
  6  the client is alive and neither log has a panic

Run from the repo root:
    python3 tests/pty/arrow_keys.py [-v]
Set REMUX_BIN to run it against another build.
"""
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pty_harness import BIN, Tui  # noqa: E402
from rundirs import run_dir  # noqa: E402

VERBOSE = "-v" in sys.argv
COLS, ROWS = 140, 40

ESC = b"\x1b"
ALT_LEFT, ALT_RIGHT, ALT_UP, ALT_DOWN = (b"\x1b[1;3" + c for c in (b"D", b"C", b"A", b"B"))
ALT_SHIFT_RIGHT = b"\x1b[1;4C"
LEFT, RIGHT, UP, DOWN = (b"\x1b[" + c for c in (b"D", b"C", b"A", b"B"))
SS3_RIGHT = b"\x1bOC"
SHIFT_RIGHT = b"\x1b[1;2C"

# Bsp everywhere, so a split's geometry does not depend on the default layout.
CONFIG = """
[appearance]
default_layout = "bsp"
"""

FAILS = []
SEQ = [0]


def check(name, ok, detail=""):
    print(("PASS " if ok else "FAIL ") + name + (f": {detail}" if detail and (VERBOSE or not ok) else ""))
    if not ok:
        FAILS.append(name)


def probe(t):
    """(name, column, row, rows, cols) of the focused pane, from its own shell."""
    SEQ[0] += 1
    n = SEQ[0]
    t.send(f"clear; printf 'W%s{n}:%s\\n' \"$P\" \"$(stty size | tr ' ' x)\"\r", 1.0)
    hits = []
    for y, row in enumerate(t.cell_rows()):
        for m in re.finditer(rf"W([A-Z]){n}:(\d+)x(\d+)", row):
            hits.append((m.group(1), m.start(), y, int(m.group(2)), int(m.group(3))))
    if len(hits) != 1:
        if VERBOSE:
            t.dump(f"probe {n}: {hits}")
        return None
    return hits[0]


def focused(t):
    hit = probe(t)
    return hit[0] if hit else None


def prefix(t, *keys, settle=0.6):
    """Ctrl-a, then each key as its own write, as a person types them."""
    t.prefix(keys[0], 0.25)
    for key in keys[1:]:
        t.send(key, 0.25)
    t.pump(settle)


def focus_by_letter(t, letter, want):
    prefix(t, b"p", letter.encode())
    got = focused(t)
    if got != want:
        raise SystemExit(f"setup: Prefix p {letter} should focus {want}, focused {got}")


def resize_delta(t, keys, axis):
    """How far `keys` in the sticky Resize menu move the focused pane's size."""
    before = probe(t)
    prefix(t, b"p", b"R", *keys)
    t.send(ESC, 0.5)
    after = probe(t)
    if before is None or after is None:
        return None
    index = 4 if axis == "cols" else 3
    return after[index] - before[index]


def main():
    print(f"binary: {BIN}")
    t = Tui(run_dir("rmx-arrows"), cols=COLS, rows=ROWS, config=CONFIG)
    try:
        t.start()
        t.pump(1.0)
        t.send("P=L\r", 0.5)
        prefix(t, b"p", b"v", settle=1.2)
        t.send("P=R\r", 0.5)
        if focused(t) != "R":
            raise SystemExit("setup: the split did not focus the new pane")

        # 1. which-key shows the arrows folded into their letters.
        t.prefix(b"p", 0.8)
        check("pane which-key shows h/← on one row", t.has("h/←"))
        check("pane which-key shows H/⇧← on one row", t.has("H/⇧←"))
        t.send(ESC, 0.4)
        t.prefix(b"", 0.8)
        check("root which-key shows Alt-h/←", t.has("Alt-h/←"))
        t.send(ESC, 0.4)

        # 2. Alt+Left / Alt+Right between side-by-side panes.
        focus_by_letter(t, "l", "R")
        t.send(ALT_LEFT, 0.6)
        check("Alt+Left (CSI 1;3D) focuses the left pane", focused(t) == "L")
        focus_by_letter(t, "h", "L")
        t.send(ALT_RIGHT, 0.6)
        check("Alt+Right (CSI 1;3C) focuses the right pane", focused(t) == "R")

        # 3. Prefix p Left / Right.
        focus_by_letter(t, "l", "R")
        prefix(t, b"p", LEFT)
        check("Prefix p Left (CSI D) focuses the left pane", focused(t) == "L")
        focus_by_letter(t, "h", "L")
        prefix(t, b"p", SS3_RIGHT)
        check("Prefix p Right (SS3 ESC O C) focuses the right pane", focused(t) == "R")

        # 4. Sticky resize on the width, compared with the letters.
        focus_by_letter(t, "h", "L")
        start = probe(t)
        by_l = resize_delta(t, [b"l", b"l"], "cols")
        resize_delta(t, [b"h", b"h"], "cols")
        check("setup: h h undoes l l", probe(t)[4] == start[4], f"{start} -> {probe(t)}")
        by_right = resize_delta(t, [RIGHT, RIGHT], "cols")
        check("Prefix p R Right Right resizes as l l does",
              by_right is not None and by_right == by_l and by_l != 0,
              f"arrows {by_right}, letters {by_l}")
        by_left = resize_delta(t, [LEFT, LEFT], "cols")
        check("Prefix p R Left Left resizes as h h does",
              by_left is not None and by_l is not None and by_left == -by_l,
              f"arrows {by_left}, letters {by_l}")

        # 5. Moving the pane with Shift+arrows.
        focus_by_letter(t, "h", "L")
        left_x = probe(t)[1]
        t.send(ALT_SHIFT_RIGHT, 0.8)
        hit = probe(t)
        check("Alt+Shift+Right (CSI 1;4C) moves the pane to the right half",
              hit is not None and hit[0] == "L" and hit[1] > COLS // 2 > left_x,
              f"{hit}, was at column {left_x}")
        prefix(t, b"p", b"H", settle=0.8)
        hit = probe(t)
        if hit is None or hit[0] != "L" or hit[1] > COLS // 2:
            raise SystemExit(f"setup: Prefix p H did not move L back to the left: {hit}")
        prefix(t, b"p", SHIFT_RIGHT, settle=0.8)
        hit = probe(t)
        check("Prefix p Shift+Right (CSI 1;2C) moves the pane to the right half",
              hit is not None and hit[0] == "L" and hit[1] > COLS // 2, f"{hit}")

        # Up / Down in a new tab with two stacked panes.
        prefix(t, b"t", b"n", settle=1.2)
        t.send("P=T\r", 0.5)
        prefix(t, b"p", b"s", settle=1.2)
        t.send("P=B\r", 0.5)
        if focused(t) != "B":
            raise SystemExit("setup: the horizontal split did not focus the new pane")

        focus_by_letter(t, "j", "B")
        t.send(ALT_UP, 0.6)
        check("Alt+Up (CSI 1;3A) focuses the upper pane", focused(t) == "T")
        focus_by_letter(t, "k", "T")
        t.send(ALT_DOWN, 0.6)
        check("Alt+Down (CSI 1;3B) focuses the lower pane", focused(t) == "B")
        focus_by_letter(t, "j", "B")
        prefix(t, b"p", UP)
        check("Prefix p Up (CSI A) focuses the upper pane", focused(t) == "T")
        focus_by_letter(t, "k", "T")
        prefix(t, b"p", DOWN)
        check("Prefix p Down (CSI B) focuses the lower pane", focused(t) == "B")

        focus_by_letter(t, "k", "T")
        start = probe(t)
        by_j = resize_delta(t, [b"j", b"j"], "rows")
        resize_delta(t, [b"k", b"k"], "rows")
        check("setup: k k undoes j j", probe(t)[3] == start[3])
        by_down = resize_delta(t, [DOWN, DOWN], "rows")
        check("Prefix p R Down Down resizes as j j does",
              by_down is not None and by_down == by_j and by_j != 0,
              f"arrows {by_down}, letters {by_j}")
        by_up = resize_delta(t, [UP, UP], "rows")
        check("Prefix p R Up Up resizes as k k does",
              by_up is not None and by_j is not None and by_up == -by_j,
              f"arrows {by_up}, letters {by_j}")

        # 6. Alive, and no panic in either log.
        check("client alive", t.alive())
    finally:
        t.kill()
    for which in ("client", "server"):
        log = t.log(which)
        check(f"{which}.log has no panic", "panicked" not in log)
        check(f"{which}.log exists", bool(log))

    print()
    if FAILS:
        print(f"FAILED: {len(FAILS)}")
        for name in FAILS:
            print("  - " + name)
        return 1
    print("ALL PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
