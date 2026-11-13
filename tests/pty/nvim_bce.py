"""A coloured Neovim window keeps its background past the end of each line.

The harness drives real Neovim in a pane through the real client binary and
reads the client's screen with pyte. The pane's TERM is xterm-256color, which
declares back-colour erase (`bce`), so Neovim paints a coloured window by
erasing with the window's background set and does not write the spaces. Each
erase must leave that background in the cells it clears, or the colour stops
where the text stops.

Neovim runs with `termguicolors`, the bundled `habamax` colorscheme (its Normal
and CursorLine backgrounds are both set and differ) and `cursorline`, on a file
of short lines. For every row of the window the harness compares the
background of a cell well past the line's text with the background of the
line's first text cell. They must be equal: Normal on every row, CursorLine on
the cursor's row.

What it covers:

  1  Neovim starts and its text cells carry a background that is not the
     terminal default (otherwise the comparison proves nothing)
  2  on the first screen, every row's background reaches past its text
  3  after j scrolls the window past its bottom and k back past its top, the
     rows the scroll brought in, and the moved cursorline, do too
  4  the client stays alive and neither log holds a panic

A key counts as drawn once the Neovim pane has changed and then reads the same
twice in a row. A pane that never settles is a failure. Every process the
harness starts gets its HOME and XDG directories, cache included, inside the
run dir. Skipped (exit 0, with a SKIP line) when `nvim` is not on PATH.

Run from the repo root: python3 tests/pty/nvim_bce.py [-v]
"""
import re
import shutil
import sys
import time

from pty_harness import Tui
from rundirs import run_dir  # noqa: E402

COLS, ROWS = 100, 30
POLL = 0.1
CHANGE_WAIT = 2.0
SETTLE_TIMEOUT = 5.0
VERBOSE = "-v" in sys.argv

fails = []


def check(cond, msg):
    if VERBOSE or not cond:
        print(("  PASS  " if cond else "  FAIL  ") + msg)
    if not cond:
        fails.append(msg)


def write_file(path):
    with open(path, "w") as f:
        f.write("".join(f"line {i}\n" for i in range(1, 201)))


# A window row: the pane's left border, the right-aligned number, the text.
ROW = re.compile(r"^│\s*(\d+) (line \d+)")


def pane_region(t):
    """The pane's interior as (text, background) per cell."""
    s = t.screen
    return [
        [(s.buffer[y][x].data, s.buffer[y][x].bg) for x in range(1, COLS - 1)]
        for y in range(1, ROWS - 2)
    ]


def press(t, key, label):
    """Send `key`, then wait until the pane has changed and reads the same twice."""
    before = pane_region(t)
    t.send(key, 0)
    deadline = time.time() + CHANGE_WAIT
    while time.time() < deadline and pane_region(t) == before:
        t.pump(POLL)
    last = pane_region(t)
    deadline = time.time() + SETTLE_TIMEOUT
    while True:
        t.pump(POLL)
        now = pane_region(t)
        if now == last:
            return
        if time.time() > deadline:
            check(False, f"{label}: the Neovim pane settled within {SETTLE_TIMEOUT}s")
            return
        last = now


def window_rows(t):
    """`(y, number, text_bg, tail_bg)` for each numbered row of the window.

    `text_bg` is the background of the line's first text cell and `tail_bg`
    that of a cell 20 columns past the end of its text.
    """
    s = t.screen
    out = []
    for y, r in enumerate(t.cell_rows()[: ROWS - 2]):
        m = ROW.match(r)
        if not m:
            continue
        text_x = m.start(2)
        tail_x = m.end(2) + 20
        if tail_x >= COLS - 1:
            continue
        out.append((y, int(m.group(1)), s.buffer[y][text_x].bg, s.buffer[y][tail_x].bg))
    return out


def verify(t, label):
    rows = window_rows(t)
    check(len(rows) >= 20, f"{label}: the window shows numbered rows ({len(rows)})")
    short = [(n, text_bg, tail_bg) for _, n, text_bg, tail_bg in rows if tail_bg != text_bg]
    check(not short, f"{label}: every row's background reaches past its text; short: {short[:3]}")
    return rows


def main():
    if shutil.which("nvim") is None:
        print("SKIP  nvim is not on PATH")
        return 0
    rundir = run_dir("rmxnvbce")
    t = Tui(rundir, cols=COLS, rows=ROWS, extra_env={"XDG_CACHE_HOME": f"{rundir}/cache"})
    t.start()
    try:
        path = f"{rundir}/home/f.txt"
        write_file(path)
        t.send(
            f"nvim --clean -n -i NONE '+set tgc cul nu so=0 nowrap' '+colorscheme habamax' {path}\r",
            2.5,
        )
        rows = verify(t, "2 start")
        text_bgs = {text_bg for _, _, text_bg, _ in rows}
        check(
            bool(text_bgs) and "default" not in text_bgs and len(text_bgs) == 2,
            f"1 the text carries the Normal and CursorLine backgrounds ({sorted(text_bgs)})",
        )
        for i in range(40):
            press(t, "j", f"3 j{i + 1}")
            verify(t, f"3 j{i + 1}")
        first = window_rows(t)
        check(bool(first) and first[0][1] > 1, f"3 j scrolled the window ({first[:1]})")
        for i in range(40):
            press(t, "k", f"3 k{i + 1}")
            verify(t, f"3 k{i + 1}")
        if fails:
            t.dump("at failure")
        check(t.alive(), "4 the client is alive")
    finally:
        t.kill()
    for which in ("client", "server"):
        check("panicked at" not in t.log(which), f"4 no panic in {which}.log")
    print(f"{'FAIL' if fails else 'PASS'}  nvim_bce: {len(fails)} failure(s)")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
