#!/usr/bin/env python3
"""Bug B (PTY): Visual mode inside a VIEW must scope to the FOCUSED CELL.

User report: "in a view, there are 4 grid panes. I am focused on the right one.
When I press Ctrl+v for visual mode, the cursor jumps to the pane on the left
and the location of the cursor is weird."

Cause: Visual mode scoped itself to `focused_pane_rect`, which the SERVER sends
for the foreground session's focused pane. Entering a view detaches, so that
rect is a stale leftover describing a layout that is not on screen -- the copy
cursor landed in whatever cell happened to overlap it, at a meaningless offset,
and the yank read the wrong pane's text.

This reproduces the report exactly (4 cells, Grid, focus a RIGHT-hand one) and
asserts the three things that were wrong:
  1. the copy cursor appears INSIDE the focused cell,
  2. `k`/`h` move it, still inside that cell,
  3. `y` yanks that cell's pane text -- not a neighbour's.

Every cell carries a distinct token, so "yanked the wrong pane" cannot pass.

Run from the repo root:  PYTHONPATH=tests/pty python3 tests/pty/view_visual_focused_cell.py
"""
import re
import sys

from pty_harness import Tui

RUNDIR = "/tmp/rmxvvf"
COLS, ROWS = 120, 40
TOKENS = ["PANEAA", "PANEBB", "PANECC", "PANEDD"]


def press(t, col, row):
    t.send(f"\x1b[<0;{col};{row}M".encode(), 0.3)


def release(t, col, row):
    t.send(f"\x1b[<0;{col};{row}m".encode(), 0.4)


def attrs(t):
    """{(y, x): (fg, bg)} for the whole screen except the status row."""
    buf = t.screen.buffer
    return {
        (y, x): (str(buf[y][x].fg), str(buf[y][x].bg))
        for y in range(t.screen.lines - 1)
        for x in range(t.screen.columns)
    }


def changed(t, baseline):
    """[(y, x)] of every cell whose colors changed since `baseline`.

    The copy cursor and the selection are both drawn by inverting the cell's
    fg/bg (`render_visual_overlay`), so a colour diff locates them without
    hardcoding theme-dependent values.
    """
    buf = t.screen.buffer
    return sorted(
        (y, x)
        for (y, x), before in baseline.items()
        if (str(buf[y][x].fg), str(buf[y][x].bg)) != before
    )


def build_view(t):
    """A 4-cell view over 4 panes, each full of its own token."""
    for i, tok in enumerate(TOKENS):
        if i:
            # Alternate split direction so all four panes exist side by side.
            t.prefix(b"pv" if i == 1 else b"ps", 0.8)
        t.send("clear\r", 0.4)
        t.send(f"for i in $(seq 1 60); do echo {tok}_$i; done\r", 1.2)
    t.pump(0.8)
    # Background tab: a session-visible pane renders the "● Active in session"
    # placeholder in its cell instead of content, and there would be nothing to
    # put a cursor on or yank.
    t.send(b"\x1bt", 0.8)
    t.prefix(b"xm", 1.0)
    t.send("j", 0.2); t.send("j", 0.2); t.send("l", 0.5)   # expand Tab 1
    for _ in range(4):                                      # mark all 4 panes
        t.send("j", 0.2); t.send(" ", 0.3)
    t.send("v", 0.3); t.send("a", 0.8)                      # AddToView picker
    t.send("\r", 1.8)                                       # "New view" -> enter
    t.pump(1.2)


def token_boxes(t):
    """{token: (min_x, max_x, min_y, max_y)} of where each token is painted."""
    boxes = {}
    for y, row in enumerate(t.rows_text()[:-1]):
        for tok in TOKENS:
            for m in re.finditer(rf"{tok}_\d+", row):
                x0, x1 = m.start(), m.end() - 1
                if tok in boxes:
                    a, b, c, d = boxes[tok]
                    boxes[tok] = (min(a, x0), max(b, x1), min(c, y), max(d, y))
                else:
                    boxes[tok] = (x0, x1, y, y)
    return boxes


def cell_interior(t, box):
    """Interior rect (x0, x1, y0, y1) of the cell whose content box is `box`.

    Walks out from the token box to the cell's own drawn frame, so the region
    covers the WHOLE cell -- including rows the token never reached, like the
    shell prompt line the copy cursor legitimately opens on. Using the token box
    itself as the bounds would reject a perfectly correct cursor.
    """
    rows = t.rows_text()
    tx0, tx1, ty0, ty1 = box
    xl = max((x for x in range(tx0) if rows[ty0][x] == "\u2502"), default=-1)
    xr = min(
        (x for x in range(tx1 + 1, t.screen.columns) if rows[ty0][x] == "\u2502"),
        default=t.screen.columns,
    )
    yt = max((y for y in range(ty0) if rows[y][max(xl, 0)] == "\u256d"), default=-1)
    yb = min(
        (y for y in range(ty1 + 1, t.screen.lines - 1) if rows[y][max(xl, 0)] == "\u2570"),
        default=t.screen.lines - 1,
    )
    return (xl + 1, xr - 1, yt + 1, yb - 1)


def require_in_view(t):
    """Hard gate: a live 4-cell VIEW with real content in every cell."""
    reasons = []
    if t.has("Session Manager"):
        reasons.append("session manager overlay still up")
    if t.has("Add Pane to View"):
        reasons.append("view picker overlay still up")
    status = t.rows_text()[-1]
    if "View 1" not in status:
        reasons.append(f"status bar is not a view status bar: {status.rstrip()!r}")
    if "grid" not in status:
        reasons.append(f"view layout is not Grid: {status.rstrip()!r}")
    if not t.has("/ Tab 1"):
        reasons.append("no view-cell title ('<session> / Tab 1') on any border")
    if t.has("Active in"):
        reasons.append("a cell shows the 'Active in session' placeholder, not content")
    boxes = token_boxes(t)
    missing = [tok for tok in TOKENS if tok not in boxes]
    if missing:
        reasons.append(f"cells missing live content: {missing}")
    if reasons:
        print("ABORT: not in a live 4-cell view -- the assertions below would "
              "be meaningless:")
        for r in reasons:
            print(f"  - {r}")
        t.dump("not in a live view")
        t.kill()
        sys.exit(1)
    return boxes


def main():
    t = Tui(RUNDIR, cols=COLS, rows=ROWS).start()
    fails = []
    try:
        build_view(t)
        boxes = require_in_view(t)
        for tok in TOKENS:
            print(f"cell {tok}: x {boxes[tok][0]}..{boxes[tok][1]}  "
                  f"y {boxes[tok][2]}..{boxes[tok][3]}")

        # --- focus a RIGHT-hand cell (the user's exact repro) -----------------
        right = [tok for tok in TOKENS if boxes[tok][0] >= COLS // 2]
        if not right:
            print("ABORT: no right-hand cell in the grid")
            t.dump("layout")
            t.kill()
            sys.exit(1)
        target = right[0]
        tx0, tx1, ty0, ty1 = cell_interior(t, boxes[target])
        print(f"focused cell interior: x {tx0}..{tx1}  y {ty0}..{ty1}")
        click_x, click_y = boxes[target][0] + 2, boxes[target][2] + 1
        press(t, click_x + 1, click_y + 1)          # SGR mouse is 1-based
        release(t, click_x + 1, click_y + 1)
        t.pump(0.7)
        print(f"focused cell: {target} (clicked {click_x},{click_y})")

        def in_target(y, x):
            return tx0 <= x <= tx1 and ty0 <= y <= ty1

        def others_hit(cells):
            """Changed cells that fall inside a DIFFERENT cell's content box."""
            bad = []
            for y, x in cells:
                for tok in TOKENS:
                    if tok == target:
                        continue
                    ox0, ox1, oy0, oy1 = cell_interior(t, boxes[tok])
                    if ox0 <= x <= ox1 and oy0 <= y <= oy1:
                        bad.append((tok, y, x))
            return bad

        # --- 1. Ctrl-v puts the copy cursor in the FOCUSED cell ---------------
        baseline = attrs(t)
        # Visual mode is the prefix chord (Ctrl-a v = `EnterVisualMode`), which
        # is what "Ctrl+v" in the report refers to -- the same `ModeChanged(
        # Mode::Visual)` path either way.
        t.prefix(b"v", 0.8)
        t.pump(0.5)
        cur = changed(t, baseline)
        print(f"after Ctrl-v: {len(cur)} cell(s) changed -> {cur[:6]}")
        if not cur:
            fails.append("Ctrl-v drew no copy cursor at all")
        else:
            outside = [c for c in cur if not in_target(*c)]
            stray = others_hit(cur)
            if stray:
                fails.append(f"the copy cursor landed in ANOTHER cell: {stray[:4]}")
            elif outside:
                fails.append(f"the copy cursor landed outside the focused cell "
                             f"{target} (x {tx0}..{tx1}, y {ty0}..{ty1}): {outside[:4]}")

        # --- 2. k / h move the cursor, still inside the focused cell ----------
        baseline = attrs(t)
        t.send("k", 0.4)
        t.send("k", 0.4)
        t.send("h", 0.4)
        t.pump(0.5)
        moved = changed(t, baseline)
        print(f"after k k h : {len(moved)} cell(s) changed -> {moved[:6]}")
        if not moved:
            fails.append("k/k/h moved nothing -- the copy cursor is not live")
        else:
            stray = others_hit(moved)
            if stray:
                fails.append(f"cursor movement escaped into another cell: {stray[:4]}")

        # --- 3. select and yank the FOCUSED cell's text -----------------------
        # Pin the cursor to column 0 first (`h` clamps there) so the yank starts
        # at the beginning of the line and the expected text is exact -- the
        # cursor opens wherever the cell's shell cursor is, which is not column 0.
        for _ in range(15):
            t.send("h", 0.12)
        t.send("v", 0.4)                            # start a char selection
        for _ in range(len(target)):
            t.send("l", 0.2)                        # extend across the token
        t.pump(0.5)
        before = len(t.yanks())
        t.send("y", 1.0)
        t.pump(0.8)
        ys = t.yanks()
        print(f"yanks       : {ys[before:]!r}")
        if len(ys) <= before:
            fails.append("y yanked nothing (no OSC 52 clipboard write)")
        else:
            yanked = ys[-1]
            wrong = [tok for tok in TOKENS if tok != target and tok in yanked]
            if wrong:
                fails.append(f"y yanked the WRONG cell ({wrong[0]}): {yanked!r}")
            elif not yanked.startswith(target):
                fails.append(f"y did not yank the focused cell's text: "
                             f"{yanked!r} (expected it to start with {target})")

        alive = t.alive()
        if not alive:
            fails.append("the client exited")
        for which in ("client", "server"):
            if "panic" in t.log(which).lower():
                fails.append(f"panic in {which}.log")
        if fails:
            t.dump("final")
    finally:
        t.kill()

    for f in fails:
        print(f"FAIL: {f}")
    print("PASS" if not fails else "FAILED")
    sys.exit(1 if fails else 0)


if __name__ == "__main__":
    main()
