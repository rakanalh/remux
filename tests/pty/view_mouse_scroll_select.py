#!/usr/bin/env python3
"""Bug A (PTY): the mouse works inside a VIEW -- wheel scrolling that SURVIVES
output, and drag-selection that highlights and yanks.

Two independent defects hid behind one report ("mouse scroll and drag selection
in a view stopped working"; normal sessions were fine):

  1. Wheel scrolling moved the cell, but `build_pane_content` rendered ONE
     snapshot at offset 0 and the PTY-output fanout cloned it to every
     subscriber -- so the next byte of output in that pane yanked the cell back
     to the live tail. With an idle `/bin/sh` (as in `issue2_view_scroll.py`)
     nothing ever produced output, so the old test passed while real panes were
     unusable. Hence assertion 2 below: scroll must HOLD across output.
  2. There was no drag-selection path for views at all -- the Drag arm only did
     edge auto-scroll. `MouseClick`/`MouseDrag` are session-scoped and a client
     in a view is DETACHED, so the normal path could never have worked either.

Both are asserted against the RIGHT/LEFT cell specifically: a fix that targets
the focused cell, or the wrong cell, must fail rather than pass by luck.

Run from the repo root:  PYTHONPATH=tests/pty python3 tests/pty/view_mouse_scroll_select.py
"""
import re
import sys

from pty_harness import Tui

RUNDIR = "/tmp/rmxvms"
COLS, ROWS = 120, 40
# Grid splits a 120-col view into two ~60-col cells; these columns sit well
# inside each cell's content area (1-based, as SGR mouse reports are).
LEFT_COL, RIGHT_COL = 30, 90


def wheel(t, up, col, row, n=1):
    b = 64 if up else 65
    for _ in range(n):
        t.send(f"\x1b[<{b};{col};{row}M".encode(), 0.2)


def press(t, col, row):
    t.send(f"\x1b[<0;{col};{row}M".encode(), 0.3)


def drag(t, col, row, n=1):
    for _ in range(n):
        t.send(f"\x1b[<32;{col};{row}M".encode(), 0.2)


def release(t, col, row):
    t.send(f"\x1b[<0;{col};{row}m".encode(), 0.4)


def nums(t, prefix):
    """Every LINE number of `prefix` currently on screen."""
    out = set()
    for r in t.rows_text():
        for m in re.finditer(rf"{prefix}_(\d+)", r):
            out.add(int(m.group(1)))
    return out


def attrs(t):
    """{(y, x): (fg, bg)} for the whole screen except the status row."""
    buf = t.screen.buffer
    return {
        (y, x): (str(buf[y][x].fg), str(buf[y][x].bg))
        for y in range(t.screen.lines - 1)
        for x in range(t.screen.columns)
    }


def highlighted(t, baseline):
    """[(y, x, char)] of every cell whose colors CHANGED since `baseline`.

    Deliberately a before/after diff rather than a match against the selection's
    literal colors: the server paints a selection as bg=Indexed(7)/fg=Indexed(0)
    (`apply_selection_highlight`), but the client resolves indexed colors through
    the active theme palette before emitting them, so the concrete values on the
    wire are theme-dependent and hardcoding them would make this test fail on a
    palette change rather than on a real regression. Nothing else repaints while
    the drag is in flight, so "colors changed" is exactly "got selected".
    """
    out = []
    buf = t.screen.buffer
    for (y, x), before in baseline.items():
        c = buf[y][x]
        if (str(c.fg), str(c.bg)) != before:
            out.append((y, x, c.data))
    return sorted(out)


def highlight_text(t, baseline):
    """The highlighted cells joined in reading order, one line per screen row."""
    cells = highlighted(t, baseline)
    if not cells:
        return ""
    lines = {}
    for y, x, ch in cells:
        lines.setdefault(y, []).append((x, ch))
    return "\n".join(
        "".join(ch for _, ch in sorted(v)).rstrip() for _, v in sorted(lines.items())
    )


def build_view(t):
    """A 2-cell view over two panes holding numbered history."""
    t.send("clear\r", 0.4)
    t.send("for i in $(seq 1 200); do echo AAA_$i; done\r", 1.4)
    t.prefix(b"pv", 0.8)
    t.send("clear\r", 0.4)
    t.send("for i in $(seq 1 200); do echo BBB_$i; done\r", 1.4)
    t.pump(0.6)
    # Park both panes in a BACKGROUND tab. A pane visible in an attached
    # session's active tab is "session-visible" and its cell renders the
    # "● Active in session" placeholder -- there would be no content to scroll
    # or select, and every assertion below would pass or fail for the wrong
    # reason.
    t.send(b"\x1bt", 0.8)
    t.prefix(b"xm", 0.9)
    # The manager opens with its search bar focused; Tab hands focus to the tree.
    t.send(b"\t", 0.3)
    t.send("j", 0.2); t.send("j", 0.2); t.send("l", 0.5)   # expand Tab 1
    t.send("j", 0.2); t.send(" ", 0.3)                     # mark pane 1
    t.send("j", 0.2); t.send(" ", 0.3)                     # mark pane 2
    t.send("v", 0.3); t.send("a", 0.8)                     # AddToView picker
    t.send("\r", 1.6)                                      # "New view" -> enter
    t.pump(1.0)


def require_in_view(t):
    """Hard gate: abort unless a live VIEW with real cell CONTENT is on screen.

    Without this the whole test can pass while exercising a normal tab (where
    the mouse always worked), or a view whose cells show the "Active in session"
    placeholder (where there is nothing to scroll or select).
    """
    reasons = []
    if t.has("Session Manager"):
        reasons.append("session manager overlay still up")
    if t.has("Add Pane to View"):
        reasons.append("view picker overlay still up")
    status = t.rows_text()[-1]
    if "View 1" not in status:
        reasons.append(f"status bar is not a view status bar: {status.rstrip()!r}")
    if not t.has("/ Tab 1"):
        reasons.append("no view-cell title ('<session> / Tab 1') on any border")
    if t.has("Active in"):
        reasons.append("a cell shows the 'Active in session' placeholder, not content")
    if not nums(t, "AAA"):
        reasons.append("no AAA_* content (left cell is not showing its pane)")
    if not nums(t, "BBB"):
        reasons.append("no BBB_* content (right cell is not showing its pane)")
    if reasons:
        print("ABORT: not in a view with live cell content -- the assertions "
              "below would be meaningless:")
        for r in reasons:
            print(f"  - {r}")
        t.dump("not in a live view")
        t.kill()
        sys.exit(1)


def main():
    t = Tui(RUNDIR, cols=COLS, rows=ROWS).start()
    fails = []
    try:
        build_view(t)
        require_in_view(t)

        # --- 1. wheel scrolls the HOVERED cell, and only it -------------------
        a_live, b_live = nums(t, "AAA"), nums(t, "BBB")
        print(f"live      : AAA {min(a_live)}..{max(a_live)}  "
              f"BBB {min(b_live)}..{max(b_live)}")

        wheel(t, up=True, col=RIGHT_COL, row=20, n=8)
        t.pump(0.5)
        a_up, b_up = nums(t, "AAA"), nums(t, "BBB")
        print(f"wheel R   : AAA {min(a_up)}..{max(a_up)}  "
              f"BBB {min(b_up)}..{max(b_up)}")
        if not (b_up and min(b_up) < min(b_live)):
            fails.append("wheel over the RIGHT cell did not scroll it into history")
        if a_up != a_live:
            fails.append(f"wheel over the RIGHT cell moved the LEFT cell too "
                         f"({min(a_live)}..{max(a_live)} -> {min(a_up)}..{max(a_up)})")

        # --- 2. the scroll SURVIVES output in that pane (the regression) ------
        # Type into the focused cell's pane. Focus followed the marking order, so
        # click the right cell first to be sure the output lands in the pane that
        # is scrolled back.
        press(t, RIGHT_COL, 20)
        release(t, RIGHT_COL, 20)
        t.pump(0.5)
        before = nums(t, "BBB")
        t.send("printf 'ZZZ_ONE\\nZZZ_TWO\\n'\r", 1.2)
        t.pump(0.8)
        after = nums(t, "BBB")
        print(f"after out : BBB {min(after)}..{max(after)} (was {min(before)}..{max(before)})")
        if not after:
            fails.append("the right cell lost its content entirely after output")
        elif max(after) >= max(b_live):
            fails.append(
                f"output SNAPPED the scrolled cell back to the live tail "
                f"(max {max(before)} -> {max(after)}, live tail {max(b_live)})")

        # --- 3. wheel down returns to the live tail --------------------------
        wheel(t, up=False, col=RIGHT_COL, row=20, n=20)
        t.pump(0.6)
        if not t.has("ZZZ_TWO"):
            fails.append("wheel-down did not return the right cell to the live tail")

        # --- 4. drag-select inside the LEFT cell ------------------------------
        # Find a row showing an AAA_ line inside the left cell and select a run
        # of characters across it.
        target_y = None
        for y, r in enumerate(t.rows_text()[:-1]):
            if re.search(r"AAA_\d+", r[:COLS // 2]):
                target_y = y
        if target_y is None:
            fails.append("no AAA_ row found in the left cell to select")
        else:
            # 1-based mouse rows/cols; select columns 3..14 of that screen row.
            my = target_y + 1
            # Baseline AFTER the press, not before: the press also focuses the
            # left cell, and a focus change re-colors BOTH cells' borders. Those
            # recolors are not selection, so folding them into the diff would
            # drown the highlight (and trip the wrong-cell check).
            press(t, 3, my)
            t.pump(0.4)
            baseline = attrs(t)
            drag(t, 8, my)
            drag(t, 14, my, n=2)
            t.pump(0.4)
            hl = highlight_text(t, baseline)
            hl_cells = highlighted(t, baseline)
            print(f"highlight : {hl!r} ({len(hl_cells)} cells)")
            if not hl_cells:
                fails.append("drag inside a view cell produced NO selection highlight")
            else:
                # The highlight must be inside the LEFT cell only.
                stray = [c for c in hl_cells if c[1] >= COLS // 2]
                if stray:
                    fails.append(f"selection highlighted the wrong cell: "
                                 f"{len(stray)} cells at x>={COLS // 2}")
                if not re.search(r"AAA_\d+|_\d+", hl):
                    fails.append(f"highlight does not cover the cell's text: {hl!r}")

            before_yanks = len(t.yanks())
            release(t, 14, my)
            t.pump(0.8)
            ys = t.yanks()
            print(f"yanks     : {ys[before_yanks:]!r}")
            if len(ys) <= before_yanks:
                fails.append("release yanked nothing (no OSC 52 clipboard write)")
            else:
                yanked = ys[-1]
                if yanked.strip() != hl.strip():
                    fails.append(f"yanked text != highlighted text: "
                                 f"{yanked!r} vs {hl!r}")

        # --- 5. a wheel DURING a drag EXTENDS the selection -------------------
        # The drag anchor is held in absolute, eviction-stable coordinates, but
        # the highlight is viewport-relative. Scrolling without re-projecting it
        # would leave the grey block on the rows it happened to occupy, which now
        # show different text -- so the anchor's own line would fall OUT of the
        # highlight and the copy would not be what the user sees. Assert instead
        # that the selection grew: it still covers the anchor line AND reaches
        # into the lines the scroll revealed.
        target_y = None
        for y, r in enumerate(t.rows_text()[:-1]):
            if re.search(r"AAA_\d+", r[:COLS // 2]):
                target_y = y
        if target_y is not None and target_y > 20:
            # Anchor well above the bottom and scroll only one notch, so the
            # anchor stays ON SCREEN. (An anchor scrolled past the fold clamps to
            # the last content row -- correct, and the same as a normal pane, but
            # it would blur what this assertion is about.)
            my = target_y - 9
            press(t, 3, my)
            t.pump(0.4)
            baseline = attrs(t)
            drag(t, 20, my - 4, n=2)              # a small multi-row selection
            t.pump(0.4)
            pre = set(int(n) for n in re.findall(r"AAA_(\d+)", highlight_text(t, baseline)))
            wheel(t, up=True, col=LEFT_COL, row=my - 4, n=1)
            t.pump(0.6)
            post = set(int(n) for n in re.findall(r"AAA_(\d+)", highlight_text(t, baseline)))
            print(f"drag+wheel: before {min(pre) if pre else None}..{max(pre) if pre else None}"
                  f"  after {min(post) if post else None}..{max(post) if post else None}")
            if not pre:
                fails.append("the multi-row drag produced no highlight")
            elif not post:
                fails.append("wheel during a drag wiped the highlight")
            else:
                if max(pre) not in post:
                    fails.append(
                        f"wheel during a drag dropped the ANCHOR line from the "
                        f"highlight (anchor AAA_{max(pre)}, highlight now "
                        f"{min(post)}..{max(post)}) -- the selection did not follow "
                        f"the scroll")
                if min(post) >= min(pre):
                    fails.append(
                        f"wheel during a drag did not extend into the revealed "
                        f"history (min {min(pre)} -> {min(post)})")
            before_yanks = len(t.yanks())
            release(t, 20, my - 4)
            t.pump(0.8)
            if len(t.yanks()) <= before_yanks:
                fails.append("releasing after a wheel-extended drag yanked nothing")

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
