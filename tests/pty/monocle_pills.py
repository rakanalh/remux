#!/usr/bin/env python3
"""The real client shows a capped tab chip ("pill") for EVERY pane in Monocle.

The client's config says `tab_style = "rounded"` and `pane_title = "{title}"`,
and four panes get long OSC 2 titles. A capped strip used to size every chip to
the longest name and drop each chip past the right edge, so at 120 columns
Monocle showed 3 pills. Two surfaces are checked, because they are composited
on different sides of the wire:

  1  a normal tab cycled to Monocle (`Alt+Space`): the SERVER composites the
     strip with the tab style the client sent;
  2  a View of the same four panes cycled to Monocle (`Prefix w Space`): the
     CLIENT composites the strip itself.

Pills are counted by their cap glyphs on the strip row.

Run from the repo root:
    python3 tests/pty/monocle_pills.py [-v]
"""
import os
import shutil
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pty_harness import Tui, sm_compose_view  # noqa: E402

VERBOSE = "-v" in sys.argv
LEFT_CAP, RIGHT_CAP = "", ""
TITLES = [
    "zsh: ~/Work/Personal/Remux",
    "claude: fix monocle pills",
    "nvim: compositor.rs",
    "sh: last pane",
]
CONFIG = """
[appearance]
pane_title = "{title}"

[appearance.theme]
tab_style = "rounded"
"""

failures = []


def check(cond, msg, detail=""):
    print(f"  {'PASS' if cond else 'FAIL'} {msg}")
    if not cond:
        failures.append(msg)
        if detail:
            print(f"       {detail}")


def set_title(t, title):
    t.send(f"printf '\\033]2;%s\\007' '{title}'\r", 0.4)


def strip_row(t):
    """The first row carrying a cap glyph, or "" when none does."""
    return next((r for r in t.rows_text() if LEFT_CAP in r or RIGHT_CAP in r), "")


def pills(row):
    return row.count(LEFT_CAP), row.count(RIGHT_CAP)


def main():
    rundir = f"/tmp/rmx-mpl-{os.getpid()}"
    t = Tui(rundir, cols=120, rows=40, config=CONFIG)
    try:
        t.start()
        set_title(t, TITLES[0])
        for title in TITLES[1:]:
            t.prefix(b"pn", 0.8)
            set_title(t, title)
        time.sleep(1.5)
        t.pump(0.5)

        print("--- 1 normal tab in Monocle")
        for _ in range(2):
            t.send(b"\x1b ", 0.8)
        row = strip_row(t)
        if VERBOSE:
            t.dump("monocle tab")
        status = t.rows_text()[-1]
        check("monocle" in status, "the tab is in Monocle", status)
        check(pills(row) == (4, 4), f"4 pills on the strip ({pills(row)})", repr(row))
        check("sh: last pane" in row, "the focused (last) pane's chip is drawn", repr(row))

        print("--- 2 a View of the four panes in Monocle")
        sm_compose_view(t, panes=(0, 1, 2, 3), settle=1.5)
        row = ""
        for _ in range(7):
            row = strip_row(t)
            if row:
                break
            t.prefix(b"w ", 0.8)
        if VERBOSE:
            t.dump("monocle view")
        status = t.rows_text()[-1]
        check("monocle" in status, "the View is in Monocle", status)
        check(row != "", "the View reached a capped Monocle strip")
        check(pills(row) == (4, 4), f"4 pills on the View's strip ({pills(row)})", repr(row))

        check(t.alive(), "the client is alive")
        for which in ("server", "client"):
            check("panicked" not in t.log(which), f"no panic in {which}.log")
    finally:
        t.kill()
        shutil.rmtree(rundir, ignore_errors=True)
    print(f"\n{'FAILED' if failures else 'OK'}: {len(failures)} failure(s)")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
