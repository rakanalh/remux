#!/usr/bin/env python3
"""`Prefix r` (`Redraw`) restores a screen the outer terminal cleared.

Cmd+K in Terminal.app and iTerm2 wipes the terminal's screen and tells the
program running in it nothing. The client's diff renderer still believes the
old screen is there, so afterwards it repaints only the cells that change and
the sidebar, the borders, the status bar and every quiet pane stay blank.

The clear is simulated by resetting the pyte screen out of band. A SECOND pyte
screen is fed every byte the client writes and is never reset, so it is what a
terminal that was never cleared would show. That makes the oracle exact: after
the redraw the two screens must match cell for cell, text and colour.

Phases 1 to 4 run `Redraw` from an `Alt-r` binding this config adds, not from
the default `Prefix r`. The prefix opens the which-key popup, and opening it
replays the whole front buffer as a side effect, so a `Prefix r` that did
nothing would still pass every screen check. Phase 5 drives the default key.

What it covers:

  1  normal tab (left sidebar, two tabs, a split): after the clear and some
     pane output the screen is mostly blank and the status bar, the pane
     borders and both panes' earlier text are gone, which proves the clear is
     real and the diff renderer does not recover on its own (the sidebar is
     checked in 4, where no frame arrives to repaint it)
  2  `Prefix r` brings back the sidebar, borders, status bar with both tabs,
     both panes' text, and the screen equals the never-cleared one
  3  the redraw re-sends the modes setup_terminal sets: mouse tracking,
     bracketed paste and the alternate screen
  4  the same inside a View, whose cells the client composites itself, with
     no output after the clear
  5  the default `Prefix r` runs the same redraw: the modes are re-sent and the
     which-key popup is closed again

Run from the repo root:
    python3 tests/pty/redraw_after_clear.py
"""
import os
import sys

import pyte

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
from pty_harness import Tui, sm_compose_view  # noqa: E402
from rundirs import run_dir  # noqa: E402

COLS, ROWS = 110, 32
SIDEBAR_W = 24

CONFIG = f"""
[[sidebar]]
edge = "left"
size = {SIDEBAR_W}
visible = true

  [[sidebar.panel]]
  plugin = "placeholder"
  weight = 1

[keybindings.command]
"Alt-r" = "Redraw"
"""

FAILURES = []


def check(name, cond, detail=""):
    if cond:
        print(f"  PASS  {name}")
    else:
        print(f"  FAIL  {name}\n        {detail}")
        FAILURES.append(name)


class ShadowTui(Tui):
    """A `Tui` that also feeds a control screen which is never cleared."""

    def start(self):
        self.control = pyte.Screen(self.cols, self.rows)
        self.control_stream = pyte.ByteStream(self.control)
        return super().start()

    def pump(self, t=0.5):
        before = len(self.raw)
        super().pump(t)
        if len(self.raw) > before:
            self.control_stream.feed(bytes(self.raw[before:]))


def cells(screen):
    return [[(c.data, c.fg, c.bg, c.bold, c.reverse)
             for c in (screen.buffer[y][x] for x in range(screen.columns))]
            for y in range(screen.lines)]


def mismatches(t):
    a, b = cells(t.screen), cells(t.control)
    return [(y, x) for y in range(t.rows) for x in range(t.cols) if a[y][x] != b[y][x]]


def non_blank(screen):
    return sum(1 for row in screen.display for ch in row if ch != " ")


def clear_like_cmd_k(t):
    t.screen.reset()


def check_cleared(t, label, needles):
    """The client did not repaint the cleared screen on its own."""
    rows = t.rows_text()
    control_ink = non_blank(t.control)
    ink = non_blank(t.screen)
    check(f"{label}: after the clear the screen stays mostly blank",
          ink < control_ink // 2, (ink, control_ink))
    for needle in needles:
        check(f"{label}: {needle!r} is gone before the redraw",
              not any(needle in r for r in rows), needle)


def check_restored(t, label, needles):
    rows = t.rows_text()
    for needle in needles:
        check(f"{label}: {needle!r} is back after the redraw",
              any(needle in r for r in rows), needle)
    bad = mismatches(t)
    check(f"{label}: the screen equals the never-cleared one",
          not bad, f"{len(bad)} cells differ, first {bad[:8]}")


def redraw(t):
    mark = len(t.raw)
    t.send(b"\x1br", 1.0)
    return bytes(t.raw[mark:])


MODES = (("mouse tracking", b"\x1b[?1000h"),
         ("SGR mouse", b"\x1b[?1006h"),
         ("bracketed paste", b"\x1b[?2004h"),
         ("alternate screen", b"\x1b[?1049h"))


def check_modes(label, emitted):
    for name, seq in MODES:
        check(f"{label}: the redraw re-sends {name}", seq in emitted, emitted[:120])


def test_normal_tab():
    label = "tab"
    t = ShadowTui(run_dir("rmx-redraw1"), cols=COLS, rows=ROWS, config=CONFIG).start()
    t.pump(1.0)
    t.prefix(b"tn", 1.2)
    t.send("printf 'LEFT%s\\n' PANE\r", 0.6)
    t.prefix(b"pv", 1.2)
    t.send("printf 'RIGHT%s\\n' PANE\r", 0.8)
    t.pump(0.5)

    needles = ["placeholder", "[NORMAL]", "Tab 1", "Tab 2", "LEFTPANE", "RIGHTPANE", "╭"]
    for needle in needles:
        check(f"{label}: setup shows {needle!r}", t.has(needle), needle)
    check(f"{label}: setup matches the control screen", not mismatches(t), mismatches(t)[:8])

    clear_like_cmd_k(t)
    t.send("printf 'AFTER%s\\n' CLEAR\r", 1.0)
    check(f"{label}: the new pane output did reach the cleared screen",
          t.has("AFTERCLEAR"), t.rows_text())
    # The sidebar is left out: a server frame repaints it on its own here.
    check_cleared(t, label, ["[NORMAL]", "Tab 2", "LEFTPANE", "RIGHTPANE", "╭ sh"])

    emitted = redraw(t)
    check_restored(t, label, needles + ["AFTERCLEAR"])

    # -- 3: the modes are sent again --------------------------------------
    check_modes(label, emitted)

    check(f"{label}: client alive", t.alive())
    for which in ("client", "server"):
        check(f"{label}: no panic in {which}.log", "panicked at" not in t.log(which))
    t.kill()


def test_view():
    label = "view"
    t = ShadowTui(run_dir("rmx-redraw2"), cols=COLS, rows=ROWS, config=CONFIG).start()
    t.pump(1.0)
    t.send("printf 'CELL%s\\n' ONE\r", 0.6)
    t.prefix(b"pv", 1.2)
    t.send("printf 'CELL%s\\n' TWO\r", 0.8)
    sm_compose_view(t, panes=(0, 1), settle=2.0)
    t.pump(1.0)
    in_view = "View 1" in t.rows_text()[-1] and t.has("/ Tab 1")
    check(f"{label}: entered the view", in_view, t.rows_text()[-1])
    if not in_view:
        t.dump("not in a view")
        t.kill()
        return
    check(f"{label}: setup matches the control screen", not mismatches(t), mismatches(t)[:8])

    # No output after the clear: a View recomposites every cell on a new
    # snapshot, so output would repaint the cells by itself and hide whether
    # the redraw did.
    clear_like_cmd_k(t)
    t.pump(1.0)
    check_cleared(t, label, ["placeholder", "View 1", "CELLONE", "CELLTWO"])

    redraw(t)
    check_restored(t, label, ["placeholder", "View 1", "CELLONE", "CELLTWO"])
    check(f"{label}: still in the view", "View 1" in t.rows_text()[-1], t.rows_text()[-1])
    check(f"{label}: client alive", t.alive())
    for which in ("client", "server"):
        check(f"{label}: no panic in {which}.log", "panicked at" not in t.log(which))
    t.kill()


def test_default_prefix_key():
    label = "prefix r"
    t = ShadowTui(run_dir("rmx-redraw3"), cols=COLS, rows=ROWS, config=CONFIG).start()
    t.pump(1.0)
    t.send("printf 'BASE%s\\n' LINE\r", 0.8)
    clean = [r for r in t.rows_text()]
    clear_like_cmd_k(t)
    mark = len(t.raw)
    t.prefix(b"r", 1.0)
    check_modes(label, bytes(t.raw[mark:]))
    check(f"{label}: the which-key popup is closed and the screen is as before",
          t.rows_text() == clean and not mismatches(t),
          [i for i, (a, b) in enumerate(zip(t.rows_text(), clean)) if a != b])
    check(f"{label}: client alive", t.alive())
    t.kill()


def main():
    print("== normal tab ==")
    test_normal_tab()
    print("== view ==")
    test_view()
    print("== default key ==")
    test_default_prefix_key()
    if FAILURES:
        print(f"\nFAILED: {len(FAILURES)}")
        for f in FAILURES:
            print(f"  - {f}")
        sys.exit(1)
    print("\nALL PASS")


if __name__ == "__main__":
    main()
