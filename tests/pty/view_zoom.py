#!/usr/bin/env python3
"""A View's zoom through the real client, with `[zoom]` set to each mode.

Two cells side by side, aliasing panes of a background tab so that neither shows
the "Active in session" placeholder. `[zoom] width = 60, height = 50`, so the
box cannot coincide with a cell. A cell's size is always read from the `stty
size` its own shell prints, and the zoom kind is told apart by whether the OTHER
cell's text is still on screen.

For each `[zoom] mode`:

  1  Alt-z and Prefix z follow the mode. `PaneToggleZoomFloat` always floats and
     `PaneToggleZoomFullscreen` always fills. A float is sized to the box, in the
     place a tab's box goes, with the keyboard and the cursor inside it.
  2  Either command releases a zoom of either kind, and the pane is back at its
     cell's size.
  3  Alt+l ends a float and steps focus, and Alt+h at the edge ends it and stays.
     A fullscreen zoom survives Alt+l and follows focus.
  4  A click on the other cell ends a float and moves focus. A click on the empty
     slot or the status row ends it and keeps focus. A click inside the box,
     over the other cell's tile, keeps it.
  5  In Monocle a float keeps the title strip, and a fullscreen zoom is refused.
  6  Under the tmux border style a float still has the box's frame and size.

A run named `extras` covers what ends or reshapes a float from outside: a
terminal resize, a smaller second viewer, a press on the empty slot reaching a
mouse-tracking application, the floated pane exiting, and the View being deleted
by another client.

A last run with a left sidebar: the box is a percentage of the content rect and
sits inside it, a press on the sidebar ends a float, and Alt+h at the edge ends a
float without the key going on to the sidebar.

Run from the repo root: python3 tests/pty/view_zoom.py [float|fullscreen|sidebar|extras]
"""
import os
import re
import shutil
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pty_harness import Tui, sm_compose_view  # noqa: E402
from harness import Client as WireClient, name_of  # noqa: E402

COLS, ROWS = 120, 40
RUNDIR = "/tmp/rmx-vzoom"
ALT_Z, ALT_U, ALT_I, ALT_Y = b"\x1bz", b"\x1bu", b"\x1bi", b"\x1by"
ALT_H, ALT_L, ALT_SPACE = b"\x1bh", b"\x1bl", b"\x1b "
TRACKER = b"printf '\\033[?1002h\\033[?1006h\\033[?1049h'; cat -v\r"
UNTRACK = b"printf '\\033[?1002l\\033[?1006l\\033[?1049l'\r"
PRESS_REPORT = re.compile(r"\^\[\[<0;\d+;\d+M")
CONFIG = """
[zoom]
mode = "{mode}"
width = 60
height = 50

[keybindings.command]
"Alt-u" = "PaneToggleZoomFloat"
"Alt-i" = "PaneToggleZoomFullscreen"
"Alt-y" = "ToggleStyle"
"""
SIDEBAR_SIZE = 34
SIDEBAR = f"""
[[sidebar]]
edge = "left"
size = {SIDEBAR_SIZE}
visible = true
  [[sidebar.panel]]
  plugin = "sessions"
"""
SETTLE = 1.4

# The area a View's cells fill is the terminal minus the status row, and the box
# is that area's percentage with a one-cell frame.
AREA_W, AREA_H = COLS, ROWS - 1
BOX_W, BOX_H = AREA_W * 60 // 100, AREA_H * 50 // 100
BOX_X, BOX_Y = (AREA_W - BOX_W) // 2, (AREA_H - BOX_H) // 2
FLOAT_SIZE = (BOX_H - 2, BOX_W - 2)
FULL_SIZE = (AREA_H - 2, AREA_W - 2)

failures = []
tag_counter = [0]


def check(name, ok, detail=""):
    print(f"  {'PASS' if ok else 'FAIL'}  {name}" + (f"  [{detail}]" if not ok else ""))
    if not ok:
        failures.append(name)


def sgr_right_press(x, y):
    return f"\x1b[<2;{x + 1};{y + 1}M".encode()


def sgr_click(x, y):
    """A left press and release at 0-based `(x, y)`; SGR is 1-based."""
    return f"\x1b[<0;{x + 1};{y + 1}M\x1b[<0;{x + 1};{y + 1}m".encode()


def screen(t):
    return "\n".join(t.rows_text())


def zoomed(t):
    return "View 1 Z" in t.rows_text()[-1]


def size_of_focused(t):
    """`(rows, cols)` as the focused cell's shell reports it. The shell assembles
    the marker, so the typed command can never match it.
    """
    tag_counter[0] += 1
    tag = f"q{tag_counter[0]}"
    t.send(f"printf 'SZ%s:%s\\n' {tag} \"$(stty size)\"\r".encode(), 0.9)
    pattern = re.compile(rf"SZ{tag}:(\d+) (\d+)")
    for row in t.rows_text():
        for m in pattern.finditer(row):
            return int(m.group(1)), int(m.group(2))
    return None


def marker_at(t, word):
    """Every `(x, y)` where the shell's assembled `RAN:<word>` is on screen."""
    return [(m.start(), y) for y, row in enumerate(t.rows_text())
            for m in re.finditer(f"RAN:{word}", row)]


def print_marker(t, word):
    t.send(f"clear; printf 'RAN:%s\\n' {word}\r".encode(), 0.7)


def reset(t):
    """No zoom, focus on the left cell, and each cell's marker on its top rows."""
    if zoomed(t):
        t.send(ALT_U, SETTLE)
    t.send(ALT_H, 0.7)
    print_marker(t, "alpha")
    t.send(ALT_L, 0.7)
    print_marker(t, "beta")
    t.send(ALT_H, 0.7)


def in_box(point):
    x, y = point
    return BOX_X < x < BOX_X + BOX_W - 1 and BOX_Y < y < BOX_Y + BOX_H - 1


def assert_float(t, label, floated="alpha", other="beta"):
    """The focused cell is floated: box size, box frame, other cell still shown."""
    got = size_of_focused(t)
    check(f"{label}: the pane is sized to the box {FLOAT_SIZE}", got == FLOAT_SIZE, got)
    check(f"{label}: the other cell is still on screen", bool(marker_at(t, other)),
          screen(t))
    check(f"{label}: the status bar shows Z", zoomed(t), t.rows_text()[-1])
    rows = t.rows_text()
    check(f"{label}: the box's corner is where a tab's box goes",
          rows[BOX_Y][BOX_X] == "╭", rows[BOX_Y])
    print_marker(t, floated)
    spots = marker_at(t, floated)
    check(f"{label}: typed keys reach the box", any(in_box(p) for p in spots), spots)
    cursor = (t.screen.cursor.x, t.screen.cursor.y)
    check(f"{label}: the cursor is in the box", in_box(cursor), cursor)


def assert_full(t, label, other="beta"):
    got = size_of_focused(t)
    check(f"{label}: the pane fills the view {FULL_SIZE}", got == FULL_SIZE, got)
    check(f"{label}: the other cell is hidden", not marker_at(t, other), screen(t))
    check(f"{label}: the status bar shows Z", zoomed(t), t.rows_text()[-1])


def assert_released(t, label, size):
    check(f"{label}: no Z in the status bar", not zoomed(t), t.rows_text()[-1])
    got = size_of_focused(t)
    check(f"{label}: the pane is back at its cell's size {size}", got == size, got)


def toggle_section(t, mode, cell_size):
    follow = assert_float if mode == "float" else assert_full
    for key, label in ((ALT_Z, "Alt-z"), (None, "Prefix z")):
        reset(t)

        def press():
            if key:
                t.send(key, SETTLE)
            else:
                t.prefix(b"z", SETTLE)

        press()
        follow(t, f"{label} under mode={mode}")
        press()
        assert_released(t, f"{label} released", cell_size)

    reset(t)
    t.send(ALT_U, SETTLE)
    assert_float(t, "PaneToggleZoomFloat")
    t.send(ALT_I, SETTLE)
    assert_released(t, "PaneToggleZoomFullscreen releases a float", cell_size)

    reset(t)
    t.send(ALT_I, SETTLE)
    assert_full(t, "PaneToggleZoomFullscreen")
    t.send(ALT_U, SETTLE)
    assert_released(t, "PaneToggleZoomFloat releases a fullscreen zoom", cell_size)


def focus_section(t, cell_size):
    reset(t)
    t.send(ALT_U, SETTLE)
    t.send(ALT_L, SETTLE)
    assert_released(t, "Alt+l on a float", cell_size)
    print_marker(t, "gamma")
    check("Alt+l on a float: focus stepped to the right cell",
          any(x >= COLS // 2 for x, _ in marker_at(t, "gamma")), screen(t))

    reset(t)
    t.send(ALT_U, SETTLE)
    t.send(ALT_H, SETTLE)
    assert_released(t, "Alt+h at the edge on a float", cell_size)
    print_marker(t, "gamma")
    check("Alt+h at the edge: focus stayed",
          any(x < COLS // 2 for x, _ in marker_at(t, "gamma")), screen(t))

    reset(t)
    t.send(ALT_I, SETTLE)
    t.send(ALT_L, SETTLE)
    check("a fullscreen zoom after Alt+l: the zoom followed focus",
          bool(marker_at(t, "beta")), screen(t))
    assert_full(t, "a fullscreen zoom after Alt+l", other="alpha")


def click_section(t, cell_size):
    # Both points are outside the box: the left one is on the left cell's tile,
    # the right one on the right cell's.
    left_outside = (3, 3)
    right_outside = (COLS - 5, ROWS - 4)

    reset(t)
    t.send(ALT_U, SETTLE)
    t.send(sgr_click(*right_outside), SETTLE)
    assert_released(t, "a click on the other cell", cell_size)
    print_marker(t, "gamma")
    check("a click on the other cell: focus moved there",
          any(x >= COLS // 2 for x, _ in marker_at(t, "gamma")), screen(t))

    reset(t)
    t.send(ALT_U, SETTLE)
    t.send(sgr_click(*left_outside), SETTLE)
    assert_released(t, "a click on the floated cell's empty slot", cell_size)
    print_marker(t, "gamma")
    check("a click on the empty slot: focus stayed",
          any(x < COLS // 2 for x, _ in marker_at(t, "gamma")), screen(t))

    reset(t)
    t.send(ALT_U, SETTLE)
    t.send(sgr_click(5, ROWS - 1), SETTLE)
    check("a click on the status row ends the float", not zoomed(t), t.rows_text()[-1])

    reset(t)
    t.send(ALT_U, SETTLE)
    t.send(sgr_right_press(BOX_X + 3, BOX_Y + 3), SETTLE)
    check("a right press inside the box keeps the float", zoomed(t), t.rows_text()[-1])
    t.send(sgr_right_press(*left_outside), SETTLE)
    check("a right press outside the box keeps the float, as in a tab", zoomed(t),
          t.rows_text()[-1])
    t.send(b"\x1b[<2;4;4m", 0.3)

    # The right cell has the higher index and floats, so the point inside the
    # box over the LEFT cell's tile is the one a first-match hit test gets wrong.
    reset(t)
    t.send(ALT_L, 0.7)
    t.send(ALT_U, SETTLE)
    t.send(sgr_click(BOX_X + 3, BOX_Y + 3), SETTLE)
    assert_float(t, "a click inside the box over the other tile", "beta", "alpha")


def monocle_section(t):
    reset(t)
    for _ in range(8):
        if "monocle" in t.rows_text()[-1]:
            break
        t.send(ALT_SPACE, 0.8)
    check("the view reached Monocle", "monocle" in t.rows_text()[-1], t.rows_text()[-1])

    t.send(ALT_I, SETTLE)
    check("Monocle: a fullscreen zoom is refused", not zoomed(t), t.rows_text()[-1])
    if zoomed(t):
        t.send(ALT_I, SETTLE)

    strip = t.rows_text()[0]
    t.send(ALT_U, SETTLE)
    got = size_of_focused(t)
    check(f"Monocle: a float is sized to the box {FLOAT_SIZE}", got == FLOAT_SIZE, got)
    check("Monocle: the title strip stays up under a float",
          t.rows_text()[0].strip() != "" and t.rows_text()[0] == strip,
          f"{strip!r} -> {t.rows_text()[0]!r}")
    t.send(ALT_U, SETTLE)
    check("Monocle: the float is released", not zoomed(t), t.rows_text()[-1])
    for _ in range(8):
        if "grid" in t.rows_text()[-1]:
            break
        t.send(ALT_SPACE, 0.8)


def tmux_section(t):
    reset(t)
    t.send(ALT_Y, 0.8)
    check("tmux style: a tile has no border", t.rows_text()[0][0] != "╭", t.rows_text()[0])
    t.send(ALT_U, SETTLE)
    got = size_of_focused(t)
    check(f"tmux style: a float is sized to the box {FLOAT_SIZE}", got == FLOAT_SIZE, got)
    rows = t.rows_text()
    check("tmux style: the box has its frame", rows[BOX_Y][BOX_X] == "╭", rows[BOX_Y])
    t.send(ALT_U, SETTLE)
    t.send(ALT_Y, 0.8)


def run(mode):
    print(f"--- [zoom] mode = {mode}")
    rundir = f"{RUNDIR}/{mode}"
    t = Tui(rundir, cols=COLS, rows=ROWS, config=CONFIG.format(mode=mode))
    try:
        t.start()
        t.pump(1.0)
        t.prefix(b"pv", 0.8)
        t.send(b"\x1bt", 0.8)
        sm_compose_view(t, panes=(0, 1), settle=1.4)
        check("a view is displayed", t.has("View 1"), screen(t))
        if not t.has("View 1"):
            return
        cell_size = size_of_focused(t)
        check("the cell's size differs from the box's",
              cell_size is not None and cell_size != FLOAT_SIZE, cell_size)

        toggle_section(t, mode, cell_size)
        focus_section(t, cell_size)
        click_section(t, cell_size)
        monocle_section(t)
        tmux_section(t)

        check("the client is alive", t.alive())
        server_log = t.log("server")
        check("the server log saw a forced float", "mode: Some(Float)" in server_log)
        check("no panic in either log",
              bool(server_log) and "panicked at" not in server_log + t.log("client"))
    finally:
        t.kill()
        shutil.rmtree(rundir, ignore_errors=True)


def compose(t):
    t.start()
    t.pump(1.0)
    t.prefix(b"pv", 0.8)
    t.send(b"\x1bt", 0.8)
    sm_compose_view(t, panes=(0, 1), settle=1.4)
    check("a view is displayed", t.has("View 1"), screen(t))


def view_over_the_wire(sock):
    """A second client, and the View as it hears of it first."""
    other = WireClient(sock)
    other.hello()
    lists = [m["ViewList"]["views"] for m in other.drain(0.8) if name_of(m) == "ViewList"]
    return other, lists[-1][0]


def resize_section(t):
    t.send(ALT_Z, SETTLE)
    cols, rows = 100, 30
    t.resize(cols, rows, SETTLE)
    box_w, box_h = cols * 60 // 100, (rows - 1) * 50 // 100
    got = size_of_focused(t)
    check("resize: the pane follows the box", got == (box_h - 2, box_w - 2), got)
    corner = t.rows_text()[(rows - 1 - box_h) // 2][(cols - box_w) // 2]
    check("resize: the box moved with the terminal", corner == "╭", corner)
    t.resize(COLS, ROWS, SETTLE)
    got = size_of_focused(t)
    check("resize: the pane is at the box again", got == FLOAT_SIZE, got)
    t.send(ALT_Z, SETTLE)


def two_terminals_section(t):
    """A smaller terminal demands a smaller box, and sizing is the minimum."""
    t.send(ALT_Z, SETTLE)
    other, view = view_over_the_wire(f"{t.rundir}/run/remux.sock")
    check("two terminals: the second one hears of the float",
          view["zoomed"] and view["zoom_mode"] == "float", view)
    pane = view["cells"][view["focused"]]["pane_id"]
    other.send({"SubscribePane": {"pane_id": pane, "cols": 40, "rows": 10, "size_demand": True}})
    t.pump(SETTLE)
    got = size_of_focused(t)
    check("two terminals: the pane fits the smaller box", got == (10, 40), got)
    check("two terminals: the float is still up", zoomed(t), t.rows_text()[-1])
    other.send({"UnsubscribePane": {"pane_id": pane}})
    other.close()
    t.pump(SETTLE)
    got = size_of_focused(t)
    check("two terminals: the pane returns to this box", got == FLOAT_SIZE, got)
    t.send(ALT_Z, SETTLE)


def slot_press_section(t):
    t.send(TRACKER, 0.8)
    t.send(ALT_Z, SETTLE)
    t.send(sgr_click(3, 3), SETTLE)
    check("a slot press ends the float", not zoomed(t), t.rows_text()[-1])
    check("a slot press reaches the tracking application",
          bool(PRESS_REPORT.search(screen(t))), screen(t))
    t.send(b"\x03", 0.5)
    t.send(UNTRACK, 0.8)


def pane_exit_section(t):
    t.send(ALT_Z, SETTLE)
    t.send(b"exit\r", SETTLE)
    check("the floated pane exiting ends the float", not zoomed(t), t.rows_text()[-1])
    check("the exited pane's tile says so", t.has("pane closed"), screen(t))


def delete_section(t):
    t.send(ALT_L, 0.7)
    t.send(ALT_Z, SETTLE)
    other, view = view_over_the_wire(f"{t.rundir}/run/remux.sock")
    other.send({"ViewDelete": {"id": view["id"]}})
    other.drain(0.5)
    other.close()
    t.pump(SETTLE)
    check("ViewDelete under a float: the view is gone", not t.has("View 1"), screen(t))
    t.send(b"printf 'K%s\n' Z\r", 1.0)
    check("ViewDelete under a float: keys reach the shell", t.has("KZ"), screen(t))


def run_extras():
    print("--- extras")
    rundir = f"{RUNDIR}/extras"
    t = Tui(rundir, cols=COLS, rows=ROWS, config=CONFIG.format(mode="float"))
    try:
        compose(t)
        resize_section(t)
        two_terminals_section(t)
        slot_press_section(t)
        pane_exit_section(t)
        delete_section(t)
        check("extras: the client is alive", t.alive())
        check("extras: no panic in either log",
              "panicked at" not in t.log("server") + t.log("client"))
    finally:
        t.kill()
        shutil.rmtree(rundir, ignore_errors=True)


def run_with_sidebar():
    print("--- with a left sidebar")
    rundir = f"{RUNDIR}/sidebar"
    config = CONFIG.format(mode="float") + SIDEBAR
    t = Tui(rundir, cols=COLS, rows=ROWS, config=config)
    width = COLS - SIDEBAR_SIZE
    box_w = width * 60 // 100
    box_x = SIDEBAR_SIZE + (width - box_w) // 2
    want = (BOX_H - 2, box_w - 2)
    try:
        t.start()
        t.pump(1.0)
        t.prefix(b"pv", 0.8)
        t.send(b"\x1bt", 0.8)
        sm_compose_view(t, panes=(0, 1), settle=1.4)
        check("sidebar: a view is displayed", t.has("View 1"), screen(t))
        cell_size = size_of_focused(t)

        t.send(ALT_Z, SETTLE)
        got = size_of_focused(t)
        check(f"sidebar: the float is sized from the content rect {want}", got == want, got)
        rows = t.rows_text()
        check("sidebar: the box sits inside the content rect",
              rows[BOX_Y][box_x] == "╭", rows[BOX_Y])
        t.send(sgr_click(5, 5), SETTLE)
        check("sidebar: a click on a panel ends the float", not zoomed(t), t.rows_text()[-1])
        # The click put the keyboard on the panel, and a press on a cell takes it
        # back.
        t.send(sgr_click(SIDEBAR_SIZE + 3, 3), SETTLE)
        assert_released(t, "sidebar: after a click on a panel", cell_size)

        t.send(ALT_Z, SETTLE)
        t.send(sgr_click(SIDEBAR_SIZE - 1, 2), SETTLE)
        assert_released(t, "sidebar: a click on the sidebar's frame", cell_size)

        t.send(ALT_Z, SETTLE)
        t.send(ALT_H, SETTLE)
        assert_released(t, "sidebar: Alt+h at the edge", cell_size)
        print_marker(t, "delta")
        check("sidebar: Alt+h ended the float and left the keyboard in the cell",
              bool(marker_at(t, "delta")), screen(t))
        check("sidebar: no panic in either log",
              "panicked at" not in t.log("server") + t.log("client"))
    finally:
        t.kill()
        shutil.rmtree(rundir, ignore_errors=True)


def main():
    for mode in sys.argv[1:] or ("float", "fullscreen", "sidebar", "extras"):
        if mode == "sidebar":
            run_with_sidebar()
        elif mode == "extras":
            run_extras()
        else:
            run(mode)
    if failures:
        print(f"FAIL view_zoom: {len(failures)} check(s)")
        sys.exit(1)
    print("PASS view_zoom")


if __name__ == "__main__":
    main()
