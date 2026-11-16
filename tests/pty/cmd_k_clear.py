#!/usr/bin/env python3
"""Cmd+K and Cmd+L, as CSI u from a configured macOS terminal, drive the clears.

A terminal configured as the install docs say sends Cmd+K as `ESC [107;9u` and
Cmd+L as `ESC [108;9u` (the codepoint, then modifier 9 = 1 + Super). Remux
pushes no keyboard enhancement flags, so this harness is also the proof that
the client decodes those bytes to k / l + Super without any.

Two side-by-side panes: LEFT holds `AAA:<n>` history, RIGHT holds `BBB:<n>` and
has focus. The prompt is `P42> `, which the shell builds from `$((6*7))`, and
the markers are printed by `printf`, so no typed command can satisfy a check.

  1  Cmd+K, sent just after the pyte screen is reset the way a terminal that
     clears its own screen would: the right pane has no `BBB:` on screen, the
     shell redrew `P42> ` at its top, and the wheel over it reveals no history
  2  the same Cmd+K repainted the screen remux owns: the left pane's `AAA:`
     lines and the status bar are back although nothing printed there
  3  the left pane is untouched: the wheel over it still reaches older `AAA:`
     history
  4  Cmd+L on a refilled right pane: the shell redrew `P42> ` at its top (the
     `^L` reached it) and the wheel still reaches `BBB:` history
  5  `Prefix p c`, the fallback for Terminal.app, clears as Cmd+K does
  6  the client is alive and neither log has a panic
  7  inside a View of both panes, Cmd+K clears the focused cell's pane through
     `ClearPaneScrollback`: its markers are gone and the shell redrew `P42> `,
     the other cell keeps its lines, and after leaving the view the wheel finds
     no history in the cleared pane and history in the other

Run from the repo root (pyte and pexpect are needed):
    uv run --no-config --no-project --with pyte --with pexpect \\
        python3 tests/pty/cmd_k_clear.py
"""
import os
import re
import sys

import pyte

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
from pty_harness import Tui, sm_compose_view  # noqa: E402
from rundirs import run_dir  # noqa: E402

RUNDIR = run_dir("rmx-cmdk")
COLS, ROWS = 120, 40
HIST = 300
PROMPT = "P42> "
CMD_K = b"\x1b[107;9u"
CMD_L = b"\x1b[108;9u"
ALT_H, ALT_L = b"\x1bh", b"\x1bl"
FAILURES = []


def check(name, cond, detail=""):
    print(f"  {'PASS' if cond else 'FAIL'}  {name}" + ("" if cond else f"\n        {detail}"))
    if not cond:
        FAILURES.append(name)


class ShadowTui(Tui):
    """A `Tui` that also feeds a control screen which is never reset, so a
    check on what the client SENT does not depend on what a reset erased."""

    def start(self):
        self.control = pyte.Screen(self.cols, self.rows)
        self.control_stream = pyte.ByteStream(self.control)
        return super().start()

    def pump(self, t=0.5):
        before = len(self.raw)
        super().pump(t)
        if len(self.raw) > before:
            self.control_stream.feed(bytes(self.raw[before:]))


def marks(t, prefix, screen=None):
    pat = re.compile(prefix + r":(\d+)")
    rows = (screen or t.screen).display
    return {int(m.group(1)) for r in rows for m in pat.finditer(r)}


def fill(t, mark):
    t.send(f"for i in $(seq 1 {HIST}); do printf '{mark}:%s\\n' $i; done\r".encode(), 2.2)
    t.pump(1.2)


def wheel(t, col, row, up, n=20):
    button = 64 if up else 65
    for _ in range(n):
        t.send(f"\x1b[<{button};{col};{row}M".encode(), 0.12)
    t.pump(0.6)


def pane_origin(t, prefix):
    """The 0-based (col, row) of the first content cell of the pane holding
    `prefix` markers, read from where the shell inside it printed them."""
    pat = re.compile(prefix + r":\d+")
    cols = sorted({m.start() for r in t.rows_text() for m in pat.finditer(r)})
    rows = [y for y, r in enumerate(t.rows_text()) if pat.search(r)]
    assert cols and rows, f"no {prefix}: markers on screen"
    return cols[0], rows[0]


def top_row(t, origin, screen=None):
    col, row = origin
    return (screen or t.screen).display[row][col:col + 40]


def check_cleared(t, label, origin):
    seen = marks(t, "BBB", t.control)
    check(f"{label}: no BBB: line is left on screen", not seen, sorted(seen)[:6])
    top = top_row(t, origin, t.control)
    check(f"{label}: the shell redrew the prompt at the pane's top",
          top.startswith(PROMPT), repr(top))
    aim = (origin[0] + 5, origin[1] + 5)
    wheel(t, *aim, up=True)
    seen = marks(t, "BBB")
    check(f"{label}: the wheel over the pane reveals no history", not seen, sorted(seen)[:6])
    wheel(t, *aim, up=False, n=40)


def two_filled_panes(t):
    """LEFT holds AAA history, RIGHT holds BBB and has focus."""
    t.send(b"clear\r", 0.5)
    t.prefix(b"pv", 1.2)
    fill(t, "BBB")
    t.send(ALT_H, 0.8)
    t.send(b"clear\r", 0.5)
    fill(t, "AAA")
    t.send(ALT_L, 0.8)


def history_under_wheel(t, prefix, origin):
    aim = (origin[0] + 5, origin[1] + 5)
    before = marks(t, prefix, t.control)
    wheel(t, *aim, up=True)
    seen = marks(t, prefix, t.control)
    wheel(t, *aim, up=False, n=40)
    return bool(seen) and (not before or min(seen) < min(before))


def test_view():
    t = ShadowTui(run_dir("rmx-cmdkv"), cols=COLS, rows=ROWS,
                  extra_env={"PS1": "P$((6*7))> "}).start()
    try:
        two_filled_panes(t)
        right, left = pane_origin(t, "BBB"), pane_origin(t, "AAA")
        sm_compose_view(t, panes=(0, 1), settle=2.0)
        t.pump(1.0)
        in_view = "View 1" in t.rows_text()[-1]
        check("7 setup: entered a view of both panes", in_view, t.rows_text()[-1])
        if not in_view:
            return
        before = {p: marks(t, p, t.control) for p in ("AAA", "BBB")}
        t.send(CMD_K, 1.5)
        after = {p: marks(t, p, t.control) for p in ("AAA", "BBB")}
        cleared = [p for p in ("AAA", "BBB") if before[p] and not after[p]]
        kept = [p for p in ("AAA", "BBB") if after[p] == before[p] and before[p]]
        check("7 view: Cmd+K cleared exactly one cell and left the other",
              len(cleared) == 1 and len(kept) == 1, (before, after))
        # The cells sit side by side, so the only prompt in the top rows is the
        # one the cleared cell's shell redrew; the other's is at its bottom.
        check("7 view: the shell redrew the prompt at the cleared cell's top",
              any(PROMPT in r for r in t.control.display[:4]), t.control.display[:4])
        t.prefix(b"wq", 1.5)
        check("7 view: left the view", "View 1" not in t.rows_text()[-1], t.rows_text()[-1])
        if len(cleared) == 1:
            origins = {"AAA": left, "BBB": right}
            gone, other = cleared[0], kept[0] if kept else None
            check(f"7 view: the cleared pane ({gone}) has no history after the view",
                  not history_under_wheel(t, gone, origins[gone]))
            if other:
                check(f"7 view: the other pane ({other}) keeps its history",
                      history_under_wheel(t, other, origins[other]))
        check("7 view: the client is alive", t.alive())
    finally:
        t.kill()
    for which in ("client", "server"):
        check(f"7 view: no panic in the {which} log", "panicked at" not in t.log(which))


def main():
    t = ShadowTui(RUNDIR, cols=COLS, rows=ROWS, extra_env={"PS1": "P$((6*7))> "}).start()
    try:
        two_filled_panes(t)
        right = pane_origin(t, "BBB")
        left = pane_origin(t, "AAA")
        assert left[0] < right[0], (left, right)
        left_live = marks(t, "AAA")
        status = t.rows_text()[-1].strip()
        check("setup: both panes hold history and the status bar is drawn",
              bool(left_live) and bool(marks(t, "BBB")) and bool(status))

        # Checked before any wheel: a scroll repaints the whole frame, which
        # would restore the reset screen whether or not Cmd+K did.
        t.screen.reset()
        t.send(CMD_K, 1.5)
        check("2 Cmd+K: the left pane's lines are repainted",
              marks(t, "AAA") == left_live, (sorted(marks(t, "AAA"))[:4], sorted(left_live)[:4]))
        check("2 Cmd+K: the status bar is repainted",
              t.rows_text()[-1].strip() == status, repr(t.rows_text()[-1].strip()[:60]))
        check_cleared(t, "1 Cmd+K", right)

        wheel(t, left[0] + 5, left[1] + 5, up=True)
        older = marks(t, "AAA")
        check("3 the left pane's history is untouched",
              bool(older) and min(older) < min(left_live), (sorted(older)[:4], min(left_live)))
        wheel(t, left[0] + 5, left[1] + 5, up=False, n=40)

        fill(t, "BBB")
        t.send(CMD_L, 1.5)
        check("4 Cmd+L: the shell redrew the prompt at the pane's top",
              top_row(t, right).startswith(PROMPT), repr(top_row(t, right)))
        wheel(t, right[0] + 5, right[1] + 5, up=True)
        check("4 Cmd+L: the wheel still reaches the history", bool(marks(t, "BBB")),
              t.rows_text()[right[1]:right[1] + 3])
        wheel(t, right[0] + 5, right[1] + 5, up=False, n=40)

        fill(t, "BBB")
        t.prefix(b"pc", 1.5)
        check_cleared(t, "5 Prefix p c", right)

        check("6 the client is alive", t.alive())
    finally:
        t.kill()
    for which in ("client", "server"):
        check(f"6 no panic in the {which} log", "panicked at" not in t.log(which))
    test_view()
    if FAILURES:
        print(f"\n{len(FAILURES)} FAILED: {FAILURES}\nlogs: {RUNDIR}/state/remux/")
        sys.exit(1)
    print("\nall passed")


if __name__ == "__main__":
    main()
