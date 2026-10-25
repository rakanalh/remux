#!/usr/bin/env python3
"""The right-click context menu through the REAL client (PTY harness).

Sends the SGR sequences a terminal emits for the right button
(`ESC[<2;x;yM` press, `ESC[<2;x;ym` release) and reads what the client paints.

  A. a shell pane            -> the menu opens; Split right splits the pane
  B. a mouse-tracking app    -> the app receives button 2; no menu
  C. that pane's border      -> the menu opens even though the app tracks
  D. a tab label             -> the tab menu opens
  E. the menu's own UX       -> Esc closes, an outside click closes and acts

Run from a SHORT path (a long worktree name changes the prompt):
  PYTHONPATH=tests/pty python3 tests/pty/context_menu.py
"""
import re
import sys

from pty_harness import Tui

RUNDIR = "/private/tmp/claude-501/cm/run1"
COLS, ROWS = 100, 30
REPORT = re.compile(r"\^\[\[<(\d+);(\d+);(\d+)([Mm])")
TRACKER = "printf '\\033[?1000h\\033[?1006h'; cat -v\r"


def rpress(t, col, row, wait=0.5):
    t.send(f"\x1b[<2;{col};{row}M".encode(), wait)


def rrelease(t, col, row, wait=0.5):
    t.send(f"\x1b[<2;{col};{row}m".encode(), wait)


def rclick(t, col, row):
    rpress(t, col, row, 0.3)
    rrelease(t, col, row, 0.6)


def lclick(t, col, row):
    t.send(f"\x1b[<0;{col};{row}M".encode(), 0.2)
    t.send(f"\x1b[<0;{col};{row}m".encode(), 0.7)


def where(t, needle):
    """(col, row), both 1-based as a mouse report counts them, of `needle`."""
    for y, line in enumerate(t.rows_text()):
        x = line.find(needle)
        if x >= 0:
            return x + 1, y + 1
    return None


def menu_up(t):
    return t.has("Split right") or (t.has("Rename") and t.has("Move left"))


def prompts(t, row):
    return t.rows_text()[row].count("$ ")


def main():
    t = Tui(RUNDIR, cols=COLS, rows=ROWS).start()
    fails = []
    try:
        t.send("clear\r", 0.5)

        # ---- A. a shell pane --------------------------------------------
        rclick(t, 20, 10)
        t.dump("A: after a right click on a shell pane")
        if not (t.has("Split right") and t.has("Split down") and t.has("Rename")):
            fails.append("A1 the pane menu did not open on a right click")
        if not t.has("Alt-z"):
            fails.append("A2 the zoom row does not show its live keybinding (Alt-z)")

        # Esc closes it.
        t.send("\x1b", 0.6)
        if menu_up(t):
            fails.append("A3 Esc did not close the menu")

        # j/k + Enter: open again, step to Split right, Enter runs it.
        rclick(t, 20, 10)
        before = prompts(t, 1)
        pos = where(t, "Split right")
        if pos is None:
            fails.append("A4 the menu did not reopen")
        else:
            # Hover highlights, then a left click on the row runs it.
            t.send(f"\x1b[<35;{pos[0] + 1};{pos[1]}M".encode(), 0.4)
            lclick(t, pos[0] + 1, pos[1])
            t.dump("A: after choosing Split right")
            if menu_up(t):
                fails.append("A5 choosing a row did not close the menu")
            if prompts(t, 1) < 2:
                fails.append(f"A6 Split right did not split ({before} -> {prompts(t, 1)} prompts)")

        # ---- B. a mouse-tracking app -------------------------------------
        t.send("clear\r", 0.4)
        t.send(TRACKER, 1.0)
        rclick(t, 70, 12)
        flat = "".join(t.rows_text())
        got = [(int(b), f) for b, _, _, f in REPORT.findall(flat)]
        t.dump("B: right click on a tracking app")
        if (2, "M") not in got or (2, "m") not in got:
            fails.append(f"B1 the app did not receive button 2 press and release: {got}")
        if menu_up(t):
            fails.append("B2 the menu opened over a mouse-tracking app")

        # ---- C. the border of that same pane -----------------------------
        # The focused pane is the right half; its top border is the first row.
        rclick(t, 70, 1)
        t.dump("C: right click on the border of the tracking pane")
        if not menu_up(t):
            fails.append("C1 the border did not open the menu while the app tracks the mouse")
        t.send("\x1b", 0.5)

        # ---- D. a tab label ----------------------------------------------
        t.send(b"\x1bt", 0.8)  # Alt-t: new tab, so Move left/right have meaning
        tab = where(t, "Tab 1")
        if tab:
            rclick(t, tab[0] + 1, tab[1])
            t.dump("D: right click on a tab label")
            if not (t.has("Move left") and t.has("Move right")):
                fails.append("D1 the tab menu did not open on a right click on the tab bar")
            t.send("\x1b", 0.5)
        else:
            fails.append("D0 found no tab label to click")

        # ---- E. an outside click closes the menu and acts ------------------
        rclick(t, 20, 10)
        if not menu_up(t):
            fails.append("E0 the menu did not open")
        else:
            lclick(t, 2, ROWS - 3)
            if menu_up(t):
                fails.append("E1 a click outside did not close the menu")
    finally:
        t.kill()
    if fails:
        print("FAIL")
        for f in fails:
            print(" -", f)
        sys.exit(1)
    print("PASS context_menu")


if __name__ == "__main__":
    main()
