#!/usr/bin/env python3
"""The status bar spans the whole terminal, whatever sidebars are shown.

A real PTY, because with a sidebar shown the CLIENT draws the bar: the server's
frame is only as wide as the content rect, and it is shared by every client on
the session, so it cannot carry a bar at this client's width. The frame harness
would see the server's content-width bar and pass on a client that never drew
its own.

What it covers:

  1  left + right sidebars: the last row is the bar from column 0 to the final
     column. Column 0 is the mode chip, the final column is the layout
     indicator's trailing cell, a middle column is the bar's own background,
     and the indicator's text ends at the terminal's right edge rather than at
     the content's
  2  the same sidebars stop one row above the bar: their bottom corners are on
     the second-to-last row and nothing of theirs is on the last
  3  a click on a tab in the bar, at a column that is UNDER THE LEFT SIDEBAR on
     every other row, switches to that tab. The client forwards it to the
     server's bar row at the same column; a plain content translation would
     clamp it to content column 0 and miss the tab
  4  a bottom sidebar sits ABOVE the bar: its box ends on the second-to-last
     row, the bar is on the last, and the panes end above the sidebar
  5  a View with a sidebar shown gets the same full-width bar, drawn from the
     view's own status line
  6  with no sidebar, the bar is where it always was

Run from the repo root:
    python3 tests/pty/status_bar_sidebars.py
"""
import os
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pty_harness import BIN, Tui, sm_compose_view  # noqa: E402

COLS, ROWS = 100, 30
LEFT_W, RIGHT_W, BOTTOM_H = 22, 18, 6

FAILURES = []


def check(name, cond, detail=""):
    if cond:
        print(f"  PASS  {name}")
    else:
        print(f"  FAIL  {name}\n        {detail}")
        FAILURES.append(name)


def theme_hex(role):
    """`role`'s default in `src/config/theme.rs`, as the hex pyte reports.

    Read out of the source so a changed default cannot leave this harness
    looking for a colour nothing paints."""
    needle = f"{role}: ThemeColor::Rgb("
    for line in open("src/config/theme.rs"):
        if needle in line:
            rgb = line.split(needle)[1].split(")")[0]
            return "".join(f"{int(v):02x}" for v in rgb.split(","))
    raise SystemExit(f"no Rgb default for {role}")


MODE_NORMAL_BG = theme_hex("mode_normal_bg")
STATUS_BAR_BG = theme_hex("status_bar_bg")
TAB_ACTIVE_BG = theme_hex("tab_active_bg")
# `layout_indicator_bg` defaults to `Indexed(245)`, which pyte reports through
# its 256-colour palette rather than as an Rgb literal in theme.rs.
LAYOUT_BG = "8a8a8a"

SIDEBAR = """
[[sidebar]]
edge = "{edge}"
size = {size}
visible = true

  [[sidebar.panel]]
  plugin = "placeholder"
  weight = 1
"""

CFG_LEFT_RIGHT = SIDEBAR.format(edge="left", size=LEFT_W) + SIDEBAR.format(
    edge="right", size=RIGHT_W
)
CFG_BOTTOM = SIDEBAR.format(edge="bottom", size=BOTTOM_H)


def bg(t, x, y):
    return str(t.screen.buffer[y][x].bg)


def stop(t):
    t.kill()
    env = {
        **os.environ,
        "XDG_RUNTIME_DIR": f"{t.rundir}/run",
        "XDG_STATE_HOME": f"{t.rundir}/state",
        "XDG_DATA_HOME": f"{t.rundir}/data",
        "XDG_CONFIG_HOME": f"{t.rundir}/config",
    }
    subprocess.run([BIN, "stop"], env=env, stdout=subprocess.DEVNULL,
                   stderr=subprocess.DEVNULL, timeout=10)


def check_logs(t, label):
    for which in ("client", "server"):
        check(f"{label}: no panic in {which}.log",
              "panicked at" not in t.log(which), t.log(which)[-1500:])


def check_full_width_bar(t, label, needle):
    """The last row is a bar running from column 0 to the final column."""
    last = ROWS - 1
    text = t.rows_text()[last]
    check(f"{label}: the last row is the status bar", needle in text, repr(text))
    check(f"{label}: column 0 is the mode chip",
          bg(t, 0, last) == MODE_NORMAL_BG and text.startswith(" [NORMAL]"),
          (bg(t, 0, last), repr(text[:12])))
    check(f"{label}: the final column is the layout indicator",
          bg(t, COLS - 1, last) == LAYOUT_BG, bg(t, COLS - 1, last))
    mid = COLS // 2
    check(f"{label}: a middle column is the bar's own background",
          bg(t, mid, last) == STATUS_BAR_BG, bg(t, mid, last))
    layout = text.rstrip().split()[-1]
    check(f"{label}: the layout indicator ends at the terminal's right edge",
          text.rstrip().endswith(layout) and len(text.rstrip()) == COLS - 1
          and layout in ("bsp", "master", "monocle", "grid", "custom"),
          repr(text[-20:]))


def test_left_and_right():
    label = "left+right"
    t = Tui("/tmp/rmx-sbs1", cols=COLS, rows=ROWS, config=CFG_LEFT_RIGHT).start()
    t.pump(1.5)
    rows = t.rows_text()
    check_full_width_bar(t, label, "[NORMAL]")

    # -- 2: the verticals stop one row above the bar -------------------------
    check(f"{label}: the left sidebar's box ends on the second-to-last row",
          rows[ROWS - 2][0] == "╰", repr(rows[ROWS - 2][:4]))
    check(f"{label}: the right sidebar's box ends on the second-to-last row",
          rows[ROWS - 2][COLS - 1] == "╯", repr(rows[ROWS - 2][-4:]))
    # The bar's own separator is a `│`, so the sidebars are recognised by their
    # corners and by the frame edge they would have in the edge columns.
    check(f"{label}: nothing of either sidebar is on the last row",
          not any(ch in "╰╯" for ch in rows[ROWS - 1])
          and rows[ROWS - 1][0] != "│" and rows[ROWS - 1][COLS - 1] != "│",
          repr(rows[ROWS - 1]))

    # -- 3: a click on the bar reaches the server's tab regions --------------
    t.prefix(b"tn", 1.5)
    last = t.rows_text()[ROWS - 1]
    x = last.find("Tab 1")
    check(f"{label}: a second tab exists and Tab 1 is inactive",
          x >= 0 and "Tab 2" in last and bg(t, x, ROWS - 1) != TAB_ACTIVE_BG,
          repr(last))
    check(f"{label}: Tab 1 sits under the left sidebar's columns, so the click "
          "discriminates", 0 <= x < LEFT_W, x)
    if x >= 0:
        # SGR mouse, 1-based.
        t.send(f"\x1b[<0;{x + 2};{ROWS}M".encode(), 0.2)
        t.send(f"\x1b[<0;{x + 2};{ROWS}m".encode(), 1.5)
        check(f"{label}: clicking Tab 1 on the client's bar switches to it",
              bg(t, x + 1, ROWS - 1) == TAB_ACTIVE_BG,
              (bg(t, x + 1, ROWS - 1), repr(t.rows_text()[ROWS - 1])))

    check(f"{label}: client alive", t.alive())
    check_logs(t, label)
    stop(t)


def test_bottom():
    label = "bottom"
    t = Tui("/tmp/rmx-sbs2", cols=COLS, rows=ROWS, config=CFG_BOTTOM).start()
    t.pump(1.5)
    t.send("printf 'PANE%s\\n' ROW\r", 1.0)
    rows = t.rows_text()
    check_full_width_bar(t, label, "[NORMAL]")
    top = ROWS - 1 - BOTTOM_H
    check(f"{label}: the bottom sidebar's box starts {BOTTOM_H} rows above the bar",
          rows[top][0] == "╭", repr(rows[top][:4]))
    check(f"{label}: the bottom sidebar's box ends on the second-to-last row",
          rows[ROWS - 2][0] == "╰" and rows[ROWS - 2][COLS - 1] == "╯",
          repr(rows[ROWS - 2]))
    check(f"{label}: the bar is not duplicated above the sidebar",
          sum("[NORMAL]" in r for r in rows) == 1,
          [i for i, r in enumerate(rows) if "[NORMAL]" in r])
    pane_rows = [i for i, r in enumerate(rows) if "PANEROW" in r]
    check(f"{label}: the pane's output is above the sidebar",
          bool(pane_rows) and max(pane_rows) < top, pane_rows)
    check(f"{label}: the pane's bottom border is directly above the sidebar",
          rows[top - 1][0] == "╰", repr(rows[top - 1][:4]))
    check(f"{label}: client alive", t.alive())
    check_logs(t, label)
    stop(t)


def test_view():
    label = "view"
    t = Tui("/tmp/rmx-sbs3", cols=COLS, rows=ROWS, config=CFG_LEFT_RIGHT).start()
    t.pump(1.0)
    t.send("clear\r", 0.4)
    t.prefix(b"pv", 0.8)
    sm_compose_view(t, panes=(0, 1), settle=2.0)
    t.pump(1.0)
    # Recognised by a view cell's title on its border, NOT by the bar: the bar
    # is the thing under test, so gating on it would skip the checks exactly
    # when the bar is missing.
    in_view = t.has("/ Tab 1") and not t.has("Session Manager")
    check(f"{label}: a view is on screen", in_view, t.rows_text())
    if in_view:
        check_full_width_bar(t, label, "View 1")
    check(f"{label}: client alive", t.alive())
    check_logs(t, label)
    stop(t)


def test_no_sidebar():
    label = "no sidebar"
    t = Tui("/tmp/rmx-sbs4", cols=COLS, rows=ROWS, config="").start()
    t.pump(1.5)
    check_full_width_bar(t, label, "[NORMAL]")
    check(f"{label}: the pane's box starts at column 0 on row 0",
          t.rows_text()[0][0] == "╭", repr(t.rows_text()[0][:4]))
    check(f"{label}: client alive", t.alive())
    check_logs(t, label)
    stop(t)


def main():
    if not os.path.exists(BIN):
        raise SystemExit(f"{BIN} not found; run `cargo build` first")
    test_left_and_right()
    test_bottom()
    test_view()
    test_no_sidebar()
    if FAILURES:
        print(f"\nFAILED: {len(FAILURES)}")
        for f in FAILURES:
            print(f"  - {f}")
        sys.exit(1)
    print("\nOK")


if __name__ == "__main__":
    main()
