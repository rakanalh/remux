#!/usr/bin/env python3
"""Directional focus reaches a sidebar only when the server has no pane that way.

A pane that fills the whole pane area touches every edge, so a purely
geometric "at the edge" test sends every Alt+<dir> into the sidebar. Two
layouts give such a pane while other panes still exist in that direction:

  1  Monocle, left sidebar, focus on pane 2: Alt+h focuses pane 1, and only a
     second Alt+h (from pane 1, which has nothing to its left) enters the
     sidebar.
  2  Monocle, right sidebar, focus on pane 1: Alt+l focuses pane 2, then the
     sidebar.
  3  Bsp with pane 2 zoomed (Prefix f), left sidebar: Alt+h focuses pane 1
     (the zoom follows it), then the sidebar.

Which pane has focus is read from the shell inside it. Each pane printed a
marker the shell assembled (`printf 'P:%s'`), and Monocle and zoom paint only
the focused pane, so the visible marker names it. After each move a fresh
marker is typed: it must appear beside the expected pane's marker, or, with a
sidebar focused, nowhere at all.

Run from the repo root:
    python3 tests/pty/sidebar_full_area_nav.py [-v]
"""
import os
import shutil
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pty_harness import Tui  # noqa: E402

VERBOSE = "-v" in sys.argv
ALT_H, ALT_L, ALT_SPACE = b"\x1bh", b"\x1bl", b"\x1b "

PANEL = """
  [[sidebar.panel]]
  plugin = "placeholder"
  weight = 1
"""


def config(edge):
    return f"""
[[sidebar]]
edge = "{edge}"
size = 24
visible = true
{PANEL}"""


failures = []


def check(cond, msg, detail=""):
    print(f"  {'PASS' if cond else 'FAIL'} {msg}")
    if not cond:
        failures.append(msg)
        if detail:
            print(f"       {detail}")


def screen(t):
    return "\n".join(t.rows_text())


def sidebar_focused(t):
    return "focused" in screen(t)


def mark(t, word):
    t.send(f"printf '%s:%s\\n' K {word}\r".encode(), 0.8)


def two_panes(t):
    t.send(b"printf '%s:%s\\n' P one\r", 0.6)
    t.prefix(b"pn", 1.0)
    t.send(b"printf '%s:%s\\n' P two\r", 0.6)


def to_monocle(t):
    for _ in range(8):
        if "monocle" in t.rows_text()[-1].lower():
            return True
        t.send(ALT_SPACE, 0.8)
    return "monocle" in t.rows_text()[-1].lower()


def expect_pane(t, word, step, label):
    s = screen(t)
    other = "two" if word == "one" else "one"
    check(not sidebar_focused(t), f"{label}: the sidebar is not focused", s)
    check(f"P:{word}" in s and f"P:{other}" not in s,
          f"{label}: pane {word} is the one painted", s)
    mark(t, step)
    s = screen(t)
    check(f"K:{step}" in s and f"P:{word}" in s,
          f"{label}: a typed marker lands in pane {word}", s)


def expect_sidebar(t, step, label):
    check(sidebar_focused(t), f"{label}: the sidebar is focused", screen(t))
    mark(t, step)
    s = screen(t)
    check(f"K:{step}" not in s, f"{label}: a typed marker reaches no pane", s)


def run(name, edge, body):
    print(f"--- {name}")
    rundir = f"/tmp/rmx-sfa-{os.getpid()}"
    t = Tui(rundir, cols=120, rows=36, config=config(edge))
    try:
        t.start()
        t.pump(1.0)
        two_panes(t)
        body(t)
        if VERBOSE:
            t.dump(name)
        check(t.alive(), f"{name}: the client is alive")
        for which in ("server", "client"):
            check("panicked" not in t.log(which), f"{name}: no panic in {which}.log")
    finally:
        t.kill()
        shutil.rmtree(rundir, ignore_errors=True)


def monocle_left(t):
    check(to_monocle(t), "the tab is in Monocle", t.rows_text()[-1])
    expect_pane(t, "two", "start", "before")
    t.send(ALT_H, 1.0)
    expect_pane(t, "one", "first", "Alt+h from pane 2")
    t.send(ALT_H, 1.0)
    expect_sidebar(t, "second", "Alt+h from pane 1")


def monocle_right(t):
    check(to_monocle(t), "the tab is in Monocle", t.rows_text()[-1])
    t.send(ALT_H, 1.0)
    expect_pane(t, "one", "start", "before")
    t.send(ALT_L, 1.0)
    expect_pane(t, "two", "first", "Alt+l from pane 1")
    t.send(ALT_L, 1.0)
    expect_sidebar(t, "second", "Alt+l from pane 2")


def zoom_left(t):
    t.prefix(b"f", 1.0)
    expect_pane(t, "two", "start", "zoomed")
    t.send(ALT_H, 1.0)
    expect_pane(t, "one", "first", "Alt+h from zoomed pane 2")
    t.send(ALT_H, 1.0)
    expect_sidebar(t, "second", "Alt+h from zoomed pane 1")


def main():
    run("1 Monocle, left sidebar", "left", monocle_left)
    run("2 Monocle, right sidebar", "right", monocle_right)
    run("3 zoomed Bsp, left sidebar", "left", zoom_left)
    print(f"\n{'FAILED' if failures else 'OK'}: {len(failures)} failure(s)")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
