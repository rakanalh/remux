"""`Prefix t m h` / `Prefix t m l`: move the active tab one position left / right.

A real PTY, because the keys go through the client's which-key tree before the
server reorders anything, and only the rendered status bar shows the result.

What it covers:

  1  which-key under `Prefix t m` lists `h left` and `l right`
  2  on the LAST tab, `t m l` changes nothing (right edge). The old `m` leaf
     sent `TabMove 0`, which moved the active tab to the FIRST position
  3  `t m h` twice walks the tab to the front, and it stays active each step
  4  on the FIRST tab, `t m h` changes nothing (left edge)
  5  `t m l` twice walks it back to the end, one position per press
  6  the status bar order is the server's order: `t 1`/`t 2`/`t 3` jump to the
     tabs in the order the bar shows them
  7  the client stays alive and neither log holds a panic

The ACTIVE tab is read from the pane, not from the status bar's styling: each
tab's shell prints a marker ASSEMBLED by `printf` (`TAB:%s` typed, `TAB:alpha`
on screen), so the typed command line can never satisfy the search, and only
the displayed tab's marker is on screen.

Run from the repo root: python3 tests/pty/tab_move_keys.py [-v]
"""
import re
import sys

from pty_harness import Tui

RUNDIR = "/tmp/rmxtmk"
COLS, ROWS = 100, 24
ESC = b"\x1b"
NAMES = ["alpha", "bravo", "charlie"]
VERBOSE = "-v" in sys.argv

fails = []


def check(cond, msg):
    print(("  PASS  " if cond else "  FAIL  ") + msg)
    if not cond:
        fails.append(msg)


def tab_order(t):
    """The tab names on the status bar, left to right."""
    bar = t.rows_text()[-1]
    return [m.group(0) for m in re.finditer("|".join(NAMES), bar)]


def active_tab(t):
    """The tab whose pane is displayed, from the markers its shell printed."""
    pane = "\n".join(t.rows_text()[:-1])
    shown = {m.group(1) for m in re.finditer(r"TAB:(\w+)", pane)}
    return shown.pop() if len(shown) == 1 else f"ambiguous:{sorted(shown)}"


def name_tab(t, name):
    t.prefix(b"tr", 0.6)
    t.send(name + "\r", 0.8)
    t.send(f"printf 'TAB:%s\\n' {name}\r", 0.8)


def move(t, key):
    t.prefix(b"tm" + key, 1.0)


def expect(t, order, active, what):
    if VERBOSE:
        t.dump(what)
    got_order, got_active = tab_order(t), active_tab(t)
    check(got_order == order, f"{what}: order {got_order} == {order}")
    check(got_active == active, f"{what}: active {got_active!r} == {active!r}")


def main():
    t = Tui(RUNDIR, cols=COLS, rows=ROWS).start()
    try:
        t.pump(1.5)
        name_tab(t, NAMES[0])
        for name in NAMES[1:]:
            t.prefix(b"tn", 1.0)
            name_tab(t, name)
        expect(t, NAMES, "charlie", "setup")

        print("[1] which-key lists the sub-group")
        t.prefix(b"tm", 0.6)
        rows = t.rows_text()
        check(any(re.search(r"\bh left\b", r) for r in rows), "`Prefix t m` shows `h left`")
        check(any(re.search(r"\bl right\b", r) for r in rows), "`Prefix t m` shows `l right`")
        t.send(ESC, 0.5)
        expect(t, NAMES, "charlie", "Esc out of `t m`")

        print("[2] right edge: `t m l` on the last tab")
        move(t, b"l")
        expect(t, ["alpha", "bravo", "charlie"], "charlie", "`t m l` on the last tab")

        print("[3] `t m h` walks left, one position per press")
        move(t, b"h")
        expect(t, ["alpha", "charlie", "bravo"], "charlie", "first `t m h`")
        move(t, b"h")
        expect(t, ["charlie", "alpha", "bravo"], "charlie", "second `t m h`")

        print("[4] left edge: `t m h` on the first tab")
        move(t, b"h")
        expect(t, ["charlie", "alpha", "bravo"], "charlie", "`t m h` on the first tab")

        print("[5] `t m l` walks right, one position per press")
        move(t, b"l")
        expect(t, ["alpha", "charlie", "bravo"], "charlie", "first `t m l`")
        move(t, b"l")
        expect(t, ["alpha", "bravo", "charlie"], "charlie", "second `t m l`")
        move(t, b"h")
        expect(t, ["alpha", "charlie", "bravo"], "charlie", "`t m h` to set up [6]")

        print("[6] the bar's order is the server's order")
        order = tab_order(t)
        for i, name in enumerate(order):
            t.prefix(b"t" + str(i + 1).encode(), 1.0)
            check(active_tab(t) == name, f"`t {i + 1}` shows {name} ({active_tab(t)!r})")

        print("[7] health")
        check(t.alive(), "the client is alive")
    finally:
        t.kill()

    for which in ("server", "client"):
        check("panicked at" not in t.log(which), f"no panic in {which}.log")

    if fails:
        print(f"\nFAILED: {len(fails)} check(s)")
        sys.exit(1)
    print("\nOK")


if __name__ == "__main__":
    main()
