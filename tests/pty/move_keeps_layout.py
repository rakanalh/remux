#!/usr/bin/env python3
"""Alt+Shift+K moving a pane keeps an automatic layout when it swaps with a
neighbour, and ejects to custom only when the pane is at the edge.

The reported bug: a View of four panes in Grid, Alt+Shift+K on a pane, and the
status bar said `custom` although nothing was resized.

A real PTY, because the move key is decoded by the CLIENT and a View's cells are
painted by the client. Each pane is tracked by a marker its own shell printed
(`printf 'P%s\\n' A` typed, `PA` searched), so the typed command cannot fake
the position.

What it covers:

  1  a tab of four panes in grid: Alt+Shift+K on the bottom-right pane swaps
     it with the pane above, the two markers trade rows, and the status bar
     still reads `grid`
  2  Alt+Shift+K again on the same (now top) pane has no neighbour, so it
     relocates to the edge and the status bar reads `custom`
  3  a tab of two panes in bsp: Alt+Shift+H on the right pane swaps the two
     and keeps `bsp`; Alt+Shift+H again at the left edge ejects to `custom`
  4  a tab of three panes where `SetMaster` (Alt-m) put the focused pane in
     the master slot (centre), so Master holds that slot by pane id:
     Alt+Shift+H moves the master pane into the left stack, the pane it
     swapped with takes the master slot, and the status bar still reads
     `master`. A reorder alone would leave the master pane where it was. Then in
     columns, Alt+Shift+L swaps the leftmost pane with its right neighbour and
     the status bar still reads `columns`
  5  the same two checks in a View of four cells in grid, entered from a
     second tab. The second key also proves focus followed the moved cell:
     had focus stayed on the old index, that cell is in the bottom row and a
     second swap would keep `grid`
  6  the client is alive and neither log has a panic

Run from the repo root:
    python3 tests/pty/move_keeps_layout.py [-v]
"""
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pty_harness import PROTOCOL_VERSION, Tui, sm_compose_view  # noqa: E402
from rundirs import run_dir  # noqa: E402

RUN_TAB = run_dir("rmx-mvkl-tab")
RUN_VIEW = run_dir("rmx-mvkl-view")
RUN_BSP = run_dir("rmx-mvkl-bsp")
RUN_MASTER = run_dir("rmx-mvkl-master")
VERBOSE = "-v" in sys.argv

MOVE_UP = b"\x1bK"
MOVE_LEFT = b"\x1bH"
MOVE_RIGHT = b"\x1bL"
MARKERS = ("PA", "PB", "PC", "PD")

CONFIG = """
[keybindings.command]
"Alt-u" = "LayoutSet grid"
"Alt-y" = "LayoutSet bsp"
"Alt-o" = "LayoutSet columns"
"""

FAILURES = []


def check(name, ok, detail=""):
    if ok:
        print(f"  ok    {name}")
    else:
        print(f"  FAIL  {name}\n        {detail}")
        FAILURES.append(name)


def status_layout(t):
    return t.rows_text()[-1].split()[-1]


def positions(t, markers=MARKERS):
    """{marker: (row, col)} for markers seen exactly once, or None if any is
    missing or repeated. `finditer`, because two panes share each screen row."""
    found = {m: [] for m in markers}
    for y, row in enumerate(t.rows_text()):
        for m in markers:
            for hit in re.finditer(re.escape(m) + r"\b", row):
                found[m].append((y, hit.start()))
    if any(len(v) != 1 for v in found.values()):
        return None
    return {m: v[0] for m, v in found.items()}


def make_four_panes(t):
    t.send(b"printf 'P%s\\n' A\r", 0.5)
    for letter in "BCD":
        t.prefix(b"pv", 0.8)
        t.send(f"printf 'P%s\\n' {letter}\r".encode(), 0.5)


def bottom_and_top_halves(pos):
    mid = (min(r for r, _ in pos.values()) + max(r for r, _ in pos.values())) / 2
    top = {m for m, (r, _) in pos.items() if r < mid}
    return top, set(pos) - top


def column_of(pos, marker):
    return pos[marker][1]


def move_checks(t, label, focused_bottom):
    """`focused_bottom`: the marker of the focused pane, in the bottom row."""
    before = positions(t)
    check(f"{label}: four markers on screen", before is not None, "\n".join(t.rows_text()))
    if before is None:
        return
    top, bottom = bottom_and_top_halves(before)
    check(f"{label}: a 2x2 grid", len(top) == 2 and len(bottom) == 2, before)
    check(f"{label}: {focused_bottom} starts in the bottom row", focused_bottom in bottom,
          before)
    above = next((m for m in top
                  if abs(column_of(before, m) - column_of(before, focused_bottom)) < 5), None)
    check(f"{label}: a pane sits above {focused_bottom}", above is not None, before)
    if above is None:
        return

    t.send(MOVE_UP, 1.5)
    after = positions(t)
    if VERBOSE:
        t.dump(f"{label} after the first Alt+Shift+K")
    check(f"{label}: the layout stays grid after a neighbour swap",
          status_layout(t) == "grid", t.rows_text()[-1])
    check(f"{label}: four markers after the swap", after is not None, "\n".join(t.rows_text()))
    if after is None:
        return
    check(f"{label}: {focused_bottom} moved into the top row",
          after[focused_bottom][0] < after[above][0], after)
    check(f"{label}: {focused_bottom} and {above} traded places",
          after[focused_bottom] == before[above] and after[above] == before[focused_bottom],
          f"before {before}\nafter {after}")
    untouched = set(MARKERS) - {focused_bottom, above}
    check(f"{label}: the other two panes did not move",
          all(after[m] == before[m] for m in untouched), f"before {before}\nafter {after}")

    t.send(MOVE_UP, 1.5)
    if VERBOSE:
        t.dump(f"{label} after the second Alt+Shift+K")
    check(f"{label}: a move at the top edge ejects to custom",
          status_layout(t) == "custom", t.rows_text()[-1])
    check(f"{label}: client is alive", t.alive())


def check_logs(t, label):
    for which in ("client", "server"):
        body = t.log(which)
        check(f"{label}: no panic in {which}.log", "panicked at" not in body, body[-1500:])
        check(f"{label}: {which}.log is not empty", len(body) > 0)


def tab_section():
    t = Tui(RUN_TAB, config=CONFIG).start()
    try:
        make_four_panes(t)
        t.send(b"\x1bu", 1.2)
        check("tab: starts in grid", status_layout(t) == "grid", t.rows_text()[-1])
        move_checks(t, "tab", "PD")
    finally:
        t.kill()
    check_logs(t, "tab")


def bsp_section():
    t = Tui(RUN_BSP, config=CONFIG).start()
    pair = ("PA", "PB")
    try:
        t.send(b"printf 'P%s\\n' A\r", 0.5)
        t.prefix(b"pv", 0.8)
        t.send(b"printf 'P%s\\n' B\r", 0.8)
        t.send(b"\x1by", 1.2)
        check("bsp: starts in bsp", status_layout(t) == "bsp", t.rows_text()[-1])
        before = positions(t, pair)
        check("bsp: B starts right of A",
              before is not None and before["PB"][1] > before["PA"][1], before)
        t.send(MOVE_LEFT, 1.5)
        after = positions(t, pair)
        check("bsp: the layout stays bsp after a neighbour swap",
              status_layout(t) == "bsp", t.rows_text()[-1])
        check("bsp: A and B traded places",
              before is not None and after is not None
              and after["PB"] == before["PA"] and after["PA"] == before["PB"],
              f"before {before}\nafter {after}")
        t.send(MOVE_LEFT, 1.5)
        check("bsp: a move at the left edge ejects to custom",
              status_layout(t) == "custom", t.rows_text()[-1])
        check("bsp: client is alive", t.alive())
    finally:
        t.kill()
    check_logs(t, "bsp")


def traded(t, label, layout, markers, mover, key):
    """Send `key` and check that `mover` and exactly one other pane traded
    places while the rest stayed and the status bar kept `layout`. Returns
    the positions before and after, or None."""
    before = positions(t, markers)
    check(f"{label}: every marker on screen before", before is not None,
          "\n".join(t.rows_text()))
    t.send(key, 1.5)
    after = positions(t, markers)
    if VERBOSE:
        t.dump(f"{label} after the move")
    check(f"{label}: the layout stays {layout} after a neighbour swap",
          status_layout(t) == layout, t.rows_text()[-1])
    check(f"{label}: every marker on screen after", after is not None,
          "\n".join(t.rows_text()))
    if before is None or after is None:
        return None
    other = next((m for m in markers if m != mover and after[m] == before[mover]), None)
    check(f"{label}: {mover} traded places with one pane",
          other is not None and after[mover] == before[other],
          f"before {before}\nafter {after}")
    rest = set(markers) - {mover, other}
    check(f"{label}: the other panes did not move",
          all(after[m] == before[m] for m in rest), f"before {before}\nafter {after}")
    return before, after


def master_section():
    t = Tui(RUN_MASTER, config=CONFIG).start()
    trio = ("PA", "PB", "PC")
    try:
        t.send(b"printf 'P%s\\n' A\r", 0.5)
        for letter in "BC":
            t.prefix(b"pv", 0.8)
            t.send(f"printf 'P%s\\n' {letter}\r".encode(), 0.5)
        t.send(b"\x1bm", 1.2)
        check("master: SetMaster switches to master", status_layout(t) == "master",
              t.rows_text()[-1])
        pos = positions(t, trio)
        check("master: the focused PC is in the centre (master) column",
              pos is not None and pos["PC"][1] == sorted(c for _, c in pos.values())[1], pos)
        moved = traded(t, "master", "master", trio, "PC", MOVE_LEFT)
        if moved is not None:
            before, after = moved
            cols = sorted(c for _, c in after.values())
            check("master: PC is now in the left column",
                  after["PC"][1] == cols[0], f"before {before}\nafter {after}")
            check("master: the pane PC swapped with holds the master slot",
                  any(after[m][1] == cols[1] and before[m][1] == cols[0] for m in trio),
                  f"before {before}\nafter {after}")

        t.send(b"\x1bo", 1.2)
        check("columns: switched to columns", status_layout(t) == "columns",
              t.rows_text()[-1])
        pos = positions(t, trio)
        leftmost = min(pos, key=lambda m: pos[m][1]) if pos else None
        check("columns: the focused PC is leftmost", leftmost == "PC", pos)
        moved = traded(t, "columns", "columns", trio, "PC", MOVE_RIGHT)
        if moved is not None:
            before, after = moved
            check("columns: PC moved one column right",
                  sorted(c for _, c in after.values()).index(after["PC"][1]) == 1,
                  f"before {before}\nafter {after}")
        check("master: client is alive", t.alive())
    finally:
        t.kill()
    check_logs(t, "master")


def view_section():
    t = Tui(RUN_VIEW, config=CONFIG).start()
    try:
        make_four_panes(t)
        # A background tab, so the composed cells are not session-visible.
        t.send(b"\x1bt", 0.8)
        sm_compose_view(t, panes=(0, 1, 2, 3), settle=1.5)
        in_view = t.has("View 1")
        check("view: a view is displayed", in_view, "\n".join(t.rows_text()))
        if not in_view:
            return
        check("view: starts in grid", status_layout(t) == "grid", t.rows_text()[-1])
        pos = positions(t)
        if pos is None:
            check("view: four markers on screen", False, "\n".join(t.rows_text()))
            return
        # Focus starts on the first cell (top-left); Alt+j takes it to the
        # bottom-left cell, whatever marker that is.
        t.send(b"\x1bj", 0.8)
        _, bottom = bottom_and_top_halves(pos)
        bottom_left = min(bottom, key=lambda m: pos[m][1])
        move_checks(t, "view", bottom_left)
        t.prefix(b"wq", 1.2)
        check("view: left the view", not t.has("View 1"), "\n".join(t.rows_text()))
        check("view: client is alive after leaving", t.alive())
    finally:
        t.kill()
    check_logs(t, "view")


def main():
    print(f"move_keeps_layout (protocol {PROTOCOL_VERSION})")
    tab_section()
    bsp_section()
    master_section()
    view_section()
    if FAILURES:
        print(f"\n{len(FAILURES)} FAILED:")
        for f in FAILURES:
            print(f"  - {f}")
        sys.exit(1)
    print("\nall passed")


if __name__ == "__main__":
    main()
